//! The local player's input (app.md 3, step 2; xr.md 5.2): both hands at a frame's host time, through the rig, as
//! pe-world's hand inputs, each submitted through agreement on this member's chain.
//!
//! The floating-point work here runs only on the author's machine. Every peer steps the encoded, signed input, so
//! determinism needs nothing from it (xr.md 5.2).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pe_seams::{
    Agreement, ChainId, Class, DomainId, Entry, EntryRef, Frame, FrameView, Hand, HandFeed, HandInput, HostTime,
    LineId, MemberKey, MoveFeed, MoveInput, ObjectId, Offset, Pos, RigTransform, SignedEntry,
};
use pe_world::input::{HandPose, Input};
use pe_world::{BodyHandPose, MoveSample};

/// What step 2 asks the world when a hand's grasp changes (app.md 3.1, step 2): the object a close grabs, and what an
/// opening hand holds. pe-app answers from the replica's present world (pe-world's `pick_grab` and claims).
pub trait Grasps {
    /// The object a close of `author`'s `hand` grabs, with the hand's target at `target` in a `Grab` stamped `now`, if
    /// one is in reach.
    fn pick(&self, author: MemberKey, hand: ObjectId, target: Pos, now: Frame) -> Option<ObjectId>;
    /// The objects `hand` holds at `now`.
    fn held(&self, hand: ObjectId, now: Frame) -> Vec<ObjectId>;
}

/// Each hand's grasp as step 2 last sent it, left then right.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Grasp {
    /// The `engaged` last sent. A grasp's edges are changes of what was sent, never of the raw sample.
    pub sent: [bool; 2],
    /// The object the hand's last close named in a `Grab`, until it opens: released at the open even when no view
    /// shows the claim yet.
    pub grabbed: [Option<ObjectId>; 2],
}

/// What step 2 sent for the hands at one tick.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HandsSent {
    /// Hand inputs agreement took, and entries it refused (hand inputs, grabs and releases).
    pub took: u32,
    pub refused: u32,
    /// Poses not sent while the stamp stood still.
    pub thinned: u32,
    /// Poses not sent because the hand was past its body's reach ([`MAX_HAND_OFFSET`] on an axis; [`body_pose`] gave
    /// none): a tracked hand that makes no entry, and whose grasp's edges wait until it is back (friend-test K4).
    pub out_of_reach: u32,
    /// Per hand, left then right: `engaged` edges sent, false to true (a close) and true to false (an open).
    pub closes: [u32; 2],
    pub opens: [u32; 2],
    /// Per hand: the `Release`s agreement took.
    pub releases: [u32; 2],
    /// Per hand: the objects it handed over at its open to the other hand, still closed on them, by a `Grab` from that
    /// hand instead of a release (two-hand holds, @coordinator-opus). Each such grab is in `grabs` too.
    pub handovers: [u32; 2],
    /// The `Grab`s agreement took, each with its hand (0 left, 1 right), so the views' records can tell their
    /// outcomes.
    pub grabs: Vec<(usize, EntryRef)>,
    /// The `Release`s and `Throw`s agreement took, each with the object it names, so the views' records can tell
    /// which applied (`ViewFlags::RELEASED_LOCAL`, render.md 8's hand-off).
    pub released: Vec<(EntryRef, ObjectId)>,
    /// Per hand: of those, the `Throw`s (world-model.md 8.1), each a release carrying the velocity the hand gave the
    /// object at the real opening.
    pub throws: [u32; 2],
    /// Each throw sent, for the log: its hand, object and velocities, and the fastest the object moved with the hand
    /// over [`THROW_PEAK_WINDOW`] before the opening, m/s, so a window can be tuned later with no format change.
    pub thrown: Vec<ThrowSent>,
}

/// One `Throw` step 2 sent (app.md 3, step 2; world-model.md 8.1), for the log.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThrowSent {
    /// 0 left, 1 right.
    pub hand: usize,
    pub object: ObjectId,
    /// What the throw carries, in the world frame: the object's centre's velocity, m/s, and the hand's spin, rad/s.
    pub linear: [f32; 3],
    pub angular: [f32; 3],
    /// The fastest the object's centre moved with the hand over [`THROW_PEAK_WINDOW`] up to the opening, m/s.
    pub peak: f32,
}

/// How far back the log's peak looks (the weak-throw fix, @fable-reviewer via @coordinator-opus).
pub const THROW_PEAK_WINDOW: std::time::Duration = std::time::Duration::from_millis(50);

/// This member's chain (S1): every entry it authors, inputs and edits alike, in order. The first entry carries the
/// nonce its chain id derives from; each later one names the one before.
pub struct Chain {
    line: LineId,
    me: MemberKey,
    nonce: [u8; 32],
    id: ChainId,
    next: u64,
    prev: Option<EntryRef>,
    /// The stamp of the last entry agreement took: an entry stamped ahead never steps below it (stamp-ahead).
    last: Option<Frame>,
}

impl Chain {
    pub fn new(line: LineId, me: MemberKey, nonce: [u8; 32]) -> Chain {
        Chain { line, me, nonce, id: ChainId::new(line, me, nonce), next: 1, prev: None, last: None }
    }

    pub fn me(&self) -> MemberKey {
        self.me
    }

    /// The chain's id, and the seq of the last entry agreement took (0 before the first).
    pub fn head(&self) -> (ChainId, u64) {
        (self.id, self.next - 1)
    }

    /// The chain's next entry, unsigned: `Agreement::submit` signs it.
    pub fn entry(&self, domain: DomainId, stamp: Frame, class: Class, payload: Vec<u8>) -> Entry {
        Entry {
            line: self.line,
            author: self.me,
            chain: self.id,
            seq: self.next,
            prev: self.prev,
            nonce: (self.next == 1).then_some(self.nonce),
            domain,
            stamp,
            class,
            payload: payload.into(),
        }
    }

    /// Agreement took `e`, this chain's next entry.
    pub fn took(&mut self, e: &SignedEntry) {
        self.next += 1;
        self.prev = Some(e.id());
        self.last = Some(e.entry.stamp);
    }

    /// The stamp of the last entry agreement took, `None` before the first.
    pub fn last_stamp(&self) -> Option<Frame> {
        self.last
    }
}

/// A nonce for a new chain. It only has to differ between this member's chains, since `ChainId::new` hashes it with
/// the line and the key: the start time to the nanosecond and the process id.
pub fn chain_nonce() -> [u8; 32] {
    let mut n = [0u8; 32];
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    n[..16].copy_from_slice(&t.to_le_bytes());
    n[16..20].copy_from_slice(&std::process::id().to_le_bytes());
    n
}

/// The hand feed in use: VR's ring while an XR session runs, the desktop hand otherwise (app.md 1; xr.md 3.3).
/// pe-app flips it when the session starts and ends. VR's ring keeps answering from its last samples after a session,
/// so a feed that took whichever answered first would hold the hands where VR left them.
pub struct SwitchedHands {
    vr: Arc<dyn HandFeed>,
    desktop: Arc<dyn HandFeed>,
    vr_on: AtomicBool,
}

impl SwitchedHands {
    pub fn new(vr: Arc<dyn HandFeed>, desktop: Arc<dyn HandFeed>) -> SwitchedHands {
        SwitchedHands { vr, desktop, vr_on: AtomicBool::new(false) }
    }

    pub fn use_vr(&self, on: bool) {
        self.vr_on.store(on, Ordering::Release);
    }
}

impl HandFeed for SwitchedHands {
    fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput> {
        if self.vr_on.load(Ordering::Acquire) { self.vr.hand_at(hand, at) } else { self.desktop.hand_at(hand, at) }
    }
}

/// The move feed in use (locomotion L4b), switched with [`SwitchedHands`]: VR's while an XR session runs, the
/// desktop's otherwise. pe-app flips both together.
pub struct SwitchedMove {
    vr: Arc<dyn MoveFeed>,
    desktop: Arc<dyn MoveFeed>,
    vr_on: AtomicBool,
}

impl SwitchedMove {
    pub fn new(vr: Arc<dyn MoveFeed>, desktop: Arc<dyn MoveFeed>) -> SwitchedMove {
        SwitchedMove { vr, desktop, vr_on: AtomicBool::new(false) }
    }

    pub fn use_vr(&self, on: bool) {
        self.vr_on.store(on, Ordering::Release);
    }
}

impl MoveFeed for SwitchedMove {
    fn move_at(&self, at: HostTime) -> Option<MoveInput> {
        if self.vr_on.load(Ordering::Acquire) { self.vr.move_at(at) } else { self.desktop.move_at(at) }
    }
}

/// The frame a body's hand poses are sent in (world-model.md 8.2, `BodyHand`; locomotion L3): the player's body and
/// the heading and head of the Move sent for the same frame, or of the last one sent. A body never turns, so its frame
/// is the world's axes at its feet; a hand's place in it is its rig-frame pose less the head's offset over the floor,
/// turned by the heading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyFrame {
    pub body: ObjectId,
    pub heading: u32,
    pub head: [f32; 3],
}

/// The most a hand may be from its body's feet on an axis, m (pe-world `player::MAX_HAND_OFFSET`): past it, tracking
/// went wrong and no pose goes.
const MAX_HAND_OFFSET: f64 = 4.0;
/// The most the head may be from the rig's origin on an axis, m (pe-world `player::MAX_HEAD_OFFSET`).
const MAX_HEAD_OFFSET: f32 = 50.0;

/// A locomotion sample as a `Move` carries it (world-model.md 8.2): the stick quantised by pe-world's own rule, the
/// stick's yaw turned from radians into the wrapping angle (2^32 one turn), the head clamped into the range pe-world
/// takes and its rotation normalized (the identity if it is no rotation). The heading and the press count go as they
/// are: both are cumulative, so a lost or thinned sample heals at the next.
pub fn move_sample(m: &MoveInput) -> MoveSample {
    let yaw = if m.stick_yaw.is_finite() {
        let turns = f64::from(m.stick_yaw) / std::f64::consts::TAU;
        ((turns - turns.floor()) * 4_294_967_296.0).round() as u64 as u32
    } else {
        0
    };
    MoveSample {
        stick: MoveSample::quantise_stick(m.stick),
        stick_yaw: yaw,
        heading: m.heading,
        jumps: m.jumps,
        head_pos: m.head.position.map(|c| if c.is_finite() { c.clamp(-MAX_HEAD_OFFSET, MAX_HEAD_OFFSET) } else { 0.0 }),
        head_rot: normalized(m.head.orientation).unwrap_or([0.0, 0.0, 0.0, 1.0]),
        tracked: m.tracked,
    }
}

/// A hand's pose in its body's frame (`BodyHand`): its rig-frame position less the head's offset over the floor,
/// turned by the heading, and its rotation turned by the heading, with the grip clamped and the grasp as decided.
/// The turn is pe-replica's ([`pe_replica::players::turned`]), the one the view's rig uses, so the hand pe-world places
/// and the hand pe-render draws from tracking agree. `None` past [`MAX_HAND_OFFSET`] or for no rotation.
pub fn body_pose(frame: &BodyFrame, h: &HandInput) -> Option<BodyHandPose> {
    let [x, y, z] = h.pose.position.map(f64::from);
    let [hx, _, hz] = frame.head.map(f64::from);
    let offset = pe_replica::players::turned(frame.heading, [x - hx, y, z - hz]);
    if !offset.iter().all(|c| c.is_finite() && c.abs() <= MAX_HAND_OFFSET) {
        return None;
    }
    let rot = normalized(mul(pe_replica::players::heading_rotation(frame.heading), h.pose.orientation))?;
    Some(BodyHandPose { offset: Offset(offset), rot, grip: h.grip.clamp(0.0, 1.0), engaged: h.closed })
}

/// Sends this frame's `Move` for `frame.body` (locomotion L3): `sample`, held `hold` frames more so a thinned or late
/// frame doesn't stop the body. Its entry's id if agreement took it.
pub fn submit_move(
    chain: &mut Chain,
    agree: &mut dyn Agreement,
    domain: DomainId,
    p: Frame,
    body: ObjectId,
    sample: MoveSample,
    hold: u8,
) -> Option<EntryRef> {
    send_input(chain, agree, domain, p, &Input::Move { body, samples: vec![sample], hold })
}

/// A hand's world pose for pe-world's input: its rig-frame pose through the rig (xr.md 3.2), the grip clamped to
/// 0..1, and engaged when the author's machine decided the grasp closed. `None` if the pose falls outside the world,
/// or its rotation is no rotation.
pub fn world_pose(rig: &RigTransform, h: &HandInput) -> Option<HandPose> {
    let [x, y, z] = h.pose.position;
    let p = rotate(rig.rotation, [x * rig.scale, y * rig.scale, z * rig.scale]);
    let pos = rig.pos.checked_add(Offset(p.map(f64::from))).ok()?;
    let rot = normalized(mul(rig.rotation, h.pose.orientation))?;
    Some(HandPose { pos, rot, grip: h.grip.clamp(0.0, 1.0), engaged: h.closed })
}

/// Step 2's hands: one input entry for each hand the feed tracks at `at`, frame `p`'s host time, each moving the local
/// member's own hand object of that side, as the newest view shows it, to its world pose. Returns the entries
/// agreement took and those it refused.
///
/// An untracked hand sends nothing, so the world coasts it and then holds it (xr.md 5.5, D52), except when its grasp
/// differs from the `engaged` last sent for that side (`sent`, left then right): that is pe-xr's deliberate release
/// of a lost hand that held something, after the grace period or on focus loss, and it goes out at the last valid
/// pose, so nothing stays held by a frozen hand (@xr-eng's Medium on cc8d0a96).
///
/// `thin`: the frame's stamp stands still (stamp-ahead is shedding its lead, app.md 3.2), so a pose would only replace
/// the previous frame's at the same stamp. Such poses aren't sent; a grasp that changed (a grab or a release) still
/// goes out, at the stamp (@coordinator-opus).
///
/// A hand input that changes the `engaged` last sent is a grasp's edge (app.md 3.1 step 2; @coordinator-opus's ruling
/// on the pick, after @fable-reviewer). A close also sends a `Grab`, in the same frame, for the object `world` picks
/// within reach of the target just sent; it is never tried again while the hand stays closed, refused or not. An
/// open sends a `Release` for each object that hand holds, and for its last close's grab, except what the other hand
/// last grabbed: a ball passed between hands stays in the hand that took it. **A two-hand hold** is held by one hand
/// at a time (one claim an object, world-model.md 7.2): if the other hand is closed when the holding hand opens, and
/// `world` picks the object for it within its reach, the open sends a `Grab` from the other hand instead, so the object
/// stays in the hand still holding it and leaves only when both have let go (@coordinator-opus).
#[allow(clippy::too_many_arguments)]
pub fn submit_hands(
    chain: &mut Chain,
    grasp: &mut Grasp,
    feed: &dyn HandFeed,
    world: &dyn Grasps,
    view: &FrameView,
    at: HostTime,
    now: HostTime,
    p: Frame,
    domain: DomainId,
    agree: &mut dyn Agreement,
    thin: bool,
    body: Option<&BodyFrame>,
    before: Option<(HostTime, RigTransform)>,
) -> HandsSent {
    let mut out = HandsSent::default();
    for (i, side) in [Hand::Left, Hand::Right].into_iter().enumerate() {
        let Some(h) = feed.hand_at(side, at).filter(|h| h.tracked || h.closed != grasp.sent[i]) else { continue };
        if thin && h.closed == grasp.sent[i] {
            out.thinned += 1;
            continue;
        }
        let Some(hand) = view.objects.iter().find(|o| o.hand_of == Some((chain.me(), side))).map(|o| o.id) else {
            continue;
        };
        // The world pose through the newest view's rig: what a grab's pick measures its reach from.
        let Some(pose) = world_pose(&view.local_rig, &h) else { continue };
        // With a body, the hand goes in the body's frame, from the heading and head of this frame's Move, so it never
        // trails the body (world-model.md 8.2; locomotion L3).
        let payload = match body {
            Some(f) => match body_pose(f, &h) {
                Some(bp) => Input::BodyHand { hand, poses: vec![bp], hold: 0 }.encode(),
                None => {
                    out.out_of_reach += 1;
                    continue;
                }
            },
            None => Input::Hand { hand, poses: vec![pose], hold: 0 }.encode(),
        };
        match agree.submit(chain.entry(domain, p, Class::Input, payload)) {
            Ok(signed) => {
                chain.took(&signed);
                out.took += 1;
            }
            Err(_) => {
                out.refused += 1;
                continue;
            }
        }
        let was = std::mem::replace(&mut grasp.sent[i], pose.engaged);
        match (was, pose.engaged) {
            (false, true) => {
                out.closes[i] += 1;
                if let Some(object) = world.pick(chain.me(), hand, pose.pos, p) {
                    match send_input(chain, agree, domain, p, &Input::Grab { object, hand: Some(hand) }) {
                        Some(id) => {
                            grasp.grabbed[i] = Some(object);
                            // Taken from the other hand: that hand's open no longer releases it.
                            if grasp.grabbed[1 - i] == Some(object) {
                                grasp.grabbed[1 - i] = None;
                            }
                            out.grabs.push((i, id));
                        }
                        None => out.refused += 1,
                    }
                }
            }
            (true, false) => {
                out.opens[i] += 1;
                // The other hand, if it is closed now and stays so this tick (its sample closed and its last edge a
                // close), at its world pose: the hand a two-hand hold passes to.
                let other_side = if i == 0 { Hand::Right } else { Hand::Left };
                let other =
                    view.objects.iter().find(|o| o.hand_of == Some((chain.me(), other_side))).map(|o| o.id).zip(
                        grasp.sent[1 - i]
                            .then(|| feed.hand_at(other_side, at))
                            .flatten()
                            .filter(|o| o.closed)
                            .and_then(|o| world_pose(&view.local_rig, &o)),
                    );
                let mut held = world.held(hand, p);
                held.extend(grasp.grabbed[i].take());
                // What the other hand last grabbed stays with it: pe-world's `Release` ends the member's claim whichever
                // hand holds it, and the present world may not show the pass between hands yet (@design-reviewer's
                // Medium on 82644261).
                held.retain(|o| grasp.grabbed[1 - i] != Some(*o));
                held.sort();
                held.dedup();
                for object in held {
                    if let Some((other, at_other)) = &other
                        && world.pick(chain.me(), *other, at_other.pos, p) == Some(object)
                    {
                        match send_input(chain, agree, domain, p, &Input::Grab { object, hand: Some(*other) }) {
                            Some(id) => {
                                grasp.grabbed[1 - i] = Some(object);
                                out.grabs.push((1 - i, id));
                                out.handovers[i] += 1;
                                continue;
                            }
                            // Refused: it leaves as a release would, below.
                            None => out.refused += 1,
                        }
                    }
                    // A throw where the hand's velocity at the real opening is known and finite (world-model.md 8.1):
                    // the grasp goes in at the stamp, after the opening, on a hand that may already be slowing, so
                    // the object takes the velocity it had with the hand when the player opened it. Else a release.
                    let throw = throw_at(feed, side, view, hand, object, now, before);
                    let input = match throw {
                        Some((linear, angular, _)) => Input::Throw { object, linear, angular },
                        None => Input::Release { object },
                    };
                    match send_input(chain, agree, domain, p, &input) {
                        Some(id) => {
                            out.releases[i] += 1;
                            out.released.push((id, object));
                            if let Some((linear, angular, peak)) = throw {
                                out.throws[i] += 1;
                                out.thrown.push(ThrowSent { hand: i, object, linear, angular, peak });
                            }
                        }
                        None => out.refused += 1,
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// What [`submit_hand_mode`] sent: `HandMode` entries agreement took, and those it refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModesSent {
    pub took: u32,
    pub refused: u32,
}

/// Sends `HandMode { hand, mode }` at stamp `p` for each of this member's hands in `view` (the pull-ahead plan's 1.10,
/// [`crate::handfeel`]); pe-world takes it from the hand's owner only. A hand not in the view yet gets nothing.
pub fn submit_hand_mode(
    chain: &mut Chain,
    view: &FrameView,
    mode: u8,
    p: Frame,
    domain: DomainId,
    agree: &mut dyn Agreement,
) -> ModesSent {
    let mut out = ModesSent::default();
    for side in [Hand::Left, Hand::Right] {
        let Some(hand) = view.objects.iter().find(|o| o.hand_of == Some((chain.me(), side))).map(|o| o.id) else {
            continue;
        };
        match send_input(chain, agree, domain, p, &Input::HandMode { hand, mode }) {
            Some(_) => out.took += 1,
            None => out.refused += 1,
        }
    }
    out
}

/// Submits `input` at stamp `p` on this member's chain: its entry's id if agreement took it.
fn send_input(
    chain: &mut Chain,
    agree: &mut dyn Agreement,
    domain: DomainId,
    p: Frame,
    input: &Input,
) -> Option<EntryRef> {
    let signed = agree.submit(chain.entry(domain, p, Class::Input, input.encode())).ok()?;
    chain.took(&signed);
    Some(signed.id())
}

/// What a `Throw` of `object` from `hand` carries, measured at the real present `now` (world-model.md 8.1): the
/// object's velocity with the hand, the hand's linear velocity plus its spin × the offset from the hand to the object,
/// and the hand's spin, in the world frame; and the fastest the object moved over
/// [`THROW_PEAK_WINDOW`] up to `now`, for the log. `None` without the hand's velocities, without both poses in the
/// view, or with a value not finite: then a plain `Release` goes (a refused throw would leave the claim to lapse).
fn throw_at(
    feed: &dyn HandFeed,
    side: Hand,
    view: &FrameView,
    hand: ObjectId,
    object: ObjectId,
    now: HostTime,
    before: Option<(HostTime, RigTransform)>,
) -> Option<([f32; 3], [f32; 3], f32)> {
    // To the object's origin, as the view gives it: its centre of mass for M1's balls. A compound whose centre of mass
    // is off its origin is measured there until pe-app reads the centre (app.md 3, step 2).
    let r = offset_vec(view.object(object)?.pos - view.object(hand)?.pos);
    let rig = &view.local_rig;
    let at = |t: HostTime| -> Option<([f32; 3], [f32; 3])> {
        let h = feed.hand_at(side, t)?;
        // The rig carries the hand (locomotion L3): its own motion at the hand, walking and a smooth turn's swing, is
        // part of the throw. Not the body's velocity: in room scale the body follows the head while the rig stands
        // still, and the tracked hand's velocity already holds that step (@design-reviewer's M1).
        let carried = rig_velocity_at(before, view, h.pose.position);
        let (v, w) = (h.linear_velocity?, h.angular_velocity.unwrap_or([0.0; 3]));
        let v = rotate(rig.rotation, v.map(|c| c * rig.scale));
        let w = rotate(rig.rotation, w);
        let [rx, ry, rz] = r;
        let spin = [w[1] * rz - w[2] * ry, w[2] * rx - w[0] * rz, w[0] * ry - w[1] * rx];
        Some(([v[0] + spin[0] + carried[0], v[1] + spin[1] + carried[1], v[2] + spin[2] + carried[2]], w))
    };
    let (linear, angular) = at(now)?;
    if !linear.iter().chain(&angular).all(|c| c.is_finite()) {
        return None;
    }
    let speed = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let window = i64::try_from(THROW_PEAK_WINDOW.as_nanos()).unwrap_or(i64::MAX);
    let peak = (0..=5)
        .filter_map(|k| at(HostTime(now.0 - window * k / 5)))
        .map(|(v, _)| speed(v))
        .filter(|s| s.is_finite())
        .fold(0.0, f32::max);
    Some((linear, angular, peak))
}

/// Views further apart than this, s, give no rig velocity for a throw: a gap, not a frame's motion.
const RIG_GAP_S: f64 = 0.1;
/// A rig faster than this at the hand, m/s [I], is a correction between the two views, not a motion a throw carries:
/// a walk (5 m/s) with a jump or fall and a turn's swing stay below it.
const RIG_THROW_CAP: f64 = 15.0;

/// The velocity the rig gives a point `p` of the rig frame (a tracked hand) between the view before (`before`, its
/// time and rig) and `view`, in the world (locomotion L3's throws): zero without the view before, across a gap, or
/// faster than [`RIG_THROW_CAP`].
fn rig_velocity_at(before: Option<(HostTime, RigTransform)>, view: &FrameView, p: [f32; 3]) -> [f32; 3] {
    let Some((t0, r0)) = before else { return [0.0; 3] };
    let dt = view.sim_time.secs_since(t0);
    if !(dt > 0.0 && dt <= RIG_GAP_S) {
        return [0.0; 3];
    }
    let (Some(p1), Some(p0)) = (rig_point(&view.local_rig, p), rig_point(&r0, p)) else { return [0.0; 3] };
    let d = (p1 - p0).0.map(|c| c / dt);
    if d.iter().map(|c| c * c).sum::<f64>().sqrt() > RIG_THROW_CAP {
        return [0.0; 3];
    }
    d.map(|c| c as f32)
}

/// Where `rig` puts the rig-frame point `p` in the world.
fn rig_point(rig: &RigTransform, p: [f32; 3]) -> Option<Pos> {
    let q = rotate(rig.rotation, p.map(|c| c * rig.scale));
    rig.pos.checked_add(Offset(q.map(f64::from))).ok()
}

/// `d` as single-precision metres (offsets inside a held object's reach are small).
fn offset_vec(d: pe_seams::Offset) -> [f32; 3] {
    d.0.map(|c| c as f32)
}

/// The Hamilton product `a b` of two quaternions in OpenXR's order, (x, y, z, w).
fn mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

/// `v` rotated by the unit quaternion `q`.
pub(crate) fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let r = mul(mul(q, [v[0], v[1], v[2], 0.0]), [-q[0], -q[1], -q[2], q[3]]);
    [r[0], r[1], r[2]]
}

fn normalized(q: [f32; 4]) -> Option<[f32; 4]> {
    let n = q.iter().map(|v| v * v).sum::<f32>().sqrt();
    (n.is_finite() && n > 1e-6).then(|| q.map(|v| v / n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{HandSource, Pos, RigPose};

    fn sample(position: [f32; 3], orientation: [f32; 4]) -> HandInput {
        HandInput {
            hand: Hand::Right,
            source: HandSource::Tracked,
            sampled_at: HostTime(0),
            tracked: true,
            pose: RigPose { position, orientation },
            linear_velocity: None,
            angular_velocity: None,
            grip: 1.5,
            closed: true,
            discontinuity: 0,
        }
    }

    /// A view at `ms` with the rig at `x` along +X, holding a hand and an object at the same place (so no spin term).
    fn throw_view(ms: i64, x: f64) -> FrameView {
        let at = Pos::ORIGIN.checked_add(Offset([x, 1.0, -0.4])).unwrap();
        let obj = |n: u128| pe_seams::ViewObject {
            id: ObjectId::from_u128(n),
            class: pe_seams::ObjectClass::World,
            pos: at,
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            linear_velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
            shape: None,
            appearance: None,
            light: None,
            radius: 0.1,
            hand_of: None,
            body_of: None,
            held_by: None,
            flags: pe_seams::ViewFlags::default(),
            version: None,
        };
        FrameView {
            frame: Frame(ms as u64),
            domain: DomainId(0),
            sim_time: HostTime::from_millis(ms),
            final_frame: None,
            health: pe_seams::Health::Agreed,
            local_rig: RigTransform {
                pos: Pos::ORIGIN.checked_add(Offset([x, 0.0, 0.0])).unwrap(),
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: 1.0,
            },
            objects: vec![obj(1), obj(2)],
            effects: vec![],
            retracted: vec![],
            counters: pe_seams::ViewCounters::default(),
            changes: vec![],
            activations: vec![],
            activations_retracted: vec![],
            hits: vec![],
            hits_from: None,
            heads: vec![],
        }
    }

    /// A right hand moving at `v` in the rig frame.
    struct Moving([f32; 3]);
    impl HandFeed for Moving {
        fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput> {
            let mut h = sample([0.0, 1.0, -0.4], [0.0, 0.0, 0.0, 1.0]);
            h.hand = hand;
            h.sampled_at = at;
            h.linear_velocity = Some(self.0);
            h.angular_velocity = Some([0.0; 3]);
            Some(h)
        }
    }

    /// @design-reviewer's M1 on L3: in room scale the rig stands still while the body follows the head, so a throw made
    /// stepping forward at 1 m/s carries the step once (from the tracked hand), never the body's velocity on top. A rig
    /// walking at 3 m/s with the hand still in it gives the object the walk, and two views far apart give nothing.
    #[test]
    fn a_throw_carries_the_rig_s_motion_once() {
        let (hand, object) = (ObjectId::from_u128(1), ObjectId::from_u128(2));
        let now = HostTime::from_millis(40);
        let step = Moving([0.0, 0.0, -1.0]);
        let still_rig = Some((HostTime::from_millis(16), throw_view(16, 0.0).local_rig));
        let (linear, _, _) = throw_at(&step, Hand::Right, &throw_view(33, 0.0), hand, object, now, still_rig).unwrap();
        assert!((linear[2] + 1.0).abs() < 1e-5 && linear[0].abs() < 1e-5, "the step once: {linear:?}");
        let walked = Some((HostTime::from_millis(16), throw_view(16, 0.0).local_rig));
        let still = Moving([0.0; 3]);
        let (linear, _, _) = throw_at(&still, Hand::Right, &throw_view(33, 0.051), hand, object, now, walked).unwrap();
        assert!((linear[0] - 3.0).abs() < 0.01 && linear[2].abs() < 1e-5, "the walk: {linear:?}");
        let gap = Some((HostTime::from_millis(-500), throw_view(0, -1.0).local_rig));
        let (linear, _, _) = throw_at(&still, Hand::Right, &throw_view(33, 0.0), hand, object, now, gap).unwrap();
        assert_eq!(linear, [0.0; 3], "a gap carries nothing");
    }

    /// A rig turned 90 degrees about +Y, scaled 2 and placed at (10, 0, 0): a hand 1 m ahead (-Z) of the player is 2 m
    /// along -X in the world, and its rotation turns with the rig.
    #[test]
    fn a_pose_goes_through_the_rig() {
        let half = std::f32::consts::FRAC_1_SQRT_2;
        let rig = RigTransform {
            pos: Pos::ORIGIN.checked_add(Offset([10.0, 0.0, 0.0])).expect("in range"),
            rotation: [0.0, half, 0.0, half],
            scale: 2.0,
        };
        let pose = world_pose(&rig, &sample([0.0, 1.0, -1.0], [0.0, 0.0, 0.0, 1.0])).expect("a pose");
        let d = pose.pos - rig.pos;
        assert!((d.0[0] + 2.0).abs() < 1e-5 && (d.0[1] - 2.0).abs() < 1e-5 && d.0[2].abs() < 1e-5, "{d:?}");
        assert!(pose.rot.iter().zip(rig.rotation).all(|(a, b)| (a - b).abs() < 1e-6), "{:?}", pose.rot);
        assert_eq!((pose.grip, pose.engaged), (1.0, true), "the grip clamps, and the grasp engages");
        assert!(world_pose(&rig, &sample([0.0; 3], [0.0; 4])).is_none(), "no rotation, no pose");
    }

    /// The chain: seq from 1, the nonce only on the first entry, and each later entry naming the one before.
    #[test]
    fn a_chain_numbers_its_entries_and_links_them() {
        let me = MemberKey([3; 32]);
        let mut c = Chain::new(LineId::origin(), me, [9; 32]);
        let first = c.entry(DomainId(0), Frame(4), Class::Input, vec![1]);
        assert_eq!((first.seq, first.prev, first.nonce), (1, None, Some([9; 32])));
        assert_eq!(first.chain, ChainId::new(LineId::origin(), me, [9; 32]));
        let signed = SignedEntry { entry: first, sig: pe_seams::Signature([0; 64]) };
        c.took(&signed);
        let second = c.entry(DomainId(0), Frame(5), Class::Input, vec![2]);
        assert_eq!((second.seq, second.prev, second.nonce), (2, Some(signed.id()), None));
    }
}
