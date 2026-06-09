#!/usr/bin/env bash
# Run the Windows launcher on a REAL remote Windows machine (ssh $WIN_TARGET) —
# pushes plan-ai.exe + the win/shared components (pre-extracted dirs), runs the
# launcher pointed at the pushed pool, and asserts the stack SERVES (ollama +
# Open-WebUI health). Mirrors test-ubuntu-vm / test-mac for Windows.
#
# Requires: WIN_TARGET = the ssh host of a Windows box (OpenSSH) with a LOGGED-IN
# session (Electron needs the desktop). Remote commands run via PowerShell.
#
# Two robustness measures, learned the hard way against a real box:
#  * SSH keepalive — a single 6-min health-poll session is otherwise reset by the
#    win sshd / a NAT middlebox under the cold-start IO load, losing the result.
#  * The supervisor runs from a .ps1 via `powershell -File` (NOT `-Command -` on
#    stdin, which silently fails to execute a long multi-line script), and writes
#    its verdict to a result file we read back as the source of truth — the long
#    session's own stdout is unreliably buffered.
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

# Keepalive so the long cold-start session isn't reset mid-poll.
SSH_OPTS=(-o ServerAliveInterval=15 -o ServerAliveCountMax=8)
ps() { ssh "${SSH_OPTS[@]}" "$HOST" powershell -NoProfile -NonInteractive -Command -; }  # script on stdin

log "remote windows test on '$HOST'"
# Make a temp dir; return it forward-slashed (scp + the launcher accept '/').
REMOTE="$(printf '%s' '$d = Join-Path $env:TEMP ("planai-" + [guid]::NewGuid().ToString("N")); New-Item -ItemType Directory -Path $d | Out-Null; ($d -replace "\\","/")' | ps | tr -d "\r")"
[ -n "$REMOTE" ] || die "ssh $HOST failed (no remote temp dir)"
cleanup() {
  printf '%s' "Get-Process plan-ai,plan.ai,ollama -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue; Remove-Item -Recurse -Force '$REMOTE' -ErrorAction SilentlyContinue" | ps >/dev/null 2>&1 || true
}
trap cleanup EXIT
log "remote workdir: $HOST:$REMOTE"

# push the launcher + this OS's component group (components/win/, pre-extracted dirs).
ssh "${SSH_OPTS[@]}" "$HOST" "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force '$REMOTE/components' | Out-Null\""
scp "${SSH_OPTS[@]}" -q "$EXE" "$HOST:$REMOTE/plan-ai.exe"
[ -d "$POOL/win" ] || die "no components/win group in $POOL — run scripts/bundle.sh win-x64"
scp "${SSH_OPTS[@]}" -q -r "$POOL/win" "$HOST:$REMOTE/components/" || true

# Supervisor script: starts the launcher, polls health, writes a result file. Shipped
# as a real .ps1 and run with -File so PowerShell actually executes the whole thing.
SUP="$(mktemp /tmp/planai-win-supervise.XXXXXX.ps1)"
cat >"$SUP" <<'PS'
param([string]$RD)
$env:PLANAI_COMPONENTS    = "$RD/components"
$env:PLANAI_PORTABLE_ROOT = "$RD"
Get-Process plan-ai,plan.ai,ollama,python -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-Item "$RD/result.txt" -ErrorAction SilentlyContinue
$p = Start-Process -FilePath "$RD/plan-ai.exe" -PassThru -WindowStyle Minimized `
        -RedirectStandardError "$RD/err.log" -RedirectStandardOutput "$RD/out.log"
$ok = 0
foreach ($i in 1..72) {
  try {
    Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 http://127.0.0.1:11434/api/version | Out-Null
    Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 http://127.0.0.1:8080/health     | Out-Null
    $ok = 1; break
  } catch {}
  if ($p.HasExited) { break }
  Start-Sleep -Seconds 5
}
"SERVICES_OK=$ok" | Out-File -Encoding ascii "$RD/result.txt"
Write-Output "SERVICES_OK=$ok"
try { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } catch {}
Get-Process plan-ai,plan.ai,ollama -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
PS
scp "${SSH_OPTS[@]}" -q "$SUP" "$HOST:$REMOTE/supervise.ps1"
rm -f "$SUP"

log "run launcher; assert ollama + Open-WebUI serve (≤6min cold start)"
ssh "${SSH_OPTS[@]}" "$HOST" "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"$REMOTE/supervise.ps1\" \"$REMOTE\"" 2>/dev/null | tr -d '\r' | tee /tmp/win-run.log | sed 's/^/    /' || true

# The result file is the source of truth (the live session's stdout can be lost).
RESULT="$(ssh "${SSH_OPTS[@]}" "$HOST" "powershell -NoProfile -Command \"Get-Content '$REMOTE/result.txt' -ErrorAction SilentlyContinue\"" 2>/dev/null | tr -d '\r')"
echo "$RESULT" | grep -qa 'SERVICES_OK=1' || {
  ssh "${SSH_OPTS[@]}" "$HOST" "powershell -NoProfile -Command \"Get-Content '$REMOTE/err.log' -ErrorAction SilentlyContinue\"" 2>/dev/null | tr -d '\r' | sed 's/^/    err: /' || true
  die "stack did not serve on windows '$HOST' (result: '$RESULT', log: /tmp/win-run.log)"
}
log "windows '$HOST': ollama + Open-WebUI served OK"
