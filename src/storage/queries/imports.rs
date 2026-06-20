use anyhow::Result;
use rusqlite::Connection;

#[derive(Debug)]
pub struct FileDependency {
    pub file_path: String,
    pub direction: String, // "outgoing" (this file imports) or "incoming" (imports this file)
    pub symbol_count: i64,
    pub depth: i32,
}

/// Get file-level import/export dependencies with recursive depth traversal.
/// direction: "outgoing" (what this file depends on), "incoming" (what depends on this file), "both"
pub fn get_import_tree(
    conn: &Connection,
    file_path: &str,
    direction: &str,
    max_depth: i32,
) -> Result<Vec<FileDependency>> {
    use crate::domain::{REL_CALLS, REL_IMPORTS};
    if !matches!(direction, "outgoing" | "incoming" | "both") {
        anyhow::bail!("invalid direction '{}': expected outgoing, incoming, or both", direction);
    }
    let max_depth = max_depth.clamp(1, 10);
    let mut results = Vec::new();

    if direction == "outgoing" || direction == "both" {
        let mut stmt = conn.prepare(
            "WITH RECURSIVE dep_tree(file_id, file_path, depth, visited_ids) AS (
                -- Seed: the starting file (use file ID for cycle detection to avoid LIKE metacharacter issues)
                SELECT f0.id, f0.path, 0, CAST(f0.id AS TEXT)
                FROM files f0 WHERE f0.path = ?2

                UNION ALL

                -- Recurse: find files that the current-depth files depend on
                SELECT DISTINCT f2.id, f2.path, dt.depth + 1,
                       dt.visited_ids || '|' || CAST(f2.id AS TEXT)
                FROM dep_tree dt
                JOIN nodes n1 ON n1.file_id = dt.file_id
                JOIN edges e ON e.source_id = n1.id AND e.relation IN (?1, ?3)
                JOIN nodes n2 ON n2.id = e.target_id
                JOIN files f2 ON f2.id = n2.file_id
                WHERE dt.depth < ?4
                  AND f2.path != ?2
                  AND ('|' || dt.visited_ids || '|') NOT LIKE '%|' || CAST(f2.id AS TEXT) || '|%'
            )
            SELECT dt.file_path, MIN(dt.depth) as min_depth,
                -- Count distinct cross-file target symbols from root to this file
                -- (a symbol both imported and called is one symbol, not two).
                (SELECT COUNT(DISTINCT nb.id)
                 FROM nodes na JOIN files fa ON fa.id = na.file_id
                 JOIN edges ea ON ea.source_id = na.id AND ea.relation IN (?1, ?3)
                 JOIN nodes nb ON nb.id = ea.target_id
                 JOIN files fb ON fb.id = nb.file_id
                 WHERE fa.path = ?2 AND fb.path = dt.file_path) as cnt
            FROM dep_tree dt
            WHERE dt.depth > 0
            GROUP BY dt.file_path
            ORDER BY min_depth, cnt DESC"
        )?;
        let rows = stmt.query_map(
            rusqlite::params![REL_IMPORTS, file_path, REL_CALLS, max_depth],
            |row| {
                Ok(FileDependency {
                    file_path: row.get(0)?,
                    direction: "outgoing".into(),
                    symbol_count: row.get(2)?,
                    depth: row.get(1)?,
                })
            },
        )?;
        for row in rows {
            results.push(row?);
        }
    }

    if direction == "incoming" || direction == "both" {
        let mut stmt = conn.prepare(
            "WITH RECURSIVE dep_tree(file_id, file_path, depth, visited_ids) AS (
                SELECT f0.id, f0.path, 0, CAST(f0.id AS TEXT)
                FROM files f0 WHERE f0.path = ?2

                UNION ALL

                SELECT DISTINCT f1.id, f1.path, dt.depth + 1,
                       dt.visited_ids || '|' || CAST(f1.id AS TEXT)
                FROM dep_tree dt
                JOIN nodes n2 ON n2.file_id = dt.file_id
                JOIN edges e ON e.target_id = n2.id AND e.relation IN (?1, ?3)
                JOIN nodes n1 ON n1.id = e.source_id
                JOIN files f1 ON f1.id = n1.file_id
                WHERE dt.depth < ?4
                  AND f1.path != ?2
                  AND ('|' || dt.visited_ids || '|') NOT LIKE '%|' || CAST(f1.id AS TEXT) || '|%'
            )
            SELECT dt.file_path, MIN(dt.depth) as min_depth,
                -- Count distinct cross-file target symbols from this file to root
                -- (a symbol both imported and called is one symbol, not two).
                (SELECT COUNT(DISTINCT nb.id)
                 FROM nodes na JOIN files fa ON fa.id = na.file_id
                 JOIN edges ea ON ea.source_id = na.id AND ea.relation IN (?1, ?3)
                 JOIN nodes nb ON nb.id = ea.target_id
                 JOIN files fb ON fb.id = nb.file_id
                 WHERE fa.path = dt.file_path AND fb.path = ?2) as cnt
            FROM dep_tree dt
            WHERE dt.depth > 0
            GROUP BY dt.file_path
            ORDER BY min_depth, cnt DESC"
        )?;
        let rows = stmt.query_map(
            rusqlite::params![REL_IMPORTS, file_path, REL_CALLS, max_depth],
            |row| {
                Ok(FileDependency {
                    file_path: row.get(0)?,
                    direction: "incoming".into(),
                    symbol_count: row.get(2)?,
                    depth: row.get(1)?,
                })
            },
        )?;
        for row in rows {
            results.push(row?);
        }
    }

    Ok(results)
}

/// True when `file_path` has a row in the `files` table (i.e. it is indexed).
/// Lets `affected` distinguish "no dependents" from "never indexed".
pub fn file_is_indexed(conn: &Connection, file_path: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE path = ?1",
        rusqlite::params![file_path],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

/// Reverse transitive dependents of `file_path` over EVERY "A depends on B" relation
/// (imports ∪ calls ∪ references ∪ implements ∪ inherits), file-level, cycle-guarded.
/// Returns (dependent_file_path, min_depth). Unlike [`get_import_tree`] (imports ∪ calls
/// only — correct for a *dependency graph* view), `affected` needs the full relation
/// set so a test that only `references`/`implements`/`inherits` a changed symbol is not
/// silently dropped from the "tests to re-run" set. No `symbol_count` subquery — callers
/// here only need the file set and depth.
pub fn get_reverse_dependents(
    conn: &Connection,
    file_path: &str,
    max_depth: i32,
) -> Result<Vec<(String, i32)>> {
    use crate::domain::{REL_CALLS, REL_IMPLEMENTS, REL_IMPORTS, REL_INHERITS, REL_REFERENCES};
    let max_depth = max_depth.clamp(1, 10);
    // Relation IN-list built from trusted constants (no user input → no injection).
    let in_list = [REL_IMPORTS, REL_CALLS, REL_REFERENCES, REL_IMPLEMENTS, REL_INHERITS]
        .iter()
        .map(|r| format!("'{r}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "WITH RECURSIVE dep_tree(file_id, file_path, depth, visited_ids) AS (
            SELECT f0.id, f0.path, 0, CAST(f0.id AS TEXT)
            FROM files f0 WHERE f0.path = ?1

            UNION ALL

            SELECT DISTINCT f1.id, f1.path, dt.depth + 1,
                   dt.visited_ids || '|' || CAST(f1.id AS TEXT)
            FROM dep_tree dt
            JOIN nodes n2 ON n2.file_id = dt.file_id
            JOIN edges e ON e.target_id = n2.id AND e.relation IN ({in_list})
            JOIN nodes n1 ON n1.id = e.source_id
            JOIN files f1 ON f1.id = n1.file_id
            WHERE dt.depth < ?2
              AND f1.path != ?1
              AND ('|' || dt.visited_ids || '|') NOT LIKE '%|' || CAST(f1.id AS TEXT) || '|%'
        )
        SELECT dt.file_path, MIN(dt.depth) AS min_depth
        FROM dep_tree dt
        WHERE dt.depth > 0
        GROUP BY dt.file_path
        ORDER BY min_depth, dt.file_path"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![file_path, max_depth], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// All distinct cross-file `imports` edges as `(source_file, target_file)` pairs
/// (source imports target), for whole-graph circular-dependency detection.
///
/// Excludes self-file edges and the synthetic `<external>` pseudo-file (unresolved
/// external/builtin imports, mirroring `project_map`). `calls` is intentionally
/// excluded: a call cycle is mutual recursion, not a circular *import*.
pub fn all_file_import_edges(conn: &Connection) -> Result<Vec<(String, String)>> {
    use crate::domain::REL_IMPORTS;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT sf.path, tf.path \
         FROM edges e \
         JOIN nodes ns ON ns.id = e.source_id \
         JOIN files sf ON sf.id = ns.file_id \
         JOIN nodes nt ON nt.id = e.target_id \
         JOIN files tf ON tf.id = nt.file_id \
         WHERE e.relation = ?1 \
           AND sf.id != tf.id \
           AND sf.path != '<external>' AND tf.path != '<external>'",
    )?;
    let rows = stmt.query_map(rusqlite::params![REL_IMPORTS], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::helpers::test_db;

    #[test]
    fn test_get_import_tree() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        // File A with two functions, File B with two functions
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('src/a.ts', 'h1', 0, 'typescript', 0)", []).unwrap();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('src/b.ts', 'h2', 0, 'typescript', 0)", []).unwrap();
        // Nodes in file A
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'function', 'funcA1', 'funcA1', 1, 10, 'fn funcA1()')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'function', 'funcA2', 'funcA2', 11, 20, 'fn funcA2()')", []).unwrap();
        // Nodes in file B
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (2, 'function', 'funcB1', 'funcB1', 1, 10, 'fn funcB1()')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (2, 'function', 'funcB2', 'funcB2', 11, 20, 'fn funcB2()')", []).unwrap();
        // funcA1 imports funcB1, funcA2 calls funcB2 — 2 cross-file edges
        conn.execute("INSERT INTO edges (source_id, target_id, relation) VALUES (1, 3, 'imports')", []).unwrap();
        conn.execute("INSERT INTO edges (source_id, target_id, relation) VALUES (2, 4, 'calls')", []).unwrap();

        let tree = get_import_tree(conn, "src/a.ts", "outgoing", 2).unwrap();
        assert!(!tree.is_empty());
        let b_dep = tree.iter().find(|d| d.file_path == "src/b.ts").unwrap();
        assert_eq!(b_dep.symbol_count, 2, "symbol_count should reflect actual cross-file edges");
        assert_eq!(b_dep.depth, 1);

        // Incoming: from B's perspective, A depends on it with 2 symbols
        let tree_in = get_import_tree(conn, "src/b.ts", "incoming", 2).unwrap();
        let a_dep = tree_in.iter().find(|d| d.file_path == "src/a.ts").unwrap();
        assert_eq!(a_dep.symbol_count, 2, "incoming symbol_count should match");
    }

    #[test]
    fn file_is_indexed_detects_presence() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute(
            "INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('src/a.rs', 'h', 0, 'rust', 0)",
            [],
        ).unwrap();
        assert!(file_is_indexed(conn, "src/a.rs").unwrap());
        assert!(!file_is_indexed(conn, "src/missing.rs").unwrap());
    }

    #[test]
    fn reverse_dependents_includes_non_import_relations() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('a.ts','h1',0,'typescript',0)", []).unwrap();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('b.ts','h2',0,'typescript',0)", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id,type,name,start_line,end_line,code_content) VALUES (1,'function','a',1,2,'')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id,type,name,start_line,end_line,code_content) VALUES (2,'function','b',1,2,'')", []).unwrap();
        // a (file 1) references b (file 2) via a 'references' edge — NOT imports/calls.
        conn.execute("INSERT INTO edges (source_id,target_id,relation) VALUES (1,2,'references')", []).unwrap();

        // get_import_tree walks only imports∪calls → must MISS the references-only dep.
        let imp = get_import_tree(conn, "b.ts", "incoming", 5).unwrap();
        assert!(imp.iter().all(|d| d.file_path != "a.ts"),
            "import_tree (imports∪calls) should not see a references-only dependent");
        // get_reverse_dependents walks all dependency relations → must INCLUDE a.ts.
        let rev = get_reverse_dependents(conn, "b.ts", 5).unwrap();
        assert!(rev.iter().any(|(p, _)| p == "a.ts"),
            "reverse_dependents must include the references dependent; got {rev:?}");
    }

    #[test]
    fn all_file_import_edges_returns_cross_file_imports_only() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('a.ts','h1',0,'typescript',0)", []).unwrap();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('b.ts','h2',0,'typescript',0)", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id,type,name,start_line,end_line,code_content) VALUES (1,'function','fa',1,2,'')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id,type,name,start_line,end_line,code_content) VALUES (2,'function','fb',1,2,'')", []).unwrap();
        // a imports b AND b imports a → a file-level cycle.
        conn.execute("INSERT INTO edges (source_id,target_id,relation) VALUES (1,2,'imports')", []).unwrap();
        conn.execute("INSERT INTO edges (source_id,target_id,relation) VALUES (2,1,'imports')", []).unwrap();
        // A 'calls' edge must NOT appear — cycles are imports-only (call cycles = recursion).
        conn.execute("INSERT INTO edges (source_id,target_id,relation) VALUES (1,2,'calls')", []).unwrap();

        let mut edges = all_file_import_edges(conn).unwrap();
        edges.sort();
        assert_eq!(
            edges,
            vec![
                ("a.ts".to_string(), "b.ts".to_string()),
                ("b.ts".to_string(), "a.ts".to_string()),
            ],
            "exactly the two cross-file import edges; the calls edge is excluded"
        );
    }

    #[test]
    fn all_file_import_edges_excludes_self_file_and_external() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('a.ts','h1',0,'typescript',0)", []).unwrap();
        // Synthetic <external> bucket for unresolved imports (mirrors project_map).
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('<external>','h2',0,'typescript',0)", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id,type,name,start_line,end_line,code_content) VALUES (1,'function','f1',1,2,'')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id,type,name,start_line,end_line,code_content) VALUES (1,'function','f2',3,4,'')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id,type,name,start_line,end_line,code_content) VALUES (2,'function','ext',1,2,'')", []).unwrap();
        // Intra-file import (same file) and an import of the <external> bucket — both excluded.
        conn.execute("INSERT INTO edges (source_id,target_id,relation) VALUES (1,2,'imports')", []).unwrap();
        conn.execute("INSERT INTO edges (source_id,target_id,relation) VALUES (1,3,'imports')", []).unwrap();

        let edges = all_file_import_edges(conn).unwrap();
        assert!(edges.is_empty(), "self-file and <external> imports must be excluded; got {edges:?}");
    }
}
