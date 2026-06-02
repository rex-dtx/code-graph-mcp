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
