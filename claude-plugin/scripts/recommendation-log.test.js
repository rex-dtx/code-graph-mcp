'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { recordRecommendation, REC_FILE } = require('./recommendation-log');

function tmpProject(t, withCodeGraph) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-rec-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  if (withCodeGraph) fs.mkdirSync(path.join(dir, '.code-graph'));
  return dir;
}

test('recordRecommendation appends a JSON line with ts + fields', (t) => {
  const cwd = tmpProject(t, true);
  assert.equal(recordRecommendation(cwd, { hook: 'grep', action: 'deny' }), true);
  const content = fs.readFileSync(path.join(cwd, '.code-graph', REC_FILE), 'utf8');
  const lines = content.trim().split('\n');
  assert.equal(lines.length, 1);
  const rec = JSON.parse(lines[0]);
  assert.equal(rec.hook, 'grep');
  assert.equal(rec.action, 'deny');
  assert.ok(typeof rec.ts === 'string' && rec.ts.length > 0, 'ts should be a timestamp');
});

test('recordRecommendation is a no-op (no dir created) when .code-graph absent', (t) => {
  const cwd = tmpProject(t, false);
  assert.equal(recordRecommendation(cwd, { hook: 'grep', action: 'hint' }), false);
  // Must NOT create the dir or file — zero footprint in non-project cwd.
  assert.equal(fs.existsSync(path.join(cwd, '.code-graph')), false);
});

test('recordRecommendation appends across calls (one line each)', (t) => {
  const cwd = tmpProject(t, true);
  recordRecommendation(cwd, { hook: 'grep', action: 'hint' });
  recordRecommendation(cwd, { hook: 'read', action: 'hint' });
  recordRecommendation(cwd, { hook: 'grep', action: 'deny' });
  const lines = fs.readFileSync(path.join(cwd, '.code-graph', REC_FILE), 'utf8').trim().split('\n');
  assert.equal(lines.length, 3);
  const hooks = lines.map((l) => JSON.parse(l).hook);
  assert.deepEqual(hooks, ['grep', 'read', 'grep']);
});

test('recordRecommendation rotates the file when it exceeds the size cap', (t) => {
  const cwd = tmpProject(t, true);
  const file = path.join(cwd, '.code-graph', REC_FILE);
  // Pre-fill > 1MB of prior events.
  const filler = 'y'.repeat(1024);
  let blob = '';
  for (let i = 0; i < 1200; i++) blob += `{"old":${i},"pad":"${filler}"}\n`;
  fs.writeFileSync(file, blob);
  assert.ok(fs.statSync(file).size > 1048576, 'precondition: file over 1MB');

  // One more recorded event must trigger rotation (rotate-before-append).
  assert.equal(recordRecommendation(cwd, { hook: 'grep', action: 'deny' }), true);

  const size = fs.statSync(file).size;
  assert.ok(size < 600000, `rotated file should be well under 1MB, got ${size}`);
  const lines = fs.readFileSync(file, 'utf8').trim().split('\n');
  // The just-recorded line is last and intact; the first surviving line is whole JSON.
  const last = JSON.parse(lines[lines.length - 1]);
  assert.equal(last.action, 'deny');
  assert.doesNotThrow(() => JSON.parse(lines[0]), 'first surviving line must be a whole JSON line');
});
