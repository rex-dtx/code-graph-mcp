//! Single-pass batched indexer. Phases share local state (transaction,
//! atomics, batch_parsed, name_to_ids, global_name_map) so the function
//! itself stays whole — the *helpers* that feed it (context, embedding,
//! Python module map, ambiguity refinement, pending-call sweep) live in
//! sibling modules.
//!
//! Phase outline:
//! - 0: delete files; pre-cascade-buffer inbound calls into pending so
//!   B → A.foo doesn't silently vanish when only A is in `delete_paths`.
//! - 1a: parallel CPU work (read + parse + extract nodes) via rayon.
//! - 1b: sequential DB inserts (file row, node rows; cascades old nodes).
//! - 2: extract relations, resolve to edges with same-file → same-language
//!   → drop/global tier order; buffer unresolved bare-name same-language
//!   calls into pending instead of dropping; track external imports/symbols.
//! - 2b / 2b-ext: virtual `<external>` nodes for unresolved imports/traits.
//! - 2c: restore cross-file inbound edges that cascade-delete just stripped.
//! - 3: build context strings (parallel), batch-update, then embed outside tx.
//! - 2c sweep: drain `pending_unresolved_calls` against the new node state.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use rayon::prelude::*;

use crate::domain::{
    is_cross_file_call_noise, max_file_size, REL_CALLS, REL_IMPLEMENTS, REL_IMPORTS, REL_INHERITS,
    REL_REFERENCES, REL_ROUTES_TO,
};
use crate::embedding::context::{build_context_string, NodeContext};
use crate::embedding::model::EmbeddingModel;
use crate::indexer::merkle::hash_file;
use crate::parser::relations::extract_relations_from_tree;
use crate::parser::treesitter::{extract_nodes_from_tree, parse_tree};
use crate::search::tokenizer::split_identifier;
use crate::storage::db::Database;
use crate::storage::queries::{
    delete_files_by_paths, delete_nodes_by_file, get_all_node_names_with_ids, get_edges_batch,
    get_inbound_cross_file_edges, get_nodes_by_file_path, get_nodes_with_files_by_ids,
    insert_edge_cached, insert_node_cached, update_context_strings_batch, upsert_file, FileRecord,
    NodeRecord, NodeResult,
};
use crate::utils::config::detect_language;

use super::context::{categorize_edges, format_route_from_metadata};
use super::embed::embed_and_store_batch;
use super::js_modules::{
    resolve_c_include_path, resolve_js_module_targets, resolve_js_specifier_path,
    resolve_php_include_path,
};
use super::python_modules::{build_python_module_map, resolve_python_module_targets};
use super::resolve::{
    bind_calls_to_imported_targets, classify_edge_confidence, prune_import_contradicted_call_edges,
    refine_ambiguous_targets, resolve_pending_calls,
};
use super::{IndexPhase, IndexResult, IndexStats, ProgressFn};

/// Heuristic: does a `.h` header contain C++-specific constructs? `.h` is C-vs-C++
/// ambiguous by extension (detect_language maps it to C), and the C grammar cannot
/// extract `class`/`namespace` symbols — so a C++ class in a `.h` header is silently
/// dropped. When any of these markers is present the header is parsed as C++ instead.
/// The markers are C++-only (`::`, access specifiers, `class`/`namespace`/`template`)
/// so a pure-C header stays C; a false positive is low-harm because the C++ grammar
/// is a near-superset of C and still extracts C functions/structs/#includes.
fn looks_like_cpp_header(source: &str) -> bool {
    source.contains("::")
        || source.contains("public:")
        || source.contains("private:")
        || source.contains("protected:")
        || source.contains("class ")
        || source.contains("namespace ")
        || source.contains("template<")
        || source.contains("template <")
}

/// Batch size for streaming indexing. Each batch processes Phase 1+2
/// then drops heavyweight data (ASTs, source strings) before the next batch.
const BATCH_SIZE: usize = 500;

/// Lightweight post-batch record — no Tree or source string.
pub(super) struct FileIndexed {
    pub rel_path: String,
    pub node_ids: Vec<i64>,
    pub node_names: Vec<String>,
}

pub(super) fn index_files(
    db: &Database,
    root: &Path,
    files: &[String],
    hashes: &HashMap<String, String>,
    model: Option<&EmbeddingModel>,
    delete_paths: &[String],
    progress: Option<ProgressFn>,
) -> Result<IndexResult> {
    // Phase transactions use `db.savepoint(...)`, NOT `conn().unchecked_transaction()`,
    // so this pipeline is atomic whether run standalone (CLI / incremental — a
    // top-level SAVEPOINT auto-starts a transaction, RELEASE commits it) OR nested
    // inside an enclosing transaction. The MCP `rebuild_index` tool wraps the whole
    // DELETE-then-reindex in one outer transaction so external fresh-connection
    // readers never observe the empty/partial mid-rebuild window and a failed rebuild
    // rolls back to the old index; `unchecked_transaction` can't be used there because
    // it always issues BEGIN, which errors inside an already-open transaction.
    //
    // Safety of the shared `&Connection` (savepoint borrows &Connection, we still read
    // via db.conn() on the same handle): (1) db.conn() and the savepoint act on the
    // same Connection; (2) concurrent access (e.g. background embedding thread) uses
    // separate DB connections — safety relies on SQLite WAL mode + busy_timeout(5000),
    // not single-threadedness.

    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    let skipped_size = AtomicUsize::new(0);
    let skipped_parse = AtomicUsize::new(0);
    let skipped_read = AtomicUsize::new(0);
    let skipped_hash = AtomicUsize::new(0);
    let skipped_language = AtomicUsize::new(0);
    // Files that parsed into a tree carrying tree-sitter ERROR node(s). Distinct
    // from skipped_parse (a hard parse failure that drops the file entirely):
    // tree-sitter's error recovery still returns a tree, so extraction proceeds
    // best-effort but a grammar error cascade can silently drop symbols. Counting
    // this makes that partial-extraction risk observable without any schema change.
    let parse_error_files = AtomicUsize::new(0);

    let mut total_nodes_created = 0usize;
    let mut total_edges_created = 0usize;
    let mut all_indexed: Vec<FileIndexed> = Vec::new();

    // Phase 0: Delete removed files in own transaction.
    //
    // Before cascade strips inbound REL_CALLS edges, capture them as pending
    // rows. Without this, deleting file A wipes B's edge to A.foo and B is
    // not in `delete_paths` (so Phase 2 won't re-extract it), leaving B with
    // neither an edge nor a pending row — the same staleness window the
    // "callee added later" buffering closes, just from the deletion side.
    // Both directions need to round-trip through pending or the v0.18.2 fix
    // is only half-complete.
    if !delete_paths.is_empty() {
        let tx = db.savepoint("idx_delete")?;

        // Resolve file IDs once (delete_files_by_paths drops them) so we can
        // query inbound calls before cascade fires.
        let mut deleted_file_ids: Vec<i64> = Vec::with_capacity(delete_paths.len());
        for path in delete_paths {
            if let Ok(Some(fid)) =
                db.conn()
                    .query_row("SELECT id FROM files WHERE path = ?1", [path], |row| {
                        row.get::<_, Option<i64>>(0)
                    })
            {
                deleted_file_ids.push(fid);
            }
        }

        let mut buffered = 0usize;
        for fid in &deleted_file_ids {
            let inbound = crate::storage::queries::get_inbound_calls_for_pending(db.conn(), *fid)?;
            for (source_id, target_name, source_language, metadata) in inbound {
                crate::storage::queries::insert_pending_unresolved_call(
                    db.conn(),
                    source_id,
                    &target_name,
                    &source_language,
                    metadata.as_deref(),
                )?;
                buffered += 1;
            }
        }
        if buffered > 0 {
            tracing::info!(
                "[index] Phase 0: buffered {} inbound calls before cascade-deleting {} file(s)",
                buffered,
                deleted_file_ids.len()
            );
        }

        delete_files_by_paths(db.conn(), delete_paths)?;
        tx.commit()?;
    }

    // CPU-bound parse result — produced in parallel, consumed sequentially for DB insert
    struct FilePreParsed {
        rel_path: String,
        source: String,
        language: String,
        tree: tree_sitter::Tree,
        hash: String,
        last_modified: i64,
        parsed_nodes: Vec<crate::parser::treesitter::ParsedNode>,
    }

    // Pre-build Python module map once (used in all batches for import resolution)
    let mut all_python_paths: HashSet<String> = files
        .iter()
        .filter(|f| f.ends_with(".py"))
        .cloned()
        .collect();
    {
        let mut stmt = db
            .conn()
            .prepare("SELECT path FROM files WHERE path LIKE '%.py'")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            all_python_paths.insert(row?);
        }
    }
    let python_module_map = build_python_module_map(&all_python_paths);

    // All indexed file paths (this run's `files` plus everything already in the
    // DB), used to resolve JS/TS relative import specifiers to a concrete file.
    // Includes pseudo-files like `<external>`; the resolver only matches real
    // relative paths so they never collide.
    let mut all_file_paths: HashSet<String> = files.iter().cloned().collect();
    {
        let mut stmt = db.conn().prepare("SELECT path FROM files")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            all_file_paths.insert(row?);
        }
    }

    // Pre-load global name->[(id, path, language)] map once before the batch loop.
    // This avoids a full table scan per batch in Phase 2 relation resolution.
    // The map is updated incrementally as each batch commits new nodes.
    // `language` drives same-language-preferred resolution to avoid cross-language
    // bare-name collisions (e.g. Rust `hasher.update()` resolving to JS `function update`).
    let mut global_name_map: HashMap<String, Vec<crate::storage::queries::NameEntry>> =
        get_all_node_names_with_ids(db.conn())?;

    // Heavyweight per-file data used during Phase 1+2, dropped after each batch
    #[allow(dead_code)]
    struct FileParsed {
        rel_path: String,
        source: String,
        language: String,
        tree: tree_sitter::Tree,
        file_id: i64,
        node_ids: Vec<i64>,
        node_names: Vec<String>,
        // Qualified names parallel to node_ids/node_names (None for <module>).
        // Needed so Phase-2 source resolution can match a relation's
        // qualified scope_name (`Class.method`) against class-based-language
        // method nodes, whose bare `name` is just `method`.
        node_qualified_names: Vec<Option<String>>,
        // Node types parallel to node_ids/node_names. Needed so inherits/implements
        // source resolution can reject a same-named function/method (a C++ inline
        // constructor shares its class's name) — only a type node can be a supertype.
        node_types: Vec<String>,
    }

    // Process files in batches — each batch does Phase 1 + Phase 2
    for batch in files.chunks(BATCH_SIZE) {
        let tx = db.savepoint("idx_batch")?;

        // --- Phase 1a: Parallel CPU-bound work (read + parse + extract nodes) ---
        let pre_parsed: Vec<FilePreParsed> = batch
            .par_iter()
            .filter_map(|rel_path| {
                let mut language = match detect_language(rel_path) {
                    Some(l) => l,
                    None => {
                        skipped_language.fetch_add(1, AtomicOrdering::Relaxed);
                        return None;
                    }
                };
                let abs_path = root.join(rel_path);

                let file_meta = std::fs::metadata(&abs_path).ok();
                if let Some(ref meta) = file_meta {
                    if meta.len() > max_file_size() {
                        tracing::debug!("Skipping large file ({} bytes): {}", meta.len(), rel_path);
                        skipped_size.fetch_add(1, AtomicOrdering::Relaxed);
                        return None;
                    }
                }

                let source = match std::fs::read_to_string(&abs_path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("Skipping file {}: {}", rel_path, e);
                        skipped_read.fetch_add(1, AtomicOrdering::Relaxed);
                        return None;
                    }
                };

                // `.h` is C-vs-C++ ambiguous by extension, so detect_language maps it
                // to C. But the C grammar can't parse `class`/`namespace`, so C++ classes
                // declared in a `.h` header (the MOST common C++ layout) — and their
                // base-class `inherits` edges — were silently dropped. When the header's
                // content actually contains C++ constructs, parse it as C++ so those
                // symbols are captured. Gated on markers so a pure-C header stays C;
                // false positives are low-harm (the C++ grammar is a near-superset of C).
                if language == "c" && rel_path.ends_with(".h") && looks_like_cpp_header(&source) {
                    language = "cpp";
                }

                let hash = match hashes.get(rel_path.as_str()) {
                    Some(h) => h.clone(),
                    None => match hash_file(&abs_path) {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::warn!("Skipping file (hash error): {}: {}", rel_path, e);
                            skipped_hash.fetch_add(1, AtomicOrdering::Relaxed);
                            return None;
                        }
                    },
                };

                let tree = match parse_tree(&source, language) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("Parse failed for {}: {}", rel_path, e);
                        skipped_parse.fetch_add(1, AtomicOrdering::Relaxed);
                        return None;
                    }
                };

                // Tree-sitter recovers from syntax errors by inserting ERROR/MISSING
                // nodes and still returning a tree, so parse "succeeds" but symbol
                // extraction below runs over a damaged parse and can silently drop
                // symbols. Surface it: warn once per file and count the pass total.
                if tree.root_node().has_error() {
                    tracing::warn!(
                        "Syntax errors in {} — symbols may be incomplete (parsed with tree-sitter error recovery)",
                        rel_path
                    );
                    parse_error_files.fetch_add(1, AtomicOrdering::Relaxed);
                }

                let last_modified = file_meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                let parsed_nodes = extract_nodes_from_tree(&tree, &source, language);

                Some(FilePreParsed {
                    rel_path: rel_path.clone(),
                    source,
                    language: language.to_string(),
                    tree,
                    hash,
                    last_modified,
                    parsed_nodes,
                })
            })
            .collect();

        let mut batch_parsed: Vec<FileParsed> = Vec::new();
        // Saved inbound edges from other files → batch files (to restore after cascade delete)
        // Tuple: (source_id, source_file_id, target_file_id, target_name, relation, metadata).
        // target_file_id is the re-indexed file the edge pointed INTO; the restore
        // re-binds ONLY to the new same-name node in THAT file, not every same-name
        // node in the batch (which fanned out cross-file / cross-language).
        #[allow(clippy::type_complexity)]
        let mut saved_inbound_edges: Vec<(i64, i64, i64, String, String, Option<String>)> =
            Vec::new();
        // Track file_ids in this batch to filter intra-batch edges in Phase 2c
        let mut batch_file_ids: HashSet<i64> = HashSet::new();

        // --- Phase 1b: Sequential DB inserts ---
        for pp in pre_parsed {
            let file_id = upsert_file(
                db.conn(),
                &FileRecord {
                    path: pp.rel_path.clone(),
                    blake3_hash: pp.hash,
                    last_modified: pp.last_modified,
                    language: Some(pp.language.clone()),
                },
            )?;

            // Save cross-file inbound edges before cascade delete destroys them.
            // file_id IS the target file these edges point into — attach it so the
            // Phase 2c restore can re-bind to the same-name node in THIS file only.
            saved_inbound_edges.extend(
                get_inbound_cross_file_edges(db.conn(), file_id)?
                    .into_iter()
                    .map(|(src, src_file, tname, rel, meta)| {
                        (src, src_file, file_id, tname, rel, meta)
                    }),
            );
            batch_file_ids.insert(file_id);

            delete_nodes_by_file(db.conn(), file_id)?;

            let mut node_ids = Vec::new();
            let mut node_names = Vec::new();
            let mut node_qualified_names: Vec<Option<String>> = Vec::new();
            let mut node_types: Vec<String> = Vec::new();

            let module_node_id = insert_node_cached(
                db.conn(),
                &NodeRecord {
                    file_id,
                    node_type: "module".into(),
                    name: "<module>".into(),
                    qualified_name: Some(pp.rel_path.clone()),
                    start_line: 1,
                    end_line: pp.source.lines().count() as i64,
                    code_content: String::new(),
                    signature: None,
                    doc_comment: None,
                    context_string: None,
                    name_tokens: None,
                    return_type: None,
                    param_types: None,
                    is_test: false,
                },
            )?;
            node_ids.push(module_node_id);
            node_names.push("<module>".into());
            // <module> resolves by its bare name; no qualified form.
            node_qualified_names.push(None);
            node_types.push("module".into());
            total_nodes_created += 1;

            for pn in &pp.parsed_nodes {
                let name_tokens = split_identifier(&pn.name);
                let node_id = insert_node_cached(
                    db.conn(),
                    &NodeRecord {
                        file_id,
                        node_type: pn.node_type.clone(),
                        name: pn.name.clone(),
                        qualified_name: pn.qualified_name.clone(),
                        start_line: pn.start_line as i64,
                        end_line: pn.end_line as i64,
                        code_content: pn.code_content.clone(),
                        signature: pn.signature.clone(),
                        doc_comment: pn.doc_comment.clone(),
                        context_string: None,
                        name_tokens: Some(name_tokens),
                        return_type: pn.return_type.clone(),
                        param_types: pn.param_types.clone(),
                        is_test: pn.is_test,
                    },
                )?;
                node_ids.push(node_id);
                node_names.push(pn.name.clone());
                node_qualified_names.push(pn.qualified_name.clone());
                node_types.push(pn.node_type.clone());
                total_nodes_created += 1;
            }

            batch_parsed.push(FileParsed {
                rel_path: pp.rel_path,
                source: pp.source,
                language: pp.language,
                tree: pp.tree,
                file_id,
                node_ids,
                node_names,
                node_qualified_names,
                node_types,
            });
        }

        // --- Phase 2: Extract relations + insert edges ---
        // Build per-batch name_to_ids and node_id_to_path from the pre-loaded global map,
        // excluding files in the current batch (their old nodes were deleted in Phase 1b).
        let batch_file_paths: HashSet<&str> =
            batch_parsed.iter().map(|pf| pf.rel_path.as_str()).collect();

        let mut name_to_ids: HashMap<String, Vec<i64>> = HashMap::new();
        let mut node_id_to_path: HashMap<i64, String> = HashMap::new();
        // Per-node language for same-language-preferred edge resolution (§ cross-lang collision).
        let mut node_id_to_language: HashMap<i64, Option<String>> = HashMap::new();

        // Add current batch's newly inserted nodes
        for pf in &batch_parsed {
            for (id, name) in pf.node_ids.iter().zip(pf.node_names.iter()) {
                name_to_ids.entry(name.clone()).or_default().push(*id);
                node_id_to_path.insert(*id, pf.rel_path.clone());
                node_id_to_language.insert(*id, Some(pf.language.clone()));
            }
        }

        // Add nodes from the global map, excluding those in current batch's files
        // (their old nodes were deleted and replaced by new ones above)
        for (name, entries) in &global_name_map {
            for (id, path, language) in entries {
                if !batch_file_paths.contains(path.as_str()) {
                    name_to_ids.entry(name.clone()).or_default().push(*id);
                    node_id_to_path.insert(*id, path.clone());
                    node_id_to_language.insert(*id, language.clone());
                }
            }
        }

        for ids in name_to_ids.values_mut() {
            ids.sort();
            ids.dedup();
        }

        // Track unresolved external Python imports: (source_module_node_id, module_name)
        let mut external_python_imports: Vec<(i64, String)> = Vec::new();
        // Track unresolved external symbols for sentinel node creation:
        // (source_id, target_name, relation) — e.g., implements edges to external traits
        let mut unresolved_externals: Vec<(i64, String, String)> = Vec::new();

        for pf in &batch_parsed {
            let relations = extract_relations_from_tree(&pf.tree, &pf.source, &pf.language);
            let local_ids: HashSet<i64> = pf.node_ids.iter().copied().collect();

            // Pre-scan this file's require-namespace bindings
            // (`const m = require('./x')`, stamped `{"q":"ns_require",...}`) →
            // resolved file path, so `m.foo()` member calls (CalleeMeta::Receiver)
            // bind to the required module in the call-resolution pass below.
            let mut ns_module_map: HashMap<String, String> = HashMap::new();
            for rel in &relations {
                if rel.relation != REL_IMPORTS {
                    continue;
                }
                if let Some(meta_str) = rel.metadata.as_deref() {
                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_str) {
                        // ESM `import * as ns` (q:"ns_import", v51) binds member
                        // calls exactly like the CJS require-namespace form.
                        if matches!(
                            meta.get("q").and_then(|v| v.as_str()),
                            Some("ns_require") | Some("ns_import")
                        ) {
                            if let Some(spec) = meta.get("js_module").and_then(|v| v.as_str()) {
                                if let Some(file) =
                                    resolve_js_specifier_path(spec, &pf.rel_path, &all_file_paths)
                                {
                                    ns_module_map.insert(rel.target_name.clone(), file);
                                }
                            }
                        }
                    }
                }
            }

            for rel in &relations {
                // Contract: extract_relations_from_tree stamps every relation with
                // source_language equal to the language argument. The
                // same-language resolution at line 811+ depends on it. Hard
                // error instead of debug_assert so a parser regression fails
                // loudly in release builds too (one string compare per
                // relation is negligible against the SQL writes below).
                if rel.source_language != pf.language {
                    anyhow::bail!(
                        "ParsedRelation.source_language ({}) does not match file language ({}); \
                         parser regressed the source_language contract",
                        rel.source_language,
                        pf.language
                    );
                }

                // Match the relation's enclosing scope (source_name) to a node.
                // Class-based languages (Python/TS/JS/Java/Ruby) qualify a
                // method's scope as `Class.method`, but the node's bare `name`
                // is just `method` — so match qualified_name too, else every
                // intra-class method-to-method edge is silently dropped.
                // Bare-scope sources (Rust impl, Go receivers, free functions)
                // still match on `name`.
                // inherits/implements describe a TYPE's supertype, so their source
                // must be a class/struct/interface/enum/trait — never a function or
                // method that merely shares the type's name. A C++ inline constructor
                // (`Widget(int){}`) produces a `method Widget` node alongside `class
                // Widget`; without this both matched `source_name == "Widget"` and the
                // constructor got a bogus `inherits` edge. Blacklist fn/method (rather
                // than whitelist type kinds) so no language's type node is missed.
                let type_source_only =
                    rel.relation == REL_INHERITS || rel.relation == REL_IMPLEMENTS;
                let mut source_ids = (0..pf.node_ids.len())
                    .filter(|&i| {
                        (pf.node_names[i] == rel.source_name
                            || pf.node_qualified_names[i].as_deref()
                                == Some(rel.source_name.as_str()))
                            && (!type_source_only
                                || !matches!(pf.node_types[i].as_str(), "function" | "method"))
                    })
                    .map(|i| pf.node_ids[i])
                    .collect::<Vec<_>>();

                // Route handlers are commonly imported from a controller file —
                // the canonical Express layout `import { getUser } from './ctrl';
                // app.get('/x', getUser)`. The routes_to relation names the handler
                // (== source == target), but the handler node lives in another
                // file, so the same-file scan above finds nothing and the route
                // edge (the handler self-edge carrying method/path) is silently
                // dropped — trace/impact/find_http_route then see no route at all.
                // Recover by resolving the handler name cross-file, same-language,
                // exactly like a call target below (refine breaks any ambiguity by
                // path locality). Only fires for routes_to with an unresolved
                // same-file source; inline + same-file named handlers already match.
                if rel.relation == REL_ROUTES_TO && source_ids.is_empty() {
                    let same_lang: Vec<i64> = name_to_ids
                        .get(&rel.source_name)
                        .map(|ids| {
                            ids.iter()
                                .copied()
                                .filter(|id| {
                                    matches!(
                                        node_id_to_language.get(id).and_then(|l| l.as_deref()),
                                        Some(l) if l == pf.language.as_str()
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    source_ids =
                        refine_ambiguous_targets(&same_lang, &pf.rel_path, &node_id_to_path);
                }

                // Module-level import markers (v51, roadmap §2.3): namespace
                // bindings (`const m = require('./x')` q:"ns_require", `import *
                // as ns from './x'` q:"ns_import") and star re-exports (`export *
                // from './x'` q:"star_reexport") name no resolvable symbol, so
                // default name resolution would mint a spurious `<external>` node
                // (or, for star's `<module>` target, cross-link a random file).
                // Instead bind them to the RESOLVED file's `<module>` node — the
                // PHP-include/C-include pattern — so a namespace-only or
                // star-barrel dependency is finally visible to deps/affected/
                // cycles/map. Unresolvable specifier (external package) → no
                // edge, same as before. Always `continue`: never fall through.
                if rel.relation == REL_IMPORTS {
                    if let Some(meta_str) = rel.metadata.as_deref() {
                        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_str) {
                            if matches!(
                                meta.get("q").and_then(|v| v.as_str()),
                                Some("ns_require") | Some("ns_import") | Some("star_reexport")
                            ) {
                                if let Some(spec) = meta.get("js_module").and_then(|v| v.as_str()) {
                                    if let Some(file) = resolve_js_specifier_path(
                                        spec,
                                        &pf.rel_path,
                                        &all_file_paths,
                                    ) {
                                        let module_targets: Vec<i64> = name_to_ids
                                            .get("<module>")
                                            .map(|ids| {
                                                ids.iter()
                                                    .copied()
                                                    .filter(|id| {
                                                        node_id_to_path
                                                            .get(id)
                                                            .map(|p| p == &file)
                                                            .unwrap_or(false)
                                                    })
                                                    .collect()
                                            })
                                            .unwrap_or_default();
                                        for &src_id in &source_ids {
                                            for &tgt_id in &module_targets {
                                                if src_id != tgt_id
                                                    && insert_edge_cached(
                                                        db.conn(),
                                                        src_id,
                                                        tgt_id,
                                                        &rel.relation,
                                                        rel.metadata.as_deref(),
                                                    )?
                                                {
                                                    total_edges_created += 1;
                                                }
                                            }
                                        }
                                    }
                                }
                                continue;
                            }
                        }
                    }
                }

                // Try Python module-constrained resolution for import edges
                if rel.relation == REL_IMPORTS {
                    if let Some(ref meta_str) = rel.metadata {
                        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_str) {
                            if let Some(python_module) =
                                meta.get("python_module").and_then(|v| v.as_str())
                            {
                                let is_module_import = meta
                                    .get("is_module_import")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                if python_module_map.contains_key(python_module) {
                                    // Internal module — try constrained resolution
                                    if let Some(module_targets) = resolve_python_module_targets(
                                        python_module,
                                        is_module_import,
                                        &rel.target_name,
                                        &python_module_map,
                                        &node_id_to_path,
                                        &name_to_ids,
                                    ) {
                                        for &src_id in &source_ids {
                                            for &tgt_id in &module_targets {
                                                if src_id != tgt_id
                                                    && insert_edge_cached(
                                                        db.conn(),
                                                        src_id,
                                                        tgt_id,
                                                        &rel.relation,
                                                        rel.metadata.as_deref(),
                                                    )?
                                                {
                                                    total_edges_created += 1;
                                                }
                                            }
                                        }
                                        continue;
                                    }
                                    // Module found but symbol not found — fall through to default
                                } else {
                                    // External module — track for virtual node creation.
                                    // For `from X import Y`, we track the module-level dependency (X),
                                    // not the individual symbol (Y), since we can't index external code.
                                    for &src_id in &source_ids {
                                        external_python_imports
                                            .push((src_id, python_module.to_string()));
                                    }
                                    continue; // No point in default resolution for external imports
                                }
                            }
                        }
                    }
                }

                // Try JS/TS relative-specifier resolution for import edges. The
                // parser stamps `{"js_module":"<specifier>"}` (imports.rs);
                // resolve the specifier against the importer's path + extension
                // probing to a concrete file so the import binds there instead
                // of a path-proximity same-name guess. Combined with Phase
                // 2d-bind, this also repoints the matching bare calls. Bare/
                // external/unindexed specifiers return None → fall through to
                // default name-based / `<external>` resolution (unchanged).
                if rel.relation == REL_IMPORTS {
                    if let Some(ref meta_str) = rel.metadata {
                        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_str) {
                            if let Some(js_module) = meta.get("js_module").and_then(|v| v.as_str())
                            {
                                if let Some(targets) = resolve_js_module_targets(
                                    js_module,
                                    &pf.rel_path,
                                    &rel.target_name,
                                    &all_file_paths,
                                    &name_to_ids,
                                    &node_id_to_path,
                                ) {
                                    for &src_id in &source_ids {
                                        for &tgt_id in &targets {
                                            if src_id != tgt_id
                                                && insert_edge_cached(
                                                    db.conn(),
                                                    src_id,
                                                    tgt_id,
                                                    &rel.relation,
                                                    rel.metadata.as_deref(),
                                                )?
                                            {
                                                total_edges_created += 1;
                                            }
                                        }
                                    }
                                    continue;
                                }
                                // Unresolved (bare pkg / re-export / unindexed) —
                                // fall through to default resolution below.
                            }
                        }
                    }
                }

                // PHP file includes: the parser stamps `{"php_include":"<path>"}`
                // on the import edge (require/require_once/include 'lib.php').
                // Resolve the path against the importer's directory + `.php`
                // probing to a concrete file, then bind to that file's <module>
                // node so deps/cycles/affected/project_map see the cross-file
                // include dependency. Unindexed/vendored paths return None →
                // fall through to default (`<external>`) resolution.
                if rel.relation == REL_IMPORTS {
                    if let Some(ref meta_str) = rel.metadata {
                        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_str) {
                            if let Some(inc) = meta.get("php_include").and_then(|v| v.as_str()) {
                                if let Some(file) =
                                    resolve_php_include_path(inc, &pf.rel_path, &all_file_paths)
                                {
                                    // Bind to the resolved file's <module> node.
                                    let module_targets: Vec<i64> = name_to_ids
                                        .get("<module>")
                                        .map(|ids| {
                                            ids.iter()
                                                .copied()
                                                .filter(|id| {
                                                    node_id_to_path
                                                        .get(id)
                                                        .map(|p| p == &file)
                                                        .unwrap_or(false)
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    if !module_targets.is_empty() {
                                        for &src_id in &source_ids {
                                            for &tgt_id in &module_targets {
                                                if src_id != tgt_id
                                                    && insert_edge_cached(
                                                        db.conn(),
                                                        src_id,
                                                        tgt_id,
                                                        &rel.relation,
                                                        rel.metadata.as_deref(),
                                                    )?
                                                {
                                                    total_edges_created += 1;
                                                }
                                            }
                                        }
                                        continue;
                                    }
                                }
                                // Unindexed include → fall through to default.
                            }
                        }
                    }
                }

                // C/C++ file includes: the parser stamps `{"c_include":"<path>"}`
                // on the import edge (`#include "widget.h"`). Resolve the path
                // against the importer's directory (and repo root) to a concrete
                // header, then bind to that file's <module> node so deps/cycles/
                // affected/project_map see the local header dependency. System
                // headers (`<stdio.h>`) / unindexed paths return None → fall
                // through to default (`<external>`) resolution (M6).
                if rel.relation == REL_IMPORTS {
                    if let Some(ref meta_str) = rel.metadata {
                        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_str) {
                            if let Some(inc) = meta.get("c_include").and_then(|v| v.as_str()) {
                                if let Some(file) =
                                    resolve_c_include_path(inc, &pf.rel_path, &all_file_paths)
                                {
                                    let module_targets: Vec<i64> = name_to_ids
                                        .get("<module>")
                                        .map(|ids| {
                                            ids.iter()
                                                .copied()
                                                .filter(|id| {
                                                    node_id_to_path
                                                        .get(id)
                                                        .map(|p| p == &file)
                                                        .unwrap_or(false)
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    if !module_targets.is_empty() {
                                        for &src_id in &source_ids {
                                            for &tgt_id in &module_targets {
                                                if src_id != tgt_id
                                                    && insert_edge_cached(
                                                        db.conn(),
                                                        src_id,
                                                        tgt_id,
                                                        &rel.relation,
                                                        rel.metadata.as_deref(),
                                                    )?
                                                {
                                                    total_edges_created += 1;
                                                }
                                            }
                                        }
                                        continue;
                                    }
                                }
                                // Unindexed include → fall through to default.
                            }
                        }
                    }
                }

                // Rust trait impl method-level edges: parser stamps
                // `{"q":"impl_method","v":"<TypeName>"}` so we can restrict
                // candidate target methods to those that actually belong to
                // this impl block (qualified_name LIKE "<TypeName>.%"). Without
                // this, N structs implementing the same trait in one file all
                // fan their method edges onto every same-name method node.
                if rel.relation == REL_IMPLEMENTS {
                    if let Some(ref meta_str) = rel.metadata {
                        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_str) {
                            if meta.get("q").and_then(|v| v.as_str()) == Some("impl_method") {
                                if let Some(impl_type) = meta.get("v").and_then(|v| v.as_str()) {
                                    use super::resolve::self_filter_candidates;
                                    let all = name_to_ids
                                        .get(&rel.target_name)
                                        .cloned()
                                        .unwrap_or_default();
                                    let filtered = self_filter_candidates(impl_type, &all, db)?;
                                    if filtered.is_empty() {
                                        // No project method belongs to this type — drop
                                        // (would be an external trait method anyway).
                                        continue;
                                    }
                                    for &src_id in &source_ids {
                                        for &tgt_id in &filtered {
                                            if src_id != tgt_id
                                                && insert_edge_cached(
                                                    db.conn(),
                                                    src_id,
                                                    tgt_id,
                                                    &rel.relation,
                                                    rel.metadata.as_deref(),
                                                )?
                                            {
                                                total_edges_created += 1;
                                            }
                                        }
                                    }
                                    continue;
                                }
                            }
                        }
                    }
                }

                // Bare-name call qualifier (Rust): inspect metadata to
                // skip / restrict candidate set before the existing fallback
                // chain. See spec
                // docs/superpowers/specs/2026-05-11-bare-name-call-qualifier-design.md.
                if rel.relation == REL_CALLS {
                    use super::resolve::{
                        method_candidates, parse_callee_metadata, path_filter_candidates,
                        self_filter_candidates, CalleeMeta,
                    };
                    match parse_callee_metadata(rel.metadata.as_deref()) {
                        Some(CalleeMeta::Receiver(recv))
                            if matches!(
                                pf.language.as_str(),
                                "javascript" | "typescript" | "tsx"
                            ) =>
                        {
                            // Cycle 4: `m.foo()` where `const m = require('./x')` —
                            // bind the method to the required module file. Only JS
                            // produces a Receiver here (extract_callee captures a
                            // simple-identifier receiver for the JS family). When recv
                            // is NOT a require-namespace binding (`arr.map()`,
                            // `res.send()`) or the method isn't in that file, fall
                            // through to the default resolution below — identical to
                            // the pre-Cycle-4 Bare path — by NOT continuing.
                            if let Some(module_file) = ns_module_map.get(&recv) {
                                let targets: Vec<i64> = name_to_ids
                                    .get(&rel.target_name)
                                    .map(|ids| {
                                        ids.iter()
                                            .copied()
                                            .filter(|id| {
                                                node_id_to_path
                                                    .get(id)
                                                    .map(|p| p == module_file)
                                                    .unwrap_or(false)
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                if !targets.is_empty() {
                                    for &src_id in &source_ids {
                                        for &tgt_id in &targets {
                                            if src_id != tgt_id
                                                && insert_edge_cached(
                                                    db.conn(),
                                                    src_id,
                                                    tgt_id,
                                                    &rel.relation,
                                                    rel.metadata.as_deref(),
                                                )?
                                            {
                                                total_edges_created += 1;
                                            }
                                        }
                                    }
                                    continue;
                                }
                            }
                            // Not a namespace binding → fall through to default.
                        }
                        Some(CalleeMeta::Chain) | Some(CalleeMeta::Receiver(_)) => {
                            // Receiver type is not statically inferable (`obj.method()`
                            // where `obj`'s type is unknown). The blanket drop here
                            // marked uniquely-named live methods (`file_exists`,
                            // `validate`) as dead and hid their callers from
                            // impact/callers. Recover ONLY the unambiguous case: a
                            // single same-language METHOD with that name, not a
                            // stdlib-noise name. A unique non-noise method cannot
                            // fan out across unrelated modules — the exact inflation
                            // the drop guarded against — so binding it is safe.
                            // Anything ambiguous (0 or >1 method candidates) or a
                            // noise name stays dropped (no buffer; re-scan won't help).
                            if is_cross_file_call_noise(&rel.target_name, pf.language.as_str()) {
                                continue;
                            }
                            let all = name_to_ids
                                .get(&rel.target_name)
                                .cloned()
                                .unwrap_or_default();
                            let same_lang: Vec<i64> = all
                                .iter()
                                .filter(|id| {
                                    matches!(
                                        node_id_to_language.get(id).and_then(|l| l.as_deref()),
                                        Some(l) if l == pf.language.as_str()
                                    )
                                })
                                .copied()
                                .collect();
                            // A receiver call can only target a method, never a
                            // same-named free function — filter those out first.
                            let methods = method_candidates(&same_lang, db)?;
                            // Prefer a same-file method if present (strongest
                            // locality signal); otherwise require a globally
                            // unique method.
                            let same_file_methods: Vec<i64> = methods
                                .iter()
                                .copied()
                                .filter(|id| local_ids.contains(id))
                                .collect();
                            let target = if same_file_methods.len() == 1 {
                                Some(same_file_methods[0])
                            } else if same_file_methods.is_empty() && methods.len() == 1 {
                                Some(methods[0])
                            } else {
                                None
                            };
                            match target {
                                Some(tgt_id) => {
                                    for &src_id in &source_ids {
                                        if src_id != tgt_id
                                            && insert_edge_cached(
                                                db.conn(),
                                                src_id,
                                                tgt_id,
                                                &rel.relation,
                                                rel.metadata.as_deref(),
                                            )?
                                        {
                                            total_edges_created += 1;
                                        }
                                    }
                                }
                                None => {
                                    // Ambiguous → drop without buffering (re-scan
                                    // won't change it). "Multiple" here covers BOTH
                                    // shapes: (a) >1 cross-file methods with no
                                    // same-file candidate, and (b) >1 same-file
                                    // methods (two structs in one file each defining
                                    // a method of this name) — the same-file>1 case
                                    // is intentionally ambiguous, not resolved, since
                                    // the receiver's type still can't pick between them.
                                    // (a) is also reached for zero method candidates.
                                }
                            }
                            continue;
                        }
                        Some(CalleeMeta::SelfRecv(impl_type))
                        | Some(CalleeMeta::SelfType(impl_type)) => {
                            let all = name_to_ids
                                .get(&rel.target_name)
                                .cloned()
                                .unwrap_or_default();
                            let same_lang: Vec<i64> = all
                                .iter()
                                .filter(|id| {
                                    matches!(
                                        node_id_to_language.get(id).and_then(|l| l.as_deref()),
                                        Some(l) if l == pf.language.as_str()
                                    )
                                })
                                .copied()
                                .collect();
                            let filtered = self_filter_candidates(&impl_type, &same_lang, db)?;
                            if filtered.is_empty() {
                                // No method on this impl type found in the project.
                                // Drop without buffering — qualifier is fixed and a
                                // re-scan will yield the same answer.
                                continue;
                            }
                            for &src_id in &source_ids {
                                for &tgt_id in &filtered {
                                    if src_id != tgt_id
                                        && insert_edge_cached(
                                            db.conn(),
                                            src_id,
                                            tgt_id,
                                            &rel.relation,
                                            rel.metadata.as_deref(),
                                        )?
                                    {
                                        total_edges_created += 1;
                                    }
                                }
                            }
                            continue;
                        }
                        Some(CalleeMeta::RecvType(recv_type)) => {
                            // Python receiver with a locally-inferred constructor
                            // type (issue #32 cause 2). Restrict same-language
                            // candidates to that type's OWN methods
                            // (self_filter_candidates → qualified_name LIKE
                            // 'Type.%'). When the type declares the method
                            // directly → bind precisely, disambiguating same-named
                            // methods on sibling classes (the whole point: pick
                            // DataWriter.write out of {DataWriter,Profile,Scenario}
                            // .write). When it does NOT — an INHERITED method
                            // (`Base.method` reached via a `Derived()` receiver) or
                            // a mis-inferred type — the filter is empty and we DO
                            // NOT continue: control falls through to the default
                            // bare resolution below. That keeps rtype strictly
                            // ADDITIVE precision — it can pick the right target
                            // among duplicates but can never DROP an edge the bare
                            // path would have resolved. (Contrast SelfRecv/SelfType,
                            // which drop on empty: a Rust `self.m()` whose `m` isn't
                            // on the impl type is a compile error, not an inherited
                            // hit, so there is nothing to fall back to.)
                            let all = name_to_ids
                                .get(&rel.target_name)
                                .cloned()
                                .unwrap_or_default();
                            let same_lang: Vec<i64> = all
                                .iter()
                                .filter(|id| {
                                    matches!(
                                        node_id_to_language.get(id).and_then(|l| l.as_deref()),
                                        Some(l) if l == pf.language.as_str()
                                    )
                                })
                                .copied()
                                .collect();
                            let filtered = self_filter_candidates(&recv_type, &same_lang, db)?;
                            if !filtered.is_empty() {
                                for &src_id in &source_ids {
                                    for &tgt_id in &filtered {
                                        if src_id != tgt_id
                                            && insert_edge_cached(
                                                db.conn(),
                                                src_id,
                                                tgt_id,
                                                &rel.relation,
                                                rel.metadata.as_deref(),
                                            )?
                                        {
                                            total_edges_created += 1;
                                        }
                                    }
                                }
                                continue;
                            }
                            // filtered empty → fall through to default resolution
                            // (inherited method / unique bare match / pending buffer).
                        }
                        Some(CalleeMeta::Path(segments)) => {
                            let all = name_to_ids
                                .get(&rel.target_name)
                                .cloned()
                                .unwrap_or_default();
                            // Same-file candidates take precedence per the bare-name
                            // qualifier design ("same-file matches still take precedence").
                            // Previously this filtered them out, so `Foo::helper()` in the
                            // same file as `impl Foo { fn helper }` produced no edge —
                            // the same-file pool was excluded before the Path filter,
                            // and the cross-file Path filter (which scans /Foo/ in the
                            // path) couldn't match a single-file project either.
                            let same_lang: Vec<i64> = all
                                .iter()
                                .filter(|id| {
                                    matches!(
                                        node_id_to_language.get(id).and_then(|l| l.as_deref()),
                                        Some(l) if l == pf.language.as_str()
                                    )
                                })
                                .copied()
                                .collect();
                            let filtered = path_filter_candidates(
                                &segments,
                                &same_lang,
                                &node_id_to_path,
                                db,
                            )?;
                            if filtered.is_empty() {
                                // No project candidate matches the Path qualifier.
                                // External crate (or unmatched module) — drop without buffering.
                                continue;
                            }
                            let final_targets = if filtered.len() > 1 {
                                refine_ambiguous_targets(&filtered, &pf.rel_path, &node_id_to_path)
                            } else {
                                filtered
                            };
                            for &src_id in &source_ids {
                                for &tgt_id in &final_targets {
                                    if src_id != tgt_id
                                        && insert_edge_cached(
                                            db.conn(),
                                            src_id,
                                            tgt_id,
                                            &rel.relation,
                                            rel.metadata.as_deref(),
                                        )?
                                    {
                                        total_edges_created += 1;
                                    }
                                }
                            }
                            continue;
                        }
                        _ => {} // None (Bare) or unrecognized q → falls through to default chain below.
                    }
                }

                // Default resolution: global name-based lookup with language-aware layering.
                // Tier order: same-file → same-language → (calls: drop) / (other: global).
                // Dropping calls without a same-language match prevents Rust `hasher.update()`
                // binding to an unrelated JS `function update()` via bare-name collision.
                let all_target_ids = name_to_ids
                    .get(&rel.target_name)
                    .cloned()
                    .unwrap_or_default();

                let same_file_targets: Vec<i64> = all_target_ids
                    .iter()
                    .filter(|id| local_ids.contains(id))
                    .copied()
                    .collect();

                let source_lang = pf.language.as_str();
                let same_language_targets: Vec<i64> = all_target_ids
                    .iter()
                    .filter(|id| !local_ids.contains(id))
                    .filter(|id| {
                        matches!(
                            node_id_to_language.get(id).and_then(|l| l.as_deref()),
                            Some(l) if l == source_lang
                        )
                    })
                    .copied()
                    .collect();

                let target_ids = if !same_file_targets.is_empty() {
                    same_file_targets
                } else if rel.relation == REL_CALLS
                    && is_cross_file_call_noise(&rel.target_name, source_lang)
                {
                    // Stdlib method names (new/default/from) — drop. Language-aware:
                    // the JS/TS family exempts non-ECMAScript names (insert/remove/
                    // contains) so user methods resolve; all else drops regardless
                    // of language (a Rust `hasher.update()` must not bind a JS fn).
                    continue;
                } else if !same_language_targets.is_empty() {
                    // Ambiguous cross-file same-language candidates (e.g. a helper
                    // name like `readJson` defined in multiple JS files) used to
                    // fan out — every same-name target got an edge, producing
                    // phantom callers across unrelated modules. Refine by
                    // non-test preference + longest common path prefix with the
                    // caller file. See `refine_ambiguous_targets` for fallback
                    // policy (keeps remaining pool on ambiguity to avoid
                    // regressing dead-code on bare-name Rust scoped calls).
                    refine_ambiguous_targets(&same_language_targets, &pf.rel_path, &node_id_to_path)
                } else if rel.relation == REL_CALLS {
                    // No same-file, no same-language candidate → buffer in
                    // pending_unresolved_calls instead of silently dropping.
                    // The post-Phase-2 sweep below promotes the row to a real
                    // edge as soon as a same-language target appears (e.g.
                    // sibling file added in a later incremental pass). Memory
                    // `feedback_incremental_edge_timing.md` documented the bug
                    // this closes: B's bare-name call to `foo()` got dropped
                    // when foo didn't exist yet, and never re-resolved when A
                    // later added `foo`. Schema cascade on source_id self-cleans
                    // when callers are removed/reindexed.
                    for &src_id in &source_ids {
                        crate::storage::queries::insert_pending_unresolved_call(
                            db.conn(),
                            src_id,
                            &rel.target_name,
                            &pf.language,
                            rel.metadata.as_deref(),
                        )?;
                    }
                    continue;
                } else if rel.relation == REL_REFERENCES {
                    // Bare-name value references (callbacks / fn pointers) share the
                    // cross-language collision risk of bare-name calls: short common
                    // names like `process` / `handler` / `run` exist in many
                    // languages. Without a same-file or same-language target, DROP
                    // rather than fall through to the global pool — a Rust
                    // `references → process` must never bind a JS `function
                    // process()` (feedback_edge_resolution_same_language). Precision
                    // over recall; no pending buffer in Phase 1 (full rebuild
                    // resolves; incremental-timing gap is the documented calls limit).
                    continue;
                } else {
                    // Structural relations (imports / inherits / implements /
                    // exports / routes_to) with no same-file and no same-EXACT-
                    // language target. Previously this fell through to the GLOBAL
                    // all-language pool, binding cross-LANGUAGE phantoms (Rust
                    // `use anyhow::Result` → a markdown "Result" heading; JS
                    // `require('fs')` → a Rust `fs` symbol) stamped `extracted`
                    // (unfilterable), polluting deps / project_map / cycles /
                    // find_references. Bind to same-language-FAMILY targets only:
                    // `detect_language` gives DIFFERENT strings within one family
                    // (`.ts`→typescript, `.tsx`→tsx, `.js`→javascript), so exact
                    // equality would wrongly DROP a real `.tsx` class extending a
                    // `.ts` base. Family filtering keeps those cross-family edges
                    // while still dropping genuinely cross-language phantoms
                    // (different families). Empty → IMPORTS/IMPLEMENTS reach the
                    // `<external>` sentinel below; the rest drop.
                    all_target_ids.iter()
                        .filter(|id| !local_ids.contains(id))
                        .filter(|id| matches!(
                            node_id_to_language.get(id).and_then(|l| l.as_deref()),
                            Some(l) if crate::utils::config::languages_compatible(l, source_lang)
                        ))
                        .copied()
                        .collect()
                };

                if target_ids.is_empty()
                    && (rel.relation == REL_IMPLEMENTS || rel.relation == REL_IMPORTS)
                {
                    // Unresolved implements target (external trait like Write, Default)
                    // OR unresolved import target (JS `require('fs')`, unresolved JS
                    // ES-import binding). Phase 2b-ext creates `<external>/<name>`
                    // sentinel nodes so the dependency graph shows the link.
                    for &src_id in &source_ids {
                        unresolved_externals.push((
                            src_id,
                            rel.target_name.clone(),
                            rel.relation.clone(),
                        ));
                    }
                } else {
                    for &src_id in &source_ids {
                        for &tgt_id in &target_ids {
                            if (src_id != tgt_id || rel.relation == REL_ROUTES_TO)
                                && insert_edge_cached(
                                    db.conn(),
                                    src_id,
                                    tgt_id,
                                    &rel.relation,
                                    rel.metadata.as_deref(),
                                )?
                            {
                                total_edges_created += 1;
                            }
                        }
                    }
                }
            }
        }

        // Phase 2b: Create virtual nodes for external Python imports
        if !external_python_imports.is_empty() {
            let ext_file_id = upsert_file(
                db.conn(),
                &FileRecord {
                    path: "<external>".into(),
                    blake3_hash: "external".into(),
                    last_modified: 0,
                    language: Some("external".into()),
                },
            )?;

            // Load existing external module nodes to avoid duplicates
            let existing_ext_nodes: HashMap<String, i64> =
                get_nodes_by_file_path(db.conn(), "<external>")?
                    .into_iter()
                    .map(|n| (n.name.clone(), n.id))
                    .collect();

            let unique_modules: HashSet<String> = external_python_imports
                .iter()
                .map(|(_, m)| m.clone())
                .collect();

            let mut ext_node_ids: HashMap<String, i64> = existing_ext_nodes;
            for module_name in &unique_modules {
                if !ext_node_ids.contains_key(module_name) {
                    let node_id = insert_node_cached(
                        db.conn(),
                        &NodeRecord {
                            file_id: ext_file_id,
                            node_type: "external_module".into(),
                            name: module_name.clone(),
                            qualified_name: Some(format!("<external>/{}", module_name)),
                            start_line: 0,
                            end_line: 0,
                            code_content: String::new(),
                            signature: None,
                            doc_comment: None,
                            context_string: None,
                            name_tokens: None,
                            return_type: None,
                            param_types: None,
                            is_test: false,
                        },
                    )?;
                    ext_node_ids.insert(module_name.clone(), node_id);
                    total_nodes_created += 1;
                }
            }

            for (source_id, module_name) in &external_python_imports {
                if let Some(&ext_id) = ext_node_ids.get(module_name) {
                    if insert_edge_cached(db.conn(), *source_id, ext_id, REL_IMPORTS, None)? {
                        total_edges_created += 1;
                    }
                }
            }
        }

        // Phase 2b-ext: Create sentinel nodes for unresolved external symbols
        // (e.g., Rust `impl Write for SharedStdout` where Write is from std::io)
        if !unresolved_externals.is_empty() {
            let ext_file_id = upsert_file(
                db.conn(),
                &FileRecord {
                    path: "<external>".into(),
                    blake3_hash: "external".into(),
                    last_modified: 0,
                    language: Some("external".into()),
                },
            )?;

            let existing_ext_nodes: HashMap<String, i64> =
                get_nodes_by_file_path(db.conn(), "<external>")?
                    .into_iter()
                    .map(|n| (n.name.clone(), n.id))
                    .collect();

            let mut ext_node_ids: HashMap<String, i64> = existing_ext_nodes;

            // Collect unique targets with inferred type
            let unique_targets: HashMap<&str, &str> = unresolved_externals
                .iter()
                .map(|(_, name, rel)| {
                    let node_type = if rel == REL_IMPLEMENTS {
                        "trait"
                    } else {
                        "module"
                    };
                    (name.as_str(), node_type)
                })
                .collect();

            for (&name, &node_type) in &unique_targets {
                if !ext_node_ids.contains_key(name) {
                    let node_id = insert_node_cached(
                        db.conn(),
                        &NodeRecord {
                            file_id: ext_file_id,
                            node_type: node_type.into(),
                            name: name.into(),
                            qualified_name: Some(format!("<external>/{}", name)),
                            start_line: 0,
                            end_line: 0,
                            code_content: String::new(),
                            signature: None,
                            doc_comment: None,
                            context_string: None,
                            name_tokens: None,
                            return_type: None,
                            param_types: None,
                            is_test: false,
                        },
                    )?;
                    ext_node_ids.insert(name.into(), node_id);
                    total_nodes_created += 1;
                }
            }

            for (source_id, target_name, relation) in &unresolved_externals {
                if let Some(&ext_id) = ext_node_ids.get(target_name.as_str()) {
                    if insert_edge_cached(db.conn(), *source_id, ext_id, relation, None)? {
                        total_edges_created += 1;
                    }
                }
            }
        }

        // Phase 2c: Restore cross-file inbound edges lost to cascade delete.
        // When a file is re-indexed, its old nodes are deleted (cascade-deleting edges).
        // Edges from OTHER files into the re-indexed file must be rebuilt using new node IDs.
        if !saved_inbound_edges.is_empty() {
            // Build (target_file_id, name) → new_node_id map for batch files. Keying
            // on the file the edge pointed INTO — not just the bare name — pins the
            // restore to the same-name node in THAT file, so a re-indexed sibling
            // file sharing the symbol name (or a cross-language same-name node in the
            // batch) can no longer steal the edge. A genuinely-removed symbol yields
            // no match → the edge drops, exactly as a full rebuild would.
            let mut batch_name_to_ids: HashMap<(i64, &str), Vec<i64>> = HashMap::new();
            for pf in &batch_parsed {
                for (id, name) in pf.node_ids.iter().zip(pf.node_names.iter()) {
                    batch_name_to_ids
                        .entry((pf.file_id, name.as_str()))
                        .or_default()
                        .push(*id);
                }
            }

            let mut restored = 0usize;
            let mut skipped_intra_batch = 0usize;
            for (source_id, source_file_id, target_file_id, target_name, relation, metadata) in
                &saved_inbound_edges
            {
                // Source file is also in this batch — source_id is stale (deleted + re-created).
                // Phase 2 already resolves cross-file edges for intra-batch files.
                if batch_file_ids.contains(source_file_id) {
                    skipped_intra_batch += 1;
                    continue;
                }
                if let Some(new_target_ids) =
                    batch_name_to_ids.get(&(*target_file_id, target_name.as_str()))
                {
                    for &new_tgt_id in new_target_ids {
                        if *source_id != new_tgt_id
                            && insert_edge_cached(
                                db.conn(),
                                *source_id,
                                new_tgt_id,
                                relation,
                                metadata.as_deref(),
                            )?
                        {
                            total_edges_created += 1;
                            restored += 1;
                        }
                    }
                }
            }
            if restored > 0 || skipped_intra_batch > 0 {
                tracing::debug!(
                    "[index] Restored {} cross-file inbound edges, skipped {} intra-batch",
                    restored,
                    skipped_intra_batch
                );
            }
        }

        tx.commit()?;

        let batch_file_count = batch_parsed.len();

        // Update global_name_map: remove old entries for batch files, add new ones
        for (_, entries) in global_name_map.iter_mut() {
            entries.retain(|(_id, path, _lang)| !batch_file_paths.contains(path.as_str()));
        }
        global_name_map.retain(|_, entries| !entries.is_empty());

        // Convert to lightweight records — drops Tree and source string
        for pf in batch_parsed {
            // Add newly committed nodes to the global map
            let pf_lang = Some(pf.language.clone());
            for (id, name) in pf.node_ids.iter().zip(pf.node_names.iter()) {
                global_name_map.entry(name.clone()).or_default().push((
                    *id,
                    pf.rel_path.clone(),
                    pf_lang.clone(),
                ));
            }
            all_indexed.push(FileIndexed {
                rel_path: pf.rel_path,
                node_ids: pf.node_ids,
                node_names: pf.node_names,
            });
            // pf.tree and pf.source are dropped here — memory freed
        }

        // Report progress after each batch
        if let Some(cb) = progress {
            cb(IndexPhase::Files, all_indexed.len(), files.len());
        }

        if files.len() > BATCH_SIZE {
            tracing::info!(
                "[index] batch {}/{}: {} files ({} nodes, {} edges)",
                all_indexed.len(),
                files.len(),
                batch_file_count,
                total_nodes_created,
                total_edges_created
            );
        }
    }

    // Finalizing heartbeat: every phase below is a full-graph pass with no
    // per-file progress, so the last `Files` tick would sit frozen for the whole
    // tail (minutes on 10k-file repos). Ticking between phases keeps the progress
    // consumer's mtime fresh — a stale-file gate can then distinguish "long tail
    // phase" from "indexer was killed". No-op when this run changed nothing, so a
    // no-diff incremental never (re)creates a progress file it never wrote to.
    let finalize_tick = || {
        if all_indexed.is_empty() && delete_paths.is_empty() {
            return;
        }
        if let Some(cb) = progress {
            cb(IndexPhase::Finalizing, all_indexed.len(), files.len());
        }
    };

    // Phase 3: Build context strings + embeddings (single transaction, lightweight)
    if !all_indexed.is_empty() {
        finalize_tick();
        let tx = db.savepoint("idx_context")?;
        let all_node_ids: Vec<i64> = all_indexed
            .iter()
            .flat_map(|fi| fi.node_ids.iter().copied())
            .collect();
        let all_edges = get_edges_batch(db.conn(), &all_node_ids)?;
        let all_node_details: HashMap<i64, (NodeResult, Option<String>)> = {
            let nodes = get_nodes_with_files_by_ids(db.conn(), &all_node_ids)?;
            nodes
                .into_iter()
                .map(|nwf| (nwf.node.id, (nwf.node, nwf.language)))
                .collect()
        };

        // Phase 3a: Build all context strings (CPU-bound, parallelized with rayon)
        // Flatten to (node_id, node_name, file_path) tuples for parallel iteration
        let node_tasks: Vec<(i64, &str, &str)> = all_indexed
            .iter()
            .flat_map(|fi| {
                fi.node_ids.iter().enumerate().map(move |(idx, &node_id)| {
                    (node_id, fi.node_names[idx].as_str(), fi.rel_path.as_str())
                })
            })
            .collect();

        let context_updates: Vec<(i64, String)> = node_tasks
            .par_iter()
            .map(|&(node_id, node_name, file_path)| {
                let edges = all_edges.get(&node_id);
                let cat = categorize_edges(edges, format_route_from_metadata);
                let node_detail = all_node_details.get(&node_id);

                let ctx = build_context_string(&NodeContext {
                    node_type: node_detail
                        .map(|(n, _)| n.node_type.clone())
                        .unwrap_or_default(),
                    name: node_name.to_string(),
                    qualified_name: node_detail.and_then(|(n, _)| n.qualified_name.clone()),
                    file_path: file_path.to_string(),
                    language: node_detail.and_then(|(_, lang)| lang.clone()),
                    signature: node_detail.and_then(|(n, _)| n.signature.clone()),
                    return_type: node_detail.and_then(|(n, _)| n.return_type.clone()),
                    param_types: node_detail.and_then(|(n, _)| n.param_types.clone()),
                    code_content: node_detail.map(|(n, _)| n.code_content.clone()),
                    routes: cat.routes,
                    callees: cat.callees,
                    callers: cat.callers,
                    inherits: cat.inherits,
                    imports: cat.imports,
                    implements: cat.implements,
                    exports: cat.exports,
                    doc_comment: node_detail.and_then(|(n, _)| n.doc_comment.clone()),
                });

                (node_id, ctx)
            })
            .collect();

        // Phase 3b: Batch update context strings in DB
        update_context_strings_batch(db.conn(), &context_updates)?;
        tx.commit()?;

        tracing::info!(
            "[index] Phase 3: context strings built for {} nodes",
            all_node_ids.len()
        );

        // Phase 3c: Embed outside the committed tx — recoverable on failure via repair_null_context_strings
        finalize_tick();
        if let Some(m) = model {
            if db.vec_enabled() {
                embed_and_store_batch(db, m, &context_updates)?;
            }
        }
    }

    // Phase 2c: sweep pending_unresolved_calls — promote any rows whose
    // target_name now resolves against a same-language node. Cheap when the
    // table is empty (typical after a full index of a self-contained codebase).
    let pending_resolved = resolve_pending_calls(db)?;
    total_edges_created += pending_resolved;
    if pending_resolved > 0 {
        tracing::info!(
            "[index] Phase 2c: resolved {} pending unresolved calls",
            pending_resolved
        );
    }

    // Phases 2d-bind, 2d-prune, and 2e are full-graph set-based passes (a JOIN over
    // all edges, a DELETE with correlated subqueries, and a GROUP-BY over all nodes).
    // Their result is a guaranteed no-op when this invocation indexed AND deleted
    // nothing: the edge set is unchanged, so the import-bind finds nothing new to
    // bind, the import-contradiction prune finds nothing to drop, and the confidence
    // reclassification recomputes identical counts. Gate the whole block on a real
    // change so no-diff incremental ticks (e.g. a file-watcher flush whose diff is
    // empty) don't pay for three full-graph scans on the hot path. When anything DID
    // change it must run GLOBALLY, not just over the changed files — adding/removing
    // a duplicate-named node in ONE file flips bind/prune eligibility and the
    // ambiguity of cross-file edges in OTHER, unchanged files. (Phase 2c above stays
    // unconditional: it early-returns on an empty pending table, so it is already
    // cheap on a no-op pass.)
    if !all_indexed.is_empty() || !delete_paths.is_empty() {
        finalize_tick();
        // Phase 2d-bind: positively resolve bare-name calls to the node an explicit
        // import in the caller's file binds them to. `refine_ambiguous_targets`
        // picks the path-closest same-name node, which can be the wrong file when
        // the caller `from X import name`s a farther one; that wrong edge is dropped
        // by the prune below, so without this bind the call would be left with no
        // edge at all. Insert the import-bound edge first, then let the prune remove
        // the contradicted proximity edge — together they repoint the call.
        let bound = bind_calls_to_imported_targets(db)?;
        total_edges_created += bound;
        if bound > 0 {
            tracing::info!(
                "[index] Phase 2d-bind: bound {} bare call(s) to their imported target",
                bound
            );
        }

        // Phase 2d: drop bare-name call edges contradicted by an explicit import in
        // the caller's file. `refine_ambiguous_targets` keeps every tied same-name
        // candidate when it has no disambiguating info; an import edge IS that info,
        // so a bare `save()` in a file that does `from db import save` must bind to
        // db.save only — the fanned-out edge to a sibling `save` elsewhere is a false
        // caller. Removes those false positives without touching the correct edge.
        let contradicted = prune_import_contradicted_call_edges(db)?;
        if contradicted > 0 {
            total_edges_created = total_edges_created.saturating_sub(contradicted);
            tracing::info!(
                "[index] Phase 2d: pruned {} import-contradicted call edges",
                contradicted
            );
        }

        // Phase 2e: classify edge confidence. Downgrades cross-file by-name
        // `calls`/`references` edges to inferred/ambiguous; every precise edge keeps
        // the column default `extracted`. Purely additive metadata — no edge
        // added or removed.
        let downgraded = classify_edge_confidence(db)?;
        if downgraded > 0 {
            tracing::info!(
                "[index] Phase 2e: classified {} cross-file by-name edge(s) as inferred/ambiguous",
                downgraded
            );
        }
    }

    // Optimize query planner statistics after bulk writes
    if !all_indexed.is_empty() {
        finalize_tick();
        let _ = db.run_optimize();
    }

    let stats = IndexStats {
        files_skipped_size: skipped_size.load(AtomicOrdering::Relaxed),
        files_skipped_parse: skipped_parse.load(AtomicOrdering::Relaxed),
        files_skipped_read: skipped_read.load(AtomicOrdering::Relaxed),
        files_skipped_hash: skipped_hash.load(AtomicOrdering::Relaxed),
        files_skipped_language: skipped_language.load(AtomicOrdering::Relaxed),
        files_with_parse_errors: parse_error_files.load(AtomicOrdering::Relaxed),
    };

    Ok(IndexResult {
        files_indexed: all_indexed.len(),
        files_deleted: delete_paths.len(),
        nodes_created: total_nodes_created,
        edges_created: total_edges_created,
        stats,
    })
}

#[cfg(test)]
mod tests {
    use super::looks_like_cpp_header;

    #[test]
    fn cpp_header_detection_upgrades_only_real_cpp() {
        // C++ markers → parse the `.h` as C++ (so class symbols aren't dropped).
        assert!(looks_like_cpp_header(
            "class Shape {\npublic:\n  void f();\n};"
        ));
        assert!(looks_like_cpp_header(
            "struct S { int x; };\nnamespace ns { int g(); }"
        ));
        assert!(looks_like_cpp_header("template<typename T> T id(T x);"));
        assert!(looks_like_cpp_header("template <class T> struct Box {};"));
        assert!(looks_like_cpp_header("int Foo::bar() { return 1; }")); // scope resolution
        assert!(looks_like_cpp_header(
            "class Widget {\nprivate:\n  int id;\n};"
        ));
        assert!(looks_like_cpp_header(
            "class Base {\nprotected:\n  int n;\n};"
        ));

        // Pure C headers have none of these → stay C (no over-eager upgrade).
        assert!(!looks_like_cpp_header(
            "#ifndef FOO_H\n#define FOO_H\nint add(int a, int b);\nstruct Point { int x; int y; };\n#endif"
        ));
        assert!(!looks_like_cpp_header(
            "typedef struct { int fd; } handle_t;\nvoid close_handle(handle_t*);"
        ));
        assert!(!looks_like_cpp_header(
            "#define MAX(a,b) ((a)>(b)?(a):(b))\nextern int errno;"
        ));
    }
}
