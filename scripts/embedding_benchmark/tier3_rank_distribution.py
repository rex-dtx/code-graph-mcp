#!/usr/bin/env python3
# scripts/embedding_benchmark/tier3_rank_distribution.py
"""Gold-rank distribution diagnostic for the tier3 exact-symbol slice.

This is the evidence behind the Phase B NO-GO (see README "Phase B go/no-go").
`eval_ranking.py` reports recall@1 / recall@10, which cannot tell whether a
recall@1 miss is *re-rankable* (the defining node is in the candidate pool but
ranked low — a definition boost could lift it) or a *retrieval miss* (the node
is never fetched — re-ranking cannot help). This script runs the real pipeline
at a deep `--top-k` and buckets each query by the gold node's actual rank:

    rank 1 | rank 2..10 (re-rankable) | rank 11..top_k | absent (retrieval miss)

The measured result (top_k=100) was: rank 11..100 == 0.000 everywhere, so the
definition-boost ceiling is just (rank 2..10) = +1.6pp overall, +0.0pp for JS —
the 14.6% miss is retrieval, not ranking. Hence Phase B NO-GO.

Reuses the committed eval_ranking harness (same isolated-root + no-reindex +
metrics-isolation invariants). Read-only.

Usage:
  python3 tier3_rank_distribution.py \
      --queries tier3_slice.jsonl \
      --root . --root /path/to/ts-project \
      [--binary ./target/release/code-graph-mcp] [--top-k 100]
"""
import argparse
import json
import os
import shutil
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from eval_ranking import McpSession, prepare_isolated_root, encode_global, decode_db_idx


def _new_buckets():
    return {"rank1": 0, "rank2_10": 0, "rank11_topk": 0, "absent": 0, "n": 0}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--queries", required=True, help="tier3 slice JSONL")
    ap.add_argument("--root", action="append", required=True,
                    help="project roots in db_idx order (== build's --db order)")
    ap.add_argument("--binary", default="./target/release/code-graph-mcp")
    ap.add_argument("--top-k", type=int, default=100, help="pool-depth probe (tool clamps <=100)")
    args = ap.parse_args()

    binary = os.path.abspath(args.binary)
    queries = []
    with open(args.queries) as fh:
        for line in fh:
            line = line.strip()
            if line:
                queries.append(json.loads(line))

    by_db = {}
    for q in queries:
        gold = q.get("gold_node_ids")
        if gold:
            by_db.setdefault(decode_db_idx(gold[0]), []).append(q)

    overall = _new_buckets()
    by_lang = {}
    tmp = tempfile.mkdtemp(prefix="cg-rankdist-", dir="/tmp")
    try:
        for db_idx in sorted(by_db):
            if db_idx >= len(args.root):
                raise SystemExit(f"db_idx {db_idx} has no matching --root (got {len(args.root)})")
            iso = prepare_isolated_root(os.path.abspath(args.root[db_idx]),
                                        os.path.join(tmp, f"db{db_idx}"))
            sess = McpSession(binary, iso, args.top_k)
            try:
                for q in by_db[db_idx]:
                    ranked = [encode_global(db_idx, lid) for lid in sess.rank(q["query"])]
                    gold = q["gold_node_ids"][0]
                    b = by_lang.setdefault(q.get("language", "?"), _new_buckets())
                    overall["n"] += 1
                    b["n"] += 1
                    if gold in ranked:
                        r = ranked.index(gold) + 1
                        key = "rank1" if r == 1 else ("rank2_10" if r <= 10 else "rank11_topk")
                    else:
                        key = "absent"
                    overall[key] += 1
                    b[key] += 1
            finally:
                sess.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    def report(name, b):
        n = b["n"] or 1
        rerank = (b["rank2_10"] + b["rank11_topk"]) / n
        print(f"{name:11s} n={b['n']:4d} | rank1={b['rank1']/n:.3f}  rank2-10={b['rank2_10']/n:.3f}  "
              f"rank11-{args.top_k}={b['rank11_topk']/n:.3f}  absent={b['absent']/n:.3f}")
        print(f"            -> re-rankable(rank2-{args.top_k}, definition-boost ceiling)={rerank:.3f}  "
              f"retrieval-miss(absent)={b['absent']/n:.3f}")

    report("OVERALL", overall)
    for lang in sorted(by_lang):
        report(lang, by_lang[lang])


if __name__ == "__main__":
    main()
