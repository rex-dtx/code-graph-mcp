'use strict';
// Tests for project-detect.js — the activation gate shared by mcp-launcher.js,
// session-init.js, and adopt.js. Run: node --test claude-plugin/scripts/project-detect.test.js
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const path = require('path');
const os = require('os');

const { PROJECT_MARKERS, isProjectRoot, isNonProjectCwd } = require('./project-detect');

function mkTmp(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-pd-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test('isNonProjectCwd: bare tmp dir (no markers) → non-project', (t) => {
  const dir = mkTmp(t);
  assert.equal(isNonProjectCwd(dir), true);
});

test('isNonProjectCwd: /tmp root (the mem-lite headless cwd) → non-project', () => {
  // claude-mem-lite spawns `claude -p` with cwd=/tmp; /tmp has no project marker.
  assert.equal(isNonProjectCwd('/tmp'), true);
});

test('isNonProjectCwd: cwd with .git → project (false)', (t) => {
  const dir = mkTmp(t);
  fs.mkdirSync(path.join(dir, '.git'));
  assert.equal(isNonProjectCwd(dir), false);
});

test('isNonProjectCwd: cwd with package.json → project (false)', (t) => {
  const dir = mkTmp(t);
  fs.writeFileSync(path.join(dir, 'package.json'), '{}');
  assert.equal(isNonProjectCwd(dir), false);
});

test('isNonProjectCwd: a real git repo under /tmp is still a project (marker wins over location)', (t) => {
  // Deliberate: we do NOT do a literal under-tmpdir check, so a repo cloned
  // into /tmp/<x> with .git is correctly treated as a project.
  const dir = mkTmp(t);
  fs.mkdirSync(path.join(dir, '.git'));
  assert.equal(isNonProjectCwd(dir), false);
});

test('isNonProjectCwd: cwd with only .code-graph → non-project (self-created dir is not a marker)', (t) => {
  // Circularity guard: once code-graph (pre-fix) created /tmp/.code-graph, a
  // naive marker set counting .code-graph would self-certify /tmp as a project.
  const dir = mkTmp(t);
  fs.mkdirSync(path.join(dir, '.code-graph'));
  assert.equal(isProjectRoot(dir), false, '.code-graph alone must not qualify as a project');
  assert.equal(isNonProjectCwd(dir), true);
});

test('PROJECT_MARKERS excludes .code-graph and includes the standard anchors', () => {
  assert.ok(!PROJECT_MARKERS.includes('.code-graph'), '.code-graph must not be a project marker');
  for (const m of ['.git', 'package.json', 'Cargo.toml', 'pyproject.toml', 'go.mod']) {
    assert.ok(PROJECT_MARKERS.includes(m), `${m} should be a marker`);
  }
});

test('isProjectRoot detects each marker', (t) => {
  for (const marker of PROJECT_MARKERS) {
    const dir = mkTmp(t);
    assert.equal(isProjectRoot(dir), false, 'bare cwd should not be a project');
    const markerPath = path.join(dir, marker);
    if (marker.startsWith('.')) fs.mkdirSync(markerPath);
    else fs.writeFileSync(markerPath, '');
    assert.equal(isProjectRoot(dir), true, `${marker} should make cwd a project`);
  }
});
