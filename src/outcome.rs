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

#[derive(Debug, Clone)]
pub enum Event {
    CgCall { tool: String, query: String, returned: Vec<ReturnedItem> },
    FileTouch { path: String },
    RawGrep,
    Other,
}

#[derive(Debug, Default)]
pub struct ParsedTranscript {
    pub events: Vec<Event>,
    pub unresolved: usize,
    pub unparseable: usize,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
}

/// Pull the inner text payload out of a tool_result's `content` (array of
/// {type:text,text} blocks, or a bare string).
fn tool_result_text(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() { return Some(s.to_string()); }
    if let Some(arr) = content.as_array() {
        for block in arr {
            if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                return Some(t.to_string());
            }
        }
    }
    None
}

pub fn parse_transcript(content: &str) -> ParsedTranscript {
    use std::collections::HashMap;
    // Pass 1: tool_use_id -> result payload text.
    let mut results: HashMap<String, String> = HashMap::new();
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue; };
        let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) else { continue; };
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                if let (Some(id), Some(text)) = (
                    b.get("tool_use_id").and_then(|x| x.as_str()),
                    b.get("content").and_then(|c| tool_result_text(c)),
                ) {
                    results.insert(id.to_string(), text);
                }
            }
        }
    }
    // Pass 2: build events in order from tool_use blocks.
    let mut out = ParsedTranscript::default();
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue; };
        if let Some(ts) = v.get("timestamp").and_then(|x| x.as_str()) {
            if out.first_ts.is_none() { out.first_ts = Some(ts.to_string()); }
            out.last_ts = Some(ts.to_string());
        }
        let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) else { continue; };
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") { continue; }
            let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let id = b.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let input = b.get("input").cloned().unwrap_or(serde_json::Value::Null);
            if let Some(tool) = cg_pull_tool(name) {
                match results.get(id) {
                    None => out.unresolved += 1,
                    Some(text) => match serde_json::from_str::<serde_json::Value>(text) {
                        Ok(payload) => {
                            let query = input.get("query").or_else(|| input.get("symbol"))
                                .or_else(|| input.get("name"))
                                .and_then(|x| x.as_str()).unwrap_or("").to_string();
                            let returned = extract_returned(&payload, is_ranked_tool(&tool));
                            out.events.push(Event::CgCall { tool, query, returned });
                        }
                        Err(_) => out.unparseable += 1,
                    },
                }
            } else if name == "Read" || name == "Edit" || name == "Write" {
                if let Some(fp) = input.get("file_path").and_then(|x| x.as_str()) {
                    out.events.push(Event::FileTouch { path: fp.to_string() });
                }
            } else if name == "Bash" {
                let cmd = input.get("command").and_then(|x| x.as_str()).unwrap_or("");
                if cmd.contains("grep ") || cmd.contains("rg ") {
                    out.events.push(Event::RawGrep);
                }
            }
        }
    }
    out
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

    #[test]
    fn parse_pairs_cg_call_with_result_then_edit() {
        let call = r#"{"type":"assistant","timestamp":"2026-06-29T10:00:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu1","name":"mcp__code-graph-dev__semantic_code_search","input":{"query":"login flow"}}]}}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":[{"type":"text","text":"[{\"file_path\":\"src/auth.rs\",\"name\":\"login\"}]"}]}]}}"#;
        let edit = r#"{"type":"assistant","timestamp":"2026-06-29T10:01:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu2","name":"Edit","input":{"file_path":"/proj/src/auth.rs"}}]}}"#;
        let content = format!("{call}\n{result}\n{edit}\n");
        let p = parse_transcript(&content);
        assert_eq!(p.unresolved, 0);
        assert_eq!(p.events.len(), 2);
        match &p.events[0] {
            Event::CgCall { tool, query, returned } => {
                assert_eq!(tool, "semantic_code_search");
                assert_eq!(query, "login flow");
                assert_eq!(returned[0].file_path, "src/auth.rs");
                assert_eq!(returned[0].rank, Some(0));
            }
            _ => panic!("expected CgCall"),
        }
        assert!(matches!(&p.events[1], Event::FileTouch { path } if path == "/proj/src/auth.rs"));
        assert_eq!(p.first_ts.as_deref(), Some("2026-06-29T10:00:00Z"));
    }

    #[test]
    fn parse_counts_unresolved_cg_call() {
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tuX","name":"mcp__code-graph-dev__ast_search","input":{"query":"q"}}]}}"#;
        let p = parse_transcript(&format!("{call}\n"));
        assert_eq!(p.unresolved, 1);
        assert!(p.events.iter().all(|e| !matches!(e, Event::CgCall { .. })));
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let p = parse_transcript("not json\n{}\n");
        assert_eq!(p.events.len(), 0);
    }
}
