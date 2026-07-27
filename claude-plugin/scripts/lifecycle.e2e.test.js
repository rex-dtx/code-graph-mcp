'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');

const repoRoot = path.resolve(__dirname, '..', '..');
const pluginRoot = path.resolve(__dirname, '..');
const lifecycleCli = path.join(__dirname, 'lifecycle.js');
const compositeCli = path.join(__dirname, 'statusline-composite.js');
const currentVersion = JSON.parse(fs.readFileSync(path.join(repoRoot, 'package.json'), 'utf8')).version;

function mkHome(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'code-graph-e2e-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function writeJson(filePath, value) {
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, JSON.stringify(value, null, 2) + '\n');
}

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf8'));
}

function runScript(homeDir, scriptPath, args = [], options = {}) {
  const env = { ...process.env, HOME: homeDir };
  // Do NOT set CLAUDE_PLUGIN_ROOT — lifecycle.js derives PLUGIN_ROOT from __dirname
  // to avoid env var leakage from other plugins in shared hook execution context.
  delete env.CLAUDE_PLUGIN_ROOT;
  return execFileSync(process.execPath, [scriptPath, ...args], {
    cwd: options.cwd || repoRoot,
    env,
    input: options.input,
    stdio: ['pipe', 'pipe', 'pipe'],
  }).toString();
}

test('lifecycle CLI handles install, disable self-heal, re-enable, and uninstall', (t) => {
  const homeDir = mkHome(t);
  const settingsPath = path.join(homeDir, '.claude', 'settings.json');
  const installedPath = path.join(homeDir, '.claude', 'plugins', 'installed_plugins.json');
  const registryPath = path.join(homeDir, '.cache', 'code-graph', 'statusline-registry.json');
  const manifestPath = path.join(homeDir, '.cache', 'code-graph', 'install-manifest.json');
  const cacheDir = path.join(homeDir, '.cache', 'code-graph');

  writeJson(settingsPath, {
    statusLine: { type: 'command', command: 'echo previous-status' },
    enabledPlugins: { 'code-graph-mcp@code-graph-mcp': true },
  });
  writeJson(installedPath, {
    plugins: {
      'code-graph-mcp@code-graph-mcp': [{
        installPath: pluginRoot,
        version: currentVersion,
        scope: 'user',
      }],
    },
  });

  runScript(homeDir, lifecycleCli, ['install']);
  let settings = readJson(settingsPath);
  let registry = readJson(registryPath);
  let manifest = readJson(manifestPath);
  assert.match(settings.statusLine.command, /statusline-composite\.js/);
  assert.equal(registry[0].id, '_previous');
  assert.equal(registry[1].id, 'code-graph');
  assert.equal(manifest.version, currentVersion);

  settings.enabledPlugins['code-graph-mcp@code-graph-mcp'] = false;
  writeJson(settingsPath, settings);
  runScript(homeDir, compositeCli, [], { input: '{}' });
  settings = readJson(settingsPath);
  assert.equal(settings.statusLine.command, 'echo previous-status');
  assert.equal(fs.existsSync(registryPath), false);

  settings.enabledPlugins['code-graph-mcp@code-graph-mcp'] = true;
  writeJson(settingsPath, settings);
  runScript(homeDir, lifecycleCli, ['install']);
  settings = readJson(settingsPath);
  registry = readJson(registryPath);
  assert.match(settings.statusLine.command, /statusline-composite\.js/);
  assert.equal(registry.length, 2);

  runScript(homeDir, lifecycleCli, ['uninstall']);
  settings = readJson(settingsPath);
  const installed = readJson(installedPath);
  assert.equal(settings.statusLine.command, 'echo previous-status');
  assert.deepEqual(settings.enabledPlugins, {});
  assert.deepEqual(installed.plugins, {});
  assert.equal(fs.existsSync(cacheDir), false);
});

test('lifecycle install writes to CLAUDE_CONFIG_DIR instead of ~/.claude when set', (t) => {
  // Multi-account isolation: a user with CLAUDE_CONFIG_DIR=~/work-claude
  // expects all plugin config (settings.json, installed_plugins.json,
  // statusline-providers backup) to land under that directory, not the
  // default ~/.claude. Default path must remain untouched.
  const homeDir = mkHome(t);
  const configDir = fs.mkdtempSync(path.join(os.tmpdir(), 'code-graph-cfgdir-'));
  t.after(() => fs.rmSync(configDir, { recursive: true, force: true }));

  const cfgSettings = path.join(configDir, 'settings.json');
  const cfgInstalled = path.join(configDir, 'plugins', 'installed_plugins.json');
  const cfgBackup = path.join(configDir, 'statusline-providers.json');
  const defaultSettings = path.join(homeDir, '.claude', 'settings.json');

  writeJson(cfgSettings, {
    statusLine: { type: 'command', command: 'echo prior-work-status' },
    enabledPlugins: { 'code-graph-mcp@code-graph-mcp': true },
  });
  writeJson(cfgInstalled, {
    plugins: {
      'code-graph-mcp@code-graph-mcp': [{
        installPath: pluginRoot,
        version: currentVersion,
        scope: 'user',
      }],
    },
  });

  // Run install with CLAUDE_CONFIG_DIR set; HOME points elsewhere.
  const env = { ...process.env, HOME: homeDir, CLAUDE_CONFIG_DIR: configDir };
  delete env.CLAUDE_PLUGIN_ROOT;
  execFileSync(process.execPath, [lifecycleCli, 'install'], {
    cwd: repoRoot, env, stdio: ['pipe', 'pipe', 'pipe'],
  });

  // Config landed in the override dir...
  const settings = readJson(cfgSettings);
  assert.match(settings.statusLine.command, /statusline-composite\.js/);
  assert.equal(fs.existsSync(cfgBackup), true,
    'statusline-providers backup should land in CLAUDE_CONFIG_DIR');

  // ...and default ~/.claude was never touched.
  assert.equal(fs.existsSync(defaultSettings), false,
    'default ~/.claude/settings.json must not be written when override is set');
});

test('composite expands a leading ~ in a _previous command instead of dropping it (issue #24)', (t) => {
  // A user whose prior statusline used a leading ~ (valid in settings.json, which
  // Claude Code runs through a shell). install() captures it verbatim as _previous.
  // The composite runs providers via execFileSync (no shell), so without tilde
  // expansion the command throws ENOENT and is silently swallowed — the user's
  // original statusline vanishes.
  const homeDir = mkHome(t);
  const prevScript = path.join(homeDir, '.claude', 'utils', 'statusline.sh');
  fs.mkdirSync(path.dirname(prevScript), { recursive: true });
  fs.writeFileSync(prevScript, '#!/bin/sh\necho "PREV-STATUSLINE-OK"\n');
  fs.chmodSync(prevScript, 0o755);

  const registryPath = path.join(homeDir, '.cache', 'code-graph', 'statusline-registry.json');
  writeJson(registryPath, [
    { id: '_previous', command: '~/.claude/utils/statusline.sh', needsStdin: true },
  ]);

  const out = runScript(homeDir, compositeCli, [], { input: '{}' });
  assert.match(out, /PREV-STATUSLINE-OK/,
    'a _previous command using a leading ~ must be tilde-expanded, not silently dropped');
});

test('expandTilde mirrors shell tilde expansion (only a leading ~ / ~/)', () => {
  const composite = require('./statusline-composite');
  const home = os.homedir();
  assert.equal(composite.expandTilde('~'), home);
  assert.equal(composite.expandTilde('~/.claude/utils/statusline.sh'),
    path.join(home, '.claude', 'utils', 'statusline.sh'));
  assert.equal(composite.expandTilde('/abs/path/script.sh'), '/abs/path/script.sh');
  assert.equal(composite.expandTilde('node'), 'node');
  assert.equal(composite.expandTilde('~user/script.sh'), '~user/script.sh',
    'other-user home dirs are not resolved');
  assert.equal(composite.expandTilde('a~/b'), 'a~/b',
    'only a leading ~ expands, not a mid-string ~');
});


test('a corrupt settings.json is backed up, never silently overwritten', (t) => {
  // readJson collapsed ENOENT and SyntaxError into the same `null`, so
  // `readJson(settingsPath()) || {}` handed install() an empty object and the
  // next atomic write replaced the whole file. One trailing comma — the most
  // common hand-edit slip — cost the user their model / env / permissions /
  // enabledPlugins and their own hooks, with no copy left anywhere.
  const homeDir = mkHome(t);
  const settingsPath = path.join(homeDir, '.claude', 'settings.json');
  const corrupt = [
    '{',
    '  "model": "opus",',
    '  "env": { "FOO": "bar" },',
    '  "permissions": { "allow": ["Bash(ls:*)"] },',
    '  "enabledPlugins": { "code-graph-mcp@code-graph-mcp": true },',   // <- trailing comma below
    '  "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": "echo mine" }] }] },',
    '}',
  ].join('\n');
  fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
  fs.writeFileSync(settingsPath, corrupt);

  runScript(homeDir, lifecycleCli, ['install']);

  const backups = fs.readdirSync(path.dirname(settingsPath))
    .filter((f) => f.startsWith('settings.json.corrupt-'));
  assert.equal(backups.length, 1,
    `install() must preserve an unparseable settings.json before rebuilding it (found: ${backups.join(', ')})`);
  assert.equal(
    fs.readFileSync(path.join(path.dirname(settingsPath), backups[0]), 'utf8'),
    corrupt,
    'the backup must be the original bytes, verbatim');

  // Backing up is only half the contract — the install must then actually
  // happen. Without this, a regression where install() backs up and then bails
  // (exactly the shape of the new refuse-path) would still pass the assertion
  // above, and the plugin would be silently inert.
  const rebuilt = readJson(settingsPath);
  assert.match(rebuilt.statusLine.command, /statusline-composite\.js/,
    'the rebuilt settings.json must carry the composite statusLine');
  assert.ok(rebuilt.hooks && Object.keys(rebuilt.hooks).length > 0,
    'the rebuilt settings.json must carry the plugin hooks');
});

// `chmod 000` is meaningless for uid 0 — root reads anything, so the refuse
// path would never be exercised and the test would assert the opposite of the
// truth. Skip loudly rather than silently pass.
const asRoot = typeof process.getuid === 'function' && process.getuid() === 0;

test('an UNREADABLE settings.json is left untouched, not rebuilt', { skip: asRoot && 'running as root' }, (t) => {
  // The first version of this fix split ENOENT from SyntaxError and mapped every
  // OTHER read error to "missing" — so a settings.json the process cannot read
  // was still rebuilt from `{}`, silently, with no backup possible. Real trigger:
  // one `sudo claude` leaves ~/.claude/settings.json root-owned 0600, and the
  // next ordinary SessionStart destroys it. `missing` must mean ENOENT alone.
  const homeDir = mkHome(t);
  const settingsPath = path.join(homeDir, '.claude', 'settings.json');
  writeJson(settingsPath, {
    model: 'opus',
    env: { FOO: 'bar' },
    hooks: { SessionStart: [{ hooks: [{ type: 'command', command: 'echo mine' }] }] },
  });
  const original = fs.readFileSync(settingsPath);
  fs.chmodSync(settingsPath, 0o000);
  t.after(() => { try { fs.chmodSync(settingsPath, 0o600); } catch { /* already gone */ } });

  let exitCode = 0;
  try {
    runScript(homeDir, lifecycleCli, ['install']);
  } catch (err) {
    exitCode = err.status;
  }

  fs.chmodSync(settingsPath, 0o600);
  assert.deepEqual(fs.readFileSync(settingsPath), original,
    'an unreadable settings.json must survive byte-for-byte');
  const backups = fs.readdirSync(path.dirname(settingsPath))
    .filter((f) => f.startsWith('settings.json.corrupt-'));
  assert.deepEqual(backups, [],
    'no backup is possible when the file cannot be read — and none should be faked');
  assert.notEqual(exitCode, 0,
    'refusing to install must not report success (`install && …` chains read exit 0 as done)');
});

test('an EMPTY settings.json is treated as absent, not corrupt', (t) => {
  // A zero-byte file is what a crash mid-write leaves behind. It carries nothing
  // worth preserving, so classifying it corrupt would litter ~/.claude with an
  // empty `.corrupt-*` copy on the way to the same rebuild.
  const homeDir = mkHome(t);
  const settingsPath = path.join(homeDir, '.claude', 'settings.json');
  fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
  fs.writeFileSync(settingsPath, '  \n');

  runScript(homeDir, lifecycleCli, ['install']);

  const backups = fs.readdirSync(path.dirname(settingsPath))
    .filter((f) => f.startsWith('settings.json.corrupt-'));
  assert.deepEqual(backups, [], 'an empty file has nothing to back up');
  assert.match(readJson(settingsPath).statusLine.command, /statusline-composite\.js/,
    'and it must still install normally');
});

test('update() refuses an unusable settings.json too, not just install()', (t) => {
  // install() and update() are separate entry points onto the same destructive
  // write. Fixing one and testing only that one is how the pair drifts.
  const homeDir = mkHome(t);
  const settingsPath = path.join(homeDir, '.claude', 'settings.json');
  const corrupt = '{ "model": "opus", }';
  fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
  fs.writeFileSync(settingsPath, corrupt);

  runScript(homeDir, lifecycleCli, ['update']);

  const backups = fs.readdirSync(path.dirname(settingsPath))
    .filter((f) => f.startsWith('settings.json.corrupt-'));
  assert.equal(backups.length, 1, 'update() must back up before rebuilding');
  assert.equal(
    fs.readFileSync(path.join(path.dirname(settingsPath), backups[0]), 'utf8'),
    corrupt,
    'update()`s backup must also be the original bytes');
});

test('a valid settings.json is never treated as corrupt', (t) => {
  // Negative control for the guard above: the backup path must not fire on the
  // normal case, or every SessionStart would litter ~/.claude with copies.
  const homeDir = mkHome(t);
  const settingsPath = path.join(homeDir, '.claude', 'settings.json');
  writeJson(settingsPath, { model: 'opus', env: { FOO: 'bar' } });

  runScript(homeDir, lifecycleCli, ['install']);

  const backups = fs.readdirSync(path.dirname(settingsPath))
    .filter((f) => f.startsWith('settings.json.corrupt-'));
  assert.deepEqual(backups, [], 'a parseable settings.json must not be backed up');
  const after = readJson(settingsPath);
  assert.equal(after.model, 'opus', 'user keys survive a normal install');
  assert.deepEqual(after.env, { FOO: 'bar' });
});

test('a BOM-prefixed but otherwise valid settings.json is not treated as corrupt', (t) => {
  // A UTF-8 BOM is JS whitespace, so `.trim()` strips it, but `JSON.parse`
  // rejects it. PowerShell 5.1's Out-File / Set-Content emit a BOM by default,
  // so a Windows user editing settings.json by hand gets a valid file that this
  // reader would call corrupt — back it up and rebuild the live one.
  const homeDir = mkHome(t);
  const settingsPath = path.join(homeDir, '.claude', 'settings.json');
  fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
  fs.writeFileSync(settingsPath, '\uFEFF' + JSON.stringify({ model: 'opus', env: { FOO: 'bar' } }, null, 2));

  runScript(homeDir, lifecycleCli, ['install']);

  const backups = fs.readdirSync(path.dirname(settingsPath))
    .filter((f) => f.startsWith('settings.json.corrupt-'));
  assert.deepEqual(backups, [], 'a BOM is not corruption');
  const after = JSON.parse(fs.readFileSync(settingsPath, 'utf8').replace(/^\uFEFF/, ''));
  assert.equal(after.model, 'opus', 'user keys survive');
  assert.deepEqual(after.env, { FOO: 'bar' });
  assert.match(after.statusLine.command, /statusline-composite\.js/);
});

test('a rebuilt settings.json is reported as destructive, not as a clean repair', (t) => {
  // healthCheck() auto-calls install() for any issue, and for a BACKUPABLE
  // corrupt file install() succeeds — so the rescan came back clean and doctor
  // printed `Hooks ✅ 1 issue(s) auto-repaired` for a run that had just moved
  // the user's model / env / permissions into a `.corrupt-*` file it never
  // named. The repair is fine; describing it as clean is not.
  const homeDir = mkHome(t);
  const settingsPath = path.join(homeDir, '.claude', 'settings.json');
  fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
  fs.writeFileSync(settingsPath, '{ "model": "opus", "env": { "FOO": "bar" }, }');

  // doctor exits non-zero whenever it leaves any issue unresolved (here: the
  // sandbox has no cargo toolchain), so capture stdout rather than let
  // execFileSync throw on an exit code unrelated to what is under test.
  let out = '';
  try { out = runScript(homeDir, path.join(__dirname, 'doctor.js'), []); }
  catch (err) { out = (err.stdout || '').toString(); }

  const hooksRow = out.split('\n').find((l) => /^\s*Hooks\s/.test(l)) || '';
  assert.doesNotMatch(hooksRow, /auto-repaired/,
    `a rebuild that replaced the user's settings must not read as a clean repair: ${hooksRow}`);
  assert.match(hooksRow, /REBUILT/, `must say the file was rebuilt: ${hooksRow}`);
  assert.match(hooksRow, /settings\.json\.corrupt-/,
    `must name the backup holding the user's original: ${hooksRow}`);

  // And the backup really is there, holding the original bytes.
  const backups = fs.readdirSync(path.dirname(settingsPath))
    .filter((f) => f.startsWith('settings.json.corrupt-'));
  assert.equal(backups.length, 1);
  assert.match(fs.readFileSync(path.join(path.dirname(settingsPath), backups[0]), 'utf8'), /"model"/);
});

test('doctor does not claim missing hooks when settings.json is unreadable', { skip: asRoot && 'running as root' }, (t) => {
  // Sibling read left on the old collapsed-`null` idiom: an unusable file became
  // `{}`, which has no hooks, so the coverage probe reported "missing 6/6
  // settings.json entries" — a confident, wrong diagnosis in the SAME table as
  // the correct "settings.json unusable" row.
  const homeDir = mkHome(t);
  const claudeDir = path.join(homeDir, '.claude');
  const settingsPath = path.join(claudeDir, 'settings.json');
  fs.mkdirSync(claudeDir, { recursive: true });
  fs.writeFileSync(settingsPath, '{ "model": "opus", }');
  fs.chmodSync(claudeDir, 0o555);
  t.after(() => { try { fs.chmodSync(claudeDir, 0o755); } catch { /* gone */ } });

  let out = '';
  try { out = runScript(homeDir, path.join(__dirname, 'doctor.js'), []); }
  catch (err) { out = (err.stdout || '').toString(); }
  fs.chmodSync(claudeDir, 0o755);

  const covRow = out.split('\n').find((l) => /^\s*Hook coverage\s/.test(l)) || '';
  assert.ok(covRow, `the Hook coverage row must exist — a probe that cannot run is itself a finding.\n${out}`);
  assert.doesNotMatch(covRow, /missing \d+\/\d+/,
    `coverage is not determinable from an unreadable file, so it must not be reported as missing: ${covRow}`);
  assert.match(covRow, /not determinable/, covRow);
});

test('doctor --check-only never writes settings.json', (t) => {
  // `--check-only` is a SHIPPED read-only contract (CHANGELOG v0.82.1: "it never
  // reaches runRepairs"). The write was never in runRepairs: runDiagnostics
  // called healthCheck(), which calls install(), which REBUILDS an unusable
  // settings.json. Measured: 36 B -> 3318 B with the model key gone, while the
  // report said "Run without --check-only to fix."
  for (const content of ['{ "model": "opus", }', '[1,2,3]', 'null', '"hello"']) {
    const homeDir = mkHome(t);
    const settingsPath = path.join(homeDir, '.claude', 'settings.json');
    fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
    fs.writeFileSync(settingsPath, content);
    const before = fs.readFileSync(settingsPath);

    try { runScript(homeDir, path.join(__dirname, 'doctor.js'), ['--check-only']); }
    catch { /* non-zero exit is expected when issues exist */ }

    assert.deepEqual(fs.readFileSync(settingsPath), before,
      `--check-only rewrote settings.json holding ${content}`);
    assert.deepEqual(
      fs.readdirSync(path.dirname(settingsPath)).filter((f) => f.startsWith('settings.json.corrupt-')),
      [],
      `--check-only created a backup for ${content} — it must not write at all`);
  }
});

test('SessionStart reports on STDOUT when it rebuilds settings.json', (t) => {
  // The honesty fix was wired into `doctor` — which a user runs deliberately —
  // and not into syncLifecycleConfig, which runs on EVERY SessionStart and calls
  // install() seven times. lifecycle.js logs the rebuild to stderr, which a
  // SessionStart hook discards, so from the user's side their model / env /
  // permissions vanished with no message at all.
  const homeDir = mkHome(t);
  const settingsPath = path.join(homeDir, '.claude', 'settings.json');
  fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
  fs.writeFileSync(settingsPath, '{"model":"opus","env":{"FOO":"bar"},}');

  let out = '';
  try {
    out = runScript(homeDir, path.join(__dirname, 'session-init.js'), [], {
      input: JSON.stringify({ source: 'startup' }),
    });
  } catch (err) { out = (err.stdout || '').toString(); }

  assert.match(out, /REBUILT/,
    `SessionStart must say so on stdout when it rebuilds settings.json.\n${out}`);
  assert.match(out, /settings\.json\.corrupt-/,
    `and must name the backup holding the original.\n${out}`);
});

// ── Contract audit 2026-07-27: shapes and permissions that reported success ──

test('a non-object `hooks` value is replaced, not silently discarded', (t) => {
  // `settings.hooks || {}` accepted an ARRAY, then assigned named properties
  // onto it — which JSON.stringify drops. install printed "settings=true" and
  // health printed "OK — all paths valid" while `"hooks": []` came back out with
  // zero of our six hooks registered: total, reported-as-success inertness.
  for (const badShape of [[], 'nonsense', 42, null]) {
    const homeDir = mkHome(t);
    const settingsPath = path.join(homeDir, '.claude', 'settings.json');
    writeJson(settingsPath, { model: 'opus', hooks: badShape });

    const out = runScript(homeDir, lifecycleCli, ['install']);
    assert.match(out, /Installed/, `install should succeed for hooks:${JSON.stringify(badShape)}`);

    const after = readJson(settingsPath);
    assert.equal(after.model, 'opus', 'unrelated user keys preserved');
    assert.equal(typeof after.hooks, 'object', 'hooks is an object');
    assert.ok(!Array.isArray(after.hooks), 'hooks is not an array');
    const registered = Object.values(after.hooks)
      .filter(Array.isArray)
      .reduce((n, entries) => n + entries.length, 0);
    assert.ok(registered > 0,
      `hooks:${JSON.stringify(badShape)} must not yield an install that registers nothing while reporting success`);
  }
});

test('an unwritable settings.json is reported and does not stamp the manifest', (t) => {
  const homeDir = mkHome(t);
  const claudeDir = path.join(homeDir, '.claude');
  const settingsPath = path.join(claudeDir, 'settings.json');
  writeJson(settingsPath, { model: 'opus' });
  const before = fs.readFileSync(settingsPath);
  const manifestBefore = path.join(homeDir, '.cache', 'code-graph', 'install-manifest.json');

  fs.chmodSync(claudeDir, 0o555);          // readable, not writable

  let stdout = '', stderr = '', code = 0;
  try {
    stdout = execFileSync(process.execPath, [lifecycleCli, 'install'], {
      cwd: repoRoot, env: { ...process.env, HOME: homeDir }, stdio: ['pipe', 'pipe', 'pipe'],
    }).toString();
  } catch (err) {
    code = err.status; stdout = err.stdout.toString(); stderr = err.stderr.toString();
  }
  // Restore here, not in t.after: mkHome's cleanup hook runs first and would
  // fail to rm a 0555 directory, turning a passing test into a hook error.
  fs.chmodSync(claudeDir, 0o755);

  assert.notEqual(code, 0, 'must not exit 0 — a chained `install && …` would read it as success');
  assert.match(stderr, /\[code-graph\] cannot write/, 'names the real cause, not a raw fs stack');
  assert.doesNotMatch(stdout, /^Installed/m, 'must not claim it installed');
  assert.deepEqual(fs.readFileSync(settingsPath), before, 'settings byte-identical');
  assert.equal(fs.existsSync(manifestBefore), false,
    'no manifest stamp — a current-version manifest would make the next run skip the retry');
});

test('cache teardown preserves a registry that still names other projects', (t) => {
  const homeDir = mkHome(t);
  const registry = path.join(homeDir, '.cache', 'code-graph', 'adopted-projects.json');
  const binary = path.join(homeDir, '.cache', 'code-graph', 'bin', 'code-graph-mcp');
  fs.mkdirSync(path.dirname(binary), { recursive: true });
  fs.writeFileSync(binary, 'x'.repeat(1024));
  writeJson(registry, ['/repo/one', '/repo/two']);

  const out = execFileSync(process.execPath, ['-e', `
    process.env.HOME = ${JSON.stringify(homeDir)};
    const { removeCacheResidue } = require(${JSON.stringify(lifecycleCli)});
    console.log(JSON.stringify({ removed: removeCacheResidue() }));
  `], { env: { ...process.env, HOME: homeDir }, cwd: repoRoot }).toString();

  assert.equal(JSON.parse(out.trim().split('\n').pop()).removed, true);
  assert.equal(fs.existsSync(binary), false, 'the ~40MB binary is still reclaimed');
  assert.equal(fs.existsSync(registry), true,
    'the registry is the ONLY record of which repos carry a managed CLAUDE.md block — ' +
    'wiping it strands every one of them with nothing left that knows where they are');
  assert.deepEqual(readJson(registry), ['/repo/one', '/repo/two']);
});

test('cache teardown leaves nothing behind when the registry is already empty', (t) => {
  // Negative control for the test above: preservation must not become residue.
  const homeDir = mkHome(t);
  const cacheDir = path.join(homeDir, '.cache', 'code-graph');
  fs.mkdirSync(path.join(cacheDir, 'bin'), { recursive: true });
  fs.writeFileSync(path.join(cacheDir, 'bin', 'code-graph-mcp'), 'x');
  writeJson(path.join(cacheDir, 'adopted-projects.json'), []);

  execFileSync(process.execPath, ['-e', `
    process.env.HOME = ${JSON.stringify(homeDir)};
    require(${JSON.stringify(lifecycleCli)}).removeCacheResidue();
  `], { env: { ...process.env, HOME: homeDir }, cwd: repoRoot });

  assert.equal(fs.existsSync(cacheDir), false,
    'an empty registry strands nothing, so re-creating the dir would just be new residue');
});
