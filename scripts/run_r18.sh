#!/usr/bin/env bash
# R18 recovery under bandwidth caps (design 10.9): the ball pit through a token-bucket cap with
# three transports. Logs go to spikes/out/r18/. Run it at low priority on a shared machine.
set -eu
cd "$(dirname "$0")/.."
cargo build --release -q -p pe-spike-session-sim
B=./target/release/pe-spike-session-sim
O=spikes/out/r18
mkdir -p $O
for k in 256 64 48 32; do
  $B capped --kbps $k > $O/capped-${k}kbps.log
  grep RESULT $O/capped-${k}kbps.log
done
# Round 3 (review R3-9): below 20 kbit/s, a step down, four members on one shared uplink, and one
# member on a link too slow to keep up (demoted).
for k in 16 12 8; do
  $B capped --kbps $k --end 30 > $O/capped-${k}kbps.log
  grep RESULT $O/capped-${k}kbps.log
done
$B capped --kbps 64 --step-ms 3000 --step-kbps 16 --end 30 > $O/capped-step-64-16.log
grep RESULT $O/capped-step-64-16.log
$B capped --kbps 64 --members 4 --shared --end 30 > $O/capped-4members-shared64.log
grep RESULT $O/capped-4members-shared64.log
$B capped --kbps 256 --members 3 --slow 3 --slow-kbps 8 --end 20 > $O/capped-slow-member.log
grep RESULT $O/capped-slow-member.log
