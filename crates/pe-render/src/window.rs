//! The desktop window and its loop (render.md 9, 10): one winit window, paced by its present (`Fifo`), drawing the
//! newest frame view through the display layer, with the desktop hand.
//!
//! - **Threads:** winit's event loop runs on the thread that calls [`run_desktop`]; in desktop mode that thread is the
//!   render thread (render.md R5, WS-app's call). The simulation publishes into the [`FrameSlot`] from its own thread
//!   and never waits on this one.
//! - **Pacing:** the period is the monitor's reported refresh rate, else the measured present interval; the display
//!   time is the next present (now plus one period). The quality ladder runs on durations.
//! - **Minimized or occluded:** nothing is drawn; the simulation keeps running.
//! - **Camera (M1 fly controls):** right-drag turns the eye, WASD moves it (Space and C for up and down), both in the
//!   rig frame. Moving the rig itself is the simulation's, through an input entry, after M1 (render.md 9).
//! - **The render scale** is not applied on the desktop yet: the desktop ladder only records it. MSAA is applied.

use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::{DQuat, DVec3};
use pe_seams::{FrameView, HostTime, RigPose};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::camera::{Camera, DepthRange, Fov};
use crate::counters::RunReport;
use crate::desktop_hand::{DesktopHand, DesktopHandFeed, DesktopHandSettings, PointerInput, Target};
use crate::effects::EffectCommand;
use crate::gpu::{BackendOverride, ContextError, GpuContext, desktop_order};
use crate::labels::text_width;
use crate::notices::{Line, StripCache, window_overlay, window_text_px};
use crate::pacing::{DESKTOP_LADDER, Pacer, PacerSettings, PeriodSource};
use crate::pose::quat;
use crate::present::{FrameSlot, Presenter};
use crate::scene::{EyeView, RenderSettings, Renderer};
use crate::{DisplayHand, LocalPresentation};

/// Where sounds and haptics go (pe-app); pe-render plays particles itself.
pub trait EffectSink {
    fn command(&mut self, c: &EffectCommand);
}

/// The desktop's settings.
pub struct DesktopConfig {
    pub title: String,
    pub size: [u32; 2],
    /// `Fifo` (vsync) by default; `Mailbox` as a setting (render.md 10).
    pub present_mode: wgpu::PresentMode,
    pub vertical_fov: f32,
    /// The shared host clock (QPC on Windows), the one the simulation stamps `sim_time` with.
    pub clock: Arc<dyn Fn() -> HostTime + Send + Sync>,
    pub presenter: Presenter,
    pub render: RenderSettings,
    pub hand: DesktopHandSettings,
    /// The eye's starting pose in the rig frame.
    pub eye: RigPose,
    /// Close the window after this long (tests, demos); `None` runs until closed.
    pub run_for: Option<Duration>,
    /// Write the window's own frame to a PNG once, this long after the window opens: the swapchain image read back
    /// in-process before it is presented, so nothing outside pe-render's drawing can appear in it.
    pub snapshot: Option<(Duration, std::path::PathBuf)>,
    /// `view_capture`'s orders (api.md 8), served once a frame after the present, from the frame's view, with this
    /// window's camera as the player's.
    pub captures: Option<Arc<crate::capture_service::CaptureQueue>>,
    /// The menus (render.md 8.2, `crate::menu`): aimed with the mouse, toggled with Tab. A left press that lands on a
    /// panel goes to it, not to the hand.
    pub ui: Option<Box<dyn crate::menu::Ui>>,
}

/// What a run measured (render.md 13's per-frame numbers, summarized).
#[derive(Clone, Debug, Default)]
pub struct DesktopReport {
    pub adapter: String,
    /// The window's setup, milliseconds: its surface and context, and the renderer's pipelines (FXC compiles them on
    /// D3D12). 0 for a view on a context the caller made.
    pub setup_ms: f64,
    pub frames: u64,
    /// Present intervals, milliseconds.
    pub intervals_ms: Vec<f64>,
    pub cpu_record_ms: Vec<f64>,
    /// GPU time per frame, from timestamp queries (empty on an adapter without them).
    pub gpu_ms: Vec<f64>,
    pub skipped: u64,
    pub quality_changes: u64,
    pub monitor_mhz: Option<u32>,
    /// The snapshot's outcome, when one was asked for.
    pub snapshot: Option<Result<std::path::PathBuf, String>>,
    /// The presenter's counters for the whole run (render.md 13), set when the window closes; None before.
    pub render: Option<RunReport>,
}

/// A lost surface is made again on the same window and device (review L3). False if that fails, so the caller backs
/// off instead of retrying at once.
fn recreate_surface(g: &mut Gfx) -> bool {
    let made = {
        let _one_at_a_time = crate::gpu::creation_lock();
        g.renderer.context().instance.create_surface(g.window.clone())
    };
    match made {
        Ok(s) => {
            // Drop the old surface and its swapchain before configuring the new one: on Vulkan, two swapchains on one
            // window can fail with VK_ERROR_NATIVE_WINDOW_IN_USE_KHR (recheck nit, part 2a).
            drop(std::mem::replace(&mut g.surface, s));
            g.surface.configure(&g.renderer.context().device, &g.config);
            true
        }
        Err(_) => false,
    }
}

/// Read the swapchain image back before present and write it as an RGBA PNG.
fn snapshot(
    g: &Gfx,
    tex: &wgpu::Texture,
    size: [u32; 2],
    path: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    if !g.config.usage.contains(wgpu::TextureUsages::COPY_SRC) {
        return Err("the surface does not allow copies".into());
    }
    use wgpu::TextureFormat as F;
    if !matches!(g.config.format, F::Bgra8Unorm | F::Bgra8UnormSrgb | F::Rgba8Unorm | F::Rgba8UnormSrgb) {
        return Err(format!("snapshots read 8-bit RGBA or BGRA only, not {:?}", g.config.format));
    }
    let ctx = g.renderer.context();
    let row = (size[0] * 4).div_ceil(256) * 256;
    let buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("window snapshot"),
        size: row as u64 * size[1] as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut enc = ctx.device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(size[1]) },
        },
        wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
    );
    ctx.queue.submit([enc.finish()]);
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    ctx.device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(Duration::from_secs(5)) })
        .map_err(|e| e.to_string())?;
    let data = buf.slice(..).get_mapped_range().map_err(|e| format!("{e:?}"))?;
    let bgra = matches!(g.config.format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
    let mut rgba = Vec::with_capacity((size[0] * size[1] * 4) as usize);
    for y in 0..size[1] as usize {
        for px in data[y * row as usize..y * row as usize + size[0] as usize * 4].chunks(4) {
            if bgra {
                rgba.extend_from_slice(&[px[2], px[1], px[0], 255])
            } else {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255])
            }
        }
    }
    drop(data);
    buf.unmap();
    let img = crate::capture::Image { width: size[0], height: size[1], rgba };
    std::fs::write(path, img.encode_png()).map_err(|e| e.to_string())?;
    Ok(path.to_path_buf())
}

#[derive(Default)]
struct Pointer {
    pos: Option<[f64; 2]>,
    left: bool,
    right: bool,
    shift: bool,
    wheel: f64,
    last_drag: Option<[f64; 2]>,
    keys: [bool; 8], // W A S D Space C Q E
    /// H, held: the controls show.
    help: bool,
    /// Tab was pressed since the last frame: the menu toggles.
    tab: bool,
    /// Left presses since the last frame.
    clicks: u32,
}

/// The desktop controls (render.md 9), two lines at the bottom of the window: shown for [`HELP_S`] seconds of the
/// world drawn with no status line up (a joiner's catch-up doesn't use them up), and whenever H is held.
pub const HELP_LINES: [&str; 2] = [
    "Point: mouse  ·  Grab: hold the left button, let go to throw  ·  While holding, nearer / farther: wheel  ·  Push: Shift",
    "Look: right-drag  ·  Move: W A S D, Space up, C down  ·  Roll: Q / E  ·  Hold H to show these again",
];
pub const HELP_S: f64 = 30.0;
/// The controls' fade at the end of [`HELP_S`], seconds.
const HELP_FADE_S: f64 = 1.0;

/// The lines the window shows over the scene, top first: the status line, the controls, then the notices.
pub fn window_lines(status: Option<&str>, help_alpha: f32, notices: &[Line]) -> Vec<Line> {
    let status = status.map(|t| Line { text: t.to_string(), alpha: 1.0 });
    let help = (help_alpha > 0.0).then(|| HELP_LINES.map(|t| Line { text: t.to_string(), alpha: help_alpha }));
    status.into_iter().chain(help.into_iter().flatten()).chain(notices.iter().cloned()).collect()
}

/// The controls' alpha after `shown_s` seconds of them shown, H held or not.
pub fn help_alpha(shown_s: f64, held: bool) -> f32 {
    if held { 1.0 } else { ((HELP_S - shown_s) / HELP_FADE_S).clamp(0.0, 1.0) as f32 }
}

/// The text size for `lines` in a window `width` pixels wide: [`window_text_px`]'s, smaller if the widest line would
/// not fit in 94 % of the width.
pub fn fitted_text_px(lines: &[Line], window: [u32; 2]) -> f32 {
    let px = window_text_px(window[1]);
    let widest = lines.iter().map(|l| text_width(&l.text, px)).fold(0.0, f32::max);
    let room = 0.94 * window[0] as f32 - 0.8 * px;
    if widest > room && widest > 0.0 { (px * room / widest).max(8.0) } else { px }
}

struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    format: wgpu::TextureFormat,
    renderer: Renderer,
}

/// The desktop view: pe-app's main thread owns the winit window and its event loop, and forwards the window's
/// events here (app.md, WS-app). [`run_desktop`] is the same view with its own event loop, for demos and tests.
pub struct DesktopView {
    cfg: DesktopConfig,
    slot: Arc<FrameSlot>,
    feed: Arc<DesktopHandFeed>,
    sink: Box<dyn EffectSink>,
    gfx: Option<Gfx>,
    pacer: Pacer,
    hand: DesktopHand,
    pointer: Pointer,
    yaw: f64,
    pitch: f64,
    eye_pos: DVec3,
    /// `Occluded(true)` from the window system.
    occluded: bool,
    /// A zero-sized window (minimized), kept apart from `occluded` (review L5).
    minimized: bool,
    /// After a failed acquire, no redraw is asked for before this.
    backoff_until: Option<Instant>,
    started: Instant,
    last_present: Option<Instant>,
    last_frame: Option<HostTime>,
    /// The newest view taken, drawn again when no new one was published.
    last_view: Option<Arc<FrameView>>,
    /// `view_capture`'s server, when the config gives a queue.
    captures: Option<crate::capture_service::CaptureServer>,
    targets: Vec<Target>,
    /// The notices strip's image (render.md 8.1).
    strip: StripCache,
    /// Seconds of the controls shown with no status line up ([`HELP_S`]).
    help_shown_s: f64,
    report: DesktopReport,
}

/// A desktop-only window's surface and a GPU context that can present to it (render.md 4): each backend of
/// [`desktop_order`] (D3D12, then Vulkan, on Windows; or the one [`crate::gpu::BACKEND_VAR`] forces) in an instance of
/// its own, hardware first, then software. A backend whose turn never comes is never loaded. A window that mirrors
/// VR draws on pe-xr's context instead ([`DesktopView::on`]). The instance, adapter and device are made under the
/// creation lock.
pub fn surface_and_context(window: Arc<Window>) -> Result<(wgpu::Surface<'static>, GpuContext), ContextError> {
    let order = desktop_order(BackendOverride::from_env()?);
    let mut failed = Vec::new();
    for software in [false, true] {
        for &backends in &order {
            match surface_and_context_on(window.clone(), backends, software) {
                Ok(made) => return Ok(made),
                Err(e) => failed.push((backends, software, e)),
            }
        }
    }
    Err(attempts_failed(failed))
}

/// Every failed attempt's reason in one error, so a machine that fails everywhere still reports why hardware D3D12
/// failed (@shader-reviewer, on review of the backend order). `NoBackend` when no attempt had a backend built here,
/// `NoAdapter` when none found an adapter (some may have had no backend).
fn attempts_failed(failed: Vec<(wgpu::Backends, bool, ContextError)>) -> ContextError {
    // No backend built here for any attempt (macOS): NoBackend; no adapter on any (some maybe no backend): NoAdapter.
    if !failed.is_empty()
        && let Some(all) = failed.iter().try_fold(wgpu::Backends::empty(), |all, (_, _, e)| match e {
            ContextError::NoBackend(b) => Some(all | *b),
            _ => None,
        })
    {
        return ContextError::NoBackend(all);
    }
    if failed.iter().all(|(_, _, e)| matches!(e, ContextError::NoAdapter | ContextError::NoBackend(_))) {
        return ContextError::NoAdapter;
    }
    let each: Vec<String> = failed
        .iter()
        .map(|(b, software, e)| {
            let name = match *b {
                wgpu::Backends::DX12 => "D3D12".to_string(),
                wgpu::Backends::VULKAN => "Vulkan".to_string(),
                other => format!("{other:?}"),
            };
            format!("{name} {}: {e}", if *software { "software" } else { "hardware" })
        })
        .collect();
    ContextError::Device(format!("no desktop context; {}", each.join("; ")))
}

fn surface_and_context_on(
    window: Arc<Window>,
    backends: wgpu::Backends,
    software: bool,
) -> Result<(wgpu::Surface<'static>, GpuContext), ContextError> {
    let (instance, surface) = {
        let _one_at_a_time = crate::gpu::creation_lock();
        let instance = crate::gpu::instance(backends)?;
        let surface = instance.create_surface(window).map_err(|e| ContextError::Device(e.to_string()))?;
        (instance, surface)
    };
    let ctx = GpuContext::desktop(instance, Some(&surface), software)?;
    Ok((surface, ctx))
}

impl DesktopView {
    /// The view on `window`: its surface and context (see [`surface_and_context`]), configured for `cfg`.
    pub fn new(
        window: Arc<Window>,
        cfg: DesktopConfig,
        slot: Arc<FrameSlot>,
        feed: Arc<DesktopHandFeed>,
        sink: Box<dyn EffectSink>,
    ) -> Result<DesktopView, ContextError> {
        let t0 = Instant::now();
        let (surface, ctx) = surface_and_context(window.clone())?;
        let mut view = Self::on(window, surface, ctx, cfg, slot, feed, sink)?;
        view.report.setup_ms = t0.elapsed().as_secs_f64() * 1e3;
        Ok(view)
    }

    /// The view on a surface and context the caller made (one at a time, under [`crate::gpu::creation_lock`]).
    pub fn on(
        window: Arc<Window>,
        surface: wgpu::Surface<'static>,
        ctx: GpuContext,
        cfg: DesktopConfig,
        slot: Arc<FrameSlot>,
        feed: Arc<DesktopHandFeed>,
        sink: Box<dyn EffectSink>,
    ) -> Result<DesktopView, ContextError> {
        let report = DesktopReport { adapter: ctx.describe(), ..Default::default() };
        let size = window.inner_size();
        let Some(mut config) = surface.get_default_config(&ctx.adapter, size.width.max(1), size.height.max(1)) else {
            return Err(ContextError::Device("the surface is not supported by the adapter".into()));
        };
        config.present_mode = cfg.present_mode;
        config.desired_maximum_frame_latency = 2;
        if cfg.snapshot.is_some()
            && surface.get_capabilities(&ctx.adapter).usages.contains(wgpu::TextureUsages::COPY_SRC)
        {
            config.usage |= wgpu::TextureUsages::COPY_SRC;
        }
        // Draw through an sRGB view, so the window reads as the capture and the headset do.
        let format = config.format.add_srgb_suffix();
        if format != config.format {
            config.view_formats = vec![format];
        }
        surface.configure(&ctx.device, &config);
        let renderer = Renderer::new(ctx, cfg.render);
        let f = quat(cfg.eye.orientation) * DVec3::NEG_Z;
        // The inverse of eye_rot: forward is (-sin yaw cos pitch, sin pitch, -cos yaw cos pitch).
        let (yaw, pitch) = ((-f.x).atan2(-f.z), f.y.clamp(-1.0, 1.0).asin());
        let eye_pos = DVec3::new(cfg.eye.position[0] as f64, cfg.eye.position[1] as f64, cfg.eye.position[2] as f64);
        let captures = cfg.captures.clone().map(crate::capture_service::CaptureServer::new);
        Ok(DesktopView {
            pacer: Pacer::new(PacerSettings::default(), &DESKTOP_LADDER),
            hand: DesktopHand::new(cfg.hand),
            cfg,
            slot,
            feed,
            sink,
            gfx: Some(Gfx { window, surface, config, format, renderer }),
            pointer: Pointer::default(),
            yaw,
            pitch,
            eye_pos,
            occluded: false,
            minimized: false,
            backoff_until: None,
            started: Instant::now(),
            last_present: None,
            last_frame: None,
            last_view: None,
            captures,
            targets: Vec::new(),
            strip: StripCache::default(),
            help_shown_s: 0.0,
            report,
        })
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.gfx.as_ref().expect("made in new").window
    }

    pub fn report(&self) -> &DesktopReport {
        &self.report
    }

    /// Ask winit for the next frame. [`DesktopView::schedule`] calls it when there is a frame to draw.
    pub fn request_redraw(&self) {
        self.window().request_redraw();
    }

    /// Call from the event loop's `about_to_wait`; pe-app's loop and [`run_desktop`]'s both do. It sets the control
    /// flow, and requests a redraw only when it will draw (review L1, part 2a, with @engine-architect):
    /// - a frame to draw: a redraw and `Poll`; the swapchain acquire paces the loop;
    /// - minimized or occluded: `Wait` (until `run_for` ends, if set); the `Resized` or `Occluded(false)` that ends
    ///   the state wakes the loop;
    /// - before the first frame view, or backing off after a failed acquire: a wait of one display period, since a
    ///   publish doesn't wake winit;
    /// - `run_for` passed: exit.
    pub fn about_to_wait(&self, el: &ActiveEventLoop) {
        if self.done() {
            el.exit();
            return;
        }
        let now = Instant::now();
        let flow = if self.occluded || self.minimized {
            match self.cfg.run_for {
                Some(d) => ControlFlow::WaitUntil(self.started + d),
                None => ControlFlow::Wait,
            }
        } else if self.slot.newest().is_none() {
            ControlFlow::WaitUntil(now + self.pacer.period())
        } else if let Some(t) = self.backoff_until.filter(|t| *t > now) {
            ControlFlow::WaitUntil(t)
        } else {
            self.request_redraw();
            ControlFlow::Poll
        };
        el.set_control_flow(flow);
    }

    /// `run_for` has passed.
    pub fn done(&self) -> bool {
        self.cfg.run_for.is_some_and(|d| self.started.elapsed() >= d)
    }

    fn eye_rot(&self) -> DQuat {
        DQuat::from_rotation_y(self.yaw) * DQuat::from_rotation_x(self.pitch)
    }

    fn eye_pose(&self) -> RigPose {
        let q = self.eye_rot();
        RigPose {
            position: [self.eye_pos.x as f32, self.eye_pos.y as f32, self.eye_pos.z as f32],
            orientation: [q.x as f32, q.y as f32, q.z as f32, q.w as f32],
        }
    }

    fn move_eye(&mut self, dt: f64) {
        let k = &self.pointer.keys;
        let f = self.eye_rot() * DVec3::NEG_Z;
        let flat = DVec3::new(f.x, 0.0, f.z).normalize_or(DVec3::NEG_Z);
        let right = flat.cross(DVec3::Y);
        let mut v = DVec3::ZERO;
        v += flat * ((k[0] as i32 - k[2] as i32) as f64);
        v += right * ((k[3] as i32 - k[1] as i32) as f64);
        v += DVec3::Y * ((k[4] as i32 - k[5] as i32) as f64);
        self.eye_pos += v.normalize_or_zero() * 1.5 * dt;
    }

    fn draw(&mut self) {
        if self.slot.newest().is_none() {
            return;
        }
        let Some(g) = self.gfx.as_mut() else { return };
        // Acquire first. The acquire waits for vsync, so the clock and the frame view are sampled after it, as fresh
        // as they can be (review L2, part 2a), and the wait is never counted as the frame's cost (b06783b4). A
        // failed acquire skips the frame before the display layer or the effects move.
        let tex = match g.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            failed => {
                self.report.skipped += 1;
                let retry_now = match failed {
                    wgpu::CurrentSurfaceTexture::Outdated => {
                        g.surface.configure(&g.renderer.context().device, &g.config);
                        true
                    }
                    wgpu::CurrentSurfaceTexture::Lost => recreate_surface(g),
                    _ => false,
                };
                if !retry_now {
                    self.backoff_until = Some(Instant::now() + self.pacer.period());
                }
                return;
            }
        };
        let t0 = Instant::now();
        // Every view published since the last frame (render.md 3); the camera and the hand use the newest.
        let batch = self.slot.take();
        let Some(view) = batch.views.last().cloned().or_else(|| self.last_view.clone()) else { return };
        self.last_view = Some(view.clone());
        let g = self.gfx.as_ref().expect("checked above");
        let mhz = g.window.current_monitor().and_then(|m| m.refresh_rate_millihertz());
        let size = [g.config.width, g.config.height];
        let now = (self.cfg.clock)();
        let dt = self.last_frame.map_or(0.0, |t| now.secs_since(t)).clamp(0.0, 0.1);
        self.last_frame = Some(now);
        self.report.monitor_mhz = mhz;
        self.pacer.update_period(mhz.map_or(PeriodSource::Measured, PeriodSource::Monitor));
        let display_time = self.pacer.desktop_display_time(now);

        // The eye, and the camera through it.
        self.move_eye(dt);
        let aspect = size[0] as f32 / size[1].max(1) as f32;
        let fov = Fov::symmetric(self.cfg.vertical_fov, aspect);
        let camera = Camera::of_eye(&view.local_rig, &self.eye_pose(), fov, DepthRange::DESKTOP);

        // The desktop hand: the ray from the camera through the cursor.
        let (nx, ny) = self.pointer.pos.map_or((0.0, 0.0), |[x, y]| {
            (x / size[0].max(1) as f64 * 2.0 - 1.0, 1.0 - y / size[1].max(1) as f64 * 2.0)
        });
        let (tx, ty) = ((fov.right as f64).tan(), (fov.up as f64).tan());
        let dir = camera.pose.rot * DVec3::new(nx * tx, ny * ty, -1.0).normalize();
        let roll = (self.pointer.keys[7] as i32 - self.pointer.keys[6] as i32) as f64;
        // The menus first: a press that lands on a panel is theirs, and the hand doesn't see it.
        let ui_out = self.cfg.ui.as_mut().map(|ui| {
            ui.frame(&crate::menu::UiInput {
                now,
                head: camera.pose,
                scale: f64::from(view.local_rig.scale),
                rays: vec![crate::menu::UiRay {
                    pointer: 2,
                    origin: camera.pose.pos,
                    dir,
                    pressed: self.pointer.left,
                    clicks: self.pointer.clicks,
                }],
                toggled: std::mem::take(&mut self.pointer.tab),
                wrist: None,
                draw_rays: false,
            })
        });
        self.pointer.clicks = 0;
        let ui_took = ui_out.as_ref().is_some_and(|o| o.took[2]);
        let input = PointerInput {
            at: now,
            origin: camera.pose.pos,
            dir,
            left: self.pointer.left && !ui_took,
            shift: self.pointer.shift,
            wheel: std::mem::take(&mut self.pointer.wheel),
            roll,
        };
        let (_, hand) = self.hand.update(&input, &self.targets, &view.local_rig);
        self.feed.push(hand);
        let local =
            LocalPresentation { display_time, hands: [None, Some(DisplayHand { pose: hand.pose, joints: None })] };

        let Some(mut frame) = self.cfg.presenter.frame_batch(&batch, &local, &[camera]) else { return };
        if frame.status.is_none() {
            self.help_shown_s += dt;
        }
        let lines =
            window_lines(frame.status.as_deref(), help_alpha(self.help_shown_s, self.pointer.help), &frame.notices);
        // The menu first, then the status, help and notices strip, so an open menu never hides a notice (render.md 8.2).
        if let Some(o) = ui_out {
            frame.scene.overlays.splice(0..0, o.overlays);
        }
        frame
            .scene
            .overlays
            .extend(self.strip.get(&lines, fitted_text_px(&lines, size)).map(|img| window_overlay(&camera, size, img)));
        for c in &frame.effects {
            self.sink.command(c);
        }
        // The hand hits what is drawn, never its own hand or what it holds.
        self.targets =
            frame.display.objects.iter().filter_map(|d| Target::of_object(view.object(d.id)?, d.pose)).collect();

        let g = self.gfx.as_mut().expect("checked above");
        let color =
            tex.texture.create_view(&wgpu::TextureViewDescriptor { format: Some(g.format), ..Default::default() });
        let stats = g.renderer.render_views(&frame.scene, &[EyeView { camera, color: &color, format: g.format, size }]);
        // The frame's cost: everything after the acquire, up to the submit (not the snapshot, a test aid).
        let cpu = t0.elapsed();
        if let Some((at, path)) = self.cfg.snapshot.clone()
            && self.started.elapsed() >= at
        {
            self.cfg.snapshot = None;
            self.report.snapshot = Some(snapshot(g, &tex.texture, size, &path));
        }
        g.window.pre_present_notify();
        g.renderer.context().queue.present(tex);
        if let Some(c) = self.captures.as_mut() {
            let at = (self.cfg.clock)();
            for e in c.serve(&mut g.renderer, &view, Some(&camera)) {
                self.cfg.presenter.capture_event(at, e);
            }
        }
        self.report.frames += 1;
        self.report.cpu_record_ms.push(stats.cpu_record_ms);
        if let Some(ms) = stats.gpu_ms {
            self.report.gpu_ms.push(ms);
        }
        let p = Instant::now();
        if let Some(l) = self.last_present {
            self.report.intervals_ms.push((p - l).as_secs_f64() * 1e3);
        }
        self.last_present = Some(p);
        let done = (self.cfg.clock)();
        self.cfg.presenter.drawn(done, &stats, self.pacer.quality());
        self.pacer.presented(done);
        if let Some(q) = self.pacer.frame_done(done, cpu, stats.gpu_ms.map(|ms| Duration::from_secs_f64(ms / 1e3))) {
            self.report.quality_changes += 1;
            g.renderer.set_msaa(q.msaa);
        }
    }

    /// One window event. Returns true when the window should close (closed, or `run_for` passed).
    pub fn window_event(&mut self, event: &WindowEvent) -> bool {
        match *event {
            WindowEvent::CloseRequested => return true,
            WindowEvent::Occluded(o) => self.occluded = o,
            WindowEvent::Resized(s) => {
                if let Some(g) = self.gfx.as_mut() {
                    self.minimized = s.width == 0 || s.height == 0;
                    if !self.minimized {
                        g.config.width = s.width;
                        g.config.height = s.height;
                        g.surface.configure(&g.renderer.context().device, &g.config);
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let p = [position.x, position.y];
                if self.pointer.right
                    && let Some(l) = self.pointer.last_drag
                {
                    self.yaw -= (p[0] - l[0]) * 0.005;
                    self.pitch = (self.pitch - (p[1] - l[1]) * 0.005).clamp(-1.5, 1.5);
                }
                self.pointer.last_drag = self.pointer.right.then_some(p);
                self.pointer.pos = Some(p);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let down = state == ElementState::Pressed;
                match button {
                    MouseButton::Left => {
                        if down && !self.pointer.left {
                            self.pointer.clicks += 1;
                        }
                        self.pointer.left = down;
                    }
                    MouseButton::Right => {
                        self.pointer.right = down;
                        self.pointer.last_drag = if down { self.pointer.pos } else { None };
                    }
                    _ => {}
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.pointer.wheel += match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 40.0,
                };
            }
            WindowEvent::ModifiersChanged(ref m) => self.pointer.shift = m.state().shift_key(),
            WindowEvent::KeyboardInput { ref event, .. } => {
                let down = event.state == ElementState::Pressed;
                if event.physical_key == PhysicalKey::Code(KeyCode::KeyH) {
                    self.pointer.help = down;
                    return false;
                }
                if event.physical_key == PhysicalKey::Code(KeyCode::Tab) {
                    self.pointer.tab |= down && !event.repeat;
                    return false;
                }
                let i = match event.physical_key {
                    PhysicalKey::Code(KeyCode::KeyW) => 0,
                    PhysicalKey::Code(KeyCode::KeyA) => 1,
                    PhysicalKey::Code(KeyCode::KeyS) => 2,
                    PhysicalKey::Code(KeyCode::KeyD) => 3,
                    PhysicalKey::Code(KeyCode::Space) => 4,
                    PhysicalKey::Code(KeyCode::KeyC) => 5,
                    PhysicalKey::Code(KeyCode::KeyQ) => 6,
                    PhysicalKey::Code(KeyCode::KeyE) => 7,
                    _ => return false,
                };
                self.pointer.keys[i] = down;
            }
            WindowEvent::RedrawRequested => {
                if !self.occluded && !self.minimized {
                    self.draw();
                }
                if self.cfg.run_for.is_some_and(|d| self.started.elapsed() >= d) {
                    return true;
                }
            }
            _ => {}
        }
        false
    }
}

/// What a [`DesktopView`] is made from, held until the window exists.
type Parts = (DesktopConfig, Arc<FrameSlot>, Arc<DesktopHandFeed>, Box<dyn EffectSink>);

/// [`run_desktop`]'s own event loop around a [`DesktopView`].
struct Standalone {
    parts: Option<Parts>,
    view: Option<DesktopView>,
    error: Option<ContextError>,
}

impl ApplicationHandler for Standalone {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let Some((cfg, slot, feed, sink)) = self.parts.take() else { return };
        let attrs = Window::default_attributes()
            .with_title(cfg.title.clone())
            .with_inner_size(winit::dpi::PhysicalSize::new(cfg.size[0], cfg.size[1]));
        let made = el
            .create_window(attrs)
            .map_err(|e| ContextError::Device(e.to_string()))
            .and_then(|w| DesktopView::new(Arc::new(w), cfg, slot, feed, sink));
        match made {
            Ok(v) => self.view = Some(v),
            Err(e) => {
                self.error = Some(e);
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let Some(v) = self.view.as_mut()
            && v.window_event(&event)
        {
            el.exit();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if let Some(v) = &self.view {
            v.about_to_wait(el);
        }
    }
}

/// Open the window on this thread and run until it closes (or `run_for` passes). Needs a desktop session.
pub fn run_desktop(
    cfg: DesktopConfig,
    slot: Arc<FrameSlot>,
    feed: Arc<DesktopHandFeed>,
    sink: Box<dyn EffectSink>,
) -> Result<DesktopReport, ContextError> {
    let el = EventLoop::new().map_err(|e| ContextError::Device(format!("event loop: {e}")))?;
    let mut app = Standalone { parts: Some((cfg, slot, feed, sink)), view: None, error: None };
    el.run_app(&mut app).map_err(|e| ContextError::Device(format!("event loop: {e}")))?;
    match (app.error, app.view) {
        (Some(e), _) => Err(e),
        (None, Some(mut v)) => {
            v.report.render = Some(v.cfg.presenter.counters().run());
            Ok(v.report)
        }
        (None, None) => Err(ContextError::Device("the window was never made".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_shows_the_status_then_the_controls_then_the_notices() {
        let notice = [Line { text: "Out of reach.".into(), alpha: 0.5 }];
        let texts = |l: Vec<Line>| l.into_iter().map(|l| l.text).collect::<Vec<_>>();
        let all = window_lines(Some("Joining: catching up…"), 1.0, &notice);
        assert_eq!(texts(all), ["Joining: catching up…", HELP_LINES[0], HELP_LINES[1], "Out of reach."]);
        assert_eq!(texts(window_lines(None, 0.0, &notice)), ["Out of reach."], "the controls gone");
        assert!(window_lines(None, 0.0, &[]).is_empty());
    }

    #[test]
    fn the_controls_show_for_their_time_then_fade_and_come_back_with_h() {
        assert_eq!(help_alpha(0.0, false), 1.0);
        assert_eq!(help_alpha(HELP_S - HELP_FADE_S, false), 1.0);
        assert!((help_alpha(HELP_S - HELP_FADE_S / 2.0, false) - 0.5).abs() < 1e-6, "fading");
        assert_eq!(help_alpha(HELP_S, false), 0.0);
        assert_eq!(help_alpha(1e6, true), 1.0, "H held");
    }

    #[test]
    fn the_controls_fit_a_narrow_window() {
        let lines = window_lines(None, 1.0, &[]);
        let wide = [3840, 1080];
        assert_eq!(fitted_text_px(&lines, wide), window_text_px(1080), "room to spare: the usual size");
        assert_eq!(fitted_text_px(&lines, [1280, 720]), window_text_px(720), "pe-app's window: the usual size");
        assert!(fitted_text_px(&lines, [640, 720]) < window_text_px(720), "half as wide: smaller text");
        for w in [640, 1280] {
            let px = fitted_text_px(&lines, [w, 720]);
            let widest = lines.iter().map(|l| text_width(&l.text, px)).fold(0.0, f32::max);
            assert!(widest + 0.8 * px <= 0.94 * w as f32 + 0.5, "{w} px wide: {widest} fits");
        }
        assert!(ab_glyph::Font::glyph_id(crate::labels::font(), '·').0 != 0, "the font has the separator");
    }

    #[test]
    fn a_failure_everywhere_keeps_every_attempts_reason() {
        let hw = |b, e| (b, false, e);
        let failed = vec![
            hw(wgpu::Backends::DX12, ContextError::Device("adapter lost".into())),
            hw(wgpu::Backends::VULKAN, ContextError::NoAdapter),
            (wgpu::Backends::DX12, true, ContextError::NoAdapter),
            (wgpu::Backends::VULKAN, true, ContextError::NoAdapter),
        ];
        let ContextError::Device(m) = attempts_failed(failed) else { panic!("a device error expected") };
        assert!(m.contains("D3D12 hardware: device: adapter lost"), "{m}");
        assert!(m.contains("Vulkan hardware: no GPU adapter"), "{m}");
        assert!(m.contains("Vulkan software: no GPU adapter"), "{m}");
        // Nothing found anywhere is still NoAdapter, which a capture reports as unavailable.
        let none = vec![
            hw(wgpu::Backends::DX12, ContextError::NoAdapter),
            hw(wgpu::Backends::VULKAN, ContextError::NoAdapter),
        ];
        assert_eq!(attempts_failed(none), ContextError::NoAdapter);
        // No backend built for any attempt (macOS) keeps its kind; mixed with no adapter, it is no adapter.
        let unbuilt = vec![hw(wgpu::Backends::VULKAN, ContextError::NoBackend(wgpu::Backends::VULKAN))];
        assert_eq!(attempts_failed(unbuilt), ContextError::NoBackend(wgpu::Backends::VULKAN));
        let mixed = vec![
            hw(wgpu::Backends::DX12, ContextError::NoAdapter),
            hw(wgpu::Backends::VULKAN, ContextError::NoBackend(wgpu::Backends::VULKAN)),
        ];
        assert_eq!(attempts_failed(mixed), ContextError::NoAdapter);
    }
}
