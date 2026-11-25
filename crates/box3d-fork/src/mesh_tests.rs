//! Authored tests for mesh pure geometry, plus the port of
//! box3d-cpp-reference/test/test_mesh.c (stride, clockwise winding, welding,
//! and the built in creators) at the end of the file.
//!
//! Covers create/AABB/ray/overlap for box and grid meshes, plus BVH vs brute ray.
//!
//! SPDX-FileCopyrightText: 2026 Erin Catto
//! SPDX-License-Identifier: MIT

use crate::core::NULL_INDEX;
use crate::distance::{make_proxy, CastOutput};
use crate::geometry::RayCastInput;
use crate::math_functions::{
    cross, distance_squared, intersect_ray_triangle, is_sane_aabb, mul, normalize, sub, Vec3,
    TRANSFORM_IDENTITY, VEC3_ONE, VEC3_ZERO,
};
use crate::mesh::{
    compute_mesh_aabb, create_box_mesh, create_grid_mesh, create_hollow_box_mesh, create_mesh,
    create_platform_mesh, create_torus_mesh, create_wave_mesh, destroy_mesh, get_height,
    get_mesh_flags, get_mesh_triangle, get_mesh_triangles, get_mesh_vertices, is_valid_mesh,
    overlap_mesh, ray_cast_mesh, Mesh, MeshData, MeshDef, ALL_CONCAVE_EDGES, MESH_VERSION,
};

fn v(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

fn ensure_small(value: f32, tolerance: f32) {
    assert!(
        !(value < -tolerance || tolerance < value),
        "|{value}| > tolerance {tolerance}"
    );
}

#[test]
fn mesh_box_create() {
    let mesh = create_box_mesh(VEC3_ZERO, v(1.0, 1.0, 1.0), true).expect("box mesh");
    assert_eq!(mesh.version, MESH_VERSION);
    assert!(is_valid_mesh(Some(&mesh)));
    assert_eq!(mesh.vertex_count, 8);
    assert_eq!(mesh.triangle_count, 12);
    assert!(mesh.node_count >= 1);
    assert!(get_height(&mesh) >= 0);
    assert!(mesh.hash != 0);

    ensure_small(mesh.bounds.lower_bound.x + 1.0, 1e-5);
    ensure_small(mesh.bounds.lower_bound.y + 1.0, 1e-5);
    ensure_small(mesh.bounds.lower_bound.z + 1.0, 1e-5);
    ensure_small(mesh.bounds.upper_bound.x - 1.0, 1e-5);
    ensure_small(mesh.bounds.upper_bound.y - 1.0, 1e-5);
    ensure_small(mesh.bounds.upper_bound.z - 1.0, 1e-5);

    destroy_mesh(mesh);
}

#[test]
fn mesh_box_winding() {
    let mesh = create_box_mesh(VEC3_ZERO, v(1.0, 1.0, 1.0), false).expect("box mesh");
    let view = Mesh::with_unit_scale(&mesh);

    for i in 0..mesh.triangle_count {
        let t = get_mesh_triangle(&view, i);
        let e1 = sub(t.vertices[1], t.vertices[0]);
        let e2 = sub(t.vertices[2], t.vertices[0]);
        let n = cross(e1, e2);
        // Outward-ish: normal should point away from origin for a unit box.
        let center = v(
            (t.vertices[0].x + t.vertices[1].x + t.vertices[2].x) / 3.0,
            (t.vertices[0].y + t.vertices[1].y + t.vertices[2].y) / 3.0,
            (t.vertices[0].z + t.vertices[1].z + t.vertices[2].z) / 3.0,
        );
        let outward = crate::math_functions::dot(n, center);
        assert!(outward > 0.0, "triangle {i} winding inverted");
    }

    destroy_mesh(mesh);
}

#[test]
fn mesh_box_aabb() {
    let mesh = create_box_mesh(v(2.0, 3.0, 4.0), v(1.0, 0.5, 0.25), false).expect("box");
    let aabb = compute_mesh_aabb(&mesh, TRANSFORM_IDENTITY, VEC3_ONE);
    ensure_small(aabb.lower_bound.x - 1.0, 1e-5);
    ensure_small(aabb.lower_bound.y - 2.5, 1e-5);
    ensure_small(aabb.lower_bound.z - 3.75, 1e-5);
    ensure_small(aabb.upper_bound.x - 3.0, 1e-5);
    ensure_small(aabb.upper_bound.y - 3.5, 1e-5);
    ensure_small(aabb.upper_bound.z - 4.25, 1e-5);

    let scaled = compute_mesh_aabb(&mesh, TRANSFORM_IDENTITY, v(2.0, 1.0, 1.0));
    ensure_small(scaled.lower_bound.x - 2.0, 1e-5);
    ensure_small(scaled.upper_bound.x - 6.0, 1e-5);

    destroy_mesh(mesh);
}

#[test]
fn mesh_box_ray_hit_miss() {
    let mesh = create_box_mesh(VEC3_ZERO, v(1.0, 1.0, 1.0), false).expect("box");
    let view = Mesh::with_unit_scale(&mesh);

    let hit_input = RayCastInput {
        origin: v(0.0, 0.0, 3.0),
        translation: v(0.0, 0.0, -4.0),
        max_fraction: 1.0,
    };
    let hit = ray_cast_mesh(&view, &hit_input);
    assert!(hit.hit);
    assert!(hit.triangle_index != NULL_INDEX);
    ensure_small(hit.fraction - 0.5, 1e-4);
    ensure_small(hit.point.z - 1.0, 1e-4);

    let miss_input = RayCastInput {
        origin: v(0.0, 0.0, 3.0),
        translation: v(0.0, 0.0, 1.0),
        max_fraction: 1.0,
    };
    let miss = ray_cast_mesh(&view, &miss_input);
    assert!(!miss.hit);

    destroy_mesh(mesh);
}

#[test]
fn mesh_box_overlap() {
    let mesh = create_box_mesh(VEC3_ZERO, v(1.0, 1.0, 1.0), false).expect("box");
    let view = Mesh::with_unit_scale(&mesh);

    // Mesh is a thin surface — probe near a face, not the hollow interior.
    let near_face = make_proxy(&[v(0.0, 0.0, 0.95)], 0.1);
    assert!(overlap_mesh(&view, TRANSFORM_IDENTITY, &near_face));

    let outside = make_proxy(&[v(5.0, 0.0, 0.0)], 0.1);
    assert!(!overlap_mesh(&view, TRANSFORM_IDENTITY, &outside));

    destroy_mesh(mesh);
}

#[test]
fn mesh_grid_create() {
    let mesh = create_grid_mesh(4, 3, 1.0, 0, false).expect("grid");
    assert_eq!(mesh.version, MESH_VERSION);
    assert_eq!(mesh.vertex_count, 5 * 4);
    assert_eq!(mesh.triangle_count, 2 * 4 * 3);
    assert!(mesh.node_count > 1);
    assert!(is_valid_mesh(Some(&mesh)));

    // Grid is centered: x in [-2, 2], z in [-1.5, 1.5]
    ensure_small(mesh.bounds.lower_bound.x + 2.0, 1e-5);
    ensure_small(mesh.bounds.upper_bound.x - 2.0, 1e-5);
    ensure_small(mesh.bounds.lower_bound.z + 1.5, 1e-5);
    ensure_small(mesh.bounds.upper_bound.z - 1.5, 1e-5);
    ensure_small(mesh.bounds.lower_bound.y, 1e-5);
    ensure_small(mesh.bounds.upper_bound.y, 1e-5);

    destroy_mesh(mesh);
}

#[test]
fn mesh_grid_ray_and_bvh_vs_brute() {
    let mesh = create_grid_mesh(8, 8, 1.0, 0, true).expect("grid");
    let view = Mesh::with_unit_scale(&mesh);

    let rays = [
        (v(0.0, 2.0, 0.0), v(0.0, -4.0, 0.0)),
        (v(-3.0, 1.0, -2.0), v(6.0, -2.0, 4.0)),
        (v(1.5, 5.0, -1.5), v(0.0, -10.0, 0.0)),
        (v(0.0, -1.0, 0.0), v(0.0, -1.0, 0.0)), // miss (below plane, going down)
        (v(10.0, 1.0, 10.0), v(1.0, 0.0, 0.0)), // miss
    ];

    for (origin, translation) in rays {
        let input = RayCastInput {
            origin,
            translation,
            max_fraction: 1.0,
        };
        let bvh = ray_cast_mesh(&view, &input);
        let brute = brute_force_ray_cast(&view, &input);

        assert_eq!(bvh.hit, brute.hit, "hit mismatch at {origin:?}");
        if bvh.hit {
            ensure_small(bvh.fraction - brute.fraction, 1e-5);
            // Coplanar grid edges can yield equal fractions on adjacent triangles;
            // require matching hit point rather than a unique triangle index.
            ensure_small(bvh.point.x - brute.point.x, 1e-4);
            ensure_small(bvh.point.y - brute.point.y, 1e-4);
            ensure_small(bvh.point.z - brute.point.z, 1e-4);
        }
    }

    destroy_mesh(mesh);
}

fn brute_force_ray_cast(mesh: &Mesh<'_>, input: &RayCastInput) -> CastOutput {
    let mut best = CastOutput {
        fraction: input.max_fraction,
        triangle_index: NULL_INDEX,
        ..Default::default()
    };
    let scale = mesh.scale;
    let clockwise = scale.x * scale.y * scale.z < 0.0;

    for triangle_index in 0..mesh.data.triangle_count {
        let tri = &mesh.data.triangles[triangle_index as usize];
        let vertex1 = mul(scale, mesh.data.vertices[tri.index1 as usize]);
        let (vertex2, vertex3) = if clockwise {
            (
                mul(scale, mesh.data.vertices[tri.index3 as usize]),
                mul(scale, mesh.data.vertices[tri.index2 as usize]),
            )
        } else {
            (
                mul(scale, mesh.data.vertices[tri.index2 as usize]),
                mul(scale, mesh.data.vertices[tri.index3 as usize]),
            )
        };

        let alpha =
            intersect_ray_triangle(input.origin, input.translation, vertex1, vertex2, vertex3);
        if alpha < best.fraction {
            let edge1 = sub(vertex2, vertex1);
            let edge2 = sub(vertex3, vertex1);
            best.normal = normalize(cross(edge1, edge2));
            best.point = crate::math_functions::add(
                input.origin,
                crate::math_functions::mul_sv(alpha, input.translation),
            );
            best.fraction = alpha;
            best.triangle_index = triangle_index;
            best.material_index = mesh.data.material_indices[triangle_index as usize] as i32;
            best.hit = true;
        }
    }
    best
}

#[test]
fn mesh_create_from_def() {
    let def = MeshDef {
        vertices: vec![v(0.0, 0.0, 0.0), v(1.0, 0.0, 0.0), v(0.0, 0.0, 1.0)],
        indices: vec![0, 1, 2],
        identify_edges: false,
        use_median_split: false,
        ..Default::default()
    };
    let mesh = create_mesh(&def, None).expect("triangle mesh");
    assert_eq!(mesh.triangle_count, 1);
    assert_eq!(mesh.vertex_count, 3);
    assert_eq!(mesh.node_count, 1);
    assert!(mesh.nodes[0].is_leaf());
    destroy_mesh(mesh);
}

#[test]
fn mesh_negative_scale_winding() {
    let mesh = create_box_mesh(VEC3_ZERO, v(1.0, 1.0, 1.0), false).expect("box");
    let view = Mesh::new(&mesh, v(1.0, -1.0, 1.0));
    let t = get_mesh_triangle(&view, 0);
    // Negative scale product swaps indices 2/3 relative to stored order.
    let stored = &mesh.triangles[0];
    assert_eq!(t.i1, stored.index1);
    assert_eq!(t.i2, stored.index3);
    assert_eq!(t.i3, stored.index2);
    destroy_mesh(mesh);
}

// Port of test_mesh.c ----------------------------------------------------------------------

// Two quads meeting at a concave crease along the shared edge 1-4. The crease exercises
// edge identification, which reads the baked winding rather than the input winding.
const VALLEY_VERTEX_COUNT: usize = 6;
const VALLEY_TRIANGLE_COUNT: usize = 4;

fn make_valley() -> (Vec<Vec3>, Vec<i32>) {
    let vertices = vec![
        v(-1.0, 1.0, -1.0),
        v(0.0, 0.0, -1.0),
        v(1.0, 1.0, -1.0),
        v(-1.0, 1.0, 1.0),
        v(0.0, 0.0, 1.0),
        v(1.0, 1.0, 1.0),
    ];

    #[rustfmt::skip]
    let indices = vec![
        0, 3, 1, // left quad
        3, 4, 1,
        1, 4, 2, // right quad
        4, 5, 2,
    ];

    (vertices, indices)
}

// A vertex laid out the way a renderer would hand it over. The position sits off the
// front of the struct so a non-zero base offset gets exercised too.
//
// C: struct FatVertex { float weight; b3Vec3 position; float uv[2]; }. The bytes are
// written field by field so the layout (24 bytes, position at byte 4) does not depend
// on Rust struct layout.
const FAT_VERTEX_SIZE: usize = 24;
const FAT_POSITION_OFFSET: usize = 4;

fn make_fat_vertices(vertices: &[Vec3]) -> Vec<u8> {
    let mut fat = Vec::with_capacity(FAT_VERTEX_SIZE * vertices.len());
    for (i, vertex) in vertices.iter().enumerate() {
        // Poison the padding so a stride mistake bakes obvious garbage instead of near misses
        let weight = 1000.0f32 + i as f32;
        fat.extend_from_slice(&weight.to_ne_bytes());
        fat.extend_from_slice(&vertex.x.to_ne_bytes());
        fat.extend_from_slice(&vertex.y.to_ne_bytes());
        fat.extend_from_slice(&vertex.z.to_ne_bytes());
        fat.extend_from_slice(&(-1000.0f32).to_ne_bytes());
        fat.extend_from_slice(&(-2000.0f32).to_ne_bytes());
    }
    fat
}

// The Rust stand-in for pointing at the first fat position: the bytes starting there.
fn fat_position_bytes(fat: &[u8]) -> Vec<u8> {
    fat[FAT_POSITION_OFFSET..].to_vec()
}

fn reverse_winding(indices: &mut [i32], triangle_count: usize) {
    for i in 0..triangle_count {
        indices.swap(3 * i + 1, 3 * i + 2);
    }
}

// The hash covers every byte of the mesh block, so this compares the tree, the vertices,
// the baked triangles and the edge flags in one shot.
fn meshes_match(mesh1: &MeshData, mesh2: &MeshData) -> bool {
    mesh1.byte_count == mesh2.byte_count && mesh1.hash == mesh2.hash
}

fn vertices_match(mesh: &MeshData, expected: &[Vec3], count: usize) -> bool {
    if mesh.vertex_count as usize != count {
        return false;
    }

    let vertices = get_mesh_vertices(mesh);
    for i in 0..count {
        if distance_squared(vertices[i], expected[i]) > f32::EPSILON {
            return false;
        }
    }

    true
}

// The valley faces up, so every baked triangle should wind counter clockwise about +Y
fn faces_up(mesh: &MeshData) -> bool {
    let vertices = get_mesh_vertices(mesh);
    let triangles = get_mesh_triangles(mesh);

    for i in 0..mesh.triangle_count as usize {
        let v1 = vertices[triangles[i].index1 as usize];
        let v2 = vertices[triangles[i].index2 as usize];
        let v3 = vertices[triangles[i].index3 as usize];

        let normal = cross(sub(v2, v1), sub(v3, v1));
        if normal.y <= 0.0 {
            return false;
        }
    }

    true
}

fn has_concave_edge(mesh: &MeshData) -> bool {
    let flags = get_mesh_flags(mesh);

    for i in 0..mesh.triangle_count as usize {
        if (flags[i] as i32 & ALL_CONCAVE_EDGES) != 0 {
            return true;
        }
    }

    false
}

fn make_valley_def(vertices: &[Vec3], indices: &[i32]) -> MeshDef {
    MeshDef {
        vertices: vertices.to_vec(),
        stride: core::mem::size_of::<Vec3>(),
        indices: indices.to_vec(),
        identify_edges: true,
        ..Default::default()
    }
}

// A def reading positions out of fat vertices, like C's MakeValleyDef( &fat[0].position, ... )
// with stride = sizeof( FatVertex ).
fn make_fat_valley_def(fat: &[u8], indices: &[i32]) -> MeshDef {
    MeshDef {
        vertex_bytes: fat_position_bytes(fat),
        stride: FAT_VERTEX_SIZE,
        indices: indices.to_vec(),
        identify_edges: true,
        ..Default::default()
    }
}

// Dense input is the reference the other cases are measured against
#[test]
fn mesh_dense_stride() {
    let (vertices, indices) = make_valley();

    let def = make_valley_def(&vertices, &indices);
    let mesh = create_mesh(&def, None).expect("mesh");

    assert_eq!(mesh.vertex_count as usize, VALLEY_VERTEX_COUNT);
    assert_eq!(mesh.triangle_count as usize, VALLEY_TRIANGLE_COUNT);
    assert_eq!(mesh.degenerate_count, 0);
    assert!(vertices_match(&mesh, &vertices, VALLEY_VERTEX_COUNT));
    assert!(faces_up(&mesh));
    assert!(has_concave_edge(&mesh));

    destroy_mesh(mesh);
}

// Interleaved input must bake to exactly the same mesh as dense input
#[test]
fn mesh_fat_stride() {
    let (vertices, indices) = make_valley();

    let dense_def = make_valley_def(&vertices, &indices);
    let dense_mesh = create_mesh(&dense_def, None).expect("dense mesh");

    let fat = make_fat_vertices(&vertices);

    let fat_def = make_fat_valley_def(&fat, &indices);
    let fat_mesh = create_mesh(&fat_def, None).expect("fat mesh");

    assert!(vertices_match(&fat_mesh, &vertices, VALLEY_VERTEX_COUNT));
    assert!(faces_up(&fat_mesh));
    assert!(meshes_match(&dense_mesh, &fat_mesh));

    destroy_mesh(fat_mesh);
    destroy_mesh(dense_mesh);
}

// Welding reads the source vertices through the stride as well
#[test]
fn mesh_stride_weld() {
    let (vertices, indices) = make_valley();

    // Split the crease so each quad owns a copy of the shared edge
    let mut split_vertices = vertices.clone();
    split_vertices.push(vertices[1]);
    split_vertices.push(vertices[4]);
    assert_eq!(split_vertices.len(), 8);

    // Point the right quad at the copies
    let mut split_indices = indices.clone();
    split_indices[6] = 6;
    split_indices[7] = 7;
    split_indices[9] = 7;

    let fat = make_fat_vertices(&split_vertices);

    let mut def = make_fat_valley_def(&fat, &split_indices);
    def.weld_vertices = true;
    def.weld_tolerance = 0.01;

    let mesh = create_mesh(&def, None).expect("mesh");

    assert_eq!(mesh.vertex_count as usize, VALLEY_VERTEX_COUNT);
    assert_eq!(mesh.triangle_count as usize, VALLEY_TRIANGLE_COUNT);
    assert!(vertices_match(&mesh, &vertices, VALLEY_VERTEX_COUNT));
    assert!(faces_up(&mesh));
    assert!(has_concave_edge(&mesh));

    destroy_mesh(mesh);
}

// Clockwise input plus the flag must bake to the same mesh as counter clockwise input
#[test]
fn mesh_clock_wise() {
    let (vertices, indices) = make_valley();

    let ccw_def = make_valley_def(&vertices, &indices);
    let ccw_mesh = create_mesh(&ccw_def, None).expect("ccw mesh");

    let mut reversed = indices.clone();
    reverse_winding(&mut reversed, VALLEY_TRIANGLE_COUNT);

    let mut cw_def = make_valley_def(&vertices, &reversed);
    cw_def.clock_wise_winding = true;
    let cw_mesh = create_mesh(&cw_def, None).expect("cw mesh");

    assert!(faces_up(&cw_mesh));
    assert!(has_concave_edge(&cw_mesh));
    assert!(meshes_match(&ccw_mesh, &cw_mesh));

    destroy_mesh(cw_mesh);
    destroy_mesh(ccw_mesh);
}

// Without the flag the same clockwise input must bake inside out
#[test]
fn mesh_clock_wise_ignored() {
    let (vertices, mut indices) = make_valley();
    reverse_winding(&mut indices, VALLEY_TRIANGLE_COUNT);

    let def = make_valley_def(&vertices, &indices);
    let mesh = create_mesh(&def, None).expect("mesh");

    assert!(!faces_up(&mesh));

    destroy_mesh(mesh);
}

// Winding, stride and welding have to compose
#[test]
fn mesh_clock_wise_stride_weld() {
    let (vertices, indices) = make_valley();

    let mut ccw_def = make_valley_def(&vertices, &indices);
    ccw_def.weld_vertices = true;
    ccw_def.weld_tolerance = 0.01;
    let ccw_mesh = create_mesh(&ccw_def, None).expect("ccw mesh");

    let mut reversed = indices.clone();
    reverse_winding(&mut reversed, VALLEY_TRIANGLE_COUNT);

    let fat = make_fat_vertices(&vertices);

    let mut cw_def = make_fat_valley_def(&fat, &reversed);
    cw_def.clock_wise_winding = true;
    cw_def.weld_vertices = true;
    cw_def.weld_tolerance = 0.01;
    let cw_mesh = create_mesh(&cw_def, None).expect("cw mesh");

    assert!(faces_up(&cw_mesh));
    assert!(meshes_match(&ccw_mesh, &cw_mesh));

    destroy_mesh(cw_mesh);
    destroy_mesh(ccw_mesh);
}

// The built in creators fill their own def, so a missed stride shows up here
#[test]
fn mesh_creators() {
    let center = v(1.0, 2.0, 3.0);
    let extent = v(0.5, 1.0, 1.5);

    let meshes = [
        create_grid_mesh(4, 4, 1.0, 2, true),
        create_wave_mesh(4, 4, 1.0, 0.5, 1.0, 1.0),
        create_torus_mesh(8, 6, 2.0, 0.5),
        create_box_mesh(center, extent, true),
        create_hollow_box_mesh(center, extent),
        create_platform_mesh(center, 2.0, 1.0, 2.0),
    ];

    for mesh in meshes {
        let mesh = mesh.expect("creator mesh");
        assert!(mesh.vertex_count >= 3);
        assert!(mesh.triangle_count >= 1);
        assert!(is_sane_aabb(mesh.bounds));

        destroy_mesh(mesh);
    }
}

// Stride validation (C rejects bad strides with NULL). The Rust port applies the stride only
// to `vertex_bytes`, so contiguous `vertices` with any stride other than 0 or 12 are rejected.
#[test]
fn mesh_stride_rejection() {
    let (vertices, indices) = make_valley();
    let fat = make_fat_vertices(&vertices);

    // Bad strides: too small, too large, not 4 aligned
    for stride in [8usize, 4100, 26] {
        let mut def = make_fat_valley_def(&fat, &indices);
        def.stride = stride;
        assert!(create_mesh(&def, None).is_none(), "stride {stride}");
    }

    // Non-12 stride with contiguous vertices and empty vertex_bytes
    let mut def = make_valley_def(&vertices, &indices);
    def.stride = FAT_VERTEX_SIZE;
    assert!(create_mesh(&def, None).is_none());

    // Stride 0 and 12 are both accepted for contiguous vertices
    for stride in [0usize, core::mem::size_of::<Vec3>()] {
        let mut def = make_valley_def(&vertices, &indices);
        def.stride = stride;
        assert!(create_mesh(&def, None).is_some(), "stride {stride}");
    }
}

// Primordial Engine fork (CCD part 2 C): a closed mesh's mass (world-model.md 3.2).

mod closed_mesh_mass {
    use crate::geometry::MassData;
    use crate::hull::{compute_hull_mass, make_box_hull};
    use crate::math_functions::{mul, Vec3, VEC3_ONE, VEC3_ZERO};
    use crate::mesh::{
        compute_closed_mesh_mass, create_box_mesh, create_mesh, is_closed_mesh, MeshData, MeshDef, MeshTriangle,
    };

    fn v(x: f32, y: f32, z: f32) -> Vec3 {
        Vec3 { x, y, z }
    }

    fn mass_of(mesh: &MeshData, scale: Vec3, density: f32) -> MassData {
        compute_closed_mesh_mass(&mesh.vertices, &mesh.triangles, scale, density)
    }

    fn mesh_of(vertices: Vec<Vec3>, indices: Vec<i32>) -> MeshData {
        create_mesh(&MeshDef { vertices, stride: core::mem::size_of::<Vec3>(), indices, ..Default::default() }, None)
            .expect("a mesh")
    }

    /// The box mesh's 8 corners and 12 outward triangles, half extents `h`, centred at `c` (as swept_tests'
    /// `box_mesh`).
    fn box_parts(c: Vec3, h: Vec3) -> (Vec<Vec3>, Vec<i32>) {
        let mut vertices = Vec::new();
        for k in 0..8 {
            let sx = if k & 1 != 0 { 1.0 } else { -1.0 };
            let sy = if k & 2 != 0 { 1.0 } else { -1.0 };
            let sz = if k & 4 != 0 { 1.0 } else { -1.0 };
            vertices.push(v(c.x + sx * h.x, c.y + sy * h.y, c.z + sz * h.z));
        }
        let indices = vec![0, 2, 1, 1, 2, 3, 4, 5, 6, 5, 7, 6, 0, 1, 4, 1, 5, 4, 2, 6, 3, 3, 6, 7, 0, 4, 2, 2, 4, 6, 1, 3, 5, 3, 7, 5];
        (vertices, indices)
    }

    fn close(a: f32, b: f32, rel: f32) -> bool {
        (a - b).abs() <= rel * a.abs().max(b.abs()).max(1e-12)
    }

    fn assert_inertia_close(a: &MassData, b: &MassData, rel: f32) {
        let scale = b.inertia.cx.x.abs().max(b.inertia.cy.y.abs()).max(b.inertia.cz.z.abs());
        for (x, y) in [
            (a.inertia.cx.x, b.inertia.cx.x),
            (a.inertia.cx.y, b.inertia.cx.y),
            (a.inertia.cx.z, b.inertia.cx.z),
            (a.inertia.cy.y, b.inertia.cy.y),
            (a.inertia.cy.z, b.inertia.cy.z),
            (a.inertia.cz.z, b.inertia.cz.z),
        ] {
            assert!((x - y).abs() <= rel * scale, "inertia {x} against {y} (scale {scale}): {a:?} {b:?}");
        }
    }

    /// A closed box mesh weighs the box: its mass, centre and inertia match Box3D's own box hull's.
    #[test]
    fn a_closed_box_mesh_weighs_the_box() {
        let h = v(0.5, 0.25, 0.1);
        for c in [VEC3_ZERO, v(0.3, -0.2, 0.7)] {
            let m = mass_of(&create_box_mesh(c, h, true).expect("box mesh"), VEC3_ONE, 2.0);
            let hull = compute_hull_mass(&make_box_hull(h.x, h.y, h.z).base, 2.0);
            assert!(close(m.mass, 0.2, 1e-6), "{m:?}");
            assert!((m.center.x - c.x).abs() < 1e-6 && (m.center.y - c.y).abs() < 1e-6 && (m.center.z - c.z).abs() < 1e-6, "{m:?}");
            assert_inertia_close(&m, &hull, 1e-5);
        }
    }

    /// Far from the shape's origin (1 km out) the sum still holds: it runs from the box's own centre.
    #[test]
    fn a_box_mesh_far_from_its_origin_keeps_its_inertia() {
        let h = v(0.5, 0.25, 0.1);
        let near = mass_of(&create_box_mesh(VEC3_ZERO, h, true).expect("box mesh"), VEC3_ONE, 2.0);
        let far = mass_of(&create_box_mesh(v(1000.0, -2000.0, 500.0), h, true).expect("box mesh"), VEC3_ONE, 2.0);
        assert!(close(far.mass, near.mass, 2e-3), "{far:?}");
        assert_inertia_close(&far, &near, 3e-3);
    }

    /// A scale applies to every vertex first: a mirroring scale keeps the positive volume, and an uneven one weighs
    /// the scaled box.
    #[test]
    fn scales_are_applied_first_and_a_mirror_keeps_its_volume() {
        let h = v(0.5, 0.25, 0.1);
        let mesh = create_box_mesh(VEC3_ZERO, h, true).expect("box mesh");
        let plain = mass_of(&mesh, VEC3_ONE, 2.0);
        for mirror in [v(-1.0, 1.0, 1.0), v(1.0, -1.0, -1.0), v(-1.0, -1.0, -1.0)] {
            let m = mass_of(&mesh, mirror, 2.0);
            assert!(close(m.mass, plain.mass, 1e-6), "{mirror:?}: {m:?}");
            assert_inertia_close(&m, &plain, 1e-5);
        }
        let s = v(2.0, 1.0, 0.5);
        let scaled = mass_of(&mesh, s, 2.0);
        let hull = compute_hull_mass(&make_box_hull(h.x * s.x, h.y * s.y, h.z * s.z).base, 2.0);
        assert!(close(scaled.mass, hull.mass, 1e-6), "{scaled:?}");
        assert_inertia_close(&scaled, &hull, 1e-5);
    }

    /// No mass: open (a triangle missing), wound inward, two boxes sharing a face (that face's edges are in four
    /// triangles), and a closed box under the 1e-9 m^3 floor (0.8 mm across).
    #[test]
    fn open_inward_shared_and_tiny_meshes_have_no_mass() {
        let (vertices, indices) = box_parts(VEC3_ZERO, v(0.5, 0.25, 0.1));
        let open = mesh_of(vertices.clone(), indices[..33].to_vec());
        assert_eq!(mass_of(&open, VEC3_ONE, 2.0).mass, 0.0);
        let inward: Vec<i32> = indices.chunks(3).flat_map(|t| [t[0], t[2], t[1]]).collect();
        assert_eq!(mass_of(&mesh_of(vertices.clone(), inward), VEC3_ONE, 2.0).mass, 0.0);
        // A second box sharing the first's +x face.
        let (more, more_indices) = box_parts(v(1.0, 0.0, 0.0), v(0.5, 0.25, 0.1));
        let mut both = vertices.clone();
        both.extend_from_slice(&more);
        let mut both_indices = indices.clone();
        both_indices.extend(more_indices.iter().map(|i| i + 8));
        assert_eq!(mass_of(&mesh_of(both, both_indices), VEC3_ONE, 2.0).mass, 0.0);
        let (small, small_indices) = box_parts(VEC3_ZERO, v(0.0004, 0.0004, 0.0004));
        assert_eq!(mass_of(&mesh_of(small, small_indices), VEC3_ONE, 2.0).mass, 0.0);
    }

    /// Closedness compares positions, not indices: a box whose faces each have their own four vertices (an exporter's
    /// hard edges) is closed and weighs the same.
    #[test]
    fn split_vertices_still_close_a_mesh() {
        let (corners, indices) = box_parts(VEC3_ZERO, v(0.5, 0.25, 0.1));
        let mut vertices = Vec::new();
        let mut split = Vec::new();
        for i in indices {
            split.push(vertices.len() as i32);
            vertices.push(corners[i as usize]);
        }
        let shared = mass_of(&create_box_mesh(VEC3_ZERO, v(0.5, 0.25, 0.1), true).expect("box mesh"), VEC3_ONE, 2.0);
        let m = mass_of(&mesh_of(vertices, split), VEC3_ONE, 2.0);
        assert!(close(m.mass, shared.mass, 1e-6), "{m:?}");
        assert_inertia_close(&m, &shared, 1e-5);
    }

    /// A triangle with two equal positions encloses nothing: it's left out of the test and the sum (given directly,
    /// since Box3D's own mesh build drops it first).
    #[test]
    fn a_triangle_with_two_equal_positions_is_left_out() {
        let (vertices, indices) = box_parts(VEC3_ZERO, v(0.5, 0.25, 0.1));
        let mut triangles: Vec<MeshTriangle> =
            indices.chunks(3).map(|t| MeshTriangle { index1: t[0], index2: t[1], index3: t[2] }).collect();
        let whole = compute_closed_mesh_mass(&vertices, &triangles, VEC3_ONE, 2.0);
        triangles.insert(5, MeshTriangle { index1: 0, index2: 3, index3: 0 });
        let with = compute_closed_mesh_mass(&vertices, &triangles, VEC3_ONE, 2.0);
        assert!(whole.mass > 0.0);
        assert_eq!(with.mass.to_bits(), whole.mass.to_bits());
    }

    /// Closedness is judged before the scale, so a scale never changes it (world-model.md 3.2 (a)). A box whose +x
    /// face has a crack, one corner 1 ulp off its copy (1.5 + 3 ulp against 1.5 + 2 ulp), is open; scaled by 0.75 in
    /// x the two positions round to the same f32, and the box still reads as open, with no mass.
    #[test]
    fn a_scale_that_merges_two_positions_leaves_a_cracked_mesh_open() {
        let (a, b) = (f32::from_bits(0x3fc0_0002), f32::from_bits(0x3fc0_0003));
        let s = v(0.75, 1.0, 1.0);
        assert_eq!(mul(s, v(a, 0.0, 0.0)).x.to_bits(), mul(s, v(b, 0.0, 0.0)).x.to_bits(), "the scale merges them");
        let (mut vertices, indices) = box_parts(VEC3_ZERO, v(a, 0.25, 0.1));
        let whole: Vec<MeshTriangle> =
            indices.chunks(3).map(|t| MeshTriangle { index1: t[0], index2: t[1], index3: t[2] }).collect();
        assert!(is_closed_mesh(&vertices, &whole));
        assert!(compute_closed_mesh_mass(&vertices, &whole, s, 2.0).mass > 0.0);
        // Corner 7 is (+a, +0.25, +0.1); the +x face's last triangle (3, 7, 5) takes a copy 1 ulp further out.
        vertices.push(v(b, 0.25, 0.1));
        let mut cracked = whole.clone();
        assert_eq!(cracked[11], MeshTriangle { index1: 3, index2: 7, index3: 5 });
        cracked[11].index2 = 8;
        assert!(!is_closed_mesh(&vertices, &cracked));
        for scale in [VEC3_ONE, s] {
            assert_eq!(compute_closed_mesh_mass(&vertices, &cracked, scale, 2.0).mass, 0.0, "{scale:?}");
        }
    }
}
