# Iteration-latency and logic-runtime benchmark

Owner: iter-bench. Docket: @item:primordial-engine/iteration-latency-and-logic-runtime-benchmark-wa.
Measured 2026-10-30 on the desktop: Ryzen 7 9700X (16 threads), Windows 11, rustc 1.99.0 stable (cargo 1.99.0). Nightly 1.94.0 (2025-12-10) was used only for the Cranelift codegen backend.

Every number has a mark. **[V]** means verified: measured here, with the raw log named. **[I]** means inferred: reasoned from sources or put together from measured parts, and not measured as a whole. Raw logs are in `spikes/out/iter-bench/`, and `tables.md` there has every table this document draws from.

## 1. Headline

1. **[V] Hot-reloading a realistic logic edit takes about 0.45 s from edit to first frame.** The guest is 6.4k LOC; the build profile is opt-level 3 without LTO, 16 codegen units, incremental (the `rel-fast` profile). The time covers editing a function, building with cargo for wasm32, wasmi validate+compile, instantiate, state migration and the first frame. The cargo build is 0.45 s and the swap 6 ms. The phase-1 guess of 1-5 s was pessimistic for this size. (`loop-main.log`)
2. **[V] `rel-fast` builds as fast as `dev` and runs about 12x faster.** Unoptimized dev wasm in wasmi ticks at 77 ms at n=10k, against 6.4 ms for release. A plain dev build is the wrong profile for playtesting logic. (`runtime-matrix-dev.jsonl`)
3. **[V] Fat LTO (the shipping profile) is the only slow lever.** It takes 1.3 s at 6.4k LOC, 2.7 s at 17k and 5.9 s at 45k with a crate split; one crate takes 1.9 / 4.3 / 10.9 s. The final link with LTO is 1.09 s of the 1.31 s build.
4. **[V] Runtime speed on the same logic at n=10k, as a multiple of native time:**
   * wasmi (eager, fuel): 7.0x
   * wasmtime Cranelift (fuel, NaN canonicalization): 1.7x
   * Cranelift with fuel only: 1.4x
   * Cranelift without either: 1.1x
   * Winch with fuel: 2.8x
   * Winch with fuel and NaN canonicalization: 3.0x

   On the phase-1 physics in wasm: wasmi 21x, Cranelift with fuel and NaN canonicalization 4.7x, Cranelift without NaN canonicalization 2.4x, Winch with fuel and NaN canonicalization 6.3x (native 0.31 ms/frame, phase 1).
5. **[V] Determinism.** The final digest was identical in all 12 runtime configs and in two separate processes. Fuel was identical across instances and processes for every eager config. wasmi lazy translation reproduced phase-1 finding d-lazy (fuel differs between instances). NaN bits are canonical only in wasmi `deterministic`, and in Cranelift or Winch with `cranelift_nan_canonicalization(true)` (Winch honours the flag despite its name). Without the flag they are the CPU's.
6. **[V] 100% Rust.** Minimal wasmtime (no default features) compiles **no C on Windows**: its build script skipped `helpers.c`, and the build directory holds no object files. **[I]** On Linux, `wasmtime`'s build.rs compiles `helpers.c` whenever `runtime` is on. wasmi has 8 packages, all pure Rust.
7. **[V] rustc_codegen_cranelift cannot emit wasm32.** It fails with "Support for this target has not been implemented yet". For a native dev build it is 15% faster than LLVM (0.50 vs 0.59 s).

## 2. Method

**Guest** (`scripts/iter_bench_gen.py` generates `spikes/iter-bench/guests`). The code is ECS-style game logic over struct-of-arrays components:
* movement: 24 kinds with a terrain host call, plus separation through a spatial grid
* AI: state machines, 24 archetypes x 6 states
* combat: 16 damage types with resistances, crits and effects
* crafting: 64 recipes over an 8-slot inventory
* spawn waves: 16
* status effects: 24
* a core crate: Vec3 math, polynomial trig (no libm, because libm differs between native and wasm), a counting-sort spatial grid with queries, world storage, hashing and save/load

Systems take `&mut dyn Host` so that codegen stays in each system crate; with a generic it would be deferred to the final crate. The guest uses `std` (Vec) on wasm32-unknown-unknown.

The same text is emitted in four shapes:
* **split**: `pe_api` + 6 `sys_*` crates + `logic_game` + the `logic_wasm` cdylib; 6.4k LOC.
* **mono**: one crate, `logic_mono`.
* **module**: `mod_movement`, a per-system cdylib holding only pe_api + sys_movement. It is build-only, because its ABI differs.
* **native**: `logic_native`, a cdylib for x86_64 with a C function-pointer host table, as a native hot-reload dylib would have. It is build-only.

`--scale 3` gives 17k LOC and `--scale 8` gives 45k LOC (`guests-x3`, `guests-x8`).

Profiles in the guest workspace:
* `dev`: opt 0, debug info, incremental
* `dev-nodbg`: dev without debug info
* `dev-o1`: dev at opt-level 1
* `rel-fast`: O3, no LTO, CGU 16, incremental
* `release`: O3, fat LTO, CGU 1
* `rel-s`: release at opt-level s, as the phase-1 guests use

All profiles use panic=abort.

**Host** (`spikes/iter-bench/host`, `iter-host`). It has three runtimes:
* wasmi 2.0.0 (`std`, `validate`, `deterministic`; eager unless `lazy`)
* wasmtime 50.0.1, `default-features = false` + `std`, `runtime`, `cranelift`, `winch`, `parallel-compilation`; with or without fuel, epoch interruption and `cranelift_nan_canonicalization`, and serial or parallel compile
* native: the same logic crates linked into the host

The host provides `terrain_height(f32,f32)`, `emit(u32,u32,f32)` and `rand_u32()`. The comparable digest is the guest world hash plus the hash and count of host-visible emits.

Commands:
* `matrix`: compile (5 runs), instantiate (20), ticks at n=1k and n=10k (30 warm-up + 200 timed frames, median), 1M host calls, save/load of n=10k state (890 KB), determinism (2 instances x 300 frames at n=2000, digest every 10 frames and fuel every frame) and a NaN probe.
* `systems`: each system alone, by mask.
* `physics`: the phase-1 `physics_in_wasm.wasm`.
* `serve`: a live sim (n=2000) that on `swap <wasm>` reads, compiles, instantiates and migrates (save in the old instance, load in the new one), then runs the first frame.

**Loop driver** (`scripts/iter_bench_loop.py`). Per config:
1. One untimed warm build.
2. Three no-op builds: cargo's floor.
3. 8 timed edits (6 at scale 3 and 8) that toggle a constant inside one function. Each build sees a real change.

"Leaf" edits `sys_movement::move_kind_0`; "core" edits `pe_api::math::smoothstep`, so every dependent crate rebuilds. The loop time runs from just before the source write to the `serve` reply; the build is `cargo build -q --target wasm32-unknown-unknown --profile P -p PKG`. `rustc-timer` (a RUSTC_WRAPPER in `spikes/iter-bench/rustc-timer`) logs each rustc call's time and argv; the driver then replays those rustc commands without cargo.

All timing runs were made while holding the pe-bench-slot slot. Spreads are min-max over the reps.

## 3. Iteration loop

### 3.1 6.4k-LOC guest, leaf edit, wasmi swap [V] (`loop-main.log`, `loop-main.jsonl`)

| shape / profile | cargo build s, median [min-max] | swap ms (compile / first tick) | edit to first frame s |
|---|---|---|---|
| split dev | 0.491 [0.421-0.550] | 61.5 (6.1 / 11.9) | 0.554 |
| split dev-nodbg | 0.439 [0.404-0.518] | 50.3 (4.8 / 9.7) | 0.491 |
| split dev-o1 | 0.700 [0.468-0.833] | 8.5 (4.6 / 1.4) | 0.709 |
| **split rel-fast** | **0.446 [0.426-0.494]** | **6.2 (3.6 / 1.4)** | **0.453** |
| split release (fat LTO) | 1.287 [1.220-1.333] | 6.7 (4.3 / 1.1) | 1.294 |
| mono dev | 0.439 [0.421-0.636] | 59.3 | 0.499 |
| mono rel-fast | 0.441 [0.410-0.588] | 7.0 | 0.450 |
| mono release | 1.903 [1.855-2.834] | 6.9 | 1.910 |
| module (one system per wasm) dev / rel-fast / release | 0.408 / 0.430 / 0.719 | not run (build only) | - |
| split dev, core edit | 0.697 [0.668-1.098] | 58.6 | 0.755 |
| mono dev, core edit | 0.397 [0.379-0.437] | 58.4 | 0.456 |
| split rel-fast, core edit | 0.567 [0.523-0.624] | 6.2 | 0.573 |
| split release, core edit | 1.586 [1.506-1.658] | 6.8 | 1.593 |
| split rel-fast, Cranelift swap | 0.452 | 28.8 (27.8 / 0.4) | 0.480 |
| split release, Cranelift swap | 1.282 | 32.5 (31.7 / 0.4) | 1.315 |

* Swaps of dev builds take about 60 ms because the unoptimized guest's save/load (state migration) runs slowly in the interpreter. The first tick of dev code is 12 ms vs 1.1-1.4 ms when optimized (n=2000).
* The Cranelift runtime adds about 25 ms of JIT time to each swap. That is less than the build's spread.

### 3.2 Size scaling [V] (`loop-scale-x3.log`, `loop-scale-x8.log`)

Cargo build s (median), leaf edit:

| LOC (split crates) | split dev | split rel-fast | split release | mono dev | mono rel-fast | mono release |
|---|---|---|---|---|---|---|
| 6.4k | 0.491 | 0.446 | 1.287 | 0.439 | 0.441 | 1.903 |
| 17k | 0.721 | 0.640 | 2.745 | 0.529 | 0.693 | 4.346 |
| 45k | 0.756 | 1.176 | 5.907 | 1.006 | 1.426 | 10.894 |

The swap grows with module size: wasmi compile is 3.6 → 8.2 → 19.9 ms on rel-fast, and the swap total is 6 → 12 → 25 ms. At 45k LOC, edit to first frame is 1.20 s on split rel-fast and 0.86 s on split dev.

### 3.3 Levers

| lever | result | mark |
|---|---|---|
| Incremental off (CARGO_INCREMENTAL=0) | split dev 0.385 s (vs 0.491 on); mono dev 0.603 (vs 0.439); split rel-fast 0.443 (vs 0.446). Small crates do better without incremental; one big crate does better with it. | [V] `loop-levers.log` |
| codegen-units=1 in dev | split dev 0.377 s (vs 0.491) | [V] `loop-levers.log` |
| Crate split | Helps LTO builds (1.29 vs 1.90 s at 6.4k; 5.9 vs 10.9 s at 45k) and large dev builds (0.76 vs 1.01 s at 45k). No difference at 6.4k on dev or rel-fast. A core edit costs more when split (0.70 vs 0.49 s dev) because every system crate rebuilds. | [V] |
| One system per wasm module | 0.41 / 0.43 / 0.72 s (dev / rel-fast / release) vs split 0.49 / 0.45 / 1.29 s. Gains only when LTO is on. It needs a multi-module host ABI (shared state through the host), which this bench did not build. | [V] build; [I] ABI cost |
| opt-level and LTO | See 3.1. opt-level s vs 3 (release): wasmi 7.09 vs 6.39 ms/tick at 10k; Cranelift 1.90 vs 1.61. | [V] `runtime-matrix-rel-s.jsonl` |
| cargo overhead | A no-op `cargo build` takes 0.055-0.070 s, which is 12-15% of a 0.45 s loop. A no-op exe spawned from Python takes 5.4 ms median. | [V] `loop-*.jsonl` meta and summary |
| Direct rustc, no cargo | mono dev 0.404 s (cargo 0.499); mono release 1.813 (1.895); split release 1.250 (1.306); split rel-fast 0.442 (0.481); split dev 0.679 (cargo **0.494**). Replaying sequentially loses cargo's pipelining (sys_movement → logic_game → logic_wasm overlap). Saving: at most about 0.09 s. | [V] `loop-breakdown.log` |
| rustc time per crate, split dev leaf edit | sys_movement 138 ms, logic_game 111, logic_wasm 258 (link + final codegen). Fat-LTO release: logic_wasm 1092 of 1264 ms. | [V] `loop-breakdown.jsonl` |
| rustc_codegen_cranelift for wasm32 | Fails: "can't compile for wasm32-unknown-unknown: Support for this target has not been implemented yet". | [V] `loop-cranelift.jsonl` (config_error) |
| rustc_codegen_cranelift, native dev build (logic_native cdylib) | 0.503 s vs 0.593 (LLVM nightly) and 0.634 (LLVM stable). The native build is no faster than the wasm rel-fast build (0.446 s). | [V] `loop-cranelift.log` |
| sccache | Not measured; not installed here. sccache does not cache incremental crates, and every crate rebuilt after an edit has changed inputs (the edited crate) or a changed dependency hash (its dependents), so each would be a cache miss. Expected gain on the edit loop: none. It could only help cold builds of external dependencies. | [I] |
| Persistent compile daemon | No stable rustc daemon exists. The ceiling of avoiding cargo and process start is the 0.05-0.09 s above. | [I] |

Your three suspicions:
* "Cranelift cannot emit wasm32": confirmed [V].
* "sccache helps little": inferred [I], not measured.
* "Process spawn and cargo overhead are a visible share": partly confirmed [V]. Visible (about 60 ms, 12-15%), but not dominant.

## 4. Logic runtime

### 4.1 Throughput, release wasm (264 KB), median tick ms [V] (`runtime-matrix-release-run1/2.jsonl`; run1 / run2)

| runtime | n=1k | n=10k | x native (10k) | fuel/frame (10k) |
|---|---|---|---|---|
| native | 0.070 / 0.070 | 0.919 / 0.899 | 1.0 | - |
| wasmi eager + fuel (current) | 0.450 / 0.452 | 6.392 / 6.199 | 7.0 | 51,778,596 |
| wasmi eager, no fuel | 0.439 / 0.441 | 6.136 / 6.233 | 6.7 | - |
| wasmi lazy + fuel (nondeterministic fuel) | 0.459 / 0.458 | 6.103 / 6.026 | 6.6 | 51,778,596 |
| wasmtime Cranelift | 0.115 / 0.081 | 1.047 / 1.050 | 1.1 | - |
| Cranelift + fuel | 0.094 / 0.095 | 1.264 / 1.262 | 1.4 | 22,315,439 |
| Cranelift + fuel + NaN canon | 0.112 / 0.112 | 1.606 / 1.602 | 1.7 | 22,315,439 |
| Cranelift + epoch + NaN canon | 0.109 / 0.107 | 1.583 / 1.571 | 1.7 | - |
| wasmtime Winch | 0.137 / 0.135 | 1.844 / 1.823 | 2.0 | - |
| Winch + fuel | 0.209 / 0.205 | 2.588 / 2.607 | 2.8 | 22,234,497 |
| Winch + NaN canon (re-run) | 0.142 | 2.131 | 2.3 | - |
| Winch + fuel + NaN canon (re-run) | 0.209 | 2.762 | 3.0 | (as Winch + fuel) |

* The re-run rows come from `runtime-matrix-winch-nancanon.jsonl`, measured after the harness fix (section 7). In that same re-run, Winch + fuel measured 2.544 ms and Cranelift + fuel + NaN canon 1.572 ms, consistent with the rows above.
* Per-frame p10-p90 spread at 10k is about ±25%, because spawn waves and crafting vary by frame. Every runtime ran the same frames.
* At 45k LOC (1.75 MB wasm) and n=10k: wasmi 12.2 ms, Cranelift + fuel + NaN canon 2.96 ms, Winch + fuel 7.5 ms. That guest runs more code per entity, so these numbers are not comparable with the 6.4k rows. (`runtime-matrix-x8-release.jsonl`)

### 4.2 Compile, instantiate, host calls, migration [V] (run1)

| runtime | compile ms (264 KB) | compile ms (1.75 MB, 45k LOC) | instantiate ms | host call, no args, ns | host call (f32,f32)->f32, ns | save / load ms (890 KB state) |
|---|---|---|---|---|---|---|
| native | - | - | - | 1.1 | 7.9 | 0.32 / 0.35 |
| wasmi eager + fuel | 4.46 | 27.3 | 0.113 | 24.8 | 32.4 | 2.35 / 2.33 |
| wasmi lazy + fuel | 1.40 | 8.9 | 0.102 | 22.1 | 32.1 | 2.38 / 2.35 |
| Cranelift (parallel) | 19.3 | - | 0.018 | 2.6 | 11.8 | 0.21 / 0.53 |
| Cranelift + fuel + NaN canon (parallel) | 33.4 | 172.7 | 0.013 | 2.8 | 13.1 | 0.22 / 0.56 |
| same, serial compile | 199.2 | 1274.9 | 0.013 | 2.7 | 13.1 | 0.22 / 0.55 |
| Winch + fuel (parallel) | 4.49 | 24.5 | 0.014 | 3.1 | 13.5 | 0.89 / 0.88 |
| Winch + fuel, serial | 17.98 | - | 0.013 | 3.2 | 13.6 | 0.90 / 0.88 |

### 4.3 Determinism [V] (`runtime-matrix-release-run1/2.jsonl`, determinism rows)

* Final digest after 300 frames (n=2000): `443eadd3f2f82295` in **all 12 configs and in both processes**. The logic is cross-runtime deterministic, and native matches too.
* Fuel per frame was identical across two instances and across two processes for every eager config:
  * wasmi eager: total 2,401,062,442
  * Cranelift: 798,570,062
  * Winch: 791,732,053

  **Fuel is not comparable across runtimes.** All peers must run the same runtime and version.
* wasmi lazy + fuel: the two instances were **not** identical (total 2,401,067,174 in the first instance). This reproduces d-lazy; eager stays mandatory.
* NaN probe (`0/0 * 1.5` and `sqrt(-1.5)` bits):
  * wasmi `deterministic` and Cranelift with `nan_canonicalization` return canonical `7fc00000`.
  * Native, and Cranelift or Winch without canonicalization, return x86's `ffc00000` for sqrt.
  * Winch with `cranelift_nan_canonicalization(true)` returns canonical bits; the digest is unchanged and two instances are identical (`runtime-matrix-winch-nancanon.jsonl`).
  * [I] Without canonicalization, NaN bits can differ between x86-64 and ARM64 peers. That only matters if the logic observes NaN bits (to_bits, hashing a NaN, copysign) or stores NaNs in hashed state.

### 4.4 Where the time goes: per system, and the hybrid [V] parts, [I] sums (`systems-release.jsonl`)

Each system run alone, n=10k, median ms:

| system | native | wasmi | Cranelift + fuel + NaN | Winch + fuel |
|---|---|---|---|---|
| movement (incl. separation and terrain host calls) | 0.604 | 5.212 | 1.357 | 1.783 |
| ai | 0.148 | 0.821 | 0.223 | 0.444 |
| inventory / crafting | 0.149 | 1.169 | 0.254 | 0.851 |
| combat, spawn, effects | 0.092 | 1.184 | 0.178 | 0.361 |
| all | 0.928 | 6.879 | 1.716 | 2.782 |
| all but movement | 0.383 | 2.292 | 0.576 | 1.323 |

Hybrid: movement native in the engine core, the rules in wasm.
* With wasmi: 0.60 + 2.29 = **2.9 ms**, against 6.9 ms all in wasmi.
* With Cranelift: 0.60 + 0.58 = **1.2 ms**, against 1.7 ms.
* Not included: copying positions and velocities (240 KB at 10k) between the core and guest memory each frame. Full 890 KB state save/load took 0.2-2.3 ms, which suggests about 0.1-0.5 ms. [I]

### 4.5 Phase-1 physics in wasm (Box3D pile, 600 frames) [V] (`physics-runtimes.jsonl`; native 0.31 ms/frame from phase 1)

| runtime | ms/frame | x native |
|---|---|---|
| wasmi eager (+fuel) | 6.53 (6.63) | 21x |
| Cranelift | 0.71 | 2.3x |
| Cranelift + fuel | 0.75 | 2.4x |
| Cranelift + NaN canon | 1.41 | 4.6x |
| Cranelift + fuel + NaN canon | 1.46-1.54 | 4.7-5.0x |
| Winch (+fuel) | 1.42 (1.53) | 4.6x (4.9x) |
| Winch + NaN canon (+fuel) | 1.93 (1.96) | 6.2x (6.3x) |

* NaN canonicalization doubles the cost of float-heavy code on Cranelift. Fuel costs about 6% on physics and about 20% on logic.
* The digest matched native in every config.
* Compile times here are single samples (60-110 ms Cranelift, 9-10 ms wasmi/Winch).

## 5. Native code per runtime config [V] (`audit-runtimes.md`, `audit-runtimes.log`)

| config | packages (Windows) | flagged by audit_native.py | C actually compiled on Windows |
|---|---|---|---|
| wasmi 2.0.0 (std, validate, deterministic) | 8 | none: 100% Rust | none |
| wasmtime 50, `std`+`runtime`+`cranelift`, no defaults | 82 | `wasmtime` (cc build-dep + helpers.c) | **none** [V]: build output has no cc lines and no .lib/.o files in the host's whole build dir |
| wasmtime 50, `std`+`runtime`+`winch`, no defaults | 84 | `wasmtime` (same) | none [I] (same build.rs path) |
| bench host (cranelift+winch+parallel-compilation) | 103 | `wasmtime`; rayon-core (`links` key, no C) | none [V] |
| wasmtime 50 default features (phase-1 probe) | 147 | zstd-sys, ittapi-sys, wasmtime-fiber, wasmtime-jit-debug, wasmtime | yes (zstd, ittapi, fiber's windows.c, helpers.c) |

How wasmtime's build.rs decides, from its source [I]:
* `helpers.c` is compiled when `runtime` is on and either the OS is supported or `debug-builtins` is on.
* On Windows it returns early unless `debug-builtins` is on.
* On Linux and macOS it always compiles it (a source comment says: "TODO: should skip this on Unix targets as well but needs a solution for `wasmtime_using_libunwind`").

So under the 100% Rust ruling, minimal wasmtime is clean on Windows only. Linux would need an upstream change or a patched fork. Not verified on Linux: WSL has no Rust toolchain or C compiler installed.

[I] wasmtime JITs into executable memory. Platforms that forbid JIT (iOS, consoles) would still need an interpreter there (wasmi, or wasmtime's Pulley interpreter, not measured).

## 6. Options for "Decisions needing the user" (100% Rust ruling)

All loop numbers are for the 6.4k-LOC guest unless stated; "x native" is logic at n=10k.

| option | edit to first frame | logic speed | physics in wasm | determinism | native code | other costs |
|---|---|---|---|---|---|---|
| **A. wasmi (current), guests on `rel-fast` for iteration, `release` to ship** | 0.45 s (1.2 s at 45k LOC) [V] | 7.0x [V] | 21x [V] | Proven: eager fuel identical; canonical NaN [V] | 100% Rust, 8 crates [V] | Host call 25 ns. Swap 6 ms. Works where JIT is banned. |
| **B. wasmtime Cranelift + fuel + NaN canon** | 0.48 s (+25-30 ms JIT; +170 ms at 45k LOC) [V] | 1.7x [V] | 4.7x [V] | Fuel identical; canonical NaN; same digest as wasmi [V] | No C on Windows [V]; compiles helpers.c on Linux/macOS [I] | 82 crates. JIT-banned platforms need a fallback [I]. Parallel compile needs rayon threads (serial: 199 ms). |
| B'. as B without NaN canonicalization | same | 1.4x [V] | 2.4x [V] | NaN bits = CPU's; safe if logic never observes NaN bits, or all peers are x86-64 [I] | as B | Needs a rule or lint: no NaN in hashed or synced state. |
| **C. wasmtime Winch + fuel + NaN canon** | 0.45 s (compile 4.9 ms, like wasmi) [V] | 3.0x (2.8x without NaN canon) [V] | 6.3x (4.9x without) [V] | Fuel identical; canonical NaN [V] | as B | Same platform caveats as B. Its gap to Cranelift grows on big modules: 7.5 vs 2.96 ms at 45k LOC [V]. |
| **D. Hybrid: engine-native hot systems (movement, physics) + wasm rules** | Unchanged for the rules; a native system edit needs an engine rebuild (not measured) | wasmi 2.9 ms vs 6.9 (2.4x faster); Cranelift 1.2 vs 1.7 [I: sums of measured parts] | physics native | Native float code must stay deterministic (it does here: same digest) [V] | Engine Rust only | Native systems cannot be hot-swapped mid-match; the core/guest data copy is not measured. Can be combined with A, B or C. |
| E. Native dev loop (logic as a native dylib in dev, wasm when shipping) | Build 0.50 s (Cranelift backend) / 0.59-0.63 s (LLVM) [V]: **no faster than wasm rel-fast at 0.45 s** | native in dev only | - | Dev and shipping would run different code paths | as A | No iteration gain. Only a debugger-convenience argument. Not recommended for speed. |

Structure findings for any option [V]:
* Use `rel-fast` (opt 3, no LTO, CGU 16, incremental) as the iteration profile. Use fat LTO only for shipping builds.
* Split logic into crates once it passes about 15-20k LOC. At 45k LOC split rel-fast is 1.18 s vs mono 1.43 s, and split release is 5.9 s vs mono 10.9 s.
* Keep the core crate (shared types and math) stable. A core edit rebuilds every system: 0.57 vs 0.45 s on rel-fast.
* Pass the host as `&mut dyn Host`, not a generic, so codegen stays in each crate.

## 7. Dead ends, caveats, open threads

* About 2.5 h were lost to an account usage limit (about 10:05 to 12:31 UTC). The timebox was missed for that reason.
* Correction made during the write-up: the first matrix never asked Winch for NaN canonicalization. The harness skipped the setting because it is named `cranelift_nan_canonicalization`, but winch-codegen honours it (`maybe_canonicalize_nan` in `isa/x64/masm.rs`). The Winch-with-canonicalization rows in section 4 come from a re-run after that fix (`runtime-matrix-winch-nancanon.jsonl`).
* The first breakdown replay ran rustc in log order, which is completion order. Cargo pipelines rustc processes, so replays failed for rel-fast and release; the `loop-levers.jsonl` replay rows show `ok: false`. Fixed (dependency order) and re-run as `loop-breakdown.*`; all replays there succeeded. Use the `loop-breakdown` numbers.
* The hybrid row is a composition of isolated system runs; running one system alone changes the sim dynamics a little. The data-sync cost was not measured.
* sccache and a compile daemon: not measured (section 3.3).
* Linux native-code status of minimal wasmtime: not built here (no Rust or C toolchain in WSL).
* Winch on the 45k-LOC guest was 7.5 ms vs Cranelift's 2.96 ms. Winch's gap grows with code size and complexity.
* Not covered: multi-module guests (one wasm per system) at run time; wasmtime's Pulley interpreter; wasm SIMD; startup on the Intel laptop.
* The edit is a constant change inside one function. Larger edits (new types, signature changes) make more crates rebuild, but the build times above are dominated by the final crate's codegen and link, so they should scale like the core-edit rows. [I]

## 8. Reproduce

From the repo root:

```
python -I scripts/iter_bench_gen.py                      # spikes/iter-bench/guests (also --scale 3 --out spikes/iter-bench/guests-x3, --scale 8 ... guests-x8)
set CARGO_TARGET_DIR=spikes/iter-bench/target/host  &&  cargo build --release --manifest-path spikes/iter-bench/host/Cargo.toml
set CARGO_TARGET_DIR=spikes/iter-bench/target/tools &&  cargo build --release --manifest-path spikes/iter-bench/rustc-timer/Cargo.toml
python -I scripts/iter_bench_loop.py --set main|levers|breakdown|cranelift   [--reps 8]
python -I scripts/iter_bench_loop.py --set scale --reps 6 --guests spikes/iter-bench/guests-x3 --tag x3
iter-host matrix  <target/guests/wasm32-unknown-unknown/release/logic_wasm.wasm> native wasmi-eager-fuel wasmtime-cranelift-fuel-nancanon wasmtime-winch-fuel ...
iter-host systems <same wasm> native wasmi-eager-fuel wasmtime-cranelift-fuel-nancanon wasmtime-winch-fuel
iter-host physics spikes/wasm-guests/target/wasm32-unknown-unknown/release/physics_in_wasm.wasm <runtimes>
python -I scripts/audit_native.py spikes/iter-bench/audit/wasmi spikes/iter-bench/audit/wasmtime-min-cranelift spikes/iter-bench/audit/wasmtime-min-winch spikes/iter-bench/host --target x86_64-pc-windows-msvc --target x86_64-unknown-linux-gnu --md spikes/out/iter-bench/audit-runtimes.md
python -I scripts/iter_bench_report.py > spikes/out/iter-bench/tables.md
```

The Cranelift set needs `rustup component add rustc-codegen-cranelift-preview --toolchain nightly` and `rustup target add wasm32-unknown-unknown --toolchain nightly`; both were installed on this desktop for this bench. The wasm hashes used are in `spikes/out/iter-bench/wasm-sha256.txt`.

Committing: `spikes/iter-bench/guests-x3` and `guests-x8` are regenerable with the generator and are large (about 18k and 45k lines each, twice over), so committing them is optional. The `Cargo.lock` files in `host/`, `guests*/`, `rustc-timer/` and `audit/` pin the measured versions.
