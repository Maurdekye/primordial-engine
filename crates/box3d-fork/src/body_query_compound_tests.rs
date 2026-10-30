//! Compound body-level overlap tests from test_body_query.c (OverlapCompoundRotatedBody).
//! Split from body_query_tests.rs to keep files under the length limit.
//!
//! SPDX-FileCopyrightText: 2025 Erin Catto
//! SPDX-License-Identifier: MIT

use crate::body::{body_overlap_shape, create_body};
use crate::compound::{create_compound, destroy_compound, CompoundDef, CompoundSphereDef};
use crate::distance::make_proxy;
use crate::geometry::{default_surface_material, Sphere};
use crate::math_functions::{
    make_quat_from_axis_angle, transform_point, Pos, Transform, Vec3, WorldTransform, PI, POS_ZERO,
    VEC3_AXIS_Z,
};
use crate::shape::create_baked_compound_shape;
use crate::types::{
    default_body_def, default_query_filter, default_shape_def, default_world_def, BodyType,
};
use crate::world::{world_overlap_shape, World};

/// A baked compound keeps its tree and its child placements in the compound frame while the query
/// proxy is in world space, so a compound on a turned body only answers correctly if the proxy is
/// brought back into that frame first. (OverlapCompoundRotatedBody)
#[test]
fn overlap_compound_rotated_body() {
    let mut world = World::new(&default_world_def());

    let mat = default_surface_material();
    let spheres = [
        CompoundSphereDef {
            sphere: Sphere {
                center: Vec3::new(-3.0, 0.0, 0.0),
                radius: 0.5,
            },
            material: mat,
        },
        CompoundSphereDef {
            sphere: Sphere {
                center: Vec3::new(3.0, 0.0, 0.0),
                radius: 0.5,
            },
            material: mat,
        },
    ];
    let compound_def = CompoundDef {
        spheres: &spheres,
        ..Default::default()
    };
    let compound = create_compound(&compound_def);
    assert!(compound.is_some());
    let compound = compound.unwrap();

    let pose = Transform {
        p: Vec3::new(10.0, 20.0, 30.0),
        q: make_quat_from_axis_angle(VEC3_AXIS_Z, 0.5 * PI),
    };

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Static;
    body_def.position = Pos {
        x: pose.p.x as _,
        y: pose.p.y as _,
        z: pose.p.z as _,
    };
    body_def.rotation = pose.q;
    let body_id = create_body(&mut world, &body_def);

    let shape_def = default_shape_def();
    create_baked_compound_shape(&mut world, body_id, &shape_def, &compound);

    let filter = default_query_filter();

    // Sitting on the second child, which the body pose has swung onto the +Y side
    let on_child = transform_point(pose, Vec3::new(3.0, 0.0, 0.0));
    let hit_proxy = make_proxy(&[on_child], 0.1);

    let mut hits = 0;
    world_overlap_shape(&world, POS_ZERO, &hit_proxy, &filter, |_| {
        hits += 1;
        true
    });
    assert_eq!(hits, 1);

    let body_transform = WorldTransform {
        p: body_def.position,
        q: body_def.rotation,
    };
    assert!(body_overlap_shape(
        &world,
        body_id,
        POS_ZERO,
        &hit_proxy,
        &filter,
        body_transform
    ));

    // The gap between the two children stays empty
    let in_gap = transform_point(pose, Vec3::new(0.0, 0.0, 0.0));
    let gap_proxy = make_proxy(&[in_gap], 0.25);

    hits = 0;
    world_overlap_shape(&world, POS_ZERO, &gap_proxy, &filter, |_| {
        hits += 1;
        true
    });
    assert_eq!(hits, 0);
    assert!(!body_overlap_shape(
        &world,
        body_id,
        POS_ZERO,
        &gap_proxy,
        &filter,
        body_transform
    ));

    destroy_compound(compound);
}
