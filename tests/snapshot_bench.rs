//! Snapshot bench — `--ignored` like other benches in this repo. Run with:
//!   cargo test --test snapshot_bench -- --ignored --nocapture
//!
//! Targets (from spec §9.4):
//!   create() runtime <5s, output size <2MB on a ~5K-node fixture
//!   try_install() end-to-end (file://) <1s
//!   nodes/edges count delta vs full-index = 0

use code_graph_mcp::snapshot;
use code_graph_mcp::storage::db::Database;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;
use tempfile::TempDir;

fn build_medium_fixture(target_files: usize) -> TempDir {
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

    // ~25 fns per file → ~5000 nodes at 200 files
    for i in 0..target_files {
        let mut content = String::new();
        for j in 0..25 {
            content.push_str(&format!(
                "pub fn fn_{i}_{j}() -> i32 {{ {} }}\n",
                if j > 0 {
                    format!("fn_{i}_{}() + 1", j - 1)
                } else {
                    "0".into()
                }
            ));
        }
        std::fs::write(p.join(format!("src/m_{i}.rs")), &content).unwrap();
    }
    Command::new("git")
        .args(["add", "."])
        .current_dir(p)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-q", "-m", "fixture"])
        .current_dir(p)
        .status()
        .unwrap();
    dir
}

fn count_nodes(db_path: &std::path::Path) -> i64 {
    Database::open(db_path)
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap()
}

#[test]
#[ignore = "perf bench; run: cargo test --test snapshot_bench -- --ignored --nocapture"]
fn bench_create_size_and_time() {
    let fixture = build_medium_fixture(200);
    let out = fixture.path().join("snapshot.db");

    let t0 = Instant::now();
    snapshot::create(fixture.path(), &out, false).unwrap();
    let elapsed = t0.elapsed();

    let raw_size = std::fs::metadata(&out).unwrap().len();
    let raw = std::fs::read(&out).unwrap();
    let compressed = zstd::encode_all(&raw[..], 9).unwrap();
    let zst_size = compressed.len() as u64;

    println!("create: {elapsed:?}, raw {raw_size} bytes, zst {zst_size} bytes");
    assert!(
        elapsed.as_secs() < 5,
        "create took {elapsed:?} (target <5s)"
    );
    assert!(
        zst_size < 2 * 1024 * 1024,
        "zst size {zst_size} exceeds 2 MiB target"
    );
}

#[test]
#[ignore = "perf bench; run: cargo test --test snapshot_bench -- --ignored --nocapture"]
fn bench_install_round_trip_under_one_second() {
    let fixture = build_medium_fixture(200);
    let raw_db = fixture.path().join("snapshot.db");
    snapshot::create(fixture.path(), &raw_db, false).unwrap();
    let raw = std::fs::read(&raw_db).unwrap();
    let zst_path: PathBuf = fixture.path().join("snapshot.db.zst");
    std::fs::write(&zst_path, zstd::encode_all(&raw[..], 9).unwrap()).unwrap();

    let target = TempDir::new().unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(target.path())
        .status()
        .unwrap();
    let url = format!("file://{}", zst_path.display());

    let t0 = Instant::now();
    snapshot::try_install(&url, target.path()).unwrap();
    let elapsed = t0.elapsed();

    println!("install (file://): {:?}", elapsed);
    assert!(
        elapsed.as_millis() < 1000,
        "install took {elapsed:?} (target <1s)"
    );
}

#[test]
#[ignore = "perf bench; run: cargo test --test snapshot_bench -- --ignored --nocapture"]
fn bench_node_count_parity_full_vs_snapshot() {
    use code_graph_mcp::indexer::pipeline::run_full_index;

    let fixture = build_medium_fixture(50);

    // Path A: full index in-place
    let a = fixture.path().join(".code-graph_a");
    std::fs::create_dir_all(&a).unwrap();
    let db_a = a.join("index.db");
    let dba = Database::open(&db_a).unwrap();
    run_full_index(&dba, fixture.path(), None, None).unwrap();
    drop(dba);
    let nodes_full = count_nodes(&db_a);

    // Path B: snapshot then install
    let snap = fixture.path().join("snap.db");
    snapshot::create(fixture.path(), &snap, false).unwrap();
    let raw = std::fs::read(&snap).unwrap();
    let zst = fixture.path().join("snap.db.zst");
    std::fs::write(&zst, zstd::encode_all(&raw[..], 9).unwrap()).unwrap();

    let target = TempDir::new().unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(target.path())
        .status()
        .unwrap();
    snapshot::try_install(&format!("file://{}", zst.display()), target.path()).unwrap();
    let nodes_snap = count_nodes(&target.path().join(".code-graph").join("index.db"));

    println!("nodes full={nodes_full} snapshot={nodes_snap}");
    assert_eq!(
        nodes_full, nodes_snap,
        "node count must match between full-index and snapshot"
    );
}
