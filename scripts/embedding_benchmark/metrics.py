"""Pure ranking-quality metrics. No I/O, no model deps — unit-tested in isolation."""
import math


def dcg(relevances: list[float]) -> float:
    """Discounted cumulative gain. relevances are in ranked order (rank 0 first)."""
    return sum(rel / math.log2(i + 2) for i, rel in enumerate(relevances))


def ndcg_at_k(ranked_ids: list[int], gold_to_rel: dict[int, float], k: int) -> float:
    """NDCG@k. gold_to_rel maps a relevant node_id to its graded relevance (1.0 = binary)."""
    ranked_rels = [gold_to_rel.get(nid, 0.0) for nid in ranked_ids[:k]]
    ideal_rels = sorted(gold_to_rel.values(), reverse=True)[:k]
    idcg = dcg(ideal_rels)
    return dcg(ranked_rels) / idcg if idcg > 0 else 0.0


def recall_at_k(ranked_ids: list[int], gold_ids: list[int], k: int) -> float:
    """Fraction of gold ids present in the top-k."""
    if not gold_ids:
        return 0.0
    top = set(ranked_ids[:k])
    return len(top & set(gold_ids)) / len(gold_ids)


def reciprocal_rank(ranked_ids: list[int], gold_ids: list[int]) -> float:
    """1/(rank of first gold hit), 0 if none. Rank is 1-based."""
    gold = set(gold_ids)
    for i, nid in enumerate(ranked_ids):
        if nid in gold:
            return 1.0 / (i + 1)
    return 0.0
