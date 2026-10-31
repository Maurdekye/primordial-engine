# P2-3: `pe-session` port plan

Author: @world-chunks (implementer). Design owner: @engine-architect. Reviewer: @design-reviewer.
Item: @item:primordial-engine/p2-3-protocol-core-pe-session-on-pe-core-pe-sim-s. Branch `p2/session`, from
main 94bd341.

## Sources

* **Protocol of record:** branch `viewchange` at **b01ebdf**: `spikes/sim-core/src/session.rs` (3,742 lines:
  P2-5 plus P2-4), `provenance.rs` (484) and `lib.rs`. Harness: `spikes/session-sim`.
* **Equivalence baseline:** `spikes/out/session-sim/summary.txt` at b01ebdf: 44 logs, every replica equal to
  its offline replay. Final acceptance is against main once P2-4 lands.
* **Scenario runner:** P2-9's `tools/session-sim` scenario files (`p2/session-sim`, 2408089).
* **API:** `PeerCore` in `crates/pe-app/src/core.rs` at ab54526 (`p2/app`), and @engine-architect's
  amendments (msg md2828937159b): `SessionCore::{new, start, receive, tick, author, outbox, events,
  next_deadline}`, `Class::{Live, Control, Repair}`, the event enum, and `set_link` for R18.
* **Consumers to keep compiling:** pe-mcp's `DrivenPeer` and `sim_view.rs` (p2/app), and pe-app's
  `node.rs`.

## The approach: port first, then change, each step measured

Changing everything at once would make every digest differ, and prove nothing. So the port goes in stages.
Each stage ends with the 44 scenarios run through `pe-session`, and every digest either identical or
changed for a stated reason.

| Stage | What | Expected digests |
|---|---|---|
| S1 | **Structure-preserving port.** The spike's `WorldSim` split into modules (below), same algorithms, same hashing, same box3d calls, behind the new crate | All 44 identical |
| S2 | **Physics through `pe-physics`** (P2-2; it reproduces the spike's goldens). The direct box3d calls become `PhysicsWorld` calls | Identical, if P2-2's equivalence holds for these scenes |
| S3 | **`SignedEntry`**: Ed25519 over `"pe-entry" \| world id \| entry id`, verified before `insert`; entry id = SHA-256 of the canonical encoding (32 bytes) instead of FNV-64 | Changed only where an entry id enters a hashed table: R14 transfer ids and facts. Explained per scenario |
| S4 | **Grabs-only claims (D72, 8.4)** through `pe-physics` grab joints: only grabs claim, every other touch applies | Changed in scenarios with touches on claimed objects (`claims`, `ties`, ...): new outcome counts reported |
| S5 | **Engine state on `pe-sim`**: the object table, claims, cooldowns and hand state as components in a `SimStore`; keyframes are a `pe-sim` snapshot plus a `pe-physics` image; the state hash moves to XXH64 | Every digest changes (hash function); equivalence shown instead by per-frame body states and table contents matching S4's, frame for frame |
| S6 | **Sans-IO `SessionCore`**, the `Membership` trait with a static implementation, R18's `set_link`, and pe-mcp and pe-app adapters | Unchanged from S5 |

S1-S2 are mechanical but large. S3-S6 are the design changes, each small and reviewable on its own.

## Crate structure (`crates/pe-session`)

| Module | From the spike | Contents |
|---|---|---|
| `entry` | `Payload`, `Entry`, `Msg`, wire `Wr`/`Rd` | Entry types; room in `Payload` for membership entries (Vote, Propose/Prevote/Precommit, Join, Kick, roles) |
| `sign` | new (S3) | `SignedEntry`, key ring, the signing domain |
| `log` | chains, `by_author`, `contiguous`, validation, acks and horizons, `refs`, stability, confirmation | The chain log. Its only membership input is the `Membership` trait |
| `evict` | proofs, freeze and vote, `maybe_evict`, suspects, demotion | Equivocation and silence handling (P2-4's view change plugs in here) |
| `membership` | members, epoch, `epoch_at`, `members_at`, audience, cuts | `trait Membership` and `StaticMembership` |
| `fetch` | wants, `take_fetches`, `on_fetch`, backoff | Gap and conflict repair, yielding to a full uplink (Repair class) |
| `sim` | `Tables`, keyframe ring, snapshot policies, `step_frame`, `apply`, carried state, hands | The replica's simulation and rollback |
| `r14` | escrow, arrivals, verdicts, attestations, certificates, `provenance.rs` | Cross-world transfers with P2-5 provenance |
| `check` | check hashes, `evaluate_checks`, non-finite checks | Sync checks |
| `core` | pe-app's `PeerCore` | The sans-IO API (S6) |
| `replay` | `replay*`, `final_log`, `digest` | The offline reference |

## Open points for @engine-architect

1. **Base:** I branch from main and port from b01ebdf by hand, so P2-4 code arrives through the port, not a
   merge. When `viewchange` lands on main, I re-check the ported files against it.
2. **Entry ids:** SHA-256 (32 bytes, `sha2` is already in the lock through ed25519-dalek) rather than BLAKE3,
   whose build needs the `cc` build-dependency resolved offline. Same security for this use; say if you
   want BLAKE3.
3. **pe-sim for engine state (S5):** per-object tables (objects, claims, cooldowns, hands) become
   components. World-level tables (escrow, arrivals, `seen`) stay one plain struct, cloned into each
   keyframe: they are small and keyed by transfer and global id, not by object.
4. **Stage order:** S2 waits for P2-2 on main. If it is late, S3-S4 go first on the spike's box3d calls.
