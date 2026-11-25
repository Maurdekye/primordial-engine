//! The desktop's locomotion (friend-test locomotion L4b): W A S D for the movement stick, the mouse's yaw into the
//! cumulative heading and its pitch into the head, Space for a jump, written once a display frame into a
//! [`MoveFeed`] the simulation thread reads as it reads pe-xr's `MoveRing` in VR.
//!
//! The mapping (@engine-architect, answer 3): the stick is relative to the heading itself (`stick_yaw`
//! 0), since the desktop's look turns the whole body; the head stays over the body's feet (its rig-frame x and z are
//! 0, so pe-world's L2 never walks the body by a head move) at [`DesktopMoveSettings::eye_height`], pitched by the
//! mouse. Turning is smooth only (no snap turn; user).

use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::sync::Mutex;

use glam::DQuat;
use pe_seams::{HostTime, MoveFeed, MoveInput, RigPose};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DesktopMoveSettings {
    /// The eye's height over the body's feet, m [I, tuning data].
    pub eye_height: f32,
    /// Radians of turn per pixel of mouse movement, for yaw and pitch (the window's M1 look used 0.005).
    pub look_per_px: f64,
    /// The pitch's limit either way, radians.
    pub max_pitch: f64,
}

impl Default for DesktopMoveSettings {
    fn default() -> Self {
        DesktopMoveSettings { eye_height: 1.6, look_per_px: 0.005, max_pitch: 1.5 }
    }
}

/// One display frame's desktop input.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MoveKeys {
    pub forward: bool,
    pub left: bool,
    pub back: bool,
    pub right: bool,
    pub jump: bool,
    /// The look's mouse movement since the last frame, pixels (x right, y down).
    pub look: [f64; 2],
}

/// The desktop's locomotion state: the heading and pitch the mouse has turned, and the jump presses.
#[derive(Clone, Debug)]
pub struct DesktopMove {
    settings: DesktopMoveSettings,
    /// The cumulative heading in turns (positive turning left), kept in 0..1 and carried as 2^32 a turn.
    turns: f64,
    pitch: f64,
    jumps: u32,
    jump_held: bool,
}

impl DesktopMove {
    pub fn new(settings: DesktopMoveSettings) -> DesktopMove {
        DesktopMove { settings, turns: 0.0, pitch: 0.0, jumps: 0, jump_held: false }
    }

    /// The sample for a display frame at `at`. A jump counts once a press, however long Space is held.
    pub fn sample(&mut self, at: HostTime, keys: &MoveKeys) -> MoveInput {
        let s = self.settings;
        // Mouse right turns right: the heading's positive way is left.
        self.turns = (self.turns - keys.look[0] * s.look_per_px / TAU).rem_euclid(1.0);
        self.pitch = (self.pitch - keys.look[1] * s.look_per_px).clamp(-s.max_pitch, s.max_pitch);
        if keys.jump && !self.jump_held {
            self.jumps = self.jumps.wrapping_add(1);
        }
        self.jump_held = keys.jump;
        let axis = |pos: bool, neg: bool| f32::from(u8::from(pos)) - f32::from(u8::from(neg));
        let mut stick = [axis(keys.right, keys.left), axis(keys.forward, keys.back)];
        let len = stick[0].hypot(stick[1]);
        if len > 1.0 {
            stick = stick.map(|c| c / len);
        }
        let q = DQuat::from_rotation_x(self.pitch).as_quat();
        MoveInput {
            sampled_at: at,
            stick,
            stick_yaw: 0.0,
            heading: self.heading(),
            jumps: self.jumps,
            head: RigPose { position: [0.0, s.eye_height, 0.0], orientation: [q.x, q.y, q.z, q.w] },
            tracked: true,
        }
    }

    /// A standstill at `at`: no stick, no new jump, the same heading and head. The window pushes it when it stops
    /// drawing (minimized, occluded, unfocused), so a key held then never walks the body on (@xr-eng's M1 on b331ffcd).
    pub fn standstill(&mut self, at: HostTime) -> MoveInput {
        self.sample(at, &MoveKeys::default())
    }

    /// The heading as `MoveInput` carries it: 2^32 a turn, wrapping.
    pub fn heading(&self) -> u32 {
        // `turns` is in 0..1, so the product is in 0..=2^32, and 2^32 wraps to 0.
        ((self.turns * 4_294_967_296.0) as u64 & 0xFFFF_FFFF) as u32
    }

    /// The heading in radians (positive turning left).
    pub fn yaw(&self) -> f64 {
        self.turns * TAU
    }

    pub fn pitch(&self) -> f64 {
        self.pitch
    }
}

/// The desktop's locomotion samples, read by the simulation thread (pe-app's `SwitchedMove` picks it while no XR
/// session runs).
#[derive(Default)]
pub struct DesktopMoveFeed {
    ring: Mutex<VecDeque<MoveInput>>,
}

impl DesktopMoveFeed {
    const KEEP: usize = 64;

    pub fn push(&self, m: MoveInput) {
        let mut r = self.ring.lock().expect("move ring");
        r.push_back(m);
        while r.len() > Self::KEEP {
            r.pop_front();
        }
    }
}

impl MoveFeed for DesktopMoveFeed {
    /// The newest sample at or before `at`; the oldest if all are later; `None` before the window's first frame.
    fn move_at(&self, at: HostTime) -> Option<MoveInput> {
        let r = self.ring.lock().expect("move ring");
        r.iter().rev().find(|m| m.sampled_at <= at).or(r.front()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(w: bool, a: bool, s: bool, d: bool) -> MoveKeys {
        MoveKeys { forward: w, left: a, back: s, right: d, ..Default::default() }
    }

    fn look(dx: f64, dy: f64) -> MoveKeys {
        MoveKeys { look: [dx, dy], ..Default::default() }
    }

    #[test]
    fn wasd_is_the_stick_relative_to_the_heading_and_diagonals_are_not_faster() {
        let mut m = DesktopMove::new(DesktopMoveSettings::default());
        let t = HostTime::from_millis(0);
        assert_eq!(m.sample(t, &keys(false, false, false, false)).stick, [0.0, 0.0]);
        assert_eq!(m.sample(t, &keys(true, false, false, false)).stick, [0.0, 1.0]);
        assert_eq!(m.sample(t, &keys(false, true, false, false)).stick, [-1.0, 0.0]);
        assert_eq!(m.sample(t, &keys(true, true, true, false)).stick, [-1.0, 0.0], "W and S cancel");
        let d = m.sample(t, &keys(true, false, false, true));
        assert!((d.stick[0] - d.stick[1]).abs() < 1e-7 && (d.stick[0].hypot(d.stick[1]) - 1.0).abs() < 1e-6);
        assert_eq!(d.stick_yaw, 0.0);
        assert!(d.tracked);
        assert_eq!((d.head.position[0], d.head.position[2]), (0.0, 0.0), "the head stays over the feet");
        assert_eq!(d.head.position[1], 1.6);
    }

    #[test]
    fn the_mouse_turns_the_heading_smoothly_and_wraps_and_pitches_the_head() {
        let mut m = DesktopMove::new(DesktopMoveSettings::default());
        let t = HostTime::from_millis(0);
        // Mouse left by a quarter turn's pixels: the heading turns left by 2^30.
        let quarter = (TAU / 4.0) / 0.005;
        let s = m.sample(t, &look(-quarter, 0.0));
        assert!((i64::from(s.heading) - (1 << 30)).abs() <= 1, "{}", s.heading);
        // Then right by a half turn: three quarters, wrapped.
        let s = m.sample(t, &look(2.0 * quarter, 0.0));
        assert!((i64::from(s.heading) - 3 * (1 << 30)).abs() <= 1, "{}", s.heading);
        assert!((m.yaw() - 0.75 * TAU).abs() < 1e-9);
        // Many small turns sum to one big one.
        let mut n = DesktopMove::new(DesktopMoveSettings::default());
        for _ in 0..1000 {
            n.sample(t, &look(-quarter / 1000.0, 0.0));
        }
        assert!((i64::from(n.heading()) - (1 << 30)).abs() <= 4);
        // Mouse up pitches the head up, about +X, and leaves the heading; the pitch is clamped.
        let s = m.sample(t, &look(0.0, -100.0));
        let [x, y, z, w] = s.head.orientation.map(f64::from);
        assert!((DQuat::from_xyzw(x, y, z, w) * glam::DVec3::NEG_Z).y > 0.4, "looking up");
        assert_eq!(s.heading, m.heading());
        m.sample(t, &look(0.0, -1e6));
        assert_eq!(m.pitch(), 1.5);
    }

    #[test]
    fn a_jump_counts_once_a_press() {
        let mut m = DesktopMove::new(DesktopMoveSettings::default());
        let t = HostTime::from_millis(0);
        let seq = [false, true, true, true, false, true, false, false, true];
        let jumps: Vec<u32> =
            seq.iter().map(|&j| m.sample(t, &MoveKeys { jump: j, ..Default::default() }).jumps).collect();
        assert_eq!(jumps, [0, 1, 1, 1, 1, 2, 2, 2, 3]);
    }

    #[test]
    fn a_standstill_stops_the_walk_and_keeps_the_heading_and_the_jumps() {
        let (feed, mut m) = (DesktopMoveFeed::default(), DesktopMove::new(DesktopMoveSettings::default()));
        let walk = MoveKeys { forward: true, jump: true, look: [-100.0, 0.0], ..Default::default() };
        let w = m.sample(HostTime::from_millis(10), &walk);
        feed.push(w);
        feed.push(m.standstill(HostTime::from_millis(20)));
        let s = feed.move_at(HostTime::from_millis(500)).expect("a sample");
        assert_eq!(s.stick, [0.0, 0.0], "not walking while the window isn't drawing");
        assert_eq!((s.heading, s.jumps, s.head), (w.heading, w.jumps, w.head));
        // Space still held when drawing resumes is no new press.
        assert_eq!(m.sample(HostTime::from_millis(30), &walk).jumps, w.jumps + 1, "released by the standstill");
    }

    #[test]
    fn the_feed_gives_the_newest_sample_at_or_before_the_frame() {
        let feed = DesktopMoveFeed::default();
        assert_eq!(feed.move_at(HostTime::from_millis(5)), None, "before the window's first frame");
        let mut m = DesktopMove::new(DesktopMoveSettings::default());
        for ms in [10, 20, 30] {
            feed.push(m.sample(HostTime::from_millis(ms), &MoveKeys { jump: ms == 20, ..Default::default() }));
        }
        assert_eq!(feed.move_at(HostTime::from_millis(25)).map(|s| s.sampled_at), Some(HostTime::from_millis(20)));
        assert_eq!(feed.move_at(HostTime::from_millis(5)).map(|s| s.sampled_at), Some(HostTime::from_millis(10)));
        assert_eq!(feed.move_at(HostTime::from_millis(99)).map(|s| s.jumps), Some(1));
    }
}
