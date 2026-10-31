# Primordial Engine

A peer-to-peer game engine in Rust for shared physics worlds, with VR as the end goal. There is no
server: every peer runs the same deterministic fixed-step simulation, and peers agree on one history
through a signed session log with rollback. Physics is Box3D, via a fork of the pure-Rust `box3d-rust`
port. Graphics will be wgpu, game logic live-editable Rust compiled to WASM, VR through OpenXR. AI agents
are first-class users: every peer exposes an MCP server, so an agent can read the world and act in it.

## Status

**Phase 2 of 6: the deterministic core and the session protocol. Milestone M1 has not been reached yet.**
This is a research codebase, not a usable engine: there is no renderer, no VR and no game yet.

M1 (design 23.2) is 2-4 desktop peers on a LAN, Windows and Linux mixed, running a physics sandbox.
Interactions are confirmed by the other members, gaps are repaired, equivocation is detected, a member
that drops out is removed by the view change, and an AI agent drives one peer over MCP.

What is on `main` now:

| Path | What it is |
|---|---|
| `crates/pe-core` | Deterministic building blocks: ids, the frame clock, world coordinates, bit-exact maths, RNG and hashing |
| `crates/pe-sim` | The sim store: fixed-size component rows, snapshots and hashing |
| `crates/pe-physics` | The engine's interface to the Box3D fork: bodies bound to sim objects, deterministic stepping, state hashes |
| `crates/box3d-fork` | Our fork of box3d-rust 0.5.0 (MIT); see its [FORK.md](crates/box3d-fork/FORK.md) for every change |
| `crates/pe-mcp` | The engine's MCP server (rmcp): the tool catalogue, tokens and scopes, stdio, named pipe or loopback HTTP |
| `tools/session-sim` | An in-process session simulator with network faults, used for regression scenarios |
| `spikes/*` | Phase-1 feasibility spikes: physics goldens and rollback, a two-peer UDP session, WASM hot-swap, iteration-latency benchmarks, a native-code audit of the dependency stack |

The session protocol crate (`pe-session`) and the networked peer (`pe-app`) are under review on
branches and are not on `main` yet. Phases 3-6 (internet play, rendering, live editing with in-game
agent panels, VR and avatars, discovery at scale) are designed but not started.

## Documentation

* [docs/design.md](docs/design.md): the architecture, the requirements it traces, the phased plan (23.2)
  and the decisions taken. It is long; sections 2 (architecture at a glance) and 23 (plan and risks) are
  the best starting points.
* [docs/spike-report.md](docs/spike-report.md): what the phase-1 spikes ran and measured.
* [docs/decisions-open.md](docs/decisions-open.md): decisions still open.
* The other files in [docs/](docs) are results and studies for individual work packages.

Results in the docs are labelled: **[V]** or "measured" for something that was run, **[I]** or
"inferred" for a judgement.

## Building and testing

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.99.0, with the `wasm32-unknown-unknown`
target), so that every machine produces byte-identical WASM. rustup installs it on first use. The same
commands work on Windows (Git Bash for the scripts) and on Linux.

```bash
cargo build --workspace --release
cargo test -p pe-core -p pe-sim -p pe-physics -p pe-mcp --release

# Phase-1 spikes
./target/release/pe-spike-physics goldens
./target/release/pe-spike-physics rollback
bash scripts/run_session_sim.sh
bash scripts/build_wasm_guests.sh && ./target/release/pe-spike-wasm-hotswap

# The whole determinism set (spike report 5j). It is heavy: run it at low priority.
bash scripts/run_determinism.sh
```

`scripts/run_matrix.sh` runs the determinism set on several machines (Windows, Linux under WSL, a
second Windows PC over ssh) and compares every output. Machine-specific settings go in
`scripts/matrix.local.sh`; see `scripts/matrix.local.example.sh`.

## The M1 demo (planned)

Four peers, on Windows and Linux, run a shared physics sandbox over UDP. Scripted loss and delay hit some
links, one member drops out mid-session and is removed by the view change, and an AI agent (Claude Code,
over MCP) drives one of the peers. At the end every peer's history is identical, and identical to an
offline replay. The steps to run it will be added here when `pe-app` lands on `main`.

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
  BSD, ISC, Zlib, Unlicense, CC0, Unicode-3.0). Among them: [rmcp](https://crates.io/crates/rmcp) for MCP,
  [wasmi](https://crates.io/crates/wasmi) for WASM and [wgpu](https://crates.io/crates/wgpu) for graphics.
