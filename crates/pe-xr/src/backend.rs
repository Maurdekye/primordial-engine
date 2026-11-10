//! The backend: everything pe-xr asks of an OpenXR runtime, as a small trait (spec 10.1).
//!
//! The session logic and the frame loop ([`crate::session`]) run against this trait, so almost all of
//! pe-xr is tested with [`crate::fake::FakeRuntime`] and no runtime at all. The real backend wraps the
//! `openxr` crate and stays thin.

use crate::hands::Reading;
use crate::seam::{Hand, HostTime, RigPose};

/// OpenXR's session states (`XrSessionState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionState {
    Idle,
    Ready,
    Synchronized,
    Visible,
    Focused,
    Stopping,
    LossPending,
    Exiting,
}

/// The events pe-xr acts on (spec 3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XrEvent {
    StateChanged(SessionState),
    InstanceLossPending,
    /// A recentre: the tracking space moves under the player.
    ReferenceSpaceChangePending,
    InteractionProfileChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XrError {
    /// The session was lost (`XR_ERROR_SESSION_LOST`).
    SessionLost,
    /// The instance was lost (`XR_ERROR_INSTANCE_LOST`).
    InstanceLost,
    /// Any other failure, with the runtime's words.
    Other(String),
}

/// What `xrWaitFrame` returned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameTiming {
    pub predicted_display_time: HostTime,
    pub predicted_display_period_ns: i64,
    pub should_render: bool,
}

/// An asymmetric field of view, in radians (OpenXR's `XrFovf`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fov {
    pub left: f32,
    pub right: f32,
    pub up: f32,
    pub down: f32,
}

/// One eye at the display time, in the rig frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EyePose {
    pub pose: RigPose,
    pub fov: Fov,
}

/// The swapchain image acquired for this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageIndex(pub u32);

/// What `xrEndFrame` submits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layers {
    /// No layers: `shouldRender` was false, or the frame was not drawn.
    None,
    /// One projection layer from the acquired image, with its depth when `with_depth`.
    Projection { with_depth: bool },
}

pub trait Backend {
    /// The host clock (spec 6.1).
    fn now(&self) -> HostTime;
    /// One event from `xrPollEvent`, or `None` when the queue is empty.
    fn poll_event(&mut self) -> Result<Option<XrEvent>, XrError>;
    /// `xrBeginSession` on READY.
    fn begin_session(&mut self) -> Result<(), XrError>;
    /// `xrEndSession` on STOPPING. The session and the GPU context are kept (spec 3.1).
    fn end_session(&mut self) -> Result<(), XrError>;
    /// `xrRequestExitSession`: leaving VR by choice.
    fn request_exit(&mut self) -> Result<(), XrError>;
    /// Destroy the swapchains, then the session, in that order (spec 3.1). The GPU context is not the
    /// backend's to destroy: it outlives the session.
    fn destroy_session(&mut self);
    /// `xrSyncActions`, while FOCUSED, before any action state is read.
    fn sync_actions(&mut self) -> Result<(), XrError>;
    fn wait_frame(&mut self) -> Result<FrameTiming, XrError>;
    fn begin_frame(&mut self) -> Result<(), XrError>;
    /// Locate one hand at a host time: a tracked hand's joints or a controller's grip, as a reading.
    fn read_hand(&mut self, hand: Hand, at: HostTime) -> Reading;
    /// Acquire, then wait for, this frame's swapchain image.
    fn acquire_image(&mut self) -> Result<ImageIndex, XrError>;
    /// `xrLocateViews` at the display time.
    fn locate_views(&mut self, display_time: HostTime) -> Result<[EyePose; 2], XrError>;
    /// Release the image, after the frame's work was submitted.
    fn release_image(&mut self) -> Result<(), XrError>;
    fn end_frame(&mut self, display_time: HostTime, layers: Layers) -> Result<(), XrError>;
}
