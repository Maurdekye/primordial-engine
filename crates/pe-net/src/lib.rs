//! pe-net (WS-net, @shader-lang). Transport (seam S4): direct QUIC connections from an invite, five lanes, the rate
//! budget, the session clock.
//!
//! Specification: docs/spec/transport.md. A skeleton from pe-seams v0, so the crate is a workspace member from M0; its
//! owner fills it in. The seams it implements or uses are in `pe_seams`.
