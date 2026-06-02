//! End-to-end tests for the bare-name call qualifier resolver rules.
//! See docs/superpowers/specs/2026-05-11-bare-name-call-qualifier-design.md.

use code_graph_mcp::indexer::pipeline::run_full_index;
use code_graph_mcp::storage::db::Database;
use std::fs;
use tempfile::TempDir;

fn write(dir: &std::path::Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&p, content).unwrap();
}

fn callers_of(db: &Database, target_name: &str) -> Vec<String> {
    let mut stmt = db.conn().prepare(
        "SELECT COALESCE(src.qualified_name, src.name) FROM edges e
         JOIN nodes tgt ON tgt.id = e.target_id
         JOIN nodes src ON src.id = e.source_id
         WHERE e.relation = 'calls' AND tgt.name = ?"
    ).unwrap();
    let rows = stmt.query_map([target_name], |r| r.get::<_, String>(0)).unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

fn callers_of_in_file(db: &Database, target_name: &str, file_rel: &str) -> Vec<String> {
    let mut stmt = db.conn().prepare(
        "SELECT COALESCE(src.qualified_name, src.name) FROM edges e
         JOIN nodes tgt ON tgt.id = e.target_id
         JOIN nodes src ON src.id = e.source_id
         JOIN files f ON f.id = tgt.file_id
         WHERE e.relation = 'calls' AND tgt.name = ? AND f.path = ?"
    ).unwrap();
    let rows = stmt.query_map([target_name, file_rel], |r| r.get::<_, String>(0)).unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

#[test]
fn chain_builder_drops_intermediate_callers() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Project has a function literally named `create` in src/snapshot/mod.rs.
    write(root, "src/snapshot/mod.rs", "pub fn create() {}\n");
    // Caller does a builder chain — `.create(true)` is a method on OpenOptions,
    // NOT the project's snapshot::create.
    write(root, "src/caller.rs", r#"
        use std::fs::OpenOptions;
        pub fn caller() {
            OpenOptions::new().create(true).open("/tmp/x").ok();
        }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let callers = callers_of(&db, "create");
    assert!(
        !callers.iter().any(|c| c.contains("caller")),
        "snapshot::create must NOT have `caller` as caller (it called .create() in a builder chain), got: {:?}",
        callers
    );
}

#[test]
fn bare_name_qualifier_drops_phantom_callers_for_file_create() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Project has snapshot::create.
    write(root, "src/snapshot/mod.rs", "pub fn create() {}\n");
    // Caller calls std::fs::File::create — Path qualifier with first segment
    // "File" which is NOT a project module → drop.
    write(root, "src/caller.rs", r#"
        use std::fs::File;
        pub fn caller() { let _ = File::create("/tmp/x"); }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let callers = callers_of(&db, "create");
    assert!(
        !callers.iter().any(|c| c.contains("caller")),
        "snapshot::create must NOT have `caller` (caller called std::fs::File::create), got: {:?}",
        callers
    );
}

#[test]
fn path_qualifier_picks_module_specific_candidate() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Two project modules each with a `create` fn.
    write(root, "src/snapshot/mod.rs", "pub fn create() {}\n");
    write(root, "src/builder/mod.rs", "pub fn create() {}\n");
    // Caller explicitly targets snapshot::create.
    write(root, "src/caller.rs", r#"
        pub fn caller() { crate::snapshot::create(); }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    // snapshot::create gets the caller; builder::create does not.
    let snap = callers_of_in_file(&db, "create", "src/snapshot/mod.rs");
    let bld = callers_of_in_file(&db, "create", "src/builder/mod.rs");

    assert!(snap.iter().any(|c| c.contains("caller")),
        "snapshot::create should have caller, got: {:?}", snap);
    assert!(!bld.iter().any(|c| c.contains("caller")),
        "builder::create should NOT have caller (qualifier was snapshot), got: {:?}", bld);
}

#[test]
fn self_method_within_impl_uses_correct_type() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    write(root, "src/db.rs", r#"
        pub struct Db;
        impl Db {
            pub fn caller(&self) { self.helper(); }
            pub fn helper(&self) {}
        }
    "#);
    // Sibling type with same-named method — must NOT win.
    write(root, "src/other.rs", r#"
        pub struct Other;
        impl Other {
            pub fn helper(&self) {}
        }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let db_helper = callers_of_in_file(&db, "helper", "src/db.rs");
    let other_helper = callers_of_in_file(&db, "helper", "src/other.rs");

    assert!(db_helper.iter().any(|c| c.contains("caller")),
        "Db::helper should have Db::caller, got: {:?}", db_helper);
    assert!(!other_helper.iter().any(|c| c.contains("caller")),
        "Other::helper should NOT have Db::caller, got: {:?}", other_helper);
}

#[test]
fn self_method_resolves_across_split_impl_blocks() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Db's caller is in db_a.rs; Db's helper is in db_b.rs (impl block split).
    write(root, "src/db_a.rs", r#"
        pub struct Db;
        impl Db {
            pub fn caller(&self) { self.helper(); }
        }
    "#);
    write(root, "src/db_b.rs", r#"
        impl crate::db_a::Db {
            pub fn helper(&self) {}
        }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let helpers = callers_of_in_file(&db, "helper", "src/db_b.rs");
    assert!(helpers.iter().any(|c| c.contains("caller")),
        "Db::helper in db_b.rs should have Db::caller from db_a.rs, got: {:?}",
        helpers);
}

#[test]
fn non_rust_callgraph_unchanged() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // JS file with simple function call — must not be qualifier-filtered.
    write(root, "src/util.js", r#"
        function helper() {}
        function caller() { helper(); }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let mut stmt = db.conn().prepare(
        "SELECT COUNT(*) FROM edges e
         JOIN nodes src ON src.id = e.source_id
         JOIN nodes tgt ON tgt.id = e.target_id
         WHERE e.relation = 'calls'
           AND src.name = 'caller'
           AND tgt.name = 'helper'"
    ).unwrap();
    let count: i64 = stmt.query_row([], |r| r.get(0)).unwrap();
    assert_eq!(count, 1, "JS caller→helper edge must survive (no qualifier filtering for non-Rust)");
}

#[test]
fn path_qualifier_resolves_single_file_rust_mod() {
    // Regression: path_filter_candidates only looked for "/domain/" or
    // "domain/" directory boundaries, so `crate::domain::foo()` resolving to
    // a function in `src/domain.rs` (single-file mod, no directory) silently
    // dropped — every cross-file qualified call into a single-file mod marked
    // the target as dead code. Accept `<last_seg>.rs` suffix too.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/domain.rs", r#"
        pub fn helper_in_domain() -> i32 { 42 }
    "#);
    write(root, "src/main.rs", r#"
        pub fn caller() -> i32 {
            crate::domain::helper_in_domain()
        }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let callers = callers_of(&db, "helper_in_domain");
    assert!(
        callers.iter().any(|c| c == "caller"),
        "caller→crate::domain::helper_in_domain() must resolve when target lives in src/domain.rs (single-file mod); got: {:?}",
        callers
    );
}

#[test]
fn same_file_generic_impl_method_edges_dont_fan_out() {
    // Regression: 3 structs each `impl SameTrait for StructX` in one file used
    // to produce 3×3 = 9 method-edge slots per method name (every struct
    // appeared to implement every same-name method) because Phase 2 resolved
    // bare target_name "run" against all 3 same-name method nodes in the
    // file. Parser now stamps {"q":"impl_method","v":"<Type>"} so the resolver
    // filters method candidates by qualified_name LIKE "<Type>.%".
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/lib.rs", r#"
        pub trait DoWork { fn run(&self); }
        pub struct A;
        impl DoWork for A { fn run(&self) {} }
        pub struct B<T>(T);
        impl<T: Clone> DoWork for B<T> { fn run(&self) {} }
        pub struct C<'a, U>(&'a U);
        impl<'a, U: Default> DoWork for C<'a, U> { fn run(&self) {} }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let mut stmt = db.conn().prepare(
        "SELECT src.name, tgt.qualified_name
         FROM edges e
         JOIN nodes src ON src.id = e.source_id
         JOIN nodes tgt ON tgt.id = e.target_id
         WHERE e.relation = 'implements'
           AND tgt.name = 'run'
         ORDER BY src.name"
    ).unwrap();
    let pairs: Vec<(String, String)> = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

    // Each struct must implement only its own method (3 edges, not 9).
    assert_eq!(pairs.len(), 3,
        "expected one implements edge per (struct, its-own-run) pair; got {:?}", pairs);
    assert!(pairs.contains(&("A".to_string(), "A.run".to_string())),
        "A should implement A.run; got {:?}", pairs);
    assert!(pairs.contains(&("B".to_string(), "B.run".to_string())),
        "B should implement B.run (bare, no <T>); got {:?}", pairs);
    assert!(pairs.contains(&("C".to_string(), "C.run".to_string())),
        "C should implement C.run (bare, no <'a, U>); got {:?}", pairs);
}

#[test]
fn path_qualifier_keeps_same_file_target() {
    // Regression: the Path branch of edge resolution filtered out local_ids
    // (same-file targets) before applying the path filter, contradicting the
    // spec's "same-file matches still take precedence". Net effect: a Rust
    // file with `impl Foo { fn helper() }` and a sibling caller doing
    // `Foo::helper()` produced no call edge — same-file pool was excluded,
    // and the cross-file Path filter (which scans `/Foo/` in the file path)
    // never matched in a single-file project.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/lib.rs", r#"
        pub struct Foo;
        impl Foo {
            pub fn helper() -> i32 { 42 }
        }
        pub fn caller() -> i32 {
            Foo::helper()
        }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let callers = callers_of(&db, "helper");
    assert!(
        callers.iter().any(|c| c == "caller"),
        "caller→Foo::helper() must produce a call edge even when target is in the same file; got: {:?}",
        callers
    );
}

#[test]
fn receiver_call_resolves_unique_method() {
    // `let f = Foo::new(); f.unique_method();` — the receiver `f` has no
    // statically-knowable type at the call site, so the parser stamps a
    // Receiver qualifier. There is EXACTLY ONE same-language method named
    // `unique_method` in the project, and the name is not stdlib noise, so the
    // resolver should bind the call to it (instead of dropping the edge and
    // marking `unique_method` as dead). This is the live-method-dead-code bug:
    // `file_exists` / `validate` were dropped this way.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/lib.rs", r#"
        pub struct Foo;
        impl Foo {
            pub fn new() -> Self { Foo }
            pub fn unique_method(&self) -> i32 { 7 }
        }
        pub fn caller() -> i32 {
            let f = Foo::new();
            f.unique_method()
        }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let callers = callers_of(&db, "unique_method");
    assert!(
        callers.iter().any(|c| c == "caller"),
        "caller→f.unique_method() must resolve when `unique_method` is the unique same-language method; got: {:?}",
        callers
    );
}

#[test]
fn receiver_call_resolves_method_not_free_function_same_name() {
    // The `validate` regression shape: a free function `validate(...)` AND a
    // method `Req::validate(&self)` share the name. A receiver call
    // `req.validate()` can only target a METHOD, never the free function, so
    // the resolver must treat the method as the unique candidate (the free
    // function is excluded by qualified_name having no `.`), bind the edge to
    // the method, and NOT to the free function.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/req.rs", r#"
        pub struct Req;
        impl Req {
            pub fn validate(&self) -> bool { true }
        }
        pub fn use_it() -> bool {
            let r = make_req();
            r.validate()
        }
        fn make_req() -> Req { Req }
    "#);
    // Free function with the SAME bare name in another module — must NOT
    // receive the receiver-call edge.
    write(root, "src/install.rs", r#"
        pub fn validate(path: &str) -> bool { !path.is_empty() }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let method_callers = callers_of_in_file(&db, "validate", "src/req.rs");
    let free_fn_callers = callers_of_in_file(&db, "validate", "src/install.rs");
    assert!(
        method_callers.iter().any(|c| c.contains("use_it")),
        "Req::validate (method) should have `use_it` as caller via r.validate(); got: {:?}",
        method_callers
    );
    assert!(
        !free_fn_callers.iter().any(|c| c.contains("use_it")),
        "free-fn validate must NOT get the receiver-call edge (receiver targets a method); got: {:?}",
        free_fn_callers
    );
}

#[test]
fn receiver_call_with_ambiguous_method_name_stays_unresolved() {
    // NEGATIVE: two DISTINCT structs each define a method `ambiguous`. A
    // receiver call `x.ambiguous()` where `x`'s type is unknown must NOT
    // fan out to both methods — there is no unique same-language method, so
    // the edge is dropped (no false-positive impact inflation).
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/a.rs", r#"
        pub struct A;
        impl A {
            pub fn ambiguous(&self) -> i32 { 1 }
        }
    "#);
    write(root, "src/b.rs", r#"
        pub struct B;
        impl B {
            pub fn ambiguous(&self) -> i32 { 2 }
        }
    "#);
    write(root, "src/caller.rs", r#"
        pub fn caller(x: SomeOpaque) -> i32 {
            x.ambiguous()
        }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let callers = callers_of(&db, "ambiguous");
    assert!(
        !callers.iter().any(|c| c.contains("caller")),
        "ambiguous() defined on two structs must NOT resolve a receiver call to either (no unique target); got: {:?}",
        callers
    );
}

#[test]
fn receiver_call_prefers_same_file_method_over_cross_file_ambiguity() {
    // The `same_file_methods.len() == 1 && methods.len() > 1` branch: the
    // method name `process` is defined on TWO structs in TWO files, so it is
    // NOT globally unique. But the caller lives in the SAME file as struct A's
    // method, so same-file preference resolves the receiver call to A.process
    // and NOT to the cross-file B.process — even though >1 global candidates
    // exist. (Without the same-file branch this would drop as ambiguous; with
    // it, locality breaks the tie toward the in-file method.)
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/a.rs", r#"
        pub struct A;
        impl A {
            pub fn process(&self) -> i32 { 1 }
        }
        pub fn caller(a: A) -> i32 {
            a.process()
        }
    "#);
    write(root, "src/b.rs", r#"
        pub struct B;
        impl B {
            pub fn process(&self) -> i32 { 2 }
        }
    "#);

    let db_path = root.join(".code-graph/graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Database::open(&db_path).unwrap();
    run_full_index(&db, root, None, None).unwrap();

    let a_callers = callers_of_in_file(&db, "process", "src/a.rs");
    let b_callers = callers_of_in_file(&db, "process", "src/b.rs");
    assert!(
        a_callers.iter().any(|c| c.contains("caller")),
        "A::process (same file as caller) should get the receiver-call edge; got: {:?}",
        a_callers
    );
    assert!(
        !b_callers.iter().any(|c| c.contains("caller")),
        "B::process (cross-file) must NOT get the edge — same-file wins the tie; got: {:?}",
        b_callers
    );
}
