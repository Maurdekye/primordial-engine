//! A fake OpenXR runtime for tests (spec 10.1).
//!
//! It scripts what a runtime would do (state changes, display periods, poses, hands, tracking loss, invalid head
//! poses, recentring, errors from any call, loss) and checks what pe-xr does back against OpenXR's rules. Every rule
//! broken is recorded in [`FakeRuntime::violations`], so a test asserts that list is empty. The rules checked:
//! - session calls in the right states, and nothing at all after the session is destroyed;
//! - wait, begin and end strictly alternating; an image acquired and released inside a frame, once;
//! - action state read only after `xrSyncActions` that frame, and the sync only while FOCUSED;
//! - views located, and the frame ended, at exactly the display time `xrWaitFrame` gave;
//! - a projection layer only when the frame should render, the session is VISIBLE or FOCUSED, an image was acquired
//!   and released this frame, and the views were valid.
//!
//! `xrDestroySession` is allowed at any time, inside a frame too, as OpenXR allows it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::{Backend, EyePose, Fov, FrameTiming, ImageIndex, Layers, Located, SessionState, XrError, XrEvent};
use crate::hands::Reading;
use crate::seam::{Hand, HandSource, HostTime, RigPose};

/// One call pe-xr made, for order checks.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    PollEvent,
    BeginSession,
    EndSession,
    RequestExit,
    DestroySession,
    SyncActions,
    WaitFrame,
    BeginFrame,
    ReadHand(Hand),
    AcquireImage,
    LocateViews(HostTime),
    ReleaseImage,
    EndFrame(Layers),
}

/// A call an error can be injected into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    PollEvent,
    BeginSession,
    EndSession,
    SyncActions,
    WaitFrame,
    BeginFrame,
    AcquireImage,
    LocateViews,
    ReleaseImage,
    EndFrame,
}

type HandScript = Box<dyn FnMut(Hand, HostTime) -> Reading>;

pub struct FakeRuntime {
    pub calls: Vec<Call>,
    pub violations: Vec<String>,
    now: HostTime,
    /// Events still to come, each released once `frames` reaches its key.
    events: VecDeque<(u64, XrEvent)>,
    /// Display periods: (from this frame on, period in ns).
    periods: Vec<(u64, i64)>,
    /// The predicted display time is this far past `now` (the display lead).
    pub lead_ns: i64,
    /// Frames (by number) for which `shouldRender` is false.
    pub not_rendered: Vec<u64>,
    /// Frames (by number) whose head pose is invalid.
    pub invalid_views: Vec<u64>,
    /// Errors to return: (call, frame number, error). Each fires once, at the first such call in that frame.
    pub fail: Vec<(Op, u64, XrError)>,
    hand_script: HandScript,
    /// Extra time to add to the next frame's display time, to fake a missed frame.
    pub skip_next_ns: i64,
    /// Set by a test's renderer while it holds wrappers of the swapchain images: destroying the swapchain then is a
    /// violation (review 1, L4: the wrappers go, and the GPU is idle on them, before xrDestroySwapchain).
    pub images_held: Arc<AtomicBool>,

    // Protocol state.
    frames: u64,
    state: SessionState,
    session_exists: bool,
    running: bool,
    exit_requested: bool,
    in_frame: bool,
    waited: bool,
    image_acquired: bool,
    released_this_frame: bool,
    views_valid_this_frame: bool,
    should_render: bool,
    synced_this_frame: bool,
    display_time: Option<HostTime>,
}

pub const PERIOD_120: i64 = 8_333_333;
pub const PERIOD_90: i64 = 11_111_111;
pub const PERIOD_72: i64 = 13_888_889;

impl FakeRuntime {
    /// A runtime whose session is created in IDLE, at a 120 Hz display period with a 40 ms lead, and controllers
    /// held still in front of the player.
    pub fn new() -> Self {
        FakeRuntime {
            calls: Vec::new(),
            violations: Vec::new(),
            now: HostTime(1_000_000_000),
            events: VecDeque::new(),
            periods: vec![(0, PERIOD_120)],
            lead_ns: 40_000_000,
            not_rendered: Vec::new(),
            invalid_views: Vec::new(),
            fail: Vec::new(),
            hand_script: Box::new(|hand, at| Reading {
                source: HandSource::Controller,
                at,
                pose: Some(RigPose {
                    position: [if hand == Hand::Left { -0.2 } else { 0.2 }, 1.0, -0.3],
                    orientation: [0.0, 0.0, 0.0, 1.0],
                }),
                linear_velocity: Some([0.0; 3]),
                angular_velocity: Some([0.0; 3]),
                grip: Some(0.0),
            }),
            skip_next_ns: 0,
            images_held: Arc::new(AtomicBool::new(false)),
            frames: 0,
            state: SessionState::Idle,
            session_exists: true,
            running: false,
            exit_requested: false,
            in_frame: false,
            waited: false,
            image_acquired: false,
            released_this_frame: false,
            views_valid_this_frame: false,
            should_render: false,
            synced_this_frame: false,
            display_time: None,
        }
    }

    /// Queue an event, released once `after_frames` frames have run.
    pub fn at(mut self, after_frames: u64, ev: XrEvent) -> Self {
        self.events.push_back((after_frames, ev));
        self
    }

    /// Queue an event now, for the next poll.
    pub fn push(&mut self, ev: XrEvent) {
        self.events.push_back((self.frames, ev));
    }

    /// The usual start: READY, SYNCHRONIZED, VISIBLE, FOCUSED, all before the first frame.
    pub fn started(self) -> Self {
        self.at(0, XrEvent::StateChanged(SessionState::Ready))
            .at(0, XrEvent::StateChanged(SessionState::Synchronized))
            .at(0, XrEvent::StateChanged(SessionState::Visible))
            .at(0, XrEvent::StateChanged(SessionState::Focused))
    }

    pub fn period_from(mut self, frame: u64, ns: i64) -> Self {
        self.periods.push((frame, ns));
        self.periods.sort_by_key(|p| p.0);
        self
    }

    pub fn hands(mut self, f: impl FnMut(Hand, HostTime) -> Reading + 'static) -> Self {
        self.hand_script = Box::new(f);
        self
    }

    /// Fail `op` in frame `frame` (1-based; 0 for calls before the first frame) with `e`.
    pub fn failing(mut self, op: Op, frame: u64, e: XrError) -> Self {
        self.fail.push((op, frame, e));
        self
    }

    /// The fake's clock, without importing the `Backend` trait.
    pub fn now_for_test(&self) -> HostTime {
        self.now
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn count(&self, c: &Call) -> usize {
        self.calls.iter().filter(|x| *x == c).count()
    }

    fn period(&self) -> i64 {
        self.periods.iter().rev().find(|p| p.0 <= self.frames).map(|p| p.1).unwrap_or(PERIOD_120)
    }

    fn violate(&mut self, what: impl Into<String>) {
        let w = what.into();
        self.violations.push(format!("frame {}: {w}", self.frames));
    }

    fn need_session(&mut self, call: &str) {
        if !self.session_exists {
            self.violate(format!("{call} after the session was destroyed"));
        }
    }

    fn injected(&mut self, op: Op) -> Result<(), XrError> {
        if let Some(i) = self.fail.iter().position(|(o, f, _)| *o == op && *f == self.frames) {
            return Err(self.fail.remove(i).2);
        }
        Ok(())
    }
}

impl Default for FakeRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for FakeRuntime {
    fn now(&self) -> HostTime {
        self.now
    }

    fn poll_event(&mut self) -> Result<Option<XrEvent>, XrError> {
        self.calls.push(Call::PollEvent);
        self.need_session("xrPollEvent");
        self.injected(Op::PollEvent)?;
        match self.events.front() {
            Some((k, _)) if *k <= self.frames => {
                let (_, ev) = self.events.pop_front().unwrap();
                if let XrEvent::StateChanged(s) = ev {
                    self.state = s;
                }
                Ok(Some(ev))
            }
            _ => Ok(None),
        }
    }

    fn begin_session(&mut self) -> Result<(), XrError> {
        self.calls.push(Call::BeginSession);
        self.need_session("xrBeginSession");
        self.injected(Op::BeginSession)?;
        if self.state != SessionState::Ready || self.running {
            self.violate(format!("xrBeginSession in {:?} (running: {})", self.state, self.running));
        }
        self.running = true;
        Ok(())
    }

    fn end_session(&mut self) -> Result<(), XrError> {
        self.calls.push(Call::EndSession);
        self.need_session("xrEndSession");
        self.injected(Op::EndSession)?;
        if self.state != SessionState::Stopping || !self.running {
            self.violate(format!("xrEndSession in {:?} (running: {})", self.state, self.running));
        }
        self.running = false;
        if self.exit_requested {
            // The runtime goes on to IDLE, then EXITING.
            let k = self.frames;
            self.events.push_back((k, XrEvent::StateChanged(SessionState::Idle)));
            self.events.push_back((k, XrEvent::StateChanged(SessionState::Exiting)));
        }
        Ok(())
    }

    fn request_exit(&mut self) -> Result<(), XrError> {
        self.calls.push(Call::RequestExit);
        self.need_session("xrRequestExitSession");
        if !self.running {
            self.violate("xrRequestExitSession while not running");
        }
        self.exit_requested = true;
        let k = self.frames;
        self.events.push_back((k, XrEvent::StateChanged(SessionState::Visible)));
        self.events.push_back((k, XrEvent::StateChanged(SessionState::Synchronized)));
        self.events.push_back((k, XrEvent::StateChanged(SessionState::Stopping)));
        Ok(())
    }

    fn destroy_session(&mut self) {
        self.calls.push(Call::DestroySession);
        if self.images_held.load(Ordering::Relaxed) {
            self.violate("the swapchain destroyed while the renderer still holds its images");
        }
        if !self.session_exists {
            self.violate("the session destroyed twice");
        }
        self.session_exists = false;
        self.running = false;
        self.in_frame = false;
        self.image_acquired = false;
    }

    fn sync_actions(&mut self) -> Result<(), XrError> {
        self.calls.push(Call::SyncActions);
        self.need_session("xrSyncActions");
        self.injected(Op::SyncActions)?;
        if self.state != SessionState::Focused {
            self.violate(format!("xrSyncActions in {:?}", self.state));
        }
        self.synced_this_frame = true;
        Ok(())
    }

    fn wait_frame(&mut self) -> Result<FrameTiming, XrError> {
        self.calls.push(Call::WaitFrame);
        self.need_session("xrWaitFrame");
        if !self.running {
            self.violate("xrWaitFrame while the session is not running");
        }
        if self.waited {
            self.violate("xrWaitFrame twice without xrBeginFrame");
        }
        if self.in_frame {
            self.violate("xrWaitFrame inside a frame that was never ended");
        }
        self.frames += 1;
        self.injected(Op::WaitFrame)?;
        let period = self.period();
        self.now = self.now.plus_nanos(period + self.skip_next_ns);
        self.skip_next_ns = 0;
        self.waited = true;
        self.synced_this_frame = false;
        self.released_this_frame = false;
        self.views_valid_this_frame = false;
        self.should_render = !self.not_rendered.contains(&self.frames)
            && matches!(self.state, SessionState::Visible | SessionState::Focused);
        let display = self.now.plus_nanos(self.lead_ns);
        self.display_time = Some(display);
        Ok(FrameTiming {
            predicted_display_time: display,
            predicted_display_period_ns: period,
            should_render: self.should_render,
        })
    }

    fn begin_frame(&mut self) -> Result<(), XrError> {
        self.calls.push(Call::BeginFrame);
        self.need_session("xrBeginFrame");
        if !self.waited {
            self.violate("xrBeginFrame without xrWaitFrame");
        }
        if self.in_frame {
            self.violate("xrBeginFrame inside a frame");
        }
        self.waited = false;
        self.injected(Op::BeginFrame)?;
        self.in_frame = true;
        Ok(())
    }

    fn read_hand(&mut self, hand: Hand, at: HostTime) -> Reading {
        self.calls.push(Call::ReadHand(hand));
        self.need_session("an action read");
        if !self.synced_this_frame {
            self.violate("an action read before xrSyncActions this frame");
        }
        (self.hand_script)(hand, at)
    }

    fn acquire_image(&mut self) -> Result<ImageIndex, XrError> {
        self.calls.push(Call::AcquireImage);
        self.need_session("xrAcquireSwapchainImage");
        if !self.in_frame {
            self.violate("an image acquired outside a frame");
        }
        if self.image_acquired {
            self.violate("an image acquired twice");
        }
        self.injected(Op::AcquireImage)?;
        self.image_acquired = true;
        Ok(ImageIndex((self.frames % 3) as u32))
    }

    fn locate_views(&mut self, display_time: HostTime) -> Result<Located, XrError> {
        self.calls.push(Call::LocateViews(display_time));
        self.need_session("xrLocateViews");
        if self.display_time != Some(display_time) {
            self.violate(format!("views located at {display_time:?}, not the display time {:?}", self.display_time));
        }
        self.injected(Op::LocateViews)?;
        let valid = !self.invalid_views.contains(&self.frames);
        self.views_valid_this_frame = valid;
        let fov = Fov { left: -0.8, right: 0.8, up: 0.8, down: -0.8 };
        let eye = |x: f32| EyePose {
            pose: RigPose {
                position: if valid { [x, 1.6, 0.0] } else { [f32::NAN; 3] },
                orientation: [0.0, 0.0, 0.0, 1.0],
            },
            fov,
        };
        Ok(Located { eyes: [eye(-0.032), eye(0.032)], valid })
    }

    fn release_image(&mut self) -> Result<(), XrError> {
        self.calls.push(Call::ReleaseImage);
        self.need_session("xrReleaseSwapchainImage");
        if !self.image_acquired {
            self.violate("an image released without being acquired");
        }
        self.injected(Op::ReleaseImage)?;
        self.image_acquired = false;
        self.released_this_frame = true;
        Ok(())
    }

    fn end_frame(&mut self, display_time: HostTime, layers: Layers) -> Result<(), XrError> {
        self.calls.push(Call::EndFrame(layers));
        self.need_session("xrEndFrame");
        if !self.in_frame {
            self.violate("xrEndFrame without xrBeginFrame");
        }
        if self.image_acquired {
            self.violate("xrEndFrame with an image still acquired");
        }
        if self.display_time != Some(display_time) {
            self.violate(format!("xrEndFrame at {display_time:?}, not the display time {:?}", self.display_time));
        }
        if let Layers::Projection { extent, depth } = layers {
            if extent.is_some_and(|[w, h]| w == 0 || h == 0) {
                self.violate("a projection layer with an empty image rectangle");
            }
            if depth.is_some() {
                self.violate("a depth layer with no depth swapchain");
            }
            if !self.should_render {
                self.violate("a projection layer when shouldRender was false");
            }
            if !matches!(self.state, SessionState::Visible | SessionState::Focused) {
                self.violate(format!("a projection layer in {:?}", self.state));
            }
            if !self.released_this_frame {
                self.violate("a projection layer with no image acquired and released this frame");
            }
            if !self.views_valid_this_frame {
                self.violate("a projection layer drawn from invalid or unlocated views");
            }
        }
        self.in_frame = false;
        self.injected(Op::EndFrame)?;
        Ok(())
    }
}
