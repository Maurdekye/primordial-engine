//! The session lifecycle and the frame loop (spec 3.1 and 7.1), over any [`Backend`].
//!
//! [`XrSession::step`] is one turn of the XR thread:
//! - step 0: drain the event queue and act on every state change;
//! - while the session is running: one frame, steps 1 to 8;
//! - otherwise: nothing, and the caller waits about 100 ms before the next step (OpenXR has no blocking wait for
//!   events).
//!
//! The simulation is never touched here. Hands go to the [`HandRing`]; drawing goes to a [`FrameSink`] (pe-render,
//! through pe-xr's graphics binding, in the application).
//!
//! **Errors** (spec 3.1). A lost session or instance ends the session at once. Any other error in a frame is survived:
//! the frame is closed cleanly (the image released, the frame ended with no layer) and counted, and the session ends
//! only if no frame has succeeded for [`FRAME_ERROR_LIMIT`]. One bad frame never ends the user's VR, nor do good and
//! bad frames alternating (that is reported as degraded).

use std::sync::Arc;
use std::time::Duration;

use pe_render::HandJoints;

use crate::backend::{Backend, DepthInfo, EyePose, FrameTiming, ImageIndex, Layers, SessionState, XrError, XrEvent};
use crate::diag::{self, Diag};
use crate::hands::{GraspConfig, HandState};
use crate::locomotion::{MoveRing, MoveState};
use crate::pointer::{Pointer, PointerState};
use crate::ring::HandRing;
use crate::seam::{Hand, HostTime, RigPose};
use crate::space::SpaceCorrection;

/// How long frames may keep failing, with errors other than a loss, before the session is ended (spec 3.1).
pub const FRAME_ERROR_LIMIT: Duration = Duration::from_secs(1);

/// What the renderer gets for one display frame. pe-render draws the newest frame view at the drawn time, with the
/// own hands at `hands` (late-latched to the display time, spec 5.6); it records the transition of the image back to
/// its handed-over state and submits before returning (spec 4.2).
pub struct FrameRequest<'a> {
    pub timing: FrameTiming,
    pub eyes: &'a [EyePose; 2],
    pub image: ImageIndex,
    /// The depth image acquired with it, when the runtime takes depth; draw depth into it and say so with
    /// [`FrameSink::depth`].
    pub depth_image: Option<ImageIndex>,
    /// The local player's hands located at the display time, for drawing only: `None` when absent or not tracked.
    pub hands: [Option<RigPose>; 2],
    /// A tracked hand's joints at the same display time, for drawing the hand (spec 6); `None` for a controller.
    pub hand_joints: [Option<HandJoints>; 2],
    /// Each hand's pointer at the display time, for the menus (spec 5.7): `None` while the hand can't point. Its
    /// `clicks` count the presses since the last drawn frame.
    pub pointers: [Option<Pointer>; 2],
    /// The menu was toggled (the menu button, or a held palm-up pinch) since the last drawn frame. Several toggles
    /// in that time, from both hands or both paths at once, count as one (@design-reviewer's nit on S1), so they
    /// can't cancel out.
    pub menu_toggled: bool,
}

pub trait FrameSink {
    /// Draw and submit. Return `false` if nothing was drawn, so the frame ends with no layers.
    fn render(&mut self, frame: &FrameRequest<'_>) -> bool;
    /// The rectangle drawn this frame, from the origin (render scale as a viewport): the projection layer's
    /// `imageRect`. `None`: the whole image.
    fn extent(&self) -> Option<[u32; 2]> {
        None
    }
    /// The depth layer's range, when this frame's depth was drawn into `FrameRequest::depth_image`. Submitted only
    /// when there was a depth image.
    fn depth(&self) -> Option<DepthInfo> {
        None
    }
    /// The frame has ended: its image was released and `xrEndFrame` returned, so nothing done here delays it. Work
    /// that must not cost a headset frame goes here, such as an agent's view capture (render.md 11). `drawn`: the
    /// frame went out with a projection layer. It runs before the next `xrWaitFrame`, which waits for the runtime
    /// anyway, so it delays the next frame only by however much it overruns that wait (spec 7.1, step 9). Not called
    /// for a frame that failed.
    fn after_frame(&mut self, drawn: bool) {
        let _ = drawn;
    }
    /// The session is about to be destroyed, its swapchain with it: drop every wrapper of the swapchain's images and
    /// wait until the GPU is idle on them before returning (review 1, L4; spec 4.3). Called once, before
    /// `xrDestroySwapchain`.
    /// Required, with no default, so a sink cannot forget it (binding review L1): a sink that holds no images
    /// implements it empty.
    fn release_swapchain(&mut self);
}

/// Why a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// EXITING: the user left VR, or the runtime asked to.
    Exited,
    /// The session was lost (LOSS_PENDING or `XR_ERROR_SESSION_LOST`): the instance is still good, so a new session
    /// may be tried on it.
    SessionLost,
    /// The instance was lost: drop it and load the runtime again.
    InstanceLost,
    /// Frames kept failing for [`FRAME_ERROR_LIMIT`].
    FrameErrors,
}

/// What one step did, for the XR thread's pacing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A frame ran (or was closed after an error): `xrWaitFrame` paced it, so step again at once.
    Frame,
    /// No frame loop runs (IDLE before READY, or after STOPPING): wait about 100 ms, then step again.
    Idle,
    /// The session is gone and destroyed. The caller keeps or drops the GPU context per spec 4.1 and goes to desktop
    /// mode.
    Ended(EndReason),
}

/// Counters (spec 9).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counters {
    pub display_frames: u64,
    pub frames_rendered: u64,
    pub frames_not_rendered: u64,
    /// Frames not drawn because the head's pose was invalid.
    pub invalid_view_frames: u64,
    /// Frames closed after an error other than a loss.
    pub frame_errors: u64,
    /// Runs of failing frames that began within [`FRAME_ERROR_LIMIT`] of an earlier failure, with good frames
    /// between them: the session limps rather than fails (good and bad frames alternating), so it is never ended
    /// for it, but it is reported as degraded.
    pub degraded_runs: u64,
    /// A predicted display time more than 1.5 periods after the previous one.
    pub missed_frames: u64,
    pub state_changes: u64,
    pub recentres: u64,
    /// How each recentre was measured (spec 3.2).
    pub recentre: crate::space::SpaceCounters,
    pub events_lost: u64,
    pub last_display_period_ns: i64,
    /// Per hand (left, right): FOCUSED frames whose reading came from hand tracking, from a controller, or had no
    /// valid pose (spec 5.4).
    pub hand_frames: [HandFrames; 2],
}

/// One hand's FOCUSED frames by where its pose came from (spec 5.4, 9).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HandFrames {
    /// From hand tracking ([`diag::PoseSource::Tracked`]).
    pub tracked: u64,
    pub controller: u64,
    pub none: u64,
}

pub struct XrSession<B: Backend> {
    backend: B,
    state: SessionState,
    running: bool,
    ended: Option<EndReason>,
    /// Whether the session (and its swapchain) has been destroyed, after the sink released the images.
    destroyed: bool,
    hands: [HandState; 2],
    /// The pointers (spec 5.7): each hand's state, its pointer at the last focused frame's display time, and the
    /// menu toggles not yet handed to a drawn frame.
    pointers: [PointerState; 2],
    pointer_now: [Option<Pointer>; 2],
    menu_toggled: bool,
    ring: Arc<HandRing>,
    /// Locomotion (spec 5.8): the state across frames, and the ring the simulation reads, if pe-app gave one.
    locomotion: MoveState,
    moves: Option<Arc<MoveRing>>,
    /// A recentre's correction, applied to every pose read (spec 3.2).
    space: SpaceCorrection,
    /// Whether the first focused frame's locomotion actions were logged (a runtime that left them unbound shows here).
    moves_logged: bool,
    last_display_time: Option<HostTime>,
    /// The last frame that ran without an error (or when the session began running). The session ends for errors
    /// only when no frame has succeeded for [`FRAME_ERROR_LIMIT`] since it.
    last_good: Option<HostTime>,
    /// The last frame error, and whether the frame before the current one succeeded.
    last_error_at: Option<HostTime>,
    previous_failed: bool,
    pub last_error: Option<XrError>,
    pub counters: Counters,
    /// The opt-in diagnostic log (`PE_XR_LOG=1`, [`crate::diag`]).
    diag: Diag,
    /// Whether the interaction profiles were logged at the first focused frame.
    profiles_logged: bool,
}

/// How far a frame got, so an error can close it cleanly.
#[derive(Default)]
struct FrameProgress {
    began: bool,
    acquired: bool,
}

impl<B: Backend> XrSession<B> {
    /// A new session, freshly created in IDLE (the backend has already attached the action sets, once, at creation).
    pub fn new(backend: B, ring: Arc<HandRing>, grasp: GraspConfig) -> Self {
        XrSession {
            backend,
            state: SessionState::Idle,
            running: false,
            ended: None,
            destroyed: false,
            hands: [HandState::new(Hand::Left, grasp), HandState::new(Hand::Right, grasp)],
            pointers: Default::default(),
            pointer_now: [None, None],
            menu_toggled: false,
            ring,
            locomotion: MoveState::default(),
            moves: None,
            space: SpaceCorrection::default(),
            moves_logged: false,
            last_display_time: None,
            last_good: None,
            last_error_at: None,
            previous_failed: false,
            last_error: None,
            counters: Counters::default(),
            diag: Diag::from_env(),
            profiles_logged: false,
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Whether a frame has failed within the last [`FRAME_ERROR_LIMIT`]: the session runs, but degraded (for the
    /// log and the counters, spec 3.1).
    pub fn degraded(&self) -> bool {
        self.last_error_at.is_some_and(|t| self.backend.now().secs_since(t) < FRAME_ERROR_LIMIT.as_secs_f64())
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Leave VR by choice: `xrRequestExitSession`, then STOPPING, IDLE and EXITING arrive as events. If the request
    /// fails (`XR_ERROR_SESSION_NOT_RUNNING` from a `running` that is one event behind is enough), the session ends
    /// here, as `Exited` or as the loss the error names, and the error is returned: the next [`Self::step`] answers
    /// `Ended` and tears down, so a caller that asks once can never be left with a session that keeps running.
    pub fn request_exit(&mut self) -> Result<(), XrError> {
        if self.running {
            let r = self.backend.request_exit();
            if let Err(e) = &r {
                let why = match e {
                    XrError::SessionLost => EndReason::SessionLost,
                    XrError::InstanceLost => EndReason::InstanceLost,
                    XrError::Other(_) => EndReason::Exited,
                };
                self.last_error = Some(e.clone());
                self.end(why);
            }
            r
        } else {
            self.end(EndReason::Exited);
            Ok(())
        }
    }

    /// End the session now, without waiting for the runtime: for a caller whose exit request the runtime accepted but
    /// never followed with STOPPING (a deadline on quitting). The next [`Self::step`] answers `Ended(Exited)` and tears
    /// down; `xrDestroySession` is valid in any state. Nothing happens if the session already ended.
    pub fn abandon(&mut self) {
        self.end(EndReason::Exited);
    }

    /// The ring the frame loop writes locomotion samples to (spec 5.8; pe-app's move feed while VR runs).
    pub fn with_moves(mut self, moves: Arc<MoveRing>) -> Self {
        self.moves = Some(moves);
        self
    }

    /// As [`Self::with_moves`], on a session already made (pe-app's opened VR).
    pub fn set_moves(&mut self, moves: Arc<MoveRing>) {
        self.moves = Some(moves);
    }

    fn hands_absent(&mut self) {
        for h in Hand::BOTH {
            self.hands[h.index()].focus_lost();
            self.ring.set_absent(h);
            self.pointers[h.index()].reset();
            self.pointer_now[h.index()] = None;
        }
        self.locomotion.focus_lost();
    }

    /// Mark the session ended. It is destroyed by [`Self::teardown`], which [`Self::step`] calls before returning
    /// `Ended`, so that the sink has released the swapchain's images first.
    fn end(&mut self, why: EndReason) {
        if self.ended.is_none() {
            self.hands_absent();
            // No session, no headset locomotion: the simulation reads none until the next one.
            if let Some(m) = &self.moves {
                m.clear();
            }
            self.running = false;
            self.ended = Some(why);
        }
    }

    /// Destroy an ended session: the sink drops its image wrappers and idles the GPU on them, then the session and
    /// its swapchain go. Once only. The application calls it itself when it ends a session outside `step` (with
    /// [`Self::request_exit`] before the session ran).
    pub fn teardown(&mut self, sink: &mut dyn FrameSink) {
        if self.ended.is_some() && !self.destroyed {
            if let Some(line) = self.diag.ended(self.ended, &self.counters) {
                diag::emit(&line);
            }
            let maxima = self.hands.each_ref().map(|h| (h.max_squeeze, h.max_curl));
            if let Some(line) = self.diag.grip_maxima(&maxima) {
                diag::emit(&line);
            }
            sink.release_swapchain();
            self.backend.destroy_session();
            self.destroyed = true;
        }
    }

    /// An error outside a frame (events, begin or end of the session): a loss ends the session as that loss; anything
    /// else ends it too, since the session's state is then unknown.
    fn fail(&mut self, e: XrError) -> Step {
        let why = match e {
            XrError::SessionLost => EndReason::SessionLost,
            XrError::InstanceLost => EndReason::InstanceLost,
            XrError::Other(_) => EndReason::FrameErrors,
        };
        self.last_error = Some(e);
        self.end(why);
        Step::Ended(why)
    }

    /// Each hand's current interaction profile, under `PE_XR_LOG` only.
    fn log_profiles(&self, why: &str) {
        if diag::enabled()
            && let Some(p) = self.backend.interaction_profiles()
            && let Some(line) = self.diag.profiles(why, &p)
        {
            diag::emit(&line);
        }
    }

    fn on_state(&mut self, s: SessionState) -> Result<(), XrError> {
        let was = self.state;
        self.state = s;
        self.counters.state_changes += 1;
        if let Some(line) = self.diag.state(was, s, self.backend.now().0) {
            diag::emit(&line);
        }
        if was == SessionState::Focused && s != SessionState::Focused {
            self.hands_absent();
        }
        match s {
            SessionState::Ready if !self.running => {
                self.backend.begin_session()?;
                self.running = true;
                self.last_display_time = None;
                self.last_good = Some(self.backend.now());
                self.previous_failed = false;
            }
            SessionState::Stopping if self.running => {
                self.backend.end_session()?;
                self.running = false;
            }
            SessionState::LossPending => self.end(EndReason::SessionLost),
            SessionState::Exiting => self.end(EndReason::Exited),
            _ => {}
        }
        Ok(())
    }

    /// One turn of the XR thread. When it returns `Ended`, the session has been torn down ([`Self::teardown`]).
    pub fn step(&mut self, sink: &mut dyn FrameSink) -> Step {
        let step = self.step_inner(sink);
        if let Step::Ended(_) = step {
            self.teardown(sink);
        }
        step
    }

    fn step_inner(&mut self, sink: &mut dyn FrameSink) -> Step {
        if let Some(why) = self.ended {
            return Step::Ended(why);
        }
        // Step 0: drain the events.
        loop {
            let ev = match self.backend.poll_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(e) => return self.fail(e),
            };
            let r = match ev {
                XrEvent::StateChanged(s) => self.on_state(s),
                XrEvent::InstanceLossPending => {
                    self.end(EndReason::InstanceLost);
                    Ok(())
                }
                XrEvent::ReferenceSpaceChangePending(change) => {
                    // Corrected from its change time on (spec 3.2), so the hands don't jump.
                    self.counters.recentres += 1;
                    self.space.on_change(change);
                    Ok(())
                }
                XrEvent::InteractionProfileChanged => {
                    self.log_profiles("changed");
                    Ok(())
                }
                XrEvent::EventsLost(n) => {
                    self.counters.events_lost += u64::from(n);
                    Ok(())
                }
            };
            if let Err(e) = r {
                return self.fail(e);
            }
            if let Some(why) = self.ended {
                return Step::Ended(why);
            }
        }
        if !self.running {
            return Step::Idle;
        }
        let mut progress = FrameProgress::default();
        match self.frame(sink, &mut progress) {
            Ok(()) => {
                self.last_good = Some(self.backend.now());
                self.previous_failed = false;
                Step::Frame
            }
            Err(e @ (XrError::SessionLost | XrError::InstanceLost)) => self.fail(e),
            Err(e) => self.survive(e, progress),
        }
    }

    /// Close a frame that failed with an error other than a loss, and end the session only if frames have been failing
    /// for [`FRAME_ERROR_LIMIT`].
    fn survive(&mut self, e: XrError, progress: FrameProgress) -> Step {
        self.counters.frame_errors += 1;
        self.last_error = Some(e);
        if progress.acquired {
            let _ = self.backend.release_image();
        }
        if progress.began {
            let display = self.last_display_time.unwrap_or_else(|| self.backend.now());
            let _ = self.backend.end_frame(display, Layers::None);
        }
        let now = self.backend.now();
        let limit = FRAME_ERROR_LIMIT.as_secs_f64();
        // A new run of failures soon after an earlier one, with good frames between: degraded, never ended for it.
        if !self.previous_failed && self.last_error_at.is_some_and(|t| now.secs_since(t) < limit) {
            self.counters.degraded_runs += 1;
        }
        self.last_error_at = Some(now);
        self.previous_failed = true;
        let since = *self.last_good.get_or_insert(now);
        if now.secs_since(since) >= limit {
            self.end(EndReason::FrameErrors);
            return Step::Ended(EndReason::FrameErrors);
        }
        Step::Frame
    }

    fn frame(&mut self, sink: &mut dyn FrameSink, progress: &mut FrameProgress) -> Result<(), XrError> {
        // Steps 1 and 2.
        let timing = self.backend.wait_frame()?;
        // If xrBeginFrame fails short of a loss after xrWaitFrame succeeded, OpenXR may block the next xrWaitFrame
        // until a frame is begun, and the loop would stall there. That takes a runtime fault or a call-order bug
        // (review 1 recheck, nit); the session still ends by the error limit only if xrWaitFrame returns.
        self.backend.begin_frame()?;
        progress.began = true;
        self.counters.display_frames += 1;
        let period = timing.predicted_display_period_ns;
        if let Some(prev) = self.last_display_time
            && (timing.predicted_display_time.0 - prev.0) as f64 > 1.5 * period as f64
        {
            self.counters.missed_frames += 1;
        }
        self.last_display_time = Some(timing.predicted_display_time);
        if period != self.counters.last_display_period_ns {
            self.counters.last_display_period_ns = period;
            self.ring.set_display_period(period);
        }

        // A recentre's correction (spec 3.2): the changes due at this display time apply before any pose is read, and
        // its yaw goes into this frame's heading, in the same sample as the corrected head and hands.
        let applied = self.space.frame(timing.predicted_display_time, self.backend.head(timing.predicted_display_time));
        if applied.yaw != 0.0 {
            self.locomotion.turn_by(applied.yaw);
        }
        if applied.unmeasured {
            // Not measurable: the jump goes through, so the hands say so.
            for h in &mut self.hands {
                h.recentre();
            }
        }
        self.counters.recentre = self.space.counters;

        // Step 3: actions and hands, whether or not this frame is drawn.
        let focused = self.state == SessionState::Focused;
        if focused {
            self.backend.sync_actions()?;
            if !self.profiles_logged {
                self.profiles_logged = true;
                self.log_profiles("first focused frame");
            }
            let now = self.backend.now();
            let shown_at = timing.predicted_display_time;
            for h in Hand::BOTH {
                let reading = self.space.reading(self.backend.read_hand(h, now));
                // The same hand where it will be when this frame is shown: the simulation runs at that time, so the
                // ring's samples carry those poses; the grip and grasp still come from now (spec 6.2).
                let shown = self.space.reading(self.backend.read_hand(h, shown_at));
                let source = diag::PoseSource::of(&reading);
                let frames = &mut self.counters.hand_frames[h.index()];
                match source {
                    diag::PoseSource::Tracked => frames.tracked += 1,
                    diag::PoseSource::Controller => frames.controller += 1,
                    diag::PoseSource::None => frames.none += 1,
                }
                if let Some(line) = self.diag.hand_source(h, source, now.0) {
                    diag::emit(&line);
                }
                if let Some(sample) = self.hands[h.index()].update_shown(reading, Some(shown)) {
                    self.ring.push(sample);
                }
                if let Some(e) = self.hands[h.index()].take_edge()
                    && let Some(line) = self.diag.grasp_edge(h, &e, now.0)
                {
                    diag::emit(&line);
                }
                // The pointer at the display time, every focused frame, so no press is lost to a frame that isn't
                // drawn (spec 5.7). The joints are those of the `shown` reading just made at that time.
                let reading = self.space.pointer(self.backend.read_pointer(h, shown_at));
                let joints = self.space.joints(self.backend.hand_joints(h, shown_at));
                let holding = self.hands[h.index()].closed();
                let step = self.pointers[h.index()].step(h, shown_at, reading, joints.as_ref(), holding);
                self.pointer_now[h.index()] = step.pointer;
                self.menu_toggled |= step.menu_toggled;
            }
        }
        // Locomotion (spec 5.8), every frame at its display time: while focused from the actions, otherwise a
        // standstill, so the simulation never holds a walk the player can't stop.
        if let Some(moves) = &self.moves {
            let at = timing.predicted_display_time;
            let reading =
                if focused { self.space.move_reading(self.backend.read_move(at)) } else { Default::default() };
            if focused && !self.moves_logged && diag::enabled() {
                self.moves_logged = true;
                let on = |b: bool| if b { "active" } else { "not active" };
                diag::emit(&format!(
                    "locomotion at the first focused frame: move {}, turn {}, jump {}, head {}",
                    on(reading.stick.is_some()),
                    on(reading.turn.is_some()),
                    on(reading.jump.is_some()),
                    if reading.head.is_some() { "located" } else { "not located" }
                ));
            }
            moves.push(self.locomotion.step(at, &reading, focused));
        }

        // Steps 4 to 7, only when the runtime wants a frame and the session is shown.
        let shown = matches!(self.state, SessionState::Visible | SessionState::Focused);
        let mut layers = Layers::None;
        if timing.should_render && shown {
            let image = self.backend.acquire_image()?;
            progress.acquired = true;
            let located = self.space.located(self.backend.locate_views(timing.predicted_display_time)?);
            if located.valid {
                // The own hands at the display time, for drawing only (spec 5.6).
                let at = timing.predicted_display_time;
                let hands = if focused {
                    Hand::BOTH.map(|h| self.space.reading(self.backend.read_hand(h, at)).pose)
                } else {
                    [None, None]
                };
                let hand_joints = Hand::BOTH
                    .map(|h| hands[h.index()].and_then(|_| self.space.joints(self.backend.hand_joints(h, at))));
                if let Some(line) = self.diag.first_eyes(&located.eyes, timing.predicted_display_period_ns) {
                    diag::emit(&line);
                }
                for line in self.diag.first_hands(&hands) {
                    diag::emit(&line);
                }
                let depth_image = self.backend.depth_image();
                let pointers = if focused { self.pointer_now } else { [None, None] };
                let menu_toggled = self.menu_toggled;
                let drawn = sink.render(&FrameRequest {
                    timing,
                    eyes: &located.eyes,
                    image,
                    depth_image,
                    hands,
                    hand_joints,
                    pointers,
                    menu_toggled,
                });
                // Handed over: the sink saw these clicks and toggles.
                self.menu_toggled = false;
                for p in &mut self.pointers {
                    p.delivered();
                }
                if drawn {
                    let depth = sink.depth().filter(|_| depth_image.is_some());
                    layers = Layers::Projection { extent: sink.extent(), depth };
                }
            } else {
                // Head tracking lost for a moment: never draw or submit unspecified poses.
                self.counters.invalid_view_frames += 1;
            }
            self.backend.release_image()?;
            progress.acquired = false;
        }
        let drawn = !matches!(layers, Layers::None);
        if drawn {
            self.counters.frames_rendered += 1;
        } else {
            self.counters.frames_not_rendered += 1;
        }
        // Step 8. If xrEndFrame itself fails, the frame is over all the same: it is never ended twice.
        progress.began = false;
        self.backend.end_frame(timing.predicted_display_time, layers)?;
        // Step 9: the sink's work after the frame.
        sink.after_frame(drawn);
        Ok(())
    }
}
