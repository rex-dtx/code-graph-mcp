use anyhow::Result;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};

use crate::domain::{CODE_GRAPH_DIR, NO_METRICS_SENTINEL};
use crate::storage::db::Database;
use crate::storage::queries;

/// `$HOME` (Unix) / `%USERPROFILE%` (Windows) without pulling the `dirs` crate,
/// which lives behind the `embed-model` feature. `None` when unset → the walk is
/// simply unbounded (degrades to the pre-home-bound behavior).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Resolve the project root from an explicit `cwd`. Mirrors the JS
/// `resolveProjectRoot` (`claude-plugin/scripts/project-root.js`); keep the two
/// in lock-step (see `feedback_hook_class_bug_sweep`).
///
/// Order:
/// 1. cwd's OWN `.git` → cwd (a real project boundary: submodule, or a fresh
///    project with a `.git` but not yet an index — the metrics-isolation fixture).
/// 2. cwd's index wins UNLESS it is STRAY — an ancestor within the `.git`
///    boundary, below `$HOME`, is itself indexed (the monorepo-subdir relic an
///    older binary created and `priority-1` then pinned, so every tool read a
///    different DB per subdir).
/// 3. Otherwise the canonical project root: nearest INDEXED ancestor, else nearest
///    ancestor `.git`, else cwd.
///
/// The walk stops at `$HOME` (exclusive) so an unrelated `~/.code-graph` /
/// `~/.git` never poisons a project beneath it.
pub fn resolve_project_root_from(cwd: &Path) -> PathBuf {
    resolve_project_root_bounded(cwd, home_dir().as_deref())
}

/// `home`-injectable core so the `$HOME` boundary is unit-testable without
/// mutating the process environment (mirrors the JS resolver's `opts.home`).
fn resolve_project_root_bounded(cwd: &Path, home: Option<&Path>) -> PathBuf {
    // 1. cwd's own `.git` is always a boundary.
    if cwd.join(".git").exists() {
        return cwd.to_path_buf();
    }
    let cwd_has_index = cwd.join(CODE_GRAPH_DIR).join("index.db").exists();

    // Walk STRICT ancestors, stopping AT `$HOME` (exclusive) or the nearest
    // `.git` root. Track the nearest indexed ancestor (the canonical root of an
    // already-indexed project) and the nearest `.git` root within that bound.
    let mut nearest_indexed: Option<PathBuf> = None;
    let mut nearest_git: Option<PathBuf> = None;
    let mut cursor = cwd.parent();
    while let Some(c) = cursor {
        if home == Some(c) {
            break; // an index/.git at-or-above home is an unrelated outer project
        }
        if nearest_indexed.is_none() && c.join(CODE_GRAPH_DIR).join("index.db").exists() {
            nearest_indexed = Some(c.to_path_buf());
        }
        if c.join(".git").exists() {
            nearest_git = Some(c.to_path_buf());
            break;
        }
        cursor = c.parent();
    }

    // 2. cwd's index wins only when it is NOT stray (no indexed ancestor in bound).
    if cwd_has_index && nearest_indexed.is_none() {
        return cwd.to_path_buf();
    }
    // 3. Prefer the indexed ancestor (canonical project index), then a `.git`
    //    root, then cwd.
    if let Some(idx) = nearest_indexed {
        return idx;
    }
    if let Some(g) = nearest_git {
        return g;
    }
    cwd.to_path_buf()
}

/// Resolve the project root from the current working directory.
pub fn resolve_project_root() -> std::io::Result<PathBuf> {
    Ok(resolve_project_root_from(&std::env::current_dir()?))
}

/// Project-root markers — the literal set the JS activation gate uses
/// (`claude-plugin/scripts/project-detect.js` `PROJECT_MARKERS`). Both layers
/// must agree on "what is a real project"; kept in sync by hand.
pub const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
];

/// True when `cwd` carries none of the recognized project markers — e.g. `/tmp`
/// or Claude Code's `$TMPDIR`, where claude-mem-lite spawns headless `claude -p`
/// calls that never use code-graph. The MCP launcher gates the same way
/// (`mcp-launcher.js` → `isNonProjectCwd`); this is the Rust counterpart so the
/// binary self-protects even when invoked directly (bypassing the launcher).
///
/// Marker-based and cwd-only — deliberately NOT keyed on an existing
/// `.code-graph/index.db`: that file is created BY this tool, so counting it
/// would let a once-polluted dir self-certify as a project on the next run
/// (same rationale as `project-detect.js`).
pub fn is_non_project_cwd(cwd: &Path) -> bool {
    !PROJECT_MARKERS.iter().any(|m| cwd.join(m).exists())
}

/// Minimal JSON-RPC loop that answers `initialize` / `tools/list` with an empty
/// catalog and rejects everything else, WITHOUT opening a database, loading the
/// embedding model, or creating `.code-graph/`. Mirrors the JS launcher's
/// `serveEmptyMcpStub`. Driven by `run_serve` when `is_non_project_cwd` holds
/// and `CODE_GRAPH_FORCE_PLUGIN_MCP` is unset.
pub fn serve_non_project_stub<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
) -> std::io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = match req.get("method").and_then(|m| m.as_str()) {
            Some(m) => m,
            None => continue,
        };
        // JSON-RPC notifications (no `id`) get no response.
        let id = match req.get("id") {
            Some(id) => id.clone(),
            None => continue,
        };
        let response = match method {
            "initialize" => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "code-graph-mcp (non-project stub)",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
            "tools/list" => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [] } }),
            "resources/list" => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "resources": [] } }),
            "prompts/list" => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "prompts": [] } }),
            "ping" => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            _ => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": "method not found (non-project stub mode)" }
            }),
        };
        writeln!(writer, "{}", response)?;
        writer.flush()?;
    }
    Ok(())
}

/// Remove empty legacy database files left behind from past naming migrations.
/// Pre-v0.5 iterations briefly used `code-graph.db`, `code_graph.db`, `graph.db`
/// before settling on `index.db`; the renames never deleted the old 0-byte stubs.
pub fn cleanup_legacy_db_files(code_graph_dir: &Path) {
    const LEGACY: &[&str] = &["code-graph.db", "code_graph.db", "graph.db"];
    for name in LEGACY {
        let p = code_graph_dir.join(name);
        if let Ok(meta) = std::fs::metadata(&p) {
            if meta.is_file() && meta.len() == 0 {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

/// Lightweight CLI context for subcommands called by hooks.
/// Does NOT load the embedding model (too slow for 5-10s hook timeouts).
pub struct CliContext {
    pub db: Database,
    pub project_root: PathBuf,
}

impl CliContext {
    pub fn open(project_root: &Path) -> Result<Self> {
        let db_path = project_root.join(CODE_GRAPH_DIR).join("index.db");
        if !db_path.exists() {
            anyhow::bail!(
                "No index found at {}. Run: code-graph-mcp incremental-index",
                db_path.display()
            );
        }
        cleanup_legacy_db_files(&project_root.join(CODE_GRAPH_DIR));
        // CLI commands behind CliContext are READERS (grep, show, callgraph,
        // health-check, …). Open non-destructively so a status poll or one-off
        // query never triggers the INDEX_VERSION wipe — only an explicit indexer
        // (reindex / incremental-index / server startup) clears + rebuilds.
        let db = Database::open_nondestructive(&db_path)?;
        Ok(Self {
            db,
            project_root: project_root.to_path_buf(),
        })
    }

    /// Try to open, returning None if no index exists (for grep fallback).
    pub fn try_open(project_root: &Path) -> Option<Self> {
        let db_path = project_root.join(CODE_GRAPH_DIR).join("index.db");
        if !db_path.exists() {
            return None;
        }
        cleanup_legacy_db_files(&project_root.join(CODE_GRAPH_DIR));
        Database::open_nondestructive(&db_path).ok().map(|db| Self {
            db,
            project_root: project_root.to_path_buf(),
        })
    }
}

// --- Argument helpers ---

/// Normalize a user-provided path argument to a project-relative string.
///
/// - `"."` → `""` (whole project — matches MCP `module_overview` semantics)
/// - `"./foo"` → `"foo"`
/// - absolute path under `project_root` → relative portion (lexical first, canonical fallback for symlinks)
/// - absolute path outside `project_root` → error
/// - relative path that escapes the root via `..` → error
/// - other relative path → unchanged
///
/// Why: indexed `file_path` columns in SQLite are project-relative. When users
/// paste an absolute path from an IDE (very common), the CLI used to silently
/// return empty/wrong results (`overview` "No symbols found", `dead-code` exit-0
/// "No dead code found", `deps` bogus barrel-scan fallback). All three are
/// indistinguishable from real "no results" → user trusts the wrong answer.
/// A relative `..` escape is worse than wrong: the index holds only in-root
/// paths, so the path can only match the disk — `deps`' barrel-scan reads
/// `project_root.join(raw)`, turning `deps ../../secret.js` into a path-traversal
/// file read that leaks the file's import/re-export lines. Reject the escape.
fn normalize_user_path(project_root: &Path, raw: &str) -> Result<String> {
    if raw == "." {
        return Ok(String::new());
    }
    if let Some(rest) = raw.strip_prefix("./") {
        return Ok(rest.to_string());
    }
    let p = Path::new(raw);
    if !p.is_absolute() {
        // Resolve `..`/`.` lexically (no filesystem touch — the target may be
        // gitignored or already deleted) and reject any prefix that climbs above
        // the root. `depth` is the component count relative to the root; it going
        // negative at any point means the path escaped.
        let mut depth: i32 = 0;
        for comp in p.components() {
            match comp {
                std::path::Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        anyhow::bail!(
                            "path '{}' escapes the project root '{}' \u{2014} use a path inside the project",
                            raw, project_root.display()
                        );
                    }
                }
                std::path::Component::Normal(_) => depth += 1,
                // CurDir (`.`) and any RootDir/Prefix don't climb above the root.
                _ => {}
            }
        }
        return Ok(raw.to_string());
    }
    if let Ok(rel) = p.strip_prefix(project_root) {
        return Ok(rel.to_string_lossy().into_owned());
    }
    if let (Ok(canon_p), Ok(canon_root)) = (p.canonicalize(), project_root.canonicalize()) {
        if let Ok(rel) = canon_p.strip_prefix(&canon_root) {
            return Ok(rel.to_string_lossy().into_owned());
        }
    }
    anyhow::bail!(
        "path '{}' is outside the project root '{}' \u{2014} use a relative path or one under the project root",
        raw, project_root.display()
    );
}

/// Strip qualified name prefix (e.g. "McpServer.handle_message" -> "handle_message")
/// so users can copy-paste names from output and use them in lookups.
fn strip_qualified_prefix(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// CLI-side fuzzy name resolution. Mirrors MCP server's `resolve_fuzzy_name` so
/// CLI `callgraph`/`refs` auto-promote a unique fuzzy match to the exact name
/// instead of just printing "Did you mean" and bailing out.
pub(crate) enum CliFuzzyResolution {
    Unique(String),
    Ambiguous(Vec<queries::NameCandidate>),
    NotFound,
}

fn resolve_fuzzy_name_cli(conn: &rusqlite::Connection, name: &str) -> Result<CliFuzzyResolution> {
    let candidates: Vec<_> = queries::find_functions_by_fuzzy_name(conn, name)?
        .into_iter()
        .filter(|c| !crate::domain::is_test_symbol(&c.name, &c.file_path))
        .collect();
    let exact: Vec<_> = candidates.iter().filter(|c| c.name == name).cloned().collect();
    if exact.len() == 1 {
        return Ok(CliFuzzyResolution::Unique(exact[0].name.clone()));
    }
    if exact.len() > 1 {
        return Ok(CliFuzzyResolution::Ambiguous(exact));
    }
    if candidates.len() == 1 {
        return Ok(CliFuzzyResolution::Unique(candidates.into_iter().next().unwrap().name));
    }
    if !candidates.is_empty() {
        return Ok(CliFuzzyResolution::Ambiguous(candidates));
    }
    Ok(CliFuzzyResolution::NotFound)
}

/// Emit the "ambiguous symbol" error in the same shape whether the command was
/// invoked with --json (one-line JSON) or default (human-readable stderr lines),
/// then exit(1). Shared by cmd_callgraph, cmd_impact when no file filter was
/// given and `crate::resolve::detect_ambiguity` returned candidates. The message
/// and JSON suggestion shape come from `crate::resolve` so the CLI and MCP give
/// identical verdicts on same-file overloads (audit 2026-06-03 #6).
fn emit_exact_ambiguity(symbol: &str, cands: &[queries::NameCandidate], json_mode: bool) -> ! {
    let message = crate::resolve::ambiguity_message(symbol, cands, crate::resolve::Surface::Cli);
    if json_mode {
        let sugg: Vec<serde_json::Value> =
            crate::resolve::candidates_to_json(cands).into_iter().take(5).collect();
        println!("{}", serde_json::json!({
            "error": message,
            "suggestions": sugg,
        }));
    } else {
        eprintln!("[code-graph] {}", message);
        for c in cands.iter().take(5) {
            eprintln!("  {} ({}) in {} [node_id {}]", c.name, c.node_type, c.file_path, c.node_id);
        }
    }
    std::process::exit(1);
}

/// Resolve a possibly-qualified symbol name (e.g. "Database.open") to a base name
/// and optional file path for disambiguation. When the user passes a qualified name,
/// we find the matching node and use its file_path as a filter so that downstream
/// queries (callgraph, impact, refs) pick the right symbol.
/// Returns (base_name, resolved_file_filter) where resolved_file_filter is Some only
/// if the qualified name resolved uniquely and no explicit --file was given.
fn resolve_qualified_symbol<'a>(
    conn: &rusqlite::Connection,
    raw_symbol: &'a str,
    explicit_file: Option<&'a str>,
) -> (&'a str, Option<String>) {
    // If user already provided --file, just strip the prefix and use their filter
    if explicit_file.is_some() {
        return (strip_qualified_prefix(raw_symbol), None);
    }
    // If the symbol contains '.', try qualified name resolution
    if raw_symbol.contains('.') {
        let base = strip_qualified_prefix(raw_symbol);
        if let Ok(nodes) = queries::get_nodes_by_name(conn, base) {
            let matched: Vec<_> = nodes
                .iter()
                .filter(|n| n.qualified_name.as_deref() == Some(raw_symbol))
                .collect();
            if matched.len() == 1 {
                if let Ok(Some(fp)) = queries::get_file_path(conn, matched[0].file_id) {
                    return (base, Some(fp));
                }
            }
        }
        return (base, None);
    }
    (raw_symbol, None)
}

// --- Output formatting ---

/// Format a node as a compact single line: `type QualifiedName  file:start-end  (params) -> return`
fn format_node_compact(node: &queries::NodeResult, file_path: &str) -> String {
    let mut out = String::with_capacity(128);
    // type prefix
    let short_type = match node.node_type.as_str() {
        "function" => "fn",
        "method" => "fn",
        "class" => "class",
        "struct" => "struct",
        "interface" => "iface",
        "trait" => "trait",
        "enum" => "enum",
        "type_alias" => "type",
        "constant" => "const",
        "variable" => "var",
        other => other,
    };
    out.push_str(short_type);
    out.push(' ');

    // name (prefer qualified)
    if let Some(ref qn) = node.qualified_name {
        out.push_str(qn);
    } else {
        out.push_str(&node.name);
    }

    // location
    out.push_str("  ");
    out.push_str(file_path);
    out.push(':');
    out.push_str(&node.start_line.to_string());
    out.push('-');
    out.push_str(&node.end_line.to_string());

    // signature parts
    if let Some(ref params) = node.param_types {
        if !params.is_empty() {
            out.push_str("  (");
            out.push_str(params);
            out.push(')');
        }
    }
    if let Some(ref ret) = node.return_type {
        if !ret.is_empty() {
            out.push_str(" -> ");
            out.push_str(ret);
        }
    }
    out
}

// --- Subcommands ---

// Internal notes — `//` (not `///`) so clap leaves them out of `--help`: only flag
// parsing lives in this struct; the git/index existence guard stays in main() — it
// must precede any resolve_project_root indexing side effect and may skip the run
// entirely (issue #8). The handler keeps its `quiet: bool` signature so the internal
// reindex/rebuild-index callers are unaffected.
/// CLI arguments for the `incremental-index` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp incremental-index",
          about = "Run incremental index update (full index when none exists)")]
pub struct IncrementalIndexArgs {
    /// Suppress progress output (used by the PostToolUse hook)
    #[arg(long)]
    pub quiet: bool,
    /// Index structure only (nodes/edges/FTS) and skip embeddings for a fast,
    /// query-ready index. Vectors backfill later (MCP server / a later run).
    #[arg(long)]
    pub no_embed: bool,
}

/// Run incremental index update.
/// If `quiet` is true, suppress non-error output.
/// Auto-creates the database and runs a full index if no index exists.
/// Map SQLITE_BUSY ("database is locked", error code 5) to an actionable hint —
/// surfaces when two indexers / an MCP server race on the same index.db. Shared
/// by the full / incremental / embed paths.
fn wrap_index_busy<T>(r: Result<T>) -> Result<T> {
    r.map_err(|e| {
        let msg = format!("{:#}", e);
        if msg.contains("database is locked") || msg.contains("Error code 5") {
            anyhow::anyhow!(
                "Another `code-graph-mcp` process is writing to .code-graph/index.db \
                 (an indexer or MCP server). Wait for it to finish, then retry. \
                 Original error: {}",
                e
            )
        } else {
            e
        }
    })
}

/// Embed any nodes still missing vectors (synchronous, unlike the server's
/// background thread). No-op without the `embed-model` feature or when the model
/// can't load. Shared by the full / incremental / rebuild paths so embedding
/// behaviour can't drift between them.
fn embed_missing_nodes(db: &Database, quiet: bool) -> Result<()> {
    if !db.vec_enabled() {
        return Ok(());
    }
    use crate::embedding::model::EmbeddingModel;
    use crate::indexer::pipeline::embed_and_store_batch;
    if let Some(model) = EmbeddingModel::load()? {
        let mut total = 0usize;
        // Skip nodes that fail to embed this run. This loop only stops on an empty
        // result, so without excluding failures a single deterministically-un-embeddable
        // node (which stays `node_vectors IS NULL` and sorts first by caller-count) would
        // be re-fetched at the head of every batch and spin the loop forever.
        let mut failed: std::collections::HashSet<i64> = std::collections::HashSet::new();
        loop {
            let exclude: Vec<i64> = failed.iter().copied().collect();
            let chunk = wrap_index_busy(queries::get_unembedded_nodes_excluding(db.conn(), 64, &exclude))?;
            if chunk.is_empty() { break; }
            let chunk_len = chunk.len();
            let embedded_ids = wrap_index_busy(embed_and_store_batch(db, &model, &chunk))?;
            total += embedded_ids.len();
            if embedded_ids.len() < chunk_len {
                let ok: std::collections::HashSet<i64> = embedded_ids.into_iter().collect();
                for (id, _) in &chunk {
                    if !ok.contains(id) { failed.insert(*id); }
                }
            }
        }
        if total > 0 && !quiet {
            let (embedded, embeddable) = queries::count_nodes_with_vectors(db.conn())?;
            eprintln!("Embedded {} nodes ({}/{})", total, embedded, embeddable);
        }
        if !failed.is_empty() && !quiet {
            eprintln!("{} node(s) could not be embedded (skipped)", failed.len());
        }
    }
    Ok(())
}

/// Build a fresh FULL index into an explicit `db_path` and embed it. The DB is
/// opened and dropped within this call, so on return the WAL is checkpointed and
/// `db_path` is self-contained — which lets `rebuild-index` build into a temp
/// file and atomically rename it over `index.db`.
fn build_full_index_at(db_path: &Path, project_root: &Path, quiet: bool, no_embed: bool) -> Result<()> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
        cleanup_legacy_db_files(parent);
    }
    // Open with vec support so embeddings can be stored.
    let db = Database::open_with_vec(db_path)?;
    use crate::indexer::pipeline::run_full_index;
    let result = wrap_index_busy(run_full_index(&db, project_root, None, None))?;
    if !quiet {
        eprintln!(
            "Full index: {} files, {} nodes, {} edges",
            result.files_indexed, result.nodes_created, result.edges_created
        );
    }
    finish_embedding(&db, quiet, no_embed)?;
    Ok(())
}

/// Shared structure-first → embedding handoff for the CLI index commands.
///
/// The structural graph (nodes/edges/FTS) is already committed and usable for
/// AST / grep / callgraph queries the moment indexing returns — embedding is a
/// separate, slow (CPU-bound) pass that only powers semantic/vector search. On a
/// large repo it dominates wall-clock (≈5 nodes/s), so a foreground `reindex`
/// could block for many minutes after the graph was already query-ready.
///
/// `--no-embed` skips it: the caller gets the fast structural index and the
/// vectors backfill later (the MCP server's background embedder fills any node
/// lacking a vector, resumably; or rerun without the flag to embed now).
fn finish_embedding(db: &Database, quiet: bool, no_embed: bool) -> Result<()> {
    if no_embed {
        if !quiet && db.vec_enabled() {
            let (embedded, embeddable) = queries::count_nodes_with_vectors(db.conn())
                .unwrap_or((0, 0));
            eprintln!(
                "Structure index ready (AST/grep/callgraph usable now). Skipping embeddings \
                 (--no-embed): {}/{} nodes have vectors; the rest backfill in the background \
                 or via `code-graph-mcp incremental-index`.",
                embedded, embeddable
            );
        }
        return Ok(());
    }
    embed_missing_nodes(db, quiet)
}

pub fn cmd_incremental_index(project_root: &Path, quiet: bool, no_embed: bool) -> Result<()> {
    let db_path = project_root.join(CODE_GRAPH_DIR).join("index.db");

    // No existing DB → full index. Delegate to build_full_index_at so the
    // full-index + embed path is shared with rebuild-index (no drift).
    if !db_path.exists() {
        if !quiet {
            eprintln!("No index found, creating full index...");
        }
        return build_full_index_at(&db_path, project_root, quiet, no_embed);
    }

    cleanup_legacy_db_files(&project_root.join(CODE_GRAPH_DIR));

    // Open with vec support so embeddings can be stored
    let db = Database::open_with_vec(&db_path)?;

    // Incremental index for the existing database.
    use crate::indexer::pipeline::run_incremental_index;
    let stats = wrap_index_busy(run_incremental_index(&db, project_root, None, None))?;
    if !quiet {
        if stats.files_deleted > 0 {
            eprintln!(
                "Incremental index: {} files updated, {} files removed, {} nodes created",
                stats.files_indexed, stats.files_deleted, stats.nodes_created
            );
        } else {
            eprintln!(
                "Incremental index: {} files updated, {} nodes created",
                stats.files_indexed, stats.nodes_created
            );
        }
    }

    finish_embedding(&db, quiet, no_embed)?;
    Ok(())
}

/// SQLite sidecar path: `<db>-wal` / `<db>-shm`. Appends the literal suffix to
/// the FULL filename (not an extension swap) — required for temp db names like
/// `index.db.rebuild-<pid>`, whose WAL is `index.db.rebuild-<pid>-wal`.
fn db_sidecar(db_path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push(suffix);
    std::path::PathBuf::from(s)
}

/// Drop the existing index.db (plus WAL/SHM) and trigger a full rebuild via
/// `cmd_incremental_index` (which auto-detects the missing DB and does a full
/// index). Mirrors MCP `rebuild_index` tool semantics.
/// `rebuild-index` arguments (clap-migrated, audit #4).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp rebuild-index",
          about = "Drop and rebuild the index from scratch (requires --confirm)")]
pub struct RebuildIndexArgs {
    /// Confirm the destructive drop-and-rebuild (required to proceed)
    #[arg(long)]
    pub confirm: bool,
    /// Suppress progress output
    #[arg(long)]
    pub quiet: bool,
    /// Index structure only and skip embeddings (vectors backfill later).
    #[arg(long)]
    pub no_embed: bool,
}

pub fn cmd_rebuild_index(project_root: &Path, args: RebuildIndexArgs) -> Result<()> {
    let confirm = args.confirm;
    let quiet = args.quiet;
    let no_embed = args.no_embed;
    // `--confirm` is a business-logic confirmation gate, NOT a clap-required arg:
    // a missing confirm is a deliberate exit-1 anyhow bail (not a parse error),
    // preserving the prior contract (test_cli_rebuild_index_requires_confirm).
    if !confirm {
        anyhow::bail!(
            "rebuild-index drops the existing index and re-parses every file. \
             Pass --confirm to proceed. Use `incremental-index` for incremental updates."
        );
    }
    // Destructive-op sanity: refuse to operate on degenerate roots. Guards against
    // a resolve_project_root regression that could return `/` or `""`.
    if project_root.as_os_str().is_empty() || project_root == Path::new("/") {
        anyhow::bail!(
            "refusing to rebuild-index with degenerate project_root ({}). \
             Run from within a git-tracked project directory.",
            project_root.display()
        );
    }
    let code_graph_dir = project_root.join(CODE_GRAPH_DIR);
    let db_path = code_graph_dir.join("index.db");

    // Atomic rebuild: build the fresh index into a temp file in the SAME dir,
    // then rename it over index.db in one syscall. Concurrent readers (a second
    // CLI invocation, or the MCP server reopening) therefore always see a
    // COMPLETE index — the old one until the rename, the new one after — instead
    // of the empty/partial window the old "remove index.db then rebuild in place"
    // left open for the entire (multi-second on large repos) rebuild.
    let temp_path = code_graph_dir.join(format!("index.db.rebuild-{}", std::process::id()));
    let temp_files = [
        temp_path.clone(),
        db_sidecar(&temp_path, "-wal"),
        db_sidecar(&temp_path, "-shm"),
    ];
    let remove_all = |paths: &[std::path::PathBuf]| {
        for p in paths {
            if p.exists() { let _ = std::fs::remove_file(p); }
        }
    };
    // Clear leftover temp files from previously-killed rebuilds (ANY pid). The
    // `index.db.rebuild-<pid>` prefix also matches their `-wal`/`-shm` sidecars.
    // A concurrent rebuild's in-progress temp could be swept too — that only
    // makes the other run's final rename fail (an error, never corruption);
    // concurrent rebuild-index runs were never supported.
    if let Ok(entries) = std::fs::read_dir(&code_graph_dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("index.db.rebuild-") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    // Build into the temp file. On failure, drop the temp and keep the existing
    // index.db intact — the rename below is the only mutation of the live index,
    // so a failed rebuild no longer leaves the user with NO index (the old
    // remove-first path did).
    if let Err(e) = build_full_index_at(&temp_path, project_root, quiet, no_embed) {
        remove_all(&temp_files);
        return Err(e);
    }
    // The temp DB closed cleanly inside build_full_index_at (WAL checkpointed);
    // remove any residual temp -wal/-shm so the renamed file is self-contained.
    remove_all(&temp_files[1..]);

    // Drop the OLD index's -wal/-shm BEFORE the rename: afterwards a stale
    // index.db-wal would be (wrongly) replayed by SQLite onto the NEW index.db.
    // The old WAL is discardable here — we're replacing the whole index. A reader
    // in the sub-millisecond gap sees the old index.db (a valid, complete file).
    remove_all(&[db_sidecar(&db_path, "-wal"), db_sidecar(&db_path, "-shm")]);

    // Atomic swap (temp and index.db share .code-graph/ → POSIX rename is atomic).
    std::fs::rename(&temp_path, &db_path)?;
    Ok(())
}

// Internal notes — `//` (not `///`) so clap leaves them out of `--help`: --json and
// --format coexist for back-compat (--json is shorthand for `--format json` and wins
// when both are given); resolved_format() below collapses them into the single `&str`
// the handler consumes, so cmd_health_check's signature and its JSON/oneline branches
// stay untouched (plan §2 item 14).
/// CLI arguments for the `health-check` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp health-check",
          about = "Query index status (nodes/edges/files, freshness, embedding coverage)")]
pub struct HealthCheckArgs {
    /// JSON output (shorthand for --format json; wins when both are set)
    #[arg(long)]
    pub json: bool,
    /// Output format: oneline (default) or json
    #[arg(long)]
    pub format: Option<String>,
}

impl HealthCheckArgs {
    /// Collapse `--json`/`--format` into the handler's format string.
    /// `--json` takes precedence; absent both, defaults to "oneline".
    /// Unrecognized `--format` values fall through to the handler's oneline branch
    /// (preserved from the prior hand-parser: only "json" was special-cased).
    pub fn resolved_format(&self) -> &str {
        if self.json {
            "json"
        } else {
            self.format.as_deref().unwrap_or("oneline")
        }
    }
}

/// Recording-side state of the recommend→use conversion metric, surfaced by
/// `stats` and `health-check` so a dark metric is a visible signal rather than
/// silence. `"absent"` = `recommendations.jsonl` missing (the PreToolUse hooks
/// that record recommendations are not active in this project — e.g. it runs a
/// dev `.mcp.json` server with the marketplace plugin disabled, so the metric is
/// structurally dark); `"empty"` = file present, no recommendations yet;
/// `"live"` = recommendations recorded.
pub fn recommendation_metric_state(project_root: &Path) -> &'static str {
    let p = project_root.join(CODE_GRAPH_DIR).join("recommendations.jsonl");
    match std::fs::read_to_string(&p) {
        Err(_) => "absent",
        Ok(c) => {
            if aggregate_recommendations_jsonl(&c).total > 0 { "live" } else { "empty" }
        }
    }
}

/// Run health check and print status, including index freshness.
pub fn cmd_health_check(project_root: &Path, format: &str) -> Result<()> {
    // JSON callers (doctor.js, scripts, MCP UIs) need a parseable response
    // even when the index is missing — bailing with a stderr-only anyhow error
    // forces them to grep messages instead of reading JSON fields.
    if format == "json" {
        let db_path = project_root.join(CODE_GRAPH_DIR).join("index.db");
        if !db_path.exists() {
            let payload = serde_json::json!({
                "healthy": false,
                "reason": "no_index",
                "issue": format!("No index found at {}. Run: code-graph-mcp incremental-index", db_path.display()),
                "nodes": 0,
                "edges": 0,
                "files": 0,
                "watching": false,
                "db_size_bytes": 0,
                "search_mode": "fts_only",
                "embedding_progress": "0/0",
                "embedding_coverage_pct": 0,
                "embedding_status": "unavailable",
                "model_available": cfg!(feature = "embed-model"),
                "snapshot": {"status": "absent", "source_url": null, "source_commit": null, "fetched_at": null, "commit_drift": null},
            });
            println!("{}", serde_json::to_string(&payload)?);
            return Ok(());
        }
    }
    let ctx = CliContext::open(project_root)?;
    // The reader open above is non-destructive: if the on-disk index was built by
    // an older INDEX_VERSION, the data is intact but a rebuild is owed. Report it
    // rather than (as before) silently wiping it on this status poll.
    let index_version_stale = ctx.db.index_version_stale();
    let conn = ctx.db.conn();
    let status = queries::get_index_status(conn, false)?;

    let expected_schema = crate::storage::schema::SCHEMA_VERSION;
    let schema_ok = status.schema_version == expected_schema;
    let has_data = status.nodes_count > 0 && status.files_count > 0;
    let healthy = schema_ok && has_data;

    // Compute index age from last_indexed_at (unix timestamp in seconds)
    let age_str = status.last_indexed_at.map(|ts| {
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 - ts)
            .unwrap_or(0);
        if elapsed < 60 { format!("{}s ago", elapsed) }
        else if elapsed < 3600 { format!("{}m ago", elapsed / 60) }
        else if elapsed < 86400 { format!("{}h ago", elapsed / 3600) }
        else { format!("{}d ago", elapsed / 86400) }
    });

    // Embedding coverage (works without sqlite-vec loaded)
    let (vectors_done, vectors_total) = queries::count_nodes_with_vectors(conn).unwrap_or((0, 0));
    let coverage_pct: i64 = if vectors_total > 0 {
        (vectors_done as f64 / vectors_total as f64 * 100.0).round() as i64
    } else {
        0
    };
    // Embedding model availability: compile-time feature flag proxy (runtime-cheap,
    // avoids loading weights which would violate CLI's hook-fast contract).
    // NOTE: This diverges from MCP `get_index_status` (which checks runtime
    // `embedding_model.is_some()` — true only after weights load). CLI reports
    // `model_available=true` whenever the binary was built with --features
    // embed-model, even if model weights are missing locally. Cross-check
    // `embedding_progress`/`embedding_status` to tell apart "compiled but not
    // loaded yet" from "compiled and embedding in progress".
    let model_available: bool = cfg!(feature = "embed-model");
    let search_mode = if model_available && vectors_done > 0 { "hybrid" } else { "fts_only" };
    let embedding_status = if !model_available {
        "unavailable"
    } else if vectors_done == 0 {
        "pending"
    } else if vectors_done >= vectors_total && vectors_total > 0 {
        "complete"
    } else {
        "partial"
    };

    // Snapshot metadata block — reads keys written by `snapshot install`.
    let snapshot_url = crate::snapshot::meta::read_meta(conn, crate::snapshot::meta::META_SNAPSHOT_SOURCE_URL)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let snapshot_commit = crate::snapshot::meta::read_meta(conn, crate::snapshot::meta::META_SNAPSHOT_SOURCE_COMMIT)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let snapshot_fetched_at = crate::snapshot::meta::read_meta(conn, crate::snapshot::meta::META_SNAPSHOT_FETCHED_AT)
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok());
    let snapshot_status = if snapshot_url.is_some() { "present" } else { "absent" };
    // commit_drift: how many local commits landed after the snapshot was taken.
    let commit_drift = snapshot_commit.as_deref().and_then(|c| {
        std::process::Command::new("git")
            .args(["rev-list", "--count", &format!("{c}..HEAD")])
            .current_dir(project_root)
            .output()
            .ok()
            .and_then(|o| if o.status.success() {
                String::from_utf8_lossy(&o.stdout).trim().parse::<i64>().ok()
            } else {
                None
            })
    });
    let snapshot_block = serde_json::json!({
        "status": snapshot_status,
        "source_url": snapshot_url,
        "source_commit": snapshot_commit,
        "fetched_at": snapshot_fetched_at,
        "commit_drift": commit_drift,
    });

    // Graph-resolution coverage (pending backlog + per-language edge counts).
    // .ok() so a stats failure never breaks the existing health-check contract.
    let resolution = queries::resolution_stats(conn).ok();

    match format {
        "json" => {
            let mut json = serde_json::json!({
                "healthy": healthy,
                "nodes": status.nodes_count,
                "edges": status.edges_count,
                "files": status.files_count,
                "watching": false,
                "schema_version": status.schema_version,
                "db_size_bytes": status.db_size_bytes,
                "search_mode": search_mode,
                "embedding_progress": format!("{}/{}", vectors_done, vectors_total),
                "embedding_coverage_pct": coverage_pct,
                "embedding_status": embedding_status,
                "model_available": model_available,
                "snapshot": snapshot_block,
                "conversion_metric": recommendation_metric_state(project_root),
                "index_version_stale": index_version_stale.is_some(),
            });
            if let Some(ref r) = resolution {
                json["resolution"] = serde_json::to_value(r).unwrap_or(serde_json::Value::Null);
            }
            if let Some(ts) = status.last_indexed_at {
                json["last_indexed_at"] = serde_json::json!(ts);
            }
            if let Some(ref age) = age_str {
                json["index_age"] = serde_json::json!(age);
            }
            if !schema_ok {
                json["issue"] = serde_json::json!(format!(
                    "schema version mismatch: got {}, expected {}",
                    status.schema_version, expected_schema
                ));
            } else if !has_data {
                json["issue"] = serde_json::json!("index is empty");
            } else if let Some(old) = index_version_stale {
                // Has data + correct schema, but built by an older extractor
                // generation. Usable now (FTS/AST), but results sharpen after a
                // rebuild — which an indexer (reindex / incremental-index / server
                // startup), not this poll, performs.
                json["issue"] = serde_json::json!(format!(
                    "index built by older version (v{} ≠ v{}); rebuild pending",
                    old, crate::domain::INDEX_VERSION
                ));
            }
            println!("{}", json);
            if !healthy {
                std::process::exit(1);
            }
        }
        _ => {
            // Print resolution coverage regardless of healthy, mirroring the JSON arm
            // which attaches the block unconditionally (F12). Healthy keeps `OK:` first.
            let print_resolution = || {
                if let Some(ref r) = resolution {
                    let summary: Vec<String> = r.edges_by_language.iter()
                        .map(|(lang, rels)| format!("{} {}", lang, rels.values().sum::<i64>()))
                        .collect();
                    println!("Resolution: {} pending; edges by lang: {}",
                        r.pending_unresolved_calls, summary.join(", "));
                }
            };
            if healthy {
                let age_info = age_str.map(|a| format!(" (updated {})", a)).unwrap_or_default();
                println!(
                    "OK: {} nodes, {} edges, {} files{}",
                    status.nodes_count, status.edges_count, status.files_count, age_info
                );
                println!("Snapshot: {}", snapshot_status);
                println!("Conversion metric: {}", match recommendation_metric_state(project_root) {
                    "live" => "live (recommendations recorded)",
                    "empty" => "active, no recommendations recorded yet",
                    _ => "DARK (no recommendations.jsonl — PreToolUse hooks not recording here)",
                });
                // Vector/embedding status — make a silent FTS5-only degradation visible
                // (the prior gap: text health-check never surfaced search_mode, so a user
                // whose model download failed had no way to see vector was inactive).
                println!("Search: {} — {}% embedded ({})",
                    if search_mode == "hybrid" { "hybrid (FTS5 + vector)" } else { "FTS5-only (vector inactive)" },
                    coverage_pct,
                    match embedding_status {
                        "unavailable" => "binary built without embed-model feature",
                        "pending" => "model not loaded yet; auto-downloads in background on first search — retry shortly, then re-check",
                        "partial" => "embedding in progress",
                        "complete" => "embeddings complete",
                        other => other,
                    });
                print_resolution();
            } else if !schema_ok {
                eprintln!(
                    "UNHEALTHY: schema version mismatch (got {}, expected {})",
                    status.schema_version, expected_schema
                );
                print_resolution();
                std::process::exit(1);
            } else {
                eprintln!("UNHEALTHY: index is empty");
                print_resolution();
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

/// Canonical name for a CLI *query* subcommand (incl. MCP-name aliases), or
/// None for housekeeping (serve/index/stats/doctor/...). Drives `record_cli_use`:
/// only code-understanding queries count as funnel conversions.
pub fn canonical_query_cmd(sub: &str) -> Option<&'static str> {
    Some(match sub {
        "grep" => "grep",
        "search" | "semantic_code_search" => "search",
        "ast-search" | "ast_search" => "ast-search",
        "callgraph" | "get_call_graph" => "callgraph",
        "impact" | "impact_analysis" => "impact",
        "affected" => "affected",
        "tour" => "tour",
        "map" | "project_map" => "map",
        "overview" | "module_overview" => "overview",
        "show" | "get_ast_node" => "show",
        "trace" | "trace_http_chain" => "trace",
        "deps" | "dependency_graph" => "deps",
        "similar" | "find_similar_code" => "similar",
        "refs" | "find_references" => "refs",
        "dead-code" | "find_dead_code" => "dead-code",
        "centrality" => "centrality",
        "file-impact" => "file-impact",
        _ => return None,
    })
}

/// Append a `{hook:"cli",action:"use",cmd}` line to recommendations.jsonl so the
/// deny→use funnel can see model-initiated CLI conversions (the 2026-06-12 daagu
/// night: 3 post-deny CLI calls, all invisible to the funnel). Mirrors the JS
/// recordRecommendation posture: best-effort, NEVER creates `.code-graph/`
/// (zero footprint outside indexed projects). Hook-internal answer runs set
/// `CODE_GRAPH_INTERNAL=1` and are skipped — they are deliveries, not conversions.
pub fn record_cli_use(project_root: &Path, cmd: &str) {
    if std::env::var("CODE_GRAPH_INTERNAL").ok().as_deref() == Some("1") {
        return;
    }
    let dir = project_root.join(CODE_GRAPH_DIR);
    if !dir.is_dir() {
        return;
    }
    // Opt-in per-project metrics silence. A `.code-graph/.no-metrics` sentinel marks
    // a development/dogfood checkout where the tool's OWN CLI is run for functionality
    // testing, sims, or ad-hoc dev — those runs would otherwise append `use` events
    // to the project's own recommendations.jsonl and read back as genuine consumer
    // adoption (the 2026-06-23 self-pollution: 184 burst rows from in-repo CLI runs).
    // Guards ONLY this recommendations-log write; MCP usage.jsonl (flush_metrics) is
    // untouched, so a dev repo's real MCP tool metrics still flow. Mirrored in JS
    // recommendation-log.js. Reversible: delete the file to re-enable.
    if dir.join(NO_METRICS_SENTINEL).exists() {
        return;
    }
    let line = serde_json::json!({
        "ts": crate::mcp::metrics::iso8601_now(),
        "hook": "cli",
        "action": "use",
        "cmd": cmd,
    });
    let rec_path = dir.join("recommendations.jsonl");
    // Bounded growth: recommendations.jsonl is append-only and (unlike
    // usage.jsonl) written per-event from both here and the JS PreToolUse hooks,
    // so rotate before appending. Same policy/constants as usage.jsonl.
    crate::mcp::metrics::rotate_jsonl_if_over(
        &rec_path,
        crate::mcp::metrics::JSONL_ROTATE_MAX_BYTES,
        crate::mcp::metrics::JSONL_ROTATE_KEEP_BYTES,
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rec_path)
    {
        use std::io::Write as _;
        let _ = writeln!(f, "{}", line);
    }
}

/// Aggregated per-tool counts across sessions.
pub struct ToolAgg {
    pub n: u64,
    pub total_ms: u64,
    pub err: u64,
    pub max_ms: u64,
}

/// Summary produced by `aggregate_usage_jsonl` — drives both human + JSON output.
pub struct UsageSummary {
    pub sessions: u64,
    pub parse_errors: u64,
    pub tools: HashMap<String, ToolAgg>,
    pub search_queries: u64,
    pub search_zero: u64,
    pub search_quality_weighted_sum: f64,
    pub search_fts_only: u64,
    pub search_hybrid: u64,
    pub full_index_count: u64,
    pub full_index_ms_sum: u64,
    pub incr_count: u64,
    pub files_indexed: u64,
    pub versions: std::collections::BTreeSet<String>,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
    /// Recommend→use funnel (per-session, window-joined from `recs` field).
    pub sessions_with_deny: u64,
    pub sessions_with_deny_and_cg: u64,
    pub sessions_with_hint: u64,
    pub sessions_with_hint_and_cg: u64,
    /// CLI-conversion legs (recs.cli_use > 0 in the session window) and the
    /// combined "any use" legs (MCP cg tool OR CLI query) — the honest funnel
    /// numerator now that deny→CLI is the proven conversion path.
    pub sessions_with_deny_and_cli: u64,
    pub sessions_with_hint_and_cli: u64,
    pub sessions_with_deny_and_use: u64,
    pub sessions_with_hint_and_use: u64,
}

impl UsageSummary {
    pub fn total_tool_calls(&self) -> u64 {
        self.tools.values().map(|a| a.n).sum()
    }
}

/// Code-understanding cg tools the DENY hook steers grep toward. Housekeeping
/// tools (start/stop_watch, get_index_status, rebuild_index) are excluded so the
/// funnel measures real "used cg instead of grep" substitution, not background
/// bookkeeping. Kept in sync by hand with the `src/mcp/tools.rs` registry.
const CG_QUERY_TOOLS: &[&str] = &[
    "get_call_graph", "get_ast_node", "module_overview", "semantic_code_search",
    "ast_search", "find_references", "project_map", "impact_analysis",
    "trace_http_chain", "dependency_graph", "find_similar_code", "find_dead_code",
    "find_http_route", "read_snippet",
];

/// Per-session funnel conversion = `num/denom` rounded to 2 decimals, or JSON
/// `null` when the bucket is empty (avoids a misleading 0.0 for "no data").
fn session_conversion(num: u64, denom: u64) -> serde_json::Value {
    if denom == 0 {
        serde_json::Value::Null
    } else {
        serde_json::json!((num as f64 / denom as f64 * 100.0).round() / 100.0)
    }
}

/// Parse and aggregate `.code-graph/usage.jsonl` content.
/// Pure function: no IO, no panics — malformed lines are counted, not fatal.
/// `last_n`: if Some, keep only the last N records before aggregating.
pub fn aggregate_usage_jsonl(content: &str, last_n: Option<usize>) -> UsageSummary {
    let mut records: Vec<serde_json::Value> = Vec::new();
    let mut parse_errors: u64 = 0;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) => records.push(v),
            Err(_) => parse_errors += 1,
        }
    }
    if let Some(n) = last_n {
        if records.len() > n {
            let drop = records.len() - n;
            records.drain(..drop);
        }
    }

    let mut summary = UsageSummary {
        sessions: records.len() as u64,
        parse_errors,
        tools: HashMap::new(),
        search_queries: 0,
        search_zero: 0,
        search_quality_weighted_sum: 0.0,
        search_fts_only: 0,
        search_hybrid: 0,
        full_index_count: 0,
        full_index_ms_sum: 0,
        incr_count: 0,
        files_indexed: 0,
        versions: std::collections::BTreeSet::new(),
        first_ts: None,
        last_ts: None,
        sessions_with_deny: 0,
        sessions_with_deny_and_cg: 0,
        sessions_with_hint: 0,
        sessions_with_hint_and_cg: 0,
        sessions_with_deny_and_cli: 0,
        sessions_with_hint_and_cli: 0,
        sessions_with_deny_and_use: 0,
        sessions_with_hint_and_use: 0,
    };

    for rec in &records {
        if let Some(v) = rec.get("v").and_then(|v| v.as_str()) {
            summary.versions.insert(v.to_string());
        }
        if let Some(ts) = rec.get("ts").and_then(|v| v.as_str()) {
            if summary.first_ts.is_none() { summary.first_ts = Some(ts.to_string()); }
            summary.last_ts = Some(ts.to_string());
        }
        if let Some(tools_obj) = rec.get("tools").and_then(|v| v.as_object()) {
            for (name, s) in tools_obj {
                let agg = summary.tools.entry(name.clone()).or_insert(ToolAgg {
                    n: 0, total_ms: 0, err: 0, max_ms: 0,
                });
                agg.n += s.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                agg.total_ms += s.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
                agg.err += s.get("err").and_then(|v| v.as_u64()).unwrap_or(0);
                let m = s.get("max_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                if m > agg.max_ms { agg.max_ms = m; }
            }
        }
        if let Some(s) = rec.get("search") {
            let q = s.get("queries").and_then(|v| v.as_u64()).unwrap_or(0);
            summary.search_queries += q;
            summary.search_zero += s.get("zero").and_then(|v| v.as_u64()).unwrap_or(0);
            summary.search_fts_only += s.get("fts_only").and_then(|v| v.as_u64()).unwrap_or(0);
            summary.search_hybrid += s.get("hybrid").and_then(|v| v.as_u64()).unwrap_or(0);
            // Per-session avg_quality → re-weight by query count to merge.
            let avg = s.get("avg_quality").and_then(|v| v.as_f64()).unwrap_or(0.0);
            summary.search_quality_weighted_sum += avg * q as f64;
        }
        if let Some(idx) = rec.get("index") {
            if let Some(ms) = idx.get("full_ms").and_then(|v| v.as_u64()) {
                summary.full_index_count += 1;
                summary.full_index_ms_sum += ms;
            }
            summary.incr_count += idx.get("incr").and_then(|v| v.as_u64()).unwrap_or(0);
            summary.files_indexed += idx.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
        }
        // Recommend→use funnel: per-session, did a session that saw a deny/hint
        // (window-joined into the `recs` field at flush) also call a cg query tool?
        let used_cg = rec.get("tools").and_then(|v| v.as_object()).is_some_and(|tools| {
            CG_QUERY_TOOLS.iter().any(|t| {
                tools.get(*t).and_then(|s| s.get("n")).and_then(|n| n.as_u64()).unwrap_or(0) > 0
            })
        });
        if let Some(recs) = rec.get("recs") {
            let deny = recs.get("deny").and_then(|v| v.as_u64()).unwrap_or(0);
            let hint = recs.get("hint").and_then(|v| v.as_u64()).unwrap_or(0);
            // CLI query runs window-joined into the session (additive v0.49 field).
            let used_cli = recs.get("cli_use").and_then(|v| v.as_u64()).unwrap_or(0) > 0;
            let used_any = used_cg || used_cli;
            if deny > 0 {
                summary.sessions_with_deny += 1;
                if used_cg { summary.sessions_with_deny_and_cg += 1; }
                if used_cli { summary.sessions_with_deny_and_cli += 1; }
                if used_any { summary.sessions_with_deny_and_use += 1; }
            }
            if hint > 0 {
                summary.sessions_with_hint += 1;
                if used_cg { summary.sessions_with_hint_and_cg += 1; }
                if used_cli { summary.sessions_with_hint_and_cli += 1; }
                if used_any { summary.sessions_with_hint_and_use += 1; }
            }
        }
    }
    summary
}

/// Aggregate of `.code-graph/recommendations.jsonl` — the JS PreToolUse hooks'
/// record of how often a code-graph tool was RECOMMENDED (raw-grep hint/deny,
/// read-fanout hint). Joined against actual tool calls in `stats` to surface the
/// real-session conversion rate the synthetic routing_bench oracle can't see.
#[derive(Default)]
pub struct RecommendationSummary {
    /// Recommendation events only (deny/hint/bypass…) — `action:"use"` lines are
    /// conversions, counted in `cli_uses` instead.
    pub total: u64,
    /// "hint" / "deny" / "bypass" → count
    pub by_action: std::collections::BTreeMap<String, u64>,
    /// "grep" / "read" → count
    pub by_hook: std::collections::BTreeMap<String, u64>,
    /// Model-initiated `code-graph-mcp <query>` runs (action:"use").
    pub cli_uses: u64,
    /// Deny segmentation: answered:true denies satisfied the need in-place, so a
    /// low deny→use read is EXPECTED for them; only static (unanswered) denies
    /// ask the model to convert. Pre-v0.47 denies lack the field → unanswered.
    pub deny_answered: u64,
    pub deny_unanswered: u64,
    /// Outcome proxy ("search-decay"): silent grep/read allows recorded by the
    /// PreToolUse hooks (action:"observe"), so the model's raw fan-out is visible
    /// alongside the deny/hint events.
    pub observe: u64,
    /// SessionStart "live context" injections (action:"live_impact", hook:"session"):
    /// the recent-change blast radius pushed at session start (v0.63). A separate
    /// counter — like observe/use it is NOT a tool-call recommendation, so it stays
    /// out of `total`/`by_action`. Surfaced in stats so the feature isn't dark.
    pub live_impact: u64,
    /// Of `deny_answered` (cg delivered a grep answer in-place), how many were
    /// IMMEDIATELY followed by ANY grep/read event. Computed in append
    /// (chronological) order; a single-user-sequential approximation (truly
    /// concurrent sessions interleave in the shared file). NOTE: this raw count
    /// is NOT a failure rate — it lumps together healthy drill-down that cg also
    /// answered (`sustained_after_answer`), file-reads acting on the answer
    /// (observe), and genuine fall-through (`fallthrough_after_answer`). Only the
    /// last means the inline answer was insufficient. Read `fallthrough_after_answer`
    /// for the honest signal.
    pub researched_after_answer: u64,
    /// Subset of `researched_after_answer`: the follow-up search was ITSELF
    /// answered by cg (an answered deny / delivered hint) AND searched a DIFFERENT
    /// pattern, so the model drilled deeper and cg kept up — each step replaced
    /// another raw grep with an answer. A win, not a miss. A verbatim re-grep of
    /// the SAME pattern is excluded (scored as fall-through) when the hook recorded
    /// the pattern; pre-fix events without a pattern field still land here (the old
    /// upper-bound behavior, back-compatible).
    pub sustained_after_answer: u64,
    /// Subset of `researched_after_answer`: the follow-up was a search cg could
    /// NOT satisfy (static deny / advisory-only hint / bypass). THIS is the honest
    /// "the inline answer was insufficient and cg couldn't help the next step"
    /// signal — the actual fan-out leak. `observe` (a file read acting on the
    /// delivered answer) is excluded from both subsets: it is not a search cg failed.
    pub fallthrough_after_answer: u64,
    /// Subset of `researched_after_answer` EXCLUDED from both sustained AND
    /// fall-through: the follow-up search is itself a NULL signal about the prior
    /// answer's sufficiency. Two shapes: `fallthrough:"no-hits"` (cg ran the next
    /// grep and found nothing — necessarily a DIFFERENT query, since a verbatim
    /// re-grep of the answered pattern would re-hit the prior answer's lines, so
    /// 0 hits ⇒ a new search, not "the answer was wrong") and `reason:"unavailable"`
    /// (cg CLI couldn't run — infra, orthogonal to answer quality). Counting either
    /// as fall-through over-states "answer insufficient" — the same over-count class
    /// as lumping in drill-down/observe (v0.64). Tracked so the named subsets of
    /// `researched_after_answer` stay legible.
    pub followup_inconclusive: u64,
}

/// Parse and aggregate `recommendations.jsonl` content. Pure: no IO, no panics —
/// malformed lines are skipped silently (telemetry, not a contract surface).
pub fn aggregate_recommendations_jsonl(content: &str) -> RecommendationSummary {
    let mut s = RecommendationSummary::default();
    // Outcome-proxy state: `armed` means the previous tool event was an answered
    // deny (cg satisfied the grep in-place); the next grep/read event of ANY
    // action is a re-search — the inline answer wasn't enough. Lines are appended
    // chronologically so a single forward pass suffices.
    let mut armed = false;
    // Pattern of the armed answered deny (when the hook recorded one). A follow-up
    // search carrying the SAME pattern is a verbatim re-grep = the inline answer was
    // ignored/insufficient (a real fall-through), NOT a deeper drill-down. Absent on
    // pre-fix events → falls back to the answered/observe split (back-compatible).
    let mut armed_pattern: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else { continue; };
        let action = v.get("action").and_then(|x| x.as_str());
        let hook = v.get("hook").and_then(|x| x.as_str());

        // Re-search detection runs on every tool event, before action bucketing.
        let is_search_event = matches!(hook, Some("grep") | Some("read"))
            && matches!(action, Some("deny") | Some("hint") | Some("bypass") | Some("observe") | Some("inject"));
        if armed {
            if is_search_event {
                s.researched_after_answer += 1;
                let follow_pattern = v.get("pattern").and_then(|x| x.as_str());
                // Split the follow-up honestly. Same-pattern takes precedence: a
                // verbatim re-grep of the SAME denied pattern (re-deny after the
                // cooldown, or a grep observe within it) means the inline answer
                // didn't end the hunt for THAT query → fall-through, NOT a win and
                // NOT "acting on the answer". Otherwise: observe = a file read
                // acting on the delivered answer; answered:true = cg ALSO answered
                // the next (deeper) step (sustained drill-down, a win); anything
                // else (static deny / advisory hint / bypass) = cg fell through.
                // The is_some() guard keeps absent==absent (pre-fix events) OUT of
                // the same-pattern branch.
                let follow_inconclusive = v.get("fallthrough").and_then(|x| x.as_str())
                    == Some("no-hits")
                    || v.get("reason").and_then(|x| x.as_str()) == Some("unavailable");
                if armed_pattern.is_some() && armed_pattern.as_deref() == follow_pattern {
                    s.fallthrough_after_answer += 1;
                } else if follow_inconclusive {
                    // The follow-up is a NULL signal about the prior answer: `no-hits`
                    // = cg ran the next grep and found nothing (a verbatim re-grep of
                    // the answered pattern would have re-hit it, so 0 hits ⇒ a NEW
                    // query, not "the answer was wrong"); `unavailable` = cg CLI
                    // couldn't run (infra). Neither means the inline answer was
                    // insufficient → exclude from fall-through (same over-count class
                    // as the observe/drill-down split). Ordered after the same-pattern
                    // check so a verbatim re-grep still scores as fall-through.
                    s.followup_inconclusive += 1;
                } else if action == Some("observe") {
                    // acting on the answer — neither sustained nor fall-through
                } else if v.get("answered").and_then(|x| x.as_bool()) == Some(true) {
                    s.sustained_after_answer += 1;
                } else {
                    s.fallthrough_after_answer += 1;
                }
            }
            armed = false; // only the IMMEDIATELY-next tool event counts
            armed_pattern = None;
        }

        // observe / use are not recommendation events: count separately, like cli use.
        match action {
            Some("use") => { s.cli_uses += 1; continue; }
            Some("observe") => { s.observe += 1; continue; }
            Some("live_impact") => { s.live_impact += 1; continue; }
            _ => {}
        }
        s.total += 1;
        if let Some(a) = action {
            *s.by_action.entry(a.to_string()).or_insert(0) += 1;
            if a == "deny" {
                if v.get("answered").and_then(|x| x.as_bool()) == Some(true) {
                    s.deny_answered += 1;
                    armed = true; // watch the next event for a re-search
                    // Remember the pattern (if recorded) so a verbatim re-grep of it
                    // is scored as fall-through, not sustained.
                    armed_pattern = v.get("pattern").and_then(|x| x.as_str()).map(String::from);
                } else {
                    s.deny_unanswered += 1;
                }
            } else if a == "inject" {
                // Compound-grep PostToolUse: an answered inject delivered cg's
                // AST-aware view of a grep that rode inside a compound command
                // (so PreToolUse never denied it). It arms the funnel exactly like
                // an answered deny — the next search event scores whether the
                // inline inject sufficed (inject→fallthrough) or cg also answered
                // the deeper step (sustained), parallel to deny→fallthrough.
                // inject is recorded only when it actually delivered hits, so it is
                // always answered; no unanswered counter (unlike deny). It still
                // lands in total/by_action via the generic map above.
                if v.get("answered").and_then(|x| x.as_bool()) == Some(true) {
                    armed = true;
                    armed_pattern = v.get("pattern").and_then(|x| x.as_str()).map(String::from);
                }
            }
        }
        if let Some(h) = hook {
            *s.by_hook.entry(h.to_string()).or_insert(0) += 1;
        }
    }
    s
}

// Idiomatic-flavor UX change — `//` (not `///`) so it stays out of clap `--help`:
// `--last <non-number>` is now a hard parse error (exit 2, clap message) instead of
// the prior warn-and-show-all fallback.
/// CLI arguments for the `stats` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp stats",
          about = "Aggregate session metrics from .code-graph/usage.jsonl")]
pub struct StatsArgs {
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Limit to the last N sessions (default: all)
    #[arg(long)]
    pub last: Option<usize>,
}

/// Numeric (semver) sort key for a version string. `versions` is stored in a
/// BTreeSet, which orders lexically — so "0.5.40" sorted AFTER "0.32.2". Parse the
/// leading digits of the first three dot-separated components so ordering is by
/// (major, minor, patch); non-numeric/missing components fall back to 0, keeping
/// the sort total and panic-free for odd version strings.
fn version_sort_key(v: &str) -> (u64, u64, u64) {
    let mut parts = v.split('.').map(|part| {
        part.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0)
    });
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// Pluralize a count for human-readable output: `1 file`, `0 files`, `2 files`.
/// Avoids the "1 files"/"1 lines" grammar glitch on single-item results (common
/// for single-file modules and one-line dead-code candidates). Naive `+s` only —
/// callers pass already-plural-friendly stems (file, line, symbol).
fn plural(n: i64, singular: &str) -> String {
    if n == 1 { format!("1 {singular}") } else { format!("{n} {singular}s") }
}

/// Print aggregated session metrics from `.code-graph/usage.jsonl`.
/// Diagnostic: shows which tools you actually use + search/index activity.
/// `--last N` limits to the most recent N sessions. `--json` emits structured output.
pub fn cmd_stats(project_root: &Path, args: StatsArgs) -> Result<()> {
    let json_mode = args.json;
    let last_n = args.last;

    let usage_path = project_root.join(CODE_GRAPH_DIR).join("usage.jsonl");
    if !usage_path.exists() {
        if json_mode {
            println!("{}", serde_json::json!({
                "sessions": 0,
                "tools": {},
                "note": format!("no usage data at {}", usage_path.display()),
            }));
        } else {
            eprintln!("No usage data yet at {}", usage_path.display());
            eprintln!("Run an MCP session first (sessions flush metrics on EOF).");
        }
        return Ok(());
    }

    let content = std::fs::read_to_string(&usage_path)?;
    let summary = aggregate_usage_jsonl(&content, last_n);

    // Conversion metric: cg tool calls vs PreToolUse recommendations. The JSONL
    // has no per-session boundary, so it is aggregated whole (last_n applies only
    // to usage sessions). Absent file → empty (default) summary.
    let rec_path = project_root.join(CODE_GRAPH_DIR).join("recommendations.jsonl");
    let rec_exists = rec_path.exists();
    let recs = std::fs::read_to_string(&rec_path).ok()
        .map(|c| aggregate_recommendations_jsonl(&c))
        .unwrap_or_default();
    // Recording-side state of the conversion metric, made explicit so a dark
    // metric (file absent → PreToolUse hooks not recording here) is never
    // silently indistinguishable from "feature absent" or "no data yet".
    let rec_state = if recs.total > 0 || recs.cli_uses > 0 { "live" } else if rec_exists { "empty" } else { "absent" };

    if summary.sessions == 0 {
        if json_mode {
            println!("{}", serde_json::json!({"sessions": 0, "tools": {}}));
        } else {
            eprintln!("No sessions recorded.");
        }
        return Ok(());
    }

    if json_mode {
        let tools_json: serde_json::Map<String, serde_json::Value> = summary.tools.iter().map(|(name, a)| {
            let avg = a.total_ms.checked_div(a.n).unwrap_or(0);
            (name.clone(), serde_json::json!({
                "n": a.n, "total_ms": a.total_ms, "avg_ms": avg, "err": a.err, "max_ms": a.max_ms,
            }))
        }).collect();
        let avg_q = if summary.search_queries > 0 {
            summary.search_quality_weighted_sum / summary.search_queries as f64
        } else { 0.0 };
        let full_avg = summary.full_index_ms_sum.checked_div(summary.full_index_count).unwrap_or(0);
        let mut sorted_versions: Vec<String> = summary.versions.iter().cloned().collect();
        sorted_versions.sort_by_key(|v| version_sort_key(v));
        println!("{}", serde_json::json!({
            "sessions": summary.sessions,
            "parse_errors": summary.parse_errors,
            "versions": sorted_versions,
            "first_ts": summary.first_ts,
            "last_ts": summary.last_ts,
            "total_tool_calls": summary.total_tool_calls(),
            "live_tools": crate::domain::LIVE_MCP_TOOLS,
            "tools": tools_json,
            "search": {
                "queries": summary.search_queries,
                "zero": summary.search_zero,
                "avg_quality": (avg_q * 100.0).round() / 100.0,
                "fts_only": summary.search_fts_only,
                "hybrid": summary.search_hybrid,
            },
            "index": {
                "full_count": summary.full_index_count,
                "full_avg_ms": full_avg,
                "incr_count": summary.incr_count,
                "files_indexed": summary.files_indexed,
            },
            "recommendations": {
                "state": rec_state,
                "total": recs.total,
                "by_action": recs.by_action.iter().map(|(k, v)| (k.clone(), serde_json::json!(v)))
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
                "by_hook": recs.by_hook.iter().map(|(k, v)| (k.clone(), serde_json::json!(v)))
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
                "cg_tool_calls": summary.total_tool_calls(),
                "cli_uses": recs.cli_uses,
                "deny_answered": recs.deny_answered,
                "deny_unanswered": recs.deny_unanswered,
                // Outcome proxy: observe = silent grep/read allows recorded by the
                // hooks. re_search_rate = fraction of answered denies immediately
                // followed by ANY grep/read — kept for back-compat, but it OVER-counts
                // insufficiency (it includes drill-down cg also answered + file-reads).
                // fallthrough_rate is the honest "inline answer insufficient" fraction:
                // the follow-up was a search cg could NOT satisfy. Both null until an
                // answered deny exists to divide by.
                "observe": recs.observe,
                "live_impact": recs.live_impact,
                "researched_after_answer": recs.researched_after_answer,
                "re_search_rate": if recs.deny_answered > 0 {
                    serde_json::json!((recs.researched_after_answer as f64 / recs.deny_answered as f64 * 100.0).round() / 100.0)
                } else { serde_json::Value::Null },
                "sustained_after_answer": recs.sustained_after_answer,
                "fallthrough_after_answer": recs.fallthrough_after_answer,
                "followup_inconclusive": recs.followup_inconclusive,
                "fallthrough_rate": if recs.deny_answered > 0 {
                    serde_json::json!((recs.fallthrough_after_answer as f64 / recs.deny_answered as f64 * 100.0).round() / 100.0)
                } else { serde_json::Value::Null },
                // tool_calls / recommendations: two independent populations, so
                // this is an activity/volume ratio, NOT a recommend→use rate. The
                // real conversion is funnel.deny_conversion / hint_conversion.
                "tool_calls_per_rec": if recs.total > 0 {
                    (summary.total_tool_calls() as f64 / recs.total as f64 * 100.0).round() / 100.0
                } else { 0.0 },
                // Per-session deny→use / hint→use funnel (window-joined attribution).
                // v0.49: *_conversion is ANY-use (MCP cg tool OR CLI query) — the
                // deny→CLI leg is the proven conversion path; *_then_cg / *_then_cli
                // keep the legs separable.
                "funnel": {
                    "deny_sessions": summary.sessions_with_deny,
                    "deny_then_cg": summary.sessions_with_deny_and_cg,
                    "deny_then_cli": summary.sessions_with_deny_and_cli,
                    "deny_then_use": summary.sessions_with_deny_and_use,
                    "deny_conversion": session_conversion(summary.sessions_with_deny_and_use, summary.sessions_with_deny),
                    "hint_sessions": summary.sessions_with_hint,
                    "hint_then_cg": summary.sessions_with_hint_and_cg,
                    "hint_then_cli": summary.sessions_with_hint_and_cli,
                    "hint_then_use": summary.sessions_with_hint_and_use,
                    "hint_conversion": session_conversion(summary.sessions_with_hint_and_use, summary.sessions_with_hint),
                },
            },
        }));
    } else {
        let mut versions: Vec<&str> = summary.versions.iter().map(|s| s.as_str()).collect();
        versions.sort_by_key(|v| version_sort_key(v));
        println!("Sessions: {}   versions: {}   {} → {}",
            summary.sessions,
            if versions.is_empty() { "-".into() } else { versions.join(",") },
            summary.first_ts.as_deref().unwrap_or("-"),
            summary.last_ts.as_deref().unwrap_or("-"),
        );
        println!("Total tool calls: {}", summary.total_tool_calls());
        if summary.parse_errors > 0 {
            println!("(warning: {} malformed line(s) skipped)", summary.parse_errors);
        }
        println!();

        let mut sorted: Vec<(&String, &ToolAgg)> = summary.tools.iter().collect();
        sorted.sort_by_key(|(_, a)| std::cmp::Reverse(a.n));

        if sorted.is_empty() {
            println!("(no tool calls recorded)");
        } else {
            println!("{:<28} {:>6} {:>10} {:>6} {:>8}", "Tool", "n", "avg_ms", "err", "max_ms");
            println!("{}", "-".repeat(62));
            let mut any_legacy = false;
            for (name, agg) in &sorted {
                let avg = agg.total_ms.checked_div(agg.n).unwrap_or(0);
                // Mark tool names no longer in the live tools/list surface (folded
                // or hidden, recorded by older sessions) so the table doesn't
                // commingle historical names with the current live set.
                let legacy = !crate::domain::LIVE_MCP_TOOLS.contains(&name.as_str());
                if legacy { any_legacy = true; }
                let label = if legacy { format!("{name} †") } else { name.to_string() };
                println!("{:<28} {:>6} {:>10} {:>6} {:>8}", label, agg.n, avg, agg.err, agg.max_ms);
            }
            if any_legacy {
                println!("  † not in the current tools/list surface (folded/hidden; from older sessions)");
            }
        }

        if summary.search_queries > 0 {
            let zero_pct = (summary.search_zero as f64 / summary.search_queries as f64 * 100.0).round() as u64;
            let avg_q = summary.search_quality_weighted_sum / summary.search_queries as f64;
            println!();
            println!("Search: {} queries, {} zero-result ({}%), hybrid/fts {}/{}, avg quality {:.2}",
                summary.search_queries, summary.search_zero, zero_pct,
                summary.search_hybrid, summary.search_fts_only, avg_q);
        }

        if summary.full_index_count > 0 || summary.incr_count > 0 {
            let full_part = match summary.full_index_ms_sum.checked_div(summary.full_index_count) {
                Some(avg) if summary.full_index_count > 0 => format!(" (avg {}ms)", avg),
                _ => String::new(),
            };
            println!("Index:  {} full{}, {} incremental, {} files indexed",
                summary.full_index_count, full_part, summary.incr_count, summary.files_indexed);
        }

        println!();
        if recs.total > 0 {
            let actions: Vec<String> = recs.by_action.iter().map(|(k, v)| format!("{v} {k}")).collect();
            let ratio = summary.total_tool_calls() as f64 / recs.total as f64;
            println!("Recommendations: {} emitted ({})", recs.total, actions.join(", "));
            if recs.deny_answered + recs.deny_unanswered > 0 {
                // answered:true denies satisfy the need in-place — read their
                // conversion separately or the funnel under-reports the feature.
                println!("Denies: {} answered in-place, {} static",
                    recs.deny_answered, recs.deny_unanswered);
            }
            if recs.cli_uses > 0 {
                println!("CLI uses: {} model-initiated code-graph-mcp queries", recs.cli_uses);
            }
            // Outcome proxy ("search-decay"): of the answered denies (cg delivered
            // the grep result in-place), how often did the model immediately keep
            // searching? Lower = the inline answer was enough. observe = the silent
            // grep/read allows that make the fan-out visible.
            if recs.deny_answered > 0 {
                // Honest fan-out signal. The follow-up after an answered deny is
                // one of: cg ALSO answered a DIFFERENT next step (sustained drill-down
                // — a win), a file read acting on the answer (observe), or the inline
                // answer didn't end the hunt — a verbatim re-grep of the same pattern
                // or a search cg couldn't satisfy (fall-through). Only fall-through
                // means the inline answer was insufficient. The raw "kept searching"
                // count lumps all three
                // and reads alarmingly high even when cg wins every step, so lead
                // with fall-through and show the raw count correctly framed.
                let ft_pct = (recs.fallthrough_after_answer as f64 / recs.deny_answered as f64 * 100.0).round() as u64;
                println!("Fall-through after cg answer: {}/{} answered denies → inline answer didn't end the hunt (verbatim re-grep or a search cg couldn't satisfy) = {ft_pct}% (the real 'answer insufficient' rate; lower is better)",
                    recs.fallthrough_after_answer, recs.deny_answered);
                if recs.sustained_after_answer > 0 {
                    println!("  ↳ drill-down sustained: {} follow-up search(es) cg also answered — cg kept up, not a miss",
                        recs.sustained_after_answer);
                }
                if recs.followup_inconclusive > 0 {
                    println!("  ↳ inconclusive (excluded): {} follow-up(s) where cg found nothing (no-hits = a new query) or was unavailable — says nothing about the prior answer",
                        recs.followup_inconclusive);
                }
                let raw_pct = (recs.researched_after_answer as f64 / recs.deny_answered as f64 * 100.0).round() as u64;
                println!("  ↳ any follow-up (raw): {}/{} = {raw_pct}% — incl. drill-down + file-reads; NOT a failure rate",
                    recs.researched_after_answer, recs.deny_answered);
            }
            if recs.observe > 0 {
                println!("Tool observes: {} silent grep/read allows recorded (fan-out timeline)", recs.observe);
            }
            // Volume ratio (NOT a conversion rate): cg tool calls and hook
            // recommendations are independent populations, so this only signals
            // activity level. The real recommend→use conversion is the Deny→use /
            // Hint→use funnel printed below.
            println!("Tool-call volume: {} cg calls / {} recommendations = {ratio:.2} (activity ratio, not conversion)",
                summary.total_tool_calls(), recs.total);
        } else if rec_exists {
            // File present but empty: hooks are wired and recording, just no
            // recommendation has fired yet.
            println!("Recommendations: 0 recorded (PreToolUse hooks active; conversion metric live, no data yet)");
        } else {
            // No file at all: the recording hooks are not active in this project
            // (e.g. a dev `.mcp.json` server with the marketplace plugin's
            // PreToolUse hooks disabled). Surface the dark state instead of
            // printing nothing — silence reads as "feature absent".
            println!("Conversion metric: DARK — no recommendations.jsonl. PreToolUse hooks are not");
            println!("  recording here, so recommend→use conversion cannot be measured in this project.");
        }
        // v0.63 — SessionStart live-context injections. Printed outside the
        // total>0 branch (it's a separate counter): a session whose only event was
        // the SessionStart injection still surfaces it instead of reading dark.
        if recs.live_impact > 0 {
            println!("Live-context: {} recent-change blast-radius injection(s) at SessionStart", recs.live_impact);
        }
        // Per-session funnel: of sessions that saw a deny/hint, how many also called
        // a cg query tool. This is the deny→use attribution the aggregate ratio can't give.
        if summary.sessions_with_deny > 0 {
            let pct = (summary.sessions_with_deny_and_use as f64 / summary.sessions_with_deny as f64 * 100.0).round() as u64;
            println!("Deny→use: {}/{} deny-sessions used cg = {}% (mcp {}, cli {})",
                summary.sessions_with_deny_and_use, summary.sessions_with_deny, pct,
                summary.sessions_with_deny_and_cg, summary.sessions_with_deny_and_cli);
        }
        if summary.sessions_with_hint > 0 {
            let pct = (summary.sessions_with_hint_and_use as f64 / summary.sessions_with_hint as f64 * 100.0).round() as u64;
            println!("Hint→use: {}/{} hint-sessions used cg = {}% (mcp {}, cli {})",
                summary.sessions_with_hint_and_use, summary.sessions_with_hint, pct,
                summary.sessions_with_hint_and_cg, summary.sessions_with_hint_and_cli);
        }
    }

    Ok(())
}

// --- grep subcommand ---

/// CLI arguments for the `grep` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp grep",
          about = "AST-context grep (ripgrep + containing function/class)")]
pub struct GrepArgs {
    /// Search pattern (ripgrep regex; use -F for literal strings)
    #[arg(allow_hyphen_values = true)]
    pub pattern: String,
    /// Optional paths to restrict the search (must be within the project root)
    pub paths: Vec<String>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Case-insensitive search
    #[arg(short = 'i', long)]
    pub ignore_case: bool,
    /// Only match whole words
    #[arg(short = 'w', long)]
    pub word_regexp: bool,
    /// Treat the pattern as a literal string, not a regex
    #[arg(short = 'F', long)]
    pub fixed_strings: bool,
    /// Print only the names of files with matches
    #[arg(short = 'l', long)]
    pub files_with_matches: bool,
    /// Show N lines before and after each match
    #[arg(short = 'C', long, value_name = "N")]
    pub context: Option<u64>,
    /// Show N lines after each match
    #[arg(short = 'A', long, value_name = "N")]
    pub after_context: Option<u64>,
    /// Show N lines before each match
    #[arg(short = 'B', long, value_name = "N")]
    pub before_context: Option<u64>,
    /// Max matches per file; 0 = unlimited
    #[arg(short = 'm', long, value_name = "N", default_value_t = 100)]
    pub max_count: u64,
    /// Accepted for grep parity; line numbers are always printed (no-op).
    #[arg(short = 'n', long = "line-number")]
    pub line_number: bool,
    /// Accepted for grep parity; the search is always recursive (no-op).
    #[arg(short = 'r', long = "recursive", visible_short_alias = 'R')]
    pub recursive: bool,
    /// Accepted for grep parity; filenames are always shown (no-op).
    #[arg(short = 'H', long = "with-filename")]
    pub with_filename: bool,
}

/// Split attached short-option context forms (`-A2` → `-A`, `2`; bundled
/// `-nA2` → `-nA`, `2`) so the `grep` subcommand accepts grep/ripgrep's attached
/// numeric syntax.
///
/// The `pattern` positional carries `allow_hyphen_values` so a flag-shaped
/// search term (e.g. `--no-default-features`) is searchable without a `--`
/// escape. The side effect: clap binds an attached short value like `-A2` —
/// which is not an *exact* registered token — to the positional as the pattern
/// instead of parsing `-A` with value `2`, leaving the real pattern to be
/// misrouted into the path list (rg then errors "No such file"). Splitting the
/// digits into a separate token makes `-A`/`-B`/`-C` exact tokens again (and a
/// bundle like `-nA2` becomes `-nA 2`, which clap parses as `-n -A=2`).
///
/// Stops at the first `--` so an intentional literal `-A2` *pattern* after the
/// separator (`grep -- -A2`) is preserved verbatim.
pub fn normalize_grep_argv(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 2);
    let mut after_sep = false;
    for a in args {
        if after_sep || a == "--" {
            after_sep = after_sep || a == "--";
            out.push(a);
            continue;
        }
        if let Some((cluster, digits)) = split_attached_context(&a) {
            out.push(cluster);
            out.push(digits);
            continue;
        }
        out.push(a);
    }
    out
}

/// If `tok` is a single-dash short-flag cluster ending in an attached value —
/// `-A2`, `-C10`, `-m5`, or a bundle like `-nA2`/`-niB3` (leading boolean shorts,
/// then a value flag `-A`/`-B`/`-C`/`-m`, then digits) — return
/// `(cluster_without_digits, digits)`. Returns `None` otherwise (incl. `--long`,
/// bare `-A`, `-z2`, `-A2x`, and `-A2B3` where a value flag is not last in the
/// bundle).
///
/// grep and ripgrep both accept these attached forms, and the value flag is only
/// ever last in a bundle (`-nA2` valid; `-A2n`/`-An2` rejected by real grep), so
/// we peel a trailing `[ABCm][0-9]+` run that sits after a run of ASCII-letter
/// shorts. clap then bundle-parses the cluster (`-nA` → `-n -A`) and the bare
/// `-A`/`-B`/`-C`/`-m` takes the now-separate digit token as its value.
fn split_attached_context(tok: &str) -> Option<(String, String)> {
    let b = tok.as_bytes();
    // single dash, not `--`, at least `-X0` (a flag char + ≥1 digit).
    if b.len() < 3 || b[0] != b'-' || b[1] == b'-' {
        return None;
    }
    // Start of the trailing ASCII-digit run (one past the last non-digit byte).
    let digit_start = b.iter().rposition(|&c| !c.is_ascii_digit())? + 1;
    if digit_start == b.len() {
        return None; // no trailing digits (e.g. `-A2x`, `-nr`)
    }
    // The byte immediately before the digits must be a value-taking flag, and
    // everything between `-` and the digits must be ASCII letters (the leading
    // boolean shorts + that value flag). Rejects `-A2B3`, `-z2`, `-2`.
    if !matches!(b[digit_start - 1], b'A' | b'B' | b'C' | b'm')
        || !b[1..digit_start].iter().all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    Some((tok[..digit_start].to_string(), tok[digit_start..].to_string()))
}

/// Return the first single-dash short-flag cluster (pre-`--`) that contains a
/// flag the `grep` subcommand does not implement — e.g. `-v`, `-c`, `-o`, `-e`,
/// `-P`. The pattern positional's `allow_hyphen_values` would otherwise swallow
/// such a flag AS the search term, pushing the real pattern into the path list →
/// a cryptic `rg: No such file or directory: <pattern>` (same failure class the
/// `-A2`/`-n` parity fixes addressed). Surfacing it lets the caller emit a clear
/// "unsupported flag" message instead.
///
/// Only clusters starting with an ASCII letter are flag candidates; `--long`
/// tokens, bare `-`, and dash-then-symbol/digit terms (`->`, `-1`, `-.*`) are
/// legitimate searchable patterns and are left for the positional. A value-taking
/// short (`-A`/`-B`/`-C`/`-m`) consumes the rest of the cluster, so judging stops
/// there. Scanning stops at the first `--` so `grep -- -v` searches the literal.
fn first_unsupported_grep_flag(args: &[String]) -> Option<String> {
    const BOOL_SHORTS: &[u8] = b"iwFlnrRHh"; // supported value-less shorts (+ -h help)
    const VALUE_SHORTS: &[u8] = b"ABCm"; // shorts that take a value (consume the tail)
    for a in args {
        if a == "--" {
            break;
        }
        let b = a.as_bytes();
        if b.len() < 2 || b[0] != b'-' || !b[1].is_ascii_alphabetic() {
            continue;
        }
        let mut i = 1;
        let mut bad = false;
        while i < b.len() {
            let c = b[i];
            if VALUE_SHORTS.contains(&c) {
                break; // value short eats the remainder (attached or next token)
            }
            if !BOOL_SHORTS.contains(&c) {
                bad = true;
                break;
            }
            i += 1;
        }
        if bad {
            return Some(a.clone());
        }
    }
    None
}

/// Parse `grep` arguments from the full process argv (including argv\[0]),
/// applying [`normalize_grep_argv`] first. Mirrors the other subcommands'
/// `skip(1)`; clap consumes the leading `grep` token as the binary-name slot.
///
/// Rejects unsupported short flags ([`first_unsupported_grep_flag`]) up front so
/// they fail with a clear message instead of being swallowed as the pattern.
pub fn parse_grep_args(argv: &[String]) -> GrepArgs {
    let raw: Vec<String> = argv.iter().skip(1).cloned().collect();
    if let Some(bad) = first_unsupported_grep_flag(&raw) {
        // --json early-bail must still emit an empty array (CLI JSON contract).
        let json = raw
            .iter()
            .take_while(|a| a.as_str() != "--")
            .any(|a| a.as_str() == "--json");
        if json {
            println!("[]");
        }
        eprintln!(
            "[code-graph] unsupported flag: {bad}. Supported: -i -w -F -l -A -B -C -m \
             (and no-op -n/-r/-R/-H). To search a literal flag-shaped string, put it \
             after --: code-graph-mcp grep -- {bad}"
        );
        grep_exit(2);
    }
    GrepArgs::parse_from(normalize_grep_argv(raw))
}

/// AST-context grep: ripgrep + AST context from index.
///
/// Output format:
/// ```text
/// src/mcp/server.rs:142  let result = handle_request(params);
///   → fn McpServer::process_message (lines 130-180)
/// ```
/// grep-parity exit codes (v0.50): 0 = matched, 1 = no match, 2 = error/usage.
/// Flushes stdout before exiting so piped consumers see complete output.
fn grep_exit(code: i32) -> ! {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

/// git-tracked files that ripgrep's walk skips: tracked ∖ `rg --files`.
/// Three blind-spot classes share this root cause (rg prunes by its own
/// ignore/hidden rules without checking tracked status):
///   1. tracked file under a gitignored dir (`docs/` ignored, doc force-added)
///   2. `dir/` + `!dir/keep/` negation — git whitelists the file, rg prunes
///      `dir/` during the walk before evaluating the negation (rg 14.x)
///   3. tracked hidden files (rg skips hidden by default)
///
/// Passing the difference as explicit file args restores `git grep` semantics.
/// Empty when git is absent / not a work tree (then rg's walk is the answer).
/// `scope_rels` (relative, validated) restricts both sides to the user paths.
fn tracked_files_missed_by_walk(project_root: &Path, scope_rels: &[String]) -> Vec<String> {
    let mut ls = Command::new("git");
    ls.args(["ls-files", "-z"]).current_dir(project_root);
    for rel in scope_rels {
        ls.arg(rel);
    }
    let Ok(out) = ls.output() else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let tracked: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| String::from_utf8(s.to_vec()).ok())
        .collect();
    if tracked.is_empty() {
        return Vec::new();
    }

    // The same walk the search performs (cwd-relative output).
    let mut rg_files = Command::new("rg");
    rg_files.arg("--files").current_dir(project_root);
    for rel in scope_rels {
        rg_files.arg(rel);
    }
    let walked: std::collections::HashSet<String> = match rg_files.output() {
        // rg --files exits 1 with empty stdout when the walk finds nothing —
        // same parse either way; only spawn failure disables the supplement.
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim_start_matches("./").to_string())
            .collect(),
        Err(_) => return Vec::new(),
    };

    tracked.into_iter().filter(|t| !walked.contains(t)).collect()
}

pub fn cmd_grep(project_root: &Path, args: GrepArgs) -> Result<()> {
    let GrepArgs {
        pattern, paths, json: json_mode,
        ignore_case, word_regexp, fixed_strings, max_count,
        files_with_matches, context, after_context, before_context,
        // -n/-r/-R/-H: accepted for grep muscle-memory parity, all no-ops here
        // (line numbers, recursion, and filenames are already the default).
        line_number: _, recursive: _, with_filename: _,
    } = args;
    let context_requested = context.is_some() || after_context.is_some() || before_context.is_some();
    // clap accepts an empty-string positional (e.g. an unset shell var expanding
    // to ""); preserve the non-empty guard + Usage string. Usage error → exit 2.
    if pattern.is_empty() {
        if json_mode {
            println!("[]");
        }
        eprintln!("Usage: code-graph-mcp grep <pattern> [paths...] [-i] [-w] [-F] [--max-count N] [--json]");
        grep_exit(2);
    }

    let root_canonical = project_root.canonicalize().unwrap_or(project_root.to_path_buf());

    // Validate every search path is within the project root (path traversal guard).
    let mut search_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut search_rels: Vec<String> = Vec::new();
    for path in &paths {
        let resolved = project_root.join(path);
        let canonical = resolved.canonicalize().unwrap_or(resolved);
        if !canonical.starts_with(&root_canonical) {
            if json_mode {
                println!("[]");
            }
            eprintln!("[code-graph] search path must be within project root: {}", path);
            grep_exit(2);
        }
        if let Ok(rel) = canonical.strip_prefix(&root_canonical) {
            search_rels.push(rel.to_string_lossy().into_owned());
        }
        search_paths.push(canonical);
    }

    let mut rg_cmd = Command::new("rg");
    if files_with_matches {
        // -l: plain one-path-per-line output (rg stops at the first match per
        // file); context flags are meaningless here, like grep, and ignored.
        rg_cmd.arg("-l");
    } else {
        rg_cmd.arg("--json").arg("-n");
        if let Some(n) = context {
            rg_cmd.arg(format!("--context={}", n));
        }
        if let Some(n) = after_context {
            rg_cmd.arg(format!("--after-context={}", n));
        }
        if let Some(n) = before_context {
            rg_cmd.arg(format!("--before-context={}", n));
        }
        if max_count > 0 {
            rg_cmd.arg(format!("--max-count={}", max_count));
        }
    }
    if ignore_case {
        rg_cmd.arg("-i");
    }
    if word_regexp {
        rg_cmd.arg("-w");
    }
    if fixed_strings {
        rg_cmd.arg("-F");
    }
    // `--` so leading-dash patterns (e.g. searching for "--no-default-features")
    // reach rg as the pattern instead of being parsed as flags.
    rg_cmd.arg("--").arg(&pattern);

    if search_paths.is_empty() {
        rg_cmd.arg(project_root);
    } else {
        for p in &search_paths {
            rg_cmd.arg(p);
        }
    }

    // git-grep parity: append tracked files the rg walk misses as explicit
    // args (explicit file args bypass rg's ignore rules). git ls-files
    // pathspecs + rg --files args are both scoped to the user's paths, so the
    // supplement honors path restrictions; files passed explicitly by the
    // user appear in the walk output and dedup naturally.
    const SUPPLEMENT_CAP: usize = 500;
    let mut supplement = tracked_files_missed_by_walk(project_root, &search_rels);
    if supplement.len() > SUPPLEMENT_CAP {
        eprintln!(
            "[code-graph] {} tracked files outside the rg walk; searching the first {} only",
            supplement.len(), SUPPLEMENT_CAP
        );
        supplement.truncate(SUPPLEMENT_CAP);
    }
    for rel in &supplement {
        // Join on project_root (not the canonicalized root) so parse_rg_json's
        // prefix-strip produces relative paths in the output.
        let abs = project_root.join(rel);
        if abs.is_file() {
            rg_cmd.arg(abs);
        }
    }

    let rg_output = rg_cmd.output();
    let rg_output = match rg_output {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if json_mode {
                println!("[]");
            }
            eprintln!("[code-graph] ripgrep (rg) not found. Install: https://github.com/BurntSushi/ripgrep");
            grep_exit(2);
        }
        Err(e) => return Err(e.into()),
    };

    // ripgrep exit codes: 0 = matched, 1 = no match, 2 = error (invalid regex,
    // unreadable path). grep-parity: surface as exit 2 — a regex parse error
    // (e.g. an unescaped `(` in `res.json(`) must not look like a no-match.
    if rg_output.status.code() == Some(2) {
        if json_mode {
            println!("[]");
        }
        let stderr = String::from_utf8_lossy(&rg_output.stderr);
        let stderr = stderr.trim();
        eprintln!(
            "[code-graph] ripgrep error: {}",
            if stderr.is_empty() { "invalid pattern or unreadable path" } else { stderr }
        );
        grep_exit(2);
    }

    // -l mode: rg already printed one path per line; relativize and pass through.
    if files_with_matches {
        let root_str = project_root.to_string_lossy().into_owned();
        let files: Vec<String> = String::from_utf8_lossy(&rg_output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| relativize_path(l, &root_str).to_string())
            .collect();
        if files.is_empty() {
            if json_mode {
                println!("[]");
            }
            eprintln!("[code-graph] No matches for: {}", pattern);
            grep_exit(1);
        }
        let write_result: std::io::Result<()> = (|| {
            let mut stdout = std::io::stdout().lock();
            if json_mode {
                let serialized = serde_json::to_string(&files)
                    .unwrap_or_else(|_| "[]".to_string());
                writeln!(stdout, "{}", serialized)?;
            } else {
                for f in &files {
                    writeln!(stdout, "{}", f)?;
                }
            }
            Ok(())
        })();
        match write_result {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => grep_exit(0),
            other => other?,
        }
        return Ok(());
    }

    // Parse rg JSON output into matches
    let matches = parse_rg_json(&rg_output.stdout, project_root);
    if matches.is_empty() {
        if json_mode {
            println!("[]");
        }
        // Surface ripgrep errors (e.g., path not found) instead of a silent exit
        let stderr = String::from_utf8_lossy(&rg_output.stderr);
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            eprintln!("[code-graph] {}", stderr);
        } else {
            eprintln!("[code-graph] No matches for: {}", pattern);
        }
        // grep parity: no match exits 1.
        grep_exit(1);
    }

    // Per-file cap honesty: a file whose match count equals the cap was likely
    // truncated — silent truncation reads as "complete results" to the caller.
    // Context lines don't count toward the cap.
    let capped_files: Vec<&str> = if max_count > 0 {
        let mut counts: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
        for m in matches.iter().filter(|m| !m.is_context) {
            *counts.entry(m.file.as_str()).or_insert(0) += 1;
        }
        let mut capped: Vec<&str> = counts
            .iter()
            .filter(|(_, &c)| c >= max_count)
            .map(|(&f, _)| f)
            .collect();
        capped.sort_unstable();
        capped
    } else {
        Vec::new()
    };
    // Fast membership for the per-match `truncated` JSON marker below: stderr
    // alone is invisible to a `--json` consumer parsing stdout, so each match in
    // a file that hit the cap carries `"truncated": true`.
    let capped_set: std::collections::HashSet<&str> = capped_files.iter().copied().collect();

    // Try to open index for AST context; cache per-file nodes for both modes.
    let ctx = CliContext::try_open(project_root);
    if let Some(ref c) = ctx {
        // Annotation syncs below may write; never let a concurrent writer
        // (MCP server watcher, another index run) stall an interactive grep
        // for the default 5s busy_timeout — fail fast and mark stale instead.
        let _ = c.db.conn().execute_batch("PRAGMA busy_timeout = 250;");
    }
    // Lazy query-time freshness (parity with the MCP file_path tools'
    // ensure_file_indexed, v0.18.0): before annotating from the index,
    // hash-compare the file and re-index it when dirty — bounded by a sync
    // budget so a repo-wide grep over many dirty files keeps its latency.
    // Beyond budget (or on write contention) annotations carry [stale].
    let sync_budget: usize = std::env::var("CODE_GRAPH_GREP_SYNC_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let mut synced = 0usize;
    let mut stale_count = 0usize;
    let mut node_cache: std::collections::HashMap<String, (Vec<queries::NodeResult>, bool)> =
        std::collections::HashMap::new();
    let mut lookup_container = |file: &str, line: u64| -> Option<(String, String, i64, i64, bool)> {
        let ctx = ctx.as_ref()?;
        if !node_cache.contains_key(file) {
            let mut stale = false;
            // Only files already in the index are sync candidates: indexing a
            // brand-new path here could pull gitignored supplement files into
            // the index, diverging from scan_directory's scope.
            let stored: Option<String> = ctx
                .db
                .conn()
                .query_row("SELECT blake3_hash FROM files WHERE path = ?1", [file], |r| r.get(0))
                .ok();
            if let Some(stored_hash) = stored {
                let abs = ctx.project_root.join(file);
                let disk = crate::indexer::merkle::hash_file(&abs).ok();
                if disk.as_deref() != Some(stored_hash.as_str()) {
                    if synced < sync_budget {
                        match crate::indexer::pipeline::ensure_file_indexed(
                            &ctx.db, &ctx.project_root, file, None,
                        ) {
                            Ok(changed) => {
                                if changed {
                                    synced += 1;
                                }
                            }
                            // SQLITE_BUSY / parse failure: annotate honestly.
                            Err(_) => stale = true,
                        }
                    } else {
                        stale = true;
                    }
                }
            }
            if stale {
                stale_count += 1;
            }
            let nodes = queries::get_nodes_by_file_path(ctx.db.conn(), file).unwrap_or_default();
            node_cache.insert(file.to_string(), (nodes, stale));
        }
        let (nodes, stale) = node_cache.get(file)?;
        find_containing_node_in(nodes, line).map(|(t, n, s, e)| (t, n, s, e, *stale))
    };

    // Output. EPIPE (reader hung up, e.g. `| head`) is not an error — finish
    // silently with exit 0 like grep instead of spraying "Broken pipe".
    let write_result: std::io::Result<()> = (|| {
        let mut stdout = std::io::stdout().lock();
        if json_mode {
            let mut json_results = Vec::new();
            for m in &matches {
                let mut entry = serde_json::json!({
                    "file": m.file,
                    "line": m.line,
                    "text": m.text,
                });
                if m.is_context {
                    entry["context"] = serde_json::json!(true);
                } else {
                    if let Some(container) = lookup_container(&m.file, m.line) {
                        let mut c = serde_json::json!({
                            "type": container.0,
                            "name": container.1,
                            "lines": format!("{}-{}", container.2, container.3),
                        });
                        if container.4 {
                            c["stale"] = serde_json::json!(true);
                        }
                        entry["container"] = c;
                    }
                    // This file hit the per-file cap — results for it are truncated.
                    if capped_set.contains(m.file.as_str()) {
                        entry["truncated"] = serde_json::json!(true);
                    }
                }
                json_results.push(entry);
            }
            let serialized = serde_json::to_string(&json_results)
                .unwrap_or_else(|_| "[]".to_string());
            writeln!(stdout, "{}", serialized)?;
        } else {
            // grep formatting: matches `file:line`, context lines `file-line`,
            // `--` between non-contiguous groups when context is shown.
            let mut prev: Option<(String, u64)> = None;
            for m in &matches {
                if context_requested {
                    if let Some((ref pf, pl)) = prev {
                        if pf != &m.file || m.line > pl + 1 {
                            writeln!(stdout, "--")?;
                        }
                    }
                    prev = Some((m.file.clone(), m.line));
                }
                let sep = if m.is_context { '-' } else { ':' };
                write!(stdout, "{}{}{}  {}", m.file, sep, m.line, m.text)?;
                if !m.text.ends_with('\n') {
                    writeln!(stdout)?;
                }
                if !m.is_context {
                    if let Some((node_type, name, start, end, stale)) =
                        lookup_container(&m.file, m.line)
                    {
                        let marker = if stale { " [stale]" } else { "" };
                        writeln!(stdout, "  → {} {} (lines {}-{}){}", node_type, name, start, end, marker)?;
                    }
                }
            }
        }
        Ok(())
    })();
    match write_result {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => grep_exit(0),
        other => other?,
    }

    if !capped_files.is_empty() {
        eprintln!(
            "[code-graph] truncated: {} file(s) hit the per-file cap of {} matches: {}. Use --max-count 0 for all matches.",
            capped_files.len(),
            max_count,
            capped_files.join(", ")
        );
    }
    if stale_count > 0 {
        eprintln!(
            "[code-graph] {} file(s) changed since last index; annotations marked [stale] — run: code-graph-mcp incremental-index",
            stale_count
        );
    }
    if ctx.is_none() {
        eprintln!("[code-graph] No index found. Run: code-graph-mcp incremental-index");
        eprintln!("[code-graph] Showing plain grep results (no AST context).");
    }

    Ok(())
}

struct GrepMatch {
    file: String,
    line: u64,
    text: String,
    /// true for -A/-B/-C context lines (rg JSON `type: "context"` records)
    is_context: bool,
}

/// Make an rg-reported path relative to the project root.
fn relativize_path<'a>(path_str: &'a str, root_str: &str) -> &'a str {
    let root_prefix = root_str.trim_end_matches('/');
    path_str
        .strip_prefix(root_prefix)
        .or_else(|| path_str.strip_prefix(root_str))
        .unwrap_or(path_str)
        .trim_start_matches('/')
}

/// Parse ripgrep JSON output into structured matches (and context lines when
/// -A/-B/-C were passed — rg interleaves `context` records in print order).
fn parse_rg_json(stdout: &[u8], project_root: &Path) -> Vec<GrepMatch> {
    let root_str = project_root.to_string_lossy().into_owned();
    let mut matches = Vec::new();
    for line in stdout.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let is_context = match v["type"].as_str() {
            Some("match") => false,
            Some("context") => true,
            _ => continue,
        };
        let data = &v["data"];
        let Some(path_str) = data["path"]["text"].as_str() else {
            continue;
        };
        let Some(line_number) = data["line_number"].as_u64() else {
            continue;
        };
        let text = data["lines"]["text"].as_str().unwrap_or("").to_string();

        matches.push(GrepMatch {
            file: relativize_path(path_str, &root_str).to_string(),
            line: line_number,
            text,
            is_context,
        });
    }
    matches
}

/// Find the innermost AST node containing the given line (from pre-loaded nodes).
fn find_containing_node_in(
    nodes: &[queries::NodeResult],
    line: u64,
) -> Option<(String, String, i64, i64)> {
    let mut best: Option<&queries::NodeResult> = None;
    for node in nodes {
        if node.start_line as u64 <= line && line <= node.end_line as u64 {
            match best {
                None => best = Some(node),
                Some(prev) => {
                    let prev_span = prev.end_line - prev.start_line;
                    let cur_span = node.end_line - node.start_line;
                    if cur_span < prev_span {
                        best = Some(node);
                    }
                }
            }
        }
    }

    best.map(|n| {
        let short_type = match n.node_type.as_str() {
            "function" | "method" => "fn",
            other => other,
        };
        let name = n
            .qualified_name
            .as_deref()
            .unwrap_or(&n.name)
            .to_string();
        (short_type.to_string(), name, n.start_line, n.end_line)
    })
}

// --- search subcommand ---

/// CLI arguments for the `search` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp search",
          about = "FTS5 text search by concept (CLI is FTS-only; MCP adds vector+RRF fusion)")]
pub struct SearchArgs {
    /// Search query (concept keywords)
    pub query: String,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Compact output
    #[arg(long)]
    pub compact: bool,
    /// Filter by language
    #[arg(long)]
    pub language: Option<String>,
    /// Filter by node type: fn, class, struct, enum, trait, type, const, var
    #[arg(long = "node-type")]
    pub node_type: Option<String>,
    // --limit and --top-k are the same arg (alias); supplying both is a clap
    // duplicate-arg error. clamp(1,100) stays in the handler; clap parse-errors
    // (exit 2) on a non-numeric value, replacing the old warn+fallback.
    /// Limit results (default: 20, max: 100); alias: --top-k
    #[arg(long, alias = "top-k")]
    pub limit: Option<i64>,
}

/// FTS5 semantic search.
///
/// Output format:
/// ```text
/// fn McpServer::handle_tool_call  src/mcp/server.rs:350-420  (name: &str, params: Value) -> Result<Value>
/// ```
pub fn cmd_search(project_root: &Path, args: SearchArgs) -> Result<()> {
    // clap accepts an empty-string positional (e.g. an unset `search "$X"`);
    // preserve the non-empty query guard with the exact Usage string.
    let query = args.query.as_str();
    if query.is_empty() {
        anyhow::bail!("Usage: code-graph-mcp search <query> [--json] [--limit N] [--top-k N] [--language <lang>] [--compact]");
    }

    let json_mode = args.json;
    let compact = args.compact;
    let language_filter = args.language.as_deref();
    let node_type_filter = args.node_type.as_deref();
    let limit: i64 = args.limit.unwrap_or(20).clamp(1, 100);

    // Validate --node-type up-front: unknown alias normalizes to an empty Vec
    // and silently filters every node away (see ast-search same fix).
    if let Some(ntf) = node_type_filter {
        if crate::domain::normalize_type_filter(ntf).is_empty() {
            anyhow::bail!(
                "Unknown node-type filter: '{}'. Valid: fn, class, struct, enum, trait, type, const, var",
                ntf
            );
        }
    }

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    // Over-fetch so post-fetch filtering can still return `limit` results. The filter
    // below ALWAYS drops <module>/test symbols, and a language/node-type filter can drop
    // far more — a selective filter over a minority language/type silently under-returns.
    // Widen the pool when a filter is active (shared policy with MCP semantic_code_search
    // via search_fetch_count); the unfiltered value stays (limit*4).max(20).
    let filtered = language_filter.is_some() || node_type_filter.is_some();
    let fetch_limit = crate::domain::search_fetch_count(limit, filtered);
    let fts_result = queries::fts5_search(conn, query, fetch_limit)?;
    if fts_result.nodes.is_empty() {
        if json_mode {
            println!("[]");
        }
        eprintln!("[code-graph] No results for: {}", query);
        // Hint: if query looks like code syntax, suggest ast-search
        if query.contains('(') || query.contains(')') || query.contains("->") || query.contains("::") || query.contains('<') {
            // Replace non-word chars with spaces, collapse multiple spaces, extract clean keywords
            let clean: String = query.chars()
                .map(|c| if c.is_alphanumeric() || c == '_' { c } else { ' ' })
                .collect();
            let keywords: Vec<&str> = clean.split_whitespace().collect();
            if !keywords.is_empty() {
                eprintln!("  Tip: For structural queries, try: code-graph-mcp ast-search --type fn --returns \"{}\"",
                    keywords.join(" "));
            }
        }
        return Ok(());
    }

    let node_ids: Vec<i64> = fts_result.nodes.iter().map(|n| n.id).collect();
    let nodes_with_files = queries::get_nodes_with_files_by_ids(conn, &node_ids)?;

    // Build id->NodeWithFile map preserving FTS rank order
    let nwf_map: std::collections::HashMap<i64, &queries::NodeWithFile> = nodes_with_files
        .iter()
        .map(|nwf| (nwf.node.id, nwf))
        .collect();

    // Normalize node_type filter for matching
    let normalized_node_types: Vec<&'static str> = node_type_filter
        .map(normalize_type_filter)
        .unwrap_or_default();

    // Filter by language, node_type, and skip test/module nodes (align with MCP behavior).
    // Count language/node_type drops separately so an over-selective filter that empties
    // the result set can say so (vs a generic "no results"), mirroring MCP's filter hint.
    let mut filtered_nodes: Vec<&queries::NodeResult> = Vec::new();
    let mut dropped_by_filter = 0usize;
    for n in &fts_result.nodes {
        // Skip <module>/<external> placeholders and test symbols, consistent with
        // MCP semantic_code_search (domain::is_skippable_result = the shared triad;
        // the CLI path previously omitted the <external> leg the MCP path applied).
        let fp = nwf_map.get(&n.id).map(|nwf| nwf.file_path.as_str()).unwrap_or("");
        if crate::domain::is_skippable_result(&n.node_type, &n.name, fp) { continue; }
        if let Some(lang) = language_filter {
            let lang_ok = nwf_map.get(&n.id)
                .and_then(|nwf| nwf.language.as_deref())
                .map(|l| l.eq_ignore_ascii_case(lang))
                .unwrap_or(false);
            if !lang_ok { dropped_by_filter += 1; continue; }
        }
        if !normalized_node_types.is_empty()
            && !normalized_node_types.iter().any(|t| n.node_type == *t)
        {
            dropped_by_filter += 1;
            continue;
        }
        filtered_nodes.push(n);
        if filtered_nodes.len() >= limit as usize { break; }
    }

    if filtered_nodes.is_empty() {
        if json_mode {
            println!("[]");
        }
        if filtered && dropped_by_filter > 0 {
            // Matches existed but the language/node_type filter removed them all — the
            // index has hits, just not of this language/type. (CLI stdout stays `[]`.)
            eprintln!(
                "[code-graph] No results for: {} — {} candidate(s) matched the query but were removed by the active filter (language: {}{}). Broaden or clear the filter.",
                query, dropped_by_filter, language_filter.unwrap_or("any"),
                node_type_filter.map(|t| format!(", node-type: {t}")).unwrap_or_default()
            );
        } else {
            eprintln!("[code-graph] No results for: {} (language: {})", query, language_filter.unwrap_or("any"));
        }
        return Ok(());
    }

    // Build file_path map from filtered results
    let file_map: std::collections::HashMap<i64, &str> = nodes_with_files
        .iter()
        .map(|nwf| (nwf.node.id, nwf.file_path.as_str()))
        .collect();

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        let results: Vec<serde_json::Value> = filtered_nodes
            .iter()
            .map(|n| {
                let fp = file_map.get(&n.id).copied().unwrap_or("?");
                serde_json::json!({
                    "node_id": n.id,
                    "type": n.node_type,
                    "name": n.qualified_name.as_deref().unwrap_or(&n.name),
                    "file_path": fp,
                    "start_line": n.start_line,
                    "end_line": n.end_line,
                    "signature": n.signature,
                    "return_type": n.return_type,
                    "param_types": n.param_types,
                })
            })
            .collect();
        writeln!(stdout, "{}", serde_json::to_string(&results)?)?;
        return Ok(());
    }

    for node in &filtered_nodes {
        let fp = file_map.get(&node.id).copied().unwrap_or("?");
        if compact {
            let name = node.qualified_name.as_deref().unwrap_or(&node.name);
            writeln!(stdout, "{}  {}:{}-{}", name, fp, node.start_line, node.end_line)?;
        } else {
            writeln!(stdout, "{}", format_node_compact(node, fp))?;
        }
    }

    if fts_result.or_fallback {
        eprintln!("[code-graph] Note: AND match insufficient, showing OR results (broader match).");
    }
    if !json_mode {
        eprintln!("[code-graph] Tip: CLI search is FTS5-only. For vector+RRF hybrid recall use MCP semantic_code_search.");
    }

    Ok(())
}

// --- ast-search subcommand ---

/// CLI arguments for the `ast-search` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp ast-search",
          about = "Structured search with --type/--returns/--params filters")]
pub struct AstSearchArgs {
    /// Search query (optional if a --type/--returns/--params filter is given)
    pub query: Option<String>,
    /// Filter by node type: fn, class, struct, enum, trait, type, const, var
    #[arg(long = "type")]
    pub type_filter: Option<String>,
    /// Filter by return type
    #[arg(long)]
    pub returns: Option<String>,
    /// Filter by parameter text
    #[arg(long)]
    pub params: Option<String>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Limit results (default: 20, max: 100)
    #[arg(long)]
    pub limit: Option<usize>,
}

/// Structured AST search: FTS5 + column filtering.
///
/// Flags: --type <type>, --returns <type>, --params <text>
pub fn cmd_ast_search(project_root: &Path, args: AstSearchArgs) -> Result<()> {
    // clap accepts an empty-string positional; treat "" as "no query" (the old
    // .filter(|q| !q.is_empty())) so the query-or-filter requirement still fires.
    let query = args.query.as_deref().filter(|q| !q.is_empty());

    let type_filter = args.type_filter.as_deref();
    let returns_filter = args.returns.as_deref();
    let params_filter = args.params.as_deref();
    let json_mode = args.json;
    let limit: usize = args.limit.unwrap_or(20).clamp(1, 100);

    // Require either a query or at least one structural filter
    let has_filters = type_filter.is_some() || returns_filter.is_some() || params_filter.is_some();
    if query.is_none() && !has_filters {
        anyhow::bail!(
            "Usage: code-graph-mcp ast-search <query> [--type fn|class|...] [--returns type] [--params text] [--json]\n\
             Either a query or at least one filter (--type, --returns, --params) is required."
        );
    }

    // Validate --type up-front: an unknown alias normalizes to an empty Vec,
    // which silently filters every node away. Surface as an error so the user
    // doesn't read "No results matching filters" and assume the index is empty.
    if let Some(tf) = type_filter {
        if crate::domain::normalize_type_filter(tf).is_empty() {
            anyhow::bail!(
                "Unknown type filter: '{}'. Valid: fn, class, struct, enum, trait, type, const, var",
                tf
            );
        }
    }

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    // Two paths: filter-only (direct SQL) vs query+filter (FTS5 then filter)
    let results_with_files: Vec<queries::NodeWithFile> = if let Some(query) = query {
        // FTS5 search then filter in Rust
        let fts_result = queries::fts5_search(conn, query, (limit * 4) as i64)?;
        if fts_result.nodes.is_empty() {
            if json_mode {
                println!("{}", serde_json::json!({"results": [], "count": 0}));
            }
            eprintln!("[code-graph] No results for: {}", query);
            return Ok(());
        }

        let node_ids: Vec<i64> = fts_result.nodes.iter().map(|n| n.id).collect();
        let all = queries::get_nodes_with_files_by_ids(conn, &node_ids)?;

        // Preserve FTS5 rank order, then apply filters
        let id_order: std::collections::HashMap<i64, usize> = node_ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let mut sorted = all;
        sorted.sort_by_key(|nwf| id_order.get(&nwf.node.id).copied().unwrap_or(usize::MAX));

        sorted
            .into_iter()
            .filter(|nwf| {
                let n = &nwf.node;
                if let Some(tf) = type_filter {
                    let normalized = normalize_type_filter(tf);
                    if !normalized.iter().any(|t| n.node_type == *t) {
                        return false;
                    }
                }
                if let Some(rf) = returns_filter {
                    match &n.return_type {
                        Some(rt) => {
                            if !rt.to_lowercase().contains(&rf.to_lowercase()) {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
                if let Some(pf) = params_filter {
                    match &n.param_types {
                        Some(pt) => {
                            if !pt.to_lowercase().contains(&pf.to_lowercase()) {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
                true
            })
            .take(limit)
            .collect()
    } else {
        // Filter-only: direct SQL query
        let normalized_types: Vec<&str>;
        let type_refs = if let Some(tf) = type_filter {
            normalized_types = normalize_type_filter(tf).into_iter().collect();
            Some(normalized_types.as_slice())
        } else {
            None
        };
        queries::get_nodes_with_files_by_filters(
            conn, type_refs, returns_filter, params_filter, None, limit,
        )?
    };

    if results_with_files.is_empty() {
        if json_mode {
            println!("{}", serde_json::json!({"results": [], "count": 0}));
        }
        eprintln!("[code-graph] No results matching filters.");
        return Ok(());
    }

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        let results: Vec<serde_json::Value> = results_with_files
            .iter()
            .map(|nwf| {
                let n = &nwf.node;
                serde_json::json!({
                    "node_id": n.id,
                    "type": n.node_type,
                    "name": n.qualified_name.as_deref().unwrap_or(&n.name),
                    "file_path": &nwf.file_path,
                    "start_line": n.start_line,
                    "end_line": n.end_line,
                    "return_type": n.return_type,
                    "param_types": n.param_types,
                })
            })
            .collect();
        // Envelope matches MCP ast_search: {results, count}
        let envelope = serde_json::json!({
            "results": results,
            "count": results_with_files.len(),
        });
        writeln!(stdout, "{}", serde_json::to_string(&envelope)?)?;
        return Ok(());
    }

    for nwf in &results_with_files {
        writeln!(stdout, "{}", format_node_compact(&nwf.node, &nwf.file_path))?;
    }
    Ok(())
}

/// Normalize type filter shorthand: fn → function/method, class → class/struct, etc.
fn normalize_type_filter(input: &str) -> Vec<&'static str> {
    let result = crate::domain::normalize_type_filter(input);
    if result.is_empty() {
        eprintln!(
            "[code-graph] Unknown type filter: '{}'. Valid: fn, class, struct, enum, trait, type, const, var",
            input
        );
    }
    result
}

// --- callgraph subcommand ---

/// CLI arguments for the `callgraph` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp callgraph",
          about = "Show call graph (callers/callees)")]
pub struct CallgraphArgs {
    /// Symbol name to analyze
    pub symbol: String,
    // --direction stays an in-handler String (NOT a clap ValueEnum) so the exact
    // "must be one of: callers, callees, both" exit-1 message is preserved.
    /// Direction: callers, callees, or both
    #[arg(long, default_value = "both")]
    pub direction: String,
    // .max(1) only (NOT clamp) stays in the handler: the engine caps depth and
    // reports requested vs effective separately, so the CLI must not pre-rewrite it.
    /// Max traversal depth (engine caps internally; default: 3)
    #[arg(long, default_value_t = 3)]
    pub depth: i32,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Compact output
    #[arg(long)]
    pub compact: bool,
    /// Show test callers/callees (hidden by default)
    #[arg(long)]
    pub include_tests: bool,
    /// Disambiguate same-name symbols by file path
    #[arg(long)]
    pub file: Option<String>,
    /// Minimum edge-resolution confidence to FOLLOW: extracted, inferred, or
    /// ambiguous. Default 'inferred' hides the ambiguous by-name fan-out (a
    /// method name shared by many defs resolving to all of them); pass
    /// 'ambiguous' to show every edge.
    #[arg(long = "min-confidence")]
    pub min_confidence: Option<String>,
}

/// Call graph display.
///
/// Output format:
/// ```text
/// handle_tool_call (src/mcp/server.rs:350)
///   ← called by: process_message (src/mcp/server.rs:130)
///   → calls: tool_semantic_search (src/mcp/server.rs:1360)
/// ```
pub fn cmd_callgraph(project_root: &Path, args: CallgraphArgs) -> Result<()> {
    // clap accepts an empty-string positional; preserve the non-empty guard.
    let raw_symbol = args.symbol.as_str();
    if raw_symbol.is_empty() {
        anyhow::bail!("Usage: code-graph-mcp callgraph <symbol> [--direction callers|callees|both] [--depth N] [--file <path>] [--json]");
    }

    let direction = args.direction.as_str();
    if !matches!(direction, "callers" | "callees" | "both") {
        anyhow::bail!("--direction must be one of: callers, callees, both");
    }
    let depth: i32 = args.depth.max(1);
    let json_mode = args.json;
    let compact = args.compact;
    let include_tests = args.include_tests;
    let explicit_file_owned: Option<String> = match args.file.as_deref() {
        Some(f) => Some(normalize_user_path(project_root, f)?),
        None => None,
    };
    let explicit_file = explicit_file_owned.as_deref();

    // Confidence floor: default 'inferred' hides the ambiguous by-name fan-out
    // (the known false-positive class) from the traversal; --min-confidence
    // ambiguous restores every edge. Validated at entry, mirroring `refs`.
    let min_conf_tier: &'static str = match args.min_confidence.as_deref() {
        None | Some("") => crate::domain::CONF_INFERRED,
        Some(c) => crate::domain::normalize_confidence(c).ok_or_else(|| {
            anyhow::anyhow!(
                "--min-confidence must be one of: extracted, inferred, ambiguous (got '{}')",
                c
            )
        })?,
    };
    let min_conf_rank = crate::domain::confidence_rank(min_conf_tier);

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let (symbol, resolved_file) = resolve_qualified_symbol(conn, raw_symbol, explicit_file);
    let file_filter = explicit_file.or(resolved_file.as_deref());

    // Exact-name ambiguity guard: a bare name with ≥2 non-test definitions
    // (cross-file OR same-file overloads) would silently merge call graphs.
    // Shared with MCP via crate::resolve so both surfaces agree (audit #6).
    if file_filter.is_none() {
        if let Some(cands) = crate::resolve::detect_ambiguity(conn, symbol)? {
            emit_exact_ambiguity(symbol, &cands, json_mode);
        }
    }

    let mut result = crate::graph::query::get_call_graph_filtered(conn, symbol, direction, depth, file_filter, min_conf_rank)?;
    // Fuzzy auto-resolve: if exact-name lookup returned nothing (or only the seed
    // node with no edges) and no --file was specified, promote a unique fuzzy
    // match. Matches MCP get_call_graph behavior.
    let has_edges = result.nodes.iter().any(|n| n.depth > 0);
    let has_seed = result.nodes.iter().any(|n| n.depth == 0);
    let mut resolved_symbol: String = symbol.to_string();
    if !(has_edges || (has_seed && file_filter.is_some())) {
        match resolve_fuzzy_name_cli(conn, symbol)? {
            CliFuzzyResolution::Unique(resolved) => {
                if resolved != symbol {
                    result = crate::graph::query::get_call_graph_filtered(conn, &resolved, direction, depth, file_filter, min_conf_rank)?;
                    eprintln!("[code-graph] Resolved '{}' → '{}'", symbol, resolved);
                }
                resolved_symbol = resolved;
            }
            CliFuzzyResolution::Ambiguous(cands) => {
                if json_mode {
                    let sugg: Vec<serde_json::Value> = cands.iter().take(5).map(|c| serde_json::json!({
                        "name": c.name, "file_path": c.file_path, "type": c.node_type,
                        "node_id": c.node_id, "start_line": c.start_line,
                    })).collect();
                    println!("{}", serde_json::json!({
                        "results": [],
                        "error": format!("Ambiguous symbol '{}': {} matches", symbol, cands.len()),
                        "candidates": sugg,
                    }));
                } else {
                    eprintln!("[code-graph] Ambiguous symbol '{}': {} matches. Did you mean:", symbol, cands.len());
                    for c in cands.iter().take(5) {
                        eprintln!("  {} ({}) in {} [node_id {}]", c.name, c.node_type, c.file_path, c.node_id);
                    }
                }
                std::process::exit(1);
            }
            CliFuzzyResolution::NotFound => { /* fall through to empty-nodes branch */ }
        }
    }
    // Intentional shadow: if fuzzy promoted, `resolved_symbol` holds the resolved
    // name; otherwise it still equals the original input (initialized at
    // `symbol.to_string()` above). Either way, `symbol` below is the correct
    // identifier to print in the "No call graph results" eprintln.
    let symbol = resolved_symbol.as_str();
    if result.nodes.is_empty() {
        if json_mode {
            println!("{{\"results\":[]}}");
        }
        eprintln!("[code-graph] No call graph results for: {}", symbol);
        std::process::exit(1);
    }

    // Filter test callers unless --include-tests is set.
    // The seed (depth=0) is kept here because the human-readable renderer
    // below uses it as the tree root. The JSON path filters it separately
    // for parity with MCP `get_call_graph` (which excludes the seed).
    let (display_nodes, test_count) = if include_tests {
        (result.nodes.iter().collect::<Vec<_>>(), 0usize)
    } else {
        let mut display = Vec::new();
        let mut tests = 0usize;
        for n in &result.nodes {
            if n.depth > 0
                && matches!(n.direction, crate::graph::query::Direction::Callers)
                && crate::domain::is_test_symbol(&n.name, &n.file_path)
            {
                tests += 1;
            } else {
                display.push(n);
            }
        }
        (display, tests)
    };

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        // Drop the seed (depth=0) — parity with MCP `get_call_graph`
        // (`format_call_graph_response` filters `n.depth > 0`). With
        // `direction=both` the seed appears twice (once per direction),
        // inflating result counts.
        let results: Vec<serde_json::Value> = display_nodes
            .iter()
            .filter(|n| n.depth > 0)
            .map(|n| {
                serde_json::json!({
                    "node_id": n.node_id,
                    "name": n.name,
                    "type": n.node_type,
                    "file_path": n.file_path,
                    "depth": n.depth,
                    "direction": n.direction.as_str(),
                    "parent_id": n.parent_id,
                })
            })
            .collect();
        let mut output = serde_json::json!({ "results": results });
        if test_count > 0 {
            output["test_callers_hidden"] = serde_json::json!(test_count);
        }
        if result.limit_hit {
            output["limit_hit"] = serde_json::json!(true);
        }
        if result.depth_capped {
            output["depth_capped"] = serde_json::json!(true);
            output["effective_max_depth"] = serde_json::json!(result.effective_max_depth);
            output["requested_max_depth"] = serde_json::json!(result.requested_max_depth);
        }
        if result.suppressed_ambiguous > 0 {
            output["ambiguous_edges_hidden"] = serde_json::json!(result.suppressed_ambiguous);
        }
        writeln!(stdout, "{}", serde_json::to_string(&output)?)?;
        return Ok(());
    }

    // Find root node (depth 0)
    let root = display_nodes.iter().find(|n| n.depth == 0);
    if let Some(root) = root {
        writeln!(stdout, "{} ({})", root.name, root.file_path)?;
    } else {
        return Ok(());
    }
    let root_id = root.unwrap().node_id;

    // Build parent_id → children map per direction, so depth-N nodes nest under
    // their *actual* depth-(N-1) parent rather than visually clumping under the
    // last sibling. Same direction filter so callers/callees subtrees stay
    // separate when --direction=both.
    use std::collections::HashMap;
    let mut children: HashMap<(i64, &'static str), Vec<&crate::graph::query::CallGraphNode>> =
        HashMap::new();
    let mut dedup = std::collections::HashSet::new();
    for n in &display_nodes {
        if n.depth == 0 {
            continue;
        }
        // Dedup cfg-gated duplicates (same name+file+direction+depth, different node_id).
        if !dedup.insert((&n.name, &n.file_path, n.direction.as_str(), n.depth)) {
            continue;
        }
        let parent = n.parent_id.unwrap_or(root_id);
        children
            .entry((parent, n.direction.as_str()))
            .or_default()
            .push(n);
    }

    fn render_subtree<W: std::io::Write>(
        out: &mut W,
        children: &HashMap<(i64, &'static str), Vec<&crate::graph::query::CallGraphNode>>,
        parent_id: i64,
        direction: &'static str,
        compact: bool,
    ) -> std::io::Result<()> {
        let arrow = match direction {
            "callers" => "←",
            _ => "→",
        };
        let arrow_text = match direction {
            "callers" => "← called by",
            _ => "→ calls",
        };
        if let Some(kids) = children.get(&(parent_id, direction)) {
            for n in kids {
                let indent = "  ".repeat(n.depth as usize);
                if compact {
                    writeln!(out, "{}{} {} ({})", indent, arrow, n.name, n.file_path)?;
                } else {
                    writeln!(
                        out,
                        "{}{}: {} ({}) [{}]",
                        indent, arrow_text, n.name, n.file_path, n.node_type
                    )?;
                }
                render_subtree(out, children, n.node_id, direction, compact)?;
            }
        }
        Ok(())
    }

    render_subtree(&mut stdout, &children, root_id, "callers", compact)?;
    render_subtree(&mut stdout, &children, root_id, "callees", compact)?;

    if test_count > 0 {
        writeln!(stdout, "  ({} test callers hidden, use --include-tests to show)", test_count)?;
    }
    if result.limit_hit {
        writeln!(
            stdout,
            "  ⚠ result truncated: hit row limit ({} rows) — more callers/callees may exist; pick a leaf and re-query",
            crate::graph::query::CALL_GRAPH_ROW_LIMIT,
        )?;
    }
    if result.depth_capped {
        writeln!(
            stdout,
            "  ⚠ depth capped to {} (requested {}) — deeper chains may exist",
            result.effective_max_depth, result.requested_max_depth,
        )?;
    }
    if result.suppressed_ambiguous > 0 {
        writeln!(
            stdout,
            "  ({} direct ambiguous by-name edge(s) hidden — use --min-confidence ambiguous to show)",
            result.suppressed_ambiguous,
        )?;
    }

    Ok(())
}

// --- impact subcommand ---

/// CLI arguments for the `impact` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp impact",
          about = "Impact analysis (callers, routes, risk level)")]
pub struct ImpactArgs {
    /// Symbol name to analyze
    pub symbol: String,
    // clamp(1,20) stays in the handler; clap parse-errors (exit 2) on non-numeric.
    /// Max traversal depth (default: 3)
    #[arg(long, default_value_t = 3)]
    pub depth: i32,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Disambiguate same-name symbols by file path
    #[arg(long)]
    pub file: Option<String>,
    // --change-type stays an in-handler String (NOT a clap ValueEnum) so the exact
    // "must be one of: signature, behavior, remove" exit-1 message is preserved.
    /// Change type: signature, behavior, or remove
    #[arg(long = "change-type", default_value = "behavior")]
    pub change_type: String,
    /// Minimum caller-edge confidence to count toward risk: extracted, inferred,
    /// or ambiguous. Default 'inferred' folds the ambiguous by-name fan-out out
    /// of the blast radius (the excluded count is still reported); pass
    /// 'ambiguous' to count every resolved caller.
    #[arg(long = "min-confidence")]
    pub min_confidence: Option<String>,
}

/// Impact analysis.
///
/// Shows callers with route info and risk level.
pub fn cmd_impact(project_root: &Path, args: ImpactArgs) -> Result<()> {
    // clap accepts an empty-string positional; preserve the non-empty guard.
    let raw_symbol = args.symbol.as_str();
    if raw_symbol.is_empty() {
        anyhow::bail!("Usage: code-graph-mcp impact <symbol> [--depth N] [--file <path>] [--change-type signature|behavior|remove] [--json]");
    }

    let depth: i32 = args.depth.clamp(1, 20);
    let json_mode = args.json;
    let explicit_file_owned: Option<String> = match args.file.as_deref() {
        Some(f) => Some(normalize_user_path(project_root, f)?),
        None => None,
    };
    let explicit_file = explicit_file_owned.as_deref();
    let change_type = args.change_type.as_str();
    if !matches!(change_type, "signature" | "behavior" | "remove") {
        anyhow::bail!("--change-type must be one of: signature, behavior, remove");
    }
    // Confidence floor for caller traversal: default 'inferred' folds the
    // ambiguous by-name fan-out out of the risk count; --min-confidence ambiguous
    // counts every caller. The excluded count is disclosed below so a folded
    // ambiguous caller never silently under-states risk.
    let min_conf_tier: &'static str = match args.min_confidence.as_deref() {
        None | Some("") => crate::domain::CONF_INFERRED,
        Some(c) => crate::domain::normalize_confidence(c).ok_or_else(|| {
            anyhow::anyhow!(
                "--min-confidence must be one of: extracted, inferred, ambiguous (got '{}')",
                c
            )
        })?,
    };
    let min_conf_rank = crate::domain::confidence_rank(min_conf_tier);

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let (symbol, resolved_file) = resolve_qualified_symbol(conn, raw_symbol, explicit_file);
    let file_filter = explicit_file.or(resolved_file.as_deref());

    // Verify symbol exists before running impact analysis
    let symbol_nodes = queries::get_nodes_by_name(conn, symbol)?;
    if symbol_nodes.is_empty() {
        if json_mode {
            println!("{}", serde_json::json!({"error": "Symbol not found", "symbol": symbol}));
        }
        eprintln!("[code-graph] Symbol not found: {}", symbol);
        let candidates = queries::find_functions_by_fuzzy_name(conn, symbol)?;
        if !candidates.is_empty() {
            eprintln!("[code-graph] Did you mean:");
            for c in candidates.iter().take(5) {
                eprintln!("  {} ({}) in {}", c.name, c.node_type, c.file_path);
            }
        }
        std::process::exit(1);
    }

    // Exact-name ambiguity guard: a bare name with ≥2 non-test definitions
    // (cross-file OR same-file overloads) would silently merge callers across
    // both, misreporting risk/blast radius. Shared with MCP via crate::resolve.
    if file_filter.is_none() {
        if let Some(cands) = crate::resolve::detect_ambiguity(conn, symbol)? {
            emit_exact_ambiguity(symbol, &cands, json_mode);
        }
    }

    let callers = queries::get_callers_with_route_info(conn, symbol, file_filter, depth, min_conf_rank)?;
    // Ambiguous callers folded out of the blast radius by the confidence floor,
    // counted across the whole returned frontier (seed direct + every kept
    // caller's pruned callers) so a TRANSITIVE ambiguous caller of a
    // uniquely-named symbol is disclosed too. Surfaced (not silently dropped) so a
    // folded real caller never under-states risk; --min-confidence ambiguous counts them.
    let caller_ids: Vec<i64> = callers.iter().filter(|c| c.depth > 0).map(|c| c.node_id).collect();
    let ambiguous_callers_excluded = crate::graph::query::count_suppressed_seed_edges(
        conn, symbol, file_filter, crate::graph::query::Direction::Callers, min_conf_rank,
    )? + crate::graph::query::count_suppressed_into(conn, &caller_ids, min_conf_rank)?;

    // Partition prod/test callers (deduped by name,file,depth), count routes/files,
    // and assess risk via the surface-shared classifier — the MCP impact tool runs
    // the identical rule. crate::graph::impact owns the prod-only route policy (a
    // test-only endpoint is not a production blast radius) and the dedup.
    let is_function_like = symbol_nodes
        .iter()
        .any(|n| crate::domain::is_function_node_type(n.node_type.as_str()));
    let impact = crate::graph::impact::classify_impact(&callers, change_type, is_function_like);
    let prod_callers = &impact.prod_callers;
    let routes = &impact.route_callers;
    let direct_callers = prod_callers.iter().filter(|c| c.depth == 1).count();
    let risk = impact.risk_level;

    // Value references (REL_REFERENCES): callbacks / fn-pointers / type-position
    // couplings the call graph misses. Prod sources, deduped by referencing symbol.
    // Mirrors the MCP impact tool (server/tools/advanced.rs) so both surfaces report
    // the same signal — CLI/MCP parity. NEVER folded into the caller counts above.
    let value_references = {
        use std::collections::HashSet;
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for n in &symbol_nodes {
            for r in queries::get_incoming_references(conn, n.id, Some(crate::domain::REL_REFERENCES))? {
                if !crate::domain::is_test_symbol(&r.name, &r.file_path) {
                    seen.insert((r.name, r.file_path));
                }
            }
        }
        seen.len()
    };

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        let mut result = serde_json::json!({
            "symbol": symbol,
            "risk": risk,
            "direct_callers": direct_callers,
            "total_callers": prod_callers.len(),
            "tests_affected": impact.test_count,
            "affected_files": impact.affected_files,
            "affected_routes": routes.len(),
            "value_references": value_references,
            "callers": prod_callers.iter().map(|c| serde_json::json!({
                "name": c.name,
                "type": c.node_type,
                "file": c.file_path,
                "depth": c.depth,
                "route": c.route_info,
            })).collect::<Vec<_>>(),
            // Covering tests behind `tests_affected` — name + file is enough for a
            // hook to build a runnable test command (e.g. `cargo test`/`pytest`).
            // Full list (not capped here); display-side capping is the surface's job.
            "test_callers": impact.test_callers.iter().map(|c| serde_json::json!({
                "name": c.name,
                "file": c.file_path,
            })).collect::<Vec<_>>(),
        });
        if let Some(warning) = impact.type_warning {
            result["warning"] = serde_json::json!(warning);
        }
        if ambiguous_callers_excluded > 0 {
            result["ambiguous_callers_excluded"] = serde_json::json!(ambiguous_callers_excluded);
            result["ambiguous_note"] = serde_json::json!(format!(
                "{} direct caller(s) resolved only by ambiguous name-match were excluded from this risk assessment; actual blast radius may be larger. Re-run with --min-confidence ambiguous to include them.",
                ambiguous_callers_excluded
            ));
        }
        writeln!(stdout, "{}", serde_json::to_string(&result)?)?;
        return Ok(());
    }

    writeln!(stdout, "Impact: {} — Risk: {}", symbol, risk)?;
    if let Some(warning) = impact.type_warning {
        writeln!(stdout, "  (warning: {})", warning)?;
    }
    writeln!(
        stdout,
        "  {} direct callers, {} total, {} files, {} routes ({} tests affected)",
        direct_callers,
        prod_callers.len(),
        impact.affected_files,
        routes.len(),
        impact.test_count
    )?;
    if ambiguous_callers_excluded > 0 {
        writeln!(
            stdout,
            "  ⚠ {} ambiguous by-name caller(s) excluded from risk — actual blast radius may be larger; use --min-confidence ambiguous to include",
            ambiguous_callers_excluded
        )?;
    }
    if value_references > 0 {
        writeln!(
            stdout,
            "  {} value reference(s) — callbacks / fn-pointers / type positions (not call-graph callers)",
            value_references
        )?;
    }

    if !routes.is_empty() {
        writeln!(stdout, "Routes:")?;
        for r in routes {
            let route_str = r.route_info.as_deref().unwrap_or("?");
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(route_str) {
                let method = v["method"].as_str().unwrap_or("?");
                let path = v["path"].as_str().unwrap_or("?");
                writeln!(stdout, "  {} {} → {} ({})", method, path, r.name, r.file_path)?;
            } else {
                writeln!(stdout, "  {} → {} ({})", route_str, r.name, r.file_path)?;
            }
        }
    }

    if !prod_callers.is_empty() {
        writeln!(stdout, "Callers:")?;
        for c in prod_callers {
            let indent = "  ".repeat(c.depth as usize);
            writeln!(stdout, "{}{}  ({}) {}", indent, c.name, c.node_type, c.file_path)?;
        }
    }

    Ok(())
}

// --- affected subcommand ---

#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp affected",
          about = "Changed files → test files to re-run (+ full blast radius)")]
pub struct AffectedArgs {
    /// Changed file paths (relative to project root, or absolute under it)
    pub files: Vec<String>,
    /// Also read newline-separated paths from stdin (e.g. `git diff --name-only | …`)
    #[arg(long)]
    pub stdin: bool,
    /// Max reverse-dependency traversal depth (default: 10; clamped 1..=10)
    #[arg(long, default_value_t = 10)]
    pub depth: i32,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// Reverse-impact: given changed files, list the test files that transitively
/// depend on them (primary) plus the full affected-file set (secondary).
pub fn cmd_affected(project_root: &Path, args: AffectedArgs) -> Result<()> {
    use std::collections::{BTreeMap, HashSet};
    use std::io::Read;

    let depth = args.depth.clamp(1, 10);

    // 1. Gather raw paths: positional + optional stdin. read_to_end + lossy UTF-8 so a
    //    non-UTF-8 path (legal on Linux) cannot break the --json envelope (F6).
    let mut raw: Vec<String> = args.files.clone();
    if args.stdin {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        raw.extend(
            String::from_utf8_lossy(&buf)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
        );
    }

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    // 2. Classify each raw input. `changed` holds normalized, INDEXED paths only;
    //    `not_indexed` reports the user's RAW input (one consistent form, F7). Inputs
    //    that normalize to "" (e.g. `.` / project root) are skipped — not a file (F2).
    let mut changed: Vec<String> = Vec::new();
    let mut not_indexed: Vec<String> = Vec::new();
    let mut seen_changed: HashSet<String> = HashSet::new();
    for r in &raw {
        let norm = match normalize_user_path(project_root, r) {
            Ok(p) => p,
            Err(_) => {
                if !not_indexed.contains(r) { not_indexed.push(r.clone()); }
                continue;
            }
        };
        if norm.is_empty() {
            continue;
        }
        if !queries::file_is_indexed(conn, &norm)? {
            if !not_indexed.contains(r) { not_indexed.push(r.clone()); }
            continue;
        }
        if seen_changed.insert(norm.clone()) {
            changed.push(norm);
        }
    }

    // 3. Union reverse dependents across all changed files over EVERY dependency
    //    relation (imports∪calls∪references∪implements∪inherits, F1), keeping only
    //    language-compatible dependents (F10) and excluding the changed files
    //    themselves from the blast radius (F4).
    let changed_set: HashSet<&str> = changed.iter().map(|s| s.as_str()).collect();
    let mut affected: BTreeMap<String, i32> = BTreeMap::new();
    for f in &changed {
        for (dep_path, dep_depth) in queries::get_reverse_dependents(conn, f, depth)? {
            if !crate::utils::config::is_compatible_lang(f, &dep_path) {
                continue;
            }
            if changed_set.contains(dep_path.as_str()) {
                continue;
            }
            affected
                .entry(dep_path)
                .and_modify(|d| if dep_depth < *d { *d = dep_depth })
                .or_insert(dep_depth);
        }
    }

    // 4. Primary output: test files among the dependents ∪ changed files that are
    //    themselves tests. `changed` is indexed-only, so a nonexistent test path can no
    //    longer land in both `tests` and `not_indexed` (F3).
    let mut tests: Vec<String> = affected
        .keys()
        .filter(|p| crate::domain::is_test_path(p))
        .cloned()
        .collect();
    for f in &changed {
        if crate::domain::is_test_path(f) && !tests.contains(f) {
            tests.push(f.clone());
        }
    }
    tests.sort();

    // 5. Emit (same-shape JSON on every path — empty included).
    let mut stdout = std::io::stdout().lock();
    if args.json {
        let affected_files: Vec<_> = affected.iter().map(|(p, d)| serde_json::json!({
            "path": p, "depth": d, "is_test": crate::domain::is_test_path(p),
        })).collect();
        let result = serde_json::json!({
            "changed": changed,
            "tests": tests,
            "affected_files": affected_files,
            "not_indexed": not_indexed,
        });
        writeln!(stdout, "{}", serde_json::to_string(&result)?)?;
        return Ok(());
    }

    writeln!(stdout, "Affected by {} changed file(s) — {} test file(s) to re-run:",
        changed.len(), tests.len())?;
    for t in &tests {
        writeln!(stdout, "  {}", t)?;
    }
    writeln!(stdout, "Full blast radius: {} file(s) (depth <= {})", affected.len(), depth)?;
    for (p, d) in &affected {
        writeln!(stdout, "  {} (depth {})", p, d)?;
    }
    if !not_indexed.is_empty() {
        writeln!(stdout, "{} input file(s) not in index: {}",
            not_indexed.len(), not_indexed.join(", "))?;
    }
    Ok(())
}

// --- map subcommand ---

/// CLI arguments for the `map` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp map",
          about = "Project architecture map (modules, deps, entry points)")]
pub struct MapArgs {
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Compact output (top modules/deps/hot functions only)
    #[arg(long)]
    pub compact: bool,
}

/// Project map — aider repo-map style.
///
/// Output format:
/// ```text
/// src/mcp/server.rs (158KB, 98 symbols)
///   McpServer: handle_tool_call, process_message, flush_metrics
/// ```
pub fn cmd_map(project_root: &Path, args: MapArgs) -> Result<()> {
    let json_mode = args.json;
    let compact = args.compact;

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let (modules, deps, entry_points, hot_functions) = queries::get_project_map(conn)?;

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        // Field names (`caller_count` / `test_caller_count`) and `--compact`
        // cap (top-10) match MCP `project_map`. CLI default returns top-15
        // (the DB LIMIT in get_project_map).
        let hot_cap = if compact { 10 } else { hot_functions.len() };
        let hot_json: Vec<serde_json::Value> = hot_functions.iter().take(hot_cap).map(|h| {
            let mut obj = serde_json::json!({
                "name": h.name,
                "type": h.node_type,
                "file": h.file,
                "caller_count": h.caller_count,
            });
            if h.test_caller_count > 0 {
                obj["test_caller_count"] = serde_json::json!(h.test_caller_count);
            }
            obj
        }).collect();

        let result = serde_json::json!({
            "modules": modules.iter().map(|m| serde_json::json!({
                "path": m.path,
                "files": m.files,
                "functions": m.functions,
                "classes": m.classes,
                "interfaces_traits": m.interfaces_traits,
                "languages": m.languages,
                "key_symbols": m.key_symbols,
            })).collect::<Vec<_>>(),
            "module_dependencies": deps.iter().map(|d| serde_json::json!({
                "from": d.from,
                "to": d.to,
                "imports": d.import_count,
            })).collect::<Vec<_>>(),
            "entry_points": entry_points.iter().map(|e| serde_json::json!({
                "route": e.route,
                "handler": e.handler,
                "file": e.file,
                "kind": e.kind,
            })).collect::<Vec<_>>(),
            "hot_functions": hot_json,
        });
        writeln!(stdout, "{}", serde_json::to_string(&result)?)?;
        return Ok(());
    }

    // Entry points
    if !entry_points.is_empty() {
        writeln!(stdout, "Entry Points:")?;
        for ep in &entry_points {
            writeln!(stdout, "  {} → {} ({})", ep.route, ep.handler, ep.file)?;
        }
        writeln!(stdout)?;
    }

    // Modules
    if modules.is_empty() {
        if entry_points.is_empty() {
            writeln!(stdout, "(empty project — no indexed source files)")?;
        }
        return Ok(());
    }
    writeln!(stdout, "Modules:")?;
    let max_modules = if compact { 15 } else { modules.len() };
    for m in modules.iter().take(max_modules) {
        let total_symbols = m.functions + m.classes + m.interfaces_traits;
        write!(
            stdout,
            "{} ({}, {}",
            m.path, plural(m.files as i64, "file"), plural(total_symbols as i64, "symbol")
        )?;
        if !m.languages.is_empty() {
            write!(stdout, ", {}", m.languages.join("/"))?;
        }
        writeln!(stdout, ")")?;
        if !m.key_symbols.is_empty() {
            writeln!(stdout, "  {}", m.key_symbols.join(", "))?;
        }
    }
    if compact && modules.len() > max_modules {
        writeln!(stdout, "  ... and {} more modules", modules.len() - max_modules)?;
    }

    // Dependencies (compact: top 10)
    if !deps.is_empty() {
        writeln!(stdout)?;
        writeln!(stdout, "Dependencies:")?;
        let max_deps = if compact { 10 } else { deps.len().min(30) };
        for d in deps.iter().take(max_deps) {
            writeln!(stdout, "  {} → {} ({} imports)", d.from, d.to, d.import_count)?;
        }
    }

    // Hot functions (compact: top 5)
    if !hot_functions.is_empty() {
        writeln!(stdout)?;
        writeln!(stdout, "Hot Functions:")?;
        let max_hot = if compact { 5 } else { hot_functions.len() };
        for h in hot_functions.iter().take(max_hot) {
            if h.test_caller_count > 0 {
                writeln!(
                    stdout,
                    "  {} ({}) — {} callers + {} test ({})",
                    h.name, h.node_type, h.caller_count, h.test_caller_count, h.file
                )?;
            } else {
                writeln!(
                    stdout,
                    "  {} ({}) — {} callers ({})",
                    h.name, h.node_type, h.caller_count, h.file
                )?;
            }
        }
    }

    Ok(())
}

// --- tour subcommand ---

/// CLI arguments for the `tour` subcommand.
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp tour",
          about = "Dependency-ordered reading order: where to start reading a repo (or subtree)")]
pub struct TourArgs {
    /// Optional path prefix to scope the tour to a subtree (omit = whole project;
    /// absolute paths under the project root are accepted)
    pub path: Option<String>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// True when module directory `module_path` is the prefix `pre` or sits under it.
/// `pre` is a normalized path; an empty prefix (from "." or omitted) matches all.
fn module_under_prefix(module_path: &str, pre: &str) -> bool {
    let pre = pre.trim_end_matches('/');
    pre.is_empty() || module_path == pre || module_path.starts_with(&format!("{}/", pre))
}

/// Reading order — lists a module's prerequisites before the modules that build
/// on them (Kahn topological sort over import edges), so reading top-to-bottom
/// orients you from the ground up. Reuses the project-map graph; read-only.
pub fn cmd_tour(project_root: &Path, args: TourArgs) -> Result<()> {
    use crate::graph::reading_order::compute_reading_order;

    let json_mode = args.json;

    // Optional subtree scope. Omitted → whole project.
    let scope: Option<String> = match args.path.as_deref() {
        None => None,
        Some("") => anyhow::bail!("path must not be empty — omit it to tour the whole project"),
        Some(raw) => Some(normalize_user_path(project_root, raw)?),
    };

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let (modules, deps, entry_points, _hot) = queries::get_project_map(conn)?;

    let modules: Vec<_> = match &scope {
        None => modules,
        Some(prefix) => modules
            .into_iter()
            .filter(|m| module_under_prefix(&m.path, prefix))
            .collect(),
    };

    let order = compute_reading_order(&modules, &deps, &entry_points);

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        // Object envelope (cli_json_empty contract: same shape on the empty path).
        let arr: Vec<serde_json::Value> = order.iter().map(|e| serde_json::json!({
            "path": e.path,
            "role": e.role.as_str(),
            "depended_on_by": e.depended_on_by,
            "depends_on": e.depends_on,
            "key_symbols": e.key_symbols,
            "in_cycle": e.in_cycle,
        })).collect();
        let result = serde_json::json!({ "reading_order": arr });
        writeln!(stdout, "{}", serde_json::to_string(&result)?)?;
        return Ok(());
    }

    if order.is_empty() {
        match &scope {
            Some(p) => writeln!(stdout, "(no indexed modules under: {})", p)?,
            None => writeln!(stdout, "(empty project — no indexed source files)")?,
        }
        return Ok(());
    }

    let cycles = order.iter().filter(|e| e.in_cycle).count();
    if cycles > 0 {
        writeln!(stdout, "Reading order (foundational → entry; {} modules, {} via cycle-break):",
            order.len(), cycles)?;
    } else {
        writeln!(stdout, "Reading order (foundational → entry; {} modules):", order.len())?;
    }
    for (i, e) in order.iter().enumerate() {
        let mut annot: Vec<String> = vec![format!("[{}]", e.role.as_str())];
        if e.in_cycle {
            annot.push("[cycle]".to_string());
        }
        if e.depended_on_by > 0 {
            annot.push(format!("depended-on-by {}", e.depended_on_by));
        }
        if !e.depends_on.is_empty() {
            let shown = e.depends_on.iter().take(3).cloned().collect::<Vec<_>>().join(",");
            let extra = e.depends_on.len().saturating_sub(3);
            let suffix = if extra > 0 { format!("+{}", extra) } else { String::new() };
            annot.push(format!("imports {}{}", shown, suffix));
        }
        write!(stdout, "  {:>2}. {}  {}", i + 1, e.path, annot.join(" · "))?;
        if !e.key_symbols.is_empty() {
            let syms = e.key_symbols.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
            write!(stdout, "  — {}", syms)?;
        }
        writeln!(stdout)?;
    }

    Ok(())
}

// --- overview subcommand ---

/// CLI arguments for the `overview` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp overview",
          about = "Module overview (symbols grouped by file and type)")]
pub struct OverviewArgs {
    /// Path prefix to scan ('.' = whole project; absolute paths under root OK)
    pub path: String,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Compact output (no caller counts)
    #[arg(long)]
    pub compact: bool,
}

/// Module overview: all symbols in files under a path prefix.
pub fn cmd_overview(project_root: &Path, args: OverviewArgs) -> Result<()> {
    // clap requires the positional (missing → exit 2), but accepts an empty
    // string; preserve the empty-path guard below for unset-shell-var `overview "$X"`.
    let raw_path = args.path.as_str();
    // Reject empty-string path: mirrors MCP `tool_module_overview` (script users
    // hit this when a shell variable is unset and overview "$X" expands to "").
    if raw_path.is_empty() {
        anyhow::bail!("path must not be empty — use '.' to scan the whole project root");
    }
    // Normalize: strip leading "./", treat bare "." as empty prefix, and resolve
    // absolute paths under the project root to their relative portion. Mirrors MCP
    // `tool_module_overview` for "./"/"." and additionally supports paste-from-IDE
    // absolute paths (the indexed `file_path` column is project-relative, so
    // unnormalized absolute paths returned "No symbols found").
    let path_prefix_owned = normalize_user_path(project_root, raw_path)?;
    let path_prefix = path_prefix_owned.as_str();

    let json_mode = args.json;
    let compact = args.compact;

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let exports = queries::get_module_exports(conn, path_prefix)?;

    // Filter out test symbols (align with MCP module_overview behavior)
    let exports: Vec<_> = exports.into_iter()
        .filter(|e| !crate::domain::is_test_symbol(&e.name, &e.file_path))
        .collect();

    if exports.is_empty() {
        // JSON empty-result contract (feedback_cli_json_empty_contract):
        // stdout must always be valid JSON. Use a clean eprintln + exit 1
        // instead of `anyhow::bail!` so the JSON-mode stderr doesn't carry
        // the anyhow `Error:` prefix that confuses log consumers.
        if json_mode {
            println!("[]");
            eprintln!("[code-graph] No symbols found under: {}", raw_path);
            std::process::exit(1);
        }
        anyhow::bail!("[code-graph] No symbols found under: {}", raw_path);
    }

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        // `caller_count` matches MCP `module_overview.active_exports[].caller_count`.
        let results: Vec<serde_json::Value> = exports
            .iter()
            .map(|e| {
                serde_json::json!({
                    "name": e.name,
                    "type": e.node_type,
                    "file": e.file_path,
                    "signature": e.signature,
                    "caller_count": e.caller_count,
                    "start_line": e.start_line,
                    "end_line": e.end_line,
                })
            })
            .collect();
        writeln!(stdout, "{}", serde_json::to_string(&results)?)?;
        return Ok(());
    }

    // Group by file
    let mut by_file: std::collections::BTreeMap<&str, Vec<&queries::ModuleExport>> =
        std::collections::BTreeMap::new();
    for e in &exports {
        by_file.entry(&e.file_path).or_default().push(e);
    }

    // Single-file path → outline format (sorted by line, signature + line range visible).
    // Replaces Read on huge files: a 3000+ line source emits ~symbol-count lines instead.
    if by_file.len() == 1 {
        let (file, symbols) = by_file.iter().next().unwrap();
        writeln!(stdout, "{}", file)?;
        let mut sorted: Vec<&queries::ModuleExport> = symbols.to_vec();
        sorted.sort_by_key(|e| e.start_line);
        for s in sorted {
            let callers = if s.caller_count > 0 {
                format!(" ({}×)", s.caller_count)
            } else {
                String::new()
            };
            if compact {
                writeln!(stdout, "  L{}-{}  {}  {}{}",
                    s.start_line, s.end_line, s.node_type, s.name, callers)?;
            } else {
                let sig = s.signature.as_deref().unwrap_or("");
                let sig_display = if sig.is_empty() {
                    String::new()
                } else {
                    format!("  {}", sig.lines().next().unwrap_or("").trim())
                };
                writeln!(stdout, "  L{}-{}  {}  {}{}{}",
                    s.start_line, s.end_line, s.node_type, s.name, callers, sig_display)?;
            }
        }
        return Ok(());
    }

    for (file, symbols) in &by_file {
        writeln!(stdout, "{}", file)?;
        // Group by type within file
        let mut by_type: std::collections::BTreeMap<&str, Vec<&&queries::ModuleExport>> =
            std::collections::BTreeMap::new();
        for s in symbols {
            by_type.entry(&s.node_type).or_default().push(s);
        }
        for (typ, syms) in &by_type {
            let names: Vec<String> = syms
                .iter()
                .map(|s| {
                    if compact {
                        s.name.clone()
                    } else if s.caller_count > 0 {
                        format!("{} ({}×)", s.name, s.caller_count)
                    } else {
                        s.name.clone()
                    }
                })
                .collect();
            writeln!(stdout, "  {}: {}", typ, names.join(", "))?;
        }
    }

    Ok(())
}

// --- show subcommand ---

/// CLI arguments for the `show` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp show",
          about = "Show symbol details (code, type, signature)")]
pub struct ShowArgs {
    /// Symbol name (required unless --node-id is given)
    pub symbol: Option<String>,
    /// Look up by node ID instead of name
    #[arg(long = "node-id")]
    pub node_id: Option<i64>,
    /// Disambiguate same-name symbols by file path
    #[arg(long)]
    pub file: Option<String>,
    /// Show callers/callees (hidden aliases: --include-refs, --include-references)
    #[arg(long = "refs", aliases = ["include-refs", "include-references"])]
    pub refs: bool,
    /// Show impact summary (hidden alias: --include-impact)
    #[arg(long = "impact", alias = "include-impact")]
    pub impact: bool,
    /// Show test callers/callees in the --refs section (hidden by default)
    #[arg(long)]
    pub include_tests: bool,
    /// Surrounding source lines (default: 3 with --node-id, else 0)
    #[arg(long = "context-lines")]
    pub context_lines: Option<usize>,
    /// Compact output
    #[arg(long)]
    pub compact: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// Show symbol details (code, type, signature).
/// CLI equivalent of MCP `get_ast_node`.
pub fn cmd_show(project_root: &Path, args: ShowArgs) -> Result<()> {
    let json_mode = args.json;
    let compact = args.compact;
    let include_refs = args.refs;
    let include_impact = args.impact;
    let file_filter_owned: Option<String> = match args.file.as_deref() {
        Some(f) => Some(normalize_user_path(project_root, f)?),
        None => None,
    };
    let file_filter = file_filter_owned.as_deref();
    let context_lines_explicit: Option<usize> = args.context_lines;
    let node_id_arg: Option<i64> = args.node_id;
    // Default context_lines=3 when using --node-id (align with MCP behavior), 0 otherwise
    let context_lines: usize = context_lines_explicit
        .unwrap_or(if node_id_arg.is_some() { 3 } else { 0 });

    // If positional arg points at a real file on disk (has a recognized code
    // extension), nudge the user toward `overview` — `show` takes symbol names.
    if node_id_arg.is_none() {
        if let Some(arg) = args.symbol.as_deref() {
            if !arg.is_empty()
                && crate::utils::config::detect_language(arg).is_some()
                && project_root.join(arg).is_file()
            {
                eprintln!(
                    "[code-graph] `{}` looks like a file path. `show` takes a symbol name (function/struct/const).",
                    arg
                );
                eprintln!(
                    "            File-level symbols: code-graph-mcp overview {}",
                    arg
                );
                eprintln!(
                    "            Full file content:  Read the file directly."
                );
                std::process::exit(1);
            }
        }
    }

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    // Resolve node(s): by --node-id, or by positional symbol name
    let nodes_with_paths: Vec<(queries::NodeResult, String)> = if let Some(nid) = node_id_arg {
        match queries::get_node_with_file_by_id(conn, nid)? {
            Some(nwf) => vec![(nwf.node, nwf.file_path)],
            None => {
                if json_mode { println!("[]"); }
                eprintln!("[code-graph] Node ID {} not found.", nid);
                std::process::exit(1);
            }
        }
    } else {
        let symbol = args.symbol.as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!(
                "Usage: code-graph-mcp show <symbol> [--node-id N] [--file <path>] [--refs] [--impact] [--context-lines N] [--compact] [--json]"
            ))?;

        let nodes = if let Some(fp) = file_filter {
            let mut found: Vec<_> = queries::get_nodes_by_file_path(conn, fp)?
                .into_iter()
                .filter(|n| n.name == symbol || n.qualified_name.as_deref() == Some(symbol))
                .collect();
            // Same `Class.method` fallback as the name path: if exact match fails
            // but the symbol has a dot, fall back to the base name within the file.
            // Why: parsers populate qualified_name inconsistently across languages
            // (Rust `impl` blocks: yes; free functions: no), so the literal-match
            // filter above used to silently miss legitimate symbols.
            if found.is_empty() && symbol.contains('.') {
                if let Some(base_name) = symbol.rsplit('.').next() {
                    found = queries::get_nodes_by_file_path(conn, fp)?
                        .into_iter()
                        .filter(|n| n.name == base_name)
                        .collect();
                }
            }
            found
        } else {
            let mut found = queries::get_nodes_by_name(conn, symbol)?;
            // `Class.method` fallback: when no node has the exact qualified name
            // stored in DB, prefer nodes whose qualified_name matches; otherwise
            // fall back to all nodes with the base name. Without this fallback,
            // `show McpServer.lock_or_recover` was reporting "Symbol not found"
            // even though `callgraph` resolves the same input via prefix-strip.
            if found.is_empty() && symbol.contains('.') {
                if let Some(base_name) = symbol.rsplit('.').next() {
                    let by_name = queries::get_nodes_by_name(conn, base_name)?;
                    let any_qualified = by_name.iter()
                        .any(|n| n.qualified_name.as_deref() == Some(symbol));
                    if any_qualified {
                        found = by_name.into_iter()
                            .filter(|n| n.qualified_name.as_deref() == Some(symbol))
                            .collect();
                    } else {
                        found = by_name;
                    }
                }
            }
            found
        };

        if nodes.is_empty() {
            if json_mode { println!("[]"); }
            eprintln!("[code-graph] Symbol not found: {}", symbol);
            let candidates = queries::find_functions_by_fuzzy_name(conn, symbol)?;
            if !candidates.is_empty() {
                eprintln!("[code-graph] Did you mean:");
                for c in candidates.iter().take(5) {
                    eprintln!("  {} ({}) in {}", c.name, c.node_type, c.file_path);
                }
            }
            std::process::exit(1);
        }

        nodes.into_iter().map(|n| {
            let fp = queries::get_file_path(conn, n.file_id)
                .ok().flatten().unwrap_or_else(|| "?".to_string());
            (n, fp)
        }).collect()
    };

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        let results: Vec<serde_json::Value> = nodes_with_paths.iter().map(|(node, fp)| {
            let mut obj = serde_json::json!({
                "node_id": node.id,
                "type": node.node_type,
                "name": node.qualified_name.as_deref().unwrap_or(&node.name),
                "file_path": fp,
                "start_line": node.start_line,
                "end_line": node.end_line,
                "signature": node.signature,
                "return_type": node.return_type,
                "param_types": node.param_types,
            });
            if !compact {
                if context_lines > 0 {
                    if let Some(code) = read_source_context(project_root, fp, node.start_line, node.end_line, context_lines) {
                        obj["code_content"] = serde_json::json!(code);
                    } else {
                        obj["code_content"] = serde_json::json!(node.code_content);
                    }
                } else {
                    obj["code_content"] = serde_json::json!(node.code_content);
                }
            }
            if include_refs {
                use crate::domain::REL_CALLS;
                let include_tests = args.include_tests;
                let callees = queries::get_edge_targets_with_files(conn, node.id, REL_CALLS).unwrap_or_default();
                let callers = queries::get_edge_sources_with_files(conn, node.id, REL_CALLS).unwrap_or_default();
                obj["calls"] = serde_json::json!(callees.iter().map(|(n, f)| serde_json::json!({"name": n, "file": f})).collect::<Vec<_>>());
                let filtered_callers: Vec<_> = if include_tests {
                    callers.iter().collect()
                } else {
                    callers.iter().filter(|(n, f)| !crate::domain::is_test_symbol(n, f)).collect()
                };
                obj["called_by"] = serde_json::json!(filtered_callers.iter().map(|(n, f)| serde_json::json!({"name": n, "file": f})).collect::<Vec<_>>());
                if !include_tests {
                    let test_count = callers.len() - filtered_callers.len();
                    if test_count > 0 {
                        obj["test_callers_hidden"] = serde_json::json!(test_count);
                    }
                }
            }
            if include_impact {
                let callers = queries::get_callers_with_route_info(conn, &node.name, Some(fp.as_str()), 3, 0).unwrap_or_default();
                let callers: Vec<_> = callers.into_iter().filter(|c| c.depth > 0).collect();
                let prod: Vec<_> = callers.iter().filter(|c| !crate::domain::is_test_symbol(&c.name, &c.file_path)).collect();
                let routes = callers.iter().filter(|c| c.route_info.is_some()).count();
                let files: std::collections::HashSet<&str> = prod.iter().map(|c| c.file_path.as_str()).collect();
                let risk = crate::domain::compute_risk_level(prod.len(), routes, false);
                obj["impact"] = serde_json::json!({
                    "risk_level": risk,
                    "direct_callers": prod.iter().filter(|c| c.depth == 1).count(),
                    "transitive_callers": prod.iter().filter(|c| c.depth > 1).count(),
                    "affected_files": files.len(),
                    "affected_routes": routes,
                });
            }
            obj
        }).collect();
        writeln!(stdout, "{}", serde_json::to_string(&results)?)?;
        return Ok(());
    }

    for (node, fp) in &nodes_with_paths {
        writeln!(stdout, "{}", format_node_compact(node, fp))?;
        if !compact {
            if context_lines > 0 {
                if let Some(code) = read_source_context(project_root, fp, node.start_line, node.end_line, context_lines) {
                    for line in code.lines() {
                        writeln!(stdout, "  {}", line)?;
                    }
                } else if !node.code_content.is_empty() {
                    for line in node.code_content.lines() {
                        writeln!(stdout, "  {}", line)?;
                    }
                }
            } else if !node.code_content.is_empty() {
                for line in node.code_content.lines() {
                    writeln!(stdout, "  {}", line)?;
                }
            }
        }
        if include_refs {
            use crate::domain::REL_CALLS;
            let include_tests = args.include_tests;
            let callees = queries::get_edge_targets_with_files(conn, node.id, REL_CALLS).unwrap_or_default();
            let callers = queries::get_edge_sources_with_files(conn, node.id, REL_CALLS).unwrap_or_default();
            if !callees.is_empty() {
                writeln!(stdout, "  Calls:")?;
                for (name, file) in &callees {
                    writeln!(stdout, "    → {} ({})", name, file)?;
                }
            }
            if !callers.is_empty() {
                let mut test_count = 0usize;
                writeln!(stdout, "  Called by:")?;
                for (name, file) in &callers {
                    if !include_tests && crate::domain::is_test_symbol(name, file) {
                        test_count += 1;
                    } else {
                        writeln!(stdout, "    ← {} ({})", name, file)?;
                    }
                }
                if test_count > 0 {
                    writeln!(stdout, "    ({} test callers hidden, use --include-tests to show)", test_count)?;
                }
            }
        }
        if include_impact {
            let callers = queries::get_callers_with_route_info(conn, &node.name, Some(fp.as_str()), 3, 0).unwrap_or_default();
            let callers: Vec<_> = callers.into_iter().filter(|c| c.depth > 0).collect();
            let prod: Vec<_> = callers.iter().filter(|c| !crate::domain::is_test_symbol(&c.name, &c.file_path)).collect();
            let routes = callers.iter().filter(|c| c.route_info.is_some()).count();
            let files: std::collections::HashSet<&str> = prod.iter().map(|c| c.file_path.as_str()).collect();
            let risk = crate::domain::compute_risk_level(prod.len(), routes, false);
            writeln!(stdout, "  Impact: {} — {} direct, {} transitive, {} files, {} routes",
                risk, prod.iter().filter(|c| c.depth == 1).count(),
                prod.iter().filter(|c| c.depth > 1).count(), files.len(), routes)?;
        }
    }

    Ok(())
}

/// Read source code with context lines from the project file system.
fn read_source_context(project_root: &Path, file_path: &str, start_line: i64, end_line: i64, context_lines: usize) -> Option<String> {
    use std::io::BufRead;
    let abs_path = project_root.join(file_path);
    let canonical = abs_path.canonicalize().ok()?;
    let root_canonical = project_root.canonicalize().ok()?;
    if !canonical.starts_with(&root_canonical) {
        return None;
    }
    let file = std::fs::File::open(&canonical).ok()?;
    let reader = std::io::BufReader::new(file);
    let start = (start_line as usize).saturating_sub(1 + context_lines);
    let end = (end_line as usize) + context_lines;
    let mut collected = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        if i >= end { break; }
        if i >= start { collected.push(line.ok()?); }
    }
    if collected.is_empty() { return None; }
    Some(collected.join("\n"))
}

// --- trace subcommand ---

/// CLI arguments for the `trace` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp trace",
          about = "Trace HTTP route → handler → downstream calls")]
pub struct TraceArgs {
    /// Route to trace (e.g. "/api/login" or "POST /api/login")
    pub route: String,
    // clamp(1,20) stays in the handler; clap parse-errors (exit 2) on non-numeric.
    /// Max traversal depth
    #[arg(long, default_value_t = 3)]
    pub depth: i32,
    // The old usage string advertised a phantom --include-middleware that the code
    // never read; --no-middleware is the real flag (middleware shown by default).
    // Migration drops the phantom and advertises --no-middleware (user-approved,
    // audit #4); --include-middleware now errors like any other stray flag.
    /// Hide downstream middleware/calls (shown by default)
    #[arg(long)]
    pub no_middleware: bool,
    /// Include test symbols in the call chain (hidden by default, matching the MCP trace tool)
    #[arg(long)]
    pub include_tests: bool,
    /// Minimum edge-resolution confidence to FOLLOW: extracted, inferred, or
    /// ambiguous. Default 'inferred' hides the ambiguous by-name fan-out (a method
    /// name shared by many defs resolving to all of them) from both the call chain
    /// and the downstream list; pass 'ambiguous' to show every edge.
    #[arg(long = "min-confidence")]
    pub min_confidence: Option<String>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// Trace HTTP route → handler → downstream calls.
/// CLI equivalent of MCP `trace_http_chain`.
pub fn cmd_trace(project_root: &Path, args: TraceArgs) -> Result<()> {
    // clap requires the positional (missing → exit 2) but accepts ""; keep the
    // non-empty guard with a Usage string (now advertising --no-middleware).
    let route_path = args.route.as_str();
    if route_path.is_empty() {
        anyhow::bail!("Usage: code-graph-mcp trace <route> [--depth N] [--no-middleware] [--json]");
    }

    let depth: i32 = args.depth.clamp(1, 20);
    let json_mode = args.json;
    let include_middleware = !args.no_middleware;
    // Hide test symbols from the recursive call chain by default, matching the MCP
    // trace_http_chain tool (server/tools/advanced.rs). The one-hop downstream list
    // stays unfiltered FOR TEST SYMBOLS on both surfaces (it still honors the
    // confidence floor below). --include-tests opts the chain back in.
    let include_tests = args.include_tests;

    // Confidence floor (default 'inferred'): hide the ambiguous by-name fan-out from
    // both the recursive chain and the one-hop downstream list, matching callgraph /
    // impact / get_call_graph (v0.77 — trace was previously rank-0 show-all).
    // --min-confidence ambiguous restores every edge. Validated at entry, mirroring
    // cmd_callgraph.
    let min_conf_tier: &'static str = match args.min_confidence.as_deref() {
        None | Some("") => crate::domain::CONF_INFERRED,
        Some(c) => crate::domain::normalize_confidence(c).ok_or_else(|| {
            anyhow::anyhow!(
                "--min-confidence must be one of: extracted, inferred, ambiguous (got '{}')",
                c
            )
        })?,
    };
    let min_conf_rank = crate::domain::confidence_rank(min_conf_tier);

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    // Parse method filter (e.g., "POST /api/login" → method=POST, path=/api/login)
    let (method_filter, path) = if let Some(idx) = route_path.find(' ') {
        (Some(route_path[..idx].to_uppercase()), &route_path[idx + 1..])
    } else {
        (None, route_path)
    };

    use crate::domain::REL_ROUTES_TO;
    let mut rows = queries::find_routes_by_path(conn, path, REL_ROUTES_TO)?;

    // Filter by HTTP method if specified (parse metadata JSON for accurate matching)
    if let Some(ref method) = method_filter {
        rows.retain(|r| {
            r.metadata.as_ref().is_some_and(|m| {
                serde_json::from_str::<serde_json::Value>(m).ok()
                    .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(|s| s.to_string()))
                    .is_some_and(|rm| rm.eq_ignore_ascii_case(method))
            })
        });
    }

    if rows.is_empty() {
        if json_mode {
            println!("{}", serde_json::json!({"handlers": [], "message": format!("No routes matching: {}", route_path)}));
        }
        anyhow::bail!("[code-graph] No routes matching: {}", route_path);
    }

    let mut stdout = std::io::stdout().lock();

    // Batch-fetch downstream calls if middleware included
    use crate::domain::REL_CALLS;
    let downstream_map = if include_middleware {
        let node_ids: Vec<i64> = rows.iter().map(|rm| rm.node_id).collect();
        queries::get_edge_target_names_batch(conn, &node_ids, REL_CALLS, min_conf_rank)?
    } else {
        std::collections::HashMap::new()
    };

    if json_mode {
        // Single JSON object envelope matching MCP trace_http_chain shape
        let mut handlers = Vec::with_capacity(rows.len());
        let mut ambiguous_hidden: usize = 0;
        for rm in &rows {
            let chain = crate::graph::query::get_call_graph_filtered(
                conn, &rm.handler_name, "callees", depth, Some(&rm.file_path), min_conf_rank,
            )?;
            ambiguous_hidden += chain.suppressed_ambiguous;
            let chain_nodes: Vec<serde_json::Value> = chain.nodes.iter()
                .filter(|n| n.depth > 0)
                .filter(|n| include_tests || !crate::domain::is_test_symbol(&n.name, &n.file_path))
                .map(|n| serde_json::json!({
                    "name": n.name, "file_path": n.file_path, "depth": n.depth,
                }))
                .collect();
            let mut entry = serde_json::json!({
                "handler_name": rm.handler_name,
                "file_path": rm.file_path,
                "start_line": rm.start_line,
                "end_line": rm.end_line,
                "metadata": rm.metadata,
                "call_chain": chain_nodes,
            });
            if chain.limit_hit || chain.depth_capped {
                entry["call_chain_truncated"] = serde_json::json!(true);
            }
            if include_middleware {
                let downstream = downstream_map.get(&rm.node_id)
                    .cloned()
                    .unwrap_or_default();
                entry["downstream_calls"] = serde_json::json!(downstream);
            }
            handlers.push(entry);
        }
        let mut envelope = serde_json::json!({
            "route": path,
            "handlers": handlers,
        });
        if ambiguous_hidden > 0 {
            envelope["ambiguous_edges_hidden"] = serde_json::json!(ambiguous_hidden);
        }
        writeln!(stdout, "{}", serde_json::to_string(&envelope)?)?;
        return Ok(());
    }

    let mut ambiguous_hidden: usize = 0;
    for rm in &rows {
        // Render the route label as "METHOD path" from the routes_to metadata
        // (matching the map's Entry Points) instead of dumping the raw JSON blob.
        let route_label = rm.metadata.as_deref()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .map(|v| format!("{} {}",
                v["method"].as_str().unwrap_or("ALL"),
                v["path"].as_str().unwrap_or(path)))
            .unwrap_or_else(|| path.to_string());
        writeln!(stdout, "{} → {} ({}:{})",
            route_label, rm.handler_name, rm.file_path, rm.start_line)?;

        if include_middleware {
            if let Some(downstream) = downstream_map.get(&rm.node_id) {
                if !downstream.is_empty() {
                    writeln!(stdout, "  downstream: {}", downstream.join(", "))?;
                }
            }
        }

        // Show call chain
        let chain = crate::graph::query::get_call_graph_filtered(
            conn, &rm.handler_name, "callees", depth, Some(&rm.file_path), min_conf_rank,
        )?;
        ambiguous_hidden += chain.suppressed_ambiguous;
        for n in &chain.nodes {
            if n.depth == 0 { continue; }
            if !include_tests && crate::domain::is_test_symbol(&n.name, &n.file_path) { continue; }
            let indent = "  ".repeat(n.depth as usize);
            writeln!(stdout, "{}→ {} ({})", indent, n.name, n.file_path)?;
        }
        if chain.limit_hit || chain.depth_capped {
            writeln!(stdout, "  ⚠ chain truncated for {}", rm.handler_name)?;
        }
    }
    if ambiguous_hidden > 0 {
        writeln!(
            stdout,
            "  ({} direct ambiguous by-name edge(s) hidden — use --min-confidence ambiguous to show)",
            ambiguous_hidden,
        )?;
    }

    Ok(())
}

/// File-level dependency graph.
/// CLI equivalent of MCP `dependency_graph`.
/// Scan a file for language-appropriate barrel / re-export / import patterns.
/// Used by `cmd_deps` as a fallback when the graph has no tracked edges for
/// a file (e.g. Rust `mod.rs` barrels that only contain `pub mod X;`).
fn scan_barrel_patterns(project_root: &Path, file_path: &str) -> Option<Vec<(usize, String)>> {
    let full = project_root.join(file_path);
    let content = std::fs::read_to_string(&full).ok()?;
    let lang = crate::utils::config::detect_language(file_path);
    let mut hits = Vec::new();
    for (idx, line) in content.lines().enumerate().take(1000) {
        let t = line.trim_start();
        let matched = match lang {
            Some("rust") => {
                t.starts_with("pub mod ")
                    || t.starts_with("mod ")
                    || t.starts_with("pub use ")
                    || t.starts_with("use ")
            }
            Some("typescript") | Some("tsx") | Some("javascript") => {
                t.starts_with("import ")
                    || (t.starts_with("export ") && t.contains(" from "))
            }
            Some("python") => {
                (t.starts_with("from ") && t.contains(" import "))
                    || t.starts_with("import ")
            }
            Some("go") | Some("java") | Some("csharp") | Some("kotlin") => {
                t.starts_with("import ")
            }
            Some("ruby") => t.starts_with("require ") || t.starts_with("require_relative "),
            Some("php") => {
                t.starts_with("use ")
                    || t.starts_with("require ")
                    || t.starts_with("include ")
            }
            _ => false,
        };
        if matched {
            hits.push((idx + 1, line.to_string()));
        }
    }
    if hits.is_empty() { None } else { Some(hits) }
}

// --- deps subcommand ---

/// CLI arguments for the `deps` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp deps",
          about = "File-level dependency graph")]
pub struct DepsArgs {
    /// File whose dependencies to show (absolute paths under root OK)
    pub file: String,
    // --direction stays a String validated in-handler (not a clap ValueEnum) so
    // the exact "must be one of" message + exit 1 are preserved for callers.
    /// Direction: outgoing, incoming, or both
    #[arg(long, default_value = "both")]
    pub direction: String,
    // clamp(1,10) stays in the handler; clap parse-errors (exit 2) on non-numeric.
    /// Max traversal depth
    #[arg(long, default_value_t = 2)]
    pub depth: i32,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Compact output
    #[arg(long)]
    pub compact: bool,
}

/// File-level dependency graph. CLI equivalent of MCP `dependency_graph`.
pub fn cmd_deps(project_root: &Path, args: DepsArgs) -> Result<()> {
    // clap requires the positional (missing → exit 2) but accepts ""; keep the
    // non-empty guard with the exact Usage string.
    let raw_file_path = args.file.as_str();
    if raw_file_path.is_empty() {
        anyhow::bail!("Usage: code-graph-mcp deps <file> [--direction outgoing|incoming|both] [--depth N] [--json]");
    }
    let file_path_owned = normalize_user_path(project_root, raw_file_path)?;
    let file_path = file_path_owned.as_str();

    let direction = args.direction.as_str();
    if !matches!(direction, "outgoing" | "incoming" | "both") {
        anyhow::bail!("--direction must be one of: outgoing, incoming, both");
    }
    let depth: i32 = args.depth.clamp(1, 10);
    let json_mode = args.json;
    let compact = args.compact;

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let deps = queries::get_import_tree(conn, file_path, direction, depth)?;
    if deps.is_empty() {
        // Barrel / index-file fallback — scan source for re-export / import lines.
        // Rust `mod.rs` with only `pub mod X;` has no tracked edges in the graph.
        if let Some(lines) = scan_barrel_patterns(project_root, file_path) {
            let mut stdout = std::io::stdout().lock();
            if json_mode {
                let result = serde_json::json!({
                    "file": file_path,
                    "depends_on": [],
                    "depended_by": [],
                    "barrel_scan": lines.iter().map(|(ln, t)| {
                        serde_json::json!({"line": ln, "text": t.trim()})
                    }).collect::<Vec<_>>(),
                    "note": "no tracked dep edges; barrel_scan is raw re-export/import lines from file scan",
                });
                writeln!(stdout, "{}", serde_json::to_string(&result)?)?;
            } else {
                writeln!(stdout, "{}", file_path)?;
                writeln!(stdout, "  (no tracked dep edges \u{2014} raw re-export/import lines from file scan:)")?;
                for (ln, text) in lines {
                    writeln!(stdout, "    {}: {}", ln, text.trim())?;
                }
            }
            return Ok(());
        }
        let file_exists = project_root.join(file_path).is_file();
        if json_mode {
            let result = serde_json::json!({
                "file": file_path,
                "depends_on": [],
                "depended_by": [],
                "error": if file_exists {
                    "No tracked dependencies (not a barrel/import file)"
                } else {
                    "File not found"
                },
            });
            println!("{}", serde_json::to_string(&result)?);
        }
        if file_exists {
            anyhow::bail!(
                "[code-graph] No tracked dependencies for: {} (not a barrel/import file \u{2014} try `code-graph-mcp overview {}` or Read directly)",
                file_path,
                file_path
            );
        } else {
            anyhow::bail!(
                "[code-graph] File not found: {} (run `code-graph-mcp incremental-index` if you just created it, or check the path)",
                file_path
            );
        }
    }

    // Filter out cross-language false edges (name-based resolution artifacts)
    // and the synthetic `<external>` bucket (unresolved imports, not a real file).
    let is_compatible_lang =
        |dep_path: &str| crate::utils::config::is_compatible_lang(file_path, dep_path);

    let outgoing: Vec<&_> = deps.iter().filter(|d| d.direction == "outgoing" && is_compatible_lang(&d.file_path)).collect();
    let incoming: Vec<&_> = deps.iter().filter(|d| d.direction == "incoming" && is_compatible_lang(&d.file_path)).collect();

    // Distinguish "no edges at all" (handled by the barrel-fallback branch above)
    // from "edges exist but all targets are <external> or cross-language" — the
    // latter previously rendered as a bare filename with no explanation, which
    // looked like a successful no-op even when the file had unresolved imports.
    let unresolved_outgoing = deps.iter()
        .filter(|d| d.direction == "outgoing" && !is_compatible_lang(&d.file_path))
        .count();
    let unresolved_incoming = deps.iter()
        .filter(|d| d.direction == "incoming" && !is_compatible_lang(&d.file_path))
        .count();

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        let mut result = serde_json::json!({
            "file": file_path,
            "depends_on": outgoing.iter().map(|d| {
                let mut obj = serde_json::json!({"file": d.file_path, "depth": d.depth});
                if !compact && d.depth == 1 { obj["symbols"] = serde_json::json!(d.symbol_count); }
                obj
            }).collect::<Vec<_>>(),
            "depended_by": incoming.iter().map(|d| {
                let mut obj = serde_json::json!({"file": d.file_path, "depth": d.depth});
                if !compact && d.depth == 1 { obj["symbols"] = serde_json::json!(d.symbol_count); }
                obj
            }).collect::<Vec<_>>(),
        });
        if unresolved_outgoing > 0 {
            result["unresolved_outgoing"] = serde_json::json!(unresolved_outgoing);
        }
        if unresolved_incoming > 0 {
            result["unresolved_incoming"] = serde_json::json!(unresolved_incoming);
        }
        writeln!(stdout, "{}", serde_json::to_string(&result)?)?;
        return Ok(());
    }

    writeln!(stdout, "{}", file_path)?;
    if !outgoing.is_empty() {
        writeln!(stdout, "  Depends on:")?;
        for d in &outgoing {
            if compact {
                writeln!(stdout, "    {}", d.file_path)?;
            } else if d.depth == 1 {
                writeln!(stdout, "    {} ({} symbols)", d.file_path, d.symbol_count)?;
            } else {
                writeln!(stdout, "    {} (depth {})", d.file_path, d.depth)?;
            }
        }
    }
    if !incoming.is_empty() {
        writeln!(stdout, "  Depended by:")?;
        for d in &incoming {
            if compact {
                writeln!(stdout, "    {}", d.file_path)?;
            } else if d.depth == 1 {
                writeln!(stdout, "    {} ({} symbols)", d.file_path, d.symbol_count)?;
            } else {
                writeln!(stdout, "    {} (depth {})", d.file_path, d.depth)?;
            }
        }
    }
    if outgoing.is_empty() && incoming.is_empty() && (unresolved_outgoing > 0 || unresolved_incoming > 0) {
        writeln!(
            stdout,
            "  (no resolved deps; {} unresolved outgoing, {} unresolved incoming — targets are <external> or in another language)",
            unresolved_outgoing, unresolved_incoming
        )?;
    }

    Ok(())
}

// --- similar subcommand ---

/// CLI arguments for the `similar` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp similar",
          about = "Find semantically similar code (requires embeddings)")]
pub struct SimilarArgs {
    /// Symbol name (required unless --node-id is given)
    pub symbol: Option<String>,
    /// Look up by node ID instead of name
    #[arg(long = "node-id")]
    pub node_id: Option<i64>,
    // clamp(1,100) stays in the handler; clap parse-errors (exit 2) on non-numeric.
    /// Number of results (default: 5, max: 100)
    #[arg(long = "top-k")]
    pub top_k: Option<i64>,
    /// Max cosine distance (default: 0.8)
    #[arg(long = "max-distance")]
    pub max_distance: Option<f64>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// Find semantically similar code.
/// CLI equivalent of MCP `find_similar_code`.
pub fn cmd_similar(project_root: &Path, args: SimilarArgs) -> Result<()> {
    let top_k: i64 = args.top_k.unwrap_or(5).clamp(1, 100);
    let max_distance: f64 = args.max_distance.unwrap_or(0.8);
    let json_mode = args.json;
    let node_id_arg: Option<i64> = args.node_id;

    // Open with vec support for vector search
    let db_path = project_root.join(CODE_GRAPH_DIR).join("index.db");
    if !db_path.exists() {
        anyhow::bail!("No index found. Run the MCP server first to create the index.");
    }
    let db = Database::open_with_vec(&db_path)?;
    let conn = db.conn();

    if !db.vec_enabled() {
        if json_mode { println!("[]"); }
        eprintln!("[code-graph] Vector search not available (sqlite-vec extension not loaded).");
        eprintln!("  To enable: build with `cargo build --release --features embed-model`.");
        eprintln!("  Alternative: use `code-graph-mcp search <query>` for text-based similarity.");
        return Ok(());
    }

    // Resolve to node_id: by --node-id or by positional symbol name. `target_label`
    // is what we display in error messages — symbol name when resolved by name,
    // "node_id N" when resolved by --node-id.
    let (node_id, target_label) = if let Some(nid) = node_id_arg {
        // Validate existence up-front — BEFORE the embedding checks below. The
        // symbol path already validates (get_first_node_id_by_name); the --node-id
        // path used not to, so a missing id fell through to the embedded_count==0
        // guard and reported a misleading "No embeddings found" instead of the
        // true cause. This check is embedding-independent → reachable and testable
        // in the default (no embed-model) build, and mirrors refs --node-id.
        if queries::get_node_by_id(conn, nid)?.is_none() {
            if json_mode { println!("[]"); }
            eprintln!("[code-graph] node_id {} not found in index", nid);
            std::process::exit(1);
        }
        (nid, format!("node_id {}", nid))
    } else {
        let symbol = args.symbol.as_deref()
            .filter(|s| !s.is_empty())
            .map(strip_qualified_prefix)
            .ok_or_else(|| anyhow::anyhow!(
                "Usage: code-graph-mcp similar <symbol> [--node-id N] [--top-k N] [--max-distance N] [--json]"
            ))?;
        match queries::get_first_node_id_by_name(conn, symbol)? {
            Some(id) => (id, symbol.to_string()),
            None => {
                if json_mode { println!("[]"); }
                // All-digit positional is almost certainly a node_id mistakenly passed
                // without the flag — guide the user instead of "Symbol not found: 1010".
                if !symbol.is_empty() && symbol.chars().all(|c| c.is_ascii_digit()) {
                    eprintln!(
                        "[code-graph] Symbol not found: {} \u{2014} did you mean `code-graph-mcp similar --node-id {}`?",
                        symbol, symbol
                    );
                } else {
                    eprintln!("[code-graph] Symbol not found: {}", symbol);
                }
                std::process::exit(1);
            }
        }
    };

    // Check embedding exists
    let (embedded_count, total_nodes) = queries::count_nodes_with_vectors(conn)?;
    if embedded_count == 0 {
        // Empty-JSON contract: every --json exit path must emit parseable stdout
        // (feedback_cli_json_empty_contract.md). This path (vec extension present
        // but no embeddings generated yet) is the only one in cmd_similar that was
        // missing it — a consumer piping stdout got an empty string → parse error.
        if json_mode { println!("[]"); }
        eprintln!("[code-graph] No embeddings found ({}/{} nodes embedded).", embedded_count, total_nodes);
        eprintln!("  To enable: build with `cargo build --release --features embed-model`,");
        eprintln!("  then restart the MCP server to generate embeddings.");
        eprintln!("  Alternative: use `code-graph-mcp search <query>` for text-based similarity.");
        std::process::exit(1);
    }

    let embedding: Vec<f32> = {
        let bytes = match queries::get_node_embedding(conn, node_id) {
            Ok(b) => b,
            Err(_) => {
                // Node exists (validated above) but this one has no embedding yet —
                // embeddings still generating. Empty-JSON contract: emit [] under
                // --json instead of bailing with empty stdout.
                if json_mode { println!("[]"); }
                eprintln!(
                    "[code-graph] No embedding for {} ({}/{} nodes embedded \u{2014} embeddings still generating; try again shortly or pick a node with `--node-id` from `show {}`).",
                    target_label, embedded_count, total_nodes, target_label
                );
                std::process::exit(1);
            }
        };
        bytemuck::cast_slice(&bytes).to_vec()
    };

    // Over-fetch so self-exclusion + max_distance + test/module post-filters don't
    // silently starve top_k (vec0 KNN can't pre-filter on joined node columns). Parity
    // with the MCP twin tool_find_similar_code; the old `top_k + 1` fell short on any drop.
    let fetch_count = crate::domain::similar_fetch_count(top_k);
    let raw_results = queries::vector_search(conn, &embedding, fetch_count)?;

    // Collect filtered results
    let mut similar: Vec<(queries::NodeResult, String, f64)> = Vec::new();
    for (id, distance) in &raw_results {
        if *id == node_id || *distance > max_distance { continue; }
        let Some(node) = queries::get_node_by_id(conn, *id)? else { continue; };
        let fp = queries::get_file_path(conn, node.file_id)?.unwrap_or_default();
        if crate::domain::is_skippable_result(&node.node_type, &node.name, &fp) { continue; }
        similar.push((node, fp, *distance));
        if similar.len() >= top_k as usize { break; }
    }

    // Observability: post-filters (max_distance + test/module) can shrink results below
    // top_k even with over-fetch. Surface to stderr; stdout JSON stays a bare array.
    let cutoff_dropped = raw_results.iter()
        .filter(|(id, dist)| *id != node_id && *dist > max_distance)
        .count();
    if (similar.len() as i64) < top_k && cutoff_dropped > 0 {
        eprintln!(
            "[code-graph] {} result(s) within max_distance={} (< top_k={}); {} nearer candidate(s) exceeded the cutoff. Raise --max-distance to widen.",
            similar.len(), max_distance, top_k, cutoff_dropped
        );
    }

    let mut stdout = std::io::stdout().lock();

    if similar.is_empty() {
        if json_mode {
            writeln!(stdout, "[]")?;
        }
        eprintln!("[code-graph] No similar code found for node_id: {}", node_id);
        return Ok(());
    }

    if json_mode {
        let json_results: Vec<serde_json::Value> = similar.iter().map(|(node, fp, distance)| {
            let similarity = 1.0 / (1.0 + distance);
            serde_json::json!({
                "node_id": node.id, "name": node.name, "type": node.node_type, "file_path": fp,
                "start_line": node.start_line, "similarity": (similarity * 10000.0).round() / 10000.0,
                "distance": (distance * 10000.0).round() / 10000.0,
            })
        }).collect();
        writeln!(stdout, "{}", serde_json::to_string(&json_results)?)?;
        return Ok(());
    }

    for (node, fp, distance) in &similar {
        let similarity = 1.0 / (1.0 + distance);
        writeln!(stdout, "{:.1}%  {} {}  {}:{}-{}",
            similarity * 100.0,
            node.node_type, node.qualified_name.as_deref().unwrap_or(&node.name),
            fp, node.start_line, node.end_line)?;
    }

    Ok(())
}

// --- refs subcommand ---

/// CLI arguments for the `refs` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp refs",
          about = "Find all references to a symbol (callers, importers, etc.)")]
pub struct RefsArgs {
    /// Symbol name (required unless --node-id is given)
    pub symbol: Option<String>,
    /// Look up by node ID (authoritative over --file)
    #[arg(long = "node-id")]
    pub node_id: Option<i64>,
    /// Disambiguate same-name symbols by file path
    #[arg(long)]
    pub file: Option<String>,
    // --relation stays an in-handler String validated at entry (before index open),
    // NOT a clap ValueEnum — so a bad --relation on a nonexistent symbol reports the
    // relation error (exit 1), not "symbol not found", and the message is preserved.
    /// Filter: calls, imports, inherits, implements, references, all
    #[arg(long)]
    pub relation: Option<String>,
    // Validated in-handler (not a clap ValueEnum) so a bad value reports a clear
    // tier error before symbol resolution, consistent with --relation.
    /// Minimum edge confidence: extracted (precise), inferred, ambiguous (default: show all)
    #[arg(long = "min-confidence")]
    pub min_confidence: Option<String>,
    /// Compact output
    #[arg(long)]
    pub compact: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// Emit the refs not-found JSON envelope on stdout. Mirrors the success-case
/// envelope shape (object with `references`/`by_relation`) plus an `error` key,
/// so a single consumer parser handles found, empty, and not-found alike — and
/// every `--json` exit path produces parseable stdout (empty-JSON contract).
/// Used by all three not-found branches: symbol, --file miss, and --node-id miss.
fn print_refs_notfound_json(symbol: &str) {
    println!("{}", serde_json::json!({
        "symbol": symbol,
        "total_references": 0,
        "by_relation": {},
        "references": [],
        "error": "Symbol not found",
    }));
}

/// Find all references to a symbol. CLI equivalent of MCP `find_references`.
pub fn cmd_refs(project_root: &Path, args: RefsArgs) -> Result<()> {
    let explicit_file_owned: Option<String> = match args.file.as_deref() {
        Some(f) => Some(normalize_user_path(project_root, f)?),
        None => None,
    };
    let explicit_file = explicit_file_owned.as_deref();
    let relation = args.relation.as_deref();
    // Validate --relation at command entry — before opening the index and before
    // symbol resolution — so a nonexistent symbol with a bad --relation reports the
    // relation error, not "symbol not found". feedback-enum-validate-at-entry.
    if let Some(r) = relation {
        if !matches!(r, "calls" | "imports" | "inherits" | "implements" | "references" | "all") {
            anyhow::bail!(
                "--relation must be one of: calls, imports, inherits, implements, references, all (got '{}')",
                r
            );
        }
    }
    // Validate --min-confidence at entry (before index open), mirroring --relation,
    // so a typo'd tier errors loudly instead of silently passing all rows.
    let min_confidence: Option<&'static str> = match args.min_confidence.as_deref() {
        None => None,
        Some(c) => match crate::domain::normalize_confidence(c) {
            Some(tier) => Some(tier),
            None => anyhow::bail!(
                "--min-confidence must be one of: extracted, inferred, ambiguous (got '{}')",
                c
            ),
        },
    };
    let json_mode = args.json;
    let compact = args.compact;
    let node_id_arg: Option<i64> = args.node_id;

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    // Resolve to (target_ids, symbol_name) — prefer --node-id for same-file multi-def disambiguation.
    // When --node-id is given, it is authoritative: --file is ignored (matches MCP find_references).
    if node_id_arg.is_some() && explicit_file.is_some() {
        eprintln!("[code-graph] Note: --file is ignored when --node-id is given (node_id is authoritative).");
    }
    let (target_ids, symbol): (Vec<i64>, String) = if let Some(nid) = node_id_arg {
        let node = match queries::get_node_by_id(conn, nid)? {
            Some(n) => n,
            None => {
                // Empty-JSON contract: emit a parseable envelope, not empty stdout.
                if json_mode { print_refs_notfound_json(&format!("node_id {}", nid)); }
                eprintln!("[code-graph] node_id {} not found in index", nid);
                std::process::exit(1);
            }
        };
        (vec![nid], node.name)
    } else {
        let raw_symbol = args.symbol.as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!(
                "Usage: code-graph-mcp refs <symbol> [--node-id N] [--file path] [--relation calls|imports|inherits|implements|references] [--min-confidence extracted|inferred|ambiguous] [--compact] [--json]"
            ))?;
        let (base, resolved_file) = resolve_qualified_symbol(conn, raw_symbol, explicit_file);
        let file_path = explicit_file.or(resolved_file.as_deref());

        if let Some(fp) = file_path {
            let nodes = queries::get_nodes_by_file_path(conn, fp)?;
            let matched: Vec<i64> = nodes.iter().filter(|n| n.name == base).map(|n| n.id).collect();
            if matched.is_empty() {
                // Empty-JSON contract: emit a parseable envelope, not empty stdout.
                if json_mode { print_refs_notfound_json(base); }
                eprintln!("[code-graph] Symbol '{}' not found in file '{}'.", base, fp);
                std::process::exit(1);
            }
            (matched, base.to_string())
        } else {
            let ids = queries::get_node_ids_by_name(conn, base)?;
            if ids.is_empty() {
                // Fuzzy auto-resolve: unique match → promote; multi → suggest; none → bail
                match resolve_fuzzy_name_cli(conn, base)? {
                    CliFuzzyResolution::Unique(resolved) => {
                        let resolved_ids = queries::get_node_ids_by_name(conn, &resolved)?;
                        (resolved_ids.into_iter().map(|(id, _)| id).collect(), resolved)
                    }
                    CliFuzzyResolution::Ambiguous(cands) => {
                        if json_mode {
                            let sugg: Vec<serde_json::Value> = cands.iter().take(5).map(|c| serde_json::json!({
                                "name": c.name, "file_path": c.file_path,
                                "type": c.node_type, "node_id": c.node_id, "start_line": c.start_line,
                            })).collect();
                            println!("{}", serde_json::json!({
                                "error": format!("Ambiguous symbol '{}': {} matches. Specify --file or --node-id to disambiguate.", base, cands.len()),
                                "suggestions": sugg,
                            }));
                        } else {
                            eprintln!("[code-graph] Ambiguous symbol '{}': {} matches. Specify --file or --node-id.", base, cands.len());
                            for c in cands.iter().take(5) {
                                eprintln!("  {} ({}) in {} [node_id {}]", c.name, c.node_type, c.file_path, c.node_id);
                            }
                        }
                        std::process::exit(1);
                    }
                    CliFuzzyResolution::NotFound => {
                        // Match the success-case envelope shape (object with
                        // references/by_relation), not a bare `[]`. Object-success
                        // commands (callgraph/trace/deps) all emit an object on the
                        // empty/error path so one parser handles both — refs was the
                        // outlier returning `[]`, which broke `.references` access.
                        if json_mode { print_refs_notfound_json(base); }
                        eprintln!("[code-graph] Symbol not found: {}", base);
                        std::process::exit(1);
                    }
                }
            } else {
                (ids.into_iter().map(|(id, _)| id).collect(), base.to_string())
            }
        }
    };
    // Intentional shadow: downstream paths want &str. Do NOT "simplify" into a
    // single binding — the tuple above must own the String so `get_node_by_id`'s
    // return doesn't get dropped across the .as_str() borrow.
    let symbol = symbol.as_str();

    use crate::domain::{REL_CALLS, REL_IMPORTS, REL_INHERITS, REL_IMPLEMENTS, REL_REFERENCES};
    let relation_filter = match relation {
        Some("calls") => Some(REL_CALLS),
        Some("imports") => Some(REL_IMPORTS),
        Some("inherits") => Some(REL_INHERITS),
        Some("implements") => Some(REL_IMPLEMENTS),
        Some("references") => Some(REL_REFERENCES),
        Some("all") | None => None,
        Some(other) => anyhow::bail!("Unknown relation '{}'. Valid: calls, imports, inherits, implements, references, all", other),
    };

    let mut all_refs: Vec<queries::IncomingReference> = Vec::new();
    // Dedup key is (name, file_path, relation) — it does NOT include the target,
    // so two edges from the same source to DIFFERENT same-name targets collapse to
    // one row. When their confidence differs, show the LOWEST (most conservative)
    // tier: the displayed confidence must not understate a hidden sibling's
    // ambiguity (L1 — surfacing low confidence is the whole point of the feature).
    let mut seen: std::collections::HashMap<(String, String, String), usize> =
        std::collections::HashMap::new();
    let mut conf_filtered = 0usize;
    for target_id in &target_ids {
        let refs = queries::get_incoming_references(conn, *target_id, relation_filter)?;
        for r in refs {
            // --min-confidence: drop refs below the requested tier (default: keep all).
            if let Some(min) = min_confidence {
                if crate::domain::confidence_rank(&r.confidence)
                    < crate::domain::confidence_rank(min)
                {
                    conf_filtered += 1;
                    continue;
                }
            }
            let key = (r.name.clone(), r.file_path.clone(), r.relation.clone());
            match seen.get(&key) {
                Some(&idx) => {
                    // Keep the worst-case (lowest) confidence among deduped siblings.
                    if crate::domain::confidence_rank(&r.confidence)
                        < crate::domain::confidence_rank(&all_refs[idx].confidence)
                    {
                        all_refs[idx].confidence = r.confidence;
                    }
                }
                None => {
                    seen.insert(key, all_refs.len());
                    all_refs.push(r);
                }
            }
        }
    }

    if json_mode {
        let items: Vec<serde_json::Value> = all_refs.iter().map(|r| {
            if compact {
                serde_json::json!({
                    "name": r.name,
                    "file_path": r.file_path,
                    "start_line": r.start_line,
                    "relation": r.relation,
                    "confidence": r.confidence,
                    "node_id": r.node_id,
                })
            } else {
                serde_json::json!({
                    "node_id": r.node_id,
                    "name": r.name,
                    "type": r.node_type,
                    "file_path": r.file_path,
                    "start_line": r.start_line,
                    "relation": r.relation,
                    "confidence": r.confidence,
                })
            }
        }).collect();
        // Group counts by relation, mirroring MCP find_references envelope
        let mut by_relation: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for r in &all_refs {
            *by_relation.entry(r.relation.clone()).or_insert(0) += 1;
        }
        let envelope = serde_json::json!({
            "symbol": symbol,
            "total_references": items.len(),
            "by_relation": by_relation,
            "references": items,
        });
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        let mut stdout = std::io::stdout().lock();
        // Annotate only non-extracted edges so precise refs stay visually clean;
        // inferred/ambiguous are the ones worth scrutiny (by-name cross-file).
        let tag = |c: &str| -> String {
            if c == crate::domain::CONF_EXTRACTED { String::new() } else { format!(" ~{c}") }
        };
        if all_refs.is_empty() {
            writeln!(stdout, "No references found for '{}'.", symbol)?;
        } else {
            writeln!(stdout, "{} references to '{}':", all_refs.len(), symbol)?;
            for r in &all_refs {
                if compact {
                    writeln!(stdout, "  [{}] {} {}{}", r.relation, r.name, r.file_path, tag(&r.confidence))?;
                } else {
                    writeln!(stdout, "  [{}] {} ({}:{}){}", r.relation, r.name, r.file_path, r.start_line, tag(&r.confidence))?;
                }
            }
        }
        if conf_filtered > 0 {
            writeln!(stdout, "({} lower-confidence ref(s) hidden by --min-confidence)", conf_filtered)?;
        }
    }

    Ok(())
}

// --- dead-code subcommand ---

/// CLI arguments for the `dead-code` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp dead-code",
          about = "Find unused code (orphans and exported-unused symbols)")]
pub struct DeadCodeArgs {
    /// Restrict the scan to this path prefix (absolute paths under root OK)
    pub path: Option<String>,
    // --node-type is preferred (matches `search` CLI + MCP param); --type is the
    // legacy alias. clap accepts any string here — the handler validates it via
    // normalize_type_filter so a typo errors loudly instead of false-clean exit 0.
    // --node-type and --type are ONE arg (alias), so supplying both is a clap
    // duplicate-arg error (exit 2) — deliberately stricter than the old parser,
    // which silently honored --node-type and ignored --type (masking a bad --type).
    /// Filter by node type: fn, class, struct, enum, trait, type, const, var (alias: --type)
    #[arg(long = "node-type", alias = "type")]
    pub node_type: Option<String>,
    /// Show test callers (hidden by default)
    #[arg(long)]
    pub include_tests: bool,
    // clap parse-errors (exit 2) on a non-numeric value, replacing the hand
    // parser's warn-and-fallback — consistent with `stats --last` under flavor B.
    /// Minimum lines to report
    #[arg(long, default_value_t = 3)]
    pub min_lines: u32,
    /// Show full code snippets (default: compact, names only)
    #[arg(long)]
    pub no_compact: bool,
    /// Exclude a path prefix (repeatable; default: claude-plugin/, benches/)
    #[arg(long)]
    pub ignore: Vec<String>,
    /// Disable the default --ignore prefixes
    #[arg(long)]
    pub no_ignore: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// Find dead code: orphans and exported-unused symbols.
/// CLI equivalent of MCP `find_dead_code`.
pub fn cmd_dead_code(project_root: &Path, args: DeadCodeArgs) -> Result<()> {
    let DeadCodeArgs {
        path, node_type, include_tests, min_lines, no_compact, ignore, no_ignore,
        json: json_mode,
    } = args;

    let path_filter_owned: Option<String> = match path.as_deref() {
        Some(p) => Some(normalize_user_path(project_root, p)?),
        None => None,
    };
    let path_filter = path_filter_owned.as_deref();
    // --node-type (preferred) and its --type alias both land in `node_type`.
    let type_filter = node_type.as_deref();
    // Validate --type/--node-type up-front: an unknown alias normalizes to an
    // empty Vec, and find_dead_code then falls through to a literal `n.type = :x`
    // match that returns zero rows — so a typo'd `--type fucntion` prints a
    // false-clean "No dead code found" with exit 0. Mirror the cmd_ast_search guard.
    if let Some(tf) = type_filter {
        if crate::domain::normalize_type_filter(tf).is_empty() {
            anyhow::bail!(
                "Unknown type filter: '{}'. Valid: fn, class, struct, enum, trait, type, const, var",
                tf
            );
        }
    }
    let compact = !no_compact;

    // --ignore <pref>: repeatable, prefix-match exclusion. --no-ignore disables defaults.
    // Defaults are owned by `domain::default_dead_code_ignores()` (claude-plugin/, benches/).
    let ignore_prefixes: Vec<String> = if no_ignore {
        Vec::new()
    } else if ignore.is_empty() {
        crate::domain::default_dead_code_ignores()
    } else {
        ignore
    };

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let raw = queries::find_dead_code(conn, path_filter, type_filter, include_tests, min_lines, 200)?;
    let pre_count = raw.len();
    let results: Vec<_> = raw.into_iter()
        .filter(|r| !ignore_prefixes.iter().any(|p| r.file_path.starts_with(p)))
        .collect();
    let ignored = pre_count - results.len();

    if results.is_empty() {
        if json_mode {
            writeln!(std::io::stdout().lock(), "[]")?;
        }
        if ignored > 0 {
            eprintln!(
                "[code-graph] No dead code found after filtering; {} suppressed by --ignore (use --no-ignore to see them).",
                ignored,
            );
        } else {
            eprintln!("[code-graph] No dead code found.");
        }
        return Ok(());
    }

    // Classify into orphans and exported-unused
    let mut orphans: Vec<&queries::DeadCodeResult> = Vec::new();
    let mut exported_unused: Vec<&queries::DeadCodeResult> = Vec::new();

    for r in &results {
        let is_exported = crate::domain::is_dead_code_exported(
            r.has_export_edge, &r.code_content, &r.file_path, &r.name);
        if is_exported {
            exported_unused.push(r);
        } else {
            orphans.push(r);
        }
    }

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        let items: Vec<serde_json::Value> = results.iter().map(|r| {
            // Same classifier as the text path + MCP — the JSON path previously
            // omitted the Go export leg, misfiling exported Go symbols as orphans.
            let is_exported = crate::domain::is_dead_code_exported(
                r.has_export_edge, &r.code_content, &r.file_path, &r.name);
            let mut obj = serde_json::json!({
                "name": r.name,
                "type": r.node_type,
                "file_path": r.file_path,
                "start_line": r.start_line,
                "end_line": r.end_line,
                "category": if is_exported { "exported_unused" } else { "orphan" },
                "lines": r.end_line - r.start_line + 1,
            });
            if !compact {
                obj["code"] = serde_json::json!(r.code_content);
            }
            obj
        }).collect();
        writeln!(stdout, "{}", serde_json::to_string(&items)?)?;
        return Ok(());
    }

    writeln!(stdout, "Dead code: {} candidates ({} orphan, {} exported-unused)",
        results.len(), orphans.len(), exported_unused.len())?;
    writeln!(stdout, "(candidates to verify — receiver-method calls (obj.method()) and cross-file const/type uses are not edge-tracked)\n")?;

    if !orphans.is_empty() {
        writeln!(stdout, "ORPHAN ({}) — no tracked references, not exported", orphans.len())?;
        for r in &orphans {
            let lines = r.end_line - r.start_line + 1;
            writeln!(stdout, "  {} {} {}:{} ({})",
                r.node_type, r.name, r.file_path, r.start_line, plural(lines, "line"))?;
            if !compact {
                for line in r.code_content.lines().take(5) {
                    writeln!(stdout, "    {}", line)?;
                }
                if r.code_content.lines().count() > 5 {
                    writeln!(stdout, "    ...")?;
                }
            }
        }
    }

    if !exported_unused.is_empty() {
        if !orphans.is_empty() { writeln!(stdout)?; }
        writeln!(stdout, "EXPORTED-UNUSED ({}) — exported/public, no tracked callers", exported_unused.len())?;
        for r in &exported_unused {
            let lines = r.end_line - r.start_line + 1;
            writeln!(stdout, "  {} {} {}:{} ({})",
                r.node_type, r.name, r.file_path, r.start_line, plural(lines, "line"))?;
            if !compact {
                for line in r.code_content.lines().take(5) {
                    writeln!(stdout, "    {}", line)?;
                }
                if r.code_content.lines().count() > 5 {
                    writeln!(stdout, "    ...")?;
                }
            }
        }
    }

    Ok(())
}

// --- centrality subcommand ---

/// CLI arguments for the `centrality` subcommand.
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp centrality",
          about = "Rank architectural chokepoints by betweenness centrality (call graph)")]
pub struct CentralityArgs {
    /// Number of functions to report (default: 15)
    #[arg(long, default_value_t = 15)]
    pub limit: u32,
    /// Include test symbols in the graph (excluded by default)
    #[arg(long)]
    pub include_tests: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// Rank functions by betweenness centrality over the `calls` graph — the
/// structural bridges that lie on the most shortest call paths between other
/// functions. Complements `map`'s caller_count "hot functions" (degree
/// centrality): a chokepoint can have few callers yet route most cross-cluster
/// traffic. CLI-only; not exposed as an MCP tool.
pub fn cmd_centrality(project_root: &Path, args: CentralityArgs) -> Result<()> {
    let CentralityArgs { limit, include_tests, json: json_mode } = args;

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let ranked = crate::graph::centrality::betweenness_centrality(
        conn,
        include_tests,
        limit as usize,
    )?;

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        // Empty → `[]` (array-shaped success), per the CLI JSON-empty contract.
        let items: Vec<serde_json::Value> = ranked.iter().map(|c| {
            serde_json::json!({
                "name": c.name,
                "type": c.node_type,
                "file_path": c.file_path,
                "betweenness": c.score,
                "normalized": c.normalized,
                "caller_count": c.caller_count,
            })
        }).collect();
        writeln!(stdout, "{}", serde_json::to_string(&items)?)?;
        return Ok(());
    }

    if ranked.is_empty() {
        eprintln!(
            "[code-graph] No chokepoints found (graph has no multi-hop call paths{}).",
            if include_tests { "" } else { "; try --include-tests" }
        );
        return Ok(());
    }

    writeln!(stdout, "Architectural chokepoints (betweenness centrality, top {}):", ranked.len())?;
    writeln!(stdout, "(functions on the most shortest call paths between others — high score = structural bridge)\n")?;
    for c in &ranked {
        writeln!(
            stdout,
            "  {:>8.1} ({:.3}) {} {} — {} callers ({})",
            c.score, c.normalized, c.node_type, c.name, c.caller_count, c.file_path
        )?;
    }

    Ok(())
}

/// CLI arguments for the `cycles` subcommand.
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp cycles",
          about = "Detect circular import dependencies (file-level)")]
pub struct CyclesArgs {
    /// Maximum number of cycles to report (default: 50)
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// Detect circular import dependencies — strongly-connected components of the
/// file-level `imports` graph. Each cycle is a set of files that transitively
/// import each other, shown with a representative shortest loop `a → b → … → a`.
/// Reported over imports only: a `calls` cycle is mutual recursion, not a
/// circular import. Most actionable for JS/TS/Python/Go; Rust intra-crate module
/// cycles are frequently benign. CLI-only; not exposed as an MCP tool.
pub fn cmd_cycles(project_root: &Path, args: CyclesArgs) -> Result<()> {
    let CyclesArgs { limit, json: json_mode } = args;

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let edges = crate::storage::queries::all_file_import_edges(conn)?;
    let mut cycles = crate::graph::cycles::find_cycles(&edges);
    cycles.truncate(limit as usize);

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        // Empty → `[]` (array-shaped success), per the CLI JSON-empty contract.
        let items: Vec<serde_json::Value> = cycles.iter().map(|c| {
            serde_json::json!({
                "files": c.files,
                "size": c.size,
                "cycle": c.path,
            })
        }).collect();
        writeln!(stdout, "{}", serde_json::to_string(&items)?)?;
        return Ok(());
    }

    if cycles.is_empty() {
        eprintln!("[code-graph] No circular import dependencies found.");
        return Ok(());
    }

    writeln!(stdout, "Circular import dependencies ({} found):", cycles.len())?;
    writeln!(stdout, "(files that transitively import each other — a → b → … → a)\n")?;
    for c in &cycles {
        writeln!(stdout, "  {}", c.headline())?;
        // When the SCC has more files than the representative loop visits, list them all.
        if c.size + 1 > c.path.len() {
            writeln!(stdout, "    files: {}", c.files.join(", "))?;
        }
    }

    Ok(())
}

/// CLI arguments for the `surprising` subcommand.
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp surprising",
          about = "Surface unexpected cross-module couplings (uncertain / sole-bridge edges)")]
pub struct SurprisingArgs {
    /// Number of connections to report (default: 15)
    #[arg(long, default_value_t = 15)]
    pub limit: u32,
    /// Include test symbols (excluded by default)
    #[arg(long)]
    pub include_tests: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// Rank "surprising connections" — cross-file `calls`/`references` edges scored by
/// resolution confidence (ambiguous > inferred > extracted), whether they cross
/// module boundaries, and whether they are the sole edge between two modules.
/// Surfaces uncertain or non-obvious couplings for review/audit; structural edges
/// (imports/inherits) are excluded. CLI-only; not exposed as an MCP tool.
pub fn cmd_surprising(project_root: &Path, args: SurprisingArgs) -> Result<()> {
    let SurprisingArgs { limit, include_tests, json: json_mode } = args;

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let found =
        crate::graph::surprising::surprising_connections(conn, include_tests, limit as usize)?;

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        // Empty → `[]` (array-shaped success), per the CLI JSON-empty contract.
        let items: Vec<serde_json::Value> = found.iter().map(|c| {
            serde_json::json!({
                "source": c.source,
                "source_file": c.source_file,
                "target": c.target,
                "target_file": c.target_file,
                "relation": c.relation,
                "confidence": c.confidence,
                "score": c.score,
                "why": c.reasons,
            })
        }).collect();
        writeln!(stdout, "{}", serde_json::to_string(&items)?)?;
        return Ok(());
    }

    if found.is_empty() {
        eprintln!(
            "[code-graph] No surprising connections found{}.",
            if include_tests { "" } else { " (try --include-tests)" }
        );
        return Ok(());
    }

    writeln!(stdout, "Surprising connections (top {}):", found.len())?;
    writeln!(stdout, "(score = low resolution confidence + crosses modules + sole bridge between them)\n")?;
    for c in &found {
        writeln!(stdout, "  [{}] {} → {}  ({} {})", c.score, c.source, c.target, c.confidence, c.relation)?;
        writeln!(stdout, "      {} → {}", c.source_file, c.target_file)?;
        writeln!(stdout, "      {}", c.reasons.join("; "))?;
    }

    Ok(())
}

/// CLI arguments for the `report` subcommand.
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp report",
          about = "Consolidated code-health report (summary, hot functions, chokepoints, cycles, surprising, dead code)")]
pub struct ReportArgs {
    /// Items per section (default: 5)
    #[arg(long, default_value_t = 5)]
    pub top: u32,
    /// Include test symbols in the analyses (excluded by default)
    #[arg(long)]
    pub include_tests: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// One-shot architecture/health overview that bundles the structural analyses
/// (hot functions, betweenness chokepoints, import cycles, surprising
/// connections, dead code) plus a corpus summary with edge-confidence breakdown.
/// Pure read-time aggregation of existing analyses. CLI-only; not an MCP tool.
pub fn cmd_report(project_root: &Path, args: ReportArgs) -> Result<()> {
    use crate::domain::{CONF_AMBIGUOUS, CONF_EXTRACTED, CONF_INFERRED};
    let ReportArgs { top, include_tests, json: json_mode } = args;
    let top = top as usize;

    let ctx = CliContext::open(project_root)?;
    let conn = ctx.db.conn();

    let status = crate::storage::queries::get_index_status(conn, false)?;

    // Edge-confidence breakdown.
    let mut conf: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    {
        let mut stmt = conn.prepare("SELECT confidence, COUNT(*) FROM edges GROUP BY confidence")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (c, n) = row?;
            conf.insert(c, n);
        }
    }
    let conf_get = |k: &str| conf.get(k).copied().unwrap_or(0);

    let (_modules, _deps, _entry, hot) = crate::storage::queries::get_project_map(conn)?;
    let chokepoints = crate::graph::centrality::betweenness_centrality(conn, include_tests, top)?;
    let mut cycles = {
        let edges = crate::storage::queries::all_file_import_edges(conn)?;
        crate::graph::cycles::find_cycles(&edges)
    };
    cycles.truncate(top);
    let surprising =
        crate::graph::surprising::surprising_connections(conn, include_tests, top)?;
    let dead = crate::storage::queries::find_dead_code(conn, None, None, include_tests, 3, top as i64)?;

    let mut stdout = std::io::stdout().lock();

    if json_mode {
        // Object envelope (sections may be empty arrays), per the CLI JSON contract.
        let report = serde_json::json!({
            "summary": {
                "files": status.files_count,
                "nodes": status.nodes_count,
                "edges": status.edges_count,
                "confidence": {
                    "extracted": conf_get(CONF_EXTRACTED),
                    "inferred": conf_get(CONF_INFERRED),
                    "ambiguous": conf_get(CONF_AMBIGUOUS),
                },
            },
            "hot_functions": hot.iter().take(top).map(|h| serde_json::json!({
                "name": h.name, "type": h.node_type, "file": h.file, "caller_count": h.caller_count,
            })).collect::<Vec<_>>(),
            "chokepoints": chokepoints.iter().map(|c| serde_json::json!({
                "name": c.name, "file": c.file_path, "betweenness": c.score, "caller_count": c.caller_count,
            })).collect::<Vec<_>>(),
            "import_cycles": cycles.iter().map(|c| serde_json::json!({
                "files": c.files, "size": c.size, "cycle": c.path,
            })).collect::<Vec<_>>(),
            "surprising_connections": surprising.iter().map(|c| serde_json::json!({
                "source": c.source, "target": c.target, "confidence": c.confidence, "score": c.score,
                "source_file": c.source_file, "target_file": c.target_file,
            })).collect::<Vec<_>>(),
            "dead_code": dead.iter().map(|d| serde_json::json!({
                "name": d.name, "type": d.node_type, "file": d.file_path, "line": d.start_line,
            })).collect::<Vec<_>>(),
        });
        writeln!(stdout, "{}", serde_json::to_string(&report)?)?;
        return Ok(());
    }

    writeln!(stdout, "# Code Health Report\n")?;
    writeln!(stdout, "## Summary")?;
    writeln!(stdout, "  {} files · {} nodes · {} edges",
        status.files_count, status.nodes_count, status.edges_count)?;
    writeln!(stdout, "  edge confidence: {} extracted · {} inferred · {} ambiguous",
        conf_get(CONF_EXTRACTED), conf_get(CONF_INFERRED), conf_get(CONF_AMBIGUOUS))?;

    writeln!(stdout, "\n## Hot functions (most-called)")?;
    if hot.is_empty() {
        writeln!(stdout, "  (none)")?;
    }
    for h in hot.iter().take(top) {
        writeln!(stdout, "  {:>4} callers  {} ({}) — {}", h.caller_count, h.name, h.node_type, h.file)?;
    }

    writeln!(stdout, "\n## Architectural chokepoints (betweenness)")?;
    if chokepoints.is_empty() {
        writeln!(stdout, "  (none)")?;
    }
    for c in &chokepoints {
        writeln!(stdout, "  {:>8.1}  {} — {}", c.score, c.name, c.file_path)?;
    }

    writeln!(stdout, "\n## Import cycles")?;
    if cycles.is_empty() {
        writeln!(stdout, "  (none)")?;
    }
    for c in &cycles {
        writeln!(stdout, "  {}", c.headline())?;
        // For larger SCCs the shortest loop omits members — name them so the report is actionable.
        if c.size + 1 > c.path.len() {
            writeln!(stdout, "    files: {}", c.files.join(", "))?;
        }
    }

    writeln!(stdout, "\n## Surprising connections")?;
    if surprising.is_empty() {
        writeln!(stdout, "  (none)")?;
    }
    for c in &surprising {
        writeln!(stdout, "  [{}] {} → {}  ({} {})", c.score, c.source, c.target, c.confidence, c.relation)?;
    }

    writeln!(stdout, "\n## Dead code (unused symbols)")?;
    if dead.is_empty() {
        writeln!(stdout, "  (none)")?;
    }
    for d in &dead {
        writeln!(stdout, "  {} ({}) — {}:{}", d.name, d.node_type, d.file_path, d.start_line)?;
    }

    Ok(())
}

/// CLI arguments for the `benchmark` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp benchmark",
          about = "Benchmark index speed, query latency, token savings")]
pub struct BenchmarkArgs {
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

/// Run benchmark: full index, incremental index, query latency, DB size, token savings.
pub fn cmd_benchmark(project_root: &Path, args: BenchmarkArgs) -> Result<()> {
    use crate::domain::CODE_GRAPH_DIR;
    use crate::indexer::pipeline::{run_full_index, run_incremental_index};
    use std::time::Instant;

    let json_mode = args.json;

    // Create a temporary database for benchmarking
    let data_dir = project_root.join(CODE_GRAPH_DIR);
    std::fs::create_dir_all(&data_dir)?;
    let bench_db_path = data_dir.join("benchmark-temp.db");
    if bench_db_path.exists() {
        std::fs::remove_file(&bench_db_path)?;
    }

    eprintln!("[benchmark] Indexing {}...", project_root.display());

    // 1. Full index timing
    let bench_db = Database::open(&bench_db_path)?;
    let t_full = Instant::now();
    let result = run_full_index(&bench_db, project_root, None, None)?;
    let full_index_ms = t_full.elapsed().as_millis() as u64;

    let files_indexed = result.files_indexed;
    let nodes_created = result.nodes_created;
    let edges_created = result.edges_created;

    eprintln!("[benchmark] Full index: {}ms ({} files, {} nodes, {} edges)",
        full_index_ms, files_indexed, nodes_created, edges_created);

    // 2. Incremental index (no-change detection — should be fast)
    let t_incr = Instant::now();
    let _ = run_incremental_index(&bench_db, project_root, None, None)?;
    let incr_index_ms = t_incr.elapsed().as_millis() as u64;

    eprintln!("[benchmark] Incremental (no-change): {}ms", incr_index_ms);

    // 3. Query latency: run 5 FTS searches, compute P50/P99
    let test_queries = ["function", "error", "config", "parse", "index"];
    let mut query_times_us: Vec<u64> = Vec::with_capacity(test_queries.len());
    let conn = bench_db.conn();

    for q in &test_queries {
        let t_q = Instant::now();
        let _ = queries::fts5_search(conn, q, 10)?;
        query_times_us.push(t_q.elapsed().as_micros() as u64);
    }

    query_times_us.sort();
    let p50_us = query_times_us[query_times_us.len() / 2];
    let p99_us = query_times_us[query_times_us.len() - 1]; // with 5 samples, P99 ≈ max

    eprintln!("[benchmark] Query latency P50: {}us, P99: {}us", p50_us, p99_us);

    // 4. DB size
    let db_size_bytes = std::fs::metadata(&bench_db_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let db_size_mb = db_size_bytes as f64 / (1024.0 * 1024.0);

    // 5. Token savings estimate: avg code_content length / 3.0 tokens per char
    let avg_content_len: f64 = conn
        .query_row(
            "SELECT COALESCE(AVG(LENGTH(code_content)), 0) FROM nodes WHERE code_content IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);
    let avg_tokens = avg_content_len / 3.0;

    // Clean up: drop connection before deleting file
    drop(bench_db);
    if bench_db_path.exists() {
        std::fs::remove_file(&bench_db_path)?;
    }
    // Also clean up WAL/SHM files that SQLite may leave behind
    let wal_path = bench_db_path.with_extension("db-wal");
    let shm_path = bench_db_path.with_extension("db-shm");
    if wal_path.exists() { let _ = std::fs::remove_file(&wal_path); }
    if shm_path.exists() { let _ = std::fs::remove_file(&shm_path); }

    if json_mode {
        let json = serde_json::json!({
            "full_index_ms": full_index_ms,
            "incremental_index_ms": incr_index_ms,
            "files_indexed": files_indexed,
            "nodes_created": nodes_created,
            "edges_created": edges_created,
            "query_p50_us": p50_us,
            "query_p99_us": p99_us,
            "db_size_mb": (db_size_mb * 100.0).round() / 100.0,
            "avg_tokens_per_node": (avg_tokens * 10.0).round() / 10.0,
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "Benchmark Results")?;
        writeln!(stdout, "=================")?;
        writeln!(stdout)?;
        writeln!(stdout, "Full index:          {:>8}ms  ({} files, {} nodes, {} edges)",
            full_index_ms, files_indexed, nodes_created, edges_created)?;
        writeln!(stdout, "Incremental (noop):  {:>8}ms", incr_index_ms)?;
        writeln!(stdout, "Query latency P50:   {:>8}us", p50_us)?;
        writeln!(stdout, "Query latency P99:   {:>8}us", p99_us)?;
        writeln!(stdout, "DB size:             {:>8.2}MB", db_size_mb)?;
        writeln!(stdout, "Avg tokens/node:     {:>8.1}", avg_tokens)?;
    }

    Ok(())
}

// --- snapshot subcommand (nested create/inspect) ---

/// CLI arguments for the `snapshot` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp snapshot",
          about = "Build or inspect a portable graph snapshot")]
pub struct SnapshotArgs {
    #[command(subcommand)]
    pub command: SnapshotCommand,
}

/// `snapshot` sub-subcommands (replaces the hand-rolled args[2]/args[3] dispatch).
#[derive(Subcommand, Debug)]
pub enum SnapshotCommand {
    /// Build a portable graph snapshot (auto zstd when --out ends in .db.zst)
    Create(SnapshotCreateArgs),
    /// Print snapshot metadata as JSON (accepts .db or .db.zst)
    Inspect(SnapshotInspectArgs),
}

/// `snapshot create` arguments.
#[derive(Parser, Debug)]
pub struct SnapshotCreateArgs {
    /// Output path (auto zstd-compresses when it ends in .db.zst)
    #[arg(long)]
    pub out: String,
    /// Include embedding vectors in the snapshot
    #[arg(long)]
    pub include_embeddings: bool,
    /// Project root to snapshot (default: the resolved project root)
    #[arg(long)]
    pub root: Option<String>,
    /// Suppress the "snapshot created" confirmation
    #[arg(long)]
    pub quiet: bool,
}

/// `snapshot inspect` arguments.
#[derive(Parser, Debug)]
pub struct SnapshotInspectArgs {
    /// Snapshot file to inspect (.db or .db.zst; format from magic bytes)
    pub file: String,
}

/// Build a portable graph snapshot. `snapshot create --out <path>
/// [--include-embeddings] [--root <dir>] [--quiet]`.
pub fn cmd_snapshot_create(project_root: &Path, args: SnapshotCreateArgs) -> Result<()> {
    let SnapshotCreateArgs { out, include_embeddings: include, root, quiet } = args;

    let root = root
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| project_root.to_path_buf());

    // Pre-flight checks for --out so SQLite VACUUM INTO doesn't leak its
    // raw "unable to open database file" error when the user passed a dir
    // or a path with a missing parent directory.
    let out_path = std::path::Path::new(&out);
    if out_path.is_dir() || out.ends_with('/') {
        anyhow::bail!(
            "--out '{}' is a directory; expected a file path (e.g. '{}snapshot.db' or '{}snapshot.db.zst')",
            out, out, out
        );
    }
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            anyhow::bail!(
                "--out parent directory does not exist: {} (create it first with `mkdir -p {}`)",
                parent.display(), parent.display()
            );
        }
    }

    crate::snapshot::create(&root, out_path, include)?;
    if !quiet {
        eprintln!("snapshot created: {}", out);
        if out.ends_with(".db.zst") {
            eprintln!(
                "integrity sidecar: {out}.blake3 \u{2014} upload BOTH to the release; \
                 consumers verify the checksum before decompressing"
            );
        }
    }
    Ok(())
}

/// Print snapshot metadata as JSON to stdout. Accepts `.db` or `.db.zst`
/// (format detected from magic bytes, not extension).
pub fn cmd_snapshot_inspect(args: SnapshotInspectArgs) -> Result<()> {
    let meta = crate::snapshot::inspect(std::path::Path::new(&args.file))?;
    println!("{}", serde_json::to_string_pretty(&meta)?);
    Ok(())
}

// --- reindex subcommand ---

/// CLI arguments for the `reindex` subcommand (audit #4 clap migration).
#[derive(Parser, Debug)]
#[command(name = "code-graph-mcp reindex",
          about = "Reset index; with --from-snapshot, refetch the published snapshot")]
pub struct ReindexArgs {
    /// Refetch the published snapshot before indexing (falls back to full index)
    #[arg(long)]
    pub from_snapshot: bool,
    /// Index structure only and skip embeddings (vectors backfill later).
    #[arg(long)]
    pub no_embed: bool,
}

/// `reindex [--from-snapshot]` — wipe `.code-graph/` index files and re-fetch
/// snapshot (or full-index if no snapshot available). Without `--from-snapshot`,
/// behaves identically to `incremental-index`.
///
/// Equivalent to user-side `rm -rf .code-graph/index.db*` + restarting the
/// MCP server, but with optional snapshot-bootstrap acceleration.
pub fn cmd_reindex(project_root: &Path, args: ReindexArgs) -> Result<()> {
    let from_snapshot = args.from_snapshot;
    let no_embed = args.no_embed;
    let cg_dir = project_root.join(crate::domain::CODE_GRAPH_DIR);

    if from_snapshot && cg_dir.exists() {
        // Remove just index.db + WAL files; leave usage.jsonl etc. intact.
        for name in ["index.db", "index.db-wal", "index.db-shm"] {
            let _ = std::fs::remove_file(cg_dir.join(name));
        }
    }

    if from_snapshot {
        if let Some(url) = crate::snapshot::resolve_snapshot_source(project_root) {
            match crate::snapshot::try_install(&url, project_root) {
                Ok(commit) => {
                    eprintln!("Snapshot installed at commit {commit}");
                    return cmd_incremental_index(project_root, false, no_embed);
                }
                Err(e) => eprintln!("Snapshot install failed ({e}), falling back to full index"),
            }
        } else {
            eprintln!("No snapshot source resolved, falling back to full index");
        }
    }

    cmd_incremental_index(project_root, false, no_embed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_embed_flag_parses_on_index_commands() {
        // `--no-embed` is the published fast-path opt-out: structure-first index,
        // skip the slow embedding pass. Verify it wires on every index command and
        // defaults off (embedding stays the default so existing behaviour holds).
        assert!(IncrementalIndexArgs::parse_from(["incremental-index", "--no-embed"]).no_embed);
        assert!(!IncrementalIndexArgs::parse_from(["incremental-index"]).no_embed);
        assert!(ReindexArgs::parse_from(["reindex", "--no-embed"]).no_embed);
        assert!(!ReindexArgs::parse_from(["reindex"]).no_embed);
        assert!(
            RebuildIndexArgs::parse_from(["rebuild-index", "--confirm", "--no-embed"]).no_embed
        );
        assert!(!RebuildIndexArgs::parse_from(["rebuild-index", "--confirm"]).no_embed);
    }

    #[test]
    fn test_no_embed_builds_structural_index_without_vectors() {
        // A --no-embed full index must still produce the structural graph (nodes),
        // and must leave zero vectors regardless of model availability — the fast,
        // query-ready state the flag promises.
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "fn alpha() { beta(); }\nfn beta() {}\n").unwrap();
        let db_path = root.join(CODE_GRAPH_DIR).join("index.db");

        build_full_index_at(&db_path, root, true, true).unwrap();

        let db = Database::open_nondestructive(&db_path).unwrap();
        let nodes: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert!(nodes > 0, "structure index must be built even with --no-embed");
        let (embedded, _embeddable) = queries::count_nodes_with_vectors(db.conn()).unwrap();
        assert_eq!(embedded, 0, "--no-embed must leave zero vectors");
    }

    #[test]
    fn test_aggregate_recommendations_research_after_answer_and_observe() {
        // Append order = chronological. Each answered deny "arms"; the next
        // grep/read event is a re-search (inline answer didn't end the hunt).
        //   t1 answered deny → t2 grep observe  = re-search
        //   t3 answered deny → t4 cli use       = conversion, NOT re-search
        //   t5 answered deny → t6 read observe  = re-search (read after answer)
        //   t7 UNanswered deny → t8 grep observe = not armed (only answered denies)
        let content = "\
{\"ts\":\"t1\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"t2\",\"hook\":\"grep\",\"action\":\"observe\"}
{\"ts\":\"t3\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"t4\",\"hook\":\"cli\",\"action\":\"use\",\"cmd\":\"grep\"}
{\"ts\":\"t5\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"t6\",\"hook\":\"read\",\"action\":\"observe\"}
{\"ts\":\"t7\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":false}
{\"ts\":\"t8\",\"hook\":\"grep\",\"action\":\"observe\"}
";
        let s = aggregate_recommendations_jsonl(content);
        assert_eq!(s.deny_answered, 3, "t1,t3,t5 answered");
        assert_eq!(s.deny_unanswered, 1, "t7 unanswered");
        assert_eq!(s.researched_after_answer, 2,
            "t1→t2 (re-grep) and t5→t6 (read) count; t3→t4 (cli use) is a conversion, not re-search");
        // Both follow-ups here are observe (a file read acting on the delivered
        // answer) → neither a sustained drill-down nor a fall-through cg failed.
        assert_eq!(s.sustained_after_answer, 0, "no follow-up was itself answered by cg");
        assert_eq!(s.fallthrough_after_answer, 0, "no follow-up was a search cg couldn't satisfy");
        assert_eq!(s.observe, 3, "t2,t6,t8 observes");
        assert_eq!(s.cli_uses, 1, "t4");
        assert_eq!(s.by_action.get("observe"), None, "observe is not a recommendation action");
        assert_eq!(s.total, 4, "4 denies counted; observe + cli use excluded from total");
    }

    #[test]
    fn test_aggregate_recommendations_followup_split_sustained_vs_fallthrough() {
        // The honest split of "follow-up after an answered deny": cg either
        // answered the next step too (sustained drill-down — a win), the model
        // read a file (observe — acting on the answer), or fell through to a
        // search cg couldn't satisfy (the real insufficiency). `use` between pairs
        // is a clean disarm (conversion, not a search).
        //   L1 answered deny → L2 answered deny       = sustained (cg kept up); L2 re-arms
        //   L2 (armed) → L3 cli use                   = conversion → disarm
        //   L4 answered deny → L5 static deny         = fall-through (cg couldn't); L5 unanswered → no arm
        //   L6 answered deny → L7 grep hint (advisory) = fall-through (no delivered answer)
        //   L8 answered deny → L9 read observe        = neither (acting on answer) → disarm
        //   L10 answered deny → L11 cli use           = conversion → disarm
        //   L12 answered deny → (end)                 = no follow-up (answer sufficed)
        let content = "\
{\"ts\":\"L1\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"L2\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true,\"mode\":\"grep\"}
{\"ts\":\"L3\",\"hook\":\"cli\",\"action\":\"use\",\"cmd\":\"grep\"}
{\"ts\":\"L4\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"L5\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":false}
{\"ts\":\"L6\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"L7\",\"hook\":\"grep\",\"action\":\"hint\"}
{\"ts\":\"L8\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"L9\",\"hook\":\"read\",\"action\":\"observe\"}
{\"ts\":\"L10\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"L11\",\"hook\":\"cli\",\"action\":\"use\",\"cmd\":\"grep\"}
{\"ts\":\"L12\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
";
        let s = aggregate_recommendations_jsonl(content);
        assert_eq!(s.deny_answered, 7, "L1,L2,L4,L6,L8,L10,L12 answered");
        assert_eq!(s.deny_unanswered, 1, "L5 static");
        assert_eq!(s.researched_after_answer, 4, "L2,L5,L7,L9 are search follow-ups; L3/L11 use disarm");
        assert_eq!(s.sustained_after_answer, 1, "L1→L2: cg answered the follow-up too");
        assert_eq!(s.fallthrough_after_answer, 2, "L4→L5 (static) and L6→L7 (advisory hint): cg couldn't satisfy");
        assert_eq!(s.observe, 1, "L9");
        assert_eq!(s.cli_uses, 2, "L3,L11");
    }

    #[test]
    fn test_aggregate_recommendations_same_pattern_regrep_is_fallthrough() {
        // Pattern fingerprint tightens `sustained` (the documented upper bound):
        // a verbatim re-grep of the SAME denied pattern after cg answered means
        // the inline answer was ignored/insufficient → fall-through, NOT a
        // drill-down win (and NOT "acting on the answer" even when it lands as a
        // grep observe within the cooldown window). A DIFFERENT pattern is genuine
        // drill-down → sustained. A follow-up WITHOUT a pattern (read observe, or
        // any pre-fix event) keeps the old behavior — back-compatible.
        //   A1 answered deny pattern=foo → arm(foo)
        //   A2 answered deny pattern=foo → SAME (re-deny after cooldown) → fall-through; re-arm(foo)
        //   A3 cli use                   → disarm
        //   A4 answered deny pattern=bar → arm(bar)
        //   A5 grep observe pattern=bar  → SAME (re-grep within cooldown) → fall-through
        //   A6 answered deny pattern=baz → arm(baz)
        //   A7 answered deny pattern=qux → DIFFERENT → sustained; re-arm(qux)
        //   A8 cli use                   → disarm
        //   A9 answered deny pattern=zap → arm(zap)
        //   A10 read observe (no pattern)→ neither (acting on the answer)
        let content = "\
{\"ts\":\"A1\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true,\"pattern\":\"foo\"}
{\"ts\":\"A2\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true,\"pattern\":\"foo\"}
{\"ts\":\"A3\",\"hook\":\"cli\",\"action\":\"use\",\"cmd\":\"grep\"}
{\"ts\":\"A4\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true,\"pattern\":\"bar\"}
{\"ts\":\"A5\",\"hook\":\"grep\",\"action\":\"observe\",\"pattern\":\"bar\"}
{\"ts\":\"A6\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true,\"pattern\":\"baz\"}
{\"ts\":\"A7\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true,\"pattern\":\"qux\"}
{\"ts\":\"A8\",\"hook\":\"cli\",\"action\":\"use\",\"cmd\":\"grep\"}
{\"ts\":\"A9\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true,\"pattern\":\"zap\"}
{\"ts\":\"A10\",\"hook\":\"read\",\"action\":\"observe\"}
";
        let s = aggregate_recommendations_jsonl(content);
        assert_eq!(s.deny_answered, 6, "A1,A2,A4,A6,A7,A9 answered");
        assert_eq!(s.researched_after_answer, 4, "A2,A5,A7,A10 follow answered denies; A3/A8 use disarm");
        assert_eq!(s.fallthrough_after_answer, 2,
            "A1→A2 (same-pattern re-deny) and A4→A5 (same-pattern re-grep observe): answer ignored");
        assert_eq!(s.sustained_after_answer, 1, "A6→A7: different pattern = genuine drill-down cg also answered");
        assert_eq!(s.observe, 2, "A5,A10");
        assert_eq!(s.cli_uses, 2, "A3,A8");
    }

    #[test]
    fn test_aggregate_recommendations_inconclusive_followup_excluded_from_fallthrough() {
        // Consumer-data over-count fix: a follow-up after an answered deny that is
        // itself a NULL signal about the prior answer must NOT count as fall-through.
        // Two shapes — `no-hits` (cg ran the next grep, found nothing → a NEW query,
        // since a verbatim re-grep of the answered pattern would re-hit it) and
        // `unavailable` (cg CLI couldn't run → infra). Same honesty principle as the
        // v0.64 drill-down/observe exclusion. Same-pattern still wins (verbatim
        // re-grep = answer ignored = real fall-through, even if it now finds nothing).
        //   N1 answered deny → N2 grep hint fallthrough=no-hits          = inconclusive
        //   N3 answered deny → N4 grep deny answered:false reason=unavail = inconclusive
        //   N5 answered deny → N6 grep static deny (answered:false)       = fall-through (cg couldn't)
        //   N7 answered deny pattern=foo → N8 same-pattern deny no-hits   = fall-through (pattern wins)
        let content = "\
{\"ts\":\"N1\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"N2\",\"hook\":\"grep\",\"action\":\"hint\",\"fallthrough\":\"no-hits\"}
{\"ts\":\"N3\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"N4\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":false,\"reason\":\"unavailable\"}
{\"ts\":\"N5\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"N6\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":false}
{\"ts\":\"N7\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true,\"pattern\":\"foo\"}
{\"ts\":\"N8\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":false,\"pattern\":\"foo\",\"fallthrough\":\"no-hits\"}
";
        let s = aggregate_recommendations_jsonl(content);
        assert_eq!(s.deny_answered, 4, "N1,N3,N5,N7 answered");
        assert_eq!(s.researched_after_answer, 4, "N2,N4,N6,N8 all follow answered denies");
        assert_eq!(s.followup_inconclusive, 2, "N2 (no-hits) + N4 (unavailable): null signal, excluded");
        assert_eq!(s.fallthrough_after_answer, 2,
            "N6 (static deny cg couldn't satisfy) + N8 (same-pattern re-grep wins over no-hits)");
        assert_eq!(s.sustained_after_answer, 0, "no follow-up was itself answered by cg");
    }

    #[test]
    fn test_aggregate_recommendations_inject_arms_and_scores_fallthrough_vs_sustained() {
        // Compound-grep PostToolUse inject: an ANSWERED inject (cg delivered the
        // AST-aware view of a compound-command grep, permission-neutrally) arms the
        // funnel exactly like an answered deny. The immediately-next search event
        // scores the inject's sufficiency, parallel to deny→fallthrough:
        //   I1 inject pattern=foo → arm(foo)
        //   I2 grep observe pattern=foo  → SAME pattern re-grep = inline answer ignored → fall-through; disarm
        //   I3 inject pattern=bar → arm(bar)
        //   I4 grep deny answered=true pattern=qux → DIFFERENT pattern, cg also answered = sustained; re-arm(qux)
        //   I5 cli use → conversion → disarm
        //   I6 inject pattern=baz → arm(baz)
        //   I7 (end) → no follow-up (answer sufficed)
        // inject also counts in total/by_action via the generic map (it is a
        // recommendation event, like deny/hint — NOT observe/use/live_impact).
        let content = "\
{\"ts\":\"I1\",\"hook\":\"grep\",\"action\":\"inject\",\"answered\":true,\"pattern\":\"foo\",\"mode\":\"grep\"}
{\"ts\":\"I2\",\"hook\":\"grep\",\"action\":\"observe\",\"pattern\":\"foo\"}
{\"ts\":\"I3\",\"hook\":\"grep\",\"action\":\"inject\",\"answered\":true,\"pattern\":\"bar\",\"mode\":\"grep\"}
{\"ts\":\"I4\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true,\"pattern\":\"qux\"}
{\"ts\":\"I5\",\"hook\":\"cli\",\"action\":\"use\",\"cmd\":\"grep\"}
{\"ts\":\"I6\",\"hook\":\"grep\",\"action\":\"inject\",\"answered\":true,\"pattern\":\"baz\",\"mode\":\"grep\"}
";
        let s = aggregate_recommendations_jsonl(content);
        assert_eq!(s.by_action.get("inject"), Some(&3), "I1,I3,I6 are inject recommendation events");
        assert_eq!(*s.by_hook.get("grep").unwrap(), 4, "I1,I3,I4,I6 are grep recommendation events (I2 observe excluded)");
        assert_eq!(s.total, 4, "I1,I3,I4,I6 in total; I2 observe + I5 use excluded");
        assert_eq!(s.researched_after_answer, 2, "I2 (after I1) and I4 (after I3) follow answered injects");
        assert_eq!(s.fallthrough_after_answer, 1, "I1→I2: same-pattern re-grep = inline inject ignored");
        assert_eq!(s.sustained_after_answer, 1, "I3→I4: different pattern, cg also answered = drill-down");
        assert_eq!(s.observe, 1, "I2");
        assert_eq!(s.cli_uses, 1, "I5");
    }

    #[test]
    fn test_aggregate_recommendations_counts_live_impact_separately() {
        // v0.63 — SessionStart live-context injections are a separate counter,
        // like observe/use: NOT in total/by_action, and they don't trip the
        // re-search arming (hook:"session" is not a grep/read search event).
        let content = "\
{\"ts\":\"t1\",\"hook\":\"session\",\"action\":\"live_impact\",\"blast\":72,\"direct\":41,\"wip\":true}
{\"ts\":\"t2\",\"hook\":\"grep\",\"action\":\"deny\",\"answered\":true}
{\"ts\":\"t3\",\"hook\":\"session\",\"action\":\"live_impact\",\"blast\":3,\"direct\":1,\"wip\":false}
{\"ts\":\"t4\",\"hook\":\"grep\",\"action\":\"hint\"}
";
        let s = aggregate_recommendations_jsonl(content);
        assert_eq!(s.live_impact, 2, "t1,t3 live_impact");
        assert_eq!(s.total, 2, "only the deny + hint are recommendation events");
        assert_eq!(s.by_action.get("live_impact"), None, "live_impact is not a recommendation action");
        assert_eq!(s.by_hook.get("session"), None, "session hook is not a recommendation hook");
        // t2 answered deny arms; t3 is live_impact (not a search event) → it must
        // NOT count as a re-search and must disarm.
        assert_eq!(s.researched_after_answer, 0, "live_impact after an answered deny is not a re-search");
    }

    #[test]
    fn resolve_project_root_prefers_existing_index_at_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let idx_dir = cwd.join(CODE_GRAPH_DIR);
        std::fs::create_dir_all(&idx_dir).unwrap();
        std::fs::write(idx_dir.join("index.db"), b"").unwrap();
        assert_eq!(resolve_project_root_from(cwd), cwd);
    }

    // Helper: give `dir` a `.code-graph/index.db` (explicit join per #937).
    fn write_index(dir: &Path) {
        let idx = dir.join(CODE_GRAPH_DIR);
        std::fs::create_dir_all(&idx).unwrap();
        std::fs::write(idx.join("index.db"), b"").unwrap();
    }

    #[test]
    fn resolve_project_root_skips_stray_nested_index() {
        // monorepo: root has .git + index; a subdir carries a STRAY index (relic
        // from an older binary) but no .git of its own. Resolving from the subdir
        // must climb to the real root, not pin the stray nested index.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write_index(root);
        let sub = root.join("backend");
        std::fs::create_dir_all(&sub).unwrap();
        write_index(&sub);
        assert_eq!(resolve_project_root_from(&sub), root);
    }

    #[test]
    fn resolve_project_root_nested_index_with_own_git_still_wins() {
        // A real nested repo (submodule / vendored project) has its OWN .git, so
        // its index is legitimate even under an indexed parent — keep it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write_index(root);
        let sub = root.join("vendored");
        std::fs::create_dir_all(sub.join(".git")).unwrap();
        write_index(&sub);
        assert_eq!(resolve_project_root_from(&sub), sub);
    }

    #[test]
    fn resolve_project_root_standalone_index_no_ancestor_still_wins() {
        // No ancestor index → a cwd index is the genuine root (guards against the
        // stray-skip over-reaching into the common single-project case).
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        write_index(cwd);
        assert_eq!(resolve_project_root_from(cwd), cwd);
    }

    #[test]
    fn resolve_project_root_cwd_own_git_no_index_is_boundary() {
        // A fresh project dir with its own `.git` but no index yet (the metrics-
        // isolation fixture) roots at itself, never an indexed ancestor.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_index(root);
        let sub = root.join("pkg");
        std::fs::create_dir_all(sub.join(".git")).unwrap();
        assert_eq!(resolve_project_root_from(&sub), sub);
    }

    #[test]
    fn resolve_project_root_home_boundary_ignores_outer_index() {
        // `~` is both a git repo AND indexed; a project below it with its own
        // index but no `.git` must resolve to itself, not be hijacked to `~`.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".git")).unwrap();
        write_index(home);
        let proj = home.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        write_index(&proj);
        assert_eq!(resolve_project_root_bounded(&proj, Some(home)), proj);
    }

    #[test]
    fn resolve_project_root_non_git_monorepo_prefers_indexed_ancestor() {
        // No `.git` anywhere: a stray subdir index under a non-git indexed root
        // resolves to the indexed ancestor (parity with the JS resolver).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_index(root);
        let sub = root.join("backend");
        std::fs::create_dir_all(&sub).unwrap();
        write_index(&sub);
        // Bound at the tmp parent so the real `~/.code-graph` can't interfere.
        assert_eq!(resolve_project_root_bounded(&sub, root.parent()), root);
    }

    #[test]
    fn resolve_project_root_unindexed_git_root_uses_indexed_mid() {
        // outer/.git (unindexed) / proj/index / backend/stray-index → resolve to
        // the indexed mid dir, not the empty git root.
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path();
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        let proj = outer.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        write_index(&proj);
        let backend = proj.join("backend");
        std::fs::create_dir_all(&backend).unwrap();
        write_index(&backend);
        assert_eq!(resolve_project_root_bounded(&backend, outer.parent()), proj);
    }

    #[test]
    fn test_record_cli_use_rotates_recommendations_jsonl() {
        // record_cli_use is the sole reader of CODE_GRAPH_INTERNAL and no other
        // test mutates it, so toggling it here is race-free.
        std::env::remove_var("CODE_GRAPH_INTERNAL");
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cg = root.join(CODE_GRAPH_DIR);
        std::fs::create_dir_all(&cg).unwrap();
        let rec = cg.join("recommendations.jsonl");
        // Pre-fill > 1MB of prior recommendation lines.
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&rec).unwrap();
            let pad = "x".repeat(1024);
            for i in 0..1200 {
                writeln!(f, "{{\"old\":{i},\"pad\":\"{pad}\"}}").unwrap();
            }
        }
        assert!(std::fs::metadata(&rec).unwrap().len() > 1_048_576);

        record_cli_use(root, "callgraph");

        let size = std::fs::metadata(&rec).unwrap().len();
        assert!(size < 600_000, "recommendations.jsonl should be rotated, got {size} bytes");
        // The freshly recorded use line is last + valid; first surviving line is whole JSON.
        let content = std::fs::read_to_string(&rec).unwrap();
        let last: serde_json::Value =
            serde_json::from_str(content.trim().lines().last().unwrap()).unwrap();
        assert_eq!(last["action"], "use");
        assert_eq!(last["cmd"], "callgraph");
        serde_json::from_str::<serde_json::Value>(content.lines().next().unwrap()).unwrap();
    }

    #[test]
    fn test_record_cli_use_skips_when_no_metrics_sentinel_present() {
        // A `.code-graph/.no-metrics` sentinel silences the recommendations-log
        // writer so a dev/dogfood checkout's own CLI runs (functionality testing,
        // sims, ad-hoc dev) don't self-pollute its adoption metrics with `use`
        // events that read back as genuine consumer traffic. Safe to toggle
        // CODE_GRAPH_INTERNAL (no test SETS it to "1"; parallel removes are idempotent).
        std::env::remove_var("CODE_GRAPH_INTERNAL");
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cg = root.join(CODE_GRAPH_DIR);
        std::fs::create_dir_all(&cg).unwrap();
        let rec = cg.join("recommendations.jsonl");

        // No sentinel → the use event is recorded.
        record_cli_use(root, "grep");
        let after_first = std::fs::read_to_string(&rec).unwrap();
        assert_eq!(after_first.lines().count(), 1, "use event recorded when no sentinel present");

        // Sentinel present → record_cli_use is a no-op; the file is byte-unchanged.
        std::fs::write(cg.join(crate::domain::NO_METRICS_SENTINEL), b"").unwrap();
        record_cli_use(root, "callgraph");
        let after_second = std::fs::read_to_string(&rec).unwrap();
        assert_eq!(after_second, after_first, "sentinel must suppress the second use event");
    }

    #[test]
    fn resolve_project_root_climbs_to_git_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let subdir = root.join("sub").join("deep");
        std::fs::create_dir_all(&subdir).unwrap();
        assert_eq!(resolve_project_root_from(&subdir), root);
    }

    #[test]
    fn resolve_project_root_falls_back_to_cwd_when_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        // canonicalize both sides: on macOS `/tmp` ↔ `/private/tmp` symlinking;
        // on Linux they match directly, so this is a no-op but keeps the test portable.
        assert_eq!(resolve_project_root_from(cwd), cwd);
    }

    #[test]
    fn is_non_project_cwd_bare_dir_is_non_project() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(is_non_project_cwd(tmp.path()));
    }

    #[test]
    fn is_non_project_cwd_each_marker_makes_it_a_project() {
        for marker in PROJECT_MARKERS {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join(marker), b"").unwrap();
            assert!(
                !is_non_project_cwd(tmp.path()),
                "{marker} should classify cwd as a project"
            );
        }
    }

    #[test]
    fn non_project_stub_answers_initialize_tools_list_and_rejects_rest() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"x"}}"#,
            "\n",
        );
        let mut out: Vec<u8> = Vec::new();
        serve_non_project_stub(std::io::Cursor::new(input), &mut out).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
        // The notification (no `id`) produces no response → exactly 3 responses.
        assert_eq!(lines.len(), 3, "got: {lines:?}");

        let init: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            init["result"]["serverInfo"]["name"],
            "code-graph-mcp (non-project stub)"
        );

        let tl: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(tl["result"]["tools"], serde_json::json!([]));

        let call: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(call["error"]["code"], -32601);
    }

    #[test]
    fn cleanup_legacy_db_files_removes_empty_legacy_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Empty legacy files — should be removed
        std::fs::write(dir.join("code-graph.db"), b"").unwrap();
        std::fs::write(dir.join("code_graph.db"), b"").unwrap();
        std::fs::write(dir.join("graph.db"), b"").unwrap();
        // Non-empty legacy file — must NOT be removed (guard against deleting real data)
        std::fs::write(dir.join("index.db"), b"real data").unwrap();
        // Unrelated file — must NOT be touched
        std::fs::write(dir.join("usage.jsonl"), b"").unwrap();

        cleanup_legacy_db_files(dir);

        assert!(!dir.join("code-graph.db").exists());
        assert!(!dir.join("code_graph.db").exists());
        assert!(!dir.join("graph.db").exists());
        assert!(dir.join("index.db").exists(), "non-empty index.db must survive");
        assert!(dir.join("usage.jsonl").exists(), "unrelated file must survive");
    }

    #[test]
    fn cleanup_legacy_db_files_keeps_non_empty_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // If a legacy file has content, it might be a real backup — don't delete.
        std::fs::write(dir.join("graph.db"), b"some content").unwrap();
        cleanup_legacy_db_files(dir);
        assert!(dir.join("graph.db").exists());
    }

    #[test]
    fn resolve_project_root_prefers_cwd_index_over_git_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let subdir = root.join("sub");
        let sub_idx = subdir.join(CODE_GRAPH_DIR);
        std::fs::create_dir_all(&sub_idx).unwrap();
        std::fs::write(sub_idx.join("index.db"), b"").unwrap();
        assert_eq!(resolve_project_root_from(&subdir), subdir);
    }

    #[test]
    fn test_normalize_type_filter() {
        assert_eq!(normalize_type_filter("fn"), vec!["function", "method"]);
        assert_eq!(normalize_type_filter("class"), vec!["class"]);
        assert_eq!(normalize_type_filter("trait"), vec!["interface", "trait"]);
        assert!(normalize_type_filter("unknown").is_empty());
    }

    #[test]
    fn test_format_node_compact() {
        let node = queries::NodeResult {
            id: 1,
            file_id: 1,
            node_type: "function".into(),
            name: "foo".into(),
            qualified_name: Some("MyClass::foo".into()),
            start_line: 10,
            end_line: 20,
            code_content: String::new(),
            signature: None,
            doc_comment: None,
            context_string: None,
            name_tokens: None,
            return_type: Some("Result<Value>".into()),
            param_types: Some("name: &str, value: i64".into()),
            is_test: false,
        };
        let formatted = format_node_compact(&node, "src/lib.rs");
        assert!(formatted.contains("fn MyClass::foo"));
        assert!(formatted.contains("src/lib.rs:10-20"));
        assert!(formatted.contains("(name: &str, value: i64)"));
        assert!(formatted.contains("-> Result<Value>"));
    }

    #[test]
    fn test_parse_rg_json_empty() {
        let root = Path::new("/project");
        assert!(parse_rg_json(b"", root).is_empty());
    }

    #[test]
    fn test_parse_rg_json_match() {
        let root = Path::new("/project");
        let json_line = br#"{"type":"match","data":{"path":{"text":"/project/src/main.rs"},"line_number":42,"lines":{"text":"fn main() {\n"}}}"#;
        let matches = parse_rg_json(json_line, root);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, "src/main.rs");
        assert_eq!(matches[0].line, 42);
    }

    #[test]
    fn test_aggregate_usage_empty() {
        let s = aggregate_usage_jsonl("", None);
        assert_eq!(s.sessions, 0);
        assert_eq!(s.parse_errors, 0);
        assert!(s.tools.is_empty());
        assert_eq!(s.total_tool_calls(), 0);
    }

    #[test]
    fn test_aggregate_usage_skips_malformed_and_blank() {
        let content = "\n\nnot-json\n{\"ts\":\"2026-04-20T00:00:00Z\",\"v\":\"0.12.1\",\"tools\":{}}\n";
        let s = aggregate_usage_jsonl(content, None);
        assert_eq!(s.sessions, 1);
        assert_eq!(s.parse_errors, 1);
    }

    #[test]
    fn test_aggregate_usage_merges_tool_counts_across_sessions() {
        let line1 = r#"{"ts":"2026-04-19T10:00:00Z","v":"0.12.0","tools":{"get_call_graph":{"n":2,"ms":200,"err":0,"max_ms":150},"project_map":{"n":1,"ms":1000,"err":0,"max_ms":1000}}}"#;
        let line2 = r#"{"ts":"2026-04-20T10:00:00Z","v":"0.12.1","tools":{"get_call_graph":{"n":3,"ms":900,"err":1,"max_ms":500}}}"#;
        let content = format!("{}\n{}\n", line1, line2);
        let s = aggregate_usage_jsonl(&content, None);
        assert_eq!(s.sessions, 2);
        assert_eq!(s.total_tool_calls(), 6);

        let cg = s.tools.get("get_call_graph").unwrap();
        assert_eq!(cg.n, 5);
        assert_eq!(cg.total_ms, 1100);
        assert_eq!(cg.err, 1);
        assert_eq!(cg.max_ms, 500); // max across sessions

        let pm = s.tools.get("project_map").unwrap();
        assert_eq!(pm.n, 1);
        assert_eq!(pm.max_ms, 1000);

        assert_eq!(s.versions.len(), 2);
        assert!(s.versions.contains("0.12.0") && s.versions.contains("0.12.1"));
        assert_eq!(s.first_ts.as_deref(), Some("2026-04-19T10:00:00Z"));
        assert_eq!(s.last_ts.as_deref(), Some("2026-04-20T10:00:00Z"));
    }

    #[test]
    fn test_aggregate_funnel_deny_and_hint_to_use() {
        // s1: deny + called cg (converted). s2: deny + NO cg (not converted).
        // s3: hint + called cg. s4: no recs (ignored by funnel). s5: deny but only
        // a housekeeping tool (get_index_status) → NOT counted as cg use.
        let s1 = r#"{"ts":"2026-06-10T10:00:00Z","v":"0.45.4","tools":{"get_call_graph":{"n":1,"ms":5,"err":0,"max_ms":5}},"recs":{"deny":2,"hint":0}}"#;
        let s2 = r#"{"ts":"2026-06-10T11:00:00Z","v":"0.45.4","tools":{},"recs":{"deny":1,"hint":1}}"#;
        let s3 = r#"{"ts":"2026-06-10T12:00:00Z","v":"0.45.4","tools":{"find_references":{"n":3,"ms":9,"err":0,"max_ms":4}},"recs":{"deny":0,"hint":1}}"#;
        let s4 = r#"{"ts":"2026-06-10T13:00:00Z","v":"0.45.4","tools":{"get_call_graph":{"n":1,"ms":5,"err":0,"max_ms":5}}}"#;
        let s5 = r#"{"ts":"2026-06-10T14:00:00Z","v":"0.45.4","tools":{"get_index_status":{"n":1,"ms":0,"err":0,"max_ms":0}},"recs":{"deny":1,"hint":0}}"#;
        let content = format!("{s1}\n{s2}\n{s3}\n{s4}\n{s5}\n");
        let s = aggregate_usage_jsonl(&content, None);
        // deny sessions: s1, s2, s5 = 3; of those, only s1 called a cg query tool.
        assert_eq!(s.sessions_with_deny, 3, "s1+s2+s5 saw a deny");
        assert_eq!(s.sessions_with_deny_and_cg, 1, "only s1 called a cg query tool (s5's get_index_status is housekeeping)");
        // hint sessions: s2, s3 = 2; of those, only s3 called cg.
        assert_eq!(s.sessions_with_hint, 2);
        assert_eq!(s.sessions_with_hint_and_cg, 1);
    }

    #[test]
    fn test_version_sort_key_is_numeric_not_lexical() {
        // Regression: the stats `versions:` list is stored in a BTreeSet (lexical),
        // so "0.5.40" sorted AFTER "0.32.2". version_sort_key must order by numeric
        // (major, minor, patch) so the displayed list reads in true version order.
        let mut vs = vec!["0.32.2", "0.5.40", "0.11.0", "0.9.0", "0.5.43", "0.7.1"];
        vs.sort_by_key(|v| version_sort_key(v));
        assert_eq!(vs, vec!["0.5.40", "0.5.43", "0.7.1", "0.9.0", "0.11.0", "0.32.2"]);
        // Lexical sort would have put "0.11.0"/"0.32.2" before "0.5.40" — guard that.
        assert!(
            vs.iter().position(|v| *v == "0.5.40").unwrap()
                < vs.iter().position(|v| *v == "0.11.0").unwrap(),
            "0.5.40 must sort before 0.11.0 (numeric), not after (lexical)"
        );
        // Odd/suffixed components fall back to 0 without panicking.
        assert_eq!(version_sort_key("0.5.40-rc1"), (0, 5, 40));
        assert_eq!(version_sort_key("weird"), (0, 0, 0));
        assert_eq!(version_sort_key("1.2"), (1, 2, 0));
    }

    #[test]
    fn test_aggregate_usage_last_n_keeps_tail() {
        let lines: Vec<String> = (0..5).map(|i|
            format!(r#"{{"ts":"2026-04-2{}T00:00:00Z","v":"0.12.1","tools":{{"t":{{"n":1,"ms":{},"err":0,"max_ms":{}}}}}}}"#, i, (i + 1) * 10, (i + 1) * 10)
        ).collect();
        let content = lines.join("\n");
        let s = aggregate_usage_jsonl(&content, Some(2));
        assert_eq!(s.sessions, 2);
        let t = s.tools.get("t").unwrap();
        // Last 2 sessions: ms 40 + 50 = 90
        assert_eq!(t.total_ms, 90);
        assert_eq!(t.max_ms, 50);
    }

    #[test]
    fn test_aggregate_recommendations_counts_by_action_and_hook() {
        let content = [
            r#"{"ts":"t1","hook":"grep","action":"deny"}"#,
            r#"{"ts":"t2","hook":"grep","action":"hint"}"#,
            r#"  "#,                                   // blank → skipped
            r#"{not json}"#,                           // malformed → skipped, not counted
            r#"{"ts":"t3","hook":"read","action":"hint"}"#,
        ].join("\n");
        let s = aggregate_recommendations_jsonl(&content);
        assert_eq!(s.total, 3, "only 3 well-formed lines counted");
        assert_eq!(s.by_action.get("hint").copied(), Some(2));
        assert_eq!(s.by_action.get("deny").copied(), Some(1));
        assert_eq!(s.by_hook.get("grep").copied(), Some(2));
        assert_eq!(s.by_hook.get("read").copied(), Some(1));
    }

    #[test]
    fn test_aggregate_recommendations_cli_uses_and_answered_split() {
        let content = [
            // answered deny (v0.47+) vs static deny (no field = pre-v0.47 or fallback)
            r#"{"ts":"t1","hook":"grep","action":"deny","answered":true}"#,
            r#"{"ts":"t2","hook":"grep","action":"deny","answered":false}"#,
            r#"{"ts":"t3","hook":"grep","action":"deny"}"#,
            r#"{"ts":"t4","hook":"grep","action":"bypass"}"#,
            // CLI conversions: counted in cli_uses, NOT in total/by_action/by_hook
            r#"{"ts":"t5","hook":"cli","action":"use","cmd":"callgraph"}"#,
            r#"{"ts":"t6","hook":"cli","action":"use","cmd":"grep"}"#,
        ].join("\n");
        let s = aggregate_recommendations_jsonl(&content);
        assert_eq!(s.total, 4, "use lines are conversions, not recommendations");
        assert_eq!(s.cli_uses, 2);
        assert_eq!(s.deny_answered, 1);
        assert_eq!(s.deny_unanswered, 2, "answered:false and missing field are both static");
        assert_eq!(s.by_action.get("bypass").copied(), Some(1));
        assert!(!s.by_hook.contains_key("cli"), "cli use lines stay out of by_hook");
    }

    #[test]
    fn test_aggregate_recommendations_empty() {
        let s = aggregate_recommendations_jsonl("");
        assert_eq!(s.total, 0);
        assert!(s.by_action.is_empty());
        assert!(s.by_hook.is_empty());
    }

    #[test]
    fn test_aggregate_usage_search_and_index_merged() {
        let l1 = r#"{"ts":"t1","v":"0.12.1","tools":{"t":{"n":1,"ms":1,"err":0,"max_ms":1}},"search":{"queries":10,"zero":2,"avg_quality":0.8,"fts_only":3,"hybrid":7},"index":{"full_ms":2000,"incr":5,"files":50,"nodes":100}}"#;
        let l2 = r#"{"ts":"t2","v":"0.12.1","tools":{"t":{"n":1,"ms":1,"err":0,"max_ms":1}},"search":{"queries":5,"zero":0,"avg_quality":0.6,"fts_only":1,"hybrid":4},"index":{"full_ms":null,"incr":3,"files":10,"nodes":20}}"#;
        let s = aggregate_usage_jsonl(&format!("{}\n{}", l1, l2), None);
        assert_eq!(s.search_queries, 15);
        assert_eq!(s.search_zero, 2);
        assert_eq!(s.search_fts_only, 4);
        assert_eq!(s.search_hybrid, 11);
        // Weighted quality: (0.8 * 10 + 0.6 * 5) / 15 = 11.0 / 15 ≈ 0.7333
        let weighted_avg = s.search_quality_weighted_sum / s.search_queries as f64;
        assert!((weighted_avg - 0.7333).abs() < 0.01, "got {}", weighted_avg);
        assert_eq!(s.full_index_count, 1);
        assert_eq!(s.full_index_ms_sum, 2000);
        assert_eq!(s.incr_count, 8);
        assert_eq!(s.files_indexed, 60);
    }

    // --- normalize_user_path ---
    // Indexed file_path columns are project-relative; users who paste absolute
    // paths from an IDE used to get silent "no results" across overview/deps/dead-code.

    #[test]
    fn test_normalize_user_path_dot_means_whole_project() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(normalize_user_path(tmp.path(), ".").unwrap(), "");
    }

    #[test]
    fn test_normalize_user_path_strips_dot_slash() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(normalize_user_path(tmp.path(), "./src/parser").unwrap(), "src/parser");
    }

    #[test]
    fn test_normalize_user_path_passes_relative_through() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(normalize_user_path(tmp.path(), "src/parser").unwrap(), "src/parser");
        assert_eq!(normalize_user_path(tmp.path(), "src/parser/").unwrap(), "src/parser/");
    }

    #[test]
    fn test_normalize_user_path_absolute_under_root_lexical() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let abs = root.join("src/parser");
        assert_eq!(normalize_user_path(root, abs.to_str().unwrap()).unwrap(), "src/parser");
    }

    #[test]
    fn test_db_sidecar_appends_suffix_to_full_filename() {
        // SQLite names the WAL `<dbfile>-wal` — a literal suffix, NOT an extension
        // swap. For `index.db` both happen to agree, but for the rebuild temp
        // `index.db.rebuild-<pid>` only the literal append is correct.
        let canonical = std::path::Path::new("/p/.code-graph/index.db");
        assert_eq!(db_sidecar(canonical, "-wal"),
            std::path::PathBuf::from("/p/.code-graph/index.db-wal"));
        assert_eq!(db_sidecar(canonical, "-shm"),
            std::path::PathBuf::from("/p/.code-graph/index.db-shm"));
        let temp = std::path::Path::new("/p/.code-graph/index.db.rebuild-1234");
        assert_eq!(db_sidecar(temp, "-wal"),
            std::path::PathBuf::from("/p/.code-graph/index.db.rebuild-1234-wal"),
            "WAL of a multi-dot temp db must append -wal, not swap the extension");
    }

    #[test]
    fn test_normalize_user_path_rejects_relative_dotdot_escape() {
        // A relative path climbing above the root must error, not pass through:
        // the index holds only in-root paths, so an escaping path can only match
        // the disk. `deps`' barrel-scan reads `project_root.join(raw)`, so this is
        // a path-traversal file read (leaks import/re-export lines), not just a
        // wrong query.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for escape in ["../secret.js", "../../etc/passwd", "a/../../b", ".."] {
            let err = normalize_user_path(root, escape).unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("escapes the project root"),
                "{escape:?} should be rejected as an escape; got: {msg}");
        }
        // Non-escaping `..` (stays at or below the root) is still allowed through.
        assert_eq!(normalize_user_path(root, "a/../b").unwrap(), "a/../b");
        assert_eq!(normalize_user_path(root, "src/sub/../mod.rs").unwrap(), "src/sub/../mod.rs");
    }

    #[test]
    fn test_normalize_user_path_absolute_outside_root_errors() {
        let tmp_root = tempfile::tempdir().unwrap();
        let tmp_other = tempfile::tempdir().unwrap();
        let abs_outside = tmp_other.path().join("foo.rs");
        let err = normalize_user_path(tmp_root.path(), abs_outside.to_str().unwrap()).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("outside the project root"), "got: {}", msg);
    }

    #[test]
    fn test_normalize_user_path_absolute_under_root_canonicalize_via_symlink() {
        // Symlink case: lexical strip fails but canonicalize succeeds.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/parser")).unwrap();
        let link_root = tmp.path().parent().unwrap().join(format!(
            "cg-norm-link-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&link_root);
        #[cfg(unix)]
        std::os::unix::fs::symlink(root, &link_root).unwrap();
        #[cfg(unix)]
        {
            let abs_via_link = link_root.join("src/parser");
            let res = normalize_user_path(root, abs_via_link.to_str().unwrap()).unwrap();
            assert_eq!(res, "src/parser");
            let _ = std::fs::remove_file(&link_root);
        }
    }

    #[test]
    fn test_normalize_grep_argv_attached_context() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // Attached numeric context forms split into flag + value.
        assert_eq!(normalize_grep_argv(s(&["grep", "-A2", "pat"])), s(&["grep", "-A", "2", "pat"]));
        assert_eq!(normalize_grep_argv(s(&["grep", "-B1", "pat"])), s(&["grep", "-B", "1", "pat"]));
        assert_eq!(normalize_grep_argv(s(&["grep", "-C10", "pat"])), s(&["grep", "-C", "10", "pat"]));
        // Bundled boolean short(s) + trailing attached context: peel the digits,
        // keep the cluster so clap parses `-nA 2` as `-n -A=2`.
        assert_eq!(normalize_grep_argv(s(&["grep", "-nA2", "pat"])), s(&["grep", "-nA", "2", "pat"]));
        assert_eq!(normalize_grep_argv(s(&["grep", "-niB3", "pat"])), s(&["grep", "-niB", "3", "pat"]));
        // Value flag not last in the bundle (`-A2B3`) → digit in the middle → left alone.
        assert_eq!(normalize_grep_argv(s(&["grep", "-A2B3"])), s(&["grep", "-A2B3"]));
        // Bare `-A` (clap takes the next token as its value) is untouched.
        assert_eq!(normalize_grep_argv(s(&["grep", "-A", "2", "pat"])), s(&["grep", "-A", "2", "pat"]));
        // Non-context single-dash flags and `--long` patterns are untouched.
        assert_eq!(normalize_grep_argv(s(&["grep", "-n", "pat"])), s(&["grep", "-n", "pat"]));
        assert_eq!(normalize_grep_argv(s(&["grep", "--no-default-features"])),
                   s(&["grep", "--no-default-features"]));
        // `-m` is the `--max-count` short alias: attached `-m2` splits like `-A2`
        // (the same allow_hyphen_values quirk forces the peel — see the fn doc).
        assert_eq!(normalize_grep_argv(s(&["grep", "-m2", "pat"])), s(&["grep", "-m", "2", "pat"]));
        assert_eq!(normalize_grep_argv(s(&["grep", "-nm2", "pat"])), s(&["grep", "-nm", "2", "pat"]));
        // Digit-suffix on an unsupported short (`-z2`) is left alone.
        assert_eq!(normalize_grep_argv(s(&["grep", "-z2", "pat"])), s(&["grep", "-z2", "pat"]));
        // Non-digit tail (`-A2x`) is not a valid attached form → left alone.
        assert_eq!(normalize_grep_argv(s(&["grep", "-A2x"])), s(&["grep", "-A2x"]));
        // `--` stops normalization so a literal `-A2` pattern survives.
        assert_eq!(normalize_grep_argv(s(&["grep", "--", "-A2"])), s(&["grep", "--", "-A2"]));
    }

    #[test]
    fn test_first_unsupported_grep_flag() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // Common grep flags we don't implement are flagged (would otherwise be
        // swallowed as the pattern → cryptic "No such file").
        for bad in ["-v", "-c", "-o", "-e", "-P", "-x", "-nv"] {
            assert_eq!(
                first_unsupported_grep_flag(&s(&["grep", bad, "pat"])).as_deref(),
                Some(bad),
                "{bad} should be reported as unsupported"
            );
        }
        // Supported shorts (incl. bundles + attached/standalone value shorts) pass.
        for ok in ["-i", "-w", "-F", "-l", "-n", "-r", "-R", "-H", "-A2", "-nA2",
                   "-niB3", "-C", "-m", "-m5", "-iw"] {
            assert_eq!(
                first_unsupported_grep_flag(&s(&["grep", ok, "pat"])),
                None,
                "{ok} is supported, must not be flagged"
            );
        }
        // Dash-then-symbol/digit terms are searchable patterns, not flags.
        for pat in ["->", "-1", "-.*", "-->foo"] {
            assert_eq!(
                first_unsupported_grep_flag(&s(&["grep", pat])),
                None,
                "{pat} is a pattern, must not be flagged"
            );
        }
        // `--` escapes a literal flag-shaped term.
        assert_eq!(first_unsupported_grep_flag(&s(&["grep", "--", "-v"])), None);
        // Unsupported flag after a supported value short's value is still caught.
        assert_eq!(
            first_unsupported_grep_flag(&s(&["grep", "-A", "2", "-v", "pat"])).as_deref(),
            Some("-v")
        );
    }
}
