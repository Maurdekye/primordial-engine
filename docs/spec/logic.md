# Logic: scripts as systems over the store (WS-logic)

Status: **draft v0**, 2026-11-10, by @iter-bench (WS-logic). Nothing in it is built. It is written on branch
`ws/logic` before `pe-seams` v0 is fixed; section 9 lists what it needs from the seams and from the other
workstreams, for @engine-architect to agree or change.

Owns: `crates/pe-logic` (the host), `crates/pe-sdk` (what a script links against), this document.

Marks: **[V]** verified here or in the named record, with the command or file; **[V file N]** verified by
@fable-reviewer in review file N; **[I]** my inference or proposal. Every proposal is [I] until a test or a
measurement says otherwise. Sizes are soft.

Inputs: `docs/refound-plan.md` (b610464), review files 03, 11 (3.2, 3.5), 13 (Part A), 17 (section 7 wins over
3.5-3.7), `docs/iteration-benchmark.md`, and the user's rulings of 2026-11-10 in `<notes>`.

## 0. The rules that bind this design

| Source | Rule | Where it lands |
|---|---|---|
| User, 100% Rust | No C/C++ compiled into the engine | 5.4: wasmtime's one Unix C file is patched out |
| User, logic is Rust | Scripts are Rust, compiled to wasm32 | 4 |
| User, D11 and Q3c | Every machine compiles every script itself; no machine runs another's build | 4, 6 |
| User, Q3 and Q3a | A code change takes effect at one agreed frame; the owner runs it at once; others keep the old code until their own compile ends, then rewind and replay | 6 |
| User, Q3b | A divergence over 2 s ends in a world-line merge with the main line; nobody waits | 6.4 (M2, with WS-diverge) |
| User, QR (b), Q8 | A per-player script runs on its player's machine only | 7 (M2) |
| User, file 17 answers | `Pos` cannot become numbers except as `pos - ORIGIN`; saved offsets name their reference; no warning or test, the types enforce it | 8 |
| User, U1 | M1 contains a small logic edit authored by an agent | 10 |
| Coordinator | Cranelift from the first build, with metering injected into the module, never wasmi | 5 |
| User, R7 | Live modification of logic by players with authority; sooner commit wins; the loser rebases | 6.1 |
| Fast iteration is priority 2 | Edit to first frame on the author's machine stays under about 1 s for a small script | 11 |

## 1. The model in one paragraph

A **script** is an asset: Rust source, stored as a content-addressed blob tree in the world model, attached to
objects through a `Script` component. Each machine compiles it with the pinned toolchain into a wasm module, then
runs a deterministic **instrumentation pass** that injects fuel and call-depth metering into the module's bytes.
The module exports **systems**. Each system declares the component types it reads and writes. Once per frame the
engine runs each system once over every object bound to it, handing it the matching rows in one batch and taking
back the rows it wrote. **Nothing in the module's memory survives a frame:** the instance is reset to its initial
image before each frame, so all lasting state is component data in the store, and rollback, hashing, saving and
merging of logic are the store's (file 11, 3.5; file 03, A1). A code commit is an ordinary edit entry with a base
version and an agreed frame.

## 2. Units and names

| Term | What it is |
|---|---|
| **Script** | An asset: a source blob tree (one crate) plus its declared manifest. Identified by an `AssetId`; each commit makes a new `ScriptVersion` (the hash of its source tree) |
| **Module** | The wasm bytes a machine built from one script version, after instrumentation. Local to the machine: never sent, never stored in the world (Q3c). Its hash is reported for comparison only |
| **System** | One exported function of a module, with a declared signature (section 3.2) |
| **Binding** | The `Script` component on an object, naming the script and its version. A system runs over the objects that have the binding and every component it declares as bound input |
| **Instance** | One wasmtime instance of one module in one domain (one domain per world until M4). Reset to its initial image every frame |

One script runs over many objects in one call. That is the "many small scripts on objects" shape the rulings imply
(file 03, 1) without one instance per object.

## 3. The guest ABI, v0

### 3.1 Imports and exports

- **Imports:** only functions from the module namespace `pe`, from a fixed list per ABI version (3.4). No memory,
  table or global imports. The validator refuses anything else.
- **Exports:**
  - `memory`;
  - `pe_abi_version() -> i32`;
  - `pe_manifest() -> i64`: a pointer and length, packed, to the manifest bytes (3.2). The host calls it once
    after instantiation, under a fixed fuel budget, and validates and hashes the result. A manifest that differs
    between two calls, or between two machines' builds of the same source, is a fault;
  - `pe_run(system: i32, batch: i32) -> i32`: runs one system over one batch. The return is a status (0 is ok);
    any other value, or a trap, is a script fault (3.6).
- **Why a function and not a custom section:** writing the manifest as a static byte array from a `macro_rules!`
  macro is awkward in a const context, and calling a function under metering costs microseconds once per load
  [I]. A custom section can replace it in a later ABI version without changing systems.

### 3.2 The manifest

Per module:
- the ABI version and the SDK version;
- the **components the script declares**: name, fields (name, kind, count), as the store's `ComponentDesc` already
  describes them [V `crates/pe-sim/src/registry.rs:80-172`]. Their ids are assigned by the world model, scoped to
  the script asset, so two scripts cannot collide on a name;
- per system:
  - `name`, `stage` (3.5) and `mode`: `shared` (default) or `per_player` (section 7, M2);
  - `bound`: the components every bound object must have, each `read` or `write`;
  - `reads_other`: components it may read on objects other than the bound one, through queries (3.4);
  - `emits`: the event types it may emit, each `once_for_world` or `once_per_player` (file 11, 3.5);
  - `creates`: whether it may create or destroy objects;
  - a fuel budget class (5.2).

The declared sets are the contract the engine relies on: a system can touch nothing it did not declare. That gives
the skipping of unchanged systems, parallel stages and the per-player split their soundness (file 03, A1;
open research item "sound dependency inference" becomes unnecessary because reads and writes are declarative).

### 3.3 A batch

For one call of one system, the host writes into the instance's memory, at an address the SDK reserves:
- a header: frame number, tick rate, the domain's seed, the count of rows `n`;
- the object ids of the `n` bound objects, sorted by `ObjectId` (the store's own iteration order [V
  `crates/pe-sim/src/lib.rs:6-8`]);
- one column per bound component, rows packed in that order (structure of arrays);
- an empty command buffer of fixed capacity.

The guest reads the columns, writes its `write` columns in place and appends commands. On return the host:
1. validates every written row (finite floats, booleans 0 or 1, object ids that exist), as the store already does
   for any row [V `registry.rs:177`];
2. writes back only the rows whose bytes changed, so the store's copy-on-write shares untouched blocks;
3. applies the commands in buffer order: create, destroy, attach, emit event, write to another object through a
   declared handle.

A trap or a bad status discards the whole call: no row and no command of it is applied (3.6).

**As built in v0** (pe-logic `run`, 2026-11-11) [V `crates/pe-logic/tests/run.rs`, with the test script
`tests/scripts/tally`]:
- **Layout:** one per script, not per system. Every component a system of the script binds has one offset in the
  SDK's area, sized for the same number of rows; the command buffer is a quarter of the area. So a column stays put
  from call to call, which the copy-in skip needs. Objects beyond one batch's rows go into further batches at the
  same offsets (5000 objects over two systems: more than one batch each, every row right).
- **A system call is all its batches.** Host functions read the store as it was when the call began, so the rows
  and commands of every batch are held until the last batch returns, then the rows are written and the commands
  applied, in batch order. A fault in any batch discards all of it.
- **Changed rows** are found against the store through `SimStore::column_raw` (N12, on main 18b60fde): when a
  batch's objects are one run of the column, 256-byte blocks that compare equal are skipped (3.3a) and only rows in
  differing blocks are compared; otherwise each row is compared at its index. Changed rows are validated with the
  store's own check (`ComponentDesc::invalid_field`). The bound objects are a merge join of the bound columns'
  sorted ids, and their row indices one merge walk per column per call, sliced per batch: host work fuel does not
  meter stays linear in the columns' lengths whatever batch size the script declares (`StageReport::join_steps`;
  @shader-reviewer). Copy-in is one copy per column when the rows are one run [V `tests/run.rs`,
  `the_write_back_finds_exactly_the_changed_rows...`, `a_change_only_in_the_tail_of_a_row_that_straddles...`,
  `finding_rows_is_one_merge_walk...`].
  A row that refers to an object that no longer exists is not refused: the store does not refuse one either.
- **Command records** name the row that gave them (its index in the batch). A create's id is pe-core's
  `ObjectId::spawned(line, row's object, frame, n)`, `n` counting that object's creations in the frame across both
  logic stages; the new object is a root with the one component the record carries, and the **spawn mark** (below).
  Events go to the stage's report with the emitting object, for the events stage.
- **A script destroys only what it spawned** (decided for M1, @coordinator-opus, with @fable-reviewer;
  [V `tests/run.rs`]):
  - Every object a script creates gets a spawn mark, a row naming the script (`run::spawn_mark_desc`: one 32-byte
    `script` field). A destroy of an object whose mark does not name the script faults the call. So giving
    someone's object one of the script's components (an edit can) does not make it the script's to destroy: of
    the coordinator's two ways to close that hole, this is the one chosen, because it needs no edit-permission rule
    and no later script command (attach, write to another object) can reopen it. **WS-world keeps the mark out of
    every edit's reach** (no edit writes or removes it) and reserves its id (N13). **A `Copy`, a paste or an import
    drops the mark** (@world-model): a member's copy of a spawn is the member's, not the script's.
  - The mark names the script's **stable asset id**, the same for every version (`run::ScriptKey`, the `modules`
    row of 6.1), never a version's hash: a new version can still destroy what an earlier version spawned.
  - The mark names the asset, not a binding: any object running the same script asset may destroy any spawn of that
    asset, wherever it is and whoever placed it. That is the useful rule, since a spawn often destroys itself.
  - **Holds and locks win:** the world passes a guard; a destroy whose subtree holds an object the guard names is
    refused, reported in `StageReport::refused`, and is not a fault (a player grabbing a spawn must not kill its
    script). The object stays.
  - **The orphan rule** (world-model.md 7.5): objects beneath the destroyed spawn that the script did not spawn are
    not taken. Each topmost one becomes a root in place, keeping its world pose and what is beneath it
    (`StageReport::orphaned`).
  - The world passes its own destroy (tombstones), or `SimStore::destroy` is used. A destroy of an object already
    gone does nothing. The orphaning and the destroy are one step: if the world refuses the destroy, or an orphan
    cannot be made a root, the store goes back to its copy-on-write snapshot from before the step, and the refusal
    is reported [V `tests/run.rs`, `a_destroy_the_world_refuses...`]. So the world's destroy must keep its effects
    in the store (N13).
- `pe_core::fpenv::check_and_reset()` runs before every `pe_run`; a `false` is counted in the stage's report
  (5.2b, N11).

### 3.3a The batch's cost, measured (2026-11-10: a clean window and a shared machine)

Probe: `logic-probe cols` (`spikes/iter-bench/logic-v0/src/cols.rs`, guest `logic-v0/cols_guest`), raw logs
in `spikes/out/iter-bench/logic-v0/cols/` (`clean/`: two runs in a clean window; runs 3 and 4 shared, holders in `runN-load.txt`; run 3 had
no other heavy job at its start). The benchmark's generated game logic runs as six systems (spawn, ai, combat,
effects, inventory, movement) over host-owned columns at 10k objects. Each system declares reads and writes
(`kind: manifest` lines: 14 per-object columns, the world's random state and counters, and the hit list as an
event buffer one system writes and the next reads). Per system and frame the host copies in the declared columns,
calls `pe_run`, and writes back the changed part of each written column, validating it. The reference is the same
split with the world left in guest memory (`resident`).

- **[V] Correct in every mode:** after 180 frames the host's columns are byte-identical to `resident`'s, and the
  guest's hash equals the native split run's. Every undeclared column filled with 0xA5 before each call
  (`poison`) changes nothing, so the six manifests cover every read, and the lost-write check covers every
  write. No row failed validation.
- **[V] Cost per frame,** clean window (20:53:21-20:53:45Z, both heavy slots, normal priority, two runs,
  `cols/clean/`), with the shared-machine runs 3-4 beside it:

| Part | clean, ms per frame | shared, ms | Note |
|---|---|---|---|
| native, the six systems split | 1.26-1.31 | 1.51-1.81 | 1.12-1.14 as one call: the split rebuilds the grid six times |
| `resident`, Cranelift alone | 1.46-1.59 | 1.60-1.88 | the in-memory shape, the old benchmark's |
| `resident`, injected v1 + observation-point NaN | 1.76-1.95 | 1.98-2.52 | |
| `resident`, injected v1 + Cranelift NaN (default) | 2.00-2.24 | 2.47-2.71 | |
| copy in (3.44 MB per frame) | 0.06-0.08 | 0.06-0.08 | every declared column, every call |
| write back by row | 0.55-0.64 | 0.62-0.68 | compare each row, validate and copy the changed rows |
| write back by block (256 B) | 0.18-0.23 | 0.20-0.24 | copy a differing block whole; validate only float, boolean and id columns |
| memory reset (2 MiB image copy) | 0.06-0.07 | 0.06-0.07 | the SDK's fixed heap, restored at each frame start |
| **`blocks-reset`, Cranelift alone** | **1.73-1.87** | 2.14-2.17 | **+0.27-0.28 over `resident`** |
| **`blocks-reset`, injected v1 + observation-point NaN** | **1.93-2.26** | 2.39-2.79 | +0.17-0.30 |
| `blocks-reset`, injected v1 + Cranelift NaN | 2.53-2.92 | 2.85-4.02 | |

- **What that says [I]:** the batch ABI costs 0.30-0.37 ms per frame for this guest (the sum of its parts,
  clean), i.e. 30-37 % of the 1 ms budget, on top of logic that by itself takes 1.1-1.3 ms natively. So this guest is heavier than the
  budget at 10k objects whatever the ABI, and the ABI's own share is dominated by the write-back diff.
- **Options to cut the write-back, not measured. Rulings, @engine-architect 2026-11-10:**
  (a) yes, in the SDK's v0 command set; (c) yes, nothing a script can see changes; (b) not in M1: a missed mark
  loses a write silently, so it comes back only with numbers, as a decision for the user.
  - (a) A system that rebuilds whole objects (spawn: 3 objects a frame) declares writes on all 14 columns, so the
    host diffs 140k rows to find 3. A `creates` command (3.3 step 3) instead of declared writes removes that.
    Measured since [V, shared, `cols/skip/`]: spawn's write-back is only 0.02-0.04 ms of the 0.24-0.29 ms total
    (unchanged blocks cost one memcmp each), so (a) is a structural fix worth little time here. The largest share
    is movement's, 0.08-0.11 ms (pos and vel change on nearly every live object).
  - (b) The SDK marks dirty blocks and the host copies only those, still validating them. A write the guest fails
    to mark is lost the same way on every machine, so determinism holds [I]; whether a lost write is acceptable
    to authors is a design call.
  - (c) Skip copying in a column the guest's slot already holds unchanged since the last call in the same frame
    (memory is reset only at the frame start). Copy-in is already small (0.06-0.08 ms). Measured since [V,
    shared, `cols/skip/`, modes `blocks-skip` and `blocks-skip-reset`; bytes and hashes still equal]: copy-in
    falls from 3.44 MB to 0.89 MB a frame with the per-frame reset (each column once) and to 0 without it, saving
    0.04-0.07 ms. In pe-logic v0 [V `tests/run.rs`: state and fuel equal with and without it]: within a stage,
  across the systems of a script, since the layout is the script's (3.3); every column is copied again at each
  stage's start, as the engine's own systems run in between. **Only write columns are skipped** (corrected
    2026-11-11, @shader-reviewer M2): after a call the host trusts only the columns it has just compared row by row
    with the store, the write columns; a read column is copied in again. The earlier claim, that `forbid(unsafe_code)`
    and shared read slices keep a script from writing a read column, was wrong: `macro_rules!` expands into the
    script crate, so what it calls is public, and a script can open the batch with a declaration of its own in safe
    code [V `tests/run.rs`, `sneak`: before the correction the next system read the forged 999]. The skip now saves
    only a written column's copy into a later reader in the same stage (`count` then `double` in the test).
- **Fuel:** per system call, identical across runs and equal to `resident`'s. With the per-frame memory reset it
  differs from `resident`'s (the allocator starts from its post-init state each frame, so the grid reallocates)
  but is the same in every run [V], so it is deterministic.

### 3.4 Host functions, v0

All are pure functions of the state at the start of the system call and their arguments. None reads time, the
machine, or anything outside the domain.

| Function | What it does | Limits |
|---|---|---|
| `pe.get(component, object, dst) -> i32` | Copies one row of a component declared in `reads_other` | Only declared components; a missing row returns 0 |
| `pe.query_near(component, radius, dst, cap) -> i32` | Objects within a radius of the bound object that have the component, with their offsets in the bound object's own space, sorted by distance then id | Radius capped per stage; results capped by `cap` |
| `pe.rand(object, stream) -> i64` | 64 bits for (frame, script, system, object, stream): pe-core's `Rng::for_frame` over the domain's seed, keyed by the rest. A pure function: the same arguments give the same bits, so each draw in a row uses its own stream [V pe-logic `tests/run.rs`: four draws distinct, the same in a fresh world, different in another frame] | The object is a pointer to its 16 bytes; the guest names it, the host keys it |
| `pe.log(ptr, len)` | A diagnostic line for the author's tools only; never affects state | Dropped outside `code.test` and the author's machine |

There is deliberately no host function for maths. Transcendental functions come from Rust's own library compiled
into the module, so they are the same bytes on every machine [I; to check with the measurements of section 11].

### 3.5 Order within a frame

- A frame runs stages in a fixed order. Proposed for M1, to agree with WS-state: **inputs applied → logic
  `pre_physics` → physics step → logic `post_physics` → events published**.
- Within a stage, systems run in a total order: by script asset id, then by the system's index in its manifest.
  v0 runs them one after another, and each sees the writes of those before it.
- Later (M2), systems with disjoint write sets and no read of each other's writes may run in parallel. The rule is
  that the result equals the sequential order exactly, which a dependency graph built from the manifests gives
  [I].

### 3.6 Faults

Every trap is deterministic under section 5: fuel, call depth, `unreachable`, an out-of-bounds access, an integer
divide by zero, a bad status, an invalid written row, a command buffer overflow, memory growth past the limit.

- The call's writes and commands are discarded: every batch of the system call (3.3). That includes a fault while
  the call is applied (a create the store refuses, past a world limit or onto an id that exists): the runner takes
  the store's copy-on-write snapshot before it applies a call and restores it, with the spawn counts, on a fault
  [V `tests/run.rs`, `a_fault_while_applying...`; design-reviewer]. The snapshot costs a reference
  count per column, but each column the call then writes is copied once more on its first write [I, unmeasured;
  checking every command's preconditions before writing would avoid the copy if it shows].
- A script that overflows rustc's shadow stack (the 64 KiB of 4.2, in linear memory) faults as an out-of-bounds
  access before the depth meter fires: the stack sits below the data and grows down past address 0 [V pe-logic
  `tests/run.rs`, `sink`]. Recursion with no frame in memory hits the depth meter [V, `dive`]. Both are the same on
  every machine, since the module's bytes fix every frame's size.
- The script's binding is marked **faulted at frame F** in the store, with the fault's kind. That is world state:
  hashed, rolled back, replayed and shown in the inspector (D14d, extended to every trap, as file 03 proposed).
  In v0 the runner reports the fault and runs no more of the script's systems that frame; the caller passes the
  set of faulted scripts in at each stage and keeps it as world state (N4), so the runner itself holds nothing
  across frames that a rollback would have to undo.
- A faulted script does not run again until a new version is committed or an editor clears the mark.
- The author receives a structured error: script, version, system, frame, kind, and for fuel and depth the budget
  that ran out.

A peer process never panics on anything a module does [I; a test in 10.3].

## 4. Building a script

### 4.1 What a script crate may contain

- One crate, edition 2024, depending only on `pe-sdk` (and later on an allow-list of pure crates, by hash).
- **No build script and no procedural macro,** in the script or in `pe-sdk`. Both are compiled for the host and
  need a host linker, which a player may not have (file 03, defect 2). `wasm32-unknown-unknown` links with the
  `rust-lld` that ships with the toolchain [I; section 11, item 6 checks it]. So `pe-sdk`'s macros are `macro_rules!` only.
- No `unsafe` outside `pe-sdk`: the build service compiles the script crate with `-F unsafe_code` (pe-sdk keeps
  its own) [V pe-logic `tests/lamp.rs`: the lamp builds; a script with an `unsafe` block is refused]. It protects the
  SDK's own invariants inside the guest (no aliasing column slices: `Batch::write` hands each column out once; `ctx`
  checks its row). **The host trusts none of it** (corrected 2026-11-11): the sandbox is the wasm boundary, every
  written row is compared and validated, and the copy-in skip trusts only those compared columns (3.3a (c)), because
  a safe script can still reach the SDK's public batch API with a declaration of its own. A rule of the script build
  (pe-seams RULINGS.md, "The script contract").
- **The build service enforces these** before it runs cargo [V `tests/lamp.rs`,
  `a_script_crate_outside_the_rules...`]: the script's `Cargo.toml` passes a strict line allowlist
  (`build::check_manifest`: sections `[package]`, `[lib]`, `[dependencies]`, `[workspace]` only; `pe-sdk = { path
  = "..." }` the one dependency; no `build` key, no profile, patch, target or build-dependency section; anything it
  does not recognise is refused, not interpreted), and the crate folder may hold no `build.rs` and no `.cargo`.
  The script's `pe-sdk` path must lead to the engine's own pe-sdk, the folder the caller of `build::build` names;
  otherwise a script could have another crate named pe-sdk built, with a build script or a procedural macro
  (@shader-reviewer, round 2) [V `tests/lamp.rs`, `a_script_may_not_choose_its_own_pe_sdk`].

### 4.2 The build

- Toolchain: pinned by hash (D75), with `wasm32-unknown-unknown`.
- Profile: `rel-fast`, the user's settled profile: opt-level 3, no LTO, 16 codegen units, `panic = "abort"`
  [V profile in `spikes/iter-bench/guests/Cargo.toml:26-32`]. It builds about as fast as `dev` and runs about 12
  times faster [V `docs/iteration-benchmark.md`, finding 2]. **Except incremental, which the script build turns
  off** (2026-11-11, @design-reviewer and @shader-reviewer L1, raised to @coordinator-opus): rustc
  promises a warm incremental cache the same semantics as a clean build, not the same bytes, and fuel follows the
  bytes. Every machine's build is the canonical one, the author's included, because the author's module hash is
  what the others compare (6.2 step 3). The 16 codegen units stay: a non-incremental build partitions the same way
  every time [I; the test below compares a warm rebuild with a clean one]. Decided (a), incremental off for every
  build, by @coordinator-opus on @fable-reviewer's advice, with conditions: every output-affecting
  setting is pinned rather than left to a default (below); the identity test also runs across two OSes (Windows and
  Linux, `scripts/iter_bench_xos.sh`, below); and it runs in the landing gate (with @provenance-eng). The edit-time cost is to be measured
  in a clean window; a noisy run on 2026-11-11 could not separate the two modes.
- **Pinned, nothing left to a default or to the machine:** the profile keys above plus debug assertions and
  overflow checks off and no rpath; `-C target-cpu=mvp` with exactly the features of 5.3
  (`+bulk-memory,+multivalue,+mutable-globals,+nontrapping-fptoint,+reference-types,+sign-ext`, the list
  `rustc --print cfg` gives for `wasm32-unknown-unknown` today; yet pinning it changed the lamp's bytes, sha256
  601ba93d to d71f7a24 [V], cause not isolated [I: the CPU name LLVM tunes for, or the flags entering cargo's
  metadata hash]; @shader-reviewer: `target-cpu=mvp` also changes the CPU LLVM's cost model tunes for, `generic` by
  default); the linker (`rust-lld`, which a machine's cargo configuration could otherwise change);
  `CARGO_INCREMENTAL=0` and no rustc wrapper in the child's environment, since
  both override a profile; the flags through `CARGO_ENCODED_RUSTFLAGS`, which overrides every other source. The
  16 codegen units do not depend on the core count. What remains is the toolchain itself, pinned by hash (D75).
  **Do not simplify the pins away** (@design-reviewer): dropping one, even to a value equal to today's default,
  changes the bytes and so every module hash, and a default may move with a toolchain.
- Flags fixed by the build service, never by the script: the target features of 5.3, a 64 KiB stack
  (`-C link-arg=-zstack-size=65536`; the 1 MiB default made most of the old spike's memory [V file 03 appendix]),
  `--remap-path-prefix` for every path (the script's folder, pe-sdk's, the target directory and Cargo's home: pe-sdk's
  panic locations survive `strip`, @shader-reviewer M3), and no debug information. [V `tests/lamp.rs`,
  `the_module_is_the_same_bytes...`: the lamp built from two copies of the sources at different places and depths,
  each with its own target directory, and rebuilt warm after a change and its undo, gives the same bytes and module
  hash, and no folder name is left in it; without pe-sdk's remap the two copies give 9,585 and 9,569 bytes.]
- **Across OSes, every source file is remapped whole on Windows.** A folder remap keeps the host's separator below
  the folder: from main 3353d65f the lamp built on Windows and on Linux (WSL, the same rustc 1.99.0 b940084d7 and
  LLVM 23.1.1) gave 9,473 bytes each but sha256 d71f7a24 and aaf59021, 3 bytes apart, all in panic locations in
  the data section: `/pe-sdk\src\guest.rs` and `src\lib.rs` against `/pe-sdk/src/guest.rs` and `src/lib.rs` [V
  `scripts/iter_bench_xos.sh`, log in `spikes/out/iter-bench/logic-v0/xos/`]. The build now remaps, on Windows, each
  file under pe-sdk's and the script's `src` (sources and `include!`d files) to its forward-slash name (the script's also by the relative path
  cargo passes), and the Windows lamp became aaf59021, Linux's bytes [V]; the identity test refuses a `\` beside
  `src` in a module (it fails without the remap). A module brought in with `#[path]` from outside `src` is not
  covered; the module-hash comparison (6.2) reports it as a build fault, not a desync, which @coordinator-opus
  accepted. The flags grow with the file count: 301 files
  make 114,652 characters on Windows, past the 32,767 a Windows variable is said to hold, and the build still works
  and gives the same module (std passes its own environment block; cargo moves a long rustc command line into an
  argument file) [V `tests/lamp.rs`, `a_script_with_many_files_builds_the_same_module`; @shader-reviewer's Low,
  refuted by measurement]. Alternative not taken (same ruling: not worth leaving the pinned stable toolchain): rustc's
  `-Z location-detail=none` drops file names from panic locations altogether, but is unstable (needs
  `RUSTC_BOOTSTRAP` on the pinned stable toolchain).
- The build runs as a child process at idle priority, one at a time per machine, in its own target directory per
  world.
- **Reproducibility is a requirement, not an assumption:** two machines' builds of one source must give the same
  bytes, or their states differ and the hash tree finds it. Shown so far for six dependency-free modules on two
  x86-64 machines only [V file 03]. The M2 acceptance (same bytes on all four hosted platforms) is the test.

### 4.3 The sandbox

M1 runs rustc on source from a trusted second machine on a LAN. rustc on strangers' source needs a sandbox
(file 03, defect 2), which is unbuilt; it is an M3 item, before public worlds. Until then, committing code needs the
`code` permission, which strangers do not have (Q6) [I].

## 5. The runtime: wasmtime with Cranelift

### 5.1 Configuration

wasmtime with default features off and `std`, `runtime`, `cranelift` on (and `parallel-compilation`, which is
rayon), as the benchmark built it [V `spikes/iter-bench/host`; audit: no C on Windows, `audit-runtimes.md`].

Fixed settings, the same on every machine:
- strategy Cranelift, optimization level `speed`;
- `cranelift_nan_canonicalization(true)`: x86-64 and ARM64 produce different NaN bits, so without it a script that
  inspects float bits diverges across platforms [V for x86-64 NaN bits in the benchmark's probe; I for ARM64];
- wasmtime's own fuel and epoch interruption **off**: metering is in the module (5.2);
- the wasm features of 5.3, and nothing else;
- `max_wasm_stack` set above the bound that the depth meter guarantees (5.2);
- a memory limit per instance through a resource limiter; `memory.grow` past it returns -1, the same on every
  machine;
- `async_stack_size` at least `max_wasm_stack`: wasmtime refuses the configuration otherwise, even with async off
  [V, pe-logic's tests];
- **the thread that runs guests has a native stack of `max_wasm_stack` plus 2 MiB** (`Settings::thread_stack`).
  wasmtime measures `max_wasm_stack` from where it entered, so a thread with less could overflow natively before the
  depth meter or wasmtime's check fires [I from wasmtime's design; the tests run on such threads].

The per-frame reset (pe-logic `Instance::reset`) copies the memory image taken after instantiation back and restores
every mutable global; when the memory has grown, a copy cannot undo it, and the instance is made afresh. Tables and
passive segments are outside what it restores, so the validator refuses the operators that change them (5.3).

Measured cost of these choices at 10k entities in the benchmark's guest, as multiples of native [V
`docs/iteration-benchmark.md` 4.1]: Cranelift 1.1x; with wasmtime's fuel 1.4x; with fuel and NaN canonicalization
1.7x. On the physics-in-wasm guest NaN canonicalization doubled the cost (0.71 to 1.41 ms per frame) [V]. Injected
metering's cost is not measured yet (section 11, item 2).

### 5.2 Injected metering

A pure-Rust pass, `pe-logic::instrument`, built on `wasmparser` and `wasm-encoder`, rewrites the module after
validation. Its output is a deterministic function of its input bytes and the pass's version, so every machine
that built the same bytes meters them the same way, whatever runtime runs them later.

- **Fuel.** A mutable global `fuel: i64`. At the start of every basic block, the block's static cost is subtracted
  and, if the result is negative, the module traps through `unreachable`. Costs are per instruction class, with
  extra weight for `memory.copy`, `memory.fill` and `memory.grow` by their length argument: one unit per 64 bytes
  (`meter::BULK_BYTES_PER_FUEL`), and for `memory.grow` the same per byte of the pages asked for, charged and
  checked just before the operator [V `tests/runtime.rs`: a loop of 64 KiB fills or copies stops within ~1,000
  iterations at 10^6 fuel; uncharged it ran 99,999]. A fuel trap leaves `meter::FUEL_TRAPPED` in the global, and the
  host checks depth first, so a depth trap is never labelled `Fuel` [V]. The host sets the
  global before each call to the system's budget and reads what is left after it.
- **Call depth.** A mutable global `depth: i32`. Each function adds its own weight on entry (1 plus its count of
  locals plus its maximum operand stack height, a static bound on its frame) and subtracts it on every exit
  (`return`, the final `end`, and a `br` to the function's outermost block). Past the limit it traps. Because the
  weight bounds the native frame, `max_wasm_stack` can be set so that the native stack can never overflow before
  the meter trips, on any compiler or CPU. That makes recursion limits deterministic, which file 03 (M-1) found
  missing [I; tests in 10.3].
- **Indirect calls and tables** pass through unchanged; the callee meters itself.
- The pass refuses a module that already defines the names it injects.
- Which trap fired is read from the globals, so the host can report fuel and depth faults by kind.

Why not wasmtime's own fuel: it works and is deterministic with eager compilation, but the fuel points then belong
to wasmtime's version, and a runtime upgrade would move every world's fault points (file 03, defect 3). Injected,
they belong to the bytes.

### 5.2a First measurements (2026-11-10, shared machine)

Probe: `spikes/iter-bench/logic-v0` (the pass is `src/instrument.rs`); raw logs in `spikes/out/iter-bench/logic-v0/`.
Every run went through `heavy.sh` beside one or two other heavy jobs, logged in `runN-load.txt`, so times are
ranges over runs 2-4 (about ±30%). Clean numbers wait for a quiet window.

- **[V] Depth meter:** the deepest recursion that completes is exactly `limit / weight - 1`, the same with
  `max_wasm_stack` at 1 MiB and 8 MiB, for frames of weight 4 and 68 (`traps-run2.jsonl`). One x86-64 machine
  only; the ARM half is [I] until the hosted runners exist.
- **[V] Fuel trap:** a spinning loop with a budget of 10^6 stops after 166,666 iterations, 3 runs of 3. Fuel per
  tick and the final digest are identical across two independent runs in every mode, and the digest equals the
  native build's.
- **[V, ranges] Cost at 10k entities, 6.4k-line guest (`bench-run2..4.jsonl`):**

  | Mode | ms per tick |
  |---|---|
  | native | 1.0-1.5 |
  | Cranelift, no metering, no NaN canonicalization | 1.18-1.39 |
  | Cranelift + NaN canonicalization | 1.58-1.63 |
  | + wasmtime fuel | 1.84-2.59 |
  | + injected v0 (fuel in the global, checked at every segment) | 2.56-3.06 |
  | + injected v1 (fuel in a local, checked at entry and loop headers; flushed around calls) | 2.22-2.57 |
  | injected v1 + NaN canonicalization at observation points (5.2b), no Cranelift canonicalization | 1.84-1.92 (run 4 only) |
  | injected v1 + Cranelift NaN + whole state copied in and out every frame (890 KB) | 2.85-3.91 |
  | the same with a fresh instance every frame | 3.35-4.12 |

  On physics-in-wasm (`physics-run2..4.jsonl`, ms per frame): Cranelift 0.86-0.95; + NaN canonicalization
  1.59-1.72; + wasmtime fuel 1.75-1.96; + injected v1 1.66-1.91; injected v1 with observation-point NaN 1.21.
- **[V] Copy versus fresh instance:** a plain copy of 1 MiB takes 0.016 ms and of 4 MiB 0.14 ms. Resetting by
  copy needs a memory that never grows, so **pe-sdk provides a fixed heap** (a bump allocator over a static region
  that is reset every frame) and the module's memory has equal minimum and maximum [I; the decision follows from
  these numbers].
- **[V] Load:** the 264 KB module instruments in 15-19 ms and compiles in 55-65 ms; a 1 KB module in 0.1 and
  1.1 ms (`load-run*.jsonl`). Both are far inside the 2 s window of 6.4.
- **[V] No host linker:** guests with no build script and no procedural macro build for wasm32 with cargo's host
  linker set to a program that does not exist; a control crate with a build script fails (`nolinker.log`).

**Clean window (coordinator's, 2026-11-10 20:12:08-20:13:15Z, both heavy slots held by this run, normal
priority; `clean/run-load.txt`).** Two runs, each with two independent instances per mode; medians in ms, run 1 /
run 2 (`clean/bench-run{1,2}.jsonl`, `clean/physics-run{1,2}.jsonl`). The logic guest's tick still varies about 20%
between runs even on a quiet machine (native 1.42 against 1.14), so compare modes within a run; physics agrees within
5%.

| Mode | logic, 10k entities | x native | physics, per frame |
|---|---|---|---|
| native | 1.42 / 1.14 | 1 | 0.31 (phase 1, not re-measured) |
| Cranelift | 1.75 / 1.20 | 1.2 / 1.1 | 0.83 / 0.85 |
| + Cranelift NaN canonicalization | 1.90 / 1.64 | 1.3 / 1.4 | 1.77 / 1.70 |
| + wasmtime fuel | 2.21 / 2.27 | 1.6 / 2.0 | 1.73 / 1.69 |
| injected v0 + Cranelift NaN | 3.08 / 3.13 | 2.2 / 2.7 | 1.78 / 1.77 |
| **injected v1 + Cranelift NaN** (the M1 default) | **1.88 / 1.87** | 1.3 / 1.6 | **1.80 / 1.65** |
| injected v1, no NaN canonicalization | 1.34 / 1.59 | 0.9 / 1.4 | 0.88 / 0.93 |
| **injected v1 + observation-point NaN** (5.2b) | **1.52 / 1.49** | 1.1 / 1.3 | **1.05 / 1.00** |
| injected v1 + Cranelift NaN, whole state copied in and out | 2.36 / 2.40 | | |
| the same, fresh instance every frame | 2.88 / 3.33 | | |
| the same, pooling allocator | 3.10 / 3.14 | | |

- [V] Every mode's digest equals native's, and fuel per tick and digest are identical between the two instances
  of every mode. Injected fuel per tick: 34,662,520 (wasmtime's count: 24,334,807; the units differ).
- [V] Load (`clean/load.jsonl`): the 264 KB module instruments in 15 ms and compiles in 35 ms; the 1 KB module in
  0.12 and 1.2 ms. Instantiation takes 0.03-0.05 ms.
- Reading [I]: injected v1 costs no more than wasmtime's own fuel and makes metering a property of the bytes, as
  ruled. NaN canonicalization is the larger lever: observation-point canonicalization brings the logic guest to
  1.1-1.3x native and physics to about 1.2x Cranelift without it. Copying the whole state each frame adds about
  0.5 ms at 10k entities; a fresh instance each frame adds 0.5-1 ms more, so the per-frame reset should be the copy
  of a fixed heap (above), not re-instantiation.

### 5.2b NaN canonicalization at observation points (an option, for @design-reviewer)

NaN *bits* differ between CPUs; whether a value is NaN does not. Bits become observable only through: a float
store to memory, a bit cast to an integer, the sign operand of `copysign`, a write to a float global, and values
handed to the host. The pass can canonicalize at exactly those points (the host canonicalizes floats it receives),
and refuse SIMD, whose integer lanes would be one more way to see the bits. Then Cranelift's own canonicalization
after every float operation is not needed.

- [V] Same NaN probe bits as Cranelift's mode (`7fc000007fc01bf5`) and the same digest; physics 1.21 against
  1.80 ms per frame.
- **Reviewed by @design-reviewer (2026-11-10, code read, nothing run):** the list is complete for
  wasm 2.0 without SIMD as the validator enforces it, and the reading of the spec (sign and payload free, NaN-ness
  fixed) is right. Conditions, all now rules of this spec:
  1. **Both directions at the host boundary:** floats the host sends into a module (results of `pe` imports,
     arguments to exports) are canonicalized too, not only those it receives.
  2. **`global.set` is resolved by index,** which is safe only because global imports are refused (3.1). That
     refusal is a stated invariant with its own test.
  3. **The pass is part of the build's identity:** its version is pinned under the SDK hash in `CommitScript`, and
     its output is byte-deterministic, or fuel and trap points differ between machines (6.2 step 3 already makes a
     differing module a build fault).
  4. **Flush-to-zero:** MXCSR FTZ/DAZ or FPCR.FZ changes non-NaN results. WS-state's floating-point guard must
     cover the thread that runs wasm, at every entry into a guest (N11). The call is
     `pe_core::fpenv::check_and_reset() -> bool` (`crates/pe-core/src/fpenv.rs:16`, on main; @vdw-physics): per thread, one register read and a compare; `false` means something had changed the
     environment, which it has already restored, and pe-logic counts it as a fault while the step goes on. A check at
     entry cannot see a change made during the stage: if a host function ever calls a native library, check again
     when it returns.
- **Before the default changes:** on an aarch64 hosted runner, digest `copysign(1.0, 0.0/0.0)` stored to memory
  (x86's default NaN is negative, Arm's positive, so -1 against +1 without canonicalization). The control:
  removing the canonicalization at `copysign` must make the two digests differ. Until then the host keeps
  Cranelift's canonicalization on, which is safe either way.

### 5.2c The patch on Linux (measurement 5)

[V] `unix_check.sh`, in WSL Ubuntu 24.04 (Linux 6.6, x86-64, rustc 1.99.0 already installed at user level),
with `CC`, `CXX` and `CC_x86_64_unknown_linux_gnu` set to `/bin/false`, so any C compile fails the build
(`unix-check.log`, 20:14:51-20:16:20Z, through `heavy.sh`):
- **Control:** the probe against the pristine registry wasmtime 50.0.1 fails in wasmtime's build script; the
  failing command is the compile of `src/runtime/vm/helpers.c`. So the setting bites.
- **Patched** (`crates/pe-logic/vendor/wasmtime-50.0.1` through `[patch.crates-io]`): builds in 71 s; no `.o` or
  `.a` in its build directories.
- **Traps on Linux:** the depth and fuel results are identical to Windows' (same deepest depths, the same 166,666
  iterations and fuel left), and foreign imports are refused. Same CPU, so this shows the OS does not matter; the
  CPU half waits for the hosted aarch64 runner.
- Not covered: macOS (no machine here; its code path keeps `load_unwind_section_api` and differs only in
  `__unw_add_dynamic_fde`), and a backtrace through wasm frames on Linux.

### 5.3 Wasm features

Allowed: the MVP, mutable globals, sign extension, non-trapping float-to-int, bulk memory, multi-value, reference
types (as stable rustc emits for `wasm32-unknown-unknown` [I; to pin with the first build]), and fixed-width SIMD (deterministic
with NaN canonicalization [I]).

Refused at validation: threads and shared memory, relaxed SIMD (its results are allowed to differ by platform),
memory64, exceptions, GC, the component model, and any feature not listed. The list is part of the ABI version.

Also refused, though they are wasm 2.0: `table.set`, `table.grow`, `table.fill`, `table.copy`, `table.init`,
`elem.drop`, `memory.init` and `data.drop`, which change tables or the dropped state of passive segments, which the
per-frame reset does not restore (5.1); and a start function, which would run at instantiation outside any system
call. rustc does not emit them for ordinary `wasm32-unknown-unknown` code [I; to check with pe-sdk's first build].

### 5.4 The C file on Linux and macOS

wasmtime's build script compiles `src/runtime/vm/helpers.c` on every Unix target when `runtime` is on, even with
default features off [V: `wasmtime-50.0.1/build.rs:58-61,113-137`; upstream TODO at line 129]. Without
`debug-builtins`, that file only provides `wasmtime_libunwind()`, which returns three weakly linked libunwind entry
points, or nulls when the process uses libgcc's unwinder [V: `helpers.c`, `src/runtime/vm/sys/unix/unwind.rs`].

Proposed patch [I]: replace it with Rust that looks the three symbols up at run time (`dlsym` on the default
handle, through `libc`), and skip `build_c_helpers` when `debug-builtins` is off. Options for carrying it:

| Option | Cost | Risk |
|---|---|---|
| A. A vendored copy of the `wasmtime` crate with the two-file patch, through `[patch.crates-io]` | Vendoring one crate; re-apply on each wasmtime upgrade | A stale fork |
| B. Upstream the change (the TODO says they want it), use A until it is released | As A, plus an upstream PR (outward-facing: needs the user's go) | None beyond A |
| C. No unwind registration on Unix | Smaller patch | Panics and backtraces through wasm frames lose fidelity; traps still work [I] |

I recommend B, starting with A. It needs a path for the vendored crate and a line in the root manifest, both
@engine-architect's (section 9). The patch is to be built and tested on Linux (WSL) before M1 depends on it
(section 11, item 5).

## 6. Code commits and activation (Q3)

### 6.1 The commit entry

A code commit is an **edit** entry (S1), payload owned by WS-world.

Agreed with @world-model (world-model.md r7, e547cea, its 7.3 and 5.4), which owns the codec:

```
CommitScript {
    script: AssetId,            // the script asset
    base: Option<ScriptVersion>,// the version the author edited; refused if stale (R7: the loser rebases).
                                // None means a new script, refused as Stale if the script exists
    source: Hash,               // root of the source blob tree; the blobs travel by hash on the bulk lane
    toolchain: Hash,            // D75
    sdk: Version,
    author_module: Hash,        // the author's built module, for comparison only, never fetched (Q3c)
    activate_at: Frame,         // the agreed frame: equal to the entry's own frame stamp (WS-logic's rule;
                                // world-model refuses anything earlier than the stamp)
}
```

The world's `modules` table holds one row per script asset: the version (source tree as a `SimBlobRef`), the
toolchain and SDK hashes, `author_module` as a plain hash, the declared components, `activate_at` and the faulted
mark. A compiled module is never a blob and never in world state (Q3c). Component ids are
`BLAKE3("pe.component" ‖ script asset id ‖ name ‖ schema hash)`, so a commit with an unchanged schema keeps its
ids and rows with no migration. Objects a system spawns get `BLAKE3("pe.spawn" ‖ world ‖ spawner ‖ frame ‖ n)`,
where `n` counts the spawner's *applied* spawns in that frame (a discarded call does not advance it).

- **Conflict rule:** a precondition edit (file 11, 3.2). A second commit on the same base is refused with the
  reason and the winning version; the author has the 10 s seat to rebase (R7).
- **Attaching a script** to an object is a plain edit that sets a `Script` component; it needs no compile if the
  machine already has that version's module.
- The new version, the activation frame and each binding's version are **world state**, inside the hash tree,
  so two machines that disagree on which code ran find it at the next cut.

### 6.2 What each machine does

1. **The author** builds and instruments first, then commits stamped with its current frame, and runs the new
   module from that frame at once. It never waits. Until the commit is final it is provisional; if it is refused
   (stale base, no permission) the author's view rolls back.
2. **Every other machine**, on receiving the entry:
   - fetches the source blobs, starts its own build, and keeps simulating with the old module as a prediction
     (Q3a: objects keep running the old code);
   - **does not vote for any cut at or past `activate_at`** until its build is ready, because it cannot yet
     compute those frames (needs S3, section 9);
   - when the build is ready and the module validates: if the agreed frame is within the replay window (a
     duration in seconds, 2 s by default, never a frame count; the tick rate is per world, 60 Hz by default), it rolls
     back to `activate_at` and replays with the new module. Its objects jump once to the new behaviour.
3. **A build can lag, never diverge** (@chunks-reviewer on world-model r7). A machine never runs a module unless
   all of these hold; otherwise it is a **build fault on that machine**, and the module is never run there:
   - its toolchain hash and SDK hash equal the row's;
   - the build succeeded and the module passed validation and instrumentation;
   - its manifest equals the one the author's build declared (the declared components in the row);
   - its module hash equals `author_module`. The hash is of the metered bytes, which are what runs
     (`runtime::Script::module_hash`, `pe_core::Hash::of("pe.logic.module", metered)`), so it covers the pass's version too.

   On a build fault the machine keeps predicting with the old module, never votes for a cut at or past
   `activate_at`, and reports the fault with both hashes. It cannot follow the world past that frame until the
   cause is fixed (for example by fetching the pinned toolchain, D75), so it lags; it never computes a state of its
   own. **For @design-reviewer:** if the author's build is the odd one, every other machine faults and finality
   stops past `activate_at` while play continues on predictions. That needs reproducible builds, which 4.2 makes a
   requirement and M2 tests on four platforms; a rule for that case (for example, refusing a commit whose module
   hash a quorum does not reproduce) is open.

From the replica's side a pending module is the same thing as a late entry: the frames since `activate_at` are a
prediction until it arrives [I; file 13, A.4].

### 6.3 Costs, from measured numbers

Frame counts below are at the default 60 Hz, for illustration only; every limit is in seconds.

| Build on that machine | Frames to replay at 60 Hz | Inside a 2 s window |
|---|---|---|
| 0.45 s (small edit, incremental, 6.4k lines) [V benchmark] | 27 | Yes |
| 1.3 s (clean build of a 6.4k-line module, LTO) [V benchmark] | 78 | Yes |
| 15-20 s (a physics-sized module) [V file 03] | 900-1,200 | No: 6.4 |

Logic re-runs on every replayed frame, so replay speed depends on logic cost, which is why Cranelift matters here
(file 03, defect 3).

### 6.4 Past the window (Q3b, M2)

When a build ends more than the window (2 s, a parameter) after `activate_at`, the machine performs the
world-line merge of Q3b: it takes the state at `activate_at` from its own history or from the others, re-simulates
everything since under the new code (its own actions included) in the background, and its changes are merged in.
That is pe-diverge's merge engine v1 (plan, C8). WS-logic's part: a build-ready event with its frame, and the old
and new modules both loadable at once so the replay can run beside the live simulation.

### 6.5 Two commits in quick succession

Exact replay from the first commit's frame needs both modules. v0 builds commits in order, one at a time. If that
proves too slow, the alternative (build only the newest, recover through the merge) goes to the architect with
numbers [I].

## 7. Per-player scripts (QR (b), M2)

- A system declared `per_player` runs on its player's machine only, over that machine's predicted state, and may
  read that player's private components.
- What it does to the shared world leaves the machine as that player's signed **input** entries
  (`ScriptEffect`), each naming the versions of the shared things it read; one whose versions are stale when it is
  ordered is refused (file 11, 3.5: no ghost effects from shared state).
- Each effect is declared `once_for_world` or `once_per_player`, so one trigger seen by N players gives one effect
  where the author meant one.
- Its state is not world state; it may live in the player's profile store.
- Its instance is reset per frame like any other, but it is never rolled back with the world: its outputs are
  inputs, and inputs are not undone [I].

M1 has no per-player script. The manifest carries `mode` from v0, so no ABI change is needed later.

## 8. Positions for script authors (file 17, section 7)

`pe-sdk` exposes the types; `pe-logic` converts at the batch boundary.

- **`Pos`**: exact everywhere, the store's chunk index plus offset [V `crates/pe-core/src/coords.rs:23-28`],
  exposed with no accessor to its numbers. Allowed: `Pos - Pos -> Offset<World>` (exact on the chunk indices
  first), `Pos + Offset<World> -> Pos`, `==`, distance, blend. `ORIGIN` is a constant `Pos`; `pos - ORIGIN` is the
  only way to absolute numbers, and it reads as the choice it is.
- **`Offset<S>`**: an ordinary `f32`/`f64` vector tagged with its space: `Own` (the bound object's own space),
  `Parent`, `World` (world axes, no origin). Mixing two spaces needs an explicit conversion through the bound
  object. Offsets have no `==`; they offer `within(distance)`.
- **Fields:** a saved field may be `Pos` or `OffsetFrom<Own | Parent | Anchor>`; a bare `Offset` field does not
  compile, because the component macro only accepts the field types that name a reference.
- **What the batch hands a system:** the bound object's own transform relative to its parent, as its inspector
  shows it, and everything else (query results, hit points, neighbours) in its own space.
- The store needs a `Pos` field kind (three `i64` and three `f64`, 48 bytes) for the first; that is WS-world's
  `FieldKind` (section 9).

M1's toy uses only `Offset<Own>` and its own fields. The types exist from v0 so the API does not break in M2 [I].

## 9. What this needs from others

To agree with @engine-architect (seams) and the owners named.

| # | From | What | Why |
|---|---|---|---|
| N1 | WS-seams, S2 | The replica calls logic at fixed stage points (3.5), with the store, the frame and the domain; logic returns rows and commands applied through the store | The frame order is a seam property |
| N2 | WS-seams, S2 | A replica can hold two modules for one script at once (old, new) and roll back to a named frame on a build-ready event | 6.2, 6.4 |
| N3 | WS-seams, S3 | A way to say "do not vote for cuts at or past frame F yet" (a vote hold), and to release it | 6.2 step 2 |
| N4 | WS-world | `CommitScript` (agreed, world-model r7) as an edit payload with a precondition; a `Script` binding component; source as a blob tree; script-scoped component ids; a `faulted` mark in the binding; a `Pos` field kind | 3.2, 3.6, 6.1, 8 |
| N5 | WS-world | Component descriptions registered at run time, versioned by frame (already planned: plan 2.1) | A commit can add components |
| N6 | WS-state | The stage order of 3.5 | Logic before and after physics |
| N7 | WS-api | `code.check` (build and validate, diagnostics as data), `code.commit`, and for M2 `code.test`. WS-logic provides the backend calls; WS-api the MCP tools | The agent's edit in M1 |
| N8 | WS-seams (root manifest) | A path and a `[patch.crates-io]` line for the patched wasmtime | 5.4 |
| N9 | WS-ci | `audit_native.py`'s allowlist covers wasmtime's tree with the patch, on Linux and macOS | 5.4 |
| N11 | WS-state | The floating-point guard (FTZ/DAZ, rounding) runs on the thread that runs wasm, at every entry into a guest (5.2b) | Non-NaN results depend on it |
| N10 | WS-render, S5 | Events emitted by scripts reach the frame view once, not once per replay (plan M1: effects fire once) | 3.3 step 3 |
| N12 | WS-world (pe-sim) | Raw column access by `ComponentId`: the sorted ids and the row bytes, read-only. **Done:** `SimStore::column_raw`, main 18b60fde; the runner uses it | The bound-object join and the 256 B block write-back of 3.3a |
| N13 | WS-world (pe-world) | Where a script's components are registered in a live world (the store's registry is fixed at `SimStore::new`), and the runner's hook in the replica's two logic stages, with its report turned into `Effects` (created, destroyed, written rows). The spawn mark's id reserved; no edit writes or removes the mark, and `Copy`, paste and import drop it. The world's destroy keeps its effects in the store, since the runner undoes by restoring the store | 3.3, 3.5, 3.6, N1, N5 |

## 10. M1 and M2

### 10.1 M1: one small logic edit, authored by an agent

Proposed demo step (to agree with the coordinator and WS-app): the ball pit's two lamps carry a `Blink` script
(a `pre_physics` system reading `Blink { period, phase }` and writing the lamp's colour and intensity). The agent
is asked to "make the lamps pulse red when a ball is near". It edits the source, runs `code.check`, and commits.
A sees the change at once. B builds it itself (about 0.5-1.3 s for a script this size [I, from 6.3]), rewinds to
the agreed frame and replays; both roots match at the next cut.

M1 builds:
- `pe-sdk` v0: component and system macros (`macro_rules!`), batch access, the command buffer, `Offset<Own>`,
  `Pos` and `ORIGIN`, `rand`;
- `pe-logic` v0: the build service, the validator, the instrumentation pass, the wasmtime host with the settings
  of 5.1, per-frame instance reset, the batch runner over the store, faults as state, and activation **inside the
  window** (6.2), which is the late-entry path the replica has anyway;
- the Unix patch of 5.4, if M1's gate runs Linux and macOS (it does: plan M0);
- the backend of `code.check` and `code.commit`.

Left for M2: the merge past the window (6.4), per-player scripts (7), `code.test` with random rollbacks and
instance resets, offset spaces beyond `Own`, parallel stages, skipping unchanged systems, the build sandbox (M3).

**Ruled by @coordinator-opus (2026-11-10):** M1 builds the inside-the-window half of Q3
activation as 6.2 describes. The author runs the change at once; each other machine builds it itself, rolls back
to the agreed frame and replays, and holds its cut votes from that frame until its build is ready. Nobody waits
locally; the cost is finality, which is background. **For @design-reviewer:** at two members the quorum is both,
so finality on the frames from `activate_at` waits for the slower build. The merge past the window (Q3b) stays in
M2 with WS-diverge.

### 10.2 M2

As the plan's M2, with the items left above and the toy of the plan (button, bin, score).

### 10.3 Acceptance tests (M1 unless marked)

- **Determinism:** the same module and inputs give the same rows, fuel left and fault points on Windows and Linux
  (all four platforms from M2).
- **Metering:** an endless loop faults by fuel at the same instruction count on every run; a deep recursion faults
  by depth at the same depth, under two different `max_wasm_stack` settings and on both CPUs.
- **Instance reset:** a script that keeps a counter in a static gives the same result with and without random
  rollbacks (because the counter is reset every frame), and the SDK documents why statics are useless.
- **Faults:** each trap kind of 3.6 discards the call, marks the binding, is identical after a replay, and never
  panics the process.
- **Validation:** modules importing anything outside `pe`, using a refused feature, or defining the injected names
  are refused with a structured error.
- **Activation:** the author runs the change from `activate_at` with no pause; B's root equals A's at the first
  cut after B's build; B never loads a module it did not build; a stale base is refused with the winner named.
- **No C:** the native audit passes with pe-logic in the tree on every gate platform.

## 11. Measurements before building, with the bench slot

Each gets a raw log under `crates/pe-logic/bench/` or the workstream's evidence folder, and its command.

1. **Batched column I/O:** the benchmark's 6.4k-line guest re-shaped to systems over host-owned columns, at 10k
   entities, against the in-memory guest (1.1x-1.7x of native). The budget is 1 ms per tick (file 03). Done on a
   clean window (3.3a): +0.30-0.37 ms per frame from the ABI's parts.
2. **Injected metering** against wasmtime's fuel, on that guest and on physics-in-wasm.
3. **Per-frame instance reset:** the cost of resetting a 128 KiB-1 MiB instance per frame, by wasmtime's
   copy-on-write image and by a plain copy, on Windows and Linux.
4. **Load time:** build, instrument, Cranelift compile and instantiate for a small script (the M1 toy), against the
   2 s window.
5. **The Unix patch:** done on Linux for build and traps (5.2c), and the native audit passes on the branch with no
   allow line (`spikes/out/iter-bench/logic-v0/audit-check.log`); backtraces and macOS remain.
6. **No host linker:** done (5.2a).

## 12. Open points

- The unit of attachment for very many distinct copies (an edited copy of a script per object, file 03, defect 4):
  each copy is a script version and a module; whether a thousand small modules fit in memory and compile time is
  measured in M2 [I].
- Whether `pe_manifest` moves to a custom section.
- The fuel cost table and budgets per stage.
- Native declarative components ("drives", file 11, 3.5) are outside this spec: data the store evaluates natively,
  with no build. Who owns them is for the architect.

## Appendix A. The M1 lamp script, as it would read against pe-sdk v0 [I, a sketch]

A sketch to fix the shape, not the final macro syntax. Everything is `macro_rules!`, so a player needs no host
linker (5.2a). The `Light` component is the engine's; `Blink` is declared by the script.

```rust
#![no_std]
#![forbid(unsafe_code)]
use pe_sdk::prelude::*;

pe_sdk::component! {
    /// Saved in the world, visible in the inspector and to MCP. Ids derive from the script asset id (6.1).
    pub struct Blink { period: f32, phase: f32, near: f32 }
}

pe_sdk::system! {
    /// Runs once per frame, before the physics, over every object bound to this script that has both.
    fn pulse(stage = pre_physics, me: (read Blink, write Light), near: query_near(Ball, 2.0)) {
        let t = ctx.time_s() + me.blink.phase;
        let on = (t / me.blink.period).fract() < 0.5;
        // Offsets from the lamp, in its own space (Offset<Own>); no absolute position is ever handed over.
        let ball_near = near.iter().any(|b| b.offset.within(me.blink.near));
        me.light.color = if ball_near { RED } else if on { WARM } else { OFF };
    }
}

pe_sdk::script!(components: [Blink], systems: [pulse]);
```

What the agent's edit in the M1 demo changes: the body of `pulse` (the `ball_near` line is the edit, "pulse red
when a ball is near"). The schema is unchanged, so the commit keeps `Blink`'s id and rows (6.1), and nothing
migrates.
