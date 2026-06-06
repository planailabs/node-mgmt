#!/usr/bin/env bash
# Pre-seed the USB models directory with the models listed in usb.lock so the
# stick works offline. Uses the downloaded ollama binary (host flavour) to
# serve + pull into a target OLLAMA_MODELS dir.
#
# Usage: scripts/seed-models.sh [models-dir]
#   models-dir defaults to ./models (the dir the app uses in dev).
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

MODELS_DIR="${1:-$REPO_ROOT/models}"
mkdir -p "$MODELS_DIR"

mapfile -t MODELS < <(jq -r '.models[]?' "$USB_LOCK")
if [ "${#MODELS[@]}" -eq 0 ]; then
  warn 'no models listed in usb.lock (.models = []) — nothing to seed'
  exit 0
fi

# Locate an ollama binary: prefer the downloaded linux-amd64 flavour, else PATH.
OLLAMA_TAG="$(ollama_version)"
EXTRACT="$VENDOR_DIR/ollama/$OLLAMA_TAG/.host-bin"
OLLAMA_BIN=""
if command -v ollama >/dev/null 2>&1; then
  OLLAMA_BIN="$(command -v ollama)"
else
  SRC="$VENDOR_DIR/ollama/$OLLAMA_TAG/ollama-linux-amd64.tar.zst"
  [ -f "$SRC" ] || die "no ollama on PATH and $SRC missing — run download-ollama.sh"
  need zstd; need tar
  rm -rf "$EXTRACT"; mkdir -p "$EXTRACT"
  zstd -dc "$SRC" | tar -x -C "$EXTRACT"
  OLLAMA_BIN="$(ls "$EXTRACT"/bin/ollama "$EXTRACT"/ollama 2>/dev/null | head -1)"
  [ -n "$OLLAMA_BIN" ] || die "could not find ollama in extracted archive"
fi

log "seeding ${#MODELS[@]} model(s) into $MODELS_DIR using $OLLAMA_BIN"
export OLLAMA_MODELS="$MODELS_DIR"
export OLLAMA_HOST="127.0.0.1:11434"

"$OLLAMA_BIN" serve >/tmp/ollama-seed.log 2>&1 &
SERVE_PID=$!
trap 'kill "$SERVE_PID" 2>/dev/null || true' EXIT

# wait for readiness
for _ in $(seq 1 30); do
  curl -sf "http://$OLLAMA_HOST/api/version" >/dev/null 2>&1 && break
  sleep 1
done

for m in "${MODELS[@]}"; do
  log "pull $m"
  "$OLLAMA_BIN" pull "$m"
done

log "seed complete -> $MODELS_DIR"
