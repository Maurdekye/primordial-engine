#!/bin/bash
# A/B of pe-app's blackout test: A = dad41a34 (integrator's base), B = 8f1cd3b0 (v10 rebased on it), M = main 5ecce898.
export CARGO="$USERPROFILE/.rustup/toolchains/1.99.0-x86_64-pc-windows-msvc/bin/cargo.exe"
H="S:/orgtree/appdata/scratch/primordial-engine/_teams/1827/heavy.sh"
T=net::tests::a_ten_second_blackout_resumes_and_every_entry_becomes_final
cd S:/primordial-engine/repo/.worktrees/lv11/crates/pe-app
for i in 1 2 3; do for X in $*; do
  echo "== round $i $X $(date -u +%H:%M:%SZ)"
  bash "$H" S:/primordial-engine/tmp/iter-bench-blackout/$X.exe --include-ignored --exact $T 2>&1 | grep -E "after the path came back|test result|panicked"
done; done
echo ALLDONE
