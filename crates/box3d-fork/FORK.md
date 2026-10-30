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

### Release validators: measured effect

Release, the engine pile scene, 5 interleaved runs of `pe-spike-physics bench`, medians. Raw data is in
`spikes/out/fork/validate-gate-ab-win-release.txt`.

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

* With scaling off, the simulation is bit-identical to the published crate. This was checked
  against the 4 C goldens, the 600-frame trace, the snapshot-image bytes and the crate's own test
  suite. See `docs/vdw-simulation.md`.
* The radius hook is a plain `fn` pointer. It is not snapshotted, the same as the other Box3D
  callbacks. The parameters are snapshotted.
* Recordings replay the scaling parameters of the recorded world, because a replay is seeded
  from a frame-0 world snapshot and keyframes are snapshots too (test
  `recording_replays_scaling_from_world_def`). A `world_set_contact_recycle_scaling` call made
  during a recording is not captured as an op.
