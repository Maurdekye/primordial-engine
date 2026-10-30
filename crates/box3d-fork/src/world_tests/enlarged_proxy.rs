//! Broad-phase tree maintenance tests from test_world.c (TestEnlargedProxyDestroyed,
//! TestCompoundShapeCount).

use crate::body::{create_body, destroy_body};
use crate::compound::{create_compound, destroy_compound, CompoundDef, CompoundHullDef};
use crate::dynamic_tree::DynamicTree;
use crate::geometry::{default_surface_material, Sphere};
use crate::hull::make_box_hull;
use crate::math_functions::{Pos, Transform, Vec3, QUAT_IDENTITY, VEC3_ZERO};
use crate::shape::{create_baked_compound_shape, create_sphere_shape, destroy_shape};
use crate::types::{default_body_def, default_shape_def, default_world_def, BodyType};
use crate::world::World;

/// (CountMovedNodes)
fn count_moved_nodes(tree: &DynamicTree) -> i32 {
    let mut count = 0;
    for i in 0..tree.node_end() {
        let node = &tree.nodes[i as usize];
        if !node.is_empty() && node.is_moved() {
            count += 1;
        }
    }

    count
}

/// A proxy can be enlarged in one step and destroyed before the next, emptying the move buffer.
/// The trees must still be rebuilt or the enlarged nodes survive into the next step. Issue #149.
/// (TestEnlargedProxyDestroyed)
#[test]
fn test_enlarged_proxy_destroyed() {
    let mut world_def = default_world_def();
    world_def.gravity = VEC3_ZERO;
    let mut world = World::new(&world_def);

    let dynamic_tree = BodyType::Dynamic as usize;

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Dynamic;

    let shape_def = default_shape_def();
    let sphere = Sphere {
        center: VEC3_ZERO,
        radius: 0.25,
    };

    // Resting bodies keep the tree deep enough that the mover has an ancestor that outlives it
    for i in 0..8 {
        body_def.position = Pos {
            x: (2.0 * i as f32) as _,
            y: 0.0,
            z: 0.0,
        };
        let resting_id = create_body(&mut world, &body_def);
        create_sphere_shape(&mut world, resting_id, &shape_def, &sphere);
    }

    // This body outruns its AABB margin every step
    body_def.position = Pos {
        x: 7.0,
        y: 1.0,
        z: 0.0,
    };
    body_def.linear_velocity = Vec3::new(6.0, 0.0, 0.0);
    let mover_id = create_body(&mut world, &body_def);
    create_sphere_shape(&mut world, mover_id, &shape_def, &sphere);

    let time_step = 1.0 / 60.0;
    world.step(time_step, 4);

    assert!(count_moved_nodes(&world.broad_phase.trees[dynamic_tree]) > 0);

    destroy_body(&mut world, mover_id);

    // Removing the leaf re-derives the marks along its path, so the tree is already consistent
    // here and nothing is left marked to drive a rebuild. What the destroy can still leave
    // stale is the DFS order the sweep refit needs, so the step has to rebuild regardless.
    world.broad_phase.trees[dynamic_tree].validate();
    assert_eq!(count_moved_nodes(&world.broad_phase.trees[dynamic_tree]), 0);

    world.step(time_step, 4);

    assert_eq!(count_moved_nodes(&world.broad_phase.trees[dynamic_tree]), 0);
    assert!(world.broad_phase.trees[dynamic_tree].dfs_ordered());
}

/// b3DestroyBody tears shapes down inline instead of going through the shape destroy path, so
/// it needs its own compound census decrement. A leaked count pins the broad-phase on the slow
/// per-candidate compound path for the life of the world and rides along into snapshots.
/// (TestCompoundShapeCount)
#[test]
fn test_compound_shape_count() {
    let world_def = default_world_def();
    let mut world = World::new(&world_def);

    assert_eq!(world.compound_shape_count, 0);

    let box_hull = make_box_hull(1.0, 1.0, 1.0);

    let hull = CompoundHullDef {
        hull: &box_hull.base,
        transform: Transform {
            p: VEC3_ZERO,
            q: QUAT_IDENTITY,
        },
        material: default_surface_material(),
    };

    let hulls = [hull];
    let compound_def = CompoundDef {
        hulls: &hulls,
        ..CompoundDef::default()
    };
    let compound = create_compound(&compound_def).expect("compound");

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Static;
    let shape_def = default_shape_def();

    // Shape destroyed on its own
    let body_id = create_body(&mut world, &body_def);
    let shape_id = create_baked_compound_shape(&mut world, body_id, &shape_def, &compound);
    assert_eq!(world.compound_shape_count, 1);
    destroy_shape(&mut world, shape_id, true);
    assert_eq!(world.compound_shape_count, 0);
    destroy_body(&mut world, body_id);

    // Shape carried away by its body
    let body_id = create_body(&mut world, &body_def);
    create_baked_compound_shape(&mut world, body_id, &shape_def, &compound);
    assert_eq!(world.compound_shape_count, 1);
    destroy_body(&mut world, body_id);
    assert_eq!(world.compound_shape_count, 0);

    destroy_compound(compound);
}
