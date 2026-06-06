#!/usr/bin/env bash
# Clean all generated build outputs and rebuild from scratch, then verify.
#
# Usage: scripts/test-clean-build.sh [target] [--full]
#   target : linux-x64 (default) | win-x64 | mac-arm64 | mac-x64
#   --full : also wipe the vendor/ download cache (re-downloads ollama + source)
#
# Without --full the download cache is kept (the wheel, runtime and bundle are
# still rebuilt), so a "clean build" is verified without re-fetching gigabytes.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

TARGET="linux-x64"; FULL=no
while [ $# -gt 0 ]; do
  case "$1" in
    --full) FULL=yes; shift ;;
    -*) die "unknown flag: $1" ;;
    *) TARGET="$1"; shift ;;
  esac
done

log "clean build outputs"
rm -rf "$DIST_DIR" "$REPO_ROOT/app/node_modules" "$REPO_ROOT/app/renderer/tailwind.css" "$REPO_ROOT/app/.stage"
[ "$FULL" = yes ] && { log "wipe vendor cache (--full)"; rm -rf "$VENDOR_DIR"; }

run() { log "+ $*"; nix develop --command bash -c "$*"; }

# ollama flavour for the target
case "$TARGET" in
  linux-x64) FLAV=ollama-linux-amd64.tar.zst ;;
  win-x64)   FLAV=ollama-windows-amd64.zip ;;
  mac-*)     FLAV=ollama-darwin.tgz ;;
esac

run "./scripts/download-ollama.sh $FLAV"
run "./scripts/download-openwebui.sh"
run "./scripts/build-openwebui.sh"
run "./scripts/make-runtime.sh $TARGET"
run "cd app && npm ci"
run "./scripts/bundle.sh $TARGET"

# verify the artifact exists
case "$TARGET" in
  linux-x64) ART="$DIST_DIR/bundle/plan-ai-"*"-linux-"*".AppImage" ;;
  win-x64)   ART="$DIST_DIR/bundle/plan-ai-"*"-win-"*".exe" ;;
  mac-*)     ART="$DIST_DIR/bundle/plan-ai-"*"-mac-"*".zip" ;;
esac
# shellcheck disable=SC2086
if ls $ART >/dev/null 2>&1; then
  log "CLEAN BUILD OK -> $(ls -lh $ART | awk '{print $5, $NF}')"
else
  die "clean build produced no artifact for $TARGET"
fi
