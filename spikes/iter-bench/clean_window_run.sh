#!/usr/bin/env bash
# Runs clean_window.sh as one heavy.sh job with heavy.slots set to 1, and on exit, whatever the outcome, restores
# heavy.slots to PE_RESTORE_SLOTS (default 3) (coordinator). Meant to be started detached, from the
# main checkout's root, with PE_HEAVY_DIR set. Writes spikes/out/iter-bench/clean-window/DONE (exit code and times) last.
set -u
D="${PE_HEAVY_DIR:?set PE_HEAVY_DIR to the folder holding heavy.sh}"
O=spikes/out/iter-bench/clean-window
mkdir -p "$O"
rm -f "$O/DONE"
prev=$(cat "$D/heavy.slots")
start=$(date -u +%FT%TZ)
restore() {
  printf '%s\n' "${PE_RESTORE_SLOTS:-3}" > "$D/heavy.slots.new" && mv "$D/heavy.slots.new" "$D/heavy.slots"
  echo "exit=${rc:-?} start=$start slots_before=$prev end=$(date -u +%FT%TZ) slots_restored_to=$(cat "$D/heavy.slots")" > "$O/DONE"
}
trap restore EXIT
printf '1\n' > "$D/heavy.slots.new" && mv "$D/heavy.slots.new" "$D/heavy.slots"
HEAVY_PRIO=normal bash "$D/heavy.sh" bash .worktrees/logic/spikes/iter-bench/clean_window.sh > "$O/runner.log" 2>&1
rc=$?
