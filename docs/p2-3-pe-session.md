# P2-3: pe-session, the protocol core on pe-core and pe-sim (status and evidence)

Branch `p2/session` (worktree `.worktrees/p2-3`). Plan: [p2-3-pe-session-plan.md](p2-3-pe-session-plan.md).
Docket: p2-3-protocol-core-pe-session-on-pe-core-pe-sim-s. Tags: **[V]** measured or verified by a run
named here, **[I]** inferred.

## Where it stands

| Stage | What | Commit | Evidence |
|---|---|---|---|
| S1 | `crates/pe-session`: the spike core split into modules (entry, membership, log, evict, fetch, check, r14, sim, replay), no logic change | 9491327 (at viewchange 758ff5e) | 56 of 56 logs identical to 758ff5e [V] |
| S3a | `SignedEntry`: Ed25519 (`verify_strict`) over `"pe-entry" \| world \| session \| entry id`, verified before insert, stored and relayed | ccde27c, ca38f2c (session id) | no digest changed; 8 logs differ only in byte counts, +64 B per entry message exactly [V] |
| S3b | Entry ids: SHA-256 of the canonical encoding, all 256 bits | 8668173 | 0 logs different once ids are masked; digests changed only in the 15 R14 logs (transfer ids key the escrow tables) [V] |
| S4 | Grabs-only claims (D72, 8.4); a touch carries only its own object's state (HO-1) | f813361 | scenario `grabrace`; with the old rule switched on the code reproduces the previous logs; across 49 scenarios exactly 562 previously refused pushes now apply, 640 touch outcomes unchanged (`scripts/compare_claim_rules.sh`) [V] |
| S3c | Ed25519 for R14 fact attestations and certificates, precommits (epoch certificates) and join attestations; `PublicKeys` for third-party checks | e66eb6f | 47 logs identical, 10 R14 logs equal with ids masked, 0 different [V] |
| P2-4 r3 | viewchange a9286fc ported by a per-module three-way merge (`scripts/p2-3/`) | 8ccbc1d | with the spike's claim rule: 37 identical, 8 bytes-only, 22 equal with ids masked, 1 differing only in a RESULT label [V] |
| S6a | `SessionCore`, the sans-IO core (`peer.rs`); the harness drives every peer through it | 70979f5 | 69 of 69 logs identical to the commit before [V] |
| S6b | `Membership` seam: trait, `ViewChangeMembership`, `StaticMembership` | 0bc32f3 | 69 of 69 identical [V] |
| S5a | The exchanged check hash is BLAKE3, 32 bytes (P22R-1, 9.1) | 669c293 | 41 identical, 8 bytes-only (+24 B per ack with a check, exactly), 20 equal with ids masked; digests changed only in the 15 R14 logs [V] |
| merge | `land/viewchange` 8ae7c08 (main's pe-physics, P22S-1 text) | 7455821 | 69 of 69 identical [V] |
| SET-1 | Every peer, the toucher included, writes a touch's carried state at its frame (SET-1, SET-2) | 8f033c8 | 61 identical, 8 equal with ids masked, 0 different; digests changed in 6 R14 logs [V] |
| S2 | The session sim's physics goes through `pe-physics` (impulses, hand targets, carried state with `set_state`, arrivals, keyframes as `WorldImage`); check hash over `pe_physics::check_hash` | c97b1d0 (on @vdw-physics's 9927b7b) | 51 identical, 18 equal with ids masked, 0 different; digests changed only in the 15 R14 logs [V] |
| S5b | The per-object engine tables (objects, grab claims, cooldowns, hand idle counters) are `pe-sim` components in `WorldSim::store`; a keyframe is a `WorldImage`, a store `Snapshot` and the world-level tables | 7fedcff | 69 of 69 identical (the hashes kept their formula) [V] |
| S5c | Per-frame state hash: XXH64 over `pe-physics`'s `state_hash`, the store's `checked_state_hash` and the world-level tables. Check hash: BLAKE3 over `pe_physics::check_hash`, the store's canonical serialization and the world-level tables | 044ce5d | 3 identical, 66 equal with ids masked, 0 different; with only digests and ids masked (no sorting) 63 equal, and the other 6 differ only in the order of id-keyed listings [V] |
| S5d | Geometry-registry compaction (P22-12) after each ring trim, from the keyframes the ring keeps (the oldest at or before min(conf, frame)) and the live state; `SimConfig::compact` (`Grown { at: 1024 }` by default) | 2304cd3, on @vdw-physics's `compact_images` 33cdd7e (in review) | with compaction after every trim (`PE_COMPACT=every`), 69 of 69 logs identical to the commit before, 0 false checks; tests: replicas on four snapshot policies, three compacting after every trim and rolling back, equal state hash every frame and check hash every check frame; a keyframe image kept past a compaction is refused (`BadImage`), nothing written [V] |
| S5e | Canonical encoding of the world-level tables (`encode_tables`/`decode_tables`); the check hash is `check_hash_of(physics, frame, store bytes, tables bytes)`, so it binds the exact bytes a transfer carries (the tables' earlier word projection was lossy) | 458c893 | 3 identical, 66 equal with ids masked, 0 different [V] |
| S5f | State transfer to a joiner: `export_state`, `trusted_check` (P22S-1), `import_state` | 8940e78 | 69 of 69 identical; tests: a joiner that restores member 1's state has member 2's state hash every frame after it; donor-only vouching, a flipped byte in each part, trailing bytes, a relabelled frame and another world are each refused with the replica unchanged [V] |
| S5g | `Msg::FetchState` / `Msg::State` and `SessionCore::request_state`, `Event::Transfer`; scenario `statejoin` | d3b221d | the 69 earlier logs identical; `statejoin`: a tampered state from member 2 refused (`Hash`), member 1's restored, frames 151..587 equal at all four replicas [V] |

Every run: `SUMMARY: every scenario ALL EQUAL, no panic, 0 false checks, 0 non-finite states` [V];
29 unit and integration tests in pe-session [V].

**Linux** (at 1404a0c; static musl builds in WSL2, kernel 6.6.87.2, `scripts/p2-3/run_linux.sh`): 29 of 29
tests pass, and every scenario's log equals the Windows log, timings masked (70 identical, SUMMARY ALL
EQUAL) [V].

**Still to do:** P2-4 rounds 4 to 6, P2-9's scenario files on this core.

## Engine state on pe-sim (S5)

* Object index `i` (the `obj` of a `Touch` or `Hand`) is the store object and the physics body
  `object_id(i)` = `ObjectId(0, i + 1)`. Indices are never reused: `obj_slots()` is the store's
  creator counter. Components 16 to 19 (`session/store.rs`): `ObjRow`, `ClaimRow`, `CooldownRow`,
  `HandIdle`. Departing destroys the object's rows (measured alone first: 69 of 69 logs identical).
* Reads: `live(i)`, `objects()`, `obj_slots()`, `claim(i)`, `claims()`; `WorldSim::store` for the rest.
* The world-level tables (escrow, arrivals, provenance, portal) stay a plain struct, `Tables`.

## State transfer (S5e to S5g; design 9.5, P22R-1, P22S-1)

* A `StateTransfer` is a world's state after a check frame, in three canonical parts: the
  `pe-physics` transfer, the store snapshot and the world-level tables. Its check hash is
  `check_hash_of` the parts, the same hash acks carry.
* A member keeps the parts of each check frame's state until a newer confirmed one supersedes it
  (`check_state` builds them anyway for the check hash, so the cost is memory: at most
  `window / check_every + 1` states, about 0.6 MB each at 256 objects [V for the size, I for the
  bound]). `export_state()` gives the newest confirmed one.
* `trusted_check(frame, donor)`: a check hash that a quorum of the members of the epoch active at that
  frame signed identically in their acks, at least one of them other than the donor. The joiner needs
  the log (the acks) first; with too few acks yet it refuses with `NoQuorum` and asks again later.
* `import_state(st, donor)` restores only if the world matches, a quorum vouches, every part is
  canonical and completely consumed (`Transfer::from_bytes`, `Snapshot::from_bytes` plus re-encoding,
  `decode_tables`) and the parts hash to the trusted check hash. The replica then continues from the
  next frame, with a floor no rollback goes below; a refusal changes nothing.
* On the wire: `Msg::FetchState`, and `Msg::State` as a repair (large; the IO may drop it, the joiner
  asks again). Transport chunking of a 0.5-1.7 MB message is phase 3's (10.x) [I].

## Geometry-registry compaction (S5d)

* In this harness's scenes the registry holds 2 blobs, so the default policy never compacts and a
  compaction drops nothing [V: `PE_CHECK_TIMING=1` prints the registry size]. Dropping geometry is
  covered by pe-physics's own tests (75 unique hulls) [V by @vdw-physics and @design-reviewer].
* Cost when forced after every trim: in `r14 ok`, 0.3-0.5 ms per compaction, about 540 compactions per
  replica over the run (in-situ, not a timing run) [V].
* The ring is the only holder of `WorldImage`s in pe-session; a joiner's transfer is a fresh
  `save_transfer`. An image kept anywhere else across a compaction is refused by `restore_image`
  (review CMP-1), never misread [V by test].

## Check hash cost (S5)

Per check frame, measured inside the scenarios (`PE_CHECK_TIMING=1`, release build, idle priority on
a shared machine: not a clean timing run) [V]:

| scenario | objects | check hash | physics part | store and world tables | physics transfer | store snapshot |
|---|---|---|---|---|---|---|
| `c1` | 256 | 415-493 µs | 392-466 µs | 22-28 µs | 547 kB | 20.6 kB |
| `ballpit --hz 90` | 502 | 1.39-1.40 ms | 1.35-1.36 ms | 39-40 µs | 1.73 MB | 40.3 kB |
| `r14 ok` | 76-107 | 148-224 µs | 135-208 µs | 13-16 µs | 148-206 kB | 6.2-8.7 kB |

The physics part, split at the end of `ballpit --hz 90`: `save_transfer` 534-540 µs, `to_bytes`
349-356 µs, BLAKE3 351-374 µs (4.6-4.9 GB/s). In `c1`: 162-164 µs, 62-73 µs, 110-111 µs [V].

* BLAKE3 is about a quarter of the check hash. The rest is the canonical save and its serialization, whose
  size is the cost driver: about 3.4 kB per body in the ball pit [V for the sizes, I for the reason:
  the transfer carries the full solver state].
* The store's part is about 3 % of the check hash [V].
* At one check frame in 30 (`SimConfig::check_every`), that is about 0.05 ms per sim frame at 502
  objects for a peer that never rolls back. A re-simulated check frame is hashed again: in the
  90 Hz ball pit, member 2 computed 111 check hashes over the run where member 1 computed 27, so about
  0.19 ms per frame [V for the counts, I for the per-frame averages].


## The API

* `peer::SessionCore::new(WorldSim)`. `WorldSim::new(world, me, members, scene, cfg, KeyRing)`;
  `.with_static_membership()` for M1.
* `receive(from, msg, now)`; `begin_tick(now, burst) -> Option<frame>`, then that frame's
  `author(payload, ahead)`, then `end_tick(now)` (or `tick(now)`); `outbox() -> Vec<Out { to, class,
  msg }>` with `Class::{Live, Control, Repair}` (Repair: fetch answers, which the IO may drop into a
  long queue); `events()`; `next_deadline()`; `set_offset(i64)` (clock sync); `set_link(queue_delay)`
  (R18).
* Time: local monotonic µs in; frames on session time (local + offset); timers on local time, so a
  clock-sync step never fires one (agreed with @engine-architect).
* `AuthorError::{NotReady, NotOwner, Stride}`.
* `KeyRing::new(secret, PublicKeys, SessionId)`; `KeyRing::test(me)` derives test identities.
* `session::Membership` (seam.rs): what the protocol core reads; `ViewChangeMembership` (`sim.vcm`)
  is what the view change writes.
* Objects: `live(i)`, `objects()`, `obj_slots()`, `claim(i)`, `claims()`, `WorldSim::store` (S5b).
* State transfer: `request_state(member)`, `Event::Transfer { from, frame, result }`;
  `WorldSim::{export_state, trusted_check, import_state}` (S5f, S5g).

## Wire cost of signatures and ids

Ball pit (`ballpit-60hz`, `ballpit-90hz`), per message, from the logs' payload counts [V]:

| | spike (b01ebdf) | S3a signatures | S3b 256-bit ids | S5a BLAKE3 check |
|---|---|---|---|---|
| hand pose | 45 B | 109 B | 133 B | 133 B |
| ack | 72 B | 136 B | 184 B | 208 B |
| all messages, 60 Hz (wire est.) | 16.9 kB/s | 26.4 kB/s | 31.7 kB/s | 33.4 kB/s |
| all messages, 90 Hz (wire est.) | 26.4 kB/s | 41.2 kB/s | 49.3 kB/s | 51.9 kB/s |

Per-entry signatures stay for P2-3 (correctness first; @engine-architect, 00:40Z). The expected
optimisation is in the transport (phase 3): one signature per datagram over each author's newest
entry in it, the hash chain authenticating the entries before it; entries that go into certificates
stay signed one by one [I].

CPU: `p2-4-vcscale-256` (256 replicas in one process, each verifying every entry) took 151 s of wall
time with signatures against 91-98 s without, on a shared machine (not a timing run) [V for the
numbers, I for any per-replica cost].

## Reproducing

* `scripts/run_session_sim.sh --pe` runs every scenario on pe-session (logs in
  `spikes/out/session-sim-pe/`).
* `scripts/compare_session_logs.sh [--ids] <ref>`: compares those logs with a reference's, timings
  masked; `BASE=spikes/out/session-sim-pe` compares with an earlier pe-session run; `--ids` masks entry
  ids, transfer ids and digests and sorts id-ordered listings (`scripts/session_log_norm.py`).
* `scripts/compare_claim_rules.sh`: S4's comparison.
* `scripts/p2-3/`: how a new spike round is merged into the modules.

## Follow-ups found on the way

* R14 facts and view-change values are still tagged 64-bit FNV hashes, so their Ed25519 signatures
  are only as strong as FNV: they should become SHA-256 or BLAKE3 digests; R14 fact signatures are not
  session-bound yet.
* `EpochCert.proofs` are unsigned entries (waits for the Membership trait's view-change port).
* `SimConfig::claim_rule` (the old rule, for S4's comparison) is removed after review.
* Spike-era harness behaviours kept for equivalence: fetch answers dropped at the IO above 100 ms of
  backlog; the background-world burst rule.
