# D58: Box3D double precision on

Author: @vdw-physics. Follow-up to P2-2, docket
@item:primordial-engine/p2-2-follow-up-turn-box3d-double-precision-on-d5. Branch
`p2/double-precision`. Status: in progress (the sim-core part waits for `viewchange` on main).

Tags: **[V]** tested here, **[I]** inferred or proposed.

## What changes

D58 (design 25.12, settled by the user) turns on the fork's `double-precision` feature: Box3D's
`Pos` (body positions, world points) becomes `f64`; rotations, velocities and collision math stay
`f32`. It is the fork's **default** feature, so every build of every workspace crate has it: a
feature enabled by only some crates would make results depend on which crates a cargo command
builds together. The f32 build stays checkable: `cargo test -p box3d-rust --no-default-features`
(the fork alone) checks Box3D C's f32 goldens.

## The upstream reference survives

Box3D C publishes determinism goldens for both precisions (test_determinism.c); the fork's
determinism tests already carry both sets.

* [V] With double precision on, the fork's suite passes (571 + 2) and matches C's double goldens.
* [V] pe-physics' upstream configuration (contact recycling off) matches C's double goldens at
  1, 3 and 8 workers: ragdolls 263 / 0xA5423F98, wave pile 272 / 0xC8E5F3D6, query spawn 242 /
  0x489585E6 (59 hits, 0x5B4429DC), mesh drop 218 / 0xD378CF04.
* Every check that holds a golden selects it with `box3d_rust::core::is_double_precision()`, so
  the f32 references stay in the source and pass in an f32 build [V].

## Records

<!-- filled from the re-bless run -->

## Code that assumed f32 positions

* `spikes/sim-core`: `BodyState([u32; 13])` held position bits as f32. It becomes 16 words: the
  position as three f64, each two u32 words, low word first, then rotation and velocities as f32
  bits (agreed with @engine-architect; pe-session follows). Waits for `viewchange` on main.
* Position constructors computed in f32 widen with `as _`; widening is exact, so f32 inputs give
  the same bits in both precisions.

## Step-time cost (next to design 25.8)

Measured 2026-10-31 11:03Z on Windows, both heavy.sh slots held, no VR: the chunks spike's `ab`
benchmark (25.8's scenes and method: 30 blocks of 20 frames, 60 Hz, 4 sub-steps, 8 interleaved
rounds, per-block minimum over rounds averaged over blocks). Four builds in the same interleave:
box3d-rust 0.5.0 as published (25.8's setup, re-measured) and our fork at db2bc80 (fork default
f32 there), each without and with `double-precision`. Raw: `spikes/out/d58/ab-fork-vs-upstream.log`,
`ab-summary.txt` [measured].

| Scene | fork f32 | fork f64 | fork f64 cost | 0.5.0 f64 cost | fork vs 0.5.0 (f32 / f64) |
|---|---|---|---|---|---|
| 1,024-body pile | 0.928 ms | 0.962 ms | +3.7% | +3.8% | -6.4% / -6.5% |
| 2,048-body pile | 2.262 ms | 2.178 ms | -3.7% | +4.7% | -6.0% / -13.5% |
| 500-ball pit | 0.651 ms | 0.662 ms | +1.7% | +5.2% | -6.9% / -10.0% |

Per-round means varied far more than these differences (ball pit 0.67-1.03 ms, 2,048 pile
2.25-4.97 ms), so a few percent is within the noise; the 2,048-body f64 "gain" is noise. Result:
double precision costs at most a few percent of step time on our fork, inside 25.8's 2-10%.

Build: in `spikes/chunks`, `cargo build --release` with and without `--features f64`, and the same
with `--config 'patch.crates-io.box3d-rust.path="<checkout>/crates/box3d-fork"'`, from a checkout
whose fork default is f32 (db2bc80); then `pe-spike-chunks ab` alternately in the four builds.
Summarize with the estimator of `scripts/chunks_summary.py`.

## Reproduce

```bash
nice -n 19 bash scripts/rebless_physics_records.sh   # every affected record, from source
cargo test -p box3d-rust --release --no-default-features   # the fork in f32: C's f32 goldens
```
