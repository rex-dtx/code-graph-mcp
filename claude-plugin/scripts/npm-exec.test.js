'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const path = require('path');

const { npmInvocation, NPM_NEEDS_SHELL } = require('./npm-exec');

// The Windows shape can't be exercised by running it on Linux, so the shape is
// asserted through a re-required module with process.platform faked. Same trick
// the rest of this suite uses for platform-specific paths.
function windowsNpmExec() {
  const real = Object.getOwnPropertyDescriptor(process, 'platform');
  Object.defineProperty(process, 'platform', { value: 'win32', configurable: true });
  delete require.cache[require.resolve('./npm-exec')];
  delete require.cache[require.resolve('./proc-opts')];
  const mod = require('./npm-exec');
  Object.defineProperty(process, 'platform', real);
  delete require.cache[require.resolve('./npm-exec')];
  return mod;
}

test('POSIX: npm runs directly, with windowsHide set and args untouched', () => {
  if (NPM_NEEDS_SHELL) return; // running the suite on Windows — see the win32 test
  const { file, args, opts } = npmInvocation(['install', '-g', '@sdsrs/code-graph@1.2.3'], { timeout: 5 });
  assert.equal(file, 'npm');
  assert.deepEqual(args, ['install', '-g', '@sdsrs/code-graph@1.2.3']);
  assert.equal(opts.shell, undefined, 'no shell on POSIX');
  assert.equal(opts.windowsHide, true);
  assert.equal(opts.timeout, 5, 'caller options survive');
});

test('win32: the command is pre-quoted into `file` and `args` stays EMPTY (DEP0190)', () => {
  // npm is npm.cmd on Windows and needs a shell, but passing `args` alongside
  // `shell: true` is DEP0190 — runtime-deprecated in Node 24, and unescaped
  // (Node space-joins the values straight into the command line). The field
  // report for issue #40 shows the deprecation warning firing on every npm
  // spawn. Everything must be in `file`, nothing in `args`.
  const { npmInvocation: win } = windowsNpmExec();
  const { file, args, opts } = win(['install', '-g', '@sdsrs/code-graph@1.2.3'], { timeout: 7 });
  assert.equal(file, 'npm install -g @sdsrs/code-graph@1.2.3');
  assert.deepEqual(args, [], 'args MUST be empty when shell is true');
  assert.equal(opts.shell, true);
  assert.equal(opts.windowsHide, true);
  assert.equal(opts.timeout, 7);
});

test('win32: arguments needing quoting are quoted, unquotable ones throw', () => {
  const { npmInvocation: win, quoteCmdArg: q } = windowsNpmExec();
  assert.equal(q('install'), 'install');
  assert.equal(q('-g'), '-g');
  assert.equal(q('@sdsrs/code-graph@1.2.3'), '@sdsrs/code-graph@1.2.3');
  assert.equal(q('C:\\Program Files\\x'), '"C:\\Program Files\\x"');
  assert.equal(win(['root', '-g']).file, 'npm root -g');
  // Shell metacharacters are neutralized by the quoting (cmd treats `&&`
  // inside double quotes as literal text), so npm receives one argument.
  assert.equal(q('a && calc.exe'), '"a && calc.exe"');
  // What quoting can NOT neutralize is refused outright. `version` reaches the
  // install specs from the GitHub API response, so this is the one input here
  // that is not a compile-time constant.
  assert.throws(() => q('%PATH%'), /cannot be safely quoted/);
  assert.throws(() => q('a\r\nnotepad'), /cannot be safely quoted/);
  assert.throws(() => win(['install', '-g', 'pkg@1.0"; calc.exe']), /cannot be safely quoted/);
});

test('no call site pairs a non-empty args array with shell: true', () => {
  // The DEP0190 combination, spelled out at a call site, is what this replaced.
  // Guard the whole plugin, not just the two sites that had it.
  const files = fs.readdirSync(__dirname).filter((f) => f.endsWith('.js') && !f.endsWith('.test.js'));
  const offenders = [];
  for (const f of files) {
    const src = fs.readFileSync(path.join(__dirname, f), 'utf8')
      .replace(/\/\*[\s\S]*?\*\//g, '')   // block comments
      .replace(/\/\/.*$/gm, '');           // line comments
    // `shell: true` is only legitimate inside npm-exec.js, where args is [].
    if (f !== 'npm-exec.js' && /shell:\s*true/.test(src)) offenders.push(f);
  }
  assert.deepEqual(offenders, [],
    'route npm through npmInvocation() instead of spelling out shell: true — see DEP0190');
});
