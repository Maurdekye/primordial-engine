//! pe-app (WS-app, @engine-architect): the application. It spawns the threads, connects their queues and orders
//! startup and shutdown; each crate keeps its own loop logic (docs/spec/app.md).
//!
//! - [`clock`]: the one host clock every thread shares, and the frame clock of a world run alone.
//! - [`net`]: the network side: the endpoint, the M1 interim's open and join, and the simulation's `NetHook`.
//! - [`genesis`]: the genesis record: what the opener builds frame 0 from (alone, or the M1 interim's two members),
//!   and its checks.
//! - [`join`]: joining from a cut: pe-app's join messages and the inviter's side of a join.
//! - [`sim`]: the simulation thread: one tick (app.md 3), its wait, the replica's budget, start and stop.
//! - [`phases`]: where the simulation thread's time goes, step by step, to find what slows a machine.
//! - [`replay`]: `--check-replay`, the opener's replay of its session from frame 0 against every final cut's root.
//! - [`standin`]: an agreement and a replica that stand in until pe-agree and pe-replica's replica land.
//! - [`vr`]: VR mode's frame sink, which draws the newest view into pe-xr's swapchain images.
//! - [`fault`]: `--inject-fault-at`, a deliberate test fault on this machine only (M1 demo step 7), and the world
//!   every replica is over.
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
pub mod cutlog;
pub mod diverge;
pub mod fault;
pub mod genesis;
pub mod handfeel;
pub mod join;
pub mod leave;
pub mod local;
pub mod log;
pub mod menu;
pub mod net;
pub mod phases;
pub mod replay;
pub mod scripted;
pub mod shown;
pub mod sim;
pub mod slow;
pub mod slowframe;
#[cfg(test)]
mod sources;
pub mod spawn;
pub mod stamp;
pub mod standin;
pub mod trace;
pub mod vr;
pub mod world;
