//! What this machine's replays cost (the re-solve cost recommendation's E1, for the 11-27 friend test; @coordinator-opus): counters and timers a driver reads after a run, nothing else. A replica measures them only when its
//! driver attaches a probe ([`crate::ReplicaImpl::with_cost_probe`]); without one its steps time nothing. They are
//! this machine's own, taken with its clock: never in a root, a vote, a view or any agreed state, so attaching a probe
//! changes no outcome.
//!
//! A *replay* is one rewind: a rollback for a changed frame (the agreement's change report, or a script commit's
//! replay), and the re-steps after it until the replica is back where it had been, or the next rewind. For each
//! replay it takes:
//! 1. the time spent taking the agreed-state root in its re-stepped grid frames, and over the run the part of every
//!    root's time whose root was later taken again at the same frame (discarded: a rewind replaced it);
//! 2. the time in the per-step bookkeeping that scans every body: the awake set before and after the physics step,
//!    the write-back of the moved poses, and the grab joints' keys over every physical object;
//! 3. the frames re-stepped before the changed frame: from the keyframe restored up to the changed frame (the
//!    keyframe spacing's share), never below the floor;
//! 4. whether the physics read anything new: a digest of what a step's physics reads (each hand's physical state and,
//!    for a physical hand, its row; the claims; each player's move input; each entry's physics effects), compared with
//!    the replaced history's at each re-stepped frame. A replay that found every frame's digest equal was caused by
//!    entries the physics never reads: an open hand with no body, a head's rotation or height. A scripted world's
//!    replays always count as read (its logic stages are not in the digest).
//!
//! And the rest of the split, so the parts add up to the replay's time (the E1 harness's split; @iter-bench, spec
//! diverge-eng/resolve-e1-harness-spec.md): the restore (the rollback itself: the keyframe's load and the history
//! truncated after it), each re-step's whole time, and within it the physics step and the keyframes saved. "Other" is
//! the re-step's time less every timed part: entries, the movers, the hand sync less its grab keys, logic, hits and
//! the world model's frame end. The digest of (4) is the probe's own cost, timed apart, so it can be taken off.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pe_seams::Frame;

/// What a driver attaches, and reads: the totals one replica (or every replica given it) adds to.
pub type CostProbe = Arc<Mutex<ReplayCosts>>;

/// A new probe.
pub fn probe() -> CostProbe {
    Arc::new(Mutex::new(ReplayCosts::default()))
}

/// Nanoseconds since `t`.
pub(crate) fn since(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// A distribution of per-replay values: exact below 64, then 8 buckets an octave (a quantile within 12.5 % below).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Histogram {
    counts: Vec<u64>,
    n: u64,
}

impl Histogram {
    fn bucket(v: u64) -> usize {
        if v < 64 {
            return v as usize;
        }
        let octave = 63 - v.leading_zeros() as usize;
        let sub = ((v >> (octave - 3)) & 7) as usize;
        64 + (octave - 6) * 8 + sub
    }

    /// The lowest value in bucket `b`.
    fn low(b: usize) -> u64 {
        if b < 64 {
            return b as u64;
        }
        let (octave, sub) = ((b - 64) / 8 + 6, (b - 64) % 8);
        (8 + sub as u64) << (octave - 3)
    }

    pub fn add(&mut self, v: u64) {
        let b = Self::bucket(v);
        if self.counts.len() <= b {
            self.counts.resize(b + 1, 0);
        }
        self.counts[b] += 1;
        self.n += 1;
    }

    pub fn len(&self) -> u64 {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The `q` quantile (0 to 1), as its bucket's lowest value; 0 when empty.
    pub fn quantile(&self, q: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let rank = ((q * self.n as f64).ceil() as u64).clamp(1, self.n);
        let mut seen = 0;
        for (b, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Self::low(b);
            }
        }
        Self::low(self.counts.len() - 1)
    }
}

/// The run's totals and the per-replay distributions (module docs). Times are in nanoseconds; per-replay
/// distributions of times in microseconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplayCosts {
    /// Replays measured: rewinds for a changed frame.
    pub replays: u64,
    /// (1) Root time in re-stepped live frames, in first-time live frames, and for a vote or finalize off the live
    /// history (`root_at_cut`'s re-step: its grid steps' roots and the one at the cut). A vote that reuses a live root
    /// adds nothing.
    pub root_replayed_ns: u64,
    pub root_live_ns: u64,
    pub root_scratch_ns: u64,
    /// (1) Of the live roots' time, that of roots taken again later at the same frame.
    pub root_discarded_ns: u64,
    /// Live roots taken in re-stepped frames, and those of all live roots taken again later.
    pub roots_replayed: u64,
    pub roots_discarded: u64,
    pub root_per_replay_us: Histogram,
    /// (2) Every-body bookkeeping in re-stepped live frames, and in every live frame.
    pub scan_replayed_ns: u64,
    pub scan_live_ns: u64,
    pub scan_per_replay_us: Histogram,
    /// (3) Frames re-stepped before the changed frame, over all replays.
    pub frames_before_change: u64,
    pub before_per_replay: Histogram,
    /// Frames re-stepped in all, over all replays (for the share (3) is of it).
    pub frames_replayed: u64,
    /// The rest of the split (module docs): the restores; the re-steps' whole time, and within it the physics step,
    /// the keyframes saved and the probe's own digest. Each over all replays.
    pub restore_ns: u64,
    pub restep_ns: u64,
    pub physics_replayed_ns: u64,
    pub keyframe_replayed_ns: u64,
    pub digest_replayed_ns: u64,
    /// The same parts in first-time live steps, for the share replays take of a machine's stepping.
    pub step_live_ns: u64,
    pub physics_live_ns: u64,
    /// A replay's whole time (its restore and its re-steps), and its restore's, per replay.
    pub replay_per_replay_us: Histogram,
    pub restore_per_replay_us: Histogram,
    /// (4) Replays that re-stepped at least one frame the replaced history had stepped (so a digest was compared),
    /// and of those, the ones whose every digest was equal: the physics read nothing new.
    pub replays_compared: u64,
    pub replays_unread: u64,
}

/// One quantity as the summary line shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerReplay {
    pub p50: u64,
    pub p95: u64,
}

impl PerReplay {
    fn of(h: &Histogram) -> PerReplay {
        PerReplay { p50: h.quantile(0.5), p95: h.quantile(0.95) }
    }
}

/// [`ReplayCosts`] as a run's summary line shows it (pe-app's `SimStats::replay_costs`): totals in microseconds,
/// shown in milliseconds.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayCostSummary {
    pub replays: u64,
    pub root_replayed_us: u64,
    pub root_per_replay_us: PerReplay,
    pub roots_replayed: u64,
    pub root_discarded_us: u64,
    pub roots_discarded: u64,
    pub root_live_us: u64,
    pub root_scratch_us: u64,
    pub scan_replayed_us: u64,
    pub scan_per_replay_us: PerReplay,
    pub scan_live_us: u64,
    pub frames_before_change: u64,
    pub before_per_replay: PerReplay,
    pub frames_replayed: u64,
    pub replays_compared: u64,
    pub replays_unread: u64,
    pub restore_us: u64,
    pub restep_us: u64,
    pub physics_replayed_us: u64,
    pub keyframe_replayed_us: u64,
    pub digest_replayed_us: u64,
    pub step_live_us: u64,
    pub physics_live_us: u64,
    pub replay_per_replay_us: PerReplay,
    pub restore_per_replay_us: PerReplay,
}

impl ReplayCostSummary {
    /// The re-steps' time not in any timed part (module docs): the re-steps less physics, roots, scans, keyframes
    /// and the digest.
    pub fn other_replayed_us(&self) -> u64 {
        let parts = self.physics_replayed_us
            + self.root_replayed_us
            + self.scan_replayed_us
            + self.keyframe_replayed_us
            + self.digest_replayed_us;
        self.restep_us.saturating_sub(parts)
    }
}

impl ReplayCosts {
    pub fn summary(&self) -> ReplayCostSummary {
        let us = |ns: u64| ns / 1_000;
        ReplayCostSummary {
            replays: self.replays,
            root_replayed_us: us(self.root_replayed_ns),
            root_per_replay_us: PerReplay::of(&self.root_per_replay_us),
            roots_replayed: self.roots_replayed,
            root_discarded_us: us(self.root_discarded_ns),
            roots_discarded: self.roots_discarded,
            root_live_us: us(self.root_live_ns),
            root_scratch_us: us(self.root_scratch_ns),
            scan_replayed_us: us(self.scan_replayed_ns),
            scan_per_replay_us: PerReplay::of(&self.scan_per_replay_us),
            scan_live_us: us(self.scan_live_ns),
            frames_before_change: self.frames_before_change,
            before_per_replay: PerReplay::of(&self.before_per_replay),
            frames_replayed: self.frames_replayed,
            replays_compared: self.replays_compared,
            replays_unread: self.replays_unread,
            restore_us: us(self.restore_ns),
            restep_us: us(self.restep_ns),
            physics_replayed_us: us(self.physics_replayed_ns),
            keyframe_replayed_us: us(self.keyframe_replayed_ns),
            digest_replayed_us: us(self.digest_replayed_ns),
            step_live_us: us(self.step_live_ns),
            physics_live_us: us(self.physics_live_ns),
            replay_per_replay_us: PerReplay::of(&self.replay_per_replay_us),
            restore_per_replay_us: PerReplay::of(&self.restore_per_replay_us),
        }
    }
}

impl fmt::Debug for ReplayCostSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self;
        let ms = |us: u64| us as f64 / 1e3;
        write!(
            f,
            "replays {}; roots in replays {:.1} ms ({} roots; per replay p50 {} us, p95 {} us), discarded {:.1} ms ({}), \
             live {:.1} ms, vote/finalize roots re-stepped {:.1} ms; body scans in replays {:.1} ms (p50 {} us, p95 {} us), \
             all live {:.1} ms; frames before the change {} of {} re-stepped (p50 {}, p95 {}); physics read nothing \
             new in {} of {} compared; split: restore {:.1} ms (p50 {} us, p95 {} us), re-steps {:.1} ms = physics \
             {:.1} + roots {:.1} + scans {:.1} + keyframes {:.1} + probe digest {:.1} + other {:.1}; per replay p50 {} \
             us, p95 {} us; first-time steps {:.1} ms (physics {:.1})",
            s.replays,
            ms(s.root_replayed_us),
            s.roots_replayed,
            s.root_per_replay_us.p50,
            s.root_per_replay_us.p95,
            ms(s.root_discarded_us),
            s.roots_discarded,
            ms(s.root_live_us),
            ms(s.root_scratch_us),
            ms(s.scan_replayed_us),
            s.scan_per_replay_us.p50,
            s.scan_per_replay_us.p95,
            ms(s.scan_live_us),
            s.frames_before_change,
            s.frames_replayed,
            s.before_per_replay.p50,
            s.before_per_replay.p95,
            s.replays_unread,
            s.replays_compared,
            ms(s.restore_us),
            s.restore_per_replay_us.p50,
            s.restore_per_replay_us.p95,
            ms(s.restep_us),
            ms(s.physics_replayed_us),
            ms(s.root_replayed_us),
            ms(s.scan_replayed_us),
            ms(s.keyframe_replayed_us),
            ms(s.digest_replayed_us),
            ms(s.other_replayed_us()),
            s.replay_per_replay_us.p50,
            s.replay_per_replay_us.p95,
            ms(s.step_live_us),
            ms(s.physics_live_us),
        )
    }
}

/// One replay under way.
#[derive(Debug, Default)]
struct Open {
    root_ns: u64,
    scan_ns: u64,
    restore_ns: u64,
    restep_ns: u64,
    before: u64,
    replayed: u64,
    compared: bool,
    read: bool,
}

/// One step's measures, for [`Probe::step`].
#[derive(Debug, Default)]
pub(crate) struct StepCost {
    pub root_ns: u64,
    pub scan_ns: u64,
    /// The digest of what the physics read ([`PhysicsReads`]); `None` for a scripted world.
    pub digest: Option<u64>,
    /// The step's whole time, and within it the physics step, the keyframe saved and the digest's own time.
    pub step_ns: u64,
    pub physics_ns: u64,
    pub keyframe_ns: u64,
    pub digest_ns: u64,
}

/// A replica's side of the probe: the shared totals, and what it needs to attribute them.
#[derive(Debug)]
pub(crate) struct Probe {
    shared: CostProbe,
    /// The live history's digest at each frame it stepped, kept until the floor passes it: a re-step compares with
    /// the replaced history's.
    digests: BTreeMap<Frame, u64>,
    /// The time each live root took, by the frame it was taken at.
    roots: BTreeMap<Frame, u64>,
    open: Option<Open>,
}

impl Probe {
    pub(crate) fn new(shared: CostProbe) -> Probe {
        Probe { shared, digests: BTreeMap::new(), roots: BTreeMap::new(), open: None }
    }

    fn with<R>(&self, f: impl FnOnce(&mut ReplayCosts) -> R) -> R {
        let mut g = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut g)
    }

    /// A root `root_at_cut`'s re-step took at the cut itself, off the grid steps (`my_root`).
    pub(crate) fn scratch_root(&mut self, ns: u64) {
        self.with(|t| t.root_scratch_ns += ns);
    }

    /// A rewind for a change at `changed`: the keyframe at `restored` was restored, so the frames from it below
    /// `changed` are re-stepped for nothing but the keyframe spacing.
    /// `restore_ns` is the rollback's own time.
    pub(crate) fn rewind(&mut self, changed: Frame, restored: Frame, restore_ns: u64) {
        self.close();
        self.open = Some(Open { before: changed.0.saturating_sub(restored.0), restore_ns, ..Open::default() });
        self.with(|t| t.restore_ns += restore_ns);
    }

    /// A step of frame `f` (`live` or a scratch re-step), `resimulated` if the live history had stepped it before.
    /// `root_at` is the frame a root was taken at, if one was.
    pub(crate) fn step(&mut self, f: Frame, live: bool, resimulated: bool, root_at: Option<Frame>, c: StepCost) {
        if !live {
            self.with(|t| t.root_scratch_ns += c.root_ns);
            return;
        }
        let old = c.digest.and_then(|_| self.digests.get(&f).copied());
        if let Some(o) = self.open.as_mut().filter(|_| resimulated) {
            o.root_ns += c.root_ns;
            o.scan_ns += c.scan_ns;
            o.restep_ns += c.step_ns;
            o.replayed += 1;
            match (c.digest, old) {
                (Some(d), Some(o_d)) => {
                    o.compared = true;
                    o.read |= d != o_d;
                }
                (None, _) => o.read = true,
                (Some(_), None) => {}
            }
        }
        let discarded = root_at.and_then(|at| self.roots.insert(at, c.root_ns));
        if let Some(d) = c.digest {
            self.digests.insert(f, d);
        } else {
            self.digests.remove(&f);
        }
        self.with(|t| {
            t.scan_live_ns += c.scan_ns;
            if resimulated {
                t.root_replayed_ns += c.root_ns;
                t.scan_replayed_ns += c.scan_ns;
                t.roots_replayed += u64::from(root_at.is_some());
                t.restep_ns += c.step_ns;
                t.physics_replayed_ns += c.physics_ns;
                t.keyframe_replayed_ns += c.keyframe_ns;
                t.digest_replayed_ns += c.digest_ns;
            } else {
                t.root_live_ns += c.root_ns;
                t.step_live_ns += c.step_ns;
                t.physics_live_ns += c.physics_ns;
            }
            if let Some(ns) = discarded {
                t.root_discarded_ns += ns;
                t.roots_discarded += 1;
            }
        });
    }

    /// The replica is back where it had been: the open replay ends.
    pub(crate) fn caught_up(&mut self) {
        self.close();
    }

    /// Nothing below `floor` is re-stepped again.
    pub(crate) fn trim(&mut self, floor: Frame) {
        self.digests = self.digests.split_off(&floor);
        self.roots = self.roots.split_off(&floor);
    }

    fn close(&mut self) {
        let Some(o) = self.open.take() else { return };
        self.with(|t| {
            t.replays += 1;
            t.root_per_replay_us.add(o.root_ns / 1_000);
            t.replay_per_replay_us.add((o.restore_ns + o.restep_ns) / 1_000);
            t.restore_per_replay_us.add(o.restore_ns / 1_000);
            t.scan_per_replay_us.add(o.scan_ns / 1_000);
            t.frames_before_change += o.before;
            t.before_per_replay.add(o.before);
            t.frames_replayed += o.replayed;
            t.replays_compared += u64::from(o.compared);
            t.replays_unread += u64::from(o.compared && !o.read);
        });
    }
}

/// The digest of what one step's physics reads (module docs, 4), built as the step goes.
pub(crate) struct PhysicsReads(DefaultHasher);

impl PhysicsReads {
    pub(crate) fn new() -> PhysicsReads {
        PhysicsReads(DefaultHasher::new())
    }

    /// Anything the physics reads, by its debug form: every field, floats by their exact digits.
    pub(crate) fn add(&mut self, tag: u8, v: &dyn fmt::Debug) {
        tag.hash(&mut self.0);
        format!("{v:?}").hash(&mut self.0);
    }

    pub(crate) fn finish(&self) -> u64 {
        self.0.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_histogram_s_quantiles_are_exact_below_64_and_within_an_eighth_above() {
        let mut h = Histogram::default();
        for v in 1..=100 {
            h.add(v);
        }
        assert_eq!((h.len(), h.quantile(0.5)), (100, 50));
        let p95 = h.quantile(0.95);
        assert!((88..=95).contains(&p95), "{p95}");
        for v in [64u64, 100, 1_000, 123_456, u64::MAX / 3] {
            let low = Histogram::low(Histogram::bucket(v));
            assert!(low <= v && v - low <= v / 8, "{v}: {low}");
        }
        assert_eq!(Histogram::default().quantile(0.5), 0);
    }

    #[test]
    fn a_replay_whose_digests_all_match_counts_as_unread_and_one_that_differs_as_read() {
        let shared = probe();
        let mut p = Probe::new(shared.clone());
        let root = |f: u64| (f % 4 == 3).then_some(Frame(f + 1));
        let cost = |f: u64, d: u64| StepCost {
            root_ns: if f % 4 == 3 { 1_000 } else { 0 },
            scan_ns: 2_000,
            digest: Some(d),
            step_ns: 10_000,
            physics_ns: 5_000,
            ..StepCost::default()
        };
        for f in 0..8 {
            p.step(Frame(f), true, false, root(f), cost(f, f));
        }
        // Rewound to 4 for a change at 6: frames 4 and 5 are the spacing's; every digest equal.
        p.rewind(Frame(6), Frame(4), 3_000);
        for f in 4..8 {
            p.step(Frame(f), true, true, root(f), cost(f, f));
        }
        // Rewound to 4 for a change at 5; frame 6's digest differs.
        p.rewind(Frame(5), Frame(4), 3_000);
        for f in 4..8 {
            p.step(Frame(f), true, true, root(f), cost(f, if f == 6 { 99 } else { f }));
        }
        p.caught_up();
        let t = shared.lock().unwrap().clone();
        assert_eq!((t.replays, t.replays_compared, t.replays_unread), (2, 2, 1));
        assert_eq!((t.frames_before_change, t.frames_replayed), (3, 8));
        assert_eq!((t.roots_replayed, t.roots_discarded), (2, 2), "frame 8's root, taken three times");
        assert_eq!((t.root_replayed_ns, t.root_live_ns, t.root_discarded_ns), (2_000, 2_000, 2_000));
        assert_eq!((t.scan_replayed_ns, t.scan_live_ns), (16_000, 32_000));
        assert_eq!((t.root_per_replay_us.quantile(0.5), t.scan_per_replay_us.quantile(0.5)), (1, 8));
        // The split: two restores of 3 us, eight re-steps of 10 us (5 of them physics), eight first-time steps.
        assert_eq!((t.restore_ns, t.restep_ns, t.physics_replayed_ns), (6_000, 80_000, 40_000));
        assert_eq!((t.step_live_ns, t.physics_live_ns), (80_000, 40_000));
        assert_eq!(t.replay_per_replay_us.quantile(0.5), 43, "3 us of restore and four 10 us re-steps");
        let s = t.summary();
        assert_eq!(s.other_replayed_us(), 80 - 40 - 2 - 16, "the re-steps less physics, roots and scans");
        // A scripted step (no digest) counts its replay as read.
        p.rewind(Frame(7), Frame(4), 0);
        p.step(Frame(4), true, true, None, StepCost { digest: None, ..StepCost::default() });
        p.caught_up();
        let t = shared.lock().unwrap().clone();
        assert_eq!((t.replays, t.replays_compared, t.replays_unread), (3, 2, 1));
    }
}
