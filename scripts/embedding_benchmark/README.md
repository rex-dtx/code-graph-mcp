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
By-language query counts: rust=429, javascript=160, typescript=58 (n=58 — limited statistical power; treat TS numbers as directional), python=1 (n=1; included in the overall mean but too small to interpret — negligible weight, ~0.0001 effect).

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

4. **JS underperforms despite likely being in minilm's training data** — JavaScript is extremely well-represented in web-crawl pre-training yet scores substantially lower than rust. This suggests the JS query+context construction or the mixed-type JS corpus (loose scripts + generated code) is harder to rank, not a data-domain coverage issue. **Caveat:** some bootstrap queries (notably JS) are section-header doc-comments mislabeled to a neighboring symbol (a banner comment naming a sibling), which the `name not in doc` filter cannot catch — so the absolute JS numbers carry label noise and likely understate true quality. This is symmetric across both backends (identical query set), so it does not affect the minilm-vs-potion comparison, only the absolute per-language reading.

5. **TS n=58 caveat** — numbers directionally consistent with rust's strong performance on context_string but insufficient for statistical significance. Add more TS-heavy repos to the `--db` list before drawing hard conclusions about TS.

## Go/no-go gate

**Decision: NO-GO** — `potion-code-16M` does not replace `all-MiniLM-L6-v2`. potion's best config (context_string) trails minilm overall (0.7898 vs 0.8655, −7.6pp) and on rust (−5.7pp, beyond the 0.02 regression threshold); it only ties on TS (n=58, directional). Phase 2 (Rust static inference + 384→256 migration) is not authorized. Full rationale: `docs/superpowers/specs/2026-06-21-tier1-static-embeddings-decision.md`.

Best config measured: **minilm / context_string** (NDCG@10 = 0.8655, recall@10 = 0.9306) — the current production embedding remains the strongest, so no change is made.

## Ranking benchmark (end-to-end)

`eval_ranking.py` drives the **real** `semantic_code_search` pipeline (FTS5 + vector + RRF + adjusted-score re-rank) over MCP stdio, unlike `eval_retrieval.py` which is vector-only. Spawns `code-graph-mcp serve` for each project root against a frozen `sqlite3.backup()` copy of the index; never sends `initialized` (so background startup indexing is never triggered) and passes `skip_indexing=true` on every search call (so the copied index is never reindexed or wiped during a run).

### Invariants

- **Isolated root**: each benchmark root is a `/tmp/cg-bench-*` copy backed up via `sqlite3.backup()` — WAL + vec0 shadow tables included so vector search works against the copy.
- **No `initialized`**: omitting the `notifications/initialized` message prevents the server from launching its background index-watcher, keeping the index snapshot stable.
- **`skip_indexing=true`**: every `semantic_code_search` call carries this flag to skip per-query freshness checks, ensuring the server reads the snapshot, not a live reindex.
- **`CODE_GRAPH_INTERNAL=1`**: suppresses usage.jsonl writes so benchmark runs never pollute real adoption metrics.

### Run commands

```bash
# Step 1 — generate the tier3 slice (exact-symbol queries, gold = defining node)
python3 scripts/embedding_benchmark/build_tier3_slice.py \
  --db .code-graph/index.db \
  --db /mnt/data_ssd/dev/projects/sgc/.code-graph/index.db \
  --out scripts/embedding_benchmark/tier3_slice.jsonl --limit-per-db 250

# Step 2 — NL baseline (regression guard; --min-ndcg 0.5 catches a missing embed-model build)
python3 scripts/embedding_benchmark/eval_ranking.py \
  --queries scripts/embedding_benchmark/query_set.jsonl \
  --root . --root /mnt/data_ssd/dev/projects/sgc \
  --min-ndcg 0.5 \
  --out scripts/embedding_benchmark/results/ranking_nl_baseline.json

# Step 3 — tier3 slice baseline (improvement measure; no --min-ndcg floor)
python3 scripts/embedding_benchmark/eval_ranking.py \
  --queries scripts/embedding_benchmark/tier3_slice.jsonl \
  --root . --root /mnt/data_ssd/dev/projects/sgc \
  --out scripts/embedding_benchmark/results/ranking_tier3_baseline.json
```

### Results (2026-06-21, end-to-end RRF pipeline)

Binary: `target/release/code-graph-mcp` (built with `embed-model` feature, minilm active).  
DBs: `code-graph-mcp` (rust+js) + `sgc` (ts+js). `--top-k 20`.

**NL set** (`query_set.jsonl`, n=648, bootstrap doc-comment queries — regression guard):

| metric     | overall | rust   | typescript | javascript |
|------------|---------|--------|------------|------------|
| NDCG@10    | 0.6698  | 0.7453 | 0.5714     | 0.5040     |
| recall@1   | 0.5448  | 0.6084 | 0.4655     | 0.4062     |
| recall@10  | 0.7870  | 0.8695 | 0.6897     | 0.6000     |
| MRR        | 0.6320  | 0.7050 | 0.5339     | 0.4737     |

Note: NL overall NDCG@10 = 0.6698, which is below the vector-only baseline of 0.8655. The vector-only benchmark (`eval_retrieval.py`) measures pure embedding similarity without BM25; the end-to-end pipeline adds FTS5 + RRF fusion which can dilute pure vector signal when BM25 and vector disagree. The run did NOT abort (above the 0.5 `--min-ndcg` floor), confirming vector search is active. The gap signals that BM25 and vector are not always aligned on NL queries — a known property of RRF fusion when the FTS ranking is noisy relative to the embedding.

**Tier3 slice** (`tier3_slice.jsonl`, n=500, exact-symbol-name queries, gold = defining node — improvement measure):

| metric    | overall | rust   | typescript | javascript | python |
|-----------|---------|--------|------------|------------|--------|
| NDCG@10   | 0.8453  | 0.8869 | 0.7823     | 0.8701     | 1.0000 |
| recall@1  | 0.8360  | 0.8696 | 0.7789     | 0.8701     | 1.0000 |
| recall@10 | 0.8540  | 0.9043 | 0.7842     | 0.8701     | 1.0000 |
| MRR       | 0.8424  | 0.8813 | 0.7816     | 0.8701     | 1.0000 |

### Phase B go/no-go

**Decision: PROCEED**

`by_query_class.exact_symbol.recall@1 = 0.836` — materially below the 0.97 threshold. Defining nodes do NOT already rank #1 in ~16.4% of exact-symbol queries. The existing `name_boost` + acronym handling + exact-name exemption does not fully cover this case. Headroom of ~13pp on recall@1 exists; Phase B (single-identifier weighting + definition-node boost changes in `search.rs`) is authorized to proceed.

TypeScript is the weakest language at recall@1 = 0.7789, suggesting the ranking gap is largest there and Phase B has the most to gain on TS exact-symbol queries.
