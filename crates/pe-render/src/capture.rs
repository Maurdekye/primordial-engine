//! The offscreen capture for WorldApi's `view_capture` (render.md 11; api.md 8).
//!
//! - **What it draws:** a snapshot at its **simulated** poses ([`Scene::simulated`]); the display layer is bypassed,
//!   so the image shows what the world holds.
//! - **Where:** on the render thread's device, into its own target. Only the readback is deferred: [`Renderer::capture`]
//!   records and submits, and [`PendingCapture::poll`] collects the image on a later frame, so a capture never
//!   stalls a present. [`PendingCapture::wait`] blocks, for tests and headless use.
//! - **`visible_list`:** projected bounding boxes without occlusion, so `visible_approx` is always true in M1.
//! - Labels beside objects (`ab_glyph`) are not drawn yet.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use pe_seams::ObjectId;

use crate::camera::Camera;
use crate::scene::{EyeView, Renderer, Scene};

/// The capture's colour format: sRGB, as the window and the headset show it.
pub const CAPTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// The largest capture api.md allows.
pub const MAX_SIZE: [u32; 2] = [1920, 1080];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// No adapter: WorldApi's `capture_unavailable`.
    Unavailable,
    /// Zero, or larger than [`MAX_SIZE`].
    BadSize,
    /// The caller's deadline passed before the readback completed.
    TimedOut,
    /// The readback failed (a lost device).
    Readback(String),
}

impl From<crate::gpu::ContextError> for CaptureError {
    fn from(e: crate::gpu::ContextError) -> Self {
        match e {
            crate::gpu::ContextError::NoAdapter => CaptureError::Unavailable,
            crate::gpu::ContextError::Device(d) => CaptureError::Readback(d),
        }
    }
}

/// A captured image: tightly packed RGBA8, sRGB-encoded, rows top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    /// PNG bytes (pure-Rust encoder). Run it off the render thread.
    pub fn encode_png(&self) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut e = png::Encoder::new(&mut out, self.width, self.height);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            let mut w = e.write_header().expect("png header to memory");
            w.write_image_data(&self.rgba).expect("png data to memory");
        }
        out
    }
}

/// One object in a capture's `visible_list`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Visible {
    pub id: ObjectId,
    /// Pixels: left, top, right, bottom, clamped to the image.
    pub rect: [f32; 4],
    /// Distance from the camera to the object's centre, metres.
    pub distance: f32,
}

/// The objects whose bounding spheres the camera sees, nearest first. No occlusion test in M1 (`visible_approx`).
pub fn visible_list(scene: &Scene, camera: &Camera, size: [u32; 2]) -> Vec<Visible> {
    let vol = camera.volume();
    let proj = camera.projection();
    let (w, h) = (size[0] as f32, size[1] as f32);
    let mut out = Vec::new();
    for it in &scene.items {
        let r = it.radius as f64;
        if !vol.touches_sphere(it.pose.pos, r) {
            continue;
        }
        let c = camera.relative(it.pose.pos);
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for k in 0..8 {
            let corner = c + glam::DVec3::new(
                if k & 1 == 0 { -r } else { r },
                if k & 2 == 0 { -r } else { r },
                if k & 4 == 0 { -r } else { r },
            );
            // A corner behind the near plane is pulled onto it, so the box stays finite.
            let z = corner.z.min(-camera.near);
            let p = proj * glam::Vec4::new(corner.x as f32, corner.y as f32, z as f32, 1.0);
            let (nx, ny) = (p.x / p.w, p.y / p.w);
            let (px, py) = ((nx * 0.5 + 0.5) * w, (0.5 - ny * 0.5) * h);
            x0 = x0.min(px);
            y0 = y0.min(py);
            x1 = x1.max(px);
            y1 = y1.max(py);
        }
        let rect = [x0.clamp(0.0, w), y0.clamp(0.0, h), x1.clamp(0.0, w), y1.clamp(0.0, h)];
        if rect[2] > rect[0] && rect[3] > rect[1] {
            out.push(Visible { id: it.id, rect, distance: c.length() as f32 });
        }
    }
    out.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
    out
}

const MAP_PENDING: u8 = 0;
const MAP_OK: u8 = 1;
const MAP_FAILED: u8 = 2;

/// A capture whose readback is in flight.
pub struct PendingCapture {
    buffer: wgpu::Buffer,
    state: Arc<AtomicU8>,
    size: [u32; 2],
    padded_row: u32,
}

impl Renderer {
    /// Draw `scene` from `camera` into an offscreen target of `size` and start its readback.
    pub fn capture(&mut self, scene: &Scene, camera: Camera, size: [u32; 2]) -> Result<PendingCapture, CaptureError> {
        if size[0] == 0 || size[1] == 0 || size[0] > MAX_SIZE[0] || size[1] > MAX_SIZE[1] {
            return Err(CaptureError::BadSize);
        }
        let device = self.context().device.clone();
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("capture"),
            size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: CAPTURE_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        self.render_views(scene, &[EyeView { camera, color: &view, format: CAPTURE_FORMAT, size }]);
        let padded_row = (size[0] * 4).div_ceil(256) * 256;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capture readback"),
            size: padded_row as u64 * size[1] as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("capture copy") });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(size[1]),
                },
            },
            wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        );
        self.context().queue.submit([enc.finish()]);
        let state = Arc::new(AtomicU8::new(MAP_PENDING));
        let s = state.clone();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            s.store(if r.is_ok() { MAP_OK } else { MAP_FAILED }, Ordering::Release);
        });
        Ok(PendingCapture { buffer, state, size, padded_row })
    }
}

impl PendingCapture {
    /// The image, if its readback has completed; never blocks.
    pub fn poll(&self, device: &wgpu::Device) -> Option<Result<Image, CaptureError>> {
        let _ = device.poll(wgpu::PollType::Poll);
        match self.state.load(Ordering::Acquire) {
            MAP_PENDING => None,
            MAP_OK => Some(Ok(self.read())),
            _ => Some(Err(CaptureError::Readback("map failed".into()))),
        }
    }

    /// Block until the image is read back, or `deadline` passes.
    pub fn wait(&self, device: &wgpu::Device, deadline: Duration) -> Result<Image, CaptureError> {
        device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(deadline) })
            .map_err(|e| CaptureError::Readback(e.to_string()))?;
        match self.state.load(Ordering::Acquire) {
            MAP_OK => Ok(self.read()),
            MAP_PENDING => Err(CaptureError::TimedOut),
            _ => Err(CaptureError::Readback("map failed".into())),
        }
    }

    fn read(&self) -> Image {
        let [w, h] = self.size;
        let Ok(data) = self.buffer.slice(..).get_mapped_range() else {
            return Image { width: w, height: h, rgba: Vec::new() };
        };
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h as usize {
            let start = row * self.padded_row as usize;
            rgba.extend_from_slice(&data[start..start + (w * 4) as usize]);
        }
        drop(data);
        self.buffer.unmap();
        Image { width: w, height: h, rgba }
    }
}
