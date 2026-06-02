//! Rust-specific extraction: `use` declarations (simple/grouped/nested/aliased)
//! and `impl Trait for Type` blocks (emits both type-level and method-level
//! IMPLEMENTS edges so the dead-code pass sees incoming edges on trait methods).
//! Also extracts REFERENCES edges for edgeless usages: path-qualified value
//! paths (`crate::domain::FOO`) and type-position usages (`field: MyType`,
//! `-> MyType`, `Vec<MyType>`).

use super::ParsedRelation;
use super::super::node_text;
use super::helpers::MAX_SUBTREE_DEPTH;
use crate::domain::{REL_IMPORTS, REL_IMPLEMENTS, REL_REFERENCES};

/// Extract import names from Rust `use` declarations by walking the tree-sitter AST.
/// Handles simple (`use foo::Bar`), grouped (`use foo::{Bar, Baz}`),
/// nested (`use foo::{bar::{A, B}}`), aliased (`use foo::Bar as B`), and glob imports.
pub(super) fn extract_rust_use_imports(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
    results: &mut Vec<ParsedRelation>,
) {
    fn collect_use_names(node: &tree_sitter::Node, source: &str, names: &mut Vec<String>) {
        collect_use_names_inner(node, source, names, 0);
    }
    fn collect_use_names_inner(node: &tree_sitter::Node, source: &str, names: &mut Vec<String>, depth: usize) {
        if depth > MAX_SUBTREE_DEPTH { return; }
        match node.kind() {
            "use_as_clause" => {
                if let Some(child) = node.named_child(0) {
                    collect_use_names_inner(&child, source, names, depth + 1);
                }
            }
            "use_wildcard" => {}
            "use_list" => {
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i) {
                        collect_use_names_inner(&child, source, names, depth + 1);
                    }
                }
            }
            "scoped_use_list" => {
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i) {
                        if child.kind() != "scoped_identifier" && child.kind() != "identifier" {
                            collect_use_names_inner(&child, source, names, depth + 1);
                        }
                    }
                }
            }
            "scoped_identifier" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = node_text(&name_node, source);
                    if !name.is_empty() && name != "*" && name != "self" {
                        names.push(name.to_string());
                    }
                }
            }
            "identifier" | "type_identifier" => {
                let name = node_text(node, source);
                if !name.is_empty() && name != "self" {
                    names.push(name.to_string());
                }
            }
            _ => {
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i) {
                        collect_use_names_inner(&child, source, names, depth + 1);
                    }
                }
            }
        }
    }

    let mut names = Vec::new();
    // The use_declaration's first named child is the argument (scoped_identifier, use_list, etc.)
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            collect_use_names(&child, source, &mut names);
        }
    }

    let scope_name = scope.unwrap_or("<module>");
    for name in names {
        results.push(ParsedRelation {
            source_name: scope_name.to_string(),
            target_name: name,
            relation: REL_IMPORTS.into(),
            metadata: None,
            source_language: String::new(),
        });
    }
}

/// Extract `impl Trait for Type` → Type implements Trait
pub(super) fn extract_rust_impl_trait(node: &tree_sitter::Node, source: &str) -> Option<ParsedRelation> {
    // impl_item has "trait" and "type" fields when it's `impl Trait for Type`
    let trait_node = node.child_by_field_name("trait")?;
    let type_node = node.child_by_field_name("type")?;
    let trait_name = node_text(&trait_node, source).to_string();
    let type_text = node_text(&type_node, source).to_string();
    // Strip generics so source resolution can match the bare struct name.
    // The `type` field on a generic impl block returns the full `Type<'a, W>`
    // text; Phase 2 source resolution (index_files.rs) does exact-name match
    // against local node names ("Type"), so without stripping, no edge would
    // emit for any generic trait impl — every method appears dead.
    let type_name = type_text.split('<').next().unwrap_or(&type_text).trim().to_string();
    if trait_name.is_empty() || type_name.is_empty() {
        return None;
    }
    Some(ParsedRelation {
        source_name: type_name,
        target_name: trait_name,
        relation: REL_IMPLEMENTS.into(),
        metadata: None,
        source_language: String::new(),
    })
}

/// Emit a `references` edge for a path-qualified usage (`a::b::FOO`) that is
/// neither a call (its parent is a `call_expression` `function` field) nor part
/// of a `use` declaration, and is the outermost `scoped_identifier`.
///
/// Outermost-only is enforced by rejecting a `scoped_identifier` whose parent is
/// itself a `scoped_identifier` (intermediate path segments). Type-position
/// paths (`scoped_type_identifier`, e.g. the inner segments of a struct-expr
/// path `crate::parser::NodeRecord { .. }`) are excluded too — that usage is
/// already covered by the `calls` edge to the struct/type name, so re-emitting a
/// `references` edge to an intermediate path segment ("parser") would be a false
/// positive.
pub(super) fn extract_rust_path_reference(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
) -> Option<ParsedRelation> {
    let parent = node.parent()?;
    match parent.kind() {
        // Callee of a call (`crate::foo::bar()`) — already a `calls` edge.
        // A `call_expression` parent where this node is NOT the `function`
        // field (e.g. a path passed as an argument) falls through to `_`.
        "call_expression"
            if parent.child_by_field_name("function").map(|f| f.id()) == Some(node.id()) =>
        {
            return None;
        }
        "use_declaration" | "scoped_use_list" | "use_list" | "use_as_clause" => return None,
        // Intermediate path segment of a longer `a::b::c` chain.
        "scoped_identifier" => return None,
        // Type-position path (struct-expr type, generic bounds, etc.). The
        // type name is already a `calls` edge; intermediate segments here are
        // module path, not a value reference.
        "scoped_type_identifier" => return None,
        _ => {}
    }
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(&name_node, source);
    if name.is_empty() || name == "self" || name == "Self" || name == "*" {
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

/// Emit a `references` edge for a `type_identifier` used in type position
/// (field type, return type, generic arg). Skips these non-usage / already-edged
/// cases so the references edge stays a pure "edgeless usage" signal:
/// - the type's own definition name (the `name` field of
///   struct/enum/type/trait/union items) — declaration, not a usage;
/// - the `name` of a bare `struct_expression` (`Foo { .. }`) — already a `calls`
///   edge, avoids a double edge;
/// - the inner `type_identifier` of a `scoped_type_identifier` that is a
///   `struct_expression` name (`mod::Foo { .. }`) — same `calls` double-edge;
/// - the `type` / `trait` field of an `impl_item` (`impl Foo` /
///   `impl Trait for Foo`) — already an IMPLEMENTS edge, and a references edge
///   there would defeat dead-code detection (direct-parent `impl_item` only;
///   generic / path-qualified impl headers are a documented residual);
/// - `Self`.
pub(super) fn extract_rust_type_reference(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
) -> Option<ParsedRelation> {
    if let Some(parent) = node.parent() {
        // The `name` field of a definition is the declaration, not a usage.
        if matches!(parent.kind(), "struct_item" | "enum_item" | "type_item" | "trait_item" | "union_item")
            && parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
        {
            return None;
        }
        // Impl-header type/trait names (`impl Widget` / `impl Trait for Widget`).
        // The impl relationship is already an IMPLEMENTS edge (see
        // extract_rust_impl_trait); a references edge here is redundant noise
        // AND defeats dead-code detection — every impl'd type would get an
        // incoming reference and could never be flagged dead. Only the
        // direct-parent `impl_item` case is covered: generic (`impl Container<T>`,
        // type field = generic_type) and path-qualified (`impl mod::Trait for
        // mod::Type`, fields = scoped_type_identifier) impl headers put the
        // type_identifier one level deeper and are a documented residual.
        if parent.kind() == "impl_item"
            && (parent.child_by_field_name("type").map(|n| n.id()) == Some(node.id())
                || parent.child_by_field_name("trait").map(|n| n.id()) == Some(node.id()))
        {
            return None;
        }
        // The `name` of `Foo { .. }` already yields a `calls` edge — don't double-emit.
        if parent.kind() == "struct_expression"
            && parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
        {
            return None;
        }
        // Path-qualified struct expr `mod::Foo { .. }`: the `name` field is a
        // `scoped_type_identifier` whose inner `name` is this `type_identifier`
        // ("Foo"). The `struct_expression` arm already strips the path and emits
        // a `calls` edge to "Foo", so this inner type_identifier must not also
        // emit a `references` edge (would be a double edge — same target).
        if parent.kind() == "scoped_type_identifier"
            && parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
        {
            if let Some(grandparent) = parent.parent() {
                if grandparent.kind() == "struct_expression"
                    && grandparent.child_by_field_name("name").map(|n| n.id()) == Some(parent.id())
                {
                    return None;
                }
            }
        }
    }
    let name = node_text(node, source);
    if name.is_empty() || name == "Self" {
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
