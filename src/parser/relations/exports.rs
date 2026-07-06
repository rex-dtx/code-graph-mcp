//! TypeScript/JavaScript export-statement extraction.
//! Captures `export function`, `export class`, `export interface`,
//! `export type`, `export enum`, `export abstract class`, and
//! `export const|let` declarations as REL_EXPORTS edges off `<module>`,
//! plus `export { X } from './mod'` re-exports (barrel/index files) as
//! REL_IMPORTS dependency edges.

use super::ParsedRelation;
use super::super::node_text;
use crate::domain::{REL_EXPORTS, REL_IMPORTS};

pub(super) fn extract_export_names(
    node: &tree_sitter::Node,
    source: &str,
    results: &mut Vec<ParsedRelation>,
) {
    // Re-export with a module source: `export { X, Y } from './mod'` /
    // `export { X as Z } from './mod'` — the barrel / index.ts pattern. Such a
    // statement is simultaneously a DEPENDENCY on './mod' and a re-export of its
    // symbols, but `extract_export_names` previously ignored the `source` field
    // entirely, so a barrel file had ZERO tracked edges: `deps` showed nothing,
    // `find-references` missed it, and affected/impact/cycles/tour could not
    // traverse THROUGH it. Emit a REL_IMPORTS edge per re-exported name, stamped
    // with the same `js_module` metadata a regular `import { X } from './mod'`
    // carries (extract_import_names), so Phase-2 resolves each to the concrete
    // file. The `name` field is the source module's export (the resolution
    // target), matching import specifiers; the optional alias is the local
    // re-export name and is irrelevant to the dependency edge.
    //
    // Out of scope (needs module-level, not name-level, resolution — a separate
    // limitation shared with namespace imports `import * as ns`): `export * from
    // './mod'` and `export * as ns from './mod'` carry no named specifiers, so no
    // per-name edge is emitted for them here.
    if let Some(src) = node.child_by_field_name("source") {
        let js_module = node_text(&src, source)
            .trim_matches(|c| c == '"' || c == '\'' || c == '`')
            .to_string();
        if !js_module.is_empty() {
            let metadata = Some(serde_json::json!({ "js_module": js_module }).to_string());
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    collect_reexport_specifiers(&child, source, metadata.as_deref(), results);
                }
            }
        }
        // A re-export statement carries no inline declaration to extract below.
        return;
    }

    // Walk direct children for exported declarations
    for i in 0..node.named_child_count() {
        let child = match node.named_child(i) {
            Some(c) => c,
            None => continue,
        };
        match child.kind() {
            "function_declaration" | "class_declaration" | "interface_declaration"
            | "type_alias_declaration" | "enum_declaration" | "abstract_class_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(&name_node, source).to_string();
                    if !name.is_empty() {
                        results.push(ParsedRelation {
                            source_name: "<module>".into(),
                            target_name: name,
                            relation: REL_EXPORTS.into(),
                            metadata: None,
                            source_language: String::new(),
                        });
                    }
                }
            }
            "lexical_declaration" => {
                // export const foo = ..., export let bar = ...
                for j in 0..child.named_child_count() {
                    if let Some(decl) = child.named_child(j) {
                        if decl.kind() == "variable_declarator" {
                            if let Some(name_node) = decl.child_by_field_name("name") {
                                let name = node_text(&name_node, source).to_string();
                                if !name.is_empty() {
                                    results.push(ParsedRelation {
                                        source_name: "<module>".into(),
                                        target_name: name,
                                        relation: REL_EXPORTS.into(),
                                        metadata: None,
                                        source_language: String::new(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Emit a REL_IMPORTS edge per re-exported name in an `export { A, B as C } from
/// '...'` clause (the `export_clause` → `export_specifier` children). Mirrors
/// extract_import_specifiers: the `name` field is the source module's export and
/// thus the dependency/resolution target; the optional `as` alias (the local
/// re-export name) does not affect the edge. Recurses through the clause wrapper.
fn collect_reexport_specifiers(
    node: &tree_sitter::Node,
    source: &str,
    metadata: Option<&str>,
    results: &mut Vec<ParsedRelation>,
) {
    if node.kind() == "export_specifier" {
        if let Some(name_node) = node.child_by_field_name("name") {
            let name = node_text(&name_node, source).to_string();
            if !name.is_empty() {
                results.push(ParsedRelation {
                    source_name: "<module>".into(),
                    target_name: name,
                    relation: REL_IMPORTS.into(),
                    metadata: metadata.map(str::to_string),
                    source_language: String::new(),
                });
            }
        }
        return;
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            collect_reexport_specifiers(&child, source, metadata, results);
        }
    }
}
