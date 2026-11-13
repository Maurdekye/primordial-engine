//! Continuous collision against static geometry (@coordinator-opus's push-tunnelling ruling, part (c)).
//!
//! pe-physics builds its world from Box3D's default definition, which has continuous collision on. At the end of each
//! step a fast dynamic body gets a time of impact against static shapes (the fast-body pass of Box3D's
//! `finalize_bodies`); bullets get one against dynamic bodies too. These tests pin that setting in the engine's world
//! and hold the M1 ball pit to it. They never switch it on themselves, so a change that turned it off would fail them.
//! - A pit ball at the push caps' top speed (world-model.md 8.1: at most 5 m/s per push, 10 m/s in all) stays in
//!   the pit, whatever it is aimed at: 73 aims along the floor, level in flight, straight down, and down into the
//!   seams where the walls meet the floor.
//! - Much smaller balls stay in too, against the pit's walls and against 1 cm walls, at the top speed and at 50 m/s
//!   (`max_push_speed` can be raised to no limit). Faster, up to Box3D's own top linear speed of 400 m/s, a small
//!   ball can tunnel through a thin wall or end embedded in a thick one: the measurement scans the speeds between and
//!   tells the outcomes apart.
//! - The same scene, run twice, gives the same state hash every frame, both within the tested speeds and above them,
//!   where small balls go through 1 cm walls. Each prints its hash, to compare across machines.
//!
//! "Stays in the pit" means a ball never passes through a wall or the floor. A push aimed upwards at the cap can carry
//! a ball over the 1.2 m walls, which is ordinary play.
//!
//! The pit's measurements mirror pe-world's preset (crates/pe-world/src/preset.rs): `BALL_R` and `BALL_DENSITY`, the
//! inner half width `PIT_W` and the walls' half height `WALL_H`, its walls' 0.1 m half thickness and its floor box.
//! A change there should change them here. The tests run under `PhysicsConfig::engine`'s gravity, 10 m/s², and the
//! seam aims are solved for it (`G`). pe-world's default of 9.81 m/s² replaces it in the replica, which changes
//! nothing here; change `G` only with the config it mirrors (@design-reviewer's L2 on 1cda443e).

use pe_physics::{BodyDesc, BodyKind, Geometry, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc};

/// The world rates the user allows (30 to 120 Hz): its ends and the default.
const RATES: [u32; 3] = [30, 60, 120];
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

/// Where a ball of radius `r` starts, and its velocity, for each aim at `speed` (73 aims, @coordinator-opus's wider
/// sweep):
/// - 32 directions along the floor from the pit's centre, rolling, every 11.25°;
/// - 16 directions level from 0.3 m up, in flight and not spinning, every 22.5°;
/// - straight down from 1 m;
/// - down at 30°, 45° and 60° below level, towards each wall's middle and each corner, starting high enough that the
///   fall under gravity lands the ball in the seam where the wall meets the floor (in the corner, both walls').
fn aims(r: f64, speed: f64) -> Vec<([f64; 3], [f32; 3])> {
    let level = |angle: f64, height: f64| {
        let (c, s) = (angle.cos(), angle.sin());
        ([0.0, height, 0.0], [(speed * c) as f32, 0.0, (speed * s) as f32])
    };
    let tau = std::f64::consts::TAU;
    let mut out: Vec<([f64; 3], [f32; 3])> = (0..32).map(|k| level(tau * f64::from(k) / 32.0, r)).collect();
    out.extend((0..16).map(|k| level(tau * f64::from(k) / 16.0, 0.3)));
    out.push(([0.0, 1.0, 0.0], [0.0, -speed as f32, 0.0]));
    for degrees in [30.0f64, 45.0, 60.0] {
        let (along, down) = (speed * degrees.to_radians().cos(), speed * degrees.to_radians().sin());
        for d in WALLS.into_iter().chain(CORNERS) {
            let n = d[0].hypot(d[1]);
            let (dx, dz) = (d[0] / n, d[1] / n);
            // The ball meets a wall's inner face when its centre is `r` short of it, along the larger component.
            let t = (PIT_W - r) / (along * dx.abs().max(dz.abs()));
            let start = [0.0, r + down * t + 0.5 * G * t * t, 0.0];
            out.push((start, [(along * dx) as f32, -down as f32, (along * dz) as f32]));
        }
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
    /// the pit. `went` says which.
    through: bool,
    went: Went,
}

/// How a ball left the pit, if it did. The first of these to happen decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Went {
    Stayed,
    /// Its centre got past a wall's outer face below the wall's top, or below the floor's bottom.
    Tunnelled,
    /// It got past a wall's outer face with its bottom above the wall's top: deflected up and over, which is play.
    OverTheTop,
    /// It ended inside a wall or the floor: its centre past a wall's inner face, or below the floor's top.
    Embedded,
}

/// One ball of radius `r`, from `start` at `velocity`, stepped for two seconds at `hz` in a pit with walls of
/// `half_thickness`. `ccd_off` switches continuous collision off, for the measurement only; otherwise the world is
/// the engine's at that rate, untouched.
fn run(hz: u32, r: f64, (start, velocity): ([f64; 3], [f32; 3]), half_thickness: f32, ccd_off: bool) -> Fate {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine().with_tick_rate(hz).unwrap()).unwrap();
    if ccd_off {
        pw.raw_mut().enable_continuous(false);
    }
    pit(&mut pw, half_thickness);
    let shape = ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: r as f32 }).with_density(BALL_DENSITY);
    pw.add_body(id(9), &BodyDesc::new(BodyKind::Dynamic, start).with_shape(shape).with_velocity(velocity, [0.0; 3]))
        .unwrap();
    let outer = PIT_W + 2.0 * f64::from(half_thickness);
    let top = 2.0 * f64::from(WALL_H);
    let mut fate = Fate { far: 0.0, low: f64::MAX, depth: 0.0, through: false, went: Went::Stayed };
    let mut p = start;
    for _ in 0..2 * hz {
        pw.step(1.0 / hz as f32).unwrap();
        p = pw.pose(id(9)).unwrap().position;
        let side = p[0].abs().max(p[2].abs());
        fate.far = fate.far.max(side);
        fate.low = fate.low.min(p[1] - r);
        fate.depth = fate.depth.max(side + r - PIT_W).max(r - p[1]);
        if fate.went == Went::Stayed && p[1] < -1.0 {
            fate.went = Went::Tunnelled;
        } else if fate.went == Went::Stayed && side > outer {
            fate.went = if p[1] - r > top { Went::OverTheTop } else { Went::Tunnelled };
        }
    }
    if fate.went == Went::Stayed && (p[0].abs().max(p[2].abs()) >= PIT_W || p[1] < 0.0) {
        fate.went = Went::Embedded;
    }
    fate.through = fate.went != Went::Stayed;
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
    for hz in RATES {
        for (k, aim) in aims(BALL_R, TOP_SPEED).into_iter().enumerate() {
            let Fate { far, low, through, .. } = run(hz, BALL_R, aim, WALL_T, false);
            let case = format!("{hz} Hz, aim {k} {aim:?}");
            assert!(!through, "{case}: it went through");
            assert!(far < PIT_W, "{case}: its centre reached {far:.3} m (the inner face is at {PIT_W} m)");
            assert!(low > -0.01, "{case}: it sank to {low:.3} m");
        }
    }
}

/// Balls far smaller than the pit's stay in too, never going through a wall or the floor: against the pit's walls and
/// against walls 1 cm thick, at the top speed and at 50 m/s. How deep they press in on the way, and what happens
/// faster, is the measurement's to report.
#[test]
fn small_fast_balls_stay_in() {
    for r in [0.02, 0.01, 0.005] {
        for half_thickness in [WALL_T, 0.005] {
            for (hz, speed) in RATES.into_iter().flat_map(|hz| [(hz, TOP_SPEED), (hz, 50.0)]) {
                for (k, aim) in aims(r, speed).into_iter().enumerate() {
                    let fate = run(hz, r, aim, half_thickness, false);
                    let case = format!("{hz} Hz, radius {r}, wall {half_thickness}, {speed} m/s, aim {k} {aim:?}");
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

/// Thirty balls fanned out from 0.3 m around the pit's centre, rolling at `speed` at walls 1 cm thick, stepped for two
/// seconds at `hz` with the engine's sub-steps for that rate. Each ball's radius comes from its index. It returns the
/// BLAKE3 of every frame's state hash, and how many balls end outside the pit.
fn fan(hz: u32, speed: f64, radius: impl Fn(u32) -> f64, first_id: u32) -> (blake3::Hash, usize) {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine().with_tick_rate(hz).unwrap()).unwrap();
    pit(&mut pw, 0.005);
    for i in 0..30u32 {
        let angle = f64::from(i) * std::f64::consts::TAU / 30.0;
        let (c, s, r) = (angle.cos(), angle.sin(), radius(i));
        let desc = BodyDesc::new(BodyKind::Dynamic, [0.3 * c, r, 0.3 * s])
            .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: r as f32 }))
            .with_velocity([(speed * c) as f32, 0.0, (speed * s) as f32], [0.0; 3]);
        pw.add_body(id(first_id + i), &desc).unwrap();
    }
    let mut all = blake3::Hasher::new();
    for _ in 0..2 * hz {
        pw.step(1.0 / hz as f32).unwrap();
        all.update(&pw.state_hash().unwrap().to_le_bytes());
    }
    let outside = (0..30u32)
        .filter(|&i| {
            let p = pw.pose(id(first_id + i)).unwrap().position;
            p[0].abs().max(p[2].abs()) >= PIT_W
        })
        .count();
    (all.finalize(), outside)
}

/// The same scene gives the same state hash every frame, at each rate: thirty 1 cm balls at the top speed, each
/// meeting a wall within a few frames. The engine takes a different sub-step path at each rate (8, 4 and 2 sub-steps).
/// It prints one hash per rate, to compare across machines and targets
/// (`cargo test --release -p pe-physics --test ccd -- --nocapture deterministic`; @design-reviewer's L1 on 1cda443e).
#[test]
fn continuous_collision_is_deterministic() {
    for hz in RATES {
        let (a, b) = (fan(hz, TOP_SPEED, |_| 0.01, 100), fan(hz, TOP_SPEED, |_| 0.01, 100));
        assert_eq!(a, b, "at {hz} Hz");
        println!("continuous_collision_is_deterministic at {hz} Hz: {}", a.0.to_hex());
    }
}

/// Above the tested speeds the same holds, at each rate: the same scene gives the same state hash every frame, though
/// small balls go through 1 cm walls (@coordinator-opus's two-machine check of the push-speed decision
/// and). Thirty balls of 0.5 and 1 cm roll at the walls at 100 m/s, or 200 m/s at 30 Hz, where nothing
/// goes through below 150 m/s. At least one ball ends outside the pit at each rate, so the hash covers going through.
#[test]
fn going_through_above_the_tested_speeds_is_deterministic() {
    let radius = |i: u32| if i.is_multiple_of(2) { 0.005 } else { 0.01 };
    for (hz, speed) in [(30, 200.0), (60, 100.0), (120, 100.0)] {
        let (a, b) = (fan(hz, speed, radius, 200), fan(hz, speed, radius, 200));
        assert_eq!(a, b, "at {hz} Hz");
        let (hash, outside) = (a.0.to_hex(), a.1);
        assert!(outside > 0, "at {hz} Hz no ball went through: the scene no longer covers going through");
        println!("going_through_above_the_tested_speeds_is_deterministic at {hz} Hz: {hash} ({outside} of 30 outside)");
    }
}

/// A measurement, not a check: with continuous collision on and off, for each ball, wall and speed, how many aims
/// tunnelled (which ones), went over the top or ended embedded, and the deepest any ball pressed into a wall or the
/// floor. Run it with `cargo test -p pe-physics --test ccd -- --ignored --nocapture`.
///
/// Measured with CCD on (@vdw-physics, 2026-11-13): nothing leaves the pit at 50 m/s or less, and nothing goes over
/// the top. Small balls tunnel only through 1 cm walls, from 150 m/s at 30 Hz and 75 m/s at 60 and 120 Hz; the pit
/// ball only at 120 Hz and 400 m/s. Against the pit's 20 cm walls nothing tunnels up to 400 m/s, but balls of 2 cm
/// and less can end embedded in a wall or the floor, from 200 m/s at 30 Hz and 75 m/s at 60 and 120 Hz. Higher rates
/// fail at lower speeds because Box3D's time of impact leaves a ball at the wall at full speed, the next step's new
/// contact is soft, and fewer sub-steps per step leave less time to push the ball back before the next collision
/// pass, which can pick a thin wall's far face.
#[test]
#[ignore]
fn measure_with_and_without_continuous_collision() {
    let runs = RATES.into_iter().map(|hz| (hz, false)).chain([(60, true)]);
    for (hz, ccd_off) in runs {
        for r in [BALL_R, 0.02, 0.01, 0.005] {
            for half_thickness in [WALL_T, 0.005] {
                for speed in [TOP_SPEED, 50.0, 75.0, 100.0, 150.0, 200.0, 300.0, BOX3D_TOP_SPEED] {
                    let fates: Vec<Fate> =
                        aims(r, speed).into_iter().map(|a| run(hz, r, a, half_thickness, ccd_off)).collect();
                    let tunnelled: Vec<usize> =
                        (0..fates.len()).filter(|&k| fates[k].went == Went::Tunnelled).collect();
                    let count = |went: Went| fates.iter().filter(|f| f.went == went).count();
                    let (over, embedded) = (count(Went::OverTheTop), count(Went::Embedded));
                    let depth = fates.iter().map(|f| f.depth).fold(0.0, f64::max);
                    let ccd = if ccd_off { "off" } else { "on" };
                    let case = format!("{hz} Hz, CCD {ccd}, radius {r}, wall {half_thickness}, {speed} m/s");
                    let n = tunnelled.len();
                    println!(
                        "{case}: of {}, {n} tunnelled {tunnelled:?}, {over} over the top, {embedded} embedded, deepest \
                         {depth:.4} m",
                        fates.len()
                    );
                }
            }
        }
    }
}
