//! Write a capture of a small test scene to a PNG, on the software adapter (never the GPU):
//! `cargo run -p pe-render --example capture_png -- out.png [cue]`. With `cue`, one ball carries the correction cue
//! (outline and ghost), as the display layer would draw it. With `labels`, two balls are highlighted and four
//! objects named, as `view_capture` with `highlight` and `label: true` draws them.

use std::time::Duration;

use glam::DQuat;
use pe_render::camera::{Camera, DepthRange, Fov};
use pe_render::gpu::{AdapterChoice, GpuContext};
use pe_render::pose::WorldPose;
use pe_render::scene::{DEFAULT_APPEARANCE, DrawItem, RenderSettings, Renderer, Scene};
use pe_seams::{Appearance, Light, LightKind, ObjectId, Offset, Pos, Shape};

fn at(d: [f64; 3]) -> Pos {
    Pos::ORIGIN.checked_add(Offset(d)).unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).cloned().unwrap_or_else(|| "capture.png".into());
    let cue = args.iter().any(|a| a == "cue");
    let labels = args.iter().any(|a| a == "labels");
    let ctx = GpuContext::headless(AdapterChoice::SoftwareOnly).expect("software adapter");
    eprintln!("adapter: {}", ctx.describe());
    let mut r = Renderer::new(ctx, RenderSettings::default());
    let mut s = Scene::default();
    let mut add = |id, p, shape, c: [f32; 4], metal: f32, rough: f32| {
        let radius = match shape {
            Shape::Sphere { radius } => radius,
            Shape::Box { half_extents: h } => (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt(),
            Shape::Capsule { radius, half_height } => radius + half_height,
            _ => 1.0,
        };
        s.items.push(DrawItem {
            id: ObjectId::from_u128(id),
            pose: WorldPose { pos: at(p), rot: DQuat::from_rotation_y(0.6) },
            scale: [1.0; 3],
            shape,
            appearance: Appearance { base_color: c, metalness: metal, roughness: rough, ..DEFAULT_APPEARANCE },
            radius,
            outline: None,
            ghost: None,
        });
    };
    add(1, [0.0, -0.05, -3.0], Shape::Box { half_extents: [3.0, 0.05, 3.0] }, [0.45, 0.45, 0.5, 1.0], 0.0, 0.8);
    add(2, [-0.5, 0.3, -3.0], Shape::Sphere { radius: 0.3 }, [0.8, 0.15, 0.1, 1.0], 0.0, 0.4);
    add(3, [0.45, 0.2, -2.5], Shape::Sphere { radius: 0.2 }, [0.15, 0.35, 0.85, 1.0], 0.0, 0.3);
    add(4, [0.15, 0.4, -3.6], Shape::Capsule { radius: 0.15, half_height: 0.25 }, [0.25, 0.7, 0.3, 1.0], 0.0, 0.5);
    add(5, [-0.1, 0.15, -2.3], Shape::Box { half_extents: [0.15, 0.15, 0.15] }, [0.9, 0.75, 0.3, 1.0], 1.0, 0.25);
    s.lights.push(pe_render::scene::LightItem {
        pose: WorldPose { pos: at([0.8, 0.8, -2.0]), rot: DQuat::IDENTITY },
        light: Light { kind: LightKind::Point, color: [1.0, 0.6, 0.3], intensity: 1.2, range: 4.0 },
    });
    if cue {
        let b = &mut s.items[2];
        b.outline = Some([0.95, 0.95, 1.0, 0.8]);
        let mut g = b.pose;
        g.pos = at([0.75, 0.2, -2.5]);
        b.ghost = Some((g, 0.3));
    }
    let cam = Camera::placed(
        WorldPose { pos: at([0.0, 1.3, -0.4]), rot: DQuat::from_rotation_x(-0.42) },
        Fov::symmetric(1.0, 16.0 / 9.0),
        DepthRange::DESKTOP,
    );
    let size = [768, 432];
    let names: Vec<(ObjectId, String)> = if labels {
        s.highlight(&[ObjectId::from_u128(2), ObjectId::from_u128(3)]);
        [(2, "Red ball"), (3, "Синий шар"), (4, "Green capsule"), (5, "Gold cube (metal)")]
            .into_iter()
            .map(|(i, n)| (ObjectId::from_u128(i), n.to_string()))
            .collect()
    } else {
        Vec::new()
    };
    let mut img = r
        .capture(&s, cam, size)
        .expect("capture")
        .wait(&r.context().device, Duration::from_secs(60))
        .expect("readback");
    img.draw_labels(&pe_render::labels::place_labels(&s, &cam, size, &names));
    std::fs::write(&out, img.encode_png()).expect("write");
    eprintln!("wrote {out}");
}
