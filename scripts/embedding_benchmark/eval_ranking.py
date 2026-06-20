#!/usr/bin/env python3
"""End-to-end retrieval benchmark for semantic_code_search.

Drives the REAL ranking pipeline (FTS + vector + RRF + adjusted-score re-rank) by
spawning `code-graph-mcp serve` over stdio, unlike eval_retrieval.py which is
vector-only. The driver/main lives below the helpers (added in the next task).
"""
import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile

from metrics import ndcg_at_k, recall_at_k, reciprocal_rank

DB_NS = 10_000_000


def decode_db_idx(gid: int) -> int:
    """Recover the source-DB index encoded into a global node id."""
    return gid // DB_NS


def encode_global(db_idx: int, local_id: int) -> int:
    """Map a server-local node id back into the global namespace."""
    return db_idx * DB_NS + local_id


def parse_tool_result(rpc_response: dict) -> list[int]:
    """Extract ranked LOCAL node_ids from a tools/call response.

    The tool's JSON is wrapped in result.content[0].text. compact=true yields either
    a JSON array of {node_id, ...} (happy path) or an object {results: [...]} for the
    empty / no-match path. Returns [] on error/empty/malformed."""
    if rpc_response.get("error"):
        return []
    result = rpc_response.get("result")
    if not isinstance(result, dict):
        return []
    content = result.get("content")
    if not content:
        return []
    text = content[0].get("text", "")
    try:
        payload = json.loads(text)
    except (json.JSONDecodeError, TypeError):
        return []
    if isinstance(payload, list):
        return [int(it["node_id"]) for it in payload if isinstance(it, dict) and "node_id" in it]
    if isinstance(payload, dict):
        return [int(it["node_id"]) for it in payload.get("results", [])
                if isinstance(it, dict) and "node_id" in it]
    return []


def prepare_isolated_root(real_root: str, workdir: str) -> str:
    """Create a throwaway project root holding a consistent snapshot of real_root's
    index.db plus project markers. Running the server here keeps its usage.jsonl
    flush out of the real project's adoption metrics, and the markers make run_serve
    activate the full (non-stub) server. sqlite3.backup() captures WAL + vec0 shadow
    tables, so vector search works against the copy."""
    src_db = os.path.join(real_root, ".code-graph", "index.db")
    if not os.path.exists(src_db):
        raise SystemExit(f"no index at {src_db}; run `code-graph-mcp rebuild-index` in {real_root}")
    dst_dir = os.path.join(workdir, ".code-graph")
    os.makedirs(dst_dir, exist_ok=True)
    src = sqlite3.connect(src_db)
    dst = sqlite3.connect(os.path.join(dst_dir, "index.db"))
    try:
        with dst:
            src.backup(dst)
    finally:
        src.close()
        dst.close()
    with open(os.path.join(workdir, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "bench-fixture"\nversion = "0.0.0"\nedition = "2021"\n')
    os.makedirs(os.path.join(workdir, ".git"), exist_ok=True)
    return workdir


class McpSession:
    """One `code-graph-mcp serve` process driven over stdio (newline-delimited JSON-RPC).

    Intentionally does NOT send notifications/initialized (would trigger background
    startup indexing); every search passes skip_indexing=true. Both guarantee the
    copied index is never reindexed/wiped."""

    def __init__(self, binary: str, root: str, top_k: int):
        env = dict(os.environ, CODE_GRAPH_INTERNAL="1")
        self.proc = subprocess.Popen(
            [binary, "serve"], cwd=root, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        self.top_k = top_k
        self._id = 0
        self._initialize()

    def _send(self, obj: dict):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def _read_response(self, want_id: int, max_lines: int = 100000) -> dict:
        for _ in range(max_lines):
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("server closed stdout before responding")
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue  # skip notifications / non-JSON noise
            if obj.get("id") == want_id:
                return obj
        raise RuntimeError(f"no JSON-RPC response with id={want_id}")

    def _initialize(self):
        self._id += 1
        self._send({"jsonrpc": "2.0", "id": self._id, "method": "initialize",
                    "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                               "clientInfo": {"name": "eval-ranking", "version": "0.1"}}})
        resp = self._read_response(self._id)
        assert resp["result"]["protocolVersion"] == "2024-11-05", "unexpected initialize response"

    def rank(self, query: str) -> list[int]:
        self._id += 1
        self._send({"jsonrpc": "2.0", "id": self._id, "method": "tools/call",
                    "params": {"name": "semantic_code_search",
                               "arguments": {"query": query, "compact": True,
                                             "top_k": self.top_k, "skip_indexing": True}}})
        return parse_tool_result(self._read_response(self._id))

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=15)
        except Exception:
            self.proc.kill()


def _agg(rows: list[dict]) -> dict:
    if not rows:
        return {"n": 0}
    keys = ["ndcg@10", "recall@1", "recall@10", "mrr"]
    out = {k: round(sum(r[k] for r in rows) / len(rows), 4) for k in keys}
    out["n"] = len(rows)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--queries", action="append", required=True, help="query JSONL (repeatable)")
    ap.add_argument("--root", action="append", required=True,
                    help="project roots in the SAME order as build's --db (db_idx = position)")
    ap.add_argument("--binary", default="./target/release/code-graph-mcp")
    ap.add_argument("--top-k", type=int, default=20)
    ap.add_argument("--head", type=int, default=0, help="debug: only first N queries per db (0=all)")
    ap.add_argument("--min-ndcg", type=float, default=0.0,
                    help="abort if overall ndcg@10 below this (use 0.5 for the NL set to catch "
                         "an embed-model-less build); leave 0 for the tier3 slice")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)

    queries = []
    for qf in args.queries:
        with open(qf) as fh:
            for line in fh:
                line = line.strip()
                if line:
                    queries.append(json.loads(line))

    by_db: dict[int, list] = {}
    for q in queries:
        gold = q.get("gold_node_ids")
        if not gold:
            continue
        by_db.setdefault(decode_db_idx(gold[0]), []).append(q)

    overall, per_lang, per_class = [], {}, {}
    # Force /tmp, not $TMPDIR: under Claude Code $TMPDIR is ~/.claude/tmp/, and a
    # benchmark crash (SIGKILL) would otherwise leak cg-bench-* into that tree.
    tmp = tempfile.mkdtemp(prefix="cg-bench-", dir="/tmp")
    try:
        for db_idx in sorted(by_db):
            if db_idx >= len(args.root):
                raise SystemExit(f"db_idx {db_idx} has no matching --root (got {len(args.root)})")
            iso = prepare_isolated_root(args.root[db_idx], os.path.join(tmp, f"db{db_idx}"))
            session = McpSession(binary, iso, args.top_k)
            qs = by_db[db_idx]
            if args.head:
                qs = qs[:args.head]
            try:
                for q in qs:
                    ranked = [encode_global(db_idx, lid) for lid in session.rank(q["query"])]
                    gold = q["gold_node_ids"]
                    rec = {
                        "ndcg@10": ndcg_at_k(ranked, {g: 1.0 for g in gold}, 10),
                        "recall@1": recall_at_k(ranked, gold, 1),
                        "recall@10": recall_at_k(ranked, gold, 10),
                        "mrr": reciprocal_rank(ranked, gold),
                    }
                    overall.append(rec)
                    per_lang.setdefault(q.get("language", "?"), []).append(rec)
                    per_class.setdefault(q.get("query_class", q.get("source", "?")), []).append(rec)
            finally:
                session.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    result = {
        "harness": "eval_ranking",
        "top_k": args.top_k,
        "overall": _agg(overall),
        "by_language": {k: _agg(v) for k, v in sorted(per_lang.items())},
        "by_query_class": {k: _agg(v) for k, v in sorted(per_class.items())},
    }
    out_dir = os.path.dirname(args.out)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    with open(args.out, "w") as fh:
        json.dump(result, fh, indent=2)
    print(json.dumps(result, indent=2))

    ov = result["overall"].get("ndcg@10", 0.0)
    if not args.head and result["overall"].get("n", 0) and ov < args.min_ndcg:
        print(f"\n[FATAL] overall ndcg@10={ov} < --min-ndcg={args.min_ndcg}: vector search is "
              f"likely inactive (binary built without embed-model, or model not downloaded), "
              f"or the harness is misrouting db_idx->root. Aborting.", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
