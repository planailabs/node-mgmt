#!/usr/bin/env bash
# Verify the project builds from a CLEAN checkout. Uses a throwaway git worktree
# so the main tree's artifacts are untouched, and reuses the vendor/ download
# cache (symlinked) so gigabytes aren't re-fetched. Builds the wheel, runtime and
# bundle for TARGET from scratch and asserts the artifact exists.
#
# Usage: scripts/test-clean-build.sh [target] [--full]
#   target : linux-x64 (default) | win-x64 | mac-arm64 | mac-x64
#   --full : do NOT reuse vendor/ — re-download everything in the clean tree
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

need git
WT="$(mktemp -d)/clean"
cleanup() { git -C "$REPO_ROOT" worktree remove --force "$WT" 2>/dev/null || true; rm -rf "$(dirname "$WT")"; }
trap cleanup EXIT

log "create clean worktree at $WT"
git -C "$REPO_ROOT" worktree add --detach "$WT" HEAD >/dev/null
git -C "$WT" submodule update --init --recursive third_party/plan-ai-design third_party/loader >/dev/null 2>&1 || true

if [ "$FULL" = no ] && [ -d "$VENDOR_DIR" ]; then
  log "reuse vendor cache (symlink)"; ln -sfn "$VENDOR_DIR" "$WT/vendor"
fi

case "$TARGET" in
  linux-x64) FLAV=ollama-linux-amd64.tar.zst ;;
  win-x64)   FLAV=ollama-windows-amd64.zip ;;
  mac-*)     FLAV=ollama-darwin.tgz ;;
esac

run() { log "+ $*"; ( cd "$WT" && nix develop "$REPO_ROOT" --command bash -c "$*" ); }
run "./scripts/download-ollama.sh $FLAV"
run "./scripts/download-openwebui.sh"
run "./scripts/build-openwebui.sh"
run "./scripts/make-runtime.sh $TARGET"
run "cd app && npm ci"
# the bundler is loader-owned (the real build calls @loader/bundle.sh); run the
# submodule copy with PLANAI_REPO_ROOT pinned to the worktree (its lib.sh would
# otherwise re-root to third_party/loader via $SCRIPT_DIR/..).
run "PLANAI_REPO_ROOT=\$PWD ./third_party/loader/scripts/bundle.sh $TARGET"

case "$TARGET" in
  linux-x64) ART=("$WT"/dist/bundle/plan-ai-*-linux-*.AppImage) ;;
  win-x64)   ART=("$WT"/dist/bundle/plan-ai-*-win-*.zip) ;;
  mac-*)     ART=("$WT"/dist/bundle/plan-ai-*-mac-*.zip) ;;
esac
if [ -e "${ART[0]}" ]; then
  log "CLEAN BUILD OK -> $(ls -lh "${ART[0]}" | awk '{print $5, $NF}')"
else
  die "clean build produced no artifact for $TARGET"
fi
