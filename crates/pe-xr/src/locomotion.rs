//! Locomotion (xr.md 5.8; friend-test L4a, @item:primordial-engine/locomotion-l4a-pe-xr-move-turn-and-jump-actions-o):
//! the controllers' sticks and jump button into `pe_seams::MoveInput` samples, and [`MoveRing`], the
//! `pe_seams::MoveFeed` the simulation thread reads (seams/move; pe-app's `SwitchedMove` picks it while VR runs).
//!
//! - **The actions:** `move`, the left thumbstick; `turn`, the right thumbstick's x; `jump`, the right A button. On
//!   the Touch profile (WiVRn's Quest 3) and the Index profile. The simple controller has none, and there is no
//!   tracked-hand locomotion (the user's ruling, controllers only).
//! - **The stick:** a radial dead zone of [`DEAD_ZONE`], rescaled so the edge of the dead zone is 0 and full tilt is
//!   1, its length clamped to 1. Raw values only: the simulation makes the velocity (the trinket seam).
//! - **Turning:** smooth only, at [`TURN_RATE_DEG_S`] at full tilt, past its own dead zone, integrated into the
//!   wrapping `heading` over each display frame's time (the user's ruling: no snap turn, no comfort options).
//! - **Jumps:** a count of press edges, wrapping; the simulation applies each increase once. A button already held
//!   at the first frame, or as focus returns, is not a press.
//! - **Not focused, or the left stick's action not active:** the stick and the turn are zero, so a held sample is a
//!   standstill, never a walk on. `tracked` is the head pose's validity; while it is false, `head` is the last valid.
use std::collections::VecDeque;
use std::sync::Mutex;

use crate::seam::{HostTime, RigPose};
use pe_seams::view::{MoveFeed, MoveInput};

/// The sticks' radial dead zone, as a fraction of full tilt [I: a common default; tune in the headset test].
pub const DEAD_ZONE: f32 = 0.15;
/// The smooth turn's rate at full tilt, degrees per second [I].
pub const TURN_RATE_DEG_S: f64 = 120.0;
/// How long a frame may count for the turn: a stall's frame doesn't spin the player.
const MAX_TURN_DT_S: f64 = 0.1;
/// One turn in `heading`'s units.
const TURN: f64 = 4_294_967_296.0;

/// One frame's locomotion actions, each `None` while its action isn't active.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MoveReading {
    /// The left thumbstick: x right, y forward, each -1..=1.
    pub stick: Option<[f32; 2]>,
    /// The right thumbstick's x: right positive.
    pub turn: Option<f32>,
    /// The right A button, held.
    pub jump: Option<bool>,
    /// The head (the VIEW space) in the rig frame, when its pose is valid.
    pub head: Option<RigPose>,
}

/// A stick after the radial dead zone: 0 inside it, rescaled to 0..1 past it, never longer than 1.
pub fn dead_zone(s: [f32; 2]) -> [f32; 2] {
    let len = (s[0] * s[0] + s[1] * s[1]).sqrt();
    if !len.is_finite() || len <= DEAD_ZONE {
        return [0.0, 0.0];
    }
    let out = ((len - DEAD_ZONE) / (1.0 - DEAD_ZONE)).min(1.0);
    [s[0] / len * out, s[1] / len * out]
}

/// One axis after the dead zone: 0 inside it, rescaled past it, clamped to -1..=1.
pub fn dead_zone_axis(x: f32) -> f32 {
    if !x.is_finite() || x.abs() <= DEAD_ZONE {
        return 0.0;
    }
    (x.signum() * (x.abs() - DEAD_ZONE) / (1.0 - DEAD_ZONE)).clamp(-1.0, 1.0)
}

/// The yaw of a rig-frame orientation `(x, y, z, w)`: radians about +Y, 0 facing -Z, positive turning left (the
/// seam's `stick_yaw`). Looking straight up or down, the yaw of its up vector's opposite stands in, so the stick
/// still means "where the face points".
pub fn yaw_of(q: [f32; 4]) -> f32 {
    let [x, y, z, w] = q.map(f64::from);
    // The forward (-Z) and the up (+Y) axes rotated by q.
    let f = [-(2.0 * (x * z + w * y)), -(2.0 * (y * z - w * x)), -(1.0 - 2.0 * (x * x + y * y))];
    let u = [2.0 * (x * y - w * z), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z + w * x)];
    let (dx, dz) = if f[0].hypot(f[2]) > 1e-3 { (f[0], f[2]) } else { (-u[0] * f[1].signum(), -u[2] * f[1].signum()) };
    (-dx).atan2(-dz) as f32
}

/// The locomotion state across frames: the heading, the jump count and the last valid head.
#[derive(Clone, Debug, Default)]
pub struct MoveState {
    /// The heading, in turns' units as a float, so fractions carry between frames; `heading` is its wrap.
    heading: f64,
    jumps: u32,
    jump_held: bool,
    last_at: Option<HostTime>,
    head: Option<RigPose>,
}

impl MoveState {
    /// This frame's sample at `at` (the display time), from `reading`. `focused`: the session has input focus.
    pub fn step(&mut self, at: HostTime, reading: &MoveReading, focused: bool) -> MoveInput {
        // The first frame, or the first since focus came back: a button already held is adopted, never a press (Quest's
        // overlay selects with A, so A can still be down as focus returns; @chunks-reviewer's L1).
        let first = self.last_at.is_none();
        let dt = self.last_at.map_or(0.0, |l| ((at.0 - l.0) as f64 / 1e9).clamp(0.0, MAX_TURN_DT_S));
        self.last_at = Some(at);
        let r = if focused { *reading } else { MoveReading { head: reading.head, ..Default::default() } };
        if let Some(t) = r.turn {
            // Right on the stick turns right, which is negative in the seam's sense.
            let rad = -f64::from(dead_zone_axis(t)) * TURN_RATE_DEG_S.to_radians() * dt;
            self.heading = (self.heading + rad / std::f64::consts::TAU * TURN).rem_euclid(TURN);
        }
        let held = r.jump.unwrap_or(false);
        if held && !self.jump_held && !first {
            self.jumps = self.jumps.wrapping_add(1);
        }
        self.jump_held = held;
        if !focused {
            // The next focused frame starts afresh: no turn time, and what is held then is adopted.
            self.last_at = None;
        }
        if r.head.is_some() {
            self.head = r.head;
        }
        let head = self.head.unwrap_or(RigPose { position: [0.0; 3], orientation: [0.0, 0.0, 0.0, 1.0] });
        MoveInput {
            sampled_at: at,
            stick: r.stick.map_or([0.0, 0.0], dead_zone),
            stick_yaw: yaw_of(head.orientation),
            heading: self.heading as u32,
            jumps: self.jumps,
            head,
            tracked: r.head.is_some(),
        }
    }

    /// A recentre's yaw (spec 3.2), radians positive turning left: the heading takes it, in the same frame's sample as
    /// the corrected head and hands (@coordinator-opus, condition 1).
    pub fn turn_by(&mut self, radians: f64) {
        self.heading = (self.heading + radians / std::f64::consts::TAU * TURN).rem_euclid(TURN);
    }

    /// Focus or the session went away: the next press counts again, and the turn's time restarts. The heading and the
    /// jump count carry on, so the simulation never sees them go back.
    pub fn focus_lost(&mut self) {
        self.jump_held = false;
        self.last_at = None;
    }
}

/// The simulation thread's view of the headset's locomotion: the newest samples, written by the frame loop.
#[derive(Default)]
pub struct MoveRing {
    ring: Mutex<VecDeque<MoveInput>>,
}

impl MoveRing {
    /// The samples kept: about 0.7 s at 90 Hz, past any simulation frame's look back.
    pub const KEEP: usize = 64;

    pub fn push(&self, m: MoveInput) {
        let mut r = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        r.push_back(m);
        while r.len() > Self::KEEP {
            r.pop_front();
        }
    }

    /// The session ended: no samples, so `move_at` answers `None` until the next one.
    pub fn clear(&self) {
        self.ring.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

impl MoveFeed for MoveRing {
    /// The newest sample at or before `at`; the oldest if every sample is later; `None` with no samples.
    fn move_at(&self, at: HostTime) -> Option<MoveInput> {
        let r = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        r.iter().rev().find(|m| m.sampled_at <= at).or(r.front()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn head(yaw_deg: f32) -> RigPose {
        let h = (yaw_deg.to_radians() / 2.0, 0.0f32);
        RigPose { position: [0.0, 1.6, 0.0], orientation: [0.0, h.0.sin(), 0.0, h.0.cos()] }
    }

    #[test]
    fn the_dead_zone_rescales_and_clamps() {
        assert_eq!(dead_zone([0.1, 0.1]), [0.0, 0.0], "inside the dead zone");
        let full = dead_zone([0.0, 1.0]);
        assert!((full[1] - 1.0).abs() < 1e-6 && full[0] == 0.0);
        let diag = dead_zone([1.0, 1.0]);
        assert!(((diag[0].powi(2) + diag[1].powi(2)).sqrt() - 1.0).abs() < 1e-6, "a square gate's corner is 1 long");
        let half = dead_zone([0.0, 0.575]);
        assert!((half[1] - 0.5).abs() < 1e-3, "{half:?}");
        assert_eq!(dead_zone([f32::NAN, 0.5]), [0.0, 0.0]);
        assert_eq!(dead_zone_axis(-0.1), 0.0);
        assert!((dead_zone_axis(-1.0) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_yaw_is_zero_facing_minus_z_and_positive_turning_left() {
        assert!(yaw_of(head(0.0).orientation).abs() < 1e-6);
        assert!((yaw_of(head(90.0).orientation) - std::f32::consts::FRAC_PI_2).abs() < 1e-5, "left");
        assert!((yaw_of(head(-45.0).orientation) + std::f32::consts::FRAC_PI_4).abs() < 1e-5, "right");
        // Looking straight down while facing left: still left.
        let (a, b) = ((90.0f32.to_radians() / 2.0), (-89.99f32.to_radians() / 2.0));
        let yaw = [0.0, a.sin(), 0.0, a.cos()];
        let pitch = [b.sin(), 0.0, 0.0, b.cos()];
        let q = crate::math::qmul(yaw, pitch);
        assert!((yaw_of(q) - std::f32::consts::FRAC_PI_2).abs() < 1e-2, "{}", yaw_of(q));
    }

    #[test]
    fn a_full_right_turn_for_a_second_turns_the_heading_right_by_the_rate() {
        let mut s = MoveState::default();
        let r = MoveReading { stick: Some([0.0, 0.0]), turn: Some(1.0), jump: Some(false), head: Some(head(0.0)) };
        let mut last = s.step(HostTime(0), &r, true);
        assert_eq!(last.heading, 0, "the first frame has no time");
        for i in 1..=90 {
            last = s.step(HostTime(i * 1_000_000_000 / 90), &r, true);
        }
        let deg = f64::from(last.heading) / TURN * 360.0;
        assert!((deg - (360.0 - TURN_RATE_DEG_S)).abs() < 0.01, "right is negative, wrapped: {deg}");
        // Inside the turn's dead zone: no turn.
        let still = MoveReading { turn: Some(0.1), ..r };
        assert_eq!(s.step(HostTime(1_100 * MS), &still, true).heading, last.heading);
    }

    #[test]
    fn a_stall_does_not_spin_the_player() {
        let mut s = MoveState::default();
        let r = MoveReading { turn: Some(1.0), ..Default::default() };
        s.step(HostTime(0), &r, true);
        let after = s.step(HostTime(5_000 * MS), &r, true);
        let deg = 360.0 - f64::from(after.heading) / TURN * 360.0;
        assert!((deg - TURN_RATE_DEG_S * MAX_TURN_DT_S).abs() < 0.01, "{deg}");
    }

    #[test]
    fn jumps_count_press_edges_and_unfocused_is_a_standstill() {
        let mut s = MoveState::default();
        let at = |i: i64| HostTime(i * 11 * MS);
        let walk =
            |jump| MoveReading { stick: Some([0.0, 1.0]), turn: Some(0.0), jump: Some(jump), head: Some(head(30.0)) };
        assert_eq!(s.step(at(0), &walk(true), true).jumps, 0, "held at the first frame: adopted, not a press");
        assert_eq!(s.step(at(1), &walk(false), true).jumps, 0);
        assert_eq!(s.step(at(2), &walk(true), true).jumps, 1);
        assert_eq!(s.step(at(3), &walk(true), true).jumps, 1, "held: one jump");
        assert_eq!(s.step(at(4), &walk(false), true).jumps, 1);
        let m = s.step(at(5), &walk(true), true);
        assert_eq!((m.jumps, m.stick), (2, [0.0, 1.0]));
        assert!((m.stick_yaw - 30.0f32.to_radians()).abs() < 1e-5 && m.tracked);
        // Focus lost with the stick pushed and A held: zeros, no new jump, the head kept.
        let lost = s.step(at(6), &walk(true), false);
        assert_eq!((lost.stick, lost.jumps), ([0.0, 0.0], 2));
        assert!(lost.tracked, "the head is still valid; only input is gone");
        // Head tracking lost: the last head, tracked false.
        let blind = s.step(at(7), &MoveReading { head: None, ..walk(false) }, true);
        assert_eq!((blind.head, blind.tracked), (head(30.0), false));
        // No stick action (a simple controller): no walk.
        assert_eq!(s.step(at(8), &MoveReading { stick: None, ..walk(false) }, true).stick, [0.0, 0.0]);
        // Focus lost, then back with A still down: adopted, no jump (@chunks-reviewer's L1).
        s.focus_lost();
        assert_eq!(s.step(at(9), &walk(true), true).jumps, 2);
    }

    #[test]
    fn the_ring_answers_the_newest_sample_at_or_before_a_time() {
        let ring = MoveRing::default();
        assert_eq!(ring.move_at(HostTime(0)), None);
        let mut s = MoveState::default();
        let r = MoveReading { stick: Some([0.0, 1.0]), ..Default::default() };
        for i in 0..100 {
            ring.push(s.step(HostTime(i * 11 * MS), &r, true));
        }
        assert_eq!(ring.move_at(HostTime(50 * 11 * MS + 5)).map(|m| m.sampled_at), Some(HostTime(50 * 11 * MS)));
        let oldest = HostTime((100 - MoveRing::KEEP as i64) * 11 * MS);
        assert_eq!(ring.move_at(HostTime(0)).map(|m| m.sampled_at), Some(oldest), "older than kept: the oldest");
        ring.clear();
        assert_eq!(ring.move_at(HostTime(i64::MAX)), None);
    }
}
