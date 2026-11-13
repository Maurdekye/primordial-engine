//! Hand bodies: each member's hands as pe-physics bodies that push what they touch (replica.md 3.3; world-model.md
//! 8.1; D18b).
//!
//! A hand object has no `Body` in the world model: its body is the replica's own, a kinematic sphere of
//! [`HAND_RADIUS`] under the hand object's id, driven towards the target its inputs write. Like everything the
//! replica does, it is a function of the world model's rows and the physics state, applied in id order inside each
//! frame's step, and nothing about it is kept outside the two. A rollback or a replay restores the physics image,
//! hand bodies included, and the next frames rebuild the rest, so they repeat exactly (8.1).
//!
//! - **When a hand is physical** is its mode (D18b, per member; @coordinator-opus mb9721962f21f, m04577af18680):
//!   "closed hand" ([`hand_mode::CLOSED_HAND`], the default) while the hand is closed (its input's `engaged`, the
//!   grasp); "always within reach" ([`hand_mode::ALWAYS_WITHIN_REACH`]) always. A hand that holds something stays
//!   physical, so its grab has a body to hold by. A hand that is not physical has no body, so it passes through
//!   everything.
//! - **A body that appears** (a hand closes, or its mode flips to always) is placed at its target, not swept there,
//!   and overlapping bodies are pushed out by Box3D's soft contacts at the world's contact speed (3 m/s), so a flip
//!   inside a pile moves its balls apart gently instead of throwing them (8.1).
//! - **Within an input's span** the body is driven to the target in one frame: `set_target`, so it pushes what it
//!   meets at the speed it moves. A move faster than [`MAX_HAND_PUSH_SPEED`] in one frame is a jump: the body is put
//!   at the target with no velocity, moved but not swept, so a pose jump never hits anything at that speed (8.1's
//!   cap by `max_push_speed`).
//! - **After the span, with no newer input:** if the input came as part of a stream (within its predecessor's span
//!   plus [`COAST`]), the body coasts at its last velocity, at most [`COAST_SPEED`], until [`COAST`] after the span;
//!   then it goes back to the hand's target by the same rule as within a span, and stays there, so it never parks
//!   away from the hand everyone sees. Only the frames in the hand's row decide, so a re-simulation repeats them
//!   (3.3, D52).

use std::time::Duration;

use pe_physics::{BodyDesc, BodyKind, Geometry, PhysicsError, PhysicsWorld, Pose, ShapeDesc};
use pe_seams::{Frame, ObjectId, Pos, SimRate, frames_ceil};

use crate::world::{HAND_RADIUS, HandRow, WorldModel};

/// D18b's two modes, as pe-world's `Hand.mode` holds them (world-model.md 8.1).
pub mod hand_mode {
    /// "Closed hand": physical while the hand is closed, and while it holds something. The default.
    pub const CLOSED_HAND: u8 = 0;
    /// "Always within reach": always physical.
    pub const ALWAYS_WITHIN_REACH: u8 = 1;
}

/// How long a hand coasts after its input's span, when the input came as part of a stream (replica.md 3.3).
pub const COAST: Duration = Duration::from_millis(100);
/// The fastest a coasting hand moves, m/s (replica.md 3.3).
pub const COAST_SPEED: f32 = 3.0;
/// The fastest a hand body pushes, m/s: a move faster than this in one frame is a jump (world-model.md 8.1's
/// `max_push_speed`, at its default until pe-world holds the setting).
pub const MAX_HAND_PUSH_SPEED: f64 = 10.0;

/// Whether `hand`'s body is physical this frame.
pub fn is_physical(hand: &HandRow, holding: bool) -> bool {
    hand.mode == hand_mode::ALWAYS_WITHIN_REACH || hand.engaged || holding
}

/// Bring the hand bodies in line with the world model's hands, before a frame's step: a body for every physical
/// hand, driven towards its target, and no other body that the world model does not list as physical. `hands` is
/// the world model's [`WorldModel::hands`], in id order. Returns the number of refused physics calls (faults, 3.1).
pub fn sync<W: WorldModel + ?Sized>(
    world: &W,
    physics: &mut PhysicsWorld,
    hands: &[HandRow],
    origin: Pos,
    rate: SimRate,
    frame: Frame,
) -> u64 {
    let holding: Vec<ObjectId> = {
        let mut ids: Vec<ObjectId> = world.claims().iter().map(|c| c.hand).filter(|&h| h != ObjectId::NONE).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let physical_hands: Vec<&HandRow> =
        hands.iter().filter(|h| is_physical(h, holding.binary_search(&h.hand).is_ok())).collect();
    // A body the world model lists neither as a physical object nor as a physical hand: a hand that opened, changed
    // mode or is gone.
    let mut bodies = world.physical();
    bodies.sort_unstable();
    let stale: Vec<ObjectId> = physics
        .objects()
        .filter(|id| bodies.binary_search(id).is_err() && !physical_hands.iter().any(|h| h.hand == *id))
        .collect();
    let mut faults = 0;
    for id in stale {
        faults += u64::from(physics.remove_body(id).is_err());
    }
    for hand in physical_hands {
        faults += u64::from(drive(physics, hand, origin, rate, frame).is_err());
    }
    faults
}

/// One physical hand's body for `frame`: made at its target if it has none, else driven, coasting or holding.
fn drive(
    physics: &mut PhysicsWorld,
    hand: &HandRow,
    origin: Pos,
    rate: SimRate,
    frame: Frame,
) -> Result<(), PhysicsError> {
    let target = Pose { position: (hand.target - origin).0, rotation: hand.rotation };
    if !physics.has_body(hand.hand) {
        let shape = ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: HAND_RADIUS });
        let desc = BodyDesc::new(BodyKind::Kinematic, target.position).with_rotation(target.rotation).with_shape(shape);
        return physics.add_body(hand.hand, &desc);
    }
    let dt = 1.0 / rate.hz() as f32;
    let span_end = hand.stamp.0.saturating_add(u64::from(hand.span));
    let coast = frames_ceil(COAST, rate.hz());
    let streamed = hand
        .previous
        .is_some_and(|(stamp, span)| hand.stamp.0 <= stamp.0.saturating_add(u64::from(span)).saturating_add(coast));
    if frame.0 >= span_end && streamed && frame.0 < span_end.saturating_add(coast) {
        let (linear, angular) = physics.velocity(hand.hand)?;
        let speed = linear.iter().map(|v| v * v).sum::<f32>().sqrt();
        let scale = if speed > COAST_SPEED { COAST_SPEED / speed } else { 1.0 };
        return physics.set_velocity(hand.hand, linear.map(|v| v * scale), angular);
    }
    // Within the span, or after it and any coast: to the target. After a coast that is back to where the world's
    // hand is, so a body never parks away from the hand everyone sees (@design-reviewer's Low on 52575b48).
    let now = physics.pose(hand.hand)?.position;
    let distance = (0..3).map(|i| (target.position[i] - now[i]).powi(2)).sum::<f64>().sqrt();
    if distance > MAX_HAND_PUSH_SPEED * f64::from(dt) {
        physics.set_state(hand.hand, target, [0.0; 3], [0.0; 3])
    } else {
        physics.set_target(hand.hand, target, dt, true)
    }
}

/// Whether `id` is a hand's: the bodies [`sync`] made, which never write poses or `BodyState` rows back to the
/// world model. `hands` is in id order.
pub fn is_hand(hands: &[HandRow], id: ObjectId) -> bool {
    hands.binary_search_by_key(&id, |h| h.hand).is_ok()
}
