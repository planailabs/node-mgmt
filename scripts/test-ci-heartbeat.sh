#!/usr/bin/env bash
# Tests for scripts/ci-heartbeat.sh — the wrapper that keeps `update-tarball`
# alive on GitLab while ninja sits on a long, silent dmg edge.
#
# GitLab's StuckCiJobsWorker drops a running job with no activity as
# `no_updates_running` after 30 minutes, so the wrapper must (a) emit a status
# line per interval while the child is quiet, (b) pass the child's output and
# exit status through unchanged, and (c) tear the child down on SIGTERM instead
# of waiting out the interval. No Nix, no network: a few `sleep`s.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
HB="$ROOT/scripts/ci-heartbeat.sh"

PASS=0; FAIL=0
ok()  { printf '\033[1;32m  PASS\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad() { printf '\033[1;31m  FAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/hermes-ci-heartbeat-XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/stamps"
# Distinct mtimes: the wrapper reports the newest stamp, and two files written
# in the same instant would make "newest" arbitrary.
touch -t 202601010101 "$tmp/stamps/comp-one.stamp"
: > "$tmp/stamps/comp-two.stamp"

# A fast child must not produce any tick, and its output/status must pass through.
out=$("$HB" --interval 5 --watch "$tmp/stamps" -- bash -c 'echo child-line; exit 0')
if [ "$(printf '%s\n' "$out" | grep -c '^child-line$')" = 1 ] && \
   [ "$(printf '%s\n' "$out" | grep -c '\[heartbeat\] elapsed=')" = 0 ]; then
  ok "fast child: output passed through, no tick"
else
  bad "fast child: unexpected output: $out"
fi

# A quiet child longer than the interval must tick, and report node progress.
out=$("$HB" --interval 1 --watch "$tmp/stamps" -- bash -c 'sleep 3')
ticks=$(printf '%s\n' "$out" | grep -c '\[heartbeat\] elapsed=' || true)
if [ "$ticks" -ge 2 ] && printf '%s\n' "$out" | grep -q 'nodes=2 last=comp-two'; then
  ok "quiet child: $ticks ticks, node progress reported"
else
  bad "quiet child: expected >=2 ticks with nodes=2, got: $out"
fi

# A failing child's status must be the wrapper's status (GitLab reads it).
set +e
"$HB" --interval 5 --watch "$tmp/stamps" -- bash -c 'exit 7' > "$tmp/fail.log" 2>&1
rc=$?
set -e
if [ "$rc" = 7 ] && grep -q 'command exited rc=7' "$tmp/fail.log"; then
  ok "failing child: rc=7 propagated and reported"
else
  bad "failing child: expected rc=7 with a report line, got rc=$rc: $(cat "$tmp/fail.log")"
fi

# SIGTERM must stop the child rather than letting the interval run out: the
# marker the child would write after 5s must never appear.
marker="$tmp/child-survived"
"$HB" --interval 5 --watch "$tmp/stamps" -- bash -c "sleep 5; : > '$marker'" > "$tmp/term.log" 2>&1 &
hb=$!
sleep 1
kill -TERM "$hb" 2>/dev/null || true
set +e
wait "$hb"; term_rc=$?
set -e
sleep 5
if [ "$term_rc" = 143 ] && [ ! -e "$marker" ]; then
  ok "SIGTERM: wrapper exited 143 and killed the child"
else
  bad "SIGTERM: rc=$term_rc marker=$([ -e "$marker" ] && echo present || echo absent)"
fi

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" = 0 ]
