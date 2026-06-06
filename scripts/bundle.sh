#!/usr/bin/env bash
# Assemble Electron resources for a target and package a single-file artifact.
#
# Usage: scripts/bundle.sh [target] [--flavour <name>]
#   target  : linux-x64 | win-x64 | mac-arm64 | mac-x64  (default: host)
#   flavour : override the ollama asset (default: plain CPU/Metal build)
#
# All targets build from a NixOS/Linux host:
#   linux -> electron-builder AppImage
#   win   -> electron-builder nsis+portable (rcedit via wine), optional
#            osslsigncode Authenticode signing if WIN_PFX is set
#   mac   -> @electron/packager assembles the .app (cross from Linux), signed with
#            rcodesign (ad-hoc, or a real identity via MAC_P12), zipped (dmg needs macOS)
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

TARGET=""; FLAVOUR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --flavour) FLAVOUR="$2"; shift 2 ;;
    -*) die "unknown flag: $1" ;;
    *) TARGET="$1"; shift ;;
  esac
done
TARGET="${TARGET:-$(host_target)}"
VERSION="$(jq -r '.version' "$REPO_ROOT/app/package.json")"

# Serialize: all targets share app/.stage, so concurrent bundles would race.
exec 9>"$REPO_ROOT/app/.stage.lock"
flock 9 || die "could not acquire bundle lock"

OLLAMA_TAG="$(ollama_version)"
OLLAMA_DIR="$VENDOR_DIR/ollama/$OLLAMA_TAG"
RUNTIME="$DIST_DIR/runtime/$TARGET"
ASSETS="$VENDOR_DIR/ow-assets"
APP="$REPO_ROOT/app"
STAGE="$APP/.stage/resources"
OUT="$DIST_DIR/bundle"

[ -d "$OLLAMA_DIR" ] || die "ollama assets missing — run scripts/download-ollama.sh"
[ -d "$RUNTIME/venv" ] || [ -d "$RUNTIME/python" ] || die "runtime missing — run scripts/make-runtime.sh $TARGET"
[ -d "$ASSETS" ] || warn "offline assets missing ($ASSETS) — WebUI RAG will need network"

default_flavour() {
  case "$TARGET" in
    linux-x64) echo "ollama-linux-amd64.tar.zst" ;;
    mac-arm64|mac-x64) echo "ollama-darwin.tgz" ;;
    win-x64) echo "ollama-windows-amd64.zip" ;;
    *) die "no default ollama flavour for $TARGET" ;;
  esac
}
ASSET="${FLAVOUR:-$(default_flavour)}"
SRC="$OLLAMA_DIR/$ASSET"
[ -f "$SRC" ] || die "ollama asset not found: $SRC (download-ollama.sh ${ASSET})"

# --- stage resources --------------------------------------------------------
log "staging resources for $TARGET (ollama: $ASSET)"
rm -rf "$APP/.stage"; mkdir -p "$STAGE/ollama" "$STAGE/runtime"
case "$ASSET" in
  *.tar.zst) need zstd; need tar; zstd -dc "$SRC" | tar -x -C "$STAGE/ollama" ;;
  *.tgz)     need tar; tar -xzf "$SRC" -C "$STAGE/ollama" ;;
  *.zip)     need unzip; unzip -q "$SRC" -d "$STAGE/ollama" ;;
  *) die "don't know how to extract $ASSET" ;;
esac
[ -d "$RUNTIME/venv" ]   && cp -a "$RUNTIME/venv"   "$STAGE/runtime/venv"
[ -d "$RUNTIME/python" ] && cp -a "$RUNTIME/python" "$STAGE/runtime/python"
[ -d "$ASSETS" ] && cp -a "$ASSETS" "$STAGE/ow-assets"

# Ensure the pinned app deps are installed — otherwise `npx` would fetch a
# different (latest) electron-builder, which rejects our electron version.
if [ ! -x "$APP/node_modules/.bin/electron-builder" ]; then
  log "installing app deps (npm ci)"
  ( cd "$APP" && npm ci )
fi

log "building tailwind css"
( cd "$APP" && npm run css )
mkdir -p "$OUT"

# ---------------------------------------------------------------------------
# electron-builder path (linux AppImage / windows nsis+portable)
package_electron_builder() {
  local EB_OS; case "$TARGET" in linux-*) EB_OS="--linux";; win-*) EB_OS="--win";; esac

  # NixOS: repoint electron-builder's generic build helpers at the nix loader
  # (never the embedded AppImage runtime — that stays generic for real Linux).
  patch_eb_build_tools() {
    [ -n "${NIX_LD:-}" ] && command -v patchelf >/dev/null 2>&1 || return 0
    local cache="${XDG_CACHE_HOME:-$HOME/.cache}/electron-builder" f
    # generic prebuilt helpers electron-builder runs at pack time: AppImage tools
    # + NSIS (makensis) for windows portable/installer targets.
    for f in $(find "$cache" -type f \( -name mksquashfs -o -name appimagetool \
        -o -name desktop-file-validate -o -name makensis \) 2>/dev/null); do
      patchelf --set-interpreter "$NIX_LD" "$f" 2>/dev/null || true
      [ -n "${NIX_LD_LIBRARY_PATH:-}" ] && patchelf --set-rpath "$NIX_LD_LIBRARY_PATH" "$f" 2>/dev/null || true
    done
  }
  build_once() { ( cd "$APP" && npx --no-install electron-builder $EB_OS --config electron-builder.yml ); }
  log "electron-builder $EB_OS -> $OUT"
  patch_eb_build_tools          # pre-patch any cached helpers (NixOS)
  if ! build_once; then
    warn "package failed; patching freshly-downloaded helpers and retrying"
    patch_eb_build_tools
    build_once
  fi

  if [ "${EB_OS}" = "--win" ]; then sign_windows; fi
  rm -rf "$APP/.stage"
  log "bundle done — see $OUT/"
}

# Optional Authenticode signing of the produced .exe via osslsigncode (NixOS-native).
sign_windows() {
  [ -n "${WIN_PFX:-}" ] || { warn "WIN_PFX unset — leaving windows artifacts unsigned"; return 0; }
  need osslsigncode
  local exe
  for exe in "$OUT"/*.exe; do
    [ -f "$exe" ] || continue
    log "osslsigncode sign $(basename "$exe")"
    osslsigncode sign -pkcs12 "$WIN_PFX" -pass "${WIN_PFX_PASS:-}" \
      -n "plan.ai" -i "https://plan.ai" -t http://timestamp.digicert.com \
      -in "$exe" -out "$exe.signed" && mv "$exe.signed" "$exe"
  done
}

# ---------------------------------------------------------------------------
# macOS path: @electron/packager (cross from Linux) + rcodesign + zip
package_mac() {
  need rcodesign
  local ARCH; case "$TARGET" in mac-arm64) ARCH=arm64;; mac-x64) ARCH=x64;; esac
  local APPROOT="$OUT/mac-$ARCH"
  rm -rf "$APPROOT"; mkdir -p "$APPROOT"

  log "@electron/packager mac/$ARCH"
  ( cd "$APP" && npx --no-install @electron/packager . "plan.ai" \
      --platform=darwin --arch="$ARCH" \
      --out="$APPROOT" --overwrite \
      --app-bundle-id=ai.plan.usb \
      --extra-resource=".stage/resources/ollama" \
      --extra-resource=".stage/resources/runtime" \
      $([ -d "$APP/.stage/resources/ow-assets" ] && echo --extra-resource=".stage/resources/ow-assets") )

  local APPDIR; APPDIR="$(ls -d "$APPROOT"/plan.ai-darwin-*/plan.ai.app 2>/dev/null | head -1)"
  [ -d "$APPDIR" ] || die "packager did not produce a .app"

  log "rcodesign sign $(basename "$APPDIR")"
  if [ -n "${MAC_P12:-}" ]; then
    rcodesign sign --p12-file "$MAC_P12" --p12-password "${MAC_P12_PASS:-}" \
      --code-signature-flags runtime "$APPDIR"
  else
    rcodesign sign "$APPDIR"   # ad-hoc signature (no Apple identity)
    warn "MAC_P12 unset — produced an AD-HOC signature (not notarizable)"
  fi
  # verify the main Mach-O (rcodesign verify operates on Mach-O, not bundles)
  rcodesign verify "$APPDIR/Contents/MacOS/plan.ai" 2>&1 | tail -2 || true

  local ZIP="$OUT/plan-ai-$VERSION-$TARGET.zip"
  ( cd "$(dirname "$APPDIR")" && zip -qry "$ZIP" "$(basename "$APPDIR")" )
  rm -rf "$APP/.stage"
  log "mac bundle -> $ZIP (dmg requires macOS; ship the signed .app zip)"
}

# --- dispatch (functions are now defined) ----------------------------------
case "$TARGET" in
  linux-*|win-*) package_electron_builder ;;
  mac-*)         package_mac ;;
  *) die "unsupported target $TARGET" ;;
esac
