//! Time: the drawn time `t_draw` (X8) and resolving an object's simulated pose to it (render.md 7).
//!
//! Frame views arrive at the simulation rate, a world setting never tied to the display. Each display frame picks
//! a drawn time by the configured rule and resolves every object to it:
//! - **(a) extrapolate** (default): `t_draw = display_time`; each object is moved from the newest frame view by its
//!   velocity, over a span capped at `l_max`;
//! - **(b) interpolate late:** `t_draw = display_time − d_render`, between the two frame views around it. Past the
//!   newest view, the newest pose is held: nothing is drawn from a guess.
//!
//! Interpolation is never counted as a correction (D8); only a change of prediction is (the display layer, D2).

use core::time::Duration;

use glam::DQuat;
use pe_seams::{FrameView, HostTime, ObjectId, ViewObject};

use crate::pose::{WorldPose, offset_vec, vec};

/// The drawn-time rule (X8).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DrawnTime {
    /// (a): draw at the display time, moving each object from the newest frame view by its velocity, for at most
    /// `l_max` (the measured display lead plus one simulation frame).
    Extrapolate { l_max: Duration },
    /// (b): draw `d_render` before the display time, between the two frame views around it.
    InterpolateLate { d_render: Duration },
}

impl Default for DrawnTime {
    /// (a), with `l_max` at Virtual Desktop's measured p95 lead (63 ms [V M0]) plus one 60 Hz frame. Tuned at the
    /// headset test (render.md R1).
    fn default() -> Self {
        DrawnTime::Extrapolate { l_max: Duration::from_millis(80) }
    }
}

impl DrawnTime {
    /// The drawn time for a display frame shown at `display_time`.
    pub fn t_draw(&self, display_time: HostTime) -> HostTime {
        match *self {
            DrawnTime::Extrapolate { .. } => display_time,
            DrawnTime::InterpolateLate { d_render } => {
                HostTime(display_time.0.saturating_sub(i64::try_from(d_render.as_nanos()).unwrap_or(i64::MAX)))
            }
        }
    }
}

/// How a pose was resolved, for the counters (render.md 7, 13).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Resolved {
    pub pose: Option<WorldPose>,
    /// The extrapolation span actually used, in seconds (negative when the drawn time is before the newest view).
    pub span: f64,
}

/// One object's pose at `t_draw` under `rule`, from `history` (frame views, oldest first, newest last). `None` if
/// the object is not in the newest view.
///
/// `pose_of` gives an object's pose in a view: normally [`WorldPose::of`]; the display layer substitutes the old
/// timeline's continuation for a corrected view under (b), so a correction blends instead of showing as one
/// simulation period of fast motion (render.md 8, D2).
pub fn resolve(
    rule: &DrawnTime,
    history: &[&FrameView],
    id: ObjectId,
    t_draw: HostTime,
    pose_of: &dyn Fn(&FrameView, &ViewObject) -> WorldPose,
) -> Resolved {
    let Some(newest) = history.last() else { return Resolved::default() };
    let Some(n) = newest.object(id) else { return Resolved::default() };
    match *rule {
        DrawnTime::Extrapolate { l_max } => {
            let cap = l_max.as_secs_f64();
            let span = t_draw.secs_since(newest.sim_time).clamp(-cap, cap);
            Resolved { pose: Some(extrapolate(n, span)), span }
        }
        DrawnTime::InterpolateLate { .. } => {
            // The newest view at or before t_draw; past the newest, hold it.
            let Some(k) = history.iter().rposition(|v| v.sim_time <= t_draw) else {
                let first = history.iter().find_map(|v| v.object(id).map(|o| pose_of(v, o)));
                return Resolved { pose: first, span: 0.0 };
            };
            if k + 1 == history.len() {
                return Resolved { pose: Some(pose_of(newest, n)), span: 0.0 };
            }
            let (va, vb) = (history[k], history[k + 1]);
            let Some(ob) = vb.object(id) else { return Resolved { pose: Some(pose_of(newest, n)), span: 0.0 } };
            let b = pose_of(vb, ob);
            let Some(oa) = va.object(id) else { return Resolved { pose: Some(b), span: 0.0 } };
            let a = pose_of(va, oa);
            let whole = vb.sim_time.secs_since(va.sim_time);
            if whole <= 0.0 {
                return Resolved { pose: Some(b), span: 0.0 };
            }
            let s = (t_draw.secs_since(va.sim_time) / whole).clamp(0.0, 1.0);
            let d = offset_vec(b.pos - a.pos) * s;
            Resolved { pose: Some(a.moved(d).with_rot(a.rot.slerp(b.rot, s).normalize())), span: 0.0 }
        }
    }
}

/// `o` moved by its velocities over `dt` seconds. Angular velocity is in world axes, rad/s.
pub fn extrapolate(o: &ViewObject, dt: f64) -> WorldPose {
    let base = WorldPose::of(o);
    let w = vec(o.angular_velocity) * dt;
    let rot = if w.length_squared() > 0.0 { (DQuat::from_scaled_axis(w) * base.rot).normalize() } else { base.rot };
    base.moved(vec(o.linear_velocity) * dt).with_rot(rot)
}
