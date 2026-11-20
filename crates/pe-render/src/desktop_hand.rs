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
//! - **Hit tests** use each drawn object's own shape (a sphere, a box, a capsule; a mesh by its bounding sphere), on
//!   the CPU, against the last drawn poses: the render thread never asks the simulation. A ray hits a solid only
//!   where it enters it from outside: a solid the camera is inside (the pit's floor box, a wall, a ball the eye
//!   flew into) is never a hit at distance 0, so it can't pin the hand to the camera (the M1 session of 2026-11-20,
//!   where the floor's 2.87 m bounding sphere held the hand 0.2 m in front of the eye).
//! - `discontinuity` is bumped when the hover depth jumps by more than 20 cm, so a jump is never read as a throw.

use std::collections::VecDeque;
use std::sync::Mutex;

use glam::{DQuat, DVec3};
use pe_seams::{
    Hand, HandFeed, HandInput, HandSource, HostTime, Pos, RigPose, RigTransform, Shape, ViewFlags, ViewObject,
};

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

/// A solid's shape, world scale applied, in the target's own frame (a capsule along its y axis).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Solid {
    Sphere { radius: f64 },
    Box { half_extents: DVec3 },
    Capsule { radius: f64, half_height: f64 },
}

/// A solid the ray may hit: a drawn object's shape at its drawn pose.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    pub pose: WorldPose,
    pub solid: Solid,
}

impl Target {
    /// A sphere (also a mesh's bounding stand-in).
    pub fn sphere(center: Pos, radius: f64) -> Target {
        Target { pose: WorldPose { pos: center, rot: DQuat::IDENTITY }, solid: Solid::Sphere { radius } }
    }

    /// The target for a drawn object: its shape scaled by the object's scale. A sphere or a capsule under a
    /// non-uniform scale takes the larger axis for its radius; a mesh is its bounding sphere (`bound`, world scale
    /// applied).
    pub fn of_shape(pose: WorldPose, shape: &Shape, scale: [f32; 3], bound: f64) -> Target {
        let [sx, sy, sz] = scale.map(|k| (k as f64).abs());
        let solid = match *shape {
            Shape::Sphere { radius } => Solid::Sphere { radius: radius as f64 * sx.max(sy).max(sz) },
            Shape::Box { half_extents: [x, y, z] } => {
                Solid::Box { half_extents: DVec3::new(x as f64 * sx, y as f64 * sy, z as f64 * sz) }
            }
            Shape::Capsule { radius, half_height } => {
                Solid::Capsule { radius: radius as f64 * sx.max(sz), half_height: half_height as f64 * sy }
            }
            Shape::Mesh(_) => Solid::Sphere { radius: bound },
        };
        Target { pose, solid }
    }

    /// The target for a view object drawn at `pose`: `None` for one with nothing to draw, the local player's own
    /// hands and what they hold (the hand hits what is drawn, never itself).
    pub fn of_object(o: &ViewObject, pose: WorldPose) -> Option<Target> {
        let own = o.flags.contains(ViewFlags::OWN_HAND) || o.flags.contains(ViewFlags::HELD_LOCAL);
        let shape = o.shape.as_ref().filter(|_| !own)?;
        Some(Target::of_shape(pose, shape, o.scale, o.radius as f64))
    }

    /// Where the ray (`d` of unit length) enters the solid, as a distance from `origin`; `None` when it misses, or
    /// when `origin` is inside or on it.
    pub fn entry(&self, origin: Pos, d: DVec3) -> Option<f64> {
        let inv = self.pose.rot.inverse();
        let o = inv * offset_vec(origin - self.pose.pos);
        let d = inv * d;
        match self.solid {
            Solid::Sphere { radius } => sphere_entry(o, d, radius),
            Solid::Box { half_extents } => box_entry(o, d, half_extents),
            Solid::Capsule { radius, half_height } => capsule_entry(o, d, radius, half_height),
        }
    }
}

/// The entry of a ray from `o` along unit `d` into a sphere at the origin; `None` from inside or on it.
fn sphere_entry(o: DVec3, d: DVec3, r: f64) -> Option<f64> {
    let b = o.dot(d);
    let c = o.length_squared() - r * r;
    if c <= 0.0 {
        return None;
    }
    let disc = b * b - c;
    if disc < 0.0 {
        return None;
    }
    let t = -b - disc.sqrt();
    (t > 0.0).then_some(t)
}

/// The slab test against a box centred at the origin; `None` from inside or on it.
fn box_entry(o: DVec3, d: DVec3, h: DVec3) -> Option<f64> {
    if o.x.abs() <= h.x && o.y.abs() <= h.y && o.z.abs() <= h.z {
        return None;
    }
    let (mut near, mut far) = (f64::NEG_INFINITY, f64::INFINITY);
    for (oi, di, hi) in [(o.x, d.x, h.x), (o.y, d.y, h.y), (o.z, d.z, h.z)] {
        if di == 0.0 {
            if oi.abs() > hi {
                return None;
            }
            continue;
        }
        let (a, b) = ((-hi - oi) / di, (hi - oi) / di);
        near = near.max(a.min(b));
        far = far.min(a.max(b));
    }
    (near <= far && near > 0.0).then_some(near)
}

/// A capsule along y, its segment from −`hh` to +`hh`: the nearer of its side's and its caps' entries; `None` from
/// inside or on it.
fn capsule_entry(o: DVec3, d: DVec3, r: f64, hh: f64) -> Option<f64> {
    let on_axis = DVec3::new(0.0, o.y.clamp(-hh, hh), 0.0);
    if (o - on_axis).length_squared() <= r * r {
        return None;
    }
    let mut best: Option<f64> = None;
    // The side: the infinite cylinder x² + z² = r², kept where |y| <= hh.
    let a = d.x * d.x + d.z * d.z;
    if a > 0.0 {
        let b = o.x * d.x + o.z * d.z;
        let c = o.x * o.x + o.z * o.z - r * r;
        let disc = b * b - a * c;
        if disc >= 0.0 {
            let t = (-b - disc.sqrt()) / a;
            if t > 0.0 && (o.y + t * d.y).abs() <= hh {
                best = Some(t);
            }
        }
    }
    for cy in [-hh, hh] {
        if let Some(t) = sphere_entry(o - DVec3::new(0.0, cy, 0.0), d, r) {
            best = Some(best.map_or(t, |b: f64| b.min(t)));
        }
    }
    best
}

/// The nearest entry along the ray, as a distance from its origin: never 0, since a solid the origin is inside is
/// not hit.
pub fn first_hit(origin: Pos, dir: DVec3, targets: &[Target]) -> Option<f64> {
    let d = dir.normalize_or_zero();
    if d == DVec3::ZERO {
        return None;
    }
    targets.iter().filter_map(|t| t.entry(origin, d)).fold(None, |best, t| Some(best.map_or(t, |b: f64| b.min(t))))
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
        let ball = [Target::sphere(at(0.0, 0.0, -2.0), 0.5)];
        let (p, i) = h.update(&input(0, false, false), &ball, &RIG);
        assert!((h.depth() - 1.47).abs() < 1e-9);
        assert!((offset_vec(p.pos - Pos::ORIGIN).z + 1.47).abs() < 1e-9);
        assert_eq!(i.discontinuity, 0, "1.5 m to 1.47 m is not a jump");
        let far = [Target::sphere(at(0.0, 0.0, -6.0), 0.5)];
        let (_, i) = h.update(&input(16, false, false), &far, &RIG);
        assert_eq!(i.discontinuity, 1);
        assert!(!i.closed && i.source == HandSource::Desktop);
    }

    #[test]
    fn a_grab_locks_the_depth_and_the_wheel_moves_it() {
        let mut h = DesktopHand::new(DesktopHandSettings::default());
        let ball = [Target::sphere(at(0.0, 0.0, -2.0), 0.5)];
        h.update(&input(0, false, false), &ball, &RIG);
        let (_, i) = h.update(&input(16, true, false), &[], &RIG);
        assert!(i.closed && i.grip == 1.0);
        let d = h.depth();
        h.update(&input(32, true, false), &[Target::sphere(at(0.0, 0.0, -0.8), 0.1)], &RIG);
        assert_eq!(h.depth(), d, "locked while closed");
        h.update(&PointerInput { wheel: 2.0, ..input(48, true, false) }, &[], &RIG);
        assert!((h.depth() - (d + 0.2)).abs() < 1e-9);
    }

    #[test]
    fn a_push_drives_past_the_surface_at_about_a_metre_a_second() {
        let mut h = DesktopHand::new(DesktopHandSettings::default());
        let ball = [Target::sphere(at(0.0, 0.0, -2.0), 0.5)];
        h.update(&input(0, false, false), &ball, &RIG);
        let mut t = 0;
        for _ in 0..30 {
            t += 10;
            h.update(&input(t, false, true), &ball, &RIG);
        }
        // 0.3 s at 1 m/s from 1.47 m, capped at 1.6 m (10 cm past the surface at 1.5 m).
        assert!((h.depth() - 1.6).abs() < 1e-6, "{}", h.depth());
    }

    fn floor_box() -> Target {
        // The pit's floor (pe-world preset): half extents [2.0, 0.5, 2.0], its centre 0.5 m below the top.
        Target {
            pose: WorldPose { pos: at(0.0, -0.5, 0.0), rot: DQuat::IDENTITY },
            solid: Solid::Box { half_extents: DVec3::new(2.0, 0.5, 2.0) },
        }
    }

    #[test]
    fn a_box_is_hit_as_a_box_and_never_from_inside() {
        let eye = at(0.0, 1.6, 1.5);
        let floor = floor_box();
        // Its bounding sphere (radius 2.87 m) holds the eye: the old test gave a hit at 0 here.
        assert!(offset_vec(eye - floor.pose.pos).length() < DVec3::new(2.0, 0.5, 2.0).length());
        let down = DVec3::new(0.0, -1.0, -0.2).normalize();
        let hit = first_hit(eye, down, &[floor]).expect("the floor's top");
        assert!((hit - 1.6 / down.y.abs()).abs() < 1e-9, "{hit}");
        assert_eq!(first_hit(eye, DVec3::Y, &[floor]), None, "looking up misses it");
        assert_eq!(first_hit(at(0.0, -0.2, 0.0), DVec3::NEG_Y, &[floor]), None, "from inside: no hit");
        // Rotated a quarter turn about y, a long thin box is hit on its long side.
        let bar = Target {
            pose: WorldPose { pos: at(0.0, 0.0, -3.0), rot: DQuat::from_rotation_y(std::f64::consts::FRAC_PI_2) },
            solid: Solid::Box { half_extents: DVec3::new(0.1, 0.1, 1.0) },
        };
        let h = first_hit(Pos::ORIGIN, DVec3::new(0.25, 0.0, -1.0), &[bar]).expect("the bar's long side");
        assert!((h - 2.9 * DVec3::new(0.25, 0.0, -1.0).length()).abs() < 1e-9, "{h}");
    }

    #[test]
    fn spheres_and_capsules_are_hit_where_the_ray_enters() {
        let ball = Target::sphere(at(0.0, 0.0, -2.0), 0.5);
        assert!((first_hit(Pos::ORIGIN, DVec3::NEG_Z, &[ball]).unwrap() - 1.5).abs() < 1e-9);
        assert_eq!(first_hit(at(0.0, 0.0, -2.1), DVec3::NEG_Z, &[ball]), None, "from inside: no hit");
        assert_eq!(first_hit(Pos::ORIGIN, DVec3::Z, &[ball]), None, "behind");
        let cap = Target {
            pose: WorldPose { pos: at(0.0, 0.0, -2.0), rot: DQuat::IDENTITY },
            solid: Solid::Capsule { radius: 0.2, half_height: 0.5 },
        };
        let side = first_hit(Pos::ORIGIN, DVec3::NEG_Z, &[cap]).unwrap();
        assert!((side - 1.8).abs() < 1e-9, "the side: {side}");
        let top = first_hit(at(0.0, 3.0, -2.0), DVec3::NEG_Y, &[cap]).unwrap();
        assert!((top - 2.3).abs() < 1e-9, "the top cap: {top}");
        assert_eq!(first_hit(at(0.0, 0.6, -2.0), DVec3::NEG_Y, &[cap]), None, "inside the top cap");
        // The nearest of several, wherever they are in the list.
        let near = Target::sphere(at(0.0, 0.0, -1.0), 0.1);
        assert!((first_hit(Pos::ORIGIN, DVec3::NEG_Z, &[cap, ball, near]).unwrap() - 0.9).abs() < 1e-9);
    }

    #[test]
    fn a_shape_takes_the_objects_scale() {
        let pose = WorldPose { pos: at(0.0, 0.0, -3.0), rot: DQuat::IDENTITY };
        let b = Target::of_shape(pose, &Shape::Box { half_extents: [0.5, 0.5, 0.5] }, [2.0, 1.0, 1.0], 9.9);
        assert_eq!(b.solid, Solid::Box { half_extents: DVec3::new(1.0, 0.5, 0.5) });
        let s = Target::of_shape(pose, &Shape::Sphere { radius: 0.1 }, [1.0, 3.0, 1.0], 9.9);
        assert_eq!(s.solid, Solid::Sphere { radius: 0.1f32 as f64 * 3.0 });
        let m = Target::of_shape(pose, &Shape::Mesh(pe_seams::BlobRef(pe_seams::Hash([0; 32]))), [1.0; 3], 0.7);
        assert_eq!(m.solid, Solid::Sphere { radius: 0.7 });
    }

    #[test]
    fn hovering_over_the_floor_puts_the_hand_on_it_not_at_the_eye() {
        // The M1 session's bug: from a spawn slot the hand sat at min_depth (0.2 m) on every frame.
        let mut h = DesktopHand::new(DesktopHandSettings::default());
        let eye = at(0.0, 1.6, 1.5);
        let dir = DVec3::new(0.0, -1.0, -1.0).normalize();
        h.update(&PointerInput { origin: eye, dir, ..input(0, false, false) }, &[floor_box()], &RIG);
        let want = 1.6 * 2f64.sqrt() - 0.03;
        assert!((h.depth() - want).abs() < 1e-9, "{} against {want}", h.depth());
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
