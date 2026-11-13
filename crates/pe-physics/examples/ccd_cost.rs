//! The cost of continuous collision against static geometry (@coordinator-opus's push-tunnelling ruling, part (c),
//! madc2d14d5bf5 and mf97c83462593): the step time with Box3D's continuous collision on and off, on two scenes in the
//! M1 ball pit (tests/ccd.rs has its geometry):
//! - `pit`: 500 pit balls (6 cm, 18 g) dropped in rows of 16 across the pit and layered upwards, settling;
//! - `fast`: 500 balls of 1 cm at the push caps' top speed (10 m/s), aimed every way at the walls.
//!
//! Each scene steps 600 frames (10 s at 60 Hz). Every configuration runs `runs` times (default 5), on and off
//! interleaved so that drift hits both. For each run it reports the mean step time, and the median and the 90th
//! percentile of the single steps. Over the runs it gives the median of those, with the minimum and the maximum as the
//! spread. Release only, on a quiet machine, at normal priority (HEAVY_PRIO=normal under heavy.sh):
//! `cargo run --release --locked -p pe-physics --example ccd_cost [runs]`.

use std::time::Instant;

use pe_physics::{BodyDesc, BodyKind, Geometry, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc};

const DT: f32 = 1.0 / 60.0;
const FRAMES: usize = 600;
const BALLS: u32 = 500;
const TOP_SPEED: f32 = 10.0;
const PIT_W: f64 = 1.2;
const WALL_T: f32 = 0.1;
const WALL_H: f32 = 0.6;

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((3u128 << 32) | u128::from(n))
}

fn static_box(at: [f64; 3], half: [f32; 3]) -> BodyDesc {
    BodyDesc::new(BodyKind::Static, at).with_shape(ShapeDesc::new(Geometry::Box { half_extents: half }))
}

fn ball(r: f32, at: [f64; 3], velocity: [f32; 3]) -> BodyDesc {
    BodyDesc::new(BodyKind::Dynamic, at)
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: r }).with_density(20.0))
        .with_velocity(velocity, [0.0; 3])
}

/// A small fraction from `i` alone (the golden ratio's), for a deterministic jitter.
fn jitter(i: u32) -> f64 {
    (f64::from(i) * 0.618_033_988_75).fract()
}

fn build(scene: &str, continuous: bool) -> PhysicsWorld {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).expect("a world");
    pw.raw_mut().enable_continuous(continuous);
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
        let desc = match scene {
            "pit" => ball(0.06, [x, 0.1 + 0.15 * layer + 0.6 * jitter(i + 13), z], [0.0; 3]),
            _ => {
                let angle = jitter(i + 29) * std::f64::consts::TAU;
                let v = [TOP_SPEED * angle.cos() as f32, 0.0, TOP_SPEED * angle.sin() as f32];
                ball(0.01, [x, 0.01 + 0.05 * layer, z], v)
            }
        };
        pw.add_body(id(100 + i), &desc).expect("ball");
    }
    pw
}

/// One run: (the mean, the median and the 90th percentile) of its step times, in microseconds.
fn run(scene: &str, continuous: bool) -> [f64; 3] {
    let mut pw = build(scene, continuous);
    let mut steps: Vec<f64> = (0..FRAMES)
        .map(|_| {
            let start = Instant::now();
            pw.step(DT).expect("a step");
            start.elapsed().as_secs_f64() * 1.0e6
        })
        .collect();
    let mean = steps.iter().sum::<f64>() / steps.len() as f64;
    steps.sort_by(f64::total_cmp);
    [mean, steps[steps.len() / 2], steps[steps.len() * 9 / 10]]
}

/// The median, the minimum and the maximum.
fn spread(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    (v[v.len() / 2], v[0], v[v.len() - 1])
}

fn main() {
    let runs: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5).max(1);
    println!("{FRAMES} frames, {BALLS} balls, {runs} runs each; step times in µs: median (min..max) over the runs");
    for scene in ["pit", "fast"] {
        let mut results: [Vec<[f64; 3]>; 2] = [Vec::new(), Vec::new()];
        for _ in 0..runs {
            for (k, continuous) in [true, false].into_iter().enumerate() {
                results[k].push(run(scene, continuous));
            }
        }
        let mut means = [0.0; 2];
        for (k, label) in ["on", "off"].into_iter().enumerate() {
            let column = |c: usize| spread(results[k].iter().map(|r| r[c]).collect());
            let ((mean, mean_lo, mean_hi), (p50, p50_lo, p50_hi), (p90, p90_lo, p90_hi)) =
                (column(0), column(1), column(2));
            means[k] = mean;
            println!(
                "{scene:>4} CCD {label:>3}: mean {mean:8.1} ({mean_lo:.1}..{mean_hi:.1})  p50 {p50:8.1} \
                 ({p50_lo:.1}..{p50_hi:.1})  p90 {p90:8.1} ({p90_lo:.1}..{p90_hi:.1})"
            );
        }
        println!("{scene:>4} on/off mean ratio {:.3}", means[0] / means[1]);
    }
}
