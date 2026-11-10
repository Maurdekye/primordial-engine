#!/usr/bin/env bash
# The full determinism set, as run for the integration branch (2026-10-30). Start it at low priority,
# so every build and run below inherits that priority (machine-load rule):
#   CARGO_BUILD_JOBS=4 cmd //c start "" //low //b //wait bash scripts/run_determinism.sh [part ...]
# Parts: physics wasm mcp session r18 p2p hooks history (default: all, in that order).
# Physics checks write to spikes/out/integration/; the other parts regenerate their usual logs.
# Timings in these logs are noisy (low priority, shared machine); digests and counts are exact.
#
# Settings for the determinism matrix (scripts/run_matrix.sh, design 23.4 P2-6); all optional:
#   PE_OUT=<dir>        write every output under <dir> instead of spikes/out (the records stay as they are)
#   PE_TARGET=<triple>  build for this target; binaries land in target/<triple>/{release,debug}
#   PE_BIN=<dir>        run prebuilt release binaries from <dir>; nothing is built but the wasm guests
#   PE_DEBUG_BIN=<dir>  the debug physics binary's directory, or "none" to skip the debug checks
#   PE_BUILD_ONLY=1     build the binaries, then stop
#   PE_HEAVY=<script>   run every cargo command as `bash <script> cargo ...` (the org's heavy-job lock)
set -u
cd "$(dirname "$0")/.."
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}
PARTS=" ${*:-physics wasm mcp session r18 p2p hooks history} "
has() { case "$PARTS" in *" $1 "*) return 0 ;; esac; return 1; }
run_cargo() { if [ -n "${PE_HEAVY:-}" ]; then bash "$PE_HEAVY" cargo "$@"; else cargo "$@"; fi; }
PO=${PE_OUT:-spikes/out}
O=$PO/integration
mkdir -p $O $PO/r15
fail=0
check() { if [ "$1" = ok ]; then echo "OK    $2"; else echo "FAIL  $2"; fail=1; fi; }
same() { if cmp -s "$1" "$2"; then check ok "$3"; else check bad "$3"; fi; }

T=${PE_TARGET:+--target $PE_TARGET}
TD=target/${PE_TARGET:+$PE_TARGET/}
DBIN=${PE_DEBUG_BIN:-${TD}debug}
if [ -z "${PE_BIN:-}" ]; then
  echo "== build"
  run_cargo build --release -q $T -p pe-spike-physics -p pe-spike-session-sim -p pe-spike-p2p \
    -p pe-spike-wasm-hotswap -p pe-mcp || exit 1
  if has physics && [ "$DBIN" != none ]; then run_cargo build -q $T -p pe-spike-physics || exit 1; fi
fi
[ "${PE_BUILD_ONLY:-}" = 1 ] && exit 0
BIN=${PE_BIN:-${TD}release}
export PE_BIN=$BIN   # the sub-scripts below run these binaries and build nothing

if has physics; then
  echo "== physics: goldens, 600-frame trace, snapshot images, rollback (release and debug)"
  for b in release debug; do
    if [ $b = release ]; then P=$BIN/pe-spike-physics; else P=$DBIN/pe-spike-physics; fi
    if [ $b = debug ] && [ "$DBIN" = none ]; then continue; fi
    $P goldens > $O/goldens-vdw-$b.log 2>&1; grep -q "RESULT: PASS" $O/goldens-vdw-$b.log && r=ok || r=bad
    check $r "engine goldens ($b)"
    $P --upstream goldens > $O/goldens-off-$b.log 2>&1; grep -q "RESULT: PASS" $O/goldens-off-$b.log && r=ok || r=bad
    check $r "Box3D C goldens with --upstream ($b)"
    $P trace 600 $O/trace-vdw-$b.txt > $O/trace-vdw-$b.log 2>&1
    same $O/trace-vdw-$b.txt spikes/out/vdw/trace-vdw-win-$b.txt "trace 600 ($b) == vdW branch record"
    $P --upstream trace 600 $O/trace-off-$b.txt > $O/trace-off-$b.log 2>&1
    # The upstream baseline (spikes/out/vdw/trace-off-*): until D58 it equalled the phase-1 f32
    # record spikes/out/trace-win-release-1.txt byte for byte; with double precision it is re-recorded.
    same $O/trace-off-$b.txt spikes/out/vdw/trace-off-win-$b.txt "trace 600 --upstream ($b) == upstream record"
    $P images $O/images-vdw-$b.txt > $O/images-vdw-$b.log 2>&1
    same $O/images-vdw-$b.txt spikes/out/vdw/images-vdw-win-$b.txt "snapshot images ($b) == vdW branch record"
    $P rollback > $O/rollback-vdw-$b.log 2>&1; grep -q "RESULT: PASS" $O/rollback-vdw-$b.log && r=ok || r=bad
    check $r "rollback T1-T5 ($b)"
  done
  [ "$DBIN" = none ] || same $O/trace-vdw-release.txt $O/trace-vdw-debug.txt "trace 600: release == debug"
  grep -h "digest" $O/trace-vdw-release.log $O/images-vdw-release.log | cut -c1-120
fi

if has wasm; then
  echo "== wasm guests and hot-swap (d1-d4, eager and lazy)"
  PE_HEAVY=${PE_HEAVY:-} bash scripts/build_wasm_guests.sh || exit 1
  R=target/wasm32-unknown-unknown/release      # logic guests, in spikes/wasm-guests
  P=target/wasm32-unknown-unknown/wasm-guest   # physics_in_wasm, in the root workspace
  I=spikes/iter-bench/guests/$R                # P2-7's larger crates
  mods() { echo spikes/wasm-guests/$R/logic_{v1,v2,runaway}.wasm $P/physics_in_wasm.wasm $I/logic_{mono,wasm}.wasm; }
  sha256sum $(mods) > $PO/guest-wasm-sha256.txt
  cat $PO/guest-wasm-sha256.txt
  $BIN/pe-spike-wasm-hotswap > $PO/wasm-hotswap-eager.log 2>&1
  grep -q "RESULT: PASS" $PO/wasm-hotswap-eager.log && r=ok || r=bad
  check $r "wasm-hotswap d1-d4 (eager): the wasm32 pile's digest == native"
  $BIN/pe-spike-wasm-hotswap --lazy > $PO/wasm-hotswap-lazy.log 2>&1
  grep -h "RESULT" $PO/wasm-hotswap-lazy.log | head -3

  echo "== module identity: the guests built again at a second checkout path (review M8)"
  E=$(mktemp -d)
  # The whole checked-out tree (about 11 MB, untracked and ignored files left out).
  git ls-files -z -co --exclude-standard | tar --null -T - -cf - | tar -x -C "$E"
  PE_HEAVY=${PE_HEAVY:-} bash scripts/build_wasm_guests.sh "$E" || exit 1
  EW=$(cygpath -m "$E" 2>/dev/null || echo "$E")
  ${PE_PYTHON:-python} -I scripts/wasm_canonical_hash.py $(mods) $(for m in $(mods); do echo "$EW/$m"; done) \
    | sed "s#$EW#<copy at another path>#" > $PO/wasm-canonical-hash.txt
  cat $PO/wasm-canonical-hash.txt
  r=ok
  for m in $(mods); do cmp -s "$m" "$E/$m" || { r=bad; echo "differs at the second path: $m"; }; done
  check $r "each of the $(mods | wc -w) modules byte-identical (raw hash) at both paths"
  (cd "$E" && grep -alE '[A-Za-z]:\\|/home/|/Users/' $(mods)) && r=bad || r=ok
  grep -alE '[A-Za-z]:\\|/home/|/Users/' $(mods) && r=bad
  check $r "no local path inside any module, at either path"
  rm -rf "$E"
fi

if has mcp; then
  echo "== MCP server (pe-mcp, embedded stdio bench)"
  # Requests are answered concurrently; sorting the lines (by id) makes the file comparable across runs.
  $BIN/pe-mcp stdio < crates/pe-mcp/session.jsonl 2> $O/mcp.err | sort > $PO/mcp-session.out.jsonl
  n=$(grep -c '"jsonrpc"' $PO/mcp-session.out.jsonl)
  [ "$n" = 10 ] && r=ok || r=bad
  check $r "pe-mcp: $n of 10 responses"
fi

if has session; then
  echo "== session simulator suite"
  PE_OUT=$PO bash scripts/run_session_sim.sh > $O/session-sim-suite.log 2>&1 && r=ok || r=bad
  cp $O/session-sim-suite.log $PO/session-sim/summary.txt
  tail -1 $O/session-sim-suite.log
  check $r "session-sim suite"
fi

if has r18; then
  echo "== R18 caps"
  PE_OUT=$PO bash scripts/run_r18.sh > $O/r18.log 2>&1 && r=ok || r=bad
  check $r "run_r18.sh"
  grep -c "RESULT" $O/r18.log
fi

if has p2p; then
  echo "== p2p over UDP"
  bash scripts/run-p2p.sh latency60 "--delay-ms 60 --jitter-ms 20" "--delay-ms 60 --jitter-ms 20" > $O/p2p-latency60.log 2>&1
  bash scripts/run-p2p.sh loss10 "--delay-ms 40 --jitter-ms 60 --loss-pct 10" "--delay-ms 40 --jitter-ms 60 --loss-pct 10" > $O/p2p-loss10.log 2>&1
  bash scripts/run-p2p.sh fault "--delay-ms 30 --jitter-ms 10" "--delay-ms 30 --jitter-ms 10 --drop-stamp 200" > $O/p2p-fault.log 2>&1
  for n in latency60 loss10 fault; do
    if grep -q "peer 1 == peer 2" $O/p2p-$n.log && [ "$(grep -c "peer [12]: offline replay.*(EQUAL)" $O/p2p-$n.log)" = 2 ]; then r=ok; else r=bad; fi
    check $r "p2p $n: peers equal, both offline replays EQUAL"
    grep -h "acceptance at receiver\|offline replay" $O/p2p-$n.log
  done
fi

if has hooks; then
  echo "== R15 hooks (call counts with the engine default)"
  $BIN/pe-spike-physics hooks > $PO/r15/hooks.log 2>&1
  grep -h "RESULT" $PO/r15/hooks.log
fi

if has history; then
  echo "== perturbation history"
  PE_OUT=$PO bash scripts/run_perturbation_history.sh > $O/perturbation-history.log 2>&1 && r=ok || r=bad
  check $r "run_perturbation_history.sh"
fi

if [ $fail = 0 ]; then echo "SUMMARY: determinism set PASSED ($PARTS)"; else echo "SUMMARY: determinism set FAILED ($PARTS)"; fi
exit $fail
