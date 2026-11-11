//! pe-app (WS-app, @engine-architect): the application. It spawns the threads, connects their queues and orders
//! startup and shutdown; each crate keeps its own loop logic (docs/spec/app.md).
//!
//! - [`clock`]: the one host clock every thread shares, and the frame clock of a world run alone.
//! - [`sim`]: the simulation thread: one tick (app.md 3), its wait, the replica's budget, start and stop.
//! - [`standin`]: an agreement and a replica that stand in until pe-agree and pe-replica's replica land.
//!
//! The binary (`src/main.rs`) runs a world alone on the desktop. The network, VR, MCP, configuration and invites join
//! as their crates land.

pub mod clock;
pub mod sim;
pub mod standin;
