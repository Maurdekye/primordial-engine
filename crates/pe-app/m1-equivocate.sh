#!/usr/bin/env bash
# M1's equivocation on real peers, on one machine: 4 pe-app peers on 127.0.0.1, every one logging
# member 3's touches (--watch-member 3). At 8 s member 3 equivocates:
#   late  (default) it signs a second version of its latest touch, already sent (and confirmed), and
#         sends it to member 1 only;
#   split it authors a fresh touch and sends version A to members 1 and 2, version B (the opposite
#         impulse) to member 4, before anyone has confirmed it. On loopback the proof reaches every
#         member within the frame, before its next simulation: no member simulates its version.
#   split-held  the split at 7986 ms, with the links between the groups (1,2 <-> 4, and 3 -> 4 once
#         version B is out) delayed 120 ms until 8.7 s, so each group simulates its own version for
#         several frames before the proof reaches it, and rolls back when it does.
# Every member traces frames 474..500 (--trace-frames): each state hash as computed and re-computed,
# the rollbacks, and the final hashes.
# The honest members hold a proof, vote member 3 out, and finish with identical histories. Runs on
# Windows (Git Bash) and Linux.
#
#   bash crates/pe-app/m1-equivocate.sh PE_APP_EXE OUT_DIR [BASE_PORT] [late|split|split-held]
#
# Exit 0 when members 1, 2 and 4 are equal to each other and to their offline replays. Member 3 writes
# to OUT_DIR/out-equivocator; removed, it stops confirming and is ended by the timeout.
set -u
E="$1"; D="$2"; P="${3:-7381}"; MODE="${4:-late}"
rm -rf "$D"
"$E" init --members 4 --host 127.0.0.1 --base-port "$P" --frames 1800 --size 4 --out "$D" > /dev/null || exit 2
uname -srm 2> /dev/null || ver
held() { # held T M..: this member's links to members M.. delayed 120 ms from T ms to 8.7 s
  local j="" m t=$1; shift
  for m in "$@"; do j="$j{\"at_ms\": $t, \"to\": $m, \"delay_ms\": 120}, {\"at_ms\": 8700, \"to\": $m, \"delay_ms\": 0}, "; done
  echo "[${j%, }]"
}
SPLIT=8000
if [ "$MODE" = split-held ]; then
  # Member 3 splits at 7986 ms (frame 479) and its own link to member 4 slows at 7996 ms, after version
  # B has gone out undelayed but before its next entry (frame 480, naming A) does.
  SPLIT=7986
  held 7900 4 > "$D/faults-1.json"; held 7900 4 > "$D/faults-2.json"; held 7996 4 > "$D/faults-3.json"; held 7900 1 2 > "$D/faults-4.json"
fi
for m in 1 2 3 4; do
  X="--trace-frames 474:500"; O="$D/out"; F=()
  [ "$MODE" = split-held ] && F=(--faults "$D/faults-$m.json")
  if [ "$m" = 3 ]; then
    O="$D/out-equivocator"
    case $MODE in
      split|split-held) X="$X --equivocate-split $SPLIT:4" ;;
      *) X="$X --equivocate-at-ms 8000 --equivocate-to 1" ;;
    esac
  fi
  timeout 70 "$E" run --session "$D/session.json" --key "$D/member-$m.key" --me "$m" --touches-per-s 3 --watch-member 3 $X "${F[@]}" --out "$O" -v > "$D/run-$m.log" 2>&1 &
done
wait
grep -hE "EQUIVOCAT|membership decision|CHECK|FINAL|fault:" "$D"/run-*.log
"$E" compare "$D/out"
