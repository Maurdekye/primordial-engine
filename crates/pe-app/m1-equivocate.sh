#!/usr/bin/env bash
# M1's equivocation on real peers, on one machine: 4 pe-app peers on 127.0.0.1, every one logging
# member 3's touches (--watch-member 3). At 8 s member 3 equivocates:
#   late  (default) it signs a second version of its latest touch, already sent (and confirmed), and
#         sends it to member 1 only;
#   split it authors a fresh touch and sends version A to members 1 and 2, version B (the opposite
#         impulse) to member 4, before anyone has confirmed it.
# The honest members hold a proof, vote member 3 out, and finish with identical histories. Runs on
# Windows (Git Bash) and Linux.
#
#   bash crates/pe-app/m1-equivocate.sh PE_APP_EXE OUT_DIR [BASE_PORT] [late|split]
#
# Exit 0 when members 1, 2 and 4 are equal to each other and to their offline replays. Member 3 writes
# to OUT_DIR/out-equivocator; removed, it stops confirming and is ended by the timeout.
set -u
E="$1"; D="$2"; P="${3:-7381}"; MODE="${4:-late}"
rm -rf "$D"
"$E" init --members 4 --host 127.0.0.1 --base-port "$P" --frames 1800 --size 4 --out "$D" > /dev/null || exit 2
uname -srm 2> /dev/null || ver
for m in 1 2 3 4; do
  X=""; O="$D/out"
  if [ "$m" = 3 ]; then
    O="$D/out-equivocator"
    if [ "$MODE" = split ]; then X="--equivocate-split 8000:4"; else X="--equivocate-at-ms 8000 --equivocate-to 1"; fi
  fi
  timeout 70 "$E" run --session "$D/session.json" --key "$D/member-$m.key" --me "$m" --touches-per-s 3 --watch-member 3 $X --out "$O" -v > "$D/run-$m.log" 2>&1 &
done
wait
grep -hE "EQUIVOCAT|membership decision|CHECK|FINAL" "$D"/run-*.log
"$E" compare "$D/out"
