# D2: parallel Box3D in our fork, bit-exact across thread counts

> **Evidence paths** (since the M0 archive, 2026-11-10): paths under `spikes/out/` in this document refer to the
> tag `archive/v0-prototype`, where those logs are kept. Main no longer carries them; the physics records the
> tests read moved to `crates/pe-physics/tests/records/`.

Docket: @item:primordial-engine/parallel-box3d-in-our-fork-bit-exact-across-thre. User ruling
2026-10-30 18:44Z: "goal is parallel". Branch `physics/parallel` from main `d79f160`.

Labels: **[V]** verified by a run or by reading the code named, **[I]** inferred.

## 1. Summary

**Feasible, and done for the stages that cost the most.** Our fork now runs pair finding, the
narrow phase, contact prepare and every solver stage on worker threads it owns (pure Rust, std
only), with results **bit-identical at every thread count and across machines**.

* **Determinism [V]:** on the desktop (AMD Ryzen 7 9700X) and on an Intel laptop (i7-13620H), the
  4 goldens, the 600-frame trace, the snapshot-image bytes and rollback T1-T5, in both engine
  and upstream modes, are byte-identical to the committed single-thread outputs at 1, 2, 3, 4,
  8, 16 and 32 threads (the debug build too, at 1-16 threads), and wasm32 still gives the
  native digest. 564 + 2 crate tests pass, including new ones that change the worker count
  partway through a run.
* **Speed [V, desktop clean window]:** at 8 threads a step costs 2.27 ms at 4,000 awake bodies
  (9.14 ms on 1 thread, 4.0x), 1.08 ms at 2,000 (2.6x), 0.65 ms at 1,000 (2.4x) and 0.33 ms
  at 500 (2.1x).
* **Rollback [V]:** restore plus 6 re-simulated frames at 8 threads costs 4.5 ms at 1,000
  bodies (8.7 ms serial) and 15.3 ms at 4,000 (55.9 ms serial). With about 5 ms for a 6-frame
  rollback, the budget moves from about 500 to about 1,000 moving bodies on this PC
  [I, from those two points].
* **Not parallel yet:** finalize bodies and CCD, bullets, sensors and island sleep, which are
  serial and about 0.3 ms at 4,000 bodies. Joints and overflow constraints stay on worker 0.
  The solver scales only about 3x because of its roughly 100 stage barriers per step. Both are
  the next levers (section 7).
* **Two concurrency bugs** were found by the matrix runs and fixed, with regression tests
  (section 5.3).

## 2. What upstream Box3D C runs in parallel, and why it stays deterministic [V, read at efca0c1]

Box3D C has no threads of its own; the application supplies `enqueueTask`/`finishTask` (or
uses the scheduler in `scheduler.c`). It parallelises:

| Stage | C entry point | How the result stays independent of the thread count |
|---|---|---|
| Tree rebuild | `b3UpdateTreesTask` (a task, overlapped with the narrow phase) | one task |
| Pair finding | `b3ParallelFor(b3CrossPairsTask / b3SelfPairsTask)` | per-worker key arrays, then **sorted** before contacts are created |
| Narrow phase | `b3ParallelFor(b3CollideTask)` | each contact writes only itself; per-worker state bit sets are ORed |
| Sensors | `b3ParallelFor(b3SensorTask)` | per-sensor output |
| Island split | `b3SplitIslandTask` (overlapped with the solver) | one task |
| Solver | `b3SolverTask` on every worker: worker 0 drives stages (prepare, integrate, warm start, solve, relax, restitution, store); each stage is an array of blocks claimed by CAS | **graph coloring**: within a color no two constraints share a dynamic body, so any split into blocks and any order gives the same bits; overflow constraints run on worker 0 only |
| Finalize + CCD | `b3ParallelFor(b3FinalizeBodiesTask)` | per-body output; tree enlargement deferred through per-worker bit sets |
| Bullets | `b3ParallelFor(b3BulletBodyTask)` | per-bullet output |

## 3. What box3d-rust 0.5 lacked [V]

Every stage ran serially. `parallel_for` ran all blocks on worker 0, `worker_count` was always
1, and the narrow phase and solver took `&mut World` throughout, so no stage could be handed to
another thread without restructuring the borrows.

## 4. What the fork does now

`WorldDef::worker_count` (default 1, clamped to 1..32) gives a world its own worker threads
(`src/worker_pool.rs`, std threads and atomics, no dependencies, no C). Idle workers spin
briefly, yield, then sleep on a condition variable, so a world that is not stepping costs no
CPU. On `wasm32` no threads are spawned and every stage runs serially, with the same results.

| Stage | Parallel in the fork? | Notes |
|---|---|---|
| Pair finding | yes | the fork uses C's single-worker block partition at every worker count; per-worker key arrays, sorted. With a custom filter set (it receives `&World`, which is not `Sync`) it stays serial |
| Narrow phase | yes | blocks of 32 contacts; each update borrows only its contact (`CollideEnv` split from `World`), which also removed two `Shape` clones (a heap allocation each) per contact per step |
| Contact prepare | yes | all colors' wide constraints in one stage, written into preallocated slots |
| Integrate velocities / positions | yes | blocks of 128 bodies |
| Warm start, solve, relax, restitution, store | yes, per color | blocks of 8 wide constraints (32 contacts); hit-event bit sets per worker, ORed |
| Joints, overflow constraints | worker 0 | in the serial order, between color stages (C runs joint blocks in the color stages; keeping them on worker 0 sidesteps motor-joint warm starts writing kinematic states) |
| Finalize bodies + CCD, bullets, sensors, island sleep, tree refit | serial | finalize mutates the broad-phase tree inline; C defers that through per-worker bit sets. Future work |

The solver stages follow C's `b3SolverTask` design (`src/solver/stages.rs`). Worker 0 runs the
same driver code as the serial path (`solve_stages` in `src/solver/solve.rs`) and publishes
each parallel stage; the other workers spin waiting for it, claim blocks with a
compare-exchange on a word tagged with the stage number, and go back to waiting. A stage ends
when all its items are done, so the next stage sees every write. The serial path runs each
stage as one block on the caller: same code, same arithmetic.

Shared data is reached through raw views (`StatesView`, `WideView`, `SharedMut`,
`SharedSlice`), so no `&mut` to shared data is formed; each user documents why its blocks are
disjoint.

## 5. Determinism

### 5.1 Why the result cannot depend on the thread count [V, by construction]

Every parallel stage has the same shape: its items are independent, each output slot is
written by exactly one block, and per-worker partial results are combined in a fixed order or
by an order-independent operation.

| Stage | Item | Writes | Combined by |
|---|---|---|---|
| Pair finding | a block of moved proxies / seeds | the worker's key array | concatenate, then **sort** (as C) |
| Narrow phase | one contact | that contact; the worker's counters and state bit set | integer sums; bit-set OR; mesh-color manifold counts applied after, one slot each |
| Prepare | one wide constraint | its preallocated slot; normal velocities of its own lane contacts | nothing to combine |
| Integrate velocities / positions | one body | that body's state | nothing |
| Warm start / solve / relax / restitution | one wide constraint of a color | the dynamic bodies of its lanes (velocities) and its own manifolds | graph coloring: no two constraints of a color share a dynamic body; non-dynamic states are never written by contacts |
| Store | one wide constraint | its lane contacts; the worker's hit-event bit set | bit-set OR |

Joints and the overflow constraints run on worker 0 between stages in the serial order. The
serial path (1 worker, or wasm32) runs the same code with each stage as one block, so it is
not a separate implementation that could drift.

Two counters changed: `sat_call_count` and `sat_cache_hit_count` are now summed over workers
and then scaled by `satMultiplier`. C scales only worker 0's share, which would make the
counters (not the simulation) depend on the thread count.

### 5.2 Evidence [V]

| Check | Desktop, Ryzen 7 9700X | Laptop, i7-13620H |
|---|---|---|
| Goldens (4 engine, 4 C) at 1/2/3/4/8/16/32 threads | all PASS | all PASS |
| 600-frame trace vs committed `trace-{vdw,off}-win-release.txt` | identical at every count (digests `80839f4a54133e13`, `7903109aab28ed9b`) | identical |
| Snapshot-image bytes vs committed `images-*-win-release.txt` | identical | identical |
| Rollback T1-T5 | PASS at every count | PASS at every count |
| par-bench final world hash, 500-4,000 bodies, 1-16 threads | identical across thread counts | identical, and equal to the desktop's |
| Crate tests | 561 + 2 pass, release and debug | 561 + 2 pass, release |
| Debug build: goldens, trace, images, rollback at 1/2/4/8/16 threads vs committed `*-win-debug.txt` | all identical / PASS | not run |
| wasm32 + wasmi (`wasm-hotswap` D4; no threads, serial path) | `80839f4a54133e13` == native | not run |

Raw outputs in `spikes/out/par/`: `determinism-win-desktop-{release,debug}.txt`,
`determinism-laptop-release.txt`, `crate-tests-win-desktop-*.log`, `wasm-d4-win-desktop.log`,
`scaling-*.txt`, `machine-*.txt`. Both machines built with
rustc 1.99.0 from a fresh target directory.

### 5.3 Bugs the matrix found

1. **A late-starting worker missed the first job (fixed in 3cbc619).** Workers took the
   current job epoch as already seen when their thread started, so a job published before a
   slow thread got going was never run by it, and `run` waited forever (seen at 16 threads
   under load). Workers now start from epoch 0. Test `first_job_reaches_every_worker`.
2. **A stale worker could claim another stage's items (fixed in 7072429).** A worker that had
   seen stage *s* could read stage *s+1*'s item count before worker 0 reset the claim word,
   still tagged *s*, and claim "leftover" items against the new count. The stage's done count
   then never matched and the step hung (the laptop, 4 threads). Worker 0 now closes the claim
   word before publishing a stage's parameters (Release stores, Acquire loads). The pre-fix
   code hung 1 of 3 runs of the stage stress test; the hardened test
   (`every_item_once_per_stage`: 20,000 alternating small and large stages at 2/4/8/16
   workers) passes.

### 5.4 Review of 73eaedc (@chunks-reviewer) and what changed

| Finding | Fix |
|---|---|
| P1, required: a panic in a helper's stage block hung the step (its items never reached `done`) | `StageSync::work` runs each block under `catch_unwind`, always counts its items, and sets a flag; worker 0 re-raises the panic when the stage ends. Test `panic_in_a_stage_block_is_re_raised` (worker 0 and helper cases) [V] |
| P2, required: the floating-point control register on every worker (4.3 rule 9) | Each helper adopts the **calling thread's** register (MXCSR on x86 with the sticky flags masked, FPCR on aarch64) at the start of every job. Parallel then equals serial even if the caller's register is not the canonical one; the engine's rule-9 check (`pe_core::fpenv::check_and_reset`, on the sim thread each tick) keeps the caller canonical, so every worker is canonical too. Resetting helpers to the canonical value instead would make a non-canonical caller's blocks round differently from its helpers'. Test `helpers_use_the_callers_fp_control` (flush-to-zero, denormals-are-zero, round-down) [V] |
| Non-blocking: a snapshot taken at frame N, restored at frame M | Test `snapshot_restores_across_worker_counts`: saved at 60 with 1 worker, restored into a 4-worker world that ran on to 100 and into a fresh 3-worker world; both continue like the 1-worker run [V] |
| Non-blocking: `$exe` unquoted in `check_par_determinism.sh` | Quoted |
| Non-blocking: `rb6/f` read like a per-frame value | The par-bench column is now `rb+6`, "restore + 6 frames, ms per rollback" |
| Non-blocking: `wait_until` never sleeps | Kept: it waits only inside a solve, between stages (finding 7.1); idle pools sleep |

After the fixes: crate tests 564 + 2 pass, and the release matrix (goldens, trace, images, rollback, both modes) is identical at 1/2/3/4/8/16/32 threads on the desktop [V].

## 6. Performance

Scenes: the engine pile (boxes, spheres, capsules; `build_pile_scene_def`) at 10x10x5,
10x10x10, 20x20x5 and 20x20x10 bodies, with **sleep off** so every body stays awake (the worst
case for the budget), engine world definition, 60 Hz, 4 sub-steps, 60 warm-up frames then 240
measured. "Rollback" is restore of a snapshot plus 6 re-simulated frames. Medians of 3
repeats, ms. Built with `--features stage-profile` (stage timers on).

### 6.1 Desktop: the user's PC, clean window [V]

AMD Ryzen 7 9700X (8 cores, 16 threads), Windows 11 Pro, rustc 1.99.0, normal priority,
2026-10-30 21:41Z. One idle Unity editor and agents' non-build processes were running; no
builds.

Step (ms per frame):

| Bodies | 1 thr | 2 thr | 4 thr | 8 thr | 16 thr | Best speed-up |
|---|---|---|---|---|---|---|
| 500 | 0.69 | 0.48 | 0.34 | 0.33 | 0.48 | 2.1x at 8 |
| 1000 | 1.55 | 1.09 | 0.73 | 0.65 | 0.85 | 2.4x at 8 |
| 2000 | 2.81 | 1.85 | 1.18 | 1.08 | 1.13 | 2.6x at 8 |
| 4000 | 9.14 | 5.56 | 3.04 | 2.27 | 2.17 | 4.2x at 16 |

Rollback, restore + 6 frames (ms):

| Bodies | 1 thr | 2 thr | 4 thr | 8 thr | 16 thr |
|---|---|---|---|---|---|
| 500 | 4.7 | 3.7 | 2.9 | 2.7 | 3.9 |
| 1000 | 8.7 | 6.9 | 5.4 | 4.5 | 6.2 |
| 2000 | 20.0 | 11.7 | 8.1 | 7.4 | 9.0 |
| 4000 | 55.9 | 31.0 | 21.4 | 15.3 | 16.3 |

### 6.2 Intel i7-13620H laptop [V]

Intel i7-13620H (6 performance + 4 efficiency cores, 16 threads), Windows 11 Home, unloaded.
Its 2,000- and 4,000-body times are about 4x the desktop's while 500 and 1,000 are about 1.2x;
laptop power or thermal limits are the likely cause [I, not investigated], so treat its
absolute numbers with care. The determinism results are unaffected.

Step (ms per frame):

| Bodies | 1 thr | 2 thr | 4 thr | 8 thr | 16 thr | Best speed-up |
|---|---|---|---|---|---|---|
| 500 | 0.82 | 0.67 | 0.51 | 0.48 | 0.58 | 1.7x at 8 |
| 1000 | 1.84 | 1.35 | 0.99 | 0.85 | 0.88 | 2.2x at 8 |
| 2000 | 12.07 | 8.23 | 5.88 | 5.51 | 6.82 | 2.2x at 8 |
| 4000 | 34.29 | 20.64 | 14.56 | 14.29 | 14.45 | 2.4x at 8 |

Rollback, restore + 6 frames (ms):

| Bodies | 1 thr | 2 thr | 4 thr | 8 thr | 16 thr |
|---|---|---|---|---|---|
| 500 | 5.6 | 5.0 | 4.1 | 3.9 | 4.6 |
| 1000 | 10.3 | 8.9 | 6.9 | 6.4 | 6.6 |
| 2000 | 69.3 | 50.6 | 35.8 | 42.7 | 43.9 |
| 4000 | 176.1 | 121.3 | 89.8 | 83.4 | 87.5 |

### 6.3 Where the time goes at 4,000 bodies, desktop [V]

| Threads | Pairs | Narrow phase | Solver (all stages) | Finalize (serial) |
|---|---|---|---|---|
| 1 | 0.72 | 4.03 | 4.08 | 0.28 |
| 8 | 0.29 | 0.54 | 1.25 | 0.27 |

(rep 1.) The narrow phase scales almost linearly (7.5x at 8 threads). The solver scales 3.3x:
it ends every color stage with a barrier, about 100 of them per step at 4 sub-steps, and small
colors run inline on worker 0. Finalize is still serial.

### 6.4 The earlier single-thread profile was taken under load

The profile at the start of the study (16.5 ms at 4,000 bodies) was taken while other agents
were building; the clean single-thread number is 9.1 ms. Its split (narrow phase 48%, solver
46%, pairs 6%) agrees with the clean one.

## 7. Findings and risks

1. **Physics workers must not run below other game work.** Spin-barriers plus OS preemption
   stall the step: if a worker is descheduled while it holds a block, every other worker waits
   for it. Under load at idle priority a 2-thread run took about 15 minutes instead of seconds
   (Windows starves idle-priority threads for up to seconds). The game must give the physics
   threads at least the priority of its other work, and use no more workers than free cores.
2. **Use about one worker per physical core.** 16 threads on the 8-core desktop was slower
   than 8 at 500-2,000 bodies.
3. **Small worlds gain little.** At 500 bodies 8 threads gives 2.1x; stage barriers dominate.
   The fork runs the solver serially below 64 awake bodies.
4. **Next levers, in order** [I]: (a) fewer solver barriers, by merging small colors or
   running several colors' stages per barrier the way C's per-color block counts allow;
   (b) parallel finalize and CCD with C's deferred enlarge bit sets; (c) joint blocks in the
   color stages (needs the kinematic-write audit noted in section 4); (d) tune block sizes in
   a clean window.
5. **Worlds own their threads.** Many worlds (chunks) would each spawn threads; a shared pool
   is a follow-up for @world-chunks' design.
6. **Custom filter callbacks** run serially, because they receive `&World`, which is not
   `Sync`. The engine does not set one.
7. **wasm32** spawns no threads and runs the identical serial path.

## 8. Reproduce

```bash
# crate tests, incl. worker-count determinism tests (parallel_tests.rs, worker_pool, stages)
cargo test -p box3d-rust --release
# determinism matrix: goldens, trace, images, rollback at every thread count, vs spikes/out/vdw
cargo build -p pe-spike-physics --release
bash scripts/check_par_determinism.sh target/release/pe-spike-physics.exe release 1 2 3 4 8 16 32
# scaling and stage breakdown (stage timers need the feature)
cargo build -p pe-spike-physics --release --features stage-profile --target-dir target/prof
target/prof/release/pe-spike-physics.exe par-bench 240 500,1000,2000,4000 1,2,4,8,16
# the clean-window driver used for section 6
bash scripts/run_par_window.sh
```
