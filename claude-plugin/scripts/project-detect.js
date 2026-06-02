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
const path = require('path');

const PROJECT_MARKERS = [
  '.git', 'package.json', 'Cargo.toml',
  'pyproject.toml', 'go.mod', 'pom.xml', 'build.gradle',
];

function isProjectRoot(cwd) {
  return PROJECT_MARKERS.some(m => fs.existsSync(path.join(cwd, m)));
}

// A cwd is "non-project" when it carries none of the recognized project
// markers. The plugin's activation gates short-circuit there: no MCP tools,
// no index creation, no SessionStart map injection, no auto-adoption.
function isNonProjectCwd(cwd = process.cwd()) {
  return !isProjectRoot(cwd);
}

module.exports = { PROJECT_MARKERS, isProjectRoot, isNonProjectCwd };
