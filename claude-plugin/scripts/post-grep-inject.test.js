'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const { spawnSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { cgTmpDir } = require('./tmp-dir');

const {
  findFoldableGrepSegment,
  isSilenced,
  isInjectDisabled,
  buildInjectText,
  commandHash,
} = require('./post-grep-inject');

// ── Pure logic: findFoldableGrepSegment ─────────────────────────────
// Reuses splitTopLevelSegments + classifyBlock from pre-grep-guide. The FIRST
// segment whose head is grep AND whose classifyBlock is non-null is the foldable
// grep to answer. Leading-grep foldable commands were DENIED in PreToolUse and
// never ran → never reach PostToolUse, so no dedup is needed here.

test('findFoldableGrepSegment: compound `echo && grep "Sym" tests/` → the grep segment', () => {
  // classifyBlock requires a QUOTED, identifier-like pattern (the deny gate's
  // contract); `EmbeddingModel` stands for the spec's illustrative `Sym`.
  const seg = findFoldableGrepSegment('echo "x" && grep "EmbeddingModel" tests/');
  assert.ok(seg, 'expected a foldable grep segment');
  assert.equal(seg.segment, 'grep "EmbeddingModel" tests/');
  assert.equal(seg.block.mode, 'grep');
});

test('findFoldableGrepSegment: `git diff && grep "Sym" src/` → the grep segment', () => {
  const seg = findFoldableGrepSegment('git diff && grep "EmbeddingModel" src/');
  assert.ok(seg);
  assert.equal(seg.segment, 'grep "EmbeddingModel" src/');
});

test('findFoldableGrepSegment: `cargo test | grep FAIL` is an output filter → null', () => {
  // single pipe is NOT a split → head stays `cargo`, not a foldable grep.
  assert.equal(findFoldableGrepSegment('cargo test | grep FAIL'), null);
});

test('findFoldableGrepSegment: a leading non-compound grep is NOT folded here (PreToolUse denies it)', () => {
  // A bare leading foldable grep is handled by PreToolUse deny; if it somehow
  // reaches PostToolUse it still classifies, but the typical compound case is the
  // target. We DO answer a lone classifyBlock-positive segment when present.
  const seg = findFoldableGrepSegment('grep "EmbeddingModel" src/');
  assert.ok(seg, 'a classifyBlock-positive grep segment is foldable');
  assert.equal(seg.block.mode, 'grep');
});

test('findFoldableGrepSegment: non-foldable hint-tier grep (marker) → null', () => {
  // bare TODO marker passes shouldHint but classifyBlock is null → not foldable.
  assert.equal(findFoldableGrepSegment('echo hi && grep "TODO" src/'), null);
});

test('findFoldableGrepSegment: no grep anywhere → null', () => {
  assert.equal(findFoldableGrepSegment('cargo build && cargo test'), null);
});

test('findFoldableGrepSegment: for-loop body grep is isolated and folded', () => {
  const seg = findFoldableGrepSegment('for s in a b; do grep "EmbeddingModel" src/; done');
  assert.ok(seg, 'loop-body grep must be foldable');
  assert.match(seg.segment, /grep "EmbeddingModel" src\//);
});

test('findFoldableGrepSegment: empty / non-string → null', () => {
  assert.equal(findFoldableGrepSegment(''), null);
  assert.equal(findFoldableGrepSegment(null), null);
});

test('findFoldableGrepSegment: show-mode (decl anchor + context flag) classifies as show', () => {
  const seg = findFoldableGrepSegment('echo go && grep "fn handle_message" -A 5 src/');
  assert.ok(seg);
  assert.equal(seg.block.mode, 'show');
  assert.deepEqual(seg.block.symbols, ['handle_message']);
});

// ── buildInjectText ─────────────────────────────────────────────────

test('buildInjectText: carries a header + the answer text', () => {
  const out = buildInjectText({ text: 'src/foo.rs:7  fn x()', truncated: false }, 'grep');
  assert.match(out, /AST-aware view of your grep/);
  assert.match(out, /src\/foo\.rs:7/);
});

test('buildInjectText: truncation note appended when truncated', () => {
  const out = buildInjectText({ text: 'hit', truncated: true }, 'grep');
  assert.match(out, /truncated/);
});

test('buildInjectText: no truncation note when not truncated', () => {
  const out = buildInjectText({ text: 'hit', truncated: false }, 'grep');
  assert.doesNotMatch(out, /truncated/);
});

// ── opt-out / kill switch ───────────────────────────────────────────

test('isSilenced: CODE_GRAPH_QUIET_HOOKS=1 → silenced; default not', () => {
  assert.equal(isSilenced({ CODE_GRAPH_QUIET_HOOKS: '1' }), true);
  assert.equal(isSilenced({}), false);
});

test('isInjectDisabled: CODE_GRAPH_NO_INJECT=1 → disabled; default not', () => {
  assert.equal(isInjectDisabled({ CODE_GRAPH_NO_INJECT: '1' }), true);
  assert.equal(isInjectDisabled({ CODE_GRAPH_NO_INJECT: '0' }), false);
  assert.equal(isInjectDisabled({}), false);
});

// ── e2e: real spawn with stub binary (mirrors pre-grep-guide harness) ──
// PostToolUse-shaped stdin {tool_input:{command:"..."}}; assert on
// hookSpecificOutput.additionalContext.

function e2eFixture(stubBody) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'post-grep-e2e-'));
  fs.mkdirSync(path.join(dir, '.code-graph'), { recursive: true });
  fs.writeFileSync(path.join(dir, '.code-graph', 'index.db'), '');
  const stub = path.join(dir, 'cg-stub.js');
  fs.writeFileSync(stub, '#!/usr/bin/env node\n' + stubBody);
  fs.chmodSync(stub, 0o755);
  return { dir, stub };
}

function runHook(cmd, fixture, extraEnv = {}, cwdOverride) {
  return spawnSync(process.execPath, [path.join(__dirname, 'post-grep-inject.js')], {
    cwd: cwdOverride || fixture.dir,
    input: JSON.stringify({ tool_input: { command: cmd } }),
    encoding: 'utf8',
    env: {
      ...process.env,
      _CG_ANSWER_BINARY: fixture.stub,
      CODE_GRAPH_QUIET_HOOKS: '0',
      CODE_GRAPH_NO_INJECT: '0',
      ...extraEnv,
    },
  });
}

function cleanupFixture(fixture, cmd) {
  fs.rmSync(fixture.dir, { recursive: true, force: true });
  try {
    fs.unlinkSync(path.join(cgTmpDir(), `.code-graph-postinject-${commandHash(cmd)}`));
  } catch { /* ok */ }
}

test('e2e: `echo "x" && grep Sym tests/` → injects additionalContext with the stub hits + records inject', () => {
  const uniq = `PostHit${Date.now()}`;
  const fixture = e2eFixture(
    `process.stdout.write('tests/foo.rs:7  fn ' + process.argv[3] + '()\\n');`);
  const cmd = `echo "x" && grep "${uniq}" tests/`;
  try {
    const res = runHook(cmd, fixture);
    assert.equal(res.status, 0);
    const out = JSON.parse(res.stdout);
    assert.equal(out.hookSpecificOutput.hookEventName, 'PostToolUse');
    assert.equal(out.hookSpecificOutput.permissionDecision, undefined,
      'PostToolUse inject must be permission-neutral (no permissionDecision)');
    assert.match(out.hookSpecificOutput.additionalContext, new RegExp(uniq));
    assert.match(out.hookSpecificOutput.additionalContext, /tests\/foo\.rs:7/);
    const recs = fs.readFileSync(
      path.join(fixture.dir, '.code-graph', 'recommendations.jsonl'), 'utf8');
    const rec = JSON.parse(recs.trim().split('\n').pop());
    assert.equal(rec.action, 'inject');
    assert.equal(rec.answered, true);
    assert.equal(rec.hook, 'grep');
    assert.equal(rec.pattern, uniq);
  } finally {
    cleanupFixture(fixture, cmd);
  }
});

test('e2e: `git diff && grep Sym src/` → inject', () => {
  const uniq = `GitDiffHit${Date.now()}`;
  const fixture = e2eFixture(
    `process.stdout.write('src/foo.rs:9  fn ' + process.argv[3] + '()\\n');`);
  const cmd = `git diff && grep "${uniq}" src/`;
  try {
    const res = runHook(cmd, fixture);
    assert.equal(res.status, 0);
    const out = JSON.parse(res.stdout);
    assert.match(out.hookSpecificOutput.additionalContext, new RegExp(uniq));
  } finally {
    cleanupFixture(fixture, cmd);
  }
});

test('e2e: `cargo test | grep FAIL` → no inject (output filter)', () => {
  const fixture = e2eFixture(`process.stdout.write('should not run\\n');`);
  const cmd = `cargo test | grep FAIL`;
  try {
    const res = runHook(cmd, fixture);
    assert.equal(res.status, 0);
    assert.equal(res.stdout.trim(), '', 'an output-filter pipe must not inject');
  } finally {
    cleanupFixture(fixture, cmd);
  }
});

test('e2e: stub reports no hits → silent (no inject)', () => {
  const uniq = `PostMiss${Date.now()}`;
  const fixture = e2eFixture(
    `process.stdout.write('[code-graph] No matches\\n');`);
  const cmd = `echo go && grep "${uniq}" src/`;
  try {
    const res = runHook(cmd, fixture);
    assert.equal(res.status, 0);
    assert.equal(res.stdout.trim(), '', 'no-hits must inject nothing');
  } finally {
    cleanupFixture(fixture, cmd);
  }
});

test('e2e: CODE_GRAPH_NO_INJECT=1 silences the hook', () => {
  const uniq = `PostOptout${Date.now()}`;
  const fixture = e2eFixture(`process.stdout.write('src/foo.rs:7  hit\\n');`);
  const cmd = `echo go && grep "${uniq}" src/`;
  try {
    const res = runHook(cmd, fixture, { CODE_GRAPH_NO_INJECT: '1' });
    assert.equal(res.status, 0);
    assert.equal(res.stdout.trim(), '', 'opt-out must silence the inject');
  } finally {
    cleanupFixture(fixture, cmd);
  }
});

test('e2e: per-command cooldown — verbatim re-run within window injects only once', () => {
  const uniq = `PostCool${Date.now()}`;
  const fixture = e2eFixture(`process.stdout.write('src/foo.rs:7  hit\\n');`);
  const cmd = `echo go && grep "${uniq}" src/`;
  try {
    const r1 = runHook(cmd, fixture);
    assert.notEqual(r1.stdout.trim(), '', 'first run injects');
    const r2 = runHook(cmd, fixture);
    assert.equal(r2.stdout.trim(), '', 'second run within cooldown is silent');
  } finally {
    cleanupFixture(fixture, cmd);
  }
});

test('e2e: no index up to $HOME → silent exit 0', () => {
  // A cwd with no .code-graph anywhere up the tree resolves to null root → exit.
  const bare = fs.mkdtempSync(path.join(os.tmpdir(), 'post-grep-noidx-'));
  const stub = path.join(bare, 'cg-stub.js');
  fs.writeFileSync(stub, '#!/usr/bin/env node\nprocess.stdout.write("hit\\n");');
  fs.chmodSync(stub, 0o755);
  const cmd = `echo go && grep "FooBar" src/`;
  try {
    const res = spawnSync(process.execPath, [path.join(__dirname, 'post-grep-inject.js')], {
      cwd: bare,
      input: JSON.stringify({ tool_input: { command: cmd } }),
      encoding: 'utf8',
      env: { ...process.env, _CG_ANSWER_BINARY: stub, HOME: bare, CODE_GRAPH_QUIET_HOOKS: '0' },
    });
    assert.equal(res.status, 0);
    assert.equal(res.stdout.trim(), '');
  } finally {
    fs.rmSync(bare, { recursive: true, force: true });
  }
});
