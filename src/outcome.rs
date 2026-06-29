use std::path::{Path, PathBuf};

/// cg PULL tools whose results are relevance-ordered → rank is meaningful.
pub const RANKED_TOOLS: &[&str] = &["semantic_code_search", "ast_search"];

/// If `name` is a code-graph MCP tool_use name (`mcp__code-graph[-dev]__<tool>`),
/// return the bare `<tool>` when it is one of the known cg query tools. The server
/// namespace varies (`code-graph` marketplace vs `code-graph-dev` dogfood), so match
/// on the trailing `__<tool>` segment, not the full prefix.
pub fn cg_pull_tool(name: &str) -> Option<String> {
    let base = name.rsplit("__").next().unwrap_or(name);
    if name.starts_with("mcp__") && name.contains("code-graph")
        && crate::domain::LIVE_MCP_TOOLS.contains(&base)
    {
        Some(base.to_string())
    } else {
        None
    }
}

pub fn is_ranked_tool(base: &str) -> bool {
    RANKED_TOOLS.contains(&base)
}

/// Encode an absolute project path to its Claude Code transcript-dir slug:
/// every `/` and `.` becomes `-` (mirrors `claude-plugin/scripts/adopt.js`
/// `memoryDir`). Verified by the integration test reading daagu's real dir.
pub fn project_slug(abs_path: &str) -> String {
    abs_path.chars().map(|c| if c == '/' || c == '.' { '-' } else { c }).collect()
}

pub fn transcript_dir(target: &Path, home: &Path) -> PathBuf {
    let slug = project_slug(&target.to_string_lossy());
    home.join(".claude").join("projects").join(slug)
}

/// True if `touched` (often absolute, from Read/Edit) ends with `returned` (often
/// repo-relative, from a cg result), compared by trailing path components. The
/// returned path carries directory context so basename collisions are unlikely.
pub fn paths_match(returned: &str, touched: &str) -> bool {
    let split = |s: &str| s.trim_start_matches('/').split('/').filter(|p| !p.is_empty())
        .map(|p| p.to_string()).collect::<Vec<_>>();
    let r = split(returned);
    let t = split(touched);
    if r.is_empty() || t.is_empty() { return false; }
    let (long, short) = if t.len() >= r.len() { (&t, &r) } else { (&r, &t) };
    long[long.len() - short.len()..] == short[..]
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReturnedItem {
    pub file_path: String,
    pub rank: Option<usize>,
}

/// Extract the returned files from a cg tool_result payload. Ranked-list tools
/// (`ranked == true`) return a top-level JSON array → array index is the rank.
/// Structural tools return a nested object/tree → recursively collect every
/// `file_path`/`file`/`path` string value, rank = None. Robust to per-tool shape.
pub fn extract_returned(payload: &serde_json::Value, ranked: bool) -> Vec<ReturnedItem> {
    if ranked {
        if let Some(arr) = payload.as_array() {
            return arr.iter().enumerate().filter_map(|(i, el)| {
                file_path_field(el).map(|fp| ReturnedItem { file_path: fp, rank: Some(i) })
            }).collect();
        }
    }
    let mut out = Vec::new();
    collect_file_paths(payload, &mut out);
    out
}

/// First of `file_path` / `file` / `path` that is a non-empty string.
fn file_path_field(v: &serde_json::Value) -> Option<String> {
    for key in ["file_path", "file", "path"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() { return Some(s.to_string()); }
        }
    }
    None
}

fn collect_file_paths(v: &serde_json::Value, out: &mut Vec<ReturnedItem>) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(fp) = file_path_field(v) {
                out.push(ReturnedItem { file_path: fp, rank: None });
            }
            for (_, val) in map { collect_file_paths(val, out); }
        }
        serde_json::Value::Array(arr) => {
            for el in arr { collect_file_paths(el, out); }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cg_pull_tool_matches_namespaced_cg_tools() {
        assert_eq!(cg_pull_tool("mcp__code-graph-dev__semantic_code_search").as_deref(), Some("semantic_code_search"));
        assert_eq!(cg_pull_tool("mcp__code-graph__get_call_graph").as_deref(), Some("get_call_graph"));
        assert_eq!(cg_pull_tool("Read"), None);
        assert_eq!(cg_pull_tool("mcp__other__semantic_code_search"), None);
        assert_eq!(cg_pull_tool("mcp__code-graph-dev__no_such_tool"), None);
    }

    #[test]
    fn ranked_vs_structural() {
        assert!(is_ranked_tool("semantic_code_search"));
        assert!(is_ranked_tool("ast_search"));
        assert!(!is_ranked_tool("get_call_graph"));
    }

    #[test]
    fn slug_replaces_slash_and_dot() {
        assert_eq!(project_slug("/mnt/data_ssd/dev/projects/code-graph-mcp"),
                   "-mnt-data_ssd-dev-projects-code-graph-mcp");
        assert_eq!(project_slug("/home/sds/.claude/x"), "-home-sds--claude-x");
    }

    #[test]
    fn paths_match_relative_vs_absolute() {
        assert!(paths_match("claude-plugin/scripts/session-init.js",
                            "/home/u/proj/claude-plugin/scripts/session-init.js"));
        assert!(paths_match("src/outcome.rs", "/x/src/outcome.rs"));
        assert!(!paths_match("src/outcome.rs", "/x/src/cli.rs"));
        assert!(!paths_match("", "/x/y"));
    }

    #[test]
    fn transcript_dir_joins_claude_projects() {
        assert_eq!(
            transcript_dir(std::path::Path::new("/a/b"), std::path::Path::new("/home/u")),
            std::path::PathBuf::from("/home/u/.claude/projects/-a-b")
        );
    }

    #[test]
    fn paths_match_when_returned_is_the_longer_path() {
        // returned absolute, touched relative — exercises the (long, short) swap
        assert!(paths_match("/x/src/outcome.rs", "src/outcome.rs"));
    }

    #[test]
    fn extract_ranked_array_assigns_index_rank() {
        let payload = serde_json::json!([
            {"file_path": "a/b.rs", "relevance": 0.9, "name": "f"},
            {"file_path": "c/d.rs", "relevance": 0.5, "name": "g"}
        ]);
        let items = extract_returned(&payload, true);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].file_path, "a/b.rs");
        assert_eq!(items[0].rank, Some(0));
        assert_eq!(items[1].rank, Some(1));
    }

    #[test]
    fn extract_structural_tree_collects_paths_without_rank() {
        // callgraph-style nested payload: file_path values buried in callers/callees.
        let payload = serde_json::json!({
            "symbol": "foo",
            "callers": [{"name": "x", "file_path": "src/x.rs"}],
            "callees": [{"name": "y", "file": "src/y.rs"}]
        });
        let items = extract_returned(&payload, false);
        let paths: Vec<&str> = items.iter().map(|i| i.file_path.as_str()).collect();
        assert!(paths.contains(&"src/x.rs"));
        assert!(paths.contains(&"src/y.rs"));
        assert!(items.iter().all(|i| i.rank.is_none()));
    }

    #[test]
    fn extract_handles_empty_and_garbage() {
        assert!(extract_returned(&serde_json::json!([]), true).is_empty());
        assert!(extract_returned(&serde_json::json!("oops"), true).is_empty());
    }
}
