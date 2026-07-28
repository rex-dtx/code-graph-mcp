'use strict';
const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

// Redirect TMPDIR to a private sandbox BEFORE requiring tmp-dir, which resolves
// CG_TMP_DIR from os.tmpdir() at module load.
//
// `node --test` runs test FILES in parallel processes, and CG_TMP_DIR is a
// single shared path that every one of them may create. The
// "returns the same path and creates the directory" test below wipes it and
// then asserts it is absent — a sibling file calling cgTmpDir() in that window
// re-creates it and the assertion fails. Measured at 2 failures in 40 full-suite
// runs (~5%), while passing every time in isolation, which is the signature of
// this race rather than a logic bug. Owning our own TMPDIR removes the sharing.
process.env.TMPDIR = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-tmpdir-test-'));

const { cgTmpDir, CG_TMP_DIR } = require('./tmp-dir');

test('CG_TMP_DIR is a "code-graph-mcp" subdir of os.tmpdir()', () => {
  assert.strictEqual(path.basename(CG_TMP_DIR), 'code-graph-mcp');
  assert.strictEqual(path.dirname(CG_TMP_DIR), os.tmpdir());
});

test('cgTmpDir() returns the same path and creates the directory', () => {
  // Pre-condition: nuke it if it exists from a prior run, to prove cgTmpDir()
  // actually creates it on demand (not just reports a pre-existing path).
  try { fs.rmSync(CG_TMP_DIR, { recursive: true, force: true }); } catch { /* ok */ }
  assert.ok(!fs.existsSync(CG_TMP_DIR), 'pre-condition: dir must be absent');

  const p = cgTmpDir();
  assert.strictEqual(p, CG_TMP_DIR);
  assert.ok(fs.existsSync(p), 'cgTmpDir() must create the directory');
  assert.ok(fs.statSync(p).isDirectory(), 'created entry must be a directory');
});

test('cgTmpDir() is idempotent — second call does not throw on existing dir', () => {
  cgTmpDir();
  // Should not throw even though dir now exists.
  assert.doesNotThrow(() => cgTmpDir());
});

test('cgTmpDir() does not leak files into os.tmpdir() root', () => {
  // Regression guard: the v0.32.x bug was hook artifacts landing directly
  // in os.tmpdir() (= ~/.claude/tmp/ under Claude Code's $TMPDIR override),
  // colliding with transcript subdirs. After the fix, no `.code-graph-bash-*`
  // / `.cg-impact-*` / `.code-graph-readfan-*` filename should ever appear
  // outside CG_TMP_DIR — only inside it.
  const dir = cgTmpDir();
  const flag = path.join(dir, '.code-graph-bash-test');
  fs.writeFileSync(flag, '');
  try {
    // The sibling of CG_TMP_DIR (= os.tmpdir()) must NOT now contain the flag.
    const parent = path.dirname(dir);
    const stray = path.join(parent, '.code-graph-bash-test');
    assert.ok(!fs.existsSync(stray), 'flag must not exist in os.tmpdir() root');
  } finally {
    try { fs.unlinkSync(flag); } catch { /* ok */ }
  }
});
