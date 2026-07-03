'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');

const { runDiagnostics, formatReport, surveyHookCoverage } = require('./doctor');
const { buildSettingsHookEntries } = require('./lifecycle');

// Build a settings.json whose hooks exactly mirror what we'd register now.
function settingsWithCurrentHooks() {
  const desired = buildSettingsHookEntries();
  const hooks = {};
  for (const [event, entries] of Object.entries(desired)) {
    hooks[event] = entries.map(e => JSON.parse(JSON.stringify(e)));
  }
  return { hooks };
}

test('runDiagnostics returns an array of check results', () => {
  const results = runDiagnostics();
  assert.ok(Array.isArray(results));
  assert.ok(results.length > 0, 'should have at least one check result');
  for (const r of results) {
    assert.equal(typeof r.name, 'string');
    assert.ok(['ok', 'warn', 'error', 'skip'].includes(r.status));
    assert.equal(typeof r.detail, 'string');
  }
});

test('formatReport produces readable output', () => {
  const results = [
    { name: 'Binary version', status: 'ok', detail: 'v0.7.16' },
    { name: 'Source fresh', status: 'warn', detail: 'src/ modified 3min after binary', fixId: 'binary-stale' },
    { name: 'Schema', status: 'ok', detail: 'v6' },
  ];
  const output = formatReport(results);
  assert.ok(output.includes('Binary version'));
  assert.ok(output.includes('v0.7.16'));
  assert.ok(output.includes('Source fresh'));
  assert.ok(output.includes('3min'));
});

test('formatReport shows issue count when problems exist', () => {
  const results = [
    { name: 'Test', status: 'warn', detail: 'problem', fixId: 'test-fix' },
  ];
  const output = formatReport(results);
  assert.ok(output.includes('1'));
  assert.ok(output.includes('issue'));
});

test('formatReport: --check-only never says "Fixing..." (it does not repair)', () => {
  const results = [
    { name: 'Hook coverage', status: 'warn', detail: 'missing', fixId: 'hooks' },
  ];
  // Default (repair mode) announces the fix.
  assert.ok(formatReport(results).includes('Fixing...'),
    'repair mode should announce Fixing...');
  // --check-only is read-only: it must NOT claim to fix, and should point the
  // user at the repair command instead.
  const checkOnly = formatReport(results, { checkOnly: true });
  assert.ok(!checkOnly.includes('Fixing...'),
    `--check-only must not say "Fixing..."; got: ${checkOnly}`);
  assert.ok(checkOnly.includes('--check-only'),
    `--check-only should hint how to fix; got: ${checkOnly}`);
});

test('formatReport shows all-clear when no problems', () => {
  const results = [
    { name: 'Binary version', status: 'ok', detail: 'v0.7.16' },
    { name: 'Schema', status: 'ok', detail: 'v6' },
  ];
  const output = formatReport(results);
  assert.ok(output.includes('All checks passed') || output.includes('0 issues'));
});

test('surveyHookCoverage reports clean when all entries are current', () => {
  const cov = surveyHookCoverage(settingsWithCurrentHooks());
  assert.equal(cov.missing.length, 0, 'no missing entries');
  assert.equal(cov.stale.length, 0, 'no stale entries');
});

test('surveyHookCoverage flags a present-but-stale hook path', () => {
  const settings = settingsWithCurrentHooks();
  // Repoint one PreToolUse entry at an old plugin-cache version dir — present,
  // recognized as ours (description unchanged), but command no longer current.
  const bash = settings.hooks.PreToolUse.find(e => e.matcher === 'Bash');
  bash.hooks[0].command = bash.hooks[0].command.replace('/scripts/', '/0.0.1-old/scripts/');
  const cov = surveyHookCoverage(settings);
  assert.equal(cov.missing.length, 0, 'entry is present, not missing');
  assert.ok(cov.stale.includes('PreToolUse:Bash'),
    `stale Bash path should be flagged; got stale=${JSON.stringify(cov.stale)}`);
});

test('surveyHookCoverage flags missing entries when settings empty', () => {
  const cov = surveyHookCoverage({});
  assert.ok(cov.missing.length === cov.expected.length, 'all expected entries missing');
  assert.equal(cov.stale.length, 0, 'nothing present to be stale');
});

// ── relicRepairGuard (v0.50.0 — doctor twin of the session-init relic guard) ──

test('relicRepairGuard blocks settings repair from a relic copy and redirects', () => {
  const { relicRepairGuard } = require('./doctor');
  const lines = [];
  // Relic context → guard fires, prints the redirect, returns true (skip install).
  assert.equal(relicRepairGuard({ relic: true, log: (s) => lines.push(s) }), true);
  assert.ok(lines.some(l => l.includes('not the active install')),
    `guard must explain why repair is skipped, got: ${lines.join(' | ')}`);
  // Active (or dev/npm) context → repair proceeds.
  assert.equal(relicRepairGuard({ relic: false, log: () => {} }), false);
});

// ── classifyEmbeddings (vector-availability — warns on silent FTS5-only) ──

test('classifyEmbeddings WARNS when embed-capable but nothing embedded (vector inactive)', () => {
  const { classifyEmbeddings } = require('./doctor');
  // The exact silent-FTS5 gap: model_available compile-flag true, real embeddable
  // nodes exist, but 0 embedded (model never downloaded/loaded).
  const r = classifyEmbeddings({ model_available: true, embedding_progress: '0/2745',
    embedding_status: 'pending', search_mode: 'fts_only' });
  assert.equal(r.status, 'warn', 'must not false-green a vector-inactive index');
  assert.match(r.detail, /FTS5-only|vector INACTIVE/);
});

test('classifyEmbeddings WARNS when binary lacks embed-model feature', () => {
  const { classifyEmbeddings } = require('./doctor');
  const r = classifyEmbeddings({ model_available: false, embedding_progress: '0/0' });
  assert.equal(r.status, 'warn');
  assert.match(r.detail, /without embed-model/);
});

test('classifyEmbeddings OK for hybrid (partial + complete) and no-embeddable', () => {
  const { classifyEmbeddings } = require('./doctor');
  assert.equal(classifyEmbeddings({ model_available: true, embedding_progress: '900/2745' }).status, 'ok');
  assert.equal(classifyEmbeddings({ model_available: true, embedding_progress: '2745/2745' }).status, 'ok');
  // total === 0 is a non-code index, genuinely nothing to embed → ok, not a false warn.
  const none = classifyEmbeddings({ model_available: true, embedding_progress: '0/0' });
  assert.equal(none.status, 'ok');
  assert.match(none.detail, /no embeddable nodes/);
});

// ── dev-rebuild feature preservation (no silent hybrid→FTS5 downgrade / ping-pong) ──
test('devBuildCommand preserves feature set: hybrid → --features embed-model, fts → --no-default-features', () => {
  const { devBuildCommand } = require('./doctor');
  assert.match(devBuildCommand(true), /--features embed-model/);
  assert.doesNotMatch(devBuildCommand(true), /--no-default-features/);
  assert.match(devBuildCommand(false), /--no-default-features/);
  assert.doesNotMatch(devBuildCommand(false), /--features embed-model/);
});

test('detectEmbedModel reads model_available from `health-check --json`; probe failure → null (never a false downgrade signal)', () => {
  const { detectEmbedModel } = require('./doctor');
  // hybrid binary
  const hybridStub = (_bin, args) => {
    assert.deepEqual(args, ['health-check', '--json']);
    return JSON.stringify({ model_available: true });
  };
  assert.equal(detectEmbedModel('/bin/cg', hybridStub), true);
  // FTS5-only binary
  assert.equal(detectEmbedModel('/bin/cg', () => JSON.stringify({ model_available: false })), false);
  // probe throws (binary broken) → null (caller defaults to FTS5 + note, not a downgrade claim)
  assert.equal(detectEmbedModel('/bin/cg', () => { throw new Error('boom'); }), null);
  // unparseable output → null
  assert.equal(detectEmbedModel('/bin/cg', () => 'not json'), null);
  // no binary → null
  assert.equal(detectEmbedModel(null), null);
});

test('unresolvedCount: repair mode exits 0 iff every found issue was fixed', () => {
  const { unresolvedCount } = require('./doctor');
  // Clean run — nothing found.
  assert.equal(unresolvedCount({ checkOnly: false, issueCount: 0, fixed: 0 }), 0);
  // Repair fixed everything ("N/N addressed") → 0, so `doctor && …` and
  // self-heal automation don't read a successful repair as failure. This is
  // the regression this contract guards: previously exited 1 on any issue found.
  assert.equal(unresolvedCount({ checkOnly: false, issueCount: 3, fixed: 3 }), 0);
  // Partial repair → the remainder is unresolved (nonzero → exit 1).
  assert.equal(unresolvedCount({ checkOnly: false, issueCount: 3, fixed: 1 }), 2);
  // Advisory-only issue with no working auto-repair (fixed stays 0) → unresolved.
  assert.equal(unresolvedCount({ checkOnly: false, issueCount: 1, fixed: 0 }), 1);
});

test('unresolvedCount: --check-only reports every found issue (never repairs)', () => {
  const { unresolvedCount } = require('./doctor');
  // check-only performs no repair, so fixed is 0; a found issue must still
  // surface as unresolved (exit 1) — check mode reports cleanliness.
  assert.equal(unresolvedCount({ checkOnly: true, issueCount: 2, fixed: 0 }), 2);
  assert.equal(unresolvedCount({ checkOnly: true, issueCount: 0, fixed: 0 }), 0);
});

test('runRepairs: hooks-invalid counts fixed only when the post-install re-scan is clean', () => {
  // hooks-invalid is raised only after diagnosis already ran install()+re-scan
  // and paths were STILL broken. The repair arm must re-verify, else it reports
  // a false exit 0 ("healthy") while the hooks stay broken. Stub the lifecycle
  // deps runRepairs pulls via require('./lifecycle') on the shared cached export
  // object; restore in finally so no other test sees the stubs.
  const { runRepairs } = require('./doctor');
  const lc = require('./lifecycle');
  const orig = { install: lc.install, scan: lc.scanForBrokenPaths, relic: lc.isStaleRelicContext };
  const hooksInvalid = [{ name: 'Hooks', status: 'warn', fixId: 'hooks-invalid' }];
  try {
    lc.isStaleRelicContext = () => false;   // not a relic → repair proceeds
    lc.install = () => {};                    // install() that cannot restore the paths
    // Re-scan still broken → must NOT count as fixed (old code did fixed++ blindly).
    lc.scanForBrokenPaths = () => [{ type: 'hook', event: 'PreToolUse:Edit', path: '/gone.js' }];
    assert.equal(runRepairs(hooksInvalid), 0, 'still-broken after install must not count as fixed');
    // Re-scan clean → the repair took effect → counts as fixed.
    lc.scanForBrokenPaths = () => [];
    assert.equal(runRepairs(hooksInvalid), 1, 'verified-clean after install counts as fixed');
  } finally {
    lc.install = orig.install;
    lc.scanForBrokenPaths = orig.scan;
    lc.isStaleRelicContext = orig.relic;
  }
});
