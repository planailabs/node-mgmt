#!/usr/bin/env bash
# Launch the LINUX bundle on this (NixOS) host under xvfb with a fresh extraction
# cache, and assert the dashboard renders. There's no separate NixOS bundle: the
# linux-x64 artifact ships the FHS helper as a squashfs, and the static-musl launcher
# detects NixOS, squashfuse-mounts it, and (in an outer bwrap) provides it as
# /nix/store before FHS-reexecing so the generic electron/ollama run. Exercises the
# full path: rust launcher -> bwrap FHS -> control plane (ollama + open-webui) ->
# localhost SPA -> thin electron.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

OUT="$DIST_DIR/bundle"
LAUNCHER="$OUT/node-mgmt.linux-x64.exe"
[ -x "$LAUNCHER" ] || die "linux bundle missing — run: make bundle TARGET=linux-x64"
[ -f "$OUT/components/linux-x64/manifest.json" ] || die "components/linux-x64 group missing beside the launcher — run: make components bundle TARGET=linux-x64"
SHOT="${1:-/tmp/nixos-dash.png}"; rm -f "$SHOT"

export PLANAI_CAPTURE="$SHOT" PLANAI_CAPTURE_DELAY="${PLANAI_CAPTURE_DELAY:-50000}"
export PLANAI_CACHE; PLANAI_CACHE="$(mktemp -d)/cache"   # fresh extraction this run

log "launching linux bundle on NixOS (PLANAI_CACHE=$PLANAI_CACHE)"
xvfb-run -a -s "-screen 0 1400x900x24" "$LAUNCHER" >/tmp/nixos-run.log 2>&1 || true

if [ -s "$SHOT" ]; then
  log "screenshot -> $SHOT ($(stat -c%s "$SHOT") bytes)"
  log "extracted into cache:"; ls "$PLANAI_CACHE/root/dist" 2>/dev/null | sed 's/^/    /'
else
  warn "no screenshot — run log:"; tail -20 /tmp/nixos-run.log
  die "linux bundle did not render on NixOS"
fi
rm -rf "$(dirname "$PLANAI_CACHE")"
