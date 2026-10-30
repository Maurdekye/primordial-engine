//! Primordial Engine physics (design 23.4, P2-2): the engine's only door to our box3d fork.
//!
//! * [`PhysicsWorld`]: one Box3D world. Bodies are created from engine descriptions
//!   ([`BodyDesc`], [`ShapeDesc`]) and bound to sim objects ([`ObjectId`]); the binding table is
//!   sorted, so iteration order is the same on every peer.
//! * Interactions: [`Push`] (impulses, forces, torques) and joints ([`JointDesc`], including
//!   grabs, D72).
//! * World images ([`WorldImage`]): save, restore in place (rollback), and restore into a fresh
//!   world with its [`GeometryBlobs`] (late join, state transfer).
//! * [`PhysicsConfig`] / [`engine_world_def`]: the engine's world definition, contact recycling
//!   included, and the worker-thread count (D2: bit-identical for every count).
//! * Several worlds per process: each `PhysicsWorld` owns its Box3D world and its threads.
//!   Box3D's length units are process-wide; the engine uses metres.
//! * Sim store: [`PhysicsWorld::write_root_transforms`] copies body poses into `pe_sim`
//!   `Transform`s.
//!
//! Determinism: all inputs are validated (no NaN or infinity enters Box3D, 4.3 rule 6), and
//! every table is ordered. The state hash ([`PhysicsWorld::state_hash`]) covers Box3D's state
//! and the tables.

mod config;
mod desc;
mod error;
mod image;
mod world;

pub use config::{engine_world_def, PhysicsConfig, SUBSTEPS};
pub use desc::{BodyDesc, BodyKind, Geometry, ShapeDesc};
pub use error::PhysicsError;
pub use image::{GeometryBlobs, WorldImage};
pub use pe_core::ids::ObjectId;
pub use world::{JointDesc, JointKey, PhysicsWorld, Pose, Push};
