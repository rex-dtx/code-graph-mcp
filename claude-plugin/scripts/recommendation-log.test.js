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
