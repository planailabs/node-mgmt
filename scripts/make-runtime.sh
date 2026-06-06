#!/usr/bin/env bash
# Assemble a relocatable Python runtime for a target with Open-WebUI installed.
#
# Usage: scripts/make-runtime.sh [target]
#   target: linux-x64 | mac-arm64 | mac-x64 | win-x64 | nixos-x64  (default: host)
#
# nixos-x64 builds a nix-native venv (nixpkgs python) that runs on NixOS as-is.
# Every other target uses the cross approach below:
# Approach (works for ALL targets, and crucially builds on NixOS): download
# python-build-standalone for the target and install the wheel + deps into its
# site-packages with `uv pip install --python-platform <triple> --only-binary`.
# This never runs the target interpreter (NixOS can't run generic ELF / foreign
# binaries), and produces a relocatable tree that runs natively on the target
# machine via `python -m uvicorn open_webui.main:app`.
#
# Packages without a matching binary wheel for the target are skipped (reported)
# — they can't be cross-compiled here. For the NixOS dev run use scripts/dev.sh
# (a nix-native venv) instead.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need uv; need tar

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
WHEEL="$(ls -t "$DIST_DIR/wheel"/open_webui-*.whl 2>/dev/null | head -1 || true)"
[ -n "$WHEEL" ] || die "no open-webui wheel — run scripts/build-openwebui.sh first"

# completeness guard: a partial resolve (uv silently dropping native wheels)
# yields a tiny, broken runtime. Require core packages + a sane size so we fail
# loudly here instead of shipping a runtime that crashes at launch.
# The kiosk does CPU inference only. On linux we install CPU torch (see CPU_INDEX
# below), so no NVIDIA CUDA wheels (~4GB) are pulled at all. This just trims
# caches/tests + any stray nvidia/triton dirs.
strip_runtime() {  # <site-packages>
  local sp="$1"
  rm -rf "$sp"/nvidia_* "$sp"/nvidia "$sp"/triton 2>/dev/null || true
  find "$sp" -type d -name '__pycache__' -prune -exec rm -rf {} + 2>/dev/null || true
  find "$sp" -type d -name 'tests' -prune -exec rm -rf {} + 2>/dev/null || true
  find "$sp" -type f -name '*.pyc' -delete 2>/dev/null || true
}

# CPU-only torch on linux (pypi torch is CUDA; +cpu has no nvidia deps and no
# eager CUDA preload). mac/win pypi wheels are already CPU-only.
torch_index_args() {
  case "$1" in
    linux-x64|nixos-x64) printf '%s\n' --extra-index-url https://download.pytorch.org/whl/cpu --index-strategy unsafe-best-match ;;
  esac
}

check_runtime() {  # <site-packages> <runtime-dir>
  local sp="$1" rt="$2" missing="" pkg bytes
  [ -f "$sp/open_webui/main.py" ] || die "[$TARGET] open_webui not installed into $sp"
  [ -f "$sp/open_webui/frontend/index.html" ] || warn "[$TARGET] frontend missing in wheel?"
  for pkg in chromadb onnxruntime fastapi uvicorn sqlalchemy; do
    [ -e "$sp/$pkg" ] || ls -d "$sp/${pkg}"* >/dev/null 2>&1 || missing="$missing $pkg"
  done
  [ -z "$missing" ] || die "[$TARGET] runtime incomplete — missing:$missing (uv dropped wheels? clear ~/.cache/uv and retry)"
  bytes=$(du -sb "$rt" | cut -f1)
  [ "$bytes" -ge 800000000 ] || die "[$TARGET] runtime suspiciously small ($((bytes/1024/1024))MB < 800MB) — native deps likely missing; clear ~/.cache/uv and retry"
}

# --- nixos: nix-native venv (runs on NixOS) ---------------------------------
if [ "$TARGET" = "nixos-x64" ]; then
  RT="$DIST_DIR/runtime/$TARGET"; VENV="$RT/venv"
  rm -rf "$RT"; mkdir -p "$RT"
  NIXPY="$(command -v python3)"
  log "[$TARGET] nix-native venv ($NIXPY) + open-webui"
  uv venv --python "$NIXPY" "$VENV"
  mapfile -t TORCH_ARGS < <(torch_index_args "$TARGET")
  VIRTUAL_ENV="$VENV" uv pip install --python "$VENV" "${TORCH_ARGS[@]}" "$WHEEL" 2>&1 | tail -3
  SP="$(ls -d "$VENV"/lib/python*/site-packages | head -1)"
  strip_runtime "$SP"
  check_runtime "$SP" "$RT"
  cat > "$RT/runtime.json" <<JSON
{ "target": "$TARGET", "python": "$PYVER", "openwebui": "$TAG",
  "wheel": "$(basename "$WHEEL")", "layout": "venv", "nix_native": true }
JSON
  log "[$TARGET] runtime ready -> $RT ($(du -sh "$RT" | cut -f1))"
  exit 0
fi

TRIPLE="$(target_triple "$TARGET")"

RT="$DIST_DIR/runtime/$TARGET"
PYDIR="$RT/python"
rm -rf "$RT"; mkdir -p "$PYDIR"

# 1. python-build-standalone interpreter for the target (install_only build).
log "[$TARGET] download python-build-standalone $PYVER ($TRIPLE)"
TARBALL="$RT/python.tar.gz"
download_verified "$(pbs_url "$TARGET")" "$TARBALL" "-"
tar -xzf "$TARBALL" -C "$PYDIR" --strip-components=1
rm -f "$TARBALL"

# 2. site-packages location for the target layout.
case "$TARGET" in
  win-*) SP="$PYDIR/Lib/site-packages" ;;
  *)     SP="$(ls -d "$PYDIR"/lib/python*/site-packages 2>/dev/null | head -1)" ;;
esac
[ -n "$SP" ] && [ -d "$SP" ] || die "site-packages not found under $PYDIR"

# 3. cross-install Open-WebUI + deps for the target (no interpreter execution).
# macOS: onnxruntime (pulled by chromadb) only ships macosx_14_0 wheels, so raise
# the deployment target uv resolves against, else resolution is unsatisfiable.
case "$TARGET" in mac-*) export MACOSX_DEPLOYMENT_TARGET=14.0 ;; esac
mapfile -t TORCH_ARGS < <(torch_index_args "$TARGET")
log "[$TARGET] install open-webui (+deps) for $TRIPLE"
uv pip install \
  --target "$SP" \
  --python-platform "$TRIPLE" \
  --python-version "$PYVER" \
  --only-binary :all: \
  "${TORCH_ARGS[@]}" \
  "$WHEEL" 2>&1 | tail -4 || warn "[$TARGET] some packages lacked $TRIPLE wheels (see above)"

strip_runtime "$SP"
check_runtime "$SP" "$RT"

cat > "$RT/runtime.json" <<JSON
{
  "target": "$TARGET",
  "triple": "$TRIPLE",
  "python": "$PYVER",
  "openwebui": "$TAG",
  "wheel": "$(basename "$WHEEL")",
  "layout": "python"
}
JSON

log "[$TARGET] runtime ready -> $RT ($(du -sh "$RT" | cut -f1))"
