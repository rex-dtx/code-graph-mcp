//! Route-aware caller traversal. Composes a call-graph traversal (this layer)
//! with route-edge metadata (storage layer). Lives in `graph`, not `storage`,
//! so storage never has to import graph — the dependency runs one way
//! (graph → storage). See tests/hardening.rs::no_storage_module_imports_graph.

use anyhow::Result;
use rusqlite::Connection;

use crate::graph::query::get_call_graph_filtered;
use crate::storage::queries::routes::fetch_route_metadata_map;
use crate::storage::queries::CallerWithRouteInfo;

/// Callers of `symbol_name`, each annotated with its `routes_to` metadata if the
/// caller is itself a route handler. `min_confidence_rank` filters caller edges
/// (see domain::confidence_rank). Behavior-identical to the pre-M9a
/// `queries::get_callers_with_route_info`.
pub fn get_callers_with_route_info(
    conn: &Connection,
    symbol_name: &str,
    file_path: Option<&str>,
    max_depth: i32,
    min_confidence_rank: u8,
) -> Result<Vec<CallerWithRouteInfo>> {
    let callers = get_call_graph_filtered(
        conn,
        symbol_name,
        "callers",
        max_depth,
        file_path,
        min_confidence_rank,
    )?;
    if callers.nodes.is_empty() {
        return Ok(vec![]);
    }
    let caller_ids: Vec<i64> = callers.nodes.iter().map(|c| c.node_id).collect();
    let route_map = fetch_route_metadata_map(conn, &caller_ids)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::queries::helpers::test_db;

    #[test]
    fn test_callers_with_routes() {
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute("INSERT INTO files (path, blake3_hash, last_modified, language, indexed_at) VALUES ('test.ts', 'h1', 0, 'typescript', 0)", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'function', 'handler', 'handler', 1, 10, 'fn handler()')", []).unwrap();
        conn.execute("INSERT INTO nodes (file_id, type, name, qualified_name, start_line, end_line, code_content) VALUES (1, 'function', 'caller', 'caller', 11, 20, 'fn caller()')", []).unwrap();
        conn.execute("INSERT INTO edges (source_id, target_id, relation, metadata) VALUES (2, 1, 'calls', NULL)", []).unwrap();
        conn.execute("INSERT INTO edges (source_id, target_id, relation, metadata) VALUES (2, 2, 'routes_to', '{\"method\":\"GET\",\"path\":\"/api/test\"}')", []).unwrap();
        let results = get_callers_with_route_info(conn, "handler", None, 3, 0).unwrap();
        assert!(!results.is_empty());
        assert!(results.iter().any(|r| r.route_info.is_some()));
    }
}
