// Focused unit tests for broad_phase proxy ops and tree marks, plus the world-level tests
// from test_broad_phase.c.
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use crate::broad_phase::{proxy_id, proxy_key, proxy_type, BroadPhase};
use crate::dynamic_tree::DEFAULT_CATEGORY_BITS;
use crate::math_functions::{Aabb, Vec3};
use crate::types::{BodyType, Capacity};

fn unit_aabb(x: f32, y: f32, z: f32) -> Aabb {
    Aabb {
        lower_bound: Vec3 {
            x: x - 0.5,
            y: y - 0.5,
            z: z - 0.5,
        },
        upper_bound: Vec3 {
            x: x + 0.5,
            y: y + 0.5,
            z: z + 0.5,
        },
    }
}

#[test]
fn proxy_key_round_trip() {
    for type_ in [BodyType::Static, BodyType::Kinematic, BodyType::Dynamic] {
        for id in [0, 1, 7, 100] {
            let key = proxy_key(id, type_);
            assert_eq!(proxy_type(key), type_);
            assert_eq!(proxy_id(key), id);
        }
    }
}

#[test]
fn create_destroy_and_move_marks() {
    let capacity = Capacity {
        static_shape_count: 8,
        dynamic_shape_count: 8,
        static_body_count: 0,
        dynamic_body_count: 0,
        contact_count: 8,
    };
    let mut bp = BroadPhase::new(&capacity);
    let static_tree = BodyType::Static as usize;
    let dynamic_tree = BodyType::Dynamic as usize;

    // Static without forcePairCreation does not mark the tree.
    let static_key = bp.create_proxy(
        BodyType::Static,
        unit_aabb(0.0, 0.0, 0.0),
        DEFAULT_CATEGORY_BITS,
        10,
        false,
    );
    assert!(!bp.trees[static_tree].has_tree_moved());
    assert_eq!(bp.shape_index(static_key), 10);

    // Dynamic always marks.
    let dyn_key = bp.create_proxy(
        BodyType::Dynamic,
        unit_aabb(2.0, 0.0, 0.0),
        DEFAULT_CATEGORY_BITS,
        20,
        false,
    );
    assert!(bp.trees[dynamic_tree].has_tree_moved());
    assert_eq!(bp.shape_index(dyn_key), 20);

    // Static with forcePairCreation marks.
    let static_forced = bp.create_proxy(
        BodyType::Static,
        unit_aabb(-2.0, 0.0, 0.0),
        DEFAULT_CATEGORY_BITS,
        11,
        true,
    );
    assert!(bp.trees[static_tree].has_tree_moved());
    let mut moved = [0i32; 4];
    let moved_count = bp.trees[static_tree].gather_moved_proxies(&mut moved);
    assert_eq!(moved_count, 1);
    assert_eq!(moved[0], proxy_id(static_forced));

    // Destroying the only moved proxy re-derives the marks along its path.
    bp.destroy_proxy(dyn_key);
    assert!(!bp.trees[dynamic_tree].has_tree_moved());

    bp.trees[static_tree].clear_moved();
    bp.validate_no_moved();

    bp.destroy();
}

#[test]
fn move_and_overlap() {
    let capacity = Capacity {
        static_shape_count: 4,
        dynamic_shape_count: 4,
        ..Capacity::default()
    };
    let mut bp = BroadPhase::new(&capacity);
    let dynamic_tree = BodyType::Dynamic as usize;

    let a = bp.create_proxy(
        BodyType::Dynamic,
        unit_aabb(0.0, 0.0, 0.0),
        DEFAULT_CATEGORY_BITS,
        1,
        false,
    );
    let b = bp.create_proxy(
        BodyType::Dynamic,
        unit_aabb(0.25, 0.0, 0.0),
        DEFAULT_CATEGORY_BITS,
        2,
        false,
    );
    assert!(bp.test_overlap(a, b));

    bp.trees[dynamic_tree].rebuild(false);
    bp.validate_no_moved();

    bp.move_proxy(b, unit_aabb(10.0, 0.0, 0.0));
    assert!(!bp.test_overlap(a, b));
    let mut moved = [0i32; 4];
    assert_eq!(bp.trees[dynamic_tree].gather_moved_proxies(&mut moved), 1);
    assert_eq!(moved[0], proxy_id(b));

    // A marked proxy with a grown box is found by the moved gather and refit.
    let grown = Aabb {
        lower_bound: Vec3 {
            x: -1.0,
            y: -1.0,
            z: -1.0,
        },
        upper_bound: Vec3 {
            x: 1.0,
            y: 1.0,
            z: 1.0,
        },
    };
    bp.mark_proxy_moved(a, grown);
    assert_eq!(bp.trees[dynamic_tree].gather_moved_proxies(&mut moved), 2);
    bp.trees[dynamic_tree].refit();
    assert_eq!(bp.trees[dynamic_tree].aabb(proxy_id(a)), grown);

    bp.validate();
    bp.trees[dynamic_tree].rebuild(false);
    bp.validate_no_moved();
}

#[test]
fn defaults_have_secret_cookie() {
    use crate::core::SECRET_COOKIE;
    use crate::types::{default_body_def, default_filter, default_world_def};

    assert_eq!(default_body_def().internal_value, SECRET_COOKIE);
    assert_eq!(default_world_def().internal_value, SECRET_COOKIE);
    assert_eq!(default_filter().group_index, 0);
}

thread_local! {
    static FILTER_CALLS: std::cell::RefCell<Vec<(i32, i32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn recording_filter(
    _world: &crate::world::World,
    id_a: crate::id::ShapeId,
    id_b: crate::id::ShapeId,
    _context: u64,
) -> bool {
    FILTER_CALLS.with(|calls| calls.borrow_mut().push((id_a.index1 - 1, id_b.index1 - 1)));
    true
}

/// C runs the cross pass through b3ParallelFor. With the default serial task system worker
/// 0 claims every block in order, and b3CrossPairsTask flushes its candidate batch at the
/// end of each block. With one worker and two cross seeds the block size is 1 (minRange 1,
/// 32 blocks per worker), so each seed is flushed on its own and the custom filter sees the
/// pairs in seed order. A single flush over both seeds would instead emit the compound pair
/// first, since b3FlushCandidatePairs expands compounds before filtering plain pairs.
///
/// Scene (the order follows from dynamic_tree.c b3InsertLeaf and broad_phase.c
/// b3GatherCrossSeeds): the second proxy inserted into a tree lands at pair index 2 as the
/// right sibling, so in both trees the first created shape is the left child. The seeds are
/// gathered breadth first over (dynamic child, static child) in (left, right) order:
/// seed 0 = (D1, plain static sphere), seed 1 = (D2, compound). C calls the filter for the
/// plain pair, then for the compound pair.
#[test]
fn cross_pass_flushes_per_parallel_for_block() {
    use crate::body::create_body;
    use crate::compound::{create_compound, CompoundDef, CompoundSphereDef};
    use crate::geometry::{default_surface_material, Sphere};
    use crate::math_functions::{Pos, VEC3_ZERO};
    use crate::shape::{create_baked_compound_shape, create_sphere_shape};
    use crate::types::{default_body_def, default_shape_def, default_world_def};
    use crate::world::{world_set_custom_filter_callback, World};

    FILTER_CALLS.with(|calls| calls.borrow_mut().clear());

    let mut world_def = default_world_def();
    world_def.gravity = VEC3_ZERO;
    let mut world = World::new(&world_def);
    assert_eq!(world.worker_count, 1);
    world_set_custom_filter_callback(&mut world, Some(recording_filter), 0);

    let mut shape_def = default_shape_def();
    shape_def.enable_custom_filtering = true;
    let sphere = Sphere {
        center: VEC3_ZERO,
        radius: 0.5,
    };

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Static;

    // Plain static sphere first, so it is the left child of the static tree.
    body_def.position = Pos {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    let plain_body = create_body(&mut world, &body_def);
    let plain_shape = create_sphere_shape(&mut world, plain_body, &shape_def, &sphere);

    let spheres = [CompoundSphereDef {
        sphere,
        material: default_surface_material(),
    }];
    let compound = create_compound(&CompoundDef {
        spheres: &spheres,
        ..CompoundDef::default()
    })
    .expect("compound");
    body_def.position = Pos {
        x: 10.0,
        y: 0.0,
        z: 0.0,
    };
    let compound_body = create_body(&mut world, &body_def);
    let compound_shape =
        create_baked_compound_shape(&mut world, compound_body, &shape_def, &compound);

    // Dynamic spheres: D1 over the plain sphere (left child), D2 over the compound.
    body_def.type_ = BodyType::Dynamic;
    body_def.position = Pos {
        x: 0.0,
        y: 0.25,
        z: 0.0,
    };
    let d1 = create_body(&mut world, &body_def);
    let d1_shape = create_sphere_shape(&mut world, d1, &shape_def, &sphere);
    body_def.position = Pos {
        x: 10.0,
        y: 0.25,
        z: 0.0,
    };
    let d2 = create_body(&mut world, &body_def);
    let d2_shape = create_sphere_shape(&mut world, d2, &shape_def, &sphere);

    crate::broad_phase::update_broad_phase_pairs(&mut world);

    let unordered = |a: i32, b: i32| (a.min(b), a.max(b));
    let calls: Vec<(i32, i32)> = FILTER_CALLS.with(|calls| {
        calls
            .borrow()
            .iter()
            .map(|&(a, b)| unordered(a, b))
            .collect()
    });
    let plain_pair = unordered(plain_shape.index1 - 1, d1_shape.index1 - 1);
    let compound_pair = unordered(compound_shape.index1 - 1, d2_shape.index1 - 1);
    assert_eq!(calls, vec![plain_pair, compound_pair]);
}

// ---------------------------------------------------------------------------------------------
// test_broad_phase.c (StaticRebuildKeepsMarks, BroadPhaseStressTest)

mod world_pairs {
    use crate::body::{
        body_disable, body_enable, body_get_contact_capacity, body_get_position, body_get_type,
        body_is_enabled, body_set_bullet, body_set_linear_velocity, body_set_transform,
        body_set_type, create_body, destroy_body, should_bodies_collide,
    };
    use crate::core::NULL_INDEX;
    use crate::dynamic_tree::DEFAULT_MASK_BITS;
    use crate::geometry::Sphere;
    use crate::height_field::create_grid;
    use crate::hull::{make_box_hull, BoxHull};
    use crate::id::{BodyId, JointId};
    use crate::joint::{create_weld_joint, joint_is_valid, joint_set_collide_connected};
    use crate::math_functions::{
        aabb_overlaps, abs_float, offset_pos, sub_pos, Pos, Vec3, POS_ZERO, QUAT_IDENTITY,
        VEC3_ZERO,
    };
    use crate::shape::{
        create_height_field_shape, create_hull_shape, create_sphere_shape, should_shapes_collide,
    };
    use crate::table::shape_pair_key;
    use crate::types::{
        default_body_def, default_shape_def, default_weld_joint_def, default_world_def, BodyType,
    };
    use crate::world::{world_rebuild_static_tree, World};

    fn pos(x: f64, y: f64, z: f64) -> Pos {
        Pos {
            x: x as _,
            y: y as _,
            z: z as _,
        }
    }

    /// Brute force check of the broad-phase pair set against every live shape pair. Must run
    /// right after a zero time step so the pair update and the disjoint contact purge have both
    /// happened against the current fat boxes. (CheckPairSet)
    fn check_pair_set(world: &World) -> bool {
        let shape_count = world.shapes.len();

        for i in 0..shape_count {
            let shape_a = &world.shapes[i];
            if shape_a.id == NULL_INDEX
                || shape_a.proxy_key == NULL_INDEX
                || shape_a.sensor_index != NULL_INDEX
            {
                continue;
            }

            let body_a = &world.bodies[shape_a.body_id as usize];

            for j in i + 1..shape_count {
                let shape_b = &world.shapes[j];
                if shape_b.id == NULL_INDEX
                    || shape_b.proxy_key == NULL_INDEX
                    || shape_b.sensor_index != NULL_INDEX
                {
                    continue;
                }

                if shape_a.body_id == shape_b.body_id {
                    continue;
                }

                let body_b = &world.bodies[shape_b.body_id as usize];

                let expected = should_shapes_collide(shape_a.filter, shape_b.filter)
                    && should_bodies_collide(world, shape_a.body_id, shape_b.body_id)
                    && aabb_overlaps(world.fat_aabbs[i], world.fat_aabbs[j]);

                let actual = world
                    .broad_phase
                    .pair_set
                    .contains_key(shape_pair_key(i as i32, j as i32, 0));

                // A sleeping contact is never refreshed, so a stale pair can outlive the overlap
                // when the other body is teleported, and a set woken by the narrow phase carries
                // its stale contacts into the awake set one collide late. With sleeping on only a
                // missing pair is a bug.
                if world.enable_sleep && actual && !expected {
                    continue;
                }

                if expected != actual {
                    println!(
                        "  pair mismatch: shapes {i} (body {} type {:?} set {}) and {j} (body {} type {:?} set {}), expected {expected}, actual {actual}",
                        shape_a.body_id,
                        body_a.type_,
                        body_a.set_index,
                        shape_b.body_id,
                        body_b.type_,
                        body_b.set_index
                    );
                    return false;
                }
            }
        }

        true
    }

    /// (RandomState, xorshift32)
    struct RandomState {
        seed: u32,
    }

    impl RandomState {
        /// (NextRandom)
        fn next(&mut self) -> u32 {
            let mut x = self.seed;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.seed = x;
            x
        }

        /// (RandomFloat)
        fn float(&mut self, lower: f32, upper: f32) -> f32 {
            let u = (self.next() & 0xFFFFFF) as f32 / 0x1000000 as f32;
            lower + (upper - lower) * u
        }

        /// (RandomInt)
        fn int(&mut self, count: i32) -> i32 {
            (self.next() % count as u32) as i32
        }
    }

    const MAX_BODIES: usize = 64;
    const STATIC_COUNT: usize = 60;
    const TILE_COUNT: usize = 9;

    /// (BodySlot)
    #[derive(Clone, Copy, Default)]
    struct BodySlot {
        body_id: BodyId,
        active: bool,
    }

    /// (StressContext)
    struct StressContext {
        random: RandomState,
        origin: Pos,
        extent: f32,
        slots: [BodySlot; MAX_BODIES],
        joints: [JointId; 4],
        joint_count: usize,
        box_hull: BoxHull,
    }

    /// (RandomPosition)
    fn random_position(context: &mut StressContext) -> Pos {
        let e = context.extent;
        let x = context.random.float(-e, e);
        let y = context.random.float(-e, e);
        let z = context.random.float(-e, e);
        offset_pos(context.origin, Vec3::new(x, y, z))
    }

    /// (RandomVelocity)
    fn random_velocity(context: &mut StressContext) -> Vec3 {
        // Some bodies outrun their margin every step and go through continuous collision
        let s = if context.random.int(4) == 0 {
            40.0
        } else {
            12.0
        };
        let x = context.random.float(-s, s);
        let y = context.random.float(-s, s);
        let z = context.random.float(-s, s);
        Vec3::new(x, y, z)
    }

    /// (CreateSlotBody)
    fn create_slot_body(world: &mut World, context: &mut StressContext, slot: usize) {
        let mut body_def = default_body_def();
        body_def.type_ = if context.random.int(10) == 0 {
            BodyType::Kinematic
        } else {
            BodyType::Dynamic
        };
        body_def.position = random_position(context);
        body_def.linear_velocity = random_velocity(context);
        body_def.is_bullet = context.random.int(3) == 0;
        body_def.gravity_scale = 0.0;
        let body_id = create_body(world, &body_def);

        let mut shape_def = default_shape_def();
        shape_def.filter.category_bits = 1u64 << context.random.int(3);
        shape_def.filter.mask_bits = if context.random.int(4) == 0 {
            0x5
        } else {
            DEFAULT_MASK_BITS
        };

        let shape_count = 1 + context.random.int(2);
        for _ in 0..shape_count {
            if context.random.int(2) == 0 {
                let x = context.random.float(-1.0, 1.0);
                let radius = context.random.float(0.2, 1.5);
                let sphere = Sphere {
                    center: Vec3::new(x, 0.0, 0.0),
                    radius,
                };
                create_sphere_shape(world, body_id, &shape_def, &sphere);
            } else {
                create_hull_shape(world, body_id, &shape_def, &context.box_hull.base);
            }
        }

        context.slots[slot].body_id = body_id;
        context.slots[slot].active = true;
    }

    /// A swarm of bodies churning through every proxy mutation the world API offers, with the
    /// pair set checked against a brute force scan after every step. (RunBroadPhaseStress)
    fn run_broad_phase_stress(seed: u32, enable_sleep: bool) -> bool {
        let mut world_def = default_world_def();
        world_def.enable_sleep = enable_sleep;
        world_def.gravity = VEC3_ZERO;
        let mut world = World::new(&world_def);
        let w = &mut world;

        let mut context = StressContext {
            random: RandomState { seed },
            origin: pos(-1900.0, 0.0, 1330.0),
            extent: 12.0,
            slots: [BodySlot::default(); MAX_BODIES],
            joints: [JointId::default(); 4],
            joint_count: 0,
            box_hull: make_box_hull(0.95, 0.6, 2.2),
        };

        // Kilometre tiles far from the origin under the swarm, like a streamed terrain
        let tile = create_grid(65, 65, Vec3::new(16.0, 1.0, 16.0), false);
        for i in 0..TILE_COUNT {
            let mut body_def = default_body_def();
            body_def.type_ = BodyType::Static;
            body_def.position = pos(
                -3072.0 + 1024.0 * (i % 3) as f64,
                -30.0,
                1024.0 * (i / 3) as f64,
            );
            let tile_id = create_body(w, &body_def);
            let mut shape_def = default_shape_def();
            shape_def.enable_speculative_contact = false;
            create_height_field_shape(w, tile_id, &shape_def, &tile);
        }

        // Static clutter inside the swarm. Some pieces ask for contacts on creation.
        let mut static_ids = [BodyId::default(); STATIC_COUNT];
        let grid = create_grid(5, 5, Vec3::new(2.0, 1.0, 2.0), false);
        let static_box = make_box_hull(3.0, 0.5, 3.0);
        for (i, static_id) in static_ids.iter_mut().enumerate() {
            let mut body_def = default_body_def();
            body_def.type_ = BodyType::Static;
            body_def.position = random_position(&mut context);
            *static_id = create_body(w, &body_def);

            let mut shape_def = default_shape_def();
            shape_def.invoke_contact_creation = (i % 4) == 0;
            shape_def.enable_speculative_contact = (i % 2) == 0;

            if i % 3 == 0 {
                create_height_field_shape(w, *static_id, &shape_def, &grid);
            } else {
                create_hull_shape(w, *static_id, &shape_def, &static_box.base);
            }
        }

        for i in 0..MAX_BODIES {
            if context.random.int(4) != 0 {
                create_slot_body(w, &mut context, i);
            }
        }

        let time_step = 1.0 / 60.0;
        let step_count = 400;

        for step in 0..step_count {
            let mutation_count = context.random.int(4);
            for _ in 0..mutation_count {
                let slot = context.random.int(MAX_BODIES as i32) as usize;
                let action = context.random.int(12);

                if !context.slots[slot].active {
                    if action < 6 {
                        create_slot_body(w, &mut context, slot);
                    }
                    continue;
                }

                let body_id = context.slots[slot].body_id;

                match action {
                    0 => {
                        destroy_body(w, body_id);
                        context.slots[slot].active = false;
                    }
                    1 | 2 => {
                        let p = random_position(&mut context);
                        body_set_transform(w, body_id, p, QUAT_IDENTITY);
                    }
                    3 => {
                        let v = random_velocity(&mut context);
                        body_set_linear_velocity(w, body_id, v);
                    }
                    4 => body_disable(w, body_id),
                    5 => body_enable(w, body_id),
                    6 => {
                        let flag = context.random.int(2) == 0;
                        body_set_bullet(w, body_id, flag);
                    }
                    7 => {
                        let types = [BodyType::Static, BodyType::Kinematic, BodyType::Dynamic];
                        let t = types[context.random.int(3) as usize];
                        body_set_type(w, body_id, t);
                        if body_get_type(w, body_id) != BodyType::Static {
                            let v = random_velocity(&mut context);
                            body_set_linear_velocity(w, body_id, v);
                        }
                    }
                    8 => {
                        // Add a joint that blocks collision, or toggle one
                        if context.joint_count < 4 {
                            let other_slot = context.random.int(MAX_BODIES as i32) as usize;
                            let other = context.slots[other_slot];
                            if other_slot != slot
                                && other.active
                                && body_get_type(w, body_id) == BodyType::Dynamic
                                && body_get_type(w, other.body_id) == BodyType::Dynamic
                            {
                                let mut joint_def = default_weld_joint_def();
                                joint_def.base.body_id_a = body_id;
                                joint_def.base.body_id_b = other.body_id;
                                joint_def.base.collide_connected = false;
                                context.joints[context.joint_count] =
                                    create_weld_joint(w, &joint_def);
                                context.joint_count += 1;
                            }
                        } else {
                            let index = context.random.int(context.joint_count as i32) as usize;
                            let joint_id = context.joints[index];
                            if joint_is_valid(w, joint_id) {
                                let flag = context.random.int(2) == 0;
                                joint_set_collide_connected(w, joint_id, flag);
                            }
                        }
                    }
                    9 => {
                        let static_index = context.random.int(STATIC_COUNT as i32) as usize;
                        let p = random_position(&mut context);
                        body_set_transform(w, static_ids[static_index], p, QUAT_IDENTITY);
                    }
                    10 => world_rebuild_static_tree(w),
                    _ => {}
                }
            }

            // Joints whose bodies were destroyed are gone too
            let mut j = 0;
            while j < context.joint_count {
                if !joint_is_valid(w, context.joints[j]) {
                    context.joints[j] = context.joints[context.joint_count - 1];
                    context.joint_count -= 1;
                } else {
                    j += 1;
                }
            }

            // Keep the swarm inside the clutter
            for i in 0..MAX_BODIES {
                let slot = context.slots[i];
                if !slot.active || !body_is_enabled(w, slot.body_id) {
                    continue;
                }

                let p = body_get_position(w, slot.body_id);
                let d = sub_pos(p, context.origin);
                let limit = 2.0 * context.extent;
                if abs_float(d.x) > limit || abs_float(d.y) > limit || abs_float(d.z) > limit {
                    let p = random_position(&mut context);
                    body_set_transform(w, slot.body_id, p, QUAT_IDENTITY);
                    let v = random_velocity(&mut context);
                    body_set_linear_velocity(w, slot.body_id, v);
                }
            }

            w.step(time_step, 4);

            // A zero step finds pairs and purges disjoint contacts without moving anything
            w.step(0.0, 1);

            if !check_pair_set(w) {
                println!("  seed={seed} sleep={enable_sleep} step={step}");
                return false;
            }
        }

        true
    }

    /// (BroadPhaseStressTest)
    ///
    /// C runs each seed at worker counts 1 and 4 without sleep and 3 with sleep. The serial port
    /// has no worker count, so each seed runs once per sleep mode.
    #[test]
    fn broad_phase_stress_test() {
        let seeds = [12345u32, 0xC0FFEE, 777];
        for &seed in &seeds {
            assert!(run_broad_phase_stress(seed, false));
            assert!(run_broad_phase_stress(seed, true));
        }
    }

    /// The broad-phase keeps its pending pair marks inside the static tree, so a full rebuild
    /// between a static mutation and the next step used to wipe them and the contact never
    /// appeared. Covers both ways a static proxy gets marked: a teleport and a shape created with
    /// invoke_contact_creation. (RunStaticRebuildKeepsMarks)
    fn run_static_rebuild_keeps_marks(use_invoke: bool) {
        let mut world_def = default_world_def();
        world_def.gravity = VEC3_ZERO;
        let mut world = World::new(&world_def);
        let w = &mut world;

        let mut body_def = default_body_def();
        body_def.type_ = BodyType::Dynamic;
        body_def.position = pos(0.0, 1.0, 0.0);
        let sphere_id = create_body(w, &body_def);
        let shape_def = default_shape_def();
        let sphere = Sphere {
            center: VEC3_ZERO,
            radius: 0.5,
        };
        create_sphere_shape(w, sphere_id, &shape_def, &sphere);

        body_def.type_ = BodyType::Static;
        body_def.position = pos(100.0, 0.0, 0.0);
        let ground_id = create_body(w, &body_def);
        let box_hull = make_box_hull(2.0, 0.5, 2.0);
        if !use_invoke {
            create_hull_shape(w, ground_id, &shape_def, &box_hull.base);
        }

        // Settle so the resting sphere carries no mark of its own
        for _ in 0..3 {
            w.step(1.0 / 60.0, 4);
        }

        body_set_transform(w, ground_id, POS_ZERO, QUAT_IDENTITY);
        if use_invoke {
            let mut invoke_def = default_shape_def();
            invoke_def.invoke_contact_creation = true;
            create_hull_shape(w, ground_id, &invoke_def, &box_hull.base);
        }

        world_rebuild_static_tree(w);
        w.step(1.0 / 60.0, 4);

        assert_eq!(body_get_contact_capacity(w, sphere_id), 1);
    }

    /// (StaticRebuildKeepsMarks)
    #[test]
    fn static_rebuild_keeps_marks() {
        run_static_rebuild_keeps_marks(false);
        run_static_rebuild_keeps_marks(true);
    }
}
