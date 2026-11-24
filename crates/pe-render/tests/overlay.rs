//! render.md 8.1: the notices strip as `Scene::overlay`, on a software adapter (WARP on Windows, lavapipe on Linux).
//! Never on the user's GPU: with no software adapter the tests report themselves skipped (stderr) and pass.
//!
//! - In the window, the strip sits at the bottom over the scene, and an item in `Scene::front` (the own hands and
//!   what they hold) is drawn over the strip, never under it.
//! - In the headset, the head-locked strip shows in both eyes, below the centre.

use std::sync::Arc;
use std::time::Duration;

use glam::DQuat;
use pe_render::camera::{Camera, DepthRange, Fov};
use pe_render::capture::Image;
use pe_render::gpu::{AdapterChoice, GpuContext};
use pe_render::notices::{Line, Panel, PanelSettings, head_of, strip, window_overlay, window_text_px};
use pe_render::pose::WorldPose;
use pe_render::scene::{DEFAULT_APPEARANCE, DrawItem, RenderSettings, Renderer, Scene, StereoTarget};
use pe_render::wgpu;
use pe_seams::{Appearance, HostTime, ObjectId, Offset, Pos, Shape};

const SIZE: [u32; 2] = [320, 180];

fn renderer(settings: RenderSettings) -> Option<Renderer> {
    match GpuContext::headless(AdapterChoice::SoftwareOnly) {
        Ok(ctx) => {
            eprintln!("adapter: {}", ctx.describe());
            Some(Renderer::new(ctx, settings))
        }
        Err(e) => {
            eprintln!("SKIPPED: no software adapter ({e})");
            None
        }
    }
}

fn at(d: [f64; 3]) -> Pos {
    Pos::ORIGIN.checked_add(Offset(d)).unwrap()
}

fn item(id: u128, pos: Pos, shape: Shape, color: [f32; 3]) -> DrawItem {
    let radius = match shape {
        Shape::Sphere { radius } => radius,
        Shape::Box { half_extents: h } => (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt(),
        _ => 1.0,
    };
    DrawItem {
        id: ObjectId::from_u128(id),
        pose: WorldPose { pos, rot: DQuat::IDENTITY },
        scale: [1.0; 3],
        shape,
        appearance: Appearance {
            base_color: [color[0], color[1], color[2], 1.0],
            emissive: color,
            ..DEFAULT_APPEARANCE
        },
        radius,
        outline: None,
        ghost: None,
    }
}

/// A red wall filling the view, 5 m ahead.
fn wall() -> DrawItem {
    item(1, at([0.0, 0.0, -5.0]), Shape::Box { half_extents: [20.0, 20.0, 0.1] }, [0.8, 0.05, 0.05])
}

fn px(img: &Image, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * img.width + x) * 4) as usize;
    img.rgba[i..i + 4].try_into().unwrap()
}

fn one_line(text: &str, px_h: f32) -> Arc<pe_render::scene::OverlayImage> {
    Arc::new(strip(&[Line { text: text.into(), alpha: 1.0 }], px_h).expect("a line"))
}

/// The window: the strip darkens the wall at the bottom centre and leaves the rest. A green ball in `front` at the
/// strip's centre stays green; the same ball in `items` is covered by the strip.
#[test]
fn the_window_strip_covers_the_scene_but_never_the_front_items() {
    let Some(mut r) = renderer(RenderSettings::default()) else { return };
    let cam = Camera::placed(
        WorldPose { pos: Pos::ORIGIN, rot: DQuat::IDENTITY },
        Fov::symmetric(1.0, 16.0 / 9.0),
        DepthRange::DESKTOP,
    );
    let image = one_line("Out of reach. Out of reach. Out of reach.", window_text_px(SIZE[1]));
    let overlay = window_overlay(&cam, SIZE, image.clone());
    // The strip's rectangle in pixels: centred, at the bottom, at its own size.
    let (w, h) = (image.width, image.height);
    let x0 = (SIZE[0] - w) / 2;
    let y1 = SIZE[1] - (0.03 * SIZE[1] as f64).round() as u32;
    let y0 = y1 - h;
    eprintln!("strip {w} x {h} px at x {x0}.., y {y0}..{y1}");
    // A green ball 0.5 m ahead whose centre projects into the strip's middle.
    let tan_half = (0.5f64).tan();
    let ndc_y = 1.0 - 2.0 * ((y0 + y1) as f64 / 2.0) / SIZE[1] as f64;
    let ball_at = at([0.0, ndc_y * tan_half * 0.5, -0.5]);
    let ball = item(2, ball_at, Shape::Sphere { radius: 0.02 }, [0.05, 0.8, 0.05]);

    let mut s = Scene { items: vec![wall()], overlays: vec![overlay.clone()], ..Scene::default() };
    s.front.push(ball.clone());
    let img = r.capture(&s, cam, SIZE).expect("capture").wait(&r.context().device, Duration::from_secs(60)).unwrap();
    let mid = ((y0 + y1) / 2).min(SIZE[1] - 1);
    let above = px(&img, SIZE[0] / 2, 10);
    let pad = px(&img, x0 + 2, mid);
    let centre = px(&img, SIZE[0] / 2, mid);
    eprintln!("wall above {above:?}, strip's pad {pad:?}, front ball at the centre {centre:?}");
    assert!(above[0] > 150 && above[0] > above[1] + 100, "the wall, untouched above the strip: {above:?}");
    assert!(pad[0] < above[0] - 40 && pad[0] > 20, "the strip's box darkens the wall (60 % black): {pad:?}");
    assert!(centre[1] > 150 && centre[1] > centre[0] + 100, "the front ball is drawn over the strip: {centre:?}");

    // Control: the same ball as an ordinary item is under the strip.
    let mut s2 = Scene { items: vec![wall(), ball], overlays: vec![overlay], ..Scene::default() };
    s2.front.clear();
    let img2 = r.capture(&s2, cam, SIZE).expect("capture").wait(&r.context().device, Duration::from_secs(60)).unwrap();
    let covered = px(&img2, SIZE[0] / 2, mid);
    eprintln!("the ball as an item, under the strip: {covered:?}");
    assert!(covered[1] < centre[1] - 40, "an ordinary item is covered by the strip: {covered:?}");
}

/// The headset: the head-locked strip (1 m ahead, 20° down) shows in both eyes, below the centre and nowhere in the
/// top half.
#[test]
fn the_head_locked_strip_shows_in_both_eyes_below_the_centre() {
    let Some(mut r) = renderer(RenderSettings { multiview: false, ..Default::default() }) else { return };
    let fov = Fov { left: -0.8, right: 0.7, up: 0.75, down: -0.8 };
    let eye = |x: f64| Camera::placed(WorldPose { pos: at([x, 1.6, 0.0]), rot: DQuat::IDENTITY }, fov, DepthRange::VR);
    let eyes = [eye(-0.032), eye(0.032)];
    let mut panel = Panel::new(PanelSettings::default());
    let image = one_line("An action of yours arrived too late and was undone.", PanelSettings::default().px);
    let overlay = panel.place(&head_of(&eyes), 1.0, image, HostTime::from_millis(0));
    let mut s = Scene { items: vec![wall()], ..Scene::default() };
    let size = [192u32, 160];
    let color_of = |r: &mut Renderer, s: &Scene| {
        let color = r.context().device.create_texture(&wgpu::TextureDescriptor {
            label: Some("eyes"),
            size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 2 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        r.render_stereo(s, eyes, &StereoTarget { color: &color, depth: None, extent: size });
        (0..2).map(|l| read_layer(r, &color, l, size)).collect::<Vec<_>>()
    };
    let bare = color_of(&mut r, &s);
    s.overlays = vec![overlay];
    let with = color_of(&mut r, &s);
    for l in 0..2 {
        let changed: Vec<u32> = (0..size[0] * size[1])
            .filter(|i| {
                let k = *i as usize * 4;
                (0..3).any(|c| bare[l][k + c].abs_diff(with[l][k + c]) > 8)
            })
            .map(|i| i / size[0])
            .collect();
        let n = changed.len();
        let top = changed.iter().filter(|y| **y < size[1] / 2).count();
        eprintln!("eye {l}: the strip changed {n} px, {top} in the top half");
        assert!(n > 300, "eye {l}: the strip shows ({n} px)");
        assert_eq!(top, 0, "eye {l}: nothing in the top half");
    }
}

fn read_layer(r: &Renderer, t: &wgpu::Texture, layer: u32, size: [u32; 2]) -> Vec<u8> {
    let device = &r.context().device;
    let bpr = (size[0] * 4).div_ceil(256) * 256;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (bpr * size[1]) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: t,
            mip_level: 0,
            origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(size[1]) },
        },
        wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
    );
    r.context().queue.submit([enc.finish()]);
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(Duration::from_secs(60)) }).expect("poll");
    let data = buf.slice(..).get_mapped_range().expect("mapped");
    let mut out = Vec::with_capacity((size[0] * size[1] * 4) as usize);
    for row in 0..size[1] {
        let s = (row * bpr) as usize;
        out.extend_from_slice(&data[s..s + (size[0] * 4) as usize]);
    }
    out
}

/// Several overlays (render.md 8.2, the menus): each is drawn, in order, so where two overlap the later one shows.
#[test]
fn several_overlays_draw_in_order() {
    use pe_render::scene::{Overlay, OverlayImage};
    let Some(mut r) = renderer(RenderSettings::default()) else { return };
    let cam = Camera::placed(
        WorldPose { pos: Pos::ORIGIN, rot: DQuat::IDENTITY },
        Fov::symmetric(1.0, 16.0 / 9.0),
        DepthRange::DESKTOP,
    );
    let solid = |c: [u8; 3]| Arc::new(OverlayImage { width: 4, height: 4, rgba: [c[0], c[1], c[2], 255].repeat(16) });
    // 1 m ahead: blue on the left half and the middle, green on the middle and the right half.
    let quad = |x: f64, c: [u8; 3]| Overlay {
        image: solid(c),
        pose: WorldPose { pos: at([x, 0.0, -1.0]), rot: DQuat::IDENTITY },
        size: [0.4, 0.3],
    };
    let shot = |r: &mut Renderer, overlays: Vec<Overlay>| {
        let s = Scene { items: vec![wall()], overlays, ..Scene::default() };
        r.capture(&s, cam, SIZE).expect("capture").wait(&r.context().device, Duration::from_secs(60)).unwrap()
    };
    let img = shot(&mut r, vec![quad(-0.15, [0, 0, 255]), quad(0.15, [0, 255, 0])]);
    let (left, mid, right) = (px(&img, 110, 90), px(&img, 160, 90), px(&img, 210, 90));
    eprintln!("left {left:?}, middle {mid:?}, right {right:?}");
    assert!(left[2] > 200 && left[1] < 60, "the first overlay, alone on the left: {left:?}");
    assert!(right[1] > 200 && right[2] < 60, "the second overlay, alone on the right: {right:?}");
    assert!(mid[1] > 200 && mid[2] < 60, "the second covers the first where they overlap: {mid:?}");
    // The other order: the first (now green) is under the second (blue).
    let img = shot(&mut r, vec![quad(0.15, [0, 255, 0]), quad(-0.15, [0, 0, 255])]);
    let mid = px(&img, 160, 90);
    assert!(mid[2] > 200 && mid[1] < 60, "order decides: {mid:?}");
}
