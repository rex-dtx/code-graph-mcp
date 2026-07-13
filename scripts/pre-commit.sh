#!/usr/bin/env bash
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"

# Strip the GIT_* vars git exports into this hook before running ANY subprocess
# that shells out to git for tempdir fixtures — both the JS tests (§2) and the
# Rust tests (§3). Without this, a partial `git commit` leaks GIT_DIR /
# GIT_INDEX_FILE / GIT_WORK_TREE into the child, which then operates on THIS
# repo's index instead of its tempdir: `git add .` records a fixture file into
# the real index and the fixture commit comes back empty. v0.80.3 fixed this for
# the cargo path only; the JS section was the sibling hole (H4), so the stripper
# is hoisted here and applied to both sections.
git_clean=(env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE -u GIT_OBJECT_DIRECTORY -u GIT_COMMON_DIR -u GIT_NAMESPACE -u GIT_PREFIX)

# ── 1. Version consistency check ─────────────────────────────
# If any version-bearing file is staged, verify EVERY location sync-versions.js
# writes matches the staged package.json: Cargo.toml, plugin.json, marketplace.json
# (metadata.version AND plugins[0].version — the field the marketplace actually
# reads), the 5 npm/<plat>/package.json, and package.json's 5 optionalDependencies
# pins. The old guard checked only 4 — a lagging linux-arm64 pin or plugins[0].version
# (no CI runner for either) shipped a release npm/marketplace couldn't resolve.
VERSION_FILES=(
  "package.json"
  "Cargo.toml"
  "claude-plugin/.claude-plugin/plugin.json"
  ".claude-plugin/marketplace.json"
  "npm/linux-x64/package.json"
  "npm/linux-arm64/package.json"
  "npm/darwin-x64/package.json"
  "npm/darwin-arm64/package.json"
  "npm/win32-x64/package.json"
)

staged_files=$(git diff --cached --name-only)
version_staged=false
for vf in "${VERSION_FILES[@]}"; do
  if echo "$staged_files" | grep -qF "$vf"; then
    version_staged=true
    break
  fi
done

if $version_staged; then
  # Read versions from STAGED content (git show :<path>), so a partial bump is
  # caught before it lands. $2 is a JS accessor (.version / .metadata.version / ...).
  read_json_ver() { git show ":$1" | node -e "process.stdout.write(String(JSON.parse(require('fs').readFileSync('/dev/stdin','utf8'))$2))"; }
  ref=$(read_json_ver package.json .version)
  mismatch=false
  report() { if [ "$2" != "$ref" ]; then echo "❌ Version mismatch: $1=$2 vs package.json=$ref"; mismatch=true; fi; }

  report Cargo.toml "$(git show :Cargo.toml | grep -m1 '^version' | sed 's/version = "\(.*\)"/\1/')"
  report plugin.json "$(read_json_ver 'claude-plugin/.claude-plugin/plugin.json' .version)"
  report marketplace.metadata "$(read_json_ver '.claude-plugin/marketplace.json' .metadata.version)"
  report 'marketplace.plugins[0]' "$(read_json_ver '.claude-plugin/marketplace.json' '.plugins[0].version')"
  for plat in linux-x64 linux-arm64 darwin-x64 darwin-arm64 win32-x64; do
    report "npm/$plat" "$(read_json_ver "npm/$plat/package.json" .version)"
  done
  # package.json optionalDependencies pins (npm install resolves the platform binary via these)
  while IFS=$'\t' read -r dep ver; do
    [ -n "$dep" ] && report "optionalDependencies[$dep]" "$ver"
  done < <(git show :package.json | node -e "const d=JSON.parse(require('fs').readFileSync('/dev/stdin','utf8')).optionalDependencies||{};for(const[k,v]of Object.entries(d))process.stdout.write(k+'\t'+v+'\n')")

  if $mismatch; then
    echo ""
    echo "Fix: node scripts/sync-versions.js $ref"
    echo "Then: git add the updated files"
    exit 1
  fi

  echo "✓ Version consistency: $ref (Cargo + plugin + marketplace×2 + 5 npm + optionalDeps)"
fi

# ── 2. Plugin JS tests ───────────────────────────────────────
# Run all JS tests if any JS file under claude-plugin/ or scripts/ is staged.
# scripts/*.test.js included since v0.49.1: install-e2e.test.js asserts
# lifecycle.js behavior but lived outside this gate (and outside CI), so the
# v0.32.0 hooks-to-settings.json inversion silently stranded it for 3 months.
js_staged=$(echo "$staged_files" | grep -c '^\(claude-plugin\|scripts\)/.*\.js$' || true)
if [ "$js_staged" -gt 0 ]; then
  echo "Running plugin JS tests..."
  for t in "$ROOT"/claude-plugin/scripts/*.test.js "$ROOT"/scripts/*.test.js; do
    [ -f "$t" ] || continue
    if ! "${git_clean[@]}" node --test "$t" > /dev/null 2>&1; then
      echo "❌ Test failed: $(basename "$t")"
      echo "Run: node --test $t"
      exit 1
    fi
  done
  echo "✓ Plugin JS tests passed"
fi

# ── 3. Rust checks ───────────────────────────────────────────
# Run cargo check + test if any Rust source is staged.
rs_staged=$(echo "$staged_files" | grep -c '\.rs$' || true)
cargo_staged=$(echo "$staged_files" | grep -c 'Cargo\.\(toml\|lock\)' || true)
if [ "$rs_staged" -gt 0 ] || [ "$cargo_staged" -gt 0 ]; then
  # git_clean (defined at the top) strips GIT_* so cargo tests that shell out to
  # git for fixtures (src/snapshot/tests.rs init_git_fixture) run against their
  # tempdir, not this repo's index.
  echo "Running cargo check..."
  if ! "${git_clean[@]}" cargo check --quiet 2>&1; then
    echo "❌ cargo check failed"
    exit 1
  fi
  echo "✓ cargo check passed"

  echo "Running cargo test..."
  if ! "${git_clean[@]}" cargo test --quiet 2>&1; then
    echo "❌ cargo test failed"
    exit 1
  fi
  echo "✓ cargo test passed"
fi

echo "Pre-commit checks passed."
