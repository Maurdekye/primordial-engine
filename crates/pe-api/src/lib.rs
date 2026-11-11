//! pe-api (WS-api, @api-eng). WorldApi (seam S6): one typed in-process API over the live world, for MCP, the
//! inspector and pe-app. Specification: docs/spec/api.md. The trait and its types are `pe_seams::world_api`.
//!
//! - `versions`: the per-connection table of versions seen, behind the default `if_version` (spec 4.2).
//! - `rate`: the per-principal token bucket (spec 9.3).
//! - `stream`: the change stream with cursors and retention, behind `watch` (spec 5.4, 9.1).
//! - `ports`: the snapshot port pe-app implements over pe-world (spec 3, 11).
//! - `query`: references, paths, selectors, paging and projection over one snapshot (spec 5.2).
//! - `strict`: unknown fields in ops are refused, not ignored.
//! - `resolve`: a transaction to explicit edits against its base snapshot (spec 5.3, 9.2 step 2).

pub mod ports;
pub mod query;
pub mod rate;
pub mod resolve;
pub mod stream;
pub mod strict;
pub mod versions;
