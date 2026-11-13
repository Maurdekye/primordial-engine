//! pe-render's counters (render.md 13; divergence.md 7; xr.md 9), per second and as distributions over a run.
//!
//! - **Fed by** the [`crate::present::Presenter`], which the window and the headset share: each display frame folds
//!   the display layer's counters for that frame and its drawn time; each draw folds the renderer's [`FrameStats`]
//!   and the quality it drew at. Whoever schedules captures reports their [`CaptureEvent`]s.
//! - **Read by** pe-app, which passes them on to pe-diverge's table, pe-xr's and MCP. pe-render pushes nothing.
//! - **Seconds** are closed as time passes them, on the host clock, from the first event. The newest
//!   [`KEPT_SECONDS`] are kept; every closed second also feeds the run's distributions of per-second values, so a
//!   long run loses no statistics.
//! - **Distributions** are log-bucket histograms: quantiles are within 1 % (they report the bucket's upper edge),
//!   means and maxima are exact.

use std::collections::{BTreeMap, VecDeque};

use pe_seams::HostTime;

use crate::display::{DisplayCounters, DisplayFrame};
use crate::pacing::Quality;
use crate::scene::FrameStats;

/// Closed seconds kept for reading (ten minutes).
pub const KEPT_SECONDS: usize = 600;

const NANOS_PER_SEC: i64 = 1_000_000_000;
/// Histogram range: values at or below `LOW` count as zero; one bucket per 1 % up to `LOW · 1.01^BUCKETS`.
const LOW: f64 = 1e-6;
const RATIO: f64 = 1.01;
const BUCKETS: usize = 2800;

/// A distribution of non-negative values.
#[derive(Clone, Debug, PartialEq)]
pub struct Histogram {
    zero: u64,
    buckets: Vec<u64>,
    count: u64,
    sum: f64,
    max: f64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram { zero: 0, buckets: vec![0; BUCKETS], count: 0, sum: 0.0, max: 0.0 }
    }
}

/// Count, mean, quantiles (within 1 %) and maximum of a [`Histogram`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    pub count: u64,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

impl Histogram {
    /// Record `v`; negative values are recorded as zero, NaN is ignored.
    pub fn record(&mut self, v: f64) {
        if v.is_nan() {
            return;
        }
        let v = v.max(0.0);
        self.count += 1;
        self.sum += v;
        self.max = self.max.max(v);
        if v <= LOW {
            self.zero += 1;
        } else {
            let i = ((v / LOW).ln() / RATIO.ln()).floor() as usize;
            self.buckets[i.min(BUCKETS - 1)] += 1;
        }
    }

    /// The value at quantile `q` (0 to 1): the upper edge of its bucket, capped at the maximum seen.
    pub fn quantile(&self, q: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let rank = ((q.clamp(0.0, 1.0) * self.count as f64).ceil() as u64).max(1);
        if rank <= self.zero {
            return 0.0;
        }
        let mut seen = self.zero;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return (LOW * RATIO.powi(i as i32 + 1)).min(self.max);
            }
        }
        self.max
    }

    pub fn summary(&self) -> Summary {
        Summary {
            count: self.count,
            mean: if self.count == 0 { 0.0 } else { self.sum / self.count as f64 },
            p50: self.quantile(0.5),
            p95: self.quantile(0.95),
            p99: self.quantile(0.99),
            max: self.max,
        }
    }
}

/// A capture's progress, as the capture port reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureEvent {
    /// Accepted, waiting for a slot between frames.
    Queued,
    /// Recorded and submitted.
    Drawn,
    /// Put off a frame because the frame budget was tight.
    Deferred,
    /// The caller's deadline passed first.
    TimedOut,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureCounts {
    pub queued: u64,
    pub drawn: u64,
    pub deferred: u64,
    pub timed_out: u64,
}

impl CaptureCounts {
    fn add(&mut self, e: CaptureEvent) {
        match e {
            CaptureEvent::Queued => self.queued += 1,
            CaptureEvent::Drawn => self.drawn += 1,
            CaptureEvent::Deferred => self.deferred += 1,
            CaptureEvent::TimedOut => self.timed_out += 1,
        }
    }
}

/// One second of counters. Distances in metres, times in milliseconds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Second {
    pub start: HostTime,
    /// The display layer's counters over this second: sums and counts added, maxima the largest.
    pub display: DisplayCounters,
    /// Draws recorded, and the largest CPU recording time and GPU time among them (GPU: readings that arrived).
    pub draws: u64,
    pub cpu_record_ms_max: f64,
    pub gpu_ms_max: Option<f64>,
    /// The lowest render scale drawn at, and the frames drawn at each MSAA level.
    pub render_scale_min: Option<f32>,
    pub msaa_frames: BTreeMap<u32, u64>,
    pub captures: CaptureCounts,
}

impl Second {
    fn new(start: HostTime) -> Second {
        Second { start, ..Second::default() }
    }

    /// `visible_error`'s mean over this second (objects in view, per display frame), in metres.
    pub fn visible_error_mean(&self) -> f64 {
        let d = &self.display;
        if d.visible_samples == 0 { 0.0 } else { d.visible_error_sum / d.visible_samples as f64 }
    }

    /// `extrapolation_error`'s mean over this second's misses, in metres.
    pub fn extrapolation_error_mean(&self) -> f64 {
        let d = &self.display;
        if d.extrapolation_misses == 0 { 0.0 } else { d.extrapolation_error_sum / d.extrapolation_misses as f64 }
    }
}

/// Fold one frame's display counters (taken, so per frame) into a sum over a span.
fn fold(acc: &mut DisplayCounters, c: &DisplayCounters) {
    acc.corrections += c.corrections;
    acc.extrapolation_misses += c.extrapolation_misses;
    acc.extrapolation_error_max = acc.extrapolation_error_max.max(c.extrapolation_error_max);
    acc.extrapolation_error_sum += c.extrapolation_error_sum;
    acc.hold_ends += c.hold_ends;
    acc.snaps_in_view += c.snaps_in_view;
    acc.jumps_out_of_view += c.jumps_out_of_view;
    acc.cues += c.cues;
    acc.cueless_large_corrections += c.cueless_large_corrections;
    acc.own_error = acc.own_error.max(c.own_error);
    acc.visible_error_max = acc.visible_error_max.max(c.visible_error_max);
    acc.visible_error_sum += c.visible_error_sum;
    acc.visible_samples += c.visible_samples;
    acc.display_frames += c.display_frames;
    acc.views_taken += c.views_taken;
}

/// The run so far: totals, and distributions per display frame and per second.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunReport {
    /// Closed seconds, and those of them that displayed frames.
    pub seconds: u64,
    pub active_seconds: u64,
    pub display: DisplayCounters,
    pub captures: CaptureCounts,
    /// `corrections_per_min` over the closed seconds that displayed frames.
    pub corrections_per_min: f64,
    /// Per display frame: CPU recording time, GPU time (readings that arrived), how far the drawn time is behind
    /// the display time (`display_time − t_draw`: 0 under X8 (a), `D_render` under (b)), the frame view's age at
    /// the display time, and the render scale.
    pub cpu_record_ms: Summary,
    pub gpu_ms: Summary,
    pub drawn_behind_ms: Summary,
    pub view_age_ms: Summary,
    pub render_scale: Summary,
    pub msaa_frames: BTreeMap<u32, u64>,
    /// Per second that displayed frames: `visible_error`'s mean and max, and corrections.
    pub visible_error_mean: Summary,
    pub visible_error_max: Summary,
    pub corrections_per_second: Summary,
}

/// pe-render's counters for one renderer, desktop or headset.
#[derive(Clone, Debug, Default)]
pub struct RenderCounters {
    current: Option<Second>,
    kept: VecDeque<Second>,
    closed: u64,
    active: u64,
    active_corrections: u64,
    run_display: DisplayCounters,
    run_captures: CaptureCounts,
    run_msaa: BTreeMap<u32, u64>,
    cpu_record_ms: Histogram,
    gpu_ms: Histogram,
    drawn_behind_ms: Histogram,
    view_age_ms: Histogram,
    render_scale: Histogram,
    second_visible_mean: Histogram,
    second_visible_max: Histogram,
    second_corrections: Histogram,
}

impl RenderCounters {
    pub fn new() -> RenderCounters {
        RenderCounters::default()
    }

    /// Close every second that `at` has passed, and return the open one.
    fn at(&mut self, at: HostTime) -> &mut Second {
        let mut cur = self.current.take().unwrap_or_else(|| Second::new(at));
        let passed = (at.0 - cur.start.0).div_euclid(NANOS_PER_SEC);
        if passed > 0 {
            let next = HostTime(cur.start.0 + passed * NANOS_PER_SEC);
            let s0 = cur.start.0;
            self.close(cur);
            // A gap (a minimized window, a paused headset) leaves empty seconds, s0+1 to s0+passed-1. All count, as
            // seconds with nothing in them; only the newest KEPT_SECONDS are made, since older ones would be dropped.
            let first = (passed - KEPT_SECONDS as i64).max(1);
            // Seconds too old to keep are empty, so only counted.
            self.closed += (first - 1) as u64;
            for k in first..passed {
                self.close(Second::new(HostTime(s0 + k * NANOS_PER_SEC)));
            }
            cur = Second::new(next);
        }
        self.current.insert(cur)
    }

    fn close(&mut self, s: Second) {
        self.closed += 1;
        // Per-second distributions and corrections_per_min cover seconds that displayed frames: a minimized window
        // must not dilute them with zeros.
        if s.display.display_frames > 0 {
            self.active += 1;
            self.active_corrections += s.display.corrections;
            self.second_visible_mean.record(s.visible_error_mean());
            self.second_visible_max.record(s.display.visible_error_max);
            self.second_corrections.record(s.display.corrections as f64);
        }
        self.kept.push_back(s);
        while self.kept.len() > KEPT_SECONDS {
            self.kept.pop_front();
        }
    }

    /// One display frame: `taken` is the display layer's counters for this frame alone
    /// ([`crate::display::DisplayLayer::take_counters`]), `frame` its result, `display_time` the time it is shown.
    pub fn display_frame(
        &mut self,
        at: HostTime,
        taken: &DisplayCounters,
        frame: &DisplayFrame,
        display_time: HostTime,
    ) {
        fold(&mut self.at(at).display, taken);
        fold(&mut self.run_display, taken);
        self.drawn_behind_ms.record((display_time.0 - frame.t_draw.0) as f64 / 1e6);
        self.view_age_ms.record(frame.view_age * 1e3);
    }

    /// One draw, at `quality`.
    pub fn drawn(&mut self, at: HostTime, stats: &FrameStats, quality: Quality) {
        let s = self.at(at);
        s.draws += 1;
        s.cpu_record_ms_max = s.cpu_record_ms_max.max(stats.cpu_record_ms);
        if let Some(g) = stats.gpu_ms {
            s.gpu_ms_max = Some(s.gpu_ms_max.map_or(g, |m| m.max(g)));
        }
        s.render_scale_min = Some(s.render_scale_min.map_or(quality.render_scale, |m| m.min(quality.render_scale)));
        *s.msaa_frames.entry(quality.msaa).or_default() += 1;
        *self.run_msaa.entry(quality.msaa).or_default() += 1;
        self.cpu_record_ms.record(stats.cpu_record_ms);
        if let Some(g) = stats.gpu_ms {
            self.gpu_ms.record(g);
        }
        self.render_scale.record(quality.render_scale as f64);
    }

    pub fn capture(&mut self, at: HostTime, e: CaptureEvent) {
        self.at(at).captures.add(e);
        self.run_captures.add(e);
    }

    /// The closed seconds kept, oldest first.
    pub fn seconds(&self) -> impl Iterator<Item = &Second> {
        self.kept.iter()
    }

    /// The newest closed second.
    pub fn last_second(&self) -> Option<&Second> {
        self.kept.back()
    }

    /// The run so far. Totals include the open second; per-second distributions only closed ones.
    ///
    /// It walks every histogram (about 70k steps), so read it once a second or on request, never per frame.
    pub fn run(&self) -> RunReport {
        RunReport {
            seconds: self.closed,
            active_seconds: self.active,
            display: self.run_display,
            captures: self.run_captures,
            corrections_per_min: if self.active == 0 {
                0.0
            } else {
                self.active_corrections as f64 * 60.0 / self.active as f64
            },
            cpu_record_ms: self.cpu_record_ms.summary(),
            gpu_ms: self.gpu_ms.summary(),
            drawn_behind_ms: self.drawn_behind_ms.summary(),
            view_age_ms: self.view_age_ms.summary(),
            render_scale: self.render_scale.summary(),
            msaa_frames: self.run_msaa.clone(),
            visible_error_mean: self.second_visible_mean.summary(),
            visible_error_max: self.second_visible_max.summary(),
            corrections_per_second: self.second_corrections.summary(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: i64) -> HostTime {
        HostTime::from_millis(ms)
    }

    fn frame(t_draw: HostTime, view_age: f64) -> DisplayFrame {
        DisplayFrame { t_draw, span: 0.0, view_age, objects: Vec::new() }
    }

    #[test]
    fn histogram_quantiles_are_within_one_percent_and_the_max_is_exact() {
        let mut h = Histogram::default();
        for i in 1..=1000 {
            h.record(i as f64 * 0.01);
        }
        let s = h.summary();
        assert_eq!(s.count, 1000);
        assert!((s.mean - 5.005).abs() < 1e-9);
        assert_eq!(s.max, 10.0);
        for (got, want) in [(s.p50, 5.0), (s.p95, 9.5), (s.p99, 9.9)] {
            assert!(got >= want && got <= want * 1.0101, "{got} against {want}");
        }
        let mut z = Histogram::default();
        z.record(0.0);
        z.record(-3.0);
        z.record(f64::NAN);
        assert_eq!((z.summary().count, z.quantile(1.0)), (2, 0.0));
    }

    #[test]
    fn seconds_close_as_time_passes_and_fold_the_frames_in_them() {
        let mut c = RenderCounters::new();
        let one = DisplayCounters {
            corrections: 1,
            visible_error_max: 0.02,
            visible_error_sum: 0.03,
            visible_samples: 3,
            display_frames: 1,
            ..Default::default()
        };
        // 90 frames in the first second, 45 in the second, then one in the fourth: the third is empty.
        for i in 0..90 {
            let at = t(i * 11);
            c.display_frame(at, &one, &frame(at, 0.02), at);
        }
        for i in 0..45 {
            let at = t(1000 + i * 22);
            c.display_frame(at, &DisplayCounters { visible_error_max: 0.05, ..one }, &frame(at, 0.02), at);
        }
        let at = t(3100);
        c.display_frame(at, &one, &frame(at, 0.02), at);
        let s: Vec<_> = c.seconds().collect();
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].start, s[1].start, s[2].start), (t(0), t(1000), t(2000)));
        assert_eq!(
            (s[0].display.display_frames, s[1].display.display_frames, s[2].display.display_frames),
            (90, 45, 0)
        );
        assert_eq!((s[0].display.visible_error_max, s[1].display.visible_error_max), (0.02, 0.05));
        assert!((s[0].visible_error_mean() - 0.01).abs() < 1e-12);
        let r = c.run();
        assert_eq!(r.seconds, 3);
        assert_eq!(r.display.corrections, 136, "totals include the open second");
        // 135 corrections in the 2 closed seconds that displayed frames; the empty one dilutes nothing.
        assert_eq!(r.active_seconds, 2);
        assert!((r.corrections_per_min - 4050.0).abs() < 1e-9, "{}", r.corrections_per_min);
        assert_eq!(r.visible_error_max.max, 0.05);
        assert_eq!(r.corrections_per_second.count, 2);
        assert!(r.visible_error_mean.p50 >= 0.01, "an empty second would put a zero here");
    }

    #[test]
    fn a_long_gap_keeps_only_the_newest_seconds_but_counts_them_all() {
        let mut c = RenderCounters::new();
        let one = DisplayCounters { display_frames: 1, ..Default::default() };
        c.display_frame(t(0), &one, &frame(t(0), 0.0), t(0));
        let late = t(3_600_000 + 500);
        c.display_frame(late, &one, &frame(late, 0.0), late);
        assert_eq!(c.run().seconds, 3600);
        assert_eq!(c.seconds().count(), KEPT_SECONDS);
        let starts: Vec<_> = c.seconds().map(|s| s.start).collect();
        assert!(starts.windows(2).all(|w| w[1].0 - w[0].0 == NANOS_PER_SEC), "kept seconds are consecutive");
        assert_eq!(c.last_second().unwrap().start, t(3_599_000));
    }

    #[test]
    fn draws_record_times_quality_and_the_drawn_time_behind() {
        let mut c = RenderCounters::new();
        let q = |s, m| Quality { render_scale: s, msaa: m };
        c.drawn(t(0), &FrameStats { cpu_record_ms: 1.5, gpu_ms: None, ..FrameStats::default() }, q(1.0, 4));
        c.drawn(t(10), &FrameStats { cpu_record_ms: 0.5, gpu_ms: Some(2.0), ..FrameStats::default() }, q(0.8, 2));
        // X8 (b): drawn 40 ms behind the display time; the view 25 ms old.
        let d = DisplayCounters::default();
        c.display_frame(t(10), &d, &frame(t(-30), 0.025), t(10));
        c.capture(t(10), CaptureEvent::Queued);
        c.capture(t(20), CaptureEvent::Deferred);
        c.capture(t(30), CaptureEvent::Drawn);
        c.display_frame(t(1500), &d, &frame(t(1500), 0.0), t(1500));
        let s = c.last_second().unwrap();
        assert_eq!((s.draws, s.cpu_record_ms_max, s.gpu_ms_max, s.render_scale_min), (2, 1.5, Some(2.0), Some(0.8)));
        assert_eq!(s.msaa_frames, BTreeMap::from([(2, 1), (4, 1)]));
        assert_eq!(s.captures, CaptureCounts { queued: 1, drawn: 1, deferred: 1, timed_out: 0 });
        let r = c.run();
        assert_eq!((r.gpu_ms.count, r.cpu_record_ms.count), (1, 2));
        assert_eq!(r.drawn_behind_ms.max, 40.0);
        assert!((r.view_age_ms.max - 25.0).abs() < 1e-9);
        assert_eq!(r.drawn_behind_ms.count, 2);
    }
}
