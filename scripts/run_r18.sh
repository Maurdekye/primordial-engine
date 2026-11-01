#!/usr/bin/env bash
# R18 recovery under bandwidth caps (design 10.9): the ball pit through a token-bucket cap with
# three transports. Logs go to spikes/out/r18/. Run it at low priority on a shared machine.
set -eu
cd "$(dirname "$0")/.."
# PE_BIN / PE_OUT: a prebuilt binary directory and an output root, as in run_session_sim.sh.
# --pe: the same runs on crates/pe-session through its harness (P2-3); logs in spikes/out/r18-pe/.
if [ "${1:-}" = --pe ]; then
  [ -n "${PE_BIN:-}" ] || cargo build --release -q -p pe-session-harness
  B=${PE_BIN:-./target/release}/pe-session-harness
  O=${PE_OUT:-spikes/out}/r18-pe
else
  [ -n "${PE_BIN:-}" ] || cargo build --release -q -p pe-spike-session-sim
  B=${PE_BIN:-./target/release}/pe-spike-session-sim
  O=${PE_OUT:-spikes/out}/r18
fi
mkdir -p "$O"
for k in 256 64 48 32; do
  "$B" capped --kbps $k > "$O"/capped-${k}kbps.log
  grep RESULT "$O"/capped-${k}kbps.log
done
# Round 3 (review R3-9): below 20 kbit/s, a step down, four members on one shared uplink, and one
# member on a link too slow to keep up (demoted).
for k in 16 12 8; do
  "$B" capped --kbps $k --end 30 > "$O"/capped-${k}kbps.log
  grep RESULT "$O"/capped-${k}kbps.log
done
"$B" capped --kbps 64 --step-ms 3000 --step-kbps 16 --end 30 > "$O"/capped-step-64-16.log
grep RESULT "$O"/capped-step-64-16.log
"$B" capped --kbps 64 --members 4 --shared --end 30 > "$O"/capped-4members-shared64.log
grep RESULT "$O"/capped-4members-shared64.log
"$B" capped --kbps 256 --members 3 --slow 3 --slow-kbps 8 --end 20 > "$O"/capped-slow-member.log
grep RESULT "$O"/capped-slow-member.log
# The first gesture after a long idle (@coordinator-opus, 2026-11-01; 10.9 item 3): two gestures 40 s
# apart (through the idle the controller does not step down into the level that congested, or finer),
# and 310 s apart (D56's 5-minute ceiling has forgotten that level, so the second gesture probes from
# level 0 again).
# Spike only until pe-session's controller has the rule and its harness the --gap option.
if [ "${1:-}" != --pe ]; then
  for k in 32 16 12; do
    "$B" capped --kbps $k --gap 40 --end 66 > "$O"/capped-${k}kbps-two-gestures.log
    grep RESULT "$O"/capped-${k}kbps-two-gestures.log
  done
  "$B" capped --kbps 16 --gap 310 --end 335 > "$O"/capped-16kbps-after-5min.log
  grep RESULT "$O"/capped-16kbps-after-5min.log
fi
