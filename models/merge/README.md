# models/merge: model v2, the merge against finality

Model v2 of WS-model (M2 S7; docs/spec/divergence.md 9.5), checked before merge engine v1 is built. It models the
scoped merge v1 the coordinator ruled (D1): whole-world replays from the base, no grouping. 9.3's
grouping, fixpoint and read-write seeds are merge v2's (M3). Its own cargo workspace, outside the engine's build.

```
bash <team folder>/heavy.sh cargo build --release                  # in models/merge
bash <team folder>/heavy.sh <target dir>/release/merge-model.exe tier1 --threads 3
```

The target dir is the one heavy.sh routes this worktree to (its cargo routing script prints it).

## Layer 1: the merge as a pure function

- **The toy world** (src/world.rs): 9.5's sketch. A parent `p` and a free `x` with bodies in one domain, two
  creation slots (`c` for main's creates, `d` for the returner's: created ids are per author and segment), the
  settings row `g`, and the admin list. The step reads every body and `g` and writes the bodies that move; contact
  is a free choice per run. A grab of a raised body is refused (`Moving`), so a returning push can change a final
  grab's outcome through contact (9.5's example). A create under, or a reparent to, an object deleted first is
  orphaned; an attach to one is refused.
- **The merge** (src/merge.rs): 9.2's inputs at `fb = 0`, `F_M = 4`, `A = 6`; the agreed order (stamp, tie rank,
  author, seq), the tie rank one free bit per frame; `E_r = E_t \ E_o`; `Adm` by step 1's combined test and step 2's
  procedure with a per-chain budget; stage 1's state; stage 2 re-applying `E_o2`; the report. Each control is a
  `Mutation`.
- **The properties** (src/props.rs), independent of the merge: props.rs has its own `replay` and its own `Adm`,
  written from 9.2's text and pseudocode, and shares only the world's rules, the input types and the footprints
  (which only pick the inputs P-M5 and P-M6's fast-forward apply to). P-M1: the merge's `Adm`, step-1 verdict, every
  returning entry's outcome (`LostToFinal`'s named entry included), `S_1`, `S_A` and both override lists equal the
  definition's and the references'. P-M2: arrival order. P-M3: nothing below `A` changes. P-M4, causal: the admitted
  entries change no final outcome that the witnessed admitted entries alone don't, and each change has a witnessed
  entry before it. P-M5: the user's delete rule, for creates, reparents and attaches. P-M6: every returning entry has
  an outcome; all admitted when step 1 passes; fast-forward. P-M7: once; a second merge changes nothing. P-M8, 9.6's
  automatic merge: base's admins, plus additions, minus removals that take effect in the merge; a member both added
  and removed is the ordinary order's, so P-M1's. And 9.5's examples, plus the review's.
- **The enumerator** (src/enumerate.rs): every input within a profile's bounds, each multiset of entries once, with
  the tie rank both ways at each stamp the returner and main share.
  - `tier1`: 14 ops (grab, write, a precondition, gravity, delete, a create under each body, a reparent, attach, and
    admin changes of the returner, of main's member 0 and of member 3), member 0 on main, at most 3 entries, contact
    off or at one frame.
  - `step2`: 4 ops, at most 4 entries, contact off: where two chains' runs meet in step 2, for its two controls.
  - `full`: 30 ops, both main members, at most 3 entries (hours).

A run passes when the unmutated merge breaks nothing and reaches every example, and each control breaks the property
named for it. Controls stop at their first counterexample.

| Control | Breaks | What it does |
|---|---|---|
| `MergeByArrival` | P-M2 | orders the replay by arrival |
| `ApplyAtStamp` | P-M3 | applies admitted entries at their stamps on main's line |
| `NoWitnessCheck` | P-M4 | treats every returning entry as witnessed |
| `RefuseAllEarlier` | P-M5 | refuses every returning entry before a final entry it conflicts with |
| `MergeDropsSide`, `VoidSplitEntries` | P-M6 | drops returning work that conflicts with nothing; leaves out main's voided entries |
| `NoDedupe` | P-M7 | counts an entry in both lines twice |
| `UnionAdminsAlways` | P-M8 | takes the union of both sides' admins |
| `GlobalBudget` (step2) | P-M1 | one step-2 budget for all chains |
| `StaleAdmSoFar` (step2) | P-M1 | tests each run against the witnessed entries alone, not everything admitted before it |

## Results

| Profile | Inputs | Base | Controls | Time |
|---|---|---|---|---|
| tier1, 2026-11-27 (before @world-chunks's review) | 11,237,520 | holds P-M1..P-M8; 8 of 8 examples reached | 8 of 8 caught | 107 s base + ~15 s controls, 3 threads |
| tier1, 2026-11-27, after the review | 79,845,500 | holds P-M1..P-M8; 13 of 13 examples reached | 8 of 8 caught | 1,135 s base + ~170 s controls, 2 threads |
| step2, 2026-11-27 | 3,494,904 | holds P-M1..P-M8 | 2 of 2 caught (`GlobalBudget`, `StaleAdmSoFar`) | 109 s base, 2 threads |
| witness rule | 1,740 cases (n = 3), 6,540 (n = 4) | sound and complete; residual reached at n = 4 and the only gap | 3 of 3 caught | < 1 s |
| layer 2 | 10 scenarios, 15.6 k-1.43 M states each | holds everything; every sometimes-property reached | 12 of 12 caught | ~40 s |

**Found on the way:** with `N_UNW = 2`, a two-entry run whose first entry conflicts spends its whole budget on the
first test and the bisection, so its independent second entry is refused with `MergeBudget`. That is 9.2's stated
limit at a budget too small for the run; at 3 (and the engine's 16) later work is kept past one conflict.

**Settled in @world-chunks's review:**
- A main entry main voided as `Late`, which the returner received and applied, is a returning entry (9.2's intent;
  `E_o` counts main's applied entries). The spec's wording at "The returning entries" says `E_o`, which includes
  voids; that is the spec's to fix (@diverge-eng's), not the model's.
- An add against a remove of one admin is decided by the ordinary order and the witnessed rule (9.6), not by the
  bit-wise three-way formula the first P-M8 used.

**Abstracted, with the reason:**
- Holds don't affect the physics: a grab's outcome depends on the body's motion (`Moving`), which is how a push
  reaches a held ball here; the hand's own forces are pe-world's.
- No permission effects beyond the admin list: the admin ops stand for them, since a permission change refuses
  later entries the same way `NotAdmin` does.
- No `BackStamped` or fork verdicts: 9.2 never applies those entries in any replay, so they are the same as absent.
- The tie rank is one bit per frame (returner before or after main). With two main members (`full`), main's own
  order at a stamp is fixed by author.

**Only in `full`, or not reached (tier1's bounds):** main entries by member 1; contact at two frames; overrides in
`[F_M, A)` other than P-M1's `S_A` and `overridden2` comparison; budget exhaustion (9.5 measures it in the harness;
`step2` reaches the budget's per-chain rule); `AfterMergePoint` (a unit test); a second merge with work still to
admit (P-M7's second merge mostly has an empty `E_r`).

## Layer 2: the members (Stateright)

src/members.rs: two honest voters and the returner B, over a growing final log of cuts (one height per frame; the
agreement itself is models/cuts'). Each member learns cuts, fetches packages by id (verified by hash, from B or any
holder) and applies merges at `F_M + D`, and votes on a cut with the packages in effect there. A cut at height `h`
includes merge entries stamped at or below `h`, each chain as a prefix, and a merge entry only once a quorum holds
its package. The agreement may void an entry; B may go silent; two chains stand for B's `Rechain`.

Scenarios: a retry (two merge entries, one package); two successive packages (`P2` names `P1`), on one chain and
across a rechain; a faulty B serving two versions of its package; two packages for one segment; a faulty B's merge
entry stamped below its package's last intent (beside an honest one); two packages whose `Branch`es bind one parent
original; `M1` refused for good (stamped too low) with `M2` naming it; `M1` void with `M2` naming it; B going silent,
beside a voter's own entry. Each rule has a control:

| Rule | Control | Property it breaks |
|---|---|---|
| A package's id binds its content (r2 Fix 1) | `UnboundPackageId` | same package everywhere (and a false root split) |
| A fetch is verified by hash (9.3a amendment 2) | `NoFetchHashCheck` | no false root split (B serves two versions of one of its entries) |
| One package per segment | `AcceptSecondPackage` | same package everywhere |
| Once, whatever the retries | `NoRetryDedupe` | once |
| `M2` only after `M1` has applied (r2) | `M2AfterFinalOnly` | causal |
| `M2` is decided at its own final cut, refused unless `M1` is final and accepted at or below it (9.3a, @diverge-eng) | `M2WaitsForPredecessor` (r2 as first written) | never stuck: a member holding every final package can vote at every cut it knows |
| The vote waits for the merge (9.3a) | `VoteBeforeMerge` | no false root split |
| `M` is stamped above its intents (r2 Fix 1) | `MBelowIntents` | stamped: no intent takes effect at or before its stamp |
| Q1(b)'s originals deduplicated across packages | `NoOriginalDedupe` | once (per parent original) |
| `M` joins a cut only once a quorum holds its package (9.3a (b), ruled) | `IncludeWithoutQuorum` | available: every final package is held or served by a live member |
| Any holder serves (9.3a (a)) | `FetchFromBOnly` | available |
| Only B's chain waits for `M`'s quorum (amendment 1) | `HoldAllChains` | only B's chain waits: another member's ready entry can always join the next cut |

**Found on the way:**
- r2's "`M2` only after `M1` has applied" halts the line if `M1` is refused for good: no member can vote again
  (@world-chunks; `M2WaitsForPredecessor`'s counterexample). Hence the rule above.
- On one chain, `M1` is final at or before `M2`. Across a `Rechain` `M2` can be final first, and is then refused.
  When both are final at one cut, members apply in application-frame order, a predecessor before its successor at
  one frame: applying in log order applied `M2` alone once its package was fetched (an earlier version of this
  model did). Merge v1 and pe-app must order this way too.

**Not modelled:** amendment 3's resubmission of an `M` that reached a cut without its quorum (it goes `Late`, as any
entry, and B resubmits; the void and retry scenarios cover each half), and amendment 4's storage bound for an
unsubmitted package (a size cap and a time-out on what members keep: a resource rule, not a safety one). At `n = 2`,
a B silent after `M` is final is the ordinary two-member silent case.

## Layer 1b: the witness rule

src/witness.rs. Layer 1 takes the witnessed set as a free choice, so the merge is checked for every set; this checks
the rule that decides it (9.3, "Witnessed": signer other than the author, heads that cover the entry,
`f - s <= B`) against what it stands for, that an honest non-author held the entry within `B` of its stamp. Every
author, stamp 0-3, receipt frame (or never) per honest non-author, and carried statement, at `n = 3` and at `n = 4`
with one Byzantine signer of any statement (9.5's sketch). It must be sound and complete at `n = 3`; at `n = 4` the
collusion residual must be reached and be the only gap. Controls `WitnessByAuthor`, `NoBackdateBound` and
`NoHeadsCheck` must each break soundness.

## Not in the model

- **`BelowBase`** (r2 Fix 2): an intent stamped below the base is refused by name in `M`. It is outside 9.2's input
  domain (`E_r` is stamped at or above `fb` by definition), so merge v1 filters it before the merge and tests the
  filter with its own control, rather than this model.

## Not yet

- The full profile's run (about 2e9 inputs, hours) and random walks past three entries.
- Writing the controls' counterexamples and a sample of base inputs as JSON for merge v1's tests to replay (plan
  section 3); results/*.txt holds them as Debug text for now.
