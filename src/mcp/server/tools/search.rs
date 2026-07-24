//! `semantic_code_search` — hybrid BM25 + vector search with RRF fusion.
//!
//! Confidence scoring (FTS sparsity / OR-fallback / source intersection),
//! acronym-heavy query detection, doc-penalty for markdown matches, and
//! token-aware compression sit here. Adjusted score combines RRF rank,
//! query quality, name match boost, and size dampening.

use super::super::*;

impl McpServer {
    pub(in crate::mcp::server) fn tool_semantic_search(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        // Per-result code_content cap used both in estimation (below) and the
        // actual result payload so compression triggers reflect real output size.
        const MAX_SEARCH_CODE_LEN: usize = 500;
        let query = required_str(args, "query")?;
        let top_k = args["top_k"]
            .as_u64()
            .or_else(|| args["limit"].as_u64())
            .unwrap_or(20)
            .clamp(1, 100) as i64;
        let node_type_filter = args["node_type"].as_str();
        let compact = args["compact"].as_bool().unwrap_or(false);

        // Validate node_type up-front: unknown aliases normalize to empty and
        // would silently filter every result away (see tool_ast_search parity).
        if let Some(nt) = node_type_filter {
            if crate::domain::normalize_type_filter(nt).is_empty() {
                return Err(anyhow!(
                    "Unknown node_type filter: '{}'. Valid: fn, class, struct, enum, trait, type, const, var",
                    nt
                ));
            }
        }

        // Validate `language` up-front and normalize to canonical case: an unknown
        // language matches no stored `language` field and would silently return an
        // empty result. Canonicalizing also accepts mixed-case input, since the
        // downstream filter is an exact match. Parity with node_type above and CLI.
        let language_filter = match args["language"].as_str() {
            Some(lf) => Some(crate::utils::config::canonical_language(lf).ok_or_else(|| {
                anyhow!(
                    "Unknown language filter: '{}'. Valid: {}",
                    lf,
                    crate::utils::config::SUPPORTED_LANGUAGES.join(", ")
                )
            })?),
            None => None,
        };

        // Query quality factor: penalize vague/short queries so relevance scores
        // reflect actual match quality, not just relative rank position.
        let meaningful_tokens: Vec<&str> = query
            .split_whitespace()
            .filter(|w| {
                let has_alnum = w.chars().any(|c| c.is_alphanumeric());
                let char_count = w.chars().count();
                has_alnum && (char_count > 1 || w.chars().all(|c| c.is_uppercase()))
            })
            .collect();
        let query_quality = match meaningful_tokens.len() {
            0 => 0.3,
            1 if meaningful_tokens[0].len() <= 2 => 0.4,
            1 => 0.7,
            2 => 0.85,
            _ => 1.0,
        };

        // Lazy model loading: pick up model if downloaded in background
        self.try_lazy_load_model();

        // Ensure index is up to date (unless caller requested read-only mode)
        if !should_skip_indexing(args) {
            self.ensure_indexed()?;
        }

        // vec0 KNN can't pre-filter on joined `nodes` columns, so language/node_type
        // filtering happens after the fetch (Phase 1 below). Widen the candidate pool
        // when a filter is active so a selective filter can't silently starve top_k.
        // The unfiltered fetch is byte-identical to the historical (top_k*4).max(20),
        // so the retrieval benchmark (which passes no filter) is unaffected.
        let filtered = language_filter.is_some() || node_type_filter.is_some();
        let fetch_count = crate::domain::search_fetch_count(top_k, filtered);
        // FTS sparsity ratio uses the base (unfiltered) pool size so a widened filtered
        // fetch doesn't spuriously depress match_confidence for filtered queries.
        let conf_fetch = crate::domain::search_fetch_count(top_k, false);
        let fts_result = queries::fts5_search(self.db.conn(), query, fetch_count)?;
        let fts_or_fallback = fts_result.or_fallback;

        // Convert to SearchResult for RRF, carrying raw BM25 scores for score blending
        let fts_search: Vec<crate::search::fusion::SearchResult> = fts_result
            .nodes
            .iter()
            .enumerate()
            .map(|(i, r)| crate::search::fusion::SearchResult {
                node_id: r.id,
                score: fts_result.bm25_scores.get(i).copied().unwrap_or(0.0),
            })
            .collect();

        // Vector search (if embedding model available and vec enabled)
        let model_guard = lock_or_recover(&self.embedding_model, "embedding_model");
        let vec_search: Vec<crate::search::fusion::SearchResult> =
            if let Some(ref model) = *model_guard {
                if self.db.vec_enabled() {
                    match model.embed(query) {
                        Ok(query_embedding) => {
                            queries::vector_search(self.db.conn(), &query_embedding, fetch_count)?
                                .iter()
                                .map(|(node_id, distance)| {
                                    // Convert distance to similarity: 1.0 - distance (L2-normalized vectors)
                                    crate::search::fusion::SearchResult {
                                        node_id: *node_id,
                                        score: 1.0 - distance,
                                    }
                                })
                                .collect()
                        }
                        Err(_) => vec![],
                    }
                } else {
                    vec![]
                }
            } else {
                vec![]
            };
        // Whether the vector channel was actually available for this query (model
        // loaded AND sqlite-vec enabled). When false, every result is FTS5-only with
        // reduced semantic recall — surfaced in the output below so the caller is not
        // silently degraded (the model auto-downloads in the background on first use).
        let vector_available = model_guard.is_some() && self.db.vec_enabled();
        drop(model_guard);

        // Track search source IDs for confidence scoring
        let fts_node_ids: std::collections::HashSet<i64> =
            fts_search.iter().map(|r| r.node_id).collect();
        let vec_node_ids: std::collections::HashSet<i64> =
            vec_search.iter().map(|r| r.node_id).collect();

        // RRF fusion (FTS + Vec when available, FTS-only otherwise)
        // k=30: sharper rank sensitivity than default 60 (top results matter more)
        // Default fts=1.0, vec=1.2: slightly favor vector similarity since FTS is now stronger
        // with name_tokens and type columns in v2 schema.
        //
        // Acronym-heavy override: queries that are entirely short uppercase tokens
        // (≤3 tokens, each ≤5 chars, all [A-Z0-9]) are letter-exact identifiers —
        // embeddings handle them poorly (training corpora rarely teach "RRF" ≈
        // "reciprocal rank fusion"), while FTS5's token-exact match is reliable.
        // Shift the weight toward FTS to let the precise channel dominate.
        let is_acronym_heavy = !meaningful_tokens.is_empty()
            && meaningful_tokens.len() <= crate::domain::ACRONYM_MAX_TOKENS
            && meaningful_tokens.iter().all(|t| {
                let len_ok = t.chars().count() <= crate::domain::ACRONYM_MAX_TOKEN_CHARS;
                let shape_ok = t
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
                len_ok && shape_ok
            });
        let (fts_weight, vec_weight) = if is_acronym_heavy {
            (
                crate::domain::ACRONYM_FTS_WEIGHT,
                crate::domain::ACRONYM_VEC_WEIGHT,
            )
        } else {
            (
                crate::domain::DEFAULT_FTS_WEIGHT,
                crate::domain::DEFAULT_VEC_WEIGHT,
            )
        };
        let fused = weighted_rrf_fusion(
            &fts_search,
            &vec_search,
            crate::domain::RERANK_RRF_K,
            fetch_count as usize,
            fts_weight,
            vec_weight,
        );

        // Match confidence: penalize when search signals are weak
        let match_confidence = {
            let mut c = 1.0_f64;
            // FTS-empty penalty: no text match → results are purely vector similarity (often noise)
            if fts_search.is_empty() && !vec_search.is_empty() {
                c *= crate::domain::CONF_VEC_ONLY_PENALTY;
            } else if !fts_search.is_empty() {
                // OR-fallback penalty: AND mode failed → query terms don't co-occur (weaker match)
                if fts_or_fallback {
                    c *= crate::domain::CONF_OR_FALLBACK_PENALTY;
                }
                // FTS sparsity: fewer results relative to fetch_count → weaker text match.
                // Skip the ratio check for precision queries (fts returns ≤4 hits): a
                // unique-identifier search legitimately has a low ratio but is a strong
                // signal, not a weak one. Only apply when we have enough FTS breadth to
                // judge "sparse vs. broad".
                if fts_search.len() >= crate::domain::CONF_SPARSITY_MIN_FTS {
                    let fts_ratio = fts_search.len() as f64 / conf_fetch as f64;
                    if fts_ratio < crate::domain::CONF_SPARSITY_R1 {
                        c *= crate::domain::CONF_SPARSITY_P1;
                    } else if fts_ratio < crate::domain::CONF_SPARSITY_R2 {
                        c *= crate::domain::CONF_SPARSITY_P2;
                    } else if fts_ratio < crate::domain::CONF_SPARSITY_R3 {
                        c *= crate::domain::CONF_SPARSITY_P3;
                    }
                }
            }
            // Source intersection: when both sources available, low overlap → less confidence.
            // Only meaningful when FTS returned enough breadth to judge overlap; for
            // precision queries (≤4 FTS hits) the intersection is naturally tiny and
            // should not count against confidence.
            if fts_search.len() >= crate::domain::CONF_SPARSITY_MIN_FTS && !vec_search.is_empty() {
                let top_ids: Vec<i64> = fused
                    .iter()
                    .take(top_k as usize)
                    .map(|r| r.node_id)
                    .collect();
                let in_both = top_ids
                    .iter()
                    .filter(|id| fts_node_ids.contains(id) && vec_node_ids.contains(id))
                    .count();
                let ratio = in_both as f64 / top_ids.len().max(1) as f64;
                if ratio < crate::domain::CONF_INTERSECTION_MIN_RATIO {
                    c *= crate::domain::CONF_INTERSECTION_PENALTY;
                }
            }
            c
        };

        // Measurement seam (env-gated, stderr-only — NO response-contract change): emit
        // the raw top-1 vector similarity alongside the final match_confidence so the
        // confidence-calibration bench can test whether it separates good-NL from
        // nonsense queries (the RRF `relevance` score does not — it is rank-fused and
        // discards similarity magnitude). Default behavior is untouched: nothing is
        // emitted unless CODE_GRAPH_EMIT_CONFIDENCE is set. vec_search is KNN-ordered
        // (nearest first), so its head carries the top raw similarity `1.0 - distance`.
        // NOTE: node_vectors is a plain vec0 table (no `distance=` metric) → sqlite-vec
        // uses L2 distance, so this is `1.0 - L2_distance`, NOT cosine similarity. For
        // L2-normalized embeddings it is order-equivalent to cosine but not equal to it.
        // See scripts/embedding_benchmark/eval_confidence.py.
        if std::env::var_os("CODE_GRAPH_EMIT_CONFIDENCE").is_some() {
            let top_vec_score = vec_search.first().map(|r| r.score).unwrap_or(f64::NAN);
            eprintln!(
                "[CONF_PROBE] q={:?} match_confidence={:.4} top_vec_score={:.4} fts_hits={} vec_hits={} or_fallback={}",
                query, match_confidence, top_vec_score, fts_search.len(), vec_search.len(), fts_or_fallback
            );
        }

        // Low-confidence warning trigger (consumed by the compressed path and
        // finalize_search_results below). Fires ONLY when the result set has no text
        // anchor at all — FTS returned nothing, so the ranking is vector similarity
        // alone, the one case where "vector-similarity only" is literally true.
        //
        // It deliberately does NOT use the match_confidence<0.5 threshold: the
        // confidence-calibration bench (scripts/embedding_benchmark/eval_confidence.py)
        // measured that match_confidence pins ~0.45 for essentially every multi-word
        // natural-language query, good and nonsense alike (OR-fallback 0.6 ×
        // intersection 0.75), and that neither match_confidence, RRF relevance, nor raw
        // top-1 vector similarity separates a good NL query from nonsense on this index.
        // The old threshold therefore warned on 100% of good NL queries (which retrieve
        // relevant results 82% of the time) — a false alarm that pushed callers to
        // distrust correct results. fts-empty is the honest, mechanically-trustworthy
        // trigger. (FTS-only degradation — vector channel down — is surfaced separately
        // as a `note` in finalize_search_results.)
        let vector_only_no_anchor = fts_search.is_empty() && !vec_search.is_empty();

        // Batch-fetch all candidate nodes with file info (single query instead of N+1)
        let candidate_ids: Vec<i64> = fused.iter().map(|r| r.node_id).collect();
        let nodes_with_files =
            queries::get_nodes_with_files_by_ids(self.db.conn(), &candidate_ids)?;

        // Build a lookup by node_id preserving the fused ranking order
        let mut nwf_map: std::collections::HashMap<i64, &queries::NodeWithFile> = nodes_with_files
            .iter()
            .map(|nwf| (nwf.node.id, nwf))
            .collect();

        // Phase 1: Collect all valid candidates with adjusted scores
        // Name match boost + size dampening counter BM25/vector bias toward large nodes
        struct Candidate<'a> {
            node: &'a queries::NodeResult,
            file_path: &'a str,
            adjusted_score: f64,
        }
        let max_rrf = fused.first().map(|f| f.score).unwrap_or(0.0);
        let query_terms_lower: Vec<String> =
            meaningful_tokens.iter().map(|t| t.to_lowercase()).collect();
        // Verbatim identifier query (e.g. "run_serve") — used for exact-name rerank
        // dominance below and the confidence exemption further down (single source).
        let query_trimmed = query.trim().to_lowercase();
        let mut candidates: Vec<Candidate> = Vec::new();
        // Count candidates that matched the query but were removed by the active
        // language/node_type filter — drives the filter-aware empty-result hint below.
        let mut dropped_by_filter = 0usize;
        for r in &fused {
            if let Some(nwf) = nwf_map.remove(&r.node_id) {
                let node = &nwf.node;
                if crate::domain::is_skippable_result(&node.node_type, &node.name, &nwf.file_path) {
                    continue;
                }
                if let Some(nt) = node_type_filter {
                    let normalized = normalize_type_filter_mcp(nt);
                    if !normalized.iter().any(|t| t == &node.node_type) {
                        dropped_by_filter += 1;
                        continue;
                    }
                }
                if let Some(lang) = language_filter {
                    if nwf.language.as_deref() != Some(lang) {
                        dropped_by_filter += 1;
                        continue;
                    }
                }

                let base_score = if max_rrf > 0.0 {
                    (r.score / max_rrf * query_quality * match_confidence * 100.0).round() / 100.0
                } else {
                    0.0
                };

                // Name match boost: symbols whose name contains query terms are more likely relevant
                let name_lower = node.name.to_lowercase();
                // Exact symbol-name match dominates the rerank: RRF already ranks an
                // exact match (tier3 recall@10 0.984 RRF-only), but base×name_boost×size
                // could bury it under vector noise + size dampening (→ 0.806). Same
                // semantics as `has_exact_name_match` (confidence exemption) below.
                let is_exact_name = name_lower == query_trimmed
                    || node
                        .qualified_name
                        .as_deref()
                        .map(|q| q.to_lowercase() == query_trimmed)
                        .unwrap_or(false);
                let name_match_count = query_terms_lower
                    .iter()
                    .filter(|t| name_lower.contains(t.as_str()))
                    .count();
                let name_boost = (1.0
                    + name_match_count as f64 * crate::domain::NAME_BOOST_PER_MATCH)
                    .min(crate::domain::NAME_BOOST_CAP);

                // Size dampening: counter BM25/vector bias toward very large nodes (>100 lines)
                let node_lines = (node.end_line.saturating_sub(node.start_line) + 1) as f64;
                let size_factor = if node_lines > crate::domain::SIZE_DAMPEN_LINES {
                    1.0 / (1.0
                        + (node_lines / crate::domain::SIZE_DAMPEN_LINES).ln()
                            * crate::domain::SIZE_DAMPEN_COEFF)
                } else {
                    1.0
                };

                // Doc penalty: markdown headings can match loosely via vector similarity
                // for code-intent queries (the tool is `semantic_code_search`). When the
                // caller has not explicitly requested markdown via `language="markdown"`,
                // demote them so README/heading prose cannot outrank real code matches.
                let doc_penalty = if nwf.language.as_deref() == Some("markdown")
                    && language_filter != Some("markdown")
                {
                    crate::domain::DOC_PENALTY_MARKDOWN
                } else {
                    1.0
                };

                let adjusted = crate::search::fusion::final_adjusted_score(
                    base_score,
                    name_boost,
                    size_factor,
                    doc_penalty,
                    is_exact_name,
                );
                candidates.push(Candidate {
                    node,
                    file_path: &nwf.file_path,
                    adjusted_score: adjusted,
                });
            }
        }

        // Phase 2: Re-rank by adjusted score (name relevance + size normalization)
        candidates.sort_by(|a, b| b.adjusted_score.total_cmp(&a.adjusted_score));
        candidates.truncate(top_k as usize);

        // Phase 3: Build results
        let mut results = Vec::new();
        for c in &candidates {
            let node = c.node;
            let score = c.adjusted_score;

            if compact {
                results.push(json!({
                    "node_id": node.id,
                    "name": node.name,
                    "type": node.node_type,
                    "file_path": c.file_path,
                    "line": format!("{}-{}", node.start_line, node.end_line),
                    "signature": node.signature,
                    "relevance": score,
                }));
            } else {
                let code = if node.code_content.len() > MAX_SEARCH_CODE_LEN {
                    let safe_end = node.code_content.floor_char_boundary(MAX_SEARCH_CODE_LEN);
                    let truncated = &node.code_content[..node.code_content[..safe_end]
                        .rfind('\n')
                        .unwrap_or(safe_end)];
                    format!(
                        "{}\n// ... truncated ({} lines total, use get_ast_node for full code)",
                        truncated,
                        node.end_line - node.start_line + 1
                    )
                } else {
                    node.code_content.clone()
                };
                results.push(json!({
                    "node_id": node.id,
                    "name": node.name,
                    "type": node.node_type,
                    "file_path": c.file_path,
                    "start_line": node.start_line,
                    "end_line": node.end_line,
                    "code_content": code,
                    "signature": node.signature,
                    "relevance": score,
                }));
            }
        }

        // Record search metrics (before potential compression return)
        lock_or_recover(&self.metrics, "metrics").record_search(
            results.len(),
            query_quality,
            vec_search.is_empty(),
        );

        // Exact-identifier exemption for the low-confidence warning: when the query
        // is a single identifier that appears verbatim as a candidate symbol name,
        // retrieval is precise regardless of the FTS breadth heuristics. Computed
        // once here so BOTH the compressed and the bare-array return paths gate the
        // noise warning identically (previously only the compressed path had it).
        let has_exact_name_match = candidates.iter().take(5).any(|c| {
            c.node.name.to_lowercase() == query_trimmed
                || c.node
                    .qualified_name
                    .as_deref()
                    .map(|q| q.to_lowercase() == query_trimmed)
                    .unwrap_or(false)
        });

        // Context Sandbox: compress only if results likely exceed token threshold.
        // Skip compression when compact=true — compact results are already token-efficient
        // (~85% smaller than full results) and contain fields (relevance, signature)
        // that would be lost by compression.
        //
        // Estimation must mirror the actual result payload: code_content is capped at
        // MAX_SEARCH_CODE_LEN per result, and context_string is NOT included in
        // the output. Estimating from raw context_string massively overestimates and
        // fires compression even for small top_k (e.g. 3) responses that would fit
        // comfortably under the token budget.
        use crate::sandbox::compressor::CompressedOutput;
        let estimated_tokens: usize = if compact {
            0
        } else {
            candidates
                .iter()
                .map(|c| {
                    let node = c.node;
                    let code_chars = node.code_content.len().min(MAX_SEARCH_CODE_LEN);
                    let sig_chars = node.signature.as_ref().map_or(0, |s| s.len());
                    let name_chars = node.name.len() + c.file_path.len();
                    // ~80 chars of JSON framing per result (keys, braces, quotes, node_id/line)
                    (code_chars + sig_chars + name_chars + 80) / crate::domain::CHARS_PER_TOKEN
                })
                .sum()
        };
        if estimated_tokens > COMPRESSION_TOKEN_THRESHOLD {
            // Build node_results and file_paths only when compression is needed
            let node_results: Vec<queries::NodeResult> = candidates
                .iter()
                .map(|c| {
                    let node = c.node;
                    queries::NodeResult {
                        id: node.id,
                        file_id: node.file_id,
                        node_type: node.node_type.clone(),
                        name: node.name.clone(),
                        qualified_name: node.qualified_name.clone(),
                        start_line: node.start_line,
                        end_line: node.end_line,
                        code_content: node.code_content.clone(),
                        signature: node.signature.clone(),
                        doc_comment: node.doc_comment.clone(),
                        context_string: node.context_string.clone(),
                        name_tokens: node.name_tokens.clone(),
                        return_type: node.return_type.clone(),
                        param_types: node.param_types.clone(),
                        is_test: node.is_test,
                    }
                })
                .collect();
            let file_paths: Vec<String> =
                candidates.iter().map(|c| c.file_path.to_string()).collect();
            if let Some(compressed) = crate::sandbox::compressor::compress_if_needed(
                &node_results,
                &file_paths,
                COMPRESSION_TOKEN_THRESHOLD,
            )? {
                let (mode, compact) = match compressed {
                    CompressedOutput::Nodes(nodes) => {
                        let items: Vec<serde_json::Value> = nodes
                            .iter()
                            .map(|c| {
                                json!({
                                    "node_id": c.node_id,
                                    "file_path": c.file_path,
                                    "summary": c.summary,
                                })
                            })
                            .collect();
                        ("compressed_nodes", items)
                    }
                    CompressedOutput::Files(groups) => {
                        let items: Vec<serde_json::Value> = groups
                            .iter()
                            .map(|g| {
                                json!({
                                    "file_path": g.file_path,
                                    "summary": g.summary,
                                    "node_ids": g.node_ids,
                                })
                            })
                            .collect();
                        ("compressed_files", items)
                    }
                    CompressedOutput::Directories(groups) => {
                        let items: Vec<serde_json::Value> = groups
                            .iter()
                            .map(|g| {
                                json!({
                                    "file_path": g.file_path,
                                    "summary": g.summary,
                                    "node_ids": g.node_ids,
                                })
                            })
                            .collect();
                        ("compressed_directories", items)
                    }
                };
                // match_confidence (FTS/vector agreement + coverage) is always surfaced as a
                // rough query-shape signal. The warning is separate and fires only when the
                // ranking has no text anchor (see vector_only_no_anchor); `has_exact_name_match`
                // (hoisted above) exempts precise single-identifier queries.
                let mut out = json!({
                    "mode": mode,
                    "message": "Results exceeded token limit. Use get_ast_node(node_id) to expand individual symbols.",
                    "match_confidence": (match_confidence * 100.0).round() / 100.0,
                    "search_mode": if vector_available { "hybrid" } else { "fts_only" },
                    "vector_available": vector_available,
                    "results": compact
                });
                if vector_only_no_anchor && !has_exact_name_match {
                    if let Some(obj) = out.as_object_mut() {
                        obj.insert("low_confidence_warning".into(), json!(VECTOR_ONLY_WARNING));
                    }
                }
                return Ok(out);
            }
        } // end estimated_tokens check

        if results.is_empty() {
            // Filter-aware: if a language/node_type filter removed candidates that DID
            // match the query, say so — the index has matches, just not of this
            // language/type. (vec0 can't pre-filter, so this is a post-fetch drop.)
            if filtered && dropped_by_filter > 0 {
                return Ok(json!({
                    "results": [],
                    "message": "No matching symbols after filtering.",
                    "hint": format!(
                        "{} candidate(s) matched the query but were removed by the active language/node_type filter. Broaden or clear the filter, or raise top_k.",
                        dropped_by_filter
                    )
                }));
            }
            let has_code_syntax = query.contains('(')
                || query.contains(')')
                || query.contains("->")
                || query.contains("::")
                || query.contains('<');
            let has_non_ascii = !query.is_ascii();
            let hint = if has_code_syntax {
                "Query looks like code syntax. For structural queries, use ast_search with type/returns/params filters instead of text search."
            } else if has_non_ascii {
                "Try using English keywords — the search index is English-optimized. Also try broader terms or check spelling."
            } else {
                "Try broader terms, check spelling, or use different keywords. The index may need rebuilding if the codebase changed significantly."
            };
            return Ok(json!({
                "results": [],
                "message": "No matching symbols found.",
                "hint": hint,
                "search_mode": if vector_available { "hybrid" } else { "fts_only" },
                "vector_available": vector_available
            }));
        }

        // Shape the response: a confident hybrid result stays a bare array (the
        // unchanged happy-path contract); a degraded (FTS-only) OR text-anchorless
        // (vector-only) result wraps in an object carrying the signal so the caller
        // doesn't silently trust it. Mirrors the warning the compressed path emits above.
        Ok(finalize_search_results(
            results,
            match_confidence,
            vector_only_no_anchor,
            has_exact_name_match,
            vector_available,
        ))
    }
}

/// Notice attached when a semantic-search result set has NO text anchor — FTS
/// returned nothing, so the ranking is vector similarity alone, the one condition
/// where "vector-similarity only" is literally true. Shared by the compressed
/// (large-result) and bare-array (small-result) returns so the two never drift.
///
/// It is deliberately NOT keyed on a match_confidence threshold: the calibration
/// bench (scripts/embedding_benchmark/eval_confidence.py) refuted match_confidence,
/// RRF relevance, AND raw top-1 vector similarity as separators of good-NL from nonsense, so
/// the old `<0.5` trigger warned on ~every natural-language query (100% of good NL
/// in the corpus) while they returned relevant results. The message states the
/// mechanic and explicitly does not claim the results are wrong.
const VECTOR_ONLY_WARNING: &str = "No exact text matches — results are ranked by vector similarity alone (no keyword anchor). Vague or natural-language queries often land here yet still return relevant symbols, so judge by the results; if they miss, add a concrete identifier or use ast_search with type/returns/params filters.";

/// Pick the response shape for an uncompressed semantic-search result set.
///
/// - vector unavailable → object with the FTS-only degradation `note`.
/// - vector-only (no FTS anchor, and not an exact-identifier hit) → object with
///   `match_confidence` + `low_confidence_warning`, so a query whose ranking rests
///   on vector similarity alone no longer slips through as a bare list with no
///   signal (the gap this closes).
/// - otherwise → a bare array (the unchanged confident-hybrid contract). Note this
///   now includes low-`match_confidence` results that DO have a text anchor: those
///   are overwhelmingly good natural-language queries, and the warning no longer
///   fires on them (see [`VECTOR_ONLY_WARNING`]).
fn finalize_search_results(
    results: Vec<serde_json::Value>,
    match_confidence: f64,
    vector_only: bool,
    has_exact_name_match: bool,
    vector_available: bool,
) -> serde_json::Value {
    if !vector_available {
        return json!({
            "results": results,
            "search_mode": "fts_only",
            "vector_available": false,
            "note": "Embedding model not loaded — results are FTS5-only (reduced semantic recall). The model auto-downloads in the background on first use; retry shortly, or run `code-graph-mcp doctor` to check status."
        });
    }
    if vector_only && !has_exact_name_match {
        return json!({
            "results": results,
            "search_mode": "hybrid",
            "match_confidence": (match_confidence * 100.0).round() / 100.0,
            "low_confidence_warning": VECTOR_ONLY_WARNING,
        });
    }
    json!(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_results() -> Vec<serde_json::Value> {
        vec![json!({"node_id": 1, "name": "foo", "relevance": 0.4})]
    }

    #[test]
    fn vector_only_result_carries_the_warning() {
        // The one honest trigger: no text anchor (fts empty → vector-only ranking).
        let out = finalize_search_results(dummy_results(), 0.30, true, false, true);
        assert!(out.is_object(), "vector-only result must wrap in an object");
        assert_eq!(out["match_confidence"], 0.3);
        assert!(out["low_confidence_warning"]
            .as_str()
            .unwrap()
            .contains("vector similarity alone"));
        assert!(out["results"].is_array());
    }

    #[test]
    fn low_confidence_with_text_anchor_no_longer_warns() {
        // The fix: a low match_confidence (0.45 — the pin for good NL queries) that HAS
        // a text anchor (vector_only=false) stays a bare array with no warning. The old
        // match_confidence<0.5 trigger warned here — on 100% of good NL queries — even
        // though they retrieve relevant results (bench: eval_confidence.py).
        let out = finalize_search_results(dummy_results(), 0.45, false, false, true);
        assert!(
            out.is_array(),
            "low-confidence result WITH a text anchor must stay a bare array"
        );
    }

    #[test]
    fn confident_hybrid_stays_a_bare_array() {
        // Contract unchanged for confident results: still a bare array, no wrapper.
        let out = finalize_search_results(dummy_results(), 0.85, false, false, true);
        assert!(
            out.is_array(),
            "confident hybrid result must stay a bare array"
        );
    }

    #[test]
    fn exact_name_match_is_exempt_from_the_warning() {
        // A precise single-identifier hit is trustworthy even with no FTS breadth —
        // stays a bare array despite being vector-only.
        let out = finalize_search_results(dummy_results(), 0.20, true, true, true);
        assert!(
            out.is_array(),
            "exact-name match must stay a bare array (warning exempt)"
        );
    }

    #[test]
    fn vector_unavailable_reports_fts_only_degradation() {
        let out = finalize_search_results(dummy_results(), 0.90, false, false, false);
        assert_eq!(out["search_mode"], "fts_only");
        assert_eq!(out["vector_available"], false);
        assert!(
            out.get("low_confidence_warning").is_none(),
            "FTS-only degradation is a separate signal from the vector-only warning"
        );
    }
}
