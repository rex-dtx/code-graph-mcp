# scripts/embedding_benchmark/eval_retrieval.py
"""Evaluate one embedding backend on the labeled query set (vector-only ranking).

Usage:
  python eval_retrieval.py --backend minilm  --field context_string \
      --db .code-graph/index.db --queries query_set.jsonl --out results/minilm_context.json
  python eval_retrieval.py --backend potion  --field code_content \
      --db .code-graph/index.db --queries query_set.jsonl --out results/potion_code.json
"""
import argparse
import json
import os
import sqlite3
import numpy as np

from metrics import ndcg_at_k, recall_at_k, reciprocal_rank

MINILM_ID = "sentence-transformers/all-MiniLM-L6-v2"
MINILM_REV = "c9745ed1d9f2"  # must match src/embedding/model.rs:75
POTION_ID = "minishlab/potion-code-16M"


def l2(mat: np.ndarray) -> np.ndarray:
    norms = np.linalg.norm(mat, axis=1, keepdims=True)
    norms[norms == 0] = 1.0
    return mat / norms


class Backend:
    def __init__(self, name: str):
        self.name = name
        if name == "minilm":
            from sentence_transformers import SentenceTransformer
            self.model = SentenceTransformer(MINILM_ID, revision=MINILM_REV)
            self._encode = lambda texts: np.asarray(
                self.model.encode(texts, batch_size=64, show_progress_bar=False), dtype=np.float32)
        elif name == "potion":
            from model2vec import StaticModel
            self.model = StaticModel.from_pretrained(POTION_ID)
            self._encode = lambda texts: np.asarray(self.model.encode(texts), dtype=np.float32)
        else:
            raise SystemExit(f"unknown backend {name!r}")

    def encode(self, texts: list[str]) -> np.ndarray:
        if not texts:
            return np.zeros((0, 1), dtype=np.float32)
        return l2(self._encode(texts))  # explicit L2 — matches the Rust l2_normalize path


def load_candidates(dbs: list[str], field: str):
    """Return (global_ids, texts) for all non-test symbols across the DBs."""
    ids, texts = [], []
    for db_idx, db_path in enumerate(dbs):
        conn = sqlite3.connect(db_path)
        conn.row_factory = sqlite3.Row
        cur = conn.execute(
            f"""SELECT n.id, n.{field} AS text
                FROM nodes n JOIN files f ON n.file_id = f.id
                WHERE n.is_test = 0 AND f.language IS NOT NULL"""
        )
        for r in cur:
            ids.append(db_idx * 10_000_000 + int(r["id"]))
            texts.append((r["text"] or "")[:2000])  # cap to keep runtime bounded
        conn.close()
    return ids, texts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--backend", choices=["minilm", "potion"], required=True)
    ap.add_argument("--field", choices=["context_string", "code_content"], required=True)
    ap.add_argument("--db", action="append", required=True)
    ap.add_argument("--queries", default="query_set.jsonl")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    ids, texts = load_candidates(args.db, args.field)

    backend = Backend(args.backend)
    print(f"[eval] encoding {len(texts)} candidates with {args.backend}/{args.field}...")
    cand = backend.encode(texts)  # (N, dim), L2-normalized

    queries = []
    with open(args.queries) as fh:
        for line in fh:
            line = line.strip()
            if line:
                queries.append(json.loads(line))

    q_texts = [q["query"] for q in queries]
    q_emb = backend.encode(q_texts)  # (Q, dim), L2-normalized

    # Per-query: cosine == dot product on normalized vectors. Rank candidates, score.
    per_lang: dict[str, list[dict]] = {}
    overall: list[dict] = []
    for qi, q in enumerate(queries):
        sims = cand @ q_emb[qi]               # (N,)
        # stable sort by (-score, id): argsort on score desc, ties broken by id asc
        order = np.lexsort((np.array(ids), -sims))
        ranked = [ids[p] for p in order]
        gold = q["gold_node_ids"]
        gold_to_rel = {g: 1.0 for g in gold}
        rec = {
            "ndcg@10": ndcg_at_k(ranked, gold_to_rel, 10),
            "recall@1": recall_at_k(ranked, gold, 1),
            "recall@10": recall_at_k(ranked, gold, 10),
            "mrr": reciprocal_rank(ranked, gold),
        }
        overall.append(rec)
        per_lang.setdefault(q["language"], []).append(rec)

    def agg(rows: list[dict]) -> dict:
        if not rows:
            return {"n": 0}
        keys = ["ndcg@10", "recall@1", "recall@10", "mrr"]
        out = {k: round(float(np.mean([r[k] for r in rows])), 4) for k in keys}
        out["n"] = len(rows)
        return out

    result = {
        "backend": args.backend,
        "field": args.field,
        "candidates": len(ids),
        "overall": agg(overall),
        "by_language": {lg: agg(rows) for lg, rows in sorted(per_lang.items())},
    }
    out_dir = os.path.dirname(args.out)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    with open(args.out, "w") as fh:
        json.dump(result, fh, indent=2)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
