#!/usr/bin/env bash
# Package a single artifact per target. Every artifact ships compressed
# component archives (dist/components/) for its OS; the in-app loader
# (main/loader.js) extracts only what the machine needs on first launch (its
# runtime + the ollama flavour matching the CPU arch). Build components first
# with `make components` (ninja packs each one via scripts/pack-component.sh).
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

# Run each argument (a command string) as a parallel background job in a subshell,
# then die if any failed. Used to overlap the independent post-electron steps for a
# single target — the nix launcher build, the app-component pack, the NixOS FHS
# export, and the component copy (which itself builds llmfit via nix) don't depend
# on each other, so there's no reason to run them one after another.
run_jobs() {
  local pids=() fail=0 j pid
  for j in "$@"; do ( eval "$j" ) & pids+=("$!"); done
  for pid in "${pids[@]}"; do wait "$pid" || fail=1; done
  [ "$fail" -eq 0 ] || die "a parallel bundle step failed"
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
  linux-arm64) OKEYS="${OLLAMA_FLAVOURS:-linux-arm64}" ;;
  win-x64)   OKEYS="${OLLAMA_FLAVOURS:-windows-amd64}" ;;
  mac-arm64|mac-x64) OKEYS="${OLLAMA_FLAVOURS:-darwin}" ;;
  *) die "unsupported target $TARGET" ;;
esac
# The standalone rust launcher's nix attr for this target (its arch-matched build).
case "$TARGET" in
  linux-x64|nixos-x64) LAUNCHER_ATTR=launcher-linux-x64 ;;
  linux-arm64)         LAUNCHER_ATTR=launcher-linux-arm64 ;;
  win-x64)             LAUNCHER_ATTR=launcher-win-x64 ;;
  mac-arm64)           LAUNCHER_ATTR=launcher-mac-arm64 ;;
  *) die "no launcher attr for target $TARGET" ;;
esac
# Component FORMAT this OS's loader consumes (it picks this from its group dir):
#   linux/nixos = squashfs (mount via squashfuse / extract via unsquashfs)
#   macOS = dmg (hdiutil mount)  ;  windows = zip (one archive; the launcher unpacks
#   it on update, the burned image carries it already unpacked)
case "$TARGET" in
  linux-*|nixos-*) FMTS="squashfs"; ;;
  win-*)           FMTS="zip";      ;;
  mac-*)           FMTS="dmg";      ;;
  *)               FMTS="tar.gz";   ;;
esac
# This bundle's component group dir (components/<target>/), keyed by TARGET (os-arch)
# so two linux arches (linux-x64 + linux-arm64) get distinct groups instead of
# colliding in one `linux` bucket. Matches crates/manifest classify()/current_platform()
# + the launcher's POOL_TARGET + platforms.json. (nixos-x64/-arm64 fold to linux-<arch>.)
case "$TARGET" in nixos-x64) GROUP=linux-x64 ;; nixos-arm64) GROUP=linux-arm64 ;; *) GROUP="$TARGET" ;; esac
# The shipped standalone-launcher filename for this target. linux carries the arch
# (two arches coexist on one drive/update server); win/mac have a single arch each.
case "$TARGET" in
  linux-*|nixos-*) LAUNCHER_NAME="plan-ai.$GROUP.exe" ;;
  win-*)           LAUNCHER_NAME="plan-ai.exe" ;;
  mac-*)           LAUNCHER_NAME="plan-ai.dmg" ;;
  *) die "no launcher filename for target $TARGET" ;;
esac
# electron-builder OS + arch flags (linux/win) and the unpacked dir it emits.
# electron-builder arch-suffixes non-default arches: x64 -> <os>-unpacked, arm64 ->
# <os>-arm64-unpacked — so two linux arches land in DISTINCT dirs and never clash.
# (mac goes through @electron/packager in package_mac, so these stay empty.)
case "$TARGET" in
  linux-x64|nixos-x64) EB_OS=--linux; EB_ARCH=--x64;   UNPACK_DIR="linux-unpacked" ;;
  linux-arm64)         EB_OS=--linux; EB_ARCH=--arm64; UNPACK_DIR="linux-arm64-unpacked" ;;
  win-x64)             EB_OS=--win;   EB_ARCH=--x64;   UNPACK_DIR="win-unpacked" ;;
  mac-*)               EB_OS="";      EB_ARCH="";      UNPACK_DIR="" ;;
  *) die "no electron metadata for target $TARGET" ;;
esac
# component base names this launcher needs (loader picks the ollama flavour)
COMP_BASES="runtime-$TARGET ow-assets"; for k in $OKEYS; do COMP_BASES="$COMP_BASES ollama-$k"; done
# usbd: the plan.ai USB daemon (control plane) ships on ALL platforms now — the
# launcher hands service ownership to it everywhere; its own supervisor remains a
# dead-code fallback only when the component is absent (plain dev builds). usbd is
# handled specially (copy_usbd_into below), NOT via COMP_BASES: the pool holds a
# per-TARGET file (usbd-<target>, so two linux arches don't collide), renamed to
# the fixed `usbd` name the launcher resolves in each OS group dir.
# a component exists in this OS's format as a FILE (base.ext) or a DIR (windows)
comp_present() { local base="$1" e; for e in $FMTS; do
  case "$e" in dir) [ -d "$COMP_SRC/$base" ] && return 0 ;; *) [ -f "$COMP_SRC/$base.$e" ] && return 0 ;; esac
done; return 1; }
comp_present "runtime-$TARGET" || die "missing runtime component for $TARGET ($FMTS) in $COMP_SRC — run: make components"

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
  copy_usbd_into "$cdst"
  # Per-OS manifest: each target owns its OWN group dir (components/<os>/), so there
  # are no cross-target races (no shared file) and the manifest is DECLARATIVE — it
  # lists exactly this OS's contents instead of a global scan. The launcher uses it
  # as the pool marker (selection is by dir scan), but it self-describes the group.
  write_os_manifest "$cdst"
  copy_llmfit_into "$cdst"
  log "components -> $cdst ($(du -sh "$cdst" | cut -f1))"
}

# Copy this target's usbd daemon (pool name usbd-<target>) into the group dir as
# the fixed `usbd` name the launcher resolves (usbd.squashfs / usbd.dmg / usbd/).
# Best-effort: absent in plain dev builds, where the launcher uses its supervisor.
copy_usbd_into() {  # <components-dir>
  local cdst="$1" ext
  for ext in $FMTS; do
    case "$ext" in
      dir) [ -d "$COMP_SRC/usbd-$TARGET" ] && { rm -rf "$cdst/usbd"; cp -a "$COMP_SRC/usbd-$TARGET" "$cdst/usbd"; } || true ;;
      *)   [ -f "$COMP_SRC/usbd-$TARGET.$ext" ] && cp -u "$COMP_SRC/usbd-$TARGET.$ext" "$cdst/usbd.$ext" || true ;;
    esac
  done
}

# Write the declarative manifest.json for this OS's component group (components/<os>/).
write_os_manifest() {  # <group-dir>
  local cdst="$1" oll="[]" k
  for k in $OKEYS; do oll="$(printf '%s' "$oll" | jq -c --arg k "$k" '. + [$k]')"; done
  jq -n --arg os "$GROUP" --arg tag "$(ollama_version)" --arg rt "runtime-$TARGET" \
        --arg app "app-$TARGET" --argjson ollama "$oll" \
    '{os:$os, ollama_tag:$tag, runtime:$rt, app:$app, ow_assets:"ow-assets", ollama:$ollama,
      note:"per-OS component group; loader mounts runtime/app/ow-assets + the ollama flavour matching the CPU arch (rocm if /dev/kfd)"}' \
    > "$cdst/manifest.json"
}

# ninja launches bundle-linux/win/mac concurrently. The ONLY part of a bundle that
# two targets can't run at once is the Electron packaging step: it reads/writes the
# shared app/ dir (node_modules) and electron-builder's global ~/.cache. So serialize
# JUST that with a lock — everything after it (component copy, dmg packing, per-OS
# launcher nix builds) writes per-target / distinctly-named outputs and runs fully in
# parallel across the three targets. (Releasing on success lets the next target start
# its electron step while this one does its parallel post-work; on failure the process
# exits and the OS drops the lock.)
APP_LOCK="$APP/.stage.lock"
app_pkg_lock()    { exec 9>"$APP_LOCK"; flock 9 || die "could not acquire app packaging lock"; }
app_pkg_unlock()  { flock -u 9 2>/dev/null || true; exec 9>&- 2>/dev/null || true; }
ensure_app_deps() { [ -x "$APP/node_modules/.bin/electron-builder" ] || ( cd "$APP" && npm ci ); }
mkdir -p "$OUT"

# Atomic phase: wipe THIS target's prior bundle outputs before (re)building, so a
# crash / disk-full mid-write can't leave a half-written tree that a rerun mistakes
# for good (electron's unpacked dir, the per-OS component group, the standalone
# launcher). ninja only runs this script when the target is out of date, so this
# fires exactly when a fresh build is wanted — no extra "needs rebuild" check needed.
# Scoped to this target's files (each target writes a distinct subtree), so it never
# races a concurrently-bundling sibling target.
clean_stale_outputs() {
  rm -rf "$OUT/components/$GROUP"
  rm -f "$OUT/$LAUNCHER_NAME"
  [ -n "$UNPACK_DIR" ] && rm -rf "$OUT/$UNPACK_DIR"
  case "$TARGET" in
    mac-*) rm -rf "$OUT/mac-arm64" "$OUT/mac-x64" ;;
  esac
  log "cleaned stale outputs for $TARGET (atomic (re)build)"
}

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
    linux-arm64)     attr=llmfit-linux-arm64; name=llmfit-linux ;;
    linux-*|nixos-*) attr=llmfit-linux-x64;   name=llmfit-linux ;;
    win-*)           attr=llmfit-win-x64;     name=llmfit-windows.exe ;;
    mac-*)           attr=llmfit-mac-arm64;   name=llmfit-darwin ;;
    *) return 0 ;;
  esac
  out="$(cd "$REPO_ROOT" && nix build ".#$attr" --no-link --print-out-paths 2>/dev/null || true)"
  [ -n "$out" ] || { warn "llmfit build failed for $TARGET — model browser disabled"; return 0; }
  bin="$(find "$out" -maxdepth 1 -type f | head -1)"
  [ -n "$bin" ] || { warn "no llmfit binary in $out"; return 0; }
  cp -f "$bin" "$pool/$name"; chmod +x "$pool/$name" 2>/dev/null || true
  log "llmfit -> components/$name ($(du -h "$pool/$name" | cut -f1))"
  # windows: the msvc-linked llmfit.exe needs VCRUNTIME140.dll beside it. Pull the
  # MSVC redist DLLs (their own nix target) into the pool too; the launcher copies
  # them next to llmfit.exe in its writable tools dir before run.
  case "$TARGET" in win-*) copy_msvc_dlls_into "$pool" ;; esac
}

# Copy the MSVC C++ redistributable DLLs (vcruntime140*.dll, …) into the shared pool
# so the windows launcher can place them beside llmfit.exe (no VC++ redist on the
# target machine). Separate nix target from llmfit — the pin lives in nix/msvc-runtime.nix.
copy_msvc_dlls_into() {  # <pool-dir>
  local pool="$1" out
  out="$(cd "$REPO_ROOT" && nix build ".#msvc-dlls-win-x64" --no-link --print-out-paths 2>/dev/null || true)"
  [ -n "$out" ] || { warn "msvc redist DLLs build failed — llmfit.exe may not start without VC++ redist"; return 0; }
  find "$out" -maxdepth 1 -type f -name '*.dll' -exec cp -f {} "$pool/" \;
  log "msvc redist -> components/ ($(find "$out" -maxdepth 1 -name '*.dll' | wc -l | tr -d ' ') DLLs)"
}

# (The splash spinner is no longer shipped in the pool — it's embedded directly in
# the launcher via build.rs/PLANAI_SPINNER_BIN, fed by the flake; see flake.nix.)

package_electron_builder() {  # linux dir / windows dir (EB_OS/EB_ARCH/UNPACK_DIR set up top)
  patch_eb_build_tools() {
    [ -n "${NIX_LD:-}" ] && command -v patchelf >/dev/null 2>&1 || return 0
    local cache="${XDG_CACHE_HOME:-$HOME/.cache}/electron-builder" f
    for f in $(find "$cache" -type f \( -name mksquashfs -o -name appimagetool \
        -o -name desktop-file-validate -o -name makensis \) 2>/dev/null); do
      patchelf --set-interpreter "$NIX_LD" "$f" 2>/dev/null || true
      [ -n "${NIX_LD_LIBRARY_PATH:-}" ] && patchelf --set-rpath "$NIX_LD_LIBRARY_PATH" "$f" 2>/dev/null || true
    done
  }
  # DEBUG=electron-builder* by default: electron-builder otherwise exits non-zero
  # with NO diagnostic on stdout (it just stops after "packaging …"), so a failure is
  # invisible — especially under ninja's captured output. The debug stream shows the
  # exact spawn/step that died. Override by exporting DEBUG before the build.
  build_once() { ( cd "$APP" && DEBUG="${DEBUG:-electron-builder*}" npx --no-install electron-builder $EB_OS $EB_ARCH --config electron-builder.yml ); }
  log "electron-builder $EB_OS $EB_ARCH -> $OUT"
  # serialized vs the other targets (shared app/ + electron-builder cache); released
  # right after so the post-electron work below overlaps across targets.
  app_pkg_lock
  ensure_app_deps
  patch_eb_build_tools
  build_once || { warn "package failed; patching helpers + retrying"; patch_eb_build_tools; build_once; }
  app_pkg_unlock
  # Everything after electron-builder is independent (distinct output files), so run
  # it concurrently: the component copy (+ llmfit nix build), the app-component pack,
  # the launcher nix build, and (linux) the NixOS FHS export overlap instead of
  # waiting on each other. None use sudo here, so there's no lock contention.
  local UNPACK="$OUT/$UNPACK_DIR"; [ -d "$UNPACK" ] || die "no $UNPACK_DIR from electron-builder"
  if [ "$EB_OS" = "--linux" ]; then
    run_jobs \
      'copy_comps_into "$OUT/components/$GROUP"' \
      'emit_app_component "$UNPACK"' \
      'emit_nixos_fhs' \
      'place_standalone_launcher "$LAUNCHER_ATTR" plan-ai "$LAUNCHER_NAME"'
  else
    run_jobs \
      'copy_comps_into "$OUT/components/$GROUP"' \
      'emit_app_component "$UNPACK"' \
      'place_standalone_launcher "$LAUNCHER_ATTR" plan-ai.exe "$LAUNCHER_NAME"'
  fi
  log "bundle done -> $OUT/ (standalone launcher + shared components/ incl. app-$TARGET)"
}

# Build a per-OS standalone launcher via nix, drop it beside the Electron app under
# its explicit shipped name, then (windows) Authenticode-sign it. Self-contained so
# run_jobs can background it (no stdout capture across the subshell boundary).
place_standalone_launcher() {  # <flake-attr> <binary-in-store> <shipped-name>
  local attr="$1" bin="$2" name="$3" L
  L="$(nix_launcher "$attr" "$bin")"
  cp -f "$L" "$OUT/$name"; chmod +x "$OUT/$name" 2>/dev/null || true
  case "$name" in *.exe) [ "$name" = plan-ai.exe ] && sign_windows || true ;; esac
  log "standalone launcher -> $OUT/$name"
}

# Emit the Electron app itself as a component (app-$TARGET) in THIS OS's format,
# into the shipped pool — the launcher mounts/links it and runs Electron from it
# (parity with runtime/ollama/ow-assets). linux=squashfs, win=zip, mac=hfsplus dmg.
#
# electron-builder/@electron/packager produce the unpacked tree IMPURELY (downloads
# + helper patching). So we store-import that tree (already signed, for mac) and let
# nix pack it OFFLINE (content-addressed + cached) — squashfs in a sandbox, dmg in a
# VM (the mount + cp -a preserves the .app's exec bit + signature), win dir
# materialised. import-build-component.sh does: store-import -> nix-build <attr> ->
# real file/dir at <out>. Same on-disk format as before, so the loader mounts it
# identically. (The three targets run concurrently but import under distinct names.)
emit_app_component() {  # <src>  (electron unpacked dir; for mac a dir holding plan.ai.app)
  local src="$1" name="app-$TARGET" cdst="$OUT/components/$GROUP"; mkdir -p "$cdst"
  case "$FMTS" in
    squashfs) "$SCRIPT_DIR/import-build-component.sh" "$name" "$src" "$name-squashfs" "$cdst/$name.squashfs" ;;
    dmg)      "$SCRIPT_DIR/import-build-component.sh" "$name" "$src" "$name-dmg"      "$cdst/$name.dmg" ;;
    # win: nix materialises the app dir (cached, content-addressed); we then pack it
    # into ONE zip the launcher unpacks on update (image carries it unpacked).
    zip)      local tmp; tmp="$(mktemp -d)"
              "$SCRIPT_DIR/import-build-component.sh" "$name" "$src" "$name-dir" "$tmp/$name"
              pack_zip "$tmp/$name" "$cdst/$name.zip"; rm -rf "$tmp" ;;
    *) die "no app-component format for FMTS=$FMTS" ;;
  esac
  log "app component (nix) -> $name [$FMTS]"
}

# raw HFS+ image macOS mounts via hdiutil, compressed to UDIF via libdmg-hfsplus.
# Thin wrapper over lib.sh's shared pack_dmg (also used by pack-component.sh).
emit_hfsplus_dmg() {  # <src-dir> <out.dmg> [volume-label]
  pack_dmg "$1" "$2" "${3:-PlanAI}" || die "dmg build failed (mkfs.hfsplus / sudo loop-mount?): $2"
  log "app component -> $(basename "$2") ($(du -h "$2" | cut -f1))"
}

# Ship the NixOS FHS helper closure (NAR) into the pool. On NixOS the static-musl
# launcher imports it + re-execs inside the sandbox so the generic glibc Electron/
# ollama run (NixOS's bare nix-ld stub can't run them directly).
emit_nixos_fhs() {
  local cdst="$OUT/components/$GROUP" fhs fhs_attr sqfs_attr
  # Arch-matched FHS: the closure is the TARGET machine's store paths, so arm64 NixOS
  # needs the aarch64 env/squashfs. emit_nixos_fhs only runs for linux targets.
  case "$TARGET" in
    linux-x64|nixos-x64) fhs_attr=nixosFhs;       sqfs_attr=nixos-fhs-squashfs-x64 ;;
    linux-arm64)         fhs_attr=nixosFhs-arm64; sqfs_attr=nixos-fhs-squashfs-arm64 ;;
    *) die "emit_nixos_fhs: unexpected target $TARGET" ;;
  esac
  command -v nix >/dev/null 2>&1 || { warn "no nix — skip NixOS FHS helper"; return 0; }
  fhs="$(cd "$REPO_ROOT" && nix build ".#$fhs_attr" --no-link --print-out-paths 2>/dev/null || true)"
  [ -n "$fhs" ] || { warn "$fhs_attr build failed — skip FHS helper"; return 0; }
  mkdir -p "$cdst"
  # The FHS closure as a squashfs (each store path at the squashfs root by hash-name).
  # On NixOS the launcher squashfuse-mounts it and bind/overlays it as /nix/store inside
  # an outer bwrap — no `nix-store --import`, so no trusted-user requirement. The wrapper
  # path (resolved under the mounted store at runtime) is recorded for the launcher.
  log "packing NixOS FHS squashfs ($sqfs_attr) in nix -> components/$GROUP/"
  "$SCRIPT_DIR/nix-component.sh" "$sqfs_attr" "$cdst/nixos-fhs.squashfs"
  echo "$fhs/bin/planai-fhs" > "$cdst/nixos-fhs.path"
  log "  nixos-fhs.squashfs ($(du -h "$cdst/nixos-fhs.squashfs" | cut -f1)) + nixos-fhs.path"
}

# Wrap the mac launcher binary in a tiny .app so Finder double-click works. Its
# MacOS executable IS the rust launcher; it mounts app-mac-*.dmg from the pool and
# runs the real Electron app from it. Signed (ad-hoc unless MAC_P12 is set).
#
# The .app ships INSIDE a dmg (plan-ai.dmg), not as a bare folder: the USB image is
# FAT32, which stores no unix exec bit and no resource-fork/xattr — a bare .app
# copied there loses its executable bit and code signature, so Gatekeeper refuses
# it. An HFS+ dmg preserves the bundle intact; the user double-clicks plan-ai.dmg,
# then plan.ai.app. The launcher then finds the shared components/ pool on the USB
# (it scans /Volumes/* — see components_dir() in launcher/src/main.rs).
build_mac_launcher_app() {
  local DMG="$OUT/plan-ai.dmg"; rm -f "$DMG"
  # Ad-hoc default (no cert): the whole wrap+sign+dmg is pure + offline, so let nix
  # build it (launcher-mac-arm64-dmg: plan.ai.app, rcodesign ad-hoc, mkDmg in a VM —
  # no host sudo loop-mount). The VM mount + cp -a preserves the .app's exec bit +
  # signature. nix owns the build + content-addressed cache.
  if [ -z "${MAC_P12:-}" ]; then
    "$SCRIPT_DIR/nix-component.sh" launcher-mac-arm64-dmg "$DMG"
    warn "MAC_P12 unset — launcher .app ad-hoc signed in nix (skipping verify)"
    log "mac launcher (nix) -> $DMG (mount it, then double-click plan.ai.app)"
    return
  fi
  # Real cert signing reads a secret p12 (impure) → stays imperative on the host.
  local L; L="$(nix_launcher launcher-mac-arm64 plan-ai)"
  local STAGE; STAGE="$(mktemp -d)"; local LAPP="$STAGE/plan.ai.app"
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
  rcodesign sign --p12-file "$MAC_P12" --p12-password "${MAC_P12_PASS:-}" --code-signature-flags runtime "$LAPP"
  # verify only a real signature — ad-hoc sigs always "fail" verify (rcodesign
  # prints "problems reported during verification"), which is just noise.
  rcodesign verify "$LAPP/Contents/MacOS/plan-ai" 2>&1 | tail -1 || true
  # Wrap the signed .app in a dmg (volume "plan.ai") so it survives the FAT32 USB
  # with exec bit + signature intact. emit_hfsplus_dmg copies the staging dir's
  # contents, so the dmg volume holds plan.ai.app at its root.
  emit_hfsplus_dmg "$STAGE" "$DMG" "plan.ai"
  rm -rf "$STAGE"
  log "mac launcher (cert-signed) -> $DMG (mount it, then double-click plan.ai.app)"
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
  # Locked vs other targets (reads shared app/); released before the rcodesign +
  # dmg work so it overlaps a concurrent linux/win bundle's post-electron steps.
  app_pkg_lock
  ensure_app_deps
  ( cd "$APP" && DEBUG="${DEBUG:-electron-*}" npx --no-install @electron/packager . "plan.ai" --platform=darwin --arch="$ARCH" \
      --out="$APPROOT" --overwrite --app-bundle-id=ai.plan.usb --ignore="(^/\.stage)" )
  app_pkg_unlock
  local APPDIR; APPDIR="$(ls -d "$APPROOT"/plan.ai-darwin-*/plan.ai.app 2>/dev/null | head -1)"
  [ -d "$APPDIR" ] || die "packager produced no .app"
  log "rcodesign sign"
  if [ -n "${MAC_P12:-}" ]; then
    rcodesign sign --p12-file "$MAC_P12" --p12-password "${MAC_P12_PASS:-}" --code-signature-flags runtime "$APPDIR"
    # verify only a real signature — ad-hoc sigs always "fail" verify (noise).
    rcodesign verify "$APPDIR/Contents/MacOS/plan.ai" 2>&1 | tail -1 || true
  else rcodesign sign "$APPDIR"; warn "MAC_P12 unset — ad-hoc signature (not notarizable; skipping verify)"; fi
  # Overlap the component copy (+ llmfit nix build, no sudo) with the dmg work. The
  # two dmg builds (app-mac component + launcher .app) BOTH need a sudo loop-mount,
  # so they stay serial w.r.t. each other inside one job to avoid loop-device churn.
  # The Electron .app ships as the app-mac component (a dmg holding plan.ai.app); the
  # launcher (plan-ai.dmg: mount → double-click plan.ai.app) finds the shared pool.
  local STAGE; STAGE="$(mktemp -d)"; cp -a "$APPDIR" "$STAGE/plan.ai.app"
  run_jobs \
    'copy_comps_into "$OUT/components/$GROUP"' \
    'emit_app_component "$STAGE"; build_mac_launcher_app'
  rm -rf "$STAGE"
  log "mac bundle -> $OUT/plan-ai.dmg (launcher) + components/app-$TARGET.dmg + shared $OUT/components/"
}

# NixOS has no separate bundle: the linux-x64 artifact ships the FHS helper
# closure (emit_nixos_fhs) and the static-musl launcher FHS-reexecs on NixOS, so
# the regular linux build runs there too. `make dev` covers local NixOS dev.

clean_stale_outputs
case "$TARGET" in
  linux-*|win-*) package_electron_builder ;;
  mac-*)         package_mac ;;
  *)             die "unknown TARGET '$TARGET' (expected linux-x64 | win-x64 | mac-arm64)" ;;
esac
