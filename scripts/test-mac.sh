#!/usr/bin/env bash
# Run the macOS launcher on a REAL remote Mac (ssh $MAC_TARGET) — pushes the
# plan-ai.dmg + the mac/shared components, mounts the dmg, runs plan.ai.app's
# launcher pointed at the pushed pool, and asserts the stack SERVES (ollama +
# Open-WebUI health). Mirrors test-ubuntu-vm but for macOS.
#
# Requires: MAC_TARGET = the ssh host of a mac with a LOGGED-IN GUI session
# (Electron needs the window server; the supervised ollama + open-webui are what
# we health-check). Best-effort screenshot is skipped (headless render is flaky).
#
# Usage: MAC_TARGET=mac scripts/test-mac.sh
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

HOST="${MAC_TARGET:?set MAC_TARGET to the ssh host of a mac (e.g. MAC_TARGET=mac)}"
need ssh; need scp
DMG="$DIST_DIR/bundle/plan-ai.dmg"
POOL="$DIST_DIR/bundle/components"
[ -f "$DMG" ]  || die "no plan-ai.dmg — run scripts/bundle.sh mac-arm64 first"
[ -d "$POOL" ] || die "no components pool — run scripts/bundle.sh mac-arm64 first"

log "remote mac test on '$HOST'"
REMOTE="$(ssh "$HOST" 'mktemp -d /tmp/planai-test.XXXXXX')" || die "ssh $HOST failed"
# Tear down the launcher dmg AND the component dmgs it mounts under the fixed cache
# (~/Library/Caches/plan-ai/root/dist/*). We `kill` the launcher below rather than
# letting it teardown, so those component mounts would otherwise leak and a later run
# (or `hdiutil`) would hit "Permission denied" mounting onto the busy mountpoint. The
# launcher self-heals such stale mounts on its next start, but a clean teardown keeps
# the box tidy regardless.
cleanup() {
  ssh "$HOST" 'mount | awk "/plan-ai\/root\/dist/ {print \$1}" | while read d; do hdiutil detach "$d" -force >/dev/null 2>&1; done; hdiutil detach "'"$REMOTE"'/mnt" -force >/dev/null 2>&1; rm -rf "'"$REMOTE"'"' 2>/dev/null || true
}
trap cleanup EXIT
log "remote workdir: $HOST:$REMOTE"

# push the launcher dmg + this OS's component group (components/mac/) the launcher needs.
ssh "$HOST" "mkdir -p '$REMOTE/components'"
scp -q "$DMG" "$HOST:$REMOTE/plan-ai.dmg"
[ -d "$POOL/mac" ] || die "no components/mac group in $POOL — run scripts/bundle.sh mac-arm64"
scp -q -r "$POOL/mac" "$HOST:$REMOTE/components/" || true

log "mount dmg + run launcher; assert ollama + Open-WebUI serve (≤6min cold start)"
# Pass $REMOTE as $1 to the remote bash (avoids quoting the path into the heredoc).
ssh "$HOST" bash -s "$REMOTE" <<'RSH' | tee /tmp/mac-run.log | sed 's/^/    /' || true
set -u
REMOTE="$1"
mkdir -p "$REMOTE/mnt"
hdiutil attach -nobrowse -noverify -mountpoint "$REMOTE/mnt" "$REMOTE/plan-ai.dmg" >/dev/null 2>&1 || { echo "MOUNT_FAILED"; exit 0; }
APP="$(ls -d "$REMOTE/mnt"/*.app/Contents/MacOS/* 2>/dev/null | head -1)"
[ -n "$APP" ] || { echo "NO_APP"; exit 0; }
export PLANAI_COMPONENTS="$REMOTE/components" PLANAI_PORTABLE_ROOT="$REMOTE"
"$APP" >"$REMOTE/run.log" 2>&1 &
PID=$!
ok=0
for _ in $(seq 1 72); do
  if curl -sf -m3 http://127.0.0.1:11434/api/version >/dev/null 2>&1 \
     && curl -sf -m3 http://127.0.0.1:8080/health >/dev/null 2>&1; then ok=1; break; fi
  kill -0 "$PID" 2>/dev/null || { echo "app exited early"; break; }
  sleep 5
done
echo "SERVICES_OK=$ok"
kill "$PID" 2>/dev/null || true; sleep 1; pkill -f 'plan.ai' 2>/dev/null || true
echo "--- run.log tail ---"; tail -30 "$REMOTE/run.log" 2>/dev/null || true
RSH

grep -qa 'SERVICES_OK=1' /tmp/mac-run.log || die "stack did not serve on mac '$HOST' (log: /tmp/mac-run.log)"
log "mac '$HOST': ollama + Open-WebUI served OK"
