# Outcome Phase-2: CLI-via-Bash cg-call parsing

- **Date**: 2026-06-29
- **Status**: Design approved (inline); pending design-doc commit → writing-plans
- **Builds on**: `docs/superpowers/specs/2026-06-29-outcome-measurement-design.md` (v1, merged `e3891f6`)
- **Type**: Focused extension of the read-only `code-graph-mcp outcome` instrument.

---

## 1. Why

v1 scores only the model's **MCP** `tool_use` calls to cg tools. But real consumer PULL is dominated by **CLI-via-Bash** calls — the model typing `code-graph-mcp callgraph X` / `... grep ...` in a Bash tool_use (daagu ≈ 46 such calls; daagu MCP cg = 0). v1 therefore reads ~0 adoption on real consumers. Phase-2 makes those CLI calls visible so the adoption rate (and, for `search`, field-MRR) reflects real traffic.

## 2. Scope decision (user-approved)

- `code-graph-mcp search` is relevance-ordered (FTS/BM25) → **ranked**: rank = result order in stdout → contributes to **field-MRR** + adoption.
- All other CLI query subcommands (`grep`, `callgraph`, `show`, `impact`, `overview`, `refs`, `deps`, `similar`, `trace`, `ast-search`, …) are structural → **rank=None** → **adoption only**.

## 3. Non-gameability (unchanged anchor)

Still counts **only transcript `tool_use`** — here, the model's **Bash** tool_use whose command invokes `code-graph-mcp`. Hooks that run the binary execute out-of-band (not as the model's tool_use), so they are NOT counted — the iron-law anchor (count real tool_use, immune to the `cli/use`-leg leak) holds for CLI exactly as for MCP.

## 4. Components (all in `src/outcome.rs`)

### 4.1 Detection — `parse_transcript` Bash branch
- Reuse `crate::cli::canonical_query_cmd(sub) -> Option<&'static str>` (`src/cli.rs:1015`) as the single source of truth for which subcommands are cg queries (it already covers MCP-name aliases + excludes housekeeping).
- A Bash `tool_use` command is a CLI cg call iff it contains the token `code-graph-mcp` followed by a token that `canonical_query_cmd` maps to `Some(canon)`. Must handle **compound** commands (`cd X && code-graph-mcp callgraph Y`, `echo … && code-graph-mcp grep …`) → scan the command's whitespace tokens for `code-graph-mcp <subcmd>` anywhere, not just at head.
- **cg-CLI is checked BEFORE the RawGrep branch** so `code-graph-mcp grep …` becomes a `CgCall`, not `RawGrep` (this also tightens the v1 vestigial `contains("grep ")` loose-match the final review noted).
- Pair with the Bash `tool_result` text (= the command's stdout) via the existing two-pass `tool_use_id` map.
- Tool name on the emitted `CgCall` = `"<canon>_cli"` (e.g. `search_cli`, `callgraph_cli`) to keep CLI distinct from MCP in `by_tool`.

### 4.2 Extraction — `extract_returned_from_cli(stdout: &str, ranked: bool) -> Vec<ReturnedItem>`
- **JSON fast-path**: if `stdout` parses as a `serde_json::Value`, delegate to `extract_returned(&value, ranked)` (covers a model that passed `--json`; the CLI `--json` shape is the same `{results}` object v1 already handles).
- **Else regex** the human output: extract unique file paths from `path:line` / `path:line-line` tokens (cg CLI uniformly surfaces hits as `src/foo.rs:63` / `CHANGELOG.md:3708-3709`). Pattern intent: a path-like token (contains `/` or `.`, ends in an extension) immediately followed by `:<digits>(-<digits>)?`. Dedupe to unique paths in **first-occurrence order**; `ranked` → rank = first-occurrence index, else `None`.

### 4.3 Ranked classification
`is_ranked_tool` (or its caller) must treat `"search_cli"` as ranked and every other `"*_cli"` as structural. Keep `RANKED_TOOLS` semantics: MCP `semantic_code_search`/`ast_search` (ranked) + the new `search_cli`.

## 5. Scoring / aggregation / labels — UNCHANGED

CLI `CgCall`s flow through `score_session` (returned files → subsequent untouched-before Read/Edit = adoption) and `aggregate` (search_cli joins the ranked field-MRR denominators; `by_tool` shows `*_cli`) exactly like MCP calls. `emit_labels` emits CLI rows too.

## 6. Error handling

- 0-hit CLI call (`[code-graph] No call graph results …`) → regex finds no `path:line` → empty `returned` → the call is counted but unadoptable (correct: a cg call that returned nothing).
- A cg-CLI `tool_use` with no paired `tool_result` → `unresolved` (same as MCP).
- stdout that is neither JSON nor contains `path:line` tokens → empty `returned` (no panic).
- A `code-graph-mcp` invocation whose subcommand is housekeeping (`canonical_query_cmd → None`, e.g. `serve`/`stats`/`doctor`) → not a CgCall; falls through (RawGrep check, then Other).

## 7. Testing

- `parse_transcript`: a Bash cg-CLI `tool_use` + a `file:line` stdout `tool_result` → one `CgCall` with the extracted unique files; `search_cli` ranked vs `callgraph_cli` structural (rank=None).
- Compound-command detection (`cd be && code-graph-mcp callgraph Foo`).
- `code-graph-mcp grep …` → `CgCall` (NOT `RawGrep`); a raw `grep …` / `rg …` still → `RawGrep`.
- JSON fast-path: a `--json` stdout (`{"results":[{file_path…}]}`) → delegates to `extract_returned`, ranks assigned for `search_cli`.
- `extract_returned_from_cli`: dedupe first-occurrence; `path:line-line` token; non-path stdout → empty.
- Housekeeping subcommand (`code-graph-mcp stats`) → not a CgCall.
- **Real daagu calibration (success criterion):** rebuild + `outcome --project /mnt/data_ssd/dev/projects/daagu` now shows ≈ the hand-forensic ~46 CLI calls with a real adoption rate (and `search_cli` field-MRR where present) — vs v1's 0. Sanity-check the count is plausible, not 0 and not absurd.

## 8. Non-goals / invariants

- Single-file extension (`src/outcome.rs`) + a read-only reuse of `cli::canonical_query_cmd`. No `INDEX_VERSION` / `SCHEMA_VERSION` bump. Read-only (writes nothing except `--emit-labels`).
- No new runtime dependency (regex via a small hand-rolled scan or the existing `regex` crate if already a dep — verify at implementation; prefer a dependency-free byte scan if `regex` is not already present, to honor the no-new-runtime-dep rule).
- CLI rank for non-`search` stays `None` (don't fabricate relevance order for `grep`/`callgraph` — that would pollute field-MRR, the exact anti-gaming rule v1 enforces).

## 9. Success criterion

Not "the code runs": **on daagu, `outcome` now reports a non-zero adoption rate over ≈46 real CLI PULL calls** (vs v1's 0), making the instrument actually reflect real consumer traffic — the gap v1's calibration exposed.
