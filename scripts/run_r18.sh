#!/usr/bin/env bash
# R18 recovery under bandwidth caps (design 10.9): the ball pit through a token-bucket cap with
# three transports. Logs go to spikes/out/r18/.
set -eu
cd "$(dirname "$0")/.."
cargo build --release -q -p pe-spike-session-sim
mkdir -p spikes/out/r18
for k in 256 64 48 32; do
  ./target/release/pe-spike-session-sim capped --kbps $k > spikes/out/r18/capped-${k}kbps.log
  grep RESULT spikes/out/r18/capped-${k}kbps.log
done
