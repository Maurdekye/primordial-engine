//! Keyframes (replica.md 4.1): saved states at the start of frames, for rollback, rung 1 and the held roots.
//!
//! The ring's policy is local: it decides only how far back a re-simulation starts, never a result. It is the old
//! loop's `Log(k)`, with its spacing set in time: a keyframe every `rate / 15` frames (4 at 60 Hz, about 67 ms) for
//! the newest `k` spacings, then about two per doubling of age, so memory grows with the logarithm of the unconfirmed
//! span. Frames the replica must be able to restore (the floor, `Cv`, held roots) are pinned and never trimmed.

use std::collections::{BTreeMap, BTreeSet};

use pe_seams::Frame;

/// The ring's policy, in frames at the world's rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Frames between keyframes while they are young.
    pub spacing: u64,
    /// How many spacings count as young.
    pub dense: u64,
}

impl Policy {
    /// `Log(4)` at `rate_hz`: every `rate / 15` frames (at least 1) for the newest 4 spacings.
    pub fn log4(rate_hz: u32) -> Policy {
        Policy { spacing: u64::from(rate_hz / 15).max(1), dense: 4 }
    }

    /// Whether a keyframe should be taken at the start of `frame`.
    pub fn takes(&self, frame: Frame) -> bool {
        frame.0.is_multiple_of(self.spacing)
    }
}

/// Saved states by frame.
#[derive(Clone, Debug)]
pub struct Ring<K> {
    keys: BTreeMap<Frame, K>,
    pinned: BTreeSet<Frame>,
}

impl<K> Default for Ring<K> {
    fn default() -> Self {
        Ring { keys: BTreeMap::new(), pinned: BTreeSet::new() }
    }
}

impl<K> Ring<K> {
    pub fn new() -> Ring<K> {
        Ring::default()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn frames(&self) -> impl Iterator<Item = Frame> + '_ {
        self.keys.keys().copied()
    }

    /// Keep `key` as the state at the start of `frame`, replacing any older one there.
    pub fn insert(&mut self, frame: Frame, key: K) {
        self.keys.insert(frame, key);
    }

    pub fn get(&self, frame: Frame) -> Option<&K> {
        self.keys.get(&frame)
    }

    /// The newest keyframe at or before `frame`: where a re-simulation of `frame` starts.
    pub fn at_or_before(&self, frame: Frame) -> Option<(Frame, &K)> {
        self.keys.range(..=frame).next_back().map(|(f, k)| (*f, k))
    }

    /// Drop every keyframe after `frame`: they belong to a history a re-simulation is replacing.
    pub fn truncate_after(&mut self, frame: Frame) {
        self.keys.retain(|f, _| *f <= frame);
    }

    /// Never trim these frames' keyframes (the floor, `Cv`, held roots); the set replaces the previous one.
    pub fn pin(&mut self, frames: impl IntoIterator<Item = Frame>) {
        self.pinned = frames.into_iter().collect();
    }

    /// Apply the policy at `present`, and drop everything below `floor` that is not pinned. The newest keyframe at or
    /// below the floor is kept too, so a re-simulation can always start at or below any frame above it.
    pub fn trim(&mut self, policy: Policy, present: Frame, floor: Frame) {
        let base = self.at_or_before(floor).map(|(f, _)| f);
        let young = policy.spacing.saturating_mul(policy.dense);
        // Old keyframes: at most two per doubling of age past the young span, the newest of each band first.
        let mut per_band: BTreeMap<u32, u32> = BTreeMap::new();
        let mut drop = Vec::new();
        for f in self.keys.keys().rev().copied() {
            if self.pinned.contains(&f) || Some(f) == base {
                continue;
            }
            if f < floor {
                drop.push(f);
                continue;
            }
            let age = present.0.saturating_sub(f.0);
            if age <= young {
                continue;
            }
            let band = (age / young.max(1)).ilog2();
            let n = per_band.entry(band).or_insert(0);
            if *n >= 2 {
                drop.push(f);
            } else {
                *n += 1;
            }
        }
        for f in drop {
            self.keys.remove(&f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(policy: Policy, to: u64) -> Ring<u64> {
        let mut r = Ring::new();
        for f in 0..to {
            if policy.takes(Frame(f)) {
                r.insert(Frame(f), f);
            }
        }
        r
    }

    #[test]
    fn spacing_follows_the_rate() {
        assert_eq!(Policy::log4(60).spacing, 4);
        assert_eq!(Policy::log4(30).spacing, 2);
        assert_eq!(Policy::log4(90).spacing, 6);
        assert_eq!(Policy::log4(120).spacing, 8);
        assert_eq!(Policy::log4(14).spacing, 1);
    }

    #[test]
    fn young_keyframes_stay_and_old_ones_thin_logarithmically() {
        let p = Policy::log4(60);
        let mut r = filled(p, 4000);
        r.trim(p, Frame(4000), Frame(0));
        let frames: Vec<u64> = r.frames().map(|f| f.0).collect();
        // Everything in the newest 16 frames is kept.
        for f in (4000 - 16..4000).filter(|f| f % 4 == 0) {
            assert!(frames.contains(&f), "young keyframe {f} kept");
        }
        // Past that, at most two per doubling of age: about 2 * log2(4000 / 16) + the young ones + the base.
        assert!(frames.len() <= 4 + 2 * 8 + 2, "{} keyframes: {frames:?}", frames.len());
        assert!(frames.len() >= 4 + 8, "{} keyframes: {frames:?}", frames.len());
        // Frame 0, the newest at or below the floor, is kept: a re-simulation can always start.
        assert_eq!(frames[0], 0);
    }

    #[test]
    fn the_floor_and_pinned_frames_bound_what_goes() {
        let p = Policy::log4(60);
        let mut r = filled(p, 400);
        r.pin([Frame(100), Frame(7)]);
        r.insert(Frame(7), 7);
        r.trim(p, Frame(400), Frame(200));
        let frames: Vec<u64> = r.frames().map(|f| f.0).collect();
        assert!(frames.contains(&7) && frames.contains(&100), "pinned frames stay: {frames:?}");
        assert!(frames.contains(&200), "the floor's keyframe stays: {frames:?}");
        assert!(!frames.iter().any(|&f| f < 200 && f != 7 && f != 100), "below the floor only pinned: {frames:?}");
        assert_eq!(r.at_or_before(Frame(250)).map(|(f, _)| f.0), frames.iter().copied().filter(|&f| f <= 250).max());
    }

    #[test]
    fn a_resimulation_replaces_the_newer_keyframes() {
        let p = Policy::log4(60);
        let mut r = filled(p, 40);
        r.truncate_after(Frame(20));
        assert_eq!(r.frames().last(), Some(Frame(20)));
        assert_eq!(r.at_or_before(Frame(39)).map(|(f, _)| f), Some(Frame(20)));
    }
}
