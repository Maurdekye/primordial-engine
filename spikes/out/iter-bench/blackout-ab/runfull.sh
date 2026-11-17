#!/bin/bash
# The integrator's condition: the whole pe-app lib suite in release, --include-ignored, tests in parallel.
export CARGO="$USERPROFILE/.rustup/toolchains/1.99.0-x86_64-pc-windows-msvc/bin/cargo.exe"
H="S:/orgtree/appdata/scratch/primordial-engine/_teams/1827/heavy.sh"
cd S:/primordial-engine/repo/.worktrees/lv11/crates/pe-app
for i in 1 2 3; do for X in $*; do
  echo "== round $i $X $(date -u +%H:%M:%SZ)"
  bash "$H" S:/primordial-engine/tmp/iter-bench-blackout/$X.exe --include-ignored > S:/primordial-engine/tmp/iter-bench-blackout/full-$X-$i.log 2>&1
  grep -E "blackout.*(ok|FAILED)|after the path came back|test result" S:/primordial-engine/tmp/iter-bench-blackout/full-$X-$i.log
done; done
echo ALLDONE
