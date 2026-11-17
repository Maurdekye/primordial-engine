//! pe-app (WS-app, @engine-architect): the application. It spawns the threads, connects their queues and orders
//! startup and shutdown; each crate keeps its own loop logic (docs/spec/app.md).
//!
//! - [`clock`]: the one host clock every thread shares, and the frame clock of a world run alone.
//! - [`net`]: the network side: the endpoint, the M1 interim's open and join, and the simulation's `NetHook`.
//! - [`genesis`]: the M1 interim's genesis record: what both machines build frame 0 from, and its checks.
//! - [`sim`]: the simulation thread: one tick (app.md 3), its wait, the replica's budget, start and stop.
//! - [`phases`]: where the simulation thread's time goes, step by step, to find what slows a machine.
//! - [`standin`]: an agreement and a replica that stand in until pe-agree and pe-replica's replica land.
//! - [`vr`]: VR mode's frame sink, which draws the newest view into pe-xr's swapchain images.
//! - [`world`]: the world a machine opens, under pe-replica's replica: for now a new, empty one.
//!
//! The binary (`src/main.rs`) runs a world alone on the desktop. The network, VR, MCP, configuration and invites join
//! as their crates land.

#[cfg(test)]
mod agent_demo;
pub mod api;
pub mod builds;
pub mod capture;
pub mod clock;
pub mod console;
pub mod genesis;
pub mod leave;
pub mod local;
pub mod net;
pub mod phases;
pub mod sim;
pub mod stamp;
pub mod standin;
pub mod vr;
pub mod world;
