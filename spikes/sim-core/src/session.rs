//! Session protocol core for the phase-1 spikes, revised after design review C1/C2/H4/H5 and the
//! user rulings of 2026-10-30: perturbations travel as **state + canonical frame**; same-frame
//! conflicts use a stable, non-grindable tie-break; programmable systems run locally and sync
//! only on perturbation; R14 cross-world transfers.
//!
//! * [`Entry`]: one perturbation (touch, hand target, transfer step, …). Its identity is the hash
//!   of its content, never `(member, seq)` alone. Each member's entries form a hash chain, so two
//!   different entries with the same `(member, seq)` are a transferable proof of equivocation.
//! * [`Ack`]: every member periodically states its *horizon* (it will author nothing stamped at
//!   or before it), the entries it holds (per author: contiguous seq and id), and the entries that
//!   reached it late. The **confirmed frame** is computed from acks only, so admission never
//!   depends on local arrival time (C1).
//! * [`WorldSim`]: one world's deterministic simulation with keyframe snapshots, rollback to the
//!   earliest changed frame, carried-state application, claims, and R14 escrow/arrival tables.
//!
//! Frame convention as in the rest of sim-core: the state at `frame` is the state at the *start*
//! of that frame; `hashes[f]` is the hash after frame `f` was stepped. `conf` is exclusive: every
//! frame `< conf` is confirmed and can never be rolled back.

use std::collections::{BTreeMap, BTreeSet};

use box3d_rust::body::{
    body_apply_linear_impulse_to_center, body_get_angular_velocity, body_get_linear_velocity,
    body_get_transform, body_set_angular_velocity, body_set_linear_velocity,
    body_set_target_transform, body_set_transform, create_body, destroy_body,
};
use box3d_rust::geometry::{Capsule, Sphere};
use box3d_rust::hull::make_box_hull;
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::{Pos, Quat, Transform, Vec3, QUAT_IDENTITY};
use box3d_rust::recording::{
    deserialize_into_shell, hash_world_state, serialize_world, GeometryRegistry, RecBuffer,
    RegistrySlot,
};
use box3d_rust::shape::{create_capsule_shape, create_hull_shape, create_sphere_shape};
use box3d_rust::types::{default_body_def, default_shape_def, BodyType};
use box3d_rust::world::World;

use crate::{fnv_u64s, splitmix64, CLAIM_FRAMES, SUBSTEPS};
pub use crate::provenance::{
    birth_world, chain_hash, check_chain, fact_committed, fork_proof, ChainError, EpochBook, EpochCerts, Export, ForkProof, Hop, ImportPolicy, Seen,
};

pub type MemberId = u32;
pub type EntryId = u64;
/// Member 0 is world logic: its actions are computed identically on every peer, never networked,
/// and never take claims (review M3).
pub const LOGIC: MemberId = 0;

// ---- object state ------------------------------------------------------------------------------

/// Exact bit pattern of a rigid body's sim state: position, rotation, linear and angular velocity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyState(pub [u32; 13]);

impl BodyState {
    pub fn read(world: &World, id: BodyId) -> Self {
        let t = body_get_transform(world, id);
        let v = body_get_linear_velocity(world, id);
        let w = body_get_angular_velocity(world, id);
        BodyState([
            t.p.x.to_bits(), t.p.y.to_bits(), t.p.z.to_bits(),
            t.q.v.x.to_bits(), t.q.v.y.to_bits(), t.q.v.z.to_bits(), t.q.s.to_bits(),
            v.x.to_bits(), v.y.to_bits(), v.z.to_bits(),
            w.x.to_bits(), w.y.to_bits(), w.z.to_bits(),
        ])
    }
    fn f(&self, i: usize) -> f32 {
        f32::from_bits(self.0[i])
    }
    pub fn pos(&self) -> Pos {
        Pos { x: self.f(0), y: self.f(1), z: self.f(2) }
    }
    pub fn rot(&self) -> Quat {
        Quat { v: Vec3 { x: self.f(3), y: self.f(4), z: self.f(5) }, s: self.f(6) }
    }
    pub fn lin(&self) -> Vec3 {
        Vec3 { x: self.f(7), y: self.f(8), z: self.f(9) }
    }
    pub fn ang(&self) -> Vec3 {
        Vec3 { x: self.f(10), y: self.f(11), z: self.f(12) }
    }
    /// Overwrite the body's state with these exact bits.
    pub fn write(&self, world: &mut World, id: BodyId) {
        body_set_transform(world, id, self.pos(), self.rot());
        body_set_linear_velocity(world, id, self.lin());
        body_set_angular_velocity(world, id, self.ang());
    }
    pub fn hash(&self) -> u64 {
        fnv_u64s(self.0.iter().map(|&b| b as u64))
    }
}

/// Everything another world needs to recreate an object (R14). `kind` selects the shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjRecord {
    pub global: u64,
    pub kind: u8,
    pub state: BodyState,
    pub src_world: u32,
    pub src_frame: u64,
    /// P2-5: the transfer that brought the object into `src_world` (0: born there).
    pub via: u64,
}

// ---- entries, acks, messages ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// Impulse on an object. `grab` = a held grab, which renews its claim; a plain impulse claims
    /// once and never renews (review M3).
    Touch { obj: u32, impulse: [f32; 3], grab: bool },
    /// Kinematic hand target, in millimetres (quantized so every peer sees the same bits), to be
    /// reached `span` frames after the stamp (1 normally; more when the author thins its entry
    /// rate under congestion, R18). `active = false` parks the hand.
    Hand { obj: u32, target_mm: [i32; 3], active: bool, span: u8 },
    /// R14, source world: carry `obj` out through the portal into escrow, bound for world `dest`.
    /// The transfer's id is derived from this entry's id (review NEW-5), never chosen.
    /// P2-5: `hop` is the certified hop that brought the object here, when this world does not
    /// hold it yet (copied from a `Provenance` entry); without it an arrived object cannot leave.
    Depart { obj: u32, dest: u32, deadline: u64, hop: Option<Hop> },
    /// R14, destination world: the object arrives. `cert` is a strict majority of the source
    /// world's members' signatures over "departure `depart` of this record to this world, by
    /// this deadline, is confirmed" (reviews NEW-4, R3-8); without it the entry does nothing.
    /// `deadline` is a frame of the destination world: a later arrival is refused (R3-6).
    /// P2-5: `chain` is the object's provenance from its birth into the source world; the
    /// certificate covers its hash.
    Arrive { transfer: u64, record: ObjRecord, depart: EntryId, deadline: u64, cert: Vec<(MemberId, u64)>, chain: Vec<Hop> },
    /// R14, source world: the destination's confirmed verdict, with a strict majority of the
    /// destination's members' signatures over it (review NEW-3). Anyone may relay it.
    Verdict { transfer: u64, accepted: bool, cert: Vec<(MemberId, u64)> },
    /// A member's signature over a cross-world fact its replica has confirmed (R14), published as
    /// a chain entry so it is equivocation-proof and fetchable.
    Attest { fact: u64, sig: u64 },
    /// P2-5, destination world: the source's certified commit of a transfer into this world, relayed
    /// so every member holds the hop the object's next departure must carry. Never simulated.
    Provenance { hop: Hop },
    /// R14 test fixture: open or close this world's arrival portal.
    Portal { open: bool },
    /// The author's ack, as a chain entry (review NEW-1): it will author nothing stamped at or
    /// before `horizon`; per other author, the newest seq it holds contiguously and that entry's
    /// id; its newest confirmed check frame and full-state hash (review H4); the membership epoch
    /// its author is in (design 10.8: an ack from a newer epoch than the reader's never counts
    /// toward the reader's confirmation).
    Ack { horizon: u64, heads: Vec<(MemberId, u32, EntryId)>, check: Option<(u64, u64)>, epoch: u32 },
    /// Vote, in membership epoch `epoch`, to remove `target`: it signed two versions of its entry
    /// `seq` (or another proof), or (`seq` = 0) it has gone silent or cannot keep up (reviews
    /// R3-2, R3-9). `frozen` is the head of target's chain this voter holds and will ever
    /// reference in this epoch (review NEW-2). A vote stands until the epoch ends (P2-4).
    Vote { epoch: u32, target: MemberId, seq: u32, frozen: (u32, EntryId) },
    /// View change (P2-4, design 10.8): the round's proposer proposes `change` for epoch `epoch`;
    /// `valid_round` is the round in which it saw a quorum prevote this value (`NO_ROUND` if none).
    Propose { epoch: u32, round: u32, valid_round: u32, change: Change },
    /// View change: this member's first-phase vote for a proposal's value in a round (0 = nil).
    Prevote { epoch: u32, round: u32, value: u64 },
    /// View change: this member's second-phase vote; a quorum of them decides the value. A value
    /// that adds members states their first frame, `boundary` (0 if none): from this entry on, the
    /// author's acks count toward no reader's confirmation at or past it until a later epoch (10.8).
    Precommit { epoch: u32, round: u32, value: u64, boundary: u64 },
}

/// `valid_round` of a proposal that no quorum has prevoted before.
pub const NO_ROUND: u32 = u32::MAX;

/// Frames a proposer waits, from the start of a round, for votes on every member it suspects.
pub const VC_GATHER: u64 = 6;

/// Two signed entries of one member that contradict each other (design 10.8): two versions of one
/// seq; an entry whose `prev` names a version of its predecessor other than `a`; or an ack `a` and a
/// later vote `b` whose frozen head drops an entry that ack referenced. Anyone holding both entries
/// (and, for a vote, the target's chain) can check it, so it travels in certificates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proof {
    pub a: EntryId,
    pub b: EntryId,
}

/// One member removed by a view change: the votes that justify it (each names its voter's frozen
/// head of the member's chain) and the cut those votes determine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub member: MemberId,
    pub votes: Vec<EntryId>,
    pub cut: (u32, EntryId),
}

/// A membership change from epoch `epoch` to `epoch + 1` (design 10.8). `excluded` are the members
/// this decision treats as proven faulty: they count neither in its quorums nor in their size. An
/// empty change is a reset: same members, every vote of the epoch cleared.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Change {
    pub epoch: u32,
    pub remove: Vec<Removal>,
    pub add: Vec<MemberId>,
    /// First frame of the next epoch: added members count from it (10.8, joins), and facts about
    /// frames from it on are the next epoch's to certify. Set on every change.
    pub boundary: u64,
    pub excluded: Vec<(MemberId, Proof)>,
}

impl Change {
    /// The value the view change votes on: a hash of the whole change, never 0 (0 is nil).
    pub fn value(&self) -> u64 {
        let mut w = Wr(Vec::with_capacity(64));
        w.change(self);
        fnv_bytes(&w.0) | 1
    }
}

/// D80 (open): how many members a view change needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuorumRule {
    /// A strict majority of the members not excluded by proof (option a): survives crashes and
    /// partitions, but tolerates no faulty member that has not been proven so.
    Majority,
    /// A Byzantine quorum (option b, recommended): more than (m + t) / 2 of the m members not
    /// excluded by proof, which tolerates t = floor((m - 1) / 3) faulty members not yet proven.
    Bft,
}

/// `(t, q)` for `m` members not excluded by proof: unproven faulty members tolerated, and the
/// quorum size. Any two quorums then share an honest member while at most t of the m are faulty.
pub fn quorum(rule: QuorumRule, m: usize) -> (usize, usize) {
    let t = match rule {
        QuorumRule::Majority => 0,
        QuorumRule::Bft => m.saturating_sub(1) / 3,
    };
    (t, (m + t + 2) / 2)
}

/// Spike model of a member's signature over its precommit of `value` in `round` of the view change
/// that ends epoch `epoch` of `world`: the signatures an epoch certificate carries.
pub fn vc_sig(world: u32, member: MemberId, epoch: u32, round: u32, value: u64) -> u64 {
    member_sig(world, member, fnv_u64s([3, epoch as u64, round as u64, value]))
}

/// An epoch certificate (design 10.8, review R3-8b): what another world needs to follow this
/// world's membership from `epoch - 1` to `epoch`. The precommits sign the change's value, so the
/// change itself (removals, joins, exclusions and the boundary frame) is what is certified.
#[derive(Clone, Debug, PartialEq)]
pub struct EpochCert {
    pub world: u32,
    /// The epoch this certificate starts (`change.epoch + 1`).
    pub epoch: u32,
    pub change: Change,
    pub round: u32,
    /// The members of `epoch`: the previous epoch's, minus the removed, plus the added.
    pub members: Vec<MemberId>,
    /// Precommit signatures over `(world, epoch - 1, round, change value)`.
    pub precommits: Vec<(MemberId, u64)>,
    /// The two entries of each exclusion proof, so a verifier needs no chain of this world.
    pub proofs: Vec<Entry>,
}

impl EpochCert {
    /// First frame of the epoch: facts about frames from here on are this epoch's to sign.
    pub fn from_frame(&self) -> u64 {
        self.change.boundary
    }

    pub fn hash(&self) -> u64 {
        let mut v = vec![self.world as u64, self.epoch as u64, self.change.value(), self.round as u64];
        v.extend(self.members.iter().map(|m| *m as u64));
        for (m, s) in &self.precommits {
            v.extend([*m as u64, *s]);
        }
        v.extend(self.proofs.iter().map(|e| e.id()));
        fnv_u64s(v)
    }
}

/// Whether `p` proves `m` faulty from its two entries alone (design 10.8): two versions of one seq;
/// an entry whose `prev` names another version of its predecessor; or an ack that references a
/// seq of some chain beyond the frozen head of a later vote by the same member against it.
pub fn proof_self_contained(m: MemberId, p: Proof, a: &Entry, b: &Entry) -> bool {
    if a.member != m || b.member != m || a.id() != p.a || b.id() != p.b {
        return false;
    }
    if a.seq == b.seq {
        return p.a != p.b;
    }
    if b.seq == a.seq + 1 {
        return b.prev != p.a;
    }
    match (&a.payload, &b.payload) {
        (Payload::Ack { heads, .. }, Payload::Vote { target, frozen, .. }) => {
            a.seq < b.seq && heads.iter().any(|h| h.0 == *target && h.1 > frozen.0)
        }
        _ => false,
    }
}

/// Check `cert` against the members of the previous epoch: every exclusion proved by the entries
/// it carries; precommits from a quorum of the members not excluded; the member list following
/// from the change.
pub fn epoch_cert_ok(rule: QuorumRule, prev: &[MemberId], cert: &EpochCert) -> bool {
    let c = &cert.change;
    if cert.epoch == 0 || c.epoch + 1 != cert.epoch {
        return false;
    }
    let by_id: BTreeMap<EntryId, &Entry> = cert.proofs.iter().map(|e| (e.id(), e)).collect();
    let mut excluded = BTreeSet::new();
    for &(m, p) in &c.excluded {
        let (Some(a), Some(b)) = (by_id.get(&p.a), by_id.get(&p.b)) else { return false };
        if !prev.contains(&m) || !excluded.insert(m) || !proof_self_contained(m, p, a, b) {
            return false;
        }
    }
    let base: Vec<MemberId> = prev.iter().copied().filter(|m| !excluded.contains(m)).collect();
    let (_, q) = quorum(rule, base.len());
    let value = c.value();
    let signers: BTreeSet<MemberId> = cert
        .precommits
        .iter()
        .filter(|(m, s)| base.contains(m) && *s == vc_sig(cert.world, *m, c.epoch, cert.round, value))
        .map(|(m, _)| *m)
        .collect();
    let mut expect: BTreeSet<MemberId> = prev.iter().copied().collect();
    for r in &c.remove {
        expect.remove(&r.member);
    }
    expect.extend(c.add.iter().copied());
    signers.len() >= q && expect.into_iter().collect::<Vec<_>>() == cert.members
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub member: MemberId,
    /// 1-based per member. A member that rejoins gets a new MemberId, so seqs never restart.
    pub seq: u32,
    /// Id of this member's previous entry (0 for seq 1): a hash chain.
    pub prev: EntryId,
    /// Canonical frame chosen by the author (its present frame when it acted).
    pub stamp: u64,
    pub payload: Payload,
    /// User ruling 09:30Z: the resulting state of the touched object after frame `stamp`.
    pub carried: Option<(u32, BodyState)>,
}

impl Entry {
    /// Spike: 64-bit FNV of the encoding. Production: BLAKE3 of the signed bytes.
    pub fn id(&self) -> EntryId {
        let mut w = Wr(Vec::with_capacity(96));
        w.entry(self);
        fnv_bytes(&w.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// Any chain entry: a perturbation, an ack or a vote.
    Entry(Entry),
    /// Ask for `author`'s entries `from..=to` (gap or conflict detection).
    Fetch { author: MemberId, from: u32, to: u32 },
    /// Everything one member sends one peer in one tick, in one datagram (R18, viz-4).
    Batch(Vec<Msg>),
    /// View change: ask for the proposal entry carrying `value` (a member that saw a quorum of
    /// precommits for a value it holds no proposal for; the proposer may have equivocated).
    FetchValue { value: u64 },
    /// A non-member asks to join the world (P2-4). Production: signed by the joiner's key, after an
    /// invite or the world's ACL admits it (12); the spike admits any request.
    Join { member: MemberId },
}

pub fn fnv_bytes(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h = (h ^ x as u64).wrapping_mul(0x100_0000_01B3);
    }
    h
}

// ---- wire encoding (also used in-process, so byte counts are real) ------------------------------

pub struct Wr(pub Vec<u8>);
impl Wr {
    fn u8(&mut self, v: u8) { self.0.push(v) }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn i32(&mut self, v: i32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn state(&mut self, s: &BodyState) { for b in s.0 { self.u32(b) } }
    fn cert(&mut self, c: &[(MemberId, u64)]) { self.u32(c.len() as u32); for (m, s) in c { self.u32(*m); self.u64(*s) } }
    fn hop(&mut self, h: &Hop) {
        self.u32(h.src); self.u32(h.dest); self.u64(h.transfer); self.u64(h.via); self.u32(h.epoch); self.u64(h.frame);
        self.u32(h.certs.len() as u32); for c in &h.certs { self.epoch_cert(c) }
        self.cert(&h.cert)
    }
    fn epoch_cert(&mut self, c: &EpochCert) {
        self.u32(c.world); self.u32(c.epoch); self.change(&c.change); self.u32(c.round);
        self.u32(c.members.len() as u32); for m in &c.members { self.u32(*m) }
        self.cert(&c.precommits);
        self.u32(c.proofs.len() as u32); for e in &c.proofs { self.entry(e) }
    }
    fn chain(&mut self, c: &[Hop]) { self.u32(c.len() as u32); for h in c { self.hop(h) } }
    fn entry(&mut self, e: &Entry) {
        self.u32(e.member);
        self.u32(e.seq);
        self.u64(e.prev);
        self.u64(e.stamp);
        match &e.payload {
            Payload::Touch { obj, impulse, grab } => {
                self.u8(1); self.u32(*obj); for c in impulse { self.u32(c.to_bits()) } self.u8(*grab as u8);
            }
            Payload::Hand { obj, target_mm, active, span } => {
                self.u8(2); self.u32(*obj); for c in target_mm { self.i32(*c) } self.u8(*active as u8); self.u8(*span);
            }
            Payload::Depart { obj, dest, deadline, hop } => {
                self.u8(3); self.u32(*obj); self.u32(*dest); self.u64(*deadline);
                match hop { Some(h) => { self.u8(1); self.hop(h) } None => self.u8(0) }
            }
            Payload::Arrive { transfer, record, depart, deadline, cert, chain } => {
                self.u8(4); self.u64(*transfer); self.u64(record.global); self.u8(record.kind);
                self.state(&record.state); self.u32(record.src_world); self.u64(record.src_frame); self.u64(record.via);
                self.u64(*depart); self.u64(*deadline); self.cert(cert); self.chain(chain);
            }
            Payload::Provenance { hop } => { self.u8(16); self.hop(hop) }
            Payload::Verdict { transfer, accepted, cert } => {
                self.u8(5); self.u64(*transfer); self.u8(*accepted as u8); self.cert(cert);
            }
            Payload::Attest { fact, sig } => { self.u8(9); self.u64(*fact); self.u64(*sig) }
            Payload::Portal { open } => { self.u8(6); self.u8(*open as u8) }
            Payload::Ack { horizon, heads, check, epoch } => {
                // Epoch 0 keeps the phase-1 layout (tag 7), so runs without a view change keep
                // their entry ids; a later epoch is tag 14 with the epoch appended.
                self.u8(if *epoch == 0 { 7 } else { 14 }); self.u64(*horizon); self.u32(heads.len() as u32);
                for (q, s, id) in heads { self.u32(*q); self.u32(*s); self.u64(*id) }
                match check { Some((f, h)) => { self.u8(1); self.u64(*f); self.u64(*h) } None => self.u8(0) }
                if *epoch != 0 { self.u32(*epoch) }
            }
            Payload::Vote { epoch, target, seq, frozen } => {
                self.u8(8); self.u32(*epoch); self.u32(*target); self.u32(*seq); self.u32(frozen.0); self.u64(frozen.1)
            }
            Payload::Propose { epoch, round, valid_round, change } => {
                self.u8(11); self.u32(*epoch); self.u32(*round); self.u32(*valid_round); self.change(change)
            }
            Payload::Prevote { epoch, round, value } => { self.u8(12); self.u32(*epoch); self.u32(*round); self.u64(*value) }
            Payload::Precommit { epoch, round, value, boundary } => {
                self.u8(13); self.u32(*epoch); self.u32(*round); self.u64(*value); self.u64(*boundary)
            }
        }
        match &e.carried {
            Some((obj, s)) => { self.u8(1); self.u32(*obj); self.state(s) }
            None => self.u8(0),
        }
    }
    fn change(&mut self, c: &Change) {
        self.u32(c.epoch);
        self.u32(c.remove.len() as u32);
        for r in &c.remove {
            self.u32(r.member);
            self.u32(r.votes.len() as u32);
            for v in &r.votes { self.u64(*v) }
            self.u32(r.cut.0);
            self.u64(r.cut.1);
        }
        self.u32(c.add.len() as u32);
        for m in &c.add { self.u32(*m) }
        self.u64(c.boundary);
        self.u32(c.excluded.len() as u32);
        for (m, p) in &c.excluded { self.u32(*m); self.u64(p.a); self.u64(p.b) }
    }
    pub fn msg(&mut self, m: &Msg) {
        match m {
            Msg::Entry(e) => { self.u8(1); self.entry(e) }
            Msg::Fetch { author, from, to } => { self.u8(3); self.u32(*author); self.u32(*from); self.u32(*to) }
            Msg::Batch(v) => { self.u8(4); self.u32(v.len() as u32); for m in v { self.msg(m) } }
            Msg::FetchValue { value } => { self.u8(5); self.u64(*value) }
            Msg::Join { member } => { self.u8(6); self.u32(*member) }
        }
    }
}

pub struct Rd<'a>(pub &'a [u8], pub usize);
impl Rd<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.0.get(self.1..self.1 + n)?;
        self.1 += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
    fn u32(&mut self) -> Option<u32> { Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    fn u64(&mut self) -> Option<u64> { Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
    fn i32(&mut self) -> Option<i32> { Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    fn f32(&mut self) -> Option<f32> { Some(f32::from_bits(self.u32()?)) }
    fn cert(&mut self) -> Option<Vec<(MemberId, u64)>> {
        let n = self.u32()? as usize;
        let mut c = Vec::with_capacity(n.min(64));
        for _ in 0..n { c.push((self.u32()?, self.u64()?)) }
        Some(c)
    }
    fn hop(&mut self) -> Option<Hop> {
        let (src, dest, transfer, via, epoch, frame) = (self.u32()?, self.u32()?, self.u64()?, self.u64()?, self.u32()?, self.u64()?);
        let n = self.u32()? as usize;
        let mut certs = Vec::with_capacity(n.min(64));
        for _ in 0..n { certs.push(self.epoch_cert()?) }
        Some(Hop { src, dest, transfer, via, epoch, frame, certs, cert: self.cert()? })
    }
    fn epoch_cert(&mut self) -> Option<EpochCert> {
        let (world, epoch, change, round) = (self.u32()?, self.u32()?, self.change()?, self.u32()?);
        let n = self.u32()? as usize;
        let mut members = Vec::with_capacity(n.min(256));
        for _ in 0..n { members.push(self.u32()?) }
        let precommits = self.cert()?;
        let n = self.u32()? as usize;
        let mut proofs = Vec::with_capacity(n.min(64));
        for _ in 0..n { proofs.push(self.entry()?) }
        Some(EpochCert { world, epoch, change, round, members, precommits, proofs })
    }
    fn chain(&mut self) -> Option<Vec<Hop>> {
        let n = self.u32()? as usize;
        let mut c = Vec::with_capacity(n.min(64));
        for _ in 0..n { c.push(self.hop()?) }
        Some(c)
    }
    fn state(&mut self) -> Option<BodyState> {
        let mut s = [0u32; 13];
        for b in &mut s { *b = self.u32()? }
        Some(BodyState(s))
    }
    fn entry(&mut self) -> Option<Entry> {
        let member = self.u32()?;
        let seq = self.u32()?;
        let prev = self.u64()?;
        let stamp = self.u64()?;
        let payload = match self.u8()? {
            1 => Payload::Touch { obj: self.u32()?, impulse: [self.f32()?, self.f32()?, self.f32()?], grab: self.u8()? != 0 },
            2 => Payload::Hand { obj: self.u32()?, target_mm: [self.i32()?, self.i32()?, self.i32()?], active: self.u8()? != 0, span: self.u8()? },
            3 => {
                let (obj, dest, deadline) = (self.u32()?, self.u32()?, self.u64()?);
                let hop = if self.u8()? == 1 { Some(self.hop()?) } else { None };
                Payload::Depart { obj, dest, deadline, hop }
            }
            4 => {
                let transfer = self.u64()?;
                let record = ObjRecord {
                    global: self.u64()?, kind: self.u8()?, state: self.state()?, src_world: self.u32()?, src_frame: self.u64()?,
                    via: self.u64()?,
                };
                Payload::Arrive { transfer, record, depart: self.u64()?, deadline: self.u64()?, cert: self.cert()?, chain: self.chain()? }
            }
            16 => Payload::Provenance { hop: self.hop()? },
            5 => Payload::Verdict { transfer: self.u64()?, accepted: self.u8()? != 0, cert: self.cert()? },
            9 => Payload::Attest { fact: self.u64()?, sig: self.u64()? },
            6 => Payload::Portal { open: self.u8()? != 0 },
            tag @ (7 | 14) => {
                let horizon = self.u64()?;
                let n = self.u32()? as usize;
                let mut heads = Vec::with_capacity(n.min(256));
                for _ in 0..n { heads.push((self.u32()?, self.u32()?, self.u64()?)) }
                let check = if self.u8()? == 1 { Some((self.u64()?, self.u64()?)) } else { None };
                let epoch = if tag == 14 { self.u32()? } else { 0 };
                Payload::Ack { horizon, heads, check, epoch }
            }
            8 => Payload::Vote { epoch: self.u32()?, target: self.u32()?, seq: self.u32()?, frozen: (self.u32()?, self.u64()?) },
            11 => Payload::Propose { epoch: self.u32()?, round: self.u32()?, valid_round: self.u32()?, change: self.change()? },
            12 => Payload::Prevote { epoch: self.u32()?, round: self.u32()?, value: self.u64()? },
            13 => Payload::Precommit { epoch: self.u32()?, round: self.u32()?, value: self.u64()?, boundary: self.u64()? },
            _ => return None,
        };
        let carried = match self.u8()? {
            1 => Some((self.u32()?, self.state()?)),
            _ => None,
        };
        Some(Entry { member, seq, prev, stamp, payload, carried })
    }
    fn change(&mut self) -> Option<Change> {
        let epoch = self.u32()?;
        let n = self.u32()? as usize;
        let mut remove = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            let member = self.u32()?;
            let k = self.u32()? as usize;
            let mut votes = Vec::with_capacity(k.min(256));
            for _ in 0..k { votes.push(self.u64()?) }
            remove.push(Removal { member, votes, cut: (self.u32()?, self.u64()?) });
        }
        let n = self.u32()? as usize;
        let mut add = Vec::with_capacity(n.min(256));
        for _ in 0..n { add.push(self.u32()?) }
        let boundary = self.u64()?;
        let n = self.u32()? as usize;
        let mut excluded = Vec::with_capacity(n.min(256));
        for _ in 0..n { excluded.push((self.u32()?, Proof { a: self.u64()?, b: self.u64()? })) }
        Some(Change { epoch, remove, add, boundary, excluded })
    }
    pub fn msg(&mut self) -> Option<Msg> {
        Some(match self.u8()? {
            1 => Msg::Entry(self.entry()?),
            3 => Msg::Fetch { author: self.u32()?, from: self.u32()?, to: self.u32()? },
            4 => {
                let n = self.u32()? as usize;
                let mut v = Vec::with_capacity(n.min(256));
                for _ in 0..n { v.push(self.msg()?) }
                Msg::Batch(v)
            }
            5 => Msg::FetchValue { value: self.u64()? },
            6 => Msg::Join { member: self.u32()? },
            _ => return None,
        })
    }
}

pub fn encode_msg(m: &Msg) -> Vec<u8> {
    let mut w = Wr(Vec::with_capacity(128));
    w.msg(m);
    w.0
}
pub fn decode_msg(b: &[u8]) -> Option<Msg> {
    Rd(b, 0).msg()
}

/// Entries that never act on the simulation: acks, attestations and the membership protocol.
pub fn is_protocol(p: &Payload) -> bool {
    matches!(
        p,
        Payload::Ack { .. }
            | Payload::Vote { .. }
            | Payload::Attest { .. }
            | Payload::Provenance { .. }
            | Payload::Propose { .. }
            | Payload::Prevote { .. }
            | Payload::Precommit { .. }
    )
}

/// Same-frame tie-break (user ruling 09:32Z): a per-frame pseudo-random permutation of members.
/// `MemberId`s are assigned in admission order by the membership log, never chosen by the peer,
/// and `seed` comes from the session/epoch genesis, so no key can be ground to win. All 32 bits of
/// the id are hashed (review L7).
pub fn tie_rank(seed: u64, frame: u64, member: MemberId) -> u64 {
    splitmix64(seed ^ frame.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (member as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
}

// ---- R14 proofs (reviews NEW-3..NEW-6) -------------------------------------------------------------

/// Largest linear speed (m/s) and spin (rad/s) an arriving object may carry (review NEW-21).
pub const MAX_ARRIVAL_SPEED: f32 = 200.0;
pub const MAX_ARRIVAL_SPIN: f32 = 2000.0;
/// Furthest deadline, in frames after its arrival, that a destination accepts (round 3, low):
/// it bounds how long a transfer stays open and its id must be kept. World config in production.
pub const MAX_DEADLINE_FRAMES: u64 = 3600;

/// P25-3: the source's certified departure carried by an `Arrive`, as one export of a fork proof.
/// The spike's certificates have no epochs yet (P2-4): epoch 0.
fn arrival_export(src: u32, depart: EntryId, record: &ObjRecord, dest: u32, deadline: u64, chain: u64, cert: &[(MemberId, u64)]) -> Export {
    Export::Departed { src, epoch: 0, depart, record: *record, dest, deadline, chain, cert: cert.to_vec() }
}

/// A transfer's id: derived from the source world and its Depart entry, so nobody can squat it.
pub fn transfer_id(src_world: u32, depart: EntryId) -> u64 {
    splitmix64(((src_world as u64) << 40) ^ depart)
}

/// "World `src` confirmed departure `depart` of `record`, bound for world `dest` by `deadline`
/// (a frame of `dest`), with provenance `chain` (its [`chain_hash`])." Covers every field of the
/// record (review R3-8) and the chain (P2-5), so a relayer can neither strip nor swap the chain.
pub fn fact_departed(src: u32, depart: EntryId, record: &ObjRecord, dest: u32, deadline: u64, chain: u64) -> u64 {
    fnv_u64s([
        1,
        record.via,
        chain,
        src as u64,
        depart,
        record.global,
        record.kind as u64,
        record.state.hash(),
        record.src_world as u64,
        record.src_frame,
        dest as u64,
        deadline,
    ])
}

/// "World `dst` confirmed transfer `transfer` as accepted / rejected."
pub fn fact_verdict(dst: u32, transfer: u64, accepted: bool) -> u64 {
    fnv_u64s([2, dst as u64, transfer, accepted as u64])
}

/// Spike model of member `member` of world `world` signing `fact`. Production: an Ed25519
/// signature by the member's key, checked against the other world's member keys (D46).
pub fn member_sig(world: u32, member: MemberId, fact: u64) -> u64 {
    splitmix64(fact ^ splitmix64((((world as u64) << 32) | member as u64) ^ 0x5157_4E41_5455_5245))
}

/// True if `cert` holds valid signatures over `fact` from a strict majority of `members`.
pub fn cert_ok(members: &[MemberId], world: u32, fact: u64, cert: &[(MemberId, u64)]) -> bool {
    let signers: BTreeSet<MemberId> =
        cert.iter().filter(|(m, sig)| members.contains(m) && *sig == member_sig(world, *m, fact)).map(|(m, _)| *m).collect();
    !members.is_empty() && signers.len() * 2 > members.len()
}

// ---- scenes --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum SceneKind {
    /// The phase-1 pile: ground plus side*side*layers boxes/spheres/capsules.
    Pile { side: usize, layers: usize },
    /// A walled pit of small balls plus one kinematic hand per listed member.
    BallPit { balls: usize, hands: usize },
    /// No bodies: a log-only world, for membership measurements at scale (P2-4).
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obj {
    pub body: BodyId,
    pub global: u64,
    pub kind: u8,
}

const BALL_R: f32 = 0.06;
pub const CLAIM_COOLDOWN: u64 = 10;
const HAND_R: f32 = 0.09;
/// Dead-reckoning speed cap for a hand without an entry (m/s).
const HAND_COAST_VMAX: f32 = 3.0;

fn add_shape(world: &mut World, body: BodyId, kind: u8) {
    let mut sd = default_shape_def();
    sd.density = 1.0;
    match kind {
        0 => {
            let h = make_box_hull(0.4, 0.4, 0.4);
            create_hull_shape(world, body, &sd, &h.base);
        }
        1 => {
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: 0.45 };
            create_sphere_shape(world, body, &sd, &s);
        }
        2 => {
            let c = Capsule {
                center1: Vec3 { x: -0.3, y: 0.0, z: 0.0 },
                center2: Vec3 { x: 0.3, y: 0.0, z: 0.0 },
                radius: 0.25,
            };
            create_capsule_shape(world, body, &sd, &c);
        }
        3 => {
            sd.density = 0.2;
            sd.base_material.rolling_resistance = 0.2;
            sd.base_material.friction = 0.6;
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: BALL_R };
            create_sphere_shape(world, body, &sd, &s);
        }
        _ => {
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: HAND_R };
            create_sphere_shape(world, body, &sd, &s);
        }
    }
}

fn static_box(world: &mut World, p: Pos, hx: f32, hy: f32, hz: f32) {
    let mut d = default_body_def();
    d.type_ = BodyType::Static;
    d.position = p;
    let b = create_body(world, &d);
    let hull = make_box_hull(hx, hy, hz);
    create_hull_shape(world, b, &default_shape_def(), &hull.base);
}

/// Builds the scene. `world_tag` makes global object ids unique across worlds (R14).
pub fn build_scene(kind: SceneKind, world_tag: u32) -> (World, Vec<Option<Obj>>) {
    let g = |i: usize| ((world_tag as u64) << 32) | i as u64;
    match kind {
        SceneKind::Pile { side, layers } => {
            let s = crate::build_pile_scene(side, layers);
            let objs = s.bodies.iter().enumerate().map(|(i, &b)| Some(Obj { body: b, global: g(i), kind: (i % 3) as u8 })).collect();
            (s.world, objs)
        }
        SceneKind::BallPit { balls, hands } => {
            let mut world = World::new(&crate::engine_world_def());
            static_box(&mut world, Pos { x: 0.0, y: -0.5, z: 0.0 }, 2.0, 0.5, 2.0);
            let (w, h) = (1.2f32, 0.6f32);
            static_box(&mut world, Pos { x: w + 0.1, y: h, z: 0.0 }, 0.1, h, w + 0.2);
            static_box(&mut world, Pos { x: -w - 0.1, y: h, z: 0.0 }, 0.1, h, w + 0.2);
            static_box(&mut world, Pos { x: 0.0, y: h, z: w + 0.1 }, w + 0.2, h, 0.1);
            static_box(&mut world, Pos { x: 0.0, y: h, z: -w - 0.1 }, w + 0.2, h, 0.1);
            let mut objs = Vec::with_capacity(balls + hands);
            let per_row = 16usize;
            let spacing = 2.0 * w / per_row as f32;
            for i in 0..balls {
                let (ix, iz, iy) = (i % per_row, (i / per_row) % per_row, i / (per_row * per_row));
                let jitter = (i as f32 * 0.618_034).fract() * 0.02 - 0.01;
                let mut d = default_body_def();
                d.type_ = BodyType::Dynamic;
                d.linear_damping = 0.1;
                d.angular_damping = 0.3;
                d.position = Pos {
                    x: -w + spacing * (ix as f32 + 0.5) + jitter,
                    y: BALL_R + 0.01 + iy as f32 * (2.0 * BALL_R + 0.01),
                    z: -w + spacing * (iz as f32 + 0.5) - jitter,
                };
                let b = create_body(&mut world, &d);
                add_shape(&mut world, b, 3);
                objs.push(Some(Obj { body: b, global: g(i), kind: 3 }));
            }
            for k in 0..hands {
                let mut d = default_body_def();
                d.type_ = BodyType::Kinematic;
                d.position = Pos { x: -0.8 + 1.6 * k as f32, y: 1.5, z: 0.0 };
                let b = create_body(&mut world, &d);
                add_shape(&mut world, b, 4);
                objs.push(Some(Obj { body: b, global: g(balls + k), kind: 4 }));
            }
            (world, objs)
        }
        SceneKind::Empty => (World::new(&crate::engine_world_def()), Vec::new()),
    }
}

// ---- rollback state ------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub member: MemberId,
    pub until: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscrowStatus {
    Pending,
    Committed,
    Returned,
    DepartRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Escrow {
    pub obj: u32,
    pub record: Option<ObjRecord>,
    pub status: EscrowStatus,
    pub depart_frame: u64,
    pub depart: EntryId,
    pub dest: u32,
    /// A frame of `dest`: an arrival stamped later is refused there, and that certified refusal
    /// returns the object (review R3-6).
    pub deadline: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    Accepted { obj: u32, frame: u64 },
    /// `until` is the transfer's deadline: past it, any replay is late, so the entry can go.
    Rejected { frame: u64, until: u64 },
}

/// Everything that rolls back together. The Box3D world image covers bodies, contacts, islands
/// and id pools; the tables cover engine state.
#[derive(Clone, Default)]
pub struct Tables {
    pub objs: Vec<Option<Obj>>,
    pub claims: BTreeMap<u32, Claim>,
    /// After a claim expires, its holder may not claim the object again for CLAIM_COOLDOWN frames,
    /// so repeated impulses cannot monopolize an object (review M3).
    pub cooldown: BTreeMap<u32, (MemberId, u64)>,
    pub escrow: BTreeMap<u64, Escrow>,
    pub arrivals: BTreeMap<u64, Arrival>,
    /// P2-5: per global id, here (with its provenance) or the transfer that last took it away.
    /// Never pruned: a world's own last exit is what lets it refuse a forked chain.
    pub seen: BTreeMap<u64, Seen>,
    pub portal_open: bool,
    /// Active hands: frames since the hand's last entry (dead reckoning, `SimConfig::hand_predict`).
    pub hand_idle: BTreeMap<u32, u64>,
}

#[derive(Clone)]
struct Keyframe {
    image: Vec<u8>,
    t: Tables,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub rollbacks: u64,
    pub resim_frames: u64,
    pub max_depth: u64,
    pub resim_us: u64,
    pub step_us: u64,
    pub steps: u64,
    /// Keyframe saves and restores (counts and time), the worst single `advance` call, and the
    /// most image bytes the ring ever held.
    pub saves: u64,
    pub save_us: u64,
    pub restore_us: u64,
    pub max_advance_us: u64,
    pub advance_us: u64,
    pub max_ring_bytes: u64,
    pub overwrites: u64,
    pub paused_frames: u64,
    pub entries_rejected_violation: u64,
    pub equivocations: u64,
    pub checks_ok: u64,
    pub checks_bad: u64,
    /// Check frames at which some body held a non-finite position, rotation or velocity (review L2).
    pub nonfinite: u64,
    /// Eviction votes found to contradict their voter's own published references (review R3-1).
    pub lying_votes: u64,
    /// Members this replica presumed crashed and voted to remove (review R3-2).
    pub suspicions: u64,
    /// Members this replica voted to demote because their link could not keep up (review R3-9).
    pub demotions: u64,
    /// View changes this replica decided, those that changed no member (resets), and the highest
    /// round any of them needed (P2-4).
    pub view_changes: u64,
    pub resets: u64,
    pub max_round: u32,
    /// Members this replica voted out for withholding references (P2-4).
    pub withholders: u64,
}

/// Which world images a member keeps between the base and its present (design 6.7). Purely local:
/// images are a cache of states the log already determines, so no setting changes any result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapPolicy {
    /// A keyframe at every multiple of `k` frames (the 3d0f98a behaviour with k = 4).
    Every(u64),
    /// Dense near the present (every `k` frames for the newest `4k`), then thinned so the spacing
    /// grows with age: about two keyframes per doubling of age.
    Log(u64),
    /// The base (newest image at or before the confirmed frame) plus one candidate, saved at the
    /// first multiple of `k` after the confirmed frame, that becomes the next base.
    Base(u64),
    /// The base plus keyframes every `k` frames for the newest `4k` frames only: a rollback
    /// deeper than that re-simulates from the base.
    Window(u64),
}

impl SnapPolicy {
    pub fn label(&self) -> String {
        match self {
            SnapPolicy::Every(k) => format!("every{k}"),
            SnapPolicy::Log(k) => format!("log{k}"),
            SnapPolicy::Base(k) => format!("base{k}"),
            SnapPolicy::Window(k) => format!("window{k}"),
        }
    }
}

/// Keyframe spacing kept at `age` frames behind the present under `SnapPolicy::Log(k)`.
pub fn log_grid(k: u64, age: u64) -> u64 {
    let r = age / k;
    if r < 4 {
        k
    } else {
        k << (62 - r.leading_zeros() as u64)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SimConfig {
    pub dt: f32,
    /// Speculation limit: a member stops advancing when `frame - conf` reaches this (frames).
    pub window: u64,
    pub snap: SnapPolicy,
    /// How far an ack's horizon may lag its own stamp, in frames: bounds backdating (review H5,
    /// user rule 14:31Z). Replaces the arrival-based late allowance; nothing is re-stamped.
    pub horizon_slack: u64,
    pub seed: u64,
    pub check_every: u64,
    /// Remote-hand dead reckoning: an active hand with no entry for a frame keeps its last
    /// commanded velocity for up to this many frames, then holds. 0 = hold at once.
    pub hand_predict: u64,
    /// A member none of whose entries has arrived for this many frames is presumed crashed and
    /// voted out (review R3-2). 1.5x the speculation limit, so a congested member is not.
    pub suspect_after: u64,
    /// The view change's quorum rule (D80, open; the default is the recommendation).
    pub quorum: QuorumRule,
    /// View-change timeouts (frames): a round's steps time out after `vc_timeout + round *
    /// vc_timeout_step` frames, so rounds lengthen until messages arrive in time.
    pub vc_timeout: u64,
    pub vc_timeout_step: u64,
    /// Acks list the heads held of every other chain. Off only for membership timing at scale, where
    /// full heads cost n^3 memory across the simulated replicas: nothing then becomes stable.
    pub ack_heads: bool,
    /// How far ahead of its present a proposer puts a join's boundary frame (P2-4).
    pub join_lead: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            dt: 1.0 / 60.0,
            window: 120,
            snap: SnapPolicy::Every(4),
            horizon_slack: 2,
            seed: 0x5052_494D_4F52_4449,
            check_every: 30,
            hand_predict: 0,
            suspect_after: 180,
            quorum: QuorumRule::Bft,
            vc_timeout: 20,
            vc_timeout_step: 10,
            ack_heads: true,
            join_lead: 60,
        }
    }
}

/// Verdict of one entry when its frame was (re)simulated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    RejectedClaimed { by: MemberId, until: u64 },
    RejectedCooldown { until: u64 },
    RejectedGone,
    RejectedPortalClosed,
    RejectedDuplicate,
    /// R14: a relay without a valid certificate from the other world (reviews NEW-3, NEW-4).
    RejectedNoProof,
    /// R14: a certified record that is not a valid body state (review NEW-21).
    RejectedInvalid,
    /// R14: the other world is not on this world's agreed route list (reviews R3-4 to R3-6).
    RejectedNoRoute,
    /// R14: the arrival is stamped after the source's deadline (review R3-6).
    RejectedLate,
    /// R14: an object with this global id is already live here (review R3-5).
    RejectedLiveId,
    /// R14: the deadline is more than `MAX_DEADLINE_FRAMES` away (round 3, low).
    RejectedDeadline,
    /// P2-5: the chain does not prove the object's way from its birth into the source world.
    RejectedNoProvenance,
    /// P2-5: the object left this world by a transfer the arrival's chain does not contain.
    RejectedFork,
    /// P2-5: the object arrived here and the departure lacks its certified hop in.
    RejectedUnsettled,
    /// P25-1: this world's import policy refuses the object's birth world, chain or kind.
    RejectedPolicy,
    /// A `Hand` entry for a hand that is not its author's: a member moves only its own hands (P2-8
    /// review, 8.4).
    RejectedNotOwner,
}

/// One member's vote to remove another, as read from its chain (reviews R3-1, R3-2).
#[derive(Clone, Copy, Debug)]
struct VoteRec {
    /// The voter's frozen head of the target's chain.
    frozen: (u32, EntryId),
    /// The frame the vote was stamped at.
    stamp: u64,
    /// The vote entry's id: a view change names the votes it relies on.
    id: EntryId,
}

/// Result of checking a vote against its voter's own earlier acks (review R3-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoteCheck {
    Valid,
    /// The vote drops an entry this ack of the voter referenced.
    Lie(EntryId),
    /// An entry needed to tell is missing (it is fetched).
    Missing,
}

/// Steps of a view-change round (Tendermint's propose, prevote and precommit).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Step {
    #[default]
    Propose,
    Prevote,
    Precommit,
}

/// The view change of the current epoch as this replica runs it (P2-4, design 10.8): Tendermint's
/// algorithm for one decision (Buchman, Kwon and Milosevic 2018, algorithm 1), whose messages are
/// chain entries, so they are signed, ordered per member, fetched like any entry, and a member
/// that sends two different ones to two members has equivocated.
#[derive(Clone, Debug, Default)]
struct Vc {
    /// Clock frame at which this replica began the instance (round 0); None while idle.
    since: Option<u64>,
    round: u32,
    step: Step,
    /// Clock frame at which the current round began (the propose timeout runs from it).
    round_at: u64,
    proposed: bool,
    /// (value, round) this replica is locked on, and the latest value it saw a quorum prevote.
    locked: Option<(u64, u32)>,
    valid: Option<(u64, u32)>,
    /// Round -> (value, valid round) of the first proposal by that round's proposer, in chain order.
    proposals: BTreeMap<u32, (u64, u32)>,
    /// Every change proposed in this epoch, by value, including another version of an equivocating
    /// proposer's entry.
    changes: BTreeMap<u64, Change>,
    /// Round -> member -> value (0 = nil): each member's first prevote and first precommit.
    prevotes: BTreeMap<u32, BTreeMap<MemberId, u64>>,
    precommits: BTreeMap<u32, BTreeMap<MemberId, u64>>,
    /// Rounds whose proposal this replica already saw a quorum prevote.
    polka_done: BTreeSet<u32>,
    /// Armed timeouts: round -> clock frame armed.
    prevote_timer: BTreeMap<u32, u64>,
    precommit_timer: BTreeMap<u32, u64>,
}

pub struct WorldSim {
    pub world_id: u32,
    pub me: MemberId,
    pub members: BTreeSet<MemberId>,
    pub epoch: u32,
    pub cfg: SimConfig,
    pub world: World,
    pub t: Tables,
    pub frame: u64,
    registry: GeometryRegistry,
    slots: Vec<RegistrySlot>,
    slot_count: usize,
    ring: BTreeMap<u64, Keyframe>,
    /// Scratch world for canonical check hashes (review NEW-7).
    scratch: Option<World>,
    /// R14: the worlds this one trades objects with, and their members: its agreed route list
    /// (D46 trust config, part of the world's genesis; reviews R3-4 to R3-6). Any other world can
    /// neither receive its objects nor vouch for one.
    pub peers_of: BTreeMap<u32, Vec<MemberId>>,
    /// P2-5: epoch certificates of every world a provenance chain may pass through (static stub of
    /// P2-4's interface), and whether arrivals check provenance (off only for the control run).
    pub epochs: EpochBook,
    pub provenance: bool,
    /// Number of objects this world was born with: its birth records.
    pub genesis_objects: u64,
    /// P25-1: what this world accepts beyond identity (birth worlds, chain worlds, kinds).
    pub import_policy: ImportPolicy,
    /// Hands belong to members (a hand is part of its member's avatar, 16.3): hand object -> owner.
    /// A `Hand` entry by anyone else is refused (P2-8 review, 8.4). In the ball pit, hand k is the
    /// k-th genesis member's. Fixed for the world's life; not part of the rollback state.
    pub hand_owners: BTreeMap<u32, MemberId>,
    /// P25-3: double spends this world holds transferable proof of, from confirmed frames only
    /// (keyed by world, global id, entry and the two transfers). Not part of the state.
    pub fork_proofs: BTreeMap<(u32, u64, u64, u64, u64), ForkProof>,
    /// Cross-world effects of each simulated frame, kept outside the rollback state: a re-simulation
    /// of the frame replaces them, and they are acted on only once the frame is confirmed (P25-3,
    /// P25-4): (commit facts to attest, fork proofs found).
    frame_effects: BTreeMap<u64, (Vec<u64>, Vec<ForkProof>)>,
    /// Confirmed commit facts not yet attested (P25-4: each attested once).
    commits_due: Vec<u64>,
    attested: BTreeSet<u64>,
    // ---- log (round 2: chains, stability, chain-local verdicts)
    known: BTreeSet<MemberId>,
    entries: BTreeMap<EntryId, Entry>,
    /// The version of each (member, seq) this member holds and references: the first one seen.
    by_author: BTreeMap<(MemberId, u32), EntryId>,
    /// Per author: the verified contiguous prefix (hash-linked) held.
    contiguous: BTreeMap<MemberId, u32>,
    /// Void by a chain-local rule (horizon violations): identical for every holder.
    violations: BTreeSet<EntryId>,
    /// Withdrawn by an equivocation proof or an eviction cut.
    disputed: BTreeSet<EntryId>,
    eff: BTreeMap<EntryId, u64>,
    at_frame: BTreeMap<u64, BTreeSet<EntryId>>,
    /// Per author: (max horizon declared so far, last ack stamp) along its verified prefix.
    chain_state: BTreeMap<MemberId, (Option<u64>, u64)>,
    /// Per author: ack seq -> max horizon declared up to that seq (valid acks only).
    ack_hist: BTreeMap<MemberId, BTreeMap<u32, u64>>,
    /// (referrer, author) -> seq -> (id, the referrer's ack that said so): what each member's acks
    /// said it holds of each chain, up to the referrer's vote against that author (review R3-1).
    /// Never pruned in the spike; production keeps only the newest reference per pair, because
    /// consecutive references must extend each other [I].
    refs: BTreeMap<(MemberId, MemberId), BTreeMap<u32, (EntryId, EntryId)>>,
    /// Per author: the highest seq ever found stable. Stability is permanent (chains only grow), so
    /// a later conflicting reference can never shrink it (review R3-1).
    stable_hi: BTreeMap<MemberId, u32>,
    /// Per member: my present frame when a new entry of its chain last reached me (review R3-2).
    heard: BTreeMap<MemberId, u64>,
    /// Per member: my present frame since when its newest validated horizon has trailed my present
    /// by more than half the speculation limit (review R3-9, demotion).
    lagging: BTreeMap<MemberId, u64>,
    /// Equivocators: first disputed seq and the two contradicting entries (two versions of one seq,
    /// or an entry and a successor that names another version of it).
    proofs: BTreeMap<MemberId, (u32, EntryId, EntryId)>,
    /// Members proven faulty by a vote that contradicts their own earlier ack (ack, vote). Their
    /// chain is consistent, so nothing of it is withdrawn before the view change removes them.
    liars: BTreeMap<MemberId, Proof>,
    /// Heads this member froze when it voted (per epoch), or cuts of removed members.
    frozen: BTreeMap<MemberId, u32>,
    /// This epoch's votes: target -> voter -> the voter's first vote against target.
    votes: BTreeMap<MemberId, BTreeMap<MemberId, VoteRec>>,
    /// Evicted members: (cut seq, first frame they no longer count for, from the cut prefix held).
    evicted: BTreeMap<MemberId, (u32, u64)>,
    /// Evicted members: the id at the cut, naming which version of their chain counts.
    cut_ids: BTreeMap<MemberId, EntryId>,
    /// Evicted members whose prefix up to the cut this replica holds and has reconciled.
    cut_done: BTreeSet<MemberId>,
    /// The view change of the current epoch.
    vc: Vc,
    /// View-change entries (votes, proposals, prevotes, precommits) of later epochs, processed on
    /// entering their epoch.
    vc_later: BTreeMap<u32, Vec<EntryId>>,
    /// Every proposal entry seen, by value (answers `Msg::FetchValue`).
    proposal_ids: BTreeMap<u64, EntryId>,
    /// Decided changes, one per epoch boundary: (change, round, precommit signatures).
    decided: Vec<(Change, u32, Vec<(MemberId, u64)>)>,
    /// Decisions: (epoch reached, my clock frame when I decided, round, members removed).
    pub vc_log: Vec<(u32, u64, u32, Vec<MemberId>)>,
    /// Test fixture: author no vote, proposal, prevote or precommit (a member that withholds them).
    pub withhold_vc: bool,
    /// Non-members this replica sends its entries to: members that asked to join, or followers.
    pub followers: BTreeSet<MemberId>,
    /// Members that asked to join and are not admitted yet.
    join_requests: BTreeSet<MemberId>,
    /// Admitted members: the first frame each counts for.
    pub admitted: BTreeMap<MemberId, u64>,
    /// This epoch: members that precommitted a value adding members, at seq, with its boundary. Their
    /// later acks count only below the boundary, until they reach a later epoch (10.8).
    join_caps: BTreeMap<MemberId, (u32, u64)>,
    /// Per member: the first seq of its chain whose ack declares each epoch after 0.
    ack_epoch_from: BTreeMap<MemberId, BTreeMap<u32, u32>>,
    /// Test fixture (D80 bridge): heads this member's acks and votes report at most.
    hidden: Option<BTreeMap<MemberId, (u32, EntryId)>>,
    /// (referrer, author) -> my present frame when the referrer's acks last referenced more of that
    /// author's chain (P2-4: a member that stops referencing a chain stalls everyone's confirmation).
    ref_advanced: BTreeMap<(MemberId, MemberId), u64>,
    /// The last frame withheld references were checked for.
    withhold_checked: u64,
    /// Test fixture (D80 bridge): a faulty member that backs every vote another member casts,
    /// prevotes and precommits every proposal it sees, in every round, echoes the nil votes of
    /// rounds it holds no proposal for, never proposes, and never applies a decision, so it can
    /// back both sides of a partition in one epoch.
    pub byz_ally: bool,
    /// The genesis members (epoch 0), from which every later epoch's members follow.
    genesis: Vec<MemberId>,
    /// Values decided by a quorum of precommits that this replica holds no proposal for:
    /// (member to ask, value), and every pair asked so far.
    value_wants: Vec<(MemberId, u64)>,
    value_asked: BTreeSet<(MemberId, u64)>,
    wants: Vec<(MemberId, MemberId, u32, u32)>,
    pub conf: u64,
    /// My clock frame (my present), set by the harness each tick.
    pub now: u64,
    my_seq: u32,
    /// Test fixture (review H5): publish horizons this many frames behind the present.
    pub horizon_lag: u64,
    /// Test fixture (review R3-1): vote with a forged frozen head of (0, 0).
    pub lie_in_votes: bool,
    my_last: EntryId,
    /// (holder, author) -> (first seq asked, when, current retry interval), all in ms.
    fetch_asked: BTreeMap<(MemberId, MemberId), (u32, u64, u64)>,
    // ---- results
    pub hashes: BTreeMap<u64, u64>,
    pub check_hashes: BTreeMap<u64, u64>,
    remote_checks: BTreeMap<u64, BTreeMap<MemberId, u64>>,
    evaluated: BTreeSet<(u64, MemberId)>,
    pub outcomes: BTreeMap<EntryId, (u64, Outcome)>,
    pub watched: BTreeSet<u32>,
    /// (frame, obj) -> object state hash after that frame, for watched objects.
    pub obj_hist: BTreeMap<(u64, u32), u64>,
    dirty_from: Option<u64>,
    /// The present a running re-simulation is heading for (keyframe thinning during re-sim).
    resim_to: u64,
    /// Record every object's position after each frame (observer measurements).
    pub track: bool,
    pub pos_hist: BTreeMap<u64, Vec<[f32; 3]>>,
    pub stats: Stats,
    pub events: Vec<String>,
}

/// Bytes needed to store `b` as a patch on `a`: every `block`-byte block of `b` that differs from
/// the block at the same offset in `a` (8 bytes of offset each), plus any tail beyond `a`.
pub fn block_delta(a: &[u8], b: &[u8], block: usize) -> u64 {
    let n = a.len().min(b.len());
    let mut bytes = 0u64;
    let mut i = 0;
    while i < n {
        let e = (i + block).min(n);
        if a[i..e] != b[i..e] {
            bytes += (e - i) as u64 + 8;
        }
        i = e;
    }
    bytes + b.len().saturating_sub(n) as u64
}

pub fn now_us() -> u64 {
    // Monotonic microseconds since first call (review L6: never SystemTime for sim clocks).
    use std::sync::OnceLock;
    use std::time::Instant;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

impl WorldSim {
    pub fn new(world_id: u32, me: MemberId, members: &[MemberId], scene: SceneKind, cfg: SimConfig) -> Self {
        let (world, objs) = build_scene(scene, world_id);
        let genesis_objects = objs.len() as u64;
        let hand_owners = match scene {
            SceneKind::BallPit { balls, hands } => {
                let mut ms = members.to_vec();
                ms.sort_unstable();
                (0..hands).zip(ms).map(|(k, m)| ((balls + k) as u32, m)).collect()
            }
            _ => BTreeMap::new(),
        };
        WorldSim {
            world_id,
            me,
            members: members.iter().copied().collect(),
            epoch: 0,
            cfg,
            world,
            t: Tables { objs, portal_open: true, ..Default::default() },
            frame: 0,
            registry: GeometryRegistry::new(),
            slots: Vec::new(),
            slot_count: 0,
            ring: BTreeMap::new(),
            scratch: None,
            peers_of: BTreeMap::new(),
            epochs: EpochBook::default(),
            provenance: true,
            genesis_objects,
            import_policy: ImportPolicy::default(),
            hand_owners,
            fork_proofs: BTreeMap::new(),
            frame_effects: BTreeMap::new(),
            commits_due: Vec::new(),
            attested: BTreeSet::new(),
            known: members.iter().copied().chain([me]).collect(),
            entries: BTreeMap::new(),
            by_author: BTreeMap::new(),
            contiguous: BTreeMap::new(),
            violations: BTreeSet::new(),
            disputed: BTreeSet::new(),
            eff: BTreeMap::new(),
            at_frame: BTreeMap::new(),
            chain_state: BTreeMap::new(),
            ack_hist: BTreeMap::new(),
            refs: BTreeMap::new(),
            stable_hi: BTreeMap::new(),
            heard: BTreeMap::new(),
            lagging: BTreeMap::new(),
            proofs: BTreeMap::new(),
            liars: BTreeMap::new(),
            frozen: BTreeMap::new(),
            votes: BTreeMap::new(),
            evicted: BTreeMap::new(),
            cut_ids: BTreeMap::new(),
            cut_done: BTreeSet::new(),
            vc: Vc::default(),
            vc_later: BTreeMap::new(),
            proposal_ids: BTreeMap::new(),
            decided: Vec::new(),
            vc_log: Vec::new(),
            withhold_vc: false,
            byz_ally: false,
            hidden: None,
            ref_advanced: BTreeMap::new(),
            withhold_checked: 0,
            followers: BTreeSet::new(),
            join_requests: BTreeSet::new(),
            admitted: BTreeMap::new(),
            join_caps: BTreeMap::new(),
            ack_epoch_from: BTreeMap::new(),
            genesis: members.to_vec(),
            value_wants: Vec::new(),
            value_asked: BTreeSet::new(),
            wants: Vec::new(),
            conf: 0,
            now: 0,
            my_seq: 0,
            horizon_lag: 0,
            lie_in_votes: false,
            my_last: 0,
            fetch_asked: BTreeMap::new(),
            hashes: BTreeMap::new(),
            check_hashes: BTreeMap::new(),
            remote_checks: BTreeMap::new(),
            evaluated: BTreeSet::new(),
            outcomes: BTreeMap::new(),
            watched: BTreeSet::new(),
            obj_hist: BTreeMap::new(),
            dirty_from: None,
            resim_to: 0,
            track: false,
            pos_hist: BTreeMap::new(),
            stats: Stats::default(),
            events: Vec::new(),
        }
    }

    fn ev(&mut self, s: String) {
        self.events.push(format!("[w{} m{} f{} c{}] {s}", self.world_id, self.me, self.frame, self.conf));
    }

    // ---- snapshots -------------------------------------------------------------------------------

    fn save(&mut self) -> Keyframe {
        let t0 = now_us();
        let mut buf = RecBuffer::new();
        serialize_world(&self.world, &mut buf, &mut self.registry);
        if self.registry.entries.len() != self.slot_count {
            self.slots = self.registry.to_slots();
            self.slot_count = self.registry.entries.len();
        }
        self.stats.saves += 1;
        self.stats.save_us += now_us() - t0;
        Keyframe { image: buf.data, t: self.t.clone() }
    }

    fn restore(&mut self, k: &Keyframe, frame: u64) {
        let t0 = now_us();
        assert!(deserialize_into_shell(&k.image, &mut self.world, &mut self.slots), "restore failed");
        self.t = k.t.clone();
        self.frame = frame;
        self.stats.restore_us += now_us() - t0;
    }

    /// Bytes of world image the ring holds now (the memory side of the snapshot knob).
    pub fn ring_bytes(&self) -> u64 {
        self.ring.values().map(|k| k.image.len() as u64).sum()
    }

    /// What the ring would hold as one full base image plus block deltas: (full bytes, bytes if
    /// every later image were stored as a delta against the base, bytes if each were stored as a
    /// delta against the previous image), with 64-byte blocks.
    pub fn ring_delta_bytes(&self) -> (u64, u64, u64) {
        let imgs: Vec<&[u8]> = self.ring.values().map(|k| k.image.as_slice()).collect();
        let Some(base) = imgs.first() else { return (0, 0, 0) };
        let full = imgs.iter().map(|i| i.len() as u64).sum();
        let vs_base = base.len() as u64 + imgs[1..].iter().map(|i| block_delta(base, i, 64)).sum::<u64>();
        let vs_prev = base.len() as u64 + imgs.windows(2).map(|w| block_delta(w[0], w[1], 64)).sum::<u64>();
        (full, vs_base, vs_prev)
    }

    /// The memory/CPU knob (design 6.7): any value, any time. Takes effect at the next keyframe
    /// decision and trim; never changes a result.
    pub fn set_snap(&mut self, p: SnapPolicy) {
        self.cfg.snap = p;
    }

    fn wants_keyframe(&self, f: u64) -> bool {
        if self.ring.contains_key(&f) {
            return false;
        }
        if self.ring.is_empty() {
            return true;
        }
        match self.cfg.snap {
            SnapPolicy::Every(k) => f % k == 0,
            // During a re-simulation, skip images the thinning would drop on arrival.
            SnapPolicy::Log(k) => f % k == 0 && f % log_grid(k, self.resim_to.max(self.frame) - f) == 0,
            SnapPolicy::Base(k) => f % k == 0 && f > self.conf && self.ring.range(self.conf + 1..).next().is_none(),
            SnapPolicy::Window(k) => f % k == 0 && self.resim_to.max(self.frame) - f < 4 * k,
        }
    }

    /// Hash of the full world image plus tables: used at confirmed check frames (review H4).
    pub fn image_hash(&mut self) -> u64 {
        // A check-hash serialization is not a keyframe: keep it out of the snapshot counters.
        let (n, us) = (self.stats.saves, self.stats.save_us);
        let k = self.save();
        (self.stats.saves, self.stats.save_us) = (n, us);
        // Review NEW-7: after a destroy, a world that stepped continuously and one restored from a
        // keyframe hold the same physics but different image bytes. Hash the image of a scratch
        // world restored from this one instead, so both paths hash alike.
        let scratch = self.scratch.get_or_insert_with(|| World::new(&crate::engine_world_def()));
        assert!(deserialize_into_shell(&k.image, scratch, &mut self.slots), "canonical restore failed");
        let mut buf = RecBuffer::new();
        serialize_world(scratch, &mut buf, &mut self.registry);
        fnv_bytes(&buf.data) ^ self.tables_hash().rotate_left(17)
    }

    fn tables_hash(&self) -> u64 {
        let mut v = vec![self.frame];
        for (i, o) in self.t.objs.iter().enumerate() {
            if let Some(o) = o {
                v.extend([i as u64, o.global, o.kind as u64]);
            }
        }
        for (o, c) in &self.t.claims {
            v.extend([*o as u64, c.member as u64, c.until]);
        }
        for (o, (m, u)) in &self.t.cooldown {
            v.extend([*o as u64, *m as u64, *u, 7]);
        }
        for (t, e) in &self.t.escrow {
            v.extend([*t, e.obj as u64, e.status as u64, e.depart_frame, e.record.map_or(0, |r| r.state.hash())]);
        }
        for (t, a) in &self.t.arrivals {
            match a {
                Arrival::Accepted { obj, frame } => v.extend([*t, 1, *obj as u64, *frame]),
                Arrival::Rejected { frame, until } => v.extend([*t, 2, *frame, *until]),
            }
        }
        for (g, s) in &self.t.seen {
            s.hash_into(*g, &mut v);
        }
        v.push(self.t.portal_open as u64);
        if self.cfg.hand_predict > 0 {
            for (o, n) in &self.t.hand_idle {
                v.extend([*o as u64, *n, 11]);
            }
        }
        fnv_u64s(v)
    }

    /// Per-frame hash: Box3D body transforms + velocities, plus every engine table.
    pub fn state_hash(&self) -> u64 {
        hash_world_state(&self.world) ^ self.tables_hash().rotate_left(29)
    }

    pub fn obj_state(&self, obj: u32) -> Option<BodyState> {
        self.t.objs.get(obj as usize).copied().flatten().map(|o| BodyState::read(&self.world, o.body))
    }

    // ---- log -------------------------------------------------------------------------------------
    //
    // Round-2 protocol (design 10.8; reviews NEW-1, NEW-2; user rule 14:31Z):
    // * every input to a deterministic decision is an entry in its author's hash chain:
    //   perturbations, acks (horizon, holdings, check hash) and eviction votes;
    // * an entry's frame is its stamp. Nothing is ever re-stamped, so an author never sees its own
    //   interaction corrected; lateness is paid by the late receiver's rollback;
    // * validity is chain-local: an entry is void iff its own chain breaks a rule, so every holder
    //   decides the same;
    // * confirmation uses only stable data: a seq of a chain is stable once every member's chain
    //   references it with the same id (Wuu-Bernstein's 2-D time table, over signed hash chains).

    pub fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.entries.get(&id)
    }

    pub fn effective_frame(&self, id: EntryId) -> Option<u64> {
        self.eff.get(&id).copied()
    }

    fn is_void(&self, id: EntryId) -> bool {
        self.violations.contains(&id) || self.disputed.contains(&id)
    }

    /// Apply or withdraw an entry's effect on the sim according to its current verdict.
    fn set_eff(&mut self, id: EntryId) {
        let e = &self.entries[&id];
        // A member that asked to join acts only once admitted (10.8): until then nothing it signs
        // touches the simulation.
        let counted = e.member == LOGIC || self.members.contains(&e.member) || self.evicted.contains_key(&e.member);
        let acts = counted && !is_protocol(&e.payload);
        let new = if acts && !self.is_void(id) { Some(e.stamp) } else { None };
        let old = self.eff.get(&id).copied();
        if old == new {
            return;
        }
        if let Some(o) = old {
            if let Some(s) = self.at_frame.get_mut(&o) {
                s.remove(&id);
            }
            self.eff.remove(&id);
            self.mark_dirty(o);
        }
        if let Some(n) = new {
            self.eff.insert(id, n);
            self.at_frame.entry(n).or_default().insert(id);
            self.mark_dirty(n);
        }
    }

    fn mark_dirty(&mut self, f: u64) {
        // Review NEW-2: confirmed frames are final. Getting here below `conf` is a protocol bug,
        // never a rollback.
        assert!(
            f >= self.conf,
            "protocol invariant violated: member {} would roll back to frame {f}, below its confirmed frame {}",
            self.me,
            self.conf
        );
        if f < self.frame {
            self.dirty_from = Some(self.dirty_from.map_or(f, |d| d.min(f)));
        }
    }

    fn publish(&mut self, e: Entry) -> Entry {
        self.my_seq += 1;
        self.my_last = e.id();
        let _ = self.insert(e.clone());
        e
    }

    /// Author an entry at the current frame. With `carry`, the entry carries the touched object's
    /// state after this frame: computed by stepping the frame once on a scratch copy.
    pub fn author(&mut self, payload: Payload, carry: Option<u32>) -> Entry {
        let stamp = if carry.is_some() {
            assert!(self.dirty_from.is_none() && self.frame >= self.now, "carrying state needs a caught-up sim");
            self.frame
        } else {
            self.frame.max(self.now)
        };
        let mut e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        if let Some(obj) = carry {
            let scratch = self.save();
            let tmp_id = e.id();
            self.entries.insert(tmp_id, e.clone());
            self.at_frame.entry(stamp).or_default().insert(tmp_id);
            self.eff.insert(tmp_id, stamp);
            self.step_frame(false);
            let st = self.obj_state(obj);
            self.entries.remove(&tmp_id);
            self.at_frame.get_mut(&stamp).unwrap().remove(&tmp_id);
            self.eff.remove(&tmp_id);
            self.outcomes.remove(&tmp_id);
            self.restore(&scratch, stamp);
            e.carried = st.map(|s| (obj, s));
            self.watched.insert(obj);
        }
        self.publish(e)
    }

    /// Author a state-free entry stamped `ahead` frames after the present (opt-in input delay).
    pub fn author_ahead(&mut self, payload: Payload, ahead: u64) -> Entry {
        let stamp = self.frame.max(self.now) + ahead;
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        self.publish(e)
    }

    /// Test fixture for review H5: author with an arbitrary (backdated) stamp.
    pub fn author_backdated(&mut self, payload: Payload, back: u64) -> Entry {
        let stamp = self.frame.max(self.now).saturating_sub(back);
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        self.publish(e)
    }

    /// Author this member's ack: a chain entry stating its horizon (it will author nothing stamped
    /// at or before it), the head it holds of every other chain, and its newest confirmed check
    /// hash (reviews NEW-1, H4).
    pub fn author_ack(&mut self) -> Entry {
        let mut heads = Vec::new();
        for (&q, &held) in &self.contiguous {
            // A vote's frozen head is this replica's last word on that chain in the epoch (R3-1).
            let s = match self.frozen.get(&q) {
                Some(&f) if !self.evicted.contains_key(&q) => held.min(f),
                _ => held,
            };
            if s > 0 && q != self.me && self.cfg.ack_heads {
                match self.hidden.as_ref().and_then(|h| h.get(&q)) {
                    Some(&(hs, hid)) if hs < s => heads.push((q, hs, hid)),
                    _ => heads.push((q, s, self.by_author[&(q, s)])),
                }
            }
        }
        let check = if self.dirty_from.is_none() {
            self.check_hashes.range(..self.conf.min(self.frame)).next_back().map(|(f, h)| (*f, *h))
        } else {
            None
        };
        let stamp = self.frame.max(self.now);
        let horizon = self.now.saturating_sub(1 + self.horizon_lag);
        let e = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp,
            payload: Payload::Ack { horizon, heads, check, epoch: self.epoch },
            carried: None,
        };
        self.publish(e)
    }

    /// Store an entry (received, fetched or my own). Returns entries to broadcast: when it proves
    /// equivocation, both versions and this member's vote.
    pub fn insert(&mut self, e: Entry) -> Vec<Entry> {
        let id = e.id();
        if self.entries.contains_key(&id) || (!self.known.contains(&e.member) && e.member != LOGIC) {
            return vec![];
        }
        if let Some(&other) = self.by_author.get(&(e.member, e.seq)) {
            // Two signed entries for the same (member, seq): a transferable proof (review C2).
            self.entries.insert(id, e.clone());
            self.disputed.insert(id);
            // Another version of a proposal still names a value someone may decide (P2-4).
            self.note_proposal(id);
            let mut out = vec![self.entries[&other].clone(), e.clone()];
            out.extend(self.on_proof(e.member, e.seq, other, id));
            return out;
        }
        self.by_author.insert((e.member, e.seq), id);
        if e.member != self.me {
            self.heard.insert(e.member, self.now);
        }
        if let Some((obj, _)) = e.carried {
            self.watched.insert(obj);
        }
        if let Payload::Touch { obj, .. } = e.payload {
            self.watched.insert(obj);
        }
        // Below the confirmed frame only a horizon violation can be new (design 10.8): never apply.
        if e.stamp < self.conf && !is_protocol(&e.payload) {
            self.violations.insert(id);
        }
        let disputed = self.proofs.get(&e.member).is_some_and(|&(k, _, _)| e.seq >= k)
            || self.evicted.get(&e.member).is_some_and(|&(cut, _)| e.seq > cut);
        if disputed {
            self.disputed.insert(id);
        }
        let (member, seq) = (e.member, e.seq);
        self.entries.insert(id, e);
        self.set_eff(id);
        let mut out = Vec::new();
        self.extend_chain(member, &mut out);
        // An entry past a hole in its author's chain shows the hole: ask for it.
        let c = self.contiguous.get(&member).copied().unwrap_or(0);
        if member != self.me && seq > c + 1 {
            self.want_all(member, c + 1, seq - 1);
        }
        // A removed member's chain may still be missing entries up to its cut.
        if self.evicted.contains_key(&member) {
            self.adopt_cut(member);
        }
        out
    }

    /// Extend `m`'s verified contiguous prefix: each next entry must link to the previous by hash.
    fn extend_chain(&mut self, m: MemberId, out: &mut Vec<Entry>) {
        // A member that asked to join is validated once admitted, when its boundary is known.
        if m != LOGIC && !self.members.contains(&m) && !self.evicted.contains_key(&m) {
            return;
        }
        loop {
            let c = self.contiguous.get(&m).copied().unwrap_or(0);
            // A removed member's chain ends at its cut. A vote only caps what the voter references
            // (stable_seq, author_ack): it keeps processing the chain, so a member wrongly
            // suspected can still take part in the reset that keeps it (P2-4).
            if self.evicted.contains_key(&m) && self.frozen.get(&m).is_some_and(|&f| c >= f) {
                break;
            }
            let Some(&id) = self.by_author.get(&(m, c + 1)) else { break };
            let prev = if c == 0 { 0 } else { self.by_author[&(m, c)] };
            if self.entries[&id].prev != prev {
                // The chain continues from a version of seq c we do not hold: fetch it. And the
                // two signed entries already prove that m signed two versions of seq c (P2-4: a
                // member that forks its vote is proven faulty even if it never sends the other).
                self.want_all(m, c, c);
                if c > 0 && !self.evicted.contains_key(&m) && !self.proofs.contains_key(&m) {
                    let held = self.by_author[&(m, c)];
                    out.push(self.entries[&held].clone());
                    out.push(self.entries[&id].clone());
                    out.extend(self.on_proof(m, c, held, id));
                }
                break;
            }
            self.contiguous.insert(m, c + 1);
            self.validate(m, id, out);
        }
    }

    /// Chain-local rules, in chain order (reviews NEW-1, H5; user rule 14:31Z):
    /// * an ack is void if its stamp goes backwards, or its horizon lags its stamp by more than
    ///   `horizon_slack` frames (so no member can hold a backdating window open);
    /// * any other entry is void if stamped at or before a horizon its author declared earlier.
    fn validate(&mut self, m: MemberId, id: EntryId, out: &mut Vec<Entry>) {
        let e = self.entries[&id].clone();
        let (max_h, last_ack) = self.chain_state.get(&m).copied().unwrap_or((None, 0));
        let valid = match &e.payload {
            Payload::Ack { horizon, .. } => e.stamp >= last_ack && horizon + self.cfg.horizon_slack >= e.stamp,
            Payload::Vote { .. } | Payload::Propose { .. } | Payload::Prevote { .. } | Payload::Precommit { .. } => true,
            // A member admitted by a view change acts from its boundary frame on (10.8).
            _ => max_h.is_none_or(|h| e.stamp > h) && self.admitted.get(&m).is_none_or(|&b| e.stamp >= b),
        };
        if !valid {
            self.stats.entries_rejected_violation += 1;
            self.ev(format!("violation: m{m} s{} stamped {} breaks its chain's horizon rules: void for every member", e.seq, e.stamp));
            self.violations.insert(id);
            self.set_eff(id);
            return;
        }
        match e.payload {
            Payload::Ack { horizon, heads, check, epoch } => {
                if epoch > 0 {
                    self.ack_epoch_from.entry(m).or_default().entry(epoch).or_insert(e.seq);
                }
                let h = max_h.map_or(horizon, |x| x.max(horizon));
                self.chain_state.insert(m, (Some(h), e.stamp));
                self.ack_hist.entry(m).or_default().insert(e.seq, h);
                for &(q, s, hid) in &heads {
                    if m != self.me {
                        let mine = self.contiguous.get(&q).copied().unwrap_or(0);
                        if q != self.me && s > mine {
                            self.want_from(m, q, mine + 1, s);
                        } else if self.by_author.get(&(q, s)).is_some_and(|&x| x != hid) {
                            self.want_from(m, q, s, s);
                        }
                    }
                    // Review R3-1: once m has voted against q, its vote's frozen head is its last
                    // word on q's chain in this epoch; later references never count toward stability.
                    if self.votes.get(&q).is_some_and(|v| v.contains_key(&m)) {
                        continue;
                    }
                    let newer = self.refs.get(&(m, q)).and_then(|r| r.keys().next_back()).is_none_or(|&old| s > old);
                    if newer {
                        self.ref_advanced.insert((m, q), self.now);
                    }
                    self.refs.entry((m, q)).or_default().insert(s, (hid, id));
                }
                if m != self.me {
                    if let Some((f, hh)) = check {
                        self.remote_checks.entry(f).or_default().insert(m, hh);
                    }
                }
                self.recompute_conf();
                self.evaluate_checks();
            }
            Payload::Vote { epoch, .. } | Payload::Propose { epoch, .. } | Payload::Prevote { epoch, .. } | Payload::Precommit { epoch, .. } => {
                if epoch > self.epoch {
                    self.vc_later.entry(epoch).or_default().push(id);
                } else if epoch == self.epoch {
                    self.record_vc(m, id, out);
                }
            }
            _ => {}
        }
    }

    /// The highest seq of `m`'s chain that every member is known to hold with identical ids:
    /// each other member's newest reference at or below my own head, checked against my copy.
    /// Monotone: what was once stable stays stable (review R3-1).
    fn stable_seq(&mut self, m: MemberId) -> u32 {
        // After voting against m, this replica references no more of m's chain than its frozen head.
        let held = self.contiguous.get(&m).copied().unwrap_or(0);
        let mine = match self.frozen.get(&m) {
            Some(&f) if !self.evicted.contains_key(&m) => held.min(f),
            _ => held,
        };
        let mut s = mine;
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for j in members {
            if j == self.me || j == m {
                continue;
            }
            let Some((rs, rid)) = self.refs.get(&(j, m)).and_then(|r| r.range(..=mine).next_back()).map(|(a, b)| (*a, b.0)) else {
                s = 0;
                break;
            };
            if self.by_author.get(&(m, rs)) != Some(&rid) {
                // j holds another version: wait for the proof (fetch it from j).
                self.want_from(j, m, rs, rs);
                s = 0;
                break;
            }
            s = s.min(rs);
        }
        let hi = self.stable_hi.entry(m).or_insert(0);
        *hi = (*hi).max(s);
        *hi
    }

    /// Confirmed frame from stable data only (reviews C1, NEW-1): frame f is confirmed once, for
    /// every member, a stable prefix of its chain declares a horizon at or after f. No decision
    /// depends on it except finality (SCM, ACL, R14 relays) and trimming.
    fn recompute_conf(&mut self) {
        let mut c = u64::MAX;
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for j in members {
            let mut s = self.stable_seq(j);
            // An ack from an epoch newer than mine never counts toward my confirmation (10.8): its
            // author may know of members I do not.
            if let Some(&first) = self.ack_epoch_from.get(&j).and_then(|a| a.range(self.epoch + 1..).map(|(_, s)| s).min()) {
                s = s.min(first.saturating_sub(1));
            }
            let mut h = self.ack_hist.get(&j).and_then(|a| a.range(..=s).next_back()).map(|(_, h)| *h);
            // After precommitting a join, its author's acks count only below the join's boundary.
            if let (Some(hh), Some(&(at, b))) = (h, self.join_caps.get(&j)) {
                if s > at {
                    h = Some(hh.min(b.saturating_sub(1)));
                }
            }
            match (h, self.admitted.get(&j)) {
                (Some(h), Some(&b)) => c = c.min((h + 1).max(b)),
                (None, Some(&b)) => c = c.min(b),
                (Some(h), None) => c = c.min(h + 1),
                (None, None) => return,
            }
        }
        // P2-4: a removed member's entries up to its cut count, and a replica that did not vote
        // may not hold them all yet. Until it does, it confirms only the frames that the acks it
        // holds of that prefix cover: an entry stamped at or before a declared horizon must come
        // before that ack in the chain (8.2).
        for (&x, &(cut, _)) in &self.evicted {
            let held = self.contiguous.get(&x).copied().unwrap_or(0);
            let right = self.cut_ids.get(&x).is_none_or(|&cid| cut == 0 || self.by_author.get(&(x, cut)) == Some(&cid));
            if held < cut || !right {
                let h = self.ack_hist.get(&x).and_then(|a| a.range(..=held.min(cut)).next_back()).map_or(0, |(_, h)| h + 1);
                c = c.min(h);
            }
        }
        if c != u64::MAX && c > self.conf {
            self.conf = c;
        }
    }

    /// Two signed entries of `m` contradict each other (`a` at seq `k`, `b` another version of it
    /// or a successor that names another version): record the proof, withdraw what it disputes,
    /// and vote to remove `m` in this epoch (design 9.3, 10.8).
    fn on_proof(&mut self, m: MemberId, k: u32, a: EntryId, b: EntryId) -> Vec<Entry> {
        if !self.proofs.contains_key(&m) {
            self.stats.equivocations += 1;
            self.ev(format!("EQUIVOCATION proof: member {m} signed two versions of seq {k} ({a:016x} / {b:016x})"));
        }
        let (k0, a0, b0) = match self.proofs.get(&m) {
            Some(&(k1, a1, b1)) if k1 <= k => (k1, a1, b1),
            _ => (k, a, b),
        };
        self.proofs.insert(m, (k0, a0, b0));
        let mut out = Vec::new();
        if m != self.me && self.members.contains(&m) && !self.voted(m) {
            out.extend(self.freeze_and_vote(m, k0));
        }
        self.reconcile(m);
        out
    }

    /// Whether this replica has voted against `m` in the current epoch.
    fn voted(&self, m: MemberId) -> bool {
        self.votes.get(&m).is_some_and(|v| v.contains_key(&self.me))
    }

    /// Freeze what this member will ever reference of m's chain in this epoch, and say so in a
    /// vote. `k0` is the disputed seq of a proof, or 0 for a member presumed crashed or too slow.
    fn freeze_and_vote(&mut self, m: MemberId, k0: u32) -> Option<Entry> {
        if self.withhold_vc {
            return None;
        }
        let f = self.contiguous.get(&m).copied().unwrap_or(0);
        // A faulty member (fixture) votes without freezing: it keeps processing that chain.
        if !self.byz_ally {
            self.frozen.insert(m, f);
        }
        let (f, fid) = match self.hidden.as_ref().and_then(|h| h.get(&m)) {
            Some(&(hs, hid)) if hs < f => (hs, hid),
            _ => (f, if f == 0 { 0 } else { self.by_author[&(m, f)] }),
        };
        let (f, fid) = if self.lie_in_votes { (0, 0) } else { (f, fid) };
        let v = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp: self.frame.max(self.now),
            payload: Payload::Vote { epoch: self.epoch, target: m, seq: k0, frozen: (f, fid) },
            carried: None,
        };
        Some(self.publish(v))
    }

    /// Review R3-2 (a silent member) and R3-9 (a member too slow to keep up), and proofs carried
    /// into a new epoch: vote to remove members. A member none of whose entries has reached this
    /// replica, directly or fetched, for `suspect_after` frames is presumed crashed, unless another
    /// member's acks show more of its chain than this replica holds (then this replica is behind,
    /// and fetches). The view change (10.8) then decides which removals happen and their cuts. A
    /// vote stands until its epoch ends; a member heard from again is kept by a reset (P2-4).
    /// Returns the votes to broadcast.
    pub fn suspects_due(&mut self) -> Vec<Entry> {
        if !self.members.contains(&self.me) {
            return vec![];
        }
        let mut out = Vec::new();
        // A proof outlives an epoch that did not remove its member: vote again.
        let proven: Vec<(MemberId, u32)> = self
            .proofs
            .iter()
            .map(|(m, p)| (*m, p.0))
            .chain(self.liars.iter().map(|(m, p)| (*m, self.entries.get(&p.b).map_or(1, |e| e.seq))))
            .collect();
        for (m, k) in proven {
            if m != self.me && self.members.contains(&m) && !self.voted(m) {
                out.extend(self.freeze_and_vote(m, k.max(1)));
            }
        }
        if self.byz_ally {
            let targets: Vec<MemberId> = self.votes.keys().copied().filter(|t| *t != self.me && self.members.contains(t)).collect();
            for t in targets {
                if !self.voted(t) {
                    out.extend(self.freeze_and_vote(t, 0));
                }
            }
            return out;
        }
        // Removal on silence needs a quorum without the removed member, so a two-member world
        // never suspects: both members pause instead (D7).
        if self.members.len() < 3 {
            return out;
        }
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for j in members {
            if j == self.me || self.voted(j) {
                continue;
            }
            let heard = self.heard.get(&j).copied().unwrap_or(0);
            // Review R3-9 (demotion, 10.9 item 6): a member whose newest validated horizon trails
            // this replica's present by more than half the speculation limit, for as long as a
            // silent member is given, is voted out the same way. Its link cannot carry the session;
            // it can follow as a spectator. A short congestion (c1: about 2 s) does not qualify.
            let h = self.chain_state.get(&j).and_then(|c| c.0).unwrap_or(0);
            let since = if self.now > h + self.cfg.window / 2 {
                *self.lagging.entry(j).or_insert(self.now)
            } else {
                self.lagging.remove(&j);
                self.now
            };
            let silent = self.now >= heard + self.cfg.suspect_after;
            let slow = self.now >= since + self.cfg.suspect_after;
            if !silent && !slow {
                continue;
            }
            let mine = self.contiguous.get(&j).copied().unwrap_or(0);
            let ahead: Vec<(MemberId, u32)> = self
                .refs
                .iter()
                .filter(|((k, a), _)| *a == j && *k != self.me && *k != j)
                .filter_map(|((k, _), r)| r.keys().next_back().filter(|&&s| s > mine).map(|&s| (*k, s)))
                .collect();
            if !ahead.is_empty() {
                for (k, s) in ahead {
                    self.want_from(k, j, mine + 1, s);
                }
                continue;
            }
            if self.withhold_vc {
                continue;
            }
            if silent {
                self.stats.suspicions += 1;
                self.ev(format!("member {j} silent for {} frames: presumed crashed; vote to remove it", self.now - heard));
            } else {
                self.stats.demotions += 1;
                self.ev(format!("member {j}'s horizon has trailed the present by over {} frames for {} frames: vote to demote it", self.cfg.window / 2, self.now - since));
            }
            out.extend(self.freeze_and_vote(j, 0));
        }
        out.extend(self.withholders_due());
        out
    }

    /// P2-4: a member whose acks have referenced no more of some chain for `suspect_after` frames,
    /// while every other member (this replica included) holds and references more of it, stalls that
    /// chain's stability, and so everyone's confirmation, as surely as a silent member. It is voted
    /// out the same way. A member that acks slowly still advances, and is not.
    fn withholders_due(&mut self) -> Vec<Entry> {
        let mut out = Vec::new();
        if self.now < self.withhold_checked + 30 || self.withhold_vc || !self.cfg.ack_heads {
            return out;
        }
        self.withhold_checked = self.now;
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        let newest = |s: &Self, k: MemberId, y: MemberId| s.refs.get(&(k, y)).and_then(|r| r.keys().next_back()).copied().unwrap_or(0);
        // Per chain y: the two lowest references to it among the other members, so "every member but
        // j" is one comparison (O(n^2) per check, not O(n^3)).
        let mut low: BTreeMap<MemberId, ((u32, MemberId), u32)> = BTreeMap::new();
        for &y in &members {
            if self.voted(y) {
                continue;
            }
            let mut best = (u32::MAX, 0);
            let mut second = u32::MAX;
            for &k in &members {
                if k == y || k == self.me {
                    continue;
                }
                let r = newest(self, k, y);
                if r < best.0 {
                    second = best.0;
                    best = (r, k);
                } else if r < second {
                    second = r;
                }
            }
            low.insert(y, (best, second));
        }
        for &j in &members {
            if j == self.me || self.voted(j) {
                continue;
            }
            let mut blocks = None;
            for (&y, &((min1, who), min2)) in &low {
                if y == j {
                    continue;
                }
                let mine = if y == self.me { self.my_seq } else { self.contiguous.get(&y).copied().unwrap_or(0) };
                let others = mine.min(if who == j { min2 } else { min1 });
                let theirs = newest(self, j, y);
                let still = self.ref_advanced.get(&(j, y)).map_or(self.now >= self.cfg.suspect_after, |&t| self.now >= t + self.cfg.suspect_after);
                if theirs < others && still {
                    blocks = Some(y);
                    break;
                }
            }
            if let Some(y) = blocks {
                self.stats.withholders += 1;
                self.ev(format!("member {j} has referenced no more of member {y}'s chain for {} frames while every other member has: vote to remove it", self.cfg.suspect_after));
                out.extend(self.freeze_and_vote(j, 0));
            }
        }
        out
    }

    /// Which of `m`'s entries count after an equivocation proof (review NEW-2): until a view change
    /// removes it, every entry from the disputed seq on is withdrawn unless it is stable (held
    /// identically by every member, so possibly confirmed somewhere); after it, exactly the cut.
    fn reconcile(&mut self, m: MemberId) {
        let k = self.proofs.get(&m).map(|p| p.0);
        let decided = self.evicted.get(&m).map(|p| p.0);
        let stable = self.stable_seq(m);
        let ids: Vec<(u32, EntryId)> = self.entries.iter().filter(|(_, e)| e.member == m).map(|(id, e)| (e.seq, *id)).collect();
        for (seq, id) in ids {
            let mine = self.by_author.get(&(m, seq)) == Some(&id);
            let keep = match (decided, k) {
                (Some(cut), _) => mine && seq <= cut,
                (None, Some(k)) => mine && (seq < k || seq <= stable),
                (None, None) => mine,
            };
            let changed = if keep { self.disputed.remove(&id) } else { self.disputed.insert(id) };
            if changed {
                self.set_eff(id);
            }
        }
    }

    /// Review R3-1: whether `voter`'s frozen head of `target`'s chain extends every reference to
    /// that chain the voter published before its vote. A vote that drops one is a lie, and the
    /// ack it contradicts plus the vote prove its voter faulty.
    fn vote_check(&mut self, target: MemberId, voter: MemberId, frozen: (u32, EntryId)) -> VoteCheck {
        let refs: Vec<(u32, EntryId, EntryId)> =
            self.refs.get(&(voter, target)).map(|r| r.iter().rev().map(|(s, (id, ack))| (*s, *id, *ack)).collect()).unwrap_or_default();
        let (mut s, mut id) = frozen;
        for (rs, rid, ack) in refs {
            if rs > frozen.0 {
                return VoteCheck::Lie(ack);
            }
            while s > rs {
                let Some(e) = self.entries.get(&id) else {
                    self.want_from(voter, target, s, s);
                    return VoteCheck::Missing;
                };
                id = e.prev;
                s -= 1;
            }
            if id != rid {
                return VoteCheck::Lie(ack);
            }
        }
        VoteCheck::Valid
    }

    /// `voter`'s vote `vote` contradicts its own earlier ack `ack`: it is proven faulty.
    fn on_lie(&mut self, voter: MemberId, ack: EntryId, vote: EntryId) {
        if self.liars.contains_key(&voter) || voter == self.me {
            return;
        }
        self.liars.insert(voter, Proof { a: ack, b: vote });
        self.stats.lying_votes += 1;
        self.ev(format!("LYING VOTE: member {voter}'s vote drops entries its own ack referenced: proven faulty"));
    }

    // ---- view change (P2-4, design 10.8) ---------------------------------------------------------

    /// Record a validated membership entry of the current epoch, in chain order.
    fn record_vc(&mut self, m: MemberId, id: EntryId, out: &mut Vec<Entry>) {
        if !self.members.contains(&m) {
            return;
        }
        let e = self.entries[&id].clone();
        match e.payload {
            Payload::Vote { target, frozen, .. } => {
                if target == m || !self.members.contains(&target) || self.votes.get(&target).is_some_and(|v| v.contains_key(&m)) {
                    return;
                }
                let rec = VoteRec { frozen, stamp: e.stamp, id };
                self.votes.entry(target).or_default().insert(m, rec);
                if let VoteCheck::Lie(ack) = self.vote_check(target, m, frozen) {
                    self.on_lie(m, ack, id);
                    if self.members.contains(&self.me) && !self.voted(m) && m != self.me {
                        out.extend(self.freeze_and_vote(m, e.seq));
                    }
                }
            }
            Payload::Propose { round, valid_round, ref change, .. } => {
                self.note_proposal(id);
                if m == self.proposer(self.epoch, round) {
                    self.vc.proposals.entry(round).or_insert((change.value(), valid_round));
                }
            }
            Payload::Prevote { round, value, .. } => {
                self.vc.prevotes.entry(round).or_default().entry(m).or_insert(value);
            }
            Payload::Precommit { round, value, boundary, .. } => {
                self.vc.precommits.entry(round).or_default().entry(m).or_insert(value);
                if boundary > 0 {
                    self.join_caps.entry(m).or_insert((e.seq, boundary));
                }
            }
            _ => {}
        }
    }

    /// Keep the change a proposal entry carries, by value, if its author proposes its round in the
    /// current epoch; any version counts (an equivocating proposer's other version may be decided).
    fn note_proposal(&mut self, id: EntryId) {
        let e = &self.entries[&id];
        let Payload::Propose { epoch, round, ref change, .. } = e.payload else { return };
        let v = change.value();
        self.proposal_ids.entry(v).or_insert(id);
        if epoch == self.epoch && change.epoch == epoch && e.member == self.proposer(epoch, round) {
            let c = change.clone();
            self.vc.changes.entry(v).or_insert(c);
        }
    }

    /// The proposer of `round` of the view change that ends `epoch`: a seeded rotation over the
    /// epoch's members, so no member can choose to propose.
    fn proposer(&self, epoch: u32, round: u32) -> MemberId {
        let mut order: Vec<MemberId> = self.members.iter().copied().collect();
        let salt = self.cfg.seed ^ ((self.world_id as u64) << 40) ^ ((epoch as u64) << 8);
        order.sort_by_key(|&m| (splitmix64(salt ^ (m as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)), m));
        order[round as usize % order.len()]
    }

    /// Members this replica holds a proof against.
    fn proven_set(&self) -> BTreeSet<MemberId> {
        self.proofs.keys().chain(self.liars.keys()).copied().filter(|m| self.members.contains(m)).collect()
    }

    fn proof_of(&self, m: MemberId) -> Proof {
        match self.proofs.get(&m) {
            Some(&(_, a, b)) => Proof { a, b },
            None => self.liars[&m],
        }
    }

    /// This epoch's members outside `excluded`, and (t, q) over them.
    fn base(&self, excluded: &BTreeSet<MemberId>) -> (BTreeSet<MemberId>, usize, usize) {
        let b: BTreeSet<MemberId> = self.members.iter().copied().filter(|m| !excluded.contains(m)).collect();
        let (t, q) = quorum(self.cfg.quorum, b.len());
        (b, t, q)
    }

    /// The members a change excludes from its quorums.
    fn excluded_of(&self, v: u64) -> Option<BTreeSet<MemberId>> {
        self.vc.changes.get(&v).map(|c| c.excluded.iter().map(|(m, _)| *m).collect())
    }

    /// Whether a quorum (counted as the change `v` defines it) sent `v` in `round` of `votes`.
    fn quorum_for(&self, votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>, round: u32, v: u64) -> bool {
        let Some(xs) = self.excluded_of(v) else { return false };
        let (base, _, q) = self.base(&xs);
        votes.get(&round).is_some_and(|r| r.iter().filter(|&(m, &x)| x == v && base.contains(m)).count() >= q)
    }

    /// Members of `base` that sent anything (`nil_only`: nil) in `round` of `votes`.
    fn count_round(votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>, round: u32, base: &BTreeSet<MemberId>, nil_only: bool) -> usize {
        votes.get(&round).map_or(0, |r| r.iter().filter(|&(m, &x)| base.contains(m) && (!nil_only || x == 0)).count())
    }

    /// The deepest point of `x`'s chain that at least `k` of the given frozen heads extend
    /// (design 10.8). With at most t faulty voters among them and `k` = their number minus t,
    /// every honest head extends it, so it keeps every entry of `x` stable anywhere; and since
    /// they number more than 2t, two branches can never both reach `k`. None while an entry
    /// needed to walk back is missing (it is fetched from the voter).
    fn robust_cut(&mut self, x: MemberId, heads: &[(MemberId, (u32, EntryId))], k: usize) -> Option<(u32, EntryId)> {
        let mut cur: Vec<(MemberId, u32, EntryId)> = heads.iter().map(|(v, (s, id))| (*v, *s, *id)).collect();
        let mut c = cur.iter().map(|h| h.1).max().unwrap_or(0);
        loop {
            if c == 0 || k == 0 {
                return Some((0, 0));
            }
            for h in cur.iter_mut() {
                while h.1 > c {
                    let Some(e) = self.entries.get(&h.2) else {
                        let (voter, s) = (h.0, h.1);
                        self.want_from(voter, x, s, s);
                        return None;
                    };
                    h.2 = e.prev;
                    h.1 -= 1;
                }
            }
            let mut groups: BTreeMap<EntryId, usize> = BTreeMap::new();
            for h in &cur {
                if h.1 == c {
                    *groups.entry(h.2).or_default() += 1;
                }
            }
            if let Some((&id, _)) = groups.iter().find(|&(_, &n)| n >= k) {
                return Some((c, id));
            }
            c -= 1;
        }
    }

    /// Whether `p` proves `m` faulty (see [`Proof`]). None while an entry it names is missing.
    fn verify_proof(&mut self, m: MemberId, p: Proof) -> Option<bool> {
        let (a, b) = (self.entries.get(&p.a)?.clone(), self.entries.get(&p.b)?.clone());
        if a.member != m || b.member != m {
            return Some(false);
        }
        if a.seq == b.seq && p.a != p.b {
            return Some(true);
        }
        if b.seq == a.seq + 1 && b.prev != p.a {
            return Some(true);
        }
        if let (Payload::Ack { heads, .. }, Payload::Vote { target, frozen, .. }) = (&a.payload, &b.payload) {
            if a.seq < b.seq {
                if let Some(&(_, s, rid)) = heads.iter().find(|h| h.0 == *target) {
                    if s > frozen.0 {
                        return Some(true);
                    }
                    let (mut fs, mut fid) = *frozen;
                    while fs > s {
                        fid = self.entries.get(&fid)?.prev;
                        fs -= 1;
                    }
                    return Some(fid != rid);
                }
            }
        }
        Some(false)
    }

    /// What this replica proposes when it is the round's proposer: remove every member that a
    /// quorum of usable votes names (each with its cut), excluding from the quorum the members it
    /// holds proofs against; with nothing to remove, a reset if a vote of this epoch stands
    /// against a member this replica hears from (it was wrongly suspected, or came back).
    fn build_change(&mut self) -> Option<Change> {
        // Only proofs that their two entries show alone, so other worlds can check the certificate.
        let mut proven = BTreeSet::new();
        let mut excluded = Vec::new();
        for m in self.proven_set() {
            let p = self.proof_of(m);
            if let (Some(a), Some(b)) = (self.entries.get(&p.a), self.entries.get(&p.b)) {
                if proof_self_contained(m, p, a, b) {
                    proven.insert(m);
                    excluded.push((m, p));
                }
            }
        }
        let (base, t, q) = self.base(&proven);
        let mut remove = Vec::new();
        let targets: Vec<MemberId> = self.votes.keys().copied().collect();
        for x in targets {
            let recs: Vec<(MemberId, VoteRec)> = self.votes[&x].iter().filter(|(v, _)| base.contains(v)).map(|(v, r)| (*v, *r)).collect();
            let mut usable = Vec::new();
            for (v, r) in recs {
                if self.vote_check(x, v, r.frozen) == VoteCheck::Valid {
                    usable.push((v, r));
                }
            }
            if usable.len() < q {
                continue;
            }
            let heads: Vec<(MemberId, (u32, EntryId))> = usable.iter().map(|(v, r)| (*v, r.frozen)).collect();
            let Some(cut) = self.robust_cut(x, &heads, usable.len() - t) else { continue };
            remove.push(Removal { member: x, votes: usable.iter().map(|(_, r)| r.id).collect(), cut });
        }
        let add: Vec<MemberId> = self.join_requests.iter().copied().filter(|j| !self.members.contains(j) && !self.evicted.contains_key(j)).collect();
        let last = self.decided.last().map_or(0, |d| d.0.boundary);
        let boundary = if add.is_empty() { self.now.max(last) } else { (self.now + self.cfg.join_lead).max(last) };
        if remove.is_empty() && add.is_empty() {
            let now = self.now;
            let reset = self.votes.iter().any(|(y, vs)| {
                let heard = self.heard.get(y).copied().unwrap_or(0);
                !proven.contains(y) && now < heard + self.cfg.suspect_after && vs.get(&self.me).is_none_or(|r| heard > r.stamp)
            });
            if !reset {
                return None;
            }
        }
        Some(Change { epoch: self.epoch, remove, add, boundary, excluded })
    }

    /// Whether `c` is a valid change for this epoch. Deterministic once this replica holds every
    /// entry it names (and has validated the voters' chains up to their votes); None until then.
    fn change_valid(&mut self, c: &Change) -> Option<bool> {
        let last = self.decided.last().map_or(0, |d| d.0.boundary);
        if c.epoch != self.epoch || c.boundary < last || c.boundary > self.now + 2 * self.cfg.join_lead {
            return Some(false);
        }
        // Joins: requests this replica holds, from non-members, with a boundary still ahead of it
        // and not too far ahead (10.8).
        let mut joiners = BTreeSet::new();
        for &j in &c.add {
            if self.members.contains(&j) || self.evicted.contains_key(&j) || !joiners.insert(j) {
                return Some(false);
            }
            if !self.join_requests.contains(&j) {
                return None;
            }
        }
        if !c.add.is_empty() && c.boundary <= self.now + 1 {
            return Some(false);
        }
        for &(m, p) in &c.excluded {
            let (Some(a), Some(b)) = (self.entries.get(&p.a), self.entries.get(&p.b)) else { return None };
            if !proof_self_contained(m, p, a, b) {
                return Some(false);
            }
        }
        let mut xs = BTreeSet::new();
        for &(m, p) in &c.excluded {
            if !self.members.contains(&m) || !xs.insert(m) || !self.verify_proof(m, p)? {
                return Some(false);
            }
        }
        let (base, t, q) = self.base(&xs);
        let mut seen = BTreeSet::new();
        for r in &c.remove {
            let x = r.member;
            if !self.members.contains(&x) || !seen.insert(x) || r.votes.len() < q {
                return Some(false);
            }
            let mut voters = BTreeSet::new();
            let mut heads = Vec::new();
            for &vid in &r.votes {
                let e = self.entries.get(&vid)?.clone();
                let Payload::Vote { epoch, target, frozen, .. } = e.payload else { return Some(false) };
                if epoch != self.epoch || target != x || !base.contains(&e.member) || !voters.insert(e.member) {
                    return Some(false);
                }
                // The voter's first vote against x in this epoch, in the copy of its chain I hold.
                match self.votes.get(&x).and_then(|v| v.get(&e.member)) {
                    Some(rec) if rec.id == vid => {}
                    Some(_) => return Some(false),
                    None => return None,
                }
                match self.vote_check(x, e.member, frozen) {
                    VoteCheck::Valid => {}
                    VoteCheck::Missing => return None,
                    VoteCheck::Lie(ack) => {
                        self.on_lie(e.member, ack, vid);
                        return Some(false);
                    }
                }
                heads.push((e.member, frozen));
            }
            if self.robust_cut(x, &heads, heads.len() - t)? != r.cut {
                return Some(false);
            }
        }
        Some(true)
    }

    fn author_vc(&mut self, payload: Payload) -> Entry {
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp: self.frame.max(self.now), payload, carried: None };
        self.publish(e)
    }

    fn start_round(&mut self, r: u32) {
        self.vc.round = r;
        self.vc.step = Step::Propose;
        self.vc.round_at = self.now;
        self.vc.proposed = false;
        self.stats.max_round = self.stats.max_round.max(r);
    }

    fn prevote(&mut self, v: u64, out: &mut Vec<Entry>) {
        self.vc.step = Step::Prevote;
        if !self.withhold_vc {
            let (epoch, round) = (self.epoch, self.vc.round);
            out.push(self.author_vc(Payload::Prevote { epoch, round, value: v }));
        }
    }

    fn precommit(&mut self, v: u64, out: &mut Vec<Entry>) {
        self.precommit_join(v, 0, out);
    }

    fn precommit_join(&mut self, v: u64, boundary: u64, out: &mut Vec<Entry>) {
        self.vc.step = Step::Precommit;
        if !self.withhold_vc {
            let (epoch, round) = (self.epoch, self.vc.round);
            out.push(self.author_vc(Payload::Precommit { epoch, round, value: v, boundary }));
        }
    }

    /// Run this epoch's view change and return the entries to broadcast. Called every tick:
    /// Tendermint's rules (algorithm 1, lines 11-65) over chain entries, timeouts in clock frames.
    pub fn view_change_due(&mut self) -> Vec<Entry> {
        let mut out = Vec::new();
        // Everyone that holds the log follows its decisions, members or not (a joiner learns so).
        if !self.byz_ally && self.try_decide(&mut out) {
            return out;
        }
        if !self.members.contains(&self.me) {
            return out;
        }
        if self.byz_ally {
            // Prevote and precommit every proposal seen, first per round (a linear chain).
            let props: Vec<(u32, u64)> = self.vc.proposals.iter().map(|(r, (v, _))| (*r, *v)).collect();
            for (r, v) in props {
                let me = self.me;
                if !self.vc.prevotes.get(&r).is_some_and(|x| x.contains_key(&me)) {
                    let epoch = self.epoch;
                    out.push(self.author_vc(Payload::Prevote { epoch, round: r, value: v }));
                    let boundary = self.vc.changes.get(&v).filter(|c| !c.add.is_empty()).map_or(0, |c| c.boundary);
                    out.push(self.author_vc(Payload::Precommit { epoch, round: r, value: v, boundary }));
                }
            }
            // Drive every round another member gave up on: echo its nil prevote or precommit where no
            // proposal is held, so a side of a partition that is no quorum alone still reaches a round
            // one of its own members proposes in.
            let (me, epoch) = (self.me, self.epoch);
            let nil_in = |votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>| -> Vec<u32> {
                votes.iter().filter(|(_, ms)| !ms.contains_key(&me) && ms.values().any(|v| *v == 0)).map(|(r, _)| *r).collect()
            };
            for r in nil_in(&self.vc.prevotes) {
                if !self.vc.proposals.contains_key(&r) {
                    out.push(self.author_vc(Payload::Prevote { epoch, round: r, value: 0 }));
                }
            }
            for r in nil_in(&self.vc.precommits) {
                if !self.vc.proposals.contains_key(&r) {
                    out.push(self.author_vc(Payload::Precommit { epoch, round: r, value: 0, boundary: 0 }));
                }
            }
            return out;
        }
        if self.vc.since.is_none() {
            let joins = self.join_requests.iter().any(|j| !self.members.contains(j) && !self.evicted.contains_key(j));
            let busy = joins || !self.votes.is_empty() || !self.vc.proposals.is_empty() || !self.vc.prevotes.is_empty() || !self.vc.precommits.is_empty();
            if !busy {
                return out;
            }
            self.vc.since = Some(self.now);
            self.start_round(0);
        }
        for _ in 0..16 {
            let before = (self.vc.round, self.vc.step, self.vc.proposed, self.vc.polka_done.len(), self.vc.prevote_timer.len(), self.vc.precommit_timer.len());
            self.vc_rules(&mut out);
            if self.try_decide(&mut out) {
                return out;
            }
            if before == (self.vc.round, self.vc.step, self.vc.proposed, self.vc.polka_done.len(), self.vc.prevote_timer.len(), self.vc.precommit_timer.len()) {
                break;
            }
        }
        out
    }

    fn vc_rules(&mut self, out: &mut Vec<Entry>) {
        let (e, r) = (self.epoch, self.vc.round);
        let proven = self.proven_set();
        let (any_base, t_any, q_any) = self.base(&proven);
        // Lines 55-56: more than t members are in a later round: go there.
        let mut hi: BTreeMap<MemberId, u32> = BTreeMap::new();
        for votes in [&self.vc.prevotes, &self.vc.precommits] {
            for (&rr, ms) in votes.range(r + 1..) {
                for m in ms.keys().filter(|m| any_base.contains(m)) {
                    let x = hi.entry(*m).or_insert(rr);
                    *x = (*x).max(rr);
                }
            }
        }
        let mut later: Vec<u32> = hi.into_values().collect();
        later.sort_unstable_by(|a, b| b.cmp(a));
        if later.len() > t_any {
            self.start_round(later[t_any]);
            return;
        }
        // Lines 14-19: the round's proposer proposes its valid value, or a new one.
        if self.vc.step == Step::Propose && !self.vc.proposed && !self.withhold_vc && self.proposer(e, r) == self.me {
            let (vr, change) = match self.vc.valid {
                Some((v, vr)) => (vr, self.vc.changes.get(&v).cloned()),
                None => (NO_ROUND, self.build_change()),
            };
            // Members that fail together are suspected within a few frames of each other: give the
            // votes on every member this replica itself suspects a few frames to arrive, so that one
            // change removes them all.
            let me = self.me;
            let gathering = vr == NO_ROUND
                && self.now < self.vc.round_at + VC_GATHER
                && change.as_ref().is_some_and(|c| self.votes.iter().any(|(y, vs)| vs.contains_key(&me) && !c.remove.iter().any(|x| x.member == *y)));
            if let Some(c) = change.filter(|_| !gathering) {
                self.vc.proposed = true;
                self.ev(format!("view change: propose in round {r}: remove {:?}, excluded {:?}", c.remove.iter().map(|x| (x.member, x.cut.0)).collect::<Vec<_>>(), c.excluded.iter().map(|x| x.0).collect::<Vec<_>>()));
                out.push(self.author_vc(Payload::Propose { epoch: e, round: r, valid_round: vr, change: c }));
            }
        }
        // Lines 22-33: prevote on the round's proposal.
        if self.vc.step == Step::Propose {
            if let Some(&(v, vr)) = self.vc.proposals.get(&r) {
                let c = self.vc.changes.get(&v).cloned();
                let ok = match c {
                    Some(c) => self.change_valid(&c),
                    None => Some(false),
                };
                let decision = match ok {
                    None => None,
                    Some(ok) if vr == NO_ROUND => Some(ok && self.vc.locked.is_none_or(|(lv, _)| lv == v)),
                    Some(ok) if vr < r && self.quorum_for(&self.vc.prevotes, vr, v) => {
                        Some(ok && self.vc.locked.is_none_or(|(lv, lr)| lr <= vr || lv == v))
                    }
                    Some(_) => None,
                };
                if let Some(yes) = decision {
                    self.prevote(if yes { v } else { 0 }, out);
                }
            }
        }
        // Lines 34-35: a quorum of prevotes of any kind arms the prevote timeout.
        if self.vc.step == Step::Prevote && !self.vc.prevote_timer.contains_key(&r) && Self::count_round(&self.vc.prevotes, r, &any_base, false) >= q_any {
            self.vc.prevote_timer.insert(r, self.now);
        }
        // Lines 36-43: a quorum prevoted the round's proposal: lock it and precommit it.
        if self.vc.step != Step::Propose && !self.vc.polka_done.contains(&r) {
            if let Some(&(v, _)) = self.vc.proposals.get(&r) {
                if self.quorum_for(&self.vc.prevotes, r, v) {
                    let c = self.vc.changes.get(&v).cloned().unwrap_or_default();
                    let in_time = c.add.is_empty() || c.boundary > self.now;
                    if self.vc.changes.contains_key(&v) && in_time && self.change_valid(&c) == Some(true) {
                        self.vc.polka_done.insert(r);
                        if self.vc.step == Step::Prevote {
                            self.vc.locked = Some((v, r));
                            self.precommit_join(v, if c.add.is_empty() { 0 } else { c.boundary }, out);
                        }
                        self.vc.valid = Some((v, r));
                    }
                }
            }
        }
        // Lines 44-46: a quorum prevoted nil.
        if self.vc.step == Step::Prevote && Self::count_round(&self.vc.prevotes, r, &any_base, true) >= q_any {
            self.precommit(0, out);
        }
        // Lines 47-48: a quorum of precommits of any kind arms the precommit timeout; a quorum of
        // nil precommits ends the round at once (no value can be decided in it).
        if Self::count_round(&self.vc.precommits, r, &any_base, true) >= q_any {
            self.start_round(r + 1);
            return;
        }
        if !self.vc.precommit_timer.contains_key(&r) && Self::count_round(&self.vc.precommits, r, &any_base, false) >= q_any {
            self.vc.precommit_timer.insert(r, self.now);
        }
        // Lines 57-65: timeouts, lengthening with the round.
        let to = self.cfg.vc_timeout + self.cfg.vc_timeout_step * r as u64;
        if self.vc.step == Step::Propose && self.now >= self.vc.round_at + to {
            self.prevote(0, out);
        }
        if self.vc.step == Step::Prevote && self.vc.prevote_timer.get(&r).is_some_and(|&a| self.now >= a + to) {
            self.precommit(0, out);
        }
        if self.vc.precommit_timer.get(&r).is_some_and(|&a| self.now >= a + to) {
            self.start_round(r + 1);
        }
        let _ = t_any;
    }

    /// Lines 49-54: a quorum precommitted a value in some round: decide it and apply it. A value
    /// this replica holds no proposal for is fetched from those that precommitted it.
    fn try_decide(&mut self, out: &mut Vec<Entry>) -> bool {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for (&rr, ms) in &self.vc.precommits {
            let mut vals: BTreeMap<u64, Vec<MemberId>> = BTreeMap::new();
            for (&m, &v) in ms {
                if v != 0 {
                    vals.entry(v).or_default().push(m);
                }
            }
            for (v, ms) in vals {
                if !self.vc.changes.contains_key(&v) {
                    unknown.push((v, ms));
                } else if self.quorum_for(&self.vc.precommits, rr, v) {
                    candidates.push((rr, v));
                }
            }
        }
        for (v, ms) in unknown {
            for m in ms {
                if m != self.me && self.value_asked.insert((m, v)) {
                    self.value_wants.push((m, v));
                }
            }
        }
        for (rr, v) in candidates {
            // A change's exclusions set its quorum: decide it only once every one is proved, or a
            // faulty minority could exclude honest members and decide alone (P2-4).
            let ex = self.vc.changes[&v].excluded.clone();
            let mut proved = true;
            for (m, p) in ex {
                if self.verify_proof(m, p) != Some(true) {
                    proved = false;
                }
            }
            if proved {
                let c = self.vc.changes[&v].clone();
                self.apply_change(c, rr, out);
                return true;
            }
        }
        false
    }

    /// Apply a decided change: the next epoch's members, each removed member's cut, a new epoch
    /// with no votes, and the epoch certificate's material.
    fn apply_change(&mut self, c: Change, round: u32, out: &mut Vec<Entry>) {
        let v = c.value();
        let xs: BTreeSet<MemberId> = c.excluded.iter().map(|(m, _)| *m).collect();
        let sigs: Vec<(MemberId, u64)> = self
            .vc
            .precommits
            .get(&round)
            .map(|ms| ms.iter().filter(|&(m, &x)| x == v && !xs.contains(m)).map(|(&m, _)| (m, vc_sig(self.world_id, m, self.epoch, round, v))).collect())
            .unwrap_or_default();
        let removed: Vec<MemberId> = c.remove.iter().map(|r| r.member).collect();
        for r in &c.remove {
            self.members.remove(&r.member);
            self.frozen.insert(r.member, r.cut.0);
            self.cut_ids.insert(r.member, r.cut.1);
            self.evicted.insert(r.member, (r.cut.0, 0));
        }
        for &j in &c.add {
            self.members.insert(j);
            self.known.insert(j);
            self.admitted.insert(j, c.boundary);
            self.followers.remove(&j);
            self.join_requests.remove(&j);
        }
        self.join_caps.clear();
        let from = self.epoch;
        self.decided.push((c.clone(), round, sigs));
        self.epoch += 1;
        self.vc_log.push((self.epoch, self.now, round, removed.clone()));
        self.stats.view_changes += 1;
        if removed.is_empty() && c.add.is_empty() {
            self.stats.resets += 1;
        }
        self.ev(format!(
            "VIEW CHANGE decided (epoch {from} -> {}, round {round}): removed {:?} with cuts {:?}, excluded by proof {:?}, added {:?} from frame {}; members {:?}",
            self.epoch,
            removed,
            c.remove.iter().map(|r| r.cut.0).collect::<Vec<_>>(),
            xs,
            c.add,
            c.boundary,
            self.members
        ));
        // A new epoch: the old one's votes, freezes and view-change state end.
        self.votes.clear();
        self.vc = Vc::default();
        let unfreeze: Vec<MemberId> = self.frozen.keys().copied().filter(|m| !self.evicted.contains_key(m)).collect();
        for m in unfreeze {
            self.frozen.remove(&m);
        }
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for m in members {
            self.extend_chain(m, out);
        }
        for x in removed {
            self.adopt_cut(x);
        }
        // An admitted member's entries act from now on (those stamped before its boundary are void).
        let joined: Vec<EntryId> = self.entries.iter().filter(|(_, e)| c.add.contains(&e.member)).map(|(id, _)| *id).collect();
        for id in joined {
            self.set_eff(id);
        }
        if let Some(ids) = self.vc_later.remove(&self.epoch) {
            for id in ids {
                let m = self.entries[&id].member;
                self.record_vc(m, id, out);
            }
        }
        self.recompute_conf();
    }

    /// Make the version of removed member `x`'s chain that ends at its cut the one this replica
    /// holds: walk back from the cut id, fetch what is missing, replace any other version (8.2
    /// verdicts are re-derived along it), and keep exactly the cut (P2-4).
    fn adopt_cut(&mut self, x: MemberId) {
        if self.cut_done.contains(&x) {
            return;
        }
        let (Some(&(cut, _)), Some(&cid)) = (self.evicted.get(&x), self.cut_ids.get(&x)) else { return };
        let held = self.contiguous.get(&x).copied().unwrap_or(0);
        if cut > 0 && !(held >= cut && self.by_author.get(&(x, cut)) == Some(&cid)) {
            let mut ids = Vec::with_capacity(cut as usize);
            let (mut s, mut id) = (cut, cid);
            loop {
                let Some(e) = self.entries.get(&id) else {
                    self.want_all(x, s, s);
                    return;
                };
                ids.push((s, id));
                if s == 1 {
                    break;
                }
                id = e.prev;
                s -= 1;
            }
            let mut changed = false;
            for &(s, id) in &ids {
                if self.by_author.get(&(x, s)) != Some(&id) {
                    self.by_author.insert((x, s), id);
                    changed = true;
                }
            }
            if changed {
                self.contiguous.insert(x, 0);
                self.chain_state.remove(&x);
                self.ack_hist.remove(&x);
            }
            let mut out = Vec::new();
            self.extend_chain(x, &mut out);
        }
        self.reconcile(x);
        if self.contiguous.get(&x).copied().unwrap_or(0) >= cut {
            let v = self.ack_hist.get(&x).and_then(|a| a.range(..=cut).next_back()).map_or(0, |(_, h)| h + 1);
            self.evicted.insert(x, (cut, v));
            self.cut_done.insert(x);
            self.ev(format!("member {x} evicted (epoch {}): its chain is cut at seq {cut}; it no longer counts from frame {v}", self.epoch));
        }
    }

    /// The certificate of the view change that started `epoch` (R3-8b).
    pub fn epoch_cert(&self, epoch: u32) -> Option<EpochCert> {
        let (c, round, sigs) = self.decided.get(epoch.checked_sub(1)? as usize)?;
        let mut proofs = Vec::new();
        for (_, p) in &c.excluded {
            proofs.push(self.entries.get(&p.a)?.clone());
            proofs.push(self.entries.get(&p.b)?.clone());
        }
        Some(EpochCert { world: self.world_id, epoch, change: c.clone(), round: *round, members: self.members_at(epoch)?, precommits: sigs.clone(), proofs })
    }

    /// This world's certificates for epochs 1..=`epoch`: what a fact signed in `epoch` carries so
    /// any world can check it from this world's genesis (P2-5).
    pub fn epoch_certs_upto(&self, epoch: u32) -> Option<Vec<EpochCert>> {
        (1..=epoch).map(|e| self.epoch_cert(e)).collect()
    }

    /// The epoch of this world active at `frame`, by the boundaries of the decided changes.
    pub fn epoch_at(&self, frame: u64) -> u32 {
        self.decided.iter().take_while(|d| d.0.boundary <= frame).count() as u32
    }

    /// A certificate for `fact`, a fact about `frame`: Attest entries of a strict majority of the
    /// members of the epoch active at that frame (P2-5's epoch-at-frame rule). Members that the next
    /// epoch excluded by proof do not count in that majority's size.
    pub fn cert_at(&self, fact: u64, frame: u64) -> Option<(u32, Vec<(MemberId, u64)>)> {
        let e = self.epoch_at(frame);
        let mut members: BTreeSet<MemberId> = self.members_at(e)?.into_iter().collect();
        if let Some((next, _, _)) = self.decided.get(e as usize) {
            for (m, _) in &next.excluded {
                members.remove(m);
            }
        }
        let mut c: BTreeMap<MemberId, u64> = BTreeMap::new();
        for (id, en) in &self.entries {
            if let Payload::Attest { fact: f, sig } = en.payload {
                if f == fact && !self.is_void(*id) && members.contains(&en.member) {
                    c.insert(en.member, sig);
                }
            }
        }
        (c.len() * 2 > members.len()).then(|| (e, c.into_iter().collect()))
    }

    /// The members of `epoch`, replayed from the genesis members and the decided changes.
    pub fn members_at(&self, epoch: u32) -> Option<Vec<MemberId>> {
        if epoch > self.epoch {
            return None;
        }
        let mut m: BTreeSet<MemberId> = self.genesis.iter().copied().collect();
        for (c, _, _) in self.decided.iter().take(epoch as usize) {
            for r in &c.remove {
                m.remove(&r.member);
            }
            for a in &c.add {
                m.insert(*a);
            }
        }
        Some(m.into_iter().collect())
    }

    /// A non-member asks to join (P2-4): keep its request, store its entries, send it the log.
    pub fn on_join(&mut self, j: MemberId) {
        if !self.members.contains(&j) && !self.evicted.contains_key(&j) && j != self.me {
            self.join_requests.insert(j);
            self.follow(j);
        }
    }

    /// Send `j` this replica's entries and store its own (a member about to join, in the spike).
    pub fn follow(&mut self, j: MemberId) {
        if !self.members.contains(&j) && j != self.me {
            self.known.insert(j);
            self.followers.insert(j);
        }
    }

    /// Who this replica sends its entries to: the members and the followers.
    pub fn audience(&self) -> Vec<MemberId> {
        self.members.iter().chain(self.followers.iter()).copied().filter(|m| *m != self.me).collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// Test fixture (D80 bridge): from now on, acks and votes report no more of every other
    /// member's chain than this replica holds now, as a faulty member hiding what it hears. It keeps
    /// processing everything. `unfreeze_others` ends it.
    pub fn freeze_others(&mut self) {
        let heads: BTreeMap<MemberId, (u32, EntryId)> = self
            .contiguous
            .iter()
            .filter(|(m, s)| **m != self.me && **s > 0)
            .map(|(m, s)| (*m, (*s, self.by_author[&(*m, *s)])))
            .collect();
        self.hidden = Some(heads);
    }

    pub fn unfreeze_others(&mut self) {
        self.hidden = None;
    }

    /// Diagnostics: this replica's view-change state and how much of each chain it holds.
    pub fn vc_debug(&self) -> String {
        let pv: Vec<String> = self.vc.prevotes.iter().map(|(r, m)| format!("r{r}:{:?}", m.iter().map(|(k, v)| (*k, *v != 0)).collect::<Vec<_>>())).collect();
        let pc: Vec<String> = self.vc.precommits.iter().map(|(r, m)| format!("r{r}:{:?}", m.iter().map(|(k, v)| (*k, *v != 0)).collect::<Vec<_>>())).collect();
        format!(
            "now {} epoch {} round {} step {:?} since {:?} proposals {:?} prevotes [{}] precommits [{}] votes {:?} contiguous {:?} heard {:?}",
            self.now,
            self.epoch,
            self.vc.round,
            self.vc.step,
            self.vc.since,
            self.vc.proposals.keys().collect::<Vec<_>>(),
            pv.join(" "),
            pc.join(" "),
            self.votes.iter().map(|(t, v)| (*t, v.keys().copied().collect::<Vec<_>>())).collect::<Vec<_>>(),
            self.contiguous,
            self.heard
        )
    }

    /// The proposer of `round` of the view change ending `epoch`, if `epoch` is the current one.
    pub fn proposer_of(&self, epoch: u32, round: u32) -> Option<MemberId> {
        (epoch == self.epoch).then(|| self.proposer(epoch, round))
    }

    /// Answer `Msg::FetchValue`: the proposal entry carrying `value`, any version.
    pub fn on_fetch_value(&self, value: u64) -> Vec<Msg> {
        self.proposal_ids.get(&value).and_then(|id| self.entries.get(id)).map(|e| vec![Msg::Entry(e.clone())]).unwrap_or_default()
    }

    fn want_from(&mut self, holder: MemberId, author: MemberId, from: u32, to: u32) {
        if holder != self.me && from <= to {
            self.wants.push((holder, author, from, to));
        }
    }

    fn want_all(&mut self, author: MemberId, from: u32, to: u32) {
        let known: Vec<MemberId> = self.known.iter().copied().collect();
        for j in known {
            self.want_from(j, author, from, to);
        }
    }

    pub fn on_fetch(&self, author: MemberId, from: u32, to: u32) -> Vec<Msg> {
        // At most 16 entries per reply (review R3-9): the requester asks again for the rest.
        (from..=to.min(from + 15))
            .filter_map(|s| self.by_author.get(&(author, s)).and_then(|id| self.entries.get(id)))
            .map(|e| Msg::Entry(e.clone()))
            .collect()
    }

    /// Fetch requests from gap and conflict detection: (who to ask, request), rate-limited.
    pub fn take_fetches(&mut self, now_ms: u64) -> Vec<(MemberId, Msg)> {
        let mut out = Vec::new();
        for (to, author, from, upto) in std::mem::take(&mut self.wants) {
            // Review R3-9: the same range is re-asked with exponential backoff, 60 ms doubling up to
            // 2 s, so a congested link is not flooded with repeats of requests whose replies are
            // still queued (a fixed 60 ms was shorter than a congested round trip).
            let wait = match self.fetch_asked.get(&(to, author)).copied() {
                Some((f, t, w)) if f == from => {
                    if now_ms < t + w {
                        continue;
                    }
                    (w * 2).min(2000)
                }
                _ => 60,
            };
            self.fetch_asked.insert((to, author), (from, now_ms, wait));
            out.push((to, Msg::Fetch { author, from, to: upto }));
        }
        for (to, value) in std::mem::take(&mut self.value_wants) {
            out.push((to, Msg::FetchValue { value }));
        }
        out
    }

    /// Compare check hashes only for confirmed frames this member has finished simulating with the
    /// complete log (viz-1: never while a re-simulation is pending).
    fn evaluate_checks(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let fin = self.conf.min(self.frame);
        let frames: Vec<u64> = self.remote_checks.range(..fin).map(|(f, _)| *f).collect();
        for f in frames {
            let Some(&mine) = self.check_hashes.get(&f) else {
                self.remote_checks.remove(&f);
                continue;
            };
            let theirs = self.remote_checks.remove(&f).unwrap_or_default();
            for (m, h) in theirs {
                if !self.evaluated.insert((f, m)) {
                    continue;
                }
                if h == mine {
                    self.stats.checks_ok += 1;
                } else {
                    self.stats.checks_bad += 1;
                    self.events.push(format!(
                        "[w{} m{}] NONDETERMINISM at confirmed frame {f}: mine {mine:016x} member {m} {h:016x}",
                        self.world_id, self.me
                    ));
                }
            }
        }
    }

    // ---- R14 attestations ------------------------------------------------------------------------

    /// Sign, as chain entries, the cross-world facts this replica has confirmed: a pending
    /// departure (for the destination) and an arrival's outcome (for the source). A relay needs a
    /// strict majority of them (reviews NEW-3, NEW-4).
    pub fn attest_due(&mut self) -> Vec<Entry> {
        if self.dirty_from.is_some() || !self.members.contains(&self.me) {
            return vec![];
        }
        let mut facts = Vec::new();
        for esc in self.t.escrow.values() {
            if let (EscrowStatus::Pending, Some(rec)) = (esc.status, esc.record) {
                if esc.depart_frame < self.conf {
                    let chain = self.chain_hash_of(rec.global);
                    facts.push(fact_departed(self.world_id, esc.depart, &rec, esc.dest, esc.deadline, chain));
                }
            }
        }
        // P2-5: a committed transfer is the hop the destination's next departure needs. P25-4: each
        // commit is queued once, when its frame is confirmed.
        self.confirm_effects();
        facts.append(&mut self.commits_due);
        for (t, a) in &self.t.arrivals {
            let (frame, accepted) = match a {
                Arrival::Accepted { frame, .. } => (*frame, true),
                Arrival::Rejected { frame, .. } => (*frame, false),
            };
            if frame < self.conf {
                facts.push(fact_verdict(self.world_id, *t, accepted));
            }
        }
        let mut out = Vec::new();
        for fact in facts {
            if self.attested.insert(fact) {
                let e = Entry {
                    member: self.me,
                    seq: self.my_seq + 1,
                    prev: self.my_last,
                    stamp: self.frame.max(self.now),
                    payload: Payload::Attest { fact, sig: member_sig(self.world_id, self.me, fact) },
                    carried: None,
                };
                out.push(self.publish(e));
            }
        }
        out
    }

    /// A certificate for `fact` from this world's members, once a strict majority has signed it.
    pub fn cert(&self, fact: u64) -> Option<Vec<(MemberId, u64)>> {
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        let mut c: BTreeMap<MemberId, u64> = BTreeMap::new();
        for (id, e) in &self.entries {
            if let Payload::Attest { fact: f, sig } = e.payload {
                if f == fact && !self.is_void(*id) && members.contains(&e.member) {
                    c.insert(e.member, sig);
                }
            }
        }
        let c: Vec<(MemberId, u64)> = c.into_iter().collect();
        cert_ok(&members, self.world_id, fact, &c).then_some(c)
    }

    // ---- P2-5 provenance ---------------------------------------------------------------------------

    /// Act on the cross-world effects of frames that are now confirmed and simulated in their final
    /// form: queue their commit facts for attestation, and keep their fork proofs.
    fn confirm_effects(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let upto = self.conf.min(self.frame);
        while let Some(e) = self.frame_effects.first_entry() {
            if *e.key() >= upto {
                break;
            }
            let (commits, forks) = e.remove();
            self.commits_due.extend(commits);
            for p in forks {
                let k = p.key();
                if !self.fork_proofs.contains_key(&k) {
                    self.ev(format!(
                        "P2-5 fork proof: world {} exported global id {:#x} twice from its entry {}: transfers {} and {}",
                        k.0, k.1, k.2, k.3, k.4
                    ));
                    self.fork_proofs.insert(k, p);
                }
            }
        }
    }

    /// What this world knows of global id `g`. An object born here that has no entry yet is here,
    /// with an empty chain.
    pub fn seen_of(&self, g: u64) -> Option<Seen> {
        self.t.seen.get(&g).cloned().or_else(|| {
            (birth_world(g) == self.world_id && (g & 0xFFFF_FFFF) < self.genesis_objects)
                .then(|| Seen::Here { via: 0, from: self.world_id, from_via: 0, chain: Vec::new(), chash: chain_hash(&[]), settled: true, entry: None })
        })
    }

    /// The provenance chain of an object here: what its next departure's `Arrive` carries.
    /// The cached hash of [`Self::chain_of`] (what a departure certificate covers).
    pub fn chain_hash_of(&self, g: u64) -> u64 {
        match self.t.seen.get(&g) {
            Some(Seen::Here { chash, .. }) => *chash,
            _ => chain_hash(&self.chain_of(g)),
        }
    }

    pub fn chain_of(&self, g: u64) -> Vec<Hop> {
        match self.seen_of(g) {
            Some(Seen::Here { chain, .. }) => chain,
            _ => Vec::new(),
        }
    }

    /// The hop a departure of local object `obj` must carry: the certified commit that brought it
    /// here, from a `Provenance` entry in this world's log, if this world does not hold it yet.
    pub fn pending_hop(&self, obj: u32) -> Option<Hop> {
        let g = self.live(obj)?.global;
        let here = self.seen_of(g)?;
        if !matches!(here, Seen::Here { settled: false, .. }) {
            return None;
        }
        // Anyone may publish a Provenance entry: take one that proves the hop.
        self.entries.iter().find_map(|(id, e)| match &e.payload {
            Payload::Provenance { hop } if !self.is_void(*id) && self.hop_in_ok(g, &here, hop) => Some(hop.clone()),
            _ => None,
        })
    }

    /// `hop` is the certified commit of the transfer that brought object `g` here.
    fn hop_in_ok(&self, g: u64, here: &Seen, hop: &Hop) -> bool {
        let Seen::Here { via, from, from_via, .. } = here else { return false };
        hop.src == *from
            && hop.dest == self.world_id
            && hop.transfer == *via
            && hop.via == *from_via
            && self.epochs.hop_ok(hop, fact_committed(hop.src, hop.transfer, g, hop.dest, hop.via))
    }

    // ---- simulation ------------------------------------------------------------------------------

    /// Sorted (entry id, entry) for frame `f`: soonest-wins order with the per-frame tie rank.
    fn frame_entries(&self, f: u64) -> Vec<(EntryId, Entry)> {
        let mut v: Vec<(EntryId, Entry)> = self
            .at_frame
            .get(&f)
            .map(|s| s.iter().filter(|id| !self.is_void(**id)).map(|id| (*id, self.entries[id].clone())).collect())
            .unwrap_or_default();
        let seed = self.cfg.seed;
        v.sort_by_key(|(_, e)| (tie_rank(seed, f, e.member), e.member, e.seq));
        v
    }

    fn live(&self, obj: u32) -> Option<Obj> {
        self.t.objs.get(obj as usize).copied().flatten()
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        id: EntryId,
        e: &Entry,
        f: u64,
        hands_set: &mut BTreeSet<u32>,
        carry: &mut Vec<(u32, BodyState)>,
        record_effects: bool,
    ) -> Outcome {
        match &e.payload {
            Payload::Touch { obj, impulse, grab } => {
                let Some(o) = self.live(*obj) else { return Outcome::RejectedGone };
                if e.member != LOGIC {
                    if let Some(c) = self.t.claims.get(obj) {
                        if c.member != e.member && c.until > f {
                            return Outcome::RejectedClaimed { by: c.member, until: c.until };
                        }
                    }
                    if let Some(&(m, u)) = self.t.cooldown.get(obj) {
                        if m == e.member && u > f && !self.t.claims.contains_key(obj) {
                            return Outcome::RejectedCooldown { until: u };
                        }
                    }
                }
                let imp = Vec3 { x: impulse[0], y: impulse[1], z: impulse[2] };
                body_apply_linear_impulse_to_center(&mut self.world, o.body, imp, true);
                if e.member != LOGIC {
                    match self.t.claims.get(obj).copied() {
                        Some(c) if c.member == e.member && c.until > f => {
                            if *grab {
                                self.t.claims.insert(*obj, Claim { member: e.member, until: f + CLAIM_FRAMES });
                            }
                        }
                        _ => {
                            self.t.claims.insert(*obj, Claim { member: e.member, until: f + CLAIM_FRAMES });
                        }
                    }
                }
                if let Some((cobj, st)) = e.carried {
                    if self.eff.get(&id) == Some(&e.stamp) {
                        carry.push((cobj, st));
                    }
                }
                Outcome::Accepted
            }
            Payload::Hand { obj, target_mm, active, span } => {
                // A member moves only its own hands; the author is the signed chain's member.
                if self.hand_owners.get(obj) != Some(&e.member) {
                    return Outcome::RejectedNotOwner;
                }
                let Some(o) = self.live(*obj) else { return Outcome::RejectedGone };
                let p = if *active {
                    Pos { x: target_mm[0] as f32 * 0.001, y: target_mm[1] as f32 * 0.001, z: target_mm[2] as f32 * 0.001 }
                } else {
                    Pos { x: 0.0, y: 1.5, z: 0.0 }
                };
                let dt = self.cfg.dt * (*span).max(1) as f32;
                body_set_target_transform(&mut self.world, o.body, Transform { p, q: QUAT_IDENTITY }, dt, true);
                hands_set.insert(*obj);
                if *active {
                    self.t.hand_idle.insert(*obj, 0);
                } else {
                    self.t.hand_idle.remove(obj);
                }
                Outcome::Accepted
            }
            Payload::Depart { obj, dest, deadline, hop } => {
                let transfer = &transfer_id(self.world_id, id);
                if self.t.escrow.contains_key(transfer) {
                    return Outcome::RejectedDuplicate;
                }
                let gone = Escrow {
                    obj: *obj,
                    record: None,
                    status: EscrowStatus::DepartRejected,
                    depart_frame: f,
                    depart: id,
                    dest: *dest,
                    deadline: *deadline,
                };
                // Reviews R3-4, R3-6: objects leave only for worlds on the agreed route list, so no
                // member can send one to a world it controls alone, or to a world nobody knows.
                if !self.peers_of.contains_key(dest) {
                    self.t.escrow.insert(*transfer, gone);
                    return Outcome::RejectedNoRoute;
                }
                let Some(o) = self.live(*obj) else {
                    self.t.escrow.insert(*transfer, gone);
                    return Outcome::RejectedGone;
                };
                if let Some(c) = self.t.claims.get(obj) {
                    if c.member != e.member && c.until > f {
                        self.t.escrow.insert(*transfer, gone);
                        return Outcome::RejectedClaimed { by: c.member, until: c.until };
                    }
                }
                // P2-5: an object that arrived here leaves again only with the certified hop in, so
                // the next destination can check its whole way from birth.
                let mut via = 0;
                if self.provenance {
                    let here = self.seen_of(o.global);
                    match here.clone() {
                        Some(Seen::Here { settled: true, via: v, .. }) => via = v,
                        Some(Seen::Here { settled: false, via: v, from, from_via, mut chain, entry, .. }) => {
                            let Some(h) = hop.as_ref().filter(|h| self.hop_in_ok(o.global, here.as_ref().unwrap(), h)) else {
                                self.t.escrow.insert(*transfer, gone);
                                return Outcome::RejectedUnsettled;
                            };
                            chain.push(h.clone());
                            via = v;
                            let chash = chain_hash(&chain);
                            self.t.seen.insert(o.global, Seen::Here { via, from, from_via, chain, chash, settled: true, entry });
                        }
                        _ => {
                            self.t.escrow.insert(*transfer, gone);
                            return Outcome::RejectedUnsettled;
                        }
                    }
                }
                let record =
                    ObjRecord { global: o.global, kind: o.kind, state: BodyState::read(&self.world, o.body), src_world: self.world_id, src_frame: f, via };
                destroy_body(&mut self.world, o.body);
                self.t.objs[*obj as usize] = None;
                self.t.claims.remove(obj);
                self.t.escrow.insert(*transfer, Escrow { record: Some(record), status: EscrowStatus::Pending, ..gone });
                Outcome::Accepted
            }
            Payload::Arrive { transfer, record, depart, deadline, cert, chain } => {
                let src = record.src_world;
                // Reviews R3-4, R3-5: only a world on the agreed route list can vouch for an object.
                let Some(members) = self.peers_of.get(&src).cloned() else { return Outcome::RejectedNoRoute };
                let chash = chain_hash(chain);
                let fact = fact_departed(src, *depart, record, self.world_id, *deadline, chash);
                if *transfer != transfer_id(src, *depart) || !cert_ok(&members, src, fact, cert) {
                    return Outcome::RejectedNoProof;
                }
                // Review NEW-21: even a certified record is untrusted input (a bug or a compromised
                // majority in the source world). Refuse non-finite or absurd states.
                let st = record.state;
                let finite = st.0.iter().all(|&b| (b >> 23) & 0xFF != 0xFF);
                let (q, v, w) = (st.rot(), st.lin(), st.ang());
                let qn = q.v.x * q.v.x + q.v.y * q.v.y + q.v.z * q.v.z + q.s * q.s;
                let sane = finite
                    && (0.99..=1.01).contains(&qn)
                    && v.x * v.x + v.y * v.y + v.z * v.z <= MAX_ARRIVAL_SPEED * MAX_ARRIVAL_SPEED
                    && w.x * w.x + w.y * w.y + w.z * w.z <= MAX_ARRIVAL_SPIN * MAX_ARRIVAL_SPIN;
                if !sane {
                    if !self.t.arrivals.contains_key(transfer) {
                        self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    }
                    return Outcome::RejectedInvalid;
                }
                if self.t.arrivals.contains_key(transfer) {
                    return Outcome::RejectedDuplicate;
                }
                // P2-5: the source's majority vouches for the departure, not for how the object got
                // there. Check the chain back to its birth world against each hop's epoch
                // certificates, and that it passes through this world's own last exit of the
                // object. A refusal is certified like any other, and returns the object to the
                // source (whose majority signed this chain).
                if self.provenance {
                    if check_chain(&self.epochs, record.global, src, record.via, chain).is_err() {
                        self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                        return Outcome::RejectedNoProvenance;
                    }
                    // P25-1: identity is proven; content and birth are the policy's call.
                    if !self.import_policy.allows(record.global, src, record.kind, chain) {
                        self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                        return Outcome::RejectedPolicy;
                    }
                    let known = self.seen_of(record.global);
                    let arrival = arrival_export(src, *depart, record, self.world_id, *deadline, chash, cert);
                    if let Some(p) = known.as_ref().and_then(|k| fork_proof(k, record.global, chain, &arrival)) {
                        if record_effects {
                            self.frame_effects.entry(f).or_default().1.push(p);
                        }
                    }
                    match known {
                        Some(Seen::Here { .. }) => {
                            self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                            return Outcome::RejectedLiveId;
                        }
                        Some(Seen::Left { transfer: out, .. }) if !chain.iter().any(|h| h.transfer == out && h.src == self.world_id) => {
                            self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                            return Outcome::RejectedFork;
                        }
                        _ => {}
                    }
                }
                // Review R3-6: past the deadline the arrival is refused, and that certified refusal
                // returns the object to the source. Nobody can hold it in limbo by never relaying:
                // anyone may relay late, and late means refused.
                if f > *deadline {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedLate;
                }
                // A deadline far ahead would keep the transfer open, and its id in this table, for
                // as long as it likes (round 3, low).
                if *deadline > f.saturating_add(MAX_DEADLINE_FRAMES) {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedDeadline;
                }
                if !self.t.portal_open {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedPortalClosed;
                }
                // Review R3-5: never a second live copy of one global id in a world.
                if self.t.objs.iter().flatten().any(|o| o.global == record.global) {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedLiveId;
                }
                let obj = self.spawn(record, Pos { x: 0.0, y: 2.0, z: 0.0 });
                self.t.arrivals.insert(*transfer, Arrival::Accepted { obj, frame: f });
                let entry = Some(Box::new(arrival_export(src, *depart, record, self.world_id, *deadline, chash, cert)));
                let here = Seen::Here { via: *transfer, from: src, from_via: record.via, chain: chain.clone(), chash, settled: false, entry };
                self.t.seen.insert(record.global, here);
                self.watched.insert(obj);
                Outcome::Accepted
            }
            Payload::Verdict { transfer, accepted, cert } => {
                let Some(esc) = self.t.escrow.get(transfer).copied() else { return Outcome::RejectedGone };
                if esc.status != EscrowStatus::Pending {
                    return Outcome::RejectedDuplicate;
                }
                let members = self.peers_of.get(&esc.dest).cloned().unwrap_or_default();
                if !cert_ok(&members, esc.dest, fact_verdict(esc.dest, *transfer, *accepted), cert) {
                    return Outcome::RejectedNoProof;
                }
                if *accepted {
                    let rec = esc.record.expect("pending escrow holds its record");
                    let (from, from_via, entry) = match self.seen_of(rec.global) {
                        Some(Seen::Here { from, from_via, entry, .. }) => (from, from_via, entry),
                        _ => (self.world_id, 0, None),
                    };
                    self.t.seen.insert(rec.global, Seen::Left { transfer: *transfer, dest: esc.dest, via: rec.via, frame: f, from, from_via, entry });
                    if record_effects {
                        self.frame_effects.entry(f).or_default().0.push(fact_committed(self.world_id, *transfer, rec.global, esc.dest, rec.via));
                    }
                    self.t.escrow.insert(*transfer, Escrow { status: EscrowStatus::Committed, record: None, ..esc });
                } else {
                    let rec = esc.record.expect("pending escrow holds its record");
                    let back = self.spawn(&rec, rec.state.pos());
                    debug_assert_eq!(back as usize, self.t.objs.len() - 1);
                    self.t.escrow.insert(*transfer, Escrow { status: EscrowStatus::Returned, ..esc });
                }
                Outcome::Accepted
            }
            Payload::Portal { open } => {
                self.t.portal_open = *open;
                Outcome::Accepted
            }
            // Protocol entries never reach the sim (set_eff schedules only perturbations).
            Payload::Ack { .. }
            | Payload::Vote { .. }
            | Payload::Attest { .. }
            | Payload::Provenance { .. }
            | Payload::Propose { .. }
            | Payload::Prevote { .. }
            | Payload::Precommit { .. } => Outcome::Accepted,
        }
    }

    /// Recreate an object from a record at `at`, keeping its rotation and velocities.
    fn spawn(&mut self, r: &ObjRecord, at: Pos) -> u32 {
        let mut d = default_body_def();
        d.type_ = BodyType::Dynamic;
        d.position = at;
        d.rotation = r.state.rot();
        d.linear_velocity = r.state.lin();
        d.angular_velocity = r.state.ang();
        let b = create_body(&mut self.world, &d);
        add_shape(&mut self.world, b, r.kind);
        self.t.objs.push(Some(Obj { body: b, global: r.global, kind: r.kind }));
        (self.t.objs.len() - 1) as u32
    }

    /// Step one frame: keyframe, entries in canonical order, hands, Box3D step, carried states.
    fn step_frame(&mut self, record: bool) {
        let f = self.frame;
        if record {
            // A re-simulation of this frame replaces its cross-world effects (P25-3, P25-4).
            self.frame_effects.remove(&f);
        }
        if record && self.wants_keyframe(f) {
            let k = self.save();
            self.ring.insert(f, k);
            self.stats.max_ring_bytes = self.stats.max_ring_bytes.max(self.ring_bytes());
        }
        // Round 3 (low): forget transfers that can no longer change. A rejected arrival goes once
        // its deadline has passed (any replay is then late, so rejected again, consistently) and
        // MAX_DEADLINE_FRAMES after it arrived (by then it is confirmed and attested: the sim never
        // runs more than the speculation limit past the confirmed frame). A resolved escrow goes
        // MAX_DEADLINE_FRAMES after its departure: a replayed verdict then finds nothing and does
        // nothing. Accepted arrivals stay: forgetting one would let a late replay of its Arrive be
        // certified "rejected" and refund an object that did arrive.
        self.t.arrivals
            .retain(|_, a| !matches!(a, Arrival::Rejected { frame, until } if *until < f && frame.saturating_add(MAX_DEADLINE_FRAMES) < f));
        self.t.escrow.retain(|_, e| e.status == EscrowStatus::Pending || e.depart_frame.saturating_add(MAX_DEADLINE_FRAMES) >= f);
        let mut hands_set = BTreeSet::new();
        let mut carry = Vec::new();
        for (id, e) in self.frame_entries(f) {
            let o = self.apply(id, &e, f, &mut hands_set, &mut carry, record);
            if record {
                self.outcomes.insert(id, (f, o));
            }
        }
        // Hands without a target this frame hold still, or, with `hand_predict`, an active hand
        // keeps its last commanded velocity for up to that many frames (dead reckoning). Both are
        // deterministic rules of the sim: a speculative frame that lacks the author's entry
        // predicts with them, and the entry's arrival replaces the prediction by rollback.
        let hands: Vec<(u32, Obj)> =
            self.t.objs.iter().enumerate().filter_map(|(i, o)| o.filter(|o| o.kind == 4).map(|o| (i as u32, o))).collect();
        for (i, o) in hands {
            if hands_set.contains(&i) {
                continue;
            }
            let coast = match self.t.hand_idle.get_mut(&i) {
                Some(n) if self.cfg.hand_predict > 0 => {
                    *n += 1;
                    *n <= self.cfg.hand_predict
                }
                _ => false,
            };
            if !coast {
                body_set_linear_velocity(&mut self.world, o.body, Vec3 { x: 0.0, y: 0.0, z: 0.0 });
            } else {
                // Never coast faster than a hand moves: a jump (a teleport-like target) would
                // otherwise be extrapolated as a 40 m/s flight.
                let v = body_get_linear_velocity(&self.world, o.body);
                let sp = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
                if sp > HAND_COAST_VMAX {
                    let k = HAND_COAST_VMAX / sp;
                    body_set_linear_velocity(&mut self.world, o.body, Vec3 { x: v.x * k, y: v.y * k, z: v.z * k });
                }
            }
        }
        let t0 = now_us();
        self.world.step(self.cfg.dt, SUBSTEPS);
        if record {
            self.stats.step_us += now_us() - t0;
            self.stats.steps += 1;
        }
        // User ruling 09:30Z: the touched object takes the carried resulting state. A no-op when
        // this peer is on the author's world line; otherwise it pulls the object onto it.
        for (obj, st) in carry {
            if let Some(o) = self.live(obj) {
                if BodyState::read(&self.world, o.body) != st {
                    st.write(&mut self.world, o.body);
                    if record {
                        self.stats.overwrites += 1;
                    }
                }
            }
        }
        // Expire claims; their holders enter a short cooldown on that object.
        let next = f + 1;
        let expired: Vec<(u32, MemberId)> = self.t.claims.iter().filter(|(_, c)| c.until <= next).map(|(o, c)| (*o, c.member)).collect();
        for (o, m) in expired {
            self.t.claims.remove(&o);
            self.t.cooldown.insert(o, (m, next + CLAIM_COOLDOWN));
        }
        self.t.cooldown.retain(|_, (_, u)| *u > next);
        self.frame = next;
        if record {
            self.hashes.insert(f, self.state_hash());
            for &obj in &self.watched.clone() {
                if let Some(s) = self.obj_state(obj) {
                    self.obj_hist.insert((f, obj), s.hash());
                }
            }
            if f % self.cfg.check_every == 0 {
                let h = self.image_hash();
                self.check_hashes.insert(f, h);
                // Review L2: a release-build finite check at every check frame. NaN payload bits differ
                // across ISAs, so a non-finite state is a divergence waiting to happen: report it.
                let bad = self.t.objs.iter().flatten().find(|o| {
                    let st = BodyState::read(&self.world, o.body);
                    st.0.iter().any(|&b| (b >> 23) & 0xFF == 0xFF)
                });
                if let Some(o) = bad {
                    self.stats.nonfinite += 1;
                    let g = o.global;
                    self.ev(format!("NON-FINITE body state at frame {f} (object global id {g:#x})"));
                }
            }
            if self.track {
                let p: Vec<[f32; 3]> = self
                    .t
                    .objs
                    .iter()
                    .map(|o| o.map_or([f32::NAN; 3], |o| {
                        let x = body_get_transform(&self.world, o.body).p;
                        [x.x as f32, x.y as f32, x.z as f32]
                    }))
                    .collect();
                self.pos_hist.insert(f, p);
            }
        }
    }

    /// Roll back if needed, then simulate forward to `target` (exclusive) unless the speculation
    /// limit is reached. Returns the number of new frames stepped.
    pub fn advance(&mut self, target: u64) -> u64 {
        let t_adv = now_us();
        let n = self.advance_inner(target);
        let us = now_us() - t_adv;
        self.stats.advance_us += us;
        self.stats.max_advance_us = self.stats.max_advance_us.max(us);
        n
    }

    fn advance_inner(&mut self, target: u64) -> u64 {
        if let Some(from) = self.dirty_from.take() {
            let now = self.frame;
            self.resim_to = now;
            let (&kf, _) = self.ring.range(..=from).next_back().expect("keyframe at or before rollback frame");
            let k = self.ring[&kf].clone();
            for f in self.ring.range(kf + 1..).map(|(f, _)| *f).collect::<Vec<_>>() {
                self.ring.remove(&f);
            }
            self.restore(&k, kf);
            self.stats.rollbacks += 1;
            self.stats.max_depth = self.stats.max_depth.max(now - from);
            let t0 = now_us();
            while self.frame < now {
                self.step_frame(true);
                self.stats.resim_frames += 1;
            }
            self.stats.resim_us += now_us() - t0;
            self.evaluate_checks();
        }
        self.trim_ring();
        let mut n = 0;
        while self.frame < target {
            if self.frame >= self.conf + self.cfg.window {
                self.stats.paused_frames += 1;
                break;
            }
            self.step_frame(true);
            n += 1;
        }
        n
    }

    /// Drop keyframes this member can never need again: everything before the newest keyframe at
    /// or before min(confirmed frame, simulated frame), once no re-simulation is pending.
    fn trim_ring(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let fin = self.conf.min(self.frame);
        let base = match self.ring.range(..=fin).next_back() {
            Some((&kf, _)) => kf,
            None => return,
        };
        let now = self.frame;
        let drop: Vec<u64> = self
            .ring
            .keys()
            .copied()
            .filter(|&f| {
                f < base
                    || (f > base
                        && match self.cfg.snap {
                            SnapPolicy::Every(k) => f % k != 0,
                            SnapPolicy::Log(k) => f % log_grid(k, now - f) != 0,
                            // Keep only the first candidate after the confirmed frame.
                            SnapPolicy::Base(_) => self.ring.range(base + 1..f).next().is_some(),
                            SnapPolicy::Window(k) => f % k != 0 || now - f >= 4 * k,
                        })
            })
            .collect();
        for f in drop {
            self.ring.remove(&f);
        }
    }

    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// Bytes of rollback snapshot data held: every keyframe's world image plus its object table
    /// (ballpit-viz; `ring_bytes` counts the world images only).
    pub fn ring_bytes_with_objs(&self) -> usize {
        self.ring.values().map(|k| k.image.len() + k.t.objs.len() * std::mem::size_of::<Option<Obj>>()).sum()
    }

    pub fn caught_up(&self) -> bool {
        self.dirty_from.is_none()
    }

    /// Entries this member holds whose effective frame is confirmed (R14 relays wait for this).
    pub fn is_confirmed(&self, id: EntryId) -> bool {
        self.eff.get(&id).is_some_and(|&f| f < self.conf)
    }

    pub fn all_entries(&self) -> Vec<Entry> {
        self.entries.values().cloned().collect()
    }

    pub fn void_ids(&self) -> BTreeSet<EntryId> {
        self.violations.union(&self.disputed).copied().collect()
    }

    pub fn members_evicted(&self) -> &BTreeMap<MemberId, (u32, u64)> {
        &self.evicted
    }
}

// ---- offline reference ---------------------------------------------------------------------------

/// Re-simulate a world from scratch with a complete, final log and effective frames: the
/// single-world reference used to check that sharing a process with other worlds, rollbacks and
/// throttling changed nothing.
pub fn replay(world_id: u32, scene: SceneKind, cfg: SimConfig, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> BTreeMap<u64, u64> {
    replay_sim(world_id, scene, cfg, members, log, frames).hashes
}

pub fn replay_sim(world_id: u32, scene: SceneKind, cfg: SimConfig, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> WorldSim {
    replay_sim_with(world_id, scene, cfg, members, &BTreeMap::new(), log, frames)
}

/// As [`replay_sim`], for a world that trades objects with others (R14 certificates).
pub fn replay_sim_with(
    world_id: u32,
    scene: SceneKind,
    cfg: SimConfig,
    members: &[MemberId],
    peers_of: &BTreeMap<u32, Vec<MemberId>>,
    log: &[(Entry, u64)],
    frames: u64,
) -> WorldSim {
    let mut s = WorldSim::new(world_id, u32::MAX, members, scene, cfg);
    s.peers_of = peers_of.clone();
    s.run_log(log, frames)
}

/// As [`replay_sim_with`], with `like`'s cross-world config: route list, epoch certificates and
/// whether provenance is checked (P2-5).
pub fn replay_sim_like(like: &WorldSim, scene: SceneKind, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> WorldSim {
    let mut s = WorldSim::new(like.world_id, u32::MAX, members, scene, like.cfg);
    s.peers_of = like.peers_of.clone();
    s.epochs = like.epochs.clone();
    s.provenance = like.provenance;
    s.import_policy = like.import_policy.clone();
    s.hand_owners = like.hand_owners.clone();
    s.run_log(log, frames)
}

impl WorldSim {
    /// Offline reference: load a final log with its effective frames and simulate `frames`.
    fn run_log(mut self, log: &[(Entry, u64)], frames: u64) -> WorldSim {
        for (e, eff) in log {
            let id = e.id();
            self.by_author.insert((e.member, e.seq), id);
            self.entries.insert(id, e.clone());
            self.eff.insert(id, *eff);
            self.at_frame.entry(*eff).or_default().insert(id);
        }
        self.conf = u64::MAX / 2;
        while self.frame < frames {
            self.step_frame(true);
        }
        self
    }
}

/// The log this member would hand to [`replay`]: every non-void entry with its effective frame.
pub fn final_log(s: &WorldSim) -> Vec<(Entry, u64)> {
    s.eff.iter().map(|(id, f)| (s.entries[id].clone(), *f)).collect()
}

pub fn digest(h: &BTreeMap<u64, u64>, frames: u64) -> u64 {
    fnv_u64s((0..frames).map(|f| h.get(&f).copied().unwrap_or(0)))
}
