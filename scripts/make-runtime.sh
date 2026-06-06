#!/usr/bin/env bash
# Assemble a relocatable Python runtime for one target: a python-build-standalone
# CPython + a relocatable uv venv with Open-WebUI (and its native deps) installed.
#
# Usage: scripts/make-runtime.sh [target]
#   target defaults to the host (linux-x64 | mac-arm64 | mac-x64 | win-x64).
#
# Cross-OS runtimes must be built on the matching OS (CI matrix): uv resolves
# and installs native wheels (chromadb/onnxruntime/...) for the running platform.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need uv

# --- resolve host target ----------------------------------------------------
host_target() {
  local os arch
  case "$(uname -s)" in
    Linux)  os=linux ;;
    Darwin) os=mac ;;
    MINGW*|MSYS*|CYGWIN*) os=win ;;
    *) die "unsupported host OS: $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch=x64 ;;
    arm64|aarch64) arch=arm64 ;;
    *) die "unsupported host arch: $(uname -m)" ;;
  esac
  echo "$os-$arch"
}

TARGET="${1:-$(host_target)}"
HOST="$(host_target)"
[ "$TARGET" = "$HOST" ] || die "target '$TARGET' != host '$HOST' — run this on a '$TARGET' host or CI runner"

PYVER="$(py_version)"
TAG="$(ow_version)"
WHEEL="$(ls -t "$DIST_DIR/wheel"/open_webui-*.whl 2>/dev/null | head -1 || true)"
[ -n "$WHEEL" ] || die "no open-webui wheel — run scripts/build-openwebui.sh first"

RT="$DIST_DIR/runtime/$TARGET"
PYDIR="$RT/python"
VENV="$RT/venv"
rm -rf "$RT"; mkdir -p "$RT"

# 1. Fetch a managed standalone CPython into our own dir (relocatable build).
log "fetch python-build-standalone CPython $PYVER -> $PYDIR"
uv python install "$PYVER" --install-dir "$PYDIR"
# Discover the interpreter from our install dir (a custom --install-dir is not
# tracked by `uv python find`). Layout: <install-dir>/cpython-<ver>-<triple>/bin.
PYBIN="$(ls "$PYDIR"/cpython-*/bin/python3 2>/dev/null | head -1 || true)"
[ -z "$PYBIN" ] && PYBIN="$(ls "$PYDIR"/cpython-*/python.exe 2>/dev/null | head -1 || true)"
[ -n "$PYBIN" ] && [ -x "$PYBIN" ] || die "could not locate managed python under $PYDIR"

# 2. Create a RELOCATABLE venv so it runs from any USB mount point.
log "create relocatable venv -> $VENV"
uv venv --relocatable --python "$PYBIN" "$VENV"

# 3. Install Open-WebUI + native deps for THIS platform into the venv.
log "install $(basename "$WHEEL") (+ native deps) into venv"
VIRTUAL_ENV="$VENV" uv pip install --python "$VENV" "$WHEEL"

# 4. Trim caches/tests to keep the runtime minimal.
find "$VENV" -type d -name '__pycache__' -prune -exec rm -rf {} + 2>/dev/null || true
find "$VENV" -type d -name 'tests' -prune -exec rm -rf {} + 2>/dev/null || true

# 5. Record a runtime manifest for the bundler.
cat > "$RT/runtime.json" <<JSON
{
  "target": "$TARGET",
  "python": "$PYVER",
  "openwebui": "$TAG",
  "wheel": "$(basename "$WHEEL")",
  "venv": "venv",
  "uvicorn": "venv/bin/python -m uvicorn open_webui.main:app"
}
JSON

log "runtime ready -> $RT (manifest: $RT/runtime.json)"
