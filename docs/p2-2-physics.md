# P2-2: physics wrapper (`pe-physics`)

Author: @vdw-physics. Package P2-2 of design 23.4. Branch `p2/p2-2`: main at b496bfa (P2-1 final)
plus D2's parallel Box3D (`physics/parallel` 3d71a75, now on main as 94bd341). Reviewer:
@design-reviewer. Docket: @item:primordial-engine/p2-2-pe-physics-the-engine-s-only-door-to-the-bo.

Tags: **[V]** tested here, **[I]** inferred or proposed.

## What is built

`crates/pe-physics` depends on the box3d fork, `pe-core` and `pe-sim`. It is the engine's only way
to reach Box3D.

| Item | What it does |
|---|---|
| `PhysicsConfig`, `engine_world_def()` | The engine world: Box3D defaults plus scale-dependent contact recycling (HarmonicMean, user ruling 2026-10-30), gravity, sleep, and the **worker-thread count** (D2; results are identical for every count). `PhysicsConfig::upstream()` turns recycling scaling off, for comparison with Box3D C's goldens only |
| `BodyDesc`, `ShapeDesc`, `Geometry` | Plain engine descriptions: static, kinematic or dynamic; sphere, capsule, box, or the convex hull of points; density and surface material. **Validated before anything reaches Box3D**: NaN and infinities, non-unit rotations, non-positive sizes and degenerate hulls are refused (4.3 rule 6) |
| `PhysicsWorld` | One Box3D world. `add_body` / `remove_body` bind bodies to sim objects by `pe_core::ObjectId`; the binding table is a sorted map, so iteration is in id order on every peer. `pose`, `velocity`, `is_awake`. A refused call changes nothing |
| `Push` | Interactions as impulses (through the centre or at a point), angular impulses, forces, forces at a point and torques. Forces and torques last one step |
| `JointDesc`, `JointKey` | Spherical, weld and distance joints between two objects or an object and the world, and a **grab** (Box3D's motor joint, D72) with a force and torque cap, steered by `set_grab_velocity`. Keys are issued in order and never reused. Removing a body forgets its joints |
| `WorldImage`, `GeometryBlobs` | `save_image`, `restore_image` (in place, for rollback) and `PhysicsWorld::from_image` (a fresh world, for late join and state transfer). An image is Box3D's world serialization plus the binding and joint tables; geometry is interned once per world and travels separately as `GeometryBlobs`. Both have canonical little-endian bytes; `from_bytes` checks the framing, that the tables are sorted and unique, and that every bound body and joint exists after the restore |
| `state_hash`, `box3d_hash` | `box3d_hash` is Box3D's own hash (body transforms and velocities, the C reference's), which the spike traces record. `state_hash` is XXH64 (pe-core) over it plus the binding and joint tables |
| `write_root_transforms` | Copies body poses into `pe_sim::Transform` for objects that are roots of the transform hierarchy. The physics world's origin is a level-0 chunk; positions are rebased into their own chunks. Objects under a transformed parent are left to the parent |
| `raw` / `raw_mut` | The Box3D world, for test scenes (the determinism scenes build through it) and tools. Bodies made there are not bound |

Several worlds per process work: each `PhysicsWorld` owns its Box3D world and its worker threads.
Box3D's length units are process-wide (as in C), so all worlds in a process share them; the
engine uses metres.

## Evidence

The done criterion is that the phase-1 spike's goldens, 600-frame trace and rollback T1-T5
reproduce through the wrapper, release and debug, on Windows and Linux, with the digests
committed in `spikes/out/vdw/`. `tests/common/mod.rs` rebuilds the spike's pile scene, its
"soonest wins" claim rule and its `RollbackSim` on pe-physics; every check runs in the engine
configuration (`vdw`) and the upstream one (`off`), at 1, 3 and 8 worker threads.

__EVIDENCE__

## Choices the design text should pick up [I]

For @engine-architect, who owns design.md:
* 23.4 P2-2: the engine reaches Box3D only through `pe-physics`; `raw_mut` is for tests and tools.
* 7 (interactions): pushes are impulses, forces and torques at the start of a frame; grabs are a
  motor joint with force and torque caps (D72).
* 6.1: a physics image is Box3D's world serialization plus the binding tables; geometry is
  interned per world and sent once (`GeometryBlobs`), not per snapshot.
* 25.2: the physics world's frame is a level-0 chunk; `write_root_transforms` rebases poses into
  the sim store's coordinates.
* 4.3 rule 9 covers every physics worker: the fork's helpers adopt the stepping thread's
  floating-point control register for every job (D2 review fix).

## Not in this package

* The session protocol's use of images (keyframe rings, rollback scheduling) is P2-3's.
* Regions, multi-world scheduling and handoff are phase 3 (`pe-worlds`).
* Moving the phase-1 spikes onto `pe-physics` is not needed for this package; they keep running
  against the fork directly and still pass.
* Per-world length units would need a fork change (Box3D reads them from a global).

## Reproduce

```bash
scripts/run_p2_2_tests.sh --linux   # Windows debug + release, Linux musl debug + release (WSL)
cargo test -p pe-physics --release  # one run
```
