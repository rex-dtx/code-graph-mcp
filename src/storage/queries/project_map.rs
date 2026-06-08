use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;

use crate::domain::{REL_CALLS, REL_IMPORTS, REL_ROUTES_TO, REL_EXPORTS};

/// Per-module (directory) statistics for the project map.
pub struct ModuleStats {
    pub path: String,
    pub files: usize,
    pub functions: usize,
    pub classes: usize,
    pub interfaces_traits: usize,
    pub languages: Vec<String>,
    pub key_symbols: Vec<String>,
}

/// Cross-module dependency edge.
pub struct ModuleDep {
    pub from: String,
    pub to: String,
    pub import_count: usize,
}

/// HTTP entry point.
pub struct EntryPoint {
    pub route: String,
    pub handler: String,
    pub file: String,
    /// `"http_route"` for framework-registered handlers; `"main"` for program entry
    /// points (fn main). Lets consumers distinguish real routes from `route="main"`.
    pub kind: String,
}

/// Hot function (most callers).
pub struct HotFunction {
    pub name: String,
    pub node_type: String,
    pub file: String,
    pub caller_count: usize,
    pub test_caller_count: usize,
}

/// Get the directory part of a file path (everything before the last '/').
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "<root>",
    }
}

/// Build a project architecture map from the knowledge graph.
#[allow(clippy::type_complexity)]
pub fn get_project_map(conn: &Connection) -> Result<(Vec<ModuleStats>, Vec<ModuleDep>, Vec<EntryPoint>, Vec<HotFunction>)> {
    // 1. Module map: SQL-level aggregation (C3: use constants, I1: GROUP BY in SQL)
    // `method` counts toward the function bucket so the per-module symbol total
    // matches what `overview` and `key_symbols` actually list (methods are
    // symbols too — excluding them undercounts OO modules and contradicts the
    // listed key_symbols). The synthetic `<external>` pseudo-file (unresolved
    // import/trait targets like `Drop`, `std::io::Write`) is not a real module
    // and its nodes are not project symbols — exclude it.
    let sql = "SELECT f.path, \
                SUM(CASE WHEN n.type IN ('function', 'method') THEN 1 ELSE 0 END), \
                SUM(CASE WHEN n.type IN ('class', 'struct', 'enum') THEN 1 ELSE 0 END), \
                SUM(CASE WHEN n.type IN ('interface', 'trait') THEN 1 ELSE 0 END), \
                GROUP_CONCAT(DISTINCT f.language) \
         FROM nodes n JOIN files f ON f.id = n.file_id \
         WHERE n.type != 'module' AND n.name != '<module>' \
           AND n.is_test = 0 \
           AND f.path != '<external>' \
         GROUP BY f.path"
        .to_string();
    let mut dir_files: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut dir_funcs: HashMap<String, usize> = HashMap::new();
    let mut dir_classes: HashMap<String, usize> = HashMap::new();
    let mut dir_ifaces: HashMap<String, usize> = HashMap::new();
    let mut dir_langs: HashMap<String, std::collections::BTreeSet<String>> = HashMap::new();
    {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, i64>(2)? as usize,
                row.get::<_, i64>(3)? as usize,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (path, funcs, classes, ifaces, langs) = row?;
            let dir = dir_of(&path).to_string();
            dir_files.entry(dir.clone()).or_default().insert(path);
            *dir_funcs.entry(dir.clone()).or_default() += funcs;
            *dir_classes.entry(dir.clone()).or_default() += classes;
            *dir_ifaces.entry(dir.clone()).or_default() += ifaces;
            if let Some(l) = langs {
                for lang in l.split(',').filter(|s| !s.is_empty()) {
                    dir_langs.entry(dir.clone()).or_default().insert(lang.to_string());
                }
            }
        }
    }

    // 2. Key symbols per module (C2: language-agnostic — use most-called functions per module)
    let mut dir_symbols: HashMap<String, Vec<String>> = HashMap::new();
    {
        let sql = "SELECT n.name, f.path, COUNT(e.id) as cnt \
             FROM nodes n \
             JOIN files f ON f.id = n.file_id \
             JOIN edges e ON e.target_id = n.id \
             WHERE e.relation = ?1 AND n.type != 'module' AND n.name != '<module>' \
               AND n.is_test = 0 \
               AND f.path != '<external>' \
               AND n.name NOT LIKE 'test\\_%' ESCAPE '\\' \
               AND f.path NOT LIKE 'tests/%' \
               AND f.path NOT LIKE 'benches/%' \
               AND f.path NOT LIKE '%_test.%' \
             GROUP BY n.id \
             ORDER BY cnt DESC \
             LIMIT 200";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([REL_CALLS], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (name, path) = row?;
            let dir = dir_of(&path).to_string();
            let syms = dir_symbols.entry(dir).or_default();
            if syms.len() < 6 && !syms.contains(&name) {
                syms.push(name);
            }
        }
    }

    // Also add explicit exports (JS/TS) where available
    {
        let sql = "SELECT DISTINCT n.name, f.path FROM edges e \
             JOIN nodes n ON n.id = e.target_id \
             JOIN files f ON f.id = n.file_id \
             WHERE e.relation = ?1 AND n.name != '<module>' \
               AND f.path != '<external>'";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([REL_EXPORTS], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (name, path) = row?;
            let dir = dir_of(&path).to_string();
            let syms = dir_symbols.entry(dir).or_default();
            if syms.len() < 8 && !syms.contains(&name) {
                syms.push(name);
            }
        }
    }

    // Assemble module stats (sorted by function count descending)
    let mut modules: Vec<ModuleStats> = dir_files.keys().map(|dir| {
        ModuleStats {
            path: dir.clone(),
            files: dir_files.get(dir).map(|s| s.len()).unwrap_or(0),
            functions: *dir_funcs.get(dir).unwrap_or(&0),
            classes: *dir_classes.get(dir).unwrap_or(&0),
            interfaces_traits: *dir_ifaces.get(dir).unwrap_or(&0),
            languages: dir_langs.get(dir).map(|s| s.iter().cloned().collect()).unwrap_or_default(),
            key_symbols: dir_symbols.remove(dir).unwrap_or_default(),
        }
    }).collect();
    modules.sort_by_key(|m| std::cmp::Reverse(m.functions));

    // 3. Cross-module dependencies (C3: use REL_IMPORTS constant)
    let mut dep_map: HashMap<(String, String), usize> = HashMap::new();
    {
        // Exclude the synthetic `<external>` bucket: imports of external/builtin
        // packages must not surface as an internal `→ <root>` dependency (they
        // otherwise dominate the Dependencies section and collide with the real
        // root source module).
        let sql = "SELECT sf.path, tf.path, COUNT(*) \
             FROM edges e \
             JOIN nodes sn ON sn.id = e.source_id \
             JOIN nodes tn ON tn.id = e.target_id \
             JOIN files sf ON sf.id = sn.file_id \
             JOIN files tf ON tf.id = tn.file_id \
             WHERE e.relation = ?1 AND sf.path != tf.path \
               AND sf.path != '<external>' AND tf.path != '<external>' \
             GROUP BY sf.path, tf.path";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([REL_IMPORTS], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)? as usize))
        })?;
        for row in rows {
            let (from_file, to_file, count) = row?;
            let from_dir = dir_of(&from_file).to_string();
            let to_dir = dir_of(&to_file).to_string();
            if from_dir != to_dir {
                *dep_map.entry((from_dir, to_dir)).or_default() += count;
            }
        }
    }
    let mut deps: Vec<ModuleDep> = dep_map.into_iter()
        .map(|((from, to), count)| ModuleDep { from, to, import_count: count })
        .collect();
    deps.sort_by_key(|d| std::cmp::Reverse(d.import_count));

    // 4. HTTP entry points (C3: use REL_ROUTES_TO constant)
    let mut entry_points = Vec::new();
    {
        let sql = "SELECT sn.name, sf.path, e.metadata \
             FROM edges e \
             JOIN nodes sn ON sn.id = e.source_id \
             JOIN files sf ON sf.id = sn.file_id \
             WHERE e.relation = ?1 \
             LIMIT 20";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([REL_ROUTES_TO], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?))
        })?;
        for row in rows {
            let (handler, file, metadata) = row?;
            let route = if let Some(ref meta) = metadata {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(meta) {
                    let method = v["method"].as_str().unwrap_or("ALL");
                    let path = v["path"].as_str().unwrap_or("?");
                    format!("{} {}", method, path)
                } else {
                    "?".into()
                }
            } else {
                "?".into()
            };
            entry_points.push(EntryPoint { route, handler, file, kind: "http_route".into() });
        }
    }

    // 4b. Program entry points: main functions with no callers (Rust/Go/C/Python/Java)
    if entry_points.is_empty() {
        let sql = "SELECT n.name, f.path FROM nodes n \
             JOIN files f ON f.id = n.file_id \
             WHERE n.name = 'main' AND n.type = 'function' \
               AND NOT EXISTS (SELECT 1 FROM edges e WHERE e.target_id = n.id AND e.relation = ?1) \
             LIMIT 5";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([REL_CALLS], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (name, file) = row?;
            entry_points.push(EntryPoint { route: "main".into(), handler: name, file, kind: "main".into() });
        }
    }

    // 5. Hot functions (C1: filter test code, split prod/test caller counts, C3: use REL_CALLS constant)
    // benches/ is classified as test/harness — see domain.rs::is_test_symbol.
    // Source-side filter clauses come from domain helpers; target-side `n.*`/`f.*`
    // filter is inline because aliases differ (n/f vs src/sf).
    let mut hot_functions = Vec::new();
    {
        let prod_join = crate::domain::prod_source_join_sql("e");
        let prod_where = crate::domain::PROD_SOURCE_FILTER_AND;
        let test_where = crate::domain::TEST_SOURCE_FILTER_OR;
        let sql = format!(
            "SELECT n.name, n.type, f.path, \
               COUNT(CASE WHEN {prod_where} THEN e.id END) as prod_cnt, \
               COUNT(CASE WHEN {test_where} THEN e.id END) as test_cnt \
             FROM nodes n \
             JOIN files f ON f.id = n.file_id \
             JOIN edges e ON e.target_id = n.id \
             {prod_join} \
             WHERE e.relation = ?1 \
               AND n.type IN ('function', 'method') \
               AND n.name != '<module>' \
               AND n.is_test = 0 \
               AND n.name NOT LIKE 'test\\_%' ESCAPE '\\' \
               AND f.path NOT LIKE 'tests/%' \
               AND f.path NOT LIKE 'benches/%' \
               AND f.path NOT LIKE '%_test.%' \
             GROUP BY n.name, n.type, f.path \
             HAVING prod_cnt > 0 \
             ORDER BY prod_cnt DESC \
             LIMIT 15"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([REL_CALLS], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? as usize, row.get::<_, i64>(4)? as usize))
        })?;
        for row in rows {
            let (name, node_type, file, count, test_count) = row?;
            hot_functions.push(HotFunction { name, node_type, file, caller_count: count, test_caller_count: test_count });
        }
    }

    Ok((modules, deps, entry_points, hot_functions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::helpers::test_db;

    #[test]
    fn test_project_map_excludes_external_and_counts_methods() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        // Real source file: 1 class + 1 method + 1 function = 3 symbols.
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('src/svc.py', 'h1', 0, 'python', 0)", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'class', 'Svc', 'Svc', 1, 9, 'class Svc')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'method', 'handle', 'Svc.handle', 2, 4, 'def handle')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'function', 'helper', 'helper', 6, 8, 'def helper')", []).unwrap();
        // Synthetic <external> pseudo-file: an unresolved external trait — must
        // not be counted as a project symbol nor surface as a module/dependency.
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('<external>', 'h2', 0, 'python', 0)", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (2, 'trait', 'Drop', 'Drop', 0, 0, '')", []).unwrap();
        // svc.py imports the external symbol (should NOT become a `→ <root>` dep).
        conn.execute("INSERT INTO edges (source_id, target_id, relation) VALUES (1, 4, 'imports')", []).unwrap();

        let (modules, deps, _eps, _hot) = get_project_map(conn).unwrap();

        let src = modules.iter().find(|m| m.path == "src").expect("src module present");
        // class(1) + method(1) + function(1) — method must count toward the total.
        assert_eq!(src.functions + src.classes + src.interfaces_traits, 3,
            "symbol total must include the method");
        // No module derived from the synthetic <external> bucket (dir_of → "<root>").
        assert!(modules.iter().all(|m| m.path != "<root>"),
            "external pseudo-file must not appear as a <root> module");
        // The external import must not surface as an internal dependency.
        assert!(deps.iter().all(|d| d.to != "<root>" && d.from != "<root>"),
            "external import must not surface as a <root> dependency");
    }
}
