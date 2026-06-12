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

function resolveProjectRoot(startDir, opts = {}) {
  const home = opts.home !== undefined ? opts.home : os.homedir();
  const exists = opts.exists || fs.existsSync;
  let dir = path.resolve(startDir || '.');
  for (;;) {
    if (exists(path.join(dir, '.code-graph', 'index.db'))) return dir;
    if (dir === home) return null;
    const parent = path.dirname(dir);
    if (parent === dir) return null;
    dir = parent;
  }
}

module.exports = { resolveProjectRoot };
