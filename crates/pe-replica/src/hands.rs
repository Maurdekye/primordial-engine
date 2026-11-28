//! Hand bodies: each member's hands as pe-physics bodies that push what they touch (replica.md 3.3; world-model.md
//! 8.1; D18b).
//!
//! A hand object has no `Body` in the world model: its body is the replica's own, a kinematic sphere of
//! [`HAND_RADIUS`] under the hand object's id, driven towards the target its inputs write. Like everything the
//! replica does, it is a function of the world model's rows and the physics state, applied in id order inside each
//! frame's step, and nothing about it is kept outside the two. A rollback or a replay restores the physics image,
//! hand bodies included, and the next frames rebuild the rest, so they repeat exactly (8.1).
//!
//! - **When a hand is physical** is its mode (D18b, per member; @coordinator-opus):
//!   "closed hand" ([`hand_mode::CLOSED_HAND`], the default) while the hand is closed (its input's `engaged`, the
//!   grasp); "always within reach" ([`hand_mode::ALWAYS_WITHIN_REACH`]) always. A hand that holds something stays
//!   physical, so its grab has a body to hold by; so an open hand that holds something still pushes what it touches,
//!   by design. A hand that is not physical has no body, so it passes through everything.
//! - **A body that appears** (a hand closes, or its mode flips to always) is placed at its target, not swept there,
//!   and overlapping bodies are pushed out by Box3D's soft contacts at the world's contact speed (3 m/s), so a flip
//!   inside a pile moves its balls apart gently instead of throwing them (8.1).
//! - **Within an input's span** the body is driven to the target in one frame: `set_target`, so it pushes what it
//!   meets at the speed it moves. **A hand that holds something has no speed cap** (the user's ruling of 2026-11-26: "remove any maximum throw speed (for v1); the grip strength should be what governs maximum throw
//!   speed"): it reaches its target every frame, and its teleport test is a jump away from its own motion
//!   ([`jump_from_motion`]), so a fast swing is never mistaken for a teleport. **An empty hand** keeps the cap: the
//!   move is measured from where the body is to its target (world-model.md 8.1's cap by `max_push_speed`;
//!   @coordinator-opus, option (b)):
//!   - **faster than [`MAX_HAND_PUSH_SPEED`]:** the body moves only that far towards the target this frame, its
//!     position and turn scaled alike, so it pushes no faster than the cap. It trails its target and catches up when
//!     the hand slows;
//!   - **beyond the teleport distance** ([`teleport_distance`]: [`TELEPORT_DISTANCE`], or two frames at the cap at
//!     low rates; for a holding hand, the jump from its motion), a tracking glitch, a real teleport, or a body
//!     trailing that far: the body is put at the target
//!     with no velocity, moved but not swept, and each object the hand's claims hold goes with it, keeping its
//!     rotation relative to the hand, with no velocity too (@design-reviewer's Low on cc037c10): a light hold, whose
//!     force the object's mass caps ([`follows_tightly`]), to its place in the hand, and a heavy one keeping its
//!     offset, so a placement never lifts what the hand's strength can't ([`placed`]; @design-reviewer's L1 on
//!     980e6ba7, @coordinator-opus). So a body never trails its target by more than the teleport
//!     distance, and a hand kept faster than the cap trails, is placed and trails again: its contacts come in bursts,
//!     and a light held object never trails it by more than a cycle's lag. Only the claimed object moves: other bodies
//!     resting on it (cups on a held tray) stay where they were. A **trailing** placement (the body was slowed by the
//!     cap in the last frame, so it is catching up, not jumping) carries a held object only where no static geometry
//!     lies in its way ([`PhysicsWorld::static_in_way`]), so a held object never passes through a wall: one left
//!     behind stays where it is, held by its spring (@coordinator-opus). A real teleport carries it
//!     through.
//! - **A hold is tight, and its force is the grip's** ([`grab_spring`], world-model.md 8.1; the user's ruling of
//!   2026-11-26, option (c)): a stiff spring ([`HOLD_HZ`]) keeps a light held object within
//!   millimetres of its place in the hand through the fastest swing or twist, and its force is at most the holder's
//!   strength (its `Hand` row's), so a heavy object lags. Nothing else limits it, so a hold presses a light object
//!   into what blocks it with the whole grip, and one held against the floor or a wall can sink into it or go through
//!   (accepted for the 11-27 kit).
//! - **A hold's spring is bounded** ([`reaches`], world-model.md 8.1): for each step it sees at most the error its
//!   force can brake from, so a heavy object settles at its place instead of overshooting; the part of the error that
//!   holds the object up keeps up to the hold's [`support`]. The bound moves the joint's anchor on the hand for
//!   the step only ([`PhysicsWorld::step_holding`]), so it is never in an image.
//! - **After the span, with no newer input:** if the input came as part of a stream (within its predecessor's span
//!   plus [`COAST`]), the body coasts at its last velocity, at most [`COAST_SPEED`], until [`COAST`] after the span;
//!   then it goes back to the hand's target by the same rule as within a span, and stays there, so it never parks
//!   away from the hand everyone sees. Only the frames in the hand's row decide, so a re-simulation repeats them
//!   (3.3, D52).
//! - **Grabs mirror the claims** (3.3; world-model.md 7.2), after the hands are driven: a claim by a hand with a body
//!   on an object with one gets a grab joint from the hand to the object, keyed from the object's id, which holds
//!   the object where it was in the hand when the joint was made: a spring pulls it to that place ([`grab_spring`]),
//!   and an angular drive keeps its rotation relative to the hand ([`GRAB_TORQUE`]). A grab joint whose claim has gone
//!   is removed, and one whose claim a renewing grab moved to another of its author's hands is made again from that
//!   hand, holding the object where it is (@design-reviewer's Medium on cc037c10). An agent's grab has no hand, so
//!   it gets no joint. The joints are in the physics image, so a rollback restores them with the bodies.

use std::time::Duration;

use pe_physics::{
    BodyDesc, BodyKind, Geometry, GrabSpring, HoldReach, JointDesc, JointKey, JointKind, PhysicsError, PhysicsWorld,
    Pose, ShapeDesc,
};
use pe_seams::Hasher;
use pe_seams::{Frame, ObjectId, Pos, SimRate, frames_ceil};

use crate::world::{ClaimRow, HAND_RADIUS, HandRow, ThrowFx, WorldModel};

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
/// M1's teleport distance, m: a hand's move longer than this in one frame is placed, not swept (@coordinator-opus).
pub const TELEPORT_DISTANCE: f64 = 0.5;

/// The teleport distance at `rate`: [`TELEPORT_DISTANCE`], or two frames at the cap where that is longer (below
/// 40 Hz), so a move between one frame at the cap and a teleport always exists to be slowed (@design-reviewer's Low
/// on world/hand-teleport 0913f962).
pub fn teleport_distance(rate: SimRate) -> f64 {
    TELEPORT_DISTANCE.max(2.0 * MAX_HAND_PUSH_SPEED / f64::from(rate.hz()))
}

/// A hold's spring stiffness, Hz: as stiff as stability allows (the user's rulings of 2026-11-19: "an
/// object that's light enough should keep it tightly constrained to your hand's position"; @coordinator-opus). It is Box3D's own stiffness for rigid joints at the engine's 1/240 s sub-step
/// (box3d-fork joint/solve.rs clamps a joint's hertz to a quarter of the sub-step rate, "to reduce jitter"). Measured
/// (pe-physics examples/hold_sweep.rs, an 18 g ball at world rates 30, 60 and 120 Hz): no jitter, overshoot or energy
/// gain at any stiffness up to 960 Hz; it sags `g / OMEGA^2` (0.07 mm), and trails a hand speeding up at 100 m/s^2 by
/// 3.6, 1.7 and 0.9 mm, where M1's 5 Hz trailed by 94 mm.
pub const HOLD_HZ: f32 = 60.0;

/// The acceleration that makes a held object light, m/s^2 (A): an object of mass `m` whose `m (A + g)` is at most the
/// holder's strength follows the hand tightly ([`follows_tightly`]), so a placement carries it to its place in the
/// hand ([`placed`]), and the hold's [`support`] is the error at which the spring's pull on it reaches `m (A + g)`.
/// Until the user's ruling of 2026-11-26 (option (c)) it also capped every hold's force at `m (A + g)`;
/// a hold's force is now the grip's alone ([`grab_spring`]). That cap held a light object to 1.5 mm in the floor and
/// never through a wall, and kept an 18 g ball from following any arm swing faster than 12 m/s or a wrist twist past
/// about 40 rad/s (measured on 2026-11-26).
pub const HOLD_ACCELERATION: f32 = 200.0;

/// The largest strength a hand's row gives, N: a strength outside `0..=MAX_STRENGTH` is clamped into it before the
/// physics sees it.
pub const MAX_STRENGTH: f32 = 10_000.0;

/// The grab spring of a hold by a hand of `strength` N (world-model.md 8.1): it pulls the held object to its place in the hand at [`HOLD_HZ`], critically damped, with at
/// most the holder's strength. Nothing else limits it (the user's ruling of 2026-11-26, option (c):
/// "the grip strength should be what governs"; until then also `mass (A + g)`, [`HOLD_ACCELERATION`]). So a light
/// object follows the fastest swing or twist of a hand within millimetres, and a heavier one lags once the hand asks
/// for more than the strength. It also means a hold presses a light object into what blocks it with the whole strength,
/// so a held object pushed against the floor or a wall can sink into it or pass through (measured in
/// `measure_push_through_at_the_grip_s_strength`). It has no
/// angular part (`max_torque` 0): pe-physics's grab joint has no frame rotation, so an angular spring would turn the
/// object to the hand's own rotation instead of keeping the one it was grabbed at. The joint's linear velocity drive
/// stays off: it is solved after the spring and is stiff, so it cancelled every velocity the spring gave, and a held
/// object only copied the hand's velocity, left wherever a jump or a knock put it (measured on cc037c10). [`hold`]
/// takes the force when it makes the joint.
pub fn grab_spring(strength: f32) -> GrabSpring {
    GrabSpring { hertz: HOLD_HZ, damping_ratio: 1.0, max_force: strength_of(strength), max_torque: 0.0 }
}

/// Whether a hand of `strength` N holds an object of `mass` kg tightly, under gravity of magnitude `g`: when its hold's
/// force cap comes from the object's mass, `mass (A + g)` at most the strength ([`grab_spring`]), so it follows a hand
/// speeding up at up to [`HOLD_ACCELERATION`]. A placement carries such a hold to its place in the hand ([`placed`]).
pub fn follows_tightly(strength: f32, mass: f32, g: f32) -> bool {
    follow(mass, g) <= strength_of(strength)
}

/// A hand row's strength as the physics takes it: clamped to `0..=MAX_STRENGTH`, NaN as 0.
fn strength_of(strength: f32) -> f32 {
    if strength.is_nan() { 0.0 } else { strength.clamp(0.0, MAX_STRENGTH) }
}

/// The force that makes an object of `mass` follow a hand speeding up at [`HOLD_ACCELERATION`] against gravity `g`.
fn follow(mass: f32, g: f32) -> f32 {
    mass.max(0.0) * (HOLD_ACCELERATION + g.max(0.0))
}

/// A grab's angular drive, N m: it matches the held object's turning to the hand's, so the object keeps the rotation
/// it was grabbed at relative to the hand.
pub const GRAB_TORQUE: f32 = 100.0;

/// The grab spring's angular frequency, 1/s: `2 pi` times [`HOLD_HZ`]. Its static sag under gravity `g` is
/// `g / OMEGA^2` (0.069 mm at 9.81 m/s^2; measured 0.07 mm at 30, 60 and 120 Hz).
pub const OMEGA: f64 = 2.0 * std::f64::consts::PI * HOLD_HZ as f64;
/// The support of a hold under gravity of magnitude `g`, m (world-model.md 8.1's `s_min`): the most of the error that
/// holds its object up the spring keeps seeing, whatever the hand's speed, `(A + g) / OMEGA^2`, the error at which the
/// spring's pull on its object reaches the hold's force cap `mass (A + g)` ([`grab_spring`]; 1.48 mm at 9.81 m/s^2).
/// So the support may use the hold's whole force to keep its object up when catching up takes the rest. It was 3
/// times the static sag, `3 g / OMEGA^2`, when the hold's force had no cap but 1,000 N (@design-reviewer); with the cap that floor starved the support while a hold sped its object back up after each
/// placement of a hand overrunning the cap, and the ball sank 4.3 cm below its place in 2 s, against 0.4 cm with this
/// one. Its excess over the cap is at most `s_min k`, 0.16 m/s at 60 Hz. In a world without gravity there is none.
pub fn support(g: f64) -> f64 {
    if g > 0.0 { (f64::from(HOLD_ACCELERATION) + g) / (OMEGA * OMEGA) } else { 0.0 }
}

/// Whether `hand`'s body is physical this frame.
pub fn is_physical(hand: &HandRow, holding: bool) -> bool {
    hand.mode == hand_mode::ALWAYS_WITHIN_REACH || hand.engaged || holding
}

/// What [`sync`] keeps from step to step to skip the passes that provably change nothing (E4, re-solve step 2;
/// diverge-eng/resolve-e4-design.md): the stale-body pass only when its inputs changed, and the grab pass only over
/// claimed objects and the objects of existing grab joints. Either way the same calls on the physics, in the same order,
/// so the same faults and the same state. Off ([`Passes::full`]), every step runs both passes over every body.
#[derive(Clone, Debug, Default)]
pub struct Passes {
    /// Whether the passes run on what is active.
    pub active: bool,
    /// Whether each active pass is checked against the full one beside it (tests): a disagreement adds to `misses`.
    pub check: bool,
    /// The disagreements [`Passes::check`] found: each stale body the full pass would remove that the active one
    /// skipped, each grab the full pass would change outside the active set, each grab joint not under its object's
    /// [`grab_key`], and each wrong cached physical list.
    pub misses: u64,
    /// The steps whose stale-body pass was skipped, its inputs unchanged.
    pub skips: u64,
    /// The world model's physical objects, sorted, and the [`WorldModel::physical_key`] they were read at.
    physical: Option<([u64; 4], Vec<ObjectId>)>,
    /// The inputs of the latest stale-body pass that found nothing to remove: the world model's key, the physics'
    /// [`PhysicsWorld::bodies_gen`] and the physical hands. While they are the same, the pass finds nothing again: the
    /// stale bodies are the bound bodies outside the physical objects, the physical hands and the players, and the first
    /// two are functions of the key, the bound bodies and the players of the generation. A pass that removed a body or
    /// was refused is never kept (its next run must retry, as the full pass does). Restores need no clearing: a
    /// restored world model's key is the one its rows had (keys never repeat across states), and every physics image
    /// restore takes a new generation.
    clean: Option<([u64; 4], u64, Vec<ObjectId>)>,
}

impl Passes {
    /// Both passes over every body, every step.
    pub fn full() -> Passes {
        Passes::default()
    }

    /// The passes over what is active, and with `check`, the full ones beside them.
    pub fn active(check: bool) -> Passes {
        Passes { active: true, check, ..Passes::default() }
    }
}

/// Bring the hand bodies in line with the world model's hands, before a frame's step: a body for every physical
/// hand, driven towards its target, and no other body that the world model does not list as physical. `hands` is
/// the world model's [`WorldModel::hands`], in id order. Returns the number of refused physics calls (faults, 3.1),
/// and each grab's reach for the step ([`reaches`]), for [`PhysicsWorld::step_holding`]. With `grab_ns`, adds the
/// time the grab joints' pass took (the replay costs, crate::costs). `passes` says whether the passes run over every
/// body or over what is active, and keeps what the latter reuses.
#[allow(clippy::too_many_arguments)]
pub fn sync<W: WorldModel + ?Sized>(
    world: &W,
    physics: &mut PhysicsWorld,
    hands: &[HandRow],
    origin: Pos,
    rate: SimRate,
    frame: Frame,
    grab_ns: Option<&mut u64>,
    passes: &mut Passes,
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
    // mode or is gone. Players are crate::players's, synced before the hands.
    let Some(key) = passes.active.then(|| world.physical_key()).flatten() else {
        (passes.physical, passes.clean) = (None, None);
        let mut bodies = world.physical();
        bodies.sort_unstable();
        return sync_with(
            world,
            physics,
            hands,
            &claims,
            &physical_hands,
            &bodies,
            None,
            origin,
            rate,
            frame,
            grab_ns,
            passes,
        );
    };
    match &passes.physical {
        Some((k, kept)) if *k == key => {
            if passes.check {
                let mut fresh = world.physical();
                fresh.sort_unstable();
                passes.misses += u64::from(*kept != fresh);
            }
        }
        _ => {
            let mut bodies = world.physical();
            bodies.sort_unstable();
            passes.physical = Some((key, bodies));
        }
    }
    let bodies = std::mem::take(&mut passes.physical.as_mut().expect("kept above").1);
    let out = sync_with(
        world,
        physics,
        hands,
        &claims,
        &physical_hands,
        &bodies,
        Some(key),
        origin,
        rate,
        frame,
        grab_ns,
        passes,
    );
    if let Some((_, kept)) = passes.physical.as_mut() {
        *kept = bodies;
    }
    out
}

/// [`sync`] on `bodies`, the world model's physical objects, sorted, read at `key` (`None`: no key, no reuse).
#[allow(clippy::too_many_arguments)]
fn sync_with<W: WorldModel + ?Sized>(
    world: &W,
    physics: &mut PhysicsWorld,
    hands: &[HandRow],
    claims: &[ClaimRow],
    physical_hands: &[&HandRow],
    bodies: &[ObjectId],
    key: Option<[u64; 4]>,
    origin: Pos,
    rate: SimRate,
    frame: Frame,
    grab_ns: Option<&mut u64>,
    passes: &mut Passes,
) -> (u64, Vec<(JointKey, HoldReach)>) {
    let stale_of = |physics: &PhysicsWorld| -> Vec<ObjectId> {
        physics
            .objects()
            .filter(|id| {
                bodies.binary_search(id).is_err()
                    && !physical_hands.iter().any(|h| h.hand == *id)
                    && !physics.is_player(*id)
            })
            .collect()
    };
    let mut hand_ids: Vec<ObjectId> = physical_hands.iter().map(|h| h.hand).collect();
    hand_ids.sort_unstable();
    let inputs = key.map(|k| (k, physics.bodies_gen(), hand_ids));
    let mut faults: u64 = 0;
    if inputs.is_some() && passes.clean == inputs && !physics.is_poisoned() {
        passes.skips += 1;
        if passes.check {
            passes.misses += stale_of(physics).len() as u64;
        }
    } else {
        let stale = stale_of(physics);
        let removed = !stale.is_empty();
        for id in stale {
            faults += u64::from(physics.remove_body(id).is_err());
        }
        // Kept only after a pass that found nothing: a removal takes a new generation anyway, and a refused one must
        // be retried next step, as the full pass would.
        passes.clean = if removed || faults > 0 { None } else { inputs };
    }
    let gravity = world.settings().gravity;
    let g = (gravity[0] * gravity[0] + gravity[1] * gravity[1] + gravity[2] * gravity[2]).sqrt();
    for &hand in physical_hands {
        let held: Vec<ObjectId> =
            claims.iter().filter(|c| c.hand == hand.hand && physics.has_body(c.object)).map(|c| c.object).collect();
        faults += u64::from(drive(physics, hand, &held, origin, rate, frame, g).is_err());
    }
    let t = grab_ns.is_some().then(std::time::Instant::now);
    if passes.active && !physics.is_poisoned() {
        let held = held_objects(physics, claims, bodies);
        if passes.check {
            passes.misses += grab_misses(physics, claims, bodies, &held);
        }
        faults += grabs(physics, claims, &held, hands);
        debug_assert_eq!(passes.misses, 0, "the hand sync's active passes disagree with the full ones at {frame:?}");
    } else {
        faults += grabs(physics, claims, bodies, hands);
    }
    if let (Some(ns), Some(t)) = (grab_ns, t) {
        *ns += crate::costs::since(t);
    }
    (faults, reaches(physics, claims, 1.0 / rate.hz() as f32, gravity))
}

/// The objects [`grabs`] can act on, sorted: of `bodies`, those a claim names with a hand, and those with a grab joint
/// (each joint's `b`, so a key that is not its object's [`grab_key`] is still found, and checked as the full pass
/// checks it). Every arm of `grabs` needs a grab joint under the object's key or a claim with a hand, so `grabs` over
/// these makes the same calls, in the same order, as over every body (@design-reviewer).
fn held_objects(physics: &PhysicsWorld, claims: &[ClaimRow], bodies: &[ObjectId]) -> Vec<ObjectId> {
    let joined = physics.grab_joints().filter_map(|k| physics.joint_objects(k).and_then(|(_, b)| b));
    let mut ids: Vec<ObjectId> = claims
        .iter()
        .filter(|c| c.hand != ObjectId::NONE)
        .map(|c| c.object)
        .chain(joined)
        .filter(|id| bodies.binary_search(id).is_ok())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The check beside the grab pass over `held` ([`Passes::check`]): each object of `bodies` outside `held` that the full
/// pass would act on, and each grab joint not under its object's [`grab_key`] (only [`hold`] makes grab joints, always
/// under that key).
fn grab_misses(physics: &PhysicsWorld, claims: &[ClaimRow], bodies: &[ObjectId], held: &[ObjectId]) -> u64 {
    let hand_of = |id: ObjectId| claims.iter().find(|c| c.object == id && c.hand != ObjectId::NONE).map(|c| c.hand);
    let acts = |object: ObjectId| {
        let key = grab_key(object);
        let hand = hand_of(object).filter(|&hand| physics.has_body(hand) && physics.has_body(object));
        match (physics.joint_kind(key), hand) {
            (Some(JointKind::Grab), None) | (None, Some(_)) => true,
            (Some(JointKind::Grab), Some(hand)) => physics.joint_objects(key).map(|(a, _)| a) != Some(Some(hand)),
            _ => false,
        }
    };
    let outside = bodies.iter().filter(|&&o| held.binary_search(&o).is_err() && acts(o)).count() as u64;
    let misplaced = physics
        .grab_joints()
        .filter(|&k| physics.joint_objects(k).and_then(|(_, b)| b).map(grab_key) != Some(k))
        .count();
    outside + misplaced as u64
}

/// How far from its place each held object's spring sees it in this frame's step, m, for
/// [`PhysicsWorld::step_holding`]: world-model.md 8.1's hold bound (@coordinator-opus). The spring asks for a speed of about [`PhysicsWorld::grab_spring_rate`] (`k`) times the error it
/// sees, relative to the hand, so the error is bounded by the room the hand's own speed at the hold (`v_hold`, its
/// turn included) leaves under [`MAX_HAND_PUSH_SPEED`]: `(max_push_speed - v_hold) / k`. A held object kept from its
/// place (pinned by a wall) comes back no faster than the cap however far it was kept, and one that lags a hand moving
/// at the cap catches up only once the hand slows. And it is bounded by the speed the hold can brake from (the tight
/// hold): its spring's force is capped ([`grab_spring`]), so it asks for no more than `sqrt(2 a e)` towards its place,
/// with `e` the object's distance from its place and `a` the deceleration its force cap gives the object along the way
/// it comes, gravity's part included (`F / m + g (up . e^)`, at least 0). Without it a spring capped near its object's
/// weight would ask for a speed it can't stop from: an 8 kg ball lifted by a hand of 110 N came up at 2.6 m/s, went
/// 12 cm past its place and fell 54 cm below it before rising again. The upward part of the error, which holds the
/// object up against gravity, keeps up to the hold's [`support`], so a held object never sags out of the hand; in a
/// world without gravity it has none. With a stiff spring (`k dt` over 1) the spring closes the error it sees within
/// a frame, so a held object kept far from its place comes back at about its reach a frame, below the cap (measured at
/// 60 Hz: 4.65 m/s for a pinned pit ball). Every grab joint of `claims`, keyed once.
pub fn reaches(physics: &PhysicsWorld, claims: &[ClaimRow], dt: f32, gravity: [f32; 3]) -> Vec<(JointKey, HoldReach)> {
    // The rate depends on the spring's stiffness and damping alone, not its force.
    let per_metre = f64::from(physics.grab_spring_rate(grab_spring(0.0), dt));
    let g = f64::from((gravity[0] * gravity[0] + gravity[1] * gravity[1] + gravity[2] * gravity[2]).sqrt());
    let up = if g > 0.0 { gravity.map(|c| -f64::from(c) / g) } else { [0.0; 3] };
    let support = support(g);
    let mut out: Vec<(JointKey, HoldReach)> = Vec::with_capacity(claims.len());
    for c in claims {
        let key = grab_key(c.object);
        if physics.joint_kind(key) != Some(JointKind::Grab) || out.iter().any(|&(k, _)| k == key) {
            continue;
        }
        let poses = (physics.pose(c.hand), physics.grab_anchor(key), physics.pose(c.object));
        let error = match poses {
            (Ok(hand), Ok(anchor), Ok(object)) => {
                let r = rotate(hand.rotation, anchor);
                [0usize, 1, 2].map(|i| hand.position[i] + f64::from(r[i]) - object.position[i])
            }
            _ => [0.0; 3],
        };
        let e = error.iter().map(|c| c * c).sum::<f64>().sqrt();
        let brake = match (physics.grab_force(key), physics.mass(c.object)) {
            (Ok(force), Ok(mass)) if e > 0.0 && mass > 0.0 => {
                let along = (0..3).map(|i| up[i] * error[i]).sum::<f64>() / e;
                let a = (f64::from(force) / f64::from(mass) + g * along).max(0.0);
                (2.0 * a * e).sqrt()
            }
            _ => f64::INFINITY,
        };
        // A holding hand has no speed cap (the user's ruling of 2026-11-26), so the hold's own force, the speed it
        // can brake from, is the only bound: a held object kept from its place comes back as fast as its grip pulls.
        // At most the error itself, which is all the spring sees anyway: the brake bound is infinite for an object at
        // its place (or one whose force or mass can't be read), and pe-physics refuses a step with an infinite reach.
        let reach = (brake / per_metre).min(e);
        out.push((key, HoldReach { reach: reach as f32, support: support as f32 }));
    }
    out
}

/// One physical hand's body for `frame`: made at its target if it has none, else driven, coasting or holding.
/// `held` are the objects its claims hold, in id order, which a jump carries with it; `g` is gravity's magnitude.
fn drive(
    physics: &mut PhysicsWorld,
    hand: &HandRow,
    held: &[ObjectId],
    origin: Pos,
    rate: SimRate,
    frame: Frame,
    g: f32,
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
    // A hand that holds something has no speed cap (the user's ruling of 2026-11-26: "remove any
    // maximum throw speed (for v1); the grip strength should be what governs maximum throw speed"), so its move in a
    // frame can't tell a teleport from a fast swing. What tells them apart is a jump away from the hand's own motion:
    // how far the move lands from where the hand's velocity would have taken it and what it holds.
    let holding = !held.is_empty();
    let jump = if holding {
        jump_from_motion(physics.velocity(hand.hand)?, &now, &target, &carried, f64::from(dt))
    } else {
        farthest
    };
    if jump > teleport_distance(rate) {
        // A body slowed by the cap in the last frame trails its target, so this is a trailing placement: it carries a
        // held object only where no static geometry lies in its way, so a held object never passes through a wall
        // (@coordinator-opus); one left behind stays where it is, held by its spring. A body that was
        // not slowed reached its target, so the move is the target's own jump, a teleport, which carries what the
        // hand holds through whatever lies between (world-model.md 8.1).
        let trailing = was_slowed(physics, hand.hand, &now, &carried)?;
        // Placed, not swept, with no velocity: a body placed into others while moving would hit them at its speed
        // plus the push-out (measured: 14.8 m/s at a 10 m/s cap when velocities were kept).
        physics.set_state(hand.hand, target, [0.0; 3], [0.0; 3])?;
        for (object, _, carried) in carried {
            let to = placed(physics, hand, &target, object, carried, g)?;
            if trailing && physics.static_in_way(object, &to)? {
                continue;
            }
            physics.set_state(object, to, [0.0; 3], [0.0; 3])?;
        }
        return Ok(());
    }
    if holding {
        return physics.set_target(hand.hand, target, dt, true);
    }
    let cap = MAX_HAND_PUSH_SPEED * f64::from(dt);
    let to = if farthest > cap { towards(&now, &target, cap / farthest) } else { target };
    physics.set_target(hand.hand, to, dt, true)
}

/// How far a holding hand's move from `now` to `target` lands from where its velocity `(v, w)` (the physics', which a
/// kinematic body keeps from its last move) would have taken it in a frame of `dt` s, m: at the hand itself and at
/// each held object (`carried`: its pose now, and where the move carries it). A steady swing at any speed lands where
/// its motion leads, so only a jump is far from it: a tracking glitch, a teleport, or a body made or placed at rest
/// whose hand is already moving fast.
fn jump_from_motion(
    (v, w): ([f32; 3], [f32; 3]),
    now: &Pose,
    target: &Pose,
    carried: &[(ObjectId, Pose, Pose)],
    dt: f64,
) -> f64 {
    let led = |p: [f64; 3], to: [f64; 3]| {
        let r = [0usize, 1, 2].map(|i| (p[i] - now.position[i]) as f32);
        distance(to, [0usize, 1, 2].map(|i| p[i] + point_velocity(v, w, r)[i] * dt))
    };
    carried.iter().map(|(_, at, to)| led(at.position, to.position)).fold(led(now.position, target.position), f64::max)
}

/// Where a placement of `hand`'s body at `target` puts `object`, which `carried` would put at its offset in the hand
/// (@design-reviewer's L1 on 980e6ba7, @coordinator-opus): a light hold ([`follows_tightly`]) at its
/// place in the hand, the grab joint's anchor at `target`, since the hand could lift it there anyway and a light object
/// that lagged would otherwise keep its lag through every placement (a hand kept at 1.5 times the cap for 2 s left a
/// pit ball 1.1-3.3 m behind; now about 0.2 m); a heavy one, or one with no grab joint from this hand yet, at
/// `carried`, so a placement never lifts what the strength can't. Its rotation relative to the hand is `carried`'s
/// either way.
fn placed(
    physics: &PhysicsWorld,
    hand: &HandRow,
    target: &Pose,
    object: ObjectId,
    carried: Pose,
    g: f32,
) -> Result<Pose, PhysicsError> {
    let key = grab_key(object);
    if physics.joint_kind(key) != Some(JointKind::Grab)
        || physics.joint_objects(key).map(|(a, _)| a) != Some(Some(hand.hand))
        || !follows_tightly(hand.strength, physics.mass(object)?, g)
    {
        return Ok(carried);
    }
    let r = rotate(target.rotation, physics.grab_anchor(key)?);
    Ok(Pose { position: [0usize, 1, 2].map(|i| target.position[i] + f64::from(r[i])), rotation: carried.rotation })
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
    let at = point_velocity(v, w, r);
    (at[0] * at[0] + at[1] * at[1] + at[2] * at[2]).sqrt()
}

/// The velocity of the point `r` from a body's origin, `v + w x r`, for the body's linear and angular velocity.
fn point_velocity(v: [f32; 3], w: [f32; 3], r: [f32; 3]) -> [f64; 3] {
    [v[0] + w[1] * r[2] - w[2] * r[1], v[1] + w[2] * r[0] - w[0] * r[2], v[2] + w[0] * r[1] - w[1] * r[0]]
        .map(f64::from)
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
/// joint can hold, so probing their keys finds every grab joint (a removed body takes its joints with it). `hands`
/// are the world model's, in id order, for each holder's strength.
fn grabs(physics: &mut PhysicsWorld, claims: &[ClaimRow], bodies: &[ObjectId], hands: &[HandRow]) -> u64 {
    let mut faults = 0;
    let held = |id: ObjectId| claims.iter().find(|c| c.object == id && c.hand != ObjectId::NONE).map(|c| c.hand);
    // A hand with a body is one of `hands` (`sync` removes any other), so a missing one never holds.
    let strength = |hand: ObjectId| hands.binary_search_by_key(&hand, |h| h.hand).map_or(0.0, |i| hands[i].strength);
    for &object in bodies {
        let key = grab_key(object);
        let joint = physics.joint_kind(key);
        let hand = held(object).filter(|&hand| physics.has_body(hand) && physics.has_body(object));
        match (joint, hand) {
            (Some(JointKind::Grab), None) => faults += u64::from(physics.remove_joint(key).is_err()),
            (None, Some(hand)) => faults += u64::from(hold(physics, key, hand, object, strength(hand)).is_err()),
            // A renewing grab that names another of its author's hands moves the claim to it (world-model.md 7.2), so
            // the joint is made again from that hand, holding the object where it is (@design-reviewer's Medium on
            // cc037c10).
            (Some(JointKind::Grab), Some(hand)) if physics.joint_objects(key).map(|(a, _)| a) != Some(Some(hand)) => {
                faults += u64::from(physics.remove_joint(key).is_err());
                faults += u64::from(hold(physics, key, hand, object, strength(hand)).is_err());
            }
            _ => {}
        }
    }
    faults
}

/// A grab joint from `hand` to `object`, holding the object where it is now in the hand, with the spring
/// [`grab_spring`] gives for the hand's `strength`.
fn hold(
    physics: &mut PhysicsWorld,
    key: JointKey,
    hand: ObjectId,
    object: ObjectId,
    strength: f32,
) -> Result<(), PhysicsError> {
    let h = physics.pose(hand)?;
    let o = physics.pose(object)?;
    let spring = grab_spring(strength);
    let offset = [o.position[0] - h.position[0], o.position[1] - h.position[1], o.position[2] - h.position[2]];
    let anchor_a = inverse_rotate(h.rotation, offset.map(|c| c as f32));
    let desc = JointDesc::Grab {
        a: Some(hand),
        b: object,
        anchor_a,
        anchor_b: [0.0; 3],
        max_force: 0.0,
        max_torque: GRAB_TORQUE,
        spring: Some(spring),
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

/// How far `object` is from its place in `hand`, m: from the object's anchor to where its grab joint holds it on the
/// hand's body (the joint's anchor on the hand). `None` without such a joint. The view's [`pe_seams::ViewFlags::
/// HELD_AWAY`] reads it (render.md 8, D4's break-away).
pub fn hold_gap(physics: &PhysicsWorld, object: ObjectId, hand: ObjectId) -> Option<f64> {
    let key = grab_key(object);
    if physics.joint_kind(key) != Some(JointKind::Grab) || physics.joint_objects(key)?.0 != Some(hand) {
        return None;
    }
    let (h, o, anchor) = (physics.pose(hand).ok()?, physics.pose(object).ok()?, physics.grab_anchor(key).ok()?);
    let r = rotate(h.rotation, anchor);
    Some(distance(o.position, [0usize, 1, 2].map(|i| h.position[i] + f64::from(r[i]))))
}

/// Whether a held object `gap` from its place in the hand is drawn where it is simulated (render.md 8, D4's
/// break-away): over [`pe_seams::BREAK_AWAY`], or, when it already was (`was_away`), still over
/// [`pe_seams::BREAK_BACK`], so a gap hovering near the threshold doesn't flicker. No gap, no break-away.
pub fn held_away(was_away: bool, gap: Option<f64>) -> bool {
    let bound = if was_away { pe_seams::BREAK_BACK } else { pe_seams::BREAK_AWAY };
    gap.is_some_and(|gap| gap > bound)
}

/// How many publications a [`pe_seams::ViewFlags::HELD_BLOCKED`] object stays blocked after its last touching contact
/// (about 130 ms at 60 Hz; provisional, @shader-reviewer's Medium on the held-object draw): a scrape along a table makes
/// and breaks contact from frame to frame, and each flip would move the drawn object by the hand's display lead.
pub const BLOCKED_HOLD: u8 = 8;

/// Whether an away held object is blocked at this publication, and the publications it then holds for: set at once
/// while `touching` (to [`BLOCKED_HOLD`]); without a touch, one less than `was` (its count at the last publication),
/// and no longer once that runs out.
pub fn blocked_hold(was: Option<u8>, touching: bool) -> Option<u8> {
    if touching {
        return Some(BLOCKED_HOLD);
    }
    was.and_then(|n| n.checked_sub(1)).filter(|&n| n > 0)
}

/// A throw (world-model.md 8.1; @item:primordial-engine/friend-test-11-27-hold-and-release-unfiltered-th): if
/// `t.object` is held by a hand's grab joint, it leaves with the throw's velocity, as far as the hold's force could have
/// changed its velocity in a frame of `dt` s: the change from its solver velocity is at most `F dt / m`, `F` the hold's
/// force, the holder's strength ([`grab_spring`]). There is no speed cap (the user's ruling of 2026-11-26: "the grip strength should be what governs maximum throw speed"): a light object a hand carries leaves
/// at the hand's velocity, and a heavy one that lags it keeps most of what its hold gave it. Nothing else gates it: not
/// how far the object is from its place, nor a wall between them ("keep it straightforward and
/// predictable"). The spin is the throw's, under Box3D's own limit of an eighth of a turn a frame. With no joint from a
/// hand the object keeps its solver velocity, as a `Release` leaves it. Whether the velocity was set. The test is the
/// physics' alone, the same on every replica.
pub fn throw(physics: &mut PhysicsWorld, t: &ThrowFx, dt: f32) -> Result<bool, PhysicsError> {
    let key = grab_key(t.object);
    let Some((Some(_), _)) = physics.joint_objects(key) else { return Ok(false) };
    if physics.joint_kind(key) != Some(JointKind::Grab) {
        return Ok(false);
    }
    let (v, _) = physics.velocity(t.object)?;
    let mass = physics.mass(t.object)?;
    let most = if mass > 0.0 { physics.grab_force(key)? * dt / mass } else { f32::INFINITY };
    let change = capped([0usize, 1, 2].map(|i| t.linear[i] - v[i]), most);
    // Box3D's own limits, which it would clamp to in the step anyway: its top linear speed (400 m/s, box3d-fork's
    // `maximum_linear_speed`, which pe-physics never changes) and an eighth of a turn a frame. Bounding here keeps a
    // wild input (a very light object, a huge spin) inside what `set_velocity` takes, instead of a fault.
    let linear = capped([0usize, 1, 2].map(|i| v[i] + change[i]), BOX3D_TOP_SPEED);
    let angular = capped(t.angular, 0.25 * std::f32::consts::PI / dt);
    physics.set_velocity(t.object, linear, angular)?;
    Ok(true)
}

/// Box3D's top linear speed, m/s (box3d-fork `WorldDef::maximum_linear_speed`; pe-physics never changes it).
const BOX3D_TOP_SPEED: f32 = 400.0;

/// `v` scaled down to length `max`, its direction kept.
fn capped(v: [f32; 3], max: f32) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > max && len > 0.0 { v.map(|c| c * (max / len)) } else { v }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scrape: contact on and off every other publication keeps it blocked throughout; it clears only after
    /// `BLOCKED_HOLD` publications with no touch (@shader-reviewer's Medium).
    #[test]
    fn a_flickering_contact_holds_blocked_and_it_clears_after_the_hold() {
        let mut state = None;
        for k in 0..20 {
            state = blocked_hold(state, k % 2 == 0);
            assert!(state.is_some(), "blocked at publication {k}");
        }
        let mut held = 0;
        while state.is_some() {
            state = blocked_hold(state, false);
            held += u8::from(state.is_some());
        }
        assert_eq!(held, BLOCKED_HOLD - 2, "the last touch was at publication 18, one untouched publication since");
        assert_eq!(blocked_hold(None, false), None, "never touched, never blocked");
    }
}
