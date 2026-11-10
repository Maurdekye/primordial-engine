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
}

/// What a run measured (render.md 13's per-frame numbers, summarized).
#[derive(Clone, Debug, Default)]
pub struct DesktopReport {
    pub adapter: String,
    pub frames: u64,
    /// Present intervals, milliseconds.
    pub intervals_ms: Vec<f64>,
    pub cpu_record_ms: Vec<f64>,
    pub skipped: u64,
    pub quality_changes: u64,
    pub monitor_mhz: Option<u32>,
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

struct App {
    cfg: DesktopConfig,
    slot: Arc<FrameSlot>,
    feed: Arc<DesktopHandFeed>,
    sink: Box<dyn EffectSink>,
    gfx: Option<Gfx>,
    error: Option<ContextError>,
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

impl App {
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
        g.window.pre_present_notify();
        g.renderer.context().queue.present(tex);
        let cpu = t0.elapsed();
        self.report.frames += 1;
        self.report.cpu_record_ms.push(stats.cpu_record_ms);
        let p = Instant::now();
        if let Some(l) = self.last_present {
            self.report.intervals_ms.push((p - l).as_secs_f64() * 1e3);
        }
        self.last_present = Some(p);
        let done = (self.cfg.clock)();
        self.pacer.presented(done);
        if let Some(q) = self.pacer.frame_done(done, cpu, None) {
            self.report.quality_changes += 1;
            g.renderer.set_msaa(q.msaa);
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() || self.error.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(self.cfg.title.clone())
            .with_inner_size(winit::dpi::PhysicalSize::new(self.cfg.size[0], self.cfg.size[1]));
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.error = Some(ContextError::Device(e.to_string()));
                el.exit();
                return;
            }
        };
        let made = {
            let _one_at_a_time = crate::gpu::creating();
            let instance = wgpu::Instance::new(instance_descriptor(backends()));
            instance.create_surface(window.clone()).map(|s| (instance, s))
        };
        let (instance, surface) = match made {
            Ok(x) => x,
            Err(e) => {
                self.error = Some(ContextError::Device(e.to_string()));
                el.exit();
                return;
            }
        };
        let ctx = match GpuContext::desktop(instance, Some(&surface)) {
            Ok(c) => c,
            Err(e) => {
                self.error = Some(e);
                el.exit();
                return;
            }
        };
        self.report.adapter = ctx.describe();
        let size = window.inner_size();
        let Some(mut config) = surface.get_default_config(&ctx.adapter, size.width.max(1), size.height.max(1)) else {
            self.error = Some(ContextError::Device("the surface is not supported by the adapter".into()));
            el.exit();
            return;
        };
        config.present_mode = self.cfg.present_mode;
        config.desired_maximum_frame_latency = 2;
        // Draw through an sRGB view, so the window reads as the capture and the headset do.
        let format = config.format.add_srgb_suffix();
        if format != config.format {
            config.view_formats = vec![format];
        }
        surface.configure(&ctx.device, &config);
        let renderer = Renderer::new(ctx, self.cfg.render);
        self.gfx = Some(Gfx { window, surface, config, format, renderer });
        self.started = Instant::now();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
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
            WindowEvent::ModifiersChanged(m) => self.pointer.shift = m.state().shift_key(),
            WindowEvent::KeyboardInput { event, .. } => {
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
                    _ => return,
                };
                self.pointer.keys[i] = down;
            }
            WindowEvent::RedrawRequested => {
                if !self.occluded {
                    self.draw();
                }
                if self.cfg.run_for.is_some_and(|d| self.started.elapsed() >= d) {
                    el.exit();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
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
    let q = quat(cfg.eye.orientation);
    let (yaw, pitch) = {
        let f = q * DVec3::NEG_Z;
        // The inverse of eye_rot: forward is (−sin yaw cos pitch, sin pitch, −cos yaw cos pitch).
        ((-f.x).atan2(-f.z), f.y.clamp(-1.0, 1.0).asin())
    };
    let eye_pos = DVec3::new(cfg.eye.position[0] as f64, cfg.eye.position[1] as f64, cfg.eye.position[2] as f64);
    let hand = DesktopHand::new(cfg.hand);
    let mut app = App {
        pacer: Pacer::new(PacerSettings::default(), &DESKTOP_LADDER),
        cfg,
        slot,
        feed,
        sink,
        gfx: None,
        error: None,
        hand,
        pointer: Pointer::default(),
        yaw,
        pitch,
        eye_pos,
        occluded: false,
        started: Instant::now(),
        last_present: None,
        last_frame: None,
        targets: Vec::new(),
        report: DesktopReport::default(),
    };
    el.run_app(&mut app).map_err(|e| ContextError::Device(format!("event loop: {e}")))?;
    match app.error {
        Some(e) => Err(e),
        None => Ok(app.report),
    }
}
