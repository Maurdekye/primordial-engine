#!/usr/bin/env bash
# R16 spike: build the chunk/region prototype in four box3d configurations and run every benchmark.
#   f32      box3d-rust 0.5.0 as published (f32 positions)
#   f64      box3d-rust 0.5.0 with `double-precision` (f64 positions)
#   *-nv     the same with World::validate_solver_sets() compiled out of release builds, the one-line
#            patch described in docs/r16-chunks-results.md (BOX3D_NOVALIDATE must point at a patched
#            copy of the crate; skipped when unset)
# Output: spikes/out/chunks/*.log
set -euo pipefail
# Machine-load rule (2026-10-30): run this at low priority with capped jobs, e.g. from Git Bash:
#   CARGO_BUILD_JOBS=4 cmd //c start "" //low //b //wait bash scripts/run_chunks_bench.sh
cd "$(dirname "$0")/../spikes/chunks"
OUT=../out/chunks
mkdir -p "$OUT"
cargo build --release --offline
CARGO_TARGET_DIR=target-f64 cargo build --release --offline --features f64
BINS=("f32:target/release/pe-spike-chunks" "f64:target-f64/release/pe-spike-chunks")
if [[ -n "${BOX3D_NOVALIDATE:-}" ]]; then
  P="patch.crates-io.box3d-rust.path=\"$BOX3D_NOVALIDATE\""
  CARGO_TARGET_DIR=target-patched cargo build --release --offline --config "$P"
  CARGO_TARGET_DIR=target-patched-f64 cargo build --release --offline --config "$P" --features f64
  git checkout Cargo.lock 2>/dev/null || true
  BINS+=("f32-nv:target-patched/release/pe-spike-chunks" "f64-nv:target-patched-f64/release/pe-spike-chunks")
fi
run() { # name subcommand
  for b in "${BINS[@]}"; do echo "### build ${b%%:*}"; "./${b#*:}" "$2"; done 2>&1 | tee "$OUT/$1.log"
}
./target/release/pe-spike-chunks precision | tee "$OUT/precision.log"
# Interleave the cost runs (5 rounds) so machine-load drift hits every build equally.
: > "$OUT/cost.log"
for round in 1 2 3 4 5; do
  for b in "${BINS[@]}"; do echo "### round $round build ${b%%:*}"; "./${b#*:}" cost; done 2>&1 | tee -a "$OUT/cost.log"
done
# f32/f64 A/B: 8 interleaved rounds of fixed 20-frame blocks; compare per-block minima across rounds.
: > "$OUT/ab.log"
for round in 1 2 3 4 5 6 7 8; do
  for b in "${BINS[@]}"; do echo "### round $round build ${b%%:*}"; "./${b#*:}" ab; done 2>&1 | tee -a "$OUT/ab.log"
done
run far far
run scale scale
run scale-mixed scale-mixed
run regions regions
run players players
run streaming streaming
./target/release/pe-spike-chunks spatial | tee "$OUT/spatial.log"
./target/release/pe-spike-chunks avatars | tee "$OUT/avatars.log"
./target/release/pe-spike-chunks closure | tee "$OUT/closure.log"
./target/release/pe-spike-chunks liveness | tee "$OUT/liveness.log"
