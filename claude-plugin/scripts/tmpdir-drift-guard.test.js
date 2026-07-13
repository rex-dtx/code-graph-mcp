#!/usr/bin/env node
'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');

// META③: under Claude Code, $TMPDIR is redirected to ~/.claude/tmp/ and writing
// there via a bare os.tmpdir() leaks/loops (memory: feedback_tmpdir_override_trap).
// All plugin scripts must route through cgTmpDir(). Only the file that DEFINES
// cgTmpDir (tmp-dir.js) may call os.tmpdir() directly.
//
// *.test.js files are excluded: their own os.tmpdir()-seeded mkdtempSync() fixture
// dirs are unique, self-cleaning, and not the shared-state leak this guard targets.
//
// One pre-existing, documented exception in production code: lifecycle.js's
// verifyHooksFire() builds a throwaway, self-cleaning mkdtempSync fixture directly
// under os.tmpdir() (NOT cgTmpDir()) on purpose — a concurrent process clearing
// <tmp>/code-graph-mcp mid-run would otherwise yank the fixture out from under an
// in-flight spawn (see the comment at that call site). Allowlisted by exact line
// content, not by file, so any OTHER new bare os.tmpdir() call added later in the
// same file still fails the guard.
const DEFINER = 'tmp-dir.js';
const ALLOWLIST = [{ file: 'lifecycle.js', contains: 'tmpBase || os.tmpdir()' }];

function listJsFiles(dir) {
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...listJsFiles(full));
    } else if (entry.isFile() && entry.name.endsWith('.js') && !entry.name.endsWith('.test.js')) {
      out.push(full);
    }
  }
  return out;
}

test('no bare os.tmpdir() outside the cgTmpDir helper', () => {
  const root = __dirname;
  const offenders = [];
  for (const full of listJsFiles(root)) {
    const name = path.basename(full);
    if (name === DEFINER) continue;
    const src = fs.readFileSync(full, 'utf8');
    src.split('\n').forEach((line, i) => {
      const code = line.replace(/\/\/.*$/, '');
      if (!/\bos\.tmpdir\s*\(/.test(code)) return;
      const allowed = ALLOWLIST.some((a) => a.file === name && line.includes(a.contains));
      if (!allowed) offenders.push(`${path.relative(root, full)}:${i + 1}: ${line.trim()}`);
    });
  }
  assert.deepStrictEqual(
    offenders,
    [],
    `bare os.tmpdir() found outside cgTmpDir(); use cgTmpDir() from tmp-dir.js instead:\n${offenders.join('\n')}`
  );
});
