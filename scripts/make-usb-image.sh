#!/usr/bin/env bash
# Generate a ready-to-burn FAT32 disk image containing the whole thing for
# Linux, Windows and macOS, plus the pre-seeded ollama models.
#
# Usage: scripts/make-usb-image.sh [out.img] [--size-mb N] [--label NAME]
#
# Layout (everything at the image root so each platform binary finds the shared
# models/ + data/ via its own USB-relative path resolution):
#   /plan-ai-<ver>-linux-x64.AppImage
#   /plan-ai-<ver>-win-x64.exe
#   /plan-ai-<ver>-mac-<arch>.zip        (unzip on macOS -> plan.ai.app)
#   /models/                              (ollama models, shared)
#   /README.txt
#
# No root required: the image is formatted with mkfs.vfat and populated with
# mtools (mcopy), never mounted. FAT32 has a 4 GiB per-file limit — the tool
# fails if any single artifact exceeds it.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

OUT="$DIST_DIR/plan-ai-usb.img"
SIZE_MB=""
LABEL="PLANAI"
while [ $# -gt 0 ]; do
  case "$1" in
    --size-mb) SIZE_MB="$2"; shift 2 ;;
    --label) LABEL="$2"; shift 2 ;;
    -*) die "unknown flag: $1" ;;
    *) OUT="$1"; shift ;;
  esac
done

need mkfs.vfat; need mcopy; need mmd; need truncate
VERSION="$(jq -r '.version' "$REPO_ROOT/app/package.json")"
BUNDLE="$DIST_DIR/bundle"

# --- collect artifacts ------------------------------------------------------
declare -a FILES=()
add_if() { [ -e "$1" ] && FILES+=("$1") && log "include $(basename "$1")"; }

shopt -s nullglob
for f in "$BUNDLE"/*.AppImage; do add_if "$f"; done          # linux
for f in "$BUNDLE"/plan-ai-*-win-*.exe; do add_if "$f"; done # windows portable/installer
for f in "$BUNDLE"/plan-ai-*-mac-*.zip; do add_if "$f"; done # macOS
shopt -u nullglob

[ "${#FILES[@]}" -gt 0 ] || die "no artifacts in $BUNDLE — run scripts/bundle.sh <target> first"

MODELS="$REPO_ROOT/models"
HAVE_MODELS=no; [ -d "$MODELS" ] && [ -n "$(ls -A "$MODELS" 2>/dev/null)" ] && HAVE_MODELS=yes

# --- FAT32 4 GiB per-file guard --------------------------------------------
FOURGIB=$((4*1024*1024*1024 - 1))
for f in "${FILES[@]}"; do
  sz=$(stat -c%s "$f")
  [ "$sz" -le "$FOURGIB" ] || die "FAT32 4GiB per-file limit exceeded: $(basename "$f") = $((sz/1024/1024))MB. Use exFAT, or split the runtime."
done

# --- size the image ---------------------------------------------------------
total=0
for f in "${FILES[@]}"; do total=$((total + $(stat -c%s "$f"))); done
[ "$HAVE_MODELS" = yes ] && total=$((total + $(du -sb "$MODELS" | cut -f1)))
if [ -z "$SIZE_MB" ]; then
  SIZE_MB=$(( total / 1048576 * 115 / 100 + 64 ))   # +15% slack + 64MB headroom
fi
log "image: $OUT  size=${SIZE_MB}MB  artifacts=${#FILES[@]}  models=$HAVE_MODELS"

# --- create + format --------------------------------------------------------
mkdir -p "$(dirname "$OUT")"
rm -f "$OUT"
truncate -s "${SIZE_MB}M" "$OUT"
mkfs.vfat -F 32 -n "$LABEL" "$OUT" >/dev/null

MC=(mcopy -i "$OUT" -s -Q)

# README
README="$(mktemp)"
cat > "$README" <<EOF
plan.ai — portable offline AI (Ollama + Open-WebUI), v$VERSION

Run on:
  Linux    : ./plan-ai-$VERSION-linux-x64.AppImage   (chmod +x first)
  Windows  : plan-ai-$VERSION-win-x64.exe
  macOS    : unzip plan-ai-$VERSION-mac-*.zip, then open plan.ai.app

Models and your data live in /models and /data on this drive (shared across
platforms). Everything runs locally and offline.
EOF
"${MC[@]}" "$README" ::/README.txt
rm -f "$README"

# artifacts at root
for f in "${FILES[@]}"; do "${MC[@]}" "$f" ::/ ; done

# shared models + empty data dir
if [ "$HAVE_MODELS" = yes ]; then mmd -i "$OUT" ::/models 2>/dev/null || true; "${MC[@]}" "$MODELS"/* ::/models/ ; fi
mmd -i "$OUT" ::/data 2>/dev/null || true

# --- report -----------------------------------------------------------------
log "contents:"; mdir -i "$OUT" :: 2>/dev/null | sed 's/^/    /' || true
log "done — burn with:  sudo dd if=$OUT of=/dev/sdX bs=4M status=progress conv=fsync"
