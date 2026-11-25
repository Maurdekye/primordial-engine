//! The local rig at each drawn frame (render.md 7; the friend test's locomotion, piece L4c).
//!
//! The simulation publishes the local player's rig at each view's frame (S5 v1.12's `FrameView::local_rig`): in a
//! world with player bodies it follows the player's body. A display draws at 72-120 Hz over 60 Hz views, so the rig
//! a display frame uses is resolved to that frame's drawn time, as the objects are ([`RigTrack::resolve`]): between
//! the two views around it, or moved on from the newest by the rig's own motion for at most the rule's `l_max`.
//! Without that, a walking player's camera would step once a view and judder.
//!
//! - **Past the newest view the point under the head is carried on,** not the rig's origin (@design-reviewer's M2):
//!   in a smooth turn the origin swings round the head while the head stands still, so carrying the origin on in a
//!   straight line would slide the drawn head sideways. The rig is rebuilt from the carried point with the newest
//!   view's rotation, then turned (below).
//! - **A correction snaps** (the user's 1a): the rig of a view a rollback corrected is simply the new
//!   one, reached when the drawn time does, with no glide. A change of velocity between view pairs larger than
//!   anything a player does in one view ([`SNAP_DV`]) is such a snap, and so is any speed past [`RIG_SPEED_CAP`]:
//!   never carried on. A long fall is (@design-reviewer's L1).
//! - **A new history starts over:** a view of another domain, or of a frame below the newest kept (a rejoin, a line
//!   switch), and a gap in the batches, clear what the track kept (@design-reviewer's L2).
//! - **Between two views the point under the head is interpolated too,** and the rig rebuilt from it with the
//!   interpolated rotation: the views' rigs turn about the head, so lerping their origins would move the head along
//!   the chord (0.3 m at a quarter turn with the head 1 m from the playspace's centre, a VR recentre's turn in one view;
//!   @chunks-reviewer on L4a.1).
//! - **A turn shows at once** ([`pivot`]): the heading is the player's own input, so the drawn rig takes the local
//!   feed's newest heading instead of waiting a tick and the display delay for the simulation to step it. The rig
//!   turns about the head by the heading's lead over the view's, however large (a recentre's can be half a turn), as
//!   the simulation will turn it, so when the simulation catches up the lead is zero and nothing moves. Only while the
//!   view has the local player's own body: without one the simulation never turns the rig, so neither does the
//!   display.

use std::collections::VecDeque;
use std::time::Duration;

use pe_seams::{DomainId, Frame, FrameView, HostTime, MoveInput, Offset, Pos, RigTransform};

/// The fastest a rig is carried on past the newest view, m/s [I]: past a player's fall (pe-physics PLAYER's
/// max_fall, 30 m/s) plus its walk (5 m/s).
pub const RIG_SPEED_CAP: f64 = 40.0;
/// The largest change of the rig's velocity between two pairs of views that is still a player's own motion, m/s [I]:
/// a jump's take-off (about 5 m/s) or a stop from a walk (5 m/s) stays below it; a correction's jump between two
/// views (0.3 m in 1/60 s is 18 m/s) is above it, and snaps.
pub const SNAP_DV: f64 = 8.0;
/// How many views the track keeps: enough to span a Shown delay.
const KEPT: usize = 16;

/// One view the track keeps: its domain, frame and time, its local rig, and the local player's own head there (the
/// point under it is what a turn keeps still), if the view has one.
#[derive(Clone, Copy, Debug)]
struct Kept {
    domain: DomainId,
    frame: Frame,
    time: HostTime,
    rig: RigTransform,
    head: Option<Pos>,
}

impl Kept {
    /// The point carried on past the newest view: under the own head, else the rig's origin.
    fn point(&self) -> Pos {
        self.head.unwrap_or(self.rig.pos)
    }
}

/// The local rig of the views a display took, by frame.
#[derive(Clone, Debug, Default)]
pub struct RigTrack {
    views: VecDeque<Kept>,
}

impl RigTrack {
    /// Keep `view`'s rig. The same frame again changes nothing; another domain, or a frame below the newest kept,
    /// starts the track over from it.
    pub fn push(&mut self, view: &FrameView) {
        if let Some(last) = self.views.back() {
            if last.domain == view.domain && view.frame == last.frame {
                return;
            }
            if last.domain != view.domain || view.frame < last.frame {
                self.views.clear();
            }
        }
        let head = view.heads.iter().find(|h| h.own).map(|h| h.pos);
        self.views.push_back(Kept {
            domain: view.domain,
            frame: view.frame,
            time: view.sim_time,
            rig: view.local_rig,
            head,
        });
        while self.views.len() > KEPT {
            self.views.pop_front();
        }
    }

    /// The rig at drawn time `t`: between the two kept views around it; the oldest's before them; past the newest,
    /// moved on for at most `l_max` by the motion of the point under the own head over the last two views, the
    /// newest's rotation kept, unless that is a correction's snap (a velocity change past [`SNAP_DV`] from the pair
    /// before, or a speed past [`RIG_SPEED_CAP`]). `None` before any view.
    pub fn resolve(&self, t: HostTime, l_max: Duration) -> Option<RigTransform> {
        let newest = *self.views.back()?;
        if t >= newest.time {
            let n = self.views.len();
            let Some(prev) = n.checked_sub(2).map(|i| self.views[i]) else { return Some(newest.rig) };
            let Some(v) = velocity(&prev, &newest) else { return Some(newest.rig) };
            let speed = v.iter().map(|c| c * c).sum::<f64>().sqrt();
            let jumped = n
                .checked_sub(3)
                .and_then(|i| velocity(&self.views[i], &prev))
                .is_some_and(|u| (0..3).map(|i| (v[i] - u[i]).powi(2)).sum::<f64>().sqrt() > SNAP_DV);
            if speed > RIG_SPEED_CAP || jumped {
                return Some(newest.rig);
            }
            let ahead = t.secs_since(newest.time).min(l_max.as_secs_f64());
            let pos = newest.rig.pos.checked_add(Offset(v.map(|c| c * ahead))).ok()?;
            return Some(RigTransform { pos, ..newest.rig });
        }
        let (mut before, mut after) = (None, None);
        for v in &self.views {
            if v.time <= t {
                before = Some(*v);
            } else {
                after = Some(*v);
                break;
            }
        }
        match (before, after) {
            (Some(a), Some(b)) => {
                let k = (t.secs_since(a.time) / b.time.secs_since(a.time)).clamp(0.0, 1.0);
                match (a.head, b.head) {
                    (Some(ha), Some(hb)) => Some(about_heads(&a.rig, ha, &b.rig, hb, k)),
                    _ => Some(lerp(&a.rig, &b.rig, k)),
                }
            }
            (None, Some(b)) => Some(b.rig),
            (Some(a), None) => Some(a.rig),
            (None, None) => None,
        }
    }

    /// Forget every view (a session ended, or a gap the caller wants to start over from).
    pub fn clear(&mut self) {
        self.views.clear();
    }
}

/// The velocity of the carried point from `a` to `b`, m/s: `None` if they're at the same time.
fn velocity(a: &Kept, b: &Kept) -> Option<[f64; 3]> {
    let dt = b.time.secs_since(a.time);
    (dt > 0.0).then(|| (b.point() - a.point()).0.map(|c| c / dt))
}

/// `a` to `b` by `k` in 0..=1: the position along the line between them, the rotation by normalized lerp (both are
/// turns about +Y, close together), the scale linearly.
fn lerp(a: &RigTransform, b: &RigTransform, k: f64) -> RigTransform {
    let d = (b.pos - a.pos).0;
    let pos = a.pos.checked_add(Offset(d.map(|c| c * k))).unwrap_or(a.pos);
    // The shorter way round.
    let dot: f32 = a.rotation.iter().zip(&b.rotation).map(|(x, y)| x * y).sum();
    let s = if dot < 0.0 { -1.0 } else { 1.0 };
    let kf = k as f32;
    let q: [f32; 4] = std::array::from_fn(|i| a.rotation[i] * (1.0 - kf) + s * b.rotation[i] * kf);
    let n = q.iter().map(|v| v * v).sum::<f32>().sqrt();
    let rotation = if n > 1e-6 { q.map(|v| v / n) } else { b.rotation };
    RigTransform { pos, rotation, scale: a.scale + (b.scale - a.scale) * kf }
}

/// `a` to `b` by `k`, each with the own head's world point (`ha`, `hb`): the rotation and the height as [`lerp`] gives
/// them, and the origin rebuilt from the interpolated head point and head offset over the floor (each view's in its
/// own rig's frame), so a turn between the views keeps the head where it is.
fn about_heads(a: &RigTransform, ha: Pos, b: &RigTransform, hb: Pos, k: f64) -> RigTransform {
    let r = lerp(a, b, k);
    let off = |rig: &RigTransform, h: Pos| {
        let d = (h - rig.pos).0;
        rotate_y(-yaw_of(rig.rotation), [d[0], 0.0, d[2]])
    };
    let (oa, ob) = (off(a, ha), off(b, hb));
    let o = [oa[0] + (ob[0] - oa[0]) * k, 0.0, oa[2] + (ob[2] - oa[2]) * k];
    let Ok(h) = ha.checked_add(Offset((hb - ha).0.map(|c| c * k))) else { return r };
    let w = rotate_y(yaw_of(r.rotation), o);
    let pos = h.checked_add(Offset([-w[0], (r.pos - h).0[1], -w[2]])).unwrap_or(r.pos);
    RigTransform { pos, ..r }
}

/// The yaw of a rig's rotation about +Y, radians (positive turning left).
fn yaw_of(q: [f32; 4]) -> f64 {
    2.0 * f64::from(q[1]).atan2(f64::from(q[3]))
}

/// A wrapping heading (2^32 one turn) in radians in [-pi, pi).
fn heading_radians(heading: u32) -> f64 {
    f64::from(heading as i32) * (std::f64::consts::PI / 2_147_483_648.0)
}

/// `rig` turned about the local player's head to the heading `local` carries: the lead of that heading over the rig's
/// own yaw, the shorter way round (up to half a turn: a VR recentre's, xr.md 3.2), about the vertical through the
/// head, where `local.head` puts it under `rig`. So the head stays where it is, and only what is around it turns, as
/// the simulation's own turn will.
pub fn pivot(rig: &RigTransform, local: &MoveInput) -> RigTransform {
    let mut lead = heading_radians(local.heading) - yaw_of(rig.rotation);
    lead = (lead + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI;
    if lead == 0.0 || !lead.is_finite() {
        return *rig;
    }
    let s = f64::from(rig.scale);
    let [hx, _, hz] = local.head.position.map(|c| f64::from(c) * s);
    // The head over the floor, in the world's axes, under the rig as it is.
    let head = rotate_y(yaw_of(rig.rotation), [hx, 0.0, hz]);
    // Turned by the lead about the head: the rig's origin moves by head - R(lead) * head.
    let turned = rotate_y(lead, head);
    let pos = rig.pos.checked_add(Offset([head[0] - turned[0], 0.0, head[2] - turned[2]])).unwrap_or(rig.pos);
    let (sl, cl) = ((lead / 2.0).sin() as f32, (lead / 2.0).cos() as f32);
    let [x, y, z, w] = rig.rotation;
    // A turn about +Y, then the rig's own rotation: [0, sl, 0, cl] * q.
    let rotation = [cl * x + sl * z, cl * y + sl * w, cl * z - sl * x, cl * w - sl * y];
    RigTransform { pos, rotation, scale: rig.scale }
}

/// `v` turned `yaw` radians about +Y (positive turning left: -Z toward -X).
fn rotate_y(yaw: f64, v: [f64; 3]) -> [f64; 3] {
    let (s, c) = yaw.sin_cos();
    [c * v[0] + s * v[2], v[1], -s * v[0] + c * v[2]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{DomainId, Pos, RigPose, ViewCounters};

    fn view(frame: u64, ms: i64, x: f64, yaw: f64) -> FrameView {
        let (s, c) = ((yaw / 2.0).sin() as f32, (yaw / 2.0).cos() as f32);
        FrameView {
            frame: Frame(frame),
            domain: DomainId(0),
            sim_time: HostTime::from_millis(ms),
            final_frame: None,
            health: pe_seams::Health::Agreed,
            local_rig: RigTransform {
                pos: Pos::ORIGIN.checked_add(Offset([x, 0.0, 0.0])).unwrap(),
                rotation: [0.0, s, 0.0, c],
                scale: 1.0,
            },
            objects: vec![],
            effects: vec![],
            retracted: vec![],
            counters: ViewCounters::default(),
            changes: vec![],
            activations: Vec::new(),
            activations_retracted: Vec::new(),
            hits: Vec::new(),
            hits_from: None,
            heads: Vec::new(),
        }
    }

    fn x(r: &RigTransform) -> f64 {
        (r.pos - Pos::ORIGIN).0[0]
    }

    /// A rig walking at 3 m/s, a view each 1/60 s: a frame drawn between two views is between them, and one drawn past
    /// the newest moves on at the walk, for at most `l_max`.
    #[test]
    fn the_rig_is_resolved_between_views_and_moved_on_past_the_newest() {
        let mut t = RigTrack::default();
        for f in 0..4u64 {
            t.push(&view(f, (f as f64 * 1000.0 / 60.0) as i64, 0.05 * f as f64, 0.0));
        }
        let at = |t: &RigTrack, ms: i64| x(&t.resolve(HostTime::from_millis(ms), Duration::from_millis(33)).unwrap());
        assert!((at(&t, 25) - 0.075).abs() < 0.004, "{}", at(&t, 25));
        assert!((at(&t, 50) - 0.15).abs() < 1e-9, "the newest at its time: {}", at(&t, 50));
        assert!((at(&t, 60) - 0.18).abs() < 0.004, "moved on at the walk: {}", at(&t, 60));
        assert!((at(&t, 500) - (0.15 + 3.0 * 0.033)).abs() < 0.01, "for at most l_max: {}", at(&t, 500));
        assert_eq!(at(&t, -10), 0.0, "before the first view: the first");
        // A view taken again changes nothing.
        t.push(&view(3, 50, 9.0, 0.0));
        assert!((at(&t, 50) - 0.15).abs() < 1e-9);
    }

    /// A rollback that moves the rig by 0.3 m between two views of a player standing still is a snap, never carried
    /// on as an 18 m/s walk.
    #[test]
    fn a_correction_snaps_and_isn_t_carried_on() {
        let mut t = RigTrack::default();
        t.push(&view(9, 983, 0.0, 0.0));
        t.push(&view(10, 1000, 0.0, 0.0));
        t.push(&view(11, 1017, 0.3, 0.0));
        let r = t.resolve(HostTime::from_millis(1040), Duration::from_millis(33)).unwrap();
        assert!((x(&r) - 0.3).abs() < 1e-9, "held at the corrected rig: {}", x(&r));
    }

    /// A quarter turn between two views (a VR recentre's in one view, @chunks-reviewer on L4a.1), the feet still and
    /// the head 1 m off the rig's origin: drawn midway between them, the head is where it was, not on the chord.
    #[test]
    fn a_turn_between_two_views_keeps_the_head_still() {
        let feet = Pos::ORIGIN;
        let head = |v: &mut FrameView| {
            v.heads = vec![pe_seams::HeadView {
                member: pe_seams::MemberKey([1; 32]),
                body: pe_seams::ObjectId::from_u128(9),
                pos: feet.checked_add(Offset([0.0, 1.6, 0.0])).unwrap(),
                rotation: [0.0, 0.0, 0.0, 1.0],
                own: true,
            }];
        };
        let mut t = RigTrack::default();
        for (f, yaw) in [(0u64, 0.0f64), (1, std::f64::consts::FRAC_PI_2)] {
            let mut v = view(f, f as i64 * 16, 0.0, yaw);
            let o = rotate_y(yaw, [1.0, 0.0, 0.0]);
            v.local_rig.pos = feet.checked_add(Offset([-o[0], 0.0, -o[2]])).unwrap();
            head(&mut v);
            t.push(&v);
        }
        let r = t.resolve(HostTime::from_millis(8), Duration::ZERO).unwrap();
        assert!((yaw_of(r.rotation) - std::f64::consts::FRAC_PI_4).abs() < 1e-5, "{}", yaw_of(r.rotation));
        let h = rotate_y(yaw_of(r.rotation), [1.0, 0.0, 0.0]);
        let w = (r.pos.checked_add(Offset(h)).unwrap() - feet).0;
        assert!(w[0].abs() < 1e-5 && w[2].abs() < 1e-5, "the head moved {w:?}");
    }

    /// @design-reviewer's L1: a steady fall at 20 m/s is carried on past the newest view, not held as a snap.
    #[test]
    fn a_long_fall_is_carried_on() {
        let mut t = RigTrack::default();
        for f in 0..4u64 {
            let mut v = view(f, (f as f64 * 1000.0 / 60.0) as i64, 0.0, 0.0);
            let y = -20.0 * f as f64 / 60.0;
            v.local_rig.pos = Pos::ORIGIN.checked_add(Offset([0.0, y, 0.0])).unwrap();
            t.push(&v);
        }
        let r = t.resolve(HostTime::from_millis(75), Duration::from_millis(33)).unwrap();
        let y = (r.pos - Pos::ORIGIN).0[1];
        assert!((y - (-1.0 - 20.0 * 0.025)).abs() < 0.02, "carried on at the fall: {y}");
    }

    /// @design-reviewer's L2: a view of another domain, or of a frame below the newest, starts over from it.
    #[test]
    fn another_domain_or_an_earlier_frame_starts_over() {
        let mut t = RigTrack::default();
        t.push(&view(100, 1000, 5.0, 0.0));
        let mut other = view(5, 1100, -2.0, 0.0);
        other.domain = DomainId(1);
        t.push(&other);
        assert!((x(&t.resolve(HostTime::from_millis(1100), Duration::ZERO).unwrap()) + 2.0).abs() < 1e-9);
        t.push(&view(3, 1200, 7.0, 0.0));
        assert!((x(&t.resolve(HostTime::from_millis(1200), Duration::ZERO).unwrap()) - 7.0).abs() < 1e-9);
    }

    /// @design-reviewer's M2: a 2 rad/s turn with the feet still and the head 1 m off the rig's origin, drawn 25 ms
    /// past the newest view under Present with the matching heading lead: the head stays within 1 cm of where it is,
    /// because the point under it, not the swinging origin, is carried on.
    #[test]
    fn a_turn_past_the_newest_view_keeps_the_head_still() {
        let head = [1.0f32, 1.6, 0.0];
        let feet = Pos::ORIGIN;
        let mut t = RigTrack::default();
        let yaw_at = |ms: f64| 2.0 * ms / 1000.0;
        for f in 0..4u64 {
            let ms = f as f64 * 1000.0 / 60.0;
            let yaw = yaw_at(ms);
            let mut v = view(f, ms as i64, 0.0, yaw);
            let o = rotate_y(yaw, [f64::from(head[0]), 0.0, f64::from(head[2])]);
            v.local_rig.pos = feet.checked_add(Offset([-o[0], 0.0, -o[2]])).unwrap();
            v.heads = vec![pe_seams::HeadView {
                member: pe_seams::MemberKey([1; 32]),
                body: pe_seams::ObjectId::from_u128(9),
                pos: feet.checked_add(Offset([0.0, 1.6, 0.0])).unwrap(),
                rotation: [0.0, 0.0, 0.0, 1.0],
                own: true,
            }];
            t.push(&v);
        }
        let at = 50.0 + 25.0;
        let r = t.resolve(HostTime::from_millis(at as i64), Duration::from_millis(33)).unwrap();
        let to = |radians: f64| (radians / std::f64::consts::PI * 2_147_483_648.0) as i64 as u32;
        let local = MoveInput {
            sampled_at: HostTime(0),
            stick: [0.0, 0.0],
            stick_yaw: 0.0,
            heading: to(yaw_at(at)),
            jumps: 0,
            head: RigPose { position: head, orientation: [0.0, 0.0, 0.0, 1.0] },
            tracked: true,
        };
        let drawn = pivot(&r, &local);
        let h = rotate_y(yaw_of(drawn.rotation), [1.0, 0.0, 0.0]);
        let w = (drawn.pos.checked_add(Offset(h)).unwrap() - feet).0;
        assert!(w[0].abs() < 0.01 && w[2].abs() < 0.01, "the head swam {w:?}");
    }

    /// The heading's lead turns the rig about the head: the head stays put, the rig's yaw is the local heading, and
    /// with no lead nothing changes. A lead past the cap turns only as far as the cap.
    #[test]
    fn a_turn_pivots_on_the_head_at_once() {
        let rig = view(1, 0, 2.0, 0.3).local_rig;
        let local = |heading: u32| MoveInput {
            sampled_at: HostTime(0),
            stick: [0.0, 0.0],
            stick_yaw: 0.0,
            heading,
            jumps: 0,
            head: RigPose { position: [0.4, 1.6, -0.2], orientation: [0.0, 0.0, 0.0, 1.0] },
            tracked: true,
        };
        let head_world = |r: &RigTransform| {
            let h = rotate_y(yaw_of(r.rotation), [0.4, 1.6, -0.2]);
            (r.pos.checked_add(Offset(h)).unwrap() - Pos::ORIGIN).0
        };
        let to = |radians: f64| (radians / std::f64::consts::PI * 2_147_483_648.0) as i64 as u32;
        let turned = pivot(&rig, &local(to(0.5)));
        assert!((yaw_of(turned.rotation) - 0.5).abs() < 1e-5, "{}", yaw_of(turned.rotation));
        let (a, b) = (head_world(&rig), head_world(&turned));
        assert!((0..3).all(|i| (a[i] - b[i]).abs() < 1e-5), "the head stays: {a:?} {b:?}");
        let same = pivot(&rig, &local(to(0.3)));
        assert!((x(&same) - 2.0).abs() < 1e-5 && (yaw_of(same.rotation) - 0.3).abs() < 1e-5);
        // A recentre's lead, close to half a turn, turns all the way at once, the head still put.
        let far = pivot(&rig, &local(to(0.3 + 2.9)));
        assert!((yaw_of(far.rotation) - (0.3 + 2.9)).abs() < 1e-5, "{}", yaw_of(far.rotation));
        let c = head_world(&far);
        assert!((0..3).all(|i| (a[i] - c[i]).abs() < 1e-5), "the head stays: {a:?} {c:?}");
    }
}
