// Wake a sleeping solver set. (b3WakeSolverSet from solver_set.c)
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::{destroy_solver_set, AWAKE_SET, DISABLED_SET, FIRST_SLEEPING_SET};
use crate::core::NULL_INDEX;
use crate::world::World;

/// Wake a solver set. Does not merge islands. (b3WakeSolverSet)
///
/// Contacts can be in several places:
/// 1. non-touching contacts in the disabled set
/// 2. non-touching contacts already in the awake set
/// 3. touching contacts in the sleeping set
///
/// This handles contact types 1 and 3. Type 2 doesn't need any action.
pub fn wake_solver_set(world: &mut World, set_index: i32) {
    use crate::body::IDENTITY_BODY_STATE;
    use crate::constraint_graph::{add_contact_to_graph, add_joint_to_graph};
    use crate::contact::contact_flags;

    debug_assert!(set_index >= FIRST_SLEEPING_SET);

    let body_count = world.solver_sets[set_index as usize].body_sims.len();
    for i in 0..body_count {
        let sim_src = world.solver_sets[set_index as usize].body_sims[i];
        let body_id = sim_src.body_id;

        debug_assert!(world.bodies[body_id as usize].set_index == set_index);
        let awake_body_count = world.solver_sets[AWAKE_SET as usize].body_sims.len() as i32;
        let flags = {
            let body = &mut world.bodies[body_id as usize];
            body.set_index = AWAKE_SET;
            body.local_index = awake_body_count;
            // Reset sleep timer
            body.sleep_time = 0.0;
            body.flags
        };

        world.solver_sets[AWAKE_SET as usize]
            .body_sims
            .push(sim_src);

        let mut state = IDENTITY_BODY_STATE;
        state.flags = flags;
        world.solver_sets[AWAKE_SET as usize]
            .body_states
            .push(state);

        // Move non-touching contacts from disabled set to awake set.
        let encoded_body_sim_index = awake_body_count;
        let mut contact_key = world.bodies[body_id as usize].head_contact_key;
        while contact_key != NULL_INDEX {
            let edge_index = (contact_key & 1) as usize;
            let contact_id = contact_key >> 1;

            contact_key = world.contacts[contact_id as usize].edges[edge_index].next_key;

            if world.contacts[contact_id as usize].set_index != DISABLED_SET {
                debug_assert!(
                    world.contacts[contact_id as usize].set_index == AWAKE_SET
                        || world.contacts[contact_id as usize].set_index == set_index
                );

                // If this contact is awake but not touching I need to fixup the body sim index here.
                let contact = &mut world.contacts[contact_id as usize];
                if contact.set_index == AWAKE_SET {
                    debug_assert!(contact.color_index == NULL_INDEX);
                    if edge_index == 0 {
                        contact.encoded_body_sim_a = encoded_body_sim_index;
                    } else {
                        contact.encoded_body_sim_b = encoded_body_sim_index;
                    }
                }

                continue;
            }

            let local_index = world.contacts[contact_id as usize].local_index;

            debug_assert!(
                0 <= local_index
                    && (local_index as usize)
                        < world.solver_sets[DISABLED_SET as usize]
                            .contact_indices
                            .len()
            );
            debug_assert!(
                world.solver_sets[DISABLED_SET as usize].contact_indices[local_index as usize]
                    == contact_id
            );

            debug_assert!(
                (world.contacts[contact_id as usize].flags & contact_flags::TOUCHING) == 0
                    && world.contacts[contact_id as usize].manifold_count() == 0
            );

            {
                let awake_contact_count =
                    world.solver_sets[AWAKE_SET as usize].contact_indices.len() as i32;
                let contact = &mut world.contacts[contact_id as usize];
                contact.set_index = AWAKE_SET;
                contact.local_index = awake_contact_count;
            }
            world.solver_sets[AWAKE_SET as usize]
                .contact_indices
                .push(contact_id);

            {
                let contact = &mut world.contacts[contact_id as usize];
                if edge_index == 0 {
                    contact.encoded_body_sim_a = encoded_body_sim_index;
                } else {
                    contact.encoded_body_sim_b = encoded_body_sim_index;
                }
            }

            let disabled_contacts = &mut world.solver_sets[DISABLED_SET as usize].contact_indices;
            let moved_local_index = disabled_contacts.len() as i32 - 1;
            disabled_contacts.swap_remove(local_index as usize);
            if moved_local_index != local_index {
                // fix moved element
                let moved_contact_index = disabled_contacts[local_index as usize];
                let moved_contact = &mut world.contacts[moved_contact_index as usize];
                debug_assert!(moved_contact.local_index == moved_local_index);
                moved_contact.local_index = local_index;
            }
        }
    }

    // Transfer touching contacts from sleeping set to constraint graph.
    {
        let contact_count = world.solver_sets[set_index as usize].contact_indices.len();
        for i in 0..contact_count {
            let contact_id = world.solver_sets[set_index as usize].contact_indices[i];
            debug_assert!(
                (world.contacts[contact_id as usize].flags & contact_flags::TOUCHING) != 0
            );
            debug_assert!(
                (world.contacts[contact_id as usize].flags & contact_flags::SIM_TOUCHING) != 0
            );
            debug_assert!(world.contacts[contact_id as usize].set_index == set_index);
            add_contact_to_graph(world, contact_id);
            world.contacts[contact_id as usize].set_index = AWAKE_SET;
        }
    }

    // transfer joints from sleeping set to awake set
    {
        let joint_count = world.solver_sets[set_index as usize].joint_sims.len();
        for i in 0..joint_count {
            let joint_sim = world.solver_sets[set_index as usize].joint_sims[i];
            let joint_id = joint_sim.joint_id;
            debug_assert!(world.joints[joint_id as usize].set_index == set_index);
            add_joint_to_graph(world, joint_sim, joint_id);
            world.joints[joint_id as usize].set_index = AWAKE_SET;
        }
    }

    // transfer island from sleeping set to awake set
    // Usually a sleeping set has only one island, but it is possible
    // that joints are created between sleeping islands and they
    // are moved to the same sleeping set.
    let island_count = world.solver_sets[set_index as usize].island_sims.len();
    for i in 0..island_count {
        let island_src = world.solver_sets[set_index as usize].island_sims[i];
        let island_id = island_src.island_id;
        let awake_island_count = world.solver_sets[AWAKE_SET as usize].island_sims.len() as i32;
        {
            let island = &mut world.islands[island_id as usize];
            island.set_index = AWAKE_SET;
            island.local_index = awake_island_count;
        }
        world.solver_sets[AWAKE_SET as usize]
            .island_sims
            .push(island_src);
    }

    // destroy the sleeping set
    destroy_solver_set(world, set_index);
}
