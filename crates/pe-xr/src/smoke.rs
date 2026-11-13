//! The scene the real-runtime smoke run draws (examples/xr_smoke; spec 10.3), here so a test can check it offline.
//!
//! pe-render draws only what a scene holds: there is no built-in ground, and an empty scene is the sky's clear
//! colour alone. The first headset run (2026-11-11) drew `Scene::default()` and showed a plain colour. So the smoke
//! scene carries its own floor, at y = 0 of the reference space (STAGE or LOCAL_FLOOR: the floor under the user),
//! and three objects at chest height ahead (-Z), so both the ground and depth are plain to see.

use pe_render::pose::{WorldPose, quat};
use pe_render::scene::{DEFAULT_APPEARANCE, DrawItem, Scene};
use pe_seams::{Appearance, ObjectId, Offset, Pos, Shape};

/// The floor's half size: 10 m each way, so from a standing head it reaches to about 9 degrees below the horizon.
pub const FLOOR_HALF: f32 = 10.0;

fn item(id: u128, at: [f64; 3], shape: Shape, color: [f32; 3]) -> DrawItem {
    let radius = match shape {
        Shape::Sphere { radius } => radius,
        Shape::Box { half_extents: h } => (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt(),
        Shape::Capsule { radius, half_height } => radius + half_height,
        Shape::Mesh(_) => 1.0,
    };
    DrawItem {
        id: ObjectId::from_u128(id),
        pose: WorldPose {
            pos: Pos::ORIGIN.checked_add(Offset(at)).expect("near the origin"),
            rot: quat([0.0, 0.0, 0.0, 1.0]),
        },
        scale: [1.0; 3],
        shape,
        appearance: Appearance { base_color: [color[0], color[1], color[2], 1.0], ..DEFAULT_APPEARANCE },
        radius,
        outline: None,
        ghost: None,
    }
}

/// A grey floor whose top face is y = 0, a red ball straight ahead, a blue ball to the left and a green capsule to
/// the right, 1.5 to 2 m away at chest height. In the rig's frame with the identity rig (the smoke run's).
pub fn scene() -> Scene {
    let mut s = Scene::default();
    s.items.push(item(
        1,
        [0.0, -0.05, 0.0],
        Shape::Box { half_extents: [FLOOR_HALF, 0.05, FLOOR_HALF] },
        [0.45, 0.45, 0.5],
    ));
    s.items.push(item(2, [0.0, 1.2, -1.5], Shape::Sphere { radius: 0.2 }, [0.85, 0.2, 0.15]));
    s.items.push(item(3, [-0.7, 1.0, -2.0], Shape::Sphere { radius: 0.25 }, [0.2, 0.4, 0.85]));
    s.items.push(item(4, [0.7, 1.0, -2.0], Shape::Capsule { radius: 0.15, half_height: 0.25 }, [0.3, 0.75, 0.3]));
    s
}
