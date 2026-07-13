//! Edge-resolution coverage baseline. A per-language edge-count drop here flags a
//! silent edge-resolution regression (the class of bug that has historically shipped
//! undetected: method→sibling-method drops, value-reference floods, qualifier loss).
//! Update the baselines deliberately when a change is a real improvement.
use std::collections::BTreeMap;
use tempfile::TempDir;

use code_graph_mcp::graph::routes::get_callers_with_route_info;
use code_graph_mcp::storage::db::Database;
use code_graph_mcp::storage::queries;

/// Index a fixed multi-language fixture, returning the project dir and an open DB.
/// Keep both bound in tests: dropping the TempDir wipes the index.
fn index_fixture() -> (TempDir, Database) {
    let project = TempDir::new().unwrap();
    let src = project.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    // TypeScript: class with two methods, one calling the sibling (intra-class call).
    std::fs::write(src.join("svc.ts"), r#"
export class Svc {
    handle(x: number): number { return this.helper(x); }
    helper(x: number): number { return x + 1; }
}
"#).unwrap();

    // Python: same intra-class sibling call.
    std::fs::write(src.join("svc.py"), r#"
class Svc:
    def handle(self, x):
        return self.helper(x)
    def helper(self, x):
        return x + 1
"#).unwrap();

    // Rust: same-file function call.
    std::fs::write(src.join("lib.rs"), r#"
pub fn helper(x: i32) -> i32 { x + 1 }
pub fn handle(x: i32) -> i32 { helper(x) }
"#).unwrap();

    let db_dir = project.path().join(code_graph_mcp::domain::CODE_GRAPH_DIR);
    std::fs::create_dir_all(&db_dir).unwrap();
    let db = Database::open(&db_dir.join("index.db")).unwrap();
    code_graph_mcp::indexer::pipeline::run_full_index(&db, project.path(), None, None).unwrap();
    (project, db)
}

fn edge_counts(db: &Database) -> BTreeMap<String, BTreeMap<String, i64>> {
    queries::resolution_stats(db.conn()).unwrap().edges_by_language
}

#[test]
fn edge_coverage_per_language_baseline() {
    let (_p, db) = index_fixture();
    let by_lang = edge_counts(&db);
    // Lower-bound baselines: each language must produce at least these call edges.
    // Raise deliberately when extraction genuinely improves.
    let calls = |lang: &str| by_lang.get(lang).and_then(|m| m.get("calls")).copied().unwrap_or(0);
    assert!(calls("typescript") >= 1, "TS calls regressed: {by_lang:?}");
    assert!(calls("python") >= 1, "Python calls regressed: {by_lang:?}");
    assert!(calls("rust") >= 1, "Rust calls regressed: {by_lang:?}");
}

#[test]
fn c_include_resolves_to_indexed_header_module() {
    // A C/C++ `#include "widget.h"` must resolve to the indexed header's <module>
    // node (an IMPORTS edge), mirroring PHP require / JS require. Before the fix
    // the include emitted only a bare stem with NO path metadata, so it fell to
    // `<external>/widget` and deps/cycles/affected/project_map under-reported the
    // local header dependency (M6). INDEX_VERSION 45→46.
    let project = TempDir::new().unwrap();
    let src = project.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("widget.h"), "int widget_add(int a, int b);\n").unwrap();
    std::fs::write(
        src.join("widget.cpp"),
        "#include \"widget.h\"\nint widget_add(int a, int b) { return a + b; }\n",
    ).unwrap();

    let db_dir = project.path().join(code_graph_mcp::domain::CODE_GRAPH_DIR);
    std::fs::create_dir_all(&db_dir).unwrap();
    let db = Database::open(&db_dir.join("index.db")).unwrap();
    code_graph_mcp::indexer::pipeline::run_full_index(&db, project.path(), None, None).unwrap();

    let conn = db.conn();
    let module_id = |path: &str| -> i64 {
        conn.query_row(
            "SELECT n.id FROM nodes n JOIN files f ON f.id = n.file_id
             WHERE n.name = '<module>' AND f.path = ?1",
            [path], |r| r.get::<_, i64>(0),
        ).unwrap_or(-1)
    };
    let cpp_mod = module_id("src/widget.cpp");
    let h_mod = module_id("src/widget.h");
    assert!(cpp_mod > 0 && h_mod > 0, "both <module> nodes must exist (cpp={cpp_mod}, h={h_mod})");

    let has_edge: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM edges WHERE source_id=?1 AND target_id=?2 AND relation='imports')",
        [cpp_mod, h_mod], |r| r.get(0),
    ).unwrap();
    assert!(
        has_edge,
        "widget.cpp #include \"widget.h\" must produce an IMPORTS edge to widget.h's <module>",
    );
}

#[test]
fn import_extraction_parity_across_full_languages() {
    // META②: every full-extraction language must emit a REL_IMPORTS edge for its
    // canonical import form. walk_for_relations (src/parser/relations/mod.rs) has
    // one arm per (language, relation); a silently missing arm is the H2/M5/M6
    // sibling-hole class this test locks against. All six full-extraction
    // languages are checked in one table so a future per-language addition (or
    // regression) can't slip through unnoticed.
    //
    // None of the import targets below need a real importee file indexed: an
    // import that doesn't resolve to a local file/symbol still produces a
    // REL_IMPORTS edge to an `<external>/<name>` sentinel node (Phase 2b/2b-ext
    // in src/indexer/pipeline/index_files.rs) — the same fallback the
    // `c_include_resolves_to_indexed_header_module` test's C++ case would hit for
    // a system header. So the plain canonical form is enough to prove the arm
    // fires; no per-language importee scaffolding is required.
    //
    // Negative control: deleting a language's import arm in
    // src/parser/relations/mod.rs (e.g. the Java `import_declaration` arm added
    // by H2) makes exactly that row's assertion fail while the others stay green.
    let cases: &[(&str, &str, &str)] = &[
        ("typescript", "a.ts", "import { x } from './b';\n"),
        ("javascript", "a.js", "const x = require('./b');\n"),
        ("go", "a.go", "package main\nimport \"fmt\"\n"),
        ("python", "a.py", "import os\n"),
        ("rust", "a.rs", "use std::fmt;\n"),
        ("java", "A.java", "import java.util.List;\n"),
    ];

    let project = TempDir::new().unwrap();
    let src = project.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for (_, file, source) in cases {
        std::fs::write(src.join(file), source).unwrap();
    }

    let db_dir = project.path().join(code_graph_mcp::domain::CODE_GRAPH_DIR);
    std::fs::create_dir_all(&db_dir).unwrap();
    let db = Database::open(&db_dir.join("index.db")).unwrap();
    code_graph_mcp::indexer::pipeline::run_full_index(&db, project.path(), None, None).unwrap();

    let by_lang = edge_counts(&db);
    for (lang, file, source) in cases {
        let imports = by_lang.get(*lang).and_then(|m| m.get("imports")).copied().unwrap_or(0);
        assert!(
            imports >= 1,
            "{lang}: expected a REL_IMPORTS edge from {file} (`{source:?}`) but found none; edges_by_language={by_lang:?}"
        );
    }
}

#[test]
fn edge_coverage_intra_class_method_call_resolves() {
    // Guards the method→sibling-method drop class (method_call_edge_drops, fixed v16).
    // Scope per-file so a Rust same-file call cannot mask a TS/Python OO regression.
    let (_p, db) = index_fixture();
    for file in ["src/svc.ts", "src/svc.py"] {
        let callers = get_callers_with_route_info(db.conn(), "helper", Some(file), 3, 0).unwrap();
        assert!(
            callers.iter().any(|c| c.name == "handle"),
            "intra-class call handle→helper must resolve in {file}; got {:?}",
            callers.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }
}
