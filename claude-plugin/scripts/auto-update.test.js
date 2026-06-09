'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');

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