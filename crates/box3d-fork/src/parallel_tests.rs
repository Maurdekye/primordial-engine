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

/// Snapshot at frame 60 with 1 worker; restore it into a world with 4 workers that has run on
/// to frame 100, and into a fresh 3-worker world; both continue exactly like the 1-worker run.
/// (Review of 73eaedc, non-blocking.)
#[test]
fn snapshot_restores_across_worker_counts() {
    use crate::recording::{deserialize_into_shell, hash_world_state, serialize_world, GeometryRegistry, RecBuffer};
    let step = |world: &mut World, n: usize| {
        for _ in 0..n {
            world.step(1.0 / 60.0, 4);
        }
    };

    let mut reference = world_with(1);
    create_wave_pile(&mut reference);
    step(&mut reference, 60);
    let mut registry = GeometryRegistry::new();
    let mut buf = RecBuffer::new();
    serialize_world(&reference, &mut buf, &mut registry);
    step(&mut reference, 90);
    let expected = hash_world_state(&reference);

    let mut slots = registry.to_slots();
    let mut ahead = world_with(4);
    create_wave_pile(&mut ahead);
    step(&mut ahead, 100);
    assert!(deserialize_into_shell(&buf.data, &mut ahead, &mut slots));
    step(&mut ahead, 90);
    assert_eq!(hash_world_state(&ahead), expected, "in place, 4 workers");

    let mut fresh = world_with(3);
    assert!(deserialize_into_shell(&buf.data, &mut fresh, &mut slots));
    step(&mut fresh, 90);
    assert_eq!(hash_world_state(&fresh), expected, "fresh world, 3 workers");
}

/// Hit events with every shape asking for them: the same events, step by step, at every worker count. Each worker flags
/// hits in its own task context during the store stage, and before every worker's bit set was sized a helper's first
/// hit bit panicked (pe-physics turns hit events on for every shape).
#[test]
fn hit_events_same_for_every_worker_count() {
    use crate::body::create_body;
    use crate::hull::make_box_hull;
    use crate::math_functions::Pos;
    use crate::shape::create_hull_shape;
    use crate::types::{default_body_def, default_shape_def, BodyType};

    let events = |workers: i32| {
        let mut world = world_with(workers);
        let mut shape = default_shape_def();
        shape.enable_hit_events = true;
        let mut def = default_body_def();
        def.position = Pos { x: 0.0 as _, y: -0.5 as _, z: 0.0 as _ };
        let ground = create_body(&mut world, &def);
        create_hull_shape(&mut world, ground, &shape, &make_box_hull(20.0, 0.5, 20.0).base);
        // 120 boxes dropped from up to 3 m, enough awake bodies for the parallel solve.
        for i in 0..120 {
            let mut def = default_body_def();
            def.type_ = BodyType::Dynamic;
            def.position = Pos {
                x: ((i % 12) as f32 * 0.6 - 3.3) as _,
                y: (0.5 + (i / 12) as f32 * 0.3) as _,
                z: ((i % 7) as f32 * 0.2) as _,
            };
            let id = create_body(&mut world, &def);
            create_hull_shape(&mut world, id, &shape, &make_box_hull(0.2, 0.2, 0.2).base);
        }
        let mut all = Vec::new();
        for _ in 0..120 {
            world.step(1.0 / 60.0, 4);
            for e in &world.contact_hit_events {
                all.push((e.shape_id_a.index1, e.shape_id_b.index1, e.approach_speed.to_bits(), e.normal.x.to_bits()));
            }
            all.push((-1, -1, 0, 0));
        }
        all
    };
    let one = events(1);
    assert!(one.iter().any(|e| e.0 >= 0), "the drop should give hits");
    for &workers in &WORKER_COUNTS[1..] {
        assert_eq!(events(workers), one, "{workers} workers");
    }
}
