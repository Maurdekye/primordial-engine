# Primordial Engine: Phase-1 Architecture Design

Status: phase-1 draft, revised after the agent review of 8a403ea/b108161 (see
[review-response.md](review-response.md)); for user review. Nothing here is built beyond the spikes.
Date: 2026-10-30. Author: engine-architect. Companions: [spike-report.md](spike-report.md),
[iteration-benchmark.md](iteration-benchmark.md).

## Priorities (user, 2026-10-30)

Every trade-off in this document was decided in this order:

1. **Optimization:** CPU, memory and bandwidth at run time.
2. **Fast iteration:** the time from a code or shader edit to seeing it run in a live session.
3. **Stability:** no forks, no lost objects, no crashes; one shared world line.
4. **Ease of use:** for players, and for the AI agents that author most changes.
5. **General accessibility:** more platforms, weaker hardware, more people.

Determinism, the 100%-Rust ruling and security are **constraints**, not priorities. They are never traded
against this list. Where the order changed an answer, the text says so. Examples:
* the logic runtime (13.3, D12);
* confirmation-based finality, which makes commits final in about 0.14 s instead of 2 s (6.4, 15.2);
* the body budget (D2);
* the ball pit's perturbation-source design (7.5);
* `rel-fast` as the build profile for committed logic (13.2, 13.7; settled by the user at 22:47Z).

## How to read this document

* **R1..R14** refer to the user's requirements (listed in section 1). Rulings are cited by time (UTC,
  2026-10-30).
* Every factual claim carries a tag:
  * **[V]** verified by a spike run on this machine; the spike-report test ID or log follows (e.g. [V a1],
    [V session-sim c1]).
  * **[S]** verified by reading the source code, `--help` output or official docs of the dependency, not by
    running it.
  * **[I]** inferred or proposed design. Not yet tested.
* Choices that go beyond what the user specified are collected in **section 24, "Decisions needing
  the user"**, each with a recommendation and alternatives. Sections refer to them as **D1..D80**, with
  letter suffixes where one topic was split (e.g. D14a). Most are now settled by the user (24 says which and
  when); the few still open are marked so.
* Review findings (C1, H4, M3, R13-C1, …) are cited where they changed the design;
  [review-response.md](review-response.md) maps each one to its fix.

## Contents

1. Requirements and rulings
2. Architecture at a glance (2.1 performance and iteration budgets)
3. Resonite to Primordial mapping
4. Determinism strategy (R3, R4)
5. Fixed-timestep loop and render decoupling (R2, R10)
6. Rollback (R3; 6.7-6.11: perturbation history, the snapshot knob, partial snapshots, what observers
   see, what can and cannot be realtime)
7. Perturbations: state + canonical frame (R3, R6)
8. "Soonest wins" without a central authority (R5)
9. Sync checks and divergence repair (R6)
10. P2P topology, transport and the session log (R1; 10.9 recovery when the network degrades, R18)
11. Peer discovery (R9)
12. Authority model (R1, R7, R8)
13. Live game logic in Rust (R7, 100%-Rust clarification)
14. Shaders (R7, shader ruling)
15. Git-style source control (R7, merge-seat ruling)
16. Objects, ECS and assets (R11)
17. MCP integration (R8, AI-forward framing)
18. VR (R10)
19. Resonite avatar import (R12, as narrowed)
20. Non-Rust boundaries (100%-Rust ruling)
21. In-game agent panels (R13)
22. Several worlds at once, and portals (R14)
23. Crate layout, phased build plan, risks
24. Decisions needing the user
25. Massive worlds: hierarchical chunks, f64 coordinates and regions (R16)
26. Built-in avatar and avatar-feature culling (R17)
27. Live-editable and replaceable physics (R15)

---

## 1. Requirements and rulings

| # | Requirement (user, 2026-10-30) | Where |
|---|---|---|
| R1 | Fully peer-to-peer: no host PC, no central authority | 10, 11, 12 |
| R2 | Fixed-timestep engine clock, independent of framerate | 5 |
| R3 | Deterministic local sim with rollback; network carries only player-physics interactions | 4, 6, 7 |
| R4 | Box3D physics; Rust + wgpu engine and graphics | 4, 18, 20 |
| R5 | Simultaneous interactions: soonest wins | 8 |
| R6 | Periodic and interaction-triggered sync checks; on mismatch find earliest interfering frame, propagate by rollback | 7.3, 9 |
| R7 | Players with authority live-edit logic and shaders; git-style SCM; earlier commit wins; later rejected with reason and asked to rebase; anti-starvation spot | 13, 14, 15 |
| R8 | AI working with players through MCP, designed in from the start | 17 (and every section's MCP notes) |
| R9 | Decentralized peer discovery: compare options, recommend one | 11 |
| R10 | Full VR support | 18 |
| R11 | Object hierarchy + ECS; assets; their interaction with determinism, rollback, P2P, SCM, MCP | 16 |
| R12 | Resonite avatar import; v1 = mesh + skeleton/skinning + textures only | 19 |
| R13 | Own agent harnesses (Claude Code, Codex, …) in in-game panels; panel-to-panel connections; mail-hub panel; panels unlockable for friends/session | 21 |
| R14 | Joined to several worlds at once; mechanics for seamless, consistent world transitions (portals) | 22 |
| R15 | Players with authority interact with, override or replace parts or all of the physics engine, live | 27 |
| R16 | Massive worlds: f64 coordinates, hierarchical chunks about 1 km wide, players of any scale, size-based simulation culling for dozens to hundreds of players | 25 |
| R17 | Built-in culling of avatars and avatar features | 26 |
| R18 | Detect network collapse and recover automatically, never replaying old input visibly | 10.9 |

Rulings that shape the design:

* **Phase 1** = design doc + feasibility spike (08:22Z).
* **Mobile/standalone (incl. Quest-native) excluded from MVP**; desktop PC VR via OpenXR. Note where a
  choice would preclude it later (08:26Z).
* **box3d-rust preferred** over C bindings (08:28Z). **"100% Rust"**: no C/C++ compiled or vendored, no C
  fallback (08:28Z). It **covers live game logic**, which is Rust, not a scripting language (08:28Z).
  **Shaders are exempt** (08:29Z).
* **AI-forward**: AI agents via MCP are the main authors of logic and shader edits (08:29Z).
* **Resonite** is the product model in broad strokes; the spec wins where they differ (08:32Z).
* **Merge seat** lasts 10 s so a waiting merger can never starve anyone else (08:51Z).
* **R13 additions** (09:12-09:15Z):
  * panels talk peer to peer within a session, and through a mail hub (a hubchat-style panel). Refined
    by D32: all messaging runs over the engine's P2P network, and the hub is an optional bridge;
  * owners can unlock panels (locked / friends / anyone in session) and revoke;
  * panel-to-panel communication needs an explicit connection, under a per-panel policy (allow all /
    ask agent / ask user / deny all).
* **Priorities** (09:26Z): optimization, fast iteration, stability, ease of use, general accessibility (see
  the top of this document). **Iteration speed decides the logic language and structure** (09:27Z, 13).
* **State + canonical frame** (09:30Z):
  * a physics interaction travels as the resulting state plus the canonical frame it occurred on;
  * receivers roll back, apply and re-simulate;
  * acceptance: the receiver's copy equals the originator's at the current frame (09:31Z, 7.3).
* **Same-frame conflicts** resolve by any stable total order that every peer computes identically and
  that no self-chosen key can game (09:32Z, 8.3).
* **Programmable systems** execute locally and synchronize over the network on perturbation, like
  Resonite (09:39Z, 7.1).
* **The ball pit is "the main question"** (09:42Z): very high-frequency perturbations need a dedicated,
  measured answer (7.5).
* The game that will run on the engine is named **Gnosarium** (12:31Z). Demo scenes may use it; the engine
  stays "Primordial Engine".
* **Core goal** (13:35Z): every reaction physically realtime and reactive for every player, everything
  perfectly synced, minimal CPU and memory. Input delay is at most an opt-in (D41). A **memory/CPU slider**
  (13:36Z, D50).
* **van der Waals contact recycling** on by default, combined by harmonic mean; small worlds use length
  units (14:20Z; [vdw-simulation.md](vdw-simulation.md)).
* **An author never sees its own interaction corrected;** the late receiver pays, and nothing is
  re-stamped (14:31Z, 8.2). Stated exceptions: grab races (D72), demotion (10.9) and portal carries (22.7).
* **Logic runtime:** wasmi everywhere now, Cranelift later (15:10Z, D12). **Shaders:** WGSL only for now
  (15:40Z, D13).
* **The open decisions** (18:42-19:21Z): the user ruled D2, D4, D11, D18a/D20, D21, D23 and D62, D27a and
  D34, D32, D49, D60 and D72, and took the recommendation for every other (19:11Z). Section 24 records
  each. The rulings raised five new questions, D74-D78, which the user settled as recommended at 19:58Z.
  P2-5 raised D79 (settled 20:07Z); the phase-2 view change raised D80, settled at 22:13Z as session
  admins over a two-thirds safety layer (12.4, 10.8).
* **Agent panels** (21:50-22:54Z): Claude Code, Codex and Antigravity as the launch harnesses, through one
  harness layer (21.1); voice prompting with a speech model built into the engine on Windows and Linux,
  and spoken replies heard nearby (21.11); panels know where the player looks and points (21.12), with
  guidance tools (21.13); game settings changed by talking to the agent (21.14); **openness first**:
  prompts, replies and agents' work are seen and heard nearby by default (21.4); confidants (21.5); a
  simplified transcript (21.2). The proposals these rulings left open, D81-D86 and D89, were ruled at
  08:31Z: as recommended, except D86 and D89, where the user chose (b) (21.4, 21.5).
* **Logic builds** use the `rel-fast` profile, without LTO (22:47Z, 13.2).

---

## 2. Architecture at a glance

```
                one peer process (identical on every machine)
 +------------------------------------------------------------------------------+
 |  MCP server (rmcp) <--- local AI agents (stdio pipe; per-session tokens)     |
 |        |  tools: read state, propose interactions, edit objects,             |
 |        |         compile/test/commit logic, validate/commit shaders, assets   |
 |        v                                                                      |
 |  Action intake: signs as the session's key, checks the local ACL view         |
 |        |                                                                      |
 |        v                                                                      |
 |  +--------------- SIM WORKERS (one task per joined world, R14) -----------+  |
 |  | session log: signed entries (state + canonical frame), acks            |  |
 |  | confirmed frame from acks -> admission, finality, keyframe trimming    |  |
 |  | order: (effective frame, per-frame tie rank, member, seq) + claims     |  |
 |  | logic modules (WASM, deterministic, fuel) -> ECS store + hierarchy     |  |
 |  | Box3D step -> carried-state overwrite (if different) -> hashes          |  |
 |  | keyframes from the confirmed frame; rollback to the earliest change    |  |
 |  +----------------------------------+--------------------------------------+  |
 |        | publishes presentation state  ^ entries, acks, commits             |
 |        v                               |                                     |
 |  RENDER/XR THREAD (display rate)    NET THREAD (one socket, all worlds)      |
 |  wgpu + WGSL (naga), extrapolation, UDP + Noise, acks, gap fetch, clock      |
 |  portals, OpenXR, local poses       sync, NAT traversal, discovery, bulk     |
 |  Content-addressed store (BLAKE3): source, wasm, WGSL, assets, commits        |
 +------------------------------------------------------------------------------+
```

* **Every peer runs the same deterministic simulation** from the same initial state, code and assets.
* **Only perturbations cross the network** (7): signed entries carrying their canonical frame and the
  state they produced, plus commits.
* Late arrivals are absorbed by rollback.
* **Acks make the confirmed frame a function of signed data** (6.4, 10.8). Below it, nothing can change,
  and hash checks prove that every peer stayed on the same world line.
* **There is no host:** ordering comes from the frame stamp plus a deterministic tie-break that every peer
  computes identically [V c1-c3, V session-sim].

### 2.1 Performance and iteration budgets

Targets for the MVP desktop configuration, and how they compare with what was measured on the Ryzen 7 9700X
(release builds):

| Budget | Target | Measured or derived |
|---|---|---|
| Sim tick | 90 Hz (11.1 ms) target, set per world; 60 Hz (16.7 ms) as the fallback where performance is a concern (D4, user 18:46Z). Frame counts in this document are at 60 Hz unless stated; protocol limits are defined in time | — |
| Sim-thread work per tick, per world, including rollback | ≤ 8 ms; anything beyond that is amortized over the next ticks (6.5) | — |
| Physics step per world | ≤ 1 ms. Serially that is about 500 awake bodies; beyond it, multi-threaded stepping in our fork (D2, user 18:44Z) | 0.36 ms at 256 bodies, 1.5-1.9 ms at 1,024 [V b6]; 0.35 ms for the 500-ball pit [V session-sim] |
| Typical rollback depth | one-way latency + ack interval: 4-8 frames | max 4 at 60 ms ± 20 ms, 9-14 with loss [V session-sim, p2p-v2]; deeper for a late receiver, which now pays for its own lateness (8.2): 38 frames after a 600 ms delay [V session-sim outage] |
| Continuous perturbation (a hand in a ball pit) | ≤ 3 ms of re-simulation per frame at 500 balls, 60 Hz, 2 members, ≤ 60 ms one-way | 289-298 re-simulated frames/s ≈ 2.6-2.8 ms per frame at 60 Hz [V ballpit, work counts × 0.35 ms per step]. **Over budget** at 90 Hz (615 frames/s, about 3.8 ms per 11.1 ms frame) and at 100 ms one-way (6-8 ms of total sim CPU per frame) [V ballpit-90hz, observer-100] (review NEW-15); budgeted per latency band, with p99 ticks (6.8). 0.27 ms with the opt-in 4-frame input delay (default 0, D41) |
| Confirmation (finality) | ≤ 200 ms on 30-60 ms links (it was 150 ms until round 2; ed347ac raised it for stable-data confirmation, D54, without saying so: review R3-12) | 8.5 frames (142 ms) at 50 ms ± 15 ms [V session-sim r14]. **Over budget under jitter and loss** [V reviewer, average / p95 / max]: 169 / 200 / 200 ms at 60 ± 20 ms; 199 / 333 / 400 ms with 2% loss added; with 2% loss and 3, 4 or 8 members, averages of 234, 275 and 344 ms |
| Snapshot memory per world | ≈ 4 keyframes (2-10 MB) typical, 31 at most | 6.3. Grows with the confirmation lag: 11 MB at 60 ± 10 ms with 2% loss, 17-19 MB at 32 kbit/s with 5% loss [V reviewer, R3-12] |
| Game logic per tick, per world | ≤ 1 ms | 6.4 ms in wasmi, 1.6 ms in wasmtime/Cranelift, 0.9 ms native at 10k entities [V iteration-benchmark 4.1] |
| **Logic edit, local build and swap** (one-function edit) | ≤ 0.5 s at up to ~10k LOC; ≤ 1.5 s at 45k LOC | 0.42-0.45 s at 6.4k LOC, 1.2-1.3 s at 45k (`rel-fast`, wasmi, idle machine) [V iteration-benchmark; reviewer rerun]. 0.58 s with 8 busy processes, 0.83-1.01 s with 14 (D12-4) |
| **Logic edit to every peer in a live session** | ≤ 1.2 s | ≈ 1.1 s: build and swap 0.45 + confirmation 0.14 + activation lead 0.5 [V components; I total] (D12-4) |
| Commit visible to every peer | confirmation + activation lead ≈ 0.14 s + 0.5 s | 15.2 |
| Shader edit to visible | ≤ 100 ms locally (naga validation is milliseconds [I]) + the same commit path | 14 |
| Hot swap at a frame | ≤ 10 ms on the sim thread | 6 ms in wasmi; Cranelift 27-35 ms, so it compiles off-thread during the activation lead (13.3) [V iteration-benchmark, reviewer] |
| Bandwidth per member, interactions + acks | 1-5 kB/s | 7.6 |
| Bandwidth, a hand engaged in a ball pit | ≤ 20 kB/s per member | 2 members: 11.9 kB/s both directions batched (65 + 30 kbit/s) at 50 ± 5 ms without loss, 18.8 kB/s per message [V r18 capped, ballpit]. Like for like with 3d0f98a's 14.7 kB/s (60 ± 10 ms, 2% loss), batched is 13.6 kB/s, 7% less; ack bytes alone grew 53% [V reviewer, R3-13]. Upload grows with members, since each peer gets its own copy: about 16 kB/s for the hand's author at 3 members and 24 kB/s at 4 [I, from the 2-member figures] (NEW-15). Signatures add 2-4 kB/s per peer [I, 10.9] |
| Head motion to photon (VR) | < 20 ms | 18.5 |

---

## 3. Resonite to Primordial mapping

| Resonite | Primordial | Note |
|---|---|---|
| World (saved) | World repository: logic source, WGSL, assets, scene/prefab data, ACL | versioned by the SCM (15) |
| Session (host-authoritative) | P2P session; deterministic sim + rollback; no host | R1 |
| Slot hierarchy | Object hierarchy: object = entity with Parent, Children, Transform | 16.2 |
| Components on slots | ECS components: engine built-ins + components declared by WASM logic modules | 16.1, 16.5 |
| ProtoFlux visual scripting | Rust game logic compiled to WASM, mostly AI-authored via MCP | 13, 17 |
| Fixed shader set (Unity-based) | Live-editable WGSL shaders under the same SCM | 14 |
| In-world live editing (inspector, gizmos) | In-world tools and MCP tools; both become interactions or commits | 7, 15, 17 |
| Users, roles, permissions (enforced by host) | Signed ACL log; every peer enforces it deterministically | 12 |
| Records/inventory, cloud asset storage (resdb) | Local content-addressed store + P2P transfer; inventory = signed manifests | 16.7 |
| Avatar (AvatarDescriptor, VRIK) | AvatarRig + IK components; import v1 = mesh + skin + textures | 18, 19 |
| Headless session hosts | Optional always-on peers with no special authority | 10.6 |
| Contacts, session browser (cloud) | Invites, session codes, mDNS, Mainline DHT, peer exchange | 11 |
| Sync members replicated through the host; drives and ProtoFlux evaluated locally | Every programmable system (physics, WASM logic, components) runs locally and deterministically on every peer; only perturbations travel, as state + canonical frame, with rollback (ruling 09:39Z) | 7 |
| Several worlds loaded, one focused (you are "away" in the others) | Joined to several worlds at once: one embodied, the others attending with real agency; portals and a settled handoff for carried objects | 22 |

---

## 4. Determinism strategy (R3, R4)

### 4.1 What was verified

* **box3d-rust 0.5.0 reproduces Box3D's C-reference determinism goldens bit-for-bit.** The four scenes
  are falling ragdolls, wave pile, query spawn and mesh drop. This held in debug and release, with
  `-C target-cpu=native` on a CPU with FMA and AVX-512, and in a static Linux (musl) build run under
  WSL. [V a1, a4, a5]
* **A 600-frame, 256-body scene with scripted interactions has an identical per-frame hash trace** across
  run-to-run, debug vs release, opt-level 1/s/3, native CPU features, and Windows-MSVC vs Linux-musl
  builds. [V a2-a5]
* **The same scene compiled to wasm32 and run in the wasmi interpreter matches the native digest.** This
  is a different target, pointer width and execution engine. [V d4]
* **Cross-machine (review M5, closed):** @design-reviewer ran the goldens, all four 600-frame traces,
  rollback T1-T5 and the d4 digest on an Intel i7-13620H laptop (no AVX-512). All were bit-identical to
  this Ryzen 7 9700X desktop [V design-reviewer's laptop determinism run].
* **Not verified:** ARM (aarch64), macOS, and Box3D multithreading (the port is serial). Upstream claims bit-identical results across Linux/Windows/macOS on x64 and arm64, enforced
  by CI [S upstream `docs/simulation.md`]. The port claims bit-exact results against the C scalar build
  [S port README]. These are claims, not our measurements.

### 4.2 Why floats are enough (no fixed point)

* Box3D is designed for cross-platform float determinism: custom `atan2`/`cos`/`sin`, ordering by creation
  order, no FP contraction [S upstream `docs/simulation.md`].
* Rust never contracts `a*b+c` into FMA and has no fast-math mode, so `target-cpu=native` cannot change
  results [V a3]. (MSVC/clang C builds need `/fp:precise` or `-ffp-contract=off`. That is moot now, since
  no C is compiled.)
* Fixed point would mean rewriting the physics engine, which the R4 ruling rules out. **Recommendation: IEEE
  f32 with engine rules (4.3) plus a CI determinism matrix that includes ARM (D3).**

### 4.3 Determinism rules for all sim code (engine and logic modules)

1. Sim code is single-threaded per world, or partitioned deterministically (6.6). No dependence on thread
   timing.
2. No wall-clock time, OS randomness or I/O in the sim. Randomness comes from the session seed plus the
   frame plus explicit streams.
3. No transcendental functions from `std`/libm in sim code; their results may vary by platform. Use
   Box3D's deterministic math or `pe_core::dmath` (P2-1): bit-exact `f32` `sin`, `cos`, `sin_cos`, `tan`,
   `atan`, `atan2`, `asin`, `acos`, `exp`, `ln`, `log2`, `pow` and `powi`, each an `f64` fdlibm kernel
   rounded to `f32`, within 1 ulp of `std` (2 for `pow`) over the tested ranges [V P2-1]. Basic IEEE ops
   (`+ - * / sqrt`, conversions) are exact.
   * **`powi` and `powf` count as transcendental here.** LLVM folds `f32::powi` at compile time with
     another algorithm: on constant inputs a release build gave results 2 ulps from the debug build's
     [V P2-1]. Use `dmath::powi` and `dmath::pow`.
4. No iteration over hash-ordered containers in the sim. Use `BTreeMap` or sorted vectors. box3d-rust's
   own HashMaps are lookup-only [S grep of the crate].
5. All inputs that enter the sim are quantized to exact f32 bit patterns at the boundary (VR poses: 18.4).
6. NaN never enters the sim. Host boundaries reject non-finite values [V d1 host checks]. WASM NaN payloads
   are canonicalized by wasmi's `deterministic` feature [S wasmi_core `value.rs`].
7. Every peer in a session runs the same engine version with the same sim-relevant configuration, verified
   by the CI determinism matrix (rule 8). Binaries differ by OS, so "bit-identical build" was unsatisfiable
   (review D12-1). Admission checks the engine version, and Box3D snapshot images carry a layout hash
   [S port `snapshot/mod.rs`]. The engine's toolchain pin is engine metadata, never a `rust-toolchain.toml`
   in a world repo (15.6, review NEW-8).
8. A CI determinism matrix (Windows x64, Linux x64, Linux arm64, macOS arm64; debug and release; native and
   wasm32) compares golden traces on every change. It also catches regressions in box3d-rust updates.
9. **NaN and the FP environment (review L2).** A debug assertion checks every body's state is finite after
   each step. Release builds check it at every check frame, one pass over body states, and report the
   first non-finite object [V session-sim: the suite fails on any non-finite state, and none occurs].
   **The FP control register holds on every simulation thread, workers included** (@chunks-reviewer's D2
   finding). Native libraries loaded in-process (OpenXR runtimes, audio drivers) can change it [I].
   * `pe_core::fpenv::check_and_reset()` compares the calling thread's whole register with the platform
     default and restores it: MXCSR 0x1F80 ignoring the sticky flags on x86-64 (so FTZ and DAZ off,
     round to nearest, exceptions masked); FPCR 0 on aarch64 (FZ, DN, rounding mode, traps, and
     FEAT_AFP's FIZ, AH and NEP). It costs one register read and a compare, and writes only if something
     changed [V P2-1 on x86-64, by setting flush-to-zero; aarch64 not run].
   * pe-sim's tick calls it first, on the sim thread, every tick [S `crates/pe-sim/src/schedule.rs`].
   * The parallel stepping's helpers (D2) run each job with the calling thread's register, copied in at
     the job's start, so a worker never steps with a setting of its own [V box3d-fork test
     `helpers_use_the_callers_fp_control`]. Any other thread that runs sim code calls the check at each
     job start.
10. **No process-global sim state (R14).** box3d-rust's length unit is a process global; it stays at
    1.0 for every world. Its `human` sample module (a thread-local RNG) is never used in sim code. Logic
    runtimes compile eagerly (22.2) [S].

### 4.4 Gaps of box3d-rust compared with the C reference (tech ruling: "note any gaps")

| Gap | Impact | Mitigation |
|---|---|---|
| Serial only (no task system or worker threads) [S port README] | Step cost grows with bodies: 0.36 ms @256, 1.5-1.9 ms @1024, 4.8-5.9 ms @2048 [V b6]; rollback re-sim multiplies this | **Multi-threaded stepping in our fork, bit-exact across thread counts and machines** (D2, user 18:44Z; feasibility study by @vdw-physics: upstream Box3D C's task system claims thread-count-independent determinism). Also independent regions stepped in parallel (6.6) |
| Scalar only (no SSE/AVX/Neon) [S] | Slower solver than the C build | Accept for MVP; SIMD in the port would have to stay bit-exact with scalar |
| Pinned to upstream `3fc20f5` [S]; upstream HEAD is `33626332` (2026-10-28) | Missing upstream fixes and features; port lag | Pin the exact version (`=0.5.0`); track upstream; re-run goldens on every bump |
| Third-party port of alpha software (Box3D v0.1.0, "alpha") [S] | API churn, latent bugs | Wrap behind `pe-physics` so the engine never calls box3d-rust directly; golden tests |
| C-mirror API (not idiomatic Rust) [S] | Ergonomics only | Thin wrapper in `pe-physics` |

No gap blocks the requirements. The serial-only port is the main performance risk (23.3).

### 4.5 Large worlds

The port has a `double-precision` feature, which maps to `BOX3D_DOUBLE_PRECISION`, with its own goldens
[S]. It gives far-from-origin precision at a cost. Not enabled in the spikes. Decision: **D3**.

---

## 5. Fixed-timestep loop and render decoupling (R2, R10)

* **Sim clock:** `frame = floor((session_time_us - epoch_us) / dt_us)`. The default is 60 Hz with 4 Box3D
  sub-steps; every world has its own tick rate (22.2).
  * `session_time` is a **monotonic** local clock plus a slewed offset estimate (10.4). It is never wall
    time: a time-service step on wall time would make the sim jump frames (review L6).
  * The sim thread steps until it reaches the frame the clock says, applying pending rollbacks first. It
    never waits for rendering [V c1 loop].
* **Drift control in both directions (review H5):**
  * every ack carries the sender's horizon, which is its present frame (10.8);
  * a peer compares its present with the median horizon of the other members, corrected for measured
    one-way latency;
  * it slews its clock by up to 1% towards that median, whether it runs ahead or behind;
  * a peer that runs behind gains nothing: its published horizons bind it (8.2), and if it stalls
    confirmation it is demoted to spectator (10.9).
* **Rendering** runs at display rate: monitor vsync, or `xrWaitFrame` at 90-120 Hz in VR (review M2):
  * Sim objects are drawn at the **predicted display time**. The newest published state is extrapolated
    with its velocities by the elapsed fraction of a frame, then error-smoothed (next bullet). Pure
    interpolation between the two newest states would show the past (17-33 ms late).
  * **Objects held by the local player's hand are attached to the late-latched hand pose** (a
    presentation override). The sim still owns them; the override only removes the visible lag between
    hand and object.
  * **Error smoothing:** when a rollback corrects a pose, the renderer blends from the displayed pose to
    the corrected one over about 100 ms, so corrections do not pop [I].
* **Presentation-only systems** (animation, particles, audio, UI, local head and unengaged hands, panels)
  run at render rate. They read sim state but never write it (7.4, 16.3).
* Tick rate (D4, user 18:46Z): **90 Hz target, configurable per world, with 60 Hz as the fallback** where
  performance is a concern. 90 Hz removes most hand-to-object mismatch in VR but costs 1.5x the CPU,
  including every re-simulated frame. What triggers the fallback: **D76**.

---

## 6. Rollback (R3)

### 6.1 Rollback state

A snapshot at frame *f* (the state before frame *f*'s entries) contains:

| Part | How | Evidence |
|---|---|---|
| Box3D world | `recording::serialize_world` image (POD + geometry registry), restored in place with `deserialize_into_shell`. Bodies created or destroyed after the snapshot are undone by the restore, and their ids are reissued identically on re-simulation | [V b1, b4; V session-sim r14: bodies destroyed by a departure and created by an arrival or return across rollbacks] |
| Engine tables | claims, object table, R14 escrow and arrival tables: plain copies | [V session-sim] |
| Sim ECS store (16) | copy-on-write columns: a snapshot shares every column buffer, and the first write afterwards copies that column once; unwritten columns are never copied. Snapshot and restore cost O(columns) | [V P2-1 `store_bench`: snapshot 0.1 us and restore 0.2-0.4 us up to 8,192 objects; a deep copy of every column takes 64-83 us at 2,048 objects, about 3% of the spike's Box3D image save] |
| Logic module state | the module's sync state (7.4): component columns, the declared state arena, active module id | [V d1 for whole linear memory; I for the arena split] |
| Session log | append-only; never rolled back. Entries are re-applied on re-simulation | [V b2, b3, d1, session-sim] |

### 6.2 Measured costs

Release build on a Ryzen 7 9700X, two bench runs (ranges reflect run-to-run timing noise) [V b6]. The
design reviewer's run on the same CPU fell inside these ranges.

| Bodies | Step (avg) | Save | Restore | Image |
|---|---|---|---|---|
| 256 | 0.36 ms | 0.25-0.26 ms | 0.25 ms | 0.59 MB |
| 1024 | 1.5-1.9 ms | 0.95-1.2 ms | 0.74-0.91 ms | 2.4 MB |
| 2048 | 4.8-5.9 ms | 2.2-2.7 ms | 1.7-2.1 ms | 5.4 MB |

The image is about 2.3-2.6 KB per body, because it contains broadphase trees, contacts and solver sets
along with body state. Whole-memory guest snapshots added about 1.1 MB per logic module at the default
1 MiB WASM stack [V d1]; 7.4 shrinks that to the declared sync state.

### 6.3 Snapshot policy

* **Revised by 6.7-6.8:** keep the base and a short window of recent keyframes under a local memory/CPU
  knob (D50). The bullets below describe 3d0f98a's Every(4) policy, which is still the spike's default.
* Keyframes are taken every K = 4 frames, and only between the **confirmed frame** (6.4) and the present.
  Rolling back restores the newest keyframe at or before the target frame and re-simulates the gap.
* Re-simulation saves keyframes only at multiples of K, not every frame (review M1).
* With confirmation 8.5 frames behind the present (6.4), a world holds about 3-4 keyframes: about
  2-2.5 MB at 256 bodies and 7-10 MB at 1,024 [I, from the image sizes]. Under jitter and loss the lag
  is longer and so is the ring: at 60 ± 10 ms with 2% loss, a 26-frame lag, Every(4) held up to
  7 images, 10.5 MB for the 500-ball pit [V snap, round 3; review PH-1]. The old proposal held a fixed 2 s
  ring (about 72 MB at 1,024 bodies).
* The speculation limit (6.4) caps the worst case at 120/K + 1 = 31 keyframes.
* Fixed-offset byte deltas of images do not help, but general-purpose compression does (6.8). Sync-state-only guest snapshots
  (7.4) remain planned.

### 6.4 Confirmed frame and the speculation limit (reviews C1, NEW-1, NEW-2)

* **Phase 1 was wrong here.** Whether an input was "too late" depended on when it reached each peer, so two
  honest peers could disagree and fork for good [V review C1 repro: an input for frame 50 admitted at frame
  169 and refused at frame 171].
* **3d0f98a was still wrong, in a subtler way** (review round 2, NEW-1). It computed the confirmed frame
  and admission verdicts from acks: signed, but point-to-point messages that nobody compared. A member that
  sent different acks to different peers forked them. So did a 6.5 s one-way ack outage among honest members.
  [V reviewer repros `ackfork` and `outage`, both forked before the fix]
* **Now every input to a decision is an entry in its author's hash chain** (10.8). That covers
  perturbations, acks (horizon, holdings, check hash) and votes. Two different versions of one chain
  position are a transferable proof of equivocation (9.3).
* **Confirmation uses stable data only.**
  * A chain entry is *stable* once every member's chain references it with the same id.
  * Frame *f* is **confirmed** once, for every member, a stable prefix of its chain declares a horizon of at
    least *f*. A horizon is a promise to author nothing stamped at or before it.
  * Any two members that confirm *f* used the same entries (Wuu and Bernstein's 2-D time table, over signed
    hash chains; 10.8).
* **No admission decision depends on the confirmed frame.** Validity is chain-local (8.2). The confirmed
  frame only drives finality (SCM, ACL and R14 relays act on confirmed facts) and trimming.
* **Confirmed frames are final, and the code asserts it** (NEW-2).
  * An equivocation found after confirmation keeps the stable version (9.3).
  * An eviction never changes an effective frame, because nothing is ever re-stamped (8.2).
  * Reaching a rollback below the confirmed frame aborts with "protocol invariant violated" instead of
    rolling back. No scenario reaches it [V session-sim, 25 scenarios].
* **Speculation limit W = 2 s (120 frames at 60 Hz, 180 at 90 Hz; D4).** A member whose present runs W frames ahead of the
  confirmed frame stops advancing its sim. It keeps networking until confirmation catches up. Persistent
  stalls lead to a view change (10.8) and, for a member that is the cause, demotion (10.9).
* **Measured** [V session-sim c1]: the reviewer's congestion scenario on the round-2 protocol.
  * Member 3's frame-50 entry reached the others about 2 s late.
  * It kept its frame: nothing is re-stamped (user rule 14:31Z). The two waiting members rolled back
    116 frames (about 80-86 ms of re-simulation each) and paused 18 frames at the limit.
  * All three 700-frame histories were bit-identical and equal to an offline replay of the final log.
* **Confirmation lag** on 50 ms ± 15 ms links with 1% loss, acks every two frames: **8.5 frames
  (142 ms)** [V session-sim r14]. It was 4.2 frames in 3d0f98a.
  * The extra ack round is the price of deciding only from data every member is known to hold. It costs
    finality latency, not responsiveness: rollback depth and local reaction do not change.
* Decisions: **D36** (finality and membership), **D54** (stable-data confirmation).

### 6.5 Re-simulation budget (review M1)

* **Cost per re-simulated frame = step + keyframe saves / K.**

  | Bodies | Step [V b6] | + save / 4 | Per re-simulated frame |
  |---|---|---|---|
  | 256 | 0.36 ms | 0.06 ms | ≈ 0.42 ms |
  | 512 | ≈ 0.7 ms [I, interpolated] | ≈ 0.12 ms | ≈ 0.8 ms |
  | 1024 | 1.5-1.9 ms | 0.24-0.30 ms | ≈ 1.7-2.2 ms |

* **Rollback depth is now one-way latency plus the ack interval, not a sync-check delay.** Missing entries
  are found by gap detection within an ack round trip (9.3); the old 45-frame check delay is gone.
  Measured depths: at most 4 frames at 60 ms ± 20 ms; 9-14 frames with 2-10% loss [V session-sim state-frame, loss, ballpit; p2p-v2].
* **Coalescing:** each tick handles everything that arrived since the last tick with one rollback, to the
  earliest affected frame (review option (c) for the ball pit) [V session-sim, ballpit].
* **Per tick, at 60 Hz:**
  * a 6-frame rollback costs about 2.5 ms at 256 bodies and 10-13 ms at 1,024;
  * the sim thread caps re-simulation at 8 ms per tick and finishes over the next ticks while the
    renderer smooths;
  * so serially about **500 awake bodies per world** fit. The user chose to go beyond that with
    **multi-threaded Box3D** in our fork, bit-exact across thread counts and machines (D2, 18:44Z), not
    with a cap. Region-scoped re-simulation (6.6) comes in phase 3.
* Debug builds are about 10x slower; determinism tests run in release.

### 6.6 Scaling past the serial port (R16, 25)

* Box3D worlds are independent, so a session can be split into **regions**, each its own Box3D world,
  stepped in parallel on worker threads. 25.5 designs regions and groups [I].
* Objects crossing regions migrate at deterministic frames, through the same escrow pattern as R14 (22.6)
  but inside one session log.
* Snapshots and rollbacks then become region-scoped: only regions touched by a late entry re-simulate.
  **Measured** [V, 25.10]: with 16 players of 125 balls each (2,016 bodies), a late entry for one player
  costs 0.55-0.68 ms of re-simulation in its own region, against 8.6-13 ms for one flat world, 15-18 times
  less. The 16 regions step in parallel in about 0.2 ms of wall time.
* Under the priorities this moves from phase 6 to phase 3 (23.2).

### 6.7 Keeping only the perturbation history (user question 13:33Z)

**Question:** can a peer keep only the history of perturbations instead of world snapshots?

**Answer: almost [V].**
* **The log is already the synchronized truth, and it is small.** A hand entry carries about 44 bytes of
  payload per frame (7.5), so two hands over the whole 2 s speculation window come to about 11 kB.
* **One full world image cannot be avoided.** Box3D, like any practical rigid-body engine, only steps
  forward: nothing reconstructs frame *f* from frame *f + n*. Rolling back to *f* therefore means
  restoring an image at or before *f* and re-simulating forward. No rollback can reach behind the
  confirmed frame (6.4), so the floor is **one base image at or before the confirmed frame**:
  * 0.5-0.6 MB for the 256-body pile and 1.9-2.3 MB at 1,024 bodies [V physics history];
  * 1.6 MB for the 500-ball pit and 3.6-3.7 MB at 1,024 balls [V snap].

  Contacts, broadphase trees and solver sets, more than body poses, make up most of an image (6.2).
* **Everything between the base and the present is a cache.** It changes only how much re-simulation a
  rollback costs, never the result. Measured: 62 runs of the ball pit, under 13 snapshot policies and a
  setting moved every 7 frames, at 500 and 1,024 balls, with and without a confirmation stall. Every run of
  the same scene ended in the same end-state hash (`020828ac9b6fb6c8` at 500 balls, `0d739d142b58347a`
  at 1,024, with the engine default's vdW recycling) [V snap].

**Re-simulating from the base** (restore plus N steps, collapsing pile, median of 5, one thread)
[V physics history]:

| Depth (frames) | 1 | 4 | 8 | 15 | 30 | 60 | 120 |
|---|---|---|---|---|---|---|---|
| 256 bodies | 0.8 ms | 2.1 | 3.7 | 6.7 | 12.7 | 27.6 | 50.3 |
| 1,024 bodies | 3.2 ms | 7.6 | 13.5 | 24.2 | 47.6 | 94.7 | 183.3 |

These timings come from a run at 442286f on a quieter machine, with upstream physics
(`physics-history-upstream-442286f.log`). The integration rerun with the engine default, at low priority
(`physics-history.log`), took 1.0-1.2x as long (noisy). One difference is not noise [V]: with the engine
default, the 256-body pile started at frame 900 falls asleep during the replay (246 bodies awake, 26 after
120 frames), so its deep re-simulations cost little (6.2 ms at 120 frames). The 1,024-body pile stays
awake.

A tick at 60 Hz has 16.7 ms. **Base-only** fits only while confirmation keeps up, because every
rollback then re-simulates the whole confirmation lag, not just the rollback depth.

**Rollbacks stay shallow even when confirmation stalls** [V snap]:
* How deep a rollback goes is set by how late entries arrive: one-way latency, the ack interval, and a
  fetch round trip after a loss. The deepest measured was 13 frames, at 60 ± 10 ms with 2% loss
  [V snap, round 3]. At 100 ms one-way it was 24 frames [V reviewer, PH-2].
* In the stall runs a third member's acks were dropped for 1.6 s. Confirmation fell 116 frames behind,
  but the deepest rollback was still 13 frames.
* The images that do the work are therefore the few just behind the present. The base is insurance for
  the rare deep rollback, such as an entry fetched late after two losses. Images in between are mostly
  dead weight.

### 6.8 Snapshot policies measured, and the memory/CPU knob

**Policies.** All of them keep the base; the spike's `SnapPolicy` implements each one [V snap]:
* `Every(k)`: an image every k frames from the base to the present. 3d0f98a used k = 4.
* `Log(k)`: an image every k frames for the newest 4k frames, then thinned so that spacing grows with age
  (about two images per doubling). Any rollback costs at most about 1.5x its depth.
* `Window(k)`: an image every k frames for the newest 4k frames, nothing older except the base. A rollback
  deeper than 4k frames re-simulates from the base.
* `Base(k)`: the base plus one candidate image just after the confirmed frame, which becomes the next
  base.

**Setup.** One hand sweeps the ball pit while member 2 watches, over 60 ± 10 ms links with 2% loss at
60 Hz, measured across the 3.66 s sweep. Confirmation lags up to 26 frames. In the **stall** case a third
member's acks are dropped for 1.6 s, so confirmation lags by up to 116 frames. Round 3 reran everything
on the current protocol, whose confirmation is one ack round slower (D54); the first figures predated it
(review PH-1).

**How CPU is reported: a model [C], not a measurement** (review PH-7). CPU per frame is the deterministic
work counts (re-simulated frames, saves, and one restore per rollback, 59 a second) times per-operation
costs for the 500-ball pit: step 0.353 ms, save 0.52 ms, restore 0.37 ms. Those costs were measured in
isolation on an idle machine before round 3, and are not in a committed log [I]. The round-3 rerun ran at
low priority on a shared machine, so its timings are noise; a clean timing run waits for an idle machine.
Under this model a policy that does the same work as another costs the same, by construction.

| Policy | Max MB held, 500 balls (steady / stall) | Max MB held, 1,024 balls (steady / stall) | Re-sim frames/s (steady / stall) | Saves/s (steady / stall) | Modelled CPU ms per frame [C] (steady / stall) |
|---|---|---|---|---|---|
| Every(1) | 39.0 / 184.5 | 88.1 / 416.1 | 203 / 189 | 204 / 190 | 3.68 / 3.48 |
| Every(2) | 19.5 / 93.8 | 44.0 / 211.6 | 233 / 219 | 103 / 94 | 2.98 / 2.82 |
| Every(4) (3d0f98a) | 10.5 / 47.7 | 23.7 / 107.4 | 292 / 278 | 53 / 47 | 2.89 / 2.76 |
| Every(8) | 6.3 / 25.3 | 14.1 / 57.1 | 410 / 396 | 26 / 24 | 3.35 / 3.25 |
| Every(16) | 3.3 / 14.2 | 7.4 / 32.1 | 655 / 641 | 13 / 13 | 4.68 / 4.60 |
| Log(4) | 9.4 / 17.7 | 21.2 / 39.8 | 292 / 278 | 53 / 47 | 2.89 / 2.76 |
| Log(8) | 6.3 / 14.4 | 14.1 / 32.4 | 410 / 396 | 26 / 24 | 3.35 / 3.25 |
| **Window(4)** | **8.2 / 8.3** | **18.4 / 18.6** | 292 / 278 | 53 / 47 | **2.89 / 2.76** |
| Window(8) | 6.3 / 8.1 | 14.1 / 18.2 | 410 / 396 | 26 / 24 | 3.35 / 3.25 |
| Base(1) | 3.3 / 3.3 | 7.4 / 7.4 | 726 / 2,724 | 21 / 11 | 5.17 / 16.84 |

[V snap, integration run with the engine default: counts and image bytes; every run of a scene ended in
the same end hash. Against round 3's upstream run, every count is identical and image bytes moved by at
most about 1%.]

What the table shows:
* **Dense images do not buy CPU** [C]. A save costs about 1.5 steps here, so saving every frame costs more
  than the re-simulation it saves. Modelled CPU is lowest at k = 2-4.
* **Thinning is free at 60 ms.** Log(k) and Window(k) did exactly the same work as Every(k), because they
  dropped only images that no rollback used [V counts]. Under the stall, Window(4) held 8.3 MB instead of
  47.7 MB, and 18.6 MB instead of 107.4 MB at 1,024 balls.
  * **At 100 ms one-way it is not** (review PH-2). The deepest rollback, 24 frames, exceeds Window(4)'s
    16, so under a stall Window(4) re-simulated 568 frames/s against 425 for Every(4) (+21% modelled CPU),
    and peaked at 11.5 MB (7 images) [V reviewer]. The adaptive window depth (below) is what keeps the
    default right at higher latency [I].
* **Base-only is the memory floor, but not cheap any more** (review PH-1). Every rollback re-simulates
  the whole confirmation lag, and round 2's stable-data confirmation made the lag longer: 726 frames/s at
  26 frames of lag, +79% modelled CPU against Window(4), where the first figures said +17%. Under a stall,
  2,724 frames/s and 16.8 ms per frame modelled. It needs a guard (below).
* **Fixed-offset byte deltas do not help; compression does** (review PH-3).
  * Storing ring images as 64-byte block deltas against the base, or against the previous image, saved at
    most 8% on the ball-pit rings and usually nothing. A contact array that grows or shrinks shifts every
    later block. The best case, a slow-moving pile, still needed 40-45% for images 1-4 frames apart, and
    saved nothing 16 or more frames apart [V snap, physics history].
  * General-purpose compression is a different matter [V reviewer, a throwaway test]: plain LZ4 cut an
    image to 44-47%, deflate to 34-38%, and XOR against the base plus LZ4 to 8-12% on a same-size pile. It
    costs about 0.5-0.7 ms to compress and 0.2 ms to decompress per 0.5 MB.
  * So the knob can also **compress cold images**: those older than the window, which a rollback rarely
    needs [I]. A pure-Rust LZ4 (`lz4_flex`) keeps the 100%-Rust rule [I].
* **The worst case does not depend on the policy.** A rollback all the way back to the confirmed frame
  re-simulates the whole lag whatever is cached: up to 51 ms at 256 bodies and 190 ms at 1,024 when
  W = 120. It can only be reduced by a smaller W, a smaller active world or region-scoped
  re-simulation (6.6). The 8 ms per-tick cap (6.5) spreads it over several ticks.

**The knob (proposal, D50).** "Snapshot memory" is one setting per peer, local and changeable at run time,
with four positions:

| Position | Policy | Max image memory (500 balls steady; stall 500 / 1,024 balls) | Modelled CPU against the best [C] | Rollback depths covered without the base |
|---|---|---|---|---|
| Speed | Log(4) | 9.4; 17.7 / 39.8 MB | best | any depth, at ≤1.5x |
| **Balanced (default)** | Window(4), adaptive depth [I] | 8.2; 8.3 / 18.6 MB | best at 60 ms | ≤ 16 frames without the adaptive depth (13 at most at 60 ms, 24 at 100 ms) |
| Lean | Window(8) | 6.3; 8.1 / 18.2 MB | +16% | ≤ 32 frames |
| Minimum | Base(1) + stall guard [I] | 3.3; 3.3 / 7.4 MB | +79% steady; guarded under a stall | none |

* **It never changes a result,** because images only cache states the log already determines.
  * Verified [V snap knob-moving]: the knob cycled every 7 frames through Every(1) → Base(4) → Log(4) →
    Window(4) → Every(8) → Base(1) while about 59 rollbacks a second were pending.
  * The end hash matched every fixed-policy run, steady and stalled, at 500 and 1,024 balls.
* **It may move at any rate.**
  * The one invariant, which every position keeps, is that the base (the newest image at or before the
    confirmed frame) is never dropped.
  * Moving towards less memory drops images at the next trim, which waits until no re-simulation is
    pending.
  * Moving towards more memory affects only frames simulated afterwards: images of past frames cannot be
    recreated without re-simulating them. It reaches full effect after at most one confirmation lag.
  * In the knob-moving runs memory peaked at 61.9 MB (500 balls, stall) while Every(1) phases refilled the
    ring.
* **Adaptive window depth [I]:** track the 99.9th-percentile rollback depth over the last few seconds and
  keep the window at max(4k, that depth + k). This is local, like the knob.
* **Stall guard for Minimum [I]:** while confirmation lags more than twice the window, add Window(8) images
  until it catches up. Otherwise one stall costs about 5x the CPU.
* **Per chunk (R16) [I]:** the policy applies to each Box3D world. Once R16 splits a session into chunks or
  regions, each keeps its own base and window, so an idle chunk holds only its base.

### 6.9 Partial snapshots: not possible through box3d-rust's API [V physics history]

The idea was to save only the perturbed islands, with their contacts and warm starts, and trust untouched,
sleeping or isolated islands to re-derive themselves.

* **Test 1: every body written back through the API.**
  * Setup: in a 256-body pile, every body's pose and velocity was saved at one frame. They were written back
    50 frames later into the same world, with `body_set_transform`, `body_set_linear_velocity` and
    `body_set_angular_velocity`.
  * Result: stepping diverged from the reference at the first frame. After 30 frames, 2 of 256 bodies were
    bit-equal and the largest position error was 340 mm (244 mm with upstream physics).
  * Control: a full image restore of the same frame matched 256 of 256.
* **Test 2: one isolated island, everything else asleep.**
  * Setup: 64 separate 4-box stacks, all asleep. One stack got a different speculative push; the other 63
    were checked to be unchanged and still asleep. That stack's 4 bodies were then restored through the
    API, given the correct push, and run for 60 frames.
  * Result: the restored stack matched 0 of 4 bodies (8.7 mm error); the other 252 bodies matched.
* **Why it fails.** A world image also holds what the API cannot set:
  * contact manifolds with their warm-start impulses;
  * sleep timers and island membership;
  * solver-set placement and constraint-graph colours;
  * broadphase trees and fat AABBs;
  * id pools.

  These are the sections `serialize_world` writes [V]. Several are global (one tree, one colour graph,
  one contact array) and their order decides iteration order, so even an island-scoped restore needs them
  bit-exact.
* **What these tests show** (review PH-4): only that the public API cannot do it. They wrote back poses and
  velocities, which is all the API allows. They do not test the idea itself, islands saved together with
  their contacts and warm starts; that needs the fork below.
* **What it would take [I].**
  * A fork of box3d-rust that snapshots per solver set (sleeping islands already live in solver sets of
    their own) and shares unchanged sleeping-island bytes between images, copy-on-write. Image memory would
    then grow with the awake set rather than the world.
  * Region-scoped worlds (6.6, R16) give most of the same benefit without a fork: an untouched region is
    never rolled back at all.
  * Decision **D51**.

### 6.10 What an observer sees of a remote hand

An observer learns of a remote player's hand motion one network latency late. Three mechanisms bring what
it sees as close to the truth, and as physically plausible, as possible. None of them makes the sim
inexact.

1. **Remote-hand dead reckoning, a world rule [V observer].** When a frame has no entry yet from an active
   hand, the sim keeps the hand's last commanded velocity, capped at 3 m/s, for up to 6 frames, and then
   holds it. The rule is part of the deterministic sim, so every peer applies it identically. On a
   speculative frame it acts as a prediction, and the real entry replaces it by rollback when it arrives.
   Before this change (3d0f98a), a hand without an entry stopped dead.
2. **Immediate local re-simulation on arrival [V, existing].** An arriving entry is applied at the next
   tick, within one frame (16.7 ms at 60 Hz, 11.1 ms at 90 Hz), by one coalesced rollback (6.5). It does
   not wait for confirmation.
3. **Render-only correction smoothing [V observer, computed offline].** When a rollback moves an object that
   has already been drawn, the renderer keeps drawing it where it was and drains the difference with a time
   constant τ. Sim state is never touched.

**Measured:** 500 balls at 60 Hz with 2% loss. Member 1 sweeps its hand and member 2 watches. Errors are
against the final confirmed history over the sweep.

| One-way latency | Remote hand error, mean / p95 (hold → dead reckoning) | Balls visibly corrected (>1 mm) per frame | Ball error p99, raw | Sim CPU per frame |
|---|---|---|---|---|
| 30 ± 5 ms | 24.0 / 33.5 → **1.3 / 2.0 mm** | 10.5 → **1.1** | 7.5 → **0.4 mm** | 3.0-3.9 ms |
| 60 ± 10 ms | 78.9 / 121.8 → **14.7 / 17.0 mm** | 21.7 → **8.6** | 30.2 → **3.6 mm** | 4.4-6.1 ms |
| 100 ± 16 ms | 148.9 / 197.3 → **40.7 / 37.0 mm** | 25.8 → **16.6** | 61.6 → **12.5 mm** | 6.1-8.1 ms |

[V observer, integration run with the engine default. The CPU column comes from 442286f on a quieter
machine; the integration run's CPU figures are low-priority noise.]

* **Dead reckoning cuts the observer's error by about 4-18x** for smooth hand motion, because the
  speculative sim pushes balls roughly where the real hand is pushing them. Its limit is any change of
  motion: a stop, a turn or a grab is seen one latency late, and the balls involved are corrected.
* **At a change of motion it overshoots briefly** (review PH-6) [V observer, round 3]. In the 0.25 s after
  the descent ends and after the sweep ends, its mean error is still lower than holding's: 23 against
  59 mm at 30 ms, 79 against 177 mm at 60 ms, and 171 against 323 mm at 100 ms. But its p95 and maximum
  are about equal or a little higher: 60 against 50 mm p95 at 30 ms, and a 797 against 758 mm maximum at
  100 ms. Those maxima come from the scripted path's own jumps, of up to 0.6 m.
* **Smoothing trades accuracy for continuity.**
  * At 60 ms latency, τ = 50 ms raises the p99 ball error against the truth from 3.6 to 7.3 mm, and
    τ = 100 ms to 12.1 mm. Corrections are shown late on purpose, but nothing jumps.
  * Large corrections still show as fast glides: up to about 10-18 m/s for the rare 1 m correction.
  * Proposal [I]: smooth corrections below about 5 cm over 50 ms, glide larger ones over 100 ms, and snap
    above about 0.5 m.
* **The hand rule is a world rule, not a local option.**
  * It shapes confirmed history wherever a confirmed frame has no entry from an active hand. Under round 2
    that happens by design when the author thins its hand stream (10.9): between two thinned entries,
    holding stops the hand, and dead reckoning carries it on to the target.
  * In the observer runs the author sends an entry every frame, and nothing is re-stamped any more, so no
    confirmed frame lacks one. Hold and dead reckoning now end in the same state there [V observer, round
    3; review PH-5], and the rule changes only what is shown before entries arrive.
  * At 442286f, before round 2, re-stamped late entries left gaps, and the two rules ended in different
    states [V observer-100 at 442286f].
  * Decision **D52**.
* **Input delay stays an opt-in setting with a default of 0 (D41).** It spares the receiver rollbacks, but
  delays the engager's own interactions by the same amount, against the requirement that every reaction
  be immediate.

### 6.11 What can and cannot be perfectly realtime

For @user, the honest limits under the core goal: every reaction physically realtime and reactive for
every player, perfectly synced, and optimized for CPU and memory.

**What can be realtime:**
* **Your own interactions, always.** Your hand and your pushes act on your sim at the present frame, with
  no added input delay (D41 default 0). Local physics answers within the same frame.
* **Perfect sync, always.** Every peer converges to a bit-identical state from the same log, and confirmed
  frames are identical everywhere [V: every run in this study].
* **Remote motion that continues smoothly.**
  * With dead reckoning, a remote hand moving steadily is shown within 1.3 / 2.0 mm (mean / p95) at 30 ms
    latency and 15 / 17 mm at 60 ms [V].
  * The balls it pushes react in the observer's view without waiting for its packets.
* **The balance of memory and CPU, chosen per device** without affecting anyone else (6.8).

**What cannot be realtime (physics, not engineering):**
* **Nobody sees another player's change of motion before its packet arrives.**
  * A stop, a turn, a grab or a throw reaches you one one-way latency later: about 15-40 ms within a
    region, and 75-150 ms across continents.
  * Until it arrives your view is a prediction; when it does, whatever it touched is corrected.
  * On 60 ms links, while a hand sweeps through 500 balls, about 11 balls per frame are visibly corrected,
    typically by a few millimetres.
* **Two players acting on one object at the same moment** each see their own action at once and the
  combined result one latency later. Exact agreement on a contested contact cannot be immediate for both.
* **CPU grows with latency × active bodies.**
  * Each late entry re-simulates every awake body from the entry's frame.
  * 500 swept balls cost about 3-4, 4-6 and 6-8 ms per frame at 30, 60 and 100 ms [V observer].
  * 1,024 balls at 60 ms cost about 10-13 ms per frame, with 99th-percentile ticks of 21-29 ms, above the
    16.7 ms frame [V snap-1024].
  * The snapshot knob cannot change this. It removes only cached gaps, not the frames between a
    perturbation and the present.
  * What can help:
    * region-scoped worlds (6.6, R16);
    * island-scoped re-simulation in a box3d-rust fork (6.9, R15);
    * multithreaded stepping in our fork, the path the user chose (D2), since box3d-rust 0.5 uses no
      threads [V: no thread use in the crate];
    * opt-in input delay;
    * fewer awake bodies.
* **The deepest rollback is bounded only by the speculation limit.**
  * An entry that arrives up to 2 s late (lost, then refetched after a lost fetch) re-simulates up to
    120 frames: 51 ms at 256 bodies and 190 ms at 1,024.
  * The work is spread over several ticks (6.5), and the view runs behind until it finishes.

---

## 7. Perturbations: state + canonical frame (R3, R6; rulings 09:30Z and 09:39Z)

### 7.1 The unifying model (user ruling 09:39Z)

* **Every programmable system runs locally and deterministically on every peer:** physics, WASM game
  logic, and the components logic drives.
* **The network carries perturbations only.** A perturbation is anything that changes the world line from
  outside the deterministic simulation.
* **Every perturbation travels as one signed entry carrying:**
  * its canonical frame (the author's present when it acted);
  * the perturbing input;
  * the **resulting state** of what it directly touched (user ruling 09:30Z).
* Receivers roll back to that frame, apply the entry, and re-simulate forward. That is the only path, for
  physics, logic and component state alike. There is no continuous state streaming.
* **This is Resonite's model made deterministic.** Resonite runs drives and ProtoFlux locally and
  replicates writes through the host. We run everything locally, and replicate perturbations peer to peer
  with rollback (3).

### 7.2 What counts as a perturbation

| Perturbation | Canonical frame | Resulting state carried |
|---|---|---|
| Player physics interaction (impulse, grab, release) | The author's present | Exact post-frame state of each touched body (7.3) |
| Engaged VR hand (a continuous source, 7.5) | Each frame of the stream | The hand target is itself the state; touched bodies are simulated, not sent |
| AI agent action through MCP (17) | The peer's present | As for the equivalent player action |
| Component write (MCP `object.set_component`, in-world inspector) | The peer's present | The written component values |
| Create, delete or reparent an object | The peer's present | The created object's record; the deletion; the new parent |
| Code, shader or asset commit (15) | Its activation frame (a confirmed commit plus a lead) | Module or asset hash; optionally the post-migration sync-state hash, so peers can check the migration |
| Cross-world transfer step (R14, 22.6) | fA in the source, fB in the destination | The full object record |
| Divergence repair (9.3) | A confirmed frame | A state transfer (image plus tables) |

* World logic (member 0) is **not** a perturbation. Every peer computes its actions identically, so they
  are never sent [V d1].

### 7.3 Resulting state, and the acceptance criterion

* **Contents:** for each body the entry touched, its exact post-frame state as bit patterns: position,
  rotation, linear and angular velocity, 13 × 32 bits = 52 bytes [V session-sim encoding].
* **Application rule** (applied identically by every peer, the author included, whenever frame *f* is
  simulated or re-simulated):
  1. apply the entry's input at the start of *f*, in canonical order (8.3);
  2. step;
  3. for each carried body, write its state with the carried bits, **always**, whether or not they already
     match (review SET-1). Writing a pose recomputes the body's centre of mass, rotation0 and world
     inertia, so a peer that skipped a write that changed nothing would still diverge at the next step on
     an off-centre body. The touch's impulse wakes the body at *f* on every peer first, so it is awake
     everywhere when the state is written (SET-2).
* **Why the rule is deterministic:** the write is the same operation on every peer, at the same frame,
  with bits taken from the log. In the confirmed history every peer holds the same log, so its world at
  *f* is the same, contact cache included, and the result is bit-identical.
* **Known gap, speculative only** [V D58 probe, @vdw-physics]: the write sets the body's bits but not its
  contact cache. A receiver whose copy differed (it lacked an entry, or is behind) gets the author's bits
  at *f* but keeps its own contacts, so its copy drifts by about 1e-12 after the next step. The confirmed
  history is unaffected, since every peer's cache is the same there. A pe-physics item is resetting the
  body's contacts on a write.
* **What the rule buys:**
  * a peer already on the author's world line writes the bits it already holds and stays bit-exact by
    determinism;
  * a peer that had diverged (it lacked an earlier entry, or hit a bug) gets the touched object pulled onto
    the author's world line at once;
  * so every interaction is also an immediate, object-scoped sync check: R6's "this sync also runs
    automatically whenever a player interacts with an object".
* **Acceptance criterion (user, 09:31Z):** after a receiver re-simulates to its present frame, its copy of
  the object must equal the author's copy at that frame. Measured:
  * **3 members, 60 ms ± 20 ms:** 22/22 uncontended checks equal, both to the author's copy at that instant and to its final history [V session-sim state-frame]. **Over real UDP between two processes:** 14/14 at 60 ms ± 20 ms, and 15/15 in the fault run [V p2p-v2]
  * **10% loss on every link:** 25/25 uncontended checks equal [V session-sim loss]; 20/21 over UDP, the one miss contended [V p2p-v2 loss10]
  * **A receiver that had missed an earlier entry on the same object:** 25/25 equal to the author's final history, and 23/25 at that instant: two checks were influenced indirectly through the pile (below). Its carried-state overwrite fired once and pulled the object onto the author's world line before the missing entry arrived [V session-sim diverged]. Checks are *contended* when another member touched the same object in the previous 10 frames; that is the scripted same-frame conflict on object 50, 10 checks per run. Those can differ while the competing entry is in flight, and all converge when it lands. Every run ends with all histories bit-identical [V]
* **The limit, stated plainly:** carried state makes the *touched object* exact. It cannot make an object
  exact while the receiver's *other* objects still differ, because they keep pushing on it. Bit-exactness
  everywhere comes from holding the same log. Gap detection guarantees that before a frame is confirmed
  (9.3), and a state transfer covers true nondeterminism (9.3, 9.5).
* **Contact island scope (D40):**
  * carrying the whole contact island of each touched body would make the local neighbourhood exact
    sooner, at 52 bytes per island body;
  * the recommendation is touched bodies only, plus an island digest so a mismatch can trigger a scoped
    state transfer.
* **Overwrites can erase late indirect effects.** An earlier-stamped entry that arrives late and pushed the
  touched object indirectly loses its effect on that object at *f*. The carried state wins, identically on
  every peer. Claims (8.4) prevent the direct case.
* **Nothing is re-stamped** (8.2), so every entry's carried state describes the frame it lands on, and is
  always applied.
* **A lossy session's outcome depends on network timing. This is deterministic given the log, but the log
  itself depends on timing** (reported by @design-reviewer with vdW on, user note 14:49Z).
  * The carried state is the author's *speculative* result: what the author's sim computed when it acted,
    with whatever entries had reached it by then.
  * So whether an earlier remote entry arrived before the author acted changes the bits it carries, and
    with them the final history. In the reviewer's runs the UDP `loss10` and `fault` final digests varied
    from run to run, while both peers and the offline replay always agreed.
  * This is the user's rule working as intended: *an author never sees its own interaction corrected.* The
    object the author touched ends where the author saw it end, on every peer. What varies with timing is
    whose view other objects follow when two players' effects meet.
* **"Contended" should mean causally connected, not "the same object"** [I, proposal].
  * With vdW on, uncontended acceptance at the instant in `diverged` is 23/25 (25/25 with upstream
    physics). Both misses are object 5, at frames 332 and 333. @vdw-physics traced the same misses
    (b8f130c): the author had not yet seen another member's touch on object 77, whose effect reached
    object 5 through the pile.
  * Final-history acceptance is 25/25 [V session-sim diverged, integration run].
  * The acceptance metric should therefore count as contended any check whose object shared a contact
    island with another member's touch in the previous 10 frames.

### 7.4 Logic and component state: what is synchronized, what stays local

* **Sync state of a logic module** (snapshotted, hashed, transferred; restored on rollback):
  * the module identity (wasm hash) and its schema version;
  * the component columns it declared through the IDL (16.5); the host stores them, so they snapshot as
    memcpy and hash by column;
  * one **declared state arena** in guest memory, for state that is not a component; the SDK's allocator
    keeps all persistent guest data there;
  * the module's RNG stream positions.
* The guest stack and scratch memory are at rest between ticks and excluded. Mutable globals other than
  `__stack_pointer`, and table mutations, are refused when a module is committed (review L3). Snapshots
  then shrink from about 1.1 MB of whole linear memory [V d1] to the arena plus the columns.
* **Local-only state never enters any of this:**
  * presentation modules and presentation components live in a separate store that the sim cannot read;
  * per-peer UI, panels, local head and unengaged hands, audio, particles and IK results are all local;
  * the host enforces the split: sim modules cannot import presentation or I/O APIs, and presentation
    modules cannot write sim components.
* **Logic perturbations** are exactly the rows of 7.2 that touch logic: player or AI input routed to
  logic, MCP component writes, a hot-swap at its activation frame, and a transferred object's components
  arriving through R14.

### 7.5 Continuous perturbations: a hand sweeping through a ball pit (the main question)

Read naively, the state+frame ruling would send every touched ball's state every frame while a hand sweeps
through a pit. The spike measured that against the alternatives: 500 balls (radius 6 cm) in a 2.4 m pit,
a kinematic hand (radius 9 cm) sweeping a sine path for 3 s, two peers, 60 ms ±10 ms one-way, 2% loss,
release build [V session-sim ballpit].

| Run | Our traffic, both directions (wire est.) | Naive state streaming, one direction | Receiver: rollbacks/s, re-sim per sim frame | End state |
|---|---|---|---|---|
| 60 Hz, 1 hand, 2% loss | 14.7 kB/s (hand entries 3.7 kB/s payload) | 1.00 MB/s exact; 0.47 MB/s quantized (≈26 B/ball) | 59/s, 2.76 ms (step 0.35 ms) | identical |
| 60 Hz, no loss | 12.4 kB/s | 1.00 MB/s | 59/s, 2.69 ms | identical |
| 60 Hz, input delay 4 frames | 14.7 kB/s | 0.97 MB/s | 2.7/s, 0.27 ms | identical |
| 90 Hz | 22.4 kB/s | 1.44 MB/s | 88/s, 3.95 ms (of an 11.1 ms frame) | identical |
| 60 Hz, two hands (one per peer) | 21.6 kB/s | 1.23 MB/s | 59/s, 3.01 ms | identical |

* The pit settles first: 0 balls changed state per frame before the sweep. During the sweep about 297 of
  500 balls moved every frame.
* "Wire est." assumes one datagram per message, with 52 bytes of IP/UDP/AEAD overhead. Batching per tick
  lowers it.
* "End state" is the full state hash (every ball, both hands, every table) at the last common frame. Every
  run's whole history also equalled an offline replay.

**Options weighed:**
* **(a) The hand as a perturbation source (recommended).** The hand's target pose is sent every frame as a
  small entry: the hand's state plus the canonical frame, quantized to 1 mm. Every peer simulates the balls
  from it. Start and end are ordinary entries, and the touched island's state is sent only when a sync
  check flags divergence [I; the spike compared end-state hashes instead]. This satisfies the ruling, because the perturbation (the hand) carries its state
  and frame and receivers roll back and re-simulate. **Measured:** about 15 kB/s for everything, in both directions together, against about 1 MB/s one way for naive exact state streaming: roughly 1/70 of the volume. The cost moves to CPU. The receiver re-simulates every frame, in one coalesced rollback about 5 frames deep, which costs 2.8 ms per 16.7 ms frame at 500 balls [V].
* **(b) Batch, delta-code and quantize island state.** This cuts naive streaming several-fold (measured
  above). But quantized state is not bit-exact unless the sender also snaps its own copy to the quantized
  values, and it still scales with the number of balls moving.
* **(c) Coalesce late arrivals:** one rollback per tick, to the earliest new entry. Built in and measured
  above. Bounding the depth is the speculation limit (6.4).
* **(d) Two hands in one pit:** both are perturbation sources. Balls are not claimed by hand contact, so
  physics resolves both hands together, deterministically. Only exclusive grabs use the tie-break (8.3).
  Measured with two hands, one per peer, sweeping in opposite directions: end state identical, 21.6 kB/s, and the receiver's re-simulation cost about the same as with one hand [V].
* **Input delay** (stamping hand targets a few frames ahead) removes most receiver rollbacks, but the
  author's own balls then react late to its hand. Stamping hand targets 4 frames (67 ms) ahead cut the receiver's rollbacks from 59/s to 2.7/s, and its re-simulation from 2.76 to 0.27 ms per frame [V]. It is an opt-in per-world setting with a default of 0 (D41): it delays the engager's own interactions by
  the same amount. Observers are served by remote-hand dead reckoning instead (6.10).

### 7.6 Bandwidth per interaction

* **An entry is about 100-170 bytes before transport overhead** [V session-sim encoding]:
  * a touch with carried state is 100 bytes;
  * a hand target without state is 44 bytes.
* **Acks** grow with the member count (review R3-11): 56 + 16(N-1) bytes with a check hash, that is 72, 88,
  104, 168, 296 and 1,064 bytes for N = 2, 3, 4, 8, 16 and 64 [V reviewer]. Every member sends one to
  every other member every two frames.
  * Idle upload per member is therefore 3.7, 8.4, 14, 46 and 157 kB/s, and 2.1 MB/s, for those member
    counts [V reviewer]. That is over the 1-5 kB/s budget (2.1) from 3 members. Production ids and
    signatures (32 bytes) would add about 2-2.5x [I].
  * **Compact acks** [I]: carry only each head's seq (a varint), plus one 8-byte digest over all
    (member, seq, id) heads. A receiver that holds those entries checks the digest; a mismatch (an
    equivocation) asks for the full heads. That is about 8 + 2N bytes plus horizon and check hash, about
    4-5x less at N = 16.
  * Region logs (R16, 25) keep N per log small. A log of many members also needs acks to a few peers at a
    time, relayed (gossip), at the cost of slower stability [I].
* In production all of these share datagrams: the per-tick batch is option (b) applied to entries, not
  to state.

### 7.7 What traffic R3 allows (review L9)

R3 says the network carries changes "only when a player interacts". The design also sends:
* acks and check hashes (they replace nothing local and carry no state);
* clock pings;
* presentation streams: remote avatar poses and panel views (never sim inputs);
* panel and hub messages (21).

Whether that reading is right is **D39**.

---

## 8. "Soonest wins" without a central authority (R5)

### 8.1 Canonical frame

An entry's canonical frame is the frame its author stamps it with: the author's present when it acted. Its
own effect is applied locally at once, so the author sees no added delay.

### 8.2 Effective frame: an entry takes effect at its stamp (reviews H5, NEW-1; user rule 14:31Z)

**The rule: an entry's effective frame is its stamp. Nothing is ever re-stamped.**
* 3d0f98a moved late entries to "the lower median of the other members' arrival frames, minus a 15-frame
  allowance". That rewrote authors' own past.
  * At 300 ms one-way, the engager rolled its own history back 107 times, up to 76 frames deep, and its pit
    visibly snapped (@ballpit-viz, viz-2).
  * The late reports it depended on were per-receiver ack contents, which forked members (NEW-1), and they
    grew without bound under congestion (viz-3).
* **User rule (14:31Z): an author never sees its own interaction corrected. Lateness is paid by the late
  receiver**, through a deeper rollback or a bounded catch-up.

**What still bounds backdating:** two chain-local rules. Every member that holds the author's chain applies
them identically, so they can never fork anyone.
1. **Published horizons are binding.** An entry stamped at or before a horizon that its author declared
   earlier in its own chain is void for every member [V session-sim backdate part 1: three entries stamped
   20 frames back were void everywhere].
2. **A horizon may not lag its own ack.** An ack whose horizon is more than `horizon_slack` frames
   (default 2) behind the ack's own stamp is void.
   * So a member cannot hold a backdating window open by publishing stale horizons.
   * Trying only stops confirmation: every member voids all such acks, and the others demote that member
     after 3 s (10.9 item 7) [V session-sim backdate part 2: its acks void for every member, no window
     opened, member 3 removed, and members 1 and 2 equal to the replay].

**What a cheater can still do:**
* backdate within one ack interval plus the slack, about 4 frames (67 ms) at the defaults;
* claim a present that lags the true one. That looks exactly like a slow link: it is accountable (others
  see the member's stamps consistently trail their own clocks) but not provable.

Accordingly, the residual H5 risk is smaller than before (about 4 frames instead of about L = 15), not zero.
A cheater can still pick, within that window, a frame where `tie_rank` favours it (8.3). Strangers may join
from day one (D21, user 18:56Z), so this window is a stated MVP residual, bounded but not closed.

**Beyond any hard bound:** a member whose entries or acks persistently arrive more than half the
speculation limit late stalls everyone's confirmation. It is demoted to a spectator by a view change rather
than allowed to rewrite others' history (10.9). The decision is **D42**, revised.

### 8.3 Total order and the tie-break (user ruling 09:32Z)

* **Order key:** `(effective frame, tie_rank(seed, frame, member), member, seq)`.
* `tie_rank` hashes the epoch seed, the frame and all 32 bits of the member id (review L7). It is a
  **per-frame random permutation of members.**
* **It cannot be ground:**
  * member ids are assigned in admission order by the membership log (10.8), never chosen by the peer;
  * the seed comes from the session or epoch genesis entry;
  * so no vanity key or chosen id changes anyone's odds.
* Decisions: **D37** (the tie-break and member ids); the reading of "soonest" itself is **D5**.
* **No permanent winner:** each member tops the order in about 1/n of frames. Measured: 150 same-frame conflicts among 3 members gave 41, 50 and 59 wins (expected 50 each; χ² = 3.24, p ≈ 0.2) [V session-sim ties].

### 8.4 Claims: grabs only (review M3; D72, user ruling 18:56Z)

* **Only grabs claim** (D72 option (a)). An accepted grab claims its object while it is held; another
  member's grab of the same object in that time is refused with `RejectedClaimed{by, until}`.
* **Every other physical interaction is never refused.** Touches, pushes and impulses all apply,
  physically, including on a held object, which they move through the grab's constraint.
* **A member moves only its own hands** (P2-8 review: @shader-reviewer's probe moved another member's
  hand through `pe-mcp`, and every peer accepted it).
  * Hands are part of a member's avatar (16.3), and an entry's author is its chain's signed member (10.3).
    So every replica refuses any entry that names a hand that is not its author's, with
    `RejectedNotOwner`: a hand move, a touch (so it takes no claim that would block the owner), and a
    carry through a portal (review HO-1). It is a validity rule like the others, not a claim: it holds at
    every frame.
  * A touch carries the resulting state of the object it touches and no other (round 2 of HO-1): a touch
    whose carried state names another object is refused, `RejectedInvalid`, before it takes any claim
    [V handowner: member 2's 46 hand entries, its grab and its carry of member 1's hand, and its touch of
    a ball carrying a state for member 1's hand are all refused at both replicas; member 1's own touch of
    its hand right after is accepted; the history equals the run without member 2's entries. With the
    carried-state check switched off, member 1's history changes].
  * Hands are the first owner-only objects; avatar parts and any other per-member object join the class
    as each kind is added [I]. In the spike ownership is fixed from the genesis list, so a joiner owns no
    hand (review HO-2); pe-session keeps it in the hashed state, set by a confirmed entry (P2-3).
  * An agent at an unlocked panel acts as the panel's owner (D27a). The panel's messages carry their
    verified sender, but the owner's peer signs the entries, so the agent moves only the owner's hands.
    `pe-mcp` also refuses such a move before authoring it (`not_owner`, 17.2).
* **The one stated exception** to "an author never sees its own interaction corrected" (14:31Z): when two
  grabs of one object land within one latency, the loser's grab is undone on re-simulation. It stands
  next to demotion (10.9) and portal carries (22.7).
* A grab renews its claim while held and lapses on release, followed by a 10-frame cooldown; there is no
  cap on how long a player may hold an object (D38).
* **World logic (member 0) never claims** and is never refused by claims.
* Agents are rate-limited at intake (17.4).
* The spike still implements the earlier rule, in which every interaction claimed its object for 30
  frames [V session-sim claims]. Phase 2 changes it (23.2).

### 8.5 Fairness

* "Soonest" is fair to the precision of clock sync plus the tie permutation. The spike's same-frame
  conflicts were scripted (review L5), so fairness under real skew is [I].
* Cheating is bounded (8.2), not prevented. Strangers may join from day one (D21): the phase-2 view change
  removes silent, withholding or colluding members (10.8), and provenance checks guard objects from other
  worlds (D46).

---

## 9. Sync checks and divergence repair (R6)

### 9.1 What is hashed

* **Every frame:** Box3D body transforms and velocities, plus every engine table (claims, objects, escrow,
  arrivals) and the frame number. This is cheap and used for local diagnostics.
* **At check frames:** the **full world image plus tables** (review H4): everything that rolls back,
  including contacts, warm-start impulses and sleep timers.
  * The image is made canonical before hashing: it is restored into a scratch world and serialized again
    (review NEW-7).
  * Without that step, a world that rolled back across a destroy and one that stepped straight through it
    held identical physics but different image bytes. The `r14-throttle` run then reported 24 false
    mismatches.
  * The round trip costs one restore plus one save per check frame (every 30 frames) [V session-sim
    r14-throttle: 18 ok / 0 bad in world A, 15 ok / 0 bad in world B].
* **Objects:** the per-object state hashes that carried state (7.3) is compared against.
* **The hash (P2-1)** [V P2-1]: XXH64, pinned in-tree and checked against the reference crate, over the
  sim store's canonical content: the frame, the seed, the objects, the creator counters and every
  non-empty column with its schema hash. State reached through different operation orders hashes and
  serializes identically. The spikes used byte-wise FNV-1a, which is several times slower on whole
  columns. `checked_state_hash()` scans the values first, so a stray NaN is a deterministic error on
  every peer, not a hash of platform-specific NaN bits; the per-frame hash uses it.
* **Which hash decides what a peer accepts** (review P22R-1, design-wide) [I]. XXH64 is not a security
  boundary: its lane steps are invertible, so a peer that sends state bytes can change them and then
  choose one 32-byte stripe that restores the hash. So:
  * the **check hash** that acks carry at check frames, and that a member compares before it accepts
    state from another peer (9.5, 25.5), is **BLAKE3** (pure, as D16 uses for content), 256 bits, over the
    canonical bytes of the whole check-frame state: the physics image and the sim store's columns and
    tables alike. The physics part already is (`pe_physics::CheckHash`, P2-2); the sim store's part,
    today `checked_state_hash()`'s XXH64, moves to BLAKE3 too (review P22S-1). It is one hash, so
    divergence detection and transfer trust use the same value;
  * XXH64 stays for the per-frame hash and local diagnostics, where nobody else chooses the bytes;
  * the reader of a state transfer consumes it completely: no trailing bytes, no unreferenced blobs,
    and free fields (names, bounding boxes) canonical or refused, so the hash binds exactly the state
    it restores;
  * the cost, about a millisecond or two of BLAKE3 per check frame for a 1.5 MB image, is to be measured
    in P2-3.
  * the exact composition P2-3 hashes (session `transfer.rs`): `BLAKE3("pe-check" | physics CheckHash |
    frame u64 | store_len u64 | store bytes | tables_len u64 | tables bytes)`, all integers little-endian.
    `"pe-check"` is 8 ASCII bytes; the physics `CheckHash` is 32 bytes (a pe-physics BLAKE3 derive-key over
    the `Transfer` bytes); `frame` is the replica's frame after stepping check frame F, i.e. F + 1 (acks
    key the hash by F); `store` is `pe_sim`'s `Snapshot::to_bytes`; `tables` are the escrow, arrivals and
    seen maps, each a u32 count then entries sorted by key (reusing the entry codec's hop, epoch-cert and
    cert layouts), then a u8 portal flag. `decode_tables` accepts only bytes that re-encode identically,
    are fully consumed, and whose cached chain hashes match their chains (9.5).

### 9.2 Cadence

* **On every interaction:** the carried state is compared with, and if needed applied over, the
  receiver's own result (7.3).
* **Periodic:** every 30 frames, but only at **confirmed** frames that this member has finished simulating
  with the complete log. A member with a re-simulation pending never compares (viz-1).
  * There, every member holds the same entries by construction, so any mismatch is true nondeterminism,
    never a missing entry (review H4).
  * The check hash travels inside the author's ack entry, so a member that shows different hashes to
    different peers is equivocating (NEW-1).
  * Measured: 0 mismatches in every session-sim scenario, including `r14-throttle`. `run_session_sim.sh`
    now fails on any bad check [V]. Every UDP run: 24 ok, 0 bad [V p2p-v2].

### 9.3 Repair

* **Missing entry (the common case).**
  * Each ack entry lists the head that its author holds of every other chain. An entry that arrives past a
    hole in its author's chain shows the hole directly.
  * Either way the member fetches the missing seqs, from the author or from whoever holds them, before the
    frame can be confirmed. Then it rolls back to the entry's canonical frame.
  * That frame *is* "the earliest frame at which a player interfered with the object" (R6), so no
    hash-range search is needed.
  * [V session-sim diverged and loss (10% loss on every link): all histories bit-identical and equal to an
    offline replay. V p2p-v2 fault: the dropped entry's hole was seen 2 frames later and fetched.]
* **Equivocation (reviews C2, NEW-1, NEW-2).** Two signed versions of one (member, seq) are a transferable
  proof.
  * Every member that holds the proof:
    1. **freezes** what it will ever reference of that chain;
    2. publishes a **vote** entry naming its frozen head;
    3. broadcasts both versions.
  * **Before eviction is decided**, every entry from the disputed seq on is withdrawn, except those that
    are already **stable**: held identically by every member, and so possibly confirmed somewhere.
  * **Eviction** is decided once every remaining member has voted (a strict majority of the epoch) and
    every vote is **stable among the remaining members** (review R3-1).
    * **A vote is checked against its voter's own chain.** Its frozen head is self-reported, so it counts
      only if it extends every reference to that chain that the voter's acks published before the vote.
      A vote that drops one is a provable lie: it is void and logged.
    * **After voting, a member's references to that chain no longer count** toward stability. Its frozen
      head is its last word.
    * **Votes are used only once every remaining member holds them identically.** A voter that sends
      different votes to different members can then stall the eviction, but cannot give two honest
      members two different cuts.
    * The **cut** is the longest common prefix of the frozen heads of the valid votes.
    * Entries after the cut are void. The evicted member stops counting from the frame after its last valid
      horizon.
    * Any stable entry is inside the cut. Every valid voter referenced it before voting, and its frozen
      head extends those references. So no confirmed frame ever changes, even when members collude. This
      is virtual synchrony's flush, decided from chain data alone.
    * **Stability never shrinks:** once every member has referenced an entry, a later conflicting
      reference cannot withdraw it (R3-1).
  * **What colluders could still do in round 3** [V badvote at 11185cc]: stall. Two colluders of four left
    no honest majority, so nothing could be decided without one of them.
  * **Since P2-4 the view change decides every removal** (10.8): a quorum's votes set the cut (with t = 0,
    the rule above), members proven faulty count in no quorum, and a forked vote is itself a proof. So
    `badvote` and `liar` now remove both colluders and the session goes on [V session-sim, 5k].
  * Measured on the reviewer's repros, ported to session-sim [V]:
    * `ackfork`: member 9 showed member 2 a different chain. Both honest members found the conflict,
      evicted 9 with a cut before its touch, and agreed. Before, they forked.
    * `lateequiv`: a second version of an already-confirmed entry. The stable version stood, 9 was evicted
      (cut at seq 103, from frame 206), and nobody panicked. Before, every honest member crashed.
    * `evict`: after an eviction, a late entry kept its frame (60) on every member. Before, a member crashed.
    * c2 equivocation before confirmation: as in 3d0f98a, both members ended equal and equal to the replay.
    * `badvote` (round 3): 9 equivocated on a touch confirmed at frame 50. Colluding member 8 showed the
      honest members a forged vote with frozen head (0, 0), and kept its genuine vote to itself.
      * Before: the cut fell to 0, and every honest member panicked.
      * Now: the forged vote never became stable, so the eviction waited. Both honest members stayed
        equal to each other and to the replay, at confirmed frame 203, paused at the speculation limit.
    * `liar` (round 3): as `badvote`, but 8 cast only the forged vote and otherwise behaved. Its vote
      contradicted its own acks and was void. 9 was evicted with a cut that kept the confirmed touch (seq
      104, from frame 208), and confirmation resumed.
  * Decision: **D55**.
* **Same entries, different state (true nondeterminism):** never observed in any spike.
  * With 3 or more members, the majority state at the confirmed frame wins and minority members take a
    state transfer.
  * With exactly 2 there is no majority. Both re-simulate from the last agreed confirmed frame; if they
    still differ, both pause and report.
  * No member's state is privileged (review M7, D6).

### 9.4 Reliability

* Acks are chain entries, so a lost ack is a hole that gets fetched like any other entry. A member
  therefore never misses another's horizon or check hash.
  * The 3d0f98a ack repeated late reports only until stamp + 2W, which forked members after a 6.5 s
    outage (NEW-1). Under the round-2 protocol, members 2 and 3 both rolled back 38 frames to the late
    entry, and all three histories matched the replay [V session-sim outage].
* Fetches are rate-limited per (holder, author) and retried while a hole persists.
* Under 10% loss: all three histories bit-identical, 51 of 51 acceptance checks equal to the author's final
  history [V session-sim loss]. Over UDP with 10% loss: both peers equal, 24 ok / 0 bad checks
  [V p2p-v2 loss10].

### 9.5 State transfer (late join, minority repair, long-parked worlds) [I]

* The donor sends a snapshot at a **confirmed** frame *F*: world image, tables, sync state of the logic
  modules, module set, claims and asset manifest. It follows with the session log from *F*.
* The receiver restores and fast-forwards. Restoring a world image into a fresh `World` works [V b4].
* **It verifies before it restores** (review P22R-1): it restores only bytes whose BLAKE3 equals the check
  hash for frame *F* (9.1), and only if it consumes them completely. So a hostile donor cannot pass off
  another state; it can only fail to deliver, and the receiver asks another member [I].
* **Which check hash it trusts** (review P22S-1): one that a quorum of the epoch's members signed
  identically in their acks for frame *F*, at least one of them a member other than the donor.
  `from_transfer` checks the bytes against whatever hash it is given, so the caller must establish the
  hash this way, never take it from the donor. A world with a single member therefore cannot vouch for
  its own transfer: the receiver trusts that member or waits for a second one [I].
* Images are memory layouts tied to the engine build. Admission already requires an identical build
  (4.3 rule 7).
* Region- or island-scoped transfer is a later optimization.

---

## 10. P2P topology, transport and the session log (R1)

### 10.1 Topology [I]

* **MVP: up to 8 players per session, all directly connected** (user 18:51Z, D23). Traffic is small: interactions, hashes,
  engaged-hand streams. Remote-avatar pose streams dominate, at roughly 3 poses x 28 bytes x 60 Hz = 5 kB/s
  per pair.
* **Larger sessions are a planned target, not an option** (same ruling): compact acks (7.6, D64) and
  epidemic or Plumtree-style relaying, where any peer forwards for others. No peer is special. Regions
  (25, D60) keep spread-out players cheap; a crowd in one place is capped at 16 active players per region
  (D62).

### 10.2 Transport options (100%-Rust audit in section 20)

| Option | 100% Rust? | Notes |
|---|---|---|
| **A. UDP + Noise (snow) + own reliability/congestion** | Yes, with `default-features=false, features=["default-resolver-crypto"]` [V audit `net-snow-nostd`]. snow's default `std` feature force-enables `ring` [V audit `net-snow-udp`; S its feature table] | Fits rollback netcode: unreliable datagrams plus redundancy, our own acks. Bulk transfers (assets, snapshots) need a congestion-controlled stream. We write it (LEDBAT-like) [I]. The spike already runs signed UDP [V c1-c3] |
| B. QUIC via quinn with a pure-Rust crypto session | quinn-proto core without TLS is pure Rust [V audit `net-quinn-no-tls`]. `rustls` + `rustls-graviola` is pure Rust [V audit], but graviola uses Rust inline assembly (149 `asm!` blocks) [S] | quinn's built-in rustls integration can only be enabled with `ring` or `aws-lc-rs` [S quinn feature table], so we would implement quinn's crypto traits ourselves. We get mature congestion control and streams |
| C. iroh (QUIC + hole punching + relays + discovery) | No: `ring` + blake3 C/asm [V audit `net-iroh`] | Most turnkey. Default relays are run by n0, a soft central point |
| D. libp2p (kad, mdns, quic, relay, dcutr) | No with QUIC (`ring`, `aws-lc-sys`) [V audit `net-libp2p`] | Large dependency tree (240+ packages) |

**Recommendation (D8): A for the MVP** (sim channel plus our own bulk channel). B stays the upgrade path
if our bulk transport proves weak.

### 10.3 Identity and security [V c1-c3 for signed datagrams; I production]

* **Keys:**
  * each user has a long-term Ed25519 identity key;
  * each device has its own key, certified by the user key;
  * each agent has its own key with a delegation certificate (12.3).
* **Member ids (review L7).** A session's membership log admits members one at a time. Each admission
  assigns the next **MemberId** (u32) and records the member's full public key and device certificate.
  * Ids are never chosen by the peer and never reused.
  * A peer that crashes and rejoins is admitted again under a **new** MemberId, so its sequence numbers
    can never collide with its earlier incarnation's (review C2).
  * Object ids `(creator MemberId, counter)` stay unique for the life of the session. Objects that
    persist in the world beyond it are stored under the world-scoped form (session id, creator MemberId,
    counter), so ids from different sessions never collide in a save, a load or a merge (16.10).
* **Entries (review C2):**
  * every entry is signed by its author and carries the id of the author's previous entry, so each
    member's entries form a hash chain;
  * an entry's identity is the hash of its signed bytes. Production uses BLAKE3; the spike uses 64-bit
    FNV over the encoding [V session-sim];
  * per-entry signatures let any member forward any entry, which fetch-based repair (9.3) needs.
* **Transport:** Noise XX/IK bound to the identity keys, with AEAD (ChaCha20-Poly1305) per packet.
* **Authorship (review L4):** receivers check that an entry's member matches its signature. The phase-1
  UDP spike accepted any peer's claimed member id inside a datagram; its successor checks it
  [V p2p after the update].

### 10.4 Clock sync [V c1-c3]

* NTP-style ping/pong. The offset comes from the minimum-RTT sample. Measured error was 0.02-5.9 ms on
  loopback with 10-60 ms of injected one-way jitter.
* **Clocks are monotonic** (review L6). The offset is slewed, never stepped.
* **RTT is measured by the receiver, from its own send and receive times** (review H5). The remote's
  reported receive time is used only for the offset, never to excuse lateness. Since round 2 nothing is
  re-stamped (8.2): clocks only place an author's entries at its own present.
* Continuous re-estimation, and a median across peers [I].

### 10.5 NAT traversal without a central server [I]

1. **Reflexive address discovery:** ask already-connected peers "what address do you see me from?"
   (STUN-like, but peer-provided).
2. **Port mapping:** UPnP-IGD (`igd-next`, pure Rust [V audit `discovery`]), NAT-PMP or PCP where routers
   allow it.
3. **Peer-introduced hole punching:** a member connected to both sides relays the two peers' candidate
   addresses and a synchronized "punch now" time.
4. **Peer relay fallback:** a member with an open port relays traffic for a symmetric-NAT peer. That peer
   has no authority, and traffic stays end-to-end encrypted.
5. If **nobody** in the session is reachable (both ends symmetric NAT and no relay-capable member), only a
   public relay helps. That is a soft central point: **D9**.

### 10.6 Join, leave, partitions [I; join building blocks V b4, c1; membership V session-sim c2]

* **Join:**
  * the invite carries addresses and the inviter's key (11);
  * an admission entry, signed by a quorum of the current members (10.8), gives the joiner its MemberId
    and the frame from which it counts towards confirmation;
  * the joiner takes a state transfer at a confirmed frame (9.5), fetches missing assets and wasm by hash
    (16.7), fast-forwards, and starts acking and authoring.
* **Leave:** a graceful leave is a signed entry. Silence leads to a stall and then a view change (10.8).
* **Partitions:** only a side holding a strict majority of the current epoch can confirm frames or change
  membership. A minority side speculates up to the speculation limit and then pauses. On heal it rejoins
  the majority's epoch by state transfer. Its unconfirmed entries can be resubmitted as new entries. No
  confirmed history is ever discarded (review H3). Two-member sessions are **D7**.
* **Always-on peers** (like Resonite's headless clients) are ordinary members that keep a world running.
  They have no authority.

### 10.7 e4mc lessons (user question)

**How e4mc works.** Sources: the e4mc README, and the relay's Rust source (`vgskye/e4mc-quiclime`, branch
`kity`), read 2026-10-30.

* The Minecraft host (the mod) opens an **outbound QUIC connection** to an e4mc relay, "QUIClime"
  (Rust, quinn + rustls, ALPN `quiclime`) [S].
* The host requests a domain. The relay assigns **`<word>-<word>.<base domain>`** from a 7,776-word list.
  The base domain is relay configuration (`QUICLIME_BASE_DOMAIN`) [S].
* **Vanilla clients** connect over TCP to that hostname. The relay reads the hostname from the Minecraft
  handshake packet and forwards the connection into the matching host's QUIC tunnel. No port forwarding is
  needed, and joining clients need no mod [S].
* **Dialtone:** a host can register an iroh ticket with the relay, which serves it at
  `/.well-known/dialtone_ticket/{domain}`, so e4mc-aware clients can switch to a direct iroh connection
  [S relay code; I for the client-side flow].
* The public relays are run by e4mc's author, so reachability depends on them. The relay is open source
  and configured by environment variables, so anyone could host one under their own domain [S config;
  I whether others do].

**Lessons for Primordial [I]:**
1. **Memorable addresses.** Random words are easy to say and share. We adopt human-readable **session
   codes** without DNS. A code has two parts that do different jobs [I]:
   * **Form:** `orbit-velvet · crimson-lantern · tide`.
     * The first pair is the **nameplate**. It is public and only locates someone to talk to.
     * The second pair is the **password**. It is secret and used only inside a PAKE.
     * The last word is a **check word**, used to catch typos.
     * All words come from a 7,776-word list, so each word carries log2(7776) = 12.9 bits.
   * **The code's creator announces itself under the nameplate.** The member who creates the code announces
     its address with BitTorrent `announce_peer` (BEP 5) under
     `infohash = BLAKE3("primordial/code/v1" ‖ nameplate)[..20]`. A relay address (10.5) also works. The
     creator re-announces until the code expires.
   * **The joiner finds the creator, then proves the password.**
     * The joiner's peer checks the check word locally, so a typo never costs an attempt.
     * It calls `get_peers(infohash)` and contacts at most 8 of the returned addresses.
     * With each, it runs a PAKE (SPAKE2, or CPace; neither pure-Rust crate is audited yet), with the
       password as input and the nameplate bound into the transcript.
   * **On success, the creator sends a normal invite ticket** (11) over the PAKE-keyed channel: session ID,
     member addresses, the inviter's key and a join secret. The join then proceeds as for any invite
     (10.6).
   * **Nothing derived from the password is ever published.** The DHT holds only the nameplate's infohash
     and the creator's address. No signed session record exists to forge.
   * **Limits:**
     * a code expires after 10 minutes, or after one use unless the owner allows more;
     * the creator accepts at most 10 failed PAKE runs per code in total, and at most one per source address
       per 5 s, then burns the code and tells its owner;
     * joining still passes the session's own admission rule, if it has one.
   * **Shorter form, through a friend.** If the joiner already knows a member (a friend record, 11), the code
     is just `crimson-lantern · tide`. The joiner contacts that member directly and runs the same PAKE.
     Nothing is published at all.
   * **Why the earlier scheme failed:**
     * A three-word code holds 3 × 12.9 = 38.8 bits (7,776³ = 4.7 × 10¹¹ codes); the check word adds none.
     * If the DHT key is a hash of the code, anyone who sees the key can test every code offline: about
       8 minutes at 10⁹ hashes/s, one GPU. That also yields the PAKE password, which defeats the PAKE.
     * A memory-hard KDF does not rescue 38.8 bits. At an assumed 10³ Argon2id guesses/s per GPU, one code
       costs about 1.3 × 10⁵ GPU-hours. But every guess is tested against all live codes at once, because
       a joiner who holds only the code cannot use a per-session salt. With 1,000 live codes, breaking
       one takes about 130 GPU-hours [I].
     * The earlier record was "signed by the session key", but a joiner who knows only the code cannot know
       that key. Deriving the key from the code instead lets anyone who recovers the code overwrite the
       record.
   * **What the new scheme gives** [I; it assumes a secure PAKE, which allows one password guess per run]:

| Attack | Bound |
|---|---|
| Offline guessing of the password | Not possible: nothing derived from it is published or sent outside a PAKE run |
| Online guessing against the creator | At most 10 guesses per code: 10 / 7,776² = 10 / 6.0 × 10⁷ ≈ 1.7 × 10⁻⁷ |
| Fake creators announced under the nameplate | Each joiner attempt contacts at most 8 candidates: ≤ 8 / 6.0 × 10⁷ ≈ 1.3 × 10⁻⁷ per attempt, and failed runs are shown to the joiner |
| Nameplate collisions (honest) | With 10⁴ live codes, a given code shares its nameplate with probability ≈ 1.7 × 10⁻⁴. The only cost is failed PAKE runs, which can burn a code; the owner makes a new one |
| Burning someone's code (denial of service) | Ten wrong guesses burn it. Low impact: the owner makes a new code |
| Someone who overhears the whole code | Can join. A code is a bearer invitation, like a ticket. Use single-use codes, or admission approval |

   * **Costs:**
     * the code grows from three words plus a check word to four plus a check word (ease of use, D35a);
     * the creator must stay online and reachable, directly or through a relay, while the code is live;
     * DHT observers learn that the announced address serves a Primordial join code, much as BitTorrent
       peers are visible. Announcing a relay address hides the creator's address (D35b).
   * **Rejected alternative:** binding a key fingerprint into the code. Resisting offline search and
     second-preimage attacks needs about 6 words (77.5 bits) plus a check word, which is not memorable.
2. **Outbound-only reachability.** An e4mc server behind NAT stays reachable by holding an outbound
   connection to a relay. Our equivalent: members with open ports, plus **volunteer relays** that anyone can run (the
   normal peer binary in relay mode).
   * They forward only end-to-end-encrypted packets, addressed by peer public key.
   * Each peer picks the relays it trusts.
   * Relays affect availability only. They have no authority, and cannot read or forge traffic.
3. **Relay first, then direct.** Dialtone and iroh both start relayed and then upgrade to a direct path. Our
   join flow does the same (10.5).
4. **Not copied:** routing by DNS hostname needs a domain operator, which is a central point. Vanilla-client
   compatibility doesn't apply, because every client runs our engine.

**iroh against our constraints:**

| Criterion | iroh 1.4 |
|---|---|
| Dial by public key | Yes: `EndpointId` is the Ed25519 public key [S docs.rs] |
| Hole punching with relay fallback | Yes: connects via a relay, then upgrades to direct [S docs.rs] |
| Relays | n0-operated by default; relays forward traffic they cannot decrypt [S docs.rs]; self-hosted relay server [I] |
| Discovery | DNS-based address lookup publishing to a DNS service (n0's by default) [S docs.rs]; pkarr / Mainline DHT and mDNS options [I] |
| R1 / R9 | No authority over sessions. The default relays and DNS are n0 infrastructure, so soft central points unless self-hosted |
| 100% Rust | **No:** `tls-ring` (default) or `tls-aws-lc-rs`, both compiling C/asm, plus blake3's C build [V audit `net-iroh`; S feature table]. Using it needs a fork with a pure-Rust rustls provider, or a user exception |

**Verdict.** iroh's model is the one 10.5 proposes: key addressing, relay-assisted hole punching,
self-hostable relays. The recommendation stands (D8): build that model on UDP + Noise in 100% Rust, add
session codes and volunteer relays (D35), and reconsider iroh only if the user allows `ring`.

### 10.8 Session log, confirmation and membership epochs (reviews C1, C2, H3, NEW-1, NEW-2)

**Model.**
* Each member's signed, hash-chained log is like a Secure Scuttlebutt feed. Together the chains form the
  hash graph of Byzantine Eventual Consistency (Kleppmann and Howard 2020).
* Stability is Wuu and Bernstein's 2-D time table (1984): an entry is known to all once every member's row
  covers it. Here the rows are signed chain entries, so a member that shows two rows has equivocated.
* Eviction follows virtual synchrony's view change (Birman): survivors agree on a cut of the evicted
  member's messages.
* A general view change (several members at once, a join) needs agreement under partial synchrony: one
  Byzantine-tolerant decision per epoch, here Tendermint's (Buchman, Kwon and Milosevic 2018) over chain
  entries (P2-4, below).

**Chains.**
* Every member's entries form a hash chain: each entry names its predecessor's id, and an entry's id is the
  hash of its bytes.
* Kinds of entry:
  * perturbations (7.2), commits (15) and ACL entries (12);
  * **acks:** horizon, the head held of every other chain, and the newest confirmed check hash;
  * **votes**, R14 **attestations** (22.6), and membership changes.
* In production every entry is signed (Ed25519, 10.3). A signature over a chain head authenticates the
  whole prefix through the hash links. So a datagram needs only one signature, on its newest entry
  [I, wire cost 10.9].

**Validity is chain-local.** Every holder of an author's chain applies the same rules (8.2):
* an entry stamped at or before a horizon declared earlier in the chain is void;
* an ack whose horizon lags its stamp by more than the slack is void.

No validity rule depends on when or whether a message reached a particular member.

**Speculation is not blocked by holes.** A member applies an entry as soon as it arrives. The verdict on it
is fixed once the author's chain is contiguous up to it.

**Stability and confirmation.**
* A seq of a member's chain is **stable** once every member's newest reference to that chain covers it with
  the same id.
* The **confirmed frame** is the minimum over members of (the horizon declared in that member's stable
  prefix + 1).
* Only finality and trimming use the confirmed frame (6.4). Rollback below it is asserted impossible.
* [V session-sim: 25 scenarios, every one bit-identical to its offline replay, 0 false checks;
  V p2p-v2 over UDP: latency60, loss10, fault]

**Gap detection and fetch:** from acks' heads, and from holes in chains (9.3).

**Membership epochs and the view change (P2-4).**
* The member list changes only through a **view change**: one decision per epoch, taken by the epoch's
  members and recorded in an **epoch certificate**. Removals (a crash, a member too slow to keep up, a
  proven cheater), joins and resets all go through it. At most one side of a partition can decide one
  (review H3), within the fault bound below.
* **Votes start it** [V session-sim crashstall, twosilent]. A member votes to remove another when:
  * it holds a proof against it (below);
  * none of its entries has reached this member for 180 frames (3 s, 1.5 times the speculation limit),
    directly or fetched, while no other member's acks show more of its chain (R3-2). A shorter threshold
    would turn congestion into removals: in `c1` a congested member is silent for 1.95 s;
  * or its horizon has trailed the present by more than half the speculation limit for as long (R3-9:
    demotion; it can follow as a spectator).

  A vote names the voter's **frozen head** of that chain: the most of it the voter references for the rest
  of the epoch, so its later references to that chain never count toward stability (R3-1). The voter keeps
  processing the chain, though: a member wrongly suspected must still be heard in the reset that keeps it
  (found by `bridge`: freezing the processing too stalled the session after a partition healed). A vote
  stands until its epoch ends.
* **Proofs.** Two signed entries of one member that contradict each other prove it faulty. Honest members
  never produce one, and anyone holding the two entries can check it:
  * two versions of one seq;
  * an entry whose `prev` names a version of its predecessor other than the one the holder has. The
    other version need not ever be seen. This is how a member that forks its own vote is caught
    [V badvote, colluders];
  * a vote whose frozen head drops an entry that its author's own earlier ack referenced (a lying vote)
    [V liar].
* **Agreement** is Tendermint's algorithm for a single decision (Buchman, Kwon and Milosevic 2018,
  algorithm 1). Its messages are chain entries, so each is signed, ordered in its author's chain and
  repaired by fetch like any entry, and a member that sends two members different ones has equivocated.
  * The epoch's view change runs in **rounds**. Each round has a **proposer**, from a seeded rotation over
    the epoch's members, so no member chooses to propose.
  * The proposer proposes a **change**. Members **prevote** it if it is valid, and nil otherwise. A quorum
    of prevotes for it **locks** a member on it, and the member **precommits** it. A quorum of precommits
    **decides** it.
  * A proposer waits up to 6 frames for votes on every member it suspects itself, so members that fail
    together leave in one change [V twosilent].
  * A round that decides nothing times out: 20 frames, plus 10 per round. A quorum of nil precommits ends
    it at once, and a member that sees more than t members in a later round moves there.
  * The locks make two decisions in one epoch impossible, whatever the network does, within the bound
    below, which counts every faulty member, proven or not. The timeouts bring a decision once messages
    arrive in time (partial synchrony).
* **Quorums (D80, settled 22:13Z: admins decide kicks, this layer commits and orders every change).** Let m
  be the epoch's members minus those the change excludes by proof; the change lists them, with their
  proofs. A quorum is q = ceil((m + t + 1) / 2) of them with t = floor((m - 1) / 3): just over two thirds,
  the Byzantine quorum. A strict majority (t = 0) stays in the spike for comparison only.
  * **The bound counts every faulty member, proven or not** (review VC-1). Two changes of one epoch
    cannot both be decided while at most 2q(n) - n - 1 of the epoch's n members are faulty (the table's
    "(b) faulty members that cannot split it" column below, never fewer than t(n)). A proof does not stretch it: a change that excludes proven
    members needs a smaller quorum, so if only one side of a partition holds the proofs, each side can
    decide alone once the bound is exceeded [V onesided, a control: 3 of 6 collude; one side excludes
    them by proof and removes the third member, the other removes the first two with the colluders].
  * Within the bound, proofs buy liveness: a session whose cheaters are proven still decides
    [V badvote, colluders].
* **The change** names, for each member it removes:
  * the votes it relies on: at least q, none from an excluded member, none a lie;
  * the **cut**: the deepest point of that member's chain that at least (votes - t) of those votes' frozen
    heads extend.

  Why the cut keeps every confirmed frame: an entry stable at any honest member was referenced by every
  member before it voted (R3-1), so every honest voter's frozen head extends it, and at most t of the
  votes are faulty. The votes number more than 2t, so two branches of a forked chain cannot both reach
  (votes - t), and the cut is unique. With t = 0 it is the longest common prefix of the frozen heads,
  round 3's rule (9.3).
* **Every change names a boundary frame** B, signed with it: the first frame of the new epoch. Added members
  count from B; facts about frames from B on are the new epoch's to certify (P2-5's epoch-at-frame
  rule). Boundaries never go backwards. For a removal B is the proposer's present; for a join, or a
  change that closes the session (D80, below), it lies ahead.
* **Validity never changes with time** (review VC-3). A Tendermint lock relies on it: a member locked on a
  change must be able to decide it later. So a boundary's lead (more than one frame ahead of the
  present, at most two join leads) is checked only when a change is first proposed, not when it is
  proposed again in a later round [V jointiming: prevotes reach two of four members 1.5 s late, past
  the join's boundary. Before the fix the epoch wedged: no decision, confirmation stuck at frame 176 and
  every sim paused at the speculation limit. Now the join is decided in round 0 at 3.6 s].
* **Exclusions are checked before anything is decided.** A change's exclusions set its quorum, so a faulty
  minority that listed honest members as "proven" could otherwise decide alone. Members prevote, lock on
  and decide a change only once every exclusion it lists checks out, and a change may list only proofs
  that their two entries show alone (two versions of one seq, a successor naming another version, or an
  ack referencing a seq beyond a later vote's frozen head), so another world can check them too.
* **A change may remove no one: a reset.** A proposer proposes one when a vote of the epoch stands against
  a member it hears from (wrongly suspected, or back). It replaces round 3's withdrawal of a silence vote,
  which is unsafe once a quorum rather than every member decides: after a withdrawal, the voter's later
  references can make entries stable that the withdrawn vote's frozen head drops, and a change naming
  that vote would cut them.
  * A vote that names a kick (D80, below) freezes the target only for that kick. Members fetch a kick they
    do not hold from the voter and from everyone. A voter whose kick nobody has delivered within 60 frames
    (half the speculation window) froze a chain for nothing, and is voted out (review VCR-1; round 3
    answered such a vote with a reset after 180 frames, which kept the voter, review VC3R-2) [V ghostkick:
    9 votes against 2 for a kick that does not exist; 9 is voted out at 3.17 s. Before the VCR-1 fix
    confirmation stopped for good at frame 108].
  * A reset waits until the newest standing vote has stood 12 frames, and at most 36 from the oldest, and
    never overtakes a removal that a quorum has voted for and that waits only for entries this replica is
    still fetching (review VC3R-1, VC3R-2). Members that suspect the same member vote within a few frames
    of each other, and a reset in between would clear the first votes before the quorum arrives.
* **Cuts use only heads that resolve** (review VCR-2). A member's vote on its own kick must freeze exactly
  its previous entry: no reference of its own can check any other head. A proposer computes a cut only
  from votes whose frozen heads it can walk down to the prefix it holds, and asks every member, not
  only the voter, for an entry it lacks [V selfkickhead: admin 1 kicks 4, and 4's own vote names a
  head it never signed; 4 is removed at 2.28 s. Before the fix the kick never went through and
  confirmation stopped at frame 116].
* **Applying a decision.**
  * Epoch e + 1 starts. Removed members leave the list. Entries after each cut are void, and a removed
    member stops counting from the frame after the last horizon inside its cut.
  * A member may not yet hold a removed member's prefix up to the cut, or may hold another branch of a
    fork in it (review VC-2). Until it holds the cut's version, it confirms only the frames that the acks
    of the prefix verified against every member's references cover: an entry stamped at or before a
    declared horizon must come before that ack in its chain (8.2), and past that prefix its copy may be
    the other branch. It asks for the cut's branch by entry id, each request returning that entry and up
    to 15 before it down its prev links, so a member that holds the other branch answers nothing (review
    VC3R-7; round 3 asked by seq from the highest missing stretch down, VCR-5). It takes the branch even as
    duplicates of the entries it holds, and confirms nothing while it switches. The branch's entries are validated again before any of them acts, so one stamped at or
    before a horizon its author declared is void, never applied below the confirmed frame (review VCR-6)
    [V fork9: 9 forks; 1 and 2 hold the branch with its touch, 3 a longer one without. Before the fix 3
    confirmed past the touch and then had to roll back below its confirmed frame; now all three equal
    the replay. V deepfork: the cut lies over 16 entries past what 3 holds of the branch; before the
    fix 3 never got it all and stopped at frame 116. V cutbackdate: the cut's branch holds a touch
    backdated 90 frames; before the fix 3 applied it below its confirmed frame and hit the rollback
    assertion at 2.29 s].
  * A replica applies a decision only once it holds every admin entry the change lists; it asks those
    that precommitted it for the ones it lacks, by id (review VCR-8) [V slowadmin: an admin's entries
    reach follower 6 2.5 s late on every path; it decides at 3.60 s once it has the entry. Before the
    fix it panicked at 2.23 s].
  * Every vote and freeze of epoch e ends. No timer restarts (review VC3R-2): a member forcing a reset
    every few hundred ms would otherwise keep a slow member from its demotion for good [V lagchurn: 9's
    messages arrive 1.3 s late and it votes against 2 in every epoch; 9 is demoted at 6.48 s after two
    resets. With round 3's restart: never, after 8 resets]. The timers on acks from an epoch the reader
    has not reached end when it reaches that epoch (review VCR-4).
  * Instead, a member holds off a vote the decision answered, if it voted alone (review VC-6): the
    decision kept the member it voted against, no other member's vote against that member stood with its
    own, and the decided proposal reached it after its vote. It waits 24 frames before voting against
    that member again, twice as long each time in a row, up to 180. A vote that others share, or that the
    decided proposal never weighed (it reached the voter first), is cast again at once, so a quorum of
    honest votes still meets when their timers run apart, or while another member forces resets.
  * The member acks at once in epoch e + 1, so its acks never skip an epoch, however many decisions fall
    between its regular acks (review VCR-3).
* **Joins** use the same decision: a change that adds members, with a boundary B ahead of the present
  (the proposer's present plus 60 frames) [V join].
  * The joiner asks to join, and members accept a change that adds it only if they hold its request and
    the world's join rules admit it, with the starting role they give (12.4). The role travels in the
    change, because a member can decide a join from others' precommits without holding the request.
  * **So do the joiner's attestations** (review VCR-7). Every member judges the join rules from the
    relationships the change carries, signature-checked, never from its own copy of the request. A joiner
    that shows each member something different cannot split them [V joinattests: 5 shows its friendship
    with admin 1 to 1 and 2 only, and 9 prevotes but never precommits; 5 joins as Guest. Before the fix 1
    and 2 locked on the join while 3 refused it: no decision, confirmation stuck at frame 176].
  * **A join request is signed by its joiner, and its requests merge** (review VC3R-4): every valid
    attestation in any of them counts. Round 3 kept a joiner's first request, so any peer could pre-empt
    it with an empty one in the joiner's name [V joinsquat: 9 asks in 5's name at 1.9 s, and 5 asks at
    2.0 s with no attestation and at 2.5 s as admin 1's friend; 5 joins as Guest at 2.63 s. Before the
    fix: never admitted]. The signature names the session (its world and genesis members) and an expiry,
    so a request cannot be replayed into a later session of the same world, and a change is not proposed
    with a request whose expiry is before its boundary (review VC4R-6).
  * **The change carries the joiner's signed request** (review VC4R-2), and every member judges the join
    from it, never from a request it happens to hold. A joiner's request may reach only some members
    (an outside joiner choosing recipients, or loss); carrying it in the change means a member that
    never received it still validates the join, instead of being locked on a value it can never judge
    [V joinwedge: 5's request reaches 1, 2 and 9 only, and 9 crashes after its first prevote; 5 is
    admitted at 2.6 s and 9 removed at 5.6 s. Before the fix 1 and 2 locked on the join that 3 could
    never validate: no decision in 8 rounds, confirmation stuck at frame 114].
  * A member precommits a join with a **cap** while B is still ahead of it: its precommit names B, and
    from it on every reader counts that member's acks only below B (the smallest B it named, review
    VC-10) until it reaches the next epoch. A precommit whose B is not ahead of its author's declared
    horizon is void, a chain-local rule, so a cap never takes back what its author's acks confirmed
    (review VC-4). There is no upper bound: a far cap restrains only its own author (review VC3R-5), and
    round 2's, two join leads past that horizon, voided the precommit of a member that acks rarely
    (VCR-12) [V lagprecommit: 3 acks once every 1.5 s and the join needs its precommit; 5 joins at 2.95 s.
    Before the fix 3's precommit was void and nothing was decided].
  * Once some member's acks are capped at or below B, others may precommit late, without a cap: every
    reader then confirms nothing from B on before the decision, whoever holds the cap (review VC-3).
  * A cap is justified by the quorum of prevotes it followed, and a **capped precommit carries the ids
    of those prevotes** (review VC4R-1). A faulty proposer that sends its prevote for the value to the
    capper and a nil version to the others would otherwise make the capper's polka invisible to them,
    and they would vote the honest capper out. So before a member votes a capper out for an unexplained
    cap, it fetches the prevote ids the cap named and counts them in **any version it then holds**:
    either they reach the quorum (fetching the proposer's value-prevote completes the polka), or holding
    two versions of one prevote proves the proposer equivocated and removes it. Locks and decisions still
    count one version per member [V prevotefork: proposer 1 sends its prevote for the join to capper 2
    and nil to observer 3; with the fix off 3 votes 2 out, with it 3 fetches 1's value-prevote and keeps
    2].
  * A cap that no quorum of prevotes explains holds every reader's confirmation below B, as a silent
    member would. A member whose capped precommit has stood 60 frames here, for a value no quorum here
    prevoted, is voted out (review VC3R-2; round 3 cleared such a cap with a reset, which kept its
    author). An honest cap follows a quorum of prevotes, which acks and fetches bring every member within
    a few round trips [V bogusprecommit: a precommit with boundary 1 is void; one just ahead of its
    author's horizon gets 9 voted out at 3.17 s, with no reset and no pause. Before the VC-4 fix
    confirmation stopped for good, at frames 112 and 143. V capstale: 9 signs such a precommit in every
    epoch; no reset, voted out at 3.17 s].
  * **Nothing counts against a member whose vote a reset overruled** (review VC3R-2). Round 3 counted it:
    overruled in 3 of the last 8 epochs, a member was voted out. But suspicion by timing cannot tell the
    voter's fault from the suspect's: a member that controls when each other member hears from it can
    make one honest member's silence timer alone fire, then reappear, again and again [V strikeframe: 9
    runs honest code with hostile timing only; with round 3's rule 1 is voted out in two of five timings,
    now in none]. A member that keeps forcing resets costs latency, not safety, and an admin can kick it
    (Limits, below). The levers round 3's rule was meant to bound have their own rules: stale caps and
    ghost kick votes get their authors voted out (above), and no timer restarts at a decision [V capchurn:
    from 2 s, 9 signs a capped precommit for no proposal in every epoch and its acks claim the next one;
    no reset, 9 voted out at 3.17 s (round 3: three resets, then 2.50 s). Before the VCR-4 fix: 88 resets,
    and 9 never removed].
  * Acks carry their author's epoch. An ack from an epoch newer than the reader's never counts toward the
    reader's confirmation, so a member that decides late cannot confirm a frame from B on without the
    joiner either. Together: no member confirms a frame from B on before it knows of the join.
  * An ack's epoch never falls and rises by at most one per ack (chain-local; otherwise void), and a
    member whose acks claim an epoch the reader has not reached within 180 frames is voted out, like a
    silent one (review VC-5) [V futureack: acks claiming epoch 999 are void and 9 is removed at 5.23 s
    as too slow; acks claiming epoch 1 with no decision get it voted out at 5.17 s. Before the fix
    confirmation stopped for good at frame 112].
  * An honest member's acks never skip an epoch, because it acks on entering each (above), and a joiner
    that catches up on several decisions at once acks each (review VCR-3) [V ackgap: 3 acks once a
    second and two decisions fall between two of its acks; V latejoin: 5 catches up on two decisions at
    once, then joins at epoch 3. Before the fix their later acks were all void, and each was voted out as
    too slow].
  * A joiner's first acks may also declare the epoch it was admitted into at once, and its silence counts
    from its admission, so a joiner restored from a state transfer (9.5) need not have acked every epoch
    since genesis (review VC3R-6). Its chain is validated only once it is admitted, so every member
    judges those acks alike [V joinskip: 5 authors no ack before it is admitted at epoch 2, and stays.
    Before the fix its acks were void and it was voted out].
  * The 180-frame timer runs whatever other members' acks show. Round 3 paused it while the reader looked
    behind (VCR-12), but a climbing member that keeps one chain a step ahead somewhere kept that true for
    good (review VC3R-1) [V aheadshield: 9's acks claim epoch 1 while it sends each of its entries to one
    honest member only, in turn; voted out at 5.30 s. Before the fix: never, and confirmation stopped at
    frame 112]. A reader that really is behind may vote against a member that is ahead legitimately, and
    that vote is harmless: the member can be ahead only if the epoch has decided, an epoch decides once,
    and the vote ends with the epoch when the reader applies the decision [V aheadlink: 3 hears only 4's
    acks for 5 s while the others decide; its votes against the members ahead, in epochs 0 and 1, remove
    none of them, and 3 is removed for not keeping up].
  * Nothing the joiner signs acts before it is admitted. Once admitted, its entries stamped before B are
    void (a chain-local rule), and it counts toward confirmation from B on. It gets the state at a
    confirmed frame and the log after it (9.5); in the spike it follows the log from the start.
  * A removal needs no cap like a join's: no member can confirm a frame past a removed member's last
    horizon inside the cut before the decision, because every entry stable anywhere lies inside the cut.
* **Withheld references are suspected too** [V refwithhold]. Stability needs every member's references, so
  a member that keeps acking but stops referencing some chain stalls everyone's confirmation as surely
  as a silent one. A member whose acks have referenced no more of some chain for 180 frames, while every
  other member (the checking replica included) holds and references more of it, is voted out the same
  way. A member that only acks slowly still advances, and is not. A member that has voted against a
  chain's author no longer references it, so that pair is skipped, and a new member's references count
  from its admission (review VC-9).
  * **A reference counts only once the entry it names is held** (reviews VC4R-3, VC5R-2, and the
    randomized generator's RG-1): both toward a chain's stability and against a member as a withholder, a
    reference to seq *s* of chain *y* counts only if this replica holds *y* up to *s* in the version the
    reference names. For stability, the member's contribution is the newest of its references that
    resolves this way, not zero when its newest names a version I lack: so a faulty member alternating a
    fake head with a stale real one cannot cap the chain either, and the unresolved references are still
    fetched for the proof. A faulty member that references entries nobody has — a fake head, even one
    only a little above the real chain so it hides in normal lag — advances its raw reference but never
    its counted one, so it stalls no chain and is caught as a withholder like one that stops referencing
    [V slowref: a member that grows slower while another fakes references to its chain is removed at
    7.4 s; fakemix: a member alternating a fake head with a refreshed stale one raises the confirmation
    lag but halts nothing; the generator's seed 1: a member faking a reference to a healthy member's
    chain is voted out. Before the fix each stalled confirmation for good].
  * While another member references more of a silent or slow member's chain than this replica holds, it
    fetches the difference before suspecting, but only for a silence limit from when that member first
    fell behind (review VC4R-3): references that never deliver no longer spare it without end. The silent
    member is removed regardless, because its own references stop [V fakeref: 9 falls silent at 2 s while
    8 fakes references to 9's chain; 9 is removed at 5.2 s].
* **Epoch certificates (review R3-8b).** Each decision yields a certificate: the decided change (so the
  signatures cover exactly what is certified), the round, the new member list, a quorum's precommit
  signatures, and the two entries of every exclusion proof. Anyone holding the previous epoch's member
  list can check it, so another world can follow a world's membership from its genesis, one certificate
  at a time. P2-5's hops carry the source's certificates up to the epoch that signed them, so a world
  checks a hop from a world not on its route list too (P25-5).
* **What each rule tolerates**, counting all of the epoch's members, proven faulty or not:

| Members | (b) quorum | (b) removable at once | (b) faulty members that cannot split it | (a) quorum | (a) removable at once | (a) faulty members that cannot split it |
|---|---|---|---|---|---|---|
| 3 | 2 | 1 | 0 | 2 | 1 | 0 |
| 4 | 3 | 1 | 1 | 3 | 1 | 1 |
| 5 | 4 | 1 | 2 | 3 | 2 | 0 |
| 6 | 4 | 2 | 1 | 4 | 2 | 1 |
| 7 | 5 | 2 | 2 | 4 | 3 | 0 |
| 8 | 6 | 2 | 3 | 5 | 3 | 1 |
| 256 | 171 | 85 | 85 | 129 | 127 | 1 |

  "Removable at once" is m - q. "Cannot split it" is 2q - m - 1: the most faulty members two quorums can
  share while still sharing an honest one. It is the safety bound, counted over every member: proofs
  lower a change's quorum for liveness, never the bound (review VC-1). A two-member session removes no
  silent member (D7), but a member proven faulty is removed by the other alone (m = 1, q = 1), and a kick
  goes through because the kicked member freezes its own chain too (below).
* **Limits.**
  * Beyond the bound, a partition between honest members that a faulty member reaches on both sides lets
    each side decide its own epoch: the session forks [V bridge, under D80 (a)].
  * A faulty member can force a reset in every epoch by voting against a member everyone hears, or by
    staying silent just under every timer. Nothing removes it automatically, since timing alone cannot
    show whose fault an overruled vote is (above); confirmation lags by about one view change while it
    does, and an admin's kick ends it [V votechurn: from 2 s to 10 s, 9 votes against 2 in every epoch; 20
    resets, 2.5 a second; m1's confirmation lag, median and maximum, 7 and 8 frames before, 16 and 40
    during, 7 and 8 after admin 1 kicks 9 at 10 s, when the resets stop. V strikeframe: 9 silent about
    3 s in each of four windows causes one to four resets in 26 s, and is removed in three of five
    timings]. The user ruled that such a member is left to admins, with no automatic rule (V4 = (a),
    08:31Z).
  * A faulty proposer can decide a reset in a round it proposes, ending the epoch's votes; they are cast
    again at once, and the next honest proposer carries them [I].
  * A faulty proposer chooses which of a joiner's attestations its change carries, so it can give the
    joiner any role a subset of them allows; an honest proposer carries all it holds (review VC3R-4) [I].
  * A faulty proposer can still leave pending admin entries out of the changes it proposes in its own
    rounds; the next honest proposer lists them in commit order [I].
  * Every fetch by id — a cut's branch, a listed admin entry, a vote's kick or frozen head — is retried
    with the same backoff as a range fetch, so one lost reply no longer stops a cut walk or a decision
    for good (review VC4R-4). A removal that a quorum has voted for waits only for entries this replica is
    still fetching; a vote whose frozen head nobody delivers within half the speculation window counts
    toward no cut and holds back no reset, and its voter is removed as a ghost kick's voter is (review
    VC4R-5).
  * A removed member is not told yet: the others stop sending to it, so it pauses. It should get the
    certificate that removed it, to rejoin as a new member (a spectator can follow at once) [I].
  * **Not enforced yet, so relied on by nothing** (reviews VC-7, VC-8; @provenance-eng's follow-up):
    * epoch-at-frame for R14 hops: a hop's frame and epoch are not in the signed fact, so a third
      party's check of the epoch active at the hop's frame trusts the carrier's labels (22.7);
    * `cert_at` counts a world's majority over the members not excluded by proof, while a third party
      counts over the whole epoch, so a correctly built hop can be refused elsewhere.
  * A removal's boundary is the proposer's present, a little before the decision, so facts a member
    attests under epoch e about frames between the boundary and its decision no longer match once it is
    in e + 1. That costs griefing, not safety [I].
* **Admins and roles (D80; user rulings 22:11Z-22:51Z).** The view change commits membership; who may
  change it is session state as well, kept per epoch and changed only by decided changes.
  * **Roles.** Admin, role 0, is built in: it always exists, holds every permission, and by default is the
    only role and every player's. Admins define other roles as subsets of the permission catalogue and
    give them to members, live. Admins are equal: none ranks above another, and the session's creator has
    no special status. The spike's catalogue has the two permissions the protocol checks, kick and roles
    (define and give); the rest of the catalogue is 12.4's.
  * **Admin entries** are chain entries: a kick, a role given, a role defined. An admin's kick authorises
    the removal alone; no member votes on it. Every member, the kicked one included, only freezes the
    target's chain for the cut, so a kick goes through even in a session of two, where the others alone
    would be no quorum.
  * **Commit order.** A change lists the admin entries it commits in order of (stamp, author, seq), each
    judged against the state the ones before it leave, and marks each applied or refused, so a refusal is
    agreed. An entry no change has judged yet stays pending into later epochs. A kick by an admin removed
    earlier in that order is refused: of two admins kicking each other in the same frame, the first in
    commit order stays [V kickwar].
  * An admin entry's stamp decides its place, so it obeys its author's horizon like any entry: it is void
    if stamped at or before a horizon its author has declared (review VCR-9). A proposer lists pending
    entries in commit order and stops at the first it cannot carry out yet (a kick still waiting for
    frozen heads), instead of letting later ones overtake it [V kickbackdate: admins 1 and 2 kick each
    other at frame 120, 2's kick backdated 30 frames; 2's kick is void and 2 is removed. Before the fix
    the backdated kick came first and removed 1].
  * A validator accepts a listed admin entry only once it has validated its author's chain up to it, and
    refuses one that validation voided, so a faulty proposer cannot commit a backdated entry either
    (review VC3R-3) [V adminvoid: 9 signs a horizon-void SetRole demoting 3 and proposes a change that
    lists it; refused, and 3 stays Admin. Before the fix it applied]. The listed entry must also be this
    member's canonical version at its seq and not disputed, so a forked admin version held only as a
    duplicate, which a proposer never lists, cannot be committed at one replica and not another (review
    VC3R-3's low). A forked admin entry does not wedge a lock (review VC5R-1): a change is prevoted or
    locked only once `change_valid` returns true, which needs the listed entry held, validated and
    canonical here; and a member proposes only entries that are stable, so an honest round's proposal
    cannot later turn invalid at an honest member. A fork of the entry that arrives afterwards proves the
    author equivocated and excludes it, which only shrinks the quorum; it does not unmake a decision a
    quorum already reached. @design-reviewer could not reproduce a wedge (`adminfork` removes the faulty
    admin either way); this is the argument, not a run [I].
  * **Admin permissions, ruled** (review VCR-10; the user's V1-V3, 08:31Z; details in 12.4): kicking an
    admin is a permission of its own, held by default only by admins (V1); the roles permission counts
    as Admin, since it can grant Admin (V2 = (b)); and when the last admin times out the session closes,
    with no longer grace period and no automatic handover (V3). The spike still lets a role with the
    kick permission kick an admin; V1's separate permission and V3's closing behaviour are not built yet
    (tracked on P2-4) [I].
  * **Kicks wait out a partition** instead of forking: under the two-thirds quorum neither side of
    {1,2} | {3,4} commits anything, and after the heal the kick holds [V kickpartition].
  * **The last admin** cannot give the role up without handing it on: such an entry is refused [V lastadmin].
  * **Owners** (12.4): an owner may give up Admin, but only a co-owner demotes or kicks an owner (Q27,
    user 22:49Z and 22:51Z). An owner removed that way can rejoin as Admin through join row 0, as the user
    accepted.
    * Agreed with @provenance-eng for the ownership chain (12.5): ownership certificates entering a
      session are admin-layer entries, committed in order like role entries, so every replica judges each
      against the same owners without reading frame state. Owner checks compare members' identity keys
      (from their join certificates) with the chain's owners, and every time window in the chain's rules
      is measured against the deciding change's boundary, never a replica's clock [I]. The spike keeps a
      fixed owner set until @design-reviewer approves the ownership spike (its review found OWN-1 to
      OWN-3).
  * **No admin left: the session closes.** A change that leaves no admin (a kick, a removal on silence or
    proof, a release) closes the session at its boundary. The boundary lies ahead, as a join's does, with
    the same precommit caps, so nothing confirmed is undone, and nothing stamped at or after it acts. A
    last admin that crashes or times out therefore closes the session (user 22:13Z) [V adminclose].
* **Measured** [V session-sim, branch `viewchange`, D80 (b) unless named; logs in `spikes/out/session-sim/`]:

| Scenario | What happens | Result |
|---|---|---|
| `crashstall` | member 3 of 3 silent from 3 s | removed with round 3's cut (seq 86, from frame 173) and history; paused 72 and 74 frames (round 3: 72); all equal |
| `twosilent` | members 5 and 6 of 6 silent from 3 s | both removed in one change at 6.15 s (round 0); paused 74 frames; all equal |
| `twosilent5` | members 4 and 5 of 5 silent from 3 s | (a): both removed in one change at 6.15 s. (b): no decision, the three left pause safely (confirmed frame 169) |
| `withheld` | 9 equivocates on a confirmed touch; 8 casts no view-change entry | 9 removed at 3.73 s, about 160 ms after its second signature, without 8; the confirmed touch stands; no pause |
| `colluders` | two of five: 9 equivocates, 8 forks its vote | both proven and removed in one change at 3.73 s; no pause |
| `badvote` | two of four: 9 equivocates, 8 forks its vote | **recovers** (round 3 stalled): both removed; confirmation resumes, no pause |
| `liar` | 8's vote contradicts its own ack | 8 proven faulty and removed with 9 |
| `badproposer` | the first proposer shows one member a reset and the others a removal; 5 silent | that round fails; round 1 removes the proposer (proven) and 5 together at 6.58 s |
| `bridge` | partition {1,2} / {3,4} from 3 to 12 s; faulty 8 reaches both sides and backs everything | (b): nothing is decided during the partition (529 paused frames); after the heal one session, and a reset at 13.67 s keeps all five, since 8 behaves again and is proven of nothing (round 1 removed it as a withholder; its references stopped only behind its own votes, which review VC-9 no longer counts). (a): {1,2} removes 3 and 4 at 6.18 s, {3,4} removes 1 and 2 at 7.80 s, and each side then removes 8: **two sessions** |
| `join` | member 4 asks to join at 2 s, touches an object at frame 420 | admitted at 2.10 s with boundary frame 175; its touch applies at every member; all four equal |
| `refwithhold` | 8 keeps acking but stops referencing the others' chains from 2 s | 8 removed at 5.23 s; paused 78 frames; all equal |
| `fork9` | 9 forks its chain: 1 and 2 get a branch with its touch at frame 120, 3 a longer one without it; 3's votes 300 ms late | 9 removed at 2.32 s with the touch's branch as the cut; 3 adopts it; all equal (before the VC-2 fix 3 confirmed past the touch, then hit the rollback assertion) |
| `jointiming` | 5 joins; prevotes reach 3 and 4 1.5 s late, past the boundary | decided in round 0 at 3.6 s, all five equal (before the VC-3 fix: wedged, confirmation stuck at frame 176) |
| `bogusprecommit` | 9 signs a precommit with boundary 1, or just ahead of its horizon | the first is void; for the second, 9 is voted out at 3.17 s with no reset and no pause; confirmation goes on (before the VC-4 fix: stuck at frames 112 and 143) |
| `futureack` | 9's acks claim epoch 999, or epoch 1 with no decision | 9 removed at 5.23 s, and at 5.17 s; confirmation resumes (before the VC-5 fix: stuck at frame 112) |
| `kickwar` | admins 1 and 2 kick each other at frame 120 | 1's kick applies first in commit order: 2 removed at 2.30 s, 2's kick refused; all equal |
| `kickpartition` | partition {1,2} / {3,4} from 2 to 6 s; admin 1 kicks 3 at 3 s | nothing decided during it; 3 removed at 6.97 s after the heal; one session |
| `lastadmin` | the only admin releases the role; later hands it to 2, then releases | release refused (2.17 s); hand-over and release applied (4.17 s, 4.23 s); admins {2} |
| `adminclose` | the only admin silent from 3 s | removed at 6.5 s; the session closes at frame 438 on both remaining members; all equal |
| `joinrules` | rules: friends of an admin join as Guest, confidants of an owner as Admin; four ask | the friend joins as Guest, the confidant and the owner (row 0) as Admin, the stranger is refused |
| `onesided` (control) | 3 of 6 collude, above the bound; partition {1,2} / {3}; proofs only for 1 and 2 | two different epoch-1 decisions: the session forks, as the restated bound allows |
| `ghostkick` | 9 votes against 2 at 2 s for a kick that does not exist | nobody delivers the kick, and 9 is voted out at 3.17 s (round 3: a reset at 5.13 s kept it; before the VCR-1 fix: stuck at frame 108) |
| `selfkickhead` | admin 1 kicks 4 at 2 s; 4's vote on its own kick names a head it never signed | 4 removed at 2.28 s (before the VCR-2 fix: never; confirmation stuck at frame 116) |
| `ackgap` | 3 acks once a second; two decisions 0.25 s apart fall between two of its acks | both decided, 3 stays (before the VCR-3 fix: its later acks void, and 3 voted out as too slow) |
| `latejoin` | 5 catches up on two decisions at once, then joins at epoch 3 | admitted at 3.10 s and stays (before the VCR-3 fix: voted out after joining) |
| `capchurn` | from 2 s, 9 signs a capped precommit for no proposal in every epoch; its acks claim the next | no reset; 9 voted out at 3.17 s (round 3: three resets, then 2.50 s; before the VCR-4 fix: 88 resets, 9 never removed) |
| `deepfork` | `fork9`, with 3's entries reaching 1 and 2 1.5 s late, so the cut lies over 16 entries past what 3 holds | 9 removed at 3.63 s; 3 fetches the branch by id, 16 entries per request; all equal (before the VCR-5 fix: 3 stuck at frame 116) |
| `cutbackdate` | `fork9` with a touch backdated 90 frames on the cut's branch | the touch is void everywhere; all equal (before the VCR-6 fix: 3 hit the rollback assertion at 2.29 s) |
| `joinattests` | 5 shows its friendship with admin 1 to 1 and 2 only; 9 prevotes but never precommits | 5 joins as Guest; 9 is removed later for not referencing 5's chain (before the VCR-7 fix: wedged, confirmation stuck at frame 176) |
| `slowadmin` | an admin's entries reach follower 6 2.5 s late on every path | 6 decides at 3.60 s once it has fetched the entry (before the VCR-8 fix: it panicked at 2.23 s) |
| `kickbackdate` | `kickwar`, with 2's kick backdated 30 frames | 2's kick is void; 2 removed at 2.27 s (before the VCR-9 fix: the backdated kick removed 1) |
| `lagprecommit` | 3 acks every 1.5 s; 9 casts no view-change vote; 5 asks to join at 2.8 s | 5 admitted at 2.95 s (before the VCR-12 fix: 3's precommit void, no decision, confirmation stuck at frame 224) |
| `aheadlink` | 3 hears only 4's acks for 5 s while the others decide an epoch | 3 removed at 5.23 s for not keeping up; its votes against the members ahead, in epochs 0 and 1, remove none of them |
| `strikeframe` | 9 runs honest code; in each of four windows its last entries reach 2 and 3 late, then it is silent about 3 s; five timings | 1 kept in all five; 9 removed in three (round 3's strikes voted 1 out in two) |
| `aheadshield` | from 2 s 9's acks claim epoch 1, and 9 sends each of its entries to one honest member only, in turn | voted out at 5.30 s (climbing alone: 5.17 s; rotating alone is harmless, conf 697-701 of 713); before the VC3R-1 fix: never, stuck at frame 112 |
| `fakeref` | 9 silent from 2 s; 8's acks claim 5 more of 9's entries than anyone holds | the withholding rule removes 9 at 5.25 s |
| `capstale` | from 2 s, 9 signs a capped precommit for no proposal in every epoch | no reset; voted out at 3.17 s (round 3: three resets, then 2.50 s) |
| `lagchurn` | from 2 s, 9's messages arrive 1.3 s late and 9 votes against 2 in every epoch | two resets; 9 demoted at 6.48 s (with round 3's timer restarts: never, after 8 resets) |
| `votechurn` | 2-10 s: 9 votes against 2 in every epoch; admin 1 kicks 9 at 10 s | 20 resets, 2.5 a second; confirmation lag median/max 7/8 frames before, 16/40 during, 7/8 after; the kick ends it |
| `adminvoid` | 9 proposes a change listing its own horizon-void SetRole demoting 3 | refused; 3 stays Admin (before the VC3R-3 fix: applied) |
| `joinsquat` | 9 asks to join in 5's name before 5 does | 9's request refused; 5's two requests merge; 5 joins as Guest at 2.63 s (before the VC3R-4 fix: never) |
| `joinskip` | 5 authors no ack before it is admitted at epoch 2 | 5 stays (before the VC3R-6 fix: its acks void, voted out) |
| `joinwedge` | 5's signed request reaches 1, 2 and 9 only; 9 crashes after its first prevote | 5 admitted at 2.6 s, 9 removed at 5.6 s (before the VC4R-2 fix: 1 and 2 locked on a join 3 can't validate, stuck at frame 114) |
| `slowref` | a member grows slower while another fakes references to its chain | the slow member removed at 7.4 s (before the VC4R-3 fix: never, stuck at frame 108) |
| `vcscale` | 64 members with ack heads, and 256 log-only members without; the last 2 silent from 1 s | every live member (62 and 254) decides the removal 2.77 s after the silence began, in one change. With heads confirmation resumes; without them nothing confirms, so the 256 run times the decision alone |

  Every scenario's honest members equal each other and the offline replay of the final log, with no false
  check failure; no confirmed frame was rolled back (that is asserted). The rest of the suite is
  unchanged except where the view change acts: `backdate` removes its lagging member sooner (66 paused
  frames instead of 128, cut at seq 87 instead of 119), `r14 crash` pauses 5 or 6 frames longer for its
  crashed carrier, and the R14 chain digests changed because hops now carry their commit frame and epoch
  certificates (every outcome is the same) [V 5k]. Round 3 changes no outcome elsewhere: decision times
  and confirmed frames move by a frame or two, because members now ack on entering an epoch [V 5k].
  Round 4 changes none either, beyond the rows above: `kickpartition` decides 2 frames sooner, and
  `bridge` under (b) ends with two resets after the heal instead of one, still one session [V 5k].
  Rounds 5 and 6 change no removal outcome by design, but they move more logs than the earlier rounds:
  the extra reference fetches and the larger capped-precommit shift view-change timing by a frame, so
  some scenarios' cuts land one entry later and their confirmed digests move with them (`evict`,
  `deepfork`), and `fakeref` and `strikeframe` now also remove the faulty member whose fake references
  or hostile timing the new rules catch. Every member still equals the replay with no false check, and
  roles and closures are unchanged [V 5k].

**Per region (R16) [I].**
* Each region log (25.5) is a session log in this sense, with its own chains, stability, confirmed frame
  and epochs. Its members are the peers that simulate that region.
* Logs interact only through proof-carrying references to facts the other log has already confirmed, as in
  R14 (22.6). There is no shared clock, and no log reads another's unconfirmed state.

**What the old 2 s timers became:**
* SCM finality and ACL ordering (12.2, 15.2) wait for confirmation: about 140 ms (8.5 frames) on 50 ms
  links [V session-sim r14];
* the rollback window became the speculation limit (6.4).

### 10.9 Recovery when the network degrades (R18)

User requirement 18 (14:29Z): when the network degrades, the engine must detect it and recover. It must
not stay collapsed, and it must not visibly replay old input. In @ballpit-viz's matrix, the 64 kbit/s cell
showed the failure under 3d0f98a: bufferbloat, re-stamping, both peers stalled at the speculation limit,
and the engager's hand replayed after the gesture had ended.

**What changed:**
1. **No re-stamping** (8.2). A late entry lands at its own frame. Nobody's past is rewritten and no hand
   replays: a stall can delay what a receiver sees, but never re-orders the author's input.
2. **One datagram per tick per peer** (viz-4).
   * Everything a member sends a peer in one tick goes in one batch, and acks are sent on frame ticks so
     they ride with the frame's entries.
   * The 500-ball pit's uplink fell from 77 to 65 kbit/s, with the hand author sending every frame
     [V r18 capped].
3. **Congestion control** on each member's uplink.
   * **Signal:** this member's own queue delay. The spike reads the bottleneck queue directly. Production
     estimates it from the one-way delay gradient of ack timestamps, as WebRTC's GCC does [I].
   * **Response:** six levels, raised quickly (above 40 ms, at most once per 100 ms) and lowered slowly
     (after 300 ms below 10 ms).
     * Each level thins the hand stream to one entry every 1/2/3/4/6/8 frames, and acks to one every
       2/2/3/4/6/8 frames. Both fall on the same frame ticks, so a hand entry and an ack share one
       datagram (review R3-9 added the two coarsest levels and the alignment).
     * A thinned hand entry carries a `span`: its target is reached over that many frames, and dead
       reckoning (6.10) covers the gaps.
     * The author applies the same thinned entries, so under congestion its own physics hand trails its
       controller by up to k frames (review R3-10). Packing k per-frame targets into one entry would keep
       the author exact: **D73**.
     * Every peer, the author included, applies the same entries, so the author's own physics stays exact
       and is never corrected.
   * Adaptation happens when entries are authored, never by dropping queued entries: chain entries cannot
     be dropped.
4. **Bounded queues:** the controller keeps the uplink queue near its 40 ms trigger. The router queue
   (500 ms in the matrix) is only reached under a cap the stream cannot fit even at the coarsest level.
5. **Repairs yield to the queue** (review R3-9). Below about 20 kbit/s, repairs were what collapsed the
   session: each drop led to fetches, re-asked every 60 ms, which is shorter than a congested round trip,
   and answered with up to 65 entries in one unsplit datagram, which the full queue then dropped whole.
   Now:
   * a fetch for the same range is re-asked with exponential backoff, from 60 ms up to 2 s;
   * a reply holds at most 16 entries, and the requester asks again for the rest;
   * a member answers a fetch only while its uplink to the requester holds under 100 ms;
   * every batch is split into datagrams of at most 1,200 bytes.
6. **Leaving a stall.**
   * A member that fell behind re-simulates in the background, at up to 8 ms per tick (6.5), and draws
     only its present. The catch-up is never shown as a replay.
   * Render-only smoothing (6.10) blends the jump.
   * Its own new input is stamped at its present, never in the past.
7. **Demotion** (review R3-9) [V capped, slow member; backdate part 2]:
   * A member whose newest validated horizon trails a replica's present by more than half the
     speculation limit for 3 s is voted out exactly like a silent member (10.8). The 3 s keep a short
     congestion from qualifying: in c1 a member's traffic is 2 s late, and it stays.
   * It then stops counting for confirmation, and its entries after the cut are void.
   * A two-member world cannot demote anyone (D7).
   * Still [I]: the demoted member continuing as a **spectator** (it keeps receiving the log and
     rendering, and its new input is refused), and rejoining as a member with a state transfer at a
     confirmed frame (9.5) once its link holds.
   * Demotion is the only case in which an author's input is undone: its entries after the cut, which no
     other member holds. Others' histories are never rewritten.
8. **Repair:** hash checks at confirmed frames and state transfer (9.3, 9.5). These cover true divergence.
   None was observed.

**Measured** (500-ball pit; member 1 sweeps; 50 ± 5 ms one-way; token-bucket cap each way, 4 kB burst,
500 ms router queue, tail drop; dead reckoning on) [V session-sim `capped`, spikes/out/r18, round 3]:

| Cap | Transport | Uplink m1 | Max queue, drops | Stalled frames | Max confirmation lag | Back to real time after the gesture | End state |
|---|---|---|---|---|---|---|---|
| 256 kbit/s | any | 65-78 kbit/s | 0 ms, 0 | 0 / 0 | 9-10 frames | at once | all equal |
| 64 kbit/s | per message (3d0f98a transport) | 77 kbit/s | 274 ms, 0 | 0 / 0 | 26 frames | 515 ms | equal |
| 64 kbit/s | batched, or batched + adaptive | 65 kbit/s | 0 ms, 0 | 0 / 0 | 9 frames | at once | equal |
| 48 kbit/s | per message | 76 kbit/s | 499 ms, 52 | 238 / 262 | 328 frames | never in the run | not converged |
| 48 kbit/s | batched | 65 kbit/s | 498 ms, 6 | 22 / 22 | 139 frames | 2.4 s | equal |
| 48 kbit/s | **batched + adaptive** | 57 kbit/s | **49 ms**, 0 | **0 / 0** | 13 frames | **at once** | equal |
| 32 kbit/s | **batched + adaptive** | 41 kbit/s | **105 ms**, 0 | **0 / 0** | 15 frames | **at once** | equal |
| 16 kbit/s | **batched + adaptive** (30 s run) | 17 kbit/s | **364 ms**, 0 | **0 / 0** | 45 frames | **at once** | equal |
| 12 kbit/s | batched + adaptive (30 s run) | 14 kbit/s | 484 ms, 11 | 1,201 / 1,214 | 1,332 frames | 19.3 s | equal |
| 8 kbit/s | batched + adaptive (30 s run) | 12 kbit/s | 498 ms, 35 | 1,586 / 1,588 | 1,706 frames | never in the run | not converged |
| 64, then 16 from t = 3 s | **batched + adaptive** (30 s run) | 36 kbit/s | **343 ms**, 0 | **0 / 0** | 33 frames | **60 ms** | equal |
| 64 per uplink, shared by 3 peers (4 members) | **batched + adaptive** (30 s run) | 68 kbit/s | **215 ms**, 0 | **0** (all four) | 33 frames | **at once** | all four equal |
| 256, but member 3 of 3 on 8 kbit/s | batched + adaptive (20 s run) | 30 kbit/s | 498 ms on member 3's links | 135 / 135 before the demotion | 255 frames | at once | member 3 demoted after 3 s of lag; members 1 and 2 equal |

* **Round 3 (review R3-9)** moved the collapse point. Before, every cell below about 20 kbit/s, and four
  members on a shared 64 kbit/s uplink (6,969 drops), collapsed for good. Now 16 kbit/s, a step from 64
  to 16, and the shared uplink stay real time. 12 kbit/s recovers within 20 s of the gesture's end, and a
  member too slow for the session is demoted.
* **The floor for two members is about 12 kbit/s each way** at 60 Hz. Per-datagram overhead dominates:
  even at the coarsest level, acks alone need about 7.4 kbit/s (7.5 datagrams/s × 124 B) [I, from the
  wire format]. Below the floor a two-member session stalls, because it cannot demote either member
  (D7). With three or more members, the member on the bad link is demoted.

* In every run the author never rolled back and none of its entries moved. The engager's view was never
  corrected.
* Without adaptation the stream collapses once the cap is below what it needs: repairs add load, queues
  fill, members stall at the speculation limit, and they do not recover while the gesture lasts.
* With adaptation the same caps keep queues under 100 ms and the session in real time, at the cost of a
  coarser remote hand (the observer's error grows with the stride, 6.10).
* **Wire cost of signatures [I]:** with one Ed25519 signature per datagram, on its newest entry, add 64 B
  per datagram. That is about 3.8 kB/s per peer at 60 datagrams/s, unless signatures ride only on acks
  (about 1.9 kB/s).
* **What the matrix does not cover** (review R3-14): two hands at once (each author then rolls back,
  124 times at 32 kbit/s in the reviewer's probe), and a production queue estimate in place of the
  spike's direct reading of the bottleneck queue.
* Decisions: **D53** (recovery policy), **D56** (controller defaults).


---

## 11. Peer discovery (R9)

Requirement 1 says no central authority. Below, a "soft central point" means infrastructure we would have
to run or depend on for discovery, even if it has no authority over sessions.

| Option | Decentralized? | Works across the internet? | Notes |
|---|---|---|---|
| Invite tickets (copy/paste, QR, chat) | Fully | Yes if either side is reachable, or via a session member (10.5) | Zero infrastructure. Ticket = session ID, member addresses, inviter key, join secret |
| mDNS / LAN broadcast | Fully | LAN only | `mdns-sd` pure Rust [V audit] |
| Gossip / peer exchange from known peers | Fully | Yes, once you know one peer | Friends list = cached keys + last addresses. Members gossip each other's addresses |
| DHT (Kademlia) of our own | Mostly | Yes | Needs bootstrap nodes to enter: a soft central point. Our own network is small at launch, so weak |
| **Mainline DHT (BitTorrent, BEP 5 / BEP 44)** | Yes; huge, operator-less public DHT | Yes | `mainline` pure Rust [V audit]. Publish signed mutable records keyed by user or session public key, holding current addresses. Entry uses well-known bootstrap hosts plus cached nodes. Public, so records are visible (encrypt contents) |
| Bootstrap or rendezvous servers of our own | No (soft central) | Yes | Simple and reliable, but against the spirit of R1 |
| Public infrastructure (Nostr relays, BitTorrent trackers) | Many independent operators | Yes | Workable fallback; adds protocol surface |

**Recommendation (D9): layered.**
1. Invite tickets as the primary flow.
2. mDNS on LAN.
3. Peer exchange inside sessions and among friends.
4. Mainline DHT records ("user K is at addresses…", "session S members…") to re-find friends and
   sessions with no infrastructure of ours.
5. No servers of our own. An optional, community-run relay or bootstrap list stays a user decision.
6. **Human-readable session codes (10.7)** use the Mainline DHT only as a rendezvous [I]:
   * a public two-word nameplate locates a member who announced itself (BEP 5);
   * a secret two-word password is used only inside a PAKE with that member, and is never published;
   * the member then hands over a normal invite ticket (item 1).

   Knowing the nameplate gives an attacker only online guesses: one per PAKE run, and at most 10 per code.
   With a friend in the session, the code shrinks to the two password words plus a check word, and nothing
   is published. Decision: **D35a**.

NAT ties in through 10.5: discovery yields candidate addresses, and members broker hole punching.

---

## 12. Authority model (R1, R7, R8)

### 12.1 Two kinds of authority

* **The simulation has no authority.** It is a deterministic function of signed inputs, and every peer
  computes it (R1).
* **"Authority" in R7 is permission to edit:** who may commit logic, shaders and assets, who may interact,
  and who may administer. It lives in a signed **ACL log** inside the world repository. [I]

### 12.2 ACL log [I]

* **Genesis:** the world creator's key signs the initial ACL. Roles: `owner`, `builder` (logic, shader and
  asset commits), `editor` (in-world object edits), `player` (interactions), `spectator`.
* **Grants, revocations and owner changes** are signed entries ordered exactly like commits (soonest wins,
  15.2). Each takes effect at its ordered frame once that frame is confirmed (6.4), so no two peers ever
  hold different ACL states for a confirmed frame. During a partition only the majority side can change
  the ACL (10.8; review H3).
* **Every peer validates** every interaction and commit against the ACL state *as of that frame*. A
  non-authorized action is rejected identically everywhere, with a machine-readable reason.
* **Optional threshold ownership:** k-of-n owners must sign ACL changes. This removes the single-root-key
  concern (D10).

### 12.3 AI agents [I]

* **Keys and certificates.** Each agent session has its own Ed25519 key. A user issues a **delegation
  certificate**: "user U authorizes agent key A with scopes {interact, edit-objects, commit-logic,
  commit-shaders, commit-assets}, expiring at T".
* **Issuing a certificate is a human action, out of band.**
  * Certificates are created only in the engine-owned approval layer (21.2), after the user confirms the
    agent, its scopes and its expiry with local input.
  * No MCP tool issues, widens or renews a certificate.
  * Agents cannot open the dialog either: the user starts the flow from the panel UI.
* **Agents never get admin scope.** These actions are human-only, and no certificate can carry them:
  `acl.grant`, `acl.revoke`, owner changes, threshold settings (D10), delegation, and loading a backup
  (16.10).
* **Sub-certificates for restricted sessions (21.3).**
  * A panel key may issue sub-certificates to the restricted sessions it runs (inbox and decider
    sessions, 21.6).
    Each is narrower in scope, expires sooner, and names the principal it serves.
  * The chain is at most user → panel → session.
  * The engine issues them only when the owner unlocks the panel or accepts a connection, never at a
    model's request.
  * Every peer verifies the chain, and the scope must narrow at each step.
* **The user key is held outside the MCP-reachable path.**
  * It is stored encrypted at rest: DPAPI user scope on Windows, the OS keyring on Linux.
  * Only the signing component behind the approval layer loads it, and that component signs only requests
    tied to a confirmation the user just made.
  * The MCP server and the harness adapters have no code path to it. The crates that handle MCP requests
    do not depend on the user-key signer.
  * Panel and session keys are separate. They sign only actions inside their certificate's scope.
* **Effective rights** = certificate scopes ∩ the user's roles, intersected with any sub-certificate's
  scope.
* **Attribution.** Certificates travel in the session log. Every peer can verify agent-signed actions and
  attribute them to user U, and to the remote party that a sub-certificate names. An action taken during
  a guest's turn at an unlocked panel (21.5) is signed by the panel's session key with the owner's
  authority, and records the guest as `requested_by`, from the engine's turn binding, never from text.
* **Revocation.** Revoking a certificate (`CertRevoked {key, effective_frame}`) stops the agent at a
  deterministic frame. The frame is a confirmed frame (6.4), so every peer stops the agent at the same
  point. Guests hold no keys of their own, so re-locking a panel needs no log entry (21.5).

### 12.4 Session admins, roles and join rules (D80; user rulings 22:11Z-22:51Z)

The world's ACL (12.2) says who may edit the world. A **session** has admins and roles of its own, which
decide who may kick, who may give roles and who may join. They are session state, changed only by signed
entries that the view change commits (10.8).

* **Admins jointly hold the session.** The role is called Admin everywhere, in the UI and in the docs
  (22:18Z): the engine has no server, and no name should suggest one.
  * Every player is an admin by default; a session can be configured so joiners get another role
    (22:11Z). Several admins are normal, and they are equal: no creator or owner super-admin (22:18Z).
  * An admin may kick anyone by one signed entry; the view change only commits it, in order (D80).
    Kicking an admin is a permission of its own: by default only admins hold it, and a session may give
    it to other roles (V1, 08:31Z).
  * There is always at least one admin: the last cannot give the role up without handing it on (22:11Z).
  * When no admin is left, by leaving, a kick, a crash or a timeout, the session closes (22:13Z). Closing
    removes every other player, tells everyone that the session has closed, and allows no further change
    to the world. There is no longer grace period and no automatic handover (V3, 08:31Z).
    * Since the roles permission counts as Admin (V2), a member whose role holds it counts as an admin
      here, so the session does not close while such a member remains (@coordinator-opus's call,
      following V2, 09:24Z).
* **Roles.** Admin is built in, holds every permission and is by default the only role. Other roles are
  named subsets of a permission catalogue that admins create and give, live (22:15Z). Resonite's role
  system inspired the catalogue and live reassignment (22:14Z). Ours differs because no single peer
  enforces it: every replica checks every entry against its epoch's roles, and role changes are ordered
  with every other membership change by the view change.
  * The catalogue [proposal]: kick; kick admins (V1: admins only by default); define and give roles,
    which counts as Admin because it can grant Admin (V2 = (b), 08:31Z); change the join rules; interact (touch, grab,
    hands); edit objects (16.9); commit logic, shaders and assets (12.2's builder rights); speak; spawn;
    save checkpoints, load backups and copy world (16.10); use agent panels. The spike checks the first two (10.8).
  * How 12.2's repository roles (`owner`, `builder`, `editor`, `player`, `spectator`) relate to session
    roles is open [proposal: the world's ACL caps what any session role can grant; world owners are
    the ownership chain's, below].
* **World owners** are not session admins: ownership is a persistent property of the world, shareable by
  several people, held as a chain of signed ownership certificates (@provenance-eng's draft, to fold in
  as a 12.x; its questions O1-O5 are with the user).
  * An owner always joins as Admin: **row 0** of every join rule list, fixed and not removable (22:48Z).
  * An owner may give up Admin; only a co-owner demotes or kicks an owner (Q27, 22:49Z and 22:51Z). An
    owner demoted or kicked that way can rejoin as Admin through row 0.
* **Join rules** (22:37Z; a matrix since 22:47Z, extended at 22:51Z): after row 0, an ordered list of
  rows "if the joiner [relation] [target], then [action]", then a fallback.
  * Relations: is; is friends with; is a confidant of; has an invite ticket from; is a named player.
  * Targets: a current player; a current admin; a current player with a given role; a world owner. A row
    matches if the relation holds with any of its targets, or with all of them.
  * Actions: refuse; admit as Admin; admit with a given role.
  * Presets: Public (everyone, as Admin by default); Invite only (an invite ticket from a current
    player); Friends only (a friend of a current admin, 22:17Z); Friends+ (a friend of any current
    player); a home world (an owner's confidants as Admin, an owner's friends as Guest, 22:36Z).
  * A relationship is a signed attestation the joiner presents: a friendship by both players, a confidant
    mark by the player who made it (D27a), an invite ticket by its issuer, for this world.
  * Rules are evaluated in the view change against the membership the change leaves. A join stands once
    committed, even if the friend who vouched leaves, and "all of" means all at that commit (22:17Z). The
    starting role travels in the change [V joinrules].
  * Discovery advertises a session's access level, so a stranger is not offered a session it cannot join
    (D9, D35) [I].
  * Not in the user's list, so not part of the model: a per-player role outside a row, and "blocked by"
    rows [proposal].

---

## 13. Live game logic in Rust (R7, 100%-Rust clarification)

### 13.1 Pipeline

```
AI or human edits Rust source (MCP: logic.write / logic.check / logic.test_in_fork)
   -> pinned toolchain compiles one module crate to wasm32-unknown-unknown (rel-fast profile, 13.7)
   -> commit {source tree hash, canonical wasm hash (13.2), schema manifest} signed by the author
   -> ordered and confirmed (15.2) -> activation frame A = confirmation + lead
   -> every peer validates the wasm and swaps it in at frame A, with state migration
```

### 13.2 Who compiles: every peer (D11, user ruling 18:53Z)

**Settled: every peer compiles every commit from source** (user, 18:53Z; the recommendation had been that
only the committer compile). Peers run only modules they built themselves, so a module can never differ
from its source.

* **A module's identity is its canonical hash (review M8):** SHA-256 over the module with all custom
  sections removed (`name`, `producers`, `target_features`). A commit carries its source tree hash and the
  author's canonical module hash; every peer builds it and compares.
* **Byte-identical builds on every machine and OS are now a hard requirement,** not a background check.
  What makes a build reproducible:
  * the toolchain is pinned exactly (rustc version, the wasm32 target's std, and codegen flags such as
    `codegen-units = 1` and the opt level), and invoked by absolute path (15.6). The repository pins it
    in `rust-toolchain.toml` (`channel = "1.99.0"`). `stable` drifted: by 2026-10-30 WSL resolved it
    to 1.99.1 while Windows had 1.99.0 (P2-7);
  * panic locations embed source paths, so builds remap the workspace root and the cargo home
    (`--remap-path-prefix`);
  * **host separators (P2-7)** [V]: cargo hands rustc a workspace member's sources as paths *relative to
    the workspace*, which a root remap does not touch, and rustc joins module paths with the host's
    separator. So a panic location reads `logic_mono\src\api\math.rs` from Windows and
    `logic_mono/src/api/math.rs` from Linux, and the modules differed. On Windows the build adds one
    remap per source directory (`dir\sub\` to `dir/sub/`, deepest last, since rustc applies the last
    match; `sepmap` in scripts/build_wasm_guests.sh);
  * cargo hashes a path dependency's crate identity by its absolute path unless the dependency lies
    inside the workspace being built, and the linker's function order follows those hashes. So every
    dependency lives inside the world repository's workspace (vendored, review L8);
  * **the name section carries host-dependent crate hashes (P2-7)** [V]: for a workspace member nested
    below the root (`spikes/sim-core`, `crates/box3d-fork`), cargo's crate hash differs between
    Windows and Linux, and the mangled function names in the `name` custom section carry it. With the
    separator remaps, physics_in_wasm differed in that section and nowhere else (canonical hash equal).
    Committed modules are built with `strip = "symbols"`, so they have no name section. Members one
    level deep (the iter-bench crates) were equal anyway. Traps then report function indices, not
    names; a peer may keep a separate, unstripped local build for debugging;
  * no build scripts or proc macros in logic crates, except the engine SDK's own (below).
  * **Shown across machines and OSes (P2-6, P2-7)** [V]: the four spike modules plus P2-7's larger
    crates (logic_mono, the 6.4k-line benchmark as one crate, and logic_wasm, the same code as nine
    path-linked crates) are raw-hash identical:
    * when built on this PC's Windows, natively in WSL Ubuntu 24.04 (Linux x86_64), and on the Intel
      laptop (Windows);
    * at two checkout paths on each;
    * in the `scripts/run_matrix.sh` matrix (spikes/out/matrix/matrix.txt).

    Before the two fixes above, three of the six differed between Windows and Linux.

    The cheaper `rel-fast` profile (opt 3, no LTO, 16 codegen units, incremental) is reproducible too:
    * byte-identical across the two OSes for logic_mono and logic_wasm;
    * at two paths;
    * after an incremental edit and its revert, equal to a clean build.

    Parallel codegen and incremental state do not leak into the output [V].
  * **Not yet shown** [I]: ARM64 and macOS hosts (deferred by the user, 19:58Z); other host
    environments (locale, file-system order) beyond the three machines.
* **Toolchain distribution.** Every peer needs the pinned toolchain: rustc and the wasm32 std, about 1 GB
  [I]. Where it comes from: **D75**. Without it a peer can watch a world but cannot activate new modules.
* **What it costs** [V parts, I composition]:
  * the author's own loop is unchanged: 0.45 s from edit to first frame (13.7);
  * a commit activates for everyone only after every peer has built it, so the activation lead must cover
    the slowest peer's build plus the source transfer. The lead adapts to the slowest member's
    reported build times. **What depends on the profile peers build with** (P2-7 build-time table,
    Intel laptop, unthrottled; spikes/out/p2-7/build-times-laptop.txt) [V]:

    | Module | Cold build (fresh target: joining) | One-line edit, release (LTO, 1 CGU) | One-line edit, `rel-fast` |
    |---|---|---|---|
    | logic_v2 (spike guest) | 0.3 s | 0.18-0.26 s | n/a |
    | logic_mono (6.4k lines) | 2.4 s | **2.24-2.27 s** | **0.44-0.53 s** |
    | logic_wasm (the same, 9 crates), leaf edit | 2.0 s | 1.51-1.55 s | n/a |
    | logic_wasm, core edit (every crate) | 2.0 s | 1.87-1.94 s | n/a |
    | physics_in_wasm (Box3D, 0.5 MB) | **19.4 s** | n/a | n/a |

    * **The release profile does not fit the lead.** The 0.5 s lead (2.1) fits only small modules
      built with release (LTO, 1 CGU); a 6.4k-line module needs about 2.3 s on the laptop.
    * **`rel-fast` fits.** It is reproducible (above) and rebuilds the 6.4k-line module in about 0.5 s.
      **Settled (user, 22:47Z, "Fast builds"):** committed modules are built with the pinned `rel-fast`
      profile (no LTO), the one the author's own loop already uses (13.7). The user accepted its costs:
      about 1% of run time against LTO and about 35% more module bytes (below). Release (LTO) would be an
      optional size optimization that every peer has to agree on [I].
    * **Not switched yet:** the guest build scripts still use the opt-s LTO profile. The switch, with
      the re-recorded module hashes, is a follow-up to P2-6/P2-7 [I].
    * **Joining with a physics-sized module** costs about 20 s of building per module.

    **On this PC** (Ryzen 7 9700X, through the lock at idle priority, so noisy; spikes/out/p2-7/build-times-desktop.txt) [V], the same edit costs:
    * 2.31-3.42 s with release;
    * 0.42-0.49 s with `rel-fast`.

    Cold builds take 3.2 s for the 6.4k-line module and 14.9 s for physics_in_wasm.

    **What dropping LTO costs at run time** (wasmi, medians of 3, this PC, noisy; spikes/out/p2-7/runtime-wasmi-desktop.txt) [V]:
    * **logic_mono:** release and `rel-fast` are equal within noise. A tick takes 0.52 vs 0.51 ms at 1,000 entities, and 7.2 vs 7.5 ms at 10,000.
    * **physics_in_wasm:**

      | Profile | Run time | Module | wasmi compile |
      |---|---|---|---|
      | `rel-fast`-like | 7.19 ms/frame | 623 KB | 16.9 ms |
      | opt 3 with LTO | 7.10 ms/frame | 585 KB | 11.3 ms |
      | today's guest profile (opt s, LTO) | 8.06 ms/frame | 461 KB | 8.7 ms |

    So `rel-fast` loses about 1% of run time against opt 3 with LTO, and is about 11% faster than today's opt-s guest profile. Its cost is size: about 35% more bytes, and double the wasmi compile time, which happens during the activation lead, off the sim thread (13.3) [I].
  * **joining** a world means building every module it runs before activating them: seconds to minutes,
    by code size [I]. Each peer caches builds by source hash, so returning to a world rebuilds only what
    changed.
* **When a peer's build does not match** the author's hash, it must not run a different module, or it
  would diverge. It pauses that world at the activation frame, as for a missing artifact (D17), and
  reports a reproducibility failure to the author and the world; the others continue. What happens next
  is **D74**.
* **Compiling untrusted source** (strangers may join from day one, D21): every peer runs rustc on other
  people's code.
  * Builds run as a restricted process (21.5): no network, only the workspace and the toolchain readable,
    and CPU, memory and time limits. rustc is not hardened against hostile input (pathological macro
    expansion or const evaluation), hence the limits.
  * Build scripts and proc macros are refused, so a build runs no committed native code.
  * Only members with builder rights may commit (12.2).
* **Rejections.** `compile_failed` on the author's own machine is feedback to the author. Peers reject what
  they can decide identically: a module that fails wasm validation or the commit-time checks (13.4), or a
  build that does not match (above).
* **Dependencies (review L8).** Logic crates may depend only on vendored crates committed inside the world
  repository's workspace, with a `Cargo.lock`. Nobody needs crates.io access.
  * Build-time code (`build.rs`, proc-macro crates) runs native code on whoever compiles. It is refused in
    logic crates, except for the engine SDK's own macros, which ship with the toolchain.
  * The toolchain itself is pinned by the engine and invoked by absolute path (15.6; review NEW-8), never
    through a `rust-toolchain.toml` or `.cargo/` that cargo or rustup would load from the workspace.

### 13.3 Runtime: weighed for speed under the determinism constraint (D12)

The phase-1 choice (wasmi) was made for determinism alone. Re-weighed with the priorities, using
@iter-bench's measurements of a 6.4k-LOC ECS-style guest
([iteration-benchmark.md](iteration-benchmark.md), all [V] on this desktop):

| Runtime config | Logic at 10k entities (× native 0.9 ms) | Physics compiled to wasm (× native) | Module compile (264 KB) | Host call | Deterministic fuel | Canonical NaN | Native code compiled |
|---|---|---|---|---|---|---|---|
| wasmi 2.0, eager, fuel (phase 1) | 6.4 ms (7.0x) | 21x | 4.5 ms | 25 ns | yes | yes | none: 8 pure-Rust crates |
| wasmtime 50 Cranelift, fuel, NaN canonicalization | 1.6 ms (1.7x) | 4.7x | 33 ms (parallel), 199 ms (serial) | 2.8 ns | yes | yes | none on Windows [V]; `helpers.c` on Linux and macOS [I, build.rs] |
| wasmtime Cranelift, fuel, no NaN canonicalization | 1.3 ms (1.4x) | 2.4x | similar | 2.8 ns | yes | no: CPU NaN bits | as above |
| wasmtime Winch (baseline JIT), fuel, NaN canonicalization | 2.8 ms (3.0x) | 6.3x | 4.9 ms | 3.1 ns | yes | yes | as above |

* **Determinism held everywhere it was asked for:**
  * one final digest across all 12 configs and two processes;
  * fuel identical across instances and processes in every eager config;
  * wasmi's lazy mode reproduced the phase-1 fuel nondeterminism.
* **Fuel counts differ between runtimes, so a session cannot mix runtimes** (review D12-1).
  * Every member must run the same runtime and version.
  * With a fixed fuel budget, wasmi traps where Cranelift and Winch finish, and Cranelift traps where Winch
    finishes [V reviewer fuelprobe].
  * 3d0f98a's "Cranelift on Windows, wasmi on Linux and macOS" therefore splits cross-OS sessions. That
    trades priority 5 (accessibility) and R1's reach for priority 1. It is withdrawn.
* **4.3 rule 7 restated:** "the same engine version and the same sim-relevant configuration, verified by
  the CI determinism matrix", not "bit-identical binaries", which differ by OS anyway.
* **wasmtime compiles C on Linux and macOS, whatever the features** (D12-2). Its build script compiles
  `helpers.c` whenever its runtime is built with `std`, and both compilers need `std` [V reviewer: the
  musl cross-check fails looking for a C compiler].
  * Ways out:
    * a maintained one-line `[patch]` of build.rs, plus Rust definitions of the weak `__unw_*` symbols
      [I];
    * a `no_std` custom-platform build with a platform layer written in Rust [I].
  * The 100%-Rust audit now flags inline `asm!` as well; 20.2 lists the packages that use it (D12-3).
* **The benchmark flattered the guest** (D12-6).
  * The guest had no generics and every system function was `#[inline(never)]`. Without those
    annotations, the Cranelift compile went from 27 to 70 ms, and the Cranelift loop to 0.52 s.
  * On the `rel-fast` wasm the pipeline actually commits, wasmi runs logic at 9.8x native and Cranelift at
    2.1x [V reviewer]. Budgets use these numbers.
* **Hot swap** (D12-5). Cranelift takes 27-35 ms at 6.4k LOC and 131 ms at 45k LOC, against the 10 ms
  budget.
  * So the swap compiles off the sim thread, during the activation lead (15.2), and caches by canonical
    hash. A peer that rolls back across an activation frame reuses the cache.
  * Alternative: swap in Winch first (4-5 ms; 23 ms at 45k LOC) and tier up to Cranelift in the
    background.
* **Settled by the user (15:10Z, D12): wasmi everywhere now; Cranelift everywhere once the patch exists and a
  fuel-exhaustion test passes on all three OSes.** Never a per-OS split. Guest physics (27) is costed
  under wasmi accordingly.
* **NaN canonicalization** doubles the cost of float-heavy code on Cranelift (physics in wasm: 4.7x vs
  2.4x). Dropping it is safe only if guests never observe NaN bits, or if every peer is x86-64. Physics
  stays native in the engine (13.8), so the logic penalty is small: 1.7x vs 1.4x.

### 13.4 Guest ABI (phase-1 sketch) [V d1, I additions]

* **Imports `pe.*`:** a read-only world view (`body_count`, `body_pos`, …) and a command buffer
  (`apply_impulse`). Production adds component queries and an object API generated from the component IDL
  (16.5).
* **Exports:** `pe_tick(frame)`, `pe_state_ptr/len/version`, `pe_scratch_ptr`, `pe_migrate(from_version,
  len)`.
* Actions issued by logic run as member-0 entries on every peer identically, are never networked, and
  never take claims (8.4) [V d1].
* **Sync state** (7.4): the host-owned component columns plus the declared state arena.
* **Commit-time checks (review L3):** modules may not export or mutate globals other than
  `__stack_pointer`, may not mutate tables, and may not import outside `pe.*`.

### 13.5 Hot reload and rollback consistency [V d1, d2]

* A code change lands at an **exact activation frame A**, so every peer runs old code up to A-1 and new code
  from A.
* **State migration:** the host copies the old state bytes into the new module's scratch buffer and calls
  `pe_migrate`.
* A peer that learns of the commit late rolls back to A: it restores physics, guest state and the module
  version, swaps, and re-simulates. In the spike, peer B learned 45 frames late and matched peer A on all
  600 frames. A control peer without the commit diverged exactly at A, which shows the swap took effect.
* Snapshots record the active module id, so rolling back across a swap restores the old module [V d1].
* Activation frames are now chosen after confirmation (15.2), so a commit can no longer be un-finalized.

### 13.6 Sandboxing (review M6)

* No WASI and no host I/O: only the `pe.*` capability API.
* **Fuel per tick**, and every host call charges fuel in proportion to its cost: a raycast or query costs
  what it does in time, not one instruction [I].
* **Running out of fuel disables the module at that frame,** as a flag in rollback state. If a late entry
  rolls the sim back so the overrun never happens, the flag rolls back with it. Reverting the module is an
  ordinary, explicitly ordered commit by a human or an agent, never an automatic SCM action (D14d).
  A runaway module stops at the identical instruction on every peer [V d3].
* **Only fuel may stop a module at a frame** (review D12-7). wasmtime's epoch interruption is driven by a
  host timer, so where it fires differs between peers and between runs [I, from how it works]. It may
  only serve as a backstop that kills a hung process outside the session, never as an input to a frame.
* Memory limits through the runtime's resource limiter. Host functions validate every argument (ids,
  finite floats) [V d1].

### 13.7 Iteration latency: measured, and the targets (user, 09:27Z)

* **Measured [V iteration-benchmark 3.1-3.3]:** a one-function edit, through cargo build to wasm32, wasmi
  compile, instantiate, state migration and the first frame:

  | Guest size | `dev` | `rel-fast` (opt 3, no LTO, incremental) | `release` (fat LTO) |
  |---|---|---|---|
  | 6.4k LOC | 0.55 s | **0.45 s** | 1.3 s |
  | 17k LOC | 0.72 s | 0.64 s | 2.7 s |
  | 45k LOC (split crates) | 0.86 s | 1.2 s | 5.9 s |

  * The phase-1 estimate of 1-5 s was pessimistic.
  * Unoptimized `dev` wasm runs 12x slower in the interpreter (77 vs 6.4 ms per tick), so it is the wrong
    profile for play-testing.
  * Fat LTO is the only slow lever. It is for shipping builds only.
* **Targets** (2.1):
  * ≤ 0.5 s on the author's peer at up to about 10k LOC per module, ≤ 1.5 s at 45k LOC;
  * + confirmation and the activation lead (about 0.6 s) for every peer.
* **Levers that do not help [V or I as marked in the benchmark]:**
  * rustc's Cranelift backend cannot emit wasm32 [V]; for a native dev build it is no faster than the
    wasm `rel-fast` loop [V];
  * sccache cannot cache crates whose inputs just changed [I];
  * calling rustc without cargo saves at most about 0.09 s [V].
* **Agents** iterate faster still with `logic.check` (diagnostics only) and `logic.test_in_fork` (17.3)
  before committing.

### 13.8 Structure: an engine-native core plus WASM rules

* **Hot, stable systems live in the engine, natively:** physics (always), and the per-entity bulk of
  movement, spatial queries and math helpers.
  * In the benchmark, movement alone was 5.2 of wasmi's 6.9 ms.
  * Moving it native leaves 2.9 ms in wasmi or 1.2 ms with Cranelift [I: a sum of measured parts; the
    per-frame data copy, about 0.1-0.5 ms, was not measured].
* **Rules stay in WASM** (AI, combat, crafting, spawning, game modes), where hot reload matters.
* **Modules stay small:** split logic into crates once a module passes about 15-20k LOC [V: at 45k LOC,
  `rel-fast` builds take 1.2 s split vs 1.4 s as one crate, and fat-LTO builds 5.9 vs 10.9 s]. Keep the
  shared core crate stable, because a core edit rebuilds every system.
* Pass the host as `&mut dyn Host`, not as a generic, so codegen stays in each crate.
* **One wasm module per system** builds fastest under LTO (0.72 s vs 1.29 s) but needs a multi-module
  host ABI. That ABI is phase-4 work [V build; I ABI cost].

---

## 14. Shaders (R7, shader ruling)

| Option | Toolchain | AI-friendliness | Fit |
|---|---|---|---|
| **WGSL** | naga parses and validates in pure Rust; wgpu-native | Text with precise spans; naga diagnostics are structured; validation takes milliseconds | Best |
| HLSL | naga has **no HLSL frontend** (inputs are WGSL, GLSL, SPIR-V only) [S naga 30 feature list on crates.io]. Needs DXC (C++, a dynamically loaded DLL) to reach SPIR-V, then naga `spv-in` | Familiar to models; DXC diagnostics are text | Adds a non-Rust boundary |
| GLSL | naga `glsl-in`, pure Rust | Fine | Older idiom |
| rust-gpu | `spirv-builder` 0.11 (2026-10-13) [S crates.io]; needs a specific nightly plus a codegen backend dylib | Shaders in Rust | Slow compiles, heavy toolchain |

**Recommendation (D13): WGSL**, with HLSL/GLSL import as an optional convenience path later.

**Validation and hot reload [I]:**
* The committer runs `shader.validate` over MCP (naga parse, validate and spans as JSON).
* Every peer re-validates before activation; it costs about a millisecond. Pipelines are rebuilt
  asynchronously and keyed by content hash.
* Shaders are presentation-only, so they cannot affect determinism. They still go through the same SCM
  ordering, authority checks and activation frames (15), so all peers see the same look. Rule: GPU output
  never feeds the sim.

**Slang was evaluated** (branch `slang/eval`, 6344ec1, `docs/slang-evaluation.md`); the user kept WGSL-only
for now (D13).
* Its WGSL output matches hand WGSL bit for bit at equal GPU cost, and fits the 100 ms edit budget with a
  resident compiler.
* It brings modules, generics and autodiff, but needs a C++ compiler that crashed on 2 of 1,200 malformed
  inputs.
* It can be added later as an author-side front end without changing the committed format (WGSL) or the
  peer-side path (naga).

---

## 15. Git-style source control (R7, merge-seat ruling)

### 15.1 Repository model [I]

* A **world repository** is content-addressed (BLAKE3, pure-Rust mode; D16). It contains:
  * `logic/` (Rust crates, one wasm module each, with vendored dependencies and a `Cargo.lock`, 13.2);
  * `shaders/` (WGSL);
  * `assets/` (large blobs referenced by hash);
  * `scene/` (prefabs and the initial object tree);
  * `acl/`;
  * `world.toml`.
* **Commits** = `{parents, tree, author_key, certificate chain?, stamp, message, signature}`. A commit is an
  entry in the session log (10.8): signed, hash-chained per author, identified by its hash.
* Live sim state (where things are now) is not in the repository. "Save world" writes a snapshot commit.

### 15.2 Ordering and finality (reviews C1, H3)

* **Ordering:** commits are ordered by the same key as interactions (8.3): effective frame, then the
  per-frame tie rank.
* **The branch head** (`main` of the live world) advances by the first-ordered valid commit whose parent is
  the current head.
* **A commit is final once its frame is confirmed** (6.4, 10.8): every member holds every entry up to that
  frame, so every member computes the same head. This replaces the phase-1 rule ("final once older than 2 s,
  because no earlier commit can still arrive"). That rule was false under loss, stalls or partitions, and
  could give two peers two heads (review C1).
  * Typical confirmation is about 140 ms (8.5 frames) on 50 ms links [V session-sim r14, round 2], against
    2 s before (priority 2).
* **Activation frame = confirmation frame + lead** (at least 30 frames, so peers can fetch the artifact).
  Edit to running for everyone is therefore build (0.45 s, 13.7) + confirmation (~0.14 s) + lead (0.5 s).
* **Partitions:** only a side with a strict majority of the current membership epoch confirms anything
  (10.8). The minority's commits stay unconfirmed and are resubmitted after the heal. Nothing that ran as
  final is ever un-finalized (review H3).

### 15.3 Conflicts and rejection (R7, review H1)

* **The merge unit is the module.** Each logic crate builds to its own wasm module (13.8).
  * Two commits with the same parent that touch **different** modules are merged by combining their
    already-signed artifacts. No one recompiles anything.
  * Two commits that touch the **same** module, or the same shader or asset path, **conflict**. The later
    one in canonical order is rejected.
  * Phase 1 proposed merging non-overlapping *paths* and recompiling. Every peer would then have needed a
    toolchain, and reproducible output (review H1).
* **The loser is rejected with a machine-readable reason:**

```json
{"code":"conflict","your_commit":"b3:…","base":"b3:…","winner":"b3:…",
 "winner_frame":1234,"conflicting_modules":["logic/doors"],
 "required_action":"rebase","merge_seat":{"scope":["logic/doors"],"expires_after_s":10}}
```

* **Other rejection codes:**
  * `unauthorized`;
  * `wasm_invalid`;
  * `module_checks_failed` (13.4);
  * `hash_mismatch` (the canonical hash does not match the bytes);
  * `shader_invalid` (naga spans);
  * `reserved_path` (15.6);
  * `missing_artifact`;
  * `seat_held_by`.

  `compile_failed`, with rustc's JSON diagnostics, is advisory feedback from the author's own toolchain,
  never a peer-side verdict (13.2).

### 15.4 The merge seat (user ruling 08:51Z; review H2)

* A rejected submitter gets a **merge seat scoped to the modules or paths it conflicted on.**
* Within that scope, the seat at the front of the queue is active for **10 s = 600 frames.** Only its
  holder's commit may advance the head for those modules. If the holder lands a valid rebased commit
  inside those 600 frames, it wins. Otherwise the seat **lapses** and the next seat in that scope
  activates.
* **Commits outside the seat's scope proceed normally.** Phase 1 made the seat a global write lock, which
  convoyed unrelated commits and could starve the five-agents-on-five-crates case it was meant to protect
  (review H2).
* A commit refused with `seat_held_by` gets **no** seat of its own. It retries when the seat ends.
* Seat timing is counted in confirmed frames, so every peer agrees on when a seat lapses.
* The 10 s starts when the seat reaches the front of its scope's queue, not when it is granted
  (otherwise queued seats could lapse before their turn). Service time per seat is roughly build (0.5 s) +
  confirmation (0.14 s) + lead (0.5 s), well inside 10 s, so queues drain. This is **D14b**.

### 15.5 Distribution [I]

Commits and objects travel P2P by hash (16.7). Finalization requires only confirmation, not that everyone
holds the bytes. Activation waits for the artifact; the policy for a peer missing it is **D17**.

### 15.6 Reserved paths (R13-C1) [I]

A commit whose tree contains a **reserved path** is invalid and is rejected with
`{"code":"reserved_path","paths":[…]}`. Every peer applies the same check, because it is a pure function
of the tree. Reserved paths are the files that agent harnesses load as configuration, instructions or
code, so a builder cannot make another player's harness run them (21.3).

* **Reserved directory names** (any path component, any depth): `.git`, `.claude`, `.claude-plugin`,
  `.codex`, `.agents`, `.gemini`, `.opencode`.
* **Reserved file names** (any depth): `CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md`, `AGENTS.override.md`,
  `GEMINI.md`, `.mcp.json`, `.lsp.json`, `.worktreeinclude`, `.geminiignore`, `opencode.json`,
  `opencode.jsonc`, `.env`, and any name starting with `.env.`.
* **Name matching** defeats Windows and macOS aliasing. Before comparing, each component is NFKC-normalized
  and case-folded, HFS+-ignorable code points are removed, and trailing dots and spaces are stripped.
* **Also invalid:**
  * empty, `.` or `..` components;
  * components containing `/`, `\`, `:`, NUL or other control characters;
  * Windows device names (`CON`, `NUL`, `COM1`, … with any extension);
  * 8.3-style short names (`XXXXXX~N`);
  * symlinks and hard links (tree entries are only files and directories).
* The list is part of the world format version. Adding a name is a rule change that takes effect at an
  activation frame, like any other validation change.
* Old commits are not rewritten. The panel-workspace materializer (21.3) strips reserved paths locally,
  whatever the commit's age.
* World authors who want to brief AI agents write `agent-guide.md` (any non-reserved name works). The
  engine serves it over MCP as world-authored, untrusted content (17.2). No harness auto-loads it.
* **Toolchain configuration is reserved too** (review NEW-8). Cargo and rustup obey configuration in any
  ancestor directory: the reviewer verified a `rust-toolchain.toml` `path=` and a `.cargo/config.toml`
  alias. Therefore:
  * the directory name `.cargo` and the file names `rust-toolchain` and `rust-toolchain.toml` are reserved
    at any depth;
  * the engine runs `cargo` and `rustc` by absolute, pinned path, with `RUSTUP_TOOLCHAIN` pinned,
    `CARGO_HOME` set to an engine-owned directory, and `--config` overrides that empty
    `build.rustc-wrapper`, `build.rustflags`, `target.*.linker`, `target.*.runner` and
    `target.*.rustflags`, the `[env]` table and every `credential-provider` (round 3: the first list missed
    the last four);
  * cargo also reads `.cargo/config.toml` from **every ancestor** of the build directory, and `--config`
    cannot remove what such a file adds to a table. So workspaces are materialized under an engine-owned
    root outside the owner's home directory, and a build fails closed if any ancestor holds a
    `.cargo/config` or `.cargo/config.toml` [I];
  * compilation runs under the restricted identity (21.4);
  * the world's toolchain pin is world-format metadata the engine reads, never a file that cargo or rustup
    auto-load (this corrects 13.2's reference to a `rust-toolchain.toml` in the world repo).
* **Allowlist, not only a denylist** (review NEW-19) [I].
  * A materialized workspace contains only world-format paths and extensions: `.rs`, `.wgsl`, `.toml`
    (manifest names only), `.md`, the asset formats of 16.7, and the world metadata files.
  * The reserved list above is kept as a second check.
  * Every command the engine spawns uses an absolute path. On Windows the engine sets
    `NoDefaultCurrentDirectoryInExePath=1`, so a `git.exe` or `pe-app.cmd` in a workspace is never picked up.
  * Commands a harness spawns itself do their own PATH search (round 3). A restricted session's PATH
    therefore holds only engine-owned directories, and never its working directory [I].
* **Name collisions** (review NEW-25): a tree whose sibling names collide after normalization (`A.rs` and
  `a.rs`; `x` and `x.`) is invalid, so how it materializes cannot depend on write order.
* **Not covered:** build-time code. `build.rs`, proc-macro crates and `[build-dependencies]` run native
  code on whoever compiles. That needs its own rule under 13.2 and D11 (see review finding L8).

---

## 16. Objects, ECS and assets (R11)

### 16.1 ECS choice (D15)

| Criterion | Own sim store | bevy_ecs 0.20 | hecs 0.11 |
|---|---|---|---|
| Deterministic iteration | By construction (sorted columns by ObjectId) | Deterministic for an identical op sequence, single-threaded; the parallel scheduler needs explicit ordering [I] | Archetype order follows op sequence [I] |
| Stable IDs across peers | ObjectId = (creator peer, counter), assigned in sim | Entity IDs are allocator-local; needs a separate ObjectId map | Same |
| Cheap snapshot/restore | memcpy of columns; dirty tracking | No built-in world snapshot; reflection serialization is slow [I] | Serialization helpers, not memcpy |
| Components defined by WASM modules at runtime | Native (IDL-described columns) | Possible via dynamic components, awkward [I] | Not designed for it |
| Schema introspection for MCP | IDL gives JSON Schema directly | bevy_reflect | None |
| 100% Rust | Yes | Yes [V audit] | Yes [V audit] |

**Recommendation: our own small sim store** for rollback-relevant state, because sim components need
byte-exact snapshot and hash and must be definable by WASM modules at runtime. Presentation-side state can
use the same store or bevy_ecs; it does not need rollback.

**Built (P2-1, `crates/pe-sim`)** [V P2-1]:
* `ObjectId { creator: u32, counter: u32 }`, ordered by creator, then counter. Counters start at 1, so
  `ObjectId::NONE` is (0, 0). The store keeps each creator's last counter, so an id is never reissued,
  even after a destroy or a rollback. Across worlds an object is `GlobalId { birth_world, object }`
  (R14, P2-5).
* One column per component, sorted by `ObjectId` (4.3 rule 4), held in `Arc` buffers with copy-on-write
  (6.1). Components are described at runtime (16.5).
* `to_bytes` and `from_bytes` are the canonical little-endian state transfer (9.5). `from_bytes` checks
  everything, because the bytes may come from another peer: sorted ids and counters, schemas, sizes and
  values, the hierarchy, and lengths that fit `usize`. Every check is O(n log n) in the objects, so a
  hostile donor cannot stall a joiner.
* The crates refuse to compile for big-endian targets, because sim state is hashed and copied as raw
  little-endian bytes.

### 16.2 Hierarchy [V P2-1 for the store; I for the rest]

* **The hierarchy is the built-in `Node` component:** parent, first and last child, and previous and next
  sibling, an intrusive list in fixed 40-byte rows, so it snapshots and hashes like any column. Only the
  hierarchy API writes it. `set_parent` refuses cycles; destroying an object removes its subtree.
  Traversal is roots in id order, then depth-first with children in list order [V P2-1].
* **`Transform`** (80 bytes) holds an `f64` position, an `f32` quaternion, an `f32` scale, a root's chunk
  (`[i64; 3]`) and a pad. An object's
  frame is its nearest ancestor with a `Transform`; how roots and children are placed is in 25.2 [V P2-1].
* World transforms are propagated in a deterministic order (depth-first, children in list order) for sim
  objects each frame; presentation propagates at render rate. They are derived data, never stored or
  hashed, and they compose in a fixed IEEE order, so every peer gets the same bits [V P2-1].
* Reparenting is an interaction (sim objects) or a presentation edit (presentation-only objects).

### 16.3 Sim state vs presentation [I]

| Sim (rolled back, hashed, sync-checked) | Presentation only (never read by sim) |
|---|---|
| Transforms of sim objects, hierarchy of sim objects | Meshes, materials, shaders, textures |
| Rigid-body binding and Box3D state | Animation and skinning poses, IK results |
| Logic-module components and state | Particles, audio, UI, post-processing |
| Claims, ownership, ACL state | Remote avatar head and hand poses (interpolated) |
| Engaged VR hand targets (18.4) | Local VR head and hands (until engaged) |

### 16.4 Box3D binding [V b1, b4; I for the component layout]

* `RigidBody {type, shapes: [ShapeRef(asset hash, local transform, material)], …}` on an object makes a
  deterministic system create the Box3D body, in ObjectId order.
* The `ObjectId ↔ BodyId` table is sim state. BodyIds survive snapshot restore [V b1, b4].
* Box3D owns the transform of physics objects; the ECS mirrors it after each step.
* Collision geometry is sim-relevant, so it comes from content-addressed assets with deterministic
  processing (16.6).

### 16.5 Components from WASM logic [I]

* A logic crate declares component schemas in an IDL (Rust attribute macros in the SDK generate a manifest
  embedded in the wasm custom section).
* **The IDL is a field list with no padding (P2-1)** [V P2-1]. A component is a list of fields (name,
  kind, offset, count) that covers its row exactly:
  * no gaps or overlaps;
  * natural alignment, and the row a multiple of its alignment.

  So no row has padding bytes, and every row is safe to hash and copy as bytes. A struct that needs
  padding declares it as a field. `schema_hash` covers the whole description; snapshots record it, and
  restoring into another schema fails. Native components and logic modules' components use the same
  descriptions. Values are checked where they enter (4.3 rule 6): no NaN, no infinities, no bools other
  than 0 or 1. Runtime registrations cannot take the engine's ids below 1024.
* The host allocates columns and exposes typed query, get and set calls. The same schemas produce the MCP
  JSON Schemas (17.2).
* Schema changes ship in commits. Activation migrates existing data: new fields get defaults, and a custom
  migrate hook runs.
* What of a module's state is synchronized, and how presentation-only state stays out of it: 7.4.

### 16.6 Asset pipeline [I]

* **AssetId** = BLAKE3 hash of the canonical processed bytes.
* **Importers** (all pure Rust [V audit `assets-pure`]): glTF (`gltf`), PNG/JPEG (`image` with only those
  features), BSON and LZ4/LZMA/Brotli (Resonite, 19), zstd decode (`ruzstd`).
* **Processing must be deterministic** for sim-relevant assets (collision meshes, convex hulls). Same input
  bytes plus same importer version give the same output hash. The importer version is part of the hash
  input.
* **Hot reload:** a commit swaps the AssetId reference at an activation frame.
* Avoided: `image` default features pull in an AV1 encoder (rav1e) [V audit]. blake3 needs its `pure`
  feature (its default compiles C/asm) [V audit; S build.rs].

### 16.7 P2P asset distribution [I]

* Peers announce have-lists of hashes (bloom filter). Missing hashes are fetched in chunks from several
  peers in parallel, with per-chunk verification (BLAKE3 tree / bao-style).
* A joiner fetches the active manifest first and lazily pulls presentation-only assets. Sim-relevant
  assets must be present before their activation frame.
* **Inventory** (Resonite's records) = signed manifests of AssetIds held locally and shareable.

### 16.8 Versioning alongside code

Assets live in the same world repository tree as code and shaders. Large blobs are stored by hash
(LFS-like) with the same commit, ordering, rejection and seat rules (15).

### 16.9 MCP tools for objects and assets (17.2)

* Objects: `object.query`, `object.create`, `object.set_component`, `object.reparent`, `object.delete`.
  They become signed interactions, are checked against the ACL, and take effect at a frame.
* Prefabs and assets: `prefab.save` and `asset.import` / `asset.get` / `asset.list`. These become commits.

### 16.10 Keeping a world: autosave, checkpoints and backups (Q29, user 08:31Z)

The user ruled (Q29, 08:31Z): worlds autosave, players also save checkpoints by hand, and every world
keeps a list of backups that can be loaded at any time. The UI is Gnosarium UX 8a.10, approved by
@shader-reviewer at 08:40Z; this is the storage and the engine rules it asks for (its need 2i).
Everything here is a proposal unless marked as a ruling [proposal; I, nothing built]. Revised after
@design-reviewer's round 1 (SV-1 to SV-11).

**What a load may change** (@coordinator-opus's rulings, 09:34Z, on SV-2 and SV-1):
* **A load restores world content, never governance.** Ownership, the ACL (`acl/`), roles, join rules,
  route lists and provenance stay at their current state across a load. So a non-owner Admin cannot use a
  load to undo a revocation or a demotion, or to bring back a former owner.
* **Cross-world exits, arrivals, escrow and the seen tables never rewind** (22.6, 22.7). An object that
  left stays gone, and one that arrived stays: the restored world is the backup's content, minus the
  objects whose current last-exit record says they have left since, plus the objects that have arrived
  since, in their current state.

**What a backup is:**
* **Content:** the world's content tree at a commit (chunk files, logic modules, shaders and assets, by
  BLAKE3 hash, D16; the tree of 15.1 without `acl/` and the governance fields of `world.toml`) and the
  runtime image at a check frame (9.2), so restored objects and the logic that runs them always match.
  Blobs are stored by hash, so a backup costs only what changed since the previous one.
* **Bound to authority** (SV-8): every backup is taken at a check frame and stores that frame's BLAKE3
  check hash with the quorum's signed acks of it (9.1, 9.5), and the epoch certificates that say who the
  quorum was, so a later verifier can check the acks (SW-7). An autosave is also the writing owner's
  signed commit. A checkpoint row is signed by its saver and carries proof that the saver's role held
  Save checkpoints at that frame. So a holder cannot plant a backup, and a load checks it against signed
  data, not only its own bytes. A single-member session has no quorum (9.5), so its backups rest on the
  owner's signature alone (SW-7).
* **Object ids that persist are world-scoped** (SW-2): (the creating session's id, creator MemberId,
  counter), where the session id is the session's world and genesis members (10.3). A session's own
  entries may name its own objects by 10.3's session-local (creator, counter); everything stored,
  loaded or merged uses the world-scoped form. So objects from different sessions never share an id:
  across a load from an earlier session's backup, with arrivals carried forward, across a head merge,
  and when the next session opens from either.
* **The engine build** (SV-4) is recorded in each backup. A load on the same build restores the runtime
  image exactly. On another build the image's memory layouts no longer apply (9.5, 4.3 rule 7), so the
  load restores from the version-neutral layer, which every backup writes for that purpose (SW-3): the
  chunk files and dormant records (25.5.7) for every body, not only for frozen chunks, with logic rebuilt
  from source under the current toolchain pin.
  * A module's declared state arena (7.4, 13.4), such as scores or quest flags, is raw guest memory whose
    layout the build decides, so it crosses builds only through the module's `pe_migrate` (13.4). A
    module with no migration path from the backup's version restores its components and starts with
    its arena reset, and the load card names those modules.
  * A cross-build load also loses the physics caches (contacts, warm starts, sleep timers). Every
    backup therefore stays loadable at any time, exactly so on its own build, and with these stated
    losses on another.

**Autosave:**
* Every 5 minutes while the world changes, and once as a session ends (V3), the session's edits are
  written into the world, so the next session starts with them. Until then the engine keeps them as
  session state (25.5.7).
* A write is a commit to the world repository (15), signed by an owner's client while an owner is
  present. With no owner present, edits wait as session state, listed for the owners, and are written
  when an owner next joins, unless the owners let builders write (8a.10). Builders writing needs a
  delegation certificate from an owner (as in 12.3) that lets them sign head references; under it the
  head tie-break below is in builders' hands too (SW-5). Until they are written, a new session starts
  without the pending edits, from the world's current head.
* **The world's head** (SV-3) is an owner-signed head reference: "head = commit C, from parent P". Every
  holder computes the same head from signed data alone; arrival order never counts. Two references from
  the same parent are a detectable conflict, as in the ownership chain:
  * if their changes touch disjoint paths, they merge as 15.3 merges two commits from one parent, and no
    card is shown;
  * if they overlap, both heads are kept and the owners see 8a.10's compare card (Keep this one, or Keep
    both as versions). Until they choose, every holder opens from the head whose reference has the lower
    hash. A writer can grind that tie-break, but it only decides which version opens until the owners
    choose, and both stay kept;
  * owner-less edits written later name as parent the head their session started from, so they follow
    the same rule.
  * Runtime state is stored per chunk (the dormant records above), so two autosaves overlap only where
    they changed the same chunks (SW-6). Shared tables have their own rules: creator counters merge by
    maximum, and the cross-world tables change only in the trading-lease holder's head (below), so a
    merge never has to combine them.
* **Two live instances and cross-world trade** (SW-1; @coordinator-opus's ruling (a), 09:51Z): only one
  live session of a world trades across worlds at a time. It holds a trading lease bound to the
  owner-signed head reference, and other instances refuse `Depart` and `Arrive`. The lease (granting,
  fencing, hand-off, expiry, what other instances show) is designed in
  @item:primordial-engine/trading-lease-only-one-live-instance-of-a-world-t.

**The backup list** holds four kinds (8a.10): **checkpoints**, kept until an owner deletes them;
**autosaves**, every one for the last hour, then hourly for a day, daily for a month and weekly after
(8a.10's schedule); **session ends**, one for each closed session; and **before load**, one for each load.
* Owners, and anyone whose role has Load backups (the Admin role has it), see the list. Anyone who sees
  it may rename a backup; only owners delete.
* Renames and deletions are signed and versioned (SV-8). A deletion is a signed tombstone, so a deleted
  checkpoint does not come back from another holder.
* Every holder of the world keeps the list and thins autosaves by the schedule, locally. A thinned
  autosave that another holder still offers is authentic, only older, so thinning needs no signature.
* Backups travel with the world, like its chunk files.

**Loading a backup:**
* It is a human action: no agent certificate can carry it (12.3). It needs the Load backups permission,
  checked at the load entry's ordered frame like a kick, so a load queued by an admin demoted earlier in
  that order is refused. The entry names the backup by its manifest hash, not by its row (SV-10).
* **In a live session** it lands like a divergence repair, as a state transfer (9.5) at an activation
  frame A. This section owns the rule (9.3 and 9.5 have no such step):
  * The load entry is ordered by the view change, like a kick, and fixes the peer set: the members of
    the epoch that ordered it, less any removed meanwhile (SV-6). Joiners during the wait do not count.
  * Each member confirms in its acks that it holds the backup (its chunks and runtime image, fetched if
    missing). A is the frame at which the last confirmation is confirmed plus a lead of at least the
    speculation window W and a margin (SV-5), rounded up so that A − 1 is a check frame (SW-7). Both
    are functions of the log, so every peer computes the same A.
  * **The deadline** (SW-4): if 30 s after the ordered frame not every member has confirmed, the load
    activates only if a quorum of the fixed peer set has. Otherwise it is cancelled and everyone is
    told, so a member that never confirms can only cause a retry, which an admin can follow with a kick.
    On the deadline path A is set the same way, from the frame at which the deadline was reached.
  * At A, a member still without the backup (at most n − q of them) pauses, as for a missing artifact
    (D17), and the view-change timers, which keep their quorum, remove it if it stays paused. Admins see
    who has not confirmed.
  * Once the load entry is ordered, new departures to other worlds are refused until A (SV-9), so the
    transfers in flight are a fixed set. The load waits for them to settle; an arrival stamped at or
    after A applies to the restored world.
  * **The before-load backup** (SV-7) is the world at A − 1, a check frame: a pure function of the log,
    so every peer produces the same one, and its check hash, signed by the quorum's acks, is stored with
    it. The load entry names it by that frame, not by its hash, and it needs no signature of its own.
  * **Entries after the switch** (SV-5): an entry stamped at or after A counts only if its author's
    chain acknowledged the activation before it, that is, holds an ack referencing the entry that fixed
    A (the last confirmation, or on the deadline path the load entry; SW-7). Any other was authored
    against the pre-load world, so it is rejected deterministically, carried state and all. The lead of
    at least W lets an honest author acknowledge in time.
  * **Object ids** (SV-5, SW-2): restored objects keep their world-scoped ids, so none collides with
    another session's. Within this session each creator's counter continues from the larger of its
    current and restored values, so no new id reuses a restored one. Every entry counted after A was
    authored against the restored world, so an id always names the restored object; entries before A
    named pre-load objects and applied before the switch.
  * The restored store's frame field becomes A, so check hashes after the load compare from A.
  * People are not world state: avatars, scale and voice stay; anyone inside restored geometry moves to
    the nearest clear spot; held objects are released and restored.
  * Two loads activate in log order, even if the second's confirmations finish first (SV-10).
* **With nobody in the world** (SV-11): a load is an owner-signed commit outside any session that moves
  the world's head to the backup's content, with governance and the cross-world tables carried forward
  as above. The next session starts from it.
* **Open as new world** starts a separate world from a backup, with a new ownership chain whose first
  owner is whoever opens it. It needs the Copy world permission, held only by owners by default
  (@coordinator-opus's direction, 08:35Z). The copy re-mints its objects' births in the new world, so
  they can later be exported from it (SV-11).

**Regions** (25.5; @design-reviewer's proposal): a save is a world-log entry stamped at a check frame F.
Each region writes its checkpoint at F once it has confirmed F, a pure function as the before-load
backup is; frozen regions contribute their `Frozen` records; cross-region perturbations stamped before F
belong to their source region's state at F (25.5.5). The backup is complete when every region has
confirmed F. This costs each region either images kept back to F or a re-simulation from its last
checkpoint before F. A load activates per region at one world-log frame, by the same rule.

---

## 17. MCP integration (R8, AI-forward framing)

MCP is the primary authoring interface, not an add-on. Every capability a human has through in-world
tools also exists as an MCP tool, with structured input and output.

### 17.1 Where it lives [I; protocol shape V mcp; transport rules S MCP spec]

* **An MCP server is embedded in every peer.** There is no central MCP server.
  * An agent drives *its user's* peer. The peer turns agent actions into signed interactions and commits.
  * **Every MCP token maps to one session and its principal** (21.3). Rights come from that mapping, never
    from tool arguments.
* **Default transport: stdio.**
  * The engine launches the harness, and the harness launches `pe-app mcp`.
  * `pe-app mcp` connects to the engine over a named pipe (Windows) or a Unix socket (Linux). Its ACL
    admits only the session's OS identity.
  * No TCP port is opened, so web pages and other machines cannot reach the server.
* **Fallback: Streamable HTTP on loopback,** for clients that cannot use stdio. The MCP specification
  requires: "Servers **MUST** validate the `Origin` header on all incoming connections to prevent DNS
  rebinding attacks"; servers "**SHOULD** bind only to localhost (127.0.0.1)"; and they "**SHOULD**
  implement proper authentication" [S MCP spec 2025-06-18]. The engine's rules:
  * **Bind:** `127.0.0.1` and `::1` only, on an ephemeral port. Never `0.0.0.0` or `::`.
  * **`Host` check:** only `127.0.0.1:<port>`, `[::1]:<port>` or `localhost:<port>` is accepted; anything
    else gets 403. A DNS-rebinding page sends its own hostname.
  * **`Origin` check:** a request that carries an `Origin` header is refused unless the origin is on an
    allow-list, which is empty by default. MCP clients that are not browsers send no `Origin`.
  * **No CORS:** the server never sends `Access-Control-Allow-Origin`.
  * **Token:** one bearer token per session, 256 random bits, sent in `Authorization`. It is compared in
    constant time and never logged, and never appears in a URL or on a command line.
  * **Rotation:** a new token at every harness launch and resume. Tokens expire when the session ends,
    after an idle TTL, and immediately on revocation. `Mcp-Session-Id` is not an authentication factor.
* **Library:** `rmcp` 3.8.0 (official Rust SDK): server, macros, schemars, stdio and Streamable HTTP
  [V audit `mcp-rmcp`; V pe-mcp].
* **Built (P2-8):** `crates/pe-mcp` replaced `spikes/mcp-stub`. Claude Code 2.1.292 drove a peer through
  a scripted scenario over the pipe and over HTTP, and both runs passed with identical results [V pe-mcp:
  `crates/pe-mcp`'s 46 tests on Windows and Linux, and the recorded runs in `spikes/out/mcp/`].
* **Pipe and socket specifics** [V pe-mcp; on Linux, the tests only]:
  * Windows: a protected DACL with one ACE, full access for the current user's SID
    (`D:P(A;;GA;;;<sid>)`, read back from the pipe in a test). The first instance is created with
    `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a second server on the same name fails. Remote clients are refused,
    and the name is random.
  * Unix: the socket lives in a fresh `0700` directory under `$XDG_RUNTIME_DIR`, the socket file is `0600`,
    and the peer's uid (SO_PEERCRED) must equal the engine's.
  * Token: the bridge's first line is `PE-MCP-AUTH <token>`, and the engine answers `OK` or
    `ERR unauthorized` and closes. The token is re-checked on every call, so revocation and expiry act on
    open connections.
  * The token reaches the harness only through its environment (`PE_MCP_TOKEN`), which harness MCP configs
    can reference (`${PE_MCP_TOKEN}`). Every subprocess the harness starts (its shell tool, for example)
    inherits it, and so can reach the peer with the harness's scopes. Launch profiles (`pe-harness`) should
    remove it from tool subprocesses where the harness allows, and give a harness no more scopes than it
    needs [I].
  * No re-issue yet: a harness idle past the token's TTL (1 h by default) loses the server until it is
    launched again. Re-issue on resume belongs to `pe-harness` [I].
* **HTTP specifics** [V pe-mcp]: the server runs stateless (no `Mcp-Session-Id`), so authority comes from
  each request's token alone. Tests cover a foreign `Host`, a suffix-attack `Host`, a second `Host`
  header, a refused and an allowed `Origin`, preflight, the absence of CORS headers, a token that is
  missing, wrong, sent in the wrong scheme or sent in the URL, and revocation.

### 17.2 Tool and resource catalog (proposed) [I]

* **Tool names use underscores:** `world_get_state`, not `world.get_state`. The Anthropic API accepts only
  `^[a-zA-Z0-9_-]{1,64}$` in tool names [I: the API's documented pattern, not re-checked here; Claude Code
  passed underscore names through unchanged in both runs, V pe-mcp]. The table keeps the dotted names as the
  catalogue's spelling.
* **Phase 2 implements** [V pe-mcp]: `session_info`, `world_get_state`, `world_get_object`,
  `world_propose_interaction`, `world_interaction_status`, `world_step`, `world_hashes`,
  `session_members`, `session_sync_status`, `scenario_load`, `scenario_fault`, `scenario_check`, and the
  resources `primordial://world/state`, `primordial://session/members` and `primordial://session/sync`.
  The full schemas are in `crates/pe-mcp/schema/tools.json`.
* **Phase-2 scopes** (the token decides them): `read`, `interact` (propose, and read its own tickets) and
  `control` (`world_step` and `scenario_*`, simulator only: a live peer's clock is real time).
  `tools/list` shows only what the token's scopes allow. The session kinds of the table below map onto
  scope sets when panels arrive.
* **Errors:** a tool error carries `{code, message, field?, details?}`. The codes are `invalid_argument`,
  `unknown_object`, `unknown_ticket`, `forbidden`, `not_owner`, `rate_limited`, `not_ready`, `peer_crashed`,
  `no_scenario_control`, `session_faulted` and `internal`. An unknown tool or a malformed request is a
  JSON-RPC error.
* **Interaction tickets:** proposing returns a ticket. Its status is `pending`, `accepted`, `rejected`
  (with reason, such as `claimed` with its holder and end frame) or `void`, plus `final` once the stamp is
  confirmed. Before that, a rollback can still change it. At most 64 unresolved interactions per principal
  (`rate_limited`) is the first per-session rate limit of 17.4.
* **A hand is its member's alone:** proposing a `hand` move for another member's hand returns `not_owner`
  before anything is authored [V pe-mcp]. That protects only an honest peer, so every replica's
  validation also refuses such an entry (8.4, `RejectedNotOwner`; in the spike on branch `viewchange`,
  in review), and pe-session carries the rule (P2-3).
* **Ordering:** the peer runs MCP calls one at a time, in arrival order. Calls a client sends in parallel
  have no defined order between them [I].

| Area | Tools / resources |
|---|---|
| Session and world | `world.get_state`, `world.query_objects`, `world.get_object`, `world.hashes`, `session.members`, `schema.components` (JSON Schema per component type), resource `primordial://world/state` with subscriptions for frame/commit/rejection events; resource `primordial://world/agent-guide` (world-authored text, returned as untrusted, 15.6) |
| Interactions | `world.propose_interaction` (same path as a player's: frame-stamped, soonest-wins, rollback [V mcp]), `object.*` edits (16.9) |
| Logic | `logic.read`, `logic.write` (draft workspace), `logic.check` (rustc `--error-format=json` diagnostics), `logic.test_in_fork`, `scm.commit`, `scm.status`, `scm.rebase`, `scm.seat_status` |
| Shaders | `shader.validate` (naga spans), `shader.commit`, `shader.preview_in_fork` (render-only) |
| Assets | `asset.import`, `asset.get`, `asset.list`, `prefab.save` |
| Admin | **Not MCP tools.** `acl.grant`, `acl.revoke`, owner changes and `agent.delegate` exist only as human actions in the engine-owned approval layer (12.3). Agents may read ACL state (`acl.view`). Session admin actions (kicks, roles, join rules; 12.4) likewise: an agent may request one, and the request waits in its owner's approval layer [proposal, Gnosarium UX section 15, item 11] |
| Panels and hubs (R13) | `panel.connect` (private session only); `panel.send` (private session: any of its connections; inbox session: its own); `panel.inbox` (private session: owner-forwarded items only); `panel.close`; `panel.requests.*` (decider session only); `hub.contacts`; `hub.send` (first contact needs owner approval); `hub.inbox` (private session: owner-forwarded items only) (21.6, 21.7) |
| Worlds and portals (R14) | `worlds.list`, `worlds.join`, `worlds.leave`, `worlds.set_presence` (owner-confirmed), `portal.*`, `transfer.request`, `transfer.status`. Every tool takes an explicit `world` (22.9) |
| Panel turn, voice and context (21.5, 21.11-21.13) | `panel.turn.sender` (the verified sender, its standing as owner, guest or confidant, and `input: voice`); `panel.speak {text}`; resource `primordial://panel/context` (the turn's spatial context; a live subscription only with the live-feed permission); `annotation.list`, `annotation.get`, resource `primordial://panel/annotations` |
| Settings (21.14) | `settings.list`, `settings.get`, `settings.set`, `settings.undo`; refused in guests' turns |

**Scopes per session kind** (the token decides the kind, 21.3):

| Session | Principal | World tools | Panel and hub tools |
|---|---|---|---|
| Private | Owner | The owner's delegated scope; commits per 17.4 | `panel.connect/send/close`, `hub.*`; inbound only when forwarded |
| Unlocked panel (21.5) | The owner, plus guests the access level admits; each turn's sender is verified and shown | The owner's, as for the private session (user ruling 19:00Z); `requested_by` records the guest | As the private session |
| Inbox | One remote panel or hub correspondent | Read-only queries if granted; world actions only if the owner's ceiling allows | Reply on its own connection or thread only |
| Decider | Untrusted request text | None | `panel.requests.list/accept/reject`, capped at the owner's ceiling |

* **Guests' and confidants' turns** at the owner's panels (21.5): settings tools refuse in a guest's turn
  (21.14), while a confidant's turn has the owner's tools. Each turn carries its own sender's spatial
  context (21.12).

**Peer-authored strings are marked untrusted [I]:**
* **Wrapping.** Every result field that holds text written by another peer is wrapped as
  `{"untrusted": {"author": "<key fingerprint>", "source": "<kind>", "text": "…"}}`. Engine-generated
  fields are never wrapped.
* **What counts as peer-authored:**
  * object names, and component string fields;
  * logic and shader source by other authors;
  * commit messages, and rejection reasons that quote peer content;
  * panel messages, hub mail and presence records;
  * the world agent guide.
* **Cleaning.** Wrapped text is sanitized by the 21.5 rules and length-capped. Long sources, such as logic
  files, come back as resources with the same marking.
* **Limits.** Restricted-session system prompts explain the convention. The marking is a hint to the model,
  not a control. The controls are scopes, containment and human confirmation.

### 17.3 Deterministic forks: the AI test bench [I, building blocks V b1/b4/d1]

* An agent can fork a **private, local copy** of the live session at the current frame: restore the latest
  snapshot into a fresh world [V b4].
* In the fork it applies a candidate commit or interactions, runs N frames faster than real time, and reads
  back hashes, metrics and object states. Nothing reaches the network.
* This gives AI authors a safe edit-test loop with exact reproducibility.
* The phase-2 simulator backend already shows the property forks rely on. The same `scenario_load`
  parameters and seed, with the same calls, give identical hashes: tested across two engines, and seen in
  two Claude Code runs over different transports that agreed on every reported number [V pe-mcp].

### 17.4 Same checks as human edits [I]

* MCP actions are signed with the session key under its certificate chain (12.3).
* They pass through the same pipeline as human actions:
  * ACL validation, frame stamping and soonest-wins ordering;
  * rollback;
  * SCM ordering, rejection and merge seats.

  No MCP path bypasses the sim or the SCM.
* **In shared worlds, agent-originated commits need a human, by policy.** A world is shared when more than
  one user holds `owner` or `builder`. `world.toml` sets `agent_commits` to one of:
  * **`confirm`** (default in shared worlds). Each commit signed by an agent key waits until its delegating
    user confirms it in the approval layer.
    * The dialog shows the diff summary, the author chain and the touched paths.
    * The user key signs the confirmation, which travels with the commit.
    * Peers reject an agent commit that lacks it.
  * **`confirm-batch`**: one confirmation covers the agent's commits for the next N minutes, or to listed
    paths.
  * **`auto`**: no confirmation. This is the default in worlds with a single builder; in a shared world it
    is an explicit owner choice.
* **Cost of `confirm`:** each commit waits for a human round trip, which slows the AI iteration loop (user
  priority 2). `logic.check` and `logic.test_in_fork` need no confirmation, so iteration before a commit is
  not slowed. Decision: **D28a**.
* **Commits prompted by a guest at an unlocked panel** follow the owner's own rules, `confirm` included
  where it applies, and record the guest as `requested_by` (21.5).
* **Interactions and object edits by agents** are not confirmed one by one, because they are too frequent.
  Scope and per-session rate limits bound them.
* **Limits:** rate limits and fuel apply per agent session.

### 17.5 AI-friendliness requirements met by design

* JSON Schemas for every tool and component.
* Structured diagnostics: rustc JSON, naga spans.
* Machine-readable rejection codes and seat status (15.3).
* Introspectable schemas and state.
* Deterministic forks for testing.
* Fast check loops: about 0.25 s for a small guest [V d5], milliseconds for WGSL validation [I].
* Tool schemas carry no non-standard JSON Schema formats. schemars' `uint32` and `uint64` made Claude Code's
  validator warn on every tool, so the server strips them and keeps the `minimum` bounds [V pe-mcp].

---

## 18. VR (R10)

### 18.1 OpenXR from Rust [V audit, I integration]

* The `openxr` crate with its default `loaded` feature loads the Khronos loader dynamically. No native
  code is compiled [V audit `vr-openxr-loaded`]. The `static` feature would compile the C++ loader with
  CMake [V audit `vr-openxr-static`], so it is excluded.
* The loader and vendor runtime (SteamVR, Meta/Oculus PC, Virtual Desktop, Pico, Monado) are non-Rust
  system components (section 20).
* Target platforms and runtimes (D18a, D20; user 18:57Z): **Windows and Linux from day one**, desktop and
  PC VR through OpenXR on Vulkan: SteamVR and Meta/Oculus PC on Windows, SteamVR and Monado on Linux.
  macOS later, flat-screen only.

### 18.2 wgpu interop [I]

* Use **Vulkan** for VR on Windows and Linux:
  * create the Vulkan instance and device through `XR_KHR_vulkan_enable2`;
  * hand them to wgpu via `wgpu-hal` (`Instance::from_hal`, `Adapter::from_hal`, `create_device_from_hal`);
  * wrap OpenXR swapchain images as wgpu textures (`create_texture_from_hal`).
* This is the approach used by existing Rust OpenXR + wgpu projects. We have not run it. Spike candidate
  for phase 2.
* D3D12 via `XR_KHR_D3D12_enable` is an alternative on Windows.
* **Stereo:** single-pass multiview (Vulkan `VK_KHR_multiview` through wgpu's multiview pipelines), with
  two-pass as fallback.

### 18.3 Frame timing [I]

* `xrWaitFrame` paces the render/XR thread at 90-120 Hz. The sim stays at a fixed 60 Hz on its own thread
  (5).
* Each XR frame draws sim state at the predicted display time: extrapolated and error-smoothed, with
  objects held by the local hand attached to the late-latched hand pose (5; review M2). Head and hand poses for local
  rendering are late-latched from `xrLocateViews`/`xrLocateSpace` at the predicted display time.
* Local head motion never waits on the sim or the network. The compositor's reprojection covers
  render-time misses.

### 18.4 Tracked poses and "only network interactions" [I]

* **Local head and hands** drive the local avatar at display rate. This is presentation.
* **Remote avatars:** pose streams at 30-60 Hz, unreliable and presentation-only. They are interpolated
  with a jitter buffer of about 100 ms. They are not rollback state. This keeps R3: poses are not physics
  interactions.
* **Hands become sim inputs only when engaged.**
  * When a hand starts a grab, or its swept path over the next frame (widened in proportion to its speed)
    reaches a dynamic object, the peer begins streaming frame-stamped **hand-target inputs** (review M2:
    at 5 m/s a hand moves 8 cm per 60 Hz frame, so a fixed few-cm margin misses fast swats): position, rotation and grip, quantized to a
    1 mm / 0.001 rad grid so every peer sees identical bits.
  * In the sim, an engaged hand is a kinematic Box3D body with that target. A grab is a weld or motor joint
    to the held body. A throw follows from the kinematic hand's velocity.
  * The engaged hand claims the touched object (8.3), so the "soonest wins" rules apply to tug-of-war.
* A hand sweeping through many objects is a continuous perturbation source (7.5).
* Engagement thresholds and whether non-engaged hands collide at all: **D18b**.

### 18.5 Comfort and latency budgets [I]

| Path | Budget | How |
|---|---|---|
| Head motion to photon | under 20 ms | Compositor reprojection plus late-latched local poses; independent of sim and network |
| Own hand on an object | 0 added input delay | Rollback netcode predicts locally; local inputs apply at the present frame |
| Remote player's action, seen locally | One-way latency plus up to one frame | Rollback re-sim plus about 100 ms error smoothing (5) |
| Rollback re-sim at 90 Hz | ≤8 ms per tick, the rest amortized | 6.5; physics budget per VR world (D2) |

### 18.6 Mobile and standalone (excluded from MVP; what would block it later)

* Nothing chosen precludes Android or Quest-native: box3d-rust, wasmi and wgpu-on-Vulkan all run there,
  and the Android OpenXR loader is loaded the same way [I].
* Performance is the blocker: a serial scalar physics port plus an interpreter on a mobile CPU.
* Avoid desktop-only assumptions in the engine core, such as 1 MiB guest stacks or a 2 GB snapshot budget.

---

## 19. Resonite avatar import (R12, as narrowed)

**v1 scope (ruling 08:34Z):** mesh, skeleton and skinning needed to wear it, and textures. Out of scope:
ProtoFlux, custom components, dynamic bones, visemes and blendshape drivers, complex materials. The path
to those is noted below, not scoped.

### 19.1 Getting the data

* **Local check result.** Resonite is installed on this machine. Its local cache is a 2.5 GB LiteDB
  database (`Data.litedb`), and there is no exported package or standalone data-tree file to parse. So the
  "parse one header" check was **not reachable cheaply**. Format details below are **[I] community
  knowledge** unless marked otherwise.
* **Path A, native package (preferred for fidelity) [I]:** Resonite's package export (`.resonitepackage`)
  is a ZIP container. It holds the record metadata, a main **data tree** (BSON, compressed; variants
  distinguished by extension, e.g. LZ4 `.lz4bson`, LZMA `.7zbson`, Brotli `.brson`), and the referenced
  assets by hash: meshes in Resonite's **MeshX** binary format, and textures as image files.
* **Path B, standard export [I, to verify]:** if Resonite's in-game export produces glTF/GLB with skinning
  intact, the importer can consume that through `gltf` with no Resonite-specific parsing.
* **Phase-2 verification task:** export one avatar both ways on this machine. Parse the package's
  container, data-tree header and MeshX header, and compare.

### 19.2 Parsing in pure Rust [V audit for the crates; I for the formats]

* ZIP: the `zip` crate with deflate-only features (still to audit).
* BSON: `bson` [V audit].
* LZ4, LZMA, Brotli: `lz4_flex`, `lzma-rs`, `brotli` [V audit; brotli-decompressor ships one C file that is
  not compiled].
* **MeshX:** a hand-written reader. One verified-by-document detail: a note in the local Resonite install
  folder (`bone_index_serialization.md`) shows MeshX bone bindings stored as four **LEB128 varints of the
  sign-extended index** (so -1 takes 10 bytes: `FF x9 01`) followed by four f32 weights. **That note was
  derived from decompiled Resonite DLLs**, which feeds the ToS question (D19).

### 19.3 Mapping to our ECS (v1) [I]

| Resonite | Primordial v1 |
|---|---|
| Slot tree (name, local position/rotation/scale, parent) | Objects + Parent/Children/Transform (presentation objects) |
| SkinnedMeshRenderer (mesh, bones, blendshape weights) | `SkinnedMesh {mesh: AssetId, bones: [ObjectId], bind_poses}`; blendshapes imported as data but not driven in v1 |
| MeshX (vertices, normals, UVs, submeshes, bones, bind poses, bindings) | Engine mesh asset (content-addressed) |
| Textures | Texture assets: PNG and JPEG decoders, pure Rust [V audit `assets-pure`]. WebP was not audited, so v1 does not decode it (review L9) |
| Materials (PBS family) | Standard PBR with albedo, normal and base color only in v1; the rest is dropped and reported |
| AvatarDescriptor / VRIK references (head, hands, feet) | `AvatarRig` component mapping humanoid bones; v1 IK = our own 3-point (head + hands) solver driving the skeleton. Whether "wear the avatar" includes this IK is asked in D19 (review L9) |

### 19.4 What does not carry over, and the fallback

| Not carried | Fallback |
|---|---|
| ProtoFlux on the avatar | Dropped. A later AI-assisted port to a Rust logic module, through MCP, is the path |
| Custom or modded components | Dropped and listed in a machine-readable import report |
| Dynamic bones | Later: presentation-only spring bones (not sim) |
| Visemes, eye tracking, blendshape drivers | Later: presentation-only drivers from audio and XR eye-gaze extensions |
| Complex or custom shaders and materials | Later: WGSL equivalents for common Resonite material families |

### 19.5 Distribution

An imported avatar becomes a content-addressed prefab: an object tree plus mesh and texture assets. Peers
fetch it by hash when the avatar appears (16.7). Avatars are presentation, except engaged hand colliders
(18.4).

---

## 20. Non-Rust boundaries (100%-Rust ruling)

Audit method: `scripts/audit_native.py` over 22 probe crates and the spike workspace, for Windows x64 and
Linux x64, counting only packages `cargo tree` says are actually built. Details are in spike-report.md
section 7. **The spike workspace itself is 100% Rust (42 packages)** [V audit].

### 20.1 Unavoidable runtime boundaries (dynamically loaded or OS-provided; nothing compiled by us)

| Boundary | Why it exists | Rust side |
|---|---|---|
| GPU drivers and graphics runtimes (D3D12, DXGI, Vulkan loader + ICDs; Metal on macOS) | Hardware access | wgpu/naga 100% Rust [V audit] |
| OS system libraries (Windows DLLs via `windows`/`windows-sys`; libc on Linux) | Sockets, threads, windowing, audio | Pure Rust bindings. The Windows import-library crate (`windows_x86_64_msvc`) ships prebuilt `.lib` stubs, not C source [V audit] |
| OpenXR loader (C++) + vendor runtime | Only way to reach headsets | `openxr` with `loaded`: loader `dlopen`ed [V audit] |
| Linux desktop: libwayland/libX11 (dlopen), ALSA (audio) | Linux windowing and audio | winit/cpal |
| The Rust compiler toolchain (rustc uses LLVM, C++) | Compiling game logic to wasm | Build tool, not linked into the engine; since D11 every peer runs it (13.2) |
| Agent harness CLIs (Claude Code, Codex CLI and Antigravity's `agy` are native vendor executables; Gemini CLI is a Node.js app) and their cloud model APIs | R13 panels | Separate local processes spoken to over stdio or a PTY; the engine-side PTY (ConPTY via `portable-pty`), terminal emulator and chat UI are Rust [V audit `agent-panels`] |
| OS speech recognition on Windows, only as the lighter fallback (D33, revised 21:51Z) | Voice input | `windows` crate bindings. The built-in speech models run on a pure-Rust runtime (21.11, D81), so they are not a boundary [I] |
| C runtimes that Rust itself links (review L1): the MSVC CRT on Windows, and MSVC `link.exe` to build; glibc on Linux (dynamic, OS-provided). Static musl builds, such as the phase-1 Linux run, embed musl's C. Official std builds may include compiler-rt objects in `compiler_builtins` | Every Rust binary | Recommend `linux-gnu`, not static musl, for Linux (D1) |
| OS containment for restricted harness sessions: AppContainer, job objects, ACLs; Linux users or namespaces | R13 inbox and decider sessions | `windows` crate bindings (21.5) |

### 20.2 Gray areas (settled 19:11Z as recommended)

* **Linux Wayland: no C is compiled** with winit's default features [V, at integration].
  * `wayland-backend` compiles its C log shim only with its `log` feature; its build script is gated on
    it [S]. In our graph that feature is off (`client_system`, `default` and `dlopen` only), and a
    Linux-target check ran its build script with no C compiler available and produced no static library
    [V]. The phase-1 audit flagged the crate from its files, not from what builds.
  * libwayland itself is loaded at runtime, a system library (20.1).
  * Rule: keep `wayland-backend/log` off; CI checks the resolved features [I].
* **Rust inline assembly.** It is Rust by the ruling's wording, but listed here (review D12-3). **Allowed**
  (settled 19:11Z, as recommended).
  `scripts/audit_native.py` now flags Rust files that use `asm!`, `global_asm!` or `naked_asm!` (a text
  scan, inspected by hand) [V `spikes/out/audit-inline-asm.md`]. Packages that use it:
  * `graviola`: 149 `asm!` blocks [S]. Only relevant if transport option B is chosen.
  * In minimal wasmtime: `wasmtime-internal-unwinder` (4 files), `cranelift-native` (1),
    `wasmtime-internal-jit-icache-coherence` (2) and `sha2` (4); on Linux also `rustix` (16).
  * In wasmi (the D12 runtime): `libm` (4).
    * On x86 it uses `sqrtss`/`sqrtsd` and a runtime-detected `vfmadd`, falling back to software FMA
      [S, inspected].
    * Both are exactly rounded under IEEE 754, so results do not depend on which path runs.
* **Shader compiler:** DXC (C++ DLL) only if HLSL is chosen (14).
* **wasmtime, if Cranelift is adopted later (D12):** with default features off it compiles no C on Windows
  [V iteration-benchmark 5]. On Linux and macOS its build script compiles `helpers.c` whatever the
  features [V reviewer round 2: the musl cross-check fails looking for a C compiler]. The user's 15:10Z
  ruling (D12) keeps wasmi everywhere until a `[patch]` removes it and fuel tests pass on all three OSes.
* **Crates that ship but do not compile C/asm files** (e.g. brotli-decompressor, qoi, rav1e without its
  `asm` feature) [V audit]. Harmless.

### 20.3 Rejected because they compile C/C++/asm [V audit]

* `ring`: quinn default, iroh, libp2p+quic, snow default via its `std` feature.
* `aws-lc-sys`: libp2p+quic.
* blake3 default build (use `pure`).
* wasmtime default build: fiber, jit-debug, ittapi, zstd-sys.
* openxr `static` (CMake C++ loader).

Decision: **D1**.

---

## 21. In-game agent panels (R13)

Players work with their **own** agent harnesses through in-game panels. The harness is a local process on
the player's machine. The engine-side panel, terminal and chat UI are Rust. The harness itself is a
separate, non-Rust or third-party process (20.1).
* **Launch harnesses: Claude Code, Codex (OpenAI's Codex CLI) and Google Antigravity** (user rulings
  21:54Z, mfa655f4d42eb, and 21:55Z), supported "in the same generic way orgtree supports these
  harnesses": one harness layer, with a driver per harness (21.1). Gemini CLI and other ACP agents can
  follow through the same layer; they are not launch harnesses.
* **The rulings of 21:50Z-22:54Z add** (their player-facing side is @gnosarium-ux's Gnosarium UX design,
  sections 8.1-8.12):
  * voice prompting with a speech model built into the engine, and spoken replies (21.11);
  * the player's gaze and pointing as prompt context (21.12), and guidance annotations (21.13);
  * game settings changed by talking to the agent (21.14);
  * open panels: people nearby see and hear prompts, replies and the agent's work (21.4);
  * confidants, friends who may use your panels as you (21.5);
  * a simplified transcript (21.2).

**Security rules for this section [I]** (they answer review findings R13-C1, R13-C2 and R13-H1..H3):
1. **One principal per restricted session.** A restricted session takes input from exactly one remote
   party (a panel connection or a hub correspondent). **The exception is an unlocked panel** (user
   ruling 19:00Z, D27a): its own session takes input from the owner, from every guest the access level
   admits and from the owner's confidants (user ruling 22:34Z), as though they were the owner. Each
   message carries signed sender attribution that the sender cannot spoof (21.5).
2. **Only the owner's sessions have built-in tools.** Every session that reads text from a remote panel or
   hub is a **restricted session**:
   * it has no built-in tools;
   * it runs under a separate OS identity;
   * it authenticates with a restricted-session API key, never the owner's login;
   * it runs under spend caps (21.5).
3. **Nothing in a world repository configures a harness.** Panel workspaces are materialized with harness
   configuration stripped (21.3), and the SCM rejects those paths (15.6).
4. **Rights come from the engine, never from the model.** The MCP server binds every call to the calling
   session's token. The agent cannot choose the principal it acts for (21.3).
5. **Unlocking ships per harness, once that harness passes the attribution and re-locking gates
   (21.9).** Restricted sessions ship per harness once it passes its containment gates.

Tags in this section: **[S]** marks a flag or text read in the installed CLI's `--help` on this machine.
**[S docs]** marks the harness's official documentation. **[S Orgtree's … page]** marks Orgtree's own
harness documentation (its harness pages, audited at its commits 32bb414 and 2ec8c0e), which describes how
Orgtree drives these CLIs; we have not re-checked it. Nothing in this section is [V].

### 21.1 Running a harness: two integration paths

**Structured mode (recommended default).** The engine spawns the harness in its machine-readable headless
or protocol mode. It renders the events as a chat UI: messages, streaming text, tool-call cards, diffs and
approvals.

| Harness (installed version) | Structured path | Isolation and restriction controls | Spend controls |
|---|---|---|---|
| Claude Code 2.1.292 (native `claude.exe`, shipped in the npm package `@anthropic-ai/claude-code`) | `claude -p --input-format stream-json --output-format stream-json --include-partial-messages`, `--mcp-config <json>`, `--resume <session-id>` [S] | `--bare`, `--restricted`, `--tools`, `--allowedTools`, `--disallowedTools`, `--setting-sources`, `--settings`, `--strict-mcp-config`, `--permission-mode`, `--permission-prompts`, `--disable-slash-commands`, `--no-session-persistence` [S]. Config home moves with `CLAUDE_CONFIG_DIR` [S docs] | `--max-budget-usd` ("only works with --print") [S]. No turn-cap flag [S] |
| Codex CLI 0.160.0 (native) | `codex exec --json` (JSONL events, `--output-schema`); `codex app-server` (experimental JSON-RPC); MCP servers via `-c mcp_servers.<name>…` [S flag; key path I] | `-s/--sandbox` (`read-only`, `workspace-write`, `danger-full-access`), `--ignore-user-config`, `--ignore-rules`, `--disable <FEATURE>`, `--ephemeral`, `--strict-config`, `-C/--cd` [S]. Config home moves with `CODEX_HOME` [S, S docs]. **`-a/--ask-for-approval` exists only on the interactive top-level command, not on `exec`, and offers only `on-request` and `never`** [S]. `exec` takes `approval_policy` through `-c` [S docs] | None [S] |
| Antigravity CLI 1.4.10 (`agy`, native) | One persistent `agy -p --input-format stream-json --output-format stream-json` process: "stream-json reads one NDJSON message per line from stdin and runs a turn for each" [S]. `--conversation <id>` resumes [S]. MCP servers through `agy mcp add` or the per-session `--settings` [S flags; keys I] | `--sandbox` ("terminal restrictions"), `--mode` (`accept-edits`, `auto`, `plan`), `--settings` ("for this session only, never saved"), `--disable-slash-commands` [S]. No equivalent of `--restricted` or `--tools` [S: none in `--help`]. Print mode cannot answer permission prompts, so Orgtree runs it with `--dangerously-skip-permissions` and denies tools through PreToolUse hooks, which "are not an OS filesystem sandbox" [S Orgtree's Antigravity page] | None [S] |
| Gemini CLI 0.57.0 (Node.js app, via npm; not a launch harness) | `--acp` (Agent Client Protocol) or `-p … -o stream-json` [S] | `--approval-mode`, `--policy`, `--allowed-mcp-server-names`, `-e/--extensions`, `-s/--sandbox` [S]. Config home moves with `GEMINI_CLI_HOME` [S docs]. Folder trust is "disabled by default", so project configuration loads from any folder [S docs] | None [S] |
| Any ACP-capable agent, later (e.g. OpenCode 2.0.28 has `opencode acp` [S]) | Agent Client Protocol over stdio; the `agent-client-protocol` crate 3.6 is Rust [V audit `agent-panels`]. Protocol semantics not yet exercised [I] | per agent | per agent |

**Flags and settings the engine never uses** [flags S; policy I]:
* **Claude Code:**
  * `--dangerously-skip-permissions`, `--allow-dangerously-skip-permissions`,
    `--permission-mode bypassPermissions`;
  * `--add-dir`, `--plugin-dir` or `--plugin-url` pointing at content that came from peers.
* **Codex:**
  * `--dangerously-bypass-hook-trust`, `--dangerously-bypass-approvals-and-sandbox`, `-s danger-full-access`;
  * in restricted sessions, also `--approve-for-me`, `-s workspace-write`, `--add-dir` and top-level
    `--search`.
* **Antigravity:** `--remote-control` ("Create a remote connection for the CLI session"); `--add-dir`
  pointing at content that came from peers; `--dangerously-skip-permissions` unless the engine's deny
  hooks are installed and checked at launch (21.3).
* **Gemini CLI:** `--skip-trust`, `-y/--yolo`, `--approval-mode yolo`, and the environment variable
  `GEMINI_CLI_TRUST_WORKSPACE=true`.

**One harness layer, a driver per harness (user ruling 21:54Z) [I].** It follows how Orgtree drives its
harnesses (Claude Code, Codex, Antigravity and others) through one runtime with a driver each [S
Orgtree's harness pages]. Every driver implements the same interface:
* **launch:** turn a launch profile (21.3, 21.5) into a command line, a clean environment, the session's
  own config home and the engine MCP server;
* **turn:** send a structured user message whose first part is the engine-written header (21.5);
* **events:** text deltas, tool calls (name, arguments, start, end, result), thinking where the harness
  exposes it, usage, and the turn's end, normalized into one stream. The transcript (21.2), the open-panel
  filter (21.4) and the spend caps (21.5) all read that stream;
* **steer:** input during a running turn where the harness allows it, otherwise at the next turn. Codex
  has native exact-turn steering; Claude Code and Antigravity take it through hooks (at PostToolUse, and
  at invocation boundaries) [S Orgtree's harness pages];
* **interrupt:** a control message (Claude Code), an exact-turn interrupt (Codex), or ending the process
  tree (Antigravity) [S Orgtree's harness pages];
* **resume:** `--resume <id>` (Claude Code), threads (Codex), `--conversation <id>` (Antigravity) [S].

**Capability flags per harness and version** (Gnosarium UX section 15, item 12), each with a plain reason
when off, so the UI can say "Sharing isn't verified yet for Codex 0.161":
* structured mode;
* unlocking verified (gates 9g, 9i, 9j and 9l, 21.9);
* restricted sessions available (that harness's containment gate);
* steering and interrupt kinds, resume;
* the models and effort levels the signed-in account offers;
* usage readout, and images in prompts.

**Launch profiles are pinned per harness version [I].** Each adapter records the harness versions its
launch profiles were verified against (21.9). On any other version:
* the owner's private panel still starts, with a warning;
* unlocking and inbox sessions stay unavailable until the profile is re-verified.
This costs ease of use after harness updates (D25). Harness flags change often (23.3, risk 10).

**PTY mode (universal fallback).** The engine runs the harness's interactive TUI under a pseudo-terminal
and renders a terminal emulator into the panel.
* PTY panels are **owner-only**. They never receive guest, panel-to-panel or hub input, and they cannot be
  unlocked, because the TUI exposes all of the harness's controls.
* **Engine side** (all audited 100% Rust [V audit `agent-panels`]):
  * `portable-pty` (ConPTY on Windows, openpty on Unix);
  * a VT emulator (`alacritty_terminal` or `vt100`);
  * `cosmic-text` for shaping and layout, with glyphs rasterized into a wgpu texture atlas.
* Decisions: **D25** (integration path), **D25a** (owner-session tool profile, 21.3).

### 21.2 Rendering and input [I]

* **A panel is a presentation-only object** (16.3):
  * a world-space quad, flat or curved, attached to the world, the player's wrist, or the desktop overlay;
  * its texture is redrawn when content changes;
  * it never enters the sim, rollback or hashes.
* **Desktop:** keyboard focus with click-to-focus, plus OS IME for text composition. The panel can also be
  shown as a screen-space overlay.
* **VR:** laser-pointer or poke interaction, an in-engine Rust virtual keyboard, and panel grab and resize
  gestures.
* **Text from other principals is sanitized before display** (rules in 21.5, "Input handling").
* **The transcript is simplified** (user ruling 22:03Z, m6b61bb507d44): consecutive tool calls collapse
  into one summary row, as in the ChatGPT and Claude web apps. Per-call detail (tool names, arguments,
  results, diffs) shows only with an advanced setting, off by default.
  * Every engine MCP tool carries a plain-language summary template ("moved 1 object", "edited 1 file"),
    and each driver maps its harness's own tools onto the same words, so a row reads "Worked: moved 1
    object, edited 1 file, ran 1 test" whatever the harness (Gnosarium UX section 15, item 9) [I].
  * Things that need the owner are never collapsed: a pending confirmation, a failed test, a rejection,
    a question from the agent [proposal, Gnosarium UX 8.2].
* **Engine-owned approval layer.** Approvals render in a layer that panel content cannot draw into:
  * connection requests;
  * commit and edit confirmations;
  * unlock and profile changes;
  * first contact with a hub recipient.

  The layer shows the requester's verified key and petname. Neither agents nor guests can imitate or
  trigger it. This is the out-of-band channel that 12.3 and 17.4 rely on.
  * The Gnosarium UX calls it **the Seal**. Its presentation is reserved: drawn last, without depth
    testing, at a reserved body-attached or screen position, with haptic and chime cues no other UI
    uses [proposal, Gnosarium UX section 15, item 6].
* **Voice:** prompting by voice with a speech model built into the engine, and spoken replies (21.11;
  D33, D77, D81, D85).

### 21.3 Wiring the harness to the engine's MCP server, and the panel workspace

**Sessions and MCP binding [I]:**
1. **Sessions.** A panel runs one **private session** for its owner; when the panel is unlocked, guests
   write into that same session (21.5). It also runs restricted sessions for other principals:
   * one per inbound connection or hub correspondent (21.6, 21.7);
   * a fresh decider session per request for "ask agent" (21.6, 21.10).
2. **Keys and certificates.**
   * The private session holds the **panel key**. The owner's user key signs its delegation certificate
     when the owner creates the panel. That is a human action (12.3).
   * A restricted session that acts in the world gets its own key. Its **sub-certificate** is signed by
     the panel key. The sub-certificate is narrower than the panel's scope, expires, and names the
     principal it serves (12.3).
3. **Tokens.** Each session gets its own MCP token: 256 random bits, created at launch, never reused, and
   invalidated when the session ends or is revoked.
4. **Transport: stdio.** The harness launches `pe-app mcp` as its MCP server.
   * `pe-app mcp` connects to the engine over a local named pipe (a Unix socket on Linux) and presents the
     token.
   * The pipe's ACL admits only that session's OS identity.
   * The token reaches the harness in the MCP config file in the session's own config directory. Only
     that identity can read the file.
   * Localhost HTTP is a fallback only, under the rules in 17.1.
5. **Rights binding (R13-H3).** The engine maps token → session → principal → certificate. It checks every
   tool call against that chain and signs with that session's key. No tool input carries a principal,
   request ID or "on behalf of" field that the engine trusts.
6. **Same checks as human edits.** Everything an agent does to the game goes through MCP tools (17.2). It
   is signed with the session's key and passes the same pipeline as any human edit (17.4):
   * ACL checks, frame stamping and soonest-wins ordering;
   * rollback;
   * SCM ordering, rejections and merge seats.
7. **Wiring per harness:**
   * Claude Code: `--mcp-config <file> --strict-mcp-config` [S].
   * Codex: `-c mcp_servers.primordial.command=…` [S flag; exact key path I].
   * Antigravity: a server entry in the session's own `--settings` file, or `agy mcp add` in the session's
     own config home [S flags; which of the two keeps sessions apart is I].
   * Gemini CLI: `--allowed-mcp-server-names primordial` [S], plus a server entry in the session's own
     `GEMINI_CLI_HOME` settings [I].

**The panel workspace (R13-C1) [I]:**
The private session's working directory holds game source for the agent's file tools. It is **never a raw
checkout of the world repository**, for three reasons:
* Structured mode runs Claude Code with `-p`, and "The workspace trust dialog is skipped when Claude is run
  in non-interactive mode" [S].
* The docs add: "Without `--bare`, a `-p` session runs the hooks in a project's `.claude/settings.json` and
  connects the servers in its `.mcp.json`, even in a folder you've never trusted" [S docs].
* Any builder can commit to the world repository (12.2). Through a raw checkout, a builder's hook would
  run as every panel owner, under the owner's OS account.

The workspace is built as follows:
* **Materialized, not checked out.** The engine writes the files of the current head into a fresh
  directory under its own data directory (e.g. `%LOCALAPPDATA%\Primordial\panels\<panel>\ws`).
  * Reserved paths (15.6) are skipped, whatever the commit's age.
  * Only regular files are written: no symlinks, no `.git` directory, no executable bits.
  * The materializer refuses to write through any directory it did not create. That blocks short-name
    and alias tricks against a `.claude` directory that a harness creates at run time.
* **Ancestor check.** Before every launch, the engine checks each ancestor directory of the workspace for
  reserved names. If it finds one, it refuses to launch and tells the owner why. The reason: Claude Code
  loads `CLAUDE.md` from "every directory above" the working directory, and Gemini CLI loads `GEMINI.md`
  and `.env` from parent directories [S docs].
* **Reserved names per harness** (their union is the 15.6 list):

| Harness | Project-level files it loads | Source |
|---|---|---|
| Claude Code | `.claude/` (settings with hooks, an `env` block and helper commands; rules, skills, commands, subagents, output styles, workflows, agent memory); `.mcp.json` (under `-p`, "Connected without asking, approved or not"); `CLAUDE.md` and `CLAUDE.local.md` (working directory, every ancestor, and subdirectories on demand); `AGENTS.md` (any directory, read when no `CLAUDE.md` exists, v2.1.277+); `.worktreeinclude` | [S docs] |
| Claude Code plugins | `.claude-plugin/`, `.lsp.json` | [I] (names in the binary) |
| Codex | `.codex/` (`config.toml` loaded "from the project root to your current working directory", `hooks.json`, rules, agents, skills); `.agents/` (skills scanned "in every directory from your current working directory up to the repository root"); `AGENTS.md` and `AGENTS.override.md` (project root down to the working directory; the docs mention no trust gate) | [S docs]; `.agents/plugins` [I] |
| Antigravity | `.agents/` (rules in `.agents/rules/`, skills); `AGENTS.md` and `GEMINI.md`, "always active for their directory scope"; MCP config under its home (`~/.gemini/antigravity-cli/`) | [S Orgtree's Antigravity page, citing Antigravity 1.3.5's Workspace Rules docs]; ancestors: unknown [I] |
| Gemini CLI | `.gemini/` (`settings.json` with MCP servers and hooks, `.env`, commands, skills, agents, policies, `system.md`, sandbox profiles); `.agents/` (skills alias); `GEMINI.md` (ancestors up to the `.git` root or home, and up to 200 subdirectories); `.env` (working directory and parents); `.geminiignore` | [S docs] (bundled with 0.57.0) |
| OpenCode | `.opencode/`, `opencode.json`, `opencode.jsonc`, `AGENTS.md` | [I] (names in the binary; docs not read) |
| git, which harnesses run | `.git` (a planted one can set `core.fsmonitor`, which runs a command on `git status`) | [I] |

* **Compilation stays in the engine.** The private agent edits files, then compiles with `logic.check` and
  `logic.test_in_fork` through MCP. The default private profile (below) has no shell, so the agent cannot
  run `cargo` on another builder's `build.rs` or proc macros. Build-time code is an open issue for 13.2
  (15.6).
* **Repository text is untrusted.** Source, comments, commit messages and object names written by other
  peers reach the private agent through its file tools and MCP results. 17.2 marks peer-authored strings
  in tool results. Files cannot be marked, so the default profile limits what an injected instruction can
  do.

**Private-session launch profile for Claude Code (the owner's own session) [flags S; profile I]:**

```text
cwd   <panel>\ws        materialized, reserved paths stripped, ancestors checked
claude -p --input-format stream-json --output-format stream-json --include-partial-messages
       --restricted --tools "Read,Edit,Write,Glob,Grep"
       --permission-mode acceptEdits
       --strict-mcp-config --mcp-config <panel>\owner\mcp.json
       --allowedTools "mcp__primordial__*"
       --resume <session-id>
```

* **`--restricted`** "removes the built-in tools that run commands or code … and WebFetch unless --tools
  names them, and ignores user, project and local settings files". It also "confines the file tools to the
  working directories" [S]. So project hooks, `env` blocks and `.mcp.json` never load, even if a reserved
  file slipped through.
* **Auth** is the owner's own login. `--bare` is not used here, because with it "Anthropic auth is strictly
  ANTHROPIC_API_KEY or apiKeyHelper via --settings (OAuth and keychain are never read)" [S]. That would
  force API billing on the owner.
* **The full profile (D25a)** adds shell and web. It uses `--setting-sources user --tools default` in place
  of `--restricted --tools …`.
  * It still excludes project and local settings: "Pass `--setting-sources user` … so Claude Code reads
    neither the project's settings files nor its `.mcp.json`" [S docs].
  * The owner confirms the switch in the approval layer, and the panel shows which profile is active.
* **Codex, Antigravity and Gemini private sessions** [I; flags S]:
  * Codex: `codex exec --json --skip-git-repo-check -C <panel>\ws -c project_root_markers=[]`.
  * Antigravity: `agy -p --input-format stream-json --output-format stream-json --disable-slash-commands
    --settings <panel>\owner\agy.json --conversation <id>`.
    * Its print mode cannot answer permission prompts, so Orgtree's adapter adds
      `--dangerously-skip-permissions` and denies every tool outside the profile with PreToolUse hooks
      [S Orgtree's Antigravity page]. Ours would do the same for the restricted profile (no terminal, web
      or subagents) and refuse to launch if the hooks are not in place.
    * Hooks are a check, not containment, so gate 9k tests them (21.9). Whether `--mode accept-edits`
      with an allow-list in `--settings` denies the rest in print mode, which would avoid the skip flag,
      is for gate 9k to find out [I].
  * Gemini CLI: `gemini --acp`.
  * None of them has an equivalent of `--restricted`. The stripped workspace and the ancestor check are
    their protection, and their sandbox and approval settings are the owner's choice.
  * Never `--dangerously-bypass-hook-trust`.

### 21.4 Locality, credentials and visibility

* **Harnesses run only on their owner's machine,** and only when the owner opens a panel or unlocks one.
  Nothing ships a harness across peers. [I, by design]
* **The owner's own harness login or API key is used only by the owner's private session.** The engine
  never reads, stores or transmits it.
* **Restricted sessions (inbox and decider sessions) never use the owner's credentials.**
  * They authenticate with a **restricted-session API key**. The owner creates it for this purpose,
    ideally in a provider workspace or project with a hard spend limit (21.5).
  * An unlocked panel is different: guests use the owner's own harness, login and AI capacity (user
    ruling 19:00Z, D34).
  * The engine stores that key encrypted, for the restricted-session OS identity only, and never sends it
    to peers.
  * Claude Code keeps "your login credentials" in `.credentials.json` in its config home [S docs].
  * On this machine, `%USERPROFILE%\.claude\.credentials.json` and `%USERPROFILE%\.codex\auth.json` both
    exist (checked by name only). Any process running as the owner can read them. That is why restricted
    sessions run under another OS identity (21.5).
* **Panels are open by default** (the user's principle at 22:28Z, m7993557894dc: "you hear all prompts
  made by those around you, hear their agents responses, see their guidlines drawn in the world, and can
  watch them work in real time"). This reverses D26's 19:11Z default. The members of the panel's world
  see and hear:
  * the prompts, typed or spoken, and the agent's replies as they stream, spoken aloud nearby (21.11);
  * the collapsed tool summaries (21.2) and the outcome of each confirmation;
  * the agent's work in the world: the objects the running turn touches and the current step [I], and
    threads, captions and ghost previews as the Gnosarium UX draws them [proposal, Gnosarium UX 8.4];
  * the guidance annotations attached to prompts (21.13) and the outline of the context a prompt sent
    (21.12);
  * the panel object, its owner, its "agent working" state, and the signed game edits it makes,
    attributable to the session key, the owner and, for a guest's or confidant's turn, the sender (12.3).
* **What stays closed by default** (Q19, user 22:34Z, m5d978da3bf64: "keep all closed by default"):
  * the right to act with the owner's authority: panels start Locked (D27a), and confidants are the one
    standing exception (21.5);
  * the approval layer's contents, until confirmed;
  * credentials and keys: harness logins, API keys, tokens and identity secrets;
  * what other people's agents push into yours: panel inboxes and the connection policy (D30, D30a);
  * direct messages;
  * eye tracking, which stays off (Q8, 21.12).
* **Everything else streams, raw tool output included** (D86 = (b), user 08:31Z): an open panel shows
  others what its owner sees: the agent's prose and the summary rows, and also the detailed tool view,
  raw tool results and arguments, file contents, shell output and diffs. Only what the list above keeps
  closed stays on the owner's peer.
  * A best-effort secret filter [proposal; it narrows D86 (b), so it is the user's call]: text that
    leaves, and text spoken aloud, first passes a filter that masks key-like and token-like strings and
    long high-entropy runs. It would be labelled best-effort, since it cannot catch everything an agent
    writes.
* **Private panel** (Q20, ruled as recommended at 22:54Z, "for now"): a per-panel switch, off by default.
  It hides the transcript, the prompt echo and the step caption from others, and spoken replies go only to
  the person who prompted. Others still see the panel, its working state and its signed edits.
* **How it travels** [I]: as non-sim presentation messages over the session transport, like pose streams
  (16.3), never in the sim, rollback or hashes. They go to the members of the panel's world (D31),
  encrypted to them; across worlds, a panel is visible where it is, like anything else (22.9).
* **Who sees which transcript in full:** the owner sees every session of the panel; a guest or confidant
  sees the conversation as the world sees it, plus their own turns; a remote party sees only the messages
  on its own connection.
* Defaults and options: **D26** (open by default since 22:28Z), **D86** (ruled (b) at 08:31Z).

### 21.5 Unlocking a panel for others (user ruling 19:00Z)

**Access levels** (per panel, owner-controlled):

| Level | Who may send input |
|---|---|
| Locked (default) | Owner only |
| Friends | Peers whose keys are on the owner's friend list (below) |
| Anyone in session | Any current session member |

**What unlocking means (user ruling 19:00Z, D27a and D34):** "unlocking your prompt panel gives
unconditional access to it as though they were you, but informs the agent who is sending".
* A guest's message goes into **the panel's own session**: the owner's agent, harness, tool profile,
  login and AI capacity, with the owner's rights. There is no separate guest session, sandbox, key, spend
  limit or approval step.
* **The agent always knows who is speaking,** and a guest cannot pretend to be someone else (below).
* **Locked stays the default** (D27a); Friends and Anyone in session are explicit choices per panel.
* **Stated risk, accepted by the user:** a guest can make the agent do anything the owner's panel can do:
  prompt injection with the owner's rights, which includes reading what the panel's tools can read,
  spending the owner's AI capacity, and game edits with the owner's authority. What bounds it: Locked by
  default, the Friends level, the panel's tool profile (the restricted profile by default, D25a), sender
  attribution, and re-locking at any time. Personal subscription terms may not allow sharing; that is the
  owner's responsibility.
* **Guests cannot change the owner's settings** (Q12, user 22:28Z: "guests cant change your local
  settings, they can only use your panel to influence the world around them"). Settings tools refuse in a
  guest's turn (21.14).
* **A guest's turn carries the guest's own gaze and pointing,** never the owner's (21.12, D82).

**Confidants (user ruling 22:34Z, m5d978da3bf64):** "some friends can be marked as "confidants", who then
get full access to your prompts as though they were you. the responding agent knows theyre a confidant of
yours when they prompt it."
* A confidant is a friend (D29) whom the owner marks. It is a **standing, per-person unlock**: the
  confidant may prompt every panel of the owner, Locked ones included, with the owner's rights, harness,
  login and AI capacity, like an unlocked panel's guest, without the panel being opened to anyone else.
* **The mark is a signed, revocable attestation** [I]: the owner's user key signs (confidant, the friend's
  key, issued, expires), and the record syncs across the owner's devices like the friend list. The
  owner's peer checks it when the confidant's signed panel input arrives. The same attestation is the "is
  a confidant of" relation that join rules read (12.4), presented by the joiner. Relationship records
  expire after about 30 days unless re-signed, and are renewed automatically while the owner is online
  (O5, ruled as recommended at 22:54Z, "for now").
* **Revoking** stops accepting the confidant's input at once and interrupts a turn of theirs in flight,
  as re-locking does. For join rules, a signed revocation supersedes the attestation [I].
* **The turn header says so:** "<petname> ✓ verified (confidant of <owner>)", and `panel.turn.sender`
  carries the same standing [I].
* **Settings:** the Q12 refusal (21.14) does not apply to confidants: "as though they were you" includes
  the owner's settings [the ruling's wording; stated explicitly in the Gnosarium UX].
* **Confirmations stay the owner's** (Q22, ruled as recommended at 22:54Z, "for now"): commits, unlocks,
  connections and kicks requested in a confidant's turn wait in the owner's approval layer. A confidant
  never confirms them.
* **Only while the owner's harness runs** [I]: a confidant can prompt only while the owner is online
  with the panel open, from inside the panel's world, because the harness runs only on the owner's
  machine (21.4). This follows from where the harness runs, not from a rule of its own.
* **Profile for confidants (D89 = (b), user 08:31Z):** a confidant's turns get the panel's own profile,
  shell and web included on a Full panel, as though they were the owner. There is no "Restricted for
  confidants" setting; the Seal keeps its plain warning (Gnosarium UX 8.5a).

**Mechanics [I]:**
* A guest types into the owner's panel in-world. The guest's peer sends **signed panel-input messages**
  over the session transport's control channel, not the sim, signed by the guest's session key.
* The owner's peer verifies the signature and checks the sender's key against the access level or the
  owner's confidant attestations, then queues the message as a turn of the panel's session. Turns from
  several people are taken in arrival order; the panel shows whose turn is running.
* **Sender attribution that cannot be spoofed:**
  * the adapter delivers every turn, the owner's included, as a structured user message
    (Claude Code `--input-format stream-json`, ACP prompt content, Codex app-server input) whose first
    part is an **engine-written header**: the sender's petname, key fingerprint and "verified", plus a
    per-session secret marker. The agent's system prompt says that only headers carrying that marker are
    genuine;
  * the sender's text follows in a separate part, sanitized as below, so a line that imitates a header is
    escaped, and a guest never learns the marker;
  * the engine MCP server also exposes the verified sender of the running turn (`panel.turn.sender`) in a
    JSON field no participant can write;
  * the engine binds the turn to its sender, so every action in that turn records `requested_by` = the
    verified sender (12.3, 17.4).
* **PTY panels are never unlockable:** a terminal stream has no field the guest cannot write, so
  attribution could be forged.
* **Re-locking** (or lowering the level) stops accepting input from guests no longer admitted at once,
  drops their queued turns, interrupts a guest's turn in flight, and closes their view streams. Guests
  hold no keys or sessions of their own, so nothing else needs revoking. Game edits already made stand,
  under the owner's authority; the owner reverts them with ordinary commits if needed.

**Containment of restricted sessions (inbox and decider sessions, 21.6) [I].** Since the 19:00Z ruling,
guests at an unlocked panel are not restricted sessions; everything below applies to inbox and decider
sessions:
* **No built-in tools.** The engine MCP server is the only tool source, with the scope of that session.
* **Separate OS identity** with no access to the owner's profile:
  * Windows: an AppContainer profile per panel (no admin rights needed), or a dedicated low-privilege
    local user (needs admin rights once, to create it).
  * Linux: a separate user, or a user namespace with its own home.
  * Each session's files live in `<data>\guests\<panel>\<session>\`: `cfg\`, an empty `ws\`, and `tmp\`.
    They are ACL'd to that identity and the engine.
  * Open question for gate 9c: the harness binaries currently live inside the owner's profile
    (`%APPDATA%\npm\…`, `%USERPROFILE%\.codex\packages\…`). The other identity needs read access to the
    binaries, but not to the credential files beside them.
* **Separate config home.** It holds only engine-written settings and the restricted-session API key,
  never the owner's login.
* **Clean environment.** The engine builds the environment from scratch: config home, key, temp
  directory and a minimal `PATH`. It never inherits the owner's environment.
* **Resource limits.** A job object (Windows) or cgroup (Linux) limits CPU, memory and process count. The
  spend caps are below.
* **Fail closed at launch.** Claude Code's `system/init` event "reports session metadata including the
  model, tools, MCP servers, and loaded plugins" [S docs]. The adapter kills the session if that event
  lists any of:
  * a tool outside the engine server;
  * any other MCP server;
  * any plugin.

**Claude Code restricted-session launch [flags S; this combination is untested: gate 9a]:**

```text
identity  per-panel AppContainer, or a low-privilege local user
env       CLAUDE_CONFIG_DIR=<session>\cfg   ANTHROPIC_API_KEY=<restricted-session key>   TEMP=TMP=<session>\tmp
cwd       <session>\ws   (empty)
claude -p --bare --restricted --tools ""
       --strict-mcp-config --mcp-config <session>\cfg\mcp.json
       --settings <session>\cfg\settings.json          (contains {"disableAllHooks": true})
       --allowedTools "mcp__primordial__*"
       --permission-mode dontAsk --permission-prompts none
       --disable-slash-commands
       --max-budget-usd <session budget>
       --system-prompt <engine-written prompt>
       --input-format stream-json --output-format stream-json --include-partial-messages
```

* **`--tools ""`** disables "all tools" of "the built-in set" [S]. Without it, `dontAsk` would still allow
  "File reads in your working directories" [S docs].
* **`--bare`** skips hooks, plugins, auto-memory and `CLAUDE.md` discovery, and "OAuth and keychain are
  never read" [S]. Without the restricted-session key, the session fails to authenticate. It never falls
  back to the owner's login.
* **`--bare` alone is not enough:** "The project's `env` block and helpers such as `awsAuthRefresh` in its
  settings files still apply" [S docs]. `--restricted` ignores user, project and local settings files
  [S], and the workspace is empty anyway.
* **`--permission-prompts none`:** "anything that would prompt is denied automatically" [S].
* **`CLAUDE_CONFIG_DIR`** moves every `~/.claude` path, `.claude.json` included, into the session's
  directory [S docs].
* **`disableAllHooks`** is the documented setting for turning hooks off for a run [S docs].
* **Resuming** an idle restricted session adds `--resume <session-id>` [S]. The transcript stays in the
  session's `cfg\`. If the owner chooses not to keep those transcripts, the adapter passes
  `--no-session-persistence` [S] instead, and an idle session cannot be resumed.

**Codex: no restricted sessions until gate 9b passes (R13-H1).** Unlocking a Codex panel needs only the
attribution gates (21.9), because guests act as the owner. Corrections to the first draft:
* **`codex exec` has no `-a/--ask-for-approval` option.** The top-level `-a` offers only two values
  [S]:
  * `on-request`: "The model decides when to ask the user for approval";
  * `never`: "Never ask for user approval Execution failures are immediately returned to the model".

  No policy that "never auto-runs" exists, and a headless session has nobody to ask anyway.
* **`--sandbox read-only` does not confine reads, as far as the evidence goes.** It selects "the sandbox
  policy to use when executing model-generated shell commands" [S], and the official docs do not say it
  confines reads [S docs]. Unless reads are confined, the remote party behind an inbox session can ask
  the agent to print `%USERPROFILE%\.codex\auth.json` (the file exists on this machine) and get it back.
* **Codex has no spend or turn limit option** [S].
* **Candidate profile for gate 9b to test** [flags S; effect untested]. It runs under a separate identity,
  with `CODEX_HOME=<session>\codex` holding only the restricted-session key:

```text
codex exec --json --ephemeral --skip-git-repo-check --strict-config --ignore-user-config --ignore-rules
      -s read-only -c approval_policy="never" -c project_root_markers=[]
      -c shell_environment_policy.inherit="none"
      --disable shell_tool --disable unified_exec --disable browser_use --disable computer_use
      --disable apps --disable plugins --disable hooks --disable multi_agent --disable image_generation
      -c mcp_servers.primordial.command=<pe-app> -C <session>\ws
```

**Gemini CLI: no restricted sessions until gate 9d passes.** The candidate profile:
* Identity and keys: a separate identity, `GEMINI_CLI_HOME=<session>\gemini` [S docs], and a
  restricted-session `GEMINI_API_KEY`.
* Mode: `--acp`, or `-p … -o stream-json` [S].
* Approvals: `--approval-mode default`. In non-interactive mode a policy's `ask_user` "is treated as
  `deny`" [S docs].
* Tools: `--policy <file>` that denies built-in tools [I], and `--allowed-mcp-server-names primordial`
  [S].

**PTY panels** are never unlockable (above). An ACP agent becomes unlockable through its own adapter once
it passes the attribution gates.

**Input handling (R13-L1) [I]:**
* Text from anyone other than the owner enters as the content of a structured user message:
  * Claude Code: `--input-format stream-json` [S];
  * ACP agents: prompt content.

  A restricted session has one principal, so it needs no sender header. An unlocked panel's turns carry
  the engine-written header above, and the engine trusts no header written in the text.
* **No engine events as user text.** The engine never injects its own events as user text into a session
  that a non-owner writes to.
  * Engine events reach agents as MCP resources or tool results, in JSON fields that the guest cannot
    write.
  * For a harness without notification support, the adapter injects a fixed template with IDs and counts
    only, never peer text. Example: "1 new connection request; call `panel.requests.list`".
* **Sanitizing.** Text from any non-owner principal is cleaned before it reaches a harness or a display:
  * C0 and C1 control characters are stripped, except newline and tab, along with ANSI/VT escape
    sequences;
  * so are bidi controls (U+202A–U+202E, U+2066–U+2069, U+200E, U+200F, U+061C), zero-width characters
    (U+200B–U+200D, U+2060, U+FEFF) and Unicode tag characters (U+E0000–U+E007F), which can hide text;
  * lines that imitate engine markers (starting with `[engine`, `[owner`, `[guest`, `[panel` or `[hub`)
    are escaped;
  * length is capped.
* **Limits.** Sanitizing stops spoofing of markers and UI. It does not stop prompt injection. Containment
  does that for restricted sessions; at an unlocked panel the user accepted it (above).

**Spend caps for restricted sessions (R13-M1).** An unlocked panel spends the owner's capacity (D34
ruling); the owner may set the adapter caps below for it, off by default.
* **Provider-side limit (the real bound) [S docs]:** the restricted-session key belongs to a provider
  workspace or project with a monthly hard limit.
  * Anthropic: "A Claude Console organization sets `organization` and `workspace` limits, which are
    monthly". The quoted page is about setting limits through the API, which is in early-access preview.
  * OpenAI: "Reaching a project hard limit returns a `429` error", and "Enforcement is not instantaneous".
  * Google: not checked.
* **Harness-side:**
  * Claude Code's `--max-budget-usd` caps one restricted process: "Maximum dollar amount to spend on API
    calls (only works with --print)" [S]. Restricted sessions always run with `-p` and an API key, so the
    cap applies. Its figures are client-side estimates [S docs].
  * Codex and Gemini CLI have no such flag [S].
* **Adapter-side, all harnesses [I]:**
  * per turn: at most T tool calls and K output tokens. Tool calls are counted exactly at the engine MCP
    server, which is the only tool server. Tokens come from streamed usage; the adapter interrupts the turn
    at the limit;
  * per correspondent: at most R turns per hour, and a cost ledger (`total_cost_usd` for Claude Code
    [S docs], token counts times a price table for the others);
  * per panel: a total per hour;
  * illustrative defaults: T = 30, K = 8,000, R = 20.
* **Turn caps alone do not bound spend,** because one agentic turn can run long. The per-turn tool-call and
  token caps bound a turn.
* **Visibility.** The owner sees usage per correspondent and per guest, the caps and the remaining
  provider budget.

**Friends without a central server [I]:**
* A friend is an Ed25519 public key the owner explicitly trusts. It is added by an in-session "add friend"
  handshake, through an invite ticket, or by comparing a key fingerprint or QR code.
* Friend lists are stored locally with petnames, and synced across the owner's own devices as signed
  records. Mutual friendship means each key is on the other's list.
* Optionally, a friend key can be cross-linked to a mail-hub person identity (21.7). That is a convenience,
  not an authority.

**Who may make the resulting game edits (authority, section 12) [I]:**
* A guest's turn acts with the owner's rights, signed by the panel's session key on the owner's behalf
  (user ruling 19:00Z). Commits record `author` = the session key, `on_behalf_of` = the owner and
  `requested_by` = the verified guest.
* World rules that bind the owner's own agent bind the guest's turns too, such as `confirm` for agent
  commits in shared worlds (D28a).
* Decisions: **D27a** and **D34** (settled by the ruling), **D29**.

### 21.6 Panel-to-panel connections (agent-to-agent, in-session and across sessions)

* **There is no implicit broadcast.**
  * Panels communicate only over an **explicitly established connection**.
  * Everything below rides the session's P2P transport (control channel), never the sim, and is not
    rolled back.
* **Eligibility ("adjacent"):** recommended to mean **session membership**. Any panel in the same session
  may *request* a connection.
  * Spatial proximity is a discovery and UX filter ("nearby panels", point to connect in VR), not a
    security boundary, because the requester controls its own position.
  * Options: **D31**.
* Panels publish a small presence record: panel ID, owner, display name, and whether it accepts requests.

**Across sessions: the engine's own P2P network (D32, user ruling 19:20Z) [I]:**
* All in-game messaging between players and agents, in a session or across sessions, uses the engine's
  P2P network. **There is no built-in or central hub.**
* **Addressing** is by player key and panel key (Ed25519), not by session.
* **Discovery** uses D9 and D35: a player publishes a signed reachability record (direct endpoints or
  volunteer relays) under its key in the Mainline DHT; friends also learn it by peer exchange and from
  invites.
* **Transport:** end-to-end encrypted (Noise, 10.2), directly or through volunteer relays, which see only
  encrypted traffic, sizes and timing.
* **Offline delivery:** volunteer relays hold encrypted mail for a recipient's key for a bounded time (for
  example 7 days) and within a per-sender quota; the recipient fetches it when it comes online. A relay
  cannot read or forge it, but can drop it.
* **The same rules as in a session:** a cross-session request goes through the same handshake, connection
  policy (allow all, ask agent, ask user, deny all), inbox routing and capability ceiling. Only eligibility
  differs: anyone who knows a panel's address may request, and the policy decides.

**Where inbound traffic goes (R13-C2) [I]:**
The connection policy (allow all, ask agent, ask user, deny all) controls admission only. It does not
decide which session reads the messages. **Inbound panel messages never reach the owner's private
session,** which holds the owner's tools and credentials. Provenance labels alone would be the prompt-level
defence that 21.5 rejects.
* **Inbox session (default handler).** Each established connection gets its own restricted session,
  contained as 21.5 describes.
  * It may reply on its own connection.
  * It may run read-only world queries, if the connection was granted them.
  * It may post a summary card to the owner.
  * It cannot open connections, send on other connections, use the hub bridge (21.7), or commit.
* **Manual inbox.** The owner can choose no agent. Messages then appear in the panel as sanitized,
  untrusted text (21.5).
* **Into the private session only per message.** The owner can forward a single message into the private
  session. It enters as an MCP tool result, marked untrusted and attributed.
  * There is no auto-forward (review NEW-9, 21.10). Each forward is one message, and it goes only into a
    restricted-profile turn (D25a), without shell or web tools.
* **Capability ceiling.** Each panel has an owner-set ceiling on what any connection may be granted.
  * Default: **text only**.
  * Above it, in order: structured JSON; asset references; world actions (rights = owner ∩ remote owner ∩
    connection scope).
  * No connection is ever granted more than the ceiling.

**Worm risk.** A message that tells its reader to forward itself to every connection is a worm.
* Under the first draft of this section, an agent with full tools that relayed panel messages could spread
  such a message through a session in seconds, and across sessions over cross-session connections.
* This design cuts the propagation path:
  * only the private session can open connections or send to arbitrary connections;
  * the private session receives inbound text only through a per-message owner action, or an opted-in
    auto-forward while it has no shell or web tools;
  * inbox sessions can reply only to their own sender.
* **Residual risk:**
  * an owner who forwards a message by hand can carry a payload one hop further;
  * an inbox session can be talked into putting text in its reply.

  Per-connection rate limits (messages per minute, message size) bound the volume.

**Handshake [I]:**
1. **Request.** A → B: `ConnectRequest`, signed by A's panel key, with these fields:
   * `from_panel`, `to_panel`;
   * `purpose` (≤280 characters);
   * `requested_capabilities` (text | structured JSON | asset references | world actions);
   * `nonce`, `expiry`;
   * A's ephemeral X25519 key.
2. **Gatekeeping on B's peer:** verify the signature, session membership, eligibility and a per-requester
   rate limit. Then apply B's **connection policy**:
   * **allow all:** accept, with capabilities capped at B's ceiling.
   * **deny all:** reject with `{code: "policy_deny"}`, with no prompt and no agent involved.
   * **ask user:** show the owning player a prompt in the approval layer (21.2), with the requester, its
     owner and the sanitized purpose. The owner accepts, rejects, or lets it time out after 60 s
     (`user_declined` / `timeout`).
   * **ask agent:** hand the decision to B's decider session (below), with a timeout (`agent_timeout`).
3. **Accept.** B → A: `ConnectAccept`, signed, with these fields:
   * `connection_id`;
   * B's ephemeral X25519 key;
   * limits (message rate, maximum size, idle TTL);
   * the granted capabilities, at most B's ceiling.

   A replies `ConnectConfirm`. Both derive per-connection keys (X25519 ECDH plus nonces). Messages stay
   end-to-end encrypted even when relayed through other peers (10.5).

**"Ask agent" [I]:**
* The decision is made by a **decider session**. It is a restricted session (21.5), and its only tools
  are `panel.requests.list`, `panel.requests.accept(id, capabilities)` and
  `panel.requests.reject(id, reason)`.
* **Each decider is scoped to one request** (review R3-15). It holds a token naming that request; `list`
  shows only it, and `accept` or `reject` of any other id is refused. At most two decider sessions run
  per panel at once; further requests queue, and a request still queued at its timeout is denied [I].
* It can grant at most the owner's ceiling (default: text only). The MCP server enforces this.
* The requester's purpose text reaches only the decider, inside a JSON field marked untrusted. The private
  session sees counts and IDs only.
* The engine MCP server also exposes the resource `primordial://panel/connection-requests`, which notifies
  on new requests. For a harness without notification support, the adapter wakes the decider with a fixed
  template, never with peer text (21.5).

**Lifecycle and teardown [I]:**
* **States:** `requested → established → closed`.
* **Messages** are sequenced, acknowledged and delivered in order. They go to the connection's inbox
  session or manual inbox, sanitized. Agents use these tools:
  * `panel.connect(to, purpose)`, from the private session only;
  * `panel.send(conn, payload)`: the private session may use any of its connections, an inbox session
    only its own;
  * `panel.inbox`: the private session gets only owner-forwarded items;
  * `panel.close(conn, reason)`.
* **Teardown triggers:**
  * either side closes;
  * the idle TTL (default 30 min) expires;
  * either owner revokes;
  * the policy changes to *deny all* (existing connections are dropped);
  * the panel closes, a peer leaves, or the session ends.

  A `Close {reason}` is sent when possible. Silence past the TTL closes the connection anyway.
* **Closing a connection** invalidates its inbox session's token and kills the session, as in revocation
  steps 2–3 (21.5).
* **Connections are ephemeral** to a session. Reconnecting after a drop re-runs the handshake, optionally
  resuming within 5 min with a resumption token.
* Decisions: **D30**, **D30a**, **D30b**.

### 21.7 The Orgtree hub bridge (optional add-on) and the hubchat-style panel

**Settled (D32, user 19:20Z): the hub is an optional add-on, only a bridge to Orgtree orgs and Hubchat
users,** through the hubchat-style panel, with a separate hub identity whose secret agents never see.
In-game messaging never depends on it (21.6).

**Refined (user 19:21Z): "players automatically have a minted hubchat identity, and each agent panel also
gets one. a primordial engine agent panel is a fourth type of mailhub client, with a purple hue."**
* Every player gets a hub person identity at first launch, and every agent panel gets its own, separate
  from its owner's. Agents never see any identity secret; in-game messaging still runs over P2P.
* **Minted locally [I].** At first launch the engine generates the player's hub keypair and ownership
  secret and derives the identity's address from the public key; each new panel gets its own the same
  way. Registration at a hub waits until the player adds one, because none is built in (D32). If
  Orgtree's address scheme does not allow self-derived addresses, the address is assigned at first
  registration and the local key is what proves ownership (the scheme is unverified, below).
  * So an automatic identity reaches Hubchat users only once the player adds a hub. Shipping a default
    hub would make it work out of the box, but that hub would be a central point: **D78**.
* **A panel's identity is bound to its owner [I]** by an attestation the owner's hub identity signs:
  `{panel address, owner address, panel name, issued}`, countersigned by the panel identity. It is part of
  the panel's hub profile, so recipients verify it and see "agent panel of <owner>". Retiring a panel
  publishes an owner-signed revocation.
* **Interface note for Orgtree (a change on the Orgtree side, not implemented here) [I]:**
  * registration carries a client type with a new, fourth value for engine panels, beside the existing
    types (their names are unverified);
  * a panel's profile carries the owner attestation, so the hub can show it and its directory can list
    panels under their owner;
  * Hubchat renders that client type with a purple hue and an "agent panel of <owner>" line, verified from
    the attestation, and marks an attestation it cannot verify as unverified;
  * mail from a panel identity is marked as agent-sent, so recipients can tell agents from people;
  * nothing changes for org or person clients.

**What is known about Orgtree's hub** [S Orgtree's public docs; nothing was connected or registered]:
* A hub is an HTTP service.
  * The main port 7370 serves an operator page with no login.
  * An optional **relay-only door** (port 7371) carries authenticated client traffic.
  * Clients probe 7370, 7378, 7371 and HTTPS.
* The hub provides no TLS itself. Use a private network (e.g. Tailscale) or a TLS proxy.
* Identities:
  * **person** identities (Hubchat users) and **organization** identities;
  * an org identity has a permanent public address, minted with an ownership secret that proves
    ownership, and the hub stores fingerprints;
  * devices link to carry a person identity.
* Clients **register, poll and transfer files** over HTTP, with at-least-once custody.
* Hubchat finds people and orgs through a directory. It supports question messages with preset options,
  and file attachments.

**Not verified [I]:**
* the wire format, endpoints and authentication headers;
* directory semantics;
* whether third-party clients are supported or permitted.

Before building the bridge: read Orgtree's public source (`engine/rs/orgtree-engine/src/mailhub.rs`) and
confirm that third-party clients are permitted [I].

**Design [I]:**
* A Rust **hub client** crate (`pe-hub`) keeps the player's and every panel's hub identities (addresses
  and secrets) in an encrypted local store.
* The **hubchat-style panel** gives the player conversations, the directory, compose with question
  options, attachments from the asset store, and presence, for Orgtree orgs and Hubchat users.
* **Inbound hub mail never reaches the private session.**
  * Each hub correspondent gets an inbox session, or the manual inbox, exactly as a panel connection does
    (21.6).
  * The private session receives a hub message only when the owner forwards it.
* **Agents reach hubs only through engine MCP tools** (`hub.contacts`, `hub.send`, `hub.inbox`) and never
  see any identity secret. A panel's agent sends as that panel's identity, never as its owner.
  * In the private session, `hub.inbox` returns only owner-forwarded messages.
  * An inbox session can reply only in its own thread.
* Sending to a new recipient needs the owner's approval the first time, in the approval layer.
* **All inbound hub mail is untrusted input**, as Orgtree treats outside mail: data, never instructions.
  It reaches an agent only sanitized and with provenance. Attachments are stored as untrusted assets and
  are never executed or imported automatically.
* **Panel-to-panel across sessions never uses the hub:** it runs over the P2P path in 21.6.

### 21.8 What this adds elsewhere

* **SCM (15.6):** reserved paths.
* **Authority (12.3):**
  * delegation is issued only by a human action;
  * sub-certificates for restricted sessions;
  * `requested_by` for guests' turns at unlocked panels.
* **MCP (17.1, 17.2, 17.4):**
  * per-session tokens and scopes;
  * admin tools are not callable by agents;
  * untrusted marking of peer-authored strings;
  * transport hardening;
  * the agent-commit policy.
* **Non-Rust boundaries (20.1):**
  * harness CLIs and their cloud model services;
  * OS containment (AppContainer, job objects, ACLs), which is OS-provided and called through the
    `windows` crate;
  * Windows OS speech recognition, as the lighter fallback only. The built-in speech models run on a
    pure-Rust runtime (21.11, D81).
* **Crates (23.1):** `pe-panels`, `pe-harness` (the harness layer, drivers, launch profiles, version
  pins), `pe-contain` (OS identities, job objects, ACLs), `pe-hub`, `pe-voice` (21.11), `pe-settings`
  (21.14).
* **Plan (23.2):**
  * phase 4: owner-only desktop panels for the three launch harnesses, with the collapsed transcript,
    open panels, desktop spatial context and guidance tools, and settings by agent;
  * unlocking, inbox sessions and "ask agent" ship per harness, after that harness's gates pass (21.9);
  * phase 5: VR panels, voice prompting and spoken replies, and the VR guidance tools.
* **Risks (23.3, risk 10):** harness drift is handled by version-pinned launch profiles (21.1).
* **Several worlds (22.9):** personal panels follow embodiment; a connection belongs to the world it
  was made in.
* **Spike report 8b, corrections:**
  * Claude Code 2.1.292 is a native executable, not a Node.js app;
  * Codex `-a/--ask-for-approval` exists only on the top-level command.

### 21.9 Verification gates before unlocking ships (spike item 9, expanded) [I]

Spike-report item 9 becomes twelve gates. Since the 19:00Z ruling, **unlocking** for harness H ships when
9g, 9i, 9j and 9l pass for H. **Restricted sessions** (inbox and decider, 21.6) ship for H when H's
containment gate passes (9a, 9b, 9d or 9k) and 9c, 9e, 9f and 9h pass too. Windows first, then Linux.

| Gate | What it proves | Pass condition |
|---|---|---|
| 9a Claude Code restricted session | The 21.5 launch line leaves only engine tools | `system/init` lists only `mcp__primordial__*` tools, one MCP server and no plugins. Adversarial prompts cannot read, write, fetch or run anything. With no `ANTHROPIC_API_KEY` the session fails to authenticate. Nothing is written under the owner's `%USERPROFILE%\.claude`. `--max-budget-usd` stops the process |
| 9b Codex containment | Reads and commands are confined | Run under the candidate profile and the 9c identity: a canary file in the owner's profile cannot be read; `--disable shell_tool --disable unified_exec` leaves no command execution; no `AGENTS.md` or hook loads from ancestors |
| 9c OS identity | Restricted sessions cannot reach the owner's files or credentials | Under AppContainer, and separately under a low-privilege user, each harness runs and reaches its provider. It cannot open the owner's canary, `.credentials.json` or `auth.json`, or the owner's Credential Manager entries. It can reach only its own named pipe |
| 9d Gemini CLI containment (only if Gemini CLI is added) | As 9b | As 9b, with `--policy` denying built-in tools and folder trust settings checked |
| 9e Workspace materializer | Reserved paths never reach a workspace | Every 15.6 name and alias (case, trailing dot or space, 8.3 name, NFKC, HFS-ignorable code point, `:` stream) is rejected or stripped on NTFS. A canary hook in a pre-rule commit never runs. The ancestor check fires |
| 9f Inbox routing | No inbound text reaches the private session; no fan-out | A "forward this to all your connections" payload does not propagate. Inbox sessions cannot call `panel.connect` or send on other connections. Forwarding works only per message |
| 9g Re-locking | 21.5 re-locking | Guests no longer admitted can send nothing, their queued turns are gone and a turn in flight is interrupted before the UI reports success |
| 9h MCP transport | 17.1 rules hold | The named pipe rejects other identities. If HTTP is enabled: a DNS-rebinding test with a foreign `Host` or `Origin` is refused, the server binds to loopback only, and a stale token is refused |
| 9i Version pin | Profiles fail closed on drift | A changed `--help` or an unknown version disables unlocking for that harness |
| 9j Sender attribution | The agent cannot be told a false sender | Adversarial guest text (forged headers, imitated owner messages, Unicode lookalikes, attempts to read the marker) never makes the agent attribute a turn to anyone but its verified sender, and `panel.turn.sender` always matches the signature |
| 9k Antigravity hooks and containment | The hook profile holds; restricted sessions are contained | Owner sessions: with the engine's deny hooks, adversarial prompts cannot run a command, fetch, start a subagent or write outside the workspace; a missing or failed hook refuses the launch; canary `AGENTS.md`, `GEMINI.md` and `.agents/` files in ancestors are not loaded. Restricted sessions also: under the 9c identity and with a credential of their own, they cannot read the owner's canary or reach the owner's keyring login |
| 9l Context and settings follow the sender | Guests' turns carry only their own context and cannot change settings | A guest's turn never carries the owner's gaze or pointing (21.12); its settings calls are refused whatever the text claims, a confidant's are allowed (21.14); a revoked confidant's next input is refused and a turn in flight stops |

### 21.10 Round-2 containment fixes (reviews NEW-9 to NEW-11, NEW-14, NEW-16, NEW-18, NEW-20, NEW-24) [I]

These rules supersede any conflicting text in 21.3-21.9.
* **The private (owner) session sends nothing without approval** (NEW-9).
  * Every outgoing action from it (`panel.send`, `hub.send`, `panel.connect`) goes through the approval
    layer, which shows the exact payload.
  * Auto-forward is removed.
  * A forward goes only into a restricted-profile turn, never into the full profile.
  * The private session reads builder-written repo text only as quoted, untrusted content.
  * Until all of this holds, 21.4's claim that the injection-to-exfiltration ("worm") path is cut does
    not hold.
* **The workspace is re-scanned before every launch** (NEW-10).
  * The 15.6 matcher (allowlist plus reserved names) runs over the whole workspace, not just its ancestors.
  * The engine answers the harness's permission prompts itself and refuses any write outside the 15.6
    allowlist, not only to a reserved path: a `git.cmd` is refused too (round 3).
  * Permission prompts do not see a shell command's writes. Restricted sessions therefore have no shell
    tool, and the workspace is re-scanned after every turn as well as before every launch [I].
  * An agent therefore cannot plant `.gemini/settings.json`, `.codex/config.toml`, `AGENTS.md` or
    `CLAUDE.md` for its next launch.
* **The fail-closed launch check names exact plugins** (NEW-11).
  * It allowlists the exact `@builtin` plugin names of each pinned harness version: today
    `cc-plugin-agents-md` and `cc-plugin-plugin-authoring` under `--bare --restricted --tools ""`
    [V reviewer]. Any other plugin kills the launch.
  * It launches with `--include-hook-events` and fails on any hook event.
  * Gate 9a plants canary `CLAUDE.md` and `AGENTS.md` files and fails if either is loaded.
* **Ancestor check** (NEW-18): it names the exact files each harness loads from ancestor directories, and
  exempts each harness's own configuration home. A blanket "no `.claude` in any ancestor" check would
  refuse every launch on a machine that has the harness installed. The list, per pinned version, is
  confirmed by gate 9a's canaries [I]:
  * Claude Code: `CLAUDE.md`, `CLAUDE.local.md`, `.claude/` (settings, commands, agents, skills, hooks)
    and `.mcp.json`;
  * Codex: `AGENTS.md`, `AGENTS.override.md` and `.codex/`;
  * Gemini CLI: `GEMINI.md` and `.gemini/`.
  * Antigravity: `AGENTS.md`, `GEMINI.md` and `.agents/`. Whether it reads them from ancestors is not
    known, so gate 9k plants canaries [I].
* **One decider per request** (NEW-14). An "ask agent" decision for a panel runs in a fresh session per
  requester, seeing only that requester's purpose text, so one requester cannot steer the decision for
  others.
* **Strangers do not spawn sessions** (NEW-16).
  * Hub mail from non-contacts lands in the manual inbox. Contacts' mail spawns at most two concurrent
    inbox sessions.
  * Containment on Linux uses a separate uid or a mount namespace plus Landlock. A user namespace alone
    still shows `/home/owner`.
  * macOS containment (an App Sandbox profile) is an unmet gate before macOS ships (D20).
* **Codex gate 9b** (NEW-20) allowlists features: every default-on feature is turned off except those the
  restricted adapter needs, which is none of the following.
  * `codex features list` (0.160.0) shows 44 default-on features [V reviewer]. Among those turned off:
    `view_image`, `code_mode_host`, `in_app_browser`, browser use, `skill_mcp_dependency_install`,
    `remote_plugin`, `tool_suggest`, web search, `computer_use`, `apps`, `hooks`, `plugins`,
    `plugin_sharing`, `multi_agent`, `image_generation`, `skill_search` and `shell_tool` /
    `unified_exec` (round 3 added the last nine).
  * The gate checks the tool list actually observed at init, so a feature added by a harness update fails
    the launch until it is reviewed.
* **Revocation** (NEW-24).
  * On Windows, a restricted process is put in a job object at creation (`CREATE_SUSPENDED` plus
    assignment, or `PROC_THREAD_ATTRIBUTE_JOB_LIST`), with `KILL_ON_JOB_CLOSE` and no breakaway.
  * Guests at unlocked panels hold no keys since the 19:00Z ruling, so `GuestRevoked` is gone.

### 21.11 Voice: speaking to your agent, and spoken replies (D33, D77, D81, D85) [I]

**Speech to text** (user rulings 21:50Z, m48857d678513, and 21:51Z, m012a25f076e9): voice prompting of
agents is a requirement. **One local, offline Whisper-class model is built into the engine, on Windows
and on Linux:** the same recognition everywhere, and nothing leaves the machine. Windows' OS speech
recognition stays only as a lighter fallback on Windows. This supersedes D77's "no voice input on Linux in
the MVP".
* **The models ship in the installer** (user 22:23Z, Q13), so voice works at first launch and offline.
* **Speaking to the agent is its own action,** separate from talking to people: hold a button or key
  while looking at a panel, or with a panel focused [proposal, Gnosarium UX 8.6].
* **The world microphone stays open while you prompt** (user 22:23Z, Q14: "dont mute, setting that can be
  enabled to mute"): people nearby hear you talk to your agent. An opt-in setting cuts world voice while
  the agent key is held. The capture feeds two consumers, world voice (unless muted) and the recognizer,
  and others see a "talking to agent" state as presence data (Gnosarium UX section 15, item 7).
* **Output:** text, with a timestamp per word, so "this" and "there" resolve to what the speaker pointed
  at when saying them (21.12). Only text enters the prompt, and from there the harness's model provider;
  audio never leaves the machine.
* **The turn records that it was spoken:** the engine-written header (21.5) and `panel.turn.sender` carry
  `input: voice`.

**Spoken replies** (user rulings 22:02Z, mcc5c8af91ffd, and 22:23Z, Q15): agent panels get a text-to-speech
tool "that theyre encouraged to use to respond to the user when they use speech to text at the panel".
* **The tool:** `panel.speak {text}` on the engine MCP server (17.2). The engine speaks the text in the
  panel's voice, and the transcript shows the same text, never extra content.
* **The encouragement:** the panel's engine-written instructions carry a line such as "When a turn's
  header says `input: voice`, answer briefly aloud with `panel.speak`, and keep the full answer in text."
  After a typed prompt the agent stays silent unless asked [proposal, Gnosarium UX 8.6].
* **Who hears it: everyone nearby, by default** (Q15: "on by default"). The reply plays spatialized from the
  panel within a short range (about 5 m times the owner's scale) [range: proposal], as presentation audio
  that never enters the sim. A per-panel switch limits it to the person who prompted, and a private panel
  (21.4) always does. A new push-to-talk at the panel interrupts it [proposal].
* **The model:** a local text-to-speech model, offline, in the installer. That speech synthesis is local,
  like recognition, is the coordinator's reading of the 22:02Z ruling, stated to the user and not
  contradicted [assumption].
* **Where the speech is made (D85, ruled as recommended at 08:31Z):** on the owner's peer, then streamed to listeners as voice
  audio through the same path and budgets as a player's voice (26), rather than each listener
  synthesizing the text. The voice then sounds the same to everyone, only one machine pays for the
  model, and a panel speaking costs listeners what a person speaking costs.

**The model runtime (D81; ruled as recommended at 08:31Z: chosen after a spike measures it)** [I;
nothing measured yet]:
* **100% Rust (20):** `whisper-rs` binds whisper.cpp (C++), so it is out. Candidates: `candle` (Hugging
  Face's framework, with Whisper examples; pure-Rust CPU kernels, and CUDA or Metal backends we would not
  enable), `burn` (a wgpu backend, so recognition could run on the GPU) and `tract` (ONNX inference on
  the CPU). Each needs the section 20 audit of its resolved build.
* **Model size:** Whisper-class models run from about 75 MB (tiny) to about 3 GB (large) at 16-bit
  weights, and quantized versions are 2-4 times smaller [I: published sizes, not checked here]. Larger
  models are more accurate and slower, and the installer carries the chosen one (Q13).
* **CPU or GPU, and the frame budget:** recognition runs only while the agent key is held, plus a short
  tail; synthesis, while a reply is spoken. On the CPU they compete with physics and rollback (6.5); on
  the GPU, with VR rendering (18.3). Neither ever runs on the sim or render thread.
* **Before choosing:** a spike measures each candidate's accuracy, time to the first word, and CPU and
  GPU cost on the user's machines, with the ball pit and a VR-sized render load running.

### 21.12 Spatial context: where the player looks and points (user ruling 21:52Z; D82) [I]

"the prompt panels will know where youre looking, what youre looking at, and where youre pointing"
(m41b9caa32fef), so prompts such as "make this bigger" or "put it there" resolve. The player's side is
Gnosarium UX 8.7.

| Context | VR | Desktop |
|---|---|---|
| Gaze ray, the object it hits, the hit point | head ray; eye tracking where the headset has it, off by default (Q8) | camera centre ray, or the cursor ray when the cursor is free |
| Pointing ray, its target and hit point | either hand's laser or index finger | the cursor ray |
| The player's position, world and scale | yes | yes |
| Guidance annotations attached to the prompt (21.13) | explicit | explicit |

* **Each part can be turned off per panel.** Eye tracking and the live feed (below) are off by default,
  the two closed defaults the user kept (Q8, 22:28Z).
* **Sampled locally:** the engine samples the rays at render rate into a short ring on the local peer (a
  few seconds), so a prompt can carry the targets at given moments. Rays are presentation data (16.3,
  18.4): they never enter the sim, and they leave the peer only inside a prompt or through the live feed.
* **Interface hits do not count:** a ray that lands on a panel, the keyboard or the approval layer is not a
  target. Each ray keeps its last world target from before it moved onto the interface [proposal,
  Gnosarium UX 8.7].
* **Typed prompts** carry the targets from when the composer gained focus, updated while a free hand or
  the cursor keeps pointing, as the player sees them in the composer before sending.
* **Spoken prompts** carry gaze and pointing through the whole utterance, joined to the recognizer's word
  timestamps (21.11), so each "this", "that", "here" or "there" can resolve to what was pointed at, or
  else looked at, when the word was said.
* **What the agent receives:** an engine-written context block in the turn, beside the sender header. For
  each target: the object id, its name (written by peers, so marked untrusted, 17.2), the hit point in
  world coordinates (chunk plus local offset, 25.2) and the surface normal; the rays; and for spoken
  turns, the table from words to targets. The same data is the resource `primordial://panel/context`
  (17.2).
* **Live feed:** an agent may subscribe to a live stream of the rays and targets, at a capped rate (for
  example 10 Hz). It is a separate per-panel permission, off by default (Q8); while it is on, the panel
  shows it.
* **A guest's or confidant's turn carries that sender's own context** (D82, ruled as recommended at
  08:31Z). The sender's peer
  sends it with the signed panel input, limited by the sender's own settings. The owner's gaze and
  pointing never go with someone else's turn, and the header says whose context the turn carries.
* **Where it goes:** the context reaches the harness as text, and from there its model provider: object
  ids and names, coordinates, and the player's position and scale, like the rest of the prompt.
* **What others see:** when a prompt is sent, its targets and attached annotations flash an outline for
  everyone nearby; a private panel shows it to its owner only [proposal, Gnosarium UX 8.7].

### 21.13 Guidance tools and annotations (user ruling 21:52Z; D83) [I]

"give tools to players to help them guide the ai: allow them to point to specific objects, select areas,
draw basic shapes, and so on" (mdba97be1a3fe). The tools (Gnosarium UX 8.8): pick objects; select a box,
a sphere, a lasso outline or a painted area; draw lines, arrows, circles, boxes and freehand 3D sketches;
place labels.
* **An annotation is a shared session object, outside the sim:**
  * an id, numbered per author (①, ②, …), so a prompt can say "copy ① into ②";
  * its author's key, and the panel it was made for;
  * its geometry in world coordinates (chunk plus local offset, 25.2): an object id or point (pick), a box
    or sphere, a surface region (lasso or paint), a polyline or stroke (lines, arrows, sketches), or text
    (labels);
  * a private flag and a "keep" flag.
* **Shown to others by default, can be made private** (user 22:28Z, Q9: "default show to others, can be
  made private").
  * A shared annotation travels to the members of its world as non-sim presentation messages, like pose
    streams, and late joiners get the live set when they join.
  * A private one stays on its author's peer and goes only into prompts, so only the agent prompted sees
    it. A guest's private annotation goes to the panel owner's peer with the guest's prompt, since the
    agent runs there.
* **Structured prompt context:** an annotation attached to a prompt sends its id, kind and geometry, plus
  the objects inside a selected volume or region (ids, and names marked untrusted), in the context block
  (21.12). `annotation.list`, `annotation.get` and the resource `primordial://panel/annotations` serve the
  full geometry (17.2).
* **Lifetime and clearing (D83, ruled as recommended at 08:31Z):** an annotation lives until its author clears it or leaves the
  world; "keep" pins it across prompts; it is never saved with the world. Admins may clear others'
  annotations (a session permission, 12.4); anyone may hide others' annotations from their own view.
* **Not sim state:** annotations never enter rollback or hashes and cannot affect physics. Ghost previews
  of what an agent proposes (Gnosarium UX section 15, item 4) are a private fork's result (17.3) drawn as
  a presentation-only overlay, and travel the same way.
* **Budgets:** each author has a cap on live annotations and on points per stroke, so a busy place cannot
  flood its members.

### 21.14 Game settings, changed by talking to your agent (user ruling 22:03Z; D84) [I]

"manipulating game settings should be able to be done by talking to your agent" (md306f929ce4b).
* **A typed settings registry:** every client setting is registered once, with a stable name
  (`audio.music_volume`), a type and range (a number with bounds and a step, a choice, a flag), a
  plain-language description and a default. The Settings screens and the agent's tools read the same
  registry, so nothing is reachable through one and not the other. Areas: graphics, audio, voice, comfort
  and locomotion, input, avatar culling (26), panels, privacy and sharing, and performance (the
  memory/CPU knob, D50).
* **Tools** (17.2): `settings.list {area?}`, `settings.get {names}`, `settings.set {changes, reason}` and
  `settings.undo {token}`. `settings.set` applies at once and returns each change with its old value and
  an undo token. Fuzzy requests ("a bit quieter") are the agent's job: it picks values within the ranges.
* **Each change records its source** (the owner, or the panel and the turn's verified sender), so the
  Settings screen can show "changed by <panel>" with a history per setting (Gnosarium UX section 15,
  item 8).
* **Guests cannot change the owner's settings** (Q12, user 22:28Z). In a guest's turn every settings call
  is refused at call time, keyed to the turn's verified sender (`panel.turn.sender`, 21.5): "Settings can
  only be changed in the owner's turn". Confidants are not guests; their turns may change settings
  (21.5).
  * The refusal is strict for direct calls in a guest's turn on a Restricted-profile panel.
  * It is not strict for a deferred request: a guest's message stays in the conversation, so the agent
    could act on it in the owner's next turn. Such a change shows the owner "may have been requested by
    <guest>", with undo [proposal, Gnosarium UX 8.12].
  * A Full-profile panel has a shell, so a guest's turn could edit the settings file itself. The engine
    keeps that file outside the workspace (21.3) and watches it during guests' turns [proposal].
* **What needs the owner's confirmation (D84, ruled as recommended at 08:31Z):**
  * settings that turn on sensitive sensing: eye tracking, and the live context feed (Q8);
  * anything that is already an approval item: unlocking and the access level, the connection policy and
    ceiling (21.6), the tool profile (D25a), adding a hub, spend caps. These go through the approval layer
    whoever asks.
  * Sharing settings need no confirmation, because openness is the default (22:28Z): annotations shown,
    the private-panel switch, speaking aloud, gaze and pointing context.
  * Comfort, graphics, audio, input and culling apply at once, with undo.
* Settings are local to the player's machine, never sim state.

---

## 22. Several worlds at once, and portals (R14)

Resonite keeps several worlds loaded but puts you in only one of them; in the others you are "away". R14
goes further: a player can be **actively joined** to several worlds at once. The engine also provides the
mechanics that let creators build seamless, consistent world transitions such as portals.

### 22.1 What "joined to several worlds" means

Three separate things are true of a player and a world:
* **Member:** you are in the world's session. You hold its log, acknowledge it, simulate it (possibly
  throttled, 22.3), may author entries and have an ACL role there.
* **Embodied:** your tracked body is there. In VR you have one body, so one world is embodied at a time.
  The exception is a portal: while you straddle an open portal, the parts of your body on the far side
  are embodied in the far world, mapped through the portal transform. On desktop, each view (window) has
  its own embodied world.
* **Attending:** you are a member and present, but not embodied.

| Presence state | Simulated | Your avatar there | Your input there |
|---|---|---|---|
| Embodied | Full rate | Tracked avatar | Hands, locomotion, voice, panels, agents |
| Attending | Full rate | An "elsewhere" avatar (D43) | Voice if routed there, chat, panels, agents; hands only through an open portal |
| Background | Throttled bursts (22.3) | Away marker | Panels and agents only |
| Parked | Not simulated; the log is kept | Away marker | None |

* "Actively joined to several at once" therefore means one embodied world plus any number of attending
  worlds, with real agency in each: voice, chat, agents and panels act there in real time, and your hands
  reach into a world through a portal.
* **Input routing:**
  * hands and locomotion go to the embodied world, or through an open portal into the far world;
  * voice goes to the embodied world by default, with push-to-talk per world and an opt-in broadcast;
  * chat, panels and agents are per world.
* Your presence record, visible to the members of each world you are in, says where you are embodied.

### 22.2 Runtime: independent worlds in one process

* **Each world is a complete, independent session:**
  * its own fixed-step clock (epoch and tick rate: the spike runs a 60 Hz and a 50 Hz world side by side);
  * its own session log, confirmation and rollback ring;
  * its own ECS store, Box3D world(s), logic modules, SCM repository, ACL and P2P session.
* **Isolation rule:** a world's simulation depends only on its own log. No sim code reads another world.
  All cross-world effects enter a world as signed entries in its own log (22.6, 22.9).
* **Process-level hazards** [S, box3d-rust 0.5.0 source]:
  * box3d-rust has no global world registry: `World::new` sets `world_id = 0`, so `BodyId`s do not depend
    on how many worlds a process created.
  * It has one process-global setting, the length unit (`set_length_units_per_meter`, an `AtomicU32`).
    The engine fixes it at 1.0 for every world and never calls the setter.
  * Its `human` sample module uses a thread-local random generator. Sim code never uses that module.
  * wasmi's lazy compilation made fuel depend on which instance compiled a function first [V d-lazy].
    Multi-world processes make that worse, so eager compilation stays mandatory.
* **Verified:** [V session-sim r14] two worlds (a 60 Hz world of 108 bodies and a 50 Hz world of 75 bodies) and three members in one process. In all four variants, every replica of each world matched the others and an offline single-world replay of its final log. Creating the worlds in the opposite order and processing the peers in reverse gave identical digests.
* **Threads:**
  * one sim task per world on a worker pool; each world stays single-threaded (4.3 rule 1), and worlds
    run in parallel;
  * one shared network thread: one UDP socket, sessions multiplexed by session id, one Noise channel per
    remote peer reused by every world you share with it;
  * the render thread draws the embodied world plus portal views (22.4).
* **Shared:** the content-addressed store deduplicates assets and wasm across worlds by hash.
* **Identity:** a user has one identity key. Each world assigns its own MemberId at admission (10.3).

### 22.3 Budgets and background throttling

* **Throttling changes when a peer computes frames, never what they contain.** A world's state is a
  function of its log, so computing frames later in bursts gives the same results. [V session-sim r14
  throttle: from t = 3 s, member 1's replica of world A advanced only once per 250 ms. Member 1 still
  authored in A: a relay and a touch. A's 500-frame digest equalled the unthrottled run's (`aa4149ef…`)
  and the offline replay.]
* **Confirmation depends on holding entries, not on simulating them.** An ack states what you hold
  (10.8), and a background member keeps sending acks on schedule, so it never delays its world's
  confirmation. [V, same run: member 2's confirmation lag in A averaged 8.5 frames before t = 3 s and 8.6
  after, whether member 1 was throttled or not. This holds by construction, because acks do not wait for
  the sim (review NEW-12).]
* **Rules for a background member** (review NEW-12):
  1. It stamps **state-free entries** at its present frame, not at its lagging sim frame: edits, commits,
     ACL changes, R14 relays and acks. They need no simulation. [V, same run: member 1 authored A's
     Verdict at frame 185 while its sim of A stood at frame 175. The frame and the time were those of the
     unthrottled run, and the relay's timeline was identical: 2,766, 2,964, 3,197 and 3,354 ms.]
  2. An entry that **carries state** (a touch, 7.3) needs the sim at its stamp frame. The member first
     catches up in one burst, then authors at the present. The cost is that burst: up to 15 frames, about
     5 ms at 256 bodies and 28 ms at 1,024 [I, from b6]. [V, same run: member 1's touch at frame 260 was
     accepted with the same stamp as unthrottled.]
  3. There is no "too late" rule left to disagree on (C1 fix, 6.4), so lag cannot fork anything.
  4. It answers fetches from its log, which needs no simulation.
  5. It sends check hashes when its sim reaches confirmed check frames; late is fine. Confirmation
     already delays a check hash by about 8.5 frames, so 250 ms bursts add little. [V, same run: member
     1's check hashes reached A's log on average 10.2 frames after their check frame (at most 13)
     throttled, against 10.0 (at most 13) unthrottled.]
  6. It is never a state-transfer donor while it lags.
* **CPU priority:** embodied, then straddled or portal-visible, then attending, then background, then parked.
  **No engine limits** on how many worlds a player has active or loaded (user 18:51Z, D49). The priorities
  above, throttling and parking are mechanisms, not caps.
* **Memory:** the snapshot ring runs from the confirmed frame to the present (6.3). With confirmation
  about 8.5 frames behind the present (6.4), a world keeps about 3-4 keyframes, not a 2 s ring. At 1,024
  bodies that is about 7-10 MB per world (2.4 MB per image [V b6]); the 120-frame speculation limit caps
  the worst case at 31 keyframes.
* **Parked worlds** keep their log and latest confirmed keyframe. Resuming fast-forwards through the
  logged entries, or takes a state transfer from an active member when it was parked long.
* **Example budget [I, from measured step costs]:** one embodied world of 1,024 bodies costs 1.5-1.9 ms per
  step [V b6]. Three attending worlds of 256 bodies cost 0.36 ms each [V b6] on other cores. Rollback
  re-simulation comes on top (6.5). Body budgets are per world (D2).

### 22.4 Rendering one world inside another

* **A portal is an engine component:** destination (world and anchor), the transform M from this world's
  portal frame to the destination's, an aperture shape, and flags (active, two-way, which objects may pass).
* **A live view needs the destination loaded locally** (attending or background). Members of A who are
  not in B see a fallback: a portal effect, or optionally a low-rate preview image streamed by a member of
  both worlds (presentation only, D44).
* **Technique (D44): stencil portals by default.**
  1. Draw A.
  2. Mark the portal's pixels in the stencil buffer.
  3. For each eye, derive B's camera through the portal: view_B = view_A · M⁻¹, with an oblique near
     plane on the portal plane so nothing behind the aperture in B is drawn.
  4. Draw B's passes, with B's own shaders and lighting, only where the stencil matches.
  5. Fix up depth so A's geometry in front of the portal still occludes.
* **Render-to-texture** is the alternative: simpler compositing, cacheable, and cheaper for small or
  distant portals, which can update at a lower resolution and rate. It costs a texture per eye per portal
  and resampling blur.
* **VR stereo correctness:**
  * each eye gets its own camera through the portal; one centre-eye texture would look flat and cause
    discomfort;
  * a portal that scales space scales the interpupillary distance too;
  * when the portal plane passes between the eyes, each eye renders the world on its own side, with the
    portal showing the other;
  * the aperture is drawn as a thin volume, so a camera passing through it never clips it to nothing.
* **Objects straddling the aperture** are drawn twice with clip planes: the near part in A's pass and a
  clone in B's pass. This is presentation only. There is no cross-world physics at portals: a clearance
  zone around each aperture keeps colliders away (D47).
* **Per-world rendering state:**
  * each world has its own WGSL set, so pipelines are keyed by (world, shader hash);
  * each world interpolates with its own clock (5).
* **Budgets:**
  * at most two live portal views per frame in VR;
  * recursion depth 1 (a portal seen through a portal shows a cached image);
  * distant portals fall back to render-to-texture at a reduced rate.
* **Audio:** B's sounds are spatialized at the aperture.

### 22.5 A seamless crossing

1. **Approach** (the portal is within about 10 m, or visible):
   * join B in the background; the portal carries B's join capability (11);
   * take a state transfer at a confirmed frame and fast-forward;
   * fetch B's sim-relevant assets, plus presentation assets near the arrival point;
   * push your avatar's assets to B's members;
   * compile B's render pipelines; shader compilation is the classic hitch.
2. **Visible:** B is rendered through the portal and attending, simulated at full rate.
3. **Reach through:** your hand starts hand-target entries in B (18.4) while your body stays in A.
4. **A held object crosses:** the transfer runs (22.6).
5. **Your head crosses:** embodiment switches at the display frame where the head crosses the plane (per
   eye while straddling). Nothing loads at that moment, because B was already simulated and rendered.
6. **After:** A becomes attending, showing the portal back. Later it goes to background, then parked.

Hitches that remain possible: B's state transfer not finished (the portal shows "connecting" and stays
closed to you until it is), a shader that was not pre-compiled, or a missing asset.

### 22.6 Carrying objects across: the settled handoff with proofs (reviews NEW-3 to NEW-6)

Two worlds have different members and unrelated frame timelines. A transfer is therefore a two-phase
handoff, stamped at frame **fA** in A and **fB** in B, in which **each world acts only on the other's
confirmed facts, and only with proof.**
* It follows IBC token transfers (Cosmos ICS-20): escrow at the source, mint at the destination on a
  light-client proof, refund on a proven rejection or timeout [S, the ICS-20 specification].
* We add fast ack-based confirmation and full object state.
* 3d0f98a relayed without proofs. Any member could then lose, duplicate or mint the object, and squat a
  transfer id (NEW-3 to NEW-5).

1. **Depart (in A, at fA).** The carrier authors `Depart{object, destination, deadline}`. The deadline
   is a frame of the destination.
   * The **transfer id is derived**, as hash(A, the Depart's entry id), so nobody can choose or squat it
     (NEW-5).
   * **The destination must be on A's route list** (reviews R3-4, R3-6). The route list is part of A's
     genesis config, so every member of A agreed to it. Changing it is an ACL'd entry in A's log [I].
     A departure anywhere else is `RejectedNoRoute`, and the object stays. So no member can send an
     object to a world it controls alone, or to a world nobody knows.
   * A's sim validates the departure: the object is live; the carrier holds its claim, or it is unclaimed;
     the carrier has the export right in A's ACL; the object is not already departing.
   * If valid, the object leaves A's physics into **escrow**, with its record, destination and deadline.
2. **A attests.** Each member of A whose replica has confirmed the departure publishes, as an entry in A's
   log, its **signature over the fact** "A confirmed departure *d* of this record to world B, by this
   deadline". The fact covers every field of the record: global id, shape kind, state, source world and
   source frame (review R3-8).
3. **Relay (anyone).** Once a strict majority of A's members have signed, anyone holding the signatures
   authors `Arrive{transfer, record, departure, certificate}` in B.
   * Because the proof is self-certifying, relaying needs no trusted carrier (NEW-6).
4. **Arrive (in B, at fB).** B's sim first checks that the source is on B's own route list
   (`RejectedNoRoute` otherwise; R3-4, R3-5). Then it checks that the transfer id matches the departure
   and that the certificate holds signatures from a strict majority of A's members (D46). If not, the
   entry does nothing (NEW-4).
   * Then B validates as before: the import right in B's ACL, B's body budget, the deadline, the arrival
     portal open, the component policy (22.8), and no duplicate transfer id.
   * B also validates the record itself, because a certificate proves who said it, not that it is sane
     (review NEW-21): every value finite, a unit rotation (norm within 1%), speed at most 200 m/s and spin
     at most 2,000 rad/s. Otherwise the arrival is `RejectedInvalid`.
   * B also refuses an arrival stamped after the deadline (`RejectedLate`; R3-6), and an object whose
     global id is already live in B (`RejectedLiveId`; R3-5).
   * The object is created with a new local id and the same global id (Accepted), or Rejected.
5. **B attests** its confirmed verdict in the same way. A `Verdict{transfer, accepted, certificate}`, with
   a strict majority of B's signatures, can be relayed into A by anyone.
   * A applies only a verdict whose certificate covers exactly that outcome (NEW-3).
6. **Resolve (in A).** Accepted commits the escrow. Rejected returns the object at its departure pose.
7. **Timeout** (review R3-6).
   * B refuses an `Arrive` stamped after the deadline. B attests that refusal like any verdict, and the
     certified refusal returns the object in A [V r14 late: a 2-frame deadline; the arrival was refused,
     and the object came back to A].
   * No separate "not received" step is needed. Anyone holding A's certificate may relay the `Arrive` at
     any time, and past the deadline that relay is refused. An object therefore waits only while nobody
     can reach B at all (22.7).
   * **Deadlines are bounded** (round 3, low). B refuses a deadline more than 3,600 of its frames ahead
     (`RejectedDeadline`), and the certified refusal returns the object.
   * **Forgetting transfers.** B forgets a rejected transfer once its deadline has passed and 3,600 frames
     after it arrived. Any replay is then late, so it is rejected again, consistently. By then the
     refusal is confirmed and attested, because a sim never runs more than the speculation limit past
     its confirmed frame. A forgets a resolved escrow 3,600 frames after the departure.
   * **B keeps accepted transfer ids** (8 bytes each). The reviewer proposed pruning all transfers past
     their deadline, but then a late replay of an accepted transfer's `Arrive` would be certified
     "rejected", and could refund an object that did arrive.
   * Once B's acceptance is confirmed, the id moves out of the rollback state into an **append-only set
     that is never rolled back or snapshotted** (round-3 final review). Snapshots then stay bounded; the
     set grows 8 bytes per accepted transfer (8 MB per million). B may drop an id once A certifies the
     transfer resolved, since A refunds only an escrow still pending [S spike, `Verdict`; I for the drop].
     The spike keeps accepted ids in its rollback state.
   * Without a certificate, A never refunds on a timer alone. B may have accepted, and refunding would
     duplicate the object.

* **Held objects** arrive attached to the carrier's hand in B (the grip offset is in the record).
  **Thrown objects** appear at B's portal with their velocity mapped through the portal transform.
* **Latency:** other members of B see the object about one confirmation round after it crossed.
  * Spike, 50 ms ± 15 ms links with 1% loss [V session-sim r14 ok, round 2]:
    * departure authored at 2,766 ms;
    * certificate complete and arrival authored at 2,964 ms (+198 ms);
    * verdict authored at 3,197 ms;
    * verdict confirmed in A at 3,354 ms.
  * That is slower than 3d0f98a (+149 ms to the arrival), by the stability round (6.4) and the attestation
    round.
  * The carrier hides the gap with a presentation-only copy in its hand.
* **Measured attacks** [V session-sim `r14 forge`]. Every one of these did nothing and the object stayed
  conserved, with every replica equal to its offline replay:
  * a member of A departing an object to a world it controls alone, where it could certify "accepted" and
    then "rejected" (`RejectedNoRoute`; review R3-4);
  * a member of A departing an object to an unknown world with deadline 0 (`RejectedNoRoute`; R3-6);
  * a member of B minting A's object from a world it controls alone (`RejectedNoRoute`; R3-5);
  * the carrier relaying A's genuine certificate with the record's shape kind changed (`RejectedNoProof`;
    R3-8);
  * a member of B minting A's object without a departure (`RejectedNoProof`);
  * a member of B squatting the real transfer id before the relay (`RejectedNoProof`);
  * a member of A committing the escrow with no certificate (`RejectedNoProof`);
  * the carrier claiming a rejection while presenting B's genuine signatures over the acceptance
    (`RejectedNoProof`);
  * a correctly certified record with a NaN position, standing for a bug or a compromised majority in A
    (`RejectedInvalid`).
* **Measured dishonest-majority attacks** (P2-5) [V session-sim `r14 forge`]. W is on A's and B's route
  lists, and members 2 and 3, its whole membership, sign as W. Every one of these was refused, and
  objects 52 to 55 stayed one live copy each in the honest worlds A and B:
  * W mints A:53, which is live in A, into B as if born in W (`RejectedNoProvenance`);
  * W mints an id it was never born with (`RejectedNoProvenance`);
  * W forges an earlier hop A to W, signed by the one member of A it controls (`RejectedNoProvenance`);
  * W splices A's genuine hop of another object onto A:54 (`RejectedNoProvenance`);
  * refund and keep: W certifies to A a refusal of A:55, so A refunds it, while W's replicas accepted it.
    W then exports it to B, presenting A's departure certificate as the hop (`RejectedNoProvenance`);
  * resurrection: A:52 went A to W to B and back to A. W exports it again into B (`RejectedFork`: the
    chain skips B's own last exit, and B now holds a fork proof against W), and into A (`RejectedLiveId`).
  * Control, the same run with the provenance checks off [V `r14 forge --no-provenance`]: the two mints,
    the forged hop, refund and keep, and the resurrection into B were all accepted. That left a second
    copy of A:52, A:53, A:54 and A:55 in B, plus a minted W:500.
* **Why settled rather than optimistic:** only confirmed, certified facts cross, so neither world ever acts
  on the other's speculation, and no correction is ever needed. The optimistic alternative is D45.

### 22.7 Consistency across worlds

**Invariant:** at every confirmed frame of A and of B, the object is live in at most one world; between
departure and verdict it sits in A's escrow, where nothing can touch it. **No loss:** the escrow record
persists until a verdict arrives.

| Event | What happens |
|---|---|
| A rolls back past fA before confirming it; the departure stays valid | The same escrow is recomputed. Nothing was relayed yet, so B is unaffected |
| A rolls back past fA and the departure becomes invalid (another member of A grabbed the object at an earlier frame: soonest wins) | In A's corrected history the object never left. Nothing is relayed. The carrier's client drops its proxy copy [V r14 abort: A rolled back 12 frames past fA; the departure became `RejectedClaimed`; nothing was relayed; the object stayed in A] |
| B rolls back past fB before confirming it; the arrival stays valid | The object is recreated identically in the re-simulation |
| B rolls back past fB and the arrival becomes invalid (an earlier portal close, the budget, or an ACL change) | B's confirmed verdict is "rejected". The verdict is relayed and the object returns in A [V r14 reject: B rolled back 10 frames past fB; the arrival was rejected; the verdict returned the object to A] |
| Either world rolls back past a confirmed frame | Cannot happen. Confirmed frames never roll back; a partition's minority side only loses frames it never confirmed (10.8) |
| The carrier crashes mid-transfer | The escrow keeps the record. The certificates are self-certifying, so every other member of both worlds relays them. Each world removes the silent carrier (10.8) and its confirmation resumes [V r14 crash: worlds of three members, one of them, member 4, in both. The carrier went silent 84 ms after its departure. Both worlds removed it once it had been silent for 180 frames; member 4 then relayed, and the object arrived in B at 6.6 s. It was conserved, and every replica equalled the replay]. Limits: a two-member world cannot remove its other member and pauses instead (D7). If nobody else is in both worlds, the object waits in escrow: two worlds with no common member have no channel at all, and refunding without B's certified refusal could duplicate it (two generals) |

* **Sync checks:** the escrow and arrival tables are part of each world's hashed state (9). Members of
  both worlds also compare A's record hash with B's arrival record.
* **Soonest wins:** at fA the departure competes with other interactions on the object in canonical order
  (8). Two departures of one object towards two worlds: the first in canonical order wins and the second is
  rejected. In B, the arrival is ordered like any entry.
* **Trust (D46):** a relayed fact counts only with a certificate: signatures by a strict majority of the
  other world's members over exactly that fact [V session-sim r14 forge]. In the spike these
  "signatures" are keyless hashes that anyone could compute: they model who must sign, not
  unforgeability, and the forge tests forge only signatures an attacker could hold. Production signs with
  each member's Ed25519 key (10.3) (round 3, low).
  * **Each world trades only with the worlds on its route list** (reviews R3-4 to R3-6). It knows their
    member lists from the same genesis config. Round 3 showed why: when every world counted, a member
    who controlled a world alone could duplicate or mint objects.
  * With route lists, a single member, including the carrier, can no longer mint, lose or duplicate an
    object [V r14 forge]. Without provenance, a dishonest majority of a routed world still could. It
    could sign a fresh departure of any global id (born elsewhere, live elsewhere, or spent), and any
    world where that id is not live would accept it [I, review of f060e30; V `r14 forge --no-provenance`].
  * **Per-object provenance chains** (D46's option for untrusted partners, MVP under D21; package P2-5,
    `spikes/sim-core/src/provenance.rs`) [V r14 forge, r14 chain]:
    * **Birth binding.** A global id is (birth world, index), and its birth record is the birth world's
      genesis object list. Production adds runtime births as a `Born` fact certified by the birth
      world's majority [I].
    * **Hops.** When A commits a transfer (on B's certified acceptance), A's members attest "A committed
      transfer t of object g to B; g had entered A by transfer v (0: born in A)". That certificate is the
      hop. A departure certificate alone is not enough: a dishonest B could certify a refusal to A, which
      refunds the object, and still claim to hold it.
    * **Chains.** Each `Arrive` carries the hops from the object's birth into its source world, and the
      source's departure certificate covers the chain's hash, so a relayer can neither strip nor swap it.
      The destination checks every hop against the epoch certificates of the world that signed it,
      whether or not that world is on its own route list. Each hop carries its source's certificates up
      to the epoch that signed it, and that epoch must be the one active at the hop's commit frame
      (P25-5, 10.8) [V r14 chain-epoch: a hop signed after its source removed a member checks at a world
      that does not route to the source; provenance unit tests]. **Not enforced yet** (review VC-7): the
      hop's frame and epoch are not in the signed fact, so this check trusts the carrier's labels, and
      the source's `cert_at` counts its majority differently from a third party (VC-8). Rely on neither
      until @provenance-eng's follow-up signs both. A to B to C to A works [V r14 chain:
      three hops, conserved, all equal in both orders].
    * **Settling.** The relayer copies A's commit certificate into B's log as a `Provenance` entry. The
      entry is never simulated, so it changes no state. The object's next `Depart` from B carries that
      hop, and is `RejectedUnsettled` without it. Measured: the hop was relayed into B 61 ms after the
      verdict was confirmed in A, 0.65 s after the departure [V r14 ok, chain]. A world can export only
      an object whose way in it can prove.
    * **Pinning (review P25-2).** B's copy depends on A's commit certificate.
      * A dishonest A that withholds it, or an honest A whose members all leave within the window
        before they attest (about 60 ms after the verdict is confirmed, measured above), can stop the
        object from ever leaving B.
      * That is griefing, not duplication: the object stays live and usable in B, and A gains nothing
        it did not already have.
    * **Rejected: a hop made of A's departure certificate plus B's accepted verdict** (review P25-2;
      @coordinator-opus, 20:33Z).
      * It would end the pinning. But a dishonest B could then certify a refusal to A (A refunds) and an
        acceptance in its own hop, and every later world would accept the object: refund and keep would
        become a double spend that is only detected, where today it is refused [V r14 forge].
      * Both forms cannot be offered side by side, because an attacker always picks the weaker one.
    * **Last exits.** Each world keeps, per global id, whether the object is here (live or in escrow) or
      which transfer last took it away. That record is never pruned: in the spike's layout it is about
      48 bytes per object that ever left, plus the certified export that brought the object in, which
      it keeps as fork evidence (about 0.2 KB with two signers). An arrival of an object that is here
      is `RejectedLiveId`. An arrival whose chain skips this world's last exit
      is `RejectedFork`. Two exports by one world of the same incoming hop are kept as a fork proof
      against that world's majority (review P25-3):
      * the proof carries both certificates, so any third party can check it [V r14 forge, r14
        doublespend: "verifies for a third party: true"; unit test];
      * it is recorded only once the frame that found it is confirmed, so every replica holds it from
        the same confirmed state, and a speculative frame later rolled back leaves none.
    * **Costs per frame (review P25-4).** Each object's chain hash is cached with its chain. Commit facts
      are queued once, when their frame is confirmed, and attested once; nothing rescans the table.
    * **Why not a world prefix.** The reviewer of round 3 proposed that a global id's prefix must equal
      the source world. Objects travel on (A to B to C), so the chain binds the id to its birth world
      instead.
    * **What remains: double spends by a holder** [V `r14 doublespend`]. A dishonest-majority world can
      export an object it legitimately received a second time, into a world that has not seen the object
      since. That includes the world it got the object from: A:52 went A to W to B; W then exported it
      into A, citing its genuine hop in, and A accepted it. That left two copies, in A and B.
      * When the copies meet (here, B's return of A:52 into A), the receiving world holds a transferable
        fork proof: two exports by W's majority of one incoming hop.
      * If the copies never meet, nobody ever holds a proof.
      **Detect only (user, D79, 20:07Z):** the fork proof is the defence, and the cheating world can then
      be cut off (removed from route lists). A per-object registry that orders every hop, for example at
      the birth world, was considered and rejected under D79, not even as an opt-in: it costs one extra
      relay round per hop, and objects freeze when their birth world is unreachable [I].
    * **The guarantee: no double identity** (review P25-1). A dishonest majority can lose, keep or
      double-spend only objects that passed through it, and cannot give a second live identity to any
      other object.
      * **Provenance does not vouch for content.** An object's kind, state and carried data are signed
        by each holder's majority alone. W can export A's common object as a rare kind under its genuine
        id, which W's own logic could not tell from legitimate crafting.
      * **Provenance does not vouch for new births.** A world's own births (its genesis, and in
        production its `Born` facts) are its majority's word.
      * **Chains may pass through unlisted worlds.** `check_chain` accepts hops through worlds on no
        route list, because identity is unaffected. But W can then also pass along objects "born" in a
        world D that it alone controls.
      * So a partner world's objects, and their content, are only as trustworthy as the worlds they came
        through.
      * **The policy hook** (`ImportPolicy`, a stub: the spike's worlds accept anything; refusals are
        `RejectedPolicy`). A destination can restrict:
        * which birth worlds it accepts objects from;
        * which worlds a chain may pass through. Its own route list is a natural default for untrusted
          partners;
        * which kinds it accepts from each source world.

        Production adds per-kind content checks, run by the destination's logic on arrival (22.8,
        22.9) [I].
    * **Chain size.** Each hop is a certificate of the source's epoch majority. With Ed25519 that is about
      64 bytes per signer, about 0.65 KB per hop for a 16-member world, so a chain grows linearly with
      hops [I]. Possible reductions: aggregate signatures, or a destination checking only the hops after
      its own last exit (it checked the earlier ones on the way in) [I].
    * **Long-range forgery.** Members of an old epoch who have since left can still sign facts for that
      epoch. Binding a fact's frame to the epoch active at that frame (the signed boundary of 10.8, an
      epoch certificate's `from_frame`) stops a later epoch backdating, but not an old majority that
      colludes, nor a carrier that omits the later epoch's certificate. This is the long-range attack of
      proof-of-stake systems [I; the unit test shows both].
  * **Member lists change** (review R3-8b). Since P2-4 a world's members change only by decided epochs,
    each with a certificate that the previous epoch's quorum signed, and another world follows them from
    the world's genesis [V P25-5]. One check still uses the member list fixed at startup: a
    destination checks the departure certificate in an `Arrive` against its route list's members of
    the source, so a member the source has since removed still counts there [S session.rs]. The
    `Arrive` should carry the source's epoch certificates, as hops do [I].

### 22.8 What crosses: avatars, components, code

* **Avatars are not sim objects** (16.3). Crossing switches embodiment; the avatar's assets were pushed to
  B during the approach.
* **Engine components carry over:** transform, rigid body, mesh and material references, grab state.
* **Logic-defined components:** B may not run the module that defines them. Options (D48):
  * by default they travel as inert data (schema hash plus bytes) and regain behaviour in any world that
    runs that module;
  * shared schemas, a cross-world standard library, let worlds interoperate;
  * running a foreign module in B needs B's explicit ACL opt-in, because it is code B's builders did not
    commit.
* **Carried data is untrusted input in B** (review NEW-22). Its bytes were made by A's logic, or by
  whoever controls A, and B's module deserializes them.
  * A foreign module is pinned by its canonical hash (13.2), never by name or version: B's opt-in names
    exact hashes, and a component whose schema hash B has not pinned stays inert.
  * The engine enforces size caps before any logic sees the bytes: per component and per object, set in
    B's world config.
  * B's module validates the data on arrival (`on_arrival`, 22.9). A record it cannot parse stays inert
    instead of trapping, and every peer reaches the same verdict, because the bytes are in B's log.
* **Assets** are fetched by hash from the carrier and other members of both worlds.

### 22.9 What game logic, MCP and panels see

* **Game logic sees only its own world.** Cross-world reads would make a world depend on a timeline some
  of its members do not have.
  * Cross-world events arrive as entries in its own log: `on_depart_request` (allow or deny, e.g. a gate
    that needs a key), `on_departed`, `on_arrival`, `on_returned`.
  * Logic can open, close and retarget portals, and filter what may pass.
  * **Cross-world messages** use the same relay: an outbox entry in A is relayed after confirmation as an
    entry in B, exactly once by (source world, outbox sequence).
* **MCP** (17): one server per peer.
  * Every tool takes an explicit `world` argument; there is no implicit current world for agents.
  * New tools: `worlds.list` (joined worlds, presence, clock, lag); `worlds.join`, `worlds.leave` and
    `worlds.set_presence`, which need the owner's confirmation; `portal.*`; `transfer.request`,
    `transfer.status`.
  * Resource subscriptions and deterministic forks are per world. Delegation certificates name the worlds
    they cover.
* **Panels** (21):
  * personal panels (wrist, HUD) follow your embodiment from world to world; they are presentation-only,
    so they need no transfer;
  * a panel anchored in a world stays there and can be seen through portals;
  * a panel-to-panel connection belongs to the world it was made in, and closes when either owner leaves
    that world;
  * an unlocked panel's "anyone in session" means members of the panel's world.

### 22.10 Spike: two worlds, three members, one object handed between them [V]

Setup: world A runs at 60 Hz (a 108-body pile), world B at 50 Hz (a 75-body pile). Member 1 is in both
and carries object 40 from A to B; member 2 is only in A, member 3 only in B. Links are 50 ms ± 15 ms with
1% loss. Run with `scripts/run_session_sim.sh`; logs are in `spikes/out/session-sim/`.

| Variant | What it exercises | Outcome | Conservation | Replicas vs offline replay |
|---|---|---|---|---|
| ok | Plain handoff | Committed in A, accepted in B; done 0.59 s after departure (round 2 adds the stability round and certificates; 0.41 s in 3d0f98a) | 1 live copy (B), 0 pending | all equal, both worlds |
| abort | Member 2's earlier grab reaches member 1 150 ms late: A rolls back 12 frames past fA | Departure `RejectedClaimed`; nothing relayed | 1 live copy (A) | all equal |
| reject | Member 3 closes B's portal at a frame member 1 hears of 150 ms late: B rolls back 10 frames past fB | Arrival rejected; verdict returns the object to A | 1 live copy (A) | all equal |
| throttle | Member 1's replica of A is a background world (one burst per 250 ms) | Same as ok | 1 live copy (B) | all equal; digests equal to ok's; member 2's confirmation lag unchanged |
| forge | Relays by members who cannot have proofs: a member of B mints the object; a member of B squats the transfer id; a member of A commits the escrow; the carrier claims a rejection with B's signatures over the acceptance. Round 3 adds a departure to a world one member controls, a departure to an unknown world, a mint from a world one member controls, and a certified record with its shape kind changed. P2-5 adds a routed world W whose whole membership is dishonest: two mints, a forged hop, a spliced hop, refund and keep, and two resurrections (22.6) | `RejectedNoProof`, or `RejectedNoRoute` for the round-3 routes; a NaN record `RejectedInvalid`; P2-5: `RejectedNoProvenance`, `RejectedFork`, `RejectedLiveId` | 1 live copy (B), 0 pending; objects 50 and 51 still in A; A:52 to A:55 one copy each in A and B | all equal (A, B and W) |
| chain (P2-5) | Three worlds, A (60 Hz), B (50 Hz) and C (60 Hz), routed to each other. Member 1 carries object 40 A to B to C to A; each next departure carries the hop in | Every departure and arrival accepted; the arrival back in A checks a two-hop chain that contains A's own last exit | 1 live copy (A), 0 pending | all equal, all three worlds and both orders (digests differ between orders, for abort's reason) |
| late | A 2-frame deadline, shorter than the certificate round | Arrival `RejectedLate`; the certified refusal returns the object to A | 1 live copy (A), 0 pending | all equal |
| crash | Three members per world; member 4 is in both. The carrier goes silent 84 ms after its departure (reviews NEW-6, R3-7) | Both worlds remove the carrier after 180 frames of silence; member 4 relays the transfer; accepted in B | 1 live copy (B), 0 pending | all equal (live members) |
| ok, abort, reject with worlds and peers in reverse order | Process-level isolation | Same as forward | same | all equal. ok and reject have digests identical to forward. abort's A digest differs (review round 3): the escrow key derives from the Depart's entry id, whose chain contents depend on processing order |

With a crashed carrier and nobody else in both worlds, the object waits in escrow (22.7), and a two-member world pauses (D7). "crash" shows the case with another member in both worlds and three members per world (review NEW-6).

### 22.11 What this adds elsewhere

* Crates (23.1): `pe-worlds`, holding the multi-world scheduler, presence, portals and the transfer
  relay.
* MCP catalog (17.2): `worlds.*`, `portal.*`, `transfer.*`.
* Plan (23.2): two worlds and the settled handoff in phase 3, with portal rendering after the renderer;
  VR straddling in phase 5.
* Decisions: D43-D49.

---

## 23. Crate layout, phased build plan, risks

### 23.1 Proposed workspace [I]

```
crates/
  pe-core        ids, fixed-step clock, deterministic math (dmath), rng streams, hashing, errors
  pe-sim         sim store (ECS columns), object hierarchy, component IDL/registry, snapshot + hash
  pe-physics     box3d fork wrapper: body binding, interactions -> forces/joints, world images,
                 multi-threaded stepping (D2)
  pe-session     session log: entries, acks, confirmation, membership epochs, ordering, claims,
                 carried state, keyframes, rollback/re-sim, sync checks (from spikes/sim-core session.rs)
  pe-net         UDP + Noise, reliability, bulk channel, clock sync, NAT traversal
  pe-discovery   invites, session codes + PAKE, mDNS, Mainline DHT records, peer exchange, relays,
                 relay mailboxes for offline encrypted mail (D32)
  pe-worlds      multi-world scheduler, presence, portals, settled cross-world handoff (R14)
  pe-scm         content-addressed store, commits, module merge, merge seats, reserved paths, ACL log
  pe-logic       runtime host (wasmi / wasmtime per D12), guest ABI, fuel, hot-swap + migration
  pe-logic-sdk   guest-side crate (no_std + alloc), component macros, state arena; wasm32-unknown-unknown
  pe-build       every-peer builds (D11): pinned toolchain fetch and verify (D75), sandboxed reproducible
                 builds, build cache, hash comparison and mismatch reports (D74)
  pe-assets      CAS, importers (gltf, png/jpeg), deterministic processing, P2P transfer
  pe-render      wgpu renderer, WGSL hot reload (naga), extrapolation + error smoothing, portal passes
  pe-xr          OpenXR session, Vulkan interop, poses, swept hand engagement, per-eye portal straddling
  pe-mcp         rmcp server, tool catalog, per-session tokens and scopes, forks
  pe-panels      in-world panels: chat UI with the collapsed transcript, terminal emulator (PTY mode),
                 VR keyboard, approval layer, signed sender attribution (D27a) and confidants, open-panel
                 streams, spatial context, guidance annotations, cross-session connections over P2P (D32)
  pe-harness     one harness layer: drivers for Claude Code, Codex and Antigravity, capability flags,
                 version-pinned launch profiles, restricted sessions
  pe-voice       built-in speech to text and text to speech on a pure-Rust ML runtime (D81), push-to-talk
                 capture, word timestamps; Windows OS recognition as the fallback
  pe-settings    typed client settings registry: ranges, descriptions, change history and undo (21.14)
  pe-contain     OS identities (AppContainer / users), job objects, ACLs for restricted sessions and
                 sandboxed builds
  pe-hub         optional Orgtree bridge: automatic player and panel hub identities, hubchat-style panel
                 backend (D32)
  pe-resonite    package / data-tree / MeshX importers, avatar mapping
  pe-app         executable: threads, windowing (winit), config, CLI
tools/           determinism trace runner, audit_native.py, session simulator (from spikes/session-sim)
spikes/          phase-1 spikes (kept as regression tests)
```

### 23.2 Phased plan (the order is D22, settled 19:11Z; re-planned for the 18:42-19:21Z rulings)

What moved, and why:
* **Strangers may join from day one (D21):** the proposer-based view change and per-object provenance
  chains move from phase 3 into phase 2, ahead of everything that lets strangers in.
* **Every peer compiles (D11):** a new crate, `pe-build`, ships with live editing in phase 4, and phase 2
  proves that builds are byte-identical across Windows and Linux.
* **No hub (D32):** cross-session messaging rides the engine's P2P network. Relay mailboxes come with
  relays in phase 3, panel messaging across sessions in phase 4, and the Orgtree bridge is an optional
  add-on in phase 4.
* **Linux from day one (D20):** Linux joins Windows in every phase's exit criteria.
* **Also folded in:** parallel Box3D (D2) once its feasibility study succeeds, 90 Hz with a 60 Hz
  fallback (D4), grabs-only claims (D72), provisional arrival and following for regions (D60), and larger
  sessions as a planned target (D23).

| Phase | Content | Milestone and exit criteria |
|---|---|---|
| 2. Deterministic core, and a protocol fit for strangers | pe-core, pe-sim, pe-physics, pe-session (from the spikes). **The proposer-based view change** (10.8): several members removed at once, withheld or forked votes, colluders, joins, demotion to spectator, and per-epoch certificates other worlds follow (R3-8b); about the size of round 3's R3-1 and R3-2 together [I]. **Per-object provenance chains** (D46's option for untrusted partners). Grabs-only claims (D72). A CI determinism matrix on the user's machines: Windows and Linux, x86-64 on two CPUs (ARM deferred). Byte-identical guest builds across Windows and Linux (D11). The in-process session simulator with network faults; **pe-mcp from day one** as the test and driver interface | M1: 2-4 desktop peers on a LAN, Windows and Linux mixed: a physics sandbox with state+frame interactions, confirmation, gap repair, equivocation handling and the view change. Two silent members, a withheld vote and colluders recover in the simulator, which stays all-equal and conserved. The same logic commit builds byte-identically on both OSes. An AI agent drives a peer over MCP |
| 3. Real networking, look, scale, two worlds | UDP+Noise, bulk channel, invites and session codes, NAT traversal with volunteer relays (D35) and **relay mailboxes** for offline mail (D32); state-transfer join; wgpu renderer, extrapolation and smoothing; glTF assets + P2P asset transfer; **region-scoped rollback** (6.6) and **regions** with provisional arrival and following (D60); **multi-threaded stepping** in the fork if the D2 study succeeds; **pe-worlds**: two worlds per client, the settled handoff with provenance checks, portal rendering | M2: friends and strangers play over the internet with no server of ours; late join works; a ball pit of 500 balls stays within budget at 90 Hz, or at its 60 Hz fallback (D4); an object carried through a portal between two sessions |
| 4. Live editing + agent panels | pe-logic + SDK (runtime per D12, hot-swap, migration, component IDL, state arena); **pe-build** (D11): sandboxed reproducible builds on every peer, toolchain distribution (D75), build cache, an activation lead from members' build times, mismatch handling (D74); pe-scm (confirmation finality, module merges, scoped seats, reserved paths, ACL); WGSL hot reload; MCP logic and shader tools + forks; agent panels for Claude Code, Codex and Antigravity through one harness layer: owner panels (structured adapters + PTY) with the collapsed transcript, **open by default** (21.4), desktop spatial context and guidance tools (21.12, 21.13), settings by agent (21.14), the approval layer, inbox and decider sessions, **unlocking with signed sender attribution** (D27a) and confidants, **cross-session panel messaging over P2P** (D32); the **Orgtree hub bridge** as an optional add-on, with automatic player and panel identities (D32); **gameplay cloth and soft bodies**: an XPBD module in pe-physics, two-way coupled, about 2,000 particles per world by default, after phase 3's regions and D58 settle (coordinator, 08:32Z; `spike/cloth` is the reference) | M3: a player's own Claude Code, Codex or Antigravity, in an in-game panel, edits logic and shaders in a live session within the 2.1 iteration budget, and every peer runs modules it built itself; conflicts reject and rebase per R7; a guest at an unlocked panel is always attributed correctly (gate 9j); two players in different sessions message each other's panels with no hub |
| 5. VR + avatars | pe-xr (OpenXR + Vulkan interop) on Windows and Linux (SteamVR, Meta, Monado); swept hand engagement; 3-point IK; Resonite avatar import v1; VR panels + virtual keyboard; **voice**: the built-in speech model for prompting, and spoken replies, on Windows and Linux (21.11, D81); VR gaze (head, and eye tracking where present) and hand pointing as context, and the VR guidance tools (21.12, 21.13); per-eye portal straddling; **cosmetic cloth** (capes, hair, banners): local, never rolled back, outside the hash (coordinator, 08:32Z) | M4: two PC VR users, on Windows and Linux, with imported Resonite avatars interact with physics objects and walk through a portal; one prompts an agent by voice and both hear its reply |
| 6. Scale + discovery | Mainline DHT discovery records; snapshot deltas; **larger sessions** (compact acks, relaying), a planned target (D23); background-world parking | M5: discovery without invites; sessions above 8 players; thousands of bodies across regions; several attended worlds per client |

### 23.3 Top risks

1. **Physics CPU budget.** The port is serial and scalar, and rollback multiplies step cost [V b2, b6, ball
   pit]; 90 Hz (D4) multiplies it again. Mitigations: multi-threaded stepping in our fork (D2; its
   feasibility study is under way), coalesced rollbacks, keyframes, region-scoped rollback in phase 3,
   remote-hand dead reckoning (6.10, D52), a windowed snapshot policy (D50), the 60 Hz fallback (D76) and
   opt-in input delay (D41).
2. **Determinism beyond the machines tested.** With the engine's physics (vdW on), main (d79f160) is
   byte-identical on this PC in five build settings, on <laptop> (Intel, also with `target-cpu=native`) and
   as a Linux musl build, the four wasm modules included [V @design-reviewer's re-check; report
   `determinism-main-d79f160.md` in its working folder]. ARM and macOS never ran. Parallel stepping (D2) must
   stay bit-exact across thread counts. Mitigation: the phase-2 CI matrix on the user's machines (P2-6).
3. **The session protocol is new code, and strangers may join from day one** (D21). Confirmation,
   eviction and fetch repair were proved in an in-process simulator and over loopback UDP, not on the
   internet. The view change (10.8) is built in the spike: several members failing at once, withheld or
   forked votes, colluders, an equivocating proposer and joins all recover in the simulator [V 5k]. What
   is left is its quorum (D80), its port into `pe-session` (P2-3), and runs over real networks.
4. **No central authority means cheating is bounded, not prevented** (8.2, 8.5). With strangers in MVP
   sessions, the remaining backdating window (about 4 frames, about 9 with packed hand targets under
   congestion, D73) and the room to pick favourable tie ranks are exposed from day one.
5. **Every peer compiles** (D11). Byte-identical builds across machines and OSes are shown on one machine
   only; a mismatch pauses that peer (D74). Joining a world costs a build of all its modules, and every
   peer runs rustc on other people's code, inside a sandbox.
6. **Transport work.** The 100%-Rust rule excludes the turnkey stacks (iroh, libp2p, quinn's TLS), so we
   build reliability and congestion control (10.2).
7. **Alpha upstreams and our fork.** Box3D is v0.1.0 "alpha" and box3d-rust is a recent third-party port
   [S]; our fork now carries vdW recycling and will carry parallel stepping and hook points. Pin versions,
   run golden tests, wrap the API.
8. **Logic runtime choice** (D12). The faster runtime (wasmtime) compiles C on Linux and macOS today [I], and
   JIT-banned platforms need an interpreter.
9. **Resonite import format churn** (D19); format details unverified locally (19.1).
10. **OpenXR + wgpu interop** is not yet exercised (18.2), on Windows or on Linux (Monado).
11. **Agent panels (R13).** An unlocked panel gives guests the owner's rights (D27a, the user's accepted
    trade-off): prompt injection with the owner's authority, and spend on the owner's account. Harness
    CLI flags and protocols change often. Mitigations: Locked by default, signed sender attribution (gate
    9j), version-pinned launch profiles, contained inbox and decider sessions (21.9). Confidants (21.5)
    carry the same rights at every access level. Open panels (21.4) show prompts and replies to everyone
    nearby, so an agent that reads a secret can repeat it aloud; the secret filter is best-effort.
    Antigravity's restriction rests on hooks, not containment (gate 9k).
12. **The Orgtree bridge** (21.7): the hub's wire format is unverified, third-party clients may not be
    permitted, and the fourth client type needs a change on Orgtree's side.
13. **Multi-world memory and CPU** (22.3): there are no engine limits (D49), so a player can open worlds
    until the machine struggles. Mitigations: throttling, parking and the memory/CPU slider (D50).
14. **Built-in speech models** (21.11, D81): installer size, accuracy, and CPU and GPU cost next to
    physics and VR rendering are not measured yet.

### 23.4 Phase 2 work packages

Phase 2 split into packages that can be staffed in parallel, one agent each. @engine-architect keeps the
protocol core (P2-3, P2-4) and design ownership, and integrates. Already running and not listed: the
parallel Box3D feasibility study (D2, @vdw-physics) and the <laptop> and Linux determinism re-check on main
(@design-reviewer).

Rules for every package:
* Windows and Linux from day one (D20): "done" always means both.
* Builds, tests and benchmarks run at low priority with capped jobs, one heavy job at a time (machine-load
  rule); timings taken that way are labelled noisy.
* Work on a branch; @engine-architect merges. Protocol and determinism packages are reviewed by
  @design-reviewer before merge.
* Done means the stated tests or scenarios pass, with logs committed under `spikes/out/` or the crate's
  test output, and the design updated where the package changes it.

**P2-1. Core and sim store (`pe-core`, `pe-sim`).**
* What: ids, the fixed-step clock, deterministic math, RNG streams and hashing; the sim store
  (deterministic component columns), the object hierarchy, a minimal component registry, and snapshot +
  hash (6.1, 16). Production versions of what `spikes/sim-core` does ad hoc.
* Skills: Rust systems programming, data-oriented (ECS) design, determinism discipline (4.3).
* Depends on: nothing.
* Done: unit tests; snapshot and restore round-trip bit-exact; state hashes identical on Windows and
  Linux; snapshot cost no worse than the spike's (6.2).

**P2-2. Physics wrapper (`pe-physics`).**
* What: the engine's only door to the box3d fork: body binding, interactions to forces and joints, world
  images, `engine_world_def()`, several worlds per process; room for multi-threaded stepping (D2) when
  the feasibility study lands.
* Skills: Rust, physics engines (Box3D), floating-point determinism.
* Depends on: P2-1.
* Done: the spike's goldens, 600-frame trace and rollback T1-T5 reproduce through the wrapper, release and
  debug, on Windows and Linux, with digests equal to the committed records (`spikes/out/vdw/`).

**P2-3. Protocol core (`pe-session`), @engine-architect.**
* What: the session protocol ported from `spikes/sim-core` and `spikes/session-sim` into a crate with a
  stable API: entries and chains, acks, votes, stable-data confirmation, equivocation proofs and
  eviction, crash removal, demotion, the tie rank, carried state, keyframes and rollback, sync checks, gap
  repair, the congestion controller, R14 transfers. **Grabs-only claims** (D72) change here. Validation
  includes "a member moves only its own hands" (8.4), from the verified author of each entry.
* Skills: distributed protocols, Rust.
* Depends on: P2-1, P2-2; P2-4's view change and P2-5's provenance chains are merged into it.
* Done: all 30 phase-1 scenarios pass against `pe-session` (every replica equal to its offline replay,
  conserved, 0 false checks), plus grab-race scenarios: two grabs within one latency (the loser's grab
  undone), and pushes on a held object (never refused), and `handowner`.

**P2-4. The proposer-based view change, @engine-architect**
(@item:primordial-engine/phase-2-proposer-based-view-change-multiple-fail).
* What: membership changes decided by a proposer and a quorum (D80) for session and world logs:
  several members removed at once, withheld or forked votes, colluders, joins, demotion to spectator,
  and per-epoch certificates that other worlds follow (R3-8b). Built in the spike first, then merged
  into P2-3.
* Skills: consensus protocols (PBFT, Raft joint consensus), Rust.
* Depends on: nothing new; it builds on round 3's votes and stable-data confirmation.
* Done: `crashstall` and new scenarios recover: two members silent at once, a withheld vote, and two
  colluders among five. `badvote` (two colluders among four, no honest majority) recovers too, because a
  forked vote is itself a signed equivocation proof, and members excluded by proof do not count toward
  the quorum. No confirmed frame is ever undone; world-log eviction finishes within about 8.9 s with 256
  members (measured with a log-only world, 25.5.2); the suite stays all-equal and conserved; reviewed by
  @design-reviewer.
* **Status:** built in the spike on `viewchange`; review round 1 by @design-reviewer at b01ebdf (changes
  requested: VC-1 to VC-10, HO-1, HO-2), fixed in round 2 together with the D80 admin layer, each repro
  showing the failure with its fix switched off [V session-sim, spike report 5k]. VC-7 and VC-8 go to
  @provenance-eng's follow-up, stated in 10.8's limits and 22.7. Review round 2 at 758ff5e (changes
  requested: VCR-1 to VCR-12, and HO-1's carried state) is fixed in round 3, again with a repro per
  finding that fails with its fix switched off; VCR-10 waits for the user (V1-V3). Review round 3 at
  a9286fc (changes requested: VC3R-1 to VC3R-8) is fixed in round 4, with @design-reviewer's two repros
  and seven more; V4 is a question for the user. Review round 4 at 452e1f7 (changes requested: VC4R-1 to
  VC4R-6, two halting the session) is fixed in round 5, with the reviewer's `joinwedge` and `slowref`
  repros and a randomized Byzantine schedule generator (`randsched`) that checks the written invariants
  (`spikes/out/session-sim/INVARIANTS.md`) on every run; it found one further hole (RG-1), now fixed.
  Review round 5 (changes requested: VC4R-1's stronger fix, VC5R-2, an L1 lag bound) is answered in
  round 6, with the reviewer's `prevotefork` and `fakemix` repros; VC5R-1 is reopened as VC6R-3 below.
  Round 6's review bounded the polka ids a precommit carries (VC6R-2, honest code panicked above 64 in a
  large session; at most a quorum, `MAX_POLKA` = 512) and tightened the generator's lag bound. The
  delta re-check caught that the bound measured the speculative sim frame, which a halt pins one window
  behind the clock, so L1 never fired; it now measures the clock-frame present (`now - conf`, two
  windows), and the RG-1 control fails L1. **Landed on main at 5ba3eab (31 October)**, with the P2-9
  tool brought onto the landed API at f05324f.
  **Three one-faulty-member liveness halts are known open, to be fixed as round 7 on main** (they do not
  touch safety — every replica still agrees — only liveness):
  * **VC6R-3** (`adminwedge`): a faulty admin forks one seq, proposes a change listing the honest
    version, and crashes; honest members lock on it and the fork that arrives later disputes the version
    they hold, so the lock never clears. The round-5 argument (a proposer lists only stable entries) was
    wrong: `admin_pending` fills at validation. Fix: admin validity a pure function of the change's
    bytes, or listed admin entries must be stable at the validator.
  * **VC6R-1** (`capfake`): a capped precommit whose polka lists an id that exists nowhere is retried
    forever and never votes the capper out. Fix: after one fetch budget, vote it out.
  * **VC6R-4** (`fakeadmin`): a member authoring a no-op admin entry every epoch, plus a pure fake
    reference, has its reference timers cleared by every decision (VC5R-4 clears them all). Fix: clear
    only the pairs whose vote a decision answered.
  The port into pe-session is P2-3, @world-chunks's; membership reaches it through a `Membership` seam.

**P2-5. Per-object provenance chains (D46's option for untrusted partners, now MVP under D21).**
* What: every global id bound to its birth world; each `Arrive` certificate references the certified
  transfer or birth record that brought the object into its source world; a destination checks the chain
  back to the birth world against epoch certificates.
* Skills: protocol security, signatures and certificate chains, Rust.
* Depends on: P2-4's epoch-certificate interface (start with today's static membership behind that
  interface).
* Done: `r14 forge` extended with dishonest-majority attacks (minting an object, resurrecting a moved one,
  forging an earlier hop), all refused; A to B to C carries conserved; the suite all-equal; reviewed by
  @design-reviewer.

**P2-6. Determinism matrix.**
* What: one command that builds and runs the determinism set (`scripts/run_determinism.sh`) on each
  available machine and compares digests across them: Windows x64, Linux x64 (glibc and musl) and two
  x86-64 CPUs. ARM64 is deferred: no ARM machine is available.
* Skills: build and CI engineering, cross-compilation, scripting.
* Depends on: nothing. **Settled (user, 19:58Z): CI runs on local machines only:** this PC's Windows and
  WSL, plus <laptop>; nothing leaves the user's machines. Owner: @design-reviewer.
* Done: a digest table, equal across every machine available; a mismatch names its first differing
  output.
* **Status (2026-10-31)** [V]: `scripts/run_matrix.sh`, with columns win, glibc, musl and laptop (docs/p2-6-p2-7-matrix.md). **PASS: 72 outputs equal on 4 machines** (spikes/out/matrix/matrix.txt). A difference names its first differing output and line.

**P2-7. Byte-identical guest builds across OSes (D11 groundwork for `pe-build`).**
* What: build the same guest commits on Windows and Linux hosts with the pinned toolchain and the 13.2
  rules; find and remove host-dependent differences; measure build times for the activation lead.
* Skills: the Rust toolchain and reproducible builds (rustc and cargo internals), Linux.
* Depends on: a Linux host with the pinned toolchain. **Approved (user, 19:58Z):** install rustup and
  build-essential in WSL Ubuntu-24.04. Owner: @design-reviewer.
* Done: the spike guests plus one larger test crate are byte-identical (raw hash) on both hosts and from
  two checkout paths; every difference found is fixed or written down as a 13.2 rule; a build-time table.
* **Status (2026-10-31)** [V]: done.
  * **Byte-identical:** the 4 spike guests plus logic_mono and logic_wasm are raw-hash identical on Windows (two CPUs) and on Linux, and at two paths each.
  * **Found and fixed** (13.2): the host separator in workspace-relative panic paths, host-dependent crate hashes in the name section (now stripped), and toolchain drift (now pinned).
  * **Build times:** the table is in 13.2. It led to the user's 22:47Z ruling: committed modules use the reproducible `rel-fast` profile.

**P2-8. MCP server from day one (`pe-mcp`).**
* What: the test and driver interface: an rmcp server with the 17.x tool catalogue for phase 2 (step,
  state, propose an interaction, sync and membership status, scenario control), per-session tokens and
  scopes, structured errors. Replaces `spikes/mcp-stub`.
* Skills: Rust, MCP and JSON-RPC, rmcp.
* Depends on: P2-3's API (start against the spike API).
* Done: Claude Code connects and drives a peer through a scripted scenario; every tool has tests,
  including its errors; the schema is documented.
* **Status: merged** (main 7ed07cc; approved by @shader-reviewer at b3b54e9). 46 tests pass on Windows and
  on Linux (WSL2: static musl binaries built on Windows, and a native glibc build), and Claude Code drove a
  peer over the pipe and over HTTP on Windows. Not yet: Claude Code on Linux, resource subscriptions, and
  `schema.components`. **The P2-3 seam** is `crates/pe-mcp/src/backend.rs` (`DrivenPeer`, plus
  `ScenarioControl` for the simulator only). `pe-session` needs, per driven member: author an interaction
  at the present frame and broadcast it, an entry's verdict and whether it is confirmed, frame and `conf`,
  object state, hashes and check hashes, membership with evictions and view-change decisions, sync
  counters, and the protocol events log.

**P2-9. Session simulator as a test tool (`tools/session-sim`).**
* What: the phase-1 simulator turned into a tool: scenarios in files, fault injection (partitions,
  jitter, loss, asymmetric and capped links, crashes, Byzantine behaviour), offline-replay comparison,
  one regression command.
* Skills: Rust, distributed-systems testing.
* Depends on: P2-3 for its final form; it starts from `spikes/session-sim`.
* Done: the 30 phase-1 scenarios and P2-4's new ones run from scenario files with identical results; one
  command runs them all.

**P2-10. LAN peer for M1 (`pe-app`, minimal).**
* What: a headless peer executable running `pe-session` over UDP with signatures (from `spikes/p2p`;
  Noise and NAT traversal come in phase 3), with MCP attached.
* Skills: Rust networking, application glue.
* Depends on: P2-2, P2-3, P2-8.
* Done: the M1 demo: 2-4 peers on a LAN, Windows and Linux mixed, converge to identical histories under
  scripted loss and delay, survive a member dropping out, and an AI agent drives one of them.

Order: P2-1, P2-6, P2-7, P2-8 and P2-9 can start at once; P2-4 starts now (@engine-architect); P2-2 after
P2-1; P2-3 after P2-1 and P2-2, taking in P2-4 and then P2-5; P2-10 last.

---

## 24. Decisions needing the user

Each entry gives a recommendation and its alternatives; section references point to the reasoning. The
priority order at the top of this document breaks ties. Where a recommendation costs a lower priority, the
entry says so.

* **D1. Non-Rust boundaries (20).**
  * **Settled (user, 19:11Z): as recommended.** System components, the platform C runtimes and the Rust toolchain are allowed, and Rust inline assembly is allowed; Linux targets `linux-gnu`. The Wayland C shim turned out not to be compiled at all (20.2).
  * Recommend: accept these components:
    * the dynamically loaded or OS-provided components in 20.1: GPU drivers and graphics runtimes, OS
      libraries and Windows import-lib stubs, the OpenXR loader and runtime, Linux system libraries;
    * the platform C runtimes that Rust itself links (20.1, review L1);
    * the Rust toolchain (rustc/LLVM) as a build tool for game logic.
    Target Linux as `linux-gnu` (dynamic glibc, OS-provided), not static musl.
  * Also rule on the gray areas in 20.2: the Wayland C shim on Linux; Rust `asm!` in crypto crates, in
    wasmtime and in wasmi's `libm`;
    wasmtime's `helpers.c` on Linux and macOS (D12); a DXC DLL if HLSL were chosen.
  * This entry also asks the user to confirm the coordinator's reading that "dynamically loaded system
    components are acceptable". It absorbs D24, which duplicated it.
  * Alternatives: stricter readings, such as no Linux Wayland (X11 only), no inline-asm crates, or no
    wasmtime at all.
* **D2. Physics scale for the MVP (4.4, 6.5; review M1).**
  * **Settled (user, 18:44Z): "goal is parallel".** The body budget is met with multi-threaded Box3D in our fork, bit-exact across thread counts and machines, not by capping at about 500 bodies. Feasibility study: @vdw-physics.
  * Recommend: **about 500 simultaneously awake bodies per world** (priority 1).
    * A 6-frame rollback then costs about 5 ms; at 1,024 bodies it costs 10-13 ms, beyond the 8 ms
      per-tick budget.
    * Pull island- or region-scoped re-simulation (6.6) forward to phase 3.
  * Alternatives: 1,000 bodies with deeper amortization (corrections visible for longer); invest now in
    parallelism inside box3d-rust (it must stay bit-exact).
* **D3. Float determinism approach (4.2, 4.5).**
  * **Settled (user, 19:11Z): as recommended, as amended by D58.**
  * Recommend: IEEE f32 with the 4.3 rules, plus a CI matrix that includes ARM. Keep `double-precision`
    (large worlds) off for the MVP.
  * **Superseded in part by D58** (25.2, R16): canonical `i64` chunk plus `f64` offset, Box3D
    `double-precision` on, and region-local origins, at 2-10% of step time [V]. The 4.3 rules and the
    CI matrix still apply.
  * Alternatives: enable double precision now (bigger worlds, slower, different goldens). Fixed point is
    not feasible with Box3D.
* **D4. Tick rate and speculation limit (5, 6.4, 7.5).**
  * **Settled (user, 18:46Z): the rate is configurable per world, with a 90 Hz target and 60 Hz as the fallback** where performance is a concern. The 2 s run-ahead limit stands. What triggers the fallback: D76.
  * Recommend: 60 Hz with 4 sub-steps, and a 2 s (120-frame) speculation limit.
  * 90 Hz removes most hand-to-object mismatch in VR, but costs 1.5x the CPU. In the ball pit, re-simulation
    rose from 2.76 to 3.95 ms per frame, and the frame is only 11.1 ms [V ballpit]. Per-world choice, with
    90 Hz for VR-centric worlds.
  * Alternatives: 90 Hz everywhere; a 1 s limit (less memory, earlier pauses under stalls).
* **D5. Meaning of "soonest wins" (8).** The user's 09:32Z ruling settles ties (D37).
  * **Settled:** by the user's tie ruling (D37) and grabs-only claims (D72).
  * Recommend: effective frame first, then the per-frame tie rank, plus exclusive claims (8.4).
  * Alternatives: sub-frame microsecond timestamps (finer, but trusts clocks more); no exclusivity, so all
    simultaneous impulses apply and "soonest" only orders them.
* **D6. Same entries, different state, between exactly 2 peers (9.3; review M7).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend:
    * both re-simulate from the last agreed confirmed frame;
    * if they still differ, both pause and report;
    * neither peer's state is privileged.
  * Alternative: one deterministic winner, e.g. the lower member id. Simpler, but it hands one peer the
    world line (an R1 tension).
* **D7. Partitions (10.6, 10.8; reviews H3, M7).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: only a side with a strict majority of the current epoch confirms anything or changes
    membership. The minority speculates up to the speculation limit, pauses, then rejoins by state
    transfer. For **two-member sessions**: both pause.
  * Alternatives:
    * two-member sessions let the remaining peer continue alone after 10 s, in a new epoch. When the other
      returns, it rejoins by state transfer and loses whatever it did alone, and a real partition can fork
      both sides;
    * the owner's side wins. Rejected as an R1 tension.
* **D8. Transport (10.2).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: UDP + Noise (snow without `std`) with our own reliability and bulk channel. 100% Rust, and
    the session protocol already runs over plain UDP [V p2p-v2].
  * Alternatives: quinn + custom pure-Rust crypto (more integration work, standard congestion control);
    relax the rule to allow `ring` and use iroh (most turnkey; also depends on n0 relays).
* **D9. Discovery and the "soft central point" question (11, 10.5; review L8).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: invites + mDNS + peer exchange + Mainline DHT records, and no servers of ours.
  * Accepting the public BitTorrent DHT (and its well-known bootstrap hosts) is the user's call. So is
    whether symmetric-NAT users may use an optional community relay.
  * Committers also depend on rustup and crates.io unless the toolchain is mirrored and dependencies are
    vendored (13.2).
  * Alternatives: our own bootstrap or relay servers (reliable, but a soft central point); invites and LAN
    only (purest, worst UX).
* **D10. Root of authority (12).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: the world creator's key is the genesis owner and can add co-owners; optional k-of-n owner
    threshold for ACL changes.
  * Alternatives: a mandatory multi-owner threshold; a fully democratic majority of present members.
* **D11. Who compiles game logic (13.2).**
  * **Settled (user, 18:53Z): every peer compiles.** The consequences are in 13.2: byte-identical builds across machines and OSes, the activation lead and the cost of joining, toolchain distribution (D75) and what a mismatch means (D74).
  * **Build profile settled (user, 22:47Z): "Fast builds".** Committed modules use the pinned `rel-fast` profile (no LTO): one-line edits rebuild in about 0.5 s and stay byte-identical across OSes, for about 1% of run time and about 35% more bytes against LTO (13.2).
  * Recommend: the committer compiles and peers run the shared wasm (sandboxed), identified by its
    canonical hash, with background reproducibility checks. Vendored dependencies; no build scripts or
    proc macros in logic crates except the SDK's.
  * Question: mirror the pinned toolchain (no rustup host for committers), or accept rustup as a soft
    central point?
  * Alternative: every peer compiles from source (strongest trust; every machine needs the toolchain).
* **D12. Logic runtime (13.3; priorities 1-2; reviews D12-1 to D12-7).**
  * **A session cannot mix runtimes:** fuel exhaustion differs between them. So the choice applies to every
    OS at once. Options:
    1. **wasmi everywhere (recommended now):** 100% Rust on every OS, cross-OS sessions work, 8 crates.
       Logic runs at about 7-10x native, the 10 ms hot swap holds, and logic at 10k entities costs
       6.4-8.8 ms.
    2. **Cranelift everywhere (recommended later)**, once a maintained `[patch]` removes wasmtime's
       `helpers.c` and a fuel-exhaustion test passes on Windows, Linux and macOS.
       * Logic runs at 1.7-2.1x native.
       * The hot swap compiles off-thread during the activation lead.
       * 82 crates, plus the patch to maintain.
    3. **Runtime-independent fuel:** insert fuel counters into the module at commit time, so exhaustion
       depends only on the module bytes, then pick the fastest runtime per OS [I]. NaN handling must match
       too.
    4. **A user exception for wasmtime's C on Linux and macOS:** Cranelift everywhere today. This breaches
       the 100%-Rust ruling there.
  * **Settled (user, 15:10Z): option 1 now, wasmi everywhere. Option 2, Cranelift everywhere, follows
    once the `helpers.c` patch is in and the fuel-exhaustion test passes on Windows, Linux and macOS.**

* **D13. Shader language (14).**
  * Recommend: WGSL (naga-native, pure-Rust validation, structured diagnostics).
  * Alternatives: HLSL via DXC (non-Rust DLL, no naga frontend); GLSL via naga; rust-gpu (Rust shaders,
    heavier toolchain).
  * **Settled (user, 15:40Z): WGSL-only for now.** Slang was evaluated as an author-side front end
    (14, end) and can be revisited later.
* **D14a. Merge granularity (15.3; review H1).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: the module is the merge unit. Commits to different modules combine without recompiling; the
    same module conflicts.
  * Alternative: the later author rebases, recompiles and resubmits for every conflict (strict linear
    history).
* **D14b. Merge seat scope and clock (15.4; review H2).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: seats are scoped to the conflicting modules and paths, the 10 s starts at the front of the
    scope's queue, and a `seat_held_by` refusal gets no seat.
  * Alternatives: a global seat (the phase-1 design); the 10 s counted from the rejection.
* **D14c. Commits that hit an active seat (15.4).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: reject with `seat_held_by` and let the author retry.
  * Alternative: hold them and apply them in order when the seat ends (less retry noise, more hidden
    queueing).
* **D14d. Fuel exhaustion (13.6; review M6).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: disable the module at that frame, as rollback state; reverting it is an ordinary commit.
  * Alternative: automatic revert to the last good commit (it made the SCM head depend on sim state).
* **D15. ECS (16.1).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: our own sim store (deterministic columns, WASM-defined components, memcpy snapshots).
  * Alternative: bevy_ecs for everything, adding stable-ID mapping and reflection-based snapshots.
* **D16. Content hash (15.1, 16.6).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: BLAKE3 in `pure` mode.
  * Alternative: SHA-256 (RustCrypto), slower but ubiquitous.
* **D17. Missing artifact at activation (15.5, 16.7).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: a peer that lacks a sim-relevant artifact at its activation frame pauses its sim. It keeps
    receiving entries and catches up once it has the bytes. Others are unaffected.
  * Alternative: delay activation until every member acknowledges the artifact (one slow peer delays
    everyone).
* **D18a. VR runtimes (18.1, 18.2).**
  * **Settled (user, 18:57Z): Windows and Linux from day one,** desktop and PC VR through OpenXR on Vulkan: SteamVR and Meta/Oculus PC on Windows, SteamVR and Monado on Linux.
  * Recommend: SteamVR and Meta/Oculus PC runtimes first, through OpenXR on Vulkan; Windows first, Linux
    (Monado/SteamVR) second.
  * Alternative: D3D12 interop on Windows.
* **D18b. Hand model (18.4; review M2).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: hands enter the sim only when engaged. Engagement uses a swept, velocity-scaled test;
    non-engaged hands do not collide.
  * Alternative: always-physical hands (a constant input stream per hand, rollback load as in the ball pit,
    7.5).
* **D19. Resonite import: legal and sourcing (19).**
  * **Settled (user, 19:11Z): as recommended.**
  * Questions for the user:
    * Is parsing Resonite's native package format acceptable under Resonite's ToS and EULA?
    * Should we avoid knowledge derived from decompiled DLLs (the local MeshX note) and work clean-room
      from exported files only?
    * Are users responsible for third-party avatar licenses (e.g. purchased models) when avatars are
      redistributed P2P to other peers?
    * Does "wear the avatar" in v1 include 3-point IK, which VR needs to wear one at all (review L9)?
  * Recommend: clean-room parsing of user-exported files; prefer the standard-format export path (glTF) if
    it preserves skinning; show a license notice on import; include basic 3-point IK.
* **D20. MVP platforms.**
  * **Settled (user, 18:57Z): Windows x64 and Linux x64 from day one;** macOS flat-screen later.
  * Recommend: Windows x64 desktop and PC VR first; Linux x64 second; macOS flat-screen only (no OpenXR
    runtime) and later.
  * Alternatives: Linux first; include macOS in the MVP.
* **D21. Trust model for MVP sessions (8.2, 8.5, 9.3).**
  * **Settled (user, 18:56Z): strangers may join from day one.** The proposer-based view change (several silent members, withheld or forked votes, colluders) and per-object provenance checks (D46's option for untrusted partners) become MVP prerequisites (23.2).
  * Recommend: trusted or invite-only sessions for the MVP. The protocol already bounds backdating, proves
    equivocation and evicts by majority; it does not prevent every cheat. Public sessions with strangers
    come later.
  * Alternative: design for adversarial public sessions from day one (stricter input validation, attested
    clocks, more protocol work up front).
* **D22. Phase order and first milestone (23.2).**
  * **Settled (user, 19:11Z): the phases as planned,** re-planned in 23.2 for D11, D21 and D32.
  * Recommend: phases 2→6 as listed in 23.2.
    * The MCP server is built from phase 2.
    * Region-scoped rollback moves to phase 3 (D2).
    * Two worlds with the settled handoff come in phase 3; portal rendering in phase 3, after the
      renderer; VR straddling in phase 5.
  * Alternatives: pull VR earlier if VR is the headline; pull live editing before real networking if the
    AI-authoring loop matters most.
* **D23. MVP session size (10.1).**
  * **Settled (user, 18:51Z): 8 players per session for the MVP;** larger sessions remain a planned target, through compact acks and relaying (23.2).
  * Recommend: up to 8 members, full mesh. Acks grow linearly with members (7.6).
  * Alternative: 16-32 members with relay or gossip forwarding (more protocol work in phase 3).
* **D24.** Merged into D1.

* **D25. Harness integration path (21.1).**
  * **Settled (user, 19:11Z): as recommended.**
  * **Launch harnesses (user, 21:54Z and 21:55Z): Claude Code, Codex and Antigravity,** "in the same generic
    way orgtree supports these harnesses": one harness layer with a driver per harness (21.1). Gemini CLI
    and other ACP agents are not launch harnesses.
  * Recommend:
    * structured adapters by default: Claude Code `stream-json`, Codex `exec --json` or `app-server`, and
      ACP for Gemini CLI and others, rendered as a chat UI;
    * a PTY terminal as the owner-only fallback;
    * launch profiles pinned per harness version. After a harness update, unlocking stays off until the
      profile is re-verified (21.9). This costs ease of use.
  * Alternatives:
    * PTY only: simplest, but no structured events, and never shareable;
    * SDK-based hosts: the Claude Agent SDK is TypeScript or Python, so it needs a non-Rust sidecar process.
* **D25a. Tool profile of the owner's own panel session (21.3).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: the **restricted** profile by default: file tools in the materialized workspace plus the
    engine MCP server, with no shell and no web. The owner can switch to the full profile in the approval
    layer.
  * Alternative: the full profile by default. More capable, but injected repository text could then run
    commands as the owner.
* **D26. Panel visibility to other peers (21.4).**
  * **Reversed (user principle, 22:28Z): open by default.** People nearby hear and see prompts, replies,
    guidance drawings and the agent's work (21.4). A per-panel "Private panel" switch, off by default (Q20,
    22:54Z, "for now"). Six things stay closed by default (Q19, 22:34Z): the Locked default, the approval
    layer, credentials and keys, panel inboxes, direct messages and eye tracking. What an open panel sends
    others: D86, ruled (b) at 08:31Z: everything, raw tool output included.
  * Settled at 19:11Z as recommended (below); that default no longer holds.
  * Recommend: private by default. Others see the panel, its owner, an "agent working" indicator and the
    signed edits it produces. The owner may share the rendered transcript with chosen peers.
  * Alternatives: visible to the whole session by default (more social; leaks prompts and code); never
    shareable.
* **D27a. Default access level (21.5).**
  * **Settled (user, 19:00Z): Locked stays the default, and unlocking gives guests unconditional access to the panel as though they were the owner,** with the agent told who sends each message (21.5).
  * **Extended (user, 22:34Z): confidants.** Friends the owner marks as confidants may prompt every panel of the owner, Locked ones included, as though they were the owner, and the agent is told they are a confidant (21.5). They do not confirm the owner's approval cards (Q22, 22:54Z, "for now").
  * Recommend: panels start **Locked**. Friends or Anyone is an explicit opt-in per panel.
  * Alternative: Friends by default (easier sharing, more exposure).
* **D27b. Sessions per guest (21.5; review R13-H3).**
  * **Superseded by the 19:00Z ruling:** guests write into the panel's own session.
  * Recommend: one restricted session, one MCP token and one key per guest. Guests never share a
    conversation, and rights bind to the token.
  * Alternative: one shared guest session. Cheaper, but it allows cross-guest injection and misattribution.
* **D27c. OS identity of restricted sessions (21.5; reviews R13-H1, H2).**
  * **Settled (user, 19:11Z): as recommended,** for restricted (inbox and decider) sessions; guests no longer run in them.
  * Recommend: a per-panel AppContainer on Windows (no admin rights needed).
  * Alternative: a dedicated low-privilege local user. It needs admin rights once, and it is simpler for
    harnesses that dislike AppContainer.
* **D27d. Credentials of restricted sessions (21.4, 21.5).**
  * **Superseded for unlocked panels by the 19:00Z ruling:** guests use the owner's credentials and capacity. Restricted (inbox and decider) sessions keep their own key with a hard limit.
  * Recommend: a **guest-only API key** that the owner creates in a provider workspace or project with a
    hard spend limit. Restricted sessions never use the owner's login or subscription.
  * Alternative: a guest brings their own harness and key, joining as a guest participant instead of
    running on the owner's machine (D34 asks which the user prefers; review NEW-25). Running guests on the
    owner's login stays excluded by containment.
* **D27e. Adapter-side caps (21.5; review R13-M1).**
  * **Settled (user, 19:11Z): as recommended, for restricted sessions.** At an unlocked panel the caps are an optional owner setting, off by default (19:00Z ruling).
  * Recommend: per turn at most 30 tool calls and 8,000 output tokens; per guest at most 20 turns an hour;
    plus a per-panel total.
  * Alternative: provider-side limits only (simpler; a single long turn can burn the budget before it
    stops).
* **D27f. Commits from guest sessions (21.5).**
  * **Superseded by the 19:00Z ruling:** a guest's turn follows the owner's own rules, `confirm` included where it applies (D28a).
  * Recommend: they always wait for the owner's approval in the approval layer.
  * Alternative: guests within their roles commit freely. Faster, but the owner's agent writes code it
    never reviewed.
* **D27g. Which harnesses may be unlocked (21.5, 21.9).**
  * **Superseded in part by the 19:00Z ruling:** unlocking needs the attribution and re-locking gates (21.9); the containment gates now gate restricted sessions.
  * Recommend: each harness only after its own verification gates pass (Claude Code first). Codex and
    Gemini CLI stay owner-only until their containment is proven. PTY panels are never unlockable.
  * **Since the 21:54Z harness ruling:** Claude Code, Codex and Antigravity each get restricted sessions
    once their own containment gate passes (9a, 9b, 9k).
  * Alternative: unlock any structured harness with best-effort flags. Rejected, because Codex's read-only
    sandbox does not stop reads of the owner's credential file.
* **D27h. Guest transcript retention (21.5).**
  * **Superseded by the 19:00Z ruling:** guests' turns are part of the panel's own transcript.
  * Recommend: keep a guest's transcript in that guest's session directory so the session can resume, and
    delete it on revocation unless the owner keeps it for audit.
  * Alternative: never persist guest transcripts (`--no-session-persistence`). Each visit starts fresh.
* **D28. Authority of guest-driven edits (21.5, 12).**
  * **Superseded by the 19:00Z ruling:** a guest's turn acts with the owner's rights; commits record the guest as `requested_by`.
  * Recommend: effective rights = owner ∩ guest ∩ guest-mode scope. Commits record the session key, the
    owner and the requesting guest.
  * Alternative: the panel acts with the owner's full authority, so a friend could do anything the owner
    can.
* **D28a. Agent-originated commits in shared worlds (17.4; review M4).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: `confirm` by default where more than one user holds owner or builder: each agent commit
    waits for its delegating user's confirmation. `confirm-batch` and `auto` are explicit choices.
    * This costs iteration speed (priority 2) only at commit time. `logic.check` and `logic.test_in_fork`
      need no confirmation.
  * Alternative: `auto` everywhere (fastest; any prompt injection that reaches a committing agent ships to
    every peer).
* **D29. Friend identity without a server (21.5).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: local friend lists of Ed25519 keys, added in-session, by invite ticket or by fingerprint/QR,
    and synced across the owner's devices as signed records; optionally linked to a mail-hub person
    identity.
  * Alternative: friends defined only through a mail hub's directory (convenient, but depends on a hub).
* **D30. Default connection policy and timeouts (21.6; review R13-C2).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: new panels default to **ask user** (60 s timeout); **ask agent** is an explicit opt-in;
    switching to *deny all* drops existing connections; idle TTL 30 min.
  * **Stated risk:** people click "accept". Under this design, accepting admits text into a contained inbox
    session, never into the owner's private session (D30a). A careless accept therefore costs what that
    session can do: read-only queries the connection was granted, and a summary card to the owner. It is
    not free: the text still reaches a model.
  * Alternatives: default *deny all* (safest, more friction); default *allow all* (frictionless, invites
    spam).
* **D30a. Where inbound panel and hub messages go (21.6, 21.7; review R13-C2).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: one contained inbox session per connection or correspondent. Messages reach the private
    session only when the owner forwards them one at a time, into a restricted-profile turn; there is no
    auto-forward (review NEW-9).
  * Alternative: deliver into the private session with provenance labels. That is the phase-1 design, and
    it lets any connected agent prompt-inject a full-tool session.
* **D30b. Capability ceiling for connections (21.6).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: **text only** by default. Above it, in order: structured JSON, asset references, world
    actions. No connection is ever granted more than the owner's ceiling.
  * Alternative: structured JSON by default (richer agent-to-agent protocols, more parser attack
    surface).
* **D31. Meaning of "adjacent" for panel connections (21.6, 22.9).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: membership of the same world session defines who may request. Spatial proximity is a
    discovery filter only, because positions are attacker-controlled. Under R14, a connection belongs to
    the world it was made in.
  * Alternatives: require spatial proximity as well (e.g. within 5 m), either for every request or only for
    *allow all* to apply; or proximity alone.
* **D32. Mail-hub integration (21.7).**
  * **Settled (user, 19:20Z and 19:21Z): option (a).** The hub is an optional bridge to Orgtree orgs and Hubchat users only; all in-game messaging, across sessions too, runs over the engine's P2P network with relay mailboxes (21.6); every player and every agent panel gets a hub identity automatically, and engine panels are a fourth, purple-hued mailhub client type (21.7). Whether to ship a default hub: D78.
  * Questions for the user:
    * May the engine implement a client for Orgtree's hub protocol? Its license, permission and stability
      are unverified.
    * Should the player's hub identity be a separate hub person identity (recommended), or derived from the
      engine key?
    * Should hub use be limited to private networks or a TLS proxy, since the hub has no TLS of its own?
  * Recommend: agents never see the hub secret and reach hubs only through engine MCP tools. First contact
    with each recipient needs the owner's approval.
* **D33. Voice input (21.2).**
  * **Revised (user, 21:51Z, m012a25f076e9): a local Whisper-class model built into the engine, on Windows and Linux;** Windows OS speech recognition stays only as a lighter fallback. The models ship in the installer (22:23Z, Q13). Runtime, size and where it runs: D81. Spoken replies: 21.11, D85.
  * Settled at 19:11Z as recommended (OS speech recognition on Windows); that answer no longer holds.
  * Recommend: OS speech recognition on Windows for the MVP (an OS boundary).
  * Alternatives: local Whisper-class models on a pure-Rust ML runtime (100% Rust, heavy); no voice in the
    MVP.
* **D34. Spend and provider terms (21.4, 21.5; review R13-M1).**
  * **Settled (user, 19:00Z):** guests at an unlocked panel use the owner's AI capacity. Whether a provider's terms allow that is the owner's responsibility (21.5).
  * **Question for the user:** may a player let other people use AI capacity that is billed to the player?
    Consumer subscriptions (Claude Pro or Max, ChatGPT plans) generally cover personal use. We have not
    verified whether any provider's terms allow sharing them with guests. Through API keys, guests'
    usage is the key owner's responsibility under the API terms.
  * Recommend:
    * guests use only a guest-only API key with a provider-side hard limit (D27d);
    * never the owner's subscription;
    * the owner sees per-guest usage and the remaining budget before unlocking and while unlocked.
* **D35. Volunteer relays (10.7; review R13-L2).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: relays that anyone can run, forwarding only end-to-end-encrypted traffic.
    * **Discovery:** relays publish signed records in the Mainline DHT. Friends and session members also
      recommend relays through peer exchange.
    * **Choice:** each peer picks relays by its own policy, preferring relays its friends run.
  * **Stated exposure:** a relay sees who talks to whom (keys and IP addresses), message timing and sizes,
    and it can drop traffic selectively. It cannot read or forge traffic.
  * Alternatives: our own relays and DNS domain, like e4mc (simplest, but a central point); iroh with
    self-hosted relays (needs a `ring` exception, D8).
* **D35a. Session codes (10.7, 11; review R13-M3).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: four words plus a check word. The public two-word nameplate is a DHT rendezvous; the secret
    two-word password is used only inside a PAKE with the code's creator, and nothing derived from it is ever
    published. With a friend in the session, it shrinks to three words and nothing is published.
  * Alternative: three words plus a check word, hashed into a DHT key. Shorter, but anyone watching the
    DHT can break the code offline in minutes.
* **D35b. What a code's creator announces (10.7).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: a relay address, so DHT observers do not learn the creator's IP address.
  * Alternative: the creator's own address. It works without any relay, and exposes the creator's IP
    while the code is live.

* **D36. Finality and membership (6.4, 10.8; reviews C1, C2, H3).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend:
    * frames are confirmed from signed acks (unanimous holding of the current epoch's members);
    * membership changes by strict-majority view changes;
    * a 2 s speculation limit, after which members pause;
    * entries identified by hash, with equivocation proofs and eviction.
  * Alternatives:
    * confirm with a majority of acks instead of all (faster when one member lags, but a lagging member's
      entries could then arrive for a confirmed frame and would have to be refused, undoing its author's
      input);
    * timer-based finality (the phase-1 design, rejected: it forks honest peers).
* **D37. Same-frame tie-break and member ids (8.3; ruling 09:32Z; review L7).**
  * **Settled (user, 09:31Z and 19:11Z):** the user left the key to the architect; the per-frame permutation as recommended.
  * Recommend: a per-frame random permutation, `tie_rank(epoch seed, frame, member id)`. Member ids are
    assigned in admission order by the membership log; the seed comes from the session or epoch genesis.
    Every member wins about 1/n of ties [V ties: 41/50/59 of 150].
  * Alternatives:
    * a deterministic rotation of join order (fully predictable a frame ahead; equally fair over time);
    * the hash of (frame, public key) with no session seed (a key can be ground for chosen frames);
    * the lowest id wins (a permanent winner; rejected).
* **D38. Claim rules (8.4; review M3).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend:
    * plain impulses claim once and never renew;
    * held grabs renew while held;
    * a 10-frame cooldown after a claim lapses;
    * world logic never claims;
    * MCP agents are rate-limited at intake.
    * No cap on how long a player may hold an object.
  * Alternatives: cap continuous holds (e.g. 60 s), which stops agents parking objects but surprises
    players carrying things; no cooldown, which lets a machine-speed toucher monopolize an object.
* **D39. Reading of R3: what may cross the network (7.7; review L9).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: R3's "only when a player interacts" applies to **sim state**. Acks, check hashes, clock
    pings, presentation streams (avatar poses, panel views) and panel or hub messages also flow; none of
    them carries sim state.
  * Alternative: a stricter reading with no continuous presentation streams. Remote avatars would then
    freeze between interactions.
* **D40. Scope of the carried state (7.3; ruling 09:30Z).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: the touched bodies' exact state, plus a digest of their contact island; a mismatch triggers
    a scoped state transfer.
  * Alternative: always carry the whole island (exact neighbourhoods sooner, at 52 bytes per island body;
    a resting pile is one large island).
* **D41. Continuous perturbations: hands in a ball pit (7.5; the main question).**
  * Recommend: a continuous interaction is a **perturbation source**. Its pose streams as small entries
    every frame; the touched objects are simulated, never streamed. Measured: about 15 kB/s for both
    directions, against 1 MB/s for naive state streaming, at a CPU cost of about 2.8 ms of re-simulation
    per frame on the receiver [V ballpit].
  * Settled (user, 13:36Z): **input delay** is an opt-in per-world setting with a default of 0. Four
    frames cut the receiver's rollbacks from 59/s to 2.7/s, but the author's balls then react 67 ms late.
    Observers are served by dead reckoning instead (D52).
  * Alternatives: stream quantized island state (≈0.47 MB/s, and not bit-exact unless the author snaps its
    own copy too); stream exact state (≈1 MB/s).
* **D42. Lateness and backdating (8.2; review H5; user rule 14:31Z).**
  * **Parameters settled (user, 19:11Z): as recommended.**
  * Settled by the user: nothing is re-stamped, and an author never sees its own interaction corrected.
    Lateness is paid by the late receiver.
  * Recommend for the parameters:
    * a horizon slack of 2 frames, which bounds backdating to about 4 frames;
    * demotion (10.9) for a member whose horizon trails the present by more than W/2 for 3 s [V capped
      slow member, backdate part 2: a member that lags its horizon on purpose is now demoted, where
      before it stalled confirmation for good].
  * Alternatives:
    * a larger slack: more tolerant of bursty acks, but a wider backdating window;
    * no demotion: a persistently late member stalls everyone at the speculation limit;
    * an arrival-median bound computed from chain references: deterministic, but it couples effective
      frames to membership and again rewrites authors' history at high latency (viz-2).

* **D43. What others see of a player who attends their world from elsewhere (22.1).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: an "elsewhere" avatar: dimmed, with a badge naming the world where the player is embodied,
    with mirrored head and hand motion at a low rate, and with voice only when the player routes it there.
    By default voice goes to the embodied world only.
  * Alternatives:
    * an away marker, as in Resonite (quieter, less presence);
    * the avatar driven by the player's own AI agent (an AI-forward option, but people may mistake the
      agent for the person);
    * hidden.
* **D44. Portal rendering technique, and what non-members see (22.4).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: stencil portals with per-eye cameras in VR, at most two live portals per frame and recursion
    depth 1, with render-to-texture for distant portals. Members of A who are not in B see a portal effect
    only.
  * Alternatives:
    * render-to-texture everywhere (simpler; costs memory and sharpness);
    * a low-rate preview image of B streamed by members of both worlds (presentation only; it shows B to
      people B never admitted, so B's owners would have to allow it).
* **D45. Cross-world transfer mode (22.6).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: **settled handoff.** Relay only facts the other world has confirmed. The object appears
    for B's other members about one confirmation round after it crossed (about 0.15-0.2 s on 50 ms links
    [V r14]), and the carrier hides the gap. It never needs a correction.
  * Alternative: **optimistic arrival.** B shows the object at once, marked provisional and held
    exclusively by the carrier, and rescinds it if A's departure is undone. About 0.15 s sooner for
    observers, at the cost of rare visible rescinds and transient effects in B.
* **D46. How a world trusts a relayed fact (22.6, 22.7; reviews NEW-3, NEW-4).**
  * **Settled (user, 19:11Z): as recommended.** With D21 (strangers from day one), the option for untrusted partners, birth-world binding with a provenance chain, is part of the MVP (23.2).
  * Recommend for every session: a certificate of signatures by a strict majority of the source (or
    destination) world's members over exactly that fact, published as attestations in that world's log. A
    relaying member's signature alone let any member mint, lose or duplicate an object.
  * Each world names, in its genesis config, the worlds it trades with (its route list), and accepts
    certificates only from them. Without route lists, a member who controls a world alone could duplicate
    or mint objects [V r14 forge; review R3-4, R3-5].
  * **Trust assumption:** every route-listed world has an honest majority. A world whose majority is
    dishonest can sign a departure of any global id, which another world accepts wherever that id is
    not live: a cross-world duplicate, or a resurrected object.
  * **Option for untrusted partners**: bind each global id to its birth world, and require a
    provenance chain. Each `Arrive` certificate then references the certified transfer, or the birth
    record, that brought the object into its source world. A destination checks the chain back to the
    birth world, which costs one extra reference per hop. **Built in the spike (P2-5, 22.7)** [V r14 forge,
    r14 chain]: mints, forged or spliced hops, refund and keep, and resurrections are refused. What remains
    is a double spend by a world of an object it legitimately held [V r14 doublespend]. **Settled (user,
    D79, 20:07Z): detect only.** It is caught by transferable fork proofs when the copies meet, and the
    cheating world can then be cut off. A per-object registry at the birth world was considered and
    rejected, not even as an opt-in (one more relay round per hop, and objects freeze when their birth
    world is unreachable) [I].
  * Cost: one attestation entry per member per transfer step, and each world must know the other's member
    keys.
  * Alternatives:
    * k-of-n with k below a majority: faster, but a colluding minority can mint;
    * a trusted carrier: MVP-only, and conservation then holds only if the carrier is honest.

* **D47. Physics at portals (22.4).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: no cross-world physics. A clearance zone around each aperture keeps colliders away, and
    straddling objects are drawn twice. Each world stays independent.
  * Alternative: couple the two sims near the aperture (cloned colliders, as in the game Portal). Worlds
    would stop being independent, a peer in only one world could not simulate it, and the R14 isolation
    rule breaks.
* **D48. Logic components on carried objects (22.8).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: they travel as inert data and regain behaviour in any world that runs their module. Shared
    schemas allow interoperation. Running foreign code needs the destination's ACL opt-in, which pins
    modules by canonical hash. Carried bytes are untrusted input: size-capped by the engine and validated
    by the destination's module (review NEW-22).
  * Alternative: carry and run the object's module wherever it goes. Richer, but every world would then run
    code its own builders never committed.
* **D49. Background worlds (22.3).**
  * **Settled (user, 18:51Z): "no limits".** No engine cap on active or loaded worlds; throttling and parking stay as mechanisms only.
  * Recommend:
    * at most 4 attending worlds and 8 loaded worlds per client, with a configurable memory budget;
    * background worlds advance in bursts of at most 250 ms;
    * a world is parked after 2 minutes unseen and unused.
    * Throttling never affects other members [V r14 throttle].
  * Alternatives: no limits (memory and CPU grow with every world opened); a hard limit of one attending
    world (Resonite-like, which falls short of R14's "actively joined to several").
* **D50. Snapshot memory knob (6.7, 6.8; user question 13:33Z and the slider requirement).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: one local setting per peer, changeable at run time, with four positions:
    * Speed: Log(4);
    * **Balanced (the default): Window(4)** with an adaptive window depth;
    * Lean: Window(8);
    * Minimum: base only, with a stall guard.
  * At the default and 60 ms one-way, the modelled CPU is the best policy's [C], by construction: Window(4)
    does Every(4)'s work and drops only images that no rollback used. Memory stays at the base plus 4-5
    images however far confirmation lags: 8.3 MB at 500 balls and 18.6 MB at 1,024 under a 116-frame
    stall, against 47.7 and 107.4 MB for 3d0f98a's Every(4) [V snap, integration run].
  * At 100 ms one-way the default needs its adaptive depth: rollbacks reach 24 frames, past the 16-frame
    window, and Window(4) then costs +21% modelled CPU under a stall and peaks at 11.5 MB [V reviewer,
    PH-2]. The adaptive depth is [I].
  * Cold images (older than the window) may also be compressed: LZ4 halves them [V reviewer, PH-3].
  * The knob never changes a result [V snap knob-moving].
  * Alternatives:
    * keep Every(4): the same CPU, up to 5.7x the memory under a stall;
    * expose k and the window depth as two expert settings;
    * a memory budget in MB from which the engine derives the policy.
* **D51. Partial (per-island) snapshots (6.9).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: not now. The public API cannot restore an island bit-exactly [V]. Take the benefit from
    region-scoped worlds first (6.6, R16), and decide on a box3d-rust fork with per-solver-set,
    copy-on-write images together with R15's fork question.
  * Alternative: fork now and snapshot per solver set. Image memory would then grow with the awake bodies,
    at the cost of maintaining the fork through every Box3D update.
* **D52. How the sim predicts a remote hand (6.10).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend:
    * Dead reckoning as the world rule for hand sources: keep the last commanded velocity, capped at
      3 m/s, for up to 6 frames, then hold. It cuts the observer's error by about 4-18x [V observer].
    * Plus render-only smoothing: τ = 50 ms below 5 cm, a glide of up to 100 ms above that, and a snap
      above 0.5 m [I].
  * Alternatives:
    * hold (3d0f98a): about 4-18x larger observer error;
    * longer coasting or curvature extrapolation: smaller error for smooth motion, larger overshoot when
      the hand stops;
    * per-world tuning of the cap and the frame count.
* **D53. Remote and late input under congestion (10.9; R18).** Settled by the user: recover, never replay.
  * **Parameters settled (user, 19:11Z): as recommended.**
  * Recommend:
    * per-tick batching, on by default;
    * the six-level controller, with hand strides 1/2/3/4/6/8 and acks every 2/2/3/4/6/8 frames, on
      shared ticks;
    * repairs that yield to the queue: backoff, small replies, no answers into a congested uplink, and
      datagrams split at 1,200 bytes (review R3-9);
    * demotion when a member's horizon trails the present by more than W/2 for 3 s (D42) [V];
    * a background catch-up budget of 8 ms per tick.
  * Alternatives:
    * fixed rates: collapse below the stream's need [V r18 capped];
    * receiver-side frame dropping: no effect on a sender's queue.
* **D54. Confirmation from stable data (6.4, 10.8; reviews NEW-1, NEW-2).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: an entry counts once every member's chain references it. This is provably convergent, and
    honest outages and two-faced members cannot fork anyone. It costs one more ack round of finality
    (8.5 frames instead of 4.2) [V].
  * Alternatives:
    * BFT quorum certificates (2f+1 of 3f+1): no wait for a silent member, at the price of a supermajority
      model and larger messages;
    * gossiped acks with conflict proofs, deciding on first receipt: faster, but a decision made before a
      conflict surfaces would have to be undone.
* **D55. Eviction cut (9.3, 10.8; review NEW-2).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: an equivocator is evicted once every remaining member has voted and the votes are stable
    among them. A vote counts only if it extends its voter's own published references; otherwise it is a
    provable lie and void. The cut is the longest common prefix of the valid votes' frozen heads, so no
    confirmed frame ever changes, also against colluders [V badvote, liar; review R3-1].
  * A silent or colluding voter can block the cut. That costs liveness, not safety, and the general view
    change takes over (10.8). Two colluders of four can stall a session, because no honest majority is
    left.
  * **Phase 2 (P2-4):** the view change now decides every removal. A quorum's votes set the cut: the
    deepest point that all but t of them extend, which is this rule when t = 0. A silent or colluding
    voter no longer blocks it, and members proven faulty count in no quorum. The quorum itself is D80.
  * Alternative: evict on a bare strict majority, with the cut chosen by a leader: faster, but it needs
    the general view-change protocol now.
* **D56. R18 defaults (10.9).** The controller thresholds:
  * **Settled (user, 19:11Z): as recommended.**
  * up above 40 ms of uplink queue, at most every 100 ms;
  * down after 300 ms below 10 ms.

  Production measures the queue by delay gradient, as GCC does [I]. These defaults kept a 32-64 kbit/s cap
  real-time with queues under about 100 ms, and 16 kbit/s with queues under 400 ms (review R3-9) [V].
  * Alternative: loss-based control (as in TCP Reno). It reacts only after the router queue is already
    full, the 500 ms bufferbloat this replaces.
* **D57. Logic-state snapshots (7.4; reviews L3, NEW-17).**
  * **Settled (user, 19:11Z): as recommended.**
  * The problem: 7.4 snapshots the component columns and one declared arena. But any `static`,
    `OnceLock`, `thread_local!` or heap allocation that outlives a tick lives outside the arena, and so
    does allocator metadata unless the arena is the allocator. Such state survives a rollback, so a peer
    that rolled back and one that did not diverge. Check frames hash only the arena, so nobody notices.
  * Options:
    * **(a) All of linear memory, with dirty-page diffs, hashed whole at check frames. Recommended.**
      Hidden state is then simply included, so it cannot diverge. A full image is about 1.1 MB per module
      at the default 1 MiB stack [V d1]. Diffs keep only the pages a tick wrote, and a typical tick writes
      few [I]. Hashing 1.1 MB at a check frame (every 30 frames) costs about 0.1 ms [I].
      * Cost not to miss (round-3 final review): wasmi has no dirty-page tracking [S wasmi 2.0 source],
        so finding the changed pages means hashing every page at each keyframe, about 1.1 MB per module
        per keyframe. With a fast non-cryptographic hash that is about 0.1 ms per keyframe [I], cheap but
        paid at every keyframe, not only at check frames.
    * **(b) The declared arena only, and zero all other writable memory after every tick.** Hidden state is
      then always zero at the start of a tick, so code that relies on it breaks visibly and identically on
      every peer. Snapshots are smaller (arena plus columns). The costs: zeroing about 1 MB per module per
      tick [I], and the SDK must lay memory out as read-only data, arena, then scratch, with the arena as
      the global allocator. A runtime that ever skips the zeroing reintroduces the silent failure.
    * **(c) Keep the arena-only rule with commit-time checks.** Cheapest, but unenforceable: a violation
      is silent (NEW-17).
  * Recommendation: (a), because it is correct by construction, and dirty pages keep its cost close to
    (b)'s.
* **D68. Which physics layers ship, and when (27.2; R15).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend:
    * phase 3: layer-1 hooks and materials and mixing, on box3d-rust's existing callbacks;
    * phase 4: contact modification, per-substep fields, and custom joints, shapes and integrators, in the
      shared box3d fork;
    * layer 3: engine-provided alternatives only;
    * layer 4: allowed, with a body budget (D70).
  * Alternatives:
    * guest-authored subsystems (layer 3 in wasm): the boundary is crossed per pair or contact row, and a
      broken solver stalls every player in the world;
    * no layer 4: simpler, but it falls short of R15's "completely replace".
* **D69. How hooks run (27.3, 27.5).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: batched per step, one guest call per hook kind with arrays in guest memory: under 2% of
    the step even under wasmi [V parts]. This needs the fork's two-pass hook points; until then hooks run
    one call per contact.
  * Alternative: one call per contact. A simpler guest API, but 7-12% of the step under wasmi.
* **D70. Budget for a replacement engine (27.5).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: a world running a guest engine declares an active-body cap, enforced by the engine: about
    48 under wasmi and about 200 under Cranelift at 60 ms one-way [I; review R15-4]. The world shows a
    warning, and new bodies are refused once the cap is reached.
  * Alternatives:
    * no cap: the world's own peers pay, and slow peers stall confirmation;
    * a CPU budget per tick, with deterministic fuel per frame instead of a body count: precise, but fuel
      depends on the build (13.3).
* **D71. Who may change physics (27.1).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: separate ACL rights, `physics.hooks` (layers 1-2) and `physics.replace` (layers 3-4), both
    granted by the world owner. Changes are commits with an activation frame, and reverting is a new
    commit.
  * Alternatives:
    * one `physics` right: simpler, but anyone allowed a one-way platform may also replace the engine;
    * physics changes only between sessions: safer, but not live-editable as R15 asks.
* **D72. Two players on one object, and "an author never sees its own interaction corrected" (8.4, 6.11;
  review R3-3).**
  * **Settled (user, 18:56Z): option (a), only grabs can lose.** When two grabs of one object land within one latency, the loser's grab is undone; every other physical interaction is never refused (8.4).
  * The problem: claims (8.4) and the per-frame tie rank decide which of two touches on one object wins.
    The loser's touch is refused on re-simulation, so a player who touched an object and saw it move can
    see that touch undone one latency later. That breaks the 14:31Z rule. R14 relies on it too: a carry
    through a portal loses to another member's earlier grab (22.7, the abort row).
  * Options:
    * **(a) Claims for grabs only.** A plain push is never refused: two pushes both apply, physically. A
      grab race, two grabs within one latency, still undoes the loser's grab. That is the one stated
      exception.
    * **(b) No refusal of any physical interaction. Recommended.** Touches and grabs both apply. A grab is a
      soft constraint, so two holders pull against each other (tug of war), and physics resolves it.
      Exclusive holding becomes a rule a world's logic can opt into for chosen objects (a single sword),
      with the exception stated for those objects. Departing through a portal stays exclusive by nature,
      because an object can be in only one world: a carry can still be undone when another member held
      the object first. That is the only engine-level exception.
    * **(c) Keep claims as they are,** and state the exception in 8.2 and 6.11, next to demotion: the loser
      of a claim race sees its own touch undone after one latency.
  * Recommendation: (b). It meets the rule for every physical interaction, and it fits the core goal
    ("every reaction physically realtime and reactive for every player"). Costs:
    * tug of war by default, and grab springs to tune;
    * **anyone can pull a held object out of another player's hand.** That griefing is what claims
      originally prevented (round 1, M3). A world that cares turns on exclusive holding for those objects,
      or limits who may grab (the ACL);
    * exclusive tools need world logic.
  * Either way, two players acting on one object in the same frame each see their own action at once and
    the combined result one latency later (6.11). That is physics, not a refusal.
* **D73. Thinned hand entries and the author's own hand (10.9; review R3-10).**
  * **Settled (user, 19:11Z): as recommended.**
  * The problem: under congestion the controller thins the hand stream to one entry every k frames, with a
    `span`. Every peer, the author included, applies the same entries, so the author's physics hand trails
    its own controller [V reviewer]:
    * 65 mm mean and 98 mm p95 at 32 kbit/s;
    * 150 mm p95 at 24 kbit/s;
    * up to 686 mm at three times the sweep speed.

    That is input delay that depends on the network, which D41 and 6.11 rule out. In VR the hand drawn
    would separate from the hand pushing the balls [I].
  * Options:
    * **(a) Pack k per-frame targets into one entry, sent every k frames. Recommended.** Each target applies
      at its own frame, so the author's hand is exact. Receivers get the targets up to k - 1 frames late,
      and pay with deeper rollbacks. Costs:
      * about 12 bytes per extra target, about 50 B per entry at k = 8, next to 52 B of datagram overhead;
      * the author's horizon must stay below the pack's first frame until the pack is sent, so
        confirmation lags by up to k frames, but only under congestion;
      * a pack may be stamped up to k - 1 frames before its author's present, a chain-local rule that
        replaces the horizon slack for packs;
      * so, under congestion, **the backdating window (8.2) and the room to pick a frame where `tie_rank`
        favours the author (8.3) widen from about 4 frames to about 9 at k = 8** (k - 1 plus the slack)
        [I, round-3 final review]. Only hand-target packs get the wider window; other entries keep the
        4-frame bound.
    * **(b) Keep thinning as it is:** the author's hand lags by up to k frames, only under congestion.
      Simplest.
    * **(c) Never thin the author's own hand below a stride of 2, and demote earlier instead:** the lag
      stays at most one frame, but the bandwidth floor rises (10.9).
  * Recommendation: (a). It keeps the author exact at every bandwidth the session survives, and moves the
    cost to receivers, which already pay for lateness (8.2).

* **D74. When a peer's build does not match (13.2; raised by D11).**
  * **Settled (user, 19:58Z): (a), with (b) as an opt-in.** On a mismatch the peer pauses the world at the activation frame and the author gets a report; the peer may choose to run the author's module for that commit.
  * The problem: every peer builds every commit, and a peer whose module differs from the author's hash
    cannot run it without diverging.
  * Options:
    * **(a) That peer pauses the world at the activation frame** (as for a missing artifact, D17) until
      the cause is fixed: a toolchain repair, or a commit that builds identically. The world reports a
      reproducibility failure to the author. Nobody ever runs code it did not build.
    * **(b) As (a), but the peer may opt in to the author's module for that commit,** verified by hash and
      sandboxed like any module. It plays on at once, giving up "compiled it myself" for that commit only.
    * **(c) The majority's build wins:** peers whose build differs take the majority's module. Quicker,
      but a peer then runs code it did not build without choosing to.
  * Recommendation: (a), with (b) as an explicit per-peer opt-in. Once builds are reproducible across
    machines (13.2), a mismatch is a bug to fix, and should be rare.
* **D75. Where the pinned toolchain comes from (13.2; raised by D11).**
  * **Settled (user, 19:58Z): (a).** Peers share the toolchain as content-addressed files verified against hashes pinned in the engine release, with the official Rust distribution as the fallback.
  * Every peer needs rustc and the wasm32 std, about 1 GB [I].
  * Options:
    * **(a) Peers distribute it among themselves** as content-addressed artifacts, verified against
      hashes pinned in the engine release, with the official Rust distribution as a fallback source. No
      server of ours, and no single point every player depends on. Recommended.
    * **(b) Always from the official Rust distribution** (rustup's servers): simplest, but a soft central
      point outside our control.
    * **(c) Bundled with the engine download:** always present, but the download grows by about 1 GB, and
      every toolchain update becomes an engine update.
  * Recommendation: (a).
* **D76. What triggers the fallback from 90 to 60 Hz (5; raised by D4).**
  * **Settled (user, 19:58Z): (a).** The world owner switches between 90 and 60 Hz, warned by the engine when members' machines cannot keep up.
  * A world's rate must change for every member at the same frame, as a logged world setting.
  * Options:
    * **(a) The world owner chooses,** changing the setting like a commit, at an activation frame. The
      engine warns the owner when members report re-simulation over budget. Predictable, and nobody's
      slow machine changes a world's physics. Recommended.
    * **(b) Automatic:** when members' reported overload persists, the world switches to 60 Hz at a
      logged frame. Self-tuning, but one weak or dishonest peer can push a world to 60 Hz, and physics
      changes mid-session.
    * **(c) Fixed when the world is created.**
  * Recommendation: (a).
* **D77. Voice input on Linux (21.2; raised by D20 and D33).**
  * **Superseded (user, 21:51Z):** Linux gets the same built-in local speech model as Windows (D33). The 19:58Z answer, no voice input on Linux in the MVP, no longer holds.
  * Linux has no OS speech-recognition service like Windows'.
  * Options:
    * **(a) No voice input on Linux in the MVP;** text through the VR keyboard. Recommended for the MVP.
    * **(b) A local speech model** (Whisper-class) on a pure-Rust ML runtime: 100% Rust and offline, but a
      download of hundreds of MB, and GPU or CPU load next to the simulation.
    * **(c) A separate non-Rust speech service on Linux,** as a 20.1 boundary.
  * Recommendation: (a) now, (b) later.
* **D78. A default hub for the automatic Hubchat identities (21.7; raised by D32).**
  * **Settled (user, 19:58Z): (a), no default hub.** The automatic identities register once the player adds a hub.
  * Every player and panel gets a hub identity at first launch, minted locally. With no hub built in, it
    reaches Hubchat users only once the player adds a hub.
  * Options:
    * **(a) No default:** the player adds a hub (an address, an invite or a QR code), and the identities
      register then. No central point, as D9, D35 and D32 intend. Recommended.
    * **(b) A preconfigured default hub** (Orgtree's or ours): identities register at first launch and
      work out of the box, but every player then depends on that hub for the bridge.
    * **(c) A list of community hubs** to choose from at first use.
  * Recommendation: (a), with (c) as a convenience once community hubs exist.
* **D79. A double spend by a world that legitimately held the object (22.7, D46; raised by P2-5).**
  * **Settled (user, 20:07Z): detect only.** A world that sends one object to two others is caught by
    transferable fork proofs when the copies meet, and it can then be cut off. A per-object registry at
    the birth world was considered and rejected, not even as an opt-in: it costs one more relay round per
    hop, and objects freeze while their birth world is unreachable.
  * The reviewer's follow-up P25-2 (each hop carrying the destination's verdict certificate) is not
    taken: @provenance-eng measured that it would turn refund-and-keep from refused into detect-only.
    The hop stays the source's commit certificate, and 22.7 states the remaining risk: a dishonest or
    vanished source world can stop an object it handed over from leaving its new world again. That is
    griefing, not duplication.
* **D81. Speech models: runtime, size and where they run (21.11; raised by the 21:51Z voice ruling).**
  **Ruled 08:31Z: as recommended.** A spike measures accuracy, time to the first word, and CPU and GPU
  cost before the runtime, model size and processor are chosen.
  * Ruled so far: one local Whisper-class speech-to-text model on Windows and Linux, shipped in the
    installer (21:51Z; 22:23Z, Q13). Local text to speech is assumed (22:02Z).
  * Options [I; nothing measured]:
    * **Runtime:** `candle` (pure-Rust CPU kernels), `burn` (wgpu, so the GPU), or `tract` (ONNX on the
      CPU). Each needs the section 20 audit. `whisper-rs` is out: it compiles whisper.cpp.
    * **Model size:** a small or "turbo"-class model, quantized, at a few hundred MB in the installer.
      Against it: tiny or base (under about 150 MB, less accurate), or large (about 3 GB at 16 bits).
    * **CPU or GPU:** the CPU by default, off the sim and render threads; the GPU once measured against
      VR frame times (18.3).
  * Recommendation: decide after a spike that measures accuracy, time to the first word, and CPU and GPU
    cost on the user's machines with the ball pit and a VR-sized render load running.
* **D82. Spatial context at shared panels, and what reaches the model provider (21.12).**
  **Ruled 08:31Z: as proposed.**
  * Ruled: panels know where the player looks, what they look at and where they point (21:52Z); eye
    tracking and the live feed are off by default (Q8, 22:28Z).
  * Proposal: a guest's or confidant's turn carries only that sender's own gaze and pointing, sent by the
    sender's peer and limited by the sender's settings; the owner's never goes with another person's turn.
    The context reaches the harness, and so its model provider, as text: object ids and names,
    coordinates, and the player's position and scale.
  * Alternative: a panel always carries its owner's context, whoever prompts. Simpler, but it shows the
    agent, during a guest's turn, what the owner is looking at.
* **D83. Guidance annotations: lifetime, and who may clear them (21.13).**
  **Ruled 08:31Z: as proposed.**
  * Ruled: shown to others by default, can be made private (Q9, 22:28Z).
  * Proposal: an annotation lives until its author clears it or leaves; "keep" pins it across prompts; it
    is never saved with the world; admins may clear others' annotations; anyone may hide others' from
    their own view.
  * Alternatives: annotations expire after some minutes unless kept; or they can be saved with the world
    as notes.
* **D84. Which settings an agent may change without the owner's confirmation (21.14).**
  **Ruled 08:31Z: as proposed.**
  * Ruled: the agent changes game settings when asked (22:03Z); guests cannot change the owner's settings
    (Q12, 22:28Z); confidants can (22:34Z).
  * Proposal: everyday settings (graphics, audio, comfort, input, culling) apply at once, with undo.
    Confirmation for sensitive sensing (eye tracking, the live context feed) and for what is already an
    approval item (unlocking, the access level, the connection policy and ceiling, the tool profile,
    adding a hub, spend caps). Sharing settings need none, because openness is the default.
  * Alternative: every change an agent makes to a setting needs a confirmation. Safer, slower, and against
    the intent of changing settings by talking.
* **D85. Where an agent's spoken reply is synthesized (21.11).**
  **Ruled 08:31Z: (a), on the panel owner's peer.**
  * Ruled: replies are spoken to people nearby by default (Q15, 22:23Z).
  * Options:
    * **(a) On the panel owner's peer,** streamed to listeners through the voice path (26): one voice for
      everyone, one machine pays for the model, and the bandwidth of a person speaking. Recommended.
    * **(b) On each listener's peer,** from the reply text the open panel already sends: no audio
      bandwidth, but every listener runs the model for every panel nearby, and the voice may differ with
      each listener's model version.
* **D86. What an open panel shows other people (21.4; the 22:28Z principle).**
  **Ruled 08:31Z: the alternative, (b).** An open panel streams everything, the detailed tool view, raw
  output and file contents included. The best-effort secret filter remains only a proposal, since it
  narrows (b).
  * Ruled: open by default, so people nearby see and hear prompts, replies, guidance and the agent's work
    (22:28Z); a private-panel switch, off by default (Q20, 22:54Z); six things stay closed (Q19, 22:34Z).
  * Proposal: others get the conversation, not the raw work: the agent's prose, the collapsed tool
    summaries (21.2), the objects the turn touches and the current step. Raw tool results and arguments,
    file contents, shell output and diffs never leave the owner's peer, and outgoing text passes a
    best-effort secret filter.
  * Alternative: stream the detailed tool view as well, to "watch them work" in full. More open, but it
    sends file contents and command output, with whatever secrets they hold, to everyone nearby.
* **D89. Which tools a confidant's turns get (21.5; raised by the 22:34Z confidant ruling).**
  **Ruled 08:31Z: (b), full as ruled.** A confidant's turns get exactly the panel's own profile, shell
  and web included on a Full panel, with no Restricted-by-default setting; the Seal keeps its plain
  warning.
  * Ruled: confidants get "full access to your prompts as though they were you" (22:34Z), and do not
    confirm the owner's approval cards (Q22, 22:54Z).
  * Options:
    * **(a) "Confidants use my panels in Restricted mode", ticked by default:** on a Full-profile panel a
      confidant's turns get file and engine tools only, with no shell or web; the owner can untick it.
      Recommended, because a Full panel's shell runs commands on the owner's computer as the owner.
    * **(b) Full, as ruled:** a confidant's turns get exactly the panel's own profile.
  * Whatever the answer, a confidant can prompt only while the owner's harness runs, on the owner's
    machine with the panel open [I: this follows from where the harness runs].

* **D80. Who may change a session's member list, and how many must agree (10.8, 12.4; raised by P2-4).**
  **Settled 22:13Z: admins over a two-thirds safety layer.**
  **Refined 08:31Z (V1-V4):** kicking an admin is its own permission, held by default only by admins
  (V1); the roles permission counts as Admin (V2 = (b)); when the last admin times out the session
  closes, removing every other player, telling everyone and allowing no further change to the world,
  with no longer grace period or handover (V3); a member that keeps forcing resets is left to admins
  (V4 = (a)). 12.4 and 10.8 carry them.
  * The user answered with session admins (22:11Z): admins may kick anyone; there is always at least one
    admin, and the last cannot give the role up without handing it on; every player is an admin by
    default; the session closes when the last admin leaves.
  * Confirmed reading (22:13Z): an admin's signed kick authorises a removal, with no vote. The
    two-thirds quorum, option (b), stays underneath only as the commit and ordering rule. Conflicting
    kicks resolve by commit order, a kick by an admin already removed is void, kicks wait out a
    partition instead of forking, and silence or crash removal stays automatic. A last admin that crashes
    or times out closes the session.
  * Roles (22:14Z, 22:15Z), owners (Q27) and join rules (22:37Z-22:51Z) are in 12.4.
  * Measured [V kickwar, kickpartition, lastadmin, adminclose, joinrules; bridge and twosilent5 for the
    quorum]. The safety bound counts every faulty member, proven or not (review VC-1, 10.8) [V onesided,
    a control above the bound]. Options (a), a strict majority, and (c), per world, are not taken.

---

## 25. Massive worlds: hierarchical chunks, f64 coordinates and regions (R16)

Author: @world-chunks (branch `chunks/r16`, a9a20cc). Reviewed by @design-reviewer (45d4940) and by
@chunks-reviewer through four re-reviews, approved at db8ee73; merged at integration. Two choices change
what players experience; they are user decisions D60 and D61 (renumbered from D55 and D56 at the merge).

The region and group model (25.5) follows the session protocol of 10.8. It is designed, not built.
Measurements: [r16-chunks-results.md](r16-chunks-results.md). Tags as in the rest of the design: **[V]**
measured by the spike in `spikes/chunks`, **[S]** read in source, **[I]** inferred or proposed.

R16 (user, 14:08Z, plus 14:09Z, 14:11Z and 14:12Z):
* raw coordinates and transforms are doubles;
* the world is a hierarchically sized chunk system, top-level chunks about 1 km wide, like the Open World
  Orchestrator (OWO) but built into the engine;
* players can scale arbitrarily up and down;
* objects far away are not simulated, depending on their size, as OWO culls by distance, so that dozens
  or hundreds of players can share a session without slowing each other down as long as they are spread
  out;
* maybe an octree on the chunk system decides locality;
* the performance impact must be measured.

Ruling 14:20Z: Box3D's slop and speculative tolerances stay absolute. Small or large worlds use Box3D
length units, per world or per region.

### 25.1 Summary

* **Canonical coordinates** are a level-0 chunk index (`i64` per axis, 1,024 m chunks) plus an `f64`
  offset inside the chunk. That covers ±2^73 m (about a million light-years) at 1.1e-13 m (2^-43 m)
  resolution everywhere. Transforms are `f64` position plus `f32` rotation and scale, the same split as box3d's
  `double-precision` mode (which makes only positions `f64`; AABBs, velocities, rotations and contact
  anchors stay `f32` [S]).
* **No per-user floating origin in the simulation.** OWO's "controlled desynchronization" gives every user
  a different frame. That cannot coexist with one bit-exact shared world line, so the sim stays canonical.
  The floating origin survives in two places that do not need to agree across peers: rendering (camera-
  relative) and each region's physics origin (deterministic, so every peer computes the same one).
* **Hierarchical levels.** A level-`L` cell is `1024 * 2^L` m wide and corner-aligned, so cells nest
  exactly. Every object is filed at the level whose cell matches its relevance distance (OWO files objects
  in chunks about 8 times their size; we use the relevance ratio).
* **Relevance decides what is simulated.** An object is simulated when:
  * some player is within `rho * r` of it (`rho` = 32 by default, a slider), it is not tiny relative to that
    player (`r >= h / 64`, `h` = player height), and it is inside the reach cap;
  * or it is awake, woken, or supporting something that is simulated.

  Everything else is dormant: frozen exactly, costing memory only.
* **Regions.** The simulated part of the world splits into regions, each its own box3d world with its own
  origin, length unit, keyframes, log and group of peers. A late entry rolls back one region, not the
  world. Peers simulate only the regions they are in.
* **Two-tier log.** A world log every member holds (membership, ACL, SCM, the region directory), and one log
  per region held by that region's group, which carries the members' anchors. The world log touches a
  region's per-frame path only through the region directory, with 10 s of slack (25.5.2).
  * Regions depend on each other only through a speed-bounded **neighbour bound**, so a stalled member far
    away never stalls a region [C: modelled].
  * Region state is a function of dormant records plus the region log.
  * No decision depends on local arrival time (the NEW-1 lesson).
  * Membership-dependent values are evaluated at the entry's frame and frozen at confirmation (the NEW-2
    lesson).
* **Arbitrary scale** uses per-region length units (ruling 14:20Z): a region simulates in units of its
  players' scale (`2^round(log2 s)`), so box3d always sees sizes near 1 unit. [V: a mixed-size stack (s/20 to
  2s) is stable from 0.1 mm to 10 km in region units; unscaled it fails below 1 cm.]
* **Headline measurements** (details in 25.8-25.11 and the results doc):
  * box3d `double-precision` costs 2-10% of step time and 1-2% of image size [V];
  * with a region-local origin, physics 100,000 km out behaves as at the origin [V];
  * a late entry rolls back one region: 0.7 ms instead of 10.5 ms for the 16-player world [V];
  * spread-out players cost each other nothing in CPU or memory: 0.12 ms per tick and 1.5 MB per player at
    2 or 256 players
    [V], against about 300 ms (282-635 ms across runs) and 368 MB unchunked at 256 [C, from measured
    region costs];
  * the hashed multi-level grid beats both octrees on query time, build time and memory, and matches them on
    update time [V];
  * box3d-rust ran validation loops in release builds, and creating N bodies cost O(N^2); fixed in
    `box3d-fork` (e1164ff) [V].

### 25.2 Coordinates and precision

| Representation | Step at 1 km | at 100 km | at 10,000 km | at 1 ly | 1 h of walking at 10,000 km |
|---|---|---|---|---|---|
| `f32` absolute | 61 um | 7.8 mm | 1 m | 1.07e9 m | stuck (step rounds to 0 beyond 524 km) |
| `f64` absolute | 1.1e-13 m | 1.5e-11 m | 1.9e-9 m | 2 m | 38 um error |
| chunk + `f32` offset | 61 um | 61 um | 61 um | 61 um | 17 cm error |
| **chunk + `f64` offset (proposed)** | 1.1e-13 m | 1.1e-13 m | 1.1e-13 m | 1.1e-13 m | **3 nm error** |

[V `precision`; exact IEEE arithmetic, so identical on every machine.]

* **Rules (extend 4.3):** coordinate code uses only `+ - * /` by powers of two, `floor`, integer shifts and
  `f64 <-> i64` conversions. Cell keys and levels come from bit patterns (`ceil_log2` reads the exponent),
  never `log2`. Rebasing to a nearer origin is exact.
* **Where `f64` lives:** object transforms (position relative to parent; roots relative to their chunk),
  dormant records, carried state, the renderer's camera maths. **Where `f32` is enough:** inside a region's
  box3d world (positions within a few km of the region origin), rotations, velocities, render vertex data
  after camera-relative conversion.
* **Below 1e-10 m or beyond 2^73 m** a parent object acts as a nested frame: child transforms are `f64`
  relative to it, so precision never runs out; regions for such players take their origin from that
  frame.
* **Transforms as built (P2-1)** [V P2-1]:
  * A root is placed by its `i64` chunk plus an `f64` offset from that chunk's corner. A child is placed
    relative to its frame (its nearest ancestor with a `Transform`) and carries no chunk.
  * A child's precision is its offset's ulp: about 1e-13 m at 1 km, 1e-10 m at 1,000 km, and 1 um out to
    about 4e9 m. Keep children within about 10^6 km of their frame; far content belongs in its own root.
  * Limits on the stored form, checked on insert, at check frames and on loading bytes: `|pos| <= 2^50` m,
    a root's `|chunk| <= 2^61`, scale magnitudes in [2^-40, 2^40], and a rotation's squared norm within
    2% of 1.
  * **Plain edits are data edits:** inserting or removing a `Transform`, and `set_parent`, store exactly
    what they are given, so a loader gets the same state whatever order it inserts in. **Keep-world edits
    are explicit** (`set_parent_keep_world` and its siblings): they rewrite the affected local transforms
    for the new frame, with the root's chunk recomputed exactly. 150 keep-world moves between rotated,
    scaled frames 5 km apart stay within 1e-8 m.
  * `maintain_transforms`, which a schedule adds as its last system, rebases roots into their chunk and repairs
    out-of-limit values written through mutable access, so the state stays loadable by joiners.

### 25.3 Hierarchy and the spatial index

* **Cells:** level `L` cell = `1024 * 2^L` m, key `(L, i128 x3)`, `cell(L + 1) = cell(L) >> 1` exactly.
  Levels -43..+63 span from cells of about 1e-10 m to the whole coordinate range. Finer levels are
  meaningful only inside a nested frame (25.2).
* **Filing:** an object of radius `r` lives at the smallest level whose cell is at least `rho_file * r`
  wide, keyed by the cell of its centre.
* **Index (user suggestion 14:12Z, measured in 25.9):** the recommendation is a **hashed multi-level grid**
  (an implicit loose octree, where the level plays the role of tree depth). The relevance query reads the
  3x3x3 neighbourhood of each occupied level, so its cost does not depend on scale.
* **The index is a cache, not sim state.** Query results are sorted by object id, so the index's internal
  layout may differ between peers and is never snapshotted or rolled back [V: identical results after
  building in shuffled order].

### 25.4 What is simulated: relevance, support, wake and freeze

An object `o` is **active** at a relevance pass when any of these rules holds; otherwise it is **dormant**.
Passes run every `P` = 8 frames, a deterministic cadence. (Rules revised after review R16-2.)

1. **Relevant to an anchor.** For some member anchor `a` (position `p`, height `h`, predicted over the
   next pass), `r_o >= h / 64` and `|p - o| <= min(rho * r_o, reach_cap(h))`. By default
   `reach_cap` = 2 km x `h / 1.7 m`.
2. **Awake stays active.** An active body that is awake stays active until it falls asleep, or until it
   leaves its region's footprint (the hard reach; review N6). A coin knocked off a table lands; it does not
   freeze in mid-air.
3. **Woken.** An object inside the swept bounds of an awake active body no more than 16 times larger
   (`r_o >= r_body / 16`) becomes active. Things you push, throw or knock over wake what they hit.
4. **Support closure** (revised after review N7).
   * **First hop, simulated.** A dormant object whose bounds touch a body active by rule 1, 2 or 3 is
     included, however small it is. The bounds are swept by that body's velocity over the coming pass.
   * **Second hop, static proxy.** Objects touching that first hop are added as static proxies: they
     collide but are not simulated. A proxy becomes simulated when it is itself in a first hop.
   * The stones under a relevant crate are therefore simulated, and the stones under them hold them up,
     without waking the whole heap.

Measured [V], `closure` scene: a 0.9 m crate on a bed of 3 cm stones and a coin knocked off a table, with
the player walking in from 20 m.

| Rule set | Crate | Stones left inside the crate | Coin | Bodies active (max) |
|---|---|---|---|---|
| Rule 1 + wake only (as of 45d4940) | falls 6 cm through the dormant stones to the ground | 81 | frozen in mid-air at 0.94 m | 49 |
| Rules 1-4 (one hop) | rests on the stones (settles 2 cm as they shift) | 0 | lands on the floor | 134 |

Measured [V], `closure2` scenes, each with an all-dynamic control (what physics alone does):

| Scene | All-dynamic control | One hop, static bounds (2a228c6) | Swept first hop + static second hop (revised) |
|---|---|---|---|
| 20 cm crate set on a 3-layer heap of 5 cm gravel: crate height (rests at 0.251) | 0.242, no gravel displaced | **0.197**: falls a layer; 68 gravel cubes sink | **0.243**, none sink; 124 static proxies |
| The same crate thrown at 2 m/s onto 1.2 cm gravel (below r/16): final height (rests at 0.112) | 0.106, 16 gravel inside the crate | 0.105, 2 inside | 0.106, 21 inside |

* **The heap shows the second hop matters.** Without it, the first hop's gravel falls into the space where
  the dormant layer below should be.
* **The thrown case is decided by the solver, not by the rule.** Gravel under 1/16 of the crate's size has
  under 1/4,096 of its mass. Box3D lets the crate sink into it even when everything is simulated, and all
  three variants end within 1 mm of each other.
* A first attempt with a 0.9 m crate on 2.5 cm gravel (a mass ratio of 47,000) sank to the ground even in the
  control, so it was replaced.

* **Dormant means frozen exactly.** Position, rotation and velocities are kept bit for bit in a dormant
  record: about 100 bytes, against about 1.9 KB per body in a box3d image [V]. Time does not pass for
  dormant objects. When something becomes active again it continues from that exact state. That state is
  canonical, because every peer froze it at the same frame by the same rules.
* **Logic catch-up.** Logic modules get `on_wake(chunk, frames_elapsed)`, a deterministic closed-form hook
  (crops grow, timers expire). Physics is never fast-forwarded.
* **Unmodified content is not stored at all.** World content comes from the SCM (committed chunk files) or
  from deterministic procedural generation per `(generator hash, cell)`. Only objects that changed carry
  a dormant record.
* **Why size-based.** With OWO-style whole-chunk activation, everything within 1 km is simulated: about
  2.5e7 objects in the streaming test's content [C]. With relevance it is 330-640 bodies, of which 1-80 are
  awake [V].

### 25.5 Regions and groups (who simulates what) [I; the prototype measures costs and a model of the dependency rule, not the protocol]

For large worlds, this replaces "every peer simulates the whole world" (2, 6). With one region it reduces
to today's design. Revised after review R16-1 and R16-3: the world log is no longer on any region's
per-frame path.

#### 25.5.1 Two tiers of log

| | World log | Region log (one per region) |
|---|---|---|
| Held and acked by | every member | the region's group |
| Carries | session membership, ACL, SCM commits, frozen-chunk record hashes, a region directory (informational), cross-region messages | perturbations of objects in the region (touches, hand and body streams, component writes); the members' **anchors**; horizons; check hashes; `Synced` declarations; `NeighbourRef` entries |
| On a region's per-frame path? | **Only through the region directory, with `S_create` (10 s) of slack.** Its other facts enter a region as relayed confirmed facts at the relay entry's frame, as in R14, so a world-log stall delays ACL and SCM effects. Simulation stalls only if world-log confirmation lags more than about 9 s (25.5.2) | yes, for that region only |
| Confirmation | every member, compact acks, slow cadence | the region's synced group members |

Both logs follow the round-2 protocol (design 10.8, @engine-architect): acks, votes, horizons and check
hashes are entries in each member's hash chain. Each log has its own chains, stability, confirmed frame,
epoch and ack cadence.
* A log depends on another log only through a confirmed fact of that log, referenced explicitly. No log
  reads another log's unconfirmed state.
* There is one chain per member per log. An eviction from a region propagates to the world log as a
  world-log view change.

#### 25.5.2 Anchors, footprints and the neighbour bound

Revised after review N1, N2 and N8. Footprints change in steps, so outsiders must never see one grow
faster than `v_max`.

* **Anchor and home region.** Each member has an anchor: the position and scale that drive activation (its
  avatar root). Each member belongs to exactly one region's group, its **home region**. Its anchor entries
  live in that region's log:
  * a coarse `Anchor{chunk, scale_exp}` when it nears a chunk boundary (with hysteresis);
  * the fine quantized position, at 4-10 Hz while it moves.
* **`Lambda` = 30 frames.** That is above the region log's measured confirmation lag of 8.5 frames (at most
  9-10 frames at 256 kbit/s; ed347ac 6.4).
  * If an anchor is still unconfirmed when it takes effect (for example during a stall), its own region
    simulates speculatively and rolls back if needed, as for any entry.
  * Other regions only ever use confirmed data (below).
* **Footprint.** The level-0 chunks within `ceil(reach_cap / 1024) + 1` of the anchor chunk, at the member's
  scale band, so a radius of at least 3 chunks at human scale. A region's footprint is the union over its
  members.
* **Speed bound and the growth clock** (reviews N1, M3, P2, Q1, Q2). A footprint may approach another
  region by at most `v_max` (default 100 m/s at human scale, 25.6) over time, plus a capped instant step.
  * Every change that grows a region's footprint takes effect at
    `max(stamp + Lambda, previous growth's effect frame) + ceil(60 J / v_max)`, where `J` is its clocked
    part (below).
  * **`G_inst`**, the instant-growth cap, is 4 chunks (4 km) at human scale and scales with the band's unit,
    as `v_max` does. A region makes at most one instant growth per `T_h`.
  * **Known regions at stamp `s`** (review Q2): those in the region's own log (its `NeighbourRef`s and
    relays) **plus every region in the world-log directory confirmed through `s - S_create`**. Every
    member holds that directory before simulating `s` (the directory gate), so the set is deterministic,
    and it contains every region that exists at `s`.
  * **Possible footprint** of a known region `q`: its envelope as the log holds it at the stamp, grown by
    `v_max(q)` x (the envelope's age + `T_h`) **plus `G_inst(q)` x `ceil((age + T_h) / T_h)`**, the instant
    growth `q` may make in that window (review Q1). `T_h`, the horizon, is 60 s.
  * **`J`** (revised after reviews M3, P2 and Q1):
    * **Instant part.** The first `G_inst` of a growth takes effect at `stamp + Lambda` if every point it
      adds is at least `D_merge` from every known region's possible footprint (margin 0 for a region of
      another band, 25.6), and the region made no instant growth in the last `T_h`. That covers a joiner,
      warp or respawn at an edge facing open country, and the first 4 km of a scale-up.
    * **Clocked part.** Everything else is clocked, with `J` its extension of the union: a growth that fails
      the test, the part of any growth beyond `G_inst`, and a second instant growth within `T_h`. So a step
      straight at a close neighbour is always clocked (review P2), and a region's outward speed is at most
      `v_max + G_inst / T_h` in every direction, toward known and unknown regions alike.
    * Every member computes the same split, because it depends only on the region's log and the confirmed
      directory at the stamp.
  * **The horizon gate.** A region simulates `f` only while `f <= known_conf_q + T_h` for every known
    region `q` whose possible footprint lies within `D_merge + v_max x T_h + 2 G_inst` of its footprint
    (both at this region's band; margin 0 instead of `D_merge` across bands): about 17 km at human scale.
    It is the 60 s pull made binding, so in practice it binds only when `q` has been stalled for 60 s.
    * For regions further away, the neighbour bound counts `q`'s instant growth instead: `q`'s reach after
      its known confirmed frame is `v_max(q) (f - known_conf_q)` plus `G_inst(q)` per started `T_h`. At
      that distance this costs slack but never gates.
    * Cost: one prefix pull a minute from each region within about 17 km, about 1 kB each [I].
  * Why the instant part is then safe [I, modelled below]:
    * it lands at least `D_merge` from `q`'s possible footprint, which bounds where `q` can be, instant
      growth included, until `T_h` after the stamp;
    * by the horizon gate, `q` cannot simulate past that without holding this region's prefix through the
      stamp; regions beyond the gate radius bound themselves with the instant growth counted;
    * two regions growing at once each count the other's possible instant growth, so their instant parts
      cannot meet unseen (review Q1). A region created too recently to be known is covered by the
      creation buffer below.
  * The growth clock is per region and runs in log order, so growths queue rather than add up. A merged
    region continues with the later of the two clocks and the more recent of the two instant-growth stamps;
    split children inherit the parent's clock and instant-growth stamp. So neither resets the one-per-`T_h`
    budget.
  * **What it costs players** [C from the constants; the experience is I]. The footprint is the area whose
    objects are simulated, not the player. A scale-up, join or step itself happens at once: the avatar,
    its colliders, held objects and everything inside the old footprint (at least 3 km around every
    member) change and simulate at the same frame. Only objects beyond the old footprint that the player
    is now entitled to stay **dormant (frozen in place, still drawn)** until the footprint reaches them:
    * a chunk crossing toward a neighbour takes about 10 s, which the footprint's one-chunk margin covers if
      the member declares the next chunk at least `v_max` x `Lambda` (50 m) before the boundary;
    * a x5.5 scale-up (+9 chunks): the first 4 km at once, the remaining 5 km over about 51 s in open
      country; all of it over about 92 s within roughly 20 km of another region;
    * a x8 scale-up (+14 chunks): about 102 s in open country, 143 s near another region;
    * a joiner at an edge (+3 km): at once in open country, up to about 31 s near another region.

    Players notice this only when something beyond about 3 km of them should move during the ramp: an
    object they throw that far, or animated logic there, which catches up through `on_wake` (25.4). A
    x5.5 player's own reach is about 10 m. Options: D61.
  * Shrinking is immediate: it only widens gaps.
* **Envelope.** A region `q`'s envelope, as another region `R` sees it, is computed from every anchor
  *stamped* up to the confirmed frame `R` holds for `q`, including growth not yet in effect.
  * By the growth clock, `q`'s true footprint at frame `f` lies within that envelope grown by
    `v_max (f - known_conf_q)`.
* **Neighbour bound.** `R` may simulate frame `f` only while, for every other region `q` of its band,
  `known_conf_q + (envelope gap - D_merge) / v_max >= f`. Regions of larger bands bound `R` too, with
  margin 0 and their own `v_max` (25.6). Beyond the horizon-gate radius, `q`'s reach also counts its
  instant growth (above).
  * `R`'s members pull `q`'s confirmed prefix, with its proof (25.5.8), from `q`'s members. They pull every
    60 s, and on demand whenever the bound comes within 2 s of `R`'s present. They record it in `R`'s log as
    `NeighbourRef{q, frame, head, envelope}`.
  * **A region gated by the bound keeps publishing horizons that advance with its present** (review N2).
    Its own confirmation is unaffected, so a stall does not propagate along a chain of close regions.
* **Region creation** (a first member, or a warp into empty space) is a world-log fact (revised after
  review M1). **Provisional arrival** (D60(a), the user's 18:42Z ruling; design note
  [r16-neighbourhood-loading.md](r16-neighbourhood-loading.md), reviewed by @chunks-reviewer at 55a9e90):
  * `RegionCreated{region, footprint}` takes effect **at its stamp `s0`**, and the arriving player
    simulates from `s0`. Until its creation is confirmed valid the region is **provisional**.
  * Its validity is checked **in world-log order**, against the directory including earlier
    `RegionCreated` entries that are still pending **and valid**. It must be at least
    `D_merge + v_max * S_create + G_inst x ceil((age + S_create) / T_h)` from every one of those envelopes,
    each also grown by `v_max` x its entry's age. The `G_inst` term (review Q1) covers instant growth by a
    region that does not know the new one yet.
  * **A provisional region's frames confirm only once the world log has confirmed its creation valid:**
    about 1.1 s (66 frames) after the stamp without a world-log stall, 7.2-7.3 s in an 8 s stall [C]. Past
    the 2 s speculation limit the arriving player pauses until then. No confirmed frame is ever undone.
  * A creation nearer than that becomes a **join** of the nearest region instead, as a per-region view
    change (25.5.3). The arriving player's speculative frames are dropped, and it continues as a joiner.
    Two newcomers spawning at the same spot therefore form one region, not two. An arriving peer that can
    already see that its creation is too close joins at once, without a provisional region.
  * **Until `s0 + S_create` the region's footprint does not grow**, whatever the cause: a step, a
    scale-up, a band change, or another player joining (review S2). Its growth clock starts at
    `s0 + S_create`, the frame from which every other region holds it in the directory. Joins into a
    provisional region are provisional too, and are resolved as its creator is.
  * **Nothing leaves a provisional region:** no `Frozen` checkpoints, no `RegionHead`, no cross-region
    messages and no world-log facts that depend on its state. A handoff into it names the `RegionCreated`
    entry, which world-log order resolves to the new region if valid, or else to the region it joins.
  * Creation validity is checked within a band only; across bands the margin is 0 (25.6), made safe by
    the reader rule there.
  * Every region simulates `f` only while it holds the world log's region directory confirmed through
    `f - S_create` (the directory gate). So every region knows a new region before its footprint takes
    effect for anyone else, and from then on the neighbour bound covers growth on both sides.
  * **Why it is safe** [I, modelled below]: while its footprint is frozen and nothing derived from its
    state leaves it, every other region sees exactly what it would see if the region took effect at
    `s0 + S_create`. Its own early frames are a private speculation, and gating their confirmation on
    validity means a discard undoes only unconfirmed frames.
  * **Start condition** (added with the P2 fix). A created region `q` that takes effect at `f0` simulates
    `f0` only once it holds, for every region `R` of its band or a larger one that could reach it, `R`'s
    confirmed prefix through `f0 - 1`. For a provisional region `f0 = s0`; the pull takes about 0.2 s
    [C], during which it simulates speculatively.
    * Why: `R`'s growth stamped before `R` knew of `q` was judged without `q`. Such growth is stamped
      before `f0`, so `q` sees it before its first frame. A merge it causes happens at `f0`, which is a
      join in effect, never late.
    * With the `G_inst` cap and the creation buffer above, the model has no case that needs it (below).
      It stays as defence in depth: it costs about 12 frames (0.2 s) after `f0` and does not depend on the
      buffer arithmetic or on how fresh the `RegionHead` was. If such an `R` is stalled, `q` waits for its
      eviction (about 3 s, 25.5.3).
    * "Could reach it": `R`'s directory envelope lies within
      `D_merge + v_max (S_create + age) + G_inst x ceil((age + S_create) / T_h)` of `q`'s footprint. One
      prefix pull per such region.
  * **Spawn regions.** Each spawn point and each portal or warp destination has a region created at the
    world's genesis (in the directory from the start, possibly with no members). Arriving there is a join,
    not a creation, so two arrivals there never conflict.
  * **What it costs players** (D60(a)). Physics starts at once wherever a player arrives. If the world log
    is stalled, the arriving player pauses after 2 s until the log confirms the arrival (about 5 s in an
    8 s stall). For the first 10 s the simulated area (at least 3 km around the player) does not grow;
    that includes friends joining further out, whose surroundings wake after those 10 s. Rarely, another
    arrival or a region within about 8 km in the same second turns the arrival into a join, and the first
    second of changes is undone.
* **The world-log stall budget** (review M4). Every region gates on the directory, and world-log
  confirmation waits on every member. So **evicting or demoting a member that holds back world-log
  confirmation must finish within `S_create` minus the world-log lag (about 8.9 s).** Otherwise every
  region in the session stalls.
  * Modelled [C] with one member silent for 15 s, 64 and 256 members, the directory gate on: world-log
    eviction in 3.8, 4 or 8 s cost unrelated regions 0 frames. In 12 s, **every** region lost about 188
    frames.
  * The round-3 view change (@engine-architect, R3-2, design 10.8, branch round3-fixes at cecda4b) meets
    it for one silent member: removal is decided about 3 s after that member's last entry, plus about one
    ack round. In the spike's crash runs the session paused 72 frames (1.2 s) and eviction landed at about
    3.1-3.3 s [V, the architect's runs]. The threshold should stay at 3 s or more: a congested member was
    silent for 1.95 s without having crashed.
  * **Covered since P2-4** [V 5k]: two members silent at once, and a member that withholds or forks its
    vote. The view change (10.8) removes them together; the world-log timing at 256 members is in 10.8's
    measured table. Under D80 (b) a world log of 256 removes up to 85 members at once.
* **`D_merge`** (default 3 km at human scale) is the smallest allowed gap between two regions' footprints.
  It is not "two footprint radii": a footprint radius is at least 3 chunks.

**Safety, modelled [C]** (`pe-spike-chunks safety`, `spikes/out/chunks/safety.log`). The model uses two
regions on a line. It counts **late merges**: merge frames that a region had already simulated past when it
first held every fact that fixes them. That needs a rollback, possibly below its confirmed frame, which 6.4
forbids. Both regions may grow, and both are checked. In the cross-band cases (25.6) the event is the frame
from which a human region must read a giant's region, and only the reader is checked.

| Case | 2a228c6 | 05d3cc0 | e10c905 | da3cd74 | **Revised** | **Provisional** |
|---|---|---|---|---|---|---|
| Chunk step, no stall (gap 4 km) | safe (360) | safe (975) | safe (360) | safe (975) | safe (975) | safe (975) |
| Chunk step toward R, q stall (gap 4 km) | **late** (360) | safe (975) | **late** (360) | safe (975) | safe (975) | safe (975) |
| Chunk step toward R, R stall (gap 4 km) | safe (360) | safe (975) | safe (360) | safe (975) | safe (975) | safe (975) |
| Chunk step toward R, R stall, R walks toward q (gap 4 km) | safe (30) | safe (645) | safe (30) | safe (645) | safe (645) | safe (645) |
| Chunk step toward R, q stall, R walks toward q (gap 8 km) | safe (1,875) | safe (2,490) | safe (1,875) | safe (2,490) | safe (2,490) | safe (2,490) |
| Chunk step toward R, R stall, R walks toward q (gap 8 km) | safe (1,875) | safe (2,490) | safe (1,875) | safe (2,490) | safe (2,490) | safe (2,490) |
| Scale x8 (+14 km), pull every 60 s only (gap 10 km) | **late** (1,030) | safe (9,632) | **late** (2,418) | safe (9,632) | safe (9,632; R pauses 11 frames) | safe (9,632; R pauses 11 frames) |
| Scale x8 (+14 km), pull every 60 s or on demand (gap 10 km) | **late** (1,030) | safe (9,632) | **late** (2,418) | safe (9,632) | safe (9,632) | safe (9,632) |
| Joiner at q's near edge (+3 km), pull every 60 s only (gap 5 km) | **late** (1,030) | safe (2,874) | **late** (1,030) | safe (2,874) | safe (2,874; R pauses 2,411 frames) | safe (2,874; R pauses 2,411 frames) |
| Joiner at q's near edge (+3 km), pull every 60 s or on demand (gap 5 km) | **late** (1,030) | safe (2,874) | **late** (1,030) | safe (2,874) | safe (2,874) | safe (2,874) |
| Walker at v_max toward R, 12 chunk steps, q stall mid-way (gap 12 km) | safe (5,250) | safe (5,865) | safe (5,250) | safe (5,865) | safe (5,865) | safe (5,865) |
| Chunk step every 5 s from 20 km toward R, q stall at frame 1500 | safe (5,030) | safe (10,685) | safe (7,610) | safe (7,535) | safe (10,370) | safe (10,370) |
| Chunk step every 5 s from 20 km toward R, q stall at frame 4000 | safe (5,030) | safe (10,685) | safe (7,610) | safe (7,535) | safe (10,370) | safe (10,370) |
| Region created 4.5 km from R, then walks toward it | **late** (1,945) | joins | joins | joins | joins | joins (decided after 66) |
| Region created 3.5 km from R, then walks toward it | **late** (1,330) | joins | joins | joins | joins | joins (decided after 66) |
| Region created 4.5 km from R and walks toward it, world log stalled 5 s | **late** (1,945) | joins | joins | joins | joins | joins (decided after 200) |
| Region created 4.5 km from R and walks toward it, world log stalled 12 s | **late** (1,945) | joins | joins | joins | joins | joins (decided after 620) |
| Both walk toward each other, q created at the closest allowed gap, v_max 100 m/s, S_create 10 s | **late** (645) | safe (1,260) | safe (1,231) | safe (1,846) | safe (3,076) | safe (3,076; valid after 66) |
| Both walk toward each other, q created at the closest allowed gap, v_max 100 m/s, S_create 10 s, world log stalled 8 s | **late** (645) | safe (1,260) | safe (1,231) | safe (1,846) | safe (3,076) | safe (3,076; valid after 440) |
| Both walk toward each other, q created at the closest allowed gap, v_max 200 m/s, S_create 10 s | **late** (646) | safe (954) | safe (1,200) | safe (1,262) | safe (2,494) | safe (2,494; valid after 66) |
| Both walk toward each other, q created at the closest allowed gap, v_max 200 m/s, S_create 10 s, world log stalled 8 s | **late** (646) | **late** (954) | safe (1,200) | safe (1,200) | safe (2,494) | safe (2,494; valid after 440) |
| Both walk toward each other, q created at the closest allowed gap, v_max 100 m/s, S_create 20 s, world log stalled 8 s | **late** (1,246) | safe (1,861) | safe (1,831) | safe (2,446) | safe (3,676) | safe (3,676; valid after 440) |
| Two regions created at the same spawn point, 10 frames apart | **late** (640) | **late** (640) | joins | joins | joins | joins (decided after 66) |
| Two-member region grows instantly toward a region being created at the closest allowed gap, then stalls 30 s | **late** (650) | safe (2,494) | **late** (1,200) | safe (1,200) | safe | safe (valid after 66) |
| Both scale up at once, +9 chunks each toward the other (gap 20.5 km) | **late** (1,035) | safe (6,565) | safe (6,565) | **late** (1,035) | safe (6,565) | safe (6,565) |
| Both scale up at once, +9 chunks each toward the other (gap 20.5 km), R stalls | **late** (1,035) | safe (6,565) | safe (6,565) | **late** (1,035) | safe (6,565) | safe (6,565) |
| Chained: q grows +4 chunks three times 5 frames apart while R scales up +9 chunks (gap 24 km), R stalls | **late** (1,040) | safe (8,404) | safe (8,404) | **late** (1,040) | safe (5,951) | safe (5,951) |
| R scales up +8 chunks toward a new region after it exists but before R's log relays it, then stalls 30 s | **late** (1,330) | safe (6,246) | **late** (1,330) | **late** (1,330) | safe (6,246) | safe (6,246; valid after 66) |
| Arrival at the closest valid gap walks and scales up inside its first S_create, R walks toward it | **late** (690) | safe (1,305) | safe (1,231) | safe (1,846) | safe (3,105) | safe (3,105; valid after 66) |
| Arrival at the closest valid gap walks and scales up inside its first S_create, R walks toward it, world log stalled 8 s | **late** (690) | safe (1,305) | safe (1,231) | safe (1,846) | safe (3,105) | safe (3,105; valid after 440) |
| Two arrivals 5 km apart, 10 frames apart, world log stalled 8 s | safe | safe | safe | safe | joins | joins (decided after 430) |
| Two arrivals at the closest valid gap, 10 frames apart, world log stalled 8 s, both walk toward each other | **late** (641) | safe (1,256) | safe (1,241) | safe (1,856) | safe (3,086) | safe (3,086; valid after 430) |
| Two arrivals at the closest valid gap, 10 frames apart, world log stalled 8 s, both scale up 4 km at once toward each other | **late** (650) | safe (3,108) | safe (1,231) | safe (3,689) | safe (3,699) | safe (3,699; valid after 430) |
| R grows 4 km at once toward an arrival at the closest valid gap at frame 605, then walks toward it | **late** (635) | safe (2,560) | safe (1,200) | safe (1,200) | safe (1,925) | safe (1,925; valid after 66) |
| R grows 4 km at once toward an arrival at the closest valid gap at frame 700, then walks toward it | **late** (730) | safe (2,560) | safe (1,200) | safe (2,560) | safe (3,188; R pauses 1,230 frames) | safe (3,188; R pauses 1,230 frames; valid after 66) |
| R grows 4 km at once toward an arrival at the closest valid gap at frame 1100, then walks toward it, world log stalled 8 s | **late** (1,130) | safe (2,560) | safe (1,200) | safe (2,560) | safe (3,588; R pauses 1,230 frames) | safe (3,588; R pauses 1,230 frames; valid after 440) |
| R grows 4 km at once toward an arrival at the closest valid gap at frame 1190, then walks toward it | **late** (1,220) | safe (2,560) | safe (1,220) | safe (2,560) | safe (3,678; R pauses 1,230 frames) | safe (3,678; R pauses 1,230 frames; valid after 66) |
| Giant region walks toward a human region from 1,000 km at its v_max, L stalls on the way | n/a | n/a | safe (4,535) | safe (5,150) | safe (5,150) | safe (5,150) |
| Giant scales up x8 (+14 of its 131 km chunks) 1,500 km from a human region, L stalled | n/a | n/a | **late** (430) | safe (9,032) | safe (6,574) | safe (6,574) |
| Giant steps once onto a human region 100 km away, L stalled | n/a | n/a | **late** (430) | safe (1,045) | safe (1,045) | safe (1,045) |
| Giant scales up +9 of its chunks to land 8 km from a human region that scales up +9 chunks at once, L stalled | n/a | n/a | **late** (1,030) | **late** (1,030) | safe (6,560) | safe (6,560) |
| Two-member giant region walks toward a human region from 1,000 km, L stalled 30 s (never evicted) | n/a | n/a | **late** (4,535) | safe (5,150) | safe (5,150; R pauses 196 frames) | safe (5,150; R pauses 196 frames) |
| Giant arrives overlapping a human region | n/a | n/a | safe (1,200) | safe (1,200) | safe (1,200) | safe (1,200; valid after 66) |
| Giant arrives 50 km from a human region, then walks toward it at its v_max | n/a | n/a | safe (1,231) | safe (1,846) | safe (1,846) | safe (1,846; valid after 66) |

Cells give the merge or read frame in brackets. "joins" means the creation became a join, so no second
region exists. In the Provisional column (D60(a)) a created region takes effect at its stamp, and "valid
after" or "decided after" gives the frames until the world log confirmed or refused the creation.

* **33 cases of steps toward a neighbour, two-sided and concurrent growth, creation and cross-band reads,
  on a line. Late: 20 of 28 under 2a228c6, 2 of 28 under 05d3cc0,
  11 of 33 under e10c905, 5 of 33 under da3cd74, 0 of 33
  under the revised rules** (0 of 33 without the start condition). The cross-band rows
  do not apply before e10c905.
* **Provisional arrival: 0 late of 44 cases**, the 33 above plus 11 arrival cases (results doc 14), with
  either known set: relays on, or only the directory through `s - S_create`. Its merge and read frames
  equal the revised rules' in every row, as the equivalence argument predicts. Each control is late in
  exactly its own case: without the growth freeze, on two concurrent 4 km scale-ups; with readers reading
  a provisional giant from `s0`, on a giant arriving over a human region.
* The da3cd74 failures are reviews Q1 (two concurrent or chained instant growths, also across bands) and
  Q2 (a region not yet relayed into the grower's log).
* **Model fix in this round:** the on-demand pull used to trigger every frame, because it compared a bound
  already clamped to the present. With that fixed, more 2a228c6 and e10c905 rows are late than before (for
  example "scale x8 ... or on demand"). The 05d3cc0 and revised conclusions did not change.
* The revised rules place created regions at their own closest allowed gap, which includes `G_inst`, and
  the matching creation case grows R by the full cap.
* The 05d3cc0 failures reproduce @chunks-reviewer's `safety2_model.py`, and the da3cd74 Q1 rows their
  `concurrent_instant_model.py`.
* This is a model of the rules on a line, not of the protocol or of 2D/3D footprints.
* The phase-3 build carries these cases as protocol tests: the model checks the rules, not the protocol
  or 2D/3D footprints [I].
* The price:
  * growth toward a neighbour lags by up to `J / v_max`;
  * an arrival away from spawn regions confirms only once the world log confirms it valid (about 1.1 s),
    and its footprint stays frozen until `s0 + S_create`;
  * a world-log stall longer than about 9 s stalls regions;
  * a neighbour stalled for 60 s stops its neighbours at the horizon gate, and a stalled giant region
    pauses the human regions it is about to cover.

**Liveness, modelled [C]** (`liveness`, 8.5-frame region lag). Setup: 64 or 256 members, alone or in groups
of 8, with one member's acks stalled 0.5-10 s. A one-member region evicts its silent member by a
**world-log** view change, taking 3.8 s (review N3); larger regions do it in-region in 2.2 s.

| Rule | Unrelated regions that lost frames | Worst loss per region |
|---|---|---|
| World log on the path (45d4940) | **all** (63 of 63, 255 of 255, 31 of 31) for stalls of 1 s or more | 5 / 65 / 77 / 77 frames for 1 / 2 / 3 / 10 s stalls in groups of 8; 5 / 65 / 125 / 173 for members alone |
| Neighbour bound, footprints 3.5-4 km apart | **none**, even when the member is gone for good | 0 |
| Neighbour bound, footprints 3.2 km apart (2 s of slack) | the 2 nearest | up to 23 frames (groups of 8), up to 119 frames (one-member regions, world-log eviction) |

A stalled member slows only its own region, and neighbours whose slack is shorter than the time to evict
it. The gaps in this table are between *footprints*; the anchors are about 6 km further apart.

#### 25.5.3 Joining, leaving and confirmation

* **Joining is a per-region view change.**
  * A member counts for `R`'s stability and confirmation only from the admission frame after its
    `Synced{R, frame}` entry.
  * Before that it is **pending**: it receives `R`'s log, and its entries into `R` stamped before admission
    are void. This rule is chain-local, so every holder applies it identically.
  * A late joiner, a warp or a respawn joins `R` directly; nothing waits on the world log.
* **To sync, a joiner needs `R`'s state at a confirmed frame.** It gets it in one of two ways:
  * a **region state transfer** from any synced member: box3d image + tables + logic sync state,
    compressed, about 0.9 MB per 1,000 bodies with lz4 [V], verified against the BLAKE3 check hash the
    group published in `R`'s log, and consumed completely (9.1, 9.5);
  * if nobody has simulated `R`'s chunks since they froze, it builds `R` itself from dormant records:
    procedural content, SCM content, and frozen-chunk records fetched by their hash.
* **Confirmation of region frame `f`.** Every synced member of `Group(R, f)` has published a horizon of at
  least `f` in `R`'s log, and holds every `R` entry up to `f`. Members outside the group never affect it.
* **Leaving mirrors joining.** The cut is the prefix all remaining group members hold, effective after the
  leaver's last valid horizon. It never touches confirmed frames.
* **A region whose whole group goes silent** (review N3). This is the common case for a one-member region:
  a spread-out player whose connection drops.
  * Nobody inside can vote it out, so eviction is a **world-log view change**, by a strict majority of the
    session (10.8). The modelled latency is about 3.8 s.
  * The same applies to a **two-member** region: its own log never removes anyone, since a strict majority
    without the silent member is impossible (10.8, D7). Its silent member is evicted through the world log.
  * Neighbours keep simulating as long as their slack lasts. With footprints at least 3.5 km apart they
    lost no frames in the model, even when the member never came back (25.5.2).
  * **Durability.** Every region publishes a checkpoint every 30 s (configurable): a `Frozen{region,
    frame, chunks, record_hash}` entry in the world log for its modified chunks.
  * The record bytes go to at least two custodians: members of the nearest other regions, chosen
    deterministically from the world log. When a region's whole group is evicted, its chunks revert to the
    last checkpoint confirmed in the world log; changes since then are lost.
  * A one-member region therefore risks up to 30 s of changes to its own chunks if its only player vanishes.
    A group of two or more keeps every confirmed frame.
* **Membership-dependent computations are evaluated at the entry's frame and frozen at confirmation**
  (the NEW-2 lesson, review R16-3). Every quantity that depends on who is in the group uses
  `Group(R, f_entry)`, the group at the frame of the entry or decision it concerns:
  * the stability set;
  * vote and eviction thresholds;
  * any median or late-report rule that survives round 2.

  A membership change takes effect only at a frame after `R`'s confirmed frame, and never re-evaluates a
  confirmed entry. **Required protocol test:** a member leaves `R` while one of its entries is still
  unconfirmed, and a member joins while a vote is open. Every member must reach the same verdicts, and
  no confirmed frame may change.

#### 25.5.4 Merge and split

* **Merge.**
  * `R` and `q` merge at the first frame `f_m` at which their footprints, computed from both regions'
    confirmed anchors, come within `D_merge`.
  * As they approach, `slack(R, q)` shrinks, and the neighbour bound makes each region wait for the
    other's confirmed state. By `f_m`, both hold both histories through `f_m - Lambda`, enough to know both
    footprints at `f_m`, so both compute the same `f_m`.
  * Members already follow the other region (below), so they hold its confirmed state and log; beyond the
    follow budget they prefetch it by state transfer during the approach (about 672 kB for a group of 8,
    0.5 s at 10 Mbit/s [C]). They simulate it as an extra region up to `f_m` and post `Synced` for it.
  * At `f_m` the box3d worlds merge deterministically. Bodies of the higher-id region are recreated in the
    other world in ObjectId order with their exact state: 0.7-0.9 us per body with the validation fix,
    57-61 us without [V].
  * The merge is **deterministic but not physics-neutral** (review R16-4): recreating a body drops its
    contact manifolds, warm-start impulses and island state. The footprint margin keeps the seam in
    dormant chunks, so the bodies recreated at a merge are mostly asleep, far from any player.
  * A member not yet synced to both parts is pending in the merged region. Only that member's liveness
    suffers, never correctness.
* **Split** (revised after review N2).
  * A region splits only when its footprint has fallen into parts at least
    `D_split = D_merge + v_max x 10 s` apart (4 km at human scale), and has stayed so for `T_split`
    (default 10 s). It then splits at a deterministic frame.
  * The 1 km of hysteresis between `D_merge` and `D_split` means a split region is never immediately
    re-merged. Each new region starts with at least 10 s of slack toward the other.
  * Bodies are partitioned by chunk. Every member already holds the whole region, so nothing is
    transferred.
* **Rollback across a merge or split.**
  * It restores the pre-event keyframes of every part, re-simulates them, and redoes the event at the same
    frame.
  * Under D50 this needs no new rule. Each part keeps its own base until the event frame is confirmed, so
    a keyframe at or before the event always exists.
  * During the transition a peer holds both bases plus the merged window: about base(A) + base(B) +
    4 merged images.
* **The D50 memory/CPU knob applies per box3d world**, so per region. A peer's budget is the sum.
* **Following nearby regions** (D60(a), the user's 18:42Z ruling "players should load the region they're
  in and nearby regions"; [r16-neighbourhood-loading.md](r16-neighbourhood-loading.md) section 3):
  * A player follows every other region within the **follow radius** (about 8 km at human scale), nearest
    first. A follower receives the region's entries and acks and simulates only its **confirmed** frames:
    no rollback and no keyframe history, one live image about 0.15 s late. Physics never crosses regions
    (25.5.5), so a followed region serves presentation and merge readiness only.
  * **No protocol coupling:** a follower publishes no acks, horizons or votes in the followed log, so the
    followed region's confirmation never waits for it. It judges that region's confirmation from stable
    chain data and proofs (25.5.8).
  * **Cost** [V parts, C composition]: per followed spread-out player 0.065 ms per tick, 0.3 MB and about
    7 kB/s; per followed group of 8, 0.47 ms, 2.3 MB and 70-95 kB/s. Following runs within a CPU budget on
    the D50 slider (for example 1 ms per tick at Balanced). Regions beyond the budget fall back to pose
    streams and last-confirmed objects, with prefetch during an approach.
  * **Upload** (review S3) [I]: a member's own region traffic is strictly prioritised over follower
    traffic; follower streams are droppable and never back-pressure the region; each member caps the
    upload it spends on followers (for example 32 kB/s, on the slider). Past the cap, followers are served
    through a fan-out relay among themselves (26.3), or take one member's confirmation summary.
  * **`NeighbourRef` rate** (review S4) [I]: one per followed region every 2 s, and on demand when the
    neighbour bound comes within 2 s of the present. The bound still reads only `NeighbourRef` entries in
    the region's own log, never a member's local follow state.
  * **Content preloading** within the same radius: chunk files by hash and dormant records (101 B each)
    are fetched ahead of need. Procedural content needs nothing (0.2-3 ms of generation per second [V]).
  * **Not recommended: full membership of neighbours.** Co-confirming neighbours would re-couple
    spread-out players, and adjacent fixed tiles have zero slack, so one stalled player would freeze its
    neighbours for 2-4 s (note section 4).

#### 25.5.5 Perturbations across region boundaries

* **Regions never touch.** Active bodies never leave their region's footprint (25.4 rule 2), and two
  footprints stay at least `D_merge` apart, so physics never crosses a region boundary.
* **Routing.** An entry targets objects by ObjectId and goes to the region holding the object at the
  entry's frame. A member may perturb only regions it is synced in; anything else is refused identically
  on every peer.
* **Bodies leaving a footprint.** A body thrown out of its region's footprint freezes at the edge, and
  continues when its chunk becomes active again. The footprint's one-chunk margin beyond `reach_cap` makes
  this rare in view of a player.
* **Carried state** (7.3) stays per object, now with `f64` positions: 64 bytes instead of 52.
* **Check hashes and divergence repair** (9) are per region. Repair is a region state transfer.

#### 25.5.6 What this costs and saves (measured in 25.10)

* **CPU per peer** scales with the bodies in the peer's own regions, not with the session size.
* **Memory per peer:** keyframes for its regions only (the D50 slider applies to the sum).
* **Bandwidth per peer:** region traffic scales with the group size. World-log traffic grows with the
  member count (membership, ACL, SCM and acks; anchors moved to region logs), and needs compact or gossiped
  acks past about 64 members.
* **Liveness:** a stalled member slows only its own region and neighbours whose slack is shorter than the
  time to evict it [C, 25.5.2]. The world log matters only through the region directory, and only for stalls
  longer than about 9 s.
* **Clustered players gain nothing:** when everyone stands in one place, it is one region and the cost is
  today's.

#### 25.5.7 Persistence, source control and procedural content [I]

* **Committed content is stored per chunk:** `world/chunks/L<level>/<x>_<y>_<z>.chunk` in the world
  repository (15), content-addressed like every asset (16.6). An edit is a commit touching only the
  chunks it changed, so two builders working in different chunks never conflict. Streaming a chunk in is
  a content-addressed fetch (16.7), done before its activation frame.
* **Procedural content** is a pure function of `(generator module hash, cell)`, run by the
  `on_populate(cell)` hook at activation. Only the seed and the generator travel. Changing the generator
  is a commit; unmodified cells regenerate at its activation frame.
* **Runtime changes** (moved, created or destroyed objects) are session state, not commits: dormant
  records keyed by chunk, persisted with the session and verified by the record hashes in the world log.
  A builder can commit them into the chunk files (OWO's "compile").
* **The world log carries `Frozen{chunk, frame, record_hash}`** when a region freezes a modified chunk, so a
  later visitor can fetch the record from anyone and verify it.

#### 25.5.8 Cross-region proofs and equivocation (review N4) [I]

* **What a `NeighbourRef` proves.** `NeighbourRef{q, frame, head, envelope}` carries `q`'s chain heads at
  `frame` and the stable chain entries that confirm them, signed by every member of `Group(q, frame)`.
  * That group is itself read from the referenced prefix: joins and leaves are chain entries (25.5.3).
  * The receiver checks the signatures against identity keys from the world log's membership.
  * A one-member `q`'s proof is that one member's signature. Nothing stronger exists without a second
    witness.
* **Detecting a region that shows different neighbours different histories.** Every region posts
  `RegionHead{region, frame, head digest, envelope}` to the world log every 5 s.
  * The audit checks **prefix consistency** (review M5). Any two signed entries at the same chain
    position with different ids, wherever they were seen (`NeighbourRef`, `RegionHead`, a state transfer), are
    a transferable equivocation proof. The signers are evicted, as in 9.3.
  * A region that posts no `RegionHead` for 3 periods (15 s) is treated as stalled, and its group is evicted
    by world-log view change. Its chunks revert to its last checkpoint (25.5.3).
  * So equivocation is detected within about 5 s plus the world-log lag. The world log stays off the
    per-frame path: regions do not wait for `RegionHead`; it only audits.
* **An object two regions both claim.** This happens only after an undetected equivocation.
  * The world log orders the conflicting claims, and the region whose `RegionHead` covering the chunk comes
    first in world-log order owns it.
  * The other region repairs forward, never below its confirmed frame. At its next frame after it holds the
    proof, it replaces its objects in those chunks with the owner's state, by a chunk-scoped state transfer,
    as in divergence repair (9.3).
  * Every peer reads the same world-log order, so the repair is deterministic.
* **MVP:** sessions are trusted (D21), so this machinery only bounds the damage of a malicious member. It
  is a phase-6 item, like the rest of public-session hardening.

### 25.6 Arbitrary scale: per-region length units (measured in 25.8)

* A region simulates in units of `u = 2^k` m, chosen from its members' scales (the smallest, quantized,
  with hysteresis). Box3D sees every length divided by `u`, so its fixed tolerances (5 mm slop, 2 cm
  speculative distance, 0.05 m/s sleep velocity, AABB margins) become relative to the region's scale. That
  is the per-world length unit of ruling 14:20Z, without calling the process-global
  `set_length_units_per_meter` (4.3 rule 10), so regions at different scales run side by side in one
  process.
* **Gravity:** "scale-invariant" (10 units/s^2) is the default. It makes a scaled player's world behave
  like a normal one, and keeps 60 Hz valid. Real gravity (10 m/s^2) at scale `s` is the same system run
  `1/sqrt(s)` times faster. It works, but costs sub-steps in proportion: 10x at 1 cm, 32x at 1 mm [V
  stability, C cost]. Simply adding sub-steps in scaled units does not work [V].
* **Mixed scales in one place** (revised after reviews N5 and M2): **hierarchical bands**.
  * Players whose scales differ by less than about 2^6 share one region at the smaller unit.
  * Further apart in scale, each band keeps **its own regions**, merging and splitting only within the band,
    at the band's own constants.
  * **Reader.** A small-band region `R` whose footprint overlaps a larger band's region `L` is a *reader* of
    `L`:
    * **When reading starts and stops** (review P1). `R` reads `L` from frame `f_r`, the first frame at
      which `L`'s footprint in effect (under `L`'s growth clock) overlaps `R`'s, and stops at the first frame
      after which they no longer overlap. Both frames are functions of `R`'s own log and `L`'s confirmed
      anchors, so every member of `R` computes the same ones.
    * **A provisional `L`** (25.5.2, D60(a)) counts as in effect for its readers only from
      `s0 + S_create`: `R` reads it from `max(first overlap, s0 + S_create)`, and the directory gate
      guarantees `R` knows `L` by then (review S1). `L` still simulates its own band from `s0`, because a
      larger band never reads its readers. So a newly arrived giant acts on smaller players' objects after
      10 s, as before.
    * **Cross-band bound.** `R` simulates `f` only while, for every larger-band region `L` in the
      directory, `known_conf_L + (envelope gap - 0) / v_max(L) >= f`, with `L`'s envelope as in 25.5.2. The
      horizon gate and the start condition apply across bands too, and each side's `J` counts the other
      with margin 0. `G_inst` scales with the band's unit (about 524 km for the x128 band). So `R` always holds every `L` anchor that fixes `f_r` before it simulates `f_r`.
    * Cost: at a giant's `v_max` (12.8 km/s for x128) the bound gives 3.9 s of slack per 50 km of gap, so
      a human region within about 100 km of a giant's region pulls its prefix every few seconds [C]. A
      stalled giant region pauses the human regions it is about to cover (196 frames in the model's 30 s
      stall).
    * it holds `L`'s log and simulates `L`'s band world itself, stepping it first each frame, so `L`'s
      bodies appear in the small world as kinematic proxies **of the same frame**;
    * it confirms a frame only after `L` has confirmed that frame (a confirmed fact of `L`). Before that it
      speculates, and rolls back with `L`;
    * `L` never reads its readers, and a small-band player perturbs `L`'s objects only by joining `L`'s
      group.
    * @engine-architect's conditions for this against the cross-log rules (round 3): `R`'s members follow
      `L`'s log themselves (they hold its chains and judge its confirmation from its stable data); `R`'s
      confirmed frame `f` is computed from `L`'s *confirmed* state at `f`; strictly one way; a deterministic
      frame mapping (same tick rate or a fixed ratio); and `L`'s view changes reach its readers, which needs
      per-epoch certificates (R3-8b).
  * So a giant does **not** merge the humans around it. Each human region stays separate, depends one way on
    the giant's region, and is independent of every other human region.
  * The mass ratio across bands is at least 2^18, so one-way coupling loses less than 4e-6 of the momentum
    [I].
  * **What it costs** [I]:
    * readers simulate the large band's bodies, which are few, and receive `L`'s log (about one hand stream
      per giant);
    * if `L` stalls, its readers' *confirmation* waits; their simulation continues up to the speculation
      limit (2 s), then pauses.
  * **Alternatives** (D61):
    * readers take `L`'s confirmed state about 10 frames late, so a giant's foot reaches small objects about
      0.17 s late, but there is no confirmation coupling;
    * one multi-band region for everyone in range (as at 05d3cc0): exact, but a x100 giant pulls every
      human within about 380 km into one group, back to everyone simulating everything (review M2).
* **Per-band constants** (review N5). `D_merge`, `D_split`, `v_max` and `S_create` scale with the band's
  unit, as footprints and `reach_cap` already do. At human scale they are 3 km, 4 km, 100 m/s and 10 s.
  * A x100 giant's band (`u` = 128 m) gets about 380 km, 512 km and 12.8 km/s. The times stay the same.
  * Regions of different bands never merge; merges and `D_merge` act within a band. Across bands, the
    cross-band bound and the reader rule above apply, with margin 0.
* **Levels and ranges follow scale:** a member's footprint, `reach_cap` and `min_r` scale with its height.
  A giant does not simulate pebbles, and an ant does not simulate mountains 50 km away.
* **Consistency with the van der Waals work:** its contact-recycling radius scales per pair, which is also
  unit-free. In region units it gets the same relative sizes at every scale.

### 25.7 OWO concepts: what carries over

| OWO | Primordial | Why |
|---|---|---|
| Floating origin, per user | Camera-relative rendering + deterministic region origins | A shared bit-exact sim cannot let each user see a different frame |
| Compound coordinate: `long3` chunk + scale factor + `float3` | `i64` chunk + `f64` offset; scale through levels and region units | Same idea, more precision, no scale field in positions |
| Chunk layers `32 * 2^n` m, objects in chunks about 8 times their size | Levels `1024 * 2^L` m, objects filed by relevance distance | Same hierarchy; the filing ratio is now the culling ratio |
| Render bubbles per layer | Render and simulation relevance per level | OWO culls rendering; we also cull simulation |
| Transitions: every chunk repositioned, O(total chunks), keep worlds under about 1,000 chunks | None. Region rebase is O(region bodies) and rare; camera-relative rendering is O(visible) | Removes OWO's chunk-count limit |
| Scale transitions | Region length units and per-level footprints | No rescaling of the world |
| Spaces: Users / Userspace / World / Procedural | Avatars (presentation) / runtime sim objects / SCM-committed chunk files / procedural cells | — |
| Compile into world chunks | Commit chunk files to the world repository | Edits to different chunks never conflict (15) |
| Populate Chunk impulse | `on_populate(cell)` deterministic logic hook, cached by (generator hash, cell) | Content is never sent, only the seed and generator |
| Anchoring vehicles to a user's space | Nothing special: vehicles are ordinary sim objects | No per-user frames, so the Appendix B carry bugs cannot exist |
| GC of empty chunks inside transitions | Sparse maps: an unmodified chunk has no record | — |
| Int64 overflow guard | The same guard at ±2^63 chunks | — |
| Settle delays (1-30 updates) | Exact frames | Determinism |
| Host-only sweeps | Every rule runs identically on every peer | P2P |

### 25.8 Measured: f64 cost, distance and scale

Full tables, method and the load caveat are in [r16-chunks-results.md](r16-chunks-results.md) (sections 1-5).

| Question | Result |
|---|---|
| f64 vs f32 box3d step | +2 to +10% (minimum envelope of 8 interleaved rounds; 1,024 and 2,048 piles, 500-ball pit) [V] |
| Snapshot size | +1 to +2% (for example 1.98 → 2.02 MB at 1,024 bodies) [V] |
| f32 absolute coordinates | a 1.3 m/s walker drifts 383 m per hour at 100 km and cannot move beyond 524 km; a ball stops rolling at 1,000 km; a tower never sleeps at 1,000 km and leans 0.39 m at 10,000 km [V] |
| f64 absolute coordinates | precise (tower within 1e-6 m at 100,000 km), but box3d's f32 AABBs inflate the broadphase: 542 → 2,562 contacts and 2.4x the step at 10,000 km; 32,142 contacts and 23x at 100,000 km [V] |
| Region-local origin (chunk centre) | behaves and costs as at the origin at every distance, in both builds [V] |
| Translation invariance | f64 with a region-local origin reproduced a chaotic pile to 1e-12 m in all 7 placements; f32 diverged by metres for any shift, including 512 m. So f64 makes region **rebases** physics-neutral [V]. **Merges** recreate bodies and are deterministic but not neutral (25.5.4) |
| Arbitrary scale | **The design's rule:** units `2^round(log2 s)` (box3d sees 0.78-1.28 units), with a mixed-size scene (cubes 2s down to s/4, balls s/10 and s/20). Stable at every scale from 0.1 mm to 10 km: small-ball penetration 0.06-0.09% of the radius, sleeps at frame 32-34. Unscaled: unstable at 0.1-1 mm; 7% penetration at 1 cm [V, `scale-mixed`]. The earlier "same hash" result used unit = s exactly, so box3d saw the identical scene; that only checks determinism (review R16-7) |
| Coordinate drift, chunk + f64 | 3 nm per hour of walking at any distance up to 1e6 light-years [V] |

**Choice:** box3d `double-precision` **on**, with region-local origins, which are needed anyway. It costs
2-10% of step time, and buys translation-invariant rebases plus precision margin inside large regions. f32 with region origins is the cheaper alternative (D58).

### 25.9 Measured: spatial index

| Index (human-scale workload, 200,000 objects, 64 players) | Query | Memory | Update per moved object | Build |
|---|---|---|---|---|
| Octree rooted at each chunk | 59 us | 183 MB | 557 ns | 149 ms |
| Loose octree rooted at each chunk | 104 us | 321 MB | 312 ns | 121 ms |
| **Hashed multi-level grid** (BTreeMap / HashMap) | **34 / 25 us** | **77 / 79 MB** | **250 / 155 ns** | **69 / 77 ms** |

On the extreme-scale workload (radii 1 um to 1,000 km, 10,000 km apart), the grid also wins: 179 us per
query vs 236 / 312 us, 69 MB vs 131 / 201 MB. Update cost is a near tie with the loose octree. In the
reviewer's rerun, the loose octree was marginally faster on human-scale updates (453 vs 487 ns), within
noise. The tree reaches 28 levels deep; the grid has 40 occupied levels,
each reached by one hash lookup. All four return identical, brute-force-checked results, including when built
in shuffled insertion order [V]. Recommendation: the hashed grid, with `BTreeMap` storage for free ordered
iteration (D63).

### 25.10 Measured: regions and player count

* **Region-scoped rollback** (16 players, 2,016 bodies): a late entry for one player costs 0.55-0.68 ms
  instead of 8.6-13 ms for the flat world (15-18 times less). The 16 regions step in parallel in about 0.2 ms of
  wall time, against about 1 ms in series. Regions gave identical hashes stepped forward, in reverse and in
  parallel, and through rollback [V].
* **Per player, f64 with the validation fix:**

  | Players | Spread out | Groups of 8 | All in one place (= today's unchunked design, any layout) |
  |---|---|---|---|
  | 2 | 0.12 ms/tick, 1.5 MB | 1.9 ms, 3.0 MB | 1.9 ms, 3.0 MB |
  | 16 | 0.12 ms, 1.5 MB | 6.8 ms, 11.6 MB | 12.6 ms, 23 MB |
  | 64 | 0.12 ms, 1.5 MB | 6.8 ms, 11.6 MB | 67 ms, 92 MB |
  | 256 | 0.12 ms, 1.5 MB | 6.8 ms, 11.6 MB | about 300 ms [C] (282-635 ms across runs), 368 MB |

  CPU per 16.7 ms tick and memory per player [V region costs, C combination]. Every player keeps a ball pit
  busy with a hand.
* **Bandwidth per player** [C, from the measured encodings in 7.6]:
  * region traffic: 0 when spread out, 94 kB/s in a group of 8;
  * world-log traffic grows with the session: with today's ack format (one line per member), 15 kB/s at 16,
    216 kB/s at 64 and **3.3 MB/s at 256**; with compact 100 B acks, 6 / 26 / 106 kB/s.
* **Conclusion:** spread-out players are independent. The remaining O(N) cost is the world log, which needs
  compact (or gossiped) acks beyond about 64 members. Clustered players are not helped by regions: 256 engaged
  players in one place would need about 300 ms per tick [C] and 51 MB/s each [C], so a region needs a cap on engaged
  members (D62).

### 25.11 Measured: streaming

A player crossing generated content for 20 s at 1.5, 15, 150 and 1,500 m/s (f64 with the validation fix):
* 330-640 active bodies, out of about 25 million objects within 1 km;
* frame total (step plus streaming) 0.10-0.18 ms mean in my run. The reviewer's rerun on a loaded machine
  gave 0.15-0.54 ms. The p99 and worst-frame tails did **not** reproduce under load (p99 1.3-6.5 ms, worst
  4-65 ms), so they are not quoted until a quiet-machine rerun;
* up to 816 bodies created and 816 destroyed per second;
* generation 0.2-3 ms per second; relevance queries under 1 ms per second.

The f32 build had to rebase the region every 4 km (0.85
ms each); the f64 build never did. A dormant record is 101 B per body, against 0.7-2 KB in a box3d image [V].

### 25.12 Decisions for the user

* **D58. Coordinates and box3d precision (25.2, 25.8).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: canonical `i64` 1,024 m chunk + `f64` offset; box3d `double-precision` on; region-local
    origins. Costs 2-10% of step time [V]; makes rebases physics-neutral (merges stay deterministic) and keeps precision
    everywhere.
  * Alternative: box3d f32 with region-local origins and rebasing every few km. 2-10% cheaper, but every
    rebase or merge perturbs chaotic scenes, deterministically but visibly. This supersedes D3's "keep
    double-precision off".
* **D59. What is simulated (25.4).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: size-based relevance, `|p - o| <= 32 r`, `r >= h/64`, reach cap 2 km x scale, on the memory/CPU
    slider. Awake bodies stay active until asleep; one-hop support closure. Dormant objects freeze exactly;
    logic catches up through `on_wake`; physics is never fast-forwarded.
  * Alternative: OWO-style whole-chunk activation, about 25 million objects within 1 km in the test
    content.
* **D60. Regions and groups (25.5).**
  * **Settled (user, 18:42Z: "players should load the region they're in and nearby regions", applied as
    option (a) by the coordinator at 19:06Z): regions as designed, plus provisional arrival, following and
    content preloading** (25.5.2, 25.5.4; [r16-neighbourhood-loading.md](r16-neighbourhood-loading.md)):
    * the two-tier log, with anchors in region logs and the speed-bounded neighbour bound (growth clock,
      `v_max` 100 m/s, `D_merge` 3 km, `D_split` 4 km);
    * region creation via the world log, taking effect at its stamp and confirming once valid (about
      1.1 s), with spawn regions at busy arrival points;
    * nearby regions followed within a slider budget, and their content preloaded.

    Modelled [C]: 0 late of 44 cases (25.5.2), with either known set. Built in phase 3 together with
    region-scoped rollback (6.6); phase 2 stays one region per world.
  * **What players notice:**
    * physics starts at once wherever you arrive; if the world log is stalled, it pauses after 2 s until
      the log confirms your arrival (about 5 s in an 8 s stall);
    * for the first 10 s your simulated area (at least 3 km around you) does not grow, including around
      friends joining further out;
    * a giant arriving over smaller players acts on their objects after 10 s;
    * rarely, another arrival or a region within about 8 km in the same second turns your arrival into a
      join, and your first second of changes is undone;
    * travel faster than `v_max` needs a declared travel footprint or a portal-style transfer, and a
      scale-up's distant surroundings wake over time (D61).
  * Rejected: (b) a 10 s wait outside spawn regions; (c) fixed tiles with co-confirming neighbours (one
    stalled player freezes adjacent occupied tiles for 2-4 s); and the earlier shorter `S_create` and
    arrivals tied to their previous region, which (a) makes unnecessary.
* **D61. Scale (25.6).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: per-region length units with scale-invariant gravity by default; real gravity as a per-world
    opt-in, time-scaled at `1/sqrt(s)` CPU cost for small players.
  * With D60(a), a scale-up within 10 s of an arrival starts its footprint growth only after those 10 s.
    Following is the same-band counterpart of the reader rule below: a giant over a human region is still
    read with the confirmation dependency, and a same-band neighbour is followed without it.
  * **How fast a region's footprint follows a scale-up** (review Q1). Players scale arbitrarily, and the
    scale change itself is never rate-limited in any option below. The question is how fast the
    *simulated area* around a scaled-up player grows (25.5.2). Options:
    * **(a) recommended: capped instant growth.** The first `G_inst` (4 km at human scale) of footprint
      growth is instant when no region is within roughly 20 km; the rest follows at `v_max` (100 m/s).
      * Scale-ups up to about x3 are fully instant in open country. A x5.5 scale-up wakes its new outer
        ring over about 51 s in open country, about 92 s near another region. A joiner at an edge facing
        open country is instant [C].
      * Noticed only if something beyond about 3 km of the player should move in that time [I].
    * **(b) no instant growth** (as at 05d3cc0). Every growth follows at `v_max`.
      * Simplest rule, fewest cases to get right. A x5.5 scale-up always takes about 92 s, and a joiner at
        an edge up to about 31 s, for objects beyond the old footprint.
    * **(c) a larger cap**, such as 16 km.
      * Scale-ups up to about x9 are fully instant in open country. But every region then reserves 16 km
        or more for its neighbours' possible instant growth, so instant growth needs about 50 km of empty
        space, and new regions must be about 12 km further from existing ones, so more creations become
        joins [C].
    * Not offered: rate-limiting the scale change itself. Players would feel it directly, and it buys
      nothing over (b).
  * **Mixed scales in one place** (players more than 2^6 apart in scale), options:
    * **(a) recommended, hierarchical bands with same-frame readers.**
      * A giant's interactions with small things are exact to the frame, and a giant never merges the human
        regions around it.
      * Cost: small players also simulate the giant band's few bodies, and a lagging giant delays their
        confirmation, pausing them after 2 s. Human regions near a giant's region pull its prefix every few
        seconds, and pause if it stalls as it approaches them (the cross-band bound).
    * **(b) hierarchical bands with delayed reads.**
      * Small regions are fully independent.
      * Cost: a giant's motion reaches small objects about 0.17 s late, so it cannot catch a ball as
        precisely as it sees it.
    * **(c) one multi-band region.**
      * Exact.
      * Cost: one x100 giant pulls every human within about 380 km into one group, losing the R16
        scaling.
* **D62. Crowding.**
  * **Settled (user, 18:51Z and 19:11Z): as recommended,** the crowd cap of 16 stands.
  * Recommend: cap engaged members per region (default 16) by stamping the inputs of members beyond the cap a
    few frames ahead (input delay, D41), and lean on avatar LOD (R17). Clustered players cannot be made cheap
    by regions [V].
* **D63. Spatial index (25.3, 25.9).**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: hashed multi-level grid (`BTreeMap`), not a pointer octree. It was faster and 2.4-4 times
    smaller on both workloads [V].
* **D64. World-log acks at scale.**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: compact digest acks on the world log from the start, and gossip relaying past about 64 members.
    Today's per-member ack lines would cost 3.3 MB/s per player at 256 [C].

### 25.13 What this adds elsewhere

* **Crates (23.1):**
  * `pe-core`: `WorldPos`, `CellKey`, `pow2`, `ceil_log2`;
  * `pe-sim`: dormant records, transforms with `f64` position;
  * `pe-worlds` (or a new `pe-regions`): the region set, groups, merge and split, relevance passes, the
    hashed grid;
  * `pe-physics`: per-region length units, region origins, box3d `double-precision`.
* **Plan (23.2):**
  * phase 2: coordinates, `f64` transforms, box3d double precision;
  * phase 3: regions, groups, region-scoped rollback, streaming, length units;
  * phase 6: compact or gossiped world acks for hundreds of members.
* **Determinism rules (4.3):** coordinate maths uses powers of two and bit-pattern logarithms only; the
  spatial index is a cache whose results are sorted by id.
* **MCP (17):** `world.chunk.get`, `world.relevance.explain` (why an object is or is not simulated) and
  `regions.list`.

---

## 26. Built-in avatar and avatar-feature culling (R17)

Author: @world-chunks (branch `chunks/r16`, a9a20cc); reviewed by @design-reviewer (45d4940, R17-1 to
R17-3) and approved; merged at integration. Measurements in 26.6 come from `spikes/chunks`
(`pe-spike-chunks avatars`); logs are in `spikes/out/chunks/avatars.log`. Tags: **[V]** measured here,
**[S]** read in source, **[I]** inferred or proposed.

R17 (user, 14:11Z): "also we need a built in system for culling avatars and avatar features". In
Resonite-like platforms that work is left to users and worlds. Here the engine does it natively,
per viewer, for whole avatars and for each feature class.

### 26.1 The rule: culling is presentation, and the sim parts of an avatar are not culled

* **An avatar is presentation** (16.3, 22.8): mesh, skinning, blend shapes, materials and shaders,
  secondary motion (dynamic bones, cloth), particles, audio, lights, IK, and presentation logic modules.
  Culling any of these is a local choice per viewer. It never enters the log, never changes a hash, and
  two viewers may cull differently.
* **The sim-relevant parts of a player are not part of the avatar.** They are perturbation sources that
  every member of the region simulates whatever it renders:

  | Part | In the sim as | Culled? |
  |---|---|---|
  | Engaged hand colliders | hand-target entries (7.5, 18.4) | never |
  | Optional body or foot collider (a world setting: can players push things?) | the same kind of kinematic source, streamed while it touches sim objects | never |
  | Activation anchor (R16) | coarse and fine anchor entries in the member's home-region log (25.5.2) | never |
  | Held or worn objects | ordinary sim objects with a claim (8.4) | never; their meshes are presentation and may be culled |
  | Avatar logic that writes sim state | a sim module, committed to the world (13), not an avatar feature | never |
* **Enforced structurally:** presentation modules cannot write sim components (7.4), and the culling pass
  reads the sim only through a shared reference (`&World`), so the compiler forbids writes. A culled
  avatar's hands still push balls on every peer.
* **Checked [V]** (`avatars-sim-isolation`):
  * Setup: a real region of 4 ball pits with sweeping hand colliders ran 300 frames, with a culling and
    presentation pass every frame whose avatars stand at the hands.
  * The region's per-frame hash digest was identical (`7418e4f8aeb60229`) with no pass, with culling off,
    and with full culling.

### 26.2 Tiers per viewer

Each avatar gets a tier per viewer, from its **angular size** `a = height / distance`, the same metric R16
uses for simulation relevance, so a giant is detailed from far away and a tiny player is culled early:

| Tier | Angular size | At 1.7 m tall | Mesh | Secondary motion | Particles | Lights | Presentation logic | Pose stream |
|---|---|---|---|---|---|---|---|---|
| T0 full | > 1/8 | < 14 m | LOD0 | all chains, 60 Hz | all | all | every frame | 60 Hz, full body |
| T1 reduced | > 1/32 | < 54 m | LOD1 | half the chains | 1/4 | 1 | every 2nd frame | 30 Hz |
| T2 far | > 1/128 | < 218 m | LOD2, atlas material, fallback shader | none (rest pose) | none | none | every 4th frame | 15 Hz, upper body |
| T3 impostor | > 1/512 | < 870 m | impostor or capsule + name | none | none | none | suspended | 5 Hz, root only |
| T4 culled | smaller, or outside the view | — | not drawn | none | none | none | suspended | none |

* **Visibility:** outside the view frustum an avatar drops to T3 at best. Its spatialized voice is kept by
  the audio rules below. Occlusion culling (hierarchical depth) can demote further later [I].
* **Hysteresis:** 15% on every threshold, so avatars do not flicker between tiers.
* **Budgets per feature class** (a per-device setting, part of the user's performance slider): at most
  16 T0 avatars, 8 avatar lights, a skinned-vertex cap and a particle cap. When over budget, the
  lowest-priority avatars are demoted. Priority is angular size, raised for friends, the current
  speaker, anyone interacting with you, and anyone you pinned.
* **Audio** has its own rules: voice is kept to a social range (default 50 m x the speaker's scale,
  spatialized), avatar sound effects to 20 m, plus a voice-count budget. Audibility, not visibility,
  decides.
* **Safety overrides:** per-avatar "always show", "show as impostor" and "block". Shader-complexity and
  particle caps also limit how much an avatar can cost a viewer on purpose.

### 26.3 How it uses R16

* Avatars are entries in the same hashed multi-level grid (25.3), filed by height, so finding the avatars
  in reach is one relevance query per viewer.
* Avatars beyond the viewer's regions are known only through world-log anchors (chunk level). They are
  drawn as T3 impostors at their anchor chunk when large enough to see, or not at all.
* **Avatars in a followed region** (25.5.4, D60(a)) are drawn from that region's entries, their sim parts
  at full rate. Only presentation-only pose detail still needs a pose stream.
* **Pose streams follow the tiers.** A viewer subscribes to a remote avatar's pose stream at its tier's
  rate (a presentation subscription, not a log). Received bandwidth then scales with what you can see,
  not with the session size.
* **Upload is the sum of everyone's subscriptions** (review R17-2). An avatar that everyone sees at T0
  (a stage performer, a giant) uploads 179 kB/s to 15 viewers, 750 kB/s to 63, and 3.0 MB/s to 255 [C].
  * The sender serves at most 16 full-rate subscribers directly (configurable).
  * Further viewers get the stream relayed through subscribers, chosen by a deterministic fan-out tree.
  * Pose streams are presentation data, so a relay adds latency but no correctness risk.

### 26.4 Determinism

* Culling decisions use local state only (camera, settings, budgets). They are never logged or hashed and
  never feed a sim decision.
* Secondary motion, particles and presentation logic are local simulations. They are not synchronized and
  may differ between viewers by design (as today for unengaged hands, 16.3).
* The sim-relevant parts in 26.1 are untouched by any tier.

### 26.5 What it adds elsewhere

* `pe-render`: tier evaluation per view (per eye pair in VR), LOD selection, impostor baking at import,
  budgets.
* `pe-xr` and the network thread: per-tier pose subscriptions.
* Avatar import (19): LOD and impostor generation at import time; an avatar without LODs gets an
  automatic decimation.
* MCP: `avatars.culling.get` and `set` (per-device budgets and per-avatar overrides), so an agent can tune
  a crowded session.

### 26.6 Measured

A synthetic avatar is evaluated on the CPU for every avatar at its tier, per frame:
* 20,000 vertices skinned from 60 bones;
* 8 dynamic-bone chains;
* 800 particles;
* 2,000 operations of presentation logic.

The viewer stands at the centre with a 110° field of view. GPU work is not timed (there is no renderer yet), so
skinned vertices, draws, lights and particles are reported as proxies. Pose bandwidth uses the per-tier rates
in 26.2. Full table: [r16-chunks-results.md](r16-chunks-results.md) section 10.

Two baselines (review R17-3):
* **none:** every avatar at full detail;
* **frustum only:** full detail inside the view, nothing outside it, which is what any engine does by default.

The second run executed at low process priority (the machine-load rule), so its CPU figures are higher and
noisier.

| Crowd (256 avatars unless noted) | CPU per frame: none / frustum only / R17 tiers | Skinned vertices: frustum only → R17 | Pose stream received: frustum only → R17 |
|---|---|---|---|
| 64 in 100 x 100 m | 8.9-11.3 / 2.9 / **1.1-1.4 ms** [V] | 340 k → 166 k [C] | 203 → **119 kB/s** [C] |
| 30 x 30 m, packed | 35-45 / 16.4 / **6.6-8.6 ms** [V] | 1.80 M → 912 k | 1,072 → **657 kB/s** |
| 200 x 200 m | 38-47 / 15.6 / **2.6-3.2 ms** [V] | 1.80 M → 342 k | 1,072 → **277 kB/s** |
| 200 x 200 m, 10% tiny and 10% giant | 47-54 / 16.1 / **3.9-5.1 ms** [V] | 1.66 M → 470 k | 989 → **344 kB/s** |
| 2 x 2 km | 38-51 / 12.3 / **0.07-0.19 ms** [V] | 1.14 M → 8 k | 679 → **29 kB/s** |

* **Against frustum-only culling**, the R17 tiers save about 2 times the CPU in dense or small crowds and
  about 5 times in a 200 m crowd. Against no culling at all, they save 8 to 15 times.
* In the 200 m crowd the distance tiers do the work; the budget adds nothing there. In the packed crowd
  the budget matters.
* Lights are capped at 8.
* The angular-size rule handles scale without special cases: giants stay detailed further out, tiny players
  drop sooner.
* Single-run CPU numbers vary by about ±20% under the machine's load.

### 26.7 Decisions for the user

* **D65. Tier thresholds and budgets.**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: the angular-size tiers in 26.2, with per-device budgets of 16 full avatars and 8 avatar lights,
    plus vertex and particle caps, all on the performance slider.
  * Friends, the current speaker and pinned avatars get priority.
* **D66. Can avatar bodies push things?**
  * Recommend: hands only by default (as today). A body or foot collider is a per-world option. It becomes one
    more kinematic perturbation source in the region log, about one more hand stream per player while in
    contact.
* **D67. Safety overrides.**
  * **Settled (user, 19:11Z): as recommended.**
  * **Settled (user, 19:11Z): as recommended.**
  * Recommend: per-avatar "show", "impostor" and "block", plus shader-complexity and particle caps, so that no
    avatar can cost a viewer unboundedly. Resonite leaves this to worlds; here it is engine default.

---

## 27. Live-editable and replaceable physics (R15)

User requirement 15 (13:43Z): players with authority can interact with, override, or completely replace
parts of the physics engine, or all of it. Physics becomes live-editable, like game logic.

### 27.1 Principles

* **Physics changes are commits.**
  * Every change to physics goes through the SCM (15) and takes effect at an **activation frame**
    (15.2), exactly like logic: a hook module, a material table, a joint or shape definition, a
    subsystem selection, or a whole replacement engine.
  * Every peer applies the change at the same frame, so the simulation stays deterministic.
  * Nothing about physics is configured outside the log.
* **Authority.**
  * World ACL rights gate each layer (12): `physics.hooks` for layers 1-2 and `physics.replace` for
    layers 3-4.
  * A grant applies from its own confirmed frame, like any ACL entry.
* **Same sandbox as logic.**
  * Guest code runs in the logic runtime (D12: wasmi now, Cranelift later), with fuel, NaN
    canonicalization and no ambient imports.
  * Its whole linear memory is rollback state (D57).
* **Deterministic order.** box3d-rust steps single-threaded, so it calls hooks in a fixed order
  [S box3d-rust 0.5: no thread use]. Batched hooks (27.3) make the order explicit: contacts in
  constraint order.

### 27.2 The layers

| Layer | What a world can change | box3d-rust today [S 0.5.0] | Needs a fork for |
|---|---|---|---|
| **1. Interaction hooks** | Custom forces and fields; contact pair filters; per-contact enable/disable (one-way platforms, ghosts); friction and restitution mixing | Per-step forces through the body API; `world_set_custom_filter_callback` (needs the shape flag at creation); `world_set_pre_solve_callback` (enable or disable a contact, given its point and normal); `world_set_friction_callback` and `world_set_restitution_callback` (mix two values, given user material ids). All are plain `fn` pointers. Filter and pre-solve also get a `u64` context; the mixers get only the two material ids (review R15-2) | Per-**substep** forces and fields (stiff springs, magnetism, buoyancy); **contact modification**: changing a contact's normal, friction, restitution or target speed. Today only `tangent_velocity` exists, per material |
| **2. Overrides** | Materials; custom joints; custom shapes; a per-body integrator (orbits, drag models, character controllers) | Surface materials with user ids, 8 joint types (distance, motor, parallel, prismatic, revolute, spherical, weld, wheel), and convex hulls, meshes, height fields and compounds. Composition covers much of this: joints from motors and forces, shapes from hulls | Guest-defined joints (a constraint row the solver iterates); guest-defined shapes (a support function or SDF the narrowphase calls); an integrator hook per body class |
| **3. Subsystem replacement** | Broadphase, narrowphase, solver | none | Traits for each subsystem in the fork |
| **4. Whole-engine replacement** | Swap Box3D for a guest physics module | — | Nothing in Box3D: a stable world/step/snapshot interface on the engine side (27.4) |

**Recommendation (D68):**
* **Layer 1, and the material and mixing part of layer 2, ship first,** on box3d-rust's existing hooks.
* **Contact modification, per-substep forces, and custom joints, shapes and integrators go into a fork.**
  @vdw-physics already maintains one for the vdW contact radius (`box3d-fork`, e1164ff and f71a233), so
  R15's extension points join it as features, upstreamed where useful.
* **Layer 3 offers only engine-provided alternatives,** chosen per world (for example a soft-step solver
  or a GPU broadphase). No guest-authored subsystems: the boundary would be crossed per pair or per
  contact row, and a broken solver can make a world unplayable for everyone in it.
* **Layer 4 is allowed, with a body budget** (27.5).

### 27.3 Where each layer runs

* **Layers 1 and 2 hooks: wasm guest, batched per step in the fork.** Stock box3d-rust asks each
  callback inline and needs the answer at once, so until the fork adds two-pass hook points, one guest
  call per contact is the only option.
  * Box3D's callbacks fill arrays in guest memory: contacts with shape ids, point, normal and depth, or
    pairs of material ids.
  * The engine then calls the guest once per hook kind per step, and reads the verdicts back.
  * Per contact this costs 12 ns in wasmi and 4.7 ns in Cranelift, against 64 ns and 16 ns for one call
    per contact (27.5).
* **Fuel:** hooks get a fuel budget per step. On exhaustion every remaining verdict takes the hook's
  declared default (for example "keep the contact"). That is identical on every peer, because fuel is
  identical within one runtime (13.3).
* **Isolation:** the guest receives only contact data and returns verdicts. It has no imports beyond
  deterministic math, and cannot touch bodies directly. A force or field hook returns impulses that the
  engine applies.
* **Layer 3:** native code in the engine build, chosen per world through a world setting. It is part of the
  engine version, so the determinism matrix covers it (4.3 rules 7-8).
* **Layer 4:** a wasm guest that implements the engine's physics interface (27.4), in the same sandbox.
  Its whole linear memory is its rollback state.

### 27.4 Changing physics mid-session: commits, activation, rollback

* **Activation.** A physics change commits like logic (15.2): confirmed, then active from activation frame
  *A*. Frames before *A* use the old physics and frames from *A* use the new, on every peer, whenever they
  are simulated or re-simulated.
* **Rollback across *A*.** The hook or engine module set is rollback state, like logic modules (13.5).
  Re-simulating from before *A* runs the old physics up to *A*, then switches.
* **Hook state** (guest memory) is snapshotted with the world image (D57). A hook swap at *A* migrates
  state the way a logic swap does (13.5).
* **Whole-engine replacement (layer 4).** The engine interface is:
  * `init_from(objects)`: build from the engine's object records (transform, velocities, shapes, materials,
    joints), the same records R14 carries (22.6);
  * `apply(entries)`, `step(dt)`;
  * `save()`, `load(image)` and a canonical `hash()`;
  * `query(...)` for rendering, MCP and logic;
  * `export_objects()` for the switch back.

  At *A* the old engine exports, and the new one is initialized from the export: deterministic, because
  the export is a pure function of the state at *A*.
* **Snapshots across *A*.** The snapshot ring may then hold images in both formats, each tagged with its
  engine. The base stays at or before the confirmed frame (6.7). A rollback to before *A* restores an
  old-engine image and redoes the switch. D50's knob applies unchanged.
* **Failure** (review R15-3). A layer-4 engine that traps or runs out of fuel during the step of frame
  *t* does so identically everywhere: the trap is a function of the log and the engine version. The
  fallback must be one too, so it never reads a peer's confirmed frame, which differs between peers:
  1. Restore the guest's image from the start of frame *t* (the state after *t* - 1, identical on every
     peer) and call `export_objects` on it. The previous engine is initialized from that export and steps
     frame *t*. The guest is marked failed from *t*, as rollback state, like a logic module's fuel flag
     (13.6).
  2. If that export traps too, fall back to the previous engine's image at the activation frame *A*, and
     re-simulate from *A* with the previous engine. Every peer holds that image, because it is the
     switch point; it stays pinned in the snapshot ring while a guest engine is active.
  3. A re-simulation that crosses *t* repeats the same fallback.
  * A runtime change (wasmi to Cranelift, D12) moves where fuel runs out. It is therefore a world-format
    version bump, never mixed within a session (13.3) [I].
* **Undo.** Reverting the commit is a new commit with its own activation frame. Confirmed history is never
  rewritten (10.8).

### 27.5 Performance

Timings in this subsection are noisy: the runs used low priority on a shared machine, as the machine-load
rule requires. Counts are exact.

**Per-hook call cost from native code into a guest, fuel on** [V `iter-host callback`,
`spikes/out/r15/callback.jsonl`]:

| Runtime | One call per contact (pre-solve) | One call per mix | Batched, per contact (host writes the record + guest loop) |
|---|---|---|---|
| native function | 1.3 ns | — | — |
| **wasmi eager (D12, now)** | **64 ns** | 79 ns | **12 ns** |
| wasmtime Cranelift + NaN canonicalization (D12, later) | 16 ns | 23 ns | 4.7 ns |
| wasmtime Winch + NaN canonicalization | 21 ns | 26 ns | 5.8 ns |

**How often Box3D calls the hooks**, on the phase-1 pile, which stays active [V `pe-spike-physics hooks`,
re-measured at integration with the engine default, vdW recycling on (review R15-1)]:

| Bodies | Pre-solve calls per step (avg / max) | Friction + restitution mixes per step | Upstream recycling (d948898) |
|---|---|---|---|
| 256 | 160 / 291 | 319 | 141 / 247, 281 |
| 1,024 | 610 / 985 | 1,220 | 488 / 862, 976 |

* **Recycling made hooks fire 13-25% more often here, not less** (27.6 had inferred less). A recycled
  contact skips the narrowphase update and its hooks [S `collide.rs`]. The engine default's radius is a
  tenth of the pair's length (the smaller half-extent or radius), about 0.04 m for these bodies of 0.4 m
  half-extent, below upstream's fixed 0.05 m, so fewer of their contacts are recycled [I from the formula,
  S `recycle_scale.rs`]. Bodies whose smallest half-extent exceeds about 0.5 m recycle more than upstream,
  and call the hooks less.
* The pile that falls asleep is unchanged: its contacts sleep, so hooks rarely run at all.

**Cost of guest hooks per step** [V parts, I composition]. Each cell is calls/step × ns/call, with the
counts above:

| Bodies | wasmi, one call each | wasmi, batched | Cranelift, batched | Step itself |
|---|---|---|---|---|
| 256 | 35 µs avg, 44 µs max (10-12% of a 0.36 ms step) | 6 µs (1.6%) | 2 µs (0.6%) | 0.36 ms [V b6] |
| 1,024 | 135 µs avg, 159 µs max (7-11%) | 22 µs (1.2-1.5%) | 9 µs (0.5%) | 1.5-1.9 ms [V b6] |

* **Layer-1 and layer-2 hooks are cheap** when batched: under 2% of the step even under wasmi.
  Batching needs the fork's two-pass hook points; on stock box3d-rust each contact is one call.
* Per-contact calls cost up to 12% under wasmi. Batching is therefore the recommended model (D69).
* The step times are b6's, measured with upstream recycling. The toggle itself costs a few flops per
  pair [V vdw-simulation.md 4.7], but fewer recycled contacts mean more narrowphase work, so this pile's
  step may be a little slower with the engine default [I].
* Re-simulation multiplies hook cost exactly as it multiplies the step (6.8).

**Whole engine in wasm (layer 4)**: Box3D itself compiled to wasm, on the phase-1 pile, where native takes
0.31 ms/frame [V iteration-benchmark 4.5]:

| Runtime | ms per frame | × native |
|---|---|---|
| **wasmi eager + fuel (D12, now)** | **6.6** | **21x** |
| Cranelift + fuel + NaN canonicalization (D12, later) | 1.46-1.54 | 4.7-5.0x |
| Cranelift + fuel, no NaN canonicalization | 0.75 | 2.4x |
| Winch + fuel + NaN canonicalization | 1.96 | 6.3x |

* Under the D12 ruling (wasmi now), a guest engine costs about 21x native. A 256-body world takes about
  6.6 ms per frame, before any rollback, against the 8 ms per-tick budget (2.1).
* With re-simulation at 60 ms one-way latency (about 5 re-simulated frames per frame, 6.10), only very
  small guest worlds fit.
* Hence the body budget for layer 4 (D70), at 60 ms one-way (about 6 steps per tick with re-simulation)
  and an 8 ms tick budget [I] (review R15-4 corrected the first figures, 64 and 256, which came to 9-10 ms):
  * about 48 active bodies under wasmi (1.65 ms per step at 64 bodies, so about 1.24 ms at 48, times 6);
  * about 200 under Cranelift (about 1.17 ms per step at 200 bodies, times 6).
  * These scale the 256-body measurement down linearly. Box3D's cost grows faster than linearly with
    contacts, so the scale-down is conservative. The final caps should come from a guest-engine run at the
    cap, with re-simulation.
* The coordinator's d4 figure (7 ms per frame interpreted, about 22x) is the same measurement as the wasmi
  row. Cranelift is 4.4-8.8x faster than wasmi.

### 27.6 What box3d-rust exposes, and what needs the fork

* **Available today [S box3d-rust 0.5.0]:**
  * pair filter, pre-solve (enable or disable a contact) and friction/restitution mixing callbacks;
  * user material ids, passed to the mixers;
  * 8 joint types, motors and forces through the body API.
* **Plumbing caveat:** these are bare `fn` pointers, not closures. The engine registers one trampoline per
  hook kind.
  * Filter and pre-solve find the world's guest through their `u64` context.
  * The friction and restitution mixers have no context: their two `u64` arguments are the material ids
    (review R15-2). Their trampoline reads a thread-local "current world", set around each
    `world.step`. Box3D steps one world on one thread at a time, so this is sound, and needs no locking.
    The fork may add a context argument instead.
* **Hooks run at narrowphase time, not every step** (review R15-1). Box3D calls the mixers and pre-solve
  only when the narrowphase updates a contact. A recycled contact keeps last frame's friction,
  restitution and enabled flag [S box3d-rust `contact/update.rs`, recycle path in `collide.rs`].
  * With vdW contact recycling, the engine default (radius up to 0.5 m), a verdict can stay stale over up
    to 0.5 m of relative motion: a one-way platform's "disable", or a friction field that changes in time.
  * So a hook that must see every step marks its contacts non-recyclable. The fork owns recycling, so
    this is a fork feature (D68). The hook API states the semantics.
  * Re-measured with vdW recycling (27.5): on the phase-1 pile (bodies of 0.4 m half-extent) hooks fire
    13-25% more often than with upstream recycling, because the scaled radius is smaller there [V counts]. Large
    objects recycle more, so their verdicts stay stale longer.
* **Missing; needs the fork:**
  * contact modification (normal, friction, restitution, target velocity per contact);
  * per-substep force and field hooks;
  * a guest constraint row for custom joints;
  * a support-function or SDF shape for custom shapes;
  * an integrator hook per body class;
  * subsystem traits for layer 3.
* **The fork already exists for vdW** (`box3d-fork`, @vdw-physics). R15's extension points join it as
  feature-gated additions. Each one keeps the determinism goldens and the snapshot layout hash (4.3),
  and changes them only by a world-format version bump.
