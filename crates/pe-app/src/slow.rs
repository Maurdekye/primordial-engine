//! A member that can't keep real time (divergence.md 4.6, F3:
//! @item:primordial-engine/a-member-that-can-t-simulate-in-real-time-freeze). Its lag is the session clock's present
//! frame less the frame its replica has stepped. Past [`SLOW_S`] it is slow, a local hint and never a vote. Its slow
//! timer runs while it is slow and resets once the lag has stayed under [`CALM_S`] for [`CALM_FOR_S`]. Past
//! [`FREEZE_AFTER_S`] of it, at the first moment it is then behind by more than [`SLOW_S`], the watch says so once
//! ([`Verdict::Freeze`], kept for F3's package work's automatic catch-up). pe-app freezes nobody: it tells the member
//! and it keeps playing (the user's). If it is still slow, it says so again after another
//! [`FREEZE_AFTER_S`]. The whole time it is behind by [`SHOW_S`] or more, its status
//! line says how far.

use pe_seams::HostTime;

/// Behind by more than this, the member is slow (@fable-reviewer's threshold).
pub const SLOW_S: f64 = 2.0;
/// The slow timer resets once the lag has stayed under this...
pub const CALM_S: f64 = 1.0;
/// ...for this long, so a member hovering at the threshold doesn't reset it at every frame.
pub const CALM_FOR_S: f64 = 5.0;
/// Slow for this long, the member freezes and resyncs (the user's "1m").
pub const FREEZE_AFTER_S: f64 = 60.0;
/// The indicator shows from this lag...
pub const SHOW_S: f64 = 0.5;
/// ...and comes down once the lag has stayed under it for this long.
pub const HIDE_AFTER_S: f64 = 2.0;

/// What the watch asks of the simulation after an observation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    /// Keep going; show `indicator` if `Some` (seconds behind).
    Run { behind_s: Option<f64> },
    /// Slow for [`FREEZE_AFTER_S`]: freeze and resync. Returned once per slow stint; [`SlowWatch::resync_ended`]
    /// starts the next.
    Freeze { behind_s: f64 },
}

#[derive(Clone, Debug, Default)]
pub struct SlowWatch {
    /// When the current slow stint began (lag past [`SLOW_S`]), until it calms down.
    slow_since: Option<HostTime>,
    /// Since when the lag has been under [`CALM_S`].
    calm_since: Option<HostTime>,
    /// Since when the lag has been under [`SHOW_S`], while the indicator is up.
    under_since: Option<HostTime>,
    shown: bool,
    /// The freeze of this slow stint was asked for already.
    fired: bool,
    /// Resyncs in this run of slow stints, the one under way included: the indicator's attempt number.
    pub attempts: u32,
    /// The largest lag seen, in seconds.
    pub lag_max_s: f64,
    /// Nanoseconds spent slow in all.
    pub slow_ns: u64,
    last: Option<HostTime>,
    /// Tests only: freeze after this many seconds slow instead of [`FREEZE_AFTER_S`].
    pub freeze_after_s: Option<f64>,
}

fn secs(a: HostTime, b: HostTime) -> f64 {
    (b.0.saturating_sub(a.0)) as f64 / 1e9
}

impl SlowWatch {
    /// One observation: at `now` the member is `lag_s` seconds behind the clock.
    pub fn observe(&mut self, now: HostTime, lag_s: f64) -> Verdict {
        self.lag_max_s = self.lag_max_s.max(lag_s);
        if let (Some(_), Some(last)) = (self.slow_since, self.last) {
            self.slow_ns += u64::try_from(now.0.saturating_sub(last.0)).unwrap_or(0);
        }
        self.last = Some(now);
        if lag_s > SLOW_S && self.slow_since.is_none() {
            self.slow_since = Some(now);
        }
        if lag_s < CALM_S {
            let since = *self.calm_since.get_or_insert(now);
            if secs(since, now) >= CALM_FOR_S {
                self.slow_since = None;
                self.fired = false;
                self.attempts = 0;
            }
        } else {
            self.calm_since = None;
        }
        if lag_s >= SHOW_S {
            self.shown = true;
            self.under_since = None;
        } else if self.shown {
            let since = *self.under_since.get_or_insert(now);
            if secs(since, now) >= HIDE_AFTER_S {
                self.shown = false;
                self.under_since = None;
            }
        }
        // Only while still behind: a member that caught up inside its stint (not yet calm for long enough to end it)
        // has nothing to resync, and a notice would say it is 0 s behind (@engine-architect's stall test).
        if let Some(since) = self.slow_since
            && !self.fired
            && lag_s > SLOW_S
            && secs(since, now) >= self.freeze_after_s.unwrap_or(FREEZE_AFTER_S)
        {
            self.fired = true;
            self.attempts += 1;
            return Verdict::Freeze { behind_s: lag_s };
        }
        Verdict::Run { behind_s: self.shown.then_some(lag_s) }
    }

    /// A resync ended, imported or failed: if the member is still slow, its next freeze comes another
    /// [`FREEZE_AFTER_S`] from `now`.
    pub fn resync_ended(&mut self, now: HostTime) {
        self.fired = false;
        self.slow_since = None;
        self.calm_since = None;
        self.last = Some(now);
    }

    /// A join's catch-up reached the present: its lag was the import's, not slowness, so the stint and the indicator
    /// start afresh (integrator-1's bounce of 507169ac: a joiner showed "Behind by 0.0 s" for 2 s after it caught up).
    pub fn caught_up(&mut self, now: HostTime) {
        self.slow_since = None;
        self.calm_since = None;
        self.under_since = None;
        self.shown = false;
        self.fired = false;
        self.attempts = 0;
        self.last = Some(now);
    }

    /// Whether the member is slow now.
    pub fn slow(&self) -> bool {
        self.slow_since.is_some()
    }
}

/// The status line while the member is behind.
pub fn behind_status(behind_s: f64) -> String {
    format!("Behind by {behind_s:.1} s")
}

/// The status line while a resync runs.
pub fn resyncing_status(behind_s: f64, attempt: u32) -> String {
    format!("Resyncing — {behind_s:.0} s behind (attempt {attempt})")
}

/// The status line at `n = 2` once slow for the freeze delay, where this machine doesn't freeze (F3 S5, the user's
/// ruling "2"): every cut needs its vote, so the session confirms only as far as it has stepped (fable's point 1: the
/// text says what is true).
pub fn cant_resync_status(behind_s: f64) -> String {
    format!("Behind by {behind_s:.0} s — the session is confirming {behind_s:.0} s late, at this machine's pace")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: f64) -> HostTime {
        HostTime((s * 1e9) as i64)
    }

    /// Runs `lag(t)` once a frame-ish (every 0.1 s) from `from` to `to`, returning the freezes' times.
    fn run(w: &mut SlowWatch, from: f64, to: f64, lag: impl Fn(f64) -> f64) -> Vec<f64> {
        let mut freezes = Vec::new();
        let mut t = from;
        while t < to {
            if let Verdict::Freeze { .. } = w.observe(at(t), lag(t)) {
                freezes.push(t);
            }
            t += 0.1;
        }
        freezes
    }

    #[test]
    fn a_join_s_catch_up_leaves_no_indicator_or_stint_behind() {
        let mut w = SlowWatch::default();
        let t = |s: f64| HostTime((s * 1e9) as i64);
        assert!(matches!(w.observe(t(0.0), 30.0), Verdict::Run { behind_s: Some(_) }));
        w.caught_up(t(0.1));
        assert_eq!(w.observe(t(0.2), 0.0), Verdict::Run { behind_s: None }, "the indicator is down at once");
        assert!(!w.slow(), "and no slow stint is under way");
    }

    #[test]
    fn a_member_that_caught_up_inside_its_stint_doesn_t_freeze_until_it_is_behind_again() {
        // 30 s behind for 3 s, then caught up: at 60 s the stint hasn't ended (no 5 s calm at 1-2 s), but it isn't
        // behind, so no freeze; once it is behind again it freezes at once.
        let lag = |t: f64| {
            if t < 3.0 {
                30.0
            } else if t < 70.0 {
                1.5
            } else {
                10.0
            }
        };
        let freezes = run(&mut SlowWatch::default(), 0.0, 80.0, lag);
        assert_eq!(freezes.len(), 1, "{freezes:?}");
        assert!(freezes[0] >= 70.0 && freezes[0] < 70.2, "{freezes:?}");
    }

    #[test]
    fn a_member_behind_for_a_minute_freezes_once_and_again_a_minute_after_its_resync() {
        let mut w = SlowWatch::default();
        let f = run(&mut w, 0.0, 70.0, |_| 3.0);
        assert_eq!(f.len(), 1);
        assert!((f[0] - 60.0).abs() < 0.11, "{f:?}");
        assert_eq!(w.attempts, 1);
        w.resync_ended(at(70.0));
        let f = run(&mut w, 70.0, 140.0, |_| 3.0);
        assert_eq!(f.len(), 1, "it keeps attempting while it keeps failing");
        assert!((f[0] - 130.0).abs() < 0.11, "{f:?}");
        assert_eq!(w.attempts, 2);
    }

    #[test]
    fn a_short_calm_doesn_t_reset_the_timer_but_five_seconds_under_a_second_does() {
        let mut w = SlowWatch::default();
        // 3 s calm at 30 s: the stint goes on, so the freeze still comes at 60 s.
        let f = run(&mut w, 0.0, 65.0, |t| if (30.0..33.0).contains(&t) { 0.5 } else { 3.0 });
        assert_eq!(f.len(), 1);
        assert!((f[0] - 60.0).abs() < 0.11, "{f:?}");
        // 6 s calm at 30 s resets it: the next stint starts at 36 s and freezes at 96 s.
        let mut w = SlowWatch::default();
        let f = run(&mut w, 0.0, 100.0, |t| if (30.0..36.0).contains(&t) { 0.5 } else { 3.0 });
        assert_eq!(f.len(), 1);
        assert!((f[0] - 96.0).abs() < 0.21, "{f:?}");
        // Hovering between 1 and 2 s never starts a stint.
        let mut w = SlowWatch::default();
        assert!(run(&mut w, 0.0, 200.0, |_| 1.5).is_empty());
        assert!(!w.slow());
    }

    #[test]
    fn the_indicator_shows_from_half_a_second_and_comes_down_two_seconds_under_it() {
        let mut w = SlowWatch::default();
        assert_eq!(w.observe(at(0.0), 0.2), Verdict::Run { behind_s: None });
        assert_eq!(w.observe(at(0.1), 0.6), Verdict::Run { behind_s: Some(0.6) });
        assert_eq!(w.observe(at(0.2), 0.3), Verdict::Run { behind_s: Some(0.3) }, "still up");
        assert_eq!(w.observe(at(2.1), 0.3), Verdict::Run { behind_s: Some(0.3) }, "1.9 s under");
        assert_eq!(w.observe(at(2.3), 0.3), Verdict::Run { behind_s: None }, "2.1 s under: down");
        assert_eq!(behind_status(3.25), "Behind by 3.2 s");
        assert_eq!(resyncing_status(61.4, 2), "Resyncing — 61 s behind (attempt 2)");
    }

    #[test]
    fn the_watch_counts_the_time_spent_slow_and_the_largest_lag() {
        let mut w = SlowWatch::default();
        run(&mut w, 0.0, 10.0, |t| if t < 5.0 { 0.0 } else { 4.0 });
        assert!((w.lag_max_s - 4.0).abs() < 1e-9);
        let slow_s = w.slow_ns as f64 / 1e9;
        assert!((slow_s - 4.9).abs() < 0.15, "{slow_s}");
    }
}
