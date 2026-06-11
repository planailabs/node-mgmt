#!/usr/bin/env bash
# Produce runtime/wheels-<target>.lock.json: every wheel from runtime/uv.lock that
# is compatible with <target> (default linux-x64 / cp312), each with an SRI hash
# (prefetching the few the pytorch index serves without one). flake.nix turns this
# into fetchurl FODs (a wheelhouse) that a vanilla derivation installs offline
# into the portable pbs tree (scripts/make-runtime.sh). This is the wheels-FOD
# realization of "uv2nix == wheels-fod": uv resolves, Nix fetches, no fixup.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need python3; need nix

# default: every distributable target (mac-x64 dropped — arm64-only wheels)
TARGETS=("$@"); [ ${#TARGETS[@]} -eq 0 ] && TARGETS=(linux-x64 linux-arm64 win-x64 mac-arm64)
[ -f "$REPO_ROOT/runtime/uv.lock" ] || die "runtime/uv.lock missing — run: cd runtime && uv lock"

for TARGET in "${TARGETS[@]}"; do
OUT="$REPO_ROOT/runtime/wheels-$TARGET.lock.json"
log "selecting $TARGET wheels from runtime/uv.lock (+ normalising hashes to SRI)"
python3 "$(dirname "${BASH_SOURCE[0]}")/select-wheels.py" "$TARGET" "$REPO_ROOT/runtime/uv.lock" "$OUT"
done
log "wheel locks written for: ${TARGETS[*]}"
