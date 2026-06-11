#!/usr/bin/env bash
# Produce hermes/wheels-<target>.lock.json: the wheels of hermes-agent's
# `[all,messaging]` closure from the VENDORED hermes uv.lock (each with an SRI
# hash), per distributable target. flake.nix turns these into wheelhouse FODs
# that nix/hermes.nix installs offline into the portable pbs tree — the same
# wheels-FOD pattern as the open-webui runtime (gen-wheels-lock.sh), but
# filtered to the closure: hermes' lock also pins extras we don't ship (voice,
# dingtalk, …) whose wheels would bloat the wheelhouse.
#
# Run after scripts/fetch-vendor.sh (needs vendor/hermes/<tag>/src). Re-run when
# usb.lock bumps `.hermes.version`.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need python3; need nix; need uv

HERMES_TAG="$(hermes_version)"
SRC="$VENDOR_DIR/hermes/$HERMES_TAG/src"
[ -f "$SRC/uv.lock" ] || die "hermes source not vendored — run scripts/fetch-vendor.sh"

TARGETS=("$@"); [ ${#TARGETS[@]} -eq 0 ] && TARGETS=(linux-x64 linux-arm64 win-x64 mac-arm64)
mkdir -p "$REPO_ROOT/hermes"

# The closure of hermes-agent[all,messaging]: a universal (marker-annotated)
# export; per-target wheel choice happens by filename tag in select-wheels.py.
# --no-emit-project: hermes-agent itself is built as a wheel by nix/hermes.nix.
NAMES="$(mktemp)"
trap 'rm -f "$NAMES"' EXIT
( cd "$SRC" && uv export --frozen --no-hashes --no-emit-project --no-annotate \
    --no-header --extra all --extra messaging ) \
  | sed -E 's/[=<>;[:space:]].*//' | grep -v '^\s*$' | grep -v '^[.-]' | sort -u > "$NAMES"
log "hermes closure: $(wc -l < "$NAMES") packages (all,messaging)"

for TARGET in "${TARGETS[@]}"; do
  OUT="$REPO_ROOT/hermes/wheels-$TARGET.lock.json"
  log "selecting $TARGET wheels from hermes uv.lock"
  python3 "$(dirname "${BASH_SOURCE[0]}")/select-wheels.py" "$TARGET" "$SRC/uv.lock" "$OUT" --only "$NAMES"
done
log "hermes wheel locks written for: ${TARGETS[*]}"
