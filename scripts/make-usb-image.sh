#!/usr/bin/env bash
# Generate a ready-to-burn FAT32 disk image with every platform artifact + the
# pre-seeded ollama models. With the component model + CPU-only runtime, each
# artifact stays under FAT32's 4 GiB per-file limit, so a plain FAT32 image (no
# exFAT, no splitting) holds everything and reads on every OS.
#
# Usage: scripts/make-usb-image.sh [out.img] [--size-mb N] [--label NAME]
#
# Layout (everything at the image root; the standalone launchers find the SHARED
# components/ + tools/ beside them, and models/ + data/ via USB-relative paths).
# The Electron app itself ships as a component (app-<os> in the pool); each tiny
# launcher mounts/links it + the runtime/ollama/ow-assets and runs Electron:
#   /plan-ai.linux.exe linux launcher (static musl ELF; chmod +x)
#   /plan-ai.exe       windows launcher
#   /plan-ai.dmg       macOS launcher (dmg holding plan.ai.app; FAT32-safe)
#   /components/   shared component pool incl. app-<os> (one copy for all platforms)
#   /tools/        static squashfuse/unsquashfs (linux mount)
#   /models/   /data/   /README.txt
#
# No root required: formatted with mkfs.vfat, populated with mtools (mcopy).
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

OUT="$DIST_DIR/plan-ai-usb.img"; SIZE_MB=""; LABEL="PLANAI"
while [ $# -gt 0 ]; do
  case "$1" in
    --size-mb) SIZE_MB="$2"; shift 2 ;;
    --label) LABEL="$2"; shift 2 ;;
    --fs) shift 2 ;;   # accepted + ignored (FAT32 only now)
    -*) die "unknown flag: $1" ;;
    *) OUT="$1"; shift ;;
  esac
done

need mkfs.vfat; need mcopy; need mmd; need truncate
VERSION="$(jq -r '.version' "$REPO_ROOT/app/package.json")"
BUNDLE="$DIST_DIR/bundle"
# update manifest inputs (see scripts/gen-update-manifest via xtask)
UPDATE_URL="${PLANAI_UPDATE_URL:-https://usb-update.plan.ai}"
COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo "")"
BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

declare -a FILES=()
add_if() { [ -e "$1" ] || return 0; FILES+=("$1"); log "include $(basename "$1")"; }
shopt -s nullglob
# the standalone launchers (the app itself rides inside components/ as app-<os>)
for f in "$BUNDLE"/plan-ai.linux.exe "$BUNDLE"/plan-ai.exe "$BUNDLE"/plan-ai.dmg; do add_if "$f"; done
shopt -u nullglob
[ "${#FILES[@]}" -gt 0 ] || die "no launchers in $BUNDLE — run scripts/bundle.sh <target> first"

# the shared component pool + mount tools that ship beside the launchers
POOL="$BUNDLE/components"; TOOLSDIR="$BUNDLE/tools"
[ -d "$POOL" ] || die "no shared components pool at $POOL — run scripts/bundle.sh <target> first"

# FAT32 4 GiB per-file guard — applies to launchers AND every component file.
FOURGIB=$((4*1024*1024*1024 - 1))
check_size() { local f; for f in "$@"; do [ -f "$f" ] || continue
  local sz; sz=$(stat -c%s "$f")
  [ "$sz" -le "$FOURGIB" ] || die "$(basename "$f") = $((sz/1024/1024))MB exceeds FAT32's 4 GiB/file limit"
done; }
check_size "${FILES[@]}"
# every file in the shared pool too (recurse — windows components are directories)
big="$(find "$POOL" -type f -size +"${FOURGIB}c" 2>/dev/null | head -1)"
[ -z "$big" ] || die "$(basename "$big") = $(( $(stat -c%s "$big")/1024/1024 ))MB exceeds FAT32's 4 GiB/file limit"

MODELS="$REPO_ROOT/models"
HAVE_MODELS=no; [ -d "$MODELS" ] && [ -n "$(ls -A "$MODELS" 2>/dev/null)" ] && HAVE_MODELS=yes

total=0
for f in "${FILES[@]}"; do total=$((total + $(du -sb "$f" | cut -f1))); done
total=$((total + $(du -sb "$POOL" | cut -f1)))
[ -d "$TOOLSDIR" ] && total=$((total + $(du -sb "$TOOLSDIR" | cut -f1)))
[ "$HAVE_MODELS" = yes ] && total=$((total + $(du -sb "$MODELS" | cut -f1)))
[ -z "$SIZE_MB" ] && SIZE_MB=$(( total / 1048576 * 115 / 100 + 128 ))
log "FAT32 image: $OUT  size=${SIZE_MB}MB  artifacts=${#FILES[@]}  models=$HAVE_MODELS"

mkdir -p "$(dirname "$OUT")"; rm -f "$OUT"; truncate -s "${SIZE_MB}M" "$OUT"
mkfs.vfat -F 32 -n "$LABEL" "$OUT" >/dev/null
MC=(mcopy -i "$OUT" -s -Q)

README="$(mktemp)"
cat > "$README" <<EOF
plan.ai — portable offline AI (Ollama + Open-WebUI), v$VERSION

Run on (each launcher mounts the matching app + runtime from /components):
  Linux    : chmod +x ./plan-ai.linux.exe   then  ./plan-ai.linux.exe
  Windows  : run plan-ai.exe
  macOS    : open plan-ai.dmg, then double-click plan.ai.app inside it

First launch unpacks/mounts the runtime for your machine into a local cache;
models and your data live in /models and /data on this drive. Everything runs
offline. (On FAT32 the unix exec bit is not stored — Linux may need a chmod +x
on the launcher; macOS ships a .dmg so the .app keeps its bit + signature.)

Report an issue / get help: https://git.plan.ai/plan-ai/usb
EOF
"${MC[@]}" "$README" ::/README.txt
for f in "${FILES[@]}"; do "${MC[@]}" "$f" ::/ ; done
# shared component pool + mount tools (one copy, beside the launchers)
mmd -i "$OUT" ::/components 2>/dev/null || true; "${MC[@]}" "$POOL"/* ::/components/
[ -d "$TOOLSDIR" ] && { mmd -i "$OUT" ::/tools 2>/dev/null || true; "${MC[@]}" "$TOOLSDIR"/* ::/tools/ ; }
if [ "$HAVE_MODELS" = yes ]; then mmd -i "$OUT" ::/models 2>/dev/null || true; "${MC[@]}" "$MODELS"/* ::/models/ ; fi
mmd -i "$OUT" ::/data 2>/dev/null || true

# update manifest: a symlink mirror of exactly the drive contents (NOT $BUNDLE
# wholesale — that holds electron-builder's *-unpacked dirs), scanned by xtask
# (shared schema + platform tagging with the launcher updater). models/+data/ are
# excluded by the manifest. Shipped at the drive root as update.json.
MIRROR="$DIST_DIR/.drive-mirror"; rm -rf "$MIRROR"; mkdir -p "$MIRROR"
for f in "${FILES[@]}"; do ln -s "$f" "$MIRROR/$(basename "$f")"; done
ln -s "$POOL" "$MIRROR/components"
[ -d "$TOOLSDIR" ] && ln -s "$TOOLSDIR" "$MIRROR/tools"
cp "$README" "$MIRROR/README.txt"
( cd "$REPO_ROOT" && nix run .#xtask -- gen-manifest "$MIRROR" \
    --version "$VERSION" --commit "$COMMIT" --url "$UPDATE_URL" --built-at "$BUILT_AT" \
    --out "$MIRROR/update.json" )
"${MC[@]}" "$MIRROR/update.json" ::/update.json
log "update.json -> drive root (url=$UPDATE_URL commit=${COMMIT:0:8})"
rm -rf "$MIRROR" "$README"

log "contents:"; mdir -i "$OUT" :: 2>/dev/null | sed 's/^/    /' || true
log "done — burn with:  sudo dd if=$OUT of=/dev/sdX bs=4M status=progress conv=fsync"
