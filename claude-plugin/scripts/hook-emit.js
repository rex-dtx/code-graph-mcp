#!/usr/bin/env node
'use strict';
// Shared hook emit envelopes. One place defines the CC hookSpecificOutput JSON
// schema so the three sibling delivery hooks (pre-read-guide / pre-edit-guide /
// post-grep-inject) cannot drift apart (feedback_hook_class_bug_sweep — no
// inline copies of shared logic). DRY mirror of the project-root.js precedent.
//
// Why these two shapes:
//   - PreToolUse plain stdout on exit 0 goes to the DEBUG LOG ONLY — it never
//     reaches the model (CC docs, code.claude.com/docs/en/hooks.md, v2026-06).
//     `additionalContext` DOES reach the model, but only alongside a
//     permissionDecision. For the safe-tool pre hooks (Read fanout hint, Edit
//     impact summary) the elevation to `allow` is negligible and is what makes
//     the carried context visible.
//   - PostToolUse honors `additionalContext` permission-neutrally (no
//     permissionDecision), so the Bash-side grep answer can be injected without
//     skipping CC's default permission prompt for the underlying tool call.

/**
 * PreToolUse allow + additionalContext envelope (string, no trailing newline).
 * Used by the safe-tool delivery hooks (pre-read-guide / pre-edit-guide) so
 * their carried hint/impact text reaches the model instead of dying in the
 * debug log.
 * @param {string} text
 * @returns {string} JSON line
 */
function emitPreToolAllowContext(text) {
  return JSON.stringify({
    hookSpecificOutput: {
      hookEventName: 'PreToolUse',
      permissionDecision: 'allow',
      additionalContext: text,
    },
  });
}

/**
 * PostToolUse additionalContext envelope (string, no trailing newline).
 * Permission-neutral: NO permissionDecision, so the underlying Bash tool call's
 * permission flow is untouched while the answer still reaches the model.
 * @param {string} text
 * @returns {string} JSON line
 */
function emitPostToolContext(text) {
  return JSON.stringify({
    hookSpecificOutput: {
      hookEventName: 'PostToolUse',
      additionalContext: text,
    },
  });
}

module.exports = { emitPreToolAllowContext, emitPostToolContext };
