//! Keyframes (replica.md 4.1): saved states at the start of frames, for rollback, rung 1 and the held roots.
//!
//! The ring's policy is local: it decides only how far back a re-simulation starts, never a result. It is the old
//! loop's `Log(k)`, with its spacing set in time: a keyframe every `rate / 15` frames (4 at 60 Hz, about 67 ms) for
//! the newest `k` spacings, then about two per doubling of age, so memory grows with the logarithm of the unconfirmed
//! span. The floor's and `Cv`'s keyframes are pinned and never trimmed.
//!
//! Held roots are not keyframes. A held root is the state with exactly its cut's entries (`root_at_cut`), which the
//! history's keyframe at that frame need not share, and a rollback below it must not take it away. The replica keeps
//! each as its own snapshot, by root (replica.md 8: `save() -> Saved`).

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

    /// Drop every keyframe after `frame`: they belong to a history a re-simulation is replacing. Pinned frames stay,
    /// as everywhere. They are the floor and `Cv`, both at or below the floor, and a re-simulation never starts below
    /// the floor (4.2), so in use no pinned frame is ever after `frame`.
    pub fn truncate_after(&mut self, frame: Frame) {
        debug_assert!(
            self.pinned.iter().all(|&p| p <= frame),
            "a rollback to {frame:?} below a pinned frame: rung 1 uses rewind_to"
        );
        let pinned = &self.pinned;
        self.keys.retain(|f, _| *f <= frame || pinned.contains(f));
    }

    /// Rung 1 (replica.md 6.1): the history after `frame` is rebuilt from `Cv`, which can be below the floor, so every
    /// keyframe after it goes, pinned ones included, and those frames are no longer pinned. The next `finalize` pins
    /// the floor again, in the rebuilt history.
    pub fn rewind_to(&mut self, frame: Frame) {
        self.keys.retain(|f, _| *f <= frame);
        self.pinned.retain(|f| *f <= frame);
    }

    /// Never trim these frames' keyframes (the floor and `Cv`, 4.1); the set replaces the previous one.
    pub fn pin(&mut self, frames: impl IntoIterator<Item = Frame>) {
        self.pinned = frames.into_iter().collect();
    }

    /// Apply the policy at `present`, and drop everything below `floor` that is not pinned. The newest keyframe at or
    /// below the floor is kept too, so a re-simulation can always start at or below any frame above it.
    ///
    /// Past the young span, a keyframe in band `b` (age in `[young * 2^b, young * 2^(b+1))`) stays only if its frame is
    /// a multiple of `spacing * 2^(b+1)`: two per band when `dense` is 4. That alignment only tightens as a frame ages,
    /// so trimming at every frame keeps exactly what trimming once would (counting the newest keyframes of each band
    /// instead thinned the old ones far below two per band when applied at every frame).
    pub fn trim(&mut self, policy: Policy, present: Frame, floor: Frame) {
        let base = self.at_or_before(floor).map(|(f, _)| f);
        let young = policy.spacing.saturating_mul(policy.dense).max(1);
        let pinned = &self.pinned;
        self.keys.retain(|&f, _| {
            if pinned.contains(&f) || Some(f) == base {
                return true;
            }
            if f < floor {
                return false;
            }
            let age = present.0.saturating_sub(f.0);
            if age <= young {
                return true;
            }
            let band = (age / young).ilog2();
            let step = 1u64.checked_shl(band + 1).and_then(|p| policy.spacing.checked_mul(p)).unwrap_or(u64::MAX);
            f.0.is_multiple_of(step)
        });
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

    /// Trimming at every frame keeps exactly what one trim at the end keeps, and old bands keep two each.
    #[test]
    fn trimming_every_frame_equals_trimming_once() {
        let p = Policy::log4(60);
        let (mut each, mut once) = (Ring::new(), filled(p, 1000));
        for f in 0..1000 {
            if p.takes(Frame(f)) {
                each.insert(Frame(f), f);
            }
            each.trim(p, Frame(f + 1), Frame(0));
        }
        once.trim(p, Frame(1000), Frame(0));
        let frames: Vec<u64> = each.frames().map(|f| f.0).collect();
        assert_eq!(frames, once.frames().map(|f| f.0).collect::<Vec<_>>());
        // Band 2 (ages 64 to 127, frames 873 to 936) keeps the multiples of 32 there: 896 and 928.
        let band2 = frames.iter().filter(|&&f| (1000 - f) >= 64 && (1000 - f) < 128).count();
        assert_eq!(band2, 2, "{frames:?}");
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

    /// A rollback never goes below a pinned frame (the floor and `Cv`), and keeps them.
    #[test]
    fn a_rollback_keeps_pinned_frames() {
        let p = Policy::log4(60);
        let mut r = filled(p, 40);
        r.pin([Frame(8), Frame(16)]);
        r.truncate_after(Frame(20));
        let frames: Vec<u64> = r.frames().map(|f| f.0).collect();
        assert_eq!(&frames[frames.len() - 3..], [12, 16, 20]);
    }

    /// Rung 1 rewinds below the floor: the pinned floor's keyframe belongs to the replaced history and goes too.
    #[test]
    fn a_rewind_drops_and_unpins_what_it_replaces() {
        let p = Policy::log4(60);
        let mut r = filled(p, 40);
        r.pin([Frame(8), Frame(32)]);
        r.rewind_to(Frame(8));
        assert_eq!(r.frames().last(), Some(Frame(8)));
        // Frame 32 is no longer pinned: a later rollback past it is not refused.
        r.insert(Frame(32), 32);
        r.truncate_after(Frame(12));
        assert_eq!(r.frames().last(), Some(Frame(8)));
    }
}
