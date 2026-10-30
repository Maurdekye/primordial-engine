# P2-1: core and sim store (`pe-core`, `pe-sim`)

Author: @world-chunks. Package P2-1 of design 23.4. Branch `p2/p2-1` from main bacfa43. Reviewer:
@design-reviewer.

Tags: **[V]** measured or tested here, **[I]** inferred or proposed.

## What is built

### `crates/pe-core`

Depends only on `bytemuck`.

| Module | What it holds |
|---|---|
| `ids` | `MemberId` (with `LOGIC` = 0) and `Frame(u64)`. `ObjectId { creator: u32, counter: u32 }`, ordered by creator then counter; counters start at 1, so `ObjectId::NONE` is (0, 0). `WorldId(u32)` and `GlobalId { birth_world, object }` (R14, P2-5) |
| `clock` | `TickRate`: whole hertz, frame boundaries in integer microseconds, so 90 Hz has no drift. `FrameClock`: `frame = floor((session_us - epoch_us) * hz / 1e6)` (design 5). `SessionClock`: local monotonic time to session time through an offset that slews by at most 1% and never runs backwards (5, 10.4) |
| `dmath` | Bit-exact `f32` `sin`, `cos`, `sin_cos`, `tan`, `atan`, `atan2`, `asin`, `acos`, `exp`, `ln`, `log2`, `pow` and `powi` (4.3 rule 3). Each evaluates an `f64` kernel (fdlibm polynomials) and converts the result to `f32`; only IEEE `+ - * /`, `sqrt`, `floor`, integer arithmetic and conversions, with no libm calls. Within 1 ulp of `std` over the tested ranges [V]. `sin`/`cos`/`tan` reduce exactly for every finite `f32`, up to `f32::MAX` (a 98-bit window of 2/pi, computed with integer arithmetic). `asin` and `acos` clamp to [-1, 1], so rounding drift cannot make NaN. `pow(1, y)` and `pow(x, 0)` are 1, as in C99 |
| `rng` | Counter-based streams (4.3 rule 2): SplitMix64 keyed by (session seed, stream). The state is one counter, so it is seekable and snapshots as 16 bytes. `Rng::for_frame(seed, stream, frame)` gives a fresh stream per frame. `below(n)` is unbiased (Lemire) |
| `hash` | XXH64, pinned in-tree and checked against the reference crate (`twox-hash`, dev-dependency only) [V]. Streaming and one-shot. It replaces the spikes' byte-wise FNV-1a for state hashes, because it is several times faster on whole columns |
| `coords` | The R16 canonical coordinates (25.2): `WorldPos` = `i64` chunk + `f64` offset, and `CellKey` levels. Ported from `spikes/chunks` with its tests. `normalize` never panics: chunk indices saturate at +-2^62 |
| `fpenv` | `check_and_reset()` (4.3 rule 9): compares the whole control register with the platform default (MXCSR 0x1F80 ignoring sticky flags on x86-64; FPCR 0 on aarch64, which covers FZ, DN, the rounding mode, traps and FEAT_AFP's FIZ/AH/NEP) and restores it. Returns whether something had changed it. Tested by setting flush-to-zero [V, x86-64; aarch64 not run] |
| `Error` | Shared, deterministic error variants |

The crate refuses to compile on big-endian targets, because sim state is hashed and copied as raw little-endian
bytes.

### `crates/pe-sim`

Depends on `pe-core` and `bytemuck`.

* **Components are described at runtime** (`ComponentDesc`). Each is a list of fields (name, kind, offset,
  count) that must cover the row exactly:
  * no gaps or overlaps;
  * natural alignment, and the row a multiple of its alignment.

  So no row has padding bytes, and every row is safe to hash and copy as bytes. A struct that needs padding
  declares it as a field (`Transform::_pad`).
  * Native components implement `Component` (a `Pod` struct plus its description, checked against the struct
    size). Logic modules' components (16.5) use the same descriptions through the raw API. The descriptions
    are the IDL that MCP schemas can come from (17.2).
  * `schema_hash` covers the whole description. Snapshots record it, and restoring into a different schema
    fails.
  * Layout arithmetic is checked and rows are capped at 64 KiB, so no description can overflow. Runtime
    descriptions (`register`) cannot take the engine's ids below 1024; engine crates use `register_engine`.
* **Values are validated at the boundary.** `insert` and `insert_raw` refuse NaN, infinities and bools other
  than 0 or 1 (4.3 rule 6). Mutable access (`get_mut`, `column_mut`) is not validated. `check_values()` scans
  everything, at check frames (4.3 rule 9).
* **The store** (`SimStore`) holds:
  * the frame and the session seed;
  * the sorted live objects;
  * each creator's last counter, so ids are never reissued, even after a destroy or a rollback;
  * one column per component, sorted by `ObjectId` (4.3 rule 4).

  Columns are `Arc` buffers with copy-on-write.
* **`checked_state_hash()`** runs `check_values()` first, so a stray NaN is a deterministic error on every
  peer rather than a hash of platform-specific NaN bits. Sync checks use it.
* **The hierarchy** (16.2) is the built-in `Node` component: parent, first and last child, and previous and next
  sibling. It is an intrusive linked list with fixed 40-byte rows, so it snapshots and hashes like any column.
  * Only the hierarchy API writes it.
  * Operations: `set_parent(child, parent, index)` refuses cycles; `destroy` removes the whole subtree.
  * Traversal: roots in id order, then depth-first with children in list order.
* **`Transform`** (80 bytes): `f64` position, `f32` quaternion rotation and `f32` scale (25.2).
  * An object's *frame* is its nearest ancestor with a `Transform`. An object without one is placed by its
    `i64` chunk plus an `f64` offset from that chunk's corner, and inserting it rebases the offset into the
    chunk exactly; an object with a frame is placed relative to it, with `chunk` zero.
  * **Limits**, checked on insert, by `check_values` and on loading bytes:
    * `|pos| <= 2^50` m, and a root's `|chunk| <= 2^61`;
    * every scale component non-zero, with magnitude in [2^-40, 2^40];
    * the rotation's squared norm within 2% of 1;
    * `_pad` zero.
  * **Hierarchy edits keep world transforms.** `set_parent`, and inserting or removing an ancestor's
    `Transform`, rewrite the affected local transforms for their new frame, with the root chunk recomputed
    exactly. An edit whose rewrite would break the limits is refused and changes nothing. `rebase_roots`
    (also a system) moves roots edited through `get_mut` back inside their chunks.
  * `world_transform(id)` and `world_transforms()` compose in a fixed IEEE order and give the same bits [V].
    Composition normalizes rotations and clamps scales, and positions saturate, so propagation never panics
    or makes NaN, even from transforms written through `get_mut`. World transforms are derived data, never
    stored or hashed.
* **Snapshots** (6.1):
  * `snapshot()` shares the buffers, at O(columns) cost. The first write to a column afterwards copies that
    column once; columns that are not written are never copied. This is 6.1's "memcpy of columns; dirty
    tracking", at column granularity.
  * `restore()` puts the buffers back, also O(columns).
  * `to_bytes()` and `from_bytes()` are the canonical little-endian serialization for state transfer (9.5).
    `from_bytes` validates everything, because the bytes may come from another peer:
    * sorted ids and the creator counters;
    * the schemas, sizes and values, including the `Transform` limits and frame rule;
    * lengths that fit `usize` (so 32- and 64-bit peers accept the same bytes) and a frame below 2^62;
    * the hierarchy, which must be a consistent forest with no cycles.

    Every check is O(n log n) in the objects, so a hostile donor cannot stall a joiner.
* **`state_hash()`** (9.1) is XXH64 over the frame, the seed, the objects, the counters and every non-empty column
  with its schema hash. It equals the hash of a snapshot of the same state and of its serialized round trip.
  State reached through different operation orders hashes and serializes identically [V].
* **The tick** (`Schedule`, design 5) runs, in order:
  1. the floating-point environment check;
  2. the frame's inputs, through a closure from the caller (the session's entries in P2-3);
  3. the systems, in order;
  4. the frame advance.

  Systems are plain `fn` pointers, so they cannot keep state outside the store. `TickCtx::rng(stream)` gives
  each frame's randomness without RNG state.

## Evidence

| Check | Result |
|---|---|
| Unit and integration tests | 31: `pe-core` 17, `pe-sim` unit 3, store 5, determinism 6 [V] |
| Snapshot and restore round trip | Bit-exact, both in place and through `to_bytes`/`from_bytes` into a fresh store: identical hashes, bytes and world-transform bits [V] |
| Rollback re-simulation | A 600-frame scripted world at 90 Hz: objects spawn, move, spin, reparent, lose components and die, driven only by RNG streams, with `dmath` in the systems, a runtime-described component and a three-creator id space. It was restored at frames 100, 250 and 599 and re-simulated to 600, in place and from bytes. Every frame's hash equals the original run's [V] |
| Golden digests, cross-platform | Equal on Windows x64 debug, Windows x64 release and Linux x64 (static musl, release, WSL2 kernel 6.6) [V]. `dmath` = 0xf1cf7f9547a5d079; scenario trace = 0x8effd667bd4fa407; world transforms = 0x18c584b928ecf125. Logs: `spikes/out/p2-1/`; rerun: `scripts/run_p2_1_tests.sh --linux` |
| wasm32 | `pe-core` and `pe-sim` compile for `wasm32-unknown-unknown` [V]. The goldens have not been run there |
| Corrupt input | Truncated, extended, unsorted, wrong-magic and wrong-schema bytes, and a broken hierarchy (cycles, loops, orphans, missing links) are refused. A failed restore leaves the store unchanged [V] |

**Snapshot cost** (`cargo run -p pe-sim --release --example store_bench`): two runs at low priority while the
user was in VR, so timings are noisy (logs `spikes/out/p2-1/store_bench-*.txt`) [V]. Each object has a node,
a Transform and a 16 B velocity; half also have a 16 B runtime component; a third are children.

| Objects | Snapshot | Restore | First write of all Transforms after a snapshot (copy-on-write) | Deep copy of everything | Hash | Serialized size | `from_bytes` |
|---|---|---|---|---|---|---|---|
| 256 | 0.1 us | 0.4 us | 0.2-0.4 us | 1.3-1.4 us | 2.2-2.7 us | 46 KB | 40-42 us |
| 1,024 | 0.1 us | 0.2-0.3 us | 1.2-20 us | 40-41 us | 7.6-8.2 us | 184 KB | 203-204 us |
| 2,048 | 0.1 us | 0.2 us | 2.1-2.6 us | 64-83 us | 16 us | 369 KB | 373-492 us |
| 8,192 | 0.1 us | 0.2 us | 14 us | 41-270 us | 62-65 us | 1.5 MB | 2.0 ms |

* **Against the spike (6.2):** the spike's snapshot saves a Box3D image: 0.25 ms at 256 bodies, 0.95-1.2 ms at
  1,024 and 2.2-2.7 ms at 2,048. The sim store's deep copy of every column is about 3% of that at 2,048
  objects, and a snapshot without copying is 0.1 us. **No worse than the spike** [V]. Physics images are
  P2-2's.
* `from_bytes` is the slowest path (about 0.25 us per object), because it validates untrusted input. It runs
  on state transfer, not per frame.

## Review of 911e759 (@design-reviewer), and what changed

| Finding | Fix |
|---|---|
| P21-1 High: `from_bytes`' cycle check was quadratic (12.6 s for a 16k chain) | A walk down from the roots counts what it reaches: O(n). A test loads a 20,000-deep chain |
| P21-2 High: a finite transform (chunk [1,0,0], local 1e300) panicked every peer | Transform limits on insert, `check_values` and `from_bytes`. `normalize` saturates instead of panicking. Composition normalizes rotations and clamps scales |
| P21-3: component descriptions overflowed `u32` | Checked arithmetic, a 64 KiB row cap, `u32::try_from` |
| P21-4: `sin`/`cos`/`tan` went non-finite for huge inputs | Exact reduction for every finite `f32`. The accuracy test and the golden sweep now include large inputs up to `f32::MAX` |
| P21-5: incomplete ARM FPCR check; raw NaN bits hashed | Whole-register comparison with the default; `checked_state_hash()` |
| P21-6: reparenting kept local transforms, so objects teleported | Keep-world reparenting, and keep-world when an ancestor gains or loses a `Transform` |
| P21-7: root positions never rebased | Rebased on insert; `rebase_roots` for `get_mut` edits |
| P21-8: overflow behaviour differed between debug and release | `Frame` arithmetic is checked explicitly, so it panics in both builds; the clock saturates; RNG and hash counters wrap; frames above 2^62 are refused on load |
| P21-9: `u64 as usize` lengths in `from_bytes` | `usize::try_from` |
| P21-10: the script's cross-check exited before comparing | Compares only test logs, fails on any failed test or digest difference, and starts each log with the commit, the working-tree state and `rustc -vV` |
| P21-11: `range_f32` could return `hi`; `pow(1, inf)`; reserved ids; a trivial test | Fixed: `range_f32` stays below `hi`; `pow(1, y) = 1`; `register` refuses ids below 1024; the failed-restore test uses a populated store |

## Findings

* **`std`'s `f32::powi` changes with optimization.** On constant-folded inputs, a release build gave results
  2 ulps away from the debug build's. With `black_box`ed inputs both gave the same bits. LLVM folds `powi` with
  a different algorithm at compile time [V]. So 4.3 rule 3 (no `std` transcendentals in sim code) should name
  `powi` and `powf` too: use `dmath::powi` and `dmath::pow`.
* **Shared cargo cache.** At 21:53 another agent's online cargo run dropped the `windows` crate's index-cache
  entry, and every `--offline` build in the workspace failed until a refetch restored it a minute later [V].
  Concurrent agents sharing one cargo home can break each other's offline builds.

## Choices the design text should pick up [I]

These are for @engine-architect, who owns design.md:
* 16.1/16.2: `ObjectId` = (creator `u32`, counter `u32`), counters from 1 and never reissued; the hierarchy as a
  `Node` column; components described by a no-padding field list (the IDL of 16.5).
* 6.1: snapshots are copy-on-write by column, so "memcpy of columns" is paid only for columns written after
  the snapshot.
* 9.1: state hash = XXH64 over the canonical content (spikes used FNV-1a).
* 4.3: rule 3 extended to `powi`/`powf` (above); rule 9 implemented by `pe_core::fpenv`.
* 25.2: roots placed by chunk in `Transform`, children relative to their nearest transformed ancestor.

## Not in this package

* Keyframe rings, rollback scheduling and entry application belong to P2-3 (`pe-session`). This package
  supplies `snapshot`, `restore`, `state_hash` and the tick, which they call.
* The `ObjectId <-> BodyId` table and the physics images belong to P2-2 (`pe-physics`).
* Page-level copy-on-write and per-column hash caching would make large, partly written columns cheaper. They
  are not needed at these sizes [I].
* Linux ARM64 and macOS: not run. That is P2-6's matrix.
