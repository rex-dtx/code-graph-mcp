'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');

const fs = require('fs');
const os = require('os');
const path = require('path');

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
  // Repoint one PreToolUse entry at an old, now-pruned plugin-cache version dir —
  // present, recognized as ours (description unchanged), but the script path no
  // longer exists on disk. replaceAll (not replace) so BOTH the `if [ -f "…" ]`
  // guard and the `node "…"` exec path move to the dead dir — a realistic stale
  // entry (the executed path is what staleness keys off; a half-mutated command
  // whose exec path stayed current is, correctly, not stale).
  const bash = settings.hooks.PreToolUse.find(e => e.matcher === 'Bash');
  bash.hooks[0].command = bash.hooks[0].command.replaceAll('/scripts/', '/0.0.1-old/scripts/');
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

test('classifyEmbeddings reports the real download outcome, not "retry shortly" (issue #35)', () => {
  const { classifyEmbeddings } = require('./doctor');
  const base = { model_available: true, embedding_progress: '0/27201',
    embedding_status: 'pending', search_mode: 'fts_only' };

  // A download that failed must SAY so, with its cause. Advising the user to
  // wait is what made a permanently-broken install look like a slow one.
  const failed = classifyEmbeddings({ ...base,
    model_download: 'download FAILED after 3 attempt(s): tls handshake rejected' });
  assert.equal(failed.status, 'warn');
  assert.match(failed.detail, /FAILED after 3 attempt\(s\): tls handshake rejected/);
  assert.doesNotMatch(failed.detail, /retry shortly/);

  // No record at all is a DIFFERENT diagnosis: the download never started.
  const never = classifyEmbeddings(base);
  assert.equal(never.status, 'warn');
  assert.match(never.detail, /NO download has ever been attempted/);
  assert.match(never.detail, /CODE_GRAPH_MODEL_DIR/);

  // In-flight is the one state where waiting IS the right advice.
  const inflight = classifyEmbeddings({ ...base, model_download: 'download in flight (attempt 1)' });
  assert.match(inflight.detail, /in flight/);
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

// ── CLI argument handling ──────────────────────────────────────────────────
//
// Contract audit follow-up: `args.includes('--check-only')` ignored every other
// argument, so a typo'd flag ran the FULL repair pass — writing settings.json and
// MEMORY.md — while the user believed they had asked for the read-only mode. A
// typo silently inverting a read-only contract is the worst shape this flag can
// have, so an unrecognized argument now stops before any diagnosis runs.

const { execFileSync } = require('child_process');

// BOTH entry points. `node lifecycle.js doctor …` carried its own copy of the
// flag parsing, so the first version of this guard fixed doctor.js and left the
// sibling running the repair pass on a typo. They now share `runDoctorCli` from
// doctor.js, and every case below is asserted against both so they cannot drift.
const ENTRY_POINTS = [
  { label: 'doctor.js', argv: (args) => [path.join(__dirname, 'doctor.js'), ...args] },
  { label: 'lifecycle.js doctor', argv: (args) => [path.join(__dirname, 'lifecycle.js'), 'doctor', ...args] },
];

function runDoctorCli(homeDir, args, entry = ENTRY_POINTS[0]) {
  try {
    const stdout = execFileSync(process.execPath, entry.argv(args), {
      // CLAUDE_CONFIG_DIR as well as HOME: claude-config.js returns
      // `process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude')`,
      // so the env var WINS and redirecting HOME alone leaves the sandbox open
      // for any developer who exports it. The `no arguments still repairs` case
      // below runs the full repair pass, which is what would land in their real
      // config. Same fix as tests/cli_e2e.rs; this JS sibling was missed.
      env: { ...process.env, HOME: homeDir, CLAUDE_CONFIG_DIR: path.join(homeDir, '.claude') },
      stdio: ['pipe', 'pipe', 'pipe'],
    }).toString();
    return { code: 0, stdout, stderr: '' };
  } catch (err) {
    return {
      code: err.status,
      stdout: err.stdout ? err.stdout.toString() : '',
      stderr: err.stderr ? err.stderr.toString() : '',
    };
  }
}

function freshHome(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-doctor-cli-'));
  fs.mkdirSync(path.join(dir, '.claude'), { recursive: true });
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

for (const entry of ENTRY_POINTS) {
  test(`${entry.label}: refuses an unknown argument instead of silently repairing`, (t) => {
    // Every near-miss spelling of the read-only flag.
    for (const typo of ['--check-onlyy', '--checkonly', '--check_only', '-check-only', '--dry-run']) {
      const home = freshHome(t);
      const r = runDoctorCli(home, [typo], entry);
      assert.equal(r.code, 2, `${typo} must be rejected, not ignored`);
      assert.match(r.stderr, /unknown argument/, `${typo} must say why`);
      assert.equal(r.stdout, '', `${typo} must not emit a diagnostic report`);
      assert.equal(
        fs.existsSync(path.join(home, '.claude', 'settings.json')), false,
        `${typo} must not have run the repair pass — that is the read-only contract ` +
        'the user thought they were invoking');
    }
  });

  test(`${entry.label}: --check-only still reports and still writes nothing`, (t) => {
    const home = freshHome(t);
    const r = runDoctorCli(home, ['--check-only'], entry);
    assert.ok(r.stdout.length > 0, 'the real flag still produces a report');
    assert.equal(fs.existsSync(path.join(home, '.claude', 'settings.json')), false,
      'read-only');
  });

  test(`${entry.label}: no arguments still repairs (the guard must not make it inert)`, (t) => {
    // Negative control for the two above.
    const home = freshHome(t);
    const r = runDoctorCli(home, [], entry);
    assert.ok(r.stdout.length > 0);
    assert.equal(fs.existsSync(path.join(home, '.claude', 'settings.json')), true,
      'the default mode must still perform repairs');
  });

  test(`${entry.label}: --help exits 0 without running diagnostics`, (t) => {
    const home = freshHome(t);
    const r = runDoctorCli(home, ['--help'], entry);
    assert.equal(r.code, 0);
    // Matches src/main.rs's help too — the two texts are kept in sync, and the
    // e2e test test_cli_js_subcommands_help_is_side_effect_free asserts the same
    // USAGE marker on the binary side.
    assert.match(r.stdout, /USAGE:\n\s+code-graph-mcp doctor/);
    assert.match(r.stdout, /--check-only/);
    assert.equal(fs.existsSync(path.join(home, '.claude', 'settings.json')), false,
      '--help must not run the repair pass — a help flag that acts is its own bug class');
  });
}

// ── an unwritable ~/.claude must be diagnosed as such by EVERY repair arm ────
//
// Round-6 F2/F4: the `settingsUnwritable` state was taught to
// `missing-hooks-in-settings` but not to `hooks-invalid`, and neither arm had a
// test. The consequence was a chmod being reported as "plugin scripts may be
// missing — reinstall the npm package": a diagnosis that sends the user to fix
// something that is not broken, from the tool whose job is to say what is.

function unwritableHome(t, seed) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-doctor-ro-'));
  const claudeDir = path.join(home, '.claude');
  fs.mkdirSync(claudeDir, { recursive: true });
  fs.writeFileSync(path.join(claudeDir, 'settings.json'), JSON.stringify(seed, null, 2) + '\n');
  fs.chmodSync(claudeDir, 0o555);
  t.after(() => {
    try { fs.chmodSync(claudeDir, 0o755); } catch { /* already restored */ }
    fs.rmSync(home, { recursive: true, force: true });
  });
  return home;
}

test('a read-only ~/.claude is reported as a permissions problem, not a missing package', (t) => {
  // The entry has to be recognizable as OURS, or the coverage survey reports it
  // as missing and the *other* arm answers — which is how the first version of
  // this test passed while the arm it names stayed unguarded. Build the real
  // entries, then repoint every path at a dead directory (same technique as the
  // stale-path survey test above) so `hooks-invalid` is what gets raised.
  const desired = buildSettingsHookEntries();
  const hooks = {};
  for (const [event, entries] of Object.entries(desired)) {
    hooks[event] = entries.map((e) => {
      const copy = JSON.parse(JSON.stringify(e));
      copy.hooks = copy.hooks.map((h) => ({
        ...h,
        command: h.command.replaceAll('/scripts/', '/0.0.1-gone/scripts/'),
      }));
      return copy;
    });
  }
  const home = unwritableHome(t, { hooks });

  const r = runDoctorCli(home, []);
  const all = r.stdout + r.stderr;

  assert.match(all, /not writable/,
    'the real cause must appear in the report');
  assert.doesNotMatch(all, /npm install -g/,
    'must NOT tell the user to reinstall a package over a chmod — that is the ' +
    'misdiagnosis this arm exists to prevent');
  // The file really was left alone.
  const settings = JSON.parse(fs.readFileSync(path.join(home, '.claude', 'settings.json'), 'utf8'));
  assert.ok(settings.hooks.PreToolUse, 'settings untouched');
});

test('a read-only ~/.claude is reported by the missing-hooks arm too', (t) => {
  // No code-graph entries at all → `missing-hooks-in-settings` rather than
  // `hooks-invalid`. Both arms print about the same failed install() call and
  // have to agree about why it failed.
  const home = unwritableHome(t, { model: 'opus' });

  const r = runDoctorCli(home, []);
  const all = r.stdout + r.stderr;

  assert.match(all, /not writable/, 'the real cause must appear here as well');
  assert.doesNotMatch(all, /already had entries/,
    'must not claim the hooks were already registered');
  assert.notEqual(r.code, 0, 'nothing was fixed, so this is not a clean run');
});

// ── the exit code must reflect what doctor could NOT fix — on every entry point ─
//
// `unresolvedCount` has a unit test above, but a predicate test does not cover
// whether anything calls the predicate — this repo learned that in v0.45.3, on a
// self-heal glue that regressed twice while its predicate stayed green. The exit
// code is now produced by three entry points (doctor.js, `lifecycle.js doctor`,
// and the Rust binary, which used to filter argv before dispatch), so the wiring
// is exactly the part that can drift.
//
// The invariant asserted here is the CHANGELOG v0.85.4 promise in its own terms:
// exit 0 when every found issue was resolved, 1 when something was left, and
// --check-only nonzero whenever any issue exists at all.
//
// HONEST SCOPE — what this can and cannot catch. It compares the exit code
// against doctor's OWN "N/M addressed" line, so it only distinguishes
// "unresolved" from "found" in an environment where they differ, i.e. where
// every found issue was fixable. On a checkout whose src/ is newer than the
// built binary, the unfixable "Source fresh" issue makes remaining>0 always, and
// then `found>0` and `remaining>0` agree — reverting `unresolvedCount` to the
// pre-v0.85.4 `return issueCount` leaves these two tests GREEN (measured; only
// the predicate test above reddens). Treat the 0-branch as pinned by the
// predicate test plus the report-vs-code consistency here, NOT by these alone.
for (const entry of ENTRY_POINTS) {
  test(`${entry.label}: exit code equals "did anything remain unfixed"`, (t) => {
    const home = freshHome(t);
    const r = runDoctorCli(home, [], entry);
    const addressed = /(\d+)\/(\d+) issue\(s\) addressed/.exec(r.stdout);
    const found = /(\d+) issue\(s\) found/.exec(r.stdout);

    if (!found) {
      // A perfectly clean sandbox: nothing found, nothing to leave unfixed.
      assert.equal(r.code, 0, `no issues found must exit 0; got ${r.code}\n${r.stdout}`);
      return;
    }
    assert.ok(addressed, `a repair run that found issues must report N/M addressed:\n${r.stdout}`);
    const fixed = Number(addressed[1]);
    const total = Number(addressed[2]);
    const remaining = total - fixed;
    assert.equal(
      r.code, remaining > 0 ? 1 : 0,
      `exit code must key off issues left UNRESOLVED (${remaining} of ${total}), not issues found. ` +
      `A run that fixed everything and still exited 1 is what broke \`doctor && …\` ` +
      `and every self-heal caller.\n${r.stdout}`
    );
  });

  test(`${entry.label}: --check-only exits nonzero while any issue exists`, (t) => {
    const home = freshHome(t);
    const r = runDoctorCli(home, ['--check-only'], entry);
    if (/issue\(s\) found/.test(r.stdout)) {
      assert.notEqual(r.code, 0,
        `--check-only must stay nonzero while issues exist — it repairs nothing, ` +
        `so "all resolved" can never be true for it.\n${r.stdout}`);
    }
    assert.doesNotMatch(r.stdout, /issue\(s\) addressed/,
      '--check-only must not claim to have addressed anything');
  });
}
