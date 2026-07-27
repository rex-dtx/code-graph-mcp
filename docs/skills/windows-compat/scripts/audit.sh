#!/usr/bin/env bash
# Scan this repo for the path/process patterns behind the shipped Windows
# defects (issues #34, #35, #36). Advisory: a hit is a place where a Windows
# spelling can diverge from a Unix one, not a proven bug. Each one needs a read
# against references/failure-modes.md.
#
# The patterns are deliberately narrow. An earlier revision matched every
# `Command::new` / `.arg(` (48 hits, no signal) and every quoted program name
# (28 hits, mostly language names in the parser) — noise that large trains you
# to skim the output, which defeats the point. Prefer a pattern that stays
# quiet until it has something to say.
#
# Usage: bash docs/skills/windows-compat/scripts/audit.sh [path...]
# Exit:  0 = no hits, 1 = hits found (so it can gate a pre-release check)
set -uo pipefail

PATHS=("${@:-src}")
hits=0

# ripgrep is this repo's own hard dependency, so assume it; fall back to grep -r.
if command -v rg >/dev/null 2>&1; then
  scan() { rg --line-number --no-heading --color=never "$1" "${PATHS[@]}" -g '!*/tests.rs' 2>/dev/null; }
  files_with() { rg --files-with-matches "$1" "${PATHS[@]}" -g '!*/tests.rs' 2>/dev/null; }
  file_has() { rg --quiet "$2" "$1" 2>/dev/null; }
else
  scan() { grep -rn --exclude='tests.rs' -E "$1" "${PATHS[@]}" 2>/dev/null; }
  files_with() { grep -rlE --exclude='tests.rs' "$1" "${PATHS[@]}" 2>/dev/null; }
  file_has() { grep -qE "$2" "$1" 2>/dev/null; }
fi

report() { # <title> <why> <pattern>
  local out
  out="$(scan "$3")" || true
  [ -z "$out" ] && return 0
  hits=$((hits + 1))
  printf '\n=== %s ===\n%s\n' "$1" "$2"
  printf '%s\n' "$out"
}

report "Separator rewrite without a platform flag" \
  "\`\\\` is a legal FILENAME char on Unix, so an unconditional replace renames real files and breaks index lookups (D5). Take the platform as a parameter, like normalize_path_display_on, so the Linux CI leg exercises the Windows branch." \
  'replace\(.\\\\.,\s*"/"\)|to_slash|from_slash'

report "Inline cfg!(windows) in path or compare logic" \
  "Untestable from the Linux/macOS legs — the exact blind spot that let #34 ship past an existing windows-latest matrix. Hoist into an _on(..., windows: bool) parameter and cover both values." \
  'cfg!\(windows\)|cfg\(windows\)'

report "to_string_lossy() feeding a comparison or map key" \
  "Produces the NATIVE spelling. If the other side came from git (always /) or from the index (normalize_rel_path, / only), the two never compare equal — that is D6. Route it through merkle::normalize_rel_path / normalize_rel_str." \
  'to_string_lossy\(\)'

report "canonicalize() whose result can reach stdout or a key" \
  "Returns the \\\\?\\ extended prefix on Windows (D2), which never string-compares equal to the non-canonical root. Strip it before printing or comparing." \
  '\.canonicalize\(\)'

report "Collection of path operands passed to a spawned process" \
  "Windows caps a whole command line at 32,767 chars (os error 206, D3). A collection argument can grow with repo size, so bound it in BYTES and batch — a count cap does not bound length. Inline flag arrays are fine and are not matched here." \
  '\.args\(&?[a-z_][a-z0-9_]*\)'

report "Path predicate hardcoding a separator or an anchored layout" \
  "Anchored prefixes like starts_with(\"tests/\") see only a repo-root layout and miss both the Windows spelling and other ecosystems (#36: src/Tests/... in C#). Prefer case-insensitive segment containment." \
  'starts_with\("[a-zA-Z_]+/|ends_with\("/'

# Two filters make this class quiet enough to be worth reading. The pattern
# wants a name that looks like a BINARY — hyphenated (code-graph-mcp) or sitting
# behind a path separator — which is what keeps the parser's `== "rust"` /
# `== "python"` language checks out of the results. Then any file that already
# mentions `.exe` is dropped, because it has demonstrably been through this.
# That second filter is what makes silence meaningful: this class fired on
# outcome.rs before the D7 fix and is quiet now, whereas a bare grep for program
# names cannot tell those two states apart.
EXE_PAT='==[[:space:]]*"[a-z][a-z0-9]*(-[a-z0-9]+)+"|ends_with\("[/\\][a-z][a-z0-9-]*"'
exe_out=""
while IFS= read -r f; do
  [ -n "$f" ] || continue
  file_has "$f" '\.exe' && continue
  local_hits="$(grep -nE "$EXE_PAT" "$f" | sed "s|^|  $f:|")"
  [ -n "$local_hits" ] && exe_out+="$local_hits"$'\n'
done < <(files_with "$EXE_PAT")
if [ -n "$exe_out" ]; then
  hits=$((hits + 1))
  printf '\n=== Executable name matched without .exe ===\n%s\n' \
    "Windows binaries carry a .exe suffix AND sit behind a \\ separator, so matching only \"name\" or \"/name\" silently fails there (D7) — every Windows invocation went unrecorded and doctor reported the funnel DARK with nothing broken. Strip .exe first, then accept both separators. Files below spawn or name a program and contain no .exe handling:"
  printf '%s' "$exe_out"
fi

printf '\n---\n'
if [ "$hits" -eq 0 ]; then
  printf 'No pattern classes hit in: %s\n' "${PATHS[*]}"
  exit 0
fi
printf '%d pattern class(es) hit. Each needs a read against references/failure-modes.md.\n' "$hits"
exit 1
