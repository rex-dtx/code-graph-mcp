#!/usr/bin/env node
'use strict';
const { execFileSync } = require('child_process');
const fs = require('fs');
const https = require('https');
const crypto = require('crypto');
const path = require('path');
const os = require('os');
const { CACHE_DIR, PLUGIN_ID, MARKETPLACE_NAME, readManifest, readJson, writeJsonAtomic, installedPluginsPath, pluginsCacheDir } = require('./lifecycle');
const { claudeHome } = require('./claude-config');
const { clearCache: clearBinaryCache } = require('./find-binary');
const { readBinaryVersion, isDevMode } = require('./version-utils');
const { cgTmpDir } = require('./tmp-dir');

// ── Environment Checks ────────────────────────────────────

/**
 * Check if a command-line tool is available on the system PATH.
 * @param {string} cmd - Command name (e.g., 'curl', 'tar')
 * @returns {boolean}
 */
function commandExists(cmd) {
  try {
    const whichCmd = process.platform === 'win32' ? 'where' : 'which';
    execFileSync(whichCmd, [cmd], { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

// ── Configuration ──────────────────────────────────────────
const GITHUB_REPO = 'sdsrss/code-graph-mcp';
const STATE_FILE = path.join(CACHE_DIR, 'update-state.json');
const BINARY_CACHE_DIR = path.join(CACHE_DIR, 'bin');
const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;        // 6h
const RATE_LIMIT_INTERVAL_MS = 24 * 60 * 60 * 1000;  // 24h if rate-limited
const FETCH_TIMEOUT_MS = 3000;

function isSilentMode(argv = process.argv.slice(2), env = process.env) {
  return argv.includes('--silent') || env.CODE_GRAPH_AUTO_UPDATE_SILENT === '1';
}

function isInstallMissingMode(argv = process.argv.slice(2)) {
  return argv.includes('--install-missing');
}

// ── Platform → GitHub release asset name mapping ──────────
function getPlatformAssetName() {
  const platform = os.platform();
  const arch = os.arch();
  const key = `${platform}-${arch}`;
  const map = {
    'linux-x64': 'code-graph-mcp-linux-x64',
    'linux-arm64': 'code-graph-mcp-linux-arm64',
    'darwin-x64': 'code-graph-mcp-darwin-x64',
    'darwin-arm64': 'code-graph-mcp-darwin-arm64',
    'win32-x64': 'code-graph-mcp-win32-x64.exe',
  };
  return map[key] || null;
}

// ── State Persistence ──────────────────────────────────────

function readState() {
  return readJson(STATE_FILE) || {};
}

function saveState(state) {
  try {
    writeJsonAtomic(STATE_FILE, state);
  } catch { /* ok */ }
}

// ── Throttle ───────────────────────────────────────────────

function shouldCheck(state) {
  if (!state.lastCheck) return true;
  const elapsed = Date.now() - new Date(state.lastCheck).getTime();
  const interval = state.rateLimited ? RATE_LIMIT_INTERVAL_MS : CHECK_INTERVAL_MS;
  return elapsed >= interval;
}

// ── Version Comparison (semver) ────────────────────────────

function compareVersions(a, b) {
  const pa = a.split('.').map(Number);
  const pb = b.split('.').map(Number);
  for (let i = 0; i < 3; i++) {
    if ((pa[i] || 0) > (pb[i] || 0)) return 1;
    if ((pa[i] || 0) < (pb[i] || 0)) return -1;
  }
  return 0;
}

// ── GitHub API ─────────────────────────────────────────────

function requestJson(url, timeoutMs = FETCH_TIMEOUT_MS) {
  return new Promise((resolve, reject) => {
    const req = https.request(url, {
      method: 'GET',
      headers: {
        'Accept': 'application/vnd.github+json',
        'User-Agent': 'code-graph-auto-update/1.0',
      },
    }, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => { body += chunk; });
      res.on('end', () => {
        if (!res.statusCode) {
          reject(new Error('missing status code'));
          return;
        }
        resolve({ statusCode: res.statusCode, body });
      });
    });

    req.setTimeout(timeoutMs, () => req.destroy(new Error('request timeout')));
    req.on('error', reject);
    req.end();
  });
}

function parseLatestRelease(data, assetName = getPlatformAssetName()) {
  if (!data || typeof data.tag_name !== 'string' || typeof data.tarball_url !== 'string') {
    return null;
  }

  let binaryUrl = null;
  if (assetName && Array.isArray(data.assets)) {
    const asset = data.assets.find((entry) => entry && entry.name === assetName);
    if (asset && typeof asset.browser_download_url === 'string') {
      binaryUrl = asset.browser_download_url;
    }
  }

  return {
    version: data.tag_name.replace(/^v/, ''),
    tarballUrl: data.tarball_url,
    binaryUrl,
  };
}

async function fetchLatestRelease(requestJsonFn = requestJson) {
  const url = `https://api.github.com/repos/${GITHUB_REPO}/releases/latest`;
  try {
    const res = await requestJsonFn(url, FETCH_TIMEOUT_MS);

    if (res.statusCode === 403) {
      const state = readState();
      saveState({ ...state, rateLimited: true });
      return null;
    }
    if (res.statusCode < 200 || res.statusCode >= 300) return null;

    const data = JSON.parse(res.body);
    return parseLatestRelease(data);
  } catch { return null; }
}

// ── Helpers ────────────────────────────────────────────────

function copyDirSync(src, dst) {
  fs.mkdirSync(dst, { recursive: true });
  for (const entry of fs.readdirSync(src, { withFileTypes: true })) {
    const srcPath = path.join(src, entry.name);
    const dstPath = path.join(dst, entry.name);
    if (entry.isDirectory()) {
      copyDirSync(srcPath, dstPath);
    } else {
      fs.copyFileSync(srcPath, dstPath);
    }
  }
}

function getExtractedPluginVersion(pluginSrc) {
  const manifest = readJson(path.join(pluginSrc, '.claude-plugin', 'plugin.json'));
  return manifest && typeof manifest.version === 'string' ? manifest.version : null;
}

function cachedBinaryPath() {
  const name = os.platform() === 'win32' ? 'code-graph-mcp.exe' : 'code-graph-mcp';
  return path.join(BINARY_CACHE_DIR, name);
}

/**
 * Decide whether the cached native binary must be (re)downloaded: true when it
 * is missing OR its actual version differs from the latest release. Version-aware
 * rather than existence-only — a stale-but-present binary must still self-heal
 * even when the plugin shell version already matches latest. manifest.version
 * tracks the plugin shell (the marketplace bumps it independently of the native
 * binary), so an existence-only check leaves the engine permanently pinned to an
 * old binary while the updater reports "up to date".
 */
function cachedBinaryNeedsUpdate(latest, { binaryPath = cachedBinaryPath(), readVersion = readBinaryVersion } = {}) {
  if (!latest || !latest.binaryUrl) return false;
  if (!fs.existsSync(binaryPath)) return true;
  return readVersion(binaryPath) !== latest.version;
}

/**
 * Throttle-bypass predicate: is a *present* cached binary stale relative to the
 * last known latest release (`state.latestVersion`, set on the previous fetch —
 * no network here)? Used so a present-but-stale binary skips the time-based
 * throttle instead of staying pinned for up to a full check interval. Returns
 * false when there is no prior latestVersion (first run fetches anyway) or the
 * binary is missing (handled by the separate `binaryMissing` bypass).
 */
function cachedBinaryStaleVsState(state, { binaryPath = cachedBinaryPath(), readVersion = readBinaryVersion } = {}) {
  if (!state || !state.latestVersion) return false;
  if (!fs.existsSync(binaryPath)) return false;
  return readVersion(binaryPath) !== state.latestVersion;
}

/**
 * Download just the platform binary from a GitHub release into the cache.
 * Used in two paths:
 *   1. As part of `downloadAndInstall` after a plugin tarball update.
 *   2. As a standalone self-heal when the cached binary is missing but the
 *      installed plugin version already matches `latest` (e.g. previous
 *      download failed silently, cache was wiped, optionalDependency
 *      install dropped the platform package).
 *
 * Returns true on successful promote, false otherwise. Never throws.
 */
async function downloadBinary(latest) {
  if (!latest || !latest.binaryUrl) return false;
  if (!commandExists('curl')) {
    console.error('[code-graph] Binary download skipped: curl not on PATH.');
    return false;
  }

  const binaryDst = cachedBinaryPath();
  const binaryTmp = binaryDst + '.tmp.' + process.pid;

  try {
    fs.mkdirSync(BINARY_CACHE_DIR, { recursive: true });
    execFileSync('curl', [
      '-sL', '-o', binaryTmp,
      latest.binaryUrl,
    ], { timeout: 60000, stdio: 'pipe' });

    // Best-effort fetch of the integrity sidecar (<asset>.sha256). curl -f makes
    // a 404 (an older release with no sidecar) fail → expectedSha stays null →
    // promoteVerifiedBinary takes the TOFU path. Same-origin, so this guards
    // corruption in transit, not a release-asset swap (version-exec is the
    // backstop there).
    let expectedSha = null;
    const shaTmp = binaryTmp + '.sha256';
    try {
      execFileSync('curl', ['-sfL', '-o', shaTmp, latest.binaryUrl + '.sha256'],
        { timeout: 30000, stdio: 'pipe' });
      expectedSha = (fs.readFileSync(shaTmp, 'utf8').trim().split(/\s+/)[0]) || null;
    } catch { /* no sidecar → TOFU */ } finally {
      try { if (fs.existsSync(shaTmp)) fs.unlinkSync(shaTmp); } catch { /* ok */ }
    }

    return promoteVerifiedBinary(binaryTmp, binaryDst, latest.version, expectedSha);
  } catch (e) {
    console.error(`[code-graph] Binary download failed: ${e.message}`);
    return false;
  }
}

/**
 * Hex sha256 of a file's contents (lowercase).
 * @param {string} filePath
 * @returns {string}
 */
function sha256File(filePath) {
  return crypto.createHash('sha256').update(fs.readFileSync(filePath)).digest('hex');
}

function promoteVerifiedBinary(binaryTmp, binaryDst, expectedVersion, expectedSha256) {
  try {
    const stat = fs.statSync(binaryTmp);
    if (stat.size <= 1_000_000) return false;

    // Integrity gate BEFORE the file is made executable or run, so a corrupted
    // or tampered download is never exec'd. The published <asset>.sha256 sidecar
    // is same-origin, so this defends transit/CDN corruption + truncation, not a
    // full release compromise (an attacker swapping the binary swaps the sidecar
    // too — the version-exec check below is the backstop there). No sidecar
    // (older release) → warn + proceed (TOFU), preserving the size + version
    // gates. Mirrors the snapshot checksum convention (src/snapshot/install.rs).
    if (expectedSha256) {
      const actualSha = sha256File(binaryTmp);
      if (actualSha.toLowerCase() !== String(expectedSha256).toLowerCase()) {
        console.error(`[code-graph] Binary checksum mismatch (sha256): expected ${expectedSha256}, got ${actualSha} — refusing to install.`);
        return false;
      }
    } else {
      console.error('[code-graph] No binary checksum sidecar found — content not verified (size + version checks still apply).');
    }

    // chmod BEFORE reading the version. readBinaryVersion executes the binary
    // (`--version`), which requires the exec bit; `curl -o` writes the tmp file
    // as 0644 (no exec bit), so reading the version first fails with EACCES →
    // null → false, which silently wedged every download path. rename preserves
    // the mode, so the promoted dst ends up 0755.
    if (os.platform() !== 'win32') {
      fs.chmodSync(binaryTmp, 0o755);
    }

    const actualVersion = readBinaryVersion(binaryTmp);
    if (!actualVersion || (expectedVersion && actualVersion !== expectedVersion)) {
      return false;
    }

    fs.renameSync(binaryTmp, binaryDst);
    clearBinaryCache();
    return true;
  } catch {
    return false;
  } finally {
    try {
      if (fs.existsSync(binaryTmp)) fs.unlinkSync(binaryTmp);
    } catch { /* ok */ }
  }
}

// ── Marketplace clone refresh ──────────────────────────────

function marketplaceCloneDir() {
  return path.join(claudeHome(), 'plugins', 'marketplaces', MARKETPLACE_NAME);
}

/**
 * Fast-forward the Claude Code marketplace clone after a plugin update.
 *
 * Auto-update writes the plugin cache + installed_plugins.json directly and
 * never touched the marketplace clone, so its marketplace.json stayed pinned
 * at the version present when the user last ran a /plugin command (observed
 * live: clone at 0.48.0 four days after 0.49.0 shipped). A stale clone makes
 * the /plugin UI report the old version and lets Claude Code re-install the
 * old plugin files from it. --ff-only + silent failure: a dirty or diverged
 * clone is Claude Code's property — never force anything there.
 */
function refreshMarketplaceClone({ dir = marketplaceCloneDir(), exec = execFileSync, timeoutMs = 15000 } = {}) {
  try {
    if (!fs.existsSync(path.join(dir, '.git'))) return false;
    if (!commandExists('git')) return false;
    exec('git', ['-C', dir, 'pull', '--ff-only', '--quiet'], { timeout: timeoutMs, stdio: 'pipe' });
    return true;
  } catch {
    return false;
  }
}

// ── Download & Install ─────────────────────────────────────

async function downloadAndInstall(latest, {
  exec = execFileSync,
  downloadBin = downloadBinary,
  refreshMarketplace = refreshMarketplaceClone,
} = {}) {
  // Pre-flight: check required CLI tools before attempting any download
  const missingTools = ['curl', 'tar'].filter(cmd => !commandExists(cmd));
  if (missingTools.length > 0) {
    console.error(`[code-graph] Auto-update skipped: missing required tools: ${missingTools.join(', ')}. Install them to enable auto-updates.`);
    return { pluginUpdated: false, binaryUpdated: false };
  }

  const tmpDir = path.join(cgTmpDir(), `update-${Date.now()}`);
  let pluginUpdated = false;
  let binaryUpdated = false;
  let marketplaceRefreshed = false;

  try {
    fs.mkdirSync(tmpDir, { recursive: true });

    // ── Step 1: Download and install plugin files from tarball ──
    const tarballPath = path.join(tmpDir, 'release.tar.gz');
    exec('curl', [
      '-sL', '-o', tarballPath,
      '-H', 'Accept: application/vnd.github+json',
      latest.tarballUrl,
    ], { timeout: 30000, stdio: 'pipe' });

    exec('tar', [
      'xzf', tarballPath, '-C', tmpDir, '--strip-components=1',
    ], { timeout: 15000, stdio: 'pipe' });

    const pluginSrc = path.join(tmpDir, 'claude-plugin');
    const pluginDst = path.join(
      pluginsCacheDir(), MARKETPLACE_NAME, 'code-graph-mcp', latest.version
    );

    if (fs.existsSync(pluginSrc) && getExtractedPluginVersion(pluginSrc) === latest.version) {
      fs.mkdirSync(pluginDst, { recursive: true });
      copyDirSync(pluginSrc, pluginDst);
      pluginUpdated = true;
    }

    // Repoint state at the new version ONLY if the plugin copy actually landed.
    // Guarding on pluginUpdated: when the copy above was skipped (pluginSrc absent, or
    // its plugin.json version drifted from the tag — the project's version sync is known
    // fragile), pluginDst was never created. Advancing installPath/manifest to it anyway
    // pointed Claude Code at a nonexistent install dir while state read "up to date".
    if (pluginUpdated) {
      // Update installed_plugins.json to point to new version
      const installedPath = installedPluginsPath();
      try {
        const installed = readJson(installedPath);
        if (installed && installed.plugins && installed.plugins[PLUGIN_ID]) {
          installed.plugins[PLUGIN_ID][0].installPath = pluginDst;
          installed.plugins[PLUGIN_ID][0].version = latest.version;
          installed.plugins[PLUGIN_ID][0].lastUpdated = new Date().toISOString();
          writeJsonAtomic(installedPath, installed);
        }
      } catch { /* not fatal */ }

      // Update install manifest
      try {
        const manifest = readManifest();
        manifest.version = latest.version;
        manifest.updatedAt = new Date().toISOString();
        writeJsonAtomic(path.join(CACHE_DIR, 'install-manifest.json'), manifest);
      } catch { /* not fatal */ }

      // Run the NEW lifecycle.js to update settings.json hooks with new paths.
      // Without this, settings.json hooks still point to the old version directory
      // until the next session's self-heal corrects them.
      try {
        const newLifecycle = path.join(pluginDst, 'scripts', 'lifecycle.js');
        if (fs.existsSync(newLifecycle)) {
          exec(process.execPath, [newLifecycle, 'update'], {
            timeout: 5000, stdio: 'pipe',
          });
        }
      } catch { /* not fatal — syncLifecycleConfig will self-heal on next session */ }
    }

    // ── Step 1.5: Fast-forward the marketplace clone so /plugin UI and any
    //    Claude-Code-side reinstall see the version we just installed.
    if (pluginUpdated) {
      marketplaceRefreshed = refreshMarketplace();
    }

    // ── Step 2: Download platform binary directly from GitHub release ──
    if (await downloadBin(latest)) {
      binaryUpdated = true;
    }

    return { pluginUpdated, binaryUpdated, marketplaceRefreshed };
  } catch (e) {
    console.error(`[code-graph] Plugin download/extract failed: ${e.message}`);
    return { pluginUpdated: false, binaryUpdated: false, marketplaceRefreshed };
  } finally {
    try { fs.rmSync(tmpDir, { recursive: true, force: true }); } catch { /* ok */ }
  }
}

// ── Main Entry ─────────────────────────────────────────────

/**
 * Self-heal the cached native binary when the plugin shell is already at latest
 * but the binary lags (missing OR a different version). This is the orchestration
 * glue that broke twice in the field (v0.45.1, v0.45.2): the decision predicate
 * was correct, but nothing guaranteed checkForUpdate actually invoked the download
 * on the shell-matches-latest path. Extracted + injectable so the wiring itself is
 * regression-tested, not just the predicate. Returns true iff a download promoted.
 */
async function selfHealStaleBinary(latest, { needsUpdate = cachedBinaryNeedsUpdate, download = downloadBinary } = {}) {
  if (!needsUpdate(latest)) return false;
  return await download(latest);
}

async function checkForUpdate({ installMissing = false } = {}) {
  try {
    // Skip in dev mode — unless the launcher explicitly requested a missing-
    // binary install, in which case we MUST proceed regardless of mode (the
    // alternative is wedging the MCP server with no binary on disk).
    if (!installMissing && isDevMode()) return null;

    const state = readState();
    // manifest.version is authoritative — /plugin update writes it directly and
    // bypasses auto-update.js, so re-sync state.installedVersion every call.
    const installedVersion = readManifest().version || '0.0.0';

    // Time-based throttle. Two conditions override it: a missing cache binary
    // (launcher cannot start) and a present-but-stale binary (otherwise it stays
    // pinned to the old version for up to a full check interval — the binary
    // self-heal would never run inside the throttle window). Both bypass to the
    // fetch + self-heal path below.
    const binaryMissing = !fs.existsSync(cachedBinaryPath());
    const binaryStale = cachedBinaryStaleVsState(state);
    if (!binaryMissing && !binaryStale && !shouldCheck(state)) {
      if (state.installedVersion !== installedVersion) {
        saveState({ ...state, installedVersion });
      }
      if (state.updateAvailable && state.latestVersion
          && compareVersions(state.latestVersion, installedVersion) > 0) {
        return { updateAvailable: true, from: installedVersion, to: state.latestVersion };
      }
      return null;
    }

    // Check GitHub for latest release
    const latest = await fetchLatestRelease();
    if (!latest) {
      saveState({ ...state, installedVersion, lastCheck: new Date().toISOString() });
      return null;
    }

    // Compare versions
    const hasUpdate = compareVersions(latest.version, installedVersion) > 0;

    if (hasUpdate) {
      const result = await downloadAndInstall(latest);
      const success = result.pluginUpdated;
      const newState = {
        lastCheck: new Date().toISOString(),
        installedVersion: success ? latest.version : installedVersion,
        latestVersion: latest.version,
        updateAvailable: !success,
        lastUpdate: success ? new Date().toISOString() : state.lastUpdate,
        rateLimited: false,
        binaryUpdated: result.binaryUpdated,
        marketplaceRefreshed: result.marketplaceRefreshed,
      };
      saveState(newState);

      return {
        updateAvailable: !success,
        updated: success,
        binaryUpdated: result.binaryUpdated,
        from: installedVersion,
        to: latest.version,
      };
    }

    // No plugin-shell update — but self-heal the native binary if it is missing
    // OR stale (see selfHealStaleBinary). The shell version (manifest.version)
    // can match latest while the cached binary lags — this is exactly the wild
    // failure observed in the field (shell at v0.45, binary pinned at v0.16.6).
    const selfHealedBinary = await selfHealStaleBinary(latest);

    saveState({
      ...state,
      installedVersion,
      lastCheck: new Date().toISOString(),
      latestVersion: latest.version,
      updateAvailable: false,
      rateLimited: false,
      binaryUpdated: selfHealedBinary || state.binaryUpdated,
    });
    return selfHealedBinary
      ? { updated: false, binaryUpdated: true, from: installedVersion, to: installedVersion }
      : null;
  } catch {
    // Silent failure — never block session
    return null;
  }
}

module.exports = {
  checkForUpdate, commandExists, isDevMode, readState, compareVersions,
  getExtractedPluginVersion, readBinaryVersion, promoteVerifiedBinary,
  isSilentMode, isInstallMissingMode,
  requestJson, parseLatestRelease, fetchLatestRelease,
  downloadBinary, cachedBinaryPath, cachedBinaryNeedsUpdate, cachedBinaryStaleVsState,
  selfHealStaleBinary,
  downloadAndInstall, refreshMarketplaceClone, marketplaceCloneDir,
};

// CLI: node auto-update.js [check|status] [--silent] [--install-missing]
if (require.main === module) {
  (async () => {
    const argv = process.argv.slice(2);
    const cmd = argv.find(arg => !arg.startsWith('--')) || 'check';
    const silent = isSilentMode(argv);
    const installMissing = isInstallMissingMode(argv);
    if (cmd === 'status') {
      const state = readState();
      console.log(JSON.stringify(state, null, 2));
    } else {
      if (!silent) console.log('Checking for updates...');
      const result = await checkForUpdate({ installMissing });
      if (silent) return;
      if (result && result.updated) {
        console.log(`Updated: v${result.from} → v${result.to} (binary: ${result.binaryUpdated ? 'yes' : 'no'})`);
      } else if (result && result.updateAvailable) {
        console.log(`Update available: v${result.to} (auto-install failed)`);
      } else if (result && result.binaryUpdated) {
        console.log(`Repaired binary cache (v${result.to})`);
      } else if (!installMissing && isDevMode()) {
        console.log('Dev mode — auto-update skipped');
      } else {
        const manifest = readManifest();
        console.log(`Up to date (v${manifest.version || 'unknown'})`);
      }
    }
  })();
}
