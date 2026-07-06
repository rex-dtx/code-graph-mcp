// Shared domain constants used across modules.
// Relation constants, embedding dimensions, and other cross-cutting concerns
// live here to avoid layer violations (e.g., parser importing from storage).

// -- Data directory --
pub const CODE_GRAPH_DIR: &str = ".code-graph";

/// Opt-in per-project metrics-silence sentinel (a file under `.code-graph/`). When
/// present, the recommendations.jsonl writers — `cli::record_cli_use` (Rust) and
/// `recommendation-log.js` (JS hooks) — skip recording, so a development/dogfood
/// checkout's own CLI/hook runs (manual functionality testing, sims, ad-hoc dev)
/// don't pollute the project's adoption metrics with self-generated events. Does
/// NOT silence MCP usage.jsonl (flush_metrics), so real dev MCP tool metrics still
/// flow. Kept in sync with the literal in claude-plugin/scripts/recommendation-log.js.
pub const NO_METRICS_SENTINEL: &str = ".no-metrics";

// -- MCP tool surface --
/// Tools surfaced in `tools/list` (the live surface MCP clients see). Single
/// source of truth so `stats` can flag legacy/folded tool names recorded in
/// usage.jsonl (e.g. `read_snippet`, `trace_http_chain` from older sessions)
/// instead of commingling them with the live set. The registry's `list_tools()`
/// is asserted to match this exactly (mcp::tools tests), so they cannot drift.
pub const LIVE_MCP_TOOLS: &[&str] = &[
    "semantic_code_search",
    "get_call_graph",
    "get_ast_node",
    "project_map",
    "module_overview",
    "ast_search",
    "find_references",
];

// -- Relation types --
pub const REL_CALLS: &str = "calls";
pub const REL_INHERITS: &str = "inherits";
pub const REL_IMPORTS: &str = "imports";
pub const REL_ROUTES_TO: &str = "routes_to";
pub const REL_IMPLEMENTS: &str = "implements";
pub const REL_EXPORTS: &str = "exports";
/// A symbol *uses* another symbol without calling/importing/inheriting it:
/// a path-qualified const/static reference (`crate::a::FOO`) or a type-position
/// usage (`field: MyStruct`). Edgeless under calls/imports, so tracked separately.
pub const REL_REFERENCES: &str = "references";

// -- Edge confidence tiers (v17+) --
// How an edge's target was resolved. Stored on `edges.confidence` and assigned
// by a single post-resolution classification pass (classify_edge_confidence),
// NOT threaded through the ~10 insert sites. Purely additive metadata: every
// edge still exists; consumers may OPT IN to filtering via --min-confidence.
//
// - extracted: same-file resolution, or a structural relation (imports / inherits
//   / implements / routes_to / exports) resolved by explicit path/parent. Precise.
// - inferred:  a cross-file `calls`/`references` edge resolved by bare name where
//   the target name is UNIQUE among same-language nodes. Likely correct.
// - ambiguous: a cross-file `calls`/`references` edge whose target name has >1
//   same-language definition — the by-name resolution could not pick uniquely.
//   The class behind the known false-positive flood (bare_name_call_qualifier,
//   method_call_edge_drops, value_reference_candidate_gen).
pub const CONF_EXTRACTED: &str = "extracted";
pub const CONF_INFERRED: &str = "inferred";
pub const CONF_AMBIGUOUS: &str = "ambiguous";

/// Rank a confidence tier high→low (extracted=2, inferred=1, ambiguous=0).
/// Unknown strings rank 0 so a corrupt/legacy value is treated as lowest, never
/// silently passing a `--min-confidence extracted` filter.
pub fn confidence_rank(c: &str) -> u8 {
    match c {
        CONF_EXTRACTED => 2,
        CONF_INFERRED => 1,
        _ => 0,
    }
}

/// Parse a user-supplied `--min-confidence` value to its canonical tier string,
/// or None if unrecognized (caller should error loudly, not silently pass-all).
pub fn normalize_confidence(input: &str) -> Option<&'static str> {
    match input.to_lowercase().as_str() {
        "extracted" | "exact" | "high" => Some(CONF_EXTRACTED),
        "inferred" | "medium" | "med" => Some(CONF_INFERRED),
        "ambiguous" | "low" | "all" => Some(CONF_AMBIGUOUS),
        _ => None,
    }
}

// Enum filters shared by the CLI and MCP surfaces. Each canonicalizes case (so
// `--direction BOTH` / MCP `direction:"Both"` are accepted like every other enum
// filter — `--node-type`, `--min-confidence`, `--language` already normalize case;
// `direction`/`relation` were the two that still matched case-sensitively inline)
// and returns None for an unknown value so callers error loudly at entry.

/// Canonicalize a call-graph `--direction` / `direction` (callers|callees|both).
pub fn normalize_call_direction(input: &str) -> Option<&'static str> {
    match input.to_lowercase().as_str() {
        "callers" => Some("callers"),
        "callees" => Some("callees"),
        "both" => Some("both"),
        _ => None,
    }
}

/// Canonicalize a dependency `--direction` / `direction` (outgoing|incoming|both).
pub fn normalize_dep_direction(input: &str) -> Option<&'static str> {
    match input.to_lowercase().as_str() {
        "outgoing" => Some("outgoing"),
        "incoming" => Some("incoming"),
        "both" => Some("both"),
        _ => None,
    }
}

/// Canonicalize a `--relation` / `relation` filter for find_references.
pub fn normalize_relation(input: &str) -> Option<&'static str> {
    match input.to_lowercase().as_str() {
        "calls" => Some("calls"),
        "imports" => Some("imports"),
        "inherits" => Some("inherits"),
        "implements" => Some("implements"),
        "references" => Some("references"),
        "all" => Some("all"),
        _ => None,
    }
}

// -- Index version --
// Bump this when parser/indexer logic changes in a way that produces different
// nodes or edges for the same source files. The server will detect a mismatch
// and automatically clear + rebuild the index.
// This is separate from SCHEMA_VERSION (which tracks table structure changes).
// Vector-only invalidation/refresh (e.g. delete_node_vectors_batch on a
// model=None incremental path) does NOT bump this — only node/edge/FTS output
// changes do; vectors regenerate via the NULL-vector background-embed convention.
pub const INDEX_VERSION: i32 = 41; // v41: TS/JS destructuring exports (`export const { host, port } = getConfig()`, `export const [a, b] = getPair()`) now extract ONE `constant` node per bound identifier instead of a single node named after the literal pattern text (`{ host, port }`). That text is no valid identifier, so `import { host }` dangled to the `<external>` sentinel and the destructured symbols were unusable by name and invisible to show/callgraph/find_references — the v39 const-export import-edge fix silently missed every destructuring form. Common in the wild: Redux `export const { actions, reducer } = slice`, React `export const { Provider } = createContext()`, RTK Query hook exports. Renamed `{ key: local }` binds the local (value) side; defaults (`{ x = 1 }`), rest (`{ ...r }` / `[...r]`), and nested patterns recurse to leaf identifiers (collect_binding_names). Only EXPORTED top-level declarations are affected (the v39 guard is unchanged). Also in v41: TS/JS `export { X, Y } from './mod'` re-exports (barrel / index files) now emit a REL_IMPORTS dependency edge per re-exported name (js_module-stamped like a regular named import, so Phase-2 resolves each to the source file). Previously a re-export produced ZERO edges — a barrel file had no tracked dependency and was invisible to deps/affected/impact/cycles/tour and missed by find-references. `export * from './mod'` wildcards stay module-level-unresolved (a shared limitation with namespace imports `import * as ns`). Existing indexes gain the per-binding nodes + re-export edges only after rebuild, hence the bump. v40: `.h` headers containing C++ constructs are now parsed as C++, not C. `.h` is C-vs-C++ ambiguous by extension so detect_language maps it to C, but the C grammar can't parse `class`/`namespace` — so C++ classes declared in a `.h` header (the MOST common C++ layout: declaration in `.h`, definition in `.cpp`) and their base-class `inherits` edges were silently dropped (the `.cpp` linked fine via is_compatible_lang, but the header's own class SYMBOLS never existed as nodes — overview/callgraph/dead-code/find_references were blind to them). index_files now content-sniffs a `.h` detected as C: if it contains C++ markers (`::`, `public:`/`private:`/`protected:`, `class `, `namespace `, `template<`) it parses as C++ (looks_like_cpp_header), so the classes/structs and their inheritance are captured. Gated on markers so a pure-C header stays C; a false positive is low-harm because the C++ grammar is a near-superset of C. Existing indexes gain the header class nodes only after rebuild, hence the bump. v39: TS/JS top-level `export const/let X = <value>` (config constants, route tables, and widely-imported singletons like `const store = defineStore(...)` / `const logger = createLogger(...)` / `const svc = new Service()`) are now extracted as `constant` symbol nodes, so `import { X } from './mod'` resolves to a real node and forms a REL_IMPORTS edge instead of binding to the `<external>` sentinel — the cross-module dependency was previously invisible to tour/affected/impact/project_map (feedback_const_export_no_import_edge). Only EXPORTED top-level declarations are extracted (a local `const x = 5` in a function body can't be imported cross-file, so extracting it would be pure noise); function-valued consts stay `function` (arrow branch, unchanged). Type mirrors the existing Rust `const_item`/`static_item` extraction — this is TS/JS reaching parity. Scope is TS/JS only: Go package-level `const`/`var`, Python module constants, and Java `static final` fields have different import idioms and remain unextracted (measured: the const-value cross-module import pattern is material in TS/JS — 66 sole-link invisible module deps across 4 sampled projects; other languages await their own evidence). Existing indexes gain the new nodes/edges only after rebuild, hence the bump. v38: method-candidate type filter escapes SQL LIKE metacharacters — a receiver/impl type name containing `_` or `%` (legal identifiers like `my_widget` / `Foo_Bar`) is now matched literally in filter_method_ids (`Data_X.%` no longer also captures `DataYX.run` via the `_` single-char wildcard). Affects the type-restricted resolution paths — Rust `self.method()` (SelfType) and Python constructor-inferred `recv.method()` (rtype, issue #32 cause 2) — which bind only to the genuine type's method during Phase-2 resolution instead of also a sibling type whose name differs only where `_` fell. Existing indexes carry the rare stale false-positive edge until rebuilt, hence the bump. v37: Python receiver-type call resolution (issue #32 cause 2) — a call `recv.method()` whose receiver type is fixed EITHER by a single local `recv = ClassName(...)` constructor assignment OR by an explicit parameter annotation `def f(recv: ClassName)` now carries `{"q":"rtype","v":"ClassName"}` metadata (infer_python_call_receiver_type), so Phase-2 resolution binds it to `ClassName.method` via self_filter_candidates instead of dropping the whole ambiguous by-name fan-out when the method name is shared across classes. Before: `writer.write()` with `write` defined on 3 classes produced NO edge → all 3 reported dead + no callers in callgraph/impact. Conservative: only a provably-single constructor assignment infers a type; a wrong/unknown type fails the candidate filter and drops (never a false cross-type edge). New edges appear only after rebuild. v36: Python decorated `def`/`class` symbols now bind to the enclosing tree-sitter `decorated_definition` wrapper, so `start_line` + `code_content` include the decorator stack instead of starting at `def`/`class` (issue #31 — `@field_validator("lat", mode="before")` and friends were silently dropped, blinding get_ast_node / semantic search to the pydantic contract). That retained decorator text also lets `find_dead_code` exclude framework-registered / attribute-accessed Python methods (pydantic validators, pytest fixtures, `@property`, `@abstractmethod`, `@overload`, NiceGUI handlers — see PYTHON_FRAMEWORK_DECORATORS) that are dispatched dynamically and thus edgeless, eliminating the dominant dead-code false-positive class (issue #32 cause 1). Existing indexes carry the pre-decorator extents (and the stale orphans) until rebuilt, hence the bump. v35: import-corroborated cross-file calls keep visibility — classify_edge_confidence no longer stamps a cross-file `calls`/`references` edge `ambiguous` (which the confidence floor hides) when the caller's file explicitly imports THAT exact target. The import binds the bare name to one node, so the edge is import-resolved (v0.59 bind_calls_to_imported_targets), not a bare-name guess among same-name siblings. Before: any target NAME defined in >=2 same-language files (process/handler/run/init/index…) made a precisely-import-bound call `ambiguous` → callgraph/impact showed NO callee for it by default. Existing indexes carry the stale `ambiguous` labels until rebuilt, hence the bump. v34: Go inheritance generics correctness (code-review fast-follow) — (a) a Go 1.18 interface type-SET constraint (`interface { Signed | Unsigned }`, one `type_elem` with >1 child) no longer emits a bogus `inherits` edge to the first union term; only genuine single-type embedded interfaces do; (b) embedded generic types (`type Sub struct { Base[int] }`, `interface { Container[T] }`) now emit `inherits` on the generic's base name (were silently dropped). v33: C++ base classes now emit `inherits` edges (`class Dog : public Animal` → Dog inherits Animal; multiple/`struct`/qualified `ns::Base`/`template Tmpl<int>` bases all bind on the simple type name; access specifiers public/private/protected skipped; C++ has no interface concept so every base is `inherits`). C has no inheritance concept and C `struct_specifier` never carries a base clause, so nothing changes for C. The C/C++ class/struct/enum dead-code exclusion stays (leaf classes still have no incoming edge). v32: inheritance-extraction parity — (a) Go struct/interface embedding now emits `inherits` edges (Go's idiomatic "is-a": `type Dog struct { Animal }` → Dog inherits Animal; `*Base` and `pkg.Type` bind on the simple type name; embedded interfaces via `type_elem` compose, methods via `method_elem` do not; a normal named field `f Foo` stays has-a) — Go previously produced ZERO inherits edges; (b) Dart mixins (`class C extends Base with M, N`) now emit `inherits` to each mixin (mixin application injects methods), and a `with`-only class no longer produces a malformed `"with M"` target from the text-clean fallback; v31: edge-resolution correctness — (a) structural relations (imports/inherits/implements/exports/routes_to) no longer fall through to the GLOBAL all-language name pool when there is no same-file/same-language target; they bind same-language-only, eliminating cross-language phantom edges (Rust `use anyhow::Result` → a markdown "Result" heading; JS `require('fs')` → a Rust `fs` symbol) that were stamped `extracted` (unfilterable) and polluted deps/project_map/affected/cycles/find_references, and letting unresolved imports/implements reach the `<external>` sentinel instead of being pre-empted; (b) the Phase-2c incremental inbound-edge restore now re-binds a saved cross-file edge only to the same-name node in the ORIGINAL target file (map keyed by (file_id, name)) rather than every same-name node in the batch, so a multi-file incremental no longer over-creates cross-file/cross-language edges a full rebuild wouldn't; v30: Dart fixes — (a) top-level functions (`int helper() {}`) are now extracted as symbols (parsed as a bare function_signature sibling under `program`, never matched before so callgraph/impact/dead-code were blind to them); (b) calls now dispatch on the `selector(argument_part)` node (callee = preceding sibling) instead of only `expression_statement`, so calls in return / assignment / argument / binary-expression positions resolve (were silently dropped — only bare `foo();` statements worked); v29 also: Express routes_to with an IMPORTED named handler (`import {getUser} from './ctrl'; app.get('/x', getUser)`) now resolves the handler cross-file (was matched only against the route file's own nodes → route silently dropped for the most common Express layout; inline + same-file handlers already worked); v29: cross-file call-noise filter is now language-aware — JS/TS `obj.insert()`/`remove()`/`contains()` resolve (not ECMAScript builtins) while genuine builtins (push/pop/get/map/filter...) still drop; PHP `$o->method()` calls are fully exempt (PHP array ops are global functions, not methods, so the Rust-collection list only produced false-positive dead code). Was reporting live JS/TS/PHP methods as dead code + hiding callers; v28: Ruby bare (parens-less) method calls in statement position now produce calls edges via a scope-aware pass that excludes local variables (Ruby's own assigned-vs-call rule), closing a recall gap where `helper` (no parens) was dropped; v27: Python + Ruby top-level (module/class-body) calls now attribute to `<module>` too (same fix as bash v26) so an entry-point function called only at top level isn't reported dead; v26: bash top-level command invocations now attribute to `<module>` (were dropped) so an entry-point function called only at script top level (`run_app "$@"`) is no longer reported dead; external commands still drop at Phase-2 resolution; v25: Flask @app.route(..., methods=['GET']) now derives the HTTP verb from the methods= kwarg (was always "ANY", breaking method-scoped trace); v24: PHP file-include imports (require/require_once/include/include_once → REL_IMPORTS to the bare file stem)

// -- Schema-mismatch marker --
// Stable machine token appended to the "this DB's schema is newer than this
// binary supports" bail (storage::db). The plugin statusline matches THIS token —
// not translatable/reword-able prose — to tell the post-update window (an old
// cached binary running against a newer index, while the new binary downloads:
// "↻ updating") apart from a genuine "offline" failure. DO NOT change this string
// without updating claude-plugin/scripts/statusline.js in lockstep.
pub const SCHEMA_TOO_NEW_MARKER: &str = "code-graph:schema-too-new";

// -- Embedding --
pub const EMBEDDING_DIM: usize = 384;

// -- Semantic-search rerank tuning (search.rs) --
// Multipliers/thresholds applied AFTER RRF fusion to rerank candidates. Named
// here (audit §4/§8) so they are tunable + ablatable in one place rather than
// scattered as magic numbers. Values are the historical ones — extracting them
// is metric-neutral; change them only with a precision@5/MRR ablation.
/// RRF constant k: sharper rank sensitivity than the textbook 60 (top hits matter more).
pub const RERANK_RRF_K: u32 = 30;
/// Acronym-heavy query detection: ≤N short uppercase tokens are letter-exact identifiers.
pub const ACRONYM_MAX_TOKENS: usize = 3;
pub const ACRONYM_MAX_TOKEN_CHARS: usize = 5;
/// Fusion weights: acronym-heavy shifts toward FTS (token-exact); default favors vector.
pub const ACRONYM_FTS_WEIGHT: f64 = 2.0;
pub const ACRONYM_VEC_WEIGHT: f64 = 0.8;
pub const DEFAULT_FTS_WEIGHT: f64 = 1.0;
pub const DEFAULT_VEC_WEIGHT: f64 = 1.2;
/// match_confidence penalties. Vector-only (no FTS hit) = largely similarity noise.
pub const CONF_VEC_ONLY_PENALTY: f64 = 0.35;
/// OR-fallback fired (AND mode found no co-occurrence) → weaker match.
pub const CONF_OR_FALLBACK_PENALTY: f64 = 0.6;
/// Only judge FTS sparsity/intersection when FTS returned enough breadth (precision
/// queries with ≤4 hits legitimately have a low ratio and must not be penalized).
pub const CONF_SPARSITY_MIN_FTS: usize = 5;
/// FTS-sparsity tiers: (ratio threshold, confidence multiplier), most-sparse first.
pub const CONF_SPARSITY_R1: f64 = 0.1;
pub const CONF_SPARSITY_P1: f64 = 0.5;
pub const CONF_SPARSITY_R2: f64 = 0.25;
pub const CONF_SPARSITY_P2: f64 = 0.65;
pub const CONF_SPARSITY_R3: f64 = 0.5;
pub const CONF_SPARSITY_P3: f64 = 0.8;
/// Source-intersection: low FTS∩vec overlap in the top-k → less confidence.
pub const CONF_INTERSECTION_MIN_RATIO: f64 = 0.2;
pub const CONF_INTERSECTION_PENALTY: f64 = 0.75;
// (match_confidence is surfaced as a raw query-shape signal; the low_confidence
// warning fires on a text-anchor mechanic, not a match_confidence threshold —
// see src/mcp/server/tools/search.rs VECTOR_ONLY_WARNING. A prior
// CONF_WARNING_THRESHOLD=0.5 was removed: the calibration bench showed no signal
// separates good NL from nonsense, so a threshold warning was ~all false alarms.)
/// Name-match boost: +per-match, capped, for symbols whose name contains query terms.
pub const NAME_BOOST_PER_MATCH: f64 = 0.3;
pub const NAME_BOOST_CAP: f64 = 2.0;
/// Exact symbol-name match dominance. When the query is verbatim a node's
/// name/qualified_name, its definition must rank first: RRF already places it
/// (tier3 exact-symbol recall@10 was 0.984 RRF-only) but the `base × name_boost ×
/// size × doc` rerank buried exact matches under vector noise + size dampening,
/// dropping recall@10 to 0.806. This additive bonus dominates any non-exact
/// `adjusted` (which lies in [0, base×CAP] ⊂ [0,2]); exact matches then order
/// among themselves by `base_score`.
pub const EXACT_NAME_MATCH_BONUS: f64 = 100.0;
/// Size dampening: counter BM25/vector bias toward very large nodes (> threshold lines).
pub const SIZE_DAMPEN_LINES: f64 = 100.0;
pub const SIZE_DAMPEN_COEFF: f64 = 0.4;
/// Doc penalty: demote markdown headings for code-intent queries (unless lang=markdown).
pub const DOC_PENALTY_MARKDOWN: f64 = 0.4;

// -- Retrieval over-fetch (post-KNN filtering compensation) --
// vec0 KNN (`embedding MATCH … LIMIT k`) cannot pre-filter on joined `nodes`
// columns, so every filter — always-on test/module/external skip plus optional
// language/node_type — is applied in Rust AFTER the top-k fetch. A fetch sized to
// top_k lets a selective filter silently starve the result set (return < top_k, or
// nothing, while matches sit just past the cutoff). We over-fetch to compensate;
// when an optional language/node_type filter is active the survivors can be a small
// minority of the nearest neighbours, so the pool is widened further.
/// Base over-fetch multiplier for semantic_code_search with no language/node_type filter.
pub const SEARCH_BASE_OVERFETCH: i64 = 4;
/// Floor so a small top_k still has candidates after the always-on test/module skip.
pub const SEARCH_FETCH_FLOOR: i64 = 20;
/// Wider over-fetch when a selective language/node_type filter is active.
pub const SEARCH_FILTER_OVERFETCH: i64 = 16;
/// Floor for the filtered case.
pub const SEARCH_FILTER_FETCH_FLOOR: i64 = 100;
/// `similar` / find_similar_code over-fetch: self-exclusion + max_distance + test/module
/// skip are all post-fetch, so fetch a multiple of top_k rather than top_k+1.
pub const SIMILAR_OVERFETCH: i64 = 3;

/// Candidate-pool size for semantic_code_search. `filtered` = a language or node_type
/// filter is active (widens the pool so the post-KNN filter cannot starve top_k). The
/// unfiltered value is byte-identical to the historical `(top_k*4).max(20)`, so the
/// retrieval benchmark — which passes no filter — is unchanged by the filtered branch.
pub fn search_fetch_count(top_k: i64, filtered: bool) -> i64 {
    if filtered {
        (top_k * SEARCH_FILTER_OVERFETCH).max(SEARCH_FILTER_FETCH_FLOOR)
    } else {
        (top_k * SEARCH_BASE_OVERFETCH).max(SEARCH_FETCH_FLOOR)
    }
}

/// Candidate-pool size for `similar` / find_similar_code. Over-fetches so the
/// post-fetch filters (self-exclusion, max_distance, test/module skip) do not starve
/// top_k — the old `top_k + 1` fell short on any single drop.
pub fn similar_fetch_count(top_k: i64) -> i64 {
    (top_k * SIMILAR_OVERFETCH).max(top_k + 1)
}

// -- Token estimation --
/// Approximate **bytes** per token for code content (1 token ≈ 3 bytes UTF-8).
///
/// Despite the historical name, all callers feed `s.len()` (UTF-8 byte length
/// in Rust) into this divisor — not Unicode char counts — which is why the
/// estimate stays sensible for CJK content too:
///
/// - ASCII: ~3-4 bytes/token in BPE → `bytes/3` slightly overestimates (safe).
/// - CJK: one char = 3 bytes UTF-8, ~1 token/char in BPE → `bytes/3 ≈ chars ≈ tokens` (accurate).
///
/// Conservative overestimation is the safe error direction: fires compression
/// earlier, never under-counts and overflows the downstream context window.
/// Used for token budget estimation across compression and search.
pub const CHARS_PER_TOKEN: usize = 3;

// -- Parsing limits --
pub const MAX_AST_DEPTH: usize = 64;
pub const MAX_RELATION_DEPTH: usize = 256;

// -- Indexing limits (env-var overridable) --

use std::sync::OnceLock;

/// Maximum file size to index. Override: CODE_GRAPH_MAX_FILE_SIZE (bytes).
/// Default: 1 MB.
pub fn max_file_size() -> u64 {
    static VAL: OnceLock<u64> = OnceLock::new();
    *VAL.get_or_init(|| {
        std::env::var("CODE_GRAPH_MAX_FILE_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_048_576)
    })
}

/// Maximum code content length stored per node. Override: CODE_GRAPH_MAX_CODE_LEN (bytes).
/// Default: 4 KB.
pub fn max_code_content_len() -> usize {
    static VAL: OnceLock<usize> = OnceLock::new();
    *VAL.get_or_init(|| {
        std::env::var("CODE_GRAPH_MAX_CODE_LEN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4096)
    })
}

/// Per-file parse timeout in milliseconds. Override: CODE_GRAPH_PARSE_TIMEOUT_MS.
/// Default: 5000 ms.
pub fn parse_timeout_ms() -> u64 {
    static VAL: OnceLock<u64> = OnceLock::new();
    *VAL.get_or_init(|| {
        std::env::var("CODE_GRAPH_PARSE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5000)
    })
}

// -- Risk level assessment --
/// Compute impact risk level from caller/route counts.
///
/// `is_breaking` is `true` for changes that force every call site to change
/// (a removal or a signature change), which pins the result to HIGH regardless
/// of caller count; behaviour-only changes leave it `false` and scale by count.
pub fn compute_risk_level(prod_callers: usize, affected_routes: usize, is_breaking: bool) -> &'static str {
    if prod_callers > 10 || affected_routes >= 3 || is_breaking {
        "HIGH"
    } else if prod_callers > 3 || affected_routes > 0 {
        "MEDIUM"
    } else {
        "LOW"
    }
}

/// True when a node type is a function-like symbol whose usages are fully
/// captured by the `calls` call graph. False for types, constants, traits,
/// modules, etc. — whose real blast-radius includes imports / field access /
/// instantiation / type annotations that impact analysis does not track.
pub fn is_function_node_type(node_type: &str) -> bool {
    matches!(node_type, "function" | "method")
}

/// Warning surfaced by impact analysis when the target is non-function-like
/// and has zero call-graph callers. Prevents the risk level from reading as
/// a misleading `LOW` for constants / types / traits whose real users are
/// imports or type references, not calls.
pub const NON_FUNCTION_IMPACT_WARNING: &str = "Impact analysis tracks function call chains. This symbol is not a function — actual usage (imports, field access, type annotations, instantiation) may be broader than shown. Use `find_references` (MCP) or `code-graph-mcp refs <symbol>` (CLI) to find all references.";

// -- Test symbol detection --
/// Check if a symbol is a test/harness function/file based on naming conventions.
/// Used by both MCP server and CLI to separate test vs production callers.
///
/// `benches/` is classified as test/harness because criterion benchmarks are
/// macro-driven entry points (`criterion_group!`) — counting them as production
/// callers inflates impact-analysis risk and corrupts caller_count rankings.
pub fn is_test_symbol(name: &str, file_path: &str) -> bool {
    name.starts_with("test_")
        || name.ends_with("Test") || name.ends_with("Tests")
        || is_test_path(file_path)
}

/// Authoritative test predicate for a graph node: trust the AST-level `nodes.is_test`
/// flag first, falling back to the [`is_test_symbol`] name/path heuristic for rows
/// that don't carry it. The flag (set by the parser for `#[cfg(test)] mod tests` /
/// `#[test]` / `@Test` / ...) catches inline unit tests with descriptive snake_case
/// names in a `src/` file that the heuristic MISSES; the heuristic still catches
/// integration tests in `tests/`, `test_`-prefixed names, and any node whose
/// `is_test` projection predates a surface.
///
/// Single source so every caller-/callee-partitioning surface (callgraph, trace,
/// `show` references) classifies tests identically — mirrors `classify_impact`'s
/// rule (`graph::impact`) and prevents the is_test "sibling-hole" drift the v0.79.1
/// audit traced across impact/callgraph/trace/show.
pub fn is_test_node(is_test_flag: bool, name: &str, file_path: &str) -> bool {
    is_test_flag || is_test_symbol(name, file_path)
}

/// True for a search/similarity candidate that every result surface skips as
/// non-real output: a file-level `<module>` placeholder, an `<external>` stub,
/// or a test symbol. Single source for the triad otherwise reimplemented in
/// `cmd_search`/`tool_semantic_search` and `cmd_similar`/`tool_find_similar_code`
/// across the CLI and MCP surfaces (a recurring drift site — the CLI search/
/// similar paths historically omitted the `<external>` leg the MCP path applied).
pub fn is_skippable_result(node_type: &str, node_name: &str, file_path: &str) -> bool {
    (node_type == "module" && node_name == "<module>")
        || file_path == "<external>"
        || is_test_symbol(node_name, file_path)
}

/// Classify a dead-code candidate as exported-but-unused (`true`) vs a true
/// orphan (`false`). Exported = visible outside its module (public/`pub`, or an
/// uppercase Go identifier, or an explicit export edge), so even without tracked
/// callers removal is a wider decision than for an orphan.
///
/// Single source for the orphan/exported split otherwise reimplemented at three
/// sites (`cmd_dead_code` text path, `cmd_dead_code` JSON path, and
/// `tool_find_dead_code`). The CLI JSON path had drifted — it omitted the Go
/// export leg the text + MCP paths apply, so exported Go symbols were misfiled as
/// orphans in `--json` output only.
pub fn is_dead_code_exported(
    has_export_edge: bool,
    code_content: &str,
    file_path: &str,
    name: &str,
) -> bool {
    has_export_edge
        || code_content.starts_with("pub ")
        || code_content.starts_with("pub(")
        || (file_path.ends_with(".go")
            && name.chars().next().is_some_and(|c| c.is_uppercase()))
}

/// File-level test classifier (path heuristics only) shared by `is_test_symbol` and
/// the `affected` command. NOT the only test-path matcher: the SQL counterparts
/// (`PROD_SOURCE_FILTER_AND` / `TEST_SOURCE_FILTER_OR` below) and the local closure in
/// `indexer::pipeline::resolve::refine_ambiguous_targets` use their own, intentionally
/// divergent patterns. See the "Five sites must agree" note below and
/// feedback_test_classifier_dual_sources.md before changing any one of them.
pub fn is_test_path(file_path: &str) -> bool {
    file_path.starts_with("tests/") || file_path.starts_with("test/")
        || file_path.starts_with("benches/") || file_path.starts_with("bench/")
        || file_path.contains("__tests__/")
        || file_path.ends_with("/tests.rs")
        || file_path.ends_with("_test.go") || file_path.ends_with("_test.rs")
        || file_path.ends_with(".test.ts") || file_path.ends_with(".test.js")
        || file_path.ends_with(".test.tsx") || file_path.ends_with(".test.jsx")
        || file_path.ends_with(".spec.ts") || file_path.ends_with(".spec.js")
        || file_path.ends_with(".spec.tsx") || file_path.ends_with(".spec.jsx")
}

/// SQL predicate mirroring [`is_test_node`] for a node aliased `node_alias` joined to
/// its file aliased `file_alias`. Returns a parenthesized boolean (`(… OR …)`) meant
/// for `NOT (…)` in a WHERE clause, so a node-level SQL surface (dead-code,
/// surprising) classifies tests identically to the Rust query-time [`is_test_node`]
/// path: the stored `is_test` flag OR the [`is_test_symbol`] name/path heuristic.
///
/// Why this exists separately from [`TEST_SOURCE_FILTER_OR`]: that one is the
/// edge-oriented (`src`/`sf` alias) variant and is intentionally NARROWER — it omits
/// the `*Test`/`*Tests` name legs and several path suffixes. Surfaces that classify a
/// *node* (not an edge source) and want full `is_test_symbol` parity — e.g. so an
/// integration test `def test_foo()` in `tests/` (whose AST `is_test` flag is 0
/// because the parser only sets it for `#[cfg(test)]`/`@Test`/... markers) is not
/// reported as dead code — must use THIS helper.
///
/// Uses `GLOB` (case-sensitive, `_` literal), not `LIKE` (case-insensitive, `_`
/// wildcard), so it matches Rust's `starts_with`/`ends_with`/`contains` EXACTLY:
/// `test_foo` matches but `Test_foo` does not, and `myTest` matches but `mytest`
/// does not — a `LIKE`-based port would wrongly flag all four. Kept in lockstep with
/// `is_test_symbol`/`is_test_path` by the `test_is_test_node_sql_matches_rust` parity
/// test (any new leg added to either must be added here and asserted there).
pub fn is_test_node_sql(node_alias: &str, file_alias: &str) -> String {
    let n = node_alias;
    let f = file_alias;
    format!(
        "({n}.is_test = 1 \
         OR {n}.name GLOB 'test_*' \
         OR {n}.name GLOB '*Test' \
         OR {n}.name GLOB '*Tests' \
         OR {f}.path GLOB 'tests/*' \
         OR {f}.path GLOB 'test/*' \
         OR {f}.path GLOB 'benches/*' \
         OR {f}.path GLOB 'bench/*' \
         OR {f}.path GLOB '*__tests__/*' \
         OR {f}.path GLOB '*/tests.rs' \
         OR {f}.path GLOB '*_test.go' \
         OR {f}.path GLOB '*_test.rs' \
         OR {f}.path GLOB '*.test.ts' \
         OR {f}.path GLOB '*.test.js' \
         OR {f}.path GLOB '*.test.tsx' \
         OR {f}.path GLOB '*.test.jsx' \
         OR {f}.path GLOB '*.spec.ts' \
         OR {f}.path GLOB '*.spec.js' \
         OR {f}.path GLOB '*.spec.tsx' \
         OR {f}.path GLOB '*.spec.jsx')"
    )
}

// -- SQL counterparts of is_test_symbol --
//
// Reused by every SQL query that counts/orders by caller_count to keep the
// classification aligned with `is_test_symbol`. Five sites must agree —
// see feedback_test_classifier_dual_sources.md for the full inventory.
//
// Convention: callers MUST alias the source node as `src` and the source file
// as `sf` and provide their own `JOIN` for the edges table. The helpers below
// emit the JOIN on `src`/`sf` and the WHERE/CASE clause body separately.

/// JOINs that attach the source node and source file to an `edges` row.
/// `edges_alias` is the alias used in the outer FROM/JOIN for the edges table.
/// Pair with [`PROD_SOURCE_FILTER_AND`] in the WHERE clause.
pub fn prod_source_join_sql(edges_alias: &str) -> String {
    format!(
        "JOIN nodes src ON src.id = {e}.source_id \
         JOIN files sf ON sf.id = src.file_id",
        e = edges_alias,
    )
}

/// AND-joined conditions that exclude test/bench source rows.
/// Combines the AST-level `src.is_test=0` flag with name and path heuristics —
/// kept in sync with `is_test_symbol`. Caller is expected to splice these
/// inside a WHERE clause already started with another condition (no leading AND
/// is added by callers — they prepend ` AND ` themselves) or inside a CASE WHEN.
pub const PROD_SOURCE_FILTER_AND: &str =
    "src.is_test = 0 \
     AND src.name NOT LIKE 'test\\_%' ESCAPE '\\' \
     AND sf.path NOT LIKE 'tests/%' \
     AND sf.path NOT LIKE 'benches/%' \
     AND sf.path NOT LIKE '%_test.%' \
     AND sf.path NOT LIKE '%/tests.rs'";

/// OR-joined inverse of [`PROD_SOURCE_FILTER_AND`] — matches test/bench sources.
/// Used by SUM/CASE constructs that count test callers separately (e.g.
/// project_map's hot_functions test_cnt CASE).
pub const TEST_SOURCE_FILTER_OR: &str =
    "src.is_test = 1 \
     OR src.name LIKE 'test\\_%' ESCAPE '\\' \
     OR sf.path LIKE 'tests/%' \
     OR sf.path LIKE 'benches/%' \
     OR sf.path LIKE '%_test.%' \
     OR sf.path LIKE '%/tests.rs'";

// -- Dead-code ignore defaults --
/// Path-prefix defaults for `find_dead_code` ignore_paths.
///
/// Macro/harness-invoked entry points are not in the static AST call graph
/// because the references go through tokens the parser can't (or doesn't yet)
/// resolve:
/// - `claude-plugin/`: hook handlers / lifecycle scripts / auto-update hooks
///   called from `settings.json` hook definitions or shell, not JS imports.
/// - `benches/`: Criterion bench fns named inside `criterion_group!(...)`
///   tokens; macro arguments are not parsed as references.
///
/// Callers wanting the unfiltered list pass `ignore_paths: []` (CLI:
/// `--no-ignore`).
pub fn default_dead_code_ignores() -> Vec<String> {
    vec!["claude-plugin/".to_string(), "benches/".to_string()]
}

// -- Node type normalization --
/// Normalize shorthand type filter into canonical AST node types.
/// Shared by CLI and MCP tool implementations.
pub fn normalize_type_filter(input: &str) -> Vec<&'static str> {
    match input.to_lowercase().as_str() {
        "fn" | "func" | "function" | "method" => vec!["function", "method"],
        "class" => vec!["class"],
        "struct" => vec!["struct"],
        "enum" => vec!["enum"],
        "interface" | "iface" | "trait" => vec!["interface", "trait"],
        "type" | "type_alias" => vec!["type_alias"],
        "const" | "constant" => vec!["constant"],
        "var" | "variable" => vec!["variable"],
        "module" => vec!["module"],
        _ => vec![],
    }
}

// -- Edge resolution noise filter --
// Common standard-library method/trait names that produce false-positive call edges
// when resolved cross-file by name alone (without type context).
// These are skipped for cross-file `calls` edge creation.
pub const CROSS_FILE_CALL_NOISE: &[&str] = &[
    "new", "default", "from", "into", "as_str", "to_string", "clone",
    "fmt", "display", "drop", "try_from", "try_into",
    "as_ref", "as_mut", "borrow", "borrow_mut", "deref", "deref_mut",
    "eq", "ne", "cmp", "partial_cmp", "hash",
    "serialize", "deserialize",
    "next", "iter", "into_iter",
    "build", "builder",
    "len", "is_empty",
    "unwrap", "unwrap_or", "unwrap_or_else", "unwrap_or_default",
    "expect", "ok", "err", "map", "map_err", "and_then",
    "or_else", "filter", "flatten",
    "push", "pop", "insert", "remove", "contains", "get",
    "to_owned", "to_vec", "collect", "join",
    "flush", "close", "read", "write",
];

// Names that live in CROSS_FILE_CALL_NOISE because they are Rust/collection
// stdlib methods (`Vec::insert`, `HashMap::remove`, `slice::contains`) but are
// NOT core-ECMAScript builtin instance methods — Arrays use `splice`, Maps use
// `has`, and there is no `Array/Object/String.insert`. In a JS/TS codebase these
// are ordinary user-defined methods (`db.insert(x)`, `cache.remove(k)`,
// `set.contains(v)`), so applying the Rust-flavored drop to them silently lost
// legitimate `calls` edges — reporting live methods as dead code and hiding
// their callers from impact/callers. Exempted for the JS family ONLY; genuine
// ECMAScript builtins still in the noise set (`push`/`pop`/`get`/`map`/`filter`/
// `join`/`read`/`write`...) stay dropped because the receiver type is unknown.
pub const JS_CALL_NOISE_EXEMPT: &[&str] = &["insert", "remove", "contains"];

/// Whether a cross-file `calls` target name should be dropped as stdlib noise
/// for a given source language.
///
/// [`CROSS_FILE_CALL_NOISE`] is a Rust/collection-stdlib list and fits languages
/// whose receivers expose method-style builtins under these exact (lowercase)
/// names — Rust, Python (`list.insert`/`dict.get`), Ruby (`Array#push/#insert`),
/// Java (`List.get`/`StringBuilder.insert`), Kotlin, Swift, C++ (`vector::insert`).
/// Two families diverge:
///   - **PHP**: `$o->method()` calls have NO stdlib-builtin-method collisions —
///     PHP's array/collection ops are global functions (`array_push`, `count`,
///     `in_array`), never methods, and SPL interface methods are user-implemented.
///     The list would only ever drop legitimate user-method edges, so it is not
///     applied (false-positive dead code otherwise).
///   - **JS/TS**: keeps the genuine ECMAScript builtins (`push`/`pop`/`get`/`map`
///     /`filter`...) but exempts the non-ECMAScript names in
///     [`JS_CALL_NOISE_EXEMPT`] (`insert`/`remove`/`contains`).
pub fn is_cross_file_call_noise(name: &str, language: &str) -> bool {
    match language {
        "php" => false,
        "javascript" | "typescript" | "tsx" => {
            !JS_CALL_NOISE_EXEMPT.contains(&name) && CROSS_FILE_CALL_NOISE.contains(&name)
        }
        _ => CROSS_FILE_CALL_NOISE.contains(&name),
    }
}

// -- Python type-annotation noise filter --
// Builtin types + `typing` generics that appear in annotation positions but
// resolve to the stdlib, not to a project symbol. Emitting `references` edges to
// them is pure noise (they'd inflate find_references / suppress dead-code on
// names like `List`/`Optional`). Mirrors CROSS_FILE_CALL_NOISE's role for calls,
// but is Python-type-specific. Kept case-sensitive: only the exact stdlib spellings.
pub const PYTHON_TYPE_REFERENCE_NOISE: &[&str] = &[
    // builtins
    "str", "int", "float", "bool", "bytes", "None", "object",
    "list", "dict", "set", "tuple", "frozenset", "complex", "type",
    // typing generics / special forms
    "Any", "List", "Dict", "Set", "Tuple", "FrozenSet", "Optional", "Union",
    "Callable", "Sequence", "Iterable", "Iterator", "Mapping", "MutableMapping",
    "Type", "ClassVar", "Final", "Literal", "Annotated", "NoReturn", "Self",
];

// -- Python framework-registered / attribute-accessed decorators --
// Methods/functions carrying these decorators are invoked DYNAMICALLY — the
// framework or language runtime dispatches them, so they never carry an incoming
// static `calls` edge even when fully live (pydantic validators resolve to
// `caller_count: 0`; a `@property` is read as `obj.x`, not called by name). That
// makes them edgeless by nature, the same guaranteed-false-positive class as
// constructors and dunder methods — reporting them as dead code invites deleting
// live code. `find_dead_code` excludes any Python function/method whose stored
// `code_content` contains one of these as an `@`-anchored substring. The decorator
// text is available because the parser binds Python symbols to the enclosing
// `decorated_definition` wrapper (issue #31, INDEX_VERSION 36), and decorators sit
// at the head of `code_content` (never lost to tail truncation). The `@` anchor
// prevents matching a longer identifier (`@field_validator` ⊄ `@my_field_validator`).
// Bias is deliberately toward false-negatives (a genuinely-dead decorated symbol
// may be missed) — the safe direction for an LLM-facing "candidates" tool.
pub const PYTHON_FRAMEWORK_DECORATORS: &[&str] = &[
    // pydantic v2: validators/serializers/computed fields registered on the model.
    "@field_validator", "@model_validator",
    "@field_serializer", "@model_serializer", "@computed_field",
    // pydantic v1
    "@validator", "@root_validator",
    // pytest fixtures — injected by name into test signatures, not called.
    "@pytest.fixture", "@fixture",
    // property-style: accessed as an attribute (`obj.x`) → no call edge emitted.
    "@property", "@cached_property", "@functools.cached_property",
    // abstract / typing.overload stubs: dispatched via a concrete override or
    // resolved at type-check time; the stub itself carries no incoming call edge.
    "@abstractmethod", "@overload",
    // web/UI framework handlers registered by the framework at import time.
    "@ui.refreshable", "@ui.page",
];

// -- Go type-position noise filter --
// UNLIKE TypeScript (where primitives are a distinct `predefined_type` kind),
// tree-sitter-go parses builtin type names (`int`, `string`, `error`, ...) as
// `type_identifier` — the SAME kind as project types. So a builtin in type
// position (`var x int`, the `string` key of `map[string]T`, `func() error`)
// would otherwise emit a `references` edge to the builtin, inflating
// find_references and suppressing dead-code on a name like `error`/`any`. This
// set lists the Go predeclared type identifiers so they can be skipped. Builtin
// FUNCTIONS (`len`, `make`, `append`, ...) and constants (`true`, `nil`) are not
// `type_identifier`, so they never reach the type-reference extractor and are
// intentionally omitted. Kept case-sensitive: only the exact predeclared
// spellings.
pub const GO_TYPE_REFERENCE_NOISE: &[&str] = &[
    "bool", "string", "error", "any", "rune", "byte", "uintptr",
    "int", "int8", "int16", "int32", "int64",
    "uint", "uint8", "uint16", "uint32", "uint64",
    "float32", "float64", "complex64", "complex128",
    "comparable",
];

// -- Java type-position noise filter --
// Java type names in type position are `type_identifier` (UNLIKE primitives —
// `int`/`long`/`double`/`boolean`/`void`/... parse as distinct
// `integral_type`/`floating_point_type`/`boolean_type`/`void_type` kinds and
// never reach the references extractor). The common JDK reference types below
// ARE `type_identifier`, so without filtering they would emit `references` edges
// to symbols that resolve to the JDK, not a project node. They drop at cross-file
// resolution anyway (no project node exists), but skipping at extraction keeps
// the edge set clean and avoids mis-binding if a project coincidentally defines a
// same-named type. This is a MODERATE set of the very common ones (java.lang
// auto-imports, common java.util collections, common annotations), NOT an attempt
// to enumerate all of java.* . Kept case-sensitive: only the exact JDK spellings.
pub const JAVA_TYPE_REFERENCE_NOISE: &[&str] = &[
    // java.lang (auto-imported)
    "String", "Object", "Integer", "Long", "Double", "Float", "Boolean",
    "Character", "Byte", "Short", "Number", "Void", "Class",
    "Exception", "RuntimeException", "Throwable", "Error",
    "Comparable", "Runnable", "Thread", "Iterable",
    // common annotations (java.lang / java.lang.annotation)
    "Override", "Deprecated", "SuppressWarnings",
    // common java.util collections + utilities
    "List", "ArrayList", "LinkedList",
    "Map", "HashMap", "TreeMap", "LinkedHashMap",
    "Set", "HashSet", "TreeSet",
    "Collection", "Optional", "Iterator",
    // java.util.stream
    "Stream",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_max_file_size_default() {
        // Without env var set, should return the default 1 MB
        assert_eq!(max_file_size(), 1_048_576);
    }

    #[test]
    fn test_max_code_content_len_default() {
        assert_eq!(max_code_content_len(), 4096);
    }

    #[test]
    fn test_parse_timeout_ms_default() {
        assert_eq!(parse_timeout_ms(), 5000);
    }

    /// Pin the dead-code ignore defaults: `criterion_group!`-named bench fns
    /// and `claude-plugin/` shell-hook scripts are unreachable from the static
    /// AST graph and would otherwise dominate orphan results.
    #[test]
    fn test_default_dead_code_ignores_includes_macro_invoked_dirs() {
        let ignores = default_dead_code_ignores();
        assert!(
            ignores.iter().any(|p| p == "benches/"),
            "benches/ must be ignored — Criterion's criterion_group!() args aren't reference-tracked, so bench fns appear orphan"
        );
        assert!(
            ignores.iter().any(|p| p == "claude-plugin/"),
            "claude-plugin/ must be ignored — hook handlers are invoked from settings.json shell, not JS imports"
        );
    }

    /// The shared enum normalizers accept case variants (parity with
    /// normalize_confidence / normalize_type_filter / canonical_language) and
    /// each rejects the OTHER direction vocabulary + bogus values.
    #[test]
    fn test_enum_normalizers_case_insensitive_and_vocab_scoped() {
        // call direction: callers|callees|both
        assert_eq!(normalize_call_direction("both"), Some("both"));
        assert_eq!(normalize_call_direction("BOTH"), Some("both"));
        assert_eq!(normalize_call_direction("Callers"), Some("callers"));
        assert_eq!(normalize_call_direction("outgoing"), None, "deps vocab rejected");
        assert_eq!(normalize_call_direction("bogus"), None);
        // dep direction: outgoing|incoming|both
        assert_eq!(normalize_dep_direction("INCOMING"), Some("incoming"));
        assert_eq!(normalize_dep_direction("both"), Some("both"));
        assert_eq!(normalize_dep_direction("callers"), None, "callgraph vocab rejected");
        // relation: calls|imports|inherits|implements|references|all
        assert_eq!(normalize_relation("CALLS"), Some("calls"));
        assert_eq!(normalize_relation("Implements"), Some("implements"));
        assert_eq!(normalize_relation("all"), Some("all"));
        assert_eq!(normalize_relation("bogus"), None);
    }

    /// `is_test_symbol` must classify Criterion bench files as harness so
    /// `bench_*` callers don't leak into impact-analysis production caller_count
    /// (e.g. `bench_fts5_search` was inflating `fts5_search`'s prod caller count).
    #[test]
    fn test_is_test_symbol_classifies_benches_as_harness() {
        assert!(is_test_symbol("bench_fts5_search", "benches/indexing.rs"));
        assert!(is_test_symbol("bench_call_graph", "benches/indexing.rs"));
        assert!(is_test_symbol("anything", "bench/foo.rs"));
        // Production code in src/ is unaffected
        assert!(!is_test_symbol("fts5_search", "src/storage/queries/search.rs"));
        assert!(!is_test_symbol("conn", "src/storage/db.rs"));
    }

    /// `is_test_node_sql` (the node-level SQL test filter used by dead-code /
    /// surprising) MUST agree with `is_test_symbol` for every (name, path) — the two
    /// are the "same predicate, two languages" and drift silently. Runs the emitted
    /// GLOB against in-memory SQLite so this is the real matcher, not a re-transcribed
    /// mirror. The near-miss negatives (`Test_helper`, `mytest`, `latest`,
    /// `src/mytests.rs`) specifically pin the GLOB (case-sensitive, `_` literal) vs
    /// LIKE (case-insensitive, `_` wildcard) distinction — a LIKE port flips them.
    #[test]
    fn test_is_test_node_sql_matches_rust() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let sql = format!(
            "SELECT {} FROM (SELECT ?1 AS name, 0 AS is_test) n, (SELECT ?2 AS path) f",
            is_test_node_sql("n", "f")
        );
        let cases = [
            // Positives — one per leg of is_test_symbol / is_test_path.
            ("test_signup", "tests/test_api.py"),
            ("test_foo", "src/lib.rs"),      // test_ name leg
            ("MyTest", "src/lib.rs"),        // *Test
            ("SuiteTests", "src/lib.rs"),    // *Tests
            ("run", "tests/foo.rs"),         // tests/
            ("run", "test/foo.rs"),          // test/
            ("run", "benches/b.rs"),         // benches/
            ("run", "bench/b.rs"),           // bench/
            ("run", "src/__tests__/x.ts"),   // __tests__/
            ("run", "src/foo/tests.rs"),     // /tests.rs
            ("run", "pkg/foo_test.go"),      // _test.go
            ("run", "src/mod_test.rs"),      // _test.rs
            ("run", "src/a.test.ts"),        // .test.ts
            ("run", "src/a.test.tsx"),       // .test.tsx
            ("run", "src/a.spec.jsx"),       // .spec.jsx
            // Negatives — production symbols…
            ("handle_signup", "src/api.py"),
            ("format_greeting", "src/models.py"),
            // …and near-misses a LIKE port would wrongly flag.
            ("Test_helper", "src/lib.rs"),   // capital T ≠ test_ (case-sensitive)
            ("mytest", "src/lib.rs"),        // lowercase ≠ *Test
            ("latest", "src/lib.rs"),        // ends 'test' not 'Test'
            ("run", "src/mytests.rs"),       // no '/' before tests.rs
            ("run", "src/attests.py"),       // 'test' substring, no path leg
        ];
        for (name, path) in cases {
            let got: i64 = conn
                .query_row(&sql, rusqlite::params![name, path], |r| r.get(0))
                .unwrap();
            assert_eq!(
                got != 0,
                is_test_symbol(name, path),
                "is_test_node_sql disagrees with is_test_symbol for ({name:?}, {path:?})"
            );
        }
        // Stored-flag leg: is_test=1 classifies as test even when the heuristic misses.
        let flag_sql = format!(
            "SELECT {} FROM (SELECT 'plain_fn' AS name, 1 AS is_test) n, (SELECT 'src/a.rs' AS path) f",
            is_test_node_sql("n", "f")
        );
        let got: i64 = conn.query_row(&flag_sql, [], |r| r.get(0)).unwrap();
        assert!(got != 0, "is_test flag=1 must classify as test even when name/path heuristic misses");
    }

    #[test]
    fn test_is_skippable_result_covers_the_triad() {
        // <module> placeholder, <external> stub, and test symbols are skipped on
        // every search/similarity surface.
        assert!(is_skippable_result("module", "<module>", "src/a.rs"));
        assert!(is_skippable_result("function", "anything", "<external>"));
        assert!(is_skippable_result("function", "test_foo", "src/a.rs"));
        assert!(is_skippable_result("function", "foo", "tests/a.rs"));
        // Real production symbols and real (named) modules are kept.
        assert!(!is_skippable_result("function", "realFn", "src/a.rs"));
        assert!(!is_skippable_result("module", "my_mod", "src/a.rs"));
    }

    #[test]
    fn test_is_dead_code_exported_covers_all_legs() {
        // Explicit export edge.
        assert!(is_dead_code_exported(true, "fn hidden() {}", "src/a.rs", "hidden"));
        // Rust `pub` / `pub(crate)` visibility from the code content.
        assert!(is_dead_code_exported(false, "pub fn f() {}", "src/a.rs", "f"));
        assert!(is_dead_code_exported(false, "pub(crate) fn f() {}", "src/a.rs", "f"));
        // Go: an uppercase identifier in a .go file is exported. This is the leg the
        // CLI JSON path used to drop — guard it on every surface now.
        assert!(is_dead_code_exported(false, "func Handler() {}", "pkg/h.go", "Handler"));
        // Go lowercase = unexported → orphan; non-Go uppercase is not Go-export.
        assert!(!is_dead_code_exported(false, "func handler() {}", "pkg/h.go", "handler"));
        assert!(!is_dead_code_exported(false, "fn Helper() {}", "src/a.rs", "Helper"));
        // Plain private function with no callers = orphan.
        assert!(!is_dead_code_exported(false, "fn helper() {}", "src/a.rs", "helper"));
    }

    /// Rust convention: `mod tests;` resolves to `<module>/tests.rs`. Functions
    /// inside (including #[test]-free helpers like `open_with_meta_table`) must
    /// classify as test callers — otherwise `find_references` / `called_by`
    /// silently treats them as production. Symptom: `get_ast_node(snapshot::create,
    /// include_references)` listed 6 src/snapshot/tests.rs entries as prod callers
    /// while `impact.test_callers_filtered` (SQL-side, AST-flag-driven) counted
    /// them as tests — the two heuristics disagreed.
    #[test]
    fn test_is_test_symbol_classifies_rust_module_tests_rs() {
        assert!(is_test_symbol("create_writes_meta", "src/snapshot/tests.rs"));
        assert!(is_test_symbol("open_with_meta_table", "src/snapshot/tests.rs"));
        assert!(is_test_symbol("anything", "src/indexer/pipeline/tests.rs"));
        // Guard against false positives: substring must be the final segment.
        assert!(!is_test_symbol("fts5_search", "src/contests.rs"));
        assert!(!is_test_symbol("normal_fn", "src/tests_helpers.rs"));
    }

    #[test]
    fn is_test_path_classifies_by_path_only() {
        // Path-based positives (no symbol name needed).
        assert!(is_test_path("tests/foo.rs"));
        assert!(is_test_path("src/auth.test.ts"));
        assert!(is_test_path("src/Button.spec.tsx"));
        assert!(is_test_path("src/Button.spec.jsx"));
        assert!(is_test_path("pkg/handler_test.go"));
        assert!(is_test_path("a/__tests__/x.js"));
        // Negatives.
        assert!(!is_test_path("src/auth.ts"));
        assert!(!is_test_path("src/main.rs"));
        // is_test_symbol still honors the name heuristic on a non-test path.
        assert!(is_test_symbol("test_login", "src/auth.rs"));
        assert!(!is_test_symbol("login", "src/auth.rs"));
    }

    #[test]
    fn is_test_node_trusts_flag_then_heuristic() {
        // The AST flag catches the heuristic-invisible inline unit test
        // (descriptive snake_case name, src/ path) — the v0.79.1 audit case.
        assert!(is_test_node(true, "two_node_cycle_is_detected", "src/graph/cycles.rs"));
        // Flag off + heuristic off ⇒ production.
        assert!(!is_test_node(false, "two_node_cycle_is_detected", "src/graph/cycles.rs"));
        // Heuristic still classifies when the flag is absent (legacy / unprojected rows).
        assert!(is_test_node(false, "test_login", "src/auth.rs"));
        assert!(is_test_node(false, "anything", "tests/integration.rs"));
        // Genuine production caller stays production under both signals.
        assert!(!is_test_node(false, "real_caller", "src/lib.rs"));
    }

    #[test]
    fn test_is_function_node_type() {
        assert!(is_function_node_type("function"));
        assert!(is_function_node_type("method"));
        assert!(!is_function_node_type("constant"));
        assert!(!is_function_node_type("struct"));
        assert!(!is_function_node_type("enum"));
        assert!(!is_function_node_type("trait"));
        assert!(!is_function_node_type("interface"));
        assert!(!is_function_node_type("type_alias"));
        assert!(!is_function_node_type("module"));
        assert!(!is_function_node_type(""));
    }

    #[test]
    fn test_rel_references_constant() {
        assert_eq!(crate::domain::REL_REFERENCES, "references");
    }

    #[test]
    fn test_search_fetch_count_unfiltered_matches_historical() {
        // Unfiltered MUST stay byte-identical to the old inline `(top_k*4).max(20)`
        // so the retrieval benchmark (which passes no language/node_type filter)
        // is unchanged. Any drift here is a metric regression, not a refactor.
        assert_eq!(search_fetch_count(20, false), 80);
        assert_eq!(search_fetch_count(100, false), 400);
        assert_eq!(search_fetch_count(1, false), 20); // floor
        assert_eq!(search_fetch_count(3, false), 20); // floor
    }

    #[test]
    fn test_search_fetch_count_filtered_widens_pool() {
        // A selective language/node_type filter is applied AFTER the KNN fetch, so the
        // pool must be wider than the unfiltered case or the filter starves top_k.
        assert!(search_fetch_count(20, true) > search_fetch_count(20, false));
        assert_eq!(search_fetch_count(20, true), 320);
        assert_eq!(search_fetch_count(1, true), 100); // floor
    }

    #[test]
    fn test_similar_fetch_count_overfetches() {
        // `similar` post-filters self + max_distance + test/module; the old `top_k + 1`
        // fell short on any single drop. Must be a multiple of top_k (MCP-twin parity).
        assert_eq!(similar_fetch_count(10), 30);
        assert_eq!(similar_fetch_count(5), 15);
        assert_eq!(similar_fetch_count(1), 3); // max(3, 2)
        assert!(similar_fetch_count(10) > 10 + 1);
    }
}
