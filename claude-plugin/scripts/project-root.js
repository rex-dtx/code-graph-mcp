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
// $HOME and at `.git` boundaries (neither is crossed, and home's own index is
// never adopted from below).

const fs = require('fs');
const os = require('os');
const path = require('path');

// A linked git worktree's `.git` is a FILE containing
// `gitdir: <main>/.git/worktrees/<name>` — resolve it to the main checkout so
// worktree sessions (Claude Code's EnterWorktree puts them under
// <main>/.claude/worktrees/<slug>) reuse the main index instead of going dark.
// A submodule's `.git` file points at `.git/modules/…` and stays a hard
// boundary: its content is a DIFFERENT codebase, not a branch copy of the
// parent. Returns null for a regular `.git` directory (EISDIR), a missing
// `.git` (ENOENT), or any gitdir not under `.git/worktrees/`.
function worktreeMainRoot(dir) {
  let raw;
  try { raw = fs.readFileSync(path.join(dir, '.git'), 'utf8'); }
  catch { return null; }
  const m = /^gitdir:\s*(.+?)\s*$/m.exec(raw);
  if (!m) return null;
  const gitdir = path.resolve(dir, m[1]);
  const marker = `${path.sep}.git${path.sep}worktrees${path.sep}`;
  const at = gitdir.lastIndexOf(marker);
  if (at < 0) return null;
  return gitdir.slice(0, at) || null;
}

// Resolves to the project's CANONICAL index dir, skipping STRAY nested indexes.
// A monorepo subdir (`daagu/backend`, `daagu/frontend`) can carry its own
// `.code-graph/index.db` — a relic an older binary created — nested under the
// real root's index. Returning the nearest such index made every consumer
// (statusline gate, hooks) read a different DB per cwd (statusline "oscillation",
// `✗ 0 nodes` in an empty subdir index). Mirror the Rust resolver: the start's
// own index wins only if it is NOT a stray nested index (no indexed ancestor) OR
// start is itself a project boundary (`.git`, i.e. a real submodule). Otherwise
// prefer the project root: the nearest indexed `.git` root, else the nearest
// indexed ancestor INSIDE the boundary (git root if any, home otherwise — the
// walk stops at both, so a nested repo never adopts the outer project's index
// and a stray `~/.code-graph` never leaks into un-indexed dirs under home).
// `null` when nothing on start→…→boundary is indexed.
//
// Rust parity note (cli::resolve_project_root_from): the Rust resolver is the
// WRITE side — in a worktree it returns the worktree itself and builds a local
// index there. This reader prefers such a local index when present (own-index
// rule), falling back to the main checkout's index only while the worktree has
// none. Divergence is intentional; keep it documented on both sides.
function resolveProjectRoot(startDir, opts = {}) {
  const home = opts.home !== undefined ? opts.home : os.homedir();
  const exists = opts.exists || fs.existsSync;
  const hasIndex = (d) => exists(path.join(d, '.code-graph', 'index.db'));
  const hasGit = (d) => exists(path.join(d, '.git'));
  const start = path.resolve(startDir || '.');

  // start's own `.git` is a hard project boundary (a real submodule / distinct
  // repo): use its index if present. With none, a linked WORKTREE falls back to
  // its main checkout's index (a worktree is a branch copy of that codebase);
  // anything else → `null` — never escape to an ancestor's index. Mirrors the
  // Rust resolver's rule 1 (which returns cwd even without an index because it
  // CREATES one; the JS reader has nothing to read → null).
  if (hasGit(start)) {
    if (hasIndex(start)) return start;
    const main = worktreeMainRoot(start);
    return main && hasIndex(main) ? main : null;
  }

  // start IS home: own-index rule only (deliberately indexed home dirs keep
  // working from home itself), and never scan ancestors ABOVE home — without
  // this, the strict-ancestor walk below starts past the home bound entirely.
  if (start === home) return hasIndex(start) ? start : null;

  // Detect whether `start` is a STRAY nested index: walk STRICT ancestors up to
  // the nearest `.git` root (project boundary), bounded at home. An indexed
  // ancestor within that boundary means start's own index is a monorepo-subdir
  // relic. Stop AT the git root — an index above it (e.g. `~/.code-graph`) is an
  // unrelated outer project and must not poison this one.
  let gitRoot = null;
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
    if (hasGit(dir)) { gitRoot = dir; break; }
  }

  // start's own index wins unless it is stray (an indexed ancestor within the
  // git boundary). start's own `.git` was already handled above.
  if (hasIndex(start) && !ancestorIndexed) return start;
  if (gitRoot && hasIndex(gitRoot)) return gitRoot;

  // Nearest indexed ancestor strictly INSIDE the boundary (git root / home are
  // stops, never candidates — see header). Covers the legit shape where only a
  // sub-project of an unindexed repo was indexed (repo/packages/foo).
  let d = hasIndex(start) ? path.dirname(start) : start;
  for (;;) {
    if (d === home || d === gitRoot) break;
    if (hasIndex(d)) return d;
    const parent = path.dirname(d);
    if (parent === d) break;
    d = parent;
  }

  // Nothing indexed inside the boundary. If that boundary is a linked worktree
  // root, subdirs resolve like the root itself does: to the main checkout.
  if (gitRoot) {
    const main = worktreeMainRoot(gitRoot);
    if (main && hasIndex(main)) return main;
  }
  return null;
}

module.exports = { resolveProjectRoot };
