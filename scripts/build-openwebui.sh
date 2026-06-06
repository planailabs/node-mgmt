#!/usr/bin/env bash
# Build the Open-WebUI wheel (frontend + backend, platform-agnostic) and
# prefetch the assets the offline kiosk needs at runtime.
#
# Outputs:
#   dist/wheel/open_webui-<v>-py3-none-any.whl   (frontend force-included)
#   vendor/ow-assets/hf/                          (sentence-transformers model)
#   vendor/ow-assets/nltk/                        (nltk punkt_tab)
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

TAG="$(ow_version)"
SRC="$VENDOR_DIR/open-webui/$TAG/src"
WHEEL_OUT="$DIST_DIR/wheel"
ASSETS="$VENDOR_DIR/ow-assets"
EMBED_MODEL="${RAG_EMBEDDING_MODEL:-sentence-transformers/all-MiniLM-L6-v2}"

[ -d "$SRC" ] || die "open-webui source missing — run scripts/download-openwebui.sh first"
need node; need npm; need uv

# 1. Frontend build (pyodide:fetch + vite build). Output is force-included into
#    open_webui/frontend by the hatch build backend (see pyproject force-include).
log "open-webui: npm ci"
( cd "$SRC" && npm ci )
log "open-webui: npm run build (pyodide:fetch + vite build)"
( cd "$SRC" && npm run build )

# 2. Build the wheel. py3-none-any: the built frontend is bundled; native deps
#    are resolved later per-platform in make-runtime.sh.
log "open-webui: uv build --wheel"
mkdir -p "$WHEEL_OUT"
( cd "$SRC" && uv build --wheel --out-dir "$WHEEL_OUT" )
WHEEL="$(ls -t "$WHEEL_OUT"/open_webui-*.whl | head -1)"
[ -n "$WHEEL" ] || die "wheel build produced no artifact in $WHEEL_OUT"
log "wheel: $WHEEL"

# 3. Prefetch offline kiosk assets using transient uv environments so the
#    devshell python stays clean.
log "prefetch embedding model: $EMBED_MODEL -> $ASSETS/hf"
mkdir -p "$ASSETS/hf"
HF_HOME="$ASSETS/hf" uv run --quiet --with huggingface_hub python - "$EMBED_MODEL" <<'PY'
import os, sys
from huggingface_hub import snapshot_download
snapshot_download(repo_id=sys.argv[1], cache_dir=os.environ["HF_HOME"])
print("ok:", sys.argv[1])
PY

log "prefetch nltk punkt_tab -> $ASSETS/nltk"
mkdir -p "$ASSETS/nltk"
uv run --quiet --with nltk python - "$ASSETS/nltk" <<'PY'
import sys, nltk
nltk.download("punkt_tab", download_dir=sys.argv[1])
print("ok: punkt_tab")
PY

log "build-openwebui: done (wheel + offline assets ready)"
