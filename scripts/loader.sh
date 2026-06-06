#!/bin/sh
# Runtime loader for plan.ai component bundles. Extracts ONLY the components the
# current machine needs (app + offline assets + this bundle's runtime + the
# ollama flavour matching uname) into a cache, then launches the dashboard.
#
# Lazy + idempotent: each component is extracted once (a marker records the
# source filename); re-runs skip extraction. The whole point is that the bundle
# can carry every ollama flavour while only the needed one is ever unpacked.
#
# Config via env (set by the bundle's generated launcher):
#   PLANAI_DIR       bundle root (contains components/)        [required]
#   PLANAI_ELECTRON  electron binary to exec                   [required]
#   PLANAI_RUNTIME   runtime archive basename in components/   [required]
#   PLANAI_CHILD_LD_LIBRARY_PATH  libs for child processes     [optional]
#   PLANAI_CACHE     extraction cache (default ~/.cache/plan-ai)
set -eu

DIR="${PLANAI_DIR:?PLANAI_DIR unset}"
COMP="$DIR/components"
CACHE="${PLANAI_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/plan-ai}"
ROOT="$CACHE/root"
mkdir -p "$ROOT/dist"

# pick the ollama flavour for this machine (prefer ROCm if an AMD GPU is present)
arch=$(uname -m); os=$(uname -s)
case "$os:$arch" in
  Linux:x86_64|Linux:amd64)  OLLAMA=ollama-linux-amd64 ;;
  Linux:aarch64|Linux:arm64) OLLAMA=ollama-linux-arm64 ;;
  Darwin:*)                  OLLAMA=ollama-darwin ;;
  *)                         OLLAMA=ollama-linux-amd64 ;;
esac
if [ "$os" = Linux ] && [ -e /dev/kfd ]; then
  for e in tar.zst tgz zip; do [ -f "$COMP/ollama-linux-amd64-rocm.$e" ] && OLLAMA=ollama-linux-amd64-rocm; done
fi

find_archive() { for e in tar.zst tgz zip; do [ -f "$COMP/$1.$e" ] && { echo "$COMP/$1.$e"; return; }; done; }

# extract <archive> <dest> once (marker = archive basename)
extract_once() {
  archive="$1"; dest="$2"
  [ -n "$archive" ] && [ -f "$archive" ] || { echo "missing component: $1" >&2; exit 1; }
  base=$(basename "$archive"); marker="$ROOT/.${dest##*/}.$base.done"
  [ -f "$marker" ] && return 0
  echo "extracting $base ..." >&2
  mkdir -p "$dest"
  case "$archive" in
    *.tar.zst) zstd -dc "$archive" | tar -x -C "$dest" ;;
    *.tgz|*.tar.gz) tar -xzf "$archive" -C "$dest" ;;
    *.zip) unzip -oq "$archive" -d "$dest" ;;
    *) echo "unknown archive type: $archive" >&2; exit 1 ;;
  esac
  : > "$marker"
}

extract_once "$COMP/app.tar.zst"        "$ROOT/app"
extract_once "$COMP/ow-assets.tar.zst"  "$ROOT/dist/ow-assets"
extract_once "$COMP/$PLANAI_RUNTIME"    "$ROOT/dist/runtime"
extract_once "$(find_archive "$OLLAMA")" "$ROOT/dist/ollama"

export PLANAI_PORTABLE_ROOT="${PLANAI_PORTABLE_ROOT:-$DIR}"   # models/data beside the bundle
[ -n "${PLANAI_CHILD_LD_LIBRARY_PATH:-}" ] && export PLANAI_CHILD_LD_LIBRARY_PATH
exec "$PLANAI_ELECTRON" "$ROOT/app" --no-sandbox "$@"
