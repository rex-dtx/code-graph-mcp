//! Python-specific extraction: REFERENCES edges for edgeless type-annotation
//! usages.
//!
//! THE KEY DIFFERENCE from Rust/TS: tree-sitter-python represents a type name in
//! an annotation as a plain `identifier` — the SAME node kind as a value
//! identifier (`u`, `account`, `compute`). There is no distinct `type_identifier`
//! kind to gate on. So this extractor gates on ANNOTATION CONTEXT, not node kind:
//! an `identifier` only emits a reference when its nearest meaningful ancestor is
//! a `type` node (tree-sitter-python wraps every annotation type in a `type`
//! node). Gating on kind alone would emit a reference for every variable and
//! function name in the file.
//!
//! Probe-confirmed annotation shapes (tree-sitter-python):
//! - parameter:  `def f(x: Foo)` → `typed_parameter` → `type[field=type]` → `identifier Foo`
//! - return:     `def f() -> Bar` → `function_definition` → `type[field=return_type]` → `identifier Bar`
//! - variable / class attr: `x: Baz` → `assignment` → `type[field=type]` → `identifier Baz`
//! - generic:    `List[User]` → `type` → `generic_type` → `identifier List` (head);
//!   the arg `User` is `type_parameter [User]` → `type` → `identifier User`
//!   (its own nested `type`).
//! - dotted:     `mod.Type` → `type` → `attribute`, where `mod` is
//!   `identifier[field=object]` and `Type` is `identifier[field=attribute]`.
//!
//! Naturally excluded (NOT under a `type` node):
//! - base classes: `class Foo(Base)` → `argument_list[field=superclasses]` → `identifier Base`
//!   (already an inherits edge);
//! - value identifiers, attribute reads, call names.

use super::ParsedRelation;
use super::super::node_text;
use super::helpers::MAX_SUBTREE_DEPTH;
use crate::domain::{REL_REFERENCES, PYTHON_TYPE_REFERENCE_NOISE};

/// True if `node` is the type NAME of an annotation, i.e. it sits in a position
/// that tree-sitter-python wraps in a `type` node. The gate walks up a small,
/// bounded chain of *type-structural* ancestors (`type`, `generic_type`,
/// `type_parameter`) and returns true only when it reaches a `type` node:
///
/// - `identifier` directly inside `type`            → `Foo` in `x: Foo` ✓
/// - `identifier` directly inside `generic_type`    → `List` in `List[User]`
///   (the generic head); its `generic_type` parent must itself be under a `type` ✓
/// - the nested-`type` generic arg                  → `User` in `List[User]`
///   reaches `type` via its own `type` parent ✓
/// - `identifier[field=attribute]` of an `attribute` whose parent is a `type`
///   → `Type` in `mod.Type` ✓ (the dotted-annotation tail)
///
/// A value identifier (`u`, `account`, `compute`) has parent `attribute`
/// (value read), `argument_list`, `call`, `assignment[field=left/right]`,
/// `parameters`, etc. — none of which lead to a `type` node — so it returns
/// false. Base classes live under `argument_list`, also false.
fn is_annotation_type_name(node: &tree_sitter::Node) -> bool {
    // Special-case the dotted tail `mod.Type`: the tail `identifier` is the
    // `attribute` field of an `attribute` node. It is an annotation type only if
    // that `attribute` is itself directly under a `type` node (not a value read
    // like `u.account`, whose `attribute` parent is under a return/expression).
    if let Some(parent) = node.parent() {
        if parent.kind() == "attribute" {
            // Only the tail (`attribute` field) is the type name; the `object`
            // segment (`mod`) is a module path, not a project type usage.
            let is_tail = parent
                .child_by_field_name("attribute")
                .map(|n| n.id())
                == Some(node.id());
            if !is_tail {
                return false;
            }
            return parent.parent().map(|gp| gp.kind() == "type").unwrap_or(false);
        }
    }

    // Walk up through type-structural wrappers only. Stop (return false) the
    // moment we hit a non-type-structural ancestor — that means this identifier
    // is in value position, not an annotation.
    let mut current = match node.parent() {
        Some(p) => p,
        None => return false,
    };
    let mut depth = 0;
    while depth <= MAX_SUBTREE_DEPTH {
        match current.kind() {
            "type" => return true,
            // `generic_type` head (`List` in `List[User]`) and `type_parameter`
            // (`[User]`) are the only intermediates between an annotation
            // `identifier` and its enclosing `type`. Keep climbing.
            "generic_type" | "type_parameter" => {
                current = match current.parent() {
                    Some(p) => p,
                    None => return false,
                };
                depth += 1;
            }
            // Any other ancestor (assignment, attribute value-read, argument_list,
            // call, parameters, block, ...) means non-annotation position.
            _ => return false,
        }
    }
    false
}

/// Emit a `references` edge for a Python `identifier` used as a type-annotation
/// name. Gated by `is_annotation_type_name` (annotation context, since Python
/// annotation types are plain `identifier`s). Skips:
/// - builtins / `typing` generics (`int`, `str`, `List`, `Optional`, ...) — they
///   resolve to the stdlib, not a project symbol (PYTHON_TYPE_REFERENCE_NOISE);
/// - empty / `_`.
///
/// Base classes (`class Foo(Base)`) are NOT reached here — they live under
/// `argument_list`, never a `type` node — so the existing inherits edge is not
/// double-emitted.
pub(super) fn extract_python_type_reference(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
) -> Option<ParsedRelation> {
    if !is_annotation_type_name(node) {
        return None;
    }
    let name = node_text(node, source);
    if name.is_empty()
        || name == "_"
        || PYTHON_TYPE_REFERENCE_NOISE.contains(&name)
    {
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
