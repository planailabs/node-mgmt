#!/usr/bin/env bash
# Create a FAT32 loop image, put the runtime data (models + data dir) on it, and
# launch the NixOS launcher with models/data living on the FAT32 filesystem —
# the real USB scenario. Captures a screenshot and asserts both services ready.
#
# This validates that Ollama (reading models) and Open-WebUI (writing DATA_DIR)
# work off a FAT32 volume. The app + python runtime run from the host (the dev/
# NixOS launcher) because a python venv can't live on FAT32 (no symlinks); the
# shippable AppImage is the single-file artifact that runs from FAT32 directly.
#
# Needs sudo for the loop mount. Run inside `nix develop`.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

IMG="$DIST_DIR/test-usb.img"
MNT="$(mktemp -d)"
SIZE_MB="${1:-3000}"
SHOT="${PLANAI_CAPTURE:-/tmp/usb-dash.png}"

cleanup() {
  sudo umount "$MNT" 2>/dev/null || true
  rmdir "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

log "create FAT32 loop image ($SIZE_MB MB) -> $IMG"
mkdir -p "$DIST_DIR"
rm -f "$IMG"; truncate -s "${SIZE_MB}M" "$IMG"
mkfs.vfat -F 32 -n PLANAITEST "$IMG" >/dev/null

log "loop-mount"
sudo mount -o loop,umask=0000 "$IMG" "$MNT"

log "stage models + data onto FAT32"
sudo mkdir -p "$MNT/models" "$MNT/data"
if [ -d "$REPO_ROOT/models" ] && [ -n "$(ls -A "$REPO_ROOT/models" 2>/dev/null)" ]; then
  sudo cp -r "$REPO_ROOT/models/." "$MNT/models/"
else
  warn "no ./models — ollama will start empty (run scripts/seed-models.sh)"
fi
sudo chmod -R 0777 "$MNT" 2>/dev/null || true

# ensure dev stack staged (resources in dist/) without launching
if [ ! -e "$DIST_DIR/runtime/venv" ] && [ ! -e "$DIST_DIR/runtime/devvenv/bin/python" ]; then
  die "dev runtime not staged — run scripts/dev.sh once first"
fi

log "launch NixOS launcher with PLANAI_PORTABLE_ROOT=$MNT (models/data on FAT32)"
export PLANAI_PORTABLE_ROOT="$MNT"
export PLANAI_CAPTURE="$SHOT" PLANAI_CAPTURE_DELAY="${PLANAI_CAPTURE_DELAY:-45000}"
xvfb-run -a -s "-screen 0 1400x900x24" "$REPO_ROOT/scripts/run-nixos.sh" >/tmp/usb-run.log 2>&1 || true

# verify: screenshot produced + FAT32 received Open-WebUI data (proves DATA_DIR on FAT32 worked)
RC=0
[ -s "$SHOT" ] && log "screenshot: $SHOT" || { warn "no screenshot produced"; RC=1; }
if ls "$MNT/data"/* >/dev/null 2>&1; then
  log "Open-WebUI wrote to FAT32 DATA_DIR:"; ls -la "$MNT/data" | sed 's/^/    /' | head
else
  warn "no data written to FAT32 DATA_DIR"; RC=1
fi
# model still readable on FAT32?
ls "$MNT/models/manifests" >/dev/null 2>&1 && log "models present on FAT32" || warn "models missing on FAT32"

exit $RC
