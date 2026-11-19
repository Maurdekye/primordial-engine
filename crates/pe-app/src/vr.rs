//! VR mode (app.md 1, 4; xr.md 3.1, 4.2): opening VR on the main thread ([`start`]), the XR thread's loop
//! ([`run_xr`]) and its frame sink ([`VrSink`]). The sink reads every frame view since the last frame and draws the
//! newest through pe-render's presenter into pe-xr's swapchain images. pe-xr owns the swapchain's wgpu wrappers ([`XrTargets`]); pe-app owns the drawing
//! (option (a), agreed with @xr-eng).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pe_render::camera::Camera;
use pe_render::capture_service::CaptureServer;
use pe_render::counters::{Histogram, RunReport, Summary};
use pe_render::drawn_time::DrawnTime;
use pe_render::gpu::GpuContext;
use pe_render::present::{Batch, FrameSlot, Presenter};
use pe_render::scene::{RenderSettings, Renderer};
use pe_render::window::EffectSink;
use pe_render::{DisplayHand, LocalPresentation};
use pe_seams::{FrameView, HostTime};
use pe_xr::backend::{Backend, DepthInfo};
use pe_xr::binding::{self, XrTargets};
use pe_xr::guard::Guard;
use pe_xr::hands::GraspConfig;
use pe_xr::loader::Discovery;
use pe_xr::open::{self, NotOpened};
use pe_xr::ring::HandRing;
use pe_xr::session::{EndReason, FrameRequest, FrameSink, Step, XrSession};

/// Draws each VR frame. It lives on the XR thread; the simulation thread reaches it only through the frame slot.
///
/// A sink serves one session: after [`FrameSink::release_swapchain`] its targets are gone and it draws nothing. A
/// restarted session comes from a new `open_vulkan`, with a new sink (@xr-eng's L1).
pub struct VrSink {
    views: ViewReader,
    presenter: Presenter,
    /// Built on the GPU context pe-xr made through the runtime (`VulkanVr`).
    renderer: Renderer,
    targets: XrTargets,
    /// Sounds go to pe-app's audio, haptics to pe-xr (render.md 5).
    effects: Box<dyn EffectSink + Send>,
    /// The render scale as a viewport: the rectangle drawn from the origin, or `None` for the whole image. The
    /// swapchain keeps its size (render.md 4).
    extent: Option<[u32; 2]>,
    /// `view_capture`'s server (api.md 8), with the left eye's camera as the player's. Served after the frame ends.
    captures: Option<AfterFrame>,
    /// X8's Present (render.md 7, VR's default): the presenter gets this host's now as the display time, so the world
    /// is drawn at the engine's present; the eye cameras and the hands keep the runtime's predicted display time.
    present: bool,
    /// The display lead, ms: the runtime's predicted display time minus now, at each frame drawn.
    lead_ms: Histogram,
    /// B (app.md 3, [`crate::shown`]): under Shown, each frame's lead also goes to the simulation thread, which runs
    /// its shown present that far ahead. Under any other rule the simulation stays at the present.
    shown: Option<Arc<crate::shown::DisplayLead>>,
    /// Where the run's counters go when the sink drops (`VrReport`).
    report: ReportSlot,
}

/// What a VR run reports at its end (@coordinator-opus): the presenter's counters for
/// the run, and the measured display lead.
#[derive(Clone, Debug)]
pub struct VrReport {
    pub render: RunReport,
    pub lead_ms: Summary,
}

/// Filled by the sink when it drops, on the XR thread; read once the XR thread has ended.
pub type ReportSlot = Arc<Mutex<Option<VrReport>>>;

impl VrSink {
    pub fn new(
        slot: Arc<FrameSlot>,
        presenter: Presenter,
        renderer: Renderer,
        targets: XrTargets,
        effects: Box<dyn EffectSink + Send>,
        extent: Option<[u32; 2]>,
        report: ReportSlot,
    ) -> VrSink {
        let present = matches!(presenter.display().settings().drawn_time, DrawnTime::Present { .. });
        VrSink {
            views: ViewReader::new(slot),
            presenter,
            renderer,
            targets,
            effects,
            extent,
            captures: None,
            present,
            lead_ms: Histogram::default(),
            shown: None,
            report,
        }
    }

    /// This sink sending each frame's display lead to `lead` (B, app.md 3.3), under Shown only: under any other rule
    /// the simulation stays at the present.
    pub fn with_shown(mut self, lead: Arc<crate::shown::DisplayLead>) -> VrSink {
        if matches!(self.presenter.display().settings().drawn_time, DrawnTime::Shown { .. }) {
            self.shown = Some(lead);
        }
        self
    }

    /// This sink serving `view_capture`'s orders once a drawn frame has ended ([`FrameSink::after_frame`]).
    pub fn with_captures(mut self, queue: Arc<pe_render::capture_service::CaptureQueue>) -> VrSink {
        self.captures = Some(AfterFrame { server: CaptureServer::new(queue), drawn: None });
        self
    }
}

/// `view_capture`'s server on the headset: [`FrameSink::render`] notes the frame it drew, and
/// [`FrameSink::after_frame`] serves the queue from it once the image is released and `xrEndFrame` has returned, so a
/// capture never stands between the eyes' draw and the compositor (render.md 11).
struct AfterFrame {
    server: CaptureServer,
    /// The view the last frame drew and its left eye's camera; taken by the next `after_frame`.
    drawn: Option<(Arc<FrameView>, Camera)>,
}

impl AfterFrame {
    /// A frame went out drawn from `view`, with `player` as its left eye.
    fn drew(&mut self, view: Arc<FrameView>, player: Camera) {
        self.drawn = Some((view, player));
    }

    /// The view and camera to serve from, once per drawn frame: `None` when the frame that ended drew nothing, so a
    /// capture never uses an older frame's view after the sink stopped drawing.
    fn ended(&mut self, drawn: bool) -> Option<(Arc<FrameView>, Camera)> {
        let last = self.drawn.take();
        if drawn { last } else { None }
    }
}

impl Drop for VrSink {
    fn drop(&mut self) {
        let r = VrReport { render: self.presenter.counters().run(), lead_ms: self.lead_ms.summary() };
        if let Ok(mut slot) = self.report.lock() {
            *slot = Some(r);
        }
    }
}

/// What a display frame reads from the frame slot (render.md 3): every view published since the last frame, once and
/// in order, and the newest of them, which the cameras use. An empty batch keeps the last view, so the frame draws it
/// again. A slot has one reader: views taken here are gone for anyone else.
pub struct ViewReader {
    slot: Arc<FrameSlot>,
    /// The newest view taken. `Presenter` keeps its own for an empty batch, but the eye cameras need the view before
    /// `frame_batch` runs, so the reader keeps one too (@design-reviewer's nit on 7b5d78d6).
    last: Option<Arc<FrameView>>,
}

impl ViewReader {
    pub fn new(slot: Arc<FrameSlot>) -> ViewReader {
        ViewReader { slot, last: None }
    }

    /// The views since the last call and the newest view, or `None` before the first view.
    pub fn take(&mut self) -> Option<(Batch, Arc<FrameView>)> {
        let batch = self.slot.take();
        let view = batch.views.last().cloned().or_else(|| self.last.clone())?;
        self.last = Some(view.clone());
        Some((batch, view))
    }
}

/// The local player's part of a display frame: the hands pe-xr located for the runtime's predicted display time, with
/// a tracked hand's joints for drawing it (xr.md 6, render.md 5), and as its display time `now` under Present (the
/// engine's present, render.md 7), else that predicted time.
fn local_presentation(frame: &FrameRequest<'_>, now: Option<HostTime>) -> LocalPresentation {
    let hand = |i: usize| frame.hands[i].map(|pose| DisplayHand { pose, joints: frame.hand_joints[i] });
    LocalPresentation { display_time: now.unwrap_or(frame.timing.predicted_display_time), hands: [hand(0), hand(1)] }
}

impl FrameSink for VrSink {
    /// Nothing is drawn before the first view, or for an image outside the swapchain: the frame ends with no layers.
    ///
    /// Every view published since the last frame feeds the display layer, the effects and the notices
    /// (`Presenter::frame_batch`, render.md 3) before the target is checked, so an image outside the swapchain skips a
    /// drawing, never a view.
    fn render(&mut self, frame: &FrameRequest<'_>) -> bool {
        let Some((batch, view)) = self.views.take() else {
            return false;
        };
        let cameras = binding::eye_cameras(&view.local_rig, frame.eyes);
        let now = crate::clock::now();
        self.lead_ms.record(frame.timing.predicted_display_time.secs_since(now) * 1e3);
        if let Some(lead) = &self.shown {
            lead.record(now, frame.timing.predicted_display_time.0.saturating_sub(now.0));
        }
        let local = local_presentation(frame, self.present.then_some(now));
        let Some(presented) = self.presenter.frame_batch(&batch, &local, &cameras) else {
            return false;
        };
        for c in &presented.effects {
            self.effects.command(c);
        }
        let Some(target) = self.targets.stereo(frame.image, frame.depth_image, self.extent) else {
            return false;
        };
        self.renderer.render_stereo(&presented.scene, cameras, &target);
        if let Some(c) = self.captures.as_mut() {
            c.drew(view, cameras[0]);
        }
        true
    }

    /// Serves `view_capture` from the frame just drawn: its scene build, labels and capture pass run here, after
    /// `xrEndFrame`, not ahead of it (render.md 11).
    fn after_frame(&mut self, drawn: bool) {
        if let Some(c) = self.captures.as_mut()
            && let Some((view, player)) = c.ended(drawn)
        {
            c.server.serve(&mut self.renderer, &view, Some(&player));
        }
    }

    fn extent(&self) -> Option<[u32; 2]> {
        self.extent
    }

    /// pe-render draws reverse-Z depth in unscaled metres whenever the target has a depth image.
    fn depth(&self) -> Option<DepthInfo> {
        self.targets.has_depth().then(binding::vr_depth_info)
    }

    /// Waits until the GPU is idle on the swapchain's images, then drops their wrappers.
    fn release_swapchain(&mut self) {
        self.targets.release(self.renderer.context());
    }
}

/// The sink moves to the XR thread. So does the session `open_vulkan` makes on the main thread (app.md 4 step 3):
/// pe-xr asserts that `VulkanVr` and its `RealBackend` are `Send`, and [`start`] hands the session to [`spawn_xr`].
const _: fn() = || {
    fn send<T: Send>() {}
    send::<VrSink>();
};

/// How the XR thread's session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XrEnd {
    pub reason: EndReason,
    /// Frame steps, including frames closed after an error.
    pub frames: u64,
}

/// The XR thread's waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XrTimes {
    /// While no frame loop runs (pe-xr's `Step::Idle`), how long the loop waits before it steps again, unless
    /// unparked.
    pub idle: Duration,
    /// After asking to leave VR, how long the loop lets the runtime take to end the session before it abandons it:
    /// a hung compositor, or a headset unplugged mid-exit, must not stop pe-app from quitting (@design-reviewer's
    /// Low on slice 4a).
    pub exit_deadline: Duration,
}

impl XrTimes {
    pub const DEFAULT: XrTimes = XrTimes { idle: Duration::from_millis(100), exit_deadline: Duration::from_secs(5) };
}

/// The XR thread's loop (app.md 1): steps the session with the sink until it ends. A frame step is paced by the
/// runtime (`xrWaitFrame`), so the loop steps again at once; while no frame loop runs, it waits `times.idle` or until
/// unparked. Once `stop` is set it asks to leave VR once and keeps stepping until the session ends; pe-xr's `step`
/// then tears it down, the sink releasing the swapchain before the session goes. A failed request ends the session
/// at once (pe-xr), and a session the runtime has not ended `times.exit_deadline` after the request is abandoned.
/// A runtime call that never returns still blocks the loop.
pub fn run_xr<B: Backend>(
    session: &mut XrSession<B>,
    sink: &mut dyn FrameSink,
    stop: &AtomicBool,
    times: XrTimes,
) -> XrEnd {
    let mut frames = 0;
    let mut asked: Option<Instant> = None;
    loop {
        if asked.is_none() && stop.load(Ordering::Acquire) {
            asked = Some(Instant::now());
            // An error has already ended the session (pe-xr), so the next step tears it down.
            let _ = session.request_exit();
        }
        if asked.is_some_and(|t| t.elapsed() >= times.exit_deadline) {
            session.abandon();
        }
        match session.step(sink) {
            Step::Frame => frames += 1,
            Step::Idle => thread::park_timeout(times.idle),
            Step::Ended(reason) => return XrEnd { reason, frames },
        }
    }
}

/// The XR thread's session and sink (pe-xr's `VrParts` after `into_parts`). Rust drops the fields in declaration
/// order, so the session goes before the sink, whose renderer holds the GPU context (@xr-eng).
/// Dropping it first ends the session the way `step` would, so a frame that panics still has the sink release the
/// swapchain's images before the session and its swapchain go.
struct XrOwned<B: Backend, S: FrameSink> {
    session: XrSession<B>,
    sink: S,
}

impl<B: Backend, S: FrameSink> XrOwned<B, S> {
    /// Ends the session if it still runs and tears it down: the sink releases the images, then the session is
    /// destroyed. Nothing happens once `step` has torn it down.
    fn end(&mut self) {
        self.session.abandon();
        self.session.teardown(&mut self.sink);
    }
}

impl<B: Backend, S: FrameSink> Drop for XrOwned<B, S> {
    fn drop(&mut self) {
        self.end();
    }
}

/// What the XR thread did while pe-app waited for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XrWait {
    /// The session ended (the user left VR, or it was lost), and the thread has torn it down and dropped it,
    /// GPU context included: pe-app goes to the desktop (xr.md 3.1, EXITING and LOSS_PENDING).
    Ended(XrEnd),
    /// Still in VR when the wait ran out.
    Running,
    /// The thread panicked. Its session was torn down on the way out ([`XrOwned`]).
    Panicked,
}

/// The running XR thread.
pub struct XrThread {
    join: JoinHandle<XrEnd>,
    stop: Arc<AtomicBool>,
    /// The session's end, sent once the session and the sink are gone.
    done: Receiver<XrEnd>,
    /// How long [`Self::stop`] waits for the thread before leaving it behind: the exit deadline and a second.
    patience: Duration,
}

impl XrThread {
    /// The session has ended, or the thread panicked.
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    /// Waits up to `timeout` for the session to end without being asked to.
    pub fn wait(&self, timeout: Duration) -> XrWait {
        match self.done.recv_timeout(timeout) {
            Ok(end) => XrWait::Ended(end),
            Err(RecvTimeoutError::Timeout) => XrWait::Running,
            Err(RecvTimeoutError::Disconnected) => XrWait::Panicked,
        }
    }

    /// Leaves VR, and waits for the thread to end, for at most its patience. The exit deadline only holds between
    /// steps, and a runtime call that never returns (a hung compositor in `xrWaitFrame`) must not stop pe-app from
    /// quitting (@design-reviewer's Low on slice 4): past its patience the thread is left behind, and the process's
    /// exit ends it. `None` if the thread panicked or was left behind.
    pub fn stop(self) -> Option<XrEnd> {
        self.stop.store(true, Ordering::Release);
        self.join.thread().unpark();
        match self.done.recv_timeout(self.patience) {
            // Sent after the teardown, so the thread is returning.
            Ok(end) => {
                let _ = self.join.join();
                Some(end)
            }
            // `wait` took the end, or the thread panicked: either way it is returning.
            Err(RecvTimeoutError::Disconnected) => self.join.join().ok(),
            Err(RecvTimeoutError::Timeout) => None,
        }
    }
}

/// Starts the XR thread with the session and the sink. They end in OpenXR's order on every path, a panic or a
/// failed spawn included ([`XrOwned`]).
pub fn spawn_xr<B, S>(session: XrSession<B>, sink: S) -> std::io::Result<XrThread>
where
    B: Backend + Send + 'static,
    S: FrameSink + Send + 'static,
{
    let owned = XrOwned { session, sink };
    let times = XrTimes::DEFAULT;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let (sent, done) = mpsc::channel();
    let join = thread::Builder::new().name("pe-xr".into()).spawn(move || {
        let end = {
            let mut owned = owned;
            run_xr(&mut owned.session, &mut owned.sink, &flag, times)
        };
        // Sent after the session, the sink and the GPU context are gone, so the desktop's context is never made
        // while the XR one is still being destroyed (app.md 4 step 3).
        let _ = sent.send(end);
        end
    })?;
    Ok(XrThread { join, stop, done, patience: times.exit_deadline + Duration::from_secs(1) })
}

/// Why VR did not start. pe-app runs on the desktop instead, and says why once (xr.md 2).
#[derive(Debug)]
pub enum NotStarted {
    /// pe-xr did not open VR: no runtime, the headset guard, no headset, or a failure while opening.
    NotOpened(NotOpened),
    /// The XR thread could not be spawned. The session was torn down.
    Thread(std::io::Error),
}

impl fmt::Display for NotStarted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotStarted::NotOpened(why) => write!(f, "VR did not open: {why}"),
            NotStarted::Thread(e) => write!(f, "the XR thread did not start: {e}"),
        }
    }
}

/// Opens VR on the main thread and starts the XR thread drawing the frame slot's views (app.md 4 steps 3 and 6). The
/// XR context is made here, before any window's. `guard` is pe-xr's headset guard: [`Guard::from_env`] with VR
/// turned on by the configuration, or [`Guard::CLOSED`] in tests. `ring` carries the located hands to the
/// simulation thread.
/// What the headset's sink draws with: the presenter, the renderer's settings, where effects go, and
/// `view_capture`'s queue when an agent may capture (api.md 8).
pub struct Drawing {
    pub presenter: Presenter,
    pub render: RenderSettings,
    pub effects: Box<dyn EffectSink + Send>,
    pub captures: Option<Arc<pe_render::capture_service::CaptureQueue>>,
    /// Receives the presenter's counters and the display lead when the sink drops (the end-of-run line).
    pub report: ReportSlot,
    /// B (app.md 3, [`crate::shown`]): where the sink sends each frame's display lead under Shown.
    pub shown: Option<Arc<crate::shown::DisplayLead>>,
}

pub fn start(
    discovery: &dyn Discovery,
    guard: &Guard,
    ring: Arc<HandRing>,
    slot: Arc<FrameSlot>,
    drawing: Drawing,
) -> Result<XrThread, NotStarted> {
    let Drawing { presenter, render, effects, captures, report, shown } = drawing;
    let parts =
        open::open_vulkan(discovery, guard, GraspConfig::default(), ring).map_err(NotStarted::NotOpened)?.into_parts();
    // Moved out one by one, so on any unwind from here they drop in reverse, in OpenXR's order: the session, then the
    // swapchain's wrappers, then the device. The renderer builds on its own handles to the same device, so a panic
    // while it builds still leaves `gpu` to go last (@xr-eng's Low on slice 4).
    let gpu = parts.gpu;
    let targets = parts.targets;
    let session = parts.session;
    let renderer = Renderer::new(shared(&gpu), render);
    // The whole image until the render scale comes (@xr-eng's L2, with the 90 Hz pacing work).
    let mut sink = VrSink::new(slot, presenter, renderer, targets, effects, None, report);
    if let Some(lead) = shown {
        sink = sink.with_shown(lead);
    }
    if let Some(q) = captures {
        sink = sink.with_captures(q);
    }
    spawn_xr(session, sink).map_err(NotStarted::Thread)
}

/// More handles to the same device: wgpu's handles are reference-counted.
fn shared(gpu: &GpuContext) -> GpuContext {
    GpuContext {
        instance: gpu.instance.clone(),
        adapter: gpu.adapter.clone(),
        device: gpu.device.clone(),
        queue: gpu.queue.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    use pe_render::display::DisplaySettings;
    use pe_render::effects::{EffectCommand, EffectSettings};
    use pe_render::scene::Environment;
    use pe_seams::{Frame, FrameClock, HostTime, RigPose, SimRate};

    /// The VR sink's reading (render.md 3): every view published since the last frame, once and in order, with the
    /// newest for the cameras; an empty batch keeps the last view; nothing before the first view.
    #[test]
    fn the_sink_reads_every_view_once_and_keeps_the_last_for_an_empty_batch() {
        let clock = crate::clock::LocalClock::new(HostTime(0), SimRate::DEFAULT);
        let view = |f: u64| crate::standin::view_at(&clock as &dyn FrameClock, Frame(f));
        let slot = Arc::new(FrameSlot::new());
        let mut views = ViewReader::new(slot.clone());
        assert!(views.take().is_none(), "nothing before the first view");
        slot.publish(view(1));
        slot.publish(view(2));
        let (b, newest) = views.take().expect("views");
        assert_eq!(b.views.iter().map(|v| v.frame.0).collect::<Vec<_>>(), [1, 2], "both, in order");
        assert_eq!(newest.frame, Frame(2));
        let (b, newest) = views.take().expect("the last view");
        assert!(b.views.is_empty(), "taken once");
        assert_eq!(newest.frame, Frame(2), "an empty batch keeps the last view");
        slot.publish(view(3));
        let (b, newest) = views.take().expect("views");
        assert_eq!((b.first, b.views.len(), newest.frame), (2, 1, Frame(3)), "numbered on from the last batch");
    }
    use pe_xr::backend::{EyePose, Fov, FrameTiming, ImageIndex, XrError};
    use pe_xr::fake::{Call, FakeRuntime, Op};
    use pe_xr::loader::NoRuntime;
    use pe_xr::ring::DEFAULT_WINDOW_NS;

    /// Waits that never run out in a test: no idle wait, and no exit deadline unless a test sets one.
    const QUICK: XrTimes = XrTimes { idle: Duration::ZERO, exit_deadline: Duration::from_secs(3_600) };

    fn ring() -> Arc<HandRing> {
        Arc::new(HandRing::new(DEFAULT_WINDOW_NS, 11_111_111))
    }

    fn session(rt: FakeRuntime) -> XrSession<FakeRuntime> {
        XrSession::new(rt, ring(), GraspConfig::default())
    }

    /// The runtime's calls that show how the session ended: (xrRequestExitSession, xrEndSession,
    /// xrDestroySession). It also asserts the fake saw no broken rule.
    fn ending(s: &XrSession<FakeRuntime>) -> (usize, usize, usize) {
        let b = s.backend();
        assert!(b.violations.is_empty(), "{:?}", b.violations);
        (b.count(&Call::RequestExit), b.count(&Call::EndSession), b.count(&Call::DestroySession))
    }

    /// Counts what the session asks of it, and asks to leave VR after `stop_after` frames.
    struct Counting<'a> {
        rendered: u64,
        released: u32,
        stop_after: u64,
        stop: &'a AtomicBool,
    }

    impl FrameSink for Counting<'_> {
        fn render(&mut self, _: &FrameRequest<'_>) -> bool {
            self.rendered += 1;
            assert!(self.rendered < 1_000, "the loop never left VR");
            if self.rendered >= self.stop_after {
                self.stop.store(true, Ordering::Release);
            }
            true
        }
        fn release_swapchain(&mut self) {
            self.released += 1;
        }
    }

    /// The loop runs frames until asked to leave, then steps until the runtime ends the session: it ends `Exited`
    /// after at least the frames before the request, and the sink released the swapchain once. The runtime ended it
    /// (xrEndSession on STOPPING). A loop that never asks to leave fails on the sink's guard instead of hanging.
    #[test]
    fn the_xr_loop_leaves_vr_on_stop_and_releases_the_swapchain_once() {
        let mut session = session(FakeRuntime::new().started());
        let stop = AtomicBool::new(false);
        let mut sink = Counting { rendered: 0, released: 0, stop_after: 5, stop: &stop };
        let end = run_xr(&mut session, &mut sink, &stop, QUICK);
        assert_eq!(end.reason, EndReason::Exited);
        assert!(end.frames >= 5, "{end:?}");
        assert_eq!(sink.released, 1);
        assert!(sink.rendered >= 5);
        assert_eq!(ending(&session), (1, 1, 1));
    }

    /// A failed exit request still ends the loop (@design-reviewer's Medium on slice 4a): pe-xr ends the session on
    /// the error, so the next step tears it down. No xrEndSession: the runtime never stopped the session, which shows
    /// the injected failure was the way out.
    #[test]
    fn a_failed_exit_request_still_ends_the_loop_and_releases_the_swapchain_once() {
        let failing = XrError::Other("XR_ERROR_SESSION_NOT_RUNNING".into());
        let mut session = session(FakeRuntime::new().started().failing(Op::RequestExit, 5, failing));
        let stop = AtomicBool::new(false);
        let mut sink = Counting { rendered: 0, released: 0, stop_after: 5, stop: &stop };
        let end = run_xr(&mut session, &mut sink, &stop, QUICK);
        assert_eq!(end.reason, EndReason::Exited);
        assert_eq!(sink.released, 1);
        assert_eq!(ending(&session), (1, 0, 1));
    }

    /// A runtime that accepts the request but never ends the session is abandoned at the deadline. With a zero
    /// deadline the loop abandons it before it reads the runtime's STOPPING: no xrEndSession, one teardown.
    #[test]
    fn past_the_exit_deadline_the_loop_abandons_the_session() {
        let mut session = session(FakeRuntime::new().started());
        let stop = AtomicBool::new(false);
        let mut sink = Counting { rendered: 0, released: 0, stop_after: 5, stop: &stop };
        let end =
            run_xr(&mut session, &mut sink, &stop, XrTimes { idle: Duration::ZERO, exit_deadline: Duration::ZERO });
        assert_eq!(end.reason, EndReason::Exited);
        assert_eq!(sink.released, 1);
        assert_eq!(ending(&session), (1, 0, 1));
    }

    /// Holds the swapchain's images from its first frame until it releases them, as the renderer does. The fake
    /// records a violation if the swapchain is destroyed while they are held.
    struct Holding {
        held: Arc<AtomicBool>,
        released: u32,
    }

    impl FrameSink for Holding {
        fn render(&mut self, _: &FrameRequest<'_>) -> bool {
            self.held.store(true, Ordering::Relaxed);
            true
        }
        fn release_swapchain(&mut self) {
            self.held.store(false, Ordering::Relaxed);
            self.released += 1;
        }
    }

    /// What dropping the XR thread's parts does after a frame panicked, called directly so the fake can be read: a
    /// running session is ended and torn down with the images released first, once.
    #[test]
    fn ending_the_xr_parts_releases_the_images_before_the_session_goes() {
        let rt = FakeRuntime::new().started();
        let held = rt.images_held.clone();
        let mut owned = XrOwned { session: session(rt), sink: Holding { held, released: 0 } };
        for _ in 0..3 {
            assert_eq!(owned.session.step(&mut owned.sink), Step::Frame);
        }
        owned.end();
        owned.end();
        assert_eq!(owned.sink.released, 1);
        assert_eq!(ending(&owned.session), (0, 0, 1));
    }

    /// A frame that panics on the XR thread still releases the swapchain, once, on the way out.
    #[test]
    fn a_panicking_frame_still_releases_the_swapchain_once() {
        struct Panicking {
            frames: u32,
            released: Arc<AtomicU32>,
        }
        impl FrameSink for Panicking {
            fn render(&mut self, _: &FrameRequest<'_>) -> bool {
                self.frames += 1;
                assert!(self.frames < 3, "a frame panicked");
                true
            }
            fn release_swapchain(&mut self) {
                self.released.fetch_add(1, Ordering::Relaxed);
            }
        }
        let released = Arc::new(AtomicU32::new(0));
        let sink = Panicking { frames: 0, released: released.clone() };
        let owned = XrOwned { session: session(FakeRuntime::new().started()), sink };
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut owned = owned;
            run_xr(&mut owned.session, &mut owned.sink, &AtomicBool::new(false), QUICK)
        }));
        assert!(ran.is_err());
        assert_eq!(released.load(Ordering::Relaxed), 1);
    }

    /// `stop` leaves behind a thread that never ends, as when a runtime call hangs, once its patience runs out. The
    /// thread here is released after 2 s, so a `stop` that joined would take that long and get its end.
    #[test]
    fn stop_leaves_a_hung_xr_thread_behind() {
        let (release, hung) = mpsc::channel::<()>();
        let (sent, done) = mpsc::channel();
        let join = thread::spawn(move || {
            let _sent = sent;
            let _ = hung.recv();
            XrEnd { reason: EndReason::Exited, frames: 0 }
        });
        let xr = XrThread { join, stop: Arc::new(AtomicBool::new(false)), done, patience: Duration::from_millis(50) };
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            drop(release);
        });
        let asked = Instant::now();
        assert_eq!(xr.stop(), None);
        assert!(asked.elapsed() < Duration::from_secs(1), "{:?}", asked.elapsed());
        releaser.join().unwrap();
    }

    /// A thread that ends gives its end to `wait`, and `stop` then joins it and gives the same end.
    #[test]
    fn stop_joins_a_thread_that_ended() {
        let end = XrEnd { reason: EndReason::SessionLost, frames: 3 };
        let (sent, done) = mpsc::channel();
        let join = thread::spawn(move || {
            let _ = sent.send(end);
            end
        });
        let xr = XrThread { join, stop: Arc::new(AtomicBool::new(false)), done, patience: Duration::from_secs(5) };
        assert_eq!(xr.wait(Duration::from_secs(5)), XrWait::Ended(end));
        assert_eq!(xr.stop(), Some(end));
    }

    struct Quiet;

    impl EffectSink for Quiet {
        fn command(&mut self, _: &EffectCommand) {}
    }

    /// Without a runtime VR does not start, and nothing is spawned: pe-app goes to the desktop. Tests never use
    /// `SystemDiscovery`, since a runtime is registered on the machines we test on.
    #[test]
    fn without_a_runtime_vr_does_not_start() {
        let presenter = Presenter::new(
            DisplaySettings::default(),
            EffectSettings::default(),
            Environment::default(),
            SimRate::DEFAULT.hz(),
        );
        let slot = Arc::new(FrameSlot::new());
        let drawing = Drawing {
            presenter,
            render: RenderSettings::default(),
            effects: Box::new(Quiet),
            captures: None,
            report: ReportSlot::default(),
            shown: None,
        };
        match start(&NoRuntime, &Guard::CLOSED, ring(), slot, drawing) {
            Err(NotStarted::NotOpened(NotOpened::Runtime(_))) => {}
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("VR started without a runtime"),
        }
    }

    /// A capture is served from the frame that ended, once, and only when that frame was drawn: `render` notes the
    /// view, `after_frame` takes it (render.md 11).
    #[test]
    fn captures_are_served_after_the_frame_from_the_view_it_drew() {
        let clock = crate::clock::LocalClock::new(HostTime(0), SimRate::DEFAULT);
        let view = |f: u64| crate::standin::view_at(&clock as &dyn FrameClock, Frame(f));
        let rig = pe_seams::RigTransform { pos: pe_seams::Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 };
        let eye = |fov: f32| {
            let fov = pe_render::camera::Fov::symmetric(fov, 1.0);
            Camera::of_eye(&rig, &RigPose::IDENTITY, fov, pe_render::camera::DepthRange::VR)
        };
        let mut c =
            AfterFrame { server: CaptureServer::new(pe_render::capture_service::CaptureQueue::new()), drawn: None };
        assert!(c.ended(true).is_none(), "nothing drawn yet");

        c.drew(view(1), eye(1.0));
        c.drew(view(2), eye(2.0));
        let (v, cam) = c.ended(true).expect("the frame drawn last");
        assert_eq!((v.frame, cam), (Frame(2), eye(2.0)));
        assert!(c.ended(true).is_none(), "served once per drawn frame");

        c.drew(view(3), eye(3.0));
        assert!(c.ended(false).is_none(), "a frame that went out undrawn serves nothing");
        assert!(c.ended(true).is_none(), "and its view is not kept for a later frame");
    }

    #[test]
    fn the_local_presentation_takes_the_display_time_and_the_located_hands() {
        let eye = EyePose { pose: RigPose::IDENTITY, fov: Fov { left: -0.8, right: 0.8, up: 0.8, down: -0.8 } };
        let right = RigPose::IDENTITY;
        let mut joints = pe_render::HandJoints([pe_render::HandJoint::default(); pe_render::HandJoints::COUNT]);
        joints.0[0] = pe_render::HandJoint { position: [0.1, 1.0, -0.3], radius: 0.02, valid: true };
        let frame = FrameRequest {
            timing: FrameTiming {
                predicted_display_time: HostTime(7_000_000),
                predicted_display_period_ns: 11_111_111,
                should_render: true,
            },
            eyes: &[eye, eye],
            image: ImageIndex(0),
            depth_image: None,
            hands: [None, Some(right)],
            hand_joints: [None, Some(joints)],
        };
        let local = local_presentation(&frame, None);
        assert_eq!(local.display_time, HostTime(7_000_000));
        assert!(local.hands[0].is_none(), "an untracked hand is not drawn");
        assert_eq!(local.hands[1].map(|h| h.pose), Some(right));
        assert_eq!(local.hands[1].and_then(|h| h.joints), Some(joints), "a tracked hand's joints are drawn with it");
        // Present: the world at now, the hands still located for the predicted display time.
        let local = local_presentation(&frame, Some(HostTime(5_000_000)));
        assert_eq!(local.display_time, HostTime(5_000_000));
        assert_eq!(local.hands[1].map(|h| h.pose), Some(right));
    }
}
