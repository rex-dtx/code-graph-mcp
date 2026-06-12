#!/usr/bin/env node
'use strict';
// PreToolUse(Edit) hook: auto-inject impact analysis when editing function definitions.
// Only fires when:
//   1. The old_string contains a function/method definition (signature being modified)
//   2. The symbol has 2+ production callers (high impact)
//   3. Same symbol not queried in last 2 minutes
// Silently exits otherwise — zero noise for normal edits.
const { execFileSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const { findBinary } = require('./find-binary');
const { cgTmpDir } = require('./tmp-dir');
const { resolveProjectRoot } = require('./project-root');
const { recordRecommendation } = require('./recommendation-log');

// v0.49 — walk up from the shell cwd (subdir-cwd fix). The per-cwd index.db
// gate kept this hook dark for entire sessions after `cd backend/` — daagu
// 2026-06-12: 115 edits, zero impact injections.
const cwd = resolveProjectRoot(process.cwd());
if (cwd === null) process.exit(0);

// Resolve binary the same way the other hooks do — bare PATH lookup misses
// npm-global installs on systems where the global bin dir isn't on PATH for
// non-login shells (a real failure mode reported in mem #8187).
const binary = findBinary();
if (!binary) process.exit(0);

// Hook-internal CLI runs are deliveries, not model-initiated conversions —
// the marker keeps them out of the recommendations.jsonl `use` funnel leg.
const internalEnv = { ...process.env, CODE_GRAPH_INTERNAL: '1' };

// --- Parse tool input ---
let input;
try {
  // fd 0, not '/dev/stdin': the path form fails ENXIO on socketpair stdin.
  input = JSON.parse(fs.readFileSync(0, 'utf8'));
} catch { process.exit(0); }

const oldStr = (input.tool_input && input.tool_input.old_string) || '';
if (!oldStr || oldStr.length < 10) process.exit(0);

// --- Extract function/method signature from the edited text ---
// Match function definitions across languages: Rust, JS/TS, Python, Go, Java/C#/Kotlin, Ruby, PHP
const fnPatterns = [
  /(?:pub\s+)?(?:async\s+)?fn\s+(\w+)/,                        // Rust
  /(?:export\s+)?(?:async\s+)?function\s+(\w+)/,                // JS/TS
  /(?:const|let|var)\s+(\w+)\s*=\s*(?:async\s+)?(?:\([^)]*\)|_)\s*=>/, // JS arrow
  /(?:async\s+)?(\w+)\s*\([^)]*\)\s*\{/,                       // JS method / Go func
  /def\s+(\w+)/,                                                // Python/Ruby
  /func\s+(\w+)/,                                               // Go/Swift
  /(?:public|private|protected|static|override|virtual|abstract|internal)\s+\S+\s+(\w+)\s*\(/, // Java/C#/Kotlin
  /(?:public\s+)?function\s+(\w+)/,                             // PHP
];

let symbol = null;
for (const pat of fnPatterns) {
  const m = oldStr.match(pat);
  if (m) {
    // Find the first captured group
    symbol = m[1] || m[2];
    break;
  }
}

// Fallback: if old_string is inside a function body (not a definition),
// extract a unique identifier from the code and grep for it to find the containing function
if (!symbol || symbol.length < 3) {
  const filePath = (input.tool_input && input.tool_input.file_path) || '';
  if (filePath && oldStr.length >= 10) {
    try {
      // Extract identifiers from old_string, try the most specific one first
      const identifiers = (oldStr.match(/\b([a-z]\w*(?:_\w+)+|[a-z]\w*(?:[A-Z]\w*)+|[A-Z]\w+\.\w+|[A-Z]\w+::\w+)\b/g) || [])
        .filter(id => id.length >= 6);
      const skipWords = new Set(['return', 'function', 'default', 'require', 'module', 'exports', 'import', 'console']);
      // Sort by length descending (longer = more specific = fewer matches)
      const candidates = [...new Set(identifiers)]
        .filter(id => !skipWords.has(id.toLowerCase()))
        .sort((a, b) => b.length - a.length);
      for (const candidate of candidates.slice(0, 5)) {
        try {
          const raw = execFileSync(binary, ['grep', candidate, filePath, '--json'], {
            cwd, timeout: 2000, encoding: 'utf8', stdio: ['pipe', 'pipe', 'pipe'],
            env: internalEnv,
          });
          const grepResult = JSON.parse(raw);
          // Pick this candidate if it has few matches (precise location)
          const withContainer = (grepResult || []).filter(m => m.container && m.container.name);
          if (withContainer.length > 0 && withContainer.length <= 5) {
            // If multiple containers, vote for the most common one
            const votes = {};
            for (const m of withContainer) {
              const cn = m.container.name;
              votes[cn] = (votes[cn] || 0) + 1;
            }
            const best = Object.entries(votes).sort((a, b) => b[1] - a[1])[0][0];
            symbol = best.includes('.') ? best.split('.').pop() : best.includes('::') ? best.split('::').pop() : best;
            break;
          }
        } catch { /* try next candidate */ }
      }
    } catch { /* grep failed or no match — fall through */ }
  }
}

if (!symbol || symbol.length < 3) process.exit(0);

// Skip common patterns that aren't real function names
if (isCommonKeyword(symbol)) {
  process.exit(0);
}

function isCommonKeyword(s) {
  return /^(if|for|while|switch|catch|else|return|new|get|set|try)$/i.test(s);
}

// --- Per-symbol cooldown: 2 minutes ---
const cooldownFile = path.join(cgTmpDir(), `.cg-impact-${symbol}`);
try {
  if (Date.now() - fs.statSync(cooldownFile).mtimeMs < 120000) process.exit(0);
} catch { /* first time for this symbol */ }

// --- Run impact analysis (JSON mode for programmatic parsing) ---
// Disambiguate via --file: file_path from tool_input is absolute, but the
// indexer stores files as repo-relative paths — converting here is what makes
// short generic symbol names (open, new, create, parse, from, init) resolve
// to a unique node instead of triggering the CLI's "Ambiguous symbol" error
// path, which previously caused silent exits for the most common edit cases.
const editedFile = (input.tool_input && input.tool_input.file_path) || '';
const relFile = editedFile ? path.relative(cwd, editedFile) : '';
let jsonResult;
try {
  const args = ['impact', symbol, '--json'];
  if (relFile && !relFile.startsWith('..')) args.push('--file', relFile);
  // v0.49 — use the resolved binary (bare 'code-graph-mcp' was PATH-dependent,
  // diverging from the findBinary() result the rest of the hook trusts).
  const raw = execFileSync(binary, args, {
    cwd,
    timeout: 2500,
    encoding: 'utf8',
    stdio: ['pipe', 'pipe', 'pipe'],
    env: internalEnv,
  });
  jsonResult = JSON.parse(raw);
} catch {
  // Symbol not found, timeout, or parse error — exit silently
  process.exit(0);
}

// CLI returns {"error": "..."} on ambiguous / not-found instead of throwing.
// Treat as silent skip — direct_callers will be undefined.
if (jsonResult && jsonResult.error) process.exit(0);

// --- Inject when the symbol has any caller (1+) ---
// Earlier gate was 2+ direct callers; reality is that editing a function with
// even one production caller benefits from a one-line impact summary, and the
// per-symbol 2-minute cooldown caps frequency. The 2+ floor was a remnant of
// the v0.21 "agent picks tools without push" assumption — same bias mem #8234
// records as bounded leverage at the bench level.
const directCallers = jsonResult.direct_callers || 0;
const totalCallers = jsonResult.total_callers || 0;
const affectedFiles = jsonResult.affected_files || 0;
const risk = jsonResult.risk || 'low';

if (directCallers < 1) process.exit(0);

// Mark cooldown
try { fs.writeFileSync(cooldownFile, ''); } catch { /* ok */ }

// Funnel visibility (v0.49): an injected impact summary is a delivered answer.
recordRecommendation(cwd, { hook: 'edit', action: 'hint', answered: true });

// --- Inject compact impact summary ---
const routeCount = jsonResult.affected_routes || 0;
const testCount = jsonResult.tests_affected || 0;

let summary = `[code-graph:impact] ${symbol}() — Risk: ${risk}\n`;
summary += `  ${directCallers} direct callers, ${totalCallers} total across ${affectedFiles} files`;
if (routeCount > 0) summary += `, ${routeCount} routes affected`;
if (testCount > 0) summary += ` (${testCount} tests)`;
summary += '\n';

// List direct callers compactly
const callers = (jsonResult.callers || []).filter(c => c.depth === 1);
if (callers.length > 0) {
  summary += '  Callers: ' + callers.map(c => `${c.name} (${c.file})`).join(', ') + '\n';
}

process.stdout.write(summary);
