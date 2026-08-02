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

/**
 * Every transform here is conditional on the site still LOOKING the way it did
 * when the rule was written — a regex that must match, an `if (obj.metadata)`
 * that must hold. When that shape drifts, the transform quietly does nothing,
 * and "nothing changed" is byte-identical to "already correct": write mode
 * prints `unchanged:` and --check prints `OK`. The site silently stops being
 * managed, and the release still ships.
 *
 * `verify` closes that hole by asserting the POST-state instead of inferring it
 * from the absence of a diff. It runs on the exact bytes that would land on
 * disk, in both --check and write mode, and returns a problem string or null.
 * A failure is NOT drift (re-running the script cannot fix it), so it gets its
 * own exit code — see EXIT_UNMANAGED.
 */
const EXIT_UNMANAGED = 3;

/** Assert `version` sits at each dotted path (array indices allowed) of a JSON site. */
const expectJsonVersion = (...paths) => (text) => {
  const obj = JSON.parse(text);
  const bad = paths.filter(
    (p) => p.split('.').reduce((o, k) => (o == null ? o : o[k]), obj) !== version
  );
  return bad.length ? `expected "${version}" at ${bad.join(', ')}` : null;
};

const updates = [
  {
    file: 'Cargo.toml',
    transform: (content) => content.replace(/^version = ".*"/m, `version = "${version}"`),
    verify: (content) =>
      new RegExp(`^version = "${version.replace(/\./g, '\\.')}"$`, 'm').test(content)
        ? null
        : `no \`version = "${version}"\` line — the [package] version pattern no longer matches`,
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
    verify: (text) => {
      const obj = JSON.parse(text);
      if (obj.version !== version) return `top-level version is ${JSON.stringify(obj.version)}`;
      // The platform binaries are resolved through optionalDependencies; one
      // left behind means `npm i` pulls a mismatched binary for that platform.
      const deps = obj.optionalDependencies || {};
      const lagging = Object.keys(deps).filter((k) => deps[k] !== version);
      if (!Object.keys(deps).length) return 'optionalDependencies is empty or absent';
      return lagging.length ? `optionalDependencies out of sync: ${lagging.join(', ')}` : null;
    },
  },
  {
    file: 'claude-plugin/.claude-plugin/plugin.json',
    json: true,
    transform: (obj) => { obj.version = version; return obj; },
    verify: expectJsonVersion('version'),
  },
  {
    file: '.claude-plugin/marketplace.json',
    json: true,
    transform: (obj) => {
      if (obj.metadata) obj.metadata.version = version;
      if (obj.plugins && obj.plugins[0]) obj.plugins[0].version = version;
      return obj;
    },
    // Both writes sit behind an `if`. Renaming either key made this file report
    // "unchanged" forever while shipping a stale version to the marketplace.
    verify: expectJsonVersion('metadata.version', 'plugins.0.version'),
  },
  {
    // The shipped GitHub Actions template pins the npm package it runs. A pin
    // that rots is a pin users will "fix" by reaching for a floating tag, so it
    // tracks the release like every other version site. The scoped package name
    // is part of the pattern on purpose: the unscoped `code-graph-mcp` name on
    // npm belongs to an unrelated publisher, so a rewrite must never be able to
    // land on it.
    file: 'claude-plugin/templates/code-graph-snapshot.yml',
    transform: (content) =>
      content.replace(
        /-p @sdsrs\/code-graph@\d+\.\d+\.\d+/,
        `-p @sdsrs/code-graph@${version}`
      ),
    // This is the one site where a silent no-op is a SUPPLY-CHAIN event, not a
    // cosmetic one: revert the line to `npx -y code-graph-mcp@latest` and the
    // regex stops matching, so the template sails through both faces unchanged
    // and every consumer's release workflow executes a stranger's package with
    // `contents: write` in hand. Assert the pin positively AND assert the
    // unscoped spelling is absent — a rewrite is not the only way it can appear.
    verify: (content) => {
      // Trailing (?![\d.]) so `@1.2.3` does not satisfy a check for `@1.2` —
      // matching on a prefix would greenlight a half-rewritten pin.
      const pinned = new RegExp(`-p @sdsrs/code-graph@${version.replace(/\./g, '\\.')}(?![\\d.])`);
      if (!pinned.test(content)) {
        return `no \`-p @sdsrs/code-graph@${version}\` pin — the npx invocation no longer matches the rewrite pattern`;
      }
      if (/npx[^\n]*(?<!@sdsrs\/)\bcode-graph-mcp@/.test(content)) {
        return 'template invokes the UNSCOPED `code-graph-mcp` npm package (belongs to an unrelated publisher)';
      }
      return null;
    },
  },
  // Platform npm packages
  ...PLATFORM_PACKAGES.map(file => ({
    file,
    json: true,
    transform: (obj) => { obj.version = version; return obj; },
    verify: expectJsonVersion('version'),
  })),
];

/** Run a site's `verify` against its post-transform bytes. Returns null or a reason. */
function verifySite(site, text) {
  if (!site.verify) return null;
  try {
    return site.verify(text);
  } catch (err) {
    return `verify threw (${err.code || err.name}: ${err.message.split('\n')[0]})`;
  }
}

// --check: read-only drift report. A site is DRIFT exactly when a write would
// change it (same transform, compared against the current bytes), so --check and
// the write path can never disagree about what is out of sync.
if (CHECK_MODE) {
  const rows = [];
  let drift = false;
  let unreadable = false;
  const unmanaged = [];
  for (const site of updates) {
    const { file, json, transform } = site;
    const filePath = path.join(root, file);
    // A site that is expected but absent is NOT agreement. This used to
    // `continue` without setting drift, so deleting a platform package.json made
    // the gate print "All version sites agree" and exit 0 — eight sites checked,
    // nine claimed.
    if (!fs.existsSync(filePath)) {
      rows.push({ file, status: 'MISSING' });
      drift = true;
      continue;
    }
    // Per-site try/catch. Unguarded, one corrupt or unreadable file threw out of
    // the loop before a single row printed, so the operator saw a stack trace
    // instead of the table — and every site after it went unexamined. With real
    // drift elsewhere the crash hid it completely, while the exit code (1, from
    // node's default uncaught-throw) was indistinguishable from a clean drift
    // report.
    let status;
    try {
      const original = fs.readFileSync(filePath, 'utf8');
      const result = json
        ? JSON.stringify(transform(JSON.parse(original)), null, 2) + '\n'
        : transform(original);
      const ok = result === original;
      if (!ok) drift = true;
      // Verify BEFORE deciding the row is clean: a site whose pattern stopped
      // matching produces result === original, i.e. the exact shape of "OK".
      const problem = verifySite(site, result);
      if (problem) {
        unmanaged.push({ file, problem });
        status = 'UNMANAGED';
      } else {
        status = ok ? 'OK' : 'DRIFT';
      }
    } catch (err) {
      unreadable = true;
      status = `UNREADABLE (${err.code || err.name}: ${err.message.split('\n')[0]})`;
    }
    rows.push({ file, status });
  }
  const width = Math.max(...rows.map((r) => r.file.length));
  console.log(`Canonical version (package.json): ${version}\n`);
  for (const { file, status } of rows) {
    console.log(`  ${file.padEnd(width)}  ${status}`);
  }
  // Distinct exit codes so a CI consumer can tell "versions disagree" (fixable
  // by re-running this script) from "a site could not be read at all" (not).
  if (unreadable) {
    console.error(`\nUNREADABLE: one or more version sites could not be parsed. Drift status for them is UNKNOWN${drift ? ' (and at least one readable site is out of sync)' : ''}.`);
    process.exit(2);
  }
  // Reported before DRIFT because the remediation is different in kind: an
  // UNMANAGED site is one this script can no longer write, so telling the
  // operator to re-run it would be a lie.
  if (unmanaged.length) {
    console.error('\nUNMANAGED: a version site no longer matches the rule that maintains it.');
    console.error('Re-running this script will NOT fix these — the transform is a silent no-op:');
    for (const { file, problem } of unmanaged) console.error(`  ${file}: ${problem}`);
    process.exit(EXIT_UNMANAGED);
  }
  if (drift) {
    console.error(`\nDRIFT: one or more files disagree with package.json (${version}). Fix with: node scripts/sync-versions.js ${version}`);
    process.exit(1);
  }
  // Wording is asserted verbatim by scripts/sync-versions.test.js (which ci.yml
  // and pre-commit.sh both run) — the count I briefly added here turned that
  // gate red. If it ever changes, change the test in the same commit.
  console.log(`\nAll version sites agree with package.json (${version}).`);
  process.exit(0);
}

let changed = 0;
const unmanaged = [];
for (const site of updates) {
  const { file, json, transform } = site;
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
  // release.yml runs THIS face, not --check. Without the post-state assertion a
  // site whose pattern rotted reports `unchanged:` and the release ships it.
  const problem = verifySite(site, result);
  if (problem) unmanaged.push({ file, problem });
}

console.log(`\nVersion synced to ${version} (${changed} file${changed !== 1 ? 's' : ''} updated)`);

// Fail before the rebuild so the diagnostic is the last thing on screen and the
// release job stops here. Files already written stay written — same partial-
// failure contract the cargo-build branch below has always had.
if (unmanaged.length) {
  console.error('\nUNMANAGED: a version site could not be written by its own rule.');
  console.error('It reported "unchanged" because the transform matched nothing, not because it was correct:');
  for (const { file, problem } of unmanaged) console.error(`  ${file}: ${problem}`);
  process.exit(EXIT_UNMANAGED);
}

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
    windowsHide: true, // no console flash — same rule as claude-plugin/scripts/proc-opts
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
