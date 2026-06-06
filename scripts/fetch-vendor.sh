#!/usr/bin/env bash
# Materialise the vendored downloads from the Nix fixed-output derivations
# (packages.vendor) into vendor/ — replacing the curl-based download scripts.
# Nix fetches + verifies each archive once and caches it content-addressed in
# /nix/store (deduped across builds/CI); here we just symlink them into the
# layout the build scripts expect, and extract the open-webui source.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need nix; need tar
OLLAMA_TAG="$(ollama_version)"; OW_TAG="$(ow_version)"

[ -f "$REPO_ROOT/vendor.lock.json" ] || die "vendor.lock.json missing — run scripts/gen-vendor-lock.sh"

log "nix build .#vendor (fixed-output downloads — cached in /nix/store)"
RESULT="$(nix build "$REPO_ROOT#vendor" --no-link --print-out-paths)"

link() { ln -sfn "$(readlink -f "$1")" "$2"; }   # point at the real store file

mkdir -p "$VENDOR_DIR/ollama/$OLLAMA_TAG" "$VENDOR_DIR/open-webui/$OW_TAG" "$VENDOR_DIR/pbs"
for f in "$RESULT/ollama/$OLLAMA_TAG"/*; do link "$f" "$VENDOR_DIR/ollama/$OLLAMA_TAG/$(basename "$f")"; done
for f in "$RESULT/pbs"/*;                 do link "$f" "$VENDOR_DIR/pbs/$(basename "$f")"; done
link "$RESULT/open-webui/$OW_TAG/source.tar.gz" "$VENDOR_DIR/open-webui/$OW_TAG/source.tar.gz"

# extract the open-webui source (what download-openwebui.sh used to do)
SRC="$VENDOR_DIR/open-webui/$OW_TAG/src"
if [ ! -f "$SRC/pyproject.toml" ]; then
  rm -rf "$SRC"; mkdir -p "$SRC"
  tar -xzf "$VENDOR_DIR/open-webui/$OW_TAG/source.tar.gz" -C "$SRC" --strip-components=1
fi

log "vendor ready: $(ls "$VENDOR_DIR/ollama/$OLLAMA_TAG" | wc -l) ollama, $(ls "$VENDOR_DIR/pbs" | wc -l) pbs, open-webui src extracted"
