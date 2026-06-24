#!/usr/bin/env node
'use strict';
// PostToolUse(Bash) hook: deliver cg's AST-aware answer for a FOLDABLE grep that
// rode inside a COMPOUND command and therefore flew past the PreToolUse deny gate
// (`echo "..." && grep Sym tests/`, `git diff && grep ...`, `for s in …; do grep`).
//
// Why PostToolUse, not PreToolUse: the leading-grep deny path (pre-grep-guide)
// only fires when the command HEAD is grep — a grep buried after a side-effecting
// sibling has head=echo/git/for and is intentionally left alone there (denying the
// whole compound command would also block the sibling). Those greps RUN, so the
// only permission-neutral way to hand the model cg's structural view is a
// PostToolUse `additionalContext` injection (CC docs v2026-06: PostToolUse honors
// additionalContext; a PreToolUse `allow` would skip the default permission prompt
// for the underlying Bash call, which we must not do).
//
// Reuses the PreToolUse pure predicates wholesale (feedback_hook_class_bug_sweep —
// no inline copies of the grep gate): splitTopLevelSegments + classifyBlock pick
// the foldable segment; pickBlockPattern / translateBreToRg / extractSearchPath +
// sanitizeSearchPath + runGrepAnswer / runShowAnswer run the exact same answer the
// deny path would have. Best-effort: any miss (no hits / unavailable / no binary)
// exits silently with NO injection — an enhancement, never a new failure mode.

const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const { cgTmpDir } = require('./tmp-dir');
const { recordRecommendation } = require('./recommendation-log');
const { runGrepAnswer, runShowAnswer, sanitizeSearchPath } = require('./cg-answer');
const { emitPostToolContext } = require('./hook-emit');
const {
  splitTopLevelSegments,
  classifyBlock,
  pickBlockPattern,
  translateBreToRg,
  extractSearchPath,
  normalizeCommandPaths,
  rebaseRelativePaths,
  resolveProjectRoot,
} = require('./pre-grep-guide');

// The command HEAD is grep/rg/ag (or git grep, or a KEY=VALUE/env prefix). Kept
// loosely aligned with pre-grep-guide's GREP_VERB; this only gates "is this
// segment a search command" so the segment splitter doesn't have to.
const GREP_HEAD = /^\s*(?:env\s+)?(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*(?:git\s+grep|grep|rg|ag)\b/;

// --- Pure logic (testable) ---

/**
 * Find the FIRST foldable grep segment in a (possibly compound) command.
 * Splits via splitTopLevelSegments (NOT on a single `|` — that is an output
 * filter), then returns the first segment whose head is grep AND whose
 * classifyBlock is non-null (the answerable symbol/show tier).
 * @returns {{segment: string, block: {mode: string, symbols?: string[]}} | null}
 */
function findFoldableGrepSegment(cmd) {
  if (!cmd || typeof cmd !== 'string') return null;
  for (const segment of splitTopLevelSegments(cmd)) {
    if (!GREP_HEAD.test(segment)) continue;
    const block = classifyBlock(segment);
    if (block) return { segment, block };
  }
  return null;
}

// Short header so the model recognizes this as cg's parallel structural view of
// the grep it just ran (the grep already executed; this is additive context).
const INJECT_HEADER = '[code-graph] AST-aware view of your grep (ran alongside):';

function buildInjectText(answer, mode) {
  const lines = [INJECT_HEADER, answer.text];
  if (answer.truncated) {
    lines.push(mode === 'show'
      ? '(truncated — re-run the `code-graph-mcp show <symbol>` command above for full source)'
      : '(truncated — run `code-graph-mcp grep "<pattern>"` yourself for the full list)');
  }
  lines.push('Each hit shows its containing fn/module — use these results directly.');
  return lines.join('\n');
}

// Kill switch — matches the sibling-hook convention (pre-grep-guide.isSilenced).
function isSilenced(env = process.env) {
  return env.CODE_GRAPH_QUIET_HOOKS === '1';
}

// New per-this-hook opt-out (released-artifact requirement): =1 disables the
// PostToolUse compound-grep injection entirely, independent of QUIET_HOOKS.
function isInjectDisabled(env = process.env) {
  return env.CODE_GRAPH_NO_INJECT === '1';
}

// Per-command cooldown, mirror of pre-grep-guide's flag pattern but with a
// DISTINCT prefix so the two hooks never share a flag (a PreToolUse deny and a
// PostToolUse inject for different commands must not suppress each other).
function commandHash(cmd) {
  return crypto.createHash('sha1').update(String(cmd)).digest('hex').slice(0, 12);
}

function flagPath(cmd) {
  return path.join(cgTmpDir(), `.code-graph-postinject-${commandHash(cmd)}`);
}

function isOnCooldown(cmd, now = Date.now(), windowMs = 60000) {
  try {
    return now - fs.statSync(flagPath(cmd)).mtimeMs < windowMs;
  } catch { return false; }
}

function markCooldown(cmd) {
  try { fs.writeFileSync(flagPath(cmd), ''); } catch { /* ok */ }
}

// --- Main execution ---

function runMain() {
  if (isSilenced() || isInjectDisabled()) return;
  // Walk up from the persistent shell cwd (subdir-cwd fix — shared resolver).
  const shellCwd = process.cwd();
  const root = resolveProjectRoot(shellCwd);
  if (root === null) return;  // no index anywhere up to $HOME

  let input;
  try {
    // fd 0, not '/dev/stdin': the path form fails ENXIO on socketpair stdin.
    input = JSON.parse(fs.readFileSync(0, 'utf8'));
  } catch { return; }

  const rawCmd = (input.tool_input && input.tool_input.command) || '';
  if (!rawCmd) return;

  // Normalize abs paths under the root + rebase subdir-relative tokens, exactly
  // like the PreToolUse path, so the segment classifier and the answer scope see
  // root-relative paths. Cooldown stays keyed on the raw command.
  let cmd = normalizeCommandPaths(rawCmd, root);
  const relPrefix = path.relative(root, shellCwd);
  if (relPrefix) cmd = rebaseRelativePaths(cmd, relPrefix, root);

  const found = findFoldableGrepSegment(cmd);
  if (!found) return;

  if (isOnCooldown(rawCmd)) return;
  markCooldown(rawCmd);

  const { segment, block } = found;
  // Run the answer exactly like the deny path.
  const pattern = translateBreToRg(segment, pickBlockPattern(segment));
  const searchPath = sanitizeSearchPath(extractSearchPath(segment));
  let answer = { status: 'unavailable' };
  let answeredMode = block.mode;
  if (block.mode === 'show') {
    answer = runShowAnswer({ cwd: root, symbols: block.symbols });
    if (answer.status !== 'hits' && pattern) {
      answeredMode = 'grep';
      answer = runGrepAnswer({ cwd: root, pattern, searchPath });
    }
  } else if (pattern) {
    answer = runGrepAnswer({ cwd: root, pattern, searchPath });
  }

  // Only inject on hits — no-hits / unavailable / no-binary stay silent (the grep
  // already ran and produced its own output; a failed cg answer adds no value and
  // 0 hits ≠ proof of absence given regex-dialect differences).
  if (answer.status !== 'hits') return;

  recordRecommendation(root, {
    hook: 'grep', action: 'inject', answered: true,
    ...(pattern ? { pattern } : {}),
    mode: answeredMode,
  });
  process.stdout.write(emitPostToolContext(buildInjectText(answer, answeredMode)) + '\n');
}

if (require.main === module) {
  runMain();
}

module.exports = {
  findFoldableGrepSegment,
  buildInjectText,
  isSilenced,
  isInjectDisabled,
  commandHash,
  isOnCooldown,
  markCooldown,
};
