//! Solve driver from solver.c: island split → prepare → sub-step loop (push, relax
//! with deferred restitution) → store → finalize → joint events → hit events →
//! broad-phase enlarge → island sleep.
//!
//! SPDX-FileCopyrightText: 2025 Erin Catto
//! SPDX-License-Identifier: MIT
#![allow(clippy::needless_range_loop)]

use super::continuous::solve_continuous;
use super::integrate::{finalize_bodies, integrate_positions_block, integrate_velocities_block};
use super::mesh_lanes::{compute_mesh_lanes, MeshLanes};
use super::stages::{ParallelStages, SerialStages, StageSync, Stages};
use super::StepContext;
use crate::body::body_flags;
use crate::broad_phase::{proxy_id, proxy_type};
use crate::constants::{GRAPH_COLOR_COUNT, RELAX_ITERATIONS, SOLVER_ITERATIONS};
use crate::constraint_graph::OVERFLOW_INDEX;
use crate::contact_solver::wide::{
    allocate_contacts_convex, allocate_contacts_mesh_wide, apply_restitution_w_range,
    prepare_contacts_convex_range, prepare_contacts_mesh_wide_range, push_contacts_w_range, solve_contacts_w_range, store_impulses_w_range,
    warm_start_contacts_w_range, StatesView, WideContactConstraints, WideView,
};
use crate::contact_solver::{
    apply_restitution, flag_hit_events, prepare_mesh_contacts, push_contacts, solve_contacts,
    store_impulses, warm_start_contacts, ContactConstraint,
};
use crate::core::NULL_INDEX;
use crate::events::{BodyMoveEvent, JointEvent};
use crate::id::{BodyId, JointId};
use crate::island::split_island;
use crate::joint::{get_joint_reaction, prepare_joint, solve_joint, warm_start_joint};
use crate::math_functions::{aabb_contains, WORLD_TRANSFORM_IDENTITY};
use crate::solver_set::{try_sleep_island, AWAKE_SET};
use crate::types::BodyType;
use crate::worker_pool::{SharedMut, SharedSlice};
use crate::world::World;
use std::ops::Range;

/// Primordial Engine fork (D2): solver stage block sizes, in bodies and in wide constraints
/// (four contacts each), and the awake body count below which the solve stays serial.
const BODY_BLOCK_SIZE: usize = 128;
const CONSTRAINT_BLOCK_SIZE: usize = 8;
const PARALLEL_SOLVE_MIN_BODIES: usize = 64;
/// Wide constraints per block of the prepare stage.
const PREPARE_BLOCK_SIZE: usize = 4;

/// The views a color stage works on.
struct ColorViews {
    convex: Vec<WideView>,
    mesh: Vec<WideView>,
}

/// Run color `color_index`'s convex then mesh wide constraints as one stage: items
/// `0..convex` are convex constraints, the rest mesh constraints. Within a color no two
/// constraints share a dynamic body, so the blocks are independent.
/// (b3_graphWideContactBlock / b3_graphContactBlock blocks of a color stage)
fn color_stage(
    stages: &dyn Stages,
    convex_count: usize,
    mesh_count: usize,
    block: &(dyn Fn(bool, Range<usize>, usize) + Sync),
) {
    stages.run(convex_count + mesh_count, CONSTRAINT_BLOCK_SIZE, &|items, worker| {
        if items.start < convex_count {
            block(false, items.start..items.end.min(convex_count), worker);
        }
        if items.end > convex_count {
            block(
                true,
                items.start.max(convex_count) - convex_count..items.end - convex_count,
                worker,
            );
        }
    });
}

/// The whole awake state array, for worker 0's serial work between stages.
///
/// # Safety
/// No stage is running (the other workers are waiting), and the slice is dropped before the
/// next stage starts.
#[inline]
unsafe fn all_states<'a>(states: StatesView) -> &'a mut [crate::body::BodyState] {
    unsafe { states.slice_mut(0..states.len()) }
}

/// Solve joints then contacts for overflow + colors. During biased solve,
/// force/torque thresholds set bits in `joint_state_bit_set`. (serial of
/// b3SolveJointsTask / b3PushContacts_* when `use_bias`, else b3SolveContacts_*, + overflow)
///
/// Primordial Engine fork (D2): each color's contacts are a stage; joints and the overflow
/// constraints stay on worker 0, in the serial order.
fn solve_joints_then_contacts(
    world: &mut World,
    overflow_constraints: &mut [ContactConstraint],
    views: &ColorViews,
    states: StatesView,
    context: &StepContext,
    stages: &dyn Stages,
    use_bias: bool,
) {
    // Overflow first — always the scalar Mesh kernels (C: b3SolveContacts_Overflow)
    {
        let mut joint_sims =
            std::mem::take(&mut world.constraint_graph.colors[OVERFLOW_INDEX as usize].joint_sims);
        {
            // SAFETY: no stage is running.
            let states = unsafe { all_states(states) };
            for joint in &mut joint_sims {
                solve_joint(joint, context, states, use_bias);
            }
        }
        if use_bias {
            for joint in &mut joint_sims {
                maybe_flag_joint_reaction(world, joint, context.inv_h);
            }
        }
        world.constraint_graph.colors[OVERFLOW_INDEX as usize].joint_sims = joint_sims;

        // SAFETY: no stage is running.
        let states = unsafe { all_states(states) };
        if use_bias {
            // (b3PushContacts_Overflow)
            push_contacts(overflow_constraints, states, context);
        } else {
            // (b3SolveContacts_Overflow)
            solve_contacts(overflow_constraints, states, context);
        }
    }

    for color_index in 0..OVERFLOW_INDEX as usize {
        let mut joint_sims =
            std::mem::take(&mut world.constraint_graph.colors[color_index].joint_sims);
        {
            // SAFETY: no stage is running.
            let states = unsafe { all_states(states) };
            for joint in &mut joint_sims {
                solve_joint(joint, context, states, use_bias);
            }
        }
        if use_bias {
            for joint in &mut joint_sims {
                maybe_flag_joint_reaction(world, joint, context.inv_h);
            }
        }
        world.constraint_graph.colors[color_index].joint_sims = joint_sims;

        let convex = views.convex[color_index];
        let mesh = views.mesh[color_index];
        // C graph blocks: joints → wide/convex → mesh (b3_graphContactBlock)
        color_stage(stages, convex.len(), mesh.len(), &|is_mesh, items, _| {
            let set = if is_mesh { mesh } else { convex };
            // SAFETY: a color stage; this block owns these constraints and their bodies.
            unsafe {
                if use_bias {
                    // (b3PushContacts_Convex, b3PushContacts_MeshWide)
                    push_contacts_w_range(set, items, states, context);
                } else {
                    // (b3SolveContacts_Convex, b3SolveContacts_MeshWide)
                    solve_contacts_w_range(set, items, states, context);
                }
            }
        });
    }
}

fn maybe_flag_joint_reaction(world: &mut World, joint: &crate::joint::JointSim, inv_h: f32) {
    if !(joint.force_threshold < f32::MAX || joint.torque_threshold < f32::MAX) {
        return;
    }
    if world.task_contexts[0]
        .joint_state_bit_set
        .get_bit(joint.joint_id as u32)
    {
        return;
    }

    let (force, torque) = get_joint_reaction(world, joint, inv_h);

    // Check thresholds. A zero threshold means all awake joints get reported.
    if force >= joint.force_threshold || torque >= joint.torque_threshold {
        world.task_contexts[0]
            .joint_state_bit_set
            .set_bit(joint.joint_id as u32);
    }
}

/// Everything `solve_stages` needs, so the parallel path can hand it to worker 0.
struct SolveArgs<'a> {
    world: &'a mut World,
    context: &'a StepContext,
    overflow_constraints: &'a mut Vec<ContactConstraint>,
    wide_constraints: &'a mut Vec<WideContactConstraints>,
    mesh_wide_constraints: &'a mut Vec<WideContactConstraints>,
    mesh_lanes: &'a [MeshLanes],
    ticks: &'a mut crate::stage_timer::Ticks,
}

/// The sub-step loop, restitution and impulse storage, on `stages`. Primordial Engine fork
/// (D2): this is the part of b3Solve that C runs as b3SolverTask stages.
fn solve_stages(args: SolveArgs<'_>, stages: &dyn Stages) {
    let SolveArgs {
        world,
        context,
        overflow_constraints,
        wide_constraints,
        mesh_wide_constraints,
        mesh_lanes,
        ticks,
    } = args;

    let views = ColorViews {
        convex: wide_constraints.iter_mut().map(WideView::new).collect(),
        mesh: mesh_wide_constraints.iter_mut().map(WideView::new).collect(),
    };

    // Prepare contact constraints. Graph colors get wide constraints for the convex contacts
    // and for the mesh contacts, the overflow color gets scalar constraints.
    // (b3PrepareContacts_Convex / b3PrepareContacts_MeshWide / b3PrepareContacts_Overflow)
    // Primordial Engine fork (D2): every color's wide constraints are one stage of blocks;
    // each constraint reads its lane contacts and bodies and writes only its own slot (and
    // the hit-event normal velocities of its own lane contacts). Overflow runs on worker 0.
    let any_restitution = {
        let World {
            constraint_graph,
            contacts,
            solver_sets,
            ..
        } = &mut *world;
        let awake = &solver_sets[AWAKE_SET as usize];
        let (sims, states) = (&awake.body_sims[..], &awake.body_states[..]);
        let warm = context.enable_warm_starting;

        // (first item, color, is_mesh) of every non-empty color array.
        let mut segments: Vec<(usize, usize, bool)> = Vec::new();
        let mut item_count = 0;
        for color_index in 0..OVERFLOW_INDEX as usize {
            for (is_mesh, count) in [
                (false, views.convex[color_index].len()),
                (true, views.mesh[color_index].len()),
            ] {
                if count > 0 {
                    segments.push((item_count, color_index, is_mesh));
                    item_count += count;
                }
            }
        }

        let any_wide = std::sync::atomic::AtomicBool::new(false);
        {
            let graph = &*constraint_graph;
            let shared_contacts = SharedSlice::new(contacts);
            let segments = &segments;
            let views = &views;
            stages.run(item_count, PREPARE_BLOCK_SIZE, &|items, _| {
                let mut any = false;
                let mut k = segments.partition_point(|segment| segment.0 <= items.start) - 1;
                let mut i = items.start;
                while i < items.end {
                    let (segment_start, color_index, is_mesh) = segments[k];
                    let segment_end = segments.get(k + 1).map_or(item_count, |next| next.0);
                    let end = items.end.min(segment_end);
                    let local = i - segment_start..end - segment_start;
                    let color = &graph.colors[color_index];
                    // SAFETY: a prepare stage; each block owns its constraints and their lane
                    // contacts, and the bodies are only read.
                    any |= unsafe {
                        if is_mesh {
                            prepare_contacts_mesh_wide_range(
                                views.mesh[color_index],
                                local,
                                &color.contacts,
                                &mesh_lanes[color_index].order,
                                shared_contacts,
                                sims,
                                states,
                                warm,
                            )
                        } else {
                            prepare_contacts_convex_range(
                                views.convex[color_index],
                                local,
                                &color.convex_contacts,
                                shared_contacts,
                                sims,
                                states,
                                warm,
                            )
                        }
                    };
                    i = end;
                    k += 1;
                }
                if any {
                    any_wide.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }

        // (b3PrepareContacts_Overflow)
        let overflow = &constraint_graph.colors[OVERFLOW_INDEX as usize];
        debug_assert!(overflow.convex_contacts.is_empty());
        let overflow_restitution = prepare_mesh_contacts(
            overflow_constraints,
            &overflow.contacts,
            contacts,
            sims,
            states,
            context,
        );
        any_wide.into_inner() || overflow_restitution
    };
    world.profile.prepare_constraints += ticks.lap();
    let gravity = world.gravity;
    let (states, sims, body_count) = {
        let awake = &mut world.solver_sets[AWAKE_SET as usize];
        debug_assert!(awake.body_sims.len() == awake.body_states.len());
        let sims = SharedMut(awake.body_sims.as_mut_ptr());
        (StatesView::new(&mut awake.body_states), sims, awake.body_states.len())
    };

    // Sub-step loop
    let sub_step_count = context.sub_step_count;
    for _sub_step_index in 0..sub_step_count {
        // (b3_stageIntegrateVelocities)
        stages.run(body_count, BODY_BLOCK_SIZE, &|items, _| {
            // SAFETY: a body stage; this block owns these bodies. The sims are only read.
            unsafe {
                let sims = std::slice::from_raw_parts(sims.ptr().add(items.start), items.len());
                integrate_velocities_block(gravity, sims, states.slice_mut(items), context);
            }
        });
        world.profile.integrate_velocities += ticks.lap();

        // Warm start: joints then contacts; overflow first, then colors.
        {
            // SAFETY: no stage is running.
            let states = unsafe { all_states(states) };
            for joint in &mut world.constraint_graph.colors[OVERFLOW_INDEX as usize].joint_sims {
                warm_start_joint(joint, states);
            }
            warm_start_contacts(overflow_constraints, states);
        }
        for color_index in 0..OVERFLOW_INDEX as usize {
            {
                // SAFETY: no stage is running.
                let states = unsafe { all_states(states) };
                for joint in &mut world.constraint_graph.colors[color_index].joint_sims {
                    warm_start_joint(joint, states);
                }
            }
            // (b3WarmStartContacts_Convex, b3WarmStartContacts_MeshWide)
            let convex = views.convex[color_index];
            let mesh = views.mesh[color_index];
            color_stage(stages, convex.len(), mesh.len(), &|is_mesh, items, _| {
                let set = if is_mesh { mesh } else { convex };
                // SAFETY: a color stage.
                unsafe { warm_start_contacts_w_range(set, items, states) };
            });
        }
        world.profile.warm_start += ticks.lap();

        for _ in 0..SOLVER_ITERATIONS {
            let use_bias = true;
            solve_joints_then_contacts(
                world,
                overflow_constraints,
                &views,
                states,
                context,
                stages,
                use_bias,
            );
        }
        world.profile.solve_impulses += ticks.lap();

        // (b3_stageIntegratePositions)
        stages.run(body_count, BODY_BLOCK_SIZE, &|items, _| {
            // SAFETY: a body stage; this block owns these bodies.
            unsafe { integrate_positions_block(states.slice_mut(items), context) };
        });
        world.profile.integrate_positions += ticks.lap();

        for _ in 0..RELAX_ITERATIONS {
            let use_bias = false;
            solve_joints_then_contacts(
                world,
                overflow_constraints,
                &views,
                states,
                context,
                stages,
                use_bias,
            );
        }
        world.profile.relax_impulses += ticks.lap();
    }

    // Restitution. Overflow first, then the colors: wide blocks then mesh blocks.
    // (b3_stageRestitution)
    let restitution_iterations = world.restitution_iterations;
    if restitution_iterations > 0 && any_restitution {
        let threshold = world.restitution_threshold;
        let propagate = world.enable_restitution_propagation;
        for _ in 0..restitution_iterations {
            {
                // SAFETY: no stage is running.
                let states = unsafe { all_states(states) };
                // (b3ApplyRestitution_Overflow)
                apply_restitution(overflow_constraints, states, context, threshold, propagate);
            }

            for color_index in 0..OVERFLOW_INDEX as usize {
                // (b3ApplyRestitution_Convex, b3ApplyRestitution_MeshWide)
                let convex = views.convex[color_index];
                let mesh = views.mesh[color_index];
                color_stage(stages, convex.len(), mesh.len(), &|is_mesh, items, _| {
                    let set = if is_mesh { mesh } else { convex };
                    // SAFETY: a color stage.
                    unsafe {
                        apply_restitution_w_range(set, items, states, context, threshold, propagate)
                    };
                });
            }
        }
    }
    world.profile.restitution += ticks.lap();

    // Store impulses. C stores the overflow contacts with b3StoreImpulses_Overflow (the Mesh
    // kernel, which also flags hit events) and the colors in the store stages
    // (b3StoreImpulses_Convex, b3StoreImpulses_MeshWide). Each worker flags hit events in its
    // own task context; the bit sets are ORed into worker 0's afterwards, like C.
    {
        let neg_hit_threshold = -world.hit_event_threshold;
        let mut has_hit = world.task_contexts[0].has_hit_events;

        store_impulses(overflow_constraints, &mut world.contacts);
        flag_hit_events(
            overflow_constraints,
            &world.contacts,
            &mut world.task_contexts[0].hit_event_bit_set,
            &mut has_hit,
            neg_hit_threshold,
        );
        world.task_contexts[0].has_hit_events = has_hit;

        let contact_count = world.contacts.len();
        let contacts = SharedMut::new(&mut world.contacts);
        let tasks = SharedMut::new(&mut world.task_contexts);
        for color_index in 0..OVERFLOW_INDEX as usize {
            let convex = &wide_constraints[color_index];
            let mesh = &mesh_wide_constraints[color_index];
            color_stage(stages, convex.len(), mesh.len(), &|is_mesh, items, worker| {
                let set = if is_mesh { mesh } else { convex };
                // SAFETY: each worker index belongs to one thread, and each contact is in one
                // lane of one constraint, so blocks write disjoint contacts.
                unsafe {
                    let task = tasks.get_mut(worker);
                    store_impulses_w_range(
                        set,
                        items,
                        contacts.ptr(),
                        contact_count,
                        &mut task.hit_event_bit_set,
                        &mut task.has_hit_events,
                        neg_hit_threshold,
                    );
                }
            });
        }

        let (first, rest) = world.task_contexts.split_at_mut(1);
        for task in rest {
            if task.has_hit_events {
                first[0].hit_event_bit_set.in_place_union(&task.hit_event_bit_set);
                first[0].has_hit_events = true;
            }
        }
    }
    world.profile.store_impulses += ticks.lap();
}

/// Solve with graph coloring. (b3Solve — serial)
///
/// Does not increment `world.step_index` — World::step owns that.
pub fn solve(world: &mut World, context: &StepContext) {
    let awake_body_count = world.solver_sets[AWAKE_SET as usize].body_sims.len();
    if awake_body_count == 0 {
        return;
    }

    world.body_move_events.resize(
        awake_body_count,
        BodyMoveEvent {
            user_data: 0,
            transform: WORLD_TRANSFORM_IDENTITY,
            body_id: BodyId::default(),
            fell_asleep: false,
        },
    );

    let mut ticks = crate::stage_timer::Ticks::now();

    let contact_id_capacity = world.contact_id_pool.id_capacity();
    let joint_id_capacity = world.joint_id_pool.id_capacity();
    {
        let task_context = &mut world.task_contexts[0];
        task_context
            .hit_event_bit_set
            .set_bit_count_and_clear(contact_id_capacity as u32);
        task_context.has_hit_events = false;
        task_context
            .joint_state_bit_set
            .set_bit_count_and_clear(joint_id_capacity as u32);
    }

    // Split an awake island. This modifies:
    // - world island array and solver set
    // - island indices on bodies, contacts, and joints
    // C runs this as a task in parallel with the constraint solve (it cannot
    // overlap FinalizeBodies); the serial port runs it here, at the point
    // where C enqueues it. (b3SplitIslandTask)
    if world.split_island_id != NULL_INDEX {
        split_island(world, world.split_island_id);
        world.split_island_id = NULL_INDEX;
    }
    world.profile.split_islands += ticks.lap();

    // Prepare joints for every color (incl. overflow) before contacts.
    // mem::take avoids &World + &mut joint_sims from the same World.
    for color_index in 0..GRAPH_COLOR_COUNT as usize {
        let mut joint_sims =
            std::mem::take(&mut world.constraint_graph.colors[color_index].joint_sims);
        for joint in &mut joint_sims {
            prepare_joint(world, joint, context);
        }
        world.constraint_graph.colors[color_index].joint_sims = joint_sims;
    }

    // Prepare contact constraints. Graph colors get wide constraints for the convex contacts
    // and for the mesh contacts, the overflow color gets scalar constraints.
    // (b3PrepareContacts_Convex / b3PrepareContacts_MeshWide / b3PrepareContacts_Overflow)
    // C dispatches the wide kernels on world->simdWidth (4 or 8). The scalar port only has
    // the width 4 kernels, and the world width can never be 8 here (b3SetSIMDWidth and
    // b3World_EnableSSE2Fallback fall back to 4 without AVX2).
    debug_assert!(world.simd_width == crate::core::SIMD_WIDTH as i32);
    let simd_shift = 2;
    let color_count = OVERFLOW_INDEX as usize;
    let mut overflow_constraints: Vec<ContactConstraint> = Vec::new();
    let mut wide_constraints: Vec<WideContactConstraints> = (0..color_count)
        .map(|_| WideContactConstraints::default())
        .collect();
    let mut mesh_wide_constraints: Vec<WideContactConstraints> = (0..color_count)
        .map(|_| WideContactConstraints::default())
        .collect();

    // Sort mesh contacts by manifold count so that contacts with more manifolds are solved
    // together keeping the SIMD lanes fuller and less ragged.
    let mesh_lanes = {
        let color_specs: Vec<&[crate::contact::ContactSpec]> = world.constraint_graph.colors
            [..color_count]
            .iter()
            .map(|color| color.contacts.as_slice())
            .collect();
        compute_mesh_lanes(&color_specs, simd_shift)
    };

    // Primordial Engine fork (D2): size every color's wide constraints now; solve_stages
    // prepares them in parallel blocks (b3_stagePrepareWideContacts, b3_stagePrepareContacts).
    for color_index in 0..color_count {
        let color = &world.constraint_graph.colors[color_index];
        allocate_contacts_convex(&mut wide_constraints[color_index], color.convex_contacts.len());
        allocate_contacts_mesh_wide(
            &mut mesh_wide_constraints[color_index],
            &mesh_lanes[color_index].manifold_starts,
        );
    }

    let args = SolveArgs {
        world,
        context,
        overflow_constraints: &mut overflow_constraints,
        wide_constraints: &mut wide_constraints,
        mesh_wide_constraints: &mut mesh_wide_constraints,
        mesh_lanes: &mesh_lanes,
        ticks: &mut ticks,
    };
    // Primordial Engine fork (D2): with worker threads the stages run on all of them, worker 0
    // driving; otherwise they run serially here. Same code and same results either way.
    let pool = args.world.worker_pool.clone();
    match pool {
        Some(pool) if awake_body_count >= PARALLEL_SOLVE_MIN_BODIES => {
            let sync = StageSync::new();
            let args = std::sync::Mutex::new(Some(args));
            pool.run(&|worker| {
                if worker == 0 {
                    struct Finish<'a>(&'a StageSync);
                    impl Drop for Finish<'_> {
                        fn drop(&mut self) {
                            self.0.finish();
                        }
                    }
                    let _finish = Finish(&sync);
                    let args = args.lock().unwrap().take().unwrap();
                    solve_stages(args, &ParallelStages::new(&sync));
                } else {
                    sync.worker_loop(worker);
                }
            });
        }
        _ => solve_stages(args, &SerialStages),
    }

    // Finalize bodies (CCD for non-bullets; bullets queued)
    let mut bullet_bodies: Vec<i32> = Vec::with_capacity(awake_body_count);
    {
        let awake_island_count = world.solver_sets[AWAKE_SET as usize].island_sims.len();
        let task_context = &mut world.task_contexts[0];
        task_context.sensor_hits.clear();
        task_context
            .awake_island_bit_set
            .set_bit_count_and_clear(awake_island_count as u32);
        task_context.split_island_id = NULL_INDEX;
        task_context.split_sleep_time = 0.0;
    }

    // Finish the user tree task that was queued earlier in the time step. This must be
    // complete before touching the broad-phase. The serial port already ran the tree task
    // inline in update_broad_phase_pairs, so only the validation remains.
    world.broad_phase.validate_no_moved();

    // Finalize bodies. Must happen after the constraint solver and after island splitting.
    finalize_bodies(world, context, &mut bullet_bodies);
    world.profile.transforms += ticks.lap();

    // Report joint events (C block between finalize and hit events).
    {
        debug_assert!(world.joint_events.is_empty());
        let world_id = world.world_id;
        let word_count = world.task_contexts[0].joint_state_bit_set.block_count();
        for k in 0..word_count {
            let mut word = world.task_contexts[0].joint_state_bit_set.block(k);
            while word != 0 {
                let ctz = word.trailing_zeros();
                let joint_id = (64 * k + ctz) as i32;

                debug_assert!((joint_id as usize) < world.joints.len());

                let joint = &world.joints[joint_id as usize];
                debug_assert!(joint.set_index == AWAKE_SET);

                world.joint_events.push(JointEvent {
                    joint_id: JointId {
                        index1: joint_id + 1,
                        world0: world_id,
                        generation: joint.generation,
                    },
                    user_data: joint.user_data,
                });

                word &= word - 1;
            }
        }
    }

    world.profile.joint_events += ticks.lap();

    // Report hit events flagged during store impulses. C runs this after the
    // joint events pass.
    {
        use crate::events::ContactHitEvent;
        use crate::id::{ContactId, ShapeId};
        use crate::math_functions::{lerp, lerp_position, offset_pos, VEC3_ZERO};

        debug_assert!(world.contact_hit_events.is_empty());

        // Fast path: if no worker flagged any hit-event candidates during
        // b2StoreImpulsesTask, skip entirely.
        if world.task_contexts[0].has_hit_events {
            let threshold = world.hit_event_threshold;
            let world_id = world.world_id;

            let word_count = world.task_contexts[0].hit_event_bit_set.block_count();
            for k in 0..word_count {
                let mut word = world.task_contexts[0].hit_event_bit_set.block(k);
                while word != 0 {
                    let ctz = word.trailing_zeros();
                    let contact_id = (64 * k + ctz) as i32;

                    let contact = &world.contacts[contact_id as usize];
                    debug_assert!(
                        contact.set_index == AWAKE_SET && contact.color_index != NULL_INDEX
                    );

                    let shape_a = &world.shapes[contact.shape_id_a as usize];
                    let shape_b = &world.shapes[contact.shape_id_b as usize];
                    let body_a = &world.bodies[shape_a.body_id as usize];
                    let body_b = &world.bodies[shape_b.body_id as usize];
                    // (b3GetBodySim)
                    let sim_a = &world.solver_sets[body_a.set_index as usize].body_sims
                        [body_a.local_index as usize];
                    let sim_b = &world.solver_sets[body_b.set_index as usize].body_sims
                        [body_b.local_index as usize];
                    let mid_center = lerp_position(sim_a.center, sim_b.center, 0.5);

                    let mut approach_speed = threshold;
                    let mut point = mid_center;
                    let mut normal = VEC3_ZERO;
                    let mut found = false;
                    let mut triangle_index = 0;
                    for manifold in &contact.manifolds {
                        for p in 0..manifold.point_count as usize {
                            let mp = &manifold.points[p];
                            let mp_approach_speed = -mp.normal_velocity;

                            // Need to check total impulse because the point may be speculative and not colliding
                            if mp_approach_speed > approach_speed && mp.total_normal_impulse > 0.0 {
                                approach_speed = mp_approach_speed;
                                point = offset_pos(mid_center, lerp(mp.anchor_a, mp.anchor_b, 0.5));
                                normal = manifold.normal;
                                triangle_index = mp.triangle_index;
                                found = true;
                            }
                        }
                    }

                    if found {
                        let event = ContactHitEvent {
                            shape_id_a: ShapeId {
                                index1: shape_a.id + 1,
                                world0: world_id,
                                generation: shape_a.generation,
                            },
                            shape_id_b: ShapeId {
                                index1: shape_b.id + 1,
                                world0: world_id,
                                generation: shape_b.generation,
                            },
                            contact_id: ContactId {
                                index1: contact.contact_id + 1,
                                world0: world_id,
                                padding: 0,
                                generation: contact.generation,
                            },
                            point,
                            normal,
                            approach_speed,
                            // shapeB is never a compound today (asserted in b3CreateContact), so the
                            // childIndex argument is irrelevant for it. shapeA carries the compound.
                            user_material_id_a: shape_a
                                .get_shape_user_material_id(contact.child_index, triangle_index),
                            user_material_id_b: shape_b
                                .get_shape_user_material_id(0, triangle_index),
                        };
                        world.contact_hit_events.push(event);
                    }

                    word &= word - 1;
                }
            }
        }
    }

    world.profile.hit_events += ticks.lap();

    // Refit BVH
    {
        world.broad_phase.trees[BodyType::Kinematic as usize].refit();
        world.broad_phase.trees[BodyType::Dynamic as usize].refit();

        world.broad_phase.validate();
    }
    world.profile.refit += ticks.lap();

    // Fast bullet bodies. Note: a bullet body may be moving slow
    if !bullet_bodies.is_empty() {
        for &sim_index in &bullet_bodies {
            solve_continuous(world, sim_index, context.dt);
        }

        // Serially enlarge broad-phase proxies for bullet shapes
        let dynamic_tree = BodyType::Dynamic as usize;

        // This loop has non-deterministic order but it shouldn't affect the result
        for &sim_index in &bullet_bodies {
            let bullet_sim =
                &mut world.solver_sets[AWAKE_SET as usize].body_sims[sim_index as usize];
            if (bullet_sim.flags & body_flags::ENLARGE_BULLET_BOUNDS) == 0 {
                continue;
            }

            // Clear flag
            bullet_sim.flags &= !body_flags::ENLARGE_BULLET_BOUNDS;

            let body_id = bullet_sim.body_id;
            debug_assert!(0 <= body_id && (body_id as usize) < world.bodies.len());

            let mut shape_id = world.bodies[body_id as usize].head_shape_id;
            while shape_id != NULL_INDEX {
                let proxy_key = world.shapes[shape_id as usize].proxy_key;
                let proxy_id_ = proxy_id(proxy_key);
                debug_assert!(proxy_type(proxy_key) == BodyType::Dynamic);

                let tree_aabb = world.broad_phase.trees[dynamic_tree].aabb(proxy_id_);
                let fat_aabb = world.fat_aabbs[shape_id as usize];

                if !aabb_contains(tree_aabb, fat_aabb) {
                    world.broad_phase.trees[dynamic_tree].enlarge_proxy(proxy_id_, fat_aabb);
                }

                shape_id = world.shapes[shape_id as usize].next_shape_id;
            }
        }
    }

    world.profile.bullets += ticks.lap();

    // Report sensor hits. This may include bullet sensor hits.
    {
        let mut drained: Vec<(i32, i32, u16)> = Vec::new();
        for task_context in &world.task_contexts {
            for hit in &task_context.sensor_hits {
                let sensor_index = world.shapes[hit.sensor_id as usize].sensor_index;
                let generation = world.shapes[hit.visitor_id as usize].generation;
                drained.push((sensor_index, hit.visitor_id, generation));
            }
        }
        for (sensor_index, visitor_id, generation) in drained {
            world.sensors[sensor_index as usize]
                .hits
                .push(crate::sensor::Visitor {
                    shape_id: visitor_id,
                    generation,
                });
        }
    }

    world.profile.sensor_hits += ticks.lap();

    // Island sleeping
    // This must be done last because putting islands to sleep invalidates the enlarged body bits.
    if world.enable_sleep {
        // Collect split island candidate for the next time step. No need to split if sleeping is disabled.
        debug_assert!(world.split_island_id == NULL_INDEX);
        let mut split_sleep_timer = 0.0f32;
        {
            let task_context = &world.task_contexts[0];
            if task_context.split_island_id != NULL_INDEX
                && task_context.split_sleep_time >= split_sleep_timer
            {
                debug_assert!(task_context.split_sleep_time > 0.0);

                // Tie breaking for determinism. Largest island id wins. C needs this
                // due to work stealing across workers; kept for the serial port so a
                // multi-worker build later cannot change the outcome.
                let tied_but_smaller = task_context.split_sleep_time == split_sleep_timer
                    && task_context.split_island_id < world.split_island_id;
                if !tied_but_smaller {
                    world.split_island_id = task_context.split_island_id;
                    split_sleep_timer = task_context.split_sleep_time;
                }
            }
        }
        let _ = split_sleep_timer;

        // Need to process in reverse because this moves islands to sleeping solver sets.
        let count = world.solver_sets[AWAKE_SET as usize].island_sims.len();
        for island_index in (0..count).rev() {
            if world.task_contexts[0]
                .awake_island_bit_set
                .get_bit(island_index as u32)
            {
                // this island is still awake
                continue;
            }

            let island_id =
                world.solver_sets[AWAKE_SET as usize].island_sims[island_index].island_id;

            try_sleep_island(world, island_id);
        }

        world.validate_solver_sets();
    }
    world.profile.sleep_islands += ticks.lap();
}
