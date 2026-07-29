use std::collections::{BTreeMap, HashSet};

use anyhow::ensure;

pub struct CompressedResult {
    pub node_id: i64,
    pub file_path: String,
    pub summary: String,
}

pub struct GroupedResult {
    pub file_path: String,
    pub summary: String,
    pub node_ids: Vec<i64>,
}

pub enum CompressedOutput {
    Nodes(Vec<CompressedResult>),
    Files(Vec<GroupedResult>),
    Directories(Vec<GroupedResult>),
}

/// Bytes of JSON framing a single result costs on the wire — keys, braces,
/// quotes, `node_id`, line numbers.
const RESULT_FRAMING_BYTES: usize = 80;

/// Token cost of ONE search result AS SERIALIZED, using the CHARS_PER_TOKEN
/// (bytes/token) ratio.
///
/// The single estimator for the search surfaces: the compression gate and the
/// compression LEVEL selector must agree, and they only agree if there is one
/// definition. The previous level-selector estimator preferred `context_string`,
/// which the response never carries, and so disagreed with the gate by orders of
/// magnitude (audit 2026-07-27).
///
/// `code_cap` is the caller's per-result `code_content` limit — estimate what
/// will be emitted, not what is in the row.
///
/// `.len()` is UTF-8 byte length, deliberately: see CHARS_PER_TOKEN's doc for
/// why byte-counting is CJK-correct without per-language branching.
pub fn estimate_result_tokens(
    code_content: &str,
    code_cap: usize,
    signature: Option<&str>,
    name: &str,
    file_path: &str,
) -> usize {
    (code_content.len().min(code_cap)
        + signature.map_or(0, |s| s.len())
        + name.len()
        + file_path.len()
        + RESULT_FRAMING_BYTES)
        / crate::domain::CHARS_PER_TOKEN
}

/// Estimate token count for a JSON value using CHARS_PER_TOKEN (bytes/token) ratio.
pub fn estimate_json_tokens(value: &serde_json::Value) -> usize {
    match serde_json::to_string(value) {
        Ok(s) => s.len() / crate::domain::CHARS_PER_TOKEN,
        Err(_) => 1, // conservative non-zero estimate on serialization failure
    }
}

/// Compress results if needed.
/// `file_paths` maps each result's index to its file path.
///
/// Returns multi-level compression based on token count:
/// - None: tokens <= threshold (no compression needed)
/// - Nodes (L1): tokens <= threshold * 3 (node summaries)
/// - Files (L2): tokens <= threshold * 8 (file groups)
/// - Directories (L3): tokens > threshold * 8 (directory groups)
///
/// `estimated_tokens` is supplied by the CALLER rather than recomputed here, and
/// that is the whole point. This function used to run its own estimator that
/// preferred `context_string` — a field the response never serializes — while
/// the caller's gate (fixed 2026-07-24) estimated from what it actually emits:
/// `code_content` capped at the surface's per-result limit, plus JSON framing.
/// The two numbers could differ by orders of magnitude, so a response the gate
/// judged barely over budget got compressed as if it were huge: ONE node with a
/// 20,956-byte context_string against 46 bytes of code was enough to demote L1
/// to L2. Only the caller knows the shape of its own payload, so only the caller
/// can estimate it — taking the number as a parameter makes a second, drifting
/// estimator impossible rather than merely unlikely.
pub fn compress_if_needed(
    results: &[crate::storage::queries::NodeResult],
    file_paths: &[String],
    token_threshold: usize,
    estimated_tokens: usize,
) -> anyhow::Result<Option<CompressedOutput>> {
    let tokens = estimated_tokens;
    if tokens <= token_threshold {
        Ok(None)
    } else if tokens <= token_threshold * 3 {
        Ok(Some(CompressedOutput::Nodes(compress_results(
            results, file_paths,
        )?)))
    } else if tokens <= token_threshold * 8 {
        Ok(Some(CompressedOutput::Files(compress_by_file(
            results, file_paths,
        )?)))
    } else {
        Ok(Some(CompressedOutput::Directories(compress_by_directory(
            results, file_paths,
        )?)))
    }
}

/// Compress results to summaries with node IDs for get_ast_node expansion
pub fn compress_results(
    results: &[crate::storage::queries::NodeResult],
    file_paths: &[String],
) -> anyhow::Result<Vec<CompressedResult>> {
    ensure!(
        results.len() == file_paths.len(),
        "results and file_paths must have same length"
    );
    Ok(results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let fp = file_paths.get(i).map(|s| s.as_str()).unwrap_or("?");
            let summary = format!(
                "{} {} in {} (lines {}-{}){}",
                r.node_type,
                r.name,
                fp,
                r.start_line,
                r.end_line,
                r.signature
                    .as_ref()
                    .map(|s| format!(" {}", s))
                    .unwrap_or_default(),
            );
            CompressedResult {
                node_id: r.id,
                file_path: fp.to_string(),
                summary,
            }
        })
        .collect())
}

/// Group results by file path, producing a summary per file.
pub fn compress_by_file(
    results: &[crate::storage::queries::NodeResult],
    file_paths: &[String],
) -> anyhow::Result<Vec<GroupedResult>> {
    ensure!(
        results.len() == file_paths.len(),
        "results and file_paths must have same length"
    );
    let mut groups: BTreeMap<String, (Vec<String>, Vec<i64>)> = BTreeMap::new();
    for (i, r) in results.iter().enumerate() {
        let fp = file_paths.get(i).map(|s| s.as_str()).unwrap_or("?");
        let entry = groups
            .entry(fp.to_string())
            .or_insert_with(|| (Vec::new(), Vec::new()));
        entry.0.push(format!("{} {}", r.node_type, r.name));
        entry.1.push(r.id);
    }
    Ok(groups
        .into_iter()
        .map(|(file_path, (symbols, node_ids))| {
            let n = symbols.len();
            let summary = format!("{}: [{}] ({} symbols)", file_path, symbols.join(", "), n);
            GroupedResult {
                file_path,
                summary,
                node_ids,
            }
        })
        .collect())
}

/// Group results by parent directory, producing a summary per directory.
pub fn compress_by_directory(
    results: &[crate::storage::queries::NodeResult],
    file_paths: &[String],
) -> anyhow::Result<Vec<GroupedResult>> {
    ensure!(
        results.len() == file_paths.len(),
        "results and file_paths must have same length"
    );
    let mut groups: BTreeMap<String, (HashSet<String>, Vec<i64>, usize)> = BTreeMap::new();
    for (i, r) in results.iter().enumerate() {
        let fp = file_paths.get(i).map(|s| s.as_str()).unwrap_or("?");
        let dir = match fp.rfind('/') {
            Some(pos) => &fp[..pos],
            None => ".",
        };
        let entry = groups
            .entry(dir.to_string())
            .or_insert_with(|| (HashSet::new(), Vec::new(), 0));
        entry.0.insert(fp.to_string());
        entry.1.push(r.id);
        entry.2 += 1;
    }
    Ok(groups
        .into_iter()
        .map(|(dir, (files, node_ids, symbol_count))| {
            let summary = format!("{}: {} files, {} symbols", dir, files.len(), symbol_count);
            GroupedResult {
                file_path: dir,
                summary,
                node_ids,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::queries::NodeResult;

    fn default_node() -> NodeResult {
        NodeResult {
            id: 0,
            file_id: 0,
            node_type: "function".into(),
            name: "default".into(),
            qualified_name: None,
            start_line: 1,
            end_line: 5,
            code_content: "".into(),
            signature: None,
            doc_comment: None,
            context_string: None,
            name_tokens: None,
            return_type: None,
            param_types: None,
            is_test: false,
        }
    }

    #[test]
    fn test_compress_returns_summaries_with_node_ids() {
        let results = vec![
            NodeResult {
                id: 1,
                name: "foo".into(),
                signature: Some("() -> i32".into()),
                code_content: "x".repeat(500),
                ..default_node()
            },
            NodeResult {
                id: 2,
                name: "bar".into(),
                signature: Some("(x: str) -> bool".into()),
                code_content: "y".repeat(500),
                ..default_node()
            },
        ];
        let file_paths = vec!["src/main.rs".to_string(), "src/lib.rs".to_string()];
        let compressed = compress_results(&results, &file_paths).unwrap();
        assert_eq!(compressed.len(), 2);
        assert_eq!(compressed[0].node_id, 1);
        assert!(compressed[0].summary.contains("foo"));
        assert!(compressed[0].summary.contains("src/main.rs"));
        assert!(compressed[0].summary.contains("() -> i32"));
    }

    #[test]
    fn test_compress_by_file() {
        let results = vec![
            NodeResult {
                id: 1,
                name: "foo".into(),
                node_type: "function".into(),
                code_content: "x".repeat(100),
                start_line: 1,
                end_line: 5,
                ..default_node()
            },
            NodeResult {
                id: 2,
                name: "bar".into(),
                node_type: "function".into(),
                code_content: "x".repeat(100),
                start_line: 10,
                end_line: 15,
                ..default_node()
            },
            NodeResult {
                id: 3,
                name: "baz".into(),
                node_type: "class".into(),
                code_content: "x".repeat(100),
                start_line: 1,
                end_line: 20,
                ..default_node()
            },
        ];
        let file_paths = vec![
            "src/auth.ts".into(),
            "src/auth.ts".into(),
            "src/models.ts".into(),
        ];
        let compressed = compress_by_file(&results, &file_paths).unwrap();
        assert_eq!(compressed.len(), 2);
        let auth_entry = compressed
            .iter()
            .find(|c| c.file_path == "src/auth.ts")
            .unwrap();
        assert!(auth_entry.summary.contains("foo"));
        assert!(auth_entry.summary.contains("bar"));
        assert!(auth_entry.node_ids.contains(&1));
        assert!(auth_entry.node_ids.contains(&2));
    }

    #[test]
    fn test_compress_by_directory() {
        let results = vec![
            NodeResult {
                id: 1,
                name: "a".into(),
                ..default_node()
            },
            NodeResult {
                id: 2,
                name: "b".into(),
                ..default_node()
            },
            NodeResult {
                id: 3,
                name: "c".into(),
                ..default_node()
            },
        ];
        let file_paths = vec![
            "src/auth/login.ts".into(),
            "src/auth/token.ts".into(),
            "src/models/user.ts".into(),
        ];
        let compressed = compress_by_directory(&results, &file_paths).unwrap();
        assert_eq!(compressed.len(), 2);
        let auth_dir = compressed
            .iter()
            .find(|c| c.file_path.contains("auth"))
            .unwrap();
        assert!(auth_dir.summary.contains("2 files"));
    }

    /// Helper mirroring the search surface: cap 500 (MAX_SEARCH_CODE_LEN).
    fn est(code: &str) -> usize {
        estimate_result_tokens(code, 500, None, "default", "src/a.rs")
    }

    #[test]
    fn test_estimate_result_tokens_respects_the_code_cap() {
        // Small content stays small.
        assert!(est("short") < 2000);
        // Large content is CAPPED, because the response caps it too — the old
        // estimator read the uncapped row and fired compression on payloads
        // that would have fit.
        let capped = est(&"x".repeat(9000));
        assert!(
            capped < 2000,
            "9000 bytes of code serialize as at most 500, got {capped} tokens"
        );
        assert_eq!(
            capped,
            est(&"x".repeat(500)),
            "anything at or over the cap must estimate identically"
        );
    }

    /// CJK contract: estimator counts UTF-8 BYTES not chars, so a CJK string
    /// of N chars (3N bytes) estimates to ~N tokens — matching BPE reality
    /// (~1 token/CJK-char). Regression guard against someone "fixing" the
    /// estimator to char-count and silently halving CJK budgets.
    #[test]
    fn test_estimate_tokens_cjk_byte_based() {
        // 1000 CJK chars = 3000 UTF-8 bytes, but the surface caps code at 500
        // bytes, so compare at a length both spellings survive intact: 150 CJK
        // chars = 450 bytes → ~150 tokens, plus the fixed JSON framing every
        // result pays (named here rather than folded into a loose band, so the
        // assertion still pins the bytes/3 ratio itself).
        let framing_tokens = RESULT_FRAMING_BYTES / crate::domain::CHARS_PER_TOKEN;
        let est_cjk = estimate_result_tokens(&"你".repeat(150), 500, None, "", "");
        let cjk_content = est_cjk - framing_tokens;
        assert!(
            (140..=160).contains(&cjk_content),
            "150 CJK chars (450 bytes) must estimate ~150 content tokens (bytes/3), got {cjk_content}",
        );
        // 150 ASCII chars (150 bytes) → ~50 tokens — confirms the divisor is
        // bytes-based (an ill-fixed char-based version would report ~50 here
        // too, but would WRONGLY report ~50 for the CJK case above).
        let est_ascii = estimate_result_tokens(&"x".repeat(150), 500, None, "", "");
        assert!(
            est_ascii < est_cjk,
            "150 CJK chars must estimate higher than 150 ASCII chars: cjk={est_cjk} ascii={est_ascii}",
        );
    }

    #[test]
    fn test_compression_level_uses_the_callers_serialized_estimate() {
        // Split brain, audit 2026-07-27: the GATE in tool_semantic_search was
        // fixed on 07-24 to estimate from what it actually serializes
        // (code_content capped at MAX_SEARCH_CODE_LEN + JSON framing), but the
        // LEVEL selector inside compress_if_needed re-estimated from
        // context_string — a field that is NOT in the response at all. Measured:
        // ONE node with context_string 20,956 B against code_content 46 B was
        // enough to push a response that belongs at L1 (node summaries) down to
        // L2 (file groups), discarding per-node detail the budget could afford.
        let threshold = 100;
        let results = vec![NodeResult {
            id: 1,
            name: "tiny".into(),
            code_content: "fn tiny() {}".into(),
            // Deliberately enormous and deliberately never serialized.
            context_string: Some("x".repeat(20_956)),
            ..default_node()
        }];
        let file_paths = vec!["src/a.rs".to_string()];

        // The caller's own estimate — just past the threshold, so L1.
        let out = compress_if_needed(&results, &file_paths, threshold, threshold + 1)
            .unwrap()
            .expect("over threshold must compress");
        assert!(
            matches!(out, CompressedOutput::Nodes(_)),
            "an estimate just over the threshold must select L1 regardless of context_string size"
        );

        // Negative control: the level still tracks the estimate it is handed, so
        // a genuinely large payload still escalates.
        let out = compress_if_needed(&results, &file_paths, threshold, threshold * 4)
            .unwrap()
            .expect("over threshold must compress");
        assert!(
            matches!(out, CompressedOutput::Files(_)),
            "4x threshold must still select L2"
        );

        // And an estimate under the threshold means no compression at all.
        assert!(
            compress_if_needed(&results, &file_paths, threshold, threshold - 1)
                .unwrap()
                .is_none(),
            "under threshold must not compress"
        );
    }
}
