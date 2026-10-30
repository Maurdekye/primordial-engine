//! Restitution tests from test_restitution.c (HeadOnTest through RestingTest). The remaining
//! subtests live in `restitution_tests_extra.rs` to satisfy the file-length limit.
//!
//! C runs every subtest in one battery that prints all measurements before asserting. Each
//! subtest is its own Rust test here; each still prints its measurements before asserting.
//!
//! SPDX-FileCopyrightText: 2026 Erin Catto
//! SPDX-License-Identifier: MIT

use crate::body::{
    body_get_angular_velocity, body_get_linear_velocity, body_get_mass, body_get_position,
    create_body,
};
use crate::geometry::Sphere;
use crate::hull::{make_box_hull, make_cube_hull};
use crate::id::BodyId;
use crate::math_functions::{abs_float, length, max_float, min_float, Pos, Vec3, VEC3_ZERO};
use crate::shape::{create_hull_shape, create_sphere_shape};
use crate::types::{default_body_def, default_shape_def, default_world_def, BodyType};
use crate::world::World;

pub(crate) const TIME_STEP: f32 = 1.0 / 60.0;
pub(crate) const SUB_STEP_COUNT: i32 = 4;

// Impact speed shared by every scenario that measures a coefficient. Well above the default
// restitution threshold so the bounce is always armed.
pub(crate) const IMPACT_SPEED: f32 = 5.0;

// A flat landing converges its Gauss-Seidel coupling in four stage iterations and the default is
// two, so the tests that read the coefficient or the spin of a flat landing pin the count
pub(crate) const RESTITUTION_ITERATIONS: i32 = 4;

pub(crate) fn pos(x: f32, y: f32, z: f32) -> Pos {
    Pos {
        x: x as _,
        y: y as _,
        z: z as _,
    }
}

/// (MakeWorld)
pub(crate) fn make_world(gravity_y: f32) -> World {
    let mut world_def = default_world_def();
    world_def.gravity = Vec3::new(0.0, gravity_y, 0.0);
    world_def.enable_sleep = false;
    World::new(&world_def)
}

/// (MakeWorldIterations)
pub(crate) fn make_world_iterations(gravity_y: f32, restitution_iterations: i32) -> World {
    let mut world_def = default_world_def();
    world_def.gravity = Vec3::new(0.0, gravity_y, 0.0);
    world_def.enable_sleep = false;
    world_def.restitution_iterations = restitution_iterations;
    World::new(&world_def)
}

/// Ground with its top surface at y = 0. (MakeGround)
pub(crate) fn make_ground(world: &mut World, restitution: f32) {
    let mut body_def = default_body_def();
    body_def.position = pos(0.0, -1.0, 0.0);
    let ground_id = create_body(world, &body_def);

    let mut shape_def = default_shape_def();
    shape_def.base_material.friction = 0.0;
    shape_def.base_material.restitution = restitution;
    let box_hull = make_box_hull(40.0, 1.0, 40.0);
    create_hull_shape(world, ground_id, &shape_def, &box_hull.base);
}

/// (MakeBall)
pub(crate) fn make_ball(
    world: &mut World,
    x: f32,
    y: f32,
    velocity_y: f32,
    restitution: f32,
) -> BodyId {
    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Dynamic;
    body_def.position = pos(x, y, 0.0);
    body_def.linear_velocity = Vec3::new(0.0, velocity_y, 0.0);
    let body_id = create_body(world, &body_def);

    let mut shape_def = default_shape_def();
    shape_def.base_material.friction = 0.0;
    shape_def.base_material.restitution = restitution;
    let sphere = Sphere {
        center: VEC3_ZERO,
        radius: 0.5,
    };
    create_sphere_shape(world, body_id, &shape_def, &sphere);
    body_id
}

/// Two free spheres closing head on with no gravity. The coefficient is the ratio of relative
/// normal speeds at the contact, which is the mass independent definition. (MeasureHeadOn)
fn measure_head_on(restitution: f32, density_b: f32, momentum_error: &mut f32) -> f32 {
    let mut world = make_world(0.0);

    let sphere = Sphere {
        center: VEC3_ZERO,
        radius: 0.5,
    };
    let mut shape_def = default_shape_def();
    shape_def.density = 1.0;
    shape_def.base_material.friction = 0.0;
    shape_def.base_material.restitution = restitution;

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Dynamic;
    body_def.position = pos(-1.0, 0.0, 0.0);
    body_def.linear_velocity = Vec3::new(0.5 * IMPACT_SPEED, 0.0, 0.0);
    let id_a = create_body(&mut world, &body_def);
    create_sphere_shape(&mut world, id_a, &shape_def, &sphere);

    body_def.position = pos(1.0, 0.0, 0.0);
    body_def.linear_velocity = Vec3::new(-0.5 * IMPACT_SPEED, 0.0, 0.0);
    let id_b = create_body(&mut world, &body_def);
    shape_def.density = density_b;
    create_sphere_shape(&mut world, id_b, &shape_def, &sphere);

    let mass_a = body_get_mass(&world, id_a);
    let mass_b = body_get_mass(&world, id_b);
    let momentum0 = mass_a * body_get_linear_velocity(&world, id_a).x
        + mass_b * body_get_linear_velocity(&world, id_b).x;

    for _ in 0..120 {
        world.step(TIME_STEP, SUB_STEP_COUNT);
    }

    let v_a = body_get_linear_velocity(&world, id_a).x;
    let v_b = body_get_linear_velocity(&world, id_b).x;
    let momentum1 = mass_a * v_a + mass_b * v_b;

    *momentum_error = (momentum1 - momentum0) / (mass_a + mass_b);
    (v_b - v_a) / IMPACT_SPEED
}

/// Ball driven into static ground with no gravity so the measurement carries no gravity bias.
/// The gap shifts where inside the time step the impact lands. (MeasureGroundBounce)
fn measure_ground_bounce(restitution: f32, gap: f32) -> f32 {
    let mut world = make_world(0.0);
    make_ground(&mut world, 0.0);

    let ball_id = make_ball(&mut world, 0.0, 0.5 + gap, -IMPACT_SPEED, restitution);

    for _ in 0..60 {
        world.step(TIME_STEP, SUB_STEP_COUNT);
    }

    let speed = body_get_linear_velocity(&world, ball_id).y;
    speed / IMPACT_SPEED
}

/// Impactor driven onto a column of resting balls pinned against the ground. The support balls
/// are dead so only the top contact can bounce. A supported target has infinite effective mass
/// along the normal, so the coefficient must not depend on the column height.
/// (MeasureSupportedBounce)
fn measure_supported_bounce(restitution: f32, support_count: i32, propagate: bool) -> f32 {
    let mut world_def = default_world_def();
    world_def.gravity = VEC3_ZERO;
    world_def.enable_sleep = false;
    world_def.enable_restitution_propagation = propagate;
    world_def.restitution_iterations = RESTITUTION_ITERATIONS;
    let mut world = World::new(&world_def);
    make_ground(&mut world, 0.0);

    for i in 0..support_count {
        make_ball(&mut world, 0.0, 0.5 + 1.0 * i as f32, 0.0, 0.0);
    }

    let y = 0.5 + 1.0 * support_count as f32 + 0.5 * IMPACT_SPEED * TIME_STEP;
    let ball_id = make_ball(&mut world, 0.0, y, -IMPACT_SPEED, restitution);

    for _ in 0..60 {
        world.step(TIME_STEP, SUB_STEP_COUNT);
    }

    let speed = body_get_linear_velocity(&world, ball_id).y;
    speed / IMPACT_SPEED
}

/// Flat box landing on all four corners at once with no gravity. (MeasureFlatBounce)
fn measure_flat_bounce(restitution: f32, sub_step_count: i32, spin: &mut f32) -> f32 {
    let mut world = make_world_iterations(0.0, RESTITUTION_ITERATIONS);
    make_ground(&mut world, 0.0);

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Dynamic;
    body_def.position = pos(0.0, 0.25 + 0.5 * IMPACT_SPEED * TIME_STEP, 0.0);
    body_def.linear_velocity = Vec3::new(0.0, -IMPACT_SPEED, 0.0);
    let box_id = create_body(&mut world, &body_def);

    let mut shape_def = default_shape_def();
    shape_def.density = 1.0;
    shape_def.base_material.friction = 0.0;
    shape_def.base_material.restitution = restitution;
    let box_hull = make_box_hull(1.0, 0.25, 1.0);
    create_hull_shape(&mut world, box_id, &shape_def, &box_hull.base);

    for _ in 0..60 {
        world.step(TIME_STEP, sub_step_count);
    }

    let speed = body_get_linear_velocity(&world, box_id).y;
    *spin = length(body_get_angular_velocity(&world, box_id));
    speed / IMPACT_SPEED
}

/// Perfectly elastic ball dropped under gravity. (MeasureDrop)
fn measure_drop(drop_height: f32, apexes: &mut [f32]) -> usize {
    let mut world = make_world(-10.0);
    make_ground(&mut world, 0.0);

    let ball_id = make_ball(&mut world, 0.0, drop_height, 0.0, 1.0);

    let mut apex_count = 0;
    let mut previous_speed = 0.0;

    let mut i = 0;
    while i < 4000 && apex_count < apexes.len() {
        world.step(TIME_STEP, SUB_STEP_COUNT);

        let speed = body_get_linear_velocity(&world, ball_id).y;
        if previous_speed > 0.0 && speed <= 0.0 {
            apexes[apex_count] = body_get_position(&world, ball_id).y as f32;
            apex_count += 1;
        }
        previous_speed = speed;
        i += 1;
    }

    apex_count
}

/// (HeadOnTest)
#[test]
fn head_on_test() {
    let restitutions = [0.0f32, 0.25, 0.5, 0.75, 1.0];
    let densities = [1.0f32, 10.0, 100.0];
    let tolerance = 0.02;

    let mut failed = false;

    for &density in &densities {
        let mut worst_error = 0.0f32;
        let mut worst_momentum = 0.0f32;

        for &restitution in &restitutions {
            let mut momentum_error = 0.0;
            let measured = measure_head_on(restitution, density, &mut momentum_error);

            let error = abs_float(measured - restitution);
            if error > worst_error {
                worst_error = error;
            }

            if abs_float(momentum_error) > abs_float(worst_momentum) {
                worst_momentum = momentum_error;
            }

            if error > tolerance {
                println!("    head on density {density:.0} e {restitution:.2} -> {measured:.4}");
                failed = true;
            }
        }

        println!(
            "    head on density {density:6.1} worst error {worst_error:.4} momentum drift {worst_momentum:.2e}"
        );

        if abs_float(worst_momentum) > 1.0e-4 {
            failed = true;
        }
    }

    assert!(!failed);
}

/// (PhaseTest)
#[test]
fn phase_test() {
    let restitutions = [0.25f32, 0.5, 0.9];
    let sample_count = 16;
    let tolerance = 0.05;

    let mut failed = false;

    for &restitution in &restitutions {
        let mut minimum = f32::MAX;
        let mut maximum = -f32::MAX;
        let mut sum = 0.0f32;

        for i in 0..sample_count {
            // Sweep the gap across one step of travel so the impact lands at every phase
            let gap = (i as f32 / sample_count as f32) * IMPACT_SPEED * TIME_STEP;
            let measured = measure_ground_bounce(restitution, gap);
            minimum = min_float(minimum, measured);
            maximum = max_float(maximum, measured);
            sum += measured;
        }

        let mean = sum / sample_count as f32;
        let spread = maximum - minimum;
        println!(
            "    phase e {restitution:.2} -> mean {mean:.4} spread {spread:.4} [{minimum:.4}, {maximum:.4}]"
        );

        if spread > tolerance || abs_float(mean - restitution) > tolerance {
            failed = true;
        }
    }

    assert!(!failed);
}

/// By default the restitution stage sweeps only the contacts that carry restitution, so the ball
/// pushes the column down and the ground under it gets no say until the next step: e = 0.9 gives
/// about 0.52 on one support. With propagation enabled the stage sweeps every contact, the ground
/// contact supplies the reaction within the stage and e = 0.9 gives 0.85 on one support and 0.72
/// on three at the pinned four iterations, decaying with depth because the column needs an
/// iteration per level. Propagation costs a normal pass over every contact per iteration, hence
/// the option. Both modes are gated: never above the free coefficient, which would be energy from
/// nowhere, and never collapsed. (SupportedTest)
#[test]
fn supported_test() {
    let restitutions = [0.5f32, 0.9];

    let mut failed = false;

    for propagate in 0..=1 {
        let floor = if propagate == 1 { 0.75 } else { 0.5 };

        for &restitution in &restitutions {
            for n in 0..=3 {
                let measured = measure_supported_bounce(restitution, n, propagate == 1);
                println!(
                    "    supported e {restitution:.2} supports {n} propagation {propagate} -> {measured:.4}"
                );

                if measured > restitution + 0.01 || measured < floor * restitution {
                    failed = true;
                }
            }
        }
    }

    assert!(!failed);
}

/// A symmetric four point landing. The stage solves the points in sequence, so a single sweep
/// leaves the box spinning: the first impulse tilts it and the next point sees a larger closing
/// speed. Four stage iterations converge the coupling and leave a small residual. The tolerance
/// admits that residual and rejects the single sweep behavior. (FlatLandingTest)
#[test]
fn flat_landing_test() {
    let restitutions = [0.5f32, 0.9];
    let sub_step_counts = [4, 8];
    let speed_tolerance = 0.01;
    let spin_tolerance = 0.03;

    let mut failed = false;

    for &restitution in &restitutions {
        for &sub_step_count in &sub_step_counts {
            let mut spin = 0.0;
            let measured = measure_flat_bounce(restitution, sub_step_count, &mut spin);
            println!(
                "    flat e {restitution:.2} substeps {sub_step_count} -> {measured:.4} spin {spin:.4}"
            );

            // Only the shipping sub step count is a gate. The wider count is reported so a
            // convergence problem can be told apart from a formulation problem.
            if sub_step_count != SUB_STEP_COUNT {
                continue;
            }

            if abs_float(measured - restitution) > speed_tolerance || spin > spin_tolerance {
                failed = true;
            }
        }
    }

    assert!(!failed);
}

/// A flat box carrying spin about a horizontal axis. All four corners stay in contact and the
/// normal constraints are linear in the body velocity, so perfect restitution reverses linear and
/// angular velocity. Four stage iterations bring both within a few thousandths. (SpinTest)
#[test]
fn spin_test() {
    let spins = [0.0f32, 0.5, 1.0, 2.0];

    let mut failed = false;

    for &spin_in in &spins {
        let mut world = make_world_iterations(0.0, RESTITUTION_ITERATIONS);
        make_ground(&mut world, 0.0);

        let mut body_def = default_body_def();
        body_def.type_ = BodyType::Dynamic;
        body_def.position = pos(0.0, 0.25 + 0.5 * IMPACT_SPEED * TIME_STEP, 0.0);
        body_def.linear_velocity = Vec3::new(0.0, -IMPACT_SPEED, 0.0);
        body_def.angular_velocity = Vec3::new(0.0, 0.0, spin_in);
        let box_id = create_body(&mut world, &body_def);

        let mut shape_def = default_shape_def();
        shape_def.density = 1.0;
        shape_def.base_material.friction = 0.0;
        shape_def.base_material.restitution = 1.0;
        let box_hull = make_box_hull(1.0, 0.25, 1.0);
        create_hull_shape(&mut world, box_id, &shape_def, &box_hull.base);

        for _ in 0..60 {
            world.step(TIME_STEP, SUB_STEP_COUNT);
        }

        let speed = body_get_linear_velocity(&world, box_id).y;
        let spin = body_get_angular_velocity(&world, box_id).z;

        println!(
            "    spin in {spin_in:+.2} -> vy {speed:+.4} (want {IMPACT_SPEED:+.4})  wz {spin:+.4} (want {:+.4})",
            -spin_in
        );

        if abs_float(speed - IMPACT_SPEED) > 0.05 || abs_float(spin + spin_in) > 0.05 {
            failed = true;
        }
    }

    assert!(!failed);
}

/// Perfectly elastic ball under gravity. Only heights where continuous collision engages are
/// used. Continuous collision lands the ball on the surface, so the bounce is armed from the true
/// impact speed and the apex holds. Slower drops resolve the impact inside the overlap and the
/// penetration recovery adds height, by design. The 10 m case is the long horizon read. (DropTest)
#[test]
fn drop_test() {
    let heights = [40.0f32, 20.0, 10.0];

    let mut failed = false;

    for &height in &heights {
        let mut apexes = [0.0f32; 6];
        let apex_count = measure_drop(height, &mut apexes);

        let mut line = format!("    drop {height:5.1} ->");
        for apex in apexes.iter().take(apex_count) {
            line += &format!(" {apex:8.3}");
        }
        println!("{line}");

        if apex_count < apexes.len() {
            failed = true;
            continue;
        }

        let mut highest = apexes[0];
        for &apex in apexes.iter().take(apex_count).skip(1) {
            highest = max_float(highest, apex);
        }

        // A later apex above the first, or a first apex above the drop, is energy from nowhere
        if highest > 1.001 * apexes[0]
            || apexes[0] > 1.001 * height
            || apexes[apex_count - 1] < 0.8 * apexes[0]
        {
            failed = true;
        }
    }

    assert!(!failed);
}

/// A cube dropped flat with an aggressive continuous safety factor so it lands square on the
/// surface. Perfectly elastic and frictionless, so it should come back to the drop height with no
/// rotation. A flat landing solves four coplanar points in sequence and one sweep over-delivers:
/// the first impulse tilts the cube, the next point sees a larger closing speed and the sum
/// overshoots the rigid body answer, leaving spin that tilts the cube for the next landing. Four
/// stage iterations converge the coupling. The bounds below reject the single sweep behavior and
/// any apex above the drop. (SingleBoxTest)
#[test]
fn single_box_test() {
    let mut world = make_world_iterations(-10.0, RESTITUTION_ITERATIONS);
    make_ground(&mut world, 0.0);

    let mut shape_def = default_shape_def();
    shape_def.density = 1.0;
    shape_def.base_material.friction = 0.0;
    shape_def.base_material.restitution = 1.0;
    let box_hull = make_cube_hull(0.5);

    let drop_height = 10.0f32;

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Dynamic;
    body_def.position = pos(0.0, drop_height, 0.0);
    body_def.safety_factor = 0.01;
    let box_id = create_body(&mut world, &body_def);
    create_hull_shape(&mut world, box_id, &shape_def, &box_hull.base);

    let mut first_spin = 0.0f32;
    let mut apexes = [0.0f32; 4];
    let mut spins = [0.0f32; 4];
    let mut apex_count = 0;
    let mut previous_speed = 0.0f32;

    // A nearly elastic bounce from 10 m takes about 165 steps, so budget for four of them
    let mut i = 0;
    while i < 1200 && apex_count < apexes.len() {
        world.step(TIME_STEP, SUB_STEP_COUNT);

        let speed = body_get_linear_velocity(&world, box_id).y;

        if apex_count == 0 && previous_speed <= 0.0 && speed > 0.0 {
            first_spin = length(body_get_angular_velocity(&world, box_id));
        }

        if previous_speed > 0.0 && speed <= 0.0 {
            apexes[apex_count] = body_get_position(&world, box_id).y as f32;
            spins[apex_count] = length(body_get_angular_velocity(&world, box_id));
            apex_count += 1;
        }
        previous_speed = speed;
        i += 1;
    }

    let mut line = format!("    single box first bounce spin {first_spin:.4} apexes");
    for k in 0..apex_count {
        line += &format!(" {:7.3} (spin {:5.2})", apexes[k], spins[k]);
    }
    println!("{line}");

    assert_eq!(apex_count, apexes.len());

    let mut failed = false;

    if first_spin > 0.01 {
        failed = true;
    }

    for &apex in apexes.iter().take(apex_count) {
        if apex < 0.95 * drop_height || apex > 1.001 * drop_height {
            failed = true;
        }
    }

    assert!(!failed);
}

/// (ThresholdTest)
#[test]
fn threshold_test() {
    let world_def = default_world_def();
    let threshold = world_def.restitution_threshold;

    let scales = [0.5f32, 0.9, 1.5, 4.0];

    let mut failed = false;

    for &scale in &scales {
        let speed = scale * threshold;

        let mut world = make_world(0.0);
        make_ground(&mut world, 0.0);
        let ball_id = make_ball(&mut world, 0.0, 0.5 + 0.5 * speed * TIME_STEP, -speed, 1.0);

        for _ in 0..60 {
            world.step(TIME_STEP, SUB_STEP_COUNT);
        }

        let ratio = body_get_linear_velocity(&world, ball_id).y / speed;

        println!("    threshold {scale:.2}x -> {ratio:.4}");

        if scale < 1.0 && ratio > 0.05 {
            failed = true;
        }

        if scale > 1.0 && ratio < 0.9 {
            failed = true;
        }
    }

    assert!(!failed);
}

/// High restitution must not wake a settled stack back up. (RestingTest)
#[test]
fn resting_test() {
    let mut world = make_world(-10.0);
    make_ground(&mut world, 0.9);

    const BOX_COUNT: usize = 10;
    let mut box_ids = [BodyId::default(); BOX_COUNT];

    let mut shape_def = default_shape_def();
    shape_def.density = 1.0;
    shape_def.base_material.friction = 0.6;
    shape_def.base_material.restitution = 0.9;
    let box_hull = make_cube_hull(0.5);

    for (i, box_id) in box_ids.iter_mut().enumerate() {
        let mut body_def = default_body_def();
        body_def.type_ = BodyType::Dynamic;
        body_def.position = pos(0.0, 0.5 + 1.0 * i as f32, 0.0);
        *box_id = create_body(&mut world, &body_def);
        create_hull_shape(&mut world, *box_id, &shape_def, &box_hull.base);
    }

    for _ in 0..240 {
        world.step(TIME_STEP, SUB_STEP_COUNT);
    }

    let settled = body_get_position(&world, box_ids[BOX_COUNT - 1]).y as f32;
    let mut drift = 0.0f32;
    let mut peak_speed = 0.0f32;

    for _ in 0..300 {
        world.step(TIME_STEP, SUB_STEP_COUNT);

        let y = body_get_position(&world, box_ids[BOX_COUNT - 1]).y as f32;
        drift = max_float(drift, abs_float(y - settled));

        for &box_id in &box_ids {
            peak_speed = max_float(peak_speed, length(body_get_linear_velocity(&world, box_id)));
        }
    }

    println!("    resting drift {drift:.5} peak speed {peak_speed:.5}");

    assert!(drift < 0.01);
    assert!(peak_speed < 0.05);
}
