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
- **Verdicts** (coordinator m33c08e879042: only a covered entry can be void). Entries a final cut newly covers take
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

## Not in v0

- **Bounded progress** (coordinator m33c08e879042, still to add): an entry an honest member holds is covered within a
  fixed number of cuts. It needs votes that carry the voter's chain heads beyond the cut, so the next proposer learns
  what it lacks and fetches it; v0's proposers cover only what they hold.
- Join, leave, return and a restarted chain (one key, a new chain id): v1. There the stamp floor must hold per key
  across chains, with a back-stamped first entry of a new chain as its control (@design-reviewer, md88ada36bfaf).
- Revoting after a repair, and RootSplit's handling past detection (WS-diverge). v0 checks that votes carry the
  voter's root and that a diverged replica learns it at the cut.
- Below quorum, the one-minute bound and split groups (v3); merging (v2).
- Votes ride in acks in the design; here they are separate signed messages. Acks' declared horizons are folded into
  entry stamps: every entry's stamp is a declared horizon.
- Leave and flood controls (@design-reviewer's L3) are not model properties: leave is v1, flood is a resource bound
  for pe-agree and the harness.
