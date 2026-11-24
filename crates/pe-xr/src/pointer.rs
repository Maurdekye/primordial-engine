//! The pointers (spec 5.7): what each hand aims with and selects with, for the menus. They are local UI: never
//! sent, never in a hand entry, never simulated.
//!
//! - **The aim:** the aim pose action (`input/aim/pose` on every controller profile, and on
//!   `XR_EXT_hand_interaction`'s profile for a tracked hand). Its ray starts at the pose's position and points
//!   along its -Z. A tracked hand without the aim action is aimed from its joints ([`from_joints`]): from the
//!   index knuckle, along the wrist to the middle knuckle.
//! - **The select:** the select value action (a controller's trigger, the simple controller's select, the hand
//!   interaction pinch), or a tracked hand's pinch from its joints (thumb tip to index tip). Pressed at
//!   [`PRESS_AT`], released below [`RELEASE_AT`].
//! - **The menu toggle:** the controller's menu button (@coordinator-opus). The palm-up pinch
//!   ([`PointerState::with_palm_menu`]: a tracked hand's palm turned up, pinched and held for [`PALM_HOLD_NS`],
//!   never while that hand holds something, and never a click) is built but **off**: on a Quest the OS claims a
//!   palm-facing pinch for its own menu, so a wrist button in pe-app replaces it.

use crate::hands::joint;
use crate::math::{Quat, Vec3, add, cross, dot, len, normalize, scale, sub};
use crate::seam::{Hand, HostTime, RigPose};
use pe_render::HandJoints;

/// The select presses at this value and releases below [`RELEASE_AT`].
pub const PRESS_AT: f32 = 0.7;
pub const RELEASE_AT: f32 = 0.35;
/// A pinch from the joints: fully pressed at this thumb-to-index distance (metres) or closer...
pub const PINCH_CLOSED_M: f32 = 0.015;
/// ...and not pressed at all from this distance.
pub const PINCH_OPEN_M: f32 = 0.035;
/// A palm counts as turned up when its normal is within about 45 degrees of the rig's up.
pub const PALM_UP_COS: f32 = 0.7;
/// How long a palm-up pinch must be held to toggle the menu.
pub const PALM_HOLD_NS: i64 = 400_000_000;

/// Where a pointer's aim came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AimSource {
    /// The aim pose action.
    Action,
    /// A tracked hand's joints.
    Joints,
}

/// What the backend read for one hand's pointer (spec 5.7).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PointerReading {
    /// The aim pose action's pose, when that action is active and its pose valid.
    pub aim: Option<RigPose>,
    /// The select value action's value (0..1), while that action is active.
    pub select: Option<f32>,
    /// The menu button, while its action is active.
    pub menu: Option<bool>,
}

/// One hand's pointer for one drawn frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pointer {
    /// The ray's origin and orientation, in the rig frame: it points along the pose's -Z ([`Pointer::direction`]).
    pub aim: RigPose,
    pub source: AimSource,
    /// The select value, 0..1.
    pub select: f32,
    /// The select is held (with hysteresis).
    pub pressed: bool,
    /// Presses since the last drawn frame (so a press is never lost to a frame that wasn't drawn).
    pub clicks: u32,
    /// The palm is turned up (a tracked hand): its select is the menu gesture, so it never clicks.
    pub palm_up: bool,
}

impl Pointer {
    /// The ray's direction: the aim pose's -Z, in the rig frame.
    pub fn direction(&self) -> Vec3 {
        rotate(self.aim.orientation, [0.0, 0.0, -1.0])
    }
}

/// A tracked hand's aim and pinch from its joints: `(aim, select, palm normal)`, or `None` if a joint it needs
/// is not valid. The aim starts at the index knuckle and points from the wrist through the middle knuckle; the
/// palm normal points out of the palm.
pub fn from_joints(hand: Hand, j: &HandJoints) -> Option<(RigPose, f32, Vec3)> {
    let p = |i: usize| j.0[i].valid.then_some(j.0[i].position);
    let (wrist, index_k, middle_k) = (p(joint::WRIST)?, p(joint::INDEX_PROXIMAL)?, p(joint::MIDDLE_PROXIMAL)?);
    let (index_m, little_m) = (p(joint::INDEX_METACARPAL)?, p(joint::LITTLE_METACARPAL)?);
    let (thumb, index_t) = (p(joint::THUMB_TIP)?, p(joint::INDEX_TIP)?);
    let forward = normalize(sub(middle_k, wrist))?;
    // The cross product of the wrist-to-index and wrist-to-little metacarpals points out of a right hand's palm,
    // and out of the back of a left hand's.
    let mut normal = normalize(cross(sub(index_m, wrist), sub(little_m, wrist)))?;
    if hand == Hand::Left {
        normal = scale(normal, -1.0);
    }
    let d = len(sub(thumb, index_t));
    let select = ((PINCH_OPEN_M - d) / (PINCH_OPEN_M - PINCH_CLOSED_M)).clamp(0.0, 1.0);
    // The ray's orientation: -Z along `forward`, +Y along the back of the hand (away from the palm).
    let orientation = look_rotation(forward, scale(normal, -1.0))?;
    Some((RigPose { position: index_k, orientation }, select, normal))
}

/// One hand's pointer state across frames.
#[derive(Clone, Debug, Default)]
pub struct PointerState {
    /// The palm-up pinch toggles the menu (off by default).
    palm_menu: bool,
    pressed: bool,
    clicks: u32,
    menu_was: bool,
    /// When the current palm-up pinch began, while it lasts.
    palm_since: Option<HostTime>,
    /// The palm-up pinch toggled the menu; it must end before it can toggle again.
    palm_fired: bool,
}

/// What one frame's step gives back: the pointer, if the hand can point, and whether the menu was toggled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerStep {
    pub pointer: Option<Pointer>,
    pub menu_toggled: bool,
}

impl PointerState {
    /// A pointer whose palm-up pinch toggles the menu (off by default: see the module's notes).
    pub fn with_palm_menu() -> Self {
        PointerState { palm_menu: true, ..PointerState::default() }
    }

    /// One frame: the backend's reading, the hand's joints (a tracked hand) and whether the hand holds something.
    pub fn step(
        &mut self,
        hand: Hand,
        at: HostTime,
        r: PointerReading,
        joints: Option<&HandJoints>,
        holding: bool,
    ) -> PointerStep {
        let mut toggled = false;
        let menu = r.menu.unwrap_or(false);
        if menu && !self.menu_was {
            toggled = true;
        }
        self.menu_was = menu;

        let fallback = joints.and_then(|j| from_joints(hand, j));
        let (aim, source, select) = match (r.aim, fallback) {
            (Some(aim), _) => (aim, AimSource::Action, r.select.or(fallback.map(|f| f.1))),
            (None, Some((aim, pinch, _))) => (aim, AimSource::Joints, r.select.or(Some(pinch))),
            (None, None) => {
                // Can't point: the press and the palm gesture end, unread clicks stay for the next drawn frame.
                self.pressed = false;
                self.palm_since = None;
                self.palm_fired = false;
                return PointerStep { pointer: None, menu_toggled: toggled };
            }
        };
        let select = select.unwrap_or(0.0);
        let palm_up = self.palm_menu && fallback.is_some_and(|(_, _, n)| dot(n, [0.0, 1.0, 0.0]) >= PALM_UP_COS);

        let was = self.pressed;
        self.pressed = if was { select >= RELEASE_AT } else { select >= PRESS_AT };
        if palm_up && !holding && self.pressed {
            let since = *self.palm_since.get_or_insert(at);
            if !self.palm_fired && at.0 - since.0 >= PALM_HOLD_NS {
                self.palm_fired = true;
                toggled = true;
            }
        } else {
            self.palm_since = None;
            if !self.pressed {
                self.palm_fired = false;
            }
        }
        // A palm-up press is the menu gesture, never a click.
        if self.pressed && !was && !palm_up {
            self.clicks += 1;
        }
        PointerStep {
            pointer: Some(Pointer { aim, source, select, pressed: self.pressed, clicks: self.clicks, palm_up }),
            menu_toggled: toggled,
        }
    }

    /// The clicks were delivered with a drawn frame.
    pub fn delivered(&mut self) {
        self.clicks = 0;
    }

    /// Focus was lost: nothing stays pressed.
    pub fn reset(&mut self) {
        *self = PointerState { palm_menu: self.palm_menu, ..PointerState::default() };
    }
}

/// Rotate `v` by the unit quaternion `q`.
pub fn rotate(q: Quat, v: Vec3) -> Vec3 {
    let u = [q[0], q[1], q[2]];
    let t = scale(cross(u, v), 2.0);
    add(add(v, scale(t, q[3])), cross(u, t))
}

/// The rotation whose -Z is `forward` and whose +Y is as near `up` as `forward` allows; `None` if they are parallel.
fn look_rotation(forward: Vec3, up: Vec3) -> Option<Quat> {
    let z = scale(forward, -1.0);
    let x = normalize(cross(up, z))?;
    let y = cross(z, x);
    // From the rotation matrix with columns x, y, z.
    let (m00, m11, m22) = (x[0], y[1], z[2]);
    let tr = m00 + m11 + m22;
    let q = if tr > 0.0 {
        let s = (tr + 1.0).sqrt() * 2.0;
        [(y[2] - z[1]) / s, (z[0] - x[2]) / s, (x[1] - y[0]) / s, 0.25 * s]
    } else if m00 > m11 && m00 > m22 {
        let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
        [0.25 * s, (y[0] + x[1]) / s, (z[0] + x[2]) / s, (y[2] - z[1]) / s]
    } else if m11 > m22 {
        let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
        [(y[0] + x[1]) / s, 0.25 * s, (z[1] + y[2]) / s, (z[0] - x[2]) / s]
    } else {
        let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
        [(z[0] + x[2]) / s, (z[1] + y[2]) / s, 0.25 * s, (x[1] - y[0]) / s]
    };
    Some(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_render::HandJoint;

    const ID: RigPose = RigPose { position: [0.0, 1.0, -0.3], orientation: [0.0, 0.0, 0.0, 1.0] };

    fn ms(n: i64) -> HostTime {
        HostTime(1_000_000_000 + n * 1_000_000)
    }

    fn aimed(select: f32) -> PointerReading {
        PointerReading { aim: Some(ID), select: Some(select), menu: Some(false) }
    }

    /// A right hand, flat, pointing along -Z; `palm_down` false turns it palm up (mirrored about the forward axis).
    /// `pinch`: the thumb-to-index distance.
    fn right_hand(palm_down: bool, pinch: f32) -> HandJoints {
        let mut j = HandJoints([HandJoint { position: [0.0; 3], radius: 0.01, valid: true }; HandJoints::COUNT]);
        // The thumb is on -x for a right hand palm down, on +x palm up.
        let s = if palm_down { 1.0 } else { -1.0 };
        let mut set = |i: usize, p: Vec3| j.0[i].position = add(p, [0.0, 1.0, 0.0]);
        set(joint::WRIST, [0.0, 0.0, 0.0]);
        set(joint::INDEX_METACARPAL, [-0.02 * s, 0.0, -0.05]);
        set(joint::LITTLE_METACARPAL, [0.02 * s, 0.0, -0.05]);
        set(joint::INDEX_PROXIMAL, [-0.02 * s, 0.0, -0.09]);
        set(joint::MIDDLE_PROXIMAL, [0.0, 0.0, -0.1]);
        set(joint::INDEX_TIP, [-0.02 * s, 0.0, -0.17]);
        set(joint::THUMB_TIP, [-0.02 * s, 0.0, -0.17 + pinch]);
        j
    }

    #[test]
    fn the_select_presses_with_hysteresis_and_counts_each_click_once() {
        let mut s = PointerState::default();
        let step =
            |s: &mut PointerState, v: f32, t: i64| s.step(Hand::Right, ms(t), aimed(v), None, false).pointer.unwrap();
        assert!(!step(&mut s, 0.6, 0).pressed);
        let p = step(&mut s, 0.75, 10);
        assert!(p.pressed && p.clicks == 1 && p.source == AimSource::Action);
        assert!(step(&mut s, 0.4, 20).pressed, "above the release: still pressed");
        assert_eq!(step(&mut s, 0.4, 30).clicks, 1, "a hold is one click");
        assert!(!step(&mut s, 0.3, 40).pressed);
        assert_eq!(step(&mut s, 0.9, 50).clicks, 2, "undelivered clicks add up");
        s.delivered();
        assert_eq!(step(&mut s, 0.9, 60).clicks, 0);
    }

    #[test]
    fn the_menu_button_toggles_on_its_press_only() {
        let mut s = PointerState::default();
        let r = |m: bool| PointerReading { aim: Some(ID), select: Some(0.0), menu: Some(m) };
        assert!(!s.step(Hand::Left, ms(0), r(false), None, false).menu_toggled);
        assert!(s.step(Hand::Left, ms(10), r(true), None, false).menu_toggled);
        assert!(!s.step(Hand::Left, ms(20), r(true), None, false).menu_toggled, "held: no repeat");
        assert!(!s.step(Hand::Left, ms(30), r(false), None, false).menu_toggled);
        // The button works with no pointer at all (an invalid aim pose).
        let blind = PointerReading { aim: None, select: None, menu: Some(true) };
        assert!(s.step(Hand::Left, ms(40), blind, None, false).menu_toggled);
    }

    #[test]
    fn a_tracked_hand_aims_along_its_fingers_and_pinches_from_its_joints() {
        let (aim, select, normal) = from_joints(Hand::Right, &right_hand(true, 0.05)).expect("valid joints");
        let p = Pointer { aim, source: AimSource::Joints, select, pressed: false, clicks: 0, palm_up: false };
        let d = p.direction();
        assert!(d[2] < -0.99, "points forward: {d:?}");
        assert!(normal[1] < -0.99, "a palm-down right hand's palm faces down: {normal:?}");
        assert_eq!(select, 0.0, "5 cm apart: no pinch");
        assert_eq!(from_joints(Hand::Right, &right_hand(true, 0.01)).unwrap().1, 1.0, "1 cm: a full pinch");
        // A joint not located: no pointer from the joints.
        let mut j = right_hand(true, 0.05);
        j.0[joint::WRIST].valid = false;
        assert!(from_joints(Hand::Right, &j).is_none());
    }

    #[test]
    fn a_held_palm_up_pinch_toggles_once_never_clicks_and_never_while_holding() {
        let up = right_hand(false, 0.01);
        assert!(from_joints(Hand::Right, &up).unwrap().2[1] > 0.99, "turned over: the palm faces up");
        let none = PointerReading::default();
        // Off by default: a palm-up pinch is then an ordinary pinch, which clicks and never toggles.
        let mut off = PointerState::default();
        let mut clicked = false;
        for t in (0..=1000).step_by(10) {
            let st = off.step(Hand::Right, ms(t), none, Some(&up), false);
            assert!(!st.menu_toggled && !st.pointer.unwrap().palm_up);
            clicked |= st.pointer.unwrap().clicks > 0;
        }
        assert!(clicked);
        let mut s = PointerState::with_palm_menu();
        let mut toggles = 0;
        for t in (0..=1000).step_by(10) {
            let st = s.step(Hand::Right, ms(t), none, Some(&up), false);
            let p = st.pointer.expect("aimed from the joints");
            assert!(p.palm_up && p.pressed);
            assert_eq!(p.clicks, 0, "the menu gesture never clicks");
            toggles += st.menu_toggled as u32;
            if st.menu_toggled {
                assert!(t >= 400, "only after the hold: {t}");
            }
        }
        assert_eq!(toggles, 1, "one toggle per gesture");
        // Released and pinched again: a second toggle.
        let open = right_hand(false, 0.05);
        s.step(Hand::Right, ms(1100), none, Some(&open), false);
        let fired: u32 =
            (0..=50).map(|k| s.step(Hand::Right, ms(1200 + k * 10), none, Some(&up), false).menu_toggled as u32).sum();
        assert_eq!(fired, 1);
        // While the hand holds something, the gesture does nothing.
        let mut h = PointerState::with_palm_menu();
        assert!((0..=100).all(|k| !h.step(Hand::Right, ms(k * 10), none, Some(&up), true).menu_toggled));
    }

    #[test]
    fn a_palm_down_pinch_clicks() {
        let mut s = PointerState::default();
        let none = PointerReading::default();
        s.step(Hand::Right, ms(0), none, Some(&right_hand(true, 0.05)), false);
        let p = s.step(Hand::Right, ms(10), none, Some(&right_hand(true, 0.01)), false).pointer.unwrap();
        assert!(p.pressed && !p.palm_up && p.clicks == 1);
    }

    #[test]
    fn rotate_and_look_rotation_agree() {
        for f in [[0.0, 0.0, -1.0], [1.0, 0.0, 0.0], [0.0, -0.6, -0.8], [-0.3, 0.2, 0.9]] {
            let f = normalize(f).unwrap();
            let q = look_rotation(f, [0.0, 1.0, 0.0]).unwrap();
            let d = rotate(q, [0.0, 0.0, -1.0]);
            assert!(len(sub(d, f)) < 1e-5, "{f:?} -> {d:?}");
        }
    }
}
