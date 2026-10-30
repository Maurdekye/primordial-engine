//! Contact body sim locator tests from test_world.c (BodySimLocatorTest, LocatorFastFlagTest).
//!
//! The contact body sim locators are persistent state. Every contact in every set carries the
//! current encoding for both of its bodies: the awake local index, a static tag, or null for a
//! body that fell asleep. The scenes below drive each site that can invalidate one. The gate is
//! `World::validate_solver_sets`, which asserts the invariant on every mutation and every step
//! (debug builds), so a dropped refresh trips there rather than producing a plausible but wrong
//! simulation.
//!
//! C runs each scene at worker counts 1 and 4 and compares the state hashes. The serial port has
//! no worker count, so each scene runs twice in fresh worlds and the hashes must agree.

use crate::body::{
    body_disable, body_enable, body_enable_sleep, body_flags, body_is_awake, body_set_awake,
    body_set_linear_velocity, body_set_motion_locks, body_set_type, create_body, destroy_body,
    get_body_full_id, get_body_sim,
};
use crate::geometry::Sphere;
use crate::hull::make_box_hull;
use crate::id::BodyId;
use crate::joint::create_spherical_joint;
use crate::math_functions::{Pos, Vec3, VEC3_ZERO};
use crate::recording::hash_world_state;
use crate::shape::{create_hull_shape, create_sphere_shape};
use crate::types::{
    default_body_def, default_shape_def, default_spherical_joint_def, default_world_def, BodyType,
    MotionLocks,
};
use crate::world::World;

fn pos(x: f32, y: f32, z: f32) -> Pos {
    Pos {
        x: x as _,
        y: y as _,
        z: z as _,
    }
}

/// (CreateLocatorWorld)
fn create_locator_world() -> World {
    let world_def = default_world_def();
    World::new(&world_def)
}

/// (CreateLocatorBody)
fn create_locator_body(
    world: &mut World,
    type_: BodyType,
    position: Pos,
    hx: f32,
    hy: f32,
    hz: f32,
) -> BodyId {
    let mut body_def = default_body_def();
    body_def.type_ = type_;
    body_def.position = position;
    let body_id = create_body(world, &body_def);

    let shape_def = default_shape_def();
    let box_hull = make_box_hull(hx, hy, hz);
    create_hull_shape(world, body_id, &shape_def, &box_hull.base);

    body_id
}

/// (StepLocatorWorld)
fn step_locator_world(world: &mut World, step_count: i32) {
    for _ in 0..step_count {
        world.step(1.0 / 60.0, 4);
    }
}

/// Steps until the body sleeps or the budget runs out. Returns the steps taken.
/// (StepUntilAsleep)
fn step_until_asleep(world: &mut World, body_id: BodyId, max_steps: i32) -> i32 {
    for i in 0..max_steps {
        world.step(1.0 / 60.0, 4);
        if !body_is_awake(world, body_id) {
            return i + 1;
        }
    }

    max_steps
}

/// Sleep and wake: first touch puts contacts in the graph, sleeping nulls the locators of the
/// island bodies, waking restores their awake indices. The plate beside the stack holds a
/// non-touching contact across the whole cycle, and that is the contact the wake path has to fix
/// up itself. A touching contact would get a fresh encoding from the graph add and hide a dropped
/// refresh. (LocatorSleepWakeScene)
fn locator_sleep_wake_scene(failed: &mut bool) -> u64 {
    let mut world = create_locator_world();
    let w = &mut world;
    create_locator_body(w, BodyType::Static, pos(0.0, -1.0, 0.0), 5.0, 1.0, 5.0);

    let mut stack_id = [BodyId::default(); 3];
    for (i, id) in stack_id.iter_mut().enumerate() {
        *id = create_locator_body(
            w,
            BodyType::Dynamic,
            pos(0.0, 0.5 + i as f32, 0.0),
            0.5,
            0.5,
            0.5,
        );
    }

    // Beside the top box, close enough that the fat bounds overlap and far enough that the
    // shapes never touch. Kinematic and awake, so the contact stays in the awake set while the
    // stack sleeps. Off to the side so it does not catch the body dropped later.
    let plate_id = create_locator_body(w, BodyType::Kinematic, pos(1.04, 2.5, 0.0), 0.5, 0.5, 0.5);
    body_enable_sleep(w, plate_id, false);

    let steps = step_until_asleep(w, stack_id[0], 300);
    if steps == 300 || body_is_awake(w, stack_id[2]) {
        *failed = true;
    }

    // Wake without contact so the non-touching contact keeps its stale locator if the wake
    // path forgets to refresh
    body_set_awake(w, stack_id[0], true);
    step_locator_world(w, 5);

    if !body_is_awake(w, stack_id[2]) {
        *failed = true;
    }

    let steps = step_until_asleep(w, stack_id[0], 300);
    if steps == 300 {
        *failed = true;
    }

    // Dropping a body on the pile drives the wake path through first touch. The pile can settle
    // and sleep again, so catch the wake as it happens.
    create_locator_body(w, BodyType::Dynamic, pos(0.0, 8.0, 0.0), 0.5, 0.5, 0.5);

    let mut woke = false;
    for _ in 0..180 {
        w.step(1.0 / 60.0, 4);
        woke = woke || body_is_awake(w, stack_id[0]);
    }

    if !woke {
        *failed = true;
    }

    hash_world_state(&world)
}

/// An island falling asleep and a body leaving the awake set both swap the last awake sim into
/// the hole. The pile that never sleeps supplies a moved body that still owns contacts.
/// (LocatorAwakeNeighborScene)
fn locator_awake_neighbor_scene(failed: &mut bool) -> u64 {
    let mut world = create_locator_world();
    let w = &mut world;
    create_locator_body(w, BodyType::Static, pos(0.0, -1.0, 0.0), 30.0, 1.0, 5.0);

    // This island sleeps
    let mut sleeper_id = [BodyId::default(); 2];
    for (i, id) in sleeper_id.iter_mut().enumerate() {
        *id = create_locator_body(
            w,
            BodyType::Dynamic,
            pos(-10.0, 0.5 + i as f32, 0.0),
            0.5,
            0.5,
            0.5,
        );
    }

    // Created after the sleepers so its sims sit at the end of the awake set and get swapped
    // into the holes the sleeping island leaves behind
    let spare_id = create_locator_body(w, BodyType::Dynamic, pos(20.0, 0.5, 0.0), 0.5, 0.5, 0.5);
    body_enable_sleep(w, spare_id, false);

    let mut awake_id = [BodyId::default(); 3];
    for (i, id) in awake_id.iter_mut().enumerate() {
        *id = create_locator_body(
            w,
            BodyType::Dynamic,
            pos(10.0, 0.5 + i as f32, 0.0),
            0.5,
            0.5,
            0.5,
        );
        body_enable_sleep(w, *id, false);
    }

    let steps = step_until_asleep(w, sleeper_id[0], 300);
    if steps == 300 || !body_is_awake(w, awake_id[0]) {
        *failed = true;
    }

    step_locator_world(w, 10);

    // Leaving the awake set swaps a body that owns contacts into the spare's slot
    body_disable(w, spare_id);
    step_locator_world(w, 10);

    body_enable(w, spare_id);
    step_locator_world(w, 10);

    hash_world_state(&world)
}

/// Destroying a body swaps the last sim of its set into the hole. The moved body keeps its
/// contacts, so its side of each one has to follow it. (LocatorDestroyScene)
fn locator_destroy_scene(_failed: &mut bool) -> u64 {
    let mut world = create_locator_world();
    let w = &mut world;
    create_locator_body(w, BodyType::Static, pos(0.0, -1.0, 0.0), 5.0, 1.0, 5.0);

    let mut stack_id = [BodyId::default(); 4];
    for (i, id) in stack_id.iter_mut().enumerate() {
        *id = create_locator_body(
            w,
            BodyType::Dynamic,
            pos(0.0, 0.5 + i as f32, 0.0),
            0.5,
            0.5,
            0.5,
        );
    }

    step_locator_world(w, 30);

    // The last awake sim moves into this hole while its contacts live
    destroy_body(w, stack_id[0]);
    step_locator_world(w, 30);

    // Three static boxes, one resting box each. Destroying the first moves the last static sim.
    let mut static_id = [BodyId::default(); 3];
    for (i, id) in static_id.iter_mut().enumerate() {
        let x = 10.0 + 4.0 * i as f32;
        *id = create_locator_body(w, BodyType::Static, pos(x, -1.0, 0.0), 1.0, 1.0, 1.0);
        create_locator_body(w, BodyType::Dynamic, pos(x, 0.5, 0.0), 0.5, 0.5, 0.5);
    }

    step_locator_world(w, 30);

    destroy_body(w, static_id[0]);
    step_locator_world(w, 30);

    hash_world_state(&world)
}

/// A kinematic body parked a hair above a pile keeps a non-touching contact to a body that falls
/// asleep, which is the one case the locator cannot name and has to leave to the cold path.
/// (LocatorKinematicScene)
fn locator_kinematic_scene(failed: &mut bool) -> u64 {
    let mut world = create_locator_world();
    let w = &mut world;
    create_locator_body(w, BodyType::Static, pos(0.0, -1.0, 0.0), 5.0, 1.0, 5.0);

    let box_id = create_locator_body(w, BodyType::Dynamic, pos(0.0, 0.5, 0.0), 0.5, 0.5, 0.5);

    // Close enough that the fat bounds overlap and a non-touching contact exists
    let plate_id = create_locator_body(w, BodyType::Kinematic, pos(0.0, 1.52, 0.0), 1.0, 0.5, 1.0);

    let steps = step_until_asleep(w, box_id, 300);
    if steps == 300 {
        *failed = true;
    }

    // Drive the plate down to touch and wake the sleeper
    body_set_linear_velocity(w, plate_id, Vec3::new(0.0, -1.0, 0.0));
    step_locator_world(w, 60);

    if !body_is_awake(w, box_id) {
        *failed = true;
    }

    hash_world_state(&world)
}

/// Set type and disable move a body between solver sets, which changes the encoding of every
/// contact it has. A joint across two sleeping sets drives the merge path.
/// (LocatorTransferScene)
fn locator_transfer_scene(failed: &mut bool) -> u64 {
    let mut world = create_locator_world();
    let w = &mut world;
    create_locator_body(w, BodyType::Static, pos(0.0, -1.0, 0.0), 20.0, 1.0, 5.0);

    let box_id = create_locator_body(w, BodyType::Dynamic, pos(0.0, 0.5, 0.0), 0.5, 0.5, 0.5);
    step_locator_world(w, 30);

    body_set_type(w, box_id, BodyType::Static);
    step_locator_world(w, 5);
    body_set_type(w, box_id, BodyType::Dynamic);
    step_locator_world(w, 5);

    body_disable(w, box_id);
    step_locator_world(w, 5);
    body_enable(w, box_id);
    step_locator_world(w, 30);

    // Two piles far enough apart to sleep in separate sets
    let sleeper_id = [
        create_locator_body(w, BodyType::Dynamic, pos(-12.0, 0.5, 0.0), 0.5, 0.5, 0.5),
        create_locator_body(w, BodyType::Dynamic, pos(12.0, 0.5, 0.0), 0.5, 0.5, 0.5),
    ];

    let steps = step_until_asleep(w, sleeper_id[0], 300);
    if steps == 300 || body_is_awake(w, sleeper_id[1]) {
        *failed = true;
    }

    let mut joint_def = default_spherical_joint_def();
    joint_def.base.body_id_a = sleeper_id[0];
    joint_def.base.body_id_b = sleeper_id[1];
    create_spherical_joint(w, &joint_def);

    step_locator_world(w, 30);

    hash_world_state(&world)
}

/// The fast flag lives on the body sim and gates contact recycling, but a flag sync rebuilds the
/// sim flags from the body row, which never carries it. A public setter called on a body in
/// flight must not drop it, or the recycle path can take a fast pair. (LocatorFastFlagTest)
#[test]
fn locator_fast_flag_test() {
    let world_def = default_world_def();
    let mut world = World::new(&world_def);
    let w = &mut world;

    create_locator_body(w, BodyType::Static, pos(0.0, -1.0, 0.0), 5.0, 1.0, 5.0);

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Dynamic;
    body_def.position = pos(-20.0, 2.0, 0.0);
    body_def.linear_velocity = Vec3::new(400.0, 0.0, 0.0);
    let bullet_id = create_body(w, &body_def);

    let shape_def = default_shape_def();
    let sphere = Sphere {
        center: VEC3_ZERO,
        radius: 0.1,
    };
    create_sphere_shape(w, bullet_id, &shape_def, &sphere);

    w.step(1.0 / 60.0, 4);

    let body_index = get_body_full_id(w, bullet_id);
    assert!((get_body_sim(w, body_index).flags & body_flags::IS_FAST) != 0);

    // Any public setter that syncs flags. The locks have to actually change or the setter
    // returns early and never syncs.
    let locks = MotionLocks {
        angular_x: true,
        ..MotionLocks::default()
    };
    body_set_motion_locks(w, bullet_id, locks);

    let body_index = get_body_full_id(w, bullet_id);
    assert!((get_body_sim(w, body_index).flags & body_flags::IS_FAST) != 0);

    w.step(1.0 / 60.0, 4);
}

/// (BodySimLocatorTest)
#[test]
fn body_sim_locator_test() {
    type LocatorScene = fn(&mut bool) -> u64;
    let scenes: [LocatorScene; 5] = [
        locator_sleep_wake_scene,
        locator_awake_neighbor_scene,
        locator_destroy_scene,
        locator_kinematic_scene,
        locator_transfer_scene,
    ];

    for (i, scene) in scenes.iter().enumerate() {
        let mut failed = false;
        let hash1 = scene(&mut failed);
        let hash2 = scene(&mut failed);
        assert!(!failed, "scene {i} failed");
        assert_eq!(hash1, hash2, "scene {i} hash mismatch");
    }
}
