# P2-2: physics wrapper (`pe-physics`)

Author: @vdw-physics. Package P2-2 of design 23.4. Branch `p2/p2-2`, with main merged at b5f3013
(P2-1 final, D2's parallel Box3D and everything since). Reviewer: @design-reviewer; the first
review's findings (P22-1 to P22-13) are answered in "Review fixes" below. Docket:
@item:primordial-engine/p2-2-pe-physics-the-engine-s-only-door-to-the-bo.

Tags: **[V]** tested here, **[I]** inferred or proposed.

## What is built

`crates/pe-physics` depends on the box3d fork, `pe-core` and `pe-sim`. It is the engine's only way
to reach Box3D.

| Item | What it does |
|---|---|
| `PhysicsConfig`, `engine_world_def()` | The engine world: Box3D defaults plus scale-dependent contact recycling (HarmonicMean, user ruling 2026-10-30), gravity, sleep, and the **worker-thread count** (D2; results are identical for every count; clamped to 1..=32). `PhysicsConfig::upstream()` turns recycling scaling off, for comparison with Box3D C's goldens only |
| `BodyDesc`, `ShapeDesc`, `Geometry`, `limits` | Plain engine descriptions: static, kinematic or dynamic; sphere, capsule, box, or the convex hull of points; density (Box3D's default is water's, 1000 kg/m^3) and surface material. **Validated and bounded before anything reaches Box3D** (4.3 rule 6): no NaN or infinity, and magnitudes within `limits` (positions 1e6 m, sizes 1e-4 to 1e4 m, density 1e5, speeds 1e4, pushes 1e9), so no finite input overflows inside Box3D. Rotations within 1% of unit length are normalized (correctly rounded `sqrt` and division, so every peer gets the same bits); Box3D itself asserts about 2e-6. Positions and points are `f64` |
| `PhysicsWorld` | One Box3D world. `add_body` / `remove_body` bind bodies to sim objects by `pe_core::ObjectId`; the binding table is a sorted map, so iteration is in id order on every peer. `pose`, `velocity`, `is_awake`. `step(dt)` refuses dt outside (0, 1] s. A refused call changes nothing |
| `Push` | Interactions as impulses (through the centre or at a point), angular impulses, forces, forces at a point and torques. Forces and torques last one step |
| `JointDesc`, `JointKey`, `JointKind`, `GrabSpring` | Spherical, weld and distance joints between two objects or an object and the world, and a **grab** (Box3D's motor joint, D72): force and torque caps, steered by `set_grab_velocity` (which wakes both bodies), with anchors and an optional spring that pulls the object to the hand. Keys are **the caller's** (`JointKey(u64)`, for example the interaction's id) and part of the state: a rollback restores the table with the world. The table keeps each joint's kind; `set_grab_velocity` on anything but a grab is `NotAGrab`. A joint from a body to itself is refused. Removing a body forgets its joints |
| `WorldImage` | `save_image` / `restore_image`: rollback, in place. Box3D's world serialization plus the binding and joint tables; geometry is interned once per world. An image whose world settings differ from the world's is refused before anything is written. A restore that fails after Box3D started writing **poisons** the world (every call refused) until a good image is restored |
| `Transfer`, `check_hash` | `save_transfer` / `PhysicsWorld::from_transfer`: a fresh world for a joining peer. Saved against a fresh geometry registry (exactly the geometry it uses, in shape order) with Box3D's debug-only validation flag cleared, so its bytes depend only on the physics state. `check_hash` (XXH64 of those bytes) is the **full-state check hash** of design 9.1: equal on every honest peer whatever its save history, profile or OS. `from_transfer` restores only if the hash equals the confirmed one, because the donor is untrusted (see P22-1) |
| `state_hash`, `box3d_hash` | `box3d_hash` is Box3D's own hash (body transforms and velocities, the C reference's), which the spike traces record. `state_hash` is the per-frame hash: XXH64 over it plus the binding and joint tables. It does not cover sleep timers, contact caches, warm starts, islands or settings; `check_hash` does |
| `write_root_transforms` | Copies body poses into `pe_sim::Transform`. The physics world's origin is a level-0 chunk. Root objects get their position rebased into their own chunk; objects under a frame get the local transform that places them there in the frame's current world transform, ancestors first (depth, then id). Local scale is kept (physics does not read it; colliders come from `BodyDesc`). All or nothing: on any refusal the transforms already written are put back |
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

__EVIDENCE__

## Review fixes

The first review (@design-reviewer at 52a9ca9, `design-reviewer/review-p2-2.md`) found that world
images reach joining peers from untrusted donors and that the restore path trusted them. The fixes
are in fcedd22; tags as above.

| Finding | Fix | Evidence |
|---|---|---|
| **P22-1** (High) a crafted image aborts a joiner: counts size `reserve` unchecked; indices never cross-checked | Fork: `SnapReader::count` reads every element count and checks it against the bytes left, with a lower bound on each record's size, before any allocation or loop (sensors, islands, the four solver-set arrays, visitors, contact and joint links, materials, manifolds, triangle caches, graph-colour joints and contacts; the rest were already checked). For semantic validity a joiner restores only a `Transfer` whose check hash equals the confirmed one (P22-5), so a donor's bytes are trusted only once honest peers have hashed the same state. Doc comment corrected | [V] fork test `hostile_counts_never_panic_or_abort` overwrites every aligned word of a real image (bodies, hulls, contacts, islands, a joint, a sensor) with `i32::MAX`, `i32::MAX/16`, 2^24 and -1: no panic, no abort. **Negative control**: with the old `sensor_count` read, the test process dies with "memory allocation of 171798691760 bytes failed". [V] pe-physics `hostile_transfers`: any changed byte fails the hash, so Box3D never reads it |
| **P22-2** config adopted unvalidated (NaN, `restitution_iterations` unclamped, unknown combine) | Fork: the config read refuses restitution iterations outside 0..=63, any non-finite setting and an unknown combine byte. `world_settings` / `image_world_settings` give a world's and an image's settings bytes (everything but the step state and the capacity high-water marks). Both restores compare them with the world's own settings **before writing**; `from_transfer` builds its world from the caller's config, so `config()` is right by construction | [V] fork tests `image_settings_match_world_settings` (recycling on and off) and `image_settings_out_of_range_refused` (i32::MAX and -1 iterations, NaN gravity, infinite hertz, combine 7). [V] `foreign_and_corrupt_images`: an engine world refuses an upstream world's image, and is unchanged and not poisoned. T4 now restores each mode with its own config (the old test used the engine config for both) |
| **P22-3** table invariants unchecked | Reading a `WorldImage` checks: object ids sorted, unique, not `NONE`; joint keys sorted and unique; no Box3D body or joint id used twice; every id in world 0; the anchor not a bound body; joint kinds known. `next_joint` is gone (caller keys, P22-4), so it cannot overflow | [V] `hostile_transfers`: a duplicated Box3D body id, swapped objects, joint kind 9 and a joint count of u32::MAX are refused |
| **P22-4** `set_grab_velocity` panics on non-grab keys; keys reissued after rollback | The joint table and images keep each joint's `JointKind`; `set_grab_velocity` returns `NotAGrab` / `UnknownJoint`. Keys are chosen by the caller from deterministic state, like ObjectId, and belong to the state: after a rollback a key names whatever joint the restored state has under it | [V] `grabs`, `images_and_transfers_round_trip_with_joints` (a joint added after the save is unknown after the restore; a key in use is `JointExists`) |
| **P22-5** state hash pose-only; image bytes not comparable between peers | `state_hash` documented as the per-frame hash. New `check_hash` over `Transfer` bytes: fresh registry in shape order, validation flag cleared, geometry content and tables included | [V] T4: the donor (which saved an image every frame) and a second peer (which never saved one) give the same check hash at frame 300 (`fc3999792b45a32f` engine, `27d8dd7d28d065de` upstream), equal at 1/3/8 workers, in debug and release, on Windows and Linux (RESULT `transfer` lines, compared across all four logs). `check_hash_is_canonical`: equal after different save histories, different for a different state |
| **P22-6** framed children never synced; half-writes | Framed objects are placed in their frame (ancestors first); every write is undone if any is refused | [V] `poses_reach_the_sim_store`: a bound frame (scale 0.5), its bound child and grandchild (local scale 3) end within 1e-5 m and 1e-5 rotation of their bodies; a body whose chunk would pass the sim store's limit refuses the write, and the earlier write is put back |
| **P22-7** validation looser than Box3D's asserts | Rotations normalized; magnitudes bounded (`limits`); same-body joints refused; `step(dt)` and gravity validated; joint anchors, lengths and caps bounded; refusals resolve every bound end before creating the world anchor | [V] `refusals_change_nothing` (radius 1e30, position 1e38, density 1e30, velocity 1e30, force 1e30, point 1e7, same-body spherical and grab, far anchor, dt 0/-1/NaN/2, NaN gravity: all refused, the world unchanged, no anchor created); `rotations_are_normalized` (a 0.8% long quaternion becomes unit to 1e-6 and steps in debug) |
| **P22-8** f32 creation positions and push points | `BodyDesc.position` and push points are `[f64; 3]`. Note: our build has the fork's `double-precision` feature **off**, so Box3D's `Pos` is `f32` and this change only prepares the API. Turning double precision on changes the snapshot layout hash and every record; that is an R16 decision for @coordinator-opus [I] | [V] records unchanged (the spike's positions are computed in f32 and widened) |
| **P22-9** grab velocity-only, no wake | Grab anchors (`anchor_a` on the hand, `anchor_b` on the object) and an optional `GrabSpring` (Box3D's motor spring, linear and angular); `set_grab_velocity` wakes both bodies. D72 unchanged: claims are pe-session's | [V] `grabs`: a sleeping ball wakes; a 0.52 kg ball held by a kinematic hand moving at 1 m/s under gravity is within 1e-4 m of the hand's anchor after 3 s |
| **P22-10** `raw_mut` breaks invariants | Behind feature `raw` (the crate's own tests enable it) | [V] the library builds and lints without it |
| **P22-11** process-wide length units | `new` and `step` refuse units other than metres (`LengthUnits`) | [I] not exercised: the recording player is the only code that changes them |
| **P22-12** the geometry registry only grows | Transfers carry only the geometry they use (fresh registry), so joiners never receive rolled-back hulls. The per-world rollback registry still only grows; compacting it (rebuild from a fresh save at a keyframe) is left to P2-3's keyframe ring [I] | [V] T4 transfer bytes |
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
* Double-precision positions (P22-8), and compacting the rollback geometry registry (P22-12).

## Reproduce

```bash
scripts/run_p2_2_tests.sh --linux   # Windows debug + release, Linux musl debug + release (WSL)
cargo test -p pe-physics --release  # one run
cargo test -p box3d-rust --release --lib snapshot_hostile   # the fork's hostile-image tests
```
