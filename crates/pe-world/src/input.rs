//! Inputs (world-model.md 8.1): a hand's pose packs, grabs and releases with their claims and cooldowns, and
//! pushes under a per-member budget. [`World::apply_input`] applies one input entry; [`World::end_frame`] expires
//! claims and cooldowns and writes each pending pack's next pose.
//!
//! The payload follows `edit`'s codec conventions: little-endian, one-byte tags, flags 0 or 1, `u32`-counted lists,
//! ids as `u128`, a `Pos` in its canonical encoding (4.5); decoding is total and strict, so `encode(decode(b)) == b`
//! for every accepted `b`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use pe_core::{Frame, MemberKey, ObjectId, Pos};
use pe_seams::{Class, Note, Outcome, Reason, SignedEntry};

use crate::apply::{Footprint, Push};
use crate::components::{Body, Hand, Shape, shape_kind};
use crate::edit::{R, W};
use crate::world::{Claim, World};

/// The most poses one hand input may carry (8.1).
pub const MAX_POSES: usize = 16;

/// One pose of a hand pack: the hand's target, its grip and whether it is engaged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HandPose {
    pub pos: Pos,
    pub rot: [f32; 4],
    pub grip: f32,
    pub engaged: bool,
}

/// An input entry's payload (8.1).
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// Frames `stamp ..= stamp + poses.len() - 1 + hold` of one hand (D73 (a)): pose `j` is the target at frame
    /// `stamp + j`; the last pose is then held `hold` more frames.
    Hand { hand: ObjectId, poses: Vec<HandPose>, hold: u8 },
    /// An impulse in newton seconds, in the world frame, at `at` (a world `Pos`) or the centre of mass.
    Push { object: ObjectId, impulse: [f32; 3], at: Option<Pos> },
    /// Claims `object` for the author's `hand` (7.2), or renews the author's claim on it. `None` for an agent with
    /// no hand, which then renews by repeating the `Grab`.
    Grab { object: ObjectId, hand: Option<ObjectId> },
    /// Ends the author's claim on `object`; without one it changes nothing and notes `NoEffect`.
    Release { object: ObjectId },
}

const TAG_HAND: u8 = 0;
const TAG_PUSH: u8 = 1;
const TAG_GRAB: u8 = 2;
const TAG_RELEASE: u8 = 3;

/// The fewest bytes one encoded pose takes: a `Pos` (three one-byte varints and three `f64`), a rotation, a grip and
/// a flag.
const MIN_POSE_BYTES: usize = 3 * (1 + 8) + 16 + 4 + 1;

impl W {
    fn f32(&mut self, v: f32) {
        self.raw(&v.to_le_bytes());
    }
    fn pos(&mut self, p: &Pos) {
        p.encode(&mut self.0);
    }
}

impl R<'_> {
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.arr()?))
    }
    fn pos(&mut self) -> Option<Pos> {
        let (p, n) = Pos::decode(self.b).ok()?;
        self.take(n)?;
        Some(p)
    }
    fn input(&mut self) -> Option<Input> {
        Some(match self.u8()? {
            TAG_HAND => {
                let hand = self.id()?;
                let n = self.count(MIN_POSE_BYTES, MAX_POSES)?;
                if n == 0 {
                    return None;
                }
                let mut poses = Vec::with_capacity(n);
                for _ in 0..n {
                    let pos = self.pos()?;
                    let rot = [self.f32()?, self.f32()?, self.f32()?, self.f32()?];
                    let grip = self.f32()?;
                    let engaged = self.flag()?;
                    poses.push(HandPose { pos, rot, grip, engaged });
                }
                Input::Hand { hand, poses, hold: self.u8()? }
            }
            TAG_PUSH => {
                let object = self.id()?;
                let impulse = [self.f32()?, self.f32()?, self.f32()?];
                let at = if self.flag()? { Some(self.pos()?) } else { None };
                Input::Push { object, impulse, at }
            }
            TAG_GRAB => {
                let object = self.id()?;
                let hand = if self.flag()? { Some(self.id()?) } else { None };
                Input::Grab { object, hand }
            }
            TAG_RELEASE => Input::Release { object: self.id()? },
            _ => return None,
        })
    }
}

impl Input {
    /// The canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match self {
            Input::Hand { hand, poses, hold } => {
                w.u8(TAG_HAND);
                w.id(*hand);
                w.len(poses.len());
                for p in poses {
                    w.pos(&p.pos);
                    for r in p.rot {
                        w.f32(r);
                    }
                    w.f32(p.grip);
                    w.u8(u8::from(p.engaged));
                }
                w.u8(*hold);
            }
            Input::Push { object, impulse, at } => {
                w.u8(TAG_PUSH);
                w.id(*object);
                for v in impulse {
                    w.f32(*v);
                }
                match at {
                    None => w.u8(0),
                    Some(p) => {
                        w.u8(1);
                        w.pos(p);
                    }
                }
            }
            Input::Grab { object, hand } => {
                w.u8(TAG_GRAB);
                w.id(*object);
                match hand {
                    None => w.u8(0),
                    Some(h) => {
                        w.u8(1);
                        w.id(*h);
                    }
                }
            }
            Input::Release { object } => {
                w.u8(TAG_RELEASE);
                w.id(*object);
            }
        }
        w.0
    }

    /// Decodes canonical bytes; `None` for anything else (`Malformed`). Values are not checked here: a non-finite
    /// float decodes, and is refused as `Invalid` when applied.
    pub fn decode(bytes: &[u8]) -> Option<Input> {
        let mut r = R { b: bytes, row_bytes: 0 };
        let input = r.input()?;
        r.b.is_empty().then_some(input)
    }
}

/// Where an entry goes in its frame (8.1, "Order within a frame"): 0 for a hand input, 1 for everything else. A
/// frame applies its entries stably sorted by this, so all hand inputs stamped `f` apply before `f`'s grabs,
/// releases, pushes and edits (8.1, "The apply order of a frame"). Read from the class and the payload's first byte
/// alone, never world state, so it is total and costs no decode. Exactly two phases; changing this is a world-format
/// version change.
pub fn input_phase(entry: &SignedEntry) -> u8 {
    let e = &entry.entry;
    u8::from(!(e.class == Class::Input && e.payload.first() == Some(&TAG_HAND)))
}

// ---- settings -----------------------------------------------------------------------------------------------------

/// The world settings inputs use (8.1), each an integer within its bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputSettings {
    /// How long a claim lasts after its newest renewal, in milliseconds (100 ..= 5,000).
    pub claim_ms: u32,
    /// How long a holder waits after a claim expires before claiming the object again, in ms (0 ..= 2,000).
    pub cooldown_ms: u32,
    /// The most impulse one member's pushes may sum to in one second, in mN·s (0 ..= 1,000,000; 0: no pushes).
    pub push_budget: u32,
    /// A hand's reach, from its target to an object's bounding sphere, in millimetres (0 ..= 2,000) (7.2).
    pub reach_mm: u32,
    /// The reach slack (7.2), also added to an object's bounding radius for a push's `at`, in mm (0 ..= 1,000).
    pub slack_mm: u32,
}

impl Default for InputSettings {
    fn default() -> Self {
        InputSettings { claim_ms: 500, cooldown_ms: 166, push_budget: 2_000, reach_mm: 100, slack_mm: 50 }
    }
}

impl InputSettings {
    /// Whether every setting is within its bound.
    pub fn valid(&self) -> bool {
        (100..=5_000).contains(&self.claim_ms)
            && self.cooldown_ms <= 2_000
            && self.push_budget <= 1_000_000
            && self.reach_mm <= 2_000
            && self.slack_mm <= 1_000
    }
}

/// `ms` as frames at `hz`: `ceil(ms × hz / 1000)`, in integer arithmetic (8.1).
pub fn frames(ms: u32, hz: u32) -> u64 {
    (u64::from(ms) * u64::from(hz)).div_ceil(1000)
}

// ---- the world tables ---------------------------------------------------------------------------------------------

/// A hand's pending pack (8.1): what `end_frame` still writes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pack {
    pub(crate) stamp: Frame,
    pub(crate) poses: Vec<HandPose>,
    pub(crate) hold: u8,
}

/// The input tables (8.1), all in the root: cooldowns, pending hand packs, and each member's pushes in the last
/// second (stamp and `|impulse|`, oldest first).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct InputTables {
    pub(crate) cooldowns: BTreeMap<(MemberKey, ObjectId), Frame>,
    pub(crate) packs: BTreeMap<ObjectId, Pack>,
    pub(crate) pushes: BTreeMap<MemberKey, VecDeque<(Frame, f64)>>,
}

fn invalid() -> Reason {
    Reason::Invalid { field: None }
}

fn finite(v: &[f32]) -> bool {
    v.iter().all(|x| x.is_finite())
}

/// `|v|`, from the `f32` components, computed in `f64` (correctly rounded, so the same on every machine).
fn norm(v: [f32; 3]) -> f64 {
    let [x, y, z] = v.map(f64::from);
    (x * x + y * y + z * z).sqrt()
}

/// A pose's values (8.1): a finite rotation whose norm is within `1 ± 1e-3`, and a grip in [0, 1]. Its `Pos` is in
/// range by construction.
fn pose_valid(p: &HandPose) -> bool {
    let n2: f64 = p.rot.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    finite(&p.rot) && (n2.sqrt() - 1.0).abs() <= 1e-3 && (0.0..=1.0).contains(&p.grip)
}

impl World {
    pub fn input_settings(&self) -> InputSettings {
        self.input_settings
    }

    /// Sets the input settings (trusted: genesis, governance). Refused, changing nothing, outside their bounds.
    pub fn set_input_settings(&mut self, s: InputSettings) -> Result<(), InputSettings> {
        if !s.valid() {
            return Err(s);
        }
        self.input_settings = s;
        Ok(())
    }

    /// The author's cooldown on `object`, if one is in the table: the frame it ends at.
    pub fn cooldown(&self, member: MemberKey, object: ObjectId) -> Option<Frame> {
        self.inputs.cooldowns.get(&(member, object)).copied()
    }

    /// A hand's pending pack (8.1): its stamp, poses and `hold`, while `end_frame` still has frames of it to cover.
    pub fn hand_pack(&self, hand: ObjectId) -> Option<(Frame, &[HandPose], u8)> {
        self.inputs.packs.get(&hand).map(|p| (p.stamp, p.poses.as_slice(), p.hold))
    }

    /// A claim's expiry from `now`, at the rate in effect.
    fn claim_until(&self, now: Frame) -> Frame {
        Frame(now.0.saturating_add(frames(self.input_settings.claim_ms, self.rate.hz())))
    }

    /// Applies one input entry at `frame`, its stamp frame (8.1). Total: any entry gives an outcome, and a refused one
    /// changes nothing. An applied input creates nothing and has no inverse.
    pub fn apply_input(&mut self, frame: Frame, entry: &SignedEntry) -> (Outcome, Footprint) {
        let e = &entry.entry;
        let input = match (e.class, Input::decode(&e.payload)) {
            (Class::Input, Some(i)) => i,
            _ => return (Outcome::Refused { op: None, reason: Reason::Malformed }, Footprint::default()),
        };
        let mut fp = Footprint::default();
        let mut notes = Vec::new();
        let r = match input {
            Input::Hand { hand, poses, hold } => self.hand_input(frame, e.author, hand, poses, hold, &mut fp),
            Input::Grab { object, hand } => self.grab(frame, e.author, object, hand),
            Input::Release { object } => {
                self.release(e.author, object, &mut notes);
                Ok(())
            }
            Input::Push { object, impulse, at } => self.push(frame, e.author, object, impulse, at, &mut fp),
        };
        match r {
            Ok(()) => (Outcome::Applied { created: Vec::new(), overwrote: Vec::new(), notes, inverse: None }, fp),
            Err(reason) => (Outcome::Refused { op: None, reason }, Footprint::default()),
        }
    }

    /// `Unknown`, or `Deleted` if a tombstone names the object.
    fn absent(&self, id: ObjectId) -> Reason {
        match self.tombstones.get(&id) {
            Some(t) => Reason::Deleted { object: id, by: t.by },
            None => Reason::Unknown(id),
        }
    }

    /// The author's own hand's row: `Unknown` or `Deleted` without the object, `NotYourHand` without a `Hand` row
    /// owned by `author`.
    fn own_hand(&self, author: MemberKey, hand: ObjectId) -> Result<Hand, Reason> {
        if !self.store.exists(hand) {
            return Err(self.absent(hand));
        }
        match self.store.get::<Hand>(hand) {
            Some(h) if h.owner == author => Ok(*h),
            _ => Err(Reason::NotYourHand),
        }
    }

    fn hand_input(
        &mut self,
        now: Frame,
        author: MemberKey,
        hand: ObjectId,
        poses: Vec<HandPose>,
        hold: u8,
        fp: &mut Footprint,
    ) -> Result<(), Reason> {
        if !poses.iter().all(pose_valid) {
            return Err(invalid());
        }
        let old = self.own_hand(author, hand)?;
        // A hand with no `Transform`, or whose frame has no world pose, cannot take a target: `Invalid`, not
        // `OutOfRange` (@chunks-reviewer's Low 1). `OutOfRange` is then only a pose its frame cannot hold.
        let framed = match self.store.frame_of(hand) {
            Some(f) => self.store.world_transform(f).is_some(),
            None => true,
        };
        if !self.store.has::<pe_sim::Transform>(hand) || !framed {
            return Err(invalid());
        }
        let first = poses[0];
        let span = poses.len() as u32 + u32::from(hold);
        let row = Hand {
            stamp: now.0,
            span,
            prev_stamp: old.stamp,
            prev_span: old.span,
            has_prev: u8::from(old.span > 0),
            grip: first.grip,
            engaged: u8::from(first.engaged),
            ..old
        };
        self.store.insert(hand, row).map_err(|_| invalid())?;
        if !self.place(hand, first.pos, first.rot) {
            let _ = self.store.insert(hand, old);
            return Err(Reason::OutOfRange);
        }
        // Implicit renewal (8.1): the newest pose engaged renews every claim this hand holds.
        if poses[poses.len() - 1].engaged && self.claims.values().any(|c| c.hand == hand && c.member == author) {
            let until = self.claim_until(now);
            for c in Arc::make_mut(&mut self.claims).values_mut() {
                if c.hand == hand && c.member == author {
                    c.until = c.until.max(until);
                }
            }
        }
        Arc::make_mut(&mut self.inputs).packs.insert(hand, Pack { stamp: now, poses, hold });
        fp.hands.push(hand);
        Ok(())
    }

    fn grab(&mut self, now: Frame, author: MemberKey, object: ObjectId, hand: Option<ObjectId>) -> Result<(), Reason> {
        if !self.store.exists(object) {
            return Err(self.absent(object));
        }
        if let Some(h) = hand {
            self.own_hand(author, h)?;
        }
        if !self.store.has::<Body>(object) {
            return Err(invalid());
        }
        if let Some(&until) = self.inputs.cooldowns.get(&(author, object))
            && until > now
        {
            return Err(Reason::Cooldown { until });
        }
        let until = self.claim_until(now);
        // Renewing the author's own unexpired claim checks only the hand and the cooldown (8.1); a renewal that moves
        // the claim to another hand is a new hold for that hand, so its reach is checked, but not `Held`
        // (@design-reviewer's Low 1).
        if let Some(c) = self.claims.get(&object)
            && c.member == author
            && c.until > now
        {
            if let Some(h) = hand
                && h != c.hand
            {
                self.in_reach(h, object)?;
            }
            let c = Arc::make_mut(&mut self.claims).get_mut(&object).expect("checked above");
            c.until = c.until.max(until);
            if let Some(h) = hand {
                c.hand = h;
            }
            return Ok(());
        }
        // Another member's claim on the object or anything in its subtree; the first in id order names the holder.
        for (&held, c) in self.claims.iter() {
            if c.member != author && c.until > now && (held == object || self.is_ancestor(object, held)) {
                return Err(Reason::Held { by: c.member });
            }
        }
        if let Some(h) = hand {
            self.in_reach(h, object)?;
        }
        let claim = Claim { hand: hand.unwrap_or(ObjectId::NONE), member: author, since: now, until };
        Arc::make_mut(&mut self.claims).insert(object, claim);
        Ok(())
    }

    fn release(&mut self, author: MemberKey, object: ObjectId, notes: &mut Vec<Note>) {
        if self.claims.get(&object).is_some_and(|c| c.member == author) {
            Arc::make_mut(&mut self.claims).remove(&object);
        } else {
            notes.push(Note::NoEffect { op: 0 });
        }
    }

    fn push(
        &mut self,
        now: Frame,
        author: MemberKey,
        object: ObjectId,
        impulse: [f32; 3],
        at: Option<Pos>,
        fp: &mut Footprint,
    ) -> Result<(), Reason> {
        if !finite(&impulse) {
            return Err(invalid());
        }
        if !self.store.exists(object) {
            return Err(self.absent(object));
        }
        if !self.store.has::<Body>(object) {
            return Err(invalid());
        }
        if let Some(c) = self.claims.get(&object)
            && c.member != author
            && c.until > now
        {
            return Err(Reason::Held { by: c.member });
        }
        if let Some(at) = at {
            let centre = self.placed(object).ok_or_else(invalid)?.pos;
            let radius = self.bounds(object).iter().map(|&(c, r)| c.dist2(&centre).sqrt() + r).fold(0.0, f64::max);
            let bound = radius + f64::from(self.input_settings.slack_mm) / 1000.0;
            if at.dist2(&centre) > bound * bound {
                return Err(invalid());
            }
        }
        let budget = self.input_settings.push_budget;
        let hz = u64::from(self.rate.hz());
        let n = norm(impulse);
        let spent: f64 = self
            .inputs
            .pushes
            .get(&author)
            .map_or(0.0, |q| q.iter().filter(|(s, _)| s.0 + hz > now.0).map(|&(_, m)| m).sum());
        if budget == 0 || (spent + n) * 1000.0 > f64::from(budget) {
            return Err(invalid());
        }
        Arc::make_mut(&mut self.inputs).pushes.entry(author).or_default().push_back((now, n));
        fp.pushes.push(Push { object, impulse, at, author });
        Ok(())
    }

    /// `OutOfReach` unless `hand`'s target is within reach plus slack of one of `object`'s bounding spheres (7.2).
    fn in_reach(&self, hand: ObjectId, object: ObjectId) -> Result<(), Reason> {
        let target = self.store.world_transform(hand).ok_or_else(invalid)?.pos;
        let s = self.input_settings;
        let reach = f64::from(s.reach_mm + s.slack_mm) / 1000.0;
        let within = self.bounds(object).iter().any(|&(c, r)| c.dist2(&target) <= (reach + r) * (reach + r));
        if within { Ok(()) } else { Err(Reason::OutOfReach) }
    }

    /// Whether `a` is a strict ancestor of `x`.
    fn is_ancestor(&self, a: ObjectId, x: ObjectId) -> bool {
        let mut at = x;
        let mut d = 0;
        while let Some(p) = self.store.parent(at) {
            if p == a {
                return true;
            }
            d += 1;
            if d > crate::world::DEPTH_LIMIT_MAX {
                return false;
            }
            at = p;
        }
        false
    }

    /// An object's world pose: its own, or its frame's if it has no `Transform` (3.3).
    fn placed(&self, id: ObjectId) -> Option<pe_sim::WorldTransform> {
        self.store.world_transform(id).or_else(|| self.store.frame_of(id).and_then(|f| self.store.world_transform(f)))
    }

    /// A body's bounding spheres (7.2): for its own `Shape` and each compound child's (a descendant reached without
    /// passing another `Body`), the shape's world centre and a radius covering it, scaled by the largest axis of its
    /// world scale. A box, mesh or hull is bounded by its `size` read as half extents.
    fn bounds(&self, body: ObjectId) -> Vec<(Pos, f64)> {
        let mut out = Vec::new();
        let mut stack = vec![body];
        while let Some(x) = stack.pop() {
            if let (Some(s), Some(w)) = (self.store.get::<Shape>(x), self.placed(x)) {
                let k = w.scale.iter().fold(0.0f64, |m, v| m.max(f64::from(v.abs())));
                let [a, b, c] = s.size.map(|v| f64::from(v.abs()));
                let r = match s.kind {
                    shape_kind::SPHERE => a * k,
                    shape_kind::CAPSULE => (a + b) * k,
                    _ => (a * a + b * b + c * c).sqrt() * k,
                };
                if r.is_finite() {
                    out.push((w.pos, r));
                }
            }
            stack.extend(self.store.children(x).filter(|&c| !self.store.has::<Body>(c)));
        }
        out
    }

    /// The input part of [`World::end_frame`] (8.1): cooldowns that ended drop; claims whose `until <= f + 1` end, each
    /// holder getting a cooldown on its object; pushes older than a second leave the windows; then each pending pack
    /// writes its pose for `f + 1`, and a pack past its coverage, or whose hand is gone, is dropped.
    pub(crate) fn end_frame_inputs(&mut self, f: Frame) {
        let g = f.next();
        let cool = frames(self.input_settings.cooldown_ms, self.rate.hz());
        let hz = u64::from(self.rate.hz());
        if self.inputs.cooldowns.values().any(|&u| u <= g) {
            Arc::make_mut(&mut self.inputs).cooldowns.retain(|_, u| *u > g);
        }
        if self.claims.values().any(|c| c.until <= g) {
            let ended: Vec<(ObjectId, MemberKey)> =
                self.claims.iter().filter(|(_, c)| c.until <= g).map(|(&o, c)| (o, c.member)).collect();
            Arc::make_mut(&mut self.claims).retain(|_, c| c.until > g);
            if cool > 0 {
                let t = Arc::make_mut(&mut self.inputs);
                for (o, m) in ended {
                    t.cooldowns.insert((m, o), Frame(g.0.saturating_add(cool)));
                }
            }
        }
        if self.inputs.pushes.values().any(|q| q.front().is_some_and(|(s, _)| s.0 + hz <= g.0)) {
            let t = Arc::make_mut(&mut self.inputs);
            for q in t.pushes.values_mut() {
                while q.front().is_some_and(|(s, _)| s.0 + hz <= g.0) {
                    q.pop_front();
                }
            }
            t.pushes.retain(|_, q| !q.is_empty());
        }
        if self.inputs.packs.is_empty() {
            return;
        }
        let mut writes = Vec::new();
        let mut done = Vec::new();
        for (&hand, p) in self.inputs.packs.iter() {
            let k = g.0.saturating_sub(p.stamp.0);
            let len = p.poses.len() as u64;
            if k >= len + u64::from(p.hold) || !self.store.has::<Hand>(hand) {
                done.push(hand);
            } else if k < len {
                writes.push((hand, p.poses[k as usize]));
            }
        }
        // A pose that cannot be placed (its frame moved so it is out of range, or the hand lost its `Transform`)
        // drops the pack, writing nothing of that pose: the hand keeps the last pose and row actually written, as past
        // coverage (@design-reviewer's ruling on @chunks-reviewer's Low 2).
        for (hand, pose) in writes {
            if !self.place(hand, pose.pos, pose.rot) {
                done.push(hand);
                continue;
            }
            if let Some(&row) = self.store.get::<Hand>(hand) {
                let row = Hand { grip: pose.grip, engaged: u8::from(pose.engaged), ..row };
                let _ = self.store.insert(hand, row);
            }
        }
        if !done.is_empty() {
            let t = Arc::make_mut(&mut self.inputs);
            for h in done {
                t.packs.remove(&h);
            }
        }
    }
}
