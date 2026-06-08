#!/usr/bin/env bash
# Pre-seed the USB models directory with the models listed in usb.lock so the
# stick works offline. Uses the VENDORED ollama (the usb.lock-pinned version) to
# serve + pull into a target OLLAMA_MODELS dir — on NixOS through the project's FHS
# sandbox (.#nixosFhs), since the generic glibc binary can't run on the bare stub.
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

# Always use the VENDORED ollama (the exact usb.lock-pinned version), never a host
# PATH ollama — the seeded model store must be reproducible and match what the USB
# ships, regardless of whatever ollama the dev box happens to have installed.
OLLAMA_TAG="$(ollama_version)"
EXTRACT="$VENDOR_DIR/ollama/$OLLAMA_TAG/.host-bin"
SRC="$VENDOR_DIR/ollama/$OLLAMA_TAG/ollama-linux-amd64.tar.zst"
[ -f "$SRC" ] || die "vendored ollama missing: $SRC — run scripts/fetch-vendor.sh (make download)"
need zstd; need tar
rm -rf "$EXTRACT"; mkdir -p "$EXTRACT"
zstd -dc "$SRC" | tar -x -C "$EXTRACT"
OLLAMA_BIN="$(ls "$EXTRACT"/bin/ollama "$EXTRACT"/ollama 2>/dev/null | head -1)"
[ -n "$OLLAMA_BIN" ] || die "could not find ollama in extracted archive"

# The vendored ollama is a generic glibc binary. On NixOS there's no
# /lib64/ld-linux and the bare nix-ld stub can't run it, so wrap it in the
# project's FHS sandbox (.#nixosFhs -> planai-fhs, runScript = `exec "$@"`, the
# same env the launcher uses to run the bundled ollama on NixOS). On a normal FHS
# distro (CI, the ubuntu VM) run it directly. RUN prefixes serve + pull; an empty
# array expands to nothing under bash's set -u.
RUN=()
if [ -e /etc/NIXOS ] || [ ! -e /lib64/ld-linux-x86-64.so.2 ]; then
  log "NixOS / non-FHS host — running the vendored ollama via planai-fhs"
  FHS="$(cd "$REPO_ROOT" && nix build .#nixosFhs --no-link --print-out-paths 2>/dev/null)/bin/planai-fhs"
  [ -x "$FHS" ] || die "could not build .#nixosFhs (needed to run the generic ollama on NixOS)"
  RUN=("$FHS")
fi

log "seeding ${#MODELS[@]} model(s) into $MODELS_DIR using $OLLAMA_BIN"
export OLLAMA_MODELS="$MODELS_DIR"
# Run a PRIVATE ollama server on a dedicated port — NOT the default 11434. If the
# host already has an ollama running there (common on a dev box), serving on 11434
# would lose the bind race and our `ollama pull` would talk to THAT server instead,
# writing into its models dir rather than OLLAMA_MODELS. A separate port guarantees
# the pull hits our server and lands in $MODELS_DIR. Override with PLANAI_SEED_PORT.
PORT="${PLANAI_SEED_PORT:-11435}"
export OLLAMA_HOST="127.0.0.1:$PORT"

"${RUN[@]}" "$OLLAMA_BIN" serve >/tmp/ollama-seed.log 2>&1 &
SERVE_PID=$!
# kill the wrapper AND the (possibly FHS-sandboxed) ollama it spawned — bwrap
# doesn't always reap children; pkill on OUR vendored path can't hit a host ollama.
trap 'kill "$SERVE_PID" 2>/dev/null || true; pkill -f "$OLLAMA_BIN serve" 2>/dev/null || true' EXIT

# wait for OUR server (and fail fast if it exited early — e.g. the port was taken)
ready=no
for _ in $(seq 1 30); do
  if curl -sf "http://$OLLAMA_HOST/api/version" >/dev/null 2>&1; then ready=yes; break; fi
  kill -0 "$SERVE_PID" 2>/dev/null || die "ollama serve exited early (port $PORT taken? see /tmp/ollama-seed.log)"
  sleep 1
done
[ "$ready" = yes ] || die "ollama serve not ready on $OLLAMA_HOST after 30s (see /tmp/ollama-seed.log)"

for m in "${MODELS[@]}"; do
  log "pull $m -> $MODELS_DIR"
  "${RUN[@]}" "$OLLAMA_BIN" pull "$m"
done

log "seed complete -> $MODELS_DIR"
