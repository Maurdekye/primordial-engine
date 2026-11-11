//! `pe-mcp`: the MCP server as a thin adapter over WorldApi (`docs/spec/api.md` section 6).
//!
//! - [`server::WorldServer`]: the nine tools on rmcp, with per-token scopes. It holds no world and runs no engine
//!   loop: every call goes to a WorldApi on a blocking worker.
//! - Transports, kept from the archived pe-mcp (`archive/crates/pe-mcp`): [`pipe`] (named pipe / Unix socket, the
//!   default, via `pe-mcp bridge`) and [`http`] (loopback Streamable HTTP, the fallback). An in-process channel uses
//!   [`server::Auth::Fixed`].
//! - [`auth`]: per-session tokens, each with its scopes and its grant (member, producer label, confirm).
//! - [`host`]: start the transports and launch a harness; pe-app calls it.
//! - [`mock`]: a fixed in-memory WorldApi for tests and `pe-mcp serve-mock`.

pub mod auth;
pub mod host;
pub mod http;
pub mod mock;
pub mod pipe;
pub mod server;
