#!/usr/bin/env bash
# R15 (design 27.5): cost of physics hooks in a wasm guest, and how often Box3D calls them.
# Runs at low priority with capped build jobs (machine-load rule); timings are noisy, counts exact.
set -eu
cd "$(dirname "$0")/.."
mkdir -p spikes/out/r15
export CARGO_BUILD_JOBS=4
(cd spikes/iter-bench/cb_guest && cmd //c start "" //low //b //wait cargo build --release --target wasm32-unknown-unknown)
(cd spikes/iter-bench/host && cmd //c start "" //low //b //wait cargo build --release)
cmd //c start "" //low //b //wait cargo build --release -p pe-spike-physics
cmd //c start "" //low //b //wait spikes/iter-bench/host/target/release/iter-host.exe callback \
  spikes/iter-bench/cb_guest/target/wasm32-unknown-unknown/release/cb_guest.wasm > spikes/out/r15/callback.jsonl
cmd //c start "" //low //b //wait target/release/pe-spike-physics.exe hooks > spikes/out/r15/hooks.log
cat spikes/out/r15/callback.jsonl spikes/out/r15/hooks.log
