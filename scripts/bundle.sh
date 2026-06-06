#!/usr/bin/env bash
# Assemble Electron resources for a target and package a single-file artifact
# with electron-builder.
#
# Usage: scripts/bundle.sh [target] [--flavour <name>]
#   target  : linux-x64 | win-x64 | mac-arm64 | mac-x64  (default: host)
#   flavour : override the ollama asset (default: plain CPU/Metal build)
#
# Must run on the matching OS (the python runtime is host-built; electron-builder
# packages for the host OS). Cross-OS legs run on their own CI runner.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

host_target() {
  local os arch
  case "$(uname -s)" in
    Linux) os=linux ;; Darwin) os=mac ;; MINGW*|MSYS*|CYGWIN*) os=win ;;
    *) die "unsupported host OS" ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch=x64 ;; arm64|aarch64) arch=arm64 ;;
    *) die "unsupported host arch" ;;
  esac
  echo "$os-$arch"
}

TARGET=""
FLAVOUR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --flavour) FLAVOUR="$2"; shift 2 ;;
    -*) die "unknown flag: $1" ;;
    *) TARGET="$1"; shift ;;
  esac
done
TARGET="${TARGET:-$(host_target)}"
HOST="$(host_target)"
[ "$TARGET" = "$HOST" ] || die "target '$TARGET' != host '$HOST' — package on a '$TARGET' runner"

OLLAMA_TAG="$(ollama_version)"
OLLAMA_DIR="$VENDOR_DIR/ollama/$OLLAMA_TAG"
RUNTIME="$DIST_DIR/runtime/$TARGET"
ASSETS="$VENDOR_DIR/ow-assets"
APP="$REPO_ROOT/app"
STAGE="$APP/.stage/resources"

[ -d "$OLLAMA_DIR" ]        || die "ollama assets missing — run scripts/download-ollama.sh"
[ -d "$RUNTIME/venv" ]      || die "python runtime missing — run scripts/make-runtime.sh $TARGET"
[ -d "$ASSETS" ]           || warn "offline assets missing ($ASSETS) — WebUI RAG will need network"

# --- pick the ollama flavour for this target -------------------------------
default_flavour() {
  case "$TARGET" in
    linux-x64) echo "ollama-linux-amd64.tar.zst" ;;
    mac-arm64|mac-x64) echo "ollama-darwin.tgz" ;;   # universal darwin build
    win-x64) echo "ollama-windows-amd64.zip" ;;
    *) die "no default ollama flavour for $TARGET" ;;
  esac
}
ASSET="${FLAVOUR:-$(default_flavour)}"
SRC="$OLLAMA_DIR/$ASSET"
[ -f "$SRC" ] || die "ollama asset not found: $SRC"

# --- stage resources --------------------------------------------------------
log "staging resources for $TARGET (ollama: $ASSET)"
rm -rf "$APP/.stage"; mkdir -p "$STAGE/ollama" "$STAGE/runtime"

# ollama: extract the archive, preserving the bin/ + lib/ layout it ships.
case "$ASSET" in
  *.tar.zst) need zstd; need tar; zstd -dc "$SRC" | tar -x -C "$STAGE/ollama" ;;
  *.tgz)     need tar; tar -xzf "$SRC" -C "$STAGE/ollama" ;;
  *.zip)     need unzip; unzip -q "$SRC" -d "$STAGE/ollama" ;;
  *) die "don't know how to extract $ASSET" ;;
esac

# python runtime (relocatable venv + standalone interpreter) + offline assets
cp -a "$RUNTIME/venv" "$STAGE/runtime/venv"
[ -d "$RUNTIME/python" ] && cp -a "$RUNTIME/python" "$STAGE/runtime/python"
[ -d "$ASSETS" ] && cp -a "$ASSETS" "$STAGE/ow-assets"

# --- build css + package ----------------------------------------------------
log "building tailwind css"
( cd "$APP" && npm run css )

log "electron-builder package -> $DIST_DIR/bundle"
EB_OS=""; case "$TARGET" in linux-*) EB_OS="--linux";; win-*) EB_OS="--win";; mac-*) EB_OS="--mac";; esac

# On NixOS the bundled AppImage build tools (mksquashfs/appimagetool) are generic
# ELF binaries that the bare nix-ld stub can't run. Repoint ONLY those build-time
# tools at the nix loader — never the embedded `runtime`, which must stay generic
# so the produced AppImage runs on ordinary Linux machines.
patch_eb_build_tools() {
  [ -n "${NIX_LD:-}" ] && command -v patchelf >/dev/null 2>&1 || return 0
  local cache="${XDG_CACHE_HOME:-$HOME/.cache}/electron-builder/appimage"
  [ -d "$cache" ] || return 0
  local f
  for f in $(find "$cache" -type f \( -name mksquashfs -o -name appimagetool -o -name desktop-file-validate \) 2>/dev/null); do
    patchelf --set-interpreter "$NIX_LD" "$f" 2>/dev/null || true
    [ -n "${NIX_LD_LIBRARY_PATH:-}" ] && patchelf --set-rpath "$NIX_LD_LIBRARY_PATH" "$f" 2>/dev/null || true
  done
}

build_once() { ( cd "$APP" && npx electron-builder $EB_OS --config electron-builder.yml ); }
if ! build_once; then
  warn "package failed (NixOS stub-ld?); patching build helpers and retrying"
  patch_eb_build_tools
  build_once
fi

rm -rf "$APP/.stage"
log "bundle done — see $DIST_DIR/bundle/"
