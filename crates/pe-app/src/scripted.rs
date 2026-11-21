//! Scripted hands (the pull-ahead plan's T1): hand moves played from a named routine or a file, so a self-test can
//! sweep, grab, lift and throw with no person at the machine. They stand in for the desktop hand, in a window or
//! headless, and for A's VR hands in a desktop run. They never replace VR's ring while an XR session runs.
//!
//! A script is one line per step, `<left|right> <step>`, each hand's steps played in order and both hands at once
//! (`#` starts a comment):
//!
//! | step | what the hand does |
//! |---|---|
//! | `rest <s>` | moves to its rest pose, a little in front of the rig, over `s` seconds |
//! | `move <x> <y> <z> <s>` | moves to a rig-frame point (metres, +Y up, -Z forward) over `s` seconds |
//! | `reach <top\|first\|pile> <s>` | moves to a ball's centre over `s` seconds, following the ball as it moves |
//! | `lift <dy> <s>` | rises `dy` metres over `s` seconds |
//! | `swing <vx> <vy> <vz> <s>` | moves at that rig-frame velocity for `s` seconds |
//! | `wait <s>` | holds still |
//! | `close`, `open` | the grasp: a close grabs what is in reach, an open lets go (app.md 3.1, step 2) |
//! | `drop` | the hand stops being tracked (an absent hand sends nothing, xr.md 5.5), through any `wait`s after it; the next other step brings it back where it was |
//!
//! The balls: `top` is the highest ball, `first` the one with the lowest id (every member picks the same ball, so two
//! members running `share` take hold of one ball), and `pile` the balls' centre across, 0.25 m above the lowest ball's centre. A
//! target is chosen from the newest view when its step starts, and the hand follows it until the step ends. A hand
//! with no steps is absent. After its last step a hand stays where it ended, as it ended.
//!
//! The named routines are scripts in this format: [`ROUTINES`]. pe-app plays one from `--hands <routine|file>`, or
//! when `hands <routine|file>` is typed into its console; `hands stop` hands the hands back to the desktop. Each step
//! that starts says so in the log (`hands: right reach top: ball 0x.. at ...`), so a checker can tie the world's grabs
//! and throws to the script.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use pe_render::present::FrameSlot;
use pe_seams::{
    FrameView, Hand, HandFeed, HandInput, HandSource, HostTime, ObjectClass, ObjectId, Pos, RigPose, Shape,
};

/// The routines `--hands` and the console's `hands` know by name.
pub const ROUTINES: &[(&str, &str)] = &[
    ("idle", "right rest 0.5\n"),
    // Through the pile at ball height, side to side three times, open.
    (
        "sweep",
        "right reach pile 1.0\nright swing 1.5 0 0 0.6\nright swing -1.5 0 0 1.2\nright swing 1.5 0 0 1.2\n\
         right swing -1.5 0 0 0.6\nright rest 1.0\n",
    ),
    // The top ball: take it, lift it 0.6 m, hold it 2 s, let it fall. The hand stays still for 0.5 s after it opens:
    // a release carries the hand's velocity just after the opening (local.rs `throw_at`), so a hand that moved away
    // at once would throw the ball.
    (
        "grab",
        "right reach top 1.0\nright wait 0.2\nright close\nright wait 0.3\nright lift 0.6 1.0\nright wait 2.0\nright open\nright wait 0.5\nright rest 1.0\n",
    ),
    // The top ball, lifted and thrown forward and up at about 4.5 m/s; the swing goes on after the open, as a
    // throwing arm does, since the release carries the hand's velocity just after the opening.
    (
        "throw",
        "right reach top 1.0\nright wait 0.2\nright close\nright wait 0.3\nright lift 0.4 0.6\nright swing 0 2 -4 0.25\n\
         right open\nright swing 0 2 -4 0.1\nright rest 1.0\n",
    ),
    // Both members on one ball: the lowest-id ball, taken, lifted 0.3 m and held 4 s. Run it on each member at once.
    (
        "share",
        "right reach first 1.5\nright wait 0.2\nright close\nright wait 0.3\nright lift 0.3 1.0\nright wait 4.0\nright open\nright wait 0.5\nright rest 1.0\n",
    ),
];

/// The scripted hands the console's `hands` command drives, once pe-app has set them up.
static CONSOLE: OnceLock<Arc<ScriptedHands>> = OnceLock::new();

/// Sets the hands the console's `hands` command drives (pe-app's main, once).
pub fn install(h: Arc<ScriptedHands>) {
    let _ = CONSOLE.set(h);
}

/// The hands the console drives, if pe-app has set them up.
pub fn installed() -> Option<Arc<ScriptedHands>> {
    CONSOLE.get().cloned()
}

/// Where a hand rests: in front of the rig, a little to its side, at about chest height (rig frame).
pub const REST: [[f32; 3]; 2] = [[-0.25, 1.1, -0.35], [0.25, 1.1, -0.35]];

/// A ball a `reach` aims at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ball {
    Top,
    First,
    Pile,
}

/// One step of one hand.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Rest(f64),
    Move([f32; 3], f64),
    Reach(Ball, f64),
    Lift(f32, f64),
    Swing([f32; 3], f64),
    Wait(f64),
    Close,
    Open,
    Drop,
}

impl Step {
    fn secs(&self) -> f64 {
        match *self {
            Step::Rest(s)
            | Step::Move(_, s)
            | Step::Reach(_, s)
            | Step::Lift(_, s)
            | Step::Swing(_, s)
            | Step::Wait(s) => s,
            Step::Close | Step::Open | Step::Drop => 0.0,
        }
    }
}

/// A script: each hand's steps, left then right.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Script {
    pub name: String,
    pub hands: [Vec<Step>; 2],
}

/// The script a routine's name or a file's path gives: a name in [`ROUTINES`] first, then a file.
pub fn load(what: &str) -> Result<Script, String> {
    if let Some((name, text)) = ROUTINES.iter().find(|(n, _)| *n == what) {
        return parse(name, text);
    }
    let text = std::fs::read_to_string(what).map_err(|e| {
        let names: Vec<&str> = ROUTINES.iter().map(|(n, _)| *n).collect();
        format!("`{what}` is neither a routine ({}) nor a readable file: {e}", names.join(", "))
    })?;
    parse(what, &text)
}

/// Reads a script (the module's format).
pub fn parse(name: &str, text: &str) -> Result<Script, String> {
    let mut s = Script { name: name.to_string(), ..Script::default() };
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let bad = |why: &str| format!("{name} line {}: `{line}`: {why}", n + 1);
        let w: Vec<&str> = line.split_whitespace().collect();
        let hand = match w[0] {
            "left" => 0,
            "right" => 1,
            _ => return Err(bad("a line starts with `left` or `right`")),
        };
        let num = |i: usize| -> Result<f64, String> {
            let v: f64 =
                w.get(i).ok_or_else(|| bad("a number is missing"))?.parse().map_err(|_| bad("not a number"))?;
            if v.is_finite() { Ok(v) } else { Err(bad("not a finite number")) }
        };
        let secs = |i: usize| -> Result<f64, String> {
            let v = num(i)?;
            if (0.0..=3600.0).contains(&v) { Ok(v) } else { Err(bad("seconds run from 0 to 3600")) }
        };
        let v3 =
            |i: usize| -> Result<[f32; 3], String> { Ok([num(i)? as f32, num(i + 1)? as f32, num(i + 2)? as f32]) };
        let (step, words) = match *w.get(1).ok_or_else(|| bad("the step is missing"))? {
            "rest" => (Step::Rest(secs(2)?), 3),
            "move" => (Step::Move(v3(2)?, secs(5)?), 6),
            "reach" => {
                let ball = match w.get(2).copied() {
                    Some("top") => Ball::Top,
                    Some("first") => Ball::First,
                    Some("pile") => Ball::Pile,
                    _ => return Err(bad("reach takes `top`, `first` or `pile`")),
                };
                (Step::Reach(ball, secs(3)?), 4)
            }
            "lift" => (Step::Lift(num(2)? as f32, secs(3)?), 4),
            "swing" => (Step::Swing(v3(2)?, secs(5)?), 6),
            "wait" => (Step::Wait(secs(2)?), 3),
            "close" => (Step::Close, 2),
            "open" => (Step::Open, 2),
            "drop" => (Step::Drop, 2),
            _ => return Err(bad("unknown step")),
        };
        if w.len() != words {
            return Err(bad(&format!("this step takes {} words", words)));
        }
        s.hands[hand].push(step);
    }
    if s.hands.iter().all(Vec::is_empty) {
        return Err(format!("{name}: no steps"));
    }
    Ok(s)
}

/// A hand's state between steps.
#[derive(Clone, Copy, Debug, PartialEq)]
struct HandState {
    at: [f32; 3],
    closed: bool,
    tracked: bool,
}

/// A step as it was started: where the hand was, and what a `reach` chose.
#[derive(Clone, Copy, Debug)]
struct Started {
    from: HandState,
    ball: Option<ObjectId>,
    /// A `reach pile`'s point, chosen once.
    point: Option<[f32; 3]>,
}

/// One hand's play of its steps.
#[derive(Debug, Default)]
struct Track {
    steps: Vec<Step>,
    /// The steps started so far, in order, each with its start time (seconds into the script).
    started: Vec<(f64, Started)>,
    /// Where the last started step ended, once its time has passed.
    ended: Option<HandState>,
    /// The hand's discontinuity counter: new at the script's start, and again when the hand comes back after a `drop`,
    /// so neither jump is read as motion or a throw.
    discontinuity: u32,
}

/// A script being played.
struct Play {
    script: Script,
    /// The host time of the first read after the script was set: its time 0.
    t0: Option<HostTime>,
    tracks: [Track; 2],
}

/// The scripted hands over the desktop hand: while a script plays, its hands; otherwise the desktop's.
pub struct ScriptedHands {
    slot: Arc<FrameSlot>,
    desktop: Arc<dyn HandFeed>,
    play: Mutex<Option<Play>>,
    /// The last discontinuity counter handed out (`Track::discontinuity`).
    bumps: AtomicU32,
}

impl ScriptedHands {
    pub fn new(slot: Arc<FrameSlot>, desktop: Arc<dyn HandFeed>) -> ScriptedHands {
        ScriptedHands { slot, desktop, play: Mutex::new(None), bumps: AtomicU32::new(0) }
    }

    /// Plays `script` from the next read on, replacing any script playing.
    pub fn start(&self, script: Script) {
        let n = self.bumps.fetch_add(1, Ordering::Relaxed) + 1;
        let tracks = [0, 1].map(|i| Track { steps: script.hands[i].clone(), discontinuity: n, ..Track::default() });
        crate::out!(
            "hands: playing `{}` ({} left steps, {} right)",
            script.name,
            tracks[0].steps.len(),
            tracks[1].steps.len()
        );
        *self.play.lock().expect("the play") = Some(Play { script, t0: None, tracks });
    }

    /// Stops the script: the desktop hand answers again.
    pub fn stop(&self) {
        if self.play.lock().expect("the play").take().is_some() {
            crate::out!("hands: stopped; the desktop hand is back");
        }
    }

    /// The console's `hands` command: `hands <routine|file>` or `hands stop`. Returns what to say.
    pub fn on_command(&self, arg: &str) -> Result<(), String> {
        match arg.trim() {
            "" => Err("`hands` takes a routine, a script file or `stop`".to_string()),
            "stop" => {
                self.stop();
                Ok(())
            }
            what => load(what).map(|s| self.start(s)),
        }
    }

    /// Whether a script is set (playing or finished).
    pub fn playing(&self) -> bool {
        self.play.lock().expect("the play").is_some()
    }
}

impl HandFeed for ScriptedHands {
    fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput> {
        let mut guard = self.play.lock().expect("the play");
        let Some(play) = guard.as_mut() else {
            drop(guard);
            return self.desktop.hand_at(hand, at);
        };
        let t0 = *play.t0.get_or_insert(at);
        let t = (at.0 - t0.0) as f64 / 1e9;
        let i = usize::from(hand == Hand::Right);
        let view = self.slot.newest();
        let name = play.script.name.clone();
        let (state, velocity) = play.tracks[i].at(i, t.max(0.0), view.as_deref(), &name, &self.bumps)?;
        state.tracked.then_some(HandInput {
            hand,
            source: HandSource::Desktop,
            sampled_at: at,
            tracked: true,
            pose: RigPose { position: state.at, orientation: [0.0, 0.0, 0.0, 1.0] },
            linear_velocity: Some(velocity),
            angular_velocity: Some([0.0; 3]),
            grip: if state.closed { 1.0 } else { 0.0 },
            closed: state.closed,
            discontinuity: play.tracks[i].discontinuity,
        })
    }
}

impl Track {
    /// The hand's state and velocity `t` seconds into the script; `None` while it has no steps. Starts each step
    /// whose time has come, in order, choosing its ball from `view`.
    fn at(
        &mut self,
        side: usize,
        t: f64,
        view: Option<&FrameView>,
        script: &str,
        bumps: &AtomicU32,
    ) -> Option<(HandState, [f32; 3])> {
        if self.steps.is_empty() {
            return None;
        }
        let rest = HandState { at: REST[side], closed: false, tracked: true };
        // Start every step whose time has come.
        loop {
            let k = self.started.len();
            let (start, from) = match self.started.last() {
                None => (0.0, rest),
                Some(&(s0, st)) => {
                    let step = self.steps[k - 1];
                    let end = s0 + step.secs();
                    if k == self.steps.len() || t < end {
                        break;
                    }
                    let ended = self.ended.unwrap_or_else(|| eval(step, &st, end - s0, view, side).0);
                    (end, ended)
                }
            };
            if k == self.steps.len() {
                break;
            }
            let step = self.steps[k];
            // A hand that comes back after a `drop` is tracked again, with a new discontinuity: it jumps, it didn't move.
            let from = if !from.tracked && !matches!(step, Step::Drop | Step::Wait(_)) {
                self.discontinuity = bumps.fetch_add(1, Ordering::Relaxed) + 1;
                HandState { tracked: true, ..from }
            } else {
                from
            };
            let st = begin(step, from, view);
            log_start(script, side, step, &st, view);
            self.started.push((start, st));
            self.ended = None;
        }
        let &(s0, st) = self.started.last()?;
        let step = self.steps[self.started.len() - 1];
        let (state, v) = eval(step, &st, (t - s0).min(step.secs()), view, side);
        if t - s0 >= step.secs() {
            self.ended.get_or_insert(state);
            return Some((self.ended.unwrap_or(state), [0.0; 3]));
        }
        Some((state, v))
    }
}

/// A step's start: its ball, chosen from `view`.
fn begin(step: Step, from: HandState, view: Option<&FrameView>) -> Started {
    match step {
        Step::Reach(Ball::Pile, _) => Started { from, ball: None, point: view.and_then(pile) },
        Step::Reach(which, _) => Started { from, ball: view.and_then(|v| pick(v, which)), point: None },
        _ => Started { from, ball: None, point: None },
    }
}

/// A step `s` seconds in (clamped to its length): the hand's state and its velocity.
fn eval(step: Step, st: &Started, s: f64, view: Option<&FrameView>, side: usize) -> (HandState, [f32; 3]) {
    let from = st.from;
    let along = |to: [f32; 3], secs: f64| -> (HandState, [f32; 3]) {
        let f = if secs <= 0.0 { 1.0 } else { (s / secs).clamp(0.0, 1.0) as f32 };
        let at = [0, 1, 2].map(|i| from.at[i] + (to[i] - from.at[i]) * f);
        let v = if secs <= 0.0 { [0.0; 3] } else { [0, 1, 2].map(|i| (to[i] - from.at[i]) / secs as f32) };
        (HandState { at, ..from }, v)
    };
    match step {
        Step::Rest(secs) => along(REST[side], secs),
        Step::Move(to, secs) => along(to, secs),
        Step::Reach(_, secs) => {
            let to = st.point.or_else(|| st.ball.and_then(|b| view.and_then(|v| rig_of(v, b)))).unwrap_or(from.at);
            along(to, secs)
        }
        Step::Lift(dy, secs) => along([from.at[0], from.at[1] + dy, from.at[2]], secs),
        Step::Swing(v, _) => {
            // A read earlier than the step's start (the input lead, a throw's samples) holds at the start.
            let k = s.max(0.0) as f32;
            (HandState { at: [0, 1, 2].map(|i| from.at[i] + v[i] * k), ..from }, v)
        }
        Step::Wait(_) => (from, [0.0; 3]),
        Step::Close => (HandState { closed: true, ..from }, [0.0; 3]),
        Step::Open => (HandState { closed: false, ..from }, [0.0; 3]),
        Step::Drop => (HandState { tracked: false, ..from }, [0.0; 3]),
    }
}

/// The view's balls: shared spheres that aren't a member's hand.
fn balls(v: &FrameView) -> impl Iterator<Item = &pe_seams::ViewObject> {
    v.objects.iter().filter(|o| {
        o.class == ObjectClass::World && o.hand_of.is_none() && matches!(o.shape, Some(Shape::Sphere { .. }))
    })
}

/// The ball `which` names in `v`.
fn pick(v: &FrameView, which: Ball) -> Option<ObjectId> {
    let height = |o: &pe_seams::ViewObject| (o.pos - Pos::ORIGIN).0[1];
    match which {
        Ball::Top => balls(v).max_by(|a, b| height(a).total_cmp(&height(b))).map(|o| o.id),
        Ball::First => balls(v).min_by_key(|o| o.id.to_u128()).map(|o| o.id),
        Ball::Pile => None,
    }
}

/// The balls' centre in the rig frame, 0.25 m above the lowest ball's centre.
fn pile(v: &FrameView) -> Option<[f32; 3]> {
    let (mut sum, mut low, mut n) = ([0.0f64; 3], f64::INFINITY, 0usize);
    for o in balls(v) {
        let p = (o.pos - Pos::ORIGIN).0;
        (0..3).for_each(|i| sum[i] += p[i]);
        low = low.min(p[1]);
        n += 1;
    }
    if n == 0 {
        return None;
    }
    let world = [sum[0] / n as f64, low + 0.25, sum[2] / n as f64];
    Some(to_rig(v, world))
}

/// `o`'s centre in the local rig's frame.
fn rig_of(v: &FrameView, o: ObjectId) -> Option<[f32; 3]> {
    let p = v.objects.iter().find(|x| x.id == o)?.pos;
    Some(to_rig(v, (p - Pos::ORIGIN).0))
}

/// A world point (absolute metres) in the local rig's frame: the inverse of local.rs's `world_pose`.
fn to_rig(v: &FrameView, world: [f64; 3]) -> [f32; 3] {
    let rig = &v.local_rig;
    let o = (rig.pos - Pos::ORIGIN).0;
    let d = [0, 1, 2].map(|i| (world[i] - o[i]) as f32);
    let q = rig.rotation;
    let r = crate::local::rotate([-q[0], -q[1], -q[2], q[3]], d);
    let s = if rig.scale.abs() > 1e-6 { rig.scale } else { 1.0 };
    r.map(|c| c / s)
}

fn log_start(script: &str, side: usize, step: Step, st: &Started, view: Option<&FrameView>) {
    let hand = ["left", "right"][side];
    let target = match step {
        Step::Reach(Ball::Pile, _) => match st.point {
            Some(p) => format!(": the pile at {:.2?} (rig)", p),
            None => ": no balls in view, so the hand stays".to_string(),
        },
        Step::Reach(_, _) => match (st.ball, st.ball.and_then(|b| view.and_then(|v| rig_of(v, b)))) {
            (Some(b), Some(p)) => format!(": ball {:#034x} at {:.2?} (rig)", b.to_u128(), p),
            _ => ": no ball in view, so the hand stays".to_string(),
        },
        _ => String::new(),
    };
    crate::out!("hands: {script}: {hand} {step:?}{target}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_routine_parses() {
        for (name, _) in ROUTINES {
            let s = load(name).expect(name);
            assert!(!s.hands[1].is_empty(), "{name} moves the right hand");
        }
    }

    #[test]
    fn a_script_reads_each_step_and_refuses_what_it_cannot_read() {
        let s = parse("t", "# a comment\nright move 0 1 -0.5 2 # inline\nleft wait 1\nright close\nleft drop\n")
            .expect("parsed");
        assert_eq!(s.hands[1], [Step::Move([0.0, 1.0, -0.5], 2.0), Step::Close]);
        assert_eq!(s.hands[0], [Step::Wait(1.0), Step::Drop]);
        for bad in [
            "middle close",
            "right fly 1",
            "right move 0 1 2",
            "right wait -1",
            "right wait x",
            "right close now",
            "right reach bottom 1",
            "",
        ] {
            assert!(parse("t", bad).is_err(), "`{bad}` refused");
        }
    }

    /// A feed whose desktop hand is never asked while a script plays.
    struct Never;
    impl HandFeed for Never {
        fn hand_at(&self, _: Hand, _: HostTime) -> Option<HandInput> {
            None
        }
    }

    fn secs(s: f64) -> HostTime {
        HostTime(1_000_000_000 + (s * 1e9) as i64)
    }

    #[test]
    fn a_script_plays_its_steps_in_time_and_holds_its_last_pose() {
        let h = ScriptedHands::new(Arc::new(FrameSlot::new()), Arc::new(Never));
        assert!(h.hand_at(Hand::Right, secs(0.0)).is_none(), "no script: the desktop answers");
        h.start(
            parse("t", "right move 0.25 1.1 -1.35 1\nright close\nright lift 0.5 0.5\nright open\n").expect("parsed"),
        );
        let at = |s: f64| h.hand_at(Hand::Right, secs(s)).expect("tracked");
        let r0 = at(0.0);
        assert_eq!((r0.pose.position, r0.closed), (REST[1], false), "from rest");
        let mid = at(0.5);
        assert!((mid.pose.position[2] - -0.85).abs() < 1e-4, "half way: {:?}", mid.pose.position);
        assert!((mid.linear_velocity.expect("v")[2] - -1.0).abs() < 1e-4);
        let up = at(1.25);
        assert!(up.closed, "closed after the move");
        assert!((up.pose.position[1] - 1.35).abs() < 1e-4, "lifting: {:?}", up.pose.position);
        let end = at(5.0);
        assert!(!end.closed, "opened at the end");
        assert!((end.pose.position[1] - 1.6).abs() < 1e-4 && (end.pose.position[2] - -1.35).abs() < 1e-4);
        assert_eq!(end.linear_velocity, Some([0.0; 3]), "still after the last step");
        assert!(h.hand_at(Hand::Left, secs(1.0)).is_none(), "the left hand has no steps: absent");
        assert_eq!(end.discontinuity, 1, "a script's start is a discontinuity");
        h.stop();
        assert!(h.hand_at(Hand::Right, secs(6.0)).is_none(), "stopped: the desktop answers again");
    }

    #[test]
    fn a_dropped_hand_comes_back_with_a_new_discontinuity() {
        let h = ScriptedHands::new(Arc::new(FrameSlot::new()), Arc::new(Never));
        h.start(
            parse(
                "t",
                "right wait 1
right drop
right wait 1
right move 0 1 -1 1
",
            )
            .expect("parsed"),
        );
        let first = h.hand_at(Hand::Right, secs(0.0)).expect("tracked").discontinuity;
        assert!(h.hand_at(Hand::Right, secs(1.5)).is_none(), "dropped: absent");
        let back = h.hand_at(Hand::Right, secs(2.5)).expect("back after the drop");
        assert_ne!(back.discontinuity, first, "the jump back is a discontinuity");
        assert!(h.hand_at(Hand::Right, secs(1.5)).is_some_and(|r| r.tracked), "an earlier read holds the step's start");
    }

    #[test]
    fn a_swing_read_before_its_start_holds_at_the_start() {
        let h = ScriptedHands::new(Arc::new(FrameSlot::new()), Arc::new(Never));
        h.start(
            parse(
                "t",
                "right wait 1
right swing 0 0 -4 1
",
            )
            .expect("parsed"),
        );
        assert_eq!(
            h.hand_at(Hand::Right, secs(0.0)).expect("tracked").pose.position,
            REST[1],
            "time 0: the first read"
        );
        let at_start = h.hand_at(Hand::Right, secs(1.0)).expect("tracked").pose.position;
        let later = h.hand_at(Hand::Right, secs(1.5)).expect("tracked").pose.position;
        let earlier = h.hand_at(Hand::Right, secs(0.9)).expect("tracked").pose.position;
        assert_eq!(at_start, REST[1]);
        assert!((later[2] - (REST[1][2] - 2.0)).abs() < 1e-4);
        assert_eq!(earlier, REST[1], "no extrapolation backwards");
    }

    #[test]
    fn steps_whose_time_passed_unread_still_start_in_order() {
        let h = ScriptedHands::new(Arc::new(FrameSlot::new()), Arc::new(Never));
        h.start(parse("t", "right close\nright wait 0.1\nright open\nright wait 0.1\nright drop\n").expect("parsed"));
        assert!(h.hand_at(Hand::Right, secs(0.0)).expect("tracked").closed);
        assert!(h.hand_at(Hand::Right, secs(10.0)).is_none(), "skipped ahead to the drop: absent");
    }
}
