//! The cost of bullets (box3d-fork D3, world-model.md 3.2): the step time in the M1 ball pit, 850 balls (6 cm, 18 g)
//! dropped in rows of 16 and layered upwards, with 0, 1, 10 and 100 bullets (1 cm, 50 g) fired across it at 20 to
//! 200 m/s from just above the walls, over 600 frames (10 s at 60 Hz). For each count it reports the mean step time,
//! the most distance queries a step spent, the holds and the sub-steps whose budget ran out. A run with an unlimited
//! budget shows what the budget must cover.
//!
//! Every configuration runs `runs` times (default 3), interleaved so that drift hits all. Release only:
//! `cargo run --release --locked -p pe-physics --example bullet_cost [runs]`.

use std::time::Instant;

use box3d_rust::world::world_get_awake_body_count;
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

fn build(bullets: u32, budget: i32) -> PhysicsWorld {
    let config = PhysicsConfig { swept_budget: budget, ..PhysicsConfig::engine() };
    let mut pw = PhysicsWorld::new(config).expect("a world");
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
    // Bullets from just above the walls, aimed down into the pit at 20 to 200 m/s.
    for k in 0..bullets {
        let angle = jitter(k + 31) * std::f64::consts::TAU;
        let (c, s) = (angle.cos(), angle.sin());
        let speed = 20.0 + 180.0 * jitter(k + 47) as f32;
        let at = [-c * (w + 0.5), 1.4 + 0.2 * jitter(k + 61), -s * (w + 0.5)];
        let dir = [c as f32, -0.5, s as f32];
        let n = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
        let mut desc = BodyDesc::new(BodyKind::Dynamic, at)
            .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.01 }).with_density(12_000.0))
            .with_velocity([speed * dir[0] / n, speed * dir[1] / n, speed * dir[2] / n], [0.0; 3]);
        desc.bullet = true;
        pw.add_body(id(5_000 + k), &desc).expect("bullet");
    }
    pw
}

/// One run's results: the mean step time in microseconds, the most queries a step spent, the holds, the exhausted
/// sub-steps, and the mean count of awake bodies after each step (exact: the same in every run).
type Run = (f64, i32, i32, i32, f64);

/// One run of `bullets` bullets with `budget` queries a sub-step.
fn run(bullets: u32, budget: i32) -> Run {
    let mut pw = build(bullets, budget);
    let (mut total, mut most, mut holds, mut exhausted, mut awake) = (0.0, 0, 0, 0, 0.0);
    for _ in 0..FRAMES {
        let start = Instant::now();
        pw.step(DT).expect("a step");
        total += start.elapsed().as_secs_f64() * 1.0e6;
        let c = pw.raw().swept_counters;
        most = most.max(c.iterations);
        holds += c.holds;
        exhausted += c.exhausted;
        awake += f64::from(world_get_awake_body_count(pw.raw()));
    }
    (total / FRAMES as f64, most, holds, exhausted, awake / FRAMES as f64)
}

/// The median, the minimum and the maximum.
fn spread(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    (v[v.len() / 2], v[0], v[v.len() - 1])
}

fn main() {
    let runs: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3).max(1);
    let budget = pe_physics::SWEPT_BUDGET;
    println!(
        "{FRAMES} frames, {BALLS} balls, {runs} runs each; mean step times in µs: median (min..max) over the runs"
    );
    let counts = [0u32, 1, 10, 100];
    let mut results: Vec<Vec<Run>> = vec![Vec::new(); counts.len()];
    for _ in 0..runs {
        for (k, &n) in counts.iter().enumerate() {
            results[k].push(run(n, budget));
        }
    }
    for (k, &n) in counts.iter().enumerate() {
        let (mean, lo, hi) = spread(results[k].iter().map(|r| r.0).collect());
        let (most, holds, exhausted, awake) = (results[k][0].1, results[k][0].2, results[k][0].3, results[k][0].4);
        println!(
            "MEASURED {n:>3} bullets, budget {budget}: step {mean:8.1} ({lo:.1}..{hi:.1})  most queries a step {most}  \
             holds {holds}  exhausted sub-steps {exhausted}  awake bodies {awake:.1} (mean)"
        );
    }
    let unlimited = run(100, pe_physics::limits::MAX_SWEPT_BUDGET);
    println!(
        "MEASURED 100 bullets, unlimited budget: step {:.1}  most queries a step {}  holds {}",
        unlimited.0, unlimited.1, unlimited.2
    );
}
