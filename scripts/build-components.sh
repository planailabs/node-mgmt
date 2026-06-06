#!/usr/bin/env bash
# Build modular component archives consumed by the in-app loader (main/loader.js)
# on every platform. Each OS artifact ships the components/ for its OS; the app
# extracts only what the machine needs (its runtime + the ollama flavour matching
# the CPU arch) on first launch.
#
# Outputs dist/components/  (all .tar.gz so the node loader streams them with the
# pure-JS `tar` package — no zstd/native dep at runtime):
#   ow-assets.tar.gz             offline embedding model + nltk (shared)
#   runtime-<target>.tar.gz      python runtime, one per built target
#   ollama-<flavour>.tar.gz      ollama, normalized from each downloaded flavour
#   manifest.json                info (loader auto-detects, this is for humans/CI)
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need tar; need jq
OUT="$DIST_DIR/components"; rm -rf "$OUT"; mkdir -p "$OUT"
OLLAMA_TAG="$(ollama_version)"
OLLAMA_DIR="$VENDOR_DIR/ollama/$OLLAMA_TAG"

# parallel gzip (pigz) when available; output is plain gzip (the node loader's
# `tar` reads it). falls back to gzip.
if command -v pigz >/dev/null 2>&1; then GZIP_CMD="pigz -p $(nproc)"; else GZIP_CMD="gzip"; fi
gz() { tar -C "$1" -cf - "${@:3}" | $GZIP_CMD > "$OUT/$2"; log "+ $2 ($(du -h "$OUT/$2" | cut -f1))"; }
pack() { gz "$@"; }

# shared offline assets
[ -d "$VENDOR_DIR/ow-assets" ] && pack "$VENDOR_DIR/ow-assets" ow-assets.tar.gz .

# runtimes (one archive per built target)
RUNTIMES="[]"
for rt in "$DIST_DIR"/runtime/*/; do
  t="$(basename "$rt")"
  [ -d "$rt/venv" ] || [ -d "$rt/python" ] || continue
  # only pack COMPLETE runtimes — a failed cross-install can leave a partial
  # python/ dir with no open_webui; never ship that.
  if ! ls "$rt"/{venv,python}/lib/python*/site-packages/open_webui/main.py \
        "$rt"/python/Lib/site-packages/open_webui/main.py >/dev/null 2>&1; then
    warn "skip incomplete runtime $t (open_webui missing)"; continue
  fi
  pack "$rt" "runtime-$t.tar.gz" .
  RUNTIMES="$(jq -c --arg t "$t" '. + [$t]' <<<"$RUNTIMES")"
done

# ollama: normalize each supported flavour (zst/tgz/zip) to a uniform .tar.gz
OLLAMAS="[]"
norm_ollama() {  # <downloaded-file> <flavour-key>
  local src="$1" key="$2" tmp; tmp="$(mktemp -d)"
  case "$src" in
    *.tar.zst) need zstd; zstd -dc "$src" | tar -x -C "$tmp" ;;
    *.tgz|*.tar.gz) tar -xzf "$src" -C "$tmp" ;;
    *.zip) need unzip; unzip -qo "$src" -d "$tmp" ;;
    *) rm -rf "$tmp"; return 1 ;;
  esac
  tar -C "$tmp" -cf - . | $GZIP_CMD > "$OUT/ollama-$key.tar.gz"
  rm -rf "$tmp"
  log "+ ollama-$key.tar.gz ($(du -h "$OUT/ollama-$key.tar.gz" | cut -f1))"
}
for key in linux-amd64 linux-arm64 linux-amd64-rocm darwin windows-amd64; do
  for ext in tar.zst tgz zip; do
    f="$OLLAMA_DIR/ollama-$key.$ext"
    [ -f "$f" ] || continue
    norm_ollama "$f" "$key" && OLLAMAS="$(jq -c --arg k "$key" '. + [$k]' <<<"$OLLAMAS")"
    break
  done
done

jq -n --arg otag "$OLLAMA_TAG" --argjson runtimes "$RUNTIMES" --argjson ollama "$OLLAMAS" \
  '{ollama_tag:$otag, ow_assets:"ow-assets.tar.gz", runtimes:$runtimes, ollama:$ollama,
    note:"loader picks runtime-<this bundles OS>.tar.gz + ollama by CPU arch (rocm if /dev/kfd)"}' \
  > "$OUT/manifest.json"

log "components -> $OUT ($(du -sh "$OUT" | cut -f1))"
