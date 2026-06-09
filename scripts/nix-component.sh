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
P="$(cd "$REPO_ROOT" && nix-build --impure nix/builds.nix -A "$ATTR" --no-out-link)"
mkdir -p "$(dirname "$OUT")"
# -L: copy the real file out of the read-only store so bundle/image can read it.
cp -fL "$P" "$OUT"
log "nix component -> $OUT ($(du -h "$OUT" | cut -f1))"
