#!/usr/bin/env python3
"""Confidence-calibration benchmark for semantic_code_search's `match_confidence`.

Sibling to eval_ranking.py (which measures *retrieval* — ndcg/recall/mrr). This
measures whether the `match_confidence` signal and its `low_confidence_warning`
actually SEPARATE query classes: a good natural-language query should read as
confident, a nonsense query should read as low-confidence noise. The v0.88.0
baseline pins ~every multi-word NL query at match_confidence≈0.45 (OR-fallback
0.6 × intersection 0.75), good and nonsense alike, so the <0.5 warning fires on
the tool's PRIMARY use case (NL search) — a false alarm — while failing to flag
actual nonsense. This harness quantifies that.

Reuses eval_ranking.prepare_isolated_root so the bench runs against a throwaway
COPY of a real index (embeddings + vec0 shadow tables preserved) and never
pollutes the real project's adoption metrics.

## Running (MUST use an embed-model binary — vector search drives confidence)

    cargo build --release            # embed-model is the default feature
    python3 scripts/embedding_benchmark/eval_confidence.py \
        --root . \
        --queries scripts/embedding_benchmark/confidence_queries.jsonl \
        --binary ./target/release/code-graph-mcp \
        --out scripts/embedding_benchmark/results/confidence_baseline.json

If the index lacks embeddings the run aborts (nonsense would look identical to
good NL — nothing to measure). See memory `eval-embed-model-gotcha`.
"""
import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from eval_ranking import prepare_isolated_root  # noqa: E402  (reuse isolation infra)

# The warning is measured directly from the response (`warning_fired`), NOT by
# comparing match_confidence to a threshold — the tool fires it on a text-anchor
# mechanic (vector-only), not a match_confidence cutoff. Kept only as a display label.
WARN_LABEL = "vector-only trigger"


class ConfSession:
    """One `serve` process; issues NON-compact semantic_code_search so the response
    carries `match_confidence` / `low_confidence_warning` (compact=true returns a
    bare id array that hides both). skip_indexing keeps the copied index intact."""

    def __init__(self, binary: str, root: str, limit: int):
        env = dict(os.environ, CODE_GRAPH_INTERNAL="1")
        self.proc = subprocess.Popen(
            [binary, "serve"], cwd=root, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        assert self.proc.stdin is not None and self.proc.stdout is not None
        self.stdin = self.proc.stdin
        self.stdout = self.proc.stdout
        self.limit = limit
        self._id = 0
        self._initialize()

    def _send(self, obj):
        self.stdin.write(json.dumps(obj) + "\n")
        self.stdin.flush()

    def _read(self, want_id, max_lines=100000):
        for _ in range(max_lines):
            line = self.stdout.readline()
            if not line:
                raise RuntimeError("server closed stdout before responding")
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if obj.get("id") == want_id:
                return obj
        raise RuntimeError(f"no response id={want_id}")

    def _initialize(self):
        self._id += 1
        self._send({"jsonrpc": "2.0", "id": self._id, "method": "initialize",
                    "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                               "clientInfo": {"name": "eval-confidence", "version": "0.1"}}})
        resp = self._read(self._id)
        assert resp["result"]["protocolVersion"] == "2024-11-05", "bad initialize"

    def search(self, query: str) -> dict:
        """Return {mode, match_confidence, warning_fired, node_ids}.

        Retrieval is matched on node_id (like eval_ranking), NOT names: the tool has
        THREE response shapes and only the bare array exposes a per-item `name`.
          - bare array         -> confident (>=0.5), no warning; items have `node_id`.
          - object results[]   -> each item `node_id` (uncompressed / compressed_nodes)
                                  or `node_ids` (compressed_files: symbols grouped per file,
                                  names buried in a `summary` string). Carries
                                  match_confidence + maybe low_confidence_warning.
        """
        self._id += 1
        self._send({"jsonrpc": "2.0", "id": self._id, "method": "tools/call",
                    "params": {"name": "semantic_code_search",
                               "arguments": {"query": query, "limit": self.limit,
                                             "skip_indexing": True}}})
        resp = self._read(self._id)
        empty = {"mode": "error", "match_confidence": None, "warning_fired": False, "node_ids": []}
        if resp.get("error"):
            return empty
        text = resp.get("result", {}).get("content", [{}])[0].get("text", "")
        try:
            payload = json.loads(text)
        except (json.JSONDecodeError, TypeError):
            return empty
        node_ids: list[int] = []
        if isinstance(payload, list):
            for it in payload:
                if isinstance(it, dict) and it.get("node_id") is not None:
                    node_ids.append(int(it["node_id"]))
            return {"mode": "bare_array", "match_confidence": None,
                    "warning_fired": False, "node_ids": node_ids}
        if isinstance(payload, dict):
            for it in payload.get("results", []):
                if not isinstance(it, dict):
                    continue
                if it.get("node_id") is not None:
                    node_ids.append(int(it["node_id"]))
                for nid in it.get("node_ids", []):          # compressed_files grouping
                    node_ids.append(int(nid))
            return {"mode": payload.get("mode", "object"),
                    "match_confidence": payload.get("match_confidence"),
                    "warning_fired": "low_confidence_warning" in payload,
                    "node_ids": node_ids}
        return empty

    def close(self):
        try:
            self.stdin.close()
            self.proc.wait(timeout=15)
        except Exception:
            self.proc.kill()


def _mean(xs):
    xs = [x for x in xs if x is not None]
    return round(sum(xs) / len(xs), 4) if xs else None


def resolve_gold_ids(db_path: str, queries: list) -> dict:
    """Map every gold_name in the corpus to its node_id(s). Node-id matching is
    shape-agnostic (see ConfSession.search); names would miss compressed responses."""
    names = sorted({n for q in queries for n in (q.get("gold_names") or [])})
    if not names:
        return {}
    conn = sqlite3.connect(db_path)
    try:
        ph = ",".join("?" * len(names))
        cur = conn.execute(f"SELECT name, id FROM nodes WHERE name IN ({ph})", names)
        out: dict = {}
        for name, nid in cur.fetchall():
            out.setdefault(name, []).append(int(nid))
    finally:
        conn.close()
    missing = [n for n in names if n not in out]
    if missing:
        raise SystemExit(f"gold_names not found in index (fix corpus or reindex): {missing}")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True, help="project root with a real .code-graph/index.db")
    ap.add_argument("--queries", required=True, help="confidence_queries.jsonl")
    ap.add_argument("--binary", default="./target/release/code-graph-mcp")
    ap.add_argument("--limit", type=int, default=10)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)

    queries = []
    with open(args.queries) as fh:
        for line in fh:
            line = line.strip()
            if line:
                queries.append(json.loads(line))

    src_db = os.path.join(args.root, ".code-graph", "index.db")
    name_to_ids = resolve_gold_ids(src_db, queries)

    rows = []
    # Force /tmp (not $TMPDIR): under Claude Code $TMPDIR is ~/.claude/tmp/ and a
    # SIGKILL mid-run would otherwise leak cg-conf-* into that tree.
    tmp = tempfile.mkdtemp(prefix="cg-conf-", dir="/tmp")
    try:
        iso = prepare_isolated_root(args.root, os.path.join(tmp, "db0"))
        session = ConfSession(binary, iso, args.limit)
        try:
            for q in queries:
                r = session.search(q["query"])
                gold_names = q.get("gold_names") or []
                gold_ids = {i for n in gold_names for i in name_to_ids.get(n, [])}
                returned = set(r["node_ids"])
                hit = bool(gold_ids & returned) if gold_names else None
                rows.append({
                    "query": q["query"],
                    "query_class": q["query_class"],
                    "expects_confident": q.get("expects_confident"),
                    "match_confidence": r["match_confidence"],
                    "warning_fired": r["warning_fired"],
                    "mode": r["mode"],
                    "retrieval_hit": hit,
                    "n_returned": len(returned),
                })
        finally:
            session.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    # ---- Per-class aggregation ----
    by_class = {}
    for r in rows:
        by_class.setdefault(r["query_class"], []).append(r)
    per_class = {}
    for cls, rs in sorted(by_class.items()):
        hits = [r["retrieval_hit"] for r in rs if r["retrieval_hit"] is not None]
        per_class[cls] = {
            "n": len(rs),
            "warning_fired_rate": round(sum(r["warning_fired"] for r in rs) / len(rs), 3),
            "bare_confident": sum(r["mode"] == "bare_array" for r in rs),
            "mean_exposed_conf": _mean([r["match_confidence"] for r in rs]),
            "retrieval_hit_rate": round(sum(hits) / len(hits), 3) if hits else None,
        }

    # ---- Calibration confusion (only queries with a confidence label) ----
    labeled = [r for r in rows if r["expects_confident"] is not None]
    false_alarm = [r for r in labeled if r["expects_confident"] and r["warning_fired"]]
    miss = [r for r in labeled if not r["expects_confident"] and not r["warning_fired"]]
    correct = [r for r in labeled if (r["expects_confident"] != r["warning_fired"])]
    confusion = {
        "labeled_n": len(labeled),
        "false_alarm_n": len(false_alarm),   # good/exact/code query WRONGLY warned
        "miss_n": len(miss),                 # nonsense query NOT warned
        "correct_n": len(correct),
        "accuracy": round(len(correct) / len(labeled), 3) if labeled else None,
        "false_alarm_queries": [r["query"] for r in false_alarm],
        "miss_queries": [r["query"] for r in miss],
    }

    # ---- Separation: good_nl vs nonsense (the headline metric) ----
    good_conf = _mean([r["match_confidence"] for r in rows if r["query_class"] == "good_nl"])
    nonsense_conf = _mean([r["match_confidence"] for r in rows if r["query_class"] == "nonsense"])
    separation = None
    if good_conf is not None and nonsense_conf is not None:
        separation = round(good_conf - nonsense_conf, 4)

    result = {
        "harness": "eval_confidence",
        "warn_trigger": WARN_LABEL,
        "n_queries": len(rows),
        "per_class": per_class,
        "calibration_confusion": confusion,
        "separation_good_nl_minus_nonsense": separation,
        "good_nl_mean_conf": good_conf,
        "nonsense_mean_conf": nonsense_conf,
        "rows": rows,
    }
    out_dir = os.path.dirname(args.out)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    with open(args.out, "w") as fh:
        json.dump(result, fh, indent=2)

    # ---- Human-readable report ----
    print(f"\n=== confidence calibration (n={len(rows)}, warn={WARN_LABEL}) ===")
    print(f"{'class':<14} {'n':>3} {'warn%':>6} {'bare_conf':>9} {'mean_conf':>9} {'retr_hit%':>9}")
    for cls in ["exact_id", "code_vocab", "good_nl", "ambiguous_nl", "nonsense"]:
        c = per_class.get(cls)
        if not c:
            continue
        mc = c["mean_exposed_conf"]
        rh = c["retrieval_hit_rate"]
        print(f"{cls:<14} {c['n']:>3} {c['warning_fired_rate']*100:>5.0f}% "
              f"{c['bare_confident']:>9} {('-' if mc is None else f'{mc:.3f}'):>9} "
              f"{('-' if rh is None else f'{rh*100:.0f}%'):>9}")
    print(f"\ncalibration accuracy: {confusion['accuracy']}  "
          f"(false alarms={confusion['false_alarm_n']}, misses={confusion['miss_n']}, "
          f"labeled={confusion['labeled_n']})")
    print(f"separation good_nl−nonsense: {separation}  "
          f"(good_nl={good_conf}, nonsense={nonsense_conf})")
    if confusion["false_alarm_queries"]:
        print("\nFALSE ALARMS (good query wrongly warned — the calibration bug):")
        for q in confusion["false_alarm_queries"]:
            print(f"  ✗ {q}")
    if confusion["miss_queries"]:
        print("\nMISSES (nonsense NOT warned):")
        for q in confusion["miss_queries"]:
            print(f"  ✗ {q}")
    print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
