#!/usr/bin/env bash
# E2E: the update → apply → re-exec lifecycle, against the REAL launcher binary.
#
# A temp "drive" carries the launcher + a v1 manifest; a local HTTP server offers
# v2. A fake Electron (PLANAI_ELECTRON) drives the real control API: check →
# wait Ready → apply. The launcher then tears down, applies the staged update
# onto the drive, and — because the running binary lives on the drive — re-execs
# itself; the second session's fake Electron exits immediately. One foreground
# process producing TWO sessions is the proof the re-exec happened (a failed
# exec would end the process after session one).
#
# Run: scripts/test-update-reexec.sh   (builds the debug launcher if needed)
set -euo pipefail
cd "$(dirname "$0")/.."

T=$(mktemp -d /tmp/planai-reexec-XXXXXX)
SRV_PID=""
cleanup() {
    [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null || true
    rm -rf "$T"
}
trap cleanup EXIT

DRIVE="$T/drive" CACHE="$T/cache" SRV="$T/server"
mkdir -p "$DRIVE" "$CACHE" "$SRV/files"

echo "==> building the debug launcher"
( cd launcher && PLANAI_SQUASHFUSE_LL="$(which true)" PLANAI_UNSQUASHFS="$(which true)" \
    cargo build --quiet )
cp launcher/target/debug/plan-ai "$DRIVE/plan-ai"

# v1 on the drive; v2 on the update server.
printf 'v1\n' > "$DRIVE/hello.txt"
printf 'v2\n' > "$SRV/files/hello.txt"
SHA1=$(sha256sum "$DRIVE/hello.txt" | cut -d' ' -f1)
SHA2=$(sha256sum "$SRV/files/hello.txt" | cut -d' ' -f1)
SIZE1=$(stat -c%s "$DRIVE/hello.txt")
SIZE2=$(stat -c%s "$SRV/files/hello.txt")

SRV_PORT=18931
UI_PORT=18932

manifest() { # version commit sha size
    cat <<EOF
{ "schema": 1, "product": "plan-ai", "version": "$1", "commit": "$2",
  "update_url": "http://127.0.0.1:$SRV_PORT",
  "files": [
    { "path": "hello.txt", "type": "file", "sha256": "$3", "size": $4,
      "platforms": ["linux-x64"] }
  ] }
EOF
}
manifest "1.0" "cafecafecafe" "$SHA1" "$SIZE1" > "$DRIVE/update.json"
manifest "2.0" "feedfeedfeed" "$SHA2" "$SIZE2" > "$SRV/manifest.json"
echo '{"platforms":["linux-x64"],"features":[]}' > "$DRIVE/platforms.json"

( cd "$SRV" && exec python3 -m http.server "$SRV_PORT" --bind 127.0.0.1 ) >/dev/null 2>&1 &
SRV_PID=$!

# The fake Electron: session 1 requests the update apply via the real API and
# waits to be killed; session 2 (after the re-exec) exits immediately.
RUNS="$T/runs"
cat > "$T/fake-electron" <<EOF
#!/usr/bin/env bash
n=\$(cat "$RUNS" 2>/dev/null || echo 0); n=\$((n+1)); echo \$n > "$RUNS"
[ "\$n" -ge 2 ] && exit 0
B="\$PLANAI_UI_URL"
curl -s -X POST "\$B/api/update/check" >/dev/null
for i in \$(seq 1 120); do
    case "\$(curl -s "\$B/api/update/status")" in *'"state":"ready"'*) break;; esac
    sleep 0.5
done
curl -s -X POST "\$B/api/update/apply" >/dev/null
sleep 60 # the launcher kills us to run the apply
EOF
chmod +x "$T/fake-electron"

echo "==> running the launcher (session 1 requests the apply; session 2 is the re-exec)"
set +e
env -i PATH="$PATH" HOME="$HOME" \
    PLANAI_PORTABLE_ROOT="$DRIVE" PLANAI_CACHE="$CACHE" \
    PLANAI_ELECTRON="$T/fake-electron" PLANAI_UI_PORT="$UI_PORT" PLANAI_DEV=1 \
    timeout 180 "$DRIVE/plan-ai" > "$T/launcher.log" 2>&1
CODE=$?
set -e

fail() { echo "FAIL: $1"; echo "--- launcher log ---"; tail -50 "$T/launcher.log"; exit 1; }

[ "$CODE" -eq 0 ] || fail "launcher exited $CODE (timeout = re-exec loop or hang)"
[ "$(cat "$RUNS" 2>/dev/null)" = "2" ] || fail "expected 2 sessions in one process (re-exec), got '$(cat "$RUNS" 2>/dev/null)'"
[ "$(cat "$DRIVE/hello.txt")" = "v2" ] || fail "hello.txt not updated to v2"
grep -q '"version": "2.0"' "$DRIVE/update.json" || fail "update.json not committed to 2.0"
[ ! -f "$DRIVE/.update-pending.json" ] || fail "pendrive marker not cleared after apply"
grep -q "lifecycle: Relaunch" "$T/launcher.log" || fail "Relaunch phase never reached"

echo "OK: update applied, marker cleared, and the launcher re-exec'd into the new session"
