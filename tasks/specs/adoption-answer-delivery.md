---
status: approved
revision: 1
---
# Adoption: answer-delivery upgrade (P0–P4)

## goal
Convert the plugin's guidance surfaces from "recommend a tool" (proven 0/40
transfer) to "deliver the answer" (proven 5/5 in-place satisfaction on the
2026-06-12 daagu night), and fix the three remaining instrument holes so every
subsequent night of real coding produces a trustworthy deny→use funnel.

## non-goals
- MCP tool-description / TriggerRate work (deferred-tool era: model never loads
  the tools; CLI is the proven path).
- Re-enabling marketplace hooks in this dogfood repo (user rejected).
- Hint copy polishing (0/40 measured).

## constraints
- Published-surface changes (deny scope broadening, new hook) follow the
  released-artifact checklist: minor SemVer bump, CHANGELOG migration note,
  env-var opt-out per feature, stderr/log discoverability.
- recommendations.jsonl / usage.jsonl shapes stay additive (no schema bump).
- cg-answer-style sync CLI calls inside hooks: ≤2s timeout, ≤4KB output,
  all failures degrade to prior behavior.
- Internal hook-initiated CLI calls must NOT count as model conversions
  (env marker `CODE_GRAPH_INTERNAL=1`).
- No INDEX_VERSION bump (no parser/edge changes).

## success-criteria
- P0a: a session with 0 MCP tool calls but ≥1 rec in window writes a usage
  record (unit + e2e test); funnel denominator includes 0-conversion sessions.
- P0b: model-initiated `code-graph-mcp <query-cmd>` appends
  `{hook:'cli',action:'use',cmd}` to recommendations.jsonl; cg-answer's
  internal call does not; stats shows deny→cli_use.
- P0c: stats text + JSON segment denies by answered:true/false.
- P1a: static deny no longer mentions CODE_GRAPH_NO_BLOCK_GREP.
- P1b: declaration-anchor + context-flag greps (-A/-B/-C) deny with `show`
  bodies; -l/--include greps deny with grep answer; --exclude* stays hint.
  Replay of 2026-06-12 daagu night: deny-class coverage ~20 → ~40 of 128.
- P1c: BRE `\|`-style patterns from plain grep are translated before cg grep
  runs (alternation answers instead of no-hits).
- P2a: pre-read-guide fires from subdir cwd (resolveProjectRoot + fd-0 stdin +
  abs-path rebase); recommendations.jsonl gains read-hook records in replay.
- P2b: read-fanout hint embeds `overview` output (answer, not advice).
- P2c: `sed -n X,Yp <srcfile>` counts toward read-fanout state.
- P3: PostToolUse(Edit) on a file whose symbols have callers emits a one-line
  impact FYI with top callers (new `file-impact` CLI, --json contract with
  empty-shape parity + caller_count DESC ordering).
- P4: adopt templates + MCP instructions lead with CLI forms.
- Ship: cargo test (default + --no-default-features), JS test suite, clippy
  +1.95.0 both feature sets, CI green, release.yml 9/9, npm + GH assets.

## open-questions
- P3 caller_count threshold: start at ≥3 non-test callers, tune on daagu data.

# Change log
- r1 (2026-06-13): initial, from the 2026-06-12 daagu night analysis.
