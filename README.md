# Primordial Engine

A peer-to-peer game engine in Rust for shared physics worlds, with VR as the end goal. There is no
server: every peer runs the same deterministic fixed-step simulation, and peers agree on one history
through a signed session log with rollback. Physics is Box3D, via a fork of the pure-Rust `box3d-rust`
port. Graphics will be wgpu, game logic live-editable Rust compiled to WASM, VR through OpenXR. AI agents
are first-class users: every peer exposes an MCP server, so an agent can read the world and act in it.

## Status

**Phase 2 of 6: the deterministic core and the session protocol. Milestone M1 is reached.**
This is a research codebase, not a usable engine: there is no renderer, no VR and no game yet.

M1 (design 23.2) is 2-4 desktop peers on a LAN, Windows and Linux mixed, running a physics sandbox.
Interactions are confirmed by the other members, gaps are repaired, equivocation is detected, a member
that drops out is removed by the view change, and an AI agent drives one peer over MCP. The runs that
show it are in [spikes/out/m1](spikes/out/m1), and [The M1 demo](#the-m1-demo) below shows how to run it:
- four peers across two machines, Windows and Linux
  ([two-machine-windows-wsl-laptop](spikes/out/m1/two-machine-windows-wsl-laptop));
- equivocation detected and the equivocator removed
  ([late](spikes/out/m1/equivocation-loopback-windows),
  [split](spikes/out/m1/equivocation-split-held-loopback-windows));
- an AI agent driving a peer on `main` (aa2680b) ([agent-windows-main](spikes/out/m1/agent-windows-main)).

What is on `main` now:

| Path | What it is |
|---|---|
| `crates/pe-core` | Deterministic building blocks: ids, the frame clock, world coordinates, bit-exact maths, RNG and hashing |
| `crates/pe-sim` | The sim store: fixed-size component rows, snapshots and hashing |
| `crates/pe-physics` | The engine's interface to the Box3D fork: bodies bound to sim objects, deterministic stepping, state hashes |
| `crates/box3d-fork` | Our fork of box3d-rust 0.5.0 (MIT); see its [FORK.md](crates/box3d-fork/FORK.md) for every change |
| `crates/pe-session` | The session protocol: the signed session log, confirmation and repair, rollback, view change, R18 congestion control, as a sans-IO `SessionCore` |
| `crates/pe-session-harness` | An in-process network harness for pe-session's scenarios and regression runs |
| `crates/pe-app` | The headless M1 peer: one member of a session over signed UDP, with clock sync and pe-mcp attached; see its [README](crates/pe-app/README.md) |
| `crates/pe-mcp` | The engine's MCP server (rmcp): the tool catalogue, tokens and scopes, stdio, named pipe or loopback HTTP |
| `tools/session-sim` | An in-process session simulator with network faults, used for regression scenarios |
| `spikes/*` | Phase-1 feasibility spikes: physics goldens and rollback, a two-peer UDP session, WASM hot-swap, iteration-latency benchmarks, a native-code audit of the dependency stack |

Phases 3-6 (internet play, rendering, live editing with in-game
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

## The M1 demo

Four peers, on Windows and Linux, run a shared physics sandbox over UDP. Scripted loss and delay hit some
links, one member drops out mid-session and is removed by the view change, and an AI agent (Claude Code,
over MCP) drives one of the peers. At the end every peer's history is identical, and identical to an
offline replay. The runs behind this are in [spikes/out/m1](spikes/out/m1); the peer's options and
limits are in [crates/pe-app/README.md](crates/pe-app/README.md).

```bash
cargo build --release -p pe-app -p pe-mcp
```

**On one machine.** Four peers on 127.0.0.1, loss and delay on members 2 and 3, member 4 dropping out at
6 s, then a comparison of every member's history and its offline replay. It ends with `ALL EQUAL`.

```bash
bash crates/pe-app/m1-loopback.sh target/release/pe-app /tmp/pe-m1
```

**On several machines** (Windows and Linux mix freely; clocks are synced continuously, so a WSL2 VM's
drifting clock is fine):

1. On any one machine, make the session: one address per member, in member order.
   ```bash
   pe-app init --addrs 192.168.0.10:7101,192.168.0.11:7101,192.168.0.12:7101,192.168.0.12:7102 --frames 7200 --out m1
   ```
   This writes `m1/session.json` (public: ids, addresses, public keys) and `m1/member-<id>.key` (each
   member's secret key).
2. Copy `session.json` to every machine, and each `member-<id>.key` only to the machine that runs that
   member. Allow inbound UDP on the members' ports (on Windows, for `pe-app.exe`; from WSL2, on the WSL
   adapter). A member binds its session address by default. A WSL2 member reached from *another* PC, in
   WSL2's default NAT mode, can't bind its LAN address: run it with `--bind 0.0.0.0:PORT` and forward
   the port on its host, or use mirrored networking. The recorded mixed runs kept WSL2 on the same PC
   as the Windows members.
3. Start every member, in any order. The session begins once all are up.
   ```bash
   pe-app run --session session.json --key member-2.key --me 2 --touches-per-s 3 --out out -v
   ```
   Optional: `--faults faults.json` scripts loss and delay on that member's datagrams
   (`[{"at_ms": 2000, "loss_pct": 10, "delay_ms": 30, "jitter_ms": 10}]`), and `--crash-at-ms 15000` makes
   it drop out without a word.
4. When the members finish, gather every machine's `out/summary-*.json` and `out/hashes-*.txt` into one
   folder and run `pe-app compare <folder>`.

**An AI agent drives a peer.** `--mcp-pipe` attaches pe-mcp to that member and launches the harness
after `--` with a fresh token. The harness reaches the peer through `pe-mcp bridge`. Run this from the
repository root (the paths are relative):

```bash
echo '{"mcpServers": {"pe": {"type": "stdio", "command": "target/release/pe-mcp", "args": ["bridge"],
  "env": {"PE_MCP_PIPE": "${PE_MCP_PIPE}", "PE_MCP_TOKEN": "${PE_MCP_TOKEN}"}}}}' > cfg.json
pe-app run --session session.json --key member-1.key --me 1 --out out -v --mcp-pipe -- claude -p "$(cat spikes/out/m1/agent-windows-rebase/agent-scenario.md)" --mcp-config cfg.json
```

The scenario file is the one the recorded agent run used: find your hand, move and hold it, watch the
dropped-out member's removal, and see `scenario_check` refused on a live peer. On a live peer,
`world_step` waits in real time while the peer runs.

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
