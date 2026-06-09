#!/usr/bin/env bash
# nix-build a component from nix/builds.nix and place a REAL file at <out>. For
# components whose SOURCE is already in nix (e.g. the ollama repack) — no
# store-import step needed. Same on-disk format as pack-component's, so the loader
# mounts it identically; nix owns the build + content-addressed caching.
#
# Usage: nix-component.sh <nix-attr> <out-file>
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need nix

ATTR="${1:?usage: nix-component.sh <nix-attr> <out-file>}"
OUT="${2:?usage: nix-component.sh <nix-attr> <out-file>}"
# -L (--print-build-logs): stream the build output so a failing component build shows
# its compiler/packer errors instead of just nix's terse "builder for … failed" line.
P="$(cd "$REPO_ROOT" && nix-build -L --impure nix/builds.nix -A "$ATTR" --no-out-link)"
mkdir -p "$(dirname "$OUT")"
# copy the real file/dir out of the read-only store so bundle/image can read it.
# dir components (used in place on FAT32) need a writable recursive copy.
if [ -d "$P" ]; then
  rm -rf "$OUT"; cp -rL "$P" "$OUT"; chmod -R u+w "$OUT"
else
  cp -fL "$P" "$OUT"
fi
log "nix component -> $OUT ($(du -sh "$OUT" | cut -f1))"
