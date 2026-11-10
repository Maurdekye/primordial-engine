//! State transfer (design 9.5, reviews P22R-1 and P22S-1): the canonical bytes of a world's state at a
//! check frame, which its check hash covers and a joiner restores from.
//!
//! The state is three parts: the physics transfer (`pe_physics::Transfer`), the sim store's snapshot
//! (`pe_sim::Snapshot`) and the world-level tables (here). Each reader consumes its bytes completely and
//! accepts only the canonical encoding, so the check hash binds exactly the state that is restored.

use super::*;

/// The check hash of a state given as its canonical parts (design 9.1): BLAKE3 over pe-physics's check
/// hash of its transfer, the frame, the sim store's snapshot bytes and the world-level tables' bytes. A
/// replica computes it over its own state; a joiner, over the bytes a donor sent.
pub fn check_hash_of(physics: &pe_physics::CheckHash, frame: u64, store: &[u8], tables: &[u8]) -> CheckHash {
    let mut h = blake3::Hasher::new();
    h.update(b"pe-check");
    h.update(&physics.0);
    h.update(&frame.to_le_bytes());
    h.update(&(store.len() as u64).to_le_bytes());
    h.update(store);
    h.update(&(tables.len() as u64).to_le_bytes());
    h.update(tables);
    CheckHash(*h.finalize().as_bytes())
}

// ---- the world-level tables ----------------------------------------------------------------------

impl Wr {
    fn record(&mut self, r: &ObjRecord) {
        self.u64(r.global);
        self.u8(r.kind);
        self.state(&r.state);
        self.u32(r.src_world);
        self.u64(r.src_frame);
        self.u64(r.via);
    }
    fn export(&mut self, x: &Option<Box<Export>>) {
        match x.as_deref() {
            None => self.u8(0),
            Some(Export::Committed(h)) => {
                self.u8(1);
                self.hop(h);
            }
            Some(Export::Departed { src, epoch, depart, record, dest, deadline, chain, cert }) => {
                self.u8(2);
                self.u32(*src);
                self.u32(*epoch);
                self.id(*depart);
                self.record(record);
                self.u32(*dest);
                self.u64(*deadline);
                self.u64(*chain);
                self.cert(cert);
            }
        }
    }
    fn tables(&mut self, t: &Tables) {
        self.u32(t.escrow.len() as u32);
        for (&tr, e) in &t.escrow {
            self.u64(tr);
            self.u32(e.obj);
            match &e.record {
                None => self.u8(0),
                Some(r) => {
                    self.u8(1);
                    self.record(r);
                }
            }
            self.u8(e.status as u8);
            self.u64(e.depart_frame);
            self.id(e.depart);
            self.u32(e.dest);
            self.u64(e.deadline);
        }
        self.u32(t.arrivals.len() as u32);
        for (&tr, a) in &t.arrivals {
            self.u64(tr);
            match *a {
                Arrival::Accepted { obj, frame } => {
                    self.u8(1);
                    self.u32(obj);
                    self.u64(frame);
                }
                Arrival::Rejected { frame, until } => {
                    self.u8(2);
                    self.u64(frame);
                    self.u64(until);
                }
            }
        }
        self.u32(t.seen.len() as u32);
        for (&g, s) in &t.seen {
            self.u64(g);
            match s {
                Seen::Here { via, from, from_via, chain, chash, settled, entry } => {
                    self.u8(1);
                    self.u64(*via);
                    self.u32(*from);
                    self.u64(*from_via);
                    self.chain(chain);
                    self.u64(*chash);
                    self.u8(*settled as u8);
                    self.export(entry);
                }
                Seen::Left { transfer, dest, via, frame, from, from_via, entry } => {
                    self.u8(2);
                    self.u64(*transfer);
                    self.u32(*dest);
                    self.u64(*via);
                    self.u64(*frame);
                    self.u32(*from);
                    self.u64(*from_via);
                    self.export(entry);
                }
            }
        }
        self.u8(t.portal_open as u8);
    }
}

impl Rd<'_> {
    fn record(&mut self) -> Option<ObjRecord> {
        Some(ObjRecord {
            global: self.u64()?,
            kind: self.u8()?,
            state: self.state()?,
            src_world: self.u32()?,
            src_frame: self.u64()?,
            via: self.u64()?,
        })
    }
    fn export(&mut self) -> Option<Option<Box<Export>>> {
        Some(match self.u8()? {
            0 => None,
            1 => Some(Box::new(Export::Committed(self.hop()?))),
            2 => Some(Box::new(Export::Departed {
                src: self.u32()?,
                epoch: self.u32()?,
                depart: self.id()?,
                record: self.record()?,
                dest: self.u32()?,
                deadline: self.u64()?,
                chain: self.u64()?,
                cert: self.cert()?,
            })),
            _ => return None,
        })
    }
    fn tables(&mut self) -> Option<Tables> {
        let mut t = Tables::default();
        for _ in 0..self.u32()? {
            let tr = self.u64()?;
            let obj = self.u32()?;
            let record = match self.u8()? {
                0 => None,
                1 => Some(self.record()?),
                _ => return None,
            };
            let status = match self.u8()? {
                0 => EscrowStatus::Pending,
                1 => EscrowStatus::Committed,
                2 => EscrowStatus::Returned,
                3 => EscrowStatus::DepartRejected,
                _ => return None,
            };
            let e = Escrow { obj, record, status, depart_frame: self.u64()?, depart: self.id()?, dest: self.u32()?, deadline: self.u64()? };
            t.escrow.insert(tr, e);
        }
        for _ in 0..self.u32()? {
            let tr = self.u64()?;
            let a = match self.u8()? {
                1 => Arrival::Accepted { obj: self.u32()?, frame: self.u64()? },
                2 => Arrival::Rejected { frame: self.u64()?, until: self.u64()? },
                _ => return None,
            };
            t.arrivals.insert(tr, a);
        }
        for _ in 0..self.u32()? {
            let g = self.u64()?;
            let s = match self.u8()? {
                1 => Seen::Here {
                    via: self.u64()?,
                    from: self.u32()?,
                    from_via: self.u64()?,
                    chain: self.chain()?,
                    chash: self.u64()?,
                    settled: self.u8()? != 0,
                    entry: self.export()?,
                },
                2 => Seen::Left {
                    transfer: self.u64()?,
                    dest: self.u32()?,
                    via: self.u64()?,
                    frame: self.u64()?,
                    from: self.u32()?,
                    from_via: self.u64()?,
                    entry: self.export()?,
                },
                _ => return None,
            };
            t.seen.insert(g, s);
        }
        t.portal_open = self.u8()? != 0;
        Some(t)
    }
}

/// The canonical encoding of the world-level tables (part of the check hash and of a state transfer).
pub fn encode_tables(t: &Tables) -> Vec<u8> {
    let mut w = Wr(Vec::with_capacity(64));
    w.tables(t);
    w.0
}

/// Reads [`encode_tables`] output, from an untrusted peer: every byte must be consumed, the encoding
/// must be the canonical one (it re-encodes to the same bytes: sorted unique keys, 0 or 1 for a bool,
/// no unused layout), and each cached chain hash must match its chain.
pub fn decode_tables(b: &[u8]) -> Option<Tables> {
    let mut r = Rd::new(b);
    let t = r.tables()?;
    let chains_ok = t.seen.values().all(|s| match s {
        Seen::Here { chain, chash, .. } => *chash == chain_hash(chain),
        Seen::Left { .. } => true,
    });
    (r.1 == b.len() && chains_ok && encode_tables(&t) == b).then_some(t)
}

// ---- donor and joiner ----------------------------------------------------------------------------

/// A world's state after its check frame `frame` (design 9.5): the three canonical parts its check hash
/// covers. A donor sends it; a joiner restores it only after [`WorldSim::import_state`]'s checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateTransfer {
    pub world: u32,
    pub frame: u64,
    /// `pe_physics::Transfer::to_bytes`.
    pub physics: Vec<u8>,
    /// `pe_sim::Snapshot::to_bytes` of the engine tables' store.
    pub store: Vec<u8>,
    /// [`encode_tables`].
    pub tables: Vec<u8>,
}

/// Why a joiner refused a state transfer. Each is the donor's failure to deliver, never a reason to
/// trust less: the joiner asks another member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferError {
    /// For another world.
    OtherWorld,
    /// No check hash for that frame was signed identically by a quorum of its epoch's members with at
    /// least one other than the donor (review P22S-1), yet.
    NoQuorum,
    /// A part is not canonical or not completely consumed.
    Physics,
    Store,
    Tables,
    /// The parts are canonical but their check hash is not the trusted one.
    Hash,
    /// Review P23-3: not an answer to this member's request to that donor ([`crate::peer::SessionCore`]).
    Unrequested,
    /// Review P23-3: the state would not move this replica forward (its frame is not past the
    /// replica's simulated frame), so restoring it would rewind.
    Behind,
}

impl WorldSim {
    /// The check hash after frame `f` (this replica's state) and the parts it covers.
    pub(super) fn check_state(&mut self, f: u64) -> (CheckHash, StateTransfer) {
        let t0 = now_us();
        let physics = self.world.save_transfer().expect("a usable physics world").to_bytes();
        let ph = pe_physics::CheckHash::of(&physics);
        let t1 = now_us();
        // A NaN is a deterministic error on every peer, never hashed (review P21-5).
        self.store.check_values().expect("no invalid value in the engine tables");
        let store = self.store.snapshot().to_bytes();
        let tables = encode_tables(&self.t);
        let c = check_hash_of(&ph, self.frame, &store, &tables);
        self.stats.checks += 1;
        self.stats.check_physics_us += t1 - t0;
        self.stats.check_rest_us += now_us() - t1;
        (c, StateTransfer { world: self.world_id, frame: f, physics, store, tables })
    }

    /// The newest confirmed check-frame state this replica holds, for a joiner (design 9.5).
    pub fn export_state(&self) -> Option<StateTransfer> {
        let fin = self.conf.min(self.frame);
        self.served.range(..fin).next_back().map(|(_, s)| s.clone())
    }

    /// The check hash a joiner trusts for check frame `frame` (review P22S-1): one that a quorum of the
    /// members of the epoch active at `frame` signed identically in their acks, at least one of them a
    /// member other than `donor`. Never the donor's word alone: a world with a single member cannot
    /// vouch for its own transfer. Members a later epoch excluded by proof still count (unlike
    /// `cert_at`, which authorizes a live act of the current membership): their acks attested frame
    /// `frame` in its own epoch, and the verdict must not depend on how far the reader's epoch has
    /// advanced (@engine-architect, 2026-10-31). If two hashes each reach a quorum, neither is trusted
    /// (review P23-8). Known limit: the epoch is the joiner's own view, so it must hold the decided
    /// changes up to `frame` (the relayed log carries them) before it asks.
    pub fn trusted_check(&self, frame: u64, donor: MemberId) -> Option<CheckHash> {
        let members = self.members_at(self.epoch_at(frame))?;
        let (_, q) = quorum(self.cfg.quorum, members.len());
        let mut signed: Vec<(CheckHash, BTreeSet<MemberId>)> = Vec::new();
        for (id, e) in &self.entries {
            if let Payload::Ack { check: Some((f, h)), .. } = &e.payload {
                if *f == frame && members.contains(&e.member) && !self.is_void(*id) {
                    match signed.iter_mut().find(|(x, _)| x == h) {
                        Some((_, by)) => {
                            by.insert(e.member);
                        }
                        None => signed.push((*h, BTreeSet::from([e.member]))),
                    }
                }
            }
        }
        // Review P23-8: two hashes each signed by a quorum prove a fault; trust neither.
        let mut vouched = signed.into_iter().filter(|(_, by)| by.len() >= q && by.iter().any(|&m| m != donor)).map(|(h, _)| h);
        let h = vouched.next()?;
        vouched.next().is_none().then_some(h)
    }

    /// Restores a donor's state transfer (design 9.5): only if its world matches, a quorum vouches for
    /// a check hash at its frame ([`WorldSim::trusted_check`], from the signed acks this replica holds),
    /// each part is canonical and completely consumed, and the parts hash to that check hash. Then the
    /// replica continues from the frame after it: entries stamped at or before it are already in the
    /// state, and it never rolls back below it. Nothing changes on a refusal.
    pub fn import_state(&mut self, st: &StateTransfer, donor: MemberId) -> Result<(), TransferError> {
        if st.world != self.world_id {
            return Err(TransferError::OtherWorld);
        }
        if st.frame < self.frame {
            return Err(TransferError::Behind);
        }
        let trusted = self.trusted_check(st.frame, donor).ok_or(TransferError::NoQuorum)?;
        // Review P23-1: the raw parts are hashed first, so nothing parses bytes no quorum vouched for.
        let ph = pe_physics::CheckHash::of(&st.physics);
        let next = st.frame + 1;
        if check_hash_of(&ph, next, &st.store, &st.tables) != trusted {
            return Err(TransferError::Hash);
        }
        let transfer = pe_physics::Transfer::from_bytes(&st.physics).map_err(|_| TransferError::Physics)?;
        let snap = pe_sim::Snapshot::from_bytes(&st.store, &session_registry()).map_err(|_| TransferError::Store)?;
        if snap.to_bytes() != st.store {
            return Err(TransferError::Store);
        }
        let t = decode_tables(&st.tables).ok_or(TransferError::Tables)?;
        // From here the bytes are a state a quorum, so an honest member, computed.
        let world = PhysicsWorld::from_transfer(PhysicsConfig::engine(), &transfer, &ph).map_err(|_| TransferError::Physics)?;
        self.store.restore(&snap).map_err(|_| TransferError::Store)?;
        self.world = world;
        self.t = t;
        self.frame = next;
        self.floor = next;
        self.dirty_from = None;
        self.compacted_len = 0;
        // What this replica computed before is superseded.
        self.hashes.clear();
        self.check_hashes.clear();
        self.check_hashes.insert(st.frame, trusted);
        self.outcomes.clear();
        self.obj_hist.clear();
        self.pos_hist.clear();
        self.ring.clear();
        let k = self.save();
        self.ring.insert(next, k);
        self.served.clear();
        self.served.insert(st.frame, st.clone());
        self.stats.imports += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tables {
        let hop = Hop { src: 2, dest: 1, transfer: 77, via: 0, epoch: 0, frame: 140, certs: vec![], cert: vec![(1, [5; 64]), (2, [6; 64])] };
        let record = ObjRecord { global: (2 << 32) | 9, kind: 1, state: BodyState([7; 16]), src_world: 2, src_frame: 130, via: 0 };
        let mut t = Tables { portal_open: true, ..Tables::default() };
        let esc = Escrow { obj: 3, record: Some(record), status: EscrowStatus::Pending, depart_frame: 150, depart: EntryId([4; 32]), dest: 2, deadline: 900 };
        t.escrow.insert(91, esc);
        t.escrow.insert(92, Escrow { record: None, status: EscrowStatus::Committed, ..esc });
        t.arrivals.insert(77, Arrival::Accepted { obj: 20, frame: 141 });
        t.arrivals.insert(78, Arrival::Rejected { frame: 160, until: 700 });
        let chain = vec![hop.clone()];
        let export = Export::Departed { src: 2, epoch: 0, depart: EntryId([8; 32]), record, dest: 1, deadline: 800, chain: 3, cert: vec![(2, [9; 64])] };
        t.seen.insert(record.global, Seen::Here { via: 77, from: 2, from_via: 0, chash: chain_hash(&chain), chain, settled: true, entry: Some(Box::new(export)) });
        t.seen.insert(record.global + 1, Seen::Left { transfer: 91, dest: 2, via: 0, frame: 152, from: 1, from_via: 0, entry: Some(Box::new(Export::Committed(hop))) });
        t
    }

    #[test]
    fn tables_round_trip() {
        let b = encode_tables(&sample());
        let t = decode_tables(&b).expect("canonical bytes decode");
        assert_eq!(encode_tables(&t), b);
        assert_eq!(decode_tables(&encode_tables(&Tables::default())).map(|t| encode_tables(&t)), Some(encode_tables(&Tables::default())));
    }

    #[test]
    fn state_messages_round_trip() {
        let st = StateTransfer { world: 3, frame: 90, physics: vec![1, 2, 3], store: vec![4; 40], tables: encode_tables(&sample()) };
        for m in [Msg::FetchState, Msg::State(Box::new(st))] {
            assert_eq!(decode_msg(&encode_msg(&m)), Some(m));
        }
    }

    #[test]
    fn tables_reader_refuses_non_canonical_bytes() {
        let b = encode_tables(&sample());
        // Trailing byte; truncated.
        assert!(decode_tables(&[b.as_slice(), &[0]].concat()).is_none());
        assert!(decode_tables(&b[..b.len() - 1]).is_none());
        // The portal flag (the last byte) as 2 instead of 1.
        let mut x = b.clone();
        *x.last_mut().unwrap() = 2;
        assert!(decode_tables(&x).is_none());
        // Escrow keys out of order: swap the two escrow transfer ids (91, 92).
        let mut x = b.clone();
        let first = 4;
        let second = (first + 8..b.len() - 8).find(|&i| b[i..i + 8] == 92u64.to_le_bytes()).expect("second escrow key");
        x[first..first + 8].copy_from_slice(&92u64.to_le_bytes());
        x[second..second + 8].copy_from_slice(&91u64.to_le_bytes());
        assert!(decode_tables(&x).is_none());
        // A cached chain hash that does not match its chain.
        let mut t = sample();
        if let Some(Seen::Here { chash, .. }) = t.seen.values_mut().next() {
            *chash ^= 1;
        }
        assert!(decode_tables(&encode_tables(&t)).is_none());
    }
}
