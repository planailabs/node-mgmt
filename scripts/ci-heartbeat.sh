#!/usr/bin/env bash
# Keep a long, quiet CI step visible to GitLab.
#
# GitLab drops a *running* job as `no_updates_running` once it sees no activity
# for 30 minutes (docs: "Job dropped after being stuck or exceeding its
# timeout"). `update-tarball` runs `make bundles`, i.e. ninja, which buffers
# every edge's output until that edge finishes, and the mac dmg packers are
# serialised into `[pools] dmg = 1` (loader.toml) because each VM packer needs
# several GiB. A single `runtime-mac-arm64` dmg edge therefore runs for tens of
# minutes with *nothing* on the trace, and the whole job can be silent for much
# longer than the threshold while it is working perfectly:
#
#   job 8750 (pipeline 3508): last trace line 01:28:02 (`runtime linux-x64`,
#   node 18/43), job dropped at 04:15:08 as `no_updates_running` — 2h47m of
#   silence, 8h cap untouched, no error anywhere.
#
# This wrapper runs the command unchanged and prints one status line per
# interval, so the trace keeps moving and the next failure is attributable to a
# node and an elapsed time instead of a wall of silence.
#
# Usage: scripts/ci-heartbeat.sh [--interval SECONDS] [--watch DIR] -- CMD [ARG...]
#
#   --interval  seconds between status lines (default 60)
#   --watch     directory whose *.stamp files mark completed nodes
#               (default dist/.ninja; skipped when absent)
#
# Exit status is the command's own.
set -euo pipefail

interval=60
watch=dist/.ninja
while [ $# -gt 0 ]; do
  case "$1" in
    --interval) interval=${2:?--interval needs a value}; shift 2 ;;
    --watch)    watch=${2:?--watch needs a value}; shift 2 ;;
    --)         shift; break ;;
    *)          break ;;
  esac
done
if [ $# -eq 0 ]; then
  echo "usage: $0 [--interval SECONDS] [--watch DIR] -- CMD [ARG...]" >&2
  exit 2
fi

started=$SECONDS
child=

elapsed() {
  local s=$(( SECONDS - started ))
  printf '%dm%02ds' $(( s / 60 )) $(( s % 60 ))
}

# Memory view for the container ceiling (cgroup v2, then v1); the CI runner is a
# cgroup with no swap, so `memory.current/max` is the number that killed the
# earlier packers. Falls back to nothing on hosts without either file.
memory() {
  local cur max
  if [ -r /sys/fs/cgroup/memory.current ] && [ -r /sys/fs/cgroup/memory.max ]; then
    cur=$(cat /sys/fs/cgroup/memory.current 2>/dev/null || echo 0)
    max=$(cat /sys/fs/cgroup/memory.max 2>/dev/null || echo 0)
  elif [ -r /sys/fs/cgroup/memory/memory.usage_in_bytes ]; then
    cur=$(cat /sys/fs/cgroup/memory/memory.usage_in_bytes 2>/dev/null || echo 0)
    max=$(cat /sys/fs/cgroup/memory/memory.limit_in_bytes 2>/dev/null || echo 0)
  else
    return 0
  fi
  case "$max" in ''|*[!0-9]*) max=0 ;; esac
  if [ "$max" -gt 0 ]; then
    printf ' mem=%d/%dMiB' $(( cur / 1048576 )) $(( max / 1048576 ))
  else
    printf ' mem=%dMiB' $(( cur / 1048576 ))
  fi
}

# Progress = completed nodes. Ninja renames the stamp in only when the edge
# succeeded, so the count and the newest name are the real "what finished last".
progress() {
  [ -d "$watch" ] || return 0
  local count newest
  count=$(find "$watch" -maxdepth 1 -name '*.stamp' 2>/dev/null | wc -l)
  newest=$(ls -t "$watch"/*.stamp 2>/dev/null | head -1 || true)
  [ -n "${newest:-}" ] || { printf ' nodes=%d' "$count"; return 0; }
  newest=${newest##*/}
  printf ' nodes=%d last=%s' "$count" "${newest%.stamp}"
}

disk() {
  local free
  free=$(df -Pk . 2>/dev/null | awk 'NR==2 { print int($4 / 1048576) }' || true)
  [ -n "${free:-}" ] && printf ' disk_free=%sGiB' "$free"
  return 0
}

tick() {
  printf '[heartbeat] elapsed=%s' "$(elapsed)"
  progress
  memory
  disk
  printf '\n'
}

cleanup() {
  if [ -n "$child" ] && kill -0 "$child" 2>/dev/null; then
    kill -TERM "$child" 2>/dev/null || true
    wait "$child" 2>/dev/null || true
  fi
}
trap cleanup EXIT
trap 'cleanup; exit 143' TERM INT

printf '[heartbeat] start after %s: %s\n' "$(elapsed)" "$*"
"$@" &
child=$!

# Sleep in one-second slices so a cancelled job tears down promptly instead of
# sitting out the rest of the interval, and so ticks stop the moment the child
# exits.
rc=0
while kill -0 "$child" 2>/dev/null; do
  i=0
  while [ "$i" -lt "$interval" ]; do
    kill -0 "$child" 2>/dev/null || break
    sleep 1
    i=$((i + 1))
  done
  kill -0 "$child" 2>/dev/null && tick
done
wait "$child" || rc=$?
child=
printf '[heartbeat] command exited rc=%d after %s\n' "$rc" "$(elapsed)"
exit "$rc"
