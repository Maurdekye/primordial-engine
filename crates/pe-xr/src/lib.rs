//! pe-xr: OpenXR for Primordial Engine, as specified in `docs/spec/xr.md`.
//!
//! pe-xr turns a headset into three things the rest of the engine understands: views to draw, hand
//! samples in the player's rig frame, and a host clock shared with the renderer and the simulation. It
//! holds no world state and never blocks the simulation.
//!
//! This first part is the runtime-independent core:
//! - [`guard`]: the headset guard, the one check before any runtime code can run (spec 10.3);
//! - [`loader`]: finding and loading the OpenXR runtime in Rust, with no Khronos loader (spec 2);
//! - [`hands`]: from raw hand readings to hand samples: grip, grasp with hysteresis, tracking loss (spec 5);
//! - [`ring`]: the ring of hand samples the simulation thread reads, without locks (spec 6.2);
//! - [`seam`]: stand-ins for pe-seams v0's S5 types (spec 8.1), until pe-seams lands.

pub mod guard;
pub mod hands;
pub mod loader;
pub mod math;
pub mod ring;
pub mod seam;
