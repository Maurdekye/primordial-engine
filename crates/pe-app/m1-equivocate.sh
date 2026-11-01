#!/usr/bin/env bash
# M1's equivocation on real peers, on one machine: 4 pe-app peers on 127.0.0.1; at 8 s member 3 signs
# a second version of its latest touch and sends it to member 1 only. The honest members hold a proof,
# vote member 3 out, and finish with identical histories. Runs on Windows (Git Bash) and Linux.
#
#   bash crates/pe-app/m1-equivocate.sh PE_APP_EXE OUT_DIR [BASE_PORT]
#
# Exit 0 when members 1, 2 and 4 are equal to each other and to their offline replays. Member 3 writes
# to OUT_DIR/out-equivocator; removed, it stops confirming and is ended by the timeout.
set -u
E="$1"; D="$2"; P="${3:-7381}"
rm -rf "$D"
"$E" init --members 4 --host 127.0.0.1 --base-port "$P" --frames 1800 --size 4 --out "$D" > /dev/null || exit 2
uname -srm 2> /dev/null || ver
for m in 1 2 3 4; do
  X=""; O="$D/out"
  [ "$m" = 3 ] && X="--equivocate-at-ms 8000 --equivocate-to 1" && O="$D/out-equivocator"
  timeout 70 "$E" run --session "$D/session.json" --key "$D/member-$m.key" --me "$m" --touches-per-s 3 $X --out "$O" -v > "$D/run-$m.log" 2>&1 &
done
wait
grep -hE "EQUIVOCAT|membership decision|CHECK" "$D"/run-*.log
"$E" compare "$D/out"
