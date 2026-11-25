//! Bullets (world-model.md 3.2): `Body.bullet` is the fork's swept body (box3d-fork D3), never Box3D's own bullet,
//! and it holds through pe-physics's rates and images.

use box3d_rust::body::{body_is_bullet, body_is_swept};
use pe_physics::{BodyDesc, BodyKind, Geometry, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc};

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((5u128 << 32) | u128::from(n))
}

fn bullet(at: [f64; 3], radius: f32, velocity: [f32; 3]) -> BodyDesc {
    let mut desc = BodyDesc::new(BodyKind::Dynamic, at)
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius }))
        .with_velocity(velocity, [0.0; 3]);
    desc.bullet = true;
    desc
}

#[test]
fn a_bullet_is_the_fork_s_swept_body_never_box3d_s_bullet() {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    pw.add_body(id(1), &bullet([0.0, 1.0, 0.0], 0.01, [0.0; 3])).unwrap();
    let body = pw.body_id(id(1)).unwrap();
    assert!(body_is_swept(pw.raw(), body));
    assert!(!body_is_bullet(pw.raw(), body));
}

/// A 1 cm wall (near face at x = 1) and a bullet of `radius` fired at it from x = 0 at `speed`, `aim` radians off its
/// normal, at `hz`: the largest x its centre reaches in a second.
fn farthest(hz: u32, speed: f32, radius: f32, aim: f32) -> f64 {
    let config = PhysicsConfig { gravity: [0.0; 3], ..PhysicsConfig::engine() }.with_tick_rate(hz).unwrap();
    let mut pw = PhysicsWorld::new(config).unwrap();
    let wall = BodyDesc::new(BodyKind::Static, [1.005, 0.0, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [0.005, 1.0, 1.0] }));
    pw.add_body(id(1), &wall).unwrap();
    let v = [speed * aim.cos(), speed * aim.sin(), 0.0];
    pw.add_body(id(2), &bullet([0.0, 0.0, 0.0], radius, v)).unwrap();
    let mut most = f64::NEG_INFINITY;
    for _ in 0..hz {
        pw.step(1.0 / hz as f32).unwrap();
        most = most.max(pw.pose(id(2)).unwrap().position[0]);
    }
    most
}

#[test]
fn a_bullet_never_crosses_a_thin_wall_at_30_60_or_120_hz() {
    for hz in [30, 60, 120] {
        for speed in [20.0f32, 100.0, 400.0] {
            for radius in [0.005f32, 0.03] {
                for k in 0..9 {
                    let aim = 0.15 * k as f32 - 0.6;
                    let x = farthest(hz, speed, radius, aim);
                    assert!(x < 1.005, "{hz} Hz, {speed} m/s, r {radius}, aim {aim}: centre at {x}");
                }
            }
        }
    }
}

#[test]
fn a_restored_image_replays_a_bullet_s_hits_exactly() {
    let config = PhysicsConfig::engine();
    let mut pw = PhysicsWorld::new(config).unwrap();
    let floor = BodyDesc::new(BodyKind::Static, [0.0, -0.5, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [10.0, 0.5, 10.0] }));
    pw.add_body(id(1), &floor).unwrap();
    for k in 0..6 {
        let b = BodyDesc::new(BodyKind::Dynamic, [2.0, 0.2 + 0.4 * f64::from(k), 0.0])
            .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [0.2, 0.2, 0.2] }));
        pw.add_body(id(10 + k), &b).unwrap();
    }
    pw.add_body(id(2), &bullet([-2.0, 1.0, 0.05], 0.03, [150.0, 0.0, 0.0])).unwrap();
    pw.step(1.0 / 60.0).unwrap();
    let image = pw.save_image().unwrap();
    let mut ahead = Vec::new();
    for _ in 0..60 {
        pw.step(1.0 / 60.0).unwrap();
        ahead.push(pw.check_hash().unwrap());
    }
    pw.restore_image(&image).unwrap();
    for (f, want) in ahead.iter().enumerate() {
        pw.step(1.0 / 60.0).unwrap();
        assert_eq!(&pw.check_hash().unwrap(), want, "frame {f} after the restore");
    }
    // The scene is all literals (no trigonometry), so tier 2 compares these lines across its four platforms: the
    // sweep's impacts, bit for bit.
    for (f, hash) in ahead.iter().enumerate().filter(|(f, _)| f % 15 == 14) {
        println!("RESULT bullets stack-hit frame {} {hash}", f + 2);
    }
}
