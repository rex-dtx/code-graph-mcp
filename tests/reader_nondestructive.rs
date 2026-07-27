//! Read commands must never destroy the index.
//!
//! `INDEX_VERSION` revalidation (wipe + rebuild) is an *indexer* responsibility.
//! When a passive consumer performs it, the wipe happens and nothing rebuilds —
//! the index stays at 0 nodes until the user notices. `health-check` and `grep`
//! caused exactly that once (the daagu incident) and were moved onto
//! `Database::open_nondestructive`; `similar` was left behind on the indexer
//! constructor because it also needed sqlite-vec, and vector support and
//! destructive revalidation were entangled in one constructor.
//!
//! This test drives the real binary, so it covers the CLI wiring (which
//! constructor `cmd_similar` reaches for), not just the storage layer.

use std::process::Command;
use tempfile::TempDir;

use code_graph_mcp::domain::CODE_GRAPH_DIR;
use code_graph_mcp::storage::db::Database;

fn cli_bin() -> String {
    env!("CARGO_BIN_EXE_code-graph-mcp").to_string()
}

fn node_count(db_path: &std::path::Path) -> i64 {
    let db = Database::open_nondestructive(db_path).unwrap();
    db.conn()
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap()
}

/// A project directory the indexer accepts: a `.git` anchor (the activation
/// gate refuses to index anything else) plus one source file.
fn fixture_project() -> TempDir {
    let project = TempDir::new().unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(project.path())
        .status()
        .unwrap();
    let src = project.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn alpha() { beta(); }\npub fn beta() {}\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    project
}

#[test]
fn similar_does_not_wipe_a_version_lagging_index() {
    let project = fixture_project();
    let db_path = project.path().join(CODE_GRAPH_DIR).join("index.db");

    let status = Command::new(cli_bin())
        .args(["incremental-index", "--quiet", "--no-embed"])
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(status.success(), "fixture index build failed");

    let before = node_count(&db_path);
    assert!(before > 0, "fixture must index some nodes (got {before})");

    // Simulate "binary upgraded past the on-disk index generation, no rebuild
    // has run yet" — the window every INDEX_VERSION bump opens for every user.
    {
        let db = Database::open_nondestructive(&db_path).unwrap();
        db.conn()
            .pragma_update(
                None,
                "application_id",
                code_graph_mcp::domain::INDEX_VERSION - 1,
            )
            .unwrap();
    }

    // `similar` may legitimately fail here (no embeddings in a --no-embed index).
    // What it must not do is take the index down with it.
    let out = Command::new(cli_bin())
        .args(["similar", "alpha"])
        .current_dir(project.path())
        .output()
        .unwrap();

    let after = node_count(&db_path);
    assert_eq!(
        after,
        before,
        "read-only `similar` wiped the index ({before} → {after} nodes); it must \
         open non-destructively like every other read command. stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // And the staleness must still be *owed*: a reader that silently re-stamped
    // application_id would mask the pending rebuild from the next indexer open.
    let db = Database::open_nondestructive(&db_path).unwrap();
    let stamped: i32 = db
        .conn()
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .unwrap();
    assert_eq!(
        stamped,
        code_graph_mcp::domain::INDEX_VERSION - 1,
        "reader must leave the stale generation stamped so the rebuild is still owed"
    );
}

/// The test above pins the `similar` INSTANCE. This one pins the CLASS.
///
/// Contract audit 2026-07-27 measured all 25 read subcommands against a
/// version-lagging index: none wipe today. But the guard for that fact was a
/// single hardcoded `.args(["similar", "alpha"])`, so read command #26 could
/// reach for the indexer constructor and every test stays green — which is
/// precisely how `similar` itself survived four audits. Sibling-hole class,
/// first-ranked finding five audits running.
#[test]
fn no_read_subcommand_wipes_a_version_lagging_index() {
    // Every subcommand that answers a question about an existing index. Failure
    // modes vary (some exit non-zero without embeddings, some print "not found")
    // and that is fine — the assertion is only that the index survives.
    const READ_COMMANDS: &[&[&str]] = &[
        &["grep", "alpha"],
        &["search", "alpha"],
        &["ast-search", "alpha"],
        &["callgraph", "alpha"],
        &["impact", "alpha"],
        &["show", "alpha"],
        &["refs", "alpha"],
        &["similar", "alpha"],
        &["map"],
        &["overview", "src"],
        &["deps"],
        &["tour"],
        &["trace", "GET /x"],
        &["dead-code"],
        &["centrality"],
        &["cycles"],
        &["surprising"],
        &["report"],
        &["stats"],
        &["health-check"],
        &["affected"],
    ];

    let project = fixture_project();
    let db_path = project.path().join(CODE_GRAPH_DIR).join("index.db");
    assert!(
        Command::new(cli_bin())
            .args(["incremental-index", "--quiet", "--no-embed"])
            .current_dir(project.path())
            .status()
            .unwrap()
            .success(),
        "fixture index build failed"
    );
    let before = node_count(&db_path);
    assert!(before > 0, "fixture must index some nodes (got {before})");

    let stale = code_graph_mcp::domain::INDEX_VERSION - 1;
    for cmd in READ_COMMANDS {
        // Re-stamp before each command: a command that DID wipe would otherwise
        // leave a rebuilt, current-generation index and let the rest pass.
        {
            let db = Database::open_nondestructive(&db_path).unwrap();
            db.conn()
                .pragma_update(None, "application_id", stale)
                .unwrap();
        }
        let out = Command::new(cli_bin())
            .args(*cmd)
            .current_dir(project.path())
            .output()
            .unwrap();
        let after = node_count(&db_path);
        assert_eq!(
            after,
            before,
            "read command `{}` wiped a version-lagging index ({before} → {after} \
             nodes). Route it through CliContext (open_nondestructive*), not the \
             indexer constructor. stderr: {}",
            cmd.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Source-level companion to the behavioural sweep above: no `cmd_*` function
/// may reach for the destructive indexer constructor.
///
/// The behavioural test can only cover subcommands someone remembered to list.
/// This one fails on the *edit* — the moment a read path types
/// `Database::open_with_vec` — without needing anybody to extend a table.
#[test]
fn only_indexer_entry_points_use_the_destructive_constructor() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli.rs"),
    )
    .expect("read src/cli.rs");

    // The two legitimate sites, both of which BUILD an index rather than read
    // one: a full index into an explicit path, and the incremental refresh.
    const ALLOWED_ANCHORS: [&str; 2] = ["fn build_full_index_at", "fn cmd_incremental_index"];

    let lines: Vec<&str> = src.lines().collect();
    let mut current_fn = "<file scope>";
    let mut offenders: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("fn ").or_else(|| t.strip_prefix("pub fn ")) {
            current_fn = rest.split('(').next().unwrap_or(rest);
        }
        // Skip doc comments and ordinary comments: several of them name the
        // constructor while explaining why a reader must NOT use it.
        if t.starts_with("//") {
            continue;
        }
        if line.contains("Database::open_with_vec")
            && !ALLOWED_ANCHORS
                .iter()
                .any(|a| a.trim_start_matches("fn ") == current_fn)
        {
            offenders.push(format!("src/cli.rs:{} (in fn {current_fn})", i + 1));
        }
    }
    assert!(
        offenders.is_empty(),
        "`Database::open_with_vec` performs the destructive INDEX_VERSION \
         revalidation. Reached from a read command it wipes the user's index and \
         nothing rebuilds it (the daagu failure; `similar` shipped this way). Use \
         CliContext::open / open_with_vec instead. Offending sites: {offenders:?}"
    );
}
