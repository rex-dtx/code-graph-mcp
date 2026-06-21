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

