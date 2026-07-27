//! Tool handlers split per family. Each child module adds an `impl McpServer`
//! block that contributes the `tool_*` method called by `handle_tool` in
//! `super::mod`. The dispatcher itself stays in `super::mod` next to the
//! JSON-RPC plumbing.
//!
//! v0.18.4 split: previously one 2354-line file. The split is mechanical (no
//! semantics changed) and bisectable — the matching commit "refactor(mcp):
//! split server/tools.rs into per-tool modules" is the diff target if you're
//! cherry-picking history.

mod advanced;
mod ast_node;
mod ast_search;
mod callgraph;
mod management;
mod overview;
mod project_map;
mod refs;
mod search;

/// Normalize a caller-supplied `file_path`/`path` tool argument to the `/`
/// spelling the index stores, at TOOL ENTRY — before the value is used either
/// as a freshness target or as an index lookup key.
///
/// Why entry and not inside the freshness helper: `ensure_file_fresh_opt`
/// normalizes internally, but its normalized value is local — it returns
/// `Result<()>`, so every caller went on to hand the RAW argument to
/// `get_nodes_by_file_path` / `get_call_graph_filtered` / `get_module_exports`.
/// An MCP client on Windows that echoes back `src\Foo.cs` therefore refreshed
/// the right file and then missed the index (which stores `src/Foo.cs`),
/// reporting `File 'src\Foo.cs' not found in index` for an indexed file — the
/// issue-#34 failure mode, half-fixed. Normalizing at entry also covers the
/// `should_skip_indexing` branch, where the freshness helper never runs at all.
///
/// MCP paths are root-relative by contract, so this is separator normalization
/// only — deliberately NOT `cli::normalize_user_path`, which additionally
/// resolves against the process cwd (see `indexer::pipeline` docs).
pub(super) fn normalize_path_arg(raw: &str) -> String {
    crate::indexer::merkle::normalize_rel_str(raw)
}
