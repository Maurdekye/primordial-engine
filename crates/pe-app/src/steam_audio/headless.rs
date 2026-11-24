//! A repeatable presentation loop. Offscreen rendering has no window or pixel readback.

use std::time::{Duration, Instant};

use pe_render::LocalPresentation;
use pe_render::camera::{Camera, DepthRange, Fov};
use pe_render::counters::Histogram;
use pe_render::gpu::{AdapterChoice, GpuContext};
use pe_render::present::{FrameSlot, Presenter};
use pe_render::scene::{EyeView, RenderSettings, Renderer, Scene};
use pe_render::wgpu;
use pe_seams::RigPose;

use pe_app::{clock, console::Quit};

const SIZE: [u32; 2] = [1280, 720];
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const PERIOD: Duration = Duration::from_nanos(16_666_667);
const EYE: RigPose = RigPose { position: [0.0, 1.4, 1.5], orientation: [0.0, 0.0, 0.0, 1.0] };

pub(crate) fn run(mut presenter: Presenter, slot: &FrameSlot, quit: &Quit, run_for: Option<Duration>) {
    let render = std::env::var_os("PE_STEAM_AUDIO_RENDER").is_some_and(|s| s == "1");
    let mut offscreen = if render {
        match Offscreen::new() {
            Ok(target) => Some(target),
            Err(e) => {
                pe_app::err!("steam audio: offscreen comparison unavailable ({e})");
                return;
            }
        }
    } else {
        None
    };
    let started = Instant::now();
    let mut next = started;
    let deadline = run_for.map(|d| started + d);
    let mut latest = None;
    let mut cpu_ms = Histogram::default();
    let mut gpu_ms = Histogram::default();
    let mut frames = 0;
    while !quit.requested() && deadline.is_none_or(|end| Instant::now() < end) {
        let batch = slot.take();
        if let Some(view) = batch.views.last() {
            latest = Some(view.clone());
        }
        if let Some(view) = &latest {
            let camera = Camera::of_eye(
                &view.local_rig,
                &EYE,
                Fov::symmetric(1.0, SIZE[0] as f32 / SIZE[1] as f32),
                DepthRange::DESKTOP,
            );
            let local = LocalPresentation { display_time: clock::now(), hands: [None, None] };
            if let Some(frame) = presenter.frame_batch(&batch, &local, &[camera]) {
                if let Some(target) = &mut offscreen {
                    let stats = target.renderer.render_views(
                        &frame.scene,
                        &[EyeView { camera, color: &target.color, format: FORMAT, size: SIZE }],
                    );
                    if started.elapsed() >= super::WARMUP {
                        cpu_ms.record(stats.cpu_record_ms);
                        if let Some(ms) = stats.gpu_ms {
                            gpu_ms.record(ms);
                        }
                    }
                }
                frames += 1;
            }
        }
        next = (next + PERIOD).max(Instant::now());
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    pe_app::out!(
        "steam audio: trial render={render} presentation_frames={frames} offscreen_cpu_ms={:?} offscreen_gpu_ms={:?}",
        cpu_ms.summary(),
        gpu_ms.summary()
    );
}

struct Offscreen {
    renderer: Renderer,
    color: wgpu::TextureView,
}

impl Offscreen {
    fn new() -> Result<Self, pe_render::gpu::ContextError> {
        let gpu = GpuContext::headless(AdapterChoice::Any)?;
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Steam Audio trial target"),
            size: wgpu::Extent3d { width: SIZE[0], height: SIZE[1], depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let color = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Ok(Self { renderer: Renderer::new(gpu, RenderSettings::default()), color })
    }
}
