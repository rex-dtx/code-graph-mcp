# Outcome Measurement: `code-graph-mcp outcome` — retrieval adoption from real transcripts

- **Date**: 2026-06-29
- **Status**: Design approved; pending spec review → writing-plans
- **Type**: Speculative R&D (not a bounded bugfix). Read-only measurement instrument.
- **Topic**: Give ranking / `is_test` / confidence-floor tuning an objective, hard-to-game judge by measuring whether code-graph retrieval results actually get *used* by the model, anchored on real session transcripts.

---

## 1. Problem & goal

Every retrieval/graph improvement so far (ranking, `is_test` classification, confidence floor) tunes a system with **no outcome judge** — knobs turned by intuition. The only existing signals answer *adoption* ("did the model call the tool?") and *volume* ("how many calls?"), never *outcome* ("after calling, did the model actually do better?").

This builds the missing judge: a number that moves when a ranking change is genuinely better or worse, measured on **real model behavior**, not a self-graded score.

The judge: **of the items code-graph returned, did the model act on them — and how highly was the acted-on item ranked?** A good ranking puts the item the model actually edits near the top.

## 2. What already exists (and why this is different)

A first-generation outcome proxy already ships: the "search-decay" / fall-through machine in `src/cli.rs:1269–1441` (`aggregate_recommendations_jsonl` + `RecommendationSummary`). After an **answered deny / answered inject** (the PUSH path delivers an inline cg answer), it watches the immediately-next grep/read and scores `sustained` (cg also answered the deeper step — a win) vs `fallthrough` (answer insufficient) vs `followup_inconclusive` (null). Live reading `fallthrough_rate ≈ 0.12` in the dev repo.

That base is sound and already de-gamed twice (the "61% re-search" healthy-drill-down artifact → 7%; the `no-hits` over-count → excluded). **This design does not rebuild it.** Its scope limits *are* the gap this fills:

| Existing fall-through proxy | This design |
|---|---|
| PUSH only (arms on deny/inject) | **PULL** — model voluntarily calls `semantic_code_search`/`ast_search`/`get_call_graph`/… |
| "did the next search stop" (proximate) | "did a **returned item** get acted on" (adoption of the actual result) |
| no rank signal | **rank of the adopted item** (field-MRR) — the ranking judge |
| reads `.code-graph/recommendations.jsonl` (hook events; the `use` leg was once pollution-leaked 100-vs-5) | reads **harness transcripts** — counts real `tool_use`, the one trustworthy anchor per the iron law |

Anchoring on transcript `tool_use` is deliberate: prior audits proved hook-delivered `use` events leaked (recorded 100 model calls; cross-transcript truth ≈ 5). Counting `tool_use` directly is immune to that leak **by construction**.

This adds **no new always-on recording, injection, or hook** — it reads data the harness already writes. That is intentionally *not* the "new pipeline" prior work warned against.

## 3. Verified data foundation

Confirmed by inspecting real transcripts (461-event sample; daagu 40 / mem 69 / code-graph-mcp 88 `.jsonl` files; ~10–50 ms parse, no truncation, no opaque blobs):

Transcripts live at `~/.claude/projects/<slug>/<session-uuid>.jsonl`, one JSON event per line. `<slug>` = project path with `/` and `.` → `-` (reuse `claude-plugin/scripts/adopt.js` `memoryDir()` slug logic; do not hand-roll).

**cg tool_use** (assistant message):
```
.type = "assistant"
.message.content[] : { type:"tool_use", id:"toolu_…",
                       name:"mcp__code-graph-dev__semantic_code_search",
                       input:{ query, top_k, compact, … } }
```

**tool_result** (next user message), matched by `tool_use_id`. The returned payload IS present as a JSON-stringified array:
```
.message.content[] : { type:"tool_result", tool_use_id:"toolu_…",
                       content[0].text = "[ {file_path, line, name, node_id,
                                            relevance, signature, type}, … ]" }
```

**Read / Edit / Write tool_use**: `.message.content[].input.file_path` is the target path.

→ Overlap (returned files ∩ subsequently-acted files), and the **rank/relevance** of the adopted item, are recoverable from the transcript alone. Feasibility: confirmed, zero blockers.

> Caveat to verify in implementation: payload shape differs per tool and may differ in `compact` mode. `semantic_code_search` / `ast_search` return a relevance-ordered flat array (rank = array index). `get_call_graph` / `find_references` / `module_overview` return structural sets (callers/refs/symbols) with file paths but **no relevance ranking**. `get_ast_node` returns a single node (+impact). Per-tool payload extractors required.

## 4. Architecture

A new **read-only** CLI subcommand:

```
code-graph-mcp outcome [--project <path>] [--since <days>] [--json] [--emit-labels <path>]
```

- Separate from `stats` because the **data source differs**: `outcome` reads harness transcripts (`~/.claude/projects/<slug>/`), `stats` reads project-local `.code-graph/*.jsonl`.
- Writes nothing, except the optional `--emit-labels <path>` JSONL (the phase-2 replay dataset) to an explicit path.
- `--project` defaults to cwd; intended to run against **consumer** transcripts (daagu / mem), not the pollution-mixed dev repo.

Data flow: `transcript_dir → for each *.jsonl (mtime ≥ --since) → parse_transcript → score_session → aggregate → render / emit_labels`.

## 5. Components (small, independently testable units)

| Unit | Responsibility | Purity |
|---|---|---|
| `transcript_dir(project_root, home) -> PathBuf` | cwd → slug → transcript dir | pure |
| `parse_transcript(&str) -> Vec<Event>` | streaming, one forward pass, skip malformed lines (telemetry, not a contract) | pure |
| per-tool `extract_returned(tool, payload_text) -> Vec<ReturnedItem>` | tool-specific payload → ranked/unranked returned items | pure |
| `score_session(&[Event]) -> SessionOutcome` | the adoption + rank logic (§6) | pure |
| `aggregate(Vec<SessionOutcome>) -> OutcomeSummary` | rates, rank histogram, field-MRR, per-tool | pure |
| `emit_labels(&[SessionOutcome], path)` | write `(query, returned, adopted, rank)` JSONL | IO |
| `render(&OutcomeSummary, json)` | human table / `--json`, mirroring `stats` style | IO |

```
Event =
  | CgCall   { tool: String, query: String, returned: Vec<ReturnedItem>, id: String }
  | FileTouch{ kind: Read | Edit, path: String }
  | RawGrep  { pattern: String }          // for the optional PULL fall-through negative
  | Other

ReturnedItem = { file_path: String, rank: Option<usize>, relevance: Option<f64>, name: String }
                                   // rank = Some only for ranked-list tools (search/ast_search)
```

`CgCall` assembly: an assistant cg `tool_use` is paired with its `tool_result` by `tool_use_id`; the result `content[0].text` is parsed by the tool's `extract_returned`.

## 6. Core scoring algorithm

`score_session` is a single forward pass holding `touched_before: HashSet<path>`:

```
for event in session_events:
    match event:
        CgCall(call):
            forward-scan from here until the NEXT CgCall (or session end):
                first FileTouch whose path ∈ call.returned
                    AND path ∉ touched_before (at the moment of the call)
                → ADOPTED; adopted_rank = that returned item's rank
            (no such touch) → not adopted
            record { tool, query, returned_n, adopted: bool, adopted_rank }
        FileTouch(t):
            touched_before.insert(t.path)
        RawGrep right after a CgCall with no adoption  → mark pull_fallthrough (optional)
```

The `touched_before` guard is load-bearing: cg gets credit only when it surfaced a file the model had **not already opened**, then acted on it. Files the model already had are excluded.

The forward-scan boundary (`until the next CgCall`) is a **named sensitivity parameter** `ADOPTION_WINDOW`, not a silent constant — it directly biases the headline rate. "Until next CgCall" is the conservative default (a later cg call's results can't be misattributed to this one), but it can systematically *under*-count when the model searches, reads several files, searches again, then edits a result of the first search. The default is therefore not asserted as correct: it is **calibrated against the daagu hand-forensic adoption labels** in the integration test (§10), and the chosen value (next-CgCall vs N-events vs end-of-session) is whichever best matches hand-labeled adoption. Picking it silently is the exact artifact class this design exists to avoid.

## 7. Metric definitions & non-gameability (load-bearing)

The recurring failure across ~10 prior audits was **loose predicates** silently counting healthy behavior as success/failure (the 61%→7% artifact; the 100-vs-5 leak). Each metric below is pinned to the failure mode it guards.

- **Adoption (binary, per cg call)** — a returned `file_path` is later Read/Edited in the same session, and was *untouched before the call*.
  - Guards: the "credit cg for files the model already had" survivorship trap (via `touched_before`).
  - Honest limitation, stated not hidden: adoption ≠ causation. We credit cg only when it surfaced something *new* that then got acted on — the strongest defensible proxy, not proof.

- **`adopted_rank`** — 0-based index of the adopted item in the returned list; best (lowest) rank if several returned items are touched. **Only defined for ranked-list tools** (`semantic_code_search`, `ast_search`). Structural tools (`get_call_graph`, `find_references`, `module_overview`) contribute to adoption but not to rank (they are not relevance-ordered).

- **field-MRR** — mean reciprocal rank `1/(adopted_rank+1)` over the **ranked-list tools**, reported in TWO clearly-labeled variants:
  - `field_mrr_adopted` — over adopted calls only → *ranking quality given the model adopted something*.
  - `field_mrr_all` — over all calls, non-adopted = 0 → conflates adoption and ranking.
  - Both are reported because collapsing them into one number is exactly the artifact class to avoid. This is the field counterpart to `routing_bench` P@1, but on **real adoption labels**, not a synthetic oracle.

- **Anchoring** — every input is a real assistant `tool_use` + real Read/Edit. **No hook-delivered `use` events are counted** (the pollution-leaked leg). Immune to the 100-vs-5 leak by construction.

- **Small-N guard** — under `MIN_N` resolved cg calls (default 20), the human output prints `LOW CONFIDENCE: N=<n> too small to conclude` and JSON sets `low_confidence:true`. mem PULL ≈ 0 yields single-digit N; daagu (≈3.5% PULL, ~46 calls) is the real test bed. Guards small-sample over-claiming.

## 8. CLI surface & output

Human:
```
Outcome (retrieval adoption)  —  project: daagu   transcripts: 40   since: 30d
Resolved cg calls: 46   (unresolved 2, unparseable 0)
Adoption: 31/46 = 67%
field-MRR (ranked tools, adopted): 0.71   (all calls): 0.48
Adopted-rank histogram: r0=18  r1=7  r2=4  r3+=2
By tool: semantic_code_search 22/30  ast_search 5/8  get_call_graph 4/8(no-rank)
```
(numbers illustrative)

`--json` emits an `outcome` object: `{ project, transcripts, since_days, cg_calls, unresolved, unparseable, adopted, adoption_rate, field_mrr_adopted, field_mrr_all, rank_histogram, by_tool{…}, n_sessions, low_confidence, first_ts, last_ts }`.

## 9. Error handling

- Malformed JSONL line → skipped (telemetry, not a contract surface).
- Transcript dir absent → explicit `no transcripts for <project> at <path>` (mirrors `stats` dark-state honesty; never silently empty).
- `tool_use` with no matching `tool_result` (truncated / in-flight session) → skipped, counted as `unresolved` (surfaced, not dropped).
- Payload not the expected JSON shape (tool returned an error; `compact` mode differs) → counted as `unparseable`.
- Read-only throughout; writes only to an explicit `--emit-labels` path.

## 10. Testing

- `parse_transcript` on a crafted fixture: cg call + result + subsequent Edit (hit / miss / already-touched-before / multi-item best-rank / no forward touch).
- per-tool `extract_returned`: ranked array (search) vs structural set (callgraph) vs compact-mode payload.
- `score_session`: adoption, `touched_before` exclusion, adopted_rank selection, the until-next-CgCall window boundary.
- field-MRR math: `_adopted` vs `_all` variants.
- Small-N guard fires under threshold.
- Integration: run against a real daagu transcript and sanity-check the count against the hand-forensic ~46 PULL calls recorded in prior audits — the number must be plausible, not just non-crashing.
- Calibration: on a small hand-labeled daagu sample, compare `ADOPTION_WINDOW` candidates (next-CgCall / N-events / end-of-session) against hand-judged adoption; pick the value that matches, and record it. This is what keeps the headline rate honest rather than an artifact of an arbitrary window.

## 11. Scope / phasing

- **v1 (this build)** — the read-only `outcome` reader + `--emit-labels`, MCP PULL tool calls, ranked-list field-MRR + all-tool adoption, consumer transcripts.
- **Phase-2 (enabled by v1's labels, not built now)**:
  - **Counterfactual replay (C)** — take `(query, adopted_file)` labels, re-run a candidate ranking against the live index, report where the adopted item ranks now → before/after evaluation of a ranking change on real labels.
  - PUSH-path coverage (deny/inject answers live in `permissionDecisionReason` / `additionalContext` text — a different transcript shape).
  - CLI-via-Bash cg calls (`code-graph-mcp search/callgraph/…` — result is Bash stdout).
  - Cross-session adoption.

## 12. Non-goals & invariants

- **No `INDEX_VERSION` / `SCHEMA_VERSION` bump** — read-only CLI; touches no index, DB, or `recommendations.jsonl` schema.
- **Additive published surface** — a new `outcome` subcommand on the end-user `code-graph-mcp` CLI (project published-client boundary). Additive, non-breaking → soft; whether to bundle it into a release is a separate later decision (v1 only needs to run and produce a trustworthy number).
- Runs against consumer transcripts; the dev repo is pollution-mixed and not the measurement target.

## 13. Success criteria

Not "the code runs," but: **on daagu, it produces a trustworthy, stable field-MRR + adoption number that is then actually used to evaluate the next ranking change.** Building the instrument and never using it to gate a change would repeat the documented "built a metric, changed nothing" trap. The metric earns its place only as the judge for subsequent retrieval tuning.
