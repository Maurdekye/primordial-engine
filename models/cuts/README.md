# models/cuts: model v0 of agreed cuts

The rules of agreed cuts (re-founding plan, docs/refound-plan.md 4.4; file 11, 3.3), written as a
[Stateright](https://crates.io/crates/stateright) model and checked exhaustively at small sizes, so pe-agree's code
follows a checked model (WS-model, M0). It is its own cargo workspace and is not part of the engine's build.

```
bash <team folder>/heavy.sh cargo build --release          # in models/cuts (its own workspace)
bash <team folder>/heavy.sh target/release/cuts-model.exe --threads 4 [check names...]
```

`cuts-model` runs every check (or the named ones) and exits non-zero if any outcome differs from its expectation:
a base check must hold every property and find every example; a control must give a counterexample. Each control's
counterexample is printed as its action trace, shortened by a shrinker (drop steps while the property still breaks).
Progress goes to stderr.

**Three kinds of search,** each labelled in the results:
- **Exhaustive:** Stateright's depth-first search over the whole reachable space (it keeps only fingerprints; its
  breadth-first search keeps the whole frontier and ran out of memory).
- **Bounded:** every behaviour of at most N steps, by `src/bounded.rs`: depth-first, keeping each state's
  shallowest depth and expanding a state again when a shorter path reaches it. Stateright's own depth bound is not
  exhaustive up to the bound (its visited mark hides a state first met at the bound), and its bounded breadth-first
  search ran out of memory at depth 17 for n = 4. Used where the full space does not finish: the n = 4 Byzantine
  space passed 184 M states, and the n = 3 four-view space 110 M, without finishing.
- **Random walks** (seeded, Stateright's simulation): only as an extra probe past the bound; a walk that breaks a
  property is a counterexample all the same, but no walk proves one holds.

## The rule modelled

- **Members and quorum.** n members, fixed membership. The quorum is D80 (b)'s (coordinator ruling, 2026-11-10):
  t = floor((n-1)/3), q = floor((n+t)/2) + 1. n = 3: q = 2, t = 0; n = 4: q = 3, t = 1.
- **Entries.** Each author signs a chain: seq, prev (the entry it follows), frame stamp, author, and the line id (pe-seams' name for the world's line; the world id in the plan)
  (bound into the signed bytes). A cut at height h covers stamps below `3h` (cuts on a fixed cadence of K = 3 frames).
- **Views and proposals.** Views 1, 2, ... are led in a seeded rotation. The leader of view v proposes a block: its
  parent (a certified block) and a cut naming, for each author, the last entry covered. A cut extends its parent's.
  The proposal carries the entries it covers.
- **Votes.** A member votes at most once per view, and only for a proposal of its current view whose cut it has
  checked: each author's newly covered segment links back to the parent's entry, every entry is signed by its author
  for this line, and is stamped below the cut's frame. q votes for a block are its certificate (QC).
- **Timeouts and the lock.** A member that gives up on a view sends a timeout carrying its highest QC, and stops
  voting in that view. q timeouts are a timeout certificate (TC) and move everyone to the next view. A proposal that
  does not extend the previous view's QC must carry a TC, and a member votes for it only if its parent is at least as
  high as the highest QC in the TC (the lock).
- **Finality (two-chain).** A block is final when its child, proposed in the very next view, is certified.
- **Verdicts** (coordinator: only a covered entry can be void). Entries a final cut newly covers take
  effect in chain order. One is void if stamped strictly below an earlier stamp in its author's chain (an equal stamp
  is valid, ordered by seq), or below the previous cut's frame (late). An uncovered entry stays pending: held, never
  applied, until a cut covers it. An entry whose (author, seq) a final cut covers with another entry is void (a fork).
  A member that lacks an earlier entry of a covered chain holds the later one as pending, and no basis for a vote.
  (The replica may still predict it, flagged unvalidated; the model has no replica.)
- **Fetch by name.** A member reports a cut final only once it holds every entry the cut covers; at least one honest
  voter holds each, so the fetch succeeds. Modelled as done at the commit.

The plan's "members vote with one hash in their next acknowledgement, and a quorum makes the cut final" leaves open
how a skipped proposer is survived without two quorums for different cuts; the two-chain rule with a lock (the
chained-HotStuff family: Jolteon, DiemBFT v4) is how v0 fills that gap, and the NoLockRule control shows the lock is
needed.

## Faults and the network

- **Network.** Every message sent stays in a set and may be delivered to anyone any number of times, in any order,
  or never: delay, reordering, duplication and loss.
- **Author fault** (member 0): runs the cut protocol but signs a fork of its seq 1, an entry back-dated below its
  predecessor, an entry forged in member 1's name, an entry signed for another line (another world), and sends any of them to any
  member at any time, or withholds them.
- **Byzantine** (member 0, n >= 4 only): author faults, plus its vote counts for every block (it equivocates), it
  times out with any certificate, and as leader it proposes any block on any certified parent with any valid
  justification and any cut, to each member separately.
- **Silent** (member 0): sends nothing.
- **What each check carries.** To keep each state space checkable, chain faults (fork, back-date) and signature
  faults (forge, replay) are checked in separate n = 3 runs, each with its controls. The n = 4 Byzantine checks
  are about quorums and locks: they carry no honest entries, and the Byzantine member's one entry reaches others
  only inside its proposals, where it tells its equivocating blocks apart.
- **Root.** Abstract, one bit: right, or wrong at one fault-injected honest member (`corrupt`). Votes are over
  (block, the voter's root), a certificate needs q votes for the same pair, and a member that sees a quorum certify
  a root it did not compute raises RootSplit.
- **Liveness profile.** Messages between protocol members are never lost, and timeouts fire only when every
  protocol member has nothing left to do (the network has settled, standing in for "after the timeout"). An end
  state is one where no honest member can make a step, whatever the faulty member has done or withheld; every end
  state must have a final cut at every honest member.

## Properties

| Property | Statement |
|---|---|
| agreement | Any two honest members' final blocks are on one chain (no two different cuts final at one height) |
| validity: chains | Every final cut's newly covered segments link to the parent's, are signed by their author for this line, and are stamped below the cut's frame |
| validity: void set the same everywhere | Per cut: any two honest members holding a cut as final reach the same verdict on every entry it covers, effective or void, with none left pending |
| no final frame undone | Every verdict an honest member has reached is the one the whole chain gives, so later entries never change it (D-1 as a property); and final cuts only extend |
| liveness | (liveness profile) every end state has a final cut at every honest member |
| examples (sometimes) | a cut is final; a faulty author's entry takes effect; an honest entry is void (late) |

## Checks and controls

Run on 2026-11-11 at 6f168e3f (4 threads, idle priority under heavy.sh; output in `results/v0-run.txt`). "States" are
unique states; "generated" counts every visit; seconds are wall clock on a shared machine.

**What is exhaustive and what is bounded.**
- **Exhaustive** at n = 3 with one author-faulty member, over one final cut (2 views, timeouts free): n3-chain and
  n3-sig check agreement, validity (chains and void sets) and no final frame undone, with every fork, back-date,
  forgery, replay and withholding of the faulty member's entries. This is the plan's M0 acceptance (agreement,
  validity, "no final frame undone", at three members with one faulty author); the d1 mutation gives its
  counterexample on the same configuration.
- **Exhaustive** with a silent member (n3-crash, 4 views), with a fault-injected replica (n3-root), and every
  liveness check.
- **Bounded** where the full space does not finish: n = 3 over 4 views (every behaviour of up to 20 steps; the full
  space passed 110 M states), n = 3 two content cuts deep (16), and every n = 4 Byzantine safety check (18; the full
  n4-byz space passed 184 M states). The quorum and lock controls give their counterexamples within the same bound
  (15 and 17 steps), so the bound reaches the attacks those rules exist for. n4-byz-chain is a light check at 14:
  chain faults are exhaustive at n = 3, and at n = 4 the Byzantine member's forked and back-dated cuts multiply its
  proposals (depth 16 passed 211 M states).
- **Random walks** (120 s each, seeded) on the two quorum twins reach depth 45-46 and find nothing.

**Fast profile** (for the gate, about 2 minutes): `cuts-model n3-chain n3-sig n3-crash n3-chain-live n3-crash-live
n4-byz-live n4-crash-live n3-root bound-n3-byz ctl-d1 ctl-unchecked ctl-quorum ctl-nolock ctl-nosig ctl-noroot
ctl-notimeout`. The bounded n = 4 checks take 1-17 minutes each and are for milestones.

| Check | Configuration | States | Generated | Depth | Seconds | Result |
|---|---|---:|---:|---:|---:|---|
| n3-chain | 3 members, one author-faulty with chain faults (forks, back-dates, withholds), safety | 13,223,520 | 102,593,905 | 33 | 28.0 | OK: all properties hold; examples found |
| n3-sig | 3 members, one author-faulty with signature faults (forges, replays), safety | 22,979,264 | 183,079,681 | 33 | 62.1 | OK: all properties hold; examples found |
| n3-crash | 3 members, one silent, 4 views (consensus: agreement through timeouts), safety | 8,614 | 23,215 | 28 | 1.0 | OK: all properties hold; examples found |
| n3-author-v4 | 3 members, author-faulty, 4 views (consensus; its full space passed 110 M states), safety | 53,961,546 | 272,224,274 | 20 | 172.1 | OK: (every behaviour of up to 20 steps) all properties hold; examples found |
| n3-chain-live | 3 members, author-faulty, chain faults, liveness | 15,048 | 75,917 | 28 | 1.0 | OK: all properties hold; examples found |
| n3-crash-live | 3 members, one silent from the start, liveness | 28 | 43 | 16 | 1.0 | OK: all properties hold; examples found |
| n3-chain-h2 | 3 members, author-faulty, chain faults, two content cuts deep, safety (depth 20 passed 247 M states without finishing) | 69,367,491 | 553,243,669 | 16 | 280.2 | OK: (every behaviour of up to 16 steps) all properties hold; examples found |
| n4-byz | 4 members, one Byzantine (equivocates votes, timeouts and proposals), safety | 65,689,946 | 356,708,986 | 18 | 201.0 | OK: (every behaviour of up to 18 steps) all properties hold; examples found |
| n4-byz-chain | 4 members, Byzantine with chain faults (fork, back-date) inside its proposals, safety; a light check (depth 16 passed 211 M states; chain faults are exhaustive at n = 3) | 70,270,241 | 390,817,337 | 14 | 180.2 | OK: (every behaviour of up to 14 steps) all properties hold; examples found |
| n4-byz-live | 4 members, Byzantine, liveness | 164,161 | 655,369 | 27 | 1.2 | OK: all properties hold; examples found |
| n4-crash-live | 4 members, one silent, liveness | 80 | 179 | 24 | 1.0 | OK: all properties hold; examples found |
| n4-byz-v4 | 4 members, Byzantine leads view 3 of 4 (the lock's case), safety | 216,739,069 | 1,804,077,838 | 18 | 1043.5 | OK: (every behaviour of up to 18 steps) all properties hold; examples found |
| n3-root | 3 members, author-faulty, member 2's replica computes a wrong root, safety | 692,420 | 4,021,834 | 30 | 2.1 | OK: all properties hold; examples found |
| n4-root | 4 members, Byzantine, member 3's replica computes a wrong root, safety | 23,060,008 | 119,371,277 | 18 | 70.0 | OK: (every behaviour of up to 18 steps) all properties hold; examples found |
| n4-byz-v4b | twin of ctl-quorum: 4 members, Byzantine leads view 1 of 4, safety | 89,681,578 | 438,606,488 | 18 | 260.2 | OK: (every behaviour of up to 18 steps) all properties hold; examples found |
| walk-n4-v4 | twin of ctl-quorum, unmutated, random walks for 120 s | 93,213,846 | 93,213,846 | 46 | 120.1 | OK: (random walks) all properties hold; examples found |
| walk-n4-v4-lock | twin of ctl-nolock, unmutated, random walks for 120 s | 88,802,616 | 88,802,616 | 45 | 120.1 | OK: (random walks) all properties hold; examples found |
| bound-n3-byz | 3 members, one Byzantine: t = 0, agreement must fail (why n >= 4) | 30,201 | 110,344 | 31 | 1.0 | OK: counterexample as expected: "agreement" (13-step trace after shrinking) |
| ctl-d1 | d1: vote without validating each chain in order | 97,846 | 482,225 | 32 | 1.0 | OK: counterexample as expected: "no final frame undone" (9-step trace after shrinking) |
| ctl-unchecked | accept the proposer's cut unchecked | 492,370 | 2,684,545 | 38 | 2.0 | OK: counterexample as expected: "validity: chains" (7-step trace after shrinking) |
| ctl-quorum | quorum one too small: split-brain | 83,573 | 273,893 | 18 | 10.0 | OK: (every behaviour of up to 18 steps) counterexample as expected: "agreement" (15-step trace after shrinking) |
| ctl-nolock | vote ignoring the timeout certificate's highest certificate | 5,356,583 | 29,491,245 | 18 | 20.0 | OK: (every behaviour of up to 18 steps) counterexample as expected: "agreement" (17-step trace after shrinking) |
| ctl-nosig | no own-hands / world-id signature check | 60,641 | 273,508 | 32 | 1.0 | OK: counterexample as expected: "validity: chains" (8-step trace after shrinking) |
| ctl-noroot | votes carry no root: a diverged replica confirms without learning it | 114,709 | 434,069 | 33 | 1.0 | OK: counterexample as expected: "root: a diverged replica learns it at the cut" (12-step trace after shrinking) |
| ctl-notimeout | no timeouts: a faulty proposer stalls the line | 1,513 | 4,802 | 17 | 1.0 | OK: counterexample as expected: "liveness: every end state has a final cut at every honest member" (0-step trace after shrinking) |


**The controls' counterexamples** (shrunk; full traces in `results/v0-run.txt`):
- **d1** (9 steps): the faulty author sends member 1 its back-dated entry and withholds the one before it; the cut
  covering the back-dated entry becomes final, and member 1, judging it alone, holds it effective; the whole chain
  makes it void. This is review D-1 as a property.
- **NoLockRule** (17 steps): a block is final at member 1 through views 1 and 2; the Byzantine leader of view 3
  proposes from genesis with a timeout certificate whose highest certificate is that block; members vote for it
  anyway, its child in view 4 is certified, and member 3 finalizes the conflicting block.
- **QuorumOneTooSmall** (15 steps): two quorums of 2 that share only the Byzantine member finalize different cuts.
- **The n = 3 bound** (13 steps): the same split with a quorum of 2 at t = 0. One equivocating voter at n = 3 is
  beyond what the quorum rule tolerates; it is why Byzantine members are modelled at n >= 4 only.

## v0.1 (coordinator conditions, @design-reviewer)

Results in `results/v0.1-run.txt` (24 checks at 7f2ee241, all as expected), `results/v0.1-frames-fast.txt` (n3-sig at
65792c74) and `results/v0.1-n2.txt`.

- **The proposer names each cut's frame** (condition 4). In v0 the cut at height h had frame 3h. Now an honest
  proposer names its parent's frame + K or + 2K (catching up after a stall), and the Byzantine proposer names + 1, + 2,
  + K or + 2K. A voter accepts a frame above the parent's and at most 2K above it. A cut covers stamps below its own
  frame, and an entry it covers is late if it is stamped below the parent's frame. A new property: frames only rise
  along a final chain, and nothing takes effect below an earlier final frame. Its control, **ctl-noframe** (voters
  accept any frame), breaks it in 7 steps; its twin **n4-byz-frames** holds for every behaviour up to 12 steps
  (9.0 M states). n3-chain is still exhaustive, at 45.3 M states (v0: 13.2 M), and so is n3-sig, at 76.6 M.
  - The lock checks (ctl-nolock, n4-byz-v4, walk-n4-v4-lock) keep fixed frames (`fixed_frames()`). Their attack is
    on certificates, and the Byzantine frame choice made ctl-nolock's 18-step space too big to search in 10 minutes
    (over 95 M states without reaching the split). With fixed frames, ctl-nolock gives its split in 30 s. This loses
    nothing the lock faces (@design-reviewer): the frame check is one more conjunct in `cut_ok`, so
    it only removes votes and can't make a certificate that wasn't there; the Byzantine member already makes distinct
    blocks in one view through `adversary_cuts`, so distinct frames add no new kind of equivocation; and the only new
    honest behaviour, + K against + 2K, is one the lock attack doesn't use.
  - **v0.2: every n = 4 quorum, lock and root check now uses fixed frames** (n4-byz, n4-byz-chain, n4-byz-live,
    n4-root, n4-byz-v4b, walk-n4-v4, ctl-quorum, and the controls built on `n4`). With the Byzantine member also
    choosing frames, n4-byz passed 454 M states at 18 steps in 33 minutes without finishing (integration run on
    6259b77c, 2026-11-11; v0 finished at 65.7 M). The argument above covers these checks too. The Byzantine frame
    choice keeps its own checks, n4-byz-frames and ctl-noframe, and honest proposers still choose frames in every
    n = 2 and n = 3 check.
  - **`FRAME_STEP_MAX` (parent + 2K) stands for "up to the voter's present".** It is not pe-agree's rule: a bound
    relative to the parent would need dozens of cuts to catch up after a 10 s stall, and a link whose round trip
    exceeds it would fall behind without limit. The upper bound's real job is to stop a proposer naming a frame in
    the voters' future, which would void as late the honest entries stamped below it but not yet sent. The model has
    no clock, so it can't tell that attack from honest lateness ("an honest entry is void (late)" is a witness of
    both); it checks only that frames rise and that late is measured from them. pe-agree's rule: above the parent's
    frame, and at most the voter's own present frame plus a small tolerance, with a harness test for a future-frame
    proposal.
- **n = 2** (condition 2): q = 2, t = 0, both members honest. **n2** is exhaustive (10.6 M states, two content
  cuts). Liveness has an early-timeout profile: up to P timeouts may fire before the network settles, standing for a
  timeout shorter than the round trip; after them, timeouts fire only at quiescence, as when an adaptive timeout has
  grown past the round trip.
  - **Measured:** at n = 2 one early timeout spoils its view, because the member that timed out no longer votes in
    it and q = 2. The two-chain needs two good views in a row, so P early timeouts alternating with good views hold
    finality off. With P = 3 the model is live in 8 views (2P + 2; n2-live-early, both leader offsets) and not in 7
    (**bound-n2-early**: certificates only in views 1, 3, 5 and 7).
  - **ctl-early** (early timeouts in every view, i.e. a timeout that never adapts) never finalizes. So at n = 2
    the adaptive timeout is needed for liveness at all, not only for speed.
  - The early profile lets blocks go P heights above the content (HMAX 6): a real chain has no height cap, and each
    spoiled view can cost a height.
- **The last cut needs a child** (condition 5): a liveness property, "every content cut ends final" (n2-live-h2,
  two content cuts). Its control, **ctl-skipempty** (a proposer with nothing new proposes nothing), fails it.
- **README notes from the review:**
  - Fetch by name is modelled as atomic at the commit. That is sound for safety: every voter checked the segment,
    and q >= t + 1 means at least one honest voter holds each entry. But the model can't fail on dissemination
    itself, so the fetch protocol and its thin-link worst case belong to pe-agree and the harness.
  - States are identified by 64-bit fingerprints, as in Stateright. At 2.2e8 states the chance of a collision is
    about 1e-3.

**Still to do before pe-agree's cut rules depend on the model (v0.2):**
- **Bounded progress: done in v0.2, below.**
- **Proposer choice** (condition 3): leader reputation (skip a member whose last view timed out) against keeping a
  proposer while its views succeed, chosen by measurement at n = 2 and n = 3. Either one gets its own control
  (condition 7).
- The bounded n = 4 checks (n4-byz, n4-byz-chain, n3 over 4 views) have not been rerun with the frame choice.

## v0.2

**Full run** at abdc8de3 (rebased as 96ee0c93), by @integrator-2 on 2026-11-11, 4 threads, idle priority: all 40
checks met their expectation, and all 14 controls and bounds gave their counterexamples. Raw output with the shrunk
traces: `results/v0.2-full.txt`. [V] Against v0: frames chosen by the proposer grew the n = 3 spaces (n3-chain 13.2 M
to 45.3 M states, n3-chain-h2 69.4 M to 282.9 M), and the n = 4 quorum, lock and root checks, which keep fixed
frames, are unchanged (n4-byz 65.7 M).

| Check | Configuration | States | Generated | Depth | Seconds | Result |
|---|---|---:|---:|---:|---:|---|
| n3-chain | 3 members, one author-faulty with chain faults (forks, back-dates, withholds), safety | 45,281,984 | 355,421,881 | 33 | 135.2 | OK: all properties hold; examples found |
| n3-sig | 3 members, one author-faulty with signature faults (forges, replays), safety | 76,634,368 | 617,897,121 | 33 | 249.9 | OK: all properties hold; examples found |
| n3-crash | 3 members, one silent, 4 views (consensus: agreement through timeouts), safety | 26,884 | 74,041 | 28 | 1.0 | OK: all properties hold; examples found |
| n3-author-v4 | 3 members, author-faulty, 4 views (consensus; its full space passed 110 M states), safety | 253,085,709 | 1,250,085,750 | 20 | 1157.1 | OK: (every behaviour of up to 20 steps) all properties hold; examples found |
| n3-chain-live | 3 members, author-faulty, chain faults, liveness | 233,634 | 784,805 | 28 | 1.1 | OK: all properties hold; examples found |
| n3-crash-live | 3 members, one silent from the start, liveness | 46 | 73 | 16 | 1.0 | OK: all properties hold; examples found |
| n3-chain-h2 | 3 members, author-faulty, chain faults, two content cuts deep, safety (depth 20 passed 247 M states without finishing) | 282,896,018 | 2,081,416,516 | 16 | 1427.3 | OK: (every behaviour of up to 16 steps) all properties hold; examples found |
| n4-byz | 4 members, one Byzantine (equivocates votes, timeouts and proposals), safety | 65,689,946 | 356,782,136 | 18 | 312.8 | OK: (every behaviour of up to 18 steps) all properties hold; examples found |
| n4-byz-chain | 4 members, Byzantine with chain faults (fork, back-date) inside its proposals, safety; a light check (depth 16 passed 211 M states; chain faults are exhaustive at n = 3) | 70,270,241 | 390,790,641 | 14 | 250.2 | OK: (every behaviour of up to 14 steps) all properties hold; examples found |
| n4-byz-live | 4 members, Byzantine, liveness | 228,864 | 825,480 | 27 | 2.2 | OK: all properties hold; examples found |
| n2 | 2 members, honest, two content cuts, safety (frames chosen by the proposer) | 10,589,788 | 34,261,590 | 38 | 29.0 | OK: all properties hold; examples found |
| n2-live | 2 members, liveness, timeouts only once settled | 224 | 342 | 20 | 1.0 | OK: all properties hold; examples found |
| n2-live-early | 2 members, liveness, up to 3 timeouts before the network settles, 8 views | 1,019,164 | 1,661,000 | 45 | 8.0 | OK: all properties hold; examples found |
| n2-live-early-o1 | the same, member 1 leads view 1 | 349,156 | 569,062 | 45 | 3.0 | OK: all properties hold; examples found |
| bound-n2-early | 2 members, 3 early timeouts, 7 views: alternating spoiled views hold finality off | 176,138 | 294,647 | 41 | 2.0 | OK: counterexample as expected: "liveness: every end state has a final cut at every honest member" |
| n2-live-h2 | 2 members, liveness, two content cuts: both end final | 859 | 1,517 | 25 | 1.0 | OK: all properties hold; examples found |
| ctl-skipempty | a proposer with nothing new proposes nothing | 301 | 467 | 24 | 1.0 | OK: counterexample as expected: "liveness: every content cut ends final" |
| n2-progress | 2 members, two content cuts, bounded progress (timeouts free) | 18,958,658 | 58,168,142 | 37 | 57.1 | OK: all properties hold; examples found |
| n3-live-h2 | 3 members, honest, two authors, two content cuts, liveness and bounded progress | 16,421,414 | 38,251,317 | 40 | 130.1 | OK: all properties hold; examples found |
| ctl-ownonly | a proposer covers only its own entries | 114,131 | 343,077 | 30 | 1.0 | OK: counterexample as expected: "progress: an entry all hold at h is covered by h + 2" |
| n2-live-early-h2 | 2 members, two content cuts, 2 early timeouts, leader reputation | 368,248 | 595,333 | 44 | 4.0 | OK: all properties hold; examples found |
| ctl-skipall | reputation without its window: skip every member that ever timed out | 6,738 | 10,774 | 41 | 1.0 | OK: counterexample as expected: "liveness: every content cut ends final" |
| ctl-early | 2 members, timeouts may always fire before delivery (no adaptive timeout) | 1,547 | 2,908 | 30 | 1.0 | OK: counterexample as expected: "liveness: every end state has a final cut at every honest member" |
| n4-crash-live | 4 members, one silent, liveness | 130 | 301 | 24 | 1.0 | OK: all properties hold; examples found |
| n4-byz-v4 | 4 members, Byzantine leads view 3 of 4 (the lock's case), safety | 216,739,069 | 1,803,875,044 | 18 | 1520.1 | OK: (every behaviour of up to 18 steps) all properties hold; examples found |
| n3-root | 3 members, author-faulty, member 2's replica computes a wrong root, safety | 2,275,248 | 13,415,359 | 30 | 7.3 | OK: all properties hold; examples found |
| n4-root | 4 members, Byzantine, member 3's replica computes a wrong root, safety | 23,060,008 | 119,376,207 | 18 | 110.1 | OK: (every behaviour of up to 18 steps) all properties hold; examples found |
| n4-byz-v4b | twin of ctl-quorum: 4 members, Byzantine leads view 1 of 4, safety | 89,681,578 | 438,588,360 | 18 | 443.0 | OK: (every behaviour of up to 18 steps) all properties hold; examples found |
| walk-n4-v4 | twin of ctl-quorum, unmutated, random walks for 120 s | 56,562,141 | 56,562,141 | 46 | 120.2 | OK: (random walks) all properties hold; examples found |
| walk-n4-v4-lock | twin of ctl-nolock, unmutated, random walks for 120 s | 53,438,086 | 53,438,086 | 45 | 121.9 | OK: (random walks) all properties hold; examples found |
| bound-n3-byz | 3 members, one Byzantine: t = 0, agreement must fail (why n >= 4) | 302,419 | 1,616,142 | 26 | 3.0 | OK: counterexample as expected: "agreement" |
| ctl-d1 | d1: vote without validating each chain in order | 1,376,315 | 7,928,287 | 32 | 5.2 | OK: counterexample as expected: "no final frame undone" |
| ctl-unchecked | accept the proposer's cut unchecked | 490,866 | 2,667,663 | 38 | 4.0 | OK: counterexample as expected: "validity: chains" |
| ctl-quorum | quorum one too small: split-brain | 85,788 | 284,661 | 18 | 10.0 | OK: (every behaviour of up to 18 steps) counterexample as expected: "agreement" |
| ctl-nolock | vote ignoring the timeout certificate's highest certificate | 5,222,379 | 28,523,861 | 18 | 40.0 | OK: (every behaviour of up to 18 steps) counterexample as expected: "agreement" |
| n4-byz-frames | twin of ctl-noframe: 4 members, Byzantine leads view 1 and names any frame from parent + 1 to + 2K | 9,004,284 | 50,385,881 | 12 | 50.0 | OK: (every behaviour of up to 12 steps) all properties hold; examples found |
| ctl-noframe | voters accept a frame not above the parent's (v0.1) | 286,169 | 1,309,090 | 10 | 10.0 | OK: (every behaviour of up to 10 steps) counterexample as expected: "no entry takes effect below an earlier final frame" |
| ctl-nosig | no own-hands / world-id signature check | 86,489 | 364,695 | 32 | 1.0 | OK: counterexample as expected: "validity: chains" |
| ctl-noroot | votes carry no root: a diverged replica confirms without learning it | 110,230 | 418,505 | 33 | 1.0 | OK: counterexample as expected: "root: a diverged replica learns it at the cut" |
| ctl-notimeout | no timeouts: a faulty proposer stalls the line | 1,509 | 4,119 | 16 | 1.0 | OK: counterexample as expected: "liveness: every end state has a final cut at every honest member" |


- **Bounded progress** (formulation by @design-reviewer; results in `results/v0.2-progress.txt`). The
  state records, per entry, the highest certified height any honest member knew when every honest member first held
  the entry *and its earlier chain* (`all_held_at`). The property: an honest entry with `all_held_at = h` is covered
  by the final chain by height h + 2 (the next proposal may predate the last delivery), when a protocol-following
  member proposed the cut at h + 2. It is checked within the content heights, and on in every liveness check and in
  n2-progress.
  - First run: "held" without the earlier chain failed n2-progress. A member held m1.2 but not m1.1, so no valid cut
    could include m1.2. That is a definition gap, not a protocol fault, and the chain condition fixes it.
  - [V] n2-progress (two content cuts, timeouts free) is exhaustive at 19.0 M states, and n3-live-h2 (three honest
    members, two authors, two content cuts) at 16.4 M. The liveness checks all still hold.
  - Control **ctl-ownonly** (a proposer covers only its own entries) fails it.
  - The stronger form, covered by h + 2 once any q - t honest members hold it, waits for votes that carry heads
    (S3 v1.1).

- **Proposer choice by measurement** (condition 3; `tools/leader_sim.py`, results in `results/v0.2-leader-sim.txt`).
  A discrete-event simulation of the same rule (chained Jolteon, cadence 50 ms, adaptive timeout 3x the slowest round
  trip, doubling per consecutive timeout), 60 s per run, fixed link delays, no jitter. It measures speed only; safety
  is the model's. Three rules: rotation; keep (the leader keeps the next view while its views get a QC); reputation
  (rotation, skipping the leader of the last timed-out view, DiemBFT v4 with a window of one). **Caveat: fixed link
  delays with no jitter; it measures speed only, not safety.** [V, simulated]:
  - **n = 2:** rotation and reputation finalize in 70 ms on 20 ms links and 300 ms with a 150 ms slow member; keep in
    90 and 600 ms. Keep's votes must return to the same leader, so each view crosses the slow link twice. With a fixed
    200 ms timeout keep never finalizes, while rotation and reputation still do: their timeouts fire only after the
    member has voted.
  - **n = 3, one slow member:** keep 90 ms, 20 cuts final per second; rotation and reputation 200 / 300 ms (p50 /
    p95), 8.6 per second, since the slow member leads every third view.
  - **A crashed member (n = 3 and n = 4):** rotation collapses to 2.1 and 4.0 cuts per second, with 510 and 720
    timeouts a minute; keep and reputation stay at about 20 per second, with 0 and 4-6 timeouts.
  - Caveat: the simulation decides each view's outcome globally, so every member derives the same leader; pe-agree
    must derive it from the certified chain to agree in the same way.
  - **DECIDED: reputation** (coordinator). It matches rotation at n = 2, where keep never finalizes
    under a short timeout, and survives a crash, where rotation collapses.
  - **A possible later departure, not adopted:** also skip a measurably slow member. The reason to revisit it is
    n = 3 with one slow member, where keep finalizes in 90 ms against reputation's 200 / 300 ms. It would need its own
    control.
- **Leader reputation in the model** (results in `results/v0.2-reputation.txt`). `leader_in(v, committed)`: rotation,
  skipping the leader of the latest view that timed out, as the proposer's committed chain shows it (a view between
  two consecutive committed blocks' views had no certified block; that view's leader is derived the same way from
  the chain before it). Every member derives the same leader from the same committed prefix. On in every liveness
  check (`live()`); the safety checks keep plain rotation, since the voting rules don't depend on who leads. The
  Byzantine member may lead a view if any protocol-following member's chain names it.
  - [V] All the liveness checks hold with it, including n2-live-early-h2 (two content cuts, 2 early timeouts) and
    n3-live-h2. bound-n2-early still fails at 7 views (2P + 2 unchanged).
  - Control **ctl-skipall** (reputation without its window: skip every member that ever timed out) fails "every
    content cut ends final". At n = 2, once both members have a timed-out view behind a committed block, nobody leads.

## Not in v0

- **Bounded progress** (coordinator, still to add): an entry an honest member holds is covered within a
  fixed number of cuts. It needs votes that carry the voter's chain heads beyond the cut, so the next proposer learns
  what it lacks and fetches it; v0's proposers cover only what they hold.
- Join, leave, return and a restarted chain (one key, a new chain id): v1. There the stamp floor must hold per key
  across chains, with a back-stamped first entry of a new chain as its control (@design-reviewer).
- Revoting after a repair, and RootSplit's handling past detection (WS-diverge). v0 checks that votes carry the
  voter's root and that a diverged replica learns it at the cut.
- Below quorum, the one-minute bound and split groups (v3); merging (v2).
- Votes ride in acks in the design; here they are separate signed messages. Acks' declared horizons are folded into
  entry stamps: every entry's stamp is a declared horizon.
- Leave and flood controls (@design-reviewer's L3) are not model properties: leave is v1, flood is a resource bound
  for pe-agree and the harness.
