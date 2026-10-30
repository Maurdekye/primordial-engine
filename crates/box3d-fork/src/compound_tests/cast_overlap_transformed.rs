//! Compound shape cast, material remap, transformed overlap, and rotated-child mover tests
//! (CompoundShapeCastMiss, CompoundShapeCastHullNormalRotation, CompoundMeshMaterialRemap,
//! CompoundOverlapTransformed, CompoundOverlapChildTypes, CompoundOverlapSegmentProxy,
//! CompoundMoverRotatedChild).

use super::{make_material, v};
use crate::compound::{
    collide_mover_and_compound, create_compound, destroy_compound, get_compound_materials,
    overlap_compound, ray_cast_compound, shape_cast_compound, CompoundCapsuleDef, CompoundDef,
    CompoundHullDef, CompoundMeshDef, CompoundSphereDef,
};
use crate::distance::make_proxy;
use crate::geometry::{
    default_surface_material, Capsule, PlaneResult, RayCastInput, ShapeCastInput, Sphere,
};
use crate::hull::make_box_hull;
use crate::math_functions::{
    make_quat_from_axis_angle, transform_point, Transform, Vec3, PI, TRANSFORM_IDENTITY,
    VEC3_AXIS_Y, VEC3_AXIS_Z, VEC3_ZERO,
};
use crate::mesh::{create_box_mesh, create_mesh, destroy_mesh, MeshData, MeshDef};

/// Matches the C ENSURE_SMALL macro, which is inclusive: pass when `-tol <= value <= tol`.
fn ensure_small(value: f32, tolerance: f32) {
    assert!(
        !(value < -tolerance || tolerance < value),
        "value {value} not within {tolerance}"
    );
}

/// (CompoundShapeCastMiss)
#[test]
fn compound_shape_cast_miss() {
    let mat = default_surface_material();
    let sph = CompoundSphereDef {
        sphere: Sphere {
            center: v(5.0, 0.0, 0.0),
            radius: 1.0,
        },
        material: mat,
    };
    let c = create_compound(&CompoundDef {
        spheres: &[sph],
        ..Default::default()
    })
    .unwrap();

    let mut input = ShapeCastInput {
        proxy: make_proxy(&[v(0.0, 5.0, 0.0)], 0.25),
        translation: v(20.0, 0.0, 0.0),
        max_fraction: 1.0,
        can_encroach: false,
    };
    assert!(!shape_cast_compound(&c, &input).hit);

    // An empty proxy has nothing to sweep
    input.proxy.count = 0;
    assert!(!shape_cast_compound(&c, &input).hit);

    destroy_compound(c);
}

/// (CompoundShapeCastHullNormalRotation)
#[test]
fn compound_shape_cast_hull_normal_rotation() {
    // Same setup as the ray cast rotation test. The result comes back in compound space, so the
    // hull local face the sweep lands on says nothing about the reported normal and point.
    let mat = default_surface_material();
    let box_hull = make_box_hull(1.0, 1.0, 1.0);

    let hull = CompoundHullDef {
        hull: &box_hull.base,
        transform: Transform {
            p: v(5.0, 0.0, 0.0),
            q: make_quat_from_axis_angle(VEC3_AXIS_Z, 0.5 * PI),
        },
        material: mat,
    };
    let c = create_compound(&CompoundDef {
        hulls: &[hull],
        ..Default::default()
    })
    .unwrap();

    let input = ShapeCastInput {
        proxy: make_proxy(&[VEC3_ZERO], 0.25),
        translation: v(20.0, 0.0, 0.0),
        max_fraction: 1.0,
        can_encroach: false,
    };
    let out = shape_cast_compound(&c, &input);
    assert!(out.hit);

    // Near face at x=4, caster radius 0.25 so contact at x=3.75
    ensure_small(out.fraction - 3.75 / 20.0, 1e-3);
    ensure_small(out.normal.x + 1.0, 1e-3);
    ensure_small(out.normal.y, 1e-3);
    ensure_small(out.normal.z, 1e-3);
    ensure_small(out.point.x - 4.0, 1e-3);
    ensure_small(out.point.y, 1e-3);
    ensure_small(out.point.z, 1e-3);
    assert_eq!(out.child_index, 0);

    destroy_compound(c);
}

/// Two separated upward facing triangles on the y = 0 plane, one per material. The bake may
/// reorder triangles but always keeps a triangle paired with its material index.
/// (MakeTwoMaterialMesh)
fn make_two_material_mesh() -> Option<MeshData> {
    let def = MeshDef {
        vertices: vec![
            v(-3.0, 0.0, -1.0),
            v(-2.0, 0.0, 1.0),
            v(-1.0, 0.0, -1.0),
            v(1.0, 0.0, -1.0),
            v(2.0, 0.0, 1.0),
            v(3.0, 0.0, -1.0),
        ],
        stride: std::mem::size_of::<Vec3>(),
        indices: vec![0, 1, 2, 3, 4, 5],
        material_indices: vec![0, 1],
        ..Default::default()
    };

    create_mesh(&def, None)
}

/// (CompoundMeshMaterialRemap)
#[test]
fn compound_mesh_material_remap() {
    // A mesh child carries its own small material table. Casts report the triangle material
    // after it has been remapped into the compound material array.
    let mat_a = make_material(0.3, 100);
    let mat_b = make_material(0.7, 200);
    let materials = [mat_a, mat_b];

    let md = make_two_material_mesh().unwrap();
    let mesh = CompoundMeshDef {
        mesh_data: &md,
        transform: TRANSFORM_IDENTITY,
        scale: v(1.0, 1.0, 1.0),
        materials: &materials,
    };
    let c = create_compound(&CompoundDef {
        meshes: &[mesh],
        ..Default::default()
    })
    .unwrap();

    let table = get_compound_materials(&c);

    // Straight down onto the material 0 triangle
    let ray_a = RayCastInput {
        origin: v(-2.0, 5.0, 0.0),
        translation: v(0.0, -10.0, 0.0),
        max_fraction: 1.0,
    };
    let out_a = ray_cast_compound(&c, &ray_a);
    assert!(out_a.hit);
    assert_eq!(table[out_a.material_index as usize].user_material_id, 100);

    let ray_b = RayCastInput {
        origin: v(2.0, 5.0, 0.0),
        translation: v(0.0, -10.0, 0.0),
        max_fraction: 1.0,
    };
    let out_b = ray_cast_compound(&c, &ray_b);
    assert!(out_b.hit);
    assert_eq!(table[out_b.material_index as usize].user_material_id, 200);

    // The shape cast path does the same remap
    let cast_input = ShapeCastInput {
        proxy: make_proxy(&[v(2.0, 5.0, 0.0)], 0.1),
        translation: v(0.0, -10.0, 0.0),
        max_fraction: 1.0,
        can_encroach: false,
    };
    let out_cast = shape_cast_compound(&c, &cast_input);
    assert!(out_cast.hit);
    assert_eq!(
        table[out_cast.material_index as usize].user_material_id,
        200
    );

    destroy_compound(c);
    destroy_mesh(md);
}

fn two_sphere_compound_def(mat: crate::geometry::SurfaceMaterial) -> [CompoundSphereDef; 2] {
    [
        CompoundSphereDef {
            sphere: Sphere {
                center: v(-3.0, 0.0, 0.0),
                radius: 0.5,
            },
            material: mat,
        },
        CompoundSphereDef {
            sphere: Sphere {
                center: v(3.0, 0.0, 0.0),
                radius: 0.5,
            },
            material: mat,
        },
    ]
}

/// (CompoundOverlapTransformed)
#[test]
fn compound_overlap_transformed() {
    // The tree and the child geometry live in the compound frame while the proxy arrives in world
    // space, so a compound that is moved and turned has to pull the proxy back before querying.
    let mat = default_surface_material();
    let spheres = two_sphere_compound_def(mat);
    let c = create_compound(&CompoundDef {
        spheres: &spheres,
        ..Default::default()
    })
    .unwrap();

    let transform = Transform {
        p: v(10.0, 20.0, 30.0),
        q: make_quat_from_axis_angle(VEC3_AXIS_Z, 0.5 * PI),
    };

    let gap_point = transform_point(transform, v(0.0, 0.0, 0.0));
    let gap = make_proxy(&[gap_point], 0.25);
    assert!(!overlap_compound(&c, transform, &gap));

    let hit_point = transform_point(transform, v(3.0, 0.0, 0.0));
    let hit = make_proxy(&[hit_point], 0.1);
    assert!(overlap_compound(&c, transform, &hit));

    // A proxy left in the compound frame must not register against the moved compound
    let stale = make_proxy(&[v(3.0, 0.0, 0.0)], 0.1);
    assert!(!overlap_compound(&c, transform, &stale));

    destroy_compound(c);
}

/// (CompoundOverlapChildTypes)
#[test]
fn compound_overlap_child_types() {
    // One child of every type, spaced far enough apart that a hit names the type it came from.
    // Hull and mesh children add a child transform underneath the compound transform.
    let mat = default_surface_material();
    let box_hull = make_box_hull(0.5, 0.5, 0.5);
    let md = create_box_mesh(VEC3_ZERO, v(0.5, 0.5, 0.5), false).unwrap();

    let capsule = CompoundCapsuleDef {
        capsule: Capsule {
            center1: v(-10.0, 0.0, 0.0),
            center2: v(-9.0, 0.0, 0.0),
            radius: 0.25,
        },
        material: mat,
    };
    let hull = CompoundHullDef {
        hull: &box_hull.base,
        transform: Transform {
            p: v(0.0, 0.0, 0.0),
            q: make_quat_from_axis_angle(VEC3_AXIS_Z, 0.25 * PI),
        },
        material: mat,
    };
    let mesh = CompoundMeshDef {
        mesh_data: &md,
        transform: Transform {
            p: v(10.0, 0.0, 0.0),
            q: make_quat_from_axis_angle(VEC3_AXIS_Y, 0.5 * PI),
        },
        scale: v(1.0, 1.0, 1.0),
        materials: &[mat],
    };
    let sphere = CompoundSphereDef {
        sphere: Sphere {
            center: v(20.0, 0.0, 0.0),
            radius: 0.5,
        },
        material: mat,
    };

    let c = create_compound(&CompoundDef {
        capsules: &[capsule],
        hulls: &[hull],
        meshes: &[mesh],
        spheres: &[sphere],
    })
    .unwrap();

    let transform = Transform {
        p: v(100.0, 200.0, 300.0),
        q: make_quat_from_axis_angle(VEC3_AXIS_Z, 0.5 * PI),
    };

    // Points sitting on or inside each child, then carried out to world space
    let local_points = [
        v(-9.5, 0.0, 0.0),  // capsule axis
        v(0.0, 0.0, 0.0),   // hull center
        v(10.0, 0.45, 0.0), // just under a mesh face
        v(20.0, 0.0, 0.0),  // sphere center
    ];

    for local_point in local_points {
        let world_point = transform_point(transform, local_point);
        let proxy = make_proxy(&[world_point], 0.1);
        assert!(overlap_compound(&c, transform, &proxy));
    }

    // A gap between the hull and the mesh
    let gap_point = transform_point(transform, v(5.0, 0.0, 0.0));
    let gap = make_proxy(&[gap_point], 0.1);
    assert!(!overlap_compound(&c, transform, &gap));

    destroy_compound(c);
    destroy_mesh(md);
}

/// (CompoundOverlapSegmentProxy)
#[test]
fn compound_overlap_segment_proxy() {
    // A multi point proxy has to be carried into the compound frame point by point
    let mat = default_surface_material();
    let spheres = two_sphere_compound_def(mat);
    let c = create_compound(&CompoundDef {
        spheres: &spheres,
        ..Default::default()
    })
    .unwrap();

    let transform = Transform {
        p: v(-40.0, 15.0, 7.0),
        q: make_quat_from_axis_angle(VEC3_AXIS_Y, 0.5 * PI),
    };

    // Segment that stays inside the gap
    let in_gap = [
        transform_point(transform, v(-1.0, 0.0, 0.0)),
        transform_point(transform, v(1.0, 0.0, 0.0)),
    ];
    let gap = make_proxy(&in_gap, 0.25);
    assert!(!overlap_compound(&c, transform, &gap));

    // Same segment stretched until one end reaches the second sphere
    let reaching = [
        transform_point(transform, v(-1.0, 0.0, 0.0)),
        transform_point(transform, v(2.9, 0.0, 0.0)),
    ];
    let hit = make_proxy(&reaching, 0.25);
    assert!(overlap_compound(&c, transform, &hit));

    destroy_compound(c);
}

/// (CompoundMoverRotatedChild)
#[test]
fn compound_mover_rotated_child() {
    // The child solves the mover in its own frame, so the plane it returns has to be carried
    // back into the compound frame before the caller sees it.
    let mat = make_material(0.25, 77);
    let box_hull = make_box_hull(0.5, 0.5, 0.5);

    // Quarter turn about z, so the compound space top face is a side face of the hull
    let hull = CompoundHullDef {
        hull: &box_hull.base,
        transform: Transform {
            p: v(0.0, 0.0, 0.0),
            q: make_quat_from_axis_angle(VEC3_AXIS_Z, 0.5 * PI),
        },
        material: mat,
    };
    let c = create_compound(&CompoundDef {
        hulls: &[hull],
        ..Default::default()
    })
    .unwrap();

    let mover = Capsule {
        center1: v(-0.1, 0.6, 0.0),
        center2: v(0.1, 0.6, 0.0),
        radius: 0.2,
    };

    let mut planes = [PlaneResult::default(); 8];
    let plane_count = collide_mover_and_compound(&mut planes, &c, &mover);
    assert!(plane_count >= 1);

    ensure_small(planes[0].plane.normal.x, 1e-3);
    ensure_small(planes[0].plane.normal.y - 1.0, 1e-3);
    ensure_small(planes[0].plane.normal.z, 1e-3);
    ensure_small(planes[0].point.y - 0.5, 1e-3);

    assert_eq!(planes[0].child_index, 0);
    let table = get_compound_materials(&c);
    assert_eq!(
        table[planes[0].material_index as usize].user_material_id,
        77
    );

    destroy_compound(c);
}
