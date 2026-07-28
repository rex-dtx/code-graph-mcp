#!/usr/bin/env node

const { spawn } = require("child_process");
const path = require("path");

// Tell find-binary.js our package root so it can locate bundled binaries
// and detect dev mode from bin/ → repo root (one level up)
process.env._FIND_BINARY_ROOT = path.resolve(__dirname, "..");

// Intercept adopt / unadopt before forwarding — they're node-only concerns
// (write to ~/.claude/projects/<slug>/memory/) and have no Rust counterpart.
// Lets `code-graph-mcp adopt` / `unadopt` work uniformly across plugin / npm / npx.
const sub = process.argv[2];

// Reject unknown flags on the intercepted subcommands BEFORE doing their work.
// `--help` was already guarded, but every OTHER token was ignored, so
// `code-graph-mcp adopt --helpp` ran adopt and wrote the user's CLAUDE.md — a
// typo away from the very side effect the --help guard exists to prevent. This
// is the fourth entry point onto the same "ignore what you don't recognise"
// idiom (doctor.js, lifecycle.js doctor, src/main.rs were the first three); it
// is the npm/npx surface, so it is the one most users reach.
function rejectUnknownFlags(name, known) {
  const unknown = process.argv.slice(3).filter((a) => !known.has(a));
  if (unknown.length) {
    process.stderr.write(
      `code-graph-mcp ${name}: unknown argument(s): ${unknown.join(" ")}\n` +
      `Run \`code-graph-mcp ${name} --help\` for usage.\n`);
    process.exit(2);
  }
}
if (sub === "adopt" || sub === "unadopt") {
  // `--help`/`-h` must be side-effect-free: adopt() writes the memory file +
  // MEMORY.md sentinel, unadopt() removes them. The Rust binary guards this for
  // direct invocation, but npm/npx routes through this wrapper, which intercepts
  // adopt/unadopt *before* the binary — so the guard must be repeated here, or
  // `code-graph-mcp adopt --help` rewrites MEMORY.md (the common new-user path).
  if (process.argv.slice(3).some((a) => a === "--help" || a === "-h")) {
    process.stdout.write(sub === "adopt"
      // Kept in sync with src/main.rs's adopt/unadopt help. This text described
      // the pre-v0.74 scheme (a sentinel in the ~/.claude memory dir) for three
      // releases after the target moved to the project's own CLAUDE.md, so npm
      // users were told this command edits a file it has not touched since.
      ? "code-graph-mcp adopt — install the code-graph steering block into the project CLAUDE.md\n\n" +
        "USAGE:\n    code-graph-mcp adopt\n\n" +
        "Writes a sentinel-wrapped managed block into <cwd>/CLAUDE.md plus a\n" +
        "<cwd>/.claude/plugin_code_graph_mcp.md detail doc, so Claude Code loads the\n" +
        "decision table each session. Run `code-graph-mcp unadopt` to remove it.\n"
      : "code-graph-mcp unadopt — remove the code-graph steering block\n\n" +
        "USAGE:\n    code-graph-mcp unadopt\n\n" +
        "Reverses `code-graph-mcp adopt`: strips the managed block from\n" +
        "<cwd>/CLAUDE.md and deletes <cwd>/.claude/plugin_code_graph_mcp.md.\n" +
        "User content outside the sentinel is kept.\n");
    process.exit(0);
  }
  rejectUnknownFlags(sub, new Set(["--help", "-h"]));
  const { adopt, unadopt, formatResult } = require("../claude-plugin/scripts/adopt");
  const result = sub === "unadopt" ? unadopt() : adopt();
  process.stdout.write(formatResult(sub, result) + "\n");
  process.exit(result.ok === false ? 1 : 0);
}

// Intercept `uninstall` — full local teardown (restore prior statusline, strip
// code-graph hooks from settings.json, delete ~/.cache/code-graph, and unadopt the
// CURRENT project). Node-only; no Rust counterpart. CC's `/plugin uninstall` fires
// no uninstall hook, so this is the user's one-shot CLI teardown. Guard `--help`
// before the destructive work (same discipline as adopt/unadopt above).
if (sub === "uninstall") {
  if (process.argv.slice(3).some((a) => a === "--help" || a === "-h")) {
    process.stdout.write(
      "code-graph-mcp uninstall — remove code-graph config + cache from this machine\n\n" +
      "USAGE:\n    code-graph-mcp uninstall [--unadopt-all] [--purge-global]\n\n" +
      "Restores your prior statusline, strips code-graph hooks from settings.json,\n" +
      "deletes ~/.cache/code-graph, and removes this project's CLAUDE.md adoption\n" +
      "block. --unadopt-all also removes the managed block + detail file from every\n" +
      "registered adopted project; --purge-global removes the globally-installed\n" +
      "@sdsrs npm packages even without the plugin-install marker. Also run\n" +
      "`/plugin uninstall code-graph-mcp` in Claude Code to sync its UI.\n");
    process.exit(0);
  }
  rejectUnknownFlags("uninstall", new Set(["--help", "-h", "--unadopt-all", "--purge-global"]));
  const lifecycle = require("../claude-plugin/scripts/lifecycle");
  const { unadopt } = require("../claude-plugin/scripts/adopt");
  const r = lifecycle.uninstall({
    purgeGlobal: process.argv.slice(3).includes("--purge-global"),
    unadoptAll: process.argv.slice(3).includes("--unadopt-all"),
  });
  let ua = { ok: false };
  try { ua = unadopt(); } catch { /* best-effort — settings/cache already cleaned */ }
  const projectUnadopted = !!(ua && (ua.blockPruned || ua.fileRemoved || ua.claudeMdRemoved));
  let out =
    `Uninstalled code-graph-mcp | settings cleaned=${r.settingsChanged}` +
    ` | this project unadopted=${projectUnadopted}\n`;
  if (r.globalPkgsRemoved.length) {
    out += `  Removed global npm package(s): ${r.globalPkgsRemoved.join(", ")}\n`;
  }
  if (r.globalPkgsRemaining.length) {
    out += `  Global npm package(s) still installed: ${r.globalPkgsRemaining.join(", ")}\n` +
      `    Remove with: npm uninstall -g ${r.globalPkgsRemaining.join(" ")}` +
      (r.pluginInstalledGlobals ? "\n" : "   (or re-run with --purge-global)\n");
  }
  if (r.unadopted.length) {
    const cleaned = r.unadopted.filter((u) => u.cleaned).length;
    out += `  Unadopted ${cleaned}/${r.unadopted.length} registered project(s) (--unadopt-all).\n`;
  }
  const otherAdopted = r.adoptedProjects.filter((p) => p !== process.cwd());
  if (otherAdopted.length) {
    out += "  Other adopted project(s) — re-run with --unadopt-all, or in each:" +
      " `code-graph-mcp unadopt` + `rm -rf .code-graph`\n" +
      otherAdopted.map((p) => `    ${p}\n`).join("");
  }
  out += "  Also run `/plugin uninstall code-graph-mcp` in Claude Code to sync its UI state.\n";
  process.stdout.write(out);
  process.exit(0);
}

const { findBinary, unsupportedPlatformHint } = require("../claude-plugin/scripts/find-binary");

const binary = findBinary();

if (!binary) {
  const hint = unsupportedPlatformHint();
  console.error(
    "Error: code-graph-mcp binary not found.\n\n" +
    (hint ? hint + "\n\n" : "") +
    "To install:\n" +
    "  npm install -g @sdsrs/code-graph\n\n" +
    "To build from source:\n" +
    "  cargo install code-graph-mcp --features embed-model\n"
  );
  process.exit(1);
}

// Spawn the binary, forwarding stdio for MCP JSON-RPC communication
const child = spawn(binary, process.argv.slice(2), {
  stdio: "inherit",
  env: process.env,
});

child.on("error", (err) => {
  console.error(`Failed to start code-graph-mcp: ${err.message}`);
  // A glibc binary installed on musl (older npm ignores the `libc` field) is present
  // but fails to exec — surface the actionable platform hint instead of a bare error.
  const hint = unsupportedPlatformHint();
  if (hint) console.error("\n" + hint);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code ?? 1);
  }
});
