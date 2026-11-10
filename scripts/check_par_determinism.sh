#!/usr/bin/env bash
# D2: pe-physics writes the same records at every worker count. For each count, the record
# generator (crates/pe-physics/examples/records.rs) writes the trace and image records of both
# configurations into a temporary folder, and each is compared with the committed record, which
# one worker wrote. The goldens and rollback T1-T5 run with one worker, in cargo test.
#   PE_HEAVY=<lock wrapper> bash scripts/check_par_determinism.sh [release|debug] [workers...]
# The default is release with 1 2 3 4 8 16 workers. Do not run it under heavy.sh: each command
# below takes the lock itself through PE_HEAVY.
set -u
cd "$(dirname "$0")/.."
low() { if [ -n "${PE_HEAVY:-}" ]; then bash "$PE_HEAVY" "$@"; else "$@"; fi; }
profile=${1:-release}
shift 1 2>/dev/null || shift $#
workers=${*:-1 2 3 4 8 16}
case $profile in
  release) flag=--release ;;
  debug) flag="" ;;
  *) echo "usage: $0 [release|debug] [workers...]"; exit 2 ;;
esac
d=crates/pe-physics/tests/records/vdw
low cargo build -q $flag -p pe-physics --example records || { echo "RESULT: FAIL (build)"; exit 1; }
exe=${CARGO_TARGET_DIR:-target}/$profile/examples/records
tmp=$(mktemp -d)
fail=0
for w in $workers; do
  if ! low "$exe" records "$tmp" "$w" > "$tmp/run.log" 2>&1; then
    echo "$profile workers=$w: the generator failed"; tail -3 "$tmp/run.log"; fail=1; continue
  fi
  line="$profile workers=$w:"
  for f in trace-vdw trace-off images-vdw images-off; do
    if cmp -s "$d/$f-win-$profile.txt" "$tmp/$f-win-$profile.txt"; then
      line="$line $f same"
    else
      line="$line $f DIFF"; fail=1
    fi
  done
  echo "$line"
done
rm -rf "$tmp"
if [ $fail = 0 ]; then echo "RESULT: PASS (identical at every worker count)"; else echo "RESULT: FAIL"; fi
exit $fail
