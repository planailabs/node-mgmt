#!/usr/bin/env bash
# Run the full stack natively on NixOS, WITHOUT the generic AppImage.
#
# The AppImage bundles a generic Electron that the bare nix-ld stub can't run.
# Instead this launcher uses the nixpkgs Electron (via ELECTRON_OVERRIDE_DIST_PATH,
# set by the devshell) on the app sources, and stages the real build outputs into
# dist/ where paths.js (dev mode) expects them:
#
#   dist/ollama/bin/ollama        (extracted linux-amd64 flavour)
#   dist/runtime/venv/...         (relocatable python-build-standalone venv)
#   dist/ow-assets/{hf,nltk}      (offline embedding model + nltk data)
#
# ollama is a static Go binary (runs on NixOS as-is); the standalone venv's
# native wheels (onnxruntime/chromadb) need FHS libs, so we export
# LD_LIBRARY_PATH from the devshell's nix library path. Children inherit it.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

TARGET="linux-x64"
OLLAMA_TAG="$(ollama_version)"
OLLAMA_SRC="$VENDOR_DIR/ollama/$OLLAMA_TAG/ollama-linux-amd64.tar.zst"
RT="$DIST_DIR/runtime/$TARGET"
APP="$REPO_ROOT/app"

[ -f "$OLLAMA_SRC" ] || die "ollama linux-amd64 missing — ./scripts/download-ollama.sh linux-amd64"
[ -d "$RT/venv" ]    || die "runtime venv missing — ./scripts/make-runtime.sh linux-x64"

# --- stage resources into dist/ (paths.js dev layout) ----------------------
log "staging resources into $DIST_DIR"
if [ ! -x "$DIST_DIR/ollama/bin/ollama" ] && [ ! -x "$DIST_DIR/ollama/ollama" ]; then
  rm -rf "$DIST_DIR/ollama"; mkdir -p "$DIST_DIR/ollama"
  need zstd; need tar
  zstd -dc "$OLLAMA_SRC" | tar -x -C "$DIST_DIR/ollama"
fi
mkdir -p "$DIST_DIR/runtime"
ln -sfn "$RT/venv" "$DIST_DIR/runtime/venv"
[ -d "$VENDOR_DIR/ow-assets" ] && ln -sfn "$VENDOR_DIR/ow-assets" "$DIST_DIR/ow-assets"

# --- ensure css built ------------------------------------------------------
[ -f "$APP/renderer/tailwind.css" ] || ( cd "$APP" && npm run css )

# --- FHS libs for the python native wheels (+ ollama runners) ---------------
if [ -n "${NIX_LD_LIBRARY_PATH:-}" ]; then
  export LD_LIBRARY_PATH="${NIX_LD_LIBRARY_PATH}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  log "LD_LIBRARY_PATH set for native deps"
else
  warn "NIX_LD_LIBRARY_PATH unset — run inside 'nix develop'"
fi

log "launching dashboard via nixpkgs electron"
cd "$APP"
exec ./node_modules/.bin/electron . "$@"
