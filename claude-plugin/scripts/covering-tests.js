'use strict';
// Pure formatter for edit-time covering-test targeting.
//
// pre-edit-guide.js feeds this the `test_callers` from `impact --json` (the tests
// that exercise the symbol being edited) plus the edited file's path. It returns a
// summary fragment that turns the bare "(N tests)" count into an actionable,
// language-aware run command — so the fix-test-iterate loop runs the TARGETED
// covering tests, not the whole suite or a guessed test name.
//
// Pure + side-effect-free so it's unit-testable by require() — unlike the hook
// itself, which top-level-exits on require (reads stdin / checks the index).

// Display cap: list at most this many "name (file)" entries before collapsing to a
// bare count. Above the cap a long per-name list is noise in the injected context.
const LIST_CAP = 6;

// Detect the project's test runner from the edited file's extension. v1 emits a
// REAL targeted command only for Rust (`cargo test` accepts bare substring filters,
// so the test fn names work directly). Other languages degrade to listing the
// covering tests with no command — a wrong/guessed command is worse than none
// (jest vs vitest vs mocha / pytest node ids / `go test -run` all differ).
function detectRunner(filePath) {
  if (typeof filePath === 'string' && /\.rs$/.test(filePath)) return 'rust';
  return null;
}

function targetedCommand(runner, names) {
  if (runner === 'rust') return `cargo test ${names.join(' ')}`;
  return null;
}

function suiteCommand(runner) {
  if (runner === 'rust') return 'cargo test';
  return null;
}

/**
 * Build the covering-tests summary fragment.
 * @param {Array<{name:string,file:string}>} testCallers  from `impact --json` test_callers
 * @param {string} editedFile  the file being edited (drives runner detection)
 * @returns {string} a `\n`-terminated fragment, or '' when there's nothing to add
 */
function formatCoveringTests(testCallers, editedFile) {
  const tests = Array.isArray(testCallers) ? testCallers.filter((t) => t && t.name) : [];
  const n = tests.length;
  if (n === 0) return '';

  const runner = detectRunner(editedFile);

  if (n <= LIST_CAP) {
    const list = tests.map((t) => `${t.name} (${t.file})`).join(', ');
    let out = `  Covering tests (${n}): ${list}\n`;
    const cmd = targetedCommand(runner, tests.map((t) => t.name));
    if (cmd) out += `  → Run after editing: ${cmd}\n`;
    return out;
  }

  // High fan-out: editing a widely-tested symbol. A long targeted command is noise
  // — point at the suite instead (mirrors the blast-size scaling in
  // session-init.js formatRecentImpact).
  let out = `  Covering tests: ${n} — editing a widely-tested symbol\n`;
  const suite = suiteCommand(runner);
  if (suite) out += `  → Run the suite after editing: ${suite}\n`;
  return out;
}

module.exports = { formatCoveringTests, LIST_CAP };
