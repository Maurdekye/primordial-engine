//! Restitution tests from test_restitution.c (NormalVelocityTest, OvershootTest,
//! ImpulseTest, WorkerParityTest). Split from `restitution_tests.rs` to satisfy the file-length
//! limit.
//!
//! SPDX-FileCopyrightText: 2026 Erin Catto
//! SPDX-License-Identifier: MIT

use crate::body::{
    body_flags, body_get_angular_velocity, body_get_contact_data, body_get_gravity_scale,
    body_get_linear_velocity, body_get_mass, body_get_mass_data, body_get_min_extent,
    body_get_position, body_get_safety_factor, body_get_transform, body_get_world_center,
    body_set_linear_velocity, create_body, get_body_full_id, get_body_sim,
};
use crate::geometry::Sphere;
use crate::hull::{make_box_hull, make_cube_hull};
use crate::id::BodyId;
use crate::math_functions::{
    abs_float, dot, inv_rotate_vector, max_float, mul_mv, mul_sv, to_vec3, Vec3, VEC3_ZERO,
};
use crate::recording::hash_world_state;
use crate::restitution_tests::{
    make_ball, make_ground, make_world, make_world_iterations, pos, IMPACT_SPEED,
    RESTITUTION_ITERATIONS, SUB_STEP_COUNT, TIME_STEP,
};
use crate::shape::{create_hull_shape, create_sphere_shape};
use crate::types::{default_body_def, default_shape_def, default_world_def, BodyType};
use crate::world::{world_get_gravity, World};

/// (RunWorkerScene)
///
/// C sets `worldDef.workerCount`; the serial port has no worker count, so every call runs the
/// same serial schedule.
fn run_worker_scene() -> u64 {
    let mut world_def = default_world_def();
    world_def.gravity = Vec3::new(0.0, -10.0, 0.0);
    world_def.enable_sleep = false;
    let mut world = World::new(&world_def);

    make_ground(&mut world, 0.6);

    for i in 0..20 {
        make_ball(
            &mut world,
            -10.0 + 1.05 * i as f32,
            3.0 + 0.13 * i as f32,
            0.0,
            0.6,
        );
    }

    for _ in 0..200 {
        world.step(TIME_STEP, SUB_STEP_COUNT);
    }

    hash_world_state(&world)
}

/// The manifold point approach speed is published only for contacts that opted into hit events.
/// It is not a stale value otherwise, it is zero, so a reader can tell "not measured" from
/// "measured zero" by whether the shape enables the events. Restitution no longer reads it at
/// all, so this is the only thing keeping the field alive. (MeasureFirstTouchNormalVelocity)
fn measure_first_touch_normal_velocity(enable_hit_events: bool) -> f32 {
    let mut world = make_world(0.0);
    make_ground(&mut world, 0.0);

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Dynamic;
    body_def.position = pos(0.0, 0.6, 0.0);
    let ball_id = create_body(&mut world, &body_def);

    let mut shape_def = default_shape_def();
    shape_def.base_material.friction = 0.0;
    shape_def.base_material.restitution = 0.0;
    shape_def.enable_hit_events = enable_hit_events;
    let sphere = Sphere {
        center: VEC3_ZERO,
        radius: 0.5,
    };
    create_sphere_shape(&mut world, ball_id, &shape_def, &sphere);

    let mut normal_velocity = f32::MAX;

    // Hold the closing speed so the sampled value does not depend on which step the pair is created
    for _ in 0..20 {
        body_set_linear_velocity(&mut world, ball_id, Vec3::new(0.0, -IMPACT_SPEED, 0.0));
        world.step(TIME_STEP, SUB_STEP_COUNT);

        let contact_data = body_get_contact_data(&world, ball_id, 1);
        if contact_data.len() == 1
            && !contact_data[0].manifolds.is_empty()
            && contact_data[0].manifolds[0].point_count > 0
        {
            normal_velocity = contact_data[0].manifolds[0].points[0].normal_velocity;
            break;
        }
    }

    normal_velocity
}

/// (NormalVelocityTest)
#[test]
fn normal_velocity_test() {
    let quiet = measure_first_touch_normal_velocity(false);
    let published = measure_first_touch_normal_velocity(true);

    println!(
        "    normal velocity hit events off {quiet:.4} on {published:.4} (closing at {:.4})",
        -IMPACT_SPEED
    );

    let mut failed = false;

    if quiet != 0.0 {
        failed = true;
    }

    if published > -0.8 * IMPACT_SPEED {
        failed = true;
    }

    assert!(!failed);
}

/// Mechanical energy of a body as symplectic Euler conserves it. The integrator updates velocity
/// before position, so the quantity that is exactly constant in free flight is not kinetic plus
/// potential but that plus half a sub-step of the gravity power, m * dot(v, g) * h / 2. Plain
/// kinetic plus potential sampled at step boundaries jumps by about g * v * h at every bounce and
/// drifts down by g^2 h^2 / 2 each sub-step, which would hide a real gain or fake one. With this
/// measure a contact that never does positive work leaves the energy flat to round-off.
/// (MeasureEnergy)
pub(crate) fn measure_energy(world: &World, body_id: BodyId) -> f32 {
    let mass_data = body_get_mass_data(world, body_id);
    let v = body_get_linear_velocity(world, body_id);
    let gravity = mul_sv(
        body_get_gravity_scale(world, body_id),
        world_get_gravity(world),
    );
    let h = TIME_STEP / SUB_STEP_COUNT as f32;

    // The inertia tensor is in the body frame, so bring the angular velocity back to it
    let w_local = inv_rotate_vector(
        body_get_transform(world, body_id).q,
        body_get_angular_velocity(world, body_id),
    );

    let kinetic =
        0.5 * mass_data.mass * dot(v, v) + 0.5 * dot(w_local, mul_mv(mass_data.inertia, w_local));
    let potential = -mass_data.mass * dot(gravity, to_vec3(body_get_world_center(world, body_id)));
    let phase = 0.5 * h * mass_data.mass * dot(v, gravity);

    kinetic + potential + phase
}

/// (OvershootResult)
#[derive(Debug, Clone, Copy, Default)]
struct OvershootResult {
    first_apex: f32,
    first_energy_ratio: f32,
    peak_energy_ratio: f32,
    speed_ratio: f32,
    bounce_count: i32,
}

/// Mirrors the Restitution Overshoot sample in samples/sample_issues.cpp: a unit cube dropped
/// flat on a floor narrower than itself, perfectly elastic, so the four contact points sit
/// inboard of the box corners. A perfectly elastic bounce cannot come back higher than it was
/// dropped from and cannot gain energy. (MeasureOvershoot)
fn measure_overshoot(continuous: bool) -> OvershootResult {
    let mut world = make_world_iterations(-10.0, RESTITUTION_ITERATIONS);

    let mut floor_def = default_body_def();
    floor_def.position = pos(0.0, -0.25, 0.0);
    let floor_id = create_body(&mut world, &floor_def);

    let floor_shape = default_shape_def();
    let floor = make_box_hull(0.375, 0.25, 0.375);
    create_hull_shape(&mut world, floor_id, &floor_shape, &floor.base);

    let drop_height = 10.0f32;

    let mut box_def = default_body_def();
    box_def.type_ = BodyType::Dynamic;
    box_def.position = pos(0.0, drop_height, 0.0);
    if continuous {
        box_def.safety_factor = 0.1;
    }
    let box_id = create_body(&mut world, &box_def);

    let mut box_shape = default_shape_def();
    box_shape.base_material.restitution = 1.0;
    let box_hull = make_cube_hull(0.5);
    create_hull_shape(&mut world, box_id, &box_shape, &box_hull.base);

    let mut result = OvershootResult::default();

    let start_energy = measure_energy(&world, box_id);
    let mut impact_speed = 0.0f32;
    let mut rebound_speed = 0.0f32;
    let mut bounce_height = 0.0f32;
    let mut previous_speed = 0.0f32;

    let mut i = 0;
    while i < 600 && result.bounce_count < 2 {
        i += 1;

        let before = body_get_linear_velocity(&world, box_id).y;
        let height_before = body_get_position(&world, box_id).y as f32;
        world.step(TIME_STEP, SUB_STEP_COUNT);
        let speed = body_get_linear_velocity(&world, box_id).y;

        if rebound_speed == 0.0 && before < 0.0 && speed > 0.0 {
            impact_speed = -before;
            rebound_speed = speed;
            bounce_height = height_before;
        }

        if rebound_speed == 0.0 {
            continue;
        }

        // Every step, not just at the apexes, because that is what the sample checks and the
        // impact step itself is where any gain appears
        let ratio = measure_energy(&world, box_id) / start_energy;
        if ratio > result.peak_energy_ratio {
            result.peak_energy_ratio = ratio;
        }

        if previous_speed > 0.0 && speed <= 0.0 {
            if result.bounce_count == 0 {
                result.first_apex = body_get_position(&world, box_id).y as f32;
                result.first_energy_ratio = ratio;
            }

            result.bounce_count += 1;
        }

        previous_speed = speed;
    }

    result.speed_ratio = rebound_speed / impact_speed;

    println!(
        "    overshoot continuous {} apex {:.4} of {:.4} at y {:.4}, speed ratio {:.4}, energy {:.4} then {:.4}",
        if continuous { 1 } else { 0 },
        result.first_apex,
        drop_height,
        bounce_height,
        result.speed_ratio,
        result.first_energy_ratio,
        result.peak_energy_ratio
    );

    result
}

/// Two runs, because two unrelated effects were tangled together here.
///
/// The impulse must never return more speed than it received. That is the restitution invariant
/// and it is checked in both runs; the unconverged four point solve failed it by 7.9 percent.
///
/// Everything else depends on whether continuous collision engages. Without it the cube moves
/// 0.23 m in the step before contact, just under the 0.25 m trigger, so the first manifold
/// appears with the cube already 0.10 m deep. It bounces from down there and climbs back through
/// that depth for free, which is worth about a tenth of a metre of apex and one percent of
/// energy, and none of it is restitution. With continuous collision the cube lands on the surface
/// and both the apex and the energy come in under where they started. The safety factor here
/// matches the one the sample sets, which is the supported answer for a body whose elastic
/// accuracy matters.
///
/// The discrete run is only gated on its first bounce. Later bounces there are a genuine blow-up,
/// not a tolerance question: the free height makes the cube tilt, the next landing is a deep
/// corner impact at 14 m/s, and energy reaches 159 percent with the cube spinning at 26 rad/s.
/// Lowering the safety factor is the fix and the sample does that; the discrete run is kept to
/// pin what happens when it is left alone, and because its speed ratio is what caught the real
/// bug. (OvershootTest)
#[test]
fn overshoot_test() {
    let drop_height = 10.0f32;

    let discrete = measure_overshoot(false);
    let continuous = measure_overshoot(true);

    // Perfectly elastic returns exactly the speed it received, so any excess is round-off. The
    // overflow and colored solvers round differently and either can land an ulp over one.
    let speed_tolerance = 1.0e-5;

    let mut failed = false;

    if discrete.speed_ratio > 1.0 + speed_tolerance
        || continuous.speed_ratio > 1.0 + speed_tolerance
    {
        failed = true;
    }

    if continuous.first_apex > drop_height || continuous.peak_energy_ratio > 1.001 {
        failed = true;
    }

    if discrete.first_apex > drop_height + 0.15 || discrete.first_energy_ratio > 1.02 {
        failed = true;
    }

    // The continuous landing leaves the cube sliding sideways a few tenths of a metre per second,
    // from friction during the sub-steps, and a floor narrower than the cube does not catch it on
    // the way back down. That is sub-step friction, not restitution, so only the first bounce is
    // required.
    if discrete.bounce_count < 2 || continuous.bounce_count < 1 {
        failed = true;
    }

    assert!(!failed);
}

/// (ImpulseResult)
#[derive(Debug, Clone, Copy, Default)]
struct ImpulseResult {
    worst_error: f32,
    approach_speed: f32,
    first_separation: f32,
    bounce_impulse: f32,
    contact_steps: i32,
    toi_steps: i32,
    toi_impulse_steps: i32,
}

/// Ball dropped under gravity with the contact impulse read back every step. Nothing else touches
/// the ball, so once gravity is taken out the change in momentum over a step is the impulse the
/// contact applied, and that is what the total normal impulse must report. A time of impact step
/// is left out of the balance: no contact was solved on it, and the sweep hands back the gravity
/// of the time it cut short. (MeasureDropImpulse)
fn measure_drop_impulse(restitution: f32, drop_height: f32) -> ImpulseResult {
    let mut world = make_world(-10.0);
    make_ground(&mut world, 0.0);

    let ball_id = make_ball(&mut world, 0.0, 0.5 + drop_height, 0.0, restitution);

    let mass = body_get_mass(&world, ball_id);
    let gravity_y = world_get_gravity(&world).y;

    let mut result = ImpulseResult::default();
    let mut touched = false;
    let mut bouncing = false;

    // Enough for the longest fall and the bounce that follows it
    for _ in 0..240 {
        let speed_before = body_get_linear_velocity(&world, ball_id).y;
        world.step(TIME_STEP, SUB_STEP_COUNT);
        let speed_after = body_get_linear_velocity(&world, ball_id).y;

        let mut measured = 0.0f32;
        let mut separation = 0.0f32;
        let contact_data = body_get_contact_data(&world, ball_id, 4);
        let contact_count = contact_data.len();
        for data in &contact_data {
            for manifold in &data.manifolds {
                for p in 0..manifold.point_count as usize {
                    measured += manifold.points[p].total_normal_impulse;
                    separation = manifold.points[p].separation;
                }
            }
        }

        // Transient body flags are cleared and rewritten every step
        let ball_index = get_body_full_id(&world, ball_id);
        let ball_sim = get_body_sim(&world, ball_index);
        if (ball_sim.flags & body_flags::HAD_TIME_OF_IMPACT) != 0 {
            result.toi_steps += 1;
            if measured != 0.0 {
                result.toi_impulse_steps += 1;
            }
            continue;
        }

        let expected = mass * (speed_after - speed_before) - mass * gravity_y * TIME_STEP;
        result.worst_error = max_float(result.worst_error, abs_float(measured - expected));

        if contact_count > 0 {
            if !touched {
                touched = true;
                bouncing = true;
                result.approach_speed = -speed_before;
                result.first_separation = separation;
            }

            if bouncing {
                result.bounce_impulse += measured;
                result.contact_steps += 1;
            }
        } else {
            bouncing = false;
        }
    }

    result
}

/// Heights on both sides of the continuous collision threshold, so the impulse is checked for a
/// ball that lands inside the overlap and for one the sweep sets down on the surface. The
/// threshold is derived from the body so the split survives a change to the default safety
/// factor.
///
/// The step balance is the real gate. The bounce total is bracketed as well so the restitution
/// sweep means something: the contact has to reverse the approach at the coefficient and may
/// carry the weight for at most as long as it lasted. The stage runs after the sub-steps, so the
/// reversal lands a step late and the ball can come in up to a step of gravity under it.
/// (ImpulseTest)
#[test]
fn impulse_test() {
    let restitutions = [0.0f32, 0.25, 0.5, 0.75, 1.0];
    let heights = [1.0f32, 5.0, 20.0, 45.0];

    let mass;
    let fast_speed;
    {
        let mut world = make_world(-10.0);
        let ball_id = make_ball(&mut world, 0.0, 0.5, 0.0, 0.0);
        mass = body_get_mass(&world, ball_id);
        fast_speed = body_get_safety_factor(&world, ball_id) * body_get_min_extent(&world, ball_id)
            / TIME_STEP;
    }

    let weight_impulse = mass * 10.0 * TIME_STEP;

    let mut failed = false;

    for &height in &heights {
        let impact_speed = (20.0 * height).sqrt();
        let expect_toi = impact_speed > fast_speed;

        // Float noise in the solver velocities scales with the impact speed, and the ball is heavy
        let tolerance = 1e-5 * mass * (10.0 + impact_speed);

        for &restitution in &restitutions {
            let result = measure_drop_impulse(restitution, height);

            let reversal = (1.0 + restitution) * mass * result.approach_speed;
            let lower = reversal - weight_impulse;
            let upper = reversal + weight_impulse * result.contact_steps as f32;

            println!(
                "    impulse drop {:4.1} e {:.2} toi {} at {:+.4} -> worst step error {:.1e}, bounce {:.1} in [{:.1}, {:.1}] over {} steps",
                height,
                restitution,
                result.toi_steps,
                result.first_separation,
                result.worst_error,
                result.bounce_impulse,
                lower,
                upper,
                result.contact_steps
            );

            if result.worst_error > tolerance {
                failed = true;
            }

            if result.contact_steps == 0 || result.toi_impulse_steps > 0 {
                failed = true;
            }

            let slack = result.contact_steps as f32 * tolerance;
            if result.bounce_impulse < lower - slack || result.bounce_impulse > upper + slack {
                failed = true;
            }

            if (result.toi_steps > 0) != expect_toi {
                println!(
                    "    continuous collision {} at {:.1} m/s (threshold {:.1} m/s)",
                    if expect_toi { "expected" } else { "unexpected" },
                    impact_speed,
                    fast_speed
                );
                failed = true;
            }
        }
    }

    assert!(!failed);
}

/// The sampled approach speed and the Poisson accumulator are per manifold point state, so they
/// must survive the split across workers.
/// C compares worker counts 1, 2, and 4; the serial port compares three fresh runs.
/// (WorkerParityTest)
#[test]
fn worker_parity_test() {
    let hash1 = run_worker_scene();
    let hash2 = run_worker_scene();
    let hash4 = run_worker_scene();

    println!("    worker hashes 0x{hash1:016x} 0x{hash2:016x} 0x{hash4:016x}");

    assert_eq!(hash1, hash2);
    assert_eq!(hash1, hash4);
}
