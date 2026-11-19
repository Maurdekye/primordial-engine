//! B: the local simulation runs at the moment the headset shows (app.md 3, render.md 7;
//! @item:primordial-engine/vr-run-the-local-simulation-at-the-moment-the-he; the user 2026-11-19, m02b2aa79f06b,
//! medf756abf194, m4d6a9fecd38d).
//!
//! The XR thread measures each drawn frame's display lead: the runtime's predicted display time minus now
//! ([`DisplayLead::record`]). The simulation thread takes a slow-moving high percentile of it ([`DisplayLead::p95`])
//! and runs its shown present that far ahead of the present, plus one simulation period and the publication's delay
//! ([`ahead_ns`]): stamps, hand samples and the replica's target move there, while agreement stays on the group
//! clock. The period and the delay let the display interpolate inside simulated frames instead of guessing past them:
//! the simulation ticks once a period, and until its next view is out, every display frame's moment must still lie
//! before the newest view's (a view's time is the end of the frame it stepped, app.md 3).
//!
//! Without an XR session nothing is recorded and the shown present is the present.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use pe_seams::HostTime;

/// How long the display lead's window reaches back.
pub const WINDOW: Duration = Duration::from_secs(2);

/// The display lead the XR thread measures, for the simulation thread's shown present. Written about 144 times a
/// second, read once a simulation tick.
#[derive(Debug, Default)]
pub struct DisplayLead {
    samples: Mutex<VecDeque<(HostTime, i64)>>,
}

impl DisplayLead {
    /// One drawn frame's lead at `now`, in nanoseconds. Samples older than [`WINDOW`] go.
    pub fn record(&self, now: HostTime, lead_ns: i64) {
        let Ok(mut s) = self.samples.lock() else { return };
        s.push_back((now, lead_ns.max(0)));
        let oldest = now.0.saturating_sub(window_ns());
        while s.front().is_some_and(|&(t, _)| t.0 < oldest) {
            s.pop_front();
        }
    }

    /// The 95th percentile of the leads within [`WINDOW`] of `now`, in nanoseconds: `None` with none (no XR session
    /// drawing, or none for that long), so the shown present falls back to the present.
    pub fn p95(&self, now: HostTime) -> Option<i64> {
        let s = self.samples.lock().ok()?;
        let oldest = now.0.saturating_sub(window_ns());
        let mut leads: Vec<i64> = s.iter().filter(|&&(t, _)| t.0 >= oldest).map(|&(_, l)| l).collect();
        if leads.is_empty() {
            return None;
        }
        leads.sort_unstable();
        Some(leads[(leads.len() * 95 / 100).min(leads.len() - 1)])
    }
}

fn window_ns() -> i64 {
    i64::try_from(WINDOW.as_nanos()).unwrap_or(i64::MAX)
}

/// The most a tick takes to publish its view after its frame's time, in nanoseconds, for [`ahead_ns`] [I: a tick of
/// M1's 200-ball pit steps in a few milliseconds].
pub const PUBLISH_MARGIN_NS: i64 = 5_000_000;

/// How far ahead of the present the simulation runs, in nanoseconds: the display lead less the `delay` setting, never
/// below zero, plus one simulation period of `period_ns` and [`PUBLISH_MARGIN_NS`], so the display interpolates; 0 with
/// no display lead.
pub fn ahead_ns(lead_ns: Option<i64>, delay: Duration, period_ns: i64) -> i64 {
    let delay = i64::try_from(delay.as_nanos()).unwrap_or(i64::MAX);
    lead_ns.map_or(0, |l| l.saturating_sub(delay).max(0) + period_ns + PUBLISH_MARGIN_NS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    #[test]
    fn the_lead_is_the_window_s_95th_percentile_and_none_without_samples() {
        let lead = DisplayLead::default();
        assert_eq!(lead.p95(HostTime(0)), None);
        // 100 frames at 46 ms and five late ones at 80 ms: the percentile stays at 46.
        for k in 0..100 {
            lead.record(HostTime(k * 7 * MS), 46 * MS);
        }
        for k in 100..105 {
            lead.record(HostTime(k * 7 * MS), 80 * MS);
        }
        assert_eq!(lead.p95(HostTime(105 * 7 * MS)), Some(46 * MS));
        // More than 5% late ones move it.
        for k in 105..120 {
            lead.record(HostTime(k * 7 * MS), 80 * MS);
        }
        assert_eq!(lead.p95(HostTime(120 * 7 * MS)), Some(80 * MS));
        // Two seconds with no frame drawn (the headset off): none, so the present again.
        assert_eq!(lead.p95(HostTime(120 * 7 * MS + 2_001 * MS)), None);
    }

    #[test]
    fn the_simulation_runs_the_lead_less_the_delay_plus_a_period_and_the_publication_ahead() {
        let period = 16_666_667;
        let extra = period + PUBLISH_MARGIN_NS;
        assert_eq!(ahead_ns(None, Duration::ZERO, period), 0, "no headset, no lead");
        assert_eq!(ahead_ns(Some(46 * MS), Duration::ZERO, period), 46 * MS + extra);
        assert_eq!(ahead_ns(Some(46 * MS), Duration::from_millis(20), period), 26 * MS + extra);
        assert_eq!(ahead_ns(Some(46 * MS), Duration::from_millis(60), period), extra, "never below the present");
    }
}
