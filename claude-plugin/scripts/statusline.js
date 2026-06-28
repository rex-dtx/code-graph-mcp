#!/usr/bin/env node
'use strict';
const { execFileSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { findBinary } = require('./find-binary');
const { resolveProjectRoot } = require('./project-root');
const lifecycle = require('./lifecycle');
const cleanupDisabledStatusline = lifecycle.cleanupDisabledStatusline || (() => ({ cleaned: false }));

// True when auto-update has a newer release queued or in flight (the background
// downloader in session-init.js hasn't promoted the new binary yet). Used to show
// a transient "updating" state instead of the alarming "offline" during that window.
// After this many consecutive failed download attempts (auto-update.js tracks
// updateAttempts), a pending update is treated as STUCK: the statusline stops
// showing "↻ updating" (which asserts an in-progress self-heal) and surfaces the
// real status instead. Without this, a persistently-failing update (missing
// tar/curl, full disk, blocked network) pins "updating" forever.
const STUCK_UPDATE_ATTEMPTS = 5;
function updatePending() {
  try {
    const st = JSON.parse(fs.readFileSync(
      path.join(os.homedir(), '.cache', 'code-graph', 'update-state.json'), 'utf8'));
    if ((st.updateAttempts || 0) >= STUCK_UPDATE_ATTEMPTS) return false;
    if (st.updateAvailable) return true;
    if (st.latestVersion && st.installedVersion && st.latestVersion !== st.installedVersion) return true;
  } catch { /* no state file or unreadable — treat as no pending update */ }
  return false;
}

const disabledCleanup = cleanupDisabledStatusline();
if (disabledCleanup.cleaned) process.exit(0);

// Only show status in projects that have a code-graph directory. The statusLine
// config is global, so we must exit silently for non-code-graph directories.
// Walk UP to the canonical project root (resolveProjectRoot) rather than keying
// on the bare process.cwd(): when the shell sits in a subdir, the bare-cwd gate
// either showed a STRAY nested subdir index (monorepo relic — the statusline
// "oscillating" between root/backend/frontend node counts) or, in a clean subdir
// with no local index, showed nothing at all. The resolver skips stray nested
// indexes, so the statusline tracks one DB — the project root — from any subdir.
//
// Start from Claude Code's AUTHORITATIVE current dir (CODE_GRAPH_STATUSLINE_CWD,
// forwarded by the composite from its stdin payload) rather than process.cwd().
// The spawned statusline's process.cwd() is an implementation detail of how
// Claude Code launches the command and need not track the session's working dir;
// the stdin `cwd` always does. Fall back to process.cwd() when unset (direct
// invocation, tests).
const startDir = process.env.CODE_GRAPH_STATUSLINE_CWD || process.cwd();
const root = resolveProjectRoot(startDir);
if (!root) {
  process.exit(0);
}
const codeGraphDir = path.join(root, '.code-graph');

// Check for background indexing progress file first
const progressFile = path.join(codeGraphDir, 'indexing-status.json');
try {
  const raw = fs.readFileSync(progressFile, 'utf8');
  const p = JSON.parse(raw);
  if (p.s === 'indexing' && p.t > 0) {
    const pct = Math.round((p.d / p.t) * 100);
    process.stdout.write(`code-graph: \u21BB indexing ${p.d}/${p.t} (${pct}%)`);
    process.exit(0);
  }
} catch { /* no progress file or parse error — continue to health check */ }

// No indexing in progress — show normal health status
if (!fs.existsSync(path.join(codeGraphDir, 'index.db'))) {
  process.exit(0);
}

const bin = findBinary();
if (!bin) {
  // No usable binary yet. If an update is queued, the background downloader is
  // still fetching it \u2014 that is "updating", not a broken "offline" state.
  process.stdout.write(updatePending() ? 'code-graph: \u21bb updating' : 'code-graph: offline');
  process.exit(0);
}

// Render the standard health line from a parsed health-check report. An
// unhealthy/empty index (healthy:false, 0 nodes) is a real, accurate state and
// is distinct from "offline" \u2014 the binary ran fine, the index just has no data.
function renderHealth(s) {
  const icon = s.healthy ? '\u2713' : '\u2717';
  let line = `code-graph: ${icon} ${s.nodes} nodes | ${s.files} files`;
  // Surface vector-backfill progress so a structurally-complete but only
  // partially-embedded index reads as "healthy and improving" (the embedding
  // backfill is resumable and runs in the background), not as something stuck.
  // Hidden when embeddings are complete, unavailable (no model), or there are no
  // nodes yet \u2014 only the in-progress states add the suffix.
  if (s.nodes > 0) {
    if (s.embedding_status === 'partial' && typeof s.embedding_coverage_pct === 'number') {
      line += ` | ${s.embedding_coverage_pct}% vec`;
    } else if (s.embedding_status === 'pending') {
      line += ' | vec pending';
    }
  }
  // An index built by an older extractor generation is usable but a rebuild is
  // owed (a background incremental-index revalidates it). Flag it so a stale
  // index doesn't masquerade as fully current.
  if (s.index_version_stale) line += ' | \u21bb rebuilding';
  if (s.watching) line += ' | watching';
  return line;
}

// A genuine report carries a boolean `healthy` field. Returns null for anything
// that isn't a parseable report (empty string, crash banner, partial output).
function parseReport(text) {
  try {
    const s = JSON.parse(text);
    return (s && typeof s.healthy === 'boolean') ? s : null;
  } catch { return null; }
}

// No usable report: the binary couldn't produce one (crashed / missing / schema
// too new). A schema-version error means the resolved binary is OLDER than the
// index it is reading \u2014 the classic post-update window where the new binary is
// still downloading. That, or any pending update, is transient: show "updating"
// so the user knows it self-heals, rather than the misleading "offline".
function statusUnavailable(errText) {
  // Primary signal: the binary's STABLE schema-too-new marker (Rust
  // domain::SCHEMA_TOO_NEW_MARKER) \u2014 not reword-able prose. Fallback to the legacy
  // phrase so a cached binary predating the marker still reads as "updating".
  const errStr = errText || '';
  const binaryOutdated = errStr.includes('code-graph:schema-too-new') || /schema version/i.test(errStr);
  return (binaryOutdated || updatePending()) ? 'code-graph: \u21bb updating' : 'code-graph: offline';
}

let report = null;
let errText = '';
try {
  report = parseReport(execFileSync(bin, ['health-check', '--format', 'json'], {
    timeout: 3000,
    stdio: ['pipe', 'pipe', 'pipe'],
    // Run the binary FROM the resolved root so its own project-root resolution
    // lands on the same DB the gate above picked (a subdir cwd would otherwise
    // re-resolve to a stray nested index inside the binary).
    cwd: root
  }).toString());
} catch (e) {
  // health-check exits NON-ZERO on an unhealthy/empty index but still writes the
  // full JSON report to stdout. The binary ran fine \u2014 recover the report from the
  // error object so an empty index shows "\u2717 0 nodes" rather than a bogus "offline".
  report = parseReport(((e && e.stdout) || '').toString());
  // Scan BOTH streams for the schema marker: the binary writes it to stderr, but
  // an empty stderr Buffer is truthy, so `stderr || stdout` would never fall
  // through — concatenate instead of short-circuiting.
  errText = [(e && e.stderr) || '', (e && e.stdout) || ''].map(String).join('\n');
}

process.stdout.write(report ? renderHealth(report) : statusUnavailable(errText));
