'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const { formatCoveringTests, LIST_CAP } = require('./covering-tests');

// ── empty / robust ──────────────────────────────────────

test('covering: empty list → no output', () => {
  assert.equal(formatCoveringTests([], 'src/a.rs'), '');
});

test('covering: missing/undefined → no output (never throws)', () => {
  assert.equal(formatCoveringTests(undefined, 'src/a.rs'), '');
  assert.equal(formatCoveringTests(null, 'src/a.rs'), '');
});

test('covering: entries without a name are dropped', () => {
  const out = formatCoveringTests(
    [{ name: 'test_real', file: 'tests/a.rs' }, { file: 'tests/nameless.rs' }],
    'src/a.rs'
  );
  assert.match(out, /Covering tests \(1\)/); // only the named one counts
  assert.match(out, /test_real/);
});

// ── Rust: a real targeted command ───────────────────────

test('covering: Rust ≤cap lists names + a targeted `cargo test` command', () => {
  const out = formatCoveringTests(
    [
      { name: 'test_alpha', file: 'tests/a.rs' },
      { name: 'test_beta', file: 'src/b.rs' },
    ],
    'src/foo.rs'
  );
  assert.match(out, /Covering tests \(2\)/);
  assert.match(out, /test_alpha \(tests\/a\.rs\)/);
  assert.match(out, /test_beta \(src\/b\.rs\)/);
  // The actionable part: a command that runs exactly the covering tests.
  assert.match(out, /Run after editing: cargo test test_alpha test_beta/);
});

// ── non-Rust: list only, never a fabricated command ─────

test('covering: non-Rust ≤cap lists names but emits NO command (no wrong command)', () => {
  const out = formatCoveringTests(
    [{ name: 'testValidate', file: 'src/auth.test.ts' }],
    'src/auth.ts'
  );
  assert.match(out, /Covering tests \(1\): testValidate \(src\/auth\.test\.ts\)/);
  assert.doesNotMatch(out, /cargo test/);
  assert.doesNotMatch(out, /Run after editing/);
});

// ── high fan-out: collapse, point at the suite ──────────

test('covering: Rust high fan-out (>cap) collapses to a count + suite command, no name list', () => {
  const many = Array.from({ length: LIST_CAP + 1 }, (_, i) => ({
    name: `test_${i}`,
    file: 'tests/wide.rs',
  }));
  const out = formatCoveringTests(many, 'src/hot.rs');
  assert.match(out, new RegExp(`Covering tests: ${LIST_CAP + 1}`));
  assert.match(out, /widely-tested/);
  assert.match(out, /Run the suite after editing: cargo test/);
  // The long per-name list must NOT be inlined when fan-out is high.
  assert.doesNotMatch(out, /test_0 \(/);
});

test('covering: non-Rust high fan-out collapses to a count with no command', () => {
  const many = Array.from({ length: LIST_CAP + 3 }, (_, i) => ({
    name: `test_${i}`,
    file: 'a.test.ts',
  }));
  const out = formatCoveringTests(many, 'src/hot.ts');
  assert.match(out, new RegExp(`Covering tests: ${LIST_CAP + 3}`));
  assert.doesNotMatch(out, /cargo test/);
});
