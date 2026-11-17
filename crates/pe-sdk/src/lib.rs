//! pe-sdk (WS-logic, @iter-bench). The script SDK: what a guest module links against. It builds for
//! wasm32-unknown-unknown and for the host, which uses [`layout`] and [`manifest`] as the one definition of the
//! batch and manifest bytes.
//!
//! Specification: docs/spec/logic.md 3 (the ABI), 4.1 (what a script may contain), 8 (positions) and appendix A.
//! Everything is `macro_rules!`: no build script and no procedural macro, so a player needs no host linker (5.2a).
//!
//! A script, in outline:
//! ```ignore
//! #![no_std]
//! #![forbid(unsafe_code)]
//! use pe_sdk::prelude::*;
//!
//! pe_sdk::component! { pub struct Blink { period: f32, phase: f32 } }
//! pe_sdk::component! { pub struct Glow { intensity: f32 } }
//! pe_sdk::system! {
//!     fn pulse(stage: PrePhysics, read: [blink: Blink], write: [glow: Glow]) |ctx, me| {
//!         let t = ctx.time_s() + me.blink.phase;
//!         me.glow.intensity = if t % me.blink.period < me.blink.period * 0.5 { 1.0 } else { 0.0 };
//!     }
//! }
//! pe_sdk::script!(components: [Blink, Glow], systems: [pulse]);
//! ```
//!
//! Module memory is reset at every frame (5.1), so a `static` keeps nothing from one frame to the next: all state a
//! script keeps is component data.

#![no_std]

pub mod color;
pub mod component;
pub mod engine;
pub mod guest;
pub mod layout;
pub mod manifest;
pub mod space;

/// What `pe_abi_version()` returns: pe-logic's `abi::ABI_VERSION`.
pub const ABI_VERSION: i32 = 1;

pub mod prelude {
    pub use crate::component::{Component, LinearRgb, LinearRgba, ObjectId};
    pub use crate::guest::{Ctx, Full};
    pub use crate::space::{ORIGIN, Offset, Own, Parent, Pos, World};
}

#[cfg(all(target_arch = "wasm32", not(test)))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // A panic is a deterministic trap: the call faults (3.6).
    core::arch::wasm32::unreachable()
}
