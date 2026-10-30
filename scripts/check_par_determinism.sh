#!/usr/bin/env bash
# D2: check that pe-spike-physics gives identical results at every worker-thread count.
# For each build dir and thread count: goldens (engine and --upstream), the 600-frame trace and
# snapshot-image bytes compared with the committed single-thread outputs in spikes/out/vdw, and
# rollback T1-T5. Usage: scripts/check_par_determinism.sh [exe] [label] [threads...]
# Run at low priority on a shared machine (see the team notes).
set -u
exe=${1:-target/release/pe-spike-physics.exe}
label=${2:-release}
shift 2 2>/dev/null || shift $#
threads=${*:-1 2 3 4 8 16}
d=spikes/out/vdw
tmp=$(mktemp -d)
fail=0
for t in $threads; do
  for m in vdw off; do
    fl="--threads=$t"; [ $m = off ] && fl="$fl --upstream"
    $exe $fl goldens > $tmp/g.log 2>&1
    $exe $fl trace 600 $tmp/t.txt > $tmp/t.log 2>&1
    $exe $fl images $tmp/i.txt > $tmp/i.log 2>&1
    $exe $fl rollback > $tmp/r.log 2>&1
    g=$(grep -c "^\[PASS\]" $tmp/g.log); gf=$(grep -c "^\[FAIL\]" $tmp/g.log)
    r=$(grep -c "^\[PASS\]" $tmp/r.log); rf=$(grep -c "^\[FAIL\]" $tmp/r.log)
    tr=$(cmp -s $tmp/t.txt $d/trace-$m-win-$label.txt && echo same || echo DIFF)
    im=$(cmp -s $tmp/i.txt $d/images-$m-win-$label.txt && echo same || echo DIFF)
    echo "$label threads=$t $m: goldens pass=$g fail=$gf | trace $tr | images $im | rollback pass=$r fail=$rf | $(grep -o 'digest [0-9a-f]*' $tmp/t.log)"
    if [ $gf != 0 ] || [ $rf != 0 ] || [ $tr != same ] || [ $im != same ]; then fail=1; fi
  done
done
rm -rf $tmp
if [ $fail = 0 ]; then echo "RESULT: PASS (identical at every thread count)"; else echo "RESULT: FAIL"; fi
exit $fail
