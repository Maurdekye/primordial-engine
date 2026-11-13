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
//!   meets at the speed it moves. The move is measured from where the body is to its target, for the hand and for
//!   everything it holds (each held object carried rigidly with the hand), and its farthest point decides
//!   (world-model.md 8.1's cap by `max_push_speed`; @coordinator-opus m9fa06577e5b1, option (b)):
//!   - **faster than [`MAX_HAND_PUSH_SPEED`]:** the body moves only that far towards the target this frame, its
//!     position and turn scaled alike, so neither it nor what it holds pushes faster than the cap. It trails its
//!     target and catches up when the hand slows, and a throw leaves at up to the cap;
//!   - **beyond the teleport distance** ([`teleport_distance`]: [`TELEPORT_DISTANCE`], or two frames at the cap at
//!     low rates), a tracking glitch, a real teleport, or a body trailing that far: the body is put at the target
//!     with no velocity, moved but not swept, and each object the hand's claims hold goes with it, keeping its place
//!     and rotation relative to the hand, with no velocity too (@design-reviewer's Low on cc037c10). So a body never
//!     trails its target by more than the teleport distance, and a hand kept faster than the cap trails, is placed
//!     and trails again: its contacts come in bursts. Only the claimed object moves: other bodies resting on it
//!     (cups on a held tray) stay where they were. A **trailing** placement (the body was slowed by the cap in the
//!     last frame, so it is catching up, not jumping) carries a held object only where no static geometry lies in its
//!     way ([`PhysicsWorld::static_in_way`]), so a held object never passes through a wall: one left behind stays
//!     where it is, held by its spring (@coordinator-opus m51d8feaf4aa5). A real teleport carries it through.
//! - **A hold's spring is bounded** ([`reaches`], world-model.md 8.1): for each step it sees at most the error the
//!   room under the cap leaves, `(max_push_speed - v_hold) / k`, so a held object kept from its place comes back no
//!   faster than the cap, and one that lags a hand at the cap catches up when the hand slows; the part of the error
//!   that holds the object up may always be [`HOLD_SUPPORT_MARGIN`] times the spring's sag. The bound moves the
//!   joint's anchor on the hand for the step only ([`PhysicsWorld::step_holding`]), so it is never in an image.
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
    BodyDesc, BodyKind, Geometry, GrabSpring, HoldReach, JointDesc, JointKey, JointKind, PhysicsError, PhysicsWorld,
    Pose, ShapeDesc,
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
/// The fastest a hand body, or anything it holds, moves when driven, m/s (world-model.md 8.1's `max_push_speed`, at
/// its default until pe-world holds the setting).
pub const MAX_HAND_PUSH_SPEED: f64 = 10.0;
/// M1's teleport distance, m: a hand's move longer than this in one frame is placed, not swept (@coordinator-opus
/// m9fa06577e5b1).
pub const TELEPORT_DISTANCE: f64 = 0.5;

/// The teleport distance at `rate`: [`TELEPORT_DISTANCE`], or two frames at the cap where that is longer (below
/// 40 Hz), so a move between one frame at the cap and a teleport always exists to be slowed (@design-reviewer's Low
/// on world/hand-teleport 0913f962).
pub fn teleport_distance(rate: SimRate) -> f64 {
    TELEPORT_DISTANCE.max(2.0 * MAX_HAND_PUSH_SPEED / f64::from(rate.hz()))
}

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

/// The grab spring's angular frequency, 1/s: `2 pi` times [`GRAB`]'s hertz. Its static sag under gravity `g` is
/// `g / OMEGA^2` (measured: 9.9 mm at 9.81 m/s^2, at 30, 60 and 120 Hz).
pub const OMEGA: f64 = 2.0 * std::f64::consts::PI * GRAB.hertz as f64;
/// How many times its static sag a hold's spring may see of the error that holds its object up, whatever the hand's
/// speed (world-model.md 8.1's `s_min`): 3, so the spring can lift twice the object's weight beyond holding it
/// (@design-reviewer m3193baf04827). Measured with it: a held ball stays 1.0 cm below its place while the hand
/// overruns the cap sideways for 2 s, and moves at most 0.24 m/s over the cap with the hand overrunning upwards.
pub const HOLD_SUPPORT_MARGIN: f64 = 3.0;

/// Whether `hand`'s body is physical this frame.
pub fn is_physical(hand: &HandRow, holding: bool) -> bool {
    hand.mode == hand_mode::ALWAYS_WITHIN_REACH || hand.engaged || holding
}

/// Bring the hand bodies in line with the world model's hands, before a frame's step: a body for every physical
/// hand, driven towards its target, and no other body that the world model does not list as physical. `hands` is
/// the world model's [`WorldModel::hands`], in id order. Returns the number of refused physics calls (faults, 3.1),
/// and each grab's reach for the step ([`reaches`]), for [`PhysicsWorld::step_holding`].
pub fn sync<W: WorldModel + ?Sized>(
    world: &W,
    physics: &mut PhysicsWorld,
    hands: &[HandRow],
    origin: Pos,
    rate: SimRate,
    frame: Frame,
) -> (u64, Vec<(JointKey, HoldReach)>) {
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
    let mut faults: u64 = 0;
    for id in stale {
        faults += u64::from(physics.remove_body(id).is_err());
    }
    for hand in physical_hands {
        let held: Vec<ObjectId> =
            claims.iter().filter(|c| c.hand == hand.hand && physics.has_body(c.object)).map(|c| c.object).collect();
        faults += u64::from(drive(physics, hand, &held, origin, rate, frame).is_err());
    }
    faults += grabs(physics, &claims, &bodies);
    (faults, reaches(physics, &claims, 1.0 / rate.hz() as f32, world.settings().gravity))
}

/// How far from its place each held object's spring sees it in this frame's step, m, for
/// [`PhysicsWorld::step_holding`]: world-model.md 8.1's hold bound (@coordinator-opus m51d8feaf4aa5, md1fd92002792,
/// me854800b7b54). The spring asks for a speed of about [`PhysicsWorld::grab_spring_rate`] (`k`) times the error it
/// sees, relative to the hand, so the error is bounded by the room the hand's own speed at the hold (`v_hold`, its
/// turn included) leaves under [`MAX_HAND_PUSH_SPEED`]: `(max_push_speed - v_hold) / k`. A held object kept from its
/// place (pinned by a wall) comes back no faster than the cap however far it was kept, and one that lags a hand moving
/// at the cap catches up only once the hand slows. The part of the error that holds the object up against gravity may
/// be at least [`HOLD_SUPPORT_MARGIN`] times the spring's static sag, `g / OMEGA^2`, so a held object never sags out
/// of the hand; in a world without gravity that floor is 0. Every grab joint of `claims`, keyed once.
pub fn reaches(physics: &PhysicsWorld, claims: &[ClaimRow], dt: f32, gravity: [f32; 3]) -> Vec<(JointKey, HoldReach)> {
    let per_metre = f64::from(physics.grab_spring_rate(GRAB, dt));
    let g = f64::from((gravity[0] * gravity[0] + gravity[1] * gravity[1] + gravity[2] * gravity[2]).sqrt());
    let support = HOLD_SUPPORT_MARGIN * g / (OMEGA * OMEGA);
    let mut out: Vec<(JointKey, HoldReach)> = Vec::with_capacity(claims.len());
    for c in claims {
        let key = grab_key(c.object);
        if physics.joint_kind(key) != Some(JointKind::Grab) || out.iter().any(|&(k, _)| k == key) {
            continue;
        }
        let speed = match (physics.velocity(c.hand), physics.pose(c.hand), physics.grab_anchor(key)) {
            (Ok((v, w)), Ok(hand), Ok(anchor)) => point_speed(v, w, rotate(hand.rotation, anchor)),
            _ => 0.0,
        };
        let reach = (MAX_HAND_PUSH_SPEED - speed).max(0.0) / per_metre;
        out.push((key, HoldReach { reach: reach as f32, against_gravity: reach.max(support) as f32 }));
    }
    out
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
    let mut carried = Vec::with_capacity(held.len());
    for &object in held {
        let at = physics.pose(object)?;
        carried.push((object, at, carry(&now, &target, &at)));
    }
    // The move's farthest point: the hand's own, or that of something it holds.
    let farthest = carried
        .iter()
        .map(|(_, at, to)| distance(at.position, to.position))
        .fold(distance(now.position, target.position), f64::max);
    if farthest > teleport_distance(rate) {
        // A body slowed by the cap in the last frame trails its target, so this is a trailing placement: it carries a
        // held object only where no static geometry lies in its way, so a held object never passes through a wall
        // (@coordinator-opus m51d8feaf4aa5); one left behind stays where it is, held by its spring. A body that was
        // not slowed reached its target, so the move is the target's own jump, a teleport, which carries what the
        // hand holds through whatever lies between (world-model.md 8.1).
        let trailing = was_slowed(physics, hand.hand, &now, &carried)?;
        // Placed, not swept, with no velocity: a body placed into others while moving would hit them at its speed
        // plus the push-out (measured: 14.8 m/s at a 10 m/s cap when velocities were kept).
        physics.set_state(hand.hand, target, [0.0; 3], [0.0; 3])?;
        for (object, _, to) in carried {
            if trailing && physics.static_in_way(object, &to)? {
                continue;
            }
            physics.set_state(object, to, [0.0; 3], [0.0; 3])?;
        }
        return Ok(());
    }
    let cap = MAX_HAND_PUSH_SPEED * f64::from(dt);
    let to = if farthest > cap { towards(&now, &target, cap / farthest) } else { target };
    physics.set_target(hand.hand, to, dt, true)
}

/// Whether the hand's body moved at the cap in the last frame, at itself or at a held object (`carried`: each held
/// object, where it is, where the hand would carry it): then it was slowed, and trails its target. A kinematic body
/// keeps the velocity its last move gave it until the next, so this reads only the physics state, which a rollback
/// restores. A margin of 0.1 % keeps a move at exactly the cap counted through rounding.
fn was_slowed(
    physics: &PhysicsWorld,
    hand: ObjectId,
    now: &Pose,
    carried: &[(ObjectId, Pose, Pose)],
) -> Result<bool, PhysicsError> {
    let (v, w) = physics.velocity(hand)?;
    let limit = 0.999 * MAX_HAND_PUSH_SPEED;
    let fast = |r: [f32; 3]| point_speed(v, w, r) >= limit;
    Ok(fast([0.0; 3])
        || carried.iter().any(|(_, at, _)| fast([0usize, 1, 2].map(|i| (at.position[i] - now.position[i]) as f32))))
}

/// The speed of the point `r` from a body's origin, for the body's linear and angular velocity `v` and `w`.
fn point_speed(v: [f32; 3], w: [f32; 3], r: [f32; 3]) -> f64 {
    let at = [v[0] + w[1] * r[2] - w[2] * r[1], v[1] + w[2] * r[0] - w[0] * r[2], v[2] + w[0] * r[1] - w[1] * r[0]];
    f64::from((at[0] * at[0] + at[1] * at[1] + at[2] * at[2]).sqrt())
}

/// The pose a fraction `f` of the way from `from` to `to`: the position along the straight line, the rotation along
/// the straight line between the two quaternions (the shorter way round), normalized. It needs no transcendental
/// function, so every machine computes the same bits.
fn towards(from: &Pose, to: &Pose, f: f64) -> Pose {
    let a = from.rotation;
    let mut b = to.rotation;
    if a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3] < 0.0 {
        b = b.map(|c| -c);
    }
    let g = f as f32;
    Pose {
        position: [0usize, 1, 2].map(|i| from.position[i] + f * (to.position[i] - from.position[i])),
        rotation: normalized([0usize, 1, 2, 3].map(|i| a[i] + g * (b[i] - a[i]))),
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
