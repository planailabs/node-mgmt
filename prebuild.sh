#!/usr/bin/env bash
# Devshell + CI-image prebuild for the `nix-image` GitLab runners. Builds the
# dev shell and the CI runner image and pushes both to the plan.ai xzar binary
# cache, so every downstream job (and future pipeline) resolves them from the
# cache instead of rebuilding. The build itself doubles as a flake sanity check.
#
# Run in CI via: bash prebuild.sh   (needs XZAR_TOKEN for the upload leg).
# Mirrors plan-ai/mac-mgmt's prebuild.sh.
set -euo pipefail

# ── Build devShell + CI image (always — validates the flake even without a token) ──
nix build .#devShells.x86_64-linux.default -o result-devshell -L
nix build .#image -o result-image -L

# ── Push to the xzar cache (protected pipelines only carry XZAR_TOKEN) ──────────
if [ -z "${XZAR_TOKEN:-}" ]; then
  echo "XZAR_TOKEN is not set; built without cache upload (expected for unprotected pipelines)."
  rm -f result-devshell result-image
  exit 0
fi

xzar config add-server planai https://xzar.plan.ai "$XZAR_TOKEN"

upload() {
  while ! xzar --server planai upload --pin "$1" --desc "$(readlink -f "$2")" --leave-after-abandon 1m "$2"; do true; done
}

upload node-mgmt/devshell result-devshell
upload node-mgmt/image     result-image

rm -f result-devshell result-image
