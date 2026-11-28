//! Mesh / height-field triangle cache refresh and BVH queries.
//!
//! Port of `b3RefreshCache` / query helpers from
//! `box3d-cpp-reference/src/mesh_contact.c`.
//!
//! SPDX-FileCopyrightText: 2025 Erin Catto
//! SPDX-License-Identifier: MIT

use super::{ContactCache, MeshContact, TriangleCache};
use crate::constants::{max_aabb_margin, speculative_distance, MAX_MESH_CONTACT_TRIANGLES};
use crate::geometry::ShapeType;
use crate::height_field::query_height_field;
use crate::math_functions::{
    aabb_contains, aabb_transform, add, invert_transform, sub, to_relative_transform, Aabb, Vec3,
    WorldTransform, POS_ZERO,
};
use crate::mesh::{query_mesh, Mesh};
use crate::shape::{Shape, ShapeGeometry};
use std::sync::Once;

/// Query mesh triangles overlapping `bounds`. (b3QueryMeshTriangles)
///
/// Primordial Engine fork (E2): every triangle overlapping `bounds`, with no capacity. Upstream stops at
/// [`MAX_MESH_CONTACT_TRIANGLES`] (512) and warns once, and the triangles past it are left out of the contact, so a
/// dense mesh let what landed on it fall through (a 4,608-triangle terrain 20 cm across dropped 7 of 9 balls). The
/// list is sorted by the caller, so its order is the triangles' index order on every machine. See FORK.md.
fn query_mesh_triangles(indices: &mut Vec<i32>, mesh: &Mesh<'_>, bounds: Aabb) {
    query_mesh(mesh, bounds, |_a, _b, _c, triangle_index| {
        indices.push(triangle_index);
        true
    });
}

/// Query height-field triangles overlapping `bounds`. (b3QueryHeightFieldTriangles)
fn query_height_field_triangles(
    indices: &mut [i32],
    capacity: i32,
    height_field: &crate::height_field::HeightFieldData,
    bounds: Aabb,
) -> i32 {
    let mut count = 0i32;
    query_height_field(height_field, bounds, |_a, _b, _c, triangle_index| {
        if count == capacity {
            return false;
        }
        indices[count as usize] = triangle_index;
        count += 1;
        count < capacity
    });
    count
}

/// Refresh the per-contact triangle cache when the mover leaves query bounds.
/// (b3RefreshCache)
///
/// Primordial Engine fork (CCD part 2 C): `mesh_moves` says the mesh's own body isn't static. The cached query
/// bounds are in world space, and the triangles in them were found in the mesh's frame as it was then, so a mesh
/// that moves has them found again each time; a static mesh keeps upstream's rule. See FORK.md.
pub(crate) fn refresh_cache(
    mesh_contact: &mut MeshContact,
    shape_a: &Shape,
    xf_a: WorldTransform,
    bounds: Aabb,
    mesh_moves: bool,
) {
    debug_assert!(
        shape_a.shape_type() == ShapeType::Mesh || shape_a.shape_type() == ShapeType::Height
    );

    // If the dynamic body didn't move out of the cached query bounds we are done!
    if !mesh_moves && aabb_contains(mesh_contact.query_bounds, bounds) {
        if let ShapeGeometry::Mesh { data, .. } = &shape_a.geometry {
            for cache in &mesh_contact.triangle_cache {
                debug_assert!(
                    0 <= cache.triangle_index && cache.triangle_index < data.triangle_count
                );
            }
        }
        return;
    }

    // Enlarge the query bounds to absorb small movement.
    let radius = max_aabb_margin() + speculative_distance();
    let extension = Vec3 {
        x: radius,
        y: radius,
        z: radius,
    };
    mesh_contact.query_bounds = Aabb {
        lower_bound: sub(bounds.lower_bound, extension),
        upper_bound: add(bounds.upper_bound, extension),
    };

    // Query triangles. C bumps these buffers from the step arena (they no longer fit on the
    // stack with 512 triangles); the port uses heap buffers.
    let triangle_capacity = MAX_MESH_CONTACT_TRIANGLES as i32;
    let mut triangle_indices: Vec<i32> = Vec::new();

    // Bounds are in world space. Convert to the local mesh frame.
    let mesh_transform = to_relative_transform(xf_a, POS_ZERO);
    let local_bounds = aabb_transform(invert_transform(mesh_transform), mesh_contact.query_bounds);

    match &shape_a.geometry {
        ShapeGeometry::Mesh { data, scale } => {
            let mesh = Mesh {
                data,
                scale: *scale,
            };
            // Primordial Engine fork (E2): no capacity for a mesh; see `query_mesh_triangles`.
            query_mesh_triangles(&mut triangle_indices, &mesh, local_bounds);
        }
        ShapeGeometry::HeightField(hf) => {
            triangle_indices.resize(MAX_MESH_CONTACT_TRIANGLES, 0);
            let count = query_height_field_triangles(
                &mut triangle_indices,
                triangle_capacity,
                hf,
                local_bounds,
            );
            triangle_indices.truncate(count as usize);
            if count == triangle_capacity {
                static ONCE: Once = Once::new();
                ONCE.call_once(|| {
                    // Primordial Engine fork: through crate::warn, so an application's log gets it. See FORK.md.
                    crate::warn(&format!(
                        "WARNING: complex mesh detected, triangle buffer capacity of {triangle_capacity} reached"
                    ));
                });
            }
        }
        _ => unreachable!(),
    }
    let triangle_count = triangle_indices.len() as i32;

    // Triangle indices must be sorted to match caches (C validates this; BVH
    // traversal is not always monotonic, so sort explicitly).
    triangle_indices[..triangle_count as usize].sort_unstable();

    debug_assert!({
        let mut ok = true;
        for i in 0..(triangle_count - 1).max(0) {
            if triangle_indices[i as usize] >= triangle_indices[(i + 1) as usize] {
                ok = false;
                break;
            }
        }
        ok || triangle_count <= 1
    });

    // Create new contact cache and match with old one.
    let mut contact_caches = vec![ContactCache::default(); triangle_count as usize];
    let mut index2 = 0i32;
    let old_count = mesh_contact.triangle_cache.len() as i32;
    for index1 in 0..triangle_count {
        contact_caches[index1 as usize] = ContactCache::default();

        while index2 < old_count
            && mesh_contact.triangle_cache[index2 as usize].triangle_index
                < triangle_indices[index1 as usize]
        {
            index2 += 1;
        }

        if index2 < old_count
            && mesh_contact.triangle_cache[index2 as usize].triangle_index
                == triangle_indices[index1 as usize]
        {
            contact_caches[index1 as usize] = mesh_contact.triangle_cache[index2 as usize].cache;
        }
    }

    mesh_contact.triangle_cache.clear();
    mesh_contact.triangle_cache.reserve(triangle_count as usize);
    for i in 0..triangle_count {
        let triangle_index = triangle_indices[i as usize];
        if let ShapeGeometry::Mesh { data, .. } = &shape_a.geometry {
            debug_assert!(0 <= triangle_index && triangle_index < data.triangle_count);
        }
        mesh_contact.triangle_cache.push(TriangleCache {
            triangle_index,
            cache: contact_caches[i as usize],
        });
    }
}
