#!/usr/bin/env bash
# Generate a ready-to-burn FAT32 disk image with every platform artifact + the
# pre-seeded ollama models. With the per-OS component groups + CPU-only runtime,
# each file stays under FAT32's 4 GiB per-file limit, so a plain FAT32 image (no
# exFAT, no splitting) holds everything and reads on every OS.
#
# Two stages:
#   1. IMPERATIVE: assemble a drive-root DIR with exactly the on-disk layout
#      (launchers + components/<os>/ + models + update.json + platforms.json +
#      README), generate the update manifest (xtask), enforce the FAT32 4 GiB guard.
#   2. IMPERATIVE (outside the nix store): mkfs.vfat + mcopy the drive-root into the
#      image OFFLINE with the PINNED userspace mtools/dosfstools from the flake
#      (.#usb-image-tools) — no VM, no sudo. We deliberately do NOT store-import the
#      drive-root: it's launchers + components + multi-GB seeded models, and a
#      `nix store add-path` would duplicate ALL of it into /nix/store. The image is
#      a pure file transform of a folder we already have on disk, so packing it in
#      place avoids that heap of duplicated data. Only the tools come from nix.
#
# Usage: scripts/make-usb-image.sh [out.img] [--label NAME] [--size-mb N]
#   --label/--size-mb are accepted for back-compat but ignored: the volume is
#   PLANAI and the size is derived from the drive-root inside the derivation.
#
# Layout (everything at the image root; each tiny launcher finds the per-TARGET group
# components/<target>/ beside it, and models/ + data/ via USB-relative paths). The
# Electron app itself ships inside its group as app-<target>:
#   /plan-ai.linux-x64.exe   linux x64 launcher (static musl ELF; chmod +x)
#   /plan-ai.linux-arm64.exe linux arm64 launcher (static musl ELF; chmod +x)
#   /plan-ai.exe             windows launcher
#   /plan-ai.dmg             macOS launcher (dmg holding plan.ai.app; FAT32-safe)
#   /components/<target>/    that target's component group (runtime, ollama, ow-assets,
#                            app, llmfit, manifest.json; linux also nixos-fhs.closure)
#   /models/   /data/   /README.txt   /update.json   /platforms.json
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need nix

OUT="$DIST_DIR/plan-ai-usb.img"
while [ $# -gt 0 ]; do
  case "$1" in
    --size-mb) shift 2 ;;   # accepted + ignored (size derived in the derivation)
    --label)   shift 2 ;;   # accepted + ignored (volume is PLANAI)
    --fs)      shift 2 ;;   # accepted + ignored (FAT32 only)
    -*) die "unknown flag: $1" ;;
    *) OUT="$1"; shift ;;
  esac
done

VERSION="$(jq -r '.version' "$REPO_ROOT/app/package.json")"
BUNDLE="$DIST_DIR/bundle"
UPDATE_URL="${PLANAI_UPDATE_URL:-https://usb-update.plan.ai}"
COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo "")"
BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

declare -a FILES=()
add_if() { [ -e "$1" ] || return 0; FILES+=("$1"); log "include $(basename "$1")"; }
shopt -s nullglob
# the standalone launchers, one per target (the app itself rides inside
# components/<target>/ as app-<target>). linux ships both arches, so the *.exe glob
# matches plan-ai.linux-x64.exe / plan-ai.linux-arm64.exe; win/mac are single-arch.
for f in "$BUNDLE"/plan-ai.*.exe "$BUNDLE"/plan-ai.exe "$BUNDLE"/plan-ai.dmg; do add_if "$f"; done
shopt -u nullglob
[ "${#FILES[@]}" -gt 0 ] || die "no launchers in $BUNDLE — run scripts/bundle.sh <target> first"

# the per-OS component groups (components/<os>/) that ship beside the launchers
POOL="$BUNDLE/components"; TOOLSDIR="$BUNDLE/tools"
[ -d "$POOL" ] || die "no component groups at $POOL — run scripts/bundle.sh <target> first"

# FAT32 4 GiB per-file guard — applies to launchers AND every component file.
FOURGIB=$((4*1024*1024*1024 - 1))
check_size() { local f; for f in "$@"; do [ -f "$f" ] || continue
  local sz; sz=$(stat -c%s "$f")
  [ "$sz" -le "$FOURGIB" ] || die "$(basename "$f") = $((sz/1024/1024))MB exceeds FAT32's 4 GiB/file limit"
done; }
check_size "${FILES[@]}"
big="$(find "$POOL" -type f -size +"${FOURGIB}c" 2>/dev/null | head -1)"
[ -z "$big" ] || die "$(basename "$big") = $(( $(stat -c%s "$big")/1024/1024 ))MB exceeds FAT32's 4 GiB/file limit"

MODELS="$REPO_ROOT/models"
HAVE_MODELS=no; [ -d "$MODELS" ] && [ -n "$(ls -A "$MODELS" 2>/dev/null)" ] && HAVE_MODELS=yes

# --- stage 1: assemble the drive-root (real on-disk layout) -----------------
# Hardlink the big trees (launchers + component groups + models) so assembly is
# near-free; store-import below copies them into the store once. README/update.json/
# platforms.json are written fresh (small, not hardlinked — they're modified here).
DRIVE="$DIST_DIR/.drive-root"; rm -rf "$DRIVE"; mkdir -p "$DRIVE/data"
log "assembling drive-root -> $DRIVE  (launchers=${#FILES[@]} models=$HAVE_MODELS)"
for f in "${FILES[@]}"; do cp -al "$f" "$DRIVE/$(basename "$f")" 2>/dev/null || cp -a "$f" "$DRIVE/"; done
cp -al "$POOL" "$DRIVE/components" 2>/dev/null || cp -a "$POOL" "$DRIVE/components"
[ -d "$TOOLSDIR" ] && { cp -al "$TOOLSDIR" "$DRIVE/tools" 2>/dev/null || cp -a "$TOOLSDIR" "$DRIVE/tools"; }
[ "$HAVE_MODELS" = yes ] && { cp -al "$MODELS" "$DRIVE/models" 2>/dev/null || cp -a "$MODELS" "$DRIVE/models"; }

cat > "$DRIVE/README.txt" <<EOF
plan.ai — portable offline AI (Ollama + Open-WebUI), v$VERSION

Run on (each launcher mounts the matching app + runtime from /components/<target>/):
  Linux x64  : chmod +x ./plan-ai.linux-x64.exe    then  ./plan-ai.linux-x64.exe
  Linux arm64: chmod +x ./plan-ai.linux-arm64.exe  then  ./plan-ai.linux-arm64.exe
  Windows    : run plan-ai.exe
  macOS      : open plan-ai.dmg, then double-click plan.ai.app inside it

First launch unpacks/mounts the runtime for your machine into a local cache;
models and your data live in /models and /data on this drive. Everything runs
offline. (On FAT32 the unix exec bit is not stored — Linux may need a chmod +x
on the launcher; macOS ships a .dmg so the .app keeps its bit + signature.)

Report an issue / get help: https://git.plan.ai/plan-ai/usb
EOF

# update manifest: scan the assembled drive-root directly (it IS exactly the drive
# contents — no electron *-unpacked dirs, unlike $BUNDLE). xtask shares the schema +
# per-OS platform tagging with the launcher updater; models/data/update.json/
# platforms.json are excluded by the manifest. Written into the drive-root.
( cd "$REPO_ROOT" && nix run .#xtask -- gen-manifest "$DRIVE" \
    --version "$VERSION" --commit "$COMMIT" --url "$UPDATE_URL" --built-at "$BUILT_AT" \
    --out "$DRIVE/update.json" )
log "update.json -> drive root (url=$UPDATE_URL commit=${COMMIT:0:8})"

# platforms.json: which platforms this USB keeps. The image ships ALL the platforms
# it was built with, so seed it with all of them — otherwise the launcher would
# create it with only the CURRENT platform on first run and prune the others.
PLATS=()
for f in "${FILES[@]}"; do case "$(basename "$f")" in
  plan-ai.linux-x64.exe)   PLATS+=(linux-x64) ;;
  plan-ai.linux-arm64.exe) PLATS+=(linux-arm64) ;;
  plan-ai.exe)             PLATS+=(win-x64) ;;
  plan-ai.dmg)             PLATS+=(mac-arm64) ;;
esac; done
printf '%s\n' "${PLATS[@]}" | jq -Rsc '{platforms: (split("\n") | map(select(length>0)))}' > "$DRIVE/platforms.json"
log "platforms.json -> drive root ($(jq -c .platforms "$DRIVE/platforms.json"))"

# image-only: drop default-off feature components (hermes, …) — they stay in the
# manifest + on the update server; the launcher downloads them when the user
# enables the feature. Also seeds the default feature set into platforms.json.
( cd "$REPO_ROOT" && nix run .#xtask -- image-prep "$DRIVE" )

# --- stage 2: pack the FAT32 image directly, OUTSIDE the nix store -----------
# Realise the pinned tools once (cached after the first build), then mkfs.vfat +
# mcopy the on-disk drive-root straight into the image. No store-import → the big
# drive-root never lands in /nix/store. Size = drive-root + 15% + 128M slack.
mkdir -p "$(dirname "$OUT")"
log "realising pinned FAT32 tools (.#usb-image-tools) …"
TOOLS="$(cd "$REPO_ROOT" && nix build --no-link --print-out-paths .#usb-image-tools)"
[ -x "$TOOLS/bin/mkfs.vfat" ] && [ -x "$TOOLS/bin/mcopy" ] || die "usb-image-tools missing mkfs.vfat/mcopy"

# Windows components ship as .zip in the update tarball, but the burned image carries
# them UNPACKED. The manifest (update.json) was generated above WITH the zip entries
# (so the launcher can diff against them on update); now expand each component zip into
# its target folder (path minus .zip), wiping it first, and remove the zip — the image
# then holds only the unpacked tree. (Linux/mac components are single squashfs/dmg
# files and stay as-is; only Windows produces zips.)
shopt -s globstar nullglob
for z in "$DRIVE"/components/**/*.zip; do
  [ -f "$z" ] || continue
  tgt="${z%.zip}"
  log "unpack $(basename "$z") -> ${tgt#"$DRIVE/"}"
  rm -rf "$tgt"; mkdir -p "$tgt"
  "$TOOLS/bin/unzip" -qo "$z" -d "$tgt" || die "unzip failed: $z"
  rm -f "$z"
done
shopt -u globstar nullglob

bytes=$(du -sb "$DRIVE" | cut -f1)
mb=$(( bytes / 1048576 * 115 / 100 + 128 ))
log "FAT32 image: ${mb}MB from drive-root $(du -sh "$DRIVE" | cut -f1) -> $OUT"
rm -f "$OUT"
"$TOOLS/bin/truncate" -s "${mb}M" "$OUT"
"$TOOLS/bin/mkfs.vfat" -F 32 -n PLANAI "$OUT" >/dev/null
# Copy the drive-root CONTENTS (not the dir) to the image root. `*` skips dotfiles —
# the drive-root has none. mcopy -s recurses incl. empty dirs.
"$TOOLS/bin/mcopy" -i "$OUT" -s -Q -b "$DRIVE"/* ::/
"$TOOLS/bin/mmd" -i "$OUT" ::/data 2>/dev/null || true
echo "contents:"; "$TOOLS/bin/mdir" -i "$OUT" :: 2>/dev/null | sed 's/^/    /' || true
log "done — burn with:  sudo dd if=$OUT of=/dev/sdX bs=4M status=progress conv=fsync"
