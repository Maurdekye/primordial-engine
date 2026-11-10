//! pe-replica (WS-state, @vdw-physics). The replica (seam S2): one domain's deterministic state machine with its
//! rollback loop, over world-model bodies; the frame view (S5).
//!
//! Specification: docs/spec/replica.md. The seams it implements or uses are in `pe_seams`.
//!
//! - [`drive`]: the S2/S3 glue of each simulation tick (pe-seams ruling 3; app.md 3), against the seam traits only.
//! - [`world`]: what the replica needs from pe-world (replica.md 8.1), until pe-world is written.
//! - [`ring`]: the keyframes (replica.md 4.1).
//! - [`mock`]: a small world model for tests and the harness, until pe-world exists.

pub mod drive;
pub mod mock;
pub mod ring;
pub mod world;

pub use drive::{DriveReport, Driver};
pub use world::WorldModel;
