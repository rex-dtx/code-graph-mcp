# Outcome Phase-2 (CLI parsing) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend `code-graph-mcp outcome` to count model-initiated CLI cg calls (`code-graph-mcp <query-subcmd>` typed in a Bash tool_use), so the adoption metric reflects real consumer PULL (daagu ≈46 such calls; v1 read 0).

**Architecture:** Add a dependency-free `path:line` scanner + `extract_returned_from_cli` to `src/outcome.rs`, then teach `parse_transcript`'s Bash branch to detect `code-graph-mcp <subcmd>` (via the existing `cli::canonical_query_cmd`) and emit a `CgCall` named `<canon>_cli` carrying the files scanned from the command's stdout. `search` is ranked (joins field-MRR); all other CLI subcommands are structural (adoption only). Scoring/aggregation/labels are unchanged — CLI `CgCall`s flow through the v1 pipeline.

**Tech Stack:** Rust 2021, `serde_json` (JSON fast-path), `anyhow`, inline `#[cfg(test)] mod tests`. NO new runtime dependency (hand-rolled scan — `regex` is not a dependency).

## Global Constraints

- **No `INDEX_VERSION` / `SCHEMA_VERSION` bump** — read-only; touches no index/DB.
- **No new runtime dependency** — the path scanner is hand-rolled (project has no `regex` dep).
- **Reuse `crate::cli::canonical_query_cmd(sub) -> Option<&'static str>`** (`src/cli.rs:1015`) as the single source of truth for which subcommands are cg queries.
- **`search` ranked, all other CLI subcommands structural** — CLI tool named `<canon>_cli`; `is_ranked_tool` recognizes only `search_cli` among CLI tools (do NOT fabricate ranks for `grep`/`callgraph` — that pollutes field-MRR).
- **Anchor on transcript `tool_use` only** — the model's Bash tool_use invoking `code-graph-mcp`. Hooks run out-of-band and are not tool_use, so not counted.
- Commit format `<type>(<scope>): <subject>` ending with `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. Pre-commit runs `cargo check`+`cargo test` on staged `.rs` — let it pass, no `--no-verify`.

---

## File Structure

All changes are in the existing `src/outcome.rs` (the v1 module). New private `scan_path_line_paths`, new public `extract_returned_from_cli`, new private `detect_cli_cg_call`; `RANKED_TOOLS` gains `"search_cli"`; the Bash arm of `parse_transcript` gains a cg-CLI branch before the RawGrep check.

---

## Task 1: CLI stdout → returned files (pure)

**Files:**
- Modify: `src/outcome.rs`

**Interfaces:**
- Consumes: `extract_returned` + `ReturnedItem` (v1), `serde_json::Value`.
- Produces: `pub fn extract_returned_from_cli(stdout: &str, ranked: bool) -> Vec<ReturnedItem>`, private `fn scan_path_line_paths(text: &str) -> Vec<String>`, and `"search_cli"` added to `RANKED_TOOLS`.

- [ ] **Step 1: Write the failing tests** (in the existing `#[cfg(test)] mod tests`)
```rust
    #[test]
    fn scan_extracts_path_line_tokens() {
        // grep-style + search-style lines; path:line and path:line-line
        let text = "src/a.rs:5  fn foo\nsrc/a.rs:9  bar\nh3 Title  CHANGELOG.md:3708-3709";
        let paths = scan_path_line_paths(text);
        assert_eq!(paths, vec!["src/a.rs", "src/a.rs", "CHANGELOG.md"]); // raw, with dups
    }

    #[test]
    fn cli_extract_human_dedupes_first_occurrence() {
        let stdout = "src/a.rs:5\nsrc/a.rs:9\nsrc/b.rs:2";
        let items = extract_returned_from_cli(stdout, false);
        let paths: Vec<&str> = items.iter().map(|i| i.file_path.as_str()).collect();
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs"]); // unique, first-occurrence order
        assert!(items.iter().all(|i| i.rank.is_none()));
    }

    #[test]
    fn cli_extract_ranked_assigns_index_rank() {
        let stdout = "h3 A  x/a.rs:1-2\nh3 B  y/b.rs:3-4";
        let items = extract_returned_from_cli(stdout, true);
        assert_eq!(items[0].file_path, "x/a.rs");
        assert_eq!(items[0].rank, Some(0));
        assert_eq!(items[1].rank, Some(1));
    }

    #[test]
    fn cli_extract_json_fast_path() {
        let stdout = r#"{"results":[{"file_path":"src/z.rs"}]}"#;
        let items = extract_returned_from_cli(stdout, true);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].file_path, "src/z.rs");
        assert_eq!(items[0].rank, Some(0));
    }

    #[test]
    fn cli_extract_no_paths_is_empty() {
        assert!(extract_returned_from_cli("[code-graph] No call graph results for: foo", false).is_empty());
    }

    #[test]
    fn search_cli_is_ranked() {
        assert!(is_ranked_tool("search_cli"));
        assert!(!is_ranked_tool("callgraph_cli"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib outcome::tests::cli outcome::tests::scan outcome::tests::search_cli`
Expected: FAIL — `scan_path_line_paths` / `extract_returned_from_cli` not defined; `search_cli` not ranked.

- [ ] **Step 3: Implement**
```rust
// add "search_cli" to the existing RANKED_TOOLS slice:
const RANKED_TOOLS: &[&str] = &["semantic_code_search", "ast_search", "search_cli"];

/// Extract file paths from cg CLI human output, where hits appear as `path:line`
/// or `path:line-line` (e.g. `src/foo.rs:63`, `CHANGELOG.md:3708-3709`). Returns the
/// path part of each `<path-like>:<digit>` token, in order WITH duplicates (caller
/// dedupes). A path-like token contains `/` or `.` and ends just before the `:`.
fn scan_path_line_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b':' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
                let mut start = i;
                while start > 0 {
                    let c = b[start - 1];
                    if c == b'/' || c == b'.' || c == b'-' || c == b'_' || c.is_ascii_alphanumeric() {
                        start -= 1;
                    } else {
                        break;
                    }
                }
                let token = &line[start..i];
                if !token.is_empty() && (token.contains('/') || token.contains('.')) {
                    out.push(token.to_string());
                }
            }
            i += 1;
        }
    }
    out
}

/// Returned files from a model-initiated cg CLI call's stdout. JSON fast-path
/// (model passed `--json` → same `{results}` shape as MCP); else scan human
/// `path:line` tokens. Dedupe to unique paths in first-occurrence order; `ranked`
/// → rank = first-occurrence index, else None.
pub fn extract_returned_from_cli(stdout: &str, ranked: bool) -> Vec<ReturnedItem> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        return extract_returned(&v, ranked);
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for path in scan_path_line_paths(stdout) {
        if seen.insert(path.clone()) {
            let rank = if ranked { Some(out.len()) } else { None };
            out.push(ReturnedItem { file_path: path, rank });
        }
    }
    out
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib outcome::tests`
Expected: all pass (the 6 new + all existing).

- [ ] **Step 5: Commit**
```bash
git add src/outcome.rs
git commit -m "feat(outcome): CLI stdout path extraction (path:line scan + JSON fast-path)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: detect cg-CLI calls in parse_transcript

**Files:**
- Modify: `src/outcome.rs` (the `name == "Bash"` arm inside `parse_transcript`'s pass 2, and a new helper)

**Interfaces:**
- Consumes: `extract_returned_from_cli` + `is_ranked_tool` (Task 1), `crate::cli::canonical_query_cmd` (`src/cli.rs:1015`), the existing `results` (id→stdout) map and `Event::CgCall`.
- Produces: private `fn detect_cli_cg_call(cmd: &str) -> Option<&'static str>`; the Bash arm now emits `Event::CgCall { tool: "<canon>_cli", .. }` for cg-CLI commands.

- [ ] **Step 1: Write the failing tests**
```rust
    #[test]
    fn parse_detects_cli_callgraph_as_cgcall_not_rawgrep() {
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"code-graph-mcp callgraph Foo"}}]}}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"b1","content":[{"type":"text","text":"src/foo.rs:10  fn Foo"}]}]}}"#;
        let p = parse_transcript(&format!("{call}\n{result}\n"));
        assert_eq!(p.events.len(), 1);
        match &p.events[0] {
            Event::CgCall { tool, returned, .. } => {
                assert_eq!(tool, "callgraph_cli");
                assert_eq!(returned[0].file_path, "src/foo.rs");
                assert_eq!(returned[0].rank, None); // structural
            }
            _ => panic!("expected CgCall, got RawGrep/Other"),
        }
    }

    #[test]
    fn parse_detects_cli_search_ranked_and_compound() {
        // compound command (cd && code-graph-mcp search) + ranked search_cli
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"b2","name":"Bash","input":{"command":"cd backend && code-graph-mcp search \"login\""}}]}}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"b2","content":[{"type":"text","text":"h3 a  src/a.rs:1-2\nh3 b  src/b.rs:3-4"}]}]}}"#;
        let p = parse_transcript(&format!("{call}\n{result}\n"));
        match &p.events[0] {
            Event::CgCall { tool, returned, .. } => {
                assert_eq!(tool, "search_cli");
                assert_eq!(returned[0].rank, Some(0));
                assert_eq!(returned[1].rank, Some(1));
            }
            _ => panic!("expected search_cli CgCall"),
        }
    }

    #[test]
    fn parse_raw_grep_still_rawgrep_and_housekeeping_ignored() {
        let raw = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"g1","name":"Bash","input":{"command":"grep -rn foo src/"}}]}}"#;
        let house = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"s1","name":"Bash","input":{"command":"code-graph-mcp stats"}}]}}"#;
        let p = parse_transcript(&format!("{raw}\n{house}\n"));
        assert!(matches!(&p.events[0], Event::RawGrep));            // raw grep unchanged
        assert!(p.events.iter().all(|e| !matches!(e, Event::CgCall { .. }))); // stats = housekeeping, not a cg call
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib outcome::tests::parse_detects outcome::tests::parse_raw_grep`
Expected: FAIL — `detect_cli_cg_call` not defined / Bash arm still classifies these as RawGrep.

- [ ] **Step 3: Implement the helper**
```rust
/// If a Bash command invokes `code-graph-mcp <query-subcommand>`, return the
/// canonical query name (via cli::canonical_query_cmd). Scans whitespace tokens so
/// COMPOUND commands (`cd x && code-graph-mcp callgraph Y`) are detected, and
/// matches both the bare binary and a path-suffixed form
/// (`./target/release/code-graph-mcp`). Housekeeping subcommands (stats/serve/…)
/// return None (canonical_query_cmd yields None for them).
fn detect_cli_cg_call(cmd: &str) -> Option<&'static str> {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    for (i, t) in toks.iter().enumerate() {
        if *t == "code-graph-mcp" || t.ends_with("/code-graph-mcp") {
            if let Some(sub) = toks.get(i + 1) {
                if let Some(canon) = crate::cli::canonical_query_cmd(sub) {
                    return Some(canon);
                }
            }
        }
    }
    None
}
```

- [ ] **Step 4: Wire it into the Bash arm of `parse_transcript`** (pass 2). Find the existing arm:
```rust
            } else if name == "Bash" {
                let cmd = input.get("command").and_then(|x| x.as_str()).unwrap_or("");
                if cmd.contains("grep ") || cmd.contains("rg ") {
                    out.events.push(Event::RawGrep);
                }
            }
```
Replace it with (cg-CLI checked BEFORE the raw-grep fallback):
```rust
            } else if name == "Bash" {
                let cmd = input.get("command").and_then(|x| x.as_str()).unwrap_or("");
                if let Some(canon) = detect_cli_cg_call(cmd) {
                    let tool = format!("{canon}_cli");
                    let ranked = is_ranked_tool(&tool);
                    match results.get(id) {
                        None => out.unresolved += 1,
                        Some(stdout) => {
                            let returned = extract_returned_from_cli(stdout, ranked);
                            out.events.push(Event::CgCall { tool, query: String::new(), returned });
                        }
                    }
                } else if cmd.contains("grep ") || cmd.contains("rg ") {
                    out.events.push(Event::RawGrep);
                }
            }
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test --lib outcome::tests`
Expected: all pass (3 new + existing; the v1 `parse_classifies_bash_grep_as_raw_grep` with `rg some_fn src/` still passes — not a code-graph-mcp call).

- [ ] **Step 6: Commit**
```bash
git add src/outcome.rs
git commit -m "feat(outcome): detect CLI-via-Bash cg calls (compound-aware, before RawGrep)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: real-data calibration (controller validation)

**Files:** none (validation only).

- [ ] **Step 1: Rebuild + run daagu**
```bash
cargo build --release
./target/release/code-graph-mcp outcome --project /mnt/data_ssd/dev/projects/daagu
./target/release/code-graph-mcp outcome --project /mnt/data_ssd/dev/projects/daagu --json
```
Verify (the success criterion):
- `cg_calls` is now NON-ZERO and plausible vs the hand-forensic ~46 daagu CLI PULL calls (same order of magnitude — not 0, not thousands).
- `by_tool` shows `*_cli` entries (e.g. `callgraph_cli`, `grep_cli`, `search_cli`).
- `adoption_rate` ∈ (0,1); if `search_cli` calls exist, `field_mrr_*` is populated.
- Spot-check a couple of `--emit-labels` rows look sane.
- Run `cargo +1.95.0 clippy --all-targets` and fix any warning (pre-push gate).

This task has no automated assertion — its deliverable is a recorded non-zero daagu number proving the instrument now sees real consumer PULL.

---

## Self-Review

**1. Spec coverage:**
- §2 search ranked / rest structural → Task 1 (`RANKED_TOOLS += search_cli`) + Task 2 (`<canon>_cli` naming). ✓
- §3 anchor on tool_use only → Task 2 reads the model's Bash tool_use. ✓
- §4.1 detection, compound, before-RawGrep, reuse canonical_query_cmd, `<canon>_cli` → Task 2. ✓
- §4.2 JSON fast-path + `path:line` scan + dedupe → Task 1. ✓
- §4.3 ranked classification → Task 1 `search_cli` in RANKED_TOOLS. ✓
- §5 scoring/aggregation/labels unchanged → no change needed (CLI CgCalls reuse the v1 pipeline; verified by Task 2 emitting standard `Event::CgCall`). ✓
- §6 error handling (0-hit empty, unresolved, non-path empty, housekeeping ignored) → Task 1 (empty) + Task 2 (unresolved, housekeeping) tests. ✓
- §7 tests + daagu calibration → Tasks 1, 2, 3. ✓
- §8 no new dep (hand-rolled scan), no INDEX/SCHEMA bump → Task 1 dependency-free. ✓

**2. Placeholder scan:** all code steps are complete. The cg-CLI `query` is set to `String::new()` deliberately (the arg isn't needed for adoption/MRR; CLI labels carry the returned files). No TBDs.

**3. Type consistency:** `extract_returned_from_cli(&str, bool) -> Vec<ReturnedItem>`, `scan_path_line_paths(&str) -> Vec<String>`, `detect_cli_cg_call(&str) -> Option<&'static str>` — names match between Task 1 (def) and Task 2 (call). `Event::CgCall { tool, query, returned }` matches the v1 enum. `is_ranked_tool`/`RANKED_TOOLS`/`extract_returned` reused unchanged. ✓

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-29-outcome-phase2-cli.md`. Two execution options:

**1. Subagent-Driven (recommended)** — fresh subagent per task, two-stage review between tasks.

**2. Inline Execution** — execute in this session with checkpoints.

Which approach?
