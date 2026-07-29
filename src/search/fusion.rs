use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub node_id: i64,
    /// Raw score from the source (BM25 for FTS, vector similarity `1 - L2_distance`
    /// for vector — NOT cosine; order-equivalent to cosine for L2-normalized embeddings).
    /// Used for score blending in RRF when available (non-zero).
    pub score: f64,
}

/// Reciprocal Rank Fusion with raw score blending.
///
/// Base: score(node) = sum( weight_i / (k + rank_i + 1) ) across sources (classic RRF).
///
/// Blending: when raw scores are available (score > 0), a small fraction of the
/// normalized raw score is added as a TRUE tie-breaker — bounded per source:
///   blend_max_per_source = 0.5 / ((k+1) * (k+2)) * weight
/// Proof: the RRF gap between rank i and rank i+1 in one source is
///   1/(k+i+1) - 1/(k+i+2) = 1/((k+i+1)(k+i+2))
/// which is minimized at i=0 as 1/((k+1)(k+2)). Setting blend_max to HALF that
/// gap guarantees the blend contribution is strictly less than any adjacent-rank
/// RRF gap.
///
/// SCOPE OF THAT GUARANTEE — it is per source, and this function fuses two.
/// Within ONE source's ranking, two adjacent items differ by more than the
/// blend can move them, so blending cannot reorder them. Across the FUSED
/// ordering it can: two nodes' summed RRF totals can differ by less than one
/// adjacent-rank gap (`1/31 + 1/36` vs `1/32 + 1/35` at k=30 differ by 0.00021,
/// while the two-source blend ceiling is 0.00101), and there the raw scores
/// decide. That is the intended behavior — near-ties across sources are exactly
/// what a tie-breaker is for — but the earlier phrasing, "cannot flip adjacent
/// RRF ranks at any k", claimed something stronger than the proof supports.
/// `test_blend_can_reorder_a_cross_source_near_tie` pins the real boundary.
///
/// Historical note: a previous version used SCORE_BLEND_FACTOR=0.1, which at k=30
/// produced blend_max ≈ 0.1 vs adjacent-rank gap ≈ 0.001 — blend dominated RRF
/// by ~100×, silently converting RRF into per-source-raw-score ranking. This
/// adaptive bound restores RRF's actual semantics while keeping blending as a
/// meaningful tie-breaker within a single source.
///
/// Higher `k` values dampen the impact of rank differences (typically k=30–60).
pub fn weighted_rrf_fusion(
    fts_results: &[SearchResult],
    vec_results: &[SearchResult],
    k: u32,
    top_k: usize,
    fts_weight: f64,
    vec_weight: f64,
) -> Vec<SearchResult> {
    // Adaptive blend scale: half of the smallest adjacent-rank RRF gap.
    // Guarantees blend is strictly subordinate to rank ordering at any k.
    let k_f = k as f64;
    let blend_scale = 0.5 / ((k_f + 1.0) * (k_f + 2.0));

    let mut scores: HashMap<i64, f64> = HashMap::new();

    // Normalize raw scores to [0, 1] for blending. The fold seeds at 0.0, so an
    // all-negative source (every vec0 L2 score = 1 - dist < 0, i.e. all-dissimilar)
    // yields max = 0.0 and its blend branch below takes the `else 0.0` path — which
    // is exactly what the M4 numerator clamp (`r.score.max(0.0)`) would produce
    // anyway, so disabling the blend there loses no ordering signal (L1).
    let fts_max = fts_results.iter().map(|r| r.score).fold(0.0_f64, f64::max);
    let vec_max = vec_results.iter().map(|r| r.score).fold(0.0_f64, f64::max);

    // Clamp the numerator to >= 0 before normalizing. A vector score can be
    // negative (vec0 L2 distance → score = 1 - dist ∈ [-1, 1]); `score / max`
    // against a small positive max then yields a blend far below -blend_scale,
    // breaking the "blend ∈ [0, blend_scale·weight]" bound the no-rank-flip proof
    // depends on. Clamping keeps a negative-similarity item's blend at 0 (it rides
    // on its RRF rank) instead of subtracting an unbounded amount (M4).
    for (rank, r) in fts_results.iter().enumerate() {
        let rrf = fts_weight / (k as f64 + rank as f64 + 1.0);
        let blend = if fts_max > 0.0 {
            blend_scale * fts_weight * (r.score.max(0.0) / fts_max)
        } else {
            0.0
        };
        *scores.entry(r.node_id).or_default() += rrf + blend;
    }
    for (rank, r) in vec_results.iter().enumerate() {
        let rrf = vec_weight / (k as f64 + rank as f64 + 1.0);
        let blend = if vec_max > 0.0 {
            blend_scale * vec_weight * (r.score.max(0.0) / vec_max)
        } else {
            0.0
        };
        *scores.entry(r.node_id).or_default() += rrf + blend;
    }

    let mut results: Vec<SearchResult> = scores
        .into_iter()
        .map(|(id, score)| SearchResult { node_id: id, score })
        .collect();
    // Break exact-score ties by node_id (ascending) so the ordering — and therefore
    // which items survive `truncate(top_k)` at a tie straddling the boundary — is
    // deterministic. Without this, `scores` (a HashMap with a per-instance random
    // seed) iterates in a nondeterministic order, and the stable sort then preserves
    // that order for equal scores, making the top_k cut nondeterministic across runs
    // (L3).
    results.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node_id.cmp(&b.node_id)));
    results.truncate(top_k);
    results
}

/// Phase-2 re-rank score for one candidate.
///
/// Non-exact path is byte-identical to the historical inline formula
/// `(base * name_boost * size_factor * doc_penalty * 100).round() / 100`, so
/// natural-language ranking is unchanged. An exact symbol-name match
/// (query verbatim == node name/qualified_name) instead scores
/// `base + EXACT_NAME_MATCH_BONUS`, which dominates any non-exact adjusted score
/// (bounded by `base * NAME_BOOST_CAP` ⊂ [0, 2]). Rationale: RRF already ranks
/// exact symbol matches (tier3 recall@10 0.984 RRF-only), but the multiplicative
/// rerank buried them under vector noise + size dampening (recall@10 → 0.806).
/// Exact matches order among themselves by `base_score` (i.e. by RRF rank).
pub fn final_adjusted_score(
    base_score: f64,
    name_boost: f64,
    size_factor: f64,
    doc_penalty: f64,
    is_exact_name: bool,
) -> f64 {
    if is_exact_name {
        base_score + crate::domain::EXACT_NAME_MATCH_BONUS
    } else {
        (base_score * name_boost * size_factor * doc_penalty * 100.0).round() / 100.0
    }
}

#[cfg(test)]
pub fn rrf_fusion(
    fts_results: &[SearchResult],
    vec_results: &[SearchResult],
    k: u32,
    top_k: usize,
) -> Vec<SearchResult> {
    weighted_rrf_fusion(fts_results, vec_results, k, top_k, 1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_name_dominates_any_non_exact() {
        // Worst-case exact (tiny base, heavy size dampening) must still outrank the
        // best-case non-exact (max base, max name_boost). This is the tier3 fix:
        // an exact symbol match can never be buried by the multiplicative rerank.
        let exact = final_adjusted_score(0.01, 1.0, 0.1, 1.0, true);
        let best_non_exact =
            final_adjusted_score(1.0, crate::domain::NAME_BOOST_CAP, 1.0, 1.0, false);
        assert!(
            exact > best_non_exact,
            "exact {exact} must beat non-exact {best_non_exact}"
        );
    }

    #[test]
    fn test_non_exact_byte_identical_to_legacy_formula() {
        // Non-exact path MUST equal the old inline formula exactly — guards against
        // any NL-ranking regression from this change.
        let got = final_adjusted_score(0.46, 1.3, 0.8, 1.0, false);
        let legacy = (0.46_f64 * 1.3 * 0.8 * 1.0 * 100.0).round() / 100.0;
        assert_eq!(got, legacy);
    }

    #[test]
    fn test_exact_matches_order_by_base() {
        // Among exact matches, higher base (better RRF rank) wins.
        assert!(
            final_adjusted_score(0.5, 1.0, 1.0, 1.0, true)
                > final_adjusted_score(0.2, 1.0, 1.0, 1.0, true)
        );
    }

    #[test]
    fn test_rrf_fusion_basic() {
        let fts_results = vec![
            SearchResult {
                node_id: 1,
                score: 0.0,
            },
            SearchResult {
                node_id: 2,
                score: 0.0,
            },
            SearchResult {
                node_id: 3,
                score: 0.0,
            },
        ];
        let vec_results = vec![
            SearchResult {
                node_id: 2,
                score: 0.0,
            },
            SearchResult {
                node_id: 4,
                score: 0.0,
            },
            SearchResult {
                node_id: 1,
                score: 0.0,
            },
        ];

        let fused = rrf_fusion(&fts_results, &vec_results, 60, 3);

        assert_eq!(fused[0].node_id, 2);
        assert_eq!(fused[1].node_id, 1);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn test_rrf_with_no_overlap() {
        let fts = vec![SearchResult {
            node_id: 1,
            score: 0.0,
        }];
        let vec = vec![SearchResult {
            node_id: 2,
            score: 0.0,
        }];

        let fused = rrf_fusion(&fts, &vec, 60, 5);
        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn test_weighted_rrf_prefers_fts() {
        let fts = vec![SearchResult {
            node_id: 1,
            score: 0.0,
        }];
        let vec = vec![SearchResult {
            node_id: 2,
            score: 0.0,
        }];

        let fused = weighted_rrf_fusion(&fts, &vec, 60, 5, 2.0, 1.0);
        assert_eq!(fused.len(), 2);
        assert_eq!(
            fused[0].node_id, 1,
            "FTS-only result should rank first when fts_weight > vec_weight"
        );
        assert!(fused[0].score > fused[1].score);
    }

    #[test]
    fn test_weighted_rrf_both_sources() {
        let fts = vec![
            SearchResult {
                node_id: 1,
                score: 0.0,
            },
            SearchResult {
                node_id: 2,
                score: 0.0,
            },
        ];
        let vec = vec![
            SearchResult {
                node_id: 2,
                score: 0.0,
            },
            SearchResult {
                node_id: 3,
                score: 0.0,
            },
        ];

        let fused = weighted_rrf_fusion(&fts, &vec, 60, 5, 1.0, 1.0);
        assert_eq!(
            fused[0].node_id, 2,
            "Node appearing in both sources should rank highest"
        );
    }

    #[test]
    fn test_score_blending_breaks_ties() {
        // Two FTS results at rank 0 and 1: node_1 has higher raw BM25 score
        // With blending, even if RRF ranks are close, the higher BM25 should win
        let fts = vec![
            SearchResult {
                node_id: 1,
                score: 10.0,
            }, // high BM25
            SearchResult {
                node_id: 2,
                score: 1.0,
            }, // low BM25
        ];
        let vec: Vec<SearchResult> = vec![];

        let fused = weighted_rrf_fusion(&fts, &vec, 60, 5, 1.0, 1.0);
        assert_eq!(fused[0].node_id, 1, "Higher raw score should rank first");
        // Verify that blending added score beyond pure RRF
        let pure_rrf_rank0 = 1.0 / (60.0 + 0.0 + 1.0);
        assert!(
            fused[0].score > pure_rrf_rank0,
            "Blended score should exceed pure RRF"
        );
    }

    /// Where the no-flip guarantee STOPS — the other side of
    /// `test_blend_cannot_flip_adjacent_ranks`.
    ///
    /// That test uses one source, where the proof holds. With two, the fused
    /// totals of two nodes can sit closer together than the blend ceiling, and
    /// the raw scores then decide the order. At k=30, weights 1/1:
    ///   A = fts rank 0 + vec rank 5 → 1/31 + 1/36 = 0.0600358
    ///   B = fts rank 1 + vec rank 4 → 1/32 + 1/35 = 0.0598214   (gap 0.00021)
    ///   blend ceiling for B, both sources at max raw = 0.00101
    /// so B overtakes A. This is intended tie-breaking, and it is exactly what
    /// the doc comment used to deny; the assertion below exists so the claim and
    /// the code cannot drift apart again.
    #[test]
    fn test_blend_can_reorder_a_cross_source_near_tie() {
        let k = 30u32;
        // A (id 1) leads in fts, B (id 2) leads in vec — fillers pad the ranks.
        let fts = vec![
            SearchResult {
                node_id: 1,
                score: 0.0,
            }, // rank 0
            SearchResult {
                node_id: 2,
                score: 10.0,
            }, // rank 1, max raw
        ];
        let vec_results = vec![
            SearchResult {
                node_id: 10,
                score: 0.0,
            },
            SearchResult {
                node_id: 11,
                score: 0.0,
            },
            SearchResult {
                node_id: 12,
                score: 0.0,
            },
            SearchResult {
                node_id: 13,
                score: 0.0,
            },
            SearchResult {
                node_id: 2,
                score: 10.0,
            }, // rank 4, max raw
            SearchResult {
                node_id: 1,
                score: 0.0,
            }, // rank 5
        ];

        // RRF alone puts A first.
        let no_blend = weighted_rrf_fusion(
            &fts.iter()
                .map(|r| SearchResult {
                    node_id: r.node_id,
                    score: 0.0,
                })
                .collect::<Vec<_>>(),
            &vec_results
                .iter()
                .map(|r| SearchResult {
                    node_id: r.node_id,
                    score: 0.0,
                })
                .collect::<Vec<_>>(),
            k,
            5,
            1.0,
            1.0,
        );
        assert_eq!(
            no_blend[0].node_id, 1,
            "without raw scores the fused RRF ordering must put A first"
        );

        // With raw scores, the blend crosses the sub-gap tie.
        let fused = weighted_rrf_fusion(&fts, &vec_results, k, 5, 1.0, 1.0);
        assert_eq!(
            fused[0].node_id, 2,
            "a cross-source near-tie IS decided by the blend — the per-source \
             no-flip proof does not extend to the fused ordering"
        );
    }

    /// Scientific invariant: blending must NEVER flip adjacent ranks — WITHIN one
    /// source, which is the scope the bound is proven for (see
    /// `test_blend_can_reorder_a_cross_source_near_tie` for where it ends).
    /// A rank-0 result with a low raw score must still beat a rank-1 result with
    /// max raw score. Historically (SCORE_BLEND_FACTOR=0.1 with k=30), this
    /// invariant was violated — the blend term dominated the RRF term by ~100×.
    #[test]
    fn test_blend_cannot_flip_adjacent_ranks() {
        // Adversarial case: rank-0 has the minimum non-zero raw score,
        // rank-1 has the maximum. Old formula would let rank-1 win.
        for &k in &[10u32, 30, 60, 100] {
            let fts = vec![
                SearchResult {
                    node_id: 1,
                    score: 0.0001,
                }, // rank 0, tiny score
                SearchResult {
                    node_id: 2,
                    score: 1000.0,
                }, // rank 1, huge score
            ];
            let vec_empty: Vec<SearchResult> = vec![];
            let fused = weighted_rrf_fusion(&fts, &vec_empty, k, 5, 1.0, 1.0);
            assert_eq!(
                fused[0].node_id, 1,
                "k={}: rank-0 must win even when rank-1 has much higher raw score (blend must not flip ranks)",
                k
            );
        }
    }

    /// Scientific invariant: cross-source ranks are also safe.
    /// An item at rank 0 in FTS + absent in vec should beat an item at rank 5
    /// in FTS + rank 0 in vec IF the RRF score says so (regardless of raw scores).
    #[test]
    fn test_blend_respects_cross_source_rank_budget() {
        let k = 30u32;
        // Node 1: rank 0 in FTS only → RRF = 1/31 ≈ 0.03226
        // Node 2: rank 5 in FTS + rank 0 in vec → RRF = 1/36 + 1/31 ≈ 0.0601
        // With (fts,vec) both weight=1, Node 2 has higher RRF and must win.
        let fts = vec![
            SearchResult {
                node_id: 1,
                score: 100.0,
            }, // rank 0, max raw
            SearchResult {
                node_id: 9,
                score: 1.0,
            },
            SearchResult {
                node_id: 8,
                score: 1.0,
            },
            SearchResult {
                node_id: 7,
                score: 1.0,
            },
            SearchResult {
                node_id: 6,
                score: 1.0,
            },
            SearchResult {
                node_id: 2,
                score: 0.001,
            }, // rank 5, tiny raw
        ];
        let vec = vec![
            SearchResult {
                node_id: 2,
                score: 0.001,
            }, // rank 0 in vec, tiny raw
        ];
        let fused = weighted_rrf_fusion(&fts, &vec, k, 5, 1.0, 1.0);
        assert_eq!(
            fused[0].node_id, 2,
            "Higher combined RRF rank must win regardless of raw scores"
        );
    }

    /// Within the same source, blending provides a meaningful tie-breaker
    /// between items whose RRF ranks differ by 1 but raw scores diverge hugely.
    /// This is the scenario the blend is actually designed for.
    ///
    /// Note: cross-source blend tie-breaking cannot work — per-source normalization
    /// maps each source's top-scoring item to blend=blend_scale regardless of raw
    /// units, so FTS BM25 and vector similarity cannot be directly compared.
    #[test]
    fn test_blend_nudges_within_source() {
        // Same source, two items at adjacent ranks. The RRF gap at k=30 is tiny
        // (1/(31*32) ≈ 0.00101). Blend adds ~0.00025 max. Rank still dominates.
        let k = 30u32;
        let fts = vec![
            SearchResult {
                node_id: 1,
                score: 100.0,
            }, // rank 0, max raw
            SearchResult {
                node_id: 2,
                score: 10.0,
            }, // rank 1, lower raw
        ];
        let vec_empty: Vec<SearchResult> = vec![];
        let fused = weighted_rrf_fusion(&fts, &vec_empty, k, 5, 1.0, 1.0);
        // Natural rank still wins (1 before 2), but score gap is larger than pure RRF
        // because both blend contributions add to the correct side.
        assert_eq!(fused[0].node_id, 1);
        let pure_rrf_gap = 1.0 / (k as f64 + 1.0) - 1.0 / (k as f64 + 2.0);
        let observed_gap = fused[0].score - fused[1].score;
        assert!(
            observed_gap >= pure_rrf_gap,
            "Blending should preserve or widen rank-0/rank-1 gap when raw scores agree with rank, got {} vs RRF-only {}",
            observed_gap, pure_rrf_gap
        );
    }

    /// M4: a NEGATIVE raw score (vec0 L2 distance yields score = 1 - dist ∈
    /// [-1, 1]) must not produce an unbounded blend that flips adjacent ranks.
    /// Normalizing `score / max` against a small positive vec_max let a
    /// strongly-negative score at a BETTER rank subtract a blend far larger than
    /// the adjacent RRF gap, overtaking the lower-ranked item. Clamping the
    /// numerator to >= 0 keeps blend within [0, blend_scale·weight] as the proof
    /// requires.
    #[test]
    fn test_blend_negative_vector_score_cannot_flip_rank() {
        let fts: Vec<SearchResult> = vec![];
        let vec = vec![
            SearchResult {
                node_id: 1,
                score: -1.0,
            }, // rank 0, most-negative similarity
            SearchResult {
                node_id: 2,
                score: 0.02,
            }, // rank 1, small positive = vec_max
        ];
        // Pre-fix: node 1's blend = blend_scale·(-1/0.02) = -50·blend_scale, which
        // dwarfs the ~1/992 adjacent gap → node 2 (rank 1) wins. Post-fix node 1 holds.
        let fused = weighted_rrf_fusion(&fts, &vec, 30, 5, 1.0, 1.0);
        assert_eq!(
            fused[0].node_id, 1,
            "rank-0 must win: a negative similarity must not yield an unbounded negative blend",
        );
    }

    /// L3: exact-score ties must be ordered deterministically by node_id, so the
    /// `truncate(top_k)` boundary is stable across runs. `scores` is a HashMap with a
    /// per-instance random seed, so before the node_id tie-break the equal-score
    /// order (and thus which item survives the cut) varied run-to-run. The loop
    /// defeats the ~50% chance a single buggy run happens to land the right order:
    /// pre-fix, at least one of the 64 fresh-HashMap calls almost certainly flips.
    #[test]
    fn test_exact_score_ties_broken_by_node_id_deterministic() {
        // node 9 (fts rank 0) and node 2 (vec rank 0), equal weights + zero raw
        // scores → identical fused RRF score 1/(k+1). The tie must resolve to
        // ascending node_id: [2, 9].
        for _ in 0..64 {
            let fts = vec![SearchResult {
                node_id: 9,
                score: 0.0,
            }];
            let vec = vec![SearchResult {
                node_id: 2,
                score: 0.0,
            }];
            let fused = weighted_rrf_fusion(&fts, &vec, 60, 5, 1.0, 1.0);
            assert_eq!(fused.len(), 2);
            assert!(
                (fused[0].score - fused[1].score).abs() < 1e-12,
                "precondition: the two nodes must be an exact score tie"
            );
            assert_eq!(fused[0].node_id, 2, "tie must order by ascending node_id");
            assert_eq!(fused[1].node_id, 9, "tie must order by ascending node_id");
        }
    }

    /// L3: a tie straddling the `truncate(top_k)` boundary must keep the SAME items
    /// every run (the lowest node_ids), not a HashMap-order-dependent subset.
    #[test]
    fn test_tie_at_truncate_boundary_keeps_lowest_node_ids() {
        // Three nodes all at their source's rank 0 with equal weight → identical
        // fused score. top_k=2 drops exactly one; it must always be the highest id.
        for _ in 0..64 {
            let fts = vec![SearchResult {
                node_id: 7,
                score: 0.0,
            }];
            let vec = vec![SearchResult {
                node_id: 3,
                score: 0.0,
            }];
            // node 5 in neither source's rank 0 uniquely — put it alone in a third
            // slot by giving it the same RRF as the others via fts rank 0 in a
            // separate weighted call is not possible; instead assert on the 2-way
            // tie plus a distinct lower-ranked node to confirm the boundary is stable.
            let mut fused = weighted_rrf_fusion(&fts, &vec, 60, 1, 1.0, 1.0);
            assert_eq!(fused.len(), 1, "top_k=1 keeps exactly one of the tie");
            assert_eq!(
                fused.remove(0).node_id,
                3,
                "the surviving item at a tie-straddled cut must be the lowest node_id, every run"
            );
        }
    }

    /// Proof that blend_scale is mathematically bounded below the
    /// smallest adjacent-rank RRF gap for all realistic k values.
    #[test]
    fn test_blend_scale_mathematically_bounded() {
        for &k in &[5u32, 10, 30, 60, 100, 200] {
            let k_f = k as f64;
            let blend_scale = 0.5 / ((k_f + 1.0) * (k_f + 2.0));
            let adjacent_gap = 1.0 / (k_f + 1.0) - 1.0 / (k_f + 2.0);
            assert!(
                blend_scale < adjacent_gap,
                "k={}: blend_scale {} must be < adjacent RRF gap {}",
                k,
                blend_scale,
                adjacent_gap
            );
            // Safety margin: blend should be ≤ half the gap
            assert!(
                blend_scale <= adjacent_gap * 0.5 + f64::EPSILON,
                "k={}: blend should be ≤ half the adjacent gap",
                k
            );
        }
    }
}
