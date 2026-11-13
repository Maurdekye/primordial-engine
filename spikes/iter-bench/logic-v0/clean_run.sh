#!/usr/bin/env bash
# Clean timing run for WS-logic (coordinator's window, both heavy slots held, HEAVY_PRIO=normal).
# Run from the repo root of the main checkout; writes into the ws/logic worktree's spikes/out.
set -u
O=".worktrees/logic/spikes/out/iter-bench/logic-v0/clean"
D="${PE_HEAVY_DIR:?set PE_HEAVY_DIR to the folder holding heavy.sh}"  # machine-specific, so not written here
P=spikes/iter-bench/target/logic-v0/release/logic-probe.exe
W=spikes/iter-bench/target/guests/wasm32-unknown-unknown/release
PH=spikes/wasm-guests/target/wasm32-unknown-unknown/release/physics_in_wasm.wasm
mkdir -p "$O"
{
  echo "# start $(date -u +%FT%TZ)"
  echo "# heavy holders:"; cat "$D"/.heavy-lock*/owner 2>/dev/null
  echo "# probe sha256: $(sha256sum "$P" | cut -c1-64)"
  sha256sum "$W/logic_wasm.wasm" "$PH"
} > "$O/run-load.txt"
for r in 1 2; do
  "$P" bench "$W/logic_wasm.wasm" > "$O/bench-run$r.jsonl" 2>&1
  "$P" physics "$PH" > "$O/physics-run$r.jsonl" 2>&1
done
"$P" load "$W/logic_wasm.wasm" spikes/wasm-guests/target/wasm32-unknown-unknown/release/logic_v1.wasm > "$O/load.jsonl" 2>&1
"$P" traps > "$O/traps.jsonl" 2>&1
{ echo "# end $(date -u +%FT%TZ)"; echo "# heavy holders:"; cat "$D"/.heavy-lock*/owner 2>/dev/null; } >> "$O/run-load.txt"
