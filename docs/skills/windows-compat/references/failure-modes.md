# Windows failure modes in this repo

Every entry is a defect that actually shipped, with the commit-visible fix. The
invariant they all violate is stated in SKILL.md; this file is the catalogue and
the lookup table.

- [Who emits which spelling](#who-emits-which-spelling)
- [D1 — separator mismatch in a set/map comparison](#d1--separator-mismatch-in-a-setmap-comparison)
- [D2 — the extended prefix leaking](#d2--the-extended-prefix-leaking)
- [D3 — argv byte limit](#d3--argv-byte-limit)
- [D4 — path heuristics that assume one ecosystem's layout](#d4--path-heuristics-that-assume-one-ecosystems-layout)
- [D5 — separator rewrite that corrupts Unix filenames](#d5--separator-rewrite-that-corrupts-unix-filenames)
- [D6 — user-supplied path becoming an index key verbatim](#d6--user-supplied-path-becoming-an-index-key-verbatim)
- [D7 — executable name matched without `.exe`](#d7--executable-name-matched-without-exe)
- [The good pattern already in the repo](#the-good-pattern-already-in-the-repo)
- [Platform facts worth not re-deriving](#platform-facts-worth-not-re-deriving)

## Who emits which spelling

| Producer | Windows output | Unix output |
|---|---|---|
| `rg` / `rg --files` | native `src\foo.rs` | `src/foo.rs` |
| `git ls-files` | **always** `src/foo.rs` | `src/foo.rs` |
| `Path::canonicalize()` | `\\?\D:\repo\src\foo.rs` | `/repo/src/foo.rs` |
| `PathBuf::join` + `to_string_lossy` | mixed: `D:\repo\src/foo.rs` | `/repo/src/foo.rs` |
| git's `gitdir:` pointer file | `/`, even on Windows | `/` |

Mixed spellings are the norm, not an edge case: `project_root.join(rel)` where
`rel` came from git produces `D:\repo\` + `src/foo.rs` in one string.

## D1 — separator mismatch in a set/map comparison

**Issue #34, the root cause behind three reported symptoms.**

`cli::tracked_files_missed_by_walk` computed `tracked ∖ walked`, where `tracked`
came from `git ls-files` (`/`) and `walked` from `rg --files` (`\`). On Windows
`walked.contains(t)` matched **nothing**, so "files rg missed" became *every
tracked file* — 3,284 of them.

That one mismatch produced all three reported defects:

1. 3,284 absolute paths appended to one argv → `os error 206` (see D3)
2. every file scanned twice (walk + supplement) → duplicated matches, printed in
   two spellings because they never compared equal for dedup
3. AST annotation silently absent, because the lookup key never matched the
   indexed path — which is why "no containing function/class" reproduced on
   Windows only

**Tell:** a `HashSet`/`HashMap` of paths whose members come from two different
producers in the table above.

**Fix:** normalize both sides through one function before comparing —
`cli::normalize_path_display`.

## D2 — the extended prefix leaking

`canonicalize()` returns `\\?\D:\…` (or `\\?\UNC\server\share\…`). Correct as a
path, wrong as output, and it never string-compares equal to the non-canonical
root.

**Fix:** strip `\\?\` / `\\?\UNC\` before printing or comparing. `dunce` does
this if a dependency is acceptable; this repo hand-rolls it in
`normalize_path_display_on` because the hand-rolled version takes the platform
as a parameter, which `dunce`'s internal `cfg(windows)` does not allow.

## D3 — argv byte limit

Windows caps an **entire command line at 32,767 characters**; POSIX `ARG_MAX` is
~2 MB. `grep` capped the *supplement file count* at 500, which bounds neither the
byte length (500 × ~110-char absolute paths ≈ 55 KB → `os error 206`) nor
correctness (silently searching 500 of 3,284 files reports "no matches" when
matches exist).

**Fix pattern** (`cli::cmd_grep`): build the flags into a `Vec<OsString>` once,
pass path operands as **relative** paths with `current_dir` set (dropping the
repeated root prefix is the single biggest byte win), then split into
byte-budgeted batches and merge. Merged exit code is worst-first: 2 (error) >
0 (matched) > 1 (no match).

A count cap is never a length bound. Budget bytes.

## D4 — path heuristics that assume one ecosystem's layout

**Issue #36.** `domain::is_test_path` recognized only JS/Rust/Go conventions, so
xUnit's `src/Tests/<Project>/<Name>Tests.cs` and Maven's `src/test/java/…`
matched nothing and `affected` reported "0 test file(s) to re-run" — a silent
false negative in the one output a CI integration acts on.

Two rules came out of the fix:

- **Anchored prefixes are a smell.** `starts_with("tests/")` only sees a
  repo-root layout. Use segment containment (`/tests/`), case-insensitively —
  .NET capitalizes `Tests`.
- **A predicate with mirrors must be enumerated before it is edited.**
  `is_test_path` is reimplemented across several sites; `domain.rs` carries a
  "Five sites must agree" note. The legs that must agree now build from
  `PASCAL_TEST_EXTS` / `PASCAL_TEST_STEMS` / `INFIX_TEST_EXTS`, with
  `test_is_test_node_sql_matches_rust` running the emitted SQL against in-memory
  SQLite to prove agreement. Two sites (`PROD_SOURCE_FILTER_AND` /
  `TEST_SOURCE_FILTER_OR`, and the closure in
  `pipeline::resolve::refine_ambiguous_targets`) are documented as deliberately
  divergent — enumerate first, then decide per site. See SKILL.md for the
  discovery command; do not work from a remembered count.

- **A mirror can live outside the language you are editing.**
  `claude-plugin/scripts/pr-impact-comment.js::isTestPath` is a JS copy of the
  same rules, and its test is literally named `isTestPath mirrors
  domain::is_test_path patterns`. Widening only the Rust side breaks that parity
  contract and makes the PR "test gaps" comment report every Java/C# test file
  as uncovered production code. This is D7's lesson again: **when one delivery
  surface knows something and a sibling surface does not, that gap is the
  defect.** Measured: an agent given an earlier draft of this skill — which
  named only the Rust and SQL mirrors — changed those two and missed the JS one,
  while an agent with no skill at all searched, found all three, and fixed them.
  A closed set in a document is an instruction to stop looking.

## D5 — separator rewrite that corrupts Unix filenames

Introduced while fixing D1; caught before release. On Unix `\` is a **legal
filename character** (only `/` and NUL are illegal), so an unconditional
`replace('\\', "/")` renames a real `src/od\bc.rs` to `src/od/bc.rs` — printing a
nonexistent path and building a lookup key that misses the indexed one. Same
failure mode as D1, in the opposite direction.

**Fix:** gate the rewrite on whether `\` is a separator *on the target
platform*, passed as a parameter (`normalize_path_display_on`).

## D6 — user-supplied path becoming an index key verbatim

`cli::normalize_user_path_from` returns the key that `affected` / `deps` /
`trace` / `show` look up. Two of its branches returned
`strip_prefix(root).to_string_lossy()` **verbatim**, keeping the native
separator — so on Windows `affected D:\repo\src\Foo.cs` produced the key
`src\Foo.cs` against an index holding `src/Foo.cs`, and a present, indexed file
was reported "not in index".

Note the asymmetry that hid it: the same function's subdirectory branch goes
through `collapse_within_root`, which decomposes into `Component`s and re-joins
with `/` — correct by construction. Only the branches that skipped component
decomposition were wrong.

**Tell:** a `to_string_lossy()` whose result is passed to a DB query, a
`HashMap`, or a `==`. Route it through `merkle::normalize_rel_path` (`Path`
input) or `merkle::normalize_rel_str` (string input) instead.

This is the worked example for the "classify the fix" rule in SKILL.md: on Unix
the correct behaviour here is the identity transform, so a mutation reverting
all three sites leaves the Unix suite green. The regression test
(`normalize_user_path_returns_index_key_spelling`) therefore asserts the
*relation* against `merkle::normalize_rel_path` rather than a literal.

## D7 — executable name matched without `.exe`

`outcome::cli_call_in_line` decided "did the agent invoke our CLI?" with
`t == "code-graph-mcp" || t.ends_with("/code-graph-mcp")`. The plugin resolves an
absolute path, which on Windows is `…\.cache\code-graph\bin\code-graph-mcp.exe`
— missed on **both** counts, separator and suffix. Every Windows invocation went
unrecorded, so the conversion metric read zero and `doctor` reported the funnel
DARK with nothing actually broken.

Worth knowing: the JS delivery surface had `.exe` handling all along
(`find-binary.js`, `auto-update.js`), while `grep -r '\.exe' src/` matched
nothing but `.execute()`. **When one delivery surface knows about a platform
quirk and a sibling surface does not, that gap is the defect.**

When matching a program name, strip the `.exe` suffix first, then accept `/` and
`\` before the stem — and keep the negative cases (`my-code-graph-mcp`,
`code-graph-mcp.exe.bak`) in the test. Live: `outcome::is_cg_binary_token`.

## The good pattern already in the repo

`cli::worktree_main_root` handles a mixed-spelling search correctly: normalize
into a **length-preserving** copy, search that, then slice the **original**
string so the returned path keeps its native separators.

```rust
let norm = s.replace('\\', "/");           // search copy — same byte length
let idx = norm.rfind("/.git/worktrees/")?;
let main_root = PathBuf::from(&s[..idx]);  // slice the ORIGINAL
```

Length-preserving matters: any index found in `norm` is valid in `s`. Reach for
this when the result must stay a usable filesystem path rather than become a
display or lookup key.

Also correct by construction: `indexer::pipeline::is_safe_relative_path`, which
traverses `Path::components()` — under Windows those split on both `/` and `\`,
so `..` rejection cannot be bypassed with the other separator.

## Platform facts worth not re-deriving

- Command line cap: 32,767 chars total on Windows; ~2 MB `ARG_MAX` on POSIX.
- `\\?\` disables Win32 path parsing; `\\?\UNC\server\share` is the UNC form.
- Windows filesystems are case-**insensitive** but case-**preserving**; the same
  volume is legitimately spelled `D:\` or `d:\`. Unix is case-sensitive — so
  case-insensitive comparison must stay Windows-only.
- `\` is legal in Unix filenames; `<>:"|?*` are illegal in Windows ones.
- Windows cannot delete or rename an open file; a test that drops a `TempDir`
  while a child process still holds a handle fails there and nowhere else.
- CRLF checked out on Windows breaks `#!/usr/bin/env python3` (it becomes
  `python3\r`). Invoke bundled scripts through the interpreter
  (`python3 script.py`), never relying on the shebang or the exec bit.
- SQLite string literals escape only `''`, never `\` — a path with a backslash
  needs no escaping, but a path with an apostrophe does.
