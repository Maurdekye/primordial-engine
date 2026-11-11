//! Continuous collision against static geometry (@coordinator-opus's push-tunnelling ruling, madc2d14d5bf5, part (c)).
//!
//! pe-physics builds its world from Box3D's default definition, which has continuous collision on. At the end of each
//! step a fast dynamic body gets a time of impact against static shapes (the fast-body pass of Box3D's
//! `finalize_bodies`); bullets get one against dynamic bodies too. These tests hold the M1 ball pit to it:
//! - a pit ball at the push caps' top speed (world-model.md 8.1: at most 5 m/s per push, 10 m/s in all), aimed at each
//!   wall and into each corner, stays in the pit;
//! - much smaller balls at that speed stay in too, against the pit's walls and against a 1 cm wall;
//! - the same scene, run twice, gives the same state hash every frame.

use pe_physics::{BodyDesc, BodyKind, Geometry, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc};

const DT: f32 = 1.0 / 60.0;
/// The push caps' top speed (world-model.md 8.1, `max_push_speed`), in m/s.
const TOP_SPEED: f32 = 10.0;
/// The M1 ball pit, as pe-world's preset builds it: the inner half width, each wall's half thickness and half height
/// (the floor's top is y = 0), and its balls' radius and density (18 g).
const PIT_W: f64 = 1.2;
const WALL_T: f32 = 0.1;
const WALL_H: f32 = 0.6;
const BALL_R: f32 = 0.06;
const BALL_DENSITY: f32 = 20.0;
/// Every horizontal direction a push can aim at a wall: each wall, then each corner.
const AIMS: [[f32; 2]; 8] =
    [[1.0, 0.0], [-1.0, 0.0], [0.0, 1.0], [0.0, -1.0], [1.0, 1.0], [-1.0, 1.0], [1.0, -1.0], [-1.0, -1.0]];

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((2u128 << 32) | u128::from(n))
}

fn static_box(at: [f64; 3], half: [f32; 3]) -> BodyDesc {
    BodyDesc::new(BodyKind::Static, at).with_shape(ShapeDesc::new(Geometry::Box { half_extents: half }))
}

/// The pit: its floor and four walls, as the preset places them. A wall `thickness` other than the preset's moves
/// only the walls' outer faces.
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

/// A ball of radius `r` resting on the floor at `(x, z)`, moving at the top speed along `aim`.
fn ball(r: f32, x: f64, z: f64, aim: [f32; 2]) -> BodyDesc {
    let n = (aim[0] * aim[0] + aim[1] * aim[1]).sqrt();
    let v = [TOP_SPEED * aim[0] / n, 0.0, TOP_SPEED * aim[1] / n];
    BodyDesc::new(BodyKind::Dynamic, [x, f64::from(r), z])
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: r }).with_density(BALL_DENSITY))
        .with_velocity(v, [0.0; 3])
}

/// One ball of radius `r` from the pit's centre along `aim`, stepped for two seconds, with continuous collision on or
/// off: how far its centre got from the pit's centre along x or z, and the lowest its bottom got.
fn run(r: f32, aim: [f32; 2], half_thickness: f32, continuous: bool) -> (f64, f64) {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    pw.raw_mut().enable_continuous(continuous);
    pit(&mut pw, half_thickness);
    pw.add_body(id(9), &ball(r, 0.0, 0.0, aim)).unwrap();
    let (mut far, mut low) = (0.0f64, f64::MAX);
    for _ in 0..120 {
        pw.step(DT).unwrap();
        let p = pw.pose(id(9)).unwrap().position;
        far = far.max(p[0].abs()).max(p[2].abs());
        low = low.min(p[1] - f64::from(r));
    }
    (far, low)
}

/// The ruling's test: a pit ball at the push caps' top speed, aimed at each wall and into each corner, stays in the
/// pit and on its floor. Its centre never passes a wall's inner face.
#[test]
fn a_pit_ball_at_the_push_cap_stays_in_the_pit() {
    for aim in AIMS {
        let (far, low) = run(BALL_R, aim, WALL_T, true);
        assert!(far < PIT_W, "aimed {aim:?}: its centre reached {far:.3} m (the inner face is at {PIT_W} m)");
        assert!(low > -0.01, "aimed {aim:?}: it sank to {low:.3} m");
    }
}

/// Balls far smaller than the pit's, at the same speed, stay in too: against the pit's walls, and against walls 1 cm
/// thick, which they cross in under a sixth of a frame's travel.
#[test]
fn small_fast_balls_stay_in_with_continuous_collision() {
    for r in [0.02, 0.01, 0.005] {
        for half_thickness in [WALL_T, 0.005] {
            for aim in AIMS {
                let (far, low) = run(r, aim, half_thickness, true);
                assert!(far < PIT_W, "radius {r}, wall {half_thickness}, aimed {aim:?}: reached {far:.3} m");
                assert!(low > -0.01, "radius {r}, wall {half_thickness}, aimed {aim:?}: sank to {low:.3} m");
            }
        }
    }
}

/// With continuous collision on, the same scene gives the same state hash every frame: thirty small balls at the top
/// speed, fanned out at the walls and corners, every one of them meeting a wall within the first few frames.
#[test]
fn continuous_collision_is_deterministic() {
    let hashes = || {
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        pit(&mut pw, 0.005);
        for i in 0..30u32 {
            let angle = f64::from(i) * std::f64::consts::TAU / 30.0;
            let (x, z) = (0.3 * angle.cos(), 0.3 * angle.sin());
            let aim = [angle.cos() as f32, angle.sin() as f32];
            pw.add_body(id(100 + i), &ball(0.01, x, z, aim)).unwrap();
        }
        (0..120)
            .map(|_| {
                pw.step(DT).unwrap();
                pw.state_hash().unwrap()
            })
            .collect::<Vec<u64>>()
    };
    assert_eq!(hashes(), hashes());
}

/// A measurement, not a check: how far the balls of the tests above get with continuous collision off. Run it with
/// `cargo test -p pe-physics --test ccd -- --ignored --nocapture`.
#[test]
#[ignore]
fn measure_without_continuous_collision() {
    for r in [BALL_R, 0.02, 0.01, 0.005] {
        for half_thickness in [WALL_T, 0.005] {
            let escaped: Vec<f64> =
                AIMS.iter().map(|&aim| run(r, aim, half_thickness, false).0).filter(|&far| far >= PIT_W).collect();
            println!(
                "radius {r}, wall half thickness {half_thickness}: {} of 8 left the pit {escaped:?}",
                escaped.len()
            );
        }
    }
}
