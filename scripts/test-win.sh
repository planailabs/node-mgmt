#!/usr/bin/env bash
# Run the Windows launcher on a REAL remote Windows machine (ssh $WIN_TARGET) —
# pushes plan-ai.exe + the win/shared components (pre-extracted dirs), runs the
# launcher pointed at the pushed pool, and asserts the stack SERVES (ollama +
# Open-WebUI health). Mirrors test-ubuntu-vm / test-mac for Windows.
#
# Requires: WIN_TARGET = the ssh host of a Windows box (OpenSSH) with a LOGGED-IN
# session (Electron needs the desktop). Remote commands run via PowerShell.
#
# Usage: WIN_TARGET=win scripts/test-win.sh
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

HOST="${WIN_TARGET:?set WIN_TARGET to the ssh host of a windows box (e.g. WIN_TARGET=win)}"
need ssh; need scp
EXE="$DIST_DIR/bundle/plan-ai.exe"
POOL="$DIST_DIR/bundle/components"
[ -f "$EXE" ]  || die "no plan-ai.exe — run scripts/bundle.sh win-x64 first"
[ -d "$POOL" ] || die "no components pool — run scripts/bundle.sh win-x64 first"

ps() { ssh "$HOST" powershell -NoProfile -NonInteractive -Command -; }  # script on stdin

log "remote windows test on '$HOST'"
# Make a temp dir; return it forward-slashed (scp + the launcher accept '/').
REMOTE="$(printf '%s' '$d = Join-Path $env:TEMP ("planai-" + [guid]::NewGuid().ToString("N")); New-Item -ItemType Directory -Path $d | Out-Null; ($d -replace "\\","/")' | ps | tr -d "\r")"
[ -n "$REMOTE" ] || die "ssh $HOST failed (no remote temp dir)"
cleanup() { printf '%s' "Remove-Item -Recurse -Force '$REMOTE' -ErrorAction SilentlyContinue" | ps >/dev/null 2>&1 || true; }
trap cleanup EXIT
log "remote workdir: $HOST:$REMOTE"

# push the launcher + the win/shared components (dirs are pre-extracted on win).
ssh "$HOST" "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force '$REMOTE/components' | Out-Null\""
scp -q "$EXE" "$HOST:$REMOTE/plan-ai.exe"
for f in manifest.json llmfit-windows.exe; do
  [ -e "$POOL/$f" ] && scp -q "$POOL/$f" "$HOST:$REMOTE/components/$f" || true
done
for d in app-win-x64 runtime-win-x64 ollama-windows-amd64 ow-assets; do
  [ -d "$POOL/$d" ] && scp -q -r "$POOL/$d" "$HOST:$REMOTE/components/" || true
done

log "run launcher; assert ollama + Open-WebUI serve (≤6min cold start)"
# Prepend a $REMOTE definition (bash-interpolated), then the PowerShell body
# (single-quoted heredoc — its $vars stay PowerShell's).
{
  printf "%s\r\n" "\$REMOTE = '$REMOTE'"
  cat <<'PS'
$env:PLANAI_COMPONENTS = "$REMOTE/components"
$env:PLANAI_PORTABLE_ROOT = "$REMOTE"
$p = Start-Process -FilePath "$REMOTE/plan-ai.exe" -PassThru -WindowStyle Minimized
$ok = 0
foreach ($i in 1..72) {
  try {
    Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 http://127.0.0.1:11434/api/version | Out-Null
    Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 http://127.0.0.1:8080/health | Out-Null
    $ok = 1; break
  } catch { }
  if ($p.HasExited) { Write-Output "app exited early"; break }
  Start-Sleep -Seconds 5
}
Write-Output "SERVICES_OK=$ok"
try { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } catch { }
Get-Process plan-ai,plan.ai -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
PS
} | ps | tr -d '\r' | tee /tmp/win-run.log | sed 's/^/    /' || true

grep -qa 'SERVICES_OK=1' /tmp/win-run.log || die "stack did not serve on windows '$HOST' (log: /tmp/win-run.log)"
log "windows '$HOST': ollama + Open-WebUI served OK"
