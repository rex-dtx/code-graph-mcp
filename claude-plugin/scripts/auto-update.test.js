'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');
const crypto = require('crypto');

// Git plumbing env vars git(1) honors over cwd. A partial `git commit` runs the
// pre-commit hook with these exported, so any `git` this suite shells out to for
// its tempdir fixtures would operate on the REAL repo index instead of the
// fixture. Strip them from THIS process (covers the raw `git clone` + the node
// `-e` sub-spawns, which inherit env) and per-call in git() below (hermetic even
// if a test sets one). Sibling of the v0.80.3 pre-commit.sh cargo-path fix (H4).
const GIT_ENV_VARS = [
  'GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_OBJECT_DIRECTORY',
  'GIT_COMMON_DIR', 'GIT_NAMESPACE', 'GIT_PREFIX',
];
for (const k of GIT_ENV_VARS) delete process.env[k];
function cleanGitEnv() {
  const e = { ...process.env };
  for (const k of GIT_ENV_VARS) delete e[k];
  return e;
}

const {
  commandExists,
  fetchLatestRelease,
  getExtractedPluginVersion,
  parseLatestRelease,
  readBinaryVersion,
  promoteVerifiedBinary,
  cachedBinaryPath,
  cachedBinaryNeedsUpdate,
  cachedBinaryStaleVsState,
  getPlatformAssetName,
  downloadBinary,
  selfHealStaleBinary,
  selfHealGlobalPkgs,
  staleGlobalPkgs,
  globalPkgVersion,
  isInstallMissingMode,
  isSilentMode,
  shouldCheck,
  resolveProxy,
  shouldHealGlobalsOnThrottle,
  inactiveNodeGlobalRelics,
} = require('./auto-update');

function mkDir(t, prefix) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test('getExtractedPluginVersion reads extracted plugin manifest version', (t) => {
  const root = mkDir(t, 'code-graph-plugin-');
  const manifest = path.join(root, '.claude-plugin', 'plugin.json');
  fs.mkdirSync(path.dirname(manifest), { recursive: true });
  fs.writeFileSync(manifest, JSON.stringify({ version: '1.2.3' }, null, 2));
  assert.equal(getExtractedPluginVersion(root), '1.2.3');
});

function writeFakeBinary(filePath, version, mode = 0o755) {
  const script = [
    '#!/usr/bin/env bash',
    'if [ "$1" = "--version" ]; then',
    `  echo "code-graph-mcp ${version}"`,
    '  exit 0',
    'fi',
    'exit 0',
    `# ${'x'.repeat(1_100_000)}`,
    '',
  ].join('\n');
  fs.writeFileSync(filePath, script);
  fs.chmodSync(filePath, mode);
}

test('promoteVerifiedBinary accepts a runnable binary with the expected version', (t) => {
  const dir = mkDir(t, 'code-graph-bin-');
  const tmp = path.join(dir, 'code-graph-mcp.tmp');
  const dst = path.join(dir, 'code-graph-mcp');
  writeFakeBinary(tmp, '1.2.3');

  assert.equal(readBinaryVersion(tmp), '1.2.3');
  assert.equal(promoteVerifiedBinary(tmp, dst, '1.2.3'), true);
  assert.equal(fs.existsSync(tmp), false);
  assert.equal(fs.existsSync(dst), true);
});

test('promoteVerifiedBinary rejects binaries with mismatched version', (t) => {
  const dir = mkDir(t, 'code-graph-bin-');
  const tmp = path.join(dir, 'code-graph-mcp.tmp');
  const dst = path.join(dir, 'code-graph-mcp');
  writeFakeBinary(tmp, '1.2.2');

  assert.equal(promoteVerifiedBinary(tmp, dst, '1.2.3'), false);
  assert.equal(fs.existsSync(tmp), false);
  assert.equal(fs.existsSync(dst), false);
});

test('promoteVerifiedBinary promotes a non-executable (0644) download — curl -o regression', (t) => {
  // `curl -o` writes the tmp file as 0644 (no exec bit). promoteVerifiedBinary
  // must chmod before reading the version (readBinaryVersion executes the
  // binary), otherwise the version read fails with EACCES → null → false and
  // every download path silently wedges. Regression for the binary-stuck-at-old
  // -version deadlock.
  if (process.platform === 'win32') return; // no exec bit on win32
  const dir = mkDir(t, 'code-graph-bin-');
  const tmp = path.join(dir, 'code-graph-mcp.tmp');
  const dst = path.join(dir, 'code-graph-mcp');
  writeFakeBinary(tmp, '1.2.3', 0o644);

  assert.equal(readBinaryVersion(tmp), null, 'precondition: 0644 binary is not executable');
  assert.equal(promoteVerifiedBinary(tmp, dst, '1.2.3'), true);
  assert.equal(fs.existsSync(dst), true);
  assert.equal(fs.statSync(dst).mode & 0o111, 0o111, 'promoted binary is executable');
  assert.equal(readBinaryVersion(dst), '1.2.3');
});

test('promoteVerifiedBinary accepts a binary matching the expected sha256', (t) => {
  const dir = mkDir(t, 'code-graph-bin-');
  const tmp = path.join(dir, 'code-graph-mcp.tmp');
  const dst = path.join(dir, 'code-graph-mcp');
  writeFakeBinary(tmp, '1.2.3');
  const sha = crypto.createHash('sha256').update(fs.readFileSync(tmp)).digest('hex');
  assert.equal(promoteVerifiedBinary(tmp, dst, '1.2.3', sha), true);
  assert.equal(fs.existsSync(dst), true);
});

test('promoteVerifiedBinary rejects a binary whose sha256 mismatches the sidecar', (t) => {
  // Tampered/corrupted download: the checksum gate runs BEFORE chmod+exec, so a
  // mismatched binary is refused and never made executable. Platform-independent
  // (no exec needed to reject).
  const dir = mkDir(t, 'code-graph-bin-');
  const tmp = path.join(dir, 'code-graph-mcp.tmp');
  const dst = path.join(dir, 'code-graph-mcp');
  writeFakeBinary(tmp, '1.2.3');
  const wrongSha = 'deadbeef'.repeat(8); // 64 hex chars, deliberately wrong
  assert.equal(promoteVerifiedBinary(tmp, dst, '1.2.3', wrongSha), false);
  assert.equal(fs.existsSync(dst), false, 'tampered binary must not be promoted');
  assert.equal(fs.existsSync(tmp), false, 'tmp cleaned up on rejection');
});

test('promoteVerifiedBinary proceeds without a sidecar (TOFU back-compat)', (t) => {
  // Older releases ship no <asset>.sha256; a null expected hash must not block
  // install (the size + version-exec gates still apply).
  const dir = mkDir(t, 'code-graph-bin-');
  const tmp = path.join(dir, 'code-graph-mcp.tmp');
  const dst = path.join(dir, 'code-graph-mcp');
  writeFakeBinary(tmp, '1.2.3');
  assert.equal(promoteVerifiedBinary(tmp, dst, '1.2.3', null), true);
  assert.equal(fs.existsSync(dst), true);
});

test('cachedBinaryNeedsUpdate is version-aware, not existence-only', (t) => {
  const dir = mkDir(t, 'code-graph-heal-');
  const binaryPath = path.join(dir, 'code-graph-mcp');
  const latest = { version: '0.45.0', binaryUrl: 'https://example.com/bin' };

  // missing binary → needs update
  assert.equal(cachedBinaryNeedsUpdate(latest, { binaryPath }), true);

  // present but stale (the actual deadlock: shell at 0.45.0, binary at 0.16.6)
  fs.writeFileSync(binaryPath, 'x');
  assert.equal(
    cachedBinaryNeedsUpdate(latest, { binaryPath, readVersion: () => '0.16.6' }),
    true,
  );

  // present and current → no update
  assert.equal(
    cachedBinaryNeedsUpdate(latest, { binaryPath, readVersion: () => '0.45.0' }),
    false,
  );

  // no binaryUrl / null latest → no-op (nothing to download)
  assert.equal(cachedBinaryNeedsUpdate({ version: '0.45.0', binaryUrl: null }, { binaryPath }), false);
  assert.equal(cachedBinaryNeedsUpdate(null, { binaryPath }), false);
});

test('cachedBinaryStaleVsState bypasses throttle only for a present-but-stale binary', (t) => {
  const dir = mkDir(t, 'code-graph-throttle-');
  const binaryPath = path.join(dir, 'code-graph-mcp');
  fs.writeFileSync(binaryPath, 'x'); // present

  // no prior latestVersion → don't bypass (first run fetches anyway)
  assert.equal(cachedBinaryStaleVsState({}, { binaryPath }), false);
  assert.equal(cachedBinaryStaleVsState(null, { binaryPath }), false);

  // present + stale vs last known latest → bypass throttle (the 6h-gap fix)
  assert.equal(
    cachedBinaryStaleVsState({ latestVersion: '0.45.1' }, { binaryPath, readVersion: () => '0.16.6' }),
    true,
  );

  // present + current → stay throttled
  assert.equal(
    cachedBinaryStaleVsState({ latestVersion: '0.45.1' }, { binaryPath, readVersion: () => '0.45.1' }),
    false,
  );

  // missing binary → false here (the separate binaryMissing bypass handles it)
  fs.rmSync(binaryPath);
  assert.equal(cachedBinaryStaleVsState({ latestVersion: '0.45.1' }, { binaryPath }), false);
});

test('shouldCheck re-verifies an up-to-date state on a short cadence (release-publish race)', () => {
  const minsAgo = (m) => new Date(Date.now() - m * 60 * 1000).toISOString();

  // never checked → always check
  assert.equal(shouldCheck({}), true);

  // Bug repro: the last check reported "up to date" (updateAvailable:false) and a
  // release published moments later. 45min on, the plain 6h throttle kept the
  // stale answer latched (every session reopen re-reported up-to-date); the short
  // up-to-date cadence must allow a re-check so the new release is discovered.
  assert.equal(shouldCheck({ lastCheck: minsAgo(45), updateAvailable: false }), true);

  // within the short window → still throttled (don't hammer the API every call)
  assert.equal(shouldCheck({ lastCheck: minsAgo(10), updateAvailable: false }), false);

  // a pending-but-unfinished update keeps the 6h steady-state interval
  assert.equal(shouldCheck({ lastCheck: minsAgo(45), updateAvailable: true }), false);

  // rate-limit backoff (24h) wins even over the up-to-date short cadence
  assert.equal(shouldCheck({ lastCheck: minsAgo(120), updateAvailable: false, rateLimited: true }), false);
});

test('shouldCheck lets a forced (session-start) check bypass the soft throttle', () => {
  const minsAgo = (m) => new Date(Date.now() - m * 60 * 1000).toISOString();

  // A new session / explicit reload is a strong "get me latest" signal: a forced
  // check runs even inside the 30min up-to-date window (contrast the non-forced
  // call on the same state, which stays throttled).
  assert.equal(shouldCheck({ lastCheck: minsAgo(10), updateAvailable: false }, { force: true }), true);
  assert.equal(shouldCheck({ lastCheck: minsAgo(10), updateAvailable: false }), false);

  // ...but a short anti-hammer floor still applies, so a crash/reopen loop can't
  // pound the GitHub API on every restart.
  assert.equal(shouldCheck({ lastCheck: minsAgo(0.5), updateAvailable: false }, { force: true }), false);

  // Rate-limit backoff wins even over force — never push more requests into a 403.
  assert.equal(shouldCheck({ lastCheck: minsAgo(60), updateAvailable: false, rateLimited: true }, { force: true }), false);
});

test('selfHealStaleBinary wires the stale-binary check to a download (the v0.45.x glue)', async () => {
  const latest = { version: '0.45.2', binaryUrl: 'https://example/bin' };

  // Field failure mode: shell already at latest, binary pinned stale → MUST download.
  let downloaded = false;
  const healed = await selfHealStaleBinary(latest, {
    needsUpdate: () => true,
    download: async () => { downloaded = true; return true; },
  });
  assert.equal(downloaded, true, 'stale binary must trigger a download');
  assert.equal(healed, true);

  // Binary current → no download, no-op.
  let touched = false;
  const noop = await selfHealStaleBinary(latest, {
    needsUpdate: () => false,
    download: async () => { touched = true; return true; },
  });
  assert.equal(touched, false, 'current binary must not download');
  assert.equal(noop, false);

  // Download fails (no curl / network) → returns false so the next session retries.
  const failed = await selfHealStaleBinary(latest, {
    needsUpdate: () => true,
    download: async () => false,
  });
  assert.equal(failed, false);
});

test('parseLatestRelease selects the matching platform asset', () => {
  const latest = parseLatestRelease({
    tag_name: 'v1.2.3',
    tarball_url: 'https://example.com/tarball.tgz',
    assets: [
      { name: 'code-graph-mcp-linux-x64', browser_download_url: 'https://example.com/linux-x64' },
      { name: 'other', browser_download_url: 'https://example.com/other' },
    ],
  }, 'code-graph-mcp-linux-x64');

  assert.deepEqual(latest, {
    version: '1.2.3',
    tarballUrl: 'https://example.com/tarball.tgz',
    binaryUrl: 'https://example.com/linux-x64',
  });
});

// ── commandExists ──────────────────────────────────────────

test('commandExists returns true for a known command (node)', () => {
  assert.equal(commandExists('node'), true);
});

test('commandExists returns false for a non-existent command', () => {
  assert.equal(commandExists('__nonexistent_cmd_xyz_12345__'), false);
});

test('cachedBinaryPath returns expected platform binary path', () => {
  const p = cachedBinaryPath();
  const expectedName = process.platform === 'win32' ? 'code-graph-mcp.exe' : 'code-graph-mcp';
  assert.equal(path.basename(p), expectedName);
  assert.ok(p.includes('.cache') && p.includes('code-graph'),
    `expected cache path to live under ~/.cache/code-graph: ${p}`);
});

test('downloadBinary returns false for missing binaryUrl (no-op safety)', async () => {
  const result = await downloadBinary({ version: '1.0.0', binaryUrl: null });
  assert.equal(result, false);
});

test('downloadBinary returns false when latest is null', async () => {
  const result = await downloadBinary(null);
  assert.equal(result, false);
});

// ── Flag parsing ───────────────────────────────────────────

test('resolveProxy honors *_PROXY env vars, precedence, and NO_PROXY (L14)', () => {
  const U = 'https://api.github.com/repos/x/y/releases/latest';
  // No proxy configured → null (direct path unchanged for the common case).
  assert.equal(resolveProxy(U, {}), null);
  // HTTPS_PROXY selected; lowercase variant also honored.
  assert.equal(resolveProxy(U, { HTTPS_PROXY: 'http://p:8080' }), 'http://p:8080');
  assert.equal(resolveProxy(U, { https_proxy: 'http://p:3128' }), 'http://p:3128');
  // HTTP_PROXY is the fallback when no HTTPS_PROXY is present…
  assert.equal(resolveProxy(U, { HTTP_PROXY: 'http://p:1' }), 'http://p:1');
  // …but HTTPS_PROXY takes precedence over HTTP_PROXY.
  assert.equal(resolveProxy(U, { HTTPS_PROXY: 'http://s:1', HTTP_PROXY: 'http://h:2' }), 'http://s:1');
  // NO_PROXY: exact host, suffix (.github.com / *.github.com), and '*' all bypass.
  assert.equal(resolveProxy(U, { HTTPS_PROXY: 'http://p:1', NO_PROXY: 'api.github.com' }), null);
  assert.equal(resolveProxy(U, { HTTPS_PROXY: 'http://p:1', NO_PROXY: '.github.com' }), null);
  assert.equal(resolveProxy(U, { HTTPS_PROXY: 'http://p:1', NO_PROXY: '*.github.com' }), null);
  assert.equal(resolveProxy(U, { HTTPS_PROXY: 'http://p:1', no_proxy: '*' }), null);
  // NO_PROXY for an unrelated host does NOT bypass.
  assert.equal(resolveProxy(U, { HTTPS_PROXY: 'http://p:1', NO_PROXY: 'example.com' }), 'http://p:1');
  // Blank proxy value and unparseable target both yield null (no crash).
  assert.equal(resolveProxy(U, { HTTPS_PROXY: '   ' }), null);
  assert.equal(resolveProxy('not a url', { HTTPS_PROXY: 'http://p:1' }), null);
});

test('isInstallMissingMode detects --install-missing in argv', () => {
  assert.equal(isInstallMissingMode(['--install-missing']), true);
  assert.equal(isInstallMissingMode(['check', '--install-missing']), true);
  assert.equal(isInstallMissingMode(['check']), false);
  assert.equal(isInstallMissingMode([]), false);
});

test('isSilentMode honors --silent flag and CODE_GRAPH_AUTO_UPDATE_SILENT env', () => {
  assert.equal(isSilentMode(['--silent'], {}), true);
  assert.equal(isSilentMode([], { CODE_GRAPH_AUTO_UPDATE_SILENT: '1' }), true);
  assert.equal(isSilentMode([], {}), false);
});

test('fetchLatestRelease parses JSON without relying on global fetch', async () => {
  const latest = await fetchLatestRelease(async () => ({
    statusCode: 200,
    body: JSON.stringify({
      tag_name: 'v2.0.0',
      tarball_url: 'https://example.com/release.tgz',
      assets: [
        { name: 'code-graph-mcp-linux-x64', browser_download_url: 'https://example.com/bin' },
      ],
    }),
  }));

  assert.equal(latest.version, '2.0.0');
  assert.equal(latest.tarballUrl, 'https://example.com/release.tgz');
});
// ── refreshMarketplaceClone (v0.49.1 marketplace-staleness fix) ────────────

const { execFileSync: execGit } = require('child_process');
const { refreshMarketplaceClone, downloadAndInstall } = require('./auto-update');

function git(cwd, ...args) {
  return execGit('git', ['-C', cwd, '-c', 'user.email=t@t', '-c', 'user.name=t', ...args],
    { stdio: 'pipe', encoding: 'utf8', env: cleanGitEnv() });
}

test('git fixtures ignore inherited GIT_* env (H4 hermeticity)', (t) => {
  // A partial `git commit` runs the pre-commit hook with GIT_DIR / GIT_INDEX_FILE
  // exported into the environment. Every `git` this suite shells out to for its
  // tempdir fixtures would otherwise inherit them and mutate the REAL repo index
  // instead — v0.80.3 was this exact class, but that fix cleaned only the cargo
  // path in pre-commit.sh; this JS test section was the sibling hole (H4). The
  // git() helper must strip GIT_* so fixtures stay hermetic however the suite is
  // launched (hook, CI, or a direct `node --test`).
  const root = mkDir(t, 'code-graph-h4-');
  const bogus = path.join(root, 'bogus-gitdir');
  const saved = process.env.GIT_DIR;
  process.env.GIT_DIR = bogus;
  t.after(() => { if (saved === undefined) delete process.env.GIT_DIR; else process.env.GIT_DIR = saved; });

  const repo = path.join(root, 'repo');
  fs.mkdirSync(repo);
  git(repo, 'init', '-q', '-b', 'main');

  assert.ok(fs.existsSync(path.join(repo, '.git')),
    'git init must create repo/.git, not honor the inherited GIT_DIR');
  assert.ok(!fs.existsSync(bogus),
    'the inherited GIT_DIR must be ignored (no repo created there)');
});

test('refreshMarketplaceClone fast-forwards a stale clone', (t) => {
  const root = mkDir(t, 'code-graph-mp-');
  const remote = path.join(root, 'remote');
  const clone = path.join(root, 'clone');

  fs.mkdirSync(remote);
  git(remote, 'init', '-q', '-b', 'main');
  fs.writeFileSync(path.join(remote, 'marketplace.json'), '{"version":"0.48.0"}');
  git(remote, 'add', '.');
  git(remote, 'commit', '-q', '-m', 'v0.48.0');
  execGit('git', ['clone', '-q', remote, clone], { stdio: 'pipe' });

  // Remote advances (a release bumped marketplace.json) — clone is now stale.
  fs.writeFileSync(path.join(remote, 'marketplace.json'), '{"version":"0.49.0"}');
  git(remote, 'commit', '-q', '-am', 'v0.49.0');

  assert.equal(refreshMarketplaceClone({ dir: clone }), true);
  assert.match(fs.readFileSync(path.join(clone, 'marketplace.json'), 'utf8'), /0\.49\.0/);
});

test('refreshMarketplaceClone is a safe no-op on non-git dirs and pull failures', (t) => {
  const root = mkDir(t, 'code-graph-mp-');
  // Not a git repo → false, no throw.
  assert.equal(refreshMarketplaceClone({ dir: root }), false);
  // Missing dir → false, no throw.
  assert.equal(refreshMarketplaceClone({ dir: path.join(root, 'nope') }), false);
  // exec throws (diverged / dirty clone) → false, no throw.
  const fakeGitDir = path.join(root, 'repo');
  fs.mkdirSync(path.join(fakeGitDir, '.git'), { recursive: true });
  assert.equal(refreshMarketplaceClone({
    dir: fakeGitDir,
    exec: () => { throw new Error('not a fast-forward'); },
  }), false);
});

test('downloadAndInstall wires the marketplace refresh + binary download (orchestration glue)', async (t) => {
  // In-process with all side-effectful deps injected would still write the
  // manifest into the REAL ~/.cache (CACHE_DIR is bound at module load), so
  // run in a subprocess with a sandboxed HOME — same pattern as install-e2e.
  const sandboxHome = mkDir(t, 'code-graph-dai-');
  const script = `
    const fs = require('fs');
    const path = require('path');
    const { downloadAndInstall } = require(${JSON.stringify(path.join(__dirname, 'auto-update.js'))});
    const latest = { version: '9.9.9', tarballUrl: 'https://example/tar', binaryUrl: null };
    const calls = [];
    const exec = (cmd, args) => {
      calls.push(cmd);
      if (cmd === 'tar') {
        // Simulate extraction: produce claude-plugin/ with a matching version.
        const tmpDir = args[args.indexOf('-C') + 1];
        const mDir = path.join(tmpDir, 'claude-plugin', '.claude-plugin');
        fs.mkdirSync(mDir, { recursive: true });
        fs.writeFileSync(path.join(mDir, 'plugin.json'), JSON.stringify({ version: '9.9.9' }));
      }
    };
    (async () => {
      let refreshed = 0;
      let binDownloads = 0;
      const result = await downloadAndInstall(latest, {
        exec,
        cmdExists: () => true, // don't depend on host curl/tar
        refreshMarketplace: () => { refreshed++; return true; },
        downloadBin: async () => { binDownloads++; return true; },
      });
      console.log(JSON.stringify({ result, refreshed, binDownloads, calls }));
    })();
  `;
  const out = execGit(process.execPath, ['-e', script], {
    env: { ...process.env, HOME: sandboxHome },
    encoding: 'utf8',
  });
  const { result, refreshed, binDownloads } = JSON.parse(out.trim().split('\n').pop());
  assert.equal(result.pluginUpdated, true, 'plugin files must install from the extracted tarball');
  assert.equal(refreshed, 1, 'marketplace refresh must run exactly once after a plugin update');
  assert.equal(result.marketplaceRefreshed, true);
  assert.equal(binDownloads, 1, 'binary download must run');
  assert.equal(result.binaryUpdated, true);
  // Plugin landed in the sandboxed cache, not the real one.
  const dst = path.join(sandboxHome, '.claude', 'plugins', 'cache',
    'code-graph-mcp', 'code-graph-mcp', '9.9.9', '.claude-plugin', 'plugin.json');
  assert.equal(fs.existsSync(dst), true, 'plugin copied into sandbox plugins cache');
});

test('downloadAndInstall does NOT repoint install state when the plugin copy is skipped (version drift)', async (t) => {
  // Guard for a silent-breakage bug: when the extracted tarball's plugin.json version
  // doesn't match latest.version, the copy is skipped and pluginDst is never created.
  // installed_plugins.json must NOT be advanced to that nonexistent dir, or Claude Code
  // ends up pointed at a missing install while state reads "up to date".
  const sandboxHome = mkDir(t, 'code-graph-dai-skip-');
  const installedDir = path.join(sandboxHome, '.claude', 'plugins');
  fs.mkdirSync(installedDir, { recursive: true });
  const installedPath = path.join(installedDir, 'installed_plugins.json');
  fs.writeFileSync(installedPath, JSON.stringify({
    plugins: { 'code-graph-mcp@code-graph-mcp': [
      { installPath: '/old/install/path', version: '0.0.1', lastUpdated: 'seed' },
    ] },
  }));

  const script = `
    const fs = require('fs');
    const path = require('path');
    const { downloadAndInstall } = require(${JSON.stringify(path.join(__dirname, 'auto-update.js'))});
    const latest = { version: '9.9.9', tarballUrl: 'https://example/tar', binaryUrl: null };
    const exec = (cmd, args) => {
      if (cmd === 'tar') {
        // Extract a claude-plugin/ whose version DRIFTS from latest → copy is skipped.
        const tmpDir = args[args.indexOf('-C') + 1];
        const mDir = path.join(tmpDir, 'claude-plugin', '.claude-plugin');
        fs.mkdirSync(mDir, { recursive: true });
        fs.writeFileSync(path.join(mDir, 'plugin.json'), JSON.stringify({ version: '0.0.0' }));
      }
    };
    (async () => {
      const result = await downloadAndInstall(latest, {
        exec,
        cmdExists: () => true, // don't depend on host curl/tar — exercise the guard deterministically
        refreshMarketplace: () => true,
        downloadBin: async () => true,
      });
      console.log(JSON.stringify({ result }));
    })();
  `;
  const out = execGit(process.execPath, ['-e', script], {
    env: { ...process.env, HOME: sandboxHome },
    encoding: 'utf8',
  });
  const { result } = JSON.parse(out.trim().split('\n').pop());
  assert.equal(result.pluginUpdated, false, 'version drift must skip the plugin copy');

  // The pre-seeded record must be UNTOUCHED — not repointed to the 9.9.9 dir.
  const rec = JSON.parse(fs.readFileSync(installedPath, 'utf8'))
    .plugins['code-graph-mcp@code-graph-mcp'][0];
  assert.equal(rec.installPath, '/old/install/path',
    'installPath must not be repointed when the copy was skipped');
  assert.equal(rec.version, '0.0.1',
    'version must not be advanced when the copy was skipped');
});

// ── selfHealGlobalPkgs: keep global npm installs (CLI shim, platform relic) in step ──
// The drift it pins: the `code-graph-mcp` CLI on PATH is the GLOBAL
// @sdsrs/code-graph package, untouched by /plugin update or the binary
// self-heal — observed at 0.46.0 while the plugin ran 0.101.0; and an
// explicitly-installed top-level platform pkg relic (0.16.6) was the landmine
// behind the MCP connect-timeout incident.

test('selfHealGlobalPkgs refreshes stale globals and resets the attempt counter', async () => {
  const latest = { version: '0.101.0' };
  let installedSpecs = null;
  const patch = await selfHealGlobalPkgs(latest, {}, {
    readStale: () => [{ name: '@sdsrs/code-graph', version: '0.46.0' }],
    install: async (specs) => { installedSpecs = specs; return true; },
  });
  assert.deepEqual(installedSpecs, ['@sdsrs/code-graph@0.101.0']);
  assert.deepEqual(patch, { globalPkgHealVersion: '0.101.0', globalPkgHealAttempts: 0 });
});

test('selfHealGlobalPkgs never installs when nothing of ours is globally installed', async () => {
  let touched = false;
  const patch = await selfHealGlobalPkgs({ version: '0.101.0' }, {}, {
    readStale: () => [],
    install: async () => { touched = true; return true; },
  });
  assert.equal(touched, false, 'no global install → no npm run (never introduce one)');
  assert.deepEqual(patch, {});
});

test('selfHealGlobalPkgs clears a leftover counter once globals are healthy', async () => {
  const patch = await selfHealGlobalPkgs(
    { version: '0.101.0' },
    { globalPkgHealVersion: '0.101.0', globalPkgHealAttempts: 2 },
    { readStale: () => [], install: async () => true },
  );
  assert.deepEqual(patch, { globalPkgHealAttempts: 0, globalPkgHealVersion: null });
});

test('selfHealGlobalPkgs counts failures per target version and stops at the cap', async () => {
  const latest = { version: '0.101.0' };
  const failInstall = async () => false;
  const stale = () => [{ name: '@sdsrs/code-graph', version: '0.46.0' }];

  // Failure increments the counter for THIS target version.
  const p1 = await selfHealGlobalPkgs(latest, {}, { readStale: stale, install: failInstall });
  assert.deepEqual(p1, { globalPkgHealVersion: '0.101.0', globalPkgHealAttempts: 1 });

  // At the cap, install is no longer attempted for the same target.
  let touched = false;
  const p2 = await selfHealGlobalPkgs(
    latest,
    { globalPkgHealVersion: '0.101.0', globalPkgHealAttempts: 3 },
    { readStale: stale, install: async () => { touched = true; return true; } },
  );
  assert.equal(touched, false, 'capped target must not retry');
  assert.deepEqual(p2, {});

  // A NEW release re-arms the counter.
  let specs = null;
  const p3 = await selfHealGlobalPkgs(
    { version: '0.102.0' },
    { globalPkgHealVersion: '0.101.0', globalPkgHealAttempts: 3 },
    { readStale: () => [{ name: '@sdsrs/code-graph', version: '0.46.0' }],
      install: async (s) => { specs = s; return true; } },
  );
  assert.deepEqual(specs, ['@sdsrs/code-graph@0.102.0']);
  assert.deepEqual(p3, { globalPkgHealVersion: '0.102.0', globalPkgHealAttempts: 0 });
});

test('staleGlobalPkgs / globalPkgVersion read top-level global installs from disk', (t) => {
  const root = mkDir(t, 'global-pkgs-');
  const shellDir = path.join(root, '@sdsrs', 'code-graph');
  fs.mkdirSync(shellDir, { recursive: true });
  fs.writeFileSync(path.join(shellDir, 'package.json'), JSON.stringify({ version: '0.46.0' }));

  assert.equal(globalPkgVersion('@sdsrs/code-graph', [root]), '0.46.0');
  assert.equal(globalPkgVersion('@sdsrs/does-not-exist', [root]), null);

  const stale = staleGlobalPkgs('0.101.0', [root]);
  assert.deepEqual(stale, [{ name: '@sdsrs/code-graph', version: '0.46.0' }]);
  assert.deepEqual(staleGlobalPkgs('0.46.0', [root]), [],
    'a global install matching latest is not stale');
});

// ── P2.1: throttle-path global heal reach (RCA 2026-07-24) ──────────────────
// The post-fetch heal never runs on the throttle early-return; the only context
// that can SEE the user's nvm/global prefix is a throttled CLI run — so a stale
// global shim (0.101.0 while the binary reached 0.103.0) never healed. This
// predicate is what lets the throttle path still attempt it.

test('shouldHealGlobalsOnThrottle: true when a target version is known and a global is stale', () => {
  const yes = shouldHealGlobalsOnThrottle(
    { latestVersion: '0.103.0' },
    { readStale: () => [{ name: '@sdsrs/code-graph', version: '0.101.0' }] });
  assert.equal(yes, true);
});

test('shouldHealGlobalsOnThrottle: false when nothing of ours is globally stale', () => {
  const no = shouldHealGlobalsOnThrottle(
    { latestVersion: '0.103.0' },
    { readStale: () => [] });
  assert.equal(no, false, 'no stale global → no npm path on the hot throttle branch');
});

test('shouldHealGlobalsOnThrottle: false before a latest version is ever known', () => {
  assert.equal(shouldHealGlobalsOnThrottle({}, { readStale: () => [{ name: 'x', version: '0' }] }), false);
  assert.equal(shouldHealGlobalsOnThrottle(null, { readStale: () => [] }), false);
});

test('shouldHealGlobalsOnThrottle: false when the parent launcher already holds the install lock', () => {
  const prev = process.env.CODE_GRAPH_INSTALL_LOCK_HELD;
  process.env.CODE_GRAPH_INSTALL_LOCK_HELD = '1';
  try {
    assert.equal(shouldHealGlobalsOnThrottle(
      { latestVersion: '0.103.0' },
      { readStale: () => [{ name: '@sdsrs/code-graph', version: '0.101.0' }] }), false,
      'must not contend for the lock the launcher parent already holds (deadlock/double-npm)');
  } finally {
    if (prev === undefined) delete process.env.CODE_GRAPH_INSTALL_LOCK_HELD;
    else process.env.CODE_GRAPH_INSTALL_LOCK_HELD = prev;
  }
});

// ── inactiveNodeGlobalRelics: our globals stranded under a non-active node ──

test('inactiveNodeGlobalRelics: reports our global under a non-active node prefix, skips the active one', (t) => {
  const root = mkDir(t, 'nvm-relics-');
  const activeDir = path.join(root, 'v24.18.0', 'lib', 'node_modules');
  const relicDir = path.join(root, 'v24.11.1', 'lib', 'node_modules');
  for (const [dir, version] of [[activeDir, '0.103.0'], [relicDir, '0.46.0']]) {
    const pkg = path.join(dir, '@sdsrs', 'code-graph');
    fs.mkdirSync(pkg, { recursive: true });
    fs.writeFileSync(path.join(pkg, 'package.json'), JSON.stringify({ version }));
  }
  const relics = inactiveNodeGlobalRelics({ dirs: [activeDir, relicDir], activeDir });
  assert.deepEqual(relics, [{ name: '@sdsrs/code-graph', version: '0.46.0', nodeModulesDir: relicDir }],
    'the active node prefix is not a relic; only the stranded one is reported');
});

test('inactiveNodeGlobalRelics: empty when our global lives only under the active node', (t) => {
  const root = mkDir(t, 'nvm-norelic-');
  const activeDir = path.join(root, 'v24.18.0', 'lib', 'node_modules');
  const pkg = path.join(activeDir, '@sdsrs', 'code-graph');
  fs.mkdirSync(pkg, { recursive: true });
  fs.writeFileSync(path.join(pkg, 'package.json'), JSON.stringify({ version: '0.103.0' }));
  assert.deepEqual(inactiveNodeGlobalRelics({ dirs: [activeDir], activeDir }), []);
});

// ── getPlatformAssetName: libc gating ───────────────────────────────────────

test('getPlatformAssetName returns null on musl (no published asset → no futile download)', () => {
  // Alpine: the glibc build downloads fine but cannot exec, so promote always
  // rejected it and every SessionStart re-pulled ~40MB forever.
  assert.equal(getPlatformAssetName({ platform: 'linux', arch: 'x64', libc: 'musl' }), null);
  assert.equal(getPlatformAssetName({ platform: 'linux', arch: 'x64', libc: 'glibc' }),
    'code-graph-mcp-linux-x64');
  assert.equal(getPlatformAssetName({ platform: 'win32', arch: 'x64', libc: 'glibc' }),
    'code-graph-mcp-win32-x64.exe');
});

// ── cachedBinaryNeedsUpdate / cachedBinaryStaleVsState: ordered compare ─────

test('cached binary NEWER than latest is not downgraded; unreadable is healed', (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-newer-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const binaryPath = path.join(dir, 'code-graph-mcp');
  fs.writeFileSync(binaryPath, 'x');
  const latest = { version: '1.0.0', binaryUrl: 'https://example.com/bin' };

  // Newer than releases/latest (dev build / API lagging a publish) → keep it.
  assert.equal(
    cachedBinaryNeedsUpdate(latest, { binaryPath, readVersion: () => '9.9.9' }),
    false, 'a newer binary must not be replaced by an older release');
  // Unreadable --version → broken → let the heal replace it.
  assert.equal(
    cachedBinaryNeedsUpdate(latest, { binaryPath, readVersion: () => null }),
    true);

  const state = { latestVersion: '1.0.0' };
  assert.equal(
    cachedBinaryStaleVsState(state, { binaryPath, readVersion: () => '9.9.9' }),
    false, 'newer-than-state must not bypass the throttle');
  assert.equal(
    cachedBinaryStaleVsState(state, { binaryPath, readVersion: () => null }),
    true, 'unreadable binary bypasses the throttle so the heal can run');
});
