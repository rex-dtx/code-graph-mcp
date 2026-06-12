---
status: implemented
revision: 2
---

# grep-parity — make `code-graph-mcp grep` a drop-in grep replacement

## goal
Fix the 4 bugs and 2 contract gaps found in the 2026-06-13 audit so `code-graph-mcp grep`
can replace interactive grep/rg usage (the pre-grep-guide deny hook promises this), while
keeping its differentiator (AST container annotation).

## non-goals
- ~~Context lines / `-l`~~ (delivered in r3 on user request); `-c` / `-v` / `-o` /
  `-q` / `-e` multi-pattern / `-f` pattern-file / stdin: still out of scope.
- Searching .git/target/node_modules (grep -rn wart): alignment target is `git grep`
  semantics (tracked + un-ignored), not raw `grep -rn`.
- MCP tool surface: CLI only; no tool schema change, no routing-bench run needed.

## constraints
- Published CLI (npx/cargo install) — exit-code change is breaking: needs CHANGELOG
  migration note + revert path (pin prior version) + discoverability (stderr is the
  product surface; CHANGELOG top note).
- `--json` empty contract (feedback_cli_json_empty_contract): every early-bail still
  emits same-shape JSON (array → `[]`).
- cg-answer.js (deny-with-answer) consumes exit codes + stdout — must be updated in
  the same change (parallel-path completeness).
- No INDEX_VERSION bump (no parser/edge changes).

## success-criteria
1. `grep "--no-default-features"` returns hits (leading-dash patterns work).
2. Per-file truncation surfaces on stderr; `--max-count 0` lifts the cap.
3. Repo-wide search finds tracked-but-gitignored files (CLAUDE.md, docs/AUDIT) —
   parity with `git grep`.
4. `... | head` exits without "Broken pipe" stderr noise.
5. Exit codes: 0 = matched, 1 = no match, 2 = error (grep-compatible).
6. `-i` / `-w` / `-F` / multi-path work and match rg ground truth.
7. `--json` mode latency within ~1.2× of text mode (shared node cache).
8. cargo test + JS suite green; cg-answer.js treats exit 1 as no-hits, not error.

## open-questions
- (resolved) opt-out for exit-code change: CHANGELOG pin-prior-version instructions,
  no env flag — keeps surface small.

# Change log
- r4 (2026-06-13): query-time freshness for annotations — lazy per-file
  hash-compare + `ensure_file_indexed` on dirty, sync-budget 8 files
  (env `CODE_GRAPH_GREP_SYNC_BUDGET`), busy_timeout 250ms, `[stale]` marker +
  stderr hint on fallback; sync restricted to files already in the index (so
  gitignored supplement files never leak into the index). +6.7ms repo-wide
  (29.6→36.3ms avg). Tests: resync-after-edit, budget-0 stale marker (text+JSON).
- r3 (2026-06-13): + `-l` (rg -l passthrough, JSON = string array) and `-A/-B/-C N`
  (rg JSON context records; grep `:`/`-`/`--` formatting; AST arrows on matches
  only). non-goals updated accordingly. Guidance surfaces (deny/hint, MCP
  instructions, CLAUDE.md, template, local adopted memory) updated in lockstep.
- r2 (2026-06-13): implemented. All 8 success-criteria met (cli_e2e 144→152,
  JS suite 605 green, clippy clean). Correction from audit: docs/AUDIT-2026-06-03.md
  is untracked (audit misread `ls` output as `git ls-files`) — staying invisible
  repo-wide is correct git-grep semantics; tracked-ignored set = 4 superpowers
  docs + CLAUDE.md, all now searchable. Criterion 7: json/text = 38.4/29.8ms avg
  (1.29×, was 2.4×). Follow-up (not this rev): pre-grep-guide could pass multiple
  files to the now-multi-path CLI — deny-surface change, needs replay validation.
- r1 (2026-06-13): initial, from audit findings.
