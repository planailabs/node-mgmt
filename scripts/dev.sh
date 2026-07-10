#!/usr/bin/env bash
# Minimal NixOS development build + run ("dev mode").
#
# Produces a NixOS-runnable stack for testing the app, WITHOUT the shipped
# bundle: a venv built from the nixpkgs Python (runs natively on NixOS) with
# Open-WebUI installed from the locally built wheel, the real pinned ollama
# binary (patchelf'd to the nix loader by run-nixos.sh), and the dashboard run
# via the nixpkgs Electron. Idempotent — re-runs skip completed steps.
#
# Usage: nix develop --command ./scripts/dev.sh [-- <electron args>]
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

OLLAMA_TAG="$(ollama_version)"
OW_TAG="$(ow_version)"
DEVVENV="$DIST_DIR/runtime/devvenv"

step() { printf '\033[1;32m▶ %s\033[0m\n' "$*" >&2; }

# 0. design tokens submodule
if [ ! -f "$REPO_ROOT/third_party/plan-ai-design/assets/input.css" ]; then
  step "init plan-ai-design submodule"
  ( cd "$REPO_ROOT" && git submodule update --init --recursive third_party/plan-ai-design )
fi

# 1. ollama — minimal single flavour (real pinned build)
if [ ! -f "$VENDOR_DIR/ollama/$OLLAMA_TAG/ollama-linux-amd64.tar.zst" ]; then
  step "download ollama linux-amd64 ($OLLAMA_TAG)"
  "$REPO_ROOT/scripts/download-ollama.sh" ollama-linux-amd64.tar.zst
fi

# 2. open-webui source + wheel + offline assets (slow — build once)
[ -d "$VENDOR_DIR/open-webui/$OW_TAG/src" ] || "$REPO_ROOT/scripts/download-openwebui.sh"
if ! ls "$DIST_DIR"/wheel/open_webui-*.whl >/dev/null 2>&1; then
  step "build open-webui wheel + offline assets ($OW_TAG)"
  "$REPO_ROOT/scripts/build-openwebui.sh"
fi
WHEEL="$(ls -t "$DIST_DIR"/wheel/open_webui-*.whl | head -1)"

# 3. nix-native venv (runs on NixOS as-is) with open-webui installed
if [ ! -x "$DEVVENV/bin/python" ]; then
  step "create nix-native venv + install open-webui"
  need uv
  NIXPY="$(command -v python3)"
  uv venv --python "$NIXPY" "$DEVVENV"
  VIRTUAL_ENV="$DEVVENV" uv pip install --python "$DEVVENV" "$WHEEL"
fi

# 4. thin Electron shell deps (dev uses the nixpkgs electron; the SPA is built +
# embedded into the rust launcher by run-nixos.sh's `nix build .#launcher-*`).
[ -d "$REPO_ROOT/app/node_modules" ] || ( cd "$REPO_ROOT/app" && npm ci )

# 5. stage resources into dist/ (paths.js dev layout)
step "stage resources into dist/"
if [ ! -e "$DIST_DIR/ollama/bin/ollama" ] && [ ! -e "$DIST_DIR/ollama/ollama" ]; then
  rm -rf "$DIST_DIR/ollama"; mkdir -p "$DIST_DIR/ollama"
  need zstd; need tar
  # -f: vendor files are symlinks into the nix store (FOD downloads); plain
  # `zstd -dc` refuses symlinked input ("is a symbolic link, ignoring").
  zstd -dcf "$VENDOR_DIR/ollama/$OLLAMA_TAG/ollama-linux-amd64.tar.zst" | tar -x -C "$DIST_DIR/ollama"
fi
mkdir -p "$DIST_DIR/runtime"
ln -sfn "devvenv" "$DIST_DIR/runtime/venv"          # relative symlink within dist/runtime
[ -d "$VENDOR_DIR/ow-assets" ] && ln -sfn "$VENDOR_DIR/ow-assets" "$DIST_DIR/ow-assets"

# 6. launch via nixpkgs electron
step "launch dashboard"
exec "$REPO_ROOT/scripts/run-nixos.sh" "$@"
