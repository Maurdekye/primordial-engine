//! Body time of impact mover tests from test_body_query.c (the MoverTOI* subtests).
//!
//! Split from body_query_tests.rs to satisfy the file-length limit.
//!
//! SPDX-FileCopyrightText: 2025 Erin Catto
//! SPDX-License-Identifier: MIT

use crate::body::{body_time_of_impact_mover, create_body, BodyToiResult};
use crate::body_query_tests::{create_query_world, identity_at};
use crate::distance::{make_proxy, time_of_impact, Sweep, ToiInput};
use crate::geometry::{Capsule, Sphere};
use crate::hull::{get_hull_points, make_box_hull, make_offset_box_hull};
use crate::math_functions::{
    distance, is_normalized, length, make_quat_from_axis_angle, mul_add, mul_quat, mul_sv,
    offset_pos, sub_pos, to_relative_transform, Pos, Vec3, WorldTransform, PI, POS_ZERO,
    QUAT_IDENTITY, VEC3_ZERO,
};
use crate::shape::{create_hull_shape, create_sphere_shape, shape_is_valid};
use crate::types::{default_body_def, default_query_filter, default_shape_def};

fn ensure_small(value: f32, tolerance: f32) {
    assert!(
        !(value < -tolerance || tolerance < value),
        "|{value}| > tolerance {tolerance}"
    );
}

// The mover capsule is expressed in the query frame, so a core segment starting at the origin
// stands the character on the query point. Targets sit on the sweep line at y = 0.
fn make_standing_mover() -> Capsule {
    Capsule {
        center1: Vec3::new(0.0, 0.0, 0.0),
        center2: Vec3::new(0.0, 1.0, 0.0),
        radius: 0.25,
    }
}

fn is_null_shape(result: &BodyToiResult) -> bool {
    result.shape_id.is_null()
}

#[test]
fn mover_toi_hits_box() {
    let (mut world, body_id) = create_query_world();

    let box_hull = make_box_hull(0.5, 0.5, 0.5);
    let shape_id = create_hull_shape(&mut world, body_id, &default_shape_def(), &box_hull.base);

    // Face at x = 4.5, mover radius 0.25, so 4.25 of the 10 unit sweep is free.
    let mover = make_standing_mover();
    let body_transform = identity_at(5.0, 0.0, 0.0);
    let result = body_time_of_impact_mover(
        &world,
        body_id,
        POS_ZERO,
        &mover,
        Vec3::new(10.0, 0.0, 0.0),
        &default_query_filter(),
        body_transform,
        body_transform,
    );

    ensure_small(result.fraction - 0.425, 1e-2);
    assert!(is_normalized(result.normal));
    ensure_small(result.normal.x + 1.0, 1e-3);

    // The result carries a shape id, so the hit shape must come back identified.
    assert!(shape_is_valid(&world, result.shape_id));
    assert_eq!(result.shape_id.index1, shape_id.index1);
    assert_eq!(result.shape_id.generation, shape_id.generation);
}

#[test]
fn mover_toi_separated() {
    let (mut world, body_id) = create_query_world();

    let box_hull = make_box_hull(0.5, 0.5, 0.5);
    create_hull_shape(&mut world, body_id, &default_shape_def(), &box_hull.base);

    // Sweeping along +Y holds the X gap at 4.25 for the whole interval.
    let mover = make_standing_mover();
    let body_transform = identity_at(5.0, 0.0, 0.0);
    let result = body_time_of_impact_mover(
        &world,
        body_id,
        POS_ZERO,
        &mover,
        Vec3::new(0.0, 10.0, 0.0),
        &default_query_filter(),
        body_transform,
        body_transform,
    );

    ensure_small(result.fraction - 1.0, 1e-6);
    assert!(!shape_is_valid(&world, result.shape_id));
}

#[test]
fn mover_toi_overlapped() {
    let (mut world, body_id) = create_query_world();

    let box_hull = make_box_hull(0.5, 0.5, 0.5);
    create_hull_shape(&mut world, body_id, &default_shape_def(), &box_hull.base);

    // Mover starts buried in the box, so there is no free interval to search.
    let mover = make_standing_mover();
    let body_transform = identity_at(0.0, 0.0, 0.0);
    let result = body_time_of_impact_mover(
        &world,
        body_id,
        POS_ZERO,
        &mover,
        Vec3::new(10.0, 0.0, 0.0),
        &default_query_filter(),
        body_transform,
        body_transform,
    );

    // Overlap should be ignored.
    ensure_small(result.fraction - 1.0, 1e-6);
    assert!(is_null_shape(&result));
}

// Two shapes on the sweep line must resolve to the nearer one whatever order the shape list
// hands them to the loop. Shapes are pushed on the head of the list, so the two bodies below
// walk their shapes in opposite orders.
#[test]
fn mover_toi_closest_shape() {
    let (mut world, near_first_id) = create_query_world();

    let near_last_id = create_body(&mut world, &default_body_def());

    let shape_def = default_shape_def();
    let near_sphere = Sphere {
        center: Vec3::new(5.0, 0.0, 0.0),
        radius: 0.5,
    };
    let far_sphere = Sphere {
        center: Vec3::new(9.0, 0.0, 0.0),
        radius: 0.5,
    };

    let near_first_hit = create_sphere_shape(&mut world, near_first_id, &shape_def, &near_sphere);
    create_sphere_shape(&mut world, near_first_id, &shape_def, &far_sphere);

    create_sphere_shape(&mut world, near_last_id, &shape_def, &far_sphere);
    let near_last_hit = create_sphere_shape(&mut world, near_last_id, &shape_def, &near_sphere);

    let mover = make_standing_mover();
    let body_transform = identity_at(0.0, 0.0, 0.0);
    let translation = Vec3::new(10.0, 0.0, 0.0);

    let near_first = body_time_of_impact_mover(
        &world,
        near_first_id,
        POS_ZERO,
        &mover,
        translation,
        &default_query_filter(),
        body_transform,
        body_transform,
    );
    let near_last = body_time_of_impact_mover(
        &world,
        near_last_id,
        POS_ZERO,
        &mover,
        translation,
        &default_query_filter(),
        body_transform,
        body_transform,
    );

    ensure_small(near_first.fraction - 0.425, 1e-2);
    assert_eq!(near_first.shape_id.index1, near_first_hit.index1);

    ensure_small(near_last.fraction - near_first.fraction, 1e-4);
    assert_eq!(near_last.shape_id.index1, near_last_hit.index1);
}

// A shape the sweep clears must not erase a hit found earlier in the list. The off path sphere
// is created first so the list hands it over last.
#[test]
fn mover_toi_keeps_hit_after_miss() {
    let (mut world, body_id) = create_query_world();

    let shape_def = default_shape_def();
    let off_path = Sphere {
        center: Vec3::new(5.0, 0.0, 20.0),
        radius: 0.5,
    };
    let on_path = Sphere {
        center: Vec3::new(5.0, 0.0, 0.0),
        radius: 0.5,
    };
    create_sphere_shape(&mut world, body_id, &shape_def, &off_path);
    let on_path_id = create_sphere_shape(&mut world, body_id, &shape_def, &on_path);

    let mover = make_standing_mover();
    let body_transform = identity_at(0.0, 0.0, 0.0);
    let result = body_time_of_impact_mover(
        &world,
        body_id,
        POS_ZERO,
        &mover,
        Vec3::new(10.0, 0.0, 0.0),
        &default_query_filter(),
        body_transform,
        body_transform,
    );

    ensure_small(result.fraction - 0.425, 1e-2);
    assert_eq!(result.shape_id.index1, on_path_id.index1);
}

// The mover capsule points live in the query frame, so sliding both the mover and the body by
// the same offset must not move the impact.
#[test]
fn mover_toi_mover_offset() {
    let (mut world, body_id) = create_query_world();

    let box_hull = make_box_hull(0.5, 0.5, 0.5);
    create_hull_shape(&mut world, body_id, &default_shape_def(), &box_hull.base);

    let translation = Vec3::new(10.0, 0.0, 0.0);
    let filter = default_query_filter();

    let at_origin = make_standing_mover();
    let transform_a = identity_at(5.0, 0.0, 0.0);
    let result_a = body_time_of_impact_mover(
        &world,
        body_id,
        POS_ZERO,
        &at_origin,
        translation,
        &filter,
        transform_a,
        transform_a,
    );

    let offset = Capsule {
        center1: Vec3::new(2.0, 0.0, 0.0),
        center2: Vec3::new(2.0, 1.0, 0.0),
        radius: 0.25,
    };
    let transform_b = identity_at(7.0, 0.0, 0.0);
    let result_b = body_time_of_impact_mover(
        &world,
        body_id,
        POS_ZERO,
        &offset,
        translation,
        &filter,
        transform_b,
        transform_b,
    );

    ensure_small(result_b.fraction - result_a.fraction, 1e-4);
    ensure_small((result_b.point.x - result_a.point.x) as f32 - 2.0, 1e-3);
}

// The body sweeps between the two supplied transforms. A bar spinning a quarter turn about Y
// reaches a mover that its start pose clears.
#[test]
fn mover_toi_rotating_body() {
    let (mut world, body_id) = create_query_world();

    let bar = make_box_hull(2.0, 0.25, 0.25);
    create_hull_shape(&mut world, body_id, &default_shape_def(), &bar.base);

    // Query origin puts the mover at world (0,0,2.1), just inside the swept end of the bar.
    let origin = Pos {
        x: 0.0,
        y: 0.0,
        z: 2.1f32 as _,
    };
    let mover = make_standing_mover();
    let transform1 = identity_at(0.0, 0.0, 0.0);
    let transform2 = WorldTransform {
        p: POS_ZERO,
        q: make_quat_from_axis_angle(Vec3::new(0.0, 1.0, 0.0), 0.5 * PI),
    };

    let spinning = body_time_of_impact_mover(
        &world,
        body_id,
        origin,
        &mover,
        VEC3_ZERO,
        &default_query_filter(),
        transform1,
        transform2,
    );

    assert!(0.0 < spinning.fraction && spinning.fraction < 1.0);

    // Holding the start pose leaves the bar along X and well clear.
    let still = body_time_of_impact_mover(
        &world,
        body_id,
        origin,
        &mover,
        VEC3_ZERO,
        &default_query_filter(),
        transform1,
        transform1,
    );

    ensure_small(still.fraction - 1.0, 1e-6);
    assert!(is_null_shape(&still));
}

// Everything is re-centered on the origin, so a huge origin must not shift the fraction and the
// hit point comes back in the origin frame.
#[test]
fn mover_toi_far_from_origin() {
    let (mut world, body_id) = create_query_world();

    let box_hull = make_box_hull(0.5, 0.5, 0.5);
    create_hull_shape(&mut world, body_id, &default_shape_def(), &box_hull.base);

    let origin = Pos {
        x: 1.0e6f32 as _,
        y: -2.0e6f32 as _,
        z: 5.0e5f32 as _,
    };
    let mover = make_standing_mover();
    let body_transform = WorldTransform {
        p: offset_pos(origin, Vec3::new(5.0, 0.0, 0.0)),
        q: QUAT_IDENTITY,
    };
    let result = body_time_of_impact_mover(
        &world,
        body_id,
        origin,
        &mover,
        Vec3::new(10.0, 0.0, 0.0),
        &default_query_filter(),
        body_transform,
        body_transform,
    );

    ensure_small(result.fraction - 0.425, 1e-2);
    ensure_small(result.normal.x + 1.0, 1e-3);
    ensure_small((result.point.x - origin.x) as f32 - 4.4, 0.5);
}

#[test]
fn mover_toi_filter() {
    let (mut world, body_id) = create_query_world();

    let box_hull = make_box_hull(0.5, 0.5, 0.5);
    create_hull_shape(&mut world, body_id, &default_shape_def(), &box_hull.base);

    // Geometry is on the sweep line, but a zero mask rejects every category.
    let mover = make_standing_mover();
    let mut filter = default_query_filter();
    filter.mask_bits = 0;
    let body_transform = identity_at(5.0, 0.0, 0.0);
    let result = body_time_of_impact_mover(
        &world,
        body_id,
        POS_ZERO,
        &mover,
        Vec3::new(10.0, 0.0, 0.0),
        &filter,
        body_transform,
        body_transform,
    );

    ensure_small(result.fraction - 1.0, 1e-6);
}

// A shape the mover starts inside is skipped, but the sweep still runs against the other shapes
// on the body.
#[test]
fn mover_toi_overlap_skips_shape() {
    let (mut world, body_id) = create_query_world();

    let shape_def = default_shape_def();

    let buried = make_box_hull(0.5, 0.5, 0.5);
    create_hull_shape(&mut world, body_id, &shape_def, &buried.base);

    // Face at x = 4.5 like MoverTOIHitsBox
    let on_path = make_offset_box_hull(0.5, 0.5, 0.5, Vec3::new(5.0, 0.0, 0.0));
    let on_path_id = create_hull_shape(&mut world, body_id, &shape_def, &on_path.base);

    let mover = make_standing_mover();
    let body_transform = identity_at(0.0, 0.0, 0.0);
    let result = body_time_of_impact_mover(
        &world,
        body_id,
        POS_ZERO,
        &mover,
        Vec3::new(10.0, 0.0, 0.0),
        &default_query_filter(),
        body_transform,
        body_transform,
    );

    ensure_small(result.fraction - 0.425, 1e-2);
    assert_eq!(result.shape_id.index1, on_path_id.index1);
    ensure_small(result.normal.x + 1.0, 1e-3);
}

fn random_float(seed: &mut u32, lower: f32, upper: f32) -> f32 {
    let mut x = *seed;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *seed = x;
    let unit = (x >> 8) as f32 / 16777216.0;
    lower + (upper - lower) * unit
}

fn random_vector(seed: &mut u32, lower: f32, upper: f32) -> Vec3 {
    let x = random_float(seed, lower, upper);
    let y = random_float(seed, lower, upper);
    let z = random_float(seed, lower, upper);
    Vec3 { x, y, z }
}

fn random_direction(seed: &mut u32) -> Vec3 {
    loop {
        let v = random_vector(seed, -1.0, 1.0);
        let len = length(v);
        if 0.1 < len && len < 1.0 {
            return mul_sv(1.0 / len, v);
        }
    }
}

// The mover sweep follows the solver's continuous rule: any time of impact strictly inside the
// sweep counts, whatever the root finder reports. A capsule pivoting around a hull vertex can
// exhaust the iteration cap, and that must not read as a clean miss. Compare against the raw
// sweep built from the same inputs.
#[test]
fn mover_toi_matches_sweep() {
    let (mut world, body_id) = create_query_world();

    let bar = make_box_hull(2.0, 0.25, 0.25);
    let shape_id = create_hull_shape(&mut world, body_id, &default_shape_def(), &bar.base);

    let mut seed: u32 = 0x9E37_79B9;
    let mut hit_count = 0;
    for _ in 0..2000 {
        // C draws these inside nested call arguments, whose evaluation order C leaves
        // unspecified. The draws are sequenced left to right here; the check compares against
        // a raw sweep built from the same inputs, so it holds for any draw order.
        let center1 = random_vector(&mut seed, -0.2, 0.2);
        let segment_length = random_float(&mut seed, 0.05, 1.5);
        let segment_direction = random_direction(&mut seed);
        let center2 = mul_add(center1, segment_length, segment_direction);
        let radius = random_float(&mut seed, 0.05, 0.5);
        let mover = Capsule {
            center1,
            center2,
            radius,
        };
        let capsule_points = [mover.center1, mover.center2];

        let o = random_vector(&mut seed, -3.5, 3.5);
        let origin = Pos {
            x: o.x as _,
            y: o.y as _,
            z: o.z as _,
        };
        let translation_length = random_float(&mut seed, 0.0, 3.0);
        let translation = mul_sv(translation_length, random_direction(&mut seed));

        let axis1 = random_direction(&mut seed);
        let angle1 = random_float(&mut seed, 0.0, PI);
        let transform1 = WorldTransform {
            p: POS_ZERO,
            q: make_quat_from_axis_angle(axis1, angle1),
        };
        let spin_axis = random_direction(&mut seed);
        let spin_angle = random_float(&mut seed, 0.001, PI);
        let spin = make_quat_from_axis_angle(spin_axis, spin_angle);
        let shift = random_vector(&mut seed, -1.0, 1.0);
        let transform2 = WorldTransform {
            p: Pos {
                x: shift.x as _,
                y: shift.y as _,
                z: shift.z as _,
            },
            q: mul_quat(spin, transform1.q),
        };

        let result = body_time_of_impact_mover(
            &world,
            body_id,
            origin,
            &mover,
            translation,
            &default_query_filter(),
            transform1,
            transform2,
        );

        // Same sweep the body query builds. The static body keeps its center at the body origin.
        let xf1 = to_relative_transform(transform1, origin);
        let xf2 = to_relative_transform(transform2, origin);

        let input = ToiInput {
            proxy_a: make_proxy(get_hull_points(&bar.base), 0.0),
            proxy_b: make_proxy(&capsule_points, mover.radius),
            sweep_a: Sweep {
                local_center: VEC3_ZERO,
                c1: xf1.p,
                c2: xf2.p,
                q1: transform1.q,
                q2: transform2.q,
            },
            sweep_b: Sweep {
                local_center: VEC3_ZERO,
                c1: VEC3_ZERO,
                c2: translation,
                q1: QUAT_IDENTITY,
                q2: QUAT_IDENTITY,
            },
            max_fraction: 1.0,
        };

        let raw = time_of_impact(&input);

        if 0.0 < raw.fraction && raw.fraction < 1.0 {
            ensure_small(result.fraction - raw.fraction, 1e-6);
            assert_eq!(result.shape_id.index1, shape_id.index1);
            assert!(is_normalized(result.normal));
            ensure_small(distance(sub_pos(result.point, origin), raw.point), 1e-4);
            hit_count += 1;
        } else {
            ensure_small(result.fraction - 1.0, 1e-6);
            assert!(is_null_shape(&result));
        }
    }

    assert!(hit_count > 0);
}
