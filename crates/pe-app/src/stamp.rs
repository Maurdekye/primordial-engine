//! Stamp-ahead (agreement.md 3; @item:primordial-engine/pe-app-stamps-a-slow-member-s-entries-ahead-by-i): when this
//! member's entries reach the others later than their cut lag can absorb, it stamps them ahead of its present by a
//! lead, so they arrive above the cuts' frames and are never void as late. Its own influence then lags by the lead;
//! its displayed hands follow tracking, not the stamped pose, so what it sees of itself does not.
//!
//! The delay comes from the other members (S3 v1.8's `own_lateness`, @world-chunks): each reports the p95 lateness
//! of this member's entries at their first arrival, signed (negative when they arrived before their stamps), with
//! the seq of the entry at the percentile. A report measures the delay less the lead that entry was stamped with, so
//! the lead used at that seq is added back. That gives the delay itself, with no loop to tune (@design-reviewer and its M1 on 0238ca46, @world-chunks). Signed, a lead that has grown too large shows
//! as early arrivals and sheds (@engine-architect's Medium on 0238ca46). The lead follows the largest corrected report
//! over the members, clamped at `max`, a world setting of a few seconds, so a faulty member can't hold this member's
//! input back without bound (@coordinator-opus, after @fable-reviewer).
//!
//! Two controllers share one signal. The receivers' adaptive cut lag absorbs the delay first, up to `target` (at most
//! the lag's cap), and the lead takes only the excess: lead = delay - target + margin. On recovery the lead drains
//! first. The default `target` is 0.1 s (@coordinator-opus): a receiver can't afford to re-step a
//! second of a slow member's input every tick, so the slow member pays its excess as its own input lag (app.md 3.2,
//! with the measurement).
//!
//! **Lead changes** (@coordinator-opus, after @fable-reviewer, on @design-reviewer's Medium on
//! a6b1035a): the lead follows the largest lead the reports called for over the last `window` frames (about 3 s), as
//! a jitter buffer follows the worst recent delay, so one spike doesn't cause cycles of rising and draining. It rises
//! at once. A stamp never steps below this chain's last ([`StampLead::stamp`]): an entry stamped below its
//! predecessor would be refused (`SubmitError::BelowOwnStamp`) or void as back-stamped. Past the clamp, hand entries
//! void as late and edits go to pe-api's re-send backstop and its held batch: the stated limit.
//!
//! **Two shed rates** (@coordinator-opus, after @fable-reviewer): while both hands are slow, the lead
//! sheds one frame every `second / surplus` frames, clamped to `min_every..=max_every` (2 to 30), so a surplus of 3 s
//! drains in about 8 s, starting at half speed. While either hand moves ([`Motion`], decided every frame), it sheds at
//! most one frame in `moving_every` (11): the others then see the hand at most 1.1 times its tracking speed, so a
//! strike lands at most about 10% harder, and a 3 s lead still drains in about 33 s. There is no hold and no backstop.
//!
//! **The pose clock** ([`StampLead::pose_time`]): a shed leaves one frame's stamp standing still, and that frame's
//! poses are thinned (app.md 3.2). Sampled at the present, the next stamp's pose would then be two tracking frames on
//! from the last: one stamp in eleven at twice the speed, the whole frame in one step. So each stamp advances the
//! poses' time by at most `every / (every - 1)` frames, never past the present, which spreads the frame a shed takes out
//! over the stamps up to the next shed.

use std::collections::VecDeque;

use pe_seams::{Frame, Pos};

/// The lead's settings, in frames at the world's rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeadConfig {
    /// The delay the receivers' cut lag absorbs before the lead takes the rest: at most pe-agree's lag cap
    /// (`cut_lag_cap_s`).
    pub target: u64,
    /// The margin entries keep above a cut's frame (pe-agree's lag margin).
    pub margin: u64,
    /// The most lead there can be (a world setting, a few seconds).
    pub max: u64,
    /// How far back the largest lead the reports called for counts: the window the lead follows.
    pub window: u64,
    /// One second, in frames: a frame is shed every `second / surplus` frames.
    pub second: u64,
    /// The fastest shedding, in frames between one-frame sheds: never a frame every frame.
    pub min_every: u64,
    /// The slowest shedding, for a surplus of a frame or two.
    pub max_every: u64,
    /// The fastest shedding while a hand moves: one frame in this many (11, at most 1.1 times the tracking speed).
    pub moving_every: u64,
}

impl LeadConfig {
    /// The target's default (@coordinator-opus): receivers then see a slow member's entries about
    /// `TARGET - margin` late, an ordinary link's cost.
    pub const TARGET: std::time::Duration = std::time::Duration::from_millis(100);
    /// The clamp's default: the most input lag one faulty member's inflated report can cause, and well inside
    /// pe-agree's per-author pending cap (app.md 3.2).
    pub const MAX_LEAD: std::time::Duration = std::time::Duration::from_secs(3);
    /// The window's default: about 3 s.
    pub const WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

    /// M1's settings at `rate`: the default target, the cut lag's margin from agreement's settings, the default clamp
    /// and window, shedding between one frame in 2 and one in 30, and at most one in 11 while a hand moves.
    pub fn m1(rate: pe_seams::SimRate) -> LeadConfig {
        let margin = std::time::Duration::from_secs_f64(crate::genesis::settings().cut_lag_margin_s);
        LeadConfig {
            target: rate.frames(LeadConfig::TARGET),
            margin: rate.frames(margin),
            max: rate.frames(LeadConfig::MAX_LEAD),
            window: rate.frames(LeadConfig::WINDOW),
            second: u64::from(rate.hz()),
            min_every: 2,
            max_every: 30,
            moving_every: 11,
        }
    }
}

/// The most runs of lead [`StampLead`] remembers. A report cites recent seqs, and the lead changes at most once a
/// tick, so this covers minutes.
const MAX_RUNS: usize = 256;

/// The lead this member stamps its own entries ahead by, and the lead it used for each of its seqs.
#[derive(Clone, Debug)]
pub struct StampLead {
    cfg: LeadConfig,
    lead: u64,
    /// Runs of lead, oldest first: from the run's seq on, its lead, until the next run.
    used: VecDeque<(u64, u64)>,
    /// The lead of the newest run [`MAX_RUNS`] let go: the lead of every seq before the oldest kept run.
    floor: u64,
    /// The present when the lead last changed.
    changed: Option<Frame>,
    /// The largest lead the reports called for at each present within the window, oldest first: one item per frame,
    /// however many ticks a frame has, so at most `window` + 1 (@design-reviewer's Low on f06bb3a4).
    recent: VecDeque<(Frame, u64)>,
    /// Where the drain under way began, if one is.
    drain_from: Option<Frame>,
    /// The largest lead so far, and the longest drain, in frames.
    largest: u64,
    longest_drain: u64,
    /// The pose clock ([`StampLead::pose_time`]): the last frame's stamp, and the present, in fractional frames, whose
    /// tracking its poses carry.
    pose: Option<(Frame, f64)>,
}

impl StampLead {
    pub fn new(cfg: LeadConfig) -> StampLead {
        StampLead {
            cfg,
            lead: 0,
            used: VecDeque::new(),
            floor: 0,
            changed: None,
            recent: VecDeque::new(),
            drain_from: None,
            largest: 0,
            longest_drain: 0,
            pose: None,
        }
    }

    /// A controller holding `lead` frames (at most `max`) from the chain's first entry on, until reports move it:
    /// the slow-link measurement's fixed lead, before S3 reports the lateness the lead follows.
    pub fn fixed(cfg: LeadConfig, lead: u64) -> StampLead {
        let lead = lead.min(cfg.max);
        StampLead { lead, used: VecDeque::from([(1, lead)]), largest: lead, ..StampLead::new(cfg) }
    }

    /// The lead in force, in frames.
    pub fn lead(&self) -> u64 {
        self.lead
    }

    /// The largest lead so far, in frames.
    pub fn largest(&self) -> u64 {
        self.largest
    }

    /// The longest drain so far, from the first frame the lead was above what the reports called for to the frame
    /// it got there, in frames; a drain under way counts up to `p` ([`StampLead::update`]'s latest present).
    pub fn longest_drain(&self) -> u64 {
        self.longest_drain
    }

    /// The lead this member stamped `seq` with. Before the oldest run it remembers: the lead of the newest run it let
    /// go, or 0 before the first run (@design-reviewer's Low on a6b1035a).
    pub fn lead_at(&self, seq: u64) -> u64 {
        self.used.iter().rev().find(|&&(from, _)| from <= seq).map_or(self.floor, |&(_, lead)| lead)
    }

    /// The delay this member's entries see at the others, in frames: the largest of their reports (the signed p95
    /// lateness at arrival, and the seq of the entry at the percentile), each corrected by the lead that entry was
    /// stamped with, and never below 0. `None` without a report.
    pub fn delay(&self, reports: &[(i64, u64)]) -> Option<u64> {
        let lead_at = |seq| i64::try_from(self.lead_at(seq)).unwrap_or(i64::MAX);
        reports.iter().map(|&(late, seq)| u64::try_from(late.saturating_add(lead_at(seq))).unwrap_or(0)).max()
    }

    /// The lead `delay` asks for: none while the delay and the margin fit in the target, then the excess, up to `max`.
    pub fn wanted(&self, delay: u64) -> u64 {
        let c = &self.cfg;
        delay.saturating_add(c.margin).saturating_sub(c.target).min(c.max)
    }

    /// Frames between one-frame sheds for a surplus of `surplus` frames: a second's frames over the surplus, between
    /// `min_every` and `max_every`; while a hand is `moving`, at least `moving_every`.
    pub fn shed_every(&self, surplus: u64, moving: bool) -> u64 {
        let c = &self.cfg;
        let every = (c.second / surplus.max(1)).clamp(c.min_every.max(1), c.max_every.max(c.min_every.max(1)));
        if moving { every.max(c.moving_every) } else { every }
    }

    /// Updates the lead at the present `p` from the reports. `next_seq` is the seq the chain's next entry gets, from
    /// which a changed lead applies; `moving` is the speed test's answer for this frame ([`Motion::moving`]). The goal
    /// is the largest lead the reports called for over the last `window` frames; with none in the window the lead
    /// stays. A rise to the goal takes effect at once; a fall sheds one frame every [`StampLead::shed_every`] frames,
    /// decided every frame. Returns the lead in force.
    pub fn update(&mut self, p: Frame, reports: &[(i64, u64)], next_seq: u64, moving: bool) -> u64 {
        if let Some(delay) = self.delay(reports) {
            let want = self.wanted(delay);
            match self.recent.back_mut() {
                Some(last) if last.0 == p => last.1 = last.1.max(want),
                _ => self.recent.push_back((p, want)),
            }
        }
        while self.recent.front().is_some_and(|&(f, _)| f.0.saturating_add(self.cfg.window) < p.0) {
            self.recent.pop_front();
        }
        let Some(goal) = self.recent.iter().map(|&(_, want)| want).max() else { return self.lead };
        if goal < self.lead && self.drain_from.is_none() {
            self.drain_from = Some(p);
        }
        let lead = if goal > self.lead {
            goal
        } else if goal < self.lead
            && self.changed.is_none_or(|c| p.0 >= c.0 + self.shed_every(self.lead - goal, moving))
        {
            self.lead - 1
        } else {
            self.note_drain(p, goal);
            return self.lead;
        };
        self.lead = lead;
        self.largest = self.largest.max(lead);
        self.changed = Some(p);
        self.note_drain(p, goal);
        match self.used.back_mut() {
            Some(run) if run.0 == next_seq => run.1 = lead,
            _ => self.used.push_back((next_seq, lead)),
        }
        while self.used.len() > MAX_RUNS {
            if let Some((_, lead)) = self.used.pop_front() {
                self.floor = lead;
            }
        }
        lead
    }

    /// The drain's length so far, and its end once the lead is at or above the goal.
    fn note_drain(&mut self, p: Frame, goal: u64) {
        if let Some(from) = self.drain_from {
            self.longest_drain = self.longest_drain.max(p.0.saturating_sub(from.0));
            if self.lead <= goal {
                self.drain_from = None;
            }
        }
    }

    /// The stamp for an entry made at the present `p`: `p` plus the lead, never below the chain's `last` stamp.
    pub fn stamp(&self, p: Frame, last: Option<Frame>) -> Frame {
        let ahead = Frame(p.0.saturating_add(self.lead));
        last.map_or(ahead, |l| ahead.max(l))
    }

    /// The present, in fractional frames, whose hand poses the frame at the present `p`, stamped `stamp`, carries:
    /// the pose clock. Each stamp advances it by at most `every / (every - 1)` frames, `every` being the fastest shed
    /// the hands allow (one in `moving_every` while a hand is `moving`, one in `min_every` otherwise), and never past
    /// `p`. A frame whose stamp stood still keeps the last frame's time, so a grasp it sends goes at the last pose.
    /// While nothing sheds this is `p`. Once a frame, after [`StampLead::update`] and [`StampLead::stamp`].
    pub fn pose_time(&mut self, p: Frame, stamp: Frame, moving: bool) -> f64 {
        let c = &self.cfg;
        let every = if moving { c.moving_every.max(c.min_every) } else { c.min_every }.max(2);
        let step = every as f64 / (every - 1) as f64;
        let now = p.0 as f64;
        let t = match self.pose {
            Some((s, t)) if stamp >= s => (t + (stamp.0 - s.0) as f64 * step).min(now),
            _ => now,
        };
        self.pose = Some((stamp, t));
        t
    }
}

/// The speed test's settings (@coordinator-opus).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionConfig {
    /// A moving hand becomes slow below this speed, in m/s.
    pub slow_below: f64,
    /// A slow hand becomes moving above this speed, in m/s.
    pub moving_above: f64,
    /// The hand collider's radius, in metres: its fastest point is this far from the pose's origin.
    pub radius: f64,
    /// The world's rate, in frames per second.
    pub hz: f64,
}

impl MotionConfig {
    /// M1's: slow below 0.25 m/s, moving above 0.35 m/s, pe-replica's hand sphere ([`pe_replica::world::HAND_RADIUS`],
    /// about the size of the M1 hand) at `hz`.
    pub fn m1(hz: u32) -> MotionConfig {
        MotionConfig {
            slow_below: 0.25,
            moving_above: 0.35,
            radius: f64::from(pe_replica::world::HAND_RADIUS),
            hz: f64::from(hz),
        }
    }
}

/// The speed test (@coordinator-opus, after @fable-reviewer): whether either of this member's hands
/// moves, decided every frame from the poses it would send.
///
/// - A hand's speed is its fastest point's: the pose's linear speed plus its angular speed times the collider's
///   radius, so a wrist flick counts.
/// - With hysteresis: a moving hand becomes slow below `slow_below`, a slow one moving above `moving_above`.
/// - A tracking jump counts as moving: the first pose after the hand was away (never seen, absent or untracked), and
///   a pose whose `discontinuity` changed.
/// - A hand the feed doesn't track sends no pose, so a shed can't speed it up: it counts as slow, as does a hand never
///   seen (a desktop feed with one hand). pe-replica coasts it in world frames, which the stamps don't touch.
#[derive(Clone, Debug)]
pub struct Motion {
    cfg: MotionConfig,
    hands: [Track; 2],
}

/// One hand's part of the speed test.
#[derive(Clone, Copy, Debug, Default)]
struct Track {
    /// The last tracked pose: its frame, world position and rotation, and the feed's discontinuity count.
    last: Option<(Frame, Pos, [f32; 4], u32)>,
    /// The hand was away (absent or untracked) since `last`.
    away: bool,
    moving: bool,
}

impl Motion {
    pub fn new(cfg: MotionConfig) -> Motion {
        Motion { cfg, hands: [Track::default(); 2] }
    }

    /// Hand `side` (0 left, 1 right) at the present `p`: its world position and rotation and the feed's discontinuity
    /// count, or `None` when the feed doesn't track it. Returns the hand's speed in m/s, infinite for a jump, `None`
    /// while away.
    pub fn observe(&mut self, p: Frame, side: usize, pose: Option<(Pos, [f32; 4], u32)>) -> Option<f64> {
        let cfg = self.cfg;
        let t = &mut self.hands[side];
        let Some((pos, rot, jumps)) = pose else {
            (t.away, t.moving) = (true, false);
            return None;
        };
        let speed = match t.last {
            Some((f, at, r, j)) if !t.away && j == jumps && p > f => {
                let moved = norm((pos - at).0) + angle(r, rot) * cfg.radius;
                moved * cfg.hz / (p.0 - f.0) as f64
            }
            _ => f64::INFINITY,
        };
        // A speed that isn't a number counts as moving.
        let speed = if speed.is_nan() { f64::INFINITY } else { speed };
        t.moving = if t.moving { speed >= cfg.slow_below } else { speed > cfg.moving_above };
        (t.last, t.away) = (Some((p, pos, rot, jumps)), false);
        Some(speed)
    }

    /// Whether either hand moves.
    pub fn moving(&self) -> bool {
        self.hands.iter().any(|t| t.moving)
    }
}

fn norm(v: [f64; 3]) -> f64 {
    v.iter().map(|c| c * c).sum::<f64>().sqrt()
}

/// The angle, in radians, of the rotation from unit quaternion `a` to `b`, (x, y, z, w): that of `conj(a) b`, read
/// with atan2 so a small turn keeps its precision and an unchanged rotation reads exactly 0.
fn angle(a: [f32; 4], b: [f32; 4]) -> f64 {
    let [ax, ay, az, aw] = a.map(f64::from);
    let [bx, by, bz, bw] = b.map(f64::from);
    let x = aw * bx - ax * bw - ay * bz + az * by;
    let y = aw * by - ay * bw - az * bx + ax * bz;
    let z = aw * bz - az * bw - ax * by + ay * bx;
    let w = aw * bw + ax * bx + ay * by + az * bz;
    2.0 * (x * x + y * y + z * z).sqrt().atan2(w.abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 60 Hz: the target at the 1 s cap, a 50 ms margin, 5 s at most, a 3 s window, shedding between 2 and 30, and at most
    /// one in 11 while a hand moves.
    fn cfg() -> LeadConfig {
        LeadConfig {
            target: 60,
            margin: 3,
            max: 300,
            window: 180,
            second: 60,
            min_every: 2,
            max_every: 30,
            moving_every: 11,
        }
    }

    #[test]
    fn the_lead_is_the_excess_over_the_target() {
        let s = StampLead::new(cfg());
        assert_eq!(s.wanted(72), 15, "1.2 s: 0.2 s over the cap, plus the margin");
        assert_eq!(s.wanted(57), 0, "within the target less the margin");
        assert_eq!(s.wanted(10_000), 300, "clamped");
        // The ruling's (b): the target at the lag's floor, 0.05 s, so the lead takes nearly the whole delay.
        let floor = StampLead::new(LeadConfig { target: 3, ..cfg() });
        assert_eq!(floor.wanted(72), 72);
    }

    /// The others measure the delay less the lead the entries were stamped with. Adding that lead back gives the
    /// delay, so the lead settles at once instead of swinging between 0 and the excess.
    #[test]
    fn a_report_corrected_by_the_lead_used_gives_a_steady_lead() {
        let mut s = StampLead::new(cfg());
        let delay = 72i64;
        let mut seq = 1u64;
        let mut leads = Vec::new();
        for p in 0..200u64 {
            // What the other member reports: the lateness of the entries it has, the newest a few seqs back.
            let covered = seq.saturating_sub(4).max(1);
            let report = delay - s.lead_at(covered) as i64;
            leads.push(s.update(Frame(p), &[(report, covered)], seq, false));
            seq += 2;
        }
        assert!(leads.iter().all(|&l| l == 15), "{leads:?}");
    }

    /// The shed rate follows the surplus: a second's frames over it, between one frame in 2 and one in 30. While a hand
    /// moves, at most one frame in 11.
    #[test]
    fn the_shed_rate_follows_the_surplus() {
        let s = StampLead::new(cfg());
        assert_eq!(s.shed_every(180, false), 2, "a 3 s surplus starts at half speed");
        assert_eq!(s.shed_every(30, false), 2);
        assert_eq!(s.shed_every(20, false), 3);
        assert_eq!(s.shed_every(6, false), 10);
        assert_eq!(s.shed_every(2, false), 30);
        assert_eq!(s.shed_every(1, false), 30, "never slower than one in 30");
        assert_eq!(s.shed_every(180, true), 11, "a hand moving: one in 11 at most");
        assert_eq!(s.shed_every(6, true), 11);
        assert_eq!(s.shed_every(2, true), 30, "slower still for a small surplus");
    }

    /// A 3 s surplus drains in about 8 s once the window lets it go, a stamp never steps down, and the
    /// stats keep the largest lead and the drain's length.
    #[test]
    fn a_3_s_surplus_drains_in_about_8_s_and_stamps_never_go_back() {
        let mut s = StampLead::new(LeadConfig { target: 6, ..cfg() });
        assert_eq!(s.update(Frame(0), &[(183, 1)], 1, false), 180, "a rise takes effect at once");
        // The link is fine again: the corrected delay is 3 frames, so the lead wants nothing.
        let (mut last, mut p, mut seq) = (None::<Frame>, 0u64, 2u64);
        while s.lead() > 0 {
            p += 1;
            let late = 3 - s.lead_at(seq - 1) as i64;
            s.update(Frame(p), &[(late, seq - 1)], seq, false);
            let stamp = s.stamp(Frame(p), last);
            assert!(last.is_none_or(|l| stamp >= l), "frame {p}: {stamp:?} below {last:?}");
            last = Some(stamp);
            seq += 1;
            assert!(p < 60 * 20, "still draining at 20 s: lead {}", s.lead());
        }
        // The window first holds the rise for 3 s; the drain itself then takes about 8 s.
        let secs = s.longest_drain() as f64 / 60.0;
        assert!((7.0..=10.0).contains(&secs), "drained in {secs:.1} s");
        assert!(p.abs_diff(180 + s.longest_drain()) <= 2, "the window, then the drain: {p} frames in all");
        assert_eq!(s.largest(), 180);
    }

    /// The lead follows the largest lead the reports called for over the window: after one spike it holds for about
    /// the window, then drains, rather than draining at once and rising again on the next spike.
    #[test]
    fn a_spike_holds_the_lead_for_the_window() {
        let mut s = StampLead::new(LeadConfig { target: 6, ..cfg() });
        assert_eq!(s.update(Frame(0), &[(66, 1)], 1, false), 63, "one spike: 63 frames of lead");
        for (p, seq) in (1..=170u64).zip(2u64..) {
            let late = 3 - s.lead_at(seq - 1) as i64;
            assert_eq!(s.update(Frame(p), &[(late, seq - 1)], seq, false), 63, "held within the window, frame {p}");
        }
        for (p, seq) in (171..=400u64).zip(172u64..) {
            let late = 3 - s.lead_at(seq - 1) as i64;
            s.update(Frame(p), &[(late, seq - 1)], seq, false);
        }
        assert!(s.lead() < 63, "drains once the spike leaves the window: {}", s.lead());
    }

    /// M1's settings at 60 Hz: the 0.1 s target, the 50 ms margin, the 3 s clamp and window, shedding 2 to 30, and 11
    /// while a hand moves.
    #[test]
    fn m1_settings_at_60_hz() {
        let c = LeadConfig::m1(pe_seams::SimRate::DEFAULT);
        assert_eq!(
            c,
            LeadConfig {
                target: 6,
                margin: 3,
                max: 180,
                window: 180,
                second: 60,
                min_every: 2,
                max_every: 30,
                moving_every: 11
            }
        );
        // A 1.2 s delay asks for 69 frames: the receivers then see the entries 3 frames late.
        assert_eq!(StampLead::new(c).wanted(72), 69);
    }

    /// A lead larger than the delay shows as early arrivals, a negative report, and sheds: with reports clamped at 0
    /// the corrected delay would read as the lead itself, and at a target equal to the margin it would never shed
    /// (@engine-architect's Medium on 0238ca46).
    #[test]
    fn a_lead_larger_than_the_delay_shows_as_early_arrivals_and_sheds() {
        let mut s = StampLead::new(LeadConfig { target: 3, window: 1, ..cfg() });
        assert_eq!(s.update(Frame(0), &[(72, 1)], 1, false), 72);
        // The link drops to 20 frames: entries now arrive 52 frames before their stamps.
        for (p, seq) in (1..=600u64).zip(2u64..) {
            let late = 20 - s.lead_at(seq - 1) as i64;
            s.update(Frame(p), &[(late, seq - 1)], seq, false);
        }
        assert_eq!(s.lead(), 20, "down to the delay");
        assert_eq!(s.delay(&[(-52, 1)]), Some(20), "a report of -52 on a lead of 72 is a delay of 20");
    }

    /// The largest corrected report counts, and the clamp bounds what one faulty member can do with it.
    #[test]
    fn the_largest_report_counts_and_the_lead_is_clamped() {
        let s = StampLead::new(cfg());
        assert_eq!(s.delay(&[(10_000, 1), (72, 1), (70, 1)]), Some(10_000));
        assert_eq!(s.wanted(10_000), 300);
        assert_eq!(s.delay(&[]), None);
    }

    /// The sim thread ticks many times a frame (inbox and queue wakes), each with reports: the window keeps one item
    /// per frame, the largest lead its ticks called for, so it holds at most `window` + 1 items.
    #[test]
    fn the_window_keeps_one_item_per_frame() {
        let mut s = StampLead::new(cfg());
        let mut next_seq = 2;
        for p in 0..400u64 {
            for late in [72, 90, 80] {
                // Seq 1 went out before any lead, so each report is the delay itself.
                s.update(Frame(p), &[(late, 1)], next_seq, false);
                next_seq += 1;
            }
        }
        assert_eq!(s.recent.len(), 181, "one item per frame of the window");
        assert!(s.recent.iter().all(|&(_, want)| want == 33), "each frame's largest: {:?}", s.recent.front());
        assert_eq!(s.lead(), 33, "the largest of the frame's ticks, 1.5 s less the target plus the margin");
    }

    #[test]
    fn without_reports_the_lead_stays() {
        let mut s = StampLead::new(cfg());
        s.update(Frame(0), &[(72, 1)], 1, false);
        assert_eq!(s.update(Frame(100), &[], 50, false), 15);
        assert_eq!(s.update(Frame(1_000), &[], 60, false), 15, "nor once the window holds none");
    }

    /// Once the oldest runs are let go, a seq before the oldest kept run reads the lead of the newest run let go, not
    /// 0 (@design-reviewer's Low on a6b1035a); before the first run ever, it reads 0.
    #[test]
    fn a_seq_before_the_oldest_kept_run_reads_the_run_let_go() {
        let mut s = StampLead::new(LeadConfig { window: 0, min_every: 1, ..cfg() });
        assert_eq!(s.lead_at(5), 0, "before any run");
        // Rises of one frame each, a run each: more runs than are kept.
        for i in 0..(MAX_RUNS as u64 + 10) {
            s.update(Frame(i), &[(63 + i as i64 - s.lead_at(1) as i64, 1)], 10 + i, false);
        }
        let oldest = s.used.front().map(|r| r.0).expect("runs");
        assert!(oldest > 10, "the first runs were let go");
        assert_eq!(s.lead_at(oldest - 1), s.floor);
        assert!(s.floor > 0, "the lead of the newest run let go, not 0");
    }

    /// A player whose hands never pause: a 3 s lead sheds one frame in 11 at most and is gone in about 33 s
    /// (@coordinator-opus), against under 10 s with the hands slow.
    #[test]
    fn a_player_who_never_pauses_drains_a_3_s_lead_in_about_33_s() {
        let drain = |moving: bool| {
            let mut s = StampLead::fixed(LeadConfig { target: 6, ..cfg() }, 180);
            let (mut p, mut seq, mut sheds) = (0u64, 2u64, Vec::new());
            while s.lead() > 0 {
                p += 1;
                let before = s.lead();
                let late = 3 - s.lead_at(seq - 1) as i64;
                s.update(Frame(p), &[(late, seq - 1)], seq, moving);
                if s.lead() < before {
                    sheds.push(p);
                }
                seq += 1;
                assert!(p < 60 * 60, "still draining at 60 s: lead {}", s.lead());
            }
            (p, sheds)
        };
        let (frames, sheds) = drain(true);
        assert!(sheds.windows(2).all(|w| w[1] - w[0] >= 11), "one frame in 11 at most");
        let secs = frames as f64 / 60.0;
        assert!((32.0..=34.0).contains(&secs), "a hand always moving: drained in {secs:.1} s");
        let (slow, _) = drain(false);
        assert!(slow < 600, "hands slow: drained in {:.1} s", slow as f64 / 60.0);
    }

    /// The pose clock spreads the frame each shed takes out over the stamps up to the next shed: while a hand moves,
    /// a stamp carries at most 1.1 tracking frames, never two; with the hands slow, at most 2. The poses never come
    /// from past the present, never go back, and trail it by at most a frame.
    #[test]
    fn the_pose_clock_spreads_each_shed_over_the_stamps_to_the_next() {
        for (moving, most) in [(true, 1.1), (false, 2.0)] {
            let mut s = StampLead::fixed(LeadConfig { target: 6, ..cfg() }, 120);
            let (mut seq, mut prev) = (2u64, None::<(Frame, f64)>);
            let mut steps = Vec::new();
            for p in 1..=1_500u64 {
                let late = 3 - s.lead_at(seq - 1) as i64;
                s.update(Frame(p), &[(late, seq - 1)], seq, moving);
                let stamp = s.stamp(Frame(p), prev.map(|(s, _)| s));
                let t = s.pose_time(Frame(p), stamp, moving);
                assert!(t <= p as f64 && t >= p as f64 - 1.0 - 1e-9, "frame {p}: pose time {t}");
                if let Some((ps, pt)) = prev {
                    assert!(t >= pt, "frame {p}: the pose time went back");
                    if stamp > ps {
                        steps.push((t - pt) / (stamp.0 - ps.0) as f64);
                    }
                }
                prev = Some((stamp, t));
                seq += 1;
            }
            assert_eq!(s.lead(), 0, "drained");
            let max = steps.iter().copied().fold(0.0f64, f64::max);
            assert!(max <= most + 1e-9, "moving {moving}: a stamp carried {max:.3} tracking frames");
            assert!(steps.iter().any(|&x| x > 1.0 + 1e-9), "moving {moving}: the sheds are spread");
        }
    }

    /// Hand 0's pose at frame `f`: its origin moving at `v` m/s along x, turning at `w` rad/s about y.
    fn pose_at(f: u64, v: f64, w: f64) -> (Pos, [f32; 4], u32) {
        let t = f as f64 / 60.0;
        let pos = Pos::ORIGIN.checked_add(pe_seams::Offset([v * t, 1.0, 0.0])).expect("in range");
        let h = w * t / 2.0;
        (pos, [0.0, h.sin() as f32, 0.0, h.cos() as f32], 0)
    }

    /// Feeds hand 0 frames `frames` at `v` m/s and `w` rad/s, each pose continuing from the last, and returns whether
    /// the hands moved after the last.
    fn run(m: &mut Motion, frames: std::ops::RangeInclusive<u64>, v: f64, w: f64, x0: &mut (f64, f64)) -> bool {
        for f in frames {
            x0.0 += v / 60.0;
            x0.1 += w / 60.0;
            let pos = Pos::ORIGIN.checked_add(pe_seams::Offset([x0.0, 1.0, 0.0])).expect("in range");
            let h = x0.1 / 2.0;
            m.observe(Frame(f), 0, Some((pos, [0.0, h.sin() as f32, 0.0, h.cos() as f32], 0)));
        }
        m.moving()
    }

    /// The speed test's hysteresis (@coordinator-opus): a slow hand stays slow up to 0.35 m/s, a moving
    /// one stays moving down to 0.25 m/s.
    #[test]
    fn the_speed_test_has_hysteresis() {
        let mut m = Motion::new(MotionConfig::m1(60));
        assert!(!m.moving(), "a hand never seen counts as slow");
        let x = &mut (0.0, 0.0);
        assert!(run(&mut m, 1..=1, 0.0, 0.0, x), "the first pose is a jump");
        assert!(!run(&mut m, 2..=2, 0.0, 0.0, x), "still");
        assert!(!run(&mut m, 3..=30, 0.3, 0.0, x), "0.3 m/s from slow: still slow");
        assert!(run(&mut m, 31..=31, 0.4, 0.0, x), "0.4 m/s: moving");
        assert!(run(&mut m, 32..=60, 0.3, 0.0, x), "0.3 m/s from moving: still moving");
        assert!(!run(&mut m, 61..=61, 0.2, 0.0, x), "0.2 m/s: slow");
        assert!(run(&mut m, 62..=62, 0.63, 0.0, x), "the sweep's 0.63 m/s: moving");
    }

    /// A wrist flick counts (@coordinator-opus): with the origin nearly still, at 0.05 m/s, a turn of
    /// 5 rad/s moves the hand's farthest point at 0.05 + 5 x 0.09 = 0.5 m/s, so the hand moves; at 2 rad/s, 0.23 m/s,
    /// it is slow.
    #[test]
    fn a_wrist_flick_with_a_nearly_still_origin_counts_as_moving() {
        let speed = |v: f64, w: f64| {
            let mut m = Motion::new(MotionConfig::m1(60));
            let (a, b) = (pose_at(10, v, w), pose_at(11, v, w));
            m.observe(Frame(10), 0, Some(a));
            (m.observe(Frame(11), 0, Some(b)).expect("tracked"), m.moving())
        };
        let (fast, moving) = speed(0.05, 5.0);
        assert!((fast - 0.5).abs() < 1e-3 && moving, "{fast:.3} m/s");
        let (origin, moving) = speed(0.05, 0.0);
        assert!((origin - 0.05).abs() < 1e-6 && !moving, "{origin:.3} m/s");
        let (slow, moving) = speed(0.05, 2.0);
        assert!((slow - 0.23).abs() < 1e-3 && !moving, "{slow:.3} m/s");
    }

    /// A tracking jump counts as moving: a bumped discontinuity, and the first pose after the hand was away. A hand the
    /// feed doesn't track counts as slow, so with the other hand never seen nothing moves.
    #[test]
    fn a_tracking_jump_counts_as_moving_and_an_untracked_hand_as_slow() {
        let mut m = Motion::new(MotionConfig::m1(60));
        let (pos, rot, _) = pose_at(0, 0.0, 0.0);
        m.observe(Frame(1), 0, Some((pos, rot, 0)));
        m.observe(Frame(2), 0, Some((pos, rot, 0)));
        assert!(!m.moving(), "still");
        assert_eq!(m.observe(Frame(3), 0, Some((pos, rot, 1))), Some(f64::INFINITY), "a bumped discontinuity");
        assert!(m.moving());
        m.observe(Frame(4), 0, Some((pos, rot, 1)));
        assert!(!m.moving(), "still again");
        let x = &mut (0.0, 0.0);
        assert!(run(&mut m, 5..=10, 1.0, 0.0, x), "a moving hand");
        assert_eq!(m.observe(Frame(11), 0, None), None);
        assert!(!m.moving(), "untracked: slow");
        assert_eq!(m.observe(Frame(12), 0, Some((pos, rot, 1))), Some(f64::INFINITY), "back: a jump");
        assert!(m.moving());
    }
}
