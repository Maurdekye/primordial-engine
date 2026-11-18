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
- **On the seam (S2 v1.2):** steps 1-4 are one call, `Replica::replay_to(&C)`. It moves neither `Cv` nor the floor;
  pe-diverge compares the root it returns with `C`'s and, if equal, calls `finalize(C)` again, the only call that
  moves `Cv`. The replica replays within a per-call budget and answers `Pending` meanwhile; pe-diverge asks again
  with the same cut on each tick, and a later cut that agrees meanwhile counts as the rung's cure.
- **If the entries from `Cv` are no longer kept** (`NotKept`: S3 trimmed past `Cv`, S3 v1.2), or `Cv`'s saved state
  fails its rehash (`Damaged`), rung 1 fails at once and rung 2 runs.
- **pe-diverge never reads entries itself.** It does not assume that the entries stamped in `[floor, C.frame)` are
  the ones `C` covers (they need not be: a cut may include void entries and leave pending ones out, S3 v1.1). The
  replica replays `C` with exactly `C`'s entries, and pe-diverge only ever hands it cuts S3 gave this member in full
  (its final cut, or a cut it voted on, as a root split's).

### 4.2 Rung 2: fetch what differs, patch, replay forward (M1)

1. **Choose a source:** one of the voters of `C`, the members that hold its root. Prefer the lowest measured
   delay; on a timeout of `T_peer`, the next one. A source is trusted for nothing: every byte is checked against
   `C`'s root, so a lying or faulty source costs time, never correctness.
2. **Descend the tree.** First rehash my own tree at `C` (S2 `Tree::verify`). If it fails, my cached hashes are
   untrusted: damage behind a stale cache would look equal, be missed and come back later, so every leaf is fetched
   rather than diffed. Then ask the source for the tree nodes under `C`'s root (S2 `Tree::node`, served by the
   source's pe-diverge over the repair lane), and compare them with my tree at my root at `C`. Every node the source
   sends must hash (`Node::hash`, S2 v1) to what its already-checked parent named, starting from `C`'s root, and an
   objects-trie node must lie under the prefix asked for: a lying source is caught at its first false node, and the
   next source is asked. Descend only into children that differ, a whole level per round trip. At the leaves are the
   differing objects and tables.
   - **Shapes:** the objects trie is addressed by nibble prefix (S2 v1), so two tries of different shapes are
     compared prefix by prefix. Where one side has a leaf or a path-compressed segment and the other a node with a
     different prefix, the difference is structural: everything under theirs is fetched, everything under mine is
     removed, and what both have is fetched, not removed.
   - **Bounds:** at most `node_request_paths` paths and `record_request_keys` keys per request; a larger answer is
     split into replies of at most `reply_budget` bytes, and partial replies are collected until the request is
     answered. The descent needs no node cap: a node is taken only if it was asked for, only once, and only if it
     hashes to what its checked parent named, so no source can make a member take more nodes than the target tree
     has. The hash check is the bound, also for a member with no tree of its own or one far smaller than the target.
   - **Serving budget:** a source serves each peer from a byte bucket of `B_serve` that refills at `R_serve`. It
     answers a request while the bucket is above zero, and takes the reply's bytes from it. Past that it answers
     `Busy` with the wait until the bucket is above zero again, never "not held" (@shader-reviewer and
     @design-reviewer, re-review Medium A). A source still serves one physics image per peer and root.
   - **Pacing:** the requester keeps its own copy of each source's bucket, taking each reply's bytes from it, with
     half the burst and a rate a quarter lower to allow for the two clocks and the network's jitter. It holds back a
     request the source would answer `Busy`. A held-back request is not a silent source. So an honest repair is
     never refused, and only a flooding member's excess is.
   - **`Busy`:** the requester asks the same source again after the wait it named, at most `T_busy`. A source that
     answers `Busy` `N_busy` times in a row is rotated, so a source can't hold a repair with it. A source naming waits so long
     that `N_busy` of them outlast `W_fg` is left another way: the repair is overtaken at `W_fg` first, and the next
     starts at the next source. Leaving for `Busy` is counted apart from source faults (`busy_rotations`).
   - **Fetch time:** a repair overtaken past `W_fg` restarts at the newest cut, and a source serves a root for
     `T_keep`. So a fetch has to finish within the shorter of the two, which is 1.5 s. At the default budget that is
     about 6 MB of nodes and records with their proofs, around 3,000 objects at about 2 KB each, proof included [I].
     M1's ball pit is well inside it. A larger world needs a source to keep a root while a peer fetches it, which
     takes an S2 call to hold a root past `T_keep` (`Replica::hold`) and proofs built by the requester from the
     nodes it already checked. Both are M2 (@coordinator-opus m589748ac46a9); M1 states the limit.
   - **With no root of mine at `C`** (S2 v1.2 `Finalized::Damaged` or `RootError::Damaged`: a frame from my floor up
     to `C` is no longer kept, which only S3's time cap does, for a member more than `T_diverge` plus `T_keep` behind):
     nothing is diffed. Every leaf and the physics image are fetched, nothing is named absent, and the replica
     imports the whole state from empty, as a joiner does [pe-seams RULINGS, S2 v1.2 round 3]. Diffing against the
     floor's root instead is a later optimization.
   - [I] At M1's size the tree is about three levels deep, so the descent is about three round trips.
3. **Fetch** the records of the differing objects at `C`, with the hashes on their path to the root (S2
   `export`). Check each record's path against `C`'s root. Reject any record that does not check, and ask the next
   source for it.
   - **Inline blobs** (world-model.md 3.4; S2 v1.8; inline blobs phase 3b,
     @item:primordial-engine/inline-small-blobs-names-and-other-256-b-blobs-r): each record reply also carries the
     1-256 B blobs its records' rows reference, as far as the source has them (`Tree::export_blobs`). pe-diverge keeps
     each once, by hash, and hands them to the import (`CutImport.blobs`, ascending by hash). They need no proof of
     their own: a blob is named by its hash in a row that is proved. The replica refuses a blob no imported row
     references (`BadBlob`), and an import that lacks a small blob this machine does not have
     (`MissingBlob`). Either is the source's fault: it is left, as for a record that fails its proof, and the next
     source is asked [R @world-model m90bbd5b28176].
4. **Patch** my state at `C` with the records (S2 `import_at_cut`). Its root must now equal `C`'s. If it does not,
   the patch is incomplete: descend again, fetching every leaf this time, at most `N_descend` times, then fail the
   rung. The replica re-simulates to the present within its budget and answers `Pending` meanwhile; pe-diverge asks
   again with the same import on each tick (no peer is timed out while it waits). Then `finalize(C)` again moves
   `Cv`.
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
- **A newer final cut waiting:** when a repair at `C` ends while a newer final cut is pending, the replica's floor is
  already past `C`. `C` is not finalized again, since pe-replica counts a cut below its floor as a protocol fault
  (@shader-reviewer, re-review L1). The repair counts as cured, and the newer cut moves `Cv` next.
- **Silence:** only a reply that answers something asked restarts `T_peer`. An empty or unasked reply doesn't
  (@shader-reviewer, re-review L2).
- **A second descent** (after `RootMismatch`) keeps the physics image, which already passed the replica's check: a
  source serves one image per peer and root (@shader-reviewer, re-review L3).
- **Tables as well as objects:** governance, claims and the other world-model tables go through `export` and
  `import` the same way as objects.
- **Script modules** [C meaa5b7bf57e9, with @fable-reviewer's conditions]: an import can bring rows that name modules
  the importer does not hold. Modules are never sent (logic.md 2, Q3c): the importer fetches the script's source
  blobs on the bulk lane and builds the module itself, over the same polled path as an activation (logic.md 6.2).
  - **Holds.** An import resets the replica's holds to what the imported state itself requires: a hold from before
    the import, for an activation the import jumped past, would sit at or below `C.frame` and never clear
    [design-reviewer, Low 1]. Each module the importer lacks holds this machine's votes (S2 `Replica::vote_holds`)
    from `C.frame`, or from the row's `activate_at` if that is later. pe-diverge never needs the missing set.
  - **Finished.** The repair is finished only when `vote_holds()` names no frame at or below `C.frame`. Until then
    pe-diverge asks again on each tick, as for `Pending`, and counts the cure when the holds clear. Holds above
    `C.frame` are ordinary pending activations, not the repair's. Activation itself never waits: the module is
    active at its row's frame on every machine, and readiness changes only whether this machine may vote.
  - **A module that does not come** [C m571b81cfbaa6]: a repair still waiting on its holds past `T_module` restarts
    at the newest final cut, as a repair overtaken past `W_fg` does, and the wait counts toward the stop rule (4.3)
    under its own failure key, `ModuleMissing`, naming the module hashes. A module no source serves then stops the
    ladder with a bundle that names it, instead of holding the vote forever. Waiting without a limit is ruled out.
  - **Why not `W_fg`** [V sizes, I times]: the wait is the source's transfer plus a clean build. The thin links run
    at 8-32 kbit/s, 1-4 KB/s (plan, M1 thin links), so `W_fg`'s 2 s carries 2-8 KB of source. The lamp's source is
    1,438 bytes, but the 6.4k-line benchmark guest's (`spikes/iter-bench/guests/logic_mono`) is 188,839 bytes:
    189 s at 1 KB/s, 47 s at 4 KB/s (16,595 bytes gzipped, if the bulk lane compresses). Its clean build takes
    1.3 s, and a physics-sized module's 15-20 s (logic.md 6.3). So `W_fg` covers only the smallest scripts.
  - **`T_module`, from the modules the repair lacks:** `T_module = W_fg + ΣS / R_thin + n T_build`, over the `n`
    modules the repair lacks, with `S` the bytes of each script's source tree, `R_thin` = 1 KB/s (the thin links'
    floor) and `T_build` = 20 s (the measured top). The sizes add because the sources share one thin link, and the
    builds add because v0 builds one at a time, in order (logic.md 6.5); taking the largest instead would time out
    an honest repair whose modules were arriving, and count a false `ModuleMissing` [design-reviewer, r2 Medium].
    The benchmark guest alone gets 2 + 189 + 20 = 211 s, two of them 2 + 378 + 40 = 420 s, and the lamp 23 s. It
    is computed from sizes because the world sets no cap on a script's source: the bounds are a blob's 64 MiB and a
    transaction's 256 MiB (world-model.md 3.4), which at 1 KB/s take 18.6 hours and 3 days. R-L1 (10) asks WS-logic
    for a cap, and for the source tree's total size in the row.
  - **Until R-L1, a deadline that grows as sizes become known** [design-reviewer, r2 Low and r3 Medium]: the row
    names only the source tree's root, by hash (logic.md 6.1), and a large tree's own nodes do not fit in 2 s (about
    20 KB for a 301-file tree, 20 s at 1 KB/s [I]). So the deadline, from the import, is
    `W_fg (1 + d) + B_known / R_thin + n T_build`, where `B_known` is every byte known so far, nodes and blobs, and
    `d` the deepest tree level reached. It is recomputed each time a node arrives naming more, from the lengths it
    gives [I: that nodes name each child by hash and length, as a `BlobRef` does (world-model.md 3.4), is for
    WS-logic to confirm]. The repair restarts and counts `ModuleMissing` when the clock passes the current deadline.
    Each level then gets its transfer time once its size is known, and `W_fg` is a latency allowance per level,
    since the row's ref gives no length for the root. Once every blob is known, it equals `T_module` plus the
    levels' latency.

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
- **A repair refused again and again stops the ladder** (@coordinator-opus m21ea2a0f11b2 item 5, m41efa3f06e8a;
  @fable-reviewer's point 4). The case: rung 2 fetches and checks the agreed state, and the replica still cannot
  take it, for instance a joiner whose world config differs. Without a stop, every final cut starts the same
  repair again and writes another bundle.
  - **What counts:** a rung-2 failure with the agreed state in hand: the import refused (the patch stayed
    incomplete), the cut's state no longer kept, the physics image not enough, finalize not agreeing after an
    import that matched, or a rate mismatch. A failure for want of data does not count, and does not reset the
    count either: every source silent or busy, no source at all, a repair overtaken. No inputs arrived, and a
    network outage must not stop repair. A root split's repair is not counted; section 5's rules end it.
  - **The key:** the rung, the failure's reason, and the differing leaves found, sorted (`Config` among them when
    the world's config differs).
  - **The stop:** after `N_stuck` counted failures in a row with one key, or `N_stuck_any` in a row with no cure
    whatever the key (a failure whose key alternates, A, B, A, B, would otherwise never stop). A rate mismatch
    stops at once: every honest source holds the same config, so no retry or other source can cure it
    (@design-reviewer m467dddd5ba66). Its key's leaves are `Config` alone, its whole cause: the other differing
    leaves drift from probe to probe in a live world and must not make each probe a new key (@shader-reviewer).
  - **On a stop:** one bundle, its `stuck` list naming each key (for `N_stuck_any`, every key seen since the last
    cure), and one `RepairStuck { cut, rung, differing, why }` event (S3.8 v1.4), which pe-app shows the user as it
    shows `RepeatedDivergence`. The config leaf is named first. Final cuts are still compared (`stuck_skipped`).
  - **While stopped:** no rung runs, except one probe repair every `T_stuck_probe` [I: detecting "the key changed"
    needs a repair attempt, and an attempt costs a fetch]. A probe that fails with the same key writes nothing new.
    One that fails with a different key restarts the ladder, counting that key from one (not after an
    `N_stuck_any` stop or a rate mismatch, which only a cure ends). A `Damaged` final cut waits for the probe too,
    up to `T_stuck_probe`, like any other diverged cut.
  - **A cure ends it:** a cure by any rung, or a final cut whose roots agree, clears both counts and the stop.
  - **Unchanged:** the case above, where cures hold and the same objects diverge again, keeps curing. Nothing in M1
    freezes or disconnects.
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

### 4.5 A join from a final cut (M1)

A machine that joins a running world (app.md 4, "B's state at E") starts with an empty replica at the reconfiguring
cut `E`, a final cut whose certified root it holds (S3 `Node::join`, `Node::start_root`). It must not step until it
holds `E`'s state. `Diverge::join(e, inviter, replica, now)` fetches that state by rung 2's whole-state path, the one
a `Damaged` cut takes (4.2): every leaf and the physics image, nothing named absent, imported from empty
[R @engine-architect md07aea6d29ef, mbccffa588899]. The small blobs the rows reference arrive with the records
(4.2 step 3), so a joiner holds its world's names.
- **Sources:** the inviter first, then `E`'s other voters. Every record is checked against `E`'s certified root,
  whoever serves it, so a wrong peer costs a retry and is never trusted (4.4 rule 2).
- **Waiting:** a join waits `T_join_peer` for its source, not `T_peer`. Its replies are the whole state, the physics
  image among them (about 1.2 MB for M1's pit, about 1 s at 10 Mbit/s [I]), and a reply arrives only whole. When
  every source has failed, the join asks them again in turn, after `T_peer`, until `T_join` from its start, and only
  then reads `Failed`. At `n = 2` the inviter is its only source, so one slow reply ended a join that waited `T_peer`
  [V @engine-architect md2fa377743a4: the two-process demo's join failed after 250 ms, and passed with 5 s]. A
  donor serves one peer a root's physics image again only after `T_join_peer`.
- **No divergence:** nothing diverged, so a join captures no evidence, writes no bundle, opens no episode, emits no
  trace event and never counts toward the stop rule (4.3). It counts only `joins` and `join_failures` (7).
- **Its state:** `Diverge::join_state()` reads `Joining { cut }`, then `Joined { cut }` once the import matched
  `E`'s root and the replica's finalize agreed at `E`, so `Cv` is `E` (a finalize answered `NotReady` is asked again
  on a later tick; @engine-architect). It reads `Failed { cut, why }` when every source failed or the replica
  refused the import. A state, not an event: S3.8's `DivergeEvent` is a seam type, and a join needs no seam change
  [I].
- **Until it ends** the embedding runs no driver and calls only `tick` and `receive`, never `on_final`. The joiner
  votes from the first cut after `E` (`E` is already final for it). At `n = 2` nothing after `E` becomes final
  until it votes, so `E` stays servable. On `Failed`, pe-app gives up: the joiner says it couldn't join and quits
  [R @engine-architect mbccffa588899]. A join again at a newer cut matters only at `n >= 3` (M2).
- **Script modules** (@item:primordial-engine/ws-diverge-script-modules-in-rung-2-repair-fault): once that item
  lands, a join also waits until `Replica::vote_holds()` names nothing at or below `E`, with 4.2's `T_module`
  deadline, so a joiner never steps a world whose modules it lacks.

## 5. Who is right: root splits

- **With a quorum root** (at most `n - q` members differ): the quorum's root is right. The minority repairs
  toward it (section 4).
- **With no quorum root** (S3 `RootSplit`): typically two players, or three that all differ. Every member applies
  the same rule [I]:
  1. Every member runs rung 1 from its own `Cv` (`replay_to` at the split cut, which moves neither `Cv` nor the
     floor, since the cut is not final) and votes again with the root it gives. Transient faults end here.
  2. Every member **runs the physics self-test** for the split, once per episode, rather than reading its last
     result. Its verdict names the split cut (S3 v1.3 `SelfTest::{Passed, Failed} { split }`) and is final for the
     split: a second, different verdict from one member for one split is equivocation, which agreement shows as
     `Failed` and reports. The verdict reaches every member in the step-1 revote, or, if the member already voted
     in that view, in a signed self-test report that changes no vote; agreement then emits `RootSplit` again with
     every voter's newest verdict, and the newest supersedes the older. A member whose verdict is `Failed`, or
     whose connection guard reported a difference (WS-net, section 10), adopts another's root. At `n = 2` a failed
     self-test is the only real evidence of which machine is wrong.
     **M1's self-test** (pe-physics `self_test_digest`; @coordinator-opus md87066c865f0) steps a fixed scene a fixed
     number of frames and takes its whole-state check hash. pe-app takes the digest at start-up and again for each
     split, each on a thread in the simulation's floating-point mode, which the digest depends on (app.md 3, step 5):
     `Passed` if they are equal, `Failed`
     otherwise or if either run fails. A vote never claims a test that did not run. The scene is a fresh world, so it
     sees how the machine computes now (CPU, floating-point mode, code), never a fault in the session's state, which
     the pick decides. The start-up digest is the value R-G1's hello will carry: once it does, it is taken before the
     hello.
  3. A **reference member** is chosen (@coordinator-opus's L4' rulings, m9cef15c38b88 and m012a33b972f6, on
     @fable-reviewer's rule):
     - **The latch:** a member any of whose verdicts for this split shows `Failed` is out for the rest of the
       episode.
     - **The pick:** among the members not latched out, the one with the smallest `H(the newest final cut's line,
       height and frame ‖ member key)`. Nothing becomes final during an episode, so every member hashes the same
       cut, and it stays fixed for the episode.
     - **When:** only once step 1's replay is done, every voter's verdict names this split, and the member is not
       behind on finality: `EntrySource::final_cut` names the newest cut S3 reported final (the driver passes every
       `CutFinal` to `Diverge::on_cut_final` as it arrives). Without the wait, at `n = 2` the correct member could
       pick before the faulty member's failed verdict reached it, import the faulty state, and the cut would go final
       on it (@design-reviewer). A voter that never sends its verdict keeps the cut pending until the liveness rules
       remove it, and the voter set is then agreed.
     - Every member that does not hold the reference's root runs rung 2 toward it (an import that moves neither
       `Cv` nor the floor) and votes again.
     - **"Most voters hold" is not used in M1.** It matters from `n = 4` (a 2-1-1 split), and comes back with a
       model check. It is a known gap until then.
  4. **The choice is sticky.** It holds until a cut is final with matching roots, which ends the episode. Only a
     latch that grows (an equivocating member) moves it, and a repair toward the old reference is then dropped.
     From `n = 4` there is one more way, a known M1 gap beside "most voters hold": S3 can declare a split before
     every vote is in (1-1-1 with one voter missing), the wait covers only the voters in `RootSplit`, and when the
     late vote arrives every member recomputes and may move to the new voter. It converges, at the cost of one
     dropped repair (@design-reviewer, Low). The fix is to wait for the cut's whole voter set from governance, with a
     missing voter left to the liveness rules; it comes back with the model check. `n = 2` is not affected: both
     voters have voted before a split exists.
     Without this, a persistent computational fault changes the roots every cut, so a pick by root hash changes
     winner every cut, and both players see their machines patched toward each other in turn [design-reviewer].
  5. The split is reported, with a fault bundle, on every machine.
- **Why a hash of the newest final cut and the key** [I]: no player is favoured in general, every member computes
  the same answer from data every voter holds identically once caught up (not its own `Cv`, which differs while
  one member is still repairing, @design-reviewer and @shader-reviewer on ff37b4fd), and nobody can steer it by
  changing state. It is arbitrary by design: **at `n = 2` with both self-tests passing, the pick is arbitrary.**
  Between two machines with no third, nothing tells which computed correctly; the rule makes them agree, and the
  bundle reports it. A later episode hashes a later final cut, so with a fault neither self-test catches, the
  reference can differ from one episode to the next.

## 6. The fault bundle

Written on every repair past rung 1, and on every root split (file 14, 4.6).

**Contents:**
- the world id, the engine build id, the platform and CPU features, the self-test result;
- `Cv` and `C`: frames, roots and voters;
- the state at `Cv` for the cells (M4: contact groups) that hold the differing objects, with the hashes on their
  path to the root, so it imports into an empty replica. In M1 a world is one cell, so it is the whole state: `Cv`'s
  cut with its heads, `Cv`'s root, the domain, the world's rate, every record with its proof (the config, the
  tables, the objects), and the physics image (pe-diverge's `CvState`, its own format version);
- every entry from `Cv` to `C`, with signatures: those stamped from `Cv`'s frame up to `C`'s, as the entry source
  serves them, each `SignedEntry::encode`d, and `C`'s heads. The source can hold entries stamped below `C` that only
  a later cut includes; a replay leaves them out of `C` and of every frame's cut. The state, the entries and the heads
  go together, and only to a bundle whose `C` is the cut the evidence was last extended to; otherwise the bundle
  carries none of them, and `evidence_skipped` counts it [shader-reviewer on 18198b28 and 076bb902: a bundle's `C`
  is always that cut, now checked, not argued];
- **script modules** [C meaa5b7bf57e9]: the runner's identity (`Replica::runner_identity`, canonical bytes, 11.1),
  and the metered bytes of every module that the state at `Cv` or an entry up to `C` names (`Replica::module`),
  each under its hash. They are taken in order of (`activate_at`, hash) until the evidence reaches `B_evidence`,
  and the rest are kept as hashes only, so two machines' bundles for one fault carry the same modules
  [design-reviewer, Low 2]. A replay then needs a store that holds the rest. A world without modules carries none.
  The bundle is a local file, so this sends no module anywhere (logic.md Q3c). This is a later version of the
  bundle format; the code waits for the seams (11.1);
- the ids of the differing objects; their records under my root and under the quorum's;
- the rung reached, the sources used, and the times taken;
- for a ladder that stopped (4.3), each repeated failure named: the rung, the reason and the differing leaves
  (`stuck`; the bundle format's version 2).

**Capture** [I]: pe-diverge takes the state and the entries itself, from the replica's tree at `Cv` (`Tree::export`
over every leaf, `Replica::physics_image`) and from `EntrySource::entries_at`, when rung 2 first runs after `Cv`
moved, and extends the entries when a newer cut is repaired toward. `Cv`'s state and the entries from it are kept
then, and the import that cures moves `Cv` past them, so the embedding could not fetch them afterwards. They go only
with the repair they were captured for: its cure's bundle takes them, and a stopped ladder's at the same `Cv`
carries them; a root split's never does, and the bundle's `cv` is the `Cv` the repair started from, the one the
evidence names. They are left out, and counted once per `Cv` (`evidence_skipped`), when `Cv`'s state or an entry
frame is no longer kept (a `Damaged` cut may be one), or when the state is over `B_evidence`. Once the cure's bundle
is written, or `Cv` moves otherwise, they are dropped. A replay starts from `CvState`'s cut, which a repair's bundle
also names as its `cv`. [I: the capture's cost, a whole-state walk, export and encode on the simulation thread once
per divergence, is not measured yet; the M1 run measures it.] The tables are numbered from 0 by their place in
`Node::Tables`, so the walk names each. An entry stamped below `Cv`'s frame that only `C` includes is late and void
(S3 v1.1), so it changes no state and is not carried.

**Rules:**
- It is self-contained: `replay-bundle <file> [--max-frames N]`, an example of the crate (`cargo run -p pe-diverge
  --example replay-bundle`), replays it on any machine. It imports the state at `Cv` into an empty replica over the
  real world (pe-replica's `PeWorld`, at the bundle's rate), steps the entries, and prints this build's root at
  every frame from `Cv` to `C`: S2's `root_at_cut` on a cut made for each frame, `Cv`'s heads advanced by `C`'s
  entries stamped below it, after stepping to just past that frame, so every root is taken near the present. Then it says whether the root at `C` is the agreed one, the diverged machine's, or
  neither, and prints the differing objects' leaf hashes there. Two machines' printouts, or two builds', differ
  first at the first frame that diverged. [I: this finds the first differing frame offline, so the live repair never
  waits for it.] Each frame's root re-steps from the nearest keyframe and holds a snapshot, so the frames are capped
  (600 by default). A state at `Cv` that does not check against its root is refused, not replayed.
- **Modules on replay** [C meaa5b7bf57e9]: the replay hands the bundle's module bytes to this build's runner and
  activates each at its row's frame, as the live world does.
  - It checks each module's bytes against the hash they are filed under (`Hash::of("pe.logic.module", bytes)`,
    logic.md 6.2) before the runner sees them, and refuses a mismatch by name: a bundle is evidence, not trusted
    [design-reviewer, Low 3].
  - Before stepping, it refuses a runner whose identity differs from the bundle's, and prints both identities, so
    the printout says what differed. A bundle with no identity recorded, over a state with module rows, is refused
    the same way, as is a build with no runner.
  - The identity is the runner's own, not the app's build: a replay on another build of the app with the same
    runner still runs, which is the cross-build comparison the tool exists for [design-reviewer, Medium].
    `--allow-runner-mismatch` runs anyway, and marks every line it prints as from a different runner.
  - A module that neither the bundle nor the local store holds is named, and the replay stops, as for a blob.
- Blobs are content-addressed and outside the hash tree, so a bundle does not carry them: a world whose objects name
  blobs replays only where the local store holds them [I]. A mesh the store lacks is named, and the replay stops,
  rather than showing as a false divergence.
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
| `source_faults` | Sources left for a fault: not holding the root, silent, or sending what fails its hash or proof | pe-diverge |
| `busy_rotations` | Sources left after `N_busy` `Busy` answers in a row: over budget, not faulty | pe-diverge |
| `repeated_divergence` | Objects marked repeatedly diverging | pe-diverge |
| `repairs_stuck`, `stuck_skipped` | Times the ladder stopped (4.3); diverged final cuts compared while stopped, with no rung run | pe-diverge |
| `evidence_skipped` | Divergences whose bundles go without replay evidence (6), once per `Cv`, and each bundle whose `C` is not the cut its evidence was extended to | pe-diverge |
| `overtaken_in_a_row` | A gauge: repairs toward a final cut overtaken by a newer one, in a row since the last cure or agreeing cut. It tells a world too big to fetch within `W_fg` (4.2) from one unlucky repair | pe-diverge |
| `root_splits` | Root-split episodes (distinct split cuts, not `RootSplit` events), and how each ended | pe-diverge |
| `bundles_written` | Bundles written | pe-diverge |
| `joins`, `join_failures` | Joins from a final cut (4.5) whose import matched its root, and joins that failed. Neither is a divergence, a cure or a rung failure | pe-diverge |
| `corrections_per_min` | Drawn objects whose drawn pose had to blend or jump after a correction, per minute | pe-render (section 8) |
| `visible_error` | Mean and max distance between drawn and simulated pose, over objects in view, per second | pe-render |
| `snaps_in_view` | Corrections that jumped rather than blended while in view | pe-render |
| `cueless_large_corrections` | Corrections in view larger than `E_cue` shown without a cue. Must be zero | pe-render |
| `own_error` | Error of the local player's own hands and held objects. Must be exactly zero | pe-render |
| `rollback_depth` | Deepest rollback, and the distribution | pe-replica (WS-state) |
| `voided_own` | The local player's entries voided as late (P5) | pe-agree, through S3 |
| `merges`, `merge_conflicts`, `merge_replay_frames` | From M2 | pe-diverge |
| `unconfirmed_time` | Time played since the last final cut, as a distribution | pe-agree (M3) |

**Health.** The health a machine reports, in its published frame view (so pe-render's "resyncing" line, render.md)
and in WorldApi's `session` (api.md 5.1), is one value: `Repairing { since }` while either:
- the replica reports it: a final cut I finalized diverged or was damaged (S5 `Health`); or
- a root split is open: S3 reported `RootSplit` for a cut above the final cut's frame, and no cut at or past that
  split's frame is final yet. Every reported split stays open until then, so a revote that settles an early split
  leaves a later one open (@engine-architect); or
- pe-diverge has an episode open: a root split not yet settled by an agreed cut, or a replay or repair under way
  (`Diverge::open_since`, the earlier of the split's cut and the cut it repairs toward).

`since` is the earliest of their frames.

A joiner (4.5) has no health before its import: its replica publishes no view. pe-app shows "joining" from
`Diverge::join_state`, and pe-api answers not-ready until the replica reaches the present; S5's health is unchanged
[R @engine-architect mbccffa588899].

The replica alone can't show a split: it learns of a divergence only at a final cut, and in a split none is final. At
`n = 2` every divergence is a split (3.2), so without the second clause a stalled world read "agreed" (found by
@engine-architect's two-process demo, 2026-11-17). pe-app composes the two and publishes a view carrying the result (`Sim::health_of`), with no seam change
[R @coordinator-opus, @item:primordial-engine/pe-app-runs-pe-diverge-app-md-step-5-and-health-r]. Until pe-app runs
pe-diverge, the split clause read S3's events directly. Now pe-app runs it (app.md 3, step 5) and keeps both: S3's
open splits, and pe-diverge's open episode.

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
- **Old effects are judged by when their cause arrived, not by their frame.** A replay can reach back a long way:
  up to `W_fg` in the foreground, and up to `T_diverge` for a merge applied at its application frame. So [I;
  design-reviewer, review of cd2e9fc9]:
  - a one-shot effect with a new key plays once if its causing entry **arrived at this machine within
    `T_effect_lag`** of the effect's frame (the poor-link target, 1 s [R meef4f248923b]). A remote push that
    arrives 0.3-1 s late still sounds its collision;
  - an effect that arises only from a merge applied at `A`, or from a replay with no newly arrived cause (a repair,
    a re-simulation), is never played. Sustained state (a lamp that is now on) shows as state, not as an effect.
    That prevents a burst of old sounds;
  - the set of played keys then covers `W_fg` (2 s), not a minute.
- A sustained effect whose event disappears after a replay is stopped, faded over `T_blend`.
- Effects of the local player's own actions play at once.
- Effects of others' actions not yet final may wait `D_effect`. [I] Default 0 s, for immediacy;
  file 14 section 6 doubts the delay is wanted.

**D7. Lost races and refusals are told,** not hidden: a lost grab has its cue (D4); a refused edit shows who won
(WS-api and pe-render, P4); an entry of mine voided as late is shown plainly (P5).

**D8. Display rate is not tick rate.** The same layer interpolates between simulated frames for any display rate
(R2). Interpolation is not a correction and does not count as one.

**Tests (WS-render's, in its crate, against pe-harness scenarios):**
- a remote push at 0.6 s one-way delay plays its collision sound exactly once; a 60 s merge plays none;
- the drawn pose reaches the simulated pose within `T_blend` of the last correction, to within float tolerance;
- `own_error == 0` in every scenario, including lost grabs and repairs;
- a replayed event's effect fires once; an undone event's sustained effect stops;
- the counters of section 7 are reported for every network scenario;
- **a control that bites:** turning blending off must raise `snaps_in_view`; blending the local hand must make
  `own_error` non-zero, and the test must catch both.

## 9. The merge engine (M2 v1, M3 v2): draft for WS-model

Not built until WS-model's model v2 checks it [C 20:05Z, C8]. Sections 9.2-9.5 are the shape to model, made precise
for it in r2 and r3 (coordinator m3644e8ee0d7c).

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

### 9.2 The inputs, at main's final frontier

Sections 9.2-9.5 are written to be modelled. Every term is defined from agreed data, and 9.5 lists the properties a
model must check, each with a control. Unmarked rules in 9.2-9.3b are [I] (diverge-eng, r2 and r3 of this section),
resting on the rulings they cite.

**A line is its entries, not its states.** A line's state is always derived by replaying its entries under the
engine's rules. A machine's own simulation, under whatever code it ran, is never an input. Code activations are
entries, so a replay takes up new code at its activation's stamp. That is how the returning player's actions are
"resimulated using the new rules" (Q3b).

**`replay(S, E, f)`:** step state `S` from its frame up to frame `f`, applying the entries of `E` in the agreed
order (stamp, then the tie rank, author, chain, seq; D37, agreement.md 3). Each entry comes with a **disposition**,
which says whether it is applied [design-reviewer, r2 Medium 2]:
- **`Final(v)`:** an entry of main's final log with its verdict `v`. A void entry (`Late`, `BackStamped`, a fork) is
  skipped. Any other is applied at its stamp. Applied this way, `S_o` reproduces main's state.
- **`AtStamp`:** applied at its stamp whatever `Late` verdict main gave it. Used for the returning entries the merge
  admits, and for every entry in `theirs`' own replay.
- **Never applied, in any replay:** an entry with a chain-validity void (`BackStamped` within its own chain, or a
  fork). Those are faults, not lateness.

The ordinary rules decide only an applied entry's state-dependent outcome: `Applied`, with any note (`Orphaned`,
`AlreadyDeleted`), or `Refused { reason }`, and for a grab whether it was granted. `replay` records each outcome.

**Footprints** (S2, 11.1) are kept per entry and per step, with **reads and writes separate**:
- an entry's footprint holds what it read and wrote. A refused entry holds the reads that refused it, its
  precondition or hold check, and writes nothing [design-reviewer, r2 Low 2];
- a step's footprint holds the objects its scripts and physics read and wrote, contacts and joints included, and
  **the code version and config rows it ran under** (rate, gravity, settings, module rows) [shader-reviewer, r2 Low
  1].

**Changed by a line** means written, not merely different at the end [shader-reviewer, r2 Medium 2]. An object or
row is changed by a line when an entry or step of that line wrote it in `[fb, F_M)`, even if it was written back
later. A **body** is changed by a line when an entry or script of that line wrote it, or when, at a later frame of
that line, it touched (by contact or joint) a body that line had already changed. Plain stepping changes no body. A
**domain** is changed by a line when one of its bodies is.

**The merge entry `M`.** The returning machine (v1), or the first proposer after reconvening (v2), submits an
ordinary entry `M`, stamped `s_M`, naming:
- **`base`:** a final cut `Cb` both lines hold, with its root `Rb`, at frame `fb`. In v1 it is the last final cut
  the returning machine verified before it diverged (its `Cv` then). `S_b` is the state at `Cb`.
- **`theirs`:** the ids of the returning line's entries stamped in `[fb, s_M)`, as a set `E_t`. These are its own
  entries and every entry it received and applied, main members' entries that main voided as late included. Each id
  comes with the witness statements the submitter holds for it (9.3).
- **`options`:** the defaults for an automatic merge (9.6).

**From submitting `M`, the returner plays on main's line** [C mf06158845447, design-reviewer]. It makes its new
entries as ordinary main-line entries, predicting the merged state, so they reach main's log and are never returning
entries. The returning player's actions during the merge are kept: no player waits (Q3b).

`M` is stamped, signed, included and given a verdict like any other entry. Members fetch the entries it names by
name, as for a cut. Let `F_M` be the frame of the final cut that first includes `M`. **The merge runs in two
stages** [C md9479c65a229, @fable-reviewer m90478b14513e]: stage 1 merges at `F_M`, where every input is final once
`M` is; stage 2 carries the result forward to the application frame `A` (9.3a).
- **`ours`:** `E_o`, main's final log stamped in `[fb, F_M)`, in the agreed order, fixed when `M` becomes final.
  `E_o2` is main's entries stamped in `[F_M, A)`, which stage 2 re-applies. Entries an earlier final merge entry
  merged in count as members of the final log at their stamps, so that no entry takes effect twice (P-M7).
- **The returning entries:** `E_r = E_t \ E_o` by entry id. An entry in both lines counts once, as main's. A
  returning entry stamped at or above `F_M` (in v2, a line's entries made after `M` was submitted) is refused with
  `AfterMergePoint`.
- **A returning entry keeps the verdict main gave it** (`Late`, or none if never included) for ever: final verdicts
  never change [C mef721d3fbd4f]. What the merge does with it is reported in `M`'s outcome (9.3, step 6). A cut that
  includes it after `M` voids it as `Late`, as usual.

**The stage 1 states, all at `F_M`:**
- `S_o = replay(S_b, E_o as Final, F_M)`: main's own state at `F_M`;
- `S_t = replay(S_b, E_t as AtStamp, F_M)`: theirs, under the rules its entries activate, stepped to `F_M`. A domain
  only `theirs` changed takes this state, not the returning line's last state [C md9479c65a229];
- `S_1 = replay(S_b, E_o as Final ∪ Adm as AtStamp, F_M)`: **the stage 1 reference**. The ordinary rules are applied
  once to both lines' actions, in one order [file 14, 4.4; R 18:58Z "every action is a commit to the main tree"].

**`Adm`, the admitted returning entries, is defined on whole-world replays,** so that the reference does not depend
on 9.3's grouping [design-reviewer and shader-reviewer, r2 Medium 1]. Unwitnessed entries are decided by runs, in
two steps [C mb6cef0210a70, m1bf05e0a31df]:
- **Every witnessed returning entry** (9.3) is in `Adm`. Let `W_t` be the witnessed returning entries.
- **Runs.** A run is a maximal sequence of one chain's unwitnessed returning entries, in chain order. A witnessed
  entry of the chain does not break it. Runs are taken in the agreed order of their first entries.
- **Passing.** A set `X` of unwitnessed entries *passes* against `Adm_so_far` when no entry of `E_o` has a different
  outcome in `replay(S_b, E_o ∪ W_t ∪ Adm_so_far, F_M)` and in the same replay with `X` added.
- **Step 1, one combined test.** If every unwitnessed run together passes against the empty set, all of them are in
  `Adm`, and the merge applies at `A1` (9.3a). Together they change no final outcome, which is all the rule asks.
- **Step 2, only if step 1 fails:** each run is decided in turn by the procedure below, whose result is the
  definition, and the merge applies at `A2` (9.3a).
  - **A budget per chain:** each chain has `N_unw` tests (section 14), with no global cap. A chain is one member's,
    and a fork is void, so the total is bounded by `N_unw` times the session's members. A silent member can exhaust
    only its own chain [C m1bf05e0a31df; shader-reviewer, r4 Medium 2].
  - **Passing is not monotone** in the length of a prefix: a grab can block a final grab, and a later release in the
    run can free it again. So the procedure finds *a* passing prefix, not necessarily the longest [design-reviewer,
    r4].
  - **Later work is kept** [C m1bf05e0a31df]: the first failing entry is refused, and the rest of the run is tested
    again as a new run, next in order, within the same budget. Entries that depended on the refused one meet the
    ordinary rules in the merged order. At `N_unw` = 16 this keeps later work past **one conflict per chain**: a
    thin player's first conflict takes about 15 tests (one full bisection of 14,400 entries), and if the remainder
    holds a second conflict, its one remaining test fails and the rest is refused with `MergeBudget`. Keeping `k`
    conflicts needs about `1 + k (1 + ceil(log2 n))` tests, and `D2` grows with the budget (both reviewers, r5). Kept
    at 16, with one conflict per chain the stated limit [C m5473c7a2aa26].

  ```text
  queue = the runs, in agreed order of first entry; budget[c] = N_unw for every chain c
  while queue is not empty:
      r = pop the first run of queue; c = its chain; n = len(r)
      if budget[c] == 0: refuse r with MergeBudget; continue
      budget[c] -= 1
      if prefix(r, n) passes: admit r; continue
      lo = 0, hi = n - 1                  // invariant: prefix(lo) passes, prefix(hi + 1) fails
      while lo < hi and budget[c] > 0:
          budget[c] -= 1
          mid = ceil((lo + hi) / 2)
          if prefix(r, mid) passes: lo = mid
          else: hi = mid - 1
      admit prefix(r, lo)
      if lo == hi:                        // the procedure ended: prefix(lo + 1) was tested and failed
          refuse entry lo + 1 with LostToFinal { entry }
          if lo + 2 <= n: push entries lo + 2 .. n of r to the front of queue, as a new run
      else:                               // the chain's budget ran out mid-procedure
          refuse entries lo + 1 .. n with MergeBudget
  ```

  "Passes" is always against `Adm_so_far`, everything admitted before it. A refused entry names the earliest final
  entry whose outcome differs in the failed test of `prefix(lo + 1)`. Authors of refused entries are told (R-M2).

**Stage 2 and the reference at `A`:**
- `S_A = replay(S_1, E_o2, A)`: stage 1's state, with main's entries in `[F_M, A)` re-applied forward by the
  ordinary rules (as `Final` once final; a member predicts until then). Only what touches stage 1's changes needs
  re-stepping (9.3a).
- **The whole reference** is `S_ref = replay(S_b, (E_o ∪ E_o2) as Final ∪ Adm as AtStamp, A)`. Every admitted entry
  is stamped below `F_M`, so `S_A = S_ref` by determinism. The model checks that 9.3's grouping and stage 2's
  partial re-stepping keep it so (P-M1).

**The function** is `merge(S_b, E_o, E_t, W, rules, options) -> Merged`, with `W` the witness statements `M` carries
and `Merged = { state at F_M, per-entry outcomes, groups, report }`. It is pure: no clock, randomness, I/O or
arrival order. Its state must equal `S_1` exactly, and its `Adm` the definition's (P-M1). Section 9.3 computes both
without replaying what only one side touched; 9.3a carries the result to `A`.

### 9.3 The computation, and the order rules

Computing stage 1 by replaying everything from `base` would cost a minute of the whole world for every merge. The
three-way comparison limits the replay to what both lines touched.

1. **Classify** every object without a body, and every world-table row, which is treated as an object (the admin
   list: 9.6), by which lines changed it (9.2); and every physics domain, as one unit with its bodies and its image,
   the same way:

| Since `base` | git's name | Result at `F_M` |
|---|---|---|
| Neither line changed it | unchanged | `S_o` (for an object or row, equal to `S_b`) |
| Only `ours` changed it | fast-forward | `S_o` |
| Only `theirs` changed it | fast-forward | `S_t` |
| Both changed it, to the same record at `F_M` (objects and rows only) | clean | that record |
| Both changed it otherwise; or a domain both changed | conflict | its group's replay (step 5) |

2. **Seed the conflict set `K`** with:
   - every object or row both lines changed, except a clean one;
   - **every object or row one line changed that the other line read** in `[fb, F_M)`: a precondition, a hold check,
     a script read, a contact, a config row or code version a step ran under. A physics step's reads of its own
     domain's bodies do not seed: plain stepping is a function of the domain's state, which step 1 classifies as a
     unit, so otherwise every domain either side changed would be seeded and "only `theirs` changed it" could never
     happen. Its reads of config and code rows still seed [design-reviewer, r3 Low]. This also seeds a clean row
     whose
     intermediate value a reader on the other side saw. A write-write test alone misses the case where a
     back-stamped write in `theirs` would refuse a final entry that only read the object [I, new in r2; accepted, C
     m00d0a845a25c]. It also catches an entry in both lines that main refused but `theirs` applied
     [shader-reviewer];
   - **every domain both lines changed:** all its bodies, and its physics image. Physics restores and steps whole
     (4.2a) [@vdw-physics, replica.md 6.3], and two bodies that met in neither line can meet in the merged one. This
     replaces the earlier overlap rule [design-reviewer], which it covers [kept, C m00d0a845a25c]. In a ball pit,
     both sides nearly always touch some ball, so a whole-domain replay is the usual case. That makes the cost
     experiment (9.5) a gate on any promise about merge speed [design-reviewer; C m00d0a845a25c].
3. **Close `K`** under *linked*. Two objects are linked when one footprint, of an entry or a step, in either line,
   at any frame in `[fb, F_M)`, includes both: the entry or step read one and wrote the other, or wrote both.
   Contacts, joints and parent links appear in the footprints. A group with a body grows to its whole domain, as in
   step 2. **The groups** are the connected parts of `K`. Afterwards every entry's footprint, in either line, lies
   inside one group or outside `K`.
4. **The entries of a group `G`:** every entry of `E_o ∪ E_r` whose footprint, in either line, meets `G`,
   deduplicated by id, in the agreed order.
5. **Replay each group** from `S_b` restricted to `G` up to `F_M` (S2's per-group restore and step, `GroupSel`,
   11.1), with the dispositions of 9.2. Each unwitnessed returning entry is decided by the group form of the
   back-dating rule below. Every returning entry outside `K` is admitted. Both must give 9.2's `Adm` (P-M1).
   - **The fixpoint:** if a group's replay has a footprint outside `K` (an entry or script that, in the merged
     order, reaches an object neither line reached), add those objects to `K`, regroup, and repeat steps 3-5. `K`
     only grows and objects are finite, so it ends. Whether a model finds a case that needs this is open (9.5).
6. **`Merged`:** the state at `F_M` is step 1's table outside `K`, and each group's replay for every object in `K`.
   `M`'s outcome, the merge report, lists:
   - for each returning entry, `MergedIn` or `Refused { reason }`, where the reason is the ordinary one, or
     `LostToFinal { entry }` from the back-dating rule;
   - for each entry of `E_o` whose outcome in its group's replay differs from main's, `Overridden { hint }`. That
     includes final entries that depended on an overridden one [shader-reviewer]. The hint names the earliest
     witnessed returning entry in its group stamped before it: a likely cause, not a proven one [design-reviewer].
     Below `F_M`, only a witnessed entry can cause an override (P-M4). The final entry's own verdict is unchanged:
     what changes is the state from `A` on [accepted, C m00d0a845a25c]. Stage 2 adds the overrides in `[F_M, A)`
     (9.3a);
   - the groups, their sizes and the frames replayed (the counters `merge_conflicts` and `merge_replay_frames`).

   Every author of a refused or overridden entry gets a notice through the refusal's reason, as for every refusal
   (R-O1, R-M2).

**The ordinary rules decide every conflict**, since the merge has none of its own:
- **an exclusive hold:** the soonest grab wins;
- **a precondition** that is stale in the merged order refuses its entry with its reason;
- **plain writes:** the later write wins;
- **a delete against an add-on** follows world-model.md 7.5 and the user's order rule [R m35138bafc46a,
  mfc2bbd9d28ad]:
  - delete first: an object created under, or reparented to, the deleted object is orphaned (`Applied`, note
    `Orphaned { deleted_by }`), and a component attached to it is refused (`Deleted { by }`), with a notice to its
    author;
  - delete second: the delete destroys the subtree, add-ons included.

**Back-dated entries: witnessed and unwitnessed.** [R mfc2bbd9d28ad: confirmed by the user, as ruled by the
coordinator in m61125c4194d4 and maf0da5275cdd, 1/1 residual included.] Returning entries that main voided as late
are merged in, since that is what "their changes are merged in" means (Q3b). How each is ordered depends on whether
its stamp was witnessed.

- **Witnessed.** A returning entry `e` (author `a`, stamp `s`) is witnessed when `M` carries a statement `(m, heads,
  f)` that is:
  - signed by a member `m` other than `a`, for `e`'s line;
  - with heads that cover `e`: `m`'s head for `e`'s chain is `e`, or a later entry that links back to it;
  - with `f - s <= B`, where `f` is the frame at which `m` first held `e`, recorded on receipt and signed later in
    any vote or acknowledgement [shader-reviewer, r2 Low 3; design-reviewer], and `B` is converted at the rate in
    effect at `s`.

  `B` is the ordinary back-dating bound, a duration set by WS-agree, about 4-10 frames or 70-170 ms at 60 Hz [V
  design 8.2, cited by @design-reviewer]. Because `f` is the receipt frame, not the frame of a later vote, `B` need
  not cover the vote interval: `f - s` is the one-way delay of an on-time entry. The carrier is WS-agree's (R-M1).
  An honest member records its true receipt frame. A faulty one can lie, which is the accepted residual that
  colluders beyond `t` can witness each other.
  - A witnessed entry is ordered by its stamp like any other, so the order rules above apply, the user's delete rule
    included. This covers an honest split group, whose entries the others on its side acknowledged in time.
- **Unwitnessed.** No such statement is carried. The usual cases are a thin player whose entries arrived late, and
  an entry back-stamped by a silent member. The rule is 9.2's definition of `Adm`. Step 1 admits every unwitnessed
  run together if together they change no final outcome. Otherwise, in step 2, each run keeps the prefix the
  procedure finds; its first failing entry is refused with `LostToFinal { entry }`, and the rest is tested again. A
  chain past its budget `N_unw` has the rest of its entries refused with `MergeBudget`. "Outcome" is the verdict
  with its reason and notes, and for a grab whether it was granted. So an unwitnessed entry loses to any final entry
  whose outcome it would change, whatever the operation: an exclusive hold, a precondition, or structure.
  - **The group form,** which 9.3 uses: the same tests, with the replays restricted to the union of the groups the
    tested entries meet [shader-reviewer, r4 nit]. That it gives the definition's answer is part of P-M1, not an
    assumption.
  - **No locality skip** [C m1bf05e0a31df]. A skip on what final outcomes read missed effects carried through
    contact and scripts, such as a returning push that hits a resting ball a final grab holds (both reviewers, r4).
    In a ball pit a sound skip would admit almost nothing untested. One may come back if a measurement shows it
    pays.
  - **Cost:** each test replays from the first tested stamp to `F_M`. The per-chain budget bounds it, and step 1
    makes the common case one test (9.3a).
- **Plain writes** come out safely in both cases. A back-stamped write loses to main's later one by the ordinary
  order, and changes no final outcome, so it is admitted and then overwritten.
- **A submitter that leaves out a witness** only makes that entry unwitnessed, which can only refuse it. In v1 the
  entries that can suffer this are the submitter's own, and main members' entries it carries that main voided anyway
  [I; shader-reviewer's nit].
- **Residuals, accepted by the user:**
  - colluders beyond `t` can witness each other's back-stamps;
  - in a 1/1 split, a lone cheater can back-date up to a minute against a lone honest player (v2, 9.4).
- **Why** [design-reviewer, High and re-review]: ordered by stamp alone, a member could stamp actions in the past,
  stay silent past `T_merge`, then return and win at the merge against honest players' final actions. A silent
  member publishes no horizon, so the 4-10 frame bound would widen to a minute. The window needs a silent member
  with no witness, which is exactly what the unwitnessed rule covers. Refusing every earlier entry instead, the
  general form, would contradict the user's delete-order ruling for honest split groups [C m61125c4194d4]. [I:
  "loses" is read as refused. Rebasing it to the application frame `A` instead would let a back-stamped delete still
  destroy main's work at `A`, which is winning by another route.]
- **History:** the coordinator first ruled option (a), protecting holds and preconditions (20:40Z, m68977e8f9830).
  @design-reviewer showed structure must be protected too; the witnessed/unwitnessed split reconciles that with the
  user's ruling.

**Things only one side touched do not change** (file 14, 4.4), except where step 2 or 3 pulls them in. That is the
"merge keeps work" property: an entry is dropped only if the ordinary rules refuse it in the merged order, or the
unwitnessed rule refuses it.

### 9.3a Stage 2, applying at `A`: no final frame is undone

- **`M` takes effect at the start of frame `A`,** before the entries stamped `A`: the state at `A` is `S_A`, and
  main's line continues from it. Every frame below `A` keeps its state and root. Every verdict at or below
  `final_frame` stands [C mef721d3fbd4f]. Since `A > F_M`, no frame that was final when `M` became final is touched.
- **Stage 2 re-applies `E_o2`** on stage 1's state from `F_M` to `A`. Only objects linked, through the footprints of
  `E_o2` or of the steps, to an object stage 1 changed relative to `S_o` need re-stepping; the rest keep main's
  state at `A`. A domain with such a body re-steps whole. A late entry stamped below `A` rolls stage 2 back as for
  any entry. Stage 1 cannot change after `M` is final, so `K` never grows late [design-reviewer, r2 Medium 3].
- **Stage 2 can override main's entries in `[F_M, A)`, through any merged change** [C mf06158845447,
  design-reviewer]. Those entries got their final verdicts on main's line without the merge. Re-applied over stage
  1's state, their outcomes can differ, even through an unwitnessed admitted entry, because the unwitnessed test
  protects only entries below `F_M`. Each is listed `Overridden { hint }`, with the merged change it met as the
  hint, and its author is told (R-M2). This is the inherent cost of applying the merge at `A > F_M`, and the window
  is bounded by `D`. The unwitnessed test is not extended to `[F_M, A)`, since that would bring back late growth.
- **`D`, the delay to `A`, is a function of the final log at `F_M`, not the submitter's estimate** [C md9479c65a229,
  mf06158845447, m1bf05e0a31df]. With step 1 and step 2 of 9.2 there are two candidate frames, `A1 = F_M + D1` and
  `A2 = F_M + D2`:
  - `N_1` is stage 1's worst-case replay in whole-world-equivalent frames, the unit `ρ` is measured in
    [design-reviewer, r3 nit], read from the log at `F_M`:
    - the replay of `theirs`: `F_M - fb`;
    - the group replays with their fixpoint rounds: capped at `F_M - fb` in total. A round that would pass the cap
      is replaced by the whole-world replay, which is `S_1` itself, so this term counts `2 (F_M - fb)`
      [shader-reviewer, r3 Low 2];
    - for `D1`, step 1's combined test, if there is any unwitnessed run: `F_M` minus the first unwitnessed stamp;
    - for `D2`, step 1's test, plus, for each chain with `m` unwitnessed entries, `min(N_unw, m (1 + ceil(log2 m)))`
      tests (at most `m` failures, each at most one bisection; one test for a single entry), each `F_M` minus that
      chain's first unwitnessed stamp. Tests run in sequence, so the chains' terms add [both reviewers, r5 Low];
  - `L` is the finality lag: from frame `F_M` until every honest member knows the cut at `F_M` is final, a bound
    stated by WS-agree (the agreement lag plus two cut intervals on good links) [shader-reviewer, r3 Low 1];
  - `ρ` is the world's `R_merge` (section 14), the replay speed over real time every member must reach. **`ρ > 1` is
    a precondition**;
  - stage 1 starts `L` after `F_M` and takes `N_1 / ρ`, and stage 2's worst case, the whole world over `D` frames,
    takes `D / ρ`. Finishing by `A` needs `L + N_1 / ρ + D / ρ <= D`, so `D = (N_1 + ρ L) / (ρ - 1)` frames, rounded
    up, converted at the rate in effect at `F_M`. That gives `D1` and `D2` from their `N_1`;
  - **which frame applies:** if step 1 passes, or there is no unwitnessed run, the merge applies at `A1`. Otherwise
    it applies at `A2`, and nothing happens at `A1`. Every member knows which by `A1`, since `D1` covers step 1. So
    the frame is a pure function of the log at `F_M` and step 1's deterministic result [C m1bf05e0a31df]. `A` below
    means whichever applies;
  - `D` is not a field of `M`;
  - members may start the replay of `theirs` on receiving `M`, before `F_M`. Counting that would save only about `ρ
    (F_M - s_M + L)` frames, under a second of `D`, so `D` does not count on it [C m410e256e7a01].

**The measured replay speed** [V, diverge-eng, mfd80586c84f8]. Conditions: main 746dbcc3; pe-harness's ball pit
(`ReplicaImpl<PeWorld>`, 60 Hz) with 4 members' seeded hand pushes every 100 ms (hand move and shove, 1,190 signed
entries a minute); a fresh replica given the whole log and stepped straight through with `advance`, whole world,
headless, one thread, a saved state per frame; release build, run under heavy.sh at idle priority, with build slots
shared with other jobs; the main PC only. The second PC is not measured.

| Run, 3,600 frames | Time | Frames/s | `ρ` |
|---|---|---|---|
| 200 balls | 2.53 s | 1,422 | 23.7 |
| 200 balls, repeat | 2.95 s | 1,219 | 20.3 |
| 200 balls, no pushes (control: the root differs, so the pushes take effect) | 2.08 s | 1,735 | 28.9 |
| 500 balls, the preset's maximum | 10.23 s | 352 | 5.9 |

**`D` at the per-world `R_merge`** (section 14) [V arithmetic; `L` = 0.5 s is [I], the agreement lag plus two cut
intervals]. `T = F_M - fb`. With no unwitnessed run, `N_1 = 3T`. `D1` takes `N_1 = 4T`. `D2` with one chain's
unwitnessed run takes `N_1 = 4T + 16T = 20T`, the whole budget at the worst case. An honest thin player a minute
behind is one run: it pays `D1` when its run changes no final outcome, and `D2` only when it conflicts. With two
other chains of one late entry each beside it, `D2` takes `N_1 = 4T + 16T + 1T + 1T = 22T` (the last row).

| Apart (`T`) | `R_merge` 15 (200 balls): none | `D1` | `D2` | `R_merge` 5 (up to 500 balls): none | `D1` | `D2` |
|---|---|---|---|---|---|---|
| 10 s | 2.7 s | 3.4 s | 15 s | 8.1 s | 11 s | 51 s |
| 1 min | 13 s | 18 s | 86 s | 46 s | 61 s | 5.0 min |
| 5 min | 65 s | 86 s | 7.2 min | 3.8 min | 5.0 min | 25 min |
| 1 min, with two more chains of one late entry each | 13 s | 18 s | 95 s | 46 s | 61 s | 5.5 min |

- **The vote waits for the merge.** A member votes on a cut at or past `A` only once it has computed `S_A`. Its root
  must include `M`'s effect, and a vote without it would be a false root split. Cuts below `A` are voted on main's
  line as before.
  - **The one remaining stall** [C md9479c65a229]: a member slower than `ρ` holds its vote at `A` until it is done.
    Play never pauses. Finality past `A` waits only if fewer than `q` members are done.
  - The stall is bounded by `T_merge`: past it, the slow member is itself a returning branch.
  - A member that has not yet built the code `theirs` needs treats the merge as a pending code activation (Q3),
    under the same bound.
- **What players see.** The change at `A` is a correction like any other. The display layer blends it (section 8),
  and a player who held something the merge took from them sees the notice of R-M2.

### 9.3b "Visibly unchanged": the pose tolerance

The coordinator ruled that untouched groups must be deterministic, not bit-exact [C 20:40Z, m68977e8f9830]. In a
domain that step 2 replays whole, a body only one side touched is replayed with the other side's activity nearby.
Its contacts can take other ids and colours, and its pose can drift.

- **The objects it applies to:** `U`, the bodies of a domain in `K` that at most one line changed (9.2). A body both
  lines changed is a conflict, not in `U`. For each, let *its side* be `ours` if only `ours` changed it or neither
  did, and `theirs` if only `theirs` did [shader-reviewer, r2 Medium 2].
- **The property,** for every object of `U` at `F_M`, in stage 1's state:
  - every record except the body's pose and velocities is equal to its side's exactly: rows, fields, structure,
    holds;
  - the distance between its position in stage 1's state and in its side's state (`S_o` or `S_t`) is at most
    `E_merge`'s distance, and the angle between the orientations at most `E_merge`'s angle (section 14).
- **One second later** [I], the harness compares again: stage 1's state stepped 1 s with no new entries, against its
  side's state stepped the same. A body with a different velocity is visibly different within a second.
- **Who checks it.** The model abstracts physics, so it checks the first point and the membership of `U`. The
  harness measures the pose bounds, in the scenario of replica.md 6.3: a moving ball pit and a resting stack, merged
  after 1, 10 and 60 s [@vdw-physics].
- **If it fails,** the risk is the one @vdw-physics named: in a whole-domain replay the other side's activity gives
  new contacts other ids and colours, and an active pile may drift past `E_merge` within a minute. A fork fix
  cheaper than history-free physics may do: create and colour new contacts in canonical shape-pair order.
  History-free physics stays "later", as in the plan. Hiding sub-visible differences is the display layer's job.

### 9.4 v1 and v2

- **v1 (M2): one branch against main.** `theirs` is one machine's line: a long compile (Q3b), or a bad link past
  2 s (plan M3, thin links, which use the same engine). Below `T_merge = 2 s` no merge is needed: the ordinary
  rollback replays the change from its agreed frame. Above it, the machine keeps playing on the old code; when its
  compile ends it replays `theirs` on a second copy in the background, then submits the merge entry. [I]
  - A lone returner has no other member on its line, so its own entries are unwitnessed in practice, and lose to
    any final entry whose outcome they would change [design-reviewer]. Main members' entries it carries, which
    main voided as late, may be witnessed by the returner itself.
- **v2 (M3): split groups.** Both sides may lack finality. `base` is the last cut both hold; both are lines of
  unconfirmed play; the merge entry is the first entry of the reconvened quorum. Within one minute the merge is
  automatic. After it, both lines know they have diverged (S3 `LineDiverged`) and become **branches**, as in git
  (9.6). pe-diverge records the branch point.
  - **A 1/1 split** (a two-player session) is always v2, since at `n = 2` neither side can finalize alone. A
    conflict between two unwitnessed entries keeps stamp order, the user's rule [R mfc2bbd9d28ad]: a hash would
    override that rule for honest pairs.
  - With neither side final, there are no final entries for an unwitnessed entry to lose to. What it loses to
    instead is for WS-model's v3 to settle.
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
  `base`). The tool itself is a later item
  (@item:primordial-engine/world-line-visualization-tool-braided-glowing-li).

**Requirement R-W1, identity under one origin (WS-world, WS-seams).** Every world now shares the origin's first
entry, so a world cannot be named by the hash of its own genesis. A branch must also not start a new genesis, or it
loses the shared history a merge needs. **Adopted by WS-world** (world-model.md r9 2.3, db29bd75, in review):
`LineId` is the hash of the entry that created the line, a branch entry or the origin's; `LineId::ORIGIN` is pinned
by a test and never changes; a branch entry names its parent line and the final cut it branches from; a merged
line's head names both parents; a chain belongs to one line, so continuing on a branch starts a new chain. Entries
bind the line id they were made on (S1's signed world id becomes that line id); a merged line accepts entries made
on a line in its ancestry **only up to that line's recorded
merge point** into it (its last included entry per chain), so both parents' signatures stay valid after a merge.
Anything made later on a parent line belongs to that line, and needs a new merge [design-reviewer, review of
8edd08db]. Object ids and chain ids that include the line id stay unique across branches. @world-model's
spec (section 2.2, `WorldId` as the genesis hash, "a fork gets a new genesis") needs this change; it is theirs and
@engine-architect's to settle.

**The fault bundle under these rules:** a bundle holds world state, so on a non-admin's machine it is discarded
with the world when they leave, unless they export it on purpose [I].

### 9.5 What the model must check (WS-model v2, before merge v1 is built)

Model v2 is the merge against finality (plan, WS-model row): v1's single branch against a final main. Each property
below holds over every reachable case at the model's sizes, and each control is a mutation that must give a
counterexample, as in model v0. The names in `code` are the controls' names; the three pe-harness already wires (P9,
`crates/pe-harness/DESIGN.md` 6.1) keep theirs.

**Properties** (P-M1 to P-M8 for v2; P-M9 and P-M10 with branches, M5):

| | Property | Control that must break it |
|---|---|---|
| P-M1 | **The computation equals the reference.** Stage 1 (9.3) gives `S_1` (9.2) on every object, exactly, with the reference's outcomes, and its group-form decisions give the definition's `Adm`, an entry outside `K` included. Unwitnessed runs are decided by 9.2's two steps and procedure, with the same budgets and the same remainders. Stage 2 with partial re-stepping gives `S_A = S_ref`: the two-stage result equals the from-`fb` definition [C md9479c65a229]. If the model finds a case where they differ, the fallback is sizing `D` for a whole-world replay of `[fb, A)`, never refusing work. | `SeedWriteWriteOnly`: no read-write seeds (step 2). Its counterexample: a back-stamped write to an object a final entry only read. `NoFixpoint` is run too; whether it breaks P-M1 at the model's sizes is the open question below. |
| P-M2 | **Determinism.** Every member computes the same `Merged`, whatever the arrival order of the entries, the witness statements and `M`, and whichever member computes it. | `MergeByArrival`: order the entries by arrival. |
| P-M3 | **No final frame undone.** At every honest member, no state, root or verdict below `A` changes because of `M`. No honest member votes on a cut at or past `A` with a root that lacks `M`'s effect. `K` and `Adm` are fixed when `M` is final. | `ApplyAtStamp`: apply admitted returning entries at their stamps on main's line, which rewrites final frames. `VoteBeforeMerge`: vote before computing, which gives a false root split. |
| P-M4 | **Only witnessed entries override, below `F_M`.** An entry of `E_o` has a different outcome in stage 1 than on main only if a witnessed returning entry stamped before it is in its group. In `[F_M, A)`, any merged change may override an entry, within `D`, and each is reported (9.3a) [C mf06158845447]. | `NoWitnessCheck`: treat every returning entry as witnessed. A lone returner's back-stamped grab then beats a final grab. |
| P-M5 | **The user's order rule holds for witnessed entries.** A witnessed returning delete stamped before a final add-on orphans an object add-on and refuses a component add-on, with a notice. One stamped after destroys it. The model must reach each case, at depth 1 and depth 2. | `RefuseAllEarlier`: refuse every returning entry stamped before a final entry it conflicts with (the general form). The witnessed delete-first case then fails. |
| P-M6 | **Keeps work, and reports every loss.** Every returning entry is `MergedIn` or `Refused` with a reason. Every final entry stands or is `Overridden` with its cause. Every object outside `K` has its fast-forward value. | `MergeDropsSide`: drop one side's non-conflicting work. `VoidSplitEntries`: leave out returning entries that main voided. |
| P-M7 | **Once.** Every entry takes effect at most once: an entry in both lines once, as main's. A second merge entry naming entries already merged in changes nothing. | `NoDedupe`: count an entry in both lines twice. A non-idempotent op, such as a create, shows it. |
| P-M8 | **Admin lists in an automatic merge** (9.6): a three-way set merge against `base`, so a removal on one side survives. | `UnionAdminsAlways`: take the union, which restores a removed admin. |
| P-M9 | **Line ancestry** (R-W1): an entry made on a parent line after its merge point is refused by the merged line. | `AcceptAfterMergePoint`. |
| P-M10 | **Consent** (9.6): the open world is never fast-forwarded to a rejoiner's changes without the rejoin consent. | `FastForwardWithoutConsent`. |

**Examples the model must reach**, the controls of [C maf0da5275cdd] among them:
- a lone returner's back-stamped grab loses, and so does its back-stamped delete of an object main later edited;
- a returning push that, in the merged order, hits a resting ball a final grab holds is refused, though nothing in
  `ours` wrote the ball (both reviewers, r4);
- a run whose early entry conflicts keeps its later, independent entries (step 2's remainder);
- a witnessed earlier delete orphans main's later add-on;
- a returning entry voided by main as `Late` that changes no final outcome is merged in at its stamp;
- a returning write to an object main never touched is kept (fast-forward);
- an entry both lines hold is applied once.

A Q3b long compile is a special case: a connected machine's entries are on time, so they are in `E_o` and the merge
has little to do. Its earlier delete orphans main's later add-on by the ordinary rule, and the harness test of
section 12 checks it.

**What the model settles first:**
- **The fixpoint** (9.3, step 5). The case to model: an object only `theirs` moved, which in the merged replay a
  script or entry of `ours` reaches. Does `NoFixpoint` break P-M1? If no case at the model's sizes needs it,
  pe-diverge still keeps it, since it costs nothing when nothing grows [I].
- **The read-write seeds** (step 2): `SeedWriteWriteOnly` must break P-M1. If it does not, the seeds can be
  narrowed.

**A sketch of a model that suffices** [I, for @world-chunks to change freely]:
- **Members:** `n = 3`, with two on main and one returner, so that a non-author can witness. One more run at `n = 4`
  with one Byzantine member, who signs any witness statement (the collusion residual), shows that the residual is
  the only gap.
- **Objects:** a parent `p` with a child `c`, and a free object `x`. Each has one field (values 0 or 1), a hold
  slot, a component slot, and a version counter. `p` and `x` have bodies in one domain, with a settings object `g`
  (gravity, say). The step is modelled truthfully, without the domain rule built in: it reads every body and `g`,
  and writes the bodies that moved; its reads of its own domain's bodies do not seed, as in 9.3 step 2. A body moves
  when written, or when it touches a moved body, contact being a nondeterministic choice. The domain rule must then
  emerge from the read-write seeds and closure, or a
  counterexample shows it is needed [design-reviewer, shader-reviewer, r2 Low 1].
- **Operations:** `Grab(o)`, `Release(o)`, `Write(o, v)`, `WriteIf(o, version)` (a precondition), `Delete(o)` (with
  its subtree), `Create(child of o)`, `Reparent(o, under)`, `Attach(component to o)`, and `Write(g, v)`.
- **Time:** stamps 0-5, `fb = 0`, `F_M = 4`, `A = 6`, and `B = 1` frame, with stage 2's entries stamped 4-5. Each
  returning entry may have a witness statement from
  each non-author, at any frame `f`, nondeterministically.
- **Inputs:** every `E_o`, `E_o2` and `E_r` of up to 2 or 3 entries each. The model computes the two stages and the
  references `S_1` and `S_ref`, and compares them.

**Measured in the harness, not the model:**
- the pose tolerance of 9.3b;
- no vote waits past `A`, for each of the two cases that grew `K` late under r2 [design-reviewer, r2 Medium 3;
  C md9479c65a229]:
  - a main entry in `(F_M, A)` that first touches an object `theirs` changed;
  - one that is main's first body change in a domain where only `theirs` had moved bodies;
- the returner grabs an object during `D`, and the grab is kept [C mf06158845447];
- a thin player a minute behind at 120 Hz returns, and `D` stays bounded [design-reviewer];
- a silent member carrying many runs exhausts its own chain's `N_unw`: its later entries are refused with
  `MergeBudget`, `D` stays within its bound, and other chains are untouched;
- a thin player a minute behind, plus two main members' late entries it applied: the thin player's run is not
  refused for budget (both reviewers, r4);
- a thin player whose run passes step 1 gets the merge at `A1`, not `A2`;
- the replay speed `ρ` that members reach, which sets `R_merge` and so `D`;
- the merge's cost, as how far `K` spreads over a minute of a busy pile (file 14, section 6). It is an experiment
  before the merge is promised (plan section 8).

pe-diverge's `merge` replays the model's traces (plan 4.1, rule 2).

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
  digest (pe-app's start-up digest, 5 step 2), and (from M2) each module's hash. A difference refuses the join with its reason (plan M1 acceptance). The
  last self-test result is part of a member's state that section 5 reads.
- **R-E1, stable effect keys (WS-state, through S5).** Events carry the key of D6.
- **R-M1, witness statements (WS-agree, from M2).** A signed statement `(m, heads, f)` by which a member shows it
  held another author's entry from frame `f`, **the frame at which it first held it**, recorded on receipt (9.3). It
  is kept for up to `T_diverge` with the merge base, and carried in a merge entry. The candidate carrier: the chain
  heads beyond the cut that every vote already carries (agreement.md 3), each with its receipt frame. During a
  split, a side's members still sign votes, though no cut becomes final. Whether a vote's heads or a separate
  acknowledgement carry it is WS-agree's choice; the merge needs only the three fields and the signature [I].
- **R-M2, the merge report reaches players and agents (WS-api, pe-render).** `M`'s outcome lists each returning
  entry as `MergedIn` or `Refused { reason }`, and each final entry it overrode as `Overridden { hint }` (9.3,
  step 6; 9.3a). Each author is told through the refusal's reason, as for every refusal. The notice of an override
  names the overridden entry and its hint, and the overrides appear in the merge preview the admins see
  [C m00d0a845a25c]. A final verdict itself never changes [C mef721d3fbd4f], so an agent watching an entry reads the
  merge's outcome from `M`, not a new verdict.
- **R-L1, a script's source has a size the repair can see (WS-logic, WS-world).** A rung-2 repair that lacks a
  module waits up to `T_module`, computed from the bytes of the script's source tree (4.2). The `modules` row
  should carry that total, or a cap on it should hold per world, so that a machine can size the wait before it
  fetches. Today the only bounds are a blob's 64 MiB and a transaction's 256 MiB, which at the thin links' 1 KB/s
  take 18.6 hours and 3 days [I].

## 11. Seams: what pe-diverge needs

**Settled in pe-seams** (`crates/pe-seams`, `RULINGS.md`): v0 on main at f3c1e6a7 with 019234f7's four changes for
WS-diverge; S2 v1 (c93fa631) after the code review of ff37b4fd; S2 v1.1 (budgets) and S3/S2 v1.2 (retention). What
pe-diverge uses, which replaces the proposal kept below for the record:
- **Only `finalize` moves `Cv` and the floor.** `Replica::finalize(&FinalCut)` returns `Agreed`, `Diverged { mine }`,
  `NotReady` or `Damaged`. The driver passes each final cut and the replica's verdict to `Diverge::on_final`; after a
  repair at a final cut, pe-diverge calls `finalize` for it again.
- **Rung 1** is `replay_to(&Cut) -> Result<Hash, RootError>`: my root at the cut, after a replay from a verified
  `Cv`; `Pending` while it runs, `NotKept` or `Damaged` for rung 2 at once.
- **Rung 2's calls:** `Replica::tree(&root)` gives `Tree::{node, export(&[LeafKey]), object_hashes, verify}` on a
  held root (the floor, `Cv`, final cuts within `T_keep`, my root at a cut I diverged at, and the root I voted with
  at each cut not yet final); `TreePath::{World, Tables, Domain, Objects { prefix }}` by nibble prefix, and
  `Node::hash`, the one hash definition, against which pe-diverge checks every node a peer sends;
  `physics_image(&root)`; `import_at_cut(&GroupSel::Domain, CutImport { cut, target_root, .. }, source)`, which
  checks every record's proof against the target root and re-simulates to the present (`Pending` meanwhile), and
  moves neither `Cv` nor the floor.
- **Root splits** (S3 v1.3): `RootSplit { cut, roots: Vec<(Hash, Vec<(MemberKey, SelfTest)>)> }`, emitted again
  whenever a voter's verdict for the split arrives; the newest for a cut supersedes the older, and the driver hands
  each to `Diverge::on_root_split`. `SelfTest::{NotRun, Passed { split }, Failed { split }}`: a verdict names its
  split, and is final for it. The driver runs the self-test on `Action::RunSelfTest { cut }`, once per episode, and
  votes on `Action::Revote`; `vote` answering `AlreadyVotedThisView` is expected there (the verdict goes out as a
  self-test report), not an error. The driver passes every `CutFinal` to `Diverge::on_cut_final` as it arrives,
  before its entries are held: a split's pick waits until `EntrySource::final_cut` has caught up with it (5, step 3).
- **`LeafKey::Config`** (S2 v1.3): the world's config bytes, a leaf of the world node. pe-diverge fetches it when
  `World.config` differs, and on every whole-state fetch; its proof is `[Node::World]`.
- **Identities and time:** `PeerId` is `MemberKey`; `CutRef` carries the line (R-W1 closed); `tick(now: HostTime,
  clock: &dyn FrameClock)`; limits are `Duration`s turned into frames by `frames_ceil`.
- **Events and actions:** trace events are `pe_seams::DivergeEvent`; what the driver must do is pe-diverge's own
  `Action` list (`Revote`, `RunSelfTest`, `WriteBundle`).
- **The repair lane's messages** (S4 `Lane::Repair`): node, record (by `LeafKey`) and physics requests and replies,
  `NotHeld`, and `Busy { retry_after_us }` for a peer over its serving budget (4.2, step 2).
- **Mutations:** `ImportUnchecked` and `PatchNoReplay` belong to pe-replica now, whose import checks the proofs and
  replays; pe-diverge's are `SkipVerify`, `StickWithSource` and `UncheckedNodes`.
- **Apply order** (@coordinator-opus, md713d4fc34d0): pe-diverge applies no entries in M1. The merge's replay (M2)
  will iterate a frame's entries only through pe-replica's one "entries of frame f in apply order" helper.

The proposal as sent (names after the plan's Appendix A sketch):

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
    fn footprint(&self) -> Footprint;   // per frame: reads and writes, per entry and per step (below)
}
```

- `Footprint` (from M2, for 9.3) keeps **reads and writes separately**, per entry and per step. A refused entry
  records the reads that refused it. A step records the code version and config rows it ran under [design-reviewer,
  shader-reviewer, r2 Lows].
- `GroupSel = Domain | Objects(BTreeSet<ObjectId>)`. M1 uses `Domain` only. A group containing any bound body acts
  as `Domain` (9.3) [agreed with @vdw-physics].
- **Settled names** [@world-model, world-model.md r5, d04394b; @vdw-physics, replica.md d21a809]: pe-world owns
  `node`, `export`, `import`, `verify` and `object_hashes`; pe-replica owns `physics_image(root)`, the receiver
  `import_at_cut(group, parts, source)`, and rung 1's `replay_from_last_agreed(source)`. pe-diverge calls those
  names; the sketch above is the shape, not the final signatures, which pe-seams v0 fixes.
- `Root` is a handle that keeps its structure alive, and is `Send`, so a worker can replay it.
- **Script modules**, in pe-seams' next version (@engine-architect mee156bec907c, on ruling meaa5b7bf57e9), all
  provided methods with empty defaults:
  - `runner_identity(&self) -> Option<RunnerIdentity>`, with `RunnerIdentity(pub Bytes)` canonical bytes, so a
    bundle can say what differed. It covers the runner's own build, not the app's, which the hello already compares:
    the wasmtime version, the fuel and limit settings, every wasmtime `Config` setting that can change a result
    (the enabled proposals, relaxed SIMD above all, and NaN canonicalization), the instrumentation pass's version
    and pe-logic's version [design-reviewer, Medium; @engine-architect md301de2c0af3: "build id" is the runner's];
  - `module(&self, hash: &Hash) -> Option<Bytes>`, the metered bytes as `author_module` hashes them;
  - `vote_holds(&self) -> Vec<Frame>`, level-triggered, which pe-app reconciles with S3's `hold_votes_from` and
    `release_votes_from`. An import resets it to what the imported state requires (4.2).

  pe-diverge reads the first two for bundles (6) and the third to finish a rung-2 repair (4.2). @vdw-physics's
  runner fills them in `ReplicaImpl`.
- Roots of the final cuts of the last `T_keep` are kept, and served to peers: enough for repair. A machine serves
  only roots it matched at a final cut, and, during a root-split episode, its own root at the split's cut, which is
  not final (section 5). The requester checks every byte against the reference's root either way.
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
- **Witnesses** (from M2, for 9.3; R-M1): for an entry, the signed statements `(m, heads, f)` by other members
  whose heads cover it, so the merge can test "witnessed within `B`". They are kept with the merge base, for up to
  `T_diverge`, and the merge entry carries those it relies on.
- From M2: the merge entry is an ordinary entry; `Merged` and (M3) `LineDiverged` events (renamed from the sketch's `Diverged`, which is pe-diverge's root-mismatch event; @harness-eng 3b8fa6c);
  `Unconfirmed(time)` and `QuorumLost`, as sketched.
- A member's vote carries its last self-test result, or the guard's verdict is otherwise visible to all (5, step 2).
- **`RepairStuck { cut, rung, differing, why }`** (S3.8 v1.4): pe-diverge raises it once per stop (4.3).
- **`ImportError::RateMismatch { world, mine }`** (S2 v1.4): the import's config names another rate. pe-diverge
  reports it and stops the ladder at once (4.3); it never adopts the world's rate.

### 11.3 S1, S4 and S5

- **S1:** a merge entry, as a protocol payload (M2). No "rebased from" link is needed: voided entries go into the
  merge as they are (9.3) [I].
- **S4:** repair messages travel on the `Repair` lane. pe-diverge's messages: `NodeRequest { root, paths }`,
  `NodeReply { root, nodes }`, `RecordRequest { root, ids }`, `RecordReply { root, records, blobs }` (wire version
  2; each blob at most 256 B). Unsigned: every reply is checked by hash.
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
    pub fn events(&mut self) -> Vec<DivergeEvent>;         // Repaired{rung}, RepeatedDivergence, RepairStuck, RootSplitEnded
    pub fn join(&mut self, e: &FinalCut, inviter: MemberKey, replica: &mut dyn Replica, now: HostTime); // 4.5
    pub fn join_state(&self) -> Option<&Join>;             // Joining, Joined or Failed, each with E's cut
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
3a. **A repair refused again and again** (4.3): the replica refuses every import the same way. The ladder stops
   after `N_stuck` (one stuck bundle naming `Config` first, one `RepairStuck`), fetches nothing until its probe, and
   a probe failing the same way writes nothing new. A different failure restarts it, and a cure clears it. Keys
   that alternate stop after `N_stuck_any`, the bundle naming both. A rate mismatch stops on the first refusal,
   with no source rotated. Controls: an outage of every source (test 10) and test 3's computational fault never
   stop the ladder.
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
is orphaned in both); a 1/1 split in both orders; two bodies only one side each moved, which meet in the merged
replay (the domain rule, 9.3 step 2); a sticky root-split
reference under a persistent fault at `n = 2`; the pose tolerance of 9.3b. pe-harness wires the mutations
`MergeByArrival`, `MergeDropsSide` and `VoidSplitEntries` to its property P9 (`crates/pe-harness/DESIGN.md` 6.1,
branch `harness/design`); pe-diverge puts them, and the other controls of 9.5, behind its `mutants` feature.

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
| `T_module` | How long a rung-2 repair waits for a module it lacks before it restarts and counts `ModuleMissing` (4.2) | Over the `n` modules a repair lacks: `W_fg + ΣS / R_thin + n T_build`, with `R_thin` = 1 KB/s and `T_build` = 20 s; 211 s for the 6.4k-line benchmark guest alone, 23 s for the lamp. Until R-L1, a deadline recomputed as the source trees' nodes name lengths, with `W_fg` per tree level (4.2) [C m571b81cfbaa6; sizes V, times I] |
| `T_peer` | Wait for a repair source before trying the next | 4 round-trip times, at least 100 ms |
| `T_join_peer` | A join's wait for its source (4.5), and the least time before a donor serves one peer the same physics image again | 5 s [V: the two-process demo joined with it, @engine-architect md2fa377743a4] |
| `T_join` | A join's deadline from its start: until then its sources are asked again in turn (4.5) | 25 s, inside pe-app's 30 s `T_join` step (app.md 4) |
| `N_descend` | Descents per rung-2 attempt | 3 |
| `B_serve`, `R_serve` | Serving budget per peer: burst and refill | 8 MiB; 4 MiB/s |
| `N_busy`, `T_busy` | `Busy` answers in a row before a source is rotated; longest wait one can ask for | 4; 1 s |
| `T_recur`, `N_recur_max` | Time within which a repeat counts; repeats before the event | 1.5 s; 3 |
| `B_evidence` | The largest replay evidence (`Cv`'s whole state, encoded) a bundle carries (6) | 64 MiB [I: the M1 fetch limit is about 6 MB, so a world that rung 2 can repair always fits] |
| `N_stuck`, `N_stuck_any`, `T_stuck_probe` | Refused repairs in a row before the ladder stops: with one key; with any key and no cure. The probe interval while stopped (4.3) | 3; 10; 30 s |
| `B_keep`, `B_bytes` | Bundles kept on disk | 50; 500 MB |
| `T_blend`, `T_fast` | Blend times | 100 ms; 40 ms |
| `E_small`, `E_snap` | Error thresholds | 5 cm or 10°; 1 m or 90° |
| `D_effect` | Delay of others' unconfirmed effects | 0 s |
| `T_effect_lag` | Greatest lag, from an effect's frame to the arrival of its cause, at which a new one-shot effect still plays | 1 s, the poor-link target [R meef4f248923b] |
| `E_merge` | Pose tolerance within which a group only one side touched counts as unchanged after a merge | Set from the M1 headset tuning: at most the display layer's sub-visible threshold. 5 cm or 10° is visible in VR at arm's length [design-reviewer] |
| `R_merge` | `ρ`, the replay speed over real time every member must reach; sets `D = (N_1 + ρ L) / (ρ - 1)` (9.3a). **A per-world setting, fixed at genesis**, set by the preset from measurement with margin. Must exceed 1. A machine slower than its world's `R_merge` holds its vote at `A`, bounded by `T_merge` [C m1bf05e0a31df] | 15 for the 200-ball pit, 5 up to 500 balls [I, the main PC only: measured `ρ` 20 at 200 balls and 5.9 at 500, idle priority, shared slots (9.3a)]. Final once the second PC is measured |
| `N_unw` | Tests of unwitnessed runs per chain, per merge; no global cap. A chain past it has its remaining entries refused with `MergeBudget` (9.2) | 16 tests [C m410e256e7a01, m1bf05e0a31df]: one honest thin player's whole procedure (`1 + ceil(log2 14,400)` = 15) |
| `B` | Witness bound: a back-dated entry is witnessed if another member first held it at most this far past its stamp (9.3). WS-agree's | The acknowledgement interval plus slack: about 70-170 ms, 4-10 frames at 60 Hz [V design 8.2] |
| `T_merge` | Divergence past which a merge replaces the ordinary rollback | 2 s [R mf7daea6446ca] |
| `T_diverge` | Unconfirmed play after which the lines diverge | 60 s [R m35138bafc46a] |
| `E_cue` | Correction in view above which a visible cue is required | a hand's width, about 10 cm [R meef4f248923b] |

## 15. Open points

- **For @design-reviewer:** the root-split rule (section 5); the vote-with-root proposal (3.2); rung 2 patching at
  the diverged cut rather than at `Cv` (4.2).
- **For @engine-architect:** S2 and S3 as in section 11.
- **For WS-world and WS-state:** R-H2, R-H3 and R-O1 (R-H1 is met by 4.2a).
- **For WS-model (@world-chunks):** section 9.5, the merge against finality, as model v2.
- **Section 9 r3, settled by the coordinator** (m00d0a845a25c, md9479c65a229, mf06158845447): the read-write seeds;
  the reference merge, defined on whole-world replays; `Overridden` as a report, final verdicts unchanged; the
  domain rule, with the cost experiment gating any promise of merge speed; the two-stage merge with `D` a function
  of the log at `F_M`; overrides in `[F_M, A)` accepted within `D`; the returner's play during `D` kept.
- **For @design-reviewer, in r3** [I]: "changed" as written, and a body changed by contact (9.2); the dispositions
  of `replay`; the witness's receipt frame (R-M1); stage 2's partial re-stepping (9.3a); the 1 s check of 9.3b.
- **Settled by the coordinator for r4 and r5** (mb6cef0210a70, mbab63a25938e, m1bf05e0a31df): runs of one chain,
  decided by an exact procedure; no locality skip; a budget of `N_unw` tests per chain, with `MergeBudget` past it;
  a combined test first, applied at `A1`, and the per-run procedure at `A2` only if it fails; a failing entry is
  refused and the rest of its run tested again; `R_merge` a per-world setting.
- **Settled by the coordinator from the measurement** (m410e256e7a01, m1bf05e0a31df): `N_unw` = 16 tests per chain;
  `R_merge` 15 for the 200-ball pit and 5 up to 500 balls, final once the second PC is measured; no credit for a
  replay of `theirs` started before `F_M`. `D` is both the width of the `[F_M, A)` override window and the
  time the returner sees the merged world while others do not (both reviewers, r3).
- **For @world-model and @engine-architect:** R-W1, world and line identity under one origin (9.6).
- **Settled by the user** (mfc2bbd9d28ad, meef4f248923b): the back-dating rule (9.3); components on deleted
  objects (R-O1); branches after a minute and the later merge (9.4); the visible-error and visibility targets (8).
  Then files 18-20: no world owner, rejoin merges with both admins' consent, admin lists unioned, one origin
  (9.6).
- **Settled by the coordinator:** untouched groups need to be deterministic,
  not bit-exact (9.5), both at 20:40Z; M1's logic edit compiles in under 2 s (ma99622993534).
- **Limits to check before worlds grow** (M2): a rung-2 fetch must finish within the shorter of `W_fg` and `T_keep`,
  about 6 MB or 3,000 objects at the default serving budget [I] (4.2); `Replica::hold` and requester-built proofs
  lift it. The root-split pick from `n = 4`: "most voters hold" and the full voter set (5, steps 3-4).
- **With the user:** nothing open for WS-diverge.
