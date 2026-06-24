---
status: approved
revision: 1
---

# Compound-grep PostToolUse answer injection + dark-hint fix

## Goal
Raise cg's delivered value on the ~80% of foldable source greps that fly past the
PreToolUse deny gate because they sit inside compound commands (`echo "..." && grep
Sym tests/`, `git diff && grep ...`, `for s in ...; do grep`). Deliver cg's answer
to the model **permission-neutrally** via a new PostToolUse(Bash) hook, and fix the
three sibling hooks whose "hint"/"impact" output is currently written to stdout on
PreToolUse exit 0 — which goes to the debug log only and never reaches the model.

## Background (verified this session)
- Transcript ground truth (mem, 7d): 751 Edit + 683 Read + 472 grep, model-initiated
  cg = 1. PULL ≈ 0. Lever is PUSH interception coverage.
- Segment-aware re-test: mem foldable grep coverage 11 (now) → 152 (if compound greps
  are seen); +141 net-new, of which 28 PURE / 113 BUNDLED with side-effecting siblings.
- CC docs (code.claude.com/docs/en/hooks.md, v2026-06): PreToolUse exit-0 plain stdout
  → debug log only, NOT shown to model. `additionalContext` reaches the model (system
  reminder, read next model request) but ONLY alongside permissionDecision allow/deny/ask
  (defer ignores it; omitted = undocumented). `permissionDecision:"allow"` does NOT bypass
  deny/ask rules, but DOES skip the default prompt — so Bash-side inject must be PostToolUse
  (permission-neutral), NOT PreToolUse allow.

## Non-goals
- Do NOT change the existing PreToolUse(Bash) leading-grep DENY+answer path (proven,
  fallthrough 0.20). It stays exactly as-is, including bypass/cooldown/sed/tail logic.
- No INDEX_VERSION / SCHEMA_VERSION bump (hook logic only; no new node/edge/column).
- No new MCP tool / CLI flag.

## Constraints
- `claude-plugin/**` is a published surface → released-artifact checklist (minor SemVer
  bump, CHANGELOG migration note, opt-out env, discoverability).
- Sibling-hook sweep is mandatory (feedback_hook_class_bug_sweep): the dark-stdout
  delivery bug exists in pre-grep-guide (hint tier), pre-read-guide (fanout hint),
  pre-edit-guide (impact summary) — fix all three. Shared logic → shared module, no
  inline copies (project-root.js precedent).
- additionalContext payload ≤ 4000 bytes (reuse cg-answer truncateAtLine cap).
- Reuse existing pure predicates from pre-grep-guide.js; do not re-implement the grep
  gate.

## Design

### 1. New `claude-plugin/scripts/post-grep-inject.js` (PostToolUse, matcher Bash)
- Read fd 0 (NOT /dev/stdin). resolveProjectRoot(process.cwd()); null → exit 0.
- Honor isSilenced (CODE_GRAPH_QUIET_HOOKS=1) and a new opt-out `CODE_GRAPH_NO_INJECT=1`.
- cmd = normalizeCommandPaths + rebaseRelativePaths (reuse the pre-grep-guide exports).
- Split cmd into segments on `&&`, `||`, `;`, newline, and `for … in`/`do`/`done`
  boundaries — NOT on single `|` (so `cargo test | grep X` keeps head=cargo and is
  excluded as an output filter). Quote-aware split.
- For each segment, run `classifyBlock(segment)` (exported from pre-grep-guide). The
  FIRST segment whose classifyBlock is non-null AND whose head is grep (GREP_HEAD) is
  the foldable grep to answer. (Leading-grep foldable commands were DENIED in PreToolUse
  and never ran → never reach PostToolUse, so no dedup needed.)
- Run the answer exactly like the deny path: show-mode → runShowAnswer, else runGrepAnswer
  with pickBlockPattern (translateBreToRg) + sanitizeSearchPath(extractSearchPath).
  status hits → inject; no-hits / unavailable / no-binary → silent exit (no inject).
- Per-command 60s cooldown (reuse commandHash/flagPath pattern; distinct flag prefix
  `.code-graph-postinject-`).
- Emit: `{ hookSpecificOutput: { hookEventName: 'PostToolUse', additionalContext: <text> } }`
  (no permissionDecision → permission-neutral; PostToolUse additionalContext is honored).
  Text: a short header ("[code-graph] AST-aware view of your grep (ran alongside):") +
  answer.text + truncation note. Record `recordRecommendation(root, { hook:'grep',
  action:'inject', answered:true, pattern, mode })`.

### 2. pre-grep-guide.js — drop the dark hint + export helpers
- DELETE the final `recordRecommendation(... action:'hint')` + `process.stdout.write(buildHint())`
  fallthrough (lines ~667-668). A grep that passes shouldHint but not classifyBlock now
  exits silently from PreToolUse (it will be picked up by PostToolUse only if classifyBlock
  is non-null — which it is not for hint-tier, so hint-tier greps simply get no output;
  that is correct — they are the unanswerable-flag / marker / multi-path cases cg can't fold).
- Export `classifyBlock` (already exported), plus ensure `normalizeCommandPaths`,
  `rebaseRelativePaths`, `resolveProjectRoot`, `extractSearchPath`, `pickBlockPattern`,
  `translateBreToRg`, `commandHash` are exported (most already are).
- Add + export `splitTopLevelSegments(cmd)` (quote-aware; the splitter described in §1) so
  PostToolUse reuses it rather than copying.
- buildHint stays exported (still referenced by tests) but is no longer emitted.

### 3. pre-read-guide.js — stdout → additionalContext
- In `trackReadAndMaybeHint`, replace `process.stdout.write((answered ? buildHintWithAnswer
  : buildHint) + '\n')` with the PreToolUse allow+additionalContext envelope:
  `{ hookSpecificOutput: { hookEventName:'PreToolUse', permissionDecision:'allow',
  additionalContext: <hint text> } }`. (Read is a safe tool; allow elevation negligible.)

### 4. pre-edit-guide.js — stdout → additionalContext
- Replace `process.stdout.write(summary)` (line 214) with the same PreToolUse allow+
  additionalContext envelope carrying `summary`. (Edit impact must stay pre-edit.)

### 5. Shared emit helper
- Add `claude-plugin/scripts/hook-emit.js` exporting `emitPreToolAllowContext(text)` and
  `emitPostToolContext(text)` returning the JSON envelope string. pre-read/pre-edit/
  post-grep-inject all use it. (DRY; one place defines the schema.)

### 6. lifecycle.js — register the PostToolUse(Bash) hook
- In `buildSettingsHookEntries()` PostToolUse array, ADD:
  `{ description: SETTINGS_HOOK_DESC.postToolUseInject, matcher: 'Bash',
  hooks: [scriptCmd('post-grep-inject.js', 5)] }`.
- Add `postToolUseInject` to SETTINGS_HOOK_DESC. surveyHookCoverage derives desired
  matchers from buildSettingsHookEntries → auto-covered; verify doctor sees it.

### 7. src/cli.rs — aggregate_recommendations_jsonl: handle `inject`
- `is_search_event` (line ~1223): add `Some("inject")` to the action set.
- Arming block (line ~1276 `if a == "deny"`): extend to `if a == "deny" || a == "inject"`
  so an answered inject arms armed/armed_pattern (lets the funnel score inject→fallthrough
  vs sustained, parallel to deny). `inject` counts in total/by_action via the generic map.
- Add a test mirroring the deny→fallthrough tests for inject (inject answered → next
  verbatim re-grep = fallthrough; different pattern = sustained).

## Success criteria
- New e2e (real spawn, stub binary): `echo "x" && grep Sym tests/` through PostToolUse →
  emits hookSpecificOutput.additionalContext containing the stub's hits; records
  action:'inject'. `cargo test | grep FAIL` → no inject (output filter). `git diff &&
  grep Sym src/` → inject.
- pre-read fanout hint and pre-edit impact now emit hookSpecificOutput (allow+
  additionalContext), asserted in their tests (was bare stdout).
- pre-grep-guide: leading foldable grep still DENIES (regression test green); hint-tier
  grep emits nothing.
- Rust: aggregate counts `inject`; inject→fallthrough test green.
- Full suite green: JS (all *.test.js), 583 lib + 187 cli_e2e (numbers may shift with new
  tests — report new totals). routing_bench unaffected (hook change) — spot check, not required.
- Opt-out `CODE_GRAPH_NO_INJECT=1` silences the new hook (tested).

## Ship
feature branch feat/compound-grep-inject → gs:/review (fresh subagent) → sync-versions
<minor bump> → CHANGELOG migration note + opt-out + discoverability → gs:/ship → tag push
→ release.yml verify. Pre-push: `cargo +1.95.0 clippy`; read feedback_ship_baseline_and_flakes.

## Open questions
- none blocking; non-blocking-inject efficacy is measured post-ship via the new `inject`
  funnel leg (inject→fallthrough vs deny→fallthrough).

## Change log
- r1 (2026-06-25): initial, post-AUTH. Design locked: PostToolUse permission-neutral inject
  + 3-sibling dark-stdout→additionalContext sweep.
