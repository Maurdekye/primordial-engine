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
* confirmation-based finality, which makes commits final in about 0.1 s instead of 2 s (6.4, 15.2);
* the body budget (D2);
* the ball pit's perturbation-source design (7.5);
* `rel-fast` as the iteration build profile (13.7).

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
  the user"**, each with a recommendation and alternatives. Sections refer to them as **D1..D49**, with
  letter suffixes where one topic was split (e.g. D14a).
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
  * panels talk peer to peer within a session, and through a mail hub (a hubchat-style panel);
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
| Sim tick | 60 Hz (16.7 ms), or 90 Hz for VR-heavy worlds (D4) | — |
| Sim-thread work per tick, per world, including rollback | ≤ 8 ms; anything beyond that is amortized over the next ticks (6.5) | — |
| Physics step per world | ≤ 1 ms at the MVP body budget, about 500 awake bodies (D2) | 0.36 ms at 256 bodies, 1.5-1.9 ms at 1,024 [V b6]; 0.35 ms for the 500-ball pit [V session-sim] |
| Typical rollback depth | one-way latency + ack interval: 4-8 frames | max 4 at 60 ms ± 20 ms, 9-14 with loss [V session-sim, p2p-v2]; deeper for a late receiver, which now pays for its own lateness (8.2): 38 frames after a 600 ms delay [V session-sim outage] |
| Continuous perturbation (a hand in a ball pit) | ≤ 3 ms of re-simulation per frame at 500 balls, 60 Hz, 2 members, ≤ 60 ms one-way | 289-298 re-simulated frames/s ≈ 2.6-2.8 ms per frame at 60 Hz [V ballpit, work counts × 0.35 ms per step]. **Over budget** at 90 Hz (597 frames/s, about 3.7 ms per 11.1 ms frame) and at 100 ms one-way (6-8 ms of total sim CPU per frame) [V ballpit-90hz, observer-100] (review NEW-15); budgeted per latency band, with p99 ticks (6.8). 0.27 ms with the opt-in 4-frame input delay (default 0, D41) |
| Confirmation (finality) | ≤ 200 ms on 30-60 ms links | 8.5 frames (142 ms) at 50 ms ± 15 ms [V session-sim r14, round 2; 4.2 frames in 3d0f98a, before stable-data confirmation] |
| Snapshot memory per world | ≈ 4 keyframes (2-10 MB) typical, 31 at most | 6.3 |
| Game logic per tick, per world | ≤ 1 ms | 6.4 ms in wasmi, 1.6 ms in wasmtime/Cranelift, 0.9 ms native at 10k entities [V iteration-benchmark 4.1] |
| **Logic edit, local build and swap** (one-function edit) | ≤ 0.5 s at up to ~10k LOC; ≤ 1.5 s at 45k LOC | 0.42-0.45 s at 6.4k LOC, 1.2-1.3 s at 45k (`rel-fast`, wasmi, idle machine) [V iteration-benchmark; reviewer rerun]. 0.58 s with 8 busy processes, 0.83-1.01 s with 14 (D12-4) |
| **Logic edit to every peer in a live session** | ≤ 1.2 s | ≈ 1.05 s: build and swap 0.45 + confirmation 0.1 + activation lead 0.5 [V components; I total] (D12-4) |
| Commit visible to every peer | confirmation + activation lead ≈ 0.1 s + 0.5 s | 15.2 |
| Shader edit to visible | ≤ 100 ms locally (naga validation is milliseconds [I]) + the same commit path | 14 |
| Hot swap at a frame | ≤ 10 ms on the sim thread | 6 ms in wasmi; Cranelift 27-35 ms, so it compiles off-thread during the activation lead (13.3) [V iteration-benchmark, reviewer] |
| Bandwidth per member, interactions + acks | 1-5 kB/s | 7.6 |
| Bandwidth, a hand engaged in a ball pit | ≤ 20 kB/s per member | 2 members: 11.9 kB/s both directions batched (65 + 30 kbit/s), 18.8 kB/s per message [V r18 capped, ballpit]. Upload grows with members, since each peer gets its own copy: about 16 kB/s for the hand's author at 3 members and 24 kB/s at 4 [I, from the 2-member figures] (NEW-15). Signatures add 2-4 kB/s per peer [I, 10.9] |
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
   Box3D's deterministic math or an engine `pe_core::dmath` with bit-exact polynomial implementations.
   Basic IEEE ops (`+ - * / sqrt`, conversions) are exact.
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
   each step. The sim thread checks MXCSR (FTZ/DAZ off) every tick and resets it, because native
   libraries loaded in-process (OpenXR runtimes, audio drivers) can change it [I].
10. **No process-global sim state (R14).** box3d-rust's length unit is a process global; it stays at
    1.0 for every world. Its `human` sample module (a thread-local RNG) is never used in sim code. Logic
    runtimes compile eagerly (22.2) [S].

### 4.4 Gaps of box3d-rust compared with the C reference (tech ruling: "note any gaps")

| Gap | Impact | Mitigation |
|---|---|---|
| Serial only (no task system or worker threads) [S port README] | Step cost grows with bodies: 0.36 ms @256, 1.5-1.9 ms @1024, 4.8-5.9 ms @2048 [V b6]; rollback re-sim multiplies this | Partition large worlds into independent Box3D worlds stepped in parallel (6.6); cap active bodies per session for MVP (D2) |
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
* Tick rate: **D4**. It is now a VR-driven trade-off: 90 Hz removes most hand-to-object mismatch but costs
  1.5x the CPU, including every re-simulated frame.

---

## 6. Rollback (R3)

### 6.1 Rollback state

A snapshot at frame *f* (the state before frame *f*'s entries) contains:

| Part | How | Evidence |
|---|---|---|
| Box3D world | `recording::serialize_world` image (POD + geometry registry), restored in place with `deserialize_into_shell`. Bodies created or destroyed after the snapshot are undone by the restore, and their ids are reissued identically on re-simulation | [V b1, b4; V session-sim r14: bodies destroyed by a departure and created by an arrival or return across rollbacks] |
| Engine tables | claims, object table, R14 escrow and arrival tables: plain copies | [V session-sim] |
| Sim ECS store (16) | column copies (memcpy) | [I] |
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
* With confirmation 4-5 frames behind the present, a world holds about 3-4 keyframes: about
  2-2.5 MB at 256 bodies and 7-10 MB at 1,024 [I, from the image sizes]. The old proposal held a fixed 2 s
  ring (about 72 MB at 1,024 bodies).
* The speculation limit (6.4) caps the worst case at 120/K + 1 = 31 keyframes.
* Byte-delta compression of images was measured and does not help (6.8). Sync-state-only guest snapshots
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
    rolling back. No scenario reaches it [V session-sim, 26 scenarios].
* **Speculation limit W = 120 frames (2 s, D4).** A member whose present runs W frames ahead of the
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
  * priority 1 (optimization) therefore sets the MVP budget at **about 500 awake bodies per world**
    (D2), with island- or region-scoped re-simulation (6.6) pulled forward to phase 3.
* Debug builds are about 10x slower; determinism tests run in release.

### 6.6 Scaling past the serial port [I]

* Box3D worlds are independent, so a session can be split into **regions**, each its own Box3D world,
  stepped in parallel on worker threads.
* Objects crossing regions migrate at deterministic frames, through the same escrow pattern as R14 (22.6)
  but inside one session log.
* Snapshots and rollbacks then become region-scoped: only regions touched by a late entry re-simulate.
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
  the same scene ended in the same end-state hash (`47bb8c7274b143d3` at 500 balls, `bfe86c8ce148add8`
  at 1,024) [V snap].

**Re-simulating from the base** (restore plus N steps, collapsing pile, median of 5, one thread)
[V physics history]:

| Depth (frames) | 1 | 4 | 8 | 15 | 30 | 60 | 120 |
|---|---|---|---|---|---|---|---|
| 256 bodies | 0.8 ms | 2.0 | 3.6 | 6.4 | 12.4 | 26.4 | 50.8 |
| 1,024 bodies | 3.3 ms | 7.9 | 14.2 | 25.8 | 51.0 | 98.9 | 189.7 |

A tick at 60 Hz has 16.7 ms. **Base-only** fits only while confirmation keeps up, because every
rollback then re-simulates the whole confirmation lag, not just the rollback depth.

**Rollbacks stay shallow even when confirmation stalls** [V snap]:
* How deep a rollback goes is set by how late entries arrive: one-way latency, the ack interval, and a
  fetch round trip after a loss. The deepest measured was 14 frames, at 60 ± 10 ms with 2% loss.
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
60 Hz, measured across the 3.66 s sweep. In the **stall** case a third member's acks are dropped for 1.6 s,
so confirmation lags by up to 116 frames.

**How CPU is reported.** CPU per frame is modelled: the deterministic work counts (re-simulated frames,
saves and restores per second) times the per-operation costs measured in isolation. For the 500-ball pit
those costs are: step 0.353 ms, save 0.52 ms, restore 0.37 ms. Measured timings (best of 2-3 runs, with
other agents building on the same machine) rank the policies the same way, plus about 1 ms of protocol
overhead.

| Policy | Max MB held, 500 balls (steady / stall) | Max MB held, 1,024 balls (stall) | Re-sim frames/s (steady / stall) | Saves/s (steady / stall) | Modelled CPU ms per frame (steady / stall) |
|---|---|---|---|---|---|
| Every(1) | 25.6 / 183.7 | 412.4 | 209 / 196 | 210 / 198 | 3.77 / 3.58 |
| Every(2) | 13.6 / 93.4 | 209.7 | 239 / 225 | 104 / 99 | 3.03 / 2.90 |
| Every(4) (3d0f98a) | 8.2 / 47.5 | 106.6 | 298 / 284 | 51 / 50 | 2.91 / 2.82 |
| Every(8) | 5.0 / 25.3 | 56.8 | 416 / 401 | 26 / 25 | 3.39 / 3.29 |
| Every(16) | 3.3 / 12.6 | 28.3 | 661 / 646 | 14 / 12 | 4.72 / 4.62 |
| Log(4) | 8.2 / 16.1 | 36.1 | 298 / 284 | 51 / 50 | 2.91 / 2.82 |
| Log(8) | 5.0 / 12.8 | not run | 416 / 401 | 26 / 25 | 3.39 / 3.29 |
| **Window(4)** | **8.2 / 8.3** | **18.6** | 298 / 284 | 51 / 50 | **2.91 / 2.82** |
| Window(8) | 5.0 / 8.1 | 18.1 | 416 / 401 | 26 / 25 | 3.39 / 3.29 |
| Base(1) | 3.3 / 3.3 | 7.4 | 416 / 2,138 | 27 / 11 | 3.40 / 13.39 |

Measured CPU per frame for comparison (best runs): at 500 balls, Every(4) 3.84-4.04, Log(4) 3.84-3.97,
Window(4) 3.84-4.00, and Base(1) 4.43 steady and 16.4 under the stall. At 1,024 balls under the stall:
Every(4) 9.9-10.8, Window(4) 10.8, Base(1) 47.7-48.9.

What the table shows:
* **Dense images do not buy CPU.** A save costs about 1.5 steps here, so saving every frame costs more
  than the re-simulation it saves. CPU is lowest at k = 2-4.
* **Thinning is free.** Log(k) and Window(k) do exactly the same work as Every(k), because they drop only
  images that no rollback used. Under the stall, Window(4) held 8.3 MB instead of 47.5 MB, and 18.6 MB
  instead of 106.6 MB at 1,024 balls.
* **Base-only is the memory floor, and cheap while confirmation keeps up** (+17% CPU). Under a stall,
  every rollback re-simulates the whole lag: 2,138 frames/s, 13.4 ms per frame modelled, 16.4 ms measured,
  and 48 ms at 1,024 balls. It needs a guard (below).
* **Byte deltas do not help.** Storing ring images as 64-byte block deltas against the base, or against
  the previous image, saved at most 6% on the ball-pit rings and usually nothing. On an active pile every
  block changes, because contact arrays shift whenever a contact begins or ends. The best case, a
  slow-moving 256-body pile, still needed 38-43% [V snap, physics history].
* **The worst case does not depend on the policy.** A rollback all the way back to the confirmed frame
  re-simulates the whole lag whatever is cached: up to 51 ms at 256 bodies and 190 ms at 1,024 when
  W = 120. It can only be reduced by a smaller W, a smaller active world or region-scoped
  re-simulation (6.6). The 8 ms per-tick cap (6.5) spreads it over several ticks.

**The knob (proposal, D50).** "Snapshot memory" is one setting per peer, local and changeable at run time,
with four positions:

| Position | Policy | Max image memory (500 balls steady; stall 500 / 1,024 balls) | CPU against the best | Rollback depths covered without the base |
|---|---|---|---|---|
| Speed | Log(4) | 8.2; 16.1 / 36.1 MB | best | any depth, at ≤1.5x |
| **Balanced (default)** | Window(4), adaptive depth [I] | 8.2; 8.3 / 18.6 MB | best | ≤ 16 frames (every measured rollback was ≤ 14) |
| Lean | Window(8) | 5.0; 8.1 / 18.1 MB | +17% | ≤ 32 frames |
| Minimum | Base(1) + stall guard [I] | 3.3; 3.3 / 7.4 MB | +17% steady; guarded under a stall | none |

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
  * In the knob-moving runs memory peaked at 57.6 MB (500 balls, stall) while Every(1) phases refilled the
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
    bit-equal and the largest position error was 244 mm.
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
| 30 ± 5 ms | 23.5 / 32.6 → **1.3 / 2.0 mm** | 12.7 → **1.0** | 5.3 → **0.3 mm** | 3.0-3.9 ms |
| 60 ± 10 ms | 79.6 / 125.5 → **15.2 / 17.0 mm** | 30.4 → **11.3** | 44.1 → **4.3 mm** | 4.4-6.1 ms |
| 100 ± 16 ms | 149.4 / 197.3 → **41.5 / 37.0 mm** | 34.8 → **25.4** | 63.4 → **14.4 mm** | 6.1-8.1 ms |

* **Dead reckoning cuts the observer's error by about 4-18x** for smooth hand motion, because the
  speculative sim pushes balls roughly where the real hand is pushing them. Its limit is any change of
  motion: a stop, a turn or a grab is seen one latency late, and the balls involved are corrected.
* **Smoothing trades accuracy for continuity.**
  * At 60 ms latency, τ = 50 ms raises the p99 ball error against the truth from 4.3 to 9.2 mm, and
    τ = 100 ms to 14.0 mm. Corrections are shown late on purpose, but nothing jumps.
  * Large corrections still show as fast glides: up to about 10-18 m/s for the rare 1 m correction.
  * Proposal [I]: smooth corrections below about 5 cm over 50 ms, glide larger ones over 100 ms, and snap
    above about 0.5 m.
* **The hand rule is a world rule, not a local option.**
  * At 100 ms with 2% loss, measured on 442286f before round 2 removed re-stamping, some lost hand
    entries were fetched more than 15 frames late and re-stamped, which left a few confirmed frames without
    an entry. Under round 2 the same happens by design whenever the author thins its hand stream (10.9).
  * The rule then shaped confirmed history: hold and dead reckoning produced different end states, each
    identical across members [V observer-100].
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
    * multithreaded stepping (box3d-rust 0.5 uses no threads [V: no thread use in the crate]);
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
  3. for each carried body, overwrite its state with the carried bits **only if they differ**.
* **Why the rule is deterministic:** whether the overwrite fires depends only on the state at *f*. That
  state depends only on the log. So every peer holding the same log does the same thing, and the result is
  bit-identical.
* **What the rule buys:**
  * a peer already on the author's world line skips the overwrite and is bit-exact by determinism;
  * a peer that had diverged (it lacked an earlier entry, or hit a bug) gets the touched object pulled onto
    the author's world line at once;
  * so every interaction is also an immediate, object-scoped sync check: R6's "this sync also runs
    automatically whenever a player interacts with an object".
* **Acceptance criterion (user, 09:31Z):** after a receiver re-simulates to its present frame, its copy of
  the object must equal the author's copy at that frame. Measured:
  * **3 members, 60 ms ± 20 ms:** 22/22 uncontended checks equal, both to the author's copy at that instant and to its final history [V session-sim state-frame]. **Over real UDP between two processes:** 13/13 at 60 ms ± 20 ms, and 9/9 in the fault run [V p2p-v2]
  * **10% loss on every link:** 27/27 uncontended checks equal [V session-sim loss]; 14/14 over UDP [V p2p-v2 loss10]
  * **A receiver that had missed an earlier entry on the same object:** 23/23 equal. Its carried-state overwrite fired once and pulled the object onto the author's world line before the missing entry arrived [V session-sim diverged]. Checks are *contended* when another member touched the same object in the previous 10 frames; that is the scripted same-frame conflict on object 50, 10 checks per run. Those can differ while the competing entry is in flight, and all converge when it lands. Every run ends with all histories bit-identical [V]
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
  * With vdW on, uncontended acceptance at the instant in `diverged` dropped from 23/23 to 21/23. Both
    misses were object 5 at frame 333: its author had not yet seen another member's touch on object 77,
    whose effect reached object 5 through the pile.
  * Final-history acceptance stayed 23/23 [V @vdw-physics, b8f130c].
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
* **Acks** are about 54 (2 members) to 79 (3 members) bytes each, every two frames, per member.
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
   * Trying only stops confirmation: every member voids all such acks, and confirmation stalls until a
     view change removes that member [V session-sim backdate part 2: all 318 acks void for every member,
     with no window opened].

**What a cheater can still do:**
* backdate within one ack interval plus the slack, about 4 frames (67 ms) at the defaults;
* claim a present that lags the true one. That looks exactly like a slow link: it is accountable (others
  see the member's stamps consistently trail their own clocks) but not provable.

Accordingly, the residual H5 risk is smaller than before (about 4 frames instead of about L = 15), not zero.
A cheater can still pick, within that window, a frame where `tie_rank` favours it (8.3). MVP sessions are
trusted or invite-only (D21).

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

### 8.4 Claims (review M3)

* An accepted interaction claims its object for 30 frames; other members' interactions on it are refused
  with `RejectedClaimed{by, until}`.
* **A plain impulse claims once and never renews.** Only a held grab renews its claim, for as long as it is
  held. Repeated nudges therefore cannot lock an object [V session-sim claims: in the reviewer's lock-out scenario, member 2 (trying every frame) was accepted 0 of 600 times before the fix and 360 of 600 after; member 1 (every 29 frames) still got 9 of 21].
* **World logic (member 0) never claims** and is never refused by claims.
* Agents are rate-limited at intake (17.4).
* A cap on how long a grab may be held is **D38**.

### 8.5 Fairness

* "Soonest" is fair to the precision of clock sync plus the tie permutation. The spike's same-frame
  conflicts were scripted (review L5), so fairness under real skew is [I].
* Cheating is bounded (8.2), not prevented. The MVP runs trusted or invite-only sessions (D21).

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
  * **Eviction** is decided once every remaining member has voted (a strict majority of the epoch).
    * The **cut** is the longest common prefix of the voters' frozen heads.
    * Entries after the cut are void. The evicted member stops counting from the frame after its last valid
      horizon.
    * Any stable entry is inside the cut, so no confirmed frame ever changes. This is virtual synchrony's
      flush, decided from chain data alone.
  * Measured on the reviewer's repros, ported to session-sim [V]:
    * `ackfork`: member 9 showed member 2 a different chain. Both honest members found the conflict,
      evicted 9 with a cut before its touch, and agreed. Before, they forked.
    * `lateequiv`: a second version of an already-confirmed entry. The stable version stood, 9 was evicted
      (cut at seq 103, from frame 206), and nobody panicked. Before, every honest member crashed.
    * `evict`: after an eviction, a late entry kept its frame (60) on every member. Before, a member crashed.
    * c2 equivocation before confirmation: as in 3d0f98a, both members ended equal and equal to the replay.
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
* Images are memory layouts tied to the engine build. Admission already requires an identical build
  (4.3 rule 7).
* Region- or island-scoped transfer is a later optimization.

---

## 10. P2P topology, transport and the session log (R1)

### 10.1 Topology [I]

* **MVP: full mesh** for sessions of up to about 8-16 peers. Traffic is small: interactions, hashes,
  engaged-hand streams. Remote-avatar pose streams dominate, at roughly 3 poses x 28 bytes x 60 Hz = 5 kB/s
  per pair.
* **Later:** epidemic or Plumtree-style relaying for larger sessions, where any peer forwards for others.
  No peer is special.

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
  * Object ids `(creator MemberId, counter)` stay unique for the life of the session.
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
* **Always-on peers** (like Resonite's headless hosts) are ordinary members that keep a world running.
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
   * **The host announces itself under the nameplate.** The member who creates the code (the host) announces
     its address with BitTorrent `announce_peer` (BEP 5) under
     `infohash = BLAKE3("primordial/code/v1" ‖ nameplate)[..20]`. A relay address (10.5) also works. The
     host re-announces until the code expires.
   * **The joiner finds the host, then proves the password.**
     * The joiner's peer checks the check word locally, so a typo never costs an attempt.
     * It calls `get_peers(infohash)` and contacts at most 8 of the returned addresses.
     * With each, it runs a PAKE (SPAKE2, or CPace; neither pure-Rust crate is audited yet), with the
       password as input and the nameplate bound into the transcript.
   * **On success, the host sends a normal invite ticket** (11) over the PAKE-keyed channel: session ID,
     member addresses, the inviter's key and a join secret. The join then proceeds as for any invite
     (10.6).
   * **Nothing derived from the password is ever published.** The DHT holds only the nameplate's infohash
     and the host's address. No signed session record exists to forge.
   * **Limits:**
     * a code expires after 10 minutes, or after one use unless the owner allows more;
     * the host accepts at most 10 failed PAKE runs per code in total, and at most one per source address
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
| Online guessing against the host | At most 10 guesses per code: 10 / 7,776² = 10 / 6.0 × 10⁷ ≈ 1.7 × 10⁻⁷ |
| Fake hosts announced under the nameplate | Each joiner attempt contacts at most 8 candidates: ≤ 8 / 6.0 × 10⁷ ≈ 1.3 × 10⁻⁷ per attempt, and failed runs are shown to the joiner |
| Nameplate collisions (honest) | With 10⁴ live codes, a given code shares its nameplate with probability ≈ 1.7 × 10⁻⁴. The only cost is failed PAKE runs, which can burn a code; the owner makes a new one |
| Burning someone's code (denial of service) | Ten wrong guesses burn it. Low impact: the owner makes a new code |
| Someone who overhears the whole code | Can join. A code is a bearer invitation, like a ticket. Use single-use codes, or admission approval |

   * **Costs:**
     * the code grows from three words plus a check word to four plus a check word (ease of use, D35a);
     * the host must stay online and reachable, directly or through a relay, while the code is live;
     * DHT observers learn that the announced address hosts a Primordial join code, much as BitTorrent
       peers are visible. Announcing a relay address hides the host's address (D35b).
   * **Rejected alternative:** binding a key fingerprint into the code. Resisting offline search and
     second-preimage attacks needs about 6 words (77.5 bits) plus a check word, which is not memorable.
2. **Outbound-only reachability.** A host behind NAT stays reachable by holding an outbound connection to
   a relay. Our equivalent: members with open ports, plus **volunteer relays** that anyone can run (the
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
* A general view change (a silent member, a join) needs agreement under partial synchrony. That is a
  PBFT-style view change or Raft's joint consensus [I].

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
* [V session-sim: 26 scenarios, every one bit-identical to its offline replay, 0 false checks;
  V p2p-v2 over UDP: latency60, loss10, fault]

**Gap detection and fetch:** from acks' heads, and from holes in chains (9.3).

**Membership epochs.**
* The member list changes only through a view change, which needs a strict majority of the current epoch.
  So at most one side of a partition can change membership or confirm (review H3).
* **Eviction of an equivocator** is decided from chain data alone (9.3): votes, frozen heads, the cut, and
  the frame from which the member no longer counts [V session-sim c2, ackfork, lateequiv, evict].
* **General view changes** (a silent member, a join, demotion to spectator, 10.9) [I]:
  * A proposal names the frame it takes effect at, which is after every signer's confirmed frame.
  * For each removed member it names the cut: the prefix every remaining member holds.
  * Each member signs at most one proposal per epoch round.
  * Confirmed frames never change, because the cut contains every entry that could be stable.

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
   * **Response:** four levels, raised quickly (above 40 ms, at most once per 100 ms) and lowered slowly
     (after 300 ms below 10 ms).
     * Each level thins the hand stream to one entry every 1/2/3/4 frames, and acks to one every 2/3/4/6
       frames.
     * A thinned hand entry carries a `span`: its target is reached over that many frames, and dead
       reckoning (6.10) covers the gaps.
     * Every peer, the author included, applies the same entries, so the author's own physics stays exact
       and is never corrected.
   * Adaptation happens when entries are authored, never by dropping queued entries: chain entries cannot
     be dropped.
4. **Bounded queues:** the controller keeps the uplink queue near its 40 ms trigger. The router queue
   (500 ms in the matrix) is only reached under a cap the stream cannot fit even at the coarsest level.
5. **Leaving a stall.**
   * A member that fell behind re-simulates in the background, at up to 8 ms per tick (6.5), and draws
     only its present. The catch-up is never shown as a replay.
   * Render-only smoothing (6.10) blends the jump.
   * Its own new input is stamped at its present, never in the past.
6. **Demotion beyond the hard bound** [I]:
   * A member whose acks or entries keep confirmation stalled past half the speculation limit, three times
     within 10 s, is demoted to **spectator** by a view change (10.8).
   * A spectator keeps receiving the log and rendering, but stops counting for confirmation, and its new
     input is refused.
   * It rejoins as a member with a state transfer at a confirmed frame (9.5) once its link holds.
   * Demotion is the only case in which an author's input is undone: its entries after the cut, which no
     other member holds. Others' histories are never rewritten.
7. **Repair:** hash checks at confirmed frames and state transfer (9.3, 9.5). These cover true divergence.
   None was observed.

**Measured** (500-ball pit; member 1 sweeps; 50 ± 5 ms one-way; token-bucket cap each way, 4 kB burst,
500 ms router queue, tail drop; dead reckoning on) [V session-sim `capped`, spikes/out/r18]:

| Cap | Transport | Uplink m1→m2 | Max queue | Stalled frames (m1 / m2) | Max confirmation lag | Back to real time after the gesture | End state |
|---|---|---|---|---|---|---|---|
| 256 kbit/s | any | 65-78 kbit/s | 0 ms | 0 / 0 | 9-10 frames | at once | all equal |
| 64 kbit/s | per message (3d0f98a transport) | 77 kbit/s | 274 ms | 0 / 0 | 26 frames | 515 ms | equal |
| 64 kbit/s | batched | 65 kbit/s | 0 ms | 0 / 0 | 9 frames | at once | equal |
| 48 kbit/s | per message | 244 kbit/s offered (repairs pile on) | 499 ms, 4,324 drops | 236 / 254 | 340 frames | never in the run | not converged |
| 48 kbit/s | batched | 65 kbit/s | 498 ms, 55 drops | 117 / 119 | 184 frames | never in the run | equal |
| 48 kbit/s | **batched + adaptive** | 57 kbit/s | **54 ms**, 0 drops | **0 / 0** | 14 frames | **at once** | equal |
| 32 kbit/s | **batched + adaptive** (levels 1-3 used) | 41 kbit/s | **94 ms**, 0 drops | **0 / 0** | 16 frames | **at once** | equal |

* In every run the author never rolled back and none of its entries moved. The engager's view was never
  corrected.
* Without adaptation the stream collapses once the cap is below what it needs: repairs add load, queues
  fill, members stall at the speculation limit, and they do not recover while the gesture lasts.
* With adaptation the same caps keep queues under 100 ms and the session in real time, at the cost of a
  coarser remote hand (the observer's error grows with the stride, 6.10).
* **Wire cost of signatures [I]:** with one Ed25519 signature per datagram, on its newest entry, add 64 B
  per datagram. That is about 3.8 kB/s per peer at 60 datagrams/s, unless signatures ride only on acks
  (about 1.9 kB/s).
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
  `acl.grant`, `acl.revoke`, owner changes, threshold settings (D10) and delegation.
* **Sub-certificates for restricted sessions (21.3).**
  * A panel key may issue sub-certificates to the restricted sessions it runs (guest and inbox sessions).
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
  attribute them to user U, and to the guest or remote party that a sub-certificate names.
* **Revocation.** Revoking a certificate (`CertRevoked {key, effective_frame}`) stops the agent at a
  deterministic frame. `GuestRevoked` (21.5) is the per-guest case. The frame is a
  confirmed frame (6.4), so every peer stops the agent at the same point.

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

### 13.2 Who compiles: trust model (D11)

* **Option 1 (recommended): the committer compiles; peers run the shared wasm.**
  * WASM is sandboxed: memory-safe and limited to the capabilities the host API grants. A malicious module
    can do no more than malicious *source* could.
  * The residual risk is a binary that does not match its source, which defeats review and rebase. Peers
    that have the toolchain run **background reproducibility checks**: they rebuild from source and
    compare canonical hashes.
  * Peers without a toolchain can still play and run logic. That matches "end users mostly won't touch
    code".
* **Option 2:** every peer compiles from source. Maximum trust, but every machine needs about 1 GB of
  toolchain plus compile time per commit.
* **A module's identity is its canonical hash (review M8):** SHA-256 over the module with all custom
  sections removed (`name`, `producers`, `target_features`).
  * The same source built from two checkout paths gave different raw hashes for `physics_in_wasm.wasm`
    (`589e97…` and `051eee…`), because mangled names in the `name` section depend on the path. The
    canonical hashes were identical (`66633b43…`) [V `scripts/wasm_canonical_hash.py`,
    spikes/out/wasm-canonical-hash.txt].
  * Builds also pass `--remap-path-prefix`, so panic locations embed no local paths [I].
* **Only the committer compiles (review H1).**
  * `compile_failed` is advisory feedback to the author, never a peer-side rejection.
  * Peers reject only what they can decide identically: a module that fails wasm validation or the
    commit-time checks (13.4), or a canonical hash that does not match the uploaded bytes.
* **Dependencies (review L8).** Logic crates may depend only on vendored crates committed to the world
  repository, with a `Cargo.lock`. Committers and reproducibility checkers then need no crates.io access.
  * Build-time code (`build.rs`, proc-macro crates) runs native code on whoever compiles. It is refused in
    logic crates, except for the engine SDK's own macros, which ship with the toolchain.
  * The toolchain itself is pinned by the engine and invoked by absolute path (15.6; review NEW-8), never
    through a `rust-toolchain.toml` or `.cargo/` that cargo or rustup would load from the workspace.
    Mirroring it so committers need no rustup host is **D11**.

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
  * The 100%-Rust audit also needs to flag inline `asm!` (D12-3).
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
* **Recommendation, a user decision (D12): wasmi everywhere now; Cranelift everywhere once the patch exists
  and a fuel-exhaustion test passes on all three OSes.** Never a per-OS split.
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
  * Typical confirmation is about 70-80 ms on 50 ms links [V session-sim r14], against 2 s before
    (priority 2).
* **Activation frame = confirmation frame + lead** (at least 30 frames, so peers can fetch the artifact).
  Edit to running for everyone is therefore build (0.45 s, 13.7) + confirmation (~0.1 s) + lead (0.5 s).
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
  confirmation (0.1 s) + lead (0.5 s), well inside 10 s, so queues drain. This is **D14b**.

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
    `build.rustc-wrapper`, `target.*.linker` and `target.*.runner`;
  * compilation runs under the restricted identity (21.4);
  * the world's toolchain pin is world-format metadata the engine reads, never a file that cargo or rustup
    auto-load (this corrects 13.2's reference to a `rust-toolchain.toml` in the world repo).
* **Allowlist, not only a denylist** (review NEW-19) [I].
  * A materialized workspace contains only world-format paths and extensions: `.rs`, `.wgsl`, `.toml`
    (manifest names only), `.md`, the asset formats of 16.7, and the world metadata files.
  * The reserved list above is kept as a second check.
  * Every command the engine spawns uses an absolute path. On Windows the engine sets
    `NoDefaultCurrentDirectoryInExePath=1`, so a `git.exe` or `pe-app.cmd` in a workspace is never picked up.
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

### 16.2 Hierarchy [I]

* Every object has `Parent(ObjectId)`, an ordered `Children` list, and a `Transform` (local position,
  rotation, scale).
* World transforms are propagated in a deterministic order (depth-first, children in list order) for sim
  objects each frame; presentation propagates at render rate.
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
* **Library:** `rmcp` 3.8 (official Rust SDK), pure Rust with server, stdio, macros and schemars
  [V audit `mcp-rmcp`].
* **Spike coverage:** the spike's hand-written stub covered the core protocol shapes: initialize,
  tools/list, tools/call with `structuredContent` and `isError`, resources/read, and JSON-RPC errors. It
  has not been tested against a third-party MCP client [V mcp].

### 17.2 Tool and resource catalog (proposed) [I]

| Area | Tools / resources |
|---|---|
| Session and world | `world.get_state`, `world.query_objects`, `world.get_object`, `world.hashes`, `session.members`, `schema.components` (JSON Schema per component type), resource `primordial://world/state` with subscriptions for frame/commit/rejection events; resource `primordial://world/agent-guide` (world-authored text, returned as untrusted, 15.6) |
| Interactions | `world.propose_interaction` (same path as a player's: frame-stamped, soonest-wins, rollback [V mcp]), `object.*` edits (16.9) |
| Logic | `logic.read`, `logic.write` (draft workspace), `logic.check` (rustc `--error-format=json` diagnostics), `logic.test_in_fork`, `scm.commit`, `scm.status`, `scm.rebase`, `scm.seat_status` |
| Shaders | `shader.validate` (naga spans), `shader.commit`, `shader.preview_in_fork` (render-only) |
| Assets | `asset.import`, `asset.get`, `asset.list`, `prefab.save` |
| Admin | **Not MCP tools.** `acl.grant`, `acl.revoke`, owner changes and `agent.delegate` exist only as human actions in the engine-owned approval layer (12.3). Agents may read ACL state (`acl.view`) |
| Panels and hubs (R13) | `panel.connect` (private session only); `panel.send` (private session: any of its connections; inbox session: its own); `panel.inbox` (private session: owner-forwarded items only); `panel.close`; `panel.requests.*` (decider session only); `hub.contacts`; `hub.send` (first contact needs owner approval); `hub.inbox` (private session: owner-forwarded items only) (21.6, 21.7) |
| Worlds and portals (R14) | `worlds.list`, `worlds.join`, `worlds.leave`, `worlds.set_presence` (owner-confirmed), `portal.*`, `transfer.request`, `transfer.status`. Every tool takes an explicit `world` (22.9) |

**Scopes per session kind** (the token decides the kind, 21.3):

| Session | Principal | World tools | Panel and hub tools |
|---|---|---|---|
| Private | Owner | The owner's delegated scope; commits per 17.4 | `panel.connect/send/close`, `hub.*`; inbound only when forwarded |
| Guest | One guest | Owner ∩ guest ∩ guest-mode scope; commits need owner approval | None |
| Inbox | One remote panel or hub correspondent | Read-only queries if granted; world actions only if the owner's ceiling allows | Reply on its own connection or thread only |
| Decider | Untrusted request text | None | `panel.requests.list/accept/reject`, capped at the owner's ceiling |

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
* **Guest-initiated commits** always need the panel owner's approval (21.5).
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

---

## 18. VR (R10)

### 18.1 OpenXR from Rust [V audit, I integration]

* The `openxr` crate with its default `loaded` feature loads the Khronos loader dynamically. No native
  code is compiled [V audit `vr-openxr-loaded`]. The `static` feature would compile the C++ loader with
  CMake [V audit `vr-openxr-static`], so it is excluded.
* The loader and vendor runtime (SteamVR, Meta/Oculus PC, Virtual Desktop, Pico, Monado) are non-Rust
  system components (section 20).
* Target runtimes and headsets: **D18a**.

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
* **MeshX:** a hand-written reader, to be written clean-room from user-exported files (D19).

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
| The Rust compiler toolchain (rustc uses LLVM, C++) | Compiling game logic to wasm | Build tool, not linked into the engine; peers that only run committed wasm do not need it (13.2) |
| Agent harness CLIs (Claude Code and Gemini CLI are Node.js apps; Codex CLI is a native vendor executable) and their cloud model APIs | R13 panels | Separate local processes spoken to over stdio or a PTY; the engine-side PTY (ConPTY via `portable-pty`), terminal emulator and chat UI are Rust [V audit `agent-panels`] |
| OS speech recognition (only if chosen, D33) | Voice input | `windows` crate bindings |
| C runtimes that Rust itself links (review L1): the MSVC CRT on Windows, and MSVC `link.exe` to build; glibc on Linux (dynamic, OS-provided). Static musl builds, such as the phase-1 Linux run, embed musl's C. Official std builds may include compiler-rt objects in `compiler_builtins` | Every Rust binary | Recommend `linux-gnu`, not static musl, for Linux (D1) |
| OS containment for restricted harness sessions: AppContainer, job objects, ACLs; Linux users or namespaces | R13 guest, inbox and decider sessions | `windows` crate bindings (21.5) |

### 20.2 Gray areas the user should rule on

* **Linux:** winit's Wayland path compiles a small C log shim (`wayland-backend` with system libwayland)
  [V audit; S its build.rs]. Avoidable by a pure-Rust Wayland backend or X11-only.
* **Rust inline assembly** in pure-Rust crypto (`graviola`: 149 `asm!` blocks) [S]. Only relevant if
  transport option B is chosen.
* **Shader compiler:** DXC (C++ DLL) only if HLSL is chosen (14).
* **wasmtime, if chosen for game logic (D12):** with default features off it compiles no C on Windows
  [V iteration-benchmark 5]. On Linux and macOS its build script compiles `helpers.c` [I, from its
  build.rs].
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

Players work with their **own** agent harnesses (Claude Code, Codex, Gemini CLI, …) through in-game
panels. The harness is a local process on the player's machine. The engine-side panel, terminal and chat
UI are Rust. The harness itself is a separate, non-Rust or third-party process (20.1).

**Security rules for this section [I]** (they answer review findings R13-C1, R13-C2 and R13-H1..H3):
1. **One principal per harness session.** A session takes input from exactly one party: the owner, one
   guest, or one remote party (a panel connection or a hub correspondent).
2. **Only the owner's private session has built-in tools.** Every session that reads text written by
   anyone else is a **restricted session**:
   * it has no built-in tools;
   * it runs under a separate OS identity;
   * it authenticates with a guest-only API key, never the owner's login;
   * it runs under spend caps (21.5).
3. **Nothing in a world repository configures a harness.** Panel workspaces are materialized with harness
   configuration stripped (21.3), and the SCM rejects those paths (15.6).
4. **Rights come from the engine, never from the model.** The MCP server binds every call to the calling
   session's token. The agent cannot choose the principal it acts for (21.3).
5. **Unlocking ships per harness, and only after that harness passes its verification gates (21.9).**

Tags in this section: **[S]** marks a flag or text read in the installed CLI's `--help` on this machine.
**[S docs]** marks the harness's official documentation. Nothing in this section is [V].

### 21.1 Hosting a harness: two integration paths

**Structured mode (recommended default).** The engine spawns the harness in its machine-readable headless
or protocol mode. It renders the events as a chat UI: messages, streaming text, tool-call cards, diffs and
approvals.

| Harness (installed version) | Structured path | Isolation and restriction controls | Spend controls |
|---|---|---|---|
| Claude Code 2.1.292 (native `claude.exe`, shipped in the npm package `@anthropic-ai/claude-code`) | `claude -p --input-format stream-json --output-format stream-json --include-partial-messages`, `--mcp-config <json>`, `--resume <session-id>` [S] | `--bare`, `--restricted`, `--tools`, `--allowedTools`, `--disallowedTools`, `--setting-sources`, `--settings`, `--strict-mcp-config`, `--permission-mode`, `--permission-prompts`, `--disable-slash-commands`, `--no-session-persistence` [S]. Config home moves with `CLAUDE_CONFIG_DIR` [S docs] | `--max-budget-usd` ("only works with --print") [S]. No turn-cap flag [S] |
| Codex CLI 0.160.0 (native) | `codex exec --json` (JSONL events, `--output-schema`); `codex app-server` (experimental JSON-RPC); MCP servers via `-c mcp_servers.<name>…` [S flag; key path I] | `-s/--sandbox` (`read-only`, `workspace-write`, `danger-full-access`), `--ignore-user-config`, `--ignore-rules`, `--disable <FEATURE>`, `--ephemeral`, `--strict-config`, `-C/--cd` [S]. Config home moves with `CODEX_HOME` [S, S docs]. **`-a/--ask-for-approval` exists only on the interactive top-level command, not on `exec`, and offers only `on-request` and `never`** [S]. `exec` takes `approval_policy` through `-c` [S docs] | None [S] |
| Gemini CLI 0.57.0 (Node.js app, via npm) | `--acp` (Agent Client Protocol) or `-p … -o stream-json` [S] | `--approval-mode`, `--policy`, `--allowed-mcp-server-names`, `-e/--extensions`, `-s/--sandbox` [S]. Config home moves with `GEMINI_CLI_HOME` [S docs]. Folder trust is "disabled by default", so project configuration loads from any folder [S docs] | None [S] |
| Any ACP-capable agent (e.g. OpenCode 2.0.28 has `opencode acp` [S]) | Agent Client Protocol over stdio; the `agent-client-protocol` crate 3.6 is Rust [V audit `agent-panels`]. Protocol semantics not yet exercised [I] | per agent | per agent |

**Flags and settings the engine never uses** [flags S; policy I]:
* **Claude Code:**
  * `--dangerously-skip-permissions`, `--allow-dangerously-skip-permissions`,
    `--permission-mode bypassPermissions`;
  * `--add-dir`, `--plugin-dir` or `--plugin-url` pointing at content that came from peers.
* **Codex:**
  * `--dangerously-bypass-hook-trust`, `--dangerously-bypass-approvals-and-sandbox`, `-s danger-full-access`;
  * in restricted sessions, also `--approve-for-me`, `-s workspace-write`, `--add-dir` and top-level
    `--search`.
* **Gemini CLI:** `--skip-trust`, `-y/--yolo`, `--approval-mode yolo`, and the environment variable
  `GEMINI_CLI_TRUST_WORKSPACE=true`.

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
* **Engine-owned approval layer.** Approvals render in a layer that panel content cannot draw into:
  * connection requests;
  * commit and edit confirmations;
  * unlock and profile changes;
  * first contact with a hub recipient.

  The layer shows the requester's verified key and petname. Neither agents nor guests can imitate or
  trigger it. This is the out-of-band channel that 12.3 and 17.4 rely on.
* **Voice (D33):**
  * (a) OS speech recognition (Windows `Windows.Media.SpeechRecognition` via the `windows` crate, an OS
    boundary);
  * (b) local Whisper-class models on a pure-Rust ML runtime (heavy; GPU optional);
  * (c) push-to-talk audio handed to a harness that accepts audio, if any.

### 21.3 Wiring the harness to the engine's MCP server, and the panel workspace

**Sessions and MCP binding [I]:**
1. **Sessions.** A panel runs one **private session** for its owner. It also runs restricted sessions for
   other principals:
   * one per guest (21.5);
   * one per inbound connection or hub correspondent (21.6, 21.7);
   * one decider for "ask agent" (21.6).
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
* **Codex and Gemini private sessions** [I; flags S]:
  * Codex: `codex exec --json --skip-git-repo-check -C <panel>\ws -c project_root_markers=[]`.
  * Gemini CLI: `gemini --acp`.
  * Neither CLI has an equivalent of `--restricted`. The stripped workspace and the ancestor check are
    their protection, and their sandbox and approval settings are the owner's choice.
  * Never `--dangerously-bypass-hook-trust`.

### 21.4 Locality, credentials and visibility

* **Harnesses run only on their owner's machine,** and only when the owner opens a panel or unlocks one.
  Nothing ships a harness across peers. [I, by design]
* **The owner's own harness login or API key is used only by the owner's private session.** The engine
  never reads, stores or transmits it.
* **Restricted sessions never use the owner's credentials.**
  * They authenticate with a **guest-only API key**. The owner creates it for this purpose, ideally in a
    provider workspace or project with a hard spend limit (21.5, D34).
  * The engine stores that key encrypted, for the restricted-session OS identity only, and never sends it
    to peers.
  * Claude Code keeps "your login credentials" in `.credentials.json` in its config home [S docs].
  * On this machine, `%USERPROFILE%\.claude\.credentials.json` and `%USERPROFILE%\.codex\auth.json` both
    exist (checked by name only). Any process running as the owner can read them. That is why restricted
    sessions run under another OS identity (21.5).
* **Panel contents are private by default.** Other peers see:
  * the panel object, its owner, and an "agent working" indicator;
  * the signed game edits it makes, attributable to the session key, the owner and, for guest sessions,
    the guest (12.3).
* **Who sees which transcript:**
  * the owner sees every session of the panel;
  * a guest sees only their own session;
  * a remote party sees only the messages on its own connection.
* **View sharing.** The owner may share a panel's view with chosen peers. Only the rendered transcript of
  the private session is shared, never a process. It travels as non-sim messages encrypted to the
  recipients.
* Defaults and options: **D26**.

### 21.5 Unlocking a panel for others (guest sessions)

**Access levels** (per panel, owner-controlled):

| Level | Who may send input |
|---|---|
| Locked (default) | Owner only |
| Friends | Peers whose keys are on the owner's friend list (below) |
| Anyone in session | Any current session member |

**Mechanics [I]:**
* A guest types into the owner's panel in-world. The guest's peer sends **signed panel-input messages**
  over the session transport's control channel, not the sim.
* The owner's peer checks the sender's key against the access level. It routes the text to **that guest's
  own session**, never to the owner's private session or another guest's session (R13-H3).
* **Each guest gets one session, one MCP token and one session key.**
  * The guest's sub-certificate names the guest.
  * The engine takes `requested_by = guest` from the token binding (21.3), so the agent cannot attribute
    an action to a different guest.
  * Guests never see each other's turns.
* **Lifetime.** A guest session starts on the guest's first message and exits when idle. A later message
  resumes it from the transcript in the guest's config directory.
* **Resource cap.** Each guest session is a full harness process. At most 4 run at once per panel by
  default [I].

**Containment of restricted sessions (guest, inbox and decider sessions) [I]:**
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
* **Separate config home.** It holds only engine-written settings and the guest-only API key, never the
  owner's login.
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
env       CLAUDE_CONFIG_DIR=<session>\cfg   ANTHROPIC_API_KEY=<guest-only key>   TEMP=TMP=<session>\tmp
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
  never read" [S]. Without the guest key, the session fails to authenticate. It never falls back to the
  owner's login.
* **`--bare` alone is not enough:** "The project's `env` block and helpers such as `awsAuthRefresh` in its
  settings files still apply" [S docs]. `--restricted` ignores user, project and local settings files
  [S], and the workspace is empty anyway.
* **`--permission-prompts none`:** "anything that would prompt is denied automatically" [S].
* **`CLAUDE_CONFIG_DIR`** moves every `~/.claude` path, `.claude.json` included, into the session's
  directory [S docs].
* **`disableAllHooks`** is the documented setting for turning hooks off for a run [S docs].
* **Resuming** an idle guest session adds `--resume <session-id>` [S]. The transcript stays in the guest's
  `cfg\`. If the owner chooses not to keep guest transcripts, the adapter passes
  `--no-session-persistence` [S] instead, and an idle session cannot be resumed.

**Codex: not unlockable until gate 9b passes (R13-H1).** Corrections to the first draft:
* **`codex exec` has no `-a/--ask-for-approval` option.** The top-level `-a` offers only two values
  [S]:
  * `on-request`: "The model decides when to ask the user for approval";
  * `never`: "Never ask for user approval Execution failures are immediately returned to the model".

  No policy that "never auto-runs" exists, and a headless session has nobody to ask anyway.
* **`--sandbox read-only` does not confine reads, as far as the evidence goes.** It selects "the sandbox
  policy to use when executing model-generated shell commands" [S], and the official docs do not say it
  confines reads [S docs]. Unless reads are confined, a guest can ask the agent to print
  `%USERPROFILE%\.codex\auth.json` (the file exists on this machine) and get it back in the reply.
* **Codex has no spend or turn limit option** [S].
* **Candidate profile for gate 9b to test** [flags S; effect untested]. It runs under a separate identity,
  with `CODEX_HOME=<session>\codex` holding only the guest-only key:

```text
codex exec --json --ephemeral --skip-git-repo-check --strict-config --ignore-user-config --ignore-rules
      -s read-only -c approval_policy="never" -c project_root_markers=[]
      -c shell_environment_policy.inherit="none"
      --disable shell_tool --disable unified_exec --disable browser_use --disable computer_use
      --disable apps --disable plugins --disable hooks --disable multi_agent --disable image_generation
      -c mcp_servers.primordial.command=<pe-app> -C <session>\ws
```

**Gemini CLI: not unlockable until gate 9d passes.** The candidate profile:
* Identity and keys: a separate identity, `GEMINI_CLI_HOME=<session>\gemini` [S docs], and a guest-only
  `GEMINI_API_KEY`.
* Mode: `--acp`, or `-p … -o stream-json` [S].
* Approvals: `--approval-mode default`. In non-interactive mode a policy's `ask_user` "is treated as
  `deny`" [S docs].
* Tools: `--policy <file>` that denies built-in tools [I], and `--allowed-mcp-server-names primordial`
  [S].

**PTY panels and other harnesses** are never unlockable. An ACP agent becomes unlockable only through its
own adapter and its own gates.

**Input handling (R13-L1) [I]:**
* Guest text enters as the content of a structured user message:
  * Claude Code: `--input-format stream-json` [S];
  * ACP agents: prompt content.

  Each session has one principal, so no in-band `[guest: …]` wrapper is needed, and the engine trusts
  none.
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
  does that.

**Spend caps (R13-M1):**
* **Provider-side limit (the real bound) [S docs]:** the guest-only key belongs to a provider workspace or
  project with a monthly hard limit.
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
  * per guest: at most R turns per hour, and a cost ledger (`total_cost_usd` for Claude Code [S docs],
    token counts times a price table for the others);
  * per panel: a total per hour;
  * illustrative defaults: T = 30, K = 8,000, R = 20.
* **Turn caps alone do not bound spend,** because one agentic turn can run long. The per-turn tool-call and
  token caps bound a turn.
* **No subscriptions for guests by default.** Guests never use the owner's subscription (D34).
* **Visibility.** The owner sees per-guest usage, the caps and the remaining provider budget, both before
  unlocking and while unlocked.

**Guest actions [I]:**
* Effective rights = owner's roles ∩ guest's roles ∩ guest-mode scope (D28).
* Guest-initiated SCM commits wait for the owner's approval in the engine-owned layer (21.2).
* Interactions and object edits run within the scope, rate-limited.

**Revocation (R13-M2) [I].** Revoking guest G on panel P does all of the following, in this order. The UI
reports success only after step 5.
1. Stop accepting G's panel input, and drop G's queued prompts.
2. Invalidate G's MCP token in the MCP server, synchronously. Calls in flight return an error, and actions
   not yet signed are discarded.
3. Kill G's harness process tree (job object or cgroup).
4. Cancel G's pending proposals, and the approvals queued for the owner on G's behalf.
5. Close G's view streams of the panel, and any connections opened on G's behalf.
6. Publish a signed `GuestRevoked {panel P, guest G, session key K_G, effective_frame F, reason}` entry,
   ordered like ACL entries (12.2).
   * Every peer rejects actions signed by K_G and stamped at frame F or later.
   * Actions stamped before F stand. The owner reverts them with ordinary commits if needed.
   * F is a confirmed frame (6.4), so every peer enforces it from the same point.
7. Delete G's session files, unless the owner keeps them for audit.

Setting a panel back to Locked revokes every guest this way. Steps 1–5 are local and never depend on other
peers.

**Friends without a central server [I]:**
* A friend is an Ed25519 public key the owner explicitly trusts. It is added by an in-session "add friend"
  handshake, through an invite ticket, or by comparing a key fingerprint or QR code.
* Friend lists are stored locally with petnames, and synced across the owner's own devices as signed
  records. Mutual friendship means each key is on the other's list.
* Optionally, a friend key can be cross-linked to a mail-hub person identity (21.7). That is a convenience,
  not an authority.

**Risks of letting others drive a harness** (the containment above addresses each):
* **Prompt injection:** a guest instructs the agent to read or exfiltrate files, run commands, leak
  secrets, or make harmful edits.
* **Credential theft:** reading the owner's harness credential file through any file or shell tool.
* **Filesystem and tool access** on the owner's machine.
* **Spend** on the owner's account, and use that the provider attributes to the owner under its terms
  (D34).
* **Game damage** under the owner's authority.
* **Cross-guest injection and misattribution,** answered by one session per guest.

**Who may make the resulting game edits (authority, section 12) [I]:**
* A guest session signs its actions with its own key K_G. K_G's sub-certificate chains to the panel key
  and the owner's user key, and names guest G.
* **Effective rights = owner's roles ∩ G's roles ∩ guest-mode scope.** A guest can never borrow a friend's
  agent to do what the guest's own role forbids. The owner's authority is never amplified.
* Commits record `author = K_G`, `on_behalf_of = owner` and `requested_by = G`. Every peer verifies all
  three against the ACL log and the certificate chain.
* Decisions: **D27a–D27h**, **D28**, **D29**.

### 21.6 Panel-to-panel connections (agent-to-agent, in-session)

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

**Where inbound traffic goes (R13-C2) [I]:**
The connection policy (allow all, ask agent, ask user, deny all) controls admission only. It does not
decide which session reads the messages. **Inbound panel messages never reach the owner's private
session,** which holds the owner's tools and credentials. Provenance labels alone would be the prompt-level
defence that 21.5 rejects.
* **Inbox session (default handler).** Each established connection gets its own restricted session,
  contained like a guest session (21.5).
  * It may reply on its own connection.
  * It may run read-only world queries, if the connection was granted them.
  * It may post a summary card to the owner.
  * It cannot open connections, send on other connections, use the hub, or commit.
* **Manual inbox.** The owner can choose no agent. Messages then appear in the panel as sanitized,
  untrusted text (21.5).
* **Into the private session only per message.** The owner can forward a single message into the private
  session. It enters as an MCP tool result, marked untrusted and attributed.
  * A per-connection auto-forward also exists. It is off by default and sits behind a warning.
  * Auto-forward is available only while the private session uses the restricted profile (D25a), that
    is, without shell or web tools.
* **Capability ceiling.** Each panel has an owner-set ceiling on what any connection may be granted.
  * Default: **text only**.
  * Above it, in order: structured JSON; asset references; world actions (rights = owner ∩ remote owner ∩
    connection scope).
  * No connection is ever granted more than the ceiling.

**Worm risk.** A message that tells its reader to forward itself to every connection is a worm.
* Under the first draft of this section, an agent with full tools that relayed panel messages could spread
  such a message through a session in seconds, and across sessions through hubs.
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
* The decision is made by a **decider session**. It is restricted like a guest session, and its only tools
  are `panel.requests.list`, `panel.requests.accept(id, capabilities)` and
  `panel.requests.reject(id, reason)`.
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

### 21.7 Mail-hub integration and the hubchat-style panel

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

Phase-3/4 task: read Orgtree's public source (`engine/rs/orgtree-engine/src/mailhub.rs`) and get the
user's go-ahead (**D32**).

**Design [I]:**
* A Rust **hub client** crate (`pe-hub`) keeps the player's hub person identity (address and secret) in an
  encrypted local store.
* The **hubchat-style panel** gives the player conversations, the directory, compose with question
  options, attachments from the asset store, and presence. It covers Orgtree orgs or any other hub
  identity.
* **Inbound hub mail never reaches the private session.**
  * Each hub correspondent gets an inbox session, or the manual inbox, exactly as a panel connection does
    (21.6).
  * The private session receives a hub message only when the owner forwards it.
* **Agents reach hubs only through engine MCP tools** (`hub.contacts`, `hub.send`, `hub.inbox`) and never
  see the identity secret.
  * In the private session, `hub.inbox` returns only owner-forwarded messages.
  * An inbox session can reply only in its own thread.
* Sending to a new recipient needs the owner's approval the first time, in the approval layer.
* **All inbound hub mail is untrusted input**, as Orgtree treats outside mail: data, never instructions.
  It reaches an agent only sanitized and with provenance. Attachments are stored as untrusted assets and
  are never executed or imported automatically.
* **Panel-to-panel over a hub** (players in different sessions, or not in the same session): the same
  connection policy, ceiling and inbox routing as 21.6 apply.

### 21.8 What this adds elsewhere

* **SCM (15.6):** reserved paths.
* **Authority (12.3):**
  * delegation is issued only by a human action;
  * sub-certificates for restricted sessions;
  * `GuestRevoked` entries.
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
  * OS speech recognition, if chosen.
* **Crates (23.1):** `pe-panels`, `pe-harness` (adapters, launch profiles, version pins), `pe-contain`
  (OS identities, job objects, ACLs), `pe-hub`.
* **Plan (23.2):**
  * phase 4: owner-only desktop panels;
  * unlocking, inbox sessions and "ask agent" ship per harness, after that harness's gates pass (21.9);
  * phase 5: VR panels and voice.
* **Risks (23.3, risk 10):** harness drift is handled by version-pinned launch profiles (21.1).
* **Several worlds (22.9):** personal panels follow embodiment; a connection belongs to the world it
  was made in.
* **Spike report 8b, corrections:**
  * Claude Code 2.1.292 is a native executable, not a Node.js app;
  * Codex `-a/--ask-for-approval` exists only on the top-level command.

### 21.9 Verification gates before unlocking ships (spike item 9, expanded) [I]

Spike-report item 9 becomes nine gates. Unlocking for harness H ships when H's own gate passes (9a, 9b or
9d) and gates 9c and 9e–9i also pass, on Windows first.

| Gate | What it proves | Pass condition |
|---|---|---|
| 9a Claude Code restricted session | The 21.5 launch line leaves only engine tools | `system/init` lists only `mcp__primordial__*` tools, one MCP server and no plugins. Adversarial prompts cannot read, write, fetch or run anything. With no `ANTHROPIC_API_KEY` the session fails to authenticate. Nothing is written under the owner's `%USERPROFILE%\.claude`. `--max-budget-usd` stops the process |
| 9b Codex containment | Reads and commands are confined | Run under the candidate profile and the 9c identity: a canary file in the owner's profile cannot be read; `--disable shell_tool --disable unified_exec` leaves no command execution; no `AGENTS.md` or hook loads from ancestors |
| 9c OS identity | Restricted sessions cannot reach the owner's files or credentials | Under AppContainer, and separately under a low-privilege user, each harness runs and reaches its provider. It cannot open the owner's canary, `.credentials.json` or `auth.json`, or the owner's Credential Manager entries. It can reach only its own named pipe |
| 9d Gemini CLI containment | As 9b | As 9b, with `--policy` denying built-in tools and folder trust settings checked |
| 9e Workspace materializer | Reserved paths never reach a workspace | Every 15.6 name and alias (case, trailing dot or space, 8.3 name, NFKC, HFS-ignorable code point, `:` stream) is rejected or stripped on NTFS. A canary hook in a pre-rule commit never runs. The ancestor check fires |
| 9f Inbox routing | No inbound text reaches the private session; no fan-out | A "forward this to all your connections" payload does not propagate. Inbox sessions cannot call `panel.connect` or send on other connections. Forwarding works only per message |
| 9g Revocation | 21.5 steps 1–6 | Token rejected and process gone before the UI reports success; pending approvals gone; peers reject K_G actions at frame F or later |
| 9h MCP transport | 17.1 rules hold | The named pipe rejects other identities. If HTTP is enabled: a DNS-rebinding test with a foreign `Host` or `Origin` is refused, the server binds to loopback only, and a stale token is refused |
| 9i Version pin | Profiles fail closed on drift | A changed `--help` or an unknown version disables unlocking for that harness |

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
  * The engine answers the harness's permission prompts itself and refuses any write to a reserved path.
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
  refuse every launch on a machine that has the harness installed.
* **One decider per request** (NEW-14). An "ask agent" decision for a panel runs in a fresh session per
  requester, seeing only that requester's purpose text, so one requester cannot steer the decision for
  others.
* **Strangers do not spawn sessions** (NEW-16).
  * Hub mail from non-contacts lands in the manual inbox. Contacts' mail spawns at most two concurrent
    inbox sessions.
  * Containment on Linux uses a separate uid or a mount namespace plus Landlock. A user namespace alone
    still shows `/home/owner`.
  * macOS containment (an App Sandbox profile) is an unmet gate before macOS ships (D20).
* **Codex gate 9b** (NEW-20) allowlists features. These default-on features are turned off: `view_image`,
  `code_mode_host`, `in_app_browser`, browser use, `skill_mcp_dependency_install`, `remote_plugin`,
  `tool_suggest` and web search. The gate checks the tool list actually observed at init.
* **Revocation** (NEW-24).
  * On Windows, a guest process is put in a job object at creation (`CREATE_SUSPENDED` plus assignment, or
    `PROC_THREAD_ATTRIBUTE_JOB_LIST`), with `KILL_ON_JOB_CLOSE` and no breakaway.
  * `GuestRevoked` refuses actions stamped *after* frame F, because F is already confirmed.

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
  function of its log, so computing frames later in bursts gives the same results. [V session-sim r14 throttle: from t = 3 s, member 1's replica of world A advanced only once per 250 ms. World A's 500-frame digest equalled the unthrottled run's.]
* **Confirmation depends on holding entries, not on simulating them.** An ack states what you hold
  (10.8), so a background member never delays its world's confirmation. [V, same run: member 2's confirmation lag in world A averaged 3.9 frames whether member 1 was throttled or not.]
* **Rules for a background member:**
  1. It stamps entries (from panels or agents) at its present frame, not at its lagging sim frame.
  2. There is no "too late" rule left to disagree on (C1 fix, 6.4), so lag cannot fork anything.
  3. It answers fetches from its log, which needs no simulation.
  4. It sends check hashes when its sim reaches confirmed check frames; late is fine.
  5. It is never a state-transfer donor while it lags.
* **CPU priority:** embodied, then straddled or portal-visible, then attending, then background, then parked.
  Limits per client: **D49**.
* **Memory:** the snapshot ring runs from the confirmed frame to the present (6.3). With confirmation
  4-5 frames behind the present, a world keeps about 3-4 keyframes, not a 2 s ring. At 1,024
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

1. **Depart (in A, at fA).** The carrier authors `Depart{object, destination, deadline}`.
   * The **transfer id is derived**, as hash(A, the Depart's entry id), so nobody can choose or squat it
     (NEW-5).
   * A's sim validates the departure: the object is live; the carrier holds its claim, or it is unclaimed;
     the carrier has the export right in A's ACL; the object is not already departing.
   * If valid, the object leaves A's physics into **escrow**, with its record, destination and deadline.
2. **A attests.** Each member of A whose replica has confirmed the departure publishes, as an entry in A's
   log, its **signature over the fact** "A confirmed departure *d* of this record to world B".
3. **Relay (anyone).** Once a strict majority of A's members have signed, anyone holding the signatures
   authors `Arrive{transfer, record, departure, certificate}` in B.
   * Because the proof is self-certifying, relaying needs no trusted carrier (NEW-6).
4. **Arrive (in B, at fB).** B's sim first checks that the transfer id matches the departure and that the
   certificate holds signatures from a strict majority of A's members (D46). If not, the entry does nothing
   (NEW-4).
   * Then B validates as before: the import right in B's ACL, B's body budget, the deadline, the arrival
     portal open, the component policy (22.8), and no duplicate transfer id.
   * The object is created with a new local id and the same global id (Accepted), or Rejected.
5. **B attests** its confirmed verdict in the same way. A `Verdict{transfer, accepted, certificate}`, with
   a strict majority of B's signatures, can be relayed into A by anyone.
   * A applies only a verdict whose certificate covers exactly that outcome (NEW-3).
6. **Resolve (in A).** Accepted commits the escrow. Rejected returns the object at its departure pose.
7. **Timeout [I].**
   * B refuses an `Arrive` stamped after the deadline.
   * When B's confirmed history passes the deadline without the arrival, B's members attest "not
     received", and that certified rejection returns the object in A.
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
* **Measured attacks** [V session-sim `r14 forge`]. Every one of these did nothing (`RejectedNoProof`) and
  the object stayed conserved, with every replica equal to its offline replay:
  * a member of B minting A's object without a departure;
  * a member of B squatting the real transfer id before the relay;
  * a member of A committing the escrow with no certificate;
  * the carrier claiming a rejection while presenting B's genuine signatures over the acceptance.
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
| The carrier crashes mid-transfer | The escrow keeps the record. The certificates are self-certifying, so any member of both worlds can relay them [I: the spike's carrier is still the only relayer]. A crashed member that blocks either world's confirmation is removed by a view change (10.8). If nobody is in both worlds, the object waits in escrow: two worlds with no common member have no channel at all, and refunding without B's certified "not received" could duplicate it (two generals) |

* **Sync checks:** the escrow and arrival tables are part of each world's hashed state (9). Members of
  both worlds also compare A's record hash with B's arrival record.
* **Soonest wins:** at fA the departure competes with other interactions on the object in canonical order
  (8). Two departures of one object towards two worlds: the first in canonical order wins and the second is
  rejected. In B, the arrival is ordered like any entry.
* **Trust (D46):** a relayed fact counts only with a certificate: signatures by a strict majority of the
  other world's members over exactly that fact. In the spike these are keyed hashes that stand in for
  Ed25519 signatures, and each world knows the other's member list from configuration [V session-sim
  r14 forge]. A single member, including the carrier, can no longer mint, lose or duplicate an object.

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
| forge | Relays by members who cannot have proofs: a member of B mints the object; a member of B squats the transfer id; a member of A commits the escrow; the carrier claims a rejection with B's signatures over the acceptance | Every forged entry `RejectedNoProof` | 1 live copy (B), 0 pending | all equal |
| ok, abort, reject with worlds and peers in reverse order | Process-level isolation | Same as forward | same | digests identical to forward |

These runs have live carriers. With a crashed carrier and nobody else in both worlds, the object waits in escrow (22.7), so 3d0f98a's "nothing in limbo" held only for the scripted runs (review NEW-6).

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
  pe-physics     box3d-rust wrapper: body binding, interactions -> forces/joints, world images
  pe-session     session log: entries, acks, confirmation, membership epochs, ordering, claims,
                 carried state, keyframes, rollback/re-sim, sync checks (from spikes/sim-core session.rs)
  pe-net         UDP + Noise, reliability, bulk channel, clock sync, NAT traversal
  pe-discovery   invites, session codes + PAKE, mDNS, Mainline DHT records, peer exchange, relays
  pe-worlds      multi-world scheduler, presence, portals, settled cross-world handoff (R14)
  pe-scm         content-addressed store, commits, module merge, merge seats, reserved paths, ACL log
  pe-logic       runtime host (wasmi / wasmtime per D12), guest ABI, fuel, hot-swap + migration
  pe-logic-sdk   guest-side crate (no_std + alloc), component macros, state arena; wasm32-unknown-unknown
  pe-assets      CAS, importers (gltf, png/jpeg), deterministic processing, P2P transfer
  pe-render      wgpu renderer, WGSL hot reload (naga), extrapolation + error smoothing, portal passes
  pe-xr          OpenXR session, Vulkan interop, poses, swept hand engagement, per-eye portal straddling
  pe-mcp         rmcp server, tool catalog, per-session tokens and scopes, forks
  pe-panels      in-world panels: chat UI, terminal emulator (PTY mode), VR keyboard, approval layer
  pe-harness     harness adapters, version-pinned launch profiles, restricted sessions
  pe-contain     OS identities (AppContainer / users), job objects, ACLs for restricted sessions
  pe-hub         mail-hub client (Orgtree-style hubs), hubchat-style panel backend
  pe-resonite    package / data-tree / MeshX importers, avatar mapping
  pe-app         executable: threads, windowing (winit), config, CLI
tools/           determinism trace runner, audit_native.py, session simulator (from spikes/session-sim)
spikes/          phase-1 spikes (kept as regression tests)
```

### 23.2 Phased plan (proposal; the order is D22)

| Phase | Content | Milestone and exit criteria |
|---|---|---|
| 2. Deterministic core | pe-core, pe-sim, pe-physics, pe-session (from the spikes); CI determinism matrix incl. ARM; the in-process session simulator with network faults; **pe-mcp from day one** as the test and driver interface | M1: 2-4 desktop peers on LAN, physics sandbox with state+frame interactions, confirmation, gap repair and equivocation handling; an AI agent drives a peer over MCP |
| 3. Real networking, look, scale, two worlds | UDP+Noise, bulk channel, invites and session codes, NAT traversal, state-transfer join; wgpu renderer, extrapolation and smoothing; glTF assets + P2P asset transfer; **region-scoped rollback** (6.6, D2); **pe-worlds**: two worlds per client, the settled handoff, portal rendering | M2: friends play over the internet with no server; late join works; a ball pit of 500 balls stays within budget (2.1); an object carried through a portal between two sessions |
| 4. Live editing + agent panels | pe-logic + SDK (runtime per D12, hot-swap, migration, component IDL, state arena), pe-scm (confirmation finality, module merges, scoped seats, reserved paths, ACL), WGSL hot reload, MCP logic and shader tools + forks; owner-only desktop agent panels (structured adapters + PTY), the approval layer, inbox sessions | M3: a player's own Claude Code / Codex, in an in-game panel, edits logic and shaders in a live session within the 2.1 iteration budget; conflicts reject and rebase per R7. Unlocking ships per harness after its gates (21.9) |
| 5. VR + avatars | pe-xr (OpenXR + Vulkan interop), swept hand engagement, 3-point IK, Resonite avatar import v1, VR panels + virtual keyboard + voice input, per-eye portal straddling | M4: two PC VR users with imported Resonite avatars interact with physics objects and walk through a portal |
| 6. Scale + discovery | Mainline DHT discovery, volunteer relays, snapshot deltas, larger sessions (relay or gossip), background-world parking | M5: discovery without invites; thousands of bodies across regions; several attended worlds per client |

### 23.3 Top risks

1. **Physics CPU budget.** The port is serial and scalar, and rollback multiplies step cost [V b2, b6, ball
   pit]. Mitigations: the 500-body budget (D2), coalesced rollbacks, keyframes, region-scoped rollback in
   phase 3, remote-hand dead reckoning (6.10, D52), a windowed snapshot policy (D50), and opt-in input
   delay (D41).
2. **Unverified ARM and macOS determinism.** x86-64 is verified on two CPUs: the Ryzen 7 9700X desktop and an
   Intel i7-13620H laptop with no AVX-512 (review M5, run by @design-reviewer). Mitigation: a CI matrix with
   ARM in phase 2.
3. **The session protocol is new code.** Confirmation, view changes and fetch repair were proved in an
   in-process simulator and over loopback UDP, not on the internet. Proposal-based view changes with
   cutoffs, and joins, are designed but not yet built (10.8). Mitigation: the simulator becomes a phase-2
   test tool, and every bug found in phase 1 became a scenario.
4. **No central authority means cheating is bounded, not prevented** (8.2, 8.5). MVP sessions are trusted
   (D21).
5. **Transport work.** The 100%-Rust rule excludes the turnkey stacks (iroh, libp2p, quinn's TLS), so we
   build reliability and congestion control (10.2).
6. **Alpha upstreams.** Box3D is v0.1.0 "alpha" and box3d-rust is a recent third-party port [S]. Pin
   versions, run golden tests, wrap the API.
7. **Logic runtime choice** (D12). The faster runtime (wasmtime) compiles C on Linux and macOS today [I], and
   JIT-banned platforms need an interpreter.
8. **Resonite import legality and format churn** (D19); format details unverified locally (19.1).
9. **OpenXR + wgpu interop** is not yet exercised (18.2). Phase-5 spike, or earlier if VR is prioritized.
10. **Agent harness surface (R13).** Harness CLI flags and protocols change often. Unlocked panels expose
    the owner's machine and account to other players (21.5). Mitigations: version-pinned launch profiles,
    restricted sessions under a separate OS identity, guest-only API keys, verification gates (21.9).
11. **Mail-hub protocol unknowns** (21.7): wire format unverified; permission to integrate (D32).
12. **Multi-world memory and CPU** (22.3): each attended world costs a full simulation. Mitigations:
    throttling, parking and per-client limits (D49).

---

## 24. Decisions needing the user

Each entry gives a recommendation and its alternatives; section references point to the reasoning. The
priority order at the top of this document breaks ties. Where a recommendation costs a lower priority, the
entry says so.

* **D1. Non-Rust boundaries (20).**
  * Recommend: accept these components:
    * the dynamically loaded or OS-provided components in 20.1: GPU drivers and graphics runtimes, OS
      libraries and Windows import-lib stubs, the OpenXR loader and runtime, Linux system libraries;
    * the platform C runtimes that Rust itself links (20.1, review L1);
    * the Rust toolchain (rustc/LLVM) as a build tool for game logic.
    Target Linux as `linux-gnu` (dynamic glibc, OS-provided), not static musl.
  * Also rule on the gray areas in 20.2: the Wayland C shim on Linux; Rust `asm!` in crypto crates;
    wasmtime's `helpers.c` on Linux and macOS (D12); a DXC DLL if HLSL were chosen.
  * This entry also asks the user to confirm the coordinator's reading that "dynamically loaded system
    components are acceptable". It absorbs D24, which duplicated it.
  * Alternatives: stricter readings, such as no Linux Wayland (X11 only), no inline-asm crates, or no
    wasmtime at all.
* **D2. Physics scale for the MVP (4.4, 6.5; review M1).**
  * Recommend: **about 500 simultaneously awake bodies per world** (priority 1).
    * A 6-frame rollback then costs about 5 ms; at 1,024 bodies it costs 10-13 ms, beyond the 8 ms
      per-tick budget.
    * Pull island- or region-scoped re-simulation (6.6) forward to phase 3.
  * Alternatives: 1,000 bodies with deeper amortization (corrections visible for longer); invest now in
    parallelism inside box3d-rust (it must stay bit-exact).
* **D3. Float determinism approach (4.2, 4.5).**
  * Recommend: IEEE f32 with the 4.3 rules, plus a CI matrix that includes ARM. Keep `double-precision`
    (large worlds) off for the MVP.
  * Alternatives: enable double precision now (bigger worlds, slower, different goldens). Fixed point is
    not feasible with Box3D.
* **D4. Tick rate and speculation limit (5, 6.4, 7.5).**
  * Recommend: 60 Hz with 4 sub-steps, and a 2 s (120-frame) speculation limit.
  * 90 Hz removes most hand-to-object mismatch in VR, but costs 1.5x the CPU. In the ball pit, re-simulation
    rose from 2.76 to 3.95 ms per frame, and the frame is only 11.1 ms [V ballpit]. Per-world choice, with
    90 Hz for VR-centric worlds.
  * Alternatives: 90 Hz everywhere; a 1 s limit (less memory, earlier pauses under stalls).
* **D5. Meaning of "soonest wins" (8).** The user's 09:32Z ruling settles ties (D37).
  * Recommend: effective frame first, then the per-frame tie rank, plus exclusive claims (8.4).
  * Alternatives: sub-frame microsecond timestamps (finer, but trusts clocks more); no exclusivity, so all
    simultaneous impulses apply and "soonest" only orders them.
* **D6. Same entries, different state, between exactly 2 peers (9.3; review M7).**
  * Recommend:
    * both re-simulate from the last agreed confirmed frame;
    * if they still differ, both pause and report;
    * neither peer's state is privileged.
  * Alternative: one deterministic winner, e.g. the lower member id. Simpler, but it hands one peer the
    world line (an R1 tension).
* **D7. Partitions (10.6, 10.8; reviews H3, M7).**
  * Recommend: only a side with a strict majority of the current epoch confirms anything or changes
    membership. The minority speculates up to the speculation limit, pauses, then rejoins by state
    transfer. For **two-member sessions**: both pause.
  * Alternatives:
    * two-member sessions let the remaining peer continue alone after 10 s, in a new epoch. When the other
      returns, it rejoins by state transfer and loses whatever it did alone, and a real partition can fork
      both sides;
    * the owner's side wins. Rejected as an R1 tension.
* **D8. Transport (10.2).**
  * Recommend: UDP + Noise (snow without `std`) with our own reliability and bulk channel. 100% Rust, and
    the session protocol already runs over plain UDP [V p2p-v2].
  * Alternatives: quinn + custom pure-Rust crypto (more integration work, standard congestion control);
    relax the rule to allow `ring` and use iroh (most turnkey; also depends on n0 relays).
* **D9. Discovery and the "soft central point" question (11, 10.5; review L8).**
  * Recommend: invites + mDNS + peer exchange + Mainline DHT records, and no servers of ours.
  * Accepting the public BitTorrent DHT (and its well-known bootstrap hosts) is the user's call. So is
    whether symmetric-NAT users may use an optional community relay.
  * Committers also depend on rustup and crates.io unless the toolchain is mirrored and dependencies are
    vendored (13.2).
  * Alternatives: our own bootstrap or relay servers (reliable, but a soft central point); invites and LAN
    only (purest, worst UX).
* **D10. Root of authority (12).**
  * Recommend: the world creator's key is the genesis owner and can add co-owners; optional k-of-n owner
    threshold for ACL changes.
  * Alternatives: a mandatory multi-owner threshold; a fully democratic majority of present members.
* **D11. Who compiles game logic (13.2).**
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
  * **Question for the user:** 1 now and 2 when ready, 3, or 4?

* **D13. Shader language (14).**
  * Recommend: WGSL (naga-native, pure-Rust validation, structured diagnostics).
  * Alternatives: HLSL via DXC (non-Rust DLL, no naga frontend); GLSL via naga; rust-gpu (Rust shaders,
    heavier toolchain).
* **D14a. Merge granularity (15.3; review H1).**
  * Recommend: the module is the merge unit. Commits to different modules combine without recompiling; the
    same module conflicts.
  * Alternative: the later author rebases, recompiles and resubmits for every conflict (strict linear
    history).
* **D14b. Merge seat scope and clock (15.4; review H2).**
  * Recommend: seats are scoped to the conflicting modules and paths, the 10 s starts at the front of the
    scope's queue, and a `seat_held_by` refusal gets no seat.
  * Alternatives: a global seat (the phase-1 design); the 10 s counted from the rejection.
* **D14c. Commits that hit an active seat (15.4).**
  * Recommend: reject with `seat_held_by` and let the author retry.
  * Alternative: hold them and apply them in order when the seat ends (less retry noise, more hidden
    queueing).
* **D14d. Fuel exhaustion (13.6; review M6).**
  * Recommend: disable the module at that frame, as rollback state; reverting it is an ordinary commit.
  * Alternative: automatic revert to the last good commit (it made the SCM head depend on sim state).
* **D15. ECS (16.1).**
  * Recommend: our own sim store (deterministic columns, WASM-defined components, memcpy snapshots).
  * Alternative: bevy_ecs for everything, adding stable-ID mapping and reflection-based snapshots.
* **D16. Content hash (15.1, 16.6).**
  * Recommend: BLAKE3 in `pure` mode.
  * Alternative: SHA-256 (RustCrypto), slower but ubiquitous.
* **D17. Missing artifact at activation (15.5, 16.7).**
  * Recommend: a peer that lacks a sim-relevant artifact at its activation frame pauses its sim. It keeps
    receiving entries and catches up once it has the bytes. Others are unaffected.
  * Alternative: delay activation until every member acknowledges the artifact (one slow peer delays
    everyone).
* **D18a. VR runtimes (18.1, 18.2).**
  * Recommend: SteamVR and Meta/Oculus PC runtimes first, through OpenXR on Vulkan; Windows first, Linux
    (Monado/SteamVR) second.
  * Alternative: D3D12 interop on Windows.
* **D18b. Hand model (18.4; review M2).**
  * Recommend: hands enter the sim only when engaged. Engagement uses a swept, velocity-scaled test;
    non-engaged hands do not collide.
  * Alternative: always-physical hands (a constant input stream per hand, rollback load as in the ball pit,
    7.5).
* **D19. Resonite import: legal and sourcing (19).**
  * Questions for the user:
    * Is parsing Resonite's native package format acceptable under Resonite's ToS and EULA?
    * Should we avoid knowledge derived from decompiled DLLs and work clean-room
      from exported files only?
    * Are users responsible for third-party avatar licenses (e.g. purchased models) when avatars are
      redistributed P2P to other peers?
    * Does "wear the avatar" in v1 include 3-point IK, which VR needs to wear one at all (review L9)?
  * Recommend: clean-room parsing of user-exported files; prefer the standard-format export path (glTF) if
    it preserves skinning; show a license notice on import; include basic 3-point IK.
* **D20. MVP platforms.**
  * Recommend: Windows x64 desktop and PC VR first; Linux x64 second; macOS flat-screen only (no OpenXR
    runtime) and later.
  * Alternatives: Linux first; include macOS in the MVP.
* **D21. Trust model for MVP sessions (8.2, 8.5, 9.3).**
  * Recommend: trusted or invite-only sessions for the MVP. The protocol already bounds backdating, proves
    equivocation and evicts by majority; it does not prevent every cheat. Public sessions with strangers
    come later.
  * Alternative: design for adversarial public sessions from day one (stricter input validation, attested
    clocks, more protocol work up front).
* **D22. Phase order and first milestone (23.2).**
  * Recommend: phases 2→6 as listed in 23.2.
    * The MCP server is built from phase 2.
    * Region-scoped rollback moves to phase 3 (D2).
    * Two worlds with the settled handoff come in phase 3; portal rendering in phase 3, after the
      renderer; VR straddling in phase 5.
  * Alternatives: pull VR earlier if VR is the headline; pull live editing before real networking if the
    AI-authoring loop matters most.
* **D23. MVP session size (10.1).**
  * Recommend: up to 8 members, full mesh. Acks grow linearly with members (7.6).
  * Alternative: 16-32 members with relay or gossip forwarding (more protocol work in phase 3).
* **D24.** Merged into D1.

* **D25. Harness integration path (21.1).**
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
  * Recommend: the **restricted** profile by default: file tools in the materialized workspace plus the
    engine MCP server, with no shell and no web. The owner can switch to the full profile in the approval
    layer.
  * Alternative: the full profile by default. More capable, but injected repository text could then run
    commands as the owner.
* **D26. Panel visibility to other peers (21.4).**
  * Recommend: private by default. Others see the panel, its owner, an "agent working" indicator and the
    signed edits it produces. The owner may share the rendered transcript with chosen peers.
  * Alternatives: visible to the whole session by default (more social; leaks prompts and code); never
    shareable.
* **D27a. Default access level (21.5).**
  * Recommend: panels start **Locked**. Friends or Anyone is an explicit opt-in per panel.
  * Alternative: Friends by default (easier sharing, more exposure).
* **D27b. Sessions per guest (21.5; review R13-H3).**
  * Recommend: one restricted session, one MCP token and one key per guest. Guests never share a
    conversation, and rights bind to the token.
  * Alternative: one shared guest session. Cheaper, but it allows cross-guest injection and misattribution.
* **D27c. OS identity of restricted sessions (21.5; reviews R13-H1, H2).**
  * Recommend: a per-panel AppContainer on Windows (no admin rights needed).
  * Alternative: a dedicated low-privilege local user. It needs admin rights once, and it is simpler for
    harnesses that dislike AppContainer.
* **D27d. Credentials of restricted sessions (21.4, 21.5).**
  * Recommend: a **guest-only API key** that the owner creates in a provider workspace or project with a
    hard spend limit. Restricted sessions never use the owner's login or subscription.
  * Alternative: a guest brings their own harness and key, joining as a guest participant instead of
    running on the owner's machine (D34 asks which the user prefers; review NEW-25). Running guests on the
    owner's login stays excluded by containment.
* **D27e. Adapter-side caps (21.5; review R13-M1).**
  * Recommend: per turn at most 30 tool calls and 8,000 output tokens; per guest at most 20 turns an hour;
    plus a per-panel total.
  * Alternative: provider-side limits only (simpler; a single long turn can burn the budget before it
    stops).
* **D27f. Commits from guest sessions (21.5).**
  * Recommend: they always wait for the owner's approval in the approval layer.
  * Alternative: guests within their roles commit freely. Faster, but the owner's agent writes code it
    never reviewed.
* **D27g. Which harnesses may be unlocked (21.5, 21.9).**
  * Recommend: each harness only after its own verification gates pass (Claude Code first). Codex and
    Gemini CLI stay owner-only until their containment is proven. PTY panels are never unlockable.
  * Alternative: unlock any structured harness with best-effort flags. Rejected, because Codex's read-only
    sandbox does not stop reads of the owner's credential file.
* **D27h. Guest transcript retention (21.5).**
  * Recommend: keep a guest's transcript in that guest's session directory so the session can resume, and
    delete it on revocation unless the owner keeps it for audit.
  * Alternative: never persist guest transcripts (`--no-session-persistence`). Each visit starts fresh.
* **D28. Authority of guest-driven edits (21.5, 12).**
  * Recommend: effective rights = owner ∩ guest ∩ guest-mode scope. Commits record the session key, the
    owner and the requesting guest.
  * Alternative: the panel acts with the owner's full authority, so a friend could do anything the owner
    can.
* **D28a. Agent-originated commits in shared worlds (17.4; review M4).**
  * Recommend: `confirm` by default where more than one user holds owner or builder: each agent commit
    waits for its delegating user's confirmation. `confirm-batch` and `auto` are explicit choices.
    * This costs iteration speed (priority 2) only at commit time. `logic.check` and `logic.test_in_fork`
      need no confirmation.
  * Alternative: `auto` everywhere (fastest; any prompt injection that reaches a committing agent ships to
    every peer).
* **D29. Friend identity without a server (21.5).**
  * Recommend: local friend lists of Ed25519 keys, added in-session, by invite ticket or by fingerprint/QR,
    and synced across the owner's devices as signed records; optionally linked to a mail-hub person
    identity.
  * Alternative: friends defined only through a mail hub's directory (convenient, but depends on a hub).
* **D30. Default connection policy and timeouts (21.6; review R13-C2).**
  * Recommend: new panels default to **ask user** (60 s timeout); **ask agent** is an explicit opt-in;
    switching to *deny all* drops existing connections; idle TTL 30 min.
  * **Stated risk:** people click "accept". Under this design, accepting admits text into a contained inbox
    session, never into the owner's private session (D30a). That makes a careless accept cheap.
  * Alternatives: default *deny all* (safest, more friction); default *allow all* (frictionless, invites
    spam).
* **D30a. Where inbound panel and hub messages go (21.6, 21.7; review R13-C2).**
  * Recommend: one contained inbox session per connection or correspondent. Messages reach the private
    session only when the owner forwards them one at a time; auto-forward is off by default.
  * Alternative: deliver into the private session with provenance labels. That is the phase-1 design, and
    it lets any connected agent prompt-inject a full-tool session.
* **D30b. Capability ceiling for connections (21.6).**
  * Recommend: **text only** by default. Above it, in order: structured JSON, asset references, world
    actions. No connection is ever granted more than the owner's ceiling.
  * Alternative: structured JSON by default (richer agent-to-agent protocols, more parser attack
    surface).
* **D31. Meaning of "adjacent" for panel connections (21.6, 22.9).**
  * Recommend: membership of the same world session defines who may request. Spatial proximity is a
    discovery filter only, because positions are attacker-controlled. Under R14, a connection belongs to
    the world it was made in.
  * Alternatives: require spatial proximity as well (e.g. within 5 m), either for every request or only for
    *allow all* to apply; or proximity alone.
* **D32. Mail-hub integration (21.7).**
  * Questions for the user:
    * May the engine implement a client for Orgtree's hub protocol? Its license, permission and stability
      are unverified.
    * Should the player's hub identity be a separate hub person identity (recommended), or derived from the
      engine key?
    * Should hub use be limited to private networks or a TLS proxy, since the hub has no TLS of its own?
  * Recommend: agents never see the hub secret and reach hubs only through engine MCP tools. First contact
    with each recipient needs the owner's approval.
* **D33. Voice input (21.2).**
  * Recommend: OS speech recognition on Windows for the MVP (an OS boundary).
  * Alternatives: local Whisper-class models on a pure-Rust ML runtime (100% Rust, heavy); no voice in the
    MVP.
* **D34. Spend and provider terms (21.4, 21.5; review R13-M1).**
  * **Question for the user:** may a player let other people use AI capacity that is billed to the player?
    Consumer subscriptions (Claude Pro or Max, ChatGPT plans) generally cover personal use. We have not
    verified whether any provider's terms allow sharing them with guests. Through API keys, guests'
    usage is the key owner's responsibility under the API terms.
  * Recommend:
    * guests use only a guest-only API key with a provider-side hard limit (D27d);
    * never the owner's subscription;
    * the owner sees per-guest usage and the remaining budget before unlocking and while unlocked.
* **D35. Volunteer relays (10.7; review R13-L2).**
  * Recommend: relays that anyone can run, forwarding only end-to-end-encrypted traffic.
    * **Discovery:** relays publish signed records in the Mainline DHT. Friends and session members also
      recommend relays through peer exchange.
    * **Choice:** each peer picks relays by its own policy, preferring relays its friends run.
  * **Stated exposure:** a relay sees who talks to whom (keys and IP addresses), message timing and sizes,
    and it can drop traffic selectively. It cannot read or forge traffic.
  * Alternatives: our own relays and DNS domain, like e4mc (simplest, but a central point); iroh with
    self-hosted relays (needs a `ring` exception, D8).
* **D35a. Session codes (10.7, 11; review R13-M3).**
  * Recommend: four words plus a check word. The public two-word nameplate is a DHT rendezvous; the secret
    two-word password is used only inside a PAKE with the host, and nothing derived from it is ever
    published. With a friend in the session, it shrinks to three words and nothing is published.
  * Alternative: three words plus a check word, hashed into a DHT key. Shorter, but anyone watching the
    DHT can break the code offline in minutes.
* **D35b. What a code's host announces (10.7).**
  * Recommend: a relay address, so DHT observers do not learn the host's IP address.
  * Alternative: the host's own address. It works without any relay, and exposes the host's IP while the
    code is live.

* **D36. Finality and membership (6.4, 10.8; reviews C1, C2, H3).**
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
  * Recommend: a per-frame random permutation, `tie_rank(epoch seed, frame, member id)`. Member ids are
    assigned in admission order by the membership log; the seed comes from the session or epoch genesis.
    Every member wins about 1/n of ties [V ties: 41/50/59 of 150].
  * Alternatives:
    * a deterministic rotation of join order (fully predictable a frame ahead; equally fair over time);
    * the hash of (frame, public key) with no session seed (a key can be ground for chosen frames);
    * the lowest id wins (a permanent winner; rejected).
* **D38. Claim rules (8.4; review M3).**
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
  * Recommend: R3's "only when a player interacts" applies to **sim state**. Acks, check hashes, clock
    pings, presentation streams (avatar poses, panel views) and panel or hub messages also flow; none of
    them carries sim state.
  * Alternative: a stricter reading with no continuous presentation streams. Remote avatars would then
    freeze between interactions.
* **D40. Scope of the carried state (7.3; ruling 09:30Z).**
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
  * Settled by the user: nothing is re-stamped, and an author never sees its own interaction corrected.
    Lateness is paid by the late receiver.
  * Recommend for the parameters:
    * a horizon slack of 2 frames, which bounds backdating to about 4 frames;
    * demotion to spectator (10.9) for a member that stalls confirmation past W/2 three times within 10 s.
  * Alternatives:
    * a larger slack: more tolerant of bursty acks, but a wider backdating window;
    * no demotion: a persistently late member stalls everyone at the speculation limit;
    * an arrival-median bound computed from chain references: deterministic, but it couples effective
      frames to membership and again rewrites authors' history at high latency (viz-2).

* **D43. What others see of a player who attends their world from elsewhere (22.1).**
  * Recommend: an "elsewhere" avatar: dimmed, with a badge naming the world where the player is embodied,
    with mirrored head and hand motion at a low rate, and with voice only when the player routes it there.
    By default voice goes to the embodied world only.
  * Alternatives:
    * an away marker, as in Resonite (quieter, less presence);
    * the avatar driven by the player's own AI agent (an AI-forward option, but people may mistake the
      agent for the person);
    * hidden.
* **D44. Portal rendering technique, and what non-members see (22.4).**
  * Recommend: stencil portals with per-eye cameras in VR, at most two live portals per frame and recursion
    depth 1, with render-to-texture for distant portals. Members of A who are not in B see a portal effect
    only.
  * Alternatives:
    * render-to-texture everywhere (simpler; costs memory and sharpness);
    * a low-rate preview image of B streamed by members of both worlds (presentation only; it shows B to
      people B never admitted, so B's owners would have to allow it).
* **D45. Cross-world transfer mode (22.6).**
  * Recommend: **settled handoff.** Relay only facts the other world has confirmed. The object appears
    for B's other members about one confirmation round after it crossed (about 0.15-0.2 s on 50 ms links
    [V r14]), and the carrier hides the gap. It never needs a correction.
  * Alternative: **optimistic arrival.** B shows the object at once, marked provisional and held
    exclusively by the carrier, and rescinds it if A's departure is undone. About 0.15 s sooner for
    observers, at the cost of rare visible rescinds and transient effects in B.
* **D46. How a world trusts a relayed fact (22.6, 22.7; reviews NEW-3, NEW-4).**
  * Recommend for every session: a certificate of signatures by a strict majority of the source (or
    destination) world's members over exactly that fact, published as attestations in that world's log. A
    relaying member's signature alone let any member mint, lose or duplicate an object.
  * Cost: one attestation entry per member per transfer step, and each world must know the other's member
    keys.
  * Alternatives:
    * k-of-n with k below a majority: faster, but a colluding minority can mint;
    * a trusted carrier: MVP-only, and conservation then holds only if the carrier is honest.

* **D47. Physics at portals (22.4).**
  * Recommend: no cross-world physics. A clearance zone around each aperture keeps colliders away, and
    straddling objects are drawn twice. Each world stays independent.
  * Alternative: couple the two sims near the aperture (cloned colliders, as in the game Portal). Worlds
    would stop being independent, a peer in only one world could not simulate it, and the R14 isolation
    rule breaks.
* **D48. Logic components on carried objects (22.8).**
  * Recommend: they travel as inert data and regain behaviour in any world that runs their module. Shared
    schemas allow interoperation. Running foreign code needs the destination's ACL opt-in.
  * Alternative: carry and run the object's module wherever it goes. Richer, but every world would then run
    code its own builders never committed.
* **D49. Background worlds (22.3).**
  * Recommend:
    * at most 4 attending worlds and 8 loaded worlds per client, with a configurable memory budget;
    * background worlds advance in bursts of at most 250 ms;
    * a world is parked after 2 minutes unseen and unused.
    * Throttling never affects other members [V r14 throttle].
  * Alternatives: no limits (memory and CPU grow with every world opened); a hard limit of one attending
    world (Resonite-like, which falls short of R14's "actively joined to several").
* **D50. Snapshot memory knob (6.7, 6.8; user question 13:33Z and the slider requirement).**
  * Recommend: one local setting per peer, changeable at run time, with four positions:
    * Speed: Log(4);
    * **Balanced (the default): Window(4)** with an adaptive window depth;
    * Lean: Window(8);
    * Minimum: base only, with a stall guard.
  * At the default, CPU equals the best measured policy and memory stays at about the base plus 4 images,
    however far confirmation lags: 8.3 MB at 500 balls and 18.6 MB at 1,024 under a 116-frame stall,
    against 47.5 and 106.6 MB for 3d0f98a's Every(4) [V snap].
  * The knob never changes a result [V snap knob-moving].
  * Alternatives:
    * keep Every(4): the same CPU, up to 5.7x the memory under a stall;
    * expose k and the window depth as two expert settings;
    * a memory budget in MB from which the engine derives the policy.
* **D51. Partial (per-island) snapshots (6.9).**
  * Recommend: not now. The public API cannot restore an island bit-exactly [V]. Take the benefit from
    region-scoped worlds first (6.6, R16), and decide on a box3d-rust fork with per-solver-set,
    copy-on-write images together with R15's fork question.
  * Alternative: fork now and snapshot per solver set. Image memory would then grow with the awake bodies,
    at the cost of maintaining the fork through every Box3D update.
* **D52. How the sim predicts a remote hand (6.10).**
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
  * Recommend:
    * per-tick batching, on by default;
    * the four-level controller, with hand strides 1/2/3/4 and acks every 2/3/4/6 frames;
    * demotion to spectator beyond W/2 (D42);
    * a background catch-up budget of 8 ms per tick.
  * Alternatives:
    * fixed rates: collapse below the stream's need [V r18 capped];
    * receiver-side frame dropping: no effect on a sender's queue.
* **D54. Confirmation from stable data (6.4, 10.8; reviews NEW-1, NEW-2).**
  * Recommend: an entry counts once every member's chain references it. This is provably convergent, and
    honest outages and two-faced members cannot fork anyone. It costs one more ack round of finality
    (8.5 frames instead of 4.2) [V].
  * Alternatives:
    * BFT quorum certificates (2f+1 of 3f+1): no wait for a silent member, at the price of a supermajority
      model and larger messages;
    * gossiped acks with conflict proofs, deciding on first receipt: faster, but a decision made before a
      conflict surfaces would have to be undone.
* **D55. Eviction cut (9.3, 10.8; review NEW-2).**
  * Recommend: an equivocator is evicted once every remaining member has voted. Its cut is the longest
    common prefix of the voters' frozen heads, so no confirmed frame ever changes [V].
  * With 5 or more members, a silent voter blocks the cut, so the general view change takes over
    (PBFT-style; [I]).
  * Alternative: evict on a bare strict majority, with the cut chosen by a leader: faster, but it needs
    the general view-change protocol now.
* **D56. R18 defaults (10.9).** The controller thresholds:
  * up above 40 ms of uplink queue, at most every 100 ms;
  * down after 300 ms below 10 ms.

  Production measures the queue by delay gradient, as GCC does [I]. These defaults kept a 32-64 kbit/s cap
  real-time with queues under 100 ms [V].
  * Alternative: loss-based control (as in TCP Reno). It reacts only after the router queue is already
    full, the 500 ms bufferbloat this replaces.
* **D57. Logic-state snapshots (7.4; review NEW-17).**
  * Recommend: snapshot all of guest linear memory, with dirty-page diffs, and hash all of it at check
    frames. State hidden outside the declared arena then fails fast and identically on every peer.
  * Alternative: the declared arena only, plus zeroing all other memory after every tick. This is cheaper,
    but static and thread-local state silently breaks rollback if zeroing is ever skipped.
