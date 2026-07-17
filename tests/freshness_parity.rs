//! Cross-surface drift-guards for two recurring bug classes.
//!
//! 1. **Compact ↔ full key-set parity** (`compact_field_allowlist`, 3rd recurrence):
//!    `tool_module_overview` builds a full JSON envelope, then `compact_module_overview`
//!    forwards a hand-maintained allowlist of top-level keys. Every prior recurrence
//!    was a new top-level key added to the full envelope that nobody added to the
//!    allowlist, so it silently vanished in `compact: true` mode with no disclosure.
//!    `compact_allowlist_covers_all_result_keys` scans the source and fails if any
//!    `result["k"] =` key in the producer is neither forwarded nor explicitly listed
//!    as deliberately compacted.
//!
//! 2. **CLI ↔ MCP query-time freshness parity** (AUDIT-2026-07-16 MED-2 follow-up):
//!    every line-number-emitting CLI subcommand must resync stale files via
//!    `refresh_files_if_stale` before reading line numbers out of the DB, and every
//!    file-path-accepting MCP tool must do the same via `ensure_file_fresh_opt`.
//!    A new command that emits line numbers without wiring in the resync ships a
//!    stale-line-number regression. The source-scanning guards below lock the known
//!    call sites so a missing one fails CI instead of shipping.
//!
//! These are source-scanning tests: they read the crate's own `.rs` files as text.
//! Cargo runs integration tests with the crate root as CWD, so the relative paths
//! below resolve. A text edit that removes a guarded call fails the guard even
//! without recompiling — that is the point.

mod common;

use common::{init_server, parse_tool_result, tool_call_json};
use serde_json::json;
use std::fs;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Task 1 runtime proof: compact mode forwards the `dependencies` payload.
// ---------------------------------------------------------------------------

/// RED before the allowlist fix: `module_overview` with `include_deps: true` sets a
/// top-level `dependencies` key on the full envelope, but the compact forwarder's
/// allowlist did not include it, so `compact: true` dropped it with no disclosure.
/// GREEN after adding the key to the allowlist in `compact_module_overview`.
#[test]
fn compact_module_overview_forwards_dependencies() {
    let project = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("src/mod_b.ts"),
        "export function bee(): number { return 1; }\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/mod_a.ts"),
        "import { bee } from './mod_b';\n\
         export function ay(): number { return bee(); }\n",
    )
    .unwrap();

    let server = init_server(&project);

    let msg = tool_call_json(
        "module_overview",
        json!({
            "path": "src/mod_a.ts",
            "include_deps": true,
            "compact": true,
        }),
    );
    let resp = server.handle_message(&msg).unwrap();
    let result = parse_tool_result(&resp);

    assert!(
        result.get("dependencies").is_some(),
        "compact + include_deps must forward `dependencies`; compact result was:\n{}",
        serde_json::to_string_pretty(&result).unwrap()
    );
}

// ---------------------------------------------------------------------------
// Task 2 drift-guard: every full-envelope top-level key is compact-forwarded.
// ---------------------------------------------------------------------------

const OVERVIEW_SRC: &str = "src/mcp/server/tools/overview.rs";

/// Keys the compact form intentionally rewrites/renames/drops rather than
/// forwarding verbatim through the `for key in [...]` allowlist. `warning` is
/// forwarded through its own dedicated `if full.get("warning")` branch (a value,
/// not a copied key), so it is covered but not in the array. If a future author
/// adds a new full-envelope key that compact handles specially, list it here with
/// a comment — do NOT add keys here just to silence the guard.
const DELIBERATELY_COMPACTED: &[&str] = &[
    // Forwarded via a dedicated `if full.get("warning").is_some()` branch.
    "warning",
];

/// Extract every `result["<key>"] =` assignment key found in `region`.
fn assigned_result_keys(region: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let marker = "result[\"";
    let mut rest = region;
    while let Some(i) = rest.find(marker) {
        let after = &rest[i + marker.len()..];
        if let Some(j) = after.find("\"]") {
            let key = &after[..j];
            let tail = after[j + 2..].trim_start();
            // Only assignments (`=`), not comparisons (`==`) or index reads.
            if tail.starts_with('=') && !tail.starts_with("==") {
                keys.push(key.to_string());
            }
            rest = &after[j + 2..];
        } else {
            break;
        }
    }
    keys
}

/// Extract the quoted keys from the `for key in [ ... ]` compact allowlist array.
fn compact_allowlist(compact_region: &str) -> Vec<String> {
    let anchor = "for key in [";
    let start = compact_region
        .find(anchor)
        .expect("compact allowlist `for key in [` not found — did the forwarder shape change?");
    let after = &compact_region[start + anchor.len()..];
    let end = after.find(']').expect("unterminated compact allowlist array");
    after[..end]
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Return the body of `fn <name>` (braces included) by brace-matching from the
/// opening `{` of the signature to its balanced close. Robust to indentation
/// (methods inside `impl` blocks) and to nested inner `fn` helpers — both of
/// which defeat line-based boundary detection. Skips `//` line comments and
/// `"…"` string literals so braces inside `format!("{}")` / error strings and
/// `// { }` comments do not unbalance the count (the target fns contain no raw
/// strings, block-comment braces, or char-literal braces — verified at authoring).
fn fn_region<'a>(src: &'a str, name: &str) -> &'a str {
    let decl = format!("fn {}(", name);
    let start = src
        .find(&decl)
        .unwrap_or_else(|| panic!("function `{name}` not found in source"));
    let bytes = src.as_bytes();
    // First `{` after the declaration opens the body (signatures have no braces).
    let mut i = start + decl.len();
    while i < bytes.len() && bytes[i] != b'{' {
        i += 1;
    }
    let body_start = i;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut in_line_comment = false;
    let mut prev = 0u8;
    while i < bytes.len() {
        let c = bytes[i];
        if in_line_comment {
            if c == b'\n' {
                in_line_comment = false;
            }
        } else if in_str {
            if c == b'"' && prev != b'\\' {
                in_str = false;
            }
        } else if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            in_line_comment = true;
        } else if c == b'"' {
            in_str = true;
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                return &src[body_start..=i];
            }
        }
        prev = c;
        i += 1;
    }
    &src[body_start..]
}

/// Returns full-envelope keys that compact neither forwards nor deliberately drops.
fn uncovered_compact_keys(overview_src: &str) -> Vec<String> {
    let producer = fn_region(overview_src, "tool_module_overview");
    let forwarder = fn_region(overview_src, "compact_module_overview");
    let allowlist = compact_allowlist(forwarder);
    let mut uncovered: Vec<String> = assigned_result_keys(producer)
        .into_iter()
        .filter(|k| !allowlist.contains(k) && !DELIBERATELY_COMPACTED.contains(&k.as_str()))
        .collect();
    uncovered.sort();
    uncovered.dedup();
    uncovered
}

#[test]
fn compact_allowlist_covers_all_result_keys() {
    let src = fs::read_to_string(OVERVIEW_SRC).expect("read overview.rs");
    let uncovered = uncovered_compact_keys(&src);
    assert!(
        uncovered.is_empty(),
        "tool_module_overview sets top-level key(s) {uncovered:?} that compact_module_overview \
         neither forwards (allowlist / dedicated branch) nor lists in DELIBERATELY_COMPACTED. \
         Add each to the `for key in [...]` allowlist in {OVERVIEW_SRC}, or document it in \
         DELIBERATELY_COMPACTED (tests/freshness_parity.rs)."
    );
}

/// Permanent negative control: prove the guard actually fires when a key is
/// missing from the allowlist. Removing `"dead_code"` from the array in a working
/// copy of the source must surface `dead_code` as uncovered.
#[test]
fn compact_allowlist_guard_detects_missing_key() {
    let src = fs::read_to_string(OVERVIEW_SRC).expect("read overview.rs");
    // Drop the `"dead_code",` allowlist entry (the trailing comma pins it to the
    // allowlist array — the producer's `result["dead_code"] =` has no trailing
    // comma, and `"dead_code_unavailable",` is a different token).
    let broken = src.replace("\"dead_code\",", "");
    let uncovered = uncovered_compact_keys(&broken);
    assert!(
        uncovered.iter().any(|k| k == "dead_code"),
        "negative control failed: removing \"dead_code\" from the allowlist should make the \
         guard report it as uncovered, but got {uncovered:?}"
    );
}

// ---------------------------------------------------------------------------
// Task 3 drift-guard: CLI + MCP query-time freshness resync coverage.
// ---------------------------------------------------------------------------

const CLI_SRC: &str = "src/cli.rs";

/// Line-number-emitting CLI subcommand handlers. Each MUST call
/// `refresh_files_if_stale` before reading line numbers out of the DB, or an
/// edited-but-not-yet-reindexed file yields stale line numbers.
///
/// ADD NEW LINE-NUMBER-EMITTING COMMANDS HERE. If you add a CLI subcommand that
/// prints file:line locations, add its handler fn name below and wire in the
/// `refresh_files_if_stale` resync — otherwise this list drifts silently.
const CLI_FRESHNESS_HANDLERS: &[&str] = &[
    "cmd_search",
    "cmd_ast_search",
    "cmd_impact",
    "cmd_overview",
    "cmd_show",
    "cmd_trace",
    "cmd_similar",
    "cmd_refs",
    "cmd_dead_code",
];

/// File-path-accepting MCP tools. Each MUST call `ensure_file_fresh_opt` (the MCP
/// shared resync path) so an edited file is reindexed before its line numbers are
/// read. ADD NEW FILE-PATH-ACCEPTING MCP TOOLS HERE.
const MCP_FRESHNESS_TOOLS: &[(&str, &str)] = &[
    ("src/mcp/server/tools/advanced.rs", "tool_dependency_graph"),
    ("src/mcp/server/tools/ast_node.rs", "tool_get_ast_node"),
    ("src/mcp/server/tools/callgraph.rs", "tool_get_call_graph"),
    ("src/mcp/server/tools/overview.rs", "tool_module_overview"),
    ("src/mcp/server/tools/refs.rs", "tool_find_references"),
];

/// A CLI handler is "missing" the resync if its fn body has no `refresh_files_if_stale(`
/// CALL (the trailing `(` excludes bare mentions in comments like "via refresh_files_if_stale)").
fn cli_handlers_missing_refresh(cli_src: &str) -> Vec<String> {
    CLI_FRESHNESS_HANDLERS
        .iter()
        .filter(|name| !fn_region(cli_src, name).contains("refresh_files_if_stale("))
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn cli_line_number_commands_call_refresh() {
    let src = fs::read_to_string(CLI_SRC).expect("read cli.rs");
    let missing = cli_handlers_missing_refresh(&src);
    assert!(
        missing.is_empty(),
        "CLI handler(s) {missing:?} emit line numbers but do not call refresh_files_if_stale — \
         edited-but-unindexed files will yield stale line numbers. Add the resync (see the other \
         handlers in {CLI_SRC}), or if a handler no longer emits line numbers remove it from \
         CLI_FRESHNESS_HANDLERS (tests/freshness_parity.rs)."
    );
}

#[test]
fn mcp_file_path_tools_call_ensure_fresh() {
    let mut missing = Vec::new();
    for (file, tool) in MCP_FRESHNESS_TOOLS {
        let src = fs::read_to_string(file).unwrap_or_else(|e| panic!("read {file}: {e}"));
        if !fn_region(&src, tool).contains("ensure_file_fresh_opt(") {
            missing.push(format!("{tool} ({file})"));
        }
    }
    assert!(
        missing.is_empty(),
        "MCP file-path tool(s) {missing:?} do not call ensure_file_fresh_opt — edited files are \
         served with stale line numbers. Add the resync, or remove the tool from \
         MCP_FRESHNESS_TOOLS (tests/freshness_parity.rs) if it no longer accepts file paths."
    );
}

/// Permanent negative control: neutralizing the `refresh_files_if_stale(&ctx.db, ...)`
/// call sites in a working copy of the CLI source must make the guard report the
/// affected handlers as missing. Proves the guard fires on a real omission without
/// mutating the shared, concurrently-edited src/cli.rs on disk.
#[test]
fn cli_freshness_guard_detects_missing_refresh() {
    let src = fs::read_to_string(CLI_SRC).expect("read cli.rs");
    let broken = src.replace(
        "refresh_files_if_stale(&ctx.db, project_root, &files);",
        "/* neutralized for negative control */;",
    );
    let missing = cli_handlers_missing_refresh(&broken);
    assert!(
        missing.iter().any(|h| h == "cmd_refs"),
        "negative control failed: neutralizing the refresh call should flag cmd_refs as missing, \
         but got {missing:?}"
    );
}
