//! Primordial Engine physics (design 23.4, P2-2): the engine's only door to our box3d fork.
//!
//! * [`PhysicsWorld`]: one Box3D world. Bodies are created from engine descriptions
//!   ([`BodyDesc`], [`ShapeDesc`]) and bound to sim objects ([`ObjectId`]); the binding table is
//!   sorted, so iteration order is the same on every peer.
//! * Interactions: [`Push`] (impulses, forces, torques) and joints ([`JointDesc`] under
//!   caller-chosen [`JointKey`]s, including grabs, D72).
//! * World images ([`WorldImage`]): save and restore in place (rollback). Transfers
//!   ([`Transfer`]): a fresh world for a joining peer, restored only if its full-state check
//!   hash matches the confirmed one (design 9.1), since the donor is untrusted.
//! * [`PhysicsConfig`] / [`engine_world_def`]: the engine's world definition, contact recycling
//!   included, and the worker-thread count (D2: bit-identical for every count).
//! * Several worlds per process: each `PhysicsWorld` owns its Box3D world and its threads.
//!   Box3D's length units are process-wide; the engine uses metres.
//! * Sim store: [`PhysicsWorld::write_root_transforms`] copies body poses into `pe_sim`
//!   `Transform`s.
//!
//! Determinism: all inputs are validated and bounded ([`limits`]; no NaN or infinity enters
//! Box3D, 4.3 rule 6), and every table is ordered. [`PhysicsWorld::state_hash`] is the
//! per-frame hash (poses, velocities, tables); [`PhysicsWorld::check_hash`] covers the whole
//! state.
//!
//! Feature `raw` exposes `PhysicsWorld::raw_mut`, for test scenes and tools only.

mod config;
mod desc;
mod error;
mod image;
mod world;

pub use config::{engine_world_def, PhysicsConfig, SUBSTEPS};
pub use desc::{limits, BodyDesc, BodyKind, Geometry, ShapeDesc};
pub use error::PhysicsError;
pub use image::{Transfer, WorldImage};
pub use pe_core::ids::ObjectId;
pub use world::{GrabSpring, JointDesc, JointKey, JointKind, PhysicsWorld, Pose, Push};
