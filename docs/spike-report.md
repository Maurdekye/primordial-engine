# Primordial Engine: Phase-1 Spike Report

Date: 2026-10-30. Author: engine-architect. Companion: [design.md](design.md).

Tags: **[V]** verified by running it here; **[S]** verified by reading source or docs; **[I]** inferred.
Test IDs (a1, b2, …) are the ones design.md cites. All raw outputs are committed under `spikes/out/`.

## 1. Summary

| Spike | Question | Result |
|---|---|---|
| a | Is Box3D usable from Rust, and deterministic across runs and builds? | **Yes [V].** box3d-rust 0.5.0 (pure Rust, no FFI needed) matches all four Box3D C-reference goldens. A 600-frame, 256-body trace with interactions is bit-identical across 7 builds, including Windows-MSVC vs Linux-musl and `target-cpu=native`. ARM and macOS were not tested |
| b | Fixed-step rollback: snapshot, step, restore, re-sim with matching hashes? | **Yes [V].** In-place and fresh-world restores replay bit-identically. Late interactions (up to 60 frames) roll back and converge exactly to the reference |
| c | P2P transport between two processes with no server? | **Yes [V].** UDP + Ed25519-signed, frame-stamped interactions; both peers' 600-frame histories equal the offline reference under latency, jitter, 10% loss, and a deliberately lost interaction that sync checks detected and repaired |
| d | Hot-swap Rust→WASM game logic mid-sim, deterministic across peers? | **Yes [V],** with wasmi in eager-compilation mode. The default lazy mode makes fuel nondeterministic [V]. Box3D itself compiled to WASM matches native [V] |
| mcp | Cheap MCP stub that reads world state? | **Yes [V]** (protocol subset, hand-written, not tested with a third-party client) |
| audit | Can the stack be 100% Rust? | **Mostly [V].** Graphics, VR (loaded loader), ECS, MCP, WASM (wasmi), assets, discovery and a UDP+Noise transport audit clean. iroh, libp2p+QUIC, quinn's TLS, wasmtime default and blake3 default compile C/asm |
| resonite | Parse one Resonite package/record header? | **Not reachable [V]:** no exported package or data-tree file exists on this machine (only a 2.5 GB LiteDB cache) |

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

### 5.3 Results [V]

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

## 6. Spike (d): WASM game-logic hot-swap, plus the MCP stub

### 6.1 Commands

```bash
(cd spikes/wasm-guests && cargo build --release --target wasm32-unknown-unknown)
cargo build -p pe-spike-wasm-hotswap --release
./target/release/pe-spike-wasm-hotswap            # d1-d4 (eager compilation)
./target/release/pe-spike-wasm-hotswap --lazy     # d-lazy (wasmi default compilation mode)
python -I scripts/time_guest_rebuild.py           # d5
(cd spikes/wasm-guests && sha256sum target/wasm32-unknown-unknown/release/*.wasm)
cargo build -p pe-spike-mcp-stub --release
./target/release/pe-spike-mcp-stub < spikes/mcp-stub/session.jsonl   # mcp
```

### 6.2 Setup

* Guests are `no_std` cdylibs: `logic_v1` (lifts objects every 45 frames), `logic_v2` (sideways pushes every
  20 frames, float "spin" state, migrates v1's state), `logic_runaway` (infinite loop), and
  `physics_in_wasm` (the whole spike-(a) reference run compiled to wasm32).
* Module sizes: 980 B, 1,143 B, 640 B and 505 KB.
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
| d4 | Box3D pile compiled to wasm32, run in wasmi | Digest `7903109aab28ed9b` **== native**. 7.0 ms/frame interpreted, about 22x native. Module validate+compile 9.1 ms eager (2.8 ms lazy) |
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

Full tables: `spikes/out/audit-stack.md`, `spikes/out/audit-spikes.md`.

## 8. Resonite format check

* Resonite is installed (`C:\Program Files (x86)\Steam\steamapps\common\Resonite`). The local data folder
  holds a 2.5 GB `Data.litedb` and an empty `Assets` folder.
* There are **no** `.resonitepackage`, `.brson`/`.7zbson`/`.lz4bson` or `.meshx` files on the machine, so no
  header could be parsed without exporting an avatar from Resonite first (needs the app and the user's
  account) [V].
* A note in the install folder, `bone_index_serialization.md`, documents MeshX bone bindings: four LEB128
  varints of sign-extended indices, then four f32 weights. It was derived from decompiled Resonite DLLs,
  which is a ToS question (design D19).
* Nothing from the user's Resonite data was copied into this repo.

## 9. Verified vs inferred ledger

Verified here [V]:
* box3d-rust golden match vs C on x86_64 Windows and Linux.
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
* Realistic logic-crate compile times (1-5 s).
* Cross-machine reproducible wasm builds.

## 10. Remaining risks and what phase 2 must verify

1. **ARM/macOS determinism:** add an aarch64 Linux and a macOS runner to a CI determinism matrix; run the
   goldens and the trace digest.
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

## 11. Reproduce everything

```bash
rustup target add wasm32-unknown-unknown x86_64-unknown-linux-musl --toolchain stable
cargo build --workspace && cargo build --workspace --release
(cd spikes/wasm-guests && cargo build --release --target wasm32-unknown-unknown)
./target/release/pe-spike-physics goldens && ./target/release/pe-spike-physics rollback && ./target/release/pe-spike-physics bench
./target/release/pe-spike-physics trace 600 spikes/out/trace-win-release-1.txt
bash scripts/run-p2p.sh latency60 "--delay-ms 60 --jitter-ms 20" "--delay-ms 60 --jitter-ms 20"
./target/release/pe-spike-wasm-hotswap
./target/release/pe-spike-mcp-stub < spikes/mcp-stub/session.jsonl
python -I scripts/audit_native.py spikes/stack-audit/* --target x86_64-pc-windows-msvc --target x86_64-unknown-linux-gnu
```
