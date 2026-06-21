"""Ground-truth check: PyTorch sentence-transformers vs the Candle PoC. Cosine should be ~1.0.

Run after `cargo run --release` (which writes rust_emb.json in this dir):
    ../embedding_benchmark/.venv/bin/python compare.py
(any venv with sentence-transformers + numpy works)
"""
import json
import numpy as np
from sentence_transformers import SentenceTransformer

# MUST stay byte-identical to src/main.rs SENTENCES
SENTENCES = [
    "def quicksort(arr): return sorted(arr)",
    "read a file from disk and return its contents",
    "rotary position embeddings for transformers",
    "fn main() { println!(\"hello world\"); }",
    "binary search over a sorted array",
]

m = SentenceTransformer("nomic-ai/CodeRankEmbed", trust_remote_code=True, device="cpu")
py = np.asarray(m.encode(SENTENCES, show_progress_bar=False), dtype=np.float32)  # document mode (no prompt)
rust = np.asarray(json.load(open("rust_emb.json")), dtype=np.float32)

def cos(a, b):
    return float(a @ b / (np.linalg.norm(a) * np.linalg.norm(b)))

print(f"{'cosine':>10}  sentence")
worst = 1.0
for i, s in enumerate(SENTENCES):
    c = cos(rust[i], py[i])
    worst = min(worst, c)
    print(f"{c:10.6f}  {s[:55]}")
print(f"\nWORST cosine = {worst:.6f}  ->  {'PASS (numerically equivalent)' if worst > 0.999 else 'FAIL'}")
