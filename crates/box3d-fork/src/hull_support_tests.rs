//! `b3FindHullSupportVertex` tie-breaking tests (upstream adec25b "SAT optimizations").
//!
//! The support search runs four lanes at a time over the SoA vertices. The SoA tail is padded
//! with repeats of vertex 0, so a padded lane ties vertex 0 exactly. The lane reduction must
//! break ties toward the lowest index so the result is always a real vertex, and it must agree
//! with a plain scan that keeps the first vertex holding the maximum dot product.

use crate::hull::{
    create_hull, find_hull_support_vertex, get_hull_points, hull_soa_vertex_count, make_box_hull,
    HullData,
};
use crate::math_functions::{dot, sub, Vec3};

fn v(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

/// The lowest index holding the maximum dot product (the behavior of the pre-SIMD scalar scan).
fn first_max_index(hull: &HullData, direction: Vec3) -> i32 {
    let mut best_index = -1;
    let mut best_dot = -f32::MAX;
    for (index, point) in get_hull_points(hull).iter().enumerate() {
        let d = dot(direction, *point);
        if d > best_dot {
            best_dot = d;
            best_index = index as i32;
        }
    }
    best_index
}

// A square pyramid has 5 vertices, so the SoA stream is padded to 8 with three copies of
// vertex 0 that land in lanes 1, 2 and 3 of the second block. When vertex 0 is the unique
// support vertex those padded lanes tie it and the reduction must still return index 0.
#[test]
fn support_vertex_padded_lanes_tie_to_vertex_zero() {
    let points = [
        v(1.0, 0.0, 1.0),
        v(-1.0, 0.0, 1.0),
        v(-1.0, 0.0, -1.0),
        v(1.0, 0.0, -1.0),
        v(0.0, 1.5, 0.0),
    ];
    let hull = create_hull(&points, 128).expect("pyramid hull");
    assert_eq!(hull.vertex_count, 5);
    assert_eq!(hull_soa_vertex_count(&hull), 8);

    let hull_points = get_hull_points(&hull);
    let mut centroid = v(0.0, 0.0, 0.0);
    for p in hull_points {
        centroid.x += 0.2 * p.x;
        centroid.y += 0.2 * p.y;
        centroid.z += 0.2 * p.z;
    }

    // Direction from the centroid toward vertex 0 makes vertex 0 the unique maximum.
    let direction = sub(hull_points[0], centroid);
    assert_eq!(first_max_index(&hull, direction), 0);
    assert_eq!(find_hull_support_vertex(&hull, direction), 0);

    // Every vertex direction agrees with the scan and stays inside the real vertex range.
    for k in 0..hull.vertex_count as usize {
        let direction = sub(hull_points[k], centroid);
        let index = find_hull_support_vertex(&hull, direction);
        assert!(0 <= index && index < hull.vertex_count);
        assert_eq!(index, first_max_index(&hull, direction));
    }

    // A zero direction ties every vertex and every padded lane: the answer is vertex 0.
    assert_eq!(find_hull_support_vertex(&hull, v(0.0, 0.0, 0.0)), 0);
}

// Box faces put four vertices on each support plane, spread across both SoA blocks and
// several lanes. The reduction must pick the lowest index among the tied vertices.
#[test]
fn support_vertex_ties_pick_lowest_index() {
    let box_hull = make_box_hull(0.5, 0.25, 1.0);
    let hull = &box_hull.base;

    let directions = [
        v(1.0, 0.0, 0.0),
        v(-1.0, 0.0, 0.0),
        v(0.0, 1.0, 0.0),
        v(0.0, -1.0, 0.0),
        v(0.0, 0.0, 1.0),
        v(0.0, 0.0, -1.0),
        v(1.0, 1.0, 0.0),
        v(0.0, -1.0, 1.0),
        v(-1.0, 0.0, 1.0),
        v(0.0, 0.0, 0.0),
    ];

    for direction in directions {
        let index = find_hull_support_vertex(hull, direction);
        assert!(0 <= index && index < hull.vertex_count);
        assert_eq!(index, first_max_index(hull, direction));
    }
}
