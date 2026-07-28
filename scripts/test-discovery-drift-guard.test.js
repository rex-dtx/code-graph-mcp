#!/usr/bin/env node
'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

// Both JS-test gates — CI (.github/workflows/ci.yml) and the local pre-commit
// hook — used a NON-RECURSIVE `*.test.js` shell glob. Every test file happens to
// sit at depth 1 today, so nothing was missed; the first one anyone puts in a
// subdirectory would have been skipped silently. That is the same failure mode
// (M10) as the hand-curated allowlist those globs were introduced to replace,
// re-entering through the replacement.
//
// This guard is BEHAVIORAL: it runs the gates' real discovery commands against a
// fixture tree containing a nested test file and asserts the file is found. A
// textual "does the source say find" assertion would pass for a command that
// finds nothing.

const ROOT = path.resolve(__dirname, '..');
const CI_YML = path.join(ROOT, '.github/workflows/ci.yml');
const PRE_COMMIT = path.join(ROOT, 'scripts/pre-commit.sh');

/** Build a throwaway repo shaped like this one, with one test file nested. */
function fixture() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-discovery-'));
  fs.mkdirSync(path.join(dir, 'claude-plugin/scripts/lib'), { recursive: true });
  fs.mkdirSync(path.join(dir, 'scripts'), { recursive: true });
  fs.writeFileSync(path.join(dir, 'claude-plugin/scripts/top.test.js'), '');
  fs.writeFileSync(path.join(dir, 'claude-plugin/scripts/lib/nested.test.js'), '');
  fs.writeFileSync(path.join(dir, 'scripts/root.test.js'), '');
  return dir;
}

/** The `files=$(...)` discovery command from the CI job, verbatim. */
function ciDiscoveryCommand() {
  const yml = fs.readFileSync(CI_YML, 'utf8');
  const start = yml.indexOf('files=$(');
  assert.notStrictEqual(start, -1, 'CI JS-test discovery assignment not found in ci.yml');
  const end = yml.indexOf(')', yml.indexOf('| sort', start)) + 1;
  assert.ok(end > start, 'could not delimit the CI discovery command');
  // Strip the YAML block indentation and the shell line continuations.
  return yml
    .slice(start, end)
    .split('\n')
    .map((l) => l.trim())
    .join(' ')
    .replace(/\\ /g, '');
}

/**
 * The `find ...` feeding pre-commit's JS-test loop, run VERBATIM — `$ROOT` is
 * bound in the shell rather than substituted into the string, so the guard
 * exercises the real source text and cannot be fooled by a rewrite of it.
 */
function preCommitDiscoveryCommand(root) {
  const sh = fs.readFileSync(PRE_COMMIT, 'utf8');
  const m = sh.match(/done < <\((find [^\n]*?)\)\n/);
  assert.ok(m, 'pre-commit JS-test loop is not fed by a `done < <(find ...)` redirect');
  return `ROOT=${JSON.stringify(root)}\n${m[1]}`;
}

function runIn(dir, command) {
  return execFileSync('bash', ['-c', command], { cwd: dir, encoding: 'utf8' });
}

test('CI JS-test discovery reaches nested test files', () => {
  const dir = fixture();
  try {
    const out = runIn(dir, `${ciDiscoveryCommand()}\nprintf '%s\\n' "$files"`);
    assert.match(out, /lib\/nested\.test\.js/, `CI discovery missed the nested file:\n${out}`);
    assert.match(out, /top\.test\.js/, `CI discovery missed the top-level file:\n${out}`);
    assert.match(out, /root\.test\.js/, `CI discovery missed scripts/:\n${out}`);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('pre-commit JS-test discovery reaches nested test files', () => {
  const dir = fixture();
  try {
    const out = runIn(dir, preCommitDiscoveryCommand(dir));
    assert.match(out, /lib\/nested\.test\.js/, `pre-commit discovery missed nested:\n${out}`);
    assert.match(out, /top\.test\.js/, `pre-commit discovery missed top-level:\n${out}`);
    assert.match(out, /root\.test\.js/, `pre-commit discovery missed scripts/:\n${out}`);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

// Permanent negative control: the assertions above must actually be able to
// fail. Run the OLD non-recursive glob against the same fixture and confirm it
// finds the depth-1 files but not the nested one — i.e. that the fixture really
// distinguishes the two mechanisms, and a guard that merely ran *some* command
// would not pass by accident.
test('the fixture distinguishes recursive discovery from the old glob', () => {
  const dir = fixture();
  try {
    const out = runIn(dir, 'ls claude-plugin/scripts/*.test.js scripts/*.test.js');
    assert.match(out, /top\.test\.js/, 'control: the old glob should still find depth-1 files');
    assert.doesNotMatch(
      out,
      /nested\.test\.js/,
      'control failed: the old non-recursive glob found a nested file, so this guard proves nothing'
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
