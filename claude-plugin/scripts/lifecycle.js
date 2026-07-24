#!/usr/bin/env node
'use strict';
const fs = require('fs');
const path = require('path');
const os = require('os');
const { claudeHome } = require('./claude-config');

const PLUGIN_ID = 'code-graph-mcp@code-graph-mcp';
const OLD_PLUGIN_IDS = [
  'code-graph@sdsrss',           // v1 legacy ID
  'code-graph@sdsrss-code-graph', // v2 legacy ID (pre-rename)
];
const MARKETPLACE_NAME = 'code-graph-mcp';
const CACHE_DIR = path.join(os.homedir(), '.cache', 'code-graph');
// Always derive from __dirname — CLAUDE_PLUGIN_ROOT env var can leak from other
// plugins when hooks run in shared process context (e.g. claude-mem-lite sets it
// to its own marketplace path, polluting all subsequent settings.json hook processes).
const PLUGIN_ROOT = path.resolve(__dirname, '..');
const MANIFEST_FILE = path.join(CACHE_DIR, 'install-manifest.json');
const REGISTRY_FILE = path.join(CACHE_DIR, 'statusline-registry.json');
// Written by the launcher's background install when ITS `npm install -g` step
// introduced the global shell + platform packages. Uninstall only removes
// global packages it can prove the plugin installed (marker present) or when
// the user passes --purge-global — a deliberate user install is never yanked.
const GLOBAL_INSTALL_MARKER = path.join(CACHE_DIR, 'global-install-marker.json');
const INSTALL_LOCK_FILE = path.join(CACHE_DIR, 'install.lock');
const SHELL_PKG = '@sdsrs/code-graph';

// Lazy resolvers — Claude Code's config dir can be overridden by CLAUDE_CONFIG_DIR
// (multi-account isolation). Re-read every call so test subprocesses with a
// different env see the right path.
function settingsPath() { return path.join(claudeHome(), 'settings.json'); }
function installedPluginsPath() { return path.join(claudeHome(), 'plugins', 'installed_plugins.json'); }
// Durable mirror outside ~/.cache/ — survives cache cleanup. Captures the
// `_previous` snapshot (pre-install statusline) and any third-party providers
// (GSD, etc.). readRegistry() self-heals from this file when primary is missing.
function providersBackupFile() { return path.join(claudeHome(), 'statusline-providers.json'); }
function pluginsCacheDir() { return path.join(claudeHome(), 'plugins', 'cache'); }

// --- Helpers ---

function readJson(filePath) {
  try { return JSON.parse(fs.readFileSync(filePath, 'utf8')); } catch { return null; }
}

function writeJsonAtomic(filePath, data) {
  const dir = path.dirname(filePath);
  fs.mkdirSync(dir, { recursive: true });
  const tmp = filePath + '.tmp.' + process.pid;
  fs.writeFileSync(tmp, JSON.stringify(data, null, 2) + '\n');
  fs.renameSync(tmp, filePath);
}

function readManifest() {
  return readJson(MANIFEST_FILE) || { version: null, config: {} };
}

function writeManifest(manifest) {
  fs.mkdirSync(CACHE_DIR, { recursive: true });
  writeJsonAtomic(MANIFEST_FILE, manifest);
}

function getPluginVersion() {
  const pj = readJson(path.join(PLUGIN_ROOT, '.claude-plugin', 'plugin.json'));
  return pj ? pj.version : '0.0.0';
}

function compositeCommand() {
  return `node ${JSON.stringify(path.join(PLUGIN_ROOT, 'scripts', 'statusline-composite.js'))}`;
}

function codeGraphStatuslineCommand() {
  return `node ${JSON.stringify(path.join(PLUGIN_ROOT, 'scripts', 'statusline.js'))}`;
}

function hasOwn(obj, key) {
  return !!obj && Object.prototype.hasOwnProperty.call(obj, key);
}

function hasInstalledPluginRecord() {
  const installed = readJson(installedPluginsPath());
  return !!(installed && installed.plugins && Array.isArray(installed.plugins[PLUGIN_ID]) && installed.plugins[PLUGIN_ID].length > 0);
}

/** The installPath Claude Code currently considers active (installed_plugins.json), or null. */
function activeInstallPath() {
  const installed = readJson(installedPluginsPath());
  const recs = installed && installed.plugins && installed.plugins[PLUGIN_ID];
  if (!Array.isArray(recs) || !recs[0] || typeof recs[0].installPath !== 'string') return null;
  return recs[0].installPath;
}

/**
 * Stale-relic detection: is THIS script running from an old plugin-cache
 * version dir while installed_plugins.json points at a different (active)
 * install that exists on disk?
 *
 * Why: a still-running Claude Code process keeps firing SessionStart from the
 * install path it loaded at startup. After auto-update installs vN+1 and
 * re-anchors manifest + settings.json, the next SessionStart in that old
 * process runs the vN scripts, whose syncLifecycleConfig sees
 * `manifest.version !== currentVersion` and — direction-blind — calls
 * update(), dragging manifest and every settings.json hook path back to the
 * vN dir. The two versions then ping-pong (observed live 2026-06-12: manifest
 * 0.49.0 → rewritten 0.48.0 fifteen minutes after a successful update).
 *
 * The authority is installed_plugins.json, not version direction: a deliberate
 * downgrade via /plugin lands installPath == the old dir, so the old scripts
 * keep full self-heal rights. Dev checkouts and npm installs are exempt
 * (pluginRoot not under the plugins cache).
 */
function isStaleRelicContext({
  pluginRoot = PLUGIN_ROOT,
  cacheRoot = pluginsCacheDir(),
  activePath = activeInstallPath(),
  existsSync = fs.existsSync,
} = {}) {
  if (!activePath) return false;
  const root = path.resolve(pluginRoot);
  const cache = path.resolve(cacheRoot);
  if (root !== cache && !root.startsWith(cache + path.sep)) return false;
  const active = path.resolve(activePath);
  if (active === root) return false;
  return existsSync(path.join(active, 'scripts', 'lifecycle.js'));
}

function isOurComposite(settings) {
  return settings.statusLine &&
    settings.statusLine.command &&
    settings.statusLine.command.includes('statusline-composite');
}

// --- StatusLine Registry ---
// Multiple providers can register. The composite script runs them all.

function readRegistry() {
  const primary = readJson(REGISTRY_FILE);
  if (primary && Array.isArray(primary) && primary.length > 0) return primary;
  // Self-heal: primary missing or empty (e.g. user cleaned ~/.cache/code-graph/).
  // Durable backup in ~/.claude/ retains `_previous` + third-party providers.
  const backup = readJson(providersBackupFile());
  if (backup && Array.isArray(backup) && backup.length > 0) {
    try { writeJsonAtomic(REGISTRY_FILE, backup); } catch { /* ok */ }
    return backup;
  }
  return [];
}

function writeRegistry(registry) {
  if (!registry || registry.length === 0) {
    try { fs.unlinkSync(REGISTRY_FILE); } catch { /* ok */ }
    try { fs.unlinkSync(providersBackupFile()); } catch { /* ok */ }
    return;
  }
  writeJsonAtomic(REGISTRY_FILE, registry);
  // Mirror to durable location so cache cleanup doesn't strand `_previous`
  // or third-party provider entries.
  try { writeJsonAtomic(providersBackupFile(), registry); } catch { /* ok */ }
}

function registerStatuslineProvider(id, command, needsStdin) {
  const registry = readRegistry();
  const idx = registry.findIndex(p => p.id === id);
  const entry = { id, command, needsStdin: !!needsStdin };
  if (idx >= 0) {
    // Update existing entry only if command changed
    if (registry[idx].command === command) return false;
    registry[idx] = entry;
  } else {
    registry.push(entry);
  }
  writeRegistry(registry);
  return true;
}

function unregisterStatuslineProvider(id) {
  const registry = readRegistry();
  const filtered = registry.filter(p => p.id !== id);
  if (filtered.length === registry.length) return false;
  writeRegistry(filtered);
  return true;
}

function isPluginExplicitlyDisabled(settings = readJson(settingsPath()) || {}) {
  return hasOwn(settings.enabledPlugins, PLUGIN_ID) && settings.enabledPlugins[PLUGIN_ID] === false;
}

function isPluginInactive(settings = readJson(settingsPath()) || {}) {
  if (isPluginExplicitlyDisabled(settings)) return true;

  const hasComposite = isOurComposite(settings);
  const hasCodeGraphRegistry = readRegistry().some((provider) => provider.id === 'code-graph');
  if (!hasComposite && !hasCodeGraphRegistry) return false;

  const installed = readJson(installedPluginsPath());
  if (!installed || !installed.plugins) return false;
  return !hasInstalledPluginRecord();
}

function detachStatuslineIntegration(settings, { compositeDoomed = true } = {}) {
  let settingsChanged = false;

  unregisterStatuslineProvider('code-graph');
  const registry = readRegistry();
  const previous = registry.find(p => p.id === '_previous' && p.command);
  // Third-party providers registered through our registry (e.g. gsd). They
  // must not be silently orphaned: with the composite gone from settings
  // their segments stop rendering while the registry entries dangle.
  const thirdParty = registry.filter(p => p.id !== '_previous' && p.id !== 'code-graph' && p.command);

  // If our composite is still configured while the plugin is disabled/uninstalled,
  // stop affecting Claude Code — but keep surviving third parties rendering.
  if (isOurComposite(settings)) {
    if (thirdParty.length > 0 && !compositeDoomed) {
      // Temporary disable: the composite script survives on disk and keeps
      // rendering the remaining providers — only our segment was unregistered.
    } else if (thirdParty.length > 0) {
      // Genuine uninstall: our composite runner dies with the plugin cache.
      // Hand the slot to the first surviving third-party provider; the rest
      // stay listed in the registry backup for manual re-wiring.
      settings.statusLine = { type: 'command', command: thirdParty[0].command };
      settingsChanged = true;
    } else if (previous) {
      settings.statusLine = { type: 'command', command: previous.command };
      settingsChanged = true;
    } else {
      delete settings.statusLine;
      settingsChanged = true;
    }
  }

  // _previous only becomes removable once no third party still relies on the
  // registry file (writeRegistry unlinks primary+backup when emptied).
  if (thirdParty.length === 0) unregisterStatuslineProvider('_previous');
  return settingsChanged;
}

function cleanupDisabledStatusline() {
  const settings = readJson(settingsPath());
  if (!settings || !isPluginInactive(settings)) {
    return { cleaned: false, settingsChanged: false };
  }

  // Decide BEFORE mutating: isPluginUninstalled reads the same composite/
  // registry markers detachStatuslineIntegration is about to remove.
  const uninstalled = isPluginUninstalled(settings);

  let settingsChanged = detachStatuslineIntegration(settings, { compositeDoomed: uninstalled });
  if (removeHooksFromSettings(settings)) settingsChanged = true;
  if (settingsChanged) {
    writeJsonAtomic(settingsPath(), settings);
  }

  // Genuine uninstall (not a temporary disable): reclaim ~/.cache/code-graph
  // too. This statusline-render path is the ONLY plugin code guaranteed to
  // still run after `/plugin uninstall` — Claude Code stops loading the
  // plugin's hooks.json, so the SessionStart teardown in session-init.js never
  // fires post-uninstall. Without this, the ~40MB cached binary leaked forever.
  let cacheRemoved = false;
  if (uninstalled) cacheRemoved = removeCacheResidue();

  return { cleaned: true, settingsChanged, cacheRemoved };
}

// --- Scope Conflict Detection ---

function checkScopeConflict() {
  const installed = readJson(installedPluginsPath());
  if (!installed || !installed.plugins) return null;
  for (const [key, entries] of Object.entries(installed.plugins)) {
    if (key === PLUGIN_ID) continue;
    // Detect any old code-graph plugin IDs still installed
    if (key.startsWith('code-graph@') || key.startsWith('code-graph-mcp@')) {
      return { existingId: key, scope: entries[0] && entries[0].scope, entries };
    }
  }
  return null;
}

// --- Migration: clean up old plugin ID remnants ---

function migrateOldPluginIds(settings) {
  let changed = false;

  for (const oldId of OLD_PLUGIN_IDS) {
    // Clean old ID from enabledPlugins
    if (settings.enabledPlugins && oldId in settings.enabledPlugins) {
      delete settings.enabledPlugins[oldId];
      changed = true;
    }

    // Clean old ID from installed_plugins.json
    const installed = readJson(installedPluginsPath());
    if (installed && installed.plugins && oldId in installed.plugins) {
      delete installed.plugins[oldId];
      writeJsonAtomic(installedPluginsPath(), installed);
    }
  }

  // Clean old marketplace names from extraKnownMarketplaces
  if (settings.extraKnownMarketplaces) {
    for (const oldName of ['sdsrss-code-graph']) {
      if (oldName in settings.extraKnownMarketplaces) {
        delete settings.extraKnownMarketplaces[oldName];
        changed = true;
      }
    }
  }

  // Clean old cache paths
  const cacheRoot = pluginsCacheDir();
  const oldCacheDirs = [
    path.join(cacheRoot, 'sdsrss', 'code-graph'),
    path.join(cacheRoot, 'sdsrss-code-graph', 'code-graph'),
    path.join(cacheRoot, 'sdsrss-code-graph'),
  ];
  for (const dir of oldCacheDirs) {
    try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* ok */ }
  }

  return changed;
}

// --- Hook identity ---
//
// v0.32.0 ARCHITECTURE CORRECTION (see project_hooks_settings.md / feedback_pretooluse_dark_under_green_health.md):
//
// Empirical finding 2026-05-24: current Claude Code only loads SessionStart
// hooks from cache/<mp>/<plugin>/<ver>/hooks/hooks.json. PreToolUse, PostToolUse,
// UserPromptSubmit, Stop, SessionEnd entries in plugin-cache hooks.json are
// SILENTLY IGNORED. Only ~/.claude/settings.json entries reach CC for those events.
//
// Therefore lifecycle.js now ACTIVELY WRITES non-SessionStart hook entries to
// settings.json (with description markers for cleanup), and the shipped
// claude-plugin/hooks/hooks.json carries only SessionStart. SessionStart entries
// in claude-plugin/hooks/hooks.json continue to be CC-loaded as before.
//
// Pattern mirrors claude-mem-lite's install.mjs (cache hooks.json cleared
// to prevent duplicate registration).

const OUR_HOOK_SCRIPTS = [
  'session-init.js',
  'incremental-index.js',
  'user-prompt-context.js',
  'pre-edit-guide.js',
  'pre-grep-guide.js',   // v0.32.0 — was in plugin-cache only, never fired
  'pre-read-guide.js',   // v0.32.0 — was in plugin-cache only, never fired
  'post-grep-inject.js', // compound-grep — PostToolUse(Bash) permission-neutral answer inject
];

// Description markers — primary cleanup discriminator (immune to env/path
// pollution per feedback_plugin_env_isolation.md). New v0.32.0 markers carry
// the version so older lifecycle.js still recognizes them as ours.
const SETTINGS_HOOK_DESC = {
  preToolUse:       '[code-graph-mcp v0.32+] PreToolUse re-routed via settings.json (cache hooks.json silently ignored for this event by current CC)',
  postToolUseEdit:  '[code-graph-mcp v0.32+] PostToolUse Write|Edit incremental-index update',
  postToolUseInject:'[code-graph-mcp v0.32+] PostToolUse Bash compound-grep answer inject (permission-neutral additionalContext)',
  userPromptSubmit: '[code-graph-mcp v0.32+] UserPromptSubmit context push',
};

const OUR_DESCRIPTIONS = [
  // Legacy v0.7.x / 0.8.x descriptions — kept so very-old installs still get cleaned up.
  'StatusLine self-heal, lifecycle sync, project map injection',
  'Auto-inject impact analysis when editing functions with 2+ callers',
  'Auto-update code graph index after file edits',
  'Inject code-graph structural context based on user intent',
  // v0.32.0 — new re-route markers
  SETTINGS_HOOK_DESC.preToolUse,
  SETTINGS_HOOK_DESC.postToolUseEdit,
  SETTINGS_HOOK_DESC.postToolUseInject,
  SETTINGS_HOOK_DESC.userPromptSubmit,
];

function isOurHookEntry(entry) {
  if (!entry || !entry.hooks) return false;
  // Primary: match by description (immune to path pollution).
  if (entry.description && OUR_DESCRIPTIONS.includes(entry.description)) return true;
  // Fallback: script basename + a delivery-surface marker in the path. TWO
  // surfaces ship these scripts: the marketplace plugin-cache (dir
  // 'code-graph-mcp') AND the global npm package (dir '@sdsrs/code-graph' — note
  // NO '-mcp' suffix). v0.32.1 tightened from bare 'code-graph' (which would
  // claim a user's own ~/code-graph/foo.js) to MARKETPLACE_NAME, but that alone
  // missed the npm-global surface, so `npm i -g`-delivered hooks were never
  // evicted and orphan-accumulated across node/version switches (RCA 2026-07-24).
  // Both markers are specific enough not to claim a user's unrelated file.
  return entry.hooks.some(h =>
    h.command && OUR_HOOK_SCRIPTS.some(s => h.command.includes(s)) &&
    (h.command.includes(MARKETPLACE_NAME) || h.command.includes(SHELL_PKG))
  );
}

function removeHooksFromSettings(settings) {
  if (!settings.hooks) return false;
  let changed = false;

  for (const event of Object.keys(settings.hooks)) {
    if (!Array.isArray(settings.hooks[event])) continue;
    const before = settings.hooks[event].length;
    settings.hooks[event] = settings.hooks[event].filter(e => !isOurHookEntry(e));
    if (settings.hooks[event].length !== before) changed = true;
    if (settings.hooks[event].length === 0) delete settings.hooks[event];
  }
  if (Object.keys(settings.hooks).length === 0) delete settings.hooks;

  return changed;
}

// --- v0.32.0: settings.json hook registration ---

// PLUGIN_ROOT (module-level, line 18) is the canonical __dirname-derived
// absolute path — never CLAUDE_PLUGIN_ROOT env (env leaks across plugins
// in settings.json hook execution context per feedback_plugin_env_isolation.md).

function buildSettingsHookEntries() {
  const root = PLUGIN_ROOT;
  const scriptCmd = (name, timeout) => {
    const script = path.join(root, 'scripts', name);
    // POSIX: existence-guarded. After `/plugin uninstall`, CC may delete the
    // plugin-cache dir before our statusline teardown gets to strip these
    // entries — in that window every Edit/Bash/Read/prompt errored on a dead
    // path. The `if` form preserves node's own exit code (PreToolUse deny =
    // exit 2); `&& … || exit 0` would swallow it. Windows keeps the bare
    // command — the hook shell there is not reliably cmd, so `if exist`
    // can't be assumed.
    const command = process.platform === 'win32'
      ? `node "${script}"`
      : `if [ -f "${script}" ]; then node "${script}"; fi`;
    return { type: 'command', command, timeout };
  };

  return {
    PreToolUse: [
      { description: SETTINGS_HOOK_DESC.preToolUse, matcher: 'Edit', hooks: [scriptCmd('pre-edit-guide.js', 4)] },
      { description: SETTINGS_HOOK_DESC.preToolUse, matcher: 'Bash', hooks: [scriptCmd('pre-grep-guide.js', 3)] },
      { description: SETTINGS_HOOK_DESC.preToolUse, matcher: 'Read', hooks: [scriptCmd('pre-read-guide.js', 3)] },
    ],
    PostToolUse: [
      { description: SETTINGS_HOOK_DESC.postToolUseEdit, matcher: 'Write|Edit', hooks: [scriptCmd('incremental-index.js', 10)] },
      { description: SETTINGS_HOOK_DESC.postToolUseInject, matcher: 'Bash', hooks: [scriptCmd('post-grep-inject.js', 5)] },
    ],
    UserPromptSubmit: [
      { description: SETTINGS_HOOK_DESC.userPromptSubmit, matcher: '', hooks: [scriptCmd('user-prompt-context.js', 5)] },
    ],
  };
}

// Idempotent two-pass: (1) evict ALL our entries (legacy v0.7+/0.8+ markers
// AND v0.32+ markers) across EVERY event — catches legacy SessionStart/
// PostToolUse entries in settings.json pointing to stale plugin-cache paths;
// (2) write fresh v0.32+ entries for the events we own. SessionStart stays
// in plugin-cache hooks.json (it's still loaded from there), so we don't
// re-write it to settings.json.
function registerHooksToSettings(settings) {
  settings.hooks = settings.hooks || {};

  // Idempotent across delivery surfaces: if every desired (event,matcher) is
  // already present exactly once, pointing at a current, existing script
  // (plugin-cache OR global-npm), do nothing. Stops the settings.json ping-pong
  // where the cache session-init and the npm-global CLI doctor each rewrote the
  // other's valid entry every run (RCA 2026-07-24). Any missing/stale/dead entry
  // — or a duplicate ( oursCount > expected) — still triggers evict+rewrite.
  const survey = surveyHookCoverage(settings);
  let oursCount = 0;
  for (const entries of Object.values(settings.hooks)) {
    if (Array.isArray(entries)) oursCount += entries.filter(isOurHookEntry).length;
  }
  if (survey.missing.length === 0 && survey.stale.length === 0
      && oursCount === survey.expected.length) {
    return false;
  }

  const before = JSON.stringify(settings.hooks);

  // Pass 1: evict our entries across every event.
  for (const event of Object.keys(settings.hooks)) {
    if (!Array.isArray(settings.hooks[event])) continue;
    settings.hooks[event] = settings.hooks[event].filter(e => !isOurHookEntry(e));
    if (settings.hooks[event].length === 0) delete settings.hooks[event];
  }

  // Pass 2: write fresh entries for our desired events.
  const desired = buildSettingsHookEntries();
  for (const [event, desiredEntries] of Object.entries(desired)) {
    const existing = Array.isArray(settings.hooks[event]) ? settings.hooks[event] : [];
    settings.hooks[event] = [...existing, ...desiredEntries];
  }

  return before !== JSON.stringify(settings.hooks);
}

// Extract the .js script path a hook command invokes — bare (`node "…"`) or
// existence-guarded (`if [ -f "…" ]; then node "…"; fi`).
function hookCmdScript(cmd) {
  const m = (cmd || '').match(/node "([^"]+\.js)"/) || (cmd || '').match(/"([^"]+\.js)"/);
  return m ? m[1] : null;
}

// Version encoded in a plugin-cache path (.../code-graph-mcp/code-graph-mcp/<ver>/scripts/…).
// Null for in-place installs (global npm), whose path never carries a version
// dir — npm overwrites the same path on upgrade, so such a path never goes
// version-stale (only dead-path-stale, caught separately by fs.existsSync).
function cacheDirVersion(scriptPath) {
  const m = (scriptPath || '').match(/\/code-graph-mcp\/code-graph-mcp\/(\d+\.\d+\.\d+[^/]*)\//);
  return m ? m[1] : null;
}

// Inventory of (event, matcher) tuples we expect to find in settings.json after
// install. Consumed by doctor (report + fix) and session-init (self-heal):
// `missing` = entry absent; `stale` = present but the registered command no
// longer matches what we'd write now (points at an old plugin-cache version
// dir / moved path). A stale path can run pre-recordRecommendation hook code,
// so the hook fires but the conversion metric stays dark — invisible to a
// present/absent check. This is the 0.45.1-registered-while-0.45.4-active
// case the RCA surfaced.
function surveyHookCoverage(settings) {
  const desired = buildSettingsHookEntries();
  const expected = [];
  const desiredCmd = {}; // key -> command string we would write now
  for (const [event, entries] of Object.entries(desired)) {
    for (const e of entries) {
      const key = `${event}:${e.matcher || '*'}`;
      expected.push(key);
      desiredCmd[key] = e.hooks && e.hooks[0] && e.hooks[0].command;
    }
  }

  const present = new Set();
  const presentCmd = {}; // key -> command currently registered
  if (settings && settings.hooks) {
    for (const [event, entries] of Object.entries(settings.hooks)) {
      if (!Array.isArray(entries)) continue;
      for (const entry of entries) {
        if (isOurHookEntry(entry)) {
          const key = `${event}:${entry.matcher || '*'}`;
          present.add(key);
          if (entry.hooks && entry.hooks[0] && entry.hooks[0].command) {
            presentCmd[key] = entry.hooks[0].command;
          }
        }
      }
    }
  }

  const { compareVersions } = require('./version-utils');
  const missing = expected.filter(k => !present.has(k));
  // Version/surface-tolerant staleness. Was an exact command-string compare,
  // which made two registration authorities (plugin-cache session-init vs
  // global-npm CLI doctor — different absolute paths) each flag the other's
  // VALID CURRENT entry stale and rewrite it → settings.json ping-pong on every
  // alternating run (RCA 2026-07-24). An entry is stale only when its script is
  // a dead path OR resolves to an OLDER plugin-cache version dir than we'd write
  // now. A present entry on a different but valid, current surface (npm in-place
  // install: file exists, no version in path) is NOT stale.
  const stale = expected.filter(k => {
    if (!present.has(k) || !presentCmd[k]) return false;
    const pScript = hookCmdScript(presentCmd[k]);
    if (!pScript) return false;
    if (!fs.existsSync(pScript)) return true;             // dead path
    const pv = cacheDirVersion(pScript);
    const dv = cacheDirVersion(hookCmdScript(desiredCmd[k]));
    if (pv && dv) return compareVersions(pv, dv) < 0;     // older cache version dir
    return false;                                         // in-place/current surface
  });
  return { expected, present: [...present], missing, stale };
}

// --- Firing self-test (v0.67.0) ---
//
// surveyHookCoverage proves a hook is WIRED; it does NOT prove the script RUNS.
// A renamed sibling module, an incompatible node, or a corrupt install leaves a
// hook registered-but-inert — invisible to every string/path check. verifyHooksFire
// spawns each registered hook the way Claude Code does (node + a synthetic CC
// stdin payload) inside a throwaway fixture and asserts it exits 0. This is the
// "does it really fire" check. What it CANNOT prove is that CC *dispatches* real
// tool-calls to it (only a live session shows that — see the Layer-B dispatch
// canary in session-init.js). Best-effort; never throws.

// Representative CC stdin payload that drives each matcher's path. The Bash
// payload is engaging (a source-tree search → a deny/hint IS emitted); the Edit
// payload's short old_string short-circuits before any binary spawn; the rest
// just exercise the require-chain + stdin parse.
function hookFirePayload(matcher) {
  switch (matcher) {
    case 'Bash':
      // A QUOTED, identifier-like pattern → classifyBlock-positive → the
      // PreToolUse deny tier emits (the hint-tier dark stdout fallthrough was
      // removed in the compound-grep change, so a non-foldable pattern would now
      // produce no output and falsely read as "didn't fire").
      return { tool_name: 'Bash', tool_input: { command: 'grep -rn "SomeUniqueSymbol" src/' } };
    case 'Read':
      return { tool_name: 'Read', tool_input: { file_path: 'src/example.rs' } };
    case 'Edit':
    case 'Write|Edit':
      return { tool_name: 'Edit', tool_input: { file_path: 'src/example.rs', old_string: 'a', new_string: 'b' } };
    case '': // UserPromptSubmit
      return { prompt: 'where is the parse function defined' };
    default:
      return { tool_name: 'Unknown', tool_input: {} };
  }
}

// The hooks CC actually loads from settings.json (PreToolUse/PostToolUse/
// UserPromptSubmit). SessionStart (hooks.json) runs every session → its own
// liveness proof → excluded here.
function defaultHookFireProbes() {
  const probes = [];
  for (const [event, entries] of Object.entries(buildSettingsHookEntries())) {
    for (const e of entries) {
      const cmd = e.hooks && e.hooks[0] && e.hooks[0].command;
      const m = (cmd || '').match(/"([^"]+\.js)"/);
      if (!m) continue;
      probes.push({ label: `${event}:${e.matcher || '*'}`, script: m[1], payload: hookFirePayload(e.matcher || '') });
    }
  }
  return probes;
}

function verifyHooksFire({ hooks, env, timeoutMs = 4000, tmpBase } = {}) {
  const { spawnSync } = require('child_process');
  const probes = hooks || defaultHookFireProbes();

  // Throwaway fixture carrying the .code-graph/index.db marker so resolveProjectRoot
  // resolves (otherwise the hooks early-return before exercising anything).
  // Base is os.tmpdir() directly (a UNIQUE mkdtemp), NOT the shared cgTmpDir()
  // subdir — a concurrent process clearing `<tmp>/code-graph-mcp` mid-run would
  // otherwise yank this fixture out from under an in-flight spawn (cwd ENOENT).
  let fixture = null;
  try {
    const base = tmpBase || os.tmpdir();
    fixture = fs.mkdtempSync(path.join(base, 'cg-hookfire-'));
    fs.mkdirSync(path.join(fixture, '.code-graph'), { recursive: true });
    fs.writeFileSync(path.join(fixture, '.code-graph', 'index.db'), '');
  } catch (e) {
    return { ok: false, results: [], error: `fixture: ${e && e.message}` };
  }

  // Force a non-silenced, no-binary-spawn firing config: the smoke tests the
  // machinery regardless of the user's silence prefs and never invokes the binary
  // (CODE_GRAPH_NO_ANSWER_IN_DENY keeps cg-answer out of the deny path).
  // Redirect the hooks' tmp state (per-command grep cooldown, read-fanout state)
  // into the throwaway fixture via TMPDIR so the smoke neither collides with a
  // real 60s cooldown (which would suppress the deny → false "didn't fire") nor
  // pollutes the user's real cooldown/state.
  const baseEnv = {
    ...process.env,
    CODE_GRAPH_QUIET_HOOKS: '0',
    CODE_GRAPH_NO_ANSWER_IN_DENY: '1',
    TMPDIR: fixture, TMP: fixture, TEMP: fixture,
    ...(env || {}),
  };

  const results = [];
  for (const h of probes) {
    let error = null, ok = false, emitted = false, code = null, signal = null;
    try {
      const r = spawnSync(process.execPath, [h.script], {
        input: JSON.stringify(h.payload || {}),
        cwd: fixture,
        env: baseEnv,
        timeout: timeoutMs,
        encoding: 'utf8',
      });
      code = r.status;
      signal = r.signal;
      ok = !r.error && r.status === 0;
      emitted = !!(r.stdout && r.stdout.trim());
      if (!ok) {
        error = r.error
          ? String(r.error.message || r.error)
          : ((r.stderr || '').trim().slice(0, 200) || `exit ${r.status}`);
      }
    } catch (e) {
      error = String((e && e.message) || e);
    }
    results.push({ label: h.label, script: h.script, ok, code, signal, emitted, error });
  }

  try { fs.rmSync(fixture, { recursive: true, force: true }); } catch { /* ok */ }

  return { ok: results.length > 0 && results.every(r => r.ok), results };
}

// --- Install (idempotent) ---

function install({ reclaimStatusline = false } = {}) {
  const version = getPluginVersion();
  const manifest = readManifest();
  const settings = readJson(settingsPath()) || {};
  let settingsChanged = false;

  // 0. Migrate from old plugin IDs
  if (migrateOldPluginIds(settings)) {
    settingsChanged = true;
  }

  // 1. StatusLine — composite approach
  //    a. Capture existing statusline as a provider (if not already composite)
  //    b. Register code-graph as a provider
  //    c. Set statusLine to composite script
  if (!isOurComposite(settings)) {
    // Displacement tracking: we held the slot before (manifest.config.statusLine)
    // but a foreign command sits there now — either another slot-claiming plugin
    // (whose own self-heal re-takes it just like ours would → statusline
    // ping-pong every session) or the user's deliberate choice. Either way,
    // silently re-claiming forever is wrong: after >2 observed displacements
    // stand down — stay registered as a provider, leave the slot alone.
    // Explicit `lifecycle.js install` (or CODE_GRAPH_FORCE_STATUSLINE=1)
    // resets the counter and re-claims.
    const currentCmd = settings.statusLine && settings.statusLine.command;
    if (reclaimStatusline || process.env.CODE_GRAPH_FORCE_STATUSLINE === '1') {
      manifest.config.statuslineDisplaced = 0;
    } else if (manifest.config.statusLine === true && currentCmd) {
      manifest.config.statuslineDisplaced = (manifest.config.statuslineDisplaced || 0) + 1;
    }
    if ((manifest.config.statuslineDisplaced || 0) > 2) {
      if (manifest.config.statusLine === true) {
        // Transition into stand-down exactly once: release claimed ownership
        // (stops the counter) and leave a breadcrumb.
        manifest.config.statusLine = false;
        process.stderr.write(
          '[code-graph] statusLine slot keeps being re-claimed by another provider — standing down.\n' +
          '            Re-claim: CODE_GRAPH_FORCE_STATUSLINE=1 or `node lifecycle.js install`\n'
        );
      }
    } else {
      // Preserve existing statusline as first provider
      if (currentCmd) {
        registerStatuslineProvider('_previous', currentCmd, true);
      }
      // Set composite as the statusLine
      settings.statusLine = { type: 'command', command: compositeCommand() };
      settingsChanged = true;
      manifest.config.statusLine = true;
    }
  } else {
    // Composite exists — ensure path is correct (may have been polluted by env leak)
    const cmd = compositeCommand();
    if (settings.statusLine.command !== cmd) {
      settings.statusLine.command = cmd;
      settingsChanged = true;
    }
    // We hold the slot — any displacement episode is over.
    if (manifest.config.statuslineDisplaced) manifest.config.statuslineDisplaced = 0;
    manifest.config.statusLine = true;
  }

  // Register code-graph provider
  registerStatuslineProvider('code-graph', codeGraphStatuslineCommand(), false);

  // 2. Hooks — v0.32.0: actively write PreToolUse/PostToolUse/UserPromptSubmit
  //    to settings.json. Plugin-cache hooks.json is silently ignored by current
  //    Claude Code for these events (SessionStart still loads from cache).
  //    registerHooksToSettings is idempotent: strips priors then appends fresh.
  const hooksRegistered = registerHooksToSettings(settings);
  if (hooksRegistered) settingsChanged = true;

  // NOTE: enabledPlugins is managed by Claude Code's plugin system, not by lifecycle.
  // Do NOT add enabledPlugins entries here — it causes phantom plugin entries
  // when the ID doesn't match the marketplace name.

  // 3. Write settings atomically if changed
  if (settingsChanged) {
    writeJsonAtomic(settingsPath(), settings);
  }

  // 4. Write manifest with version
  manifest.version = version;
  manifest.installedAt = manifest.installedAt || new Date().toISOString();
  manifest.updatedAt = new Date().toISOString();
  writeManifest(manifest);

  return { version, settingsChanged, statusLineClaimed: manifest.config.statusLine, hooksRegistered };
}

// --- Uninstall (clean all config) ---

/** Which of our npm packages exist at a global top level right now. */
function installedGlobalPkgs() {
  const { globalNodeModulesCandidates, PLATFORM_PKG } = require('./find-binary');
  const found = [];
  for (const name of [SHELL_PKG, PLATFORM_PKG]) {
    for (const root of globalNodeModulesCandidates()) {
      if (fs.existsSync(path.join(root, name, 'package.json'))) { found.push(name); break; }
    }
  }
  return found;
}

function defaultRunNpm(args) {
  const { spawnSync } = require('child_process');
  const { npmSpawnOpts } = require('./npm-exec');
  try {
    const r = spawnSync('npm', args, npmSpawnOpts({
      timeout: 120000, stdio: 'pipe', encoding: 'utf8',
    }));
    return !r.error && r.status === 0;
  } catch { return false; }
}

function uninstall({ purgeGlobal = false, unadoptAll = false, runNpm = defaultRunNpm, scanGlobalPkgs = installedGlobalPkgs } = {}) {
  const settings = readJson(settingsPath());
  let settingsChanged = false;

  if (settings) {
    // 1. StatusLine: remove code-graph integration and restore prior statusline.
    if (detachStatuslineIntegration(settings)) {
      settingsChanged = true;
    }

    // 2. Hooks: remove from settings.json
    if (removeHooksFromSettings(settings)) {
      settingsChanged = true;
    }

    // 3. Remove all known IDs from enabledPlugins
    if (settings.enabledPlugins) {
      for (const id of [PLUGIN_ID, ...OLD_PLUGIN_IDS]) {
        if (id in settings.enabledPlugins) {
          delete settings.enabledPlugins[id];
          settingsChanged = true;
        }
      }
    }

    // 4. Write settings if changed
    if (settingsChanged) {
      writeJsonAtomic(settingsPath(), settings);
    }
  }

  // 5. Remove all known IDs from installed_plugins.json
  const installedPlugins = readJson(installedPluginsPath());
  if (installedPlugins && installedPlugins.plugins) {
    let ipChanged = false;
    for (const id of [PLUGIN_ID, ...OLD_PLUGIN_IDS]) {
      if (id in installedPlugins.plugins) {
        delete installedPlugins.plugins[id];
        ipChanged = true;
      }
    }
    if (ipChanged) writeJsonAtomic(installedPluginsPath(), installedPlugins);
  }

  // 5.5. Global npm packages + adoption inventory — read BEFORE step 6 wipes
  // CACHE_DIR (both the install marker and the adopted-projects registry live
  // there). The launcher's background install runs `npm install -g` on the
  // user's behalf; nothing on the Claude Code uninstall path ever removes those
  // packages (~40MB platform binary + CLI shim left on PATH forever).
  const pluginInstalledGlobals = !!readJson(GLOBAL_INSTALL_MARKER);
  let adoptedProjects = [];
  try { adoptedProjects = require('./adopt').readAdoptedProjects(); } catch { /* POSIX-only helper — ok */ }

  // 5.4. --unadopt-all: sweep every registered project's managed CLAUDE.md
  // block + generated detail file (unadopt is marker-guarded, so user files
  // are never touched; the .code-graph/ index dir is project DATA and stays —
  // its removal is listed in the guidance instead of automated).
  const unadopted = [];
  if (unadoptAll && adoptedProjects.length) {
    let unadoptFn = null;
    try { unadoptFn = require('./adopt').unadopt; } catch { /* POSIX-only — skip */ }
    if (unadoptFn) {
      for (const project of adoptedProjects) {
        try {
          const r = unadoptFn({ cwd: project });
          unadopted.push({ project, ok: !!(r && r.ok), cleaned: !!(r && (r.blockPruned || r.fileRemoved || r.claudeMdRemoved)) });
        } catch (e) {
          unadopted.push({ project, ok: false, error: (e && e.message) || String(e) });
        }
      }
      try { adoptedProjects = require('./adopt').readAdoptedProjects(); } catch { /* ok */ }
    }
  }
  let globalPkgsRemoved = [];
  let globalPkgsRemaining = scanGlobalPkgs();
  if (globalPkgsRemaining.length && (pluginInstalledGlobals || purgeGlobal)) {
    if (runNpm(['uninstall', '-g', ...globalPkgsRemaining])) {
      globalPkgsRemoved = globalPkgsRemaining;
      globalPkgsRemaining = scanGlobalPkgs(); // re-scan: report only what actually survived
    }
  }

  // 6. Remove cache directory
  try { fs.rmSync(CACHE_DIR, { recursive: true, force: true }); } catch { /* ok */ }

  // 7. Remove plugin files from cache (all known paths, including parent dirs)
  const cacheRoot = pluginsCacheDir();
  const pluginCacheDirs = [
    path.join(cacheRoot, MARKETPLACE_NAME),
    path.join(cacheRoot, 'sdsrss-code-graph'),
    path.join(cacheRoot, 'sdsrss', 'code-graph'),
  ];
  for (const dir of pluginCacheDirs) {
    try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* ok */ }
  }

  return { settingsChanged, pluginInstalledGlobals, globalPkgsRemoved, globalPkgsRemaining, adoptedProjects, unadopted };
}

// --- Update (refresh config points) ---

function update() {
  const version = getPluginVersion();
  const manifest = readManifest();
  const oldVersion = manifest.version;
  const settings = readJson(settingsPath()) || {};
  let settingsChanged = false;

  // 0. Migrate from old plugin IDs
  if (migrateOldPluginIds(settings)) {
    settingsChanged = true;
  }

  // 1. Update composite command path if version changed
  if (isOurComposite(settings)) {
    const cmd = compositeCommand();
    if (settings.statusLine.command !== cmd) {
      settings.statusLine.command = cmd;
      settingsChanged = true;
    }
  }

  // 2. Update code-graph provider in registry
  registerStatuslineProvider('code-graph', codeGraphStatuslineCommand(), false);

  // 3. Hooks — v0.32.0: register PreToolUse/PostToolUse/UserPromptSubmit in
  //    settings.json (idempotent; absolute paths re-anchor on every update).
  const hooksRegistered = registerHooksToSettings(settings);
  if (hooksRegistered) settingsChanged = true;

  // NOTE: enabledPlugins is managed by Claude Code's plugin system, not by lifecycle.

  // 4. Write settings if changed
  if (settingsChanged) {
    writeJsonAtomic(settingsPath(), settings);
  }

  // 5. Clear update-check cache (force re-check after update)
  const updateCache = path.join(CACHE_DIR, 'update-check');
  try { fs.unlinkSync(updateCache); } catch { /* ok */ }

  // 6. Update manifest
  manifest.version = version;
  manifest.updatedAt = new Date().toISOString();
  writeManifest(manifest);

  // 7. Clean up old cached versions (keep the newest few). NOTE: older cache
  //    dirs are NOT always inert — a running MCP server's launcher path
  //    (<version>/scripts/mcp-launcher.js) is resolved + cached by Claude Code
  //    for the whole session, so pruning the version a live process is bound to
  //    breaks `/mcp` reconnect with -32000 (MODULE_NOT_FOUND). cleanupOldCacheVersions
  //    therefore skips any version still referenced by a live process cmdline.
  cleanupOldCacheVersions(5);

  return { oldVersion, version, settingsChanged, hooksRegistered };
}

/**
 * Remove old plugin cache versions, keeping the N most recent.
 * Cache layout: ~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/
 */
function cleanupOldCacheVersions(
  keep = 5,
  getActiveCmdlines = readActiveProcessCmdlines,
  cacheParent = path.join(pluginsCacheDir(), MARKETPLACE_NAME),
) {
  // Command lines of all live processes — used by the in-use guard below to
  // avoid deleting a version a running MCP server is still bound to. Failure to
  // enumerate ⇒ [] ⇒ pruning falls back to recency-only (pre-guard behavior).
  let cmdlines;
  try { cmdlines = getActiveCmdlines() || []; } catch { cmdlines = []; }
  try {
    // List all subdirectories under the marketplace cache
    const entries = fs.readdirSync(cacheParent, { withFileTypes: true });
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      const pluginDir = path.join(cacheParent, entry.name);
      try {
        const versions = fs.readdirSync(pluginDir, { withFileTypes: true })
          .filter(d => d.isDirectory())
          .map(d => ({
            name: d.name,
            path: path.join(pluginDir, d.name),
            mtime: fs.statSync(path.join(pluginDir, d.name)).mtimeMs,
          }))
          .sort((a, b) => b.mtime - a.mtime); // newest first

        if (versions.length <= keep) continue;

        for (const v of versions.slice(keep)) {
          // In-use guard: never delete a version dir a live process is running
          // from. Claude Code caches the resolved launcher path
          // (<version>/scripts/mcp-launcher.js) for the session; deleting that
          // dir breaks `/mcp` reconnect with -32000. Trailing separator stops
          // `0.8` from matching `0.80.x`.
          if (cmdlines.some(c => c.includes(v.path + path.sep))) continue;
          try {
            fs.rmSync(v.path, { recursive: true, force: true });
          } catch { /* permission error — skip */ }
        }
      } catch { /* can't read plugin dir — skip */ }
    }
  } catch { /* cache dir doesn't exist — nothing to clean */ }
}

/**
 * Best-effort list of running process command lines, for cleanupOldCacheVersions'
 * in-use guard. Linux reads /proc/<pid>/cmdline; macOS/BSD shells out to `ps`;
 * any other platform or failure returns [] (pruning then falls back to
 * recency-only — the same behavior as before the guard existed).
 */
function readActiveProcessCmdlines() {
  try {
    if (process.platform === 'linux' && fs.existsSync('/proc')) {
      const out = [];
      for (const pid of fs.readdirSync('/proc')) {
        if (!/^\d+$/.test(pid)) continue;
        try {
          const raw = fs.readFileSync(path.join('/proc', pid, 'cmdline'), 'utf8');
          if (raw) out.push(raw.replace(/\0/g, ' '));
        } catch { /* pid exited or unreadable — skip */ }
      }
      return out;
    }
  } catch { /* fall through to ps */ }
  try {
    const { execFileSync } = require('child_process');
    return execFileSync('ps', ['-axww', '-o', 'command='], {
      encoding: 'utf8', maxBuffer: 8 * 1024 * 1024,
    }).split('\n').filter(Boolean);
  } catch { /* unsupported platform — caller falls back to recency-only */ }
  return [];
}

// --- Health Check ---
// Validates all registered paths in settings.json point to existing scripts.
// Returns { healthy, issues, repaired, remaining }.
//   issues:    pre-repair detection list (what was wrong on entry)
//   repaired:  true only after a post-repair re-scan returned zero issues
//              (was previously set blindly to true whenever install() ran,
//              which would lie if install() couldn't actually fix something)
//   remaining: post-repair detection list — present iff install() was invoked;
//              empty array means repair succeeded

function scanForBrokenPaths() {
  const settings = readJson(settingsPath()) || {};
  const issues = [];

  // Check statusLine path
  if (isOurComposite(settings)) {
    const m = settings.statusLine.command.match(/node\s+"([^"]+)"/);
    if (m && m[1] && !fs.existsSync(m[1])) {
      issues.push({ type: 'statusLine', path: m[1] });
    }
  }

  // Check hook paths
  if (settings.hooks) {
    for (const [event, entries] of Object.entries(settings.hooks)) {
      if (!Array.isArray(entries)) continue;
      for (const entry of entries) {
        if (!isOurHookEntry(entry) || !entry.hooks) continue;
        for (const h of entry.hooks) {
          const m = h.command && h.command.match(/node\s+"([^"]+)"/);
          if (m && m[1] && !fs.existsSync(m[1])) {
            issues.push({ type: 'hook', event, path: m[1] });
          }
        }
      }
    }
  }

  // Check registry paths
  const registry = readRegistry();
  for (const provider of registry) {
    if (provider.id === '_previous') continue;
    const m = provider.command && provider.command.match(/node\s+"([^"]+)"/);
    if (m && m[1] && !fs.existsSync(m[1])) {
      issues.push({ type: 'registry', id: provider.id, path: m[1] });
    }
  }

  return issues;
}

function healthCheck() {
  const issues = scanForBrokenPaths();

  if (issues.length === 0) {
    return { healthy: true, issues, repaired: false };
  }

  // Attempt auto-repair, then re-scan to confirm the issues actually went
  // away. install() may legitimately fail to resolve a problem (binary path
  // permanently gone, registry corrupted, etc.) and the previous code lied
  // by always returning repaired:true.
  install();
  const remaining = scanForBrokenPaths();
  return {
    healthy: false,
    issues,
    repaired: remaining.length === 0,
    remaining,
  };
}

// True when the plugin has been UNINSTALLED (removed from installed_plugins.json),
// as opposed to merely toggled OFF (isPluginExplicitlyDisabled — the user may
// re-enable). The distinction matters because the uninstall teardown below is
// destructive (deletes the cached binary, unwinds project adoption); doing that
// on a temporary disable would force a re-download + re-adopt on re-enable.
function isPluginUninstalled(settings = readJson(settingsPath()) || {}) {
  if (isPluginExplicitlyDisabled(settings)) return false;
  return isPluginInactive(settings);
}

// Remove the ~/.cache/code-graph residue (the ~40MB binary, update-state,
// statusline-registry, install-manifest). The settings-only self-heal
// (cleanupDisabledStatusline) leaves this behind; the SessionStart teardown calls
// this so a CC `/plugin uninstall` (which fires no uninstall hook) still reclaims
// the disk. Idempotent: rm is force, so repeat SessionStarts are no-ops. Does NOT
// touch the plugin-cache script dirs — those are CC-managed and may be executing.
function removeCacheResidue() {
  try { fs.rmSync(CACHE_DIR, { recursive: true, force: true }); return true; }
  catch { return false; }
}

module.exports = {
  install, uninstall, update, healthCheck, scanForBrokenPaths, checkScopeConflict,
  isPluginExplicitlyDisabled, isPluginInactive, isPluginUninstalled, removeCacheResidue,
  cleanupDisabledStatusline,
  readManifest, readJson, writeJsonAtomic,
  readRegistry, writeRegistry,
  getPluginVersion, cleanupOldCacheVersions,
  removeHooksFromSettings, isOurHookEntry,
  registerHooksToSettings, buildSettingsHookEntries,                  // v0.32.0
  surveyHookCoverage, compositeCommand,                                // v0.49.1 — version-aware self-heal
  verifyHooksFire, defaultHookFireProbes,                              // v0.67.0 — firing self-test
  activeInstallPath, isStaleRelicContext,                              // v0.49.1 — stale-relic downgrade guard
  SETTINGS_HOOK_DESC, OUR_HOOK_SCRIPTS, OUR_DESCRIPTIONS,              // v0.32.0 — for tests
  PLUGIN_ROOT,                                                         // v0.32.1 — for tests / consumers
  registerStatuslineProvider, unregisterStatuslineProvider,
  installedGlobalPkgs, GLOBAL_INSTALL_MARKER, INSTALL_LOCK_FILE, SHELL_PKG,   // uninstall residue
  PLUGIN_ID, OLD_PLUGIN_IDS, MARKETPLACE_NAME, CACHE_DIR, REGISTRY_FILE,
  settingsPath, installedPluginsPath, providersBackupFile, pluginsCacheDir,
};

// CLI: node lifecycle.js <install|uninstall|update|health>
if (require.main === module) {
  const cmd = process.argv[2];
  if (cmd === 'install') {
    // Explicit CLI install = user intent: reset any statusline stand-down and re-claim.
    const r = install({ reclaimStatusline: true });
    console.log(`Installed v${r.version} | settings=${r.settingsChanged} | statusLine=${r.statusLineClaimed}`);
  } else if (cmd === 'uninstall') {
    const r = uninstall({
      purgeGlobal: process.argv.includes('--purge-global'),
      unadoptAll: process.argv.includes('--unadopt-all'),
    });
    console.log(`Uninstalled | settings cleaned=${r.settingsChanged}`);
    if (r.unadopted.length) {
      const cleaned = r.unadopted.filter((u) => u.cleaned).length;
      console.log(`  Unadopted ${cleaned}/${r.unadopted.length} registered project(s):`);
      for (const u of r.unadopted) {
        console.log(`    ${u.ok ? (u.cleaned ? 'cleaned' : 'nothing-to-clean') : `FAILED (${u.error || 'unknown'})`}  ${u.project}`);
      }
      console.log('    Their .code-graph/ index dirs are project data — remove per project with `rm -rf .code-graph` if unwanted.');
    }
    if (r.globalPkgsRemoved.length) {
      console.log(`  Removed global npm package(s): ${r.globalPkgsRemoved.join(', ')}`);
    }
    if (r.globalPkgsRemaining.length) {
      console.log(`  Global npm package(s) still installed: ${r.globalPkgsRemaining.join(', ')}`);
      console.log(`    Remove with: npm uninstall -g ${r.globalPkgsRemaining.join(' ')}`);
      if (!r.pluginInstalledGlobals) {
        console.log('    (left in place: no plugin-install marker, so they may be your own install; --purge-global forces removal)');
      }
    }
    if (r.adoptedProjects.length) {
      console.log('  Adopted project(s) still carrying a managed CLAUDE.md block + .code-graph/ index:');
      for (const p of r.adoptedProjects) console.log(`    ${p}`);
      console.log('    Clean all at once: re-run with --unadopt-all, or per project `code-graph-mcp unadopt` + `rm -rf .code-graph`.');
    }
    console.log('  Note: also run `/plugin uninstall code-graph-mcp` inside Claude Code to sync its UI state.');
  } else if (cmd === 'update') {
    const r = update();
    console.log(`Updated ${r.oldVersion} → ${r.version} | settings=${r.settingsChanged}`);
  } else if (cmd === 'health') {
    const r = healthCheck();
    if (r.healthy) {
      console.log('Health: OK — all paths valid');
    } else {
      console.log(`Health: ${r.issues.length} issue(s) found${r.repaired ? ' — repaired' : ''}`);
      for (const issue of r.issues) {
        console.log(`  ${issue.type}: ${issue.path || issue.id}`);
      }
    }
  } else if (cmd === 'doctor') {
    const { runDoctor } = require('./doctor');
    const checkOnly = process.argv.includes('--check-only');
    // Exit on issues that remain UNRESOLVED after repair, not issues found —
    // a run that fixes everything exits 0. See unresolvedCount in doctor.js.
    const { unresolved } = runDoctor({ checkOnly });
    process.exit(unresolved > 0 ? 1 : 0);
  } else if (cmd === 'verify-hooks-fire') {
    // v0.67.0 — Layer-A firing self-test. Spawned detached by session-init
    // (off the SessionStart budget); writes a small state file the next
    // SessionStart reads to surface failures.
    const res = verifyHooksFire();
    const failures = res.results.filter(r => !r.ok).map(r => r.label);
    try {
      fs.mkdirSync(CACHE_DIR, { recursive: true });
      writeJsonAtomic(path.join(CACHE_DIR, 'hook-fire-state.json'),
        { ts: new Date().toISOString(), ok: res.ok, failures });
    } catch { /* best-effort telemetry */ }
    console.log(`Hook firing: ${res.ok ? 'OK' : 'FAIL'} (${res.results.length} probed${failures.length ? ', failed: ' + failures.join(', ') : ''})`);
  } else {
    console.error('Usage: lifecycle.js <install|uninstall|update|health|doctor|verify-hooks-fire>');
    process.exit(1);
  }
}
