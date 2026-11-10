# D58: Box3D double precision on

> **Evidence paths** (since the M0 archive, 2026-11-10): paths under `spikes/out/` in this document refer to the
> tag `archive/v0-prototype`, where those logs are kept. Main no longer carries them; the physics records the
> tests read moved to `crates/pe-physics/tests/records/`.

Author: @vdw-physics. Follow-up to P2-2, docket
@item:primordial-engine/p2-2-follow-up-turn-box3d-double-precision-on-d5. Branch
`p2/double-precision`. Status: records re-blessed, P2-6 matrix PASS; in review.

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

Re-recorded 2026-10-31 by one run of `scripts/rebless_physics_records.sh` on the merged tree
(main f65851e merged into this branch), Windows release and debug plus WSL Linux; no log edited by
hand. Results [V]:

* `spikes/out/vdw/`: goldens and rollback T1-T5 PASS; 600-frame trace digest
  `cc60208f048dd6d7` (vdW branch) and `e8a2386a0f2ed7d6` (upstream configuration), release ==
  debug; snapshot images digest `c6f338649541031d`. `check_par_determinism.sh` PASS at 1 and 4
  threads, release and debug.
* `run_determinism.sh`, every part PASS: `integration/`, `session-sim/` (suite), `r18/`,
  `p2p-v2/` (three UDP runs, peers and offline replays equal), `r15/hooks.log`,
  `perturbation-history/`, guest module hashes (each of the 6 modules byte-identical at two
  checkout paths; they change because the guests link sim-core's 16-word state), MCP session 10/10.
  `wasm-hotswap-lazy.log` keeps its two FAILs (D1b, D3: fuel under lazy compilation); main has the
  same two, so they predate D58.
* `p2-2/`: 68 RESULT lines on each of Windows/Linux x debug/release, all equal and matching
  `spikes/out/vdw`; fork suite 573 + 2 pass (`fork-suite-windows-release.txt`).
* `session-sim/`: 92 of 92 scenario logs reproduce, every one ALL EQUAL, 0 false checks, 0
  non-finite states.

One scenario moved its check window. A carried state is 16 words instead of 13, so an entry that
carries one is 12 bytes larger (fault-capped: 34 entries, 100 B to 112 B on average). On
fault-capped's 12 kbit/s link the capped member now confirms to frame 516 instead of 538, so its
`check_world` covers frames 0..510 (it was 0..530, past the confirmed history). The other fault
scenarios keep their windows. Bandwidth cost [V]: +12 B per carried state on the wire, nothing
else.

Not re-recorded: records that are evidence of a named earlier commit and were built in f32 (the
phase-1 files at the top of `spikes/out/`, `chunks/`, `fork/`, `p2-1/`, `p2-7/`, `par/`,
`iter-bench/`, `mcp/`, `d58/`).

P2-6 matrix at 063f0ce [V]: `scripts/run_matrix.sh` on this PC (Windows), its WSL (glibc and
musl) and the Intel laptop (Windows, over ssh at idle priority): every column's determinism set
passes and all 124 outputs are equal on the 4 machines (`spikes/out/matrix/matrix.txt`).

## Code that assumed f32 positions

* `spikes/sim-core`: `BodyState([u32; 13])` held position bits as f32. It becomes 16 words: the
  position as three f64, each two u32 words, low word first, then rotation and velocities as f32
  bits (agreed with @engine-architect; pe-session follows).
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
