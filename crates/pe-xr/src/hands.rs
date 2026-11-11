//! From raw hand readings to hand samples (spec 5).
//!
//! The backend reads each hand once per display frame: a tracked hand's 26 joints, or a controller's grip
//! pose and squeeze. [`HandState`] turns those readings into [`HandInput`] samples:
//! - `grip` from the joints (or the runtime's grasp value, or the squeeze);
//! - `closed`, the grasp, with hysteresis, so it never chatters;
//! - tracking loss: the last valid pose is kept, a hand that holds something releases after a grace
//!   period, and reacquiring bumps `discontinuity`;
//! - focus loss releases at once, and the hand becomes absent.

use crate::math::{Vec3, angle_between, sub};
use crate::seam::{Hand, HandInput, HandSource, HostTime, RigPose};

/// `XR_EXT_hand_tracking`'s joint indices (the spec's `XrHandJointEXT`).
pub mod joint {
    pub const PALM: usize = 0;
    pub const WRIST: usize = 1;
    pub const THUMB_METACARPAL: usize = 2;
    pub const THUMB_TIP: usize = 5;
    pub const INDEX_METACARPAL: usize = 6;
    pub const INDEX_TIP: usize = 10;
    pub const MIDDLE_METACARPAL: usize = 11;
    pub const RING_METACARPAL: usize = 16;
    pub const LITTLE_METACARPAL: usize = 21;
    pub const COUNT: usize = 26;
}

/// One joint's position in the rig frame, and whether the runtime marked it valid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Joint {
    pub position: Vec3,
    pub valid: bool,
}

/// How grip and grasp are judged. Every number is a setting, tuned at the headset test (spec 5.3, 5.5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraspConfig {
    /// `closed` turns on when `grip` rises to this.
    pub close_at: f32,
    /// `closed` turns off when `grip` falls below this.
    pub open_at: f32,
    /// A hand that holds something and loses tracking releases after this long, in seconds (X3: 250 ms).
    pub lost_grace_s: f64,
    /// The finger curl, in radians summed over two joints, that counts as a full fist.
    pub fist_curl_rad: f32,
}

impl Default for GraspConfig {
    fn default() -> Self {
        GraspConfig { close_at: 0.7, open_at: 0.4, lost_grace_s: 0.25, fist_curl_rad: 3.0 }
    }
}

/// How far a finger is bent: the bend at its proximal joint plus the bend at its intermediate joint, in
/// radians. `base` is the finger's metacarpal joint; the four joints after it are proximal, intermediate,
/// distal and tip. `None` if a joint it needs is invalid or two coincide.
pub fn finger_curl(joints: &[Joint; joint::COUNT], base: usize) -> Option<f32> {
    let p = |i: usize| joints[i].valid.then_some(joints[i].position);
    let (meta, prox, inter, dist) = (p(base)?, p(base + 1)?, p(base + 2)?, p(base + 3)?);
    let a = angle_between(sub(prox, meta), sub(inter, prox))?;
    let b = angle_between(sub(inter, prox), sub(dist, inter))?;
    Some(a + b)
}

/// The grip of a tracked hand from its joints: the mean curl of the middle, ring and little fingers,
/// scaled so that 0 is open and 1 a fist (spec 5.3). `None` if no finger can be judged.
pub fn grip_from_joints(joints: &[Joint; joint::COUNT], config: &GraspConfig) -> Option<f32> {
    let curls: Vec<f32> = [joint::MIDDLE_METACARPAL, joint::RING_METACARPAL, joint::LITTLE_METACARPAL]
        .into_iter()
        .filter_map(|base| finger_curl(joints, base))
        .collect();
    if curls.is_empty() {
        return None;
    }
    let mean = curls.iter().sum::<f32>() / curls.len() as f32;
    Some((mean / config.fist_curl_rad).clamp(0.0, 1.0))
}

/// The distance from the thumb tip to the index tip, in metres: the pinch, kept for later tools and not in
/// M1's entries (spec 5.3).
pub fn pinch_distance(joints: &[Joint; joint::COUNT]) -> Option<f32> {
    let (t, i) = (joints[joint::THUMB_TIP], joints[joint::INDEX_TIP]);
    (t.valid && i.valid).then(|| crate::math::len(sub(t.position, i.position)))
}

/// What the backend read for one hand in one display frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    pub source: HandSource,
    pub at: HostTime,
    /// The grip pose, if it is valid this frame.
    pub pose: Option<RigPose>,
    pub linear_velocity: Option<Vec3>,
    pub angular_velocity: Option<Vec3>,
    /// The grip value this frame, if one could be judged.
    pub grip: Option<f32>,
}

/// One hand's state across frames.
#[derive(Clone, Debug)]
pub struct HandState {
    hand: Hand,
    config: GraspConfig,
    last_pose: Option<RigPose>,
    last_source: Option<HandSource>,
    grip: f32,
    closed: bool,
    tracked: bool,
    /// When tracking was lost, while it is lost.
    lost_since: Option<HostTime>,
    discontinuity: u32,
    /// Releases forced by the grace period, for the counters (spec 9).
    pub forced_releases: u64,
}

impl HandState {
    pub fn new(hand: Hand, config: GraspConfig) -> Self {
        HandState {
            hand,
            config,
            last_pose: None,
            last_source: None,
            grip: 0.0,
            closed: false,
            tracked: false,
            lost_since: None,
            discontinuity: 0,
            forced_releases: 0,
        }
    }

    pub fn discontinuity(&self) -> u32 {
        self.discontinuity
    }

    /// A recentre moved the tracking space under the player (spec 3.2): the next pose jumps.
    pub fn recentre(&mut self) {
        self.discontinuity = self.discontinuity.wrapping_add(1);
    }

    /// Input focus was lost (the headset taken off, a system menu): release at once, and the hand is
    /// absent until a reading comes again (spec 5.5).
    pub fn focus_lost(&mut self) {
        self.closed = false;
        self.tracked = false;
        self.lost_since = None;
        self.last_source = None;
    }

    /// Take one frame's reading and return the sample to publish, or `None` while the hand has never been
    /// seen (it is absent).
    pub fn update(&mut self, r: Reading) -> Option<HandInput> {
        if self.last_source.is_some_and(|s| s != r.source) {
            self.discontinuity = self.discontinuity.wrapping_add(1);
        }
        self.last_source = Some(r.source);

        match r.pose {
            Some(pose) => {
                if !self.tracked && self.last_pose.is_some() {
                    // Tracking reacquired: the pose may jump; it is not a throw.
                    self.discontinuity = self.discontinuity.wrapping_add(1);
                }
                self.tracked = true;
                self.lost_since = None;
                self.last_pose = Some(pose);
                if let Some(g) = r.grip {
                    self.grip = g.clamp(0.0, 1.0);
                }
                if self.closed {
                    if self.grip < self.config.open_at {
                        self.closed = false;
                    }
                } else if self.grip >= self.config.close_at {
                    self.closed = true;
                }
            }
            None => {
                self.tracked = false;
                let since = *self.lost_since.get_or_insert(r.at);
                if self.closed && r.at.secs_since(since) >= self.config.lost_grace_s {
                    self.closed = false;
                    self.forced_releases += 1;
                }
            }
        }

        let pose = self.last_pose?;
        Some(HandInput {
            hand: self.hand,
            source: r.source,
            sampled_at: r.at,
            tracked: self.tracked,
            pose,
            linear_velocity: if self.tracked { r.linear_velocity } else { None },
            angular_velocity: if self.tracked { r.angular_velocity } else { None },
            grip: self.grip,
            closed: self.closed,
            discontinuity: self.discontinuity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(ms: i64, grip: Option<f32>, tracked: bool) -> Reading {
        Reading {
            source: HandSource::Controller,
            at: HostTime::from_millis(ms),
            pose: tracked.then_some(RigPose { position: [0.0, 1.0, -0.3], orientation: [0.0, 0.0, 0.0, 1.0] }),
            linear_velocity: tracked.then_some([0.0; 3]),
            angular_velocity: None,
            grip,
        }
    }

    /// A finger along -Z, bent by `bend` radians at each of its two middle joints.
    fn hand_with_bend(bend: f32) -> [Joint; joint::COUNT] {
        let mut j = [Joint { position: [0.0; 3], valid: true }; joint::COUNT];
        for base in
            [joint::INDEX_METACARPAL, joint::MIDDLE_METACARPAL, joint::RING_METACARPAL, joint::LITTLE_METACARPAL]
        {
            let x = base as f32 * 0.01;
            let mut pos = [x, 0.0, 0.0];
            let mut dir = 0.0f32; // angle in the Y-Z plane, 0 = -Z
            j[base].position = pos;
            for (k, bone) in [0.04f32, 0.03, 0.02, 0.02].into_iter().enumerate() {
                if k == 1 || k == 2 {
                    dir += bend;
                }
                pos = [x, pos[1] - bone * dir.sin(), pos[2] - bone * dir.cos()];
                j[base + 1 + k].position = pos;
            }
        }
        j
    }

    #[test]
    fn grip_is_zero_open_and_one_in_a_fist() {
        let c = GraspConfig::default();
        assert!(grip_from_joints(&hand_with_bend(0.0), &c).unwrap() < 0.01);
        let fist = grip_from_joints(&hand_with_bend(1.5), &c).unwrap();
        assert!((fist - 1.0).abs() < 1e-6, "{fist}");
        let half = grip_from_joints(&hand_with_bend(0.75), &c).unwrap();
        assert!((half - 0.5).abs() < 0.01, "{half}");
    }

    #[test]
    fn grip_skips_invalid_fingers_and_needs_one() {
        let c = GraspConfig::default();
        let mut j = hand_with_bend(0.75);
        j[joint::MIDDLE_METACARPAL + 2].valid = false;
        assert!(grip_from_joints(&j, &c).is_some());
        for base in [joint::RING_METACARPAL, joint::LITTLE_METACARPAL] {
            j[base].valid = false;
        }
        assert_eq!(grip_from_joints(&j, &c), None);
    }

    #[test]
    fn closed_has_hysteresis_and_never_chatters() {
        let mut h = HandState::new(Hand::Right, GraspConfig::default());
        let mut flips = 0;
        let mut last = false;
        // Noise around 0.55, between the two thresholds, after one real close.
        let grips = [0.2, 0.75, 0.6, 0.5, 0.58, 0.45, 0.52, 0.41, 0.6, 0.3, 0.5, 0.69, 0.55];
        for (i, g) in grips.into_iter().enumerate() {
            let s = h.update(reading(i as i64 * 8, Some(g), true)).unwrap();
            if s.closed != last {
                flips += 1;
                last = s.closed;
            }
        }
        // Closed at 0.75, opened at 0.3, and nothing else in between.
        assert_eq!(flips, 2);
        assert!(!last);
    }

    #[test]
    fn a_lost_hand_that_holds_releases_after_the_grace_period() {
        let mut h = HandState::new(Hand::Left, GraspConfig::default());
        assert!(h.update(reading(0, Some(0.9), true)).unwrap().closed);
        let s = h.update(reading(100, None, false)).unwrap();
        assert!(!s.tracked && s.closed, "still held inside the grace period");
        assert_eq!(s.pose.position, [0.0, 1.0, -0.3], "the last valid pose is kept");
        assert!(s.linear_velocity.is_none());
        assert!(h.update(reading(349, None, false)).unwrap().closed);
        let s = h.update(reading(350, None, false)).unwrap();
        assert!(!s.closed, "released 250 ms after tracking was lost");
        assert_eq!(h.forced_releases, 1);
    }

    #[test]
    fn a_lost_hand_that_does_not_hold_just_goes_untracked() {
        let mut h = HandState::new(Hand::Left, GraspConfig::default());
        h.update(reading(0, Some(0.1), true));
        let s = h.update(reading(1000, None, false)).unwrap();
        assert!(!s.tracked && !s.closed);
        assert_eq!(h.forced_releases, 0);
    }

    #[test]
    fn reacquiring_recentring_and_changing_source_bump_the_discontinuity() {
        let mut h = HandState::new(Hand::Right, GraspConfig::default());
        let d0 = h.update(reading(0, Some(0.1), true)).unwrap().discontinuity;
        assert_eq!(h.update(reading(8, Some(0.1), true)).unwrap().discontinuity, d0, "plain motion");
        h.update(reading(16, None, false));
        let d1 = h.update(reading(24, Some(0.1), true)).unwrap().discontinuity;
        assert_eq!(d1, d0 + 1, "reacquired");
        h.recentre();
        let d2 = h.update(reading(32, Some(0.1), true)).unwrap().discontinuity;
        assert_eq!(d2, d1 + 1, "recentred");
        let mut r = reading(40, Some(0.1), true);
        r.source = HandSource::Tracked;
        assert_eq!(h.update(r).unwrap().discontinuity, d2 + 1, "source changed");
    }

    #[test]
    fn focus_loss_releases_at_once() {
        let mut h = HandState::new(Hand::Right, GraspConfig::default());
        assert!(h.update(reading(0, Some(0.9), true)).unwrap().closed);
        h.focus_lost();
        // Focus back, but the grip reading still says "closed": a new close needs no reopen first, since
        // the hysteresis state was reset with the release.
        let s = h.update(reading(500, Some(0.5), true)).unwrap();
        assert!(!s.closed, "0.5 is below close_at, so the hand stays open after focus returns");
    }

    #[test]
    fn a_hand_never_seen_is_absent() {
        let mut h = HandState::new(Hand::Left, GraspConfig::default());
        assert_eq!(h.update(reading(0, None, false)), None);
    }
}
