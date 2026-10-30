// Primordial Engine fork (D2): the determinism scenes give the same sleep step and state hash
// for every worker count, and a world's worker count can change between steps.
//
// SPDX-License-Identifier: MIT

use crate::determinism::{
    create_falling_ragdolls, create_mesh_drop, create_wave_pile, update_falling_ragdolls,
    update_mesh_drop, update_wave_pile,
};
use crate::types::default_world_def;
use crate::world::{world_set_worker_count, World};

const WORKER_COUNTS: [i32; 5] = [1, 2, 3, 4, 8];

fn world_with(workers: i32) -> World {
    let mut def = default_world_def();
    def.worker_count = workers;
    World::new(&def)
}

/// Step until settled; `workers_at(step)` may change the worker count mid-run.
fn run<D>(
    workers: i32,
    workers_at: impl Fn(i32) -> Option<i32>,
    create: fn(&mut World) -> D,
    update: fn(&World, &mut D) -> bool,
    result: fn(&D) -> (i32, u32),
) -> (i32, u32) {
    let mut world = world_with(workers);
    let mut data = create(&mut world);
    for step in 0..1000 {
        if let Some(n) = workers_at(step) {
            world_set_worker_count(&mut world, n);
        }
        world.step(1.0 / 60.0, 4);
        if update(&world, &mut data) {
            break;
        }
    }
    result(&data)
}

#[test]
fn wave_pile_same_for_every_worker_count() {
    let reference = run(1, |_| None, create_wave_pile, update_wave_pile, |d| (d.sleep_step, d.hash));
    for workers in WORKER_COUNTS {
        let got = run(workers, |_| None, create_wave_pile, update_wave_pile, |d| (d.sleep_step, d.hash));
        assert_eq!(got, reference, "{workers} workers");
    }
}

#[test]
fn ragdolls_same_for_every_worker_count() {
    let f = |w| {
        run(w, |_| None, create_falling_ragdolls, update_falling_ragdolls, |d| {
            (d.sleep_step, d.hash)
        })
    };
    let reference = f(1);
    for workers in WORKER_COUNTS {
        assert_eq!(f(workers), reference, "{workers} workers");
    }
}

#[test]
fn mesh_drop_same_for_every_worker_count() {
    let create = |world: &mut World| create_mesh_drop(world, crate::math_functions::POS_ZERO);
    let f = |w| run(w, |_| None, create, update_mesh_drop, |d| (d.sleep_step, d.hash));
    let reference = f(1);
    for workers in WORKER_COUNTS {
        assert_eq!(f(workers), reference, "{workers} workers");
    }
}

#[test]
fn worker_count_can_change_between_steps() {
    let reference = run(1, |_| None, create_wave_pile, update_wave_pile, |d| (d.sleep_step, d.hash));
    let switching = run(
        2,
        |step| match step % 37 {
            0 => Some(4),
            12 => Some(1),
            25 => Some(3),
            _ => None,
        },
        create_wave_pile,
        update_wave_pile,
        |d| (d.sleep_step, d.hash),
    );
    assert_eq!(switching, reference);
}
