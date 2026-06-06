#!/usr/bin/env bash
# Generate a ready-to-burn disk image containing the whole thing for Linux,
# Windows and macOS, plus the pre-seeded ollama models.
#
# Usage: scripts/make-usb-image.sh [out.img] [--size-mb N] [--label NAME] [--fs auto|fat32|exfat]
#
# Filesystem:
#   fat32 — universal, but max 4 GiB per file (the linux AppImage is larger);
#           written with mtools, no root required.
#   exfat — supports >4 GiB files; written via a loop mount (needs sudo).
#   auto  — exfat if any artifact exceeds 4 GiB, else fat32 (default).
#
# Layout (everything at the image root so each platform binary finds the shared
# models/ + data/ via its own USB-relative path resolution):
#   /plan-ai-<ver>-linux-x64.AppImage
#   /plan-ai-<ver>-win-x64.zip            (extract -> plan.ai.exe)
#   /plan-ai-<ver>-mac-<arch>.zip         (unzip on macOS -> plan.ai.app)
#   /models/   /data/   /README.txt
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

OUT="$DIST_DIR/plan-ai-usb.img"; SIZE_MB=""; LABEL="PLANAI"; FS="auto"
while [ $# -gt 0 ]; do
  case "$1" in
    --size-mb) SIZE_MB="$2"; shift 2 ;;
    --label) LABEL="$2"; shift 2 ;;
    --fs) FS="$2"; shift 2 ;;
    -*) die "unknown flag: $1" ;;
    *) OUT="$1"; shift ;;
  esac
done

need truncate
VERSION="$(jq -r '.version' "$REPO_ROOT/app/package.json")"
BUNDLE="$DIST_DIR/bundle"

# --- collect artifacts ------------------------------------------------------
declare -a FILES=()
add_if() { [ -e "$1" ] && FILES+=("$1") && log "include $(basename "$1")"; }
shopt -s nullglob
for f in "$BUNDLE"/*.AppImage; do add_if "$f"; done
for f in "$BUNDLE"/plan-ai-*-win-*.zip "$BUNDLE"/plan-ai-*-win-*.exe; do add_if "$f"; done
for f in "$BUNDLE"/plan-ai-*-mac-*.zip; do add_if "$f"; done
shopt -u nullglob
[ "${#FILES[@]}" -gt 0 ] || die "no artifacts in $BUNDLE — run scripts/bundle.sh <target> first"

MODELS="$REPO_ROOT/models"
HAVE_MODELS=no; [ -d "$MODELS" ] && [ -n "$(ls -A "$MODELS" 2>/dev/null)" ] && HAVE_MODELS=yes

# --- choose filesystem ------------------------------------------------------
FOURGIB=$((4*1024*1024*1024 - 1))
BIGGEST=0
for f in "${FILES[@]}"; do sz=$(stat -c%s "$f"); [ "$sz" -gt "$BIGGEST" ] && BIGGEST=$sz; done
if [ "$FS" = auto ]; then
  if [ "$BIGGEST" -gt "$FOURGIB" ]; then FS=exfat; else FS=fat32; fi
fi
if [ "$FS" = fat32 ] && [ "$BIGGEST" -gt "$FOURGIB" ]; then
  die "a file exceeds FAT32's 4 GiB limit ($((BIGGEST/1024/1024))MB). Use --fs exfat."
fi
log "filesystem: $FS  (largest file $((BIGGEST/1024/1024))MB)"

# --- size the image ---------------------------------------------------------
total=0
for f in "${FILES[@]}"; do total=$((total + $(stat -c%s "$f"))); done
[ "$HAVE_MODELS" = yes ] && total=$((total + $(du -sb "$MODELS" | cut -f1)))
[ -z "$SIZE_MB" ] && SIZE_MB=$(( total / 1048576 * 115 / 100 + 128 ))
log "image: $OUT  size=${SIZE_MB}MB  artifacts=${#FILES[@]}  models=$HAVE_MODELS"

mkdir -p "$(dirname "$OUT")"; rm -f "$OUT"; truncate -s "${SIZE_MB}M" "$OUT"

README="$(mktemp)"
cat > "$README" <<EOF
plan.ai — portable offline AI (Ollama + Open-WebUI), v$VERSION

Run on:
  Linux    : ./plan-ai-$VERSION-linux-x64.AppImage   (chmod +x first)
  Windows  : extract plan-ai-$VERSION-win-x64.zip, run plan.ai.exe
  macOS    : unzip plan-ai-$VERSION-mac-*.zip, then open plan.ai.app

Models and your data live in /models and /data on this drive (shared across
platforms). Everything runs locally and offline.
EOF

if [ "$FS" = fat32 ]; then
  # --- FAT32 via mtools (no root) -------------------------------------------
  need mkfs.vfat; need mcopy; need mmd
  mkfs.vfat -F 32 -n "$LABEL" "$OUT" >/dev/null
  MC=(mcopy -i "$OUT" -s -Q)
  "${MC[@]}" "$README" ::/README.txt
  for f in "${FILES[@]}"; do "${MC[@]}" "$f" ::/ ; done
  if [ "$HAVE_MODELS" = yes ]; then mmd -i "$OUT" ::/models 2>/dev/null || true; "${MC[@]}" "$MODELS"/* ::/models/ ; fi
  mmd -i "$OUT" ::/data 2>/dev/null || true
  log "contents:"; mdir -i "$OUT" :: 2>/dev/null | sed 's/^/    /' || true
else
  # --- exFAT via loop mount (needs sudo) ------------------------------------
  need mkfs.exfat
  mkfs.exfat -n "$LABEL" "$OUT" >/dev/null
  MNT="$(mktemp -d)"
  cleanup() { sudo umount "$MNT" 2>/dev/null || true; rmdir "$MNT" 2>/dev/null || true; }
  trap cleanup EXIT
  sudo mount -o loop,uid=$(id -u),gid=$(id -g) "$OUT" "$MNT"
  cp "$README" "$MNT/README.txt"
  for f in "${FILES[@]}"; do cp "$f" "$MNT/"; done
  mkdir -p "$MNT/data"
  if [ "$HAVE_MODELS" = yes ]; then mkdir -p "$MNT/models"; cp -r "$MODELS/." "$MNT/models/"; fi
  sync
  log "contents:"; ls -lh "$MNT" | sed 's/^/    /'
fi
rm -f "$README"

log "done ($FS) — burn with:  sudo dd if=$OUT of=/dev/sdX bs=4M status=progress conv=fsync"
