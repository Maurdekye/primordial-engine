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
//! - **What a hand holds jumps with it** (@design-reviewer's Low on cc037c10): on a jump, each object the hand's
//!   claims hold is put where the hand's move takes it, keeping its place and rotation relative to the hand, with no
//!   velocity, so it is moved but not swept too. A move is also a jump when it would take a held object faster than
//!   the cap, as a fast turn does to an object held away from the hand's centre. Only the claimed object moves:
//!   other bodies resting on it (cups on a held tray) stay where they were. A throw released while its hand jumps
//!   (moves faster than the cap) leaves with no velocity.
//! - **After the span, with no newer input:** if the input came as part of a stream (within its predecessor's span
//!   plus [`COAST`]), the body coasts at its last velocity, at most [`COAST_SPEED`], until [`COAST`] after the span;
//!   then it goes back to the hand's target by the same rule as within a span, and stays there, so it never parks
//!   away from the hand everyone sees. Only the frames in the hand's row decide, so a re-simulation repeats them
//!   (3.3, D52).
//! - **Grabs mirror the claims** (3.3; world-model.md 7.2), after the hands are driven: a claim by a hand with a body
//!   on an object with one gets a grab joint from the hand to the object, keyed from the object's id, which holds
//!   the object where it was in the hand when the joint was made: a spring pulls it to that place ([`GRAB`]), and an
//!   angular drive keeps its rotation relative to the hand ([`GRAB_TORQUE`]). A grab joint whose claim has gone is
//!   removed, and one whose claim a renewing grab moved to another of its author's hands is made again from that
//!   hand, holding the object where it is (@design-reviewer's Medium on cc037c10). An agent's grab has no hand, so
//!   it gets no joint. The joints are in the physics image, so a rollback restores them with the bodies.

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

/// A grab's spring: it pulls the held object to its place in the hand at 5 Hz, critically damped, with at most
/// 1,000 N. It has no angular part (`max_torque` 0): pe-physics's grab joint has no frame rotation, so an angular
/// spring would turn the object to the hand's own rotation instead of keeping the one it was grabbed at. The joint's
/// linear velocity drive is off: it is solved after the spring and is stiff, so it cancelled every velocity the
/// spring gave, and a held object only copied the hand's velocity, left wherever a jump or a knock put it (measured
/// on cc037c10). M1's defaults until the user's verdict on hand feel at the headset test (inferred).
pub const GRAB: GrabSpring = GrabSpring { hertz: 5.0, damping_ratio: 1.0, max_force: 1000.0, max_torque: 0.0 };
/// A grab's angular drive, N m: it matches the held object's turning to the hand's, so the object keeps the rotation
/// it was grabbed at relative to the hand.
pub const GRAB_TORQUE: f32 = 100.0;

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
        let held: Vec<ObjectId> =
            claims.iter().filter(|c| c.hand == hand.hand && physics.has_body(c.object)).map(|c| c.object).collect();
        faults += u64::from(drive(physics, hand, &held, origin, rate, frame).is_err());
    }
    faults + grabs(physics, &claims, &bodies)
}

/// One physical hand's body for `frame`: made at its target if it has none, else driven, coasting or holding.
/// `held` are the objects its claims hold, in id order, which a jump carries with it.
fn drive(
    physics: &mut PhysicsWorld,
    hand: &HandRow,
    held: &[ObjectId],
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
    let now = physics.pose(hand.hand)?;
    let cap = MAX_HAND_PUSH_SPEED * f64::from(dt);
    let mut carried = Vec::with_capacity(held.len());
    for &object in held {
        let at = physics.pose(object)?;
        carried.push((object, at, carry(&now, &target, &at)));
    }
    // A jump: the hand, or something it holds, would move faster than the cap (@design-reviewer's Low on cc037c10).
    let jump = distance(now.position, target.position) > cap
        || carried.iter().any(|(_, at, to)| distance(at.position, to.position) > cap);
    if !jump {
        return physics.set_target(hand.hand, target, dt, true);
    }
    physics.set_state(hand.hand, target, [0.0; 3], [0.0; 3])?;
    for (object, _, to) in carried {
        physics.set_state(object, to, [0.0; 3], [0.0; 3])?;
    }
    Ok(())
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
            // A renewing grab that names another of its author's hands moves the claim to it (world-model.md 7.2), so
            // the joint is made again from that hand, holding the object where it is (@design-reviewer's Medium on
            // cc037c10).
            (Some(JointKind::Grab), Some(hand)) if physics.joint_objects(key).map(|(a, _)| a) != Some(Some(hand)) => {
                faults += u64::from(physics.remove_joint(key).is_err());
                faults += u64::from(hold(physics, key, hand, object).is_err());
            }
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
        max_force: 0.0,
        max_torque: GRAB_TORQUE,
        spring: Some(GRAB),
    };
    physics.add_joint(key, &desc)
}

/// Where `p` goes when what holds it moves rigidly from `from` to `to`: its place and rotation relative to `from`,
/// kept relative to `to`.
fn carry(from: &Pose, to: &Pose, p: &Pose) -> Pose {
    let offset = [p.position[0] - from.position[0], p.position[1] - from.position[1], p.position[2] - from.position[2]];
    let moved = rotate(to.rotation, inverse_rotate(from.rotation, offset.map(|c| c as f32)));
    Pose {
        position: [0usize, 1, 2].map(|i| to.position[i] + f64::from(moved[i])),
        rotation: normalized(mul(to.rotation, mul(conjugate(from.rotation), p.rotation))),
    }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f64>().sqrt()
}

/// `v` turned by the unit quaternion `q` (x, y, z, w).
fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let u = [q[0], q[1], q[2]];
    let w = q[3];
    let cross =
        |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let t = cross(u, v).map(|c| 2.0 * c);
    let ut = cross(u, t);
    [v[0] + w * t[0] + ut[0], v[1] + w * t[1] + ut[1], v[2] + w * t[2] + ut[2]]
}

/// `v` turned by the inverse of the unit quaternion `q`: into `q`'s frame.
fn inverse_rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    rotate(conjugate(q), v)
}

fn conjugate(q: [f32; 4]) -> [f32; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

/// The quaternion product `a b` (x, y, z, w): `b`'s turn, then `a`'s.
fn mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

/// `q` at unit length, as Box3D's exact setters take it.
fn normalized(q: [f32; 4]) -> [f32; 4] {
    let n = q.iter().map(|c| c * c).sum::<f32>().sqrt();
    q.map(|c| c / n)
}
