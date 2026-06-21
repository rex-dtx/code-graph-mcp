'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { runGrepAnswer, runShowAnswer, runOverviewAnswer, truncateAtLine } = require('./cg-answer');

// Stub "binary": a node script that reacts to its first real arg so one stub
// covers hits / no-hits / error / timeout cases.
let stubDir;
let stubPath;

test.before(() => {
  stubDir = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-answer-test-'));
  stubPath = path.join(stubDir, 'cg-stub.js');
  fs.writeFileSync(stubPath, `#!/usr/bin/env node
'use strict';
const pattern = process.argv[3] || '';
if (pattern === 'HangForever') { setTimeout(() => {}, 60000); }
else if (pattern === 'ExplodePlease') { process.exit(3); }
else if (pattern === 'NothingHere') {
  process.stdout.write('[code-graph] No matches for: NothingHere\\n');
} else if (pattern === 'NothingHereExit1') {
  // v0.50 grep-parity binary: no match → empty stdout + exit 1
  process.exit(1);
} else {
  process.stdout.write(
    'src/storage/db.rs:42  fn ' + pattern + '() {\\n' +
    '  -> fn ' + pattern + ' (lines 42-60)\\n' +
    'args=' + JSON.stringify(process.argv.slice(2)) + '\\n');
}
`);
});

test.after(() => {
  fs.rmSync(stubDir, { recursive: true, force: true });
});

// Wrap the stub so spawnSync can exec it directly: binary = node, leading arg
// trick is not possible (runGrepAnswer controls args), so expose via a shim
// shell-free approach: point binary at node and prepend the script through
// _CG_ANSWER_BINARY handling is binary-only. Instead make the stub itself
// executable with a node shebang and rely on exec.
function stubBinary() {
  fs.chmodSync(stubPath, 0o755);
  return stubPath;
}

test('runGrepAnswer: hits → status hits with stdout text', () => {
  const r = runGrepAnswer({ cwd: stubDir, pattern: 'fts5_search', binary: stubBinary() });
  assert.equal(r.status, 'hits');
  assert.match(r.text, /fn fts5_search/);
});

test('runGrepAnswer: passes grep subcommand, pattern and path as argv', () => {
  const r = runGrepAnswer({
    cwd: stubDir, pattern: 'fts5_search', searchPath: 'src/storage/', binary: stubBinary(),
  });
  assert.equal(r.status, 'hits');
  assert.match(r.text, /args=\["grep","fts5_search","src\/storage\/"\]/);
});

test('runGrepAnswer: child env carries CODE_GRAPH_INTERNAL=1 (not a funnel conversion)', () => {
  // Stub variant that echoes the marker back in its output.
  const envStub = path.join(stubDir, 'cg-env-stub.js');
  fs.writeFileSync(envStub, `#!/usr/bin/env node
process.stdout.write('internal=' + (process.env.CODE_GRAPH_INTERNAL || '') + '\\n');
`);
  fs.chmodSync(envStub, 0o755);
  const r = runGrepAnswer({ cwd: stubDir, pattern: 'whatever', binary: envStub });
  assert.equal(r.status, 'hits');
  assert.match(r.text, /internal=1/,
    'hook-internal CLI runs must be marked so record_cli_use skips them');
});

test('runGrepAnswer: omits path argv when no searchPath', () => {
  const r = runGrepAnswer({ cwd: stubDir, pattern: 'fts5_search', binary: stubBinary() });
  assert.match(r.text, /args=\["grep","fts5_search"\]/);
});

test('runGrepAnswer: CLI "[code-graph] No matches" → status no-hits', () => {
  const r = runGrepAnswer({ cwd: stubDir, pattern: 'NothingHere', binary: stubBinary() });
  assert.equal(r.status, 'no-hits');
});

test('runGrepAnswer: exit 1 (v0.50 grep-parity no-match) → status no-hits', () => {
  const r = runGrepAnswer({ cwd: stubDir, pattern: 'NothingHereExit1', binary: stubBinary() });
  assert.equal(r.status, 'no-hits',
    'grep-parity exit 1 means no match, not a failed binary');
});

test('runGrepAnswer: exit >1 → unavailable', () => {
  const r = runGrepAnswer({ cwd: stubDir, pattern: 'ExplodePlease', binary: stubBinary() });
  assert.equal(r.status, 'unavailable');
});

test('runGrepAnswer: missing binary → no-binary (distinct from runtime unavailable)', () => {
  const r = runGrepAnswer({ cwd: stubDir, pattern: 'fts5_search', binary: null });
  assert.equal(r.status, 'no-binary',
    'a null binary is the flagship-dark case and must be distinguishable from a runtime failure');
});

test('runGrepAnswer: nonexistent binary path → unavailable (spawn failure, not no-binary)', () => {
  // A non-null path that fails to spawn is a runtime failure, NOT a missing
  // binary — `no-binary` is reserved for findBinary() returning falsy.
  const r = runGrepAnswer({
    cwd: stubDir, pattern: 'fts5_search', binary: path.join(stubDir, 'nope-bin'),
  });
  assert.equal(r.status, 'unavailable');
});

test('runGrepAnswer: timeout → unavailable', () => {
  const r = runGrepAnswer({
    cwd: stubDir, pattern: 'HangForever', binary: stubBinary(), timeoutMs: 300,
  });
  assert.equal(r.status, 'unavailable');
});

test('runGrepAnswer: empty pattern → unavailable (never spawns)', () => {
  const r = runGrepAnswer({ cwd: stubDir, pattern: '', binary: stubBinary() });
  assert.equal(r.status, 'unavailable');
});

test('runGrepAnswer: oversized pattern (>200ch) → unavailable (never spawns)', () => {
  const r = runGrepAnswer({ cwd: stubDir, pattern: 'A'.repeat(201), binary: stubBinary() });
  assert.equal(r.status, 'unavailable');
});

test('runGrepAnswer: long output is truncated with marker', () => {
  // Stub echoes args= line; force truncation via tiny maxBytes
  const r = runGrepAnswer({
    cwd: stubDir, pattern: 'fts5_search', binary: stubBinary(), maxBytes: 30,
  });
  assert.equal(r.status, 'hits');
  assert.equal(r.truncated, true);
  assert.ok(Buffer.byteLength(r.text, 'utf8') <= 30);
});

// ── truncateAtLine (pure) ───────────────────────────────────────────

test('truncateAtLine: under limit → unchanged, not truncated', () => {
  const { text, truncated } = truncateAtLine('a\nb\nc', 100);
  assert.equal(text, 'a\nb\nc');
  assert.equal(truncated, false);
});

test('truncateAtLine: cuts at a line boundary', () => {
  const input = 'line-one\nline-two\nline-three\n';
  const { text, truncated } = truncateAtLine(input, 20);
  assert.equal(truncated, true);
  // 20-byte budget fits 'line-one\nline-two' (17B); the half-cut 'li' is dropped
  assert.equal(text, 'line-one\nline-two');
});

test('truncateAtLine: single oversized line → hard cut', () => {
  const { text, truncated } = truncateAtLine('x'.repeat(50), 10);
  assert.equal(truncated, true);
  assert.equal(Buffer.byteLength(text, 'utf8'), 10);
});

// ── v0.48 sanitizeSearchPath: glob args reach rg literally (no shell) ──

test('sanitizeSearchPath: truncates at first glob segment (daagu denied command)', () => {
  const { sanitizeSearchPath } = require('./cg-answer');
  assert.equal(
    sanitizeSearchPath('backend/app/services/llm_engine/*.py'),
    'backend/app/services/llm_engine');
});

test('sanitizeSearchPath: clean path unchanged; leading glob drops scope; falsy → undefined', () => {
  const { sanitizeSearchPath } = require('./cg-answer');
  assert.equal(sanitizeSearchPath('src/storage/'), 'src/storage/');
  assert.equal(sanitizeSearchPath('*.py'), undefined);
  assert.equal(sanitizeSearchPath('src/**/x.rs'), 'src');
  assert.equal(sanitizeSearchPath('src/file[1].rs'), 'src');
  assert.equal(sanitizeSearchPath(''), undefined);
  assert.equal(sanitizeSearchPath(undefined), undefined);
});

test('runGrepAnswer: glob searchPath is truncated before spawn (defensive layer)', () => {
  const r = runGrepAnswer({
    cwd: stubDir, pattern: 'fts5_search', searchPath: 'src/storage/*.rs', binary: stubBinary(),
  });
  assert.equal(r.status, 'hits');
  assert.match(r.text, /args=\["grep","fts5_search","src\/storage"\]/);
});

// ── runShowAnswer (v0.49) — show-mode deny bodies ────────────────────

test('runShowAnswer: concatenates per-symbol show output with $ headers', () => {
  const r = runShowAnswer({ cwd: stubDir, symbols: ['alpha_one', 'beta_two'], binary: stubBinary() });
  assert.equal(r.status, 'hits');
  assert.match(r.text, /\$ code-graph-mcp show alpha_one/);
  assert.match(r.text, /\$ code-graph-mcp show beta_two/);
});

test('runShowAnswer: skips non-identifier symbols, all-skipped → unavailable-safe no-hits', () => {
  const r = runShowAnswer({ cwd: stubDir, symbols: ['$(rm -rf)', 'a|b'], binary: stubBinary() });
  assert.equal(r.status, 'no-hits');
});

test('runShowAnswer: caps at 3 symbols', () => {
  const r = runShowAnswer({
    cwd: stubDir, symbols: ['s_one', 's_two', 's_three', 's_four'], binary: stubBinary(),
  });
  assert.equal(r.status, 'hits');
  assert.doesNotMatch(r.text, /show s_four/);
});

test('runShowAnswer: empty symbol list → unavailable', () => {
  assert.equal(runShowAnswer({ cwd: stubDir, symbols: [], binary: stubBinary() }).status, 'unavailable');
});

test('runShowAnswer: failing binary → no-hits (caller falls back to grep answer)', () => {
  const r = runShowAnswer({ cwd: stubDir, symbols: ['ExplodePlease'], binary: stubBinary() });
  assert.equal(r.status, 'no-hits');
});

test('runShowAnswer: missing binary → no-binary (distinct from runtime no-hits/unavailable)', () => {
  const r = runShowAnswer({ cwd: stubDir, symbols: ['alpha_one'], binary: null });
  assert.equal(r.status, 'no-binary');
});

// ── runOverviewAnswer (v0.49) — read-fanout delivered module map ──────

test('runOverviewAnswer: hits → status hits with stdout text', () => {
  const r = runOverviewAnswer({ cwd: stubDir, dir: 'src/storage', binary: stubBinary() });
  assert.equal(r.status, 'hits');
  assert.match(r.text, /args=\["overview","src\/storage"\]/);
});

test('runOverviewAnswer: CLI "No matches" → no-hits', () => {
  const r = runOverviewAnswer({ cwd: stubDir, dir: 'NothingHere', binary: stubBinary() });
  assert.equal(r.status, 'no-hits');
});

test('runOverviewAnswer: failing binary → unavailable', () => {
  const r = runOverviewAnswer({ cwd: stubDir, dir: 'ExplodePlease', binary: stubBinary() });
  assert.equal(r.status, 'unavailable');
});

test('runOverviewAnswer: missing binary → no-binary (distinct from runtime unavailable)', () => {
  const r = runOverviewAnswer({ cwd: stubDir, dir: 'src/storage', binary: null });
  assert.equal(r.status, 'no-binary');
});

test('runOverviewAnswer: empty/oversized dir → unavailable (never spawns)', () => {
  assert.equal(runOverviewAnswer({ cwd: stubDir, dir: '', binary: stubBinary() }).status, 'unavailable');
  assert.equal(
    runOverviewAnswer({ cwd: stubDir, dir: 'a'.repeat(301), binary: stubBinary() }).status,
    'unavailable');
});
