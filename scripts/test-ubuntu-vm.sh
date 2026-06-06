#!/usr/bin/env bash
# Boot an Ubuntu VM (incus, KVM-accelerated) and run the Linux AppImage inside it
# under xvfb, to prove the bundle runs on a stock Ubuntu — not just NixOS.
# Pulls a screenshot back out and asserts it rendered.
#
# Uses incus (preferred: easy to instrument). Default Ubuntu 26.04, fallback 24.04.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

UBUNTU="${UBUNTU_VERSION:-26.04}"
VM="planai-test"
APPIMAGE="$(ls -t "$DIST_DIR"/bundle/plan-ai-*-linux-*.AppImage 2>/dev/null | head -1 || true)"
SHOT="${1:-/tmp/ubuntu-dash.png}"

[ -n "$APPIMAGE" ] || die "no AppImage — run scripts/bundle.sh linux-x64 first"
command -v incus >/dev/null 2>&1 || die "incus not available"

cleanup() { incus delete -f "$VM" 2>/dev/null || true; }
trap cleanup EXIT
incus delete -f "$VM" 2>/dev/null || true

launch_vm() {
  local img="$1"
  log "incus launch $img (VM, KVM)"
  # Components MOUNT in place (no extraction to RAM/tmpfs), so modest RAM is fine.
  # 30GiB root holds the ~3GB AppImage; ${PLANAI_VM_MEM:-6GiB} RAM by default.
  incus launch "$img" "$VM" --vm \
    -c limits.cpu="${PLANAI_VM_CPU:-4}" -c limits.memory="${PLANAI_VM_MEM:-6GiB}" \
    -d root,size=30GiB 2>/dev/null
}
launch_vm "images:ubuntu/$UBUNTU/cloud" || {
  warn "ubuntu/$UBUNTU not available, falling back to 24.04"
  UBUNTU=24.04; launch_vm "images:ubuntu/$UBUNTU/cloud" || die "could not launch ubuntu VM"
}

log "wait for VM agent"
for _ in $(seq 1 60); do incus exec "$VM" -- true 2>/dev/null && break; sleep 2; done
incus exec "$VM" -- cat /etc/os-release | grep -i version= | sed 's/^/    /' || true

log "install electron + xvfb runtime deps in VM"
incus exec "$VM" -- bash -c '
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  # Electron/Chromium runtime libs. Try t64 names (ubuntu >=24.04), fall back to
  # the pre-t64 names; install best-effort and report what is still missing.
  pkgs="xvfb dbus dbus-x11 ca-certificates fuse3 fuse libfuse2t64 libnss3 libnspr4 libdrm2 libgbm1 \
    libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libxkbcommon0 libxshmfence1 \
    libpango-1.0-0 libcairo2 libcups2t64 libatk1.0-0t64 libatk-bridge2.0-0t64 \
    libatspi2.0-0t64 libgtk-3-0t64 libasound2t64 libglib2.0-0t64"
  apt-get install -y -qq $pkgs >/dev/null 2>&1 || true
  # fall back to non-t64 names for whatever did not resolve
  apt-get install -y -qq libfuse2 libcups2 libatk1.0-0 libatk-bridge2.0-0 \
    libatspi2.0-0 libgtk-3-0 libasound2 libglib2.0-0 >/dev/null 2>&1 || true
  echo "deps install attempted"
' 2>&1 | sed 's/^/    /'

log "push AppImage into VM"
incus file push "$APPIMAGE" "$VM/root/plan-ai.AppImage"
incus exec "$VM" -- chmod +x /root/plan-ai.AppImage

log "run AppImage headless under xvfb (FUSE mount; capture screenshot)"
incus exec "$VM" -- bash -c '
  set -x
  export PLANAI_CAPTURE=/root/shot.png PLANAI_CAPTURE_DELAY=50000
  export TMPDIR=/root/tmp; mkdir -p "$TMPDIR"   # disk-backed (avoid tmpfs OOM)
  modprobe fuse 2>/dev/null || true
  cd /root
  # Components MOUNT in place (squashfuse) — no big extraction. electron/chromium
  # needs a session D-Bus or it errors and fails to shut down cleanly, so wrap in
  # dbus-run-session (provides DBUS_SESSION_BUS_ADDRESS) under xvfb.
  timeout 300 xvfb-run -a -s "-screen 0 1400x900x24" \
    dbus-run-session -- ./plan-ai.AppImage --no-sandbox >/root/run.log 2>&1 || true
  echo "--- run.log tail ---"; tail -20 /root/run.log
  echo "--- free / oom ---"; free -m; dmesg 2>/dev/null | grep -iE "killed process|out of memory" | tail -3 || true
  ls -l /root/shot.png 2>/dev/null || echo "NO SCREENSHOT"
' 2>&1 | sed 's/^/    /'

# always pull the full run log to the host for diagnosis
incus file pull "$VM/root/run.log" /tmp/ubuntu-run.log 2>/dev/null || true

log "pull screenshot"
if incus file pull "$VM/root/shot.png" "$SHOT" 2>/dev/null && [ -s "$SHOT" ]; then
  log "ubuntu $UBUNTU screenshot -> $SHOT  ($(stat -c%s "$SHOT") bytes)"
else
  die "no screenshot produced in ubuntu VM (full log: /tmp/ubuntu-run.log)"
fi
