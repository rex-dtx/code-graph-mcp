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
const { NPM_NEEDS_SHELL } = require('./npm-exec');
const { acquireLock } = require('./install-lock');

const NPM_TIMEOUT_MS = 60000;
const GITHUB_TIMEOUT_MS = 90000;

/**
 * Run one install step, capture its stderr, and invoke `cb` exactly once when
 * the step is over — whether it exited, timed out (spawn's `timeout` option
 * SIGTERMs and still emits 'exit'), or failed to start at all ('error' without
 * 'exit', e.g. npm missing from PATH). Never throws: a failed step just means
 * the chain moves on. `cb` receives the step's exit code (null when it never
 * exited cleanly); `spawnOpts` merges extras into the spawn options (shell for
 * npm on Windows, env for the auto-update child).
 */
function runStep(cmd, args, timeoutMs, prefix, spawnFn, cb, spawnOpts = {}) {
  let settled = false;
  let exitCode = null;
  const done = () => {
    if (settled) return;
    settled = true;
    cb(exitCode);
  };

  let child;
  try {
    child = spawnFn(cmd, args, {
      timeout: timeoutMs,
      stdio: ['ignore', 'ignore', 'pipe'],
      ...spawnOpts,
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
  child.on('exit', (code) => {
    exitCode = code;
    if (stderr.trim()) {
      process.stderr.write(stderr.trim().split('\n').map((l) => `${prefix} ${l}\n`).join(''));
    }
    done();
  });
}

/**
 * Kick off the background install chain. Fire-and-forget: exactly one of
 * `onInstalled` / `onFailed` is eventually called — EXCEPT when `lockPath` is
 * given and another live session already holds the lock, in which case the
 * chain is skipped entirely (neither callback fires; that session's install
 * lands and our stub's poller picks the binary up).
 *
 * - findBinary / clearCache: injected from find-binary.js (clear the disk
 *   cache before each re-resolve so a pre-install negative result can't mask a
 *   freshly landed binary).
 * - onInstalled: a step produced a resolvable binary — attempt the stub→real
 *   handover now instead of waiting for the stub's next 4s poll.
 * - onFailed: both steps ran and no binary resolved — surface manual hints.
 * - recordGlobalInstall: called when the npm step itself exited 0 AND yielded a
 *   binary — i.e. the plugin (not the user) introduced the global packages.
 *   lifecycle.js uninstall uses that marker to know it owns their removal.
 * - lockPath: opt-in inter-process lock — N cold sessions otherwise run
 *   parallel `npm install -g` against one global prefix (npm staging is not
 *   concurrency-safe).
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
  recordGlobalInstall = null,
  lockPath = null,
}) {
  let lock = null;
  if (lockPath) {
    lock = acquireLock(lockPath);
    if (!lock) {
      process.stderr.write('[code-graph] another session is already installing the binary; this stub will pick it up when it lands\n');
      return;
    }
  }
  const finish = (fn) => {
    if (lock) { lock.release(); lock = null; }
    fn();
  };
  const resolved = () => {
    clearCache();
    return findBinary();
  };

  runStep('npm', ['install', '-g', `@sdsrs/code-graph@${version}`], npmTimeoutMs, '[code-graph][npm]', spawnFn, (npmExit) => {
    if (resolved()) {
      if (npmExit === 0 && recordGlobalInstall) {
        try { recordGlobalInstall(); } catch { /* marker is best-effort */ }
      }
      finish(onInstalled);
      return;
    }
    process.stderr.write('[code-graph] npm install did not yield a binary; falling back to GitHub release download...\n');
    runStep(process.execPath, [autoUpdateScript, '--silent', '--install-missing'], githubTimeoutMs, '[code-graph][auto-update]', spawnFn, () => {
      if (resolved()) { finish(onInstalled); return; }
      finish(onFailed);
    }, {
      // The child would otherwise try to take the same install lock we hold.
      env: { ...process.env, CODE_GRAPH_INSTALL_LOCK_HELD: '1' },
    });
  }, NPM_NEEDS_SHELL ? { shell: true } : {});
}

module.exports = { installBinaryInBackground, runStep, NPM_TIMEOUT_MS, GITHUB_TIMEOUT_MS };
