//! The desktop hand in M1's ball pit (the M1 session of 2026-11-20, laptop B): its hit test reads each drawn
//! object's own shape, so a ray from a spawn slot reaches what the cursor points at.
//!
//! In that session the hand sat 0.2 m in front of the camera on every frame. pe-render tested every solid by its
//! bounding sphere and counted a ray starting inside one as a hit at 0. The floor box's sphere (radius 2.87 m) holds
//! an eye anywhere near a spawn slot, so the hover depth clamped to `min_depth`. This test opens the pit as pe-app
//! does (pe_harness::ballpit, a member's replica and its frame view), builds the targets as the window does
//! ([`Target::of_object`]), and points at the floor and at balls from an eye at a member's spawn point.

use glam::DVec3;
use pe_harness::ballpit;
use pe_render::desktop_hand::{DesktopHand, DesktopHandSettings, PointerInput, Target, first_hit};
use pe_render::pose::{WorldPose, offset_vec};
use pe_seams::{HostTime, Offset, Pos, Replica, RigTransform, Shape};
use pe_world::preset::BALL_R;

fn at(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).expect("in range")
}

#[test]
fn from_a_spawn_slot_the_hand_reaches_the_floor_and_the_balls() {
    let keys = ballpit::keys(2);
    let world = ballpit::genesis(11, 200, &keys).expect("the pit admits both members");
    let me = keys.sorted_keys()[0];
    let replica = ballpit::replica(world, me).expect("the replica opens");
    let view = replica.view();
    let targets: Vec<Target> = view.objects.iter().filter_map(|o| Target::of_object(o, WorldPose::of(o))).collect();

    // An eye 1.6 m above member 1's spawn point (pe_harness::ballpit's SPAWNS[0], 1.8 m from the middle).
    let eye = at(1.8, 1.6, 0.0);
    let floor = view
        .objects
        .iter()
        .filter(|o| matches!(o.shape, Some(Shape::Box { .. })))
        .max_by(|a, b| a.radius.total_cmp(&b.radius))
        .expect("the floor box");
    let to_floor = offset_vec(eye - floor.pos).length();
    assert!(
        to_floor < floor.radius as f64,
        "the eye is inside the floor's bounding sphere ({to_floor:.2} m < {:.2} m), the session's condition",
        floor.radius
    );

    // Straight down: the floor's top (the pit's origin plane), or a ball lying on it.
    let down = first_hit(eye, DVec3::NEG_Y, &targets).expect("the floor below");
    assert!((1.6 - 2.0 * BALL_R - 1e-6..=1.6 + 1e-6).contains(&down), "down: {down}");

    // Each ball: the ray enters it, or a solid before it, never at the eye.
    let mut balls: Vec<(f64, Pos)> = view
        .objects
        .iter()
        .filter(|o| matches!(o.shape, Some(Shape::Sphere { .. })) && o.hand_of.is_none())
        .map(|o| (offset_vec(o.pos - eye).length(), o.pos))
        .collect();
    assert!(balls.len() >= 100, "{} balls", balls.len());
    balls.sort_by(|a, b| a.0.total_cmp(&b.0));
    let rig = RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 };
    for (k, (dist, pos)) in balls.iter().take(20).enumerate() {
        let dir = offset_vec(*pos - eye) / *dist;
        let hit = first_hit(eye, dir, &targets).expect("the ball or what is before it");
        assert!(hit > 0.5 && hit <= dist - BALL_R + 1e-6, "ball {k}: hit {hit:.3} m, the ball at {dist:.3} m");
        let mut hand = DesktopHand::new(DesktopHandSettings::default());
        let input = PointerInput {
            at: HostTime::from_millis(0),
            origin: eye,
            dir,
            left: false,
            shift: false,
            wheel: 0.0,
            roll: 0.0,
        };
        hand.update(&input, &targets, &rig);
        assert!((hand.depth() - (hit - 0.03)).abs() < 1e-9, "ball {k}: the hand 3 cm short of the hit");
    }
}
