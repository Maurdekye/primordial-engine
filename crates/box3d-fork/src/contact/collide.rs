// Narrow-phase collide pass and contact state machine from physics_world.c.
//
// Serial path only (worker_count == 1).
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::contact_flags;
use super::{destroy_contact, update_contact, CollideEnv, Contact};
use crate::bitset::BitSet;
use crate::body::{body_flags, is_static_sim_index, BodySim};
use crate::constants::{
    speculative_distance, CONTACT_MANIFOLD_COUNT_BUCKETS, CONTACT_RECYCLE_ANGULAR_DISTANCE,
    GRAPH_COLOR_COUNT,
};
use crate::constraint_graph::{add_contact_to_graph, remove_contact_from_graph};
use crate::core::{ctz64, NULL_INDEX};
use crate::dynamic_tree::overlap_v;
use crate::events::{ContactBeginTouchEvent, ContactEndTouchEvent};
use crate::id::{ContactId, ShapeId};
use crate::island::{link_contact, unlink_contact};
use crate::math_functions::{
    abs, conjugate, distance_squared, dot, dot_quat, inv_mul_quat, inv_mul_world_transforms,
    length_squared, make_matrix_from_quat, max, max_float, min_float, min_int, mul_mv, mul_quat,
    sub, sub_pos, Vec3,
};
use crate::recycle_scale::{
    default_recycle_radius, ContactRecycleRadiusFn, ContactRecycleScaling, RecyclePairInput,
};
use crate::body::Body;
use crate::math_functions::Aabb;
use crate::solver_set::{SolverSet, AWAKE_SET, STATIC_SET};
use crate::worker_pool::{BlockCounter, SharedMut};
use crate::world::{TaskContext, World};

/// Primordial Engine fork: contacts per block claimed by a worker in the collide pass.
const COLLIDE_BLOCK_SIZE: usize = 32;

/// Modified cross used by contact recycling (simd.h b3ModifiedCross scalar).
fn modified_cross(a: Vec3, b: Vec3) -> Vec3 {
    Vec3 {
        x: a.y * b.z + a.z * b.y,
        y: a.z * b.x + a.x * b.z,
        z: a.x * b.y + a.y * b.x,
    }
}

/// (b3AddNonTouchingContact)
fn add_non_touching_contact(world: &mut World, contact_id: i32) {
    debug_assert!(world.contacts[contact_id as usize].set_index == AWAKE_SET);
    let local_index = world.solver_sets[AWAKE_SET as usize].contact_indices.len() as i32;
    {
        let contact = &mut world.contacts[contact_id as usize];
        contact.color_index = NULL_INDEX;
        contact.local_index = local_index;
    }
    world.solver_sets[AWAKE_SET as usize]
        .contact_indices
        .push(contact_id);
}

/// Resolve a contact body sim from the encoded locator. Awake and static bodies are found
/// directly; any other set (a non-touching contact against a sleeping body) goes through the
/// body. Returns a copy because the collide task only reads the sim.
/// (b3ResolveContactBodySim)
fn resolve_contact_body_sim(
    solver_sets: &[SolverSet],
    bodies: &[Body],
    encoded_index: i32,
    body_id: i32,
) -> BodySim {
    if encoded_index >= 0 {
        return solver_sets[AWAKE_SET as usize].body_sims[encoded_index as usize];
    }

    if is_static_sim_index(encoded_index) {
        return solver_sets[STATIC_SET as usize].body_sims[(-encoded_index - 2) as usize];
    }

    let body = &bodies[body_id as usize];
    solver_sets[body.set_index as usize].body_sims[body.local_index as usize]
}

/// (b3RemoveNonTouchingContact)
fn remove_non_touching_contact(world: &mut World, set_index: i32, local_index: i32) {
    let set = &mut world.solver_sets[set_index as usize];
    let moved_index = set.contact_indices.len() as i32 - 1;
    set.contact_indices.swap_remove(local_index as usize);
    if moved_index != local_index {
        let moved_contact_id = set.contact_indices[local_index as usize];
        let moved = &mut world.contacts[moved_contact_id as usize];
        debug_assert!(moved.set_index == set_index);
        debug_assert!(moved.color_index == NULL_INDEX);
        debug_assert!(moved.local_index == moved_index);
        moved.local_index = local_index;
    }
}

/// Read-only inputs of the collide pass. Primordial Engine fork: split out of `World` so that
/// blocks of contacts can be updated on worker threads.
struct CollideTask<'a> {
    env: CollideEnv<'a>,
    fat_aabbs: &'a [Aabb],
    solver_sets: &'a [SolverSet],
    bodies: &'a [Body],
    contact_count: usize,
    recycle_distance: f32,
    recycle_scaling: ContactRecycleScaling,
    recycle_radius_fcn: ContactRecycleRadiusFn,
}

/// Per-contact narrow phase over one block of `contact_indices`. (b3CollideTask)
///
/// Writes only the listed contacts, `task`, and `mesh_counts` (the graph-color manifold counts
/// of mesh contacts, applied by the caller).
///
/// # Safety
/// The indices are distinct contact ids below `t.contact_count`, `contacts` points at the
/// world's contact array, and no other thread touches these contacts during the call.
unsafe fn collide_task(
    t: &CollideTask,
    contacts: SharedMut<Contact>,
    contact_indices: &[i32],
    task: &mut TaskContext,
    mesh_counts: &mut Vec<(i32, i32, u16)>,
) {
    let recycle_distance = t.recycle_distance;
    let speculative = speculative_distance();
    let recycle_distance_non_touching = min_float(recycle_distance, speculative);
    // Primordial Engine fork: scale-dependent recycling radius (crate::recycle_scale).
    let recycle_scaling = t.recycle_scaling;
    let recycle_radius_fcn = t.recycle_radius_fcn;

    for &contact_index in contact_indices {
        debug_assert!((contact_index as usize) < t.contact_count);
        // SAFETY: the function contract.
        let contact = unsafe { contacts.get_mut(contact_index as usize) };
        debug_assert!(contact.contact_id == contact_index);

        let shape_id_a = contact.shape_id_a;
        let shape_id_b = contact.shape_id_b;

        // Do proxies still overlap?
        let overlap = overlap_v(
            &t.fat_aabbs[shape_id_a as usize],
            &t.fat_aabbs[shape_id_b as usize],
        );
        if !overlap {
            contact.flags |= contact_flags::SIM_DISJOINT;
            contact.flags &= !contact_flags::SIM_TOUCHING;
            task
                .contact_state_bit_set
                .set_bit(contact_index as u32);
            continue;
        }

        // Update contact respecting shape/body order (A,B)
        let encoded_a = contact.encoded_body_sim_a;
        let encoded_b = contact.encoded_body_sim_b;
        let sim_a = resolve_contact_body_sim(
            t.solver_sets,
            t.bodies,
            encoded_a,
            contact.edges[0].body_id,
        );
        let sim_b = resolve_contact_body_sim(
            t.solver_sets,
            t.bodies,
            encoded_b,
            contact.edges[1].body_id,
        );

        let was_touching =
            (contact.flags & contact_flags::SIM_TOUCHING) != 0;

        let transform_a = sim_a.transform;
        let transform_b = sim_b.transform;
        let is_fast = ((sim_a.flags | sim_b.flags) & body_flags::IS_FAST) != 0;

        let recycle_tolerance = if was_touching {
            recycle_distance
        } else {
            recycle_distance_non_touching
        };

        let flags = contact.flags;
        if !is_fast
            && recycle_distance > 0.0
            && (flags & contact_flags::RELATIVE_TRANSFORM_VALID) != 0
            && (flags & contact_flags::RECYCLE) != 0
        {
            // Primordial Engine fork: with scaling enabled the tolerance is per pair; with it
            // disabled this block is skipped and the upstream value above is used unchanged.
            let recycle_tolerance = if recycle_scaling.enabled {
                let input = RecyclePairInput {
                    length_a: sim_a.min_extent,
                    length_b: sim_b.min_extent,
                    static_a: is_static_sim_index(encoded_a),
                    static_b: is_static_sim_index(encoded_b),
                    base_distance: recycle_distance,
                };
                // max_float also maps NaN to 0, which disables recycling for the pair.
                let r = max_float(recycle_radius_fcn(&input, &recycle_scaling), 0.0);
                if was_touching {
                    r
                } else {
                    min_float(r, speculative)
                }
            } else {
                recycle_tolerance
            };
            // The scalar part of b3InvMulQuat is just the quaternion dot product.
            // cos(relative_angle/2) = scalar(conj(q1) * q2) = dot(q1, q2)
            // A small relative angle means this value is close to 1. Need to use abs or square
            // due to double cover.
            let angle_a = dot_quat(transform_a.q, contact.cached_rotation_a);
            let angle_b = dot_quat(transform_b.q, contact.cached_rotation_b);
            let angular_distance = min_float(angle_a * angle_a, angle_b * angle_b);

            let xf = inv_mul_world_transforms(transform_a, transform_b);
            let xfc = contact.cached_relative_pose;
            let max_extent_a = if is_static_sim_index(encoded_a) {
                Vec3 {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                }
            } else {
                sim_a.max_extent
            };
            let max_extent_b = if is_static_sim_index(encoded_b) {
                Vec3 {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                }
            } else {
                sim_b.max_extent
            };
            let max_extent = max(max_extent_a, max_extent_b);
            let dist_squared = distance_squared(xf.p, xfc.p);

            if angular_distance > CONTACT_RECYCLE_ANGULAR_DISTANCE
                && dist_squared < recycle_tolerance * recycle_tolerance
            {
                let distance = dist_squared.sqrt();
                let slack = recycle_tolerance - distance;
                let qr = inv_mul_quat(xfc.q, xf.q);
                let arc = modified_cross(abs(qr.v), max_extent);
                let arc_sq = 4.0 * length_squared(arc);
                if arc_sq < slack * slack {
                    let dq_a = mul_quat(transform_a.q, conjugate(contact.cached_rotation_a));
                    let dq_b = mul_quat(transform_b.q, conjugate(contact.cached_rotation_b));
                    let matrix_a = make_matrix_from_quat(dq_a);
                    let matrix_b = make_matrix_from_quat(dq_b);
                    let dc = sub_pos(sim_b.center, sim_a.center);

                    let manifold_count = contact.manifold_count();
                    for manifold_index in 0..manifold_count as usize {
                        let normal =
                            contact.manifolds[manifold_index].normal;
                        let point_count = contact.manifolds
                            [manifold_index]
                            .point_count;
                        for point_index in 0..point_count as usize {
                            let mp = &mut contact.manifolds
                                [manifold_index]
                                .points[point_index];
                            let r_a = mul_mv(matrix_a, mp.anchor_a);
                            let r_b = mul_mv(matrix_b, mp.anchor_b);
                            let dp = crate::math_functions::add(dc, sub(r_b, r_a));
                            mp.separation = mp.base_separation + dot(dp, normal);
                            mp.normal_velocity = 0.0;
                            mp.persisted = true;
                        }
                    }

                    task.recycled_contact_count += 1;
                    let bucket_index =
                        min_int(manifold_count, CONTACT_MANIFOLD_COUNT_BUCKETS as i32 - 1);
                    if bucket_index > 0 {
                        task.manifold_counts
                            [(bucket_index - 1) as usize] += 1;
                    }
                    continue;
                }
            }
        }

        {
            let contact = &mut *contact;
            contact.cached_rotation_a = transform_a.q;
            contact.cached_rotation_b = transform_b.q;
            contact.cached_relative_pose = inv_mul_world_transforms(transform_a, transform_b);
            contact.flags |= contact_flags::RELATIVE_TRANSFORM_VALID;
        }

        let touching = update_contact(
            &t.env,
            task,
            contact,
            shape_id_a,
            sim_a.local_center,
            transform_a,
            shape_id_b,
            sim_b.local_center,
            transform_b,
            is_fast,
        );

        let manifold_count = contact.manifold_count();
        let bucket_index = min_int(manifold_count, CONTACT_MANIFOLD_COUNT_BUCKETS as i32 - 1);
        if bucket_index > 0 {
            task.manifold_counts
                [(bucket_index - 1) as usize] += 1;
        }

        if touching
            && was_touching
            && (contact.flags & contact_flags::SIM_MESH_CONTACT) != 0
        {
            let color_index = contact.color_index;
            let local_index = contact.local_index;
            debug_assert!(color_index != NULL_INDEX);
            debug_assert!(0 <= color_index && color_index < GRAPH_COLOR_COUNT);
            mesh_counts.push((color_index, local_index, manifold_count as u16));
        }

        if touching && !was_touching {
            contact.flags |= contact_flags::SIM_STARTED_TOUCHING;
            task
                .contact_state_bit_set
                .set_bit(contact_index as u32);
        } else if !touching && was_touching {
            contact.flags |= contact_flags::SIM_STOPPED_TOUCHING;
            task
                .contact_state_bit_set
                .set_bit(contact_index as u32);
        }

        if touching {
            let manifold_count = contact.manifold_count();
            for manifold_index in 0..manifold_count as usize {
                let point_count =
                    contact.manifolds[manifold_index].point_count;
                for point_index in 0..point_count as usize {
                    // Cache separation
                    let mp = &mut contact.manifolds[manifold_index]
                        .points[point_index];
                    mp.base_separation = mp.separation;
                }
            }
        }
    }
}

/// Narrow-phase collision. (b3Collide)
pub fn collide(world: &mut World, dt: f32) {
    debug_assert!(world.worker_count > 0);

    let mut touching_count = 0;
    for i in 0..GRAPH_COLOR_COUNT as usize {
        let color = &world.constraint_graph.colors[i];
        touching_count += color.convex_contacts.len() + color.contacts.len();
    }

    let non_touching_count = world.solver_sets[AWAKE_SET as usize].contact_indices.len();
    let contact_count = touching_count + non_touching_count;
    if contact_count == 0 {
        return;
    }

    let mut contact_indices = Vec::with_capacity(contact_count);
    for i in 0..GRAPH_COLOR_COUNT as usize {
        let color = &world.constraint_graph.colors[i];
        contact_indices.extend_from_slice(&color.convex_contacts);
        for spec in &color.contacts {
            contact_indices.push(spec.contact_id);
        }
    }
    debug_assert!(contact_indices.len() == touching_count);
    if non_touching_count > 0 {
        contact_indices.extend_from_slice(&world.solver_sets[AWAKE_SET as usize].contact_indices);
    }

    let contact_id_capacity = world.contact_id_pool.id_capacity();
    for task in &mut world.task_contexts {
        task.contact_state_bit_set
            .set_bit_count_and_clear(contact_id_capacity as u32);
        task.sat_call_count = 0;
        task.sat_cache_hit_count = 0;
        task.recycled_contact_count = 0;
        task.manifold_counts = [0; CONTACT_MANIFOLD_COUNT_BUCKETS];
    }

    // Primordial Engine fork: blocks of contacts run on the world's worker threads when it has
    // them. Each contact is written by exactly one block, the per-worker counters are summed and
    // the per-worker bit sets ORed below, so the result does not depend on the worker count or
    // on which worker ran which block.
    let worker_count = world.task_contexts.len();
    let mut mesh_counts: Vec<Vec<(i32, i32, u16)>> = vec![Vec::new(); worker_count];
    {
        let pool = world.worker_pool.clone();
        let contact_recycle_radius_fcn = world.contact_recycle_radius_fcn;
        let World {
            contacts,
            shapes,
            fat_aabbs,
            solver_sets,
            bodies,
            task_contexts,
            simd_width,
            friction_callback,
            restitution_callback,
            pre_solve_fcn,
            pre_solve_context,
            world_id,
            contact_recycle_distance,
            contact_recycle_scaling,
            ..
        } = world;
        let t = CollideTask {
            env: CollideEnv {
                shapes,
                simd_width: *simd_width,
                friction_callback: *friction_callback,
                restitution_callback: *restitution_callback,
                pre_solve_fcn: *pre_solve_fcn,
                pre_solve_context: *pre_solve_context,
                world_id: *world_id,
            },
            fat_aabbs,
            solver_sets,
            bodies,
            contact_count: contacts.len(),
            recycle_distance: *contact_recycle_distance,
            recycle_scaling: *contact_recycle_scaling,
            recycle_radius_fcn: contact_recycle_radius_fcn.unwrap_or(default_recycle_radius),
        };
        let contacts = SharedMut::new(contacts);
        match pool {
            Some(pool) if contact_indices.len() > COLLIDE_BLOCK_SIZE => {
                debug_assert!(pool.worker_count() == worker_count);
                let blocks = BlockCounter::new(contact_indices.len(), COLLIDE_BLOCK_SIZE);
                let tasks = SharedMut::new(task_contexts);
                let meshes = SharedMut::new(&mut mesh_counts);
                let contact_indices = &contact_indices;
                pool.run(&|worker_index| {
                    // SAFETY: each worker index is used by exactly one thread; each block of
                    // distinct contact ids is claimed by exactly one worker.
                    let task = unsafe { tasks.get_mut(worker_index) };
                    let mesh = unsafe { meshes.get_mut(worker_index) };
                    while let Some(range) = blocks.claim() {
                        unsafe { collide_task(&t, contacts, &contact_indices[range], task, mesh) };
                    }
                });
            }
            _ => {
                // SAFETY: single thread; the contact ids are distinct.
                unsafe {
                    collide_task(&t, contacts, &contact_indices, &mut task_contexts[0], &mut mesh_counts[0])
                };
            }
        }
    }
    for counts in &mesh_counts {
        for &(color_index, local_index, manifold_count) in counts {
            world.constraint_graph.colors[color_index as usize].contacts[local_index as usize]
                .manifold_count = manifold_count;
        }
    }

    // C scales only worker 0's SAT counts by satMultiplier; the fork scales the total so the
    // counters do not depend on the worker count either.
    let sat_multiplier = if dt > 0.0 { 1 } else { 0 };
    let mut sat_call_count = 0;
    let mut sat_cache_hit_count = 0;
    let mut manifold_counts = [0; CONTACT_MANIFOLD_COUNT_BUCKETS];
    for task in &world.task_contexts {
        sat_call_count += task.sat_call_count;
        sat_cache_hit_count += task.sat_cache_hit_count;
        for (total, count) in manifold_counts.iter_mut().zip(task.manifold_counts) {
            *total += count;
        }
    }
    world.sat_call_count = sat_multiplier * sat_call_count;
    world.sat_cache_hit_count = sat_multiplier * sat_cache_hit_count;
    world.manifold_counts = manifold_counts;

    // Union worker bitsets.
    let mut bit_set: BitSet = world.task_contexts[0].contact_state_bit_set.clone();
    for task in &world.task_contexts[1..] {
        bit_set.in_place_union(&task.contact_state_bit_set);
    }
    let end_event_array_index = world.end_event_array_index;
    let world_id = world.world_id;

    for k in 0..bit_set.block_count() {
        let mut bits = bit_set.block(k);
        while bits != 0 {
            let ctz = ctz64(bits);
            let contact_id = (64 * k + ctz) as i32;

            let (shape_id_a, shape_id_b, flags, generation) = {
                let contact = &world.contacts[contact_id as usize];
                debug_assert!(contact.set_index == AWAKE_SET);
                (
                    contact.shape_id_a,
                    contact.shape_id_b,
                    contact.flags,
                    contact.generation,
                )
            };

            let shape_a = &world.shapes[shape_id_a as usize];
            let shape_b = &world.shapes[shape_id_b as usize];
            let shape_id_a_full = ShapeId {
                index1: shape_a.id + 1,
                world0: world_id,
                generation: shape_a.generation,
            };
            let shape_id_b_full = ShapeId {
                index1: shape_b.id + 1,
                world0: world_id,
                generation: shape_b.generation,
            };
            let contact_full_id = ContactId {
                index1: contact_id + 1,
                world0: world_id,
                padding: 0,
                generation,
            };

            if (flags & contact_flags::SIM_DISJOINT) != 0 {
                destroy_contact(world, contact_id, false);
            } else if (flags & contact_flags::SIM_STARTED_TOUCHING) != 0 {
                debug_assert!(world.contacts[contact_id as usize].island_id == NULL_INDEX);

                if (flags & contact_flags::ENABLE_CONTACT_EVENTS) != 0 {
                    world.contact_begin_events.push(ContactBeginTouchEvent {
                        shape_id_a: shape_id_a_full,
                        shape_id_b: shape_id_b_full,
                        contact_id: contact_full_id,
                    });
                }

                debug_assert!(world.contacts[contact_id as usize].manifold_count() > 0);
                debug_assert!(world.contacts[contact_id as usize].set_index == AWAKE_SET);

                world.contacts[contact_id as usize].flags &= !contact_flags::SIM_STARTED_TOUCHING;
                world.contacts[contact_id as usize].flags |= contact_flags::TOUCHING;
                link_contact(world, contact_id);

                debug_assert!(world.contacts[contact_id as usize].color_index == NULL_INDEX);

                let old_local_index = world.contacts[contact_id as usize].local_index;
                add_contact_to_graph(world, contact_id);
                remove_non_touching_contact(world, AWAKE_SET, old_local_index);
            } else if (flags & contact_flags::SIM_STOPPED_TOUCHING) != 0 {
                world.contacts[contact_id as usize].flags &= !contact_flags::SIM_STOPPED_TOUCHING;
                world.contacts[contact_id as usize].flags &= !contact_flags::TOUCHING;

                if (world.contacts[contact_id as usize].flags
                    & contact_flags::ENABLE_CONTACT_EVENTS)
                    != 0
                {
                    world.contact_end_events[end_event_array_index as usize].push(
                        ContactEndTouchEvent {
                            shape_id_a: shape_id_a_full,
                            shape_id_b: shape_id_b_full,
                            contact_id: contact_full_id,
                        },
                    );
                }

                debug_assert!(world.contacts[contact_id as usize].manifolds.is_empty());

                let color_index = world.contacts[contact_id as usize].color_index;
                let local_index = world.contacts[contact_id as usize].local_index;
                let body_id_a = world.contacts[contact_id as usize].edges[0].body_id;
                let body_id_b = world.contacts[contact_id as usize].edges[1].body_id;
                let is_mesh = (world.contacts[contact_id as usize].flags
                    & contact_flags::SIM_MESH_CONTACT)
                    != 0;

                unlink_contact(world, contact_id);
                add_non_touching_contact(world, contact_id);
                remove_contact_from_graph(
                    world,
                    body_id_a,
                    body_id_b,
                    color_index,
                    local_index,
                    is_mesh,
                );
            }

            bits &= bits - 1;
        }
    }

    world.validate_solver_sets();
}
