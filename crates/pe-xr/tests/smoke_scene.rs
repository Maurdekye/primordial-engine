//! The smoke run's scene, drawn offline the way the headset run draws it (spec 10.3): `render_stereo` into a
//! two-layer colour image and a two-layer D32 depth image, with the eye cameras pe-xr builds from a standing head's
//! eye poses and the headset's asymmetric field of view. On a software adapter (WARP on Windows, lavapipe on Linux),
//! never the user's GPU: with none, the test reports itself skipped (stderr) and passes.
//!
//! It fails on an empty frame: the first headset run drew `Scene::default()`, which is the sky's clear colour alone.

use std::time::Duration;

use pe_render::gpu::{AdapterChoice, GpuContext};
use pe_render::scene::{RenderSettings, Renderer, Scene, StereoTarget};
use pe_render::wgpu;
use pe_seams::{Pos, RigPose, RigTransform};
use pe_xr::backend::{EyePose, Fov};
use pe_xr::binding;

const SIZE: [u32; 2] = [160, 176];

fn renderer() -> Option<Renderer> {
    match GpuContext::headless(AdapterChoice::SoftwareOnly) {
        Ok(ctx) => {
            eprintln!("adapter: {}", ctx.describe());
            Some(Renderer::new(ctx, RenderSettings { multiview: false, ..Default::default() }))
        }
        Err(e) => {
            eprintln!("SKIPPED: no software adapter ({e})");
            None
        }
    }
}

fn texture(r: &Renderer, format: wgpu::TextureFormat) -> wgpu::Texture {
    r.context().device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test swapchain image"),
        size: wgpu::Extent3d { width: SIZE[0], height: SIZE[1], depth_or_array_layers: 2 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// One colour layer's RGBA8 texels, rows packed.
fn read_layer(r: &Renderer, t: &wgpu::Texture, layer: u32) -> Vec<[u8; 4]> {
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
            aspect: wgpu::TextureAspect::All,
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
    let mut out = Vec::with_capacity((SIZE[0] * SIZE[1]) as usize);
    for y in 0..SIZE[1] as usize {
        for x in 0..SIZE[0] as usize {
            let i = y * row as usize + x * 4;
            out.push([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        }
    }
    out
}

/// Both eyes of a user standing at the origin of a floor-level space (STAGE or LOCAL_FLOOR), looking ahead (-Z),
/// with a Quest 3-like asymmetric field of view (angles in radians, as OpenXR gives them).
fn standing_eyes() -> [EyePose; 2] {
    let eye = |x: f32, fov: Fov| EyePose {
        pose: RigPose { position: [x, 1.6, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
        fov,
    };
    [
        eye(-0.032, Fov { left: -0.94, right: 0.79, up: 0.89, down: -0.96 }),
        eye(0.032, Fov { left: -0.79, right: 0.94, up: 0.89, down: -0.96 }),
    ]
}

/// Both layers drawn as the headset run draws them; the sky's colour (the top-left texel, above the horizon).
fn draw(r: &mut Renderer, scene: &Scene) -> ([Vec<[u8; 4]>; 2], [u8; 4]) {
    let rig = RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 };
    let cameras = binding::eye_cameras(&rig, &standing_eyes());
    let color = texture(r, wgpu::TextureFormat::Rgba8UnormSrgb);
    let depth = texture(r, wgpu::TextureFormat::Depth32Float);
    r.render_stereo(scene, cameras, &StereoTarget { color: &color, depth: Some(&depth), extent: SIZE });
    let layers = [0, 1].map(|l| read_layer(r, &color, l));
    let sky = layers[0][0];
    (layers, sky)
}

fn not_sky(pixels: &[[u8; 4]], sky: [u8; 4]) -> usize {
    pixels.iter().filter(|p| **p != sky).count()
}

#[test]
fn an_empty_scene_is_the_sky_alone_which_is_what_the_first_headset_run_showed() {
    let Some(mut r) = renderer() else { return };
    let (layers, sky) = draw(&mut r, &Scene::default());
    for (l, px) in layers.iter().enumerate() {
        assert_eq!(not_sky(px, sky), 0, "layer {l}: pe-render draws nothing for an empty scene, no ground included");
    }
}

#[test]
fn the_smoke_scene_shows_the_floor_below_the_horizon_and_the_objects_ahead_in_both_eyes() {
    let Some(mut r) = renderer() else { return };
    let (layers, sky) = draw(&mut r, &pe_xr::smoke::scene());
    let w = SIZE[0] as usize;
    let h = SIZE[1] as usize;
    for (l, px) in layers.iter().enumerate() {
        // The bottom fifth of the image looks 40 to 44 degrees down: floor, from 1.4 m away.
        let bottom = &px[(h * 4 / 5) * w..];
        let floor = not_sky(bottom, sky);
        eprintln!(
            "layer {l}: {floor} of {} bottom-fifth texels are floor; {} of {} not sky",
            bottom.len(),
            not_sky(px, sky),
            px.len()
        );
        assert!(
            floor * 10 >= bottom.len() * 9,
            "layer {l}: only {floor} of {} texels in the bottom fifth are floor (sky {sky:?})",
            bottom.len()
        );
        // The red ball straight ahead, 0.4 m below the eye at 1.5 m: a reddish texel near the middle.
        let red = px.iter().filter(|p| p[0] > 120 && p[0] > p[1].saturating_add(40) && p[0] > p[2].saturating_add(40));
        assert!(red.count() > 20, "layer {l}: the red ball ahead is not drawn");
    }
}
