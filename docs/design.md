# Primordial Engine: Phase-1 Architecture Design

Status: phase-1 draft for agent review, then user review. Nothing here is built beyond the spikes.
Date: 2026-10-30. Author: engine-architect. Companion: [spike-report.md](spike-report.md).

## How to read this document

* **R1..R13** refer to the user's requirements (listed in section 1). Rulings are cited by date.
* Every factual claim carries a tag:
  * **[V]** verified by a spike run on this machine; the spike-report test ID follows (e.g. [V a1]).
  * **[S]** verified by reading the source code or official docs of the dependency, not by running it.
  * **[I]** inferred or proposed design. Not yet tested.
* Choices that go beyond what the user specified are collected in **section 23, "Decisions needing
  the user"**, each with a recommendation and alternatives. Sections refer to them as **D1..D35**.

## Contents

1. Requirements and rulings
2. Architecture at a glance
3. Resonite to Primordial mapping
4. Determinism strategy (R3, R4)
5. Fixed-timestep loop and render decoupling (R2, R10)
6. Rollback (R3)
7. "Only network physics interactions" (R3)
8. "Soonest wins" without a central authority (R5)
9. Sync checks and divergence repair (R6)
10. P2P topology and transport (R1)
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
22. Crate layout, phased build plan, risks
23. Decisions needing the user

---

## 1. Requirements and rulings

| # | Requirement (user, 2026-10-30) | Where |
|---|---|---|
| R1 | Fully peer-to-peer: no host PC, no central authority | 10, 11, 12 |
| R2 | Fixed-timestep engine clock, independent of framerate | 5 |
| R3 | Deterministic local sim with rollback; network carries only player-physics interactions | 4, 6, 7 |
| R4 | Box3D physics; Rust + wgpu engine and graphics | 4, 18, 20 |
| R5 | Simultaneous interactions: soonest wins | 8 |
| R6 | Periodic and interaction-triggered sync checks; on mismatch find earliest interfering frame, propagate by rollback | 9 |
| R7 | Players with authority live-edit logic and shaders; git-style SCM; earlier commit wins; later rejected with reason and asked to rebase; anti-starvation spot | 13, 14, 15 |
| R8 | AI working with players through MCP, designed in from the start | 17 (and every section's MCP notes) |
| R9 | Decentralized peer discovery: compare options, recommend one | 11 |
| R10 | Full VR support | 18 |
| R11 | Object hierarchy + ECS; assets; their interaction with determinism, rollback, P2P, SCM, MCP | 16 |
| R12 | Resonite avatar import; v1 = mesh + skeleton/skinning + textures only | 19 |
| R13 | Own agent harnesses (Claude Code, Codex, …) in in-game panels; panel-to-panel connections; mail-hub panel; panels unlockable for friends/session | 21 |

Rulings that shape the design:

* **Phase 1** = design doc + feasibility spike (08:22Z).
* **Mobile/standalone (incl. Quest-native) excluded from MVP**; desktop PC VR via OpenXR. Note where a choice
  would preclude it later (08:26Z).
* **box3d-rust preferred** over C bindings (08:28Z). **"100% Rust"**: no C/C++ compiled or vendored, no C
  fallback (08:28Z). It **covers live game logic**, which is Rust, not a scripting language (08:28Z).
  **Shaders are exempt** (08:29Z).
* **AI-forward**: AI agents via MCP are the main authors of logic and shader edits (08:29Z).
* **Resonite** is the product model in broad strokes; the spec wins where they differ (08:32Z).
* **Merge seat** lasts 10 s so a waiting merger can never starve anyone else (08:51Z).
* **R13 additions** (09:12-09:15Z): panels talk peer to peer within a session and through a mail hub
  (hubchat-style panel); owners can unlock panels (locked / friends / anyone in session) and revoke;
  panel-to-panel needs an explicit connection under a per-panel policy (allow all / ask agent / ask user
  / deny all).

---

## 2. Architecture at a glance

```
                one peer process (identical on every machine)
 +-----------------------------------------------------------------------------+
 |  MCP server (rmcp) <--- local AI agents (stdio / localhost HTTP + token)    |
 |        |  tools: read state, propose interactions, edit objects,            |
 |        |         compile/test/commit logic, validate/commit shaders, assets  |
 |        v                                                                     |
 |  Action intake: signs as user/agent key, checks local ACL view               |
 |        |                                                                     |
 |        v                                                                     |
 |  +-------------------- SIM THREAD (fixed 60 Hz, deterministic) ----------+  |
 |  | input log (frame-stamped, signed) -> order: (frame, seeded tie, peer) |  |
 |  | claims ("soonest wins")  -> logic modules (WASM, wasmi, fuel)         |  |
 |  | sim ECS store + object hierarchy -> Box3D (box3d-rust) step           |  |
 |  | snapshot ring (world image + ECS + guest memory) -> rollback/resim    |  |
 |  | per-frame hashes (world, objects, inputs) -> sync checks              |  |
 |  +-------------------------------+---------------------------------------+  |
 |        | publishes interpolable state       ^ inputs, commits, hashes        |
 |        v                                    |                                |
 |  RENDER/XR THREAD (display rate)       NET THREAD                            |
 |  wgpu + WGSL (naga), interpolation,    UDP + Noise (snow), reliability,      |
 |  OpenXR swapchains, local poses        clock sync, NAT traversal, discovery, |
 |                                        P2P asset/commit transfer             |
 |  Content-addressed store (BLAKE3): source, wasm, WGSL, assets, commits       |
 +-----------------------------------------------------------------------------+
```

Core idea: every peer runs the same deterministic simulation from the same initial state, code and
assets. Only frame-stamped, signed **interactions** (and code/asset commits) cross the network. Late
arrivals are absorbed by rollback. Hash exchanges prove that everyone stayed on the same world line,
and repair it when they did not. There is no host: ordering comes from the sim-frame stamp plus a
deterministic tie-break that every peer computes identically. [V c1-c3]

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
| Contacts, session browser (cloud) | Invites, mDNS, Mainline DHT, peer exchange | 11 |

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
* **Not verified:** ARM (aarch64), macOS, other x86 microarchitectures, and Box3D multithreading (the port
  is serial). Upstream claims bit-identical results across Linux/Windows/macOS on x64 and arm64, enforced
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
7. Every peer in a session runs a bit-identical engine build. Admission checks the engine build ID, and
   Box3D snapshot images carry a layout hash [S port `snapshot/mod.rs`]. The toolchain is pinned in
   `rust-toolchain.toml`.
8. A CI determinism matrix (Windows x64, Linux x64, Linux arm64, macOS arm64; debug and release; native and
   wasm32) compares golden traces on every change. It also catches regressions in box3d-rust updates.

### 4.4 Gaps of box3d-rust compared with the C reference (tech ruling: "note any gaps")

| Gap | Impact | Mitigation |
|---|---|---|
| Serial only (no task system or worker threads) [S port README] | Step cost grows with bodies: 0.36 ms @256, 1.5-1.9 ms @1024, 4.8-5.9 ms @2048 [V b6]; rollback re-sim multiplies this | Partition large worlds into independent Box3D worlds stepped in parallel (6.6); cap active bodies per session for MVP (D2) |
| Scalar only (no SSE/AVX/Neon) [S] | Slower solver than the C build | Accept for MVP; SIMD in the port would have to stay bit-exact with scalar |
| Pinned to upstream `3fc20f5` [S]; upstream HEAD is `33626332` (2026-10-28) | Missing upstream fixes and features; port lag | Pin the exact version (`=0.5.0`); track upstream; re-run goldens on every bump |
| Third-party port of alpha software (Box3D v0.1.0, "alpha") [S] | API churn, latent bugs | Wrap behind `pe-physics` so the engine never calls box3d-rust directly; golden tests |
| C-mirror API (not idiomatic Rust) [S] | Ergonomics only | Thin wrapper in `pe-physics` |

No gap blocks the requirements. The serial-only port is the main performance risk (22.3).

### 4.5 Large worlds

The port has a `double-precision` feature, which maps to `BOX3D_DOUBLE_PRECISION`, with its own goldens
[S]. It gives far-from-origin precision at a cost. Not enabled in the spikes. Decision: **D3**.

---

## 5. Fixed-timestep loop and render decoupling (R2, R10)

* **Sim clock:** `frame = floor((session_time_us - epoch_us) / 16_667)`, at 60 Hz with 4 Box3D sub-steps.
  `session_time` is local time plus the clock-offset estimate (10.4). The sim thread steps until it reaches
  the frame the clock says, applying pending rollbacks first. It never waits for rendering. [V c1 loop]
* **Drift control:** if a peer runs ahead of the session median (its inputs arrive "early" everywhere), it
  slows its clock by up to 1% until aligned, like GGPO's frame-advantage throttling. [I]
* **Render thread** runs at display rate: monitor vsync, or `xrWaitFrame` in VR at 90-120 Hz. It
  interpolates between the two newest published sim states with `alpha = (t - t_prev)/dt`. The sim
  publishes a compact, double-buffered "presentation snapshot" of transforms after each step. Rollback
  rewrites recent states. The renderer applies **error smoothing**: it blends from the old displayed pose
  to the corrected pose over about 100 ms, so corrections do not pop. [I]
* **Presentation-only systems** (animation, particles, audio, UI, local VR head and hands) run at render
  rate. They read sim state but never write it (16.3).
* Tick rate (60 Hz) and rollback window: **D4**.

---

## 6. Rollback (R3)

### 6.1 Rollback state

A snapshot at frame *f* (the state before frame *f*'s inputs) contains:

| Part | How | Evidence |
|---|---|---|
| Box3D world | `recording::serialize_world` image (POD + geometry registry); restored in place with `deserialize_into_shell`; engine-held `BodyId`s stay valid | [V b1, b4] |
| Engine claim table ("soonest wins") | `BTreeMap` copy | [V b1-b3] |
| Sim ECS store (16) | column copies (memcpy) | [I] |
| Logic module state | guest linear memory + active module ID; restore = fresh instance of that module + memory copy | [V d1] |
| Input log, commit log | append-only; not rolled back (re-applied on re-sim) | [V b2, b3, d1] |

### 6.2 Measured costs

Release build on a Ryzen 7 9700X, two bench runs (ranges reflect run-to-run timing noise) [V b6]:

| Bodies | Step (avg) | Save | Restore | Image |
|---|---|---|---|---|
| 256 | 0.36 ms | 0.25-0.26 ms | 0.25 ms | 0.59 MB |
| 1024 | 1.5-1.9 ms | 0.95-1.2 ms | 0.74-0.91 ms | 2.4 MB |
| 2048 | 4.8-5.9 ms | 2.2-2.7 ms | 1.7-2.1 ms | 5.4 MB |

The image is about 2.3-2.6 KB per body, because it contains broadphase trees, contacts and solver sets
along with body state. Guest memory adds about 1.1 MB per logic module at the default 1 MiB WASM stack
[V d1].

### 6.3 Snapshot ring policy [I]

* A snapshot every frame for a 2 s window costs 120 x 2.4 MB, about 290 MB at 1024 bodies. That is too much.
* **Proposal:** keyframe every K = 4 frames (about 72 MB at 1024 bodies), restore the nearest keyframe at or
  before the rollback frame, and re-sim the gap. Reduce further with per-page delta compression of world
  images and guest memory (most bytes do not change frame to frame), and shrink the guest stack
  (`-z stack-size=65536`).

### 6.4 Rollback depth and window

* Rollback depth = how late an interaction arrives = one-way latency + jitter + loss recovery. At 60-80 ms
  one-way delay the measured depth was 5 frames, and 11 frames with 10% loss [V c1, c2].
* **Window = 120 frames (2 s)** [I, D4]. Inputs older than the window are refused (`TooLate`) [V b5] and
  handled by state transfer (9.5).
* Sync-check repair needs a deeper rollback, about 65 frames in the spike [V c3], so the window must cover
  the check delay too.

### 6.5 Re-simulation budget [I]

* A 60-frame rollback at 256 bodies re-simulated in 33 ms [V b2]. That is two 60 Hz frames, or three 90 Hz
  VR frames.
* The sim thread therefore caps re-sim work per tick (e.g. 8 ms) and catches up over several ticks while
  the renderer shows the best-known state with smoothing.
* Most rollbacks are shallow (5-11 frames measured), so this matters mainly for repairs and loss bursts.

### 6.6 Scaling past the serial port [I]

* Box3D worlds are independent, so the engine can split a session into **regions**, each its own Box3D
  world, stepped in parallel on worker threads.
* Objects crossing regions migrate at deterministic frames. Snapshots and rollbacks can then be
  region-scoped: only regions touched by a late interaction re-simulate. That fits R3's "interactions are
  local" model well.
* Not needed for the MVP target (D2).

---

## 7. "Only network physics interactions" (R3)

* **An interaction** is any player- or agent-originated change to sim state:
  * impulses or forces on dynamic bodies;
  * grab, hold and release (a kinematic target joint for the duration of the hold);
  * spawning or destroying objects;
  * setting sim component values (in-world editing);
  * per-frame hand targets while a VR hand is physically engaged (18.4).
* Code, shader, asset and ACL changes are **commits**: interactions with a longer ordering and finality
  path (15).
* **Everything else is local:** gravity, collisions, sleeping, joints, logic-module behaviour, and
  everything that follows from interactions. It is computed independently on every peer from the same
  state, code and inputs. Nothing about it is sent. [V c1: only `Input` and control messages cross the
  wire; 98-111 datagrams per peer for a 10 s session]
* **Wire format** [V c1, I for production]: `{frame, peer, seq, object_id, kind, payload}`. Signed (spike:
  Ed25519 per datagram; production: Noise session AEAD plus signed log batches, 10.3). Sent redundantly
  until acknowledged [V c2].
* **Consistency** follows from determinism plus the same input set plus the same initial state and code. It
  is checked, not assumed (9).
* **Bandwidth:** two peers' interactions total a few kB per minute in the spike. VR hands streaming while
  engaged cost about 60 Hz x 32 bytes = 2 kB/s per engaged hand per receiver [I]. Remote-avatar poses are
  presentation-only and unreliable (18.4).

---

## 8. "Soonest wins" without a central authority (R5)

1. **Logical time = sim frame.** All peers share the session epoch (agreed at session creation) and keep a
   clock offset estimate (10.4). A peer stamps an interaction with the frame at which it applied it
   locally, which is its present. [V c1]
2. **Total order:** `(frame, tie = splitmix64(session_seed ^ frame*K ^ peer<<40), peer, seq)`. Earlier frame
   wins outright. Within a frame, the seeded hash rotates priority between peers frame by frame, so no peer
   is permanently favoured. `(peer, seq)` makes the order total. Every peer computes the same order. [V b3,
   c1]
3. **Claims make "wins" meaningful.** An accepted interaction claims its object for `CLAIM_FRAMES` (30
   frames, 0.5 s, in the spike). While a claim is active, interactions from other peers on that object are
   rejected. The rejection is deterministic and machine-readable: `RejectedClaimed{by_peer, until_frame}`.
   The claimant may keep interacting; a held grab renews its claim. [V b3, c1: same verdicts on both peers:
   at frame 120 peer 1 accepted and peer 2 rejected; peer 2's frame-125 attempt rejected; peer 1's frame-126
   follow-up accepted]
4. **"Arrive at once"** is read as "stamped for the same frame or overlapping claim windows". Arrival order
   on the wire never matters; late arrivals roll back. Alternatives (sub-frame timestamps, additive
   physics with no exclusivity): **D5**.
5. **Clock skew and fairness.** Offset estimation error was 0.02-5.9 ms on loopback with injected jitter [V c1-c3],
   well under one 16.7 ms frame. Over the internet, asymmetric routes bias it by up to half the RTT
   asymmetry [I]. So "soonest" is fair to about one frame, with equal-frame ties shared fairly.
6. **Backdating (cheating)** [I]. A peer can lie about its frame stamps to win. Partial defences:
   * signed, sequenced input logs: each entry carries the sender's own "sent at frame", and stamps may not
     precede it by more than 2 frames;
   * statistical detection: a peer whose inputs consistently arrive "older" than its RTT explains gets
     flagged, then removed by an owner or a majority (12).
   Without a central authority this is detection, not prevention. Trust model: **D21**.

---

## 9. Sync checks and divergence repair (R6)

### 9.1 What is hashed [V state hash and input digest; I per-object hashes]

* **State hash per frame:** Box3D body transforms and velocities (`hash_world_state`), plus claims and the
  frame number [V a2]. Logic-module state and module ID are added when logic runs [V d1].
* **Input digest per frame:** an order-independent hash of the interactions known for that frame [V c3].
* **Per-object hashes [I]** ("snapshots of certain objects' state", R6): transform, velocities and sleep
  state of tracked objects. Tracked = recently interacted, explicitly important, or sampled round-robin.
  This localizes divergence to objects and islands, not just "the world".

### 9.2 Cadence

* **Periodic:** every 30 frames (0.5 s). The hash for check frame *c* is sent once the peer reaches
  *c + 45*, so the hash is final unless something arrives beyond the expected latency [V c1-c3].
* **Interaction-triggered:** when an interaction on object O at frame *f* is applied, a check for *f + 15*
  is scheduled, scoped to O and its island [V c3 triggered at whole-world scope; I for object scope].

### 9.3 Repair: earliest interfering frame, then rollback [V c3]

1. Check mismatch at frame *c*: the peers exchange per-frame `(state hash, input digest)` for
   `[c - 60, c]` (bounded by the window).
2. Scan forward to the **earliest frame where state hashes differ**. That is where some player's
   interaction changed the object's world line on one peer but not the other.
3. If the **input digests also differ** at that frame, the peers swap the interactions they each lack.
   Each receiver rolls back to that frame and re-simulates. Both converge.
   * In the spike, peer 2's frame-200 interaction was never transmitted. The check at frame 210 detected
     the mismatch at frame 257. Frame 200 was identified as earliest. The interaction was exchanged, and
     peer 1 rolled back 65 frames. Both peers' 600-frame histories then matched the offline reference.
4. Stale mismatches (a hash compared before an in-flight interaction landed) re-check as "histories agree
   now" and are dropped [V c3].

### 9.4 Same inputs, different state

This means true nondeterminism, memory corruption or tampering. The spike detects and labels it but does
not repair it [V c3 code path, not triggered].

**Proposed repair [I]:**
* With 3 or more peers, the majority state hash at the earliest divergent frame wins. Minority peers
  receive a state transfer (9.5) and are flagged.
* With 2 peers there is no majority (**D6**).
* Every such event is logged with both states for debugging. It should never happen if 4.3 holds.

### 9.5 State transfer (beyond the window, late join, minority repair) [I]

* The donor sends a snapshot at a confirmed frame *F*: world image, ECS columns, guest memories, module
  set, claims and asset manifest. It follows with the input and commit logs from *F*.
* The receiver restores and fast-forwards. Restoring a world image into a fresh `World` works
  [V b4].
* Images are POD memory layouts tied to the engine build (layout hash). Admission already requires an
  identical build (4.3 rule 7).
* Region- or island-scoped transfer is a later optimization.

---

## 10. P2P topology and transport (R1)

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

### 10.3 Identity and security [V c1-c3, I production]

* Each user and device has an Ed25519 keypair. Agents get their own keys with delegation (12.3).
* **Spike:** every datagram is Ed25519-signed, and the peer key is pinned from its self-signed Hello.
* **Production:** Noise XX/IK handshake bound to the Ed25519 identity. AEAD (ChaCha20-Poly1305) per
  packet. Input and commit log entries are individually signed, so any peer can forward them and third
  parties can verify authorship. That is required for repair (9.3) and relaying.

### 10.4 Clock sync [V c1-c3]

* NTP-style ping/pong. The minimum-RTT sample's offset becomes the estimate. Measured error was 0.02-5.9 ms
  on loopback with 10-60 ms of injected one-way jitter.
* **Production [I]:** continuous re-estimation, a median across peers, and slew instead of jumps.

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

### 10.6 Join, leave, partitions [I; join building blocks V b4, c1]

* **Join:** the invite gives addresses plus the inviter's key (11). The joiner connects to any member and
  gets a state transfer (9.5) plus the logs. It fetches missing assets and wasm by hash from all members
  (16.7), fast-forwards, and starts sending inputs.
* **Leave:** a graceful "bye", or timeout after 3 s of silence. Membership needs no consensus for
  simulation correctness. An input either reached someone, who will spread it in repair (9.3), or nobody,
  in which case it never happened. Membership matters for ACL votes and seats (12, 15). There, a member is
  "present" at frame *f* if it acknowledged frame *f* in its signed log.
* **Partitions:** both halves keep simulating. On heal, sync checks diverge and input exchange repairs
  anything within the window. Beyond the window the halves have irreconcilable histories. Policy (larger
  partition wins vs replaying the minority's inputs at heal time): **D7**.
* **Always-on peers** (like Resonite's headless hosts) are ordinary peers that keep a world running. They
  have no authority.

---

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
1. **Memorable addresses.** Two random words are easy to say and share. We adopt human-readable **session
   codes** (e.g. `crimson-lantern-orbit` plus a check word) without DNS:
   * the code hashes to a Mainline DHT key (BEP 44 record) that holds the session's member addresses,
     signed by the session key;
   * the code also seeds a PAKE (SPAKE2; the pure-Rust `spake2` crate, not yet audited), so a guessed or
     overheard code alone cannot impersonate the session.
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
6. Human-readable session codes (e4mc-style words, 10.7) resolve through the same DHT records, with a
   PAKE so the code alone cannot impersonate the session.

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
  15.2). Each takes effect at its ordered frame.
* **Every peer validates** every interaction and commit against the ACL state *as of that frame*. A
  non-authorized action is rejected identically everywhere, with a machine-readable reason.
* **Optional threshold ownership:** k-of-n owners must sign ACL changes. This removes the single-root-key
  concern (D10).

### 12.3 AI agents [I]

* Each agent has its own Ed25519 key. A user issues a **delegation certificate**: "user U authorizes agent
  A with scopes {interact, edit-objects, commit-logic, commit-shaders, commit-assets}, expiring at T".
* An agent's effective rights are the intersection of the certificate scopes and the user's roles.
  Certificates travel in the session log, so every peer can verify agent-signed actions and attribute
  them to user U.
* Revoking the certificate stops the agent immediately at a deterministic frame.

---

## 13. Live game logic in Rust (R7, 100%-Rust clarification)

### 13.1 Pipeline

```
AI or human edits Rust source (MCP: logic.write / logic.check / logic.test_in_fork)
   -> pinned toolchain compiles crate to wasm32-unknown-unknown (cdylib, no_std + alloc SDK)
   -> commit {source tree hash, wasm hash, schema manifest} signed by author
   -> ordered and finalized by the SCM (15) -> activation frame A
   -> every peer validates the wasm (wasmi) and swaps it in at frame A, with state migration
```

### 13.2 Who compiles: trust model (D11)

* **Option 1 (recommended): the committer compiles; peers run the shared wasm.**
  * WASM is sandboxed: memory-safe and limited to the capabilities the host API grants. A malicious module
    can do no more than malicious *source* could.
  * The residual risk is a binary that doesn't match its source, which defeats review and rebase. Peers
    that have the toolchain run **background reproducibility checks**: rebuild from source, compare
    hashes, flag mismatches.
  * Rebuilding identical source on this machine reproduced the identical wasm (sha256 `f9c5e895…`) [V d5].
    Cross-machine reproducibility needs a pinned toolchain and `--remap-path-prefix` [I].
  * Peers without a toolchain can still play and run logic. That matches "end users mostly won't touch
    code".
* **Option 2: every peer compiles from source.** Maximum trust, but every machine needs about 1 GB of
  toolchain plus seconds of compile per commit.

### 13.3 Runtime: wasmi (D12)

* **wasmi 2.0**, pure Rust [V audit `logic-wasmi`], configured as follows:
  * `deterministic` feature: the Wasm 3.0 deterministic profile, NaN canonicalization [S];
  * fuel metering [V d1b, d3];
  * **eager compilation**. Verified finding: wasmi's default `LazyTranslation` charges fuel for translating
    each function on first use, so fuel consumption depended on which instance in the process called first.
    Measured 49,831 vs 46,800 fuel on two peers, and the runaway module halted at different points
    [V d-lazy]. Eager compilation fixes both [V d1b, d3].
* **Speed:** the Box3D scene ran about 22x slower interpreted than native (7.0 ms vs 0.31 ms per frame)
  [V d4]. Game logic should be orchestration, with heavy lifting done by host APIs (physics, queries, math
  helpers), so interpretation cost stays small. Logic ticks in the spike used ≤83 fuel units [V d1b].
* **Alternative: wasmtime (Cranelift JIT).** Much faster, but its default build compiles C (fiber, jit-debug,
  ittapi, zstd-sys) [V audit `logic-wasmtime`]. Whether a reduced feature set is pure Rust is unverified.

### 13.4 Guest ABI (phase-1 sketch) [V d1]

* **Imports `pe.*`:** read-only world view (`body_count`, `body_pos`, …) and command buffer
  (`apply_impulse`). Production adds component queries and an object API generated from the component IDL
  (16.5).
* **Exports:** `pe_tick(frame)`, `pe_state_ptr/len/version`, `pe_scratch_ptr`, `pe_migrate(from_version,
  len)`.
* Logic-issued actions are applied as peer-0 ("world logic") interactions on every peer identically. They
  are never networked, because every peer computes them. [V d1]

### 13.5 Hot reload and rollback consistency [V d1, d2]

* A code change lands at an **exact activation frame A**, so every peer runs old code up to A-1 and new code
  from A.
* **State migration:** the host copies the old state bytes into the new module's scratch buffer and calls
  `pe_migrate`.
* A peer that learns of the commit late rolls back to A: it restores physics, guest memory and *module
  version*, swaps, and re-simulates. In the spike, peer B learned 45 frames late and matched peer A on all
  600 frames. A control peer without the commit diverged exactly at A, which shows the swap took effect.
* Snapshots record the active module ID, so rolling back across a swap restores the old module [V d1].

### 13.6 Sandboxing [V d3, I]

* No WASI and no host I/O: only the `pe.*` capability API.
* Per-tick fuel budget. A runaway module stops at the identical instruction on every peer [V d3]. The
  policy is to disable that module at that frame everywhere and auto-revert to the last good commit (D14).
* Memory limits through wasmi's resource limiter. Host functions validate every argument (IDs, finite
  floats) [V d1].

### 13.7 Compile latency: what "real time" means [V d5, I]

* A tiny no_std guest rebuilt in 0.21-0.29 s after a one-line edit. The first build took 1.24 s
  [V d5].
* A realistic logic crate on an engine SDK is estimated at 1-5 s [I].
* End to end: edit, then compile (about 1-5 s), then SCM finalization (one ordering window, about 2 s),
  then activation lead (about 0.5 s). Changes go live **within seconds**, without restarting anything.
* AI agents iterate faster with `logic.check` (compile and diagnostics only) and `logic.test_in_fork`
  (17.3) before committing.

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
  * `logic/` (Rust crates), `shaders/` (WGSL), `assets/` (large blobs referenced by hash), `scene/` (prefabs
    and initial object tree), `acl/`, and `world.toml`.
* **Commits** = `{parents, tree, author_key, delegation_cert?, proposal_frame, message, signature}`.
* Live sim state (where things are now) is not in the repo. "Save world" writes a snapshot commit.

### 15.2 Ordering under "soonest wins" (no central authority) [I]

* Commits are proposals stamped with the sim frame of submission. They are ordered by the same key as
  interactions (8.2).
* A branch head (`main` of the live world) advances by the **first-ordered valid commit whose parent is the
  current head**.
* A commit becomes **final** once it is older than the ordering window (2 s), because no earlier-stamped
  commit can still arrive. Every peer then computes the same head.
* Activation frame = finalization frame + lead (≥30 frames, so peers can fetch artifacts).

### 15.3 Conflicts and rejection (R7)

* A later commit with the same parent as a winning commit **conflicts**. Proposal: it conflicts only if
  the two touch overlapping paths. Non-overlapping commits are auto-rebased by a deterministic tree merge
  and re-validated (compile and naga) (D14).
* The loser is **rejected with a machine-readable reason**:

```json
{"code":"conflict","your_commit":"b3:…","base":"b3:…","winner":"b3:…",
 "winner_frame":1234,"overlapping_paths":["logic/doors/src/lib.rs"],
 "required_action":"rebase","merge_seat":{"position":1,"expires_after_s":10}}
```

* Other rejection codes: `unauthorized`, `compile_failed` (with rustc JSON diagnostics),
  `shader_invalid` (naga spans), `fuel_exhausted_at_frame`, `missing_asset`, `seat_held_by`.

### 15.4 The merge seat (user ruling: 10 s)

* A rejected submitter gets a **merge seat** in a FIFO queue ordered by rejection order.
* The front seat is **active for 10 s = 600 frames**. Only the seat holder's commit may advance the head.
  If the holder lands a valid rebased commit (parent = head) inside those 600 frames, it wins. Otherwise
  the seat **lapses** and the next seat activates.
* Commits from others during an active seat are not lost. They are rejected with `seat_held_by`, which
  also queues a seat for them.
* The worst-case wait is bounded by 10 s x queue position, so nobody is starved, including by a stalled
  seat holder.
* Seat timing is counted in sim frames, so every peer agrees on lapse.
* Open detail: does the 10 s start when the seat is granted or when it reaches the front? The design
  assumes the front (otherwise queued seats could lapse before their turn): **D14**.

### 15.5 Distribution [I]

Commits and objects travel P2P by hash (16.7). Finalization requires only ordering, not that everyone holds
the bytes. Activation waits for the artifact; the policy for a peer missing it is **D17**.

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

### 17.1 Where it lives [I; protocol shape V mcp]

* **An MCP server embedded in every peer.** There is no central MCP server.
  * Local agents connect over stdio (agent launched by the engine) or Streamable HTTP on localhost with a
    user-issued bearer token.
  * The agent drives *its user's* peer. The peer turns agent actions into signed interactions and commits.
* **Library:** `rmcp` 3.8 (official Rust SDK), pure Rust with server, stdio, macros and schemars
  [V audit `mcp-rmcp`].
* The spike's hand-written stub covered the core protocol shapes: initialize, tools/list, tools/call with
  `structuredContent` and `isError`, resources/read, and JSON-RPC errors. It has not been tested against a
  third-party MCP client [V mcp].

### 17.2 Tool and resource catalog (proposed) [I]

| Area | Tools / resources |
|---|---|
| Session and world | `world.get_state`, `world.query_objects`, `world.get_object`, `world.hashes`, `session.members`, `schema.components` (JSON Schema per component type), resource `primordial://world/state` with subscriptions for frame/commit/rejection events |
| Interactions | `world.propose_interaction` (same path as a player's: frame-stamped, soonest-wins, rollback [V mcp]), `object.*` edits (16.9) |
| Logic | `logic.read`, `logic.write` (draft workspace), `logic.check` (rustc `--error-format=json` diagnostics), `logic.test_in_fork`, `scm.commit`, `scm.status`, `scm.rebase`, `scm.seat_status` |
| Shaders | `shader.validate` (naga spans), `shader.commit`, `shader.preview_in_fork` (render-only) |
| Assets | `asset.import`, `asset.get`, `asset.list`, `prefab.save` |
| Admin (owner scope) | `acl.grant`, `acl.revoke`, `agent.delegate` |
| Panels and hubs (R13) | `panel.connect`, `panel.send`, `panel.inbox`, `panel.close`, `panel.requests.*` ("ask agent" decisions), `hub.contacts`, `hub.send`, `hub.inbox` (21.6, 21.7) |

### 17.3 Deterministic forks: the AI test bench [I, building blocks V b1/b4/d1]

* An agent can fork a **private, local copy** of the live session at the current frame: restore the latest
  snapshot into a fresh world [V b4].
* In the fork it applies a candidate commit or interactions, runs N frames faster than real time, and reads
  back hashes, metrics and object states. Nothing reaches the network.
* This gives AI authors a safe edit-test loop with exact reproducibility.

### 17.4 Same checks as human edits

* MCP actions are signed with the agent key under its delegation certificate (12.3).
* They pass through the same ACL validation, frame stamping, soonest-wins ordering, rollback, SCM
  ordering, rejection and merge seats. No MCP path bypasses the sim or the SCM.
* Rate limits and fuel apply per agent.

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
* Target runtimes and headsets: **D18**.

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
* Each XR frame interpolates sim state to the predicted display time. Head and hand poses for local
  rendering are late-latched from `xrLocateViews`/`xrLocateSpace` at the predicted display time.
* Local head motion never waits on the sim or the network. The compositor's reprojection covers
  render-time misses.

### 18.4 Tracked poses and "only network interactions" [I]

* **Local head and hands** drive the local avatar at display rate. This is presentation.
* **Remote avatars:** pose streams at 30-60 Hz, unreliable and presentation-only. They are interpolated
  with a jitter buffer of about 100 ms. They are not rollback state. This keeps R3: poses are not physics
  interactions.
* **Hands become sim inputs only when engaged.**
  * When a hand comes within a few cm of a dynamic object (prediction margin) or starts a grab, the peer
    begins streaming frame-stamped **hand-target inputs**: position, rotation and grip, quantized to a
    1 mm / 0.001 rad grid so every peer sees identical bits.
  * In the sim, an engaged hand is a kinematic Box3D body with that target. A grab is a weld or motor joint
    to the held body. A throw follows from the kinematic hand's velocity.
  * The engaged hand claims the touched object (8.3), so the "soonest wins" rules apply to tug-of-war.
* Engagement thresholds and whether non-engaged hands collide at all: **D18**.

### 18.5 Comfort and latency budgets [I]

| Path | Budget | How |
|---|---|---|
| Head motion to photon | under 20 ms | Compositor reprojection plus late-latched local poses; independent of sim and network |
| Own hand on an object | 0 added input delay | Rollback netcode predicts locally; local inputs apply at the present frame |
| Remote player's action, seen locally | One-way latency plus up to one frame | Rollback re-sim plus about 100 ms error smoothing (5) |
| Rollback re-sim at 90 Hz | ≤8 ms per tick, the rest amortized | 6.5; physics budget per VR world (D2) |

### 18.6 Mobile and standalone (excluded from MVP; what would block it later)

* Nothing chosen precludes Android or Quest-native: box3d-rust, wasmi and wgpu-on-Vulkan all run there,
  and the Android OpenXR loader is loaded the same way.
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
| Textures | Texture assets (PNG/JPEG/WebP decoders, pure Rust) |
| Materials (PBS family) | Standard PBR with albedo, normal and base color only in v1; the rest is dropped and reported |
| AvatarDescriptor / VRIK references (head, hands, feet) | `AvatarRig` component mapping humanoid bones; v1 IK = our own 3-point (head + hands) solver driving the skeleton |

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

### 20.2 Gray areas the user should rule on

* **Linux:** winit's Wayland path compiles a small C log shim (`wayland-backend` with system libwayland)
  [V audit; S its build.rs]. Avoidable by a pure-Rust Wayland backend or X11-only.
* **Rust inline assembly** in pure-Rust crypto (`graviola`: 149 `asm!` blocks) [S]. Only relevant if
  transport option B is chosen.
* **Shader compiler:** DXC (C++ DLL) only if HLSL is chosen (14).
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

### 21.1 Hosting a harness: two integration paths

**Structured mode (recommended default).** The engine spawns the harness in its machine-readable
headless or protocol mode. It renders the events as a chat UI: messages, streaming text, tool-call cards,
diffs, approvals. Flags were checked against each installed CLI's `--help` on this machine [S]:

| Harness (installed version) | Structured path | Restriction and spend controls available |
|---|---|---|
| Claude Code 2.1.292 (Node.js app, installed via npm) | `claude -p --input-format stream-json --output-format stream-json --include-partial-messages`, plus `--mcp-config <json>` and `--resume <session-id>` for a persistent conversation | `--tools`, `--allowedTools`, `--disallowedTools`, `--strict-mcp-config`, `--permission-mode`, `--max-budget-usd`, `--add-dir`, `--append-system-prompt` |
| Codex CLI 0.160.0 (native x64 executable) | `codex exec --json` (JSONL events, `--output-schema`); `codex app-server` (experimental JSON-RPC; `generate-json-schema` emits its protocol schema); MCP servers via `-c mcp_servers.<name>…` overrides or `codex mcp add` | `-s/--sandbox`, `-a/--ask-for-approval`, `-C/--cd`, `--ephemeral` |
| Gemini CLI (Node.js app, via npm) | `--acp` (Agent Client Protocol) or `-o stream-json` | `--allowed-mcp-server-names` |
| Any ACP-capable agent | Agent Client Protocol over stdio; the `agent-client-protocol` crate 3.6 is Rust [V audit `agent-panels`]. Protocol semantics not yet exercised [I] | per agent |

* **PTY mode (universal fallback):** run the harness's interactive TUI under a pseudo-terminal and render a
  terminal emulator into the panel.
* **Engine side** (all audited 100% Rust [V audit `agent-panels`]):
  * `portable-pty` (ConPTY on Windows, openpty on Unix);
  * a VT emulator (`alacritty_terminal` or `vt100`);
  * `cosmic-text` for shaping and layout, with glyphs rasterized into a wgpu texture atlas.
* PTY mode cannot be safely shared (21.5), because the TUI exposes the harness's full controls.
* Decision: **D25**.

### 21.2 Rendering and input [I]

* **A panel is a presentation-only object** (16.3): a world-space quad, flat or curved, attached to the
  world, the player's wrist, or the desktop overlay. Its texture is redrawn when content changes. A
  panel never enters the sim, rollback or hashes.
* **Desktop:** keyboard focus with click-to-focus, plus OS IME for text composition. The panel can also be
  shown as a screen-space overlay.
* **VR:** laser-pointer or poke interaction, an in-engine Rust virtual keyboard, and panel grab and resize
  gestures.
* **Voice (D33):**
  * (a) OS speech recognition (Windows `Windows.Media.SpeechRecognition` via the `windows` crate, an OS
    boundary);
  * (b) local Whisper-class models on a pure-Rust ML runtime (heavy; GPU optional);
  * (c) push-to-talk audio handed to a harness that accepts audio, if any.

### 21.3 Wiring the harness to the engine's MCP server (R8)

1. Each panel gets its **own agent key** and a delegation certificate from its owner (12.3), scoped by the
   owner (e.g. interact + edit-objects + commit-logic).
2. The engine launches the harness with an MCP config pointing at the **local peer's** MCP server. That is
   either a stdio command (`pe-app mcp --panel <id>`) or a localhost Streamable-HTTP URL with a per-panel
   bearer token [I].
   * Claude Code: `--mcp-config` plus `--strict-mcp-config`, so only the engine server loads [S flags].
   * Codex: `-c mcp_servers.primordial.command=…` [S flag; exact key path I].
3. Everything the agent does to the game goes through MCP tools (17.2), so it is signed with the panel's
   agent key and passes the same ACL checks, frame stamping, soonest-wins ordering, rollback, SCM
   ordering, rejections and merge seats as any human edit (17.4).
4. The harness's working directory is a panel workspace holding a checkout of the world repository. The
   agent can use its own file tools on game source, then `logic.check`, `logic.test_in_fork` and
   `scm.commit` through MCP.

### 21.4 Locality, credentials and visibility

* **Harnesses run only on their owner's machine,** under the owner's OS account and the owner's own
  harness login or API key. The engine never reads, stores or transmits harness credentials. Nothing ever
  runs a harness on behalf of another peer, and nothing ships a harness across peers. [I, by design]
* **Panel contents are private by default.** Other peers see the panel object, its owner, and an
  "agent working" indicator, plus the signed game edits the agent makes, which are attributable to the
  agent key and the owner (12.3).
* The owner may **share a panel's view** with chosen peers. The rendered transcript, never the harness
  process, streams over the session transport as non-sim messages, encrypted to the recipients.
* Defaults and options: **D26**.

### 21.5 Unlocking a panel for others (shared AI chat)

**Access levels** (per panel, owner-controlled):

| Level | Who may send input |
|---|---|
| Locked (default) | Owner only |
| Friends | Peers whose keys are on the owner's friend list (below) |
| Anyone in session | Any current session member |

**Mechanics [I]:**
* A guest types into the owner's panel in-world. The guest's peer sends **signed panel-input messages**
  over the session P2P transport. These go on the control channel, not the sim.
* The owner's peer verifies the sender's key against the access level, then forwards the text to the
  harness as a user turn **wrapped with provenance**, e.g. `[guest: <name>, key <fingerprint>]`.
* **Revocation is local and immediate.** The owner's peer stops accepting that guest's input, drops queued
  prompts, and can end the guest-mode harness session. Revoking never depends on other peers.

**Friends without a central server [I]:**
* A friend is an Ed25519 public key the owner explicitly trusts. It is added by an in-session "add friend"
  handshake, through an invite ticket, or by comparing a key fingerprint or QR code.
* Friend lists are stored locally with petnames and synced across the owner's own devices as signed
  records. Mutual friendship means each key is on the other's list.
* Optionally, a friend key can be cross-linked to a mail-hub person identity (21.7). That is a
  convenience, not an authority.

**Risks of letting others drive your harness:**
* **Prompt injection:** a guest instructs the agent to read or exfiltrate files, run commands, leak
  secrets, or make harmful edits.
* **Filesystem and tool access** on the owner's machine through the harness's built-in shell and file
  tools.
* **Spend** on the owner's account or subscription, plus usage that the AI provider attributes to the
  owner under its terms.
* **Game damage** done under the owner's authority.

**Default guest-mode sandbox while unlocked (D27) [I]:**
* Guests are served by a **separate guest-mode harness session**, never the owner's private conversation.
* Built-in shell, file-write, web and network tools are removed. For Claude Code: `--tools`/`--disallowedTools`
  plus `--strict-mcp-config`. For Codex: `--sandbox read-only` and an approval policy that never
  auto-runs [S flags exist; exact configuration I].
* The only tool server is the engine MCP server, with **guest-scoped** tools.
* The working directory is a throwaway workspace with no owner files.
* The process runs under OS containment: a Windows job object or AppContainer, or Landlock / bubblewrap on
  Linux.
* **Spend caps:** the engine counts turns per guest per hour and per panel. Claude Code also enforces
  `--max-budget-usd` [S]. Codex has no such flag in its help output, so the engine enforces turn caps.
* Guest-initiated SCM commits require the owner's one-click approval by default.
* PTY-mode panels cannot be unlocked.
* The system prompt (`--append-system-prompt`) marks guest turns as untrusted. Safety rests on the hard
  tool restrictions above, not on that prompt.

**Who may make the resulting game edits (authority, section 12) [I]:**
* Actions from a guest-driven panel are signed by the panel's agent key and carry the guest's signed
  request hash.
* **Effective rights = owner's roles ∩ guest's roles ∩ guest-mode scope.** A guest can never borrow a
  friend's agent to do what the guest's own role forbids. The owner's authority is never amplified.
* Commits record `author = agent key`, `on_behalf_of = owner`, `requested_by = guest`, and every peer
  verifies all three against the ACL log.
* Decision: **D28**.

### 21.6 Panel-to-panel connections (agent-to-agent, in-session)

* There is **no implicit broadcast.** Panels communicate only over an **explicitly established
  connection**. Everything below rides the session's P2P transport (control channel), never the sim, and
  is not rolled back.
* **Eligibility ("adjacent"):** recommended to mean **session membership**: any panel in the same session
  may *request* a connection. Spatial proximity is a discovery and UX filter ("nearby panels", point to
  connect in VR), not a security boundary, because positions are controlled by the requester. Options:
  **D31**.
* Panels publish a small presence record (panel ID, owner, display name, whether it accepts requests).

**Handshake [I]:**
1. **Request.** A → B: `ConnectRequest {from_panel, to_panel, purpose (≤280 chars), requested_capabilities
   (text | structured JSON | asset references), nonce, expiry, A's ephemeral X25519 key}`, signed by A's
   panel key.
2. **Gatekeeping on B's peer:** verify the signature, session membership, eligibility and a per-requester
   rate limit. Then apply B's **connection policy**:
   * **allow all:** accept.
   * **deny all:** reject with `{code: "policy_deny"}` (no prompt, no agent involvement).
   * **ask user:** show the owning player an in-world prompt with requester, owner and purpose. Accept,
     reject, or let it time out after 60 s (`user_declined` / `timeout`).
   * **ask agent:** hand the decision to B's own AI (below), with a timeout (`agent_timeout`).
3. **Accept.** B → A: `ConnectAccept {connection_id, B's ephemeral X25519 key, limits (message rate, max
   size, idle TTL), granted capabilities}`, signed. A replies `ConnectConfirm`. Both derive per-connection
   keys (X25519 ECDH plus nonces). Messages stay end-to-end encrypted even when relayed through other
   peers (10.5).

**"Ask agent" presented to the harness [I]:**
* The engine MCP server exposes a resource `primordial://panel/connection-requests`, which notifies on new
  requests, plus tools `panel.requests.list`, `panel.requests.accept(id, capabilities)` and
  `panel.requests.reject(id, reason)`.
* For harnesses without MCP notification support, the structured adapter injects a short, clearly marked
  engine event into the session: "connection request from …; decide with panel.requests.accept/reject".
* The request's purpose text is untrusted input and is quoted with provenance.

**Lifecycle and teardown [I]:**
* **States:** `requested → established → closed`.
* **Messages** are sequenced, acknowledged, and delivered in order. Each is delivered to the receiving
  harness as untrusted input with provenance. Agents use `panel.connect(to, purpose)`, `panel.send(conn,
  payload)`, `panel.inbox` and `panel.close(conn, reason)`.
* **Teardown triggers:** either side closes; idle TTL (default 30 min) expires; either owner revokes; the
  policy changes to *deny all* (existing connections are dropped); the panel closes; a peer leaves; the
  session ends. A `Close {reason}` is sent when possible. Silence past the TTL closes it anyway.
* **Connections are ephemeral** to a session. Reconnecting after a drop re-runs the handshake, optionally
  resuming within 5 min with a resumption token.

### 21.7 Mail-hub integration and the hubchat-style panel

**What is known about Orgtree's hub** [S Orgtree's public docs; nothing was connected or registered]:
* A hub is an HTTP service. The main port 7370 serves an operator page with no login. An optional
  **relay-only door** (port 7371) carries authenticated client traffic. Clients probe 7370, 7378, 7371 and
  HTTPS.
* The hub provides no TLS itself; use a private network (e.g. Tailscale) or a TLS proxy.
* Identities: **person** identities (Hubchat users) and **organization** identities. An org identity has a
  permanent public address minted with an ownership secret that proves ownership; the hub stores
  fingerprints. Devices link to carry a person identity.
* Clients **register, poll and transfer files** over HTTP, with at-least-once custody.
* Hubchat finds people and orgs through a directory. It supports question messages with preset options
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
* **Agents in panels** reach hubs only through engine MCP tools (`hub.contacts`, `hub.send`, `hub.inbox`).
  They never see the identity secret.
* Sending to a new recipient needs the owner's approval the first time.
* **All inbound hub mail is untrusted input**, as Orgtree treats outside mail: data, never instructions.
  It reaches an agent only with provenance.
* **Panel-to-panel over a hub:** two panels in different sessions, or players not in the same session, can
  exchange messages through hub identities, with the same connection policy (21.6) applied to first
  contact.

### 21.8 What this adds elsewhere

* MCP catalog (17.2): `panel.*`, `panel.requests.*` and `hub.*` tools.
* Non-Rust boundaries (20.1): harness CLIs and their cloud model services, plus OS speech recognition if
  chosen.
* Crates (22.1): `pe-panels`, `pe-harness`, `pe-hub`.
* Plan (22.2): desktop panels in phase 4, VR panels and voice in phase 5.

---

## 22. Crate layout, phased build plan, risks

### 22.1 Proposed workspace [I]

```
crates/
  pe-core        ids, fixed-step clock, deterministic math (dmath), rng streams, hashing, errors
  pe-sim         sim store (ECS columns), object hierarchy, component IDL/registry, snapshot + hash
  pe-physics     box3d-rust wrapper: body binding, interactions -> forces/joints, world images
  pe-rollback    input log, ordering (soonest wins), claims, snapshot ring, re-sim scheduler, sync checks
  pe-net         UDP + Noise, reliability, bulk channel, clock sync, membership, NAT traversal
  pe-discovery   invites, mDNS, Mainline DHT records, peer exchange
  pe-scm         content-addressed store, commits, ordering/finality, merge seats, tree merge, ACL log
  pe-logic       wasmi host, guest ABI, fuel, hot-swap + migration, component bindings
  pe-logic-sdk   guest-side crate (no_std + alloc) and component macros; target wasm32-unknown-unknown
  pe-assets      CAS, importers (gltf, png/jpeg), deterministic processing, P2P transfer
  pe-render      wgpu renderer, WGSL hot reload (naga), interpolation + error smoothing
  pe-xr          OpenXR session, Vulkan interop, poses, hand engagement
  pe-mcp         rmcp server, tool catalog, agent auth, forks
  pe-panels      in-world panels: chat UI, terminal emulator (PTY mode), VR keyboard, text (cosmic-text)
  pe-harness     harness adapters: Claude stream-json, Codex exec/app-server, ACP, PTY; guest-mode sandbox
  pe-hub         mail-hub client (Orgtree-style hubs), hubchat-style panel backend
  pe-resonite    package / data-tree / MeshX importers, avatar mapping
  pe-app         executable: threads, windowing (winit), config, CLI
tools/           determinism trace runner, audit_native.py, session simulator (N peers, netem)
spikes/          phase-1 spikes (kept as regression tests)
```

### 22.2 Phased plan (proposal; ordering is D22)

| Phase | Content | Milestone and exit criteria |
|---|---|---|
| 2. Deterministic core | pe-core, pe-sim (store + hierarchy), pe-physics, pe-rollback from the spikes; CI determinism matrix incl. ARM; in-process N-peer session simulator with network conditions; **pe-mcp from day one** as the test and driver interface | M1: 2-4 desktop peers on LAN, physics sandbox with interactions, sync checks, repair; AI agent drives a peer over MCP |
| 3. Real networking + look | UDP+Noise transport, bulk channel, invites, NAT traversal, state-transfer join; wgpu renderer + interpolation; glTF assets + P2P asset transfer | M2: friends play over the internet with no server; late join works |
| 4. Live editing + agent panels | pe-logic + SDK (WASM, hot-swap, migration, component IDL), pe-scm (ordering, rejection, merge seats, ACL), WGSL hot reload, MCP logic/shader tools + forks; desktop agent panels (structured adapters + PTY), unlockable panels with guest-mode sandbox, panel-to-panel connections, hub panel | M3: a player's own Claude Code / Codex, in an in-game panel, edits logic and shaders in a live session; conflicts reject and rebase per R7 |
| 5. VR + avatars | pe-xr (OpenXR + Vulkan interop), hands engagement, 3-point IK, Resonite avatar import v1, VR panels + virtual keyboard + voice input | M4: two PC VR users with imported Resonite avatars interact with physics objects |
| 6. Scale + discovery | Mainline DHT discovery, region partitioning (parallel worlds), snapshot deltas, larger sessions | M5: discovery without invites; 1000s of bodies |

### 22.3 Top risks

1. **Physics CPU budget (serial port).** Rollback re-sim multiplies step cost [V b2, b6]. Mitigations: 6.5
   amortization, 6.6 region partitioning, body budgets (D2).
2. **Unverified ARM and macOS determinism.** Mitigation: CI matrix in phase 2. Until then only x86_64 is
   verified [V].
3. **Snapshot memory.** 2.4 MB per frame at 1024 bodies plus 1.1 MB guest memory [V b6, d1]. Mitigations:
   keyframes, deltas, small guest stacks (6.3).
4. **No central authority means no prevention of timestamp cheating** (8.6). Detection plus ACL eviction
   only. MVP targets trusted sessions (D21).
5. **Transport work.** The 100%-Rust rule excludes the turnkey stacks (iroh, libp2p, quinn's TLS), so we
   build reliability and congestion control (10.2).
6. **Alpha upstreams.** Box3D is v0.1.0 "alpha" and box3d-rust is a recent third-party port [S]. Pin
   versions, run golden tests, wrap the API.
7. **Resonite import legality and format churn** (D19); format details unverified locally (19.1).
8. **OpenXR + wgpu interop** is not yet exercised (18.2). Phase-5 spike, or earlier if VR is prioritized.
9. **Agent harness surface (R13).** Harness CLI flags and protocols change often (Codex's app-server is
   marked experimental) [S]. Unlocked panels expose the owner's machine and account to other players
   (21.5). Mitigations: adapters behind one trait, PTY fallback, guest-mode sandbox, spend caps.
10. **Mail-hub protocol unknowns** (21.7): wire format unverified; permission to integrate (D32).

---

## 23. Decisions needing the user

Each entry gives a recommendation and its alternatives. Section references point to the reasoning.

* **D1. Non-Rust boundaries (20).**
  * Recommend: accept the dynamically loaded or OS-provided components in 20.1 (GPU drivers and graphics
    runtimes, OS libraries and Windows import-lib stubs, the OpenXR loader and runtime, Linux system libs)
    and the Rust toolchain (rustc/LLVM) as a build tool for game logic.
  * Also rule on the gray areas in 20.2: the Wayland C shim on Linux, Rust `asm!` in crypto crates, and a
    DXC DLL if HLSL were chosen.
  * Alternative: stricter readings. No Linux Wayland (X11 only). No inline-asm crates. Every peer must
    avoid the toolchain (Option 1 in D11 already allows that).
* **D2. Physics scale for the MVP (4.4, 6.5).**
  * Recommend: target at most about 1,000 simultaneously awake bodies per session world on the serial
    box3d-rust. Add region partitioning in phase 6.
  * Alternatives: invest early in partitioning, or in contributing parallelism to box3d-rust (it must stay
    bit-exact).
* **D3. Float determinism approach (4.2, 4.5).**
  * Recommend: IEEE f32 with the 4.3 rules plus a CI matrix including ARM. Keep `double-precision` (large
    worlds) off for the MVP.
  * Alternatives: enable double precision now (bigger worlds, slower, different goldens). Fixed point is
    not feasible with Box3D.
* **D4. Tick rate and rollback window (5, 6.4).**
  * Recommend: 60 Hz sim with 4 sub-steps; 2 s (120-frame) window.
  * Alternatives: 90 Hz to match VR displays (+50% CPU, smoother hand physics). A 1 s window (less memory,
    more state transfers) or 4 s (more memory and re-sim).
* **D5. Meaning of "soonest wins" (8).**
  * Recommend: frame stamp, then a seeded fair tie-break within a frame, plus exclusive claims (0.5 s, or
    for the duration of a grab).
  * Alternatives: sub-frame microsecond timestamps (finer, but trusts clocks more); lowest peer ID wins ties
    (simple, unfair); no exclusivity, all simultaneous impulses apply (then "soonest" only orders them).
* **D6. Divergence with identical inputs between exactly 2 peers (9.4).**
  * Recommend: both restore the last agreed frame and re-simulate; if that diverges again, the session
    owner's state wins and the event is reported.
  * Alternatives: the lower key hash wins; or the session pauses for the user.
* **D7. Partition healing beyond the rollback window (10.6).**
  * Recommend: the partition containing more members (tie: the owner's side) keeps its history. The other
    side's interactions are re-applied as new inputs at heal time where still valid.
  * Alternative: always keep the owner's side; or ask the users.
* **D8. Transport (10.2).**
  * Recommend: UDP + Noise (snow without `std`) with our own reliability and bulk channel. 100% Rust and
    proven by spike c's model.
  * Alternatives: quinn + custom pure-Rust crypto (more integration work, standard congestion control);
    relax the rule to allow `ring` and use iroh (most turnkey; also depends on n0 relays).
* **D9. Discovery and the "soft central point" question (11, 10.5).**
  * Recommend: invites + mDNS + peer exchange + Mainline DHT records, and no servers of ours.
  * Accepting the public BitTorrent DHT (and its well-known bootstrap hosts) is the user's call. So is
    whether symmetric-NAT users may use an optional community relay.
  * Alternatives: our own bootstrap or relay servers (reliable, but a soft central point); invites and LAN
    only (purest, worst UX).
* **D10. Root of authority (12).**
  * Recommend: the world creator's key is genesis owner and can add co-owners; optional k-of-n owner
    threshold for ACL changes.
  * Alternatives: mandatory multi-owner threshold; a fully democratic majority of present members.
* **D11. Who compiles game logic (13.2).**
  * Recommend: the committer compiles and peers run the shared wasm (sandboxed), with background
    reproducibility checks by peers that have the toolchain.
  * Alternative: every peer compiles from source (strongest trust; every machine needs the Rust toolchain
    and pays compile time).
* **D12. WASM runtime (13.3).**
  * Recommend: wasmi (pure Rust; deterministic profile, fuel and eager compilation verified).
  * Alternative: wasmtime/Cranelift for speed. Its default build compiles C; whether a pure-Rust
    configuration exists needs an audit.
* **D13. Shader language (14).**
  * Recommend: WGSL (naga-native, pure-Rust validation, structured diagnostics).
  * Alternatives: HLSL via DXC (non-Rust DLL, no naga frontend); GLSL via naga; rust-gpu (Rust shaders,
    heavier toolchain).
* **D14. SCM conflict rules and merge seats (15.3, 15.4, 13.6).**
  * Recommend:
    * conflict = same parent and overlapping paths, with non-overlapping commits auto-merged
      deterministically;
    * the 10 s seat clock starts when the seat reaches the front of the queue;
    * non-seat commits during an active seat are rejected with `seat_held_by` and queued;
    * fuel exhaustion auto-reverts the module to the last good commit.
  * Alternatives: strict linear history (any concurrent commit rejects); the seat clock starts at
    rejection; non-seat commits wait instead of rejecting.
* **D15. ECS (16.1).**
  * Recommend: our own sim store (deterministic columns, WASM-defined components, memcpy snapshots).
  * Alternative: bevy_ecs for everything, adding stable-ID mapping and reflection-based snapshots.
* **D16. Content hash (15.1, 16.6).**
  * Recommend: BLAKE3 in `pure` mode.
  * Alternative: SHA-256 (RustCrypto), slower but ubiquitous.
* **D17. Missing artifact at activation (15.5, 16.7).**
  * Recommend: a peer lacking a sim-relevant artifact at its activation frame pauses its sim, still
    receiving inputs, and catches up via fast-forward once it has the bytes. Others are unaffected.
  * Alternative: delay activation until all present members acknowledge (one slow peer delays everyone).
* **D18. VR targets and hand model (18).**
  * Recommend: SteamVR and Meta/Oculus PC runtimes first, through OpenXR on Vulkan; Windows first, Linux
    (Monado/SteamVR) second.
  * Hands enter the sim only when engaged (proximity or grab); non-engaged hands do not collide.
  * Alternatives: D3D12 interop; always-physical hands (constant input stream, more rollback load).
* **D19. Resonite import: legal and sourcing (19).**
  * Questions for the user:
    * Is parsing Resonite's native package format acceptable under Resonite's ToS and EULA?
    * Should we avoid knowledge derived from decompiled DLLs and work clean-room
      from exported files only?
    * Are users responsible for third-party avatar licenses (e.g. purchased models) when avatars are
      redistributed P2P to other peers?
  * Recommend: clean-room parsing of user-exported files, prefer the standard-format export path (glTF) if
    it preserves skinning, and show users a license notice on import.
* **D20. MVP platforms.**
  * Recommend: Windows x64 desktop and PC VR first; Linux x64 second; macOS flat-screen only (no OpenXR
    runtime) and later.
  * Alternatives: Linux-first; include macOS in the MVP.
* **D21. Trust model for MVP sessions (8.6, 9.4).**
  * Recommend: trusted or invite-only sessions for the MVP, with cheating detection and owner eviction.
    Public sessions with strangers come later.
  * Alternative: design for adversarial public sessions from day one (more protocol work and stricter input
    validation up front).
* **D22. Phase order and first milestone (22.2).**
  * Recommend: phases 2→6 as listed, with the MCP server built from phase 2.
  * Alternatives: pull VR (phase 5) earlier if VR is the headline; pull live editing (phase 4) before real
    networking (phase 3) if the AI-authoring loop matters most.
* **D23. MVP session size (10.1).**
  * Recommend: up to 8 peers, full mesh.
  * Alternative: 16-32 with relay or gossip forwarding (more protocol work in phase 3).
* **D24. Coordinator interpretation to confirm.** "Dynamically loaded system components are acceptable" is
  the coordinator's reading of the 100%-Rust rule, not yet confirmed by the user. D1 asks for that
  confirmation explicitly.

* **D25. Harness integration path (21.1).**
  * Recommend: structured adapters as the default (Claude Code `stream-json`, Codex `exec --json` /
    `app-server`, ACP for Gemini and others), rendered as a chat UI, with a PTY terminal as universal
    fallback.
  * Alternatives: PTY-only (simplest, works with any CLI, but no structured events and unsafe to share);
    SDK-based hosts (Claude Agent SDK is TypeScript/Python, so it would need a non-Rust sidecar process).
* **D26. Panel visibility to other peers (21.4).**
  * Recommend: private by default. Others see the panel, its owner, an "agent working" indicator and the
    signed edits it produces. The owner may share the rendered transcript with chosen peers.
  * Alternatives: visible to the whole session by default (more social, leaks prompts and code); never
    shareable.
* **D27. Unlocked-panel defaults and guest-mode sandbox (21.5). These are risky defaults.**
  * Recommend:
    * panels start **Locked**;
    * Friends or Anyone is an explicit per-panel opt-in;
    * guests get a separate guest-mode harness session with no shell, file-write, web or network tools;
    * the engine MCP server is the only tool server, with a throwaway workspace and OS containment;
    * spend caps per guest and per panel (Claude Code `--max-budget-usd`, engine turn caps for others);
    * owner approval for commits from guest prompts;
    * PTY panels cannot be unlocked.
  * Alternatives: let guests share the owner's normal harness session and tools (convenient, exposes the
    owner's files and account); no unlocking at all.
* **D28. Authority of guest-driven edits (21.5, 12).**
  * Recommend: effective rights = owner ∩ guest ∩ guest-mode scope; commits record agent, owner and
    requesting guest.
  * Alternative: the panel acts with the owner's full authority (a friend could do anything the owner
    can).
* **D29. Friend identity without a server (21.5).**
  * Recommend: local friend lists of Ed25519 keys, added in-session, by invite ticket or by fingerprint/QR,
    synced across the owner's devices as signed records; optional link to a mail-hub person identity.
  * Alternative: friends defined only through a mail hub's directory (convenient, but depends on a hub).
* **D30. Panel connection policy default and timeouts (21.6).**
  * Recommend: new panels default to **ask user** (60 s timeout); **ask agent** is an explicit opt-in;
    switching to *deny all* drops existing connections; idle TTL 30 min.
  * Alternatives: default *deny all* (safest, more friction); default *allow all* (frictionless, invites
    spam and prompt injection).
* **D31. Meaning of "adjacent" for panel connections (21.6).**
  * Recommend: session membership defines who may request. Spatial proximity is a discovery filter only,
    because positions are attacker-controlled.
  * Alternatives: require spatial proximity as well (e.g. within 5 m) for requests, or only for *allow
    all* to apply; or proximity alone.
* **D32. Mail-hub integration (21.7).**
  * Questions for the user:
    * May the engine implement a client for Orgtree's hub protocol (license, permission, and the
      protocol's stability are unverified)?
    * Should the player's hub identity be a separate hub person identity (recommended) or derived from the
      engine key?
    * Should hub use be limited to private networks or a TLS proxy, since the hub has no TLS of its own?
  * Recommend: agents never see the hub secret and use it only through engine MCP tools, with owner
    approval for first contact with each recipient.
* **D33. Voice input (21.2).**
  * Recommend: OS speech recognition on Windows for the MVP (an OS boundary).
  * Alternatives: local Whisper-class models on a pure-Rust ML runtime (100% Rust, heavy); no voice in the
    MVP.
* **D34. Spend and provider-terms responsibility (21.4, 21.5).**
  * Recommend: harness credentials stay in the harness's own configuration and are never read or relayed
    by the engine. The owner is shown per-guest usage and caps before unlocking a panel.
  * Alternative: none that keeps credentials local; pooled or relayed credentials are excluded by R13.
* **D35. e4mc-style addressing and relays (10.7).**
  * Recommend: human-readable session codes resolved via Mainline DHT plus PAKE; volunteer relays that
    anyone can run, forwarding only end-to-end-encrypted traffic, chosen per peer.
  * Alternatives: our own relay and DNS domain like e4mc (simplest, but a central point); iroh with
    self-hosted relays (needs a `ring` exception, D8).
