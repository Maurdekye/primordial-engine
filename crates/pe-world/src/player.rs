//! Player bodies (world-model.md 8.2; friend-test locomotion, piece L2): the `Move` input that drives a member's body,
//! and `BodyHand`, a hand pack in its body's frame. Only a world whose preset gives members a body has `Player` rows
//! ([`crate::components::register_player`]); every table here is empty in any other world, so its root is the one it
//! always had.
//!
//! - **`Move`** carries the raw locomotion samples (the trinket seam: never a derived velocity),
//!   packed and held like a hand pack. Only the body's owner may send it. The first sample is written at the stamp
//!   frame; [`World::end_frame`] writes the later ones, and when the pack has run out the stick goes to zero.
//! - **`jump`** is 1 only on the frame the press count rose, so a held or re-sent sample never jumps twice.
//! - **`prev_head` and `prev_heading`** are what the step used last frame: `end_frame` rolls them forward, so the
//!   mover takes the room-scale step as a difference, from rows alone.
//! - **`BodyHand`** poses are offsets and rotations in the body's frame (the body never turns, so its frame is the
//!   world's axes at its feet). The sender computes them from the heading and head pose it put in that frame's Move;
//!   receivers only read the result, so no trig runs here.

use std::sync::Arc;

use pe_core::{Frame, MemberKey, ObjectId, Offset};
use pe_seams::Reason;

use crate::apply::Footprint;
use crate::components::{Hand, Player};
use crate::world::World;

/// The most samples one `Move` may carry, as for a hand pack (8.1).
pub const MAX_SAMPLES: usize = crate::input::MAX_POSES;

/// How far a body's hand may be placed from the body's feet, per axis, in metres [I]: tracking is bounded, so a
/// larger offset is a bad input, not a long arm.
pub const MAX_HAND_OFFSET: f64 = 4.0;

/// How far the head may be from the rig's origin, per axis, in metres [I]: a room-scale space, not a world.
pub const MAX_HEAD_OFFSET: f32 = 50.0;

/// The most [`Player::moved`] can be, in metres: the reach slack a body's last step adds is clamped to it, so a
/// respawn or a bad mover step can't open a hand's reach. About the mover's top speed plus a fall's over one step
/// (@chunks-reviewer; [I], @vdw-physics's numbers win).
pub const MAX_MOVED: f64 = 0.5;

/// One locomotion sample (pe-seams' `MoveInput`, quantised): what the step reads for one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MoveSample {
    /// The stick after its dead zone, x right and y forward, in steps of 1/127 (-127 ..= 127).
    pub stick: [i8; 2],
    /// The yaw the stick is relative to, in the rig frame (2^32 is one turn).
    pub stick_yaw: u32,
    /// The cumulative heading the player asked for (2^32 is one turn, positive turning left, 0 facing -Z).
    pub heading: u32,
    /// Jump presses so far, wrapping.
    pub jumps: u32,
    /// The head's pose in the rig frame.
    pub head_pos: [f32; 3],
    pub head_rot: [f32; 4],
    pub tracked: bool,
}

impl MoveSample {
    /// The stick as a sample carries it (@xr-eng): its length clamped to 1, then each axis rounded to
    /// steps of 1/127, symmetric, never -128. A non-finite axis is 0.
    pub fn quantise_stick(stick: [f32; 2]) -> [i8; 2] {
        let s = stick.map(|v| if v.is_finite() { f64::from(v) } else { 0.0 });
        let len = (s[0] * s[0] + s[1] * s[1]).sqrt();
        let k = if len > 1.0 { 1.0 / len } else { 1.0 };
        s.map(|v| (v * k * 127.0).round().clamp(-127.0, 127.0) as i8)
    }
}

/// One pose of a `BodyHand` pack: the hand's target in its body's frame, its grip and whether it is engaged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyHandPose {
    pub offset: Offset,
    pub rot: [f32; 4],
    pub grip: f32,
    pub engaged: bool,
}

/// A pending pack: what `end_frame` still writes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pending<T> {
    pub(crate) stamp: Frame,
    pub(crate) items: Vec<T>,
    pub(crate) hold: u8,
}

/// The fewest bytes one encoded sample takes.
pub(crate) const MIN_SAMPLE_BYTES: usize = 2 + 4 + 4 + 4 + 12 + 16 + 1;
/// The fewest bytes one encoded body-hand pose takes.
pub(crate) const MIN_BODY_POSE_BYTES: usize = 24 + 16 + 4 + 1;

fn unit(rot: &[f32; 4]) -> bool {
    let n2: f64 = rot.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    rot.iter().all(|v| v.is_finite()) && (n2.sqrt() - 1.0).abs() <= 1e-3
}

pub(crate) fn sample_valid(s: &MoveSample) -> bool {
    s.stick.iter().all(|&v| v != i8::MIN)
        && s.head_pos.iter().all(|v| v.is_finite() && v.abs() <= MAX_HEAD_OFFSET)
        && unit(&s.head_rot)
}

pub(crate) fn body_pose_valid(p: &BodyHandPose) -> bool {
    p.offset.0.iter().all(|v| v.is_finite() && v.abs() <= MAX_HAND_OFFSET)
        && unit(&p.rot)
        && (0.0..=1.0).contains(&p.grip)
}

/// `row` with `s` written as the sample the step reads now. The first sample a body ever gets also sets what the
/// step "used last frame", so the start is not a room-scale step, and adopts the press count without a jump.
fn with_sample(row: Player, s: &MoveSample) -> Player {
    let first = row.span == 0 && row.stamp == 0;
    let rose = !first && s.jumps != row.jumps;
    let mut r = Player {
        stick: s.stick,
        stick_yaw: s.stick_yaw,
        heading: s.heading,
        jumps: s.jumps,
        head_pos: s.head_pos,
        head_rot: s.head_rot,
        tracked: u8::from(s.tracked),
        jump: row.jump | u8::from(rose),
        ..row
    };
    if first {
        r.prev_head = s.head_pos;
        r.prev_heading = s.heading;
    }
    r
}

/// One player, as the step reads it (S2's `players()`): the body, its owner and its row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerRow {
    pub body: ObjectId,
    pub owner: MemberKey,
    pub player: Player,
}

impl World {
    /// Every player body with its row, in id order: what the mover reads each frame.
    pub fn players(&self) -> Vec<PlayerRow> {
        if self.store.registry().get(crate::components::ids::PLAYER).is_none() {
            return Vec::new();
        }
        let mut v: Vec<PlayerRow> =
            self.store.iter::<Player>().map(|(body, p)| PlayerRow { body, owner: p.owner, player: *p }).collect();
        v.sort_unstable_by_key(|p| p.body);
        v
    }

    /// `member`'s player body, if the preset gave it one (8.2).
    pub fn body_of(&self, member: MemberKey) -> Option<ObjectId> {
        self.store.registry().get(crate::components::ids::PLAYER)?;
        self.store.iter::<Player>().filter(|(_, p)| p.owner == member).map(|(b, _)| b).min()
    }

    /// The author's own body's row: `Unknown` or `Deleted` without the object, `NotYourHand` without a `Player` row
    /// owned by `author` [I: a body is the member's own as a hand is; pe-seams has no reason of its own for it yet].
    fn own_body(&self, author: MemberKey, body: ObjectId) -> Result<Player, Reason> {
        if !self.store.exists(body) {
            return Err(self.absent(body));
        }
        match self.store.get::<Player>(body) {
            Some(p) if p.owner == author => Ok(*p),
            _ => Err(Reason::NotYourHand),
        }
    }

    /// A `Move` input (8.2): the owner's samples for `now` onwards.
    pub(crate) fn move_input(
        &mut self,
        now: Frame,
        author: MemberKey,
        body: ObjectId,
        samples: Vec<MoveSample>,
        hold: u8,
        fp: &mut Footprint,
    ) -> Result<(), Reason> {
        if samples.is_empty() || !samples.iter().all(sample_valid) {
            return Err(Reason::Invalid { field: None });
        }
        let old = self.own_body(author, body)?;
        let row =
            Player { stamp: now.0, span: samples.len() as u32 + u32::from(hold), ..with_sample(old, &samples[0]) };
        self.store.insert(body, row).map_err(|_| Reason::Invalid { field: None })?;
        let t = Arc::make_mut(&mut self.inputs);
        if samples.len() > 1 {
            t.moves.insert(body, Pending { stamp: now, items: samples, hold });
        } else {
            t.moves.remove(&body);
        }
        fp.players.push(body);
        Ok(())
    }

    /// A `BodyHand` input (8.2): the owner's hand poses in its body's frame, for `now` onwards.
    pub(crate) fn body_hand_input(
        &mut self,
        now: Frame,
        author: MemberKey,
        hand: ObjectId,
        poses: Vec<BodyHandPose>,
        hold: u8,
        fp: &mut Footprint,
    ) -> Result<(), Reason> {
        if poses.is_empty() || !poses.iter().all(body_pose_valid) {
            return Err(Reason::Invalid { field: None });
        }
        let old = self.own_hand(author, hand)?;
        // Only a hand whose parent is a player body takes body-frame poses; any other hand takes `Hand` inputs.
        if !self.store.parent(hand).is_some_and(|b| self.store.has::<Player>(b))
            || !self.store.has::<pe_sim::Transform>(hand)
        {
            return Err(Reason::Invalid { field: None });
        }
        let first = poses[0];
        let row = Hand {
            stamp: now.0,
            span: poses.len() as u32 + u32::from(hold),
            prev_stamp: old.stamp,
            prev_span: old.span,
            has_prev: u8::from(old.span > 0),
            grip: first.grip,
            engaged: u8::from(first.engaged),
            ..old
        };
        if !self.place_in_body(hand, &first) {
            return Err(Reason::OutOfRange);
        }
        self.store.insert(hand, row).map_err(|_| Reason::Invalid { field: None })?;
        if poses[poses.len() - 1].engaged {
            self.renew_claims_of(author, hand, now);
        }
        let t = Arc::make_mut(&mut self.inputs);
        t.packs.remove(&hand);
        if poses.len() > 1 {
            t.body_packs.insert(hand, Pending { stamp: now, items: poses, hold });
        } else {
            t.body_packs.remove(&hand);
        }
        fp.hands.push(hand);
        Ok(())
    }

    /// Writes `hand`'s local transform from a body-frame pose, keeping its scale. False, writing nothing, if the pose
    /// is out of a child's range.
    fn place_in_body(&mut self, hand: ObjectId, p: &BodyHandPose) -> bool {
        let Some(old) = self.store.get::<pe_sim::Transform>(hand).copied() else {
            return false;
        };
        let n: f64 = p.rot.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>().sqrt();
        let rot = p.rot.map(|v| (f64::from(v) / n) as f32);
        let local = pe_sim::Transform { chunk: [0; 3], pos: p.offset.0, rot, ..old };
        local.validate(true).is_ok() && self.store.insert(hand, local).is_ok()
    }

    /// The end of `f` for player bodies (8.2), before the next frame's inputs: what the step used becomes `prev_*`,
    /// the one-shot jump clears, each pending pack writes frame `f + 1`'s item, and a Move pack that has run out
    /// leaves the stick at zero. Only rows that change are written.
    pub(crate) fn end_frame_players(&mut self, f: Frame) {
        let g = f.next();
        if !self.inputs.body_packs.is_empty() {
            let mut writes = Vec::new();
            let mut done = Vec::new();
            for (&hand, p) in self.inputs.body_packs.iter() {
                let k = g.0.saturating_sub(p.stamp.0);
                let len = p.items.len() as u64;
                if k >= len + u64::from(p.hold) || !self.store.has::<Hand>(hand) {
                    done.push(hand);
                } else if k < len {
                    writes.push((hand, p.items[k as usize]));
                }
            }
            for (hand, pose) in writes {
                if !self.place_in_body(hand, &pose) {
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
                    t.body_packs.remove(&h);
                }
            }
        }
        if self.store.registry().get(crate::components::ids::PLAYER).is_none() {
            return;
        }
        let rows: Vec<(ObjectId, Player)> = self.store.iter::<Player>().map(|(b, p)| (b, *p)).collect();
        let mut done = Vec::new();
        for (body, old) in rows {
            let mut row = Player { prev_head: old.head_pos, prev_heading: old.heading, jump: 0, ..old };
            if let Some(t) = self.store.world_transform(body) {
                let feet = (t.pos - pe_core::Pos::ORIGIN).0;
                let d: f64 = feet.iter().zip(old.prev_feet).map(|(a, b)| (a - b) * (a - b)).sum::<f64>().sqrt();
                row.moved = d.min(MAX_MOVED) as f32;
                row.prev_feet = feet;
            }
            if old.span > 0 {
                let k = g.0.saturating_sub(old.stamp);
                match self.inputs.moves.get(&body) {
                    Some(p) if (k as usize) < p.items.len() => row = with_sample(row, &p.items[k as usize]),
                    _ => {}
                }
                if k >= u64::from(old.span) {
                    row.stick = [0, 0];
                    if self.inputs.moves.contains_key(&body) {
                        done.push(body);
                    }
                }
            }
            if row != old {
                let _ = self.store.insert(body, row);
            }
        }
        if !done.is_empty() {
            let t = Arc::make_mut(&mut self.inputs);
            for b in done {
                t.moves.remove(&b);
            }
        }
    }
}

// ---- the pending packs in the hand-packs table (world-model.md 6.6) ----------------------------------------------

/// The key suffix of a `BodyHand` pack in the hand-packs table: the hand's id then this byte, so a world without
/// player bodies has exactly the table it always had.
pub(crate) const KEY_BODY_HAND: u8 = 1;
/// The key suffix of a `Move` pack: the body's id then this byte.
pub(crate) const KEY_MOVE: u8 = 2;

fn head(stamp: Frame, hold: u8, n: usize) -> Vec<u8> {
    let mut v = stamp.0.to_le_bytes().to_vec();
    v.push(hold);
    v.extend_from_slice(&(n as u32).to_le_bytes());
    v
}

pub(crate) fn encode_move_pack(p: &Pending<MoveSample>) -> Vec<u8> {
    let mut v = head(p.stamp, p.hold, p.items.len());
    for s in &p.items {
        v.extend_from_slice(&[s.stick[0] as u8, s.stick[1] as u8]);
        for x in [s.stick_yaw, s.heading, s.jumps] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        for x in s.head_pos.iter().chain(&s.head_rot) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.push(u8::from(s.tracked));
    }
    v
}

pub(crate) fn encode_body_pack(p: &Pending<BodyHandPose>) -> Vec<u8> {
    let mut v = head(p.stamp, p.hold, p.items.len());
    for q in &p.items {
        for x in q.offset.0 {
            v.extend_from_slice(&x.to_le_bytes());
        }
        for x in q.rot.iter().chain([&q.grip]) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.push(u8::from(q.engaged));
    }
    v
}

/// A strict little-endian reader for the two encodings above.
struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (a, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.arr::<1>()?[0])
    }
    fn flag(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.arr()?))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.arr()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.arr()?))
    }
    fn head(&mut self, max: usize) -> Option<(Frame, u8, usize)> {
        let stamp = Frame(u64::from_le_bytes(self.arr()?));
        let hold = self.u8()?;
        let n = self.u32()? as usize;
        (1..=max).contains(&n).then_some((stamp, hold, n))
    }
}

pub(crate) fn decode_move_pack(b: &[u8]) -> Option<Pending<MoveSample>> {
    let mut r = Rd(b);
    let (stamp, hold, n) = r.head(MAX_SAMPLES)?;
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        let stick = [r.u8()? as i8, r.u8()? as i8];
        let (stick_yaw, heading, jumps) = (r.u32()?, r.u32()?, r.u32()?);
        let head_pos = [r.f32()?, r.f32()?, r.f32()?];
        let head_rot = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
        let tracked = r.flag()?;
        items.push(MoveSample { stick, stick_yaw, heading, jumps, head_pos, head_rot, tracked });
    }
    r.0.is_empty().then_some(Pending { stamp, items, hold })
}

pub(crate) fn decode_body_pack(b: &[u8]) -> Option<Pending<BodyHandPose>> {
    let mut r = Rd(b);
    let (stamp, hold, n) = r.head(crate::input::MAX_POSES)?;
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        let offset = Offset([r.f64()?, r.f64()?, r.f64()?]);
        let rot = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
        let grip = r.f32()?;
        let engaged = r.flag()?;
        items.push(BodyHandPose { offset, rot, grip, engaged });
    }
    r.0.is_empty().then_some(Pending { stamp, items, hold })
}
