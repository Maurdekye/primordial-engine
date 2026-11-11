//! The tick rate's sub-step rule (docs/spec/replica.md 3.5 and 13; the user's ruling of 2026-11-10, mc8dc9e5a6ffb):
//! a world ticks 30 to 120 times a second, in whole hertz, with ceil(240 / rate) Box3D sub-steps.

mod common;

use common::*;
use pe_physics::{
    PhysicsConfig, PhysicsError, PhysicsWorld, SUBSTEPS, TICK_HZ_DEFAULT, TICK_HZ_MAX, TICK_HZ_MIN, sub_steps_for_rate,
};

#[test]
fn sub_steps_follow_the_rate() {
    let table = [(30, 8), (31, 8), (45, 6), (48, 5), (59, 5), (60, 4), (72, 4), (80, 3), (90, 3), (119, 3), (120, 2)];
    for (hz, n) in table {
        assert_eq!(sub_steps_for_rate(hz), n, "{hz} Hz");
    }
    // The fewest sub-steps that keep the sub-step rate at 240 Hz or more.
    for hz in TICK_HZ_MIN..=TICK_HZ_MAX {
        let n = sub_steps_for_rate(hz) as u32;
        assert!(hz * n >= 240, "{hz} Hz: {n} sub-steps are too few");
        assert!(n == 1 || hz * (n - 1) < 240, "{hz} Hz: {n} sub-steps are more than needed");
    }
    // 60 Hz keeps the four sub-steps every committed record was made with.
    assert_eq!(sub_steps_for_rate(TICK_HZ_DEFAULT), SUBSTEPS);
    assert_eq!(PhysicsConfig::engine().sub_steps, SUBSTEPS);
    assert_eq!(sub_steps_for_rate(0), 1);
    assert_eq!(sub_steps_for_rate(240), 1);
    assert_eq!(sub_steps_for_rate(1000), 1);
}

#[test]
fn the_rate_and_the_sub_steps_are_bounded() {
    let c = PhysicsConfig::engine();
    for hz in [0, 29, 121, 240] {
        assert!(
            matches!(c.with_tick_rate(hz), Err(PhysicsError::InvalidValue("tick rate"))),
            "{hz} Hz is outside the range"
        );
    }
    for hz in [30, 60, 90, 120] {
        assert_eq!(c.with_tick_rate(hz).unwrap().sub_steps, sub_steps_for_rate(hz));
    }
    assert_eq!(c.with_tick_rate(60).unwrap(), c);
    for n in [0, -1, 17] {
        assert!(PhysicsWorld::new(PhysicsConfig { sub_steps: n, ..c }).is_err(), "{n} sub-steps");
    }
    assert!(PhysicsWorld::new(PhysicsConfig { sub_steps: 16, ..c }).is_ok());
}

/// The setting reaches the step: one pile stepped at 30 Hz with the rule's 8 sub-steps and with the old fixed 4 ends in
/// different states, and two worlds with one configuration stay equal.
#[test]
fn the_sub_steps_reach_the_step() {
    let run = |config: PhysicsConfig| {
        let (mut pw, _) = pile_world(config, 4, 2);
        for _ in 0..30 {
            pw.step(1.0 / 30.0).unwrap();
        }
        pw.box3d_hash().unwrap()
    };
    let rule = PhysicsConfig::engine().with_tick_rate(30).unwrap();
    assert_eq!(rule.sub_steps, 8);
    assert_eq!(run(rule), run(rule));
    assert_ne!(run(rule), run(PhysicsConfig::engine()));
}
