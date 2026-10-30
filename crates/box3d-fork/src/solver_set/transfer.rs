// Merge solver sets and transfer bodies and joints between sets. (b3MergeSolverSets,
// b3TransferBody, b3TransferJoint from solver_set.c)
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::{destroy_solver_set, AWAKE_SET, FIRST_SLEEPING_SET};
use crate::core::NULL_INDEX;
use crate::world::World;

/// This is called when joints are created between sets. I want to allow the sets
/// to continue sleeping if both are asleep. Otherwise one set is waked.
/// Islands will get merged when the set is woke. (b3MergeSolverSets)
pub fn merge_solver_sets(world: &mut World, set_id1: i32, set_id2: i32) {
    debug_assert!(set_id1 >= FIRST_SLEEPING_SET);
    debug_assert!(set_id2 >= FIRST_SLEEPING_SET);

    // Move the fewest number of bodies
    let count1 = world.solver_sets[set_id1 as usize].body_sims.len();
    let count2 = world.solver_sets[set_id2 as usize].body_sims.len();
    let (set_id1, set_id2) = if count1 < count2 {
        (set_id2, set_id1)
    } else {
        (set_id1, set_id2)
    };

    // transfer bodies
    {
        let body_count = world.solver_sets[set_id2 as usize].body_sims.len();
        for i in 0..body_count {
            let sim_src = world.solver_sets[set_id2 as usize].body_sims[i];

            let target_count = world.solver_sets[set_id1 as usize].body_sims.len() as i32;
            let body = &mut world.bodies[sim_src.body_id as usize];
            debug_assert!(body.set_index == set_id2);
            body.set_index = set_id1;
            body.local_index = target_count;

            world.solver_sets[set_id1 as usize].body_sims.push(sim_src);
        }
    }

    // transfer contacts
    {
        let contact_count = world.solver_sets[set_id2 as usize].contact_indices.len();
        for i in 0..contact_count {
            let contact_index = world.solver_sets[set_id2 as usize].contact_indices[i];

            let target_count = world.solver_sets[set_id1 as usize].contact_indices.len() as i32;
            let contact = &mut world.contacts[contact_index as usize];
            debug_assert!(contact.set_index == set_id2);
            contact.set_index = set_id1;
            contact.local_index = target_count;

            world.solver_sets[set_id1 as usize]
                .contact_indices
                .push(contact_index);
        }
    }

    // transfer joints
    {
        let joint_count = world.solver_sets[set_id2 as usize].joint_sims.len();
        for i in 0..joint_count {
            let joint_src = world.solver_sets[set_id2 as usize].joint_sims[i];

            let target_count = world.solver_sets[set_id1 as usize].joint_sims.len() as i32;
            let joint = &mut world.joints[joint_src.joint_id as usize];
            debug_assert!(joint.set_index == set_id2);
            joint.set_index = set_id1;
            joint.local_index = target_count;

            world.solver_sets[set_id1 as usize]
                .joint_sims
                .push(joint_src);
        }
    }

    // transfer islands
    {
        let island_count = world.solver_sets[set_id2 as usize].island_sims.len();
        for i in 0..island_count {
            let island_src = world.solver_sets[set_id2 as usize].island_sims[i];
            let island_id = island_src.island_id;

            let target_count = world.solver_sets[set_id1 as usize].island_sims.len() as i32;
            let island = &mut world.islands[island_id as usize];
            island.set_index = set_id1;
            island.local_index = target_count;

            world.solver_sets[set_id1 as usize]
                .island_sims
                .push(island_src);
        }
    }

    // destroy the merged set
    destroy_solver_set(world, set_id2);

    world.validate_solver_sets();
}

/// Move a body sim between solver sets. (b3TransferBody)
pub fn transfer_body(
    world: &mut World,
    target_set_index: i32,
    source_set_index: i32,
    body_id: i32,
) {
    use crate::body::{body_flags, IDENTITY_BODY_STATE};

    debug_assert!(world.bodies[body_id as usize].head_contact_key == NULL_INDEX);

    if target_set_index == source_set_index {
        return;
    }

    let source_index = world.bodies[body_id as usize].local_index;
    let mut source_sim =
        world.solver_sets[source_set_index as usize].body_sims[source_index as usize];

    // Clear transient body flags
    world.bodies[body_id as usize].flags &= !body_flags::BODY_TRANSIENT_FLAGS;
    source_sim.flags &= !(body_flags::IS_FAST | body_flags::BODY_TRANSIENT_FLAGS);

    let target_index = world.solver_sets[target_set_index as usize].body_sims.len() as i32;
    world.solver_sets[target_set_index as usize]
        .body_sims
        .push(source_sim);

    // Remove body sim from solver set that owns it
    {
        let source_sims = &mut world.solver_sets[source_set_index as usize].body_sims;
        let moved_index = source_sims.len() as i32 - 1;
        source_sims.swap_remove(source_index as usize);
        if moved_index != source_index {
            // Fix moved body index
            let moved_id = source_sims[source_index as usize].body_id;
            let moved_body = &mut world.bodies[moved_id as usize];
            debug_assert!(moved_body.local_index == moved_index);
            moved_body.local_index = source_index;
            crate::body::refresh_body_contact_indices(world, moved_id);
        }
    }

    if source_set_index == AWAKE_SET {
        world.solver_sets[source_set_index as usize]
            .body_states
            .swap_remove(source_index as usize);
    } else if target_set_index == AWAKE_SET {
        let mut state = IDENTITY_BODY_STATE;
        state.flags = world.bodies[body_id as usize].flags;
        world.solver_sets[target_set_index as usize]
            .body_states
            .push(state);
    }

    let body = &mut world.bodies[body_id as usize];
    body.set_index = target_set_index;
    body.local_index = target_index;
}

/// Move a joint sim between solver sets or the constraint graph. (b3TransferJoint)
pub fn transfer_joint(
    world: &mut World,
    target_set_index: i32,
    source_set_index: i32,
    joint_id: i32,
) {
    use crate::constants::GRAPH_COLOR_COUNT;
    use crate::constraint_graph::{add_joint_to_graph, remove_joint_from_graph};

    if target_set_index == source_set_index {
        return;
    }

    let local_index = world.joints[joint_id as usize].local_index;
    let color_index = world.joints[joint_id as usize].color_index;

    // Retrieve source.
    let source_sim = if source_set_index == AWAKE_SET {
        debug_assert!(0 <= color_index && color_index < GRAPH_COLOR_COUNT);
        world.constraint_graph.colors[color_index as usize].joint_sims[local_index as usize]
    } else {
        debug_assert!(color_index == NULL_INDEX);
        world.solver_sets[source_set_index as usize].joint_sims[local_index as usize]
    };

    // Create target and copy. Fix joint.
    if target_set_index == AWAKE_SET {
        add_joint_to_graph(world, source_sim, joint_id);
        world.joints[joint_id as usize].set_index = AWAKE_SET;
    } else {
        let target_count = world.solver_sets[target_set_index as usize]
            .joint_sims
            .len() as i32;
        {
            let joint = &mut world.joints[joint_id as usize];
            joint.set_index = target_set_index;
            joint.local_index = target_count;
            joint.color_index = NULL_INDEX;
        }
        world.solver_sets[target_set_index as usize]
            .joint_sims
            .push(source_sim);
    }

    // Destroy source.
    if source_set_index == AWAKE_SET {
        let body_id_a = world.joints[joint_id as usize].edges[0].body_id;
        let body_id_b = world.joints[joint_id as usize].edges[1].body_id;
        remove_joint_from_graph(world, body_id_a, body_id_b, color_index, local_index);
    } else {
        let source_sims = &mut world.solver_sets[source_set_index as usize].joint_sims;
        let moved_index = source_sims.len() as i32 - 1;
        source_sims.swap_remove(local_index as usize);
        if moved_index != local_index {
            // fix swapped element
            let moved_id = source_sims[local_index as usize].joint_id;
            world.joints[moved_id as usize].local_index = local_index;
        }
    }
}
