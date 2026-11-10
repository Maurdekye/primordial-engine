//! The engine's world definition.

use box3d_rust::recycle_scale::ContactRecycleScaling;
use box3d_rust::types::{WorldDef, default_world_def};

/// Box3D sub-steps per tick at the default tick rate, 60 Hz: [`sub_steps_for_rate`]`(60)`.
pub const SUBSTEPS: i32 = 4;

/// The tick rates a world may have, in whole hertz, and the default (the user's ruling of 2026-11-10,
/// mc8dc9e5a6ffb; docs/spec/replica.md 3.5 and 13). The world's setting is pe-seams's `SimRate`. These equal its
/// `MIN_HZ`, `MAX_HZ` and `DEFAULT`, so pe-physics refuses what the seam refuses; pe-replica's tests pin them equal.
pub const TICK_HZ_MIN: u32 = 30;
pub const TICK_HZ_MAX: u32 = 120;
pub const TICK_HZ_DEFAULT: u32 = 60;

/// Box3D sub-steps per tick at `hz` ticks a second: ceil(240 / hz), at least 1, so the sub-step rate never falls
/// below 240 Hz. Below that Box3D softens contacts: it caps their stiffness at an eighth of the sub-step rate, against
/// the world's 30 Hz (box3d-fork `world/step.rs:90`). Measured in docs/spec/replica.md 13.1. `hz` 0 gives 1.
pub const fn sub_steps_for_rate(hz: u32) -> i32 {
    if hz == 0 {
        return 1;
    }
    let n = 240u32.div_ceil(hz);
    if n == 0 { 1 } else { n as i32 }
}

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
    /// Box3D sub-steps per tick, 1 to [`crate::limits::MAX_SUB_STEPS`]: [`SUBSTEPS`] by default, and
    /// [`sub_steps_for_rate`] of the world's tick rate through [`PhysicsConfig::with_tick_rate`].
    pub sub_steps: i32,
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
            sub_steps: SUBSTEPS,
        }
    }

    /// This configuration for a world that ticks `hz` times a second: [`sub_steps_for_rate`]`(hz)` sub-steps.
    /// Refuses a rate outside [`TICK_HZ_MIN`]..=[`TICK_HZ_MAX`]. The caller steps with `dt = 1.0 / hz as f32`.
    pub fn with_tick_rate(mut self, hz: u32) -> Result<PhysicsConfig, crate::PhysicsError> {
        if !(TICK_HZ_MIN..=TICK_HZ_MAX).contains(&hz) {
            return Err(crate::PhysicsError::InvalidValue("tick rate"));
        }
        self.sub_steps = sub_steps_for_rate(hz);
        Ok(self)
    }

    /// Upstream Box3D's defaults (fixed contact-recycling distance), for comparison with the
    /// C goldens. Not for engine worlds.
    pub fn upstream() -> PhysicsConfig {
        PhysicsConfig { contact_recycle_scaling: ContactRecycleScaling::disabled(), ..PhysicsConfig::engine() }
    }

    pub fn with_workers(mut self, workers: i32) -> PhysicsConfig {
        self.workers = workers;
        self
    }

    /// Refuse a gravity that is not finite or beyond [`crate::limits::MAX_GRAVITY`], and a sub-step count outside
    /// 1..=[`crate::limits::MAX_SUB_STEPS`].
    pub fn validate(&self) -> Result<(), crate::PhysicsError> {
        if !(1..=crate::limits::MAX_SUB_STEPS).contains(&self.sub_steps) {
            return Err(crate::PhysicsError::InvalidValue("sub-steps"));
        }
        crate::desc::bounded(&self.gravity, crate::limits::MAX_GRAVITY, "gravity")
    }

    /// The Box3D world definition for this configuration.
    pub fn world_def(&self) -> WorldDef {
        let mut def = default_world_def();
        def.contact_recycle_scaling = self.contact_recycle_scaling;
        def.gravity = box3d_rust::math_functions::Vec3 { x: self.gravity[0], y: self.gravity[1], z: self.gravity[2] };
        def.enable_sleep = self.enable_sleep;
        def.worker_count = self.workers;
        def
    }
}

/// The engine's Box3D world definition ([`PhysicsConfig::engine`]).
pub fn engine_world_def() -> WorldDef {
    PhysicsConfig::engine().world_def()
}
