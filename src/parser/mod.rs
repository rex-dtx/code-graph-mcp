pub mod lang_config;
pub mod languages;
pub mod treesitter;
pub mod relations;

/// Safely extract the text corresponding to a tree-sitter node from the source string.
/// Returns `""` if the byte range is out of bounds.
pub fn node_text<'a>(node: &tree_sitter::Node, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

/// Recognize an Express/Fastify/Koa-style HTTP route registration call
/// (`app|router|server|fastify.METHOD(path, ...)`) and return its (METHOD, path).
/// Single source of truth for the recognized receiver objects + HTTP-method map,
/// so route-edge extraction (`relations::routes`) and inline-handler node
/// materialization (`treesitter` + the relations walker) can never drift.
pub(crate) fn express_route_method_path(
    call: &tree_sitter::Node,
    source: &str,
) -> Option<(&'static str, String)> {
    let function = call.child_by_field_name("function")?;
    if function.kind() != "member_expression" {
        return None;
    }
    let object = function.child_by_field_name("object")?;
    let property = function.child_by_field_name("property")?;
    if !matches!(node_text(&object, source), "app" | "router" | "server" | "fastify") {
        return None;
    }
    let method = match node_text(&property, source) {
        "get" => "GET",
        "post" => "POST",
        "put" => "PUT",
        "delete" => "DELETE",
        "patch" => "PATCH",
        "use" => "USE",
        _ => return None,
    };
    let args = call.child_by_field_name("arguments")?;
    let first = args.named_child(0)?;
    let path = node_text(&first, source)
        .trim_matches(|c| c == '\'' || c == '"')
        .to_string();
    Some((method, path))
}

/// Build the resolution-stable node name for an inline route handler from its
/// method + path, e.g. ("GET", "/api/users") → "GET /api/users". Returns None
/// when `path` isn't a concrete route path, so callers keep the legacy
/// `<module>` attribution instead of emitting an edge to a handler node the
/// materialization step won't create.
pub(crate) fn synthetic_route_handler_name(method: &str, path: &str) -> Option<String> {
    let path = path.trim_matches(|c| c == '\'' || c == '"');
    if !path.starts_with('/') {
        return None;
    }
    Some(format!("{} {}", method, path))
}

/// If `node` is the inline arrow / function-expression handler of an HTTP route
/// registration (the LAST argument of an `app|router|server|fastify.METHOD(path,
/// ...)` call), return the synthetic handler node name "METHOD path". Used by the
/// node extractor (Phase 1) and the relations walker (Phase 2) so the handler
/// node, its scoped calls, and the routes_to edge all share one name.
pub(crate) fn route_handler_name(node: &tree_sitter::Node, source: &str) -> Option<String> {
    if !matches!(node.kind(), "arrow_function" | "function_expression" | "function") {
        return None;
    }
    let args = node.parent().filter(|p| p.kind() == "arguments")?;
    let n = args.named_child_count();
    if n < 2 {
        return None; // need at least (path, handler)
    }
    // Must be the LAST argument — the handler, not a middleware arrow.
    if args.named_child(n - 1)?.id() != node.id() {
        return None;
    }
    let call = args.parent().filter(|p| p.kind() == "call_expression")?;
    let (method, path) = express_route_method_path(&call, source)?;
    synthetic_route_handler_name(method, &path)
}
