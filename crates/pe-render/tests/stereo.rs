//! render.md 5 and xr.md 4.2: `render_stereo` into a headset's two-layer swapchain images, on a software adapter
//! (WARP on Windows, lavapipe on Linux). Never on the user's GPU: with no software adapter the tests report
//! themselves skipped (stderr) and pass. WARP under FXC has no multiview, so there the two-pass path is tested and
//! the multiview comparison skips; lavapipe runs it.
//!
//! `PE_RENDER_TEST_HARDWARE=1` runs them on the best hardware adapter instead, which is how multiview is checked on
//! a real GPU. Set it only for a run the coordinator has approved.

use std::time::Duration;

use glam::{DQuat, DVec3};
use pe_render::camera::{Camera, DepthRange, Fov};
use pe_render::gpu::{AdapterChoice, GpuContext};
use pe_render::pose::WorldPose;
use pe_render::scene::{DEFAULT_APPEARANCE, DrawItem, RenderSettings, Renderer, Scene, StereoTarget};
use pe_render::wgpu;
use pe_seams::{Appearance, ObjectId, Offset, Pos, Shape};

const SIZE: [u32; 2] = [192, 160];

fn renderer(settings: RenderSettings) -> Option<Renderer> {
    let choice = if std::env::var("PE_RENDER_TEST_HARDWARE").as_deref() == Ok("1") {
        AdapterChoice::Any
    } else {
        AdapterChoice::SoftwareOnly
    };
    match GpuContext::headless(choice) {
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

fn ball(id: u128, pos: Pos, radius: f32, color: [f32; 4]) -> DrawItem {
    DrawItem {
        id: ObjectId::from_u128(id),
        pose: WorldPose { pos, rot: DQuat::IDENTITY },
        scale: [1.0; 3],
        shape: Shape::Sphere { radius },
        appearance: Appearance { base_color: color, ..DEFAULT_APPEARANCE },
        radius,
        outline: None,
        ghost: None,
    }
}

/// Two balls ahead of a head at 1.6 m; the eyes 64 mm apart, looking along −Z, with VR's finite depth range.
fn scene() -> (Scene, [Camera; 2]) {
    let mut s = Scene::default();
    s.items.push(ball(1, at([0.0, 1.6, -2.0]), 0.5, [0.8, 0.2, 0.15, 1.0]));
    s.items.push(ball(2, at([0.6, 1.2, -3.0]), 0.3, [0.2, 0.4, 0.85, 1.0]));
    let fov = Fov { left: -0.8, right: 0.7, up: 0.75, down: -0.8 };
    let eye = |x: f64| Camera::placed(WorldPose { pos: at([x, 1.6, 0.0]), rot: DQuat::IDENTITY }, fov, DepthRange::VR);
    (s, [eye(-0.032), eye(0.032)])
}

fn texture(r: &Renderer, format: wgpu::TextureFormat, layers: u32) -> wgpu::Texture {
    r.context().device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test swapchain image"),
        size: wgpu::Extent3d { width: SIZE[0], height: SIZE[1], depth_or_array_layers: layers },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// One layer's texels, 4 bytes each, rows packed.
fn read_layer(r: &Renderer, t: &wgpu::Texture, layer: u32, aspect: wgpu::TextureAspect) -> Vec<u8> {
    let ctx = r.context();
    let row = (SIZE[0] * 4).div_ceil(256) * 256;
    let buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: row as u64 * SIZE[1] as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut enc = ctx.device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: t,
            mip_level: 0,
            origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
            aspect,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(SIZE[1]) },
        },
        wgpu::Extent3d { width: SIZE[0], height: SIZE[1], depth_or_array_layers: 1 },
    );
    ctx.queue.submit([enc.finish()]);
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    ctx.device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(Duration::from_secs(60)) })
        .expect("poll");
    let data = buf.slice(..).get_mapped_range().expect("mapped");
    let mut out = Vec::with_capacity((SIZE[0] * SIZE[1] * 4) as usize);
    for y in 0..SIZE[1] as usize {
        out.extend_from_slice(&data[y * row as usize..y * row as usize + SIZE[0] as usize * 4]);
    }
    out
}

fn depth_at(d: &[u8], x: u32, y: u32) -> f32 {
    let i = ((y * SIZE[0] + x) * 4) as usize;
    f32::from_le_bytes(d[i..i + 4].try_into().unwrap())
}

/// The pixel a world point projects to through `cam`, and its reverse-Z depth.
fn project(cam: &Camera, p: DVec3) -> (u32, u32, f32) {
    let local = cam.relative(at([p.x, p.y, p.z]));
    let c = cam.projection() * glam::Vec4::new(local.x as f32, local.y as f32, local.z as f32, 1.0);
    let ndc = c / c.w;
    let x = ((ndc.x * 0.5 + 0.5) * SIZE[0] as f32) as u32;
    let y = ((0.5 - ndc.y * 0.5) * SIZE[1] as f32) as u32;
    (x, y, ndc.z)
}

/// Each eye's layer equals a single-view capture from that eye, pixel for pixel: the stereo path draws what the
/// desktop path draws, into the right layer.
#[test]
fn each_layer_matches_a_single_view_of_its_eye() {
    let Some(mut r) = renderer(RenderSettings { multiview: false, ..Default::default() }) else { return };
    let (s, eyes) = scene();
    let color = texture(&r, wgpu::TextureFormat::Rgba8UnormSrgb, 2);
    r.render_stereo(&s, eyes, &StereoTarget { color: &color, depth: None, extent: SIZE });
    let layers = [0, 1].map(|l| read_layer(&r, &color, l, wgpu::TextureAspect::All));
    assert_ne!(layers[0], layers[1], "the eyes are 64 mm apart: their images differ");
    for (l, eye) in eyes.into_iter().enumerate() {
        let single = r.capture(&s, eye, SIZE).expect("capture");
        let img = single.wait(&r.context().device, Duration::from_secs(60)).expect("readback");
        let differing = img.rgba.chunks(4).zip(layers[l].chunks(4)).filter(|(a, b)| a != b).count();
        assert_eq!(differing, 0, "layer {l}: {differing} pixels differ from the single view");
    }
}

/// The headset's depth layer (review L2): reverse-Z depth stored in both layers, matching the projection that
/// `xr_depth_info` describes, through the MSAA resolve and through the direct single-sample path alike.
#[test]
fn the_headset_depth_layer_holds_the_projected_depth() {
    for msaa in [4, 1] {
        let Some(mut r) = renderer(RenderSettings { msaa, multiview: false, ..Default::default() }) else { return };
        let (s, eyes) = scene();
        let color = texture(&r, wgpu::TextureFormat::Rgba8UnormSrgb, 2);
        let depth = texture(&r, wgpu::TextureFormat::Depth32Float, 2);
        r.render_stereo(&s, eyes, &StereoTarget { color: &color, depth: Some(&depth), extent: SIZE });
        for (l, eye) in eyes.iter().enumerate() {
            let d = read_layer(&r, &depth, l as u32, wgpu::TextureAspect::DepthOnly);
            // The near ball's front point, straight ahead of its centre from this eye.
            let front = DVec3::new(0.0, 1.6, -1.5);
            let (x, y, expect) = project(eye, front);
            let got = depth_at(&d, x, y);
            assert!(got > 0.0, "msaa {msaa}, layer {l}: no depth at the ball ({x}, {y})");
            // A sphere's depth near its front point varies slowly; within a pixel or two of it, 1 %.
            assert!((got - expect).abs() / expect < 0.01, "msaa {msaa}, layer {l}: depth {got}, projected {expect}");
            // Nothing is behind the top-left corner: the cleared far value, 0.
            assert_eq!(depth_at(&d, 1, 1), 0.0, "msaa {msaa}, layer {l}: the empty corner has depth");
            // And the layer's numbers describe that projection.
            let info = DepthRange::VR.xr_depth_info();
            assert!(info.near_z > info.far_z, "reverse Z: near_z is the far plane");
        }
    }
}

/// Render scale: only `extent` is drawn; outside it the image is the clear colour (the compositor reads only the
/// `imageRect`).
#[test]
fn only_the_extent_is_drawn() {
    let Some(mut r) = renderer(RenderSettings { multiview: false, ..Default::default() }) else { return };
    let (s, eyes) = scene();
    let color = texture(&r, wgpu::TextureFormat::Rgba8UnormSrgb, 2);
    let half = [SIZE[0] / 2, SIZE[1] / 2];
    r.render_stereo(&s, eyes, &StereoTarget { color: &color, depth: None, extent: half });
    let img = read_layer(&r, &color, 0, wgpu::TextureAspect::All);
    let px = |x: u32, y: u32| img[((y * SIZE[0] + x) * 4) as usize..][..4].to_vec();
    let clear = px(SIZE[0] - 1, SIZE[1] - 1);
    let outside = (0..SIZE[1])
        .flat_map(|y| (0..SIZE[0]).map(move |x| (x, y)))
        .filter(|&(x, y)| x >= half[0] || y >= half[1])
        .filter(|&(x, y)| px(x, y) != clear)
        .count();
    assert_eq!(outside, 0, "{outside} pixels drawn outside the extent");
    let inside = (0..half[1]).flat_map(|y| (0..half[0]).map(move |x| (x, y))).filter(|&(x, y)| px(x, y) != clear);
    assert!(inside.count() > 200, "the scene is drawn inside the extent");
}

/// Where the device has multiview, one multiview pass draws the same images as two passes (pixels equal, on one
/// adapter). WARP under FXC has none, so it skips there.
#[test]
fn multiview_draws_what_two_passes_draw() {
    let Some(mut one) = renderer(RenderSettings::default()) else { return };
    if !one.multiview_available(one.settings().msaa) {
        eprintln!("SKIPPED: the software adapter has no multiview (with MSAA)");
        return;
    }
    let Some(mut two) = renderer(RenderSettings { multiview: false, ..Default::default() }) else { return };
    let (s, eyes) = scene();
    let a = texture(&one, wgpu::TextureFormat::Rgba8UnormSrgb, 2);
    let b = texture(&two, wgpu::TextureFormat::Rgba8UnormSrgb, 2);
    one.render_stereo(&s, eyes, &StereoTarget { color: &a, depth: None, extent: SIZE });
    two.render_stereo(&s, eyes, &StereoTarget { color: &b, depth: None, extent: SIZE });
    for l in 0..2 {
        assert_eq!(
            read_layer(&one, &a, l, wgpu::TextureAspect::All),
            read_layer(&two, &b, l, wgpu::TextureAspect::All),
            "layer {l} differs"
        );
    }
}
