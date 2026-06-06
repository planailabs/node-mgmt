#!/usr/bin/env bash
# Regenerate every lock/manifest after bumping versions in usb.lock. Run this
# whenever you change ollama/open-webui/python/pbs versions, then commit the
# updated lock files. Run inside `nix develop`.
#
# Regenerates:
#   vendor.lock.json      download FOD hashes (gen-vendor-lock.sh)
#   runtime/uv.lock       python deps for uv2nix (uv lock)
#   app/package-lock.json electron app deps (npm)
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

step() { printf '\033[1;32m▶ %s\033[0m\n' "$*" >&2; }

step "vendor.lock.json (ollama + python-build-standalone + open-webui FODs)"
"$REPO_ROOT/scripts/gen-vendor-lock.sh"

step "runtime/uv.lock (open-webui python deps, CPU torch)"
( cd "$REPO_ROOT/runtime" && uv lock --upgrade )

step "app/package-lock.json (electron app)"
( cd "$REPO_ROOT/app" && npm install --package-lock-only --no-audit --no-fund )

log "locks updated — review & commit: vendor.lock.json runtime/uv.lock app/package-lock.json"
log "pinned versions:"; jq -r '"  ollama \(.ollama.version)  open-webui \(.openwebui.version)  python \(.python)  pbs \(.pbs_release)"' "$USB_LOCK"
