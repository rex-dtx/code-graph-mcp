---
status: approved
revision: 1
---

# deny-grep-inline-answer — deny 时直接给答案

## goal

When `pre-grep-guide.js` denies a symbol-shaped raw grep, run the AST-aware
equivalent (`code-graph-mcp grep "<pattern>" [path]`) synchronously inside the
hook and embed the actual results in the deny reason — so Claude receives the
answer without having to choose to call a cg tool. This bypasses the measured
~0% recommend→use transfer rate: the model doesn't need to initiate a new tool
call when the result is already in front of it.

## non-goals

- No change to `pre-read-guide.js` / `pre-edit-guide.js` (hint-only hooks; no deny tier).
- No broadening of the deny boundary (`shouldBlock` predicate unchanged).
- No Rust changes: CLI `grep` verified to NOT write usage.jsonl, so the
  deny→use funnel denominator/numerator logic (`count_recs_in_window`,
  `CG_QUERY_TOOLS`) is untouched. `answered` is an additive JSONL field that
  serde_json::Value readers ignore.
- No version bump in the feature commit (done at ship time via sync-versions).

## constraints

- Hook latency budget: spawnSync timeout 2000ms; CLI grep measured 18ms warm
  in this repo. Timeout/failure → graceful fallback to the v0.46 static deny.
- Deny reason size: results truncated at line boundary to ≤4000 bytes.
- Telemetry must never break the tool call (mirror recommendation-log.js
  swallow-all posture).
- Pattern dialect mismatch (BRE `\|` vs ripgrep regex): cg-grep 0 hits is NOT
  proof of absence → on no-hits, ALLOW the raw grep through with a one-line
  FYI instead of denying (denying with "no matches" could mislead).
- Opt-out (released-artifact checklist): `CODE_GRAPH_NO_ANSWER_IN_DENY=1`
  restores the v0.46 static deny. `CODE_GRAPH_NO_BLOCK_GREP=1` still downgrades
  the whole block tier to hint.
- No shell interpolation: spawnSync with array args only.

## success-criteria

1. Denied symbol grep with ≥1 cg hit → deny reason contains the actual
   `code-graph-mcp grep` output + the command that produced it + escape hatch.
   recommendations.jsonl gains `{action:"deny", answered:true}`.
2. cg grep returns no matches → raw grep ALLOWED, one-line FYI emitted,
   `{action:"hint", fallthrough:"no-hits"}` recorded.
3. Binary missing / CLI error / timeout / oversized pattern → static deny
   (v0.46 behavior), `{action:"deny", answered:false}`.
4. All new logic covered by node --test (cg-answer.test.js + pre-grep-guide
   additions, incl. stdin-spawn e2e with stub binary via `_CG_ANSWER_BINARY`);
   full existing JS + cargo test suites stay green.
5. Live smoke in this repo: hook fed a real denied command produces deny JSON
   with embedded real results.

## open-questions

- Funnel interpretation shift: answered denies satisfy the need in-place, so
  deny→use conversion will read LOW even when the feature works. Reading
  Piece 3 must segment by `answered` (jq) — noted in CHANGELOG + memory.

# Change log

- r1 (2026-06-11): initial, approved via user instruction "把方案 2 实现了，deny 时直接给答案".
