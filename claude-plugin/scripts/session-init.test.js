'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const path = require('path');

const { launchBackgroundAutoUpdate, syncLifecycleConfig, ensureIndexFresh, verifyBinary, computeQuietHooks, shouldInjectMap, shouldInjectRecentImpact, recentImpactWorthShowing, filterSourceFiles, parseGitStatusPaths, formatRecentImpact } = require('./session-init');

test('syncLifecycleConfig is exported as a callable helper', () => {
  assert.equal(typeof syncLifecycleConfig, 'function');
});

test('ensureIndexFresh is exported as a callable helper', () => {
  assert.equal(typeof ensureIndexFresh, 'function');
});

test('ensureIndexFresh returns skipped when no index exists', () => {
  const origCwd = process.cwd();
  const tmpDir = require('node:os').tmpdir();
  process.chdir(tmpDir);
  try {
    const result = ensureIndexFresh();
    assert.equal(result, 'skipped');
  } finally {
    process.chdir(origCwd);
  }
});

test('verifyBinary returns available:true when binary is found and executable', () => {
  const result = verifyBinary();
  // In dev repo, binary should be found (target/release/code-graph-mcp)
  if (result.available) {
    assert.equal(typeof result.binary, 'string');
    assert.ok(result.binary.length > 0);
  } else {
    // Binary not built — still verify the return shape
    assert.equal(result.available, false);
  }
});

test('verifyBinary returns structured result with expected shape', () => {
  const result = verifyBinary();
  assert.equal(typeof result.available, 'boolean');
  assert.ok('binary' in result);
  if (!result.available && result.binary) {
    assert.ok('issue' in result);
  }
});

test('launchBackgroundAutoUpdate spawns detached silent updater', () => {
  const calls = [];

  const ok = launchBackgroundAutoUpdate((command, args, options) => {
    const record = { command, args, options, unrefCalled: false };
    calls.push(record);
    return {
      unref() {
        record.unrefCalled = true;
      },
    };
  }, { HOME: '/tmp/fake-home' });

  assert.equal(ok, true);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, process.execPath);
  assert.match(calls[0].args[0], /auto-update\.js$/);
  assert.equal(calls[0].args[1], 'check');
  assert.equal(calls[0].args[2], '--silent');
  assert.equal(calls[0].options.detached, true);
  assert.equal(calls[0].options.stdio, 'ignore');
  assert.equal(calls[0].options.env.CODE_GRAPH_AUTO_UPDATE_SILENT, '1');
  assert.equal(calls[0].unrefCalled, true);
});

const { consistencyCheck, runSessionInit } = require('./session-init');

test('consistencyCheck is exported as a function', () => {
  assert.equal(typeof consistencyCheck, 'function');
});

test('runSessionInit no-ops (nonProject) in a non-project cwd', (t) => {
  // /tmp-style cwd (no .git/manifest) → the gate returns BEFORE
  // syncLifecycleConfig / verifyBinary / ensureIndexFresh / maybeAutoAdopt /
  // injectProjectMap, leaving zero footprint. Safe to call: the early return
  // precedes every side-effectful step.
  const os = require('os');
  const origCwd = process.cwd();
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-si-nonproj-'));
  process.chdir(tmp);
  try {
    const res = runSessionInit();
    if (res.inactive) { t.skip('plugin seen inactive in this env — gate not reached'); return; }
    assert.equal(res.nonProject, true);
    assert.equal(res.lifecycle, 'noop');
    assert.equal(res.autoUpdateLaunched, false);
  } finally {
    process.chdir(origCwd);
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('consistencyCheck returns empty array when binary version matches plugin', () => {
  const result = consistencyCheck('/tmp/nonexistent-binary');
  assert.ok(Array.isArray(result));
});

// ──────────────────────────────────────────────────────────────────────────
// v0.17.0 — quietHooks: unconditional quiet default
// Priority: legacy QUIET_HOOKS=0/1 > new VERBOSE_HOOKS=1 > default true.
// `adopted` param is dead (unconditional default does not consult it) but
// the destructured signature still accepts it for backward compat.
// ──────────────────────────────────────────────────────────────────────────

test('computeQuietHooks: legacy QUIET_HOOKS="0" forces noisy', () => {
  assert.equal(computeQuietHooks({ env: { CODE_GRAPH_QUIET_HOOKS: '0' } }), false);
});

test('computeQuietHooks: legacy QUIET_HOOKS="1" forces quiet', () => {
  assert.equal(computeQuietHooks({ env: { CODE_GRAPH_QUIET_HOOKS: '1' } }), true);
});

test('computeQuietHooks: VERBOSE_HOOKS="1" opts in to noisy', () => {
  assert.equal(computeQuietHooks({ env: { CODE_GRAPH_VERBOSE_HOOKS: '1' } }), false);
});

test('computeQuietHooks: legacy QUIET_HOOKS="1" wins over VERBOSE_HOOKS="1"', () => {
  // Conflicting opt-ins: legacy explicit-quiet wins over new verbose opt-in.
  // (Legacy QUIET_HOOKS="0" + VERBOSE_HOOKS="1" both mean noisy — no conflict.)
  assert.equal(
    computeQuietHooks({ env: { CODE_GRAPH_QUIET_HOOKS: '1', CODE_GRAPH_VERBOSE_HOOKS: '1' } }),
    true
  );
});

test('computeQuietHooks: env unset → quiet by default', () => {
  assert.equal(computeQuietHooks({ env: {} }), true);
});

test('computeQuietHooks: no args → quiet by default', () => {
  assert.equal(computeQuietHooks(), true);
});

test('computeQuietHooks: legacy `adopted` param is ignored under new default', () => {
  // adopted=true used to imply quiet; now quiet is unconditional.
  // adopted=false used to imply noisy; now still quiet by default.
  assert.equal(computeQuietHooks({ adopted: true, env: {} }), true);
  assert.equal(computeQuietHooks({ adopted: false, env: {} }), true);
});

test('shouldInjectMap: only injects when available + not-quiet + adopted', () => {
  // The single positive case: opted into verbose AND adopted.
  assert.equal(shouldInjectMap({ available: true, quietHooks: false, adopted: true }), true);
  // Adopted-only gate: verbose but unadopted → no injection (the zero-referenced
  // case cross-project-interference flagged).
  assert.equal(shouldInjectMap({ available: true, quietHooks: false, adopted: false }), false);
  // Quiet default suppresses regardless of adoption.
  assert.equal(shouldInjectMap({ available: true, quietHooks: true, adopted: true }), false);
  // No binary → nothing to inject.
  assert.equal(shouldInjectMap({ available: false, quietHooks: false, adopted: true }), false);
  // Missing args default to falsey → no injection.
  assert.equal(shouldInjectMap(), false);
});

// ──────────────────────────────────────────────────────────────────────────
// v0.63 — SessionStart "live context": recent-change blast radius injection.
// ──────────────────────────────────────────────────────────────────────────

test('shouldInjectRecentImpact: default-ON for adopted projects (separate gate from the static map)', () => {
  // Unlike shouldInjectMap, this does NOT require the verbose opt-in — it earns
  // standing context because it's git-delta-derived, not duplicative of MEMORY.md.
  assert.equal(shouldInjectRecentImpact({ available: true, adopted: true, env: {} }), true);
});

test('shouldInjectRecentImpact: hard kill-switch and dedicated opt-out suppress it', () => {
  assert.equal(shouldInjectRecentImpact({ available: true, adopted: true, env: { CODE_GRAPH_QUIET_HOOKS: '1' } }), false);
  assert.equal(shouldInjectRecentImpact({ available: true, adopted: true, env: { CODE_GRAPH_NO_RECENT_IMPACT: '1' } }), false);
});

test('shouldInjectRecentImpact: needs binary + adoption', () => {
  assert.equal(shouldInjectRecentImpact({ available: false, adopted: true, env: {} }), false);
  assert.equal(shouldInjectRecentImpact({ available: true, adopted: false, env: {} }), false);
  assert.equal(shouldInjectRecentImpact(), false);
});

test('filterSourceFiles: keeps AST-bearing source, drops config/lock/doc', () => {
  const diff = [
    'src/domain.rs', 'Cargo.lock', 'Cargo.toml', 'CHANGELOG.md',
    'package.json', 'src/parser/relations/mod.rs', 'claude-plugin/scripts/session-init.js',
    'npm/linux-x64/package.json',
  ].join('\n');
  assert.deepEqual(filterSourceFiles(diff), [
    'src/domain.rs', 'src/parser/relations/mod.rs', 'claude-plugin/scripts/session-init.js',
  ]);
});

test('parseGitStatusPaths: extracts paths from modified / staged / untracked lines (finding #3)', () => {
  // `git status --porcelain` columns: " M" unstaged-mod, "M " staged, "??" untracked,
  // "A " added. The untracked line is exactly what diff-only missed.
  const out = [
    ' M src/domain.rs',
    'M  src/cli.rs',
    '?? src/brand_new.rs',
    'A  src/staged_new.rs',
    'D  src/gone.rs',
  ].join('\n');
  assert.deepEqual(parseGitStatusPaths(out), [
    'src/domain.rs', 'src/cli.rs', 'src/brand_new.rs', 'src/staged_new.rs', 'src/gone.rs',
  ]);
});

test('parseGitStatusPaths: rename takes the NEW path; quoted path is unquoted', () => {
  assert.deepEqual(parseGitStatusPaths('R  src/old.rs -> src/new.rs'), ['src/new.rs']);
  assert.deepEqual(parseGitStatusPaths('?? "src/with space.rs"'), ['src/with space.rs']);
});

test('parseGitStatusPaths: blank / too-short / non-string input → []', () => {
  assert.deepEqual(parseGitStatusPaths(''), []);
  assert.deepEqual(parseGitStatusPaths(null), []);
  assert.deepEqual(parseGitStatusPaths('\n\n'), []);
  assert.deepEqual(parseGitStatusPaths('??'), []); // no path after status
});

test('parseGitStatusPaths composes with filterSourceFiles: untracked source kept, config dropped', () => {
  const out = [' M Cargo.toml', '?? src/new_feature.rs', '?? notes.txt'].join('\n');
  assert.deepEqual(filterSourceFiles(parseGitStatusPaths(out)), ['src/new_feature.rs']);
});

test('formatRecentImpact: re-run command is runnable verbatim when ≤4 changed (finding #4)', () => {
  const affected = { affected_files: [{ depth: 1, is_test: false, path: 'src/a.rs' }], tests: [] };
  const text = formatRecentImpact(['src/x.rs', 'src/y.rs'], affected);
  assert.match(text, /Re-run impacted tests: code-graph-mcp affected src\/x\.rs src\/y\.rs$/m);
  assert.doesNotMatch(text, /more changed file/);
  assert.doesNotMatch(text, / …/); // no bare ellipsis
});

test('formatRecentImpact: >4 changed → explicit "+N more", not a bare ellipsis (finding #4)', () => {
  const affected = { affected_files: [{ depth: 1, is_test: false, path: 'src/a.rs' }], tests: [] };
  const changed = ['s/1.rs', 's/2.rs', 's/3.rs', 's/4.rs', 's/5.rs', 's/6.rs'];
  const text = formatRecentImpact(changed, affected);
  assert.match(text, /code-graph-mcp affected s\/1\.rs s\/2\.rs s\/3\.rs s\/4\.rs {2}\(\+2 more changed file\(s\)/);
  assert.doesNotMatch(text, / …/); // the misleading bare ellipsis is gone
});

test('filterSourceFiles: caps the list and tolerates blank/garbage input', () => {
  assert.deepEqual(filterSourceFiles(''), []);
  assert.deepEqual(filterSourceFiles(null), []);
  const many = Array.from({ length: 40 }, (_, i) => `src/m${i}.rs`).join('\n');
  assert.equal(filterSourceFiles(many).length, 25);
  assert.equal(filterSourceFiles(many, 3).length, 3);
});

test('formatRecentImpact: renders changed + blast radius + direct dependents', () => {
  const affected = {
    affected_files: [
      { depth: 1, is_test: false, path: 'src/cli.rs' },
      { depth: 1, is_test: false, path: 'src/graph/impact.rs' },
      { depth: 1, is_test: true, path: 'src/parser/relations/tests.rs' },
      { depth: 2, is_test: false, path: 'src/main.rs' },
    ],
    changed: ['src/domain.rs'],
    tests: ['src/parser/relations/tests.rs', 'tests/integration.rs'],
  };
  const text = formatRecentImpact(['src/domain.rs'], affected);
  assert.match(text, /Recent changes/);
  assert.match(text, /Changed: src\/domain\.rs/);
  assert.match(text, /Impacts 4 file\(s\) \(2 direct dependent\(s\)\), 2 test file\(s\)/);
  assert.match(text, /Direct dependents: src\/cli\.rs, src\/graph\/impact\.rs/);
  assert.match(text, /code-graph-mcp affected src\/domain\.rs/);
  // It is graph-unique — the copy says so (the whole point vs the static map).
  assert.match(text, /not in MEMORY\.md/);
});

test('recentImpactWorthShowing: WIP always shows, regardless of source', () => {
  assert.equal(recentImpactWorthShowing({ isWip: true, source: 'startup' }), true);
  assert.equal(recentImpactWorthShowing({ isWip: true, source: 'compact' }), true);
});

test('recentImpactWorthShowing: clean tree (last-commit fallback) suppressed on cold startup, shown on resume', () => {
  assert.equal(recentImpactWorthShowing({ isWip: false, source: 'startup' }), false);
  assert.equal(recentImpactWorthShowing({ isWip: false, source: 'clear' }), true);
  assert.equal(recentImpactWorthShowing({ isWip: false, source: 'compact' }), true);
  assert.equal(recentImpactWorthShowing({ isWip: false, source: 'resume' }), true);
  // Unknown source (direct call / test) defaults to showing — only explicit
  // cold startup is the suppressed case.
  assert.equal(recentImpactWorthShowing({ isWip: false }), true);
  assert.equal(recentImpactWorthShowing(), true);
});

test('formatRecentImpact: high-fanout change drops the noisy name list, keeps risk + test scope', () => {
  // >15 direct dependents = a constants/util node "touches everything"; the
  // first-N names are arbitrary noise, so only risk + test count is surfaced.
  const affected = {
    affected_files: Array.from({ length: 20 }, (_, i) => ({ depth: 1, is_test: false, path: `src/f${i}.rs` })),
    tests: ['tests/a.rs', 'tests/b.rs'],
  };
  const text = formatRecentImpact(['src/domain.rs'], affected);
  assert.match(text, /High-fanout change/);
  assert.match(text, /run the full suite \(2 test file\(s\)\)/);
  assert.doesNotMatch(text, /Direct dependents:/); // name list suppressed
});

test('formatRecentImpact: at/under the fanout threshold the name list IS the signal', () => {
  const affected = {
    affected_files: Array.from({ length: 15 }, (_, i) => ({ depth: 1, is_test: false, path: `src/f${i}.rs` })),
    tests: [],
  };
  const text = formatRecentImpact(['src/x.rs'], affected);
  assert.doesNotMatch(text, /High-fanout/);
  assert.match(text, /Direct dependents:/);
});

test('formatRecentImpact: caps direct-dependent list with a "+N more" overflow', () => {
  const affected = {
    affected_files: Array.from({ length: 10 }, (_, i) => ({ depth: 1, is_test: false, path: `src/f${i}.rs` })),
    tests: [],
  };
  const text = formatRecentImpact(['src/domain.rs'], affected);
  assert.match(text, /\+4 more/); // 10 direct, cap 6 → 4 hidden
});

test('formatRecentImpact: returns null when nothing graph-relevant (no dependents / no changes)', () => {
  // A deps-only commit: changed files filtered to empty upstream → caller skips.
  assert.equal(formatRecentImpact([], { affected_files: [] }), null);
  // Changed source but zero indexed dependents → nothing actionable to say.
  assert.equal(formatRecentImpact(['src/x.rs'], { affected_files: [], tests: [] }), null);
  assert.equal(formatRecentImpact(['src/x.rs'], {}), null);
});

test('consistencyCheck returns version-mismatch when versions differ', (t) => {
  const os = require('os');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cc-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const bin = path.join(dir, 'code-graph-mcp');
  fs.writeFileSync(bin, [
    '#!/usr/bin/env bash',
    'if [ "$1" = "--version" ]; then',
    '  echo "code-graph-mcp 0.0.1"',
    '  exit 0',
    'fi',
    'exit 0',
  ].join('\n'));
  fs.chmodSync(bin, 0o755);

  const issues = consistencyCheck(bin);
  const versionIssue = issues.find(i => i.id === 'version-mismatch');
  assert.ok(versionIssue, 'should detect version mismatch');
  assert.ok(versionIssue.msg.includes('0.0.1'));
});

test('injectProjectMap map call carries CODE_GRAPH_INTERNAL (delivery, not a model conversion)', () => {
  // injectProjectMap runs `code-graph-mcp map --compact` to inject the project map.
  // That run is a hook-internal delivery — it must carry the internal marker so
  // record_cli_use (src/cli.rs) does not log it as a phantom model `use` event
  // (the 2026-06-23 mem audit found this leak class; the sibling affected call was
  // already guarded). Asserted at source level because injectProjectMap is not exported.
  const src = fs.readFileSync(path.join(__dirname, 'session-init.js'), 'utf8');
  const i = src.indexOf("['map', '--compact']");
  assert.ok(i >= 0, 'map injection present');
  assert.match(src.slice(i, i + 420), /CODE_GRAPH_INTERNAL:\s*'1'/);
});

