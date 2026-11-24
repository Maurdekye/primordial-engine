//! pe-world (WS-world, @world-model). The world model: the object tree, components and blobs, the hash tree, edit
//! entries and their conflict rules, governance validity.
//!
//! Specification: docs/spec/world-model.md. The seams it implements or uses are in `pe_seams`.
//!
//! * [`components`]: the engine's components for M1 (3.2) and their registry.
//! * [`World`]: one world's state (5.1), with the trusted construction calls.
//! * [`WorldSnapshot`]: the published, read-only view of one frame that WorldApi reads (9.4).
//! * [`edit`]: the edit payload and its canonical codec (7.1); [`World::apply_edit`] applies one (7.2-7.7).
//! * [`preset`]: M1's disposable ball pit, and member admission ([`World::admit`]).
//! * [`tree`]: the hash tree (6): object records, the world tables, the trie, and the S2 `Tree` calls.
//! * [`input`]: the input payload (8.1); [`World::apply_input`] applies one, and [`World::end_frame`] expires claims.

mod apply;
pub mod branch;
pub mod components;
pub mod edit;
mod import;
pub mod inline;
pub mod input;
mod leave;
pub mod player;
pub mod preset;
mod read;
mod replica;
pub mod script;
mod source;
pub mod templates;
#[cfg(feature = "test-util")]
pub mod test_util;
pub mod tree;
mod world;

pub use apply::{ActivationResult, Blocked, Draft, Footprint, FrameEnd, Push, Thrown, Voided};
pub use components::BlobRef;
pub use input::{HandPose, Input, InputSettings, input_phase};
pub use leave::LeaveReport;
pub use pe_seams::Version;
pub use player::{BodyHandPose, MoveSample, PlayerRow};
pub use read::{ObjectInfo, Value, WorldSnapshot};
pub use replica::{GRAVITY, Settings};
pub use script::{
    BuildFailure, EngineComponent, LogicReport, ModuleRow, PendingCommit, RekeyReport, VoidKind, script_component_id,
};
pub use tree::WorldTree;
pub use world::{
    BlobStore, Claim, DEPTH_LIMIT, DEPTH_LIMIT_MAX, GENESIS, TOMBSTONE_SECONDS, Tombstone, World, WorldError, perm,
};
