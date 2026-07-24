'use strict';
// Regression suite for the hook-registration drift that let stale/duplicate
// code-graph hooks accumulate in settings.json across node-version / delivery-
// surface switches (RCA 2026-07-24):
//
//   P0-a  isOurHookEntry required MARKETPLACE_NAME ('code-graph-mcp') in the
//         command path, but the GLOBAL npm package installs under the package
//         name '@sdsrs/code-graph' (NO '-mcp'). Hooks delivered via `npm i -g`
//         were therefore invisible to the eviction pass and orphan-accumulated
//         — a bare v24.11.1(0.46.0) + v24.18.0(0.101.0) pair firing alongside
//         the plugin-cache set, i.e. every Edit/Read/Bash/prompt hook ran 2-3x.
//
//   P0-b  surveyHookCoverage flagged an entry `stale` on an EXACT command-string
//         mismatch. Two registration authorities (plugin-cache session-init vs
//         global-npm CLI doctor) derive different absolute paths, so each ran
//         considered the other's VALID CURRENT entry stale and rewrote it →
//         settings.json ping-pong on every alternating run. Staleness must be
//         version/surface-tolerant: a present entry whose script exists and is
//         current (either surface) is NOT stale; only a dead path or an older
//         plugin-cache version dir is.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');

const lifecycle = require('./lifecycle');

// --- P0-a: recognition of the global-npm delivery surface --------------------

test('P0-a isOurHookEntry recognizes a global-npm (@sdsrs/code-graph) delivered hook', () => {
  const bareNpmEntry = {
    matcher: 'Edit',
    hooks: [{
      type: 'command',
      command: 'node "/home/u/.nvm/versions/node/v24.11.1/lib/node_modules/@sdsrs/code-graph/claude-plugin/scripts/pre-edit-guide.js"',
    }],
  };
  assert.ok(lifecycle.isOurHookEntry(bareNpmEntry),
    'a hook whose path uses the npm package name @sdsrs/code-graph (no -mcp) and carries no description must still be recognized as ours');
});

test('P0-a registerHooksToSettings evicts orphan global-npm hooks (no cross-surface duplicates)', () => {
  // Two legacy bare nvm entries for the SAME event (two node versions) — the
  // exact shape that accumulated in the field. After registration the desired
  // set must appear exactly once; the orphans must be gone.
  const settings = { hooks: { PreToolUse: [
    { matcher: 'Edit', hooks: [{ type: 'command', command: 'node "/x/.nvm/versions/node/v24.11.1/lib/node_modules/@sdsrs/code-graph/claude-plugin/scripts/pre-edit-guide.js"' }] },
    { matcher: 'Edit', hooks: [{ type: 'command', command: 'node "/x/.nvm/versions/node/v24.18.0/lib/node_modules/@sdsrs/code-graph/claude-plugin/scripts/pre-edit-guide.js"' }] },
  ] } };
  lifecycle.registerHooksToSettings(settings);
  const preEdit = settings.hooks.PreToolUse.filter(e =>
    (e.hooks || []).some(h => h.command.includes('pre-edit-guide.js')));
  assert.equal(preEdit.length, 1,
    `expected exactly one pre-edit-guide registration after dedup, got ${preEdit.length}`);
});

// --- P0-b: version/surface-tolerant staleness --------------------------------

test('P0-b surveyHookCoverage flags a dead-path entry as stale (dangling after node uninstall)', () => {
  // A present entry pointing at a script file that no longer exists (the node
  // version was uninstalled) must be reported stale so re-registration heals it.
  const desired = lifecycle.buildSettingsHookEntries();
  const settings = { hooks: {} };
  for (const [event, entries] of Object.entries(desired)) settings.hooks[event] = [];
  // Register the real desired set, then corrupt one entry to a dead path.
  lifecycle.registerHooksToSettings(settings);
  const deadCmd = 'if [ -f "/home/u/.nvm/versions/node/v24.11.1/lib/node_modules/@sdsrs/code-graph/claude-plugin/scripts/pre-edit-guide.js" ]; then node "/home/u/.nvm/versions/node/v24.11.1/lib/node_modules/@sdsrs/code-graph/claude-plugin/scripts/pre-edit-guide.js"; fi';
  const preEdit = settings.hooks.PreToolUse.find(e => e.matcher === 'Edit');
  preEdit.hooks[0].command = deadCmd;
  const survey = lifecycle.surveyHookCoverage(settings);
  assert.ok(survey.stale.includes('PreToolUse:Edit'),
    `dead-path entry must be flagged stale; stale=${JSON.stringify(survey.stale)}`);
});

test('P0-b a valid current entry on a different delivery surface is NOT stale (no ping-pong)', (t) => {
  // Present entries live at a DIFFERENT but valid, existing global-npm-shaped
  // path (files present on disk, in-place install → no version in path). The
  // desired set (PLUGIN_ROOT) differs by string but is the same current version.
  // surveyHookCoverage must NOT flag these stale, so registerHooksToSettings is
  // a no-op — the cache↔npm rewrite churn stops.
  const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-surface-'));
  t.after(() => fs.rmSync(fixture, { recursive: true, force: true }));
  const scriptsDir = path.join(fixture, '.nvm/versions/node/v24.18.0/lib/node_modules/@sdsrs/code-graph/claude-plugin/scripts');
  fs.mkdirSync(scriptsDir, { recursive: true });

  const desired = lifecycle.buildSettingsHookEntries();
  const settings = { hooks: {} };
  for (const [event, entries] of Object.entries(desired)) {
    settings.hooks[event] = entries.map(e => {
      const name = (e.hooks[0].command.match(/([a-z-]+\.js)/) || [])[1];
      const script = path.join(scriptsDir, name);
      fs.writeFileSync(script, '// present on this surface');
      return {
        description: e.description,
        matcher: e.matcher,
        hooks: [{ type: 'command', command: `if [ -f "${script}" ]; then node "${script}"; fi`, timeout: e.hooks[0].timeout }],
      };
    });
  }
  const survey = lifecycle.surveyHookCoverage(settings);
  assert.deepEqual(survey.missing, [], `nothing missing; got ${JSON.stringify(survey.missing)}`);
  assert.deepEqual(survey.stale, [], `a valid current cross-surface entry must not be stale; got ${JSON.stringify(survey.stale)}`);

  const before = JSON.stringify(settings.hooks);
  const changed = lifecycle.registerHooksToSettings(settings);
  assert.equal(changed, false, 'registerHooksToSettings must be a no-op when a valid current set is already present');
  assert.equal(JSON.stringify(settings.hooks), before, 'settings.hooks must be untouched (no ping-pong rewrite)');
});
