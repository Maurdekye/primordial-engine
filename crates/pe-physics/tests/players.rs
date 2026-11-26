//! Players (@item:primordial-engine/locomotion-l1-pe-physics-the-first-player-mover-m): the first mover on flat
//! ground, walls, steps, slopes, jumps, light and heavy bodies, and through images and worker counts.

use pe_physics::{
    BodyDesc, BodyKind, Geometry, MoverInput, ObjectId, PLAYER, PhysicsConfig, PhysicsWorld, PlayerParams, ShapeDesc,
    walk_axes,
};

const DT: f32 = 1.0 / 60.0;
const ME: u32 = 1;

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((7u128 << 32) | u128::from(n))
}

fn static_box(pw: &mut PhysicsWorld, n: u32, at: [f64; 3], half: [f32; 3]) {
    let desc = BodyDesc::new(BodyKind::Static, at).with_shape(ShapeDesc::new(Geometry::Box { half_extents: half }));
    pw.add_body(id(n), &desc).unwrap();
}

/// A world with a floor whose top is at y = 0 and a player standing on it at `feet`.
fn world_at(feet: [f64; 3]) -> PhysicsWorld {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    static_box(&mut pw, 100, [0.0, -0.5, 0.0], [30.0, 0.5, 30.0]);
    pw.add_player(id(ME), feet, &PLAYER).unwrap();
    pw
}

fn feet(pw: &PhysicsWorld) -> [f64; 3] {
    pw.pose(id(ME)).unwrap().position
}

/// One frame: the move, then the step.
fn frame(pw: &mut PhysicsWorld, input: MoverInput) {
    pw.move_player(id(ME), &input, &[], &PLAYER, DT).unwrap();
    pw.step(DT).unwrap();
}

fn walking(x: f32, z: f32) -> MoverInput {
    MoverInput { walk: [x, z], ..MoverInput::default() }
}

const JUMP: MoverInput = MoverInput { walk: [0.0, 0.0], room: [0.0, 0.0], jump: true, hands: [None; 2] };

#[test]
fn a_player_standing_still_stays_on_the_ground() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    for _ in 0..120 {
        frame(&mut pw, MoverInput::default());
    }
    let p = feet(&pw);
    assert!(p[1].abs() < 0.01 && p[0].abs() < 1.0e-6 && p[2].abs() < 1.0e-6, "feet at {p:?}");
    assert!(pw.player(id(ME)).unwrap().grounded);
}

#[test]
fn a_player_dropped_from_above_lands_on_its_feet() {
    let mut pw = world_at([0.0, 2.0, 0.0]);
    for _ in 0..90 {
        frame(&mut pw, MoverInput::default());
    }
    let p = feet(&pw);
    assert!(p[1].abs() < 0.01, "feet at {p:?}");
    assert!(pw.player(id(ME)).unwrap().grounded);
}

#[test]
fn walking_reaches_the_asked_speed_and_no_more_than_the_cap() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    for _ in 0..120 {
        frame(&mut pw, walking(0.0, -4.0));
    }
    let p = feet(&pw);
    // 2 s at 4 m/s, less the 0.1 s it takes to reach it at 40 m/s².
    assert!((-8.0..=-7.7).contains(&p[2]) && p[0].abs() < 1.0e-4, "feet at {p:?}");
    let v = pw.player(id(ME)).unwrap().velocity;
    assert!((v[2] + 4.0).abs() < 1.0e-4, "velocity {v:?}");

    let mut fast = world_at([0.0, 0.0, 0.0]);
    for _ in 0..60 {
        frame(&mut fast, walking(30.0, 0.0));
    }
    let v = fast.player(id(ME)).unwrap().velocity;
    assert!((v[0] - PLAYER.max_walk).abs() < 1.0e-4, "velocity {v:?}");
}

#[test]
fn a_wall_stops_the_player_and_it_slides_along_it() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    // A wall whose near face is at x = 2.
    static_box(&mut pw, 101, [2.5, 1.0, 0.0], [0.5, 1.0, 10.0]);
    for _ in 0..120 {
        frame(&mut pw, walking(4.0, 0.0));
    }
    // Box3D's mover lets the capsule rest up to a linear slop (5 mm) inside what it touches.
    let p = feet(&pw);
    assert!(p[0] <= 2.0 - f64::from(PLAYER.radius) + 6.0e-3 && p[0] > 1.6, "feet at {p:?}");
    let v = pw.player(id(ME)).unwrap().velocity;
    assert!(v[0].abs() < 1.0e-3, "velocity into the wall {v:?}");
    // At 45° into it, the player slides along it.
    for _ in 0..60 {
        frame(&mut pw, walking(3.0, 3.0));
    }
    let q = feet(&pw);
    assert!(q[0] <= 2.0 - f64::from(PLAYER.radius) + 6.0e-3 && q[2] - p[2] > 2.0, "feet at {q:?}");
}

#[test]
fn a_step_lower_than_the_ride_is_climbed_and_a_higher_one_blocks() {
    let mut low = world_at([0.0, 0.0, 0.0]);
    // A 0.2 m step from x = 1 on.
    static_box(&mut low, 101, [6.0, 0.1, 0.0], [5.0, 0.1, 5.0]);
    for _ in 0..90 {
        frame(&mut low, walking(3.0, 0.0));
    }
    let p = feet(&low);
    assert!(p[0] > 3.0 && (p[1] - 0.2).abs() < 0.01, "feet at {p:?}");

    let mut high = world_at([0.0, 0.0, 0.0]);
    static_box(&mut high, 101, [6.0, 0.25, 0.0], [5.0, 0.25, 5.0]);
    for _ in 0..90 {
        frame(&mut high, walking(3.0, 0.0));
    }
    let p = feet(&high);
    assert!(p[0] < 1.0 && p[1].abs() < 0.01, "feet at {p:?}");
}

/// A ramp rising toward +x from x = 1, `rise` metres over 4, as a hull, object `n`. The rises are literals, never
/// `tan()`: the standard library's trigonometry is the platform's, and tier 2 compares the golden across platforms.
fn ramp(pw: &mut PhysicsWorld, n: u32, rise: f32) {
    let points =
        vec![[1.0, 0.0, -3.0], [1.0, 0.0, 3.0], [5.0, 0.0, -3.0], [5.0, 0.0, 3.0], [5.0, rise, -3.0], [5.0, rise, 3.0]];
    let desc = BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(ShapeDesc::new(Geometry::Hull { points }));
    pw.add_body(id(n), &desc).unwrap();
}

/// 4 tan 30°, 4 tan 60° and 4 tan 25°: ramps of 30°, 60° and 25°.
const RISE_30: f32 = 2.309_401;
const RISE_60: f32 = 6.928_203;
const RISE_25: f32 = 1.865_231;

#[test]
fn a_walkable_slope_is_climbed_and_a_steep_one_is_not() {
    let mut gentle = world_at([0.0, 0.0, 0.0]);
    ramp(&mut gentle, 101, RISE_30);
    for _ in 0..90 {
        frame(&mut gentle, walking(2.0, 0.0));
    }
    let p = feet(&gentle);
    assert!(p[0] > 2.5 && p[1] > 0.6, "feet at {p:?}");

    // The steep one blocks where the capsule touches its face (the feet stop near x = 1), and there the player stays
    // on the floor: the face nearer the feet doesn't hide it.
    let mut steep = world_at([0.0, 0.0, 0.0]);
    ramp(&mut steep, 101, RISE_60);
    for f in 0..90 {
        frame(&mut steep, walking(2.0, 0.0));
        let p = feet(&steep);
        assert!(p[1].abs() < 0.01 && p[0] < 1.1, "frame {f}: feet at {p:?}");
    }
    assert!(steep.player(id(ME)).unwrap().grounded);
}

#[test]
fn a_player_dropped_on_a_steep_face_slides_down_it() {
    // @design-reviewer's M1 on cbe1603b: a player that falls onto a face too steep to stand on (the 60° ramp, 3.5 m up
    // under x = 3) slides down it to the floor. Its velocity is the slide's, so the fall it records is the fall it
    // makes, and at the foot of the face it lands on the floor, never sinking below it.
    let mut pw = world_at([3.0, 4.5, 0.0]);
    ramp(&mut pw, 101, RISE_60);
    let (mut landed, mut descent, mut recorded) = (None, 0.0f64, 0.0f64);
    let mut last = feet(&pw);
    for f in 0..150 {
        frame(&mut pw, MoverInput::default());
        let (p, state) = (feet(&pw), pw.player(id(ME)).unwrap());
        assert!(p[1] > -0.01, "frame {f}: the feet sank below the floor, to {p:?}");
        if state.grounded && landed.is_none() {
            landed = Some(f);
        }
        // On the face: pushed off x = 3 and not landed yet.
        if landed.is_none() && p[0] < 3.0 - 1.0e-3 {
            descent += last[1] - p[1];
            recorded -= f64::from(state.velocity[1] * DT);
        }
        last = p;
    }
    let p = feet(&pw);
    assert!(landed.is_some_and(|f| f < 120), "landed at frame {landed:?}, feet at {p:?}");
    assert!(p[1].abs() < 0.01 && p[0] < 1.5, "feet at {p:?}");
    assert!(descent > 2.0 && (recorded / descent - 1.0).abs() < 0.1, "fell {descent} m, recorded {recorded} m");
}

#[test]
fn a_jump_rises_as_high_as_its_speed_and_gravity_say() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    frame(&mut pw, MoverInput::default());
    frame(&mut pw, JUMP);
    let mut top = 0.0f64;
    for _ in 0..90 {
        frame(&mut pw, MoverInput::default());
        top = top.max(feet(&pw)[1]);
    }
    let want = f64::from(PLAYER.jump_speed * PLAYER.jump_speed / (2.0 * PLAYER.gravity));
    assert!((top - want).abs() < 0.05, "apex {top}, want {want}");
    assert!(feet(&pw)[1].abs() < 0.01, "landed at {:?}", feet(&pw));
}

/// Walk off a ledge at x = 1 (the floor ends there; the ground beyond is 2 m lower), pressing jump `after` frames
/// after the player left it: whether it jumped.
fn jumped_after_leaving(after: usize) -> bool {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    static_box(&mut pw, 100, [-9.0, -0.5, 0.0], [10.0, 0.5, 10.0]);
    static_box(&mut pw, 102, [0.0, -2.5, 0.0], [30.0, 0.5, 30.0]);
    pw.add_player(id(ME), [0.0, 0.0, 0.0], &PLAYER).unwrap();
    let mut left = None;
    for f in 0..120 {
        let input =
            MoverInput { walk: [2.0, 0.0], jump: left.is_some_and(|l| f == l + after), ..MoverInput::default() };
        pw.move_player(id(ME), &input, &[], &PLAYER, DT).unwrap();
        pw.step(DT).unwrap();
        let state = pw.player(id(ME)).unwrap();
        if left.is_none() && !state.grounded && state.velocity[1] <= 0.0 {
            left = Some(f + 1);
        }
        if state.velocity[1] > 0.0 {
            return true;
        }
    }
    false
}

#[test]
fn coyote_time_lets_a_late_jump_through_and_no_later() {
    assert!(jumped_after_leaving(2), "a jump 2 frames after leaving the ledge");
    assert!(!jumped_after_leaving(12), "a jump 12 frames after leaving the ledge");
}

/// Drop the player from `height`, pressing jump `before` frames before the frame it lands: whether it jumped on
/// landing.
fn jumped_on_landing(before: usize) -> bool {
    // A first fall finds the landing frame.
    let landing = {
        let mut pw = world_at([0.0, 1.0, 0.0]);
        (0..120usize)
            .find(|_| {
                frame(&mut pw, MoverInput::default());
                pw.player(id(ME)).unwrap().grounded
            })
            .unwrap()
    };
    let mut pw = world_at([0.0, 1.0, 0.0]);
    for f in 0..=landing + 2 {
        let input = MoverInput { jump: f + before == landing, ..MoverInput::default() };
        frame(&mut pw, input);
        if f > landing.saturating_sub(before) && pw.player(id(ME)).unwrap().velocity[1] > 0.0 {
            return true;
        }
    }
    false
}

#[test]
fn a_press_just_before_landing_jumps_on_landing() {
    assert!(jumped_on_landing(3), "a press 3 frames early");
    assert!(!jumped_on_landing(12), "a press 12 frames early");
}

#[test]
fn a_ceiling_stops_a_jump() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    // A ceiling whose underside is at 2 m.
    static_box(&mut pw, 101, [0.0, 2.5, 0.0], [5.0, 0.5, 5.0]);
    frame(&mut pw, MoverInput::default());
    frame(&mut pw, JUMP);
    let mut top = 0.0f64;
    for _ in 0..60 {
        frame(&mut pw, MoverInput::default());
        top = top.max(feet(&pw)[1]);
    }
    assert!(top <= 2.0 - f64::from(PLAYER.height) + 0.01, "the head reached {}", top + f64::from(PLAYER.height));
}

fn dynamic_box(pw: &mut PhysicsWorld, n: u32, at: [f64; 3], half: [f32; 3], mass: f32) {
    let volume = 8.0 * half[0] * half[1] * half[2];
    let desc = BodyDesc::new(BodyKind::Dynamic, at)
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: half }).with_density(mass / volume));
    pw.add_body(id(n), &desc).unwrap();
}

fn ball(pw: &mut PhysicsWorld, n: u32, at: [f64; 3], radius: f32, mass: f32) {
    let volume = 4.0 / 3.0 * std::f32::consts::PI * radius * radius * radius;
    let desc = BodyDesc::new(BodyKind::Dynamic, at)
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius }).with_density(mass / volume));
    pw.add_body(id(n), &desc).unwrap();
}

#[test]
fn a_heavy_crate_blocks_the_player_and_holds_it_up() {
    // A 200 kg crate 1 m tall, its near face at x = 2: it blocks, and it doesn't move.
    let mut pw = world_at([0.0, 0.0, 0.0]);
    dynamic_box(&mut pw, 101, [2.5, 0.5, 0.0], [0.5, 0.5, 0.5], 200.0);
    for _ in 0..120 {
        frame(&mut pw, walking(4.0, 0.0));
    }
    let p = feet(&pw);
    let crate_at = pw.pose(id(101)).unwrap().position;
    assert!(p[0] < 2.0 - f64::from(PLAYER.radius) + 6.0e-3, "feet at {p:?}");
    assert!((crate_at[0] - 2.5).abs() < 1.0e-3, "the crate moved to {crate_at:?}");

    // A 100 kg slab 0.2 m tall is a step: the player stands on it, and its weight presses on it.
    let mut pw = world_at([0.0, 0.0, 0.0]);
    dynamic_box(&mut pw, 101, [3.0, 0.1, 0.0], [1.5, 0.1, 1.5], 100.0);
    for _ in 0..60 {
        frame(&mut pw, walking(3.0, 0.0));
    }
    for _ in 0..60 {
        frame(&mut pw, MoverInput::default());
    }
    let p = feet(&pw);
    assert!(p[0] > 2.0 && (p[1] - 0.2).abs() < 0.01, "feet at {p:?}");
    assert!(pw.player(id(ME)).unwrap().grounded);
}

#[test]
fn a_light_ball_in_the_way_is_shoved_and_the_player_walks_on() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    ball(&mut pw, 101, [2.0, 0.1, 0.0], 0.1, 1.0);
    for _ in 0..90 {
        frame(&mut pw, walking(3.0, 0.0));
    }
    let p = feet(&pw);
    let b = pw.pose(id(101)).unwrap().position;
    assert!(p[0] > 4.0, "the player stopped at {p:?}");
    assert!(b[0] > 2.3 || b[2].abs() > 0.3, "the ball stayed at {b:?}");
}

#[test]
fn a_held_ball_is_not_shoved() {
    let run = |held: &[ObjectId]| {
        let mut pw = world_at([0.0, 0.0, 0.0]);
        ball(&mut pw, 101, [0.3, 0.6, 0.0], 0.1, 1.0);
        for _ in 0..30 {
            pw.move_player(id(ME), &walking(1.0, 0.0), held, &PLAYER, DT).unwrap();
            pw.step(DT).unwrap();
        }
        pw.pose(id(101)).unwrap().position
    };
    let free = run(&[]);
    let held = run(&[id(101)]);
    assert!(free[0] > 0.6 || free[2].abs() > 0.3, "a free ball inside the player is shoved: {free:?}");
    assert!((held[0] - 0.3).abs() < 0.01 && held[2].abs() < 0.01, "the held ball was shoved: {held:?}");
}

#[test]
fn a_player_spawned_inside_a_box_is_pushed_out() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    // A static block around the capsule's core, its faces 0.4 m from the core in x and z.
    static_box(&mut pw, 101, [0.1, 1.0, 0.0], [0.5, 0.7, 2.0]);
    for _ in 0..10 {
        frame(&mut pw, MoverInput::default());
    }
    let p = feet(&pw);
    // Out through the nearest face, x = -0.4: the capsule's side there or beyond.
    assert!(p[0] < -0.4 - f64::from(PLAYER.radius) + 0.02, "feet at {p:?}");
}

#[test]
fn a_crate_pinning_a_player_against_a_wall_never_pushes_it_into_the_wall() {
    // A wall with its face at x = -0.3, and a crate closing on the player from +x at 1 m/s: a kinematic one, and a
    // 200 kg dynamic one driven at that speed every frame, as a hand holding it would (@coordinator-opus). The dynamic one ends against the wall with the player inside it.
    for kinematic in [true, false] {
        let mut pw = world_at([0.0, 0.0, 0.0]);
        static_box(&mut pw, 101, [-0.8, 1.0, 0.0], [0.5, 1.0, 5.0]);
        if kinematic {
            let desc = BodyDesc::new(BodyKind::Kinematic, [1.5, 0.6, 0.0])
                .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [0.5, 0.5, 0.5] }))
                .with_velocity([-1.0, 0.0, 0.0], [0.0; 3]);
            pw.add_body(id(102), &desc).unwrap();
        } else {
            dynamic_box(&mut pw, 102, [1.5, 0.5, 0.0], [0.5, 0.5, 0.5], 200.0);
        }
        for f in 0..120 {
            if !kinematic {
                pw.set_velocity(id(102), [-1.0, 0.0, 0.0], [0.0; 3]).unwrap();
            }
            frame(&mut pw, MoverInput::default());
            let p = feet(&pw);
            // The capsule's side stays within a slop of the wall's face (x = -0.3): the static wall wins.
            let held = p.iter().all(|c| c.is_finite()) && p[0] > -0.3 + f64::from(PLAYER.radius) - 6.0e-3;
            assert!(held, "kinematic {kinematic}, frame {f}: feet at {p:?}");
        }
        let crate_at = pw.pose(id(102)).unwrap().position;
        assert!(crate_at[0] < 0.4, "kinematic {kinematic}: the crate never reached the player, at {crate_at:?}");
    }
}

#[test]
fn a_corner_stops_the_player_and_holds_it() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    // Two walls meeting in an inside corner, their faces at x = 2 and z = 2.
    static_box(&mut pw, 101, [2.5, 1.0, 0.0], [0.5, 1.0, 10.0]);
    static_box(&mut pw, 102, [0.0, 1.0, 2.5], [10.0, 1.0, 0.5]);
    let corner = 2.0 - f64::from(PLAYER.radius);
    let mut last = feet(&pw);
    for f in 0..150 {
        frame(&mut pw, walking(3.0, 3.0));
        let p = feet(&pw);
        assert!(p[0] <= corner + 6.0e-3 && p[2] <= corner + 6.0e-3, "frame {f}: feet at {p:?}");
        if f >= 120 {
            // In the corner it stays put: no jitter, no creep along either wall.
            let off = (p[0] - last[0]).abs().max((p[2] - last[2]).abs());
            assert!(off < 1.0e-4, "frame {f}: moved {off} m in the corner, feet at {p:?}");
        }
        last = p;
    }
    let p = feet(&pw);
    assert!(p[0] > corner - 0.01 && p[2] > corner - 0.01, "feet at {p:?}");
    let v = pw.player(id(ME)).unwrap().velocity;
    assert!(v[0].abs() < 1.0e-3 && v[2].abs() < 1.0e-3, "velocity into the corner {v:?}");
}

/// Six light balls at the player's shins, in a ring a little inside the push volume.
const RING: [[f64; 2]; 6] = [[0.25, 0.0], [-0.25, 0.0], [0.0, 0.25], [0.0, -0.25], [0.18, 0.18], [-0.18, -0.18]];

#[test]
fn a_kick_of_several_balls_is_the_same_whatever_order_they_were_made_in() {
    // The balls are made in id order in one world and in reverse in the other, so Box3D numbers them differently and
    // its queries meet them in another order. Each ball's kick depends only on the ball (its depth and speed, the
    // kicks given in object-id order), so after one move every ball has the same velocity in both, bit for bit.
    let kicked = |reverse: bool| {
        let mut pw = world_at([0.0, 0.0, 0.0]);
        let mut order: Vec<u32> = (0..6).collect();
        if reverse {
            order.reverse();
        }
        for k in order {
            let [x, z] = RING[k as usize];
            ball(&mut pw, 110 + k, [x, 0.1, z], 0.1, 0.5);
        }
        pw.move_player(id(ME), &MoverInput::default(), &[], &PLAYER, DT).unwrap();
        (0..6u32).map(|k| pw.velocity(id(110 + k)).unwrap().0.map(f32::to_bits)).collect::<Vec<_>>()
    };
    let forward = kicked(false);
    assert!(forward.iter().all(|v| v.iter().any(|&c| f32::from_bits(c) != 0.0)), "a ball wasn't kicked: {forward:?}");
    assert_eq!(forward, kicked(true));
}

#[test]
fn the_pose_returned_is_the_pose_after_the_step() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    for f in 0..120 {
        let input = if f == 30 { JUMP } else { walking(3.0, 1.0) };
        let pose = pw.move_player(id(ME), &input, &[], &PLAYER, DT).unwrap();
        pw.step(DT).unwrap();
        let after = pw.pose(id(ME)).unwrap();
        let off = (0..3).map(|i| (after.position[i] - pose.position[i]).abs()).fold(0.0, f64::max);
        assert!(off < 1.0e-5, "frame {f}: {off} m off");
        assert_eq!(after.rotation, [0.0, 0.0, 0.0, 1.0]);
    }
}

#[test]
fn walk_axes_turn_with_the_yaw() {
    let close = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 2.0e-3 && (a[1] - b[1]).abs() < 2.0e-3;
    let quarter = 1u32 << 30;
    assert!(close(walk_axes([0.0, 1.0], 0), [0.0, -1.0]), "forward at yaw 0 is -z");
    assert!(close(walk_axes([1.0, 0.0], 0), [1.0, 0.0]), "right at yaw 0 is +x");
    assert!(close(walk_axes([0.0, 1.0], quarter), [-1.0, 0.0]), "forward a quarter turn left is -x");
    assert!(close(walk_axes([0.0, 1.0], 2 * quarter), [0.0, 1.0]), "forward half a turn round is +z");
    assert!(close(walk_axes([0.0, 1.0], 3 * quarter), [1.0, 0.0]), "forward three quarters round is +x");
}

#[test]
fn moves_are_refused_for_bad_input() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    static_box(&mut pw, 101, [5.0, 0.5, 5.0], [0.5, 0.5, 0.5]);
    let bad = MoverInput { walk: [f32::NAN, 0.0], ..MoverInput::default() };
    assert!(pw.move_player(id(ME), &bad, &[], &PLAYER, DT).is_err());
    assert!(pw.move_player(id(ME), &MoverInput::default(), &[], &PLAYER, 0.0).is_err());
    assert!(pw.move_player(id(101), &MoverInput::default(), &[], &PLAYER, DT).is_err());
    let tall = PlayerParams { ride: 2.0, ..PLAYER };
    assert!(pw.move_player(id(ME), &MoverInput::default(), &[], &tall, DT).is_err());
    assert!(pw.add_player(id(ME), [0.0; 3], &PLAYER).is_err(), "already bound");
}

/// A scripted walk-and-jump: a 600-frame course over a step, a slope, a heavy crate, a ledge and light balls, with
/// jumps, turns and room-scale steps. The check hash every 60 frames. With `restore_at`, the world is saved to an
/// image and restored from it at that frame, as a rollback to the present would.
fn walk_and_jump(workers: i32, restore_at: Option<u32>) -> Vec<String> {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine().with_workers(workers)).unwrap();
    static_box(&mut pw, 100, [0.0, -0.5, 0.0], [30.0, 0.5, 30.0]);
    static_box(&mut pw, 101, [4.0, 0.1, 0.0], [1.0, 0.1, 3.0]);
    ramp(&mut pw, 103, RISE_25);
    dynamic_box(&mut pw, 102, [-3.0, 0.4, 2.0], [0.6, 0.4, 0.6], 150.0);
    for k in 0..6 {
        ball(&mut pw, 110 + k, [1.0 + 0.4 * f64::from(k), 0.1, -2.0], 0.1, 0.5);
    }
    pw.add_player(id(ME), [0.0, 0.0, 0.0], &PLAYER).unwrap();
    let mut out = Vec::new();
    for f in 0..600u32 {
        if restore_at == Some(f) {
            let image = pw.save_image().unwrap();
            pw.restore_image(&image).unwrap();
        }
        let yaw = f.wrapping_mul(7_158_278); // about a turn every 600 frames
        let stick = [((f / 40) % 3) as f32 - 1.0, 1.0];
        let walk = walk_axes(stick, yaw).map(|c| 3.0 * c);
        let room = if f % 50 == 0 { [0.05, -0.03] } else { [0.0, 0.0] };
        let input = MoverInput { walk, room, jump: f % 97 == 13, hands: [None; 2] };
        pw.move_player(id(ME), &input, &[], &PLAYER, DT).unwrap();
        pw.step(DT).unwrap();
        if f % 60 == 59 {
            out.push(format!("{f} {:?} {}", pw.pose(id(ME)).unwrap().position, pw.check_hash().unwrap()));
        }
    }
    out
}

#[test]
fn the_walk_and_jump_is_the_same_at_every_worker_count_and_after_an_image() {
    let one = walk_and_jump(1, None);
    for workers in [2, 4, 8] {
        assert_eq!(walk_and_jump(workers, None), one, "{workers} workers");
    }
    assert_eq!(walk_and_jump(4, Some(300)), one, "through an image at frame 300");
    for line in &one {
        println!("RESULT players walk-and-jump {line}");
    }
}

#[test]
fn a_restored_image_replays_a_player_exactly() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    for k in 0..4 {
        ball(&mut pw, 110 + k, [1.0 + 0.3 * f64::from(k), 0.1, 0.0], 0.1, 0.5);
    }
    let input = |f: u32| MoverInput {
        walk: walk_axes([0.3, 1.0], f.wrapping_mul(40_000_000)).map(|c| 3.0 * c),
        room: [0.0; 2],
        jump: f % 45 == 7,
        hands: [None; 2],
    };
    for f in 0..30 {
        pw.move_player(id(ME), &input(f), &[], &PLAYER, DT).unwrap();
        pw.step(DT).unwrap();
    }
    let image = pw.save_image().unwrap();
    let mut ahead = Vec::new();
    for f in 30..120 {
        pw.move_player(id(ME), &input(f), &[], &PLAYER, DT).unwrap();
        pw.step(DT).unwrap();
        ahead.push((pw.check_hash().unwrap(), pw.state_hash().unwrap()));
    }
    pw.restore_image(&image).unwrap();
    for (k, f) in (30..120).enumerate() {
        pw.move_player(id(ME), &input(f), &[], &PLAYER, DT).unwrap();
        pw.step(DT).unwrap();
        assert_eq!((pw.check_hash().unwrap(), pw.state_hash().unwrap()), ahead[k], "frame {f} after the restore");
    }
}

#[test]
fn a_body_crossing_the_heavy_line_under_rollback_replays_exactly() {
    // A crate of 30 kg (shoved) that becomes 50 kg (solid) at frame 40: rebuilt, as an edit would.
    let script = |pw: &mut PhysicsWorld, f: u32| {
        if f == 40 {
            let at = pw.pose(id(101)).unwrap().position;
            pw.remove_body(id(101)).unwrap();
            dynamic_box(pw, 101, at, [0.3, 0.3, 0.3], 50.0);
        }
        pw.move_player(id(ME), &walking(2.0, 0.0), &[], &PLAYER, DT).unwrap();
        pw.step(DT).unwrap();
    };
    let mut pw = world_at([0.0, 0.0, 0.0]);
    dynamic_box(&mut pw, 101, [1.5, 0.3, 0.0], [0.3, 0.3, 0.3], 30.0);
    for f in 0..20 {
        script(&mut pw, f);
    }
    let image = pw.save_image().unwrap();
    let mut ahead = Vec::new();
    for f in 20..90 {
        script(&mut pw, f);
        ahead.push(pw.check_hash().unwrap());
    }
    pw.restore_image(&image).unwrap();
    for (k, f) in (20..90).enumerate() {
        script(&mut pw, f);
        assert_eq!(pw.check_hash().unwrap(), ahead[k], "frame {f} after the restore");
    }
}

/// The friend test of 11-27 (point 3): leaning down to pick a ball up carries the capsule forward under
/// the head (the room step) over the ball. A ball by the player's own hand isn't shoved; without the hand it is.
#[test]
fn leaning_down_to_a_ball_by_the_hand_doesnt_shove_it() {
    let run = |hand: Option<[f64; 3]>| {
        let mut pw = world_at([0.0, 0.0, 0.0]);
        ball(&mut pw, 101, [0.45, 0.1, 0.0], 0.1, 1.0);
        for f in 0..40 {
            // The head leans 0.4 m forward over 8 frames, then holds there.
            let room = if f < 8 { [0.05, 0.0] } else { [0.0, 0.0] };
            let input = MoverInput { room, hands: [hand, None], ..MoverInput::default() };
            pw.move_player(id(ME), &input, &[], &PLAYER, DT).unwrap();
            pw.step(DT).unwrap();
        }
        pw.pose(id(101)).unwrap().position
    };
    let free = run(None);
    let reached = run(Some([0.45, 0.2, 0.0]));
    assert!(free[0] > 0.6 || free[2].abs() > 0.15, "with no hand by it the ball is shoved: {free:?}");
    assert!((reached[0] - 0.45).abs() < 0.01 && reached[2].abs() < 0.01, "the ball by the hand moved: {reached:?}");
}

/// The friend test of 11-27: players are walls to each other. One walks into the other and stops at
/// its capsule, and the one walked into stays where it stands.
#[test]
fn a_player_walking_into_another_stops_at_it() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    pw.add_player(id(2), [1.0, 0.0, 0.0], &PLAYER).unwrap();
    for _ in 0..90 {
        pw.move_player(id(ME), &walking(4.0, 0.0), &[], &PLAYER, DT).unwrap();
        pw.move_player(id(2), &MoverInput::default(), &[], &PLAYER, DT).unwrap();
        pw.step(DT).unwrap();
    }
    let (a, b) = (feet(&pw), pw.pose(id(2)).unwrap().position);
    let gap = 2.0 * f64::from(PLAYER.radius);
    assert!(a[0] < 1.0 - gap + 6.0e-3 && a[0] > 1.0 - gap - 0.05, "the walker's feet at {a:?}");
    assert!((b[0] - 1.0).abs() < 1.0e-3 && b[2].abs() < 1.0e-3, "the other player moved to {b:?}");
}

/// Players are walls only to each other's moves: no solver pair forms, so a heavy ball (never shoved) dropped onto a
/// player falls through it to the floor, as before.
#[test]
fn a_ball_dropped_on_a_player_falls_through_it() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    ball(&mut pw, 101, [0.0, 2.5, 0.0], 0.1, 50.0);
    for _ in 0..90 {
        frame(&mut pw, MoverInput::default());
    }
    let b = pw.pose(id(101)).unwrap().position;
    assert!(b[1] < 0.2, "the ball stopped at {b:?}");
}

/// Two players on one spot (a respawn to a fixed point, say) step apart by id and stay near it, instead of both
/// stepping the same way every frame (@chunks-reviewer's M1 on a306b59c).
#[test]
fn two_players_on_one_spot_step_apart_and_stay() {
    let mut pw = world_at([0.0, 0.0, 0.0]);
    pw.add_player(id(2), [0.0, 0.0, 0.0], &PLAYER).unwrap();
    for _ in 0..30 {
        pw.move_player(id(ME), &MoverInput::default(), &[], &PLAYER, DT).unwrap();
        pw.move_player(id(2), &MoverInput::default(), &[], &PLAYER, DT).unwrap();
        pw.step(DT).unwrap();
    }
    let (a, b) = (feet(&pw), pw.pose(id(2)).unwrap().position);
    let apart = ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
    assert!(apart >= 2.0 * f64::from(PLAYER.radius) - 0.01 && apart <= 1.1, "{a:?} {b:?}");
    assert!(a[0].abs() <= 0.6 && b[0].abs() <= 0.6, "they ran off: {a:?} {b:?}");
}
