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
for v in ok abort reject throttle forge late crash chain chain-epoch; do run r14-$v r14 $v; done
for v in ok abort reject chain; do run r14-$v-reverse r14 $v --reverse; done
# P2-5 controls, outside the pass/fail check below (both are expected NOT CONSERVED): the forge
# attacks with provenance checks off, and the double spend that provenance chains cannot stop.
mkdir -p $O/p2-5-controls
for c in "r14-forge-no-provenance r14 forge --no-provenance" "r14-doublespend r14 doublespend"; do
  set -- $c; name=$1; shift; "$B" "$@" > "$O/p2-5-controls/$name.log" 2>&1; echo "== control $name"; grep "^RESULT" "$O/p2-5-controls/$name.log"
done
run ballpit-60hz ballpit
run ballpit-60hz-noloss ballpit --loss 0
run ballpit-60hz-ahead4 ballpit --ahead 4
run ballpit-90hz ballpit --hz 90
run ballpit-60hz-2hands ballpit --hands 2
# Round-2 review repros (design 10.8): an adversarial member and a one-way outage.
for v in ackfork lateequiv evict outage; do run review2-$v $v; done
# Round-3 review repros (design 9.3, 10.8): colluding voters, a crashed member.
for v in badvote liar crashstall; do run review3-$v $v; done
# P2-4, the view change (design 10.8): several members at once, a withheld vote, colluders, an
# equivocating proposer, a join, and the D80 quorum comparisons (twosilent5 and bridge run both rules;
# bridge under a strict majority forks by design, and each side is checked on its own).
for v in twosilent twosilent5 withheld colluders badproposer bridge join refwithhold; do run p2-4-$v $v; done
# A member moves only its own hand, and touches or carries no one else's (design 8.4; P2-8 review, HO-1).
run handowner handowner
# P2-4 review round 2 (design 10.8): each repro also reports its run with the fix switched off.
for v in fork9 jointiming bogusprecommit futureack; do run p2-4-$v $v; done
# D80 admins and roles (design 10.8, 12.1): kicks in commit order, a kick across a partition, the last
# admin, closing a session with no admin, the join rules.
for v in kickwar kickpartition lastadmin adminclose joinrules; do run d80-$v $v; done
# P2-4 review round 3 (design 10.8): each repro also reports its run with the fix switched off.
for v in ghostkick selfkickhead ackgap latejoin capchurn deepfork cutbackdate joinattests slowadmin kickbackdate lagprecommit aheadlink; do run p2-4-$v $v; done
# P2-4 review round 4 (design 10.8): the same, for the round-3 findings and round 4's own changes.
for v in strikeframe aheadshield fakeref capstale lagchurn votechurn adminvoid joinsquat joinskip; do run p2-4-$v $v; done
# P2-4 review round 5 (design 10.8): the join wedge and the slow/fake-reference halt.
for v in joinwedge slowref prevotefork; do run p2-4-$v $v; done
for v in fakemix adminfork; do run p2-4-$v $v; done
# The randomized Byzantine schedule generator (not a scenario file): a fixed seed range, checked
# against the written invariants (spikes/out/session-sim/INVARIANTS.md). A larger run is separate.
"$B" randsched --seeds 12 > "$O/randsched.log" 2>&1; echo "== randsched"; grep "^RESULT" "$O/randsched.log"
# Review VC-1, a control outside the check below (expected to fork): three of six collude, above the
# bound of t(6) = 1 faulty member.
mkdir -p $O/p2-4-controls
"$B" onesided > "$O/p2-4-controls/onesided.log" 2>&1; echo "== control onesided"; grep "^RESULT" "$O/p2-4-controls/onesided.log"
run p2-4-vcscale-64 vcscale --members 64 --silent 2
run p2-4-vcscale-256 vcscale --members 256 --silent 2 --no-heads
# Fail on any fork, replay mismatch, panic, false check or lost/duplicated object (reviews NEW-2, NEW-7).
bad=$(grep -lE "MISMATCH|DIFFER|FAIL|PANIC|panicked|NOT CONSERVED" $O/*.log)
falsechecks=$(grep -hoE "checks ok [0-9]+ bad [1-9][0-9]*|nonfinite [1-9][0-9]*" $O/*.log | head -5)
if [ -n "$bad$falsechecks" ]; then
  echo "SUMMARY: FAILED: $bad $falsechecks"
  exit 1
fi
echo "SUMMARY: every scenario ALL EQUAL, no panic, 0 false checks, 0 non-finite states"

