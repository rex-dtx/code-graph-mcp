#!/usr/bin/env python3
"""Build a Tier-3 "exact symbol name" retrieval slice from one or more
.code-graph/index.db files.

Each query is a bare symbol name; the gold is the node that DEFINES that symbol.
This exercises the ranking changes Tier 3 targets (single-identifier weighting +
definition boost), which the doc-comment NL query set does not. Only symbols with
a UNIQUE name (within their DB) are emitted, so the gold node is unambiguous.

Usage:
  python3 build_tier3_slice.py --db /path/a/.code-graph/index.db \
      --db /path/b/.code-graph/index.db --out tier3_slice.jsonl --limit-per-db 250
"""
import argparse
import json
import sqlite3
import sys
from collections import Counter

DB_NS = 10_000_000
CODE_TYPES = {
    "function", "method", "class", "struct", "enum", "trait", "interface", "type",
}


# Mirrors domain::PASCAL_TEST_* / INFIX_TEST_EXTS. One of five must-agree copies
# of this predicate; see the "Five sites must agree" note in src/domain.rs. Both
# sides generate from these lists rather than transcribing boolean chains.
PASCAL_TEST_EXTS = ("cs", "vb", "fs", "java", "kt", "scala", "swift", "php")
PASCAL_TEST_STEMS = ("Test", "Tests", "Spec")
INFIX_TEST_EXTS = ("go", "rs", "py", "dart")


def is_test_path(p: str) -> bool:
    """Port of src/domain.rs is_test_path.

    Kept leg-for-leg in sync with the Rust original: this script selects gold
    records for the retrieval benchmark, and a narrower predicate here silently
    admits test symbols the binary excludes, inflating the measured miss rate.
    """
    # Case-insensitive test/tests DIRECTORY segment at any depth (issue #36:
    # Maven's src/test/java/... and xUnit's src/Tests/<Project>/...).
    lower = p.lower()
    if (lower.startswith(("tests/", "test/"))
            or "/tests/" in lower or "/test/" in lower):
        return True
    # PascalCase test-class convention: case-SENSITIVE and pinned to a known
    # extension so src/latest.cs and src/mytests.rs stay production.
    if any(p.endswith(f"{stem}.{ext}")
           for stem in PASCAL_TEST_STEMS for ext in PASCAL_TEST_EXTS):
        return True
    if any(p.endswith(f"_test.{ext}") for ext in INFIX_TEST_EXTS):
        return True
    if lower.endswith(".py") and (lower.startswith("test_")
                                  or "/test_" in lower
                                  or lower.endswith("conftest.py")):
        return True
    return (p.startswith(("benches/", "bench/")) or "__tests__/" in p
            or p.endswith(("/tests.rs", ".test.ts", ".test.js",
                           ".test.tsx", ".test.jsx", ".spec.ts", ".spec.js",
                           ".spec.tsx", ".spec.jsx")))


def is_test_symbol(name: str, path: str) -> bool:
    """Port of src/domain.rs is_test_symbol. The binary's candidate loop excludes
    these from semantic_code_search (search.rs:201), so selecting them as gold makes
    the tool's CORRECT exclusion register as a retrieval 'miss'. The `is_test = 0`
    column filter alone is INSUFFICIENT — it misses path/name test symbols whose
    is_test column is 0 (e.g. build_oracle in tests/routing_bench.rs). This dual-
    classifier gap inflated the now-retracted 'tier3 14.6% retrieval miss'
    (see feedback_test_classifier_dual_sources; README Phase B correction)."""
    return (name.startswith("test_") or name.endswith(("Test", "Tests"))
            or is_test_path(path))


def build(dbs: list[str], limit_per_db: int = 250) -> list[dict]:
    queries: list[dict] = []
    for db_idx, db_path in enumerate(dbs):
        conn = sqlite3.connect(db_path)
        conn.row_factory = sqlite3.Row
        rows = conn.execute(
            """
            SELECT n.id, n.name, n.type, f.language, f.path
            FROM nodes n JOIN files f ON n.file_id = f.id
            WHERE n.is_test = 0 AND f.language IS NOT NULL
            """
        ).fetchall()
        conn.close()

        cand = [
            r for r in rows
            if r["type"] in CODE_TYPES
            and r["name"]
            and len(r["name"]) >= 3
            and r["name"].replace("_", "").isalnum()  # identifier-ish; drops operators/paths
            and not is_test_symbol(r["name"], r["path"])  # match the binary's exclusion
        ]
        name_counts = Counter(r["name"] for r in cand)
        uniq = [r for r in cand if name_counts[r["name"]] == 1]
        uniq.sort(key=lambda r: r["id"])  # deterministic
        for r in uniq[:limit_per_db]:
            gid = db_idx * DB_NS + int(r["id"])
            queries.append({
                "query_id": f"sym:{gid}",
                "query": r["name"],
                "gold_node_ids": [gid],
                "source": "tier3",
                "query_class": "exact_symbol",
                "language": r["language"],
            })
    return queries


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", action="append", required=True, help="path to a .code-graph/index.db")
    ap.add_argument("--out", default="-")
    ap.add_argument("--limit-per-db", type=int, default=250)
    args = ap.parse_args()

    queries = build(args.db, args.limit_per_db)

    if args.out == "-":
        for q in queries:
            sys.stdout.write(json.dumps(q, ensure_ascii=False) + "\n")
    else:
        with open(args.out, "w") as f:
            for q in queries:
                f.write(json.dumps(q, ensure_ascii=False) + "\n")

    by_lang: dict[str, int] = {}
    for q in queries:
        by_lang[q["language"]] = by_lang.get(q["language"], 0) + 1
    print(f"[build_tier3_slice] {len(queries)} exact_symbol queries; by language={by_lang}",
          file=sys.stderr)


if __name__ == "__main__":
    main()
