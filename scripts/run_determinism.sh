#!/usr/bin/env bash
# The full determinism set, as run for the integration branch (2026-10-30). Start it at low priority,
# so every build and run below inherits that priority (machine-load rule):
#   CARGO_BUILD_JOBS=4 cmd //c start "" //low //b //wait bash scripts/run_determinism.sh [part ...]
# Parts: physics wasm mcp session r18 p2p hooks history (default: all, in that order).
# Physics checks write to spikes/out/integration/; the other parts regenerate their usual logs.
# Timings in these logs are noisy (low priority, shared machine); digests and counts are exact.
set -u
cd "$(dirname "$0")/.."
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}
PARTS=" ${*:-physics wasm mcp session r18 p2p hooks history} "
has() { case "$PARTS" in *" $1 "*) return 0 ;; esac; return 1; }
O=spikes/out/integration
mkdir -p $O
fail=0
check() { if [ "$1" = ok ]; then echo "OK    $2"; else echo "FAIL  $2"; fail=1; fi; }
same() { if cmp -s "$1" "$2"; then check ok "$3"; else check bad "$3"; fi; }

echo "== build"
cargo build --release -q -p pe-spike-physics -p pe-spike-session-sim -p pe-spike-p2p \
  -p pe-spike-wasm-hotswap -p pe-mcp || exit 1
if has physics; then cargo build -q -p pe-spike-physics || exit 1; fi

if has physics; then
  echo "== physics: goldens, 600-frame trace, snapshot images, rollback (release and debug)"
  for b in release debug; do
    P=target/$b/pe-spike-physics
    $P goldens > $O/goldens-vdw-$b.log 2>&1; grep -q "RESULT: PASS" $O/goldens-vdw-$b.log && r=ok || r=bad
    check $r "engine goldens ($b)"
    $P --upstream goldens > $O/goldens-off-$b.log 2>&1; grep -q "RESULT: PASS" $O/goldens-off-$b.log && r=ok || r=bad
    check $r "Box3D C goldens with --upstream ($b)"
    $P trace 600 $O/trace-vdw-$b.txt > $O/trace-vdw-$b.log 2>&1
    same $O/trace-vdw-$b.txt spikes/out/vdw/trace-vdw-win-$b.txt "trace 600 ($b) == vdW branch record"
    $P --upstream trace 600 $O/trace-off-$b.txt > $O/trace-off-$b.log 2>&1
    same $O/trace-off-$b.txt spikes/out/trace-win-release-1.txt "trace 600 --upstream ($b) == phase-1 record"
    $P images $O/images-vdw-$b.txt > $O/images-vdw-$b.log 2>&1
    same $O/images-vdw-$b.txt spikes/out/vdw/images-vdw-win-$b.txt "snapshot images ($b) == vdW branch record"
    $P rollback > $O/rollback-vdw-$b.log 2>&1; grep -q "RESULT: PASS" $O/rollback-vdw-$b.log && r=ok || r=bad
    check $r "rollback T1-T5 ($b)"
  done
  same $O/trace-vdw-release.txt $O/trace-vdw-debug.txt "trace 600: release == debug"
  grep -h "digest" $O/trace-vdw-release.log $O/images-vdw-release.log | cut -c1-120
fi

if has wasm; then
  echo "== wasm guests and hot-swap (d1-d4, eager and lazy)"
  bash scripts/build_wasm_guests.sh || exit 1
  R=target/wasm32-unknown-unknown/release      # logic guests, in spikes/wasm-guests
  P=target/wasm32-unknown-unknown/wasm-guest   # physics_in_wasm, in the root workspace
  sha256sum spikes/wasm-guests/$R/*.wasm $P/physics_in_wasm.wasm > spikes/out/guest-wasm-sha256.txt
  cat spikes/out/guest-wasm-sha256.txt
  ./target/release/pe-spike-wasm-hotswap > spikes/out/wasm-hotswap-eager.log 2>&1
  grep -q "RESULT: PASS" spikes/out/wasm-hotswap-eager.log && r=ok || r=bad
  check $r "wasm-hotswap d1-d4 (eager): the wasm32 pile's digest == native"
  ./target/release/pe-spike-wasm-hotswap --lazy > spikes/out/wasm-hotswap-lazy.log 2>&1
  grep -h "RESULT" spikes/out/wasm-hotswap-lazy.log | head -3

  echo "== module identity: the guests built again at a second checkout path (review M8)"
  E=$(mktemp -d)
  # The whole checked-out tree (about 11 MB, untracked and ignored files left out).
  git ls-files -z -co --exclude-standard | tar --null -T - -cf - | tar -x -C "$E"
  bash scripts/build_wasm_guests.sh "$E" || exit 1
  EW=$(cygpath -m "$E" 2>/dev/null || echo "$E")
  python -I scripts/wasm_canonical_hash.py spikes/wasm-guests/$R/*.wasm $P/physics_in_wasm.wasm \
    "$EW"/spikes/wasm-guests/$R/*.wasm "$EW"/$P/physics_in_wasm.wasm \
    | sed "s#$EW#<copy at another path>#" > spikes/out/wasm-canonical-hash.txt
  cat spikes/out/wasm-canonical-hash.txt
  n=$(grep -oE "canonical sha256 [0-9a-f]+" spikes/out/wasm-canonical-hash.txt | sort -u | wc -l)
  [ "$n" = 4 ] && r=ok || r=bad
  check $r "canonical hash of each of the 4 modules equal at both paths"
  grep -aqE '[A-Za-z]:\\' $P/physics_in_wasm.wasm "$E"/$P/physics_in_wasm.wasm && r=bad || r=ok
  check $r "no local (drive-letter) path inside either physics_in_wasm.wasm"
  rm -rf "$E"
fi

if has mcp; then
  echo "== MCP server (pe-mcp, embedded stdio bench)"
  # Requests are answered concurrently; sorting the lines (by id) makes the file comparable across runs.
  ./target/release/pe-mcp stdio < crates/pe-mcp/session.jsonl 2> $O/mcp.err | sort > spikes/out/mcp-session.out.jsonl
  n=$(grep -c '"jsonrpc"' spikes/out/mcp-session.out.jsonl)
  [ "$n" = 10 ] && r=ok || r=bad
  check $r "pe-mcp: $n of 10 responses"
fi

if has session; then
  echo "== session simulator suite"
  bash scripts/run_session_sim.sh > $O/session-sim-suite.log 2>&1 && r=ok || r=bad
  cp $O/session-sim-suite.log spikes/out/session-sim/summary.txt
  tail -1 $O/session-sim-suite.log
  check $r "session-sim suite"
fi

if has r18; then
  echo "== R18 caps"
  bash scripts/run_r18.sh > $O/r18.log 2>&1 && r=ok || r=bad
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
  ./target/release/pe-spike-physics hooks > spikes/out/r15/hooks.log 2>&1
  grep -h "RESULT" spikes/out/r15/hooks.log
fi

if has history; then
  echo "== perturbation history"
  bash scripts/run_perturbation_history.sh > $O/perturbation-history.log 2>&1 && r=ok || r=bad
  check $r "run_perturbation_history.sh"
fi

if [ $fail = 0 ]; then echo "SUMMARY: determinism set PASSED ($PARTS)"; else echo "SUMMARY: determinism set FAILED ($PARTS)"; fi
exit $fail
