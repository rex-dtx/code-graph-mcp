#!/usr/bin/env node
'use strict';
// Real-session conversion metric (JS counterpart to the Rust MCP usage.jsonl).
//
// The PreToolUse hooks (pre-grep-guide / pre-read-guide) RECOMMEND a code-graph
// tool when Claude reaches for raw grep / fans out Reads. This module records
// each emitted recommendation so `code-graph-mcp stats` can compute the *field*
// conversion rate (recommend → actual cg tool call) — the metric the synthetic
// routing_bench oracle can't see (memory: self-dogfood-blindspot / feedback_routing_bench).
//
// Bounded + best-effort by construction:
//   - appends to <cwd>/.code-graph/recommendations.jsonl and NEVER creates the
//     `.code-graph` dir — so a non-project / tmp cwd (no index) leaves zero
//     footprint, mirroring each hook's existing `.code-graph/index.db` guard.
//   - swallows every error: telemetry must never break or delay a tool call.
const fs = require('fs');
const path = require('path');

const REC_FILE = 'recommendations.jsonl';
// Opt-in per-project metrics-silence sentinel (under .code-graph/). Mirror of the
// Rust `domain::NO_METRICS_SENTINEL` — keep the literal in sync.
const NO_METRICS_FILE = '.no-metrics';

// Bounded growth: recommendations.jsonl is append-only and written per-event
// from BOTH here and the Rust CLI (cli::record_cli_use). Keep these constants in
// sync with the Rust side (mcp::metrics::JSONL_ROTATE_MAX_BYTES / KEEP_BYTES).
const ROTATE_MAX_BYTES = 1048576; // 1 MB
const ROTATE_KEEP_BYTES = 524288; // 512 KB

/**
 * Best-effort size-based rotation: if `file` exceeds ROTATE_MAX_BYTES, rewrite
 * it keeping ~the last ROTATE_KEEP_BYTES, trimmed *forward* to the next line
 * boundary so no partial line survives. Swallows every error — telemetry
 * rotation must never break or delay a tool call. Mirror of the Rust
 * `mcp::metrics::rotate_jsonl_if_over`.
 * @param {string} file  absolute path to the JSONL file
 */
function rotateIfNeeded(file) {
  try {
    if (fs.statSync(file).size <= ROTATE_MAX_BYTES) return;
    const buf = fs.readFileSync(file);
    const start = Math.max(0, buf.length - ROTATE_KEEP_BYTES);
    const nl = buf.indexOf(0x0a, start); // first newline at/after start
    const trimStart = nl >= 0 ? nl + 1 : start;
    fs.writeFileSync(file, buf.subarray(trimStart));
  } catch {
    /* missing file or IO error → skip; the append below still runs */
  }
}

/**
 * Append one recommendation event to <cwd>/.code-graph/recommendations.jsonl.
 * @param {string} cwd        project root (the hook's process.cwd())
 * @param {object} event      e.g. { hook: 'grep', action: 'deny' }
 * @returns {boolean} true if a line was written
 */
function recordRecommendation(cwd, event = {}) {
  try {
    const dir = path.join(cwd, '.code-graph');
    // Append-only: do NOT create .code-graph. Its absence means "not an indexed
    // project" — recording there would pollute non-project cwds.
    if (!fs.existsSync(dir)) return false;
    // Opt-in metrics silence for dev/dogfood checkouts: when the project marks
    // itself with `.code-graph/.no-metrics`, the tool's own hook/CLI runs (sims,
    // functionality testing) must not self-pollute its adoption metrics. Mirrors
    // the Rust cli::record_cli_use guard. Reversible: delete the file to re-enable.
    if (fs.existsSync(path.join(dir, NO_METRICS_FILE))) return false;
    const file = path.join(dir, REC_FILE);
    rotateIfNeeded(file); // rotate-before-append so the file never exceeds ~max + one line
    const line = JSON.stringify({ ts: new Date().toISOString(), ...event }) + '\n';
    fs.appendFileSync(file, line);
    return true;
  } catch {
    return false;
  }
}

module.exports = { recordRecommendation, REC_FILE };
