#!/usr/bin/env bash
# Perturbation-history study (design 6.7-6.11, spike report 5f). Release build. Work counts, image
# bytes and end hashes are deterministic; timings are only meaningful on an otherwise idle machine
# (round 3 reran it at low priority, one repetition: its timings are noisy). Logs go to
# spikes/out/perturbation-history/.
set -euo pipefail
cd "$(dirname "$0")/.."
# PE_BIN / PE_OUT: a prebuilt binary directory and an output root, as in run_session_sim.sh.
[ -n "${PE_BIN:-}" ] || cargo build --release -p pe-spike-physics -p pe-spike-session-sim
bin=${PE_BIN:-./target/release}
out=${PE_OUT:-spikes/out}/perturbation-history
mkdir -p "$out"
"$bin"/pe-spike-physics history > "$out/physics-history.log"
"$bin"/pe-spike-session-sim snap --reps 1 > "$out/snap-500.log"
"$bin"/pe-spike-session-sim snap --stall --reps 1 > "$out/snap-500-stall.log"
"$bin"/pe-spike-session-sim snap --balls 1024 --reps 1 > "$out/snap-1024.log"
"$bin"/pe-spike-session-sim snap --balls 1024 --stall --reps 1 > "$out/snap-1024-stall.log"
for d in 30 60 100; do
  "$bin"/pe-spike-session-sim observer --delay "$d" > "$out/observer-$d.log"
done
grep -h "RESULT" "$out"/*.log
