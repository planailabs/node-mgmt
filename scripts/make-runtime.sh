#!/usr/bin/env bash
# Assemble a relocatable Python runtime for a target with Open-WebUI installed.
#
# Usage: scripts/make-runtime.sh [target]
#   target: linux-x64 | mac-arm64 | win-x64 | nixos-x64  (default: host)
#
# EVERYTHING is built with nix — there is no non-nix fallback:
#
#   * linux-x64 / win-x64 / mac-arm64 -> the wheels-FOD derivation
#     (flake .#runtime-<target>, see nix/runtime.nix). uv resolves, nix fetches
#     each wheel as a fixed-output derivation, and a vanilla derivation installs
#     them offline into the python-build-standalone tree with no fixup. The result
#     is PROVEN portable (generic /lib64 interpreter, $ORIGIN rpath, zero
#     /nix/store refs); we copy it OUT of the store so nothing shipped points at
#     the store. Cross-resolution never executes the target interpreter, so all
#     three targets build on NixOS.
#
#   * nixos-x64 -> a nix-native venv (nixpkgs python via uv) that runs on NixOS
#     as-is. This intentionally references /nix/store: the NixOS bundle launches
#     under nixpkgs Electron on a nix machine, so a generic interpreter (which
#     would need patchelf) is unnecessary here.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need nix; need tar

host_target() {
  local os arch
  case "$(uname -s)" in
    Linux) os=linux ;; Darwin) os=mac ;; MINGW*|MSYS*|CYGWIN*) os=win ;;
    *) die "unsupported host OS" ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch=x64 ;; arm64|aarch64) arch=arm64 ;;
    *) die "unsupported host arch" ;;
  esac
  echo "$os-$arch"
}

TARGET="${1:-$(host_target)}"
PYVER="$(py_version)"
TAG="$(ow_version)"

# completeness guard: a partial install yields a tiny, broken runtime. Require
# core packages + a sane size so we fail loudly here rather than ship a runtime
# that crashes at launch.
check_runtime() {  # <site-packages> <runtime-dir>
  local sp="$1" rt="$2" missing="" pkg bytes
  [ -f "$sp/open_webui/main.py" ] || die "[$TARGET] open_webui not installed into $sp"
  [ -f "$sp/open_webui/frontend/index.html" ] || warn "[$TARGET] frontend missing in wheel?"
  for pkg in chromadb onnxruntime fastapi uvicorn sqlalchemy; do
    [ -e "$sp/$pkg" ] || ls -d "$sp/${pkg}"* >/dev/null 2>&1 || missing="$missing $pkg"
  done
  [ -z "$missing" ] || die "[$TARGET] runtime incomplete — missing:$missing"
  bytes=$(du -sb "$rt" | cut -f1)
  [ "$bytes" -ge 800000000 ] || die "[$TARGET] runtime suspiciously small ($((bytes/1024/1024))MB < 800MB) — native deps missing"
}

# --- nixos: nix-native venv (runs on NixOS) ---------------------------------
if [ "$TARGET" = "nixos-x64" ]; then
  RT="$DIST_DIR/runtime/$TARGET"; VENV="$RT/venv"
  rm -rf "$RT"; mkdir -p "$RT"
  trap '[ -f "$RT/runtime.json" ] || rm -rf "$RT"' EXIT
  NIXPY="$(command -v python3)"
  log "[$TARGET] nix-native venv ($NIXPY) + open-webui"
  uv venv --python "$NIXPY" "$VENV"
  # CPU torch (no ~4GB NVIDIA CUDA) via the pytorch index, matching the wheel locks.
  VIRTUAL_ENV="$VENV" uv pip install --python "$VENV" \
    --extra-index-url https://download.pytorch.org/whl/cpu --index-strategy unsafe-best-match \
    "open-webui==$(printf '%s' "$TAG" | sed 's/^v//')" 2>&1 | tail -3
  SP="$(ls -d "$VENV"/lib/python*/site-packages | head -1)"
  rm -rf "$SP"/nvidia_* "$SP"/nvidia "$SP"/triton 2>/dev/null || true
  find "$SP" -type d -name '__pycache__' -prune -exec rm -rf {} + 2>/dev/null || true
  check_runtime "$SP" "$RT"
  cat > "$RT/runtime.json" <<JSON
{ "target": "$TARGET", "python": "$PYVER", "openwebui": "$TAG",
  "layout": "venv", "nix_native": true }
JSON
  log "[$TARGET] runtime ready -> $RT ($(du -sh "$RT" | cut -f1))"
  exit 0
fi

# --- distributable targets: the wheels-FOD nix derivation, copied out of store
nix_runtime_attr() {
  case "$1" in
    linux-x64) echo "runtime-linux-x64" ;;
    win-x64)   echo "runtime-win-x64" ;;
    mac-arm64) echo "runtime-mac-arm64" ;;
    *)         echo "" ;;
  esac
}
ATTR="$(nix_runtime_attr "$TARGET")"
[ -n "$ATTR" ] || die "[$TARGET] no nix runtime for target (supported: linux-x64, win-x64, mac-arm64, nixos-x64)"

log "[$TARGET] building portable runtime via nix (.#$ATTR)"
STORE="$(cd "$REPO_ROOT" && nix build ".#$ATTR" --no-link --print-out-paths)" \
  || die "[$TARGET] nix runtime build failed"

RT="$DIST_DIR/runtime/$TARGET"; PYDIR="$RT/python"
rm -rf "$RT"; mkdir -p "$RT"
trap '[ -f "$RT/runtime.json" ] || rm -rf "$RT"' EXIT
# copy out of the store into a writable, self-contained tree under python/ (the
# layout paths.js/pack-component expect). -a (not -L) keeps the tree's internal
# relative symlinks intact; the derivation output references no other store path,
# so the copy is store-free. PRESERVE mode (the interpreter + .so files must keep
# their +x bit — only drop ownership, since we copy out of the store as a user)
# then add owner-write so the tree is mutable.
cp -a --no-preserve=ownership "$STORE" "$PYDIR"
chmod -R u+w "$PYDIR"

case "$TARGET" in
  win-*) SP="$PYDIR/Lib/site-packages" ;;
  *)     SP="$(ls -d "$PYDIR"/lib/python*/site-packages 2>/dev/null | head -1)" ;;
esac
[ -n "$SP" ] && [ -d "$SP" ] || die "[$TARGET] site-packages not found under $PYDIR"
check_runtime "$SP" "$RT"

cat > "$RT/runtime.json" <<JSON
{ "target": "$TARGET", "triple": "$(target_triple "$TARGET")", "python": "$PYVER",
  "openwebui": "$TAG", "layout": "python", "source": "nix:$ATTR" }
JSON

log "[$TARGET] runtime ready (nix) -> $RT ($(du -sh "$RT" | cut -f1))"
