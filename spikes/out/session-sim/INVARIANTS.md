# Session-protocol invariants (P2-4)

These are the properties the view change must keep, written down (coordinator's request after round 4)
so the randomized schedule generator `randsched` can check them, rather than the reviewer finding the
next hole by hand. They are stated for one world's session; design 10.8 is the prose.

## Fault model

* `n` members, of whom at most `t = floor((n - 1) / 3)` are faulty (Byzantine): they may crash, delay,
  reorder or withhold their own messages, send different messages to different members, equivocate on
  their own chain, sign bogus view-change entries, claim false references or epochs, or (as an outside
  joiner) deliver a join request to only part of the session.
* The quorum is `q = ceil((n + t + 1) / 2)` (D80 (b), the default). Two quorums share at least
  `2q - n > t` members, so they share an honest one: the safety bound.
* The links **between honest members** are reliable and eventually timely: a message an honest member
  sends another arrives within the network's delay and jitter, after a global stabilisation time (GST).
  Before GST the generator may delay or drop across the whole session; a faulty member's own links stay
  adversarial for as long as its drawn behaviour says.

## Safety (hold at every moment, every schedule)

* **S1 Agreement.** Over the lowest confirmed frame among the members that are honest, not crashed, and
  still members, every one holds the same per-frame history, and it equals the offline replay of the
  final log. [checked: equal digests, and equal to `replay(final_log)`]
* **S2 No rolled-back confirmation.** No member ever lowers its confirmed frame, nor changes an entry at
  or below it. [checked: `mark_dirty`'s assertion; a violation panics the run, which the generator
  reports]
* **S3 Integrity.** No member reports a false check-hash mismatch, and no member reaches a non-finite
  body state. [checked: `checks_bad == 0`, `nonfinite == 0`]
* **S4 Agreed membership and admin outcomes.** The members that are honest, not crashed and still
  members agree on the member set, the roles, and every admin entry's applied/refused outcome.
  [checked: equal member sets; the admin scenarios check outcomes]
* **S5 No honest removal.** A member that is honest and not crashed, whose links to the other honest
  members are clean (the generator's honest members are, but for those S6's check exempts, whom S5's
  check covers too), is never removed at any honest member.
  [checked: no honest, non-crashed member appears in any honest member's evicted set]
* **S6 No false accusation** (@coordinator-opus, 2026-11-01; review S6-1). A member that is honest, not
  crashed and clean is never voted against, to remove or to demote it, by any honest member.
  * *Clean* means the member is timely and keeps up. Timely: every message it sends reaches every honest
    member within W/2 frames of being sent (half the speculation window, 60 frames, 1 s at 60 Hz).
    Keeping up: it acks at its configured interval, and its declared horizon stays within W/2 of its
    present.
  * Timely, because W/2 is the shortest time any view-change rule waits for a member's evidence before
    acting on its absence: a capped precommit's polka, a kick vote's kick. A clean member can always
    supply its own evidence in that time, including the prevotes its precommit names, by fetch. So other
    members' delays never make an accusation of a clean member correct. That is VC4R-1's case: the
    missing prevotes were a third member's, and the capper was clean.
  * Keeping up, because the silence and demotion rules (and, until trust survey #4, the
    withheld-reference rule) judge a member's own
    responsiveness. A member that stalls (a paused process, an overloaded machine) may still have its few
    messages arrive promptly, but its horizon falls behind. It is not clean, and those rules' votes
    against it are what 10.9 item 7 prescribes (review S6-1).
  * Why S5 is not enough: with one faulty member, the honest members a false accusation fools plus the
    faulty member stay short of a quorum, so nobody is removed. The vote itself is the fault, as
    prevotefork's control checks.
  [checked: no honest member's event log holds a vote against an honest, non-crashed member, except the
  members the generator makes unclean: VC4R-1's drawn late members (round-0 prevotes held back past W/2)
  and VCR-12's slow acker (its horizon up to 1.5 s behind). With VC4R-1 and VC8R-1 off it fails on 5 of
  120 seeds, the first at seed 20; with every fix on, all 120 hold]

## Liveness (hold after GST, within a bound)

* **L1 Progress, within a lag bound, once the admin has kicked.** After GST, the members that are honest,
  not crashed and still members confirm within a bounded lag of their present. Since trust survey #4
  (user rulings 2026-11-01) a live faulty member that holds confirmation (fake references, a bogus cap,
  a ghost kick vote, acks climbing to an epoch no decision backs) is no longer voted out but left to an
  admin, so the generator models that admin: at GST the honest member with the lowest id kicks every
  such faulty member still in the session, and L1 is measured as before, at the end of the run, three
  windows or more after the kicks. So for the relaxed shapes L1 reads "until kicked": at the end of the run no such member's present
  minus its confirmed frame exceeds two speculation windows (about 4 s). The present is the member's
  clock frame (`now`, driven by the wall clock each tick), not its speculative sim frame: on a halt the
  sim stops one window ahead of the frozen confirmed frame, so `frame - conf` is pinned at one window
  however long the halt lasts, while `now - conf` keeps growing. A halt therefore grows its lag past the
  bound; a healthy or merely slow session keeps it small once the faults stop. [checked: the end lag
  `now - conf` of the live honest members. Two earlier versions were too weak: the first checked only
  that confirmation advanced at all, which a one-frame blip passed; the second measured the sim frame,
  which a halt pins one window behind the clock so it never reached the bound (review of round 6.1).
  Both were @design-reviewer's findings; the RG-1 control `randsched --seed 1 --fixes-off 0x800000`
  now fails L1 (lag 653 > 240 at the end, present 920, conf frozen at 267 since GST).]
* **L2 Faulty members do not hold the session, past an admin's kick.** A faulty member that goes silent,
  or whose own horizon falls behind, is removed by the silence and demotion rules; a proven equivocator
  is excluded and removed. A live faulty member that stalls confirmation any other way (withholding or
  faking references, a stale cap, a ghost kick vote, a frozen head nobody delivers, acks claiming an
  epoch no decision backs, or forcing view changes) holds the session until an admin kicks it: the
  user's trust principle (trust survey #4, which absorbed the open question **V4**). Its cost is
  latency, never safety. [checked indirectly through L1, with the generator's kicks at GST; the named
  repros check each lever and that the kick ends it]

The same rulings accept one cost for honest members: a live member with a one-way inbound cut (its own
messages reach everyone, it hears almost nothing) is caught by neither silence nor demotion, and holds
confirmation until its link heals or an admin kicks it (`aheadlink-long`, `aheadlink-cut`). The
generator draws no such member: its honest links are clean.

## The generator

`pe-spike-session-sim randsched [--seeds N] [--from S] [--seed S] [--fixes-off HEX] [--verbose]`.

Each seed draws, from a splitmix64 stream:
- `n` in 4..=7, and a faulty set of size 0..=t;
- one behaviour per faulty member: crash, slow-send, selective/rotating send, fake references, a
  climbing ack epoch, a withheld view-change vote, a bogus capped precommit each epoch, suspicion-vote
  churn, a ghost kick vote, or an equivocated view-change prevote (at GST the honest admin kicks the fake
  referencers, bogus cappers, ghost voters and climbers, L1);
- an optional outside joiner whose request reaches a random subset of the members, optionally crashing
  after its first prevote;
- a GST after which the faulty members' network games stop. A crash stays, and so does a ClimbAhead
  member's rotation (below): a faulty member's own links may stay adversarial.

It runs the deterministic simulator and checks S1-S6 and L1 on every seed.

Four review holes need a particular shape. The generator builds each a share of the time, from its own
stream, so the other draws keep theirs:

| Hole | Shape | Share |
|---|---|---|
| VC4R-1 (prevotefork) | An equivocator sends its value-prevote to k cappers and nil to the rest. h - q + 1 other honest members' round-0 prevotes reach everyone at least 1.2 s late, by every path. The remaining honest members observe. A capper counts q prevotes and caps; an observer counts q - 1 and judges the cap; k + 1 < q precommits decide nothing first | half the equivocator draws |
| VC4R-2 (joinwedge) | A faulty member crashes right after its first view-change prevote, and the join request reaches it and q - 1 honest members only | a quarter of the draws with a faulty member and a joiner |
| VCR-12 (lagprecommit) | Every faulty member withholds its view-change votes. One honest member acks once every 1.5 s, strictly on its grid, and the joiner asks everyone 2.7-2.95 s in, late in that member's period. Drawn only where the honest members are no more than q | a quarter of the draws with a faulty member |
| VC3R-1 | ClimbAhead: a member climbs to an unbacked epoch and delivers each of its own entries to one member only, rotating, for the rest of the run | a quarter of the remaining draws with a faulty member |

`--fixes-off HEX` sets `WorldSim::fixes_off` on every member, so the generator can be run against an
older protocol to confirm it finds that round's holes. Measured on 120 seeds each, after trust survey #4
(with the admin's kicks at GST) [V, relax-antistall]:

| Fixes off | Seeds that fail (of 120) | First | Invariant |
|---|---|---|---|
| none | none | - | every one holds |
| VC8R-1 (`0x1000000000`) | none | - | every one holds (before #4, with VC4R-1 off too: 5 seeds, S6; no rule accuses a capper now) |
| VC4R-2 (`0x400000`) | 3: 35, 58, 114 | 35 | L1 |
| VC-5's epoch rule (`0x8`) | none | - | every one holds (before #4: 6 seeds, L1) |
| VCR-12 (`0x2000`) | 8: 1, 22, 74, 76, 78, 100, 105, 106 | 1 | L1 |
| VC4R-3, RG-1 (`0x800000`) | none | - | every one holds (before #4: 8 seeds, L1) |

VC4R-1 and VC3R-1 are retired with the rules they fixed (their bits do nothing). Without VC-5's epoch
rule or VC4R-3's held-references rule, the halt each leaves is a live faulty member holding confirmation,
which the generator's admin now ends at GST, so L1 can no longer tell those fixes are off. The named
repros still show each: `futureack`'s fix-off control stops at frame 108, and `slowref`'s at frame 102.

Two generator bugs had hidden VC4R-1. A late prevote reached the nil side at once, through a third
member's repair. And the drawn late member could be the capper itself, whose precommit then waited
behind its own prevote, so the cap and the polka arrived together.

`randsched` found one hole the reviewer had not (RG-1): a faulty member that references a healthy
member's chain beyond what it holds, with a small live offset that hides inside normal lag, caps that
chain's stability without being caught as a withholder. The fix (a reference counts toward stability only
once the entry it names is held) is in `stable_seq`, under `fix::VC4R3`. Until trust survey #4 it also
fed the withholding rule, and `randsched --fixes-off 0x800000` reproduced the halt; since #4 the faker is
left to the admin, whose kick at GST ends it.
