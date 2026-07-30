'use strict';
// npm is `npm.cmd` on Windows: child_process spawn/execFileSync cannot exec a
// .cmd without a shell (and Node >= 18.20 throws EINVAL spawning .cmd directly
// as a CVE-2024-27980 mitigation). Every bare `spawn('npm', ...)` in the
// install/update flow therefore silently ENOENT'd on Windows while
// commandExists('npm') (via `where`) said npm was present.
const { hidden } = require('./proc-opts');

const NPM_NEEDS_SHELL = process.platform === 'win32';

// Args we are willing to hand to cmd.exe unquoted: flags, package specs,
// versions, paths. Everything else gets double-quoted; anything that cannot be
// made safe inside double quotes throws rather than being passed through.
const SAFE_CMD_ARG = /^[A-Za-z0-9_@./:\\+=-]+$/;

/**
 * Quote one argument for the `cmd.exe /d /s /c "..."` line Node builds when
 * `shell: true`. Node itself only space-joins `args` in that mode — the reason
 * DEP0190 runtime-deprecated the combination in Node 24 — so the joining (and
 * the quoting) has to happen here.
 * @param {string} arg
 * @returns {string}
 */
function quoteCmdArg(arg) {
  const s = String(arg);
  if (SAFE_CMD_ARG.test(s)) return s;
  // `"` ends our quoting; `%` and `!` are expanded inside double quotes; CR/LF
  // splits the command line. None of our call sites produce these — a throw is
  // a loud bug report, not a user-facing failure path.
  if (/["%!\r\n]/.test(s)) {
    throw new Error(`npm argument cannot be safely quoted for cmd.exe: ${JSON.stringify(s)}`);
  }
  // Double a trailing run of backslashes. cmd.exe itself does not treat `\` as
  // an escape, but the RECEIVING program's MSVCRT argv parser does: `"C:\x\"`
  // reads the final `\"` as an escaped quote and swallows the rest of the
  // command line into that argument. Only reachable for path-shaped args, which
  // no current call site passes — but this helper reads as general-purpose.
  const trailing = /\\+$/.exec(s);
  return `"${trailing ? s + trailing[0] : s}"`;
}

/**
 * Build a child_process invocation for `npm <args>` that is correct on both
 * platforms. Spread into spawn/spawnSync/execFileSync:
 *
 *   const { file, args, opts } = npmInvocation(['root', '-g'], { timeout: 2000 });
 *   execFileSync(file, args, opts);
 *
 * On Windows the whole command is pre-quoted into `file` with an EMPTY `args`
 * array: passing `args` alongside `shell: true` is DEP0190 (runtime-deprecated
 * in Node 24 — it space-joins without escaping, which is a shell-injection
 * hole) and printed a deprecation warning on every npm call.
 *
 * @param {string[]} args
 * @param {object} [opts] - extra child_process options
 * @returns {{file: string, args: string[], opts: object}}
 */
function npmInvocation(args, opts = {}) {
  if (!NPM_NEEDS_SHELL) return { file: 'npm', args: [...args], opts: hidden(opts) };
  return {
    file: ['npm', ...args.map(quoteCmdArg)].join(' '),
    args: [],
    opts: hidden({ ...opts, shell: true }),
  };
}

module.exports = { npmInvocation, quoteCmdArg, NPM_NEEDS_SHELL };
