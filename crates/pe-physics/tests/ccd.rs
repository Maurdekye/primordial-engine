//! Continuous collision against static geometry (@coordinator-opus's push-tunnelling ruling, part (c)).
//!
//! pe-physics builds its world from Box3D's default definition, which has continuous collision on. At the end of each
//! step a fast dynamic body gets a time of impact against static shapes (the fast-body pass of Box3D's
//! `finalize_bodies`); bullets get one against dynamic bodies too. These tests pin that setting in the engine's world
//! and hold the M1 ball pit to it. They never switch it on themselves, so a change that turned it off would fail them.
//! - A pit ball at the push caps' top speed (world-model.md 8.1: at most 5 m/s per push, 10 m/s in all) stays in
//!   the pit, aimed at each wall, into each corner, straight down, and down into each seam where a wall meets the
//!   floor.
//! - Much smaller balls stay in too, against the pit's walls and against 1 cm walls, at the top speed and at 50 m/s
//!   (`max_push_speed` can be raised to no limit). At Box3D's own top linear speed, 400 m/s, a small ball rolling
//!   along the floor can go through a wall: the measurement scans the speeds between.
//! - The same scene, run twice, gives the same state hash every frame.
//!
//! "Stays in the pit" means a ball never passes through a wall or the floor. A push aimed upwards at the cap can carry
//! a ball over the 1.2 m walls, which is ordinary play.
//!
//! The pit's measurements mirror pe-world's preset (crates/pe-world/src/preset.rs): `BALL_R` and `BALL_DENSITY`, the
//! inner half width `PIT_W` and the walls' half height `WALL_H`, its walls' 0.1 m half thickness and its floor box.
//! A change there should change them here.

use pe_physics::{BodyDesc, BodyKind, Geometry, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc};

const DT: f32 = 1.0 / 60.0;
/// The engine's gravity (`PhysicsConfig::engine`), m/s², for aiming a fall at a seam.
const G: f64 = 10.0;
/// The push caps' top speed (world-model.md 8.1, `max_push_speed`), m/s.
const TOP_SPEED: f64 = 10.0;
/// Box3D's top linear speed (its world definition's `maximum_linear_speed`), m/s.
const BOX3D_TOP_SPEED: f64 = 400.0;
/// pe-world's preset: the pit's inner half width, its walls' half thickness and half height (the floor's top is
/// y = 0), and its balls' radius and density (18 g).
const PIT_W: f64 = 1.2;
const WALL_T: f32 = 0.1;
const WALL_H: f32 = 0.6;
const BALL_R: f64 = 0.06;
const BALL_DENSITY: f32 = 20.0;
/// The walls' horizontal directions, and each corner's.
const WALLS: [[f64; 2]; 4] = [[1.0, 0.0], [-1.0, 0.0], [0.0, 1.0], [0.0, -1.0]];
const CORNERS: [[f64; 2]; 4] = [[1.0, 1.0], [-1.0, 1.0], [1.0, -1.0], [-1.0, -1.0]];

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((2u128 << 32) | u128::from(n))
}

fn static_box(at: [f64; 3], half: [f32; 3]) -> BodyDesc {
    BodyDesc::new(BodyKind::Static, at).with_shape(ShapeDesc::new(Geometry::Box { half_extents: half }))
}

/// The pit: its floor and four walls, as the preset places them. A wall `half_thickness` other than the preset's
/// moves only the walls' outer faces.
fn pit(pw: &mut PhysicsWorld, half_thickness: f32) {
    pw.add_body(id(1), &static_box([0.0, -0.5, 0.0], [2.0, 0.5, 2.0])).unwrap();
    let (w, t, h) = (PIT_W, f64::from(half_thickness), f64::from(WALL_H));
    let side = (w + 2.0 * t) as f32;
    let walls = [
        ([w + t, h, 0.0], [half_thickness, WALL_H, side]),
        ([-w - t, h, 0.0], [half_thickness, WALL_H, side]),
        ([0.0, h, w + t], [side, WALL_H, half_thickness]),
        ([0.0, h, -w - t], [side, WALL_H, half_thickness]),
    ];
    for (k, (at, half)) in walls.into_iter().enumerate() {
        pw.add_body(id(2 + k as u32), &static_box(at, half)).unwrap();
    }
}

/// Where a ball of radius `r` starts, and its velocity, for each aim at `speed`:
/// - along the floor from the pit's centre, at each wall and into each corner;
/// - straight down from 1 m;
/// - down at 45° from above the pit's centre, into the seam where each wall meets the floor, and down along the
///   diagonal into one corner's seam, starting high enough that the fall under gravity lands it there.
fn aims(r: f64, speed: f64) -> Vec<([f64; 3], [f32; 3])> {
    let mut out = Vec::new();
    for d in WALLS.into_iter().chain(CORNERS) {
        let n = d[0].hypot(d[1]);
        out.push(([0.0, r, 0.0], [(speed * d[0] / n) as f32, 0.0, (speed * d[1] / n) as f32]));
    }
    out.push(([0.0, 1.0, 0.0], [0.0, -speed as f32, 0.0]));
    let seams = WALLS.into_iter().map(|d| (d, 2.0f64.sqrt())).chain([([1.0, 1.0], 3.0f64.sqrt())]);
    for (d, n) in seams {
        let along = speed / n;
        let t = (PIT_W - r) / along;
        let start = [0.0, r + along * t + 0.5 * G * t * t, 0.0];
        out.push((start, [(along * d[0]) as f32, -along as f32, (along * d[1]) as f32]));
    }
    out
}

/// What became of one ball.
struct Fate {
    /// How far its centre got from the pit's centre along x or z.
    far: f64,
    /// The lowest its bottom got.
    low: f64,
    /// The deepest it got into a wall or the floor, its surface past the face, while it was stopped there.
    depth: f64,
    /// It went through: its centre got past a wall's outer face or below the floor's bottom, or it ended outside
    /// the pit.
    through: bool,
}

/// One ball of radius `r`, from `start` at `velocity`, stepped for two seconds in a pit with walls of
/// `half_thickness`. `ccd_off` switches continuous collision off, for the measurement only; otherwise the world is
/// the engine's, untouched.
fn run(r: f64, (start, velocity): ([f64; 3], [f32; 3]), half_thickness: f32, ccd_off: bool) -> Fate {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    if ccd_off {
        pw.raw_mut().enable_continuous(false);
    }
    pit(&mut pw, half_thickness);
    let shape = ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: r as f32 }).with_density(BALL_DENSITY);
    pw.add_body(id(9), &BodyDesc::new(BodyKind::Dynamic, start).with_shape(shape).with_velocity(velocity, [0.0; 3]))
        .unwrap();
    let outer = PIT_W + 2.0 * f64::from(half_thickness);
    let mut fate = Fate { far: 0.0, low: f64::MAX, depth: 0.0, through: false };
    let mut p = start;
    for _ in 0..120 {
        pw.step(DT).unwrap();
        p = pw.pose(id(9)).unwrap().position;
        let side = p[0].abs().max(p[2].abs());
        fate.far = fate.far.max(side);
        fate.low = fate.low.min(p[1] - r);
        fate.depth = fate.depth.max(side + r - PIT_W).max(r - p[1]);
        fate.through |= side > outer || p[1] < -1.0;
    }
    fate.through |= p[0].abs().max(p[2].abs()) >= PIT_W || p[1] < 0.0;
    fate
}

/// The engine's world has continuous collision on, at every tick rate it allows.
#[test]
fn the_engine_s_world_has_continuous_collision_on() {
    for hz in [30, 60, 120] {
        let config = PhysicsConfig::engine().with_tick_rate(hz).unwrap();
        assert!(PhysicsWorld::new(config).unwrap().raw().enable_continuous, "at {hz} Hz");
    }
}

/// The ruling's test: a pit ball at the push caps' top speed stays in the pit, whatever it is aimed at. Its centre
/// never passes a wall's inner face, and it never sinks into the floor.
#[test]
fn a_pit_ball_at_the_push_cap_stays_in_the_pit() {
    for (k, aim) in aims(BALL_R, TOP_SPEED).into_iter().enumerate() {
        let Fate { far, low, through, .. } = run(BALL_R, aim, WALL_T, false);
        assert!(!through, "aim {k} {aim:?}: it went through");
        assert!(far < PIT_W, "aim {k} {aim:?}: its centre reached {far:.3} m (the inner face is at {PIT_W} m)");
        assert!(low > -0.01, "aim {k} {aim:?}: it sank to {low:.3} m");
    }
}

/// Balls far smaller than the pit's stay in too, never going through a wall or the floor: against the pit's walls and
/// against walls 1 cm thick, at the top speed and at 50 m/s. How deep they press in on the way, and what happens
/// faster, is the measurement's to report.
#[test]
fn small_fast_balls_stay_in() {
    for r in [0.02, 0.01, 0.005] {
        for half_thickness in [WALL_T, 0.005] {
            for speed in [TOP_SPEED, 50.0] {
                for (k, aim) in aims(r, speed).into_iter().enumerate() {
                    let fate = run(r, aim, half_thickness, false);
                    let case = format!("radius {r}, wall {half_thickness}, {speed} m/s, aim {k} {aim:?}");
                    assert!(
                        !fate.through,
                        "{case}: it went through (reached {:.3} m, low {:.3} m)",
                        fate.far, fate.low
                    );
                }
            }
        }
    }
}

/// The same scene gives the same state hash every frame: thirty 1 cm balls at the top speed, fanned out at walls 1 cm
/// thick, each meeting one within a few frames. It prints the hash of all 120 frames' hashes, to compare across
/// machines and targets: `cargo test --release -p pe-physics --test ccd -- --nocapture`.
#[test]
fn continuous_collision_is_deterministic() {
    let hashes = || {
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        pit(&mut pw, 0.005);
        for i in 0..30u32 {
            let angle = f64::from(i) * std::f64::consts::TAU / 30.0;
            let (c, s) = (angle.cos(), angle.sin());
            let desc = BodyDesc::new(BodyKind::Dynamic, [0.3 * c, 0.01, 0.3 * s])
                .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.01 }))
                .with_velocity([(TOP_SPEED * c) as f32, 0.0, (TOP_SPEED * s) as f32], [0.0; 3]);
            pw.add_body(id(100 + i), &desc).unwrap();
        }
        (0..120)
            .map(|_| {
                pw.step(DT).unwrap();
                pw.state_hash().unwrap()
            })
            .collect::<Vec<u64>>()
    };
    let (a, b) = (hashes(), hashes());
    assert_eq!(a, b);
    let all = a.iter().fold(blake3::Hasher::new(), |mut h, x| {
        h.update(&x.to_le_bytes());
        h
    });
    println!("continuous_collision_is_deterministic: {}", all.finalize().to_hex());
}

/// A measurement, not a check: with continuous collision on and off, for each ball, wall and speed, which aims went
/// through and the deepest any ball pressed into a wall or the floor. Run it with
/// `cargo test -p pe-physics --test ccd -- --ignored --nocapture`.
#[test]
#[ignore]
fn measure_with_and_without_continuous_collision() {
    for ccd_off in [false, true] {
        for r in [BALL_R, 0.02, 0.01, 0.005] {
            for half_thickness in [WALL_T, 0.005] {
                for speed in [TOP_SPEED, 50.0, 75.0, 100.0, 150.0, 200.0, 300.0, BOX3D_TOP_SPEED] {
                    let fates: Vec<Fate> =
                        aims(r, speed).into_iter().map(|a| run(r, a, half_thickness, ccd_off)).collect();
                    let through: Vec<usize> = (0..fates.len()).filter(|&k| fates[k].through).collect();
                    let depth = fates.iter().map(|f| f.depth).fold(0.0, f64::max);
                    let ccd = if ccd_off { "off" } else { "on" };
                    let case = format!("CCD {ccd}, radius {r}, wall {half_thickness}, {speed} m/s");
                    println!("{case}: through {through:?} of {}, deepest {depth:.4} m", fates.len());
                }
            }
        }
    }
}
