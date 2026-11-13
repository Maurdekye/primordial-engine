//! Holds: a grab's spring sees a bounded error for a step ([`PhysicsWorld::step_holding`], world-model.md 8.1's hold
//! bound), its anchor comes back after the step, the bound keeps its support against gravity, and
//! [`PhysicsWorld::static_in_way`] finds static geometry in the way of a held object's move (a trailing placement's
//! wall check).

use pe_physics::{
    BodyDesc, BodyKind, Geometry, GrabSpring, HoldReach, JointDesc, JointKey, ObjectId, PhysicsConfig, PhysicsError,
    PhysicsWorld, Pose, ShapeDesc,
};

const DT: f32 = 1.0 / 60.0;
/// pe-replica's grab spring (hands::GRAB): 5 Hz, critically damped, 1,000 N, no angular spring.
const SPRING: GrabSpring = GrabSpring { hertz: 5.0, damping_ratio: 1.0, max_force: 1000.0, max_torque: 0.0 };
const KEY: JointKey = JointKey(7);
const ID_ROT: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((1u128 << 32) | n as u128)
}

fn weightless() -> PhysicsConfig {
    PhysicsConfig { gravity: [0.0; 3], ..PhysicsConfig::engine() }
}

/// A pit ball: 3 cm, 160 kg/m3 (18 g).
fn pit_ball(at: [f64; 3]) -> BodyDesc {
    BodyDesc::new(BodyKind::Dynamic, at)
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.03 }).with_density(160.0))
}

fn static_box(at: [f64; 3], half_extents: [f32; 3]) -> BodyDesc {
    BodyDesc::new(BodyKind::Static, at).with_shape(ShapeDesc::new(Geometry::Box { half_extents }))
}

/// A world with a kinematic hand (id 1) at `hand` holding a pit ball (id 2) at `ball`, by a grab whose place for the
/// ball is the hand's centre plus `place`.
fn holding(config: PhysicsConfig, hand: [f64; 3], ball: [f64; 3], place: [f32; 3]) -> PhysicsWorld {
    let mut pw = PhysicsWorld::new(config).unwrap();
    let body = BodyDesc::new(BodyKind::Kinematic, hand)
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.1 }));
    pw.add_body(id(1), &body).unwrap();
    pw.add_body(id(2), &pit_ball(ball)).unwrap();
    let grab = JointDesc::Grab {
        a: Some(id(1)),
        b: id(2),
        anchor_a: place,
        anchor_b: [0.0; 3],
        max_force: 0.0,
        max_torque: 100.0,
        spring: Some(SPRING),
    };
    pw.add_joint(KEY, &grab).unwrap();
    pw
}

fn speed(pw: &PhysicsWorld, n: u32) -> f64 {
    let (v, _) = pw.velocity(id(n)).unwrap();
    f64::from(v.iter().map(|c| c * c).sum::<f32>().sqrt())
}

fn bits(v: [f32; 3]) -> [u32; 3] {
    v.map(f32::to_bits)
}

/// A ball 2 m from its place, its spring bounded at 0.5 m, is pulled as one 0.5 m from its place is: the same
/// velocity after a step (the hand is kinematic, so where its anchor sits does not enter the solve). Coming back it is
/// never faster than the spring's rate times the bound (it keeps speeding up while the bound holds, so it beats the
/// ball released from 0.5 m, whose error shrinks at once), where unbounded it comes back more than three times as
/// fast. After every step the grab's anchor is the one it was made with, to the bit.
#[test]
fn a_bounded_hold_pulls_as_if_its_object_were_at_the_bound() {
    let reach = HoldReach { reach: 0.5, against_gravity: 0.5 };
    let mut far = holding(weightless(), [0.0; 3], [-2.0, 0.0, 0.0], [0.0; 3]);
    let mut near = holding(weightless(), [0.0; 3], [-0.5, 0.0, 0.0], [0.0; 3]);
    let mut free = holding(weightless(), [0.0; 3], [-2.0, 0.0, 0.0], [0.0; 3]);
    let home = far.grab_anchor(KEY).unwrap();
    far.step_holding(DT, &[(KEY, reach)]).unwrap();
    near.step(DT).unwrap();
    let (vf, vn) = (far.velocity(id(2)).unwrap().0, near.velocity(id(2)).unwrap().0);
    assert!((vf[0] - vn[0]).abs() <= 1e-5 * vn[0].abs() && vf[1] == 0.0 && vf[2] == 0.0, "{vf:?} vs {vn:?}");
    assert_eq!(bits(far.grab_anchor(KEY).unwrap()), bits(home), "the anchor is put back");
    let (mut top_far, mut top_near, mut top_free) = (speed(&far, 2), speed(&near, 2), 0.0_f64);
    for _ in 0..90 {
        far.step_holding(DT, &[(KEY, reach)]).unwrap();
        near.step(DT).unwrap();
        free.step(DT).unwrap();
        assert_eq!(bits(far.grab_anchor(KEY).unwrap()), bits(home));
        top_far = top_far.max(speed(&far, 2));
        top_near = top_near.max(speed(&near, 2));
        top_free = top_free.max(speed(&free, 2));
    }
    println!(
        "RESULT bounded hold: top speed from 2 m bounded at 0.5 m {top_far:.2}, from 0.5 m {top_near:.2}, from 2 m unbounded {top_free:.2} m/s"
    );
    let rate = f64::from(far.grab_spring_rate(SPRING, DT));
    assert!(top_far > top_near && top_far < rate * 0.5, "no faster than {} m/s: {top_far}", rate * 0.5);
    assert!(top_free > 3.0 * top_far, "unbounded it comes back much faster: {top_free}");
    assert!(far.pose(id(2)).unwrap().position[0].abs() < 0.01, "and it gets back to its place");
}

/// The part of the error against gravity has its own bound, so a hold whose reach is zero (its hand at the cap) still
/// holds its object up: a ball hanging at its place stays within 2 cm of it for a second (its static sag, 9.9 mm),
/// where with no support only the spring's damping slows its fall (it sinks more than 10 cm).
#[test]
fn a_hold_keeps_its_support_against_gravity() {
    let config = PhysicsConfig { gravity: [0.0, -9.81, 0.0], ..PhysicsConfig::engine() };
    let mut held = holding(config, [0.0, 2.0, 0.0], [0.0, 1.5, 0.0], [0.0, -0.5, 0.0]);
    let mut dropped = holding(config, [0.0, 2.0, 0.0], [0.0, 1.5, 0.0], [0.0, -0.5, 0.0]);
    let (support, none) =
        (HoldReach { reach: 0.0, against_gravity: 0.03 }, HoldReach { reach: 0.0, against_gravity: 0.0 });
    let mut sag = 0.0_f64;
    for _ in 0..60 {
        held.step_holding(DT, &[(KEY, support)]).unwrap();
        dropped.step_holding(DT, &[(KEY, none)]).unwrap();
        sag = sag.max(1.5 - held.pose(id(2)).unwrap().position[1]);
    }
    println!(
        "RESULT support: sag {sag:.4} m held, {:.3} m without support",
        1.5 - dropped.pose(id(2)).unwrap().position[1]
    );
    assert!(sag < 0.02, "held up: {sag}");
    assert!(1.5 - dropped.pose(id(2)).unwrap().position[1] > 0.1, "sinks without support");
}

/// Every refusal changes nothing: nothing steps, and the anchor stays.
#[test]
fn step_holding_refusals_change_nothing() {
    let mut pw = holding(weightless(), [0.0; 3], [-2.0, 0.0, 0.0], [0.0; 3]);
    pw.add_body(id(3), &pit_ball([1.0, 0.0, 0.0])).unwrap();
    let weld = JointDesc::Weld { a: id(3), b: None, anchor_a: [0.0; 3], anchor_b: [1.0, 0.0, 0.0] };
    pw.add_joint(JointKey(8), &weld).unwrap();
    let fine = HoldReach { reach: 0.5, against_gravity: 0.5 };
    let before = (pw.state_hash().unwrap(), bits(pw.grab_anchor(KEY).unwrap()));
    let refusals = [
        pw.step_holding(DT, &[(KEY, fine), (KEY, fine)]),
        pw.step_holding(DT, &[(KEY, HoldReach { reach: f32::NAN, against_gravity: 0.5 })]),
        pw.step_holding(DT, &[(KEY, HoldReach { reach: 0.5, against_gravity: -0.1 })]),
        pw.step_holding(DT, &[(KEY, HoldReach { reach: 1.0e9, against_gravity: 0.5 })]),
        pw.step_holding(DT, &[(KEY, fine), (JointKey(99), fine)]),
        pw.step_holding(DT, &[(KEY, fine), (JointKey(8), fine)]),
        pw.step_holding(0.0, &[(KEY, fine)]),
    ];
    assert!(matches!(refusals[0], Err(PhysicsError::InvalidValue(_))));
    assert!(matches!(refusals[1], Err(PhysicsError::InvalidValue(_))));
    assert!(matches!(refusals[2], Err(PhysicsError::InvalidValue(_))));
    assert!(matches!(refusals[3], Err(PhysicsError::InvalidValue(_))));
    assert_eq!(refusals[4], Err(PhysicsError::UnknownJoint(99)));
    assert_eq!(refusals[5], Err(PhysicsError::NotAGrab(8)));
    assert!(matches!(refusals[6], Err(PhysicsError::InvalidValue(_))));
    assert_eq!((pw.state_hash().unwrap(), bits(pw.grab_anchor(KEY).unwrap())), before);
    assert_eq!(pw.grab_anchor(JointKey(8)), Err(PhysicsError::NotAGrab(8)));
    assert_eq!(pw.grab_anchor(JointKey(99)), Err(PhysicsError::UnknownJoint(99)));
}

/// The spring's speed per metre of error is Box3D's soft-constraint bias rate, `omega / (2 zeta + h omega)`, at the
/// configured sub-step.
#[test]
fn the_grab_spring_rate_is_box3d_s_bias_rate() {
    let pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    let h = DT / pw.config().sub_steps as f32;
    let omega = 2.0 * std::f32::consts::PI * SPRING.hertz;
    let expected = omega / (2.0 * SPRING.damping_ratio + h * omega);
    assert_eq!(pw.grab_spring_rate(SPRING, DT).to_bits(), expected.to_bits());
    println!("RESULT grab spring rate at 60 Hz: {expected:.3} /s");
}

fn to(position: [f64; 3]) -> Pose {
    Pose { position, rotation: ID_ROT }
}

/// Static geometry in the way of a held object's move, by Box3D's mover cast against static shapes only: a wall it
/// would cross is in the way; the floor it rests on is not while it slides along it, but is when it moves into it; a
/// wall it touches is not when it moves away; a wall's end its centre passes 1 cm beside is (its volume overlaps it,
/// though a ray from its centre would miss); a dynamic body is never in the way; a box is swept as the capsule inside
/// it. The query changes nothing.
#[test]
fn static_geometry_in_the_way_of_a_move() {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    pw.add_body(id(1), &static_box([0.0, -1.0, 0.0], [20.0, 1.0, 20.0])).unwrap();
    pw.add_body(id(2), &static_box([0.30, 0.5, 0.0], [0.05, 0.5, 1.0])).unwrap();
    pw.add_body(id(3), &pit_ball([-1.0, 0.03, 2.0])).unwrap();
    pw.add_body(id(4), &pit_ball([0.22, 0.5, 0.0])).unwrap();
    pw.add_body(id(5), &pit_ball([0.0, 0.5, 1.01])).unwrap();
    pw.add_body(id(9), &pit_ball([0.0, 0.5, 1.05])).unwrap();
    let cube = BodyDesc::new(BodyKind::Dynamic, [-1.0, 0.5, -0.5])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [0.1, 0.1, 0.1] }));
    pw.add_body(id(6), &cube).unwrap();
    pw.add_body(id(7), &pit_ball([-1.0, 0.5, 3.0])).unwrap();
    pw.add_body(
        id(8),
        &BodyDesc::new(BodyKind::Dynamic, [-0.5, 0.5, 3.0])
            .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [0.1, 0.4, 0.4] })),
    )
    .unwrap();
    let before = pw.state_hash().unwrap();
    let in_way = |pw: &PhysicsWorld, n: u32, p: [f64; 3]| pw.static_in_way(id(n), &to(p)).unwrap();
    assert!(!in_way(&pw, 3, [1.0, 0.03, 2.0]), "sliding along the floor it rests on");
    assert!(in_way(&pw, 3, [-1.0, -0.5, 2.0]), "into the floor");
    assert!(in_way(&pw, 4, [0.8, 0.5, 0.0]), "through the wall it touches");
    assert!(!in_way(&pw, 4, [-0.5, 0.5, 0.0]), "away from the wall it touches");
    assert!(in_way(&pw, 5, [0.8, 0.5, 1.01]), "past the wall's end, its volume overlapping it by 2 cm");
    assert!(!in_way(&pw, 9, [0.8, 0.5, 1.05]), "past the wall's end, clear of it by 2 cm");
    assert!(!in_way(&pw, 7, [0.0, 0.5, 3.0]), "a dynamic box is not in the way");
    assert!(in_way(&pw, 6, [1.0, 0.5, -0.5]), "a box swept through the wall");
    assert!(!in_way(&pw, 6, [-1.0, 0.5, -2.5]), "a box swept along it");
    assert_eq!(pw.state_hash().unwrap(), before, "the query changes nothing");
    let skew = Pose { position: [0.0, 0.5, 0.0], rotation: [0.0, 0.0, 0.0, 2.0] };
    assert!(matches!(pw.static_in_way(id(3), &skew), Err(PhysicsError::InvalidValue(_))));
    assert_eq!(pw.static_in_way(id(99), &to([0.0; 3])), Err(PhysicsError::NotBound(id(99))));
}
