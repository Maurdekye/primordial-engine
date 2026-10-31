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
    let mut r = Rd(b, 0);
    let t = r.tables()?;
    let chains_ok = t.seen.values().all(|s| match s {
        Seen::Here { chain, chash, .. } => *chash == chain_hash(chain),
        Seen::Left { .. } => true,
    });
    (r.1 == b.len() && chains_ok && encode_tables(&t) == b).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tables {
        let hop = Hop { src: 2, dest: 1, transfer: 77, via: 0, epoch: 0, frame: 140, certs: vec![], cert: vec![(1, [5; 64]), (2, [6; 64])] };
        let record = ObjRecord { global: (2 << 32) | 9, kind: 1, state: BodyState([7; 13]), src_world: 2, src_frame: 130, via: 0 };
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
