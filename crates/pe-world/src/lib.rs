//! pe-world (WS-world, @world-model). The world model: the object tree, components and blobs, the hash tree, edit
//! entries and their conflict rules, governance validity.
//!
//! Specification: docs/spec/world-model.md. The seams it implements or uses are in `pe_seams`.
//!
//! * [`components`]: the engine's components for M1 (3.2) and their registry.
//! * [`World`]: one world's state (5.1), with the trusted construction calls.
//! * [`WorldSnapshot`]: the published, read-only view of one frame that WorldApi reads (9.4).

pub mod components;
mod read;
mod world;

pub use components::BlobRef;
pub use pe_seams::Version;
pub use read::{ObjectInfo, Value, WorldSnapshot};
pub use world::{BlobStore, Claim, DEPTH_LIMIT, DEPTH_LIMIT_MAX, GENESIS, World, WorldError};
