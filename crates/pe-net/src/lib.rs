//! pe-net (WS-net, @shader-lang). Transport (seam S4): direct QUIC connections from an invite, five lanes, the rate
//! budget, the session clock.
//!
//! Specification: docs/spec/transport.md. Built sans-IO (transport.md 7): every module takes time and bytes as
//! inputs and returns bytes and events, so the harness drives exactly the production code.
//!
//! - [`core`]: the sans-IO `Core` (7): quinn-proto's endpoint and connections, the lanes over them, the lifecycle.
//! - [`budget`]: one link's rate budget, delay guard and ride gate (4).
//! - [`clock`]: the session clock (5) and the published `FrameClock` model (5.7).
//! - [`lanes`]: one link's lanes (3): framing, reliability, the pose slot, the stream path.
//! - [`lifecycle`]: the hello, the welcome and the close reasons (6.3-6.6).
//! - [`thread`]: the network thread and the simulation thread's handle (7).

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod budget;
pub mod clock;
pub mod core;
pub mod lanes;
pub mod lifecycle;
pub mod thread;
