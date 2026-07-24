'use strict';
// Tests for launcher-install.js — the background install chain that replaced
// mcp-launcher.js's old SYNCHRONOUS npm(60s)+GitHub(90s) missing-binary path.
// The sync path blocked the MCP handshake past Claude Code's 30s connect
// timeout, so a cold install always presented as "connection timed out after
// 30000ms". These tests drive the chain with an injected spawn so no npm and
// no network is ever touched.
//
// Run: node --test claude-plugin/scripts/launcher-install.test.js
const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const path = require('path');

const { installBinaryInBackground } = require('./launcher-install');

// Fake child_process.spawn: records each call, returns an EventEmitter child
// whose exit (or error) is emitted on the next tick — after runStep has
// attached its handlers, mirroring real spawn timing.
function fakeSpawn(plan) {
  const calls = [];
  const fn = (cmd, args, opts) => {
    const idx = calls.length;
    calls.push({ cmd, args, opts });
    const child = new EventEmitter();
    child.stderr = new EventEmitter();
    process.nextTick(() => {
      const step = plan[idx] || { exit: 0 };
      if (step.stderr) child.stderr.emit('data', step.stderr);
      if (step.error) child.emit('error', new Error(step.error));
      else child.emit('exit', step.exit ?? 0);
    });
    return child;
  };
  fn.calls = calls;
  return fn;
}

// Await the chain outcome: resolves 'installed' or 'failed'.
function runChain({ spawnFn, binaryByStep }) {
  return new Promise((resolve) => {
    let step = 0;
    installBinaryInBackground({
      version: '1.2.3',
      // Called once after each completed step; binaryByStep[n] is what
      // findBinary "sees" after step n (null = still missing).
      findBinary: () => binaryByStep[step++] ?? null,
      clearCache: () => {},
      onInstalled: () => resolve('installed'),
      onFailed: () => resolve('failed'),
      spawnFn,
    });
  });
}

test('npm step succeeds → onInstalled, GitHub fallback never spawned', async () => {
  const spawnFn = fakeSpawn([{ exit: 0 }]);
  const outcome = await runChain({ spawnFn, binaryByStep: ['/cache/bin/code-graph-mcp'] });
  assert.equal(outcome, 'installed');
  assert.equal(spawnFn.calls.length, 1, 'must not run the GitHub fallback after npm succeeds');
  assert.equal(spawnFn.calls[0].cmd, 'npm');
  assert.deepEqual(spawnFn.calls[0].args, ['install', '-g', '@sdsrs/code-graph@1.2.3']);
});

test('npm yields no binary → falls back to auto-update --install-missing', async () => {
  const spawnFn = fakeSpawn([{ exit: 0 }, { exit: 0 }]);
  const outcome = await runChain({ spawnFn, binaryByStep: [null, '/cache/bin/code-graph-mcp'] });
  assert.equal(outcome, 'installed');
  assert.equal(spawnFn.calls.length, 2);
  assert.equal(spawnFn.calls[1].cmd, process.execPath);
  assert.equal(path.basename(spawnFn.calls[1].args[0]), 'auto-update.js');
  assert.deepEqual(spawnFn.calls[1].args.slice(1), ['--silent', '--install-missing'],
    '--install-missing must bypass the dev-mode short-circuit (issue #12)');
});

test('npm missing from PATH (spawn error) → still proceeds to fallback', async () => {
  const spawnFn = fakeSpawn([{ error: 'spawn npm ENOENT' }, { exit: 0 }]);
  const outcome = await runChain({ spawnFn, binaryByStep: [null, '/cache/bin/code-graph-mcp'] });
  assert.equal(outcome, 'installed');
  assert.equal(spawnFn.calls.length, 2, 'an unstartable npm must not abort the chain');
});

test('both steps exhaust without a binary → onFailed exactly once', async () => {
  const spawnFn = fakeSpawn([{ exit: 1, stderr: 'npm ERR! network' }, { exit: 1 }]);
  const outcome = await runChain({ spawnFn, binaryByStep: [null, null] });
  assert.equal(outcome, 'failed');
  assert.equal(spawnFn.calls.length, 2);
});

test('install steps run detached from the MCP handshake (no *Sync spawn)', () => {
  // The whole point of this module: nothing in it may block the event loop
  // while the stub answers initialize. Guard against a regression back to
  // spawnSync/execSync.
  const src = require('fs').readFileSync(path.join(__dirname, 'launcher-install.js'), 'utf8');
  assert.doesNotMatch(src, /spawnSync|execSync|execFileSync/,
    'launcher-install must never use synchronous child_process APIs');
});

// ── Inter-process install lock + global-install marker ──────────────────────

const fsLock = require('node:fs');
const osLock = require('node:os');

function mkLockDir() {
  return fsLock.mkdtempSync(path.join(osLock.tmpdir(), 'cg-li-lock-'));
}

test('lockPath held by a live process → chain skipped entirely (no spawn, no callbacks)', (t) => {
  const dir = mkLockDir();
  t.after(() => fsLock.rmSync(dir, { recursive: true, force: true }));
  const lockPath = path.join(dir, 'install.lock');
  // Our own pid is alive → the lock reads as genuinely held.
  fsLock.writeFileSync(lockPath, JSON.stringify({ pid: process.pid, at: 'x' }));

  const spawnFn = fakeSpawn([{ exit: 0 }]);
  let called = false;
  installBinaryInBackground({
    version: '1.2.3',
    findBinary: () => '/cache/bin/code-graph-mcp',
    clearCache: () => {},
    onInstalled: () => { called = true; },
    onFailed: () => { called = true; },
    spawnFn,
    lockPath,
  });
  assert.equal(spawnFn.calls.length, 0, 'no install step may run while another session holds the lock');
  assert.equal(called, false, 'neither callback fires — the other session owns the outcome');
});

test('lock is taken for the chain and released on completion', async (t) => {
  const dir = mkLockDir();
  t.after(() => fsLock.rmSync(dir, { recursive: true, force: true }));
  const lockPath = path.join(dir, 'install.lock');

  const spawnFn = fakeSpawn([{ exit: 0 }]);
  await new Promise((resolve) => {
    installBinaryInBackground({
      version: '1.2.3',
      findBinary: () => '/cache/bin/code-graph-mcp',
      clearCache: () => {},
      onInstalled: resolve,
      onFailed: resolve,
      spawnFn,
      lockPath,
    });
  });
  assert.equal(fsLock.existsSync(lockPath), false, 'lock released once the chain settles');
});

test('recordGlobalInstall fires only when the npm step itself succeeded', async () => {
  // npm exit 0 + binary resolved → the plugin introduced the global packages.
  let recorded = 0;
  const spawnOk = fakeSpawn([{ exit: 0 }]);
  await new Promise((resolve) => {
    installBinaryInBackground({
      version: '1.2.3',
      findBinary: () => '/cache/bin/code-graph-mcp',
      clearCache: () => {},
      onInstalled: resolve,
      onFailed: resolve,
      spawnFn: spawnOk,
      recordGlobalInstall: () => { recorded++; },
    });
  });
  assert.equal(recorded, 1, 'marker written after a successful npm install');

  // npm fails, GitHub fallback lands the binary → NOT a global npm install.
  const spawnFallback = fakeSpawn([{ exit: 1 }, { exit: 0 }]);
  await new Promise((resolve) => {
    let step = 0;
    installBinaryInBackground({
      version: '1.2.3',
      findBinary: () => (step++ === 0 ? null : '/cache/bin/code-graph-mcp'),
      clearCache: () => {},
      onInstalled: resolve,
      onFailed: resolve,
      spawnFn: spawnFallback,
      recordGlobalInstall: () => { recorded++; },
    });
  });
  assert.equal(recorded, 1, 'no marker when the binary came from the GitHub fallback');
});

test('auto-update fallback child carries CODE_GRAPH_INSTALL_LOCK_HELD (no self-deadlock)', async () => {
  const spawnFn = fakeSpawn([{ exit: 1 }, { exit: 0 }]);
  await new Promise((resolve) => {
    installBinaryInBackground({
      version: '1.2.3',
      findBinary: () => null,
      clearCache: () => {},
      onInstalled: resolve,
      onFailed: resolve,
      spawnFn,
    });
  });
  assert.equal(spawnFn.calls.length, 2);
  assert.equal(spawnFn.calls[1].opts.env.CODE_GRAPH_INSTALL_LOCK_HELD, '1',
    'the spawned auto-update must not try to take the lock its parent holds');
});
