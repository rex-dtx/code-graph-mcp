'use strict';
// Shared project-root resolution for the PreToolUse hooks (v0.49 — extracted
// from pre-grep-guide so pre-read-guide gets the same subdir-cwd fix without a
// circular require).
//
// The hook's process.cwd() follows the PERSISTENT shell, not the project root:
// after the model runs `cd backend/`, every per-cwd gate (index.db existence,
// relative-path matching) fails silently for the rest of the session (daagu
// 2026-06-11: 38/40 head-greps dark; the read hook never recorded AT ALL).
// Walk up to the nearest ancestor holding `.code-graph/index.db`; stop at
// $HOME (checked, not crossed) and fs root.

const fs = require('fs');
const os = require('os');
const path = require('path');

// Resolves to the project's CANONICAL index dir, skipping STRAY nested indexes.
// A monorepo subdir (`daagu/backend`, `daagu/frontend`) can carry its own
// `.code-graph/index.db` — a relic an older binary created — nested under the
// real root's index. Returning the nearest such index made every consumer
// (statusline gate, hooks) read a different DB per cwd (statusline "oscillation",
// `✗ 0 nodes` in an empty subdir index). Mirror the Rust resolver: the start's
// own index wins only if it is NOT a stray nested index (no indexed ancestor) OR
// start is itself a project boundary (`.git`, i.e. a real submodule). Otherwise
// prefer the project root: the nearest indexed `.git` root, else the outermost
// indexed dir on the chain. `null` when nothing on start→…→home is indexed.
function resolveProjectRoot(startDir, opts = {}) {
  const home = opts.home !== undefined ? opts.home : os.homedir();
  const exists = opts.exists || fs.existsSync;
  const hasIndex = (d) => exists(path.join(d, '.code-graph', 'index.db'));
  const hasGit = (d) => exists(path.join(d, '.git'));
  const start = path.resolve(startDir || '.');

  // start's own `.git` is a hard project boundary (a real submodule / distinct
  // repo): use its index if present, else `null` — never escape to an ancestor's
  // index. Mirrors the Rust resolver's rule 1 (which returns cwd even without an
  // index because it CREATES one; the JS reader has nothing to read → null).
  if (hasGit(start)) return hasIndex(start) ? start : null;

  // Detect whether `start` is a STRAY nested index: walk STRICT ancestors up to
  // the nearest `.git` root (project boundary), bounded at home. An indexed
  // ancestor within that boundary means start's own index is a monorepo-subdir
  // relic. Stop AT the git root — an index above it (e.g. `~/.code-graph`) is an
  // unrelated outer project and must not poison this one.
  let gitRootIndexed = null;
  let ancestorIndexed = false;
  let dir = start;
  for (;;) {
    const parent = path.dirname(dir);
    // Stop BEFORE home (and fs root): an index at/above home (e.g. ~/.code-graph
    // from indexing a home dir) is an unrelated outer project, never a parent
    // that makes `start` stray.
    if (parent === dir || parent === home) break;
    dir = parent;
    if (hasIndex(dir)) ancestorIndexed = true;
    if (hasGit(dir)) { if (hasIndex(dir)) gitRootIndexed = dir; break; }
  }

  // start's own index wins unless it is stray (an indexed ancestor within the
  // git boundary). start's own `.git` was already handled above.
  if (hasIndex(start) && !ancestorIndexed) return start;
  if (gitRootIndexed) return gitRootIndexed;
  // Otherwise the nearest indexed ancestor (skipping a stray start), bounded at
  // home; null if nothing on the chain is indexed. Mirrors the original walk.
  let d = hasIndex(start) ? path.dirname(start) : start;
  for (;;) {
    if (hasIndex(d)) return d;
    if (d === home) return null;
    const parent = path.dirname(d);
    if (parent === d) return null;
    d = parent;
  }
}

module.exports = { resolveProjectRoot };
