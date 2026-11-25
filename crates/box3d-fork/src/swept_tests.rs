// Primordial Engine fork (D3): swept bodies (`solver/swept.rs`).
//
// SPDX-License-Identifier: MIT

use crate::body::{
    body_get_linear_velocity, body_get_mass as body_mass, body_get_position, body_is_awake, body_set_angular_velocity,
    body_set_linear_velocity, create_body,
};
use crate::math_functions::length_squared;
use crate::geometry::{Capsule, Sphere};
use crate::hull::make_box_hull;
use crate::id::BodyId;
use crate::math_functions::{Pos, Vec3, VEC3_ZERO};
use crate::recording::hash_world_state;
use crate::recording::snapshot::clone_world_via_snapshot;
use crate::shape::{create_capsule_shape, create_hull_shape, create_sphere_shape};
use crate::types::{default_body_def, default_shape_def, default_world_def, BodyType, WorldDef};
use crate::world::{world_set_worker_count, World};

const DT: f32 = 1.0 / 60.0;
const SUB_STEPS: i32 = 4;

fn v3(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

fn pos(x: f32, y: f32, z: f32) -> Pos {
    Pos { x: x as _, y: y as _, z: z as _ }
}

fn x_of(world: &World, id: BodyId) -> f32 {
    body_get_position(world, id).x as f32
}

fn no_gravity() -> WorldDef {
    let mut def = default_world_def();
    def.gravity = VEC3_ZERO;
    def
}

/// A static box wall whose near face is at `x`, `thickness` thick, 2 m square.
fn wall(world: &mut World, x: f32, thickness: f32) {
    let mut def = default_body_def();
    def.position = pos(x + 0.5 * thickness, 0.0, 0.0);
    let id = create_body(world, &def);
    create_hull_shape(world, id, &default_shape_def(), &make_box_hull(0.5 * thickness, 1.0, 1.0).base);
}

/// A dynamic ball at `at` moving at `velocity`.
fn ball(world: &mut World, at: Pos, radius: f32, velocity: Vec3, swept: bool) -> BodyId {
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.position = at;
    def.linear_velocity = velocity;
    def.swept = swept;
    let id = create_body(world, &def);
    create_sphere_shape(world, id, &default_shape_def(), &Sphere { center: VEC3_ZERO, radius });
    id
}

/// A ball 1 m before a 1 cm wall, aimed at it `aim` radians off its normal at `speed`: the largest x its centre
/// reaches in a second.
fn farthest(speed: f32, radius: f32, aim: f32, swept: bool) -> f32 {
    let mut world = World::new(&no_gravity());
    wall(&mut world, 1.0, 0.01);
    let id = ball(&mut world, pos(0.0, 0.0, 0.0), radius, v3(speed * aim.cos(), speed * aim.sin(), 0.0), swept);
    let mut most = f32::NEG_INFINITY;
    for _ in 0..60 {
        world.step(DT, SUB_STEPS);
        most = most.max(x_of(&world, id));
    }
    most
}

#[test]
fn a_swept_ball_never_crosses_a_thin_static_wall() {
    let mut crossed_unswept = 0;
    for &speed in &[10.0f32, 50.0, 100.0, 200.0, 400.0] {
        for &radius in &[0.005f32, 0.02, 0.1] {
            for k in 0..7 {
                let aim = 0.2 * k as f32 - 0.6;
                let most = farthest(speed, radius, aim, true);
                // Never past the wall's middle (it is 1 cm thick, from x = 1).
                assert!(most < 1.005, "swept: {speed} m/s, r {radius}, aim {aim}: centre reached {most}");
                if farthest(speed, radius, aim, false) > 1.01 {
                    crossed_unswept += 1;
                }
            }
        }
    }
    println!("MEASURED unswept balls through the 1 cm wall: {crossed_unswept} of 105");
}

#[test]
fn a_swept_ball_bounces_off_a_wall_with_its_restitution() {
    for &(restitution, expect) in &[(1.0f32, -100.0f32), (0.0, 0.0)] {
        let mut world = World::new(&no_gravity());
        wall(&mut world, 1.0, 0.01);
        let mut def = default_body_def();
        def.type_ = BodyType::Dynamic;
        def.swept = true;
        def.linear_velocity = v3(100.0, 0.0, 0.0);
        let id = create_body(&mut world, &def);
        let mut shape = default_shape_def();
        shape.base_material.restitution = restitution;
        shape.base_material.friction = 0.0;
        create_sphere_shape(&mut world, id, &shape, &Sphere { center: VEC3_ZERO, radius: 0.02 });
        for _ in 0..30 {
            world.step(DT, SUB_STEPS);
        }
        let v = body_get_linear_velocity(&world, id).x;
        assert!((v - expect).abs() < 1.0, "restitution {restitution}: {v}");
        assert!(x_of(&world, id) < 1.0);
    }
}

#[test]
fn two_balls_head_on_keep_their_momentum_and_never_pass() {
    let mut world = World::new(&no_gravity());
    let a = ball(&mut world, pos(0.0, 0.0, 0.0), 0.02, v3(300.0, 0.0, 0.0), true);
    let b = ball(&mut world, pos(2.0, 0.0, 0.0), 0.02, v3(-300.0, 0.0, 0.0), false);
    for _ in 0..30 {
        world.step(DT, SUB_STEPS);
        assert!(x_of(&world, a) < x_of(&world, b), "passed through each other");
    }
    let (va, vb) = (body_get_linear_velocity(&world, a), body_get_linear_velocity(&world, b));
    assert!((va.x + vb.x).abs() < 0.01 * 300.0, "momentum {} + {}", va.x, vb.x);
}

#[test]
fn two_swept_balls_head_on_never_pass() {
    let mut world = World::new(&no_gravity());
    let a = ball(&mut world, pos(0.0, 0.0, 0.0), 0.005, v3(400.0, 0.0, 0.0), true);
    let b = ball(&mut world, pos(3.0, 0.0, 0.0), 0.005, v3(-400.0, 0.0, 0.0), true);
    for _ in 0..30 {
        world.step(DT, SUB_STEPS);
        assert!(x_of(&world, a) < x_of(&world, b), "passed through each other");
    }
}

/// Off centre the two may slide past each other, but only after the hit has turned them aside.
#[test]
fn two_swept_balls_off_centre_turn_each_other_aside() {
    let mut world = World::new(&no_gravity());
    let a = ball(&mut world, pos(0.0, 0.0, 0.0), 0.005, v3(400.0, 0.0, 0.0), true);
    let b = ball(&mut world, pos(3.0, 0.004, 0.0), 0.005, v3(-400.0, 0.0, 0.0), true);
    world.step(DT, SUB_STEPS);
    let (va, vb) = (body_get_linear_velocity(&world, a), body_get_linear_velocity(&world, b));
    assert!(va.y < -1.0 && vb.y > 1.0, "not turned aside: {va:?} {vb:?}");
    assert!((va.x + vb.x).abs() < 1.0 && (va.y + vb.y).abs() < 1.0, "momentum {va:?} {vb:?}");
}

#[test]
fn a_swept_ball_wakes_a_sleeping_box_and_moves_it() {
    let mut world = World::new(&default_world_def());
    // Ground.
    let mut def = default_body_def();
    def.position = pos(0.0, -0.5, 0.0);
    let ground = create_body(&mut world, &def);
    create_hull_shape(&mut world, ground, &default_shape_def(), &make_box_hull(20.0, 0.5, 20.0).base);
    // A box at rest, left to fall asleep.
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.position = pos(2.0, 0.25, 0.0);
    let block = create_body(&mut world, &def);
    create_hull_shape(&mut world, block, &default_shape_def(), &make_box_hull(0.25, 0.25, 0.25).base);
    for _ in 0..240 {
        world.step(DT, SUB_STEPS);
    }
    assert!(!body_is_awake(&world, block), "the box should be asleep");
    // A 5 cm ball of density 100 (52 g) into the 125 g box.
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.swept = true;
    def.position = pos(0.0, 0.25, 0.0);
    def.linear_velocity = v3(300.0, 0.0, 0.0);
    let bullet = create_body(&mut world, &def);
    let mut shape = default_shape_def();
    shape.density = 100.0;
    create_sphere_shape(&mut world, bullet, &shape, &Sphere { center: VEC3_ZERO, radius: 0.05 });
    let (mut moved, mut queries) = (false, 0);
    for _ in 0..20 {
        world.step(DT, SUB_STEPS);
        assert!(x_of(&world, bullet) < x_of(&world, block), "the ball passed into the box");
        moved |= body_get_linear_velocity(&world, block).x > 0.1;
        queries += world.swept_counters.iterations;
    }
    assert!(moved, "the hit never moved the box");
    // The sweep ran (a slow pair after the hit costs nothing, so the last step alone may spend no query).
    assert!(queries > 0);
}

#[test]
fn with_its_budget_spent_a_swept_ball_is_held_never_let_through() {
    let mut world = World::new(&no_gravity());
    wall(&mut world, 1.0, 0.01);
    let id = ball(&mut world, pos(0.0, 0.0, 0.0), 0.005, v3(400.0, 0.0, 0.0), true);
    world.swept_iteration_budget = 1;
    let mut holds = 0;
    for _ in 0..30 {
        world.step(DT, SUB_STEPS);
        holds += world.swept_counters.holds;
        assert!(x_of(&world, id) < 1.005);
    }
    assert!(holds > 0, "a budget of one query should run out");
}

/// Swept balls of each shape thrown into a loose pile, enough bodies for the parallel solve.
fn pile(world: &mut World) -> Vec<BodyId> {
    let mut def = default_body_def();
    def.position = pos(0.0, -0.5, 0.0);
    let ground = create_body(world, &def);
    create_hull_shape(world, ground, &default_shape_def(), &make_box_hull(20.0, 0.5, 20.0).base);
    let mut ids = Vec::new();
    for i in 0..80 {
        let mut def = default_body_def();
        def.type_ = BodyType::Dynamic;
        def.position = pos((i % 8) as f32 * 0.3, 0.2 + (i / 8) as f32 * 0.3, 0.0);
        let id = create_body(world, &def);
        create_hull_shape(world, id, &default_shape_def(), &make_box_hull(0.1, 0.1, 0.1).base);
        ids.push(id);
    }
    for k in 0..3 {
        let mut def = default_body_def();
        def.type_ = BodyType::Dynamic;
        def.swept = true;
        def.position = pos(-3.0, 0.5 + k as f32 * 0.6, 0.05 * k as f32);
        def.linear_velocity = v3(150.0 + 50.0 * k as f32, 0.0, 0.0);
        def.angular_velocity = v3(0.0, 0.0, 30.0);
        let id = create_body(world, &def);
        let shape = default_shape_def();
        match k {
            0 => create_sphere_shape(world, id, &shape, &Sphere { center: VEC3_ZERO, radius: 0.05 }),
            1 => create_capsule_shape(
                world,
                id,
                &shape,
                &Capsule { center1: v3(-0.05, 0.0, 0.0), center2: v3(0.05, 0.0, 0.0), radius: 0.03 },
            ),
            _ => create_hull_shape(world, id, &shape, &make_box_hull(0.04, 0.04, 0.04).base),
        };
        ids.push(id);
    }
    ids
}

#[test]
fn swept_hits_give_the_same_state_at_every_worker_count() {
    let mut hashes: Vec<Vec<u64>> = Vec::new();
    for &workers in &[1, 2, 4, 8] {
        let mut def = default_world_def();
        def.worker_count = workers;
        let mut world = World::new(&def);
        pile(&mut world);
        let mut run = Vec::new();
        for _ in 0..90 {
            world.step(DT, SUB_STEPS);
            run.push(hash_world_state(&world));
        }
        hashes.push(run);
    }
    for run in &hashes[1..] {
        assert_eq!(run, &hashes[0]);
    }
}

#[test]
fn a_restored_snapshot_replays_the_same_swept_hits() {
    let mut world = World::new(&default_world_def());
    pile(&mut world);
    for _ in 0..10 {
        world.step(DT, SUB_STEPS);
    }
    let mut copy = clone_world_via_snapshot(&world).expect("a snapshot");
    assert_eq!(copy.swept_bodies, world.swept_bodies, "the swept list is rebuilt from the flags");
    world_set_worker_count(&mut copy, 1);
    for _ in 0..60 {
        world.step(DT, SUB_STEPS);
        copy.step(DT, SUB_STEPS);
        assert_eq!(hash_world_state(&copy), hash_world_state(&world));
    }
}

#[test]
fn a_world_with_no_swept_body_runs_no_sweep() {
    let mut world = World::new(&default_world_def());
    let id = ball(&mut world, pos(0.0, 1.0, 0.0), 0.05, VEC3_ZERO, false);
    body_set_linear_velocity(&mut world, id, v3(1.0, 0.0, 0.0));
    for _ in 0..10 {
        world.step(DT, SUB_STEPS);
        assert_eq!(world.swept_counters, Default::default());
    }
}

/// A swept body that never comes near the others leaves them exactly as a world without it steps them, bit for bit:
/// the sweep touches only its candidates (the method's "bit-identical without bullets"; measured first on M1's
/// 850-ball pit, scratch/ccd/far1.log).
#[test]
fn a_far_swept_body_leaves_the_others_bit_identical() {
    let run = |with: bool| {
        let mut world = World::new(&default_world_def());
        let mut def = default_body_def();
        def.position = pos(0.0, -0.5, 0.0);
        let ground = create_body(&mut world, &def);
        create_hull_shape(&mut world, ground, &default_shape_def(), &make_box_hull(30.0, 0.5, 30.0).base);
        // A pile of twelve balls dropped onto the ground: contacts, jostling, islands.
        let pile: Vec<BodyId> = (0..12)
            .map(|k| {
                let at = pos(0.15 * (k % 4) as f32, 0.1 + 0.25 * (k / 4) as f32, 0.03 * (k % 3) as f32);
                ball(&mut world, at, 0.1, VEC3_ZERO, false)
            })
            .collect();
        if with {
            // Far off, fired further away.
            ball(&mut world, pos(20.0, 1.0, 0.0), 0.01, v3(30.0, 5.0, 0.0), true);
        }
        for _ in 0..120 {
            world.step(DT, SUB_STEPS);
        }
        pile.iter()
            .map(|&b| {
                let p = body_get_position(&world, b);
                let v = body_get_linear_velocity(&world, b);
                [p.x.to_bits(), p.y.to_bits(), p.z.to_bits(), v.x.to_bits() as u64, v.y.to_bits() as u64, v.z.to_bits() as u64]
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(run(true), run(false));
}

/// Three bullets fired into a pile of balls in a box stay well inside the budget, with no hold. The closing bound
/// along the contact normal is what keeps rolling and jostling contacts cheap: without it a hundred bullets in M1's
/// pit spent 262144 queries a step (scratch/ccd/cost-red-ccd-directional.log). Returns the most queries a step and
/// the holds.
fn bullets_into_a_pile() -> (i32, i32) {
    let mut world = World::new(&default_world_def());
    let mut def = default_body_def();
    def.position = pos(0.0, -0.5, 0.0);
    let floor = create_body(&mut world, &def);
    create_hull_shape(&mut world, floor, &default_shape_def(), &make_box_hull(2.0, 0.5, 2.0).base);
    for (x, z, hx, hz) in [(0.55, 0.0, 0.05, 0.6), (-0.55, 0.0, 0.05, 0.6), (0.0, 0.55, 0.6, 0.05), (0.0, -0.55, 0.6, 0.05)] {
        let mut def = default_body_def();
        def.position = pos(x, 0.3, z);
        let wall = create_body(&mut world, &def);
        create_hull_shape(&mut world, wall, &default_shape_def(), &make_box_hull(hx, 0.3, hz).base);
    }
    for k in 0..48 {
        let at = pos(0.13 * (k % 6) as f32 - 0.33, 0.07 + 0.13 * (k / 36) as f32, 0.13 * ((k / 6) % 6) as f32 - 0.33);
        ball(&mut world, at, 0.06, VEC3_ZERO, false);
    }
    for _ in 0..60 {
        world.step(DT, SUB_STEPS);
    }
    for (k, speed) in [(0, 60.0f32), (1, 120.0), (2, 30.0)] {
        let at = pos(-0.9, 0.5 + 0.1 * k as f32, 0.1 * k as f32 - 0.1);
        ball(&mut world, at, 0.01, v3(speed, -0.3 * speed, 0.0), true);
    }
    let (mut most, mut holds) = (0, 0);
    for _ in 0..120 {
        world.step(DT, SUB_STEPS);
        most = most.max(world.swept_counters.iterations);
        holds += world.swept_counters.holds;
    }
    (most, holds)
}

#[test]
fn bullets_into_a_pile_cost_few_queries_and_no_hold() {
    // Measured: at most 38 queries a step; 96 without the closing bound along the normal (scratch/ccd/pile-*.log).
    let (most, holds) = bullets_into_a_pile();
    assert!(most <= 64 && holds == 0, "{most} queries in a step, {holds} holds");
}

/// A bullet that starts inside something (spawned or teleported into it, or a crate edited in over it) isn't held
/// there: the contact solver pushes it out, a static box or a sleeping dynamic one (@design-reviewer's M1 on
/// d78fdfc0). Two starts: 3 cm inside the face (the cores overlap), and 20 cm inside (the cores' point clouds overlap,
/// M1's case). Returns, for each case, where the ball's centre ends and the holds over the last 30 frames.
fn bullets_started_inside_a_box() -> Vec<(f32, i32)> {
    [(false, 0.22f32), (true, 0.22), (false, 0.05), (true, 0.05)]
        .into_iter()
        .map(|(dynamic, at)| {
            let mut world = World::new(&no_gravity());
            let mut def = default_body_def();
            if dynamic {
                def.type_ = BodyType::Dynamic;
                def.is_awake = false;
            }
            let block = create_body(&mut world, &def);
            create_hull_shape(&mut world, block, &default_shape_def(), &make_box_hull(0.25, 0.25, 0.25).base);
            let id = ball(&mut world, pos(at, 0.0, 0.0), 0.05, VEC3_ZERO, true);
            let mut late = 0;
            for f in 0..60 {
                world.step(DT, SUB_STEPS);
                if f >= 30 {
                    late += world.swept_counters.holds;
                }
            }
            (x_of(&world, id) - x_of(&world, block), late)
        })
        .collect()
}

#[test]
fn a_bullet_started_inside_a_box_is_pushed_out_not_held() {
    for (k, (x, late)) in bullets_started_inside_a_box().into_iter().enumerate() {
        // Out past the face (0.25) by its radius, less the contact solver's slop.
        assert!(x > 0.25 + 0.05 - 0.01 && late == 0, "case {k}: centre {x} m from the box's, {late} late holds");
    }
}

/// A box bullet sliding and tumbling across a dynamic box floor (not a static plane) for a second, with no hold: the
/// rounded cores give a box-on-box graze room before the floor branch (@design-reviewer's Q1 on d78fdfc0). Returns
/// the holds and the Separates (counted as holds too) over the second.
fn a_box_bullet_across_a_box_floor() -> i32 {
    let mut world = World::new(&default_world_def());
    let mut def = default_body_def();
    def.position = pos(0.0, -0.5, 0.0);
    let ground = create_body(&mut world, &def);
    create_hull_shape(&mut world, ground, &default_shape_def(), &make_box_hull(20.0, 0.5, 20.0).base);
    // A heavy dynamic plank as the floor the bullet crosses.
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.position = pos(1.5, 0.05, 0.0);
    let plank = create_body(&mut world, &def);
    let mut shape = default_shape_def();
    shape.density = 2000.0;
    create_hull_shape(&mut world, plank, &shape, &make_box_hull(2.0, 0.05, 1.0).base);
    for _ in 0..30 {
        world.step(DT, SUB_STEPS);
    }
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.swept = true;
    def.allow_fast_rotation = true;
    def.position = pos(0.0, 0.2, 0.0);
    def.linear_velocity = v3(12.0, -2.0, 0.0);
    def.angular_velocity = v3(3.0, 0.0, -30.0);
    let id = create_body(&mut world, &def);
    create_hull_shape(&mut world, id, &default_shape_def(), &make_box_hull(0.04, 0.01, 0.02).base);
    let mut holds = 0;
    for _ in 0..60 {
        world.step(DT, SUB_STEPS);
        holds += world.swept_counters.holds;
        assert!(body_get_position(&world, id).y as f32 > 0.0, "the bullet sank through the plank");
    }
    holds
}

#[test]
fn a_box_bullet_tumbling_across_a_box_floor_is_never_held() {
    let holds = a_box_bullet_across_a_box_floor();
    assert!(holds == 0, "{holds} holds");
}

// ---- The ruling's cases (@coordinator-opus) ----

/// A dynamic box asleep in a world without gravity, its near face at `x`.
fn sleeping_box(world: &mut World, x: f32, half: f32, density: f32) -> BodyId {
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.position = pos(x + half, 0.0, 0.0);
    def.is_awake = false;
    let id = create_body(world, &def);
    let mut shape = default_shape_def();
    shape.density = density;
    shape.base_material.restitution = 1.0;
    shape.base_material.friction = 0.0;
    create_hull_shape(world, id, &shape, &make_box_hull(half, half, half).base);
    id
}

/// An elastic, frictionless swept ball.
fn elastic_ball(world: &mut World, at: Pos, radius: f32, density: f32, velocity: Vec3) -> BodyId {
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.position = at;
    def.linear_velocity = velocity;
    def.swept = true;
    let id = create_body(world, &def);
    let mut shape = default_shape_def();
    shape.density = density;
    shape.base_material.restitution = 1.0;
    shape.base_material.friction = 0.0;
    create_sphere_shape(world, id, &shape, &Sphere { center: VEC3_ZERO, radius });
    id
}

/// The box's velocity, the balls' momentum and the energy after one step of `balls` elastic hits on a sleeping box.
fn hits_on_a_sleeping_box(balls: usize) -> (f32, f32, f32, f32, f32) {
    let mut world = World::new(&no_gravity());
    let block = sleeping_box(&mut world, 1.0, 0.25, 1.0);
    let v = 60.0f32;
    let ids: Vec<BodyId> = (0..balls)
        .map(|k| elastic_ball(&mut world, pos(0.0, 0.1 - 0.2 * k as f32, 0.0), 0.05, 100.0, v3(v, 0.0, 0.0)))
        .collect();
    assert!(!body_is_awake(&world, block));
    let m = body_mass(&world, ids[0]);
    let m_box = body_mass(&world, block);
    let energy = |w: &World| {
        let mut e = 0.5 * m_box * length_squared(body_get_linear_velocity(w, block));
        for &id in &ids {
            e += 0.5 * m * length_squared(body_get_linear_velocity(w, id));
        }
        e
    };
    let before = energy(&world);
    world.step(DT, SUB_STEPS);
    assert!(body_is_awake(&world, block), "the hits woke the box");
    assert!(world.swept_counters.deferred >= balls as i32);
    let p: f32 = ids.iter().map(|&id| m * body_get_linear_velocity(&world, id).x).sum::<f32>()
        + m_box * body_get_linear_velocity(&world, block).x;
    (body_get_linear_velocity(&world, block).x, p, balls as f32 * m * v, before, energy(&world))
}

/// Two bullets hit one sleeping box in the same step: the second sees the box's pending velocity from the first, so
/// the box doesn't leave at about twice one hit's speed; momentum is kept and no energy is made (spin included).
#[test]
fn two_bullets_into_one_sleeping_box_see_its_pending_velocity() {
    let (one, ..) = hits_on_a_sleeping_box(1);
    let (two, p, p0, e0, e1) = hits_on_a_sleeping_box(2);
    assert!(one > 1.0 && two > one, "one hit {one}, two {two}");
    assert!(two < 1.9 * one, "a ~2v launch: two hits {two} vs one {one}");
    assert!((p - p0).abs() < 0.01 * p0, "momentum {p} vs {p0}");
    assert!(e1 <= 1.01 * e0, "energy made: {e0} -> {e1}");
}

/// A bullet a hundred times the sleeper's mass: it can't stop, so it waits at the sleeper for the rest of the step,
/// then the woken sleeper carries the impulse away; nothing passes through and momentum is kept.
#[test]
fn a_heavy_bullet_into_a_light_sleeper_keeps_momentum_and_never_passes() {
    let mut world = World::new(&no_gravity());
    let light = sleeping_box(&mut world, 1.0, 0.05, 1.0);
    let m_light = body_mass(&world, light);
    let radius = 0.05f32;
    let density = 100.0 * m_light / (4.0 / 3.0 * core::f32::consts::PI * radius * radius * radius);
    let heavy = elastic_ball(&mut world, pos(0.0, 0.0, 0.0), radius, density, v3(100.0, 0.0, 0.0));
    let m_heavy = body_mass(&world, heavy);
    assert!((m_heavy / m_light - 100.0).abs() < 1.0);
    let mut holds = 0;
    for _ in 0..20 {
        world.step(DT, SUB_STEPS);
        holds += world.swept_counters.holds;
        assert!(x_of(&world, heavy) < x_of(&world, light), "the heavy ball passed the light box");
    }
    assert!(holds > 0, "the heavy ball should have waited at the sleeper");
    let p = m_heavy * body_get_linear_velocity(&world, heavy).x + m_light * body_get_linear_velocity(&world, light).x;
    assert!((p - m_heavy * 100.0).abs() < 0.01 * m_heavy * 100.0, "momentum {p}");
}

/// A sleeping stack hit off centre, from a snapshot taken before the hit: the restored copy replays it exactly.
#[test]
fn an_off_centre_hit_on_a_sleeping_stack_replays_the_same_after_a_rollback() {
    let mut world = World::new(&default_world_def());
    let mut def = default_body_def();
    def.position = pos(0.0, -0.5, 0.0);
    let ground = create_body(&mut world, &def);
    create_hull_shape(&mut world, ground, &default_shape_def(), &make_box_hull(20.0, 0.5, 20.0).base);
    let mut stack = Vec::new();
    for k in 0..4 {
        let mut def = default_body_def();
        def.type_ = BodyType::Dynamic;
        def.position = pos(2.0, 0.25 + 0.5 * k as f32, 0.0);
        let id = create_body(&mut world, &def);
        create_hull_shape(&mut world, id, &default_shape_def(), &make_box_hull(0.25, 0.25, 0.25).base);
        stack.push(id);
    }
    for _ in 0..300 {
        world.step(DT, SUB_STEPS);
    }
    assert!(stack.iter().all(|&id| !body_is_awake(&world, id)), "the stack should be asleep");
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.swept = true;
    def.position = pos(0.0, 1.6, 0.2);
    def.linear_velocity = v3(200.0, 0.0, 0.0);
    let bullet = create_body(&mut world, &def);
    // The stack's boxes have the default density, 1000 kg/m^3 (125 kg each): a heavy bullet (1.34 kg) moves one.
    let mut shape = default_shape_def();
    shape.density = 5000.0;
    create_sphere_shape(&mut world, bullet, &shape, &Sphere { center: VEC3_ZERO, radius: 0.04 });
    let mut copy = clone_world_via_snapshot(&world).expect("a snapshot");
    for step in 0..90 {
        world.step(DT, SUB_STEPS);
        copy.step(DT, SUB_STEPS);
        if step == 0 {
            assert!(body_is_awake(&world, stack[3]), "the hit woke the top box");
            assert!(body_get_linear_velocity(&world, stack[3]).x > 0.5, "and pushed it");
        }
        assert_eq!(hash_world_state(&copy), hash_world_state(&world));
    }
}

/// An isolated off-centre pair, a ball into a spinning free box: the linear momentum is kept on every axis.
#[test]
fn an_isolated_off_centre_pair_keeps_its_momentum() {
    let mut world = World::new(&no_gravity());
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.position = pos(2.0, 0.1, -0.07);
    def.angular_velocity = v3(1.0, 2.0, 3.0);
    let block = create_body(&mut world, &def);
    create_hull_shape(&mut world, block, &default_shape_def(), &make_box_hull(0.2, 0.3, 0.25).base);
    let ball_id = ball(&mut world, pos(0.0, 0.0, 0.0), 0.03, v3(250.0, 3.0, -2.0), true);
    let momentum = |w: &World| {
        let (mb, mk) = (body_mass(w, ball_id), body_mass(w, block));
        let (vb, vk) = (body_get_linear_velocity(w, ball_id), body_get_linear_velocity(w, block));
        v3(mb * vb.x + mk * vk.x, mb * vb.y + mk * vk.y, mb * vb.z + mk * vk.z)
    };
    let before = momentum(&world);
    for _ in 0..10 {
        world.step(DT, SUB_STEPS);
    }
    let after = momentum(&world);
    let scale = before.x.abs();
    for (b, a) in [(before.x, after.x), (before.y, after.y), (before.z, after.z)] {
        assert!((a - b).abs() < 0.01 * scale, "momentum {before:?} -> {after:?}");
    }
    assert!(body_get_linear_velocity(&world, block).x > 0.1, "the box was hit");
}

/// Capsule and box bullets, tumbling end over end, never cross a thin static wall. Up to 150 m/s they aren't left stuck
/// against it either: from the tenth frame on no sweep holds them (traced 2026-11-25: a rod and a box deep against the
/// wall were held every sub-step while the contact solver tried to push them out). Known limit, not asserted: at 400
/// m/s and 200 rad/s the box takes 35 hits in a frame, ends spinning at about 1900 rad/s with its centre still
/// closing, and is held against the wall from then on.
#[test]
fn tumbling_capsule_and_box_bullets_never_cross_a_thin_wall() {
    for shape_kind in 0..2 {
        for &speed in &[50.0f32, 150.0, 400.0] {
            for &spin in &[0.0f32, 40.0, 200.0] {
                let mut world = World::new(&no_gravity());
                wall(&mut world, 1.0, 0.01);
                let mut def = default_body_def();
                def.type_ = BodyType::Dynamic;
                def.swept = true;
                def.allow_fast_rotation = true;
                def.linear_velocity = v3(speed, 0.0, 0.0);
                def.angular_velocity = v3(0.0, 0.0, spin);
                let id = create_body(&mut world, &def);
                let shape = default_shape_def();
                if shape_kind == 0 {
                    let c = Capsule { center1: v3(-0.06, 0.0, 0.0), center2: v3(0.06, 0.0, 0.0), radius: 0.01 };
                    create_capsule_shape(&mut world, id, &shape, &c);
                } else {
                    create_hull_shape(&mut world, id, &shape, &make_box_hull(0.06, 0.01, 0.03).base);
                }
                for f in 0..30 {
                    world.step(DT, SUB_STEPS);
                    let x = x_of(&world, id);
                    assert!(x < 1.005, "shape {shape_kind}, {speed} m/s, spin {spin}: centre at {x}");
                    let holds = world.swept_counters.holds;
                    let what = format!("shape {shape_kind}, {speed} m/s, spin {spin}");
                    assert!(f < 10 || speed > 150.0 || holds == 0, "{what}: frame {f}, {holds} holds");
                }
            }
        }
    }
}

/// A scene of bullets resting and sliding on a plane and on a stack, run with and without the flag.
fn resting_scene(swept: bool) -> (World, Vec<BodyId>) {
    let mut world = World::new(&default_world_def());
    let mut def = default_body_def();
    def.position = pos(0.0, -0.5, 0.0);
    let ground = create_body(&mut world, &def);
    create_hull_shape(&mut world, ground, &default_shape_def(), &make_box_hull(20.0, 0.5, 20.0).base);
    for k in 0..2 {
        let mut def = default_body_def();
        def.type_ = BodyType::Dynamic;
        def.position = pos(-2.0, 0.25 + 0.5 * k as f32, 0.0);
        let id = create_body(&mut world, &def);
        create_hull_shape(&mut world, id, &default_shape_def(), &make_box_hull(0.25, 0.25, 0.25).base);
    }
    let mut ids = Vec::new();
    let shapes: [(f32, f32, f32, Vec3); 4] = [
        (0.0, 0.1, 0.0, VEC3_ZERO),               // a box at rest on the plane
        (2.0, 0.05, 0.0, v3(5.0, 0.0, 0.0)),      // a capsule sliding on the plane
        (-2.0, 1.1, 0.0, VEC3_ZERO),              // a box resting on the stack
        (4.0, 0.1, 1.0, v3(0.0, 0.0, -3.0)),      // a box sliding on the plane
    ];
    for (i, &(x, y, z, v)) in shapes.iter().enumerate() {
        let mut def = default_body_def();
        def.type_ = BodyType::Dynamic;
        def.swept = swept;
        def.position = pos(x, y, z);
        def.linear_velocity = v;
        let id = create_body(&mut world, &def);
        if i == 1 {
            let c = Capsule { center1: v3(-0.1, 0.0, 0.0), center2: v3(0.1, 0.0, 0.0), radius: 0.05 };
            create_capsule_shape(&mut world, id, &default_shape_def(), &c);
        } else {
            create_hull_shape(&mut world, id, &default_shape_def(), &make_box_hull(0.1, 0.1, 0.1).base);
        }
        ids.push(id);
    }
    (world, ids)
}

/// Box and capsule bullets resting and sliding on the plane and on a stack: no sweep event and no hold, they fall
/// asleep, and they move exactly as unflagged bodies do.
#[test]
fn resting_and_sliding_bullets_raise_no_sweep_event_and_fall_asleep() {
    let (mut swept, ids) = resting_scene(true);
    let (mut plain, _) = resting_scene(false);
    for _ in 0..240 {
        swept.step(DT, SUB_STEPS);
        plain.step(DT, SUB_STEPS);
        let c = swept.swept_counters;
        assert_eq!((c.hits, c.holds, c.embedded, c.exhausted), (0, 0, 0, 0), "{c:?}");
        assert_eq!(hash_world_state(&swept), hash_world_state(&plain), "the flag changed a quiet scene");
    }
    assert!(ids.iter().all(|&id| !body_is_awake(&swept, id)), "the bullets should be asleep");
}

/// A swept ball crossing a fast-turning kinematic paddle at its pivot (1 m in a sub-step while the paddle turns
/// 11.25 deg, @astra-deliberator) is turned back, never through.
#[test]
fn a_swept_ball_crossing_a_turning_kinematic_paddle_at_its_pivot_is_turned_back() {
    let mut world = World::new(&no_gravity());
    let mut def = default_body_def();
    def.type_ = BodyType::Kinematic;
    def.position = pos(2.0, 0.0, 0.0);
    def.allow_fast_rotation = true;
    // 11.25 deg per 1/240 s.
    def.angular_velocity = v3(0.0, 0.196_349_54 * 240.0, 0.0);
    let paddle = create_body(&mut world, &def);
    create_hull_shape(&mut world, paddle, &default_shape_def(), &make_box_hull(0.005, 0.3, 0.3).base);
    let id = elastic_ball(&mut world, pos(0.0, 0.0, 0.0), 0.02, 1.0, v3(240.0, 0.0, 0.0));
    for _ in 0..3 {
        world.step(DT, SUB_STEPS);
    }
    assert!(body_get_linear_velocity(&world, id).x < 0.0, "not turned back: {:?}", body_get_linear_velocity(&world, id));
    assert!(x_of(&world, id) < 2.0);
}

/// A swept impact is reported among the step's contact hit events, like a contact's, when a shape asks for them: its
/// approach speed and normal, with a null contact id. Without the flag, none.
#[test]
fn a_swept_impact_is_a_hit_event_when_its_shapes_ask() {
    for flag in [true, false] {
        let mut world = World::new(&no_gravity());
        let mut def = default_body_def();
        def.position = pos(1.005, 0.0, 0.0);
        let wall_id = create_body(&mut world, &def);
        let mut shape = default_shape_def();
        shape.enable_hit_events = flag;
        create_hull_shape(&mut world, wall_id, &shape, &make_box_hull(0.005, 1.0, 1.0).base);
        let id = ball(&mut world, pos(0.0, 0.0, 0.0), 0.02, v3(100.0, 0.0, 0.0), true);
        world.step(DT, SUB_STEPS);
        let events: Vec<_> = crate::world::world_get_contact_events(&world).hit_events.to_vec();
        if !flag {
            assert!(events.is_empty(), "{events:?}");
            continue;
        }
        assert_eq!(events.len(), 1, "{events:?}");
        let e = events[0];
        assert_eq!(e.contact_id.index1, 0, "a sweep has no contact");
        assert!((e.approach_speed - 100.0).abs() < 1.0, "{e:?}");
        assert!(e.normal.x.abs() > 0.99, "{e:?}");
        assert!(x_of(&world, id) < 1.0);
    }
}

/// A static, frictionless, perfectly elastic thin box whose face, `half` long each way, lies on the plane through `at`
/// with unit normal `normal` (in the x-y plane): the box turned about z by the quaternion (0, 0, `sine`, `cosine`).
fn mirror(world: &mut World, at: Vec3, normal: Vec3, sine: f32, cosine: f32, half: f32) {
    let thick = 0.005;
    let mut def = default_body_def();
    def.position = pos(at.x - thick * normal.x, at.y - thick * normal.y, 0.0);
    def.rotation = crate::math_functions::Quat { v: v3(0.0, 0.0, sine), s: cosine };
    let id = create_body(world, &def);
    let mut shape = default_shape_def();
    shape.base_material.restitution = 1.0;
    shape.base_material.friction = 0.0;
    create_hull_shape(world, id, &shape, &make_box_hull(thick, half, 0.3).base);
}

/// One 1/240 s step (1.67 m): two 45 degree mirrors turn a 400 m/s bullet from +x to +y, then to -x, toward a static
/// ball beside its first path. Its third path lies inside the bounding box of the first two, which the searches
/// covered, but outside both paths, so only a search on that path finds the ball. The bullet must bounce off it, not
/// pass through. It spins (frictionless): each box grows by its piece's turn, the first box most, so the third path
/// lies inside that bounding box by about a millimetre on every side, not by rounding (in z the paths' boxes would
/// differ only by rounding).
#[test]
fn a_bullet_deflected_twice_in_a_sub_step_still_finds_what_its_third_path_meets() {
    let mut world = World::new(&no_gravity());
    let (r, s8, c8) = (0.01f32, 0.382_683_43f32, 0.923_879_5f32);
    let root_half = 0.707_106_77f32;
    // Mirror A, facing (-1, 1): its thin axis along (1, -1), a -45 degree turn.
    mirror(&mut world, v3(0.8, 0.0, 0.0), v3(-root_half, root_half, 0.0), -s8, c8, 0.3);
    // Mirror B above the first hit, facing (-1, -1): a +45 degree turn.
    let hit_x = 0.8 - r / root_half;
    mirror(&mut world, v3(hit_x, 0.4, 0.0), v3(-root_half, -root_half, 0.0), s8, c8, 0.3);
    // The ball on the third path, where B sends the bullet.
    let mut def = default_body_def();
    let ball_y = 0.4 - r / root_half;
    def.position = pos(0.5, ball_y, 0.0);
    let c_id = create_body(&mut world, &def);
    let mut shape = default_shape_def();
    shape.base_material.restitution = 1.0;
    shape.base_material.friction = 0.0;
    create_sphere_shape(&mut world, c_id, &shape, &Sphere { center: VEC3_ZERO, radius: 0.02 });

    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.swept = true;
    def.linear_velocity = v3(400.0, 0.0, 0.0);
    def.angular_velocity = v3(0.0, 0.0, 40.0);
    let id = create_body(&mut world, &def);
    create_sphere_shape(&mut world, id, &shape, &Sphere { center: VEC3_ZERO, radius: r });
    world.step(DT / SUB_STEPS as f32, 1);
    // Back off the ball (it meets it at about t = 0.87): on the mirrors' side of it, heading for B again.
    let p = body_get_position(&world, id);
    let v = body_get_linear_velocity(&world, id);
    assert!(
        p.x as f32 > 0.53 && v.x > 390.0,
        "the bullet passed through the ball: at ({}, {}) moving {v:?}, counters {:?}",
        p.x,
        p.y,
        world.swept_counters
    );
}

/// A bullet deflected after another body's reach grew is searched again within the larger reach, even when its new
/// path lies inside a box it searched before: that box's dynamic query reached only as far as the smaller reach.
/// One 1/240 s step (h): bullet S (120 m/s, 0.5 m a step) turns back at a wall at t = 0.5. A heavy bullet S2 knocks a
/// light unswept ball B (S's weight) to 240 m/s at t = 0.3, so the reach grows from 0.5 m to 0.7 m. B started out of
/// reach of S's first search and of its search at t = 0.3, but within the larger reach of S's path back, and meets S
/// at about t = 0.96. S must bounce off B, not pass it. S spins (frictionless), so its first box is wider than its
/// path back by the turn, about a millimetre on every side: inside it by more than rounding.
#[test]
fn a_bullet_deflected_after_the_reach_grew_finds_a_body_the_smaller_reach_missed() {
    let mut world = World::new(&no_gravity());
    let s = elastic_ball(&mut world, pos(0.0, 0.0, 0.0), 0.01, 1.0, v3(120.0, 0.0, 0.0));
    body_set_angular_velocity(&mut world, s, v3(0.0, 0.0, 40.0));
    wall(&mut world, 0.26, 0.1);
    let b_x = -0.655f32;
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.position = pos(b_x, 0.0, 0.0);
    let b = create_body(&mut world, &def);
    let mut shape = default_shape_def();
    shape.density = 1.0;
    shape.base_material.restitution = 1.0;
    shape.base_material.friction = 0.0;
    create_sphere_shape(&mut world, b, &shape, &Sphere { center: VEC3_ZERO, radius: 0.01 });
    // S2 closes the 0.15 m to B in 0.3 of the step.
    elastic_ball(&mut world, pos(b_x - 0.01 - 0.05 - 0.15, 0.0, 0.0), 0.05, 1000.0, v3(120.0, 0.0, 0.0));

    world.step(DT / SUB_STEPS as f32, 1);
    let (xs, xb) = (x_of(&world, s), x_of(&world, b));
    let vb = body_get_linear_velocity(&world, b);
    assert!(xs > xb, "S at {xs} passed B at {xb} (B moving {vb:?}), counters {:?}", world.swept_counters);
    // S and B weigh the same, so the elastic hit swaps their velocities.
    assert!(vb.x < 0.0, "B never met S: moving {vb:?}");
}

/// A swept ball C knocks an unswept ball n down from (`x`, 0.1) at about t = 0.33 and stops: all three balls weigh
/// the same, and C's 40 m/s goes to n. C starts 5 cm above n, beyond the contact solver's reach, so the sweep makes
/// that impact (a slop deep). n then crosses the line of a spinning bullet A at about t = 0.93. A sets off at
/// 120 m/s along +x from the origin. A's first search found n beside its path and turned it away: n was at rest,
/// 9 cm off A's line. Returns A and n.
fn knocked_into_a_bullet(world: &mut World, x: f32) -> (BodyId, BodyId) {
    let a = elastic_ball(world, pos(0.0, 0.0, 0.0), 0.01, 1.0, v3(120.0, 0.0, 0.0));
    body_set_angular_velocity(world, a, v3(0.0, 0.0, 40.0));
    let mut def = default_body_def();
    def.type_ = BodyType::Dynamic;
    def.position = pos(x, 0.1, 0.0);
    let n = create_body(world, &def);
    let mut shape = default_shape_def();
    shape.density = 1.0;
    shape.base_material.restitution = 1.0;
    shape.base_material.friction = 0.0;
    create_sphere_shape(world, n, &shape, &Sphere { center: VEC3_ZERO, radius: 0.01 });
    elastic_ball(world, pos(x, 0.1 + 0.01 + 0.01 + 0.05, 0.0), 0.01, 1.0, v3(0.0, -40.0, 0.0));
    (a, n)
}

/// One 1/240 s step: a ball knocked into a bullet's path is met (at about t = 0.9, as A nears x = 0.465), though the
/// bullet's search turned it away and the bullet is never deflected before they meet. The ball's new piece brings
/// the pair back.
#[test]
fn a_ball_knocked_into_a_bullet_s_path_is_met() {
    let mut world = World::new(&no_gravity());
    let (a, n) = knocked_into_a_bullet(&mut world, 0.465);
    world.step(DT / SUB_STEPS as f32, 1);
    let (va, vn) = (body_get_linear_velocity(&world, a), body_get_linear_velocity(&world, n));
    // The ball comes down across the bullet's line as the bullet arrives: the hit turns the bullet downward.
    assert!(va.y < -10.0, "the bullet passed the ball: the bullet moving {va:?}, the ball {vn:?}");
}

/// One 1/240 s step: a ball knocked toward a bullet's line is met on the bullet's way back from a wall (it turns at
/// about t = 0.51 and meets the ball at about t = 0.9, back near x = 0.045). When the ball was knocked, the paths
/// still didn't meet; the bullet's path back lies inside the box of its first search, so it isn't searched again.
/// Its new piece brings the pair back, because the ball's piece changed after the search turned it away.
#[test]
fn a_ball_knocked_toward_a_bullet_s_line_is_met_on_its_way_back() {
    let mut world = World::new(&no_gravity());
    let (a, n) = knocked_into_a_bullet(&mut world, 0.045);
    wall(&mut world, 0.26, 0.1);
    world.step(DT / SUB_STEPS as f32, 1);
    let (va, vn) = (body_get_linear_velocity(&world, a), body_get_linear_velocity(&world, n));
    assert!(va.y < -10.0, "the bullet passed the ball: the bullet moving {va:?}, the ball {vn:?}");
}
