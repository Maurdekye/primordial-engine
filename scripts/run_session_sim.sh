#!/usr/bin/env bash
# Builds and runs every session-protocol scenario (in-process, virtual clock; deterministic).
# Logs go to spikes/out/session-sim/; the RESULT lines are the evidence cited in docs/design.md.
# usage: scripts/run_session_sim.sh            (from any directory; needs only a Rust toolchain)
set -u
cd "$(dirname "$0")/.."
cargo build --release -q -p pe-spike-session-sim || exit 1
B=target/release/pe-spike-session-sim
O=spikes/out/session-sim
mkdir -p $O
run() { local name=$1; shift; "$B" "$@" > "$O/$name.log" 2>&1; echo "== $name"; grep "^RESULT" "$O/$name.log"; }
run c1 c1
run c2 c2
run state-frame state-frame
run diverged diverged
run loss loss
run ties ties
run claims claims
run backdate backdate
for v in ok abort reject throttle forge; do run r14-$v r14 $v; done
for v in ok abort reject; do run r14-$v-reverse r14 $v --reverse; done
run ballpit-60hz ballpit
run ballpit-60hz-noloss ballpit --loss 0
run ballpit-60hz-ahead4 ballpit --ahead 4
run ballpit-90hz ballpit --hz 90
run ballpit-60hz-2hands ballpit --hands 2
# Round-2 review repros (design 10.8): an adversarial member and a one-way outage.
for v in ackfork lateequiv evict outage; do run review2-$v $v; done
# Fail on any fork, replay mismatch, panic or false check (reviews NEW-2, NEW-7).
bad=$(grep -lE "MISMATCH|DIFFER|FAIL|PANIC|panicked" $O/*.log)
falsechecks=$(grep -hoE "checks ok [0-9]+ bad [1-9][0-9]*" $O/*.log | head -5)
if [ -n "$bad$falsechecks" ]; then
  echo "SUMMARY: FAILED: $bad $falsechecks"
  exit 1
fi
echo "SUMMARY: every scenario ALL EQUAL, no panic, 0 false checks"

