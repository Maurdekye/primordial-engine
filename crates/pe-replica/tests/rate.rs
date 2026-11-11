//! One tick-rate range for the two crates that state it (the user's ruling mc8dc9e5a6ffb): pe-physics's
//! `with_tick_rate` refuses rates outside `TICK_HZ_MIN..=TICK_HZ_MAX`, and pe-seams's `SimRate` holds the world's rate.

use pe_physics::PhysicsConfig;
use pe_seams::SimRate;

#[test]
fn physics_and_the_seams_agree_on_the_rate_range() {
    assert_eq!(pe_physics::TICK_HZ_MIN, SimRate::MIN_HZ);
    assert_eq!(pe_physics::TICK_HZ_MAX, SimRate::MAX_HZ);
    assert_eq!(pe_physics::TICK_HZ_DEFAULT, SimRate::DEFAULT.hz());
    // Every rate the seams accept configures the physics, and no other rate does.
    for hz in 0..=SimRate::MAX_HZ + 1 {
        assert_eq!(PhysicsConfig::engine().with_tick_rate(hz).is_ok(), SimRate::new(hz).is_some(), "{hz} Hz");
    }
}
