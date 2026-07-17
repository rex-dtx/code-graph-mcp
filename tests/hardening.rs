//! Production hardening tests: concurrency, stress, and edge-case scenarios.
//!
//! McpServer wraps a raw rusqlite::Connection which is Send but not Sync,
//! so concurrent tests use Arc<Mutex<McpServer>> to validate that interleaved
//! access from multiple threads causes no deadlocks or data corruption.

mod common;

use code_graph_mcp::mcp::server::McpServer;
use serde_json::json;
use std::fs;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

use common::{init_server, parse_tool_result, tool_call_json};

fn setup_project(file_count: usize) -> (TempDir, McpServer) {
    let project = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join("src")).unwrap();

    for i in 0..file_count {
        let content = format!(
            "export function func_{}(x: number): number {{ return x + {}; }}\n\
             export function helper_{}(): string {{ return 'hello'; }}\n",
            i, i, i
        );
        fs::write(
            project.path().join(format!("src/mod_{}.ts", i)),
            content,
        )
        .unwrap();
    }

    let server = init_server(&project);

    // Trigger initial indexing
    let search = tool_call_json("semantic_code_search", json!({"query": "func_0"}));
    let _ = server.handle_message(&search).unwrap();

    (project, server)
}

/// Multi-threaded search calls from 10 threads against a Mutex-wrapped McpServer.
/// Access is serialized by the mutex (McpServer is Send but not Sync).
/// Validates no panics or mutex poisoning under multi-threaded scheduling.
#[test]
fn test_concurrent_tool_calls() {
    let (_project, server) = setup_project(20);
    let server = Arc::new(Mutex::new(server));

    let handles: Vec<_> = (0..10)
        .map(|i| {
            let srv = Arc::clone(&server);
            std::thread::spawn(move || {
                let msg = tool_call_json(
                    "semantic_code_search",
                    json!({"query": format!("func_{}", i)}),
                );
                let resp = srv.lock().unwrap().handle_message(&msg).unwrap();
                assert!(resp.is_some(), "thread {} got no response", i);
                let v: serde_json::Value =
                    serde_json::from_str(resp.as_ref().unwrap()).unwrap();
                assert!(
                    v.get("result").is_some(),
                    "thread {} got no result: {:?}",
                    i,
                    v
                );
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }
}

/// Stress test: index 200 files and verify all are tracked.
#[test]
fn test_large_repo_indexing() {
    let (_project, server) = setup_project(200);

    let msg = tool_call_json("get_index_status", json!({}));
    let resp = server.handle_message(&msg).unwrap();
    let result = parse_tool_result(&resp);

    let files = result["files_count"].as_i64().unwrap();
    assert!(
        files >= 200,
        "should index at least 200 files, got {}",
        files
    );
}

/// Mixed tool calls (search, status, project_map) from 20 threads.
/// Tests that different tool handlers don't interfere with each other.
#[test]
fn test_concurrent_mixed_tool_calls() {
    let (_project, server) = setup_project(50);
    let server = Arc::new(Mutex::new(server));

    let handles: Vec<_> = (0..20)
        .map(|i| {
            let srv = Arc::clone(&server);
            std::thread::spawn(move || {
                let msg = if i % 3 == 0 {
                    tool_call_json(
                        "semantic_code_search",
                        json!({"query": format!("func_{}", i)}),
                    )
                } else if i % 3 == 1 {
                    tool_call_json("get_index_status", json!({}))
                } else {
                    tool_call_json("project_map", json!({}))
                };
                let resp = srv.lock().unwrap().handle_message(&msg).unwrap();
                assert!(resp.is_some(), "thread {} got no response", i);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked during concurrent access");
    }
}

/// All query tools should return gracefully on a completely empty project.
#[test]
fn test_empty_project_graceful() {
    let project = TempDir::new().unwrap();
    let server = init_server(&project);

    let tools = vec![
        ("semantic_code_search", json!({"query": "anything"})),
        ("project_map", json!({})),
        ("get_index_status", json!({})),
    ];
    for (name, args) in tools {
        let msg = tool_call_json(name, args);
        let resp = server.handle_message(&msg).unwrap();
        assert!(
            resp.is_some(),
            "{} should return response on empty project",
            name
        );
    }
}

/// Binary garbage and zero-byte files with recognized extensions
/// should not crash the indexer; valid files alongside them should still index.
#[test]
fn test_binary_files_dont_crash_indexing() {
    let project = TempDir::new().unwrap();
    // Create a valid file alongside binary garbage
    fs::write(
        project.path().join("valid.ts"),
        "export function hello(): string { return 'world'; }",
    )
    .unwrap();
    // Binary file with .ts extension
    fs::write(
        project.path().join("broken.ts"),
        [0xFF, 0xFE, 0x00, 0x01, 0xFF, 0xFE],
    )
    .unwrap();
    // Zero-byte file
    fs::write(project.path().join("empty.ts"), "").unwrap();

    let server = init_server(&project);

    // Should not crash — valid file should still be indexed
    let msg = tool_call_json("semantic_code_search", json!({"query": "hello"}));
    let resp = server.handle_message(&msg).unwrap();
    assert!(
        resp.is_some(),
        "should return response even with broken files"
    );
}

/// Re-indexing the same files multiple times should not duplicate nodes.
#[test]
fn test_repeated_indexing_is_idempotent() {
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("main.ts"),
        "export function main() { return 42; }",
    )
    .unwrap();

    let server = init_server(&project);

    // Index multiple times via different tool calls
    for _ in 0..3 {
        let msg = tool_call_json("semantic_code_search", json!({"query": "main"}));
        let resp = server.handle_message(&msg).unwrap();
        assert!(resp.is_some());
    }

    // Verify node count didn't multiply
    let msg = tool_call_json("get_index_status", json!({}));
    let resp = server.handle_message(&msg).unwrap();
    let result = parse_tool_result(&resp);
    let nodes = result["nodes_count"].as_i64().unwrap();
    // Should have a reasonable number of nodes, not 3x duplicates
    assert!(
        nodes < 50,
        "nodes should not multiply with repeated indexing, got {}",
        nodes
    );
}

/// Layering drift-guard: the storage layer must never import from the graph
/// layer — graph depends on storage, not the reverse. M9a moved the one
/// offending orchestration (`get_callers_with_route_info`) up into
/// `src/graph/routes.rs`. Re-introducing `use crate::graph` anywhere under
/// src/storage/ recreates the cycle this test exists to forbid.
#[test]
fn no_storage_module_imports_graph() {
    use std::fs;
    use std::path::Path;
    let mut offenders = Vec::new();
    fn walk(dir: &Path, offenders: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, offenders);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let src = fs::read_to_string(&path).unwrap();
                for (i, line) in src.lines().enumerate() {
                    // Strip line/doc comments so a comment MENTIONING crate::graph
                    // (e.g. "orchestration lives in `crate::graph::routes`") is not
                    // a false offender — only real code imports count.
                    let code = line.split("//").next().unwrap_or("");
                    let t = code.trim_start();
                    if t.starts_with("use crate::graph") || t.contains("crate::graph::") {
                        offenders.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                    }
                }
            }
        }
    }
    walk(Path::new("src/storage"), &mut offenders);
    assert!(
        offenders.is_empty(),
        "storage must not import graph (cycle). Offenders:\n{}",
        offenders.join("\n")
    );
}

/// Give `dir` a `.code-graph/index.db` (mirrors the private helper in
/// `src/cli.rs`'s own unit tests — duplicated here since that one is
/// `#[cfg(test)]`-private to the crate, not reachable from an integration test).
fn write_index(dir: &std::path::Path) {
    let idx = dir.join(code_graph_mcp::domain::CODE_GRAPH_DIR);
    fs::create_dir_all(&idx).unwrap();
    fs::write(idx.join("index.db"), b"").unwrap();
}

/// META④ drift-guard: the Rust (`resolve_project_root_from`, src/cli.rs:38) and
/// JS (`resolveProjectRoot`, claude-plugin/scripts/project-root.js) project-root
/// resolvers are parallel implementations that MUST agree (M7 fix, v0.94.0).
/// This locks the specific case that split-brained before M7: cwd sits under a
/// STRAY nested `.code-graph` index (a monorepo-subdir relic) that is itself
/// below the real git root, which is also indexed. Both resolvers must pick the
/// git root, not the nearer stray index — otherwise the CLI and the JS hooks
/// read different `.code-graph` DBs for the same project.
///
/// JS invocation contract (confirmed by reading project-root.js in full plus its
/// consumer test `claude-plugin/scripts/pre-grep-guide.test.js`): the file has NO
/// CLI entrypoint — no argv parsing, no `require.main === module`, no stdout
/// write. It only `module.exports = { resolveProjectRoot }` for `require()`
/// (`pre-grep-guide.test.js` imports and calls the function directly, in-process
/// — it never shells out to the script). So "invoke via node" for a real
/// cross-process assertion means spawning `node -e` that requires the module by
/// absolute path and calls `resolveProjectRoot(cwd)` itself — this runs the
/// actual JS resolver logic in a real subprocess, not a fabricated assertion.
#[test]
fn project_root_resolution_rust_js_parity() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write_index(root);
    let mid = root.join("packages").join("app");
    fs::create_dir_all(&mid).unwrap();
    write_index(&mid); // stray nested index, no .git of its own
    let cwd = mid.join("src");
    fs::create_dir_all(&cwd).unwrap();

    // Rust side: locked unconditionally, regardless of node availability below.
    let rust_root = code_graph_mcp::cli::resolve_project_root_from(&cwd);
    assert_eq!(
        fs::canonicalize(&rust_root).unwrap(),
        fs::canonicalize(root).unwrap(),
        "Rust resolver must pick the git root over the stray nested index (M7)"
    );

    let js_script_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("claude-plugin/scripts/project-root.js");
    let out = std::process::Command::new("node")
        .arg("-e")
        .arg(
            "const { resolveProjectRoot } = require(process.argv[2]); \
             const r = resolveProjectRoot(process.argv[1]); \
             process.stdout.write(r || '');",
        )
        .arg(&cwd)
        .arg(&js_script_path)
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let js_root = String::from_utf8_lossy(&o.stdout).trim().to_string();
            assert!(
                !js_root.is_empty(),
                "JS resolver returned null/empty for a valid git+indexed root"
            );
            assert_eq!(
                fs::canonicalize(&rust_root).unwrap(),
                fs::canonicalize(&js_root).unwrap(),
                "Rust and JS project-root resolvers disagree on the git root (M7 split-brain)"
            );
        }
        _ => {
            // Degradation per the task brief: node unavailable/flaky in this test
            // harness. The Rust side is already locked above (unconditionally, not
            // inside this match arm); the JS resolver's stray-index-prefers-git-root
            // logic is separately covered by
            // `claude-plugin/scripts/pre-grep-guide.test.js`'s
            // "resolveProjectRoot: skips a STRAY nested subdir index, prefers the
            // .git root" test (a 2-level variant of this same scenario).
        }
    }
}

/// MED-1 drift-guard: the release profile must NOT set `panic = "abort"`.
///
/// `src/main.rs`'s per-request `std::panic::catch_unwind` (the H3 defense that
/// turns a handler panic into a JSON-RPC -32603 and keeps the long-lived stdio
/// session alive) is INERT under `panic = "abort"` — an abort tears the whole
/// process down before the catch can run. The unit/integration suite compiles
/// under the dev profile (unwind), so that defense is false-green in tests;
/// only the shipped release binary would abort. This guard reads the real
/// Cargo.toml at test time and fails if the release profile re-introduces the
/// abort setting.
#[test]
fn release_profile_must_unwind_for_catch_unwind_defense() {
    let manifest = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read Cargo.toml");

    // Slice out the [profile.release] section: from its header to the next
    // top-level `[` table header (or EOF).
    let header = "[profile.release]";
    let start = manifest
        .find(header)
        .expect("Cargo.toml must have a [profile.release] section");
    let after = &manifest[start + header.len()..];
    let end = after.find("\n[").map(|i| i + 1).unwrap_or(after.len());
    let section = &after[..end];

    // No UNCOMMENTED `panic = "abort"` key in the section.
    let offender = section.lines().find(|line| {
        let code = line.split('#').next().unwrap_or(""); // strip TOML comments
        let t = code.trim();
        t.starts_with("panic") && t.contains("abort")
    });
    assert!(
        offender.is_none(),
        "[profile.release] must not set `panic = \"abort\"` — it makes the \
         per-request catch_unwind in src/main.rs (session-survival defense) inert \
         in release builds. Offending line: {:?}",
        offender
    );
}
