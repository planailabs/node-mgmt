#!/usr/bin/env bash
# Build a component squashfs IN NIX (pure, cached) from a folder: store-import the
# folder, nix-build <name>-squashfs, and place a REAL .squashfs file at <out>. The
# format is byte-compatible with pack-component.sh's (same mksquashfs flags), so the
# loader mounts it identically — nix just owns the build + content-addressed caching
# (a changed source -> new store path -> rebuild; unchanged -> instant).
#
# Usage: build-component-squashfs.sh <store-name> <src-dir> <out.squashfs>
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need nix

NAME="${1:?usage: build-component-squashfs.sh <name> <src-dir> <out.squashfs>}"
SRC="${2:?usage: build-component-squashfs.sh <name> <src-dir> <out.squashfs>}"
OUT="${3:?usage: build-component-squashfs.sh <name> <src-dir> <out.squashfs>}"
[ -d "$SRC" ] || die "not a directory: $SRC"

"$SCRIPT_DIR/store-import.sh" "$NAME" "$SRC"
P="$(cd "$REPO_ROOT" && nix-build --impure nix/builds.nix -A "$NAME-squashfs" --no-out-link)"
mkdir -p "$(dirname "$OUT")"
# -L: copy the real squashfs file (not a symlink into the read-only store) so the
# bundle/image steps can read it like any other component file.
cp -fL "$P" "$OUT"
log "squashfs (nix) -> $OUT ($(du -h "$OUT" | cut -f1))"
