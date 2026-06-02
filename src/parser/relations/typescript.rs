//! TypeScript/TSX-specific extraction: REFERENCES edges for edgeless
//! type-position usages.
//!
//! A `type_identifier` in type position (type annotation `x: Foo`, return type
//! `(): Foo`, generic arg `Array<Foo>`, field type `size: Foo`) is a usage of a
//! type that produces no `calls`/`imports`/`inherits` edge on its own, so
//! `find_dead_code` would flag `Foo` dead and `find_references` would miss it.
//! This mirrors `rust::extract_rust_type_reference` for TS/TSX.
//!
//! Tree-sitter-typescript represents every type-position name as a
//! `type_identifier`; primitive types (`string`, `number`, `boolean`, `any`,
//! `void`, `unknown`, `never`) parse as `predefined_type`, so they are
//! naturally excluded. JavaScript has no type annotations, so this is a no-op
//! there — the gate is `language == "typescript" || language == "tsx"`.

use super::ParsedRelation;
use super::super::node_text;
use crate::domain::REL_REFERENCES;

/// Emit a `references` edge for a `type_identifier` used in type position.
/// Skips the cases already covered by another edge (or that aren't usages) so
/// the references edge stays a pure "edgeless usage" signal:
/// - the type's OWN definition name — the `name` field of
///   `interface_declaration` / `class_declaration` / `type_alias_declaration` /
///   `enum_declaration` (declaration, not a usage);
/// - heritage types — a `type_identifier` anywhere inside `extends_clause` /
///   `implements_clause` / `class_heritage` already yields an inherits/implements
///   edge, so a references edge there would be a double edge (and would defeat
///   dead-code detection for the supertype).
///
/// Note: the `name` field of `generic_type` (`Array` in `Array<Foo>`, `Map` in
/// `Map<K, V>`) IS a real usage and is intentionally NOT skipped — only the
/// declaration-name skip is keyed off the four declaration parent kinds, not off
/// "is some parent's name field".
pub(super) fn extract_ts_type_reference(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
) -> Option<ParsedRelation> {
    if let Some(parent) = node.parent() {
        // The `name` field of a type/interface/class/enum declaration is the
        // declaration, not a usage. (class/enum names are `type_identifier` /
        // `identifier`; only `type_identifier` reaches this fn, but interface
        // and type-alias names ARE `type_identifier`, so this matters.)
        if matches!(
            parent.kind(),
            "interface_declaration"
                | "class_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
        ) && parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
        {
            return None;
        }
        // Heritage types (`extends Bar` / `implements Iface`) already produce
        // inherits/implements edges — don't double-emit a reference.
        if matches!(
            parent.kind(),
            "extends_clause" | "implements_clause" | "class_heritage"
        ) {
            return None;
        }
    }
    let name = node_text(node, source);
    if name.is_empty() {
        return None;
    }
    Some(ParsedRelation {
        source_name: scope.unwrap_or("<module>").to_string(),
        target_name: name.to_string(),
        relation: REL_REFERENCES.into(),
        metadata: None,
        source_language: String::new(),
    })
}
