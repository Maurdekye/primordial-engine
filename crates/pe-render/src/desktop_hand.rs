//! The desktop hand (render.md 9): a mouse ray that produces the same `HandInput` as pe-xr's tracked hands, for B,
//! who has no headset (plan M1, demo step 4).
//!
//! - **Hovering:** the hand sits where the ray first hits a drawn solid, 3 cm short of its surface, so pointing at
//!   something never disturbs it. With nothing hit, it stays at the last depth (1.5 m to start).
//! - **Grab:** the left button closes the hand; while closed the ray's depth is locked at the grab distance, so the
//!   held object moves in the plane facing the camera. The wheel changes the depth. Releasing opens the hand; its
//!   velocity at release throws.
//! - **Push** (deliberate, review L3): holding Shift drives the hand along the ray to 10 cm past the surface it
//!   hovers over, at about 1 m/s.
//! - **Orientation:** the hand faces along the ray; Q and E roll it.
//! - **Hit tests** use the drawn scene's bounding spheres, on the CPU, against the last drawn poses: the render
//!   thread never asks the simulation. A box is tested by its bounding sphere in M1.
//! - `discontinuity` is bumped when the hover depth jumps by more than 20 cm, so a jump is never read as a throw.

use std::collections::VecDeque;
use std::sync::Mutex;

use glam::{DQuat, DVec3};
use pe_seams::{Hand, HandFeed, HandInput, HandSource, HostTime, Pos, RigPose, RigTransform};

use crate::pose::{WorldPose, offset_vec, quat};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DesktopHandSettings {
    pub hover_gap: f64,
    pub start_depth: f64,
    pub push_past: f64,
    pub push_speed: f64,
    pub jump: f64,
    pub min_depth: f64,
    pub max_depth: f64,
    /// Metres of depth per wheel notch.
    pub wheel_step: f64,
    /// Radians of roll per second while Q or E is held.
    pub roll_rate: f64,
}

impl Default for DesktopHandSettings {
    fn default() -> Self {
        DesktopHandSettings {
            hover_gap: 0.03,
            start_depth: 1.5,
            push_past: 0.10,
            push_speed: 1.0,
            jump: 0.20,
            min_depth: 0.2,
            max_depth: 20.0,
            wheel_step: 0.1,
            roll_rate: 2.0,
        }
    }
}

/// The input of one display frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerInput {
    pub at: HostTime,
    /// The ray from the camera through the cursor, in the world.
    pub origin: Pos,
    pub dir: DVec3,
    pub left: bool,
    pub shift: bool,
    /// Wheel notches since the last frame (positive: away).
    pub wheel: f64,
    /// −1 (Q), 0 or +1 (E).
    pub roll: f64,
}

/// A solid the ray may hit: a drawn object's bounding sphere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    pub center: Pos,
    pub radius: f64,
}

/// The nearest hit along the ray, as a distance from its origin.
pub fn first_hit(origin: Pos, dir: DVec3, targets: &[Target]) -> Option<f64> {
    let d = dir.normalize_or_zero();
    if d == DVec3::ZERO {
        return None;
    }
    let mut best: Option<f64> = None;
    for t in targets {
        let oc = offset_vec(t.center - origin);
        let b = oc.dot(d);
        let c = oc.length_squared() - t.radius * t.radius;
        let disc = b * b - c;
        if disc < 0.0 {
            continue;
        }
        let s = disc.sqrt();
        let hit = if b - s > 0.0 {
            b - s
        } else if b + s > 0.0 {
            0.0
        } else {
            continue;
        };
        if best.is_none_or(|x| hit < x) {
            best = Some(hit);
        }
    }
    best
}

pub struct DesktopHand {
    settings: DesktopHandSettings,
    depth: f64,
    closed: bool,
    roll: f64,
    discontinuity: u32,
    last: Option<(HostTime, WorldPose)>,
    velocity: DVec3,
}

impl DesktopHand {
    pub fn new(settings: DesktopHandSettings) -> DesktopHand {
        DesktopHand {
            settings,
            depth: settings.start_depth,
            closed: false,
            roll: 0.0,
            discontinuity: 0,
            last: None,
            velocity: DVec3::ZERO,
        }
    }

    pub fn depth(&self) -> f64 {
        self.depth
    }

    /// One display frame: the hand's world pose, and its `HandInput` in the rig frame.
    pub fn update(&mut self, input: &PointerInput, targets: &[Target], rig: &RigTransform) -> (WorldPose, HandInput) {
        let s = self.settings;
        let dt = self.last.map_or(0.0, |(t, _)| input.at.secs_since(t)).max(0.0);
        let dir = input.dir.normalize_or(DVec3::NEG_Z);
        if input.left && !self.closed {
            self.closed = true; // grab: the depth locks where it is
        } else if !input.left && self.closed {
            self.closed = false;
        }
        self.depth = (self.depth + input.wheel * s.wheel_step).clamp(s.min_depth, s.max_depth);
        if !self.closed {
            if let Some(hit) = first_hit(input.origin, dir, targets) {
                let want = if input.shift { hit + s.push_past } else { (hit - s.hover_gap).max(s.min_depth) };
                if input.shift {
                    // Driven along the ray at about push_speed, never teleported into the solid.
                    let step = s.push_speed * dt;
                    self.depth += (want - self.depth).clamp(-step, step);
                } else {
                    if (want - self.depth).abs() > s.jump {
                        self.discontinuity += 1;
                    }
                    self.depth = want;
                }
            }
            self.depth = self.depth.clamp(s.min_depth, s.max_depth);
        }
        self.roll += input.roll * s.roll_rate * dt;
        let face = DQuat::from_rotation_arc(DVec3::NEG_Z, dir);
        let rot = (face * DQuat::from_rotation_z(self.roll)).normalize();
        let pose = WorldPose { pos: input.origin, rot }.moved(dir * self.depth);
        if let Some((t, p)) = self.last
            && input.at > t
        {
            let v = offset_vec(pose.pos - p.pos) / input.at.secs_since(t);
            // A light smoothing, so a throw reads the motion of the last few frames.
            self.velocity = self.velocity * 0.5 + v * 0.5;
        }
        self.last = Some((input.at, pose));
        let r_inv = quat(rig.rotation).inverse();
        let k = 1.0 / (rig.scale as f64).max(1e-9);
        let local = (r_inv * offset_vec(pose.pos - rig.pos)) * k;
        let lrot = (r_inv * pose.rot).normalize();
        let lvel = (r_inv * self.velocity) * k;
        let hand = HandInput {
            hand: Hand::Right,
            source: HandSource::Desktop,
            sampled_at: input.at,
            tracked: true,
            pose: RigPose {
                position: [local.x as f32, local.y as f32, local.z as f32],
                orientation: [lrot.x as f32, lrot.y as f32, lrot.z as f32, lrot.w as f32],
            },
            linear_velocity: Some([lvel.x as f32, lvel.y as f32, lvel.z as f32]),
            angular_velocity: None,
            grip: if self.closed { 1.0 } else { 0.0 },
            closed: self.closed,
            discontinuity: self.discontinuity,
        };
        (pose, hand)
    }
}

/// The desktop hand's samples, read by the simulation thread like pe-xr's tracked hands.
#[derive(Default)]
pub struct DesktopHandFeed {
    ring: Mutex<VecDeque<HandInput>>,
}

impl DesktopHandFeed {
    const KEEP: usize = 64;

    pub fn push(&self, h: HandInput) {
        let mut r = self.ring.lock().expect("hand ring");
        r.push_back(h);
        while r.len() > Self::KEEP {
            r.pop_front();
        }
    }
}

impl HandFeed for DesktopHandFeed {
    /// The newest sample at or before `at`; the oldest if all are later.
    fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput> {
        if hand != Hand::Right {
            return None;
        }
        let r = self.ring.lock().expect("hand ring");
        r.iter().rev().find(|h| h.sampled_at <= at).or(r.front()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::Offset;

    fn at(x: f64, y: f64, z: f64) -> Pos {
        Pos::ORIGIN.checked_add(Offset([x, y, z])).unwrap()
    }

    const RIG: RigTransform = RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 };

    fn input(t_ms: i64, left: bool, shift: bool) -> PointerInput {
        PointerInput {
            at: HostTime::from_millis(t_ms),
            origin: Pos::ORIGIN,
            dir: DVec3::NEG_Z,
            left,
            shift,
            wheel: 0.0,
            roll: 0.0,
        }
    }

    #[test]
    fn hovering_stops_short_and_a_jump_is_a_discontinuity() {
        let mut h = DesktopHand::new(DesktopHandSettings::default());
        let ball = [Target { center: at(0.0, 0.0, -2.0), radius: 0.5 }];
        let (p, i) = h.update(&input(0, false, false), &ball, &RIG);
        assert!((h.depth() - 1.47).abs() < 1e-9);
        assert!((offset_vec(p.pos - Pos::ORIGIN).z + 1.47).abs() < 1e-9);
        assert_eq!(i.discontinuity, 0, "1.5 m to 1.47 m is not a jump");
        let far = [Target { center: at(0.0, 0.0, -6.0), radius: 0.5 }];
        let (_, i) = h.update(&input(16, false, false), &far, &RIG);
        assert_eq!(i.discontinuity, 1);
        assert!(!i.closed && i.source == HandSource::Desktop);
    }

    #[test]
    fn a_grab_locks_the_depth_and_the_wheel_moves_it() {
        let mut h = DesktopHand::new(DesktopHandSettings::default());
        let ball = [Target { center: at(0.0, 0.0, -2.0), radius: 0.5 }];
        h.update(&input(0, false, false), &ball, &RIG);
        let (_, i) = h.update(&input(16, true, false), &[], &RIG);
        assert!(i.closed && i.grip == 1.0);
        let d = h.depth();
        h.update(&input(32, true, false), &[Target { center: at(0.0, 0.0, -0.8), radius: 0.1 }], &RIG);
        assert_eq!(h.depth(), d, "locked while closed");
        h.update(&PointerInput { wheel: 2.0, ..input(48, true, false) }, &[], &RIG);
        assert!((h.depth() - (d + 0.2)).abs() < 1e-9);
    }

    #[test]
    fn a_push_drives_past_the_surface_at_about_a_metre_a_second() {
        let mut h = DesktopHand::new(DesktopHandSettings::default());
        let ball = [Target { center: at(0.0, 0.0, -2.0), radius: 0.5 }];
        h.update(&input(0, false, false), &ball, &RIG);
        let mut t = 0;
        for _ in 0..30 {
            t += 10;
            h.update(&input(t, false, true), &ball, &RIG);
        }
        // 0.3 s at 1 m/s from 1.47 m, capped at 1.6 m (10 cm past the surface at 1.5 m).
        assert!((h.depth() - 1.6).abs() < 1e-6, "{}", h.depth());
    }

    #[test]
    fn the_feed_returns_the_sample_for_a_frame() {
        let f = DesktopHandFeed::default();
        let mut h = DesktopHand::new(DesktopHandSettings::default());
        for k in 0..5 {
            let (_, i) = h.update(&input(k * 16, false, false), &[], &RIG);
            f.push(i);
        }
        assert_eq!(f.hand_at(Hand::Right, HostTime::from_millis(40)).unwrap().sampled_at, HostTime::from_millis(32));
        assert!(f.hand_at(Hand::Left, HostTime::from_millis(40)).is_none());
    }
}
