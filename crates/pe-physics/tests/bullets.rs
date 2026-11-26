//! Bullets (world-model.md 3.2): `Body.bullet` is the fork's swept body (box3d-fork D3), never Box3D's own bullet,
//! and it holds through pe-physics's rates and images. Hull bullets are swept too (CCD part 2 E), and every shape
//! pe-physics makes is one the sweep takes as a partner (a tripwire for height fields and compounds).

use box3d_rust::body::{body_get_shapes, body_is_bullet, body_is_swept};
use box3d_rust::shape::shape_get_type;
use box3d_rust::solver::sweeps_against;
use pe_physics::{BodyDesc, BodyKind, Geometry, Mesh, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc};

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

/// Points round a sphere of `radius`: `n` of them on a golden-angle spiral, all on the hull.
fn round(n: u32, radius: f32) -> Vec<[f32; 3]> {
    (0..n)
        .map(|k| {
            let y = 1.0 - 2.0 * (k as f32 + 0.5) / n as f32;
            let ring = (1.0 - y * y).sqrt();
            let phi = k as f32 * 2.399_963;
            [radius * ring * phi.cos(), radius * y, radius * ring * phi.sin()]
        })
        .collect()
}

/// A wall 1 cm from x = 1 to 1.01, facing the bullet: a box, or a one-sided mesh (two triangles at x = 1, front -x).
fn thin_wall(mesh: bool) -> BodyDesc {
    if mesh {
        let v = [[0.0, -1.0, -1.0], [0.0, -1.0, 1.0], [0.0, 1.0, 1.0], [0.0, 1.0, -1.0]];
        let quad = Mesh::new(&v, &[[0, 1, 2], [0, 2, 3]]).unwrap();
        let shape = ShapeDesc::new(Geometry::Mesh { mesh: quad, scale: [1.0; 3], both_sides: false });
        BodyDesc::new(BodyKind::Static, [1.0, 0.0, 0.0]).with_shape(shape)
    } else {
        BodyDesc::new(BodyKind::Static, [1.005, 0.0, 0.0])
            .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [0.005, 1.0, 1.0] }))
    }
}

/// Corners of a box of half extents `h`.
fn corners(h: [f32; 3]) -> Vec<[f32; 3]> {
    (0..8u32)
        .map(|k| {
            let s = |bit: u32| if k & bit != 0 { 1.0 } else { -1.0 };
            [h[0] * s(1), h[1] * s(2), h[2] * s(4)]
        })
        .collect()
}

/// Hull bullets are swept as spheres, capsules and boxes are (part 2's Q6): a tetrahedron and a cube 2 cm across, a
/// 32-point hull round a 10 cm ball, and a sliver 4 cm across and 2 mm thick, each spinning at 26 rad/s about a tilted
/// axis (capped at 23.6 at 30 Hz), never cross a 1 cm box wall or a one-sided mesh wall at 30, 60 or 120 Hz: their
/// centres never cross the wall's mid-plane inside its face. A bullet that slides along the face and off its
/// edge may end up beside the wall. The tetrahedron and the cube went through at 30 and 60 Hz until the sweep bounded a
/// pair that meets during a step to the step's end (CCD part 2 E).
#[test]
fn spinning_hull_bullets_never_cross_a_thin_wall_at_30_60_or_120_hz() {
    let hulls = [
        ("tetrahedron", vec![[0.01, 0.01, 0.01], [0.01, -0.01, -0.01], [-0.01, 0.01, -0.01], [-0.01, -0.01, 0.01]]),
        ("cube", corners([0.01; 3])),
        ("32 points", round(32, 0.05)),
        ("sliver", corners([0.02, 0.001, 0.02])),
    ];
    for hz in [30, 60, 120] {
        for (name, points) in &hulls {
            for mesh in [false, true] {
                let plane = if mesh { 1.0 } else { 1.005 };
                for speed in [20.0f32, 100.0, 400.0] {
                    for k in 0..9 {
                        let aim = 0.15 * k as f32 - 0.6;
                        let config =
                            PhysicsConfig { gravity: [0.0; 3], ..PhysicsConfig::engine() }.with_tick_rate(hz).unwrap();
                        let mut pw = PhysicsWorld::new(config).unwrap();
                        pw.add_body(id(1), &thin_wall(mesh)).unwrap();
                        let shape = ShapeDesc::new(Geometry::Hull { points: points.clone() }).with_density(7800.0);
                        let mut desc = BodyDesc::new(BodyKind::Dynamic, [-0.1, 0.0, 0.0])
                            .with_shape(shape)
                            .with_velocity([speed * aim.cos(), speed * aim.sin(), 0.0], [9.0, 20.0, -14.0]);
                        desc.bullet = true;
                        pw.add_body(id(2), &desc).unwrap();
                        let mut last = pw.pose(id(2)).unwrap().position;
                        for frame in 0..hz / 2 {
                            pw.step(1.0 / hz as f32).unwrap();
                            let p = pw.pose(id(2)).unwrap().position;
                            if (last[0] < plane) != (p[0] < plane) {
                                let f = (plane - last[0]) / (p[0] - last[0]);
                                let (y, z) = (last[1] + f * (p[1] - last[1]), last[2] + f * (p[2] - last[2]));
                                let wall = if mesh { "mesh" } else { "box" };
                                assert!(
                                    y.abs() > 0.98 || z.abs() > 0.98,
                                    "{name}, {hz} Hz, {speed} m/s, aim {aim}, {wall} wall: through at frame {frame}, \
                                     from {last:?} to {p:?}"
                                );
                            }
                            last = p;
                        }
                    }
                }
            }
        }
    }
}

/// A bullet rolling slowly on a floor costs no sweep query (CCD part 2 E): its contact holds it, so the sweep judges
/// it over the sub-step alone, where it can't close a slop. A pair with no contact this step is judged to the step's
/// end instead, which would sweep this one (0.8 m/s of closing: 3.3 mm a sub-step, 13 mm over a step's four).
#[test]
fn a_bullet_rolling_slowly_in_contact_costs_no_sweep_query() {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    let floor = BodyDesc::new(BodyKind::Static, [0.0, -0.5, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [10.0, 0.5, 10.0] }));
    pw.add_body(id(1), &floor).unwrap();
    pw.add_body(id(2), &bullet([0.0, 0.05, 0.0], 0.05, [0.0; 3]).with_velocity([0.4, 0.0, 0.0], [0.0, 0.0, -8.0]))
        .unwrap();
    for frame in 0..60 {
        pw.step(1.0 / 60.0).unwrap();
        let c = pw.raw().swept_counters;
        assert!(frame < 5 || c.iterations == 0, "frame {frame}: {c:?}");
    }
    assert!(pw.velocity(id(2)).unwrap().0[0] > 0.2, "still rolling");
}

/// A tripwire (CCD part 2 E): every shape pe-physics makes is one the bullet sweep takes as a partner, by the fork's
/// own list (`box3d_rust::solver::sweeps_against`: spheres, capsules, hulls and meshes; Box3D's height fields and
/// compounds are neither swept nor swept against, replica.md 9.2). A new [`Geometry`] kind stops this from compiling
/// until it is listed here, and a new Box3D shape type stops the fork's list compiling; one that makes a height field
/// or a compound fails this until the sweep takes that type.
#[test]
fn every_shape_pe_physics_makes_is_one_the_sweep_takes() {
    let quad = Mesh::new(&[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]], &[[0, 2, 1]]).unwrap();
    let samples = [
        Geometry::Sphere { center: [0.0; 3], radius: 0.1 },
        Geometry::Capsule { center1: [-0.1, 0.0, 0.0], center2: [0.1, 0.0, 0.0], radius: 0.05 },
        Geometry::Box { half_extents: [0.1; 3] },
        Geometry::Hull { points: vec![[0.0, 0.0, 0.0], [0.1, 0.0, 0.0], [0.0, 0.1, 0.0], [0.0, 0.0, 0.1]] },
        Geometry::Mesh { mesh: quad.clone(), scale: [1.0; 3], both_sides: false },
        Geometry::Mesh { mesh: quad, scale: [1.0; 3], both_sides: true },
    ];
    for geometry in samples {
        let name = match &geometry {
            Geometry::Sphere { .. } => "sphere",
            Geometry::Capsule { .. } => "capsule",
            Geometry::Box { .. } => "box",
            Geometry::Hull { .. } => "hull",
            Geometry::Mesh { both_sides: false, .. } => "mesh",
            Geometry::Mesh { both_sides: true, .. } => "mesh with both sides",
        };
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        pw.add_body(id(1), &BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(ShapeDesc::new(geometry))).unwrap();
        let shapes = body_get_shapes(pw.raw(), pw.body_id(id(1)).unwrap(), 8);
        assert!(!shapes.is_empty(), "{name}: no shape");
        for shape in shapes {
            let kind = shape_get_type(pw.raw(), shape);
            assert!(sweeps_against(kind), "{name} makes a {kind:?}, which the bullet sweep doesn't take");
        }
    }
}
