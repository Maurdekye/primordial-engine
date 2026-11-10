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
use pe_seams::{HostTime, RigPose, ViewFlags};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::camera::{Camera, DepthRange, Fov};
use crate::desktop_hand::{DesktopHand, DesktopHandFeed, DesktopHandSettings, PointerInput, Target};
use crate::effects::EffectCommand;
use crate::gpu::{ContextError, GpuContext, backends, instance_descriptor};
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
}

/// What a run measured (render.md 13's per-frame numbers, summarized).
#[derive(Clone, Debug, Default)]
pub struct DesktopReport {
    pub adapter: String,
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
    occluded: bool,
    started: Instant,
    last_present: Option<Instant>,
    last_frame: Option<HostTime>,
    targets: Vec<Target>,
    report: DesktopReport,
}

/// The window's surface and a GPU context that can present to it: Vulkan first, then D3D12 (render.md 4). The
/// instance, adapter and device are made under the creation lock.
pub fn surface_and_context(window: Arc<Window>) -> Result<(wgpu::Surface<'static>, GpuContext), ContextError> {
    let (instance, surface) = {
        let _one_at_a_time = crate::gpu::creation_lock();
        let instance = wgpu::Instance::new(instance_descriptor(backends()));
        let surface = instance.create_surface(window).map_err(|e| ContextError::Device(e.to_string()))?;
        (instance, surface)
    };
    let ctx = GpuContext::desktop(instance, Some(&surface))?;
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
        let (surface, ctx) = surface_and_context(window.clone())?;
        Self::on(window, surface, ctx, cfg, slot, feed, sink)
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
            started: Instant::now(),
            last_present: None,
            last_frame: None,
            targets: Vec::new(),
            report,
        })
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.gfx.as_ref().expect("made in new").window
    }

    pub fn report(&self) -> &DesktopReport {
        &self.report
    }

    /// Ask winit for the next frame (call from `about_to_wait`).
    pub fn request_redraw(&self) {
        self.window().request_redraw();
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
        let Some(view) = self.slot.newest() else { return };
        let Some(g) = self.gfx.as_ref() else { return };
        let mhz = g.window.current_monitor().and_then(|m| m.refresh_rate_millihertz());
        let size = [g.config.width, g.config.height];
        let now = (self.cfg.clock)();
        let dt = self.last_frame.map_or(0.0, |t| now.secs_since(t)).clamp(0.0, 0.1);
        self.last_frame = Some(now);
        self.report.monitor_mhz = mhz;
        self.pacer.update_period(mhz.map_or(PeriodSource::Measured, PeriodSource::Monitor));
        let display_time = self.pacer.desktop_display_time(now);
        let t0 = Instant::now();

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
        let input = PointerInput {
            at: now,
            origin: camera.pose.pos,
            dir,
            left: self.pointer.left,
            shift: self.pointer.shift,
            wheel: std::mem::take(&mut self.pointer.wheel),
            roll,
        };
        let (_, hand) = self.hand.update(&input, &self.targets, &view.local_rig);
        self.feed.push(hand);
        let local = LocalPresentation { display_time, hands: [None, Some(DisplayHand { pose: hand.pose })] };

        let frame = self.cfg.presenter.frame(&view, &local, &[camera]);
        for c in &frame.effects {
            self.sink.command(c);
        }
        // The hand hits what is drawn, never its own hand or what it holds.
        self.targets = frame
            .display
            .objects
            .iter()
            .filter_map(|d| {
                let o = view.object(d.id)?;
                let own = o.flags.contains(ViewFlags::OWN_HAND) || o.flags.contains(ViewFlags::HELD_LOCAL);
                (o.shape.is_some() && !own).then_some(Target { center: d.pose.pos, radius: o.radius as f64 })
            })
            .collect();

        let g = self.gfx.as_mut().expect("checked above");
        // The frame's own work so far. Acquiring the next image waits for vsync, so it is not the frame's cost: counted
        // in, every frame looks one period long and the ladder drops at a steady rate (seen in the first window run).
        let before_acquire = t0.elapsed();
        let tex = match g.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            _ => {
                self.report.skipped += 1;
                g.surface.configure(&g.renderer.context().device, &g.config);
                return;
            }
        };
        let color =
            tex.texture.create_view(&wgpu::TextureViewDescriptor { format: Some(g.format), ..Default::default() });
        let stats = g.renderer.render_views(&frame.scene, &[EyeView { camera, color: &color, format: g.format, size }]);
        if let Some((at, path)) = self.cfg.snapshot.clone()
            && self.started.elapsed() >= at
        {
            self.cfg.snapshot = None;
            self.report.snapshot = Some(snapshot(g, &tex.texture, size, &path));
        }
        g.window.pre_present_notify();
        g.renderer.context().queue.present(tex);
        let cpu = before_acquire + Duration::from_secs_f64(stats.cpu_record_ms / 1e3);
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
                    self.occluded = s.width == 0 || s.height == 0;
                    if !self.occluded {
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
                    MouseButton::Left => self.pointer.left = down,
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
                if !self.occluded {
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

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        if let Some(v) = &self.view {
            v.request_redraw();
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
    el.set_control_flow(ControlFlow::Poll);
    let mut app = Standalone { parts: Some((cfg, slot, feed, sink)), view: None, error: None };
    el.run_app(&mut app).map_err(|e| ContextError::Device(format!("event loop: {e}")))?;
    match (app.error, app.view) {
        (Some(e), _) => Err(e),
        (None, Some(v)) => Ok(v.report),
        (None, None) => Err(ContextError::Device("the window was never made".into())),
    }
}
