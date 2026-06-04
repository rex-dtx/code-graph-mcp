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
    const line = JSON.stringify({ ts: new Date().toISOString(), ...event }) + '\n';
    fs.appendFileSync(path.join(dir, REC_FILE), line);
    return true;
  } catch {
    return false;
  }
}

module.exports = { recordRecommendation, REC_FILE };
