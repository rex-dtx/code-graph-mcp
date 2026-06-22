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
if (sub === "adopt" || sub === "unadopt") {
  // `--help`/`-h` must be side-effect-free: adopt() writes the memory file +
  // MEMORY.md sentinel, unadopt() removes them. The Rust binary guards this for
  // direct invocation, but npm/npx routes through this wrapper, which intercepts
  // adopt/unadopt *before* the binary — so the guard must be repeated here, or
  // `code-graph-mcp adopt --help` rewrites MEMORY.md (the common new-user path).
  if (process.argv.slice(3).some((a) => a === "--help" || a === "-h")) {
    process.stdout.write(sub === "adopt"
      ? "code-graph-mcp adopt — install the code-graph memory file + MEMORY.md sentinel\n\n" +
        "USAGE:\n    code-graph-mcp adopt\n\n" +
        "Writes plugin_code_graph_mcp.md and a sentinel block into this project's\n" +
        "~/.claude memory so Claude Code auto-loads the decision table. Run\n" +
        "`code-graph-mcp unadopt` to remove it.\n"
      : "code-graph-mcp unadopt — remove the code-graph memory file + sentinel\n\n" +
        "USAGE:\n    code-graph-mcp unadopt\n\n" +
        "Reverses `code-graph-mcp adopt`: deletes the memory file and the MEMORY.md\n" +
        "sentinel block. User content outside the sentinel is kept.\n");
    process.exit(0);
  }
  const { adopt, unadopt, formatResult } = require("../claude-plugin/scripts/adopt");
  const result = sub === "unadopt" ? unadopt() : adopt();
  process.stdout.write(formatResult(sub, result) + "\n");
  process.exit(result.ok === false ? 1 : 0);
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
