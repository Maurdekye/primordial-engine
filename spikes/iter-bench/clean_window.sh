#!/usr/bin/env bash
# Clean timing window for WS-logic edit time and October's rel-fast recheck (coordinator).
# Run as ONE heavy.sh job from the main checkout's root, with HEAVY_PRIO=normal and heavy.slots = 1:
#   HEAVY_PRIO=normal bash "$PE_HEAVY_DIR/heavy.sh" bash .worktrees/logic/spikes/iter-bench/clean_window.sh
# Phase 0 (untimed) builds pe-logic's load_time example. Phase 1 waits until the other slots (.heavy-lock-2,
# -3, -int) are free and no other cargo.exe or rustc.exe runs, logging every check (gives up after
# PE_DRAIN_MAX_S, default 1500 s, exit 3). Phase 2 times: iter_bench_loop.py --set recheck --reps 8, then
# iter_bench_logic_edit.py --reps 10 --tag clean. The machine state is logged at start, before timing and at end.
set -u
D="${PE_HEAVY_DIR:?set PE_HEAVY_DIR to the folder holding heavy.sh}"  # machine-specific, so not written here
WT=.worktrees/logic
O=spikes/out/iter-bench/clean-window
mkdir -p "$O"
LOG="$O/window.log"
export CARGO_TARGET_DIR="$PWD/spikes/iter-bench/target/clean-window"

state() {
  echo "# $1 $(date -u +%FT%TZ)"
  echo "# heavy.slots=$(cat "$D/heavy.slots") heavy.jobs=$(cat "$D/heavy.jobs") HEAVY_PRIO=${HEAVY_PRIO:-unset}"
  echo "# slot holders:"; for l in "$D"/.heavy-lock*; do [ -d "$l" ] && echo "#   $(basename "$l"): $(cat "$l/owner" 2>/dev/null)"; done
  echo "# heavy processes (cargo, rustc, cuts-model, *-model, link):"
  timeout 60 tasklist 2>/dev/null | grep -iE '^(cargo|rustc|cuts-model|[a-z0-9_-]*-model|link)\.exe' | sed 's/^/#   /'
  echo "# cpu load %: $(timeout 60 wmic cpu get loadpercentage 2>/dev/null | tr -dc '0-9 ' | xargs)"
}

{
  state start
  echo "# phase 0: build load_time example (untimed)"
  t=$(date +%s)
  (cd "$WT" && cargo build -q -p pe-logic --release --locked --example load_time) 2>&1 | tail -5
  echo "# phase 0 exit ${PIPESTATUS[0]} in $(( $(date +%s) - t )) s"

  echo "# phase 1: drain"
  t0=$(date +%s)
  while :; do
    others=$(ls -d "$D/.heavy-lock-2" "$D/.heavy-lock-3" "$D/.heavy-lock-int" 2>/dev/null | xargs -r -n1 basename | xargs)
    procs=$(timeout 60 tasklist 2>/dev/null | grep -ciE '^(cargo|rustc)\.exe')
    echo "#   $(date -u +%T)Z other slots: [${others}] cargo/rustc processes: ${procs}"
    [ -z "$others" ] && [ "$procs" -eq 0 ] && break
    if [ $(( $(date +%s) - t0 )) -ge "${PE_DRAIN_MAX_S:-1500}" ]; then echo "# DRAIN TIMEOUT"; state abort; exit 3; fi
    sleep 20
  done
  echo "# drained after $(( $(date +%s) - t0 )) s"
  state before-timing

  echo "# phase 2a: python -I scripts/iter_bench_loop.py --set recheck --reps 8"
  t=$(date +%s); python -I scripts/iter_bench_loop.py --set recheck --reps 8; echo "# 2a exit $? in $(( $(date +%s) - t )) s"
  state between
  echo "# phase 2b: python -I scripts/iter_bench_logic_edit.py --worktree $WT --reps 10 --tag clean"
  t=$(date +%s); python -I scripts/iter_bench_logic_edit.py --worktree "$WT" --reps 10 --tag clean
  echo "# 2b exit $? in $(( $(date +%s) - t )) s"
  state end
} 2>&1 | tee "$LOG"
exit "${PIPESTATUS[0]}"
