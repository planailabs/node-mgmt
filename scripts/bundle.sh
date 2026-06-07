#!/usr/bin/env bash
# Package a single artifact per target. Every artifact ships compressed
# component archives (dist/components/) for its OS; the in-app loader
# (main/loader.js) extracts only what the machine needs on first launch (its
# runtime + the ollama flavour matching the CPU arch). Build components first
# with scripts/build-components.sh (or `make components`).
#
# All targets build from a NixOS/Linux host:
#   linux  -> electron-builder AppImage
#   win    -> electron-builder zip            (osslsigncode signing if WIN_PFX set)
#   mac    -> @electron/packager + rcodesign  (zipped .app; dmg needs macOS)
#   nixos  -> runnable dir (+ tarball) launched via the nixpkgs Electron
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

host_target() {
  local os arch
  case "$(uname -s)" in
    Linux) os=linux ;; Darwin) os=mac ;; MINGW*|MSYS*|CYGWIN*) os=win ;;
    *) die "unsupported host OS" ;;
  esac
  case "$(uname -m)" in x86_64|amd64) arch=x64 ;; arm64|aarch64) arch=arm64 ;; *) die "arch?" ;; esac
  echo "$os-$arch"
}

TARGET="${1:-$(host_target)}"
VERSION="$(jq -r '.version' "$REPO_ROOT/app/package.json")"
APP="$REPO_ROOT/app"
COMP_SRC="$DIST_DIR/components"
OUT="$DIST_DIR/bundle"

# runtime key + the ollama flavours this OS ships (loader picks one by arch/GPU)
# ollama flavours this OS ships (loader picks one). Default is the single CPU
# build so artifacts stay under FAT32's 4 GiB limit (no exFAT/splitting needed);
# override for a GPU image, e.g. OLLAMA_FLAVOURS="linux-amd64 linux-amd64-rocm".
case "$TARGET" in
  linux-x64|nixos-x64) OKEYS="${OLLAMA_FLAVOURS:-linux-amd64}" ;;
  win-x64)   OKEYS="${OLLAMA_FLAVOURS:-windows-amd64}" ;;
  mac-arm64|mac-x64) OKEYS="${OLLAMA_FLAVOURS:-darwin}" ;;
  *) die "unsupported target $TARGET" ;;
esac
# Component FORMAT this OS's loader consumes (it picks this from the shared pool):
#   linux/nixos = squashfs (mount via squashfuse / extract via unsquashfs)
#   windows     = tar.gz   ;   macOS = dmg (hdiutil mount, verified on real macOS)
case "$TARGET" in
  linux-*|nixos-*) FMTS="squashfs"; MOUNTABLE=1 ;;
  win-*)           FMTS="tar.gz";   MOUNTABLE=0 ;;
  mac-*)           FMTS="dmg";      MOUNTABLE=0 ;;
  *)               FMTS="tar.gz";   MOUNTABLE=0 ;;
esac
# component base names this launcher needs (loader picks the ollama flavour)
COMP_BASES="runtime-$TARGET ow-assets"; for k in $OKEYS; do COMP_BASES="$COMP_BASES ollama-$k"; done
[ -f "$COMP_SRC/runtime-$TARGET.${FMTS%% *}" ] || die "missing runtime component runtime-$TARGET.${FMTS%% *} in $COMP_SRC — run: make runtime TARGET=$TARGET && scripts/build-components.sh"

# Components ship OUTSIDE the launcher as a SHARED pool beside it (not embedded),
# so each launcher stays small and all platforms share one copy on the USB. The
# loader (main/loader.js) finds components/ + tools/ next to the AppImage/exe/.app
# (or via PLANAI_COMPONENTS). Copy this target's format(s) + static mount tools in.
copy_comps_into() {  # <components-dir> <tools-parent-dir>
  local cdst="$1" tdst="$2" base ext; mkdir -p "$cdst"
  for base in $COMP_BASES; do for ext in $FMTS; do
    [ -f "$COMP_SRC/$base.$ext" ] && cp -u "$COMP_SRC/$base.$ext" "$cdst/" || true
  done; done
  [ -f "$COMP_SRC/manifest.json" ] && cp -u "$COMP_SRC/manifest.json" "$cdst/"
  if [ "$MOUNTABLE" = 1 ]; then
    local T; T="$(cd "$REPO_ROOT" && nix build .#linuxMountTools --no-link --print-out-paths 2>/dev/null || true)"
    if [ -n "$T" ] && [ -d "$T/bin" ]; then mkdir -p "$tdst/bin"; cp -L "$T/bin/"* "$tdst/bin/"; chmod -R u+w "$tdst"
    else warn "linuxMountTools unavailable — loader will extract"; fi
  fi
  log "components -> $cdst ($(du -sh "$cdst" | cut -f1))"
}

exec 9>"$APP/.stage.lock"; flock 9 || die "could not acquire bundle lock"
[ -x "$APP/node_modules/.bin/electron-builder" ] || ( cd "$APP" && npm ci )
( cd "$APP" && npm run css )
mkdir -p "$OUT"

# ---------------------------------------------------------------------------
package_electron_builder() {  # linux AppImage / windows zip
  local EB_OS; case "$TARGET" in linux-*) EB_OS="--linux";; win-*) EB_OS="--win";; esac
  patch_eb_build_tools() {
    [ -n "${NIX_LD:-}" ] && command -v patchelf >/dev/null 2>&1 || return 0
    local cache="${XDG_CACHE_HOME:-$HOME/.cache}/electron-builder" f
    for f in $(find "$cache" -type f \( -name mksquashfs -o -name appimagetool \
        -o -name desktop-file-validate -o -name makensis \) 2>/dev/null); do
      patchelf --set-interpreter "$NIX_LD" "$f" 2>/dev/null || true
      [ -n "${NIX_LD_LIBRARY_PATH:-}" ] && patchelf --set-rpath "$NIX_LD_LIBRARY_PATH" "$f" 2>/dev/null || true
    done
  }
  build_once() { ( cd "$APP" && npx --no-install electron-builder $EB_OS --config electron-builder.yml ); }
  log "electron-builder $EB_OS -> $OUT"
  patch_eb_build_tools
  build_once || { warn "package failed; patching helpers + retrying"; patch_eb_build_tools; build_once; }
  [ "$EB_OS" = "--win" ] && sign_windows || true
  copy_comps_into "$OUT/components" "$OUT/tools"   # shared pool beside the launcher
  log "bundle done -> $OUT/ (bare launcher + shared components/)"
}

sign_windows() {  # optional Authenticode signing via osslsigncode
  [ -n "${WIN_PFX:-}" ] || { warn "WIN_PFX unset — windows artifacts unsigned"; return 0; }
  need osslsigncode
  for exe in "$OUT"/*.exe; do [ -f "$exe" ] || continue
    log "osslsigncode sign $(basename "$exe")"
    osslsigncode sign -pkcs12 "$WIN_PFX" -pass "${WIN_PFX_PASS:-}" -n plan.ai -i https://plan.ai \
      -t http://timestamp.digicert.com -in "$exe" -out "$exe.s" && mv "$exe.s" "$exe"
  done
}

package_mac() {  # @electron/packager (cross) + rcodesign
  need rcodesign
  local ARCH; case "$TARGET" in mac-arm64) ARCH=arm64;; mac-x64) ARCH=x64;; esac
  local APPROOT="$OUT/mac-$ARCH"; rm -rf "$APPROOT"; mkdir -p "$APPROOT"
  log "@electron/packager mac/$ARCH"
  # bare .app — components ship OUTSIDE it (shared pool beside the .app). --ignore
  # keeps the (now absent) .stage and any local build cruft out of the bundle.
  ( cd "$APP" && npx --no-install @electron/packager . "plan.ai" --platform=darwin --arch="$ARCH" \
      --out="$APPROOT" --overwrite --app-bundle-id=ai.plan.usb --ignore="(^/\.stage)" )
  local APPDIR; APPDIR="$(ls -d "$APPROOT"/plan.ai-darwin-*/plan.ai.app 2>/dev/null | head -1)"
  [ -d "$APPDIR" ] || die "packager produced no .app"
  log "rcodesign sign"
  if [ -n "${MAC_P12:-}" ]; then
    rcodesign sign --p12-file "$MAC_P12" --p12-password "${MAC_P12_PASS:-}" --code-signature-flags runtime "$APPDIR"
  else rcodesign sign "$APPDIR"; warn "MAC_P12 unset — ad-hoc signature (not notarizable)"; fi
  rcodesign verify "$APPDIR/Contents/MacOS/plan.ai" 2>&1 | tail -1 || true
  copy_comps_into "$OUT/components" "$OUT/tools"   # shared dmg pool beside the .app
  local ZIP="$OUT/plan-ai-$VERSION-$TARGET.zip"
  ( cd "$(dirname "$APPDIR")" && zip -qry "$ZIP" "$(basename "$APPDIR")" )
  log "mac bundle -> $ZIP (bare .app) + shared $OUT/components/"
}

package_nixos() {  # runnable dir launched via nixpkgs electron (uses the node loader)
  [ -n "${ELECTRON_OVERRIDE_DIST_PATH:-}" ] || die "run in 'nix develop'"
  local ELECTRON_BIN="$ELECTRON_OVERRIDE_DIST_PATH/electron"
  [ -x "$ELECTRON_BIN" ] || die "nix electron not at $ELECTRON_BIN"
  need patchelf; local PATCHELF_DIR; PATCHELF_DIR="$(dirname "$(command -v patchelf)")"
  local DIR="$OUT/plan-ai-nixos-x64"; rm -rf "$DIR"; mkdir -p "$DIR/app" "$DIR/components"
  # app + its production node_modules (tar, for the loader)
  cp -a "$APP/main" "$APP/renderer" "$APP/package.json" "$DIR/app/"
  cp -a "$APP/node_modules" "$DIR/app/node_modules"
  # nixos stays self-contained (a dir bundle): its squashfs components + static
  # unsquashfs live inside the dir (the loader force-extracts on NixOS; no FUSE).
  copy_comps_into "$DIR/components" "$DIR/tools"
  mkdir -p "$DIR/models" "$DIR/data"
  cat > "$DIR/plan-ai" <<EOF
#!/bin/sh
# plan.ai NixOS launcher: nixpkgs Electron + the in-app component loader, which
# extracts the right ollama flavour + the nix runtime and patchelf's ollama to
# the nix loader (PLANAI_NIX_LD). Requires a nix store (paths baked below).
here=\$(CDPATH= cd -- "\$(dirname -- "\$0")" && pwd)
export PLANAI_COMPONENTS="\$here/components"
export PLANAI_MOUNT_TOOLS="\$here/tools/bin"   # static unsquashfs for the loader
export PLANAI_PORTABLE_ROOT="\${PLANAI_PORTABLE_ROOT:-\$here}"
export PLANAI_CHILD_LD_LIBRARY_PATH="${NIX_LD_LIBRARY_PATH}"
export PLANAI_NIX_LD="${NIX_LD}"
export PLANAI_NIX_LD_LIBRARY_PATH="${NIX_LD_LIBRARY_PATH}"
export PATH="${PATCHELF_DIR}:\$PATH"   # patchelf for the loader's ollama fixup
exec "${ELECTRON_BIN}" "\$here/app" --no-sandbox "\$@"
EOF
  chmod +x "$DIR/plan-ai"
  need zstd; need tar
  ( cd "$OUT" && tar -cf - plan-ai-nixos-x64 | zstd -q -19 -T0 -o "plan-ai-$VERSION-nixos-x64.tar.zst" -f )
  log "nixos bundle -> $DIR (./plan-ai) + $OUT/plan-ai-$VERSION-nixos-x64.tar.zst"
}

case "$TARGET" in
  nixos-*)       package_nixos ;;
  linux-*|win-*) package_electron_builder ;;
  mac-*)         package_mac ;;
esac
