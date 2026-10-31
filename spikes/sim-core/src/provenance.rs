//! R14 per-object provenance chains (design 22.7, D46's option for untrusted partners; phase-2
//! package P2-5). Without them, a route-listed world whose majority is dishonest can certify a
//! departure of any global id: one born elsewhere, live elsewhere, or spent.
//!
//! * **Birth binding.** A global id is `birth world << 32 | index`. Its birth record is the birth
//!   world's genesis object list (the spike has no runtime births; production adds a certified
//!   `Born` fact by the birth world's majority [I]).
//! * **Hops.** Once a source world commits a transfer (the destination's certified acceptance), its
//!   members attest [`fact_committed`]. That certificate is the [`Hop`]: proof that the object
//!   really left `src` for `dest`, having entered `src` by transfer `via` (0: born there). A
//!   departure certificate alone is not enough: a dishonest destination could certify "rejected"
//!   to the source (refund) and still claim the object.
//! * **Chains.** Every `Arrive` carries the hops that brought the object from its birth world into
//!   the source; the source's departure certificate covers the chain's hash, so a relayer cannot
//!   strip or swap it. The destination checks each hop against the epoch certificates of the
//!   world that signed it ([`check_chain`]), whether or not that world is on its route list.
//! * **Last exits.** Each world remembers, per global id, whether the object is here or which
//!   transfer last took it away ([`Seen`]). An arrival whose chain skips that exit is a fork.
//!
//! What remains [I]: a dishonest-majority world can still double-spend an object it legitimately
//! received, into a world that has not seen the object since (both exports cite the same incoming
//! hop, and are a transferable equivocation proof against that world). User ruling D79: detect
//! only, by those fork proofs ([`ForkProof`], with both certificates); a per-object registry
//! ordering every hop was rejected.
//!
//! The guarantee is **no double identity** (review P25-1): content and new births are each
//! holder's word, which a destination restricts with its [`ImportPolicy`]. A source that withholds
//! its commit certificate pins the object in its destination (griefing, not duplication; P25-2).

use std::collections::{BTreeMap, BTreeSet};

use crate::fnv_u64s;
use crate::session::{cert_ok, epoch_cert_ok, fact_departed, transfer_id, EntryId, EpochCert, MemberId, ObjRecord, QuorumRule};

/// The world an object was born in.
pub fn birth_world(global: u64) -> u32 {
    (global >> 32) as u32
}

/// One completed hop of an object's journey, with the certificate of the world it left.
#[derive(Clone, Debug, PartialEq)]
pub struct Hop {
    pub src: u32,
    pub dest: u32,
    pub transfer: u64,
    /// The transfer that brought the object into `src` (0: born there).
    pub via: u64,
    /// The membership epoch of `src` whose majority signed `cert`.
    pub epoch: u32,
    /// The frame of `src` at which the transfer committed: `epoch` must be the epoch active then.
    pub frame: u64,
    /// `src`'s epoch certificates 1..=`epoch` (P2-4), so any world can check `epoch` from `src`'s
    /// genesis, on its route list or not.
    pub certs: Vec<EpochCert>,
    /// Signatures over [`fact_committed`] by a strict majority of `src`'s members in `epoch` (those
    /// not excluded by proof, when `certs` holds the next epoch's certificate: VC-8).
    pub cert: Vec<(MemberId, u64)>,
}

impl Hop {
    /// The fact `cert` signs for object `global`: its frame and epoch included (VC-7), so a carrier
    /// can't relabel either.
    pub fn fact(&self, global: u64) -> u64 {
        fact_committed(self.src, self.transfer, global, self.dest, self.via, self.frame, self.epoch)
    }
}

/// "World `src` committed transfer `transfer` of object `global` to world `dest` at its frame
/// `frame`, in its epoch `epoch`; the object had entered `src` by transfer `via`." A member signs it
/// only for the epoch active at that frame in its own log (VC-7), and signs again if a later change
/// moves that frame into another epoch.
pub fn fact_committed(src: u32, transfer: u64, global: u64, dest: u32, via: u64, frame: u64, epoch: u32) -> u64 {
    fnv_u64s([3, src as u64, transfer, global, dest as u64, via, frame, epoch as u64])
}

/// Hash of a chain, certificates included, so a departure certificate pins the exact chain.
pub fn chain_hash(chain: &[Hop]) -> u64 {
    let mut v = vec![chain.len() as u64];
    for h in chain {
        v.extend([h.src as u64, h.dest as u64, h.transfer, h.via, h.epoch as u64, h.frame, h.certs.len() as u64]);
        v.extend(h.certs.iter().map(|c| c.hash()));
        v.push(h.cert.len() as u64);
        for (m, s) in &h.cert {
            v.extend([*m as u64, *s]);
        }
    }
    fnv_u64s(v)
}

/// What P2-5 needs from P2-4's epoch certificates. Production: a world id commits to its genesis
/// document, and each membership change is certified by the previous epoch's majority, so anyone
/// can verify any world's epoch chain [I].
pub trait EpochCerts {
    /// Members of `world` in `epoch`.
    fn members(&self, world: u32, epoch: u32) -> Option<&[MemberId]>;
    /// How many objects `world` was born with: its birth records.
    fn genesis_objects(&self, world: u32) -> Option<u64>;
    /// The members of `epoch` whose majority certifies a fact: all of them, minus those the next
    /// epoch's certificate excluded by proof if it is known (VC-8; the source's `cert_at` counts the
    /// same way).
    fn quorum(&self, world: u32, epoch: u32) -> Option<Vec<MemberId>> {
        self.members(world, epoch).map(|m| m.to_vec())
    }
    /// `cert` holds signatures over `fact` by a strict majority of `world`'s quorum in `epoch`.
    fn cert_ok(&self, world: u32, epoch: u32, fact: u64, cert: &[(MemberId, u64)]) -> bool {
        self.quorum(world, epoch).is_some_and(|m| cert_ok(&m, world, fact, cert))
    }
}

/// P2-4's epoch certificates as P2-5 uses them (P25-5): each world's genesis (members and birth
/// records), and the epochs verified from it certificate by certificate. In the spike every world's
/// genesis is known, standing in for self-certifying world ids (a hash of the genesis document).
#[derive(Clone, Debug)]
pub struct EpochBook {
    pub rule: QuorumRule,
    pub worlds: BTreeMap<u32, WorldEpochs>,
}

impl Default for EpochBook {
    fn default() -> Self {
        EpochBook { rule: QuorumRule::Bft, worlds: BTreeMap::new() }
    }
}

/// A partner world's epoch at the frame a cross-world fact is about, with its epoch certificates
/// (1..=`epoch`, and the next one once decided), so a receiver checks the signers from the partner's
/// genesis rather than a member list fixed at startup (22.7, R3-8b). The frame comes with the fact:
/// a departure's is its record's `src_frame`, a verdict's travels in the `Verdict`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct EpochProof {
    pub epoch: u32,
    pub certs: Vec<EpochCert>,
}

impl EpochProof {
    pub fn hash(&self) -> u64 {
        fnv_u64s([self.epoch as u64, self.certs.len() as u64].into_iter().chain(self.certs.iter().map(|c| c.hash())))
    }
}

/// One world's known epochs: members and first frame of each (epoch 0 is the genesis), and the
/// members of each that the next epoch's certificate excluded by proof (empty until it is known).
#[derive(Clone, Debug, Default)]
pub struct WorldEpochs {
    pub objects: u64,
    pub members: Vec<Vec<MemberId>>,
    pub from: Vec<u64>,
    pub excluded: Vec<Vec<MemberId>>,
}

impl EpochBook {
    pub fn genesis(&mut self, world: u32, members: &[MemberId], objects: u64) {
        let mut m = members.to_vec();
        m.sort_unstable();
        self.worlds.insert(world, WorldEpochs { objects, members: vec![m], from: vec![0], excluded: vec![vec![]] });
    }

    /// Add `cert` as its world's next epoch if it checks against the previous one. A certificate
    /// for an epoch already known must agree with it.
    pub fn add(&mut self, cert: &EpochCert) -> bool {
        let rule = self.rule;
        let Some(w) = self.worlds.get_mut(&cert.world) else { return false };
        let e = cert.epoch as usize;
        if e == 0 || e > w.members.len() {
            return false;
        }
        if e < w.members.len() {
            return w.members[e] == cert.members && w.from[e] == cert.from_frame();
        }
        if !epoch_cert_ok(rule, &w.members[e - 1], cert) || cert.from_frame() < w.from[e - 1] {
            return false;
        }
        let mut ex: Vec<MemberId> = cert.change.excluded.iter().map(|(m, _)| *m).collect();
        ex.sort_unstable();
        w.excluded[e - 1] = ex;
        w.members.push(cert.members.clone());
        w.from.push(cert.from_frame());
        w.excluded.push(Vec::new());
        true
    }

    /// This book plus `certs`, if every one checks.
    pub fn with(&self, certs: &[EpochCert]) -> Option<EpochBook> {
        let mut b = self.clone();
        certs.iter().all(|c| b.add(c)).then_some(b)
    }

    /// The epoch of `world` active at `frame`, as far as this book knows.
    pub fn epoch_at(&self, world: u32, frame: u64) -> Option<u32> {
        let w = self.worlds.get(&world)?;
        Some(w.from.iter().rposition(|&f| f <= frame)? as u32)
    }

    /// `cert` is a majority of `world`'s quorum in the epoch `proof` names, which is the one active at
    /// `frame` by the certificates `proof` carries (they must extend this book), over `fact`.
    pub fn vouched(&self, world: u32, frame: u64, proof: &EpochProof, fact: u64, cert: &[(MemberId, u64)]) -> bool {
        self.with(&proof.certs).is_some_and(|b| b.epoch_at(world, frame) == Some(proof.epoch) && b.cert_ok(world, proof.epoch, fact, cert))
    }

    /// A hop's certificate checks: its epoch certificates extend this book, its epoch is the one
    /// active at its frame by them, and a strict majority of that epoch's quorum signed its fact for
    /// `global`.
    pub fn hop_ok(&self, h: &Hop, global: u64) -> bool {
        self.with(&h.certs).is_some_and(|b| b.epoch_at(h.src, h.frame) == Some(h.epoch) && b.cert_ok(h.src, h.epoch, h.fact(global), &h.cert))
    }
}

impl EpochCerts for EpochBook {
    fn members(&self, world: u32, epoch: u32) -> Option<&[MemberId]> {
        self.worlds.get(&world)?.members.get(epoch as usize).map(|m| m.as_slice())
    }
    fn genesis_objects(&self, world: u32) -> Option<u64> {
        self.worlds.get(&world).map(|w| w.objects)
    }
    fn quorum(&self, world: u32, epoch: u32) -> Option<Vec<MemberId>> {
        let w = self.worlds.get(&world)?;
        let ex = w.excluded.get(epoch as usize)?;
        Some(w.members.get(epoch as usize)?.iter().copied().filter(|m| !ex.contains(m)).collect())
    }
}

/// Why a chain does not prove an object's way into its source world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainError {
    /// The global id is not a birth record of its birth world.
    NoBirth,
    /// Hop `i` does not continue from where the chain had the object.
    Link(usize),
    /// Hop `i`'s certificate is not a majority of its world's epoch.
    Cert(usize),
    /// Hop `i`'s epoch certificates do not check, or its epoch is not the one active at its frame.
    Epoch(usize),
    /// The chain does not end with the object entering the source by `via`.
    End,
}

/// Check that `chain` takes object `global` from its birth to world `src`, entering it by `via`.
/// Each hop is checked against the epoch certificates it carries (P25-5).
pub fn check_chain(book: &EpochBook, global: u64, src: u32, via: u64, chain: &[Hop]) -> Result<(), ChainError> {
    let born = birth_world(global);
    if !book.genesis_objects(born).is_some_and(|n| (global & 0xFFFF_FFFF) < n) {
        return Err(ChainError::NoBirth);
    }
    let (mut at, mut by) = (born, 0u64);
    for (i, h) in chain.iter().enumerate() {
        if h.src != at || h.via != by || h.dest == h.src {
            return Err(ChainError::Link(i));
        }
        let Some(b) = book.with(&h.certs) else { return Err(ChainError::Epoch(i)) };
        if b.epoch_at(h.src, h.frame) != Some(h.epoch) {
            return Err(ChainError::Epoch(i));
        }
        if !b.cert_ok(h.src, h.epoch, h.fact(global), &h.cert) {
            return Err(ChainError::Cert(i));
        }
        (at, by) = (h.dest, h.transfer);
    }
    if at != src || by != via {
        return Err(ChainError::End);
    }
    Ok(())
}

/// One world's certified export of an object: proof that its majority let the object go.
#[derive(Clone, Debug, PartialEq)]
pub enum Export {
    /// The source's commit certificate ([`fact_committed`]): a hop of a chain.
    Committed(Hop),
    /// The source's departure certificate ([`fact_departed`]), as carried by the `Arrive` that
    /// brought the object into a world, with the source's epoch at the departure frame and its epoch
    /// certificates, so a third party checks it from the source's genesis.
    Departed { src: u32, proof: EpochProof, depart: EntryId, record: ObjRecord, dest: u32, deadline: u64, chain: u64, cert: Vec<(MemberId, u64)> },
}

impl Export {
    pub fn world(&self) -> u32 {
        match self {
            Export::Committed(h) => h.src,
            Export::Departed { src, .. } => *src,
        }
    }
    /// The transfer the object had entered the exporting world by.
    pub fn via(&self) -> u64 {
        match self {
            Export::Committed(h) => h.via,
            Export::Departed { record, .. } => record.via,
        }
    }
    /// The transfer it left by.
    pub fn transfer(&self) -> u64 {
        match self {
            Export::Committed(h) => h.transfer,
            Export::Departed { src, depart, .. } => transfer_id(*src, *depart),
        }
    }
    /// Hash of every field, certificate included (for the state hash).
    pub fn digest(&self) -> u64 {
        let mut v = match self {
            Export::Committed(h) => vec![1, chain_hash(std::slice::from_ref(h))],
            Export::Departed { src, proof, depart, record, dest, deadline, chain, cert } => {
                let mut v = vec![2, *src as u64, proof.hash(), *depart, record.global, record.kind as u64, record.state.hash()];
                v.extend([record.src_world as u64, record.src_frame, record.via, *dest as u64, *deadline, *chain]);
                v.extend(cert.iter().flat_map(|(m, s)| [*m as u64, *s]));
                v
            }
        };
        v.push(self.transfer());
        fnv_u64s(v)
    }

    /// The certificate is a quorum of the exporting world's epoch, over this export of `global`; a
    /// hop's epoch is checked from the certificates it carries (P25-5).
    pub fn verify(&self, book: &EpochBook, global: u64) -> bool {
        match self {
            Export::Committed(h) => book.hop_ok(h, global),
            Export::Departed { src, proof, depart, record, dest, deadline, chain, cert } => {
                record.global == global
                    && book.vouched(*src, record.src_frame, proof, fact_departed(*src, *depart, record, *dest, *deadline, *chain, proof.epoch), cert)
            }
        }
    }
}

/// What a world knows of one global id (part of its rollback state).
#[derive(Clone, Debug, PartialEq)]
pub enum Seen {
    /// The object is here, live or in escrow. It entered by `via` (0: born here) from world `from`,
    /// where it had entered by `from_via`; `entry` is the certified export that brought it (None:
    /// born here). `chain` is its verified provenance up to `from`, and `chash` its cached
    /// [`chain_hash`]; once `settled`, `chain` also holds the hop into this world, and the object
    /// may leave again.
    Here { via: u64, from: u32, from_via: u64, chain: Vec<Hop>, chash: u64, settled: bool, entry: Option<Box<Export>> },
    /// The object left by `transfer` (committed at `frame`) for `dest`, having entered by `via`
    /// from world `from` (where it had entered by `from_via`), by the export `entry`.
    Left { transfer: u64, dest: u32, via: u64, frame: u64, from: u32, from_via: u64, entry: Option<Box<Export>> },
}

impl Seen {
    /// Everything this record holds goes into the state hash, so a sync check catches any
    /// difference in it between replicas: the chain itself (not only its cached hash) and the
    /// export that brought the object in, with its certificate.
    pub fn hash_into(&self, global: u64, v: &mut Vec<u64>) {
        match self {
            Seen::Here { via, from, from_via, chain, chash, settled, entry } => {
                debug_assert_eq!(*chash, chain_hash(chain), "cached chain hash out of date");
                v.extend([global, 12, *via, *from as u64, *from_via, chain_hash(chain), *settled as u64]);
                v.push(entry.as_ref().map_or(0, |e| e.digest()));
            }
            Seen::Left { transfer, dest, via, frame, from, from_via, entry } => {
                v.extend([global, 13, *transfer, *dest as u64, *via, *frame, *from as u64, *from_via]);
                v.push(entry.as_ref().map_or(0, |e| e.digest()));
            }
        }
    }

    fn entry(&self) -> Option<&Export> {
        match self {
            Seen::Here { entry, .. } | Seen::Left { entry, .. } => entry.as_deref(),
        }
    }
}

/// A transferable equivocation proof against `world`: its majority certified two different exports
/// of object `global`, both of the one entry `via` (P25-3: with both certificates, so any third
/// party can check it with [`ForkProof::verify`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ForkProof {
    pub world: u32,
    pub global: u64,
    pub via: u64,
    pub a: Export,
    pub b: Export,
}

impl ForkProof {
    pub fn key(&self) -> (u32, u64, u64, u64, u64) {
        (self.world, self.global, self.via, self.a.transfer(), self.b.transfer())
    }
    pub fn verify(&self, book: &EpochBook) -> bool {
        [&self.a, &self.b].iter().all(|x| x.world() == self.world && x.via() == self.via && x.verify(book, self.global))
            && self.a.transfer() != self.b.transfer()
    }
}

/// Compare what this world knows of an object (`known`) with an arriving path: the chain's hops
/// plus the source's own departure (`arrival`). Two different exports by one world of the same
/// entry are a double spend by that world's majority.
pub fn fork_proof(known: &Seen, global: u64, chain: &[Hop], arrival: &Export) -> Option<ForkProof> {
    let mine = known.entry()?;
    let path = chain.iter().map(|h| Export::Committed(h.clone())).chain([arrival.clone()]);
    path.filter(|x| x.world() == mine.world() && x.via() == mine.via() && x.transfer() != mine.transfer())
        .map(|b| ForkProof { world: mine.world(), global, via: mine.via(), a: mine.clone(), b })
        .next()
}

/// What a destination accepts beyond identity (P25-1). Provenance proves that an object has one
/// identity, never two live copies; it does not vouch for the object's content (kind, state and
/// carried data are signed by each holder's majority alone), nor for the honesty of the world that
/// gave birth to it. This hook lets a world restrict both. Stub: the spike's worlds accept anything.
#[derive(Clone, Debug, Default)]
pub struct ImportPolicy {
    /// Birth worlds whose objects this world accepts (None: any).
    pub births: Option<BTreeSet<u32>>,
    /// Worlds a chain may pass through (None: any, including worlds off this world's route list).
    pub chain_worlds: Option<BTreeSet<u32>>,
    /// Per source world, the kinds accepted from it (None: any). Production adds per-kind content
    /// checks, run by the destination's logic on arrival (22.8, 22.9).
    pub kinds: Option<BTreeMap<u32, BTreeSet<u8>>>,
}

impl ImportPolicy {
    pub fn allows(&self, global: u64, src: u32, kind: u8, chain: &[Hop]) -> bool {
        self.births.as_ref().is_none_or(|b| b.contains(&birth_world(global)))
            && self.chain_worlds.as_ref().is_none_or(|w| chain.iter().all(|h| w.contains(&h.src)))
            && self.kinds.as_ref().is_none_or(|k| k.get(&src).is_some_and(|s| s.contains(&kind)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::member_sig;

    fn book() -> EpochBook {
        let mut b = EpochBook::default();
        b.genesis(1, &[1, 2], 100);
        b.genesis(2, &[1, 3], 100);
        b.genesis(3, &[1, 4], 100);
        b
    }

    fn hop(b: &EpochBook, src: u32, dest: u32, transfer: u64, via: u64, g: u64) -> Hop {
        let fact = fact_committed(src, transfer, g, dest, via, 0, 0);
        let cert = b.members(src, 0).unwrap().iter().map(|&m| (m, member_sig(src, m, fact))).collect();
        Hop { src, dest, transfer, via, epoch: 0, frame: 0, certs: Vec::new(), cert }
    }

    #[test]
    fn a_b_c_a() {
        let b = book();
        let g = (1u64 << 32) | 40;
        assert_eq!(check_chain(&b, g, 1, 0, &[]), Ok(()));
        let h1 = hop(&b, 1, 2, 11, 0, g);
        let h2 = hop(&b, 2, 3, 22, 11, g);
        let h3 = hop(&b, 3, 1, 33, 22, g);
        assert_eq!(check_chain(&b, g, 2, 11, &[h1.clone()]), Ok(()));
        assert_eq!(check_chain(&b, g, 3, 22, &[h1.clone(), h2.clone()]), Ok(()));
        assert_eq!(check_chain(&b, g, 1, 33, &[h1.clone(), h2.clone(), h3]), Ok(()));
        // Born elsewhere, minted, forged, spliced, or cut short.
        assert_eq!(check_chain(&b, g, 2, 0, &[]), Err(ChainError::End));
        assert_eq!(check_chain(&b, (1u64 << 32) | 100, 1, 0, &[]), Err(ChainError::NoBirth));
        assert_eq!(check_chain(&b, (9u64 << 32) | 1, 9, 0, &[]), Err(ChainError::NoBirth));
        let mut forged = h1.clone();
        forged.cert.truncate(1);
        assert_eq!(check_chain(&b, g, 2, 11, &[forged]), Err(ChainError::Cert(0)));
        assert_eq!(check_chain(&b, g + 1, 2, 11, &[h1.clone()]), Err(ChainError::Cert(0)));
        assert_eq!(check_chain(&b, g, 3, 22, &[h2]), Err(ChainError::Link(0)));
        assert_eq!(check_chain(&b, g, 3, 22, &[h1]), Err(ChainError::End));
    }

    /// P25-5: world 1 (members 1, 2, 4) removes member 2 from frame 300 (epoch 1). A hop signed by
    /// epoch 1 checks only with that certificate attached, whether or not world 1 is on the checking
    /// world's route list; epoch 0 can no longer sign for a frame of epoch 1 once epoch 1 is known.
    #[test]
    fn epochs_from_certificates() {
        use crate::session::{vc_sig, Change, Removal};
        let mut b = EpochBook::default();
        b.genesis(1, &[1, 2, 4], 100);
        b.genesis(2, &[1, 3], 100);
        let change = Change { epoch: 0, remove: vec![Removal { member: 2, votes: vec![11, 12], cut: (40, 99), kick: 0 }], add: vec![], boundary: 300, excluded: vec![], admin: vec![], add_roles: vec![], add_attests: vec![], add_reqs: vec![] };
        let value = change.value();
        let sig = |m| (m, vc_sig(1, m, 0, 1, value));
        let c1 = EpochCert { world: 1, epoch: 1, change: change.clone(), round: 1, members: vec![1, 4], precommits: vec![sig(1), sig(4)], proofs: vec![] };
        assert!(b.with(&[c1.clone()]).is_some());
        // One signature of three is no quorum; a member list that does not follow is refused.
        assert!(b.with(&[EpochCert { precommits: vec![sig(1)], ..c1.clone() }]).is_none());
        assert!(b.with(&[EpochCert { members: vec![1, 2, 4], ..c1.clone() }]).is_none());
        // Signatures over another change do not carry over to this one.
        let other = Change { boundary: 200, ..change.clone() };
        assert!(b.with(&[EpochCert { change: other, ..c1.clone() }]).is_none());
        let g = (1u64 << 32) | 7;
        let signed = |frame: u64, epoch: u32, ms: &[MemberId]| {
            let fact = fact_committed(1, 21, g, 2, 0, frame, epoch);
            ms.iter().map(|&m| (m, member_sig(1, m, fact))).collect::<Vec<_>>()
        };
        let h = Hop { src: 1, dest: 2, transfer: 21, via: 0, epoch: 1, frame: 420, certs: vec![c1.clone()], cert: signed(420, 1, &[1, 4]) };
        assert_eq!(check_chain(&b, g, 2, 21, &[h.clone()]), Ok(()));
        // Without the certificate, epoch 1 is unknown.
        assert_eq!(check_chain(&b, g, 2, 21, &[Hop { certs: vec![], ..h.clone() }]), Err(ChainError::Epoch(0)));
        // Epoch 0's majority (1 and 2) signing for frame 420, once epoch 1 is known: refused.
        let old = Hop { epoch: 0, cert: signed(420, 0, &[1, 2]), ..h.clone() };
        assert_eq!(check_chain(&b, g, 2, 21, &[old.clone()]), Err(ChainError::Epoch(0)));
        // ... but accepted if the carrier omits epoch 1 (the long-range residual of 22.7: only a
        // colluding old majority signs a frame of epoch 1 as epoch 0).
        assert_eq!(check_chain(&b, g, 2, 21, &[Hop { certs: vec![], ..old.clone() }]), Ok(()));
        // VC-7: the frame and epoch are signed, so a carrier can't relabel them: the honest epoch-0
        // certificate for frame 250 doesn't pass as frame 420, nor epoch 1's as epoch 0.
        let early = Hop { epoch: 0, frame: 250, cert: signed(250, 0, &[1, 2]), ..h.clone() };
        assert_eq!(check_chain(&b, g, 2, 21, &[early.clone()]), Ok(()));
        assert_eq!(check_chain(&b, g, 2, 21, &[Hop { frame: 260, ..early }]), Err(ChainError::Cert(0)));
        assert_eq!(check_chain(&b, g, 2, 21, &[Hop { frame: 250, ..h.clone() }]), Err(ChainError::Epoch(0)));
        // A tampered certificate does not check.
        let mut bad = c1;
        bad.precommits[1].1 ^= 1;
        assert_eq!(check_chain(&b, g, 2, 21, &[Hop { certs: vec![bad], ..h }]), Err(ChainError::Epoch(0)));
    }

    /// VC-8: world 1 (members 1, 2, 3, 4) excludes member 4 by proof in epoch 1's certificate. A hop
    /// of epoch 0 signed by 1 and 2 is a majority of the quorum {1, 2, 3} once epoch 1's certificate
    /// is carried (as the source's `cert_at` counts), and not without it (a majority of all four).
    #[test]
    fn excluded_members_leave_the_quorum() {
        use crate::session::{vc_sig, Change, Entry, Payload, Proof};
        let mut b = EpochBook::default();
        b.genesis(1, &[1, 2, 3, 4], 100);
        b.genesis(2, &[1, 3], 100);
        // Member 4 signed two different entries with one seq: an equivocation proof.
        let e = |fact| Entry { member: 4, seq: 1, prev: 0, stamp: 0, payload: Payload::Attest { fact, sig: 0 }, carried: None };
        let (ea, eb) = (e(1), e(2));
        let proof = Proof { a: ea.id(), b: eb.id() };
        let change = Change { epoch: 0, remove: vec![], add: vec![], boundary: 300, excluded: vec![(4, proof)], admin: vec![], add_roles: vec![], add_attests: vec![], add_reqs: vec![] };
        let value = change.value();
        let sig = |m| (m, vc_sig(1, m, 0, 1, value));
        let c1 = EpochCert { world: 1, epoch: 1, change, round: 1, members: vec![1, 2, 3, 4], precommits: vec![sig(1), sig(2), sig(3)], proofs: vec![ea, eb] };
        let with1 = b.with(&[c1.clone()]).expect("a valid epoch certificate with an exclusion");
        assert_eq!(with1.quorum(1, 0), Some(vec![1, 2, 3]));
        let g = (1u64 << 32) | 7;
        let fact = fact_committed(1, 21, g, 2, 0, 100, 0);
        let h = Hop { src: 1, dest: 2, transfer: 21, via: 0, epoch: 0, frame: 100, certs: vec![c1], cert: [1, 2].iter().map(|&m| (m, member_sig(1, m, fact))).collect() };
        assert_eq!(check_chain(&b, g, 2, 21, &[h.clone()]), Ok(()));
        assert_eq!(check_chain(&b, g, 2, 21, &[Hop { certs: vec![], ..h }]), Err(ChainError::Cert(0)));
    }

    /// R3-8b for departures: world 1 (members 1, 2, 4) removes member 2 from frame 300. A departure
    /// at frame 420 signed by epoch 1 (members 1 and 4), carried with epoch 1's certificate, convinces a
    /// third party; the old majority (1 and 2) labelled epoch 0 doesn't, once the certificate is
    /// carried; and nobody can relabel the epoch, since the fact covers it.
    #[test]
    fn departures_follow_the_source_epochs() {
        use crate::session::{fact_departed, vc_sig, BodyState, Change, ObjRecord, Removal};
        let mut b = EpochBook::default();
        b.genesis(1, &[1, 2, 4], 100);
        b.genesis(2, &[1, 3], 100);
        let change = Change { epoch: 0, remove: vec![Removal { member: 2, votes: vec![11, 12], cut: (40, 99), kick: 0 }], add: vec![], boundary: 300, excluded: vec![], admin: vec![], add_roles: vec![], add_attests: vec![], add_reqs: vec![] };
        let value = change.value();
        let sig = |m| (m, vc_sig(1, m, 0, 1, value));
        let c1 = EpochCert { world: 1, epoch: 1, change, round: 1, members: vec![1, 4], precommits: vec![sig(1), sig(4)], proofs: vec![] };
        let g = (1u64 << 32) | 7;
        let record = ObjRecord { global: g, kind: 0, state: BodyState([0; 13]), src_world: 1, src_frame: 420, via: 0 };
        let export = |epoch: u32, certs: Vec<EpochCert>, by: &[MemberId]| {
            let fact = fact_departed(1, 9, &record, 2, 900, chain_hash(&[]), epoch);
            let cert = by.iter().map(|&m| (m, member_sig(1, m, fact))).collect();
            Export::Departed { src: 1, proof: EpochProof { epoch, certs }, depart: 9, record, dest: 2, deadline: 900, chain: chain_hash(&[]), cert }
        };
        assert!(export(1, vec![c1.clone()], &[1, 4]).verify(&b, g));
        // Epoch 1 unknown without its certificate.
        assert!(!export(1, vec![], &[1, 4]).verify(&b, g));
        // The removed member's old majority, once epoch 1's certificate is carried: refused.
        assert!(!export(0, vec![c1.clone()], &[1, 2]).verify(&b, g));
        // ... accepted only if the carrier omits it: the long-range residual of 22.7.
        assert!(export(0, vec![], &[1, 2]).verify(&b, g));
        // Epoch 1's signatures relabelled as epoch 0 (or the reverse) check against no fact.
        let mut relabelled = export(1, vec![c1.clone()], &[1, 4]);
        if let Export::Departed { proof, .. } = &mut relabelled {
            proof.epoch = 0;
            proof.certs.clear();
        }
        assert!(!relabelled.verify(&b, g));
    }

    #[test]
    fn fork_proof_convinces_a_third_party() {
        let b = book();
        let g = (1u64 << 32) | 52;
        // World 2 received g by transfer 11 and certified two exports of it: 22 to C, 33 to A.
        let known = Seen::Left { transfer: 9, dest: 3, via: 22, frame: 0, from: 2, from_via: 11, entry: Some(Box::new(Export::Committed(hop(&b, 2, 3, 22, 11, g)))) };
        let other = Export::Committed(hop(&b, 2, 1, 33, 11, g));
        let p = fork_proof(&known, g, &[hop(&b, 1, 2, 11, 0, g)], &other).expect("two exports of entry 11");
        assert_eq!(p.key(), (2, g, 11, 22, 33));
        assert!(p.verify(&b));
        // A forged half, or one export of a different entry, convinces nobody.
        let mut forged = p.clone();
        if let Export::Committed(h) = &mut forged.b {
            h.cert.pop();
        }
        assert!(!forged.verify(&b));
        let unrelated = Export::Committed(hop(&b, 2, 1, 33, 12, g));
        assert!(fork_proof(&known, g, &[], &unrelated).is_none());
    }

    #[test]
    fn import_policy() {
        let b = book();
        let g = (1u64 << 32) | 40;
        let chain = [hop(&b, 1, 2, 11, 0, g)];
        assert!(ImportPolicy::default().allows(g, 2, 1, &chain));
        let births = ImportPolicy { births: Some([2].into()), ..Default::default() };
        assert!(!births.allows(g, 2, 1, &chain));
        let chain_worlds = ImportPolicy { chain_worlds: Some([2, 3].into()), ..Default::default() };
        assert!(!chain_worlds.allows(g, 2, 1, &chain));
        let kinds = ImportPolicy { kinds: Some([(2, [0, 2].into())].into()), ..Default::default() };
        assert!(!kinds.allows(g, 2, 1, &chain) && kinds.allows(g, 2, 2, &chain));
    }
}
