'use strict';
/**
 * Background binary installer for mcp-launcher.js.
 *
 * Replaces the launcher's old SYNCHRONOUS missing-binary chain — `npm install
 * -g` (60s timeout) then the GitHub-release fallback (90s) — which ran BEFORE
 * any MCP JSON-RPC was answered. Claude Code's connect timeout is 30s, so a
 * cold install always presented as "connection timed out after 30000ms" and
 * the tools only appeared on a later reconnect. The launcher now answers the
 * handshake from an upgradeable 0-tool stub immediately and runs this chain in
 * the background; `onInstalled` fires as soon as a step yields a resolvable
 * binary so the caller can hand the live connection over to it.
 *
 * Steps (same order + timeouts as the old sync chain):
 *   1. npm install -g @sdsrs/code-graph@<version>   — the normal package path
 *   2. auto-update.js --silent --install-missing    — direct GitHub release
 *      download, for when npm succeeds but the platform optionalDependency
 *      fails silently (OS-mismatch tolerance, flaky registry — issue #12)
 *
 * `spawnFn` is injectable so the chain is unit-testable without touching npm
 * or the network (launcher-install.test.js).
 */
const { spawn } = require('child_process');
const path = require('path');

const NPM_TIMEOUT_MS = 60000;
const GITHUB_TIMEOUT_MS = 90000;

/**
 * Run one install step, capture its stderr, and invoke `cb` exactly once when
 * the step is over — whether it exited, timed out (spawn's `timeout` option
 * SIGTERMs and still emits 'exit'), or failed to start at all ('error' without
 * 'exit', e.g. npm missing from PATH). Never throws: a failed step just means
 * the chain moves on.
 */
function runStep(cmd, args, timeoutMs, prefix, spawnFn, cb) {
  let settled = false;
  const done = () => {
    if (settled) return;
    settled = true;
    cb();
  };

  let child;
  try {
    child = spawnFn(cmd, args, {
      timeout: timeoutMs,
      stdio: ['ignore', 'ignore', 'pipe'],
    });
  } catch (e) {
    process.stderr.write(`[code-graph] install step ${cmd} failed to start: ${e.message}\n`);
    done();
    return;
  }

  let stderr = '';
  if (child.stderr) child.stderr.on('data', (d) => { stderr += d.toString(); });
  child.on('error', (err) => {
    process.stderr.write(`[code-graph] install step ${cmd} failed to start: ${err.message}\n`);
    done();
  });
  child.on('exit', () => {
    if (stderr.trim()) {
      process.stderr.write(stderr.trim().split('\n').map((l) => `${prefix} ${l}\n`).join(''));
    }
    done();
  });
}

/**
 * Kick off the background install chain. Fire-and-forget: exactly one of
 * `onInstalled` / `onFailed` is eventually called.
 *
 * - findBinary / clearCache: injected from find-binary.js (clear the disk
 *   cache before each re-resolve so a pre-install negative result can't mask a
 *   freshly landed binary).
 * - onInstalled: a step produced a resolvable binary — attempt the stub→real
 *   handover now instead of waiting for the stub's next 4s poll.
 * - onFailed: both steps ran and no binary resolved — surface manual hints.
 */
function installBinaryInBackground({
  version,
  findBinary,
  clearCache,
  onInstalled,
  onFailed,
  spawnFn = spawn,
  autoUpdateScript = path.join(__dirname, 'auto-update.js'),
  npmTimeoutMs = NPM_TIMEOUT_MS,
  githubTimeoutMs = GITHUB_TIMEOUT_MS,
}) {
  const resolved = () => {
    clearCache();
    return findBinary();
  };

  runStep('npm', ['install', '-g', `@sdsrs/code-graph@${version}`], npmTimeoutMs, '[code-graph][npm]', spawnFn, () => {
    if (resolved()) { onInstalled(); return; }
    process.stderr.write('[code-graph] npm install did not yield a binary; falling back to GitHub release download...\n');
    runStep(process.execPath, [autoUpdateScript, '--silent', '--install-missing'], githubTimeoutMs, '[code-graph][auto-update]', spawnFn, () => {
      if (resolved()) { onInstalled(); return; }
      onFailed();
    });
  });
}

module.exports = { installBinaryInBackground, runStep, NPM_TIMEOUT_MS, GITHUB_TIMEOUT_MS };
