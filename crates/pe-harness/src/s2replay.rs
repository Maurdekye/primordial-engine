//! P2's offline replay through S2, for a replica with no key-value oracle (pe-world's): a fresh replica from genesis,
//! given the whole final log up front, steps it straight through with [`Replica::advance`] and takes its root at
//! each final cut with [`Replica::root_at_cut`]. Nothing in it comes from pe-agree or from the live run's replicas:
//! no late entry, no rollback, no prediction, no finalize. P2 then holds the live run's agreed roots, reached through
//! prediction, late arrivals and re-simulation, to this straight replay ([`crate::props::p2_live_equals_replay`]).
//!
//! The replay's source holds each final entry at its stamp, in agreed order; voided entries are absent, as a final
//! cut makes them. It never trims, and it reports no final cut, so the replica's floor stays at genesis and it keeps
//! a saved state for every frame of the run. Test runs so far are a few hundred frames (the ball-pit run: 240); past
//! [`MAX_FRAMES`] the replay refuses and returns nothing, so P2 fails loudly instead of the replay eating memory.
//!
//! P2's keys put every root on line 0. TODO(branches): key by the cut's line once a run has more than one.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use pe_seams::{EntrySource, FinalCut, Frame as SeamFrame, Replica, SignedEntry};

use crate::seamnode::FinalLog;
use crate::trace::{Frame, Hash, Line};

/// The longest run the replay steps: a minute at 60 Hz, a bound chosen well above the runs measured (240 frames),
/// not a measured limit.
pub const MAX_FRAMES: u64 = 3_600;

/// The most `advance` calls one frame may take before the replay gives up on it (the budget below makes one enough).
const MAX_CALLS: usize = 1_000;

/// The final log as an entry source: every entry at its stamp, every frame kept.
struct Static {
    at: BTreeMap<u64, Vec<SignedEntry>>,
}

impl EntrySource for Static {
    fn final_cut(&self) -> Option<&FinalCut> {
        None
    }
    fn entries_at(&self, frame: SeamFrame) -> Option<&[SignedEntry]> {
        Some(self.at.get(&frame.0).map_or(&[], Vec::as_slice))
    }
    fn kept_from(&self) -> SeamFrame {
        SeamFrame(0)
    }
    fn keep_final_from(&mut self, _cv: SeamFrame, _floor: SeamFrame) {}
    fn take_changed_from(&mut self) -> Option<SeamFrame> {
        None
    }
}

/// The roots of `replica`, fresh from genesis, at each cut in `log` whose frame is in `frames`, keyed as P2 reads
/// them (line 0, the cut's frame). A frame with no cut in `log`, or whose root the replica can't give, is left out,
/// and P2 reports it as missing. Empty if a frame passes [`MAX_FRAMES`].
pub fn replay<R: Replica>(mut replica: R, log: &FinalLog, frames: &BTreeSet<Frame>) -> BTreeMap<(Line, Frame), Hash> {
    if frames.last().is_some_and(|&f| f > MAX_FRAMES) {
        return BTreeMap::new();
    }
    let mut src = Static { at: BTreeMap::new() };
    for e in &log.entries {
        src.at.entry(e.entry.stamp.0).or_default().push(e.clone());
    }
    replica.set_budget(Duration::from_secs(3_600));
    let mut cuts: Vec<_> = log.cuts.values().filter(|c| frames.contains(&c.at.frame.0)).collect();
    cuts.sort_by_key(|c| (c.at.frame, c.at.height));
    let mut out = BTreeMap::new();
    for cut in cuts {
        let mut calls = 0;
        while calls < MAX_CALLS && !replica.advance(cut.at.frame, &mut src).caught_up {
            calls += 1;
        }
        if let Ok(root) = replica.root_at_cut(cut, &mut src) {
            out.insert((0, cut.at.frame.0), root.0);
        }
    }
    out
}

/// The longest log among `logs`, with every log's cuts: the replay's input. A cut any member saw final wins its
/// height over one only voted on. P1 checks that the members agree.
pub fn reference(logs: &[FinalLog]) -> FinalLog {
    let mut out = logs.iter().max_by_key(|l| l.entries.len()).cloned().unwrap_or_default();
    for l in logs {
        for (h, c) in &l.cuts {
            out.record(c, l.finals.contains(h));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{Cut, CutRef, LineId};

    fn cut(height: u64, frame: u64) -> Cut {
        Cut { at: CutRef { line: LineId::origin(), height, frame: SeamFrame(frame) }, heads: Vec::new() }
    }

    #[test]
    fn a_final_cut_wins_its_height_over_a_voted_one_in_any_order() {
        let (voted, fin) = (cut(3, 18), cut(3, 24));
        let mut a = FinalLog::default();
        a.record(&voted, false);
        a.record(&fin, true);
        a.record(&voted, false);
        assert_eq!(a.cuts[&3], fin);
        let mut only_voted = FinalLog::default();
        only_voted.record(&voted, false);
        let mut only_final = FinalLog::default();
        only_final.record(&fin, true);
        for logs in [[only_voted.clone(), only_final.clone()], [only_final, only_voted]] {
            let r = reference(&logs);
            assert_eq!(r.cuts[&3], fin);
            assert!(r.finals.contains(&3));
        }
    }

    #[test]
    fn the_replay_refuses_a_run_past_its_frame_bound() {
        let frames: BTreeSet<Frame> = [6, MAX_FRAMES + 6].into();
        let mut log = FinalLog::default();
        log.record(&cut(1, 6), true);
        // The bound is checked before the replica is touched, so any replica will do; the key-value one is at hand.
        let r = crate::kvrep::KvSeamReplica::new(Vec::new(), &crate::kv::Template::AllAdmin, &[]);
        assert!(replay(r, &log, &frames).is_empty());
    }
}
