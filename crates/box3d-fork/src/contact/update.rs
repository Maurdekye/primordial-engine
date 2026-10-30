// Narrow-phase contact update: convex manifold compute and update_contact.
// Mesh/height manifolds: mesh_contact.rs.
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::mesh_contact::{apply_mesh_hit_flags, compute_mesh_manifolds};
use super::{contact_flags, ContactCache, ContactGeometry, ConvexContact};
use crate::constants::MAX_MANIFOLD_POINTS;
use crate::core::NULL_INDEX;
use crate::distance::SimplexCache;
use crate::geometry::ShapeType;
use crate::id::ShapeId;
use crate::manifold::{
    collide_capsule_and_sphere, collide_capsules, collide_hull_and_capsule,
    collide_hull_and_sphere, collide_hulls_at_width, collide_spheres, make_feature_id,
    LocalManifold, Manifold, ManifoldPoint, SatCache,
};
use crate::math_functions::{
    add, inv_mul_world_transforms, make_matrix_from_quat, max_float, mul_mv, mul_world_transforms,
    neg, offset_pos, rotate_vector, sub, sub_pos, WorldTransform,
};
use crate::shape::{shape_flags, Shape, ShapeGeometry};
use super::{CollideEnv, Contact};
use crate::world::TaskContext;
use std::sync::Arc;

fn convex_cache(geometry: &mut ContactGeometry) -> &mut ContactCache {
    match geometry {
        ContactGeometry::Convex(convex) => &mut convex.cache,
        ContactGeometry::Mesh(_) => {
            // Convex update always runs on convex geometry contacts.
            geometry.as_convex_cache()
        }
    }
}

impl ContactGeometry {
    fn as_convex_cache(&mut self) -> &mut ContactCache {
        *self = ContactGeometry::Convex(ConvexContact::default());
        match self {
            ContactGeometry::Convex(c) => &mut c.cache,
            ContactGeometry::Mesh(_) => unreachable!(),
        }
    }
}

fn ensure_sat_cache(cache: &mut ContactCache) -> &mut SatCache {
    if !matches!(cache, ContactCache::Sat(_)) {
        *cache = ContactCache::Sat(SatCache::default());
    }
    match cache {
        ContactCache::Sat(s) => s,
        ContactCache::Simplex(_) => unreachable!(),
    }
}

fn ensure_simplex_cache(cache: &mut ContactCache) -> &mut SimplexCache {
    if !matches!(cache, ContactCache::Simplex(_)) {
        *cache = ContactCache::Simplex(SimplexCache::default());
    }
    match cache {
        ContactCache::Simplex(s) => s,
        ContactCache::Sat(_) => unreachable!(),
    }
}

fn shape_rolling_radius(geometry: &ShapeGeometry) -> f32 {
    match geometry {
        ShapeGeometry::Sphere(s) => s.radius,
        ShapeGeometry::Capsule(c) => c.radius,
        ShapeGeometry::Hull(h) => 0.25 * h.inner_radius,
        _ => 0.0,
    }
}

/// (b3ComputeConvexManifold)
fn compute_convex_manifold(
    env: &CollideEnv,
    task: &mut TaskContext,
    contact: &mut Contact,
    geom_a: &ShapeGeometry,
    xf_a: WorldTransform,
    geom_b: &ShapeGeometry,
    xf_b: WorldTransform,
) -> bool {
    let type_a = geom_a.shape_type();
    let type_b = geom_b.shape_type();

    let mut geom_manifold = LocalManifold::default();
    let capacity = MAX_MANIFOLD_POINTS as i32;
    let transform_b_to_a = inv_mul_world_transforms(xf_a, xf_b);
    let simd_width = env.simd_width;

    match type_a {
        ShapeType::Sphere => {
            debug_assert!(type_b == ShapeType::Sphere);
            let ShapeGeometry::Sphere(sphere_a) = geom_a else {
                unreachable!()
            };
            let ShapeGeometry::Sphere(sphere_b) = geom_b else {
                unreachable!()
            };
            collide_spheres(
                &mut geom_manifold,
                capacity,
                sphere_a,
                sphere_b,
                transform_b_to_a,
            );
        }
        ShapeType::Capsule => {
            let ShapeGeometry::Capsule(capsule_a) = geom_a else {
                unreachable!()
            };
            if type_b == ShapeType::Sphere {
                let ShapeGeometry::Sphere(sphere_b) = geom_b else {
                    unreachable!()
                };
                collide_capsule_and_sphere(
                    &mut geom_manifold,
                    capacity,
                    capsule_a,
                    sphere_b,
                    transform_b_to_a,
                );
            } else {
                debug_assert!(type_b == ShapeType::Capsule);
                let ShapeGeometry::Capsule(capsule_b) = geom_b else {
                    unreachable!()
                };
                collide_capsules(
                    &mut geom_manifold,
                    capacity,
                    capsule_a,
                    capsule_b,
                    transform_b_to_a,
                );
            }
        }
        ShapeType::Hull => {
            debug_assert!(type_a == ShapeType::Hull);
            let ShapeGeometry::Hull(hull_a) = geom_a else {
                unreachable!()
            };
            let cache = convex_cache(&mut contact.geometry);

            if type_b == ShapeType::Sphere {
                let ShapeGeometry::Sphere(sphere_b) = geom_b else {
                    unreachable!()
                };
                let simplex = ensure_simplex_cache(cache);
                collide_hull_and_sphere(
                    &mut geom_manifold,
                    capacity,
                    hull_a,
                    sphere_b,
                    transform_b_to_a,
                    simplex,
                );
            } else if type_b == ShapeType::Capsule {
                let ShapeGeometry::Capsule(capsule_b) = geom_b else {
                    unreachable!()
                };
                let simplex = ensure_simplex_cache(cache);
                collide_hull_and_capsule(
                    &mut geom_manifold,
                    capacity,
                    hull_a,
                    capsule_b,
                    transform_b_to_a,
                    simplex,
                );
            } else {
                debug_assert!(type_b == ShapeType::Hull);
                let ShapeGeometry::Hull(hull_b) = geom_b else {
                    unreachable!()
                };
                let sat = ensure_sat_cache(cache);
                collide_hulls_at_width(
                    &mut geom_manifold,
                    capacity,
                    hull_a,
                    hull_b,
                    transform_b_to_a,
                    sat,
                    simd_width,
                );
                task.sat_call_count += 1;
                task.sat_cache_hit_count += sat.hit as i32;
            }
        }
        _ => {
            debug_assert!(false, "compute_convex_manifold expects convex types");
            return false;
        }
    }

    if geom_manifold.point_count == 0 {
        contact.manifolds.clear();
        return false;
    }

    let mut old_points: [ManifoldPoint; MAX_MANIFOLD_POINTS];
    let old_count;
    {
        let contact = &mut *contact;
        if contact.manifolds.is_empty() {
            contact.manifolds.push(Manifold::default());
            old_points = [ManifoldPoint::default(); MAX_MANIFOLD_POINTS];
            old_count = 0;
        } else {
            old_count = contact.manifolds[0].point_count;
            old_points = contact.manifolds[0].points;
        }
    }

    let matrix_a = make_matrix_from_quat(xf_a.q);
    {
        let manifold = &mut contact.manifolds[0];
        debug_assert!(
            0 < geom_manifold.point_count
                && geom_manifold.point_count as usize <= MAX_MANIFOLD_POINTS
        );
        manifold.point_count = geom_manifold.point_count;
        manifold.normal = mul_mv(matrix_a, geom_manifold.normal);

        for i in 0..geom_manifold.point_count as usize {
            let source = &geom_manifold.points[i];
            let target = &mut manifold.points[i];
            target.anchor_a = mul_mv(matrix_a, source.point);
            target.anchor_b = add(target.anchor_a, sub_pos(xf_a.p, xf_b.p));
            target.separation = source.separation;
            target.feature_id = make_feature_id(source.pair);
            target.triangle_index = NULL_INDEX;
            target.normal_velocity = 0.0;
        }

        for i in 0..geom_manifold.point_count as usize {
            let pt2_feature = manifold.points[i].feature_id;
            let mut matched_impulse = 0.0;
            let mut persisted = false;

            for old in old_points.iter_mut().take(old_count as usize) {
                if pt2_feature == old.feature_id {
                    matched_impulse = old.normal_impulse;
                    persisted = true;
                    // Claimed — prevent double match (C: featureId = UINT32_MAX).
                    old.feature_id = u32::MAX;
                    break;
                }
            }

            let pt2 = &mut manifold.points[i];
            pt2.total_normal_impulse = 0.0;
            pt2.persisted = persisted;
            pt2.normal_impulse = if persisted { matched_impulse } else { 0.0 };
        }
    }

    // Claim matched old feature ids like C (UINT32_MAX); Rust uses a local copy
    // so claiming is unnecessary beyond the first match break above.

    true
}

/// (b3UpdateConvexContact)
fn update_convex_contact(
    env: &CollideEnv,
    task: &mut TaskContext,
    contact: &mut Contact,
    shape_a: &Shape,
    geom_a: &ShapeGeometry,
    xf_a: WorldTransform,
    shape_b: &Shape,
    geom_b: &ShapeGeometry,
    xf_b: WorldTransform,
    flip: bool,
) -> bool {
    let touching =
        compute_convex_manifold(env, task, contact, geom_a, xf_a, geom_b, xf_b);

    if !touching {
        debug_assert!(contact.manifolds.is_empty());
        return false;
    }

    debug_assert!(contact.manifold_count() == 1);

    if flip {
        let manifold = &mut contact.manifolds[0];
        manifold.normal = neg(manifold.normal);
        for i in 0..manifold.point_count as usize {
            let mp = &mut manifold.points[i];
            std::mem::swap(&mut mp.anchor_a, &mut mp.anchor_b);
        }
    }

    let material_a = shape_a.get_material(0);
    let material_b = shape_b.get_material(0);

    let friction_cb = env.friction_callback
        .unwrap_or(crate::world::default_friction_callback);
    let restitution_cb = env.restitution_callback
        .unwrap_or(crate::world::default_restitution_callback);

    contact.friction = friction_cb(
        material_a.friction,
        material_a.user_material_id,
        material_b.friction,
        material_b.user_material_id,
    );
    contact.restitution = restitution_cb(
        material_a.restitution,
        material_a.user_material_id,
        material_b.restitution,
        material_b.user_material_id,
    );

    if material_a.rolling_resistance > 0.0 || material_b.rolling_resistance > 0.0 {
        let radius_a = shape_rolling_radius(geom_a);
        let radius_b = shape_rolling_radius(geom_b);
        let max_radius = max_float(radius_a, radius_b);
        contact.rolling_resistance =
            max_float(material_a.rolling_resistance, material_b.rolling_resistance) * max_radius;
    } else {
        contact.rolling_resistance = 0.0;
    }

    let tangent_velocity_a = rotate_vector(xf_a.q, material_a.tangent_velocity);
    let tangent_velocity_b = rotate_vector(xf_b.q, material_b.tangent_velocity);
    contact.tangent_velocity =
        sub(tangent_velocity_a, tangent_velocity_b);

    if env.pre_solve_fcn.is_some()
        && (contact.flags & contact_flags::SIM_ENABLE_PRE_SOLVE_EVENTS)
            != 0
    {
        let pre_solve = env.pre_solve_fcn.unwrap();
        let ctx = env.pre_solve_context;
        let world_id = env.world_id;
        let shape_id_a = ShapeId {
            index1: shape_a.id + 1,
            world0: world_id,
            generation: shape_a.generation,
        };
        let shape_id_b = ShapeId {
            index1: shape_b.id + 1,
            world0: world_id,
            generation: shape_b.generation,
        };
        let point = offset_pos(
            xf_a.p,
            contact.manifolds[0].points[0].anchor_a,
        );
        let normal = contact.manifolds[0].normal;
        let still_touching = pre_solve(shape_id_a, shape_id_b, point, normal, ctx);
        if !still_touching {
            contact.manifolds.clear();

            // Invalidate recycling cache.
            contact.flags &= !contact_flags::RELATIVE_TRANSFORM_VALID;
            return false;
        }
    }

    if (shape_a.flags & shape_flags::ENABLE_HIT_EVENTS) != 0
        || (shape_b.flags & shape_flags::ENABLE_HIT_EVENTS) != 0
    {
        contact.flags |= contact_flags::SIM_ENABLE_HIT_EVENT;
    } else {
        contact.flags &= !contact_flags::SIM_ENABLE_HIT_EVENT;
    }

    true
}

/// Update the contact manifold and touching status. (b3UpdateContact)
pub(crate) fn update_contact(
    env: &CollideEnv,
    task: &mut TaskContext,
    contact: &mut Contact,
    shape_id_a: i32,
    local_center_a: crate::math_functions::Vec3,
    xf_a: WorldTransform,
    shape_id_b: i32,
    local_center_b: crate::math_functions::Vec3,
    xf_b: WorldTransform,
    is_fast: bool,
) -> bool {
    debug_assert!(env.shapes[shape_id_b as usize].shape_type() != ShapeType::Compound);

    let type_a = env.shapes[shape_id_a as usize].shape_type();

    let touching = if type_a == ShapeType::Compound {
        update_compound_contact(
            env,
            task,
            contact,
            shape_id_a,
            xf_a,
            shape_id_b,
            xf_b,
            is_fast,
        )
    } else if type_a == ShapeType::Mesh || type_a == ShapeType::Height {
        let shape_a = &env.shapes[shape_id_a as usize];
        let shape_b = &env.shapes[shape_id_b as usize];
        let touching = compute_mesh_manifolds(
            env,
            task,
            contact,
            shape_a,
            None,
            xf_a,
            shape_b,
            xf_b,
            is_fast,
        );
        apply_mesh_hit_flags(contact, shape_a, shape_b);
        debug_assert!(
            (touching && !contact.manifolds.is_empty())
                || (!touching && contact.manifolds.is_empty())
        );
        touching
    } else {
        // Convex vs convex. Primordial Engine fork: the shapes are borrowed from the collide
        // env instead of cloned (the clones only worked around borrowing `World`).
        let shape_a = &env.shapes[shape_id_a as usize];
        let shape_b = &env.shapes[shape_id_b as usize];
        let geom_a = &shape_a.geometry;
        let geom_b = &shape_b.geometry;
        update_convex_contact(
            env,
            task,
            contact,
            shape_a,
            geom_a,
            xf_a,
            shape_b,
            geom_b,
            xf_b,
            false,
        )
    };

    if touching {
        let center_a = rotate_vector(xf_a.q, local_center_a);
        let center_b = rotate_vector(xf_b.q, local_center_b);
        let contact = &mut *contact;
        for manifold in &mut contact.manifolds {
            for j in 0..manifold.point_count as usize {
                let mp = &mut manifold.points[j];
                mp.anchor_a = sub(mp.anchor_a, center_a);
                mp.anchor_b = sub(mp.anchor_b, center_b);
            }
        }
        contact.flags |= contact_flags::SIM_TOUCHING;
    } else {
        contact.flags &= !contact_flags::SIM_TOUCHING;
    }

    touching
}

fn update_compound_contact(
    env: &CollideEnv,
    task: &mut TaskContext,
    contact: &mut Contact,
    shape_id_a: i32,
    xf_a: WorldTransform,
    shape_id_b: i32,
    xf_b: WorldTransform,
    is_fast: bool,
) -> bool {
    use crate::compound::{get_compound_child, ChildGeometry};

    let child_index = contact.child_index;
    let shape_a = &env.shapes[shape_id_a as usize];
    let shape_b = &env.shapes[shape_id_b as usize];
    let geom_b = &shape_b.geometry;
    let type_b = geom_b.shape_type();

    let ShapeGeometry::Compound(compound) = &shape_a.geometry else {
        unreachable!()
    };
    let child = get_compound_child(compound, child_index);
    let child_transform = child.transform;
    let material_indices = child.material_indices;

    // Handle child material for non-meshes. The convex kernels below read the
    // struck child's material, not entry 0 of the compound material table, so
    // build a temporary child shape (clone of the compound) with the child's
    // single material. Matches childShapeA setup in b3UpdateContact. The mesh
    // branch keeps the compound materials and builds its own child shape.
    let child_shape_a = if child.shape_type != ShapeType::Mesh {
        debug_assert!((material_indices[0] as i32) < shape_a.material_count());
        let mut child_shape_a = shape_a.clone();
        child_shape_a.material = shape_a.shape_materials()[material_indices[0] as usize];
        child_shape_a.materials.clear();
        Some(child_shape_a)
    } else {
        None
    };
    let child_shape_a = child_shape_a.as_ref().unwrap_or(&shape_a);

    let touching = match child.geometry {
        ChildGeometry::Capsule(c) => {
            let child_geom = ShapeGeometry::Capsule(c);
            let flip = type_b == ShapeType::Hull;
            if flip {
                update_convex_contact(
                    env,
                    task,
                    contact,
                    &shape_b,
                    &geom_b,
                    xf_b,
                    child_shape_a,
                    &child_geom,
                    xf_a,
                    true,
                )
            } else {
                update_convex_contact(
                    env,
                    task,
                    contact,
                    child_shape_a,
                    &child_geom,
                    xf_a,
                    &shape_b,
                    &geom_b,
                    xf_b,
                    false,
                )
            }
        }
        ChildGeometry::Hull(h) => {
            let child_geom = ShapeGeometry::Hull(Arc::new(h.clone()));
            let xf_child = mul_world_transforms(xf_a, child_transform);
            update_convex_contact(
                env,
                task,
                contact,
                child_shape_a,
                &child_geom,
                xf_child,
                &shape_b,
                &geom_b,
                xf_b,
                false,
            )
        }
        ChildGeometry::Sphere(s) => {
            let child_geom = ShapeGeometry::Sphere(s);
            let flip = type_b == ShapeType::Capsule || type_b == ShapeType::Hull;
            if flip {
                update_convex_contact(
                    env,
                    task,
                    contact,
                    &shape_b,
                    &geom_b,
                    xf_b,
                    child_shape_a,
                    &child_geom,
                    xf_a,
                    true,
                )
            } else {
                update_convex_contact(
                    env,
                    task,
                    contact,
                    child_shape_a,
                    &child_geom,
                    xf_a,
                    &shape_b,
                    &geom_b,
                    xf_b,
                    false,
                )
            }
        }
        ChildGeometry::Mesh(mesh) => {
            let mut child_shape = shape_a.clone();
            child_shape.geometry = ShapeGeometry::Mesh {
                data: mesh.data.clone(),
                scale: mesh.scale,
            };
            let xf_child = mul_world_transforms(xf_a, child_transform);
            let touching = compute_mesh_manifolds(
                env,
                task,
                contact,
                &child_shape,
                Some(material_indices),
                xf_child,
                &shape_b,
                xf_b,
                is_fast,
            );
            apply_mesh_hit_flags(contact, &shape_a, &shape_b);
            debug_assert!(
                (touching && !contact.manifolds.is_empty())
                    || (!touching && contact.manifolds.is_empty())
            );
            touching
        }
    };

    if touching {
        let offset = rotate_vector(xf_a.q, child_transform.p);
        let contact = &mut *contact;
        for manifold in &mut contact.manifolds {
            for j in 0..manifold.point_count as usize {
                manifold.points[j].anchor_a = add(manifold.points[j].anchor_a, offset);
            }
        }
    }

    touching
}

/// Test helper with the pre-fork signature: [`update_contact`] on a contact of `world`.
#[cfg(test)]
pub(crate) fn update_contact_in_world(
    world: &mut crate::world::World,
    worker_index: i32,
    contact_id: i32,
    shape_id_a: i32,
    local_center_a: crate::math_functions::Vec3,
    xf_a: WorldTransform,
    shape_id_b: i32,
    local_center_b: crate::math_functions::Vec3,
    xf_b: WorldTransform,
    is_fast: bool,
) -> bool {
    let env = CollideEnv {
        shapes: &world.shapes,
        simd_width: world.simd_width,
        friction_callback: world.friction_callback,
        restitution_callback: world.restitution_callback,
        pre_solve_fcn: world.pre_solve_fcn,
        pre_solve_context: world.pre_solve_context,
        world_id: world.world_id,
    };
    update_contact(
        &env,
        &mut world.task_contexts[worker_index as usize],
        &mut world.contacts[contact_id as usize],
        shape_id_a,
        local_center_a,
        xf_a,
        shape_id_b,
        local_center_b,
        xf_b,
        is_fast,
    )
}
