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
//! - [`backend`], [`session`]: the session lifecycle and the frame loop over a small backend trait (spec 3.1,
//!   7.1), tested against [`fake`], a fake runtime that checks OpenXR's call order;
//! - [`clock`], [`real`]: the host clock, and the real runtime at the instance level: extensions, the
//!   headset lookup, `XrTime` to host time (spec 2, 3.3, 6.1); [`real_session`], the real backend over the
//!   `openxr` crate, generic over its graphics type;
//! - [`seam`]: stand-ins for pe-seams v0's S5 types (spec 8.1), until pe-seams lands.

pub mod backend;
pub mod binding;
pub mod clock;
pub mod fake;
pub mod guard;
pub mod hands;
pub mod loader;
pub mod math;
pub mod real;
pub mod real_session;
pub mod ring;
pub mod seam;
pub mod session;

#[cfg(test)]
mod session_tests;
