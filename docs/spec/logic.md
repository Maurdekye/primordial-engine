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

A **script** is an asset: Rust source, stored as one content-addressed blob in the world model (4.1a), attached to
an object by giving it the script's **tag**, a zero-size component every script has (3.2). Each machine compiles it with the pinned toolchain into a wasm module, then
runs a deterministic **instrumentation pass** that injects fuel and call-depth metering into the module's bytes.
The module exports **systems**. Each system declares the component types it reads and writes. Once per frame the
engine runs each system once over every object that carries the script's tag and the components it binds, handing it the matching rows in one batch and taking
back the rows it wrote. **Nothing in the module's memory survives a frame:** the instance is reset to its initial
image before each frame, so all lasting state is component data in the store, and rollback, hashing, saving and
merging of logic are the store's (file 11, 3.5; file 03, A1). A code commit is an ordinary edit entry with a base
version and an agreed frame.

## 2. Units and names

| Term | What it is |
|---|---|
| **Script** | An asset: a source blob (one crate, 4.1a) plus its declared manifest. Identified by an `AssetId`; each commit makes a new `ScriptVersion` (the hash of its source blob) |
| **Module** | The wasm bytes a machine built from one script version, after instrumentation. Local to the machine: never sent, never stored in the world (Q3c). Its hash is reported for comparison only |
| **System** | One exported function of a module, with a declared signature (section 3.2) |
| **Tag** | The zero-size component each script version has implicitly, named `pe.tag`, its id made like its other components' (6.1). Attaching a script to an object is adding its tag; detaching is removing it, and the script's other rows stay. A system runs over the objects that carry the tag and every component it binds. An object may carry any number of scripts' tags (@coordinator-opus, @fable-reviewer's shape) |
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
  describes them [V `crates/pe-sim/src/registry.rs:80-172`]. Their ids are assigned by the world model from the
  script version's content hash (6.1), so two scripts cannot collide on a name;
- implicitly, not in the manifest: the script's **tag** (section 2). Every system binds it as well as its declared
  components, without the module seeing it: the runner joins the tag's column into each system's bound objects and
  never copies it in (`Loaded::bind` takes the tag's id after the declared ones) [V `tests/run.rs`,
  `only_objects_carrying_the_tag_run_the_script...`]. A system may therefore bind only engine components and still
  runs only where the script is attached;
- **engine components** it binds, named `pe.<Name>` (`pe_sdk::engine::{Light, Transform}`, byte-for-byte copies of the
  engine's; a script's own names are identifiers, so the two cannot collide). Scripts may write engine components
  (@coordinator-opus): the world exports a table of the engine components scripts may bind, each with
  its id and a "scripts may write" mark (world-model.md 3.2: `Transform`, `Light`, `Appearance` writable). `Loaded::bind`
  refuses a `pe.` name the table leaves out, a layout that differs from the registry's description (size, and each
  field's name, kind, offset and count), and a `write` the mark does not allow; reading any listed one is allowed.
  v0's SDK ships `Light` and `Transform`; `Appearance`, writable in the table, comes when a script needs it (its
  `BlobRef` field has no SDK kind yet) (@design-reviewer)
  [V `tests/run.rs`, `binding_checks_the_engine_table_and_the_layout`, `the_sdk_engine_components_are_the_engine_layouts`];
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
- **Engine rows a script wrote** pass the world's edit checks as well as the store's field check: `StageCtx::validate`
  (pe-world's `World::check_script_row`: the edit rules, held pose, no author, since attaching the tag carried the
  owner's consent). With no check given, a call that writes an engine row faults (fail closed; @shader-reviewer).
  A refusal faults the call as `InvalidRow` with the world's reason, and nothing is clamped
  (@coordinator-opus) [V `tests/run.rs`, `an_engine_row_the_world_refuses_faults_the_call`,
  `a_script_writes_an_engine_component_only_where_it_is_attached`].
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
  logic stages; the new object is a root with the one component the record carries, which must be one of the script's own (v0:
  an engine row on a new object would need the world's check before the object exists; a create of one faults the
  call), and the script's **tag** (below)
  [V `tests/run.rs`, `what_a_script_creates_carries_its_tag`].
  Events go to the stage's report with the emitting object, for the events stage.
- **A script destroys only objects carrying its tag** (@coordinator-opus, replacing the spawn mark of; [V `tests/run.rs`, `a_script_destroys_only_objects_carrying_its_tag`]):
  - What the script created carries its tag, and so does every object an editor attached it to. A destroy of an
    object without the tag faults the call. Giving someone's object one of the script's components (an edit can)
    does not make it the script's: the tag decides. **Adding or removing a script's tag needs the same right as
    editing the object** (world-model.md 7.9), so a script cannot be attached to someone else's object to gain the
    destroy right over it.
  - The tag belongs to one version (6.1); a commit re-keys every row of the old version, tags included, so a new
    version can destroy what an earlier one created.
  - A script's destroy goes through `SimStore::destroy` (the runner undoes a refused destroy and every command of a
    faulting call by restoring the store), and the runner reports every object it removed (`StageReport::removed`)
    as well as the orphans. The world then writes a tombstone for each removed object from its stage-start state,
    so `Undelete` and the delete-order rule hold for script destroys too (@coordinator-opus;
    world-model.md 9.2a).
  - **Holds and locks win:** the world passes a guard; a destroy that would **remove** an object the guard names is
    refused, reported in `StageReport::refused`, and is not a fault (a player grabbing a spawn must not kill its
    script). An untagged descendant is orphaned, not removed, so a hold on it (or on what is beneath it) does not
    stop the destroy (@design-reviewer) [V `tests/run.rs`, `a_hold_on_an_orphan_does_not_stop...`].
  - **Copy, paste and import keep the tag** (it is an ordinary row): a copy runs the script, and the script may
    destroy it. That reverses the spawn mark's rule (copies dropped the mark), and is what a copier usually wants;
    detaching the copy is removing its tag (@design-reviewer).
  - **The orphan rule** (world-model.md 7.5): objects beneath the destroyed one that do not carry the script's tag
    are not taken, nor is anything beneath them. Each surviving object whose parent is removed becomes a root in
    place, keeping its world pose and what is beneath it (`StageReport::orphaned`); an untagged object under a
    surviving parent stays where it is [V `tests/run.rs`, `an_untagged_object_under_a_surviving_parent...`,
    @shader-reviewer].
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
- The script is marked **faulted at frame F** in its `modules` row (6.1), keyed by asset like every fault, with the
  fault's kind as `runtime::Fault::code` numbers it (1 Fuel, 2 Depth, 3 Unreachable, 4 OutOfBounds, 5 DivideByZero,
  6 IntegerOverflow, 7 BadConversion, 8 IndirectCall, 9 BadPointer, 10 Status, 11 InvalidRow, 12 BadCommand,
  13 Other; never renumbered; `HostStack` has no code, being local) [V `tests/runtime.rs`, `fault_codes...`]. That is world state:
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

### 4.1a The source: one blob, with its size in the row and a cap (R-L1)

For divergence.md 10's R-L1 (@diverge-eng; @coordinator-opus). The cap's number is decided
(@coordinator-opus); the rest is for @design-reviewer.
Without a size the repair can see, a dishonest source stretches a rung-2 repair's `T_module` wait up to a blob's
64 MiB (18.6 hours at the thin links' 1 KB/s). Both halves of R-L1, because each is cheap:

- **Size in the row: the source is one blob, not a tree.** A script version's source is a single blob, a canonical
  archive of the crate. The `modules` row and `CommitScript` already name it by `SimBlobRef`, hash and `u64` length
  (world-model.md 3.4, pe-world `ModuleRow::source`), so the row then gives the whole source's size before any byte
  is fetched. The length cannot lie: a fetch reads exactly that many bytes and checks them against the hash, so a
  peer that sends more or fewer fails the check like any corrupt blob. V, the version, stays the blob's hash.
  Divergence.md 4.2's per-level deadline (nodes naming their children's lengths) is then not needed: `T_module` is
  known from the rows alone.
- **The archive, so that the same crate gives the same V everywhere:** the bytes `"pe.src.v0"`, then for each file,
  in byte order of its path, `lp(path) ‖ lp(bytes)`, where `lp` is a `u32` little-endian length and the path is
  relative to the crate root and `/`-separated, with no duplicate. Only the crate's `src/**.rs` files are in it.
  **`Cargo.toml` is not** (@design-reviewer's Low on c66e6cd5): it is the one free-form file, and the cheapest way
  to commit a build fault on purpose (a path, git or registry dependency, a `build =` key, features, another
  edition). The build service writes it from one fixed template, with the package name `script`, edition 2024 and
  the engine's pe-sdk as the only dependency, so 4.1's manifest rules hold by construction. The build service unpacks it into a fresh folder and
  builds there (4.2), so no path of the author's machine enters the module. **Refused at apply** (@coordinator-opus, option A of @world-model's): one pure function, `pe_world::script::check_source(&[u8])`,
  checks the format and the path rule below. `CommitScript` calls it on the source, which under the cap always rides
  inside the entry, and refuses an archive that is not canonical (bad magic or lengths, unsorted, a duplicate, a
  path outside the rules) as `Invalid`; the old version keeps running. pe-logic's build service calls the same
  function before unpacking. Refusing at build instead would make a malformed commit a failed build on every
  machine, voided only once enough voters had built and posted `BuildFailed` that the quorum was unreachable (6.2a item 4),
  rather than refused at once.
- **Paths that unpack the same on every filesystem** (@design-reviewer). Otherwise one V could build
  different crates: `src/A.rs` and `src/a.rs` are one file on Windows and on macOS by default, names that differ
  only by Unicode normalization collide on macOS, and Windows refuses or rewrites reserved names and some
  characters. So every path is `src/` followed by directory segments and a file segment. A directory segment is 1 to 64
  bytes of `[a-z0-9_]`; a file segment is a stem of 1 to 61 bytes of `[a-z0-9_]` and then `.rs`, 64 bytes at most
  with the extension (no bare `.rs`). A whole path is at most 128 bytes and 8 segments, `src` included, so that
  unpacked under the build service's folder it stays well inside Windows's 260-character `MAX_PATH` and no
  machine's build fails on its OS alone (@design-reviewer's Medium on world/source-check 1babd1b7); the build
  service keeps its own folder short enough for that [I]. `src/lib.rs` must be present and every file must be
  UTF-8, since without either no machine could build it (@world-model's additions, approved by @design-reviewer).
  pe-world's constants are the rule: `MAX_PATH`, `MAX_SEGMENTS`, `MAX_SEGMENT`, `path_ok`. No
  segment's stem may be a Windows reserved name (`con`, `prn`, `aux`, `nul`, `com0`-`com9`, `lpt0`-`lpt9`; @chunks-reviewer's L1). That is
  also the shape a Rust module name takes, so no real script loses anything [I]. ASCII only and lower case only,
  so no two allowed paths collide after case folding or normalization.
- **A cap: `MAX_SOURCE_BYTES` = 256 KiB** in v0, `pe_world::script::MAX_SOURCE_BYTES` (@world-model). `CommitScript` with a longer `source.len` is refused `TooLarge { limit }`, checked first, from the
  length alone, without opening the blob; pe-logic's build service imports the same constant. It bounds the wait
  for one module at `W_fg + 262 s + T_build` = 284 s with divergence.md's `R_thin` = 1 KB/s and `T_build` = 20 s. And it keeps every source under the 1 MiB limit
  for simulation blobs that travel inside their edit entry (world-model.md 3.4), so a machine that received the
  commit entry has the source. A machine that gets the `modules` rows through an import (a rung-2 repair) and not
  the entry still fetches the blob by hash; that is the wait `T_module` covers, sized from the rows' lengths
  (@diverge-eng).
- **The cost on a thin link [I]** (@design-reviewer's Low): a source inside its commit entry crosses the author's
  link before the author's later entries, so a 256 KiB commit at 1 KB/s delays the author's own chain by about
  262 s, and their hand entries meanwhile arrive late or void. The 1 MiB in-entry rule already allowed this; the cap
  makes 256 KiB the largest case. The alternative is to send sources on the bulk lane by reference, as blobs over
  1 MiB already go, so that the entry stays small and only the build waits. For @coordinator-opus to decide whether
  M1 cares; M1's edit is the 1.2 KB lamp (about 1 s).

Sizes, crate `Cargo.toml` plus `src/**.rs`, so slightly above the archive's [V, `wc -c` on 2026-11-13]:

| script | bytes | fits 256 KiB | at 1 KB/s (1,000 B/s) |
|---|---|---|---|
| lamp (pe-logic tests) | 1,213 | yes | 1.2 s |
| tally (pe-logic tests) | 6,881 | yes | 7 s |
| benchmark guest, 6.4k lines (`spikes/iter-bench/guests/logic_mono`) | 188,993 | yes | 189 s |
| benchmark guest, about 18k lines (`guests-x3`) | 518,019 | no | 518 s |
| benchmark guest, about 45k lines (`guests-x8`) | 1,342,178 | no | 1,342 s |

**Decided: 256 KiB** (@coordinator-opus), to be revisited if real games hit it; a larger game
splits into several scripts. 256 KiB holds about 8,900 lines at the 6.4k-line guest's 29.5 bytes per line, much more than M1's
agent-authored edit needs (the lamp is 1.2 KB). It still leaves the slowest honest single-module repair at under
5 minutes on the thinnest link. The alternatives considered:
- **64 KiB:** a 88 s worst case, but it refuses the 6.4k-line benchmark guest, our only measured
  "realistic" logic.
- **1 MiB:** about 35,000 lines, with an 18-minute worst case. It is the in-entry limit, so nothing larger could
  travel inside a commit anyway.

A script bigger than the cap is split into several scripts, or waits for a later version of this rule (M2: a
per-world setting, or a source that depends on a shared library asset). **Built:** pe-world has the format,
the cap and the check (`MAX_SOURCE_BYTES`, `check_source`, `source_files`, `pack_source`; @world-model,
world/source-check, main c3962919), and `CommitScript` refuses a non-canonical archive at apply. pe-logic's build
service packs a crate's `src` with `build::pack`, refusing a file the archive cannot carry rather than skipping it,
and builds an archive with `build::build_archive`: it checks it with the same `source_files`, unpacks it into a
fresh folder beside the template's `Cargo.toml`, and builds there [V `tests/archive.rs`: the lamp packs canonically
and builds to the same bytes unpacked at two depths; a `.txt`, an upper-case name, a hyphen and a non-UTF-8 file are
refused; a non-canonical archive is refused before anything is written; a work folder the service did not write is
never deleted].

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
  Linux, `scripts/iter_bench_xos.sh`, below); and it runs in the landing gate (with @provenance-eng). Its edit-time cost, measured in a clean window
  on 2026-11-13 on the `tally` script: 0.347 s per edit with incremental off against 0.265 s with it on, +0.08 s
  [V `docs/iteration-benchmark.md` 3.4]; loading the built module adds about 5 ms (metering 0.9 ms, Cranelift
  4.1 ms).
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
  covered by the remaps; it is refused by the dep-info check (6.2b), on every machine alike. (Before the
  build-asymmetry ruling, the module-hash comparison reported it as a build fault, which @coordinator-opus had
  accepted; under the ruling a build only some machines pass splits the session, 6.2 step 3.) The flags grow with the file count: 301 files
  make 114,652 characters on Windows, past the 32,767 a Windows variable is said to hold, and the build still works
  and gives the same module (std passes its own environment block; cargo moves a long rustc command line into an
  argument file) [V `tests/lamp.rs`, `a_script_with_many_files_builds_the_same_module`; @shader-reviewer's Low,
  refuted by measurement]. Alternative not taken (same ruling: not worth leaving the pinned stable toolchain): rustc's
  `-Z location-detail=none` drops file names from panic locations altogether, but is unstable (needs
  `RUSTC_BOOTSTRAP` on the pinned stable toolchain).
- The build runs as a child process at idle priority, one at a time per machine, in its own target directory per
  world.
- **The author builds what the others build** (@shader-reviewer's Low 2 on v6-r1): the author's module, the
  `author_module` its commit names, is `build::build_archive(build::pack(dir))`, the archive it commits built from a
  fresh folder with the template's `Cargo.toml`, and never `build::build(dir)` on its own crate, whose package name
  and manifest are the author's. Every other machine builds the archive, so any other path risks a module hash
  nobody else reproduces. Today the two give the same bytes (the lamp, 9,473 bytes: the package name does not reach
  the stripped module), and `tests/archive.rs`, `a_direct_build_of_the_authors_crate_differs_only_as_pinned_here`,
  pins that, so a change shows up [V]. `build::pack` also refuses a symlink or junction under `src`, and anything it
  cannot read, which the author's own build would otherwise still see (@shader-reviewer's Low 1) [V the same file,
  `pack_refuses_a_link_under_src`, with a junction on Windows].
- **Reproducibility is a requirement, not an assumption:** two machines' builds of one source must give the same
  bytes, or their states differ and the hash tree finds it. And every member must be able to build every script:
  a member whose build fails splits off (6.2 step 3, the user's ruling), so 6.2b's prevention keeps
  a build a function of the source and the build identity alone. Shown so far for six dependency-free modules on two
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
     differing module a failed build, `Mismatch`).
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
    source: SimBlobRef,         // the source blob (4.1a): hash and length, at most MAX_SOURCE_BYTES
    toolchain: Hash,            // D75
    sdk: Version,
    author_module: Hash,        // the author's built module, for comparison only, never fetched (Q3c)
    activate_at: Frame,         // the agreed frame: equal to the entry's own frame stamp (WS-logic's rule;
                                // world-model refuses anything earlier than the stamp)
}
```

The world's `modules` table holds one row per script asset: the version (the source blob as a `SimBlobRef`, 4.1a), the
toolchain and SDK hashes, `author_module` as a plain hash, the declared components, `activate_at` and the faulted
mark. A compiled module is never a blob and never in world state (Q3c). Component ids, the tag's included, are
`u128::from_le_bytes(BLAKE3("pe.component" ‖ V ‖ name ‖ schema hash)[..16])`, where **V is the content hash of one
script version** (its source blob's hash; @coordinator-opus, world-model.md 3.2). A commit therefore
**re-keys** every row of the old version's components, tags included, to the new version's ids, in the commit's own
transaction, so no frame sees half-moved rows (world-model.md 7.9; fields map by name and kind). The same operation
on one copy is a future per-copy fork. The runner never hashes ids: the world hands them to `Loaded::bind`.
The `modules` row stays keyed by the stable asset id (`run::ScriptKey`), which orders scripts within a stage and
keys faults. Objects a system spawns get `BLAKE3("pe.spawn" ‖ world ‖ spawner ‖ frame ‖ n)`,
where `n` counts the spawner's *applied* spawns in that frame (a discarded call does not advance it).

- **Conflict rule:** a precondition edit (file 11, 3.2). A second commit on the same base is refused with the
  reason and the winning version; the author has the 10 s seat to rebase (R7).
- **Attaching a script** to an object is a plain edit that adds the script's tag, under the object's edit right; it
  needs no compile if the machine already has that version's module.
- The new version, the activation frame and which objects carry which tag are **world state**, inside the hash tree,
  so two machines that disagree on which code ran find it at the next cut.

### 6.2 What each machine does

1. **The author** builds and instruments first, then commits stamped with its current frame, and runs the new
   module from that frame at once. It never waits. Until the commit is final it is provisional; if it is refused
   (stale base, no permission) the author's view rolls back.
2. **Every other machine**, on receiving the entry:
   - fetches the source blobs, starts its own build, and keeps simulating with the old module as a prediction
     (Q3a: objects keep running the old code). At `activate_at` the world re-keys the script's rows to the new
     version's ids (6.1), so the old module is bound **under ids derived from the new version's V**, with the new
     tag: every component whose schema the edit left alone has the same id either way, so a logic-only edit keeps
     running the old code on the re-keyed rows, then jumps when the build is ready (@coordinator-opus). **Known limit of Q3a:** if the edit changed a component's schema, the old module cannot bind
     under the new ids (its declared description is not registered there), and the script pauses on that machine
     until its build is ready, about 0.5-1.3 s for a small script [I, 6.3]. Either way those frames are predictions,
     replayed from `activate_at` [V `tests/activation.rs`, `a_logic_only_edit_keeps_running...`,
     `a_schema_edit_pauses...`; red: bound under the old version's ids, the logic-only case froze];
   - **does not vote for any cut at or past `activate_at`** until its build is ready, because it cannot yet
     compute those frames (needs S3, section 9);
   - when the build is ready and the module validates: if the agreed frame is within the replay window (a
     duration in seconds, 2 s by default, never a frame count; the tick rate is per world, 60 Hz by default), it rolls
     back to `activate_at` and replays with the new module. Its objects jump once to the new behaviour.
3. **Every member builds what its line activates; a slow build lags, a failed one splits off** (user rulings; @coordinator-opus).
   A machine never runs a module unless all of these hold:
   - its build identity (6.2b) is the session's, and its toolchain hash and SDK hash equal the row's;
   - the build succeeded within the memory cap (6.2a item 5), read nothing outside its archive and pe-sdk (the
     dep-info check, 6.2b), and the module passed validation and instrumentation;
   - its manifest equals the one the author's build declared (the declared components in the row);
   - its module hash equals `author_module`. The hash is of the metered bytes, which are what runs
     (`runtime::Script::module_hash`, `pe_core::Hash::of("pe.logic.module", metered)`), so it covers the pass's version too.

   - **A slow build is never a failure** ("no limit"). A machine whose build is still running lags
     for as long as the build takes: it keeps predicting with the old module, and it does not vote for a cut at or
     past `F` until it has the module. Before the deadline `D` it simply has not attested (6.2a). No timer stops a
     slow compiler, no deadline splits a slow machine, and slowness is never posted as a failure.
   - **A failed build** is posted as `BuildFailed` (6.2a item 2) with its reason, and only after the local retries
     of 6.2b (a `Local` failure is retried twice, a `Mismatch` rebuilt once from a clean folder), since under the
     ruling posting it can cost the member the session. If its line activates the version anyway (a quorum
     attested), a member that failed to build it **splits off at once** (Q2 (a)): no grace period. A member that fell
     behind and fails to build a version already active does the same. The split is the member's own leave, posted
     by its replica (world-model.md 2.4; in M1 it leaves the session): pe-world records the failure and never removes
     a member itself (@world-model). If the line voids the version, nobody splits.
   - **A joining machine** builds every active version before it becomes a member, with no deadline.
     A joiner whose build of an active version fails does not become a member, and is told why.
   - **The author** (Q1 (c)): when its commit is voided, it is told who could not build it and why (`BuildFailed`'s
     reason, or `Unattested`); its source stays intact and re-committable. It may then choose to leave and carry on
     with the script on a line of its own, where it re-commits as the only voter (in M1: end the session). Nothing
     splits automatically over a voided commit.
   - **No machine needs a compiler-less path:** every member must run the build service (Q3; no watch-only mode).

   If the author's build is the odd one (a module nobody reproduces, or a source that does not compile), no quorum
   attests and 6.2a voids the commit, at the deadline or as soon as the quorum is unreachable.

From the replica's side a pending module is the same thing as a late entry: the frames since `activate_at` are a
prediction until it arrives [I; file 13, A.4].

### 6.2a Activation by attestation

**The problem** (@coordinator-opus): with activation at the commit's own frame, one author with
commit rights can stop finality for everyone. A source that does not compile, or an `author_module` hash the author
did not build from that source, is a build fault on every other machine (6.2 step 3); none of them can compute
frames past `activate_at`, so no cut past it reaches a quorum. 4.1a's `check_source` removes the cheapest forms (a
malformed archive, a hostile `Cargo.toml`); a compile error in a `.rs` file remains. **Requirements:** no single
author can stop finality, and the outcome is identical on every machine.

**The rule** (@fable-reviewer's attestation, with a deadline, as @design-reviewer suggested):

1. **A commit is pending, not active.** `CommitScript` writes the new version into the asset's modules row as
   *pending* (V, hashes, declared components, the commit's frame `c`, its deadline, and **the voter list at `c`**,
   snapshotted from the world's governance members; `q` comes from its length). It re-keys nothing yet. Every
   machine keeps running the current version. The re-key's limits (`RekeyBlocked`, `MAX_REKEY_ROWS`) are checked at
   the commit, which is refused as today, and again at `F` (item 3). Rulings on pe-world's points:
   @coordinator-opus, adopting @world-model. In M1 every member votes, and genesis checks it:
   the world's governance members must equal pe-agree's `NodeConfig` members, refused by name otherwise
   (@engine-architect's `Genesis::check`). A voter bit for later epochs is a future seam
   (@engine-architect, @world-model).
2. **Attestations are entries.** A voting member whose build of the pending version passes all of 6.2 step 3's
   checks against the row posts `AttestBuild { script, version: V, module: Hash }`, an entry stamped with its frame
   like any other. It needs no edit right, only voter status at `c`. The commit counts as its author's
   attestation, stamped `c`, if the author is in the voter list.
   **A failed build is an entry too** (6.2 step 3; pe-world: @world-model, reviewer @chunks-reviewer): after its
   local retries a voter posts `BuildFailed { script, version: V, reason, toolchain, sdk, module: Option<Hash> }`.
   The reason is an enum, `Compile`, `Refused` (6.2b's dep-info check), `Cap`, `Mismatch` or `Local`, plus the
   hashes, **never compiler output**: the entry goes to every member and stays in the log, and a rustc message can
   carry local paths. One per voter, counted against the quorum like an attestation, in these orders: a later
   success by the same voter supersedes its failure while the commit is pending; a failure after a success is
   refused; nothing changes a void. A failure for a version that is already active is accepted as a record only:
   it is the log's statement that this member split off (6.2 step 3), which the notices read.
3. **Activation frame `F`:** the least frame `f` such that at least `q` distinct voters at `c` have a counted
   attestation stamped at or before `f` (agreement.md: `t = floor((n-1)/3)`, `q = floor((n+t)/2) + 1`, over the
   voters at `c`); that is, the `q`-th smallest stamp among the counted attestations, one per voter
   (@design-reviewer's Medium: by stamps, not by log order, which need not be monotone in stamps).
   The world performs the activation **in `end_frame(F)`**, after all of `F`'s entries and both of its logic stages:
   the re-key of 6.1 and the row's switch from pending to current, in one transaction, so the new code runs from
   `F + 1` and the order of entries inside `F` does not matter. If the re-key at `F` is blocked or too large, the
   commit is voided there instead, with that fault (`RekeyBlockedAtActivation` or `RekeyTooLarge`, distinct from
   the source cap's `TooLarge`) and the frame on the row, and the old version stays. The author's `EDIT` right is
   re-checked at `F` too: if the author no longer holds it, the commit is voided with `AuthorRevokedAtActivation`,
   so revoking someone's rights also stops their pending code (@coordinator-opus). **A known limit:** any member
   holding an affected object can so void someone else's commit anywhere in `c..F`, not only at `c` as before
   (@design-reviewer, adopted). So the state at a frame depends only on entries stamped at or before it, and every machine computes
   the same `F` from the final log. An epoch change between `c` and `F` does not reset the count; `F` may fall in
   the next epoch.
   **Which attestations count:** its version is the row's pending V; its module equals the registered
   `author_module` (a different hash counts for nothing and is logged as evidence); its stamp is at or after `c`;
   its author is a voter at `c` (a non-voter's is ignored); one per voter; none once the commit is active or void;
   a late one is void by the ordinary rule. One that does not count is refused with a reason (`Stale` when its
   version is not, or no longer, the pending V; `Invalid` for another module hash, a non-voter's, or a voter's
   second), and its signed entry stays in the final log as evidence of who claimed what.
4. **The deadline:** if no frame `f <= D = c + ceil(2 T_module * hz)` qualifies as `F`, the world voids the commit in
   `end_frame(D)`: the row's pending part is cleared, the current version stays bound, and the row records the fault
   (`Unattested`, with the frame) for the author. `end_frame(f)` tests activation first, then the void if `f == D`,
   so an attestation stamped at `D` that completes the quorum activates. `T_module = W_fg + source.len / R_thin +
   T_build` is divergence.md 4.2's wait for one module, computed from the row's length (4.1a), so a larger source
   gets a longer deadline: the lamp 23 s, so 46 s; a 256 KiB source 284 s, so 568 s [I; the factor 2 is a margin,
   accepted by @coordinator-opus]. The constants are divergence.md's: `W_fg` = 2 s, `R_thin` = 1,000
   B/s, `T_build` = 20 s; pe-world mirrors them in integer milliseconds. A machine still building at `D` has simply
   not attested: the deadline voids the commit, never the machine (6.2 step 3). At n = 2 a missing attestation
   therefore voids at `D` as `Unattested`, and Q1 (c) applies.
   **An unreachable quorum voids at once** (the shared void rule, for `BuildFailed` and for Leave; @coordinator-opus, @fable-reviewer): in `end_frame(f)`, after the activation test and before the deadline test, the
   commit is voided as `QuorumUnreachable` when the counted attestations, plus the voters at `c` who are still
   members and have neither attested nor failed, number fewer than `q`. At n = 2 the first `BuildFailed` voids at
   once. A leave (world-model.md 2.4) keeps the voters at `c`, an attestation the leaver gave before leaving still
   counts, and the leaver's own pending commit voids at activation as `AuthorRevokedAtActivation` (@world-model). **This is a policy, not a necessity:** a failure can be superseded by a later success, and
   voiding early gives that chance up for a fast, certain answer that is the same on every machine.
5. **One memory budget everywhere; no time limit** (Q4 (b) and; @design-reviewer's Low 2, adopted by
   @coordinator-opus, for the memory cap). The build service runs every build under **1 GiB peak
   committed memory** for the whole build (cargo, rustc and rust-lld together, a Windows job object's
   `JobMemoryLimit`, a cgroup's `memory.max` on Linux [I]), with the job count pinned (6.2b) so the cap measures the
   same build on every machine, and a machine attests only a build that stayed within it. Otherwise a source crafted
   to build only on big machines could activate with the help of `t` faulty attesters while smaller honest machines
   cannot follow. **The cap is a margin, not a removal** (@fable-reviewer): Windows and Linux count memory
   differently, so a source sitting at 1 GiB can pass on one machine and fail on another [I]. **There is no
   wall-time cap:** a slow build lags (6.2 step 3). **The author's margins** keep honest scripts far from both: the
   editor refuses to commit a script whose own build took over 15 s or peaked over 512 MiB on the author's machine,
   and says why. They protect honest authors only. Measured on this desktop, clean builds with the service's flags,
   n=3, median [min-max] [V shared machine, idle priority, `spikes/out/iter-bench/build-budget/`,
   `scripts/iter_bench_build_budget.py`]:

   | guest | source | peak, whole build | peak, largest process | wall |
   |---|---|---|---|---|
   | lamp (with pe-sdk) | 780 B | 105.6 MiB [105.6-105.9] | 99.6 MiB | 0.52 s [0.51-0.60] |
   | 6.4k-line guest (`logic_mono`) | 188,839 B | 148.5 MiB [148.5-149.1] | 142.3 MiB | 1.28 s [1.25-1.56] |

   1 GiB is about 7 times the 6.4k-line guest's peak. Peak memory moves with the job count (the 6.4k-line guest
   142.8, 151.4 and 157.1 MiB at 1, 4 and 16 jobs; the module's bytes do not move) [V
   `spikes/out/iter-bench/build-asym/`]. Neither the cap nor the margins are built yet.
6. **A second commit while one is pending** is refused as `Stale` with the pending entry as `winner`: the author
   waits for activation or the void [I; the simplest v0 rule, kept for 6.5].

**Why it meets the requirements [I]:**
- Before `F`, every machine runs the current version, which they all have, so finality never waits on the new
  build. After `F`, at least `q` voters have the module, so a quorum can compute and vote. A slow minority holds its
  own vote until it has built; a member whose build fails splits off (6.2 step 3).
- `q` attestations include at least `q - t >= t + 1` honest members (`q = 3`, `t = 1` at `n = 4`), so the module was
  built by an honest member. `t` faulty members attesting to a module nobody can build cannot reach `q`.
- A module nobody reproduces is never attested, so it never activates; the deadline or the unreachable quorum
  voids it, and the void is a function of the final log alone, identical everywhere.

**What changes for the author: (i), provisional, pending the user** (@coordinator-opus, asking the
user through @liaison-opus, since it reads Q3 differently). Q3 has the
agreed frame equal to the commit's stamp, with the author running the new code at once. Under this rule, the agreed
frame is `F`, after a quorum has built. Two ways to show it to the author:
- **(i) Run at once, as a prediction** (built to, provisionally): the author's machine runs the new module from `c`, as now, and
  when `F` is known, replays from `c`: the current code until `F`, the new code after. The author sees the edit at
  once, then one correction, of about the time from `c` to `F`. For the lamp that is the quorum's build plus an
  entry's latency, about 0.5-2 s [I: 0.35 s build, 6.3]. Other machines replay from `F`, as they replay from
  `activate_at` today.
- **(ii) Wait for `F`:** no correction, but the edit shows only after the quorum has built, about 0.5-2 s later.

The lamp's `c` to `F` is to be measured on this desktop and on the second PC once attestation is built (pe-world
and pe-replica); the 0.5-2 s above is inferred.

**Not chosen:**
- **A deadline alone,** with activation at `c`: it voids a module a slow quorum would have built, and finality
  still stops until the deadline.
- **Voiding retroactively,** keeping activation at `c` unless the deadline passes without a quorum: the state at
  frames after `c` would then depend on entries stamped after them, which no other part of the world does.

**Needs from others:**
- @world-model: the pending row, `AttestBuild`, activation and the re-key at `F`, and the void at the deadline
  (built, main c3962919 and 1d401fde). Under the build-asymmetry rulings: `BuildFailed`, the shared void rule
  (`QuorumUnreachable`), the void notice to the author (Q1 (c)), and the split of a member that fails an activated
  version (Q2 (a); in M1 a leave).
- @engine-architect (S3): the voter set at `c` and `q` for the count.
- @diverge-eng: `T_module` from the row, which 4.1a already provides.

The runner needs nothing new: the lagging binding of 6.2 step 2 applies from `F` instead of `activate_at`.

### 6.2b Prevention: a build is a function of its source and the build identity

Under 6.2 step 3 a build that one member passes and another fails splits the session, so the build service makes
a build depend on nothing but the version V and the **build identity B**, and the session compares B at join.
Then a failure is the same on every machine, 6.2a voids the commit, and nobody splits. The analysis, with its
measurements: the build-asymmetry draft, section 3 (@iter-bench and @engine-architect, revised to
@fable-reviewer's view). **Not built yet:** @iter-bench builds it in slices, reviewed by @shader-reviewer.

- **B** = the toolchain's identity (the release, rustc commit and LLVM version, plus the hash of the wasm32 std
  component; D75), pe-sdk's source hash, a build-service version (template, profile, flags, remaps; bumped like
  `RESULT_VERSION`) and the metering pass's version. pe-app's session `BUILD` carries it, so genesis refuses a
  member whose B differs, by name. Players on different engine versions therefore cannot play together, as today.
- **The toolchain by absolute path:** the service runs the pinned toolchain's own `cargo` and `rustc` (`CARGO`,
  `RUSTC`), never a rustup proxy, with `RUSTUP_TOOLCHAIN` removed. Measured: the service's command through the
  proxy, from a folder outside the repo, silently built with another toolchain (this PC's default, nightly 1.94)
  and gave other module hashes; by absolute path, the pinned toolchain and a minimal copy of it gave the expected
  hashes [V `scripts/iter_bench_toolchain_min.py`, `spikes/out/iter-bench/toolchain/`]. The service's check that a toolchain
  folder holds no `rustup` is only a heuristic (a proxy or a package manager's shim copied elsewhere passes it); the
  actual guard is D75's pinned toolchain hash, checked before the toolchain is used (@shader-reviewer, on v7-r1). The minimal set is
  439.8 MiB on disk, 100.5 MiB as an xz download (Windows x86-64; the full rustup install is 1,333 MiB) [V].
- **The child's environment:** cleared, then a fixed list (the temp folder, `SYSTEMROOT` on Windows) [I]. Its
  working directory is the unpacked crate, `CARGO_HOME` a folder the service owns, and `build_archive` refuses a
  work folder with a cargo configuration (`.cargo/config.toml`) in any folder above it. A `rust-toolchain` file
  there can no longer matter, since no proxy runs. Measured: an environment
  variable read by `option_env!`, and an `[env]` entry in a `.cargo/config.toml` in the working folder, each
  changed the module hash [V `scripts/iter_bench_build_asym.py`, `spikes/out/iter-bench/build-asym/`]. Inside the
  archive neither can occur: 4.1a's paths allow only `src/**.rs`.
- **The dep-info check:** after the build, the service reads rustc's dep-info files and refuses the module
  (`Refused`) if any file it lists lies outside the unpacked `src/` (or pe-sdk's `src/` for pe-sdk) or any
  `env-dep` line appears. rustc records every file a crate read (`include!`, `include_str!`, `include_bytes!`,
  `#[path]` modules) and every variable it asked for, set or not [V `build-asym/depinfo.log`]. It refuses what a
  script tried to read, not what it got, so it refuses on every machine.
- **The job count pinned** (4 [I]): the bytes do not depend on it, peak memory does (item 5).
- **Deterministic compile limits:** `-F long_running_const_eval`, so a const-eval loop is the same compile error
  everywhere; a raised `#![recursion_limit]` or `#![type_length_limit]` is refused [I].
- **Retries before `BuildFailed`:** a `Local` failure (no compiler diagnostic: disk, antivirus, a killed process)
  twice; a `Mismatch` once, from a clean folder and an empty target directory.
- **A bug bundle:** a `Compile` or `Refused` failure on a version another member attested, or a `Mismatch` that
  survives the clean rebuild, means prevention has a gap. The machine saves the archive, B, both module hashes,
  the full compiler output and dep-info, the toolchain's file hashes and the OS and CPU locally, and tells the
  user where. Nothing of it goes into the entry.
- **Permanent tests** (`tests/build_isolation.rs`, each with a red control): an environment variable, a cargo
  configuration and a `rust-toolchain` file around the build leave the hash unchanged; `option_env!`, `env!`, an
  outside `include_bytes!` and a `#[path]` module outside `src` are `Refused`; 1 and 4 jobs give the same hash.
- **Open, for @coordinator-opus [I]:** saved worlds after an engine update whose B differs (their rows name a
  toolchain no member has). The draft recommends re-committing each script with the same source and the new
  toolchain, attested as usual, and fetching the old toolchain by hash only for a script that fails to migrate.
  And a canary in B (each install's module hashes for the lamp and `tally`), so a host whose compiler emits other
  bytes is refused at join.

What prevention leaves: a local failure that survives its retries, the memory cap's margin, a host whose
compiler differs in a way not yet measured (only Windows and Linux on one x86-64 PC so far [V]), a compiler crash
on one host, and a dishonest author aiming at a limit. Those split a member off, as ruled.

### 6.3 Costs, from measured numbers

Frame counts below are at the default 60 Hz, for illustration only; every limit is in seconds.

| Build on that machine | Frames to replay at 60 Hz | Inside a 2 s window |
|---|---|---|
| 0.45 s (small edit, incremental, 6.4k lines) [V benchmark] | 27 | Yes |
| 0.35 s (a script edit, incremental off, `tally`) [V benchmark 3.4] | 21 | Yes |
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
| N4 | WS-world | `CommitScript` (agreed, world-model r7) as an edit payload with a precondition; each version's tag (3.2) and the re-key of 6.1; the source as one blob of at most `MAX_SOURCE_BYTES`, refused `TooLarge` above it (4.1a); script-scoped component ids; a `faulted` mark in the `modules` row (3.6); a `Pos` field kind | 3.2, 3.6, 6.1, 8 |
| N5 | WS-world | Component descriptions registered at run time, versioned by frame (already planned: plan 2.1) | A commit can add components |
| N6 | WS-state | The stage order of 3.5 | Logic before and after physics |
| N7 | WS-api | `code.check` (build and validate, diagnostics as data), `code.commit`, and for M2 `code.test`. WS-logic provides the backend calls; WS-api the MCP tools | The agent's edit in M1 |
| N8 | WS-seams (root manifest) | A path and a `[patch.crates-io]` line for the patched wasmtime | 5.4 |
| N9 | WS-ci | `audit_native.py`'s allowlist covers wasmtime's tree with the patch, on Linux and macOS | 5.4 |
| N11 | WS-state | The floating-point guard (FTZ/DAZ, rounding) runs on the thread that runs wasm, at every entry into a guest (5.2b) | Non-NaN results depend on it |
| N10 | WS-render, S5 | Events emitted by scripts reach the frame view once, not once per replay (plan M1: effects fire once) | 3.3 step 3 |
| N12 | WS-world (pe-sim) | Raw column access by `ComponentId`: the sorted ids and the row bytes, read-only. **Done:** `SimStore::column_raw`, main 18b60fde; the runner uses it | The bound-object join and the 256 B block write-back of 3.3a |
| N13 | WS-world (pe-world) | Where a script's components are registered in a live world (the store's registry is fixed at `SimStore::new`), and the runner's hook in the replica's two logic stages, with its report turned into `Effects` (created, destroyed, written rows). No spawn mark (replaced by the tag); tag add and remove under the object's edit right. The world's destroy keeps its effects in the store, since the runner undoes by restoring the store | 3.3, 3.5, 3.6, N1, N5 |

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
- **Faults:** each trap kind of 3.6 discards the call, marks the script's modules row, is identical after a replay, and never
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
    /// Saved in the world, visible in the inspector and to MCP. Ids derive from the version's content hash (6.1).
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
