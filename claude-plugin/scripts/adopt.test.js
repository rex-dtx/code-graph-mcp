'use strict';
const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const path = require('path');
const os = require('os');
const {
  adopt, unadopt, memoryDir, stripSentinelBlock,
  isAdopted, isPluginModeInstall, maybeAutoAdopt, needsRefresh, isProjectRoot,
  detectProjectType, buildBlock, migrateLegacyMemoryDir,
  SENTINEL_BEGIN, SENTINEL_END, MANAGED_BY, TEMPLATE_PATH, TARGET_NAME,
  PROJECT_MARKERS,
} = require('./adopt');

// Legacy v1 sentinel (pre-v0.74, lived in the memory-dir MEMORY.md). Hard-coded
// here because the strip/migration path must keep removing it after the constant
// moved to v2.
const SENTINEL_BEGIN_V1 = '<!-- code-graph-mcp:begin v1 -->';

function makeSandbox() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-adopt-home-'));
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-adopt-cwd-'));
  // Mark the sandbox cwd as a real project — adopt() gates on a project marker.
  fs.mkdirSync(path.join(cwd, '.git'));
  return {
    home, cwd,
    claudeMd: path.join(cwd, 'CLAUDE.md'),
    detail: path.join(cwd, '.claude', TARGET_NAME),
    cleanup: () => {
      fs.rmSync(home, { recursive: true, force: true });
      fs.rmSync(cwd, { recursive: true, force: true });
    },
  };
}

// ── memoryDir (legacy slug — still used by migrateLegacyMemoryDir) ──────────

test('memoryDir slugifies cwd path', () => {
  assert.strictEqual(
    memoryDir('/home/alice/proj', '/home/alice'),
    '/home/alice/.claude/projects/-home-alice-proj/memory'
  );
});

test('memoryDir replaces underscores and dots (Claude Code slug convention)', () => {
  assert.strictEqual(
    memoryDir('/mnt/data_ssd/dev/projects/code-graph-mcp', '/home/u'),
    '/home/u/.claude/projects/-mnt-data-ssd-dev-projects-code-graph-mcp/memory'
  );
  assert.strictEqual(
    memoryDir('/home/sds/.claude/x', '/home/sds'),
    '/home/sds/.claude/projects/-home-sds--claude-x/memory'
  );
});

test('memoryDir honors CLAUDE_CONFIG_DIR override (multi-account isolation)', () => {
  const prev = process.env.CLAUDE_CONFIG_DIR;
  process.env.CLAUDE_CONFIG_DIR = '/home/alice/work-claude';
  try {
    assert.strictEqual(
      memoryDir('/home/alice/proj', '/home/alice'),
      '/home/alice/work-claude/projects/-home-alice-proj/memory'
    );
  } finally {
    if (prev === undefined) delete process.env.CLAUDE_CONFIG_DIR;
    else process.env.CLAUDE_CONFIG_DIR = prev;
  }
});

// ── buildBlock — the managed CLAUDE.md block ────────────────────────────────

test('buildBlock generic: v2 sentinel + 6 base rows + pointer', () => {
  const block = buildBlock('generic');
  assert.ok(block.startsWith(SENTINEL_BEGIN), 'opens with v2 BEGIN');
  assert.ok(block.endsWith(SENTINEL_END), 'closes with END');
  assert.ok(block.includes('| Who calls X / what X calls | `code-graph-mcp callgraph X` |'));
  assert.ok(block.includes('| Impact before editing a fn | `code-graph-mcp impact X` |'));
  assert.ok(block.includes('Full command + MCP-tool table: `.claude/plugin_code_graph_mcp.md`'));
  assert.ok(!block.includes('trace'), 'generic has no HTTP-trace row');
});

test('buildBlock web-rs inserts the HTTP-route → handler row', () => {
  const block = buildBlock('web-rs');
  assert.ok(block.includes('HTTP route → handler chain'), 'web project gets trace row');
  assert.ok(block.includes('`code-graph-mcp trace "GET /api/x"`'));
});

test('buildBlock frontend surfaces a find-references audit row', () => {
  const block = buildBlock('frontend');
  assert.ok(block.includes('Rename / refactor audit (refs)'));
  assert.ok(block.includes('`code-graph-mcp refs X`'));
});

test('buildBlock is deterministic (byte-identical across calls)', () => {
  assert.strictEqual(buildBlock('rust'), buildBlock('rust'));
  assert.strictEqual(buildBlock('generic'), buildBlock(undefined));
});

// ── adopt — installs CLAUDE.md block + .claude/ detail ──────────────────────

test('adopt creates CLAUDE.md with the block when none exists', () => {
  const sb = makeSandbox();
  try {
    const res = adopt({ cwd: sb.cwd });
    assert.strictEqual(res.ok, true);
    assert.strictEqual(res.created, true);
    assert.strictEqual(res.claudeMdWritten, true);
    assert.strictEqual(res.detailWritten, true);
    const cmd = fs.readFileSync(sb.claudeMd, 'utf8');
    assert.ok(cmd.includes(SENTINEL_BEGIN) && cmd.includes(SENTINEL_END));
    assert.ok(fs.existsSync(sb.detail), 'detail file written under .claude/');
    assert.ok(fs.readFileSync(sb.detail, 'utf8').startsWith(MANAGED_BY), 'detail has managed-by marker');
  } finally { sb.cleanup(); }
});

test('adopt injects the block into an existing CLAUDE.md, preserving user prose', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(sb.claudeMd, '# My Project\n\nUser instructions here.\n');
    const res = adopt({ cwd: sb.cwd });
    assert.strictEqual(res.created, false);
    assert.strictEqual(res.claudeMdWritten, true);
    const cmd = fs.readFileSync(sb.claudeMd, 'utf8');
    assert.ok(cmd.includes('User instructions here.'), 'preserves user prose');
    assert.ok(cmd.includes(SENTINEL_BEGIN), 'block appended');
  } finally { sb.cleanup(); }
});

test('adopt is idempotent — no duplicate block, no write on re-run', () => {
  const sb = makeSandbox();
  try {
    adopt({ cwd: sb.cwd });
    const res2 = adopt({ cwd: sb.cwd });
    assert.strictEqual(res2.claudeMdWritten, false, 'second run leaves CLAUDE.md alone');
    assert.strictEqual(res2.detailWritten, false, 'second run leaves detail alone');
    const cmd = fs.readFileSync(sb.claudeMd, 'utf8');
    const esc = SENTINEL_BEGIN.replace(/[\\/[\]^$.*+?()|{}]/g, '\\$&');
    assert.strictEqual((cmd.match(new RegExp(esc, 'g')) || []).length, 1, 'block appears exactly once');
  } finally { sb.cleanup(); }
});

test('adopt block reflects detected project type (web-rs → trace row in CLAUDE.md)', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'Cargo.toml'), '[dependencies]\naxum = "0.7"\n');
    adopt({ cwd: sb.cwd });
    assert.ok(fs.readFileSync(sb.claudeMd, 'utf8').includes('HTTP route → handler chain'));
  } finally { sb.cleanup(); }
});

test('adopt heals a malformed prior block (orphan BEGIN) and preserves neighbors', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(sb.claudeMd,
      `# Project\n\nKeep me.\n\n${SENTINEL_BEGIN}\n- stale partial block\n\nAlso keep me.\n`);
    const res = adopt({ cwd: sb.cwd });
    assert.strictEqual(res.healed, true);
    const cmd = fs.readFileSync(sb.claudeMd, 'utf8');
    const esc = SENTINEL_BEGIN.replace(/[\\/[\]^$.*+?()|{}]/g, '\\$&');
    assert.strictEqual((cmd.match(new RegExp(esc, 'g')) || []).length, 1, 'exactly one block');
    assert.ok(cmd.includes('Keep me.') && cmd.includes('Also keep me.'), 'neighbors preserved');
    assert.ok(!cmd.includes('stale partial block'), 'stale block purged');
  } finally { sb.cleanup(); }
});

test('adopt refuses a non-project cwd and writes nothing', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-adopt-home-'));
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-adopt-cwd-')); // no marker
  try {
    const res = adopt({ cwd });
    assert.strictEqual(res.ok, false);
    assert.strictEqual(res.reason, 'not-a-project');
    assert.ok(!fs.existsSync(path.join(cwd, 'CLAUDE.md')), 'no CLAUDE.md written');
    assert.ok(!fs.existsSync(path.join(cwd, '.claude')), 'no .claude dir created');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(cwd, { recursive: true, force: true });
  }
});

test('adopt writes atomically — no .tmp residue in cwd or .claude', () => {
  const sb = makeSandbox();
  try {
    adopt({ cwd: sb.cwd });
    const cwdResidue = fs.readdirSync(sb.cwd).filter((f) => f.includes('.tmp.'));
    const claudeResidue = fs.readdirSync(path.join(sb.cwd, '.claude')).filter((f) => f.includes('.tmp.'));
    assert.deepStrictEqual(cwdResidue, []);
    assert.deepStrictEqual(claudeResidue, []);
  } finally { sb.cleanup(); }
});

test('writeFileAtomic cleans its temp file when rename fails (no orphaned .tmp)', () => {
  const sb = makeSandbox();
  const realRename = fs.renameSync;
  try {
    fs.renameSync = () => { const e = new Error('EROFS: simulated read-only fs'); e.code = 'EROFS'; throw e; };
    try { adopt({ cwd: sb.cwd }); } catch { /* expected — rename failed */ }
    fs.renameSync = realRename;
    // .claude may or may not exist depending on which write failed first; tolerate both.
    const dirs = [sb.cwd, path.join(sb.cwd, '.claude')].filter((d) => fs.existsSync(d));
    for (const d of dirs) {
      assert.deepStrictEqual(fs.readdirSync(d).filter((f) => f.includes('.tmp.')), [],
        `failed rename must not orphan a temp in ${d}`);
    }
  } finally {
    fs.renameSync = realRename;
    sb.cleanup();
  }
});

// ── unadopt ─────────────────────────────────────────────────────────────────

test('unadopt removes the block + detail file, preserving user prose', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(sb.claudeMd, '# Project\n\nMy own notes.\n');
    adopt({ cwd: sb.cwd });
    const res = unadopt({ cwd: sb.cwd, home: sb.home });
    assert.strictEqual(res.fileRemoved, true);
    assert.strictEqual(res.blockPruned, true);
    assert.strictEqual(res.claudeMdRemoved, false, 'CLAUDE.md kept — has user prose');
    assert.ok(!fs.existsSync(sb.detail), 'detail file gone');
    const cmd = fs.readFileSync(sb.claudeMd, 'utf8');
    assert.ok(!cmd.includes(SENTINEL_BEGIN), 'block removed');
    assert.ok(cmd.includes('My own notes.'), 'user prose preserved');
  } finally { sb.cleanup(); }
});

test('unadopt deletes a CLAUDE.md that contained only our block', () => {
  const sb = makeSandbox();
  try {
    adopt({ cwd: sb.cwd }); // creates a block-only CLAUDE.md
    const res = unadopt({ cwd: sb.cwd, home: sb.home });
    assert.strictEqual(res.claudeMdRemoved, true);
    assert.ok(!fs.existsSync(sb.claudeMd), 'block-only CLAUDE.md removed');
  } finally { sb.cleanup(); }
});

test('unadopt will NOT delete a user file lacking our managed-by marker', () => {
  const sb = makeSandbox();
  try {
    fs.mkdirSync(path.join(sb.cwd, '.claude'));
    fs.writeFileSync(sb.detail, 'user-authored notes, not ours\n');
    const res = unadopt({ cwd: sb.cwd, home: sb.home });
    assert.strictEqual(res.fileRemoved, false, 'unmarked file is not deleted');
    assert.ok(fs.existsSync(sb.detail), 'user file survives');
  } finally { sb.cleanup(); }
});

test('unadopt is a no-op when never adopted', () => {
  const sb = makeSandbox();
  try {
    const res = unadopt({ cwd: sb.cwd, home: sb.home });
    assert.strictEqual(res.fileRemoved, false);
    assert.strictEqual(res.blockPruned, false);
  } finally { sb.cleanup(); }
});

// ── isAdopted ───────────────────────────────────────────────────────────────

test('isAdopted: false fresh, true after adopt, false after unadopt', () => {
  const sb = makeSandbox();
  try {
    assert.strictEqual(isAdopted({ cwd: sb.cwd }), false);
    adopt({ cwd: sb.cwd });
    assert.strictEqual(isAdopted({ cwd: sb.cwd }), true);
    unadopt({ cwd: sb.cwd, home: sb.home });
    assert.strictEqual(isAdopted({ cwd: sb.cwd }), false);
  } finally { sb.cleanup(); }
});

test('isAdopted: false when block present but detail file missing', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(sb.claudeMd, `${SENTINEL_BEGIN}\nx\n${SENTINEL_END}\n`);
    assert.strictEqual(isAdopted({ cwd: sb.cwd }), false, 'needs both block + detail');
  } finally { sb.cleanup(); }
});

// ── needsRefresh ────────────────────────────────────────────────────────────

test('needsRefresh: false right after adopt', () => {
  const sb = makeSandbox();
  try {
    adopt({ cwd: sb.cwd });
    assert.strictEqual(needsRefresh({ cwd: sb.cwd }), false);
  } finally { sb.cleanup(); }
});

test('needsRefresh: true when detail-doc body drifts from shipped template', () => {
  const sb = makeSandbox();
  try {
    adopt({ cwd: sb.cwd });
    fs.writeFileSync(sb.detail, `${MANAGED_BY}\n# stale content from an older plugin\n`);
    assert.strictEqual(needsRefresh({ cwd: sb.cwd }), true);
  } finally { sb.cleanup(); }
});

test('needsRefresh: true when the CLAUDE.md block drifts (project type change)', () => {
  const sb = makeSandbox();
  try {
    adopt({ cwd: sb.cwd }); // generic block
    // Now the project gains a web framework — block should switch to web-rs.
    fs.writeFileSync(path.join(sb.cwd, 'Cargo.toml'), '[dependencies]\naxum = "0.7"\n');
    assert.strictEqual(needsRefresh({ cwd: sb.cwd }), true);
  } finally { sb.cleanup(); }
});

test('needsRefresh: false when not adopted (nothing to refresh)', () => {
  const sb = makeSandbox();
  try {
    assert.strictEqual(needsRefresh({ cwd: sb.cwd }), false);
  } finally { sb.cleanup(); }
});

// ── maybeAutoAdopt ──────────────────────────────────────────────────────────

const PLUGIN_SCRIPTS = '/home/u/.claude/plugins/cache/code-graph-mcp/scripts';

test('maybeAutoAdopt skips when CODE_GRAPH_NO_AUTO_ADOPT=1', () => {
  const sb = makeSandbox();
  try {
    const res = maybeAutoAdopt({ cwd: sb.cwd, home: sb.home, scriptPath: PLUGIN_SCRIPTS, env: { CODE_GRAPH_NO_AUTO_ADOPT: '1' } });
    assert.strictEqual(res.reason, 'opted-out');
    assert.deepStrictEqual(res.migrated, { memoryIndexPruned: false, legacyDetailRemoved: false }, 'consistent migrated shape on early return');
    assert.strictEqual(isAdopted({ cwd: sb.cwd }), false);
  } finally { sb.cleanup(); }
});

test('maybeAutoAdopt skips when not plugin-mode (npm install path)', () => {
  const sb = makeSandbox();
  try {
    const res = maybeAutoAdopt({ cwd: sb.cwd, home: sb.home, scriptPath: '/usr/local/lib/node_modules/@sdsrs/code-graph/claude-plugin/scripts', env: {} });
    assert.strictEqual(res.reason, 'not-plugin-mode');
    assert.deepStrictEqual(res.migrated, { memoryIndexPruned: false, legacyDetailRemoved: false }, 'consistent migrated shape on early return');
    assert.strictEqual(isAdopted({ cwd: sb.cwd }), false);
  } finally { sb.cleanup(); }
});

test('maybeAutoAdopt installs when plugin-mode + not-yet-adopted', () => {
  const sb = makeSandbox();
  try {
    const res = maybeAutoAdopt({ cwd: sb.cwd, home: sb.home, scriptPath: PLUGIN_SCRIPTS, env: {} });
    assert.strictEqual(res.attempted, true);
    assert.strictEqual(res.reason, 'adopted');
    assert.strictEqual(res.result.ok, true);
    assert.strictEqual(isAdopted({ cwd: sb.cwd }), true);
  } finally { sb.cleanup(); }
});

test('maybeAutoAdopt is already-adopted when in sync (no gratuitous write)', () => {
  const sb = makeSandbox();
  try {
    adopt({ cwd: sb.cwd });
    const mtime = fs.statSync(sb.claudeMd).mtimeMs;
    const res = maybeAutoAdopt({ cwd: sb.cwd, home: sb.home, scriptPath: PLUGIN_SCRIPTS, env: {} });
    assert.strictEqual(res.reason, 'already-adopted');
    assert.strictEqual(fs.statSync(sb.claudeMd).mtimeMs, mtime, 'CLAUDE.md not touched');
  } finally { sb.cleanup(); }
});

test('maybeAutoAdopt refreshes a drifted detail doc (reason=refreshed)', () => {
  const sb = makeSandbox();
  try {
    adopt({ cwd: sb.cwd });
    fs.writeFileSync(sb.detail, `${MANAGED_BY}\n# stale\n`);
    const res = maybeAutoAdopt({ cwd: sb.cwd, home: sb.home, scriptPath: PLUGIN_SCRIPTS, env: {} });
    assert.strictEqual(res.reason, 'refreshed');
    const shipped = fs.readFileSync(TEMPLATE_PATH);
    const cur = fs.readFileSync(sb.detail);
    const nl = cur.indexOf(0x0a);
    assert.ok(shipped.equals(cur.subarray(nl + 1)), 'detail re-synced to shipped template');
  } finally { sb.cleanup(); }
});

test('maybeAutoAdopt skips refresh when CODE_GRAPH_NO_TEMPLATE_REFRESH=1 (locks edits)', () => {
  const sb = makeSandbox();
  try {
    adopt({ cwd: sb.cwd });
    const userEdit = `${MANAGED_BY}\n# my hand-edited table\n`;
    fs.writeFileSync(sb.detail, userEdit);
    const res = maybeAutoAdopt({ cwd: sb.cwd, home: sb.home, scriptPath: PLUGIN_SCRIPTS, env: { CODE_GRAPH_NO_TEMPLATE_REFRESH: '1' } });
    assert.strictEqual(res.reason, 'already-adopted');
    assert.strictEqual(fs.readFileSync(sb.detail, 'utf8'), userEdit, 'user edit preserved');
  } finally { sb.cleanup(); }
});

test('maybeAutoAdopt surfaces not-a-project for a bare cwd', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-adopt-home-'));
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-adopt-cwd-'));
  try {
    const res = maybeAutoAdopt({ cwd, home, scriptPath: PLUGIN_SCRIPTS, env: {} });
    assert.strictEqual(res.result.ok, false);
    assert.strictEqual(res.result.reason, 'not-a-project');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(cwd, { recursive: true, force: true });
  }
});

// ── migrateLegacyMemoryDir — auto-upgrade cleanup of the pre-v0.74 scheme ────

function seedLegacy(sb) {
  const dir = memoryDir(sb.cwd, sb.home);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, TARGET_NAME), `<!-- adopted-by: ${sb.cwd} -->\nold detail table\n`);
  fs.writeFileSync(path.join(dir, 'MEMORY.md'),
    `# Memory Index\n\n- [user_note.md](user_note.md) — keep me\n\n${SENTINEL_BEGIN_V1}\n- old code-graph router line\n${SENTINEL_END}\n`);
  return { dir, memIndex: path.join(dir, 'MEMORY.md'), legacyDetail: path.join(dir, TARGET_NAME) };
}

test('migrate strips the legacy v1 MEMORY.md block + deletes the adopted-by detail file', () => {
  const sb = makeSandbox();
  try {
    const L = seedLegacy(sb);
    const res = migrateLegacyMemoryDir({ cwd: sb.cwd, home: sb.home });
    assert.strictEqual(res.memoryIndexPruned, true);
    assert.strictEqual(res.legacyDetailRemoved, true);
    assert.ok(!fs.existsSync(L.legacyDetail), 'legacy detail deleted');
    const mem = fs.readFileSync(L.memIndex, 'utf8');
    assert.ok(!mem.includes(SENTINEL_BEGIN_V1), 'v1 sentinel removed');
    assert.ok(mem.includes('keep me'), "user's other memory preserved");
  } finally { sb.cleanup(); }
});

test('migrate will NOT delete a legacy detail file lacking the adopted-by marker', () => {
  const sb = makeSandbox();
  try {
    const dir = memoryDir(sb.cwd, sb.home);
    fs.mkdirSync(dir, { recursive: true });
    const userFile = path.join(dir, TARGET_NAME);
    fs.writeFileSync(userFile, 'a user file that happens to share the name\n');
    const res = migrateLegacyMemoryDir({ cwd: sb.cwd, home: sb.home });
    assert.strictEqual(res.legacyDetailRemoved, false);
    assert.ok(fs.existsSync(userFile), 'unmarked file survives');
  } finally { sb.cleanup(); }
});

test('migrate is a no-op when there is nothing to clean', () => {
  const sb = makeSandbox();
  try {
    const res = migrateLegacyMemoryDir({ cwd: sb.cwd, home: sb.home });
    assert.deepStrictEqual(res, { memoryIndexPruned: false, legacyDetailRemoved: false });
  } finally { sb.cleanup(); }
});

test('maybeAutoAdopt runs the legacy migration then installs the new scheme', () => {
  const sb = makeSandbox();
  try {
    const L = seedLegacy(sb);
    const res = maybeAutoAdopt({ cwd: sb.cwd, home: sb.home, scriptPath: PLUGIN_SCRIPTS, env: {} });
    assert.ok(res.migrated.memoryIndexPruned && res.migrated.legacyDetailRemoved, 'legacy cleaned');
    assert.ok(!fs.existsSync(L.legacyDetail), 'legacy detail gone');
    assert.ok(!fs.readFileSync(L.memIndex, 'utf8').includes(SENTINEL_BEGIN_V1), 'v1 block gone');
    assert.strictEqual(isAdopted({ cwd: sb.cwd }), true, 'new CLAUDE.md scheme installed');
  } finally { sb.cleanup(); }
});

// ── stripSentinelBlock (matches v1 + v2) ────────────────────────────────────

test('stripSentinelBlock removes a well-formed v2 block, preserving neighbors', () => {
  const before = `# Index\nKeep.\n\n${SENTINEL_BEGIN}\nbody\n${SENTINEL_END}\n\n- [x.md](x.md)\n`;
  const after = stripSentinelBlock(before);
  assert.ok(!after.includes(SENTINEL_BEGIN) && !after.includes(SENTINEL_END));
  assert.ok(after.includes('Keep.') && after.includes('- [x.md](x.md)'));
});

test('stripSentinelBlock removes a legacy v1 block (version-agnostic match)', () => {
  const before = `# Index\n${SENTINEL_BEGIN_V1}\n- old line\n${SENTINEL_END}\n- [keep.md](keep.md)\n`;
  const after = stripSentinelBlock(before);
  assert.ok(!after.includes(SENTINEL_BEGIN_V1), 'v1 begin removed');
  assert.ok(after.includes('- [keep.md](keep.md)'), 'neighbor preserved');
});

test('stripSentinelBlock self-heals orphan BEGIN without END', () => {
  const before = `# Index\n- [a.md](a.md) — entry\n${SENTINEL_BEGIN}\nbody\n\n- [b.md](b.md) — survivor\n`;
  const after = stripSentinelBlock(before);
  assert.ok(!after.includes(SENTINEL_BEGIN), 'orphan BEGIN removed');
  assert.ok(after.includes('survivor') && after.includes('entry'));
});

test('stripSentinelBlock self-heals orphan END line', () => {
  const before = `# Index\n- [a.md](a.md)\n${SENTINEL_END}\n- [b.md](b.md)\n`;
  const after = stripSentinelBlock(before);
  assert.ok(!after.includes(SENTINEL_END));
  assert.ok(after.includes('- [a.md](a.md)') && after.includes('- [b.md](b.md)'));
});

// ── platform guard ──────────────────────────────────────────────────────────

test('Windows platform is rejected with clear reason', { skip: process.platform === 'win32' }, () => {
  const orig = process.platform;
  Object.defineProperty(process, 'platform', { value: 'win32', configurable: true });
  try {
    const sb = makeSandbox();
    try {
      assert.strictEqual(adopt({ cwd: sb.cwd }).reason, 'windows-not-supported');
      assert.strictEqual(unadopt({ cwd: sb.cwd, home: sb.home }).reason, 'windows-not-supported');
    } finally { sb.cleanup(); }
  } finally {
    Object.defineProperty(process, 'platform', { value: orig, configurable: true });
  }
});

// ── template integrity ──────────────────────────────────────────────────────

test('template file exists and contains the decision table', () => {
  assert.ok(fs.existsSync(TEMPLATE_PATH), `template at ${TEMPLATE_PATH}`);
  const content = fs.readFileSync(TEMPLATE_PATH, 'utf8');
  assert.ok(content.includes('get_call_graph'), 'mentions get_call_graph');
  assert.ok(content.includes('CODE_GRAPH_QUIET_HOOKS'), 'mentions env gate');
  assert.ok(content.includes('.claude/plugin_code_graph_mcp.md'), 'describes the new layout');
});

// ── isPluginModeInstall ─────────────────────────────────────────────────────

test('isPluginModeInstall recognizes ~/.claude/plugins/... paths', () => {
  assert.strictEqual(isPluginModeInstall('/home/user/.claude/plugins/cache/code-graph-mcp@0.9.0/scripts'), true);
});

test('isPluginModeInstall rejects npm global / dev / npx paths', () => {
  assert.strictEqual(isPluginModeInstall('/usr/local/lib/node_modules/@sdsrs/code-graph/claude-plugin/scripts'), false);
  assert.strictEqual(isPluginModeInstall('/mnt/data_ssd/dev/projects/code-graph-mcp/claude-plugin/scripts'), false);
  assert.strictEqual(isPluginModeInstall('/tmp/npx-abc123/node_modules/@sdsrs/code-graph/claude-plugin/scripts'), false);
});

test('isPluginModeInstall recognizes CLAUDE_CONFIG_DIR/plugins/... paths', () => {
  const prev = process.env.CLAUDE_CONFIG_DIR;
  process.env.CLAUDE_CONFIG_DIR = '/home/alice/work-claude';
  try {
    assert.strictEqual(isPluginModeInstall('/home/alice/work-claude/plugins/cache/code-graph-mcp@0.31.0/scripts'), true);
    assert.strictEqual(isPluginModeInstall('/home/user/.claude/plugins/cache/code-graph-mcp/scripts'), true);
    assert.strictEqual(isPluginModeInstall('/home/alice/work-claude/projects/foo/memory'), false);
  } finally {
    if (prev === undefined) delete process.env.CLAUDE_CONFIG_DIR;
    else process.env.CLAUDE_CONFIG_DIR = prev;
  }
});

// ── isProjectRoot markers ───────────────────────────────────────────────────

test('isProjectRoot detects each marker', () => {
  for (const marker of PROJECT_MARKERS) {
    const cwd = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-marker-'));
    try {
      assert.strictEqual(isProjectRoot(cwd), false, 'bare cwd should not be a project');
      const markerPath = path.join(cwd, marker);
      if (marker.startsWith('.')) fs.mkdirSync(markerPath);
      else fs.writeFileSync(markerPath, '');
      assert.strictEqual(isProjectRoot(cwd), true, `${marker} should make cwd a project`);
    } finally {
      fs.rmSync(cwd, { recursive: true, force: true });
    }
  }
});

// ── detectProjectType (unchanged logic; tailoring still feeds buildBlock) ────

test('detectProjectType returns generic for an empty cwd', () => {
  const sb = makeSandbox();
  try { assert.strictEqual(detectProjectType(sb.cwd), 'generic'); } finally { sb.cleanup(); }
});

test('detectProjectType returns rust for a Cargo.toml without web framework', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'Cargo.toml'), '[package]\nname="x"\n[dependencies]\nserde="1"\n');
    assert.strictEqual(detectProjectType(sb.cwd), 'rust');
  } finally { sb.cleanup(); }
});

test('detectProjectType returns web-rs when Cargo.toml has axum', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'Cargo.toml'), '[dependencies]\naxum = "0.7"\n');
    assert.strictEqual(detectProjectType(sb.cwd), 'web-rs');
  } finally { sb.cleanup(); }
});

test('detectProjectType returns frontend for React/Next deps', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'package.json'), '{"dependencies":{"next":"^14","react":"^18"}}');
    assert.strictEqual(detectProjectType(sb.cwd), 'frontend');
  } finally { sb.cleanup(); }
});

test('detectProjectType returns web-node for express', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'package.json'), '{"dependencies":{"express":"^4"}}');
    assert.strictEqual(detectProjectType(sb.cwd), 'web-node');
  } finally { sb.cleanup(); }
});

test('detectProjectType returns web-py for FastAPI in pyproject.toml', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'pyproject.toml'), '[tool.poetry.dependencies]\nfastapi = "^0.115"\n');
    assert.strictEqual(detectProjectType(sb.cwd), 'web-py');
  } finally { sb.cleanup(); }
});

test('detectProjectType ignores commented-out web-framework deps in Cargo.toml', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'Cargo.toml'),
      '[package]\nname="x"\n[dependencies]\n# axum = "0.7"  # disabled\nserde = "1"\n');
    assert.strictEqual(detectProjectType(sb.cwd), 'rust');
  } finally { sb.cleanup(); }
});

test('detectProjectType ignores axum in [dev-dependencies] only', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'Cargo.toml'),
      '[package]\nname="x"\n[dependencies]\nserde = "1"\n[dev-dependencies]\naxum = "0.7"\n');
    assert.strictEqual(detectProjectType(sb.cwd), 'rust');
  } finally { sb.cleanup(); }
});

test('detectProjectType ignores react in devDependencies', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'package.json'),
      JSON.stringify({ dependencies: { lodash: '^4' }, devDependencies: { react: '^18' } }));
    assert.strictEqual(detectProjectType(sb.cwd), 'node');
  } finally { sb.cleanup(); }
});

test('detectProjectType ignores // indirect deps in go.mod', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'go.mod'),
      'module example.com/x\n\nrequire (\n\tgithub.com/some/cli v1.0.0\n\tgithub.com/gin-gonic/gin v1.9.0 // indirect\n)\n');
    assert.strictEqual(detectProjectType(sb.cwd), 'go');
  } finally { sb.cleanup(); }
});

test('detectProjectType handles malformed package.json without throwing', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'package.json'), '{not valid json');
    assert.strictEqual(detectProjectType(sb.cwd), 'node');
  } finally { sb.cleanup(); }
});

test('detectProjectType detects PEP 621 [project] dependencies block', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'pyproject.toml'),
      '[project]\nname = "x"\ndependencies = ["fastapi>=0.115", "uvicorn"]\n');
    assert.strictEqual(detectProjectType(sb.cwd), 'web-py');
  } finally { sb.cleanup(); }
});

test('detectProjectType reads requirements.txt as fallback', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'requirements.txt'), '# web stack\nflask>=3.0\ngunicorn\n');
    assert.strictEqual(detectProjectType(sb.cwd), 'web-py');
  } finally { sb.cleanup(); }
});

test('CODE_GRAPH_PROJECT_TYPE env override beats file-based detection', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'Cargo.toml'), '[package]\nname="x"\n');
    assert.strictEqual(detectProjectType(sb.cwd, { CODE_GRAPH_PROJECT_TYPE: 'web-rs' }), 'web-rs');
  } finally { sb.cleanup(); }
});

test('CODE_GRAPH_PROJECT_TYPE env override falls through on invalid value', () => {
  const sb = makeSandbox();
  try {
    fs.writeFileSync(path.join(sb.cwd, 'Cargo.toml'), '[package]\nname="x"\n');
    assert.strictEqual(detectProjectType(sb.cwd, { CODE_GRAPH_PROJECT_TYPE: 'web-rust' }), 'rust');
  } finally { sb.cleanup(); }
});
