# Primordial Engine

A peer-to-peer game engine in Rust for shared physics worlds, with VR as the end goal. There is no
server: every peer runs the same deterministic fixed-step simulation, and peers agree on one history
through a signed session log with rollback. Physics is Box3D, via a fork of the pure-Rust `box3d-rust`
port. Graphics will be wgpu, game logic live-editable Rust compiled to WASM, VR through OpenXR. AI agents
are first-class users: every peer exposes an MCP server, so an agent can read the world and act in it.

## Status

**Re-founding, milestone M0.** An independent review on 2026-11-10 found the deterministic base sound and the
work above it the wrong shape for what remains, and the project owner ordered a rewrite above the base. The
plan is [docs/refound-plan.md](docs/refound-plan.md): a vertical slice first (see, touch in VR and edit through
an agent, between two machines), then authoring, sessions that survive real networks, regions, and the platform.
This is a research codebase, not a usable engine: there is no renderer, no VR and no game yet.

What is on `main` now:

| Path | What it is |
|---|---|
| `crates/pe-core` | Deterministic building blocks: ids, the frame clock, world coordinates, bit-exact maths, RNG and hashing |
| `crates/pe-sim` | The sim store: fixed-size component rows, snapshots and hashing |
| `crates/pe-physics` | The engine's interface to the Box3D fork: bodies bound to sim objects, deterministic stepping, state hashes |
| `crates/box3d-fork` | Our fork of box3d-rust 0.5.0 (MIT); see its [FORK.md](crates/box3d-fork/FORK.md) for every change |
| `docs/spec/` | Short specifications of the new subsystems, one per workstream |
| `archive/` | The old engine's session core, its harness and simulator, its MCP server and peer, and the physics spike, kept for reading while the new crates port from them. Not built. Removed at milestone M3 |
| `spikes/` | References with their own builds: the chunk benchmarks, the WASM guests, the iteration benchmark, a native-code audit of candidate dependencies |

**The prototype before the re-founding** (the signed session log with unanimous confirmation, the view
change, thin-link congestion control, the M1 demo of four peers over UDP, every evidence log) is kept at
the tag `archive/v0-prototype`. Its documents are in [docs/archive/](docs/archive).

## Documentation

* [docs/refound-plan.md](docs/refound-plan.md): the build plan, with its milestones, workstreams, tests and the
  owner's rulings.
* [docs/spec/](docs/spec): one short specification per subsystem.
* The other files in [docs/](docs) describe the kept base: the core and store (P2-1), the physics interface
  (P2-2), the determinism matrix, parallel physics, double precision and the van der Waals contact radius.
* [docs/archive/](docs/archive): the old design document and the old engine's studies.

Results in the docs are labelled: **[V]** or "measured" for something that was run, **[I]** or
"inferred" for a judgement.

## Building and testing

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.99.0, with the `wasm32-unknown-unknown`
target), so that every machine produces byte-identical WASM. rustup installs it on first use. The same
commands work on Windows (Git Bash for the scripts) and on Linux.

```bash
cargo build --workspace --release
cargo test -p pe-core -p pe-sim -p pe-physics --release
cargo test -p box3d-rust --release   # the fork's own suite
```

The physics records that pe-physics's tests reproduce bit for bit are in `crates/pe-physics/tests/records/`.

## How this was made

The design, the code and the documentation were written by a team of AI agents
coordinated in Orgtree, a tool for running organisations of agents, with the project owner making the
design decisions. That explains some conventions in the docs and commit messages:

* `@name` refers to an agent of that team (for example @engine-architect, who wrote the design).
* "User ruling", "D" and "V" numbers, and timestamps such as "19:58Z" refer to the owner's design
  decisions and when they were made.
* Message ids (`m…`) and `@item:` references point into the team's internal records, which are not part
  of this repository.

## Licence

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)), or
* MIT licence ([LICENSE-MIT](LICENSE-MIT)),

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
work by you, as defined in the Apache-2.0 licence, shall be dual licensed as above, without any
additional terms or conditions.

`crates/box3d-fork` is the exception: it is MIT only, under its own [LICENSE](crates/box3d-fork/LICENSE).

## Credits

* [Box3D](https://github.com/erincatto/box3d) by Erin Catto: the physics engine (MIT).
* [box3d-rust](https://github.com/larsbrubaker/box3d-rust) by Lars Brubaker: the pure-Rust port of Box3D,
  forked at 0.5.0 in `crates/box3d-fork` (MIT).
* The Rust crates listed in `Cargo.lock`, under their own licences (all permissive: MIT, Apache-2.0,
  BSD, ISC, Zlib, Unlicense, CC0, Unicode-3.0).
