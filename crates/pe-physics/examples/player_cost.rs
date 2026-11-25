//! The cost of players (@item:primordial-engine/locomotion-l1-pe-physics-the-first-player-mover-m): the time
//! [`PhysicsWorld::move_player`] takes per player per frame, and the step time, in the M1 ball pit (850 balls, 6 cm,
//! 18 g) with 0 or 4 players, either on open floor beside the pit or wading through it, walking circles over 600
//! frames (10 s at 60 Hz).
//!
//! Every configuration runs `runs` times (default 3), interleaved so that drift hits all. Release only:
//! `cargo run --release --locked -p pe-physics --example player_cost [runs]`.
//!
//! Wall-clock times on a shared machine are noisy, so each configuration also prints what the step had to do, which
//! is exact: the mean count of awake bodies and a digest of the balls' final poses. Two configurations with the same
//! digest and counts stepped the same balls the same way, so any difference in their step times is the machine's.

use std::time::Instant;

use box3d_rust::world::world_get_awake_body_count;
use pe_physics::{BodyDesc, BodyKind, Geometry, MoverInput, ObjectId, PLAYER, PhysicsConfig, PhysicsWorld, ShapeDesc};

const DT: f32 = 1.0 / 60.0;
const FRAMES: usize = 600;
const BALLS: u32 = 850;
const PIT_W: f64 = 1.2;
const WALL_T: f32 = 0.1;
const WALL_H: f32 = 0.6;
const PLAYERS: u32 = 4;

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

#[derive(Clone, Copy, PartialEq)]
enum Where {
    None,
    /// On the floor, never moved: what their bodies alone cost the step.
    Idle,
    Floor,
    Pit,
}

/// Each player's circle: its centre and radius.
fn circle(at: Where, k: u32) -> ([f64; 2], f64) {
    match at {
        Where::Pit => ([0.3 * f64::from(k % 2) - 0.15, 0.3 * f64::from(k / 2) - 0.15], 0.5),
        _ => ([4.0 + 2.5 * f64::from(k % 2), 2.5 * f64::from(k / 2) - 1.25], 1.0),
    }
}

fn build(at: Where) -> PhysicsWorld {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).expect("a world");
    pw.add_body(id(1), &static_box([0.0, -0.5, 0.0], [10.0, 0.5, 10.0])).expect("floor");
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
    if at != Where::None {
        for k in 0..PLAYERS {
            let (c, r) = circle(at, k);
            pw.add_player(id(5_000 + k), [c[0] + r, 0.0, c[1]], &PLAYER).expect("player");
        }
    }
    pw
}

/// One run's results.
struct Run {
    /// The mean move time per player per frame, in microseconds.
    per: f64,
    /// The mean step time, in microseconds.
    step: f64,
    /// The most a frame's moves took, in microseconds.
    most: f64,
    /// The mean count of awake bodies over the frames, after each step.
    awake: f64,
    /// A digest (FNV-1a) of the balls' final positions and rotations, bit for bit.
    balls: u64,
}

fn run(at: Where) -> Run {
    let mut pw = build(at);
    let (mut moves, mut most, mut steps, mut awake) = (0.0, 0.0f64, 0.0, 0.0);
    for f in 0..FRAMES {
        let start = Instant::now();
        if at != Where::None && at != Where::Idle {
            for k in 0..PLAYERS {
                // Walking its circle at 2 m/s: the velocity along the tangent at the frame's angle.
                let (_, r) = circle(at, k);
                let angle = 2.0 * f64::from(f as u32) * f64::from(DT) / r;
                let walk = [(-angle.sin() * 2.0) as f32, (angle.cos() * 2.0) as f32];
                let input = MoverInput { walk, room: [0.0; 2], jump: f % 120 == 60 };
                pw.move_player(id(5_000 + k), &input, &[], &PLAYER, DT).expect("a move");
            }
        }
        let moved = start.elapsed().as_secs_f64() * 1.0e6;
        moves += moved;
        most = most.max(moved);
        let start = Instant::now();
        pw.step(DT).expect("a step");
        steps += start.elapsed().as_secs_f64() * 1.0e6;
        awake += f64::from(world_get_awake_body_count(pw.raw()));
    }
    let mut balls = 0xcbf2_9ce4_8422_2325u64;
    for i in 0..BALLS {
        let pose = pw.pose(id(100 + i)).expect("a ball");
        let bits =
            pose.position.iter().map(|c| c.to_bits()).chain(pose.rotation.iter().map(|c| u64::from(c.to_bits())));
        for word in bits {
            for byte in word.to_le_bytes() {
                balls = (balls ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    let per = if matches!(at, Where::None | Where::Idle) { 0.0 } else { moves / (FRAMES as f64 * f64::from(PLAYERS)) };
    Run { per, step: steps / FRAMES as f64, most, awake: awake / FRAMES as f64, balls }
}

/// The median, the minimum and the maximum.
fn spread(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    (v[v.len() / 2], v[0], v[v.len() - 1])
}

fn main() {
    let runs: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3).max(1);
    println!(
        "{FRAMES} frames, {BALLS} balls, {PLAYERS} players, {runs} runs each; µs: median (min..max) over the runs"
    );
    let configs = [
        (Where::None, "no players"),
        (Where::Idle, "players standing, unmoved"),
        (Where::Floor, "players on the floor"),
        (Where::Pit, "players in the pit"),
    ];
    let mut results: Vec<Vec<Run>> = configs.iter().map(|_| Vec::new()).collect();
    for _ in 0..runs {
        for (k, &(at, _)) in configs.iter().enumerate() {
            results[k].push(run(at));
        }
    }
    for (k, &(_, name)) in configs.iter().enumerate() {
        let (mv, mv_lo, mv_hi) = spread(results[k].iter().map(|r| r.per).collect());
        let (st, st_lo, st_hi) = spread(results[k].iter().map(|r| r.step).collect());
        let most = results[k].iter().map(|r| r.most).fold(0.0, f64::max);
        // The work is the same in every run of a configuration (the scene is deterministic); the first run's stands.
        let (awake, balls) = (results[k][0].awake, results[k][0].balls);
        let same = results[k].iter().all(|r| r.balls == balls && r.awake == awake);
        println!(
            "MEASURED {name:<22} move per player {mv:7.1} ({mv_lo:.1}..{mv_hi:.1})  step {st:8.1} ({st_lo:.1}..{st_hi:.1})  \
             most a frame's moves took {most:.1}"
        );
        println!(
            "EXACT    {name:<22} awake bodies {awake:7.1} (mean)  balls' digest {balls:016x}  same in every run: {same}"
        );
    }
}
