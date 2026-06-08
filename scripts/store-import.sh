#!/usr/bin/env bash
# Import an imperatively built/fetched folder into the nix store (content-addressed)
# and pin it with an indirect GC root, so PURE nix derivations can consume it via
# `builtins.storePath`. This keeps the genuinely-networked/impure step OUTSIDE nix
# while nix owns the build graph + caching + invalidation for everything downstream
# (a changed folder -> new store path -> dependents rebuild). The GC root is NOT
# optional: without it `nix-store --gc` would drop the import and break incremental
# builds. Records the store path under dist/.stores/<name> for `xtask gen-stores`.
#
# Usage: scripts/store-import.sh <name> <folder>
#   <name> becomes the nix attr in nix/stores.nix (dashes are fine).
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need nix

NAME="${1:?usage: store-import.sh <name> <folder>}"
DIR="${2:?usage: store-import.sh <name> <folder>}"
[ -d "$DIR" ] || die "not a directory: $DIR"

GCROOTS="$REPO_ROOT/.nix-gcroots"; RECORDS="$DIST_DIR/.stores"
mkdir -p "$GCROOTS" "$RECORDS"

log "store-add $NAME ($(du -sh "$DIR" | cut -f1)) …"
P="$(nix store add-path "$DIR" --name "$NAME")"
# indirect root: survives nix-store --gc AND auto-updates if the path changes.
nix-store --add-root "$GCROOTS/$NAME" --indirect --realise "$P" >/dev/null
printf '%s\n' "$P" > "$RECORDS/$NAME"
log "imported $NAME -> $P"
