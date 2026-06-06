#!/usr/bin/env bash
# Assemble a relocatable Python runtime for a target with Open-WebUI installed.
#
# Usage: scripts/make-runtime.sh [target]
#   target: linux-x64 | mac-arm64 | mac-x64 | win-x64  (default: host)
#
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
TRIPLE="$(target_triple "$TARGET")"
WHEEL="$(ls -t "$DIST_DIR/wheel"/open_webui-*.whl 2>/dev/null | head -1 || true)"
[ -n "$WHEEL" ] || die "no open-webui wheel — run scripts/build-openwebui.sh first"

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
log "[$TARGET] install open-webui (+deps) for $TRIPLE"
uv pip install \
  --target "$SP" \
  --python-platform "$TRIPLE" \
  --python-version "$PYVER" \
  --only-binary :all: \
  "$WHEEL" 2>&1 | tail -4 || warn "[$TARGET] some packages lacked $TRIPLE wheels (see above)"

# completeness guard: a partial cross-resolve (e.g. uv silently dropping native
# wheels) yields a tiny, broken runtime. Require the core packages + a sane size
# so we fail loudly here instead of shipping a runtime that crashes at launch.
[ -f "$SP/open_webui/main.py" ] || die "[$TARGET] open_webui not installed into $SP"
[ -f "$SP/open_webui/frontend/index.html" ] || warn "[$TARGET] frontend missing in wheel?"
MISSING=""
for pkg in chromadb onnxruntime fastapi uvicorn sqlalchemy; do
  [ -e "$SP/$pkg" ] || ls -d "$SP/${pkg}"* >/dev/null 2>&1 || MISSING="$MISSING $pkg"
done
[ -z "$MISSING" ] || die "[$TARGET] runtime incomplete — missing:$MISSING (uv dropped wheels? clear ~/.cache/uv and retry)"
RT_BYTES=$(du -sb "$RT" | cut -f1)
[ "$RT_BYTES" -ge 800000000 ] || die "[$TARGET] runtime suspiciously small ($((RT_BYTES/1024/1024))MB < 800MB) — native deps likely missing; clear ~/.cache/uv and retry"

# trim caches
find "$RT" -type d -name '__pycache__' -prune -exec rm -rf {} + 2>/dev/null || true

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
