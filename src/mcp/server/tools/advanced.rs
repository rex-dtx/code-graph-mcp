//! Advanced tools (folded into core 7 in next pass — kept here as an
//! intermediate stop so the split refactor is bisectable):
//!
//! - `impact_analysis` → being absorbed by `get_ast_node include_impact=true`
//! - `trace_http_chain` → being absorbed by `get_call_graph` (`route_path` mode)
//! - `dependency_graph` → being absorbed by `module_overview` (`include_deps`)
//! - `find_similar_code` → being absorbed by `get_ast_node` (`include_similar`)
//! - `find_dead_code` → being absorbed by `ast_search` (`dead_code` filter) /
//!   `module_overview` (`include_dead`)
//!
//! Until that fold lands, these handlers stay reachable via raw JSON-RPC
//! `tools/call` and via CLI subcommands.

use super::super::*;
use super::callgraph::attach_truncation_flags;
use crate::domain::default_dead_code_ignores;

impl McpServer {
    pub(in crate::mcp::server) fn tool_trace_http_chain(&self, args: &serde_json::Value) -> Result<serde_json::Value> {
        // Empty/whitespace-only acts like missing — otherwise the substring
        // route match treats "" as a wildcard and returns "no routes found"
        // when the project has no routes, which is indistinguishable from a
        // misspelled route.
        let route_path_raw = args["route_path"].as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("route_path is required (e.g. 'GET /api/users')"))?;
        let depth = args["depth"].as_i64().unwrap_or(3).clamp(1, 20) as i32;
        let include_middleware = args["include_middleware"].as_bool().unwrap_or(true);

        if !should_skip_indexing(args) {
            self.ensure_indexed()?;
        }

        let (method_filter, route_path) = parse_route_input(route_path_raw);

        use crate::domain::{REL_CALLS, REL_ROUTES_TO};
        let mut rows = queries::find_routes_by_path(self.db.conn(), route_path, REL_ROUTES_TO)?;
        filter_routes_by_method(&mut rows, &method_filter);

        // Batch-fetch downstream calls for all handlers in one query
        let downstream_map = if include_middleware {
            let node_ids: Vec<i64> = rows.iter().map(|rm| rm.node_id).collect();
            queries::get_edge_target_names_batch(self.db.conn(), &node_ids, REL_CALLS)?
        } else {
            std::collections::HashMap::new()
        };

        let mut handlers: Vec<serde_json::Value> = Vec::new();
        for rm in &rows {
            let mut handler = json!({
                "node_id": rm.node_id,
                "metadata": rm.metadata,
                "handler_name": rm.handler_name,
                "handler_type": rm.handler_type,
                "file_path": rm.file_path,
                "start_line": rm.start_line,
                "end_line": rm.end_line,
            });

            apply_inline_handler_metadata(&mut handler, rm.metadata.as_deref());

            if include_middleware {
                let downstream = downstream_map.get(&rm.node_id)
                    .cloned()
                    .unwrap_or_default();
                handler["downstream_calls"] = json!(downstream);
            }

            // Recursive call chain via call graph
            let chain = crate::graph::query::get_call_graph(
                self.db.conn(), &rm.handler_name, "callees", depth, Some(&rm.file_path),
            )?;
            let chain_nodes: Vec<serde_json::Value> = chain.nodes.iter()
                .filter(|n| n.depth > 0) // exclude root (the handler itself)
                .filter(|n| !is_test_symbol(&n.name, &n.file_path))
                .map(|n| json!({
                    "node_id": n.node_id,
                    "name": n.name,
                    "type": n.node_type,
                    "file_path": n.file_path,
                    "depth": n.depth,
                }))
                .collect();
            handler["call_chain"] = json!(chain_nodes);
            if chain.limit_hit || chain.depth_capped {
                handler["call_chain_truncated"] = json!(true);
            }

            handlers.push(handler);
        }

        let mut result = json!({
            "route": route_path,
            "handlers": handlers,
        });
        if handlers.is_empty() {
            result["message"] = json!("No matching routes found. This may mean: (1) the project has no HTTP routes, (2) the route pattern didn't match, or (3) routes use a framework not yet supported. Try a broader pattern or use semantic_code_search to find route handlers.");
        }

        // Compress if result exceeds token threshold
        let tokens = crate::sandbox::compressor::estimate_json_tokens(&result);
        if tokens > COMPRESSION_TOKEN_THRESHOLD {
            let compressed_handlers: Vec<serde_json::Value> = handlers.iter().map(|h| {
                json!({
                    "node_id": h["node_id"],
                    "handler_name": h["handler_name"],
                    "file_path": h["file_path"],
                    "start_line": h["start_line"],
                    "end_line": h["end_line"],
                    "chain_count": h["call_chain"].as_array().map_or(0, |a| a.len()),
                })
            }).collect();
            return Ok(json!({
                "mode": "compressed_http_chain",
                "message": "HTTP chain exceeded token limit. Use get_ast_node(node_id) or get_call_graph(symbol_name) to expand.",
                "route": route_path,
                "results": compressed_handlers,
            }));
        }

        // Discourage attach_truncation_flags compile-warn for unused import in
        // case future edits drop the call_graph fanout above.
        let _ = attach_truncation_flags;

        Ok(result)
    }

    pub(in crate::mcp::server) fn tool_impact_analysis(&self, args: &serde_json::Value) -> Result<serde_json::Value> {
        // Validate enum args at tool entry, before any index/freshness work — a
        // typo'd change_type must error cleanly instead of after ensure_indexed ran
        // (which both wastes work and can mask the typo behind an index error).
        // feedback-enum-validate-at-entry.
        let change_type = args.get("change_type")
            .and_then(|v| v.as_str())
            .unwrap_or("behavior");
        if !matches!(change_type, "signature" | "behavior" | "remove") {
            return Err(anyhow!("change_type must be one of: signature, behavior, remove (got '{}')", change_type));
        }

        if !should_skip_indexing(args) {
            self.ensure_indexed()?;
            // Edit-aware: file_path is the disambiguator for "I just changed
            // this symbol in this file, what breaks?" — refresh that file
            // before computing impact so the result reflects post-edit edges.
            self.ensure_file_fresh_opt(args.get("file_path").and_then(|v| v.as_str()))?;
        }

        let symbol_name = required_str(args, "symbol_name")?;
        let depth = args.get("depth")
            .and_then(|v| v.as_i64())
            .unwrap_or(3)
            .clamp(1, 20) as i32;
        let file_path = args.get("file_path").and_then(|v| v.as_str());

        // Disambiguate: check if symbol matches multiple distinct nodes (cross-file
        // OR same-file overloads). Message + suggestion shape shared with the CLI
        // and get_call_graph via crate::resolve (audit #6).
        if file_path.is_none() {
            if let Some(cands) = self.disambiguate_symbol(symbol_name)? {
                return Ok(json!({
                    "symbol": symbol_name,
                    "change_type": change_type,
                    "error": crate::resolve::ambiguity_message(symbol_name, &cands, crate::resolve::Surface::Mcp),
                    "suggestions": crate::resolve::candidates_to_json(&cands),
                }));
            }
        }

        let mut resolved_name = symbol_name.to_string();
        let mut callers = queries::get_callers_with_route_info(
            self.db.conn(), symbol_name, file_path, depth
        )?;

        // Fuzzy fallback: if no callers found, try fuzzy name resolution
        if callers.is_empty() {
            match self.resolve_fuzzy_name(symbol_name)? {
                FuzzyResolution::Unique(resolved) => {
                    resolved_name = resolved;
                    callers = queries::get_callers_with_route_info(
                        self.db.conn(), &resolved_name, file_path, depth
                    )?;
                }
                FuzzyResolution::Ambiguous(suggestions) => {
                    return Ok(json!({
                        "symbol": symbol_name,
                        "change_type": change_type,
                        "direct_callers": [],
                        "transitive_callers": [],
                        "affected_routes": [],
                        "affected_files": 0,
                        "risk_level": "LOW",
                        "summary": format!("No exact match for '{}'. Did you mean one of these?", symbol_name),
                        "candidates": suggestions,
                    }));
                }
                FuzzyResolution::NotFound => {
                    return Err(anyhow!("Symbol '{}' not found in index. Cannot assess impact. Use semantic_code_search to find the correct symbol name.", symbol_name));
                }
            }
        }

        // Exclude root node (depth 0) — it's the queried symbol itself, not a caller
        let callers: Vec<_> = callers.into_iter().filter(|c| c.depth > 0).collect();

        // Separate production callers from test callers
        let is_test = |c: &&queries::CallerWithRouteInfo| {
            is_test_symbol(&c.name, &c.file_path)
        };
        let prod_callers: Vec<_> = callers.iter().filter(|c| !is_test(c)).collect();
        let test_callers: Vec<_> = callers.iter().filter(|c| is_test(c)).collect();

        let affected_files: std::collections::HashSet<&str> = prod_callers.iter()
            .map(|c| c.file_path.as_str()).collect();
        let affected_routes: Vec<serde_json::Value> = callers.iter()
            .filter_map(|c| {
                c.route_info.as_ref().and_then(|meta| serde_json::from_str(meta).ok())
            }).collect();

        let direct: Vec<_> = prod_callers.iter().filter(|c| c.depth == 1).collect();
        let transitive: Vec<_> = prod_callers.iter().filter(|c| c.depth > 1).collect();

        // Non-function symbols (constant/struct/enum/trait/...) have usages beyond
        // the call graph (imports, field access, type annotations). With zero
        // callers, risk must be UNKNOWN rather than the default LOW.
        let type_warning = if prod_callers.is_empty() {
            let nodes = queries::get_nodes_by_name(self.db.conn(), &resolved_name)?;
            let is_function_like = nodes.iter()
                .any(|n| crate::domain::is_function_node_type(n.node_type.as_str()));
            if !is_function_like {
                Some(crate::domain::NON_FUNCTION_IMPACT_WARNING)
            } else {
                None
            }
        } else {
            None
        };

        // Risk based on production callers, not test callers. When the target is a
        // type with no call-graph callers, flag it UNKNOWN instead of LOW — the
        // warning above already explains why, but risk_level is the field LLMs act on.
        let risk_level: &'static str = if type_warning.is_some() {
            "UNKNOWN"
        } else {
            crate::domain::compute_risk_level(
                prod_callers.len(), affected_routes.len(), change_type == "remove"
            )
        };

        // Value references: callers that mention this symbol as a VALUE (callback /
        // fn pointer / type-position) rather than calling it — coupling the call
        // graph misses. Separate depth-1 signal; NEVER folded into the calls-based
        // caller counts so `direct_callers` stays pure control-flow. Prod sources,
        // deduped by referencing symbol.
        let value_references = {
            let nodes = queries::get_nodes_by_name(self.db.conn(), &resolved_name)?;
            let mut seen = std::collections::HashSet::new();
            for n in &nodes {
                let refs = queries::get_incoming_references(
                    self.db.conn(), n.id, Some(crate::domain::REL_REFERENCES)
                )?;
                for r in refs {
                    if is_test_symbol(&r.name, &r.file_path) {
                        continue;
                    }
                    seen.insert((r.name.clone(), r.file_path.clone()));
                }
            }
            seen.len()
        };

        let mut result = json!({
            "symbol": &resolved_name,
            "change_type": change_type,
            "direct_callers": direct.iter().map(|c| json!({
                "name": c.name, "file": c.file_path, "depth": c.depth
            })).collect::<Vec<_>>(),
            "transitive_callers": transitive.iter().map(|c| json!({
                "name": c.name, "file": c.file_path, "depth": c.depth
            })).collect::<Vec<_>>(),
            "affected_routes": affected_routes,
            "affected_files": affected_files.len(),
            "value_references": value_references,
            "risk_level": risk_level,
            "tests_affected": test_callers.len(),
            "summary": format!("Changing {} affects {} routes, {} functions across {} files [{}] ({} tests affected)",
                &resolved_name, affected_routes.len(), prod_callers.len(), affected_files.len(), risk_level, test_callers.len())
        });
        if let Some(warning) = type_warning {
            result["warning"] = json!(warning);
        }
        Ok(result)
    }

    pub(in crate::mcp::server) fn tool_dependency_graph(&self, args: &serde_json::Value) -> Result<serde_json::Value> {
        // Validate required + enum args at tool entry, before any index/freshness
        // work, so a missing file_path or bogus direction errors cleanly instead of
        // after ensure_indexed ran. feedback-enum-validate-at-entry.
        let file_path = args["file_path"].as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("file_path is required (relative to project root)"))?;
        let direction = args.get("direction")
            .and_then(|v| v.as_str())
            .unwrap_or("both");
        if !matches!(direction, "outgoing" | "incoming" | "both") {
            return Err(anyhow!(
                "direction must be one of: outgoing, incoming, both (got '{}')",
                direction
            ));
        }

        if !should_skip_indexing(args) {
            self.ensure_indexed()?;
            // Edit-aware: file_path is required for this tool — post-edit
            // staleness is the canonical failure mode here.
            self.ensure_file_fresh_opt(Some(file_path))?;
        }
        let depth = args.get("depth")
            .and_then(|v| v.as_i64())
            .unwrap_or(2)
            .clamp(1, 10) as i32;
        let compact = args["compact"].as_bool().unwrap_or(false);

        // Check if file exists in index
        let file_nodes = queries::get_nodes_by_file_path(self.db.conn(), file_path)?;
        if file_nodes.is_empty() {
            let hint = if file_path.ends_with('/') || !file_path.contains('.') {
                // Looks like a directory — suggest using module_overview instead
                let dir = if file_path.ends_with('/') { file_path.to_string() } else { format!("{}/", file_path) };
                format!(
                    "Path '{}' looks like a directory. Use module_overview(path=\"{}\") for directory-level analysis, or specify an exact file (e.g., '{}mod.rs')",
                    file_path, file_path, dir
                )
            } else {
                format!("File '{}' not found in index. Check path is relative to project root.", file_path)
            };
            return Ok(json!({
                "file": file_path,
                "depends_on": [],
                "depended_by": [],
                "warning": hint,
                "summary": format!("File '{}' not found in index", file_path)
            }));
        }

        let deps = queries::get_import_tree(self.db.conn(), file_path, direction, depth)?;

        // Filter out cross-language false edges (e.g. Rust file "calling" a JS function
        // due to name-based resolution matching common names like `update`, `read`, etc.)
        // Also drop the synthetic `<external>` bucket — it's a container for unresolved
        // imports, not a real file dependency.
        let is_compatible_lang =
            |dep_path: &str| crate::utils::config::is_compatible_lang(file_path, dep_path);

        let outgoing: Vec<serde_json::Value> = deps.iter()
            .filter(|d| d.direction == "outgoing")
            .filter(|d| is_compatible_lang(&d.file_path))
            .map(|d| {
                let mut obj = json!({
                    "file": d.file_path,
                    "depth": d.depth,
                });
                // Only show symbols for direct dependencies (depth 1);
                // deeper entries have 0 direct edges from root which is misleading
                // Skip symbols in compact mode to save tokens
                if !compact && d.depth == 1 {
                    obj["symbols"] = json!(d.symbol_count);
                }
                obj
            })
            .collect();

        let incoming: Vec<serde_json::Value> = deps.iter()
            .filter(|d| d.direction == "incoming")
            .filter(|d| is_compatible_lang(&d.file_path))
            .map(|d| {
                let mut obj = json!({
                    "file": d.file_path,
                    "depth": d.depth,
                });
                if !compact && d.depth == 1 {
                    obj["symbols"] = json!(d.symbol_count);
                }
                obj
            })
            .collect();

        Ok(json!({
            "file": file_path,
            "depends_on": outgoing,
            "depended_by": incoming,
            "summary": format!("{} depends on {} file{}, {} file{} depend{} on it",
                file_path,
                outgoing.len(), if outgoing.len() == 1 { "" } else { "s" },
                incoming.len(), if incoming.len() == 1 { "" } else { "s" },
                if incoming.len() == 1 { "s" } else { "" })
        }))
    }

    pub(in crate::mcp::server) fn tool_find_similar_code(&self, args: &serde_json::Value) -> Result<serde_json::Value> {
        self.try_lazy_load_model();
        if !should_skip_indexing(args) {
            self.ensure_indexed()?;
        }

        // Accept node_id directly, or resolve from symbol_name. Treat empty
        // symbol_name as absent — without this, the error message echoes
        // "Symbol '' not found" which looks like a real lookup miss.
        let node_id = if let Some(id) = args["node_id"].as_i64() {
            id
        } else if let Some(name) = args["symbol_name"].as_str().filter(|s| !s.trim().is_empty()) {
            match queries::get_first_node_id_by_name(self.db.conn(), name)? {
                Some(id) => id,
                None => return Err(anyhow!("Symbol '{}' not found in index. Use semantic_code_search to find the correct symbol name, or check spelling.", name)),
            }
        } else {
            return Err(anyhow!("Either node_id or symbol_name is required. Provide symbol_name (e.g. \"my_function\") or node_id (from other tool results)."));
        };
        let top_k = args.get("top_k")
            .and_then(|v| v.as_i64())
            .unwrap_or(5)
            .clamp(1, 100);
        let max_distance = args.get("max_distance")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.8);

        // Check if embeddings are available
        if !self.db.vec_enabled() {
            return Err(anyhow!("Embedding not available. Build with --features embed-model."));
        }

        // Check if any embeddings exist at all
        let (embedded_count, total_nodes) = queries::count_nodes_with_vectors(self.db.conn())?;
        if embedded_count == 0 {
            return Err(anyhow!("No embeddings found ({} nodes indexed, 0 embedded). The embedding model may not be loaded — restart the MCP server with the embed-model feature enabled. Alternative: use semantic_code_search with a descriptive query to find similar code by text matching.", total_nodes));
        }

        // Get the node's embedding
        let embedding: Vec<f32> = {
            let bytes = queries::get_node_embedding(self.db.conn(), node_id)
                .map_err(|_| anyhow!("No embedding found for node_id {}. Node may not have been embedded yet ({}/{} nodes embedded).", node_id, embedded_count, total_nodes))?;
            bytemuck::cast_slice(&bytes).to_vec()
        };

        // Search for similar vectors. Fetch extra so max_distance filtering
        // doesn't silently starve `top_k` — we need enough candidates to know
        // whether the cutoff actually dropped results. Shared over-fetch policy with
        // the CLI `similar` twin (single source of truth, avoids CLI↔MCP drift).
        let fetch_count = crate::domain::similar_fetch_count(top_k);
        let results = queries::vector_search(self.db.conn(), &embedding, fetch_count)?;

        // Split raw (self excluded) from cutoff-filtered candidates so we can
        // report whether max_distance is hiding matches.
        let raw_non_self: Vec<(i64, f64)> = results.iter()
            .filter(|(id, _)| *id != node_id)
            .map(|(id, dist)| (*id, *dist))
            .collect();
        let candidates: Vec<(i64, f64)> = raw_non_self.iter()
            .filter(|(_, dist)| *dist <= max_distance)
            .copied()
            .collect();
        let cutoff_dropped = raw_non_self.len() - candidates.len();
        let candidate_ids: Vec<i64> = candidates.iter().map(|(id, _)| *id).collect();
        let nodes_with_files = queries::get_nodes_with_files_by_ids(self.db.conn(), &candidate_ids)?;
        let node_map: std::collections::HashMap<i64, &queries::NodeWithFile> =
            nodes_with_files.iter().map(|nf| (nf.node.id, nf)).collect();

        let similar: Vec<serde_json::Value> = candidates.iter()
            .filter_map(|(id, distance)| {
                let nf = node_map.get(id)?;
                if crate::domain::is_skippable_result(&nf.node.node_type, &nf.node.name, &nf.file_path) {
                    return None;
                }
                let similarity = 1.0 / (1.0 + distance);
                Some(json!({
                    "node_id": nf.node.id,
                    "name": nf.node.name,
                    "type": nf.node.node_type,
                    "file_path": nf.file_path,
                    "start_line": nf.node.start_line,
                    "similarity": (similarity * 10000.0).round() / 10000.0,
                    "distance": (distance * 10000.0).round() / 10000.0,
                }))
            })
            .take(top_k as usize)
            .collect();

        let mut out = json!({
            "query_node_id": node_id,
            "results": similar,
            "count": similar.len(),
            "top_k": top_k,
            "max_distance": max_distance,
        });
        if (similar.len() as i64) < top_k && cutoff_dropped > 0 {
            out["cutoff_applied"] = json!(true);
            out["cutoff_dropped"] = json!(cutoff_dropped);
            out["hint"] = json!(format!(
                "Fewer results than top_k ({}): {} candidate(s) exceeded max_distance={}. Raise max_distance to widen the search.",
                top_k, cutoff_dropped, max_distance
            ));
        }
        Ok(out)
    }

    pub(in crate::mcp::server) fn tool_find_dead_code(&self, args: &serde_json::Value) -> Result<serde_json::Value> {
        let path = args["path"].as_str();
        let node_type = args["node_type"].as_str();
        // Validate node_type up-front: an unknown alias normalizes to an empty Vec
        // and find_dead_code falls through to a literal `n.type = :x` match that
        // returns zero rows — a false-clean empty result. Mirror tool_ast_search.
        if let Some(tf) = node_type {
            if crate::domain::normalize_type_filter(tf).is_empty() {
                return Err(anyhow!(
                    "Unknown type filter: '{}'. Valid: fn, class, struct, enum, trait, type, const, var",
                    tf
                ));
            }
        }
        let include_tests = args["include_tests"].as_bool().unwrap_or(false);
        let min_lines = args["min_lines"].as_u64().unwrap_or(3) as u32;
        let compact = args["compact"].as_bool().unwrap_or(true);

        // ignore_paths: prefix-match exclusions. When omitted, apply defaults for
        // shell-invoked entry points (plugin hooks / lifecycle scripts) that the
        // static AST call graph can't track. Pass an empty array to disable.
        let (ignore_prefixes, ignore_was_defaulted) = match args.get("ignore_paths") {
            Some(serde_json::Value::Array(arr)) => (
                arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>(),
                false,
            ),
            _ => (default_dead_code_ignores(), true),
        };

        if !should_skip_indexing(args) {
            self.ensure_indexed()?;
        }

        let raw_results = queries::find_dead_code(
            self.db.conn(),
            path,
            node_type,
            include_tests,
            min_lines,
            200,
        )?;
        let pre_filter_count = raw_results.len();
        let results: Vec<_> = raw_results.into_iter()
            .filter(|r| !ignore_prefixes.iter().any(|p| r.file_path.starts_with(p)))
            .collect();
        let ignored_count = pre_filter_count - results.len();

        if results.is_empty() {
            let mut summary = "No dead code found with the given filters.".to_string();
            if ignored_count > 0 {
                summary.push_str(&format!(
                    " ({} result(s) suppressed by ignore_paths; pass ignore_paths:[] to see them.)",
                    ignored_count
                ));
            }
            return Ok(json!({
                "results": [],
                "orphan_count": 0,
                "exported_unused_count": 0,
                "ignored_count": ignored_count,
                "ignore_paths_applied": ignore_prefixes,
                "ignore_paths_defaulted": ignore_was_defaulted,
                "summary": summary,
            }));
        }

        // Classify into orphans and exported-unused
        let mut orphan_items: Vec<serde_json::Value> = Vec::new();
        let mut exported_items: Vec<serde_json::Value> = Vec::new();

        for r in &results {
            let is_exported = r.has_export_edge
                || r.code_content.starts_with("pub ")
                || r.code_content.starts_with("pub(")
                || (r.file_path.ends_with(".go")
                    && r.name.chars().next().is_some_and(|c| c.is_uppercase()));
            let lines = r.end_line - r.start_line + 1;
            let mut item = json!({
                "name": r.name,
                "type": r.node_type,
                "file_path": r.file_path,
                "start_line": r.start_line,
                "end_line": r.end_line,
                "lines": lines,
                "category": if is_exported { "exported_unused" } else { "orphan" },
            });
            if !compact {
                item["code"] = json!(r.code_content);
            }
            if is_exported {
                exported_items.push(item);
            } else {
                orphan_items.push(item);
            }
        }

        let mut all_items = orphan_items.clone();
        all_items.extend(exported_items.iter().cloned());

        Ok(json!({
            "results": all_items,
            "orphan_count": orphan_items.len(),
            "exported_unused_count": exported_items.len(),
            "ignored_count": ignored_count,
            "ignore_paths_applied": ignore_prefixes,
            "ignore_paths_defaulted": ignore_was_defaulted,
            // "candidates" not "results": receiver-method calls and cross-file
            // const/type uses are not edge-tracked, so a flagged symbol may still
            // be used — the caller should verify before treating it as dead.
            "summary": if ignored_count > 0 {
                format!("Dead code: {} candidates ({} orphan, {} exported-unused); {} suppressed by ignore_paths (pass ignore_paths:[] to see them). Verify — receiver-method/cross-file uses aren't edge-tracked.",
                    all_items.len(), orphan_items.len(), exported_items.len(), ignored_count)
            } else {
                format!("Dead code: {} candidates ({} orphan, {} exported-unused). Verify — receiver-method/cross-file uses aren't edge-tracked.",
                    all_items.len(), orphan_items.len(), exported_items.len())
            },
        }))
    }
}
