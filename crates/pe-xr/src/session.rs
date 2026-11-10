//! The session lifecycle and the frame loop (spec 3.1 and 7.1), over any [`Backend`].
//!
//! [`XrSession::step`] is one turn of the XR thread:
//! - step 0: drain the event queue and act on every state change;
//! - while the session is running: one frame, steps 1 to 8;
//! - otherwise: nothing, and the caller waits about 100 ms before the next step (OpenXR has no blocking
//!   wait for events).
//!
//! The simulation is never touched here. Hands go to the [`HandRing`]; drawing goes to a [`FrameSink`]
//! (pe-render, through pe-xr's graphics binding, in the application).

use std::sync::Arc;

use crate::backend::{Backend, EyePose, FrameTiming, Layers, SessionState, XrError, XrEvent};
use crate::hands::{GraspConfig, HandState};
use crate::ring::HandRing;
use crate::seam::{Hand, HostTime};

/// What the renderer gets for one display frame. pe-render draws the newest frame view at the drawn
/// time, with the own hands late-latched; it records the transition of the image back to its handed-over
/// state and submits before returning (spec 4.2).
pub struct FrameRequest<'a> {
    pub timing: FrameTiming,
    pub eyes: &'a [EyePose; 2],
    pub image: crate::backend::ImageIndex,
}

pub trait FrameSink {
    /// Draw and submit. Return `false` if nothing was drawn, so the frame ends with no layers.
    fn render(&mut self, frame: &FrameRequest<'_>) -> bool;
    /// Whether depth is submitted with the projection layer.
    fn submits_depth(&self) -> bool {
        false
    }
}

/// What one step did, for the XR thread's pacing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A frame ran: `xrWaitFrame` paced it, so step again at once.
    Frame,
    /// No frame loop runs (IDLE before READY, or after STOPPING): wait about 100 ms, then step again.
    Idle,
    /// The session is gone (EXITING or LOSS_PENDING, or lost). The caller destroys the instance if it was
    /// lost, keeps or drops the GPU context per spec 4.1, and goes to desktop mode.
    Ended { lost: bool },
}

/// Counters (spec 9).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counters {
    pub display_frames: u64,
    pub frames_rendered: u64,
    pub frames_not_rendered: u64,
    /// A predicted display time more than 1.5 periods after the previous one.
    pub missed_frames: u64,
    pub state_changes: u64,
    pub recentres: u64,
    pub last_display_period_ns: i64,
}

pub struct XrSession<B: Backend> {
    backend: B,
    state: SessionState,
    running: bool,
    ended: Option<bool>,
    hands: [HandState; 2],
    ring: Arc<HandRing>,
    last_display_time: Option<HostTime>,
    pub counters: Counters,
}

impl<B: Backend> XrSession<B> {
    /// A new session, freshly created in IDLE (the backend has already attached the action sets, once,
    /// at creation).
    pub fn new(backend: B, ring: Arc<HandRing>, grasp: GraspConfig) -> Self {
        XrSession {
            backend,
            state: SessionState::Idle,
            running: false,
            ended: None,
            hands: [
                HandState::new(Hand::Left, grasp),
                HandState::new(Hand::Right, grasp),
            ],
            ring,
            last_display_time: None,
            counters: Counters::default(),
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Leave VR by choice: `xrRequestExitSession`, then STOPPING, IDLE and EXITING arrive as events.
    pub fn request_exit(&mut self) -> Result<(), XrError> {
        if self.running {
            self.backend.request_exit()
        } else {
            self.end(false);
            Ok(())
        }
    }

    fn hands_absent(&mut self) {
        for h in Hand::BOTH {
            self.hands[h.index()].focus_lost();
            self.ring.set_absent(h);
        }
    }

    fn end(&mut self, lost: bool) {
        if self.ended.is_none() {
            self.hands_absent();
            self.running = false;
            self.backend.destroy_session();
            self.ended = Some(lost);
        }
    }

    fn on_error(&mut self, e: XrError) -> Step {
        let lost = matches!(e, XrError::SessionLost | XrError::InstanceLost);
        self.end(lost);
        Step::Ended { lost }
    }

    fn on_state(&mut self, s: SessionState) -> Result<(), XrError> {
        let was = self.state;
        self.state = s;
        self.counters.state_changes += 1;
        if was == SessionState::Focused && s != SessionState::Focused {
            self.hands_absent();
        }
        match s {
            SessionState::Ready if !self.running => {
                self.backend.begin_session()?;
                self.running = true;
                self.last_display_time = None;
            }
            SessionState::Stopping if self.running => {
                self.backend.end_session()?;
                self.running = false;
            }
            SessionState::LossPending => self.end(true),
            SessionState::Exiting => self.end(false),
            _ => {}
        }
        Ok(())
    }

    /// One turn of the XR thread.
    pub fn step(&mut self, sink: &mut dyn FrameSink) -> Step {
        if let Some(lost) = self.ended {
            return Step::Ended { lost };
        }
        // Step 0: drain the events.
        loop {
            let ev = match self.backend.poll_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(e) => return self.on_error(e),
            };
            let r = match ev {
                XrEvent::StateChanged(s) => self.on_state(s),
                XrEvent::InstanceLossPending => {
                    self.end(true);
                    Ok(())
                }
                XrEvent::ReferenceSpaceChangePending => {
                    self.counters.recentres += 1;
                    for h in &mut self.hands {
                        h.recentre();
                    }
                    Ok(())
                }
                XrEvent::InteractionProfileChanged => Ok(()),
            };
            if let Err(e) = r {
                return self.on_error(e);
            }
            if let Some(lost) = self.ended {
                return Step::Ended { lost };
            }
        }
        if !self.running {
            return Step::Idle;
        }
        match self.frame(sink) {
            Ok(()) => Step::Frame,
            Err(e) => self.on_error(e),
        }
    }

    fn frame(&mut self, sink: &mut dyn FrameSink) -> Result<(), XrError> {
        // Steps 1 and 2.
        let timing = self.backend.wait_frame()?;
        self.backend.begin_frame()?;
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

        // Step 3: actions and hands, whether or not this frame is drawn.
        if self.state == SessionState::Focused {
            self.backend.sync_actions()?;
            let now = self.backend.now();
            for h in Hand::BOTH {
                let reading = self.backend.read_hand(h, now);
                if let Some(sample) = self.hands[h.index()].update(reading) {
                    self.ring.push(sample);
                }
            }
        }

        // Steps 4 to 7.
        let mut layers = Layers::None;
        if timing.should_render {
            let image = self.backend.acquire_image()?;
            let eyes = self.backend.locate_views(timing.predicted_display_time)?;
            let drawn = sink.render(&FrameRequest {
                timing,
                eyes: &eyes,
                image,
            });
            self.backend.release_image()?;
            if drawn {
                layers = Layers::Projection {
                    with_depth: sink.submits_depth(),
                };
            }
        }
        if matches!(layers, Layers::None) {
            self.counters.frames_not_rendered += 1;
        } else {
            self.counters.frames_rendered += 1;
        }
        // Step 8.
        self.backend
            .end_frame(timing.predicted_display_time, layers)
    }
}
