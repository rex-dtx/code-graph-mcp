#!/usr/bin/env node
'use strict';
/**
 * Tests for scripts/sync-versions.js — release tooling that bumps the version
 * across 9 files atomically. A bug here means red CI / "already published"
 * E403s on republish (memory: feedback_version_sync.md).
 *
 * Strategy: copy sync-versions.js + fixture file tree into a temp dir, run it
 * as a subprocess, assert every target file got the new version.
 *
 * Run: node --test scripts/sync-versions.test.js
 */
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');

const SCRIPT_PATH = path.resolve(__dirname, 'sync-versions.js');

const PLATFORM_TARGETS = [
  'npm/linux-x64/package.json',
  'npm/linux-arm64/package.json',
  'npm/darwin-x64/package.json',
  'npm/darwin-arm64/package.json',
  'npm/win32-x64/package.json',
];

function mkdtempT(t, prefix) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function writeJson(p, obj) {
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, JSON.stringify(obj, null, 2) + '\n');
}

/**
 * Set up a minimal fixture mirroring the real repo layout that sync-versions
 * touches. sync-versions resolves root via path.resolve(__dirname, '..'), so
 * we copy the script under temp/scripts/ — its __dirname will be temp/scripts
 * and its derived root will be temp/.
 */
function setupFixture(t, oldVersion = '0.0.1') {
  const root = mkdtempT(t, 'sync-versions-fixture-');
  fs.mkdirSync(path.join(root, 'scripts'));
  fs.copyFileSync(SCRIPT_PATH, path.join(root, 'scripts', 'sync-versions.js'));

  fs.writeFileSync(
    path.join(root, 'Cargo.toml'),
    `[package]\nname = "fixture"\nversion = "${oldVersion}"\nedition = "2021"\n`,
  );

  writeJson(path.join(root, 'package.json'), {
    name: '@sdsrs/code-graph',
    version: oldVersion,
    optionalDependencies: {
      '@sdsrs/code-graph-linux-x64': oldVersion,
      '@sdsrs/code-graph-linux-arm64': oldVersion,
      '@sdsrs/code-graph-darwin-x64': oldVersion,
      '@sdsrs/code-graph-darwin-arm64': oldVersion,
      '@sdsrs/code-graph-win32-x64': oldVersion,
    },
  });

  writeJson(path.join(root, 'claude-plugin/.claude-plugin/plugin.json'), {
    name: 'code-graph-mcp', version: oldVersion,
  });

  writeJson(path.join(root, '.claude-plugin/marketplace.json'), {
    metadata: { version: oldVersion },
    plugins: [{ name: 'code-graph-mcp', version: oldVersion }],
  });

  for (const rel of PLATFORM_TARGETS) {
    writeJson(path.join(root, rel), { name: `@sdsrs/${path.basename(path.dirname(rel))}`, version: oldVersion });
  }

  return root;
}

function readJson(p) {
  return JSON.parse(fs.readFileSync(p, 'utf8'));
}

// Test env: skip the auto-rebuild step. The fixture is not a real Cargo crate,
// so a real `cargo build --release` would fail or scan the host system. All
// existing tests assert version-sync behavior, not build behavior — they set
// SYNC_VERSIONS_SKIP_BUILD=1. Dedicated build/skip tests live further down.
const SKIP_BUILD_ENV = { ...process.env, SYNC_VERSIONS_SKIP_BUILD: '1' };

test('sync-versions bumps Cargo.toml + 8 JSON files atomically', (t) => {
  const root = setupFixture(t);
  const stdout = execFileSync(
    process.execPath,
    [path.join(root, 'scripts', 'sync-versions.js'), '1.2.3'],
    { cwd: root, stdio: 'pipe', encoding: 'utf8', env: SKIP_BUILD_ENV },
  );
  // Lock the success-path total. A regression that drops one of the 9 targets
  // without removing the per-target assertions below would otherwise pass
  // (each remaining target gets checked individually) — the count assertion
  // is the only thing that flags "we silently stopped touching one of them".
  assert.match(stdout, /\(9 files updated\)/,
    'atomic-bump on a complete fixture must report exactly 9 files updated');

  // Cargo.toml uses regex replace, not JSON
  const cargoToml = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8');
  assert.match(cargoToml, /^version = "1\.2\.3"$/m,
    'Cargo.toml version line must be rewritten in-place');

  // package.json: top-level + every optionalDependency
  const pkg = readJson(path.join(root, 'package.json'));
  assert.equal(pkg.version, '1.2.3', 'package.json top-level version');
  for (const [dep, ver] of Object.entries(pkg.optionalDependencies)) {
    assert.equal(ver, '1.2.3', `optionalDependencies["${dep}"] must follow top-level version`);
  }

  // plugin.json + marketplace.json
  assert.equal(readJson(path.join(root, 'claude-plugin/.claude-plugin/plugin.json')).version, '1.2.3');
  const market = readJson(path.join(root, '.claude-plugin/marketplace.json'));
  assert.equal(market.metadata.version, '1.2.3', 'marketplace metadata.version');
  assert.equal(market.plugins[0].version, '1.2.3', 'marketplace plugins[0].version');

  // All 5 platform packages
  for (const rel of PLATFORM_TARGETS) {
    assert.equal(readJson(path.join(root, rel)).version, '1.2.3', `${rel} version`);
  }
});

test('sync-versions rejects invalid semver and exits non-zero', (t) => {
  const root = setupFixture(t);
  const result = require('child_process').spawnSync(
    process.execPath,
    [path.join(root, 'scripts', 'sync-versions.js'), 'not-a-version'],
    { cwd: root, stdio: 'pipe', encoding: 'utf8', env: SKIP_BUILD_ENV },
  );
  assert.equal(result.status, 1, 'invalid semver must exit 1');
  assert.match(result.stderr, /Usage:/, 'stderr should print usage hint');

  // Files unchanged
  assert.match(fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8'), /version = "0\.0\.1"/,
    'Cargo.toml must not be touched on bad input');
  assert.equal(readJson(path.join(root, 'package.json')).version, '0.0.1',
    'package.json must not be touched on bad input');
});

test('sync-versions skips files that are missing without erroring', (t) => {
  const root = setupFixture(t);
  // Remove one platform package — sync-versions should warn-skip, not crash.
  fs.rmSync(path.join(root, 'npm/win32-x64'), { recursive: true });

  const result = require('child_process').spawnSync(
    process.execPath,
    [path.join(root, 'scripts', 'sync-versions.js'), '1.2.3'],
    { cwd: root, stdio: 'pipe', encoding: 'utf8', env: SKIP_BUILD_ENV },
  );
  assert.equal(result.status, 0, 'exit 0 even when a target is missing');
  // skip messages go to stderr (console.warn); success summary lands on stdout.
  assert.match(result.stderr, /skip: npm\/win32-x64\/package\.json/,
    'stderr must surface the skipped file via console.warn');
  assert.match(result.stdout, /\(8 files updated\)/,
    'success summary should reflect the 8 files that did get bumped');

  // Remaining platform packages still got bumped
  for (const rel of PLATFORM_TARGETS.filter(p => !p.includes('win32-x64'))) {
    assert.equal(readJson(path.join(root, rel)).version, '1.2.3');
  }
});

test('sync-versions is idempotent — running with the same version reports unchanged', (t) => {
  const root = setupFixture(t, '1.2.3');
  const out = execFileSync(process.execPath, [path.join(root, 'scripts', 'sync-versions.js'), '1.2.3'], {
    cwd: root, stdio: 'pipe', encoding: 'utf8', env: SKIP_BUILD_ENV,
  });
  // All files are already at 1.2.3 — script should report 0 updated.
  assert.match(out, /\(0 files? updated\)/, 'idempotent run must report 0 changes');
});

test('SYNC_VERSIONS_SKIP_BUILD=1 skips cargo build and announces the skip', (t) => {
  const root = setupFixture(t);
  const stdout = execFileSync(
    process.execPath,
    [path.join(root, 'scripts', 'sync-versions.js'), '1.2.3'],
    { cwd: root, stdio: 'pipe', encoding: 'utf8', env: SKIP_BUILD_ENV },
  );
  assert.match(stdout, /Skipped cargo build \(SYNC_VERSIONS_SKIP_BUILD=1\)/,
    'must print the skip notice so the operator knows binary may be stale');
  assert.doesNotMatch(stdout, /Rebuilding release binary/,
    'must not run the build step when SKIP env is set');
});

test('--check exits 0 when every version site agrees with package.json', (t) => {
  const root = setupFixture(t, '1.2.3');
  const result = require('child_process').spawnSync(
    process.execPath,
    [path.join(root, 'scripts', 'sync-versions.js'), '--check'],
    { cwd: root, stdio: 'pipe', encoding: 'utf8' },
  );
  assert.equal(result.status, 0, 'a consistent tree must exit 0');
  assert.match(result.stdout, /All version sites agree with package\.json \(1\.2\.3\)/,
    '--check must confirm agreement on the success path');
  assert.doesNotMatch(result.stdout, /DRIFT/, 'no site should be flagged when all agree');
});

test('--check exits 1, flags the drifted file, and writes nothing', (t) => {
  const root = setupFixture(t, '1.2.3');
  // Introduce drift: one platform package lags behind package.json's 1.2.3.
  const drifted = path.join(root, 'npm/linux-x64/package.json');
  writeJson(drifted, { name: '@sdsrs/linux-x64', version: '0.0.1' });

  const result = require('child_process').spawnSync(
    process.execPath,
    [path.join(root, 'scripts', 'sync-versions.js'), '--check'],
    { cwd: root, stdio: 'pipe', encoding: 'utf8' },
  );
  assert.equal(result.status, 1, 'any drift must exit 1');
  assert.match(result.stdout, /npm\/linux-x64\/package\.json\s+DRIFT/,
    'the lagging file must be marked DRIFT in the table');

  // Read-only contract: --check must not rewrite the drifted file (or any other).
  assert.equal(readJson(drifted).version, '0.0.1',
    '--check must leave the drifted file untouched');
  assert.equal(readJson(path.join(root, 'package.json')).version, '1.2.3',
    '--check must leave package.json untouched');
});

test('default (no SKIP env) attempts cargo build — fixture is not a crate so build fails with exit 2', (t) => {
  const root = setupFixture(t);
  // Sanity: this fixture has Cargo.toml [package] but no src/, so a real
  // `cargo build --release` will error. We exploit that to confirm the build
  // step runs (and that we surface the right exit code + diagnostic) without
  // needing to vendor a fake cargo or actually compile anything.
  //
  // The PATH passthrough is required so `cargo` resolves. If cargo is missing
  // from the host PATH, status will be null (ENOENT) and the assertion below
  // still catches non-zero-exit semantics.
  const env = { ...process.env };
  delete env.SYNC_VERSIONS_SKIP_BUILD;
  const result = require('child_process').spawnSync(
    process.execPath,
    [path.join(root, 'scripts', 'sync-versions.js'), '1.2.3'],
    { cwd: root, stdio: 'pipe', encoding: 'utf8', env },
  );

  // Version files still got written before build was attempted.
  assert.equal(readJson(path.join(root, 'package.json')).version, '1.2.3',
    'version sync must happen before build so partial-failure does not leave files unchanged');

  // Build step ran (its banner is on stdout).
  assert.match(result.stdout, /Rebuilding release binary/,
    'default invocation must announce + attempt the build');

  // Failed build surfaces exit 2 + remediation hint.
  assert.equal(result.status, 2,
    'cargo build failure must exit 2 (distinct from semver-parse exit 1)');
  assert.match(result.stderr, /Version files were updated but target\/release\/code-graph-mcp is stale/,
    'stderr must tell the operator what state the repo is in');
  assert.match(result.stderr, /Fix the build, then run: cargo build --release/,
    'stderr must give the recovery command');
});
