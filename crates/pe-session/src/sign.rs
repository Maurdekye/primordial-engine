//! Entry signatures (P2-3 S3, design 10.8): every entry travels as a [`SignedEntry`], signed by its
//! author with Ed25519 over `"pe-entry" | world id | entry id`. A replica verifies the signature
//! before [`crate::session::WorldSim::insert`] stores the entry, and keeps it, so it can relay the
//! entry (fetch answers, equivocation proofs) with its author's signature.
//!
//! The entry id is the hash of the entry's canonical encoding, so the signature covers the whole
//! entry. The world id in the signed message keeps an entry from being replayed into another world.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Deref;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use crate::session::{Entry, EntryId, MemberId};
use crate::splitmix64;

/// The signing domain of entries: no other signed message starts with these bytes.
pub const ENTRY_DOMAIN: &[u8; 8] = b"pe-entry";

/// An Ed25519 signature.
pub type Sig = [u8; 64];

/// An entry with its author's signature. Derefs to the [`Entry`].
#[derive(Clone, Debug, PartialEq)]
pub struct SignedEntry {
    pub entry: Entry,
    pub sig: Sig,
}

impl Deref for SignedEntry {
    type Target = Entry;
    fn deref(&self) -> &Entry {
        &self.entry
    }
}

/// The bytes an entry's signature covers.
pub fn signing_message(world: u32, id: EntryId) -> Vec<u8> {
    let mut m = Vec::with_capacity(ENTRY_DOMAIN.len() + 4 + 32);
    m.extend_from_slice(ENTRY_DOMAIN);
    m.extend_from_slice(&world.to_le_bytes());
    m.extend_from_slice(&id.0);
    m
}

/// The signing domain of facts: R14 attestations, precommits of the view change (S3c).
pub const FACT_DOMAIN: &[u8; 8] = b"pe-fact\0";

/// The bytes a fact signature covers: `"pe-fact\0" | world | fact`. Facts are tagged 64-bit hashes
/// for now (S3c keeps the spike's fact hashing; making them collision-resistant is a follow-up).
pub fn fact_message(world: u32, fact: u64) -> [u8; 20] {
    let mut m = [0u8; 20];
    m[..8].copy_from_slice(FACT_DOMAIN);
    m[8..12].copy_from_slice(&world.to_le_bytes());
    m[12..].copy_from_slice(&fact.to_le_bytes());
    m
}

/// A signature as 64-bit words, for 64-bit hash inputs (certificate and export digests).
pub fn sig_words(s: &Sig) -> [u64; 8] {
    std::array::from_fn(|i| u64::from_le_bytes(s[8 * i..8 * i + 8].try_into().unwrap()))
}

/// Members' public keys. Member ids name players across worlds in phase 2 (one id per peer), so
/// one directory serves certificates from other worlds too.
#[derive(Clone, Debug, Default)]
pub struct PublicKeys {
    keys: BTreeMap<MemberId, VerifyingKey>,
    /// Test identities: any member's key is derived from its id (see [`KeyRing::test`]).
    test: bool,
    derived: RefCell<BTreeMap<MemberId, VerifyingKey>>,
}

impl PublicKeys {
    pub fn new(keys: BTreeMap<MemberId, VerifyingKey>) -> Self {
        PublicKeys { keys, test: false, derived: RefCell::default() }
    }

    /// Every member's test identity (see [`KeyRing::test`]).
    pub fn test() -> Self {
        PublicKeys { keys: BTreeMap::new(), test: true, derived: RefCell::default() }
    }

    pub fn add(&mut self, m: MemberId, k: VerifyingKey) {
        self.keys.insert(m, k);
    }

    pub fn get(&self, m: MemberId) -> Option<VerifyingKey> {
        if let Some(k) = self.keys.get(&m) {
            return Some(*k);
        }
        if !self.test {
            return None;
        }
        Some(*self.derived.borrow_mut().entry(m).or_insert_with(|| test_signing_key(m).verifying_key()))
    }

    /// Strict Ed25519 verification of `sig` by `m` over `msg`: no malleable signatures, no
    /// small-order keys.
    pub fn verify(&self, m: MemberId, msg: &[u8], sig: &Sig) -> bool {
        self.get(m).is_some_and(|k| k.verify_strict(msg, &Signature::from_bytes(sig)).is_ok())
    }

    /// Whether `se` carries a valid signature by its author.
    pub fn entry_ok(&self, world: u32, se: &SignedEntry) -> bool {
        self.verify(se.member, &signing_message(world, se.id()), &se.sig)
    }

    /// Whether `sig` is `m`'s signature over `fact` in `world`.
    pub fn fact_ok(&self, m: MemberId, world: u32, fact: u64, sig: &Sig) -> bool {
        self.verify(m, &fact_message(world, fact), sig)
    }
}

/// This replica's signing key (none for an offline replay) and every member's public key.
#[derive(Clone)]
pub struct KeyRing {
    secret: Option<SigningKey>,
    pub public: PublicKeys,
}

impl KeyRing {
    pub fn new(secret: Option<SigningKey>, public: PublicKeys) -> Self {
        KeyRing { secret, public }
    }

    /// No keys: verifies nothing and signs nothing (the offline reference, which reads a final log
    /// that its replicas already verified).
    pub fn none() -> Self {
        KeyRing::new(None, PublicKeys::default())
    }

    /// Deterministic test identities for simulations: every member's key is derived from its id, so
    /// joiners need no key exchange and a scenario can sign as an adversarial member. Never for a
    /// real session: anyone can compute these keys.
    pub fn test(me: MemberId) -> Self {
        KeyRing { secret: Some(test_signing_key(me)), public: PublicKeys::test() }
    }

    fn key(&self) -> &SigningKey {
        self.secret.as_ref().expect("signing needs this replica's key")
    }

    /// Signs `e`, which must be this replica's own entry. Panics without a signing key.
    pub fn sign(&self, world: u32, e: Entry) -> SignedEntry {
        let sig = self.key().sign(&signing_message(world, e.id())).to_bytes();
        SignedEntry { entry: e, sig }
    }

    /// Signs `fact` in `world` as this replica's member. Panics without a signing key.
    pub fn sign_fact(&self, world: u32, fact: u64) -> Sig {
        self.key().sign(&fact_message(world, fact)).to_bytes()
    }

    pub fn verify(&self, world: u32, se: &SignedEntry) -> bool {
        self.public.entry_ok(world, se)
    }
}

/// The test identity of `m` (see [`KeyRing::test`]).
pub fn test_signing_key(m: MemberId) -> SigningKey {
    let mut seed = [0u8; 32];
    for (i, c) in seed.chunks_mut(8).enumerate() {
        c.copy_from_slice(&splitmix64(0x7065_2d74_6573_746b ^ ((m as u64) << 8) ^ i as u64).to_le_bytes());
    }
    SigningKey::from_bytes(&seed)
}

/// Signs `e` as its author's test identity: scenario fixtures for adversarial members.
pub fn test_sign(world: u32, e: Entry) -> SignedEntry {
    let sig = test_signing_key(e.member).sign(&signing_message(world, e.id())).to_bytes();
    SignedEntry { entry: e, sig }
}

/// `m`'s test-identity signature over `fact` in `world`: scenario fixtures and unit tests.
pub fn test_fact_sig(m: MemberId, world: u32, fact: u64) -> Sig {
    test_signing_key(m).sign(&fact_message(world, fact)).to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Payload;

    fn entry(member: MemberId, seq: u32) -> Entry {
        Entry { member, seq, prev: EntryId::NONE, stamp: 5, payload: Payload::Touch { obj: 1, impulse: [0.0; 3], grab: false }, carried: None }
    }

    #[test]
    fn sign_and_verify() {
        let ring = KeyRing::test(3);
        let se = ring.sign(7, entry(3, 1));
        let other = KeyRing::test(4);
        assert!(other.verify(7, &se));
        // Another world, another entry or another author does not verify.
        assert!(!other.verify(8, &se));
        let mut changed = se.clone();
        changed.entry.stamp += 1;
        assert!(!other.verify(7, &changed));
        let mut forged = se.clone();
        forged.entry.member = 4;
        assert!(!other.verify(7, &forged));
        // Without the author's public key nothing verifies.
        assert!(!KeyRing::none().verify(7, &se));
        assert_eq!(test_sign(7, entry(3, 1)), se);
    }

    #[test]
    fn fact_signatures() {
        use crate::session::cert_ok;
        let ring = KeyRing::test(2);
        let sig = ring.sign_fact(5, 77);
        let keys = PublicKeys::test();
        assert_eq!(sig, test_fact_sig(2, 5, 77));
        assert!(keys.fact_ok(2, 5, 77, &sig));
        // Another fact, world or member does not verify; no key, no verification.
        assert!(!keys.fact_ok(2, 5, 78, &sig) && !keys.fact_ok(2, 6, 77, &sig) && !keys.fact_ok(3, 5, 77, &sig));
        assert!(!PublicKeys::default().fact_ok(2, 5, 77, &sig));
        // A certificate needs a strict majority of valid signatures.
        let good = |m| (m, test_fact_sig(m, 5, 77));
        assert!(cert_ok(&keys, &[1, 2, 3], 5, 77, &[good(1), good(2)]));
        assert!(!cert_ok(&keys, &[1, 2, 3], 5, 77, &[good(1)]));
        assert!(!cert_ok(&keys, &[1, 2, 3], 5, 77, &[good(1), (2, test_fact_sig(4, 5, 77))]));
    }

    #[test]
    fn replicas_drop_bad_signatures_and_relay_good_ones() {
        use crate::session::{Msg, SceneKind, SimConfig, WorldSim};
        let members = [1, 2, 3];
        let sim = |me| WorldSim::new(7, me, &members, SceneKind::Empty, SimConfig::default(), KeyRing::test(me));
        let (mut a, mut b) = (sim(1), sim(2));
        let ack = a.author_ack();
        // A tampered signature, a signature by another member's key, and the same entry signed for
        // another world are all dropped and counted.
        let mut bad = ack.clone();
        bad.sig[0] ^= 1;
        let mut wrong_key = test_sign(7, Entry { member: 2, ..ack.entry.clone() });
        wrong_key.entry.member = 1;
        let other_world = test_sign(8, ack.entry.clone());
        for e in [bad, wrong_key, other_world] {
            assert!(b.insert(e).is_empty());
        }
        assert_eq!(b.stats.bad_sigs, 3);
        assert!(b.entry(ack.id()).is_none());
        // The genuine entry is stored, and relayed with its author's signature.
        b.insert(ack.clone());
        assert_eq!(b.stats.bad_sigs, 3);
        assert_eq!(b.signed_entry(ack.id()), Some(ack.clone()));
        assert_eq!(b.on_fetch(1, 1, 1), vec![Msg::Entry(ack.clone())]);
        // Signed bytes round-trip through the wire encoding.
        let m = Msg::Entry(ack);
        assert_eq!(crate::session::decode_msg(&crate::session::encode_msg(&m)), Some(m));
    }
}
