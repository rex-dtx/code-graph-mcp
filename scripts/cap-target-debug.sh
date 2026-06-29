#!/usr/bin/env bash
# Cap target/debug — auto-reclaim disk when rust-analyzer + cargo (which never
# GCs stale dep fingerprints) balloon it. Preserves target/release, which the
# project .mcp.json and ~/.cache/code-graph/binary-path point at.
#
# Safe to run unattended (cron / timer): it skips while a compile is active so it
# never yanks artifacts out from under a build, and it only ever touches
# target/debug — never target/release.
#
# Tunables (env):
#   CG_DEBUG_CAP_GB    threshold in GiB before it clears target/debug (default 25)
#   CG_TARGET_DEBUG    override the dir (test hook; default <project>/target/debug)
set -eu

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEBUG_DIR="${CG_TARGET_DEBUG:-$PROJECT_ROOT/target/debug}"
THRESHOLD_GB="${CG_DEBUG_CAP_GB:-25}"

[ -d "$DEBUG_DIR" ] || exit 0

# Guard against an accidental rm of the wrong path (§8): the dir must end in
# /target/debug, never target/release or anything else.
case "$DEBUG_DIR" in
  */target/debug) : ;;
  *) echo "cap-target-debug: refusing unexpected path: $DEBUG_DIR" >&2; exit 1 ;;
esac

# Don't delete artifacts a live build (manual cargo, or rust-analyzer's rustc)
# is mid-write on — try again next tick.
if pgrep -x cargo >/dev/null 2>&1 || pgrep -x rustc >/dev/null 2>&1; then
  echo "$(date -Iseconds) cap-target-debug: compile active, skipping"
  exit 0
fi

size_kb="$(du -sk "$DEBUG_DIR" 2>/dev/null | cut -f1 || true)"
[ -n "${size_kb:-}" ] || exit 0
size_gb=$(( size_kb / 1024 / 1024 ))

if [ "$size_gb" -ge "$THRESHOLD_GB" ]; then
  rm -rf "$DEBUG_DIR"
  echo "$(date -Iseconds) cap-target-debug: cleared target/debug (was ${size_gb}G >= ${THRESHOLD_GB}G)"
else
  echo "$(date -Iseconds) cap-target-debug: target/debug ${size_gb}G (< ${THRESHOLD_GB}G), no action"
fi
