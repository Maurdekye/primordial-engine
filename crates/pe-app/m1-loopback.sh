#!/usr/bin/env bash
# The M1 scenario on one machine: 4 pe-app peers on 127.0.0.1, scripted loss and delay on members 2
# and 3, member 4 dropping out at 6 s, then `pe-app compare`. Runs on Windows (Git Bash) and Linux.
#
#   bash crates/pe-app/m1-loopback.sh PE_APP_EXE OUT_DIR [BASE_PORT]
#
# Exit 0 when the surviving members' histories are identical and equal their offline replays.
set -u
E="$1"; D="$2"; P="${3:-7271}"
rm -rf "$D"
"$E" init --members 4 --host 127.0.0.1 --base-port "$P" --frames 1200 --size 4 --out "$D" > /dev/null || exit 2
cat > "$D/faults-2.json" <<'EOF'
[{"at_ms": 2000, "loss_pct": 15, "delay_ms": 40, "jitter_ms": 15},
 {"at_ms": 9000, "to": 1, "loss_pct": 30, "delay_ms": 120, "jitter_ms": 40},
 {"at_ms": 14000, "loss_pct": 0, "delay_ms": 0}]
EOF
echo '[{"at_ms": 1000, "loss_pct": 5, "delay_ms": 25, "jitter_ms": 10}]' > "$D/faults-3.json"
uname -srm 2> /dev/null || ver
for m in 1 2 3 4; do
  X=""
  [ -f "$D/faults-$m.json" ] && X="--faults $D/faults-$m.json"
  [ "$m" = 4 ] && X="--crash-at-ms 6000"
  timeout 90 "$E" run --session "$D/session.json" --key "$D/member-$m.key" --me "$m" --touches-per-s 3 $X --out "$D/out" -v > "$D/run-$m.log" 2>&1 &
done
wait
grep -hE "fault:|drop-out|membership|DONE|CHECK|slow step" "$D"/run-*.log
"$E" compare "$D/out"
