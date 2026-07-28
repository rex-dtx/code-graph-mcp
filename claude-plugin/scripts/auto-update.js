#!/usr/bin/env node
'use strict';
const { execFileSync, spawn } = require('child_process');
const fs = require('fs');
const https = require('https');
const http = require('http');
const crypto = require('crypto');
const path = require('path');
const os = require('os');
const { CACHE_DIR, PLUGIN_ID, MARKETPLACE_NAME, readManifest, readJson, writeJsonAtomic, installedPluginsPath, pluginsCacheDir } = require('./lifecycle');
const { claudeHome } = require('./claude-config');
const { clearCache: clearBinaryCache, globalNodeModulesCandidates, nvmNodeModulesDirs, PLATFORM_PKG, detectLibc } = require('./find-binary');
const { readBinaryVersion, compareVersions, isDevMode } = require('./version-utils');
const { cgTmpDir } = require('./tmp-dir');
const { npmSpawnOpts } = require('./npm-exec');
const { acquireLock } = require('./install-lock');

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
const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;        // 6h — steady-state re-check
const UP_TO_DATE_RECHECK_MS = 30 * 60 * 1000;        // 30min — re-verify an "up to date" result (release-race guard)
const SESSION_START_MIN_GAP_MS = 2 * 60 * 1000;      // 2min — anti-hammer floor for forced (session-start) checks
const RATE_LIMIT_INTERVAL_MS = 24 * 60 * 60 * 1000;  // 24h if rate-limited
const FETCH_TIMEOUT_MS = 3000;

function isSilentMode(argv = process.argv.slice(2), env = process.env) {
  return argv.includes('--silent') || env.CODE_GRAPH_AUTO_UPDATE_SILENT === '1';
}

function isInstallMissingMode(argv = process.argv.slice(2)) {
  return argv.includes('--install-missing');
}

// High-intent trigger (session start / explicit reload) → bypass the soft
// throttle so an available update is picked up immediately, not on the next
// 6h/30min tick. Passed by session-init's launchBackgroundAutoUpdate.
function isForceMode(argv = process.argv.slice(2)) {
  return argv.includes('--force');
}

// ── Platform → GitHub release asset name mapping ──────────
function getPlatformAssetName({ platform = os.platform(), arch = os.arch(), libc = null } = {}) {
  // No musl asset is published: the glibc linux build downloads fine but cannot
  // exec on Alpine, so promoteVerifiedBinary always rejected it and — with the
  // binary still missing — every SessionStart bypassed the throttle and pulled
  // the same futile ~40MB again. Null stops the download path entirely; the
  // launcher surfaces unsupportedPlatformHint (cargo install / glibc image).
  if (platform === 'linux' && (libc || detectLibc()) === 'musl') return null;
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

// Whether to hit GitHub now. Keyed to the previous check's outcome, with a force
// override for high-intent triggers (session start / explicit reload). Ordering:
//   1. rate-limit backoff (24h) wins over everything — never push more requests
//      into a GitHub 403.
//   2. force → only the short SESSION_START_MIN_GAP_MS floor applies, so opening
//      a new session re-checks immediately while a crash/reopen loop still can't
//      hammer the API.
//   3. otherwise → an "up to date" result is re-verified on a short cadence
//      (UP_TO_DATE_RECHECK_MS). This is the release-publish race guard: a version
//      can go live seconds AFTER a check that said "up to date", and the plain 6h
//      interval left it invisible for the full 6h (observed live — v0.85.7
//      published 8s after a check pinned v0.85.6). A pending-but-unfinished update
//      keeps the 6h steady-state interval.
function shouldCheck(state, { force = false } = {}) {
  if (!state.lastCheck) return true;
  const elapsed = Date.now() - new Date(state.lastCheck).getTime();
  if (state.rateLimited) return elapsed >= RATE_LIMIT_INTERVAL_MS;
  if (force) return elapsed >= SESSION_START_MIN_GAP_MS;
  const interval = state.updateAvailable === false ? UP_TO_DATE_RECHECK_MS : CHECK_INTERVAL_MS;
  return elapsed >= interval;
}

// ── Version Comparison ─────────────────────────────────────
// compareVersions is imported from version-utils.js (single canonical,
// pre-release-aware implementation) and re-exported below.

// ── GitHub API ─────────────────────────────────────────────

/**
 * Resolve the proxy URL to use for a target URL, honoring HTTPS_PROXY/HTTP_PROXY
 * (and lowercase variants) plus NO_PROXY. Returns null when no proxy applies, so
 * the direct path stays byte-identical for users without a proxy configured.
 * @param {string} targetUrl
 * @param {NodeJS.ProcessEnv} [env]
 * @returns {string|null}
 */
function resolveProxy(targetUrl, env = process.env) {
  let host;
  try { host = new URL(targetUrl).hostname.toLowerCase(); } catch { return null; }
  const noProxy = (env.NO_PROXY || env.no_proxy || '').trim();
  if (noProxy === '*') return null;
  for (const raw of noProxy.split(',').map(s => s.trim().toLowerCase()).filter(Boolean)) {
    const bare = raw.replace(/^\*?\./, ''); // ".github.com" / "*.github.com" → "github.com"
    if (host === bare || host.endsWith('.' + bare)) return null;
  }
  const proxy = env.HTTPS_PROXY || env.https_proxy || env.HTTP_PROXY || env.http_proxy;
  return proxy && proxy.trim() ? proxy.trim() : null;
}

function requestJson(url, timeoutMs = FETCH_TIMEOUT_MS) {
  return new Promise((resolve, reject) => {
    const headers = {
      'Accept': 'application/vnd.github+json',
      'User-Agent': 'code-graph-auto-update/1.0',
    };
    const onResponse = (res) => {
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
    };

    const proxy = resolveProxy(url);
    if (proxy) {
      // Node's https module ignores *_PROXY env vars. curl-based binary downloads
      // already honor the proxy; tunnel the release-metadata GET over an HTTP
      // CONNECT to reach parity for users behind a corporate proxy.
      let pu, target;
      try { pu = new URL(proxy); target = new URL(url); }
      catch { reject(new Error('invalid proxy or target URL')); return; }
      const connectHeaders = {};
      if (pu.username) {
        const cred = `${decodeURIComponent(pu.username)}:${decodeURIComponent(pu.password)}`;
        connectHeaders['Proxy-Authorization'] = 'Basic ' + Buffer.from(cred).toString('base64');
      }
      const connectReq = http.request({
        host: pu.hostname,
        port: pu.port || 80,
        method: 'CONNECT',
        path: `${target.hostname}:${target.port || 443}`,
        headers: connectHeaders,
      });
      connectReq.on('connect', (res, socket) => {
        if (res.statusCode !== 200) {
          socket.destroy();
          reject(new Error(`proxy CONNECT failed: ${res.statusCode}`));
          return;
        }
        const req = https.request(url, {
          method: 'GET', headers, socket, agent: false, servername: target.hostname,
        }, onResponse);
        req.setTimeout(timeoutMs, () => req.destroy(new Error('request timeout')));
        req.on('error', reject);
        req.end();
      });
      connectReq.setTimeout(timeoutMs, () => connectReq.destroy(new Error('proxy connect timeout')));
      connectReq.on('error', reject);
      connectReq.end();
      return;
    }

    const req = https.request(url, { method: 'GET', headers }, onResponse);
    req.setTimeout(timeoutMs, () => req.destroy(new Error('request timeout')));
    req.on('error', reject);
    req.end();
  });
}

// Published by release.yml alongside the five platform binaries, each with a
// `.sha256` sidecar. Distinct from `tarball_url`, which is GitHub's
// auto-generated source archive and has no checksum published anywhere.
const PLUGIN_ASSET_NAME = 'claude-plugin.tar.gz';

function parseLatestRelease(data, assetName = getPlatformAssetName()) {
  if (!data || typeof data.tag_name !== 'string' || typeof data.tarball_url !== 'string') {
    return null;
  }

  const assetUrl = (name) => {
    if (!name || !Array.isArray(data.assets)) return null;
    const asset = data.assets.find((entry) => entry && entry.name === name);
    return asset && typeof asset.browser_download_url === 'string'
      ? asset.browser_download_url
      : null;
  };

  return {
    version: data.tag_name.replace(/^v/, ''),
    tarballUrl: data.tarball_url,
    pluginTarballUrl: assetUrl(PLUGIN_ASSET_NAME),
    binaryUrl: assetUrl(assetName),
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
  const current = readVersion(binaryPath);
  if (!current) return true; // unreadable/broken binary — let the heal replace it
  // Ordered compare, not string inequality: a binary NEWER than releases/latest
  // (dev build, or the API momentarily lagging a publish) must not be downgraded.
  return compareVersions(current, latest.version) < 0;
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
  const current = readVersion(binaryPath);
  if (!current) return true; // unreadable/broken — bypass throttle so the heal runs
  // Ordered compare (see cachedBinaryNeedsUpdate): newer-than-state is not stale.
  return compareVersions(current, state.latestVersion) < 0;
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

    // Integrity sidecar (<asset>.sha256), fail-CLOSED. `curl -f` turns a 404 into
    // a throw. One retry, because the alternative to a transient network blip is
    // no update this cycle — the installed binary keeps working and the next
    // check tries again, which is a strictly safer failure than exec'ing bytes
    // nothing vouched for.
    //
    // This used to fall through to a TOFU path on a missing sidecar, which made
    // it the one download chain in the repo that was fail-OPEN while
    // `src/snapshot/install.rs` (whose comment reads "this used to warn and fail
    // OPEN") is fail-closed. release.yml publishes a sidecar for every binary of
    // every release — verified back to v0.100.0 — and downloads always target
    // `releases/latest`, so there is no reachable no-sidecar case left to serve.
    // Same-origin, so this defends transit/CDN corruption and truncation, not a
    // release-asset swap; the version-exec check is the backstop there.
    let expectedSha = null;
    const shaTmp = binaryTmp + '.sha256';
    for (let attempt = 0; attempt < 2 && !expectedSha; attempt++) {
      try {
        execFileSync('curl', ['-sfL', '-o', shaTmp, latest.binaryUrl + '.sha256'],
          { timeout: 30000, stdio: 'pipe' });
        expectedSha = (fs.readFileSync(shaTmp, 'utf8').trim().split(/\s+/)[0]) || null;
      } catch { /* retry once, then refuse below */ } finally {
        try { if (fs.existsSync(shaTmp)) fs.unlinkSync(shaTmp); } catch { /* ok */ }
      }
    }
    if (!expectedSha) {
      console.error(`[code-graph] Refusing to install: no sha256 sidecar for ${latest.binaryUrl} (fetched twice). The current binary is unchanged; the next update check will retry.`);
      try { fs.unlinkSync(binaryTmp); } catch { /* ok */ }
      return false;
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
    // too — the version-exec check below is the backstop there).
    //
    // Fail-CLOSED: no expected sha, no install. The previous "warn and proceed"
    // arm made this the only fail-open link in the four download chains, against
    // a fail-closed `src/snapshot/install.rs` — and a warning printed to stderr
    // during a background auto-update is seen by nobody.
    if (!expectedSha256) {
      console.error('[code-graph] No expected sha256 supplied — refusing to install an unverified binary.');
      try { fs.unlinkSync(binaryTmp); } catch { /* ok */ }
      return false;
    }
    const actualSha = sha256File(binaryTmp);
    if (actualSha.toLowerCase() !== String(expectedSha256).toLowerCase()) {
      console.error(`[code-graph] Binary checksum mismatch (sha256): expected ${expectedSha256}, got ${actualSha} — refusing to install.`);
      return false;
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
  cmdExists = commandExists,
} = {}) {
  // Pre-flight: check required CLI tools before attempting any download
  const missingTools = ['curl', 'tar'].filter(cmd => !cmdExists(cmd));
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

    // ── Step 1: Download and install plugin files from the release asset ──
    //
    // Fail-CLOSED on integrity, like the binary chain. This step extracts an
    // archive and then COPIES ITS JAVASCRIPT into the plugin cache, where Claude
    // Code runs it as hooks on every tool call — so of the four download chains
    // it is the one where unverified bytes become executed code, and it was the
    // only one with no checksum at all (`tarball_url` is GitHub's generated
    // source archive; nothing publishes a digest for it).
    // `claude-plugin.tar.gz` + `.sha256` are published by release.yml for every
    // release from the one carrying this change onward, and updates always
    // target `releases/latest` — so a missing asset means something is wrong
    // with the release, not that we are talking to an older one. Refusing leaves
    // the user on their current, working plugin version; the binary update below
    // still runs.
    if (!latest.pluginTarballUrl) {
      console.error(`[code-graph] Plugin update skipped: release ${latest.version} publishes no ${PLUGIN_ASSET_NAME} — refusing to install plugin code from an unverifiable source archive.`);
      return { pluginUpdated: false, binaryUpdated: await downloadBin(latest), marketplaceRefreshed: false };
    }
    const tarballPath = path.join(tmpDir, PLUGIN_ASSET_NAME);
    exec('curl', [
      '-sL', '-o', tarballPath,
      '-H', 'Accept: application/octet-stream',
      latest.pluginTarballUrl,
    ], { timeout: 30000, stdio: 'pipe' });

    const shaPath = tarballPath + '.sha256';
    let expectedSha = null;
    try {
      exec('curl', ['-sfL', '-o', shaPath, latest.pluginTarballUrl + '.sha256'],
        { timeout: 30000, stdio: 'pipe' });
      expectedSha = (fs.readFileSync(shaPath, 'utf8').trim().split(/\s+/)[0]) || null;
    } catch { /* refused just below */ }
    const actualSha = fs.existsSync(tarballPath) ? sha256File(tarballPath) : null;
    if (!expectedSha || !actualSha || expectedSha.toLowerCase() !== actualSha.toLowerCase()) {
      console.error(`[code-graph] Plugin tarball integrity check failed (expected ${expectedSha || '<no sidecar>'}, got ${actualSha || '<no download>'}) — refusing to extract.`);
      return { pluginUpdated: false, binaryUpdated: await downloadBin(latest), marketplaceRefreshed: false };
    }

    // No --strip-components: the asset archives `claude-plugin/` itself, while
    // GitHub's source tarball wraps everything in `<owner>-<repo>-<sha>/`.
    exec('tar', [
      'xzf', tarballPath, '-C', tmpDir,
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

// ── Global npm package self-heal ───────────────────────────
// The `code-graph-mcp` CLI on the user's PATH is the GLOBAL npm shell package
// (@sdsrs/code-graph) — a delivery surface entirely outside the marketplace
// plugin, so /plugin update and the binary self-heal above never touch it. In
// the field it drifts for months (a 0.46.0 wrapper delegating to a 0.101.0
// binary) and users were expected to run `npm update -g` by hand — which also
// breaks on unrelated npm-config quirks (EALLOWGIT). Same story for a platform
// package installed EXPLICITLY at the global top level (the old launcher's
// manual-install hint suggested exactly that): that relic was the 0.16.6
// landmine behind the MCP connect-timeout incident.
//
// Heal contract: refresh ONLY what the user already installed globally (never
// introduce a global install they didn't ask for), one bounded npm run per
// release target, silent failure (an unhealable npm env must not block or spam).

const SHELL_PKG = '@sdsrs/code-graph';
const GLOBAL_PKG_HEAL_MAX_ATTEMPTS = 3;
const GLOBAL_PKG_HEAL_TIMEOUT_MS = 180000; // npm resolves + downloads the platform optionalDependency (~40MB)

/** Installed version of a top-level GLOBAL npm package, or null when absent. */
function globalPkgVersion(name, roots = null) {
  for (const root of (roots || globalNodeModulesCandidates())) {
    try {
      const pkg = readJson(path.join(root, name, 'package.json'));
      if (pkg && pkg.version) return pkg.version;
    } catch { /* not installed under this root */ }
  }
  return null;
}

/** Globally-installed packages of ours whose version lags `latestVersion`. */
function staleGlobalPkgs(latestVersion, roots = null) {
  const out = [];
  for (const name of [SHELL_PKG, PLATFORM_PKG]) {
    const ver = globalPkgVersion(name, roots);
    if (ver && compareVersions(ver, latestVersion) < 0) out.push({ name, version: ver });
  }
  return out;
}

/**
 * Global installs of ours stranded under a NON-active node version. nvm keeps a
 * separate global prefix per node; switching the default node leaves the old
 * prefix's `@sdsrs/code-graph` behind — invisible to selfHealGlobalPkgs (which
 * only sees, and can only `npm install -g` into, the ACTIVE node's prefix) yet
 * still able to seed stale settings.json hooks / shadow PATH shims (the
 * v24.11.1@0.46.0 relic firing beside the active install — RCA 2026-07-24).
 * Detection-only: returns each relic's package + version + node prefix so doctor
 * can surface it with manual remediation. `dirs`/`activeDir` injectable for tests.
 */
function inactiveNodeGlobalRelics({ dirs = null, activeDir = null } = {}) {
  const active = path.resolve(activeDir
    || path.join(path.dirname(process.execPath), '..', 'lib', 'node_modules'));
  const roots = dirs || nvmNodeModulesDirs();
  const out = [];
  for (const dir of roots) {
    if (path.resolve(dir) === active) continue; // active prefix → not a relic
    for (const name of [SHELL_PKG, PLATFORM_PKG]) {
      const version = globalPkgVersion(name, [dir]);
      if (version) out.push({ name, version, nodeModulesDir: dir });
    }
  }
  return out;
}

/** One targeted `npm install -g` for the given specs. Resolves true on exit 0. */
function npmInstallGlobal(specs) {
  return new Promise((resolve) => {
    if (!commandExists('npm')) { resolve(false); return; }
    const child = spawn('npm', ['install', '-g', ...specs], npmSpawnOpts({
      timeout: GLOBAL_PKG_HEAL_TIMEOUT_MS,
      stdio: ['ignore', 'ignore', 'pipe'],
    }));
    let stderr = '';
    child.stderr.on('data', (d) => { stderr += d.toString(); });
    child.on('error', () => resolve(false));
    child.on('exit', (code) => {
      if (code === 0) {
        console.error(`[code-graph] global npm package(s) refreshed: ${specs.join(' ')}`);
        resolve(true);
      } else {
        const tail = stderr.trim().split('\n').slice(-2).join(' | ');
        console.error(`[code-graph] global npm refresh failed (exit ${code}): ${tail}`);
        resolve(false);
      }
    });
  });
}

/**
 * Self-heal globally-installed shell/platform packages to `latest.version`.
 * Returns a state patch (spread into the update-state save): attempts are
 * counted PER target version so a persistently-failing npm env stops being
 * retried after GLOBAL_PKG_HEAL_MAX_ATTEMPTS, and the counter re-arms when the
 * next release moves the target.
 */
async function selfHealGlobalPkgs(latest, state, {
  readStale = staleGlobalPkgs,
  install = npmInstallGlobal,
} = {}) {
  if (!latest || !latest.version) return {};
  const stale = readStale(latest.version);
  if (stale.length === 0) {
    // Healthy (or nothing installed globally) — clear any leftover counter.
    return state.globalPkgHealAttempts ? { globalPkgHealAttempts: 0, globalPkgHealVersion: null } : {};
  }
  const attempts = state.globalPkgHealVersion === latest.version ? (state.globalPkgHealAttempts || 0) : 0;
  if (attempts >= GLOBAL_PKG_HEAL_MAX_ATTEMPTS) return {};
  const ok = await install(stale.map((s) => `${s.name}@${latest.version}`));
  return {
    globalPkgHealVersion: latest.version,
    globalPkgHealAttempts: ok ? 0 : attempts + 1,
  };
}

// Whether a THROTTLED checkForUpdate should still attempt the global-npm
// self-heal. The post-fetch heal below only runs on the non-throttle path, but
// the ONLY context that can SEE a user's nvm/global prefix is a CLI run under
// that node (globalNodeModulesCandidates is execPath-derived) — and such a run,
// once binary+shell are current, short-circuits at the throttle early-return and
// never reaches the heal. That gap stranded a global `code-graph-mcp` shim at
// 0.101.0 while the binary reached 0.103.0 (RCA 2026-07-24). Cheap local
// package.json read (readStale) gates the slow, lock-guarded npm path. Split out
// so the decision is unit-testable without the full checkForUpdate harness.
function shouldHealGlobalsOnThrottle(state, { readStale = staleGlobalPkgs } = {}) {
  if (!state || !state.latestVersion) return false;
  if (process.env.CODE_GRAPH_INSTALL_LOCK_HELD === '1') return false; // parent launcher holds it
  return readStale(state.latestVersion).length > 0;
}

// `requestJsonFn` is a test seam, forwarded to fetchLatestRelease — the same
// injection point that function already exposes. It exists so the 403 path can
// be driven without a network: that path is where the rate-limit backoff either
// engages or is silently erased, and no other observable distinguishes the two.
async function checkForUpdate({ installMissing = false, force = false, requestJsonFn } = {}) {
  let installLock = null;
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
    if (!binaryMissing && !binaryStale && !shouldCheck(state, { force })) {
      if (state.installedVersion !== installedVersion) {
        saveState({ ...state, installedVersion });
      }
      // Global-npm shell/platform self-heal reaches the throttle window too (see
      // shouldHealGlobalsOnThrottle). Cheap local check first; only the actually-
      // stale case takes the slow, lock-guarded npm path.
      if (shouldHealGlobalsOnThrottle(state)) {
        installLock = acquireLock(path.join(CACHE_DIR, 'install.lock'));
        if (installLock) {
          const globalHeal = await selfHealGlobalPkgs({ version: state.latestVersion }, state);
          saveState({ ...readState(), ...globalHeal });
        }
      }
      if (state.updateAvailable && state.latestVersion
          && compareVersions(state.latestVersion, installedVersion) > 0) {
        return { updateAvailable: true, from: installedVersion, to: state.latestVersion };
      }
      return null;
    }

    // Check GitHub for latest release
    const latest = await fetchLatestRelease(requestJsonFn || requestJson);
    if (!latest) {
      // Re-read, do NOT spread the pre-fetch `state`. On a 403 fetchLatestRelease
      // writes `rateLimited: true` to the state file, and this is the branch it
      // returns null through — spreading the stale snapshot wrote that flag
      // straight back to whatever it was before (normally absent). The 24h
      // RATE_LIMIT_INTERVAL_MS backoff in shouldCheck() therefore never engaged:
      // it read a state where rateLimited had just been erased by the very call
      // that set it, and kept polling GitHub on the ordinary interval while
      // already rate-limited. Dead code since the backoff was written.
      saveState({ ...readState(), installedVersion, lastCheck: new Date().toISOString() });
      return null;
    }

    // Compare versions
    const hasUpdate = compareVersions(latest.version, installedVersion) > 0;

    // Inter-process gate for every mutating path below (plugin-cache copy,
    // binary download, global npm heals): concurrent sessions racing here ran
    // parallel `npm install -g` against one global prefix and clobbered each
    // other's state-file counters (rateLimited, heal attempts). Skip-if-held:
    // the holder does the work and its state outcome wins. The launcher's
    // install chain already holds this lock across its spawn of this script —
    // it marks that with CODE_GRAPH_INSTALL_LOCK_HELD so we don't deadlock
    // against our own parent.
    if (process.env.CODE_GRAPH_INSTALL_LOCK_HELD !== '1') {
      installLock = acquireLock(path.join(CACHE_DIR, 'install.lock'));
      if (!installLock) return null;
    }

    if (hasUpdate) {
      const result = await downloadAndInstall(latest);
      const success = result.pluginUpdated;
      const newState = {
        lastCheck: new Date().toISOString(),
        installedVersion: success ? latest.version : installedVersion,
        latestVersion: latest.version,
        updateAvailable: !success,
        // Consecutive failed-download counter. The statusline shows "↻ updating"
        // while updateAvailable is set; without a bound, a persistently-failing
        // update (missing tar/curl, full disk, blocked network) pins "updating"
        // forever, asserting a self-heal that never happens. The statusline stops
        // trusting it past STUCK_UPDATE_ATTEMPTS; success resets to 0.
        updateAttempts: success ? 0 : (state.updateAttempts || 0) + 1,
        lastUpdate: success ? new Date().toISOString() : state.lastUpdate,
        rateLimited: false,
        binaryUpdated: result.binaryUpdated,
        marketplaceRefreshed: result.marketplaceRefreshed,
      };
      // Keep any globally-installed shell/platform npm packages in step with
      // the release the plugin just moved to (see selfHealGlobalPkgs).
      const globalHeal = await selfHealGlobalPkgs(latest, state);
      saveState({ ...newState, ...globalHeal });

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

    // Same for the GLOBAL npm delivery surface (the `code-graph-mcp` CLI on
    // PATH + any explicitly-installed platform package): nothing else ever
    // updates it, and stale copies drift for months (0.46.0 wrapper) or years
    // (the 0.16.6 platform relic).
    const globalHeal = await selfHealGlobalPkgs(latest, state);

    saveState({
      ...state,
      installedVersion,
      lastCheck: new Date().toISOString(),
      latestVersion: latest.version,
      updateAvailable: false,
      rateLimited: false,
      binaryUpdated: selfHealedBinary || state.binaryUpdated,
      ...globalHeal,
    });
    return selfHealedBinary
      ? { updated: false, binaryUpdated: true, from: installedVersion, to: installedVersion }
      : null;
  } catch {
    // Silent failure — never block session
    return null;
  } finally {
    if (installLock) installLock.release();
  }
}

module.exports = {
  checkForUpdate, commandExists, isDevMode, readState, compareVersions, shouldCheck,
  getExtractedPluginVersion, readBinaryVersion, promoteVerifiedBinary,
  isSilentMode, isInstallMissingMode, isForceMode,
  requestJson, resolveProxy, parseLatestRelease, fetchLatestRelease,
  PLUGIN_ASSET_NAME,
  downloadBinary, cachedBinaryPath, cachedBinaryNeedsUpdate, cachedBinaryStaleVsState,
  getPlatformAssetName,
  selfHealStaleBinary,
  selfHealGlobalPkgs, staleGlobalPkgs, globalPkgVersion, npmInstallGlobal,
  shouldHealGlobalsOnThrottle, inactiveNodeGlobalRelics,
  downloadAndInstall, refreshMarketplaceClone, marketplaceCloneDir,
};

// CLI: node auto-update.js [check|status] [--silent] [--install-missing]
if (require.main === module) {
  (async () => {
    const argv = process.argv.slice(2);
    const cmd = argv.find(arg => !arg.startsWith('--')) || 'check';
    const silent = isSilentMode(argv);
    const installMissing = isInstallMissingMode(argv);
    const force = isForceMode(argv);
    if (cmd === 'status') {
      const state = readState();
      console.log(JSON.stringify(state, null, 2));
    } else {
      if (!silent) console.log('Checking for updates...');
      const result = await checkForUpdate({ installMissing, force });
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
