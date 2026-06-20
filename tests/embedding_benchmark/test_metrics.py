import math
import sys, pathlib
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "scripts" / "embedding_benchmark"))
from metrics import dcg, ndcg_at_k, recall_at_k, reciprocal_rank


def test_dcg_basic():
    # rel=1 at rank0 (log2(2)=1), rel=1 at rank1 (log2(3))
    assert math.isclose(dcg([1.0, 1.0]), 1.0 + 1.0 / math.log2(3))


def test_ndcg_gold_at_top_is_one():
    assert math.isclose(ndcg_at_k([5, 1, 2], {5: 1.0}, 10), 1.0)


def test_ndcg_gold_at_rank3():
    # single gold at index 2 (rank 3): DCG = 1/log2(4)=0.5, IDCG=1.0
    assert math.isclose(ndcg_at_k([1, 2, 5], {5: 1.0}, 10), 0.5)


def test_ndcg_gold_outside_k_is_zero():
    assert ndcg_at_k([1, 2, 3], {5: 1.0}, 3) == 0.0


def test_recall_at_k_hit_and_miss():
    assert recall_at_k([1, 2, 5], [5], 3) == 1.0
    assert recall_at_k([1, 2, 3], [5], 3) == 0.0
    assert recall_at_k([1, 2, 5, 9], [5, 9], 4) == 1.0


def test_reciprocal_rank():
    assert reciprocal_rank([1, 2, 5], [5]) == 1.0 / 3.0
    assert reciprocal_rank([1, 2, 3], [5]) == 0.0
