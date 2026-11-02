#!/usr/bin/env bash
# Warm the xzar binary cache with the closures the `deploy` stage rebuilds.
#
# `update-tarball` runs `nix develop -c make bundles`, whose ninja graph builds
# the SPA (`scripts/build-spa.sh` -> `nix build .#spa`), the four `hermes-*`
# components and the four `runtime-*` wheel installs (14-82 min each on a cold
# store). All of those are functions of nixpkgs + the vendored FODs (`usb.lock` /
# `vendor.lock.json` / the `hermes|runtime/wheels-*.lock.json` wheelhouses) and
# NOT of the git tree, so their store paths are stable across commits and can be
# pinned here. When the runner's store has been garbage-collected the deploy job
# then substitutes them instead of rebuilding:
#
#   pipeline 3098 / job 7664 spent 5h20m rebuilding 1041 derivations on a cold
#   store (dioxus SPA 82 min, hermes-linux-* 45 min, hermes-win-x64 17 min,
#   runtimes ~38 min) and was killed by the 6h cap on node 29/43.
#
# Deliberately NOT warmed here: anything whose input is an impure `store-import`
# produced inside the job (the runtime squashfs/zip/dmg packs, `ow-assets`, the
# electron bundle legs). There is no stable derivation to pin — caching those
# needs the FOD/cacheable treatment tracked in `.gitlab-ci.yml`.
#
# Run in CI via:  bash scripts/warm-deploy-cache.sh   (needs XZAR_TOKEN to upload)
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.."
REPO_ROOT="$PWD"

# "<flake attr>:<pin name>" — pin names mirror `prebuild.sh`'s `node-mgmt/<thing>`.
TARGETS=(
  "spa:node-mgmt/spa"
  "hermes-linux-x64:node-mgmt/hermes-linux-x64"
  "hermes-linux-arm64:node-mgmt/hermes-linux-arm64"
  "hermes-win-x64:node-mgmt/hermes-win-x64"
  "hermes-mac-arm64:node-mgmt/hermes-mac-arm64"
  "runtime-linux-x64:node-mgmt/runtime-linux-x64"
  "runtime-linux-arm64:node-mgmt/runtime-linux-arm64"
  "runtime-win-x64:node-mgmt/runtime-win-x64"
  "runtime-mac-arm64:node-mgmt/runtime-mac-arm64"
)

# Say up front whether each closure is a cache hit. A cold one is the whole
# reason this job exists, and ninja/prebuild output alone would only show it as
# a wall of compiler lines.
for entry in "${TARGETS[@]}"; do
  attr="${entry%%:*}"
  echo "── cache status for .#${attr} ──"
  nix build ".#${attr}" --dry-run 2>&1 | grep -E 'will be (built|fetched)' || true
done

results=()
cleanup() {
  if (( ${#results[@]} )); then rm -f "${results[@]}"; fi
}
trap cleanup EXIT

for entry in "${TARGETS[@]}"; do
  attr="${entry%%:*}"
  out="result-warm-${attr}"
  echo "── building .#${attr} ──"
  nix build ".#${attr}" -o "$out" -L
  results+=("$out")
done

# ── Push to the xzar cache (protected pipelines only carry XZAR_TOKEN) ──────────
if [ -z "${XZAR_TOKEN:-}" ]; then
  echo "XZAR_TOKEN is not set; warmed without cache upload (expected for unprotected pipelines)."
  exit 0
fi

xzar config add-server planai https://xzar.plan.ai "$XZAR_TOKEN"

upload() {
  while ! xzar --server planai upload --pin "$1" --desc "$(readlink -f "$2")" --leave-after-abandon 1m "$2"; do true; done
}

for entry in "${TARGETS[@]}"; do
  attr="${entry%%:*}"
  upload "${entry#*:}" "result-warm-${attr}"
done
