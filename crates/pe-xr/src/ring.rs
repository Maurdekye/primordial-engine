//! The ring of hand samples the simulation thread reads (spec 6.2).
//!
//! The XR thread pushes one sample per hand per display frame. The simulation thread asks for frame k's
//! hand at frame k's host time, and gets:
//! - between two samples: the interpolation of the two, unless a discontinuity lies between them, in
//!   which case the earlier sample (never a blend across a jump);
//! - past the newest sample: the newest, extrapolated from its velocity for at most one display period,
//!   or as it is when it has no velocity or is not tracked;
//! - older than the ring (after a stall, while the simulation catches up): the oldest sample held,
//!   counted.
//!
//! Neither side waits on a lock: the writer publishes an immutable snapshot with one atomic swap, and a
//! reader loads the current one.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use arc_swap::ArcSwap;

use crate::math::{add, lerp3, qintegrate, qslerp, scale};
use crate::seam::{Hand, HandFeed, HandInput, HostTime, RigPose};

/// How much history the ring keeps, in nanoseconds (about 100 ms).
pub const DEFAULT_WINDOW_NS: i64 = 100_000_000;

#[derive(Clone, Default)]
struct Snapshot {
    /// Per hand, oldest first. Empty while the hand is absent.
    hands: [Vec<HandInput>; 2],
}

/// What the ring has seen, for the counters (spec 9).
#[derive(Debug, Default)]
pub struct RingCounters {
    pub extrapolated: AtomicU64,
    /// Extrapolations cut short at the one-period limit.
    pub extrapolation_clamped: AtomicU64,
    pub older_than_ring: AtomicU64,
    /// The age of the newest sample at each read: how far the asked time is past it (0 when the read falls inside
    /// the ring). Its tail decides whether this arc-swap ring is kept (spec 6.2): any age above one display period,
    /// in a run of minutes with a build alongside, means the fixed ring replaces it.
    pub sample_age: AgeHistogram,
}

/// Width of one [`AgeHistogram`] bucket: 0.25 ms.
pub const AGE_BUCKET_NS: i64 = 250_000;
/// Buckets up to 64 ms; older ages land in the last.
const AGE_BUCKETS: usize = 256;

/// A lock-free histogram of sample ages, with the exact maximum: both sides only ever add, so readers on any thread
/// and the simulation thread's writes never wait on each other.
#[derive(Debug)]
pub struct AgeHistogram {
    buckets: [AtomicU64; AGE_BUCKETS],
    max_ns: AtomicI64,
}

impl Default for AgeHistogram {
    fn default() -> Self {
        AgeHistogram { buckets: std::array::from_fn(|_| AtomicU64::new(0)), max_ns: AtomicI64::new(0) }
    }
}

impl AgeHistogram {
    pub fn record(&self, age_ns: i64) {
        let age = age_ns.max(0);
        let i = ((age / AGE_BUCKET_NS) as usize).min(AGE_BUCKETS - 1);
        self.buckets[i].fetch_add(1, Ordering::Relaxed);
        self.max_ns.fetch_max(age, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum()
    }

    pub fn max_ns(&self) -> i64 {
        self.max_ns.load(Ordering::Relaxed)
    }

    /// The age at or under which a fraction `q` of the reads fell (`0.999` for p99.9), as the upper edge of its
    /// bucket, so never under the truth; capped at the maximum. `None` before any read.
    pub fn quantile_ns(&self, q: f64) -> Option<i64> {
        let counts: Vec<u64> = self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return None;
        }
        let want = ((q.clamp(0.0, 1.0) * total as f64).ceil() as u64).max(1);
        let mut seen = 0;
        for (i, c) in counts.iter().enumerate() {
            seen += c;
            if seen >= want {
                return Some(((i as i64 + 1) * AGE_BUCKET_NS).min(self.max_ns()));
            }
        }
        Some(self.max_ns())
    }
}

pub struct HandRing {
    snapshot: ArcSwap<Snapshot>,
    window_ns: i64,
    /// The longest extrapolation allowed: one display period, set by the frame loop as the runtime reports
    /// it.
    max_extrapolation_ns: AtomicI64,
    pub counters: RingCounters,
}

impl HandRing {
    pub fn new(window_ns: i64, display_period_ns: i64) -> Self {
        HandRing {
            snapshot: ArcSwap::from_pointee(Snapshot::default()),
            window_ns,
            max_extrapolation_ns: AtomicI64::new(display_period_ns),
            counters: RingCounters::default(),
        }
    }

    /// The display period changed (the runtime's rate is whatever it gives, spec 7.2).
    pub fn set_display_period(&self, ns: i64) {
        self.max_extrapolation_ns.store(ns.max(0), Ordering::Relaxed);
    }

    /// Publish a sample. Only the XR thread writes; samples for one hand arrive in time order.
    pub fn push(&self, sample: HandInput) {
        let old = self.snapshot.load();
        let mut next = Snapshot::clone(&old);
        let list = &mut next.hands[sample.hand.index()];
        if list.last().is_some_and(|l| l.sampled_at >= sample.sampled_at) {
            // Out of order or repeated: keep the ring sorted, drop the stale one.
            return;
        }
        list.push(sample);
        let cutoff = sample.sampled_at.0 - self.window_ns;
        // Keep one sample at or before the cutoff, so a request at the window's edge can still interpolate.
        let keep_from = list.iter().rposition(|s| s.sampled_at.0 <= cutoff).unwrap_or(0);
        list.drain(..keep_from);
        self.snapshot.store(Arc::new(next));
    }

    /// The hand became absent (focus lost, session stopped): `hand_at` returns `None` until a new sample.
    pub fn set_absent(&self, hand: Hand) {
        let old = self.snapshot.load();
        if old.hands[hand.index()].is_empty() {
            return;
        }
        let mut next = Snapshot::clone(&old);
        next.hands[hand.index()].clear();
        self.snapshot.store(Arc::new(next));
    }
}

fn interpolate(a: &HandInput, b: &HandInput, at: HostTime) -> HandInput {
    let span = (b.sampled_at.0 - a.sampled_at.0) as f64;
    let t = (((at.0 - a.sampled_at.0) as f64) / span) as f32;
    let lerp_opt = |x: Option<[f32; 3]>, y: Option<[f32; 3]>| match (x, y) {
        (Some(x), Some(y)) => Some(lerp3(x, y, t)),
        _ => None,
    };
    HandInput {
        sampled_at: at,
        pose: RigPose {
            position: lerp3(a.pose.position, b.pose.position, t),
            orientation: qslerp(a.pose.orientation, b.pose.orientation, t),
        },
        linear_velocity: lerp_opt(a.linear_velocity, b.linear_velocity),
        angular_velocity: lerp_opt(a.angular_velocity, b.angular_velocity),
        grip: a.grip + (b.grip - a.grip) * t,
        // Discrete states change at the later sample: the grasp is never anticipated.
        closed: a.closed,
        tracked: a.tracked && b.tracked,
        ..*a
    }
}

impl HandFeed for HandRing {
    fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput> {
        let snap = self.snapshot.load();
        let list = &snap.hands[hand.index()];
        let (first, last) = (list.first()?, list.last()?);

        if at < first.sampled_at {
            self.counters.older_than_ring.fetch_add(1, Ordering::Relaxed);
            return Some(HandInput { sampled_at: at, ..*first });
        }
        self.counters.sample_age.record(at.0 - last.sampled_at.0);
        if at >= last.sampled_at {
            let dt_ns = at.0 - last.sampled_at.0;
            if dt_ns == 0 || !last.tracked {
                return Some(HandInput { sampled_at: at, ..*last });
            }
            let Some(v) = last.linear_velocity else {
                return Some(HandInput { sampled_at: at, ..*last });
            };
            let max = self.max_extrapolation_ns.load(Ordering::Relaxed);
            let used_ns = dt_ns.min(max);
            if used_ns < dt_ns {
                self.counters.extrapolation_clamped.fetch_add(1, Ordering::Relaxed);
            }
            self.counters.extrapolated.fetch_add(1, Ordering::Relaxed);
            let dt = used_ns as f32 * 1e-9;
            let orientation = match last.angular_velocity {
                Some(w) => qintegrate(last.pose.orientation, w, dt),
                None => last.pose.orientation,
            };
            return Some(HandInput {
                sampled_at: at,
                pose: RigPose { position: add(last.pose.position, scale(v, dt)), orientation },
                ..*last
            });
        }
        // first <= at < last: find the bracketing pair.
        let i = list.partition_point(|s| s.sampled_at <= at); // list[i-1] <= at < list[i]
        let (a, b) = (&list[i - 1], &list[i]);
        if a.sampled_at == at || a.discontinuity != b.discontinuity || a.source != b.source {
            return Some(HandInput { sampled_at: at, ..*a });
        }
        Some(interpolate(a, b, at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::HandSource;

    const PERIOD: i64 = 8_333_333; // 120 Hz

    fn sample(ms: f64, x: f32, vx: Option<f32>) -> HandInput {
        HandInput {
            hand: Hand::Right,
            source: HandSource::Controller,
            sampled_at: HostTime((ms * 1e6) as i64),
            tracked: true,
            pose: RigPose { position: [x, 1.0, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
            linear_velocity: vx.map(|v| [v, 0.0, 0.0]),
            angular_velocity: None,
            grip: x,
            closed: false,
            discontinuity: 0,
        }
    }

    fn t(ms: f64) -> HostTime {
        HostTime((ms * 1e6) as i64)
    }

    #[test]
    fn absent_until_the_first_sample() {
        let r = HandRing::new(DEFAULT_WINDOW_NS, PERIOD);
        assert_eq!(r.hand_at(Hand::Right, t(0.0)), None);
        r.push(sample(0.0, 0.0, None));
        assert!(r.hand_at(Hand::Right, t(0.0)).is_some());
        assert_eq!(r.hand_at(Hand::Left, t(0.0)), None, "the other hand is still absent");
        r.set_absent(Hand::Right);
        assert_eq!(r.hand_at(Hand::Right, t(0.0)), None);
    }

    #[test]
    fn interpolates_between_samples() {
        let r = HandRing::new(DEFAULT_WINDOW_NS, PERIOD);
        r.push(sample(0.0, 0.0, Some(1.0)));
        r.push(sample(10.0, 0.01, Some(1.0)));
        let h = r.hand_at(Hand::Right, t(2.5)).unwrap();
        assert!((h.pose.position[0] - 0.0025).abs() < 1e-6, "{h:?}");
        assert_eq!(h.sampled_at, t(2.5));
        assert!((h.grip - 0.0025).abs() < 1e-6);
    }

    #[test]
    fn never_blends_across_a_discontinuity() {
        let r = HandRing::new(DEFAULT_WINDOW_NS, PERIOD);
        r.push(sample(0.0, 0.0, None));
        let mut jump = sample(10.0, 0.5, None);
        jump.discontinuity = 1;
        r.push(jump);
        assert_eq!(r.hand_at(Hand::Right, t(9.0)).unwrap().pose.position[0], 0.0);
        assert_eq!(r.hand_at(Hand::Right, t(10.0)).unwrap().pose.position[0], 0.5);
    }

    #[test]
    fn extrapolates_at_most_one_display_period() {
        let r = HandRing::new(DEFAULT_WINDOW_NS, PERIOD);
        r.push(sample(0.0, 0.0, Some(2.0))); // 2 m/s
        let h = r.hand_at(Hand::Right, t(4.0)).unwrap();
        assert!((h.pose.position[0] - 0.008).abs() < 1e-5, "4 ms at 2 m/s: {h:?}");
        let far = r.hand_at(Hand::Right, t(50.0)).unwrap();
        let limit = 2.0 * PERIOD as f32 * 1e-9;
        assert!((far.pose.position[0] - limit).abs() < 1e-5, "clamped to one period: {far:?}");
        assert_eq!(r.counters.extrapolated.load(Ordering::Relaxed), 2);
        assert_eq!(r.counters.extrapolation_clamped.load(Ordering::Relaxed), 1);
        r.set_display_period(PERIOD / 2);
        let far = r.hand_at(Hand::Right, t(50.0)).unwrap();
        assert!((far.pose.position[0] - limit / 2.0).abs() < 1e-5, "a new period takes effect");
    }

    #[test]
    fn no_velocity_or_untracked_means_no_extrapolation() {
        let r = HandRing::new(DEFAULT_WINDOW_NS, PERIOD);
        r.push(sample(0.0, 0.3, None));
        assert_eq!(r.hand_at(Hand::Right, t(5.0)).unwrap().pose.position[0], 0.3);
        let mut lost = sample(1.0, 0.3, Some(5.0));
        lost.tracked = false;
        r.push(lost);
        assert_eq!(r.hand_at(Hand::Right, t(5.0)).unwrap().pose.position[0], 0.3);
        assert_eq!(r.counters.extrapolated.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_sample_age_histogram_keeps_the_tail() {
        let h = AgeHistogram::default();
        assert_eq!(h.quantile_ns(0.999), None);
        for _ in 0..9_990 {
            h.record(100_000); // 0.1 ms
        }
        for _ in 0..10 {
            h.record(9_000_000); // 9 ms: the 0.1 % tail
        }
        h.record(-5); // a read inside the ring
        assert_eq!(h.count(), 10_001);
        assert_eq!(h.max_ns(), 9_000_000);
        assert_eq!(h.quantile_ns(0.5), Some(AGE_BUCKET_NS));
        assert_eq!(h.quantile_ns(0.999), Some(AGE_BUCKET_NS), "10 of 10 001 is under 0.1 %");
        h.record(9_000_000);
        h.record(9_000_000);
        assert_eq!(h.quantile_ns(0.999), Some(9_000_000), "capped at the maximum, inside its bucket");
        h.record(1_000_000_000);
        assert_eq!(h.max_ns(), 1_000_000_000, "past the last bucket, the maximum is still exact");
    }

    #[test]
    fn reads_record_the_newest_samples_age() {
        let r = HandRing::new(DEFAULT_WINDOW_NS, 8_333_333);
        r.push(sample(1000.0, 0.0, None));
        r.push(sample(1008.0, 0.0, None));
        r.hand_at(Hand::Right, HostTime(1_004_000_000)); // inside: age 0
        r.hand_at(Hand::Right, HostTime(1_011_000_000)); // 3 ms past the newest
        assert_eq!(r.counters.sample_age.count(), 2);
        assert_eq!(r.counters.sample_age.max_ns(), 3_000_000);
    }

    #[test]
    fn older_than_the_ring_returns_the_oldest_and_counts_it() {
        let r = HandRing::new(20_000_000, PERIOD); // a 20 ms window
        for i in 0..10 {
            r.push(sample(i as f64 * 8.0, i as f32, None));
        }
        let h = r.hand_at(Hand::Right, t(0.0)).unwrap();
        // Samples at 0..72 ms; the window keeps from the last one at or before 72 - 20 = 52 ms: 48 ms.
        assert_eq!(h.pose.position[0], 6.0);
        assert_eq!(h.sampled_at, t(0.0));
        assert_eq!(r.counters.older_than_ring.load(Ordering::Relaxed), 1);
        // A time inside the window still interpolates.
        let h = r.hand_at(Hand::Right, t(52.0)).unwrap();
        assert!((h.pose.position[0] - 6.5).abs() < 1e-5, "{h:?}");
    }

    #[test]
    fn stale_or_repeated_samples_are_dropped() {
        let r = HandRing::new(DEFAULT_WINDOW_NS, PERIOD);
        r.push(sample(10.0, 1.0, None));
        r.push(sample(10.0, 2.0, None));
        r.push(sample(5.0, 3.0, None));
        assert_eq!(r.hand_at(Hand::Right, t(10.0)).unwrap().pose.position[0], 1.0);
    }

    #[test]
    fn a_reader_on_another_thread_and_the_writer_run_concurrently() {
        let r = Arc::new(HandRing::new(DEFAULT_WINDOW_NS, PERIOD));
        let reader = {
            let r = r.clone();
            std::thread::spawn(move || {
                let mut seen = 0u64;
                for i in 0..20_000 {
                    if r.hand_at(Hand::Right, t(i as f64 * 0.01)).is_some() {
                        seen += 1;
                    }
                }
                seen
            })
        };
        for i in 0..2_000 {
            r.push(sample(i as f64 * 0.1, 0.0, Some(0.0)));
        }
        let _ = reader.join().unwrap();
        assert!(r.hand_at(Hand::Right, t(199.9)).is_some());
    }
}
