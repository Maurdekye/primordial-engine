// Body-level query / cast / overlap API from body.c.
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::lifecycle::{get_body_full_id, get_body_sim};
use super::types::{BodyCastResult, BodyToiResult};
use crate::core::NULL_INDEX;
use crate::distance::{make_proxy, time_of_impact, ShapeProxy, Sweep, ToiInput, ToiState};
use crate::geometry::{Capsule, RayCastInput, ShapeCastInput};
use crate::id::{BodyId, ShapeId};
use crate::math_functions::{
    clamp_int, offset_pos, to_relative_transform, transform_point, Pos, Vec3, WorldTransform,
    QUAT_IDENTITY, VEC3_ZERO,
};
use crate::shape::{
    make_shape_proxy, overlap_shape, ray_cast_shape, shape_cast_shape, should_query_collide,
    ShapeGeometry,
};
use crate::types::QueryFilter;
use crate::world::World;

/// Cast a ray against a body's shapes. Results are in world space relative to
/// `origin`; `body_transform` (not the body's stored pose) drives the geometry.
/// Ignores back-side collision on meshes and height-fields.
/// (b3Body_CastRay)
pub fn body_cast_ray(
    world: &World,
    body_id: BodyId,
    origin: Pos,
    translation: Vec3,
    filter: &QueryFilter,
    max_fraction: f32,
    body_transform: WorldTransform,
) -> BodyCastResult {
    debug_assert!(!world.locked);
    if world.locked {
        return BodyCastResult::default();
    }

    let mut result = BodyCastResult::default();
    let body_index = get_body_full_id(world, body_id);

    // The consistent framing is to center on the ray origin.
    let mut shape_input = RayCastInput {
        origin: VEC3_ZERO,
        translation,
        max_fraction,
    };

    let transform = to_relative_transform(body_transform, origin);

    let mut shape_id = world.bodies[body_index as usize].head_shape_id;
    while shape_id != NULL_INDEX {
        let shape = &world.shapes[shape_id as usize];
        // Careful with id: advance before using shape fields below.
        shape_id = shape.next_shape_id;

        if !should_query_collide(&shape.filter, filter) {
            continue;
        }

        let shape_output = ray_cast_shape(shape, transform, &shape_input);

        if !shape_output.hit {
            continue;
        }

        if shape_output.fraction > shape_input.max_fraction {
            continue;
        }

        let id = ShapeId {
            index1: shape.id + 1,
            world0: body_id.world0,
            generation: shape.generation,
        };

        let material_index = clamp_int(shape_output.material_index, 0, shape.material_count() - 1);
        let user_material_id = shape.shape_materials()[material_index as usize].user_material_id;

        result = BodyCastResult {
            shape_id: id,
            point: offset_pos(origin, shape_output.point),
            normal: shape_output.normal,
            fraction: shape_output.fraction,
            triangle_index: shape_output.triangle_index,
            user_material_id,
            iterations: shape_output.iterations,
            hit: true,
        };

        shape_input.max_fraction = shape_output.fraction;
    }

    result
}

/// Cast a shape proxy against a body's shapes. Ignores back-side collision on meshes and height-fields.
/// (b3Body_CastShape)
pub fn body_cast_shape(
    world: &World,
    body_id: BodyId,
    origin: Pos,
    proxy: &ShapeProxy,
    translation: Vec3,
    filter: &QueryFilter,
    max_fraction: f32,
    can_encroach: bool,
    body_transform: WorldTransform,
) -> BodyCastResult {
    debug_assert!(!world.locked);
    if world.locked {
        return BodyCastResult::default();
    }

    let mut result = BodyCastResult::default();
    let body_index = get_body_full_id(world, body_id);

    let transform = to_relative_transform(body_transform, origin);

    let mut shape_input = ShapeCastInput {
        proxy: *proxy,
        translation,
        max_fraction,
        can_encroach,
    };

    let mut shape_id = world.bodies[body_index as usize].head_shape_id;
    while shape_id != NULL_INDEX {
        let shape = &world.shapes[shape_id as usize];
        shape_id = shape.next_shape_id;

        if !should_query_collide(&shape.filter, filter) {
            continue;
        }

        let shape_output = shape_cast_shape(shape, transform, &shape_input);

        if !shape_output.hit {
            continue;
        }

        if shape_output.fraction > shape_input.max_fraction {
            continue;
        }

        let id = ShapeId {
            index1: shape.id + 1,
            world0: body_id.world0,
            generation: shape.generation,
        };
        let material_index = clamp_int(shape_output.material_index, 0, shape.material_count() - 1);
        let user_material_id = shape.shape_materials()[material_index as usize].user_material_id;

        result = BodyCastResult {
            shape_id: id,
            point: offset_pos(origin, shape_output.point),
            normal: shape_output.normal,
            fraction: shape_output.fraction,
            triangle_index: shape_output.triangle_index,
            user_material_id,
            iterations: shape_output.iterations,
            hit: true,
        };

        shape_input.max_fraction = shape_output.fraction;
    }

    result
}

/// Test whether a shape proxy overlaps any of a body's shapes. (b3Body_OverlapShape)
pub fn body_overlap_shape(
    world: &World,
    body_id: BodyId,
    origin: Pos,
    proxy: &ShapeProxy,
    filter: &QueryFilter,
    body_transform: WorldTransform,
) -> bool {
    debug_assert!(!world.locked);
    if world.locked {
        return false;
    }

    let body_index = get_body_full_id(world, body_id);
    let transform = to_relative_transform(body_transform, origin);

    let mut shape_id = world.bodies[body_index as usize].head_shape_id;
    while shape_id != NULL_INDEX {
        let shape = &world.shapes[shape_id as usize];
        shape_id = shape.next_shape_id;

        if !should_query_collide(&shape.filter, filter) {
            continue;
        }

        if overlap_shape(shape, transform, proxy) {
            return true;
        }
    }

    false
}

/// Perform a time of impact between a character mover and a body using specified
/// sweep transforms. Initial overlap of any shape on the body is ignored. A
/// non-overlapped shape can still be hit. Only considers convex shapes on the
/// body. (b3Body_TimeOfImpactMover)
pub fn body_time_of_impact_mover(
    world: &World,
    body_id: BodyId,
    origin: Pos,
    mover: &Capsule,
    mover_translation: Vec3,
    filter: &QueryFilter,
    body_transform1: WorldTransform,
    body_transform2: WorldTransform,
) -> BodyToiResult {
    let mut result = BodyToiResult {
        fraction: 1.0,
        ..Default::default()
    };

    debug_assert!(!world.locked);
    if world.locked {
        return result;
    }

    let xf1 = to_relative_transform(body_transform1, origin);
    let xf2 = to_relative_transform(body_transform2, origin);

    let body_index = get_body_full_id(world, body_id);
    let body_sim = get_body_sim(world, body_index);
    let local_center = body_sim.local_center;

    let capsule_points = [mover.center1, mover.center2];
    let mut input = ToiInput {
        proxy_a: ShapeProxy::default(),
        proxy_b: make_proxy(&capsule_points, mover.radius),
        sweep_a: Sweep {
            local_center,
            c1: transform_point(xf1, local_center),
            c2: transform_point(xf2, local_center),
            q1: body_transform1.q,
            q2: body_transform2.q,
        },
        sweep_b: Sweep {
            local_center: VEC3_ZERO,
            c1: VEC3_ZERO,
            c2: mover_translation,
            q1: QUAT_IDENTITY,
            q2: QUAT_IDENTITY,
        },
        max_fraction: 1.0,
    };

    let mut shape_id = world.bodies[body_index as usize].head_shape_id;
    while shape_id != NULL_INDEX {
        let shape = &world.shapes[shape_id as usize];
        shape_id = shape.next_shape_id;

        if !should_query_collide(&shape.filter, filter) {
            continue;
        }

        match &shape.geometry {
            ShapeGeometry::Sphere(_) | ShapeGeometry::Capsule(_) | ShapeGeometry::Hull(_) => {}
            _ => continue,
        }

        input.proxy_a = make_shape_proxy(shape);

        let output = time_of_impact(&input);
        debug_assert!(output.state != ToiState::Unknown);

        // Mimic behavior in b3ContinuousQueryCallback. Ignore shapes that initially overlap.
        if 0.0 < output.fraction && output.fraction < result.fraction {
            input.max_fraction = output.fraction;

            result.point = offset_pos(origin, output.point);
            result.normal = output.normal;
            result.fraction = output.fraction;
            result.shape_id = ShapeId {
                index1: shape.id + 1,
                world0: body_id.world0,
                generation: shape.generation,
            };
        }
    }

    result
}
