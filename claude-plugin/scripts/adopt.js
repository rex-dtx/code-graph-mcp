#!/usr/bin/env node
'use strict';
// adopt / unadopt — writes plugin_code_graph_mcp.md into this project's
// Claude Code auto-memory dir (~/.claude/projects/<slug>/memory/, also
// read/written by claude-mem-lite) and maintains a sentinel-bracketed index
// entry in MEMORY.md. Idempotent. Used by invited-memory pattern with
// CODE_GRAPH_QUIET_HOOKS=1.
const fs = require('fs');
const path = require('path');
const os = require('os');
const { PROJECT_MARKERS, isProjectRoot, isNonProjectCwd } = require('./project-detect');

const SENTINEL_BEGIN = '<!-- code-graph-mcp:begin v1 -->';
const SENTINEL_END = '<!-- code-graph-mcp:end -->';
// Collision-detection marker. Slug encoding `[^a-zA-Z0-9-]→'-'` is lossy,
// so two cwds (e.g. /foo/bar and /foo bar) can resolve to the same memory
// dir. Adopt records its absolute cwd as the file's first-line HTML comment;
// re-adopt from a different cwd surfaces a warning.
const ADOPTED_BY_RE = /^<!-- adopted-by: (.+?) -->\r?\n?/;
function readAdoptedBy(filePath) {
  try {
    const first = fs.readFileSync(filePath, 'utf8').split('\n', 1)[0];
    const m = first.match(/^<!-- adopted-by: (.+?) -->/);
    return m ? m[1] : null;
  } catch { return null; }
}
// Atomic write (tmp in same dir → rename) so a crash mid-write can't leave a
// half-written MEMORY.md / detail file — the dir is shared with claude-mem-lite,
// which reads MEMORY.md on every keyword match. Mirrors lifecycle.js
// writeJsonAtomic / auto-update.js binary promote; accepts a string or Buffer.
function writeFileAtomic(filePath, data) {
  const tmp = filePath + '.tmp.' + process.pid;
  fs.writeFileSync(tmp, data);
  try {
    fs.renameSync(tmp, filePath);
  } catch (e) {
    // rename can fail (ENOSPC / EACCES / EROFS on the dir). Don't orphan the
    // temp in the shared memory dir — mirror auto-update.js's binary promote,
    // which cleans its tmp on failure. Best-effort unlink, then rethrow original.
    try { fs.unlinkSync(tmp); } catch { /* already gone */ }
    throw e;
  }
}
// One-liner per MEMORY.md spec ("each entry should be one line"). All routing
// triggers from prior multi-line block preserved verbatim — collapsing to single
// line is a structural fix, not a signal change. Decision table lives in the
// linked plugin_code_graph_mcp.md; this line is the router. Tag syntax
// `[tag1, tag2]` per spec for explicit keyword matching.
//
// Generic default — used when no project-type markers detected (e.g. /tmp,
// scratch dirs, mixed repos). Per-type variants live in `buildIndexLine` and
// are computed per-cwd at adopt + needsRefresh time. Adopted-project receives
// the typed variant; everyone else falls back to this canonical line.
// Tags MUST be ≥4 chars and topic-specific (per claudemd §11-EXT Tag-specificity).
// Generic single-word English tags (impact / refs / overview / semantic / deps /
// trace / route / similar) substring-match release-notes / commit-message prose
// via the §11 read-the-file hook regex (word-boundary + 0–2 declension chars),
// producing false-positive denies. Each tag below aligns with its MCP tool name
// (impact_analysis / find_references / module_overview / …) so hyphenated literals
// never collide with natural prose.
const INDEX_LINE =
  '- [code-graph-mcp](plugin_code_graph_mcp.md) ' +
  '[impact-analysis, callgraph, find-references, module-overview, semantic-search, ast-search, dead-code, find-similar-code, dependency-graph, trace-http-chain] — ' +
  '改 X 影响面/谁调用 X/X 被谁用/看 X 源码/Y 模块长啥样/概念查询 优先于 Grep；字面匹配走 Grep。' +
  'Bash 直呼 CLI 最快（零加载）：`code-graph-mcp callgraph X / show X / overview <dir> / grep "pat" / impact X`；' +
  'MCP 核心 7（get_call_graph/module_overview/semantic_code_search/ast_search/find_references/get_ast_node/project_map），决策表见全文';

// memdir L1 升格 (per sdscc 重构方案 §5.0): the INDEX_LINE that lands in
// MEMORY.md is what Claude sees first on every keyword match. Tailoring it
// per project type primes the right tools and demotes the irrelevant ones —
// e.g. a Rust CLI never benefits from `trace_http_chain` priming, and a React
// frontend cares more about `find_references` for rename audits than `impact`.
//
// Detection is cheap substring-on-marker (no AST, no graph): the cost is one
// fs.readFileSync per cwd. Failure mode is silent fall-back to 'generic' —
// false-negatives are strictly safer than false-positives that promote the
// wrong tool.
function readFileQuiet(p) {
  try { return fs.readFileSync(p, 'utf8'); } catch { return ''; }
}

// Valid project type buckets — also serves as the allow-list for
// `CODE_GRAPH_PROJECT_TYPE` env override. Anything not in this set falls back
// to file-based detection (so a typo'd env var does not silently break).
const PROJECT_TYPES = new Set([
  'rust', 'web-rs', 'web-node', 'web-py', 'web-go',
  'frontend', 'python', 'go', 'node', 'generic',
]);

// Cargo.toml: strip `# ...` comment lines (line-leading or trailing). Then
// extract the contents of the [dependencies] section only — dev/build/target
// deps don't characterize the project's runtime web-vs-cli posture.
//
// Why a state machine and not a TOML parser: we have zero deps and don't
// want to add one for a coarse classification. This handles the >95% case
// (well-formed [dependencies] block); pathological TOML (e.g. inline-table
// dependencies) falls through to false-negative `rust` which is safer than
// false-positive `web-rs`.
function extractCargoRuntimeDeps(cargo) {
  const lines = cargo.split(/\r?\n/);
  const out = [];
  let inDeps = false;
  for (const raw of lines) {
    // Strip `# comment` (line-leading or trailing). Inside string literals
    // `#` is rare in Cargo dep specs; accept the false-strip risk.
    const line = raw.replace(/(^|\s)#.*$/, '$1').trim();
    if (line.startsWith('[')) {
      // New section heading — only `[dependencies]` (canonical, exact match)
      // gates web-framework detection. `[dev-dependencies]`,
      // `[build-dependencies]`, `[target.'...'.dependencies]` deliberately
      // skipped: a project that pulls in axum only for tests is not a web
      // project for routing purposes.
      inDeps = (line === '[dependencies]');
      continue;
    }
    if (inDeps && line) out.push(line);
  }
  return out.join('\n');
}

// pyproject.toml: same pattern as Cargo.toml — strip comments, scan only
// [tool.poetry.dependencies] (Poetry) or [project.dependencies] (PEP 621).
function extractPyRuntimeDeps(pyproj) {
  const lines = pyproj.split(/\r?\n/);
  const out = [];
  let inDeps = false;
  for (const raw of lines) {
    const line = raw.replace(/(^|\s)#.*$/, '$1').trim();
    if (line.startsWith('[')) {
      inDeps = (
        line === '[tool.poetry.dependencies]' ||
        line === '[project.dependencies]' ||
        line === '[project]'  // PEP 621 inline `dependencies = [...]` lives here
      );
      continue;
    }
    if (inDeps && line) out.push(line);
  }
  return out.join('\n');
}

// go.mod: skip `// indirect` lines (transitive deps) and `// comment` lines.
// Direct require blocks are what matter for "is this a web service?" — a
// project that transitively pulls gin via a CLI dep is still a CLI.
function extractGoDirectRequires(gomod) {
  const lines = gomod.split(/\r?\n/);
  const out = [];
  let inRequire = false;
  for (const raw of lines) {
    const trimmed = raw.trim();
    if (trimmed.startsWith('//')) continue;       // pure comment line
    if (/\/\/\s*indirect\b/.test(raw)) continue;  // indirect dep marker
    if (trimmed === 'require (') { inRequire = true; continue; }
    if (inRequire && trimmed === ')') { inRequire = false; continue; }
    if (trimmed.startsWith('require ')) out.push(trimmed.slice(8).trim());
    else if (inRequire && trimmed) out.push(trimmed);
  }
  return out.join('\n');
}

function detectProjectType(cwd = process.cwd(), env = process.env) {
  // 2D: env override beats file-based detection. Honors only valid bucket
  // names; invalid override silently falls through to detection (avoids a
  // typo'd env var silently classifying everything as 'generic'). Power
  // users / CI can pin without touching the heuristics.
  const override = env && env.CODE_GRAPH_PROJECT_TYPE;
  if (override && PROJECT_TYPES.has(override)) {
    return override;
  }

  const cargo = readFileQuiet(path.join(cwd, 'Cargo.toml'));
  if (cargo) {
    const deps = extractCargoRuntimeDeps(cargo);
    // Web-framework detection: match on the dep name token (start-of-line
    // or after whitespace/quote) to avoid hits inside path strings or
    // unrelated metadata. `hyper` deliberately omitted from web-rs — it is
    // also commonly used as a plain HTTP client in CLI tools (false-positive
    // risk too high). axum/actix-web/etc. are unambiguous server stacks.
    if (/^(actix-web|axum|rocket|warp|poem|tide|salvo)\s*=/m.test(deps)) {
      return 'web-rs';
    }
    return 'rust';
  }

  const pkgRaw = readFileQuiet(path.join(cwd, 'package.json'));
  if (pkgRaw) {
    let pkg = null;
    try { pkg = JSON.parse(pkgRaw); } catch { /* malformed → fall through */ }
    if (pkg && typeof pkg === 'object') {
      // Only `dependencies` matters — devDependencies are build/test only,
      // and a project with `react` only in devDependencies is likely a
      // component library, not a frontend app.
      const deps = pkg.dependencies && typeof pkg.dependencies === 'object'
        ? Object.keys(pkg.dependencies)
        : [];
      const has = (name) => deps.includes(name);
      if (has('next') || has('react') || has('vue') || has('svelte') ||
          has('@angular/core') || has('nuxt') || has('astro') ||
          has('remix') || has('solid-js')) {
        return 'frontend';
      }
      if (has('express') || has('fastify') || has('koa') || has('hono') ||
          has('@nestjs/core') || has('@hapi/hapi')) {
        return 'web-node';
      }
    }
    return 'node';
  }

  const pyproj = readFileQuiet(path.join(cwd, 'pyproject.toml'));
  if (pyproj) {
    const deps = extractPyRuntimeDeps(pyproj);
    if (/\b(django|flask|fastapi|starlette|sanic|tornado|quart)\b/i.test(deps)) {
      return 'web-py';
    }
    return 'python';
  }
  // requirements.txt fallback: line-format `pkg==ver` / `pkg>=ver`. Strip
  // `#` comment lines; scan remaining as a flat list (no section headers
  // in this format).
  const reqs = readFileQuiet(path.join(cwd, 'requirements.txt'));
  if (reqs) {
    const cleaned = reqs.split(/\r?\n/)
      .map(l => l.replace(/(^|\s)#.*$/, '$1').trim())
      .filter(Boolean)
      .join('\n');
    if (/^(django|flask|fastapi|starlette|sanic|tornado|quart)\b/im.test(cleaned)) {
      return 'web-py';
    }
    return 'python';
  }

  const gomod = readFileQuiet(path.join(cwd, 'go.mod'));
  if (gomod) {
    const direct = extractGoDirectRequires(gomod);
    if (/\b(gin-gonic|labstack\/echo|gofiber|go-chi|gorilla\/mux)\b/.test(direct)) {
      return 'web-go';
    }
    return 'go';
  }
  return 'generic';
}

// Build the MEMORY.md index line for a project type. The 'generic' bucket
// returns the canonical INDEX_LINE so untyped projects (and the existing
// adopt.test.js fixtures, which use empty tmp dirs) stay byte-identical.
//
// For typed projects, the difference from generic is the tag set + the lead
// sentence — body of plugin_code_graph_mcp.md is unchanged. Decision table
// stays one source of truth; the index line just primes which subset matters
// most for THIS project.
function buildIndexLine(projectType = 'generic') {
  const prefix = '- [code-graph-mcp](plugin_code_graph_mcp.md) ';
  // v0.49 — CLI form leads: in Claude Code the MCP tools are deferred (need a
  // ToolSearch load before first call) while Bash is always live; the only
  // conversions observed in real coding nights were CLI invocations.
  const coreSuffix =
    'Bash 直呼 CLI 最快（零加载）：`code-graph-mcp callgraph X / show X / overview <dir> / grep "pat" / impact X`；' +
    'MCP 核心 7（get_call_graph/module_overview/semantic_code_search/ast_search/find_references/get_ast_node/project_map），决策表见全文';
  switch (projectType) {
    case 'web-rs':
    case 'web-node':
    case 'web-py':
    case 'web-go':
      return prefix +
        '[trace-http-chain, http-route, callgraph, impact-analysis, find-references, module-overview, semantic-search, dependency-graph] — ' +
        'HTTP 路由→handler 链路用 trace_http_chain（或 get_call_graph route_path=）；改 handler 影响面用 impact；' +
        '其他结构化查询同上 优先于 Grep。' + coreSuffix;
    case 'frontend':
      return prefix +
        '[find-references, module-overview, semantic-search, callgraph, impact-analysis, ast-search] — ' +
        '组件重命名/重构用 find_references（含 imports/inherits）；模块层级用 module_overview；' +
        '改 props/接口前用 impact 看下游；HTTP route 通常不适用。' + coreSuffix;
    case 'rust':
    case 'go':
    case 'python':
    case 'node':
      return prefix +
        '[callgraph, impact-analysis, find-references, module-overview, semantic-search, ast-search, dead-code, dependency-graph] — ' +
        '改 X 影响面/谁调用 X/Y 模块 优先于 Grep；HTTP route 追踪通常不适用（无 web 框架）；' +
        '字面匹配走 Grep。' + coreSuffix;
    case 'generic':
    default:
      return INDEX_LINE;
  }
}
const TEMPLATE_PATH = path.resolve(__dirname, '..', 'templates', 'plugin_code_graph_mcp.md');
const TARGET_NAME = 'plugin_code_graph_mcp.md';

// Claude Code slug convention: every non-alphanumeric-non-hyphen char → `-`.
// `/mnt/data_ssd/dev/proj` → `-mnt-data-ssd-dev-proj`
// `/home/sds/.claude/x`   → `-home-sds--claude-x`  (double-dash from `/.`)
//
// `home` is the OS home dir (default `os.homedir()`). When `CLAUDE_CONFIG_DIR`
// is set it overrides `home/.claude`, so multi-account users (personal vs work)
// land in the directory Claude Code itself is using for `projects/`.
function memoryDir(cwd = process.cwd(), home = os.homedir()) {
  const slug = cwd.replace(/[^a-zA-Z0-9-]/g, '-');
  const claudeDir = process.env.CLAUDE_CONFIG_DIR || path.join(home, '.claude');
  return path.join(claudeDir, 'projects', slug, 'memory');
}

function escapeRegex(s) {
  return s.replace(/[\\/[\]^$.*+?()|{}]/g, '\\$&');
}

// Strip our sentinel block — well-formed first, then self-heal orphan begin/end.
// Shared by adopt (so re-adopt rewrites a stale/malformed block) and unadopt.
function stripSentinelBlock(text) {
  const wellFormed = new RegExp(
    `${escapeRegex(SENTINEL_BEGIN)}[\\s\\S]*?${escapeRegex(SENTINEL_END)}\\n?`, 'g'
  );
  let out = text.replace(wellFormed, '');
  // Orphan BEGIN with no matching END (truncation / partial edit).
  // Strip from BEGIN to the next blank line or EOF — the file is shared with
  // claude-mem-lite, so we must not eat past a blank-line boundary.
  if (out.includes(SENTINEL_BEGIN)) {
    out = out.replace(
      new RegExp(`${escapeRegex(SENTINEL_BEGIN)}[\\s\\S]*?(?=\\n\\n|$)`, 'g'),
      ''
    );
  }
  // Orphan END line by itself.
  if (out.includes(SENTINEL_END)) {
    out = out.split('\n').filter(l => l.trim() !== SENTINEL_END).join('\n');
  }
  // Collapse blank-line runs introduced by stripping mid-paragraph blocks.
  return out.replace(/\n{3,}/g, '\n\n');
}

function platformGuard() {
  if (process.platform === 'win32') {
    return { ok: false, reason: 'windows-not-supported' };
  }
  return null;
}

// Project-marker detection (PROJECT_MARKERS / isProjectRoot / isNonProjectCwd)
// now lives in project-detect.js — the single activation gate shared with
// mcp-launcher.js and session-init.js. Imported above and re-exported below.

function adopt({ cwd, home, templatePath } = {}) {
  const blocked = platformGuard();
  if (blocked) return blocked;

  const effectiveCwd = cwd || process.cwd();
  // Gate adoption on a real-project cwd BEFORE touching the filesystem. The
  // check must run even when the memory dir already exists: Claude Code
  // pre-creates ~/.claude/projects/<slug>/memory for every session (including
  // the ~2035 headless /tmp mem-lite calls), and the old guard — nested inside
  // `if (!fs.existsSync(dir))` — was bypassed in exactly that case, letting
  // /tmp get adopted (sentinel written into its MEMORY.md). See project-detect.js.
  if (isNonProjectCwd(effectiveCwd)) {
    return { ok: false, reason: 'not-a-project', dir: memoryDir(cwd, home), cwd: effectiveCwd };
  }
  const dir = memoryDir(cwd, home);
  if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
  const target = path.join(dir, TARGET_NAME);
  const tpl = templatePath || TEMPLATE_PATH;
  if (!fs.existsSync(tpl)) {
    return { ok: false, reason: 'no-template', template: tpl };
  }
  // Slug-collision detection: read prior adopted-by marker before overwrite.
  let collisionWith = null;
  if (fs.existsSync(target)) {
    const prevCwd = readAdoptedBy(target);
    if (prevCwd && prevCwd !== effectiveCwd) collisionWith = prevCwd;
  }
  // Write marker + template. Marker is HTML comment → invisible in rendered
  // markdown but preserved by needsRefresh's bytewise compare (skipped via
  // ADOPTED_BY_RE strip below).
  const tplBody = fs.readFileSync(tpl);
  const marker = Buffer.from(`<!-- adopted-by: ${effectiveCwd} -->\n`);
  writeFileAtomic(target, Buffer.concat([marker, tplBody]));

  const indexPath = path.join(dir, 'MEMORY.md');
  const index = fs.existsSync(indexPath) ? fs.readFileSync(indexPath, 'utf8') : '# Memory Index\n';
  // Per-project index line: tagged tools + lead sentence tailored to the
  // detected project type. Falls back to the canonical INDEX_LINE for
  // generic / untyped cwds (preserves byte-identity with prior versions).
  const indexLine = buildIndexLine(detectProjectType(effectiveCwd));
  const desiredBlock = `${SENTINEL_BEGIN}\n${indexLine}\n${SENTINEL_END}`;

  // Already-adopted-and-well-formed: skip the write entirely.
  if (index.includes(desiredBlock)) {
    return { ok: true, target, indexPath, indexed: false, healed: false, collisionWith };
  }

  const cleaned = stripSentinelBlock(index);
  const healed = cleaned !== index;
  const base = cleaned.endsWith('\n') ? cleaned : cleaned + '\n';
  writeFileAtomic(indexPath, base + desiredBlock + '\n');
  return { ok: true, target, indexPath, indexed: true, healed, collisionWith };
}

// v0.9.0 — "已 adopt" 判定：template 文件在 + MEMORY.md 内有我们的 sentinel 块。
// 用在 maybeAutoAdopt 里做幂等门，也用在 session-init 里推导 quietHooks。
function isAdopted({ cwd, home } = {}) {
  const dir = memoryDir(cwd, home);
  const target = path.join(dir, TARGET_NAME);
  const indexPath = path.join(dir, 'MEMORY.md');
  if (!fs.existsSync(target) || !fs.existsSync(indexPath)) return false;
  const index = fs.readFileSync(indexPath, 'utf8');
  return index.includes(SENTINEL_BEGIN) && index.includes(SENTINEL_END);
}

// v0.11.0 — shipped template / INDEX_LINE 与已落地版本出现漂移时返回 true。
// 让已 adopt 的项目在下次 SessionStart 自动对齐到插件最新决策表，避免"老用户
// 永远停留在首次 adopt 时的 snapshot"。手动编辑会被覆盖——锁定方式：
// CODE_GRAPH_NO_TEMPLATE_REFRESH=1。
function needsRefresh({ cwd, home, templatePath } = {}) {
  const dir = memoryDir(cwd, home);
  const target = path.join(dir, TARGET_NAME);
  const indexPath = path.join(dir, 'MEMORY.md');
  const tpl = templatePath || TEMPLATE_PATH;
  if (!fs.existsSync(target) || !fs.existsSync(tpl) || !fs.existsSync(indexPath)) {
    return false;
  }
  const shipped = fs.readFileSync(tpl);
  const current = fs.readFileSync(target);
  // Strip the leading "<!-- adopted-by: ... -->\n" collision marker (D fix)
  // before bytewise comparing — its presence/path naturally diverges from
  // the shipped template.
  let body = current;
  const nl = current.indexOf(0x0a);
  if (nl > 0 && ADOPTED_BY_RE.test(current.subarray(0, nl + 1).toString())) {
    body = current.subarray(nl + 1);
  }
  if (!shipped.equals(body)) return true;
  const index = fs.readFileSync(indexPath, 'utf8');
  // Compare against the typed INDEX_LINE for this project. Detection is
  // deterministic (file-existence + substring scan) so adopt and needsRefresh
  // always agree on the variant. Drift triggers refresh — including when a
  // project gains a web framework dep and switches type bucket.
  const effectiveCwd = cwd || process.cwd();
  const indexLine = buildIndexLine(detectProjectType(effectiveCwd));
  const desiredBlock = `${SENTINEL_BEGIN}\n${indexLine}\n${SENTINEL_END}`;
  return !index.includes(desiredBlock);
}

// 检测脚本是否从 Claude Code 插件 cache 运行。
// 走 __dirname 而非 CLAUDE_PLUGIN_ROOT — 后者在多插件共存时会互相污染
// （见 feedback_plugin_env_isolation.md）。
// 默认匹配 `.claude/plugins/` 路径；CLAUDE_CONFIG_DIR 自定义目录时
// 走 startsWith(CLAUDE_CONFIG_DIR/plugins/) 兜底。
function isPluginModeInstall(scriptPath = __dirname) {
  const sep = path.sep;
  if (scriptPath.includes(`${sep}.claude${sep}plugins${sep}`)) return true;
  const envDir = process.env.CLAUDE_CONFIG_DIR;
  if (envDir) {
    const marker = path.join(envDir, 'plugins') + sep;
    if (scriptPath.startsWith(marker)) return true;
  }
  return false;
}

// C' 上下文感知默认（v0.9.0）：插件模式下首次 SessionStart 静默 adopt。
// /plugin install 本身已构成知情同意；npm / npx / 裸 checkout 保持 opt-in。
// 退出：CODE_GRAPH_NO_AUTO_ADOPT=1。
function maybeAutoAdopt({ cwd, home, env, scriptPath } = {}) {
  env = env || process.env;
  if (env.CODE_GRAPH_NO_AUTO_ADOPT === '1') {
    return { attempted: false, reason: 'opted-out' };
  }
  if (!isPluginModeInstall(scriptPath || __dirname)) {
    return { attempted: false, reason: 'not-plugin-mode' };
  }
  if (isAdopted({ cwd, home })) {
    // v0.11.0: shipped template / INDEX_LINE 漂移时重跑 adopt 对齐。
    // opt-out: CODE_GRAPH_NO_TEMPLATE_REFRESH=1（锁定手动编辑）。
    if (env.CODE_GRAPH_NO_TEMPLATE_REFRESH !== '1' && needsRefresh({ cwd, home })) {
      const result = adopt({ cwd, home });
      return { attempted: true, reason: 'refreshed', result };
    }
    return { attempted: false, reason: 'already-adopted' };
  }
  const result = adopt({ cwd, home });
  return { attempted: true, reason: 'adopted', result };
}

function unadopt({ cwd, home } = {}) {
  const blocked = platformGuard();
  if (blocked) return blocked;

  const dir = memoryDir(cwd, home);
  const target = path.join(dir, TARGET_NAME);
  const indexPath = path.join(dir, 'MEMORY.md');
  let fileRemoved = false;
  let indexPruned = false;

  if (fs.existsSync(target)) {
    fs.unlinkSync(target);
    fileRemoved = true;
  }
  if (fs.existsSync(indexPath)) {
    const before = fs.readFileSync(indexPath, 'utf8');
    const after = stripSentinelBlock(before);
    if (after !== before) {
      writeFileAtomic(indexPath, after);
      indexPruned = true;
    }
  }
  return { ok: true, fileRemoved, indexPruned, target, indexPath };
}

function formatResult(action, result) {
  if (!result.ok && result.reason === 'windows-not-supported') {
    return '[code-graph] adopt/unadopt are POSIX-only — claude-mem-lite slug ' +
           'convention on Windows is unverified. Edit MEMORY.md manually to opt in.';
  }
  if (action === 'adopt') {
    if (!result.ok) {
      if (result.reason === 'no-memory-dir') {
        return `[code-graph] Memory dir not found: ${result.dir}\n` +
               '  Run \`claude\` at least once in this project to create it.';
      }
      if (result.reason === 'not-a-project') {
        return `[code-graph] Not a project root: ${result.cwd}\n` +
               '  No project marker (.git, Cargo.toml, package.json, pyproject.toml, ...).\n' +
               '  cd into a real project before running adopt.';
      }
      if (result.reason === 'no-template') {
        return `[code-graph] Template missing: ${result.template}`;
      }
      return `[code-graph] adopt failed: ${result.reason || 'unknown'}`;
    }
    const lines = [`[code-graph] Adopted → ${result.target}`];
    if (result.collisionWith) {
      lines.push(`[code-graph] ⚠ slug collision: this dir was previously adopted by ${result.collisionWith}.`);
      lines.push('[code-graph]   Memory dir is shared — sentinels overwritten. ' +
                 'Investigate path encoding clash (Claude Code slug = path with non-[a-zA-Z0-9-] → "-").');
    }
    if (result.healed) lines.push(`[code-graph] Healed malformed sentinel block → ${result.indexPath}`);
    else if (result.indexed) lines.push(`[code-graph] Indexed → ${result.indexPath}`);
    else lines.push(`[code-graph] Index already up-to-date — no write`);
    // v0.17.0: SessionStart project_map injection is OFF by default (regardless
    // of adoption). Adoption now only governs MEMORY.md sentinel + decision-table
    // refresh; the noisy hook needs an explicit opt-in.
    lines.push('[code-graph] Active. SessionStart project_map injection: OFF (default).');
    lines.push('[code-graph] Opt in to map dump:  CODE_GRAPH_VERBOSE_HOOKS=1');
    lines.push('[code-graph] Legacy override:     CODE_GRAPH_QUIET_HOOKS=0 (force noisy) / =1 (force quiet)');
    return lines.join('\n');
  }
  if (action === 'unadopt') {
    const lines = [];
    if (result.fileRemoved) lines.push(`[code-graph] Removed → ${result.target}`);
    if (result.indexPruned) lines.push(`[code-graph] De-indexed → ${result.indexPath}`);
    if (!result.fileRemoved && !result.indexPruned) lines.push('[code-graph] Nothing to unadopt');
    return lines.join('\n');
  }
  return '';
}

if (require.main === module) {
  const action = process.argv[2] === 'unadopt' ? 'unadopt' : 'adopt';
  const result = action === 'unadopt' ? unadopt() : adopt();
  process.stdout.write(formatResult(action, result) + '\n');
  process.exit(result.ok === false ? 1 : 0);
}

module.exports = {
  adopt, unadopt, memoryDir, formatResult, stripSentinelBlock,
  isAdopted, isPluginModeInstall, maybeAutoAdopt, needsRefresh, isProjectRoot,
  detectProjectType, buildIndexLine,
  extractCargoRuntimeDeps, extractPyRuntimeDeps, extractGoDirectRequires,
  SENTINEL_BEGIN, SENTINEL_END, INDEX_LINE, TEMPLATE_PATH, TARGET_NAME,
  PROJECT_MARKERS, PROJECT_TYPES, isNonProjectCwd,
};
