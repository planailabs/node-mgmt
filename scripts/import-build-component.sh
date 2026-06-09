#!/usr/bin/env bash
# Store-import a folder, then nix-build a component derivation from it and place a
# REAL file at <out>. For components whose source is an imperatively-built/fetched
# folder (e.g. vendor/ow-assets): the import makes it a content-addressed store path
# the nix derivation (<attr> in nix/builds.nix) consumes OFFLINE — squashfs in a
# sandbox, dmg in a VM. The on-disk format matches pack-component.sh's, so the loader
# mounts it identically; nix owns the build + caching (changed source -> rebuild).
#
# Usage: import-build-component.sh <store-name> <src-dir> <nix-attr> <out-file>
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need nix

NAME="${1:?usage: import-build-component.sh <store-name> <src-dir> <nix-attr> <out-file>}"
SRC="${2:?usage: import-build-component.sh <store-name> <src-dir> <nix-attr> <out-file>}"
ATTR="${3:?usage: import-build-component.sh <store-name> <src-dir> <nix-attr> <out-file>}"
OUT="${4:?usage: import-build-component.sh <store-name> <src-dir> <nix-attr> <out-file>}"
[ -d "$SRC" ] || die "not a directory: $SRC"

"$SCRIPT_DIR/store-import.sh" "$NAME" "$SRC"
P="$(cd "$REPO_ROOT" && nix-build --impure nix/builds.nix -A "$ATTR" --no-out-link)"
mkdir -p "$(dirname "$OUT")"
# -L: copy the real file out of the read-only store so bundle/image can read it.
cp -fL "$P" "$OUT"
log "nix component -> $OUT ($(du -h "$OUT" | cut -f1))"
