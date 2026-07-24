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

// --- Field-shape smoke: the full 2026-07-24 incident state in one settings.json

test('smoke: mixed stale plugin-cache + dual-node bare npm orphans converge to exactly one current set', () => {
  // The exact accumulated shape observed in the field before v0.104.0: an old
  // plugin-cache version's guarded entries AND bare global-npm entries from two
  // nvm node versions, all coexisting — every hook fired 2-3x. One registration
  // pass must converge to exactly one current entry per desired (event,matcher),
  // and must not touch entries that are not ours.
  const staleCache = (name) => ({
    matcher: 'Edit',
    hooks: [{ type: 'command', command: `if [ -f "/home/u/.claude/plugins/cache/code-graph-mcp/code-graph-mcp/0.103.0/scripts/${name}" ]; then node "/home/u/.claude/plugins/cache/code-graph-mcp/code-graph-mcp/0.103.0/scripts/${name}"; fi` }],
  });
  const bareNpm = (nodeVer, name) => ({
    matcher: 'Edit',
    hooks: [{ type: 'command', command: `node "/home/u/.nvm/versions/node/${nodeVer}/lib/node_modules/@sdsrs/code-graph/claude-plugin/scripts/${name}"` }],
  });
  const foreign = {
    matcher: 'Edit',
    hooks: [{ type: 'command', command: 'node "/home/u/.claude/plugins/cache/other-vendor/other-plugin/1.0.0/scripts/pre-edit.js"' }],
  };

  const settings = { hooks: {
    PreToolUse: [
      staleCache('pre-edit-guide.js'),
      bareNpm('v24.11.1', 'pre-edit-guide.js'),
      bareNpm('v24.18.0', 'pre-edit-guide.js'),
      foreign,
    ],
    PostToolUse: [
      { matcher: 'Write|Edit', hooks: [{ type: 'command', command: 'node "/home/u/.nvm/versions/node/v24.11.1/lib/node_modules/@sdsrs/code-graph/claude-plugin/scripts/incremental-index.js"' }] },
    ],
    UserPromptSubmit: [
      { matcher: '', hooks: [{ type: 'command', command: 'if [ -f "/home/u/.claude/plugins/cache/code-graph-mcp/code-graph-mcp/0.103.0/scripts/user-prompt-context.js" ]; then node "/home/u/.claude/plugins/cache/code-graph-mcp/code-graph-mcp/0.103.0/scripts/user-prompt-context.js"; fi' }] },
    ],
  } };

  const changed = lifecycle.registerHooksToSettings(settings);
  assert.equal(changed, true, 'a mixed stale/orphan state must trigger a rewrite');

  const desired = lifecycle.buildSettingsHookEntries();
  for (const [event, desiredEntries] of Object.entries(desired)) {
    for (const d of desiredEntries) {
      const script = (d.hooks[0].command.match(/([a-z-]+\.js)/) || [])[1];
      const matches = (settings.hooks[event] || []).filter(e =>
        (e.hooks || []).some(h => h.command.includes(script)));
      assert.equal(matches.length, 1,
        `${event}/${script}: expected exactly 1 entry after convergence, got ${matches.length}`);
      assert.ok(matches[0].hooks[0].command.includes(d.hooks[0].command.match(/"([^"]+)"/)[1]),
        `${event}/${script}: surviving entry must point at the current PLUGIN_ROOT script`);
    }
  }

  const oursTotal = Object.values(settings.hooks).flat()
    .filter(e => lifecycle.isOurHookEntry(e)).length;
  const expectedTotal = Object.values(desired).flat().length;
  assert.equal(oursTotal, expectedTotal,
    `exactly ${expectedTotal} of our entries must remain, got ${oursTotal}`);

  const foreignSurvivors = (settings.hooks.PreToolUse || []).filter(e =>
    (e.hooks || []).some(h => h.command.includes('other-vendor')));
  assert.equal(foreignSurvivors.length, 1, 'a foreign plugin hook must survive untouched');

  // Idempotence: a second pass over the converged state is a no-op.
  const after = JSON.stringify(settings.hooks);
  assert.equal(lifecycle.registerHooksToSettings(settings), false,
    'second registration over a converged state must be a no-op');
  assert.equal(JSON.stringify(settings.hooks), after, 'converged state must be stable');
});
