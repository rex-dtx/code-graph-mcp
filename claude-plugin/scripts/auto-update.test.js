'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');
const crypto = require('crypto');

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
  downloadBinary,
  selfHealStaleBinary,
  isInstallMissingMode,
  isSilentMode,
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
    { stdio: 'pipe', encoding: 'utf8' });
}

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
