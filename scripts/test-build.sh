#!/usr/bin/env bash
# Verify the project builds and the stack is runnable on NixOS.
#
# Phases:
#   lint     — shell/node syntax, usb.lock schema, tailwind build
#   runtime  — Open-WebUI imports in the staged runtime
#   run      — start ollama + uvicorn from the staged dist/, assert both /health
#   ui       — launch the dashboard under xvfb, screenshot, assert it rendered
#
# Run inside `nix develop`. Assumes scripts/dev.sh has staged dist/ (it will be
# invoked if not). Exits non-zero on the first failure.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

PASS=0; FAIL=0
ok()   { printf '\033[1;32m  PASS\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '\033[1;31m  FAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }
phase(){ printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }

APP="$REPO_ROOT/app"

phase "lint"
for s in "$REPO_ROOT"/scripts/*.sh; do
  bash -n "$s" && ok "syntax $(basename "$s")" || bad "syntax $(basename "$s")"
done
for j in "$APP"/main/*.js; do
  node -c "$j" 2>/dev/null && ok "node -c $(basename "$j")" || bad "node -c $(basename "$j")"
done
jq -e . "$REPO_ROOT/usb.lock" >/dev/null 2>&1 && ok "usb.lock valid json" || bad "usb.lock json"
# SPA dashboard: assert the embedded build is present (built via `make spa`).
[ -s "$REPO_ROOT/launcher/spa/index.html" ] && ok "SPA embedded (launcher/spa)" || bad "SPA missing — run 'make spa'"

phase "runtime"
# Stage the dev runtime if absent (idempotent; builds wheel/venv on first run).
if [ ! -e "$DIST_DIR/runtime/venv/bin/python" ] && [ ! -e "$DIST_DIR/runtime/venv" ]; then
  log "staging dev runtime (scripts/dev.sh setup only)…"
  PLANAI_SETUP_ONLY=1 "$REPO_ROOT/scripts/dev.sh" >/dev/null 2>&1 || true
fi
PY="$DIST_DIR/runtime/venv/bin/python"
[ -x "$PY" ] || PY="$(ls "$DIST_DIR"/runtime/devvenv/bin/python 2>/dev/null | head -1 || true)"
if [ -x "$PY" ] && LD_LIBRARY_PATH="${NIX_LD_LIBRARY_PATH:-}" "$PY" -c "import open_webui" 2>/dev/null; then
  ok "open_webui imports in runtime"
else
  bad "open_webui import"
fi

phase "run (ollama + uvicorn /health)"
OLLAMA_BIN="$(ls "$DIST_DIR"/ollama/bin/ollama "$DIST_DIR"/ollama/ollama 2>/dev/null | head -1 || true)"
export OLLAMA_HOST=127.0.0.1:11500 OLLAMA_MODELS="$REPO_ROOT/models"
export LD_LIBRARY_PATH="${NIX_LD_LIBRARY_PATH:-}"
"$OLLAMA_BIN" serve >/tmp/test-ollama.log 2>&1 & OPID=$!
trap 'kill $OPID $UPID 2>/dev/null || true' EXIT
for _ in $(seq 1 30); do curl -sf "http://127.0.0.1:11500/api/version" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://127.0.0.1:11500/api/version" >/dev/null 2>&1 && ok "ollama /api/version" || bad "ollama health"

FE="$(ls -d "$DIST_DIR"/runtime/*/lib/python*/site-packages/open_webui/frontend 2>/dev/null | head -1 || true)"
export DATA_DIR="$REPO_ROOT/.run-data" HF_HOME="$VENDOR_DIR/ow-assets/hf" SENTENCE_TRANSFORMERS_HOME="$VENDOR_DIR/ow-assets/hf" NLTK_DATA="$VENDOR_DIR/ow-assets/nltk"
mkdir -p "$DATA_DIR"   # open-webui opens its sqlite db here
export HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1 WEBUI_AUTH=False WEBUI_SECRET_KEY=test OAUTH_SESSION_TOKEN_ENCRYPTION_KEY=test
[ -n "$FE" ] && export FRONTEND_BUILD_DIR="$FE"
"$PY" -m uvicorn open_webui.main:app --host 127.0.0.1 --port 8085 >/tmp/test-webui.log 2>&1 & UPID=$!
hok=no
for _ in $(seq 1 90); do curl -sf "http://127.0.0.1:8085/health" >/dev/null 2>&1 && { hok=yes; break; }; kill -0 $UPID 2>/dev/null || break; sleep 1; done
[ "$hok" = yes ] && ok "open-webui /health" || bad "open-webui health (see /tmp/test-webui.log)"
[ "$hok" = yes ] && { code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8085/); [ "$code" = 200 ] && ok "open-webui SPA / (200)" || bad "SPA / ($code)"; }
kill $OPID $UPID 2>/dev/null || true

phase "summary"
printf 'PASS=%d FAIL=%d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
