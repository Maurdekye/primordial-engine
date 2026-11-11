//! Primordial Engine sim store (design 16, 6.1; package P2-1, D15: our own store, not bevy_ecs).
//!
//! * [`registry`]: component descriptions. Every component is a fixed-size row of plain data whose
//!   fields are described at runtime (name, kind, offset), so native components and components declared
//!   by WASM logic modules (16.5) are stored, validated, snapshotted and hashed the same way.
//! * [`SimStore`]: the objects of one world and one sorted column per component. Columns are sorted by
//!   [`ObjectId`](pe_core::ObjectId), so iteration order is a function of the state alone (4.3 rule 4).
//!   Columns are copy-on-write: a [`Snapshot`] shares them, and the first write after a snapshot copies
//!   only the columns it touches.
//! * [`hierarchy`]: parents and ordered children, kept in the built-in [`Node`] component.
//! * [`transform`]: the built-in [`Transform`] component (`f64` position, `f32` rotation and scale,
//!   roots placed by chunk: 25.2) and deterministic world-transform propagation.
//! * [`schedule`]: the fixed-step tick: an ordered list of stateless system functions.
//!
//! Snapshot, restore, serialization and the state hash are in [`snapshot`]. The hash covers the frame,
//! the object table and every non-empty column with its schema hash.

mod column;
pub mod hierarchy;
pub mod registry;
pub mod schedule;
pub mod snapshot;
mod store;
pub mod transform;

pub use column::RowVersion;
pub use hierarchy::Node;
pub use registry::{Component, ComponentDesc, ComponentId, FieldDesc, FieldKind, Registry};
pub use schedule::{Schedule, SystemFn, TickCtx};
pub use snapshot::Snapshot;
pub use store::SimStore;
pub use transform::{Transform, WorldTransform};
