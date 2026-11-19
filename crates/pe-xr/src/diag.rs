//! The opt-in diagnostic log (spec 10.3): with `PE_XR_LOG=1` in the environment, pe-xr writes a few lines to
//! standard error about a real-runtime run, whichever application drives the session: the runtime's name and
//! version with the API versions negotiated and used, the reference space at creation, each session state change, the first drawn frame's eyes and display period, the first tracked pose of
//! each hand, and the end reason with the counters at teardown. It is for headset checks, where only the user sees
//! the frame: the lines say where the runtime put the floor, the head and the hands. Off by default; it never changes
//! what the session does.

use std::sync::OnceLock;

use pe_seams::{Hand, RigPose};

use crate::backend::{EyePose, SessionState};
use crate::hands::{GraspEdge, Reading};
use crate::seam::HandSource;
use crate::session::{Counters, EndReason};

/// The environment variable that turns the log on when it is `1`.
pub const LOG_ENV: &str = "PE_XR_LOG";

/// Whether this process logs, read once from [`LOG_ENV`].
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var(LOG_ENV).as_deref() == Ok("1"))
}

/// Where [`emit`] sends its lines instead of standard error, once an application sets it ([`set_sink`]).
static SINK: OnceLock<fn(&str)> = OnceLock::new();

/// Sends this process's lines to `sink` instead of standard error: pe-app's `--log` writes them to its log as well as
/// the console. Once per process; false if a sink was already set.
pub fn set_sink(sink: fn(&str)) -> bool {
    SINK.set(sink).is_ok()
}

/// Write one line to standard error (or the sink, [`set_sink`]), prefixed `pe-xr:`.
pub fn emit(line: &str) {
    let text = format!("pe-xr: {line}");
    match SINK.get() {
        Some(sink) => sink(&text),
        None => eprintln!("{text}"),
    }
}

/// What a session has logged so far: each first is logged once. Its methods return the line to write, or `None`
/// when off or already logged, so the session writes them and tests read them.
#[derive(Debug, Default)]
pub struct Diag {
    on: bool,
    eyes_logged: bool,
    hand_logged: [bool; 2],
    /// Each hand's pose source in the last FOCUSED frame, to log its changes.
    source: [Option<PoseSource>; 2],
}

/// Where a hand's pose came from in one frame (spec 5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoseSource {
    /// Hand tracking (XR_EXT_hand_tracking): the hand's joints are drawn. The pose itself comes from the joints, or,
    /// with XR_EXT_hand_interaction, from the grip action space.
    Tracked,
    /// A controller's grip pose.
    Controller,
    /// No valid pose.
    None,
}

impl PoseSource {
    pub fn of(r: &Reading) -> PoseSource {
        match (r.pose, r.source) {
            (None, _) => PoseSource::None,
            (Some(_), HandSource::Tracked) => PoseSource::Tracked,
            (Some(_), _) => PoseSource::Controller,
        }
    }
}

impl Diag {
    pub fn new(on: bool) -> Self {
        Diag { on, ..Diag::default() }
    }

    /// From the environment ([`enabled`]).
    pub fn from_env() -> Self {
        Diag::new(enabled())
    }

    pub fn state(&self, from: SessionState, to: SessionState, at_ns: i64) -> Option<String> {
        self.on.then(|| format!("state {from:?} -> {to:?} at {at_ns} ns"))
    }

    /// The first drawn frame's eyes (rig frame) and the runtime's display period.
    pub fn first_eyes(&mut self, eyes: &[EyePose; 2], period_ns: i64) -> Option<String> {
        if !self.on || self.eyes_logged {
            return None;
        }
        self.eyes_logged = true;
        let mut s = format!("first drawn frame: display period {period_ns} ns");
        for (i, e) in eyes.iter().enumerate() {
            s.push_str(&format!(
                "; eye {i} position {:?} orientation {:?} fov {:?}",
                e.pose.position, e.pose.orientation, e.fov
            ));
        }
        Some(s)
    }

    /// The first tracked pose (rig frame) of each hand, as drawn.
    pub fn first_hands(&mut self, hands: &[Option<RigPose>; 2]) -> Vec<String> {
        let mut out = Vec::new();
        if !self.on {
            return out;
        }
        for h in Hand::BOTH {
            let i = h.index();
            if let (Some(p), false) = (hands[i], self.hand_logged[i]) {
                self.hand_logged[i] = true;
                out.push(format!(
                    "first tracked {h:?} hand: position {:?} orientation {:?}",
                    p.position, p.orientation
                ));
            }
        }
        out
    }

    /// A hand's pose source, when it changes (the first FOCUSED frame included): controllers put down for bare hands,
    /// or picked up again.
    pub fn hand_source(&mut self, hand: Hand, source: PoseSource, at_ns: i64) -> Option<String> {
        let last = self.source[hand.index()].replace(source);
        (self.on && last != Some(source))
            .then(|| format!("{hand:?} hand pose from {source:?} (was {last:?}) at {at_ns} ns"))
    }

    /// A close or an open of a hand's grasp, with both signals and which one decided it.
    pub fn grasp_edge(&self, hand: Hand, e: &GraspEdge, at_ns: i64) -> Option<String> {
        self.on.then(|| {
            format!(
                "{hand:?} hand {} by the {:?} at {at_ns} ns: squeeze {} ({}), curl {}",
                if e.closed { "closed" } else { "opened" },
                e.by,
                e.squeeze.map_or("-".into(), |v| format!("{v:.2}")),
                if e.squeeze.is_some() { "action active" } else { "action inactive" },
                e.curl.map_or("-".into(), |v| format!("{v:.2}")),
            )
        })
    }

    /// Each hand's current interaction profile, `why` saying when it was read.
    pub fn profiles(&self, why: &str, profiles: &[String; 2]) -> Option<String> {
        self.on.then(|| format!("interaction profiles ({why}): Left {}, Right {}", profiles[0], profiles[1]))
    }

    /// Each hand's largest squeeze and curl over the session, at teardown.
    pub fn grip_maxima(&self, maxima: &[(Option<f32>, Option<f32>); 2]) -> Option<String> {
        let f = |v: Option<f32>| v.map_or("never read".to_string(), |v| format!("{v:.2}"));
        self.on.then(|| {
            format!(
                "grip maxima: Left squeeze {}, curl {}; Right squeeze {}, curl {}",
                f(maxima[0].0),
                f(maxima[0].1),
                f(maxima[1].0),
                f(maxima[1].1)
            )
        })
    }

    pub fn ended(&self, why: Option<EndReason>, counters: &Counters) -> Option<String> {
        self.on.then(|| {
            let never = if self.hand_logged == [false, false] { "; no hand was ever tracked while drawn" } else { "" };
            format!("session torn down: {why:?}{never}; {counters:?}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Fov;

    fn eye(x: f32) -> EyePose {
        EyePose {
            pose: RigPose { position: [x, 1.6, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
            fov: Fov { left: -0.8, right: 0.8, up: 0.8, down: -0.8 },
        }
    }

    #[test]
    fn off_logs_nothing() {
        let mut d = Diag::new(false);
        assert!(d.state(SessionState::Idle, SessionState::Ready, 1).is_none());
        assert!(d.first_eyes(&[eye(-0.03), eye(0.03)], 11_111_111).is_none());
        let p = RigPose { position: [0.2, 1.2, -0.4], orientation: [0.0, 0.0, 0.0, 1.0] };
        assert!(d.first_hands(&[Some(p), Some(p)]).is_empty());
        assert!(d.ended(Some(EndReason::Exited), &Counters::default()).is_none());
    }

    #[test]
    fn grasp_edges_profiles_and_maxima_are_logged_only_when_on() {
        use crate::hands::{GraspEdge, GripSignal};
        let e = GraspEdge { closed: true, by: GripSignal::Squeeze, squeeze: Some(0.9), curl: Some(0.45) };
        let profiles = ["/interaction_profiles/oculus/touch_controller".to_string(), "none".to_string()];
        let maxima = [(Some(0.9), Some(0.5)), (None, Some(0.3))];
        let off = Diag::new(false);
        assert!(off.grasp_edge(Hand::Left, &e, 5).is_none());
        assert!(off.profiles("changed", &profiles).is_none());
        assert!(off.grip_maxima(&maxima).is_none());
        let on = Diag::new(true);
        assert_eq!(
            on.grasp_edge(Hand::Left, &e, 5).unwrap(),
            "Left hand closed by the Squeeze at 5 ns: squeeze 0.90 (action active), curl 0.45"
        );
        let open = GraspEdge { closed: false, by: GripSignal::Curl, squeeze: None, curl: Some(0.2) };
        assert_eq!(
            on.grasp_edge(Hand::Right, &open, 6).unwrap(),
            "Right hand opened by the Curl at 6 ns: squeeze - (action inactive), curl 0.20"
        );
        assert_eq!(
            on.profiles("changed", &profiles).unwrap(),
            "interaction profiles (changed): Left /interaction_profiles/oculus/touch_controller, Right none"
        );
        assert_eq!(
            on.grip_maxima(&maxima).unwrap(),
            "grip maxima: Left squeeze 0.90, curl 0.50; Right squeeze never read, curl 0.30"
        );
    }

    #[test]
    fn each_first_is_logged_once() {
        let mut d = Diag::new(true);
        let first = d.first_eyes(&[eye(-0.03), eye(0.03)], 11_111_111).expect("the first frame is logged");
        assert!(first.contains("11111111 ns") && first.contains("eye 1 position [0.03, 1.6, 0.0]"), "{first}");
        assert!(d.first_eyes(&[eye(-0.03), eye(0.03)], 11_111_111).is_none(), "only the first frame");

        let p = RigPose { position: [0.2, 1.2, -0.4], orientation: [0.0, 0.0, 0.0, 1.0] };
        assert_eq!(d.first_hands(&[None, None]).len(), 0);
        let right = d.first_hands(&[None, Some(p)]);
        assert_eq!(right.len(), 1);
        assert!(right[0].contains("Right") && right[0].contains("[0.2, 1.2, -0.4]"), "{}", right[0]);
        let both = d.first_hands(&[Some(p), Some(p)]);
        assert_eq!(both.len(), 1, "the right hand was already logged");
        assert!(both[0].contains("Left"));
        assert!(d.first_hands(&[Some(p), Some(p)]).is_empty());

        let end = d.ended(Some(EndReason::Exited), &Counters::default()).expect("logged");
        assert!(end.contains("Exited") && !end.contains("no hand"), "{end}");
    }

    #[test]
    fn a_hands_pose_source_is_logged_when_it_changes() {
        let mut d = Diag::new(true);
        let first = d.hand_source(Hand::Left, PoseSource::Controller, 1).expect("the first source is logged");
        assert!(first.contains("Left hand pose from Controller (was None)"), "{first}");
        assert!(d.hand_source(Hand::Left, PoseSource::Controller, 2).is_none(), "unchanged");
        let put_down = d.hand_source(Hand::Left, PoseSource::Tracked, 3).expect("controllers put down");
        assert!(put_down.contains("from Tracked (was Some(Controller)) at 3 ns"), "{put_down}");
        assert!(d.hand_source(Hand::Right, PoseSource::Tracked, 4).is_some(), "each hand on its own");
        assert!(Diag::new(false).hand_source(Hand::Left, PoseSource::Tracked, 5).is_none(), "off logs nothing");
    }

    #[test]
    fn the_end_says_when_no_hand_was_tracked() {
        let d = Diag::new(true);
        let end = d.ended(Some(EndReason::Exited), &Counters::default()).expect("logged");
        assert!(end.contains("no hand was ever tracked"), "{end}");
    }
}
