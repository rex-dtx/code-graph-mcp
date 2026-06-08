//! Go-specific extraction: REFERENCES edges for edgeless type-position usages.
//!
//! Like Rust/TS (and UNLIKE Python), tree-sitter-go represents a type name in
//! type position as a distinct `type_identifier` kind, so the extractor gates on
//! node kind. The kinds were probe-confirmed against tree-sitter-go:
//! - field type:        `type S struct { f Foo }` → `field_declaration[field=type]` → `type_identifier Foo`
//! - param type:        `func g(x Foo)` → `parameter_declaration[field=type]` → `type_identifier Foo`
//! - return type:       `func g() Bar` → `function_declaration[field=result]` → `type_identifier Bar`
//! - var type:          `var v Foo` → `var_spec[field=type]` → `type_identifier Foo`
//! - slice element:     `[]Foo` → `slice_type[field=element]` → `type_identifier Foo`
//! - map key/value:     `map[string]Foo` → `map_type[field=key/value]` → `type_identifier`
//! - composite literal: `Foo{}` → `composite_literal[field=type]` → `type_identifier Foo`
//! - method receiver:   `func (r *Foo) M()` → `pointer_type` → `type_identifier Foo`
//! - own definition:    `type Foo struct {}` → `type_spec[field=name]` → `type_identifier Foo` (SKIP)
//! - qualified type:    `pkg.Type` → `qualified_type` with head `package_identifier` (`pkg`)
//!   and tail `type_identifier[field=name]` (`Type`). The head is a
//!   `package_identifier`, NOT a `type_identifier`, so it is naturally excluded;
//!   only the tail `Type` reaches this fn and emits — exactly the desired
//!   "reference the type, not the package path" behavior, no extra handling.
//!
//! Naturally excluded (NOT `type_identifier`):
//! - value selectors `pkg.Func()` / `obj.field` → the head is `identifier`, the
//!   tail is `field_identifier`;
//! - function / variable / field NAMES → `identifier` / `field_identifier`.
//!
//! Builtins (`int`, `string`, `error`, ...) ARE `type_identifier` in
//! tree-sitter-go (unlike TS's `predefined_type`), so they must be filtered via
//! GO_TYPE_REFERENCE_NOISE — otherwise `var x int` would emit a reference to
//! `int`.

use super::ParsedRelation;
use super::super::node_text;
use super::helpers::MAX_SUBTREE_DEPTH;
use crate::domain::{REL_REFERENCES, GO_TYPE_REFERENCE_NOISE};

/// Emit a `references` edge for a `type_identifier` used in type position. Skips
/// the cases already covered by another edge (or that aren't usages) so the
/// references edge stays a pure "edgeless usage" signal:
/// - the type's OWN definition name — the `name` field of a `type_spec`
///   (`type Foo struct {}` / `type Bar = Baz`) — declaration, not a usage;
/// - Go predeclared builtin types (`int`, `string`, `error`, ...) — they resolve
///   to the language, not a project symbol (GO_TYPE_REFERENCE_NOISE);
/// - empty.
///
/// The method-receiver type (`func (r *Foo) M()`, `Foo` under a `pointer_type`)
/// is INTENTIONALLY emitted: it is a real usage of `Foo` and produces no other
/// edge (Go method ownership is not tracked via a `calls`/`inherits` edge here),
/// so a reference correctly links the method's file to the type and keeps `Foo`
/// out of dead-code false positives.
pub(super) fn extract_go_type_reference(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
) -> Option<ParsedRelation> {
    if let Some(parent) = node.parent() {
        // The `name` field of a `type_spec` is the declaration (`type Foo ...`),
        // not a usage. (The RHS type of a `type Alias = Base` lives in the `type`
        // field, not `name`, so aliased base types still emit.)
        if parent.kind() == "type_spec"
            && parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
        {
            return None;
        }
    }
    let name = node_text(node, source);
    if name.is_empty() || GO_TYPE_REFERENCE_NOISE.contains(&name) {
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

/// Emit a `references` edge for a Go `identifier` used as a function VALUE — a
/// callback passed/stored/returned by bare name. Value positions:
///   - call argument (`register(handler)`) — under an `argument_list`;
///   - RHS of `:=` / `=` (`cb := handler`) — an `expression_list` that is the
///     `right` field of a `short_var_declaration` / `assignment_statement`;
///   - `return handler` — an `expression_list` under a `return_statement`;
///   - `var cb = handler` — the `value` field of a `var_spec`.
///
/// Self-exclusion is structural: a call's callee is the `function` field of a
/// `call_expression` (not `argument_list`); selectors (`pkg.Fn`) use
/// `field_identifier`. M2/M2.5: a name equal to a parameter / receiver or a local
/// binding (`:=`, `var`, `range`, `=` target) of an enclosing function is a local,
/// not a global-fn reference — skip. Builtins are filtered by name.
pub(super) fn extract_go_value_reference(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
) -> Option<ParsedRelation> {
    let parent = node.parent()?;
    let in_value_position = match parent.kind() {
        "argument_list" => true,
        "expression_list" => match parent.parent() {
            Some(gp) => match gp.kind() {
                "return_statement" => true,
                "short_var_declaration" | "assignment_statement" => {
                    gp.child_by_field_name("right").map(|r| r.id()) == Some(parent.id())
                }
                _ => false,
            },
            None => false,
        },
        "var_spec" => parent.child_by_field_name("value").map(|v| v.id()) == Some(node.id()),
        // Phase 3b: composite-literal field value (`Handler{OnEvent: handler}` /
        // positional `[]fn{a, b}`). A keyed element is `key : value` — only the
        // value (last named child) emits, never the field-name key.
        "keyed_element" => {
            let n = parent.named_child_count();
            n >= 2 && parent.named_child(n - 1).map(|c| c.id()) == Some(node.id())
        }
        "literal_value" => true,
        "literal_element" => match parent.parent() {
            Some(gp) if gp.kind() == "keyed_element" => {
                let n = gp.named_child_count();
                n >= 2 && gp.named_child(n - 1).map(|c| c.id()) == Some(parent.id())
            }
            Some(gp) => gp.kind() == "literal_value",
            None => false,
        },
        _ => false,
    };
    if !in_value_position {
        return None;
    }
    let name = node_text(node, source);
    if name.is_empty() || name == "_" || GO_TYPE_REFERENCE_NOISE.contains(&name) {
        return None;
    }
    if go_enclosing_fn_local_names(node, source).contains(name) {
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

/// Collect local binding names visible to a Go value-reference candidate:
/// parameters + receiver of every enclosing func (declaration / literal — func
/// literals capture outer scope) + `:=` / `var` / `range` / `=` targets in the
/// nearest function body. M2/M2.5 exclusion; over-collection is precision-safe.
fn go_enclosing_fn_local_names(node: &tree_sitter::Node, source: &str) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let mut nearest_body_done = false;
    let mut cur = node.parent();
    while let Some(n) = cur {
        if matches!(n.kind(), "function_declaration" | "method_declaration" | "func_literal") {
            for field in ["parameters", "receiver"] {
                if let Some(p) = n.child_by_field_name(field) {
                    collect_go_idents(&p, source, &mut names, 0);
                }
            }
            if !nearest_body_done {
                if let Some(body) = n.child_by_field_name("body") {
                    collect_go_local_targets(&body, source, &mut names, 0);
                }
                nearest_body_done = true;
            }
        }
        cur = n.parent();
    }
    names
}

/// Walk a function body collecting `:=` / `var` / `range` / `=` TARGET names (the
/// `left` / `name` field), not RHS values. Recurses into nested blocks.
fn collect_go_local_targets(
    node: &tree_sitter::Node,
    source: &str,
    out: &mut std::collections::HashSet<String>,
    depth: usize,
) {
    if depth > MAX_SUBTREE_DEPTH {
        return;
    }
    match node.kind() {
        "short_var_declaration" | "assignment_statement" | "range_clause" => {
            if let Some(l) = node.child_by_field_name("left") {
                collect_go_idents(&l, source, out, 0);
            }
        }
        "var_spec" => {
            if let Some(nm) = node.child_by_field_name("name") {
                collect_go_idents(&nm, source, out, 0);
            }
        }
        _ => {}
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            collect_go_local_targets(&child, source, out, depth + 1);
        }
    }
}

/// Collect all `identifier` names in a subtree.
fn collect_go_idents(
    node: &tree_sitter::Node,
    source: &str,
    out: &mut std::collections::HashSet<String>,
    depth: usize,
) {
    if depth > MAX_SUBTREE_DEPTH {
        return;
    }
    if node.kind() == "identifier" {
        out.insert(node_text(node, source).to_string());
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            collect_go_idents(&child, source, out, depth + 1);
        }
    }
}
