//! From raw hand readings to hand samples (spec 5).
//!
//! The backend reads each hand once per display frame: a tracked hand's 26 joints, or a controller's grip
//! pose and squeeze. [`HandState`] turns those readings into [`HandInput`] samples:
//! - two grip signals: the squeeze action's value while that action is active (a controller's squeeze, or the
//!   runtime's grasp value through `XR_EXT_hand_interaction`), and the joints' curl for a tracked hand;
//! - `closed`, the grasp, with hysteresis, so it never chatters ([`Grasp`]: either signal closes the hand, and the
//!   one that closed it opens it);
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
    /// The squeeze action's value this frame, while that action is active: a controller's squeeze, or the
    /// runtime's grasp value through `XR_EXT_hand_interaction`, whichever interaction profile is current.
    pub grip: Option<f32>,
    /// A tracked hand's grip from its joints' curl ([`grip_from_joints`]), if one could be judged.
    pub curl: Option<f32>,
}

/// The grip signal that closed a hand ([`Grasp`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GripSignal {
    Squeeze,
    Curl,
}

/// A close or an open of the grasp, with what decided it (for the diagnostic log).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraspEdge {
    pub closed: bool,
    /// The signal that closed the hand (on a close), or that closed it before (on an open).
    pub by: GripSignal,
    pub squeeze: Option<f32>,
    pub curl: Option<f32>,
}

/// The grasp from two grip signals (spec 5.3). Some runtimes report a tracked hand's joints while a controller is
/// held (SteamVR): the curl around a held controller need not reach `close_at`, so the squeeze must close the hand
/// too, and a bare hand has only the curl.
/// - **Close:** when either signal rises through `close_at` (it was below, or not read, the frame before).
/// - **The squeeze always acts:** rising through `close_at` it takes over a grasp the curl closed (a close edge by
///   the squeeze), and falling from at least `open_at` to below it opens the hand whichever signal closed it.
/// - **Open:** otherwise when the signal that closed the hand falls below `open_at`, or, while that signal is not
///   read, the other one does. With neither read, the grasp holds.
///   So a resting curl above `open_at` can't hold a released
///   squeeze closed, and a curl that stays above `close_at` around a held controller doesn't close the hand again
///   once the squeeze opened it: it has to rise through `close_at` again.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Grasp {
    closed_by: Option<GripSignal>,
    last_squeeze: Option<f32>,
    last_curl: Option<f32>,
}

impl Grasp {
    pub fn closed(&self) -> bool {
        self.closed_by.is_some()
    }

    /// Opens without an edge (focus loss, the lost-hand grace). The signals read before are forgotten, so a squeeze
    /// still held when the hand is read again closes it again, as a fresh reading would.
    pub fn release(&mut self) {
        *self = Grasp::default();
    }

    /// One frame's signals; the edge, if the grasp changed.
    pub fn step(&mut self, squeeze: Option<f32>, curl: Option<f32>, c: &GraspConfig) -> Option<GraspEdge> {
        let rose = |now: Option<f32>, before: Option<f32>| {
            now.is_some_and(|v| v >= c.close_at) && before.is_none_or(|b| b < c.close_at)
        };
        // Only from a read value at or above `open_at`: a controller left on at rest doesn't open a bare hand's grasp.
        let fell = |now: Option<f32>, before: Option<f32>| {
            now.is_some_and(|v| v < c.open_at) && before.is_some_and(|b| b >= c.open_at)
        };
        let edge = match self.closed_by {
            None => {
                let by = if rose(squeeze, self.last_squeeze) {
                    Some(GripSignal::Squeeze)
                } else if rose(curl, self.last_curl) {
                    Some(GripSignal::Curl)
                } else {
                    None
                };
                by.map(|by| {
                    self.closed_by = Some(by);
                    GraspEdge { closed: true, by, squeeze, curl }
                })
            }
            // The squeeze always acts (@coordinator-opus m9afb0b751e6d, on @design-reviewer's Medium): rising through
            // `close_at` it takes over a grasp the curl closed, and falling through `open_at` it opens the hand
            // whichever signal closed it. A curl already high round a held controller can't keep it closed.
            Some(GripSignal::Curl) if rose(squeeze, self.last_squeeze) => {
                self.closed_by = Some(GripSignal::Squeeze);
                Some(GraspEdge { closed: true, by: GripSignal::Squeeze, squeeze, curl })
            }
            Some(by) if fell(squeeze, self.last_squeeze) => {
                self.closed_by = None;
                Some(GraspEdge { closed: false, by, squeeze, curl })
            }
            Some(by) => {
                let (own, other) = match by {
                    GripSignal::Squeeze => (squeeze, curl),
                    GripSignal::Curl => (curl, squeeze),
                };
                own.or(other).filter(|&v| v < c.open_at).map(|_| {
                    self.closed_by = None;
                    GraspEdge { closed: false, by, squeeze, curl }
                })
            }
        };
        self.last_squeeze = squeeze;
        self.last_curl = curl;
        edge
    }
}

/// One hand's state across frames.
#[derive(Clone, Debug)]
pub struct HandState {
    hand: Hand,
    config: GraspConfig,
    last_pose: Option<RigPose>,
    last_source: Option<HandSource>,
    grip: f32,
    grasp: Grasp,
    /// The last close or open, until [`Self::take_edge`] reads it.
    edge: Option<GraspEdge>,
    /// The largest squeeze and curl read, for the diagnostic log.
    pub max_squeeze: Option<f32>,
    pub max_curl: Option<f32>,
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
            grasp: Grasp::default(),
            edge: None,
            max_squeeze: None,
            max_curl: None,
            tracked: false,
            lost_since: None,
            discontinuity: 0,
            forced_releases: 0,
        }
    }

    pub fn discontinuity(&self) -> u32 {
        self.discontinuity
    }

    /// The last close or open since the previous call, for the diagnostic log.
    pub fn take_edge(&mut self) -> Option<GraspEdge> {
        self.edge.take()
    }

    /// A recentre moved the tracking space under the player (spec 3.2): the next pose jumps.
    pub fn recentre(&mut self) {
        self.discontinuity = self.discontinuity.wrapping_add(1);
    }

    /// Input focus was lost (the headset taken off, a system menu): release at once, and the hand is
    /// absent until a reading comes again (spec 5.5).
    pub fn focus_lost(&mut self) {
        self.grasp.release();
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
                let squeeze = r.grip.map(|g| g.clamp(0.0, 1.0));
                let curl = r.curl.map(|g| g.clamp(0.0, 1.0));
                let max = |m: Option<f32>, v: Option<f32>| match (m, v) {
                    (Some(m), Some(v)) => Some(m.max(v)),
                    (m, v) => m.or(v),
                };
                self.max_squeeze = max(self.max_squeeze, squeeze);
                self.max_curl = max(self.max_curl, curl);
                if let Some(g) = max(squeeze, curl) {
                    self.grip = g;
                }
                if let Some(e) = self.grasp.step(squeeze, curl, &self.config) {
                    self.edge = Some(e);
                }
            }
            None => {
                self.tracked = false;
                let since = *self.lost_since.get_or_insert(r.at);
                if self.grasp.closed() && r.at.secs_since(since) >= self.config.lost_grace_s {
                    self.grasp.release();
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
            closed: self.grasp.closed(),
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
            curl: None,
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
    fn either_signal_closes_and_the_one_that_closed_opens() {
        let c = GraspConfig::default();
        // A held controller on a runtime that reports joints: the curl stays at 0.5, the squeeze closes and opens.
        let mut g = Grasp::default();
        assert_eq!(g.step(Some(0.0), Some(0.5), &c), None);
        let e = g.step(Some(0.9), Some(0.5), &c).expect("the squeeze closes");
        assert!(e.closed && e.by == GripSignal::Squeeze);
        assert_eq!(g.step(Some(0.5), Some(0.5), &c), None, "between the thresholds: still closed");
        let e = g.step(Some(0.1), Some(0.5), &c).expect("the squeeze opens");
        assert!(!e.closed && e.by == GripSignal::Squeeze);
        // A bare hand: no squeeze, the curl closes and opens.
        let mut g = Grasp::default();
        assert!(g.step(None, Some(0.8), &c).is_some_and(|e| e.closed && e.by == GripSignal::Curl));
        assert!(g.step(None, Some(0.3), &c).is_some_and(|e| !e.closed));
    }

    #[test]
    fn a_resting_curl_does_not_hold_a_released_squeeze_closed() {
        let c = GraspConfig::default();
        let mut g = Grasp::default();
        g.step(Some(0.0), Some(0.45), &c);
        assert!(g.step(Some(0.9), Some(0.45), &c).is_some());
        // The squeeze is let go; the curl rests at 0.45, above open_at: the hand opens all the same.
        assert!(g.step(Some(0.1), Some(0.45), &c).is_some_and(|e| !e.closed));
        assert!(!g.closed());
    }

    #[test]
    fn a_curl_that_stays_high_does_not_close_again_until_it_rises_again() {
        let c = GraspConfig::default();
        let mut g = Grasp::default();
        // Fingers wrapped round a held controller at a curl of 0.8, the squeeze released: open.
        g.step(Some(0.0), Some(0.6), &c);
        assert_eq!(g.step(Some(0.0), Some(0.6), &c), None);
        assert!(g.step(Some(0.9), Some(0.8), &c).is_some_and(|e| e.closed && e.by == GripSignal::Squeeze));
        assert!(g.step(Some(0.1), Some(0.8), &c).is_some_and(|e| !e.closed && e.by == GripSignal::Squeeze));
        // The curl stays at 0.8: it does not close the hand again until it falls below close_at and rises again.
        assert_eq!(g.step(Some(0.0), Some(0.8), &c), None);
        assert_eq!(g.step(Some(0.0), Some(0.6), &c), None);
        assert!(g.step(Some(0.0), Some(0.75), &c).is_some_and(|e| e.closed && e.by == GripSignal::Curl));
    }

    /// @design-reviewer's Medium: a curl already at 0.8 round a held controller closes the hand; the squeeze must
    /// still close (take over) and open it, on each path the curl can close by.
    #[test]
    fn a_curl_already_high_never_keeps_the_squeeze_from_opening() {
        let c = GraspConfig::default();
        let press_and_release = |g: &mut Grasp, curl: Option<f32>| {
            let e = g.step(Some(0.9), curl, &c).expect("the squeeze takes the grasp over");
            assert!(e.closed && e.by == GripSignal::Squeeze);
            let e = g.step(Some(0.1), curl, &c).expect("and opens it");
            assert!(!e.closed);
            assert_eq!(g.step(Some(0.0), curl, &c), None, "the curl, still high, doesn't close it again");
            assert!(!g.closed());
        };
        // The first frame, curl already at 0.8.
        let mut g = Grasp::default();
        assert!(g.step(Some(0.0), Some(0.8), &c).is_some_and(|e| e.closed && e.by == GripSignal::Curl));
        press_and_release(&mut g, Some(0.8));
        // After a release (focus loss, the lost-hand grace): the same.
        g.release();
        assert!(g.step(Some(0.0), Some(0.8), &c).is_some_and(|e| e.closed && e.by == GripSignal::Curl));
        press_and_release(&mut g, Some(0.8));
        // The curl flickers through None and comes back at 0.8.
        g.step(Some(0.0), None, &c);
        assert!(g.step(Some(0.0), Some(0.8), &c).is_some_and(|e| e.closed && e.by == GripSignal::Curl));
        press_and_release(&mut g, Some(0.8));
        // Picking the controller up: the curl rises from an open hand's 0.2 to 0.8.
        let mut g = Grasp::default();
        g.step(Some(0.0), Some(0.2), &c);
        assert!(g.step(Some(0.0), Some(0.8), &c).is_some_and(|e| e.closed && e.by == GripSignal::Curl));
        press_and_release(&mut g, Some(0.8));
    }

    #[test]
    fn a_falling_squeeze_opens_a_curl_grasp_but_a_resting_one_does_not() {
        let c = GraspConfig::default();
        // A bare-hand fist while a controller lies on at 0: the resting squeeze doesn't open it.
        let mut g = Grasp::default();
        g.step(Some(0.0), Some(0.2), &c);
        assert!(g.step(Some(0.0), Some(0.8), &c).is_some_and(|e| e.by == GripSignal::Curl));
        assert_eq!(g.step(Some(0.0), Some(0.8), &c), None);
        assert!(g.closed());
        // A squeeze half pressed, then let go, opens it even though the curl closed it.
        assert_eq!(g.step(Some(0.5), Some(0.8), &c), None);
        assert!(g.step(Some(0.1), Some(0.8), &c).is_some_and(|e| !e.closed && e.by == GripSignal::Curl));
    }

    #[test]
    fn a_release_forgets_the_signals_so_a_held_squeeze_closes_again() {
        let c = GraspConfig::default();
        let mut g = Grasp::default();
        assert!(g.step(Some(0.9), None, &c).is_some());
        g.release();
        assert!(!g.closed());
        assert!(g.step(Some(0.9), None, &c).is_some_and(|e| e.closed), "read again after the release");
    }

    #[test]
    fn with_the_closing_signal_unread_the_other_decides_and_with_none_the_grasp_holds() {
        let c = GraspConfig::default();
        let mut g = Grasp::default();
        assert!(g.step(Some(0.9), Some(0.5), &c).is_some());
        // The controller is put down: the squeeze is no longer read, the curl decides.
        assert_eq!(g.step(None, Some(0.5), &c), None);
        assert_eq!(g.step(None, None, &c), None, "nothing read: holds");
        assert!(g.closed());
        assert!(g.step(None, Some(0.3), &c).is_some_and(|e| !e.closed && e.by == GripSignal::Squeeze));
    }

    #[test]
    fn the_hand_state_records_its_edges_and_maxima() {
        let mut h = HandState::new(Hand::Right, GraspConfig::default());
        let mut r = reading(0, Some(0.2), true);
        r.curl = Some(0.4);
        h.update(r);
        assert_eq!(h.take_edge(), None);
        let mut r = reading(8, Some(0.95), true);
        r.curl = Some(0.5);
        assert!(h.update(r).unwrap().closed);
        let e = h.take_edge().expect("a close");
        assert_eq!((e.closed, e.by, e.squeeze, e.curl), (true, GripSignal::Squeeze, Some(0.95), Some(0.5)));
        assert_eq!(h.take_edge(), None, "read once");
        assert_eq!((h.max_squeeze, h.max_curl), (Some(0.95), Some(0.5)));
    }

    #[test]
    fn a_hand_never_seen_is_absent() {
        let mut h = HandState::new(Hand::Left, GraspConfig::default());
        assert_eq!(h.update(reading(0, None, false)), None);
    }
}
