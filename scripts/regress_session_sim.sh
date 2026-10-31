#!/usr/bin/env bash
# Runs every session-protocol scenario file (tools/session-sim/scenarios) and compares each log
# with the committed one in spikes/out/session-sim-pe (CPU timings masked; pe-session's logs, P2-3). Fails on any difference,
# fork, replay mismatch, panic, false check or lost object. See tools/session-sim/README.md.
# usage: scripts/regress_session_sim.sh [--only a,b] [--jobs N] [--bless]
set -eu
cd "$(dirname "$0")/.."
cargo build --release -q -p pe-session-sim
exe="${CARGO_TARGET_DIR:-target}/release/pe-session-sim"
"$exe" regress "$@"
