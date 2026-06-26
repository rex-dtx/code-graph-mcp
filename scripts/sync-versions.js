#!/usr/bin/env node
'use strict';
/**
 * Sync version across all project files.
 * Usage: node scripts/sync-versions.js <version>
 * Example: node scripts/sync-versions.js 0.5.27
 */
const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const version = process.argv[2];
if (!version || !/^\d+\.\d+\.\d+$/.test(version)) {
  console.error('Usage: node scripts/sync-versions.js <semver>');
  console.error('Example: node scripts/sync-versions.js 0.5.27');
  process.exit(1);
}

const root = path.resolve(__dirname, '..');

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
