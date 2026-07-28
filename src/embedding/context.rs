pub struct NodeContext {
    pub node_type: String,
    pub name: String,
    pub qualified_name: Option<String>,
    pub file_path: String,
    pub language: Option<String>,
    pub signature: Option<String>,
    pub return_type: Option<String>,
    pub param_types: Option<String>,
    pub code_content: Option<String>,
    pub routes: Vec<String>,
    pub callees: Vec<String>,
    pub callers: Vec<String>,
    pub inherits: Vec<String>,
    pub imports: Vec<String>,
    pub implements: Vec<String>,
    pub exports: Vec<String>,
    pub doc_comment: Option<String>,
}

/// Per-list cap on the graph-relation lines. Applies to EVERY relation list:
/// `inherits`/`imports`/`implements`/`exports` used to join all of them while
/// `routes`/`callees`/`callers` capped, and those four are emitted BEFORE the
/// doc comment and the code body — so one wide list (a barrel file importing 200
/// names is ordinary) evicted the symbol's own doc and body from the 512-token
/// window, inverting the priority order this function exists to enforce.
const MAX_RELATIONS: usize = 10;

/// `label: a, b, c (+N)` — first [`MAX_RELATIONS`] entries plus the dropped
/// count. The `(+N)` suffix is not decoration: a silently truncated list reads as
/// a complete one, both to a model consuming the embedding text and to anyone
/// debugging recall.
fn relation_line(label: &str, items: &[String]) -> String {
    let suffix = if items.len() > MAX_RELATIONS {
        format!(" (+{})", items.len() - MAX_RELATIONS)
    } else {
        String::new()
    };
    format!(
        "{}: {}{}",
        label,
        items
            .iter()
            .take(MAX_RELATIONS)
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
        suffix
    )
}

pub fn build_context_string(info: &NodeContext) -> String {
    let mut parts = Vec::new();

    // Priority order optimized for embedding models with 512-token limits:
    // High-value structural signals first, code content last (most likely to be truncated).

    // 1. Signature (always short, high value for search matching)
    if let Some(sig) = &info.signature {
        parts.push(format!("signature: {}", sig));
    }

    // 2. Type information (high value for structural search)
    if let Some(rt) = &info.return_type {
        if !rt.is_empty() {
            parts.push(format!("returns: {}", rt));
        }
    }
    if let Some(pt) = &info.param_types {
        if !pt.is_empty() {
            parts.push(format!("params: {}", pt));
        }
    }

    // 3. Identity: type + name + file (critical for disambiguation)
    let display_name = info.qualified_name.as_deref().unwrap_or(&info.name);
    parts.push(format!("{} {}", info.node_type, display_name));
    if let Some(lang) = &info.language {
        parts.push(format!("{} in {}", lang, info.file_path));
    } else {
        parts.push(format!("in {}", info.file_path));
    }

    // 4. Graph relations (structural signals that survive truncation)
    for (label, items) in [
        ("routes", &info.routes),
        ("calls", &info.callees),
        ("called_by", &info.callers),
        ("inherits", &info.inherits),
        ("imports", &info.imports),
        ("implements", &info.implements),
        ("exports", &info.exports),
    ] {
        if !items.is_empty() {
            parts.push(relation_line(label, items));
        }
    }

    // 5. Doc comment (medium priority — often short enough to survive)
    if let Some(doc) = &info.doc_comment {
        parts.push(format!("doc: {}", doc));
    }

    // 6. Code content last (most likely to be truncated at 512 tokens, least loss)
    if let Some(code) = &info.code_content {
        parts.push(format!("code: {}", code));
    }

    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_context_string() {
        let info = NodeContext {
            node_type: "function".into(),
            name: "validateToken".into(),
            qualified_name: None,
            file_path: "src/auth/middleware.ts".into(),
            language: Some("typescript".into()),
            signature: Some("(token: string) -> Promise<User | null>".into()),
            return_type: Some("Promise<User | null>".into()),
            param_types: Some("(token: string)".into()),
            code_content: Some(
                "function validateToken(token: string) { return jwt.verify(token); }".into(),
            ),
            routes: vec!["POST /api/login".into(), "GET /api/profile".into()],
            callees: vec!["jwt.verify".into(), "UserRepo.findById".into()],
            callers: vec!["authMiddleware".into(), "handleLogin".into()],
            inherits: vec![],
            imports: vec!["jwt".into(), "UserRepo".into()],
            implements: vec![],
            exports: vec![],
            doc_comment: Some("Validates JWT token and returns the associated user".into()),
        };

        let ctx = build_context_string(&info);
        assert!(ctx.contains("function validateToken"));
        assert!(ctx.contains("typescript in src/auth/middleware.ts"));
        assert!(ctx.contains("returns: Promise<User | null>"));
        assert!(ctx.contains("params: (token: string)"));
        assert!(ctx.contains("calls: jwt.verify, UserRepo.findById"));
        assert!(ctx.contains("called_by: authMiddleware, handleLogin"));
        assert!(ctx.contains("routes: POST /api/login, GET /api/profile"));
        assert!(ctx.contains("imports: jwt, UserRepo"));
        assert!(ctx.contains("code: function validateToken(token: string)"));
    }

    #[test]
    fn test_context_string_code_before_graph() {
        let info = NodeContext {
            node_type: "function".into(),
            name: "handler".into(),
            qualified_name: None,
            file_path: "api.ts".into(),
            language: None,
            signature: Some("(req: Request) -> Response".into()),
            return_type: Some("Response".into()),
            param_types: Some("(req: Request)".into()),
            code_content: Some("function handler(req: Request) { return ok(); }".into()),
            routes: vec![],
            callees: vec!["ok".into()],
            callers: vec!["router".into()],
            inherits: vec![],
            imports: vec![],
            implements: vec![],
            exports: vec![],
            doc_comment: Some("Handles requests".into()),
        };
        let ctx = build_context_string(&info);
        let sig_pos = ctx.find("signature:").unwrap();
        let identity_pos = ctx.find("function handler").unwrap();
        let calls_pos = ctx.find("calls:").unwrap();
        let code_pos = ctx.find("code:").unwrap();
        // Priority: signature → identity → graph relations → doc → code (code last, truncation-safe)
        assert!(sig_pos < identity_pos, "signature before identity");
        assert!(identity_pos < calls_pos, "identity before calls");
        assert!(
            calls_pos < code_pos,
            "calls before code (code last for truncation safety)"
        );
    }

    #[test]
    fn test_build_context_string_minimal() {
        let info = NodeContext {
            node_type: "function".into(),
            name: "helper".into(),
            qualified_name: None,
            file_path: "utils.ts".into(),
            language: None,
            signature: None,
            return_type: None,
            param_types: None,
            code_content: None,
            routes: vec![],
            callees: vec![],
            callers: vec![],
            inherits: vec![],
            imports: vec![],
            implements: vec![],
            exports: vec![],
            doc_comment: None,
        };

        let ctx = build_context_string(&info);
        assert!(ctx.contains("function helper"));
        assert!(ctx.contains("in utils.ts"));
        assert!(!ctx.contains("calls:"));
        assert!(!ctx.contains("routes:"));
    }

    #[test]
    fn test_relation_lists_are_capped_symmetrically() {
        // callees/callers/routes capped at MAX_RELATIONS; inherits/imports/
        // implements/exports joined ALL of them. Those four are emitted BEFORE
        // doc and code in the priority order, so an unbounded list (a barrel
        // file importing 200 names is ordinary) pushes the symbol's own doc and
        // body out of the 512-token embedding window entirely — the very
        // truncation the ordering exists to control.
        let many = |prefix: &str| (0..25).map(|i| format!("{prefix}{i}")).collect::<Vec<_>>();
        let info = NodeContext {
            node_type: "class".into(),
            name: "Wide".into(),
            qualified_name: None,
            file_path: "src/wide.ts".into(),
            language: Some("typescript".into()),
            signature: None,
            return_type: None,
            param_types: None,
            code_content: Some("class Wide {}".into()),
            routes: many("route"),
            callees: many("callee"),
            callers: many("caller"),
            inherits: many("base"),
            imports: many("imp"),
            implements: many("iface"),
            exports: many("exp"),
            doc_comment: Some("Wide type.".into()),
        };
        let ctx = build_context_string(&info);
        for (label, prefix) in [
            ("routes", "route"),
            ("calls", "callee"),
            ("called_by", "caller"),
            ("inherits", "base"),
            ("imports", "imp"),
            ("implements", "iface"),
            ("exports", "exp"),
        ] {
            assert!(
                ctx.contains(&format!("{label}: ")),
                "{label} line missing from: {ctx}"
            );
            assert!(
                !ctx.contains(&format!("{prefix}10")),
                "{label} must stop at 10 entries, but {prefix}10 is present: {ctx}"
            );
            assert!(
                ctx.contains(&format!("{prefix}9")),
                "{label} must keep the first 10 entries, {prefix}9 missing: {ctx}"
            );
        }
        // No silent caps: every truncated list discloses how many it dropped.
        assert_eq!(
            ctx.matches("(+15)").count(),
            7,
            "each of the 7 relation lists must disclose its dropped count: {ctx}"
        );
        // The whole point — doc and code still make it into the string.
        assert!(ctx.contains("doc: Wide type."), "doc dropped: {ctx}");
        assert!(ctx.contains("code: class Wide {}"), "code dropped: {ctx}");
    }
}
