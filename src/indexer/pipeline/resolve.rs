//! Cross-file call resolution helpers shared by the main `index_files` walk
//! and the post-index `pending_unresolved_calls` sweep.
//!
//! - `refine_ambiguous_targets`: disambiguator — when a call's target name
//!   matches N same-language nodes across files, prefer non-test paths and
//!   the longest common path prefix with the caller.
//! - `resolve_pending_calls`: drains buffered same-language-but-callee-not-yet-
//!   indexed rows once the callee appears (post-incremental sweep).

use anyhow::Result;
use std::collections::HashMap;

use crate::storage::db::Database;
use crate::storage::queries::{
    delete_pending_unresolved_call, insert_edge_cached, list_pending_unresolved_calls,
};
use crate::domain::REL_CALLS;

/// Decoded form of `edges.metadata` for REL_CALLS rows. See
/// `docs/superpowers/specs/2026-05-11-bare-name-call-qualifier-design.md`
/// §"Wire protocol" for the JSON shapes this parses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CalleeMeta {
    Path(Vec<String>),
    SelfType(String),
    SelfRecv(String),
    Receiver(String),
    Chain,
}

/// Parse a `{"q":"...", "v":"..."}` JSON metadata blob. Returns None for
/// metadata produced by other relations (routes, python imports), absent
/// metadata, or unrecognized `q` values.
pub(super) fn parse_callee_metadata(s: Option<&str>) -> Option<CalleeMeta> {
    let raw = s?;
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let q = v.get("q")?.as_str()?;
    match q {
        "chain" => Some(CalleeMeta::Chain),
        "path" => {
            let payload = v.get("v")?.as_str()?;
            let segments: Vec<String> = payload.split("::").map(String::from).collect();
            if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
                None
            } else {
                Some(CalleeMeta::Path(segments))
            }
        }
        "self" => v.get("v")?.as_str().map(|t| CalleeMeta::SelfRecv(t.to_string())),
        "stype" => v.get("v")?.as_str().map(|t| CalleeMeta::SelfType(t.to_string())),
        "recv" => v.get("v")?.as_str().map(|r| CalleeMeta::Receiver(r.to_string())),
        _ => None,
    }
}

/// Disambiguate N same-language cross-file candidates for a single call/import
/// target. Returns a subset. A single-element result is the authoritative
/// winner; ties fall back to the full input so the caller does not
/// inadvertently drop legitimate edges.
///
/// Heuristic: (1) prefer non-test-file candidates when the caller is not
/// itself a test file; (2) among the preferred pool, keep only those tied
/// for the longest byte-common path prefix with the caller. Previous
/// versions dropped on ambiguity, which regressed dead-code detection for
/// bare-name Rust calls like `crate::domain::foo()` where scoped_identifier
/// extraction keeps only `foo` and two `foo` definitions under `src/` tie
/// on prefix — better to keep both edges than to report `foo` as dead.
pub(super) fn refine_ambiguous_targets(
    candidates: &[i64],
    caller_rel_path: &str,
    node_id_to_path: &HashMap<i64, String>,
) -> Vec<i64> {
    if candidates.len() <= 1 {
        return candidates.to_vec();
    }

    let is_test_path = |p: &str| {
        p.contains(".test.") || p.contains("_test.")
            || p.starts_with("tests/") || p.contains("/tests/")
            || p.starts_with("test/") || p.contains("/test/")
            || p.contains(".spec.")
    };
    let caller_is_test = is_test_path(caller_rel_path);

    // Pass 1: prefer non-test candidates when the caller is non-test code.
    let pool: Vec<i64> = if caller_is_test {
        candidates.to_vec()
    } else {
        let non_test: Vec<i64> = candidates.iter().copied()
            .filter(|id| {
                let p = node_id_to_path.get(id).map(String::as_str).unwrap_or("");
                !is_test_path(p)
            })
            .collect();
        if non_test.is_empty() { candidates.to_vec() } else { non_test }
    };

    if pool.len() == 1 { return pool; }

    // Pass 2: keep only candidates tied for the longest common path prefix
    // with the caller. Byte-wise prefix is a rough proxy for module locality
    // — e.g. `claude-plugin/scripts/session-init.js` shares 21 bytes with
    // `claude-plugin/scripts/lifecycle.js` but 0 bytes with `scripts/*`.
    let prefix_len = |p: &str| -> usize {
        caller_rel_path.bytes().zip(p.bytes())
            .take_while(|(a, b)| a == b)
            .count()
    };
    let max_prefix = pool.iter()
        .map(|id| prefix_len(node_id_to_path.get(id).map(String::as_str).unwrap_or("")))
        .max()
        .unwrap_or(0);
    let closest: Vec<i64> = pool.iter().copied()
        .filter(|id| prefix_len(node_id_to_path.get(id).map(String::as_str).unwrap_or("")) == max_prefix)
        .collect();

    if closest.len() == 1 { return closest; }

    // Still ambiguous — return the remaining pool rather than dropping. This
    // keeps dead-code precision high for edges we cannot confidently prune
    // (most notably Rust bare-name scoped calls) at the cost of leaving a
    // small amount of fan-out; the single-winner fast path above handles
    // the common case (unique non-test match, or unique closest path).
    if !closest.is_empty() { closest } else { pool }
}

/// Sweep `pending_unresolved_calls` against the current node state. Rows whose
/// `(target_name, source_language)` now match a real node become a `calls`
/// edge and the pending row is dropped; rows that still don't resolve stay
/// buffered for the next index pass.
///
/// Resolution priority mirrors Phase 2: same-language candidates only (no
/// cross-language promotion — memory `feedback_edge_resolution_same_language.md`
/// flags that as the canonical false-positive class), with
/// `refine_ambiguous_targets` applied when multiple candidates share the name.
///
/// Returns the number of edges inserted by this sweep.
pub(super) fn resolve_pending_calls(db: &Database) -> Result<usize> {
    let pending = list_pending_unresolved_calls(db.conn())?;
    if pending.is_empty() {
        return Ok(0);
    }

    // Build name → [(node_id, language)] map ONCE, then iterate pending rows
    // in memory. Narrowed by `n.name IN (SELECT DISTINCT target_name ...)` so
    // even a 1-row pending table doesn't trigger a full nodes-table scan on
    // every incremental pass — for a 100K-node project the unfiltered SELECT
    // was 100K rows × every index call, even with no work to do.
    let mut name_to_lang_targets: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    let mut node_id_to_path: HashMap<i64, String> = HashMap::new();
    {
        let mut stmt = db.conn().prepare(
            "SELECT n.id, n.name, COALESCE(f.language, ''), f.path
             FROM nodes n JOIN files f ON f.id = n.file_id
             WHERE f.language IS NOT NULL
               AND n.name IN (SELECT DISTINCT target_name FROM pending_unresolved_calls)"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, name, lang, path) = row?;
            if lang.is_empty() {
                continue;
            }
            name_to_lang_targets.entry(name).or_default().push((id, lang));
            node_id_to_path.insert(id, path);
        }
    }

    // Map source_id → source file path so refine_ambiguous_targets gets the
    // proximity hint it needs.
    let source_ids: Vec<i64> = pending.iter().map(|p| p.source_id).collect();
    let mut source_id_to_path: HashMap<i64, String> = HashMap::new();
    if !source_ids.is_empty() {
        let placeholders = std::iter::repeat_n("?", source_ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT n.id, f.path FROM nodes n JOIN files f ON f.id = n.file_id WHERE n.id IN ({})",
            placeholders
        );
        let mut stmt = db.conn().prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = source_ids.iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, path) = row?;
            source_id_to_path.insert(id, path);
        }
    }

    let mut edges_added = 0usize;
    let mut to_delete: Vec<i64> = Vec::new();

    for row in &pending {
        let candidates: Vec<i64> = name_to_lang_targets.get(&row.target_name)
            .map(|entries| entries.iter()
                .filter(|(_, lang)| *lang == row.source_language)
                .map(|(id, _)| *id)
                .filter(|id| *id != row.source_id) // self-call guard
                .collect())
            .unwrap_or_default();

        if candidates.is_empty() {
            continue; // still unresolvable — leave buffered
        }

        let refined = if candidates.len() > 1 {
            let source_path = source_id_to_path.get(&row.source_id).cloned().unwrap_or_default();
            refine_ambiguous_targets(&candidates, &source_path, &node_id_to_path)
        } else {
            candidates
        };

        for tgt_id in &refined {
            if insert_edge_cached(
                db.conn(),
                row.source_id,
                *tgt_id,
                REL_CALLS,
                row.metadata.as_deref(),
            )? {
                edges_added += 1;
            }
        }
        to_delete.push(row.id);
    }

    for id in to_delete {
        delete_pending_unresolved_call(db.conn(), id)?;
    }

    Ok(edges_added)
}

/// Filter a candidate set down to those matching the Path qualifier:
///   (1) file path contains "/seg1/seg2/" OR starts with "seg1/seg2/", OR
///   (2) qualified_name contains the segment chain joined by `.` as a
///       contiguous segment (anchored on `.` or boundary).
///
/// Storage uses `.` separator for qualified_name (treesitter.rs:582), NOT `::`.
/// Returns the filtered subset; empty result is a meaningful signal
/// (no project candidate matches → caller should drop the edge).
pub(super) fn path_filter_candidates(
    segments: &[String],
    candidates: &[i64],
    node_id_to_path: &std::collections::HashMap<i64, String>,
    db: &crate::storage::db::Database,
) -> anyhow::Result<Vec<i64>> {
    if candidates.is_empty() || segments.is_empty() {
        return Ok(candidates.to_vec());
    }
    let path_chain = segments.join("/");
    let qn_chain = segments.join(".");

    let placeholders: String = std::iter::repeat_n("?", candidates.len()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id, COALESCE(qualified_name, '') FROM nodes WHERE id IN ({})",
        placeholders
    );
    let mut stmt = db.conn().prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = candidates.iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut id_to_qn: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    for r in rows {
        let (id, qn) = r?;
        id_to_qn.insert(id, qn);
    }

    // For the LAST segment, also accept the path ending in `<seg>.rs` —
    // Rust commonly puts single-file mods at `src/<mod>.rs` (e.g. `src/domain.rs`
    // for `crate::domain::*`), which has no `/domain/` directory boundary the
    // directory-style check below would catch. Without this, every
    // `crate::domain::foo()` call drops on the floor and `domain::foo` looks dead.
    let last_seg = segments.last().cloned().unwrap_or_default();
    let single_file_suffix = if !last_seg.is_empty() {
        Some(format!("/{}.rs", last_seg))
    } else {
        None
    };

    let kept: Vec<i64> = candidates.iter().copied().filter(|id| {
        let path = node_id_to_path.get(id).map(String::as_str).unwrap_or("");
        let qn = id_to_qn.get(id).map(String::as_str).unwrap_or("");

        let path_match = path.contains(&format!("/{}/", path_chain))
            || path.starts_with(&format!("{}/", path_chain))
            || single_file_suffix.as_deref().is_some_and(|sfx| path.ends_with(sfx));

        let qn_match = qn == qn_chain
            || qn.starts_with(&format!("{}.", qn_chain))
            || qn.contains(&format!(".{}.", qn_chain))
            || qn.ends_with(&format!(".{}", qn_chain));

        path_match || qn_match
    }).collect();
    Ok(kept)
}

/// Filter candidates to those whose `qualified_name` belongs to `impl_type`
/// (i.e. is a method of the named type). Storage encodes this as `Type.method`
/// with `.` separator (treesitter.rs qualified_name assignment).
///
/// Not file-restricted — Rust allows `impl Type {}` blocks to span multiple
/// files (e.g. `impl Database` is split across 3+ files in this repo), so we
/// match by `qualified_name LIKE 'Type.%'` across all files.
pub(super) fn self_filter_candidates(
    impl_type: &str,
    candidates: &[i64],
    db: &crate::storage::db::Database,
) -> anyhow::Result<Vec<i64>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: String = std::iter::repeat_n("?", candidates.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id FROM nodes
         WHERE id IN ({})
           AND qualified_name LIKE ? || '.%'",
        placeholders
    );
    let mut stmt = db.conn().prepare(&sql)?;
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = candidates
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>)
        .collect();
    params.push(Box::new(impl_type.to_string()));
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| row.get::<_, i64>(0))?;
    let kept: Vec<i64> = rows.filter_map(|r| r.ok()).collect();
    Ok(kept)
}

/// Filter candidates to those whose `qualified_name` denotes a METHOD — i.e.
/// contains a `.` separator (`Type.method`), as opposed to a free function
/// whose `qualified_name` equals its bare name. A receiver call `obj.method()`
/// can only bind to a method, never a free function, so this is the gate the
/// receiver-resolution arm uses to exclude same-named free functions before
/// deciding whether a unique target exists.
///
/// Storage encodes methods as `Type.method` (treesitter.rs qualified_name
/// assignment) and free functions as just `name`.
pub(super) fn method_candidates(
    candidates: &[i64],
    db: &crate::storage::db::Database,
) -> anyhow::Result<Vec<i64>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: String = std::iter::repeat_n("?", candidates.len())
        .collect::<Vec<_>>()
        .join(",");
    // qualified_name LIKE '%.%' — any node whose qualified_name carries a
    // `Type.` prefix. NULL qualified_name (rare) is excluded by LIKE.
    let sql = format!(
        "SELECT id FROM nodes WHERE id IN ({}) AND qualified_name LIKE '%.%'",
        placeholders
    );
    let mut stmt = db.conn().prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = candidates
        .iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(params.as_slice(), |row| row.get::<_, i64>(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_metadata_bare_returns_none() {
        assert!(parse_callee_metadata(None).is_none());
    }

    #[test]
    fn parse_metadata_path() {
        let m = parse_callee_metadata(Some(r#"{"q":"path","v":"snapshot"}"#)).unwrap();
        assert!(matches!(m, CalleeMeta::Path(ref segs) if segs == &["snapshot"]));
    }

    #[test]
    fn parse_metadata_path_multi_segment() {
        let m = parse_callee_metadata(Some(r#"{"q":"path","v":"a::b::c"}"#)).unwrap();
        assert!(matches!(m, CalleeMeta::Path(ref segs) if segs == &["a", "b", "c"]));
    }

    #[test]
    fn parse_metadata_self_recv() {
        let m = parse_callee_metadata(Some(r#"{"q":"self","v":"Db"}"#)).unwrap();
        assert!(matches!(m, CalleeMeta::SelfRecv(ref t) if t == "Db"));
    }

    #[test]
    fn parse_metadata_self_type() {
        let m = parse_callee_metadata(Some(r#"{"q":"stype","v":"Db"}"#)).unwrap();
        assert!(matches!(m, CalleeMeta::SelfType(ref t) if t == "Db"));
    }

    #[test]
    fn parse_metadata_recv() {
        let m = parse_callee_metadata(Some(r#"{"q":"recv","v":"path"}"#)).unwrap();
        assert!(matches!(m, CalleeMeta::Receiver(ref r) if r == "path"));
    }

    #[test]
    fn parse_metadata_chain() {
        let m = parse_callee_metadata(Some(r#"{"q":"chain"}"#)).unwrap();
        assert!(matches!(m, CalleeMeta::Chain));
    }

    #[test]
    fn parse_metadata_routes_or_python_imports_returns_none() {
        // Other relations also use metadata; resolver should skip non-call shapes.
        assert!(parse_callee_metadata(Some(r#"{"method":"GET","path":"/api"}"#)).is_none());
        assert!(parse_callee_metadata(Some(r#"{"python_module":"foo","is_module_import":false}"#)).is_none());
    }
}
