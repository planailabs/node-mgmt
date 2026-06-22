#!/usr/bin/env bash
# Shared helpers for plan-ai-usb-minimal scripts.
# Source this file: . "$(dirname "$0")/lib.sh"
#
# The shared build helpers (paths, logging, need, temp-dirs, loader_cfg, lock, …)
# are OWNED by the loader submodule — this file INHERITS them rather than mirroring
# them, so there is nothing to keep in sync. Only the project-local helpers below
# (no upstream consumer) live here.
#
# Pin REPO_ROOT to THIS repo before sourcing upstream: the submodule's lib.sh
# honours PLANAI_REPO_ROOT and would otherwise re-root to third_party/loader via
# its own $SCRIPT_DIR/.. (this file sits at the project root's scripts/, so
# $BASH_SOURCE/.. is the project root). Set, but do NOT export: the build engine
# exports it for the real build, while test-clean-build.sh deliberately runs its
# worktree sub-builds WITHOUT it so they self-root to the worktree — exporting
# here would leak the main-tree root into those and break the isolation.
PLANAI_REPO_ROOT="${PLANAI_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
. "$PLANAI_REPO_ROOT/third_party/loader/scripts/lib.sh"

# ── project-local helpers ────────────────────────────────────────────────────
# --- test drive provisioning ------------------------------------------------
# Used by the remote test harnesses (test-mac.sh / test-win.sh). Seed a staged
# drive-root with update.json + platforms.json so the launcher treats it as an
# already-provisioned drive and runs from the LOCAL pushed components — instead of
# bootstrapping a full download from the PROD update server (the launcher sets
# bootstrap_update when either file is absent). Mirrors what make-usb-image.sh
# writes onto a real drive, so the remote test validates the locally-built
# artifacts rather than prod.
#
#   seed_drive_manifest <staged-drive-root> <platform>
#
# <staged-drive-root> must already mirror the remote drive layout (the launcher
# at the root + components/<platform>/…); the manifest is generated from it, so
# its entries match what gets pushed. The URL is a dead localhost address: the
# test never auto-fetches (bootstrap is off), and a manual check can't reach prod.
seed_drive_manifest() {
  local drive="$1" platform="$2"
  need jq
  local commit; commit="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo test)"
  ( cd "$REPO_ROOT" && nix run ".#xtask" -- gen-manifest "$drive" \
      --version "0.0.0-test" --commit "$commit" --url "http://127.0.0.1:1/" \
      --out "$drive/update.json" ) || die "gen-manifest failed for $drive"
  printf '{"platforms":["%s"]}\n' "$platform" > "$drive/platforms.json"
  log "seeded update.json + platforms.json ($platform) — launcher runs from the local pool"
}
