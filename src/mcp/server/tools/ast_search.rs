//! `ast_search` — structural enumeration with type/returns/params filters.
//! Generic-fallback hint kicks in when zero hits + returns_filter has angle brackets.

use super::super::*;

impl McpServer {
    pub(in crate::mcp::server) fn tool_ast_search(&self, args: &serde_json::Value) -> Result<serde_json::Value> {
        if !should_skip_indexing(args) {
            self.ensure_indexed()?;
        }

        let query = args["query"].as_str().map(|s| s.trim()).filter(|s| !s.is_empty());
        let type_filter = args["type"].as_str();
        let returns_filter = args["returns"].as_str();
        let params_filter = args["params"].as_str();
        let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;

        let has_filters = type_filter.is_some() || returns_filter.is_some() || params_filter.is_some();
        if query.is_none() && !has_filters {
            return Err(anyhow!("Either query or at least one filter (type, returns, params) is required."));
        }

        // Validate type up-front: unknown aliases normalize to an empty Vec,
        // which would silently filter every node away. Surface the typo so the
        // caller doesn't read "No results" and assume the index is empty.
        if let Some(tf) = type_filter {
            if crate::domain::normalize_type_filter(tf).is_empty() {
                return Err(anyhow!(
                    "Unknown type filter: '{}'. Valid: fn, class, struct, enum, trait, type, const, var",
                    tf
                ));
            }
        }

        let results: Vec<queries::NodeWithFile> = if let Some(q) = query {
            // FTS5 search + filter in Rust
            let fts_result = queries::fts5_search(self.db.conn(), q, (limit * 4) as i64)?;
            if fts_result.nodes.is_empty() {
                return Ok(json!({ "results": [], "message": "No results found." }));
            }
            let node_ids: Vec<i64> = fts_result.nodes.iter().map(|n| n.id).collect();
            let all = queries::get_nodes_with_files_by_ids(self.db.conn(), &node_ids)?;

            // Preserve FTS5 rank order
            let id_order: std::collections::HashMap<i64, usize> = node_ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
            let mut sorted = all;
            sorted.sort_by_key(|nwf| id_order.get(&nwf.node.id).copied().unwrap_or(usize::MAX));

            sorted.into_iter()
                .filter(|nwf| {
                    let n = &nwf.node;
                    // Skip <module>/<external> placeholders and test symbols, consistent
                    // with semantic_code_search/find_similar_code (is_skippable_result).
                    // ast_search previously leaked these into structural results.
                    if crate::domain::is_skippable_result(&n.node_type, &n.name, &nwf.file_path) {
                        return false;
                    }
                    if let Some(tf) = type_filter {
                        let types = normalize_type_filter_mcp(tf);
                        if !types.contains(&n.node_type) {
                            return false;
                        }
                    }
                    if let Some(rf) = returns_filter {
                        match &n.return_type {
                            Some(rt) => if !rt.to_lowercase().contains(&rf.to_lowercase()) { return false; },
                            None => return false,
                        }
                    }
                    if let Some(pf) = params_filter {
                        match &n.param_types {
                            Some(pt) => if !pt.to_lowercase().contains(&pf.to_lowercase()) { return false; },
                            None => return false,
                        }
                    }
                    true
                })
                .take(limit)
                .collect()
        } else {
            // Filter-only: direct SQL
            let normalized = type_filter.map(normalize_type_filter_mcp);
            let type_refs: Option<Vec<&str>> = normalized.as_ref()
                .map(|v| v.iter().map(|s| s.as_str()).collect());
            queries::get_nodes_with_files_by_filters(
                self.db.conn(),
                type_refs.as_deref(),
                returns_filter, params_filter, None, limit,
            )?
        };

        let items: Vec<serde_json::Value> = results.iter().map(|nwf| {
            let n = &nwf.node;
            json!({
                "node_id": n.id,
                "name": n.qualified_name.as_deref().unwrap_or(&n.name),
                "type": n.node_type,
                "file_path": nwf.file_path,
                "start_line": n.start_line,
                "end_line": n.end_line,
                "signature": n.signature,
                "return_type": n.return_type,
                "param_types": n.param_types,
            })
        }).collect();

        let mut response = json!({
            "results": items,
            "count": items.len(),
        });

        // FTS-rank fallback: when query+type returns zero (FTS rank can drown
        // structs/enums under function-name hits — e.g. query="Result" type=struct
        // bottoms out because top FTS hits for "Result" are functions like
        // `compress_results`), retry as SQL `name LIKE '%query%'` + type filter.
        // Single-identifier queries only — multi-word/operator queries stay FTS-only
        // since LIKE substring is not a useful fallback for them.
        if items.is_empty() && type_filter.is_some() {
            if let Some(q) = query {
                if is_identifier_like(q) {
                    let normalized = type_filter.map(normalize_type_filter_mcp);
                    let type_refs: Option<Vec<&str>> = normalized.as_ref()
                        .map(|v| v.iter().map(|s| s.as_str()).collect());
                    let retry = queries::get_nodes_with_files_by_filters(
                        self.db.conn(), type_refs.as_deref(),
                        returns_filter, params_filter, Some(q), limit,
                    )?;
                    if !retry.is_empty() {
                        let retry_items: Vec<serde_json::Value> = retry.iter().map(|nwf| {
                            let n = &nwf.node;
                            json!({
                                "node_id": n.id,
                                "name": n.qualified_name.as_deref().unwrap_or(&n.name),
                                "type": n.node_type,
                                "file_path": nwf.file_path,
                                "start_line": n.start_line,
                                "end_line": n.end_line,
                                "signature": n.signature,
                                "return_type": n.return_type,
                                "param_types": n.param_types,
                            })
                        }).collect();
                        response["results"] = json!(retry_items);
                        response["count"] = json!(retry_items.len());
                        response["hint"] = json!(format!(
                            "FTS rank had no '{}' under type filter; falling back to name-substring match.", q
                        ));
                    }
                }
            }
        }

        // Generic-fallback hint: when returns_filter has angle brackets and zero hits,
        // retry with the inner-most type as a suggestion so the caller sees "did you mean Relation?"
        // rather than an empty response.
        if response["count"].as_u64().unwrap_or(0) == 0 {
            if let Some(rf) = returns_filter {
                if let Some(inner) = strip_outer_generic(rf) {
                    let normalized = type_filter.map(normalize_type_filter_mcp);
                    let type_refs: Option<Vec<&str>> = normalized.as_ref()
                        .map(|v| v.iter().map(|s| s.as_str()).collect());
                    let retry = queries::get_nodes_with_files_by_filters(
                        self.db.conn(), type_refs.as_deref(),
                        Some(&inner), params_filter, None, 100,
                    )?;
                    if !retry.is_empty() {
                        let n = retry.len();
                        let plural = if n == 1 { "" } else { "es" };
                        response["hint"] = json!(format!(
                            "No match for returns='{}'. Substring '{}' has {} match{} — try that.",
                            rf, inner, n, plural
                        ));
                        let mut suggested = serde_json::Map::new();
                        suggested.insert("returns".to_string(), json!(inner));
                        if let Some(tf) = type_filter { suggested.insert("type".to_string(), json!(tf)); }
                        if let Some(pf) = params_filter { suggested.insert("params".to_string(), json!(pf)); }
                        if let Some(q) = query { suggested.insert("query".to_string(), json!(q)); }
                        response["suggested_query"] = serde_json::Value::Object(suggested);
                    }
                }
            }
        }

        Ok(response)
    }
}

/// True when `s` looks like a single identifier (alphanumeric + underscore, no
/// whitespace). Used to gate the name-substring fallback — multi-word queries
/// like "function returning result" should not silently turn into LIKE patterns.
fn is_identifier_like(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::is_identifier_like;

    #[test]
    fn test_is_identifier_like() {
        assert!(is_identifier_like("Result"));
        assert!(is_identifier_like("FtsResult"));
        assert!(is_identifier_like("snake_case"));
        assert!(is_identifier_like("with42numbers"));
        assert!(is_identifier_like("中文标识符"));
        assert!(!is_identifier_like(""));
        assert!(!is_identifier_like("two words"));
        assert!(!is_identifier_like("Result<T>"));
        assert!(!is_identifier_like("a:b"));
        assert!(!is_identifier_like("path/to/file"));
    }
}
