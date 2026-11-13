//! pe-diverge (WS-diverge, @diverge-eng). Divergence detection and repair (docs/spec/divergence.md): the repair
//! ladder, root splits, fault bundles, counters.
//!
//! M1 scope, built here on seams v0 (`pe_seams`):
//! - detection: the replica compares its root with every final cut's (`finalize`); pe-diverge acts on the verdict
//!   (spec 3);
//! - the repair ladder's rung 1 (the replica's replay from the last verified cut) and rung 2 (fetch only what
//!   differs; the replica checks every record's proof against the agreed root, imports it and replays) (spec 4);
//! - root splits with no quorum root, with a sticky reference member (spec 5);
//! - fault bundles (spec 6), counters (spec 7), and the repair lane's messages (spec 11.3).
//!
//! Sans-IO: no thread, socket, clock or randomness. The merge engine (spec 9) is not built until WS-model's model v2
//! has checked it.

pub mod bundle;
pub mod counters;
pub mod ladder;
pub mod msg;
pub mod split;

pub use bundle::FaultBundle;
pub use counters::Counters;
pub use ladder::{Action, Config, Diverge, Mutant};
