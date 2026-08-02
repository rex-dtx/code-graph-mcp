//! A file the grammar cannot parse must still be RECORDED, not left lying.
//!
//! `pre_parse_batch` has three "we did not parse this" exits and they are NOT
//! interchangeable:
//!   - oversize  -> `Skipped` (advance the hash, drop stale nodes)
//!   - parse failure -> `Skipped` (same: the bytes are known, they just do not
//!     parse, so the old symbols are definitively wrong)
//!   - read / hash failure -> `Nothing` (transient environment fault; one bad
//!     read must not erase a file's symbols)
//!
//! The oversize arm has a regression test. The parse-failure arm had none, so
//! nothing stopped it being "simplified" back to `Nothing` — which restores the
//! original defect exactly: the file keeps lying with symbols from before it
//! broke, and its stored hash never advances so `compute_diff` re-reports it as
//! changed on every single run, forever.
//!
//! # Why this is its own test binary
//!
//! `parse_tree` only returns `Err` when tree-sitter's parser hits its timeout
//! (it recovers from ordinary syntax errors by inserting ERROR nodes and
//! returning a tree, so bad syntax alone will NOT get here). That timeout comes
//! from `CODE_GRAPH_PARSE_TIMEOUT_MS` through a process-global `OnceLock`, which
//! latches on the first read. Inside the lib test binary another test parses
//! first and pins it at the 5 s default, so the override would be silently
//! inert. Each `tests/*.rs` compiles to its own binary, which is the only place
//! this knob is actually settable.

use code_graph_mcp::indexer::pipeline::{run_full_index, run_incremental_index};
use code_graph_mcp::storage::db::Database;
use std::fs;
use tempfile::TempDir;

/// Budget for one file's parse. The ordinary 36-byte files below parse in
/// microseconds, so this is never at risk for them.
const PARSE_BUDGET_MS: &str = "5";

/// Deeply nested parentheses, sized from a MEASURED margin rather than an
/// assumed one. Tree-sitter is *linear* on this shape at ~1.5 µs/level (40k →
/// 63 ms, 100k → 149 ms, 200k → 289 ms on the dev box), so 200k levels sits
/// ~58x past the 5 ms budget above; even hardware an order of magnitude faster
/// keeps a 5x margin.
///
/// An earlier draft used 40k against a 50 ms budget on the guess that the parse
/// was superlinear and would take "seconds". It takes 63 ms — a 1.26x margin,
/// i.e. a test that a slightly quicker machine turns red. Timing thresholds get
/// measured, not reasoned about.
///
/// 400 KB, comfortably under the 1 MiB `max_file_size`, so this exercises the
/// PARSE exit and not the oversize one beside it (asserted by the mutation
/// cross-check: breaking the oversize arm leaves this test green).
fn pathological_source() -> String {
    let depth = 200_000;
    let mut s = String::with_capacity(depth * 2 + 64);
    s.push_str("export const boom = ");
    for _ in 0..depth {
        s.push('(');
    }
    s.push('1');
    for _ in 0..depth {
        s.push(')');
    }
    s.push_str(";\n");
    s
}

fn file_row(db: &Database, path: &str) -> Option<(String, i64)> {
    db.conn()
        .query_row(
            "SELECT f.blake3_hash, (SELECT COUNT(*) FROM nodes n WHERE n.file_id = f.id)
             FROM files f WHERE f.path = ?1",
            [path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
}

#[test]
fn unparsable_file_is_recorded_so_it_stops_lying_and_stops_rediffing() {
    // SAFETY: single-threaded, first statement of the only test in this binary,
    // so nothing has read the OnceLock yet.
    unsafe {
        std::env::set_var("CODE_GRAPH_PARSE_TIMEOUT_MS", PARSE_BUDGET_MS);
    }

    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let src = project_dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    // Starts as ordinary, parseable source so there is a real symbol to become
    // stale. Without this leg the test could only show "no nodes appeared",
    // which a `Nothing` return satisfies just as well.
    fs::write(
        src.join("wide.ts"),
        "export function Wide() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        src.join("other.ts"),
        "export function Other() { return 2; }\n",
    )
    .unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let (hash_before, nodes_before) = file_row(&db, "src/wide.ts")
        .expect("precondition: the parseable version must be indexed at all");
    assert!(
        nodes_before > 0,
        "precondition: wide.ts must contribute symbols before it breaks, got {nodes_before}"
    );

    // Now make it unparseable within the budget.
    fs::write(src.join("wide.ts"), pathological_source()).unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    // NOT asserted via `files_indexed`: a skipped file is deliberately absent
    // from `all_indexed`, so that counter reads 0 whether the file was recorded
    // or dropped on the floor. It cannot tell the two apart, which is exactly
    // the distinction under test. Ask the database instead.
    let (hash_after, nodes_after) = file_row(&db, "src/wide.ts")
        .expect("an unparsable file must keep its files row, not vanish from the index");
    assert_eq!(
        nodes_after, 0,
        "stale symbols from before the file broke are still being served: {nodes_after} node(s)"
    );
    assert_ne!(
        hash_before, hash_after,
        "stored hash never advanced — the run did not record the file at all"
    );
    // Stronger than "it changed": it must equal what is on disk, because that
    // equality is precisely what `compute_diff` tests. Anything else and the
    // file is re-reported as changed on every run for the rest of time.
    let on_disk = code_graph_mcp::indexer::merkle::hash_file(&src.join("wide.ts")).unwrap();
    assert_eq!(
        hash_after, on_disk,
        "stored hash does not match the bytes on disk, so this file re-diffs forever"
    );

    // The other file must be untouched — a skip that took its neighbours' nodes
    // with it would satisfy every assertion above.
    let (_, other_nodes) = file_row(&db, "src/other.ts").expect("other.ts must still be indexed");
    assert!(
        other_nodes > 0,
        "recording the skipped file must not disturb its neighbours"
    );

    // Idempotence: a second run over an unchanged tree must leave the recorded
    // identity exactly where it is.
    let second = run_incremental_index(&db, project_dir.path(), None, None).unwrap();
    assert_eq!(
        second.files_deleted, 0,
        "an unparsable file must not be deleted"
    );
    assert_eq!(
        file_row(&db, "src/wide.ts"),
        Some((on_disk, 0)),
        "the recorded identity must survive a follow-up run unchanged"
    );
}
