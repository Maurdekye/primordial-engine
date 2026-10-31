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

## Reproduce

```bash
nice -n 19 bash scripts/rebless_physics_records.sh   # every affected record, from source
cargo test -p box3d-rust --release --no-default-features   # the fork in f32: C's f32 goldens
```
