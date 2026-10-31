# Primordial Engine: Phase-1 Spike Report

Date: 2026-10-30, revised the same day after the design review. Author: engine-architect. Companions:
[design.md](design.md), [review-response.md](review-response.md),
[iteration-benchmark.md](iteration-benchmark.md) (by @iter-bench).

Tags: **[V]** verified by running it here; **[S]** verified by reading source or docs; **[I]** inferred.
Test IDs (a1, b2, …) are the ones design.md cites. All raw outputs are committed under `spikes/out/`.

## 1. Summary

| Spike | Question | Result |
|---|---|---|
| a | Is Box3D usable from Rust, and deterministic across runs and builds? | **Yes [V].** box3d-rust 0.5.0 (pure Rust, no FFI needed) matches all four Box3D C-reference goldens. A 600-frame, 256-body trace with interactions is bit-identical across 7 builds, including Windows-MSVC vs Linux-musl and `target-cpu=native`. **Cross-machine:** an Intel i7-13620H laptop (no AVX-512) reproduced the goldens, all four traces, rollback T1-T5 and the d4 digest bit-identically (run by @design-reviewer; review M5). ARM and macOS were not tested |
| b | Fixed-step rollback: snapshot, step, restore, re-sim with matching hashes? | **Yes [V].** In-place and fresh-world restores replay bit-identically. Late interactions (up to 60 frames) roll back and converge exactly to the reference |
| c | P2P transport between two processes with no server? | **Yes [V].** The phase-1 protocol (5.3) is superseded by **c-v2** (5b): the session protocol of design 6-10, with state + canonical frame, ack-based confirmation and gap fetch, over the same signed UDP. Both peers' 600-frame histories were identical and equal to an offline replay of the final log in all three runs (latency, 10% loss, a never-transmitted entry). Every acceptance check passed |
| session | Do the review's fixes (C1, C2, H4, H5, M3) and the user's state+frame and tie-break rulings hold? | **Yes [V]** (5c). In-process, on a virtual clock: no fork in the C1 scenario; equivocation proved and the member evicted; 52/52 checks under 10% loss; uncontended acceptance 22/22, 25/25 under 10% loss, 25/25 against the final history in `diverged` (23/25 at the instant with vdW physics, 5j); ties 41/50/59 of 150; backdating voided or bounded |
| ball pit | A hand sweeping through 500 balls, without per-ball state streaming? | **Yes [V]** (5d). About 15 kB/s for everything in both directions, against 1 MB/s one way for naive state streaming. The receiver re-simulates 2.8 ms per frame. The end state is bit-identical |
| r14 | Two worlds with different tick rates in one process, and an object handed between them? | **Yes [V]** (5e). Settled handoff; conserved in the ok, abort, reject, throttle, forge, late and crash variants; each world equals its offline replay, also when worlds and peers are processed in reverse order (abort's digest then differs from forward, because its escrow key derives from chain contents) |
| history | Can a peer keep only the perturbation history? Which snapshots, at what memory and CPU? | **Almost [V]** (5f). One base image at or before the confirmed frame is unavoidable; the rest is a cache that never changes results (62 runs, 13 policies, one end hash per scene). Window(4) matches the best modelled CPU at 60 ms with 8.3 MB instead of 47.9 MB under a 116-frame confirmation stall (round 3); at 100 ms it needs its adaptive depth. Partial restores through the API are not bit-exact. Remote-hand dead reckoning cuts the observer's error about 4-18x |
| round 2 | Do the round-2 Criticals and Highs hold fixed, and do capped links recover? | **Yes [V]** (5g). The reviewer's four repros pass (no fork, no panic); proofless R14 relays do nothing; batching plus the congestion controller keep 32-64 kbit/s caps real-time. Finality lag rose from 4.2 to 8.5 frames |
| round 3 | Do colluders, crashes, untrusted worlds and links below 20 kbit/s break the protocol? | **No fork in any case; stalls only where no honest majority exists [V]** (5i). Two colluders stall safely instead of crashing everyone; a crashed member is removed after 3 s; a crashed carrier's transfer completes through another member; worlds trade only along route lists; 16 kbit/s stays real time, 12 kbit/s recovers, a slow third member is demoted. Two members need about 12 kbit/s each way |
| integration | Does everything still hold with every branch merged and vdW physics on? | **Yes [V]** (5j). The full determinism set passes at low priority: goldens, traces, images and rollback in release and debug, the wasm digest, 30 session scenarios, 10 R18 cells, UDP and the snapshot study. Two defects found and fixed: a test-harness bug exposed by the new physics, and module identity (M8) depending on the checkout path |
| view change | Can a session remove several failed or cheating members at once, and admit joiners, without a fork? | **Yes, with D80 settled [V]** (5k). Two members silent at once, colluders, a lying proposer and joins all recover with every member equal; at 256 members the removal is decided 2.77 s after the silence. Under (a), one cheater bridging a partition splits a session of five in two |
| r15 | What does it cost to run physics hooks or a whole physics engine as a wasm guest? | **Measured [V, noisy]** (5h). A per-contact hook call costs 64 ns in wasmi and 16 ns in Cranelift; batched, 12 and 4.7 ns. On an active 1,024-body pile, guest hooks cost 7-11% of the step one call at a time, 1.2-1.5% batched. A whole engine in wasm costs 21x native under wasmi |
| iteration | How fast is the logic edit loop, and which runtime is fastest under the rules? | **Measured [V]** by @iter-bench ([iteration-benchmark.md](iteration-benchmark.md)). Edit to first frame 0.45 s at 6.4k LOC; logic speed relative to native: wasmi 7.0x, wasmtime Cranelift (fuel, NaN canonicalization) 1.7x; minimal wasmtime compiles no C on Windows |
| m8 | Is a module's identity reproducible across checkout paths? | **Yes [V], by canonical hash** (6.5). Raw hashes differ by path; hashes over the code and data sections are identical |
| d | Hot-swap Rust→WASM game logic mid-sim, deterministic across peers? | **Yes [V],** with wasmi in eager-compilation mode. The default lazy mode makes fuel nondeterministic [V]. Box3D itself compiled to WASM matches native [V] |
| mcp | Cheap MCP stub that reads world state? | **Yes [V]** (protocol subset, hand-written, not tested with a third-party client) |
| audit | Can the stack be 100% Rust? | **Mostly [V].** Graphics, VR (loaded loader), ECS, MCP, WASM (wasmi), assets, discovery and a UDP+Noise transport audit clean. iroh, libp2p+QUIC, quinn's TLS, wasmtime default and blake3 default compile C/asm |
| resonite | Parse one Resonite package/record header? | **Not reachable [V]:** no exported package or data-tree file exists on this machine (only a 2.5 GB LiteDB cache) |
| R13 / e4mc research | Can harnesses be driven headlessly and wired to our MCP? How does e4mc work? | **Read, not run [S]:** structured modes and restriction flags confirmed in the installed Claude Code 2.1.292, Codex 0.160.0 and Gemini CLI help. e4mc relay routing read in its Rust source. iroh features read in its docs. Orgtree hub facts from its public docs (no hub contacted). Panel crates audit 100% Rust [V] |

## 2. Environment

| Item | Value |
|---|---|
| Machine | AMD Ryzen 7 9700X (Zen 5; `avx2`, `avx512f`, `fma` available), 16 threads |
| OS | Windows 11 Pro 10.0.26200; WSL2 Ubuntu 24.04.1 (kernel 6.6.87.2) for Linux runs |
| Toolchain | rustc 1.99.0 stable (2026-09-28), `x86_64-pc-windows-msvc`; targets added: `wasm32-unknown-unknown`, `x86_64-unknown-linux-musl` |
| Shell | Git Bash on Windows (commands below use bash syntax) |
| Box3D upstream | github.com/erincatto/box3d, MIT, C17, release v0.1.0 (2026-06-30, "alpha"); HEAD `33626332` (2026-10-28) [S] |
| box3d-rust | 0.5.0 (crates.io, MIT, zero dependencies), ports upstream `3fc20f5`, serial + scalar [S] |
| Other crates | ed25519-dalek 3.0.0, wasmi 2.0.0 (`deterministic`), serde_json 1 |

## 3. Spike (a): Box3D from Rust, determinism

### 3.1 Availability (research) [S]

* **Box3D exists.** Erin Catto's 3D successor to Box2D: MIT, portable C17 with a C API, alpha v0.1.0.
  Its docs state determinism across thread counts and 64-bit platforms (custom atan2/cos/sin; no FP
  contraction; ordering by creation order), checked by a CI unit test.
* **Rust routes:**
  * **box3d-rust** is a pure-Rust port claiming bit-exact results vs the C scalar build, with snapshots and
    recording. Chosen per user ruling.
  * **boxddd** / boxddd-sys are C bindings that compile vendored C with `cc`. Excluded by the 100%-Rust
    ruling.
  * No FFI of our own was needed.
* **Snapshot API:** box3d-rust exposes `recording::{serialize_world, deserialize_into_shell,
  clone_world_via_snapshot, hash_world_state}` publicly. In upstream C the equivalents
  (`b3SerializeWorld`/`b3DeserializeIntoShell`) are internal, not part of the public API.

### 3.2 Commands

These are the phase-1 commands. Since the vdW merge, engine worlds use scale-dependent contact
recycling (`engine_world_def()`, vdw-simulation.md), so the files below are reproduced with
`pe-spike-physics --upstream`; without it, `goldens` checks the engine goldens and `trace` gives the engine
default's digest (3.3, last paragraph).

```bash
cargo build -p pe-spike-physics && cargo build -p pe-spike-physics --release
./target/debug/pe-spike-physics goldens            # a1
./target/release/pe-spike-physics goldens          # a1
./target/release/pe-spike-physics trace 600 spikes/out/trace-win-release-1.txt   # a2
./target/release/pe-spike-physics trace 600 spikes/out/trace-win-release-2.txt   # a2
./target/debug/pe-spike-physics   trace 600 spikes/out/trace-win-debug.txt       # a3
RUSTFLAGS="-C opt-level=1" cargo build -p pe-spike-physics --release --target-dir target/opt1   # a3
RUSTFLAGS="-C opt-level=s" cargo build -p pe-spike-physics --release --target-dir target/opts   # a3
RUSTFLAGS="-C target-cpu=native" cargo build -p pe-spike-physics --release --target-dir target/native  # a4
./target/{opt1,opts,native}/release/pe-spike-physics trace 600 spikes/out/trace-win-<variant>.txt
./target/native/release/pe-spike-physics goldens   # a4
# a5: Linux, static musl, linked by Rust's bundled lld (no C toolchain needed)
cargo build -p pe-spike-physics --release --target x86_64-unknown-linux-musl \
  --config 'target.x86_64-unknown-linux-musl.linker="rust-lld"'
MSYS_NO_PATHCONV=1 wsl.exe -d Ubuntu-24.04 -- bash "/mnt/e/Libraries/Desktop/Primordial Engine/scripts/run-linux-physics.sh"
for f in spikes/out/trace-*.txt; do cmp spikes/out/trace-win-release-1.txt $f; done
```

### 3.3 Results [V]

**a1/a4/a5: C-reference goldens** (`test_determinism.c` float values, as carried in box3d-rust's tests):

| Scene | Expected (C) | Win debug | Win release | Win native | Linux musl |
|---|---|---|---|---|---|
| Falling ragdolls | sleep 291, 0xFF4C605B | match | match | match | match |
| Wave pile | 272, 0x266F83B0 | match | match | match | match |
| Query spawn | 242, 0xF3AD9BA6, 59 hits, 0xE3271F3D | match | match | match | match |
| Mesh drop | 226, 0xD2FF069D | match | match | match | match |

**a2-a5: per-frame trace.** The scene is 256 dynamic bodies (boxes, spheres, capsules) on a ground box, 60 Hz,
4 sub-steps, 11 scripted interactions from 2 peers, including a same-frame conflict. Each line holds
the Box3D world hash and the full state hash (world + claims + frame) for one frame.

| Build | 600-frame trace vs release run 1 | Digest |
|---|---|---|
| release run 2 (a2) | identical | `7903109aab28ed9b` |
| debug (a3) | identical | same |
| release, opt-level=1 (a3) | identical | same |
| release, opt-level=s (a3) | identical | same |
| release, `target-cpu=native` (FMA, AVX-512) (a4) | identical | same |
| Linux x86_64 musl, run in WSL2 (a5) | identical | same |

Evidence: `spikes/out/trace-*.txt`, `trace-comparison.txt`, `physics-goldens-*.log`, `physics-linux-musl.log`.

**These are upstream-physics records.** With the engine default (vdW recycling on), the engine goldens
pass and the trace digest is `80839f4a54133e13`, identical across release, debug, opt-level 1 and s, and
`target-cpu=native` [V @vdw-physics, `spikes/out/vdw/`]. The integration run repeated release and debug:
both equal those records, and `--upstream` still gives `7903109aab28ed9b` [V `spikes/out/integration/`,
5j]. Linux musl and the Intel laptop were not re-run with the engine default.

**Speed:** 0.31 ms/frame release (Windows), 0.40 ms/frame (Linux musl under WSL), 3.8 ms/frame debug.

**Not covered:** aarch64, macOS, other x86 vendors, and Box3D multithreading (the port has none).

## 4. Spike (b): rollback

### 4.1 Commands

```bash
./target/release/pe-spike-physics rollback   # b1-b5 + timings
./target/debug/pe-spike-physics rollback
./target/release/pe-spike-physics bench      # b6
```

### 4.2 Results [V]

| ID | Test | Result |
|---|---|---|
| b1 | Run to 300, snapshot, step to 400, **restore in place**, step to 400 again | Replay == first run == reference for all 100 frames; engine-held `BodyId`s still valid |
| b2 | Interaction for frame 200 learned at frame 260 | Pre-rollback history diverged as expected. 1 rollback, depth 60, 60 frames re-simulated in 33 ms. Final 600 frames == reference |
| b3 | All 11 interactions delivered 1-45 frames late (pseudo-random) | 11 rollbacks, max depth 41, 310 frames re-simulated; 600 frames == reference. Verdicts: frame 120 peer 1 accepted / peer 2 `RejectedClaimed{by 1, until 150}`; frame 125 peer 2 rejected; frame 126 peer 1 accepted |
| b4 | Snapshot at 300 restored into a **fresh `World`** (late-join path) | 300 following frames == reference |
| b5 | Interaction 300 frames old with a 120-frame window | Refused: `TooLate{frame 100, oldest 280}` |

All of b1-b5 also pass in debug and in the Linux build (`physics-rollback-*.log`, `physics-linux-musl.log`).

**b6: costs** (release, Windows, average over 300 frames of a settling pile; two runs):

| Bodies | Step | Snapshot save | Restore | Image size |
|---|---|---|---|---|
| 256 | 0.36 ms (max 0.63) | 0.25-0.26 ms | 0.25 ms | 588,930 B |
| 1024 | 1.54-1.91 ms (max 3.5) | 0.95-1.18 ms | 0.74-0.91 ms | 2,426,102 B |
| 2048 | 4.77-5.92 ms (max 11.2) | 2.21-2.66 ms | 1.67-2.07 ms | 5,411,108 B |

## 5. Spike (c): P2P transport

### 5.1 What it is

* Two processes (`pe-spike-p2p`), each running the same pile scene on a real-time 60 Hz fixed step.
* Plain UDP on loopback; no server.
* Every datagram is Ed25519-signed, and the peer key is pinned from its self-signed Hello.
* NTP-style clock-offset estimate, from the minimum-RTT ping/pong sample.
* Peer 1 proposes the session epoch, a session parameter rather than an ongoing authority.
* Each peer stamps its own scripted interactions with its current frame and sends them reliably (resend
  every 100 ms until acked). Late remote interactions trigger rollback.
* Sync checks every 30 frames, plus 15 frames after each interaction, compared 45 frames later.
* On mismatch the peers exchange per-frame (state hash, input digest) ranges, find the earliest divergent
  frame, swap missing interactions and roll back.
* Artificial one-way delay, uniform jitter and loss are injected at the sender.

### 5.2 Commands

```bash
cargo build -p pe-spike-p2p --release
bash scripts/run-p2p.sh latency60 "--delay-ms 60 --jitter-ms 20" "--delay-ms 60 --jitter-ms 20"   # c1
bash scripts/run-p2p.sh loss10 "--delay-ms 40 --jitter-ms 60 --loss-pct 10" "--delay-ms 40 --jitter-ms 60 --loss-pct 10"  # c2
bash scripts/run-p2p.sh fault "--delay-ms 30 --jitter-ms 10" "--delay-ms 30 --jitter-ms 10 --drop-seq 3"  # c3
```

`run-p2p.sh` compares each peer's 600-frame state-hash history with the offline reference from spike (a)
and with the other peer.

### 5.3 Results [V] (phase-1 protocol, superseded by 5b)

| ID | Conditions (one-way, each direction) | Both peers == reference? | Rollbacks (max depth) | Sync checks | Clock offset estimate (true: 0) | Datagrams sent / dropped |
|---|---|---|---|---|---|---|
| c1 | 60 ms + 0-20 ms jitter | **yes** | p1 6 (6), p2 5 (5) | 31 ok, 0 bad each | -0.02 ms / -5.2 ms | 110 / 0 each |
| c2 | 40 ms + 0-60 ms jitter, 10% loss | **yes** | p1 6 (11), p2 5 (5) | 28 / 27 ok, 0 bad | -5.9 ms / -0.54 ms | 104 / 16, 107 / 12 |
| c3 | 30 ms + 0-10 ms; peer 2 **never transmits** its frame-200 interaction | **yes** | p1 6 (**65**), p2 5 (3) | p1: 29 ok, 2 mismatched; p2: 29 ok, 1 mismatched | -0.63 ms / +0.69 ms | 111 / 0, 98 / 0 |

c3 sequence (peer 1 log):
1. `SYNC MISMATCH at frame 210` (detected at frame 257).
2. `earliest divergent frame 200: interaction logs differ -> exchanging inputs`.
3. `reconciled missing input peer 2 seq 3 frame 200 -> rollback`.
4. The later check re-tests as `histories agree now (stale check)`.

Final digests on all six peer runs: `933b3a6e42ab5b94`. Logs: `spikes/out/p2p-*.log`; histories:
`p2p-*.txt`.

**Not covered:** real internet paths, NAT traversal, more than 2 peers, encryption (signing only), join,
leave, partitions, bandwidth under VR pose load.

### 5b. Spike (c-v2): the session protocol over UDP [V]

* **What changed.** `spikes/p2p` was rewritten on `pe_sim_core::session` (design 6-10):
  * entries carry the canonical frame and the touched body's resulting state;
  * acks carry horizons and holdings, and the confirmed frame is computed from acks only;
  * gaps found in acks are fetched; check hashes ride in acks;
  * an entry is accepted only from its authenticated author (review L4);
  * clocks are monotonic (review L6).
* **Acceptance check (user, 09:31Z):** after a receiver re-simulates a remote touch to its present frame,
  it logs its copy of the object. `scripts/run-p2p.sh` compares that copy with the author's final history
  of the same object at the same frame.

```bash
cargo build --release -p pe-spike-p2p
bash scripts/run-p2p.sh latency60 "--delay-ms 60 --jitter-ms 20" "--delay-ms 60 --jitter-ms 20"
bash scripts/run-p2p.sh loss10 "--delay-ms 40 --jitter-ms 60 --loss-pct 10" "--delay-ms 40 --jitter-ms 60 --loss-pct 10"
bash scripts/run-p2p.sh fault "--delay-ms 30 --jitter-ms 10" "--delay-ms 30 --jitter-ms 10 --drop-stamp 200"
```

Integration run (round-3 protocol, engine default physics with vdW recycling), low priority:

| Run | Peer 1 == peer 2 (600 frames) | Each peer == offline replay of its own log | Acceptance (receiver 1 / receiver 2) | Rollbacks (max depth) | Checks | Datagrams sent / dropped |
|---|---|---|---|---|---|---|
| latency60 | yes | yes, `66d3f4a12f8bdef2` | 8/8 / 6/6 | 6 (4) / 6 (5) | 24 ok, 0 bad | 437 / 0, 438 / 0 |
| loss10 | yes | yes, `66d3f4a12f8bdef2` | 9/10 / 11/11 | 6 (19) / 6 (5) | 23 and 24 ok, 0 bad | 667 / 78, 627 / 65 |
| fault: peer 2 never transmits its frame-200 entry | yes | yes, `66d3f4a12f8bdef2` | 9/9 / 6/6 | 6 (7) / 6 (2) | 24 ok, 0 bad | 416 / 0, 420 / 0 |

* **Fault-run repair path:** peer 2's next ack declared the entry. Peer 1 saw the gap at frame 203, fetched
  it, received it 7 frames late and rolled back. The old range search is gone.
* **loss10's one miss** is receiver 1's copy of object 50 at frame 135, inside the scripted same-frame
  conflict on that object: contended, so it may differ while the competing entry is in flight.
* **In this run all three produced the same history** (`66d3f4a1…`), because no entry was re-stamped.
  **That is not guaranteed under loss with the engine default [V]:** @vdw-physics got two different
  loss10 histories in four runs, and @design-reviewer three more; fault varied too. In every run both
  peers agreed and equalled their offline replays. The cause is the state+frame rule, not recycling: an
  entry carries its author's speculative state, which depends on what the author had received in time
  (vdw-simulation.md 5.1). With upstream physics the scripted touches happened not to be sensitive
  (`cd613a64…` in every run).
* An earlier fault attempt hit an OS-level stall of peer 1's process (no frames advanced for 3.6 s). The
  late entry was then re-stamped deterministically, by the arrival bound, and both peers still converged.
  That log was overwritten by the rerun shown above.
* Logs: `spikes/out/p2p-v2/`.

### 5c. Session simulator: the review fixes [V]

* `spikes/session-sim` runs N members and M worlds in one process. It has a virtual clock, and virtual
  links with delay, jitter, loss and scripted faults.
* Every message is encoded to bytes, so byte counts are real.
* The output is deterministic: the same command prints the same RESULT lines.
* `scripts/run_session_sim.sh` builds it and runs every scenario. Logs are in `spikes/out/session-sim/`,
  with a summary in `summary.txt`.
* **This table records round 1 (3d0f98a).** Rounds 2 and 3 changed the protocol: nothing is re-stamped
  (5g), and a member lagging its horizon is demoted (5i). Current results, with vdW physics: 5j.

| Scenario | Setup | Result |
|---|---|---|
| c1 | The reviewer's C1: member 3's frame-50 entry reaches the others at frames 169 and 171 | "Before" (phase-1 rule) reproduces the fork. "After": all three 700-frame histories identical and equal to an offline replay; 50/50 checks. The entry was re-stamped to frame 153 by the arrival bound. The others paused 6-9 frames at the speculation limit |
| c2 | Member 9 sends different seq-1 entries to members 1 and 2 | "Before" reproduces the permanent divergence. "After": equivocation proved at both, both versions voided, member 9 evicted by majority about 10 frames later; honest histories identical and equal to replay |
| state-frame | 3 members, 60 ms ± 20 ms, the scripted schedule | Uncontended acceptance 22/22 (at that instant and against the final history); 52/52 checks; max rollback depth 4 |
| diverged | Member 2 misses member 3's frame-300 entry for about 0.4 s; member 1 touches the same object at 305 | 23/23 uncontended acceptance; the carried-state overwrite fired once; gap repair; histories identical |
| loss | 10% loss, 40 ms ± 30 ms | 27/27 uncontended acceptance; 52/52 checks per pair; histories identical |
| ties | 3 members, 150 same-frame conflicts on one object | Wins 41 / 50 / 59 (χ² = 3.24, p ≈ 0.2); histories identical |
| claims | Member 1 nudges every 29 frames, member 2 every frame (the reviewer's M3) | Member 2 accepted 360/600 (0/600 under the phase-1 rule), member 1 9/21 |
| backdate | Member 3 stamps entries 20 frames in the past | Part 1, honest horizons: all 3 voided as horizon violations. Part 2, horizons lagged 30 frames: re-stamped +7 frames by the arrival bound; confirmation lags 30 frames for everyone |

*Contended* acceptance checks are those where another member touched the same object in the previous
10 frames: the scripted same-frame conflict on object 50, 10 per run. They can differ while the competing
entry is in flight, and they converge when it lands.

### 5d. The ball pit (the user's "main question") [V]

* **Setup:** 500 balls (radius 6 cm, with rolling resistance so the pit settles) in a 2.4 m walled pit; a
  kinematic hand (radius 9 cm) sweeping a sine path for 3 s; two members, 60 ms ± 10 ms, 2% loss unless
  stated.
* **Option (a), the hand as a perturbation source:** one 44-byte entry per frame with the quantized
  target.
* **Naive state streaming** is measured on the author: every ball whose state changed in a frame, at 56
  bytes each.

| Run | Our traffic, both directions | Naive, one direction | Receiver rollbacks/s, re-sim per frame | End state |
|---|---|---|---|---|
| 60 Hz, 1 hand | 14.7 kB/s | 1.00 MB/s (0.47 MB/s quantized) | 59/s, 2.76 ms (step 0.35 ms) | identical |
| 60 Hz, 0% loss | 12.4 kB/s | 1.00 MB/s | 59/s, 2.69 ms | identical |
| 60 Hz, input delay 4 frames | 14.7 kB/s | 0.97 MB/s | 2.7/s, 0.27 ms | identical |
| 90 Hz | 22.4 kB/s | 1.44 MB/s | 88/s, 3.95 ms | identical |
| 60 Hz, two hands (one per member) | 21.6 kB/s | 1.23 MB/s | 59/s, 3.01 ms | identical |

* The pit was settled before the sweep (0 balls changing per frame); about 297 balls moved per frame during
  it.
* "Our traffic" counts each message as one datagram with 52 bytes of overhead.
* Every run's history equals an offline replay.

### 5e. Two worlds in one process, one object handed between them (R14) [V]

* **Setup:**
  * world A: 60 Hz, a 108-body pile; world B: 50 Hz, a 75-body pile;
  * member 1 is in both and carries object 40; member 2 is only in A, member 3 only in B;
  * links 50 ms ± 15 ms, 1% loss;
  * the settled handoff of design 22.6.

| Variant | What it exercises | Outcome | Live copies at the end (A + B), pending escrow |
|---|---|---|---|
| ok | Plain handoff; member 1 also touches object 25 in A at frame 260 | Committed. Departure authored at 2,766 ms; certified arrival authored in B at 2,964 ms; arrival confirmed in B and verdict authored at 3,197 ms; verdict confirmed in A at 3,354 ms (round 2) | 0 + 1, 0 |
| abort | Member 2's earlier grab reaches member 1 150 ms late; A rolls back 12 frames past fA | Departure `RejectedClaimed`; nothing relayed | 1 + 0, 0 |
| reject | Member 3 closes B's portal, and member 1 hears of it 150 ms late; B rolls back 10 frames past fB | Arrival rejected; the verdict returns the object to A | 1 + 0, 0 |
| throttle | Member 1's replica of A advances only once per 250 ms from t = 3 s, and member 1 keeps authoring in A | As ok; both worlds' digests equal the ok run's. The Verdict went out at frame 185 while the throttled sim stood at 175, at the ok run's time; the touch at frame 260 kept its stamp after a catch-up burst. Member 2's confirmation lag in A is unchanged (8.5 frames before t = 3 s, 8.6 after). Member 1's check hashes reach the log 10.2 frames after their check frame (10.0 unthrottled) (review NEW-12) | 0 + 1, 0 |
| ok, abort, reject in reverse order | Worlds created B first, peers processed in reverse | Digests identical to the forward runs | as forward |

In every variant, every replica of each world equals the others and an offline replay of that world's
final log.

### 5f. Perturbation history, the snapshot knob, and what observers see [V]

Design 6.7-6.11. Logs: `spikes/out/perturbation-history/`.

* **Re-simulation from one base image** (`pe-spike-physics history`; restore plus N steps, median of 5;
  `physics-history-upstream-442286f.log`, from 442286f on a quieter machine, upstream physics):
  * 256 bodies: 0.8 / 2.1 / 6.7 / 27.6 / 50.3 ms at depths 1 / 4 / 15 / 60 / 120;
  * 1,024 bodies: 3.2 / 7.6 / 24.2 / 94.7 / 183.3 ms.
  * The integration rerun with the engine default (`physics-history.log`, low priority) took 1.0-1.2x as
    long. With vdW recycling the 256-body pile started at frame 900 falls asleep during the replay (246
    awake, 26 after 120 frames): 6.2 ms at depth 120.
* **Partial restore through the API is not bit-exact** (which shows the API's limit, not that per-island
  snapshots with contacts and warm starts cannot work; review PH-4):
  * every body's pose and velocity written back diverges at the first frame (2/256 equal after 30 frames,
    340 mm with the engine default, 244 mm with upstream physics);
  * an isolated stack restored while 63 others sleep matched 0/4 (8.7 mm); the others 252/252.
* **Image deltas (64-byte blocks)** save at most 8% on ball-pit rings, and nothing on an active pile.
  General-purpose compression does better: LZ4 to 44-47%, deflate to 34-38% [V reviewer, PH-3].
* **Snapshot policies on the protocol** (`pe-spike-session-sim snap [--balls 1024] [--stall]`; ball pit,
  60 ± 10 ms, 2% loss):
  * every policy and the moving knob end in the same hash per scene: `020828ac9b6fb6c8` at 500 balls,
    `0d739d142b58347a` at 1,024 (engine default; `47bb8c7274b143d3` and `bfe86c8ce148add8` with upstream
    physics);
  * maximum image memory under a 116-frame stall (round 3, current protocol): Every(4) 47.9 MB, Log(4)
    17.8 MB, Window(4) 8.3 MB, base only 3.3 MB, at 278 re-simulated frames/s for the first three;
  * steady (26-frame lag): Every(4) 10.5 MB, Window(4) 8.3 MB; base only re-simulates 726 frames/s,
    +79% modelled CPU (review PH-1; the first figure was +17%);
  * base only under the stall: 2,724 re-simulated frames/s (16.8 ms per frame modelled);
  * CPU figures are a model [C], from the work counts; the round-3 timings ran at low priority and are
    noise (review PH-7).
* **Observer** (`pe-spike-session-sim observer --delay 30|60|100`): remote-hand error, mean / p95, hold →
  dead reckoning:
  * 30 ms: 24.0 / 33.5 → 1.3 / 2.0 mm;
  * 60 ms: 78.9 / 121.8 → 14.7 / 17.0 mm;
  * 100 ms: 148.9 / 197.3 → 40.7 / 37.0 mm.
  * Balls visibly corrected per frame at 60 ms: 21.7 → 8.6 (engine default).
  * The hand errors do not depend on the physics: round 3's upstream run gave the same values.
* **Regression:** the default settings (Every(4), hold) reproduce the ball-pit scenario's end hash
  `020828ac9b6fb6c8` and digest `84fd01d1d13bb6a5` (engine default; with upstream physics, 3d0f98a's
  `47bb8c7274b143d3` and `1a9f1fb5fce6acc6`).

### 5g. Round 2: chain-entry protocol, R14 proofs, recovery under caps [V]

Design 6.4, 8.2, 9.3, 10.8-10.9 and 22.6; review-response.md, round 2. Logs: `spikes/out/session-sim/`,
`spikes/out/p2p-v2/` and `spikes/out/r18/`.

* **Protocol.** Acks, votes and attestations are chain entries. Confirmation comes from stable data. Nothing
  is re-stamped.
* **Suite.** `scripts/run_session_sim.sh` runs 25 scenarios. Every replica is bit-identical to its offline
  replay, with no panic and 0 bad checks, and the script now fails otherwise.
* **The reviewer's repros:**

  | Repro | 3d0f98a | Round 2 |
  |---|---|---|
  | `ackfork`: member 9 shows member 2 a different chain | honest members forked | conflict proven, 9 evicted (cut 1), touch void at both, equal |
  | `outage`: 6.5 s one-way ack outage, honest members | forked | equal; members 2 and 3 rolled back 38 frames |
  | `lateequiv`: second version of a confirmed entry | every honest member panicked | stable version kept, 9 evicted (cut 103, from frame 206), equal |
  | `evict`: eviction after a 600 ms-late entry | a member panicked | the entry stays at frame 60 everywhere; equal |

* **Other scenarios:**
  * `c1`: the 2 s-late entry keeps frame 50; members 1 and 2 roll back 116 frames; equal.
  * `backdate`: three entries backdated 20 frames are void everywhere. Lagging horizons void all 318 of
    member 3's acks: no window opens, and confirmation stalls.
  * Uncontended acceptance: 22/22, 37/37 (10% loss) and 26/26 (diverged), both at the instant and against
    the author's final history.
* **Confirmation lag:** 8.5 frames at 50 ms ± 15 ms (4.2 in 3d0f98a).
* **UDP** (`scripts/run-p2p.sh`):
  * latency60: equal, acceptance 7/7 and 6/6;
  * loss10: equal, 24 ok / 0 bad checks, acceptance 3/3 and 15/15;
  * fault: hole seen at frame 202, fetched, equal.
* **R14 proofs** (`r14 forge`). Each of these is `RejectedNoProof`, and the object is conserved:
  * a member of B mints the object;
  * a member of B squats the transfer id;
  * a member of A commits the escrow without B's certificate;
  * the carrier claims a rejection with B's signatures over the acceptance.

  A correctly certified record with a NaN position is `RejectedInvalid` (review NEW-21). The ok, abort,
  reject and throttle runs conserve the object as before. Canonical check hashes give
  `r14-throttle` 18/0 and 15/0 (was 42/24).
* **R18 under caps** (`scripts/run_r18.sh`; ball pit, 50 ms one-way, token bucket with a 500 ms queue):
  * per-tick batching cuts the hand author's uplink from 77 to 65 kbit/s;
  * the adaptive controller keeps 64, 48 and 32 kbit/s caps real-time: 0 stalled frames, 0 drops, queues
    at most 94 ms;
  * without it, 48 and 32 kbit/s collapse: up to 5,210 drops and hundreds of stalled frames, and the
    members never catch up within the run;
  * in every run the author never rolls back.

### 5h. R15: physics hooks from a wasm guest [V, timings noisy]

Design 27.5. Logs: `spikes/out/r15/`. Runs used low priority on a shared machine, so timings are noisy;
call counts are exact.

* **Per-call cost from native code into a guest, fuel on** (`iter-host callback`, guest
  `spikes/iter-bench/cb_guest`):

  | Runtime | Per pre-solve call | Batched, per contact |
  |---|---|---|
  | wasmi | 64 ns | 12 ns |
  | Cranelift + NaN canonicalization | 16 ns | 4.7 ns |
  | Winch | 21 ns | 5.8 ns |
  | native | 1.3 ns | — |

* **Hook frequency on the active phase-1 pile** (`pe-spike-physics hooks`), re-measured at integration
  with the engine default (vdW recycling on; review R15-1):
  * 256 bodies: 160 pre-solve calls and 319 friction/restitution mixes per step (upstream recycling: 141
    and 281);
  * 1,024 bodies: 610 and 1,220 (upstream: 488 and 976).
  * So 13-25% more calls: the scaled recycling radius of these bodies (0.4 m half-extent), about 0.04 m,
    is below upstream's 0.05 m, and a recycled contact skips its hooks [S `collide.rs`; I for the cause].
* **Composed cost:**
  * one guest call per contact: 7-12% of the step under wasmi;
  * batched (needs the fork): under 2% under wasmi.
* Box3D itself in wasm: 21x native under wasmi and 4.7-5x under Cranelift with NaN canonicalization
  (iteration-benchmark 4.5).

### 5i. Round 3: colluders, crashes, R14 trust, and links below 20 kbit/s [V]

Design 9.3, 10.8, 10.9, 22.6, 22.7. Logs: `spikes/out/session-sim/` (`review3-*`, `r14-*`) and
`spikes/out/r18/`. Runs used low priority; outcomes and counts are deterministic.

* **Suite:** 30 scenarios. Every replica equals its offline replay, and every transferred object is
  conserved, with no panic and 0 false checks. The script now also fails on a lost or duplicated object.
* **Colluders** (`badvote`, `liar`; members 1, 2, 8, 9, where 9 equivocates on a touch confirmed at
  frame 50):
  * 8 forks its vote and shows the honest members a forged frozen head of (0, 0): before, every honest
    member panicked. Now the forged vote never becomes stable, and the eviction waits. The honest members
    stay equal to the replay at confirmed frame 203: a safe stall, as two colluders of four leave no
    honest majority;
  * 8 votes only with the forged head: the lie is detected and void, 9 is evicted with a cut that keeps
    the confirmed touch (seq 104, from frame 208), and the session goes on.
* **A crashed member** (`crashstall`, member 3 of 3 silent from t = 3 s): removed after 180 frames of
  silence (cut at seq 86, from frame 173), after 72 paused frames. At t = 15 s confirmation is 4-6 frames
  behind the present, and both survivors equal the replay. Before: stuck at frame 169, 604 paused frames.
* **R14 trust** (`r14 forge`, now 9 forged entries): a departure to a world one member controls, a
  departure to an unknown world and a mint from a world one member controls are `RejectedNoRoute`; a
  certified record with its shape kind changed is `RejectedNoProof`; a NaN record is `RejectedInvalid`.
  Objects 50 and 51 stay in A, and object 40 is conserved.
* **Deadline** (`r14 late`, a 2-frame deadline): the arrival is `RejectedLate`, and the certified refusal
  returns the object to A.
* **Crashed carrier** (`r14 crash`; worlds of three members, member 4 in both; the carrier goes silent
  84 ms after departing): both worlds remove it, member 4 relays, and the object arrives in B at 6.6 s,
  conserved.
* **Links below 20 kbit/s** (`scripts/run_r18.sh`, batched + adaptive transport):

  | Cell | Stalled frames | Max queue | Back to real time after the gesture | End state |
  |---|---|---|---|---|
  | 16 kbit/s | 0 / 0 | 364 ms | at once | equal |
  | 12 kbit/s | 1,201 / 1,214 | 484 ms | 19.3 s | equal |
  | 8 kbit/s, two members | 1,586 / 1,588 | 498 ms | never in 30 s | not converged (below the floor) |
  | 64, then 16 kbit/s from t = 3 s | 0 / 0 | 343 ms | 60 ms | equal |
  | 4 members, 64 kbit/s shared per uplink | 0 (all four) | 215 ms | at once | all four equal |
  | 3 members, member 3 on 8 kbit/s | 135 / 135, before member 3 is demoted | 498 ms on member 3's links | at once | members 1 and 2 equal |

  Before round 3, every cell below about 20 kbit/s, and the shared uplink, collapsed for good.
* **UDP spike** rerun with the round-3 repair changes: latency60, loss10 and fault all equal, 24 ok and
  0 bad checks per peer.

### 5j. Integration: every branch merged, everything re-run with vdW physics [V]

Branch `integration` (2026-10-30), off 3d0f98a: round 2, R15, the round-2 follow-ups and round 3
(`round3-fixes`, 11185cc, approved by @design-reviewer); vdW contact recycling (`vdw/contact-recycling`,
d082ab5); R16 and R17 (`chunks/r16`, a9a20cc; design 25 and 26); the ball-pit video matrix
(`viz/ballpit-matrix`, 46a9e02); and the WGSL-only note from the Slang evaluation (6344ec1, design 14).

```bash
CARGO_BUILD_JOBS=4 cmd //c start "" //low //b //wait bash scripts/run_determinism.sh   # parts: physics wasm mcp session r18 p2p hooks history
```

Everything ran at low priority, one job at a time; physics outputs are in `spikes/out/integration/`, the
rest regenerate their usual logs. Digests and counts are exact; timings are noise.

| Part | Result |
|---|---|
| Physics, release and debug | Engine goldens pass; the Box3D C goldens pass with `--upstream`. The 600-frame trace is `80839f4a54133e13` in both, equal to the vdW branch's record; `--upstream` gives the phase-1 `7903109aab28ed9b`. Snapshot images equal the vdW record (`7a47a3bfcdbd7eda` release). Rollback T1-T5 pass |
| Wasm (d1-d4) | Pass. The wasm32 pile's digest is `80839f4a54133e13`, equal to native. d-lazy fails, as it should (6.3) |
| Module identity (M8) | All four modules built at a second checkout path are byte-identical, after two fixes (6.5) |
| MCP stub | 10 of 10 responses, the same outcomes |
| Session suite | 30 scenarios: every replica equals its offline replay, every transfer is conserved, no panic, 0 false checks, 0 non-finite states. Against round 3, only physics-dependent lines changed: digests, the ball pit's changed balls per frame (292 / 284 / 375 / 303 at 60 Hz / ahead 4 / 2 hands / 90 Hz), and `diverged`'s acceptance at the instant, 23/25 (25/25 against the final history; design 6) |
| R18 caps (10 cells) | Every rate, queue, drop, stall and recovery figure equals round 3's (5i); only end digests changed |
| UDP (latency60, loss10, fault) | Peers equal and both replays EQUAL in every run; all three `66d3f4a12f8bdef2` in this run. Under loss the history can vary between runs (5b) |
| R15 hooks | 13-25% more calls per step on the active pile (5h) |
| Perturbation history | Every work count equals round 3's; image bytes moved by at most about 1%; end hashes `020828ac9b6fb6c8` (500 balls) and `0d739d142b58347a` (1,024) for every policy; design 6.7-6.10 refreshed |

**Found at integration:**
* **A test bug, not a protocol bug.** `r14 forge` picked the first escrow in A's map as the real
  transfer. Transfer ids derive from the carried state, so with vdW physics the map order changed, and it
  picked a forged departure's escrow and panicked. It now takes the pending escrow, and fails if any of
  its 9 attacks is not tried. The 9 outcomes are the same as in round 3.
* **Module identity had regressed** since the fork became a path dependency (6.5). Fixed and verified.
* **Hooks fire more often with vdW** on the phase-1 pile (bodies of 0.4 m half-extent), not less as 27.6
  had inferred (5h).
* **The physics-history pile at frame 900 is not always awake:** with vdW recycling the 256-body pile
  falls asleep during the replay, so the log now prints how many bodies are awake after the deepest
  re-simulation instead of "this pile never sleeps".
* **Some doc numbers were stale before integration** and are refreshed: the observer table (from
  442286f), the 90 Hz ball-pit work count (from 3b02a68), and the knob-moving memory peak.

### 5k. Phase 2, P2-4: the proposer-based view change [V]

Design 10.8, 22.7 (P25-5) and 8.4 (`handowner`). Branch `viewchange`, on main 9af3414. Logs:
`spikes/out/session-sim/` (`p2-4-*`, `review3-*`, `handowner`, `r14-chain-epoch`, `summary.txt`). The
suite ran at idle priority under the org build lock; outcomes, frames and digests are deterministic, wall
times are noise.

```bash
cargo test --release -p pe-sim-core --lib     # 4 provenance unit tests, epoch certificates included
bash scripts/run_session_sim.sh               # 53 scenario logs, plus the P2-5 and P2-4 controls
```

* **Suite:** every replica equals its offline replay, every transferred object is conserved, no panic,
  0 false checks, 0 non-finite states, and no confirmed frame is rolled back (asserted). The p2p spike
  builds. Design 10.8 has the full table; in short (D80 (b) unless named):
  * **Several failures at once:** two of six silent members are removed in one change at 6.15 s
    (`twosilent`); two of five are removed under (a), and under (b) the other three pause safely at
    confirmed frame 169 (`twosilent5`).
  * **Proofs and colluders:** the round-3 stalls recover. `badvote` and `liar` remove both colluders with
    no pause (round 3: stuck at confirmed frame 203 after 150 paused frames); `colluders` removes both in
    one change at 3.73 s; `withheld` removes the equivocator about 160 ms after its second signature,
    without the member that casts no view-change entry.
  * **A lying proposer** (`badproposer`): its round fails, and round 1 removes it with the silent member
    at 6.58 s.
  * **A cheater bridging a partition** (`bridge`, five members): under (a) the two sides remove each other
    and end as two sessions, the fork D80 described; under (b) nothing is decided during the partition
    (529 paused frames), and after the heal the one session resets at 13.67 s, keeping the cheater, which
    behaves again and is proven of nothing (round 1 removed it at 13.65 s as a withholder; see round 2).
  * **Joins:** member 4 is admitted at 2.10 s with boundary frame 175, and its touch at frame 420 applies
    at every member (`join`). A member that keeps acking but stops referencing the others' chains is
    removed at 5.23 s (`refwithhold`).
  * **Scale** (`vcscale`, a log-only world, two members silent from 1 s): all 62 live members of 64 (with
    ack heads) and all 254 of 256 (without) decide the removal 2.77 s after the silence began, against the
    world log's 8.9 s budget (25.5.2). Without heads nothing confirms, so the 256 run times the decision
    alone: full heads grow as n³ when every replica runs in one process, about 3 GB at 256 [I]. Wall
    times 41 s and 91 s.
* **Epoch certificates across worlds** (P25-5, `r14 chain-epoch`): A removes member 5 (epoch 1 from frame
  250); its hop to B, signed by epoch 1 at frame 326, carries that certificate, and C, which does not
  route to A, accepts the chain. The object is conserved and every world equals its replay.
* **A member moves only its own hands** (`handowner`): member 2's 46 entries for member 1's hand are
  refused at both replicas, and the history equals the run without them.
* **Changed against main**, outside the new scenarios: `backdate` removes its lagging member sooner (66
  paused frames instead of 128, cut at seq 87 instead of 119); `r14 crash` pauses 5 or 6 frames longer
  (80 and 78 instead of 75 and 72); in `crashstall` member 2 pauses 74 frames instead of 72, with the same
  cut and history; and the `r14 forge` and `r14 chain` digests changed because hops now carry their
  commit frame and epoch certificates. Every outcome is unchanged; the ball pit's lines differ only in
  timings.
* **Found and fixed while building it:** exclusions were not verified before deciding, so a faulty
  minority could have shrunk the quorum; a pending joiner's entries acted before admission; a vote froze
  processing of the suspect's chain, so after `bridge`'s heal no reset could be decided; and a member
  acking without referencing others stalled confirmation for good (it is now voted out).
* **Round 2** (@design-reviewer's review of b01ebdf asked for changes: VC-1 to VC-10, HO-1, HO-2). Fixed
  on `viewchange` together with the D80 admin layer, which the user settled at 22:13Z. The whole suite
  is again ALL EQUAL, with no panic and 0 false checks; sim-core's 4 unit tests and pe-mcp's 46 tests
  pass, and the p2p spike builds.
  * Each review repro runs twice, once with its fix switched off by a fixture flag
    (`WorldSim::fixes_off`), so the failure and the fix show in one log:

    | Repro | With the fix switched off | With the fix |
    |---|---|---|
    | `fork9` (VC-2, Critical) | member 3, holding the other branch of 9's fork, confirms past the touch, then trips the rollback assertion at 2.35 s | all three equal the replay; 9 is cut on its touch's branch |
    | `jointiming` (VC-3) | no decision; confirmation stuck at frame 176, every sim paused at the speculation limit | the join is decided in round 0 at 3.6 s |
    | `bogusprecommit` (VC-4) | confirmation stuck for good at frame 112 (boundary 1) and 143 (just ahead) | the first precommit is void; the second's cap is cleared by a reset at 2.13 s |
    | `futureack` (VC-5) | confirmation stuck for good at frame 112 | 9 is removed at 5.23 s (acks void, so too slow) and at 5.17 s (an epoch nobody decided) |

  * `fork9` found a second half of VC-2: while a replica re-validated the cut's branch, each replayed ack
    recomputed confirmation before that branch's entries acted, so confirmation could pass a frame they
    were about to change. Confirmation now waits until the switch is done.
  * VC-1: the bound is restated over every faulty member, proven or not (10.8, D80). The `onesided`
    control (3 of 6 collude, proofs only on one side of a partition) forks, as the bound allows.
  * VC-6, VC-9, VC-10 and HO-1 are fixed; `handowner` now also covers a grab and a carry of another
    member's hand. VC-7 and VC-8 go to @provenance-eng's follow-up, stated plainly in 22.7 and 10.8.
  * The D80 admin layer: `kickwar`, `kickpartition`, `lastadmin`, `adminclose` and `joinrules`, all equal
    (results in 10.8's table).
  * Changed against round 1: `bridge` under (b) now ends with a reset that keeps member 8, whose stopped
    references lay behind its own votes, which VC-9 no longer counts as withholding; and `handowner`'s
    digests, since member 1 now touches its own hand in both runs. Every other line equals round 1's,
    timings aside.
* **Round 3** (@design-reviewer's review of 758ff5e asked for changes: VCR-1 to VCR-12, and HO-1's carried
  state). VCR-10 is a question for the user (V1-V3) and is not built. The rest is fixed on `viewchange`,
  each with a repro that runs with its fix switched off first:

    | Repro | With the fix switched off | With the fix |
    |---|---|---|
    | `ghostkick` (VCR-1) | confirmation stuck at frame 108 | a reset at 5.13 s answers the vote |
    | `selfkickhead` (VCR-2) | the kick never goes through; confirmation stuck at frame 116 | 4 removed at 2.28 s |
    | `ackgap`, `latejoin` (VCR-3) | the member's later acks are void, and it is voted out as too slow | every member stays |
    | `capchurn` (VCR-4) | 88 resets, the faulty member never removed | three resets, then it is voted out at 2.50 s |
    | `deepfork` (VCR-5) | 3 never gets the whole cut branch and stops at frame 116 | all equal |
    | `cutbackdate` (VCR-6) | 3 applies a backdated entry below its confirmed frame: rollback assertion at 2.29 s | the entry is void; all equal |
    | `joinattests` (VCR-7) | 1 and 2 lock on the join, 3 refuses it: wedged, confirmation stuck at frame 176 | 5 joins as Guest |
    | `slowadmin` (VCR-8) | the follower panics at 2.23 s | it decides at 3.60 s, once it holds the entry |
    | `kickbackdate` (VCR-9) | the backdated kick comes first and removes the other admin | the backdated kick is void |
    | `lagprecommit` (VCR-12) | the lagging member's precommit is void: no decision, stuck at frame 224 | 5 admitted at 2.95 s |
    | `handowner` (HO-1) | member 1's history changes | the carried state is refused; same history |

  * `aheadlink` (VCR-12) checks a claim rather than a fix: a member that cannot hear the others is
    removed for not keeping up, and the votes it casts against the members ahead remove none of them.
  * VCR-11: the encoding refuses a join or change with more than 64 attestations and a role id wider
    than 16 bits, rather than reading part of it, and an epoch at the change-layout flag is not encoded
    (four new sim-core unit tests).
  * Every other scenario keeps its outcome; decision times and confirmed frames move by a frame or two,
    because members now ack on entering an epoch.
* **Round 4** (@design-reviewer's review of a9286fc asked for changes: VC3R-1 to VC3R-8). Round 3's strike
  rule and its ahead exemption are gone; the levers they guarded get rules of their own (design 10.8):

    | Repro | With the fix switched off | With the fix |
    |---|---|---|
    | `strikeframe` (VC3R-2, the reviewer's) | round 3's strikes vote honest 1 out in two of five timings | 1 kept in all five |
    | `aheadshield` (VC3R-1, the reviewer's) | the climber, sending each entry to one member in turn, is never removed; stuck at frame 112 | voted out at 5.30 s |
    | `lagchurn` (VC3R-2) | timers restarting at each decision: the lagging member is never demoted, after 8 resets | demoted at 6.48 s |
    | `adminvoid` (VC3R-3) | a faulty proposer commits its own horizon-void SetRole | refused |
    | `joinsquat` (VC3R-4) | a request in the joiner's name pre-empts it: never admitted | joins as Guest at 2.63 s |
    | `joinskip` (VC3R-6) | a joiner that acked nothing before admission: acks void, voted out | it stays |

  * `capstale` and `capchurn`: a stale cap now gets its author voted out, with no reset (3.17 s). `ghostkick`:
    a kick vote whose kick nobody delivers gets its voter voted out (3.17 s); round 3 reset and kept it.
  * `votechurn` measures what is left: a member voting against an honest one in every epoch causes 2.5
    resets a second and raises the median confirmation lag from 7 to 16 frames, until an admin kicks it.
    Whether such a member should be voted out automatically is the user's question V4.
  * `fakeref` and `aheadlink` are checks, not repros: fake references do not spare a silent member (the
    withholding rule removes it), and a member behind casts votes against the members ahead harmlessly.
  * VC3R-5: a cap has no upper bound now (`lagprecommit` still passes); VC3R-7: a cut's branch is fetched
    by id (`deepfork` still passes); two new sim-core unit tests for the signed join and the branch fetch.
  * `handowner` now checks and digests only frames both replicas have confirmed (0..346), as
    @ballpit-viz's tool port found it should; the history is still the same with and without member 2's
    entries.
  * Every other scenario keeps its outcome: `kickpartition` decides 2 frames sooner, `bridge` under (b)
    ends with two resets after the heal instead of one, and timings move by a frame or two.
* **Round 5** (@design-reviewer's review of 452e1f7 asked for changes: VC4R-1 to VC4R-6, two of them
  halting the session). All are fixed on `viewchange`:

    | Repro | With the fix switched off | With the fix |
    |---|---|---|
    | `joinwedge` (VC4R-2, the reviewer's) | a join seen by only part of the session, plus a crash, wedges it; stuck at frame 114 | 5 admitted at 2.6 s, 9 removed at 5.6 s |
    | `slowref` (VC4R-3, the reviewer's) | fake references shield a slow member; stuck at frame 108 | the slow member removed at 7.4 s |

  * VC4R-1: a cap is justified by its polka counted in any prevote version a reader holds, so an
    equivocated prevote cannot get an honest capper voted out. VC4R-4: fetches by id retry with backoff.
    VC4R-5: a vote whose frozen head nobody delivers blocks no reset and removes its voter. VC4R-6: a join
    signature names the session and an expiry. VC3R-3's low: a listed admin entry must be the canonical,
    undisputed version.
  * **A reference now counts only once the entry it names is held**, both toward stability and against a
    withholder (VC4R-3). This is also what caught the generator's finding below.
* **A randomized Byzantine schedule generator** (`randsched`, coordinator's request). Each seed draws a
  membership of 4 to 7, a faulty set within `t`, a behaviour per faulty member (crash, delay, selective
  delivery, fake references, a climbing epoch, a withheld vote, a bogus cap, suspicion churn, a ghost
  vote), an optional partial-delivery joiner, and a GST, then checks the written invariants S1-S5 and L1
  (`spikes/out/session-sim/INVARIANTS.md`). 80 seeds hold every invariant on the round-5 protocol.
  * **It found RG-1**, a hole the four hand reviews had not: a faulty member that references a healthy
    member's chain a little beyond what it holds, so the fake hides inside normal lag, caps that chain's
    stability without being caught as a withholder. `randsched --fixes-off 0x800000` reproduces it
    (seed 1); the verified-reference rule fixes it.
  * **Coverage of the earlier rounds' holes, measured** by running the generator with each fix switched
    off: 40 seeds find VC4R-3/RG-1 at once, but do not reproduce VC4R-2 (`joinwedge` needs a crash timed
    to a particular prevote), VCR-12 or VC-5 within that budget, and the generator does not yet craft
    equivocated view-change votes (VC4R-1). So it complements the hand repros rather than replacing them
    yet; strengthening its join and equivocation draws is the next step.
* **Not done yet:** the port into `pe-session` (P2-3, @world-chunks) and real networks; removed members
  are not told; the departure half of a fork proof still records epoch 0, and VC-7 and VC-8 are open
  (22.7); VCR-10 and V4 wait for the user; the generator's join and equivocation coverage is to be
  widened.

## 6. Spike (d): WASM game-logic hot-swap, plus the MCP stub

### 6.1 Commands

```bash
bash scripts/build_wasm_guests.sh                 # logic guests and physics_in_wasm, local paths remapped (6.5)
cargo build -p pe-spike-wasm-hotswap --release
./target/release/pe-spike-wasm-hotswap            # d1-d4 (eager compilation)
./target/release/pe-spike-wasm-hotswap --lazy     # d-lazy (wasmi default compilation mode)
python -I scripts/time_guest_rebuild.py           # d5
sha256sum spikes/wasm-guests/target/wasm32-unknown-unknown/release/*.wasm target/wasm32-unknown-unknown/wasm-guest/physics_in_wasm.wasm
cargo build -p pe-spike-mcp-stub --release
./target/release/pe-spike-mcp-stub < spikes/mcp-stub/session.jsonl   # mcp
```

### 6.2 Setup

* Guests are `no_std` cdylibs: `logic_v1` (lifts objects every 45 frames), `logic_v2` (sideways pushes every
  20 frames, float "spin" state, migrates v1's state), `logic_runaway` (infinite loop), and
  `physics_in_wasm` (the whole spike-(a) reference run compiled to wasm32).
* Module sizes: 980 B, 1,143 B, 640 B and 497 KB (505 KB in phase 1, with upstream physics).
* Since integration, `physics_in_wasm` is a member of the root workspace (`spikes/physics-in-wasm`,
  profile `wasm-guest`), and the logic guests keep their own workspace (6.5).
* The host is wasmi 2.0 with `deterministic`, fuel on (1,000,000 per tick) and `CompilationMode::Eager`.
* Peer A knows the v1→v2 commit (activation frame 300) in advance. Peer B learns it, together with all
  interactions in frames 290-344, at frame 345. Peer C never learns it.

### 6.3 Results [V]

| ID | Check | Result |
|---|---|---|
| d1 | A and B per-frame hashes (physics + claims + guest state + module ID) | **Identical for 600 frames.** B made 1 rollback across the swap, re-simulating 45 frames in 31 ms. Swap step 0.8 ms. Snapshot 1.70 MB per frame (0.57 MB physics + 1.1 MB guest memory, mostly the default 1 MiB stack) |
| d2 | Control C (no commit) vs A | First mismatch **exactly at frame 300**, so the swap took effect |
| d1b | Fuel per frame on A vs B | Identical (46,800 total; max 83 per tick) |
| d3 | Runaway tick, run twice | Trapped "all fuel consumed" after about 0.18 ms; guest state at halt identical (`bde1c36627cfa6ca`) both times |
| d4 | Box3D pile compiled to wasm32, run in wasmi | Digest `80839f4a54133e13` (engine default; `7903109aab28ed9b` with upstream physics in phase 1) **== native**. 7.0 ms/frame interpreted, about 22x native. Module validate+compile 9.1 ms eager (2.8 ms lazy) (phase-1 timings; noisy reruns at low priority were slower) |
| d-lazy | Same as d1b/d3 with wasmi's default `LazyTranslation` | **FAIL:** fuel A 49,831 vs B 46,800; the runaway halts at different points (`79f77bfc…` vs `bde1c366…`). Cause, confirmed in wasmi source: lazy translation is charged as fuel on first use of each function, which depends on process history. **Engine must use eager compilation** |
| d5 | Edit→rebuild latency (one-constant change in `logic_v2`) | release (opt-level s, LTO, 1 CGU): 0.22-0.29 s, first edit 0.78-1.24 s; dev: 0.19-0.24 s. Rebuilding identical source gives an identical wasm (`logic_v2` sha256 `f9c5e895…`) |

### 6.4 MCP stub [V]

Ten JSON-RPC requests were piped through `pe-spike-mcp-stub` (`spikes/mcp-stub/session.jsonl` →
`spikes/out/mcp-session.out.jsonl`):

* `initialize` negotiated `2025-06-18`. `tools/list` returned 3 tools with JSON Schemas.
* `world_step` (60 frames) returned frame and state hash. `world_get_state` returned objects with
  positions, velocities and claims.
* `world_propose_interaction` was scheduled at the current frame. After `world_step` its verdict came back
  `Accepted`; it went through the same `RollbackSim::add_input` path as network input.
* Unknown object returned `isError` with `{"code":"unknown_object",…}`. A NaN impulse returned `isError`
  with `{"code":"invalid_argument",…}`.
* `resources/read primordial://world/state` returned 256 objects. An unknown method returned JSON-RPC
  `-32601`.
* **Not covered:** a real MCP client (e.g. Claude Code) connecting; rmcp itself (only audited).

### 6.5 Reproducible module identity (review M8) [V]

```bash
bash scripts/run_determinism.sh wasm    # builds at this checkout and at a copy elsewhere, then compares
python -I scripts/wasm_canonical_hash.py <wasm built at path 1> <wasm built at path 2>
```

* **Phase 1:** the whole source tree was copied to a second directory and the guests rebuilt there.
  `physics_in_wasm.wasm` had raw SHA-256 `589e97a7008badfd` (repo path) vs `051eee…` (other path), with
  only mangled names in the `name` section differing, and canonical hash `66633b435cac94ab` for both.
* **At integration this broke, in two ways [V]:**
  * the box3d fork is a path dependency, so panic locations embedded the absolute checkout path: 83
    copies of it in `physics_in_wasm.wasm`, and canonical hashes `3380e839…` and `4fdb6eb1…` at two
    worktrees, 168 bytes apart;
  * with paths remapped (`--remap-path-prefix`), the sizes matched but at some paths the type, function,
    element and code sections were permuted (356 kB of code bytes differed; data identical). Cargo hashes
    a path dependency's crate identity by its absolute path unless the dependency lies inside the
    workspace being built, and the linker's function order follows those hashes. Whether two paths
    happened to agree was luck.
* **Fix:** `scripts/build_wasm_guests.sh` remaps the checkout root and the cargo home (long Windows names:
  a temp directory's 8.3 short name did not match what rustc sees), and `physics_in_wasm` builds in the
  root workspace, which contains sim-core and the fork.
* **Result:** all four modules built at a second checkout path (a copy of the working tree in a temp
  directory) are byte-identical to this checkout's, raw hash included; `physics_in_wasm` is
  `4ee3b3bb0aa6ec8f` raw, `456e42657efc62ae` canonical. No drive-letter path remains in it.
* Output: `spikes/out/wasm-canonical-hash.txt`.

### 6.6 Iteration benchmark and logic runtimes (by @iter-bench) [V]

Full method, tables and options: [iteration-benchmark.md](iteration-benchmark.md). Raw logs:
`spikes/out/iter-bench/`. Headlines, all on this desktop:
* **Edit to first frame:** a one-function edit in a 6.4k-LOC ECS-style guest takes 0.45 s with the
  `rel-fast` profile (0.55 s dev, 1.3 s fat LTO); 1.2 s at 45k LOC.
* **Logic at 10k entities, relative to native (0.9 ms):**
  * wasmi 7.0x;
  * wasmtime Cranelift with fuel and NaN canonicalization 1.7x;
  * Winch with fuel and NaN canonicalization 3.0x.
* **Physics compiled to wasm:** 21x, 4.7x and 6.3x respectively.
* **Determinism:** the digest was identical in all 12 configs; eager fuel was identical per runtime and
  differs between runtimes.
* **Native code:** minimal wasmtime compiles no C on Windows; its build.rs compiles `helpers.c` on Linux
  and macOS [I].
* rustc's Cranelift backend cannot target wasm32.

## 7. Native-code audit (100%-Rust ruling)

### 7.1 Method

* `scripts/audit_native.py` asks `cargo tree -e normal,build --target <triple>` which packages would
  actually be built.
* It then classifies each from `cargo metadata` facts: build script; build-time dependency on cc, cmake,
  nasm-rs, cxx-build or bindgen; whether the package ships C/C++/asm files; its `links` key; Windows
  import-library crates.
* "COMPILES NATIVE CODE" means a native build helper plus shipped native sources. It can still be gated off
  inside `build.rs`, so each hit was checked by hand.
* `cargo metadata` alone over-reports, because it unifies features more broadly than the real resolver. An
  early run wrongly flagged `ring` under rustls-graviola until restricted to `cargo tree`.

```bash
python -I scripts/audit_native.py . --target x86_64-pc-windows-msvc --target x86_64-unknown-linux-gnu --md spikes/out/audit-spikes.md
python -I scripts/audit_native.py spikes/stack-audit/* --target x86_64-pc-windows-msvc --target x86_64-unknown-linux-gnu --md spikes/out/audit-stack.md
```

### 7.2 Results [V] (Windows x64 / Linux x64)

| Probe | Packages built | Verdict | Native items found |
|---|---|---|---|
| spike workspace (this repo) | 42 / 42 | **100% Rust** | none |
| gfx-wgpu (wgpu 30 + naga wgsl/spv/hlsl-out) | 76 / 63 | **100% Rust** | khronos-egl, wayland-sys: system libs (dlopen) |
| window-winit 0.30 | 14 / 74 | Win: **100% Rust**; Linux: **not** | Win: `windows_x86_64_msvc` import-lib stubs. Linux: `wayland-backend` compiles a C log shim [S build.rs]; x11-dl/wayland-sys system libs |
| vr-openxr-loaded (default `loaded`) | 5 / 5 | **100% Rust** | openxr-sys ships loader sources but does not build them; loader is dlopen'ed |
| vr-openxr-static | 7 / 7 | **not** | openxr-sys builds the C++ loader with CMake |
| net-quinn-default | 50 / 50 | **not** | ring (C + asm) |
| net-quinn-no-tls | 33 / 31 | **100% Rust** | none (bring your own crypto) |
| net-rustls-graviola | 12 / 13 | **100% Rust** | graviola uses Rust `asm!` (149 blocks) [S]: gray area |
| net-iroh 1.4 | 252 / 245 | **not** | ring; blake3 C/asm |
| net-libp2p 0.57 (+quic) | 243 / 240 | **not** | ring; aws-lc-sys (cmake, C/C++/asm) |
| net-snow-udp (snow default) | 53 / 54 | **not** | ring, force-enabled by snow's `std` feature (`ring/std`, not `ring?/std`) [S] |
| net-snow-nostd (`default-resolver-crypto` only) | 47 / 48 | **100% Rust** | none |
| logic-wasmi (deterministic) | 8 / 8 | **100% Rust** | none |
| logic-wasmtime 50 (default) | 147 / 146 | **not** | wasmtime, -fiber, -jit-debug (C), ittapi-sys, zstd-sys |
| mcp-rmcp 3.8 (server, stdio, macros, schemars) | 61 / 60 | **100% Rust** | none |
| ecs-bevy 0.20 / ecs-hecs 0.11 | 71 / 69, 4 / 4 | **100% Rust** | none |
| hash-blake3 default | 8 / 8 | **not** | C + asm via cc |
| hash-blake3 `pure` | 8 / 8 | **100% Rust** by hand check | cc is still a build dependency, but `build.rs` takes Rust-intrinsics paths when `pure` is set [S] |
| assets-pure (image png+jpeg, gltf, bson, lz4_flex, lzma-rs, brotli, ruzstd) | 81 / 82 | **100% Rust** | brotli-decompressor ships 1 C file, not built |
| assets-image-default | 94 / 94 | 100% Rust, but heavy | pulls rav1e (AV1 encoder); asm not built without its `asm`/nasm feature |
| audio-cpal 0.18 | 17 / 8 | **100% Rust** | Linux: alsa-sys links system libasound |
| discovery (mainline 8, mdns-sd 0.21, igd-next 0.18) | 98 / 97 | **100% Rust** | none |
| agent-panels (portable-pty 0.9, alacritty_terminal 0.26, vt100 0.18, agent-client-protocol 3.6, cosmic-text 0.19) | 135 / 128 | **100% Rust** | Win: `windows_x86_64_msvc` import-lib stubs; nix, signal-hook, ttf-parser ship C files that are not built |

Full tables: `spikes/out/audit-stack.md`, `spikes/out/audit-spikes.md`.

## 8. Resonite format check

* Resonite is installed (`C:\Program Files (x86)\Steam\steamapps\common\Resonite`). The local data folder
  holds a 2.5 GB `Data.litedb` and an empty `Assets` folder.
* There are **no** `.resonitepackage`, `.brson`/`.7zbson`/`.lz4bson` or `.meshx` files on the machine, so no
  header could be parsed without exporting an avatar from Resonite first (needs the app and the user's
  account) [V].
* Nothing from the user's Resonite data was copied into this repo.

## 8b. R13 and e4mc research (read, not run) [S]

* **Harness CLIs**, from `--help` of the installed versions:
  * **Claude Code 2.1.292:** `-p`, `--input-format text|stream-json`, `--output-format text|json|stream-json`,
    `--include-partial-messages`, `--mcp-config`, `--strict-mcp-config`, `--tools`, `--allowedTools`,
    `--disallowedTools`, `--permission-mode`, `--max-budget-usd`, `--add-dir`, `--append-system-prompt`,
    `--resume`.
  * **Codex CLI 0.160.0:** `exec --json`, `--output-schema`, `-s/--sandbox`, `-C/--cd`, `--ephemeral`,
    `-c key=value`, `mcp add`, and `app-server` (marked experimental, with `generate-json-schema`).
    **Correction:** `-a/--ask-for-approval` exists only on the interactive top-level command, not on `exec`,
    and offers only `on-request` and `never` (review R13-H1).
  * **Gemini CLI:** `--acp`, `-o stream-json`, `--allowed-mcp-server-names`.
  * **Correction:** Claude Code 2.1.292 is a native `claude.exe` shipped inside its npm package. Gemini CLI
    is a Node.js app. `codex.exe` is a native PE32+ x86-64 executable.
  * The restriction flags that design 21 now relies on (`--bare`, `--restricted`, `--tools ""`,
    `--setting-sources`, `--permission-prompts`, `--disable-slash-commands`, `--no-session-persistence`;
    Codex `--ignore-user-config`, `--ignore-rules`, `--disable`, `--strict-config`) were read from `--help`
    on this machine [S]. None has been run in a containment test yet (design 21.9).
* **e4mc** (`vgskye/e4mc-quiclime`, branch `kity`, Rust):
  * hosts connect outbound over QUIC (quinn + rustls, ALPN `quiclime`);
  * domains are `<word>-<word>.<QUICLIME_BASE_DOMAIN>` from a 7,776-word list (`routing.rs`);
  * vanilla TCP clients are routed by the Minecraft handshake's server address (`main.rs`);
  * an iroh "Dialtone" ticket is served at `/.well-known/dialtone_ticket/{domain}`.
* **iroh** (docs.rs): `EndpointId` is the Ed25519 public key; it connects via a relay, then hole-punches to
  direct; relays forward traffic they cannot decrypt; n0 relays are the default; DNS-based address lookup.
  The audit shows iroh is not 100% Rust (ring, blake3 C).
* **Orgtree mail hub** (Orgtree glossary and public repo docs; no hub contacted or registered):
  * HTTP; operator port 7370 with no login; relay-only door 7371 for authenticated client traffic; no TLS
    of its own;
  * person and org identities, with org addresses minted with an ownership secret;
  * clients register, poll and transfer files with at-least-once custody.
  * The wire format and authentication headers are not documented there [I].

## 9. Verified vs inferred ledger

Verified here [V]:
* box3d-rust golden match vs C on x86_64 Windows and Linux; cross-machine on an Intel laptop (run by
  @design-reviewer).
* The session protocol (5b-5e): ack-based confirmation with no local "too late" (C1); equivocation proof
  and majority eviction (C2); state + canonical frame with the acceptance criterion; gap-fetch repair;
  checks only at confirmed frames, carried in acks (H4); the arrival bound and horizon violations (H5);
  non-renewing claims with a cooldown (M3); the per-frame tie permutation; rollback through keyframes.
* The ball-pit bandwidth, rollback and CPU figures, with bit-identical end states.
* Two worlds in one process at different tick rates; the settled cross-world handoff with rollback on both
  sides; throttling without effect on results or on others' confirmation.
* Iteration-loop latency for realistic guests and runtime throughput (iteration-benchmark.md).
* Canonical wasm hash identical across checkout paths.
* Bit-identical traces across 7 builds.
* Snapshot/restore and rollback correctness, with costs.
* Box3D in WASM equals native.
* Serverless 2-peer convergence under delay, jitter and loss.
* Sync-check detection, earliest-frame localization and input-exchange repair.
* Signed datagrams.
* Clock-offset accuracy on loopback.
* WASM hot-swap with migration and rollback across the swap.
* Deterministic fuel (eager mode only).
* Runaway halting.
* Edit→rebuild latency for a small guest; same-source reproducible wasm on one machine.
* MCP stub protocol subset.
* Native-code audit verdicts.

Verified by reading [S]:
* Upstream Box3D determinism design and claims.
* wasmi NaN canonicalization and lazy-translation fuel charging.
* naga has no HLSL frontend.
* quinn/iroh TLS feature coupling to ring/aws-lc.
* snow's `std` → ring quirk.
* blake3 `pure` path.
* wayland-backend C shim.
* graviola inline asm.
* box3d-rust gaps: serial, scalar, pinned commit.
* Harness CLI flags, e4mc relay routing, iroh features and the Orgtree hub facts listed in section 8b.

Inferred [I] (design proposals, not yet tested):
* Determinism on ARM and macOS (upstream and port claim it).
* Keyframe/delta snapshot savings.
* Region partitioning.
* Re-sim amortization.
* Noise transport and own congestion control.
* NAT traversal through peers.
* Mainline DHT discovery.
* Join by state transfer across machines.
* Partition healing.
* ACL log and agent delegation.
* SCM ordering, finality, auto-merge and merge-seat mechanics.
* Own ECS store.
* Asset CAS and P2P transfer.
* OpenXR + wgpu Vulkan interop and multiview.
* Hand-engagement model.
* Resonite package and MeshX parsing beyond the documented bone-binding detail.
* Cross-machine reproducible wasm builds (one machine, two paths: verified).
* View changes with cutoffs, and joins by admission entry (only eviction after equivocation was run).
* The session protocol over real internet paths and with more than 3 members.
* Portal rendering, VR straddling, presence and avatars across worlds.
* All of R13's mechanics: panel hosting, guest-mode sandbox effectiveness, connection handshake, hub client
  protocol.
* Session codes with PAKE; volunteer relays.

## 10. Remaining risks and what phase 2 must verify

1. **ARM/macOS determinism:** x86-64 is verified on two CPUs (Zen 5, Intel Raptor Lake). Add an aarch64
   Linux and a macOS runner to a CI determinism matrix; run the goldens and the trace digest.
2. **Physics budget:** the serial port at 1-2k bodies plus deep rollbacks exceeds a 60/90 Hz frame. Verify
   re-sim amortization and region partitioning before committing to session scales (design D2).
3. **Snapshot memory:** about 2.4 MB/frame at 1024 bodies plus about 1.1 MB per logic module. Prototype
   keyframes, deltas and small guest stacks; measure.
4. **Transport:** build and soak-test UDP+Noise with real internet paths, NAT traversal, and 4-8 peers.
5. **OpenXR + wgpu interop:** a small headset spike (Vulkan `from_hal`, swapchain wrap, multiview).
6. **MCP:** connect a real MCP client to an rmcp-based server.
7. **Resonite:** export one avatar (package + any standard export) and parse container, data-tree header and
   MeshX, after the user rules on D19.
8. **Upstream drift:** box3d-rust pins upstream `3fc20f5` while upstream is at `33626332`; re-run goldens
   on every bump.
9. **R13:** drive Claude Code (`stream-json`) and Codex (`exec --json` / `app-server`) from a Rust adapter
   against an rmcp engine server; prove the guest-mode restrictions actually block shell and file tools.
10. **Mail hub:** with the user's go-ahead (design D32), read Orgtree's hub source and prototype a client
    against a private test hub.
11. **Session protocol at scale:** removing one silent, slow or equivocating member is built (5i). Still to
    build and verify: the proposer-based view change (several members at once, joins, per-epoch
    certificates), a named phase-3 prerequisite (design 23.2); 4-8 members over real internet paths; and
    partition heal. The simulator becomes a phase-2 test tool.
12. **Harness containment gates** (design 21.9) before any panel can be unlocked.
13. **Portals and multiple worlds:** rendering, VR straddling, and the memory and CPU budget of several
    attended worlds (design 22.3).

## 11. Reproduce everything

```bash
rustup target add wasm32-unknown-unknown x86_64-unknown-linux-musl --toolchain stable
cargo build --workspace && cargo build --workspace --release
bash scripts/build_wasm_guests.sh
bash scripts/run_determinism.sh                     # the integration determinism set (5j), low priority
./target/release/pe-spike-physics goldens && ./target/release/pe-spike-physics rollback && ./target/release/pe-spike-physics bench
./target/release/pe-spike-physics trace 600 spikes/out/trace-win-release-1.txt
bash scripts/run-p2p.sh latency60 "--delay-ms 60 --jitter-ms 20" "--delay-ms 60 --jitter-ms 20"   # also loss10, fault (5b)
bash scripts/run_session_sim.sh                     # every session, ball-pit, R14 and review scenario (5c-5e, 5g, 5i)
bash scripts/run_perturbation_history.sh            # re-sim vs depth, partial restore, snapshot knob, observer (5f)
bash scripts/run_r18.sh                             # ball pit through 256 to 8 kbit/s caps, a step, shared uplinks, a slow member (5g, 5i)
bash scripts/run_r15.sh                             # physics-hook call cost and frequency (5h); low priority
python -I scripts/wasm_canonical_hash.py spikes/wasm-guests/target/wasm32-unknown-unknown/release/*.wasm target/wasm32-unknown-unknown/wasm-guest/physics_in_wasm.wasm
# iteration benchmark: see docs/iteration-benchmark.md section 8
./target/release/pe-spike-wasm-hotswap
./target/release/pe-spike-mcp-stub < spikes/mcp-stub/session.jsonl
python -I scripts/audit_native.py spikes/stack-audit/* --target x86_64-pc-windows-msvc --target x86_64-unknown-linux-gnu
```
