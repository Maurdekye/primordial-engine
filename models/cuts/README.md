# models/cuts: model v0 of agreed cuts

The rules of agreed cuts (re-founding plan, docs/refound-plan.md 4.4; file 11, 3.3), written as a
[Stateright](https://crates.io/crates/stateright) model and checked exhaustively at small sizes, so pe-agree's code
follows a checked model (WS-model, M0). It is its own cargo workspace and is not part of the engine's build.

```
bash <team folder>/heavy.sh cargo build --release          # in models/cuts
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

RESULTS

## Not in v0

- Join, leave, return and a restarted chain (one key, a new chain id): v1. There the stamp floor must hold per key
  across chains, with a back-stamped first entry of a new chain as its control (@design-reviewer, md88ada36bfaf).
- Revoting after a repair, and RootSplit's handling past detection (WS-diverge). v0 checks that votes carry the
  voter's root and that a diverged replica learns it at the cut.
- Below quorum, the one-minute bound and split groups (v3); merging (v2).
- Votes ride in acks in the design; here they are separate signed messages. Acks' declared horizons are folded into
  entry stamps: every entry's stamp is a declared horizon.
- Leave and flood controls (@design-reviewer's L3) are not model properties: leave is v1, flood is a resource bound
  for pe-agree and the harness.
