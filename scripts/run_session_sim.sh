#!/usr/bin/env bash
# Builds and runs every session-protocol scenario (in-process, virtual clock; deterministic).
# Logs go to spikes/out/session-sim/; the RESULT lines are the evidence cited in docs/design.md.
# usage: scripts/run_session_sim.sh            (from any directory; needs only a Rust toolchain)
#        scripts/run_session_sim.sh --pe       the same scenarios on crates/pe-session through its fork
#                                              of this harness, crates/pe-session-harness (P2-3); logs in
#                                              spikes/out/session-sim-pe/
# PE_BIN=<dir> runs a prebuilt binary from <dir> without building; PE_OUT=<dir> writes the logs under
# <dir>/session-sim (or <dir>/session-sim-pe) instead of spikes/out (both used by scripts/run_matrix.sh).
set -u
cd "$(dirname "$0")/.."
pe=no; [ "${1:-}" = --pe ] && pe=yes
if [ $pe = yes ]; then
  [ -n "${PE_BIN:-}" ] || cargo build --release -q -p pe-session-harness || exit 1
  B=${PE_BIN:-target/release}/pe-session-harness
  O=${PE_OUT:-spikes/out}/session-sim-pe
else
  [ -n "${PE_BIN:-}" ] || cargo build --release -q -p pe-spike-session-sim || exit 1
  B=${PE_BIN:-target/release}/pe-spike-session-sim
  O=${PE_OUT:-spikes/out}/session-sim
fi
mkdir -p "$O" || exit 1
# Every log this run must write: a missing log, or one without a RESULT line, fails the run (a log
# that could not be written must never pass as ALL EQUAL; chunks-reviewer, P2-3 + D58 review).
expect=()
run() { local name=$1; shift; expect+=("$name"); "$B" "$@" > "$O/$name.log" 2>&1; echo "== $name"; grep "^RESULT" "$O/$name.log"; }
run c1 c1
run c2 c2
run state-frame state-frame
run diverged diverged
run loss loss
run ties ties
run claims claims
# P2-3 S4: grab races under D72 (only grabs claim; --pe only, the spike core has the earlier rule).
if [ $pe = yes ]; then run grabrace grabrace; fi
# P2-3 S5: a joiner catches up by state transfer (design 9.5, P22S-1; --pe only).
if [ $pe = yes ]; then run p2-3-statejoin statejoin; fi
run backdate backdate
for v in ok abort reject throttle forge late crash chain chain-epoch; do run r14-$v r14 $v; done
for v in ok abort reject chain; do run r14-$v-reverse r14 $v --reverse; done
# P2-5 controls, outside the pass/fail check below (both are expected NOT CONSERVED): the forge
# attacks with provenance checks off, and the double spend that provenance chains cannot stop.
mkdir -p "$O/p2-5-controls" || exit 1
for c in "r14-forge-no-provenance r14 forge --no-provenance" "r14-doublespend r14 doublespend"; do
  set -- $c; name=$1; shift; expect+=("p2-5-controls/$name"); "$B" "$@" > "$O/p2-5-controls/$name.log" 2>&1; echo "== control $name"; grep "^RESULT" "$O/p2-5-controls/$name.log"
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
# futureack's fix-off part is the only guard left on VC-5's epoch rule, and slowref's on VC4R-3: since
# trust survey #4, randsched's admin kick at GST ends the halt either leaves (review RA-1). Keep both.
for v in fork9 jointiming bogusprecommit futureack; do run p2-4-$v $v; done
# D80 admins and roles (design 10.8, 12.1): kicks in commit order, a kick across a partition, the last
# admin, closing a session with no admin, the join rules.
for v in kickwar kickpartition lastadmin adminclose joinrules; do run d80-$v $v; done
# V1 and V2 (user 08:31Z, design 12.4): kicking an admin needs its own permission; a roles-holder
# counts as an admin, so it keeps the session open when the last Admin leaves.
run d80-kickadmin kickadmin
# P2-4 review round 3 (design 10.8): each repro also reports its run with the fix switched off.
for v in ghostkick selfkickhead ackgap latejoin capchurn deepfork cutbackdate joinattests slowadmin kickbackdate lagprecommit aheadlink; do run p2-4-$v $v; done
# Trust survey #4: aheadlink's cut-off lasting 30 s, its misses 5 s late (long) or lost (cut): nothing removes 3.
for v in aheadlink-long aheadlink-cut; do run p2-4-$v $v; done
# Thin links Part B on: the same one-way cut at 4 and 8 members (@coordinator-opus 16:36Z).
for v in aheadlink-long-b4 aheadlink-cut-b4 aheadlink-long-b8 aheadlink-cut-b8; do run p2-4-$v $v; done
# P2-4 review round 4 (design 10.8): the same, for the round-3 findings and round 4's own changes.
for v in strikeframe aheadshield fakeref capstale lagchurn votechurn adminvoid joinsquat joinskip; do run p2-4-$v $v; done
# P2-4 review round 5 (design 10.8): the join wedge and the slow/fake-reference halt.
for v in joinwedge slowref prevotefork; do run p2-4-$v $v; done
for v in fakemix adminfork; do run p2-4-$v $v; done
# P2-4 review round 6, fixed in round 7 (design 10.8): three halts by one faulty member; each
# repro also reports its run with the fix switched off.
for v in capfake fakeadmin adminwedge; do run p2-4-$v $v; done
# P2-4 review rounds 7-8, fixed in round 8 (design 10.8): admin validity along the entry's own
# ancestry, a capper's fetch budget at the slow-member allowance, and a forker's prevotes in polkas.
for v in voidfork slowcapper voiddeep plainfork pfsilent; do run p2-4-$v $v; done
# The randomized Byzantine schedule generator (not a scenario file): a fixed seed range, checked
# against the written invariants (spikes/out/session-sim/INVARIANTS.md). A larger run is separate.
expect+=(randsched); "$B" randsched --seeds 12 > "$O/randsched.log" 2>&1; echo "== randsched"; grep "^RESULT" "$O/randsched.log"
# Review VC-1, a control outside the check below (expected to fork): three of six collude, above the
# bound of t(6) = 1 faulty member.
mkdir -p "$O/p2-4-controls" || exit 1
expect+=(p2-4-controls/onesided); "$B" onesided > "$O/p2-4-controls/onesided.log" 2>&1; echo "== control onesided"; grep "^RESULT" "$O/p2-4-controls/onesided.log"
run p2-4-vcscale-64 vcscale --members 64 --silent 2
run p2-4-vcscale-256 vcscale --members 256 --silent 2 --no-heads
# Fail on any fork, replay mismatch, panic, false check or lost/duplicated object (reviews NEW-2, NEW-7).
missing=""
for n in "${expect[@]}"; do grep -q "^RESULT" "$O/$n.log" 2>/dev/null || missing="$missing $n"; done
if [ -n "$missing" ]; then
  echo "SUMMARY: FAILED: missing or empty logs (no RESULT line):$missing"
  exit 1
fi
bad=$(grep -lE "MISMATCH|DIFFER|FAIL|PANIC|panicked|NOT CONSERVED" "$O"/*.log)
falsechecks=$(grep -hoE "checks ok [0-9]+ bad [1-9][0-9]*|nonfinite [1-9][0-9]*" "$O"/*.log | head -5)
if [ -n "$bad$falsechecks" ]; then
  echo "SUMMARY: FAILED: $bad $falsechecks"
  exit 1
fi
echo "SUMMARY: every scenario ALL EQUAL, no panic, 0 false checks, 0 non-finite states"

