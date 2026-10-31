//! Entries, messages and their byte encoding; the same-frame tie rank.

use super::*;

pub type MemberId = u32;
/// The check hash members exchange at check frames (design 9.1, review P22R-1): BLAKE3 over the
/// whole canonical state, so a donor cannot forge a state that matches it (XXH64 and FNV can be
/// steered). The per-frame `state_hash` stays a 64-bit local diagnostic.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CheckHash(pub [u8; 32]);

impl std::fmt::Debug for CheckHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in &self.0[..8] {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// An entry's identity: the SHA-256 of its canonical encoding, all 256 bits (S3b). Signatures cover
/// it, so a shorter id would let an author grind two entries with one id. All zeros means "none"
/// (the `prev` of a member's first entry).
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId(pub [u8; 32]);

impl EntryId {
    pub const NONE: EntryId = EntryId([0; 32]);

    /// The first 8 bytes, little-endian: for hash inputs and log labels only, never as an identity.
    pub fn short(&self) -> u64 {
        self.words()[0]
    }

    /// All 256 bits as four little-endian words, for 64-bit hash inputs.
    pub fn words(&self) -> [u64; 4] {
        std::array::from_fn(|i| u64::from_le_bytes(self.0[8 * i..8 * i + 8].try_into().unwrap()))
    }
}

impl std::fmt::Debug for EntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.short().swap_bytes())
    }
}
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
    pub(super) fn f(&self, i: usize) -> f32 {
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
    Arrive { transfer: u64, record: ObjRecord, depart: EntryId, deadline: u64, cert: Vec<(MemberId, Sig)>, chain: Vec<Hop> },
    /// R14, source world: the destination's confirmed verdict, with a strict majority of the
    /// destination's members' signatures over it (review NEW-3). Anyone may relay it.
    Verdict { transfer: u64, accepted: bool, cert: Vec<(MemberId, Sig)> },
    /// A member's signature over a cross-world fact its replica has confirmed (R14), published as
    /// a chain entry so it is equivocation-proof and fetchable.
    Attest { fact: u64, sig: Sig },
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
    Ack { horizon: u64, heads: Vec<(MemberId, u32, EntryId)>, check: Option<(u64, CheckHash)>, epoch: u32 },
    /// Vote, in membership epoch `epoch`, to remove `target`: it signed two versions of its entry
    /// `seq` (or another proof), or (`seq` = 0) it has gone silent or cannot keep up (reviews
    /// R3-2, R3-9). `frozen` is the head of target's chain this voter holds and will ever
    /// reference in this epoch (review NEW-2). A vote stands until the epoch ends (P2-4).
    /// `kick` names the kick this vote answers (0: the voter's own suspicion or proof). A kick vote
    /// only freezes the chain for the cut, may come from the target itself, and counts toward no
    /// removal but that kick's (D80).
    Vote { epoch: u32, target: MemberId, seq: u32, frozen: (u32, EntryId), kick: EntryId },
    /// View change (P2-4, design 10.8): the round's proposer proposes `change` for epoch `epoch`;
    /// `valid_round` is the round in which it saw a quorum prevote this value (`NO_ROUND` if none).
    Propose { epoch: u32, round: u32, valid_round: u32, change: Change },
    /// View change: this member's first-phase vote for a proposal's value in a round (0 = nil).
    Prevote { epoch: u32, round: u32, value: u64 },
    /// View change: this member's second-phase vote; a quorum of them decides the value. A value
    /// that adds members states their first frame, `boundary` (0 if none): from this entry on, the
    /// author's acks count toward no reader's confirmation at or past it until a later epoch (10.8).
    /// `sig` is the author's signature over [`vc_fact`] (S3c): the epoch certificate collects these,
    /// so another world can check it without the entries.
    Precommit { epoch: u32, round: u32, value: u64, boundary: u64, sig: Sig },
    /// D80 (user 22:13Z): a kick of `target` by a member holding the KICK permission, for the view
    /// change that ends `epoch`. It authorises the removal alone: members vote only to freeze the
    /// target's chain for the cut, never to approve it. The view change commits it in order with
    /// every other membership change. Like every admin entry it stays pending across epochs until a
    /// decision applies or refuses it; `epoch` is the first epoch whose change may include it.
    Kick { epoch: u32, target: MemberId },
    /// Give `member` the role `role`, for the view change that ends `epoch` (ROLES permission).
    /// Refused if it would leave the session without an admin.
    SetRole { epoch: u32, member: MemberId, role: RoleId },
    /// Define custom role `role` as the permission set `perms` (ROLES permission). Admin (role 0) is
    /// built in and cannot be redefined.
    DefineRole { epoch: u32, role: RoleId, perms: u64 },
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
    /// SHA-256 of the canonical encoding (S3b; the spike used 64-bit FNV).
    pub fn id(&self) -> EntryId {
        use sha2::Digest;
        let mut w = Wr(Vec::with_capacity(128));
        w.entry(self);
        EntryId(sha2::Sha256::digest(&w.0).into())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// Any chain entry: a perturbation, an ack or a vote, with its author's signature (S3).
    Entry(SignedEntry),
    /// Ask for `author`'s entries `from..=to` (gap or conflict detection).
    Fetch { author: MemberId, from: u32, to: u32 },
    /// Everything one member sends one peer in one tick, in one datagram (R18, viz-4).
    Batch(Vec<Msg>),
    /// View change: ask for the proposal entry carrying `value` (a member that saw a quorum of
    /// precommits for a value it holds no proposal for; the proposer may have equivocated).
    FetchValue { value: u64 },
    /// A non-member asks to join the world (P2-4). Production: signed by the joiner's key, after an
    /// invite or the world's ACL admits it (12). `attests`: the relationships it shows for the
    /// world's join rules (D80).
    Join { member: MemberId, attests: Vec<Attest> },
    /// Review VCR-8: ask for the entry with this id (an admin entry a decided change lists).
    FetchEntry { id: EntryId },
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
    pub(super) fn u8(&mut self, v: u8) { self.0.push(v) }
    pub(super) fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    pub(super) fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()) }
    pub(super) fn id(&mut self, v: EntryId) { self.0.extend_from_slice(&v.0) }
    pub(super) fn i32(&mut self, v: i32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    pub(super) fn state(&mut self, s: &BodyState) { for b in s.0 { self.u32(b) } }
    pub(super) fn sig(&mut self, s: &Sig) { self.0.extend_from_slice(s) }
    pub(super) fn cert(&mut self, c: &[(MemberId, Sig)]) { self.u32(c.len() as u32); for (m, s) in c { self.u32(*m); self.sig(s) } }
    pub(super) fn hop(&mut self, h: &Hop) {
        self.u32(h.src); self.u32(h.dest); self.u64(h.transfer); self.u64(h.via); self.u32(h.epoch); self.u64(h.frame);
        self.u32(h.certs.len() as u32); for c in &h.certs { self.epoch_cert(c) }
        self.cert(&h.cert)
    }
    pub(super) fn epoch_cert(&mut self, c: &EpochCert) {
        self.u32(c.world); self.u32(c.epoch); self.change(&c.change); self.u32(c.round);
        self.u32(c.members.len() as u32); for m in &c.members { self.u32(*m) }
        self.cert(&c.precommits);
        self.u32(c.proofs.len() as u32); for e in &c.proofs { self.entry(e) }
    }
    pub(super) fn chain(&mut self, c: &[Hop]) { self.u32(c.len() as u32); for h in c { self.hop(h) } }
    pub(super) fn attests(&mut self, a: &[Attest]) {
        assert!(a.len() <= MAX_ATTESTS, "{} attestations: at most {MAX_ATTESTS} travel", a.len());
        self.u32(a.len() as u32);
        for x in a { self.u8(x.rel.tag()); self.u32(x.with); self.sig(&x.sig_with); self.sig(&x.sig_joiner) }
    }
    pub(super) fn entry(&mut self, e: &Entry) {
        self.u32(e.member);
        self.u32(e.seq);
        self.id(e.prev);
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
                self.id(*depart); self.u64(*deadline); self.cert(cert); self.chain(chain);
            }
            Payload::Provenance { hop } => { self.u8(16); self.hop(hop) }
            Payload::Verdict { transfer, accepted, cert } => {
                self.u8(5); self.u64(*transfer); self.u8(*accepted as u8); self.cert(cert);
            }
            Payload::Attest { fact, sig } => { self.u8(9); self.u64(*fact); self.sig(sig) }
            Payload::Portal { open } => { self.u8(6); self.u8(*open as u8) }
            Payload::Ack { horizon, heads, check, epoch } => {
                // Epoch 0 keeps the phase-1 layout (tag 7), so runs without a view change keep
                // their entry ids; a later epoch is tag 14 with the epoch appended.
                self.u8(if *epoch == 0 { 7 } else { 14 }); self.u64(*horizon); self.u32(heads.len() as u32);
                for (q, s, id) in heads { self.u32(*q); self.u32(*s); self.id(*id) }
                match check { Some((f, h)) => { self.u8(1); self.u64(*f); self.0.extend_from_slice(&h.0) } None => self.u8(0) }
                if *epoch != 0 { self.u32(*epoch) }
            }
            Payload::Vote { epoch, target, seq, frozen, kick } => {
                // A vote that answers no kick keeps tag 8 (and so its id); a kick vote is tag 15.
                self.u8(if *kick == EntryId::NONE { 8 } else { 15 }); self.u32(*epoch); self.u32(*target); self.u32(*seq); self.u32(frozen.0); self.id(frozen.1);
                if *kick != EntryId::NONE { self.id(*kick) }
            }
            Payload::Kick { epoch, target } => { self.u8(17); self.u32(*epoch); self.u32(*target) }
            Payload::SetRole { epoch, member, role } => { self.u8(18); self.u32(*epoch); self.u32(*member); self.u32(*role as u32) }
            Payload::DefineRole { epoch, role, perms } => { self.u8(19); self.u32(*epoch); self.u32(*role as u32); self.u64(*perms) }
            Payload::Propose { epoch, round, valid_round, change } => {
                self.u8(11); self.u32(*epoch); self.u32(*round); self.u32(*valid_round); self.change(change)
            }
            Payload::Prevote { epoch, round, value } => { self.u8(12); self.u32(*epoch); self.u32(*round); self.u64(*value) }
            Payload::Precommit { epoch, round, value, boundary, sig } => {
                self.u8(13); self.u32(*epoch); self.u32(*round); self.u64(*value); self.u64(*boundary); self.sig(sig)
            }
        }
        match &e.carried {
            Some((obj, s)) => { self.u8(1); self.u32(*obj); self.state(s) }
            None => self.u8(0),
        }
    }
    pub(super) fn change(&mut self, c: &Change) {
        // A change with admin entries or kicks sets the epoch's top bit and carries them; any other
        // keeps the P2-4 layout, so its value is unchanged (D80).
        let ext = !c.admin.is_empty()
            || c.remove.iter().any(|r| r.kick != EntryId::NONE)
            || c.add_roles.iter().any(|&r| r != ADMIN)
            || c.add_attests.iter().any(|a| !a.is_empty());
        // Review VCR-11: the top bit marks the layout, so an epoch must stay below it.
        assert!(c.epoch < CHANGE_EXT, "epoch {} does not fit a change's encoding", c.epoch);
        self.u32(if ext { c.epoch | CHANGE_EXT } else { c.epoch });
        self.u32(c.remove.len() as u32);
        for r in &c.remove {
            self.u32(r.member);
            self.u32(r.votes.len() as u32);
            for v in &r.votes { self.id(*v) }
            self.u32(r.cut.0);
            self.id(r.cut.1);
            if ext { self.id(r.kick) }
        }
        self.u32(c.add.len() as u32);
        for m in &c.add { self.u32(*m) }
        self.u64(c.boundary);
        self.u32(c.excluded.len() as u32);
        for (m, p) in &c.excluded { self.u32(*m); self.id(p.a); self.id(p.b) }
        if ext {
            self.u32(c.admin.len() as u32);
            for (id, ok) in &c.admin { self.id(*id); self.u8(*ok as u8) }
            self.u32(c.add_roles.len() as u32);
            for r in &c.add_roles { self.u32(*r as u32) }
            self.u32(c.add_attests.len() as u32);
            for a in &c.add_attests { self.attests(a) }
        }
    }
    pub fn msg(&mut self, m: &Msg) {
        match m {
            Msg::Entry(e) => { self.u8(1); self.entry(e); self.0.extend_from_slice(&e.sig) }
            Msg::Fetch { author, from, to } => { self.u8(3); self.u32(*author); self.u32(*from); self.u32(*to) }
            Msg::Batch(v) => { self.u8(4); self.u32(v.len() as u32); for m in v { self.msg(m) } }
            Msg::FetchValue { value } => { self.u8(5); self.u64(*value) }
            Msg::Join { member, attests } => {
                self.u8(if attests.is_empty() { 6 } else { 7 }); self.u32(*member);
                if !attests.is_empty() {
                    self.attests(attests);
                }
            }
            Msg::FetchEntry { id } => { self.u8(8); self.id(*id) }
        }
    }
}

pub struct Rd<'a>(pub &'a [u8], pub usize);
impl Rd<'_> {
    pub(super) fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.0.get(self.1..self.1 + n)?;
        self.1 += n;
        Some(s)
    }
    pub(super) fn u8(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
    pub(super) fn u32(&mut self) -> Option<u32> { Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    pub(super) fn u64(&mut self) -> Option<u64> { Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
    pub(super) fn id(&mut self) -> Option<EntryId> { Some(EntryId(self.take(32)?.try_into().ok()?)) }
    pub(super) fn i32(&mut self) -> Option<i32> { Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    pub(super) fn f32(&mut self) -> Option<f32> { Some(f32::from_bits(self.u32()?)) }
    pub(super) fn sig(&mut self) -> Option<Sig> { self.take(64)?.try_into().ok() }
    pub(super) fn cert(&mut self) -> Option<Vec<(MemberId, Sig)>> {
        let n = self.u32()? as usize;
        let mut c = Vec::with_capacity(n.min(64));
        for _ in 0..n { c.push((self.u32()?, self.sig()?)) }
        Some(c)
    }
    pub(super) fn hop(&mut self) -> Option<Hop> {
        let (src, dest, transfer, via, epoch, frame) = (self.u32()?, self.u32()?, self.u64()?, self.u64()?, self.u32()?, self.u64()?);
        let n = self.u32()? as usize;
        let mut certs = Vec::with_capacity(n.min(64));
        for _ in 0..n { certs.push(self.epoch_cert()?) }
        Some(Hop { src, dest, transfer, via, epoch, frame, certs, cert: self.cert()? })
    }
    pub(super) fn epoch_cert(&mut self) -> Option<EpochCert> {
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
    pub(super) fn chain(&mut self) -> Option<Vec<Hop>> {
        let n = self.u32()? as usize;
        let mut c = Vec::with_capacity(n.min(64));
        for _ in 0..n { c.push(self.hop()?) }
        Some(c)
    }
    pub(super) fn attests(&mut self) -> Option<Vec<Attest>> {
        // Review VCR-11: a longer list is refused, never read in part.
        let n = self.u32()? as usize;
        if n > MAX_ATTESTS {
            return None;
        }
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let rel = match self.u8()? { 1 => Rel::Friend, 2 => Rel::Confidant, 3 => Rel::Invited, _ => return None };
            v.push(Attest { rel, with: self.u32()?, sig_with: self.sig()?, sig_joiner: self.sig()? });
        }
        Some(v)
    }
    /// Review VCR-11: a role id is 16 bits; a wider value is refused, never narrowed.
    pub(super) fn role(&mut self) -> Option<RoleId> {
        RoleId::try_from(self.u32()?).ok()
    }
    pub(super) fn state(&mut self) -> Option<BodyState> {
        let mut s = [0u32; 13];
        for b in &mut s { *b = self.u32()? }
        Some(BodyState(s))
    }
    pub(super) fn entry(&mut self) -> Option<Entry> {
        let member = self.u32()?;
        let seq = self.u32()?;
        let prev = self.id()?;
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
                Payload::Arrive { transfer, record, depart: self.id()?, deadline: self.u64()?, cert: self.cert()?, chain: self.chain()? }
            }
            16 => Payload::Provenance { hop: self.hop()? },
            5 => Payload::Verdict { transfer: self.u64()?, accepted: self.u8()? != 0, cert: self.cert()? },
            9 => Payload::Attest { fact: self.u64()?, sig: self.sig()? },
            6 => Payload::Portal { open: self.u8()? != 0 },
            tag @ (7 | 14) => {
                let horizon = self.u64()?;
                let n = self.u32()? as usize;
                let mut heads = Vec::with_capacity(n.min(256));
                for _ in 0..n { heads.push((self.u32()?, self.u32()?, self.id()?)) }
                let check = if self.u8()? == 1 { Some((self.u64()?, CheckHash(self.take(32)?.try_into().ok()?))) } else { None };
                let epoch = if tag == 14 { self.u32()? } else { 0 };
                Payload::Ack { horizon, heads, check, epoch }
            }
            tag @ (8 | 15) => {
                let (epoch, target, seq, frozen) = (self.u32()?, self.u32()?, self.u32()?, (self.u32()?, self.id()?));
                let kick = if tag == 15 { self.id()? } else { EntryId::NONE };
                Payload::Vote { epoch, target, seq, frozen, kick }
            }
            17 => Payload::Kick { epoch: self.u32()?, target: self.u32()? },
            18 => Payload::SetRole { epoch: self.u32()?, member: self.u32()?, role: self.role()? },
            19 => Payload::DefineRole { epoch: self.u32()?, role: self.role()?, perms: self.u64()? },
            11 => Payload::Propose { epoch: self.u32()?, round: self.u32()?, valid_round: self.u32()?, change: self.change()? },
            12 => Payload::Prevote { epoch: self.u32()?, round: self.u32()?, value: self.u64()? },
            13 => Payload::Precommit { epoch: self.u32()?, round: self.u32()?, value: self.u64()?, boundary: self.u64()?, sig: self.sig()? },
            _ => return None,
        };
        let carried = match self.u8()? {
            1 => Some((self.u32()?, self.state()?)),
            _ => None,
        };
        Some(Entry { member, seq, prev, stamp, payload, carried })
    }
    pub(super) fn change(&mut self) -> Option<Change> {
        let raw = self.u32()?;
        let (ext, epoch) = (raw & CHANGE_EXT != 0, raw & !CHANGE_EXT);
        let n = self.u32()? as usize;
        let mut remove = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            let member = self.u32()?;
            let k = self.u32()? as usize;
            let mut votes = Vec::with_capacity(k.min(256));
            for _ in 0..k { votes.push(self.id()?) }
            let cut = (self.u32()?, self.id()?);
            let kick = if ext { self.id()? } else { EntryId::NONE };
            remove.push(Removal { member, votes, cut, kick });
        }
        let n = self.u32()? as usize;
        let mut add = Vec::with_capacity(n.min(256));
        for _ in 0..n { add.push(self.u32()?) }
        let boundary = self.u64()?;
        let n = self.u32()? as usize;
        let mut excluded = Vec::with_capacity(n.min(256));
        for _ in 0..n { excluded.push((self.u32()?, Proof { a: self.id()?, b: self.id()? })) }
        let mut admin = Vec::new();
        let mut add_roles = vec![ADMIN; add.len()];
        let mut add_attests = vec![Vec::new(); add.len()];
        if ext {
            let n = self.u32()? as usize;
            for _ in 0..n { admin.push((self.id()?, self.u8()? != 0)) }
            let n = self.u32()? as usize;
            add_roles = Vec::with_capacity(n.min(256));
            for _ in 0..n { add_roles.push(self.role()?) }
            let n = self.u32()? as usize;
            add_attests = Vec::with_capacity(n.min(256));
            for _ in 0..n { add_attests.push(self.attests()?) }
        }
        Some(Change { epoch, remove, add, boundary, excluded, admin, add_roles, add_attests })
    }
    pub fn msg(&mut self) -> Option<Msg> {
        Some(match self.u8()? {
            1 => Msg::Entry(SignedEntry { entry: self.entry()?, sig: self.take(64)?.try_into().ok()? }),
            3 => Msg::Fetch { author: self.u32()?, from: self.u32()?, to: self.u32()? },
            4 => {
                let n = self.u32()? as usize;
                let mut v = Vec::with_capacity(n.min(256));
                for _ in 0..n { v.push(self.msg()?) }
                Msg::Batch(v)
            }
            5 => Msg::FetchValue { value: self.u64()? },
            tag @ (6 | 7) => {
                let member = self.u32()?;
                let attests = if tag == 7 { self.attests()? } else { Vec::new() };
                Msg::Join { member, attests }
            }
            8 => Msg::FetchEntry { id: self.id()? },
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
            | Payload::Kick { .. }
            | Payload::SetRole { .. }
            | Payload::DefineRole { .. }
    )
}

/// Same-frame tie-break (user ruling 09:32Z): a per-frame pseudo-random permutation of members.
/// `MemberId`s are assigned in admission order by the membership log, never chosen by the peer,
/// and `seed` comes from the session/epoch genesis, so no key can be ground to win. All 32 bits of
/// the id are hashed (review L7).
pub fn tie_rank(seed: u64, frame: u64, member: MemberId) -> u64 {
    splitmix64(seed ^ frame.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (member as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
}
