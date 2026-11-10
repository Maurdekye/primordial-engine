//! Pacing to what the display gives, and the render-scale controller (render.md 10).
//!
//! - **The period** is whatever the display reports: the runtime's predicted display period in VR (every frame),
//!   the monitor's refresh rate on the desktop, else the measured present interval. pe-render never assumes 90 or
//!   120 Hz [R m981c546957f2].
//! - **Budget per period:** over budget in more than a quarter of the frames of the last 0.25 s drops a step; within
//!   70% of budget for 2 s goes back up a step. In VR, MSAA drops before the render scale. Every limit here is a
//!   duration, never a frame count [R md4b42d16d9df].

use std::collections::VecDeque;
use std::time::Duration;

use pe_seams::HostTime;

/// Where the display period comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PeriodSource {
    /// The runtime's predicted display period for this frame (VR).
    Runtime(Duration),
    /// The monitor's reported refresh rate, in millihertz (winit's `refresh_rate_millihertz`).
    Monitor(u32),
    /// Neither: use the measured present interval.
    Measured,
}

/// One step of the quality ladder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quality {
    pub render_scale: f32,
    pub msaa: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PacerSettings {
    /// The window over which "over budget" is judged, and the share of its frames that trips a drop.
    pub over_window: Duration,
    pub over_share: f64,
    /// How long every frame must stay within `headroom` of the budget before stepping back up.
    pub up_window: Duration,
    pub headroom: f64,
    /// How long measured present intervals are kept for the measured period.
    pub measure_window: Duration,
}

impl Default for PacerSettings {
    fn default() -> Self {
        PacerSettings {
            over_window: Duration::from_millis(250),
            over_share: 0.25,
            up_window: Duration::from_secs(2),
            headroom: 0.7,
            measure_window: Duration::from_millis(500),
        }
    }
}

/// The quality ladder for VR: MSAA 4, 2, 1 at full scale, then the render scale down to 0.7 in steps of 0.1.
pub const VR_LADDER: [Quality; 6] = [
    Quality { render_scale: 1.0, msaa: 4 },
    Quality { render_scale: 1.0, msaa: 2 },
    Quality { render_scale: 1.0, msaa: 1 },
    Quality { render_scale: 0.9, msaa: 1 },
    Quality { render_scale: 0.8, msaa: 1 },
    Quality { render_scale: 0.7, msaa: 1 },
];

/// The desktop's ladder: MSAA stays at 4; the render scale goes down to 0.7.
pub const DESKTOP_LADDER: [Quality; 4] = [
    Quality { render_scale: 1.0, msaa: 4 },
    Quality { render_scale: 0.9, msaa: 4 },
    Quality { render_scale: 0.8, msaa: 4 },
    Quality { render_scale: 0.7, msaa: 4 },
];

#[derive(Clone, Copy, Debug)]
struct Sample {
    at: HostTime,
    /// The frame's cost (the larger of CPU recording and GPU time) over its period.
    load: f64,
}

/// The pacer: the display period, the display time of the next frame, and the quality step.
pub struct Pacer {
    settings: PacerSettings,
    ladder: &'static [Quality],
    step: usize,
    since_change: Option<HostTime>,
    samples: VecDeque<Sample>,
    presents: VecDeque<HostTime>,
    period: Duration,
}

impl Pacer {
    pub fn new(settings: PacerSettings, ladder: &'static [Quality]) -> Pacer {
        assert!(!ladder.is_empty());
        Pacer {
            settings,
            ladder,
            step: 0,
            since_change: None,
            samples: VecDeque::new(),
            presents: VecDeque::new(),
            // A first guess only, until the display reports or a present is measured.
            period: Duration::from_micros(16_667),
        }
    }

    pub fn quality(&self) -> Quality {
        self.ladder[self.step]
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    /// Record a present at `at` (desktop), for the measured period.
    pub fn presented(&mut self, at: HostTime) {
        self.presents.push_back(at);
        let keep = self.settings.measure_window;
        while self.presents.front().is_some_and(|t| t.plus(keep) < at) {
            self.presents.pop_front();
        }
    }

    /// The period for this frame, from `source`; the median measured interval when the display reports nothing.
    pub fn update_period(&mut self, source: PeriodSource) -> Duration {
        self.period = match source {
            PeriodSource::Runtime(p) if !p.is_zero() => p,
            PeriodSource::Monitor(mhz) if mhz > 0 => Duration::from_nanos(1_000_000_000_000 / mhz as u64),
            _ => {
                let mut iv: Vec<i64> =
                    self.presents.iter().zip(self.presents.iter().skip(1)).map(|(a, b)| b.0 - a.0).collect();
                if iv.is_empty() {
                    self.period
                } else {
                    iv.sort_unstable();
                    Duration::from_nanos(iv[iv.len() / 2].max(1) as u64)
                }
            }
        };
        self.period
    }

    /// The desktop's display time for a frame started at `now`: the next present, now plus one period.
    pub fn desktop_display_time(&self, now: HostTime) -> HostTime {
        now.plus(self.period)
    }

    /// Record a finished frame's costs at `at`, against this frame's period; maybe change the quality step.
    /// Returns the new quality when it changed.
    pub fn frame_done(&mut self, at: HostTime, cpu: Duration, gpu: Option<Duration>) -> Option<Quality> {
        let cost = cpu.max(gpu.unwrap_or_default()).as_secs_f64();
        let load = cost / self.period.as_secs_f64().max(1e-6);
        self.samples.push_back(Sample { at, load });
        let s = self.settings;
        let longest = s.up_window.max(s.over_window);
        while self.samples.front().is_some_and(|x| x.at.plus(longest) < at) {
            self.samples.pop_front();
        }
        let since = self.since_change.unwrap_or(self.samples.front().map_or(at, |x| x.at));
        let recent: Vec<&Sample> =
            self.samples.iter().filter(|x| x.at.plus(s.over_window) >= at && x.at >= since).collect();
        // Judge "over" only once a full window has passed since the last change, so one change settles first.
        let over_ready = since.plus(s.over_window) <= at;
        if over_ready && !recent.is_empty() {
            let over = recent.iter().filter(|x| x.load > 1.0).count() as f64 / recent.len() as f64;
            if over > s.over_share && self.step + 1 < self.ladder.len() {
                self.step += 1;
                self.since_change = Some(at);
                return Some(self.quality());
            }
        }
        let up_ready = since.plus(s.up_window) <= at;
        if up_ready && self.step > 0 {
            let all_fine = self
                .samples
                .iter()
                .filter(|x| x.at >= since && x.at.plus(s.up_window) >= at)
                .all(|x| x.load <= s.headroom);
            if all_fine {
                self.step -= 1;
                self.since_change = Some(at);
                return Some(self.quality());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(p: &mut Pacer, from: HostTime, secs: f64, hz: f64, cost_ms: f64) -> (HostTime, Vec<Quality>) {
        let mut changes = Vec::new();
        let n = (secs * hz) as i64;
        let mut t = from;
        for _ in 0..n {
            t = t.plus_nanos((1e9 / hz) as i64);
            p.update_period(PeriodSource::Runtime(Duration::from_secs_f64(1.0 / hz)));
            if let Some(q) = p.frame_done(t, Duration::from_secs_f64(cost_ms / 1e3), None) {
                changes.push(q);
            }
        }
        (t, changes)
    }

    #[test]
    fn over_budget_drops_msaa_before_scale_and_recovers() {
        for hz in [72.0, 90.0, 120.0] {
            let mut p = Pacer::new(PacerSettings::default(), &VR_LADDER);
            let period_ms = 1e3 / hz;
            // Steadily 20% over budget: steps down one per 0.25 s window, MSAA first.
            let (t, down) = run(&mut p, HostTime(0), 0.8, hz, period_ms * 1.2);
            assert!(down.len() >= 2, "{hz}: {down:?}");
            assert_eq!(down[0], VR_LADDER[1]);
            assert_eq!(down[1], VR_LADDER[2]);
            // Comfortably within budget: frames of the overloaded run still in the last 0.25 s may trip one more
            // drop; then, after 2 s within budget, exactly one step back up from the lowest step reached.
            let (_, after) = run(&mut p, t, 2.6, hz, period_ms * 0.5);
            let lowest =
                down.iter().chain(&after).map(|q| VR_LADDER.iter().position(|l| l == q).unwrap()).max().unwrap();
            assert!(after.len() <= 2, "{hz}: {after:?}");
            assert_eq!(p.quality(), VR_LADDER[lowest - 1], "{hz}: {down:?} then {after:?}");
        }
    }

    #[test]
    fn a_single_miss_changes_nothing() {
        let mut p = Pacer::new(PacerSettings::default(), &DESKTOP_LADDER);
        let (t, c) = run(&mut p, HostTime(0), 1.0, 165.0, 3.0);
        assert!(c.is_empty());
        p.frame_done(t.plus_nanos(6_000_000), Duration::from_millis(30), None);
        let (_, c) = run(&mut p, t.plus_nanos(6_000_000), 1.0, 165.0, 3.0);
        assert!(c.is_empty(), "{c:?}");
    }

    #[test]
    fn the_period_follows_the_display() {
        let mut p = Pacer::new(PacerSettings::default(), &DESKTOP_LADDER);
        assert_eq!(p.update_period(PeriodSource::Monitor(164_999)), Duration::from_nanos(6_060_642));
        for k in 0..20 {
            p.presented(HostTime(k * 13_888_889));
        }
        assert_eq!(p.update_period(PeriodSource::Measured), Duration::from_nanos(13_888_889));
        assert_eq!(p.update_period(PeriodSource::Runtime(Duration::from_micros(8_333))), Duration::from_micros(8_333));
    }
}
