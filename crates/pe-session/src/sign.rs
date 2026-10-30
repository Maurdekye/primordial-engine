//! Entry signatures (P2-3 S3, design 10.8): every entry travels as a [`SignedEntry`], signed by its
//! author with Ed25519 over `"pe-entry" | world id | entry id`. A replica verifies the signature
//! before [`crate::session::WorldSim::insert`] stores the entry, and keeps it, so it can relay the
//! entry (fetch answers, equivocation proofs) with its author's signature.
//!
//! The entry id is the hash of the entry's canonical encoding, so the signature covers the whole
//! entry. The world id in the signed message keeps an entry from being replayed into another world.

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
    let mut m = Vec::with_capacity(ENTRY_DOMAIN.len() + 4 + 8);
    m.extend_from_slice(ENTRY_DOMAIN);
    m.extend_from_slice(&world.to_le_bytes());
    m.extend_from_slice(&id.to_le_bytes());
    m
}

/// This replica's signing key (none for an offline replay) and every member's public key.
#[derive(Clone)]
pub struct KeyRing {
    secret: Option<SigningKey>,
    public: BTreeMap<MemberId, VerifyingKey>,
    /// Test identities: any member's key is derived from its id (see [`KeyRing::test`]).
    test: bool,
}

impl KeyRing {
    pub fn new(secret: Option<SigningKey>, public: BTreeMap<MemberId, VerifyingKey>) -> Self {
        KeyRing { secret, public, test: false }
    }

    /// No keys: verifies nothing and signs nothing (the offline reference, which reads a final log
    /// that its replicas already verified).
    pub fn none() -> Self {
        KeyRing::new(None, BTreeMap::new())
    }

    /// Deterministic test identities for simulations: every member's key is derived from its id, so
    /// joiners need no key exchange and a scenario can sign as an adversarial member. Never for a
    /// real session: anyone can compute these keys.
    pub fn test(me: MemberId) -> Self {
        KeyRing { secret: Some(test_signing_key(me)), public: BTreeMap::new(), test: true }
    }

    pub fn add_public(&mut self, m: MemberId, k: VerifyingKey) {
        self.public.insert(m, k);
    }

    pub fn public_key(&mut self, m: MemberId) -> Option<VerifyingKey> {
        if self.test && !self.public.contains_key(&m) {
            self.public.insert(m, test_signing_key(m).verifying_key());
        }
        self.public.get(&m).copied()
    }

    /// Signs `e`, which must be this replica's own entry. Panics without a signing key.
    pub fn sign(&self, world: u32, e: Entry) -> SignedEntry {
        let key = self.secret.as_ref().expect("signing needs this replica's key");
        let sig = key.sign(&signing_message(world, e.id())).to_bytes();
        SignedEntry { entry: e, sig }
    }

    /// Whether `se` carries a valid signature by its author's key (strict verification: no
    /// malleable signatures, no small-order keys).
    pub fn verify(&mut self, world: u32, se: &SignedEntry) -> bool {
        let Some(k) = self.public_key(se.member) else { return false };
        k.verify_strict(&signing_message(world, se.id()), &Signature::from_bytes(&se.sig)).is_ok()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Payload;

    fn entry(member: MemberId, seq: u32) -> Entry {
        Entry { member, seq, prev: 0, stamp: 5, payload: Payload::Touch { obj: 1, impulse: [0.0; 3], grab: false }, carried: None }
    }

    #[test]
    fn sign_and_verify() {
        let ring = KeyRing::test(3);
        let se = ring.sign(7, entry(3, 1));
        let mut other = KeyRing::test(4);
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
