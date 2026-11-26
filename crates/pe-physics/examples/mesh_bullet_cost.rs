//! What CCD part 2's shapes cost (part 2 E): the hull builder per build (each differently placed hull is built again,
//! @coordinator-opus), bullets of each hull kind against spheres, bullets over a dense mesh floor, and a
//! scene of bullets at turning mesh paddles.
//!
//! `cargo run --release -p pe-physics --example mesh_bullet_cost`. Timings depend on the machine; they are printed as
//! COST lines, never RESULT lines (a RESULT line is a cross-platform contract). Counts are the same everywhere.

use std::time::Instant;

use pe_physics::{BodyDesc, BodyKind, Geometry, Mesh, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc, check_hull};

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((17u128 << 32) | u128::from(n))
}

fn corners(h: [f32; 3]) -> Vec<[f32; 3]> {
    (0..8u32)
        .map(|k| {
            let s = |bit: u32| if k & bit != 0 { 1.0 } else { -1.0 };
            [s(1) * h[0], s(2) * h[1], s(4) * h[2]]
        })
        .collect()
}

/// Points round a ball of radius `r`: the 98 normalized integer directions of the 5 x 5 x 5 cube's surface.
fn shell(r: f32) -> Vec<[f32; 3]> {
    let mut points = Vec::new();
    for i in -2i32..=2 {
        for j in -2i32..=2 {
            for k in -2i32..=2 {
                if i.abs().max(j.abs()).max(k.abs()) == 2 {
                    let (x, y, z) = (i as f32, j as f32, k as f32);
                    let l = (x * x + y * y + z * z).sqrt();
                    points.push([r * x / l, r * y / l, r * z / l]);
                }
            }
        }
    }
    points
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Microseconds per [`check_hull`] (the shape's checks and one build, or two when a big hull is built again at 44).
fn per_build(points: &[[f32; 3]]) -> f64 {
    let rounds = 2000;
    let samples: Vec<f64> = (0..5)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(check_hull(std::hint::black_box(points)).is_ok());
            }
            t.elapsed().as_secs_f64() * 1e6 / f64::from(rounds)
        })
        .collect();
    median(samples)
}

/// A flat mesh floor of `n` x `n` cells `cell` wide, centred on the origin, front up.
fn grid_floor(n: u32, cell: f32) -> Mesh {
    let half = n as f32 * cell / 2.0;
    let vertices: Vec<[f32; 3]> = (0..(n + 1) * (n + 1))
        .map(|k| [(k % (n + 1)) as f32 * cell - half, 0.0, (k / (n + 1)) as f32 * cell - half])
        .collect();
    let mut triangles = Vec::new();
    for z in 0..n {
        for x in 0..n {
            let a = z * (n + 1) + x;
            triangles.extend([[a, a + n + 1, a + n + 2], [a, a + n + 2, a + 1]]);
        }
    }
    Mesh::new(&vertices, &triangles).unwrap()
}

/// Mean step time (µs, after the first frame) and the most sweep queries in a step, over `frames` frames.
fn time(pw: &mut PhysicsWorld, frames: u32) -> (f64, i32) {
    pw.step(1.0 / 60.0).unwrap();
    let mut most = 0;
    let t = Instant::now();
    for _ in 1..frames {
        pw.step(1.0 / 60.0).unwrap();
        most = most.max(pw.raw().swept_counters.iterations);
    }
    (t.elapsed().as_secs_f64() * 1e6 / f64::from(frames - 1), most)
}

/// 40 bullets of `shape` fired at 150 m/s into a 1 cm box wall from 2 m, spinning, over a box floor.
fn volley(shape: &Geometry, bullet: bool) -> PhysicsWorld {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    let floor = BodyDesc::new(BodyKind::Static, [0.0, -0.5, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [10.0, 0.5, 10.0] }));
    pw.add_body(id(1), &floor).unwrap();
    let wall = BodyDesc::new(BodyKind::Static, [2.005, 1.0, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [0.005, 1.0, 3.0] }));
    pw.add_body(id(2), &wall).unwrap();
    for k in 0..40u32 {
        let at = [0.0, 0.3 + 0.04 * f64::from(k % 10), -1.5 + 0.75 * f64::from(k / 10)];
        let mut desc = BodyDesc::new(BodyKind::Dynamic, at)
            .with_shape(ShapeDesc::new(shape.clone()).with_density(7800.0))
            .with_velocity([150.0, 2.0, 0.5], [9.0, 20.0, -14.0]);
        desc.bullet = bullet;
        pw.add_body(id(10 + k), &desc).unwrap();
    }
    pw
}

fn main() {
    println!("Timings are rough (one machine, low priority); counts are exact.");
    // The hull builder, per build.
    let tetra = vec![[0.1, 0.1, 0.1], [0.1, -0.1, -0.1], [-0.1, 0.1, -0.1], [-0.1, -0.1, 0.1]];
    let builds: Vec<(&str, Vec<[f32; 3]>)> = vec![
        ("tetrahedron (4 points)", tetra.clone()),
        ("box (8)", corners([0.1; 3])),
        ("16 points round a ball", shell(0.5)[..16].to_vec()),
        ("32 points round a ball", shell(0.5)[..32].to_vec()),
        ("44 points round a ball (one build)", shell(0.5)[..44].to_vec()),
        ("64 points round a ball (built again at 44)", shell(0.5)[..64].to_vec()),
        ("98 points round a ball (built again at 44)", shell(0.5)),
    ];
    for (name, points) in &builds {
        println!("COST hull builder, {name}: {:.1} µs a build", per_build(points));
    }
    // Bullets of each hull kind against spheres.
    let kinds: Vec<(&str, Geometry)> = vec![
        ("sphere r 1 cm", Geometry::Sphere { center: [0.0; 3], radius: 0.01 }),
        ("tetrahedron 2 cm", Geometry::Hull { points: tetra.iter().map(|p| p.map(|c| c / 10.0)).collect() }),
        ("box 2 cm", Geometry::Box { half_extents: [0.01; 3] }),
        ("32-point hull 4 cm", Geometry::Hull { points: shell(0.02)[..32].to_vec() }),
        ("sliver 4 cm x 2 mm", Geometry::Hull { points: corners([0.02, 0.001, 0.02]) }),
    ];
    for (name, shape) in &kinds {
        let (swept, most) = time(&mut volley(shape, true), 60);
        let (plain, _) = time(&mut volley(shape, false), 60);
        println!(
            "COST 40 spinning {name} bullets at a wall: step {swept:.1} µs swept, {plain:.1} unflagged; most queries a \
             step {most}"
        );
    }
    // Bullets over a dense mesh floor (5,000 triangles) and the same floor as a box.
    for mesh in [true, false] {
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        let floor = if mesh {
            BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(ShapeDesc::new(Geometry::Mesh {
                mesh: grid_floor(50, 0.2),
                scale: [1.0; 3],
                both_sides: false,
            }))
        } else {
            BodyDesc::new(BodyKind::Static, [0.0, -0.5, 0.0])
                .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [5.0, 0.5, 5.0] }))
        };
        pw.add_body(id(1), &floor).unwrap();
        for k in 0..10u32 {
            let mut desc = BodyDesc::new(BodyKind::Dynamic, [-4.5, 0.012, -4.0 + 0.8 * f64::from(k)])
                .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.01 }))
                .with_velocity([100.0, 0.0, 3.0], [0.0; 3]);
            desc.bullet = true;
            pw.add_body(id(10 + k), &desc).unwrap();
        }
        let (step, most) = time(&mut pw, 6);
        let floor = if mesh { "a 5,000-triangle mesh floor" } else { "a box floor" };
        println!("COST 10 bullets skimming {floor} at 100 m/s: step {step:.1} µs, most queries a step {most}");
    }
    // Bullets at three turning mesh paddles (closed box meshes, 1 m arms, 30 rad/s).
    for bullet in [true, false] {
        let mut pw = PhysicsWorld::new(PhysicsConfig { gravity: [0.0; 3], ..PhysicsConfig::engine() }).unwrap();
        let (v, t) = {
            let c = corners([1.0, 0.1, 0.025]);
            let t = vec![
                [0, 2, 1],
                [1, 2, 3],
                [4, 5, 6],
                [5, 7, 6],
                [0, 1, 4],
                [1, 5, 4],
                [2, 6, 3],
                [3, 6, 7],
                [0, 4, 2],
                [2, 4, 6],
                [1, 3, 5],
                [3, 7, 5],
            ];
            (c, t)
        };
        let paddle = Mesh::new(&v, &t).unwrap();
        for k in 0..3u32 {
            let desc = BodyDesc::new(BodyKind::Kinematic, [3.0 * f64::from(k), 0.0, 0.0])
                .with_shape(ShapeDesc::new(Geometry::Mesh { mesh: paddle.clone(), scale: [1.0; 3], both_sides: false }))
                .with_velocity([0.0; 3], [0.0, 30.0, 0.0]);
            pw.add_body(id(1 + k), &desc).unwrap();
        }
        for k in 0..30u32 {
            let at = [3.0 * f64::from(k % 3) - 0.5 + 0.1 * f64::from(k / 3), 0.0, 3.0 + 0.2 * f64::from(k / 3)];
            let mut desc = BodyDesc::new(BodyKind::Dynamic, at)
                .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.02 }))
                .with_velocity([0.0, 0.0, -60.0], [0.0; 3]);
            desc.bullet = bullet;
            pw.add_body(id(10 + k), &desc).unwrap();
        }
        let (step, most) = time(&mut pw, 30);
        let what = if bullet { "bullets" } else { "unflagged balls" };
        println!("COST 30 {what} at three paddles turning at 30 rad/s: step {step:.1} µs, most queries a step {most}");
    }
}
