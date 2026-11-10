#!/usr/bin/env bash
# R18 over a capped link (design 10.9): 3 pe-app peers on 127.0.0.1 in a ball pit, member 1 sweeping
# its hand every frame, 25 +-5 ms one-way on every link, and from 2 s a bandwidth cap on member 1's
# uplinks (token bucket, 4 kB burst, 500 ms tail-drop queue, or CoDel with CODEL=1), then
# `pe-app compare`. Runs on Windows (Git Bash) and Linux.
#
#   bash crates/pe-app/r18-capped.sh PE_APP_EXE OUT_DIR CAP_KBPS [on|off] [BASE_PORT] [FRAMES]
#
# on (default): the link signal feeds the controller; off: --no-link-signal (level stays 0).
# Each member writes out/link-<id>.csv (level, signal, the cap's real queue, kbit/s, confirmation
# lag, every 250 ms). Exit 0 when every history is identical and equals its offline replay.
set -u
E="$1"; D="$2"; CAP="$3"; SIG="${4:-on}"; P="${5:-7401}"; F="${6:-1800}"
CODEL=$([ "${CODEL:-0}" = 1 ] && echo true || echo false)
rm -rf "$D"
"$E" init --members 3 --host 127.0.0.1 --base-port "$P" --frames "$F" --scene ballpit --size 500 --out "$D" > /dev/null || exit 2
echo "[{\"at_ms\": 0, \"delay_ms\": 25, \"jitter_ms\": 10}, {\"at_ms\": 2000, \"delay_ms\": 25, \"jitter_ms\": 10, \"rate_kbps\": $CAP, \"queue_ms\": 500, \"codel\": $CODEL}]" > "$D/faults-1.json"
echo '[{"at_ms": 0, "delay_ms": 25, "jitter_ms": 10}]' > "$D/faults-other.json"
X=""; [ "$SIG" = off ] && X="--no-link-signal"
uname -srm 2> /dev/null || ver
for m in 1 2 3; do
  FF="$D/faults-other.json"; H=""
  [ "$m" = 1 ] && { FF="$D/faults-1.json"; H="--hand-sweep"; }
  timeout $(( F / 60 + 60 )) "$E" run --session "$D/session.json" --key "$D/member-$m.key" --me "$m" --faults "$FF" $H $X --out "$D/out" -v > "$D/run-$m.log" 2>&1 &
done
wait
grep -hE "fault:|membership|DONE|CHECK|slow step|paused" "$D"/run-*.log | head -20
"$E" compare "$D/out"
