# Van der Waals simulation: a scale-dependent contact-recycling radius

Date: 2026-10-30. Author: @vdw-physics. Branch `vdw/contact-recycling`. Docket item:
@item:primordial-engine/van-der-waals-simulation-scale-dependent-contact.

Tags: **[V]** verified by running it here (raw output committed under `spikes/out/vdw/`);
**[S]** verified by reading source; **[I]** inferred.

## 1. Summary

* **What upstream does [S].** The recycling radius is one world-wide length,
  `contact_recycle_distance` = 0.05 m. It is a manifold-reuse tolerance on how far the pair's
  relative pose has drifted since the narrow phase last ran (section 2).
* **What we built [V].** The radius can now depend on the pair:
  `r = clamp(0.1 * min(innerRadius_A, innerRadius_B), 5 mm, 0.5 m)`. It sits behind
  `WorldDef::contact_recycle_scaling`, which is off by default, and the formula can be replaced
  for R15. It lives in our fork, `crates/box3d-fork`, through a `[patch.crates-io]` entry.
* **Toggle off is bit-identical to the unmodified crate [V].** Checks that passed: the 4 C goldens;
  the 600-frame trace (digest `7903109aab28ed9b`) in release, debug, opt-level 1, opt-level s and
  target-cpu=native; the snapshot-image bytes (digest `35af2efac5bfeb27`, equal to a build against
  the crates.io crate); rollback T1-T5; and all 547 of the crate's own tests.
* **Toggle on is deterministic [V].** The trace (digest `a9041fec18d56c02`) is identical across
  all 5 builds. Goldens and image bytes match between debug and release. Rollback T1-T5 pass in
  debug and release, including T4, where the toggle travels inside the snapshot to a fresh default
  world. Every one of the 120 benchmark configurations reproduced its final hash on a re-run.
* **Headline numbers [V]:**
  * **Small objects.** A 1 cm cube sliding off a ledge rides on stale contact points
    **3.83 cube-widths** past the edge with the fixed radius, against 0.29 with no recycling. The
    scaled radius gives **0.34**. At 10 cm: fixed 0.44, scaled 0.30, no recycling 0.30.
  * **Large objects.** In an 864-box pile, the recycle hit rate rises from 56% to 71% at 10 m and
    from 35% to 61% at 30 m. Step time falls **5% at 10 m (0.485 to 0.461 ms) and 10% at 30 m
    (0.635 to 0.573 ms)**.
  * **1 m objects.** The scaled radius was calibrated to match fixed here: unit-cube scenes give
    identical hashes in both modes.
  * **Structures.** Stacks, pyramids and a 48-plank tower from 10 cm to 30 m: no collapse in any
    recycling mode, and sleep onset within about ±10% of fixed.
* **What it does not fix [V].** At 1 cm and below, every stack, pyramid and tower collapses in
  every mode, including with recycling off. Box3D's other absolute tolerances dominate there:
  `linear_slop` 5 mm, speculative distance 2 cm, the 0.05 m/s sleep threshold, and AABB margins
  that make a 1 mm pile collide all-pairs. The engine-wide fix is
  `set_length_units_per_meter`, not recycling.
* **Decisions for the user:** see section 7.

## 2. What upstream does (step 1)

**Where it lives [S].** box3d-rust `src/contact/collide.rs`, `collide_task`, is a faithful port of
Box3D C `src/physics_world.c`, `b3CollideTask`. The fixed distance is
`World::contact_recycle_distance`. Its default is `constants::contact_recycle_distance()`, which is
`10 * linear_slop` = **0.05 m** (C `B3_CONTACT_RECYCLE_DISTANCE`). You can change it at run time with
`world_set_contact_recycle_distance` (C `b3World_SetContactRecycleDistance`). It is a single
world-wide length. It is also serialized in the world snapshot.

**What it controls [S].** Each step, for every awake contact whose fat AABBs still overlap,
`collide_task` decides whether to skip the narrow phase and reuse the manifold built on an earlier
frame. The pair needs the `RECYCLE` flag (both bodies have `enable_contact_recycling`, and
speculative contact is on). Neither body may be `IS_FAST`, and the cached relative pose must be
valid. The pair is then recycled when both of these hold:

1. **Angular gate (fixed).** `min(dot(qA, qA0)^2, dot(qB, qB0)^2) > 0.99240388`. This is
   cos^2 of half the angle, so each body may turn at most about 10 degrees since the manifold was
   built.
2. **Translational and arc test (the tunable one).** Let `d = |p_rel - p_rel0|`. This is the drift
   of B's pose relative to A since the manifold was built. It accumulates across recycled frames,
   because the cache is only refreshed when the narrow phase runs. The test needs
   `d < tolerance` and also `2 * |modcross(|q_r.v|, max_extent)| < tolerance - d`. The second part
   is conservative advancement: it charges the rotation arc of the larger body's `max_extent`
   against the slack that remains.

   `tolerance` is `contact_recycle_distance` for touching pairs, and
   `min(contact_recycle_distance, speculative_distance = 4 * linear_slop = 0.02 m)` for pairs that
   are not touching.

When a pair is recycled, the anchors stay fixed in each body. Only each point's `separation` is
re-derived: `base_separation + dot(dp, normal)`. `normal_velocity` is zeroed and the point is
marked `persisted`.

**Classification.** It is a **manifold-reuse tolerance on accumulated anchor drift**. In other
words, it is an anchor-drift threshold measured on the relative pose. It is not the speculative
margin, although the speculative margin caps it for pairs that are not touching. The rotational
part is already partly scale-aware, because the arc uses `max_extent`. The translational part is
one absolute length for every pair. That is the "naive van der Waals" the user described.

**Upstream check [S].** box3d-rust 0.5.0's CHANGELOG says it is synced to Box3D C **`efca0c1`**.
Its README, and our `docs/spike-report.md` section 2, still say `3fc20f5`, which is stale. The
port's `collide_task` matches `efca0c1` line for line: the `isFast == false` gate,
`normalVelocity = 0`, and the gates and formulas above. `3fc20f5` still had
`(isFast == false || isMeshContact == false)`. Upstream `main` (`3362633`) has the same recycling
code as `efca0c1`.

## 3. Formula (step 3)

For each pair, when `WorldDef::contact_recycle_scaling.enabled`:

```text
L_i = BodySim::min_extent of body i        smallest shape extent ("inner radius")
L   = min(L_a, L_b)                        Min combine (default); a static side is ignored
r   = clamp(k * L, r_min, r_max)           k = 0.1, r_min = 0.005 m, r_max = 0.5 m
tolerance = r                              touching pair
tolerance = min(r, 0.02 m)                 non-touching pair (as upstream)
```

The angular gate and the arc test are unchanged. The arc test now spends its slack out of `r`.

**Why each piece:**

* **Inner radius (`min_extent`), not the bounding radius.** It is already stored on every
  `BodySim`. It is the same quantity Box3D uses for its continuous-collision "fast body" test, and
  it is the distance a contact point can slide before it falls off the body's narrowest feature. A
  plank's thickness, not its length, decides how far it can slide off an edge. The bounding radius
  (`max_extent`) is already used by the arc test.
* **Min combine.** Stale points are wrong by about the drift `d`. Measured against the smaller body,
  that error matters most: a pebble on a boulder must not slide a pebble-width on stale points.
  `HarmonicMean` (`2ab/(a+b)`, between min and 2*min) is available as an option. A static side is
  ignored (infinite size), so a box on the ground uses the box's own size.
* **k = 0.1.** This makes a 1 m cube (`min_extent` 0.5 m) get exactly upstream's 0.05 m. Box3D's
  constants are tuned for objects around 1 m, so behavior at that size does not change.
* **r_min = 0.005 m (one `linear_slop`).** Box3D cannot resolve contact geometry below the linear
  slop, so a tighter tolerance only adds narrow-phase cost.
* **r_max = 0.5 m.** This bounds the absolute anchor drift on very large bodies (L >= 5 m).
* **Determinism.** It uses only `*`, `+`, `/`, `min`, `max` and compares on `f32`, which are
  IEEE-754 exactly rounded. There is no new state per contact. The inputs are `min_extent`, which is
  already rollback state, and the parameters, which are in the snapshot.

**Override point (R15).** There are two levels:

* `ContactRecycleScaling { enabled, length_fraction, min_distance, max_distance, combine }` is the
  parameter override. You can set it in `WorldDef` or with `world_set_contact_recycle_scaling`.
* `world_set_contact_recycle_radius_fn(Some(f))` replaces the formula. `f` is a plain
  `fn(&RecyclePairInput, &ContactRecycleScaling) -> f32`, and it receives both lengths, both
  static flags and the base distance. It must be pure and deterministic. Like Box3D's other
  callbacks, it is not snapshotted, so you must re-register it on a fresh world. A return value of
  0, a negative value or NaN disables recycling for that pair.

**Snapshot and rollback.** The parameters are written after the world-config flags byte, and only
when the toggle is on (new flag bit `0x20`, 13 bytes). With the toggle off, the image bytes are
exactly upstream's. A late-joiner restore into a `World::new(&default_world_def())` picks the
toggle up from the image (rollback test T4 below).

## 4. Benchmark (step 4)

### 4.1 Method

`pe-spike-physics vdw-bench 1800 <out.jsonl>` and `pe-spike-physics vdw-perf 1200 5 <out.jsonl>`
(source: `spikes/physics/src/vdw_bench.rs`). The machine and toolchain are those in
`docs/spike-report.md` section 2: Ryzen 7 9700X, Windows 11, rustc 1.99.0, release build, serial
port. Each run is 60 Hz with 4 substeps.

* **Scales.** s = 1 mm, 1 cm, 10 cm, 1 m, 10 m and 30 m. s is the edge of the reference cube.
  Every length in a scene scales with s, but gravity stays at 10 m/s^2. Large scenes therefore
  evolve more slowly in body-lengths per second, since time scales as sqrt(s). That is real
  physics, not a benchmark artifact.
* **Scenes.**
  * **stack:** 10 cubes dropped with 0.1 s gaps.
  * **pyramid:** base 10, 55 boxes.
  * **pile:** 216 boxes, largest edges 0.25 to 2 s, aspect ratios 1 or 0.5, dropped into a bin.
  * **tower:** Jenga-style, 16 levels of 3 planks measuring 3 x 0.6 x 1 s.
  * **ledge:** 8 frictionless cubes sliding at 0.5 to 4 s per second off a platform edge.
  * **bigpile:** 864 boxes, used for timing.
* **Modes.**
  * **off:** recycling disabled. The narrow phase runs every frame, so this is the
    no-approximation reference.
  * **fixed:** upstream.
  * **vdw:** the scaled radius with default parameters.
  * **vdw-hm:** the same with the HarmonicMean combine.
* **Statistics.** The scenes are chaotic, so each `vdw-bench` configuration runs 5 seeds with
  slightly different initial conditions, and the tables show means over seeds. `vdw-perf` runs
  modes interleaved over 5 repeats and reports the median.
* **Metrics.**
  * **hit:** recycled contacts divided by awake contacts.
  * **sleep:** seconds until every body sleeps.
  * **collapse:** seeds in which any body ended more than s/2 from its ideal rest pose.
  * **overhang:** the cube centre's distance past the edge, in s, when it has dropped 0.05 s.
* **Raw output:** `spikes/out/vdw/vdw-bench-win-release.{txt,jsonl}` (the jsonl has one line per
  seed) and `spikes/out/vdw/vdw-perf-win-release.{txt,jsonl}`.

### 4.2 Small objects: stale support at a ledge [V]

This is the failure the fixed radius causes on small objects. A recycled manifold keeps its
anchors, so a cube that slides past an edge stays supported until its accumulated drift exceeds
the tolerance.

| s | off (reference) | fixed | vdw | fixed error | vdw error |
|---|---|---|---|---|---|
| 1 mm | 1.74 | never dropped in 30 s | 3.82 | (inf) | +2.08 |
| 1 cm | 0.29 | **3.83** | **0.34** | +3.54 | +0.05 |
| 10 cm | 0.30 | **0.44** | **0.30** | +0.14 | +0.002 |
| 1 m | 0.49 | 0.49 | 0.49 | +0.006 | +0.006 |
| 10 m | 1.01 | 1.01 | 1.02 | 0 | +0.011 |
| 30 m | 1.54 | 1.54 | 1.54 | 0 | 0 |

Overhang is the mean over 8 cubes and 5 seeds, in cube widths. Larger cubes overhang more in every
mode because they take sqrt(s) longer to tip at the same speed in s per second.

At 1 mm, the `r_min` = 5 mm clamp is 5 cube widths, so vdw does no better than fixed. Box3D itself
is out of range there: the no-recycling reference is already 1.7 widths. At 10 m the scaled radius
(0.5 m) allows some recycling that fixed (0.05 m) does not, and that costs 0.011 widths, about
11 cm.

### 4.3 Large objects: performance on an 864-box pile [V]

| s | mode | step ms (median of 5) | min of 5 | hit | mean contacts |
|---|---|---|---|---|---|
| 10 cm | off / fixed / vdw / vdw-hm | 0.123 / 0.101 / 0.102 / 0.097 | | 0 / 89.0 / 80.3 / 80.6% | 2589 / 2617 / 2642 / 2642 |
| 1 m | off / fixed / vdw / vdw-hm | 0.240 / 0.172 / 0.172 / 0.161 | | 0 / 72.7 / 66.1 / 66.2% | 1551 / 1496 / 1589 / 1584 |
| 10 m | off | 0.616 | 0.603 | 0% | 1249 |
| 10 m | fixed | 0.485 | 0.475 | 55.9% | 1222 |
| 10 m | **vdw** | **0.461 (-5.0%)** | 0.450 | **71.2%** | 1201 |
| 10 m | vdw-hm | 0.423 (-12.9%) | 0.417 | 71.0% | 1195 |
| 30 m | off | 0.698 | 0.675 | 0% | 1046 |
| 30 m | fixed | 0.635 | 0.615 | 35.1% | 1042 |
| 30 m | **vdw** | **0.573 (-9.8%)** | 0.556 | **60.5%** | 1021 |
| 30 m | vdw-hm | 0.565 (-11.0%) | 0.532 | 60.2% | 1011 |

At 10 m and 30 m the pile never sleeps within the 20 s run, so these are clean comparisons of the
cost per awake frame. At 10 cm and 1 m the bodies sleep at different times in each mode, so the
all-frames mean mixes cost with sleep timing. There vdw recycles less than fixed (80% against 89%
at 10 cm), because many pile boxes are smaller than 1 m and get a tighter radius, but the step
time does not measurably change. That the gain comes from recycled narrow-phase calls is **[I]**.
The hit-rate change is **[V]**.

### 4.4 Mixed pile, 216 boxes, stability [V]

| s | mode | sleep s (5 seeds) | hit | drift max (last third, in s) |
|---|---|---|---|---|
| 1 cm | off / fixed / vdw | 13.97 / 1.16 / 1.16 | 0 / 89.6 / 89.4% | 0 |
| 10 cm | off / fixed / vdw / vdw-hm | 1.41 / 1.49 / 1.67 / 1.67 | 0 / 86.5 / 75.3 / 75.7% | 0 |
| 1 m | off / fixed / vdw / vdw-hm | 8.00 / 5.18 / 5.48 / 6.55 | 0 / 69.4 / 60.5 / 63.1% | 0 |
| 10 m | off / fixed / vdw / vdw-hm | 24.6 (3 never) / 22.0 (1 never) / 16.4 (4 never) / 24.7 (2 never) | 0 / 51.2 / 67.8 / 68.3% | 0.16 / 0.93 / 1.00 / 0.08 |
| 30 m | off / fixed / vdw / vdw-hm | never / never / never / never | 0 / 45.2 / 67.7 / 69.7% | 0.72 / 0.70 / **0.36** / 0.40 |

No body tunnelled through the ground in any run. Sleep onset in a chaotic pile varies a lot from
seed to seed. Only differences larger than about 20% should be read as real, and none of the
vdw-versus-fixed differences above clearly are.

Recycling itself does clearly help stability: with recycling off, the 1 cm pile takes 14 s to
sleep against 1.2 s, and the 1 m pile 8.0 s against 5.2 s. This supports the user's
"dramatically more stable" observation **[V]**.

### 4.5 Structures: stack, pyramid, tower [V]

| s | scene | collapse (seeds) off / fixed / vdw | sleep s off / fixed / vdw | hit fixed / vdw |
|---|---|---|---|---|
| 10 cm | stack | **3/5** / 0/5 / 0/5 | 2.20 / 0.82 / 0.79 | 97.9 / 94.7% |
| 10 cm | pyramid | 0 / 0 / 0 | 0.90 / 0.87 / 0.87 | 98.0 / 96.2% |
| 10 cm | tower | 0 / 0 / 0 | 0.94 / 1.05 / 0.95 | 96.9 / 94.7% |
| 1 m | all three | 0 / 0 / 0 | identical in fixed and vdw (same final hash) | identical |
| 10 m | stack | 0 / 0 / 0 | 3.46 / 3.33 / 3.55 | 97.6 / 98.9% |
| 10 m | pyramid | 0 / 0 / 0 | 2.19 / 2.24 / 2.23 | 96.3 / 97.6% |
| 10 m | tower | 0 / 0 / 0 | 3.13 / 2.86 / 3.01 | 96.2 / 97.5% |
| 30 m | stack | 0 / 0 / 0 | 7.49 / 7.32 / 7.25 | 97.1 / 99.5% |
| 30 m | pyramid | 0 / 0 / 0 | 2.66 / 3.05 / 2.91 | 94.4 / 98.6% |
| 30 m | tower | 0 / 0 / 0 | 3.46 / 3.62 / 3.55 | 95.1 / 98.8% |
| 1 mm, 1 cm | all three | **5/5 in every mode** | | |

Sag (mean drop below the ideal rest pose) is the same in fixed and vdw to 3 significant figures:
0.0016 s at 10 m and 0.0005 s at 30 m for the stack. Resting structures drift far less than any
of these tolerances, so the radius rarely binds once a structure is at rest.

### 4.6 Box3D determinism scenes (C-golden scenes run with the toggle on) [V]

| Scene | sleep step: no recycling / fixed (= C golden) / vdw |
|---|---|
| falling ragdolls | 309 / 291 / 361 |
| wave pile | 279 / 272 / 307 |
| query spawn | 242 / 242 / 242 |
| mesh drop | 221 / 226 / 220 |

These are single chaotic samples. The ragdolls' limbs are thin capsules, so their small inner
radius gives them a 5 to 10 mm recycling radius. They recycle less and settle later. Whether that
is more or less accurate is **[I]** and not established by these numbers.

### 4.7 Cost of the toggle itself [V]

In cases where fixed and vdw give the same trajectory (unit-cube stack, pyramid and tower at 1 m,
which have identical final hashes), the step times agree within run-to-run noise. Pyramid at 1 m,
awake ms: 0.0448 fixed against 0.0449 vdw. The per-pair cost is a few flops, inside a branch that
only runs for pairs that are candidates for recycling.

## 5. Determinism and rollback (step 5) [V]

Raw output is in `spikes/out/vdw/`. Commands are in section 8.

| Check | Toggle off | Toggle on |
|---|---|---|
| 4 C goldens, release / debug | PASS / PASS (identical output) | values identical in release and debug (C goldens do not apply) |
| 600-frame trace digest | `7903109aab28ed9b`, equal to `spikes/out/trace-win-release-1.txt` byte for byte | `a9041fec18d56c02` |
| Trace identical across release, debug, opt-level=1, opt-level=s, target-cpu=native | yes (5/5) | yes (5/5) |
| Goldens with target-cpu=native | PASS | same values as release |
| Snapshot image bytes, 13 snapshots over 600 frames | release `35af2efac5bfeb27`, debug `b5dc824fbd65fcd0`, both equal to a build against the unmodified crates.io crate (`scratch/upstream-probe`) | release and debug each deterministic |
| Rollback T1-T5, release / debug | PASS / PASS | PASS / PASS. T4 restores into `World::new(&default_world_def())`, so the toggle came from the image |
| box3d-rust test suite (547 upstream + 5 new), release / debug | 552 / 552 pass | (the new tests cover the toggle) |
| `vdw-bench`: re-run seed 0, final world hash | 60/60 configs reproduce (modes off, fixed) | 60/60 configs reproduce (modes vdw, vdw-hm) |

**Release versus debug image bytes [S].** The image bytes differ between release and debug in
both modes, and the unmodified crate behaves the same way. The snapshot header carries a
`SNAP_FLAG_VALIDATION` bit that is set only in `debug_assertions` builds
(`recording/snapshot/mod.rs:106`). Images have the same length, and `deserialize_into_shell` does
not check that bit, so debug and release peers can still exchange snapshots. Simulation state is
identical.

**Not covered.** Linux, aarch64, macOS and wasm32 with the toggle on (the musl target is not
installed in the current toolchain), a second machine, and the recording/replay system.

## 6. Code map

| Where | What |
|---|---|
| [crates/box3d-fork/FORK.md](../crates/box3d-fork/FORK.md) | Provenance, license, every changed file |
| `crates/box3d-fork/src/recycle_scale.rs` | Parameters, combine modes, hook type, default formula |
| `crates/box3d-fork/src/contact/collide.rs` | Per-pair tolerance, used only when enabled |
| `crates/box3d-fork/src/recording/snapshot/pods.rs` | Snapshot flag `0x20` and the parameters |
| `crates/box3d-fork/src/recycle_scale_tests.rs` | Formula, hook, ledge regression, snapshot round trip |
| `spikes/physics/src/vdw_bench.rs` | Benchmark scenes and metrics |
| `spikes/physics/src/main.rs` | `--vdw` and `--no-recycle` flags, `images`, `vdw-bench`, `vdw-perf` |
| `spikes/sim-core/src/lib.rs` | `build_pile_scene_def` (pile scene with a caller-supplied `WorldDef`) |

## 7. Decisions for the user

1. **Default for Primordial Engine worlds.** The fork keeps the toggle off by default, so the port
   still matches the C goldens. I recommend turning it on in the engine's world definition.
   * **For:** it fixes the small-object stale-support artifact (1 to 10 cm) and gives 5 to 10%
     faster steps for 10 to 30 m objects. It is identical at 1 m, and no structure collapsed
     because of it.
   * **Against:** 10 cm to 1 m piles recycle 7 to 11 points less (no measurable step-time change),
     and thin-limbed ragdolls settle later.
   * **Cost:** our own determinism goldens would need recording with the toggle on.
2. **Combine mode.** Min (default) is the conservative choice. HarmonicMean was faster in the big
   pile (-13% at 10 m, -11% at 30 m) and showed no stability loss here. It is exposed but not the
   default.
3. **Sub-centimetre objects.** Recycling is not the limiting factor below about 2 cm. If the game
   needs small objects, the engine-wide lever is `set_length_units_per_meter` (for example,
   centimetres), which scales every Box3D tolerance, including these defaults. A scale-dependent
   slop or speculative margin would be a much larger, solver-level change, and I have not
   attempted it.
4. **Recording/replay.** The parameters are carried by snapshots but not by `recording::session`
   streams. Extend the recording format if replays must reproduce worlds with the toggle on.

## 8. Reproduce

```bash
cargo build -p pe-spike-physics --release && cargo build -p pe-spike-physics
# toggle off (upstream) and on (--vdw); repeat with target/debug
./target/release/pe-spike-physics goldens
./target/release/pe-spike-physics --vdw goldens
./target/release/pe-spike-physics --no-recycle goldens
./target/release/pe-spike-physics trace 600 spikes/out/vdw/trace-off-win-release.txt
./target/release/pe-spike-physics --vdw trace 600 spikes/out/vdw/trace-vdw-win-release.txt
./target/release/pe-spike-physics images spikes/out/vdw/images-off-win-release.txt
./target/release/pe-spike-physics --vdw images spikes/out/vdw/images-vdw-win-release.txt
./target/release/pe-spike-physics rollback && ./target/release/pe-spike-physics --vdw rollback
RUSTFLAGS="-C target-cpu=native" cargo build -p pe-spike-physics --release --target-dir target/native  # also opt-level=1, s
./target/release/pe-spike-physics vdw-bench 1800 spikes/out/vdw/vdw-bench-win-release.jsonl
./target/release/pe-spike-physics vdw-perf 1200 5 spikes/out/vdw/vdw-perf-win-release.jsonl
cargo test -p box3d-rust --release && cargo test -p box3d-rust
```

Comparing against the unmodified crate needs a crate outside this workspace, because the
workspace `[patch]` would otherwise apply. It depends on `box3d-rust = "=0.5.0"` and on
`spikes/sim-core` by path, and it runs the same reference loop as `images`.
