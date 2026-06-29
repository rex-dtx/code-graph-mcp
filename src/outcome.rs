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

#[derive(Debug, Clone)]
pub struct CallOutcome {
    pub tool: String,
    pub query: String,
    pub returned_files: Vec<String>,
    pub adopted: bool,
    pub adopted_rank: Option<usize>,
    pub ranked: bool,
}

pub fn score_session(events: &[Event]) -> Vec<CallOutcome> {
    use std::collections::HashSet;
    let mut touched_before: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (i, ev) in events.iter().enumerate() {
        match ev {
            Event::FileTouch { path } => { touched_before.insert(path.clone()); }
            Event::CgCall { tool, query, returned } => {
                // Candidate returned files not already opened before this call.
                let candidates: Vec<&ReturnedItem> = returned.iter()
                    .filter(|it| !touched_before.iter().any(|t| paths_match(&it.file_path, t)))
                    .collect();
                // Forward scan until the next CgCall.
                let mut best_rank: Option<usize> = None;
                let mut adopted = false;
                for ev2 in &events[i + 1..] {
                    match ev2 {
                        Event::CgCall { .. } => break,
                        Event::FileTouch { path } => {
                            for it in &candidates {
                                if paths_match(&it.file_path, path) {
                                    adopted = true;
                                    best_rank = match (best_rank, it.rank) {
                                        (None, r) => r,
                                        (Some(b), Some(r)) => Some(b.min(r)),
                                        (Some(b), None) => Some(b),
                                    };
                                }
                            }
                        }
                        _ => {}
                    }
                }
                out.push(CallOutcome {
                    tool: tool.clone(),
                    query: query.clone(),
                    returned_files: returned.iter().map(|r| r.file_path.clone()).collect(),
                    adopted,
                    adopted_rank: best_rank,
                    ranked: is_ranked_tool(tool),
                });
            }
            _ => {}
        }
    }
    out
}

pub const MIN_N: usize = 20;

#[derive(Debug, Default)]
pub struct OutcomeSummary {
    pub transcripts: usize,
    pub sessions: usize,
    pub cg_calls: usize,
    pub unresolved: usize,
    pub unparseable: usize,
    pub adopted: usize,
    pub adoption_rate: f64,
    pub ranked_calls: usize,
    pub ranked_adopted: usize,
    pub field_mrr_adopted: f64,
    pub field_mrr_all: f64,
    pub rank_histogram: std::collections::BTreeMap<usize, usize>,
    pub by_tool: std::collections::BTreeMap<String, (usize, usize)>, // tool -> (calls, adopted)
    pub low_confidence: bool,
}

pub fn aggregate(
    calls: &[CallOutcome],
    transcripts: usize,
    sessions: usize,
    unresolved: usize,
    unparseable: usize,
) -> OutcomeSummary {
    let mut s = OutcomeSummary {
        transcripts,
        sessions,
        unresolved,
        unparseable,
        cg_calls: calls.len(),
        ..Default::default()
    };
    let mut rr_adopted_sum = 0.0f64;
    let mut rr_all_sum = 0.0f64;
    for c in calls {
        let e = s.by_tool.entry(c.tool.clone()).or_insert((0, 0));
        e.0 += 1;
        if c.adopted {
            s.adopted += 1;
            e.1 += 1;
            if let Some(r) = c.adopted_rank {
                *s.rank_histogram.entry(r).or_insert(0) += 1;
            }
        }
        if c.ranked {
            s.ranked_calls += 1;
            let rr = if c.adopted {
                c.adopted_rank.map(|r| 1.0 / (r as f64 + 1.0)).unwrap_or(0.0)
            } else {
                0.0
            };
            rr_all_sum += rr;
            if c.adopted {
                s.ranked_adopted += 1;
                rr_adopted_sum += rr;
            }
        }
    }
    s.adoption_rate = if s.cg_calls > 0 { s.adopted as f64 / s.cg_calls as f64 } else { 0.0 };
    s.field_mrr_adopted = if s.ranked_adopted > 0 { rr_adopted_sum / s.ranked_adopted as f64 } else { 0.0 };
    s.field_mrr_all = if s.ranked_calls > 0 { rr_all_sum / s.ranked_calls as f64 } else { 0.0 };
    s.low_confidence = s.cg_calls < MIN_N;
    s
}

// ── Task 6: Orchestration, render, CLI wiring ────────────────────────────────

use anyhow::Result;
use clap::Parser;
use std::time::{Duration, SystemTime};

#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp outcome",
          about = "Measure whether code-graph retrieval results get adopted by the model (read-only; reads session transcripts)")]
pub struct OutcomeArgs {
    /// Project whose transcripts to read (absolute path; default: resolved project root)
    #[arg(long)]
    pub project: Option<String>,
    /// Only transcripts modified within the last N days
    #[arg(long)]
    pub since: Option<u64>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Append (query, returned, adopted, rank) label rows as JSONL to this path
    #[arg(long)]
    pub emit_labels: Option<String>,
}

/// Read every transcript in `dir`, score each session, aggregate. Pure-ish (only fs reads).
pub fn run_outcome(dir: &std::path::Path, since_days: Option<u64>) -> (OutcomeSummary, Vec<CallOutcome>) {
    let cutoff = since_days.map(|d| SystemTime::now() - Duration::from_secs(d * 86_400));
    let mut all_calls = Vec::new();
    let mut transcripts = 0usize;
    let mut unresolved = 0usize;
    let mut unparseable = 0usize;
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
        if let Some(cut) = cutoff {
            if let Ok(meta) = entry.metadata() {
                if meta.modified().map(|m| m < cut).unwrap_or(false) { continue; }
            }
        }
        let Ok(content) = std::fs::read_to_string(&path) else { continue; };
        let parsed = parse_transcript(&content);
        unresolved += parsed.unresolved;
        unparseable += parsed.unparseable;
        all_calls.extend(score_session(&parsed.events));
        transcripts += 1;
    }
    let summary = aggregate(&all_calls, transcripts, transcripts, unresolved, unparseable);
    (summary, all_calls)
}

pub fn cmd_outcome(project_root: &std::path::Path, args: OutcomeArgs) -> Result<()> {
    let home = crate::cli::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory ($HOME / $USERPROFILE not set)"))?;
    let target = match &args.project {
        Some(p) => std::path::PathBuf::from(p),
        None => project_root.to_path_buf(),
    };
    let dir = transcript_dir(&target, &home);
    if !dir.is_dir() {
        if args.json {
            println!("{}", serde_json::json!({"outcome": {"state": "absent", "dir": dir.display().to_string()}}));
        } else {
            eprintln!("No transcripts for {} at {}", target.display(), dir.display());
        }
        return Ok(());
    }
    let (s, calls) = run_outcome(&dir, args.since);
    if let Some(path) = &args.emit_labels {
        emit_labels(&calls, path)?;
    }
    if args.json { render_json(&s, &target); } else { render_human(&s, &target); }
    Ok(())
}

/// Stub — Task 7 replaces this with the real body that writes JSONL label rows.
pub fn emit_labels(_calls: &[CallOutcome], _path: &str) -> Result<()> { Ok(()) }

fn render_human(s: &OutcomeSummary, target: &std::path::Path) {
    println!("Outcome (retrieval adoption)  \u{2014}  project: {}", target.display());
    println!("Transcripts: {}   resolved cg calls: {}   (unresolved {}, unparseable {})",
             s.transcripts, s.cg_calls, s.unresolved, s.unparseable);
    if s.low_confidence {
        println!("LOW CONFIDENCE: N={} (< {}) \u{2014} too small to conclude", s.cg_calls, MIN_N);
    }
    println!("Adoption: {}/{} = {:.0}%", s.adopted, s.cg_calls, s.adoption_rate * 100.0);
    println!("field-MRR (ranked tools)  adopted: {:.2}   all: {:.2}", s.field_mrr_adopted, s.field_mrr_all);
    let hist: Vec<String> = s.rank_histogram.iter().map(|(r, n)| format!("r{r}={n}")).collect();
    println!("Adopted-rank histogram: {}", if hist.is_empty() { "-".into() } else { hist.join("  ") });
    for (tool, (calls, adopted)) in &s.by_tool {
        println!("  {:<24} {}/{}", tool, adopted, calls);
    }
}

fn render_json(s: &OutcomeSummary, target: &std::path::Path) {
    println!("{}", serde_json::json!({"outcome": {
        "state": "live",
        "project": target.display().to_string(),
        "transcripts": s.transcripts,
        "cg_calls": s.cg_calls,
        "unresolved": s.unresolved,
        "unparseable": s.unparseable,
        "adopted": s.adopted,
        "adoption_rate": (s.adoption_rate * 100.0).round() / 100.0,
        "ranked_calls": s.ranked_calls,
        "field_mrr_adopted": (s.field_mrr_adopted * 1000.0).round() / 1000.0,
        "field_mrr_all": (s.field_mrr_all * 1000.0).round() / 1000.0,
        "rank_histogram": s.rank_histogram.iter().map(|(k,v)| (k.to_string(), *v)).collect::<std::collections::BTreeMap<_,_>>(),
        "by_tool": s.by_tool.iter().map(|(k,(c,a))| (k.clone(), serde_json::json!({"calls": c, "adopted": a}))).collect::<serde_json::Map<_,_>>(),
        "low_confidence": s.low_confidence,
    }}));
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
        assert_eq!(p.last_ts.as_deref(), Some("2026-06-29T10:01:00Z"));
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

    #[test]
    fn parse_counts_unparseable_result_payload() {
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu1","name":"mcp__code-graph-dev__semantic_code_search","input":{"query":"q"}}]}}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":[{"type":"text","text":"not valid json"}]}]}}"#;
        let p = parse_transcript(&format!("{call}\n{result}\n"));
        assert_eq!(p.unparseable, 1);
        assert_eq!(p.unresolved, 0);
        assert!(p.events.iter().all(|e| !matches!(e, Event::CgCall { .. })));
    }

    // ── score_session helpers ──────────────────────────────────────────────

    fn cg(tool: &str, files: &[(&str, Option<usize>)]) -> Event {
        Event::CgCall {
            tool: tool.into(),
            query: "q".into(),
            returned: files.iter().map(|(f, r)| ReturnedItem { file_path: f.to_string(), rank: *r }).collect(),
        }
    }
    fn touch(p: &str) -> Event { Event::FileTouch { path: p.into() } }

    #[test]
    fn adopted_when_forward_edit_hits_returned_untouched_file() {
        let events = vec![
            cg("semantic_code_search", &[("src/a.rs", Some(0)), ("src/b.rs", Some(1))]),
            touch("/proj/src/b.rs"),
        ];
        let outs = score_session(&events);
        assert_eq!(outs.len(), 1);
        assert!(outs[0].adopted);
        assert_eq!(outs[0].adopted_rank, Some(1));
    }

    #[test]
    fn not_adopted_when_file_touched_before_the_call() {
        let events = vec![
            touch("/proj/src/a.rs"),
            cg("semantic_code_search", &[("src/a.rs", Some(0))]),
            touch("/proj/src/a.rs"),
        ];
        let outs = score_session(&events);
        assert!(!outs[0].adopted, "a.rs was already open before the call");
    }

    #[test]
    fn window_stops_at_next_cg_call() {
        let events = vec![
            cg("ast_search", &[("src/a.rs", Some(0))]),
            cg("ast_search", &[("src/z.rs", Some(0))]),
            touch("/proj/src/a.rs"), // after the 2nd call → not credited to the 1st
        ];
        let outs = score_session(&events);
        assert!(!outs[0].adopted); // a.rs touched after call 2 — outside call 1's window
        assert!(!outs[1].adopted); // call 2 returned z.rs; only a.rs was touched
    }

    #[test]
    fn best_rank_is_lowest_when_multiple_returned_items_are_touched() {
        // rank 2 and rank 0 both touched → adopted_rank should be Some(0) (the lower)
        let events = vec![
            cg("semantic_code_search", &[
                ("src/a.rs", Some(2)),
                ("src/b.rs", Some(0)),
                ("src/c.rs", Some(5)),
            ]),
            touch("/proj/src/a.rs"),
            touch("/proj/src/b.rs"),
        ];
        let outs = score_session(&events);
        assert!(outs[0].adopted);
        assert_eq!(outs[0].adopted_rank, Some(0), "lowest rank among touched items wins");
    }

    #[test]
    fn structural_tool_adopted_with_no_rank() {
        // get_call_graph is NOT ranked — returned items have rank: None
        // touching a returned file should still mark adopted=true; adopted_rank stays None
        let events = vec![
            cg("get_call_graph", &[("src/graph.rs", None), ("src/storage.rs", None)]),
            touch("/proj/src/graph.rs"),
        ];
        let outs = score_session(&events);
        assert!(outs[0].adopted, "structural tool file should be adopted when touched");
        assert_eq!(outs[0].adopted_rank, None, "structural items carry no rank");
        assert!(!outs[0].ranked, "get_call_graph is not a ranked tool");
    }

    #[test]
    fn parse_classifies_bash_grep_as_raw_grep() {
        let bash = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"rg some_fn src/"}}]}}"#;
        let p = parse_transcript(&format!("{bash}\n"));
        assert_eq!(p.events.len(), 1);
        assert!(matches!(&p.events[0], Event::RawGrep));
    }

    // ── aggregate / OutcomeSummary helpers ───────────────────────────────────

    fn co(tool: &str, ranked: bool, adopted: bool, rank: Option<usize>) -> CallOutcome {
        CallOutcome { tool: tool.into(), query: "q".into(), returned_files: vec![], adopted, adopted_rank: rank, ranked }
    }

    #[test]
    fn mrr_reported_two_ways() {
        let calls = vec![
            co("semantic_code_search", true, true, Some(0)),  // rr = 1.0
            co("semantic_code_search", true, true, Some(2)),  // rr = 1/3
            co("semantic_code_search", true, false, None),    // rr = 0 for _all only
        ];
        let s = aggregate(&calls, 1, 1, 0, 0);
        assert_eq!(s.cg_calls, 3);
        assert_eq!(s.adopted, 2);
        // adopted-only: mean(1.0, 0.333) = 0.667
        assert!((s.field_mrr_adopted - 0.6667).abs() < 0.001);
        // all ranked: mean(1.0, 0.333, 0.0) = 0.444
        assert!((s.field_mrr_all - 0.4444).abs() < 0.001);
        assert!(s.low_confidence); // 3 < MIN_N
    }

    #[test]
    fn structural_tools_excluded_from_mrr_but_counted_in_adoption() {
        let calls = vec![co("get_call_graph", false, true, None)];
        let s = aggregate(&calls, 1, 1, 0, 0);
        assert_eq!(s.adopted, 1);
        assert_eq!(s.ranked_calls, 0);
        assert_eq!(s.field_mrr_adopted, 0.0); // no ranked calls → 0, not NaN
    }

    #[test]
    fn parse_handles_bare_string_tool_result_content() {
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu1","name":"mcp__code-graph-dev__semantic_code_search","input":{"query":"q"}}]}}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"[{\"file_path\":\"src/a.rs\"}]"}]}}"#;
        let p = parse_transcript(&format!("{call}\n{result}\n"));
        assert_eq!(p.unresolved, 0);
        assert_eq!(p.unparseable, 0);
        match &p.events[0] {
            Event::CgCall { returned, .. } => assert_eq!(returned[0].file_path, "src/a.rs"),
            _ => panic!("expected CgCall"),
        }
    }

    #[test]
    fn run_outcome_e2e_over_temp_transcript_dir() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let mut f = std::fs::File::create(dir.path().join("session1.jsonl")).unwrap();
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"mcp__code-graph-dev__semantic_code_search","input":{"query":"q"}}]}}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"[{\"file_path\":\"src/a.rs\"}]"}]}]}}"#;
        let edit = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/x/src/a.rs"}}]}}"#;
        writeln!(f, "{call}\n{result}\n{edit}").unwrap();
        let (summary, calls) = run_outcome(dir.path(), None);
        assert_eq!(summary.cg_calls, 1);
        assert_eq!(summary.adopted, 1);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].adopted);
    }
}
