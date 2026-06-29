use std::path::{Path, PathBuf};

/// cg PULL tools whose results are relevance-ordered → rank is meaningful.
const RANKED_TOOLS: &[&str] = &["semantic_code_search", "ast_search"];

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
}
