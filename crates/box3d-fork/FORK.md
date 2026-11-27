# box3d-fork: Primordial Engine fork of box3d-rust 0.5.0

* **Upstream:** [box3d-rust](https://github.com/larsbrubaker/box3d-rust) 0.5.0 from crates.io
  (git `f2ca61e39b65`), a pure-Rust port of Erin Catto's Box3D. Its CHANGELOG says 0.5.0 is synced to
  Box3D C `efca0c1`. Its README still names the older `3fc20f5` pin.
* **License:** MIT. See [LICENSE](LICENSE), unchanged. The SPDX headers are kept on every file.
* **How it is wired:** the workspace `Cargo.toml` has
  `[patch.crates-io] box3d-rust = { path = "crates/box3d-fork" }`, so `box3d-rust = "=0.5.0"`
  resolves here. The package name and version stay the same.
* **Vendored verbatim:** commit `5645a0e` holds the published sources byte-for-byte. Only the
  manifest was rewritten: no workspace, demo or bench sections, and the lints were inlined. The
  original manifest is kept as `Cargo.upstream.toml`. Line endings were then normalized to LF.
  Git stores LF, so this changes nothing in the repository.
* **Never formatted:** `rustfmt.toml` here sets `disable_all_formatting`, which overrides the
  workspace's for every file of the crate. A reformat would bury the upstream diff, so
  `cargo fmt` over the workspace leaves the fork as it is.

## Changes

Every change is marked in the source with a `Primordial Engine fork` comment.

| File | Change |
| --- | --- |
| `src/recycle_scale.rs` (new) | `ContactRecycleScaling` parameters, `RecycleLengthCombine` (HarmonicMean by default), `RecyclePairInput`, the `ContactRecycleRadiusFn` hook and the default formula |
| `src/lib.rs` | `pub mod recycle_scale` |
| `src/types/world.rs` | `WorldDef::contact_recycle_scaling`, disabled by default (upstream behavior; Primordial Engine worlds turn it on in `pe_sim_core::engine_world_def`) |
| `src/world/mod.rs` | `World::contact_recycle_scaling` and `World::contact_recycle_radius_fcn` |
| `src/world/api.rs` | `world_set/get_contact_recycle_scaling` and `world_set_contact_recycle_radius_fn` |
| `src/contact/collide.rs` | When scaling is on, `collide_task` computes a per-pair tolerance inside the recycle branch. When it is off, the upstream path runs unchanged |
| `src/recording/snapshot/pods.rs` | World-config flag bit `0x20` plus 13 bytes of parameters, written only when scaling is on |
| `src/recycle_scale_tests.rs` (new) | Tests for the formula, the hook, the 1 cm ledge regression, and the snapshot and recording round trips |
| `Cargo.toml` | Fork metadata; see the header comment |
| `src/world/validate.rs`, `src/island.rs` | `World::validate_solver_sets` and `validate_island` return at once in release builds. Upstream ran their loops in release with every assert compiled away. C compiles them out unless `B3_VALIDATE` is set. Debug builds still validate. Found by @world-chunks |
| `src/worker_pool.rs` (new) | D2: the worlds' worker threads (`WorkerPool`), `BlockCounter`, `SharedMut`, `SharedSlice`; pure Rust, no dependencies. None on wasm32 or with 1 worker. Helpers adopt the caller's FP control register (MXCSR/FPCR, inline asm) for each job |
| `src/types/world.rs`, `src/world/mod.rs`, `src/world/api.rs` | D2: `WorldDef::worker_count` (default 1); `World::new` applies it; `world_set_worker_count` also (re)creates the pool |
| `src/solver/stages.rs` (new) | D2: C's `b3SolverTask` stage machine: worker 0 publishes stages, workers claim blocks with a stage-tagged compare-exchange; a panicking block is counted and re-raised on worker 0 |
| `src/solver/solve.rs`, `src/solver/integrate.rs` | D2: `solve_stages` runs prepare, integrate, warm start, solve, relax, restitution and store as stages, serially or on the workers; joints and overflow stay on worker 0 in the serial order. Stage timers |
| `src/contact_solver/wide/*` | D2: `*_range` block versions of the wide kernels over raw views (`view.rs`, new); prepare writes preallocated slots (`allocate_contacts_*`); gather/scatter through `StatesView` |
| `src/contact/collide.rs`, `update.rs`, `mesh_contact.rs`, `mod.rs` | D2: the narrow phase takes a read-only `CollideEnv` plus the contact it updates instead of `&mut World`, and runs in blocks on the workers; no per-contact `Shape` clones. SAT and manifold counters are summed over workers (C scales only worker 0's by `satMultiplier`) |
| `src/broad_phase/pairs.rs`, `src/parallel_for.rs` | D2: pair finding runs its blocks on the workers (serially if a custom filter is set); the block partition is the single-worker one at every worker count |
| `src/stage_timer.rs` (new), `Cargo.toml` | Feature `stage-profile`: fills `world::Profile` with stage timings (off by default) |
| `src/world/step.rs` | Stage timers |
| `src/parallel_tests.rs` (new) | D2: determinism scenes identical at 1/2/3/4/8 workers and with the worker count changed mid-run |
| `src/solver/solve.rs` | D2: every worker's hit-event bit set is sized and cleared at each solve, as C does for each task context; only worker 0's was, so with hit events on and several workers a helper's first hit bit panicked. `parallel_tests.rs`: hit events identical at every worker count |
| `src/lib.rs`, `src/contact/mesh_cache.rs` | `set_warning_sink`: the narrow phase's complex-mesh warning goes to a sink an application sets (pe-app's `--log` writes it to its log as well as the console), else to standard error as upstream. Not part of the simulation |
| `src/solver/swept.rs` (new) | D3: swept bodies (pe-world's bullets, world-model.md 3.2). Inside each sub-step, on worker 0: a search before `integrate positions` (each swept body's path box, widened by how far any awake body can have moved), then conservative advancement on GJK, stepped by a lower bound (a separating plane's gap) over the larger of two closing bounds (isotropic, and along the contact normal), earliest impact first, a two-body impulse at the contact point with the pair's mixed friction and restitution, a search again after a deflection or a separation unless one box searched holds the new path (and of every swept body, the earlier boxes dropped, when a body's reach grows), and a candidate a search turned away (the two paths' boxes apart) looked at again when either body's piece changes. Mesh triangles are partners (part 2 A): one pair per triangle whose box meets the bullet's path in the mesh body's frame over the mesh's own motion, as `get_mesh_triangle` gives it; one-sided (a bullet whose centre starts a piece behind the plane is left alone); the face normal at a smooth feature (`smooth_feature`, the mesh's edge flags); a triangle's own sphere and its plane in the zero-query culls. The sweep's start rotates (part 2 B): each sub-step starts just after the pair whose sweep ran out of budget, a step's first at its step index. A touching pair is swept on rounded cores moved inward by a share of each shape's inner size: to a quarter slop apart, to touching, then two bands below their depth at each graze or hit, never past a floor short of crossing the thinner core; at the floor, an unprovable pair keeps only its motion apart along the normal. A pair that can't close a slop is left to the contact solver: over the rest of the sub-step if a touching contact joins its shapes this step, else to the step's end, since a pair that meets during a step has no contact until the next (part 2 E; so too the floor's unprovable pair). A held pair's manifold is the step's start's, so a hull spinning about 0.9 rad a step meets it on stale anchors (0 crossings in part 2 E's 42 runs of shape, spin and rate). One whose bounding spheres never meet is proven apart without a query. `sweeps_against` names the partner shape types, with no wildcard arm, for pe-physics's tripwire. A budget of distance queries per sub-step (1024), past which every body connected to an unproven pair is held. Sleeping bodies stay put, take a pending velocity that later hits see, and wake with it at the step's end |
| `src/body/types.rs`, `src/types/body.rs`, `src/body/lifecycle.rs`, `src/body/api.rs` | D3: body flag `SWEPT` (0x0002_0000, in the snapshot's flags word), `BodyDef::swept`, `body_set_swept`, `body_is_swept`; create and destroy keep `World::swept_bodies` |
| `src/world/mod.rs`, `src/solver/mod.rs`, `src/solver/solve.rs`, `src/solver/integrate.rs` | D3: `World::swept_bodies` (body ids in order, rebuilt from the flags at a snapshot restore), `swept_iteration_budget` (`SWEPT_ITERATION_BUDGET`), `swept_counters`; the pass's two calls in the sub-step loop and its finish after the step; a swept body skips Box3D's post-step continuous solve and has its boxes updated in `finalize_bodies` |
| `src/recording/snapshot/mod.rs`, `src/recording/writers.rs` | D3: a restore rebuilds the swept list; a body definition's `swept` rides in bit 1 of its bullet byte, so one without it writes the upstream bytes. `body_set_swept` is not captured as a recording op |
| `src/swept_tests.rs` (new) | D3: thin walls at up to 400 m/s, restitution, balls head-on and off centre, sleeping boxes and stacks, a heavy bullet into a light sleeper, momentum, tumbling capsule and box bullets, small spinning hulls slowed by their hits (part 2 E), resting and sliding bullets that raise no sweep event, a turning kinematic paddle, the budget's hold, worker counts and snapshot replay |
| `src/mesh/mass.rs` (new), `src/mesh/mod.rs`, `src/shape/dispatch.rs` | CCD part 2 C: a closed mesh's mass (world-model.md 3.2). `compute_shape_mass` gives a mesh shape the mass of the solid it encloses, where upstream gives none. Closedness is judged on the built mesh's positions as given, before the scale: every edge in exactly two triangles, once each way, positions compared bit for bit (−0 as +0, by sorted position ids and sorted 64-bit edges), triangles with two equal positions left out. Then one signed tetrahedron per built triangle, in its order, from the centre of the scaled positions' bounding box, summed in `f64` and rounded to `f32` once, a mirroring scale's winding read back. An open mesh, one wound inward and one enclosing under 10⁻⁹ m³ keep upstream's no mass. `is_closed_mesh` and `compute_closed_mesh_mass` are public. A body without a mesh keeps a bit-identical mass |
| `src/contact/mesh_cache.rs`, `src/contact/mesh_contact.rs` | CCD part 2 C: a moving mesh's triangles. Upstream's `refresh_cache` finds a mesh contact's triangles again only when the other shape's world bounds leave the cached box, which holds only while the mesh is static. A mesh on a body that isn't static has them found again at every update; static meshes keep upstream's rule. Each triangle's cache carries over, as upstream's refresh does. Without it, a dynamic mesh box whose contact began 0.1 m above a box floor kept an empty list and fell through |
| `src/shape/lifecycle.rs`, `src/shape/api.rs` | `shape_get_mesh`, C's `b3Shape_GetMesh`, which the port lacked: the shape's mesh and its scale |
| `src/mesh_tests.rs` | CCD part 2 C: closed-mesh mass (a box against Box3D's box hull, far from its origin, scaled and mirrored; open, inward, shared and tiny meshes; split vertices; two equal positions; a scale that rounds two positions into one) |
| `src/math_functions/types.rs` | E2: `MIN_SCALE` (`B3_MIN_SCALE`), the floor `safe_scale` raises a mesh's scale axis to, is 1e-6 where upstream has 0.01. Box3D applies a mesh's scale exactly (the narrow phase gets `scale * vertex`; triangle queries and rays use `1 / scale`), so 0.01 was no numeric limit: on a mesh flattened on one axis, contacts were right to 1e-38 and rays to 1e-35 from 1 km, and a uniform scale to 1e-8 neither fell through nor gave a NaN (E2 spike runs f1, f3 and f7). Any scale of 0.01 or more is unchanged, so every world that built before steps bit-identically |

### Release validators: measured effect

Release, the engine pile scene, 5 interleaved runs of `pe-spike-physics bench`, medians. Raw data is in
`spikes/out/fork/validate-gate-ab-win-release.txt` at the tag `archive/v0-prototype`: the M0 archive removed that
folder from main, along with the spike (now `archive/spikes/physics`). pe-physics's `examples/records.rs bench` is
its successor.

| Bodies | Scene build, before → after | Step, before → after |
| --- | --- | --- |
| 256 | 0.78 → 0.44 ms | 462 → 481 µs (noise) |
| 1024 | 4.0 → 1.3 ms | 2144 → 1859 µs (−13%) |
| 2048 | 15.3 → 2.3 ms | 7695 → 5242 µs (−32%) |
| 4096 | 63.5 → 4.2 ms | 13951 → 10077 µs (−28%) |

Before the change, build time was quadratic in body count, because every `create_body` walked
every body. After it, build time is linear. The simulation is unchanged: the C goldens
(`--upstream`), the engine goldens, the 600-frame traces and snapshot-image bytes in both modes,
and rollback T1-T5 are byte-identical to the committed outputs in release, opt-level 1,
opt-level s and target-cpu=native. Debug runs the same code as before.

## Invariants

* A world with no swept body steps bit-identically to one built without D3: nothing in `solver/swept.rs` runs.
* A world with no mesh shape on a moving body steps bit-identically to one built without CCD part 2 C, and a body
  without a mesh has a bit-identical mass.
* Results do not depend on `worker_count` (D2). Every parallel stage writes each output from
  exactly one block, and blocks are combined in a fixed order or with order-independent
  operations (sorted pair keys, ORed bit sets, integer sums). See `docs/parallel-physics.md`.

* With scaling off, the simulation is bit-identical to the published crate. This was checked
  against the 4 C goldens, the 600-frame trace, the snapshot-image bytes and the crate's own test
  suite. See `docs/vdw-simulation.md`.
* The radius hook is a plain `fn` pointer. It is not snapshotted, the same as the other Box3D
  callbacks. The parameters are snapshotted.
* Recordings replay the scaling parameters of the recorded world, because a replay is seeded
  from a frame-0 world snapshot and keyframes are snapshots too (test
  `recording_replays_scaling_from_world_def`). A `world_set_contact_recycle_scaling` call made
  during a recording is not captured as an op.
