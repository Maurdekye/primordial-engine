//! pe-replica (WS-state, @vdw-physics). The replica (seam S2): one domain's deterministic state machine with its
//! rollback loop, over world-model bodies; the frame view (S5).
//!
//! Specification: docs/spec/replica.md. The seams it implements or uses are in `pe_seams`.
//!
//! - [`drive`]: the S2/S3 glue of each simulation tick (pe-seams ruling 3; app.md 3), against the seam traits only.
//! - [`world`]: what the replica needs from the world model (replica.md 8.1), and
//!   [`apply_order`], the one order in which a frame's entries apply (by [`Phase`], then agreed order).
//! - [`ring`]: the keyframes (replica.md 4.1).
//! - [`replica`]: the replica itself, [`ReplicaImpl`], over a [`WorldModel`] and pe-physics; [`bodies`] is its
//!   physics glue.
//! - [`players`]: player bodies (world-model.md 8.2), each walked by pe-physics's mover once a frame before the
//!   hands, and the view's rig and heads drawn from them.
//! - [`mock`]: a small world model for tests and the harness.
//! - [`peworld`]: pe-world's `World` as the replica's [`WorldModel`], [`PeWorld`].

pub mod bodies;
pub mod drive;
pub mod hands;
pub mod logic;
pub mod mock;
pub mod peworld;
pub mod players;
pub mod replica;
pub mod ring;
pub mod world;

pub use drive::{DriveReport, Driver};
pub use logic::{ScriptRunner, event_kind};
pub use peworld::PeWorld;
pub use replica::{CorrectedInteraction, InteractionChecks, ReplicaConfig, ReplicaImpl};
pub use world::{Phase, WorldModel, apply_order};
