# Van der Waals simulation: a scale-dependent contact-recycling radius

> **Evidence paths** (since the M0 archive, 2026-11-10): paths under `spikes/out/` in this document refer to the
> tag `archive/v0-prototype`, where those logs are kept. Main no longer carries them; the physics records the
> tests read moved to `crates/pe-physics/tests/records/`.

Date: 2026-10-30. Author: @vdw-physics. Branch `vdw/contact-recycling`. Docket item:
@item:primordial-engine/van-der-waals-simulation-scale-dependent-contact.

Tags: **[V]** verified by running it here (raw output committed under `spikes/out/vdw/`);
**[S]** verified by reading source; **[I]** inferred.

## 1. Summary

* **What upstream does [S].** The recycling radius is one world-wide length,
  `contact_recycle_distance` = 0.05 m. It is a manifold-reuse tolerance on how far the pair's
  relative pose has drifted since the narrow phase last ran (section 2).
* **What we built [V].** The radius can now depend on the pair:
  `r = clamp(0.1 * HM(innerRadius_A, innerRadius_B), 5 mm, 0.5 m)`, where HM is the harmonic
  mean `2ab/(a+b)` (a `Min` combine is available). It sits behind
  `WorldDef::contact_recycle_scaling`, and the formula can be replaced for R15. It lives in our
  fork, `crates/box3d-fork`, through a `[patch.crates-io]` entry.
* **On by default in Primordial Engine worlds (user ruling 2026-10-30) [V].**
  `pe_sim_core::engine_world_def()` turns it on, and every engine and spike world uses it. The
  fork crate's own `default_world_def()` stays upstream (off), so the port still matches the C
  goldens. Section 5.1 lists every recorded digest that changed.
* **Toggle off is bit-identical to the unmodified crate [V].** Checks that passed (now run with
  `--upstream`): the 4 C goldens; the 600-frame trace (digest `7903109aab28ed9b`) in release,
  debug, opt-level 1, opt-level s and target-cpu=native; the snapshot-image bytes (digest
  `35af2efac5bfeb27`, equal to a build against the crates.io crate); rollback T1-T5; and all 547
  of the crate's own tests.
* **Toggle on (the engine default) is deterministic [V].** The trace (digest
  `80839f4a54133e13`) is identical across all 5 builds and in wasm32 under wasmi
  (`wasm-hotswap` D4). The new engine goldens pass in all 5 builds. Image bytes are deterministic
  in release and in debug; the two differ only by the debug-only validation flag (section 5).
  Rollback T1-T5 pass in all 5 builds, including T4, where the toggle travels inside the
  snapshot to a fresh world. Every one of the 120 benchmark configurations reproduced its final
  hash on a re-run.
* **Headline numbers [V]:**
  * **Small objects.** A 1 cm cube sliding off a ledge rides on stale contact points
    **3.83 cube-widths** past the edge with the fixed radius, against 0.29 with no recycling. The
    scaled radius gives **0.34**. At 10 cm: fixed 0.44, scaled 0.30, no recycling 0.30.
  * **Large objects.** In an 864-box pile, the recycle hit rate rises from 56% to 71% at 10 m and
    from 35% to 60% at 30 m. With the HarmonicMean default, step time falls **13% at 10 m (0.485
    to 0.423 ms) and 11% at 30 m (0.635 to 0.565 ms)**. With sleeping off, so every frame is
    awake, the falls are 5% and 12% (section 4.3).
  * **1 m objects.** The scaled radius was calibrated to match fixed here: unit-cube scenes give
    identical hashes in both modes. In the mixed-size 1 m pile, with sleeping off, the per-frame
    cost is the same in every recycling mode (0.584 fixed, 0.586 ms HarmonicMean). An earlier
    "+46% per awake frame" reading came from sleep timing, not from per-frame cost (section 4.3).
  * **Structures.** Stacks, pyramids and a 48-plank tower from 10 cm to 30 m: no collapse in any
    recycling mode, and sleep onset within about ±10% of fixed.
* **What it does not fix [V].** At 1 cm and below, every stack, pyramid and tower collapses in
  every mode, including with recycling off. Box3D's other absolute tolerances dominate there:
  `linear_slop` 5 mm, speculative distance 2 cm, the 0.05 m/s sleep threshold, and AABB margins
  that make a 1 mm pile collide all-pairs. At 1 mm the scaled radius also leaves the ledge cube
  further past the edge than no recycling (3.82 against 1.74 cube widths, section 4.2), because
  the 5 mm lower clamp is 5 cube widths. The engine-wide fix is `set_length_units_per_meter`, not
  recycling (user ruling, section 7).
* **User rulings (2026-10-30):** see section 7.

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
L   = 2 L_a L_b / (L_a + L_b)              HarmonicMean combine (default); a static side is ignored
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
* **HarmonicMean combine (default, user ruling 2026-10-30).** Stale points are wrong by about the
  drift `d`. Measured against the smaller body, that error matters most: a pebble on a boulder must
  not slide a pebble-width on stale points. The harmonic mean `2ab/(a+b)` stays between `min` and
  `2 * min`, so the smaller body still bounds the radius, but unequal pairs get a little more
  room. It was faster than `Min` in the big pile with no measured stability loss (section 4).
  `Min` remains available as the conservative option. A static side is ignored (infinite size),
  so a box on the ground uses the box's own size, and the two modes agree there and for
  equal-sized pairs.
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
  0, a negative value or NaN disables recycling for that pair. Because a peer restoring into a
  fresh world would otherwise fall back to the default formula silently, R15 must make the
  formula's identity part of world admission if it allows custom formulas **[I]**.

**Snapshot and rollback.** The parameters are written after the world-config flags byte, and only
when the toggle is on (new flag bit `0x20`, 13 bytes). With the toggle off, the image bytes are
exactly upstream's. A restore takes the toggle and the parameters from the image, whatever world
it restores into: the crate test `snapshot_carries_scaling_only_when_enabled` restores into a
`default_world_def()` world, and rollback test T4 into a fresh engine world.

**Recording and replay [V].** A recording seeds its replay from a frame-0 world snapshot, and its
keyframes are snapshots too, so a world created with the toggle on replays with the same
parameters. The crate test `recording_replays_scaling_from_world_def` records a 1 cm ledge run
with non-default parameters, replays it through `validate_replay` and `RecPlayer`, and gets the
same parameters and the same final position bit for bit. The run with the toggle off ends
elsewhere, so the test would catch lost parameters. The remaining gap is narrower than first
reported: a call to `world_set_contact_recycle_scaling` *during* a recording is not captured as
an op, unlike upstream's `world_set_contact_recycle_distance`. Engine worlds set the parameters
in the world definition and never change them mid-session, so this does not affect them.

## 4. Benchmark (step 4)

### 4.1 Method

`pe-spike-physics vdw-bench 1800 <out.jsonl>` and `pe-spike-physics vdw-perf 1200 5 <out.jsonl>`
(source: `spikes/physics/src/vdw_bench.rs`). The machine and toolchain are those in
`docs/spike-report.md` section 2: Ryzen 7 9700X, Windows 11, rustc 1.99.0, release build, serial
port. Each run is 60 Hz with 4 substeps. All timings here were taken before the fork stopped
running `validate_solver_sets` and `validate_island` in release (`crates/box3d-fork/FORK.md`).
That overhead applied to every mode alike, so the comparisons stand, but absolute step times are
now lower.

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
  * **bigpile:** 864 boxes, used for timing. **bigpile-awake** is the same pile with sleeping
    turned off, so every frame pays the full awake cost.
* **Modes.**
  * **off:** recycling disabled. The narrow phase runs every frame, so this is the
    no-approximation reference.
  * **fixed:** upstream.
  * **vdw:** the scaled radius with the Min combine (the default when these runs were made).
  * **vdw-hm:** the same with the HarmonicMean combine, which is now the default and what engine
    worlds use. The mode names are kept so the committed tables stay reproducible.
* **Statistics.** The scenes are chaotic, so each `vdw-bench` configuration runs 5 seeds with
  slightly different initial conditions, and the tables show means over seeds. `vdw-perf` runs
  modes interleaved over 5 repeats and reports the median.
* **Metrics.**
  * **hit:** recycled contacts divided by awake contacts.
  * **sleep:** seconds until every body sleeps.
  * **collapse:** seeds in which any body ended more than s/2 from its ideal rest pose.
  * **overhang:** the cube centre's distance past the edge, in s, when it has dropped 0.05 s.
* **Raw output:** `spikes/out/vdw/vdw-bench-win-release.{txt,jsonl}` (the jsonl has one line per
  seed), `spikes/out/vdw/vdw-perf-win-release.{txt,jsonl}` and
  `spikes/out/vdw/vdw-perf-awake-win-release.{txt,jsonl}`.

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

The ledge is static, so the HarmonicMean default gives the same radius as `vdw` (Min) here.
Overhang is the mean over 8 cubes and 5 seeds, in cube widths. Larger cubes overhang more in every
mode because they take sqrt(s) longer to tip at the same speed in s per second.

At 1 mm, the `r_min` = 5 mm clamp is 5 cube widths, so vdw does no better than fixed. Box3D itself
is out of range there: the no-recycling reference is already 1.7 widths. At 10 m the scaled radius
(0.5 m) allows some recycling that fixed (0.05 m) does not, and that costs 0.011 widths, about
11 cm.

### 4.3 Large objects: performance on an 864-box pile [V]

**With sleeping on (`bigpile`).** "awake ms" is the mean over frames that started with at least
one awake body.

| s | mode | step ms (median of 5) | awake ms | hit | mean contacts |
|---|---|---|---|---|---|
| 10 cm | off / fixed / vdw / vdw-hm | 0.123 / 0.101 / 0.102 / 0.097 | 0.879 / 0.710 / 0.701 / 0.672 | 0 / 89.0 / 80.3 / 80.6% | 2589 / 2617 / 2642 / 2642 |
| 1 m | off / fixed / vdw / vdw-hm | 0.240 / 0.172 / 0.172 / 0.161 | 0.484 / **0.309 / 0.451 / 0.408** | 0 / 72.7 / 66.1 / 66.2% | 1551 / 1496 / 1589 / 1584 |
| 10 m | off | 0.616 | 0.616 | 0% | 1249 |
| 10 m | fixed | 0.485 | 0.485 | 55.9% | 1222 |
| 10 m | vdw | 0.461 (-5.0%) | 0.461 | 71.2% | 1201 |
| 10 m | **vdw-hm (default)** | **0.423 (-12.9%)** | 0.423 | **71.0%** | 1195 |
| 30 m | off | 0.698 | 0.698 | 0% | 1046 |
| 30 m | fixed | 0.635 | 0.635 | 35.1% | 1042 |
| 30 m | vdw | 0.573 (-9.8%) | 0.573 | 60.5% | 1021 |
| 30 m | **vdw-hm (default)** | **0.565 (-11.0%)** | 0.565 | **60.2%** | 1011 |

**With sleeping off (`bigpile-awake`), every frame awake, median of 5 [V].**

| s | off | fixed | vdw (Min) | vdw-hm (default) | hit fixed / vdw / vdw-hm |
|---|---|---|---|---|---|
| 10 cm | 1.071 | 0.721 | 0.695 (-3.6%) | **0.683 (-5.4%)** | 99.1 / 98.6 / 98.6% |
| 1 m | 0.860 | 0.584 | 0.581 (-0.6%) | **0.586 (+0.3%)** | 95.7 / 94.6 / 94.9% |
| 10 m | 0.799 | 0.602 | 0.582 (-3.3%) | **0.570 (-5.3%)** | 76.1 / 82.6 / 83.7% |
| 30 m | 0.718 | 0.620 | 0.555 (-10.4%) | **0.546 (-11.9%)** | 49.6 / 68.2 / 69.6% |

**Reading the 1 m row.** With sleeping on, awake ms at 1 m rises 46% (Min) and 32%
(HarmonicMean) against fixed. @design-reviewer found this, and it reproduces. With sleeping off,
the same pile costs the same per frame in all three recycling modes, within 0.6%. So the rise is
not a per-frame cost of the scaled radius **[V]**. The likely cause is what the awake mean
averages over **[I]**: it counts every frame with at least one awake body, whatever the number
awake. A mode that keeps a long tail of cheap frames, with only a few bodies still awake, gets a
lower mean, and one that puts the whole pile to sleep sooner gets a higher one. The all-frames
step mean at 1 m is flat to slightly better (0.172 / 0.172 / 0.161). A 1 m scene that stays awake,
like the ball pit, pays no extra. @design-reviewer's 500-ball session-sim run found the same
within noise.

At 10 m and 30 m the pile never sleeps within the 20 s run, so both tables measure cost per awake
frame there. Even so, the 10 m HarmonicMean gain differs between the two runs (-12.9% against
-5.3%). Turning sleep off changes island bookkeeping and the trajectory (the final hashes differ),
and I have not isolated which of these accounts for the gap **[I]**. Quote the 10 m gain as 5 to
13%. At 10 cm and 1 m the
scaled radius recycles slightly less than fixed, because many pile boxes are smaller than 1 m and
get a tighter radius. That the gain comes from recycled narrow-phase calls is **[I]**. The
hit-rate changes are **[V]**.

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

| Scene | sleep step: no recycling / fixed (= C golden) / vdw (Min) / HarmonicMean (engine golden) |
|---|---|
| falling ragdolls | 309 / 291 / 361 / 358 |
| wave pile | 279 / 272 / 307 / 307 |
| query spawn | 242 / 242 / 242 / 242 |
| mesh drop | 221 / 226 / 220 / 220 |

The last column is now the **engine golden** that `pe-spike-physics goldens` checks by default
(hashes in section 5.1). These are single chaotic samples. The ragdolls' limbs are thin capsules, so their small inner
radius gives them a 5 to 10 mm recycling radius. They recycle less and settle later. Whether that
is more or less accurate is **[I]** and not established by these numbers.

### 4.7 Cost of the toggle itself [V]

In cases where fixed and vdw give the same trajectory (unit-cube stack, pyramid and tower at 1 m,
which have identical final hashes), the step times agree within run-to-run noise. Pyramid at 1 m,
awake ms: 0.0448 fixed against 0.0449 vdw. The per-pair cost is a few flops, inside a branch that
only runs for pairs that are candidates for recycling.

## 5. Determinism and rollback (step 5) [V]

Raw output is in `spikes/out/vdw/`. Commands are in section 8.

Re-recorded 2026-10-30 with the engine default (HarmonicMean, on). Files are named `-vdw-` for
the engine default and `-off-` for `--upstream`. The five builds are release, debug,
opt-level=1, opt-level=s and target-cpu=native.

| Check | Toggle off (`--upstream`) | Toggle on (engine default) |
|---|---|---|
| 4 goldens, 5 builds | C goldens: PASS 5/5 | engine goldens: PASS 5/5 |
| 600-frame trace digest | `7903109aab28ed9b`, equal to `spikes/out/trace-win-release-1.txt` byte for byte | `80839f4a54133e13` |
| Trace identical across the 5 builds | yes (5/5) | yes (5/5) |
| Trace in wasm32, interpreted by wasmi (`wasm-hotswap` D4) | (was `7903109aab28ed9b` before default-on) | `80839f4a54133e13` **== native** |
| Snapshot image bytes, 13 snapshots over 600 frames | release `35af2efac5bfeb27`, debug `b5dc824fbd65fcd0`, both equal to a build against the unmodified crates.io crate (`scratch/upstream-probe`) | release `7a47a3bfcdbd7eda` (opt1, opts and native identical), debug `9fa979e8af603190` |
| Rollback T1-T5, 5 builds | PASS 5/5 | PASS 5/5. T4 restores into a fresh world, and the toggle comes from the image |
| box3d-rust test suite (547 upstream + 6 new), release / debug | 553 / 553 pass | (the new tests cover the toggle) |
| `vdw-bench`: re-run seed 0, final world hash | 60/60 configs reproduce (modes off, fixed) | 60/60 configs reproduce (modes vdw, vdw-hm) |

**Release versus debug image bytes [S].** The image bytes differ between release and debug in
both modes, and the unmodified crate behaves the same way. The snapshot header carries a
`SNAP_FLAG_VALIDATION` bit that is set only in `debug_assertions` builds
(`recording/snapshot/mod.rs:106`). Images have the same length, and `deserialize_into_shell` does
not check that bit, so debug and release peers can still exchange snapshots. Simulation state is
identical.

**Not covered.** Linux, aarch64 and macOS with the toggle on, and a second machine. The
recording system is covered by the crate test in section 3.

### 5.1 Recorded digests that changed when the default turned on [V]

Every world built through `pe_sim_core` (`build_pile_scene`, the session scenes, the ball pit,
late-joiner restores) now uses `engine_world_def()`. Each output below was re-run on this branch.
Every determinism check still passes: all members agree, offline replays are equal, and the
native and wasm digests match. Only the digests and other trajectory-dependent numbers changed.

| Output | Before (toggle off) | After (engine default) |
|---|---|---|
| Engine goldens, ragdolls / wave / query / mesh (`spikes/out/vdw/goldens-vdw-*`) | C goldens 291 `0xFF4C605B` / 272 `0x266F83B0` / 242 `0xF3AD9BA6` / 226 `0xD2FF069D` | 358 `0xB1EB47AE` / 307 `0x0809EA96` / 242 `0x9EA3A8B9` (query hits 59, `0xE3271F3D` unchanged) / 220 `0xCE3B7A62` |
| `pe-spike-physics trace 600` digest | `7903109aab28ed9b` | `80839f4a54133e13` (776324c's `--vdw` trace with Min was `a9041fec18d56c02`) |
| `wasm-hotswap` D4 (`spikes/out/wasm-hotswap-{eager,lazy}.log`) | `7903109aab28ed9b` | `80839f4a54133e13`; the check in `spikes/wasm-hotswap/src/main.rs` is updated |
| `physics_in_wasm.wasm` raw SHA-256 (`spikes/out/guest-wasm-sha256.txt`) | `589e97a7008badfd…` | `1184af4c59cce8ed…` at b8f130c, then `f15b4349e8417d46…` after the release-validator change (same digest). The guest workspace now has the same `[patch]`. The logic guests are byte-identical |
| p2p over UDP, latency60 / loss10 / fault (`spikes/out/p2p-v2/`) | `cd613a64b7088dbe` (loss10 stable over 3 of 3 runs, per @design-reviewer) | latency60 `66d3f4a12f8bdef2`, reproduced in 4 of 4 runs here. **loss10 and fault vary from run to run:** loss10 gave `66d3f4a1…` 3 times and `93ee0a17…` once here, and @design-reviewer got 3 more values. Fault gave `66d3f4a1…` in 4 of 4 runs here, but @design-reviewer got `eb59c80c…`. In every run, peer 1 == peer 2 and the offline replay is EQUAL (`repeat-runs-engine-default.txt`). See the note below |
| session-sim state-frame / loss | `eb64716d53886595` | `255efbf3d9457fdd` |
| session-sim diverged | `eb64716d53886595` | `60ddd49e7dbc3ed7` (see below) |
| session-sim c1 after / c2 after | `2375c7697cc99d72` / `866944464816eb58` | `363d9187ffb4eceb` / `70fcaec32e3c69d2` |
| session-sim ties / claims | `fd715814787415c5` / `8c1020d28d4dfd83` | `bf9a1dede91fe191` / `73ef41c38a9a0bd2` |
| session-sim backdate part 1 / part 2 | `2b34e4c05ca9ee12` / `439f5a393e172759` | `154a1f2b17360d56` / `f306841cdb5f34cc` |
| session-sim r14 ok and throttle, world A / B | `740fb52b228e1268` / `2aa29bd8029fc8b6` | `3afcee83511f3bbc` / `7a8eaaeb80ef52b1` |
| session-sim r14 abort, A / B | `e7e8ec8ce85c2df7` / `774720801bda8068` | `03e304275f697b53` / `c85a509f8b86a712` |
| session-sim r14 reject, A / B | `7765e0c494f87711` / `385c0a2138478f74` | `60b5e15a67c384dc` / `1597788da93d2abe` |
| session-sim ballpit 60 Hz (2% loss and no loss) | `1a9f1fb5fce6acc6` | `84fd01d1d13bb6a5` (equal to @design-reviewer's independent HarmonicMean run) |
| session-sim ballpit ahead4 / 2 hands / 90 Hz | `e684194ebc387d16` / `0b529d1dbae12118` / `e4d428f81bb11195` | `a26b494508aba729` / `89bba46995b379c6` / `70fbefcc1fe64f75` |
| session-sim ballpit, average balls changed per frame (60 Hz / ahead4 / 2 hands / 90 Hz) | 297 / 289 / 365 / 286 | 292 / 284 / 375 / 306 |
| MCP stub (`spikes/out/mcp-session.out.jsonl`) | state hashes and object values | new values; the same 10 responses and the same error/ok outcomes |

**p2p under loss: the outcome depends on network timing [V], and the cause is in the protocol
[I].** Over real UDP, a lossy run's final world is not a function of the scripted inputs alone,
although the peers always agree and the offline replay is always EQUAL. @design-reviewer traced
one loss10 divergence to frame 470, member 2's scripted touch. Under the state+frame ruling, an
entry carries its author's speculative post-frame state. Whether member 1's earlier entries had
arrived in time depends on loss and timing, so the carried state does too ("carried state wins").
With upstream physics that touch happened not to be sensitive; with the engine default it is.
This is not a recycling defect. It belongs in the session protocol's documentation.

**Kept as toggle-off records, not re-run.** The top-level `spikes/out/trace-*.txt`,
`physics-*.log` and `trace-linux-musl-release.txt` files are the phase-1 upstream records
(`docs/spike-report.md`'s commands for them now need `--upstream`).
`pe-spike-physics --upstream` still reproduces them (trace `7903109aab28ed9b`). Two sets could
not be re-run here, because Python is not installed on this machine:
`spikes/out/iter-bench/physics-runtimes.jsonl` and `spikes/out/wasm-canonical-hash.txt` (the
canonical hash `66633b43…`). Both are toggle-off values.

**Docs citing toggle-off values.** These lines in documents I do not own now describe toggle
off: `docs/spike-report.md` lines 95 and 318 (`7903109aab28ed9b` for a2 and d4), 207-209
(`cd613a64b7088dbe`), and 345-347 (`589e97a7…`, `66633b43…`); `docs/design.md` line 1012
(`66633b43…`), and line 468 (diverged 23/23, see below). *Fixed at integration (2026-10-30, branch
`integration`): each now gives the engine default's value or is labelled upstream.*

**At integration [V].** The integration branch merged this work with rounds 2 and 3 of the session
protocol and re-ran the set (spike-report 5j). The physics records above reproduce byte for byte in
release and debug. The session-sim digests in the table equal the "after" column for state-frame, loss,
diverged, c2, ties, claims, backdate part 1, r14 abort (B) and the ball pit at 60 Hz, ahead4 and 2 hands.
c1, backdate part 2, the other r14 runs and the 90 Hz ball pit differ: rounds 2 and 3 changed exactly
those scenarios (their upstream digests changed too). Their current values are in the logs.

**session-sim diverged: acceptance at that instant fell from 23/23 to 21/23 [V].** Final-history
acceptance is still 23/23, and all three members and the offline replay agree. The two misses are
both object 5 at frame 333, which member 2 touched while it was still missing member 3's frame-300
touch on object 77. The result depends only on the combine mode: toggle off and `Min` both give
23/23, and HarmonicMean gives 21/23 (each built and run on this branch). My reading **[I]**: in
the HarmonicMean trajectory, object 77's disturbance reaches object 5 through the pile before
frame 333. The receivers, who have member 3's entry, then see a different object 5 than the
author. The scenario counts a check as contended only when the *same* object was touched in the
previous 10 frames, so this indirect influence counts as uncontended. That is a property of the
acceptance metric and the protocol, not of recycling. I have left it to the session-protocol
owner.

## 6. Code map

| Where | What |
|---|---|
| [crates/box3d-fork/FORK.md](../crates/box3d-fork/FORK.md) | Provenance, license, every changed file |
| `crates/box3d-fork/src/recycle_scale.rs` | Parameters, combine modes, hook type, default formula |
| `crates/box3d-fork/src/contact/collide.rs` | Per-pair tolerance, used only when enabled |
| `crates/box3d-fork/src/recording/snapshot/pods.rs` | Snapshot flag `0x20` and the parameters |
| `crates/box3d-fork/src/recycle_scale_tests.rs` | Formula, hook, ledge regression, snapshot and recording round trips |
| `spikes/sim-core/src/lib.rs` | `engine_world_def()` (toggle on, used by every engine world); `build_pile_scene_def` (pile with a caller-supplied `WorldDef`) |
| `spikes/physics/src/vdw_bench.rs` | Benchmark scenes and metrics |
| `spikes/physics/src/main.rs` | Engine goldens, `--upstream` and `--no-recycle` flags, `images`, `vdw-bench`, `vdw-perf` |
| `spikes/wasm-guests/Cargo.toml` | The same `[patch]`, so the wasm32 pile uses the fork |

## 7. User rulings (2026-10-30)

These were open decisions in 776324c. The user answered them at 14:20Z.

1. **On by default in Primordial Engine worlds. Done.** `pe_sim_core::engine_world_def()` turns
   the toggle on, and every engine and spike world is built from it. The fork crate's own
   `default_world_def()` keeps upstream's default (off), so the port still matches the C goldens
   with `--upstream`. Our determinism goldens and traces are re-recorded with the toggle on
   (sections 5 and 5.1).
2. **HarmonicMean is the default combine mode. Done.** `ContactRecycleScaling::enabled()` and
   `disabled()` both carry `RecycleLengthCombine::HarmonicMean`. `Min` remains available. Old
   snapshots are unaffected, because an enabled image stores its combine byte.
3. **Sub-centimetre objects: no scaled slop or speculative tolerances.** Below about 2 cm, Box3D's
   absolute tolerances (`linear_slop`, the speculative distance, the sleep threshold, AABB
   margins) limit accuracy, not recycling. A world of small objects should call
   `set_length_units_per_meter` before it creates the world, for example with 100 for
   centimetres. That scales every Box3D tolerance, including this feature's `min_distance` and
   `max_distance` defaults, which are defined in meters times the length units **[S]**
   (`constants.rs`, the body and world defaults, `recycle_scale.rs`). The defaults are read when
   `engine_world_def()` and the body definitions are built, so set the units first. The game
   then expresses its lengths in those units.
4. **Recording/replay: documented limitation.** It is narrower than first reported: recordings do
   carry parameters set in the world definition (section 3, verified by a crate test). Two things
   are not captured: a mid-recording `world_set_contact_recycle_scaling` call, and a custom
   `ContactRecycleRadiusFn`, which is also absent from snapshots. Neither affects engine worlds
   today. Adding a recording op would change the recording format away from upstream's, so it was
   not trivial and I did not do it.

## 8. Reproduce

```bash
cargo build -p pe-spike-physics --release && cargo build -p pe-spike-physics
# engine default (toggle on) and --upstream (toggle off); repeat with target/debug
./target/release/pe-spike-physics goldens                 # engine goldens
./target/release/pe-spike-physics --upstream goldens      # C goldens
./target/release/pe-spike-physics --no-recycle goldens
./target/release/pe-spike-physics trace 600 spikes/out/vdw/trace-vdw-win-release.txt
./target/release/pe-spike-physics --upstream trace 600 spikes/out/vdw/trace-off-win-release.txt
./target/release/pe-spike-physics images spikes/out/vdw/images-vdw-win-release.txt
./target/release/pe-spike-physics --upstream images spikes/out/vdw/images-off-win-release.txt
./target/release/pe-spike-physics rollback && ./target/release/pe-spike-physics --upstream rollback
RUSTFLAGS="-C target-cpu=native" cargo build -p pe-spike-physics --release --target-dir target/native  # also opt-level=1, s
./target/release/pe-spike-physics vdw-bench 1800 spikes/out/vdw/vdw-bench-win-release.jsonl
./target/release/pe-spike-physics vdw-perf 1200 5 spikes/out/vdw/vdw-perf-win-release.jsonl
./target/release/pe-spike-physics vdw-perf 1200 5 spikes/out/vdw/vdw-perf-awake-win-release.jsonl bigpile-awake
cargo test -p box3d-rust --release && cargo test -p box3d-rust
# digests that depend on the engine world definition (section 5.1)
bash scripts/build_wasm_guests.sh   # since integration; physics_in_wasm builds in the root workspace
./target/release/pe-spike-wasm-hotswap
bash scripts/run_session_sim.sh
bash scripts/run-p2p.sh latency60 "--delay-ms 60 --jitter-ms 20" "--delay-ms 60 --jitter-ms 20"   # also loss10, fault: docs/spike-report.md
```

Comparing against the unmodified crate needs a crate outside this workspace, because the
workspace `[patch]` would otherwise apply. It depends on `box3d-rust = "=0.5.0"` and on
`spikes/sim-core` by path, and it runs the same reference loop as `images`.
