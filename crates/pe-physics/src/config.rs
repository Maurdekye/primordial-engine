//! The engine's world definition.

use box3d_rust::recycle_scale::ContactRecycleScaling;
use box3d_rust::types::{default_world_def, WorldDef};

/// Box3D sub-steps per frame.
pub const SUBSTEPS: i32 = 4;

/// How a physics world is configured. Every field changes the simulation except `workers`,
/// which only changes how fast it runs (D2: results are bit-identical for every count).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicsConfig {
    /// Worker threads for stepping (1 = serial). Clamped to 1..=32. No threads on wasm32.
    pub workers: i32,
    /// Scale-dependent contact recycling (the "van der Waals simulation"). On for engine
    /// worlds (user ruling 2026-10-30); off reproduces upstream Box3D and its C goldens.
    pub contact_recycle_scaling: ContactRecycleScaling,
    /// Gravity in m/s^2.
    pub gravity: [f32; 3],
    /// Bodies may sleep.
    pub enable_sleep: bool,
}

impl Default for PhysicsConfig {
    fn default() -> Self {
        PhysicsConfig::engine()
    }
}

impl PhysicsConfig {
    /// The engine's configuration: Box3D defaults plus scale-dependent contact recycling
    /// with the HarmonicMean combine, one worker.
    pub fn engine() -> PhysicsConfig {
        PhysicsConfig {
            workers: 1,
            contact_recycle_scaling: ContactRecycleScaling::enabled(),
            gravity: [0.0, -10.0, 0.0],
            enable_sleep: true,
        }
    }

    /// Upstream Box3D's defaults (fixed contact-recycling distance), for comparison with the
    /// C goldens. Not for engine worlds.
    pub fn upstream() -> PhysicsConfig {
        PhysicsConfig {
            contact_recycle_scaling: ContactRecycleScaling::disabled(),
            ..PhysicsConfig::engine()
        }
    }

    pub fn with_workers(mut self, workers: i32) -> PhysicsConfig {
        self.workers = workers;
        self
    }

    /// The Box3D world definition for this configuration.
    pub fn world_def(&self) -> WorldDef {
        let mut def = default_world_def();
        def.contact_recycle_scaling = self.contact_recycle_scaling;
        def.gravity = box3d_rust::math_functions::Vec3 {
            x: self.gravity[0],
            y: self.gravity[1],
            z: self.gravity[2],
        };
        def.enable_sleep = self.enable_sleep;
        def.worker_count = self.workers;
        def
    }
}

/// The engine's Box3D world definition ([`PhysicsConfig::engine`]).
pub fn engine_world_def() -> WorldDef {
    PhysicsConfig::engine().world_def()
}
