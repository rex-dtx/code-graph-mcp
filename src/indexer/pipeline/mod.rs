//! Indexer pipeline. Public entry points + per-concern submodules:
//! - `embed`: batch embedding store
//! - `context`: context-string assembly + recovery paths
//! - `python_modules`: dotted-path → file-path resolution map
//! - `resolve`: ambiguous-target refinement + pending-call sweep
//! - `index_files`: the giant Phase-0..3 orchestrator (kept whole — its
//!   phases share local transaction/atomics/batch state)

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::embedding::model::EmbeddingModel;
use crate::indexer::merkle::{compute_diff, scan_directory, scan_directory_cached, DirectoryCache};
use crate::storage::db::Database;
use crate::storage::queries::{delete_files_by_paths, get_all_file_hashes, get_dirty_node_ids};

mod context;
mod embed;
mod index_files;
mod js_modules;
mod python_modules;
mod resolve;

#[cfg(test)]
mod tests;

pub use context::repair_null_context_strings;
pub use embed::embed_and_store_batch;

use context::regenerate_context_strings;
use index_files::index_files;

/// Counters for indexing observability — tracks skipped items.
#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub files_skipped_size: usize,
    pub files_skipped_parse: usize,
    pub files_skipped_read: usize,
    pub files_skipped_hash: usize,
    pub files_skipped_language: usize,
    /// Files that parsed into a tree carrying tree-sitter ERROR node(s). Unlike the
    /// `files_skipped_*` counters, these files WERE indexed — tree-sitter recovers
    /// from syntax errors and still yields a tree — but symbol extraction ran over a
    /// damaged parse, so some symbols may be missing. Observability only.
    pub files_with_parse_errors: usize,
}

pub struct IndexResult {
    pub files_indexed: usize,
    pub files_deleted: usize,
    pub nodes_created: usize,
    pub edges_created: usize,
    pub stats: IndexStats,
}

/// Which stage of an index run a progress event describes.
/// `Files` fires after each parsed batch with a moving `done` count; `Finalizing`
/// fires as a heartbeat between the post-batch full-graph phases (context strings,
/// pending-call sweep, import bind/prune, ANALYZE) where `done` no longer moves.
/// Consumers use the distinction to render "finalizing" instead of a frozen
/// `done/total` — and the heartbeat itself keeps the progress file's mtime fresh
/// so a stale-file gate can tell "long tail phase" apart from "indexer died".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPhase {
    Files,
    Finalizing,
}

/// Progress callback: called with (phase, files_done, files_total).
pub type ProgressFn<'a> = &'a dyn Fn(IndexPhase, usize, usize);

/// Name of the statusline progress file under `.code-graph/`, written by the MCP
/// server's startup indexing thread and read by the plugin statusline.
pub const INDEXING_STATUS_FILE: &str = "indexing-status.json";

/// Age past which `indexing-status.json` is a leftover from a killed process, not
/// a live indexer: live runs heartbeat at least once per batch / finalize phase,
/// orders of magnitude more often than this. The statusline applies the same
/// threshold (INDEXING_STALE_MS in statusline.js) before trusting the file.
pub const INDEXING_STATUS_STALE_SECS: u64 = 120;

/// Remove a leftover `.code-graph/indexing-status.json` older than
/// [`INDEXING_STATUS_STALE_SECS`]. A killed session (SIGKILL, session exit)
/// skips the IndexGuard drop that normally deletes the file, pinning the
/// statusline at a phantom "indexing N/M" forever. Safe against a live indexer:
/// its file is fresh and left alone, and even a racing removal is repaired by
/// the indexer's next heartbeat write.
pub fn remove_stale_indexing_status(project_root: &Path) {
    remove_indexing_status_older_than(
        project_root,
        std::time::Duration::from_secs(INDEXING_STATUS_STALE_SECS),
    );
}

/// Testable inner: remove the progress file when its mtime is at least `max_age`
/// old (or unreadable — an mtime we can't read cannot prove liveness).
pub fn remove_indexing_status_older_than(project_root: &Path, max_age: std::time::Duration) {
    let path = project_root
        .join(crate::domain::CODE_GRAPH_DIR)
        .join(INDEXING_STATUS_FILE);
    let Ok(meta) = std::fs::metadata(&path) else {
        return;
    };
    let stale = meta
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|age| age >= max_age)
        .unwrap_or(true);
    if stale {
        let _ = std::fs::remove_file(&path);
    }
}

pub fn run_full_index(
    db: &Database,
    project_root: &Path,
    model: Option<&EmbeddingModel>,
    progress: Option<ProgressFn>,
) -> Result<IndexResult> {
    let current_hashes = scan_directory(project_root)?;
    let files: Vec<String> = current_hashes.keys().cloned().collect();
    index_files(
        db,
        project_root,
        &files,
        &current_hashes,
        model,
        &[],
        progress,
    )
}

/// Reindex a single file when its on-disk hash differs from the stored hash.
/// No-op when the hashes match (or `rel_path` was never indexed in a way that
/// would currently reindex it). Returns true when a reindex (or stale-row
/// cleanup) actually fired.
///
/// Used by query-time freshness: when an MCP tool receives an explicit
/// `file_path` argument, the agent is signaling "I just edited this; please
/// answer against the current bytes." The 30s `last_incremental_check`
/// debounce in the server is too coarse for tight Edit→search loops.
///
/// Cross-file dirty-edge handling mirrors `run_incremental_index`: collect
/// dirty node IDs **before** re-indexing (cascade delete strips old edges),
/// True when `rel_path` is a safe project-relative path: no absolute root and no
/// leading `..` that climbs above the project. [`ensure_file_indexed`] keys the
/// files table by this relative path, so anything else is meaningless as a key
/// *and* a path-traversal risk — the MCP `file_path` args reach the freshness
/// path (`ensure_file_fresh_opt`) without going through `normalize_user_path`.
pub(crate) fn is_safe_relative_path(rel_path: &str) -> bool {
    use std::path::Component;
    let mut depth: i32 = 0;
    for comp in Path::new(rel_path).components() {
        match comp {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            // An absolute root or a Windows prefix (`C:\`) escapes the project.
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// then regenerate context strings + embeddings once the new nodes exist.
pub fn ensure_file_indexed(
    db: &Database,
    project_root: &Path,
    rel_path: &str,
    model: Option<&EmbeddingModel>,
) -> Result<bool> {
    // Defense-in-depth: rel_path is a project-relative DB key by contract, but a
    // caller that forwards an unnormalized client path (absolute, or `..` climbing
    // out of the project) must not make us stat/hash/index a file outside
    // project_root. Treat such a path as "nothing to refresh" — consistent with
    // the not-indexable early returns below, not a hard error.
    if !is_safe_relative_path(rel_path) {
        return Ok(false);
    }
    let abs_path = project_root.join(rel_path);

    // Missing-file path: drop stale row so future queries don't return phantom nodes.
    if !abs_path.is_file() {
        let exists_in_db: Option<i64> = db
            .conn()
            .query_row("SELECT id FROM files WHERE path = ?1", [rel_path], |row| {
                row.get(0)
            })
            .ok();
        if exists_in_db.is_some() {
            let tx = db.conn().unchecked_transaction()?;
            delete_files_by_paths(db.conn(), &[rel_path.to_string()])?;
            tx.commit()?;
            return Ok(true);
        }
        return Ok(false);
    }

    // Skip files we wouldn't index in the first place (binary / wrong language).
    if crate::utils::config::detect_language(rel_path).is_none() {
        return Ok(false);
    }

    let on_disk_hash = crate::indexer::merkle::hash_file(&abs_path)?;
    let stored_hash: Option<String> = db
        .conn()
        .query_row(
            "SELECT blake3_hash FROM files WHERE path = ?1",
            [rel_path],
            |row| row.get(0),
        )
        .ok();

    if stored_hash.as_deref() == Some(&on_disk_hash) {
        return Ok(false);
    }

    // Cross-file edges into this file's nodes need their context strings rebuilt
    // *after* the node IDs are replaced — capture the dirty set BEFORE re-indexing.
    let dirty_node_ids = collect_dirty_node_ids(db, std::slice::from_ref(&rel_path.to_string()))?;

    let mut hashes: HashMap<String, String> = HashMap::new();
    hashes.insert(rel_path.to_string(), on_disk_hash);
    let files = vec![rel_path.to_string()];
    index_files(db, project_root, &files, &hashes, model, &[], None)?;

    if !dirty_node_ids.is_empty() {
        regenerate_context_strings(db, &dirty_node_ids, model)?;
    }
    Ok(true)
}

pub fn run_incremental_index(
    db: &Database,
    project_root: &Path,
    model: Option<&EmbeddingModel>,
    progress: Option<ProgressFn>,
) -> Result<IndexResult> {
    let start = std::time::Instant::now();
    let stored_hashes = get_all_file_hashes(db.conn())?;
    let current_hashes = scan_directory(project_root)?;
    let diff = compute_diff(&stored_hashes, &current_hashes);

    // Preserve <external> pseudo-file across incremental indexes
    let deleted_files: Vec<String> = diff
        .deleted_files
        .into_iter()
        .filter(|p| p != "<external>")
        .collect();
    let to_index: Vec<String> = [diff.new_files, diff.changed_files].concat();

    let dirty_node_ids = if !to_index.is_empty() {
        collect_dirty_node_ids(db, &to_index)?
    } else {
        HashSet::new()
    };

    let result = index_files(
        db,
        project_root,
        &to_index,
        &current_hashes,
        model,
        &deleted_files,
        progress,
    )?;

    if !dirty_node_ids.is_empty() {
        // Heartbeat: context-string regeneration for dirty dependents runs after
        // index_files' own finalize ticks and can take a while on wide fan-in.
        if let Some(cb) = progress {
            cb(IndexPhase::Finalizing, result.files_indexed, to_index.len());
        }
        regenerate_context_strings(db, &dirty_node_ids, model)?;
    }

    if result.files_indexed > 0 || !deleted_files.is_empty() {
        tracing::info!(
            "[incremental] {} files changed, {} deleted, {} nodes, {} edges, {:.1}s",
            result.files_indexed,
            deleted_files.len(),
            result.nodes_created,
            result.edges_created,
            start.elapsed().as_secs_f64()
        );
    }

    Ok(result)
}

/// Incremental index with directory mtime cache for faster scanning.
/// Files in unchanged directories are skipped entirely.
pub fn run_incremental_index_cached(
    db: &Database,
    project_root: &Path,
    model: Option<&EmbeddingModel>,
    dir_cache: Option<&DirectoryCache>,
    progress: Option<ProgressFn>,
) -> Result<(IndexResult, DirectoryCache)> {
    let start = std::time::Instant::now();
    let stored_hashes = get_all_file_hashes(db.conn())?;
    let (mut current_hashes, new_cache) = scan_directory_cached(project_root, dir_cache)?;

    // Merge stored hashes for files in unchanged directories.
    // scan_directory_cached skips files in unchanged dirs, so we need to
    // carry forward their stored hashes to prevent false "deleted" diffs.
    // Use new_cache.file_mtimes (populated for ALL walked files) to check existence
    // without per-file stat calls.
    for (path, hash) in &stored_hashes {
        if !current_hashes.contains_key(path) && new_cache.file_exists(path) {
            current_hashes.insert(path.clone(), hash.clone());
        }
    }

    let diff = compute_diff(&stored_hashes, &current_hashes);

    // Preserve <external> pseudo-file across incremental indexes
    let deleted_files: Vec<String> = diff
        .deleted_files
        .into_iter()
        .filter(|p| p != "<external>")
        .collect();
    let to_index: Vec<String> = [diff.new_files, diff.changed_files].concat();

    let dirty_node_ids = if !to_index.is_empty() {
        collect_dirty_node_ids(db, &to_index)?
    } else {
        HashSet::new()
    };

    let result = index_files(
        db,
        project_root,
        &to_index,
        &current_hashes,
        model,
        &deleted_files,
        progress,
    )?;

    if !dirty_node_ids.is_empty() {
        // Heartbeat: context-string regeneration for dirty dependents runs after
        // index_files' own finalize ticks and can take a while on wide fan-in.
        if let Some(cb) = progress {
            cb(IndexPhase::Finalizing, result.files_indexed, to_index.len());
        }
        regenerate_context_strings(db, &dirty_node_ids, model)?;
    }

    if result.files_indexed > 0 || !deleted_files.is_empty() {
        tracing::info!(
            "[incremental] {} files changed, {} deleted, {} nodes, {} edges, {:.1}s",
            result.files_indexed,
            deleted_files.len(),
            result.nodes_created,
            result.edges_created,
            start.elapsed().as_secs_f64()
        );
    }

    Ok((result, new_cache))
}

/// Collect node IDs in OTHER files that have edges pointing to nodes in the changed files.
/// Must be called BEFORE re-indexing (cascade delete removes old edges).
fn collect_dirty_node_ids(db: &Database, changed_paths: &[String]) -> Result<HashSet<i64>> {
    let mut changed_file_ids = Vec::new();
    for path in changed_paths {
        let file_id: Option<i64> = db
            .conn()
            .query_row("SELECT id FROM files WHERE path = ?1", [path], |row| {
                row.get(0)
            })
            .ok();
        if let Some(id) = file_id {
            changed_file_ids.push(id);
        }
    }
    let ids = get_dirty_node_ids(db.conn(), &changed_file_ids)?;
    Ok(ids.into_iter().collect())
}
