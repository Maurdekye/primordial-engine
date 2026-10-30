// Put an island to sleep. (b3TrySleepIsland from solver_set.c)
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::{SolverSet, AWAKE_SET, DISABLED_SET};
use crate::body::sleep_body_sim_index;
use crate::core::NULL_INDEX;
use crate::island::IslandSim;
use crate::world::World;

/// Move a sleeping island (bodies, contacts, joints) from the awake set into a
/// new sleeping solver set. (b3TrySleepIsland)
pub fn try_sleep_island(world: &mut World, island_id: i32) {
    use crate::constants::GRAPH_COLOR_COUNT;
    use crate::constraint_graph::OVERFLOW_INDEX;
    use crate::contact::contact_flags;

    debug_assert!(world.islands[island_id as usize].set_index == AWAKE_SET);

    // Cannot put an island to sleep while it has a pending split and more than one body.
    if world.islands[island_id as usize].constraint_remove_count > 0
        && world.islands[island_id as usize].bodies.len() > 1
    {
        return;
    }

    // island is sleeping
    // - create new sleeping solver set
    // - move island to sleeping solver set
    // - identify non-touching contacts that should move to sleeping solver set or disabled set
    // - remove old island
    // - fix island
    let sleep_set_id = world.solver_set_id_pool.alloc_id();
    if sleep_set_id == world.solver_sets.len() as i32 {
        world.solver_sets.push(SolverSet::free_slot());
    }

    {
        let island = &world.islands[island_id as usize];
        debug_assert!(
            0 <= island.local_index
                && (island.local_index as usize)
                    < world.solver_sets[AWAKE_SET as usize].island_sims.len()
        );

        let body_capacity = island.bodies.len();
        let contact_capacity = island.contacts.len();
        let joint_capacity = island.joints.len();

        let sleep_set = &mut world.solver_sets[sleep_set_id as usize];
        *sleep_set = SolverSet::default();
        sleep_set.set_index = sleep_set_id;
        sleep_set.body_sims.reserve(body_capacity);
        sleep_set.contact_indices.reserve(contact_capacity);
        sleep_set.joint_sims.reserve(joint_capacity);
    }

    // move awake bodies to sleeping set
    // this shuffles around bodies in the awake set
    {
        let body_count = world.islands[island_id as usize].bodies.len();
        for i in 0..body_count {
            let body_id = world.islands[island_id as usize].bodies[i];
            debug_assert!(world.bodies[body_id as usize].set_index == AWAKE_SET);
            debug_assert!(world.bodies[body_id as usize].island_id == island_id);
            debug_assert!(world.bodies[body_id as usize].island_index == i as i32);

            // Update the body move event to indicate this body fell asleep
            // It could happen the body is forced asleep before it ever moves.
            let body_move_index = world.bodies[body_id as usize].body_move_index;
            if body_move_index != NULL_INDEX {
                let move_event = &mut world.body_move_events[body_move_index as usize];
                debug_assert!(move_event.body_id.index1 - 1 == body_id);
                debug_assert!(
                    move_event.body_id.generation == world.bodies[body_id as usize].generation
                );
                move_event.fell_asleep = true;
                world.bodies[body_id as usize].body_move_index = NULL_INDEX;
            }

            let awake_body_index = world.bodies[body_id as usize].local_index;

            // move body sim to sleep set
            let awake_sim =
                world.solver_sets[AWAKE_SET as usize].body_sims[awake_body_index as usize];
            let sleep_body_index = world.solver_sets[sleep_set_id as usize].body_sims.len() as i32;
            world.solver_sets[sleep_set_id as usize]
                .body_sims
                .push(awake_sim);

            {
                let awake_sims = &mut world.solver_sets[AWAKE_SET as usize].body_sims;
                let moved_index = awake_sims.len() as i32 - 1;
                awake_sims.swap_remove(awake_body_index as usize);
                if moved_index != awake_body_index {
                    // fix local index on moved element
                    let moved_id = awake_sims[awake_body_index as usize].body_id;
                    let moved_body = &mut world.bodies[moved_id as usize];
                    debug_assert!(moved_body.local_index == moved_index);
                    moved_body.local_index = awake_body_index;

                    if moved_body.island_id != island_id {
                        crate::body::refresh_body_contact_indices(world, moved_id);
                    }
                }
            }

            // destroy state, no need to clone
            world.solver_sets[AWAKE_SET as usize]
                .body_states
                .swap_remove(awake_body_index as usize);

            {
                let body = &mut world.bodies[body_id as usize];
                body.set_index = sleep_set_id;
                body.local_index = sleep_body_index;
            }

            // Move non-touching contacts to the disabled set.
            // Non-touching contacts may exist between sleeping islands and there is no clear ownership.
            let mut contact_key = world.bodies[body_id as usize].head_contact_key;
            while contact_key != NULL_INDEX {
                let contact_id = contact_key >> 1;
                let edge_index = (contact_key & 1) as usize;

                debug_assert!(world.contacts[contact_id as usize].set_index == AWAKE_SET);

                contact_key = world.contacts[contact_id as usize].edges[edge_index].next_key;

                if world.contacts[contact_id as usize].color_index != NULL_INDEX {
                    // contact is touching and will be moved separately
                    debug_assert!(
                        (world.contacts[contact_id as usize].flags & contact_flags::TOUCHING) != 0
                    );
                    continue;
                }

                if edge_index == 0 {
                    world.contacts[contact_id as usize].encoded_body_sim_a = NULL_INDEX;
                } else {
                    world.contacts[contact_id as usize].encoded_body_sim_b = NULL_INDEX;
                }

                // the other body may still be awake, it still may go to sleep and then it will be responsible
                // for moving this contact to the disabled set.
                let other_edge_index = edge_index ^ 1;
                let other_body_id =
                    world.contacts[contact_id as usize].edges[other_edge_index].body_id;
                if world.bodies[other_body_id as usize].set_index == AWAKE_SET {
                    continue;
                }

                let local_index = world.contacts[contact_id as usize].local_index;
                debug_assert!(
                    world.solver_sets[AWAKE_SET as usize].contact_indices[local_index as usize]
                        == contact_id
                );

                debug_assert!(world.contacts[contact_id as usize].manifold_count() == 0);
                debug_assert!(
                    (world.contacts[contact_id as usize].flags & contact_flags::TOUCHING) == 0
                );

                // Move the non-touching contact to the disabled set.
                {
                    let disabled_contact_count = world.solver_sets[DISABLED_SET as usize]
                        .contact_indices
                        .len() as i32;
                    let contact = &mut world.contacts[contact_id as usize];
                    contact.set_index = DISABLED_SET;

                    // This is mandatory for validation to work correctly
                    contact.local_index = disabled_contact_count;
                }
                world.solver_sets[DISABLED_SET as usize]
                    .contact_indices
                    .push(contact_id);

                let awake_contacts = &mut world.solver_sets[AWAKE_SET as usize].contact_indices;
                let moved_local_index = awake_contacts.len() as i32 - 1;
                awake_contacts.swap_remove(local_index as usize);
                if moved_local_index != local_index {
                    // fix moved element
                    let moved_contact_index = awake_contacts[local_index as usize];
                    let moved_contact = &mut world.contacts[moved_contact_index as usize];
                    debug_assert!(moved_contact.local_index == moved_local_index);
                    moved_contact.local_index = local_index;
                }
            }
        }
    }

    // move touching contacts to sleeping set
    // this shuffles contacts in the awake set
    {
        let contact_count = world.islands[island_id as usize].contacts.len();
        for i in 0..contact_count {
            let contact_id = world.islands[island_id as usize].contacts[i].contact_id;
            debug_assert!(world.contacts[contact_id as usize].set_index == AWAKE_SET);
            debug_assert!(world.contacts[contact_id as usize].island_id == island_id);
            debug_assert!(world.contacts[contact_id as usize].island_index == i as i32);
            let color_index = world.contacts[contact_id as usize].color_index;
            debug_assert!(0 <= color_index && color_index < GRAPH_COLOR_COUNT);

            // Remove bodies from graph coloring associated with this constraint
            if color_index != OVERFLOW_INDEX {
                // might clear a bit for a static body, but this has no effect
                let body_id_a = world.contacts[contact_id as usize].edges[0].body_id;
                let body_id_b = world.contacts[contact_id as usize].edges[1].body_id;
                let color = &mut world.constraint_graph.colors[color_index as usize];
                color.body_set.clear_bit(body_id_a as u32);
                color.body_set.clear_bit(body_id_b as u32);
            }

            let sleep_contact_index = world.solver_sets[sleep_set_id as usize]
                .contact_indices
                .len() as i32;
            world.solver_sets[sleep_set_id as usize]
                .contact_indices
                .push(contact_id);

            let local_index = world.contacts[contact_id as usize].local_index;
            let is_mesh =
                (world.contacts[contact_id as usize].flags & contact_flags::SIM_MESH_CONTACT) != 0;
            if is_mesh || color_index == OVERFLOW_INDEX {
                let color = &mut world.constraint_graph.colors[color_index as usize];
                let moved_local_index = color.contacts.len() as i32 - 1;
                color.contacts.swap_remove(local_index as usize);
                if moved_local_index != local_index {
                    // fix moved element
                    let moved_contact_id = color.contacts[local_index as usize].contact_id;
                    let moved_contact = &mut world.contacts[moved_contact_id as usize];
                    debug_assert!(moved_contact.local_index == moved_local_index);
                    moved_contact.local_index = local_index;
                }
            } else {
                let color = &mut world.constraint_graph.colors[color_index as usize];
                let moved_local_index = color.convex_contacts.len() as i32 - 1;
                color.convex_contacts.swap_remove(local_index as usize);
                if moved_local_index != local_index {
                    // fix moved element
                    let moved_contact_id = color.convex_contacts[local_index as usize];
                    let moved_contact = &mut world.contacts[moved_contact_id as usize];
                    debug_assert!(moved_contact.local_index == moved_local_index);
                    moved_contact.local_index = local_index;
                }
            }

            let contact = &mut world.contacts[contact_id as usize];
            contact.set_index = sleep_set_id;
            contact.color_index = NULL_INDEX;
            contact.local_index = sleep_contact_index;
            contact.encoded_body_sim_a = sleep_body_sim_index(contact.encoded_body_sim_a);
            contact.encoded_body_sim_b = sleep_body_sim_index(contact.encoded_body_sim_b);
        }
    }

    // move joints
    // this shuffles joints in the awake set
    {
        let joint_count = world.islands[island_id as usize].joints.len();
        for i in 0..joint_count {
            let joint_id = world.islands[island_id as usize].joints[i].joint_id;
            debug_assert!(world.joints[joint_id as usize].set_index == AWAKE_SET);
            debug_assert!(world.joints[joint_id as usize].island_id == island_id);
            debug_assert!(world.joints[joint_id as usize].island_index == i as i32);
            let color_index = world.joints[joint_id as usize].color_index;
            let local_index = world.joints[joint_id as usize].local_index;

            debug_assert!(0 <= color_index && color_index < GRAPH_COLOR_COUNT);

            let awake_joint_sim = world.constraint_graph.colors[color_index as usize].joint_sims
                [local_index as usize];

            if color_index != OVERFLOW_INDEX {
                // might clear a bit for a static body, but this has no effect
                let body_id_a = world.joints[joint_id as usize].edges[0].body_id;
                let body_id_b = world.joints[joint_id as usize].edges[1].body_id;
                let color = &mut world.constraint_graph.colors[color_index as usize];
                color.body_set.clear_bit(body_id_a as u32);
                color.body_set.clear_bit(body_id_b as u32);
            }

            let sleep_joint_index =
                world.solver_sets[sleep_set_id as usize].joint_sims.len() as i32;
            world.solver_sets[sleep_set_id as usize]
                .joint_sims
                .push(awake_joint_sim);

            {
                let color = &mut world.constraint_graph.colors[color_index as usize];
                let moved_index = color.joint_sims.len() as i32 - 1;
                color.joint_sims.swap_remove(local_index as usize);
                if moved_index != local_index {
                    // fix moved element
                    let moved_id = color.joint_sims[local_index as usize].joint_id;
                    let moved_joint = &mut world.joints[moved_id as usize];
                    debug_assert!(moved_joint.local_index == moved_index);
                    moved_joint.local_index = local_index;
                }
            }

            let joint = &mut world.joints[joint_id as usize];
            joint.set_index = sleep_set_id;
            joint.color_index = NULL_INDEX;
            joint.local_index = sleep_joint_index;
        }
    }

    // move island struct
    {
        debug_assert!(world.islands[island_id as usize].set_index == AWAKE_SET);

        let island_index = world.islands[island_id as usize].local_index;
        world.solver_sets[sleep_set_id as usize]
            .island_sims
            .push(IslandSim { island_id });

        let awake_islands = &mut world.solver_sets[AWAKE_SET as usize].island_sims;
        let moved_island_index = awake_islands.len() as i32 - 1;
        awake_islands.swap_remove(island_index as usize);
        if moved_island_index != island_index {
            // fix index on moved element
            let moved_island_id = awake_islands[island_index as usize].island_id;
            let moved_island = &mut world.islands[moved_island_id as usize];
            debug_assert!(moved_island.local_index == moved_island_index);
            moved_island.local_index = island_index;
        }

        let island = &mut world.islands[island_id as usize];
        island.set_index = sleep_set_id;
        island.local_index = 0;
    }

    if world.split_island_id == island_id {
        world.split_island_id = NULL_INDEX;
    }

    world.validate_solver_sets();
}
