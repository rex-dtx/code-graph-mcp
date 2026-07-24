//! Rust-specific extraction: `use` declarations (simple/grouped/nested/aliased)
//! and `impl Trait for Type` blocks (emits both type-level and method-level
//! IMPLEMENTS edges so the dead-code pass sees incoming edges on trait methods).
//! Also extracts REFERENCES edges for edgeless usages: path-qualified value
//! paths (`crate::domain::FOO`) and type-position usages (`field: MyType`,
//! `-> MyType`, `Vec<MyType>`).

use super::super::node_text;
use super::helpers::MAX_SUBTREE_DEPTH;
use super::ParsedRelation;
use crate::domain::{REL_CALLS, REL_IMPLEMENTS, REL_IMPORTS, REL_REFERENCES};

/// Leftmost path segment of a `use` argument: `std::fs` → "std",
/// `std::{fs, io}` → "std", `crate::x::Y` → "crate", `fs` → "fs".
/// Descends `path` fields (and through `use_as_clause`) to the root token.
fn use_path_root<'a>(node: &tree_sitter::Node, source: &'a str) -> &'a str {
    let mut cur = *node;
    for _ in 0..MAX_SUBTREE_DEPTH {
        if cur.kind() == "use_as_clause" {
            match cur.named_child(0) {
                Some(c) => {
                    cur = c;
                    continue;
                }
                None => break,
            }
        }
        match cur.child_by_field_name("path") {
            Some(p) => cur = p,
            None => break,
        }
    }
    node_text(&cur, source)
}

/// Extract import names from Rust `use` declarations by walking the tree-sitter AST.
/// Handles simple (`use foo::Bar`), grouped (`use foo::{Bar, Baz}`),
/// nested (`use foo::{bar::{A, B}}`), aliased (`use foo::Bar as B`), and glob imports.
///
/// Statically-known EXTERNAL roots (`std`/`core`/`alloc`/`proc_macro`) are
/// skipped whole (audit 2026-07-24, IDX v52): their bare trailing segment used
/// to enter the global bare-name lookup with no qualifier metadata, where it
/// bound to whatever single same-family project symbol shared the name —
/// every `use std::fs;` in the repo produced a phantom `imports → fn fs`
/// edge onto a `#[cfg(test)]` helper in `js_modules.rs`, polluting 4
/// module_dependencies pairs in `map` (one of them 100% phantom). A std
/// import can never resolve to a project symbol, so no edge (not even an
/// `<external>` sentinel) beats a plausible-but-wrong one. Non-std external
/// crates (`anyhow`, …) can't be told apart from workspace-sibling crates
/// without Cargo.toml knowledge, so they still take the bare-name path.
pub(super) fn extract_rust_use_imports(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
    results: &mut Vec<ParsedRelation>,
) {
    const EXTERNAL_ROOTS: &[&str] = &["std", "core", "alloc", "proc_macro"];
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            if EXTERNAL_ROOTS.contains(&use_path_root(&child, source)) {
                return;
            }
        }
    }
    fn collect_use_names(node: &tree_sitter::Node, source: &str, names: &mut Vec<String>) {
        collect_use_names_inner(node, source, names, 0);
    }
    fn collect_use_names_inner(
        node: &tree_sitter::Node,
        source: &str,
        names: &mut Vec<String>,
        depth: usize,
    ) {
        if depth > MAX_SUBTREE_DEPTH {
            return;
        }
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
pub(super) fn extract_rust_impl_trait(
    node: &tree_sitter::Node,
    source: &str,
) -> Option<ParsedRelation> {
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
    let type_name = type_text
        .split('<')
        .next()
        .unwrap_or(&type_text)
        .trim()
        .to_string();
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
/// positive. `self`/`Self`/glob `*`/inferred-placeholder `_` names are skipped.
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
        // Macro path (`tracing::error!`, `serde_json::json!`): the `macro` field of
        // a `macro_invocation`. The macro name tail is not a value reference — it
        // collides with same-named fns (`error`, `warn`).
        "macro_invocation" => return None,
        // Intermediate path segment of a longer `a::b::c` chain.
        "scoped_identifier" => return None,
        // Type-position path (struct-expr type, generic bounds, etc.). The
        // type name is already a `calls` edge; intermediate segments here are
        // module path, not a value reference.
        "scoped_type_identifier" => return None,
        _ => {}
    }
    // Type-associated path used as a value (`String::as_str`, `MyType::method` passed
    // as a fn pointer): the bare tail cannot resolve to the correct associated item
    // (std methods aren't indexed; a same-named local fn is the wrong target), so
    // suppress. Heuristic: a PascalCase head segment is a type; lowercase heads
    // (`crate`, `self`, `super`, snake_case module names like `domain`) are genuine
    // module-path values and still emit. Lowercase PRIMITIVE-type heads (`str::trim`,
    // `u32::MAX`) are also type-associated and are caught by the primitive list below.
    let path_text = node_text(node, source);
    if path_text
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
    {
        return None;
    }
    let head = path_text.split("::").next().unwrap_or(path_text);
    if matches!(
        head,
        "str"
            | "bool"
            | "char"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f32"
            | "f64"
    ) {
        return None;
    }
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(&name_node, source);
    // `_` added defensively (inferred-type placeholder) alongside self/Self/glob.
    if name.is_empty() || name == "self" || name == "Self" || name == "*" || name == "_" {
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
/// - `Self`;
/// - `_`, the inferred-type placeholder (`Vec<_>`, `collect::<Vec<_>>()`), which
///   tree-sitter parses as a `type_identifier` but is not a real type usage.
pub(super) fn extract_rust_type_reference(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
) -> Option<ParsedRelation> {
    if let Some(parent) = node.parent() {
        // The `name` field of a definition is the declaration, not a usage.
        if matches!(
            parent.kind(),
            "struct_item" | "enum_item" | "type_item" | "trait_item" | "union_item"
        ) && parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
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
    // `_` is the inferred-type placeholder (`Vec<_>`, `collect::<Vec<_>>()`),
    // which tree-sitter parses as a `type_identifier`. It is not a real type
    // usage; emitting a `references` edge to "_" piles every occurrence onto
    // whatever node happens to be named "_" (e.g. the `const _: () = assert!(..)`
    // guard), inflating its caller/impact counts with phantom edges.
    if name.is_empty() || name == "Self" || name == "_" {
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

/// Emit a `references` edge for a BARE `identifier` used as a function VALUE —
/// a callback / function pointer passed in a call-argument position. Two shapes:
///   - direct argument:  `register(handler)` → identifier parent is `arguments`;
///   - address-of arg:   `signal(&shutdown)` → identifier under `reference_expression`
///     whose parent is `arguments`.
///
/// Self-exclusion is structural: a call's *callee* identifier has parent
/// `call_expression` (the `function` field), never `arguments`, so it never fires
/// here (and remains a `calls` edge). Path-qualified values (`crate::foo::bar`) are
/// `scoped_identifier`, handled by `extract_rust_path_reference`. `self/Self/_` skip.
///
/// M2 (param exclusion): a bare id equal to a parameter of the enclosing function
/// (or an enclosing closure) is a pass-through of a LOCAL binding, not a reference to
/// a same-named global fn — skip it. Without M2, `fn run(handler: F){ spawn(handler) }`
/// would fabricate an edge to whatever global `handler` happens to exist (FP-a).
/// Calls made inside macro token trees (`assert_eq!(foo(x), y)`, `macro_rules!`
/// rule bodies): tree-sitter parses macro arguments/bodies as opaque
/// `token_tree`s — no `call_expression` exists — so every such call was
/// invisible: the target showed as dead code and impact/callgraph missed the
/// calling fn (field failure 2026-07-24: `impact grep_exit` missed cmd_stats'
/// `sout!` body; tests calling a fn only inside `assert_eq!` were absent).
/// Heuristic: an `identifier` token directly followed by a `(…)` token_tree is
/// a call. Exclusions:
///   - previous token `.` → method tail; the receiver is unrecoverable in a
///     token soup, and a bare-name edge would alias every same-named fn
///   - previous token `::` → path tail (v1 skips: std/type-associated paths
///     dominate and the bare tail aliases same-named local fns)
///   - previous token `$` → macro fragment variable, not a name
///   - previous token is a definition keyword → macro-generated item, not a call
///   - no enclosing named scope → skip, parity with the call_expression arm's
///     deliberate Rust top-level omission
///
/// A macro name itself never matches: `foo!(…)` puts a `!` between the
/// identifier and the token_tree.
pub(super) fn extract_rust_macro_token_call(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
) -> Option<ParsedRelation> {
    let scope = scope?;
    let parent = node.parent()?;
    if parent.kind() != "token_tree" {
        return None;
    }
    let next = node.next_sibling()?;
    if next.kind() != "token_tree" || source.as_bytes().get(next.start_byte()) != Some(&b'(') {
        return None;
    }
    if let Some(prev) = node.prev_sibling() {
        if matches!(
            prev.kind(),
            "." | "::"
                | "$"
                | "fn"
                | "struct"
                | "enum"
                | "union"
                | "trait"
                | "mod"
                | "type"
                | "impl"
        ) {
            return None;
        }
    }
    let name = node_text(node, source);
    if name.is_empty() || name == "self" || name == "Self" || name == "_" {
        return None;
    }
    // Tuple-variant/tuple-struct PATTERNS (`matches!(x, Some(y))`) parse
    // identically to calls in a token soup — token_tree carries no
    // pattern-vs-expression split (audit 2026-07-24, empirically reproduced:
    // `matches!(x, Some(y))` fabricated a calls→Some edge). Variant and type
    // names are CamelCase by convention (non_camel_case_types lint), while the
    // fn calls this pass exists to recover are snake_case — so skip
    // uppercase-initial names. Cost: constructor uses like `vec![Some(1)]`
    // stay invisible, exactly as they were before this pass existed.
    if name.chars().next().is_some_and(|c| c.is_uppercase()) {
        return None;
    }
    Some(ParsedRelation {
        source_name: scope.to_string(),
        target_name: name.to_string(),
        relation: REL_CALLS.into(),
        metadata: None,
        source_language: String::new(),
    })
}

pub(super) fn extract_rust_value_reference(
    node: &tree_sitter::Node,
    source: &str,
    scope: Option<&str>,
) -> Option<ParsedRelation> {
    let parent = node.parent()?;
    let in_value_position = match parent.kind() {
        // Phase 1: call argument, or `&fn` argument.
        "arguments" => true,
        "reference_expression" => parent
            .parent()
            .map(|gp| gp.kind() == "arguments")
            .unwrap_or(false),
        // Phase 2: binding RHS (`let cb = handler`) — only the `value` field, never
        // the `pattern` (which is a local binding, handled by M2.5).
        "let_declaration" => parent.child_by_field_name("value").map(|v| v.id()) == Some(node.id()),
        // Phase 2: struct field value (`Config { cb: handler }`) — `value` field
        // only, never the `field` name.
        "field_initializer" => {
            parent.child_by_field_name("value").map(|v| v.id()) == Some(node.id())
        }
        // Phase 2: explicit `return handler;`.
        "return_expression" => true,
        // Phase 2: tail expression of a block (`fn f() -> F { handler }`) — the last
        // named child with no trailing `;` (a `;` would reparent to
        // expression_statement). M2.5 still excludes a tail that is a local.
        "block" => {
            let n = parent.named_child_count();
            n > 0 && parent.named_child(n - 1).map(|c| c.id()) == Some(node.id())
        }
        _ => false,
    };
    if !in_value_position {
        return None;
    }
    let name = node_text(node, source);
    if name.is_empty() || name == "self" || name == "Self" || name == "_" {
        return None;
    }
    if enclosing_fn_local_names(node, source).contains(name) {
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

/// Collect LOCAL binding names visible to a value-reference candidate so M2/M2.5
/// can suppress them — a bare id equal to a local is a pass-through of that local,
/// not a reference to a same-named global fn. Collects:
///   - parameters of the nearest enclosing `function_item` + any enclosing closures
///     (M2 — `type_identifier` types are excluded since only `identifier` nodes are
///     gathered; generic `<F>` params live in `type_parameters`, not collected);
///   - `let` binding names in the nearest function body (M2.5) — gathered from the
///     `let_declaration` `pattern` field ONLY (never the RHS `value`), so
///     `let db = open()` contributes `db` but not `open`. This kills the dominant
///     Phase-1 false positive: `let db = open(); run(&db)` where an accessor fn/
///     method `db` also exists (`conn`, `db`, `picks` … measured in dogfooding).
///
/// Rust `let` scope is function-local (nested fns don't capture), so the nearest
/// `function_item` is the correct boundary.
fn enclosing_fn_local_names(
    node: &tree_sitter::Node,
    source: &str,
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let mut cur = node.parent();
    while let Some(n) = cur {
        match n.kind() {
            "closure_expression" => {
                if let Some(p) = n.child_by_field_name("parameters") {
                    collect_param_idents(&p, source, &mut names, 0);
                }
            }
            "function_item" => {
                if let Some(p) = n.child_by_field_name("parameters") {
                    collect_param_idents(&p, source, &mut names, 0);
                }
                if let Some(body) = n.child_by_field_name("body") {
                    collect_rust_binding_names(&body, source, &mut names, 0);
                }
                break;
            }
            _ => {}
        }
        cur = n.parent();
    }
    names
}

/// Walk a function body collecting binding names from every pattern-introducing
/// node's `pattern` field (not RHS values / scrutinees), so M2.5 suppresses bare
/// ids that are locals rather than global-fn references:
///   - `let_declaration`  (`let db = …`)
///   - `let_condition`    (`if let Some(node) = …` / `while let`)
///   - `match_arm`        (`Ok(val) => …`, `Err(error) => …`)
///   - `for_expression`   (`for item in …`)
///
/// Collecting from the `pattern` field gathers binding idents (and harmlessly any
/// enum-variant/const names in the pattern — over-collection is precision-safe).
/// Recurses to reach bindings in nested blocks. Used by `enclosing_fn_local_names`.
fn collect_rust_binding_names(
    node: &tree_sitter::Node,
    source: &str,
    out: &mut std::collections::HashSet<String>,
    depth: usize,
) {
    if depth > MAX_SUBTREE_DEPTH {
        return;
    }
    if matches!(
        node.kind(),
        "let_declaration" | "let_condition" | "match_arm" | "for_expression"
    ) {
        if let Some(pat) = node.child_by_field_name("pattern") {
            collect_param_idents(&pat, source, out, 0);
        }
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            collect_rust_binding_names(&child, source, out, depth + 1);
        }
    }
}

fn collect_param_idents(
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
            collect_param_idents(&child, source, out, depth + 1);
        }
    }
}
