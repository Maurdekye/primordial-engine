//! Restitution energy test from test_restitution.c (EnergyTest). Split from
//! `restitution_tests_extra.rs` to satisfy the file-length limit.
//!
//! SPDX-FileCopyrightText: 2026 Erin Catto
//! SPDX-License-Identifier: MIT

use crate::body::create_body;
use crate::hull::make_box_hull;
use crate::id::BodyId;
use crate::math_functions::{make_quat_from_axis_angle, Vec3, VEC3_AXIS_Z};
use crate::restitution_tests::{make_world, pos, SUB_STEP_COUNT, TIME_STEP};
use crate::restitution_tests_extra::measure_energy;
use crate::shape::create_hull_shape;
use crate::types::{default_body_def, default_shape_def, BodyType};

/// (EnergyScene)
struct EnergyScene {
    name: &'static str,
    safety_factor: f32,
    friction: f32,
    angle: f32,
    spin: f32,
    height: f32,
    velocity_y: f32,
    half_width: f32,
    half_height: f32,
    bullet: bool,
    stack: bool,
    step_count: i32,
}

/// Peak energy over the run relative to the start, for every dynamic body in the scene.
/// Perfectly elastic and the energy may still only go down: friction and the soft contact
/// position correction dissipate, and a bounce may return at most what the contact absorbed.
/// (MeasurePeakEnergy)
fn measure_peak_energy(scene: &EnergyScene) -> f32 {
    let mut world = make_world(-10.0);

    let mut ground_def = default_body_def();
    ground_def.position = pos(0.0, -1.0, 0.0);
    let ground_id = create_body(&mut world, &ground_def);
    let mut ground_shape = default_shape_def();
    ground_shape.base_material.friction = scene.friction;
    let ground = make_box_hull(40.0, 1.0, 40.0);
    create_hull_shape(&mut world, ground_id, &ground_shape, &ground.base);

    let mut shape_def = default_shape_def();
    shape_def.density = 1.0;
    shape_def.base_material.friction = scene.friction;
    shape_def.base_material.restitution = 1.0;
    let box_hull = make_box_hull(scene.half_width, scene.half_height, scene.half_width);

    let mut body_ids: Vec<BodyId> = Vec::with_capacity(4);

    if scene.stack {
        // Resting box under the dropped one, like the TwoBoxRestitution sample
        let mut body_def = default_body_def();
        body_def.type_ = BodyType::Dynamic;
        body_def.position = pos(0.0, scene.half_height, 0.0);
        body_def.safety_factor = scene.safety_factor;
        let body_id = create_body(&mut world, &body_def);
        create_hull_shape(&mut world, body_id, &shape_def, &box_hull.base);
        body_ids.push(body_id);
    }

    {
        let mut body_def = default_body_def();
        body_def.type_ = BodyType::Dynamic;
        body_def.position = pos(0.0, scene.height, 0.0);
        body_def.rotation = make_quat_from_axis_angle(VEC3_AXIS_Z, scene.angle);
        body_def.angular_velocity = Vec3::new(0.0, 0.0, scene.spin);
        body_def.linear_velocity = Vec3::new(0.0, scene.velocity_y, 0.0);
        body_def.safety_factor = scene.safety_factor;
        body_def.is_bullet = scene.bullet;
        let body_id = create_body(&mut world, &body_def);
        create_hull_shape(&mut world, body_id, &shape_def, &box_hull.base);
        body_ids.push(body_id);
    }

    let mut start = 0.0f32;
    for &body_id in &body_ids {
        start += measure_energy(&world, body_id);
    }

    let mut peak = start;
    let mut peak_step = -1;

    for i in 0..scene.step_count {
        world.step(TIME_STEP, SUB_STEP_COUNT);

        let mut energy = 0.0f32;
        for &body_id in &body_ids {
            energy += measure_energy(&world, body_id);
        }

        if energy > peak {
            peak = energy;
            peak_step = i;
        }
    }

    let ratio = peak / start;
    println!(
        "    energy {:<24} peak {ratio:.5} at step {peak_step}",
        scene.name
    );
    ratio
}

/// Perfectly elastic material with friction, dropped every way that used to gain energy: a
/// spinning edge landing, tumbling, a flat landing with spin, a box falling on a resting box, and
/// a tall box landing flat on its narrow end. Energy must never rise above the start. This is the
/// load bearing property of restitution. Accuracy of the coefficient and freedom from spin are
/// secondary.
///
/// The tall boxes guard the stage iteration count. Their contact points sit close together while
/// the inertia is long, so the off diagonal of the multi point normal mass is large and one sweep
/// overshoots. (EnergyTest)
#[test]
fn energy_test() {
    macro_rules! scene {
        ($name:expr, $sf:expr, $fr:expr, $angle:expr, $spin:expr, $h:expr, $vy:expr, $hw:expr, $hh:expr, $bullet:expr, $stack:expr, $steps:expr) => {
            EnergyScene {
                name: $name,
                safety_factor: $sf,
                friction: $fr,
                angle: $angle,
                spin: $spin,
                height: $h,
                velocity_y: $vy,
                half_width: $hw,
                half_height: $hh,
                bullet: $bullet,
                stack: $stack,
                step_count: $steps,
            }
        };
    }

    let scenes = [
        scene!(
            "spinning edge",
            0.01,
            0.6,
            -1.43,
            7.9,
            0.7,
            -1.9,
            0.5,
            0.5,
            false,
            false,
            300
        ),
        scene!(
            "spinning edge fast",
            0.01,
            0.6,
            -0.8,
            12.0,
            3.0,
            -8.0,
            0.5,
            0.5,
            false,
            false,
            300
        ),
        scene!("tumble", 0.01, 0.6, -0.3, 3.0, 8.0, 0.0, 0.5, 0.5, false, false, 600),
        scene!(
            "tumble discrete",
            0.5,
            0.6,
            -0.3,
            3.0,
            8.0,
            0.0,
            0.5,
            0.5,
            false,
            false,
            600
        ),
        scene!(
            "flat with spin",
            0.01,
            0.6,
            0.0,
            2.0,
            2.0,
            -5.0,
            0.5,
            0.5,
            false,
            false,
            300
        ),
        scene!(
            "flat frictionless",
            0.01,
            0.0,
            0.0,
            0.0,
            10.0,
            0.0,
            0.5,
            0.5,
            false,
            false,
            1200
        ),
        scene!("edge", 0.01, 0.6, -0.2, 0.0, 5.0, 0.0, 0.5, 0.5, false, false, 600),
        scene!("two box", 0.02, 0.6, 0.0, 0.0, 4.0, 0.0, 0.5, 0.5, true, true, 1500),
        scene!(
            "two box discrete",
            0.5,
            0.6,
            0.0,
            0.0,
            4.0,
            0.0,
            0.5,
            0.5,
            false,
            true,
            1500
        ),
        scene!(
            "tall flat frictionless",
            0.01,
            0.0,
            0.0,
            0.0,
            10.0,
            0.0,
            0.5,
            1.0,
            false,
            false,
            1200
        ),
        scene!(
            "tall flat friction",
            0.01,
            0.6,
            0.0,
            0.0,
            10.0,
            0.0,
            0.5,
            1.0,
            false,
            false,
            1200
        ),
        scene!(
            "narrow flat frictionless",
            0.01,
            0.0,
            0.0,
            0.0,
            10.0,
            0.0,
            0.25,
            1.0,
            false,
            false,
            1200
        ),
        scene!(
            "narrow flat friction",
            0.01,
            0.6,
            0.0,
            0.0,
            10.0,
            0.0,
            0.25,
            1.0,
            false,
            false,
            1200
        ),
    ];

    // The soft contact position correction lifts a body that lands inside the ground, which is
    // potential energy from nowhere at any restitution. Continuous collision keeps landings on
    // the surface, so the tolerance only has to cover the stage. A four point flat landing does
    // not converge at the default two stage iterations the way the two point landing does in 2D:
    // the cube comes back with 0.2% extra. The in-relax scheme this replaced gained 2.5% to 29%
    // on these scenes, so the tolerance still separates the two.
    let tolerance = 5.0e-3;
    let discrete_tolerance = 2.0e-2;

    let mut failed = false;

    for scene in &scenes {
        let ratio = measure_peak_energy(scene);
        let limit = if scene.safety_factor < 0.1 {
            tolerance
        } else {
            discrete_tolerance
        };
        if ratio > 1.0 + limit {
            failed = true;
        }
    }

    assert!(!failed);
}
