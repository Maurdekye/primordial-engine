//! A recentre's space correction (spec 3.2, locomotion L4a.1).
//!
//! When the runtime recentres, re-localises or the chaperone is edited (`XrEventDataReferenceSpaceChangePending`),
//! every pose it locates in pe-xr's reference space jumps, although the player didn't move. pe-xr keeps a
//! correction `C`, a rigid transform of the rig frame applied at the source to every pose it hands on (the eyes for
//! drawing, the head, the hands, the pointers, the joints), so nothing downstream sees the jump.
//!
//! At each change, from its change time on, with `J` the jump (a pose in the new space is `J ∘ p` in the old
//! space), `C ← RotAbout(q, -ψ) ∘ C ∘ J`: `ψ` is the jump's yaw and `q` the corrected head. The yaw goes into the
//! locomotion heading instead (@coordinator-opus, on @fable-reviewer's). The
//! simulation turns the rig about the head, so the head's world pose stays put and the body turns to line up under
//! it. `C` then stays a translation. [`RecentreYaw::Keep`] instead leaves the yaw in `C`, so nothing turns.
//!
//! `J` is the event's `poseInPreviousSpace` when the runtime says it's valid. Otherwise it's measured from the head:
//! the jump between the last head before the change time and the first one after it, so the head's world pose
//! stays continuous across the change. Recentres are about gravity, so `J` keeps only its yaw.
//!
//! The layer's views stay the runtime's own: the compositor needs them in its space. Only the drawn world moves.

use std::collections::VecDeque;

use pe_render::HandJoints;

use crate::backend::{EyePose, Located};
use crate::hands::Reading;
use crate::locomotion::{MoveReading, yaw_of};
use crate::math::{Quat, Vec3, add, cross, qmul, qnormalize, scale, sub};
use crate::pointer::PointerReading;
use crate::seam::{HostTime, RigPose};

/// Where a recentre's yaw goes. The one switch (@coordinator-opus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecentreYaw {
    /// Into the heading: at a recentre, others see the body turn to line up under the head.
    Heading,
    /// Kept in the correction: nothing turns, and the recentre has no visible effect.
    Keep,
}

/// The ruled choice.
pub const RECENTRE_YAW: RecentreYaw = RecentreYaw::Heading;

/// How far from a change's time the heads that measure it may be [I]: a head moves little in half a second.
pub const MAX_HEAD_GAP_NS: i64 = 500_000_000;

/// One reference-space change, as the runtime announced it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpaceChange {
    /// From this time on, poses are located in the new space.
    pub change_time: HostTime,
    /// The new space's origin in the previous space, when the runtime says it's valid.
    pub previous: Option<RigPose>,
}

/// What a frame's changes did, for the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Applied {
    /// Radians to add to the heading (positive turning left, as [`yaw_of`]).
    pub yaw: f64,
    /// A change came due that could be measured neither from the event nor from the head: its jump goes through
    /// uncorrected.
    pub unmeasured: bool,
}

/// How each change was measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaceCounters {
    pub by_pose: u64,
    pub by_head: u64,
    pub unmeasured: u64,
}

#[derive(Clone, Debug)]
pub struct SpaceCorrection {
    c: RigPose,
    pending: VecDeque<SpaceChange>,
    /// The last raw head located, with its time: it measures a change only if it was located before the change time
    /// and within [`MAX_HEAD_GAP_NS`] of it (@chunks-reviewer's L1).
    last_head: Option<(HostTime, RigPose)>,
    /// The latest applied change's time and the correction before it: a reading located before that time (a hand
    /// read at `now`, earlier than the display time) still takes the old correction (@chunks-reviewer's L2). Only the
    /// latest: with two changes due in one frame, a reading between their times gets the correction between them for
    /// that one sample, which takes two recentres within a frame.
    before: Option<(HostTime, RigPose)>,
    mode: RecentreYaw,
    pub counters: SpaceCounters,
}

impl Default for SpaceCorrection {
    fn default() -> Self {
        Self::new(RECENTRE_YAW)
    }
}

impl SpaceCorrection {
    pub fn new(mode: RecentreYaw) -> Self {
        Self {
            c: RigPose::IDENTITY,
            pending: VecDeque::new(),
            last_head: None,
            before: None,
            mode,
            counters: SpaceCounters::default(),
        }
    }

    /// The runtime announced a change.
    pub fn on_change(&mut self, change: SpaceChange) {
        self.pending.push_back(change);
    }

    /// Once per frame, before any pose is corrected: applies each change due at `at` (the display time), given the
    /// raw head located at `at`.
    pub fn frame(&mut self, at: HostTime, raw_head: Option<RigPose>) -> Applied {
        let mut out = Applied::default();
        while let Some(ch) = self.pending.front().copied() {
            if ch.change_time.0 > at.0 {
                break;
            }
            self.pending.pop_front();
            let jump = match ch.previous {
                Some(p) => {
                    self.counters.by_pose += 1;
                    Some(RigPose { position: p.position, orientation: yaw_quat(f64::from(yaw_of(p.orientation))) })
                }
                None => match (self.last_head, raw_head) {
                    // Both heads close to the change time, one on each side: a head polled after the change in the
                    // new space, or one from long ago (the player walked meanwhile), measures nothing.
                    (Some((old_t, old)), Some(new))
                        if old_t.0 < ch.change_time.0
                            && ch.change_time.0 - old_t.0 <= MAX_HEAD_GAP_NS
                            && at.0 - ch.change_time.0 <= MAX_HEAD_GAP_NS =>
                    {
                        self.counters.by_head += 1;
                        let r = yaw_quat(f64::from(yaw_of(old.orientation)) - f64::from(yaw_of(new.orientation)));
                        Some(RigPose { position: sub(old.position, rotate(r, new.position)), orientation: r })
                    }
                    _ => None,
                },
            };
            let Some(jump) = jump else {
                self.counters.unmeasured += 1;
                out.unmeasured = true;
                continue;
            };
            self.before = Some((ch.change_time, self.c));
            let cj = compose(self.c, jump);
            match self.mode {
                RecentreYaw::Keep => self.c = cj,
                RecentreYaw::Heading => {
                    let psi = f64::from(yaw_of(jump.orientation));
                    // The pivot: the corrected head now, or (not located) the last one.
                    let q = match (raw_head, self.last_head) {
                        (Some(h), _) => compose(cj, h).position,
                        (None, Some((_, h))) => compose(self.c, h).position,
                        (None, None) => cj.position,
                    };
                    let r = yaw_quat(-psi);
                    let about = RigPose { position: sub(q, rotate(r, q)), orientation: r };
                    self.c = compose(about, cj);
                    out.yaw += psi;
                }
            }
        }
        // A head only counts as "before" a change while one is still pending: it's raw, in the space it was located
        // in, and the next change's measurement compares it with the first head after.
        if let Some(h) = raw_head {
            self.last_head = Some((at, h));
        }
        out
    }

    /// The correction now (identity until a change).
    pub fn correction(&self) -> RigPose {
        self.c
    }

    pub fn pose(&self, p: RigPose) -> RigPose {
        compose(self.c, p)
    }

    pub fn point(&self, p: Vec3) -> Vec3 {
        add(self.c.position, rotate(self.c.orientation, p))
    }

    /// A direction or a velocity: rotated, not moved.
    pub fn vector(&self, v: Vec3) -> Vec3 {
        rotate(self.c.orientation, v)
    }

    /// No change has applied: every pose passes through bit for bit.
    fn identity(&self) -> bool {
        self.c == RigPose::IDENTITY
    }

    /// A hand reading, corrected by the correction that held at its own locate time (`r.at`).
    pub fn reading(&self, r: Reading) -> Reading {
        let c = match self.before {
            Some((t, c0)) if r.at.0 < t.0 => c0,
            _ => self.c,
        };
        if c == RigPose::IDENTITY {
            return r;
        }
        Reading {
            pose: r.pose.map(|p| compose(c, p)),
            linear_velocity: r.linear_velocity.map(|v| rotate(c.orientation, v)),
            angular_velocity: r.angular_velocity.map(|v| rotate(c.orientation, v)),
            ..r
        }
    }

    pub fn pointer(&self, r: PointerReading) -> PointerReading {
        if self.identity() {
            return r;
        }
        PointerReading { aim: r.aim.map(|p| self.pose(p)), ..r }
    }

    pub fn joints(&self, j: Option<HandJoints>) -> Option<HandJoints> {
        if self.identity() {
            return j;
        }
        j.map(|mut j| {
            for joint in &mut j.0 {
                joint.position = self.point(joint.position);
            }
            j
        })
    }

    pub fn move_reading(&self, r: MoveReading) -> MoveReading {
        if self.identity() {
            return r;
        }
        MoveReading { head: r.head.map(|p| self.pose(p)), ..r }
    }

    /// The eyes for drawing. The layer keeps the runtime's own views (the backend stores them).
    pub fn located(&self, l: Located) -> Located {
        if self.identity() || !l.valid {
            return l;
        }
        Located { eyes: l.eyes.map(|e| EyePose { pose: self.pose(e.pose), ..e }), ..l }
    }
}

/// A rotation of `psi` radians about +Y (positive turning left).
pub fn yaw_quat(psi: f64) -> Quat {
    let h = psi / 2.0;
    [0.0, h.sin() as f32, 0.0, h.cos() as f32]
}

/// `v` rotated by the unit quaternion `q`.
pub fn rotate(q: Quat, v: Vec3) -> Vec3 {
    let u = [q[0], q[1], q[2]];
    let t = scale(cross(u, v), 2.0);
    add(add(v, scale(t, q[3])), cross(u, t))
}

/// `a ∘ b`: `b` first, then `a`.
pub fn compose(a: RigPose, b: RigPose) -> RigPose {
    RigPose {
        position: add(a.position, rotate(a.orientation, b.position)),
        orientation: qnormalize(qmul(a.orientation, b.orientation)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-4;

    fn pose(position: Vec3, yaw_deg: f64) -> RigPose {
        RigPose { position, orientation: yaw_quat(yaw_deg.to_radians()) }
    }

    fn inverse(p: RigPose) -> RigPose {
        let c = [-p.orientation[0], -p.orientation[1], -p.orientation[2], p.orientation[3]];
        RigPose { position: scale(rotate(c, p.position), -1.0), orientation: c }
    }

    fn close(a: Vec3, b: Vec3) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < EPS)
    }

    fn yaw_deg(p: RigPose) -> f64 {
        f64::from(yaw_of(p.orientation)).to_degrees()
    }

    /// The world as the simulation draws it: the heading turns the rig about the corrected head.
    fn world(c: &SpaceCorrection, heading: f64, head_raw: RigPose, p_raw: RigPose) -> RigPose {
        let q = c.pose(head_raw).position;
        let r = yaw_quat(heading);
        let about = RigPose { position: sub(q, rotate(r, q)), orientation: r };
        compose(about, c.pose(p_raw))
    }

    /// The runtime's jump: 90° left and 2 m aside. A pose in the new space is `jump ∘ p` in the old one.
    fn jump() -> RigPose {
        pose([2.0, 0.0, -1.0], 90.0)
    }

    #[test]
    fn a_change_with_its_pose_keeps_the_head_and_hands_where_they_were_and_turns_the_heading() {
        let mut c = SpaceCorrection::new(RecentreYaw::Heading);
        let head_old = pose([0.3, 1.6, 0.2], 20.0);
        let hand_old = pose([0.5, 1.1, -0.3], -10.0);
        assert_eq!(c.frame(HostTime(100), Some(head_old)), Applied::default());
        let before_head = world(&c, 0.0, head_old, head_old);
        let before_hand = world(&c, 0.0, head_old, hand_old);
        c.on_change(SpaceChange { change_time: HostTime(150), previous: Some(jump()) });
        // The same body, located in the new space.
        let to_new = inverse(jump());
        let (head_new, hand_new) = (compose(to_new, head_old), compose(to_new, hand_old));
        let a = c.frame(HostTime(200), Some(head_new));
        assert!((a.yaw.to_degrees() - 90.0).abs() < 1e-3, "the jump's yaw goes into the heading: {}", a.yaw);
        assert!(!a.unmeasured);
        let after_head = world(&c, a.yaw, head_new, head_new);
        let after_hand = world(&c, a.yaw, head_new, hand_new);
        assert!(close(after_head.position, before_head.position), "{after_head:?} vs {before_head:?}");
        assert!(close(after_hand.position, before_hand.position), "{after_hand:?} vs {before_hand:?}");
        assert!((yaw_deg(after_hand) - yaw_deg(before_hand)).abs() < 1e-2);
        assert!(close(c.correction().orientation[..3].try_into().unwrap(), [0.0; 3]), "C stays a translation");
        // The corrected head itself is continuous in the rig frame: only its yaw moved into the heading.
        assert!(close(c.pose(head_new).position, head_old.position));
        assert!((yaw_deg(c.pose(head_new)) - (20.0 - 90.0)).abs() < 1e-2);
        assert_eq!(c.counters, SpaceCounters { by_pose: 1, by_head: 0, unmeasured: 0 });
    }

    #[test]
    fn without_its_pose_the_jump_is_measured_from_the_head_across_the_change_time() {
        let mut c = SpaceCorrection::new(RecentreYaw::Heading);
        let head_old = pose([0.3, 1.6, 0.2], 20.0);
        c.frame(HostTime(100), Some(head_old));
        c.on_change(SpaceChange { change_time: HostTime(300), previous: None });
        // Before its time: nothing applies, and the head is still the old space's.
        assert_eq!(c.frame(HostTime(200), Some(head_old)), Applied::default());
        let head_new = compose(inverse(jump()), head_old);
        let a = c.frame(HostTime(300), Some(head_new));
        assert!((a.yaw.to_degrees() - 90.0).abs() < 1e-3);
        assert!(close(c.pose(head_new).position, head_old.position), "the head's world pose is continuous");
        assert_eq!(c.counters.by_head, 1);
    }

    #[test]
    fn keep_leaves_the_yaw_in_the_correction_and_undoes_the_jump_exactly() {
        let mut c = SpaceCorrection::new(RecentreYaw::Keep);
        let hand_old = pose([0.5, 1.1, -0.3], -10.0);
        c.frame(HostTime(100), Some(pose([0.0, 1.6, 0.0], 0.0)));
        c.on_change(SpaceChange { change_time: HostTime(100), previous: Some(jump()) });
        let a = c.frame(HostTime(110), None);
        assert_eq!(a, Applied { yaw: 0.0, unmeasured: false });
        let back = c.pose(compose(inverse(jump()), hand_old));
        assert!(close(back.position, hand_old.position));
        assert!((yaw_deg(back) - (-10.0)).abs() < 1e-2);
        assert!(close(c.vector([0.0, 0.0, -1.0]), [-1.0, 0.0, 0.0]), "velocities turn with it");
    }

    #[test]
    fn a_change_with_no_pose_and_no_head_is_counted_unmeasured_and_corrects_nothing() {
        let mut c = SpaceCorrection::default();
        c.on_change(SpaceChange { change_time: HostTime(0), previous: None });
        let a = c.frame(HostTime(10), None);
        assert!(a.unmeasured);
        assert_eq!(c.correction(), RigPose::IDENTITY);
        assert_eq!(c.counters.unmeasured, 1);
    }

    #[test]
    fn two_changes_compose() {
        let mut c = SpaceCorrection::new(RecentreYaw::Keep);
        let p = pose([0.4, 1.0, 0.1], 5.0);
        c.on_change(SpaceChange { change_time: HostTime(10), previous: Some(jump()) });
        c.on_change(SpaceChange { change_time: HostTime(20), previous: Some(pose([-1.0, 0.0, 0.5], -30.0)) });
        c.frame(HostTime(15), None);
        let mid = compose(inverse(jump()), p);
        assert!(close(c.pose(mid).position, p.position), "only the first is due");
        c.frame(HostTime(25), None);
        let new = compose(inverse(pose([-1.0, 0.0, 0.5], -30.0)), mid);
        assert!(close(c.pose(new).position, p.position));
    }

    #[test]
    fn a_head_from_after_the_change_or_long_before_it_measures_nothing() {
        let head_old = pose([0.3, 1.6, 0.2], 20.0);
        let head_new = compose(inverse(jump()), head_old);
        // Polled a frame late: the last head was already located in the new space, at the change time.
        let mut late = SpaceCorrection::default();
        late.frame(HostTime(1_000), Some(head_new));
        late.on_change(SpaceChange { change_time: HostTime(1_000), previous: None });
        assert!(late.frame(HostTime(2_000), Some(head_new)).unmeasured, "no head before the change time");
        // The headset was off for a second: the player may have walked.
        let mut stale = SpaceCorrection::default();
        stale.frame(HostTime(0), Some(head_old));
        let t = MAX_HEAD_GAP_NS + 10;
        stale.on_change(SpaceChange { change_time: HostTime(t), previous: None });
        assert!(stale.frame(HostTime(t + 1), Some(head_new)).unmeasured, "the head before is too old");
        assert_eq!((late.correction(), stale.correction()), (RigPose::IDENTITY, RigPose::IDENTITY));
    }

    #[test]
    fn a_reading_located_before_the_change_time_keeps_the_old_correction() {
        let mut c = SpaceCorrection::new(RecentreYaw::Keep);
        c.on_change(SpaceChange { change_time: HostTime(500), previous: Some(jump()) });
        c.frame(HostTime(600), None);
        let hand = pose([0.5, 1.1, -0.3], 0.0);
        let read = |at: i64, p: RigPose| Reading {
            source: crate::seam::HandSource::Controller,
            at: HostTime(at),
            pose: Some(p),
            linear_velocity: None,
            angular_velocity: None,
            grip: None,
            curl: None,
        };
        // Read at `now` (before the change time): still in the old space, so it passes as it was.
        assert_eq!(c.reading(read(450, hand)).pose, Some(hand));
        // At the display time, past it: in the new space, corrected back.
        let fixed = c.reading(read(600, compose(inverse(jump()), hand))).pose.unwrap();
        assert!(close(fixed.position, hand.position));
    }
}
