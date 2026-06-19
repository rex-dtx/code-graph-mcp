# code-graph-mcp

## Project Overview

Rust MCP server that indexes codebases into an AST knowledge graph with semantic search. Communicates via JSON-RPC 2.0 over stdio.

## Tech Stack

- **Language**: Rust 2021 edition
- **Parser**: Tree-sitter — extraction depth varies by language:
  - **Full** (symbols + calls + imports + inheritance + routes + test markers): TS/TSX, JS, Go, Python, Rust, Java
  - **Smoke-tested** (symbols + calls + imports + inheritance): C#, Kotlin, Ruby, PHP, Swift, Dart
  - **Limited** (symbols + calls + `#include` imports + gtest test markers; no `Class::method` scope): C, C++
  - **Scripting**: Bash (functions + commands + `source`/`.` imports), Markdown (headings)
  - **File-FTS only** (no AST symbols extracted): HTML, CSS, JSON
- **Storage**: SQLite (rusqlite with bundled-full) + FTS5 + sqlite-vec (bundled C extension via build.rs)
- **Embedding**: Candle (optional, feature-gated `embed-model`)
- **File watching**: notify crate
- **Hashing**: blake3 for Merkle tree change detection

## Module Layout

```
src/
├── domain.rs     # Shared constants (relation types, limits, dimensions) — canonical source
├── mcp/          # JSON-RPC protocol, tool registry, server (stdio entry point)
├── parser/       # Tree-sitter AST parsing, relation extraction, language dispatch
├── indexer/      # 3-phase pipeline (parse → extract → embed), Merkle tree, file watcher
├── storage/      # SQLite schema init, CRUD operations, parameterized queries
├── graph/        # Recursive CTE call graph queries (callers/callees)
├── search/       # RRF fusion (BM25 + vector similarity)
├── embedding/    # EmbeddingModel struct, context builder
├── sandbox/      # Context compressor with token estimation
└── utils/        # Language detection from file extension, config
```

## Key Commands

```bash
cargo check                        # Type check
cargo build --release              # Full build with embedding
cargo build --no-default-features  # Build without embedding model
cargo test                         # Run all tests
cargo test --no-default-features   # Tests without embedding
```

## Important Patterns

- **Feature gating**: `embed-model` feature controls Candle dependencies; code using embeddings must be behind `#[cfg(feature = "embed-model")]`
- **Database**: SQLite with sqlite-vec compiled from `vendor/sqlite-vec/sqlite-vec.c` via `build.rs`
- **Relation constants**: Defined in `src/domain.rs` (re-exported from `storage/schema.rs`) — use constants (e.g., `REL_CALLS`) instead of hardcoded strings
- **Schema**: Defined in `src/storage/schema.rs` — parameterized queries in `src/storage/queries.rs`
- **MCP tools**: Registered in `src/mcp/tools.rs`, handled in `src/mcp/server/tools.rs`
- **Data directory**: `.code-graph/` under project root, auto-created and gitignored

## Conventions

- Commit format: `<type>(<scope>): <subject>` (e.g., `feat(parser): add relation extraction`)
- Error handling: `anyhow::Result` throughout, tracing for logging to stderr
- Tests: Unit tests in modules, integration tests in `tests/integration.rs`

## Code Graph Integration

Repo-wide AST + FTS + vector index. Prefer code-graph MCP over multi-round Grep/Read when intent matches one of these triggers (LSP only sees open files; code-graph sees the whole repo):

| Intent | Tool | Replaces |
|--------|------|----------|
| "Who calls X?" / "X 调了什么？" | `get_call_graph symbol_name=X` | N rounds of `grep "X("` |
| "改 X 影响什么？" / before editing a fn declaration | `get_ast_node symbol_name=X include_impact=true` | guess + read every caller |
| "Y 模块/目录长啥样？" / unfamiliar dir | `module_overview path=Y/` | Glob + Read×N |
| Concept search without exact symbol (e.g. "code that handles retries") | `semantic_code_search query="..."` | guesswork-driven Grep |
| HTTP route → handler chain | `get_call_graph route_path="GET /api/x"` | manual route table + Read |

Still use Grep for exact strings/regex (especially in non-code files: JSON, lockfiles, logs). Still use Read for files you're about to edit. CLI escape hatch: `code-graph-mcp <map|tour|overview|show|callgraph|impact|refs|dead-code|trace|grep|health-check>` — same data, Bash-friendly output. (`tour [PATH]` = dependency-ordered reading order: where to start reading a repo/subtree; CLI-only.) `code-graph-mcp grep` is a drop-in grep: `-F` literal / `-i` / `-w` / `-l` / `-A/-B/-C N` context / multi-path / `--max-count 0`, grep-compatible exit codes (0/1/2), git-grep-grade recall (finds tracked-but-gitignored files like this CLAUDE.md), each hit annotated with its containing fn/class. Full decision table: `MEMORY.md → plugin_code_graph_mcp.md`.

## Autonomy

`AUTONOMY_LEVEL: aggressive` — solo dev + bypassPermissions + fix-test-iterate workflow. Activates `~/.claude/CLAUDE.md` §5.1: cross-module refactor (≥3 Modules) → soft; internal-only Δ-contract → soft; dev-only deps → none; delete in safe-paths → no surface-required.

**Published-client boundary (HARD — keeps Δ-contract at hard AUTH)**:
- `src/mcp/tools.rs` tool schema — client is Claude Code (external) → published
- `claude-plugin/**` CLI flags and npm-facing surface → published
- Cargo `code-graph-mcp` CLI flags used by end users via npx/cargo install → published

**Internal (Δ-contract → soft)**:
- Rust module-to-module function signatures, struct fields, internal trait impls
- SQLite schema changes are **always hard** (migration rule in core §5 never downgrades)

**NEVER-downgrade** (from core §5.1): §8 SAFETY, Iron Law #2, Anti-hallucination, Destructive-smoke, Session-exit, User-global-state audit, `.env`/secrets, migration, `~/.claude/settings.json` / user-global hooks / MCP config, L3-enter.
