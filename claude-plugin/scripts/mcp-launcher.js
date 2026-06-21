#!/usr/bin/env node
'use strict';
/**
 * MCP server launcher — resolves binary via find-binary.js, auto-installs
 * if missing, then spawns with stdio forwarding for JSON-RPC.
 *
 * Used by .mcp.json so the plugin controls binary discovery instead of
 * relying on the binary being in PATH.
 */
const { spawn, spawnSync } = require('child_process');
const path = require('path');
const fs = require('fs');
const { isNonProjectCwd } = require('./project-detect');

// Set plugin root so find-binary.js can locate bundled/dev binaries
// Always derive from __dirname — CLAUDE_PLUGIN_ROOT can leak from other plugins
process.env._FIND_BINARY_ROOT = path.resolve(__dirname, '..');

// --- Tool-catalog dedup gate -----------------------------------------------
// If the user's project has its own .mcp.json registering a code-graph server
// (the recommended pattern for dev work on this repo — points at a local
// `target/release/code-graph-mcp` so usage telemetry lands in the project's
// `.code-graph/usage.jsonl`), the plugin's own MCP server adds a SECOND copy
// of the same 7 tools to the catalog, costing context budget and splitting
// the agent's choice between two equivalent namespaces.
//
// Detect that case and serve a minimal "0-tools" MCP stub so this plugin
// stops contributing to the catalog. Hooks, skills, agents stay registered
// (they live outside the MCP server). Env override
// `CODE_GRAPH_FORCE_PLUGIN_MCP=1` bypasses the gate.
function projectHasLocalCodeGraphMcp(cwd) {
  try {
    const p = path.join(cwd, '.mcp.json');
    if (!fs.existsSync(p)) return false;
    const cfg = JSON.parse(fs.readFileSync(p, 'utf8'));
    const servers = (cfg && cfg.mcpServers) || {};
    return Object.keys(servers).some(n => /code[-_]?graph/i.test(n));
  } catch { return false; }
}

function serveEmptyMcpStub() {
  let buf = '';
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', (chunk) => {
    buf += chunk;
    let nl;
    while ((nl = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, nl).trim();
      buf = buf.slice(nl + 1);
      if (!line) continue;
      let req;
      try { req = JSON.parse(line); } catch { continue; }
      if (!req || typeof req.method !== 'string') continue;
      // JSON-RPC notifications (id missing) get no response.
      if (typeof req.id === 'undefined') continue;
      const method = req.method;
      let result, error;
      if (method === 'initialize') {
        result = {
          protocolVersion: '2024-11-05',
          capabilities: { tools: { listChanged: false } },
          serverInfo: { name: 'code-graph-mcp (plugin stub, dedup)', version: '0.31.1' },
        };
      } else if (method === 'tools/list') {
        result = { tools: [] };
      } else if (method === 'resources/list') {
        result = { resources: [] };
      } else if (method === 'prompts/list') {
        result = { prompts: [] };
      } else {
        error = { code: -32601, message: 'method not found (plugin MCP is in dedup stub mode)' };
      }
      const resp = error
        ? { jsonrpc: '2.0', id: req.id, error }
        : { jsonrpc: '2.0', id: req.id, result };
      process.stdout.write(JSON.stringify(resp) + '\n');
    }
  });
  process.stdin.on('end', () => process.exit(0));
}

if (process.env.CODE_GRAPH_FORCE_PLUGIN_MCP !== '1' && projectHasLocalCodeGraphMcp(process.cwd())) {
  process.stderr.write(
    '[code-graph] project .mcp.json registers a code-graph server; ' +
    'plugin MCP serving 0 tools to avoid duplicate catalog entries. ' +
    'Set CODE_GRAPH_FORCE_PLUGIN_MCP=1 to override.\n'
  );
  serveEmptyMcpStub();
  return; // top-level function scope of mcp-launcher.js
}

// --- Non-project cwd gate ---------------------------------------------------
// In a non-project working directory (no .git/manifest — e.g. /tmp, where
// claude-mem-lite spawns ~2035 headless `claude -p` JSON-extraction calls that
// never use code-graph), don't spawn the binary at all: serve the same 0-tool
// stub. Eliminates the MCP-server spin-up + the ~780B `instructions` block +
// an empty .code-graph/index.db being created in throwaway dirs. Same
// CODE_GRAPH_FORCE_PLUGIN_MCP=1 override as the dedup gate above.
if (process.env.CODE_GRAPH_FORCE_PLUGIN_MCP !== '1' && isNonProjectCwd(process.cwd())) {
  process.stderr.write(
    '[code-graph] non-project cwd (no .git/manifest); plugin MCP serving 0 tools, ' +
    'no index created. Set CODE_GRAPH_FORCE_PLUGIN_MCP=1 to override.\n'
  );
  serveEmptyMcpStub();
  return;
}

const { findBinary, clearCache, unsupportedPlatformHint } = require('./find-binary');

let binary = findBinary();

// Auto-install binary if not found (first-time install)
if (!binary) {
  let version = 'latest';
  try {
    const pj = path.join(__dirname, '..', '.claude-plugin', 'plugin.json');
    version = JSON.parse(fs.readFileSync(pj, 'utf8')).version || 'latest';
  } catch { /* use latest */ }

  process.stderr.write(`[code-graph] Binary not found, installing @sdsrs/code-graph@${version}...\n`);
  const npmResult = spawnSync('npm', ['install', '-g', `@sdsrs/code-graph@${version}`], {
    timeout: 60000, stdio: ['ignore', 'pipe', 'pipe'], encoding: 'utf8',
  });
  if (npmResult.error || npmResult.status !== 0) {
    process.stderr.write('[code-graph] npm install failed.\n');
    if (npmResult.stderr) {
      process.stderr.write(npmResult.stderr.trim().split('\n').map(l => `[code-graph][npm] ${l}\n`).join(''));
    }
  } else {
    clearCache();
    binary = findBinary();
    if (binary) {
      process.stderr.write(`[code-graph] Installed at ${binary}\n`);
    }
  }
}

// Fallback: npm install may have succeeded but optionalDependencies for the
// platform binary can fail silently (npm tolerates OS-mismatch + flaky
// registry). Pull the platform binary directly from the GitHub release.
//
// --install-missing bypasses auto-update.js's isDevMode() short-circuit. The
// marketplace ships the full repo (including Cargo.toml at the workspace root),
// so dev-mode heuristics that look for Cargo.toml were misclassifying every
// marketplace install as dev mode and skipping this fallback (issue #12).
if (!binary) {
  process.stderr.write('[code-graph] Falling back to GitHub release download...\n');
  const result = spawnSync(
    process.execPath,
    [path.join(__dirname, 'auto-update.js'), '--silent', '--install-missing'],
    { timeout: 90000, stdio: ['ignore', 'pipe', 'pipe'], encoding: 'utf8' }
  );
  if (result.stderr && result.stderr.trim()) {
    process.stderr.write(result.stderr.trim().split('\n').map(l => `[code-graph][auto-update] ${l}\n`).join(''));
  }
  if (result.error) {
    process.stderr.write(`[code-graph] auto-update spawn failed: ${result.error.message}\n`);
  } else if (result.status !== 0) {
    process.stderr.write(`[code-graph] auto-update exited with status ${result.status}\n`);
  }
  clearCache();
  binary = findBinary();
  if (binary) {
    process.stderr.write(`[code-graph] Installed at ${binary}\n`);
  }
}

if (!binary) {
  const installedViaMarketplace = fs.existsSync(
    path.join(__dirname, '..', '.claude-plugin', 'plugin.json')
  );
  const platformHint = unsupportedPlatformHint();
  if (platformHint) {
    // Unsupported platform (Alpine/musl or native Windows-on-ARM): the per-platform
    // npm package does not exist, so the generic "npm install @sdsrs/code-graph-<plat>-<arch>"
    // suggestion below would point at a nonexistent package. Show the source/emulation hint.
    process.stderr.write('[code-graph] Binary not found.\n' + platformHint + '\n');
    process.exit(1);
  }
  process.stderr.write('[code-graph] Binary not found. Install manually:\n');
  if (installedViaMarketplace) {
    process.stderr.write(
      '  # Re-install the plugin via Claude Code marketplace:\n' +
      '  /plugin uninstall code-graph-mcp && /plugin install code-graph-mcp@code-graph-mcp\n' +
      '  # Or install the binary directly via npm:\n'
    );
  }
  process.stderr.write(
    '  npm install -g @sdsrs/code-graph @sdsrs/code-graph-' + process.platform + '-' + process.arch + '\n' +
    '  # or, equivalent split form:\n' +
    '  npm install -g @sdsrs/code-graph\n' +
    '  npm install -g @sdsrs/code-graph-' + process.platform + '-' + process.arch + '\n'
  );
  process.exit(1);
}

// Pre-spawn: verify binary is executable (catches macOS quarantine, permission issues)
try {
  fs.accessSync(binary, fs.constants.X_OK);
} catch {
  process.stderr.write(`[code-graph] Binary not executable: ${binary}\n`);
  if (process.platform === 'darwin') {
    process.stderr.write(
      'macOS may be quarantining the downloaded binary. Fix with:\n' +
      `  xattr -d com.apple.quarantine "${binary}"\n` +
      `  chmod +x "${binary}"\n`
    );
  } else {
    process.stderr.write(`Fix: chmod +x "${binary}"\n`);
  }
  process.exit(1);
}

// Spawn binary with stdio inheritance for MCP JSON-RPC
const child = spawn(binary, ['serve'], {
  stdio: 'inherit',
  env: process.env,
});

child.on('error', (err) => {
  process.stderr.write(`[code-graph] Failed to start: ${err.message}\n`);
  if (process.platform === 'darwin' && (err.code === 'EACCES' || err.code === 'EPERM')) {
    process.stderr.write(
      'macOS may be blocking this binary. Try:\n' +
      `  xattr -d com.apple.quarantine "${binary}"\n`
    );
  }
  // A glibc binary installed on musl (older npm ignores the `libc` field) is present
  // but execs into a loader error — surface the actionable platform hint.
  const platformHint = unsupportedPlatformHint();
  if (platformHint) process.stderr.write(platformHint + '\n');
  process.exit(1);
});

child.on('exit', (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code ?? 1);
  }
});
