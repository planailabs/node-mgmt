#!/usr/bin/env bash
# Launch the built NixOS bundle (dist/bundle/plan-ai-nixos-x64/plan-ai) under
# xvfb with a fresh extraction cache, and assert the dashboard renders. Exercises
# the full path: nixpkgs electron -> in-app component loader (extract runtime +
# ollama, patchelf ollama) -> supervise ollama + open-webui.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DIR="$DIST_DIR/bundle/plan-ai-nixos-x64"
[ -x "$DIR/plan-ai" ] || die "nixos bundle missing — run: make nixos"
SHOT="${1:-/tmp/nixos-dash.png}"; rm -f "$SHOT"

export PLANAI_CAPTURE="$SHOT" PLANAI_CAPTURE_DELAY="${PLANAI_CAPTURE_DELAY:-50000}"
export PLANAI_CACHE; PLANAI_CACHE="$(mktemp -d)/cache"   # fresh extraction this run

log "launching nixos bundle (PLANAI_CACHE=$PLANAI_CACHE)"
xvfb-run -a -s "-screen 0 1400x900x24" "$DIR/plan-ai" >/tmp/nixos-run.log 2>&1 || true

if [ -s "$SHOT" ]; then
  log "nixos bundle screenshot -> $SHOT ($(stat -c%s "$SHOT") bytes)"
  log "extracted into cache:"; ls "$PLANAI_CACHE/root/dist" 2>/dev/null | sed 's/^/    /'
else
  warn "no screenshot — run log:"; tail -20 /tmp/nixos-run.log
  die "nixos bundle did not render"
fi
rm -rf "$(dirname "$PLANAI_CACHE")"
