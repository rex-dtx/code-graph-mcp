//! Integration tests for shared graph snapshot. All tests use file:// URLs
//! to keep network out of CI — real GitHub API paths are exercised manually.

use code_graph_mcp::snapshot;
use code_graph_mcp::storage::db::Database;
use std::process::Command;
use tempfile::TempDir;

fn init_git_repo_with_src() -> TempDir {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(p)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "t@t"])
        .current_dir(p)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "t"])
        .current_dir(p)
        .status()
        .unwrap();
    std::fs::create_dir_all(p.join("src")).unwrap();
    std::fs::write(
        p.join("src/lib.rs"),
        "pub fn alpha() {}\npub fn beta() { alpha(); }\npub fn gamma() { beta(); }\n",
    )
    .unwrap();
    std::fs::write(p.join("src/main.rs"), "fn main() { /* placeholder */ }\n").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(p)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-q", "-m", "init"])
        .current_dir(p)
        .status()
        .unwrap();
    dir
}

fn build_snapshot_zst(src: &TempDir) -> std::path::PathBuf {
    let raw = src.path().join("snapshot.db");
    snapshot::create(src.path(), &raw, false).unwrap();
    let bytes = std::fs::read(&raw).unwrap();
    let zst = src.path().join("snapshot.db.zst");
    std::fs::write(&zst, zstd::encode_all(&bytes[..], 9).unwrap()).unwrap();
    zst
}

fn count_nodes(db_path: &std::path::Path) -> i64 {
    let db = Database::open(db_path).unwrap();
    db.conn()
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap()
}
fn count_edges(db_path: &std::path::Path) -> i64 {
    let db = Database::open(db_path).unwrap();
    db.conn()
        .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn snapshot_round_trip_node_counts_match() {
    let src = init_git_repo_with_src();
    let zst = build_snapshot_zst(&src);

    let target = init_git_repo_with_src();
    let url = format!("file://{}", zst.display());
    snapshot::try_install(&url, target.path()).unwrap();

    let installed = target.path().join(".code-graph").join("index.db");
    let raw = src.path().join("snapshot.db");
    assert_eq!(
        count_nodes(&installed),
        count_nodes(&raw),
        "node count must match between snapshot source and installed"
    );
    assert_eq!(
        count_edges(&installed),
        count_edges(&raw),
        "edge count must match between snapshot source and installed"
    );
}

#[test]
fn snapshot_install_falls_back_on_corrupt_archive() {
    let target = init_git_repo_with_src();
    let bad = TempDir::new().unwrap();
    let bad_path = bad.path().join("bad.db.zst");
    std::fs::write(&bad_path, b"\x00\x00\x00 not zstd").unwrap();
    let url = format!("file://{}", bad_path.display());

    let err = snapshot::try_install(&url, target.path()).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("zstd")
            || err.to_string().to_lowercase().contains("decode"),
        "got: {err}"
    );

    let cg_dir = target.path().join(".code-graph");
    if cg_dir.exists() {
        for entry in std::fs::read_dir(&cg_dir).unwrap().flatten() {
            let s = entry.file_name().to_string_lossy().into_owned();
            assert!(
                s != "index.db" && !s.ends_with(".partial"),
                "leftover after failure: {s}"
            );
        }
    }
}

#[test]
fn snapshot_install_rejects_newer_schema() {
    let src = init_git_repo_with_src();
    let raw = src.path().join("snapshot.db");
    snapshot::create(src.path(), &raw, false).unwrap();

    // Manually overwrite snapshot_schema_version to a number larger than our build
    let db = Database::open(&raw).unwrap();
    db.conn()
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = ?2",
            rusqlite::params![
                "999",
                code_graph_mcp::snapshot::meta::META_SNAPSHOT_SCHEMA_VERSION
            ],
        )
        .unwrap();
    drop(db);

    let bytes = std::fs::read(&raw).unwrap();
    let zst = src.path().join("snapshot.db.zst");
    std::fs::write(&zst, zstd::encode_all(&bytes[..], 9).unwrap()).unwrap();

    let target = init_git_repo_with_src();
    let url = format!("file://{}", zst.display());
    let err = snapshot::try_install(&url, target.path()).unwrap_err();
    assert!(
        err.to_string().contains("newer") || err.to_string().contains("999"),
        "got: {err}"
    );
}

#[test]
fn snapshot_install_concurrent_serialized_via_filesystem() {
    // Two threads racing to install the same snapshot. Verifiable contract:
    // at least one creates the final index.db and no partials remain.
    use std::sync::Arc;
    use std::thread;

    let src = init_git_repo_with_src();
    let zst = build_snapshot_zst(&src);
    let target = Arc::new(init_git_repo_with_src());
    let url = format!("file://{}", zst.display());

    let (t1, t2) = {
        let url_a = url.clone();
        let url_b = url.clone();
        let r_a = Arc::clone(&target);
        let r_b = Arc::clone(&target);
        (
            thread::spawn(move || snapshot::try_install(&url_a, r_a.path())),
            thread::spawn(move || snapshot::try_install(&url_b, r_b.path())),
        )
    };
    let r1 = t1.join().unwrap();
    let r2 = t2.join().unwrap();
    assert!(
        r1.is_ok() || r2.is_ok(),
        "at least one install should succeed"
    );
    let installed = target.path().join(".code-graph").join("index.db");
    assert!(installed.exists());

    for entry in std::fs::read_dir(target.path().join(".code-graph"))
        .unwrap()
        .flatten()
    {
        let s = entry.file_name().to_string_lossy().into_owned();
        assert!(!s.ends_with(".partial"), "leftover partial: {s}");
    }
}

#[test]
fn snapshot_then_incremental_picks_up_drift() {
    use code_graph_mcp::indexer::pipeline::run_incremental_index;

    let src = init_git_repo_with_src();
    let zst = build_snapshot_zst(&src);

    let target = init_git_repo_with_src();
    // Modify a file BEFORE install so incremental sees drift
    std::fs::write(target.path().join("src/lib.rs"),
        "pub fn alpha() {}\npub fn beta() { alpha(); }\npub fn gamma() { beta(); }\npub fn delta() { gamma(); }\n").unwrap();

    let url = format!("file://{}", zst.display());
    snapshot::try_install(&url, target.path()).unwrap();

    let installed = target.path().join(".code-graph").join("index.db");
    let nodes_before = count_nodes(&installed);

    let db = Database::open_with_vec(&installed).unwrap();
    run_incremental_index(&db, target.path(), None, None).unwrap();

    let nodes_after = count_nodes(&installed);
    assert!(
        nodes_after > nodes_before,
        "expected delta function to add nodes; before={nodes_before} after={nodes_after}"
    );
}
