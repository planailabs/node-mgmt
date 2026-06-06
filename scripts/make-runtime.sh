#!/usr/bin/env bash
# Assemble a relocatable Python runtime for one target with Open-WebUI installed.
#
# Usage: scripts/make-runtime.sh [target]
#   target: linux-x64 | mac-arm64 | mac-x64 | win-x64  (default: host)
#
# HOST target  -> uv-managed standalone CPython + a relocatable venv (native deps
#                 resolved by running the interpreter).
# CROSS target -> download python-build-standalone for the target and install the
#                 wheel + deps into its site-packages with `uv pip install
#                 --python-platform <triple> --only-binary` (no interpreter run).
#                 This lets NixOS produce mac/windows runtimes. Packages without a
#                 matching binary wheel are skipped (reported), since they can't be
#                 cross-compiled here.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need uv

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
HOST="$(host_target)"
PYVER="$(py_version)"
TAG="$(ow_version)"
WHEEL="$(ls -t "$DIST_DIR/wheel"/open_webui-*.whl 2>/dev/null | head -1 || true)"
[ -n "$WHEEL" ] || die "no open-webui wheel — run scripts/build-openwebui.sh first"

RT="$DIST_DIR/runtime/$TARGET"
rm -rf "$RT"; mkdir -p "$RT"

if [ "$TARGET" = "$HOST" ]; then
  # ---- native: standalone CPython + relocatable venv ----------------------
  PYDIR="$RT/python"; VENV="$RT/venv"
  log "[$TARGET] fetch standalone CPython $PYVER (native)"
  uv python install "$PYVER" --install-dir "$PYDIR"
  PYBIN="$(ls "$PYDIR"/cpython-*/bin/python3 2>/dev/null | head -1 || true)"
  [ -z "$PYBIN" ] && PYBIN="$(ls "$PYDIR"/cpython-*/python.exe 2>/dev/null | head -1 || true)"
  [ -n "$PYBIN" ] && [ -x "$PYBIN" ] || die "could not locate managed python under $PYDIR"
  log "[$TARGET] create relocatable venv + install open-webui"
  uv venv --relocatable --python "$PYBIN" "$VENV"
  VIRTUAL_ENV="$VENV" uv pip install --python "$VENV" "$WHEEL"
  LAYOUT="venv"
else
  # ---- cross: python-build-standalone + --python-platform install ---------
  TRIPLE="$(target_triple "$TARGET")"
  PYDIR="$RT/python"
  log "[$TARGET] download python-build-standalone ($TRIPLE)"
  TARBALL="$RT/python.tar.gz"
  download_verified "$(pbs_url "$TARGET")" "$TARBALL" "-"
  mkdir -p "$PYDIR"; tar -xzf "$TARBALL" -C "$PYDIR" --strip-components=1; rm -f "$TARBALL"
  # site-packages location differs by OS
  case "$TARGET" in
    win-*) SP="$PYDIR/Lib/site-packages" ;;
    *)     SP="$(ls -d "$PYDIR"/lib/python*/site-packages 2>/dev/null | head -1)" ;;
  esac
  [ -n "$SP" ] || die "site-packages not found under $PYDIR"
  log "[$TARGET] cross-install open-webui (+deps) for $TRIPLE into site-packages"
  uv pip install \
    --target "$SP" \
    --python-platform "$TRIPLE" \
    --python-version "$PYVER" \
    --only-binary :all: \
    "$WHEEL" 2>&1 | tail -3 || warn "[$TARGET] some packages lacked $TRIPLE wheels (see above)"
  LAYOUT="python"
fi

# trim caches
find "$RT" -type d -name '__pycache__' -prune -exec rm -rf {} + 2>/dev/null || true

cat > "$RT/runtime.json" <<JSON
{
  "target": "$TARGET",
  "python": "$PYVER",
  "openwebui": "$TAG",
  "wheel": "$(basename "$WHEEL")",
  "layout": "$LAYOUT",
  "cross": $([ "$TARGET" = "$HOST" ] && echo false || echo true)
}
JSON

log "runtime ready -> $RT (layout: $LAYOUT)"
