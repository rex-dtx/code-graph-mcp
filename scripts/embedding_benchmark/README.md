# Embedding Retrieval Benchmark

Offline A/B of code embeddings for `semantic_code_search`. Vector-only ranking
(isolates the embedding variable). Gate for the spec's Phase 2 Rust work:
`docs/superpowers/specs/2026-06-21-tier1-static-embeddings-design.md`.

## Prerequisites

- One or more repos indexed with `code-graph-mcp rebuild-index` (gives `.code-graph/index.db`).
- For TS coverage, index a TS-heavy project and pass an extra `--db`.
  TS project used: `/mnt/data_ssd/dev/projects/sgc` (ts+js).
- Install Python deps: `pip install -r requirements.txt`

## Run

```bash
# Regenerate labeled query set (stdlib-only — system python3 works)
python3 build_query_set.py \
  --db .code-graph/index.db \
  --db /path/to/ts-project/.code-graph/index.db \
  --real real_queries.jsonl \
  --out query_set.jsonl

# Evaluate one cell of the 2x2 matrix (venv python required — ML deps)
.venv/bin/python eval_retrieval.py \
  --backend {minilm,potion} \
  --field {context_string,code_content} \
  --db .code-graph/index.db \
  [--db extra.db ...] \
  --queries query_set.jsonl \
  --out results/<backend>_<field>.json
```

To run the full 2x2 matrix:

```bash
Q=query_set.jsonl
D="--db .code-graph/index.db --db /path/to/ts-project/.code-graph/index.db"
for backend in minilm potion; do
  for field in context_string code_content; do
    .venv/bin/python eval_retrieval.py --backend $backend --field $field $D \
      --queries $Q --out results/${backend}_${field}.json
  done
done
```

## Results (2026-06-21, query_set n=648, candidates=5879)

DBs: `code-graph-mcp` (rust+js) + `sgc` (ts+js).  
By-language query counts: rust=429, javascript=160, typescript=58 (n=58 — limited statistical power; treat TS numbers as directional), python=1 (excluded from analysis).

| backend | field          | NDCG@10 (overall) | rust   | typescript | javascript | recall@1 | recall@10 |
|---------|----------------|-------------------|--------|------------|------------|----------|-----------|
| minilm  | context_string | 0.8655            | 0.9355 | 0.8486     | 0.6890     | 0.8025   | 0.9306    |
| minilm  | code_content   | 0.4394            | 0.5458 | 0.2936     | 0.2097     | 0.2685   | 0.6235    |
| potion  | context_string | 0.7898            | 0.8782 | 0.8456     | 0.5312     | 0.7099   | 0.8673    |
| potion  | code_content   | 0.3804            | 0.4622 | 0.2796     | 0.1980     | 0.2500   | 0.5247    |

**Baseline**: minilm / context_string (NDCG@10 = 0.8655).  
**Potion's best field**: context_string (NDCG@10 = 0.7898 vs 0.3804 for code_content).

## Key findings

1. **context_string dominates code_content for both backends** — the gap is large (minilm: 0.8655 vs 0.4394; potion: 0.7898 vs 0.3804). The spec §0.4 field choice is answered: use `context_string`.

2. **minilm beats potion overall** (0.8655 vs 0.7898, −7.6pp). The gap is consistent across rust (0.9355 vs 0.8782) and javascript (0.6890 vs 0.5312).

3. **Rust > TS > JS by NDCG@10 (despite JS's heavy presence in web-crawl pre-training)** — the rust gap over JS is large for both backends on context_string (minilm: 0.9355 vs 0.6890; potion: 0.8782 vs 0.5312). TS lands between rust and JS despite n=58 being a small sample. (None of the three is fine-tuned for this task; the surprise is that JS, the most pre-training-abundant language, ranks lowest.)

4. **JS underperforms despite likely being in minilm's training data** — JavaScript is extremely well-represented in web-crawl pre-training yet scores substantially lower than rust. This suggests the JS query+context construction or the mixed-type JS corpus (loose scripts + generated code) is harder to rank, not a data-domain coverage issue.

5. **TS n=58 caveat** — numbers directionally consistent with rust's strong performance on context_string but insufficient for statistical significance. Add more TS-heavy repos to the `--db` list before drawing hard conclusions about TS.

## Go/no-go gate

**Decision: NO-GO** — `potion-code-16M` does not replace `all-MiniLM-L6-v2`. potion's best config (context_string) trails minilm overall (0.7898 vs 0.8655, −7.6pp) and on rust (−5.7pp, beyond the 0.02 regression threshold); it only ties on TS (n=58, directional). Phase 2 (Rust static inference + 384→256 migration) is not authorized. Full rationale: `docs/superpowers/specs/2026-06-21-tier1-static-embeddings-decision.md`.

Best config measured: **minilm / context_string** (NDCG@10 = 0.8655, recall@10 = 0.9306) — the current production embedding remains the strongest, so no change is made.
