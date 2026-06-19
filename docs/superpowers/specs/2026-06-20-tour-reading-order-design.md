# `tour` — Dependency-Ordered Reading Order

**Status**: design approved 2026-06-20 · **Level**: L2 (additive CLI surface, published)
**Origin**: feature scouting vs `Egonex-AI/Understand-Anything` (its LLM "guided tours" → we steal only the deterministic structural skeleton: dependency-ordered reading order). `①` config/infra FTS indexing was **dropped** after verification showed `code-graph-mcp grep` already covers config text search (ripgrep over disk, index-independent), leaving `①` only marginal semantic-search/map-listing value at the cost of an INDEX_VERSION rebuild.

## Goal

Answer "I just landed in this repo (or this subtree) — what do I read first?" deterministically, with zero LLM and zero new index machinery. A new module's foundational dependencies are listed before the modules that build on them, so reading top-to-bottom builds understanding from the ground up.

## Surface

`code-graph-mcp tour [PATH] [--json]`

- `PATH` (optional, positional): scope to a subtree (filter modules whose dir is under `PATH`), mirroring `overview`. Normalized via the same path handling as other commands.
- `--json`: machine envelope. Default: human/model-readable text.
- **CLI-only** (no MCP tool) — avoids L3 LLM-visible-metadata budget + routing competition; the model reaches it via Bash, our documented fast path. Promote to an MCP tool later only if adoption warrants.
- Read-only query over the existing index — **no INDEX_VERSION bump, no reindex**.

## Data source

Reuse `queries::get_project_map(conn)` → `(Vec<ModuleStats>, Vec<ModuleDep>, Vec<EntryPoint>, _)`.
- `ModuleDep { from, to, import_count }`: `from` imports `to` (REL_IMPORTS, already excludes `<external>`). So `to` is a prerequisite of `from`.
- `EntryPoint { kind, file, .. }`: `kind == "main"` / `"http_route"` → label the containing module `[entry]`.
- `ModuleStats { path, key_symbols, files, .. }`: per-module annotation.

## Algorithm — pure, testable

New pure fn `compute_reading_order(&[ModuleStats], &[ModuleDep], &[EntryPoint]) -> Vec<ReadingOrderEntry>` (own file `src/graph/reading_order.rs` so it unit-tests in isolation; cli.rs is already large).

Kahn topological sort, **prerequisites first**:
1. Build prereq→dependent edges: for each `ModuleDep{from,to}` (only when both `from` and `to` are in the in-scope module set), add edge `to → from`; `indegree[from] += 1`.
2. Seed queue with all modules whose `indegree == 0` (import nothing internal = foundational leaves).
3. Pop in **deterministic order** (sort ready set ascending by path), emit, decrement each dependent's indegree, enqueue when it reaches 0.
4. **Cycle break (deterministic)**: when the queue empties but modules remain, pick the remaining module with the smallest indegree (tie → lexicographically smallest path), emit it flagged `in_cycle = true`, decrement its dependents, continue. Repeat until all emitted.

Determinism is mandatory (same index → same order); no `HashMap` iteration order in the output path — collect/sort explicitly.

### `ReadingOrderEntry`
```
path: String              // module dir
role: Role                // Entry | Foundational | Core | Mid
depended_on_by: usize     // # in-scope modules that import this one
depends_on: Vec<String>   // in-scope modules this one imports (for the annotation)
key_symbols: Vec<String>  // from ModuleStats
in_cycle: bool
```
Role precedence: `Entry` (module contains an entry point) > `Foundational` (indegree 0 / imports nothing internal) > `Core` (`depended_on_by` in the top tier, threshold tuned in impl — e.g. `>= 3` or top-quartile) > `Mid`.

## Output

Text:
```
Reading order (foundational → entry; <N> modules[, <C> in dependency cycles]):
  1. src/domain.rs        [foundational] · depended-on-by 4 · REL_CALLS, REL_IMPORTS
  2. src/utils            [foundational] · depended-on-by 3 · detect_language, is_compatible_lang
  3. src/storage/queries  [core] · imports domain,utils · upsert_file, insert_node
  ...
  N. src/main.rs          [entry] · main
```
Empty index → `(empty project — no indexed source files)` (mirror `cmd_map`), and `--json` MUST still emit the same-shape envelope `{"reading_order": []}` per the `cli_json_empty` contract (no bare bail to stderr).

JSON envelope:
```json
{ "reading_order": [
  { "path": "...", "role": "foundational", "depended_on_by": 4,
    "depends_on": ["..."], "key_symbols": ["..."], "in_cycle": false }
] }
```

## Wiring

- `main.rs`: add `Some("tour") => { resolve_project_root → TourArgs::parse_from → cmd_tour }` arm.
- `cli.rs`: `TourArgs { path: Option<String>, json: bool }` (clap, single-line `///`, explicit `about=` to avoid the `clap_help_doc_leak` long-about leak); `cmd_tour` mirrors `cmd_map` (CliContext::open → get_project_map → compute_reading_order → text/json).
- `cli.rs::canonical_query_cmd`: add `"tour" => "tour"` so the deny→use funnel sees it.
- No `domain.rs` / schema / INDEX_VERSION change.

## Testing (RED-first)

Unit (`reading_order.rs`):
- linear chain A→B→C ⇒ order C,B,A (prereqs first), deterministic across runs.
- diamond (A→B, A→C, B→D, C→D) ⇒ D first, A last; B,C ordered by path.
- cycle (A→B→A) ⇒ all emitted once, flagged `in_cycle`, deterministic pick.
- role labels: entry-point module → Entry; zero-indegree → Foundational.
- empty input ⇒ empty vec.

CLI (`tests/cli_*` per existing pattern):
- `test_cli_tour_basic`: fixture repo → text output lists modules, foundational before entry.
- `test_cli_tour_json_shape`: `--json` → `{"reading_order":[...]}` parses, fields present.
- `test_cli_json_empty_tour`: empty index + `--json` → exactly `{"reading_order":[]}`, exit 0 (cli_json_empty contract).
- `test_cli_tour_path_scope`: `tour src/` filters to subtree.

## Out of scope (YAGNI)

Per-file ordering (module/dir granularity only, matches data source); `--depth`; MCP tool; layer auto-classification (separate, lower-value idea).
