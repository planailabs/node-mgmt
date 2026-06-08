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
#   macOS = dmg (hdiutil mount)  ;  windows = dir (pre-extracted, used in place)
case "$TARGET" in
  linux-*|nixos-*) FMTS="squashfs"; ;;
  win-*)           FMTS="dir";      ;;
  mac-*)           FMTS="dmg";      ;;
  *)               FMTS="tar.gz";   ;;
esac
# component base names this launcher needs (loader picks the ollama flavour)
COMP_BASES="runtime-$TARGET ow-assets"; for k in $OKEYS; do COMP_BASES="$COMP_BASES ollama-$k"; done
# a component exists in this OS's format as a FILE (base.ext) or a DIR (windows)
comp_present() { local base="$1" e; for e in $FMTS; do
  case "$e" in dir) [ -d "$COMP_SRC/$base" ] && return 0 ;; *) [ -f "$COMP_SRC/$base.$e" ] && return 0 ;; esac
done; return 1; }
comp_present "runtime-$TARGET" || die "missing runtime component for $TARGET ($FMTS) in $COMP_SRC — run: make runtime TARGET=$TARGET && scripts/build-components.sh"

# Components ship OUTSIDE the launcher as a SHARED pool beside it (not embedded),
# so each launcher stays small and all platforms share one copy on the USB. The
# rust launcher finds components/ next to the AppImage/exe/.app (or via
# PLANAI_COMPONENTS). The squashfs mount tools (squashfuse_ll/unsquashfs) are
# EMBEDDED in the launcher binary (build.rs), so no external tools/ dir is shipped.
copy_comps_into() {  # <components-dir>
  local cdst="$1" base ext; mkdir -p "$cdst"
  for base in $COMP_BASES; do for ext in $FMTS; do
    case "$ext" in
      dir) [ -d "$COMP_SRC/$base" ] && { rm -rf "$cdst/$base"; cp -a "$COMP_SRC/$base" "$cdst/"; } || true ;;
      *)   [ -f "$COMP_SRC/$base.$ext" ] && cp -u "$COMP_SRC/$base.$ext" "$cdst/" || true ;;
    esac
  done; done
  [ -f "$COMP_SRC/manifest.json" ] && cp -u "$COMP_SRC/manifest.json" "$cdst/"
  copy_llmfit_into "$cdst"
  log "components -> $cdst ($(du -sh "$cdst" | cut -f1))"
}

exec 9>"$APP/.stage.lock"; flock 9 || die "could not acquire bundle lock"
[ -x "$APP/node_modules/.bin/electron-builder" ] || ( cd "$APP" && npm ci )
mkdir -p "$OUT"

# ---------------------------------------------------------------------------
# Cross-built RUST launcher (rust prepares the runtime, then launches Electron),
# shipped BESIDE the Electron app. Running it mounts/links the shared components/
# pool and sets PLANAI_RESOURCES before exec'ing the bare app — the same job the
# in-app node loader does, but as a native entry point (parity with the linux
# AppImage, whose AppRun already IS this launcher). Prints the built binary path.
nix_launcher() {  # <flake-attr> <binary-name>
  local attr="$1" bin="$2" out
  out="$(cd "$REPO_ROOT" && nix build ".#$attr" --no-link --print-out-paths)" \
    || die "launcher build failed: .#$attr"
  [ -f "$out/$bin" ] || die "launcher $bin missing in $out"
  echo "$out/$bin"
}

# Copy the bundled llmfit binary for THIS target into the shared pool, with an
# OS-distinct name (one pool holds every platform's copy). The launcher runs it
# (`llmfit system --json` for GPU detect + `llmfit serve` for the model browser).
copy_llmfit_into() {  # <pool-dir>
  local pool="$1" attr name out bin
  case "$TARGET" in
    linux-*|nixos-*) attr=llmfit-linux-x64; name=llmfit-linux ;;
    win-*)           attr=llmfit-win-x64;   name=llmfit-windows.exe ;;
    mac-*)           attr=llmfit-mac-arm64; name=llmfit-darwin ;;
    *) return 0 ;;
  esac
  out="$(cd "$REPO_ROOT" && nix build ".#$attr" --no-link --print-out-paths 2>/dev/null || true)"
  [ -n "$out" ] || { warn "llmfit build failed for $TARGET — model browser disabled"; return 0; }
  bin="$(find "$out" -maxdepth 1 -type f | head -1)"
  [ -n "$bin" ] || { warn "no llmfit binary in $out"; return 0; }
  cp -f "$bin" "$pool/$name"; chmod +x "$pool/$name" 2>/dev/null || true
  log "llmfit -> components/$name ($(du -h "$pool/$name" | cut -f1))"
}

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
  copy_comps_into "$OUT/components"   # shared pool beside the launcher
  if [ "$EB_OS" = "--linux" ]; then
    local UNPACK="$OUT/linux-unpacked"; [ -d "$UNPACK" ] || die "no linux-unpacked from electron-builder"
    emit_app_component "$UNPACK"          # -> components/app-linux-x64.squashfs
    emit_nixos_fhs                        # -> components/nixos-fhs.{closure,path}
    # named .linux.exe (a distinct, explicit per-OS launcher name)
    local L; L="$(nix_launcher launcher-linux-x64 plan-ai)"
    cp -f "$L" "$OUT/plan-ai.linux.exe"; chmod +x "$OUT/plan-ai.linux.exe"
    log "linux standalone launcher -> $OUT/plan-ai.linux.exe (static musl)"
  else
    local UNPACK="$OUT/win-unpacked"; [ -d "$UNPACK" ] || die "no win-unpacked from electron-builder"
    emit_app_component "$UNPACK"          # -> components/app-win-x64/ (used in place)
    local L; L="$(nix_launcher launcher-win-x64 plan-ai.exe)"
    cp -f "$L" "$OUT/plan-ai.exe"
    sign_windows || true                  # signs the standalone launcher exe (if WIN_PFX set)
    log "win standalone launcher -> $OUT/plan-ai.exe"
  fi
  log "bundle done -> $OUT/ (standalone launcher + shared components/ incl. app-$TARGET)"
}

# Emit the Electron app itself as a component (app-$TARGET) in THIS OS's format,
# into the shipped pool — the launcher mounts/links it and runs Electron from it
# (parity with runtime/ollama/ow-assets). linux=squashfs, win=dir, mac=hfsplus dmg.
emit_app_component() {  # <src>  (electron unpacked dir; for mac a dir holding plan.ai.app)
  local src="$1" name="app-$TARGET" cdst="$OUT/components"; mkdir -p "$cdst"
  case "$FMTS" in
    squashfs) need mksquashfs; rm -f "$cdst/$name.squashfs"
      mksquashfs "$src" "$cdst/$name.squashfs" -comp zstd -processors "$(nproc)" -all-root -no-xattrs -noappend -quiet
      log "app component -> $name.squashfs ($(du -h "$cdst/$name.squashfs" | cut -f1))" ;;
    dir) rm -rf "$cdst/$name"; mkdir -p "$cdst/$name"; cp -a "$src/." "$cdst/$name/"
      log "app component -> $name/ ($(du -sh "$cdst/$name" | cut -f1))" ;;
    dmg) emit_hfsplus_dmg "$src" "$cdst/$name.dmg" ;;
    *) die "no app-component format for FMTS=$FMTS" ;;
  esac
}

# raw HFS+ image macOS mounts via hdiutil. Needs mkfs.hfsplus (hfsprogs) + sudo
# loop mount (the dev box has both — same path build-components.sh uses for dmgs).
emit_hfsplus_dmg() {  # <src-dir> <out.dmg>
  need mkfs.hfsplus
  local src="$1" img="$2" mnt sz raw
  raw="$(mktemp -u).rawhfs"
  sz=$(du -sb "$src" | cut -f1); sz=$(( sz * 11 / 10 + 64*1024*1024 ))   # +10% +64MB overhead
  truncate -s "$sz" "$raw"
  mkfs.hfsplus -v PlanAI "$raw" >/dev/null 2>&1 || die "mkfs.hfsplus failed: $raw"
  mnt="$(mktemp -d)"
  sudo mount -o loop,umask=0000 "$raw" "$mnt" || die "loop-mount failed (sudo?) for $raw"
  sudo cp -a "$src/." "$mnt/"; sync; sudo umount "$mnt"; rmdir "$mnt" 2>/dev/null || true
  # Compress the bare HFS+ image into a proper UDIF dmg (Finder-mountable, ~3x
  # smaller) with libdmg-hfsplus (no macOS/hdiutil needed). The launcher mounts
  # UDIF via a normal `hdiutil attach`; falls back to bare HFS+ if the tool fails.
  rm -f "$img"
  local DMGTOOL; DMGTOOL="$(cd "$REPO_ROOT" && nix build .#libdmg-hfsplus --no-link --print-out-paths 2>/dev/null)/bin/dmg"
  if [ -x "$DMGTOOL" ] && "$DMGTOOL" dmg "$raw" "$img" >/dev/null 2>&1; then
    rm -f "$raw"
    log "app component -> $(basename "$img") ($(du -h "$img" | cut -f1)) [UDIF compressed]"
  else
    warn "libdmg-hfsplus unavailable — shipping uncompressed bare HFS+ dmg"
    mv "$raw" "$img"
    log "app component -> $(basename "$img") ($(du -h "$img" | cut -f1)) [bare hfsplus]"
  fi
}

# Ship the NixOS FHS helper closure (NAR) into the pool. On NixOS the static-musl
# launcher imports it + re-execs inside the sandbox so the generic glibc Electron/
# ollama run (NixOS's bare nix-ld stub can't run them directly).
emit_nixos_fhs() {
  local cdst="$OUT/components" fhs
  command -v nix-store >/dev/null 2>&1 || { warn "no nix-store — skip NixOS FHS helper"; return 0; }
  fhs="$(cd "$REPO_ROOT" && nix build .#nixosFhs --no-link --print-out-paths 2>/dev/null || true)"
  [ -n "$fhs" ] || { warn "nixosFhs build failed — skip FHS helper"; return 0; }
  log "exporting NixOS FHS closure (NAR) -> components/ (first-run import on NixOS)"
  nix-store --export $(nix-store -qR "$fhs") > "$cdst/nixos-fhs.closure"
  echo "$fhs/bin/planai-fhs" > "$cdst/nixos-fhs.path"
  log "  nixos-fhs.closure ($(du -h "$cdst/nixos-fhs.closure" | cut -f1)) + nixos-fhs.path"
}

# Wrap the mac launcher binary in a tiny .app so Finder double-click works. Its
# MacOS executable IS the rust launcher; it mounts app-mac-*.dmg from the pool and
# runs the real Electron app from it. Signed (ad-hoc unless MAC_P12 is set).
build_mac_launcher_app() {
  local L; L="$(nix_launcher launcher-mac-arm64 plan-ai)"
  local LAPP="$OUT/plan.ai.app"; rm -rf "$LAPP"
  mkdir -p "$LAPP/Contents/MacOS" "$LAPP/Contents/Resources"
  # nix store binaries are read-only; rcodesign signs in place → needs u+w.
  cp -f "$L" "$LAPP/Contents/MacOS/plan-ai"; chmod 0755 "$LAPP/Contents/MacOS/plan-ai"
  cat > "$LAPP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>plan.ai</string>
  <key>CFBundleDisplayName</key><string>plan.ai</string>
  <key>CFBundleIdentifier</key><string>ai.plan.usb.launcher</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>plan-ai</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
EOF
  printf 'APPL????' > "$LAPP/Contents/PkgInfo"
  if [ -n "${MAC_P12:-}" ]; then
    rcodesign sign --p12-file "$MAC_P12" --p12-password "${MAC_P12_PASS:-}" --code-signature-flags runtime "$LAPP"
  else rcodesign sign "$LAPP"; warn "MAC_P12 unset — launcher .app ad-hoc signed"; fi
  rcodesign verify "$LAPP/Contents/MacOS/plan-ai" 2>&1 | tail -1 || true
  log "mac launcher .app -> $LAPP (double-clickable; runs the app-mac component)"
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
  copy_comps_into "$OUT/components"   # shared dmg pool beside the launcher
  # The Electron .app ships as the app-mac component (a dmg holding plan.ai.app);
  # the launcher mounts it and runs Electron from it.
  local STAGE; STAGE="$(mktemp -d)"; cp -a "$APPDIR" "$STAGE/plan.ai.app"
  emit_app_component "$STAGE"; rm -rf "$STAGE"
  # standalone, Finder-double-clickable launcher .app sitting beside the pool.
  build_mac_launcher_app
  log "mac bundle -> $OUT/plan.ai.app (launcher) + components/app-$TARGET.dmg + shared $OUT/components/"
}

# NixOS has no separate bundle: the linux-x64 artifact ships the FHS helper
# closure (emit_nixos_fhs) and the static-musl launcher FHS-reexecs on NixOS, so
# the regular linux build runs there too. `make dev` covers local NixOS dev.

case "$TARGET" in
  linux-*|win-*) package_electron_builder ;;
  mac-*)         package_mac ;;
  *)             die "unknown TARGET '$TARGET' (expected linux-x64 | win-x64 | mac-arm64)" ;;
esac
