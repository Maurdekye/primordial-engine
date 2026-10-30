//! Mover material, triangle, and one sided collision tests from test_mover.c.
//!
//! Split from mover_tests.rs to satisfy the file-length limit.
//!
//! SPDX-FileCopyrightText: 2025 Erin Catto
//! SPDX-License-Identifier: MIT

use crate::body::{body_collide_mover, create_body, BodyPlaneResult};
use crate::compound::{
    create_compound, destroy_compound, get_compound_materials, CompoundDef, CompoundHullDef,
    CompoundMeshDef,
};
use crate::geometry::{default_surface_material, Capsule, PlaneResult, Sphere, SurfaceMaterial};
use crate::height_field::{
    collide_mover_and_height_field, create_height_field, destroy_height_field, HeightFieldData,
    HeightFieldDef,
};
use crate::hull::{make_box_hull, make_transformed_box_hull};
use crate::math_functions::WorldTransform;
use crate::math_functions::{
    Transform, Vec3, POS_ZERO, QUAT_IDENTITY, TRANSFORM_IDENTITY, VEC3_ZERO,
};
use crate::mesh::{
    collide_mover_and_mesh, create_mesh, destroy_mesh, get_mesh_material_indices, Mesh, MeshData,
    MeshDef,
};
use crate::shape::{
    create_baked_compound_shape, create_capsule_shape, create_hull_shape, create_mesh_shape,
    create_sphere_shape, shape_get_mesh_material_count, shape_is_valid,
};
use crate::types::{
    default_body_def, default_query_filter, default_shape_def, default_world_def, BodyType,
};
use crate::world::{world_collide_mover, World};

fn ensure_small(value: f32, tolerance: f32) {
    assert!(
        !(value < -tolerance || tolerance < value),
        "|{value}| > tolerance {tolerance}"
    );
}

fn v(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

fn capsule(c1: Vec3, c2: Vec3, radius: f32) -> Capsule {
    Capsule {
        center1: c1,
        center2: c2,
        radius,
    }
}

// Mover queries report which material a contact plane came from
// b3PlaneResult::materialIndex follows different paths per shape type. Meshes
// report the per triangle material index. Compounds remap the child result
// through the child material table. Convex shapes report index 0.
fn make_material(friction: f32, user_id: u64) -> SurfaceMaterial {
    let mut m = default_surface_material();
    m.friction = friction;
    m.user_material_id = user_id;
    m
}

// C zero-initializes the compound hull def, so its material is all zeros.
fn zero_material() -> SurfaceMaterial {
    SurfaceMaterial {
        friction: 0.0,
        restitution: 0.0,
        rolling_resistance: 0.0,
        tangent_velocity: VEC3_ZERO,
        user_material_id: 0,
        custom_color: 0,
    }
}

// Two separated upward facing triangles on the y = 0 plane, one per material. The bake may
// reorder triangles but always keeps a triangle paired with its material index.
fn make_two_material_mesh() -> Option<MeshData> {
    let vertices = vec![
        v(-3.0, 0.0, -1.0),
        v(-2.0, 0.0, 1.0),
        v(-1.0, 0.0, -1.0),
        v(1.0, 0.0, -1.0),
        v(2.0, 0.0, 1.0),
        v(3.0, 0.0, -1.0),
    ];
    let indices = vec![0, 1, 2, 3, 4, 5];
    let material_indices = vec![0u8, 1];

    let def = MeshDef {
        vertices,
        stride: core::mem::size_of::<Vec3>(),
        indices,
        material_indices,
        ..Default::default()
    };

    create_mesh(&def, None)
}

// Flat 3x3 vertex field at y = 0, one cell material per entry. Caller destroys.
fn make_flat_field(materials: &[u8; 4], clockwise: bool) -> HeightFieldData {
    let heights = [0.0f32; 9];

    let def = HeightFieldDef {
        heights: heights.to_vec(),
        material_indices: materials.to_vec(),
        scale: v(1.0, 1.0, 1.0),
        count_x: 3,
        count_z: 3,
        global_minimum_height: -1.0,
        global_maximum_height: 1.0,
        clockwise_winding: clockwise,
    };

    create_height_field(&def)
}

// Collects up to 16 planes, like C's PlaneCapture / CapturePlaneFcn.
fn capture_planes(world: &World, mover: &Capsule) -> Vec<PlaneResult> {
    let mut captured = Vec::new();
    world_collide_mover(
        world,
        POS_ZERO,
        mover,
        &default_query_filter(),
        |_shape_id, planes| {
            for plane in planes {
                if captured.len() < 16 {
                    captured.push(*plane);
                }
            }
            true
        },
    );
    captured
}

fn identity_transform() -> WorldTransform {
    WorldTransform {
        p: POS_ZERO,
        q: QUAT_IDENTITY,
    }
}

#[test]
fn mover_world_mesh_materials() {
    let mut world = World::new(&default_world_def());

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Static;
    let body_id = create_body(&mut world, &body_def);

    let mesh = make_two_material_mesh().expect("mesh");
    assert_eq!(mesh.material_count, 2);

    let materials = vec![make_material(0.2, 1), make_material(0.8, 2)];
    let mut shape_def = default_shape_def();
    shape_def.materials = materials;
    create_mesh_shape(&mut world, body_id, &shape_def, &mesh, v(1.0, 1.0, 1.0));

    world.step(1.0 / 60.0, 1);

    let baked_material_indices = get_mesh_material_indices(&mesh);

    // Mover hanging just above the first triangle so its radius reaches 0.05 into the surface
    let mover = capsule(v(-2.0, 0.15, 0.0), v(-2.0, 0.35, 0.0), 0.2);

    let capture = capture_planes(&world, &mover);

    assert_eq!(capture.len(), 1);
    assert!(capture[0].plane.normal.y > 0.99);
    ensure_small(capture[0].plane.offset - 0.05, 1e-4);
    ensure_small(capture[0].point.y, 1e-4);
    assert_eq!(capture[0].material_index, 0);
    assert!(capture[0].triangle_index >= 0);
    assert!(capture[0].triangle_index < mesh.triangle_count);
    assert_eq!(
        baked_material_indices[capture[0].triangle_index as usize],
        0
    );

    // Same mover over the second triangle
    let mover2 = capsule(v(2.0, 0.15, 0.0), v(2.0, 0.35, 0.0), 0.2);

    let capture2 = capture_planes(&world, &mover2);

    assert_eq!(capture2.len(), 1);
    assert!(capture2[0].plane.normal.y > 0.99);
    assert_eq!(capture2[0].material_index, 1);
    assert_eq!(
        baked_material_indices[capture2[0].triangle_index as usize],
        1
    );

    drop(world);
    destroy_mesh(mesh);
}

#[test]
fn mover_world_compound_mesh_materials() {
    let mut world = World::new(&default_world_def());

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Static;
    let body_id = create_body(&mut world, &body_def);

    let mesh = make_two_material_mesh().expect("mesh");

    // The hull material and the two mesh materials are distinct, so the compound material
    // table gets one slot per material. Children bake in hull then mesh order.
    let mesh_materials = [make_material(0.3, 101), make_material(0.6, 202)];
    let hull_material = make_material(0.9, 303);

    let mesh_child = [CompoundMeshDef {
        mesh_data: &mesh,
        transform: TRANSFORM_IDENTITY,
        scale: v(1.0, 1.0, 1.0),
        materials: &mesh_materials,
    }];

    let box_hull = make_box_hull(0.5, 0.5, 0.5);
    let hull_child = [CompoundHullDef {
        hull: &box_hull.base,
        transform: Transform {
            p: v(-8.0, 0.0, 0.0),
            q: QUAT_IDENTITY,
        },
        material: hull_material,
    }];

    let compound_def = CompoundDef {
        hulls: &hull_child,
        meshes: &mesh_child,
        ..Default::default()
    };
    let compound = create_compound(&compound_def).expect("compound");
    assert_eq!(compound.material_count, 3);

    let baked_materials = get_compound_materials(&compound);
    assert_eq!(baked_materials[0].user_material_id, 303);
    assert_eq!(baked_materials[1].user_material_id, 101);
    assert_eq!(baked_materials[2].user_material_id, 202);

    let shape_def = default_shape_def();
    create_baked_compound_shape(&mut world, body_id, &shape_def, &compound);

    world.step(1.0 / 60.0, 1);

    // Mover on top of the hull child face at y = 0.5
    let hull_mover = capsule(v(-8.0, 0.65, 0.0), v(-8.0, 0.85, 0.0), 0.2);

    let hull_capture = capture_planes(&world, &hull_mover);

    assert_eq!(hull_capture.len(), 1);
    assert!(hull_capture[0].plane.normal.y > 0.99);
    assert_eq!(hull_capture[0].child_index, 0);
    assert_eq!(hull_capture[0].material_index, 0);
    assert_eq!(
        baked_materials[hull_capture[0].material_index as usize].user_material_id,
        303
    );

    // Mover over the second mesh triangle. The mesh reports triangle material 1, which the
    // compound remaps through the child material table to the shared slot of meshMaterials[1].
    let mesh_mover = capsule(v(2.0, 0.15, 0.0), v(2.0, 0.35, 0.0), 0.2);

    let mesh_capture = capture_planes(&world, &mesh_mover);

    assert_eq!(mesh_capture.len(), 1);
    assert!(mesh_capture[0].plane.normal.y > 0.99);
    assert_eq!(mesh_capture[0].child_index, 1);
    assert_eq!(mesh_capture[0].material_index, 2);
    assert_eq!(
        baked_materials[mesh_capture[0].material_index as usize].user_material_id,
        202
    );

    drop(world);
    destroy_compound(compound);
    destroy_mesh(mesh);
}

#[test]
fn mover_body_material_indices() {
    let mut world = World::new(&default_world_def());

    let body_id = create_body(&mut world, &default_body_def());

    // One shape of each convex type, spaced out along X
    let shape_def = default_shape_def();

    let sphere = Sphere {
        center: v(0.0, 0.5, 0.0),
        radius: 0.5,
    };
    create_sphere_shape(&mut world, body_id, &shape_def, &sphere);

    let box_hull = make_transformed_box_hull(
        0.5,
        0.5,
        0.5,
        Transform {
            p: v(5.0, 0.0, 0.0),
            q: QUAT_IDENTITY,
        },
    );
    create_hull_shape(&mut world, body_id, &shape_def, &box_hull.base);

    let body_capsule = capsule(v(9.0, 0.0, 0.0), v(11.0, 0.0, 0.0), 0.3);
    create_capsule_shape(&mut world, body_id, &shape_def, &body_capsule);

    let body_transform = identity_transform();

    // Convex shapes report the base material as index 0
    let movers = [
        capsule(v(0.0, 1.15, 0.0), v(0.0, 1.35, 0.0), 0.2),
        capsule(v(5.0, 0.65, 0.0), v(5.0, 0.85, 0.0), 0.2),
        capsule(v(10.0, 0.45, 0.0), v(10.0, 0.65, 0.0), 0.2),
    ];

    for mover in &movers {
        let mut planes = [BodyPlaneResult::default(); 4];
        let count = body_collide_mover(
            &world,
            body_id,
            &mut planes,
            POS_ZERO,
            mover,
            &default_query_filter(),
            body_transform,
        );

        assert_eq!(count, 1);
        assert!(shape_is_valid(&world, planes[0].shape_id));
        assert!(planes[0].result.plane.normal.y > 0.99);
        assert_eq!(planes[0].result.material_index, 0);
        assert_eq!(planes[0].result.child_index, 0);
    }
}

#[test]
fn mover_body_skips_mesh_and_compound() {
    // The body level mover query handles convex shapes only. Mesh and compound shapes are
    // skipped, so their material indices are only available through b3World_CollideMover.
    let mut world = World::new(&default_world_def());

    let body_id = create_body(&mut world, &default_body_def());

    let shape_def = default_shape_def();

    let mesh = make_two_material_mesh().expect("mesh");
    create_mesh_shape(&mut world, body_id, &shape_def, &mesh, v(1.0, 1.0, 1.0));

    let box_hull = make_box_hull(0.5, 0.5, 0.5);
    let hull_child = [CompoundHullDef {
        hull: &box_hull.base,
        transform: Transform {
            p: v(10.0, 0.0, 0.0),
            q: QUAT_IDENTITY,
        },
        material: zero_material(),
    }];
    let compound_def = CompoundDef {
        hulls: &hull_child,
        ..Default::default()
    };
    let compound = create_compound(&compound_def).expect("compound");
    create_baked_compound_shape(&mut world, body_id, &shape_def, &compound);

    let body_transform = identity_transform();

    // Mover over the mesh triangle and mover over the compound hull child both find nothing
    let mesh_mover = capsule(v(-2.0, 0.15, 0.0), v(-2.0, 0.35, 0.0), 0.2);
    let compound_mover = capsule(v(10.0, 0.65, 0.0), v(10.0, 0.85, 0.0), 0.2);

    let movers = [mesh_mover, compound_mover];
    for mover in &movers {
        let mut planes = [BodyPlaneResult::default(); 4];
        let count = body_collide_mover(
            &world,
            body_id,
            &mut planes,
            POS_ZERO,
            mover,
            &default_query_filter(),
            body_transform,
        );
        assert_eq!(count, 0);
    }

    drop(world);
    destroy_compound(compound);
    destroy_mesh(mesh);
}

// One sided mover collision.
// Mover queries keep only triangles facing the mover. The front side follows the
// baked winding: up for a default mesh or height field, down when the height
// field carries clockwiseWinding. A mirror in the mesh scale must not flip it.
#[test]
fn mover_mesh_backside() {
    let mesh = make_two_material_mesh().expect("mesh");
    let shape = Mesh::new(&mesh, v(1.0, 1.0, 1.0));

    let mut planes = [PlaneResult::default(); 4];

    // On the front of the left triangle, so a plane comes back
    let above = capsule(v(-2.0, 0.15, 0.0), v(-2.0, 0.35, 0.0), 0.2);
    let count = collide_mover_and_mesh(&mut planes, &shape, &above);
    assert_eq!(count, 1);
    assert!(planes[0].plane.normal.y > 0.99);
    assert_eq!(planes[0].child_index, 0);
    assert!(planes[0].triangle_index >= 0 && planes[0].triangle_index < mesh.triangle_count);

    // The same spot seen from behind the face is culled
    let below = capsule(v(-2.0, -0.35, 0.0), v(-2.0, -0.15, 0.0), 0.2);
    let count = collide_mover_and_mesh(&mut planes, &shape, &below);
    assert_eq!(count, 0);

    destroy_mesh(mesh);
}

#[test]
fn mover_mesh_mirrored_scale() {
    let mesh = make_two_material_mesh().expect("mesh");

    // Reflecting the scale flips the triangle winding, the collision swaps it back,
    // so the front stays up. Local x = -2 maps onto the source right triangle, material 1.
    let shape = Mesh::new(&mesh, v(-1.0, 1.0, 1.0));

    let mut planes = [PlaneResult::default(); 4];

    let above = capsule(v(-2.0, 0.15, 0.0), v(-2.0, 0.35, 0.0), 0.2);
    let count = collide_mover_and_mesh(&mut planes, &shape, &above);
    assert_eq!(count, 1);
    assert!(planes[0].plane.normal.y > 0.99);
    assert_eq!(planes[0].material_index, 1);
    assert_eq!(
        get_mesh_material_indices(&mesh)[planes[0].triangle_index as usize],
        1
    );

    let below = capsule(v(-2.0, -0.35, 0.0), v(-2.0, -0.15, 0.0), 0.2);
    let count = collide_mover_and_mesh(&mut planes, &shape, &below);
    assert_eq!(count, 0);

    destroy_mesh(mesh);
}

#[test]
fn mover_height_field_backside() {
    let materials = [0u8, 0, 0, 0];
    let hf = make_flat_field(&materials, false);

    let mut planes = [PlaneResult::default(); 4];

    // Standing on the front (upper) face of the default winding
    let above = capsule(v(0.3, 0.15, 0.25), v(0.3, 0.35, 0.25), 0.2);
    let count = collide_mover_and_height_field(&mut planes, &hf, &above);
    assert_eq!(count, 1);
    assert!(planes[0].plane.normal.y > 0.99);
    ensure_small(planes[0].plane.offset - 0.05, 1e-4);

    // Under the surface is the back side and gets culled
    let below = capsule(v(0.3, -0.35, 0.25), v(0.3, -0.15, 0.25), 0.2);
    let count = collide_mover_and_height_field(&mut planes, &hf, &below);
    assert_eq!(count, 0);

    destroy_height_field(hf);
}

#[test]
fn mover_height_field_report() {
    let materials = [1u8, 2, 0, 0];
    let hf = make_flat_field(&materials, false);

    let mut planes = [PlaneResult::default(); 4];

    // (0.3, 0.25) sits on the x + z <= 1 side of cell (0,0), which holds triangle 0
    let first = capsule(v(0.3, 0.15, 0.25), v(0.3, 0.35, 0.25), 0.2);
    let count = collide_mover_and_height_field(&mut planes, &hf, &first);
    assert_eq!(count, 1);
    assert_eq!(planes[0].triangle_index, 0);
    assert_eq!(planes[0].child_index, 0);
    assert_eq!(planes[0].material_index, 1);

    // (1.3, 0.3) sits on the x + z <= 2 side of cell (0,1), which holds triangle 2
    let second = capsule(v(1.3, 0.15, 0.3), v(1.3, 0.35, 0.3), 0.2);
    let count = collide_mover_and_height_field(&mut planes, &hf, &second);
    assert_eq!(count, 1);
    assert_eq!(planes[0].triangle_index, 2);
    assert_eq!(planes[0].material_index, 2);

    destroy_height_field(hf);
}

// A clockwise height field faces down, see HeightFieldWinding. Backside culling
// must respect the flag: below the surface is the front side, above is the back.
#[test]
fn mover_height_field_clockwise() {
    let materials = [0u8, 0, 0, 0];
    let hf = make_flat_field(&materials, true);

    let mut planes = [PlaneResult::default(); 4];

    let below = capsule(v(0.3, -0.35, 0.25), v(0.3, -0.15, 0.25), 0.2);
    let count = collide_mover_and_height_field(&mut planes, &hf, &below);
    assert_eq!(count, 1);
    assert!(planes[0].plane.normal.y < -0.99);
    ensure_small(planes[0].plane.offset - 0.05, 1e-4);
    assert!(planes[0].triangle_index == 0 || planes[0].triangle_index == 1);

    let above = capsule(v(0.3, 0.15, 0.25), v(0.3, 0.35, 0.25), 0.2);
    let count = collide_mover_and_height_field(&mut planes, &hf, &above);
    assert_eq!(count, 0);

    destroy_height_field(hf);
}

// A mesh can be baked with more materials than the shape carries. The reported index must stay
// inside the shape's material array.
#[test]
fn mover_world_mesh_material_clamp() {
    let mut world = World::new(&default_world_def());

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Static;
    let body_id = create_body(&mut world, &body_def);

    let mesh = make_two_material_mesh().expect("mesh");
    assert_eq!(mesh.material_count, 2);

    // Base material only
    let shape_def = default_shape_def();
    let shape_id = create_mesh_shape(&mut world, body_id, &shape_def, &mesh, v(1.0, 1.0, 1.0));
    let shape_material_count = shape_get_mesh_material_count(&world, shape_id);
    assert_eq!(shape_material_count, 1);

    world.step(1.0 / 60.0, 1);

    // Over the triangle baked with material 1
    let mover = capsule(v(2.0, 0.15, 0.0), v(2.0, 0.35, 0.0), 0.2);
    let capture = capture_planes(&world, &mover);

    assert_eq!(capture.len(), 1);
    assert_eq!(capture[0].material_index, shape_material_count - 1);

    drop(world);
    destroy_mesh(mesh);
}
