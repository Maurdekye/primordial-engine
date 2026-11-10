#!/usr/bin/env bash
# Clock-sync fault runs on loopback (design 10.4's evidence, review CM-3), all within 10.8's bound.
#
#   bash crates/pe-app/clock-faults.sh EXE OUT skew4|single4|collude7 [median] [BASE_PORT] [FRAMES]
#
# skew4:    4 members; member 4's clock runs 9 % slow (the start-up transient, review M2).
# single4:  4 members; member 4 shows member 1 a clock running 5 % fast (and only member 1).
# collude7: 7 members (t = 2). Members 6 and 7 share a slow machine (their clocks run 5 % slow).
#           Members 4 and 5 collude: to 6 and 7 they report a clock 1 s behind and as slow as theirs,
#           to 1-3 one 1 s ahead and 5 % fast. A plain median pins 6 and 7 (they never correct);
#           the fault-tolerant midpoint does not.
# `median` runs every member with --clock-median (the contrast). Every member runs R18 and 3
# touches/s; the run ends with compare and a count of membership decisions per member.
set -u
E=$1; D=$2; CASE=$3; MODE=${4:-ftm}; BASE=${5:-7501}; F=${6:-5400}
X=""; [ "$MODE" = median ] && X="--clock-median"
rm -rf "$D"; mkdir -p "$D"
case $CASE in
skew4 | single4) N=4 ;;
collude7) N=7 ;;
*) echo "unknown case $CASE"; exit 2 ;;
esac
"$E" init --members $N --host 127.0.0.1 --base-port $BASE --frames $F --size 4 --out "$D" > /dev/null || exit 2
for m in $(seq 1 $N); do
    A=""
    case $CASE/$m in
    skew4/4) A="--clock-skew-ppm -90000" ;;
    single4/4) A="--clock-lie 1:0:50000" ;;
    collude7/6 | collude7/7) A="--clock-skew-ppm -50000" ;;
    collude7/4 | collude7/5) A="--clock-lie 6:-1000:-50000,7:-1000:-50000,1:1000:50000,2:1000:50000,3:1000:50000" ;;
    esac
    echo "member $m: $A $X" >> "$D/args.txt"
    timeout $(( F / 60 + 60 )) "$E" run --session "$D/session.json" --key "$D/member-$m.key" --me $m --touches-per-s 3 $A $X --out "$D/out" -v > "$D/run-$m.log" 2>&1 &
done
wait
for m in $(seq 1 $N); do
    echo "member $m: $(grep -c 'membership decision' "$D/run-$m.log") membership decisions, $(grep -c 'paused at' "$D/run-$m.log") pauses; last decision: $(grep 'membership decision' "$D/run-$m.log" | tail -1 | sed 's/.*decision: //')"
done | tee "$D/decisions.txt"
"$E" compare "$D/out" | tee "$D/compare.txt" | tail -$(( N + 3 ))
