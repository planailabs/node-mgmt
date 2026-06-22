#!/usr/bin/env bash
# Run the Linux launcher (plan-ai.linux-x64.exe — the real shipped artifact, static
# musl) inside a stock Ubuntu instance (incus) under xvfb, to prove the bundle
# runs on Ubuntu — not just NixOS. Asserts the stack SERVES
# (ollama + Open-WebUI health); screenshot is best-effort (headless render is
# flaky). Default Ubuntu 26.04 (override UBUNTU_VERSION).
#
# Uses a privileged CONTAINER with /dev/fuse rather than a KVM VM: VM creation
# stalls for minutes on busy hosts, while containers launch in seconds and still
# exercise the real squashfuse mount path.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

UBUNTU="${UBUNTU_VERSION:-26.04}"
# unique per run so concurrent test runs don't collide on the instance name
VM="${PLANAI_VM_NAME:-planai-test-$$-${RANDOM}}"
LAUNCHER="$DIST_DIR/bundle/plan-ai.linux-x64.exe"
SHOT="${1:-/tmp/ubuntu-dash.png}"

[ -f "$LAUNCHER" ] || die "no plan-ai.linux-x64.exe — run make bundle TARGET=linux-x64 first"
command -v incus >/dev/null 2>&1 || die "incus not available"

# --ephemeral so the instance self-destructs if the run is killed before cleanup;
# the trap also force-deletes this run's own VM (never a sibling concurrent run).
cleanup() { incus delete -f "$VM" 2>/dev/null || true; }
trap cleanup EXIT
log "test VM: $VM"

# We use a privileged system CONTAINER (not a KVM VM): on this host VM creation
# intermittently stalls for minutes, while containers launch in seconds. A
# privileged container with /dev/fuse passed through still exercises the real
# squashfuse MOUNT path (and the AppImage's own FUSE), so it's a faithful "runs
# on stock Ubuntu" check — just far more reliable.
launch_ctr() {
  local img="$1"
  timeout -k 10 -s KILL "${PLANAI_VM_LAUNCH_TIMEOUT:-120}" incus launch "$img" "$VM" --ephemeral \
    -c security.privileged=true -c security.nesting=true \
    -c limits.cpu="${PLANAI_VM_CPU:-4}" -c limits.memory="${PLANAI_VM_MEM:-6GiB}" 2>/dev/null || return 1
  # hot-plug /dev/fuse so squashfuse_ll + the AppImage can mount
  incus config device add "$VM" fuse unix-char source=/dev/fuse path=/dev/fuse 2>/dev/null || true
}
# This host intermittently STALLS instance creation for minutes (heavy IO), but a
# good launch finishes in ~25s. Use a short per-attempt timeout + retries: kill a
# stalled launch, clean the partial instance, retry.
launch_with_retry() {
  local img="$1" n="${PLANAI_VM_LAUNCH_RETRIES:-5}" i
  for i in $(seq 1 "$n"); do
    log "incus launch $img (privileged container + /dev/fuse) — attempt $i/$n"
    launch_ctr "$img" && return 0
    warn "launch attempt $i/$n stalled/failed; cleaning up + retrying"
    incus delete -f "$VM" 2>/dev/null || true
    sleep 5
  done
  return 1
}
# Prefer a locally CACHED container image for the codename (the
# images:linuxcontainers.org remote is sometimes slow); else fetch from remote.
cached_ctr_image() {
  local codename; case "$UBUNTU" in
    26.04) codename=resolute ;; 24.04) codename=noble ;; 22.04) codename=jammy ;; *) return 1 ;;
  esac
  incus image list --format csv -c f,d,t 2>/dev/null \
    | awk -F, -v c="$codename" 'tolower($2) ~ c && $3 ~ /CONTAINER/ {print $1; exit}'
}
FP="$(cached_ctr_image || true)"
if [ -n "${FP:-}" ]; then
  log "using cached $UBUNTU container image $FP"
  launch_with_retry "$FP" || die "launch from cached image $FP failed after retries"
else
  launch_with_retry "images:ubuntu/$UBUNTU" || die "could not launch ubuntu $UBUNTU container"
fi

log "wait for VM agent"
for _ in $(seq 1 60); do incus exec "$VM" -- true 2>/dev/null && break; sleep 2; done
incus exec "$VM" -- cat /etc/os-release | grep -i version= | sed 's/^/    /' || true

log "install electron + xvfb runtime deps in VM"
incus exec "$VM" -- bash -c '
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  # Electron/Chromium runtime libs. Try t64 names (ubuntu >=24.04), fall back to
  # the pre-t64 names; install best-effort and report what is still missing.
  pkgs="xvfb dbus dbus-x11 at-spi2-core curl ca-certificates fuse3 fuse libfuse2t64 libnss3 libnspr4 libdrm2 libgbm1 \
    libgl1 libglx-mesa0 libgl1-mesa-dri libegl1 \
    libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libxkbcommon0 libxshmfence1 \
    libpango-1.0-0 libcairo2 libcups2t64 libatk1.0-0t64 libatk-bridge2.0-0t64 \
    libatspi2.0-0t64 libgtk-3-0t64 libasound2t64 libglib2.0-0t64"
  apt-get install -y -qq $pkgs >/dev/null 2>&1 || true
  # fall back to non-t64 names for whatever did not resolve
  apt-get install -y -qq libfuse2 libcups2 libatk1.0-0 libatk-bridge2.0-0 \
    libatspi2.0-0 libgtk-3-0 libasound2 libglib2.0-0 >/dev/null 2>&1 || true
  echo "deps install attempted"
' 2>&1 | sed 's/^/    /'

log "push the launcher + shared components/ + tools/ into VM (siblings, as on the USB)"
incus file push "$LAUNCHER" "$VM/root/plan-ai.linux-x64.exe"
incus exec "$VM" -- chmod +x /root/plan-ai.linux-x64.exe
# components ship OUTSIDE the launcher; it finds them next to itself (here/parent).
# Push the shared pool built by bundle.sh.
POOL="$DIST_DIR/bundle/components"; TOOLS="$DIST_DIR/bundle/tools"
[ -d "$POOL" ] || die "no shared components pool at $POOL — run make bundle TARGET=linux-x64"
incus file push -r "$POOL" "$VM/root/" 2>/dev/null
[ -d "$TOOLS" ] && incus file push -r "$TOOLS" "$VM/root/" 2>/dev/null || true
incus exec "$VM" -- bash -c 'chmod +x /root/tools/bin/* 2>/dev/null; ls /root/components/linux-x64/*.squashfs >/dev/null 2>&1 && echo "components staged beside launcher" || echo "WARN no components"'

log "run the launcher on stock Ubuntu; assert the stack serves (screenshot best-effort)"
# What proves "runs on Ubuntu": ollama + Open-WebUI actually serving. These are
# child processes the Electron MAIN process spawns/supervises — independent of the
# renderer, which can't reliably paint under headless xvfb (chromium renderer-IPC
# quirk: "Terminating ... no connection"). So we assert HTTP health, not pixels,
# and capture a screenshot only opportunistically.
incus exec "$VM" -- bash -c '
  set -x
  # NOTE: do NOT set PLANAI_CAPTURE here — the capture hook calls app.quit() after
  # its delay, which would kill the app before Open-WebUI (~20s cold start) is
  # ready. We assert health instead and screenshot is dropped (headless render is
  # unreliable anyway).
  export TMPDIR=/root/tmp; mkdir -p "$TMPDIR"
  export NO_AT_BRIDGE=1 GTK_A11Y=none ELECTRON_ENABLE_LOGGING=1 LIBGL_ALWAYS_SOFTWARE=1
  modprobe fuse 2>/dev/null || true
  cd /root
  # --disable-gpu forces software compositing (no GPU/DRI in the VM);
  # dbus-run-session gives a session bus. The renderer may still not paint under
  # headless xvfb, but the MAIN process spawns/supervises ollama + uvicorn anyway.
  xvfb-run -a -s "-screen 0 1400x900x24" \
    dbus-run-session -- ./plan-ai.linux-x64.exe --no-sandbox --disable-gpu --disable-dev-shm-usage \
    >/root/run.log 2>&1 &
  APP=$!
  ok=""
  for i in $(seq 1 72); do   # up to ~6min for first cold start (ollama + uvicorn)
    if curl -sf -m 3 http://127.0.0.1:11434/api/version >/dev/null 2>&1 \
       && curl -sf -m 3 http://127.0.0.1:8080/health >/dev/null 2>&1; then ok=1; break; fi
    kill -0 "$APP" 2>/dev/null || { echo "app exited early"; break; }
    sleep 5
  done
  echo "SERVICES_OK=${ok:-0}" | tee -a /root/run.log   # marker pulled to the host
  curl -s -m 3 http://127.0.0.1:11434/api/version 2>/dev/null | head -c 200; echo
  sleep 2   # give the capture hook a chance if the window did paint
  echo "--- run.log tail ---"; tail -30 /root/run.log
  kill "$APP" 2>/dev/null; sleep 1; pkill -f plan-ai 2>/dev/null || true
  ls -l /root/shot.png 2>/dev/null || echo "no screenshot (best-effort)"
  true   # teardown kill must not propagate a non-zero exit
' 2>&1 | sed 's/^/    /' || true

incus file pull "$VM/root/run.log" /tmp/ubuntu-run.log 2>/dev/null || true
incus file pull "$VM/root/shot.png" "$SHOT" 2>/dev/null && [ -s "$SHOT" ] \
  && log "ubuntu $UBUNTU screenshot -> $SHOT ($(stat -c%s "$SHOT") bytes)" \
  || warn "no screenshot (headless render; non-fatal)"

# success criterion: the services served on stock Ubuntu
SVC="$(grep -oa 'SERVICES_OK=[01]' /tmp/ubuntu-run.log 2>/dev/null | tail -1)"
[ "$SVC" = "SERVICES_OK=1" ] || die "stack did not serve on ubuntu $UBUNTU (full log: /tmp/ubuntu-run.log)"
log "ubuntu $UBUNTU: ollama + Open-WebUI served OK"
