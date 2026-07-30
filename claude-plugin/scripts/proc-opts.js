'use strict';
/**
 * Child-process option defaults shared by every spawn/exec in this plugin.
 *
 * Windows creates a NEW visible console window for every console-subsystem
 * child whose parent has no console of its own — and none of our parents do:
 * the MCP server (`node mcp-launcher.js`), the hooks and the statusline are all
 * launched hidden by Claude Code. Node's `windowsHide` defaults to `false` on
 * EVERY child_process API (spawn/spawnSync/exec/execSync/execFile/execFileSync),
 * so each `where` / `curl` / `tar` / `npm` child flashed a console window for
 * ~1s and stole keyboard focus. Reported as 5–7 flashes per session start
 * (issue #40); the auto-update treadmill fixed alongside it made that per
 * session, forever.
 *
 * `windowsHide: true` maps to CREATE_NO_WINDOW, which only stops the child from
 * ALLOCATING a console — inherited stdio handles still work, so an interactive
 * `doctor` run in a real terminal is unaffected. No-op on non-Windows.
 *
 * Every child_process call site under claude-plugin/scripts/ must route through
 * here (or set windowsHide itself); `windows-hide.test.js` fails the build on a
 * new call site that doesn't.
 */
function hidden(opts = {}) {
  return { windowsHide: true, ...opts };
}

module.exports = { hidden };
