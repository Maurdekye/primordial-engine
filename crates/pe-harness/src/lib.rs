//! pe-harness: the deterministic session harness of the re-founding (WS-harness).
//!
//! The design is `DESIGN.md` beside this crate's manifest. This is M0, step 1 (DESIGN.md 12): the parts that need
//! no seam yet.
//! - [`rng`]: every random draw is a pure function of (seed, stream, index, purpose).
//! - [`clock`]: global virtual time, the event queue and each node's local clock.
//! - [`net`]: directed links with delay, jitter, loss, duplication, a bottleneck with lanes, and partitions.
//! - [`kv`]: `KvReplica`, the small hashed state machine that stands in for seam S2.
//! - [`kvrep`]: that state machine behind S2 v1's `Replica`, for pe-agree's tests and the probes.
//! - [`trace`]: what every node did, stamped with global time; the checkers read only this.
//! - [`props`]: the property checkers, each with planted-violation tests (DESIGN.md 5, 6.2).
//! - [`sim`]: the runner over [`sim::NodeUnderTest`], and P2's offline replay.
//! - [`lockstep`]: the lockstep mock behind S3's `Agreement`, unanimous on purpose so P8 bites.
//! - [`seamnode`]: a node over the real seams (S3 + S2 + the drive loop), as the runner's `NodeUnderTest`.
//! - [`mock`]: `LockstepNode`, an old-style unanimous node that tests the harness until pe-agree exists.
//! - [`randsched`]: random fault schedules, pure in their seed, and a shrinker for the failing ones.
//! - [`pending`]: the register of tests waiting for a milestone, each expected to fail with one named property.
//!
//! When pe-seams v0 lands, `kv` implements S2's trait and `net` carries S4's lanes. The local stand-in types here
//! (ids, lanes, entries) are then replaced by the seam's.

pub mod clock;
pub mod kv;
pub mod kvrep;
pub mod lockstep;
pub mod mock;
pub mod net;
pub mod pending;
pub mod props;
pub mod randsched;
pub mod rng;
pub mod seamnode;
pub mod sim;
pub mod trace;
