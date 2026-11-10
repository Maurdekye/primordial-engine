#!/usr/bin/env bash
# Re-records pe-physics's committed records from source in one command (rule of 2026-10-31: no
# hand-edited records). Whichever branch lands second on main reruns it on the merged tree and
# commits the result.
#   PE_HEAVY=<lock wrapper> bash scripts/rebless_physics_records.sh
# Do not run it under heavy.sh: heavy.sh is not re-entrant, and every cargo command below takes
# the lock itself through PE_HEAVY.
#
# Since the M0 archive the physics spike is gone (archive/spikes/physics), and the generator is
# pe-physics's own example, crates/pe-physics/examples/records.rs. The old script's other steps
# re-recorded the old engine's sessions; they are archived with it (archive/scripts/).
#
# What it does:
#  1. writes trace-{vdw,off}-win-{release,debug}.txt and images-{vdw,off}-win-{release,debug}.txt
#     into crates/pe-physics/tests/records/vdw, from release and debug builds of the generator;
#  2. checks that the debug trace equals the release trace (the image records differ by profile
#     by design: the image header carries a debug-only validation flag);
#  3. runs pe-physics's tests in release and debug: spike_records compares the records, and the
#     goldens and rollback T1-T5 are tests too.
# The spike's .log files in that folder are evidence of the commits that wrote them; nothing
# regenerates them.
set -euo pipefail
cd "$(dirname "$0")/.."
low() { if [ -n "${PE_HEAVY:-}" ]; then bash "$PE_HEAVY" "$@"; else "$@"; fi; }
step() { echo; echo "== $*"; }
d=crates/pe-physics/tests/records/vdw

step "1. records in $d (release and debug)"
low cargo run -q --release -p pe-physics --example records -- records "$d"
low cargo run -q -p pe-physics --example records -- records "$d"

step "2. the debug trace equals the release trace"
for m in vdw off; do
  cmp -s "$d/trace-$m-win-debug.txt" "$d/trace-$m-win-release.txt" \
    || { echo "FAIL: the $m trace differs between debug and release"; exit 1; }
done
echo "PASS"

step "3. pe-physics's tests (release and debug)"
low cargo test -q --release -p pe-physics
low cargo test -q -p pe-physics

step "done: review the diff, then commit it"
git status --short -- "$d"
