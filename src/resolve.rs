//! Shared symbol-resolution / ambiguity detection used by BOTH the CLI
//! (`src/cli.rs`) and the MCP server (`src/mcp/server`). Single source of truth
//! for "is this bare name ambiguous?" so the two surfaces never give
//! contradictory verdicts.
//!
//! Audit 2026-06-03 #6: the CLI previously gated ambiguity on distinct *files*
//! (`detect_exact_ambiguity`), while MCP gated on the *count* of non-test
//! definitions (`disambiguate_symbol`). For two `fn new()` in the same file the
//! CLI called it unique and silently merged their call graphs, while MCP
//! correctly refused — the same input, opposite answers. Both now delegate here.

use anyhow::Result;
use rusqlite::Connection;

use crate::storage::queries::{self, NameCandidate};

/// Which surface is rendering the message — only affects flag/tool wording
/// (`--file` vs `file_path`, `show --node-id` vs `get_ast_node`), never the
/// ambiguity decision itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Cli,
    Mcp,
}

/// Detect whether a bare symbol `name` resolves to ≥2 non-test definitions.
/// Returns the candidate definitions when ambiguous (same-file OR cross-file),
/// `None` when unique or not found.
///
/// Same-file multi-defs (e.g. two `fn new()` in one module for different impl
/// blocks) count as ambiguous because `file_path` alone can't split them — they
/// need a `node_id` (see `find_references` / `get_ast_node`). This is the gate
/// MCP already used; the CLI now shares it.
pub fn detect_ambiguity(conn: &Connection, name: &str) -> Result<Option<Vec<NameCandidate>>> {
    let with_files = queries::get_nodes_with_files_by_name(conn, name)?;
    let non_test: Vec<NameCandidate> = with_files
        .iter()
        .filter(|nf| !crate::domain::is_test_symbol(&nf.node.name, &nf.file_path))
        .map(|nf| NameCandidate {
            name: nf.node.name.clone(),
            file_path: nf.file_path.clone(),
            node_type: nf.node.node_type.clone(),
            node_id: nf.node.id,
            start_line: nf.node.start_line,
        })
        .collect();
    if non_test.len() > 1 {
        Ok(Some(non_test))
    } else {
        Ok(None)
    }
}

/// True when the candidates span ≥2 distinct files (cross-file collision, which
/// `file_path` *can* disambiguate). False when every definition lives in one
/// file (same-file overloads, which only `node_id` can split).
pub fn spans_multiple_files(cands: &[NameCandidate]) -> bool {
    let mut files = std::collections::HashSet::new();
    for c in cands {
        files.insert(c.file_path.as_str());
    }
    files.len() > 1
}

/// Render candidate definitions as the canonical JSON suggestion shape shared by
/// every tool's ambiguity response (`name` / `file_path` / `type` / `node_id` /
/// `start_line`). Single-sourced so CLI `--json` and MCP stay byte-identical.
pub fn candidates_to_json(cands: &[NameCandidate]) -> Vec<serde_json::Value> {
    cands
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "file_path": c.file_path,
                "type": c.node_type,
                "node_id": c.node_id,
                "start_line": c.start_line,
            })
        })
        .collect()
}

/// Build the accurate ambiguity message for `name` given its candidate defs.
///
/// Cross-file → advise the file selector (which works). Same-file overloads →
/// advise the `node_id` path via the node-oriented tools, because `get_call_graph`
/// / `impact` resolve by name and *cannot* split same-file defs; the file
/// selector would be a dead end. `surface` only swaps flag/tool names.
pub fn ambiguity_message(name: &str, cands: &[NameCandidate], surface: Surface) -> String {
    let n = cands.len();
    if spans_multiple_files(cands) {
        match surface {
            Surface::Cli => format!(
                "Ambiguous symbol '{name}': {n} matches in different files. Specify --file to disambiguate."
            ),
            Surface::Mcp => format!(
                "Ambiguous symbol '{name}': {n} matches in different files. Specify file_path to disambiguate."
            ),
        }
    } else {
        let file = cands.first().map(|c| c.file_path.as_str()).unwrap_or("");
        match surface {
            Surface::Cli => format!(
                "Ambiguous symbol '{name}': {n} definitions in the same file ({file}). \
                 callgraph/impact resolve by name and can't split same-file overloads — \
                 inspect a specific one with `show --node-id <N>` (node_ids below)."
            ),
            Surface::Mcp => format!(
                "Ambiguous symbol '{name}': {n} definitions in the same file ({file}). \
                 get_call_graph/impact_analysis resolve by name and can't split same-file \
                 overloads — pass a node_id below to get_ast_node or find_references."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::queries::NameCandidate;

    fn cand(name: &str, file: &str, id: i64, line: i64) -> NameCandidate {
        NameCandidate {
            name: name.to_string(),
            file_path: file.to_string(),
            node_type: "function".to_string(),
            node_id: id,
            start_line: line,
        }
    }

    #[test]
    fn same_file_overloads_do_not_span_multiple_files() {
        let cands = vec![cand("new", "lib.rs", 1, 5), cand("new", "lib.rs", 2, 9)];
        assert!(!spans_multiple_files(&cands));
    }

    #[test]
    fn cross_file_collision_spans_multiple_files() {
        let cands = vec![cand("open", "a.rs", 1, 5), cand("open", "b.rs", 2, 9)];
        assert!(spans_multiple_files(&cands));
    }

    #[test]
    fn cross_file_message_advises_file_selector() {
        let cands = vec![cand("open", "a.rs", 1, 5), cand("open", "b.rs", 2, 9)];
        // Stays byte-compatible with the pre-refactor MCP message + metrics
        // classifier fixture (mcp::metrics ErrKind::classify).
        assert_eq!(
            ambiguity_message("open", &cands, Surface::Mcp),
            "Ambiguous symbol 'open': 2 matches in different files. Specify file_path to disambiguate."
        );
        let cli = ambiguity_message("open", &cands, Surface::Cli);
        assert!(cli.contains("Specify --file"));
        assert!(!cli.contains("--node-id"), "cross-file: callgraph/impact have no --node-id");
    }

    #[test]
    fn same_file_message_advises_node_id_path() {
        let cands = vec![cand("new", "lib.rs", 1, 5), cand("new", "lib.rs", 2, 9)];
        let cli = ambiguity_message("new", &cands, Surface::Cli);
        assert!(cli.contains("same file"), "must name the same-file case: {cli}");
        assert!(cli.contains("--node-id"), "must point at the node_id path: {cli}");
        let mcp = ambiguity_message("new", &cands, Surface::Mcp);
        assert!(mcp.contains("same file"));
        assert!(mcp.contains("node_id"));
        // Both surfaces keep the "Ambiguous symbol" prefix so metrics classify
        // them as Ambiguous, not Other.
        assert!(cli.starts_with("Ambiguous symbol"));
        assert!(mcp.starts_with("Ambiguous symbol"));
    }
}
