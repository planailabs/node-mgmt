#!/usr/bin/env bash
# Launch the dashboard on NixOS via the nixpkgs Electron (set by the devshell's
# ELECTRON_OVERRIDE_DIST_PATH), using whatever has been staged into dist/:
#
#   dist/ollama/bin/ollama   (or dist/ollama/ollama)
#   dist/runtime/venv/...     (python venv: nix-native for dev, standalone for product)
#   dist/ow-assets/{hf,nltk}  (offline embedding model + nltk data)
#
# paths.js (dev mode) resolves resources from dist/. Children inherit
# LD_LIBRARY_PATH so Open-WebUI's native wheels (onnxruntime/chromadb) find
# libstdc++ etc. Generic (non-nix) ELF binaries staged here are patchelf'd to the
# nix loader so they run on NixOS too.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

APP="$REPO_ROOT/app"
[ -x "$DIST_DIR/ollama/bin/ollama" ] || [ -x "$DIST_DIR/ollama/ollama" ] || die "ollama not staged in dist/ (run scripts/dev.sh)"
[ -e "$DIST_DIR/runtime/venv" ] || die "python venv not staged in dist/runtime/venv (run scripts/dev.sh)"

# Repoint any generic (FHS) ELF interpreter at the nix loader so it runs under
# the bare nix-ld stub. nix-built binaries already use the nix loader → skipped.
patch_generic_elf() {
  local f="$1"
  [ -f "$f" ] || return 0
  command -v patchelf >/dev/null 2>&1 || return 0
  local interp; interp="$(patchelf --print-interpreter "$f" 2>/dev/null || true)"
  case "$interp" in
    /lib64/*|/lib/*)  # generic FHS loader → repoint
      patchelf --set-interpreter "$NIX_LD" "$f" 2>/dev/null || true
      patchelf --add-rpath "$NIX_LD_LIBRARY_PATH" "$f" 2>/dev/null || true ;;
  esac
}

if [ -n "${NIX_LD:-}" ]; then
  for b in "$DIST_DIR/ollama/bin/ollama" "$DIST_DIR/ollama/ollama"; do patch_generic_elf "$b"; done
  # standalone python interpreters (no-op for a nix-native dev venv)
  while IFS= read -r p; do patch_generic_elf "$p"; done < <(find "$DIST_DIR/runtime" -type f -name 'python3*' 2>/dev/null)
fi


# Pass nix libs to the CHILDREN only (Open-WebUI's native wheels, ollama runners).
# Do NOT set LD_LIBRARY_PATH for electron itself — mixing foreign libs into the
# nixpkgs electron crashes it (SIGILL). config.js applies this per child.
if [ -n "${NIX_LD_LIBRARY_PATH:-}" ]; then
  export PLANAI_CHILD_LD_LIBRARY_PATH="$NIX_LD_LIBRARY_PATH"
else
  warn "NIX_LD_LIBRARY_PATH unset — run inside 'nix develop'"
fi

# setup-only: stage dist/ (runtime + ollama + assets) but don't launch electron.
# Used by the dev-mode tests to prepare the runtime without opening a window.
if [ -n "${PLANAI_SETUP_ONLY:-}" ]; then
  log "setup-only: staged dist/ (no launch)"
  exit 0
fi

# Drive the SAME native launcher as the shipped artifacts, in DEV mode: it does the
# shared llmfit GPU-detect + serve (no bash reimplementation), then runs the nixpkgs
# Electron against the source app. The dev specifics (nix-native runtime staged in
# dist/, patchelf'd ollama, child LD libs) were prepared above; PLANAI_DEV makes the
# launcher skip component mounting + the NixOS FHS re-exec and use these instead.
LAUNCHER="$(cd "$REPO_ROOT" && nix build .#launcher-linux-x64 --no-link --print-out-paths 2>/dev/null)/plan-ai"
[ -x "$LAUNCHER" ] || die "launcher build failed (nix build .#launcher-linux-x64)"
LLMFIT="$(cd "$REPO_ROOT" && nix build .#llmfit-linux-x64 --no-link --print-out-paths 2>/dev/null)/llmfit"

export PLANAI_DEV=1
export PLANAI_RESOURCES="$DIST_DIR"                          # staged dev runtime/ollama/assets
export PLANAI_ELECTRON="$ELECTRON_OVERRIDE_DIST_PATH/electron"  # nixpkgs electron
export PLANAI_ELECTRON_APP="$APP"                            # source app dir
[ -x "$LLMFIT" ] && export PLANAI_LLMFIT="$LLMFIT" || warn "llmfit unavailable — no GPU panel / Models tab"

log "launching dashboard via the native launcher (dev mode) + nixpkgs electron"
exec "$LAUNCHER" "$@"
