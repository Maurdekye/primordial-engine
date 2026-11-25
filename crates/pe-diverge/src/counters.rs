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
    /// Times the ladder stopped after repairs failed again and again (4.3).
    pub repairs_stuck: u64,
    /// Diverged final cuts compared while the ladder was stopped, with no rung run.
    pub stuck_skipped: u64,
    /// Repairs toward a final cut overtaken by a newer one, in a row since the last cure or agreeing cut: a gauge,
    /// not a total. It tells "too big to fetch within `w_fg`" (4.2) from "unlucky once" (@design-reviewer, Low).
    pub overtaken_in_a_row: u64,
    /// Divergences whose bundles go without replay evidence (6), counted once per `Cv`: `Cv`'s state or the entries
    /// from it no longer kept, or the state over `evidence_max_bytes`. The capture is not tried again for that `Cv`.
    pub evidence_skipped: u64,
    pub root_splits: u64,
    /// A replay or repair under way when a root split at a newer cut arrived, a re-proposal of the same height
    /// included (agreement.md L4'). The split is what matters, so the work moves there: no failure, a note (5).
    pub split_overtook: u64,
    /// Split replays or repairs overtaken in a row since the last split episode ended: a gauge, not a total
    /// (@fable-reviewer). With the split cut pinned (agreement.md L4') it stays at 0 at n = 2.
    pub split_overtaken_in_a_row: u64,
    /// `RootSplit` reports for the open episode's cut, the first included: one per view that re-proposed the pinned
    /// cut and split again, plus one per newly shown verdict. A gauge for the open episode; `split_reports_max` keeps
    /// the most any episode took (@coordinator-opus, point 2).
    pub split_reports: u64,
    pub split_reports_max: u64,
    pub root_splits_ended: u64,
    pub bundles_written: u64,
    /// Messages from peers that did not decode.
    pub bad_messages: u64,
    /// Well-formed replies nobody was waiting for (late, or from a source already given up): ignored.
    pub stale_replies: u64,
    /// Moves of the session to a fresh line (F3's branch at n = 2, [`crate::Diverge::switch_line`]).
    pub line_switches: u64,
    /// Root splits whose target this machine already held without a cure (an import that finished after its repair
    /// was dropped): the target was voted again ([`crate::Action::Revote`]) instead of repaired to.
    pub revotes_held: u64,
    /// Requests refused: an unknown root, a peer over its budget, or a second physics image of one root.
    pub serve_refused: u64,
    /// Messages not sent because they broke a codec limit (nothing is truncated).
    pub encode_refused: u64,
    /// Blobs and record pieces a repair or join pulled after the record replies named them (divergence.md 4.2, step 3).
    pub blobs_pulled: u64,
    pub pieces_pulled: u64,
    /// Named blobs an earlier, overtaken attempt had already hashed: taken from the cache, not pulled again.
    pub blobs_reused: u64,
    /// Runs of `SLOW_OVERTAKES` repairs overtaken in a row with nothing new kept: the link too slow to resync.
    pub link_too_slow: u64,
    /// Joins from a final cut whose import matched its certified root, and joins that failed ([`crate::Join`]).
    pub joins: u64,
    pub join_failures: u64,
    /// Resyncs of a member that could not keep real time, at a final cut the others certified ([`crate::Diverge::resync`]),
    /// and resyncs that failed.
    pub resyncs: u64,
    pub resync_failures: u64,
}
