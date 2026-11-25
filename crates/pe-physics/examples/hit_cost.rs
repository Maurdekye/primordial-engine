//! The cost of hit events (@item:primordial-engine/collision-hit-events-from-pe-physics-through-pe-r): the step time
//! with every shape's hit events on, as pe-physics builds them, and off (cleared through the raw Box3D world, which
//! no engine path does), plus the time [`PhysicsWorld::hits`] takes and how many hits a frame has, in the M1 ball pit
//! with 850 balls (6 cm, 18 g) dropped in rows of 16 and layered upwards, settling over 600 frames (10 s at 60 Hz).
//!
//! Every configuration runs `runs` times (default 5), on and off interleaved so that drift hits both. Release only:
//! `cargo run --release --locked -p pe-physics --example hit_cost [runs]`.

use std::time::Instant;

use box3d_rust::body::body_get_shapes;
use box3d_rust::shape::shape_enable_hit_events;
use pe_physics::{BodyDesc, BodyKind, Geometry, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc};

const DT: f32 = 1.0 / 60.0;
const FRAMES: usize = 600;
const BALLS: u32 = 850;
const PIT_W: f64 = 1.2;
const WALL_T: f32 = 0.1;
const WALL_H: f32 = 0.6;

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((3u128 << 32) | u128::from(n))
}

fn static_box(at: [f64; 3], half: [f32; 3]) -> BodyDesc {
    BodyDesc::new(BodyKind::Static, at).with_shape(ShapeDesc::new(Geometry::Box { half_extents: half }))
}

/// A small fraction from `i` alone (the golden ratio's), for a deterministic jitter.
fn jitter(i: u32) -> f64 {
    (f64::from(i) * 0.618_033_988_75).fract()
}

fn build(hits: bool) -> PhysicsWorld {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).expect("a world");
    pw.add_body(id(1), &static_box([0.0, -0.5, 0.0], [2.0, 0.5, 2.0])).expect("floor");
    let (w, t, h) = (PIT_W, f64::from(WALL_T), f64::from(WALL_H));
    let side = (w + 2.0 * t) as f32;
    let walls = [
        ([w + t, h, 0.0], [WALL_T, WALL_H, side]),
        ([-w - t, h, 0.0], [WALL_T, WALL_H, side]),
        ([0.0, h, w + t], [side, WALL_H, WALL_T]),
        ([0.0, h, -w - t], [side, WALL_H, WALL_T]),
    ];
    for (k, (at, half)) in walls.into_iter().enumerate() {
        pw.add_body(id(2 + k as u32), &static_box(at, half)).expect("wall");
    }
    let spacing = 2.0 * w / 16.0;
    for i in 0..BALLS {
        let (col, row, layer) = (f64::from(i % 16), f64::from((i / 16) % 16), f64::from(i / 256));
        let x = -w + spacing * (col + 0.5) + 0.01 * (jitter(i) - 0.5);
        let z = -w + spacing * (row + 0.5) + 0.01 * (jitter(i + 7) - 0.5);
        let desc = BodyDesc::new(BodyKind::Dynamic, [x, 0.1 + 0.15 * layer + 0.6 * jitter(i + 13), z])
            .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.06 }).with_density(20.0));
        pw.add_body(id(100 + i), &desc).expect("ball");
    }
    if !hits {
        let ids: Vec<ObjectId> = pw.objects().collect();
        for object in ids {
            let body = pw.body_id(object).expect("a body");
            for shape in body_get_shapes(pw.raw(), body, 64) {
                shape_enable_hit_events(pw.raw_mut(), shape, false);
            }
        }
    }
    pw
}

/// One run: the mean step time and the mean `hits()` time in microseconds, and the hits over the run.
fn run(hits: bool) -> (f64, f64, usize) {
    let mut pw = build(hits);
    let (mut step_us, mut read_us, mut count) = (0.0, 0.0, 0);
    for _ in 0..FRAMES {
        let start = Instant::now();
        pw.step(DT).expect("a step");
        step_us += start.elapsed().as_secs_f64() * 1.0e6;
        let start = Instant::now();
        count += pw.hits().len();
        read_us += start.elapsed().as_secs_f64() * 1.0e6;
    }
    (step_us / FRAMES as f64, read_us / FRAMES as f64, count)
}

/// The median, the minimum and the maximum.
fn spread(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    (v[v.len() / 2], v[0], v[v.len() - 1])
}

fn main() {
    let runs: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5).max(1);
    println!("{FRAMES} frames, {BALLS} balls, {runs} runs each; times in µs: median (min..max) over the runs");
    let mut results: [Vec<(f64, f64, usize)>; 2] = [Vec::new(), Vec::new()];
    for _ in 0..runs {
        for (k, hits) in [true, false].into_iter().enumerate() {
            results[k].push(run(hits));
        }
    }
    let mut means = [0.0; 2];
    for (k, label) in ["on", "off"].into_iter().enumerate() {
        let (step, step_lo, step_hi) = spread(results[k].iter().map(|r| r.0).collect());
        let (read, read_lo, read_hi) = spread(results[k].iter().map(|r| r.1).collect());
        let count = results[k][0].2;
        means[k] = step;
        println!(
            "MEASURED hits {label:>3}: step mean {step:8.1} ({step_lo:.1}..{step_hi:.1})  hits() {read:6.2} \
             ({read_lo:.2}..{read_hi:.2})  hits over {FRAMES} frames {count}"
        );
    }
    println!("MEASURED on/off step ratio {:.3}", means[0] / means[1]);
}
