use anyhow::Result;
use rusqlite::Connection;

pub fn insert_node_vector(conn: &Connection, node_id: i64, embedding: &[f32]) -> Result<()> {
    let bytes: &[u8] = bytemuck::cast_slice(embedding);
    conn.execute(
        "INSERT OR REPLACE INTO node_vectors(node_id, embedding) VALUES (?1, ?2)",
        rusqlite::params![node_id, bytes],
    )?;
    Ok(())
}

/// Batch insert vectors using a single prepared statement.
/// For best performance, caller should wrap in a transaction (avoids per-statement fsync).
pub fn insert_node_vectors_batch(conn: &Connection, vectors: &[(i64, Vec<f32>)]) -> Result<()> {
    if vectors.is_empty() {
        return Ok(());
    }
    // vec0 virtual tables do not support INSERT OR REPLACE, so delete first.
    let mut del_stmt = conn.prepare_cached(
        "DELETE FROM node_vectors WHERE node_id = ?1"
    )?;
    let mut ins_stmt = conn.prepare_cached(
        "INSERT INTO node_vectors(node_id, embedding) VALUES (?1, ?2)"
    )?;
    for (node_id, embedding) in vectors {
        let bytes: &[u8] = bytemuck::cast_slice(embedding);
        del_stmt.execute(rusqlite::params![node_id])?;
        ins_stmt.execute(rusqlite::params![node_id, bytes])?;
    }
    Ok(())
}

/// Drop vectors for the given node IDs so the background embedder re-selects them
/// via the `node_vectors.node_id IS NULL` convention in `get_unembedded_nodes`.
/// Used by the incremental path when context strings changed but no model was
/// available to re-embed inline (the watcher/drift path passes model=None to avoid
/// holding the model lock across I/O). Wrapped in a transaction to avoid per-row fsync.
pub fn delete_node_vectors_batch(conn: &Connection, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    {
        let mut del_stmt = conn.prepare_cached(
            "DELETE FROM node_vectors WHERE node_id = ?1"
        )?;
        for id in ids {
            del_stmt.execute(rusqlite::params![id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn vector_search(conn: &Connection, query_embedding: &[f32], limit: i64) -> Result<Vec<(i64, f64)>> {
    let bytes: &[u8] = bytemuck::cast_slice(query_embedding);
    let mut stmt = conn.prepare(
        "SELECT node_id, distance FROM node_vectors WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2"
    )?;
    let rows = stmt.query_map(rusqlite::params![bytes, limit], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;
    let results = rows.collect::<Result<Vec<_>, _>>()?;
    Ok(results)
}

pub fn get_node_embedding(conn: &Connection, node_id: i64) -> Result<Vec<u8>> {
    let bytes: Vec<u8> = conn.query_row(
        "SELECT embedding FROM node_vectors WHERE node_id = ?1",
        [node_id],
        |row| row.get(0),
    )?;
    Ok(bytes)
}

// --- Unembedded nodes ---

/// Get (node_id, context_string) for nodes that have context strings but no vectors.
/// Returns at most `limit` rows per call to bound memory usage.
pub fn get_unembedded_nodes(conn: &Connection, limit: usize) -> Result<Vec<(i64, String)>> {
    // Priority: embed hot-path nodes first (most referenced = highest value for search)
    // Uses LEFT JOIN + GROUP BY instead of correlated subquery for better performance
    let mut stmt = conn.prepare(
        "SELECT n.id, n.context_string
         FROM nodes n
         LEFT JOIN node_vectors nv ON n.id = nv.node_id
         LEFT JOIN edges e ON e.target_id = n.id
         WHERE nv.node_id IS NULL AND n.context_string IS NOT NULL
         GROUP BY n.id
         ORDER BY COUNT(e.target_id) DESC
         LIMIT ?1"
    )?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

/// Like [`get_unembedded_nodes`] but skips `exclude` node IDs in SQL. The backfill loops
/// pass the set of nodes that failed to embed THIS run so the same hot-path-first poison
/// node isn't re-fetched at the head of every batch (which would starve the embeddable
/// nodes behind it, or — in the CLI loop that only stops on an empty result — spin forever).
pub fn get_unembedded_nodes_excluding(
    conn: &Connection,
    limit: usize,
    exclude: &[i64],
) -> Result<Vec<(i64, String)>> {
    if exclude.is_empty() {
        return get_unembedded_nodes(conn, limit);
    }
    // Don't bind one parameter per excluded id: on a large repo the backfill
    // loop's `failed` set can grow toward the full node count, and a single
    // `NOT IN (?,?,…)` would exceed SQLite's variable cap (issue #30). The
    // GROUP BY / ORDER BY / LIMIT ranking can't be split across NOT-IN chunks,
    // so instead over-fetch by |exclude| and drop the excluded ids in Rust:
    // the limit-th non-excluded row sits at position <= limit + |exclude| in
    // the ranked stream, so this window always yields the same top-`limit` set
    // the SQL filter would have.
    let exclude_set: std::collections::HashSet<i64> = exclude.iter().copied().collect();
    let over_fetch = limit.saturating_add(exclude.len());
    let rows = get_unembedded_nodes(conn, over_fetch)?;
    Ok(rows
        .into_iter()
        .filter(|(id, _)| !exclude_set.contains(id))
        .take(limit)
        .collect())
}

/// Count nodes with embeddings vs total embeddable nodes.
/// Returns (with_vectors, total_embeddable).
pub fn count_nodes_with_vectors(conn: &Connection) -> Result<(i64, i64)> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM nodes WHERE context_string IS NOT NULL", [], |r| r.get(0)
    )?;
    // node_vectors table may not exist when embed-model feature is disabled; return 0 in that case
    let with_vectors: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_vectors", [], |r| r.get(0)
    ).unwrap_or(0);
    Ok((with_vectors, total))
}

/// Count embeddable-but-unembedded nodes (have a `context_string`, no vector yet).
/// Mirrors the `WHERE` filter of [`get_unembedded_nodes`] but returns only the count,
/// so the periodic backfill driver can cheaply detect whether NEW un-embedded work has
/// appeared (e.g. nodes added by a CLI/hook `ensure_file_indexed` with `model=None`)
/// without fetching payloads or loading the embedding model. Returns 0 when the vector
/// table is absent (embed-model feature disabled).
pub fn count_unembedded_nodes(conn: &Connection) -> Result<i64> {
    // Probe for the vec table explicitly so its ABSENCE (embed-model disabled) returns 0,
    // while a genuine read error on the count below (e.g. SQLITE_BUSY under writer
    // contention) PROPAGATES as Err. A blanket `.unwrap_or(0)` would mask that transient
    // as "no work", and the periodic backfill driver — which falls back to its current
    // floor on Err — would instead reset its floor to 0 and futilely reload the model.
    let has_vectors_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='node_vectors'",
        [], |r| r.get(0),
    )?;
    if has_vectors_table == 0 {
        return Ok(0);
    }
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM nodes n \
         LEFT JOIN node_vectors nv ON n.id = nv.node_id \
         WHERE nv.node_id IS NULL AND n.context_string IS NOT NULL",
        [], |r| r.get(0),
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::files::{upsert_file, FileRecord};
    use super::super::helpers::test_db;
    use super::super::nodes::{insert_node, NodeRecord};

    #[test]
    fn node_delete_reaps_vector_no_orphan_either_path() {
        // The v0.79.1 audit flagged "orphan vectors never GC'd: vec0 has no FK". In
        // fact the `nodes_vectors_ad` AFTER DELETE trigger reaps a node's vector on
        // BOTH a direct node delete AND an FK-cascade delete (file removal): SQLite
        // fires a child table's AFTER DELETE trigger on FK cascade even with
        // recursive_triggers off (production: foreign_keys=ON, recursive_triggers
        // unset). So no orphan is ever created — this guards that invariant against a
        // future change to the trigger, the delete path, or the pragmas.
        use super::super::files::delete_files_by_paths;
        use super::super::nodes::delete_nodes_by_file;
        let (db, _tmp) = test_db();
        let conn = db.conn();
        conn.execute_batch(&crate::storage::schema::create_vec_tables_sql()).unwrap();
        let vec_count = |c: &Connection| -> i64 {
            c.query_row("SELECT COUNT(*) FROM node_vectors", [], |r| r.get(0)).unwrap()
        };
        let add_embedded = |c: &Connection, path: &str| -> i64 {
            let fid = upsert_file(c, &FileRecord {
                path: path.into(), blake3_hash: "h".into(), last_modified: 1, language: None,
            }).unwrap();
            let nid = insert_node(c, &NodeRecord {
                file_id: fid, node_type: "function".into(), name: "f".into(),
                qualified_name: None, start_line: 1, end_line: 2, code_content: String::new(),
                signature: None, doc_comment: None, context_string: Some("ctx".into()),
                name_tokens: None, return_type: None, param_types: None, is_test: false,
            }).unwrap();
            insert_node_vector(c, nid, &vec![0.0f32; crate::domain::EMBEDDING_DIM]).unwrap();
            fid
        };

        // Path 1 — file removal → FK cascade deletes the node → trigger reaps the vector.
        add_embedded(conn, "a.ts");
        assert_eq!(vec_count(conn), 1, "vector inserted");
        delete_files_by_paths(conn, &["a.ts".into()]).unwrap();
        assert_eq!(vec_count(conn), 0, "FK-cascade delete must reap the vector (no orphan)");

        // Path 2 — direct node delete (the changed-file reindex path) → trigger reaps it.
        let fid2 = add_embedded(conn, "b.ts");
        assert_eq!(vec_count(conn), 1, "vector inserted");
        delete_nodes_by_file(conn, fid2).unwrap();
        assert_eq!(vec_count(conn), 0, "direct node delete must reap the vector (no orphan)");
    }

    #[test]
    fn test_get_unembedded_nodes_priority_order() {
        // Verify that get_unembedded_nodes returns nodes ordered by edge reference count (most referenced first)
        let (db, _tmp) = test_db();
        let conn = db.conn();
        let fid = upsert_file(conn, &FileRecord {
            path: "t.ts".into(), blake3_hash: "h".into(), last_modified: 1, language: None,
        }).unwrap();

        // Create 3 nodes with context strings
        let nid1 = insert_node(conn, &NodeRecord {
            file_id: fid, node_type: "function".into(), name: "popular".into(),
            qualified_name: None, start_line: 1, end_line: 5,
            code_content: "function popular() {}".into(),
            signature: None, doc_comment: None, context_string: Some("function popular".into()),
            name_tokens: None, return_type: None, param_types: None, is_test: false,
        }).unwrap();
        let nid2 = insert_node(conn, &NodeRecord {
            file_id: fid, node_type: "function".into(), name: "moderate".into(),
            qualified_name: None, start_line: 10, end_line: 15,
            code_content: "function moderate() {}".into(),
            signature: None, doc_comment: None, context_string: Some("function moderate".into()),
            name_tokens: None, return_type: None, param_types: None, is_test: false,
        }).unwrap();
        let nid3 = insert_node(conn, &NodeRecord {
            file_id: fid, node_type: "function".into(), name: "lonely".into(),
            qualified_name: None, start_line: 20, end_line: 25,
            code_content: "function lonely() {}".into(),
            signature: None, doc_comment: None, context_string: Some("function lonely".into()),
            name_tokens: None, return_type: None, param_types: None, is_test: false,
        }).unwrap();

        // Create a caller node (no context string so it won't appear in results)
        let caller = insert_node(conn, &NodeRecord {
            file_id: fid, node_type: "function".into(), name: "caller".into(),
            qualified_name: None, start_line: 30, end_line: 35,
            code_content: "function caller() {}".into(),
            signature: None, doc_comment: None, context_string: None,
            name_tokens: None, return_type: None, param_types: None, is_test: false,
        }).unwrap();

        // "popular" gets 3 incoming edges, "moderate" gets 1, "lonely" gets 0
        for _ in 0..3 {
            // Use different callers for unique edges - but we only have one caller node
            // Use different relations to make them unique
            conn.execute(
                "INSERT OR IGNORE INTO edges (source_id, target_id, relation) VALUES (?1, ?2, ?3)",
                rusqlite::params![caller, nid1, "calls"],
            ).unwrap();
        }
        // Add additional edges with different metadata to make them unique
        conn.execute(
            "INSERT INTO edges (source_id, target_id, relation, metadata) VALUES (?1, ?2, 'calls', 'a')",
            rusqlite::params![caller, nid1],
        ).unwrap();
        conn.execute(
            "INSERT INTO edges (source_id, target_id, relation, metadata) VALUES (?1, ?2, 'calls', 'b')",
            rusqlite::params![caller, nid1],
        ).unwrap();
        conn.execute(
            "INSERT INTO edges (source_id, target_id, relation) VALUES (?1, ?2, 'calls')",
            rusqlite::params![caller, nid2],
        ).unwrap();

        // Create vec tables for the LEFT JOIN to work
        conn.execute_batch(&crate::storage::schema::create_vec_tables_sql()).unwrap();

        let results = get_unembedded_nodes(conn, 10).unwrap();
        assert_eq!(results.len(), 3, "should return all 3 nodes with context strings");

        // First result should be "popular" (most referenced: 3 edges)
        assert_eq!(results[0].0, nid1, "most referenced node should be first");
        // Second should be "moderate" (1 edge)
        assert_eq!(results[1].0, nid2, "moderately referenced node should be second");
        // Third should be "lonely" (0 edges)
        assert_eq!(results[2].0, nid3, "unreferenced node should be last");
    }

    #[test]
    fn test_get_unembedded_nodes_excluding_skips_ids() {
        // The backfill loops use this to advance past nodes that failed to embed; verify
        // excluded IDs are never returned even though they're still unembedded, and that
        // excluding the whole set yields empty (so the loop terminates instead of spinning).
        let (db, _tmp) = test_db();
        let conn = db.conn();
        let fid = upsert_file(conn, &FileRecord {
            path: "t.ts".into(), blake3_hash: "h".into(), last_modified: 1, language: None,
        }).unwrap();
        let mk = |name: &str| insert_node(conn, &NodeRecord {
            file_id: fid, node_type: "function".into(), name: name.into(),
            qualified_name: None, start_line: 1, end_line: 2,
            code_content: format!("function {name}() {{}}"),
            signature: None, doc_comment: None, context_string: Some(format!("function {name}")),
            name_tokens: None, return_type: None, param_types: None, is_test: false,
        }).unwrap();
        let a = mk("aa");
        let b = mk("bb");
        let c = mk("cc");
        conn.execute_batch(&crate::storage::schema::create_vec_tables_sql()).unwrap();

        // No exclusion → all three (delegates to get_unembedded_nodes).
        assert_eq!(get_unembedded_nodes_excluding(conn, 10, &[]).unwrap().len(), 3);

        // Excluding b → only a and c; b never appears though it's still unembedded.
        let got = get_unembedded_nodes_excluding(conn, 10, &[b]).unwrap();
        let ids: Vec<i64> = got.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), 2, "excluded node must be skipped");
        assert!(ids.contains(&a) && ids.contains(&c) && !ids.contains(&b));

        // Excluding every unembedded node → empty, the backfill loop's termination signal.
        assert!(get_unembedded_nodes_excluding(conn, 10, &[a, b, c]).unwrap().is_empty());
    }

    #[test]
    fn test_get_unembedded_nodes_excluding_large_set() {
        // Regression for issue #30: the old NOT IN (?,?,…) bound one parameter
        // per excluded id, so a `failed` set near the node count blew SQLite's
        // variable cap. The over-fetch+filter path must still return the right
        // non-excluded nodes when |exclude| > MAX_IN_PARAMS.
        use super::super::helpers::MAX_IN_PARAMS;
        let (db, _tmp) = test_db();
        let conn = db.conn();
        let fid = upsert_file(conn, &FileRecord {
            path: "t.ts".into(), blake3_hash: "h".into(), last_modified: 1, language: None,
        }).unwrap();
        conn.execute_batch(&crate::storage::schema::create_vec_tables_sql()).unwrap();

        let n = MAX_IN_PARAMS + 3; // 503 unembedded nodes
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            ids.push(insert_node(conn, &NodeRecord {
                file_id: fid, node_type: "function".into(), name: format!("f{i}"),
                qualified_name: None, start_line: i as i64 + 1, end_line: i as i64 + 1,
                code_content: String::new(), signature: None, doc_comment: None,
                context_string: Some(format!("ctx{i}")), name_tokens: None,
                return_type: None, param_types: None, is_test: false,
            }).unwrap());
        }

        // Exclude the first MAX_IN_PARAMS + 1 ids (crosses the old IN-clause cap).
        let exclude = &ids[..MAX_IN_PARAMS + 1];
        let got = get_unembedded_nodes_excluding(conn, 10, exclude).unwrap();
        let got_ids: std::collections::HashSet<i64> = got.iter().map(|(id, _)| *id).collect();
        let expected: std::collections::HashSet<i64> = ids[MAX_IN_PARAMS + 1..].iter().copied().collect();
        assert_eq!(got_ids, expected, "exactly the non-excluded nodes must remain");

        // Excluding everything still terminates with an empty result.
        assert!(get_unembedded_nodes_excluding(conn, 10, &ids).unwrap().is_empty());
    }
}
