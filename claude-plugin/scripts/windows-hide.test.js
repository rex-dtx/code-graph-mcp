'use strict';
// Drift guard for issue #40: on Windows every console-subsystem child spawned
// from a console-less parent (our MCP server, hooks and statusline are all
// launched hidden by Claude Code) gets a NEW visible console window unless the
// spawn passes `windowsHide: true` — which Node defaults to FALSE on every
// child_process API. The field report was 5–7 console windows flashing and
// stealing keyboard focus on every session start.
//
// One-off fixes rot: this file re-derives the call-site list from source on
// every run, so a NEW spawn added tomorrow fails here instead of shipping a
// flash. It is a static check by necessity — the behavior itself only
// reproduces on Windows, which CI does not run for the JS suite.
const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const path = require('path');

// Names that reach child_process. `spawnFn` is the injected-seam spelling used
// by launcher-install.js / session-init.js — its default IS spawn, so its
// options object needs the same treatment.
const CALL_NAMES = ['spawnSync', 'spawn', 'execFileSync', 'execFile', 'execSync', 'exec', 'spawnFn'];
const CALL_RE = new RegExp(`(?<![.\\w$])(${CALL_NAMES.join('|')})\\s*\\(`, 'g');

// Accepted spellings inside the argument list: the shared helper, the npm
// invocation builder (which folds windowsHide in), or an explicit literal.
const GUARDED_RE = /(?<![.\w$])(hidden|npmInvocation)\s*\(|windowsHide/;

/**
 * Replace the CONTENTS of strings, template literals, regex literals and
 * comments with spaces, preserving offsets and length. Lets the scanner treat
 * `'exec('` inside a string, or a call named in a comment, as what they are:
 * not call sites.
 *
 * Regex literals are NOT optional here. Skipping them looked harmless until
 * user-prompt-context.js's ``/`([a-zA-Z_]\w{2,})`/g`` — a regex containing
 * backticks — was read as the start of a template literal, which put every
 * quote after it out of phase and hid that file's one execFileSync from the
 * sweep. A guard that silently stops seeing part of the tree is worse than no
 * guard; `masker sees every file that spawns` below is the cross-check.
 * @param {string} src
 * @returns {string}
 */
function maskLiterals(src) {
  const out = src.split('');
  let i = 0;
  let prev = ''; // last significant (non-space) code char, for regex-vs-divide
  const blank = (from, to) => { for (let k = from; k < to && k < out.length; k++) if (out[k] !== '\n') out[k] = ' '; };
  while (i < src.length) {
    const c = src[i];
    const n = src[i + 1];
    if (c === '/' && n === '/') {
      const end = src.indexOf('\n', i);
      blank(i, end === -1 ? src.length : end);
      i = end === -1 ? src.length : end;
    } else if (c === '/' && n === '*') {
      const end = src.indexOf('*/', i + 2);
      blank(i, end === -1 ? src.length : end + 2);
      i = end === -1 ? src.length : end + 2;
    } else if (c === '/' && (prev === '' || '(,=:[!&|?{};+-*%~^<>'.includes(prev))) {
      // Regex literal (the only `/` that can legally follow those); anything
      // else — an identifier, `)`, `]`, a digit — is division.
      let j = i + 1;
      let inClass = false;
      while (j < src.length && src[j] !== '\n') {
        if (src[j] === '\\') { j += 2; continue; }
        if (src[j] === '[') inClass = true;
        else if (src[j] === ']') inClass = false;
        else if (src[j] === '/' && !inClass) break;
        j++;
      }
      blank(i + 1, j);
      prev = '/';
      i = j + 1;
    } else if (c === '"' || c === "'" || c === '`') {
      let j = i + 1;
      while (j < src.length && src[j] !== c) {
        if (src[j] === '\\') j++;
        j++;
      }
      blank(i + 1, j);
      prev = c;
      i = j + 1;
    } else {
      if (!/\s/.test(c)) prev = c;
      i++;
    }
  }
  return out.join('');
}

/**
 * Text of the argument list starting at the `(` index, without the outer parens.
 * @param {string} masked
 * @param {number} openIdx
 * @returns {string|null} null when unbalanced (should not happen in valid JS)
 */
function argText(masked, openIdx) {
  let depth = 0;
  for (let i = openIdx; i < masked.length; i++) {
    const c = masked[i];
    if (c === '(' || c === '[' || c === '{') depth++;
    else if (c === ')' || c === ']' || c === '}') {
      depth--;
      if (depth === 0) return masked.slice(openIdx + 1, i);
    }
  }
  return null;
}

/**
 * Unguarded child_process call sites in one source text.
 * @param {string} src
 * @returns {Array<{name: string, line: number}>}
 */
function findUnguarded(src) {
  const masked = maskLiterals(src);
  const bad = [];
  let m;
  CALL_RE.lastIndex = 0;
  while ((m = CALL_RE.exec(masked)) !== null) {
    const open = masked.indexOf('(', m.index + m[1].length);
    const args = argText(masked, open);
    if (args === null) continue;
    let ok = GUARDED_RE.test(args);
    if (!ok) {
      // Options passed as a variable (`gitOpts`, `npm.opts`): resolve the
      // binding in the same file and test its initializer instead.
      const last = args.split(',').pop().trim().replace(/\.opts$/, '');
      if (/^[A-Za-z_$][\w$]*$/.test(last)) {
        const decl = new RegExp(`(?:const|let|var)\\s+${last}\\s*=([^;]*)`).exec(masked);
        ok = !!decl && GUARDED_RE.test(decl[1]);
      }
    }
    if (!ok) bad.push({ name: m[1], line: src.slice(0, m.index).split('\n').length });
  }
  return bad;
}

// ── Negative control: the checker must actually reject an unguarded call ──
// Without this, a scanner that silently matched nothing (bad regex, masking
// bug) would report a clean sweep over the whole plugin.
test('the guard rejects an unguarded spawn (negative control)', () => {
  assert.deepEqual(
    findUnguarded("execFileSync('curl', ['-sL', url], { timeout: 1000 });").map((b) => b.name),
    ['execFileSync']);
  assert.deepEqual(findUnguarded("const o = { timeout: 1 };\nspawnSync('npm', a, o);").map((b) => b.name),
    ['spawnSync']);
  // ...and must ACCEPT each guarded spelling.
  assert.deepEqual(findUnguarded("execSync(cmd, hidden({ cwd }));"), []);
  assert.deepEqual(findUnguarded("spawn(f, a, { windowsHide: true, cwd });"), []);
  assert.deepEqual(findUnguarded("const npm = npmInvocation(['root','-g']);\nspawn(npm.file, npm.args, npm.opts);"), []);
  assert.deepEqual(findUnguarded("const gitOpts = hidden({ cwd });\nexecSync('git log', gitOpts);"), []);
  // Not call sites: a regex .exec, a call named inside a string or comment.
  assert.deepEqual(findUnguarded("const m = RE.exec(raw);\n// spawn('x', a, {})\nconst s = \"execSync('y', {})\";"), []);
  // A regex literal containing a quote/backtick must not throw the masker out
  // of phase and blind it to everything after — this exact shape (a backtick
  // inside a regex in user-prompt-context.js) hid a real call site.
  assert.deepEqual(
    findUnguarded("const s = m.match(/`(\\w+)`/g);\nexecFileSync(bin, a, { timeout: 1 });").map((b) => b.name),
    ['execFileSync']);
  // ...and division must still be division, not an unterminated regex.
  assert.deepEqual(findUnguarded("const half = total / 2;\nspawn(f, a, hidden({}));"), []);
});

test('the masker sees every file that spawns (guard-of-the-guard)', () => {
  // Cross-check with a dumb detector: any file that routes options through
  // hidden()/npmInvocation() must yield at least one call site to the scanner.
  // Zero would mean the masking silently swallowed that file's code — the
  // failure mode that shipped in the first version of this test.
  const files = fs.readdirSync(__dirname).filter((f) => f.endsWith('.js') && !f.endsWith('.test.js'));
  const blind = [];
  for (const f of files) {
    if (f === 'proc-opts.js' || f === 'npm-exec.js') continue; // define the helpers, don't spawn
    const src = fs.readFileSync(path.join(__dirname, f), 'utf8');
    if (!/(?<![.\w$])(hidden|npmInvocation)\s*\(/.test(src)) continue;
    CALL_RE.lastIndex = 0;
    if (!CALL_RE.test(maskLiterals(src))) blind.push(f);
  }
  assert.deepEqual(blind, [], 'the scanner found no call sites in files that demonstrably spawn');
});

test('every child_process call site in claude-plugin/scripts sets windowsHide', () => {
  const files = fs.readdirSync(__dirname)
    .filter((f) => f.endsWith('.js') && !f.endsWith('.test.js'))
    .sort();
  const failures = [];
  let scanned = 0;
  for (const f of files) {
    const src = fs.readFileSync(path.join(__dirname, f), 'utf8');
    const masked = maskLiterals(src);
    CALL_RE.lastIndex = 0;
    scanned += (masked.match(CALL_RE) || []).length;
    for (const bad of findUnguarded(src)) failures.push(`${f}:${bad.line} — ${bad.name}(...)`);
  }
  // Positive control on the scan itself: the sweep covered ~30 call sites when
  // written. A rewrite that stops finding them must fail here, not pass empty.
  assert.ok(scanned >= 25, `expected the scanner to still find the call sites, found ${scanned}`);
  assert.deepEqual(failures, [],
    'these child_process calls will flash a console window on Windows — wrap the options in hidden({...}) from ./proc-opts');
});

test('proc-opts.hidden defaults windowsHide on and preserves caller options', () => {
  const { hidden } = require('./proc-opts');
  assert.equal(hidden().windowsHide, true);
  assert.deepEqual(hidden({ cwd: '/x', timeout: 5 }), { windowsHide: true, cwd: '/x', timeout: 5 });
  // An explicit caller value wins — nothing in-tree needs it, but silently
  // overriding a deliberate `false` would be a surprise.
  assert.equal(hidden({ windowsHide: false }).windowsHide, false);
});
