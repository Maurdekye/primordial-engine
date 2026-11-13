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
//!   physical, so its grab has a body to hold by; so an open hand that holds something still pushes what it touches,
//!   by design. A hand that is not physical has no body, so it passes through everything.
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
//! - **Grabs mirror the claims** (3.3; world-model.md 7.2), after the hands are driven: a claim by a hand with a body
//!   on an object with one gets a grab joint from the hand to the object, keyed from the object's id, which holds
//!   the object where it was in the hand when the joint was made, by a motor with a spring ([`GRAB`]); a grab joint
//!   whose claim has gone is removed. An agent's grab has no hand, so it gets no joint. The joints are in the physics
//!   image, so a rollback restores them with the bodies.

use std::time::Duration;

use pe_physics::{
    BodyDesc, BodyKind, Geometry, GrabSpring, JointDesc, JointKey, JointKind, PhysicsError, PhysicsWorld, Pose,
    ShapeDesc,
};
use pe_seams::Hasher;
use pe_seams::{Frame, ObjectId, Pos, SimRate, frames_ceil};

use crate::world::{ClaimRow, HAND_RADIUS, HandRow, WorldModel};

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

/// A grab's motor and spring: the joint drives the object towards the hand's motion with at most 1,000 N and
/// 100 N m, and springs it back to its place in the hand at 5 Hz, critically damped. M1's defaults, as pe-physics's
/// grab tests use them, until the user's verdict on hand feel at the headset test (inferred).
pub const GRAB: GrabSpring = GrabSpring { hertz: 5.0, damping_ratio: 1.0, max_force: 1000.0, max_torque: 100.0 };

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
    let claims = world.claims();
    let holding: Vec<ObjectId> = {
        let mut ids: Vec<ObjectId> = claims.iter().map(|c| c.hand).filter(|&h| h != ObjectId::NONE).collect();
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
    faults + grabs(physics, &claims, &bodies)
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

/// The key of the grab joint that holds `object`: one per object, as a claim is (world-model.md 7.2).
pub fn grab_key(object: ObjectId) -> JointKey {
    let mut h = Hasher::new("pe-replica.grab");
    h.write_u128(object.to_u128());
    let bytes = h.finish().0;
    JointKey(u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")))
}

/// The grab joints for `claims`, after the hands are driven: one for each claim by a hand with a body on an object
/// with one, and none other. `bodies` are the world model's physical objects, in id order: every object a grab
/// joint can hold, so probing their keys finds every grab joint (a removed body takes its joints with it).
fn grabs(physics: &mut PhysicsWorld, claims: &[ClaimRow], bodies: &[ObjectId]) -> u64 {
    let mut faults = 0;
    let held = |id: ObjectId| claims.iter().find(|c| c.object == id && c.hand != ObjectId::NONE).map(|c| c.hand);
    for &object in bodies {
        let key = grab_key(object);
        let joint = physics.joint_kind(key);
        let hand = held(object).filter(|&hand| physics.has_body(hand) && physics.has_body(object));
        match (joint, hand) {
            (Some(JointKind::Grab), None) => faults += u64::from(physics.remove_joint(key).is_err()),
            (None, Some(hand)) => faults += u64::from(hold(physics, key, hand, object).is_err()),
            _ => {}
        }
    }
    faults
}

/// A grab joint from `hand` to `object`, holding the object where it is now in the hand.
fn hold(physics: &mut PhysicsWorld, key: JointKey, hand: ObjectId, object: ObjectId) -> Result<(), PhysicsError> {
    let h = physics.pose(hand)?;
    let o = physics.pose(object)?;
    let offset = [o.position[0] - h.position[0], o.position[1] - h.position[1], o.position[2] - h.position[2]];
    let anchor_a = inverse_rotate(h.rotation, offset.map(|c| c as f32));
    let desc = JointDesc::Grab {
        a: Some(hand),
        b: object,
        anchor_a,
        anchor_b: [0.0; 3],
        max_force: GRAB.max_force,
        max_torque: GRAB.max_torque,
        spring: Some(GRAB),
    };
    physics.add_joint(key, &desc)
}

/// `v` turned by the inverse of the unit quaternion `q` (x, y, z, w): into `q`'s frame.
fn inverse_rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let u = [-q[0], -q[1], -q[2]];
    let w = q[3];
    let cross =
        |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let t = cross(u, v).map(|c| 2.0 * c);
    let ut = cross(u, t);
    [v[0] + w * t[0] + ut[0], v[1] + w * t[1] + ut[1], v[2] + w * t[2] + ut[2]]
}
