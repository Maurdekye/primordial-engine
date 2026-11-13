//! The counters pe-diverge keeps itself (divergence.md 7). The display layer's come from pe-render, rollback depth
//! from pe-replica and `voided_own` from pe-agree; pe-app gathers them all.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub cuts_compared: u64,
    pub cuts_matched: u64,
    pub diverged: u64,
    /// Final cuts where the replica could not take its root at all (`Finalized::Damaged`): rung 2 imported the
    /// whole state.
    pub damaged: u64,
    /// Repairs by the rung that cured them; index 0 is rung 1.
    pub cured_by_rung: [u64; 5],
    /// Rung attempts that failed.
    pub rung_failures: u64,
    /// From the final cut that showed a mismatch to the cure, in host nanoseconds, one entry per cure.
    pub repair_latency_nanos: Vec<u64>,
    /// Bytes of records and physics images fetched for repair.
    pub repair_bytes: u64,
    /// A source whose records failed their proof, that did not hold the root, or went silent: each moves to the next.
    pub source_faults: u64,
    /// Sources left after answering `Busy` `n_busy_max` times in a row: over budget, not faulty, so counted apart
    /// from `source_faults` (@shader-reviewer, nit a).
    pub busy_rotations: u64,
    /// Leaves marked repeatedly diverging.
    pub repeated_divergence: u64,
    pub root_splits: u64,
    pub root_splits_ended: u64,
    pub bundles_written: u64,
    /// Messages from peers that did not decode.
    pub bad_messages: u64,
    /// Well-formed replies nobody was waiting for (late, or from a source already given up): ignored.
    pub stale_replies: u64,
    /// Requests refused: an unknown root, a peer over its budget, or a second physics image of one root.
    pub serve_refused: u64,
    /// Messages not sent because they broke a codec limit (nothing is truncated).
    pub encode_refused: u64,
}
