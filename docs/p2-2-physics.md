# P2-2: physics wrapper (`pe-physics`)

Author: @vdw-physics. Package P2-2 of design 23.4. Branch `p2/p2-2`, with main merged at b5f3013
(P2-1 final, D2's parallel Box3D and everything since). Reviewer: @design-reviewer; the findings
of the first review (P22-1 to P22-13) and the second (P22R-1 to P22R-8) are answered in "Review
fixes" below. Docket:
@item:primordial-engine/p2-2-pe-physics-the-engine-s-only-door-to-the-bo.

Tags: **[V]** tested here, **[I]** inferred or proposed.

## What is built

`crates/pe-physics` depends on the box3d fork, `pe-core` and `pe-sim`. It is the engine's only way
to reach Box3D.

| Item | What it does |
|---|---|
| `PhysicsConfig`, `engine_world_def()` | The engine world: Box3D defaults plus scale-dependent contact recycling (HarmonicMean, user ruling 2026-10-30), gravity, sleep, and the **worker-thread count** (D2; results are identical for every count; clamped to 1..=32). `PhysicsConfig::upstream()` turns recycling scaling off, for comparison with Box3D C's goldens only |
| `BodyDesc`, `ShapeDesc`, `Geometry`, `limits` | Plain engine descriptions: static, kinematic or dynamic; sphere, capsule, box, or the convex hull of 4 to 1024 points (at most 64 shapes per body); density (Box3D's default is water's, 1000 kg/m^3) and surface material. **Validated and bounded before anything reaches Box3D** (4.3 rule 6): no NaN or infinity, and magnitudes within `limits` (positions 1e6 m, sizes 1e-4 to 1e4 m, density 1e5, speeds 1e4, pushes 1e9), so no finite input overflows inside Box3D. Rotations within 1% of unit length are normalized (correctly rounded `sqrt` and division, so every peer gets the same bits); Box3D itself asserts about 2e-6. Positions and points are `f64` |
| `PhysicsWorld` | One Box3D world. `add_body` / `remove_body` bind bodies to sim objects by `pe_core::ObjectId`; the binding table is a sorted map, so iteration is in id order on every peer. `pose`, `velocity`, `is_awake`. `step(dt)` refuses dt outside (0, 1] s. A refused call changes nothing |
| `set_state`, `set_pose`, `set_velocity`, `set_target` | For P2-3's carried state and hands. `set_state` applies a touch entry's carried state: the pose and both velocities with **exactly** the given bits, the body awake and its sleep timer at zero. **Every peer applies it, the toucher included, at the same frame**: writing a pose makes Box3D recompute the derived state (centre of mass, inertia, proxy), which for an off-centre body differs by rounding from the toucher's integrated one, and the hashes do not show it until the next step (review SET-1); sleep state is not carried, carried state is awake (SET-2). A rotation is taken as it is when Box3D's unit-length check passes and refused otherwise (never renormalized). Values are bounded by `limits`; every check runs before any write. `set_pose` / `set_velocity` are the primitives (no wake / wake on a non-zero velocity, as Box3D's setters). `set_target` is Box3D's `body_set_target_transform`, dt in [1 ms, 1 s]; Box3D turns a body by at most 45 degrees per step. Descriptors follow the same rotation rule: a rotation Box3D takes as unit keeps its bits, so arrivals spawn exactly [V `carried_state_lands_exactly`, `targets_drive_hands`, `rotations_are_normalized`] |
| `Push` | Interactions as impulses (through the centre or at a point), angular impulses, forces, forces at a point and torques. Forces and torques last one step |
| `JointDesc`, `JointKey`, `JointKind`, `GrabSpring` | Spherical, weld and distance joints between two objects or an object and the world, and a **grab** (Box3D's motor joint, D72): force and torque caps, steered by `set_grab_velocity` (which wakes both bodies), with anchors and an optional spring that pulls the object to the hand. Keys are **the caller's** (`JointKey(u64)`, for example the interaction's id) and part of the state: a rollback restores the table with the world. The table keeps each joint's kind; `set_grab_velocity` on anything but a grab is `NotAGrab`. A joint from a body to itself is refused. Removing a body forgets its joints |
| `WorldImage` | `save_image` / `restore_image`: rollback, in place, **local only**. Box3D's world serialization plus the binding and joint tables; geometry is interned once per world. Images are trusted because the only way to get one is `save_image` (`WorldImage::from_bytes` exists only under feature `raw`). An image whose world settings differ from the world's is refused before anything is written. A restore that fails after Box3D started writing **poisons** the world (every call, saves and hashes included, refused) until a good image is restored |
| `Transfer`, `CheckHash` | `save_transfer` / `PhysicsWorld::from_transfer`: a fresh world for a joining peer. Saved against a fresh geometry registry (exactly the geometry it uses, in shape order) with Box3D's debug-only validation flag cleared, so its bytes depend only on the physics state. `check_hash` is the **full-state check hash** of design 9.1: **BLAKE3** (pure Rust, D16, with a key-derivation context) over all of those bytes, 32 bytes long, equal on every honest peer whatever its save history, profile or OS. `from_transfer` restores only if the hash equals the confirmed one, because the donor is untrusted; Box3D must then read every byte, and the new world must save back to exactly the transfer. A live world's transfer is canonical whatever path it took: after a destroy, a freed shape slot is written as a restored world holds it (fork fix 9927b7b, found by @world-chunks; `live_images_are_canonical`, `transfer_canonical_after_destroy`) |
| `state_hash`, `box3d_hash` | `box3d_hash` is Box3D's own hash (body transforms and velocities, the C reference's), which the spike traces record. `state_hash` is the per-frame hash: XXH64 over it plus the binding and joint tables. It does not cover sleep timers, contact caches, warm starts, islands or settings; `check_hash` does |
| `write_root_transforms` | Copies body poses into `pe_sim::Transform`. The physics world's origin is a level-0 chunk. Root objects get their position rebased into their own chunk; objects under a frame get the local transform that places them there in the frame's current world transform, ancestors first (depth, then id). Local scale is kept (physics does not read it; colliders come from `BodyDesc`). A pose the sim store refuses leaves that object's transform unchanged and is reported in `WriteBack::refused`; the others are written |
| `raw` / `raw_mut` | The Box3D world. `raw_mut` is behind feature `raw`, for test scenes (the determinism scenes build through it) and tools: it can break every invariant here |

Several worlds per process work: each `PhysicsWorld` owns its Box3D world and its worker threads.
Box3D's length units are process-wide (as in C), so all worlds in a process share them; the
engine uses metres, and `new` and `step` refuse anything else (`LengthUnits`; the fork's
recording player swaps the units while it plays).

## Evidence

The done criterion is that the phase-1 spike's goldens, 600-frame trace and rollback T1-T5
reproduce through the wrapper, release and debug, on Windows and Linux, with the digests
committed in `spikes/out/vdw/`. `tests/common/mod.rs` rebuilds the spike's pile scene, its
"soonest wins" claim rule and its `RollbackSim` on pe-physics; every check runs in the engine
configuration (`vdw`) and the upstream one (`off`), at 1, 3 and 8 worker threads.

| Check | Windows x64 debug | Windows x64 release | Linux x64 musl debug (WSL2) | Linux x64 musl release (WSL2) |
|---|---|---|---|---|
| Goldens: 4 engine + 4 Box3D C scenes, at 1/3/8 workers | PASS | PASS | PASS | PASS |
| 600-frame trace equal to `trace-{vdw,off}-win-<profile>.txt` line for line; digests `80839f4a54133e13` (engine) and `7903109aab28ed9b` (upstream) | PASS | PASS | PASS | PASS |
| World-image bytes every 50 frames equal to `images-*-win-<profile>.txt` | PASS | PASS | PASS | PASS |
| Rollback T1-T5; T4 is a late joiner: transfer bytes into a fresh world at another worker count, accepted only for the check hash a second peer (one that never saved an image) computed | PASS | PASS | PASS | PASS |
| Transfer check hash (BLAKE3) at frame 300: `80d8a5de696a5d2f...` (engine), `10fbeff4bb73db7e...` (upstream), the same at 1/3/8 workers; full values in the RESULT `transfer` lines | PASS | PASS | PASS | PASS |
| Three worlds in one process, stepped interleaved (engine at 2 and 4 workers, upstream at 1) | PASS | PASS | PASS | PASS |
| API (13 tests): refusals and bounds; hull point and shape counts; rotation normalizing; hull bodies and pushes; images and transfers with joints; canonical check hash; a rolled-back peer's check hash equal to a straight-through peer's; grabs (kinds, wake, spring); foreign, corrupt and poisoning images; hostile transfers; forged confirmations; sim-store write-back with frames and refused poses; worker-count changes mid-run | PASS | PASS | PASS | PASS |

All [V], at commit 5245776 with a clean tree, rustc 1.99.0; logs in `spikes/out/p2-2/`. Each log has
the same 50 RESULT lines (goldens, trace digests, image digests per profile, transfer check
hashes, rollback, the rolled-back check hash, the grab spring), and the script fails if any
differs; debug and release differ only in the 6 image-digest lines. The Linux runs reproduce
the Windows records byte for byte, image bytes included, and the transfer check hashes are equal
in debug and release, which is what lets peers of either profile confirm each other.

The fork's own suite passes (`cargo test -p box3d-rust --release`: 570 + 2, Windows, including the
hostile-image, pair-set, trailing-byte and record-size tests). `cargo clippy -p pe-physics -p
box3d-rust --all-targets` is clean.

Run times (Windows, idle priority, shared machine, noisy): the record tests take 15.4 s in release
and 186 s in debug.

## Review fixes

### Round 2

The second review (@design-reviewer at 593770f, "Round 2" in `design-reviewer/review-p2-2.md`)
found that the check hash guarding joiners was XXH64, which a hostile donor can forge, and that a
crafted broad-phase pair set could hang or panic every step. The fixes are in 5245776.

| Finding | Fix | Evidence |
|---|---|---|
| **P22R-1** (High) XXH64 is forgeable, so "restore only the confirmed hash" does not stop a hostile donor; free bytes to hide a forging stripe | `CheckHash` is BLAKE3 (crate `blake3` 1.8 in `pure` mode, as D16 settles; `new_derive_key` with a pe-physics context) over all of the transfer bytes, 32 bytes long. No free bytes: the fork's `deserialize_into_shell` now requires the image to be read to its last byte, and `from_transfer` requires the restored world to save back to exactly the transfer, which refuses unused, duplicate or out-of-order geometry and any non-canonical encoding. `state_hash` stays XXH64: it detects divergence between honest peers, which is all it is for | [V] T4 at 1/3/8 workers on both OSes and profiles: the joiner's check hash equals the confirmed one and a hash differing in one bit is refused. [V] `forged_confirmations_still_refused`: confirmed for its own hash (as if the confirmation were forged), a transfer with an extra geometry blob or one unread world byte is refused. [V] fork `trailing_bytes_refused` |
| **P22R-2** (High) `des_hash_set` accepts capacity 0 (index panic) or a full table (`find_slot` never ends) | The reader requires capacity at least 16 and a power of two, count at most half the capacity (`add_key` grows before that, so an empty slot always exists), count equal to the occupied slots, empty slots all zero, and every key non-zero, with its own `key_hash`, at the slot `find_slot` reaches for it | [V] fork `hostile_pair_sets_refused`: the scene's real pair set and a half-full set are accepted; capacity 0, 8 and 24, a full table (count 8 or 16), a low count, a wrong hash, a zero key, bytes in an empty slot and a key off its probe chain are refused. **Negative control**: with the old checks the test fails at its first case (capacity 0 accepted) |
| **P22R-3** (Medium) a restored grab key mapped to a weld panics `set_grab_velocity` | Restores check every joint's `JointKind` against Box3D's `joint_get_type` (grab = motor) | [V] `forged_confirmations_still_refused`: a grab recorded as a weld, and a spherical joint recorded as a grab, are refused, through a transfer and through a local image. **Negative control**: without the check the grab-as-weld transfer is accepted and the test fails |
| **P22R-4** (Low) saves and hashes allowed while poisoned | `save_image`, `save_transfer`, `check_hash`, `state_hash` and `box3d_hash` return `Result` and refuse a poisoned world | [V] `foreign_and_corrupt_images` |
| **P22R-5** (Low) a public, hash-less restore path | `WorldImage::from_bytes` exists only under feature `raw` (tests and tools), so an image can only come from `save_image`; `restore_image` documented as local rollback only | [V] the library builds without `raw` |
| **P22R-6** (Low) hull points and shapes per body unbounded | `limits::MAX_HULL_POINTS` = 1024, `limits::MAX_SHAPES_PER_BODY` = 64 | [V] `hull_and_shape_counts_bounded`: 1024 points and 64 shapes are accepted, 1025 and 65 refused |
| **P22R-7** (Low) one unrepresentable pose blocks every write-back | `write_root_transforms` returns `WriteBack { written, refused }`: each transform is written whole or not at all, refused objects keep theirs and are listed in write order, the rest are written | [V] `poses_reach_the_sim_store`: a body past the chunk limit is reported and unchanged, the other body is written, and the store's values check |
| **P22R-8** (Low) no test that a rolled-back peer's check hash equals a straight-through peer's; minimum record sizes untested | Tests added | [V] `rolled_back_peer_has_the_straight_check_hash`: after a 20-frame mispredicted branch (other pushes, a hull body created and destroyed, an image saved every frame) and a restore, 60 frames of re-simulation give the straight peer's `state_hash` every frame and its `check_hash` every 10 frames. [V] fork `record_minimums_within_writer_sizes`: the five named minimums (body sim 100, body state 40, joint sim 100, manifold 40, material 32) are at most the writer's size for every such record of a real scene. The literal minimums (sensor 16, island 28, island sim 4, visitor 6, links 12, cache 8, colour contact 10) are the fixed fields read right after each count [I, by reading] |

The pair set is the reviewer's case, but other Box3D structures in an image (trees, island and
contact links, solver-set indices) are not checked for the same kind of consistency. They do not
need to be: a joiner restores only bytes whose BLAKE3 hash honest peers confirmed, so it gets
exactly an honest state; the reader's checks are defence in depth. Rollback images are local [I].

### Round 1

The first review (@design-reviewer at 52a9ca9, `design-reviewer/review-p2-2.md`) found that world
images reach joining peers from untrusted donors and that the restore path trusted them. The fixes
are in fcedd22; tags as above.

| Finding | Fix | Evidence |
|---|---|---|
| **P22-1** (High) a crafted image aborts a joiner: counts size `reserve` unchecked; indices never cross-checked | Fork: `SnapReader::count` reads every element count and checks it against the bytes left, with a lower bound on each record's size, before any allocation or loop (sensors, islands, the four solver-set arrays, visitors, contact and joint links, materials, manifolds, triangle caches, graph-colour joints and contacts; the rest were already checked). For semantic validity a joiner restores only a `Transfer` whose check hash equals the confirmed one (P22-5), so a donor's bytes are trusted only once honest peers have hashed the same state. Doc comment corrected | [V] fork test `hostile_counts_never_panic_or_abort` overwrites every aligned word of a real image (bodies, hulls, contacts, islands, a joint, a sensor) with `i32::MAX`, `i32::MAX/16`, 2^24 and -1: no panic, no abort. **Negative control**: with the old `sensor_count` read, the test process dies with "memory allocation of 171798691760 bytes failed". [V] pe-physics `hostile_transfers`: any changed byte fails the hash, so Box3D never reads it |
| **P22-2** config adopted unvalidated (NaN, `restitution_iterations` unclamped, unknown combine) | Fork: the config read refuses restitution iterations outside 0..=63, any non-finite setting and an unknown combine byte. `world_settings` / `image_world_settings` give a world's and an image's settings bytes (everything but the step state and the capacity high-water marks). Both restores compare them with the world's own settings **before writing**; `from_transfer` builds its world from the caller's config, so `config()` is right by construction | [V] fork tests `image_settings_match_world_settings` (recycling on and off) and `image_settings_out_of_range_refused` (i32::MAX and -1 iterations, NaN gravity, infinite hertz, combine 7). [V] `foreign_and_corrupt_images`: an engine world refuses an upstream world's image, and is unchanged and not poisoned. T4 now restores each mode with its own config (the old test used the engine config for both) |
| **P22-3** table invariants unchecked | Reading a `WorldImage` checks: object ids sorted, unique, not `NONE`; joint keys sorted and unique; no Box3D body or joint id used twice; every id in world 0; the anchor not a bound body; joint kinds known. `next_joint` is gone (caller keys, P22-4), so it cannot overflow | [V] `hostile_transfers`: a duplicated Box3D body id, swapped objects, joint kind 9 and a joint count of u32::MAX are refused |
| **P22-4** `set_grab_velocity` panics on non-grab keys; keys reissued after rollback | The joint table and images keep each joint's `JointKind`; `set_grab_velocity` returns `NotAGrab` / `UnknownJoint`. Keys are chosen by the caller from deterministic state, like ObjectId, and belong to the state: after a rollback a key names whatever joint the restored state has under it | [V] `grabs`, `images_and_transfers_round_trip_with_joints` (a joint added after the save is unknown after the restore; a key in use is `JointExists`) |
| **P22-5** state hash pose-only; image bytes not comparable between peers | `state_hash` documented as the per-frame hash. New `check_hash` over `Transfer` bytes: fresh registry in shape order, validation flag cleared, geometry content and tables included (BLAKE3 since round 2) | [V] T4: the donor (which saved an image every frame) and a second peer (which never saved one) give the same check hash at frame 300 (XXH64 then: `fc3999792b45a32f` engine, `27d8dd7d28d065de` upstream), equal at 1/3/8 workers, in debug and release, on Windows and Linux (RESULT `transfer` lines, compared across all four logs). `check_hash_is_canonical`: equal after different save histories, different for a different state |
| **P22-6** framed children never synced; half-writes | Framed objects are placed in their frame (ancestors first); each transform is written whole or not at all (round 1 undid every write if any was refused; round 2 reports refused objects instead, P22R-7) | [V] `poses_reach_the_sim_store`: a bound frame (scale 0.5), its bound child and grandchild (local scale 3) end within 1e-5 m and 1e-5 rotation of their bodies |
| **P22-7** validation looser than Box3D's asserts | Rotations normalized; magnitudes bounded (`limits`); same-body joints refused; `step(dt)` and gravity validated; joint anchors, lengths and caps bounded; refusals resolve every bound end before creating the world anchor | [V] `refusals_change_nothing` (radius 1e30, position 1e38, density 1e30, velocity 1e30, force 1e30, point 1e7, same-body spherical and grab, far anchor, dt 0/-1/NaN/2, NaN gravity: all refused, the world unchanged, no anchor created); `rotations_are_normalized` (a 0.8% long quaternion becomes unit to 1e-6 and steps in debug) |
| **P22-8** f32 creation positions and push points | `BodyDesc.position` and push points are `[f64; 3]`. Our build has the fork's `double-precision` feature **off**, so Box3D's `Pos` is `f32` and this change prepares the API. D58 (design 25.12) turns double precision on; @engine-architect ruled it a follow-up right after P2-2 merges and before P2-3 freezes the snapshot format: @item:primordial-engine/p2-2-follow-up-turn-box3d-double-precision-on-d5 | [V] records unchanged (the spike's positions are computed in f32 and widened) |
| **P22-9** grab velocity-only, no wake | Grab anchors (`anchor_a` on the hand, `anchor_b` on the object) and an optional `GrabSpring` (Box3D's motor spring, linear and angular); `set_grab_velocity` wakes both bodies. D72 unchanged: claims are pe-session's | [V] `grabs`: a sleeping ball wakes; a 0.52 kg ball held by a kinematic hand moving at 1 m/s under gravity is within 1e-4 m of the hand's anchor after 3 s |
| **P22-10** `raw_mut` breaks invariants | Behind feature `raw` (the crate's own tests enable it) | [V] the library builds and lints without it |
| **P22-11** process-wide length units | `new` and `step` refuse units other than metres (`LengthUnits`) | [I] not exercised: the recording player is the only code that changes them |
| **P22-12** the geometry registry only grows | Transfers carry only the geometry they use (fresh registry), so joiners never receive rolled-back hulls. The per-world rollback registry still only grows; @engine-architect ruled that compacting it lands with P2-3's keyframe ring (@world-chunks), invisible to the sim and rebuilt only from the oldest kept keyframe at or below the confirmed frame | [V] T4 transfer bytes |
| **P22-13** a failed restore leaves no poisoned flag | `PhysicsWorld::is_poisoned`; while poisoned, step, push, add, remove, joints, pose, velocity and write-back are refused (`Poisoned`); a successful `restore_image` clears it | [V] `foreign_and_corrupt_images` |

One finding of my own while testing the grab: `ShapeDesc::new`'s doc said density 1, but Box3D's
default is 1000 kg/m^3, so a 0.5 m ball weighs 524 kg. The doc is corrected; the behaviour was
always Box3D's default.

## Choices the design text should pick up [I]

For @engine-architect, who owns design.md:
* 23.4 P2-2: the engine reaches Box3D only through `pe-physics`; `raw_mut` (feature `raw`) is for
  tests and tools.
* 7 (interactions): pushes are impulses, forces and torques at the start of a frame; grabs are a
  motor joint with force and torque caps and an optional spring (D72). Joint keys come from the
  interaction that makes the joint.
* 6.1: a physics image is Box3D's world serialization plus the binding tables; geometry is
  interned per world, not repeated per snapshot.
* 9.1 / 9.5: a joiner's physics state is a `Transfer`, restored only when its `check_hash`
  equals the hash the session confirmed for that frame; `state_hash` is the per-frame hash.
  The check hash is BLAKE3 and 32 bytes, so the session's confirmation of it (the acks' `check`
  field) must carry 32 bytes. XXH64 detects divergence between honest peers but is no security
  boundary: every state transfer verified against a check hash (9.1, 9.5, region transfer)
  needs a cryptographic one (review P22R-1).
* 25.2: the physics world's frame is a level-0 chunk; `write_root_transforms` rebases poses into
  the sim store's coordinates and places framed objects in their frames.
* 4.3 rule 9 covers every physics worker: the fork's helpers adopt the stepping thread's
  floating-point control register for every job (D2 review fix).

## Not in this package

* The session protocol's use of images and transfers (keyframe rings, rollback scheduling, the
  confirmation of check hashes) is P2-3's.
* Regions, multi-world scheduling and handoff are phase 3 (`pe-worlds`).
* Moving the phase-1 spikes onto `pe-physics` is not needed for this package; they keep running
  against the fork directly and still pass.
* Per-world length units would need a fork change (Box3D reads them from a global).
* Double-precision positions (P22-8, the D58 follow-up), and compacting the rollback geometry
  registry (P22-12, P2-3).

## Reproduce

```bash
scripts/run_p2_2_tests.sh --linux   # Windows debug + release, Linux musl debug + release (WSL)
cargo test -p pe-physics --release  # one run
cargo test -p box3d-rust --release --lib -- snapshot_hostile record_tests   # the fork's hostile-image tests
```
