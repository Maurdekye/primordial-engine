//! `pe-mcp`: the engine's MCP server (design 17), the phase-2 test and driver interface (23.4 P2-8).
//!
//! * [`server::PeerServer`]: the tool catalogue on rmcp, with per-token scopes.
//! * [`engine`]: the thread that owns the driven peer; [`backend::DrivenPeer`] is what a peer
//!   must provide (the session simulator now, `pe-session` later).
//! * Transports: stdio (embedded bench), [`pipe`] (named pipe / Unix socket, the default for a
//!   harness, via `pe-mcp bridge`) and [`http`] (loopback Streamable HTTP, the fallback).
//! * [`auth`]: per-session tokens.
//!
//! The tool schemas are in `schema/tools.json` and described in `README.md`.

pub mod auth;
pub mod backend;
pub mod engine;
pub mod http;
pub mod model;
pub mod pipe;
pub mod server;
pub mod sim_backend;
