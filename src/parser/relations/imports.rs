//! Generic and Python-specific import extraction.
//! Generic side handles JS/TS/Java-style `import { Foo } from '...'` shapes
//! by walking import_clause/import_specifier subtrees. Python side keeps its
//! own paths because `from X import Y, Z` carries module-resolution metadata
//! that other languages don't have.

use super::super::node_text;
use super::helpers::MAX_SUBTREE_DEPTH;
use super::ParsedRelation;
use crate::domain::REL_IMPORTS;

pub(super) fn extract_import_names(
    node: &tree_sitter::Node,
    source: &str,
    results: &mut Vec<ParsedRelation>,
) {
    // Capture the ES module specifier (`from '../util/helper'`) so the indexer
    // can resolve a relative import to a concrete file (mirrors Python's
    // python_module metadata). The `source` field is the string literal; strip
    // its quotes. Absent (no `from` clause) → no metadata, default resolution.
    // The specifier is stamped on every binding this statement introduces.
    let js_module = node
        .child_by_field_name("source")
        .map(|s| node_text(&s, source))
        .map(|raw| {
            raw.trim_matches(|c| c == '"' || c == '\'' || c == '`')
                .to_string()
        })
        .filter(|s| !s.is_empty());
    let metadata: Option<String> = js_module
        .as_ref()
        .map(|m| serde_json::json!({ "js_module": m }).to_string());

    // Walk children looking for import specifiers or identifiers
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            match child.kind() {
                "import_clause" | "import_specifier" | "dotted_name" => {
                    // ESM namespace form `import * as ns from './m'`: the clause
                    // carries a namespace_import, which the specifier walk below
                    // does not know — it used to drop the whole binding
                    // (roadmap 2026-07-18 §2.3). Emit the q:"ns_import" marker
                    // (alias + specifier) that the indexer binds module-level and
                    // feeds into ns_module_map for `ns.foo()` member calls.
                    emit_namespace_import(&child, source, js_module.as_deref(), results);
                    // For named imports: import { Foo, Bar } from '...'
                    extract_import_specifiers(&child, source, results, metadata.as_deref());
                }
                "namespace_import" => {
                    emit_namespace_import(&child, source, js_module.as_deref(), results);
                }
                "identifier" => {
                    let name = node_text(&child, source).to_string();
                    if !name.is_empty() && name != "from" {
                        results.push(ParsedRelation {
                            source_name: "<module>".into(),
                            target_name: name,
                            relation: REL_IMPORTS.into(),
                            metadata: metadata.clone(),
                            source_language: String::new(),
                        });
                    }
                }
                _ => {
                    extract_import_names_recursive(&child, source, results, metadata.as_deref());
                }
            }
        }
    }
}

/// Emit the ns_import marker for a `namespace_import` (`* as ns`) found either
/// directly or as a child of the import_clause. Marker shape mirrors the CJS
/// `q:"ns_require"` one (mod.rs) so the indexer's ns_module_map + module-level
/// binding treat ESM and CJS namespaces identically. No specifier → no marker
/// (nothing to resolve against).
fn emit_namespace_import(
    node: &tree_sitter::Node,
    source: &str,
    js_module: Option<&str>,
    results: &mut Vec<ParsedRelation>,
) {
    let ns = if node.kind() == "namespace_import" {
        Some(*node)
    } else {
        (0..node.named_child_count())
            .filter_map(|i| node.named_child(i))
            .find(|c| c.kind() == "namespace_import")
    };
    let Some(ns) = ns else { return };
    let Some(alias) = (0..ns.named_child_count())
        .filter_map(|i| ns.named_child(i))
        .find(|c| c.kind() == "identifier")
    else {
        return;
    };
    let Some(module) = js_module else { return };
    results.push(ParsedRelation {
        source_name: "<module>".into(),
        target_name: node_text(&alias, source).to_string(),
        relation: REL_IMPORTS.into(),
        metadata: Some(serde_json::json!({ "q": "ns_import", "js_module": module }).to_string()),
        source_language: String::new(),
    });
}

fn extract_import_specifiers(
    node: &tree_sitter::Node,
    source: &str,
    results: &mut Vec<ParsedRelation>,
    metadata: Option<&str>,
) {
    extract_import_specifiers_inner(node, source, results, metadata, 0);
}

fn extract_import_specifiers_inner(
    node: &tree_sitter::Node,
    source: &str,
    results: &mut Vec<ParsedRelation>,
    metadata: Option<&str>,
    depth: usize,
) {
    if depth > MAX_SUBTREE_DEPTH {
        return;
    }
    if node.kind() == "import_specifier" {
        if let Some(name_node) = node.child_by_field_name("name") {
            let name = node_text(&name_node, source).to_string();
            results.push(ParsedRelation {
                source_name: "<module>".into(),
                target_name: name,
                relation: REL_IMPORTS.into(),
                metadata: metadata.map(str::to_string),
                source_language: String::new(),
            });
        }
        return;
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            extract_import_specifiers_inner(&child, source, results, metadata, depth + 1);
        }
    }
}

fn extract_import_names_recursive(
    node: &tree_sitter::Node,
    source: &str,
    results: &mut Vec<ParsedRelation>,
    metadata: Option<&str>,
) {
    extract_import_names_recursive_inner(node, source, results, metadata, 0);
}

fn extract_import_names_recursive_inner(
    node: &tree_sitter::Node,
    source: &str,
    results: &mut Vec<ParsedRelation>,
    metadata: Option<&str>,
    depth: usize,
) {
    if depth > MAX_SUBTREE_DEPTH {
        return;
    }
    if node.kind() == "import_specifier" || node.kind() == "identifier" {
        let name = if node.kind() == "import_specifier" {
            node.child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| node_text(node, source).to_string())
        } else {
            node_text(node, source).to_string()
        };
        if !name.is_empty() && name != "from" {
            results.push(ParsedRelation {
                source_name: "<module>".into(),
                target_name: name,
                relation: REL_IMPORTS.into(),
                metadata: metadata.map(str::to_string),
                source_language: String::new(),
            });
        }
        return;
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            extract_import_names_recursive_inner(&child, source, results, metadata, depth + 1);
        }
    }
}

/// Extract imports from Python `import X` / `import X, Y` statements.
/// AST: import_statement -> dotted_name ("os") ...
/// Adds metadata `{"python_module": "X", "is_module_import": true}` for module resolution.
pub(super) fn extract_python_import_names(
    node: &tree_sitter::Node,
    source: &str,
    results: &mut Vec<ParsedRelation>,
) {
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            if child.kind() == "dotted_name" || child.kind() == "identifier" {
                let name = node_text(&child, source).to_string();
                if !name.is_empty() {
                    let metadata = serde_json::json!({
                        "python_module": &name,
                        "is_module_import": true
                    })
                    .to_string();
                    results.push(ParsedRelation {
                        source_name: "<module>".into(),
                        target_name: name,
                        relation: REL_IMPORTS.into(),
                        metadata: Some(metadata),
                        source_language: String::new(),
                    });
                }
            } else if child.kind() == "aliased_import" {
                // import os as operating_system — extract the original module name
                if let Some(module) = child.named_child(0) {
                    let name = node_text(&module, source).to_string();
                    if !name.is_empty() {
                        let metadata = serde_json::json!({
                            "python_module": &name,
                            "is_module_import": true
                        })
                        .to_string();
                        results.push(ParsedRelation {
                            source_name: "<module>".into(),
                            target_name: name,
                            relation: REL_IMPORTS.into(),
                            metadata: Some(metadata),
                            source_language: String::new(),
                        });
                    }
                }
            }
        }
    }
}

/// Extract imports from Python `from X import Y, Z` statements.
/// AST: import_from_statement -> dotted_name ("collections"), dotted_name ("OrderedDict"), dotted_name ("defaultdict")
/// The first dotted_name is the module; the rest are imported names.
/// Adds metadata `{"python_module": "X"}` for module-constrained resolution.
pub(super) fn extract_python_from_import_names(
    node: &tree_sitter::Node,
    source: &str,
    results: &mut Vec<ParsedRelation>,
) {
    // Prefer tree-sitter field name for module (more robust than positional heuristic)
    let mut module_path: Option<String> = node
        .child_by_field_name("module_name")
        .map(|m| node_text(&m, source).to_string());
    let mut is_first_dotted_name = module_path.is_none();
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            match child.kind() {
                "dotted_name" => {
                    if is_first_dotted_name {
                        // First dotted_name is the module name — capture it for resolution
                        module_path = Some(node_text(&child, source).to_string());
                        is_first_dotted_name = false;
                    } else {
                        // Subsequent dotted_names are imported symbols
                        let name = node_text(&child, source).to_string();
                        if !name.is_empty() {
                            let metadata = module_path
                                .as_ref()
                                .map(|m| serde_json::json!({"python_module": m}).to_string());
                            results.push(ParsedRelation {
                                source_name: "<module>".into(),
                                target_name: name,
                                relation: REL_IMPORTS.into(),
                                metadata,
                                source_language: String::new(),
                            });
                        }
                    }
                }
                "identifier" => {
                    // Some tree-sitter versions parse simple import names as bare identifiers
                    // (e.g., `from os import path` where `path` is an identifier, not dotted_name)
                    let name = node_text(&child, source).to_string();
                    if !name.is_empty() {
                        let metadata = module_path
                            .as_ref()
                            .map(|m| serde_json::json!({"python_module": m}).to_string());
                        results.push(ParsedRelation {
                            source_name: "<module>".into(),
                            target_name: name,
                            relation: REL_IMPORTS.into(),
                            metadata,
                            source_language: String::new(),
                        });
                    }
                }
                "aliased_import" => {
                    // from X import Y as Z — extract Y (the original name)
                    if let Some(original) = child.named_child(0) {
                        let name = node_text(&original, source).to_string();
                        if !name.is_empty() {
                            let metadata = module_path
                                .as_ref()
                                .map(|m| serde_json::json!({"python_module": m}).to_string());
                            results.push(ParsedRelation {
                                source_name: "<module>".into(),
                                target_name: name,
                                relation: REL_IMPORTS.into(),
                                metadata,
                                source_language: String::new(),
                            });
                        }
                    }
                }
                "wildcard_import" => {
                    // from X import * — record as wildcard
                    let metadata = module_path
                        .as_ref()
                        .map(|m| serde_json::json!({"python_module": m}).to_string());
                    results.push(ParsedRelation {
                        source_name: "<module>".into(),
                        target_name: "*".into(),
                        relation: REL_IMPORTS.into(),
                        metadata,
                        source_language: String::new(),
                    });
                }
                _ => {}
            }
        }
    }
}
