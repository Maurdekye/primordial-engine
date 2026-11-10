# Divergence and recovery (WS-diverge)

Status: v1.10, 2026-11-10. Approved as a design by @design-reviewer (m4dcfeabea340), after their review of v0
(bf94698), @vdw-physics's answer on R-H1 and the coordinator's rulings of 20:40Z. v1.6 records the user's answers
of about 19:45Z (meef4f248923b, mfc2bbd9d28ad; file 14 section 8). v1.7 adds section 9.6, from the rulings of
19:50-19:59Z (files 18-20). Owner: @diverge-eng (WS-diverge). Reviewers: @design-reviewer, then @shader-reviewer.
Nothing in it is built. It specifies `crates/pe-diverge` and the requirements pe-diverge places on other
workstreams. Only WS-diverge edits this file and `crates/pe-diverge`.

Sources: the re-founding plan (`docs/refound-plan.md`, branch `docs/refound-plan` at b610464: section 3.1, the
seams in 3.2 and Appendices A and B); the review's file 14 (divergence and recovery), file 11 sections 3.0-3.3 and
file 13 Part A (`_teams/1847/fable-review/`); the user's rulings and the coordinator's decisions of 2026-11-10 in
`_shared/user-design-decisions.md`.

Marks: **[V]** verified in the repository or the named record; **[R]** a user ruling, quoted or cited by message id;
**[C]** a coordinator decision; **[I]** my design judgement, open to review. Every number marked [I] is a parameter
(section 14), not a constant.

## 0. The page in short

- **The goal it serves** [R mfc7e0ddc4137, 18:56:50Z]: the local user is never held up; everyone converges on one
  world line; divergences are repaired "promptly and with as little visibility or impact on the user's experience
  as possible".
- **pe-diverge is sans-IO.** It takes events, messages and time, drives a replica through seam S2, and returns
  messages for the repair lane. It owns no thread, socket or clock.
- **M1 builds four things:**
  1. detection: every final cut's root is compared with this machine's own root at that cut (section 3);
  2. the repair ladder's rungs 1 and 2: replay locally from the last cut that matched, then fetch only the objects
     that differ, verified by hash, and replay forward (section 4);
  3. the fault bundle: a self-contained, offline-replayable record of each repair past rung 1 (section 6);
  4. the counters (section 7).
- **It also hands WS-render the display layer's requirements** (section 8): corrections are blended, a player's own
  hands and what they hold are never corrected, one-shot effects fire once.
- **M2 builds the merge engine v1 and M3 v2** (section 9): one pure function, a three-way merge at main's final
  frontier [C 20:05Z]. It is drafted here for WS-model to model before anything is built.
- **What it needs from the seams** (section 11): a vote that carries the voter's root, a root-split event, roots
  kept for the last few cuts, tree nodes and object records served by hash, and a guarantee that equal roots give
  equal futures.

## 1. Scope

**pe-diverge does:**
- compare roots at final cuts and classify a mismatch;
- run the repair ladder (rungs 1-2 in M1; 3-5 in M3);
- resolve a root split where no root has a quorum (section 5);
- write fault bundles and keep the counters;
- compute merges (M2, M3), as a pure function that WS-agree calls [C 20:05Z];
- run the background self-check (M3).

**pe-diverge does not:**
- decide finality, order entries or void late entries: that is pe-agree's (S3);
- simulate: that is the replica's (S2), owned by WS-state and WS-world;
- draw anything: the display layer is pe-render's (WS-render). This file states its requirements only;
- guard connections: the version and self-test comparison is pe-net's (WS-net). pe-diverge consumes its result
  (section 5) and states what the guard must compare (section 10).

## 2. Terms

- **Cut:** a frame `f`, the last entry included for each author, and a root. Final once a quorum has voted for it
  (file 11, 3.3). Quorum: `t = floor((n-1)/3)`, `q = floor((n+t)/2) + 1` [C 20:05Z]; so `q = 2` at `n = 2` and
  `n = 3`.
- **Root:** the hash, 256 bits, of the replica's whole state at a frame (S1, S2). The tree under it runs from the
  world to cells to objects (file 11, 3.2).
- **My root at a cut:** the root this replica computes at frame `f` with exactly the cut's entries in effect, and no
  others. Entries this machine predicted with that are void under the cut are removed first (section 3.1).
- **Verified cut (`Cv`):** the newest final cut whose root this machine has matched. It is the base for rung 1 and
  for every merge.
- **Diverged:** a final cut's root differs from my root at that cut.
- **Main:** the line a quorum finalizes. It is never a host (R1; plan 5.2).
- **Branch:** a run of frames that one machine, or one group, simulated without a final cut it matched.

## 3. Detection

### 3.1 When a root is compared

- On each final cut `C` (S3 event `CutFinal`), pe-diverge asks the replica for its root at `C` (S2
  `root_at_cut`). The replica conforms first: it rolls back past any entry stamped at or before `C.frame` that the
  cut voids, and replays with the cut's entries only.
- A machine computes its root at `C` only once it holds every entry in `C`: the d1 rule [plan 4.2]. Before that
  it neither votes nor compares.
- Equal roots: `Cv := C`. The replica may release the roots of final cuts older than `T_keep` (section 14).
- Different roots: this machine has diverged at `C`. The ladder starts (section 4).

[I] Comparing at every final cut, not every frame, costs one hash per cut and finds a fault within one cut interval
(about 0.15 s on good links: 9 frames at 60 Hz [V file 12]).

### 3.2 The vote carries the root

Detection needs agreement to say which root a quorum holds. The proposal to WS-agree (S3, section 11.2) [I]:
- A proposal names the cut and the proposer's root.
- A member that holds every entry votes with one hash over `(cut, my root)`.
- Only votes with equal hashes count toward the quorum. So a cut is final only with a root that a quorum computed
  for itself.
- A member whose root differs still sends its vote. It counts for nothing, and every member sees the
  disagreement.
- If votes for one cut split so that no root can reach a quorum, agreement emits `RootSplit { cut, roots: [(root,
  voters)] }` to every member (section 5).

**Why a vote never waits for repair:** up to `n - q` diverged members do not block the quorum: one at `n = 3` to
`n = 5`, two at `n = 6`. At `n = 2` (`q = 2`) one faulty member blocks it, as do three members whose roots all
differ. Section 5 settles those cases.

Two consequences [design-reviewer, review of bf94698]:
- an honest member that has diverged counts toward `t`, exactly as a faulty one does;
- roots are visible in votes, so a voter could copy one without computing it. That is a fault within `t`, like
  any other lie; the background self-check (M3) is what catches it, on the copier's own machine.

### 3.3 Kinds of mismatch

pe-diverge only sees "my root is not the quorum's". It learns which kind of fault from what cures it:

| Cured by | Kind of fault (file 14, 2.3) | Example |
|---|---|---|
| Rung 1 | Transient, in live state only | A bit flip in a body; a replay-order bug that does not recur |
| Rung 2, once | Persistent in this machine's saved state | A corrupted saved root (F4) |
| Rung 2, then it recurs | Computational: this machine computes differently | A build or CPU that differs (F1, F3); a logic module that behaves differently (F2) |
| Not by repair | State outside the hash (F5) | Shows as two equal roots whose futures differ. Section 10 requires it never to exist |

## 4. The repair ladder

Each rung is tried only if the one before has failed. A rung succeeds when the replica's root at the diverged cut
equals the quorum's root. Throughout, the machine keeps simulating and drawing: no rung pauses the local user
[R mf7daea6446ca]. The display layer blends whatever the repair moves (section 8).

### 4.1 Rung 1: replay locally from the verified cut (M1)

1. Restore the root at `Cv` (S2 `restore`) on the replica.
2. **Verify it:** recompute its hashes from the records, ignoring every cached hash (S2 `verify`), **and
   recompute the physics hashes from the restored physics world itself**: a stored `physics_global` or `BodyState`
   hash proves nothing about the physics bytes behind it [@vdw-physics, `docs/spec/replica.md` 6.1, d21a809 on
   `ws/state`]. If either does not equal `Cv`'s root, the saved state itself is damaged: go to rung 2 at once.
3. Replay from `Cv.frame` to `C.frame` with the final log's entries (S3 `final_entries(Cv, C)`).
4. Compare the root with `C`'s.
   - Equal: the fault was transient. Replay on from `C` to the current frame with the entries held, predictions
     included, and carry on. Count a rung-1 cure (section 7). No bundle is written; a log line is.
   - Different: go to rung 2.

- **Network:** none. **Cost:** `C.frame - Cv.frame` frames of replay, normally one cut interval, plus one full
  rehash. [I] At M1's size (500 balls) this fits in the frame; from M2 it runs on a second copy (S2 `fork`) and is
  swapped in, as every deep replay does (file 14, 4.2).
- **If `Cv` is older than `T_keep`,** its root has been released: skip rung 1.

### 4.2 Rung 2: fetch what differs, patch, replay forward (M1)

1. **Choose a source:** one of the voters of `C`, the members that hold its root. Prefer the lowest measured
   delay; on a timeout of `T_peer`, the next one. A source is trusted for nothing: every byte is checked against
   `C`'s root, so a lying or faulty source costs time, never correctness.
2. **Descend the tree.** Ask the source for the child hashes of the tree nodes under `C`'s root (S2 `node`,
   served by the source's pe-diverge over the repair lane). Compare them with my tree at my root at `C`. Descend
   only into children that differ, a whole level per round trip. At the leaves are the differing objects.
   - [I] At M1's size the tree is about three levels deep, so the descent is about three round trips.
3. **Fetch** the records of the differing objects at `C`, with the hashes on their path to the root (S2
   `export`). Check each record's path against `C`'s root. Reject any record that does not check, and ask the next
   source for it.
4. **Patch** my state at `C` with the records (S2 `import`). Its root must now equal `C`'s. If it does not, the
   patch is incomplete: descend again against the patched tree, at most `N_descend` times, then fail the rung.
5. **Replay forward** from `C.frame` to the current frame, with every entry held since: final ones and
   predictions, my own included. This is the same replay as after a late entry.
6. **Record:** write a fault bundle (section 6) and count a rung-2 cure.

- **Network:** the descent plus the differing objects. **Seen by the user:** one blended correction of the
  objects that differed and of whatever they touched since `C`.
- **When repair data arrives late:** if, when the fetch completes, `C` is older than the replica can replay in
  the foreground (older than `W_fg`), restart the rung at the newest final cut instead. A persistent fault shows there
  too, and the descent is short. [I]
- **Cells and groups:** the tree's interior nodes are cells, and from M4 contact groups. S2 `import` patches
  objects inside a cell without reloading the cell [C 20:05Z, "per-group save, restore and step"].
- **A source that has released `C`'s root** answers "not held", and the next voter is asked.
- **Tables as well as objects:** governance, claims and the other world-model tables go through `export` and
  `import` the same way as objects.

### 4.2a Physics: one canonical image per domain

Physics state cannot be patched body by body. Box3D's step depends on state that belongs to no single body or
pair: body and contact ids come from free-list pools [V `crates/box3d-fork/src/id_pool.rs:36-44`, cited by
@vdw-physics], and a contact's solver colour depends on which contacts began earlier and on body ids
[V `constraint_graph.rs:75-130`, same]. Bodies written back through the API diverged at the first step
[V design.md 6.9, cited by @design-reviewer]. So, by WS-state's decision (`docs/spec/replica.md`, branch
`ws/state`):
- the replica's root hashes the world-model root together with pe-physics's full-state check hash over its
  canonical transfer bytes, which cover the pools, colours, contacts, warm starts, sleep timers and islands
  [V `image.rs:343-347`, `477-481`];
- **in M1, rung 2 for physics** fetches the source's whole physics transfer at `C`, checks it against the root's
  physics part, restores the whole physics world, and replays forward. Exact; about 1.2 MB for the M1 ball pit
  [V file 02 bench, cited by @vdw-physics]. World-model rows are still patched one by one;
- **from M2**, incremental images make the physics image a content-addressed chunk tree, and rung 2 fetches only
  the chunks whose hashes differ. Still exact, with traffic in proportion to what changed;
- the repair record format treats physics as "one blob tree per domain, by hash" from the start.

### 4.3 When rung 2 does not hold

- **The same objects diverge again** within `T_recur` of a rung-2 cure: this machine computes them
  differently. In M1, pe-diverge keeps curing them by rung 2 at each cut, at most `N_recur_max` times, marks the
  objects "repeatedly diverging" in the counters and the bundle, and raises a `RepeatedDivergence` event that
  pe-app shows the user, plainly: "this machine computes <object> differently; it is being kept in step from the
  others". [I]
- **M3 adds the higher rungs** (file 14, 4.3):
  - rung 3, reload the whole cell (or contact group) at the newest cut, behind a brief fade;
  - rung 4, rejoin from a cut, a visible reconnect;
  - rung 5, quarantine: the object or module that keeps diverging is frozen on every machine, by an ordinary
    entry, and reported.
- Until M3, M1's escalation stops at the event above: nothing in M1 freezes or disconnects.

### 4.4 Rules that hold on every rung

1. **No final frame is undone.** Repair moves this machine's state to the final root; it never changes a final
   cut, and never rolls back below `Cv`.
2. **No unverified byte is imported.** Every record crosses a hash check against a quorum's root.
3. **Repair never blocks the frame.** Replays reaching back further than `W_fg` run on a second copy (M2 on), and
   shallower ones in the foreground within the frame budget.
4. **The local player's own entries survive.** A repair replays them; it never drops them.
5. **Repair serves others while it runs.** A machine that is itself repairing still serves tree nodes and records
   of roots it has verified.

## 5. Who is right: root splits

- **With a quorum root** (at most `n - q` members differ): the quorum's root is right. The minority repairs
  toward it (section 4).
- **With no quorum root** (S3 `RootSplit`): typically two players, or three that all differ. Every member applies
  the same rule [I]:
  1. Every member runs rung 1 from its own `Cv` and votes again. Transient faults end here.
  2. If the split persists, every member **re-runs the physics self-test** at once, rather than reading its last
     result. A member whose fresh self-test fails, or whose connection guard reported a difference (WS-net,
     section 10), adopts another's root. At `n = 2` a fresh failure is the only real evidence of which machine is
     wrong.
  3. If it still persists, a **reference member** is chosen once per episode: the member whose root most voters
     hold, with ties going to the smaller `H(Cv's root ‖ member key)`. Every member that does not hold the
     reference member's root runs rung 2 toward it and votes again.
  4. **The choice is sticky.** It holds until a cut is final with matching roots, which ends the episode. Without
     this, a persistent computational fault changes the roots every cut, so a pick by root hash changes winner
     every cut, and both players see their machines patched toward each other in turn [design-reviewer].
  5. The split is reported, with a fault bundle, on every machine.
- **Why a hash of `Cv` and the key** [I]: no player is favoured in general, every member computes the same answer
  from data agreed before the split, and nobody can steer it by changing state. It is arbitrary by design. Between
  two machines with no third, nothing tells which computed correctly; the rule makes them agree, and the bundle
  reports it.

## 6. The fault bundle

Written on every repair past rung 1, and on every root split (file 14, 4.6).

**Contents:**
- the world id, the engine build id, the platform and CPU features, the self-test result;
- `Cv` and `C`: frames, roots and voters;
- the state at `Cv` for the cells (M4: contact groups) that hold the differing objects, with the hashes on their
  path to the root, so it imports into an empty replica;
- every entry from `Cv` to `C`, with signatures;
- the ids of the differing objects; their records under my root and under the quorum's;
- the rung reached, the sources used, and the times taken.

**Rules:**
- It is self-contained: `pe-diverge replay-bundle <file>` (an example binary of the crate) replays it on any
  machine and prints, per frame, the hashes of the differing objects. Two machines' printouts, or two builds',
  differ first at the first frame that diverged. [I: this finds the first differing frame offline, so the live
  repair never waits for it.]
- It is a local file, in the app's data folder. It is never sent anywhere without the user's action. It holds
  other players' inputs.
- On a non-admin's machine it is discarded with the world when they leave, unless they export it on purpose
  (9.6).
- At most `B_keep` bundles and `B_bytes` in all; the oldest goes first.
- Its format is versioned, and readable by `pe-harness` and by agents over MCP (WS-api: a `faults` resource).

## 7. Counters

pe-diverge keeps them, or collects them from their source through the events named; pe-app shows them, and MCP
exposes them (WS-api). They are measured in every harness scenario (plan 4.1, rule 4).

| Counter | Definition | Source |
|---|---|---|
| `cuts_compared`, `cuts_matched` | Final cuts compared at, and matched | pe-diverge |
| `diverged` | Mismatches found, by kind of state (field, governance, claim, body; from M2 logic, module) | pe-diverge, from the tree's leaves |
| `cured_by_rung[1..5]` | Repairs, by the rung that cured them | pe-diverge |
| `repair_latency` | From the `CutFinal` that showed the mismatch to an equal root, as a distribution | pe-diverge |
| `repair_bytes` | Bytes fetched for repair | pe-diverge |
| `repeated_divergence` | Objects marked repeatedly diverging | pe-diverge |
| `root_splits` | `RootSplit` events, and how each ended | pe-diverge |
| `bundles_written` | Bundles written | pe-diverge |
| `corrections_per_min` | Drawn objects whose drawn pose had to blend or jump after a correction, per minute | pe-render (section 8) |
| `visible_error` | Mean and max distance between drawn and simulated pose, over objects in view, per second | pe-render |
| `snaps_in_view` | Corrections that jumped rather than blended while in view | pe-render |
| `cueless_large_corrections` | Corrections in view larger than `E_cue` shown without a cue. Must be zero | pe-render |
| `own_error` | Error of the local player's own hands and held objects. Must be exactly zero | pe-render |
| `rollback_depth` | Deepest rollback, and the distribution | pe-replica (WS-state) |
| `voided_own` | The local player's entries voided as late (P5) | pe-agree, through S3 |
| `merges`, `merge_conflicts`, `merge_replay_frames` | From M2 | pe-diverge |
| `unconfirmed_time` | Time played since the last final cut, as a distribution | pe-agree (M3) |

## 8. The display layer: requirements for WS-render

pe-render builds it (plan 3.1, WS-render); WS-diverge states what it must do and how it is tested. The numbers are
parameters, to be tuned with a person in VR at the M1 checkpoint (plan section 8), within the user's targets
[R meef4f248923b, answer 8; file 14 8.3]:
- **no correction larger than a hand's width in view without a visible cue** (`E_cue`, section 14);
- others see an action within **0.25 s on good links and 1 s on poor ones**. That is a harness property over every
  network scenario, owned by WS-harness; the display layer's blends count inside it, so `T_blend` must leave room.

**D1. Two poses.** Every drawn object has a simulated pose (from S5) and a drawn pose (pe-render's own). The
simulation never reads the drawn pose.

**D2. Corrections blend.** When the simulated pose of an object moves discontinuously (a rollback, a repair or a
merge changed it), the drawn pose keeps its place and closes the gap:
- over `T_blend` (about 0.1 s [I], file 14 4.1), smoothly, with no overshoot;
- the gap is in position and orientation both;
- a further correction during a blend starts from the current drawn pose; it never jumps back.

How pe-render knows a correction happened: S5 marks, per object, that this frame's simulated pose was rewritten by a
replay since the last publication (a `corrected` flag, or the correction offset). This is a requirement on S5
(section 11.3).

**D3. Choose by size and by view** (file 14, 4.1):
- an error below `E_small` blends over `T_blend`;
- an error above `E_snap`, out of view, jumps at once;
- any correction in view larger than `E_cue` (a hand's width) carries a visible cue, whether it blends or jumps
  [R meef4f248923b]. The cue's form is WS-render's to design; `cueless_large_corrections` counts misses and must be
  zero;
- above `E_snap` in view: a fast blend over `T_fast`, or a jump if the blend's straight path would pass through
  another drawn solid. [I: the solid test may wait until after M1; until then, in view always blends fast.]

**D4. Never correct what the player holds.** The local player's own hands, and any object the local player holds in
the current predicted state, never receive a correction offset. `own_error`, the offset applied to them, is exactly
zero: a test, not a measurement [R m21f5bdac80b3]. On the desktop they are drawn at their simulated pose. In VR they
are drawn at pe-xr's late-latched hand pose, and a held object at that pose times the grip offset: presentation
ahead of the simulation by design, never a correction (`docs/spec/xr.md` 5.6; render.md 8, c85927f1 on
`ws/render`).
- When an object leaves the local player's grip because a race was lost (P3), it becomes blendable from that
  moment, starting from where the hand had it. It slides out of the grip over `T_blend` with a short cue (sound or
  haptic) [plan M1 demo step 4].

**D5. Remote hands blend freely,** with the same rules as objects. Guessing them between updates is WS-net's and
WS-state's (P2).

**D6. One-shot effects fire once.** Sounds, particles and haptics are keyed by the event that caused them. The key
is stable across replays [I]: `(frame, causing entry's hash or "physics", object id, event kind, ordinal within
that object's events that frame)`.
- A key already played is never played again.
- **Stale effects are not played.** A replay can reach back a long way: up to `W_fg` in the foreground, and up to
  `T_diverge` for a merge applied at its application frame. A one-shot effect whose frame is older than
  `T_stale_effect` before the drawn time is never played, even if its key is new; sustained state (a lamp that is
  now on) shows as state, not as an effect. So the set of played keys needs keeping only for `T_stale_effect`, not
  for a minute [I].
- A sustained effect whose event disappears after a replay is stopped, faded over `T_blend`.
- Effects of the local player's own actions play at once.
- Effects of others' actions not yet final may wait `D_effect`. [I] Default 0 s, for immediacy;
  file 14 section 6 doubts the delay is wanted.

**D7. Lost races and refusals are told,** not hidden: a lost grab has its cue (D4); a refused edit shows who won
(WS-api and pe-render, P4); an entry of mine voided as late is shown plainly (P5).

**D8. Display rate is not tick rate.** The same layer interpolates between simulated frames for any display rate
(R2). Interpolation is not a correction and does not count as one.

**Tests (WS-render's, in its crate, against pe-harness scenarios):**
- the drawn pose reaches the simulated pose within `T_blend` of the last correction, to within float tolerance;
- `own_error == 0` in every scenario, including lost grabs and repairs;
- a replayed event's effect fires once; an undone event's sustained effect stops;
- the counters of section 7 are reported for every network scenario;
- **a control that bites:** turning blending off must raise `snaps_in_view`; blending the local hand must make
  `own_error` non-zero, and the test must catch both.

## 9. The merge engine (M2 v1, M3 v2): draft for WS-model

Not built until WS-model's model v2 checks it [C 20:05Z, C8]. This section is the shape to model.

### 9.1 What the user asked for

- Q3b [R mf7daea6446ca], verbatim: "no player should ever wait for other players' actions to finish. if a length
  divergence occurs (>2 seconds), the diverging player performs a world line merge with main: main hands them back
  the state that they diverged from, everything is resimulated using the new rules from that point forward,
  including the actions the player took, and their changes to the world state are merged in".
- On reconvening [R 18:58Z], verbatim: "treat it like a git merge where every action is a commit to the main tree,
  but with deterministic rules for resolving it whenever possible".
- A delete against an add-on [R m35138bafc46a]: "if the delete occurred first, then the add-on is orphaned. if the
  delete occured second, then everything is destroyed".
- The bound [R m35138bafc46a]: one minute at worst; after it, the lines diverge for good, and may be re-merged later
  by choice from the full record [R 19:09Z].
- The coordinator's reading [C 20:05Z]: a three-way merge at main's final frontier; base, the diverged-from state;
  theirs, the returning machine's replay under the new rules; ours, main's final state; deterministic rules resolve
  conflicts; no final frame is undone. [C: an [I] reading, open to the user's correction.]

### 9.2 One pure function

```text
merge(base: Root, ours: Line, theirs: Line, rules: Rules) -> Merged
  Line   = { root: Root, entries: [SignedEntry] since base, footprint: per object, the frames it was written }
  Merged = { root: Root, kept: [entry], refused: [(entry, reason)], conflicts: [group], report }
```

- **Pure:** no clock, no randomness, no I/O, no arrival order. The same inputs give the same `Merged` on every
  machine. That is the property "merge determinism" (plan 4.4).
- **Inputs are agreed.** The merge runs as an ordinary entry. The returning machine (v1), or the first proposer
  after reconvening (v2), submits a merge entry naming `base`, the hashes of `theirs.entries`, and an
  **application frame `A = F + D`**. When a final cut includes it at frame `F`, every member computes
  `merge(base, ours = main at A, theirs, rules)` in the background and applies it at `A`. Main's frames before
  `A` stay as they were. [I]
- **`D` covers the compute.** Every member must replay up to a minute of `theirs` from `base`. The submitter sets
  `D` from the replay length at a conservative rate (`R_merge`, section 14), so that members finish before `A` and
  do not roll back a second time. A member that finishes late rolls back to `A`, as for any late entry
  [design-reviewer].
- **A member that has not yet built the code `theirs` needs** cannot compute the merge. It treats the merge as a
  pending code activation (Q3): it keeps predicting, does not vote on cuts at or past `A` until it has built the
  code and computed the merge, then rolls back to `A`. Past `T_merge`, it is itself a returning branch.
- **`theirs` is replayed under the new rules:** from `base`, with `theirs.entries` and the code in effect on main,
  so the returning machine's own actions are re-simulated (Q3b).

### 9.3 The rules, per group of objects

For every object, compare `base`, `ours` and `theirs` by the tree (equal hashes need no look inside):

| Since `base` | git's name | Result |
|---|---|---|
| Neither line changed it | unchanged | `base` |
| Only `ours` changed it | fast-forward | `ours` |
| Only `theirs` changed it | fast-forward | `theirs` |
| Both changed it to the same record | clean | that record |
| Both changed it differently | conflict | the group is resolved by replay (below) |

**Conflict groups.** A conflicting object, and every object it touched in either line (contact, a joint, a
parent, a script's reads and writes) since `base`, form one group, closed under "touched". Each group is resolved
by the engine's ordinary rules, applied once to both lines' actions [file 14, 4.4].

**A group with any bound body acts as the whole domain** [@vdw-physics, replica.md 6.3]: physics, and the rows of
every object with a body, are restored to `base` and replayed together. Physics can only be restored whole (4.2a),
so a group restoring only its own rows would leave bodies outside it re-stepped while their rows stayed in the
present: the body table would no longer equal the world model, and edits would be applied twice. Per-group restore
and patch hold for physics-free rows only.

1. start from the group's state at `base` (S2 per-group restore [C 20:05Z]; `GroupSel`, 11.1);
2. take both lines' entries on the group, as a set: an entry in both lines counts once;
3. order them by frame stamp, ties by the seeded rank (D37), as the engine always orders;
4. replay them on the group (S2 per-group step). The soonest grab wins; a stale precondition is refused with its
   reason; a later plain write wins; a delete and an add-on fall as the user ruled, by their order;
5. the group's state at `F` is the result.

**Voided entries are merged in, but are never back-dated past finality.** An entry of `theirs` that main voided
as late is in the set: that is what "their changes are merged in" means, and what the property "voiding and
merging fit together" checks (plan 4.4). How it is ordered depends on whether its stamp was witnessed.
**[R mfc2bbd9d28ad: confirmed by the user, as ruled by the coordinator in m61125c4194d4 and maf0da5275cdd,
1/1 residual included.]**

- **Witnessed:** an entry is witnessed when a signed acknowledgement by a member other than its author, on either
  side, references it, and that acknowledgement's stamp is within `B` of the entry's stamp. `B` is the normal
  back-dating bound: the acknowledgement interval plus slack, a duration set by WS-agree; about 4-10 frames, or
  70-170 ms, at 60 Hz [V design 8.2, cited by @design-reviewer]. A witnessed entry is ordered by its stamp like any other, and the user's order rule applies:
  a delete first orphans the add-on, a delete second destroys it [R m35138bafc46a]. This covers an honest split
  group, and a connected machine back from a long compile (Q3b), whose entries the others acknowledged in time.
- **Unwitnessed:** no such acknowledgement exists. A thin player whose entries arrived late is the usual case, as
  is an entry back-stamped by a silent member. Such an entry **loses to any final `ours` entry whose outcome it
  would change**, whatever the operation: an exclusive hold, a precondition, or structure. For example, a
  back-stamped delete that would refuse main's final writes to the object (`Deleted`), or orphan main's final
  add-ons. It is refused, with the reason "lost to a final entry". Where it changes no final outcome, it replays
  at its stamp. This matches the confirmed rule that the late or stale side loses conflicts.
- **A 1/1 split** (a two-player session): a conflict between two unwitnessed entries keeps stamp order, the user's
  rule. A hash would override that rule for honest pairs.
  [I: "loses" is read as refused. Rebasing it to the application frame `A` instead would let a back-stamped
  delete still destroy main's work at `A`, which is winning by another route.]
- **Plain writes** are safe under the ordinary order in both cases: a back-stamped write loses to main's later
  one.
- **Residuals, accepted by the user:**
  - colluders beyond `t` can witness each other's back-stamps;
  - in a 1/1 split, a lone cheater can back-date up to a minute against a lone honest player.
- **Why** [design-reviewer, High and re-review]: ordered by stamp alone, a member could stamp actions in the past,
  stay silent past `T_merge`, then return and win at the merge against honest players' final actions. A silent
  member publishes no horizon, so the 4-10 frame bound would widen to a minute. The window needs a silent member
  with no witness, which is exactly what the unwitnessed rule covers. Refusing every earlier entry instead, the
  general form, would contradict the user's delete-order ruling for honest split groups [C m61125c4194d4].
- **History:** the coordinator first ruled option (a), protecting holds and preconditions (20:40Z, m68977e8f9830).
  @design-reviewer showed structure must be protected too; the witnessed/unwitnessed split reconciles that with
  the user's ruling.
- **In v2** (split groups, neither side final), neither side has final entries to protect. What an unwitnessed
  entry loses to there is for WS-model's v3 to settle.

**Overlaps pull objects in.** An object only `theirs` moved, whose resulting bounds overlap an object `ours`
moved, joins the conflict group. So the merge never leaves interpenetrating bodies at the application frame
[design-reviewer].

**Things only one side touched do not change** (file 14, 4.4), except where the group closure pulls them in. That
is the "merge keeps work" property: an action is dropped only if the ordinary rules would have refused it with no
split.

### 9.4 v1 and v2

- **v1 (M2): one branch against main.** `theirs` is one machine's line: a long compile (Q3b), or a bad link past
  2 s (plan M3, thin links, which use the same engine). Below `T_merge = 2 s` no merge is needed: the ordinary
  rollback replays the change from its agreed frame. Above it, the machine keeps playing on the old code; when its
  compile ends it replays `theirs` on a second copy in the background, then submits the merge entry. [I]
- **v2 (M3): split groups.** Both sides may lack finality. `base` is the last cut both hold; both are lines of
  unconfirmed play; the merge entry is the first entry of the reconvened quorum. Within one minute the merge is
  automatic. After it, both lines know they have diverged (S3 `LineDiverged`) and become **branches**, as in git
  (9.6). pe-diverge records the branch point.
- **Merging later, by choice (M5):** section 9.6.

### 9.6 Branches, rejoining, and merges between any two worlds

The user's rulings of 19:45-19:59Z (file 14 section 8; file 18; file 20) [R meef4f248923b, m6987a48ed7f0,
ma9035760e0c1, mb3da953109cb, m2c5ed59eb9ab, m20c57d575b3f, md2e515db107c, mdad0d5626305].

**Who keeps a version.**
- There is no world owner. Every admin is a co-owner and keeps their own version, written as they play. While
  admins play together their versions are the session's agreed state.
- A non-admin holds the world only while playing; it is discarded when they leave.
- A session with no admin keeps nothing unless an admin joins before it ends, and every player sees a permanent
  mark saying so. An admin who arrives while it runs is offered its changes as a merge, under the usual rules
  [file 18 2.2, accepted].
- Having been an admin for any time gives a player the right to keep a saved copy as it stood up to the moment
  they stopped being one. That copy is a branch like any other.

**When a merge happens.** One function (9.2) serves every case; only the trigger and the consent differ:

| Trigger | Consent | Default |
|---|---|---|
| A split heals within `T_diverge` | None: automatic | The merge of 9.3 |
| A former admin reopens a world while another admin of it has it open (the rejoin) | Both: the rejoining admin and the admins present. If either declines, the rejoin is refused, and both stay branches [R mb3da953109cb]. With several admins present, any one refusal refuses [I, file 18 6] | Merge, unless the rejoining admin chose to stay forked [R ma9035760e0c1] |
| Admins choose to merge two branches, or two worlds, later | An admin of each side, after a preview computed in a copy [file 14 8.2] | The exact merge. The fast merge ("the later change wins, object by object", with the delete-order rule) is an option offered for long histories apart, never the default, and the preview shows what it would do differently [R meef4f248923b, answer 7] |

- **If only one version changed** since the two parted, there is nothing to merge, and one side is brought up to
  date. With no decision only when the side that changed is **the one already open**: a rejoiner who changed
  nothing is simply brought up to date. Bringing the open world up to a rejoiner's changes (for example, a former
  admin who kept editing their saved copy) takes the rejoin consent of the table above [file 18 2.7, 6;
  design-reviewer, review of 8edd08db].
- **Long merges** run on a metered background thread, however long they take [R meef4f248923b, answer 4].
- **Admins after a merge.** Adding and removing admins (any admin, either way [R mb3da953109cb, m2c5ed59eb9ab])
  is WS-gov's; the merge treats the admin list in two ways:
  - **an automatic merge** (a split healed within `T_diverge`) resolves it like any other table: a three-way set
    merge against `base`. Base's admins, plus additions on either side, minus removals on either side, with an add
    and a remove of one member decided by the ordinary order and the witnessed rule (9.3). So a removal made on one
    side during a short split survives, and no one is restored without being asked [design-reviewer, review of
    8edd08db];
  - **a consented merge** (a rejoin, or a later merge) returns both sides' admins together, as the user ruled
    [R m20c57d575b3f; file 18 7], and the preview lists every admin the merge would restore. The ruling chose this
    for exactly the case where one side removed someone, on the ground that agreeing to merge is agreeing to share
    the world. [C m031c67aeaf77: the union applies to consented merges only, since the ruling says it is "shown in
    the preview" and only consented merges have one; the user may overrule.]

**One origin.** Every world descends from one empty origin; preset worlds branch from it with preset content; a
world is a version, a point in one tree; creating a world is branching [R md2e515db107c, mdad0d5626305; file 20].
For the merge this means:
- **`base` always exists:** the last state the two versions share, found by walking their records back to their
  newest common stored state. At worst it is the origin. Two worlds made from the same preset share the preset.
- **Shared history is kept once.** Entries both records hold are the same entries, by id.
- **Worlds that share only the origin** (or only a preset, outside the preset's own content):
  - nothing both touched, so nothing is replayed, however long each history is;
  - the incoming world's own objects are placed by an offset the admins choose in the preview, by default beside
    the other and clear of it. The merge applies it to `theirs`'s objects that are not in `base`, as a whole-chunk
    shift, which is exact; shared preset objects are not shifted [@world-model r9]. Which fields shift (root
    transforms only, or every `Pos`-typed field, as @design-reviewer has asked) is WS-world's to settle, and the
    merge follows it;
  - the world-wide settings of the world merged into stand, unless the admins choose otherwise in the preview;
  - world-wide logic stays limited to the things it came with, unless an admin widens it: each object records the
    line it was born on (`Node.born_on`, into a hashed `lines` table [@world-model r9]), and WS-logic scopes by it.
    This wants a worked example before it is built (file 20, 7.4).
- These choices are inputs: `merge(base, ours, theirs, rules, options)` with
  `options = { placement: Offset, settings_from: Ours | Theirs | per setting, logic_scope, fast: bool }`.
  The default options give the exact merge with the defaults above. Options are part of the merge entry, so every
  machine computes the same result.

**What the record must carry, for the merge and the world line tool** [I; file 19, file 20]:
- every branch point (the cut it parted from) and every merge point (the merge entry, both parents), so the tree
  of versions can be walked;
- at each stored state, a small summary per player [C m578e520a173b]. pe-diverge computes it from the footprints
  (S2); WS-world stores it in the world record beside the stored state, signed with it but outside the root
  [@world-model, world-model.md r9 2.3, db29bd75]. Per member key: entries by class, edits applied and refused,
  the count of objects touched with a digest of their sorted ids, and the line id with any branch or merge in the
  interval;
- per branch, the measures the tool draws: time apart, and how much changed (objects changed on each side since
  `base`). The tool itself is a later item (@item:primordial-engine/world-line-visualization-tool-braided-glowing-li).

**Requirement R-W1, identity under one origin (WS-world, WS-seams).** Every world now shares the origin's first
entry, so a world cannot be named by the hash of its own genesis. A branch must also not start a new genesis, or it
loses the shared history a merge needs. **Adopted by WS-world** (world-model.md r9 2.3, db29bd75, in review):
`LineId` is the hash of the entry that created the line, a branch entry or the origin's; `LineId::ORIGIN` is pinned
by a test and never changes; a branch entry names its parent line and the final cut it branches from; a merged
line's head names both parents; a chain belongs to one line, so continuing on a branch starts a new chain. Entries
bind the line id they were made on (S1's signed world id becomes that line id); a merged line accepts entries made on a line in its ancestry **only up to that line's recorded
merge point** into it (its last included entry per chain), so both parents' signatures stay valid after a merge.
Anything made later on a parent line belongs to that line, and needs a new merge [design-reviewer, review of
8edd08db]. Object ids and chain ids that include the line id stay unique across branches. @world-model's
spec (section 2.2, `WorldId` as the genesis hash, "a fork gets a new genesis") needs this change; it is theirs and
@engine-architect's to settle.

**The fault bundle under these rules:** a bundle holds world state, so on a non-admin's machine it is discarded
with the world when they leave, unless they export it on purpose [I].

### 9.5 What the model must settle first

- Whether "ours" and "theirs" as states (the coordinator's three-way reading) and the per-group replay agree on
  every non-conflict object. The case to model: an object only `theirs` moved, which in the merged replay would be
  hit by something `ours` moved. The rule above takes `theirs` unchanged and lets the next step resolve any overlap;
  the alternative pulls it into the group.
- That no final frame is undone, with the merge applied at `A`.
- The back-dating rule above, with its controls [C maf0da5275cdd]:
  - a lone returner's back-stamped grab, and its back-stamped delete of an object main later edited, must lose;
  - a witnessed earlier delete must orphan main's add-on;
  - Q3b: a connected long-compile machine's earlier delete must orphan main's later add-on.
- The overlap rule, with its case: an object only `theirs` moved, overlapping one that `ours` moved.
- **Admin lists and fast-forwards (9.6)**, with controls: a removal on one side survives an automatic merge; the
  open world is never fast-forwarded to a rejoiner's changes without the rejoin consent; an entry made on a parent
  line after its merge point is refused by the merged line.
- **Physics and group-level merging** [@vdw-physics]. Under 4.2a, physics restores and steps a whole domain, so the
  exact reference is a replay of the whole domain from `base`. Until history-free physics exists (file 02 A2;
  file 11 3.1, "later"; about 6-10 weeks of fork work), a group only one side touched is not guaranteed to come out
  bit-identical to that side's branch: its contacts can take other ids and colours because of the other side's
  activity. **Ruled [C 20:40Z]: deterministic is enough, not bit-exact.** Every machine must reach the same
  merged state, and the visible result of a group only one side touched must stay unchanged within a stated pose
  tolerance (`E_merge`, section 14). That is a property checked in model v2 and measured in the harness. Hiding
  sub-visible differences is the display layer's job. History-free physics stays "later", as in the plan.
  - **The risk to measure** [@vdw-physics, I]: in a whole-domain replay, the other side's activity can give an
    untouched group's new contacts other ids and colours, and an active pile may drift past `E_merge` within a
    minute. The harness scenario (replica.md 6.3): a moving ball pit and a resting stack, merged after 1, 10 and
    60 s. If it fails, a fork fix cheaper than history-free physics may do: create and colour new contacts in
    canonical shape-pair order.
- That `merge` is independent of arrival order, and of which machine computes it.
- **Controls that bite:** ordering by arrival; voiding entries made during a split; dropping one side's
  non-conflicting work. Each must give a counterexample.
- [I] The merge's cost: how far a conflict group spreads over a minute of a busy pile (file 14, section 6). An
  experiment before the merge is promised (plan section 8).

## 10. Requirements on other workstreams

- **R-H1, equal roots give equal futures (WS-world, WS-state).** Every byte a step reads is under the root.
  **Met by WS-state's decision (4.2a):** the physics image, as one canonical blob tree per domain, is hashed under
  the root and restored whole. What follows was the problem. Today the per-frame physics hash leaves out sleep timers,
  contact caches, warm starts and islands [V `crates/pe-physics/src/world.rs:684-688`]; only the whole-image check
  hash covers them [V `crates/pe-physics/src/image.rs:345`]. Rung 2 patches objects; if their contact and solver
  state is not patched with them, the patched replica diverges again on the next step, and per-body import cannot
  work (4.2a). Test (WS-state, in its plan): assemble a replica from an imported root (the world-model subtree plus
  the physics transfer), and step it for 10 s of simulated time beside one that ran continuously; every frame's root must be
  equal. Negative control: importing per-body state only must diverge.
- **R-H2, everything that matters is hashed (WS-world).** Fields, governance, claims and cooldowns from M1; logic
  state and module versions from M2 (plan M1 acceptance; file 14 F5). A divergence injected in any one is found by
  the tree and cured by rungs 1 and 2: a test of pe-diverge's, per kind.
- **R-H3, verified restore (WS-world).** A restored root can be rehashed from its records, ignoring caches (rung 1
  step 2).
- **R-O1, orphaned add-ons (WS-world).** An edit that adds to an object deleted earlier in the order, for example a
  child parented to it or a component attached, is applied detached, at the world's root and keeping its world
  pose: it is orphaned, not refused [R m35138bafc46a]. A delete removes the subtree as it is at that point in the
  order, so a later delete destroys the add-on too. This is the ordinary rule with no split, so that the merge
  needs none of its own. It means an add-on does not take the parent's version as a refusing precondition.
  It must hold at any depth: an add-on under a grandchild of the deleted object is orphaned too, so a tombstone
  covers the deleted subtree, not only its root [design-reviewer, Low 4].
  **Adopted by WS-world** (spec r2, 2df5a04, branch `world/spec`) for objects: a create or reparent under a deleted
  object is orphaned at the root, keeping its world pose. Attaching a *component* to a deleted object is refused
  (`Deleted{by}`), since a row cannot exist without its object. That is the ordinary rule too, so the merge needs
  nothing of its own. **Confirmed by the user** [R mfc2bbd9d28ad]: a component attached to an object deleted first
  is refused, **with a notice to its author** (through the refusal's reason, as every refusal is reported:
  WS-api and pe-render, P4).
- **R-G1, the connection guard (WS-net).** Before a join, compare: the engine build id, the physics self-test's
  digest, and (from M2) each module's hash. A difference refuses the join with its reason (plan M1 acceptance). The
  last self-test result is part of a member's state that section 5 reads.
- **R-E1, stable effect keys (WS-state, through S5).** Events carry the key of D6.

## 11. Seams: what pe-diverge needs

A proposal to @engine-architect for seams v0, to be agreed before it is fixed. The names follow the plan's
Appendix A sketch.

### 11.1 S2, Replica

The sketch's `differing(&other: &Hash)` cannot be answered locally: a replica holds only its own tree. In its
place [I]:

```rust
pub trait Replica {
    // as sketched: step, save, restore, root_hash, permits
    fn root_of(&self, root: &Root) -> Hash;                    // the hash of any kept root
    fn root_at_cut(&mut self, cut: &Cut) -> Hash;              // conformed to the cut: its entries only (3.1)
    fn verify(&self, root: &Root) -> Result<(), Mismatch>;     // rehash from records, no caches (R-H3)
    fn node(&self, root: &Root, path: &TreePath) -> Option<Node>;  // child (key, Hash) pairs, or a leaf
    fn export(&self, root: &Root, ids: &[ObjectId]) -> Records;    // records, with their paths to the root
    fn physics_image(&self, root: &Root) -> PhysicsBlobTree;    // the whole domain's image, by hash (4.2a)
    fn import(&mut self, at: &Root, records: Records) -> Result<Root, ImportError>; // checks every path
    fn object_hashes(&self, ids: &[ObjectId]) -> Vec<(ObjectId, Hash)>;  // for replay-bundle
    fn fork(&self, root: &Root) -> Self where Self: Sized;     // a second copy, for background replay (M2)
    // M2-M3, for the merge [C 20:05Z]:
    fn group_restore(&mut self, root: &Root, group: &GroupSel);
    fn group_step(&mut self, frame: Frame, group: &GroupSel, ordered: &[SignedEntry]) -> Vec<Outcome>;
    fn footprint(&self) -> Footprint;                          // objects written or touched, per frame
}
```

- `GroupSel = Domain | Objects(BTreeSet<ObjectId>)`. M1 uses `Domain` only. A group containing any bound body acts
  as `Domain` (9.3) [agreed with @vdw-physics].
- **Settled names** [@world-model, world-model.md r5, d04394b; @vdw-physics, replica.md d21a809]: pe-world owns
  `node`, `export`, `import`, `verify` and `object_hashes`; pe-replica owns `physics_image(root)`, the receiver
  `import_at_cut(group, parts, source)`, and rung 1's `replay_from_last_agreed(source)`. pe-diverge calls those
  names; the sketch above is the shape, not the final signatures, which pe-seams v0 fixes.
- `Root` is a handle that keeps its structure alive, and is `Send`, so a worker can replay it.
- Roots of the final cuts of the last `T_keep` are kept, and served to peers: enough for repair.
- **Merge bases are kept separately.** A member keeps its `Cv` root, and the log since, for as long as it may be a
  branch (up to `T_diverge`, a minute), whatever `T_keep` says. Older bases come from the world record (M3).

### 11.2 S3, Agreement

What pe-diverge needs from pe-agree, beyond the sketch:
- `CutFinal { cut, frame, root, voters }`: the root the quorum signed, and who holds it.
- Votes over `(cut, the voter's own root)`, and `RootSplit { cut, roots: [(Hash, voters)] }` when no root can reach
  a quorum (3.2).
- A way to vote again on a pending cut after a repair (`revote(cut, root)`), for section 5.
- `final_entries(from, to)`: the final log between two cuts, for rung 1, the bundle and the merge.
- `Voided { entry, reason }`, as sketched: for `voided_own` and for the merge (9.3).
- **Witnesses** (from M2, for 9.3): for an entry, the signed acknowledgements by other members that reference it,
  with their stamps, so the merge can test "witnessed within `B`". They are kept with the merge base, for up to
  `T_diverge`.
- From M2: the merge entry is an ordinary entry; `Merged` and (M3) `LineDiverged` events (renamed from the sketch's `Diverged`, which is pe-diverge's root-mismatch event; @harness-eng 3b8fa6c);
  `Unconfirmed(time)` and `QuorumLost`, as sketched.
- A member's vote carries its last self-test result, or the guard's verdict is otherwise visible to all (5, step 2).

### 11.3 S1, S4 and S5

- **S1:** a merge entry, as a protocol payload (M2). No "rebased from" link is needed: voided entries go into the
  merge as they are (9.3) [I].
- **S4:** repair messages travel on the `Repair` lane. pe-diverge's messages: `NodeRequest { root, paths }`,
  `NodeReply { root, nodes }`, `RecordRequest { root, ids }`, `RecordReply { root, records }`. Unsigned: every reply
  is checked by hash.
- **S5:** per object, a `corrected` flag or offset (D2), whether the local player holds it (D4), and effect events
  with stable keys and a retracted list (D6).

### 11.4 pe-diverge's own interface

```rust
pub struct Diverge { /* state machine: Cv, the ladder's state, counters, peers */ }
impl Diverge {
    pub fn on_event(&mut self, ev: &agree::Event, replica: &mut impl Replica);   // CutFinal, RootSplit, Voided...
    pub fn receive(&mut self, from: PeerId, msg: &[u8], replica: &mut impl Replica) -> Result<(), Fault>;
    pub fn tick(&mut self, now: Frame, replica: &mut impl Replica) -> Vec<Outgoing>;  // Repair-lane messages
    pub fn counters(&self) -> &Counters;
    pub fn take_bundles(&mut self) -> Vec<FaultBundle>;   // pe-app writes them to disk
    pub fn events(&mut self) -> Vec<DivergeEvent>;         // Repaired{rung}, RepeatedDivergence, RootSplitEnded
}
pub fn merge(base: &Root, ours: &Line, theirs: &Line, rules: &Rules, replica: &mut impl Replica) -> Merged; // M2
```

- No method takes a clock, socket or random source. `receive` never panics on any bytes: bad bytes are a `Fault`
  naming the sender (plan 4.1).

## 12. Tests and controls

In `crates/pe-diverge/tests`, using pe-harness as a library (plan 3.1). Each property runs over the harness's
network model with random schedules.

**M1:**
1. **A transient fault:** one ball's live state is altered on B. Rung 1 cures it at the next cut; the roots are
   equal; no bundle; `cured_by_rung[1] == 1`.
2. **A persistent fault:** the same ball is altered in B's saved root too. Rung 1 fails its verify; rung 2 fetches
   that ball only (`repair_bytes` bounded by one object's records and the descent); a bundle is written.
3. **A computational fault:** B's replica perturbs one ball each step (a test-only mutation of the replica). Rung 2
   cures it each cut; after `N_recur_max` repeats within `T_recur`, `RepeatedDivergence` is raised; play never
   pauses.
4. **Every kind of state** (R-H2): the same as 2 for a field, a governance record and a claim.
4a. **A bound body:** rung 2 cures it by restoring the physics image (4.2a), with `repair_bytes` about one image.
5. **A lying source:** a peer serves a wrong record. It is rejected by hash; the next source cures it; no wrong
   byte is imported.
6. **Two players, a root split:** section 5's rule makes both agree; both report it.
7. **Late repair data:** the fetch is delayed past `W_fg`; the rung restarts at the newest cut and cures it.
8. **Bytes from a peer** (property test): no message on the repair lane panics pe-diverge.
9. **Replay of a bundle** reproduces the divergence on a clean replica, and prints the first frame that differs
   when run against the reference.
10. **No final frame undone, no pause:** across all of the above, the harness's properties hold (plan 4.1).

**Controls that bite** (each must make a test fail):
- importing records without checking their paths (test 5 must fail);
- patching at `C` but not replaying forward (test 2 must fail: the root at the next cut differs);
- comparing roots before conforming to the cut (false mismatches: `diverged > 0` on a clean run must fail);
- skipping the verify in rung 1 (test 2 must fail: rung 1 claims a cure that the next cut disproves);
- leaving physics caches out of the patch (R-H1's test must fail).

**M2-M3:** the merge properties of 9.5, first in the model, then on pe-diverge's `merge` with the model's traces
replayed through it (plan 4.1, rule 2); a merge scenario for each order of a delete and an add-on, at depth 1 and
at depth 2 [R m35138bafc46a]; a lone returner with back-stamped grabs, and with a back-stamped delete of an
object main later edited (both lose); a witnessed earlier delete, and a connected long-compile machine's earlier delete (main's add-on
is orphaned in both); a 1/1 split in both orders; the overlap case; a sticky root-split
reference under a persistent fault at `n = 2`. pe-harness wires the mutations `MergeByArrival`, `MergeDropsSide`
and `VoidSplitEntries` to its property P9 (`crates/pe-harness/DESIGN.md` 6.1, branch `harness/design`); pe-diverge
puts them behind its `mutants` feature.

## 13. Milestones

| Milestone | WS-diverge builds | Depends on |
|---|---|---|
| M0 | This specification; S2 and S3 agreed with @engine-architect | — |
| M1 | Detection; rungs 1 and 2; root splits; the bundle and `replay-bundle`; the counters; the display layer's requirements to WS-render; tests 1-10 | S2 from WS-state and WS-world (R-H1 to R-H3); S3 from WS-agree; S4 repair lane; pe-harness |
| M2 | The merge engine v1, after model v2 | WS-model v2; S2 fork, group restore and step, footprint |
| M3 | The merge engine v2; rungs 3-5; the background self-check; the one-minute bound's divergence, with WS-agree | WS-model v3; the world record (WS-world) |
| M5 | Merging later, by choice; rejoin merges; merges between unrelated worlds (9.6) | M3; R-W1 |

**A point for the coordinator** [I]: the user put a logic edit into M1 (U1, mc40fc6379bc6), and Cranelift moved into
M1 [C 19:52Z]. A compile on B longer than 2 s would then need Q3b's merge in M1. Below 2 s, the ordinary rollback
covers it. Unless M1's logic edit is kept small enough to compile in under 2 s on the second PC, merge v1 (and model
v2) move into M1. This file assumes the edit is small and the merge stays in M2.

## 14. Parameters

Every limit is a duration, never a frame count: the tick rate is configurable per world within a supported range,
60 Hz by default [R md4b42d16d9df]. A duration converts to frames at the world's rate, rounded up, **at the rate in
effect at the frame where the bound starts**, and that rate is recorded with any stored bound. A rate change
mid-session never moves a bound that is already running [design-reviewer].

| Name | Meaning | Default [I] |
|---|---|---|
| `T_keep` | Age of the final cuts whose roots are kept and served (and at least the last 2 cuts) | 1.5 s |
| `W_fg` | Deepest replay run in the foreground | 2 s: the old speculation window, `2 * hz` frames at any rate [V `spikes/session-sim/src/r18.rs:108`] |
| `T_peer` | Wait for a repair source before trying the next | 4 round-trip times, at least 100 ms |
| `N_descend` | Descents per rung-2 attempt | 3 |
| `T_recur`, `N_recur_max` | Time within which a repeat counts; repeats before the event | 1.5 s; 3 |
| `B_keep`, `B_bytes` | Bundles kept on disk | 50; 500 MB |
| `T_blend`, `T_fast` | Blend times | 100 ms; 40 ms |
| `E_small`, `E_snap` | Error thresholds | 5 cm or 10°; 1 m or 90° |
| `D_effect` | Delay of others' unconfirmed effects | 0 s |
| `T_stale_effect` | Age past which a newly seen one-shot effect is not played | 0.25 s |
| `E_merge` | Pose tolerance within which a group only one side touched counts as unchanged after a merge | Set from the M1 headset tuning: at most the display layer's sub-visible threshold. 5 cm or 10° is visible in VR at arm's length [design-reviewer] |
| `R_merge` | Replay rate assumed when setting a merge's application frame | 2x real time |
| `T_merge` | Divergence past which a merge replaces the ordinary rollback | 2 s [R mf7daea6446ca] |
| `T_diverge` | Unconfirmed play after which the lines diverge | 60 s [R m35138bafc46a] |
| `E_cue` | Correction in view above which a visible cue is required | a hand's width, about 10 cm [R meef4f248923b] |

## 15. Open points

- **For @design-reviewer:** the root-split rule (section 5); the vote-with-root proposal (3.2); rung 2 patching at
  the diverged cut rather than at `Cv` (4.2).
- **For @engine-architect:** S2 and S3 as in section 11.
- **For WS-world and WS-state:** R-H2, R-H3 and R-O1 (R-H1 is met by 4.2a).
- **For WS-model:** section 9.5.
- **For @world-model and @engine-architect:** R-W1, world and line identity under one origin (9.6).
- **Settled by the user** (mfc2bbd9d28ad, meef4f248923b): the back-dating rule (9.3); components on deleted
  objects (R-O1); branches after a minute and the later merge (9.4); the visible-error and visibility targets (8).
  Then files 18-20: no world owner, rejoin merges with both admins' consent, admin lists unioned, one origin
  (9.6).
- **Settled by the coordinator:** untouched groups need to be deterministic,
  not bit-exact (9.5), both at 20:40Z; M1's logic edit compiles in under 2 s (ma99622993534).
- **With the user:** nothing open for WS-diverge.
