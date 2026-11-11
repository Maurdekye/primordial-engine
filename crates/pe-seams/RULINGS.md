# pe-seams v0: the rulings

WS-seams (@engine-architect), 2026-11-10. Seams v0 takes the owners' proposals in their specifications: world-model.md
r9, replica.md v0.1 (ws/state 31bf9471), divergence.md v1.10, crates/pe-harness/DESIGN.md (harness/design 9eb28642),
xr.md draft 6, render.md draft 2, transport.md draft 1.2, logic.md (ws/logic 03348346), api.md v1 and its s6.rs
(ws/api 001dffec), and models/cuts (model/cuts-v0 d6fe552e). Where two proposals disagreed, this file says which v0
takes and why. "Provisional" items may change in v1, through WS-seams. Everything else is fixed for M1.

## How to read the crate

- `ids`: `Hash`, `ObjectId`, `MemberKey`, `LineId`, `ChainId`, `EntryRef`, `Pos`, `Offset` (with `Frame` from pe-core).
- `entry` (S1), `replica` (S2), `agree` (S3), `transport` (S4), `view` (S5), `world_api` (S6, feature `world-api`).
- `trace`: one trace vocabulary over S3's and pe-diverge's events. `time`: `HostTime` and durations.
- `test_util` (feature `test-util`): an entry builder, deterministic test keys with a stand-in signature, `Corrupt`.

## The transition of the id types

The id, hash and position types belong in pe-core (world-model.md 9.1, accepted). WS-world's pe-core change is on
`world/core-v1` (stage A, 426d8420) and lands only with pe-sim and pe-physics adapted. So v0 carries copies of their
shapes, from 426d8420: the same names, fields, derives and formulas. When `world/core-v1` lands, that landing replaces
`crates/pe-seams/src/ids.rs`'s definitions with `pub use pe_core::{...}` (@world-model makes the edit; I review it).
Code that imports them from `pe_seams` does not change. Golden vectors produced by pe-core at 426d8420 (a line id,
a chain id, the three object-id derivations, two `Pos` encodings, a `Pos - Pos` and a context hash) pin the copies;
the re-export landing must pass the same test unchanged (@design-reviewer, m0bb9196df5ec).

## The floor, and a member that did not vote

A member that was not among a cut's voters can learn the cut is final while some of its entries are still on their
way (@design-reviewer's High on a18ab39f). So `EntrySource::final_cut` is the newest final cut whose entries this
member holds in full; the replica's floor follows it, and `advance` re-simulates down to that floor, not to the
line's newest final cut. The driver passes `final_cut()` to `finalize` each time it changes. A harness case for
@harness-eng: a non-voter at n = 4 that receives one of a final cut's entries late.

## S2 v1: only `finalize` moves `Cv` and the floor; the tree by prefix

@design-reviewer's High on pe-diverge ff37b4fd (m85b5e2e6f7fc): at n = 2 every divergence is a root split, whose cut
is not final, and v0's rung 1 and import moved `Cv` and the floor to the cut they repaired. Had that cut then been
superseded, the replica would have refused to re-simulate below it, and late entries would have been lost. So, in
v1:
- **Only `finalize` moves `Cv` and the floor.** `replay_to(&Cut, source) -> Result<Hash, RootError>` (rung 1, which
  replaces `replay_from_last_agreed` and its `Rung1`) and `import_at_cut` change the live state only, re-simulated to
  the present, and return my root at the cut. They work at a final cut and at a cut not final yet. After a repair at a
  final cut, the driver calls `finalize` for that cut again; it moves `Cv` once my root matches. @diverge-eng proposed
  a separate dry call and a `provisional` flag on imports (mbde4239f0f8a); one rule for every repair needs neither.
- **`CutImport`** carries the full `Cut` (the imported state is the state at its frame with exactly its entries) and
  a `target_root`: a final cut's quorum root, or a root split's reference root.
- **The tree by prefix** (@design-reviewer's Medium 1; agreed with @world-model, world-model.md 6.2 r12, and
  @diverge-eng). `TreePath` names the upper levels (`World`, `Tables`, `Domain`) and addresses a domain's objects
  trie by nibble prefix (`Nibbles`, most significant first), so two tries of different shapes are compared prefix by
  prefix; `Tree::node` returns the node covering the prefix, passing through path-compressed segments, or `Empty`.
  `Node::Inner` carries its full prefix. A record's proof is its nodes from the root.
- **`Node::hash`** (Medium 3) is the one definition of every node's hash: pe-world builds its tree with it, and
  pe-diverge checks each node a peer sends against the hash its parent named, from the agreed root, so a lying
  source is caught at its first lie. The trie formulas are @world-model's (m02fcb0bd43e8): an inner node binds its
  depth, its packed prefix, its child mask and its children; a leaf binds the id and the object leaf hash; the empty
  trie is `BLAKE3-derive("pe.empty")`. The upper levels follow 6.2, with counts before the variable lists.
  A malformed node has no hash, so a peer's bad node is a fault, never a panic.

## S2 v1.1: bounded work per call

@design-reviewer's review of docs/spec/app.md draft 1 (mf5074c5ab2fd): a repair's replay of up to `W_fg` (2 s,
120-240 frames), or a rollback that deep, run in one tick, stalls the simulation thread, and agreement's votes and
messages with it, for hundreds of milliseconds. So the replica re-simulates at most a budget per call
(`Replica::set_budget`, in simulated time, a local setting) and resumes on the next call; `Advance::caught_up` says
whether it reached the present; `root_at_cut` and `replay_to` return `RootError::Pending`, `import_at_cut`
`ImportError::Pending`, to be called again on a later tick; `finalize` returns `NotReady` until the replica has
re-simulated to the cut. Until it reaches the present again, `view` keeps the last view published there. M2's second
copy later keeps the old prediction moving during a deep replay instead. The budget's floor is two frames of simulated time at
the world's rate; a smaller value is raised to it (@design-reviewer, m94ee513a61ac).

## S3 v1.1: dissemination before finality

@harness-eng's question (m2f91e690922b) was whether P4 can stay strict when an author sends an entry to only some
members. @coordinator-opus decided option (a) (mc5d9ba695401), taking @fable-reviewer's suggestion and its narrower
wording (m9d60cb892d0c; the user's ruling mfcf10231065f): P4 stays strict, and S3 promises dissemination. The
contract line, in agree.rs:

> An entry held by any honest member of a log is held by every honest member of that log before any of them treats
> as final the first cut that covers or voids it.

- It says "member of that log", not "of the session", so it survives the move to per-cell logs (file 11, 3.4).
- An entry held only by its faulty author is out of scope.
- pe-agree (@world-chunks) relays each new entry once, the fast path. The guarantee is fetch-by-name at the cut: a
  member does not finalize a cut until it holds what the cut names, and it fetches anything missing from any voter,
  not only from the author. The per-author rate budget counts relayed and fetched entries, void ones included.
  pe-agree's cut model gains the property that honest members' void sets are equal, against an adversary that sends
  to only some members.
- The harness keeps P4 strict and also runs probe-d1 with the relay dropped, so the fetch path is tested.
- Waiting for a missing payload can stall a cut, so the fetch's worst case on a thin link is measured before M1's
  acceptance, not assumed.

## The conflicts between proposals, and v0's rulings

| # | Question | v0 takes | Who changes what |
|---|---|---|---|
| 1 | The tree calls' names | `node`, `export` (by `LeafKey`, so tables are repaired like objects; @diverge-eng, m454882c5cf87), `object_hashes` and `verify`, on `Tree`: pe-world's calls on one held root, reached through `Replica::tree(&root)`. The receiver of an import is pe-replica's `import_at_cut` (2) | replica.md drops `tree_node`, `object_state` and `patch` as seam names; pe-world's internal patch call is its own |
| 2 | The import receiver | `Replica::import_at_cut(group, CutImport, source) -> Result<Hash, ImportError>`: every record checked against the target root first, then a replay to the present; `NeedsPhysicsImage` when a bound body is in it without the image; it moves neither `Cv` nor the floor (v1) | divergence.md 11.1's `import(at, records)` becomes this call |
| 3 | How a replica is stepped | replica.md 8: `advance(present, source)`, `outcome`, `root_at_cut`, `finalize`, and rung 1 (`replay_to` since v1). Keyframes, save and restore stay inside the replica; permission checks are outcomes. Nobody outside restores an arbitrary state | world-model.md 9.2 and divergence.md 11.1 drop `step`, `save`, `restore` and `permits` from the trait. The driver loop is WS-state's `drive` in pe-replica (@harness-eng's 3.2) |
| 4 | Roots | Named by `Hash`; held roots are reached through `Replica::tree`. A `Send` root handle comes with M2's fork | divergence.md's `Root` handle and `root_of` go |
| 5 | `physics_image` | `physics_image(&Hash) -> Option<PhysicsImage>`, one blob with its check hash in v0 (provisional: chunking by hash with the Bulk lane) | divergence.md's `PhysicsBlobTree` waits for that |
| 6 | The second copy (M2) | Not in v0. v1 adds a fork, per-group restore and step, and the footprint, with the merge | — |
| 7 | How long roots are kept | The floor, `Cv`, and the final cuts within `T_keep` (1.5 s by default, at least the last 2 cuts), served to peers; also my root at a cut I diverged at, until its repair ends, and the root I voted with at each cut not yet final (a root split's reference serves it; @diverge-eng, m454882c5cf87). The "last 8 cuts" I wrote earlier is withdrawn: every limit is a duration (md4b42d16d9df) | world-model.md 9.2 and DESIGN.md S2.4 drop "8" |
| 8 | Where `physics_global` sits | In each domain's root, as world-model.md r9 6.2 has it (@design-reviewer's r5 Low 1): M4's domains then change no format | replica.md 5.1 follows r9 |
| 9 | pe-world's side of a step | Not a seam (pe-world and pe-replica are both behind S2). replica.md 3's list holds: each applied entry returns its physics effects (changed objects, pushes, a carried check), acted on before the next entry, and `World::end_frame(f)` expires claims and cooldowns | @world-model provides it in pe-world's API |
| 10 | Where WorldApi reads poses | Poses and velocities from the published frame view (S5), live; rows from pe-world's snapshot. `BodyState` is hash-only and never read by the API | replica.md 2.3's "velocities as of the last cut" option goes |
| 11 | The time a sans-IO core gets | `HostTime` for every core, with the frame from pe-net's clock model through `FrameClock` (S4), passed to `tick` as an argument so it is no hidden input: S3's is `tick(now, clock)` | divergence.md 11.4's `tick(now: Frame)` becomes `tick(now: HostTime, clock)` |
| 12 | The cut cadence's units | A duration, a world setting, turned into frames by `frames_ceil` at the rate in effect where the bound starts. The model keeps its frame constant | replica.md 5.3's "K, a world setting" is in seconds |
| 13 | World id or line id | The line id: `Entry.line` is signed, `CutRef.line` names the cut's line, and ids derive over it. `WorldId` goes. @diverge-eng's R-W1 is closed | The harness's envelope (DESIGN.md S1.1) and the model's `world` field read "line"; api.md 11's "world" in the id derivation reads "line" |
| 14 | The event vocabulary | `AgreeEvent` (S3) and `DivergeEvent` (pe-diverge), wrapped by `TraceEvent` for the harness. The final-cut event is `CutFinal { cut, root, voters }`; below-quorum divergence is `LineDiverged` | The harness's P1 reads `CutFinal` |
| 15 | The model's actions | The model keeps its own vocabulary; the harness's `conform` maps `TraceEvent` onto it | — |
| 16 | What a cut holds | `Cut { at: CutRef { line, height, frame }, heads: [ChainHead { chain, author, seq, entry }] }`; `FinalCut` adds the root and the voters' signatures. Views and certificates are pe-agree's protocol, not the seam | — |
| 17 | Two `Outcome`s | Both, in their modules: `replica::Outcome` is the world's verdict on an entry; `world_api::Outcome` is the API's report of a transaction | — |
| 18 | `Transaction`, `Op`, `Target` | pe-world's binary transaction stays in pe-world; S6's JSON transaction is in `world_api`; api.md 5.3 maps one onto the other | — |
| 19 | Refusal vocabularies | `replica::Reason` is the world's (provisional; @world-model owns its list). `world_api::RefusalCode` is the API's, which adds reasons outside the world (rate, readiness, compile, size). pe-api maps | — |
| 20 | Id types at the seam | S1's types. S6 keeps its text forms (`"o:…"` and the rest) with `From` both ways; an object id's text is its `u128` big-endian, so text order is numeric order. `PeerId` is `MemberKey` | transport.md 3.1's `[u8; 32]` reads `MemberKey`; pe-diverge's `PeerId = u32` stand-in goes |
| 21 | `Actor` and `Producer` | pe-world's `Actor` is in the edit payload; S6's `Producer` is the API's; pe-api maps (`Hand` is `Actor::Person`) | — |
| 22 | `CommitScript`'s fields | pe-world's op, with world-model.md's field names. In M1, `activate_at` equals the entry's stamp (WS-logic's rule), and pe-world refuses any other value as `Invalid` | logic.md 6.1 takes world-model's names; world-model.md 7.1 narrows "no earlier than" to "equal to" for M1 |
| 23 | A view object's fields | id, class, `Pos`, rotation, scale, velocities, shape, appearance, light, bounding radius, `hand_of` (member and hand), `held_by` (member and the hand object), flags `OWN_HAND`, `HELD_LOCAL`, `CORRECTED`, edit version. No drawn pose (D1: it is pe-render's) and no correction offset (pe-render computes it from consecutive views). The grip offset is derived from `held_by`'s hand | — |
| 24 | Frame view: trait or type | A plain struct, published as `Arc<FrameView>` and never mutated after; consumers take `&FrameView`, and a mock is a value | xr.md 8.1 and render.md 5 read `&FrameView` for `&dyn FrameView` |
| 25 | A one-shot effect's key | `EffectKey { frame, cause, object, kind, ordinal }` (divergence.md D6, render.md 8). replica.md 7's keys are instances of it. "Once for the world" or "once per player" is a property of a script's effect kind (logic.md 3.2), not of the key | — |
| 26 | The rig's position type | `Pos` | xr.md 8.1's `WorldPos` reads `Pos` |

## The other open asks

- **@api-eng's O8b (serde):** the feature `world-api` turns on S6, with serde, serde_json and schemars. Nothing else in
  the workspace pays for them. s6.rs moved in unchanged, except: each id's JSON Schema pattern asked for as many hex
  digits as the id has bytes, so a validating client would have refused every real id; it now asks for twice as
  many (a test pins it). And `From` conversions to S1's ids were added.
- **@ballpit-viz's render.md 12:** velocities, `CORRECTED`, effect keys with the retracted list, a bounding radius and
  `Arc` frame views are in. The grip offset is derived (23). An effect's freshness is `resimulated` plus
  `trigger_arrived`, the arrival time of the entry whose arrival caused the re-simulation (provisional, to settle
  with the display layer's tests).
- **Signing:** `Signer` and `Verifier` traits in S1, so pe-seams carries no cryptography. pe-net-crypto implements
  them with Ed25519, strict verification, over `SIGNING_PREFIX ‖ signing bytes`. `test_util` has a stand-in for
  in-process tests.
- **S1's byte layout** (format 1: `Entry::signing_bytes`, `SignedEntry::encode`) is provisional; WS-agree may change
  it in v1, through WS-seams. A chain's first entry carries its nonce, so anyone can check its chain id.
- **The vote hold** (@iter-bench's N3): `hold_votes_from(frame)` and `release_votes_from(frame)`.
- **The simulation rate** (the user, mc8dc9e5a6ffb): `SimRate`, 30 to 120 Hz in whole hertz, 60 by default;
  `FrameClock::rate_hz` always returns one. pe-physics runs `ceil(240 / rate)` sub-steps (@vdw-physics).
- **Voids name their cut:** `AgreeEvent::Voided { entry, author, reason, cut }`, so the harness checks that every
  void is decided by a final cut and that members agree on which (@harness-eng, m3bb675a96918).
- **Root splits** carry each voter's self-test verdict (`RootSplit { roots: [(root, [(member, SelfTest)])] }`), for
  the tie rule at n = 2 (@diverge-eng, m454882c5cf87).
- **The node hash at the seam** came in S2 v1 (`Node::hash`), above.
- **macOS:** the vendored wasmtime's unwind registration through `dlsym` is untested there; no determinism claim for
  macOS until tier 2 runs a trap test on it (@design-reviewer's Low).
- **Roots for votes:** S3 lists the cuts it wants roots for (`wants_roots`); the driver answers with
  `Replica::root_at_cut` and `Agreement::vote`, which is also the revote after a repair.
- **The script contract** (@iter-bench, m7a29c468fa08): the script crate is `forbid(unsafe_code)`, and the SDK hands
  read columns out as shared slices. pe-logic's skip of unchanged copy-in depends on it, so it is a rule of the
  script build, not a convention.

## Skeletons, and merging them into your branch

Every M1 crate is a workspace member now, with a skeleton `Cargo.toml` and `src/lib.rs` (`src/main.rs` for pe-app).
Your crate and its manifest are yours; add dependencies there. When you merge main into a branch that already has
the crate:
- keep your sources; take the skeleton's `[package]` header (`edition.workspace` and the others) and its
  dependency on pe-seams, and add yours;
- delete the interim `[workspace]` table and the crate's own `Cargo.lock`;
- replace local seam stand-ins with `pe_seams` (pe-xr's and pe-diverge's `seam.rs`, the harness's stand-in types,
  pe-api's `s6.rs`, which is now `pe_seams::world_api`).

`models/cuts` and `feasibility/` keep their own workspaces (excluded at the root). `crates/pe-logic/vendor` is excluded
too: it is the patched wasmtime, used through `[patch.crates-io]`, never built, tested, formatted or linted as a member.
The root `rustfmt.toml` is `max_width = 120` with `use_small_heuristics = "Max"`, the closest stable settings to the
kept crates; their owners reformat them once before the gate checks formatting. box3d-fork is never formatted.
