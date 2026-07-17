#!/usr/bin/env node
'use strict';
/**
 * Sync version across all project files.
 * Usage: node scripts/sync-versions.js <version>   # write mode
 *        node scripts/sync-versions.js --check      # read-only drift check
 * Example: node scripts/sync-versions.js 0.5.27
 *
 * --check reads every version site the script knows, compares each against the
 * canonical version (package.json), prints a per-file OK/DRIFT table, and exits
 * 1 on any drift. It writes NOTHING — safe to run in CI / pre-commit.
 */
const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const root = path.resolve(__dirname, '..');

const CHECK_MODE = process.argv[2] === '--check';

// In --check mode the canonical version is whatever package.json declares; every
// other site is compared against it. In write mode it's the CLI-supplied semver.
let version;
if (CHECK_MODE) {
  const pkgPath = path.join(root, 'package.json');
  version = JSON.parse(fs.readFileSync(pkgPath, 'utf8')).version;
  if (!version || !/^\d+\.\d+\.\d+$/.test(version)) {
    console.error(`--check: package.json version is not valid semver: ${JSON.stringify(version)}`);
    process.exit(1);
  }
} else {
  version = process.argv[2];
  if (!version || !/^\d+\.\d+\.\d+$/.test(version)) {
    console.error('Usage: node scripts/sync-versions.js <semver>');
    console.error('Example: node scripts/sync-versions.js 0.5.27');
    process.exit(1);
  }
}

const PLATFORM_PACKAGES = [
  'npm/linux-x64/package.json',
  'npm/linux-arm64/package.json',
  'npm/darwin-x64/package.json',
  'npm/darwin-arm64/package.json',
  'npm/win32-x64/package.json',
];

const updates = [
  {
    file: 'Cargo.toml',
    transform: (content) => content.replace(/^version = ".*"/m, `version = "${version}"`),
  },
  {
    file: 'package.json',
    json: true,
    transform: (obj) => {
      obj.version = version;
      // Sync optionalDependencies to same version
      if (obj.optionalDependencies) {
        for (const key of Object.keys(obj.optionalDependencies)) {
          obj.optionalDependencies[key] = version;
        }
      }
      return obj;
    },
  },
  {
    file: 'claude-plugin/.claude-plugin/plugin.json',
    json: true,
    transform: (obj) => { obj.version = version; return obj; },
  },
  {
    file: '.claude-plugin/marketplace.json',
    json: true,
    transform: (obj) => {
      if (obj.metadata) obj.metadata.version = version;
      if (obj.plugins && obj.plugins[0]) obj.plugins[0].version = version;
      return obj;
    },
  },
  // Platform npm packages
  ...PLATFORM_PACKAGES.map(file => ({
    file,
    json: true,
    transform: (obj) => { obj.version = version; return obj; },
  })),
];

// --check: read-only drift report. A site is DRIFT exactly when a write would
// change it (same transform, compared against the current bytes), so --check and
// the write path can never disagree about what is out of sync.
if (CHECK_MODE) {
  const rows = [];
  let drift = false;
  for (const { file, json, transform } of updates) {
    const filePath = path.join(root, file);
    if (!fs.existsSync(filePath)) {
      rows.push({ file, status: 'SKIP (not found)' });
      continue;
    }
    const original = fs.readFileSync(filePath, 'utf8');
    let result;
    if (json) {
      result = JSON.stringify(transform(JSON.parse(original)), null, 2) + '\n';
    } else {
      result = transform(original);
    }
    const ok = result === original;
    if (!ok) drift = true;
    rows.push({ file, status: ok ? 'OK' : 'DRIFT' });
  }
  const width = Math.max(...rows.map((r) => r.file.length));
  console.log(`Canonical version (package.json): ${version}\n`);
  for (const { file, status } of rows) {
    console.log(`  ${file.padEnd(width)}  ${status}`);
  }
  if (drift) {
    console.error(`\nDRIFT: one or more files disagree with package.json (${version}). Fix with: node scripts/sync-versions.js ${version}`);
    process.exit(1);
  }
  console.log(`\nAll version sites agree with package.json (${version}).`);
  process.exit(0);
}

let changed = 0;
for (const { file, json, transform } of updates) {
  const filePath = path.join(root, file);
  if (!fs.existsSync(filePath)) {
    console.warn(`  skip: ${file} (not found)`);
    continue;
  }
  const original = fs.readFileSync(filePath, 'utf8');
  let result;
  if (json) {
    const obj = JSON.parse(original);
    result = JSON.stringify(transform(obj), null, 2) + '\n';
  } else {
    result = transform(original);
  }
  if (result !== original) {
    fs.writeFileSync(filePath, result);
    console.log(`  updated: ${file}`);
    changed++;
  } else {
    console.log(`  unchanged: ${file}`);
  }
}

console.log(`\nVersion synced to ${version} (${changed} file${changed !== 1 ? 's' : ''} updated)`);

// Keep the dev MCP binary (.mcp.json → ./target/release/code-graph-mcp) aligned
// with the version we just wrote into Cargo.toml. Without this, every release
// leaves target/release/code-graph-mcp one version behind, and the next dev
// session's MCP `instructions` field reports the stale version.
// Opt-out: SYNC_VERSIONS_SKIP_BUILD=1 (tests + CI scenarios where building
// the actual crate is irrelevant or impossible).
if (process.env.SYNC_VERSIONS_SKIP_BUILD === '1') {
  console.log('\nSkipped cargo build (SYNC_VERSIONS_SKIP_BUILD=1).');
} else {
  // SYNC_VERSIONS_FEATURES (e.g. "embed-model") appends `--features <val>` to the
  // local rebuild. The dev MCP server (.mcp.json → target/release/code-graph-mcp)
  // is built with the crate default (`default = []`, no embedding) and therefore
  // reports `model_available:false` / `vec pending`; set this to "embed-model" so
  // the dev server can produce semantic vectors. CI/release set SKIP_BUILD=1, so
  // this never affects them.
  const features = (process.env.SYNC_VERSIONS_FEATURES || '').trim();
  const buildArgs = ['build', '--release'];
  if (features) buildArgs.push('--features', features);
  const featureNote = features ? ` (--features ${features})` : '';
  console.log(`\nRebuilding release binary so local MCP picks up new version${featureNote}...`);
  const t0 = Date.now();
  const result = spawnSync('cargo', buildArgs, {
    cwd: root,
    stdio: 'inherit',
  });
  const dt = ((Date.now() - t0) / 1000).toFixed(1);
  if (result.status !== 0) {
    console.error(`\nERROR: cargo build --release exited ${result.status} after ${dt}s.`);
    console.error('Version files were updated but target/release/code-graph-mcp is stale.');
    console.error('Fix the build, then run: cargo build --release');
    process.exit(2);
  }
  console.log(`\nRelease binary rebuilt in ${dt}s — target/release/code-graph-mcp now ${version}`);
}
