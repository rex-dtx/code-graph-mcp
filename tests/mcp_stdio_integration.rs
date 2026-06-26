//! End-to-end MCP protocol tests via stdio JSON-RPC.
//!
//! These tests spawn `code-graph-mcp serve` as a subprocess, talk to it
//! through stdin/stdout, and assert on the live JSON-RPC responses.
//! Cover the fix points that unit tests can't reach:
//!   - prod-first sort ordering survives serde_json round-trip and
//!     centralized_compress truncation (R1/R2 fixes)
//!   - SQL caller_count filtering produces the same shape MCP clients see (R4/R5)
//!   - find_references explanatory error for test-only symbols (A fix)

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

fn binary_path() -> String {
    env!("CARGO_BIN_EXE_code-graph-mcp").to_string()
}

/// Build a fixture project with one target function plus enough callers
/// (mix of prod, inline test, tests/ dir, benches/) to force compression
/// truncation and stress the prod-first sort.
fn setup_fixture_project() -> TempDir {
    let project = TempDir::new().unwrap();

    let src = project.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let tests_dir = project.path().join("tests");
    std::fs::create_dir_all(&tests_dir).unwrap();
    let benches_dir = project.path().join("benches");
    std::fs::create_dir_all(&benches_dir).unwrap();

    // Target with 3 prod callers in src/cli.rs
    std::fs::write(src.join("target.rs"), "pub fn target_fn() -> i32 { 42 }\n").unwrap();
    std::fs::write(src.join("lib.rs"), "pub mod target;\npub mod cli;\npub mod inline_tests;\n").unwrap();
    std::fs::write(src.join("cli.rs"), r#"use crate::target::target_fn;
pub fn prod_caller_a() -> i32 { target_fn() }
pub fn prod_caller_b() -> i32 { target_fn() + 1 }
pub fn prod_caller_c() -> i32 { target_fn() + 2 }
"#).unwrap();

    // 25 inline tests in src/inline_tests.rs (trigger compression > 20-element cap)
    let mut inline = String::from("use crate::target::target_fn;\n");
    for i in 0..25 {
        inline.push_str(&format!(
            "#[cfg(test)]\n#[test]\nfn test_inline_{i:02}_calls_target() {{ assert_eq!(target_fn(), 42); }}\n"
        ));
    }
    std::fs::write(src.join("inline_tests.rs"), inline).unwrap();

    // 5 integration tests in tests/integration.rs
    let mut integ = String::new();
    for i in 0..5 {
        integ.push_str(&format!(
            "#[test]\nfn test_integ_{i}_calls_target() {{ assert_eq!(fixture_lib::target::target_fn(), 42); }}\n"
        ));
    }
    std::fs::write(tests_dir.join("integration.rs"), integ).unwrap();

    // 1 bench
    std::fs::write(benches_dir.join("bench_target.rs"),
        "fn bench_target() { let _ = fixture_lib::target::target_fn(); }\n").unwrap();

    // Cargo.toml so the indexer picks the right language root
    std::fs::write(project.path().join("Cargo.toml"), r#"[package]
name = "fixture_lib"
version = "0.0.0"
edition = "2021"

[lib]
path = "src/lib.rs"
"#).unwrap();

    // Index in-process (faster + deterministic than letting the spawned
    // server do it on first call).
    let db_dir = project.path().join(code_graph_mcp::domain::CODE_GRAPH_DIR);
    std::fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("index.db");
    let db = code_graph_mcp::storage::db::Database::open(&db_path).unwrap();
    code_graph_mcp::indexer::pipeline::run_full_index(&db, project.path(), None, None).unwrap();
    drop(db);

    project
}

struct McpClient {
    child: Child,
    next_id: i64,
    reader: BufReader<std::process::ChildStdout>,
    init_response: Value,
}

impl McpClient {
    fn spawn(project_root: &std::path::Path) -> Self {
        let mut child = Command::new(binary_path())
            .arg("serve")
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mcp server");
        let stdout = child.stdout.take().expect("stdout piped");
        let reader = BufReader::new(stdout);
        let mut client = Self { child, next_id: 1, reader, init_response: Value::Null };

        // Initialize handshake — required before tools/list or tools/call
        let init = client.request("initialize", json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "stdio-test", "version": "0.0.0"},
        }), Duration::from_secs(15));
        assert!(
            init.get("result").is_some(),
            "initialize failed: {:?}",
            init
        );
        client.init_response = init;
        client
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let stdin = self.child.stdin.as_mut().expect("stdin piped");
        writeln!(stdin, "{}", req).expect("write request");
        stdin.flush().expect("flush stdin");

        let start = Instant::now();
        loop {
            if start.elapsed() > timeout {
                panic!("MCP request {} timed out after {:?}", method, timeout);
            }
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).expect("read line");
            if n == 0 {
                panic!("MCP server closed stdout before response to {}", method);
            }
            let line_trim = line.trim();
            if line_trim.is_empty() {
                continue;
            }
            let resp: Value = match serde_json::from_str(line_trim) {
                Ok(v) => v,
                Err(_) => continue, // skip non-JSON lines (shouldn't happen on stdout, but be defensive)
            };
            // Filter notifications (no id) and other-id responses
            if resp.get("id").and_then(|i| i.as_i64()) == Some(id) {
                return resp;
            }
        }
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call",
            json!({"name": name, "arguments": args}),
            Duration::from_secs(30))
    }

    /// Fire-and-forget JSON-RPC notification (no id, no response expected).
    #[cfg_attr(not(feature = "embed-model"), allow(dead_code))]
    fn notify(&mut self, method: &str, params: Value) {
        let req = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let stdin = self.child.stdin.as_mut().expect("stdin piped");
        writeln!(stdin, "{}", req).expect("write notification");
        stdin.flush().expect("flush notification");
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// MCP wraps tool results as `{result: {content: [{type: "text", text: <json-string>}]}}`.
/// Pull out the inner JSON.
fn extract_tool_payload(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected result.content[0].text string in: {}", resp));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("tool text not JSON ({}): {}", e, text))
}

// =============================================================================
// Tests
// =============================================================================

/// P0.1: `run_serve` must serve a 0-tool stub in a non-project cwd (no
/// .git/manifest), mirroring the JS launcher gate (mcp-launcher.js). It must
/// NOT create `.code-graph/` in the throwaway dir. Closes the parallel path
/// the v0.33.0 launcher gate left open for direct-binary invocations.
#[test]
fn mcp_non_project_cwd_serves_zero_tool_stub() {
    let bare = TempDir::new().unwrap(); // no Cargo.toml / .git / package.json
    let mut client = McpClient::spawn(bare.path());

    // initialize response must identify the non-project stub
    let name = client.init_response["result"]["serverInfo"]["name"]
        .as_str()
        .unwrap_or("");
    assert!(
        name.contains("stub"),
        "expected non-project stub serverInfo, got: {}",
        client.init_response
    );

    // tools/list must be empty
    let resp = client.request("tools/list", json!({}), Duration::from_secs(10));
    let tools = resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools not an array: {}", resp));
    assert!(
        tools.is_empty(),
        "non-project stub must serve 0 tools, got {}",
        tools.len()
    );

    // unknown method → -32601 stub error
    let err = client.request("tools/call", json!({"name": "get_call_graph"}), Duration::from_secs(10));
    assert_eq!(
        err["error"]["code"].as_i64(),
        Some(-32601),
        "stub must reject tool calls: {}",
        err
    );

    // and no index must have been created in the throwaway dir
    assert!(
        !bare.path().join(".code-graph").exists(),
        "stub must not create .code-graph/ in a non-project cwd"
    );
}

/// Positive control: CODE_GRAPH_FORCE_PLUGIN_MCP=1 overrides the gate, so even
/// a bare dir gets the full server (non-empty tool catalog).
#[test]
fn mcp_force_plugin_mcp_overrides_non_project_gate() {
    let bare = TempDir::new().unwrap();
    let mut child = Command::new(binary_path())
        .arg("serve")
        .current_dir(bare.path())
        .env("CODE_GRAPH_FORCE_PLUGIN_MCP", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp server");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    let stdin = child.stdin.as_mut().expect("stdin piped");

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"t","version":"0"}}}}}}"#).unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{}}}}"#).unwrap();
    stdin.flush().unwrap();

    let start = Instant::now();
    let mut tools_len = None;
    while start.elapsed() < Duration::from_secs(20) {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.get("id").and_then(|i| i.as_i64()) == Some(2) {
                tools_len = v["result"]["tools"].as_array().map(|a| a.len());
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        tools_len.unwrap_or(0) > 0,
        "FORCE_PLUGIN_MCP=1 must serve the full tool catalog even in a bare dir, got {:?}",
        tools_len
    );
}

/// R1 fix: get_ast_node called_by must put prod callers first when test-heavy.
/// Without the sort, post-truncation `called_by` would be all-test (the bug).
#[test]
fn mcp_get_ast_node_called_by_prod_first_under_truncation() {
    let project = setup_fixture_project();
    let mut client = McpClient::spawn(project.path());

    let resp = client.call_tool("get_ast_node", json!({
        "symbol_name": "target_fn",
        "include_references": true,
        "include_tests": true,
        "compact": true,
    }));

    let body = extract_tool_payload(&resp);
    let called_by = body["called_by"].as_array()
        .unwrap_or_else(|| panic!("called_by is not an array: {}", body));

    assert!(
        !called_by.is_empty(),
        "called_by must have entries (target has 3 prod + many test callers)"
    );

    // Look at the first 3 entries — these should be the 3 prod callers
    // (post-sort, prod come first; tests at tail).
    let first_three_names: Vec<&str> = called_by.iter().take(3)
        .filter_map(|x| x["name"].as_str())
        .collect();
    let prod_count = first_three_names.iter()
        .filter(|n| n.starts_with("prod_caller_"))
        .count();
    assert!(
        prod_count >= 2,
        "first 3 of called_by should include >=2 prod_caller_*; got {:?} (full body: {})",
        first_three_names, body
    );
}

/// R2 fix: find_references default include_tests=true must put prod first.
#[test]
fn mcp_find_references_default_prod_first() {
    let project = setup_fixture_project();
    let mut client = McpClient::spawn(project.path());

    let resp = client.call_tool("find_references", json!({
        "symbol_name": "target_fn",
        "compact": true,
    }));

    let body = extract_tool_payload(&resp);
    let refs = body["references"].as_array()
        .unwrap_or_else(|| panic!("references not array: {}", body));
    assert!(!refs.is_empty(), "references must have entries");

    let first_three_names: Vec<&str> = refs.iter().take(3)
        .filter_map(|x| x["name"].as_str())
        .collect();
    let prod_count = first_three_names.iter()
        .filter(|n| n.starts_with("prod_caller_"))
        .count();
    assert!(
        prod_count >= 2,
        "first 3 of references should include >=2 prod_caller_*; got {:?}",
        first_three_names
    );
}

/// R4/R5 fix + A fix: caller_count is prod-only and find_references on a
/// test-only symbol returns an explanatory error (not "not found").
#[test]
fn mcp_caller_count_prod_only_and_test_symbol_error_explains() {
    let project = setup_fixture_project();
    let mut client = McpClient::spawn(project.path());

    // module_overview src — target_fn must have caller_count == 3 (prod-only),
    // not 31 (3 prod + 25 inline test + 5 tests/ + 1 bench, target reachable).
    let overview = client.call_tool("module_overview", json!({
        "path": "src",
        "compact": true,
    }));
    let body = extract_tool_payload(&overview);
    let active = body["active"].as_array().expect("active array");
    let target = active.iter()
        .find(|e| e["name"].as_str() == Some("target_fn"))
        .unwrap_or_else(|| panic!("target_fn missing from active exports: {}", body));
    let caller_count = target["caller_count"].as_i64().expect("caller_count i64");
    assert_eq!(
        caller_count, 3,
        "caller_count must be 3 prod-only (3 prod_caller_* in src/cli.rs), \
         not include test/bench sources; got {}",
        caller_count
    );

    // A fix: find_references on a test-only symbol should error with
    // "exists but all matches are in test/bench paths" rather than the old
    // misleading "not found".
    let resp = client.call_tool("find_references", json!({
        "symbol_name": "test_inline_00_calls_target",
    }));
    // Tool errors come back either as JSON-RPC error or as result.isError=true with text.
    let err_text = resp.get("error")
        .and_then(|e| e["message"].as_str())
        .or_else(|| {
            if resp["result"]["isError"].as_bool() == Some(true) {
                resp["result"]["content"][0]["text"].as_str()
            } else { None }
        })
        .unwrap_or_else(|| panic!("expected error response, got: {}", resp));

    assert!(
        err_text.contains("test/bench paths") || err_text.contains("bypass the test filter"),
        "error must explain the test filter; got: {}",
        err_text
    );
}

#[test]
fn mcp_impact_analysis_lists_test_callers() {
    // CLI/MCP parity: impact_analysis must surface the covering-test identities
    // (name + file) behind `tests_affected`, mirroring the `impact` CLI subcommand,
    // so an editor hook can build a runnable test command. target_fn is exercised by
    // 25 inline tests (src/inline_tests.rs) + integration tests + a bench; the 3
    // prod_caller_* are NOT test callers.
    let project = setup_fixture_project();
    let mut client = McpClient::spawn(project.path());
    let resp = client.call_tool("impact_analysis", json!({ "symbol_name": "target_fn" }));
    let body = extract_tool_payload(&resp);

    let tests_affected = body["tests_affected"]
        .as_u64()
        .unwrap_or_else(|| panic!("tests_affected u64 in: {}", body));
    assert!(
        tests_affected >= 1,
        "target_fn is exercised by test/bench callers; got {tests_affected}"
    );

    // Parity invariant: the surfaced identity list is the FULL set behind the
    // count (no data-layer cap), mirroring the `impact` CLI subcommand. (Magnitude
    // is intentionally not pinned — call edges from macro-wrapped calls in the
    // fixture resolve unevenly; the invariant below is what guarantees the feature.)
    let test_callers = body["test_callers"]
        .as_array()
        .unwrap_or_else(|| panic!("test_callers must be a JSON array (CLI/MCP parity); body: {}", body));
    assert_eq!(
        test_callers.len() as u64,
        tests_affected,
        "the surfaced identity list must match tests_affected exactly (full list, no cap)"
    );
    for tc in test_callers {
        let name = tc["name"].as_str().expect("test caller carries a name");
        let file = tc["file"].as_str().expect("test caller carries a file");
        assert!(!file.is_empty(), "file identity needed to build the test command");
        assert!(
            name.starts_with("test_") || name.starts_with("bench_"),
            "covering tests are the fixture's test_*/bench_* callers; got {name}"
        );
        assert!(
            !name.starts_with("prod_caller_"),
            "a prod caller must not appear among covering tests: {name}"
        );
    }
}

/// Regression: enum-valued direction/deps_direction args must be validated at the
/// tool entry. Previously, `get_call_graph` echoed a bogus direction back through
/// the ambiguity-resolution path (two errors for one mistake), `dependency_graph`
/// only rejected after index-freshness checks ran, and `module_overview` silently
/// swallowed bogus `deps_direction` into a `dependencies_unavailable` field.
#[test]
fn mcp_enum_args_validated_at_tool_entry() {
    let project = setup_fixture_project();
    let mut client = McpClient::spawn(project.path());

    let tool_err = |resp: &Value| -> String {
        if resp["result"]["isError"].as_bool() == Some(true) {
            resp["result"]["content"][0]["text"].as_str().unwrap_or("").to_string()
        } else {
            panic!("expected isError=true, got: {}", resp);
        }
    };

    // get_call_graph direction enum
    let r = client.call_tool("get_call_graph", json!({
        "symbol_name": "target_fn", "direction": "sideways",
    }));
    assert!(tool_err(&r).contains("direction must be one of: callers, callees, both"),
        "get_call_graph should reject bad direction at entry; got: {}", tool_err(&r));

    // dependency_graph direction enum
    let r = client.call_tool("dependency_graph", json!({
        "file_path": "src/lib.rs", "direction": "upside_down",
    }));
    assert!(tool_err(&r).contains("direction must be one of: outgoing, incoming, both"),
        "dependency_graph should reject bad direction at entry; got: {}", tool_err(&r));

    // module_overview deps_direction enum (this was silently swallowed before)
    let r = client.call_tool("module_overview", json!({
        "path": "src/lib.rs", "include_deps": true, "deps_direction": "upside_down",
    }));
    assert!(tool_err(&r).contains("deps_direction must be one of"),
        "module_overview should reject bad deps_direction at entry; got: {}", tool_err(&r));

    // module_overview deps_direction must be validated UNCONDITIONALLY — even
    // without include_deps and for a directory path. Before the fix the check was
    // gated inside `if include_deps { if path-is-file {...} }`, so this path never
    // validated and returned a normal OK overview, hiding the typo.
    let r = client.call_tool("module_overview", json!({
        "path": "src", "deps_direction": "upside_down",
    }));
    assert!(tool_err(&r).contains("deps_direction must be one of"),
        "module_overview must reject bad deps_direction even without include_deps; got: {}", tool_err(&r));

    // impact_analysis change_type enum (validated at entry, before index work)
    let r = client.call_tool("impact_analysis", json!({
        "symbol_name": "target_fn", "change_type": "sideways",
    }));
    assert!(tool_err(&r).contains("change_type must be one of"),
        "impact_analysis should reject bad change_type at entry; got: {}", tool_err(&r));

    // find_references relation enum typo
    let r = client.call_tool("find_references", json!({
        "symbol_name": "target_fn", "relation": "call",
    }));
    assert!(tool_err(&r).contains("Unknown relation filter"),
        "find_references should reject bad relation at entry; got: {}", tool_err(&r));
}

/// Regression: `relation` must be validated BEFORE symbol resolution, so a bogus
/// relation on a nonexistent symbol reports the relation error — not the
/// "symbol not found" error that would otherwise mask the real typo.
#[test]
fn mcp_find_references_invalid_relation_precedes_resolution() {
    let project = setup_fixture_project();
    let mut client = McpClient::spawn(project.path());
    let r = client.call_tool("find_references", json!({
        "symbol_name": "definitely_absent_symbol_xyz", "relation": "bogus",
    }));
    let text = if r["result"]["isError"].as_bool() == Some(true) {
        r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string()
    } else {
        panic!("expected isError=true, got: {}", r);
    };
    assert!(text.contains("Unknown relation filter"),
        "relation must be validated before symbol resolution; got: '{}'", text);
}

/// Regression (#4): find_dead_code must reject an unknown node_type loudly rather
/// than returning a false-clean empty result (a literal `n.type = :x` → 0 rows).
#[test]
fn mcp_find_dead_code_rejects_unknown_node_type() {
    let project = setup_fixture_project();
    let mut client = McpClient::spawn(project.path());
    let r = client.call_tool("find_dead_code", json!({ "node_type": "fucntion" }));
    let text = if r["result"]["isError"].as_bool() == Some(true) {
        r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string()
    } else {
        panic!("expected isError=true, got: {}", r);
    };
    assert!(text.contains("Unknown type filter"),
        "find_dead_code must reject unknown node_type; got: '{}'", text);
}

/// Regression: an "edit-only" session that issues NO code-graph tool call must
/// still get its index embedded. The embedding backfill used to be kicked off
/// only by `consume_startup_index_result()`, which runs on an incoming MCP
/// message (i.e. a tool call). With no tool call the finished startup index's
/// vectors were stranded — the daagu "2% vec, never moves" symptom. The fix
/// drives the backfill from the startup-index thread itself, so the handshake
/// alone is enough.
#[cfg(feature = "embed-model")]
#[test]
fn mcp_startup_embeds_without_any_tool_call() {
    use code_graph_mcp::storage::db::Database;
    use code_graph_mcp::storage::queries::count_nodes_with_vectors;

    // Coverage note: this is a LOCAL gate. CI (ci.yml) runs only --no-default-features
    // and default (both no-embed), and release.yml builds embed-model but does not
    // `cargo test --features embed-model`, so this executes only on a local
    // `cargo test --features embed-model` with the model present. It needs real weights
    // to observe embedding; skip loudly when absent rather than false-fail.
    if code_graph_mcp::embedding::model::EmbeddingModel::load().ok().flatten().is_none() {
        eprintln!("[skip] embedding model weights unavailable; cannot observe backfill");
        return;
    }

    let project = setup_fixture_project();
    let db_path = project.path()
        .join(code_graph_mcp::domain::CODE_GRAPH_DIR)
        .join("index.db");

    // Precondition: the in-process index (built with model=None) has embeddable
    // nodes but zero vectors. Open with vec so node_vectors exists for polling.
    {
        let db = Database::open_with_vec(&db_path).unwrap();
        let (with_vectors, total) = count_nodes_with_vectors(db.conn()).unwrap();
        assert!(total > 0, "fixture must have embeddable nodes (got total={total})");
        assert_eq!(with_vectors, 0, "fixture must start with 0 vectors (got {with_vectors})");
    }

    // Drive ONLY the lifecycle handshake: initialize (in spawn) + the initialized
    // notification. Never send a tools/call.
    let mut client = McpClient::spawn(project.path());
    client.notify("notifications/initialized", json!({}));

    // The backfill runs asynchronously in the startup-index thread. Poll the
    // vector count until it climbs above zero.
    let mut embedded = 0i64;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(500));
        if let Ok(db) = Database::open_with_vec(&db_path) {
            if let Ok((with_vectors, _)) = count_nodes_with_vectors(db.conn()) {
                embedded = with_vectors;
                if embedded > 0 {
                    break;
                }
            }
        }
    }

    assert!(
        embedded > 0,
        "startup index must embed nodes with NO tool call; got {embedded} vectors after 60s"
    );
}
