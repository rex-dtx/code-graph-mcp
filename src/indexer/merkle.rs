use anyhow::Result;
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

use crate::utils::config::detect_language;

/// Normalize a path's component separators to `/` regardless of host OS.
///
/// All code-graph relative paths (DB rows, CLI output, JSON responses,
/// gitignore-style prefix checks) MUST use forward slashes. On Windows
/// `Path::strip_prefix(...).to_string_lossy()` yields `pkg\scripts\foo.js`
/// which breaks (1) literal `starts_with("src/")` checks, (2) cross-platform
/// test assertions, (3) API contract parity between Linux and Windows
/// users of the tool. This helper does the conversion on Windows and is
/// a no-op on Unix.
#[inline]
pub(crate) fn normalize_rel_path(rel: &Path) -> String {
    #[cfg(windows)]
    {
        rel.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        rel.to_string_lossy().to_string()
    }
}

pub struct DiffResult {
    pub new_files: Vec<String>,
    pub changed_files: Vec<String>,
    pub deleted_files: Vec<String>,
}

/// Hash a file using streaming blake3 (constant memory, handles large files).
pub fn hash_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 16384];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Well-known dependency/build directories that must never be indexed, even
/// when the project has no `.gitignore` (or isn't a git repo, so the `ignore`
/// crate's gitignore rules don't apply). Hidden dirs (`.git`, `.venv`,
/// `.code-graph`, `.mypy_cache`, …) are already skipped via `.hidden(true)`;
/// this covers the *non-hidden* ones that contain real, indexable source —
/// `node_modules/` (JS/TS), `vendor/` (Go), `target/` (Rust/Maven build output).
/// Matched on whole path segments so a directory `target/` is excluded but a
/// file `target.rs` is not, at any nesting depth (e.g. `packages/x/node_modules`).
fn is_excluded_build_dir(rel_str: &str) -> bool {
    const EXCLUDED: &[&str] = &["node_modules", "vendor", "target", "bower_components"];
    rel_str.split('/').any(|seg| EXCLUDED.contains(&seg))
}

/// The walkers below run with the `ignore` crate default `follow_links=false`,
/// so a symlinked source file arrives with `file_type().is_file() == false` and
/// is dropped by the `is_file()` guards — previously the ONLY skip path in this
/// file with no log (audit 2026-07-24: monorepo shared-package symlinks silently
/// vanished from the index with zero observability). Returns the rel path when
/// the skipped entry is a symlink that would otherwise have been indexed.
/// Following links needs cycle/escape protection and is tracked separately.
fn symlink_skip_candidate(entry: &ignore::DirEntry, root: &Path) -> Option<String> {
    if !entry.file_type().is_some_and(|ft| ft.is_symlink()) {
        return None;
    }
    let rel = entry.path().strip_prefix(root).ok()?;
    let rel_str = normalize_rel_path(rel);
    if is_excluded_build_dir(&rel_str) || detect_language(&rel_str).is_none() {
        return None;
    }
    Some(rel_str)
}

/// One aggregate warn per scan (not per file) so the periodic watcher-driven
/// rescans don't spam a line per symlink on every pass.
fn warn_skipped_symlinks(skipped: &[String]) {
    if let Some(first) = skipped.first() {
        tracing::warn!(
            "{} symlinked source file(s) skipped — symlinks are not followed and never indexed (e.g. {})",
            skipped.len(),
            first
        );
    }
}

pub fn scan_directory(root: &Path) -> Result<HashMap<String, String>> {
    // Collect eligible file paths first, then hash in parallel
    let walker = WalkBuilder::new(root)
        .hidden(true) // skip hidden files
        .git_ignore(true) // respect .gitignore
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut file_paths: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut skipped_symlinks: Vec<String> = Vec::new();
    for entry in walker {
        // Skip per-entry errors (permission denied on a subdir, broken
        // symlink, etc.) rather than aborting the whole scan. Without this,
        // one chmod-000 subdir kills `rebuild-index` for the entire repo.
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Skipping directory entry: {}", e);
                continue;
            }
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            if let Some(rel) = symlink_skip_candidate(&entry, root) {
                skipped_symlinks.push(rel);
            }
            continue;
        }
        let path = entry.path();
        if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = normalize_rel_path(rel);
            if rel_str == ".git" || rel_str.starts_with(".git/") {
                continue;
            }
            if is_excluded_build_dir(&rel_str) {
                continue;
            }
            if detect_language(&rel_str).is_none() {
                continue;
            }
            file_paths.push((rel_str, path.to_path_buf()));
        }
    }
    warn_skipped_symlinks(&skipped_symlinks);

    Ok(hash_files_parallel(&file_paths))
}

/// Hash a list of (relative_path, absolute_path) pairs in parallel using rayon.
fn hash_files_parallel(files: &[(String, std::path::PathBuf)]) -> HashMap<String, String> {
    files
        .par_iter()
        .filter_map(|(rel_str, path)| match hash_file(path) {
            Ok(h) => Some((rel_str.clone(), h)),
            Err(e) => {
                tracing::warn!("Skipping file (hash error): {}: {}", path.display(), e);
                None
            }
        })
        .collect()
}

/// Cache of directory and file modification times for skipping unchanged subtrees.
#[derive(Debug, Clone, Default)]
pub struct DirectoryCache {
    dir_mtimes: HashMap<String, SystemTime>,
    /// Per-file mtime cache. Used to detect content modifications in directories
    /// whose own mtime hasn't changed (dir mtime only changes on file add/remove,
    /// not on content modification in ext4/btrfs).
    file_mtimes: HashMap<String, SystemTime>,
}

impl DirectoryCache {
    /// Check if a file was seen during the last directory walk.
    pub fn file_exists(&self, path: &str) -> bool {
        self.file_mtimes.contains_key(path)
    }
}

/// Scan directory with optional mtime cache. Directories whose mtime
/// hasn't changed since the cached value can skip file hashing.
///
/// Known blind spot (accepted tradeoff, audit 2026-07-24): the skip decision
/// compares mtimes for equality, so a content edit landing within the same
/// filesystem timestamp tick as the previous scan (coarse-mtime filesystems,
/// two edits inside one tick) is invisible to this path. The interactive flow
/// is covered anyway — `ensure_file_indexed` always re-hashes content with no
/// mtime shortcut — but background/periodic freshness for files nobody
/// explicitly re-queries can lag until the next real mtime change.
pub fn scan_directory_cached(
    root: &Path,
    cache: Option<&DirectoryCache>,
) -> Result<(HashMap<String, String>, DirectoryCache)> {
    let mut hashes = HashMap::new();
    let mut new_cache = DirectoryCache::default();
    let mut changed_dirs: HashSet<String> = HashSet::new();

    // Collect all entries, logging (not propagating) per-entry errors so that
    // a single unreadable subdir doesn't kill the whole scan.
    let entries: Vec<_> = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(err) => {
                tracing::warn!("Skipping directory entry: {}", err);
                None
            }
        })
        .collect();

    // Pass 1: identify changed directories
    for entry in &entries {
        if !entry.file_type().is_some_and(|ft| ft.is_dir()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            let rel_str = normalize_rel_path(rel);
            if rel_str.starts_with(".git") {
                continue;
            }

            if let Ok(meta) = entry.path().metadata() {
                if let Ok(mtime) = meta.modified() {
                    new_cache.dir_mtimes.insert(rel_str.clone(), mtime);
                    let is_changed = match cache {
                        Some(c) => c.dir_mtimes.get(&rel_str) != Some(&mtime),
                        None => true,
                    };
                    if is_changed {
                        changed_dirs.insert(rel_str);
                    }
                }
            }
        }
    }
    // Root considered changed only when there is no prior cache (first scan)
    if cache.is_none() {
        changed_dirs.insert(String::new());
    }

    // Pass 2: collect files that need hashing, then hash in parallel.
    // Directory mtime only changes on file add/remove (not content edits on ext4/btrfs),
    // so we also check individual file mtimes to catch content modifications.
    let mut files_to_hash: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut skipped_symlinks: Vec<String> = Vec::new();
    for entry in &entries {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            if let Some(rel) = symlink_skip_candidate(entry, root) {
                skipped_symlinks.push(rel);
            }
            continue;
        }
        let path = entry.path();
        if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = normalize_rel_path(rel);
            if rel_str == ".git" || rel_str.starts_with(".git/") {
                continue;
            }
            if is_excluded_build_dir(&rel_str) {
                continue;
            }
            if detect_language(&rel_str).is_none() {
                continue;
            }

            let parent_dir = rel.parent().map(normalize_rel_path).unwrap_or_default();

            // Track file mtime in the new cache
            let file_mtime = path.metadata().ok().and_then(|m| m.modified().ok());
            if let Some(mtime) = file_mtime {
                new_cache.file_mtimes.insert(rel_str.clone(), mtime);
            }

            if !changed_dirs.contains(&parent_dir) {
                // Directory unchanged — check if individual file mtime changed
                let file_changed =
                    match (file_mtime, cache.and_then(|c| c.file_mtimes.get(&rel_str))) {
                        (Some(current), Some(cached)) => current != *cached,
                        (Some(_), None) => true, // No cached mtime — treat as changed
                        _ => false,
                    };
                if !file_changed {
                    continue;
                }
            }

            files_to_hash.push((rel_str, path.to_path_buf()));
        }
    }

    // Hash files in parallel
    warn_skipped_symlinks(&skipped_symlinks);
    hashes.extend(hash_files_parallel(&files_to_hash));

    Ok((hashes, new_cache))
}

pub fn compute_diff(
    old: &HashMap<String, String>,
    current: &HashMap<String, String>,
) -> DiffResult {
    let mut new_files = Vec::new();
    let mut changed_files = Vec::new();
    let mut deleted_files = Vec::new();

    for (path, hash) in current {
        match old.get(path) {
            None => new_files.push(path.clone()),
            Some(old_hash) if old_hash != hash => changed_files.push(path.clone()),
            _ => {}
        }
    }

    for path in old.keys() {
        if !current.contains_key(path) {
            deleted_files.push(path.clone());
        }
    }

    DiffResult {
        new_files,
        changed_files,
        deleted_files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_hash_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("test.txt");
        fs::write(&file, "hello world").unwrap();
        let hash = hash_file(&file).unwrap();
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // blake3 hex = 64 chars
    }

    #[test]
    fn test_diff_detects_new_files() {
        let old: HashMap<String, String> = HashMap::new();
        let mut current = HashMap::new();
        current.insert("a.rs".into(), "hash1".into());

        let diff = compute_diff(&old, &current);
        assert_eq!(diff.new_files.len(), 1);
        assert_eq!(diff.changed_files.len(), 0);
        assert_eq!(diff.deleted_files.len(), 0);
    }

    #[test]
    fn test_diff_detects_changed_files() {
        let mut old = HashMap::new();
        old.insert("a.rs".into(), "hash1".into());

        let mut current = HashMap::new();
        current.insert("a.rs".into(), "hash2".into());

        let diff = compute_diff(&old, &current);
        assert_eq!(diff.new_files.len(), 0);
        assert_eq!(diff.changed_files.len(), 1);
        assert_eq!(diff.deleted_files.len(), 0);
    }

    #[test]
    fn test_diff_detects_deleted_files() {
        let mut old = HashMap::new();
        old.insert("a.rs".into(), "hash1".into());
        let current: HashMap<String, String> = HashMap::new();

        let diff = compute_diff(&old, &current);
        assert_eq!(diff.deleted_files.len(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn test_scan_directory_skips_symlinked_file_and_candidate_detection() {
        // Pins CURRENT behavior (audit 2026-07-24 P1-3): symlinked source files
        // are not followed and never indexed. The observability fix only adds a
        // warn — if a future change makes the walker follow links, this test
        // must be updated deliberately alongside cycle/escape protection.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("real.rs"), "fn a() {}").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real.rs"), tmp.path().join("linked.rs"))
            .unwrap();
        let hashes = scan_directory(tmp.path()).unwrap();
        assert!(hashes.contains_key("real.rs"));
        assert!(
            !hashes.contains_key("linked.rs"),
            "symlinked file unexpectedly indexed — update the symlink warn path too"
        );
        // The skipped symlink is recognized as a would-have-been-indexed
        // candidate (what the aggregate warn reports)…
        let entry = ignore::WalkBuilder::new(tmp.path())
            .build()
            .filter_map(|e| e.ok())
            .find(|e| e.path().file_name().is_some_and(|n| n == "linked.rs"))
            .unwrap();
        assert_eq!(
            symlink_skip_candidate(&entry, tmp.path()).as_deref(),
            Some("linked.rs")
        );
        // …while non-source symlinks stay out of the warn.
        fs::write(tmp.path().join("notes.xyz"), "").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("notes.xyz"), tmp.path().join("l.xyz")).unwrap();
        let entry = ignore::WalkBuilder::new(tmp.path())
            .build()
            .filter_map(|e| e.ok())
            .find(|e| e.path().file_name().is_some_and(|n| n == "l.xyz"))
            .unwrap();
        assert_eq!(symlink_skip_candidate(&entry, tmp.path()), None);
    }

    #[test]
    fn test_scan_directory_cached_skips_unchanged() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main(){}").unwrap();

        let (hashes1, cache1) = scan_directory_cached(tmp.path(), None).unwrap();
        assert_eq!(hashes1.len(), 1);

        // Second scan with same cache: should return empty (dirs unchanged → files skipped)
        let (hashes2, _cache2) = scan_directory_cached(tmp.path(), Some(&cache1)).unwrap();
        // Files in unchanged dirs are skipped
        assert_eq!(hashes2.len(), 0);
    }

    #[test]
    fn test_scan_directory_cached_detects_new_file() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main(){}").unwrap();

        let (_hashes1, cache1) = scan_directory_cached(tmp.path(), None).unwrap();

        // Add a new file (changes directory mtime)
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(tmp.path().join("src/lib.rs"), "pub fn lib(){}").unwrap();

        let (hashes2, _cache2) = scan_directory_cached(tmp.path(), Some(&cache1)).unwrap();
        // Both files should be hashed since src/ dir changed
        assert_eq!(hashes2.len(), 2);
        assert!(hashes2.contains_key("src/lib.rs"));
    }

    #[test]
    fn test_scan_directory_cached_detects_root_file_content_change() {
        // Verifies that editing a file directly in the project root is detected
        // on a cached scan, even though root is no longer unconditionally marked changed.
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        // File directly in root (parent_dir = "")
        fs::write(tmp.path().join("main.rs"), "fn main(){}").unwrap();

        let (_hashes1, cache1) = scan_directory_cached(tmp.path(), None).unwrap();

        // Modify content of root-level file (dir mtime does NOT change)
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(
            tmp.path().join("main.rs"),
            "fn main(){ println!(\"changed\"); }",
        )
        .unwrap();

        let (hashes2, _cache2) = scan_directory_cached(tmp.path(), Some(&cache1)).unwrap();
        // The file mtime check (Pass 2) should detect the content change
        assert_eq!(hashes2.len(), 1);
        assert!(hashes2.contains_key("main.rs"));
    }

    #[test]
    fn test_scan_directory_respects_gitignore() {
        let tmp = TempDir::new().unwrap();
        // Initialize a git repo so that .gitignore rules are respected by the ignore crate
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join(".gitignore"), "node_modules/\n*.log").unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main(){}").unwrap();
        fs::create_dir_all(tmp.path().join("node_modules")).unwrap();
        fs::write(tmp.path().join("node_modules/pkg.js"), "x").unwrap();
        fs::write(tmp.path().join("debug.log"), "log").unwrap();

        let hashes = scan_directory(tmp.path()).unwrap();
        assert!(hashes.contains_key("src/main.rs"));
        assert!(!hashes.contains_key("node_modules/pkg.js"));
        assert!(!hashes.contains_key("debug.log"));
    }

    #[test]
    fn test_scan_excludes_build_dirs_without_gitignore() {
        // No .git / .gitignore — the `ignore` crate's gitignore rules don't
        // apply, so build/dependency dirs must be excluded by the hardcoded
        // safety net. A *file* named `target.rs` must still be indexed.
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main(){}").unwrap();
        fs::write(tmp.path().join("src/target.rs"), "pub fn t() -> i32 { 1 }").unwrap();
        fs::create_dir_all(tmp.path().join("node_modules/pkg")).unwrap();
        fs::write(tmp.path().join("node_modules/pkg/i.js"), "function dep(){}").unwrap();
        fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
        fs::write(tmp.path().join("target/debug/junk.rs"), "pub fn j(){}").unwrap();
        fs::create_dir_all(tmp.path().join("packages/a/node_modules/b")).unwrap();
        fs::write(
            tmp.path().join("packages/a/node_modules/b/c.js"),
            "function nested(){}",
        )
        .unwrap();

        let hashes = scan_directory(tmp.path()).unwrap();
        assert!(hashes.contains_key("src/main.rs"));
        assert!(
            hashes.contains_key("src/target.rs"),
            "a file named target.rs is source, not a build dir"
        );
        assert!(!hashes.contains_key("node_modules/pkg/i.js"));
        assert!(!hashes.contains_key("target/debug/junk.rs"));
        assert!(
            !hashes.contains_key("packages/a/node_modules/b/c.js"),
            "nested node_modules must be excluded"
        );
    }

    #[test]
    fn test_is_excluded_build_dir() {
        assert!(is_excluded_build_dir("node_modules/x.js"));
        assert!(is_excluded_build_dir("packages/a/node_modules/b.js"));
        assert!(is_excluded_build_dir("target/debug/x.rs"));
        assert!(is_excluded_build_dir("vendor/lib/x.go"));
        assert!(!is_excluded_build_dir("src/target.rs"));
        assert!(!is_excluded_build_dir("src/vendoring.rs"));
        assert!(!is_excluded_build_dir("src/main.rs"));
    }
}
