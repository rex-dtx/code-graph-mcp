//! Shared graph snapshot — producer/consumer for `code-graph-snapshot-*.db.zst`
//! GitHub Release artifacts. See docs/superpowers/specs/2026-05-10-shared-graph-snapshot-design.md.

pub mod config;
pub mod install;
pub mod meta;

#[cfg(test)]
mod tests;

pub use install::{resolve_snapshot_source, try_install};
pub use meta::SnapshotMeta;

use anyhow::{Context, Result};
use std::path::Path;

/// Build a snapshot at `out` by running a full index in a temp dir, dropping
/// the vec table, vacuuming, writing meta keys, then VACUUM INTO the output
/// path.
///
/// When `out` ends in `.db.zst`, the result is zstd-compressed (level 9, to
/// match the producer workflow template). For any other extension the raw
/// SQLite file is written, and the caller is responsible for compression.
pub fn create(root: &Path, out: &Path, include_vec: bool) -> Result<()> {
    use crate::indexer::pipeline::run_full_index;
    use crate::storage::db::Database;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Index into a staging DB in a temp dir so we don't clobber the
    // project's own .code-graph/index.db.
    let tmp = tempfile::tempdir().context("create tempdir for snapshot build")?;
    let staging_db = tmp.path().join("staging.db");

    {
        let db = if include_vec {
            Database::open_with_vec(&staging_db)?
        } else {
            Database::open(&staging_db)?
        };

        run_full_index(&db, root, None, None)?;

        let conn = db.conn();

        // Drop vec table when caller doesn't want it (defensive IF EXISTS).
        if !include_vec {
            conn.execute_batch("DROP TABLE IF EXISTS node_vectors;")?;
        }
        // Never ship the content-hash embedding_cache: it is a LOCAL reuse cache (rebuilt on
        // demand and tied to this machine's model fingerprint), so it only bloats the artifact
        // (~1x the vectors) and a recipient with a different model would drop it on open anyway.
        conn.execute_batch("DROP TABLE IF EXISTS embedding_cache;")?;

        // Best-effort git commit hash; empty string if not a git repo.
        // Silence stderr so non-repo callers (snapshot CLI on a non-git dir,
        // unit tests creating fixtures without git init) don't see git's
        // `fatal: not a git repository` line leak through.
        let source_commit = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        meta::write_meta(conn, meta::META_SNAPSHOT_SOURCE_COMMIT, &source_commit)?;
        meta::write_meta(conn, meta::META_SNAPSHOT_CREATED_AT, &now.to_string())?;
        meta::write_meta(
            conn,
            meta::META_SNAPSHOT_TOOL_VERSION,
            env!("CARGO_PKG_VERSION"),
        )?;
        meta::write_meta(
            conn,
            meta::META_SNAPSHOT_SCHEMA_VERSION,
            &crate::storage::schema::SCHEMA_VERSION.to_string(),
        )?;
        meta::write_meta(
            conn,
            meta::META_SNAPSHOT_INCLUDES_VEC,
            if include_vec { "true" } else { "false" },
        )?;

        // Merge WAL back into main file so VACUUM INTO produces a single
        // self-contained file with no -wal/-shm sidecars.
        conn.execute_batch("PRAGMA journal_mode = DELETE;")?;
    } // `db` (and its Connection) dropped here — file fully flushed.

    // VACUUM INTO writes a compacted copy; destination must not exist.
    let compress_output = out.to_string_lossy().ends_with(".db.zst");
    let vacuum_target: std::path::PathBuf = if compress_output {
        tmp.path().join("compacted.db")
    } else {
        out.to_path_buf()
    };

    let conn = rusqlite::Connection::open(&staging_db)?;
    // Uniformity: pin mmap=0 on every connection-open path (matches db.rs open()/
    // open_readonly). Not a live hazard here — a short-lived, single-process producer
    // connection, not a long-lived reader during a concurrent VACUUM — but keeps a
    // future nonzero SQLITE_DEFAULT_MMAP_SIZE from silently re-enabling mmap anywhere.
    conn.execute_batch("PRAGMA mmap_size = 0;")?;
    let target_str = vacuum_target.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{target_str}';"))
        .with_context(|| format!("VACUUM INTO '{}'", vacuum_target.display()))?;

    if compress_output {
        let raw = std::fs::read(&vacuum_target).context("read compacted db")?;
        let compressed = zstd::encode_all(&raw[..], 9).context("zstd encode snapshot")?;
        std::fs::write(out, &compressed).context("write compressed snapshot")?;
        // Integrity sidecar: blake3 of the compressed artifact, hex-encoded.
        // Consumers fetch `<asset>.blake3` and verify it before decompressing.
        // Mirrors the blake3 artifact-integrity convention used for the embedding
        // model download (src/embedding/model.rs). Upload BOTH the `.db.zst` and
        // this `.db.zst.blake3` to the GitHub release.
        let digest = blake3::hash(&compressed).to_hex();
        let sidecar = format!("{}.blake3", out.display());
        std::fs::write(&sidecar, digest.as_bytes())
            .with_context(|| format!("write checksum sidecar {sidecar}"))?;
    }

    Ok(())
}

/// Open a snapshot file and read its meta. Accepts both zstd-compressed
/// (`.db.zst`, what producers/consumers exchange) and raw SQLite (`.db`,
/// the direct output of [`create`] before zstd compression). The format is
/// detected from the file's magic bytes, not the extension.
pub fn inspect(file: &Path) -> Result<SnapshotMeta> {
    use crate::storage::db::Database;

    // First-call site for the user's path. Without context the user-facing
    // error is just "No such file or directory (os error 2)" with no path —
    // typo at the CLI gives a useless error.
    let file_size_bytes = std::fs::metadata(file)
        .with_context(|| format!("stat snapshot file '{}'", file.display()))?
        .len();

    let raw_bytes =
        std::fs::read(file).with_context(|| format!("read snapshot file '{}'", file.display()))?;

    // zstd magic = 0x28 0xB5 0x2F 0xFD; SQLite = "SQLite format 3\0".
    let db_bytes = if raw_bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        zstd::decode_all(&raw_bytes[..]).context("zstd decode")?
    } else if raw_bytes.starts_with(b"SQLite format 3\0") {
        raw_bytes
    } else {
        anyhow::bail!(
            "{} is not a code-graph snapshot — expected zstd-compressed (.db.zst) or raw SQLite (.db)",
            file.display()
        );
    };

    let tmp = tempfile::tempdir().context("inspect tempdir")?;
    let decompressed = tmp.path().join("snapshot.db");
    std::fs::write(&decompressed, &db_bytes).context("write snapshot for inspect")?;

    let db = Database::open(&decompressed)?;
    let conn = db.conn();

    let source_commit =
        meta::read_meta(conn, meta::META_SNAPSHOT_SOURCE_COMMIT)?.unwrap_or_default();
    let source_url = meta::read_meta(conn, meta::META_SNAPSHOT_SOURCE_URL)?;
    let created_at = meta::read_meta(conn, meta::META_SNAPSHOT_CREATED_AT)?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let tool_version = meta::read_meta(conn, meta::META_SNAPSHOT_TOOL_VERSION)?.unwrap_or_default();
    let schema_version = meta::read_meta(conn, meta::META_SNAPSHOT_SCHEMA_VERSION)?
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);
    // Magic check above only validates the SQLite header (first 16 bytes). A
    // truncated db file passes the header check, then Database::open creates
    // empty schema and every meta lookup returns None → defaults. Without
    // this guard, `inspect` would return a fake "valid empty snapshot" with
    // zeroed fields. Real snapshots always carry a non-zero schema_version.
    if schema_version == 0 && source_commit.is_empty() && tool_version.is_empty() {
        anyhow::bail!(
            "{} is not a valid code-graph snapshot — meta is missing or unreadable (file may be truncated or corrupt)",
            file.display()
        );
    }
    let includes_vec = meta::read_meta(conn, meta::META_SNAPSHOT_INCLUDES_VEC)?
        .map(|s| s == "true")
        .unwrap_or(false);
    let fetched_at =
        meta::read_meta(conn, meta::META_SNAPSHOT_FETCHED_AT)?.and_then(|s| s.parse::<i64>().ok());

    let node_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap_or(0);
    let edge_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
        .unwrap_or(0);

    Ok(SnapshotMeta {
        source_commit,
        source_url,
        created_at,
        tool_version,
        schema_version,
        includes_vec,
        fetched_at,
        node_count,
        edge_count,
        file_size_bytes,
    })
}
