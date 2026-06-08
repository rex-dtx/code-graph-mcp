use super::*;
use crate::domain::{REL_ROUTES_TO, REL_EXPORTS, REL_REFERENCES};

#[test]
fn test_extract_bash_call_relations() {
    let code = r#"#!/usr/bin/env bash

run_pipeline() {
    fetch_data "$1"
    transform_records
    /usr/bin/cat report.txt
    ./scripts/finalize.sh
    : noop
    [ -f /tmp/lock ] && exit 1
    echo "$RESULT"
    foo$VAR something
    $(dynamic_cmd)
}
"#;
    let relations = extract_relations(code, "bash").unwrap();
    let calls: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_CALLS && r.source_name == "run_pipeline")
        .map(|r| r.target_name.as_str())
        .collect();
    // Static, identifier-shaped callees → emitted (path prefix stripped).
    assert!(calls.contains(&"fetch_data"), "missing fetch_data, got: {:?}", calls);
    assert!(calls.contains(&"transform_records"), "missing transform_records, got: {:?}", calls);
    assert!(calls.contains(&"cat"), "missing cat (path prefix stripped), got: {:?}", calls);
    assert!(calls.contains(&"finalize.sh"), "missing finalize.sh (./prefix stripped), got: {:?}", calls);
    assert!(calls.contains(&"echo"), "missing echo, got: {:?}", calls);
    // Non-static / non-identifier-shaped → skipped.
    assert!(!calls.contains(&":"), "':' should be skipped, got: {:?}", calls);
    assert!(!calls.contains(&"["), "'[' test command should be skipped, got: {:?}", calls);
    assert!(!calls.iter().any(|c| c.contains('$')),
        "variable expansions / substitutions should be skipped, got: {:?}", calls);
}

#[test]
fn test_extract_c_include_imports() {
    let code = "#include \"local/utils.h\"\n#include <stdio.h>\n#include \"helpers.hpp\"\n\nint main() { return 0; }\n";
    let relations = extract_relations(code, "c").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(imports.contains(&"utils"),
        "C: missing utils (.h stripped, path stripped), got: {:?}", imports);
    assert!(imports.contains(&"stdio"),
        "C: missing stdio (system_lib_string), got: {:?}", imports);
    assert!(imports.contains(&"helpers"),
        "C: missing helpers (.hpp stripped), got: {:?}", imports);
    let import_sources: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.source_name.as_str())
        .collect();
    assert!(import_sources.iter().all(|s| *s == "<module>"),
        "all C #include sources should be <module>, got: {:?}", import_sources);
}

#[test]
fn test_extract_cpp_include_imports() {
    let code = "#include <vector>\n#include \"my/header.hpp\"\n\nint main() { return 0; }\n";
    let relations = extract_relations(code, "cpp").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(imports.contains(&"vector"),
        "C++: missing vector (system header, no extension), got: {:?}", imports);
    assert!(imports.contains(&"header"),
        "C++: missing header (.hpp stripped + path stripped), got: {:?}", imports);
}

#[test]
fn test_cpp_qualified_call_and_method_scope() {
    // C++ Class::method scope: qualified calls must produce edges (previously
    // dropped at extract_callee_name's `_ => None`), and method call sources
    // must be scoped as Class.method (previously `<module>` — scope_name has no
    // `name` field for C/C++ function_definition).
    let code = r#"
class Engine {
    void start() { ignite(); }
};
void Engine::ignite() { }
void run() { Engine::ignite(); }
"#;
    let relations = extract_relations(code, "cpp").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    // (1) qualified call `Engine::ignite()` inside run() must be an edge to `ignite`
    assert!(calls.iter().any(|(s, t)| *s == "run" && *t == "ignite"),
        "Engine::ignite() qualified call should yield run→ignite; got: {:?}", calls);
    // (2) in-class method scope: start() calls ignite() → source Engine.start
    assert!(calls.iter().any(|(s, t)| *s == "Engine.start" && *t == "ignite"),
        "in-class method call source should be Engine.start; got: {:?}", calls);
}

#[test]
fn test_extract_bash_source_imports() {
    let code = r#"#!/usr/bin/env bash
source ./lib/utils.sh
source "/etc/profile.d/lang.sh"
source 'helpers.bash'
. ~/.bashrc
. /usr/local/etc/init
source $HOME/dynamic.sh
source "${LIB_DIR}/runtime.sh"

bootstrap() {
    source ./conditional/feature.sh
    fetch_data
}
"#;
    let relations = extract_relations(code, "bash").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    // Static, .sh-stripped, path-stripped targets.
    assert!(imports.contains(&"utils"), "missing utils, got: {:?}", imports);
    assert!(imports.contains(&"lang"), "missing lang (double-quoted), got: {:?}", imports);
    assert!(imports.contains(&"helpers"),
        "missing helpers (single-quoted, .bash stripped), got: {:?}", imports);
    assert!(imports.contains(&".bashrc"),
        "missing .bashrc (no extension to strip), got: {:?}", imports);
    assert!(imports.contains(&"init"), "missing init, got: {:?}", imports);
    assert!(imports.contains(&"feature"),
        "missing feature (inside function), got: {:?}", imports);
    // Dynamic paths skipped.
    assert!(!imports.iter().any(|i| i.contains('$') || i.contains('{')),
        "dynamic paths should be skipped, got: {:?}", imports);
    // All imports use <module> as source_name (mirrors JS require pattern).
    let import_sources: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.source_name.as_str())
        .collect();
    assert!(import_sources.iter().all(|s| *s == "<module>"),
        "all import sources should be <module>, got: {:?}", import_sources);
    // `source ./conditional/feature.sh` inside bootstrap() must NOT also
    // emit a CALLS edge for `source`.
    let calls_to_source: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_CALLS && r.target_name == "source")
        .map(|r| r.source_name.as_str())
        .collect();
    assert!(calls_to_source.is_empty(),
        "`source` should not emit CALLS, got source_names: {:?}", calls_to_source);
}

#[test]
fn test_extract_call_relations() {
    let code = r#"
function handleLogin(req) {
    const user = validateToken(req.token);
    sendResponse(req, user);
}
"#;
    let relations = extract_relations(code, "typescript").unwrap();
    let calls: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(calls.contains(&"validateToken"), "got calls: {:?}", calls);
    assert!(calls.contains(&"sendResponse"), "got calls: {:?}", calls);
}

#[test]
fn test_extract_import_relations() {
    let code = r#"
import { UserService } from './services/user';
import jwt from 'jsonwebtoken';
"#;
    let relations = extract_relations(code, "typescript").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(imports.contains(&"UserService"), "got imports: {:?}", imports);
}

#[test]
fn test_extract_js_commonjs_require() {
    let code = r#"
const fs = require('node:fs');
const path = require('path');
const lifecycle = require('./lifecycle');
const versionUtils = require('../utils/version-utils.js');
"#;
    let relations = extract_relations(code, "javascript").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(imports.contains(&"fs"),        "expected fs import, got: {:?}", imports);
    assert!(imports.contains(&"path"),      "expected path import, got: {:?}", imports);
    assert!(imports.contains(&"lifecycle"), "expected lifecycle import, got: {:?}", imports);
    assert!(imports.contains(&"version-utils"),
        "expected stripped .js extension, got: {:?}", imports);
}

#[test]
fn test_extract_tsx_commonjs_require_and_route() {
    // TSX shares the JS/TS pipeline but went through a distinct config.name —
    // require() and Express route arms previously matched only "js"|"ts".
    let code = r#"
const React = require('react');
const { helpers } = require('./helpers');
app.get('/api/widgets', getWidgets);
"#;
    let relations = extract_relations(code, "tsx").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str()).collect();
    assert!(imports.contains(&"react"),   "tsx require('react'); got: {:?}", imports);
    assert!(imports.contains(&"helpers"), "tsx require('./helpers'); got: {:?}", imports);

    let routes: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_ROUTES_TO)
        .map(|r| r.target_name.as_str()).collect();
    assert!(routes.contains(&"getWidgets"),
        "tsx Express route target; got: {:?}", routes);
}

#[test]
fn test_extract_inherits_relations() {
    let code = r#"
class AdminService extends UserService {
    getPermissions() { return []; }
}
"#;
    let relations = extract_relations(code, "typescript").unwrap();
    let inherits: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_INHERITS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(inherits.contains(&"UserService"), "got inherits: {:?}", inherits);
}

#[test]
fn test_extract_express_routes() {
    let code = r#"
app.post('/api/login', handleLogin);
app.get('/api/users/:id', getUser);
"#;
    let relations = extract_relations(code, "typescript").unwrap();
    let routes: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_ROUTES_TO)
        .map(|r| (r.metadata.as_deref().unwrap_or(""), r.target_name.as_str()))
        .collect();
    assert!(routes.iter().any(|(meta, target)| meta.contains("/api/login") && *target == "handleLogin"),
        "got routes: {:?}", routes);
}

#[test]
fn test_extract_express_inline_arrow_routes() {
    let code = r#"
router.post('/api/login', async (req, res) => {
    const valid = validateCredentials(req.body.email);
    res.json({ token: 'ok' });
});
router.get('/api/users/:id', authMiddleware, async (req, res) => {
    res.json(user);
});
"#;
    let relations = extract_relations(code, "typescript").unwrap();
    let routes: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_ROUTES_TO)
        .map(|r| (r.metadata.as_deref().unwrap_or(""), r.target_name.as_str()))
        .collect();
    assert!(routes.iter().any(|(meta, _target)| meta.contains("/api/login") && meta.contains("\"inline\":true")),
        "should detect inline arrow handler route, got: {:?}", routes);
    assert!(routes.iter().any(|(meta, _target)| meta.contains("/api/users/:id")),
        "should detect multi-arg inline route, got: {:?}", routes);
}

#[test]
fn test_extract_python_flask_routes() {
    let code = r#"
@app.route('/api/users', methods=['GET'])
def get_users():
    return jsonify(users)
"#;
    let relations = extract_relations(code, "python").unwrap();
    let routes: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_ROUTES_TO)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(routes.contains(&"get_users"), "got routes: {:?}", routes);
}

// --- Task 2: Java inheritance ---

#[test]
fn test_extract_java_inheritance() {
    let code = "public class Dog extends Animal {\n    public void bark() {}\n}\n";
    let relations = extract_relations(code, "java").unwrap();
    let inherits: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_INHERITS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(inherits.contains(&"Animal"), "got: {:?}", inherits);
}

// --- Task 3: Python imports ---

#[test]
fn test_extract_python_import() {
    let code = "import os\n";
    let relations = extract_relations(code, "python").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(imports.contains(&"os"), "got: {:?}", imports);
}

#[test]
fn test_extract_python_from_import() {
    let code = "from collections import OrderedDict, defaultdict\n";
    let relations = extract_relations(code, "python").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(imports.contains(&"OrderedDict"), "got: {:?}", imports);
    assert!(imports.contains(&"defaultdict"), "got: {:?}", imports);
}

// --- Task 4: Python class inheritance ---

#[test]
fn test_extract_python_inheritance() {
    let code = "class Dog(Animal):\n    def bark(self):\n        pass\n";
    let relations = extract_relations(code, "python").unwrap();
    let inherits: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_INHERITS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(inherits.contains(&"Animal"), "got: {:?}", inherits);
}

#[test]
fn test_extract_rust_use_imports() {
    let source = r#"
use std::collections::HashMap;
use anyhow::Result;

fn main() {
    let m: HashMap<String, String> = HashMap::new();
}
"#;
    let tree = crate::parser::treesitter::parse_tree(source, "rust").unwrap();
    let relations = extract_relations_from_tree(&tree, source, "rust");
    let imports: Vec<&ParsedRelation> = relations.iter().filter(|r| r.relation == REL_IMPORTS).collect();
    assert!(imports.iter().any(|r| r.target_name == "HashMap"), "should import HashMap, got: {:?}", imports.iter().map(|r| &r.target_name).collect::<Vec<_>>());
    assert!(imports.iter().any(|r| r.target_name == "Result"), "should import Result, got: {:?}", imports.iter().map(|r| &r.target_name).collect::<Vec<_>>());
}

#[test]
fn test_extract_go_import_relations() {
    let source = r#"
package main

import (
    "fmt"
    "net/http"
)

func main() {
    fmt.Println("hello")
}
"#;
    let tree = crate::parser::treesitter::parse_tree(source, "go").unwrap();
    let relations = extract_relations_from_tree(&tree, source, "go");
    let imports: Vec<&ParsedRelation> = relations.iter().filter(|r| r.relation == REL_IMPORTS).collect();
    assert!(imports.iter().any(|r| r.target_name == "fmt"), "should import fmt, got: {:?}", imports.iter().map(|r| &r.target_name).collect::<Vec<_>>());
    assert!(imports.iter().any(|r| r.target_name == "http"), "should import http, got: {:?}", imports.iter().map(|r| &r.target_name).collect::<Vec<_>>());
}

#[test]
fn test_extract_rust_grouped_use_imports() {
    let source = r#"
use std::collections::{HashMap, HashSet, BTreeMap};
use std::io::Read as _;

fn main() {}
"#;
    let tree = crate::parser::treesitter::parse_tree(source, "rust").unwrap();
    let relations = extract_relations_from_tree(&tree, source, "rust");
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(imports.contains(&"HashMap"), "should import HashMap, got: {:?}", imports);
    assert!(imports.contains(&"HashSet"), "should import HashSet, got: {:?}", imports);
    assert!(imports.contains(&"BTreeMap"), "should import BTreeMap, got: {:?}", imports);
    assert!(imports.contains(&"Read"), "should import Read (not 'Read as _'), got: {:?}", imports);
    // Should NOT contain braces or 'as _'
    assert!(!imports.iter().any(|i| i.contains('{')), "should not have brace in import names: {:?}", imports);
}

#[test]
fn test_python_route_no_false_positive_on_cache_get() {
    // @cache.get should NOT be detected as a route (cache is not a known framework receiver)
    let code = r#"
@cache.get('/dashboard')
def get_dashboard():
    return render_template('dashboard.html')
"#;
    let relations = extract_relations(code, "python").unwrap();
    let routes: Vec<&ParsedRelation> = relations.iter()
        .filter(|r| r.relation == REL_ROUTES_TO)
        .collect();
    assert!(routes.is_empty(), "should not detect route from @cache.get, got: {:?}",
        routes.iter().map(|r| (&r.source_name, &r.target_name)).collect::<Vec<_>>());
}

#[test]
fn test_python_route_no_false_positive_on_getter() {
    // A decorator containing "get" as substring (e.g., @target) should NOT be detected as a route
    let code = r#"
@cache_target('/dashboard')
def get_dashboard():
    return render_template('dashboard.html')
"#;
    let relations = extract_relations(code, "python").unwrap();
    let routes: Vec<&ParsedRelation> = relations.iter()
        .filter(|r| r.relation == REL_ROUTES_TO)
        .collect();
    assert!(routes.is_empty(), "should not detect route from @login_required, got: {:?}", routes.iter().map(|r| (&r.source_name, &r.target_name)).collect::<Vec<_>>());
}

#[test]
fn test_python_route_detects_dotted_pattern() {
    // @app.get('/path') should still be detected
    let code = r#"
@app.get('/api/items')
def list_items():
    return items
"#;
    let relations = extract_relations(code, "python").unwrap();
    let routes: Vec<&ParsedRelation> = relations.iter()
        .filter(|r| r.relation == REL_ROUTES_TO)
        .collect();
    assert!(!routes.is_empty(), "should detect route from @app.get, got no routes");
    assert!(routes[0].target_name == "list_items", "target should be list_items");
}

/// Regression: Python `call` nodes (tree-sitter uses `call`, not `call_expression`)
/// must produce REL_CALLS edges. Previously dropped because the extractor only
/// handled `"call_expression"` and `"call" if config.name == "ruby"`, leaving
/// Python in the default-no-match path. README documents Python as "Full" tier
/// (calls + imports + inheritance + routes + test markers) — without this,
/// every Python project shows caller_count=0 and false-positive dead code.
#[test]
fn test_extract_python_bare_call() {
    let code = "def caller():\n    return used_fn()\n";
    let relations = extract_relations(code, "python").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    assert!(calls.iter().any(|(s, t)| *s == "caller" && *t == "used_fn"),
        "Python bare call `used_fn()` inside `caller` should emit REL_CALLS edge; got: {:?}", calls);
}

#[test]
fn test_extract_python_method_call() {
    // obj.method() — Python `attribute` node inside `call.function`.
    let code = "def caller(obj):\n    return obj.method()\n";
    let relations = extract_relations(code, "python").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    assert!(calls.iter().any(|(s, t)| *s == "caller" && *t == "method"),
        "Python method call `obj.method()` inside `caller` should emit REL_CALLS edge with target=method; got: {:?}", calls);
}

#[test]
fn test_extract_rust_impl_trait() {
    let source = r#"
struct MyStruct;
trait MyTrait { fn do_thing(&self); fn other(&self); }
impl MyTrait for MyStruct {
    fn do_thing(&self) {}
    fn other(&self) {}
}
"#;
    let relations = extract_relations(source, "rust").unwrap();
    let impls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_IMPLEMENTS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    // Type-level: MyStruct implements MyTrait
    assert!(impls.contains(&("MyStruct", "MyTrait")), "got implements: {:?}", impls);
    // Method-level: MyStruct → do_thing, MyStruct → other
    assert!(impls.contains(&("MyStruct", "do_thing")), "method-level edge missing for do_thing: {:?}", impls);
    assert!(impls.contains(&("MyStruct", "other")), "method-level edge missing for other: {:?}", impls);
    assert_eq!(impls.len(), 3, "expected 3 implements edges (1 type + 2 methods), got: {:?}", impls);
}

#[test]
fn test_extract_rust_impl_trait_generic_type_strips_params() {
    // Regression: `impl<'a, W: Write> Write for CapWriter<'a, W>` stored
    // source_name as the verbatim "CapWriter<'a, W>" text from the tree-sitter
    // type field. Phase 2 source resolution does exact name match against
    // local node names ("CapWriter"), so source_ids ended up empty and no
    // implements edges emitted — every method on a generic trait impl
    // appeared as dead code. Strip generics so source_name is the bare type.
    let source = r#"
trait MyTrait { fn do_thing(&self); }
struct Generic<'a, W: std::io::Write>(&'a W);
impl<'a, W: std::io::Write> MyTrait for Generic<'a, W> {
    fn do_thing(&self) {}
}
"#;
    let relations = extract_relations(source, "rust").unwrap();
    let impls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_IMPLEMENTS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    assert!(
        impls.contains(&("Generic", "MyTrait")),
        "type-level edge must use bare struct name (no generics); got: {:?}", impls
    );
    assert!(
        impls.contains(&("Generic", "do_thing")),
        "method-level edge must use bare struct name; got: {:?}", impls
    );
}

#[test]
fn test_bare_impl_no_implements_relation() {
    // `impl Type { ... }` (no trait) should produce zero REL_IMPLEMENTS relations
    let source = r#"
struct MyStruct;
impl MyStruct {
    fn new() -> Self { MyStruct }
    fn do_thing(&self) {}
}
"#;
    let relations = extract_relations(source, "rust").unwrap();
    let impls: Vec<_> = relations.iter()
        .filter(|r| r.relation == REL_IMPLEMENTS)
        .collect();
    assert!(impls.is_empty(), "bare impl should produce no implements relations, got: {:?}",
        impls.iter().map(|r| (&r.source_name, &r.target_name)).collect::<Vec<_>>());
}

#[test]
fn test_rust_struct_instantiation_creates_calls_edge() {
    let source = r#"
struct Config { verbose: bool, path: String }

fn build_config() -> Config {
    Config { verbose: true, path: "/tmp".into() }
}
"#;
    let relations = extract_relations(source, "rust").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    assert!(calls.contains(&("build_config", "Config")),
        "struct instantiation should create calls edge, got: {:?}", calls);
}

#[test]
fn test_rust_scoped_struct_instantiation() {
    let source = r#"
fn create() {
    let node = crate::parser::NodeRecord { name: "foo".into() };
}
"#;
    let relations = extract_relations(source, "rust").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    // Should strip path prefix, keeping just "NodeRecord"
    assert!(calls.contains(&("create", "NodeRecord")),
        "scoped struct should strip path, got: {:?}", calls);
}

#[test]
fn test_rust_path_reference_to_const_emits_references_edge() {
    let src = r#"
fn build() -> String {
    let w = crate::domain::SHARED;
    w.to_string()
}
"#;
    let rels = extract_relations(src, "rust").unwrap();
    let has_ref = rels.iter().any(|r|
        r.relation == REL_REFERENCES && r.target_name == "SHARED");
    assert!(has_ref, "expected a references edge to SHARED; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_rust_path_reference_as_call_argument_emits_references_edge() {
    // A path-qualified value passed as a call ARGUMENT (not the callee) is a
    // real value usage and MUST still emit a references edge — distinct from
    // the callee case, where the `function`-field path is already a calls edge.
    let src = r#"fn f() { do_thing(crate::domain::CB); }"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "CB"),
        "arg-position path must emit a references edge to CB; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_rust_path_reference_call_callee_does_not_emit_references_edge() {
    let src = r#"fn build() { crate::domain::compute(); }"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_CALLS && r.target_name == "compute"),
        "call must be a calls edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "compute"),
        "a called fn must NOT also be a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_rust_path_reference_struct_expr_path_does_not_emit_references_edge() {
    // Path-qualified struct instantiation uses `scoped_type_identifier`, whose
    // inner segments are `scoped_identifier` nodes. They must NOT leak a
    // `references` edge to an intermediate path segment ("parser") — that path
    // is already covered by the `calls` edge to the struct ("NodeRecord").
    let src = r#"fn create() { let node = crate::parser::NodeRecord { name: 1 }; }"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES),
        "struct-expr type path must not emit references edges; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_extract_go_http_routes() {
    let code = r#"
package main

func main() {
    http.HandleFunc("/api/health", healthCheck)
}
"#;
    let relations = extract_relations(code, "go").unwrap();
    assert!(relations.iter().any(|r| r.relation == REL_ROUTES_TO && r.target_name == "healthCheck"),
        "got relations: {:?}", relations.iter().map(|r| (&r.relation, &r.target_name)).collect::<Vec<_>>());
}

#[test]
fn test_extract_ts_implements() {
    let code = "class UserService implements IUserService {\n    getUser() { return null; }\n}\n";
    let relations = extract_relations(code, "typescript").unwrap();
    let impls: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPLEMENTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(impls.contains(&"IUserService"), "got implements: {:?}", impls);
}

#[test]
fn test_extract_java_implements() {
    let code = "public class ArrayList implements List, Serializable {\n}\n";
    let relations = extract_relations(code, "java").unwrap();
    let impls: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPLEMENTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(impls.contains(&"List"), "got implements: {:?}", impls);
}

#[test]
fn test_extract_ts_exports() {
    let code = "export function handleLogin(req: Request) {}\nexport class AuthService {}\n";
    let relations = extract_relations(code, "typescript").unwrap();
    let exports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_EXPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(exports.contains(&"handleLogin"), "got exports: {:?}", exports);
    assert!(exports.contains(&"AuthService"), "got exports: {:?}", exports);
}

#[test]
fn test_go_selector_call_relations() {
    // Go receiver.Method() calls should be extracted
    let code = r#"
package main

import "fmt"

func main() {
    fmt.Println("hello")
    http.HandleFunc("/", handler)
}
"#;
    let relations = extract_relations(code, "go").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    assert!(calls.contains(&("main", "Println")),
        "fmt.Println() should create call relation, got: {:?}", calls);
    assert!(calls.contains(&("main", "HandleFunc")),
        "http.HandleFunc() should create call relation, got: {:?}", calls);
}

#[test]
fn test_rust_scoped_call_relations() {
    // Self::method() and Path::func() should be extracted as call relations
    let code = r#"
impl Database {
    fn open() {
        Self::open_impl(false);
    }
    fn open_impl(flag: bool) {
        HashMap::new();
    }
}
"#;
    let relations = extract_relations(code, "rust").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    assert!(calls.contains(&("open", "open_impl")),
        "Self::open_impl() should create call relation, got: {:?}", calls);
    assert!(calls.contains(&("open_impl", "new")),
        "HashMap::new() should create call relation, got: {:?}", calls);
}

#[test]
fn test_rust_method_call_on_object() {
    // obj.method() should also be extracted as a call relation
    let code = r#"
fn test_func() {
    let server = McpServer::from_project_root(path).unwrap();
    server.handle_message(init).unwrap();
    tool_call_json("search", args);
}
"#;
    let relations = extract_relations(code, "rust").unwrap();
    eprintln!("All relations:");
    for r in &relations {
        eprintln!("  {} --[{}]--> {}", r.source_name, r.relation, r.target_name);
    }
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    eprintln!("Calls: {:?}", calls);
    assert!(calls.contains(&("test_func", "from_project_root")),
        "McpServer::from_project_root() should create call, got: {:?}", calls);
    assert!(calls.contains(&("test_func", "handle_message")),
        "server.handle_message() should create call, got: {:?}", calls);
    assert!(calls.contains(&("test_func", "tool_call_json")),
        "tool_call_json() should create call, got: {:?}", calls);
}

#[test]
fn test_rust_try_expr_and_match_calls() {
    // Reproduce actual patterns from main.rs run_serve: try expressions, match scrutinee, method calls
    let code = r#"
fn run_serve() {
    let project_root = std::env::current_dir().unwrap();
    let server = code_graph_mcp::mcp::server::McpServer::from_project_root(&project_root).unwrap();
    server.set_notify_writer(Box::new(io::stdout()));
    match server.handle_message(&buf) {
        Ok(Some(response)) => {
            writeln!(stdout, "{}", response).unwrap();
            stdout.flush().unwrap();
        }
        Ok(None) => {}
        Err(e) => {}
    }
    server.run_startup_tasks();
    server.flush_metrics();
}
"#;
    let relations = extract_relations(code, "rust").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    assert!(calls.contains(&("run_serve", "from_project_root")),
        "McpServer::from_project_root() missing, got: {:?}", calls);
    assert!(calls.contains(&("run_serve", "set_notify_writer")),
        "server.set_notify_writer() missing, got: {:?}", calls);
    assert!(calls.contains(&("run_serve", "handle_message")),
        "server.handle_message() missing, got: {:?}", calls);
    assert!(calls.contains(&("run_serve", "run_startup_tasks")),
        "server.run_startup_tasks() missing, got: {:?}", calls);
    assert!(calls.contains(&("run_serve", "flush_metrics")),
        "server.flush_metrics() missing, got: {:?}", calls);
}

#[test]
fn test_scope_qualification_class_method() {
    // Methods inside a class should have scope qualified as ClassName.method_name
    let code = r#"
class UserService {
    getUser(id) {
        return this.db.findById(id);
    }
    deleteUser(id) {
        this.getUser(id);
        this.db.remove(id);
    }
}
"#;
    let relations = extract_relations(code, "typescript").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    // The scope for getUser should be "UserService.getUser", not just "getUser"
    assert!(calls.iter().any(|(src, tgt)| *src == "UserService.getUser" && *tgt == "findById"),
        "getUser scope should be qualified as UserService.getUser, got calls: {:?}", calls);
    assert!(calls.iter().any(|(src, tgt)| *src == "UserService.deleteUser" && *tgt == "getUser"),
        "deleteUser scope should be qualified as UserService.deleteUser, got calls: {:?}", calls);
}

#[test]
fn test_scope_standalone_function_not_qualified() {
    // Standalone functions (not inside a class) should NOT be qualified with a class prefix
    let code = r#"
function doWork() {
    process();
}
"#;
    let relations = extract_relations(code, "typescript").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    assert!(calls.iter().any(|(src, tgt)| *src == "doWork" && *tgt == "process"),
        "standalone function scope should remain unqualified, got calls: {:?}", calls);
}

#[test]
fn test_rust_deeply_nested_scoped_call() {
    // code_graph_mcp::cli::cmd_show() should extract "cmd_show" as the callee
    let code = r#"
fn main() {
    print_version();
    code_graph_mcp::cli::cmd_show(&project_root, &args);
    std::env::current_dir();
}
fn print_version() {}
"#;
    let relations = extract_relations(code, "rust").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    eprintln!("All calls: {:?}", calls);
    assert!(calls.contains(&("main", "print_version")),
        "simple call should work, got: {:?}", calls);
    assert!(calls.contains(&("main", "cmd_show")),
        "deeply nested scoped call should extract rightmost name, got: {:?}", calls);
    assert!(calls.contains(&("main", "current_dir")),
        "std::env::current_dir() should extract current_dir, got: {:?}", calls);
}

#[test]
fn test_rust_match_arm_dispatch_calls() {
    // Calls inside match arms should be detected — this is the pattern used by
    // handle_tool (self.tool_*) and main (code_graph_mcp::cli::cmd_*)
    let code = r#"
impl Server {
    fn handle_tool(&self, name: &str) -> i32 {
        let result = match name {
            "search" => self.tool_search(),
            "map" => self.tool_map(),
            _ => 0,
        };
        self.log_result();
        result
    }
}
"#;
    let relations = extract_relations(code, "rust").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    eprintln!("Match arm calls: {:?}", calls);
    // Note: Rust `impl` blocks don't set class context (unlike class {} in TS/JS),
    // so scope is just "handle_tool" not "Server.handle_tool"
    assert!(calls.contains(&("handle_tool", "tool_search")),
        "self.tool_search() in match arm should be detected, got: {:?}", calls);
    assert!(calls.contains(&("handle_tool", "tool_map")),
        "self.tool_map() in match arm should be detected, got: {:?}", calls);
    assert!(calls.contains(&("handle_tool", "log_result")),
        "self.log_result() outside match should be detected, got: {:?}", calls);
}

#[test]
fn test_real_handle_tool_dispatch_pattern() {
    // Reproduce the exact pattern from McpServer::handle_tool in mod.rs
    let code = r#"
impl McpServer {
    fn handle_tool(&self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
        let start = std::time::Instant::now();
        let result = match name {
            "semantic_code_search" => self.tool_semantic_search(args),
            "get_call_graph" => self.tool_get_call_graph(args),
            "find_http_route" | "trace_http_chain" => self.tool_trace_http_chain(args),
            "get_ast_node" | "read_snippet" => self.tool_get_ast_node(args),
            "start_watch" => self.tool_start_watch(),
            "stop_watch" => self.tool_stop_watch(),
            "get_index_status" => self.tool_get_index_status(),
            "rebuild_index" => self.tool_rebuild_index(args),
            "impact_analysis" => self.tool_impact_analysis(args),
            "module_overview" => self.tool_module_overview(args),
            "dependency_graph" => self.tool_dependency_graph(args),
            "find_similar_code" => self.tool_find_similar_code(args),
            "project_map" => self.tool_project_map(args),
            "ast_search" => self.tool_ast_search(args),
            "find_references" => self.tool_find_references(args),
            "find_dead_code" => self.tool_find_dead_code(args),
            _ => Err(anyhow!("Unknown tool")),
        };
        let elapsed = start.elapsed();
        lock_or_recover(&self.metrics, "metrics")
            .record_tool_call(name, elapsed.as_millis() as u64, false);
        result
    }
}
"#;
    let relations = extract_relations(code, "rust").unwrap();
    let calls: Vec<(&str, &str)> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| (r.source_name.as_str(), r.target_name.as_str()))
        .collect();
    eprintln!("All calls from handle_tool ({}):", calls.len());
    for (src, tgt) in &calls {
        eprintln!("  {} -> {}", src, tgt);
    }
    assert!(calls.iter().any(|(_, t)| *t == "tool_semantic_search"),
        "tool_semantic_search not found in: {:?}", calls);
    assert!(calls.iter().any(|(_, t)| *t == "tool_find_dead_code"),
        "tool_find_dead_code not found in: {:?}", calls);
    assert!(calls.iter().any(|(_, t)| *t == "lock_or_recover"),
        "lock_or_recover not found in: {:?}", calls);
    assert!(calls.iter().any(|(_, t)| *t == "record_tool_call"),
        "record_tool_call not found in: {:?}", calls);
}

/// Every ParsedRelation returned by extract_relations must be stamped with
/// the source language of its originating file. This invariant underpins
/// the same-language edge resolution in pipeline.rs; a parser regression
/// that silently returned empty source_language would reintroduce the
/// cross-language false-positive calls edges we guarded against.
#[test]
fn test_source_language_stamped_on_all_relations() {
    // One minimal sample per supported language. We only assert:
    //   (a) every returned relation carries the right source_language stamp
    //   (b) across all cases combined, at least one relation was produced
    //       (guards against the parser regressing to "zero relations globally")
    let cases = &[
        ("rust", "fn a() { b(); } fn b() {}"),
        ("javascript", "function a() { b(); } function b() {}"),
        ("typescript", "function a() { b(); } function b() {}"),
        ("go", "package p\nfunc a() { b() }\nfunc b() {}\n"),
    ];
    let mut total_relations = 0usize;
    for (lang, src) in cases {
        let relations = extract_relations(src, lang).unwrap();
        total_relations += relations.len();
        for r in &relations {
            assert_eq!(
                r.source_language, *lang,
                "{}: relation {:?} → {:?} has wrong source_language {:?}",
                lang, r.source_name, r.target_name, r.source_language
            );
        }
    }
    assert!(
        total_relations > 0,
        "expected at least one relation across all language samples — parser regression?"
    );
}

// --- Tier 2 inheritance smoke tests (Phase A audit) ---
// Expected-behavior tests: a failure here = a real bug to fix in Phase B.

#[test]
fn test_extract_kotlin_inheritance() {
    // Kotlin: `class S : Base(), Cloneable` — Base is concrete (constructor
    // call), Cloneable is interface. Both should produce INHERITS edges
    // (Kotlin doesn't syntactically distinguish, the type system does).
    let code = "class UserService : BaseService(), Cloneable {\n    fun foo() {}\n}\n";
    let relations = extract_relations(code, "kotlin").unwrap();
    let inherits: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_INHERITS || r.relation == REL_IMPLEMENTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(inherits.contains(&"BaseService"),
        "Kotlin: missing BaseService, got: {:?}", inherits);
    assert!(inherits.contains(&"Cloneable"),
        "Kotlin: missing Cloneable, got: {:?}", inherits);
}

#[test]
fn test_extract_swift_inheritance() {
    // Swift: `class S: BaseService, Codable` — comma-separated conformance.
    let code = "class UserService: BaseService, Codable, Hashable {\n    func foo() {}\n}\n";
    let relations = extract_relations(code, "swift").unwrap();
    let inherits: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_INHERITS || r.relation == REL_IMPLEMENTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(inherits.contains(&"BaseService"),
        "Swift: missing BaseService, got: {:?}", inherits);
    assert!(inherits.contains(&"Codable"),
        "Swift: missing Codable, got: {:?}", inherits);
    assert!(inherits.contains(&"Hashable"),
        "Swift: missing Hashable, got: {:?}", inherits);
}

#[test]
fn test_extract_dart_inheritance() {
    // Dart has 3 inheritance keywords: extends (single), implements (multi),
    // with (mixin, multi). All conceptually contribute to type lineage.
    let code = "class UserService extends BaseService implements Loggable, Cacheable {\n  void foo() {}\n}\n";
    let relations = extract_relations(code, "dart").unwrap();
    let lineage: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_INHERITS || r.relation == REL_IMPLEMENTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(lineage.contains(&"BaseService"),
        "Dart: missing BaseService (extends), got: {:?}", lineage);
    assert!(lineage.contains(&"Loggable"),
        "Dart: missing Loggable (implements), got: {:?}", lineage);
    assert!(lineage.contains(&"Cacheable"),
        "Dart: missing Cacheable (implements), got: {:?}", lineage);
}

#[test]
fn test_extract_php_inheritance() {
    // PHP: extends (single class) + implements (multiple interfaces).
    let code = "<?php\nclass UserService extends BaseService implements Loggable, Cacheable {\n    public function foo() {}\n}\n";
    let relations = extract_relations(code, "php").unwrap();
    let inherits: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_INHERITS)
        .map(|r| r.target_name.as_str())
        .collect();
    let implements: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPLEMENTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(inherits.contains(&"BaseService"),
        "PHP: missing BaseService (extends), got INHERITS: {:?}", inherits);
    assert!(implements.contains(&"Loggable"),
        "PHP: missing Loggable (implements), got IMPLEMENTS: {:?}", implements);
    assert!(implements.contains(&"Cacheable"),
        "PHP: missing Cacheable (implements), got IMPLEMENTS: {:?}", implements);
}

#[test]
fn test_extract_ruby_inheritance() {
    let code = "class UserService < BaseService\n  def foo\n  end\nend\n";
    let relations = extract_relations(code, "ruby").unwrap();
    let inherits: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_INHERITS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(inherits.contains(&"BaseService"),
        "Ruby: missing BaseService, got: {:?}", inherits);
}

// --- Tier 2 calls + imports smoke tests (Phase C audit) ---

#[test]
fn test_extract_kotlin_calls() {
    let code = "fun process() {\n    fetch()\n    store()\n}\n";
    let relations = extract_relations(code, "kotlin").unwrap();
    let calls: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_CALLS && r.source_name == "process")
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(calls.contains(&"fetch"),
        "Kotlin: missing fetch call, got: {:?}", calls);
    assert!(calls.contains(&"store"),
        "Kotlin: missing store call, got: {:?}", calls);
}

#[test]
fn test_extract_kotlin_imports() {
    let code = "import com.example.UserService\nimport kotlinx.coroutines.flow.Flow\n\nfun process() {}\n";
    let relations = extract_relations(code, "kotlin").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(!imports.is_empty(),
        "Kotlin: expected at least one IMPORTS edge, got 0 (relations: {:?})",
        relations.iter().map(|r| (&r.relation, &r.target_name)).collect::<Vec<_>>());
    assert!(imports.iter().any(|i| i == &"UserService" || i.contains("UserService")),
        "Kotlin: missing UserService import, got: {:?}", imports);
}

#[test]
fn test_extract_swift_calls() {
    let code = "func process() {\n    fetch()\n    store()\n}\n";
    let relations = extract_relations(code, "swift").unwrap();
    let calls: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_CALLS && r.source_name == "process")
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(calls.contains(&"fetch"),
        "Swift: missing fetch call, got: {:?}", calls);
    assert!(calls.contains(&"store"),
        "Swift: missing store call, got: {:?}", calls);
}

#[test]
fn test_extract_swift_imports() {
    let code = "import Foundation\nimport UIKit\n\nfunc process() {}\n";
    let relations = extract_relations(code, "swift").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(imports.contains(&"Foundation"),
        "Swift: missing Foundation, got: {:?}", imports);
    assert!(imports.contains(&"UIKit"),
        "Swift: missing UIKit, got: {:?}", imports);
}

#[test]
fn test_extract_dart_calls() {
    let code = "void process() {\n  fetch();\n  store();\n}\n";
    let relations = extract_relations(code, "dart").unwrap();
    let calls: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_CALLS && r.source_name == "process")
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(calls.contains(&"fetch"),
        "Dart: missing fetch call, got: {:?}", calls);
    assert!(calls.contains(&"store"),
        "Dart: missing store call, got: {:?}", calls);
}

#[test]
fn test_extract_dart_imports() {
    let code = "import 'package:flutter/material.dart';\nimport 'dart:async';\n\nvoid process() {}\n";
    let relations = extract_relations(code, "dart").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(!imports.is_empty(),
        "Dart: expected at least one IMPORTS edge, got 0 (relations: {:?})",
        relations.iter().map(|r| (&r.relation, &r.target_name)).collect::<Vec<_>>());
    assert!(imports.iter().any(|i| i.contains("material") || i.contains("flutter")),
        "Dart: missing material/flutter import, got: {:?}", imports);
    assert!(imports.iter().any(|i| i.contains("async") || i.contains("dart:async")),
        "Dart: missing async import, got: {:?}", imports);
}

#[test]
fn test_extract_php_calls() {
    let code = "<?php\nfunction process() {\n    fetch();\n    store();\n}\n";
    let relations = extract_relations(code, "php").unwrap();
    let calls: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_CALLS && r.source_name == "process")
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(calls.contains(&"fetch"),
        "PHP: missing fetch call, got: {:?}", calls);
    assert!(calls.contains(&"store"),
        "PHP: missing store call, got: {:?}", calls);
}

#[test]
fn test_extract_php_imports() {
    let code = "<?php\nuse App\\Services\\UserService;\nuse App\\Models\\Order;\n\nfunction process() {}\n";
    let relations = extract_relations(code, "php").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(!imports.is_empty(),
        "PHP: expected at least one IMPORTS edge, got 0 (relations: {:?})",
        relations.iter().map(|r| (&r.relation, &r.target_name)).collect::<Vec<_>>());
    assert!(imports.iter().any(|i| i.contains("UserService")),
        "PHP: missing UserService, got: {:?}", imports);
    assert!(imports.iter().any(|i| i.contains("Order")),
        "PHP: missing Order, got: {:?}", imports);
}

#[test]
fn test_extract_ruby_calls() {
    // Bare names (`fetch`, `store`) in Ruby are statically ambiguous
    // (local var read vs. method call) — tree-sitter-ruby parses them
    // as `identifier`, not `call`. Use parens to force the call shape.
    let code = "def process\n  fetch()\n  store()\nend\n";
    let relations = extract_relations(code, "ruby").unwrap();
    let calls: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_CALLS && r.source_name == "process")
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(calls.contains(&"fetch"),
        "Ruby: missing fetch call, got: {:?}", calls);
    assert!(calls.contains(&"store"),
        "Ruby: missing store call, got: {:?}", calls);
}

#[test]
fn test_extract_ruby_imports() {
    let code = "require 'json'\nrequire_relative 'helper'\n\ndef process\nend\n";
    let relations = extract_relations(code, "ruby").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(imports.contains(&"json"),
        "Ruby: missing json (require), got: {:?}", imports);
    assert!(imports.contains(&"helper"),
        "Ruby: missing helper (require_relative), got: {:?}", imports);
}

#[test]
fn test_extract_csharp_calls() {
    let code = "class App {\n    void Process() {\n        Fetch();\n        Store();\n    }\n}\n";
    let relations = extract_relations(code, "csharp").unwrap();
    let calls: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_CALLS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(calls.contains(&"Fetch"),
        "C#: missing Fetch call, got: {:?}", calls);
    assert!(calls.contains(&"Store"),
        "C#: missing Store call, got: {:?}", calls);
}

#[test]
fn test_extract_csharp_imports() {
    let code = "using System;\nusing System.Collections.Generic;\n\nclass App {}\n";
    let relations = extract_relations(code, "csharp").unwrap();
    let imports: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPORTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(!imports.is_empty(),
        "C#: expected at least one IMPORTS edge, got 0 (relations: {:?})",
        relations.iter().map(|r| (&r.relation, &r.target_name)).collect::<Vec<_>>());
    assert!(imports.iter().any(|i| i == &"System" || i.contains("System")),
        "C#: missing System import, got: {:?}", imports);
}

#[test]
fn test_extract_csharp_inheritance() {
    // C#: `class S : Base, IInterface` — current code uses IFoo prefix
    // heuristic to split into INHERITS (Base) vs IMPLEMENTS (IInterface).
    let code = "class UserService : BaseService, IDisposable, ICloneable {\n    public void Foo() {}\n}\n";
    let relations = extract_relations(code, "csharp").unwrap();
    let inherits: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_INHERITS)
        .map(|r| r.target_name.as_str())
        .collect();
    let implements: Vec<&str> = relations.iter()
        .filter(|r| r.relation == REL_IMPLEMENTS)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(inherits.contains(&"BaseService"),
        "C#: missing BaseService (INHERITS), got: {:?}", inherits);
    assert!(implements.contains(&"IDisposable"),
        "C#: missing IDisposable (IMPLEMENTS), got: {:?}", implements);
    assert!(implements.contains(&"ICloneable"),
        "C#: missing ICloneable (IMPLEMENTS), got: {:?}", implements);
}

#[test]
fn test_rust_callee_path_qualifier_strips_crate() {
    let code = "fn caller() { crate::snapshot::create(); }";
    let relations = extract_relations(code, "rust").unwrap();
    let call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "create")
        .expect("missing call to create");
    assert_eq!(
        call.metadata.as_deref(),
        Some(r#"{"q":"path","v":"snapshot"}"#),
        "metadata should encode Path qualifier with crate stripped"
    );
}

// T3: single-segment Type::method path
#[test]
fn test_rust_callee_type_method_call_path() {
    let code = r#"fn caller() { File::create("/tmp/x"); }"#;
    let relations = extract_relations(code, "rust").unwrap();
    let call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "create")
        .expect("missing call to create");
    assert_eq!(
        call.metadata.as_deref(),
        Some(r#"{"q":"path","v":"File"}"#),
        "single-segment Path with non-reserved name should be preserved"
    );
}

// T4: reserved-only path collapses to bare
#[test]
fn test_rust_callee_crate_only_path_collapses_to_bare() {
    let code = "fn caller() { crate::foo(); }";
    let relations = extract_relations(code, "rust").unwrap();
    let call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "foo")
        .expect("missing call to foo");
    assert_eq!(
        call.metadata, None,
        "crate::foo() qualifier collapses to Bare after stripping reserved prefix"
    );
}

// T5: super:: strip, multi-segment, chained reserved prefixes
#[test]
fn test_rust_callee_super_prefix_stripped() {
    // super:: must be stripped per reserved-prefix rule.
    let code = "fn caller() { super::sibling::foo(); }";
    let relations = extract_relations(code, "rust").unwrap();
    let call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "foo")
        .expect("missing call to foo");
    assert_eq!(
        call.metadata.as_deref(),
        Some(r#"{"q":"path","v":"sibling"}"#),
    );
}

#[test]
fn test_rust_callee_multi_segment_path_preserved() {
    let code = "fn caller() { crate::a::b::c::deep(); }";
    let relations = extract_relations(code, "rust").unwrap();
    let call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "deep")
        .expect("missing call to deep");
    assert_eq!(
        call.metadata.as_deref(),
        Some(r#"{"q":"path","v":"a::b::c"}"#),
    );
}

#[test]
fn test_rust_callee_chained_reserved_prefixes_stripped() {
    // Multiple consecutive reserved prefixes: ensure drain(..skip) consumes
    // ALL leading reserved segments, not just the first.
    let code = "fn caller() { super::super::foo(); }";
    let relations = extract_relations(code, "rust").unwrap();
    let call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "foo")
        .expect("missing call to foo");
    assert_eq!(
        call.metadata, None,
        "two consecutive `super::` segments + bare name → fully stripped → Bare"
    );
}

#[test]
fn test_rust_callee_obj_method_receiver_qualifier() {
    let code = "fn caller(p: &std::path::Path) { p.exists(); }";
    let relations = extract_relations(code, "rust").unwrap();
    let call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "exists")
        .expect("missing call to exists");
    assert_eq!(
        call.metadata.as_deref(),
        Some(r#"{"q":"recv","v":"p"}"#),
        "obj.method() where obj is a plain identifier emits Receiver qualifier"
    );
}

#[test]
fn test_rust_callee_builder_chain_qualifier() {
    let code = r#"fn caller() {
        OpenOptions::new().create(true).open("/tmp/x");
    }"#;
    let relations = extract_relations(code, "rust").unwrap();

    // OpenOptions::new() → Path
    let new_call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "new")
        .expect("missing call to new");
    assert_eq!(
        new_call.metadata.as_deref(),
        Some(r#"{"q":"path","v":"OpenOptions"}"#),
    );

    // .create(true) — receiver is call_expression → Chain
    let create_call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "create")
        .expect("missing call to create");
    assert_eq!(
        create_call.metadata.as_deref(),
        Some(r#"{"q":"chain"}"#),
    );

    // .open(...) — receiver is also call_expression → Chain
    let open_call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "open")
        .expect("missing call to open");
    assert_eq!(
        open_call.metadata.as_deref(),
        Some(r#"{"q":"chain"}"#),
    );
}

#[test]
fn test_rust_callee_self_recv_within_impl() {
    let code = r#"
        struct Db;
        impl Db {
            fn caller(&self) { self.helper(); }
            fn helper(&self) {}
        }
    "#;
    let relations = extract_relations(code, "rust").unwrap();
    let call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "helper")
        .expect("missing call to helper");
    assert_eq!(
        call.metadata.as_deref(),
        Some(r#"{"q":"self","v":"Db"}"#),
        "self.method() inside impl Db emits SelfRecv with type name"
    );
}

#[test]
fn test_rust_callee_self_type_within_impl() {
    let code = r#"
        struct Db;
        impl Db {
            fn make() -> Self { Self::default() }
        }
        impl Default for Db { fn default() -> Self { Db } }
    "#;
    let relations = extract_relations(code, "rust").unwrap();
    let call = relations.iter()
        .find(|r| r.relation == REL_CALLS && r.target_name == "default" && r.source_name.contains("make"))
        .expect("missing call to default from make");
    assert_eq!(
        call.metadata.as_deref(),
        Some(r#"{"q":"stype","v":"Db"}"#),
        "Self::method() inside impl Db emits SelfType with type name"
    );
}

#[test]
fn test_non_rust_callee_metadata_unchanged() {
    let code = "function caller() { foo.bar(); baz(); }";
    let relations = extract_relations(code, "javascript").unwrap();
    for r in relations.iter().filter(|r| r.relation == REL_CALLS) {
        assert_eq!(
            r.metadata, None,
            "non-Rust REL_CALLS relations must keep metadata=None (regression guard for {})",
            r.target_name
        );
    }
}

#[test]
fn test_rust_type_usage_emits_references_edge() {
    let src = r#"
struct AppConfig {
    widget: WidgetConfig,
}
fn make() -> WidgetConfig { WidgetConfig {} }
"#;
    let rels = extract_relations(src, "rust").unwrap();
    let ref_count = rels.iter().filter(|r|
        r.relation == REL_REFERENCES && r.target_name == "WidgetConfig").count();
    // Exactly 2: the field type + the return type. The `WidgetConfig {}`
    // struct-expr name is skipped (already a `calls` edge), and the struct's
    // own `name` (AppConfig) is not WidgetConfig.
    assert_eq!(ref_count, 2,
        "expected exactly 2 references edges to WidgetConfig (field type + return type); got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_rust_type_definition_name_does_not_self_reference() {
    let src = r#"struct Foo { x: u32 }"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Foo"),
        "a struct's own name must not be a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_rust_inherent_impl_header_does_not_emit_references_edge() {
    // `impl Widget` — the impl-header type name is already covered by the impl
    // machinery; a references edge would defeat dead-code detection (Widget
    // would always look used).
    let src = r#"impl Widget { fn f() {} }"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Widget"),
        "an inherent impl header type must not be a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_rust_trait_impl_header_does_not_emit_references_edge() {
    // `impl MyTrait for Widget` — neither the trait name nor the type name
    // should yield a references edge (both are IMPLEMENTS-edge territory).
    let src = r#"impl MyTrait for Widget {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES
            && (r.target_name == "Widget" || r.target_name == "MyTrait")),
        "a trait impl header (type or trait) must not be a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_rust_return_type_still_emits_references_edge_alongside_impl() {
    // Guard against the impl-header skip over-reaching: a real type usage in a
    // return position must still emit a references edge even when the same type
    // also appears in an impl header.
    let src = r#"
impl Widget { fn f() {} }
fn make() -> Widget { todo!() }
"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Widget"),
        "a return-type usage of Widget must still emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_rust_inferred_type_placeholder_does_not_emit_references_edge() {
    let src = r#"fn f() { let v: Vec<_> = items.collect::<Vec<_>>(); }"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "_"),
        "inferred-type placeholder `_` must not emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_ts_type_usage_emits_references_edge() {
    // A type used in type position (interface field type, return type, var
    // annotation) must emit a `references` edge. The interface's OWN name
    // `Widget` must NOT self-reference.
    let src = r#"interface Widget { size: number } function make(): Widget { return null as any; } const w: Widget = make();"#;
    let rels = extract_relations(src, "typescript").unwrap();
    let has_ref = rels.iter().any(|r|
        r.relation == REL_REFERENCES && r.target_name == "Widget");
    assert!(has_ref,
        "expected a references edge to Widget (return type / annotation); got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
    // The interface's own name must not self-reference.
    let widget_self_ref = rels.iter().any(|r|
        r.relation == REL_REFERENCES && r.target_name == "Widget" && r.source_name == "Widget");
    assert!(!widget_self_ref,
        "the interface's own name `Widget` must not self-reference; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_ts_generic_arg_emits_references_edge() {
    // Type used as a generic argument (`Array<Foo>`) is a real type-position
    // usage and must emit a references edge.
    let src = r#"function g(): Array<Foo> { return []; }"#;
    let rels = extract_relations(src, "typescript").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Foo"),
        "a generic-arg type usage of Foo must emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_ts_extends_clause_does_not_emit_references_edge() {
    // `class Foo extends Bar {}` — Bar is an inherits/extends edge, NOT a
    // references edge (avoid double-emit).
    let src = r#"class Foo extends Bar {}"#;
    let rels = extract_relations(src, "typescript").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Bar"),
        "an extends-clause superclass must not be a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_ts_implements_clause_does_not_emit_references_edge() {
    // `class Foo implements Iface {}` — Iface is an implements edge, NOT a
    // references edge.
    let src = r#"class Foo implements Iface {}"#;
    let rels = extract_relations(src, "typescript").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Iface"),
        "an implements-clause interface must not be a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_ts_predefined_types_do_not_emit_references_edge() {
    // Primitives (string/number/boolean) parse as `predefined_type`, not
    // `type_identifier`, so they are naturally excluded.
    let src = r#"function f(x: number, y: string): boolean { return true; }"#;
    let rels = extract_relations(src, "typescript").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES
            && matches!(r.target_name.as_str(), "number" | "string" | "boolean")),
        "predefined primitive types must not emit references edges; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_ts_type_alias_rhs_emits_but_name_does_not() {
    // `type Alias = Widget;` — the RHS `Widget` is a usage (emit), the alias
    // name `Alias` is a declaration (skip).
    let src = r#"type Alias = Widget;"#;
    let rels = extract_relations(src, "typescript").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Widget"),
        "type-alias RHS Widget must emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Alias"),
        "type-alias own name Alias must not self-reference; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

// --- Python type-annotation REFERENCES edges ---
// tree-sitter-python wraps annotation types in a `type` node; the type NAME is a
// plain `identifier` (same kind as value identifiers), so the gate is the
// annotation context (`type`-node ancestor), not the node kind. Base classes
// live in `argument_list [field=superclasses]` (not a `type` node) → naturally
// excluded; value identifiers (`u`, `account`, `compute`) never sit under a
// `type` node.

#[test]
fn test_python_param_and_return_annotation_emit_references() {
    // `def make(u: User) -> Account:` — param type `User` + return type `Account`
    // must emit references edges; the value identifier `u` and the attribute read
    // `account` (`u.account`) must NOT.
    let src = "def make(u: User) -> Account:\n    return u.account\n";
    let rels = extract_relations(src, "python").unwrap();
    let refs: Vec<&str> = rels.iter()
        .filter(|r| r.relation == REL_REFERENCES)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(refs.contains(&"User"),
        "param annotation `User` must emit a references edge; got: {:?}", refs);
    assert!(refs.contains(&"Account"),
        "return annotation `Account` must emit a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"u"),
        "value identifier `u` must NOT be a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"account"),
        "attribute read `account` must NOT be a references edge; got: {:?}", refs);
}

#[test]
fn test_python_annotated_class_attr_emits_references() {
    // `class Service:\n    cache: Cache` — annotated class attribute → reference
    // to `Cache`.
    let src = "class Service:\n    cache: Cache\n";
    let rels = extract_relations(src, "python").unwrap();
    let refs: Vec<&str> = rels.iter()
        .filter(|r| r.relation == REL_REFERENCES)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(refs.contains(&"Cache"),
        "annotated class attr `Cache` must emit a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"cache"),
        "the annotated name `cache` (LHS) must NOT be a references edge; got: {:?}", refs);
}

#[test]
fn test_python_base_class_does_not_emit_references() {
    // `class Foo(Base):` — Base is an inherits edge (extracted by inheritance),
    // NOT a references edge (avoid double-emit).
    let src = "class Foo(Base):\n    pass\n";
    let rels = extract_relations(src, "python").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Base"),
        "base class `Base` must NOT be a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_python_builtin_annotation_does_not_emit_references() {
    // `x: int = 3` — `int` is a builtin, must NOT emit a references edge (noise).
    let src = "x: int = 3\n";
    let rels = extract_relations(src, "python").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "int"),
        "builtin `int` must NOT be a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_python_generic_arg_annotation_emits_references_skips_typing_generic() {
    // `def g(items: List[User]) -> Dict[str, User]:` — `User` (generic arg) is a
    // project type → reference; the `typing` generics `List`/`Dict` and builtin
    // `str` are stdlib → skipped.
    let src = "def g(items: List[User]) -> Dict[str, User]:\n    return {}\n";
    let rels = extract_relations(src, "python").unwrap();
    let refs: Vec<&str> = rels.iter()
        .filter(|r| r.relation == REL_REFERENCES)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(refs.contains(&"User"),
        "generic arg `User` must emit a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"List") && !refs.contains(&"Dict"),
        "typing generics `List`/`Dict` must be skipped; got: {:?}", refs);
    assert!(!refs.contains(&"str"),
        "builtin `str` must be skipped; got: {:?}", refs);
}

#[test]
fn test_python_dotted_annotation_emits_tail_only_and_no_value_attr_noise() {
    // `meta: mod.Meta = None` (dotted annotation) → reference to the tail `Meta`
    // only, NOT the module path head `mod`. And a value attribute read
    // `obj.method` in non-annotation position must NOT emit any reference.
    let src = "def f(self):\n    meta: mod.Meta = None\n    v = obj.attr\n    return v\n";
    let rels = extract_relations(src, "python").unwrap();
    let refs: Vec<&str> = rels.iter()
        .filter(|r| r.relation == REL_REFERENCES)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(refs.contains(&"Meta"),
        "dotted annotation tail `Meta` must emit a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"mod"),
        "dotted annotation head `mod` (module path) must NOT be a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"obj") && !refs.contains(&"attr"),
        "value attribute read `obj.attr` must NOT emit any references edge; got: {:?}", refs);
}

// --- Go type-position REFERENCES edges ---
// tree-sitter-go represents a type name in type position as a `type_identifier`
// (like Rust/TS, a distinct kind from value identifiers). UNLIKE TS, Go builtins
// (`int`, `string`, ...) are ALSO `type_identifier`, so a builtin skip-set
// (GO_TYPE_REFERENCE_NOISE) is required. Value selectors (`pkg.Func()`,
// `obj.field`) use `field_identifier`/`identifier`, never `type_identifier`, so
// they are naturally excluded. The qualified-type head (`pkg` in `pkg.Type`) is a
// `package_identifier` (naturally excluded); only the tail `Type` is a
// `type_identifier`.

#[test]
fn test_go_param_and_return_type_emit_references() {
    // `func make(u User) Account { return Account{} }` — param type `User` +
    // return type `Account` must emit references; the struct's OWN definition
    // name `Account` (from `type Account struct {}`) must NOT self-reference, and
    // builtins must not emit.
    let src = "type Account struct {}\nfunc make(u User) Account { return Account{} }\n";
    let rels = extract_relations(src, "go").unwrap();
    let refs: Vec<&str> = rels.iter()
        .filter(|r| r.relation == REL_REFERENCES)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(refs.contains(&"User"),
        "param type `User` must emit a references edge; got: {:?}", refs);
    assert!(refs.contains(&"Account"),
        "return type `Account` must emit a references edge; got: {:?}", refs);
    // The struct's own definition name must not self-reference.
    let account_self_ref = rels.iter().any(|r|
        r.relation == REL_REFERENCES && r.target_name == "Account" && r.source_name == "Account");
    assert!(!account_self_ref,
        "the struct's own name `Account` must not self-reference; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_go_struct_field_type_emits_references() {
    // `type S struct { conn Conn }` — the field type `Conn` is a usage → reference.
    // The field NAME `conn` (`field_identifier`) and the struct name `S` must not.
    let src = "type S struct {\n\tconn Conn\n}\n";
    let rels = extract_relations(src, "go").unwrap();
    let refs: Vec<&str> = rels.iter()
        .filter(|r| r.relation == REL_REFERENCES)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(refs.contains(&"Conn"),
        "struct field type `Conn` must emit a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"conn"),
        "the field name `conn` must NOT emit a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"S"),
        "the struct's own name `S` must NOT self-reference; got: {:?}", refs);
}

#[test]
fn test_go_value_selector_does_not_emit_references() {
    // `pkg.DoThing()` — `DoThing` is a `field_identifier` on a value selector, NOT
    // a `type_identifier`, so it must not emit a references edge.
    let src = "func run() {\n\tpkg.DoThing()\n}\n";
    let rels = extract_relations(src, "go").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "DoThing"),
        "a value selector call `pkg.DoThing()` must NOT emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "pkg"),
        "the selector operand `pkg` must NOT emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_go_builtin_type_does_not_emit_references() {
    // `var x int` — `int` is a `type_identifier` in tree-sitter-go but a builtin,
    // so it must be filtered out by GO_TYPE_REFERENCE_NOISE.
    let src = "var x int\n";
    let rels = extract_relations(src, "go").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "int"),
        "builtin `int` must NOT emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_java_param_and_return_and_new_type_emit_references() {
    // `class Svc { Account make(User u) { return new Account(); } }` — param type
    // `User`, return type `Account`, and `new Account()` type all emit references.
    // The class's OWN definition names `Account`/`Svc` (the `name` field of a
    // class_declaration, which is an `identifier`, NOT a `type_identifier`) must
    // NOT self-reference. Primitives must not emit.
    let src = "class Account {}\nclass Svc { Account make(User u) { return new Account(); } }\n";
    let rels = extract_relations(src, "java").unwrap();
    let refs: Vec<&str> = rels.iter()
        .filter(|r| r.relation == REL_REFERENCES)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(refs.contains(&"User"),
        "param type `User` must emit a references edge; got: {:?}", refs);
    assert!(refs.contains(&"Account"),
        "return type / `new Account()` type `Account` must emit a references edge; got: {:?}", refs);
    // Class definition names must never self-reference.
    let account_self = rels.iter().any(|r|
        r.relation == REL_REFERENCES && r.target_name == "Account" && r.source_name == "Account");
    assert!(!account_self,
        "the class's own name `Account` must not self-reference; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
    assert!(!refs.contains(&"Svc"),
        "the class's own name `Svc` must NOT emit a references edge; got: {:?}", refs);
}

#[test]
fn test_java_field_type_emits_references() {
    // `class S { Conn conn; }` — field type `Conn` is a usage → reference; the
    // field NAME `conn` (an `identifier`, not a `type_identifier`) and the class
    // name `S` must not.
    let src = "class S { Conn conn; }\n";
    let rels = extract_relations(src, "java").unwrap();
    let refs: Vec<&str> = rels.iter()
        .filter(|r| r.relation == REL_REFERENCES)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(refs.contains(&"Conn"),
        "field type `Conn` must emit a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"conn"),
        "the field name `conn` must NOT emit a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"S"),
        "the class's own name `S` must NOT self-reference; got: {:?}", refs);
}

#[test]
fn test_java_heritage_types_do_not_emit_references() {
    // `class Foo extends Bar implements Baz {}` — `Bar` (superclass clause) and
    // `Baz` (super_interfaces clause) already yield inherits/implements edges, so
    // they must NOT also emit references edges.
    let src = "class Foo extends Bar implements Baz {}\n";
    let rels = extract_relations(src, "java").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Bar"),
        "superclass `Bar` must NOT emit a references edge (heritage); got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "Baz"),
        "interface `Baz` must NOT emit a references edge (heritage); got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_java_jdk_noise_type_does_not_emit_references() {
    // `class S { String name; }` — `String` is a JDK common type (noise); it must
    // be filtered out by JAVA_TYPE_REFERENCE_NOISE.
    let src = "class S { String name; }\n";
    let rels = extract_relations(src, "java").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "String"),
        "JDK type `String` must NOT emit a references edge (noise); got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_java_primitive_type_does_not_emit_references() {
    // `class S { int x; }` — `int` is `integral_type`, a SEPARATE kind from
    // `type_identifier`, so it is naturally excluded (never reaches the extractor).
    let src = "class S { int x; }\n";
    let rels = extract_relations(src, "java").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "int"),
        "primitive `int` must NOT emit a references edge (separate kind); got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_java_generic_arg_and_qualified_tail_emit_only_tail() {
    // Generic arg `List<Foo>` → `Foo` emits. Qualified type `pkg.Sub.Deep field;`
    // → only the chain TAIL `Deep` emits; the package-path segments `pkg`/`Sub`
    // (also `type_identifier`s under nested `scoped_type_identifier`) must NOT.
    let src = "class A { java.util.List<Foo> g() { return null; } pkg.Sub.Deep d; }\n";
    let rels = extract_relations(src, "java").unwrap();
    let refs: Vec<&str> = rels.iter()
        .filter(|r| r.relation == REL_REFERENCES)
        .map(|r| r.target_name.as_str())
        .collect();
    assert!(refs.contains(&"Foo"),
        "generic arg `Foo` in `List<Foo>` must emit a references edge; got: {:?}", refs);
    assert!(refs.contains(&"Deep"),
        "qualified-type tail `Deep` in `pkg.Sub.Deep` must emit a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"pkg"),
        "qualified-type package segment `pkg` must NOT emit a references edge; got: {:?}", refs);
    assert!(!refs.contains(&"Sub"),
        "qualified-type package segment `Sub` must NOT emit a references edge; got: {:?}", refs);
    // `java.util.List` is JDK noise on the tail (List) and path segments java/util.
    assert!(!refs.contains(&"java") && !refs.contains(&"util"),
        "qualified-type package segments `java`/`util` must NOT emit a references edge; got: {:?}", refs);
}

// ─────────────────────────────────────────────────────────────────────────
// Phase 1: bare-identifier function-VALUE references (callbacks / fn pointers)
//
// RED tests (Inc 0). Existing `references` extraction covers type-position and
// path-qualified value usages; the gap is a BARE `identifier` used as a function
// value (passed as a callback / fn pointer). Positive cases (R1–R3, R8–R9) are
// EXPECTED TO FAIL until candidate generation lands. Negative/guard cases
// (R4–R7, R10–R11) lock the precision boundary and may already pass.
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_r1_rust_bare_fn_call_arg_emits_references_edge() {
    let src = r#"fn caller() { register(handler); } fn handler() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES
        && r.target_name == "handler" && r.source_name == "caller"),
        "bare fn passed as a call argument must emit references caller->handler; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r2_rust_bare_fn_hof_arg_emits_references_edge() {
    let src = r#"fn caller() { let _ = xs.iter().map(double); } fn double() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES
        && r.target_name == "double" && r.source_name == "caller"),
        "bare fn passed to a HOF (.map) must emit references caller->double; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r3_rust_address_of_fn_arg_emits_references_edge() {
    let src = r#"fn caller() { signal(&shutdown); } fn shutdown() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES
        && r.target_name == "shutdown" && r.source_name == "caller"),
        "address-of fn passed as arg (&shutdown) must emit references caller->shutdown; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r4_rust_param_passed_as_arg_does_not_emit_references_edge() {
    // `handler` is a PARAMETER of `run`, not the global fn — passing it through
    // must NOT emit a references edge (M2 param exclusion).
    let src = r#"fn run<F>(handler: F) { spawn(handler); } fn handler() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES
        && r.target_name == "handler" && r.source_name == "run"),
        "a parameter passed through must NOT emit a references edge (M2); got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r5_rust_call_in_arg_position_does_not_emit_references_edge() {
    let src = r#"fn caller() { foo(bar()); } fn bar() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "bar"),
        "a called fn in arg position (bar()) is a calls edge, not references; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r6_rust_field_access_arg_does_not_emit_references_edge() {
    let src = r#"fn caller() { foo(x.field); }"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES
        && (r.target_name == "field" || r.target_name == "x")),
        "member access in arg position must not emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r7_rust_call_callee_does_not_also_emit_value_reference() {
    let src = r#"fn caller() { register(handler); } fn handler() {} fn register<F>(_f: F) {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_CALLS && r.target_name == "register"),
        "the callee must still be a calls edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "register"),
        "the callee must NOT also be a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r8_js_bare_fn_call_arg_emits_references_edge() {
    let src = r#"function caller() { arr.map(myFunc); } function myFunc() {}"#;
    let rels = extract_relations(src, "javascript").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES
        && r.target_name == "myFunc" && r.source_name == "caller"),
        "bare fn passed to .map must emit references caller->myFunc; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r9_js_callback_arg_emits_references_edge() {
    let src = r#"function caller() { on('click', handler); } function handler() {}"#;
    let rels = extract_relations(src, "javascript").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES
        && r.target_name == "handler" && r.source_name == "caller"),
        "bare fn passed as a callback arg must emit references caller->handler; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r10_ts_param_passed_as_arg_does_not_emit_references_edge() {
    let src = r#"function run(cb: Fn) { q(cb); }"#;
    let rels = extract_relations(src, "typescript").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES
        && r.target_name == "cb" && r.source_name == "run"),
        "a TS parameter passed through must NOT emit a references edge (M2); got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r11_js_call_in_arg_position_does_not_emit_references_edge() {
    let src = r#"function caller() { foo(bar()); } function bar() {}"#;
    let rels = extract_relations(src, "javascript").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "bar"),
        "a called fn in arg position (bar()) is a calls edge, not references; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r16_rust_local_let_binding_does_not_emit_references_edge() {
    // `db` is a LOCAL `let` binding (holds a value), not the global fn `db` — passing
    // it by `&` or as an arg must NOT emit a reference to the same-named fn (M2.5).
    // This is the dominant Phase-1 false positive: idiomatic `let db = open();
    // run(&db)` where an accessor fn/method `db` also exists.
    let src = r#"fn caller() { let db = open(); run(&db); use_it(db); } fn db() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "db"),
        "a local `let` binding passed as arg must NOT emit a references edge (M2.5); got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r17_js_local_const_binding_does_not_emit_references_edge() {
    let src = r#"function caller() { const cb = make(); run(cb); } function cb() {}"#;
    let rels = extract_relations(src, "javascript").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "cb"),
        "a local const binding passed as arg must NOT emit a references edge (M2.5); got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r18_rust_genuine_fn_pointer_still_emits_after_m2_5() {
    // M2.5 must NOT over-suppress: a module-level fn passed as a callback (no local
    // binding shadows it) still emits. Guards against the local-binding exclusion
    // accidentally killing real callbacks.
    let src = r#"fn caller() { query_map(params, map_row); } fn map_row() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES
        && r.target_name == "map_row" && r.source_name == "caller"),
        "a genuine fn-pointer callback (no local shadow) must still emit references; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_n1_rust_macro_path_does_not_emit_references_edge() {
    // `tracing::error!(...)` is a macro_invocation whose `macro` field is the scoped
    // path `tracing::error`. The path-reference extractor must NOT treat the macro
    // name tail (`error`) as a value reference — it collides with same-named fns.
    let src = r#"fn f() { tracing::error!("boom {}", x); } fn error() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "error"),
        "a macro path tail must NOT emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_n2_rust_type_associated_path_does_not_emit_references_edge() {
    // `String::as_str` passed as a fn pointer is a Type::method associated path. We
    // cannot resolve the associated item (std method, not in the index), and binding
    // the bare tail `as_str` to an unrelated local fn is a false positive. Suppress
    // PascalCase-head (type-associated) value paths.
    let src = r#"fn f() { let _ = xs.iter().map(String::as_str); } fn as_str() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "as_str"),
        "a Type::method associated path must NOT emit a references edge to the bare tail; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_n3_rust_module_path_value_still_emits_after_noise_fix() {
    // Guard: the noise fix (macro + type-associated suppression) must NOT touch
    // legitimate lowercase module-path value references (`crate::domain::SHARED`).
    let src = r#"fn build() { let w = crate::domain::SHARED; }"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "SHARED"),
        "a lowercase module-path value reference must still emit; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r19_rust_if_let_binding_does_not_emit_references_edge() {
    // `node` bound by `if let Some(node) = ...` is a local, not the global fn `node`.
    // if-let/while-let bindings are `let_condition` patterns, NOT `let_declaration`.
    let src = r#"fn caller() { if let Some(node) = g() { use_it(node); } } fn node() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "node"),
        "an if-let pattern binding must NOT emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r20_rust_for_loop_binding_does_not_emit_references_edge() {
    let src = r#"fn caller() { for item in xs { take(item); } } fn item() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES && r.target_name == "item"),
        "a for-loop pattern binding must NOT emit a references edge; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}

#[test]
fn test_r21_rust_match_arm_binding_does_not_emit_references_edge() {
    let src = r#"fn caller() { match g() { Ok(val) => keep(val), Err(error) => log(error) } } fn val() {} fn error() {}"#;
    let rels = extract_relations(src, "rust").unwrap();
    assert!(!rels.iter().any(|r| r.relation == REL_REFERENCES
        && (r.target_name == "val" || r.target_name == "error")),
        "match-arm pattern bindings must NOT emit references edges; got: {:?}",
        rels.iter().map(|r| (r.relation.as_str(), r.source_name.as_str(), r.target_name.as_str())).collect::<Vec<_>>());
}
