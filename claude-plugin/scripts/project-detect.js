#!/usr/bin/env node
'use strict';
// Shared "is this a real project?" detector for the plugin's activation gates.
//
// Why this exists: code-graph half-activates in non-project working
// directories — most visibly the ~2035 headless `claude -p` calls
// claude-mem-lite spawns with cwd=/tmp ("Return ONLY valid JSON"), none of
// which ever use code-graph. Each one paid an MCP-server spin-up + a ~780B
// `instructions` block + a SessionStart map probe + an empty
// /tmp/.code-graph/index.db, plus adopt() writing a decision-table sentinel
// into ~/.claude/projects/-tmp/memory/MEMORY.md. This module is the single
// gate the launcher (mcp-launcher.js), the SessionStart hook (session-init.js),
// and adopt (adopt.js) consult to fully no-op there.
//
// Detection is project-MARKER based, NOT a literal "is cwd under os.tmpdir()"
// check. Rationale: (1) /tmp and Claude Code's $TMPDIR have no .git/manifest,
// so the marker check already classifies every temp / headless cwd as
// non-project; (2) a literal under-tmpdir test would wrongly skip a real git
// repo that happens to be cloned under /tmp AND would break this repo's own
// tmpdir-based test sandboxes. Markers mirror what Claude Code itself
// recognizes. `.code-graph` is deliberately NOT a marker — it is created BY
// this tool, so counting it would let a once-polluted /tmp self-certify as a
// project on the next session (circular).
const fs = require('fs');
const os = require('os');
const path = require('path');

const PROJECT_MARKERS = [
  '.git', 'package.json', 'Cargo.toml',
  'pyproject.toml', 'go.mod', 'pom.xml', 'build.gradle',
];

function isProjectRoot(cwd) {
  return PROJECT_MARKERS.some(m => fs.existsSync(path.join(cwd, m)));
}

// Walk up from `cwd` to the nearest ancestor carrying a project marker, bounded
// by $HOME (exclusive) and the filesystem root. Returns the marker dir, or null
// if none. Mirrors the Rust binary's `resolve_project_root_bounded` so the JS
// activation gate and the binary AGREE: before this, the gate checked ONLY the
// literal cwd, so launching from a marker-less monorepo SUBDIR (e.g.
// `repo/backend/` with `.git` only at `repo/`) classified it non-project and
// served the 0-tool stub — even though the binary would have resolved `repo/`
// and answered queries. The $HOME-exclusive bound keeps every /tmp / headless
// cwd non-project (a stray marker in $HOME must not certify them).
function findProjectRoot(cwd = process.cwd()) {
  const home = os.homedir();
  let dir = path.resolve(cwd);
  for (;;) {
    if (dir === home) return null;        // $HOME-exclusive: don't certify $HOME or above
    if (isProjectRoot(dir)) return dir;
    const parent = path.dirname(dir);
    if (parent === dir) return null;      // filesystem root
    dir = parent;
  }
}

// A cwd is "non-project" when neither it NOR any ancestor (up to $HOME) carries a
// recognized project marker. The plugin's activation gates short-circuit there:
// no MCP tools, no index creation, no SessionStart map injection, no auto-adoption.
function isNonProjectCwd(cwd = process.cwd()) {
  return findProjectRoot(cwd) === null;
}

module.exports = { PROJECT_MARKERS, isProjectRoot, findProjectRoot, isNonProjectCwd };
