'use strict';
// npm is `npm.cmd` on Windows: child_process spawn/execFileSync cannot exec a
// .cmd without a shell (and Node >= 18.20 throws EINVAL spawning .cmd directly
// as a CVE-2024-27980 mitigation). Every bare `spawn('npm', ...)` in the
// install/update flow therefore silently ENOENT'd on Windows while
// commandExists('npm') (via `where`) said npm was present. All args routed
// through here are fixed flags / package specs — shell-quoting-safe.
const NPM_NEEDS_SHELL = process.platform === 'win32';

/** Merge shell:true into spawn/exec options when the platform needs it. */
function npmSpawnOpts(opts = {}) {
  return NPM_NEEDS_SHELL ? { ...opts, shell: true } : opts;
}

module.exports = { npmSpawnOpts, NPM_NEEDS_SHELL };
