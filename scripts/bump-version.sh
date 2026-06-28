#!/bin/bash
set -euo pipefail
VERSION=${1:?Usage: scripts/bump-version.sh <version>}
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Build the dev binary WITH embed-model so the local MCP server (.mcp.json →
# target/release/code-graph-mcp) keeps producing semantic vectors. A bare bump
# would rebuild a no-embed binary, silently disabling vector search for every dev
# project sharing target/release (see feedback_build_release). Opt out by setting
# SYNC_VERSIONS_FEATURES= (empty) or SYNC_VERSIONS_SKIP_BUILD=1 before invoking.
# `-` (not `:-`) so an explicitly-empty SYNC_VERSIONS_FEATURES= is honored as
# "no features" (matching sync-versions.js, which treats empty as a no-embed
# build); only an UNSET var defaults to embed-model. Safe under `set -u`.
export SYNC_VERSIONS_FEATURES="${SYNC_VERSIONS_FEATURES-embed-model}"
node scripts/sync-versions.js "$VERSION"

# Cargo.lock
cargo update -p code-graph-mcp 2>/dev/null || true
echo "Updated Cargo.lock"

echo ""
echo "All versions updated to $VERSION"
echo "Next: git add -A && git commit -m 'chore: bump to $VERSION' && git tag v$VERSION && git push && git push --tags"
