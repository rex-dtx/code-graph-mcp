#!/usr/bin/env python3
"""Self-consistent exact-symbol retrieval-drop diagnostic.

Sidesteps the tier3_slice id-drift trap (node ids are reassigned on every
re-index, so committed gold_node_ids go stale): query = a symbol's OWN name,
gold = the set of node ids sharing that name, both read from ONE frozen snapshot
that the binary also serves. So gold ids can never mismatch the served index.

Answers the open question from feedback_eval_ranking_embed_model_gotcha: the
tier3 'absent' miss (~14.6%) — is it a FIXABLE retrieval-stage drop (FTS
garbage-guard / camelCase tokenization) or a true retrieval ceiling? For every
absent case we replay the real FTS path (fts5_search_impl, incl. garbage-guard)
to classify the cause.

Read-only. Reuses the eval_ranking harness invariants (isolated root, no
reindex, skip_indexing, CODE_GRAPH_INTERNAL).
"""
import argparse
import os
import re
import shutil
import sqlite3
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from eval_ranking import McpSession, prepare_isolated_root

STOP = {"a", "an", "and", "the", "or", "in", "of", "for", "to", "with", "is", "it", "this",
        "that", "by", "from", "on", "at", "as", "be", "are", "was", "were", "been", "all",
        "each", "how", "what", "when"}
BM25 = "bm25(nodes_fts, 5.0, 3.0, 2.0, 2.0, 1.0, 5.0, 1.0, 1.0)"
_CAMEL = re.compile(r"[a-z][A-Z]|[A-Z]{2}[a-z]")


def split_identifier(name: str) -> str:
    """Port of src/search/tokenizer.rs split_identifier (camelCase/snake + original)."""
    parts, cur = [], ""
    chars = list(name)
    n = len(chars)
    i = 0
    while i < n:
        c = chars[i]
        if c == "_":
            if cur:
                parts.append(cur); cur = ""
            i += 1
            continue
        if c.isupper() and cur:
            last_lower = cur[-1].islower()
            acronym_end = cur[-1].isupper() and i + 1 < n and chars[i + 1].islower()
            if last_lower or acronym_end:
                parts.append(cur); cur = ""
        cur += c
        i += 1
    if cur:
        parts.append(cur)
    if name not in parts:
        parts.append(name)
    return " ".join(parts)


def build_terms(query: str) -> list[str]:
    """Mirror fts5_search_impl term build (acronym expansion omitted — symbol-name
    queries don't trigger it; it would only widen, never explain a guard-kill)."""
    terms = set()
    for w in query.split():
        if w.lower() in STOP:
            continue
        for piece in split_identifier(w).split():
            san = "".join(c for c in piece if c.isalnum() or c == "_")
            if len(san) >= 2:
                terms.add(san)
    return sorted(terms)


def fts_classify(conn, query: str, fetch: int):
    """Replay fts5_search_impl incl. the garbage-guard. Returns (mode, fts_ids).

    mode ∈ {empty_terms, and_ok, guard_killed, or_fallback}. fts_ids are the
    LOCAL rowids the real FTS path would surface (empty for guard_killed)."""
    terms = build_terms(query)
    if not terms:
        return ("empty_terms", [])
    quoted = [f'"{t}"' for t in terms]
    sql = (f"SELECT fts.rowid FROM nodes_fts fts JOIN nodes n ON n.id = fts.rowid "
           f"WHERE nodes_fts MATCH ? AND n.is_test = 0 ORDER BY {BM25} LIMIT ?")
    if len(terms) > 1:
        rows = conn.execute(sql, (" AND ".join(quoted), fetch)).fetchall()
        ids = [r[0] for r in rows]
        if len(ids) >= max(3, fetch // 10):
            return ("and_ok", ids)
        if len(ids) == 0:
            orig_wc = len([w for w in query.split() if w.lower() not in STOP])
            if orig_wc <= 1:
                san_orig = "".join(c for c in query if c.isalnum() or c == "_")
                if len(san_orig) >= 2:
                    exists = conn.execute(
                        "SELECT 1 FROM nodes_fts fts JOIN nodes n ON n.id = fts.rowid "
                        "WHERE nodes_fts MATCH ? AND n.is_test = 0 LIMIT 1",
                        (f'"{san_orig}"',)).fetchone()
                    if not exists:
                        return ("guard_killed", [])
    rows = conn.execute(sql, (" OR ".join(quoted), fetch)).fetchall()
    return ("or_fallback", [r[0] for r in rows])


# Mirrors domain::PASCAL_TEST_* / INFIX_TEST_EXTS. One of five must-agree copies
# of this predicate; see the "Five sites must agree" note in src/domain.rs.
PASCAL_TEST_EXTS = ("cs", "vb", "fs", "java", "kt", "scala", "swift", "php")
# `Spec` means TEST only in ScalaTest/Kotest. Elsewhere it is a production
# noun (OpenApiSpec.cs, WireSpec.java), so it gets a narrower ext set.
SPEC_TEST_EXTS = ("scala", "kt")
PASCAL_TEST_STEM_EXTS = (
    ("Test", PASCAL_TEST_EXTS),
    ("Tests", PASCAL_TEST_EXTS),
    ("Spec", SPEC_TEST_EXTS),
)
INFIX_TEST_EXTS = ("go", "rs", "py", "dart")


def is_test_path(p: str) -> bool:
    """Port of src/domain.rs is_test_path (path-based test-file detection).

    Kept leg-for-leg in sync with the Rust original: this diagnostic reproduces
    the binary's exclusion set, so a narrower predicate here misattributes an
    excluded symbol to a retrieval drop.
    """
    # Case-insensitive test/tests DIRECTORY segment at any depth (issue #36).
    lower = p.lower()
    if (lower.startswith(("tests/", "test/"))
            or "/tests/" in lower or "/test/" in lower):
        return True
    # PascalCase test-class convention: case-SENSITIVE, pinned to a known ext.
    if any(p.endswith(f"{stem}.{ext}")
           for stem, exts in PASCAL_TEST_STEM_EXTS for ext in exts):
        return True
    if any(p.endswith(f"_test.{ext}") for ext in INFIX_TEST_EXTS):
        return True
    # pytest conventions. Case-SENSITIVE like the PascalCase leg above: pytest
    # matches python_files without normcase and finds conftest by the literal
    # basename, so api/Test_Signup.py is a production module. Matches the
    # case-sensitive GLOB in is_test_node_sql.
    if p.endswith(".py") and (p.startswith("test_")
                              or "/test_" in p
                              or p.endswith("conftest.py")):
        return True
    return (p.startswith(("benches/", "bench/")) or "__tests__/" in p
            or p.endswith(("/tests.rs", ".test.ts", ".test.js",
                           ".test.tsx", ".test.jsx", ".spec.ts", ".spec.js",
                           ".spec.tsx", ".spec.jsx")))


def is_test_symbol(name: str, path: str) -> bool:
    """Port of src/domain.rs is_test_symbol — the EXACT exclusion the binary's
    candidate loop applies (search.rs:201). Probing on the nodes.is_test column
    alone misses path-based test files (e.g. build_oracle in tests/routing_bench.rs,
    is_test=0) and mislabels the binary's correct exclusion as a retrieval miss."""
    return (name.startswith("test_") or name.endswith("Test")
            or name.endswith("Tests") or is_test_path(path))


def select_probes(conn, cap: int):
    """Distinct symbol names the binary CAN actually return. A node is retrievable
    only if it survives BOTH exclusion gates the binary applies:
      1. FTS SQL filter `n.is_test = 0` (indexer-detected #[test]/#[cfg(test)] —
         catches inline Rust unit tests with descriptive non-`test_` names), and
      2. candidate-loop is_test_symbol(name,path) (path/name convention).
    gold = retrievable ids only; names with no retrievable node are not probes
    (the binary can never return them, so querying them is a false 'absent')."""
    rows = conn.execute(
        "SELECT n.name, n.id, f.language, f.path, n.is_test FROM nodes n JOIN files f ON f.id = n.file_id "
        "WHERE n.type IN "
        "  ('function','method','class','struct','enum','trait','interface','type') "
        "  AND n.name NOT LIKE '<%' AND length(n.name) >= 2 AND f.path != '<external>' "
        "ORDER BY n.name").fetchall()
    by_name = {}
    lang_of = {}
    for name, nid, lang, path, is_test in rows:
        if is_test == 1 or is_test_symbol(name, path):
            continue  # binary excludes this node; not a retrievable gold
        by_name.setdefault(name, []).append(nid)
        lang_of.setdefault(name, lang or "?")
    names = sorted(by_name)
    if cap and len(names) > cap:
        step = len(names) / cap
        names = [names[int(i * step)] for i in range(cap)]
    return [(nm, by_name[nm], lang_of[nm]) for nm in names]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", action="append", required=True,
                    help="frozen project roots (each holds .code-graph/index.db)")
    ap.add_argument("--binary", default="./target/release/code-graph-mcp")
    ap.add_argument("--top-k", type=int, default=100)
    ap.add_argument("--cap", type=int, default=500, help="max distinct names probed per db")
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)
    fetch = max(args.top_k * 4, 20)

    def blank():
        return {"n": 0, "rank1": 0, "rank2_10": 0, "rank11_topk": 0, "absent": 0,
                "guard_killed": 0, "fts_has_gold": 0, "fts_no_gold": 0}
    overall = blank()
    by_lang = {}
    samples = []

    tmp = tempfile.mkdtemp(prefix="cg-diag-run-", dir="/tmp")
    try:
        for root in args.root:
            fdb = os.path.join(os.path.abspath(root), ".code-graph", "index.db")
            conn = sqlite3.connect(fdb)
            probes = select_probes(conn, args.cap)
            iso = prepare_isolated_root(os.path.abspath(root),
                                        os.path.join(tmp, os.path.basename(root.rstrip("/")) or "r"))
            sess = McpSession(binary, iso, args.top_k)
            print(f"[{root}] probing {len(probes)} distinct symbol names...", file=sys.stderr)
            try:
                for name, gold_ids, lang in probes:
                    ranked = sess.rank(name)
                    gold = set(gold_ids)
                    b = by_lang.setdefault(lang, blank())
                    overall["n"] += 1; b["n"] += 1
                    hit_rank = next((i + 1 for i, nid in enumerate(ranked) if nid in gold), None)
                    if hit_rank == 1:
                        overall["rank1"] += 1; b["rank1"] += 1
                    elif hit_rank and hit_rank <= 10:
                        overall["rank2_10"] += 1; b["rank2_10"] += 1
                    elif hit_rank:
                        overall["rank11_topk"] += 1; b["rank11_topk"] += 1
                    else:
                        overall["absent"] += 1; b["absent"] += 1
                        mode, fts_ids = fts_classify(conn, name, fetch)
                        if mode == "guard_killed":
                            cls = "guard_killed"
                        elif gold & set(fts_ids):
                            cls = "fts_has_gold"
                        else:
                            cls = "fts_no_gold"
                        overall[cls] += 1; b[cls] += 1
                        if len(samples) < 40:
                            samples.append({"q": name, "lang": lang, "cls": cls, "mode": mode,
                                            "terms": build_terms(name), "multitoken": bool(_CAMEL.search(name) or "_" in name)})
            finally:
                sess.close()
            conn.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    def report(label, b):
        n = b["n"] or 1
        print(f"{label:11s} n={b['n']:4d} | rank1={b['rank1']/n:.3f}  rank2-10={b['rank2_10']/n:.3f}  "
              f"rank11-{args.top_k}={b['rank11_topk']/n:.3f}  absent={b['absent']/n:.3f}")
        a = b["absent"] or 1
        print(f"            absent breakdown: guard_killed={b['guard_killed']/n:.3f} "
              f"fts_has_gold={b['fts_has_gold']/n:.3f} fts_no_gold={b['fts_no_gold']/n:.3f}  "
              f"(of absent: guard={b['guard_killed']/a:.0%} has_gold={b['fts_has_gold']/a:.0%} no_gold={b['fts_no_gold']/a:.0%})")

    print("\n=== exact-symbol retrieval-drop diagnostic (real binary, top_k=%d) ===" % args.top_k)
    report("OVERALL", overall)
    for lang in sorted(by_lang):
        report(lang, by_lang[lang])
    print("\n--- absent samples (q -> class | FTS terms | multitoken) ---")
    for s in samples:
        print(f"  {s['q']:32.32} [{s['lang']:10.10}] {s['cls']:13} mode={s['mode']:12} "
              f"multitoken={int(s['multitoken'])} terms={s['terms']}")


if __name__ == "__main__":
    main()
