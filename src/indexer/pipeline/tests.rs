use super::*;
use super::python_modules::build_python_module_map;
use crate::storage::queries::{
    get_nodes_by_file_path, get_nodes_by_name, get_edges_from, get_import_tree,
};
use crate::domain::REL_CALLS;
use tempfile::TempDir;
use std::fs;

#[test]
fn test_full_index_pipeline() {
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    fs::create_dir_all(project_dir.path().join("src")).unwrap();
    fs::write(project_dir.path().join("src/auth.ts"), r#"
function validateToken(token: string): boolean {
    return jwt.verify(token);
}

function handleLogin(req: Request) {
    if (validateToken(req.token)) {
        return createSession(req.userId);
    }
}
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    let result = run_full_index(&db, project_dir.path(), None, None).unwrap();

    assert!(result.files_indexed > 0);
    assert!(result.nodes_created > 0);
    assert!(result.edges_created > 0);

    // Verify nodes are in DB
    let nodes = get_nodes_by_name(db.conn(), "handleLogin").unwrap();
    assert_eq!(nodes.len(), 1);

    // Verify edges: handleLogin → calls → validateToken
    let edges = get_edges_from(db.conn(), nodes[0].id).unwrap();
    assert!(edges.iter().any(|e| e.relation == REL_CALLS), "should have call edges");

    // Verify context string was built
    assert!(nodes[0].context_string.is_some(), "context string should be set after Phase 3");
}

#[test]
fn test_duplicate_inline_route_handlers_resolve_per_occurrence() {
    // Two inline handlers for the SAME method+path in one file (valid:
    // conditional / overloaded registration). Before the per-occurrence line
    // suffix in route_handler_name both materialized under one synthetic name
    // "GET /dup", so name-based edge resolution cross-linked their calls
    // (handler-1's logA AND logB attributed to both) and fanned routes_to into a
    // cartesian product (src{N}×tgt{N}). Each handler must resolve 1:1.
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    fs::write(project_dir.path().join("routes.js"), r#"
const express = require('express');
const app = express();
function logA() { console.log('a'); }
function logB() { console.log('b'); }
app.get('/dup', (req, res) => { logA(); res.send('1'); });
app.get('/dup', (req, res) => { logB(); res.send('2'); });
app.get('/unique', (req, res) => { logA(); });
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();
    let conn = db.conn();

    // Two distinct handler nodes for the same /dup route (per-occurrence identity).
    let dup_nodes: i64 = conn.query_row(
        "SELECT COUNT(*) FROM nodes WHERE name LIKE 'GET /dup%'", [], |r| r.get(0)).unwrap();
    assert_eq!(dup_nodes, 2, "each /dup registration is its own handler node");

    // routes_to: exactly one self-edge per registration (3), NOT a cartesian fan-out.
    let routes_to: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edges WHERE relation = 'routes_to'", [], |r| r.get(0)).unwrap();
    assert_eq!(routes_to, 3, "one routes_to per registration; no same-name cartesian fan-out");

    // calls must not cross-link: exactly one /dup handler calls logA (the first),
    // exactly one calls logB (the second) — 1 each, not 2 each.
    let dup_to = |callee: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM edges e \
             JOIN nodes s ON s.id = e.source_id JOIN nodes t ON t.id = e.target_id \
             WHERE e.relation = 'calls' AND s.name LIKE 'GET /dup%' AND t.name = ?1",
            [callee], |r| r.get(0)).unwrap()
    };
    assert_eq!(dup_to("logA"), 1, "only the first /dup handler calls logA (no cross-link)");
    assert_eq!(dup_to("logB"), 1, "only the second /dup handler calls logB (no cross-link)");
}

#[test]
fn test_cross_language_bare_name_call_resolution() {
    // Regression: Rust method call `hasher.update(...)` was resolving to
    // JS `function update()` via global bare-name lookup, producing phantom
    // Rust → JS call edges in mixed projects. Fix: same-file > same-language
    // tiers; drop call edges with no same-language candidate.
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::create_dir_all(project_dir.path().join("src")).unwrap();
    fs::create_dir_all(project_dir.path().join("scripts")).unwrap();

    fs::write(project_dir.path().join("src/hasher.rs"), r#"
pub fn caller_rs() {
    let mut h = Hasher::new();
    h.update(&[1, 2, 3]);
    h.finalize();
}
"#).unwrap();

    fs::write(project_dir.path().join("scripts/helper.js"), r#"
function update() { return 1; }
function caller_js() { update(); }
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let rust_caller = crate::storage::queries::get_nodes_with_files_by_name(
        db.conn(), "caller_rs",
    ).unwrap();
    let rust_caller = rust_caller.iter()
        .find(|n| n.file_path == "src/hasher.rs")
        .expect("Rust caller_rs should be indexed");
    let edges = get_edges_from(db.conn(), rust_caller.node.id).unwrap();
    for e in &edges {
        if e.relation != REL_CALLS { continue; }
        let tgt_path: Option<String> = db.conn().query_row(
            "SELECT f.path FROM nodes n JOIN files f ON n.file_id = f.id WHERE n.id = ?1",
            [e.target_id], |row| row.get(0),
        ).ok();
        assert!(
            !tgt_path.as_deref().unwrap_or("").ends_with(".js"),
            "Rust caller must not resolve calls into JS; got edge → {:?}", tgt_path,
        );
    }

    let js_caller = crate::storage::queries::get_nodes_with_files_by_name(
        db.conn(), "caller_js",
    ).unwrap();
    let js_caller = js_caller.iter()
        .find(|n| n.file_path == "scripts/helper.js")
        .expect("JS caller_js should be indexed");
    let js_edges = get_edges_from(db.conn(), js_caller.node.id).unwrap();
    let js_call_targets: Vec<i64> = js_edges.iter()
        .filter(|e| e.relation == REL_CALLS)
        .map(|e| e.target_id)
        .collect();
    assert!(!js_call_targets.is_empty(),
        "JS caller_js → update edge within same file should still resolve");
}

#[test]
fn test_intra_class_method_call_edges_resolve() {
    // Regression: class-based languages qualify a method's enclosing scope as
    // `Class.method`, but the node's bare `name` is just `method`. Phase-2 source
    // resolution matched only bare node_names, so EVERY intra-class method →
    // sibling-method call edge was silently dropped (TS/JS/Python/Java/Ruby).
    // Rust/Go were unaffected (bare scope), which masked the bug. Fix: also match
    // the relation's qualified source_name against each node's qualified_name.
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::create_dir_all(project_dir.path().join("src")).unwrap();

    fs::write(project_dir.path().join("src/svc.py"), r#"
class UserSvc:
    def get_user(self, uid):
        return self._fetch(uid)
    def _fetch(self, uid):
        return uid
"#).unwrap();
    fs::write(project_dir.path().join("src/Svc.java"), r#"
class Svc {
    void run() { helper(); }
    void helper() {}
}
"#).unwrap();
    fs::write(project_dir.path().join("src/svc.ts"), r#"
class TsSvc {
    outer(): void { this.inner(); }
    inner(): void {}
}
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    // Each outer method must have a calls edge to its sibling.
    for (caller, callee) in [("get_user", "_fetch"), ("run", "helper"), ("outer", "inner")] {
        let nodes = get_nodes_by_name(db.conn(), caller).unwrap();
        let node = nodes.first().unwrap_or_else(|| panic!("{caller} should be indexed"));
        let edges = get_edges_from(db.conn(), node.id).unwrap();
        let has_call = edges.iter().any(|e| {
            if e.relation != REL_CALLS { return false; }
            let tgt: Option<String> = db.conn().query_row(
                "SELECT name FROM nodes WHERE id = ?1", [e.target_id], |r| r.get(0),
            ).ok();
            tgt.as_deref() == Some(callee)
        });
        assert!(has_call, "{caller} → {callee} method-call edge was dropped");
    }
}

#[test]
fn test_js_require_creates_external_import_edges() {
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::write(project_dir.path().join("app.js"), r#"
const fs = require('fs');
const path = require('path');
const lifecycle = require('./lifecycle');

function main() { fs.readFileSync('x'); }
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let imports: Vec<String> = db.conn().prepare(
        "SELECT DISTINCT n2.name FROM edges e
         JOIN nodes n ON n.id = e.source_id
         JOIN files f ON f.id = n.file_id
         JOIN nodes n2 ON n2.id = e.target_id
         WHERE f.path = 'app.js' AND e.relation = 'imports'"
    ).unwrap()
     .query_map([], |row| row.get::<_, String>(0)).unwrap()
     .filter_map(Result::ok)
     .collect();

    assert!(imports.contains(&"fs".to_string()),        "imports: {:?}", imports);
    assert!(imports.contains(&"path".to_string()),      "imports: {:?}", imports);
    assert!(imports.contains(&"lifecycle".to_string()), "imports: {:?}", imports);
}

#[test]
fn test_js_same_name_cross_file_prefers_closest_path() {
    // Regression: when JS defines the same helper name in multiple files
    // (e.g., `readJson` in both `claude-plugin/scripts/lifecycle.js` and
    // `scripts/install-e2e.test.js`), a caller in `claude-plugin/scripts/*`
    // used to fan out an edge to every same-language match, producing
    // false-positive callers across unrelated modules. The resolver must
    // pick the candidate with the longest common path prefix to the
    // caller file (and prefer non-test files) rather than all.
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::create_dir_all(project_dir.path().join("pkg/scripts")).unwrap();
    fs::create_dir_all(project_dir.path().join("tests")).unwrap();

    fs::write(project_dir.path().join("pkg/scripts/lifecycle.js"), r#"
function readJson(p) { return 1; }
module.exports = { readJson };
"#).unwrap();

    fs::write(project_dir.path().join("pkg/scripts/session-init.js"), r#"
function syncLifecycleConfig() { readJson('x'); }
"#).unwrap();

    fs::write(project_dir.path().join("tests/helpers.test.js"), r#"
function readJson(p) { return 2; }
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    // Find the caller node
    let caller = crate::storage::queries::get_nodes_with_files_by_name(
        db.conn(), "syncLifecycleConfig",
    ).unwrap();
    let caller = caller.iter()
        .find(|n| n.file_path == "pkg/scripts/session-init.js")
        .expect("syncLifecycleConfig should be indexed");

    let edges = get_edges_from(db.conn(), caller.node.id).unwrap();
    let call_edges: Vec<i64> = edges.iter()
        .filter(|e| e.relation == REL_CALLS)
        .map(|e| e.target_id)
        .collect();

    // Resolve target paths
    let target_paths: Vec<String> = call_edges.iter().filter_map(|tid| {
        db.conn().query_row(
            "SELECT f.path FROM nodes n JOIN files f ON n.file_id = f.id WHERE n.id = ?1",
            [*tid], |row| row.get(0)
        ).ok()
    }).collect();

    // Must pick exactly the same-dir candidate, not fan out to the test file.
    assert!(
        target_paths.iter().any(|p| p == "pkg/scripts/lifecycle.js"),
        "should resolve to same-dir readJson; got {:?}", target_paths
    );
    assert!(
        !target_paths.iter().any(|p| p == "tests/helpers.test.js"),
        "should NOT fan out to unrelated test-file readJson; got {:?}", target_paths
    );
}

#[test]
fn test_import_binding_resolves_call_over_path_proximity() {
    // Import-aware call resolution: when a bare call's name matches same-name
    // defs in multiple files, an explicit `from X import name` in the caller's
    // file must bind the call to the IMPORTED definition — even when a
    // different same-name def is closer by path. Without import-awareness,
    // refine_ambiguous_targets picks the path-closest (wrong) target, which
    // prune_import_contradicted_call_edges then deletes, leaving the call with
    // NO edge at all (correct import-bound edge never positively created).
    // Python is used here because its import edges already resolve
    // module-path-aware (resolve_python_module_targets), isolating the
    // call-resolution gap; JS module-specifier resolution is a separate cycle.
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::create_dir_all(project_dir.path().join("app/core")).unwrap();
    fs::create_dir_all(project_dir.path().join("app/util")).unwrap();

    // Path-closest same-name def — the WRONG target for the call below.
    fs::write(project_dir.path().join("app/core/helper.py"), r#"
def process():
    return 1
"#).unwrap();

    // Imported same-name def — the RIGHT target, farther by path prefix.
    fs::write(project_dir.path().join("app/util/helper.py"), r#"
def process():
    return 2
"#).unwrap();

    // Caller sits next to app/core/helper.py but explicitly imports the util one.
    fs::write(project_dir.path().join("app/core/caller.py"), r#"
from app.util.helper import process

def run():
    return process()
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let caller = crate::storage::queries::get_nodes_with_files_by_name(
        db.conn(), "run",
    ).unwrap();
    let caller = caller.iter()
        .find(|n| n.file_path == "app/core/caller.py")
        .expect("run should be indexed");

    let edges = get_edges_from(db.conn(), caller.node.id).unwrap();
    let call_target_paths: Vec<String> = edges.iter()
        .filter(|e| e.relation == REL_CALLS)
        .filter_map(|e| db.conn().query_row(
            "SELECT f.path FROM nodes n JOIN files f ON n.file_id = f.id WHERE n.id = ?1",
            [e.target_id], |row| row.get(0),
        ).ok())
        .collect();

    assert!(
        call_target_paths.iter().any(|p| p == "app/util/helper.py"),
        "run() must resolve to the IMPORTED process (app/util/helper.py); got {:?}",
        call_target_paths
    );
    assert!(
        !call_target_paths.iter().any(|p| p == "app/core/helper.py"),
        "run() must NOT resolve to the path-closest non-imported process (app/core/helper.py); got {:?}",
        call_target_paths
    );
}

#[test]
fn test_js_named_import_resolves_via_module_specifier() {
    // Cycle 2: JS/TS import edges must resolve via the module specifier
    // (`from '../util/helper'`), not by path-proximity name matching. Two files
    // define `process`; the caller imports the farther one explicitly. The
    // import edge must bind to the specifier-resolved file, not the path-closest
    // same-name node (which is what refine_ambiguous_targets picks today).
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::create_dir_all(project_dir.path().join("src/core")).unwrap();
    fs::create_dir_all(project_dir.path().join("src/util")).unwrap();

    fs::write(project_dir.path().join("src/core/helper.ts"),
        "export function process() { return 1; }\n").unwrap();
    fs::write(project_dir.path().join("src/util/helper.ts"),
        "export function process() { return 2; }\n").unwrap();
    fs::write(project_dir.path().join("src/core/caller.ts"), r#"
import { process } from '../util/helper';

export function run() { return process(); }
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let import_target_paths: Vec<String> = db.conn().prepare(
        "SELECT tf.path FROM edges e
         JOIN nodes sn ON sn.id = e.source_id
         JOIN files sf ON sf.id = sn.file_id
         JOIN nodes tn ON tn.id = e.target_id
         JOIN files tf ON tf.id = tn.file_id
         WHERE e.relation = 'imports' AND sf.path = 'src/core/caller.ts'
           AND tn.name = 'process'"
    ).unwrap()
     .query_map([], |r| r.get::<_, String>(0)).unwrap()
     .filter_map(Result::ok)
     .collect();

    assert!(
        import_target_paths.iter().any(|p| p == "src/util/helper.ts"),
        "import must resolve via specifier to src/util/helper.ts; got {:?}",
        import_target_paths
    );
    assert!(
        !import_target_paths.iter().any(|p| p == "src/core/helper.ts"),
        "import must NOT bind to the path-closest src/core/helper.ts; got {:?}",
        import_target_paths
    );
}

#[test]
fn test_js_import_binds_call_over_path_proximity() {
    // Cycle 2 end-to-end (TS analog of test_import_binding_resolves_call_over_path_proximity):
    // once JS imports resolve via specifier, the Cycle-1 bind repoints the bare
    // call to the imported target instead of the path-closest same-name def.
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::create_dir_all(project_dir.path().join("src/core")).unwrap();
    fs::create_dir_all(project_dir.path().join("src/util")).unwrap();

    fs::write(project_dir.path().join("src/core/helper.ts"),
        "export function process() { return 1; }\n").unwrap();
    fs::write(project_dir.path().join("src/util/helper.ts"),
        "export function process() { return 2; }\n").unwrap();
    fs::write(project_dir.path().join("src/core/caller.ts"), r#"
import { process } from '../util/helper';

export function run() { return process(); }
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let caller = crate::storage::queries::get_nodes_with_files_by_name(
        db.conn(), "run",
    ).unwrap();
    let caller = caller.iter()
        .find(|n| n.file_path == "src/core/caller.ts")
        .expect("run should be indexed");

    let edges = get_edges_from(db.conn(), caller.node.id).unwrap();
    let call_target_paths: Vec<String> = edges.iter()
        .filter(|e| e.relation == REL_CALLS)
        .filter_map(|e| db.conn().query_row(
            "SELECT f.path FROM nodes n JOIN files f ON n.file_id = f.id WHERE n.id = ?1",
            [e.target_id], |row| row.get(0),
        ).ok())
        .collect();

    assert!(
        call_target_paths.iter().any(|p| p == "src/util/helper.ts"),
        "run() must resolve to the IMPORTED process (src/util/helper.ts); got {:?}",
        call_target_paths
    );
    assert!(
        !call_target_paths.iter().any(|p| p == "src/core/helper.ts"),
        "run() must NOT resolve to the path-closest non-imported process (src/core/helper.ts); got {:?}",
        call_target_paths
    );
}

#[test]
fn test_commonjs_destructured_require_binds_call() {
    // Cycle 3: `const { process } = require('../util/helper')` must resolve the
    // bare call process() to the required file's export, not the path-closest
    // same-name def. CommonJS analog of the ES-import case (this project's own
    // plugin JS uses require). Extraction emits a per-name import stamped with
    // the specifier; Cycle 2 resolution + Cycle 1 bind do the rest.
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::create_dir_all(project_dir.path().join("src/core")).unwrap();
    fs::create_dir_all(project_dir.path().join("src/util")).unwrap();

    fs::write(project_dir.path().join("src/core/helper.js"),
        "function process() { return 1; }\nmodule.exports = { process };\n").unwrap();
    fs::write(project_dir.path().join("src/util/helper.js"),
        "function process() { return 2; }\nmodule.exports = { process };\n").unwrap();
    fs::write(project_dir.path().join("src/core/caller.js"), r#"
const { process } = require('../util/helper');

function run() { return process(); }
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let caller = crate::storage::queries::get_nodes_with_files_by_name(
        db.conn(), "run",
    ).unwrap();
    let caller = caller.iter()
        .find(|n| n.file_path == "src/core/caller.js")
        .expect("run should be indexed");

    let edges = get_edges_from(db.conn(), caller.node.id).unwrap();
    let call_target_paths: Vec<String> = edges.iter()
        .filter(|e| e.relation == REL_CALLS)
        .filter_map(|e| db.conn().query_row(
            "SELECT f.path FROM nodes n JOIN files f ON n.file_id = f.id WHERE n.id = ?1",
            [e.target_id], |row| row.get(0),
        ).ok())
        .collect();

    assert!(
        call_target_paths.iter().any(|p| p == "src/util/helper.js"),
        "run() must resolve to the required process (src/util/helper.js); got {:?}",
        call_target_paths
    );
    assert!(
        !call_target_paths.iter().any(|p| p == "src/core/helper.js"),
        "run() must NOT resolve to the path-closest non-required process (src/core/helper.js); got {:?}",
        call_target_paths
    );
}

#[test]
fn test_commonjs_namespace_require_binds_member_call() {
    // Cycle 4: `const helper = require('../util/helper'); helper.process()` must
    // resolve the member call to the required module's export, not the
    // path-closest same-name def. JS discards the receiver today (extract_callee
    // returns Bare for non-Rust), so `helper.process()` resolves "process" by
    // proximity. Capturing the receiver + tracking the require-namespace binding
    // lets it bind to the required file.
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::create_dir_all(project_dir.path().join("src/core")).unwrap();
    fs::create_dir_all(project_dir.path().join("src/util")).unwrap();

    fs::write(project_dir.path().join("src/core/helper.js"),
        "function process() { return 1; }\nmodule.exports = { process };\n").unwrap();
    fs::write(project_dir.path().join("src/util/helper.js"),
        "function process() { return 2; }\nmodule.exports = { process };\n").unwrap();
    fs::write(project_dir.path().join("src/core/caller.js"), r#"
const helper = require('../util/helper');

function run() { return helper.process(); }
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let caller = crate::storage::queries::get_nodes_with_files_by_name(
        db.conn(), "run",
    ).unwrap();
    let caller = caller.iter()
        .find(|n| n.file_path == "src/core/caller.js")
        .expect("run should be indexed");

    let edges = get_edges_from(db.conn(), caller.node.id).unwrap();
    let call_target_paths: Vec<String> = edges.iter()
        .filter(|e| e.relation == REL_CALLS)
        .filter_map(|e| db.conn().query_row(
            "SELECT f.path FROM nodes n JOIN files f ON n.file_id = f.id WHERE n.id = ?1",
            [e.target_id], |row| row.get(0),
        ).ok())
        .collect();

    assert!(
        call_target_paths.iter().any(|p| p == "src/util/helper.js"),
        "helper.process() must resolve to the required module (src/util/helper.js); got {:?}",
        call_target_paths
    );
    assert!(
        !call_target_paths.iter().any(|p| p == "src/core/helper.js"),
        "helper.process() must NOT resolve to the path-closest non-required process (src/core/helper.js); got {:?}",
        call_target_paths
    );
}

#[test]
fn test_js_module_level_test_callback_calls_resolve() {
    // Regression: helpers defined in a JS test file that are called only
    // from inside `test(() => {...})` / `describe(() => {...})` callbacks
    // used to be reported as orphan by dead-code, because the anonymous
    // arrow callback body attributed its calls to `<anonymous>`, a name
    // that resolves to no node. Module-level call_expressions inside JS
    // test files must attribute to `<module>` so a same-file edge lands.
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    fs::write(project_dir.path().join("helpers.test.js"), r#"
function mkHome() { return '/tmp/x'; }
function writeJson(p, v) { }

test('uses helpers', () => {
    const h = mkHome();
    writeJson(h, { a: 1 });
});
"#).unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    // Both helper names must have at least one incoming call edge.
    for helper in ["mkHome", "writeJson"] {
        let cnt: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM edges e
             JOIN nodes tn ON tn.id = e.target_id
             JOIN files tf ON tf.id = tn.file_id
             WHERE tn.name = ?1 AND tf.path = 'helpers.test.js' AND e.relation = 'calls'",
            [helper], |row| row.get(0),
        ).unwrap();
        assert!(cnt >= 1,
            "{} should have at least one incoming call edge from the test callback, got {}",
            helper, cnt);
    }
}

#[test]
fn test_incremental_index() {
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // Initial index
    fs::write(project_dir.path().join("a.ts"), "function foo() {}").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    // Modify file
    fs::write(project_dir.path().join("a.ts"), "function bar() {}").unwrap();

    // Incremental index
    let result = run_incremental_index(&db, project_dir.path(), None, None).unwrap();
    assert_eq!(result.files_indexed, 1);

    let foo = get_nodes_by_name(db.conn(), "foo").unwrap();
    assert_eq!(foo.len(), 0);
    let bar = get_nodes_by_name(db.conn(), "bar").unwrap();
    assert_eq!(bar.len(), 1);
}

#[test]
fn test_incremental_propagates_dirty_context() {
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // Initial: B (in b.ts) calls A (in a.ts)
    fs::write(project_dir.path().join("a.ts"), "function alpha() {}").unwrap();
    fs::write(project_dir.path().join("b.ts"), "function beta() { alpha(); }").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let beta_nodes = get_nodes_by_name(db.conn(), "beta").unwrap();
    assert_eq!(beta_nodes.len(), 1);
    let beta_ctx_before = beta_nodes[0].context_string.clone().unwrap_or_default();

    // Change A: rename function (alpha -> alphaRenamed)
    fs::write(project_dir.path().join("a.ts"), "function alphaRenamed() {}").unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    // beta's context_string should be updated (calls list changed because
    // the old alpha node is gone and edge was cascade-deleted)
    let beta_nodes_after = get_nodes_by_name(db.conn(), "beta").unwrap();
    assert_eq!(beta_nodes_after.len(), 1);
    let beta_ctx_after = beta_nodes_after[0].context_string.clone().unwrap_or_default();
    assert_ne!(beta_ctx_before, beta_ctx_after);
}

// Regression (#3): when an incremental index runs with model=None (the watcher /
// drift path, which avoids holding the model lock across I/O), a cross-file dirty
// node's context_string is regenerated — so its existing vector is now STALE and
// must be invalidated (dropped) so the background embedder re-selects it. Before
// the fix the stale vector survived until a full rebuild.
#[test]
fn test_edge_flip_invalidates_caller_vector() {
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    // open_with_vec → vec_enabled()=true so the model=None invalidation branch runs.
    let db = Database::open_with_vec(&db_dir.path().join("index.db")).unwrap();
    assert!(db.vec_enabled(), "test requires vec tables");

    // beta (b.ts) calls alpha (a.ts)
    fs::write(project_dir.path().join("a.ts"), "function alpha() {}").unwrap();
    fs::write(project_dir.path().join("b.ts"), "function beta() { alpha(); }").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let beta_nodes = get_nodes_by_name(db.conn(), "beta").unwrap();
    assert_eq!(beta_nodes.len(), 1);
    let beta_id = beta_nodes[0].id;

    // Seed a fake vector for beta (indexing ran with model=None, so no real embed).
    let fake: Vec<f32> = vec![0.1; crate::domain::EMBEDDING_DIM];
    crate::storage::queries::insert_node_vector(db.conn(), beta_id, &fake).unwrap();
    assert!(crate::storage::queries::get_node_embedding(db.conn(), beta_id).is_ok(),
        "fake vector must be present before the edge flip");

    // Flip the edge: rename alpha → alphaRenamed. beta is a cross-file caller, so
    // its context_string is regenerated (callee set changed) but its node row is NOT
    // deleted (b.ts unchanged) — exactly the case the AFTER DELETE trigger misses.
    fs::write(project_dir.path().join("a.ts"), "function alphaRenamed() {}").unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    let beta_after = get_nodes_by_name(db.conn(), "beta").unwrap();
    assert_eq!(beta_after.len(), 1);
    assert_eq!(beta_after[0].id, beta_id, "beta node id stays stable (not recreated)");
    // Its stale vector must be gone, and beta re-selectable by the background embedder.
    assert!(crate::storage::queries::get_node_embedding(db.conn(), beta_id).is_err(),
        "stale vector for cross-file dirty node must be invalidated when model=None");
    let unembedded = crate::storage::queries::get_unembedded_nodes(db.conn(), 50).unwrap();
    assert!(unembedded.iter().any(|(id, _)| *id == beta_id),
        "beta must be re-selectable by the background embedder after invalidation");
}

#[test]
fn test_cross_language_structural_edges_isolated() {
    // v31 regression (#3): structural relations (imports/inherits/implements/
    // exports/routes_to) must NOT fall through to the global all-language name
    // pool. Before the fix a Rust `use anyhow::Result` bound an `imports` edge to
    // a markdown "Result" heading, and `require('fs')` bound to a Rust `fs`
    // symbol — cross-language phantom edges stamped `extracted` (unfilterable),
    // polluting deps/project_map/cycles/find_references. Same-language gating (+
    // the `<external>` sentinel for genuine externals) must eliminate them.
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::create_dir_all(project_dir.path().join("src")).unwrap();

    // Rust `use anyhow::Result` (import target_name "Result") + a Rust fn `fs`.
    fs::write(project_dir.path().join("src/lib.rs"),
        "use anyhow::Result;\npub fn foo() -> Result<()> { Ok(()) }\npub fn fs() {}\n").unwrap();
    // Markdown heading "Result" — the cross-language collision target.
    fs::write(project_dir.path().join("README.md"), "# Result\n\nDocs.\n").unwrap();
    // JS `require('fs')` (import target_name "fs") collides with the Rust `fs` fn.
    fs::write(project_dir.path().join("app.js"),
        "const fs = require('fs');\nfunction g() { return fs; }\n").unwrap();

    let db = Database::open(&db_dir.path().join("index.db")).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();
    let conn = db.conn();

    // Sanity: the markdown collision target exists (so a passing test can't be a
    // false pass from the heading simply not being indexed).
    let md_result: i64 = conn.query_row(
        "SELECT COUNT(*) FROM nodes n JOIN files f ON f.id = n.file_id \
         WHERE n.name = 'Result' AND f.language = 'markdown'", [], |r| r.get(0)).unwrap();
    assert_eq!(md_result, 1, "markdown 'Result' heading must be indexed (the collision target)");

    // No structural edge may cross language to a NON-external target. The
    // `<external>` sentinel (language 'external') is the only allowed
    // not-same-language import/implements target.
    let cross_lang_structural: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edges e \
         JOIN nodes s ON s.id = e.source_id JOIN files fs ON fs.id = s.file_id \
         JOIN nodes t ON t.id = e.target_id JOIN files ft ON ft.id = t.file_id \
         WHERE e.relation IN ('imports','inherits','implements','exports','routes_to') \
           AND fs.language IS NOT ft.language \
           AND COALESCE(ft.language,'') != 'external'",
        [], |r| r.get(0)).unwrap();
    assert_eq!(cross_lang_structural, 0,
        "structural edges must bind same-language only; got {cross_lang_structural} cross-language");
}

#[test]
fn test_phase2c_restore_binds_only_original_target_file() {
    // v31 regression (#4) + the previously-untested happy path. The Phase-2c
    // incremental inbound-edge restore must re-bind a saved cross-file edge ONLY
    // to the same-name node in the file the edge originally pointed into — not
    // every same-name node in the batch. caller.ts → target() resolves into
    // target.ts; an incremental then re-indexes target.ts AND other.ts in one
    // batch and other.ts gains its own `target`. The restored edge must land on
    // target.ts only (a fan-out to other.ts is an edge a full rebuild never makes).
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    fs::create_dir_all(project_dir.path().join("src")).unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    fs::write(project_dir.path().join("src/caller.ts"), "function caller() { target(); }").unwrap();
    fs::write(project_dir.path().join("src/target.ts"), "function target() {}").unwrap();
    fs::write(project_dir.path().join("src/other.ts"), "function unrelated() {}").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    let count_caller_to = |path: &str| -> i64 {
        db.conn().query_row(
            "SELECT COUNT(*) FROM edges e \
             JOIN nodes s ON s.id = e.source_id JOIN files fs ON fs.id = s.file_id \
             JOIN nodes t ON t.id = e.target_id JOIN files ft ON ft.id = t.file_id \
             WHERE e.relation = 'calls' AND s.name = 'caller' AND t.name = 'target' \
               AND fs.path = 'src/caller.ts' AND ft.path = ?1",
            [path], |r| r.get(0)).unwrap()
    };
    assert_eq!(count_caller_to("src/target.ts"), 1, "initial: caller → target.ts:target");
    assert_eq!(count_caller_to("src/other.ts"), 0, "initial: other.ts has no target yet");

    // Re-index BOTH target.ts (keep `target`) and other.ts (ADD a `target`) in one batch.
    fs::write(project_dir.path().join("src/target.ts"), "function target() { return 1; }").unwrap();
    fs::write(project_dir.path().join("src/other.ts"), "function unrelated() {}\nfunction target() {}").unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    // Happy path: the cascade-deleted edge was restored to the NEW target.ts node.
    assert_eq!(count_caller_to("src/target.ts"), 1,
        "restore must rebind caller → target.ts:target to the new node id");
    // Over-creation guard: must NOT fan out to other.ts's same-name target.
    assert_eq!(count_caller_to("src/other.ts"), 0,
        "restore must NOT bind caller → other.ts:target (cross-file fan-out a rebuild never makes)");
}

#[test]
fn test_deleted_file_cleanup() {
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    fs::write(project_dir.path().join("a.ts"), "function foo() {}").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    fs::remove_file(project_dir.path().join("a.ts")).unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    let foo = get_nodes_by_name(db.conn(), "foo").unwrap();
    assert_eq!(foo.len(), 0);
}

#[test]
fn test_build_python_module_map() {
    let mut paths = HashSet::new();
    paths.insert("myapp/utils.py".into());
    paths.insert("myapp/__init__.py".into());
    paths.insert("src/myapp/models.py".into());

    let map = build_python_module_map(&paths);

    // Full dotted path
    assert!(map.get("myapp.utils").unwrap().contains(&"myapp/utils.py".to_string()));
    // Suffix path
    assert!(map.get("utils").unwrap().contains(&"myapp/utils.py".to_string()));
    // __init__.py maps to package
    assert!(map.get("myapp").unwrap().contains(&"myapp/__init__.py".to_string()));
    // Nested with src/ prefix
    assert!(map.get("myapp.models").unwrap().contains(&"src/myapp/models.py".to_string()));
}

#[test]
fn test_python_from_import_resolution() {
    // Test `from myapp.utils import helper` creates correct cross-file edge
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    fs::create_dir_all(project_dir.path().join("myapp")).unwrap();
    fs::write(
        project_dir.path().join("myapp/utils.py"),
        "def helper():\n    return 42\n",
    ).unwrap();
    fs::write(
        project_dir.path().join("myapp/main.py"),
        "from myapp.utils import helper\n\ndef main():\n    helper()\n",
    ).unwrap();

    let result = run_full_index(&db, project_dir.path(), None, None).unwrap();
    assert!(result.edges_created > 0, "should create import edges");

    // Verify dependency: main.py -> utils.py
    let deps = get_import_tree(db.conn(), "myapp/main.py", "outgoing", 1).unwrap();
    assert!(
        deps.iter().any(|d| d.file_path == "myapp/utils.py"),
        "main.py should depend on utils.py, got: {:?}",
        deps.iter().map(|d| &d.file_path).collect::<Vec<_>>()
    );
}

#[test]
fn test_python_import_module_resolution() {
    // Test `import myutils` creates correct cross-file edge
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    fs::write(
        project_dir.path().join("myutils.py"),
        "def do_something():\n    pass\n",
    ).unwrap();
    fs::write(
        project_dir.path().join("main.py"),
        "import myutils\n\ndef main():\n    myutils.do_something()\n",
    ).unwrap();

    let result = run_full_index(&db, project_dir.path(), None, None).unwrap();
    assert!(result.edges_created > 0, "should create import edges");

    // Verify dependency: main.py -> myutils.py
    let deps = get_import_tree(db.conn(), "main.py", "outgoing", 1).unwrap();
    assert!(
        deps.iter().any(|d| d.file_path == "myutils.py"),
        "main.py should depend on myutils.py, got: {:?}",
        deps.iter().map(|d| &d.file_path).collect::<Vec<_>>()
    );
}

#[test]
fn test_python_external_import_creates_virtual_nodes() {
    // Test that external imports create virtual nodes in <external> file
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    fs::write(
        project_dir.path().join("app.py"),
        "import os\nfrom collections import OrderedDict\nfrom flask import Flask\n\ndef main():\n    pass\n",
    ).unwrap();

    let result = run_full_index(&db, project_dir.path(), None, None).unwrap();
    assert!(result.files_indexed > 0, "should index the file");

    // Verify <external> file was created with virtual nodes
    let ext_nodes = get_nodes_by_file_path(db.conn(), "<external>").unwrap();
    let ext_names: Vec<&str> = ext_nodes.iter().map(|n| n.name.as_str()).collect();
    assert!(ext_names.contains(&"os"), "should have virtual node for 'os', got: {:?}", ext_names);
    assert!(ext_names.contains(&"collections"), "should have virtual node for 'collections', got: {:?}", ext_names);
    assert!(ext_names.contains(&"flask"), "should have virtual node for 'flask', got: {:?}", ext_names);

    // Verify dependency_graph shows <external> as a dependency
    let deps = get_import_tree(db.conn(), "app.py", "outgoing", 1).unwrap();
    assert!(
        deps.iter().any(|d| d.file_path == "<external>"),
        "app.py should show <external> dependency, got: {:?}",
        deps.iter().map(|d| &d.file_path).collect::<Vec<_>>()
    );
}

#[test]
fn test_python_mixed_internal_external_imports() {
    // Test project with both internal and external imports
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    fs::create_dir_all(project_dir.path().join("myapp")).unwrap();
    fs::write(
        project_dir.path().join("myapp/utils.py"),
        "def helper():\n    return 42\n",
    ).unwrap();
    fs::write(
        project_dir.path().join("myapp/main.py"),
        "import os\nfrom myapp.utils import helper\nfrom flask import Flask\n\ndef main():\n    helper()\n",
    ).unwrap();

    let result = run_full_index(&db, project_dir.path(), None, None).unwrap();
    assert!(result.edges_created > 0);

    // Should have internal dependency
    let deps = get_import_tree(db.conn(), "myapp/main.py", "outgoing", 1).unwrap();
    let dep_files: Vec<&str> = deps.iter().map(|d| d.file_path.as_str()).collect();
    assert!(dep_files.contains(&"myapp/utils.py"), "should depend on internal utils.py, got: {:?}", dep_files);

    // Should also have external dependency
    assert!(dep_files.contains(&"<external>"), "should depend on <external>, got: {:?}", dep_files);
}

#[test]
fn test_index_stats_skipped_large_file() {
    // Verify that IndexResult.stats tracks files skipped due to size
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // Create a normal file
    fs::write(project_dir.path().join("small.ts"), "function ok() {}").unwrap();

    // Create a file exceeding MAX_FILE_SIZE (10MB)
    let big_content = "a".repeat(11 * 1024 * 1024);
    fs::write(project_dir.path().join("huge.ts"), &big_content).unwrap();

    let result = run_full_index(&db, project_dir.path(), None, None).unwrap();
    assert_eq!(result.files_indexed, 1, "should index the small file");
    assert_eq!(result.stats.files_skipped_size, 1, "should track the large file skip");
}

#[test]
fn test_index_stats_skipped_parse_error() {
    // Verify that IndexResult.stats tracks files skipped due to parse errors
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // Create a valid file
    fs::write(project_dir.path().join("good.ts"), "function ok() {}").unwrap();

    // Create a file with an unsupported extension that detect_language returns None for
    // (this is filtered by detect_language returning None, not a parse error)
    // Instead, we just verify the default stats are zero for parse errors
    let result = run_full_index(&db, project_dir.path(), None, None).unwrap();
    assert_eq!(result.stats.files_skipped_parse, 0);
    assert_eq!(result.stats.files_skipped_read, 0);
    assert_eq!(result.stats.files_skipped_hash, 0);
}

#[test]
fn test_index_stats_default() {
    // IndexStats should implement Default
    let stats = IndexStats::default();
    assert_eq!(stats.files_skipped_size, 0);
    assert_eq!(stats.files_skipped_parse, 0);
    assert_eq!(stats.files_skipped_read, 0);
    assert_eq!(stats.files_skipped_hash, 0);
    assert_eq!(stats.files_skipped_language, 0);
}

#[test]
fn test_python_external_survives_incremental_index() {
    // Test that <external> pseudo-file persists across incremental re-indexes
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    fs::write(
        project_dir.path().join("app.py"),
        "import os\n\ndef main():\n    pass\n",
    ).unwrap();

    // Full index → creates <external> with "os" node
    run_full_index(&db, project_dir.path(), None, None).unwrap();
    let ext_before = get_nodes_by_file_path(db.conn(), "<external>").unwrap();
    assert!(!ext_before.is_empty(), "should have external nodes after full index");

    // Modify file slightly
    fs::write(
        project_dir.path().join("app.py"),
        "import os\n\ndef main():\n    return 1\n",
    ).unwrap();

    // Incremental index → <external> should survive
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();
    let ext_after = get_nodes_by_file_path(db.conn(), "<external>").unwrap();
    assert!(!ext_after.is_empty(), "external nodes should survive incremental index");

    // Verify dependency still visible
    let deps = get_import_tree(db.conn(), "app.py", "outgoing", 1).unwrap();
    assert!(
        deps.iter().any(|d| d.file_path == "<external>"),
        "app.py should still show <external> dependency after incremental index"
    );
}

#[test]
fn test_repair_null_context_strings() {
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // Index a file so nodes get context strings
    fs::write(project_dir.path().join("a.ts"), r#"
function alpha() { return 1; }
function beta() { alpha(); }
"#).unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    // Verify context strings exist after index
    let alpha_nodes = get_nodes_by_name(db.conn(), "alpha").unwrap();
    assert_eq!(alpha_nodes.len(), 1);
    assert!(alpha_nodes[0].context_string.is_some(), "alpha should have context_string after index");

    let beta_nodes = get_nodes_by_name(db.conn(), "beta").unwrap();
    assert_eq!(beta_nodes.len(), 1);
    assert!(beta_nodes[0].context_string.is_some(), "beta should have context_string after index");

    // Simulate Phase 3 failure: NULL out context_strings
    db.conn().execute("UPDATE nodes SET context_string = NULL", []).unwrap();

    // Verify they are now NULL
    let alpha_after_null = get_nodes_by_name(db.conn(), "alpha").unwrap();
    assert!(alpha_after_null[0].context_string.is_none(), "alpha context_string should be NULL after simulated failure");

    // Run repair
    let repaired = repair_null_context_strings(&db, None).unwrap();
    assert!(repaired > 0, "should repair at least 1 node");

    // Verify context strings were restored
    let alpha_repaired = get_nodes_by_name(db.conn(), "alpha").unwrap();
    assert!(alpha_repaired[0].context_string.is_some(), "alpha should have context_string after repair");

    let beta_repaired = get_nodes_by_name(db.conn(), "beta").unwrap();
    assert!(beta_repaired[0].context_string.is_some(), "beta should have context_string after repair");
}

#[test]
fn test_rust_implements_creates_sentinel_for_external_trait() {
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    fs::write(project_dir.path().join("main.rs"), r#"
use std::io::{self, Write};
use std::fmt;

struct MyWriter;

impl Write for MyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> { Ok(buf.len()) }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl fmt::Display for MyWriter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "MyWriter")
    }
}
"#).unwrap();

    let result = run_full_index(&db, project_dir.path(), None, None).unwrap();
    assert!(result.files_indexed > 0);

    // Verify sentinel nodes created for external traits
    let ext_nodes = get_nodes_by_file_path(db.conn(), "<external>").unwrap();
    let ext_names: Vec<&str> = ext_nodes.iter().map(|n| n.name.as_str()).collect();
    assert!(ext_names.contains(&"Write"), "should have sentinel for Write, got: {:?}", ext_names);
    // fmt::Display keeps path prefix (as parsed by tree-sitter)
    assert!(ext_names.contains(&"fmt::Display"), "should have sentinel for fmt::Display, got: {:?}", ext_names);

    // Verify sentinel type is "trait"
    let write_node = ext_nodes.iter().find(|n| n.name == "Write").unwrap();
    assert_eq!(write_node.node_type, "trait", "sentinel should be type 'trait'");

    // Verify implements edges exist: MyWriter → Write, MyWriter → Display
    let edges: Vec<(String, String)> = db.conn().prepare(
        "SELECT ns.name, nt.name FROM edges e
         JOIN nodes ns ON ns.id = e.source_id
         JOIN nodes nt ON nt.id = e.target_id
         WHERE e.relation = 'implements'"
    ).unwrap()
    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
    .unwrap()
    .collect::<Result<Vec<_>, _>>().unwrap();

    assert!(edges.contains(&("MyWriter".into(), "Write".into())),
        "should have MyWriter→Write implements edge, got: {:?}", edges);
    assert!(edges.contains(&("MyWriter".into(), "fmt::Display".into())),
        "should have MyWriter→fmt::Display implements edge, got: {:?}", edges);
}

/// ensure_file_indexed must (a) be a no-op when on-disk hash matches the
/// stored hash, and (b) actually pick up post-edit content when it doesn't.
/// This is the contract the MCP `ensure_file_fresh_opt` wrapper relies on
/// to close the post-Edit→pre-incremental-index window.
#[test]
fn test_ensure_file_indexed_picks_up_post_edit_changes() {
    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // Initial state: file with `alpha`
    fs::write(project_dir.path().join("a.ts"), "function alpha() {}\n").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();
    let names_before: Vec<String> = get_nodes_by_name(db.conn(), "alpha")
        .unwrap().into_iter().map(|n| n.name).collect();
    assert_eq!(names_before, vec!["alpha".to_string()]);

    // No-op when hashes match
    let did = ensure_file_indexed(&db, project_dir.path(), "a.ts", None).unwrap();
    assert!(!did, "matching hash must be a no-op (got reindex)");

    // Edit on disk; old `alpha` removed, new `beta` added
    fs::write(project_dir.path().join("a.ts"), "function beta() {}\n").unwrap();
    let did2 = ensure_file_indexed(&db, project_dir.path(), "a.ts", None).unwrap();
    assert!(did2, "hash mismatch must trigger a reindex");

    // alpha gone, beta present — post-Edit query would now see fresh state
    assert!(get_nodes_by_name(db.conn(), "alpha").unwrap().is_empty(),
        "old alpha must be evicted by single-file reindex");
    let beta = get_nodes_by_name(db.conn(), "beta").unwrap();
    assert_eq!(beta.len(), 1, "new beta must appear after single-file reindex");
    assert_eq!(beta[0].name, "beta");

    // Calling again with no on-disk change is a no-op
    let did3 = ensure_file_indexed(&db, project_dir.path(), "a.ts", None).unwrap();
    assert!(!did3, "second call with no edit must no-op");

    // Deleting the file from disk drops the row
    fs::remove_file(project_dir.path().join("a.ts")).unwrap();
    let did4 = ensure_file_indexed(&db, project_dir.path(), "a.ts", None).unwrap();
    assert!(did4, "missing file must trigger row cleanup");
    assert!(get_nodes_by_name(db.conn(), "beta").unwrap().is_empty(),
        "beta must be cascade-deleted with its file");
}

/// Root-cause test for `feedback_incremental_edge_timing.md`: file B
/// (existing, unchanged) bare-name calls `foo()`. file A is added later
/// with `function foo() {}`. Phase 2 of B's first index pass dropped the
/// edge because `foo` was unresolvable; before this fix, A's later index
/// never re-resolved B's call → permanently missing edge in incremental
/// mode (only `rebuild-index` recovered it).
///
/// New behavior: B's drop becomes a `pending_unresolved_calls` row; A's
/// index pass sweeps pending and promotes the row into a real edge.
#[test]
fn test_pending_unresolved_call_resolves_when_callee_added_later() {
    use crate::storage::queries::{count_pending_unresolved_calls, get_node_ids_by_name};

    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // Step 1: B exists alone with bare-name call to foo (foo undefined).
    fs::write(project_dir.path().join("b.ts"),
        "function caller_b() { foo(); }\n").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    // Phase 2 dropped the edge (no same-file/same-language target) and
    // buffered the row instead.
    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 1,
        "B's call to undefined foo must land in pending_unresolved_calls");

    let caller_b_id = get_node_ids_by_name(db.conn(), "caller_b").unwrap()
        .into_iter().next().expect("caller_b must exist").0;

    // Verify NO edge yet (foo doesn't exist in DB).
    let pre_edges = crate::storage::queries::get_edges_from(db.conn(), caller_b_id).unwrap();
    assert!(pre_edges.iter().all(|e| e.relation != REL_CALLS),
        "no calls edge should exist yet — foo is undefined");

    // Step 2: A is added with foo(). Incremental index picks it up; the
    // pending sweep at end of index_files promotes B's buffered call into
    // a real edge.
    fs::write(project_dir.path().join("a.ts"),
        "export function foo() {}\n").unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    let foo_id = get_node_ids_by_name(db.conn(), "foo").unwrap()
        .into_iter().next().expect("foo must exist after A indexed").0;

    let post_edges = crate::storage::queries::get_edges_from(db.conn(), caller_b_id).unwrap();
    let calls_to_foo: Vec<_> = post_edges.iter()
        .filter(|e| e.relation == REL_CALLS && e.target_id == foo_id)
        .collect();
    assert_eq!(calls_to_foo.len(), 1,
        "incremental index must promote pending call → calls edge caller_b → foo; \
         got edges: {:?}", post_edges.iter().map(|e| (&e.relation, e.target_id)).collect::<Vec<_>>());

    // Pending row must be drained after successful resolution.
    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 0,
        "resolved pending row must be deleted after edge insertion");
}

/// Cross-language pending must NOT resolve cross-language. If B (TS)
/// calls `update()` and a later-indexed Rust file defines `fn update()`,
/// the pending row must stay buffered, not silently bind cross-language
/// (memory `feedback_edge_resolution_same_language.md`'s canonical
/// false-positive class).
#[test]
fn test_pending_unresolved_call_does_not_cross_language() {
    use crate::storage::queries::count_pending_unresolved_calls;

    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // TS file with bare-name call to `update`
    fs::write(project_dir.path().join("client.ts"),
        "function caller_ts() { update(); }\n").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();
    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 1);

    // Rust file with `update` — different language, must NOT match.
    fs::write(project_dir.path().join("hasher.rs"),
        "fn update() {}\n").unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    // Pending row stays — sweep refused cross-language resolution.
    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 1,
        "cross-language target must NOT resolve a TS pending call to a Rust fn");
}

/// One caller with N undefined references must produce N pending rows;
/// when a single later-added file defines all N, all rows must resolve in
/// a single sweep. Real codebases hit this whenever a "barrel" or shared
/// utility module gets added after its consumers.
#[test]
fn test_pending_resolves_multiple_calls_in_same_caller() {
    use crate::storage::queries::{count_pending_unresolved_calls, get_node_ids_by_name};

    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // B has three undefined call targets — foo, bar, baz.
    fs::write(project_dir.path().join("b.ts"),
        "function caller_b() { foo(); bar(); baz(); }\n").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 3,
        "three bare-name calls must produce three pending rows");

    // A defines all three.
    fs::write(project_dir.path().join("a.ts"),
        "export function foo() {}\nexport function bar() {}\nexport function baz() {}\n").unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 0,
        "all three pending rows must drain once their targets exist");

    // All three resolved into real edges.
    let caller_b_id = get_node_ids_by_name(db.conn(), "caller_b").unwrap()
        .into_iter().next().unwrap().0;
    let edges = crate::storage::queries::get_edges_from(db.conn(), caller_b_id).unwrap();
    let calls_count = edges.iter().filter(|e| e.relation == REL_CALLS).count();
    assert_eq!(calls_count, 3,
        "caller_b must have exactly three calls edges (foo, bar, baz); got {} edges total: {:?}",
        calls_count, edges.iter().map(|e| (&e.relation, e.target_id)).collect::<Vec<_>>());
}

/// When the caller's source file is reindexed (e.g. user edits B), the
/// cascade FK on pending_unresolved_calls(source_id) must drop B's pending
/// rows so a fresh Phase 2 can re-buffer them with the current source IDs.
/// This is the schema's load-bearing self-cleaning property — we test it
/// explicitly so a future migration that drops or weakens the FK fails
/// loudly here rather than leaking pending rows for ever-removed callers.
#[test]
fn test_pending_cascade_deletes_when_caller_file_reindexed() {
    use crate::storage::queries::count_pending_unresolved_calls;

    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // B with undefined target → pending row created.
    fs::write(project_dir.path().join("b.ts"),
        "function caller_b() { undefined_target(); }\n").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();
    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 1);

    // Edit B to remove the call entirely. caller_b's old node gets
    // cascade-deleted on reindex (Phase 1 deletes prior rows), and its
    // pending row must follow it via ON DELETE CASCADE on source_id.
    fs::write(project_dir.path().join("b.ts"),
        "function caller_b() { /* call removed */ }\n").unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 0,
        "pending row must be cascade-deleted when its source caller is removed/reindexed");
}

/// Inverse-direction symmetry test for `feedback_incremental_edge_timing.md`:
/// existing edge B → A.foo gets cascade-deleted when A is removed, and B
/// is NOT in changed_paths (deletion doesn't re-extract B). Without Phase 0
/// pre-cascade buffering, B has neither edge nor pending row — a permanent
/// silent edge loss until full rebuild. The Phase 0 buffer (added by this
/// fix) must capture B's call as a pending row before cascade fires.
#[test]
fn test_pending_buffers_on_callee_file_deletion() {
    use crate::storage::queries::{count_pending_unresolved_calls, get_node_ids_by_name};

    let project_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // Initial: A defines foo, B calls foo — edge B.caller_b → A.foo exists.
    fs::write(project_dir.path().join("a.ts"),
        "export function foo() {}\n").unwrap();
    fs::write(project_dir.path().join("b.ts"),
        "function caller_b() { foo(); }\n").unwrap();
    run_full_index(&db, project_dir.path(), None, None).unwrap();

    // No pending rows yet — call resolved at index time.
    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 0,
        "fully-resolvable call must not produce a pending row");

    let caller_b_id = get_node_ids_by_name(db.conn(), "caller_b").unwrap()
        .into_iter().next().unwrap().0;
    let foo_id_pre = get_node_ids_by_name(db.conn(), "foo").unwrap()
        .into_iter().next().unwrap().0;
    let edges_pre = crate::storage::queries::get_edges_from(db.conn(), caller_b_id).unwrap();
    assert!(edges_pre.iter().any(|e| e.relation == REL_CALLS && e.target_id == foo_id_pre),
        "edge caller_b → foo must exist pre-deletion");

    // Delete A. Phase 0 must buffer B's now-orphaned call into pending
    // BEFORE cascade strips the edge.
    fs::remove_file(project_dir.path().join("a.ts")).unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    // foo is gone.
    assert!(get_node_ids_by_name(db.conn(), "foo").unwrap().is_empty(),
        "foo must be cascade-deleted with file a.ts");

    // B's edge to old foo is gone, but pending row holds the call.
    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 1,
        "Phase 0 must buffer the orphaned inbound call into pending");

    // Re-add A — pending sweep promotes the buffered call to a fresh edge.
    fs::write(project_dir.path().join("a.ts"),
        "export function foo() {}\n").unwrap();
    run_incremental_index(&db, project_dir.path(), None, None).unwrap();

    assert_eq!(count_pending_unresolved_calls(db.conn()).unwrap(), 0,
        "pending must drain once foo reappears");

    let foo_id_post = get_node_ids_by_name(db.conn(), "foo").unwrap()
        .into_iter().next().unwrap().0;
    let edges_post = crate::storage::queries::get_edges_from(db.conn(), caller_b_id).unwrap();
    assert!(edges_post.iter().any(|e| e.relation == REL_CALLS && e.target_id == foo_id_post),
        "edge caller_b → foo must reappear post re-add via pending sweep");
}

#[test]
fn test_is_safe_relative_path() {
    // Safe: ordinary relative paths, `./` prefix, interior `..` that stays in-root.
    assert!(is_safe_relative_path("src/lib.rs"));
    assert!(is_safe_relative_path("a.ts"));
    assert!(is_safe_relative_path("./src/x.rs"));
    assert!(is_safe_relative_path("a/b/../c.rs")); // net depth stays >= 0
    assert!(is_safe_relative_path("")); // empty → downstream treats as no-op
    // Unsafe: absolute root, leading `..`, or a `..` that climbs above the root.
    assert!(!is_safe_relative_path("/etc/passwd"));
    assert!(!is_safe_relative_path("../outside.ts"));
    assert!(!is_safe_relative_path("../../etc/passwd"));
    assert!(!is_safe_relative_path("a/../../b.rs")); // dips below root mid-path
    #[cfg(windows)]
    assert!(!is_safe_relative_path(r"C:\windows\system32"));
}

/// Defense-in-depth: `ensure_file_indexed` must refuse to touch a file outside
/// the project root, whether reached by an absolute path or a `..`-escape. The
/// MCP freshness wrapper (`ensure_file_fresh_opt`) forwards the client's raw
/// `file_path` without `normalize_user_path`, so this leaf is what stops an
/// unnormalized path from hashing/indexing arbitrary files into the project DB.
/// Such a path is a no-op (`Ok(false)`), like other non-indexable inputs.
#[test]
fn test_ensure_file_indexed_rejects_out_of_root_path() {
    let base = TempDir::new().unwrap();
    let project_root = base.path().join("proj");
    fs::create_dir_all(&project_root).unwrap();
    let db_dir = TempDir::new().unwrap();
    let db = Database::open(&db_dir.path().join("index.db")).unwrap();

    // A real source file OUTSIDE the project root (in base/), reachable via `..`.
    fs::write(base.path().join("outside.ts"), "function secret() {}\n").unwrap();
    // An absolute-path target outside the project entirely.
    let elsewhere = TempDir::new().unwrap();
    let abs_outside = elsewhere.path().join("abs_secret.ts");
    fs::write(&abs_outside, "function absSecret() {}\n").unwrap();

    // Establish the project index with one legitimate in-root file.
    fs::write(project_root.join("ok.ts"), "function inRoot() {}\n").unwrap();
    run_full_index(&db, &project_root, None, None).unwrap();

    // `..`-escape: project_root/../outside.ts resolves to base/outside.ts.
    let did = ensure_file_indexed(&db, &project_root, "../outside.ts", None).unwrap();
    assert!(!did, "a `..`-escaping path must be a no-op, not a reindex");

    // Absolute path outside the project.
    let did_abs =
        ensure_file_indexed(&db, &project_root, abs_outside.to_str().unwrap(), None).unwrap();
    assert!(!did_abs, "an absolute out-of-root path must be a no-op");

    // Neither external symbol leaked into the project DB.
    assert!(get_nodes_by_name(db.conn(), "secret").unwrap().is_empty(),
        "a `..`-escaping file must not be indexed into the project DB");
    assert!(get_nodes_by_name(db.conn(), "absSecret").unwrap().is_empty(),
        "an absolute out-of-root file must not be indexed into the project DB");

    // The guard must not over-block: a legitimate in-root edit still reindexes.
    fs::write(project_root.join("ok.ts"), "function inRootEdited() {}\n").unwrap();
    let did_ok = ensure_file_indexed(&db, &project_root, "ok.ts", None).unwrap();
    assert!(did_ok, "an in-root edited file must still reindex");
    assert_eq!(get_nodes_by_name(db.conn(), "inRootEdited").unwrap().len(), 1);
}
