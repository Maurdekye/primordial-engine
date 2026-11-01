//! Entry signatures (P2-3 S3, design 10.8): every entry travels as a [`SignedEntry`], signed by its
//! author with Ed25519 over `"pe-entry" | world id | session id | entry id`. A replica verifies the signature
//! before [`crate::session::WorldSim::insert`] stores the entry, and keeps it, so it can relay the
//! entry (fetch answers, equivocation proofs) with its author's signature.
//!
//! The entry id is the hash of the entry's canonical encoding, so the signature covers the whole
//! entry. The world id and the session id (the session's genesis) in the signed message keep an
//! entry from being replayed into another world, or into a later session of the same world.

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

/// A session's identity: the hash of its genesis (members, keys, scene, config), so no two
/// sessions share one. Production derives it from the genesis document.
pub type SessionId = [u8; 32];

/// The session id of every test identity ring ([`KeyRing::test`]).
pub const TEST_SESSION: SessionId = *b"pe-test-session-0000000000000000";

/// The bytes an entry's signature covers.
pub fn signing_message(world: u32, session: &SessionId, id: EntryId) -> Vec<u8> {
    let mut m = Vec::with_capacity(ENTRY_DOMAIN.len() + 4 + 32 + 32);
    m.extend_from_slice(ENTRY_DOMAIN);
    m.extend_from_slice(&world.to_le_bytes());
    m.extend_from_slice(session);
    m.extend_from_slice(&id.0);
    m
}

/// The signing domain of facts: R14 attestations, precommits of the view change (S3c).
pub const FACT_DOMAIN: &[u8; 8] = b"pe-fact\0";

/// What members sign besides entries (review P23-2): a 256-bit BLAKE3 digest of a fact's typed
/// fields, keyed by its kind ([`FactWriter`]). The spike's 64-bit FNV facts inverted step by step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fact(pub [u8; 32]);

impl Fact {
    /// No fact (scenario fixtures for invalid attestations).
    pub const ZERO: Fact = Fact([0; 32]);
}

/// The kinds of fact, each hashed under its own BLAKE3 `derive_key` context, so a digest of one kind
/// is never a digest of another (the spike's view-change and commit facts shared tag 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactKind {
    /// A precommit's view-change value: epoch, round, value.
    VcValue,
    /// R14: a transfer committed in its source: src, transfer, global, dest, via.
    Committed,
    /// R14: an object departed its source (`fact_departed`).
    Departed,
    /// R14: the destination's verdict: dst, transfer, accepted.
    Verdict,
    /// A join attestation's relationship (signed in world 0: it holds in any world).
    Rel,
    /// Round 4's join signature (a fix-off fixture).
    Join,
    /// World ownership (12.5): a certificate's body, its id, which owner keys sign
    /// (`ownership::owner_message`).
    OwnerCert,
    /// World ownership: a session committed a certificate (`ownership::Attest`), signed by its members.
    OwnerCommit,
}

impl FactKind {
    pub fn context(self) -> &'static str {
        match self {
            FactKind::VcValue => "primordial-engine 2026 fact vc-value v1",
            FactKind::Committed => "primordial-engine 2026 fact committed v1",
            FactKind::Departed => "primordial-engine 2026 fact departed v1",
            FactKind::Verdict => "primordial-engine 2026 fact verdict v1",
            FactKind::Rel => "primordial-engine 2026 fact rel v1",
            FactKind::Join => "primordial-engine 2026 fact join v1",
            FactKind::OwnerCert => "primordial-engine 2026 fact owner-cert v1",
            FactKind::OwnerCommit => "primordial-engine 2026 fact owner-commit v1",
        }
    }
}

/// The canonical writer of a fact: fixed-width little-endian fields in a fixed order, a u32 length
/// before anything variable-length, under the kind's context.
pub struct FactWriter(blake3::Hasher);

impl FactWriter {
    pub fn new(kind: FactKind) -> Self {
        FactWriter(blake3::Hasher::new_derive_key(kind.context()))
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.update(&v.to_le_bytes());
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.0.update(&v.to_le_bytes());
        self
    }
    pub fn bool(mut self, v: bool) -> Self {
        self.0.update(&[v as u8]);
        self
    }
    pub fn id(mut self, id: &EntryId) -> Self {
        self.0.update(&id.0);
        self
    }
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.0.update(&(b.len() as u32).to_le_bytes());
        self.0.update(b);
        self
    }
    pub fn finish(self) -> Fact {
        Fact(*self.0.finalize().as_bytes())
    }
}

/// A 64-bit BLAKE3 digest under `context` (the view-change value, the provenance chain hash): not
/// invertible like the spike's FNV, though still 64 bits wide against a birthday search.
pub fn hash64(context: &str, b: &[u8]) -> u64 {
    let h = blake3::Hasher::new_derive_key(context).update(b).finalize();
    u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap())
}

/// The bytes a fact signature covers: `"pe-fact\0" | world | session | fact`. The session is the
/// signing world's (review P23-2: a fact cannot be replayed into a later session of that world);
/// world 0, for attestations that hold in any world, has the all-zero session.
pub fn fact_message(world: u32, session: &SessionId, fact: Fact) -> [u8; 76] {
    let mut m = [0u8; 76];
    m[..8].copy_from_slice(FACT_DOMAIN);
    m[8..12].copy_from_slice(&world.to_le_bytes());
    m[12..44].copy_from_slice(session);
    m[44..].copy_from_slice(&fact.0);
    m
}

/// The signing domain of join requests (review VC4R-6).
pub const JOIN_DOMAIN: &[u8; 8] = b"pe-join\0";

/// The bytes a join request's signature covers: `"pe-join\0" | session | joiner | expires`. The
/// session id binds it to one session of one world, so it cannot be replayed into a later one.
pub fn join_message(session: &SessionId, joiner: MemberId, expires: u64) -> [u8; 52] {
    let mut m = [0u8; 52];
    m[..8].copy_from_slice(JOIN_DOMAIN);
    m[8..40].copy_from_slice(session);
    m[40..44].copy_from_slice(&joiner.to_le_bytes());
    m[44..].copy_from_slice(&expires.to_le_bytes());
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
    /// Each known world's session id, to verify its facts (review P23-2).
    sessions: BTreeMap<u32, SessionId>,
    /// Test identities: any member's key is derived from its id, and every world's session is
    /// [`TEST_SESSION`] (see [`KeyRing::test`]).
    test: bool,
    derived: RefCell<BTreeMap<MemberId, VerifyingKey>>,
}

impl PublicKeys {
    pub fn new(mut keys: BTreeMap<MemberId, VerifyingKey>) -> Self {
        keys.remove(&crate::session::LOGIC);
        PublicKeys { keys, sessions: BTreeMap::new(), test: false, derived: RefCell::default() }
    }

    /// Every member's test identity (see [`KeyRing::test`]).
    pub fn test() -> Self {
        PublicKeys { keys: BTreeMap::new(), sessions: BTreeMap::new(), test: true, derived: RefCell::default() }
    }

    /// Member id 0 (world logic) never has a key (review P23-10): it is refused here.
    pub fn add(&mut self, m: MemberId, k: VerifyingKey) {
        if m != crate::session::LOGIC {
            self.keys.insert(m, k);
        }
    }

    /// Registers `world`'s session id, so its facts verify here.
    pub fn add_world(&mut self, world: u32, session: SessionId) {
        self.sessions.insert(world, session);
    }

    /// The session whose facts `world`'s members sign: world 0 (any world) has the all-zero session;
    /// an unknown world has none, so none of its facts verify.
    pub fn session_of(&self, world: u32) -> Option<SessionId> {
        if world == 0 {
            return Some([0; 32]);
        }
        self.sessions.get(&world).copied().or(self.test.then_some(TEST_SESSION))
    }

    /// No key for member id 0 (world logic), even among test identities (review P23-10), so no entry
    /// from the wire can claim world logic's exemptions.
    pub fn get(&self, m: MemberId) -> Option<VerifyingKey> {
        if m == crate::session::LOGIC {
            return None;
        }
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

    /// Whether `se` carries a valid signature by its author, for `world`'s session `session`.
    pub fn entry_ok(&self, world: u32, session: &SessionId, se: &SignedEntry) -> bool {
        self.verify(se.member, &signing_message(world, session, se.id()), &se.sig)
    }

    /// Whether `sig` is `m`'s signature over `fact` in `world` (in that world's session).
    pub fn fact_ok(&self, m: MemberId, world: u32, fact: Fact, sig: &Sig) -> bool {
        self.session_of(world).is_some_and(|s| self.verify(m, &fact_message(world, &s, fact), sig))
    }

    /// Whether `sig` is joiner `j`'s request to join session `session` until `expires`.
    pub fn join_ok(&self, j: MemberId, session: &SessionId, expires: u64, sig: &Sig) -> bool {
        self.verify(j, &join_message(session, j, expires), sig)
    }
}

/// This replica's signing key (none for an offline replay), every member's public key, and the
/// session the replica's entries belong to.
#[derive(Clone)]
pub struct KeyRing {
    secret: Option<SigningKey>,
    pub public: PublicKeys,
    pub session: SessionId,
}

impl KeyRing {
    pub fn new(secret: Option<SigningKey>, public: PublicKeys, session: SessionId) -> Self {
        KeyRing { secret, public, session }
    }

    /// No keys: verifies nothing and signs nothing (the offline reference, which reads a final log
    /// that its replicas already verified).
    pub fn none() -> Self {
        KeyRing::new(None, PublicKeys::default(), [0; 32])
    }

    /// Deterministic test identities for simulations: every member's key is derived from its id, so
    /// joiners need no key exchange and a scenario can sign as an adversarial member. Never for a
    /// real session: anyone can compute these keys.
    pub fn test(me: MemberId) -> Self {
        KeyRing { secret: Some(test_signing_key(me)), public: PublicKeys::test(), session: TEST_SESSION }
    }

    fn key(&self) -> &SigningKey {
        self.secret.as_ref().expect("signing needs this replica's key")
    }

    /// Signs `e`, which must be this replica's own entry. Panics without a signing key.
    pub fn sign(&self, world: u32, e: Entry) -> SignedEntry {
        let sig = self.key().sign(&signing_message(world, &self.session, e.id())).to_bytes();
        SignedEntry { entry: e, sig }
    }

    /// Signs `fact` in `world` as this replica's member. Panics without a signing key, or for a world
    /// whose session is unknown.
    pub fn sign_fact(&self, world: u32, fact: Fact) -> Sig {
        let s = self.public.session_of(world).expect("signing a fact of a world with a known session");
        self.key().sign(&fact_message(world, &s, fact)).to_bytes()
    }

    /// This replica's request to join its session until `expires` (review VC4R-6).
    pub fn sign_join(&self, me: MemberId, expires: u64) -> Sig {
        self.key().sign(&join_message(&self.session, me, expires)).to_bytes()
    }

    pub fn verify(&self, world: u32, se: &SignedEntry) -> bool {
        self.public.entry_ok(world, &self.session, se)
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

/// Signs `e` as its author's test identity, in [`TEST_SESSION`]: scenario fixtures for adversarial
/// members.
pub fn test_sign(world: u32, e: Entry) -> SignedEntry {
    let sig = test_signing_key(e.member).sign(&signing_message(world, &TEST_SESSION, e.id())).to_bytes();
    SignedEntry { entry: e, sig }
}

/// `m`'s test-identity signature over `fact` in `world` (in [`TEST_SESSION`], or world 0's): scenario
/// fixtures and unit tests.
pub fn test_fact_sig(m: MemberId, world: u32, fact: Fact) -> Sig {
    let s = PublicKeys::test().session_of(world).unwrap();
    test_signing_key(m).sign(&fact_message(world, &s, fact)).to_bytes()
}

/// `j`'s test-identity request to join session `session` until `expires`.
pub fn test_join_sig(j: MemberId, session: &SessionId, expires: u64) -> Sig {
    test_signing_key(j).sign(&join_message(session, j, expires)).to_bytes()
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
        // Nor does the same entry in another session of the same world.
        let mut later = KeyRing::test(4);
        later.session[0] ^= 1;
        assert!(!later.verify(7, &se));
        // Without the author's public key nothing verifies.
        assert!(!KeyRing::none().verify(7, &se));
        assert_eq!(test_sign(7, entry(3, 1)), se);
    }

    #[test]
    fn fact_signatures() {
        use crate::session::cert_ok;
        let f = |n: u64| FactWriter::new(FactKind::Verdict).u64(n).finish();
        let ring = KeyRing::test(2);
        let sig = ring.sign_fact(5, f(77));
        let keys = PublicKeys::test();
        assert_eq!(sig, test_fact_sig(2, 5, f(77)));
        assert!(keys.fact_ok(2, 5, f(77), &sig));
        // Another fact, world or member does not verify; no key, no verification.
        assert!(!keys.fact_ok(2, 5, f(78), &sig) && !keys.fact_ok(2, 6, f(77), &sig) && !keys.fact_ok(3, 5, f(77), &sig));
        assert!(!PublicKeys::default().fact_ok(2, 5, f(77), &sig));
        // A certificate needs a strict majority of valid signatures.
        let good = |m| (m, test_fact_sig(m, 5, f(77)));
        assert!(cert_ok(&keys, &[1, 2, 3], 5, f(77), &[good(1), good(2)]));
        assert!(!cert_ok(&keys, &[1, 2, 3], 5, f(77), &[good(1)]));
        assert!(!cert_ok(&keys, &[1, 2, 3], 5, f(77), &[good(1), (2, test_fact_sig(4, 5, f(77)))]));
    }

    /// Review P23-2: a fact is bound to its kind and to its world's session.
    #[test]
    fn facts_are_bound_to_their_kind_and_session() {
        let same = |k| FactWriter::new(k).u32(1).u64(2).u64(3).finish();
        assert_ne!(same(FactKind::VcValue), same(FactKind::Committed));
        assert_ne!(FactWriter::new(FactKind::Rel).bytes(&[1, 2]).finish(), FactWriter::new(FactKind::Rel).bytes(&[1]).bytes(&[2]).finish());
        let fact = same(FactKind::VcValue);
        // A world registered with its own session: a test signature (TEST_SESSION) does not verify there.
        let mut keys = PublicKeys::test();
        keys.add_world(5, [7; 32]);
        let sig = test_fact_sig(2, 5, fact);
        assert!(!keys.fact_ok(2, 5, fact, &sig));
        assert!(PublicKeys::test().fact_ok(2, 5, fact, &sig));
        // A world with no known session verifies nothing (production directory, test keys added).
        let mut prod = PublicKeys::new(BTreeMap::new());
        prod.add(2, test_signing_key(2).verifying_key());
        assert!(!prod.fact_ok(2, 5, fact, &sig));
        prod.add_world(5, TEST_SESSION);
        assert!(prod.fact_ok(2, 5, fact, &sig));
        // World 0 (attestations that hold in any world) has the all-zero session.
        assert!(prod.fact_ok(2, 0, fact, &test_fact_sig(2, 0, fact)));
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
