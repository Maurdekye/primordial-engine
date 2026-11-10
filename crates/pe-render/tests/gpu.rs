//! render.md 14, tests 4 and 5, on a software adapter (WARP on Windows, lavapipe on Linux). They never draw on the
//! user's GPU: with no software adapter they report themselves skipped (stderr) and pass, which the gate's log
//! shows.

use std::time::Duration;

use glam::DQuat;
use pe_render::camera::{Camera, DepthRange, Fov};
use pe_render::capture::{CaptureError, Image, visible_list};
use pe_render::gpu::{AdapterChoice, ContextError, GpuContext};
use pe_render::pose::WorldPose;
use pe_render::scene::{DEFAULT_APPEARANCE, DrawItem, Renderer, RenderSettings, Scene};
use pe_seams::{Appearance, ObjectId, Offset, Pos, Shape};

fn renderer() -> Option<Renderer> {
    match GpuContext::headless(AdapterChoice::SoftwareOnly) {
        Ok(ctx) => {
            eprintln!("adapter: {}", ctx.describe());
            Some(Renderer::new(ctx, RenderSettings::default()))
        }
        Err(e) => {
            eprintln!("SKIPPED: no software adapter ({e})");
            None
        }
    }
}

fn at(base: Pos, d: [f64; 3]) -> Pos {
    base.checked_add(Offset(d)).unwrap()
}

fn item(id: u128, pos: Pos, shape: Shape, color: [f32; 4]) -> DrawItem {
    let radius = match shape {
        Shape::Sphere { radius } => radius,
        Shape::Box { half_extents: h } => (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt(),
        _ => 1.0,
    };
    DrawItem {
        id: ObjectId(id),
        pose: WorldPose { pos, rot: DQuat::from_rotation_y(0.5) },
        scale: [1.0; 3],
        shape,
        appearance: Appearance { base_color: color, ..DEFAULT_APPEARANCE },
        radius,
        outline: None,
        ghost: None,
    }
}

/// A small scene around `base`: a floor, two balls and a capsule, seen from 1.6 m up.
fn scene_at(base: Pos) -> (Scene, Camera) {
    let mut s = Scene::default();
    s.items.push(item(1, at(base, [0.0, -0.05, -3.0]), Shape::Box { half_extents: [3.0, 0.05, 3.0] }, [0.5, 0.5, 0.55, 1.0]));
    s.items.push(item(2, at(base, [-0.4, 0.3, -3.0]), Shape::Sphere { radius: 0.3 }, [0.8, 0.2, 0.15, 1.0]));
    s.items.push(item(3, at(base, [0.5, 0.2, -2.6]), Shape::Sphere { radius: 0.2 }, [0.2, 0.4, 0.85, 1.0]));
    s.items.push(item(4, at(base, [0.1, 0.25, -3.6]), Shape::Capsule { radius: 0.15, half_height: 0.25 }, [0.3, 0.75, 0.3, 1.0]));
    let cam = Camera::placed(
        WorldPose { pos: at(base, [0.0, 1.6, 0.0]), rot: DQuat::from_rotation_x(-0.45) },
        Fov::symmetric(1.0, 16.0 / 9.0),
        DepthRange::DESKTOP,
    );
    (s, cam)
}

fn capture(r: &mut Renderer, s: &Scene, cam: Camera, size: [u32; 2]) -> Image {
    let p = r.capture(s, cam, size).expect("capture");
    p.wait(&r.context().device, Duration::from_secs(60)).expect("readback")
}

/// Test 4: a scene placed 10^15 m from the origin draws the same pixels as at the origin, because only differences
/// reach the GPU.
#[test]
fn a_scene_far_from_the_origin_draws_the_same_image() {
    let Some(mut r) = renderer() else { return };
    let (s0, c0) = scene_at(Pos::ORIGIN);
    let far = at(Pos::ORIGIN, [1e15, -4e14, 7e14]);
    let (s1, c1) = scene_at(far);
    let a = capture(&mut r, &s0, c0, [256, 144]);
    let b = capture(&mut r, &s1, c1, [256, 144]);
    let differing = a.rgba.chunks(4).zip(b.rgba.chunks(4)).filter(|(x, y)| x != y).count();
    assert_eq!(differing, 0, "{differing} pixels differ at 10^15 m");
    // And the image is not empty: the balls are drawn.
    let bg = &a.rgba[0..4];
    assert!(a.rgba.chunks(4).filter(|p| *p != bg).count() > 2000);
}

/// Test 5: a known scene has the expected coverage and identical pixels twice; `visible_list` names the object.
#[test]
fn a_capture_has_the_expected_coverage_and_repeats_exactly() {
    let Some(mut r) = renderer() else { return };
    let mut s = Scene::default();
    let d = 5.0f64;
    let rad = 1.0f32;
    s.items.push(item(7, at(Pos::ORIGIN, [0.0, 0.0, -d]), Shape::Sphere { radius: rad }, [0.9, 0.9, 0.9, 1.0]));
    s.environment.ambient = [0.5; 3];
    let vfov = 1.0f32;
    let size = [256u32, 144];
    let cam = Camera::placed(
        WorldPose { pos: Pos::ORIGIN, rot: DQuat::IDENTITY },
        Fov::symmetric(vfov, size[0] as f32 / size[1] as f32),
        DepthRange::DESKTOP,
    );
    let a = capture(&mut r, &s, cam, size);
    let b = capture(&mut r, &s, cam, size);
    assert_eq!(a, b, "two captures differ");
    let bg = a.rgba[0..4].to_vec();
    let covered = a.rgba.chunks(4).filter(|p| p.iter().zip(&bg).any(|(x, y)| x.abs_diff(*y) > 8)).count() as f64;
    // The silhouette's angular radius is asin(r/d); its radius in pixels follows from the projection.
    let alpha = (rad as f64 / d).asin();
    let rpx = alpha.tan() / (vfov as f64 * 0.5).tan() * size[1] as f64 * 0.5;
    let expected = std::f64::consts::PI * rpx * rpx;
    assert!((covered / expected - 1.0).abs() < 0.06, "covered {covered} px, expected {expected:.0}");
    let v = visible_list(&s, &cam, size);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].id, ObjectId(7));
    let [x0, y0, x1, y1] = v[0].rect;
    assert!(x0 < 128.0 && x1 > 128.0 && y0 < 72.0 && y1 > 72.0, "{:?}", v[0].rect);
    eprintln!("coverage {covered} px against {expected:.0} expected");
}

/// The cue's outline draws around the object, not over it.
#[test]
fn the_outline_draws_around_the_silhouette() {
    let Some(mut r) = renderer() else { return };
    let (mut s, cam) = scene_at(Pos::ORIGIN);
    let plain = capture(&mut r, &s, cam, [256, 144]);
    s.items[1].outline = Some([1.0, 1.0, 0.0, 1.0]);
    let lined = capture(&mut r, &s, cam, [256, 144]);
    let changed = plain.rgba.chunks(4).zip(lined.rgba.chunks(4)).filter(|(x, y)| x != y).count();
    assert!(changed > 40, "outline changed only {changed} pixels");
    // The ball's centre keeps its colour.
    let v = visible_list(&s, &cam, [256, 144]);
    let ball = v.iter().find(|v| v.id == ObjectId(2)).unwrap();
    let (cx, cy) = (((ball.rect[0] + ball.rect[2]) / 2.0) as usize, ((ball.rect[1] + ball.rect[3]) / 2.0) as usize);
    let i = (cy * 256 + cx) * 4;
    assert_eq!(plain.rgba[i..i + 4], lined.rgba[i..i + 4]);
}

#[test]
fn no_adapter_is_capture_unavailable() {
    assert_eq!(GpuContext::headless_on(pe_render::wgpu::Backends::empty(), AdapterChoice::Any).err(), Some(ContextError::NoAdapter));
    assert_eq!(CaptureError::from(ContextError::NoAdapter), CaptureError::Unavailable);
    let Some(mut r) = renderer() else { return };
    let (s, cam) = scene_at(Pos::ORIGIN);
    assert_eq!(r.capture(&s, cam, [4000, 10]).err(), Some(CaptureError::BadSize));
}
