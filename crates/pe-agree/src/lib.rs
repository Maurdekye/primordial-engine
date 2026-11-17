//! pe-agree (WS-agree, @world-chunks). Agreed cuts (seam S3): proposals, votes over the voter's own root, final cuts,
//! holds on votes.
//!
//! Specification: docs/spec/agreement.md (branch agree/spec); the checked model is models/cuts.
//!
//! - [`core`]: the consensus core (views, votes, certificates, timeouts, the lock, two-chain finality, leader
//!   reputation), independent of entries.
//! - [`node`]: seam S3 ([`pe_seams::Agreement`]) and the replica's [`pe_seams::EntrySource`] over the three.
//! - [`codec`]: the wire messages.
//! - [`epoch`]: a joiner's start, the epoch certificate.
//! - [`cuts`]: building a cut as the leader and checking a proposed one before voting.

pub mod codec;
pub mod core;
pub mod cuts;
pub mod epoch;
pub mod node;

pub use node::{LEAVE, Node, NodeConfig, RateBudget, is_leave};

#[cfg(test)]
mod core_tests;
#[cfg(test)]
mod node_tests;
