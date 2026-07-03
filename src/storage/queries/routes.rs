use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;

use super::helpers::{make_placeholders, MAX_IN_PARAMS};

pub struct RouteMatch {
    pub node_id: i64,
    pub metadata: Option<String>,
    pub handler_name: String,
    pub handler_type: String,
    pub file_path: String,
    pub start_line: i64,
    pub end_line: i64,
}

pub fn find_routes_by_path(conn: &Connection, route_path: &str, relation: &str) -> Result<Vec<RouteMatch>> {
    // Use json_extract for precise path matching instead of LIKE substring.
    // Match if the route_path is a prefix of the stored path (handles both exact and prefix matches).
    let mut stmt = conn.prepare(
        "SELECT e.source_id, e.metadata, n.name, n.type, f.path, n.start_line, n.end_line
         FROM edges e
         JOIN nodes n ON n.id = e.source_id
         JOIN files f ON f.id = n.file_id
         WHERE e.relation = ?2
         AND e.metadata IS NOT NULL
         AND (json_extract(e.metadata, '$.path') = ?1
              OR json_extract(e.metadata, '$.path') LIKE ?3 ESCAPE '\\')"
    )?;

    // Support both exact match and prefix match with path boundary
    // (e.g., "/api/users" matches "/api/users/:id" but not "/api/userservices")
    let escaped = route_path.replace('%', "\\%").replace('_', "\\_");
    let prefix_pattern = format!("{}/%", escaped);
    let rows = stmt.query_map(rusqlite::params![route_path, relation, prefix_pattern], |row| {
        Ok(RouteMatch {
            node_id: row.get(0)?,
            metadata: row.get(1)?,
            handler_name: row.get(2)?,
            handler_type: row.get(3)?,
            file_path: row.get(4)?,
            start_line: row.get(5)?,
            end_line: row.get(6)?,
        })
    })?;
    let results = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(results)
}

// --- Caller + route info query ---

#[derive(Debug)]
pub struct CallerWithRouteInfo {
    pub node_id: i64,
    pub name: String,
    pub node_type: String,
    pub file_path: String,
    pub depth: i32,
    pub route_info: Option<String>, // JSON metadata from routes_to edge
    /// Authoritative AST-level test flag (`nodes.is_test`) carried from the call
    /// graph. Drives the prod/test partition in `classify_impact` so an inline
    /// unit test the `is_test_symbol` name/path heuristic misses is still excluded
    /// from the production blast radius.
    pub is_test: bool,
}

/// Get all callers of a symbol, annotating any that are HTTP route handlers.
///
/// `min_confidence_rank` gates which caller edges are followed (per
/// `domain::confidence_rank`: extracted=2, inferred=1, ambiguous=0). Impact
/// surfaces pass `inferred` to fold the ambiguous by-name fan-out out of the
/// risk assessment; pass `0` (ambiguous) to traverse every caller edge (the
/// historical behavior — used by callers that are not risk assessment, e.g.
/// `show`).
pub fn get_callers_with_route_info(
    conn: &Connection,
    symbol_name: &str,
    file_path: Option<&str>,
    max_depth: i32,
    min_confidence_rank: u8,
) -> Result<Vec<CallerWithRouteInfo>> {
    use crate::graph::query::get_call_graph_filtered;
    use crate::domain::REL_ROUTES_TO;

    let callers = get_call_graph_filtered(conn, symbol_name, "callers", max_depth, file_path, min_confidence_rank)?;

    if callers.nodes.is_empty() {
        return Ok(vec![]);
    }

    // Batch fetch route metadata for all callers (avoids N+1 queries)
    let mut route_map: HashMap<i64, String> = HashMap::new();
    let caller_ids: Vec<i64> = callers.nodes.iter().map(|c| c.node_id).collect();
    for chunk in caller_ids.chunks(MAX_IN_PARAMS) {
        let placeholders = make_placeholders(1, chunk.len());
        let sql = format!(
            "SELECT e.source_id, e.metadata FROM edges e WHERE e.source_id IN ({}) AND e.relation = ?{}",
            placeholders,
            chunk.len() + 1
        );
        let mut params: Vec<&dyn rusqlite::types::ToSql> = chunk.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
        let rel: &dyn rusqlite::types::ToSql = &REL_ROUTES_TO;
        params.push(rel);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (id, meta) = row?;
            if let Some(meta) = meta {
                route_map.entry(id).or_insert(meta);
            }
        }
    }

    let results = callers
        .nodes
        .iter()
        .map(|caller| CallerWithRouteInfo {
            node_id: caller.node_id,
            name: caller.name.clone(),
            node_type: caller.node_type.clone(),
            file_path: caller.file_path.clone(),
            depth: caller.depth,
            route_info: route_map.get(&caller.node_id).cloned(),
            is_test: caller.is_test,
        })
        .collect();
    Ok(results)
}

// --- Module queries ---

#[derive(Debug, Clone)]
pub struct ModuleExport {
    pub node_id: i64,
    pub name: String,
    pub node_type: String,
    pub signature: Option<String>,
    pub file_path: String,
    pub caller_count: i64,
    pub start_line: i64,
    pub end_line: i64,
}

/// Get all exported symbols from files under a directory prefix.
/// For JS/TS, uses explicit `exports` edges. For other languages (Rust, Go, Python, etc.),
/// falls back to returning all named top-level symbols (functions, structs, classes, etc.).
pub fn get_module_exports(conn: &Connection, dir_prefix: &str) -> Result<Vec<ModuleExport>> {
    use crate::domain::{REL_EXPORTS, REL_CALLS};
    let escaped_prefix = dir_prefix.replace('%', "\\%").replace('_', "\\_");
    let prefix_pattern = format!("{}%", escaped_prefix);

    // Per-file export semantics (decided per file, NOT globally over the prefix):
    //   - a file that declares explicit exports (ESM `export` → REL_EXPORTS edges)
    //     contributes ONLY its exported symbols (surface the public API);
    //   - a file with no export edges at all (Python / Rust / Go / CommonJS / …)
    //     contributes every named top-level symbol.
    // The former shape ran these as two GLOBAL phases and returned as soon as any
    // ESM export existed under the prefix — silently dropping every
    // non-export-language file in the same tree (`overview src` / MCP
    // `module_overview` hid all Python/Rust/Go source next to a `.ts` file).
    // The `?3 = REL_EXPORTS` set of export-bearing files is computed once
    // (uncorrelated NOT IN), so this stays a single scan.
    //
    // Filter n.is_test=0 — AST-level flag catches inline `#[cfg(test)] mod tests`
    // whose names don't match the name-heuristic in is_test_symbol.
    // Caller count subquery uses domain helpers to filter source-side test edges
    // so prod-only counts align with project_map / find_references / get_ast_node
    // impact (see feedback_test_classifier_dual_sources for the full inventory).
    let prod_join = crate::domain::prod_source_join_sql("e2");
    let prod_where = crate::domain::PROD_SOURCE_FILTER_AND;
    let sql = format!(
        "SELECT DISTINCT n.id, n.name, n.type, n.signature, f.path,
                COALESCE(cc.cnt, 0) as caller_count,
                n.start_line, n.end_line
         FROM nodes n
         JOIN files f ON f.id = n.file_id
         LEFT JOIN (
             SELECT e2.target_id, COUNT(*) as cnt
             FROM edges e2
             {prod_join}
             WHERE e2.relation = ?2
               AND {prod_where}
             GROUP BY e2.target_id
         ) cc ON cc.target_id = n.id
         WHERE f.path LIKE ?1 ESCAPE '\\'
           AND n.type != 'module'
           AND n.name != '<module>'
           AND n.is_test = 0
           AND f.path != '<external>'
           AND (
               EXISTS (
                   SELECT 1 FROM edges ex
                   WHERE ex.target_id = n.id AND ex.relation = ?3
               )
               OR n.file_id NOT IN (
                   SELECT en.file_id
                   FROM edges ex2
                   JOIN nodes en ON en.id = ex2.target_id
                   WHERE ex2.relation = ?3
               )
               -- Methods of an exported class are public API too, but ESM emits an
               -- export edge only for the class, never its methods — so in an
               -- export-bearing (TS/JS) file they were dropped from overview /
               -- module_overview, while Python/Rust/Go methods (files with no export
               -- edges → the NOT IN branch above) showed. Include n when an exported
               -- node in the SAME file owns it by qualified_name (`<owner>.<n.name>`,
               -- exact so no LIKE-escaping of `_`/`%`). In practice only class members
               -- get a dotted qualified_name (top-level symbols are bare), so this is
               -- methods of exported classes. Kept LAST and file-bounded (cls filtered
               -- by file_id + qualified_name, hitting idx_nodes_file, before the export
               -- check) so this correlated lookup fires only for the few non-exported
               -- rows in export-bearing files — never for no-export (Python/Rust/Go)
               -- files, which the cheap materialized NOT IN branch short-circuits first.
               OR EXISTS (
                   SELECT 1 FROM nodes cls
                   WHERE cls.file_id = n.file_id
                     AND n.qualified_name = cls.qualified_name || '.' || n.name
                     AND EXISTS (
                         SELECT 1 FROM edges ex4
                         WHERE ex4.target_id = cls.id AND ex4.relation = ?3
                     )
               )
           )
         ORDER BY caller_count DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![&prefix_pattern, REL_CALLS, REL_EXPORTS], |row| {
        Ok(ModuleExport {
            node_id: row.get(0)?,
            name: row.get(1)?,
            node_type: row.get(2)?,
            signature: row.get(3)?,
            file_path: row.get(4)?,
            caller_count: row.get(5)?,
            start_line: row.get(6)?,
            end_line: row.get(7)?,
        })
    })?;
    let all: Vec<ModuleExport> = rows.collect::<std::result::Result<Vec<_>, _>>()?;

    // Deduplicate by (name, file_path) — keeps highest caller_count.
    // Handles feature-gated duplicates (e.g. #[cfg(feature)] producing two nodes for same symbol).
    let mut best: HashMap<(String, String), ModuleExport> = HashMap::with_capacity(all.len());
    for export in all {
        let key = (export.name.clone(), export.file_path.clone());
        best.entry(key)
            .and_modify(|existing| {
                if export.caller_count > existing.caller_count {
                    *existing = export.clone();
                }
            })
            .or_insert(export);
    }
    // `HashMap::into_values()` iterates in a per-run-random order, which discarded
    // the SQL `ORDER BY caller_count DESC` and made `overview` / `module_overview`
    // print the same symbols in a different order on every run (same bug class as
    // the call-graph merge). Re-establish a deterministic TOTAL order: caller_count
    // DESC (the relevance the SQL intended), then (file_path, start_line, name).
    // `name` is the final key BECAUSE (file_path, start_line) alone is NOT unique —
    // e.g. cfg-gated or macro-generated symbols can share a start_line in one file;
    // without `name` those ties keep the random HashMap order. (The dedup key above
    // is (name, file_path), so name closes the order.) file/line/name are all
    // source-derived, so the order is stable across index rebuilds (unlike node_id).
    let mut result: Vec<ModuleExport> = best.into_values().collect();
    result.sort_by(|a, b| {
        b.caller_count
            .cmp(&a.caller_count)
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::helpers::test_db;

    #[test]
    fn test_callers_with_routes() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        // Insert test data: file -> handler node -> route edge, caller -> calls -> handler
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('test.ts', 'h1', 0, 'typescript', 0)", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'function', 'handler', 'handler', 1, 10, 'fn handler()')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'function', 'caller', 'caller', 11, 20, 'fn caller()')", []).unwrap();
        // caller (node 2) calls handler (node 1)
        conn.execute("INSERT INTO edges (source_id, target_id, relation, metadata) VALUES (2, 1, 'calls', NULL)", []).unwrap();
        // caller (node 2) is also a route handler
        conn.execute("INSERT INTO edges (source_id, target_id, relation, metadata) VALUES (2, 2, 'routes_to', '{\"method\":\"GET\",\"path\":\"/api/test\"}')", []).unwrap();

        let results = get_callers_with_route_info(db.conn(), "handler", None, 3, 0).unwrap();
        assert!(!results.is_empty());
        // Verify route info is attached to the caller that is a route handler
        assert!(results.iter().any(|r| r.route_info.is_some()));
    }

    #[test]
    fn test_get_module_exports() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('src/auth/validator.ts', 'h1', 0, 'typescript', 0)", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, signature) VALUES (1, 'function', 'validateUser', 'validateUser', 1, 10, 'function validateUser() {}', '(token: string) => User')", []).unwrap();
        // Add an export edge (module-level node exports this function)
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'module', 'validator', 'validator', 0, 0, '')", []).unwrap();
        conn.execute("INSERT INTO edges (source_id, target_id, relation) VALUES (2, 1, 'exports')", []).unwrap();

        let exports = get_module_exports(conn, "src/auth/").unwrap();
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "validateUser");
    }

    /// Regression: in an export-bearing (TS/JS) file, methods of an EXPORTED class
    /// must surface too — ESM emits an export edge only for the class, so the old
    /// "show only export-edge targets" rule dropped every method from overview /
    /// module_overview (while Python/Rust methods showed). A method of a
    /// NON-exported class must stay hidden (not public API).
    #[test]
    fn test_get_module_exports_includes_methods_of_exported_class() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('src/models.ts', 'h1', 0, 'typescript', 0)", []).unwrap();
        // Exported class Animal (id 1) + its method speak (id 2, qn 'Animal.speak').
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'class', 'Animal', 'Animal', 1, 5, 'class Animal {}')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'method', 'speak', 'Animal.speak', 2, 3, 'speak() {}')", []).unwrap();
        // Module node (id 3) exports the class (edge module -> Animal).
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'module', 'models', 'models', 0, 0, '')", []).unwrap();
        conn.execute("INSERT INTO edges (source_id, target_id, relation) VALUES (3, 1, 'exports')", []).unwrap();
        // Non-exported internal class Helper (id 4) + its method secret (id 5).
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'class', 'Helper', 'Helper', 6, 8, 'class Helper {}')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'method', 'secret', 'Helper.secret', 7, 7, 'secret() {}')", []).unwrap();

        let names: Vec<String> = get_module_exports(conn, "src/models")
            .unwrap().iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"Animal".to_string()), "exported class shown: {names:?}");
        assert!(names.contains(&"speak".to_string()), "method of exported class shown: {names:?}");
        assert!(!names.contains(&"Helper".to_string()), "non-exported class hidden: {names:?}");
        assert!(!names.contains(&"secret".to_string()), "method of non-exported class hidden: {names:?}");
    }

    /// Regression: `get_module_exports` must return a deterministic, relevance-
    /// ordered list. It dedups through a HashMap and previously returned
    /// `best.into_values().collect()`, whose per-run-random iteration order
    /// discarded the SQL `ORDER BY caller_count DESC` — so `overview` /
    /// `module_overview` printed the same symbols shuffled on every call. Order is
    /// now caller_count DESC, then (file_path, start_line). Here beta and gamma tie
    /// on caller_count (2) and must be broken by start_line (beta L20 < gamma L30).
    #[test]
    fn test_get_module_exports_deterministic_order() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        // Target file has NO export edges → every top-level symbol is contributed.
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('src/target.py', 'h1', 0, 'python', 0)", []).unwrap();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('src/callers.py', 'h2', 0, 'python', 0)", []).unwrap();
        for (name, line) in [("alpha", 10), ("beta", 20), ("gamma", 30), ("delta", 40), ("epsilon", 50)] {
            conn.execute(
                "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'function', ?1, ?1, ?2, ?2, '')",
                rusqlite::params![name, line],
            ).unwrap();
        }
        // Prod caller nodes in a second file (node ids 6..=13).
        for i in 0..8 {
            conn.execute(
                "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (2, 'function', ?1, ?1, ?2, ?2, '')",
                rusqlite::params![format!("c{i}"), 100 + i],
            ).unwrap();
        }
        // caller_count: alpha=3, beta=2, gamma=2, delta=1, epsilon=0 (targets 1..=5).
        for (src, tgt) in [(6, 1), (7, 1), (8, 1), (9, 2), (10, 2), (11, 3), (12, 3), (13, 4)] {
            conn.execute("INSERT INTO edges (source_id, target_id, relation) VALUES (?1, ?2, 'calls')", rusqlite::params![src, tgt]).unwrap();
        }

        let run1: Vec<String> = get_module_exports(conn, "src/target").unwrap().iter().map(|e| e.name.clone()).collect();
        let run2: Vec<String> = get_module_exports(conn, "src/target").unwrap().iter().map(|e| e.name.clone()).collect();
        assert_eq!(run1, run2, "two calls must return an identical order (was HashMap-shuffled)");
        assert_eq!(run1, vec!["alpha", "beta", "gamma", "delta", "epsilon"],
            "order must be caller_count DESC then (file, start_line): beta(2,L20) before gamma(2,L30)");
    }

    /// Regression (adversarial-review finding): the synthetic `<external>`
    /// pseudo-file holds unresolved import targets (`numpy`, `std::io::Write`, …),
    /// NOT project symbols. They all share caller_count=0 / file='<external>' /
    /// start_line=0, so under the sort's `(caller_count, file_path, start_line)`
    /// prefix they all tied and `overview .` (empty prefix → `LIKE '%'` matches
    /// `<external>`) shuffled them every run. `get_project_map` already excludes
    /// `<external>`; `get_module_exports` now does too — so they neither appear nor
    /// destabilize the whole-project view.
    #[test]
    fn test_get_module_exports_excludes_external_pseudo_file() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('src/app.py', 'h1', 0, 'python', 0)", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'function', 'run', 'run', 5, 9, '')", []).unwrap();
        // Synthetic <external> pseudo-file: several unresolved imports, all at
        // caller_count 0 / start_line 0 — the cluster that used to shuffle.
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('<external>', 'h2', 0, 'python', 0)", []).unwrap();
        for name in ["numpy", "collections", "os", "sys"] {
            conn.execute(
                "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (2, 'external_module', ?1, ?1, 0, 0, '')",
                rusqlite::params![name],
            ).unwrap();
        }
        // Whole-project view (empty prefix → LIKE '%' matches every file).
        let exports = get_module_exports(conn, "").unwrap();
        let names: Vec<&str> = exports.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"run"), "real project symbol must be present; got {names:?}");
        assert!(exports.iter().all(|e| e.file_path != "<external>"),
            "<external> pseudo-symbols must be excluded from overview; got {names:?}");
    }

    #[test]
    fn test_get_module_exports_filters_is_test_nodes() {
        // Rust fallback path: inline `#[cfg(test)] mod tests { #[test] fn foo }`
        // whose names don't prefix-match `test_` must still be excluded via the
        // AST-level n.is_test flag. See feedback_test_filter_propagation.md.
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute(
            "INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at)
             VALUES ('src/foo.rs', 'h1', 0, 'rust', 0)",
            [],
        ).unwrap();
        // Real export — name doesn't match is_test_symbol heuristic
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (1, 'function', 'compute_thing', 'compute_thing', 1, 5, 'fn compute_thing(){}', 0)",
            [],
        ).unwrap();
        // Inline test fn — name doesn't match heuristic either, but is_test=1
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (1, 'function', 'arrays_are_homogeneous', 'arrays_are_homogeneous', 10, 20, 'fn arrays_are_homogeneous(){}', 1)",
            [],
        ).unwrap();

        let exports = get_module_exports(conn, "src/foo.rs").unwrap();
        let names: Vec<&str> = exports.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"compute_thing"), "real export missing: {:?}", names);
        assert!(
            !names.contains(&"arrays_are_homogeneous"),
            "is_test=1 node leaked into module exports: {:?}", names,
        );
    }

    #[test]
    fn test_get_module_exports_mixed_language_no_drop() {
        // Regression: a directory mixing an ESM file (explicit `export` →
        // REL_EXPORTS edges) with a non-export-language file (Python/Rust/…, no
        // export edges) must show symbols from BOTH files. The former global
        // two-phase form returned as soon as Phase 1 (any ESM export under the
        // prefix) found something, silently dropping every non-export-language
        // file in the same tree — so `overview src` / MCP `module_overview` hid
        // all Python/Rust/Go source whenever a sibling .ts file had exports.
        let (db, _tmp) = test_db();
        let conn = db.conn();
        // ESM file with an explicit export
        conn.execute(
            "INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at)
             VALUES ('src/a.ts', 'h1', 0, 'typescript', 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (1, 'function', 'tsExported', 'tsExported', 1, 5, 'export function tsExported(){}', 0)",
            [],
        ).unwrap(); // node 1
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (1, 'module', '<module>', '<module>', 0, 0, '', 0)",
            [],
        ).unwrap(); // node 2 (module source of the export edge)
        conn.execute("INSERT INTO edges (source_id, target_id, relation) VALUES (2, 1, 'exports')", []).unwrap();
        // Python file with NO export edges (Python has no `export` concept)
        conn.execute(
            "INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at)
             VALUES ('src/b.py', 'h2', 0, 'python', 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (2, 'function', 'py_func', 'py_func', 1, 5, 'def py_func(): pass', 0)",
            [],
        ).unwrap(); // node 3

        let exports = get_module_exports(conn, "src/").unwrap();
        let names: Vec<&str> = exports.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tsExported"), "ESM export missing: {:?}", names);
        assert!(
            names.contains(&"py_func"),
            "non-export-language symbol dropped in mixed-language dir: {:?}", names,
        );
    }

    #[test]
    fn test_get_module_exports_hides_nonexported_in_esm_file() {
        // Intent preserved: within a file that DOES declare exports, a
        // non-exported internal symbol stays hidden (the overview surfaces the
        // public API of ESM files, not their internals). Guards against the
        // per-file fix over-widening ESM files to all top-level symbols.
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute(
            "INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at)
             VALUES ('src/a.ts', 'h1', 0, 'typescript', 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (1, 'function', 'publicApi', 'publicApi', 1, 5, 'export function publicApi(){}', 0)",
            [],
        ).unwrap(); // node 1 (exported)
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (1, 'function', 'internalHelper', 'internalHelper', 7, 9, 'function internalHelper(){}', 0)",
            [],
        ).unwrap(); // node 2 (NOT exported)
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (1, 'module', '<module>', '<module>', 0, 0, '', 0)",
            [],
        ).unwrap(); // node 3 (module)
        conn.execute("INSERT INTO edges (source_id, target_id, relation) VALUES (3, 1, 'exports')", []).unwrap();

        let exports = get_module_exports(conn, "src/a.ts").unwrap();
        let names: Vec<&str> = exports.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"publicApi"), "exported symbol missing: {:?}", names);
        assert!(
            !names.contains(&"internalHelper"),
            "non-exported internal symbol leaked from an ESM file: {:?}", names,
        );
    }

    #[test]
    fn test_get_module_exports_caller_count_excludes_test_sources() {
        // Counterpart to project_map's hot_functions test: caller_count must
        // count only production callers. Test/benches sources must not inflate
        // it. Three callers (1 prod + 2 test) — count must be 1.
        let (db, _tmp) = test_db();
        let conn = db.conn();
        // Production file with the target export
        conn.execute(
            "INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at)
             VALUES ('src/foo.rs', 'h1', 0, 'rust', 0)",
            [],
        ).unwrap();
        // Bench file
        conn.execute(
            "INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at)
             VALUES ('benches/bench_foo.rs', 'h2', 0, 'rust', 0)",
            [],
        ).unwrap();
        // Tests dir file
        conn.execute(
            "INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at)
             VALUES ('tests/integration.rs', 'h3', 0, 'rust', 0)",
            [],
        ).unwrap();
        // Target: production export
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (1, 'function', 'compute_thing', 'compute_thing', 1, 5, 'fn compute_thing(){}', 0)",
            [],
        ).unwrap(); // node 1 (target)
        // Prod caller (real production code)
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (1, 'function', 'prod_caller', 'prod_caller', 10, 20, 'fn prod_caller(){}', 0)",
            [],
        ).unwrap(); // node 2
        // Bench caller (path = benches/, name doesn't start with test_)
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (2, 'function', 'bench_compute', 'bench_compute', 1, 10, 'fn bench_compute(){}', 0)",
            [],
        ).unwrap(); // node 3
        // Integration test caller (path = tests/, but is_test=0 since path-based)
        conn.execute(
            "INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content, is_test)
             VALUES (3, 'function', 'test_compute_works', 'test_compute_works', 1, 10, 'fn test_compute_works(){}', 0)",
            [],
        ).unwrap(); // node 4
        // Edges: all three call the target (node 1)
        conn.execute("INSERT INTO edges (source_id, target_id, relation, metadata) VALUES (2, 1, 'calls', NULL)", []).unwrap();
        conn.execute("INSERT INTO edges (source_id, target_id, relation, metadata) VALUES (3, 1, 'calls', NULL)", []).unwrap();
        conn.execute("INSERT INTO edges (source_id, target_id, relation, metadata) VALUES (4, 1, 'calls', NULL)", []).unwrap();

        let exports = get_module_exports(conn, "src/foo.rs").unwrap();
        let target = exports.iter().find(|e| e.name == "compute_thing")
            .expect("compute_thing must be in exports");
        assert_eq!(
            target.caller_count, 1,
            "caller_count must exclude bench_/tests/ source edges; got {} (expected 1 prod-only)",
            target.caller_count,
        );
    }
}
