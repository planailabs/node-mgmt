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
  incus launch "$img" "$VM" --vm -c limits.cpu=4 -c limits.memory=6GiB 2>/dev/null
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
  apt-get install -y -qq xvfb libfuse2t64 libnss3 libgtk-3-0t64 libasound2t64 \
    libgbm1 libdrm2 ca-certificates >/dev/null 2>&1 || \
  apt-get install -y -qq xvfb libfuse2 libnss3 libgtk-3-0 libasound2 libgbm1 libdrm2 ca-certificates >/dev/null 2>&1
  echo "deps installed"
' 2>&1 | sed 's/^/    /'

log "push AppImage into VM"
incus file push "$APPIMAGE" "$VM/root/plan-ai.AppImage"
incus exec "$VM" -- chmod +x /root/plan-ai.AppImage

log "run AppImage headless under xvfb (extract-and-run; capture screenshot)"
incus exec "$VM" -- bash -c '
  export PLANAI_CAPTURE=/root/shot.png PLANAI_CAPTURE_DELAY=45000
  cd /root
  timeout 160 xvfb-run -a -s "-screen 0 1400x900x24" \
    ./plan-ai.AppImage --appimage-extract-and-run --no-sandbox >/root/run.log 2>&1 || true
  echo "--- run.log tail ---"; tail -8 /root/run.log
  ls -l /root/shot.png 2>/dev/null || echo "NO SCREENSHOT"
' 2>&1 | sed 's/^/    /'

log "pull screenshot"
if incus file pull "$VM/root/shot.png" "$SHOT" 2>/dev/null && [ -s "$SHOT" ]; then
  log "ubuntu $UBUNTU screenshot -> $SHOT  ($(stat -c%s "$SHOT") bytes)"
else
  die "no screenshot produced in ubuntu VM (see run.log above)"
fi
