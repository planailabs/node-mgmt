#!/usr/bin/env bash
# Generate a ready-to-burn FAT32 disk image with every platform artifact + the
# pre-seeded ollama models. With the component model + CPU-only runtime, each
# artifact stays under FAT32's 4 GiB per-file limit, so a plain FAT32 image (no
# exFAT, no splitting) holds everything and reads on every OS.
#
# Usage: scripts/make-usb-image.sh [out.img] [--size-mb N] [--label NAME]
#
# Layout (everything at the image root; the bare launchers find the SHARED
# components/ + tools/ beside them, and models/ + data/ via USB-relative paths):
#   /plan-ai-<ver>-linux-x86_64.AppImage   (bare launcher)
#   /plan-ai-<ver>-win-x64.zip             (bare; extract beside components/)
#   /plan-ai-<ver>-mac-<arch>.zip          (bare .app; unzip beside components/)
#   /components/   shared component pool (one copy for all platforms)
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

declare -a FILES=()
add_if() { [ -e "$1" ] && FILES+=("$1") && log "include $(basename "$1")"; }
shopt -s nullglob
for f in "$BUNDLE"/*.AppImage "$BUNDLE"/plan-ai-*-win-*.zip "$BUNDLE"/plan-ai-*-mac-*.zip; do add_if "$f"; done
shopt -u nullglob
[ "${#FILES[@]}" -gt 0 ] || die "no artifacts in $BUNDLE — run scripts/bundle.sh <target> first"

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
shopt -s nullglob; check_size "$POOL"/*; shopt -u nullglob

MODELS="$REPO_ROOT/models"
HAVE_MODELS=no; [ -d "$MODELS" ] && [ -n "$(ls -A "$MODELS" 2>/dev/null)" ] && HAVE_MODELS=yes

total=0
for f in "${FILES[@]}"; do total=$((total + $(stat -c%s "$f"))); done
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

Run on:
  Linux    : ./plan-ai-$VERSION-linux-x86_64.AppImage   (chmod +x first)
  Windows  : extract plan-ai-$VERSION-win-x64.zip, run plan.ai.exe
  macOS    : unzip plan-ai-$VERSION-mac-*.zip, then open plan.ai.app

First launch unpacks the runtime for your machine into a local cache; models and
your data live in /models and /data on this drive. Everything runs offline.
EOF
"${MC[@]}" "$README" ::/README.txt; rm -f "$README"
for f in "${FILES[@]}"; do "${MC[@]}" "$f" ::/ ; done
# shared component pool + mount tools (one copy, beside the launchers)
mmd -i "$OUT" ::/components 2>/dev/null || true; "${MC[@]}" "$POOL"/* ::/components/
[ -d "$TOOLSDIR" ] && { mmd -i "$OUT" ::/tools 2>/dev/null || true; "${MC[@]}" "$TOOLSDIR"/* ::/tools/ ; }
if [ "$HAVE_MODELS" = yes ]; then mmd -i "$OUT" ::/models 2>/dev/null || true; "${MC[@]}" "$MODELS"/* ::/models/ ; fi
mmd -i "$OUT" ::/data 2>/dev/null || true

log "contents:"; mdir -i "$OUT" :: 2>/dev/null | sed 's/^/    /' || true
log "done — burn with:  sudo dd if=$OUT of=/dev/sdX bs=4M status=progress conv=fsync"
