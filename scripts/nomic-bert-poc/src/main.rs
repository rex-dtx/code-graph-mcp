// Candle reimplementation of nomic-bert (CodeRankEmbed) — feasibility PoC.
//
// Proves the code-specialized model that scored +2.49pp NDCG@10 in
// scripts/embedding_benchmark CAN run on our Candle/Rust stack (candle-transformers
// has NO nomic-bert). Forward written with candle-nn primitives only; matches
// PyTorch sentence-transformers at cosine=1.0 on 5/5 sentences (see compare.py).
//
// Forward semantics verified line-by-line against modeling_hf_nomic_bert.py:
//   embeddings(word + token_type[0]) -> emb_ln -> 12x postnorm block
//   block: h = norm1(attn(h) + h); h = norm2(mlp(h) + h)        [prenorm=false]
//   attn:  fused Wqkv (no bias) -> RoPE(base=1000, full head_dim, non-interleaved)
//          on q,k -> scores = q@k^T / sqrt(head_dim) -> softmax -> @v -> out_proj (no bias)
//   mlp (SwiGLU): fc11(x) * silu(fc12(x)) -> fc2                 [all no bias; fc12 is gated]
//   pooling: CLS (token 0)   [1_Pooling/config.json: pooling_mode_cls_token=true, NOT mean]
//
// NOT production: single-sentence (no batch/padding mask), CPU, f32, no query prefix.
use anyhow::Result;
use candle_core::{DType, Device, Tensor, D};
use candle_nn::VarBuilder;
use tokenizers::Tokenizer;

const H: usize = 768;
const NH: usize = 12;
const HD: usize = 64;
const NL: usize = 12;
const INNER: usize = 3072;
const VOCAB: usize = 30528;
const EPS: f64 = 1e-12;
const BASE: f64 = 1000.0;

// SENTENCES — MUST stay byte-identical to compare.py
const SENTENCES: [&str; 5] = [
    "def quicksort(arr): return sorted(arr)",
    "read a file from disk and return its contents",
    "rotary position embeddings for transformers",
    "fn main() { println!(\"hello world\"); }",
    "binary search over a sorted array",
];

/// Resolve the CodeRankEmbed model dir: argv[1] > $CODERANK_DIR > first HF-cache snapshot.
/// (The snapshot sha changes across model revisions, so we glob rather than hardcode it.)
fn resolve_model_dir() -> Result<String> {
    if let Some(a) = std::env::args().nth(1) {
        return Ok(a);
    }
    if let Ok(d) = std::env::var("CODERANK_DIR") {
        return Ok(d);
    }
    let home = std::env::var("HOME")?;
    let base = format!("{home}/.cache/huggingface/hub/models--nomic-ai--CodeRankEmbed/snapshots");
    let dir = std::fs::read_dir(&base)
        .map_err(|e| anyhow::anyhow!("{base}: {e} — download the model first (see README)"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .ok_or_else(|| anyhow::anyhow!("no snapshot under {base}"))?;
    Ok(dir.to_string_lossy().into_owned())
}

fn layer_norm(x: &Tensor, w: &Tensor, b: &Tensor) -> Result<Tensor> {
    let mean = x.mean_keepdim(D::Minus1)?;
    let xc = x.broadcast_sub(&mean)?;
    let var = xc.sqr()?.mean_keepdim(D::Minus1)?;
    let xn = xc.broadcast_div(&var.affine(1.0, EPS)?.sqrt()?)?;
    Ok(xn.broadcast_mul(w)?.broadcast_add(b)?)
}

// x (seq, in) @ w^T  where w is (out, in)  -> (seq, out)
fn linear(x: &Tensor, w: &Tensor) -> Result<Tensor> {
    Ok(x.matmul(&w.t()?)?)
}

struct Layer {
    wqkv: Tensor,
    op: Tensor,
    fc11: Tensor,
    fc12: Tensor,
    fc2: Tensor,
    n1w: Tensor,
    n1b: Tensor,
    n2w: Tensor,
    n2b: Tensor,
}

fn main() -> Result<()> {
    let model_dir = resolve_model_dir()?;
    eprintln!("[poc] model dir: {model_dir}");
    let dev = Device::Cpu;
    let tok =
        Tokenizer::from_file(format!("{model_dir}/tokenizer.json")).map_err(anyhow::Error::msg)?;
    let vb = unsafe {
        VarBuilder::from_mmaped_safetensors(
            &[format!("{model_dir}/model.safetensors")],
            DType::F32,
            &dev,
        )?
    };

    let word = vb.get((VOCAB, H), "embeddings.word_embeddings.weight")?;
    let tte = vb.get((2, H), "embeddings.token_type_embeddings.weight")?;
    let eln_w = vb.get((H,), "emb_ln.weight")?;
    let eln_b = vb.get((H,), "emb_ln.bias")?;

    let mut layers = Vec::new();
    for i in 0..NL {
        let p = format!("encoder.layers.{i}");
        layers.push(Layer {
            wqkv: vb.get((3 * H, H), &format!("{p}.attn.Wqkv.weight"))?,
            op: vb.get((H, H), &format!("{p}.attn.out_proj.weight"))?,
            fc11: vb.get((INNER, H), &format!("{p}.mlp.fc11.weight"))?,
            fc12: vb.get((INNER, H), &format!("{p}.mlp.fc12.weight"))?,
            fc2: vb.get((H, INNER), &format!("{p}.mlp.fc2.weight"))?,
            n1w: vb.get((H,), &format!("{p}.norm1.weight"))?,
            n1b: vb.get((H,), &format!("{p}.norm1.bias"))?,
            n2w: vb.get((H,), &format!("{p}.norm2.weight"))?,
            n2b: vb.get((H,), &format!("{p}.norm2.bias"))?,
        });
    }

    // inv_freq[j] = 1 / base^((2j)/head_dim), j in 0..head_dim/2
    let inv_freq: Vec<f32> = (0..HD / 2)
        .map(|j| (1.0 / BASE.powf((2 * j) as f64 / HD as f64)) as f32)
        .collect();

    let mut out: Vec<Vec<f32>> = Vec::new();
    for s in SENTENCES.iter() {
        let enc = tok.encode(*s, true).map_err(anyhow::Error::msg)?;
        let ids: Vec<u32> = enc.get_ids().to_vec();
        let seq = ids.len();
        let ids_t = Tensor::from_vec(ids, (seq,), &dev)?;

        // embeddings: word + token_type[0], then emb_ln
        let mut h = word.index_select(&ids_t, 0)?; // (seq, 768)
        let tt0 = tte.narrow(0, 0, 1)?; // (1, 768)
        h = h.broadcast_add(&tt0)?;
        h = layer_norm(&h, &eln_w, &eln_b)?;

        // cos/sin (seq, head_dim/2)
        let mut fv = Vec::with_capacity(seq * HD / 2);
        for t in 0..seq {
            for j in 0..HD / 2 {
                fv.push(t as f32 * inv_freq[j]);
            }
        }
        let freqs = Tensor::from_vec(fv, (seq, HD / 2), &dev)?;
        let cos = freqs.cos()?;
        let sin = freqs.sin()?;

        for l in &layers {
            let qkv = linear(&h, &l.wqkv)?; // (seq, 2304)
            let q = qkv.narrow(1, 0, H)?;
            let k = qkv.narrow(1, H, H)?;
            let v = qkv.narrow(1, 2 * H, H)?;
            let to4 = |t: &Tensor| -> Result<Tensor> {
                Ok(t.reshape((1, seq, NH, HD))?.transpose(1, 2)?.contiguous()?) // (1,12,seq,64)
            };
            let q = candle_nn::rotary_emb::rope(&to4(&q)?, &cos, &sin)?;
            let k = candle_nn::rotary_emb::rope(&to4(&k)?, &cos, &sin)?;
            let v = to4(&v)?;
            let scores = q
                .matmul(&k.transpose(2, 3)?.contiguous()?)?
                .affine(1.0 / (HD as f64).sqrt(), 0.0)?; // (1,12,seq,seq)
            let probs = candle_nn::ops::softmax_last_dim(&scores)?;
            let ctx = probs.matmul(&v)?; // (1,12,seq,64)
            let ctx = ctx.transpose(1, 2)?.contiguous()?.reshape((seq, H))?;
            let attn = linear(&ctx, &l.op)?;
            h = layer_norm(&attn.add(&h)?, &l.n1w, &l.n1b)?;

            // SwiGLU: fc11(h) * silu(fc12(h)) -> fc2
            let up = linear(&h, &l.fc11)?;
            let gate = linear(&h, &l.fc12)?;
            let silu = gate.mul(&candle_nn::ops::sigmoid(&gate)?)?;
            let mlp = linear(&up.mul(&silu)?, &l.fc2)?;
            h = layer_norm(&mlp.add(&h)?, &l.n2w, &l.n2b)?;
        }

        let cls: Vec<f32> = h.narrow(0, 0, 1)?.reshape((H,))?.to_vec1::<f32>()?;
        out.push(cls);
    }

    std::fs::write("rust_emb.json", serde_json::to_string(&out)?)?;
    println!(
        "wrote {} embeddings ({} dims each) -> rust_emb.json",
        out.len(),
        out[0].len()
    );
    Ok(())
}
