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
  members are clean (the generator's honest members always are), is never removed at any honest member.
  [checked: no honest, non-crashed member appears in any honest member's evicted set]

## Liveness (hold after GST, within a bound)

* **L1 Progress, within a lag bound.** After GST, the members that are honest, not crashed and still
  members confirm within a bounded lag of their present: at the end of the run no such member's present
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
* **L2 Faulty members do not hold the session.** A faulty member that stalls confirmation — by going
  silent, by withholding or faking references, by a stale cap, by a ghost kick vote, or by a join
  request that reaches only part of the session — is removed, or its lever is neutralised, within a
  bounded time, so L1 holds despite it. [checked indirectly through L1; the named repros check each
  lever directly]

L2 has one accepted exception, the user's open question **V4**: a faulty member that keeps forcing view
changes by voting against members everyone hears, or by staying just inside every timer, is not removed
automatically. It costs latency, not safety (L1 still holds, with a higher confirmation lag), and an
admin can kick it. The generator does not fail a run for this.

## The generator

`pe-spike-session-sim randsched [--seeds N] [--from S] [--seed S] [--fixes-off HEX] [--verbose]`.

Each seed draws, from a splitmix64 stream: `n` in 4..=7; a faulty set of size 0..=t; one behaviour per
faulty member (crash, slow-send, selective/rotating send, fake references, a climbing ack epoch, a
withheld view-change vote, a bogus capped precommit each epoch, suspicion-vote churn, or a ghost kick
vote); an optional outside joiner whose request reaches a random subset of the members, optionally
crashing after its first prevote; and a GST after which the faulty members' network games stop (a crash
stays). It runs the deterministic simulator and checks S1-S5 and L1 on every seed.

`--fixes-off HEX` sets `WorldSim::fixes_off` on every member, so the generator can be run against an
older protocol to confirm it finds that round's holes. Coverage is reported in the round-5 note.

`randsched` found one hole the reviewer had not (RG-1): a faulty member that references a healthy
member's chain beyond what it holds, with a small live offset that hides inside normal lag, caps that
chain's stability without being caught as a withholder. The fix (a reference counts toward stability and
against a withholder only once the entry it names is held) is in `suspects_due`/`withholders_due`, under
`fix::VC4R3`, and `randsched --fixes-off 0x800000` reproduces the halt.
