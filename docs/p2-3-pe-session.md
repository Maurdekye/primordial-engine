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

Every run: `SUMMARY: every scenario ALL EQUAL, no panic, 0 false checks, 0 non-finite states` [V];
22 unit and integration tests in pe-session [V].

**Still to do:** the rest of S5 (the joiner's state transfer, accepted only against a check hash a
quorum signed, P22S-1; the geometry-registry compaction P22-12 in the keyframe ring, on @vdw-physics's
`compact_images`), P2-4 rounds 4 and 5, P2-9's scenario files on this core, Linux runs.

## Engine state on pe-sim (S5)

* Object index `i` (the `obj` of a `Touch` or `Hand`) is the store object and the physics body
  `object_id(i)` = `ObjectId(0, i + 1)`. Indices are never reused: `obj_slots()` is the store's
  creator counter. Components 16 to 19 (`session/store.rs`): `ObjRow`, `ClaimRow`, `CooldownRow`,
  `HandIdle`. Departing destroys the object's rows (measured alone first: 69 of 69 logs identical).
* Reads: `live(i)`, `objects()`, `obj_slots()`, `claim(i)`, `claims()`; `WorldSim::store` for the rest.
* The world-level tables (escrow, arrivals, provenance, portal) stay a plain struct, `Tables`.

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
