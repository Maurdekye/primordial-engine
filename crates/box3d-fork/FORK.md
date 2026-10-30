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
| `src/recycle_scale.rs` (new) | `ContactRecycleScaling` parameters, `RecycleLengthCombine`, `RecyclePairInput`, the `ContactRecycleRadiusFn` hook and the default formula |
| `src/lib.rs` | `pub mod recycle_scale` |
| `src/types/world.rs` | `WorldDef::contact_recycle_scaling`, disabled by default |
| `src/world/mod.rs` | `World::contact_recycle_scaling` and `World::contact_recycle_radius_fcn` |
| `src/world/api.rs` | `world_set/get_contact_recycle_scaling` and `world_set_contact_recycle_radius_fn` |
| `src/contact/collide.rs` | When scaling is on, `collide_task` computes a per-pair tolerance inside the recycle branch. When it is off, the upstream path runs unchanged |
| `src/recording/snapshot/pods.rs` | World-config flag bit `0x20` plus 13 bytes of parameters, written only when scaling is on |
| `src/recycle_scale_tests.rs` (new) | Tests for the formula, the hook, the 1 cm ledge regression and the snapshot round trip |

## Invariants

* With scaling off, the simulation is bit-identical to the published crate. This was checked
  against the 4 C goldens, the 600-frame trace, the snapshot-image bytes and the crate's own test
  suite. See `docs/vdw-simulation.md`.
* The radius hook is a plain `fn` pointer. It is not snapshotted, the same as the other Box3D
  callbacks. The parameters are snapshotted.
* The recording/replay system (`recording::session`) does not capture the scaling parameters. A
  replayed recording runs with the parameters of its `WorldDef`.
