#!/usr/bin/env bash
# Tear down the preview started by preview.sh (mock + dx serve) and clean the
# generated tailwind.css.
LOG=/tmp/planai-preview
for svc in dx mock; do
  if [ -f "$LOG/$svc.pid" ]; then
    pkill -P "$(cat "$LOG/$svc.pid")" 2>/dev/null || true
    kill "$(cat "$LOG/$svc.pid")" 2>/dev/null || true
    rm -f "$LOG/$svc.pid"
  fi
done
# dx forks a child that holds the port; sweep by name as a backstop.
pkill -f "dx serve" 2>/dev/null || true
pkill -f "plan-ai-mock" 2>/dev/null || true
rm -rf "$(git rev-parse --show-toplevel)/launcher/spa-src/assets"
echo "preview stopped"
