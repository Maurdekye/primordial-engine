# R16/R17 spike results: hierarchical chunks, f64 coordinates, regions, scale, avatar culling

Date: 2026-10-30. Author: @world-chunks. Branch `chunks/r16`. Design: [design.md](design.md)
sections 25 and 26 (merged at integration; decisions renumbered D58-D67 -> D63-D67).

Every number is labelled:
* **[V]** measured by `spikes/chunks` on this machine;
* **[C]** computed from measured inputs by a stated formula;
* **[I]** inferred, not measured.

**Setup:** Ryzen 7 9700X (8 cores, 16 threads), Windows 11, rustc 1.99.0 stable, release builds (opt-level 3, no
LTO, as in the phase-1 spikes), box3d-rust 0.5.0, 60 Hz with 4 sub-steps.

**Load caveat:** other agents ran benchmarks on the same machine during these runs. Single runs varied by up to
2x (for example, the 2,048-body step ranged from 3.9 to 12.7 ms over five rounds). The f32-vs-f64 comparison
therefore uses 8 interleaved rounds and takes, for every 20-frame block, the minimum over rounds (the
estimator least affected by other load). Elsewhere, treat small differences (under about 20%) as noise.

**Reproduce:** `scripts/run_chunks_bench.sh`, with `BOX3D_NOVALIDATE` pointing at a box3d-rust copy carrying
the one-line patch in section 2. Summarize with `python -I scripts/chunks_summary.py`. For the A/B rounds, run
`pe-spike-chunks ab` alternately in the four builds. Raw logs are in `spikes/out/chunks/`.

**Four builds:** `f32` and `f64` are box3d-rust as published, without and with its `double-precision` feature.
`f32-nv` and `f64-nv` add the patch from section 2.

## 1. Coordinate precision [V, exact IEEE arithmetic]

Representation step (ulp) at distance D from the origin:

| D | f32 absolute | f64 absolute | chunk + f32 offset | chunk + f64 offset |
|---|---|---|---|---|
| 1 km | 6.1e-5 m | 1.1e-13 m | 6.1e-5 m | 1.1e-13 m |
| 100 km | 7.8e-3 m | 1.5e-11 m | 6.1e-5 m | 1.1e-13 m |
| 10,000 km | 1 m | 1.9e-9 m | 6.1e-5 m | 1.1e-13 m |
| 1e9 m | 64 m | 1.2e-7 m | 6.1e-5 m | 1.1e-13 m |
| 100 AU | 1.0e6 m | 2.0e-3 m | 6.1e-5 m | 1.1e-13 m |
| 1 light-year | 1.1e9 m | 2 m | 6.1e-5 m | 1.1e-13 m |
| 100,000 ly | 7.0e13 m | 1.3e5 m | 6.1e-5 m | 1.1e-13 m |

The chunk index range is ±9.4e21 m (±1.0e6 light-years). OWO's range is 2^69 m, about 62,400 ly.

Error after 1 hour of walking (1.3 m/s, 60 Hz, `x += v * dt`; true distance 4,680 m):

| D | f32 absolute | f64 absolute | chunk + f32 | chunk + f64 |
|---|---|---|---|---|
| 1 km | 10.4 m | 1.8e-8 m | 0.17 m | 2.9e-9 m |
| 100 km | 383 m | 1.7e-7 m | 0.17 m | 2.9e-9 m |
| 10,000 km | 4,680 m (cannot move) | 3.8e-5 m | 0.17 m | 3.0e-9 m |
| 100 AU | cannot move | 39 m | 0.17 m | 2.9e-9 m |
| 1 ly | cannot move | 4,680 m (cannot move) | 0.17 m | 2.9e-9 m |

With f32 absolute coordinates, at D ≥ 2^19 m (524 km) the ulp is 6.25 cm, so a 2.17 cm step rounds to zero.

## 2. box3d-rust runs validation loops in release builds [V]

`World::validate_solver_sets()` walks every solver set and body. Only its `debug_assert!`s are compiled out;
the loops still run in release builds. In the C reference, the function is compiled out unless
`B3_VALIDATE` is set. It is called from `create_body`, `destroy_body`, shape creation and destruction, type
changes, sleep and wake transfers, and several times per step.

**The patch:** an early `return` when `!cfg!(debug_assertions)`, the pattern the crate already uses for
`validate_no_moved`.

| | unpatched | patched |
|---|---|---|
| create_body + sphere shape, sleeping, 3,500 bodies already present | 56 us per body | 0.67 us |
| Move 1,000 bodies between two worlds (region merge), f64 | 60.6 ms | 0.89 ms |
| Create 1,000 sleeping cubes from records / destroy them, f64 | 9.0 / 6.6 ms | 2.4 / 0.17 ms |
| Step, minimum envelope over 8 rounds: 1,024-body pile, f32 | 1.071 ms | 0.986 ms (-8%) |
| the same, 2,048-body pile, f32 | 2.722 ms | 2.342 ms (-14%) |
| the same, 500-ball pit with a sweeping hand, f32 | 0.773 ms | 0.700 ms (-9%) |

Without the patch, creating N bodies costs O(N^2).

The patch does not change behaviour. Region-determinism digests are identical with and without it
(`23d1a1b558a9a21d` in f32, `91f16f96995a6161` in f64). @vdw-physics is landing it in `crates/box3d-fork` as a
separate commit, verified against the goldens. All other numbers below are given for both builds where they
differ.

## 3. f32 vs f64 positions in box3d [V]

Step time, minimum envelope over 8 interleaved rounds (30 blocks of 20 frames each):

| Scene | f32 | f64 | f64 cost | f32-nv | f64-nv | f64 cost (nv) |
|---|---|---|---|---|---|---|
| 1,024-body pile | 1.071 ms | 1.160 ms | +8.4% | 0.986 ms | 1.081 ms | +9.6% |
| 2,048-body pile | 2.722 ms | 2.776 ms | +2.0% | 2.342 ms | 2.534 ms | +8.2% |
| 500-ball pit + hand | 0.773 ms | 0.786 ms | +1.8% | 0.700 ms | 0.737 ms | +5.3% |

Snapshot images grow 1-2% with f64: 464,306 → 472,288 B at 256 bodies; 1,984,094 → 2,019,062 B at 1,024;
4,076,504 → 4,159,856 B at 2,048; 1,835,697 → 1,853,175 B for the ball pit. Save and restore times are inside
the noise.

**Result: double precision costs 2-10% of step time and 1-2% of snapshot size.**

## 4. Physics far from the origin [V]

Each scene is placed at offset (D + 0.37, D/2 + 0.21, -0.75 D + 0.13), far on all three axes as on a planet's
surface. "Region-local" places the same scene relative to the centre of its 1,024 m chunk, the proposed region
origin.

**Rolling ball** (2 m/s for 10 s; at the origin it ends at x = 7.2432 m). The table gives the final x and the
largest deviation from the origin run:

| D | f32 absolute | f32 region-local | f64 absolute | f64 region-local |
|---|---|---|---|---|
| 1 km | 7.2429 (4e-4) | 7.2431 (5e-4) | 7.2431 (1.2e-4) | 7.2431 (7e-5) |
| 100 km | 7.4347 (0.20 m) | 7.2432 (1.6e-4) | 7.2430 (1.9e-4) | 7.2432 (3e-5) |
| 1,000 km | 0.0675 (stuck) | 7.2431 (1.6e-4) | 7.2431 (1.2e-4) | 7.2431 (6e-5) |
| 10,000 km | -0.37 (stuck) | 7.2432 (1.6e-4) | 7.2430 (1.5e-4) | 7.2432 (3e-5) |
| 100,000 km | -0.37 (stuck) | 7.2432 (1.6e-4) | 7.2431 (8e-5) | 7.2432 (3e-5) |

**8-cube tower:** largest deviation from the origin run, and whether it settles.

| D | f32 absolute | f32 region-local | f64 absolute | f64 region-local |
|---|---|---|---|---|
| 1 km | 3.6e-4 m, settles | 1.3e-4 m | 7e-14 m | 4e-14 m |
| 100 km | 9.0e-3 m, settles | 1.9e-4 m | 5e-11 m | 6e-14 m |
| 1,000 km | 2.5e-2 m, **never sleeps** | 1.0e-4 m | 6.5e-10 m | 3e-14 m |
| 10,000 km | 0.37 m, **top 0.39 m off-axis, awake** | 1.9e-4 m | 8.7e-7 m | 6e-14 m |
| 100,000 km | 1.7 m, **awake** | 1.9e-4 m | 8.1e-7 m | 6e-14 m |

**256-body pile: broadphase cost.** Contacts at frame 600, and mean step time:

| D | f32 absolute | f32 region-local | f64 absolute | f64 region-local |
|---|---|---|---|---|
| 0 | 578, 0.23 ms | 537, 0.25 ms | 542, 0.22 ms | 542, 0.24 ms |
| 1,000 km | 699, 0.32 ms | 518, 0.41 ms | 735, 0.23 ms | 542, 0.22 ms |
| 10,000 km | 2,975, 1.26 ms | 578, 0.25 ms | **2,562, 0.52 ms** | 542, 0.23 ms |
| 100,000 km | 10,905, 16.1 ms | 566, 0.21 ms | **32,142, 4.96 ms** | 542, 0.23 ms |

* **f64 positions alone are not enough far away.** box3d keeps AABBs, rotations and contact anchors in f32. At
  10,000-100,000 km the f32 AABBs (rounded outward, 1-8 m ulp) inflate the broadphase: about 5 to 60 times the
  contacts and 2 to 23 times the step time.
* **A region-local origin fixes both builds.** Box3D coordinates stay within about 1 km, and cost and
  behaviour equal the origin's at every D.
* **f64 is translation-invariant in practice; f32 is not.** The pile is chaotic. In f64 with a region-local
  origin, it reproduced the origin run to within 1e-12 m in all 7 placements, with the same contact count. In
  f32 any translation, including a pure 512 m shift, sent the pile down a different but equally valid path
  (metres apart after 10 s). So with f64, rebasing a region origin does not perturb the physics; with f32
  it does, deterministically but visibly in chaotic scenes. A region **merge** is different: it recreates
  bodies, which drops contacts and warm starts. It is deterministic but not neutral (review R16-4); this
  was not measured.

## 5. Arbitrary scale: per-region length units [V]

An 8-cube tower of edge s on a slab, 600 frames. Variants:
* **unscaled:** metres, as box3d is.
* **unit = s, invariant gravity:** the region's length unit is s and gravity is 10 units/s², so the world
  behaves at every scale as a 1 m world does.
* **unit = s, real gravity:** 10 m/s² expressed in units.
* **time-scaled real gravity:** the invariant system with time running `1/sqrt(s)` faster, that is
  `dt / sqrt(s)` per tick at the same 240 Hz invariant sub-step rate.

| s | unscaled | unit = s, invariant | unit = s, real g, 4 sub-steps | unit = s, real g, time-scaled |
|---|---|---|---|---|
| 0.1 mm | hovers 17.6 edges above the ground | stands; **hash identical to 1 m** | explodes | stands (400 sub-steps per tick) |
| 1 mm | hovers 0.86 edges up | identical to 1 m | explodes | stands (127) |
| 1 cm | **collapses** (top 138 edges away) | identical to 1 m | collapses | stands (40) |
| 10 cm | stands, sleeps late (frame 91) | identical to 1 m | stands | stands (13) |
| 1 m | stands (frame 49) | identical | identical | identical |
| 10 m - 10 km | stands | identical to 1 m | stands, sleeps sooner | stands; 1 sub-step, real time runs slower |

* **What the "hash identical" column shows** (review R16-7): with unit = s exactly, box3d sees the identical
  1-unit scene at every scale. The identical hash therefore only shows determinism, not that the units rule
  works.
* **The real test (`scale-mixed`)** uses the design's rule, unit = `2^round(log2 s)`, so box3d sees 0.78-1.28
  units. The scene mixes sizes: a stack of cubes with edges 2s, s, s/2 and s/4, a ball of radius s/20 on top,
  and 8 balls of radius s/10 on the ground. With invariant gravity, 600 frames:

  | s | unscaled | unit = 2^round(log2 s) |
  |---|---|---|
  | 0.1 mm, 1 mm | **unstable** (the stack does not stay put) | stable; small-ball penetration 0.09 / 0.07% of the radius; sleeps at frame 32 / 34 |
  | 1 cm | stable, but small balls sink **7%** of their radius | stable, 0.06%, frame 32 |
  | 10 cm | stable, 0.7% | stable, 0.09%, frame 32 |
  | 1 m | stable, 0.07%, frame 32 | identical (unit = 1) |
  | 10 m - 10 km | stable | stable, 0.06-0.09%, frame 32-33 |

  So per-region length units with power-of-two quantization keep box3d in its tuned range at every scale
  from 0.1 mm to 10 km, with mixed sizes from s/20 to 2s in one region. This is ruling 14:20Z's "length
  units", obtained per region without the process-global `set_length_units_per_meter`.
* The unscaled failures match @vdw-physics's finding (absolute 5 mm slop, 2 cm speculative distance).
* **Real gravity at small scale is the invariant system run faster:** sub-steps grow as `1/sqrt(s)`, so CPU for
  awake bodies grows about 3x at 10 cm, 10x at 1 cm and 32x at 1 mm [C: sub-step count; box3d cost is linear in
  sub-steps, I]. Plain sub-stepping in scaled units (4 → 40 to 256 sub-steps) did **not** fix it: the tower
  still collapsed or exploded.

## 6. Spatial index (user suggestion: an octree on the chunk system) [V]

Indexes compared:
* a chunk-rooted octree: tight, each object in the deepest node that contains its box;
* a chunk-rooted loose octree (k = 2);
* a hashed multi-level grid, keyed by (level, cell), in a `BTreeMap` (ordered) or a `HashMap`.

All four return identical results, verified against brute force on 16 queries per workload. They also return
identical results when built in shuffled insertion order (determinism check: true for both workloads).

Workloads:
* **Human-surface:** 200,000 objects on 32 x 32 km, radii from 2 cm to 50 m plus 200 objects of 50 m-2 km; 64
  human players (ratio 32, min radius h/64); 10% of objects moving at 1-5 m/s and 1% at 300 m/s.
* **Extreme-scale:** 200,000 objects with radii from 1 um to 1,000 km in 64 clusters across 10,000 km; players
  from 10 um to 100 km tall.

| Workload | Index | Build | Memory | Query | Nodes / tested per query | Update per moved object |
|---|---|---|---|---|---|---|
| human | octree, chunk-rooted | 149 ms | 183 MB | 59 us | 1,592 / 567 | 557 ns |
| human | loose octree, chunk-rooted | 121 ms | 321 MB | 104 us | 2,110 / 219 | 312 ns |
| human | **hashed grid, BTreeMap** | **69 ms** | **77 MB** | **34 us** | 459 / 741 | **250 ns** |
| human | **hashed grid, HashMap** | 77 ms | 79 MB | **25 us** | 459 / 741 | **155 ns** |
| extreme | octree, chunk-rooted | 152 ms | 131 MB | 236 us | 1,070 / 6,837 | 568 ns |
| extreme | loose octree, chunk-rooted | 103 ms | 201 MB | 312 us | 1,657 / 6,710 | 206 ns |
| extreme | **hashed grid, BTreeMap** | **67 ms** | **69 MB** | **179 us** | 709 / 7,038 | 212 ns |
| extreme | hashed grid, HashMap | 72 ms | 71 MB | 184 us | 709 / 7,038 | **168 ns** |

Queries return 182 (human) and 401 (extreme) objects on average.

**Recommendation: the hashed multi-level grid.**
* It is the fastest on query and build, and uses 2.4-4.2 times less memory.
* Update cost is a near tie. In the reviewer's rerun the loose octree was marginally faster on human-scale
  updates (453 vs 487 ns), and it was within 25% on the extreme workload here.
* It has no depth: a 1 um object is one hash lookup away, not 28 tree levels.
* A query reads 27 cells per occupied level, independent of scale.

`BTreeMap` versus `HashMap` matters little. The `BTreeMap` gives ordered iteration for free; the `HashMap` is
about 1.4 times faster per query on the human workload. Since results are sorted by id before use, either
works. The user's octree intuition is right about the structure: the grid is an implicit loose octree, where
tree depth is the level. Only the pointer-based tree is rejected, on these numbers.

## 7. Regions against one flat world [V]

16 players, each with a 125-ball pit and a sweeping hand (2,016 bodies):
* **flat:** one box3d world with all 16 pits, 200 m apart;
* **regions:** 16 box3d worlds of one pit each.

| | f32 | f64 | f32-nv | f64-nv |
|---|---|---|---|---|
| Step, flat world | 1.06 ms | 1.20 ms | 0.78 ms | 0.86 ms |
| Step, 16 regions in series | 1.13 ms | 1.23 ms | 0.91 ms | 1.08 ms |
| Step, 16 regions in parallel (wall clock) | 0.24 ms | 0.23 ms | 0.18 ms | 0.23 ms |
| **Late entry for one player, 6-frame rollback: flat** | 11.2 ms | 13.0 ms | 8.6 ms | 10.5 ms |
| **The same: only that player's region** | **0.61 ms** | **0.66 ms** | **0.55 ms** | **0.68 ms** |
| Snapshot images, flat / sum of regions | 4.62 / 4.72 MB | 4.70 / 4.80 MB | same | same |
| Merge two regions, per moved body (1,000 bodies) | 57 us | 61 us | 0.69 us | 0.89 us |

* **A rollback costs 15-18 times less** when only the affected region re-simulates.
* **Regions step in parallel:** 4.7-5.3 times less wall time on 16 threads. That is a gain only for a peer
  that simulates several regions; a player usually simulates one.
* Splitting into regions costs about 2% more image bytes (per-world overhead).
* **Determinism [V]:** independent regions produced identical per-frame hashes when stepped forward, in reverse
  order, or in parallel on 8 threads. A region restored to frame 200 and re-simulated to 300 matched its straight
  run frame for frame. All four builds passed.

## 8. Per-player cost against player count [V for CPU and memory, C for bandwidth]

Each player has a 125-ball pit with a hand sweeping through it (126 bodies, about 71 awake). Layouts:
* **spread:** every player more than 2 km from the others, so one region each;
* **groups of 8:** 8 players together, groups far apart;
* **clustered:** everyone in one place.
* **Unchunked:** today's design, in which every peer simulates every player's pit whatever the layout. Its
  cost equals the clustered layout's.

**CPU per 16.7 ms tick** [V region costs; C combination]: own region's step, plus a keyframe save every 4
frames, plus (when another engaged player shares the region) one coalesced 5-frame rollback per tick, as the
ball-pit run measured (59 rollbacks/s, about 5 deep). **Memory** [C]: live world plus 4 keyframes, 5 x the
measured image. Build `f64-nv` (the recommended configuration):

| Players | Layout | Bodies simulated per player | CPU per tick | Memory |
|---|---|---|---|---|
| 2 / 16 / 64 / 256 | spread | 126 | **0.12 ms** | **1.5 MB** |
| 16 / 64 / 256 | groups of 8 | 1,008 | 6.8 ms | 11.6 MB |
| 16 | clustered = unchunked | 2,016 | 12.6 ms | 23 MB |
| 64 | clustered = unchunked | 8,064 | 67 ms | 92 MB |
| 256 | clustered = unchunked | 32,256 | about 300 ms [C] (282-635 ms across runs) | 368 MB |

Unpatched `f64` for comparison: 0.14 ms spread; 588 ms (f32) for 256 unchunked.

**Measured region costs (f64-nv):**

| Pits | Bodies | Awake | Step | Save | Restore | Restore + 5 steps | Image | lz4 |
|---|---|---|---|---|---|---|---|---|
| 1 | 126 | 71 | 0.065 ms | 0.20 ms | 0.18 ms | 0.73 ms | 300 kB | 97 kB |
| 8 | 1,008 | 366 | 0.47 ms | 1.5 ms | 1.5 ms | 5.9 ms | 2.3 MB | 672 kB |
| 16 | 2,016 | 680 | 0.73 ms | 3.0 ms | 2.6 ms | 11.1 ms | 4.6 MB | 1.3 MB |
| 64 | 8,064 | 2,598 | 6.0 ms | 12.7 ms | 12.3 ms | 58 ms | 18.5 MB | 5.2 MB |
| 256 | 32,256 | 10,401 | 31 ms | 53 ms | 54 ms | 265 ms | 73.7 MB | 20.5 MB |

**Bandwidth received per player** [C, from the design's measured encodings (7.6)]:
* a hand-target entry is 44 B;
* an ack for an n-member log is 4 + 25n B, sent every 2 frames;
* 52 B of IP/UDP/AEAD overhead per datagram;
* every other member of the region streams hand targets at 60 Hz;
* the world log carries interest anchors at 1 Hz per member and acks at 2 Hz.

| Players | Region tier, spread | Region tier, groups of 8 | World tier, full-mesh acks | World tier, compact 100 B acks |
|---|---|---|---|---|
| 2 | 0 | 8.9 kB/s (2 members) | 0.3 kB/s | 0.4 kB/s |
| 16 | 0 | 94 kB/s | 15 kB/s | 6 kB/s |
| 64 | 0 | 94 kB/s | 216 kB/s | 26 kB/s |
| 256 | 0 | 94 kB/s | **3.3 MB/s** | 106 kB/s |
| 256, clustered or unchunked | 51 MB/s | — | 3.3 MB/s | — |

* **Spread players cost each other almost nothing:** per-player CPU and memory are flat from 2 to 256
  players: about 2,500 times less CPU than unchunked at 256, using the computed unchunked figure.
  Compute only: liveness coupling through the logs is modelled separately in section 13.
* **What still grows with N is the world log.** With today's ack format (one 25 B line per member) it reaches
  3.3 MB/s per player at 256. Compact acks keep it at about 100 kB/s; gossip would keep it near constant [I].
  The world log needs compact acks beyond about 64 members.
* **Clustering cannot be optimized away by regions.** 256 engaged players in one place cost about 300 ms per tick [C] and
  51 MB/s each. A region needs a cap on engaged members, or else input delay and coarser LOD (D62).

## 9. Streaming [V]

A player crosses procedurally generated content for 20 s.
* **Content:** 24 objects per ground cell at levels -9 to +3 (cells 2 m to 8 km, radii 2 cm to 340 m);
  about 25 million objects lie within 1 km [C].
* **Relevance:** `|p - o| <= 24 r`, evaluated every 8 frames with look-ahead.
* **Player:** at 1 m height up to 15 m/s, at 40 m height for 150 and 1,500 m/s.

Steady state, after 2 s (`f64-nv`, then `f64` unpatched). **The p99 and max columns did not reproduce on a
loaded machine.** The reviewer measured p99 of 1.3-6.5 ms and max of 4-65 ms, and means within about 2x.
Treat the tails as unverified until a quiet-machine rerun:

| Speed | Active bodies, mean / max | Frame total, mean / p99 / max | Bodies created + destroyed per s | Generation | Query |
|---|---|---|---|---|---|
| 1.5 m/s | 567 / 583 | 0.17 / 0.47 / 0.92 ms (unpatched 0.23 / 0.86 / 1.13) | 59 + 61 | 0.17 ms/s | 0.87 ms/s |
| 15 m/s | 611 / 641 | 0.18 / 0.72 / 1.16 ms (0.45 / 3.2 / 4.2) | 442 + 440 | 0.57 ms/s | 0.88 ms/s |
| 150 m/s | 331 / 356 | 0.10 / 0.52 / 2.8 ms (0.17 / 1.0 / 2.9) | 127 + 128 | 1.5 ms/s | 0.60 ms/s |
| 1,500 m/s | 385 / 430 | 0.10 / 0.93 / 5.5 ms (0.37 / 3.7 / 8.3) | 816 + 816 | 3.0 ms/s | 0.56 ms/s |

* Size-based relevance simulates about 600 of 25 million objects within 1 km.
* Initial load is 1.5-2 ms (patched) or 2-3.5 ms (unpatched).
* **The f32 build needs region rebasing:** 7 rebases at 1,500 m/s, one every 4 km, at 0.85 ms each patched
  (3.9 ms unpatched). The f64 build needs none here.
* **Records vs images:**
  * a dormant record is 101 B per body; a box3d image is 705-741 B per body for these sleeping cubes, and
    1.8-2.0 KB for active piles;
  * activating 1,000 bodies from records takes 2.0-2.4 ms (patched) against 1.0-1.1 ms to restore an image;
  * freezing them takes 0.03 ms to read plus 0.17-0.22 ms to destroy.

## 10. Avatar culling (R17) [V CPU, C GPU proxies and bandwidth]

A synthetic avatar:
* 20,000 vertices x 4 weights from 60 bones;
* 8 dynamic-bone chains of 10 joints;
* 800 particles;
* 2,000 operations of presentation logic.

It is evaluated on the CPU per frame for every avatar at its tier. Viewer at the centre, 110° field of view.
GPU work is not timed; the proxies are skinned vertices, draw calls, lights and particles. Pose bandwidth uses
the per-tier rates in 26.2.

| Crowd | Policy | CPU per frame | Skinned vertices | Draws | Lights | Particles | Pose stream received |
|---|---|---|---|---|---|---|---|
| 64 in 100 x 100 m | none | 8.9 ms | 1.28 M | 256 | 128 | 51,200 | 762 kB/s |
| | distance + frustum + budget | **1.1 ms** | 166 k | 86 | 8 | 5,200 | **119 kB/s** |
| 256 in 30 x 30 m (packed) | none | 34.9 ms | 5.12 M | 1,024 | 512 | 204,800 | 3.0 MB/s |
| | distance | 29.9 ms | 3.99 M | 836 | 418 | 148,400 | 2.5 MB/s |
| | distance + frustum | 11.1 ms | 1.46 M | 470 | 152 | 55,200 | 931 kB/s |
| | + budget (16 full avatars, 8 lights) | **6.6 ms** | 912 k | 378 | 8 | 27,600 | **657 kB/s** |
| 256 in 200 x 200 m | none | 38.2 ms | 5.12 M | 1,024 | 512 | 204,800 | 3.0 MB/s |
| | distance + frustum + budget | **2.6-3.1 ms** | 342 k | 283 | 8 | 5,800 | **277 kB/s** |
| 256 in 200 m, 10% tiny and 10% giant | none | 47 ms | 5.12 M | 1,024 | 512 | 204,800 | 3.0 MB/s |
| | distance + frustum + budget | **3.9-4.5 ms** | 470 k | 291 | 8 | 12,600 | **344 kB/s** |
| 256 in 2 x 2 km | none | 38.5 ms | 5.12 M | 1,024 | 512 | 204,800 | 3.0 MB/s |
| | distance + frustum + budget | **0.07-0.09 ms** | 8 k | 152 | 0 | 0 | **29 kB/s** |

* In typical crowds, culling saves 8 to 15 times the CPU, 7 to 15 times the skinned vertices, and 6 to 11
  times the pose bandwidth **against no culling**.
* Against a **frustum-only** baseline (review R17-3; rerun at low priority, so absolute CPU is higher):

  | Crowd | Frustum only | R17 tiers | Gain |
  |---|---|---|---|
  | 64 in 100 m | 2.9 ms | 1.4 ms | 2x |
  | Packed | 16.4 ms | 8.6 ms | 1.9x |
  | 200 m | 15.6 ms | 3.2 ms | 4.9x |
  | Mixed scales | 16.1 ms | 5.1 ms | 3.2x |
  | 2 km | 12.3 ms | 0.19 ms | 65x |

* **The sim is untouched [V]:** with a real region of 4 ball pits and hand colliders, a culling pass taking
  `&World` every frame, under three policies (no pass, culling off, full culling), left the 300-frame hash
  digest identical: `7418e4f8aeb60229`.
* **Upload for an avatar every viewer sees at T0** [C]: 179 kB/s to 15 viewers, 750 kB/s to 63, and
  3.0 MB/s to 255. Hence the relay fan-out in 26.3.
* In the packed crowd the budget does the work: 5.3 times.
* In a spread crowd almost everything is an impostor.
* Mixed scales behave as intended: giants are kept in detail from further away and tiny players are culled
  sooner, by the same angular-size rule.
* Single-run CPU numbers vary by about ±20% under the machine's load.

## 11. What was not measured

* The two-tier log, groups, merge/split protocol and region state transfer are designed (25.5), not built.
  They also depend on the round-2 protocol, which is still under review.
  * The per-player bandwidth is a calculation from the design's measured encodings.
  * Section 13 models the dependency rule, not the protocol.
* Wake propagation (25.4 rule 3) and the mixed-scale one-way proxies (25.6) are not prototyped.
* Section 12 tests support closure and awake-stays-active in one scene only.
* Sections 13 and 14 model the dependency and safety rules, not the protocol; 25.5.8 (equivocation) is not modelled.
* The quiet-machine rerun of the streaming tails is pending; the user's PC is in use for VR.
* GPU cost of avatars: no renderer exists yet.
* Real-gravity CPU at small scale is inferred from the sub-step count. The tower sleeps too fast to time it
  awake.
* Cross-machine determinism of the f64 build: the goldens exist in box3d-rust [S], but they were not run on a
  second machine here.

## 12. Support closure and awake bodies (review R16-2) [V]

**Scene (`closure`):**
* a 0.9 m crate just placed on a bed of 144 stones of 3 cm, each relevant only within 0.96 m;
* a 1.5 cm coin knocked off a 1 m table at 1 m/s;
* the player's anchor waits 20 m away, then walks in at 4 m/s and stops beside the crate.

| Rules | Crate (rests at y = 0.51) | Stones inside the crate at the end | Coin | Bodies active (max) |
|---|---|---|---|---|
| Relevance + wake only (as of 45d4940) | falls to 0.45: the dormant stones are not in the world | **81** (the stones activate inside it later and overlap silently) | **frozen in mid-air at 0.94 m** | 49 |
| + awake stays active until asleep, + one-hop support closure | stays at 0.49-0.51 (the stones shift 2 cm under it) | 0 | lands on the floor (y = 0.015) | 134 |

Closure costs bodies (134 vs 49 here), because the stones under a relevant crate are simulated. Being
one-hop, it does not pull in the rest of the bed.

## 13. Liveness: does one stalled member slow distant regions? (reviews R16-1, N3) [C, rule-level model]

`pe-spike-chunks liveness` simulates the two dependency rules frame by frame. Inputs:
* 60 Hz;
* region confirmation lag 9 frames (8.5 at ed347ac 6.4, rounded up) [I, @engine-architect's measurement];
* world-log confirmation lag 66 frames [I];
* speculation limit 120 frames;
* in-region eviction 132 frames after a member's acks stop;
* **for a one-member region, eviction by world-log view change, 228 frames** (review N3);
* `v_max` 100 m/s and `D_merge` 3 km;
* 50 ms to learn a neighbour's confirmed frame.

One member stops acking for S seconds (10 s stands in for gone for good). The table counts frames that
**unrelated** regions (not containing that member) cannot simulate on time. Gaps are between
**footprints**; anchors are about 6 km further apart.

| Members, layout | Footprint gap | Stall | World log on the path (45d4940) | Neighbour bound (revised) |
|---|---|---|---|---|
| 64 or 256, alone or in groups of 8 | 3.5 or 4 km | 0.5 s | 0 | 0 |
| same | same | 1 s | every region loses 5 frames | 0 |
| same | same | 2 s | every region loses 65 frames | 0 |
| same | same | 3 s | every region loses 77 (groups of 8) / 125 (alone) frames | 0 |
| same | same | 10 s | every region loses 77 / 173 frames | 0 |
| same | 3.2 km (2 s of slack) | 2 / 3 / 10 s | as above | the 2 nearest lose 11 / 23 / 23 frames (groups of 8), 11 / 71 / 119 (alone) |

For 10 s stalls with 256 members alone, that is 44,115 region-frames lost under the old rule and none under
the new one. Raw output: `spikes/out/chunks/liveness.log`.

## 14. Safety: late merges and late reads under footprint steps, two-sided and concurrent growth, creation and scale bands (reviews N1, M1, P1, P2, Q1, Q2) [C, rule-level model]

`pe-spike-chunks safety` models two regions on a line. Both may grow toward each other, and both are
checked. A **late merge** is a merge frame that a region had already simulated past when it first held
every fact that fixes it. That would need a rollback, possibly below its confirmed frame, which design 6.4
forbids. In the cross-band cases the event is the first frame from which a human region must read a giant's
region (their footprints overlap, margin 0); only the reader is checked.

Rule sets (design 25.5.2 and 25.6 give the revised rules in full):
* **2a228c6.**
* **05d3cc0:** the growth clock, envelopes by stamp, creation at `stamp + Lambda` checked against the
  confirmed directory.
* **e10c905:**
  * creation at `stamp + S_create`, validated in world-log order against pending creations;
  * `J` = the reduction of the gap to a known region's possible footprint (review P2);
  * no bound between bands (review P1).
* **da3cd74:**
  * `J` = 0 only if every added point is at least `D_merge` from every known possible footprint;
  * the horizon gate, the start condition and the cross-band bound;
  * known regions only from relays in the region's own log.
* **Revised (reviews Q1, Q2):**
  * instant growth capped at `G_inst` (4 chunks), at most one per `T_h`, and counted in every possible
    footprint and in the creation buffer;
  * the known set includes the world-log directory confirmed through `s - S_create`.
  * "revised-no-start" is the same without the start condition (a control).

| Case | 2a228c6 | 05d3cc0 | e10c905 | da3cd74 | **Revised** |
|---|---|---|---|---|---|
| Chunk step, no stall (gap 4 km) | safe (360) | safe (975) | safe (360) | safe (975) | safe (975) |
| Chunk step toward R, q stall (gap 4 km) | **late** (360) | safe (975) | **late** (360) | safe (975) | safe (975) |
| Chunk step toward R, R stall (gap 4 km) | safe (360) | safe (975) | safe (360) | safe (975) | safe (975) |
| Chunk step toward R, R stall, R walks toward q (gap 4 km) | safe (30) | safe (645) | safe (30) | safe (645) | safe (645) |
| Chunk step toward R, q stall, R walks toward q (gap 8 km) | safe (1,875) | safe (2,490) | safe (1,875) | safe (2,490) | safe (2,490) |
| Chunk step toward R, R stall, R walks toward q (gap 8 km) | safe (1,875) | safe (2,490) | safe (1,875) | safe (2,490) | safe (2,490) |
| Scale x8 (+14 km), pull every 60 s only (gap 10 km) | **late** (1,030) | safe (9,632) | **late** (2,418) | safe (9,632) | safe (9,632; R pauses 11 frames) |
| Scale x8 (+14 km), pull every 60 s or on demand (gap 10 km) | **late** (1,030) | safe (9,632) | **late** (2,418) | safe (9,632) | safe (9,632) |
| Joiner at q's near edge (+3 km), pull every 60 s only (gap 5 km) | **late** (1,030) | safe (2,874) | **late** (1,030) | safe (2,874) | safe (2,874; R pauses 2,411 frames) |
| Joiner at q's near edge (+3 km), pull every 60 s or on demand (gap 5 km) | **late** (1,030) | safe (2,874) | **late** (1,030) | safe (2,874) | safe (2,874) |
| Walker at v_max toward R, 12 chunk steps, q stall mid-way (gap 12 km) | safe (5,250) | safe (5,865) | safe (5,250) | safe (5,865) | safe (5,865) |
| Chunk step every 5 s from 20 km toward R, q stall at frame 1500 | safe (5,030) | safe (10,685) | safe (7,610) | safe (7,535) | safe (10,370) |
| Chunk step every 5 s from 20 km toward R, q stall at frame 4000 | safe (5,030) | safe (10,685) | safe (7,610) | safe (7,535) | safe (10,370) |
| Region created 4.5 km from R, then walks toward it | **late** (1,945) | joins | joins | joins | joins |
| Region created 3.5 km from R, then walks toward it | **late** (1,330) | joins | joins | joins | joins |
| Region created 4.5 km from R and walks toward it, world log stalled 5 s | **late** (1,945) | joins | joins | joins | joins |
| Region created 4.5 km from R and walks toward it, world log stalled 12 s | **late** (1,945) | joins | joins | joins | joins |
| Both walk toward each other, q created at the closest allowed gap, v_max 100 m/s, S_create 10 s | **late** (645) | safe (1,260) | safe (1,231) | safe (1,846) | safe (3,076) |
| Both walk toward each other, q created at the closest allowed gap, v_max 100 m/s, S_create 10 s, world log stalled 8 s | **late** (645) | safe (1,260) | safe (1,231) | safe (1,846) | safe (3,076) |
| Both walk toward each other, q created at the closest allowed gap, v_max 200 m/s, S_create 10 s | **late** (646) | safe (954) | safe (1,200) | safe (1,262) | safe (2,494) |
| Both walk toward each other, q created at the closest allowed gap, v_max 200 m/s, S_create 10 s, world log stalled 8 s | **late** (646) | **late** (954) | safe (1,200) | safe (1,200) | safe (2,494) |
| Both walk toward each other, q created at the closest allowed gap, v_max 100 m/s, S_create 20 s, world log stalled 8 s | **late** (1,246) | safe (1,861) | safe (1,831) | safe (2,446) | safe (3,676) |
| Two regions created at the same spawn point, 10 frames apart | **late** (640) | **late** (640) | joins | joins | joins |
| Two-member region grows instantly toward a region being created at the closest allowed gap, then stalls 30 s | **late** (650) | safe (2,494) | **late** (1,200) | safe (1,200) | safe |
| Both scale up at once, +9 chunks each toward the other (gap 20.5 km) | **late** (1,035) | safe (6,565) | safe (6,565) | **late** (1,035) | safe (6,565) |
| Both scale up at once, +9 chunks each toward the other (gap 20.5 km), R stalls | **late** (1,035) | safe (6,565) | safe (6,565) | **late** (1,035) | safe (6,565) |
| Chained: q grows +4 chunks three times 5 frames apart while R scales up +9 chunks (gap 24 km), R stalls | **late** (1,040) | safe (8,404) | safe (8,404) | **late** (1,040) | safe (5,951) |
| R scales up +8 chunks toward a new region after it exists but before R's log relays it, then stalls 30 s | **late** (1,330) | safe (6,246) | **late** (1,330) | **late** (1,330) | safe (6,246) |
| Giant region walks toward a human region from 1,000 km at its v_max, L stalls on the way | n/a | n/a | safe (4,535) | safe (5,150) | safe (5,150) |
| Giant scales up x8 (+14 of its 131 km chunks) 1,500 km from a human region, L stalled | n/a | n/a | **late** (430) | safe (9,032) | safe (6,574) |
| Giant steps once onto a human region 100 km away, L stalled | n/a | n/a | **late** (430) | safe (1,045) | safe (1,045) |
| Giant scales up +9 of its chunks to land 8 km from a human region that scales up +9 chunks at once, L stalled | n/a | n/a | **late** (1,030) | **late** (1,030) | safe (6,560) |
| Two-member giant region walks toward a human region from 1,000 km, L stalled 30 s (never evicted) | n/a | n/a | **late** (4,535) | safe (5,150) | safe (5,150; R pauses 196 frames) |

Cells give the merge or read frame in brackets. "joins" means the creation became a join, so no second
region exists.

* **33 cases of steps toward a neighbour, two-sided and concurrent growth, creation and cross-band reads,
  on a line. Late: 20 of 28 under 2a228c6, 2 of 28 under 05d3cc0,
  11 of 33 under e10c905, 5 of 33 under da3cd74, 0 of 33
  under the revised rules** (0 of 33 without the start condition). The cross-band rows
  do not apply before e10c905.
* The da3cd74 failures are reviews Q1 (two concurrent or chained instant growths, also across bands) and
  Q2 (a region not yet relayed into the grower's log).
* **Model fix in this round:** the on-demand pull used to trigger every frame, because it compared a bound
  already clamped to the present. With that fixed, more 2a228c6 and e10c905 rows are late than before (for
  example "scale x8 ... or on demand"). The 05d3cc0 and revised conclusions did not change.
* The revised rules place created regions at their own closest allowed gap, which includes `G_inst`, and
  the matching creation case grows R by the full cap.
* The 05d3cc0 failures reproduce @chunks-reviewer's `safety2_model.py`, and the da3cd74 Q1 rows their
  `concurrent_instant_model.py`.

* The `liveness` model has no horizon gate. It cannot bind there, because every modelled stall ends within
  15 s, well under `T_h` (60 s) [I].

**World-log stall budget** (`liveness-gate` rows, review M4). Setup: the region-directory gate is on, one
member is silent for 15 s, footprints are 4 km apart. Frames lost by unrelated regions, against how fast the
world log evicts or demotes that member:

| Members | 3.8 s | 4 s | 8 s | 12 s |
|---|---|---|---|---|
| 64 alone | 0 | 0 | 0 | **every region, up to 188 frames** |
| 256 alone | 0 | 0 | 0 | **every region, up to 188 frames** |
| 256 in groups of 8 | 0 | 0 | 0 | **every region, up to 185 frames** |

Budget: world-log eviction or demotion within `S_create` minus the world-log lag (about 8.9 s).

@engine-architect's round-3 view change (design 10.8, cecda4b) evicts one silent member in about 3.1-3.3 s
in their crash runs [V, theirs], inside the budget. It does not yet handle two members silent at once [I].

## 15. Support closure on heaps and thrown objects (review N7) [V]

`pe-spike-chunks closure2`, each scene with an all-dynamic control.

| Scene | All-dynamic control | One hop, static bounds | Swept first hop + static second hop |
|---|---|---|---|
| 20 cm crate on a 3-layer heap of 5 cm gravel: final height (rests at 0.251) | 0.242; no gravel displaced | **0.197**; 68 gravel cubes sank | **0.243**; none sank; 124 static proxies |
| The crate thrown at 2 m/s onto 1.2 cm gravel (below r/16): final height (rests at 0.112), gravel inside | 0.106, 16 | 0.105, 2 | 0.106, 21 |

* **The second hop fixes the heap**, matching the control within 1 mm.
* **The thrown case is the solver's limit, not the rule's.** Gravel under 1/16 of the crate's size has a
  mass ratio over 4,096. Box3D lets the crate sink into it even when everything is simulated, and all three
  variants end within 1 mm.
* A first version (0.9 m crate on 2.5 cm gravel, mass ratio 47,000) sank to the ground even in the control,
  so it was replaced.
