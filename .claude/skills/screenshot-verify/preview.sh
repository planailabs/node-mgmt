#!/usr/bin/env bash
# Start the SPA preview (tailwind + mock API + dx serve) in the BACKGROUND and
# print the URL to screenshot. Unlike `make ui` (foreground, hot-reload TUI),
# this is scriptable. Run ./stop.sh to tear it down.
set -euo pipefail
ROOT="$(git rev-parse --show-toplevel)"
LOG=/tmp/planai-preview
mkdir -p "$LOG"

echo "==> tailwind"
( cd "$ROOT/launcher/spa-src" && tailwindcss \
    -i ../../third_party/plan-ai-design/assets/input.css \
    -o assets/tailwind.css --config tailwind.config.js ) >/dev/null 2>&1

echo "==> mock API (:9999)"
( cd "$ROOT" && cargo run --manifest-path mock-server/Cargo.toml ) >"$LOG/mock.log" 2>&1 &
echo $! >"$LOG/mock.pid"

echo "==> dx serve (compiling wasm, ~10-40s)"
( cd "$ROOT/launcher/spa-src" && dx serve ) >"$LOG/dx.log" 2>&1 &
echo $! >"$LOG/dx.pid"

# dx prints "Build completed" once the wasm is served.
for _ in $(seq 1 120); do grep -q "Build completed" "$LOG/dx.log" 2>/dev/null && break; sleep 2; done

# dx defaults to :8080 but falls back if taken (e.g. a real Open-WebUI). Find
# the actual port from the dx-wrapped listener.
sleep 1
PORT="$(ss -ltnp 2>/dev/null | grep -oE '127\.0\.0\.1:[0-9]+ .*dx-wrapped' | grep -oE '127\.0\.0\.1:[0-9]+' | head -1 | cut -d: -f2)"
PORT="${PORT:-8080}"
echo "PREVIEW_URL=http://127.0.0.1:${PORT}"
echo "MOCK_API=http://127.0.0.1:9999/api   (POST /api/platforms to toggle features)"
