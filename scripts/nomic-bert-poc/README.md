# nomic-bert on Candle — feasibility PoC

Proves that **`nomic-ai/CodeRankEmbed`** (architecture `nomic-bert-2048`) runs on our
**Candle/Rust** embedding stack and matches PyTorch numerically. This is the gating
feasibility question behind adopting a code-specialized embedding model.

## Why this exists

`scripts/embedding_benchmark` measured CodeRankEmbed at **+2.49pp NDCG@10**
(0.8917 vs all-MiniLM 0.8668) on our query set — the first model to beat the
production embedding. But CodeRankEmbed is a custom architecture and
**`candle-transformers` has no nomic-bert implementation**, and our standard
`bert.rs` path (absolute positions + plain FFN) can't run it. So: can we run it
on Candle at all? This PoC answers yes.

## Result

`src/main.rs` is a ~180-line forward written with **candle-nn primitives only**
(no `candle-transformers`): it loads the real safetensors and reproduces the model
forward. Versus PyTorch `sentence-transformers`:

```
WORST cosine = 1.000000  ->  PASS (numerically equivalent)   # 5/5 sentences
```

## Architecture details that must hold in any production port

- **SwiGLU MLP**: `fc11(x) * silu(fc12(x)) -> fc2` — `fc12` is the gated side (counter-intuitive), all no-bias.
- **RoPE**: `base=1000` (not 10000), full head_dim, non-interleaved → `candle_nn::rotary_emb::rope` matches.
- **Postnorm**: `h = norm1(attn(h) + h)`; `h = norm2(mlp(h) + h)` (LayerNorm with bias).
- **Pooling**: CLS / token-0 (`1_Pooling/config.json: pooling_mode_cls_token=true`, **not** mean).
- **Attention**: fused `Wqkv` no-bias, scale `1/sqrt(head_dim)`, bidirectional (no causal mask).
- **Embeddings**: `word + token_type[0]`, then `emb_ln`; no position-embedding tensor (RoPE).

## Run

Prereq: a local CodeRankEmbed HF cache. Easiest:
```bash
../embedding_benchmark/.venv/bin/python -c \
  "from sentence_transformers import SentenceTransformer as S; S('nomic-ai/CodeRankEmbed', trust_remote_code=True)"
```
Then:
```bash
cargo run --release                                   # writes rust_emb.json
../embedding_benchmark/.venv/bin/python compare.py    # prints per-sentence cosine
```
Model dir resolution: `argv[1]` > `$CODERANK_DIR` > first HF-cache snapshot.

## Status / not-production

Single-sentence, CPU, f32, document mode (no query prefix). A production
`src/embedding/nomic_bert.rs` still needs: batch + padding attention mask, GPU
path, asymmetric query prefix (`"Represent this query for searching relevant code: "`),
the 384→768 embedding-dim migration, and `EmbeddingModel` integration — all
engineering, not feasibility. Whether to do it is a cost/benefit call (see the
`project_cocoindex_competitive_analysis` memory): +2.49pp vs ~1-day port +
breaking re-index + ~6× CPU inference. `trust_remote_code` downloads/executes the
model's custom Python — fine for this dev PoC.
