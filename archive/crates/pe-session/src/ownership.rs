//! World ownership as a chain of signed certificates (user ruling 2026-10-30 22:39Z, "Signed chain,
//! no blockchain"; design 12.5). Ownership is a lasting property of a world, separate from a session's
//! admins, and can be shared. It needs no registry or service of ours.
//!
//! * **Genesis.** Signed by every creator. The world id is the hash of the genesis certificate (in
//!   production, of the genesis document that contains it).
//! * **Certificates.** Each names its parent and is *valid* if its signatures satisfy the rule of the
//!   owner set at the parent: a function of its causal past alone. Its id is its body hash, without
//!   the signatures: copies with different signatures are one certificate, whose signatures that
//!   matter a book merges until a session attests it; the attestation carries the committed
//!   signatures, and a book counts only those it verifies (OWNR-4, OWNS-5, OWNX-1).
//! * **Session attestations.** A session that commits a certificate records the world frame it did so
//!   at; exported, that is an [`Attest`], signed by a strict majority of the session's members (P2-4's
//!   epoch certificates), so anyone can check it offline. A certificate **counts** only once some
//!   session attested it: one signed on an old copy and never committed changes nothing (round 2's
//!   mitigation for the residual of forks). A session commits only certificates built on its current
//!   head, so within one session the chain is linear.
//! * **Consent only** (user ruling 2026-10-31 08:31Z, OT = (a)): no owner is removed against its will
//!   under rule A, and nothing depends on time. A recovery key is a plain backup of its owner's key: its
//!   signature counts as the owner's. Bound session lineages (trusted attestations, and with them
//!   removal of a vanished owner) are a later package.
//! * **Rule A** (O1): adding needs every owner; anyone may leave, except the last owner; nobody is
//!   removed. Rules B (any owner adds; no removal) and C (k of n, instant removal) are kept.
//! * **The owner set** ([`Book::view`]) comes from an effective pass over the certificates held, in
//!   causal order (OWNR-1): a certificate applies only if it counts at its parent's *effective* state;
//!   otherwise its branch inherits the parent's state. After a fork the heads join as an
//!   observed-remove set (O4): a Leave or removal sticks unless the same history adds the person back.
//!   A change on a branch counts only if owners that counting changes on concurrent branches added
//!   since it split off approve it too (OWN-3). Concurrent mutual removals void each other (OWNR-7).
//!   The rule after a fork is the newest by causality, the strictest among concurrent ones (OWNR-9).
//!
//! What remains [I]: keys that once held full authority can still add a co-owner by forking at that
//! state, if they also run a session on an old copy to get it attested. That is detected (an
//! equivocation proof, "Ownership disputed"). Preventing it needs bound session lineages (later) or a
//! global order, which the user rejected (D79).
//!
//! Signatures (the pe-session port, design 10.8): owner keys are Ed25519 keys and sign a certificate's
//! id, a [`FactKind::OwnerCert`] fact, under their own domain with no world or session
//! ([`owner_message`]): a certificate holds in every session of its world, and names its world. A session's attestation is a [`FactKind::OwnerCommit`] fact signed by a strict
//! majority of its members, checked through P2-4's epoch certificates ([`EpochCerts::cert_ok`]).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use crate::provenance::EpochCerts;
use crate::session::MemberId;
use crate::sign::{test_fact_sig, test_signing_key, Fact, FactKind, FactWriter, Sig};

/// A player's long-term identity key (design 10.3): an Ed25519 verifying key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub [u8; 32]);

impl std::fmt::Debug for Key {
    /// A test identity prints as `k<n>` ([`test_key`]), any other key as the start of its bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match test_key_name(*self) {
            Some(n) => write!(f, "k{n}"),
            None => write!(f, "key:{:02x}{:02x}{:02x}{:02x}", self.0[0], self.0[1], self.0[2], self.0[3]),
        }
    }
}

/// A certificate's id: the [`FactKind::OwnerCert`] digest of its body, everything but the signatures.
pub type CertId = Fact;
/// A session lineage: the session that committed a certificate (spike: its world id).
pub type Lineage = u32;

/// Attested certificates one book holds at most (OWNR-6). Unattested ones are bounded per key and per
/// certificate instead (`UNATTESTED_PER_KEY`, `INHERITED_PER_CERT`). A co-owner with a session of its
/// own can fill it with attested solo siblings, each judged on its own history (OWNX-2, D87). Ownership
/// changes are rare; each insert re-runs the effective pass, O(N log N). Production compacts a long
/// linear history into a checkpoint signed by the owners [I].
pub const MAX_CERTS: usize = 1024;

/// Review OWNY-3: the most bytes a certificate's encoding holds (a genesis of about 500 owners, each
/// signing); [`Cert::decode`] refuses more.
pub const MAX_CERT_BYTES: usize = 65_536;
/// The solo changes (not signed by every owner at their parent) a world's history may hold, split into
/// per-owner shares (OWNU-6, OWNV-3, OWNV-4, OWNW-3). The genesis owners split it evenly. A newcomer's
/// share comes out of the remaining shares of the owners who signed its addition, and a leaver's
/// remaining share passes to the remaining owners (OWNX-4), so along one history the shares never add
/// up to more than this. An owner's remaining share only ever shrinks through certificates it signed
/// itself (invariant 9), except that a removal under rule C passes the target's to its removers
/// (OWNX-5). Across copies that each add an owner and then merge, shares (joined by minimum) can add
/// up to more, by at most one gift per copy (OWNX-3). Leave and clearing one's own recovery key are
/// exempt, but each is bounded by the additions and namings before it, so solo changes and their
/// exemptions take at most half the book along one history; joint changes keep the rest. Merges are
/// exempt too (any owner of the join signs one; honest sessions merge all heads, OWNW-2).
pub const SOLO_CERTS: usize = MAX_CERTS / 4;

/// Verified (key, certificate, signature) triples and test identities, per thread. Verification is a
/// pure function, so the memo changes no outcome; it spares the passes that re-read signer sets
/// (`Cert::signers`) an Ed25519 check each time.
const SIG_MEMO_MAX: usize = 1 << 18;
type TestKeys = (HashMap<u32, Key>, HashMap<Key, u32>);
thread_local! {
    static SIG_MEMO: RefCell<HashMap<(Key, CertId, Sig), bool>> = RefCell::new(HashMap::new());
    static TEST_KEYS: RefCell<TestKeys> = RefCell::new(Default::default());
}

/// The signing domain of ownership certificates: no other signed message starts with these bytes.
pub const OWNER_DOMAIN: &[u8; 8] = b"pe-own\0\0";

/// The bytes an owner key signs: `"pe-own\0\0" | certificate id`. No world or session: the id covers
/// the certificate's world (its genesis id), and it must verify in every session of that world.
pub fn owner_message(id: CertId) -> [u8; 40] {
    let mut m = [0u8; 40];
    m[..8].copy_from_slice(OWNER_DOMAIN);
    m[8..].copy_from_slice(&id.0);
    m
}

/// Whether `sig` is `key`'s signature over certificate `id` (strict Ed25519 over [`owner_message`]).
pub fn key_ok(key: Key, id: CertId, sig: &Sig) -> bool {
    if let Some(ok) = SIG_MEMO.with(|m| m.borrow().get(&(key, id, *sig)).copied()) {
        return ok;
    }
    let ok = VerifyingKey::from_bytes(&key.0).is_ok_and(|k| k.verify_strict(&owner_message(id), &Signature::from_bytes(sig)).is_ok());
    SIG_MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= SIG_MEMO_MAX {
            m.clear();
        }
        m.insert((key, id, *sig), ok);
    });
    ok
}

/// `sk`'s signature over certificate `id`.
pub fn sign_cert(sk: &SigningKey, id: CertId) -> Sig {
    sk.sign(&owner_message(id)).to_bytes()
}

/// Test identity `n`'s ownership key: the verifying key of [`test_signing_key`]`(n)`. Scenarios and
/// tests name keys by number. Never for a real world: anyone can compute these keys.
pub fn test_key(n: u32) -> Key {
    if let Some(k) = TEST_KEYS.with(|t| t.borrow().0.get(&n).copied()) {
        return k;
    }
    let k = Key(test_signing_key(n).verifying_key().to_bytes());
    TEST_KEYS.with(|t| {
        let mut t = t.borrow_mut();
        t.0.insert(n, k);
        t.1.insert(k, n);
    });
    k
}

/// The number of a test identity's key this thread has derived ([`test_key`]).
pub fn test_key_name(k: Key) -> Option<u32> {
    TEST_KEYS.with(|t| t.borrow().1.get(&k).copied())
}

/// Who must approve an `Add` (and, for removals, who among the other owners). Ordered by strictness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum SignRule {
    /// B: any one owner adds; nobody can remove another owner.
    Any,
    /// C: `k` of the owners (all of them if fewer); removal is instant.
    Threshold(u8),
    /// A (ruled): every owner.
    #[default]
    All,
}

impl SignRule {
    fn code(self) -> u32 {
        match self {
            SignRule::Any => 1,
            SignRule::Threshold(k) => 0x100 | k as u32,
            SignRule::All => 3,
        }
    }
    fn from_code(x: u32) -> Option<SignRule> {
        match x {
            1 => Some(SignRule::Any),
            3 => Some(SignRule::All),
            x if x >> 8 == 1 => Some(SignRule::Threshold(x as u8)),
            _ => None,
        }
    }
    /// `signers` meet this rule over the owners in `of`.
    fn met(self, signers: &BTreeSet<Key>, of: &BTreeSet<Key>) -> bool {
        let n = signers.intersection(of).count();
        !of.is_empty()
            && match self {
                SignRule::Any => n >= 1,
                SignRule::Threshold(k) => n >= (k as usize).clamp(1, of.len()),
                SignRule::All => n == of.len(),
            }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// The world's first certificate. `nonce` keeps two worlds made by the same people apart (256 bits,
    /// like a session nonce, against a birthday collision). `owners` is sorted, without repeats.
    Genesis { owners: Vec<Key>, rule: SignRule, nonce: [u8; 32] },
    Add { key: Key },
    /// An owner gives up ownership; signed by that owner's own key.
    Leave { key: Key },
    /// Rule C only: `k` of the other owners remove `key` at once.
    Remove { key: Key },
    /// Every owner signs.
    SetRule { rule: SignRule },
    /// `owner` (its own key, or its recovery key acting for it) names, replaces or clears its recovery
    /// key: a plain backup of the owner's key (OT = (a)).
    SetRecovery { owner: Key, recovery: Option<Key> },
    /// Joins concurrent certificates: the chain is linear again. Its owner set is forced (the join).
    Merge,
}

impl Change {
    /// The change's canonical bytes: a code, then its fields, fixed-width little-endian, a u32 count
    /// before a list.
    fn encode(&self, v: &mut Vec<u8>) {
        match self {
            Change::Genesis { owners, rule, nonce } => {
                v.push(1);
                v.extend(rule.code().to_le_bytes());
                v.extend(nonce);
                v.extend((owners.len() as u32).to_le_bytes());
                owners.iter().for_each(|k| v.extend(k.0));
            }
            Change::Add { key } => key_field(v, 2, key),
            Change::Leave { key } => key_field(v, 3, key),
            Change::Remove { key } => key_field(v, 4, key),
            Change::SetRule { rule } => {
                v.push(5);
                v.extend(rule.code().to_le_bytes());
            }
            Change::SetRecovery { owner, recovery } => {
                key_field(v, 6, owner);
                match recovery {
                    None => v.push(0),
                    Some(r) => key_field(v, 1, r),
                }
            }
            // Codes 7, 8, 10 and 11 were the window and recovery changes, removed under OT = (a).
            Change::Merge => v.push(9),
        }
    }

    fn decode(r: &mut Rd) -> Option<Change> {
        Some(match r.u8()? {
            1 => {
                let (rule, nonce, n) = (SignRule::from_code(r.u32()?)?, r.b32()?, r.u32()? as usize);
                let owners = (0..n.min(MAX_CERT_BYTES / 32)).map(|_| r.key()).collect::<Option<Vec<_>>>()?;
                if owners.len() != n {
                    return None;
                }
                Change::Genesis { owners, rule, nonce }
            }
            2 => Change::Add { key: r.key()? },
            3 => Change::Leave { key: r.key()? },
            4 => Change::Remove { key: r.key()? },
            5 => Change::SetRule { rule: SignRule::from_code(r.u32()?)? },
            6 => {
                let owner = r.key()?;
                let recovery = match r.u8()? {
                    0 => None,
                    1 => Some(r.key()?),
                    _ => return None,
                };
                Change::SetRecovery { owner, recovery }
            }
            9 => Change::Merge,
            _ => return None,
        })
    }

    /// The key a removal takes out.
    fn removes(&self) -> Option<Key> {
        match self {
            Change::Remove { key } => Some(*key),
            _ => None,
        }
    }
}

/// One ownership certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cert {
    /// The world id (the genesis certificate's id); [`Fact::ZERO`] in the genesis certificate itself.
    pub world: CertId,
    /// 0 for genesis; one more than the highest parent's.
    pub seq: u32,
    /// Parents: none for genesis, several (concurrent) for a merge, one otherwise.
    pub prev: Vec<CertId>,
    pub change: Change,
    pub sigs: Vec<(Key, Sig)>,
}

impl Cert {
    /// Everything but the signatures, in its one encoding: world 32 B, seq u32, the parents (u32
    /// count, 32 B each), the change.
    fn body(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(48 + 32 * self.prev.len());
        v.extend(self.world.0);
        v.extend(self.seq.to_le_bytes());
        v.extend((self.prev.len() as u32).to_le_bytes());
        self.prev.iter().for_each(|p| v.extend(p.0));
        self.change.encode(&mut v);
        v
    }

    /// The certificate's id, which each key signs: the [`FactKind::OwnerCert`] digest of its body.
    /// Copies with different signatures are one certificate; a book merges their signatures that
    /// matter until a session attests it (OWNR-4, OWNS-5).
    pub fn id(&self) -> CertId {
        FactWriter::new(FactKind::OwnerCert).bytes(&self.body()).finish()
    }

    /// Signed by these keys' holders.
    pub fn signed_by(mut self, sks: &[&SigningKey]) -> Cert {
        let id = self.id();
        self.sigs.extend(sks.iter().map(|sk| (Key(sk.verifying_key().to_bytes()), sign_cert(sk, id))));
        self
    }

    /// Signed by test identities `ns` ([`test_key`]).
    pub fn signed_test(mut self, ns: &[u32]) -> Cert {
        let id = self.id();
        self.sigs.extend(ns.iter().map(|&n| (test_key(n), sign_cert(&test_signing_key(n), id))));
        self
    }

    /// The keys with a valid signature on this certificate.
    pub fn signers(&self) -> BTreeSet<Key> {
        let id = self.id();
        self.sigs.iter().filter(|(k, s)| key_ok(*k, id, s)).map(|&(k, _)| k).collect()
    }

    /// An unsigned genesis certificate: every creator signs it ([`Cert::signed_by`]).
    pub fn genesis(owners: &[Key], rule: SignRule, nonce: [u8; 32]) -> Cert {
        let mut o = owners.to_vec();
        o.sort_unstable();
        o.dedup();
        Cert { world: Fact::ZERO, seq: 0, prev: vec![], change: Change::Genesis { owners: o, rule, nonce }, sigs: vec![] }
    }

    /// The genesis of test identities `ns`, signed by each; `nonce` fills the nonce's first 8 bytes.
    pub fn genesis_test(ns: &[u32], rule: SignRule, nonce: u64) -> Cert {
        let keys: Vec<Key> = ns.iter().map(|&n| test_key(n)).collect();
        let mut n = [0u8; 32];
        n[..8].copy_from_slice(&nonce.to_le_bytes());
        Cert::genesis(&keys, rule, n).signed_test(ns)
    }

    /// For reports: every field, signatures included.
    pub fn digest(&self) -> u64 {
        let mut h = Digest::new();
        h.fact(self.id()).u64(self.sigs.len() as u64);
        self.sigs.iter().for_each(|(k, s)| {
            h.key(*k).bytes(s);
        });
        h.finish()
    }

    /// Wire form (session log entries): the body, then the signatures (u32 count; key 32 B,
    /// signature 64 B each).
    pub fn encode(&self) -> Vec<u8> {
        let mut v = self.body();
        v.extend((self.sigs.len() as u32).to_le_bytes());
        for (k, s) in &self.sigs {
            v.extend(k.0);
            v.extend(s);
        }
        v
    }

    /// The inverse of [`Cert::encode`]. Refuses more than [`MAX_CERT_BYTES`] and trailing bytes, so
    /// each certificate has one encoding (OWN-11).
    pub fn decode(b: &[u8]) -> Option<Cert> {
        if b.len() > MAX_CERT_BYTES {
            return None;
        }
        let mut r = Rd(b);
        let (world, seq, np) = (Fact(r.b32()?), r.u32()?, r.u32()? as usize);
        let prev = (0..np.min(b.len() / 32)).map(|_| r.b32().map(Fact)).collect::<Option<Vec<_>>>()?;
        if prev.len() != np {
            return None;
        }
        let change = Change::decode(&mut r)?;
        let ns = r.u32()? as usize;
        let sigs = (0..ns.min(b.len() / 96)).map(|_| Some((r.key()?, r.sig()?))).collect::<Option<Vec<_>>>()?;
        if sigs.len() != ns || !r.0.is_empty() {
            return None;
        }
        Some(Cert { world, seq, prev, change, sigs })
    }
}

fn key_field(v: &mut Vec<u8>, code: u8, k: &Key) {
    v.push(code);
    v.extend(k.0);
}

/// A reader over a certificate's bytes.
struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn b32(&mut self) -> Option<[u8; 32]> {
        self.take(32)?.try_into().ok()
    }
    fn key(&mut self) -> Option<Key> {
        self.b32().map(Key)
    }
    fn sig(&mut self) -> Option<Sig> {
        self.take(64)?.try_into().ok()
    }
}

/// A 64-bit BLAKE3 digest for reports and the book's comparison digest (not a signed fact).
struct Digest(blake3::Hasher);

impl Digest {
    fn new() -> Self {
        Digest(blake3::Hasher::new_derive_key("primordial-engine 2026 ownership digest v1"))
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.update(&v.to_le_bytes());
        self
    }
    fn key(&mut self, k: Key) -> &mut Self {
        self.0.update(&k.0);
        self
    }
    fn fact(&mut self, f: Fact) -> &mut Self {
        self.0.update(&f.0);
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.u64(b.len() as u64);
        self.0.update(b);
        self
    }
    fn finish(&self) -> u64 {
        u64::from_le_bytes(self.0.finalize().as_bytes()[..8].try_into().unwrap())
    }
}

/// "Session `lineage` (in membership epoch `epoch`) committed certificate `cert` of world `world`,
/// signed by `signers`, at frame `frame` of that session", signed by a strict majority of the
/// session's members. Production: P2-4's epoch certificates over a session id [I]. Who may be a
/// trusted lineage is the open trust-root question (D87); the spike trusts any valid attestation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attest {
    pub cert: CertId,
    pub lineage: Lineage,
    pub epoch: u32,
    pub frame: u64,
    /// The certificate's key signatures the session committed (frozen from then on). A book counts
    /// only the signers whose signature it verifies (OWNX-1): a session can't name a signer.
    pub keysigs: Vec<(Key, Sig)>,
    pub sigs: Vec<(MemberId, Sig)>,
}

impl Attest {
    /// The [`FactKind::OwnerCommit`] fact: world 32 B, certificate 32 B, lineage, epoch, frame, then
    /// the committed key signatures (u32 count; key and signature each length-prefixed), sorted by key.
    pub fn fact(world: CertId, cert: CertId, lineage: Lineage, epoch: u32, frame: u64, keysigs: &[(Key, Sig)]) -> Fact {
        let mut w = FactWriter::new(FactKind::OwnerCommit).bytes(&world.0).bytes(&cert.0).u32(lineage).u32(epoch).u64(frame).u32(keysigs.len() as u32);
        for (k, s) in keysigs {
            w = w.bytes(&k.0).bytes(s);
        }
        w.finish()
    }
    /// Signed by every member given, as test identities (production: the session's epoch certificate,
    /// P2-4 [I]). `keysigs` are the certificate's signatures as committed, sorted by key.
    pub fn make(world: CertId, cert: CertId, lineage: Lineage, frame: u64, keysigs: &[(Key, Sig)], members: &[MemberId]) -> Attest {
        let mut keysigs = keysigs.to_vec();
        keysigs.sort_unstable();
        keysigs.dedup_by_key(|x| x.0);
        let f = Attest::fact(world, cert, lineage, 0, frame, &keysigs);
        Attest { cert, lineage, epoch: 0, frame, keysigs, sigs: members.iter().map(|&m| (m, test_fact_sig(m, lineage, f))).collect() }
    }
    pub fn verify(&self, world: CertId, sessions: &dyn EpochCerts) -> bool {
        let f = Attest::fact(world, self.cert, self.lineage, self.epoch, self.frame, &self.keysigs);
        sessions.cert_ok(self.lineage, self.epoch, f, &self.sigs)
    }
    /// The signers whose signatures on the certificate check: the committed signer set (OWNX-1).
    pub fn signers(&self) -> BTreeSet<Key> {
        self.keysigs.iter().filter(|(k, s)| key_ok(*k, self.cert, s)).map(|&(k, _)| k).collect()
    }
}

/// Why a certificate is refused, or doesn't count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnError {
    WrongWorld,
    /// Not a well-formed genesis, or not signed by every creator.
    BadGenesis,
    /// A parent is not held. Nothing is stored (OWN-10); deliver the parents first.
    MissingParent,
    /// `seq` does not follow its parents, or `prev` is wrong for the change (a merge of parents
    /// that are not pairwise concurrent, OWN-1).
    BadLink,
    /// The signatures do not satisfy the rule of the owner set at the parent (for removals,
    /// including owners added on concurrent branches, OWN-3).
    Signers,
    AlreadyOwner,
    NotOwner,
    /// The last owner can't leave or be removed.
    LastOwner,
    /// Not under this rule (removal outside rule C), or no rule.
    NotAllowed,
    /// No session has attested the certificate, and nobody who matters at a head signed it.
    Unattested,
    /// A session commits only certificates built on the head of what it committed itself.
    Stale,
    /// Two concurrent counting removals each remove a signer of the other (OWNR-7).
    Conflict,
    /// The book holds [`MAX_CERTS`] certificates.
    Full,
    /// Every key (or certificate) the certificate is charged to already carries its limit of
    /// unattested certificates (OWNS-5, OWNU-4). Send it again with its attestation.
    Quota,
    /// Every owner who signed this solo change used its share of [`SOLO_CERTS`] (OWNU-6, OWNW-3).
    Budget,
}

/// Unattested certificates (held or kept) charged to one key at most (OWNS-5, OWNU-2, OWNU-4,
/// OWNV-1). A certificate is charged to its own signers that are owners or recovery keys at a current
/// head; it gets in if one of them has room, so a signature someone else merges in never costs an
/// honest signer its place (OWNW-1). Keys are counted one by one, never in groups, so nobody's flood
/// crowds out another key.
pub const UNATTESTED_PER_KEY: usize = 64;
/// Unattested certificates signed by nobody who matters at a head (a key invited or minted by an
/// unattested certificate) that one unattested certificate may carry (OWNT-2, OWNV-1).
pub const INHERITED_PER_CERT: usize = 16;

/// Who an unattested certificate is charged to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Charge {
    /// Its own signers that matter at a current head.
    Keys(BTreeSet<Key>),
    /// The unattested certificates it hangs from.
    Roots(BTreeSet<CertId>),
}

/// The owner set and its rules after some certificate (or the join of several heads).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Owners {
    pub owners: BTreeSet<Key>,
    pub rule: SignRule,
    /// Removal is off: after joining copies whose rules differ in it (one under rule B), until the
    /// owners set the rule again (OWNT-7).
    pub no_removal: bool,
    /// The join would have left nobody (copies whose last owners left or removed each other), so
    /// every head's owners stayed.
    pub nobody_left: bool,
    /// Observed-remove bookkeeping: each owner's live add tags (the certificates that added them).
    /// The tags a removal has seen are derived when heads join, from the removals in their past.
    tags: BTreeMap<Key, BTreeSet<CertId>>,
    /// Each owner's recovery keys (plain backups of its key), with the certificate that named each.
    pub recovery: BTreeMap<Key, BTreeMap<Key, CertId>>,
    /// Solo changes each owner signed (itself or through a recovery key) along this history
    /// (OWNU-6). Joined by maximum.
    pub solo: BTreeMap<Key, u32>,
    /// Each owner's share of [`SOLO_CERTS`]: set when it became an owner, lowered only by the gifts it
    /// signed (OWNV-4, OWNW-3). Joined by minimum.
    pub share: BTreeMap<Key, u32>,
    pub seq: u32,
}

impl Owners {
    fn others(&self, key: Key) -> BTreeSet<Key> {
        self.owners.iter().copied().filter(|&o| o != key).collect()
    }

    fn add_owner(&mut self, key: Key, tag: CertId) {
        self.owners.insert(key);
        self.tags.insert(key, [tag].into());
    }

    fn drop_owner(&mut self, key: Key) {
        self.owners.remove(&key);
        self.tags.remove(&key);
        self.recovery.remove(&key);
    }

    /// `from`'s remaining share (its share minus its solo count) passes to `to` in equal parts, the
    /// remainder to the lowest keys, so the shares stay conserved (OWNX-4).
    fn pass_share(&mut self, from: Key, to: &[Key]) {
        let rem = self.share.get(&from).copied().unwrap_or(0).saturating_sub(self.solo.get(&from).copied().unwrap_or(0));
        if rem == 0 || to.is_empty() {
            return;
        }
        let n = to.len() as u32;
        for (i, k) in to.iter().enumerate() {
            *self.share.entry(*k).or_default() += rem / n + u32::from((i as u32) < rem % n);
        }
        *self.share.entry(from).or_default() -= rem;
    }

    /// The keys acting in a signer set: the signers, plus every owner whose recovery key signed (a
    /// recovery key is a plain backup of its owner's key, OT = (a)).
    fn acting(&self, signers: &BTreeSet<Key>) -> BTreeSet<Key> {
        let mut a = signers.clone();
        a.extend(self.owners.iter().copied().filter(|o| self.recovery.get(o).is_some_and(|r| r.keys().any(|k| signers.contains(k)))));
        a
    }

    /// Keys whose signatures matter on a certificate built on this state: owners and recovery keys.
    fn relevant(&self, k: Key) -> bool {
        self.owners.contains(&k) || self.recovery.values().any(|r| r.contains_key(&k))
    }

    pub fn digest(&self) -> u64 {
        let mut h = Digest::new();
        h.u64(self.rule.code() as u64).u64(self.no_removal as u64 | (self.nobody_left as u64) << 1).u64(self.seq as u64).u64(self.owners.len() as u64);
        h.u64(self.tags.len() as u64);
        for (k, t) in &self.tags {
            h.key(*k).u64(t.len() as u64);
            t.iter().for_each(|c| {
                h.fact(*c);
            });
        }
        h.u64(self.recovery.len() as u64);
        for (o, r) in &self.recovery {
            h.key(*o).u64(r.len() as u64);
            r.iter().for_each(|(k, c)| {
                h.key(*k).fact(*c);
            });
        }
        h.u64(self.solo.len() as u64);
        self.solo.iter().for_each(|(o, n)| {
            h.key(*o).u64(*n as u64);
        });
        h.u64(self.share.len() as u64);
        self.share.iter().for_each(|(o, n)| {
            h.key(*o).u64(*n as u64);
        });
        h.finish()
    }
}

/// Per-key join over several maps: counts by maximum, shares by minimum (over the maps that have the
/// key), so a join never gives an owner more room than either copy did.
fn join_by<'a>(ms: impl Iterator<Item = &'a BTreeMap<Key, u32>>, f: fn(u32, u32) -> u32) -> BTreeMap<Key, u32> {
    let mut out: BTreeMap<Key, u32> = BTreeMap::new();
    for m in ms {
        for (&k, &n) in m {
            out.entry(k).and_modify(|e| *e = f(*e, n)).or_insert(n);
        }
    }
    out
}

/// A book's current answer: the heads, the owner set the world acts on, and the held certificates
/// that don't count, with why.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct View {
    /// Counting certificates with no counting child.
    pub heads: Vec<CertId>,
    /// Two counting certificates ever shared a parent: "Ownership disputed". It comes from the fork
    /// history, so no merge clears it (OWNS-8); the fork proofs say who forked.
    pub disputed: bool,
    pub owners: Owners,
    pub refused: Vec<(CertId, OwnError)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct Inner {
    /// Every held certificate by id (its body hash), with the signatures of every copy seen that
    /// matter (owners' and recovery keys' at its parent), merged until a session attests it.
    certs: BTreeMap<CertId, Cert>,
    /// Each certificate's causal past, itself included, shared between clones (OWNT-10).
    anc: BTreeMap<CertId, Arc<BTreeSet<CertId>>>,
    /// Attested commits: certificate -> lineage -> (frame, the signer set committed).
    commits: BTreeMap<CertId, BTreeMap<Lineage, (u64, BTreeSet<Key>)>>,
    /// Certificates whose parents are held but that aren't valid yet (under-signed, or a state that
    /// may change), kept to retry as copies with more signatures and attestations arrive (OWNT-5).
    /// Copies merge their signatures (OWNU-3). They count for nothing and aren't in the digest.
    parked: BTreeMap<CertId, Cert>,
    /// Unattested certificates, held or kept, and who each is charged to, derived again after each
    /// pass (OWNU-2, OWNU-4, OWNV-1).
    charged: BTreeMap<CertId, Charge>,
    /// The signed attestations imported from other sessions, the winning one per certificate and
    /// lineage, to forward as they are (OWNV-7).
    proofs: BTreeMap<(CertId, Lineage), Attest>,
    /// Owners and recovery keys at the current heads: the keys an unattested certificate is charged to.
    head_keys: BTreeSet<Key>,
    /// The counting certificates, as of the last pass.
    counted: BTreeSet<CertId>,
    /// Results of the last pass: signer sets, linear validity and state, effective states, the view,
    /// the digest.
    signers: BTreeMap<CertId, BTreeSet<Key>>,
    linear: BTreeMap<CertId, Owners>,
    invalid: BTreeSet<CertId>,
    eff: BTreeMap<CertId, Owners>,
    view: View,
    digest: u64,
}

/// What one verifier holds of a world's ownership. Cloning shares everything until the next change
/// (keyframes, OWNR-6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Book {
    pub world: CertId,
    pub genesis: CertId,
    inner: Arc<Inner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    Applied,
    Duplicate,
    /// Not valid yet (under-signed, say), kept to retry as more arrives (it counts for nothing).
    Parked,
}

/// How a certificate is admitted: charged to keys under their quota (unattested), already charged
/// (a parked one retried), or freely, because a session attests it or because only its validity is
/// being checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admit {
    Pool,
    Settled,
    Free,
}

impl Book {
    pub fn new(genesis: Cert) -> Result<Book, OwnError> {
        let Change::Genesis { owners, rule, .. } = &genesis.change else { return Err(OwnError::BadGenesis) };
        let set: BTreeSet<Key> = owners.iter().copied().collect();
        // The owners sorted without repeats: one encoding, so one id, per genesis.
        let sorted = owners.windows(2).all(|w| w[0] < w[1]);
        if genesis.world != Fact::ZERO || genesis.seq != 0 || !genesis.prev.is_empty() || set.is_empty() || !sorted {
            return Err(OwnError::BadGenesis);
        }
        if *rule == SignRule::Threshold(0) || !genesis.signers().is_superset(&set) {
            return Err(OwnError::BadGenesis);
        }
        let id = genesis.id();
        let mut g = genesis;
        g.sigs.retain(|(k, s)| set.contains(k) && key_ok(*k, id, s));
        let inner = Inner { anc: [(id, Arc::new([id].into()))].into(), certs: [(id, g)].into(), ..Default::default() };
        let mut b = Book { world: id, genesis: id, inner: Arc::new(inner) };
        b.recompute();
        Ok(b)
    }

    pub fn cert(&self, id: CertId) -> Option<&Cert> {
        self.inner.certs.get(&id)
    }

    pub fn len(&self) -> usize {
        self.inner.certs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.certs.is_empty()
    }

    fn attested_count(&self) -> usize {
        self.inner.commits.len()
    }

    /// Store `c` if its parents are held and it is valid against their state. Unattested, it is
    /// charged (OWNT-2, OWNU-4, OWNV-1) and counts for nothing until a session attests it. An orphan is
    /// refused, never stored (OWNU-2): deliver the parents first. Under-signed, it is kept for copies
    /// with more signatures, charged likewise; copies merge their signatures (OWNU-3), until a session
    /// attests it.
    pub fn insert(&mut self, c: Cert) -> Result<Insert, OwnError> {
        let r = self.admit(c)?;
        self.recompute();
        Ok(r)
    }

    /// Admission of an unattested certificate, keeping an under-signed one.
    fn admit(&mut self, c: Cert) -> Result<Insert, OwnError> {
        match self.insert_raw(c.clone(), Admit::Pool) {
            Ok(r) => Ok(r),
            Err(e) => self.park(c, e),
        }
    }

    /// Keep an under-signed certificate (every parent held and valid, a signature that matters there,
    /// charged like any unattested certificate) until copies with more signatures, or its attestation,
    /// arrive. Every other refusal is final, since no signature fixes it (OWNV-5).
    fn park(&mut self, mut c: Cert, e: OwnError) -> Result<Insert, OwnError> {
        if e != OwnError::Signers {
            return Err(e);
        }
        let Some(p) = self.parent_state(&c.prev) else { return Err(e) };
        let id = c.id();
        c.sigs.retain(|(k, s)| p.relevant(*k) && key_ok(*k, id, s));
        if let Some(held) = self.inner.parked.get(&id) {
            c.sigs.extend(held.sigs.iter().copied());
        }
        c.sigs.sort_unstable();
        c.sigs.dedup_by_key(|x| x.0);
        if !self.inner.charged.contains_key(&id) {
            let Some(ch) = self.charge_of(&c.prev, &c.signers(), &self.inner.charged) else { return Err(e) };
            if !self.has_room(&ch) {
                return Err(OwnError::Quota);
            }
            Arc::make_mut(&mut self.inner).charged.insert(id, ch);
        }
        Arc::make_mut(&mut self.inner).parked.insert(id, c.clone());
        // More signatures may make it valid now.
        if self.insert_raw(c, Admit::Settled).is_ok() {
            return Ok(Insert::Applied);
        }
        Ok(Insert::Parked)
    }

    /// Whom an unattested certificate on `prev`, with these signers (those that matter at its parent),
    /// is charged to (OWNV-1, OWNV-5): its own signers that are owners or recovery keys at a current
    /// head. If none, the unattested certificates it hangs from (whoever invited or minted its
    /// signers), each carrying at most [`INHERITED_PER_CERT`] such certificates. Otherwise nobody pays,
    /// and it isn't kept.
    fn charge_of(&self, prev: &[CertId], signers: &BTreeSet<Key>, charged: &BTreeMap<CertId, Charge>) -> Option<Charge> {
        if signers.is_empty() {
            return None;
        }
        let own: BTreeSet<Key> = signers.iter().copied().filter(|k| self.inner.head_keys.contains(k)).collect();
        if !own.is_empty() {
            return Some(Charge::Keys(own));
        }
        let roots: BTreeSet<CertId> = prev
            .iter()
            .flat_map(|p| match charged.get(p) {
                Some(Charge::Keys(_)) => vec![*p],
                Some(Charge::Roots(r)) => r.iter().copied().collect(),
                None => vec![],
            })
            .collect();
        (!roots.is_empty()).then_some(Charge::Roots(roots))
    }

    /// One of the keys (or certificates) charged has room (OWNW-1: a merged signature never costs an
    /// honest signer its place).
    fn has_room(&self, ch: &Charge) -> bool {
        let charged = self.inner.charged.values();
        match ch {
            Charge::Keys(ks) => ks.iter().any(|k| charged.clone().filter(|c| matches!(c, Charge::Keys(x) if x.contains(k))).count() < UNATTESTED_PER_KEY),
            Charge::Roots(rs) => rs.iter().any(|r| charged.clone().filter(|c| matches!(c, Charge::Roots(x) if x.contains(r))).count() < INHERITED_PER_CERT),
        }
    }

    /// Insert many, in any order (OWNS-9), keeping under-signed ones. Returns what was refused
    /// (OWNS-10: junk is skipped, not fatal).
    pub fn insert_all(&mut self, certs: &[Cert]) -> Vec<(CertId, OwnError)> {
        self.insert_attested(certs, &[], &crate::provenance::EpochBook::default())
    }

    /// As [`Book::insert_all`], with the sessions' attestations, in any order within the batch. The
    /// batch's attestations are matched to its own certificates (OWNU-1): an attested certificate
    /// gets in freely, whatever the batch's size. Attestations of certificates neither in the batch
    /// nor held are dropped: send them with their certificates.
    pub fn insert_attested(&mut self, certs: &[Cert], attests: &[Attest], sessions: &dyn EpochCerts) -> Vec<(CertId, OwnError)> {
        let mut by: BTreeMap<CertId, Vec<&Attest>> = BTreeMap::new();
        for a in attests.iter().filter(|a| a.verify(self.world, sessions)) {
            by.entry(a.cert).or_default().push(a);
        }
        let mut sorted: Vec<&Cert> = certs.iter().collect();
        sorted.sort_by_key(|c| (c.seq, c.id()));
        // OWNW-4: the attested certificates first, then a pass, so the unattested ones are judged
        // against the heads the batch made.
        let (attested, rest): (Vec<&Cert>, Vec<&Cert>) = sorted.into_iter().partition(|c| by.contains_key(&c.id()));
        let mut refused = Vec::new();
        for c in attested {
            let id = c.id();
            match self.insert_raw(c.clone(), Admit::Free) {
                Ok(_) => {
                    for a in &by[&id] {
                        self.record_raw(id, a.lineage, a.frame, a.signers(), Some(a));
                    }
                }
                Err(OwnError::MissingParent) if rest.iter().any(|r| c.prev.contains(&r.id())) => {}
                Err(e) => {
                    if let Err(e) = self.park(c.clone(), e) {
                        refused.push((id, e));
                    }
                }
            }
        }
        self.recompute();
        for c in rest {
            if let Err(e) = self.admit(c.clone()) {
                refused.push((c.id(), e));
            }
        }
        // Attested certificates that hang from unattested ones in the batch, and attestations of
        // certificates held (or kept) before it.
        let late: Vec<&Cert> = certs.iter().filter(|c| by.contains_key(&c.id()) && !self.inner.certs.contains_key(&c.id())).collect();
        for c in late {
            if let Err(e) = self.insert_raw(c.clone(), Admit::Free) {
                refused.push((c.id(), e));
            }
        }
        // Attestations of certificates held (or kept) before the batch.
        for (id, v) in &by {
            for a in v {
                self.attest_raw(*id, a.lineage, a.frame, a.signers(), Some(a));
            }
        }
        self.recompute();
        refused
    }

    /// Record an attestation of a held or kept certificate; anything else is dropped. False if the
    /// certificate isn't held afterwards.
    fn attest_raw(&mut self, id: CertId, lineage: Lineage, frame: u64, signers: BTreeSet<Key>, proof: Option<&Attest>) -> bool {
        if let Some(c) = self.inner.parked.get(&id).cloned() {
            let _ = self.insert_raw(c, Admit::Free);
        }
        if !self.inner.certs.contains_key(&id) {
            return false;
        }
        self.record_raw(id, lineage, frame, signers, proof);
        true
    }

    /// The local session committed `id` at its `frame` (its own record; exported as an [`Attest`]),
    /// with the certificate's signer set as it holds it now. A kept certificate gets in with it; one
    /// the book doesn't hold changes nothing (OWNU-7).
    pub fn record(&mut self, id: CertId, lineage: Lineage, frame: u64) {
        let Some(c) = self.inner.certs.get(&id).or_else(|| self.inner.parked.get(&id)) else { return };
        let signers = c.signers();
        if self.attest_raw(id, lineage, frame, signers, None) {
            self.recompute();
        }
    }

    /// One lineage's attestations of one certificate can differ (a session equivocating): the lowest
    /// frame wins, then the smallest signer set, whatever the order they arrive in (OWNT-6). The
    /// winner's signed bytes are kept, so the book can forward it (OWNV-7). An attested certificate is
    /// charged to nobody.
    fn record_raw(&mut self, id: CertId, lineage: Lineage, frame: u64, signers: BTreeSet<Key>, proof: Option<&Attest>) {
        if !self.inner.certs.contains_key(&id) {
            return;
        }
        let inner = Arc::make_mut(&mut self.inner);
        inner.charged.remove(&id);
        let key = |f: u64, s: &BTreeSet<Key>| (f, s.len(), s.iter().copied().collect::<Vec<_>>());
        let e = inner.commits.entry(id).or_default().entry(lineage);
        let (won, tie) = match &e {
            std::collections::btree_map::Entry::Vacant(_) => (true, false),
            std::collections::btree_map::Entry::Occupied(o) => {
                let (f0, s0) = o.get();
                (key(frame, &signers) < key(*f0, s0), key(frame, &signers) == key(*f0, s0))
            }
        };
        if won {
            *e.or_default() = (frame, signers);
            match proof {
                Some(a) => inner.proofs.insert((id, lineage), a.clone()),
                None => inner.proofs.remove(&(id, lineage)),
            };
        } else if tie && let Some(a) = proof {
            inner.proofs.entry((id, lineage)).or_insert_with(|| a.clone());
        }
    }

    /// Import another session's attestation, if its signatures check out and the certificate is held
    /// or kept; otherwise nothing changes (send the certificate first).
    pub fn attest(&mut self, a: &Attest, sessions: &dyn EpochCerts) -> bool {
        if !a.verify(self.world, sessions) || !self.attest_raw(a.cert, a.lineage, a.frame, a.signers(), Some(a)) {
            return false;
        }
        self.recompute();
        true
    }

    /// Every other session's attestation this book imported and holds (the winning one per
    /// certificate and lineage), with its signatures, to forward as they are (OWNV-7). A session's
    /// own commits are exported with [`Book::export`].
    pub fn attestations(&self) -> Vec<Attest> {
        self.inner.proofs.values().cloned().collect()
    }

    /// Tests: `c` delivered with session `lineage`'s attestation at `frame` (as `insert_attested`
    /// does with a verified one), on any parent.
    #[cfg(test)]
    fn insert_recorded(&mut self, c: Cert, lineage: Lineage, frame: u64) -> Result<Insert, OwnError> {
        let id = c.id();
        let r = self.insert_raw(c.clone(), Admit::Free).or_else(|e| self.park(c, e))?;
        self.attest_raw(id, lineage, frame, self.inner.certs.get(&id).map(Cert::signers).unwrap_or_default(), None);
        self.recompute();
        Ok(r)
    }

    /// The parents of `c` this book doesn't hold: what to ask a peer for after `MissingParent`
    /// (OWNV-7).
    pub fn missing_parents(&self, c: &Cert) -> Vec<CertId> {
        c.prev.iter().copied().filter(|p| !self.inner.certs.contains_key(p)).collect()
    }

    /// The attestations of every certificate session `lineage` committed in this book.
    pub fn export(&self, lineage: Lineage, members: &[MemberId]) -> Vec<Attest> {
        self.inner
            .commits
            .iter()
            .filter_map(|(c, m)| m.get(&lineage).map(|(f, s)| Attest::make(self.world, *c, lineage, *f, &self.committed_sigs(*c, lineage, s), members)))
            .collect()
    }

    /// The signatures of the committed signer set `s` of `id` in `lineage`: from the imported
    /// attestation, or (a local commit) from the certificate as held.
    fn committed_sigs(&self, id: CertId, lineage: Lineage, s: &BTreeSet<Key>) -> Vec<(Key, Sig)> {
        let from = self.inner.proofs.get(&(id, lineage)).map(|a| a.keysigs.clone()).unwrap_or_else(|| self.inner.certs[&id].sigs.clone());
        from.into_iter().filter(|(k, sig)| s.contains(k) && key_ok(*k, id, sig)).collect()
    }

    /// A session commits `c` at its `frame` on a head of the counting chain (any session may adopt
    /// it), or a merge of counting heads, and only if it then counts (OWNT-4). A held but uncommitted
    /// certificate gets committed. Otherwise nothing changes.
    pub fn commit_local(&mut self, c: Cert, lineage: Lineage, frame: u64) -> Result<Insert, OwnError> {
        let id = c.id();
        if self.inner.commits.get(&id).is_some_and(|m| m.contains_key(&lineage)) {
            return Ok(Insert::Duplicate);
        }
        let heads = &self.inner.view.heads;
        if !c.prev.is_empty() && c.prev.iter().all(|p| self.inner.certs.contains_key(p)) && !c.prev.iter().all(|p| heads.contains(p)) {
            return Err(OwnError::Stale);
        }
        // OWNW-2: an honest session's merge joins every counting head, never a subset.
        if c.change == Change::Merge && c.prev.len() != heads.len() {
            return Err(OwnError::Stale);
        }
        if self.attested_count() >= MAX_CERTS {
            return Err(OwnError::Full);
        }
        let mut b = self.clone();
        b.insert_raw(c, Admit::Free)?;
        let signers = b.inner.certs[&id].signers();
        b.record_raw(id, lineage, frame, signers, None);
        b.recompute();
        if let Some(&(_, e)) = b.inner.view.refused.iter().find(|(r, _)| *r == id) {
            return Err(e);
        }
        *self = b;
        Ok(Insert::Applied)
    }

    /// The state a certificate built on `prev` is judged against, as of the last pass.
    fn parent_state(&self, prev: &[CertId]) -> Option<Owners> {
        match prev {
            [] => None,
            [p] => (self.inner.linear.contains_key(p) && !self.inner.invalid.contains(p)).then(|| self.inner.linear[p].clone()),
            ps => ps.iter().all(|p| self.inner.linear.contains_key(p)).then(|| self.join(ps, &self.inner.linear, &|x| !self.inner.invalid.contains(&x))),
        }
    }

    fn insert_raw(&mut self, mut c: Cert, mode: Admit) -> Result<Insert, OwnError> {
        if c.world != self.world {
            return Err(OwnError::WrongWorld);
        }
        if c.prev.is_empty() {
            return Err(OwnError::BadGenesis);
        }
        if !c.prev.iter().all(|p| self.inner.certs.contains_key(p)) {
            return Err(OwnError::MissingParent);
        }
        let id = c.id();
        // A held certificate stays held, even if its parent has turned invalid since: a copy is a
        // duplicate (so an attestation that comes with it is still recorded).
        if self.inner.commits.contains_key(&id) || (self.inner.certs.contains_key(&id) && self.parent_state(&c.prev).is_none()) {
            return Ok(Insert::Duplicate);
        }
        let Some(p) = self.parent_state(&c.prev) else { return Err(OwnError::BadLink) };
        // Only signatures that can matter: owners' and recovery keys' at the parent (OWNS-5).
        c.sigs.retain(|(k, s)| p.relevant(*k) && key_ok(*k, id, s));
        if let Some(parked) = self.inner.parked.get(&id) {
            // OWNU-3: a copy of a parked certificate merges its signatures; it never replaces it.
            c.sigs.extend(parked.sigs.iter().copied().filter(|&(k, _)| p.relevant(k)));
        }
        c.sigs.sort_unstable();
        c.sigs.dedup_by_key(|x| x.0);
        if let Some(held) = self.inner.certs.get(&id) {
            if self.inner.commits.contains_key(&id) {
                return Ok(Insert::Duplicate);
            }
            let have = held.signers();
            let more: Vec<(Key, Sig)> = c.sigs.iter().copied().filter(|(k, _)| !have.contains(k)).collect();
            if !more.is_empty() {
                let inner = Arc::make_mut(&mut self.inner);
                let held = inner.certs.get_mut(&id).unwrap();
                held.sigs.extend(more);
                held.sigs.sort_unstable();
            }
            return Ok(Insert::Duplicate);
        }
        let signers = c.signers();
        let state = if c.change == Change::Merge {
            let mut ps = c.prev.clone();
            ps.sort_unstable();
            ps.dedup();
            // OWN-1: only certificates that really diverged can be joined.
            let concurrent = ps.iter().enumerate().all(|(i, a)| ps[i + 1..].iter().all(|b| self.concurrent(*a, *b)));
            if ps.len() < 2 || ps != c.prev || !concurrent || c.seq != p.seq + 1 {
                return Err(OwnError::BadLink);
            }
            if !SignRule::Any.met(&p.acting(&signers), &p.owners) {
                return Err(OwnError::Signers);
            }
            let mut m = p.clone();
            m.seq = c.seq;
            m
        } else {
            if c.prev.len() != 1 {
                return Err(OwnError::BadLink);
            }
            self.step(&p, &c, id, &signers)?
        };
        match mode {
            Admit::Pool => {
                // OWNT-2, OWNU-4, OWNV-1: charged to its own signers that matter at a head (or what it
                // hangs from), one of which has room. Nobody to charge: it needs its attestation.
                let Some(ch) = self.charge_of(&c.prev, &signers, &self.inner.charged) else { return Err(OwnError::Unattested) };
                if !self.has_room(&ch) {
                    return Err(OwnError::Quota);
                }
                Arc::make_mut(&mut self.inner).charged.insert(id, ch);
            }
            Admit::Settled => {}
            Admit::Free => {
                if self.attested_count() >= MAX_CERTS {
                    return Err(OwnError::Full);
                }
            }
        }
        let mut anc: BTreeSet<CertId> = c.prev.iter().flat_map(|p| self.inner.anc[p].iter().copied()).collect();
        anc.insert(id);
        let inner = Arc::make_mut(&mut self.inner);
        inner.parked.remove(&id);
        inner.anc.insert(id, Arc::new(anc));
        // The linear state now, so a child in the same batch is judged without another pass (OWNT-10).
        inner.linear.insert(id, state);
        inner.signers.insert(id, signers);
        inner.certs.insert(id, c);
        Ok(Insert::Applied)
    }

    fn concurrent(&self, a: CertId, b: CertId) -> bool {
        !self.inner.anc[&a].contains(&b) && !self.inner.anc[&b].contains(&a)
    }

    fn parent(&self, id: CertId) -> Option<CertId> {
        let c = &self.inner.certs[&id];
        (c.prev.len() == 1).then(|| c.prev[0])
    }

    /// The certificate's signers: the sets sessions committed, if any attested it, else every
    /// signature held.
    fn signers_of(&self, id: CertId) -> BTreeSet<Key> {
        match self.inner.commits.get(&id) {
            Some(m) => m.values().flat_map(|(_, s)| s.iter().copied()).collect(),
            None => self.inner.certs[&id].signers(),
        }
    }

    /// The state after `c` (id `id`), whose single parent left state `p`. A recovery key's signature
    /// counts as its owner's (OT = (a)). Nothing here depends on attestations or time.
    fn step(&self, p: &Owners, c: &Cert, id: CertId, signers: &BTreeSet<Key>) -> Result<Owners, OwnError> {
        if c.seq != p.seq + 1 {
            return Err(OwnError::BadLink);
        }
        let signers = &p.acting(signers);
        let mut s = p.clone();
        let removal = |key: Key| -> Result<(), OwnError> {
            if !p.owners.contains(&key) {
                return Err(OwnError::NotOwner);
            }
            if p.owners.len() == 1 {
                return Err(OwnError::LastOwner);
            }
            if p.rule == SignRule::Any || p.no_removal {
                return Err(OwnError::NotAllowed);
            }
            if !p.rule.met(signers, &p.others(key)) {
                return Err(OwnError::Signers);
            }
            Ok(())
        };
        match c.change {
            Change::Genesis { .. } => return Err(OwnError::BadGenesis),
            Change::Merge => return Err(OwnError::BadLink),
            Change::Add { key } => {
                if p.owners.contains(&key) {
                    return Err(OwnError::AlreadyOwner);
                }
                if !p.rule.met(signers, &p.owners) {
                    return Err(OwnError::Signers);
                }
                s.add_owner(key, id);
                // OWNW-3: the newcomer's share comes out of the remaining shares of the owners who
                // signed its addition (each gives its remaining over the new owner count). Nobody
                // else's share changes, and the shares never add up to more than SOLO_CERTS.
                let n = s.owners.len() as u32;
                let mut gift = 0;
                for g in p.owners.iter().filter(|o| signers.contains(o)) {
                    let rem = p.share.get(g).copied().unwrap_or(0).saturating_sub(p.solo.get(g).copied().unwrap_or(0));
                    let give = rem / n;
                    *s.share.entry(*g).or_default() -= give;
                    gift += give;
                }
                let used = s.solo.get(&key).copied().unwrap_or(0);
                s.share.insert(key, used + gift);
            }
            Change::Leave { key } => {
                if !p.owners.contains(&key) {
                    return Err(OwnError::NotOwner);
                }
                if p.owners.len() == 1 {
                    return Err(OwnError::LastOwner);
                }
                if !signers.contains(&key) {
                    return Err(OwnError::Signers);
                }
                s.drop_owner(key);
                // OWNX-4: the leaver's remaining share passes to the remaining owners (it signed the
                // Leave), so a resale keeps the world's share.
                let to: Vec<Key> = s.owners.iter().copied().collect();
                s.pass_share(key, &to);
            }
            Change::Remove { key } => {
                if !matches!(p.rule, SignRule::Threshold(_)) {
                    return Err(OwnError::NotAllowed);
                }
                removal(key)?;
                s.drop_owner(key);
                // The target's remaining share goes to the owners who removed it (OWNX-4, OWNX-5: a
                // share that changes without its holder signing).
                let to: Vec<Key> = p.owners.iter().copied().filter(|o| *o != key && signers.contains(o)).collect();
                s.pass_share(key, &to);
            }
            Change::SetRule { rule } => {
                if rule == SignRule::Threshold(0) {
                    return Err(OwnError::NotAllowed);
                }
                if !SignRule::All.met(signers, &p.owners) {
                    return Err(OwnError::Signers);
                }
                s.rule = rule;
                s.no_removal = false;
            }
            Change::SetRecovery { owner, recovery } => {
                if !p.owners.contains(&owner) || recovery.is_some_and(|r| p.owners.contains(&r) || r == owner) {
                    return Err(OwnError::NotOwner);
                }
                if !signers.contains(&owner) {
                    return Err(OwnError::Signers);
                }
                // Clearing is exempt from the solo cap, so only a key that is there can be cleared.
                if recovery.is_none() && !p.recovery.contains_key(&owner) {
                    return Err(OwnError::NotAllowed);
                }
                // One backup key backs one owner (OWNX-6).
                if recovery.is_some_and(|r| p.recovery.iter().any(|(o, m)| *o != owner && m.contains_key(&r))) {
                    return Err(OwnError::NotAllowed);
                }
                s.recovery.remove(&owner);
                if let Some(r) = recovery {
                    s.recovery.insert(owner, [(r, id)].into());
                }
            }
        }
        // OWNU-6, OWNV-3, OWNW-3: a solo change (not every owner at the parent signed it) needs one
        // signing owner (itself or through its recovery key) with share left. Exempt: Leave, and
        // clearing one's own recovery key, the way out of a stolen one (OWNV-2).
        let by: Vec<Key> = p.owners.iter().copied().filter(|o| signers.contains(o)).collect();
        let exempt = matches!(c.change, Change::Leave { .. } | Change::SetRecovery { recovery: None, .. });
        if !exempt && !p.owners.is_subset(signers) && !by.is_empty() {
            let room = |o: &Key| p.solo.get(o).copied().unwrap_or(0) < p.share.get(o).copied().unwrap_or(0);
            if !by.iter().any(room) {
                return Err(OwnError::Budget);
            }
            // Each signing owner pays one, but never beyond its share: an owner with nothing left
            // co-signs for free rather than running into debt that a share passed to it later
            // would pay off first (OWNX-4).
            for o in by {
                let share = s.share.get(&o).copied().unwrap_or(0);
                let e = s.solo.entry(o).or_default();
                if *e < share {
                    *e += 1;
                }
            }
        }
        // A recovery key never stays one once it is an owner itself.
        let owners = s.owners.clone();
        for r in s.recovery.values_mut() {
            r.retain(|k, _| !owners.contains(k));
        }
        s.recovery.retain(|_, r| !r.is_empty());
        s.seq = c.seq;
        Ok(s)
    }

    /// The join of several states (O4): observed-remove over owners and recovery keys. `states` holds
    /// the state each certificate was applied to (effective or linear), and `counts` says which applied.
    fn join(&self, heads: &[CertId], states: &BTreeMap<CertId, Owners>, counts: &dyn Fn(CertId) -> bool) -> Owners {
        let st: Vec<&Owners> = heads.iter().map(|h| &states[h]).collect();
        // What the removals in the heads' past saw: the add tags (and recovery tags) they took away.
        let past: BTreeSet<CertId> = heads.iter().flat_map(|h| self.inner.anc[h].iter().copied()).collect();
        let mut dead: BTreeSet<(Key, CertId)> = BTreeSet::new();
        let mut dead_rec: BTreeSet<CertId> = BTreeSet::new();
        for &r in past.iter().filter(|&&r| counts(r)) {
            let Some(p) = self.parent(r) else { continue };
            let Some(ps) = states.get(&p) else { continue };
            let gone = match self.inner.certs[&r].change {
                Change::Leave { key } | Change::Remove { key } => Some(key),
                Change::SetRecovery { owner, .. } => {
                    dead_rec.extend(ps.recovery.get(&owner).into_iter().flat_map(|m| m.values().copied()));
                    None
                }
                _ => None,
            };
            if let Some(k) = gone {
                dead.extend(ps.tags.get(&k).into_iter().flatten().map(|&t| (k, t)));
                dead_rec.extend(ps.recovery.get(&k).into_iter().flat_map(|m| m.values().copied()));
            }
        }
        let mut tags: BTreeMap<Key, BTreeSet<CertId>> = BTreeMap::new();
        for s in &st {
            for (k, t) in &s.tags {
                tags.entry(*k).or_default().extend(t.iter().copied().filter(|&x| !dead.contains(&(*k, x))));
            }
        }
        tags.retain(|_, t| !t.is_empty());
        let nobody_left = tags.is_empty();
        if nobody_left {
            // Nobody left: keep every head's owners rather than nobody.
            for s in &st {
                for (k, t) in &s.tags {
                    tags.entry(*k).or_default().extend(t);
                }
            }
        }
        let owners: BTreeSet<Key> = tags.keys().copied().collect();
        let mut recovery: BTreeMap<Key, BTreeMap<Key, CertId>> = BTreeMap::new();
        for s in &st {
            for (o, r) in &s.recovery {
                for (k, c) in r {
                    if owners.contains(o) && !owners.contains(k) && !dead_rec.contains(c) {
                        recovery.entry(*o).or_default().insert(*k, *c);
                    }
                }
            }
        }
        Owners {
            owners,
            // OWNS-4, OWNT-7: concurrent rules take the strictest in each dimension: who adds (Any <
            // Threshold < All), and whether removal is allowed (not under Any). The owners loosen it
            // again after a merge.
            rule: st.iter().map(|s| s.rule).max().unwrap_or_default(),
            no_removal: st.iter().any(|s| s.rule == SignRule::Any || s.no_removal) && st.iter().any(|s| s.rule != SignRule::Any),
            nobody_left: nobody_left || st.iter().any(|s| s.nobody_left),
            tags,
            recovery,
            solo: join_by(st.iter().map(|s| &s.solo), u32::max),
            share: join_by(st.iter().map(|s| &s.share), u32::min),
            seq: st.iter().map(|s| s.seq).max().unwrap_or(0),
        }
    }

    /// Every certificate in causal order: the linear pass (validity, with the signatures held now),
    /// then the effective pass (OWNR-1): a certificate applies to its parent's effective state only if
    /// it counts there; otherwise its branch inherits the parent's state.
    fn recompute(&mut self) {
        let mut order: Vec<CertId> = self.inner.certs.keys().copied().collect();
        order.sort_by_key(|id| (self.inner.certs[id].seq, *id));
        let signers: BTreeMap<CertId, BTreeSet<Key>> = order.iter().map(|&id| (id, self.signers_of(id))).collect();
        // Linear pass.
        let mut linear: BTreeMap<CertId, Owners> = BTreeMap::new();
        let mut invalid: BTreeSet<CertId> = BTreeSet::new();
        for &id in &order {
            let c = &self.inner.certs[&id];
            let s = if c.prev.is_empty() {
                let Change::Genesis { owners, rule, .. } = &c.change else { unreachable!() };
                let mut s = Owners { rule: *rule, ..Default::default() };
                for &k in owners {
                    s.add_owner(k, id);
                    s.share.insert(k, (SOLO_CERTS / owners.len()) as u32);
                }
                Some(s)
            } else if c.prev.iter().any(|p| invalid.contains(p)) {
                None
            } else if c.change == Change::Merge {
                let mut m = self.join(&c.prev, &linear, &|x| !invalid.contains(&x));
                let ok = SignRule::Any.met(&m.acting(&signers[&id]), &m.owners) && c.seq == m.seq + 1;
                m.seq = c.seq;
                ok.then_some(m)
            } else {
                self.step(&linear[&c.prev[0]], c, id, &signers[&id]).ok()
            };
            match s {
                Some(s) => {
                    linear.insert(id, s);
                }
                None => {
                    invalid.insert(id);
                    let mut s = c.prev.first().map(|p| linear[p].clone()).unwrap_or_default();
                    s.seq = c.seq;
                    linear.insert(id, s);
                }
            }
        }
        {
            let inner = Arc::make_mut(&mut self.inner);
            inner.signers = signers;
            inner.linear = linear;
            inner.invalid = invalid;
        }
        // Effective pass, twice if concurrent counting removals void each other (OWNR-7, OWNS-2).
        let (mut eff, mut counted, mut refused) = self.effective(&order, &BTreeSet::new());
        let removals: Vec<CertId> = counted.iter().copied().filter(|id| self.inner.certs[id].change.removes().is_some()).collect();
        let voided: BTreeSet<CertId> = removals
            .iter()
            .copied()
            .filter(|&a| {
                removals.iter().any(|&b| {
                    let (ta, tb) = (self.inner.certs[&a].change.removes().unwrap(), self.inner.certs[&b].change.removes().unwrap());
                    let acting = |x: CertId| self.inner.linear[&self.inner.certs[&x].prev[0]].acting(&self.inner.signers[&x]);
                    a != b && self.concurrent(a, b) && acting(b).contains(&ta) && acting(a).contains(&tb)
                })
            })
            .collect();
        if !voided.is_empty() {
            (eff, counted, refused) = self.effective(&order, &voided);
        }
        // Heads: counting certificates with no counting child.
        let parents: BTreeSet<CertId> = counted.iter().flat_map(|c| self.inner.certs[c].prev.iter().copied()).collect();
        let heads: Vec<CertId> = counted.iter().copied().filter(|id| !parents.contains(id)).collect();
        let owners = match &heads[..] {
            [h] => eff[h].clone(),
            hs => self.join(hs, &eff, &|x| counted.contains(&x)),
        };
        // OWNS-8, OWNT-8: disputed if two attested, valid certificates whose whole past was
        // committed ever shared a parent (even if both are voided).
        let mut committed: BTreeSet<CertId> = BTreeSet::new();
        for id in &order {
            let c = &self.inner.certs[id];
            if c.prev.is_empty() || (self.inner.commits.contains_key(id) && c.prev.iter().all(|p| committed.contains(p))) {
                committed.insert(*id);
            }
        }
        let mut by_prev: BTreeMap<CertId, usize> = BTreeMap::new();
        for id in &committed {
            let c = &self.inner.certs[id];
            if c.prev.len() == 1 && !self.inner.invalid.contains(id) {
                *by_prev.entry(c.prev[0]).or_default() += 1;
            }
        }
        let disputed = by_prev.values().any(|&n| n > 1);
        let view = View { disputed, heads, owners, refused };
        // OWNU-5: the digest covers what counts (with the committed signer sets), never the junk a
        // book happens to hold, so it doesn't depend on arrival order.
        let mut h = Digest::new();
        h.fact(self.world).u64(counted.len() as u64).u64(disputed as u64);
        for c in &counted {
            h.fact(*c);
            let commits = self.inner.commits.get(c);
            h.u64(commits.map_or(0, |m| m.len() as u64));
            for (l, (f, s)) in commits.into_iter().flatten() {
                h.u64(*l as u64).u64(*f).u64(s.len() as u64);
                s.iter().for_each(|k| {
                    h.key(*k);
                });
            }
        }
        h.u64(view.owners.digest());
        let digest = h.finish();
        let inner = Arc::make_mut(&mut self.inner);
        inner.view = view;
        inner.digest = digest;
        inner.eff = eff;
        inner.counted = counted;
        self.recharge();
    }

    /// After each pass, charges follow the current heads (OWNV-1). An unattested certificate nobody is
    /// charged for any more (its signers no longer matter at a head, or what it hung from is gone) is
    /// dropped, with what hangs from it, unless a session attested something built on it (OWNV-5).
    fn recharge(&mut self) {
        let mut keys = BTreeSet::new();
        for h in &self.inner.view.heads {
            let s = &self.inner.eff[h];
            keys.extend(s.owners.iter().copied());
            keys.extend(s.recovery.values().flat_map(|r| r.keys().copied()));
        }
        Arc::make_mut(&mut self.inner).head_keys = keys;
        let mut order: Vec<CertId> = self.inner.certs.keys().copied().collect();
        order.sort_by_key(|id| (self.inner.certs[id].seq, *id));
        // Attested certificates and everything in their past stay.
        let mut needed: BTreeSet<CertId> = BTreeSet::new();
        for id in order.iter().rev() {
            if self.inner.commits.contains_key(id) || needed.contains(id) {
                needed.insert(*id);
                needed.extend(self.inner.certs[id].prev.iter().copied());
            }
        }
        // Charges change as the heads move: in causal order, a certificate stays if one key (or
        // certificate) it is charged to is under its quota there, as admission would have kept it.
        let mut charged = BTreeMap::new();
        let mut per_key: BTreeMap<Key, usize> = BTreeMap::new();
        let mut per_root: BTreeMap<CertId, usize> = BTreeMap::new();
        let mut fits = |ch: &Charge, take: bool| -> bool {
            let ok = match ch {
                Charge::Keys(ks) => ks.iter().any(|k| per_key.get(k).copied().unwrap_or(0) < UNATTESTED_PER_KEY),
                Charge::Roots(rs) => rs.iter().any(|r| per_root.get(r).copied().unwrap_or(0) < INHERITED_PER_CERT),
            };
            if ok || take {
                match ch {
                    Charge::Keys(ks) => ks.iter().for_each(|k| *per_key.entry(*k).or_default() += 1),
                    Charge::Roots(rs) => rs.iter().for_each(|r| *per_root.entry(*r).or_default() += 1),
                }
            }
            ok
        };
        let mut drop: BTreeSet<CertId> = BTreeSet::new();
        for id in &order {
            let c = &self.inner.certs[id];
            // The genesis and attested certificates are charged to nobody.
            if c.prev.is_empty() || self.inner.commits.contains_key(id) {
                continue;
            }
            let orphaned = c.prev.iter().any(|p| drop.contains(p));
            match self.charge_of(&c.prev, &c.signers(), &charged) {
                // One an attestation depends on stays, charged or not.
                Some(ch) if needed.contains(id) => {
                    fits(&ch, true);
                    charged.insert(*id, ch);
                }
                Some(ch) if !orphaned && fits(&ch, false) => {
                    charged.insert(*id, ch);
                }
                _ if needed.contains(id) => {}
                _ => {
                    drop.insert(*id);
                }
            }
        }
        let mut drop_parked = vec![];
        for (id, c) in &self.inner.parked {
            let held = c.prev.iter().all(|p| self.inner.certs.contains_key(p) && !drop.contains(p));
            match self.charge_of(&c.prev, &c.signers(), &charged) {
                Some(ch) if held && fits(&ch, false) => {
                    charged.insert(*id, ch);
                }
                _ => drop_parked.push(*id),
            }
        }
        let inner = Arc::make_mut(&mut self.inner);
        inner.charged = charged;
        for id in &drop {
            inner.certs.remove(id);
            inner.anc.remove(id);
            inner.linear.remove(id);
            inner.signers.remove(id);
            inner.eff.remove(id);
            inner.invalid.remove(id);
        }
        for id in drop_parked {
            inner.parked.remove(&id);
        }
        if !drop.is_empty() {
            inner.view.refused.retain(|(r, _)| !drop.contains(r));
        }
    }

    #[allow(clippy::type_complexity)]
    fn effective(&self, order: &[CertId], voided: &BTreeSet<CertId>) -> (BTreeMap<CertId, Owners>, BTreeSet<CertId>, Vec<(CertId, OwnError)>) {
        let inner = &self.inner;
        let mut eff: BTreeMap<CertId, Owners> = BTreeMap::new();
        let mut counted: BTreeSet<CertId> = BTreeSet::new();
        let mut refused: Vec<(CertId, OwnError)> = Vec::new();
        // A certificate counts only if its whole causal past was committed by some session: what is
        // built on an uncommitted certificate counts for nothing either.
        let mut uncommitted: BTreeSet<CertId> = BTreeSet::new();
        for &id in order {
            let c = &inner.certs[&id];
            if !c.prev.is_empty() && (!inner.commits.contains_key(&id) || c.prev.iter().any(|p| uncommitted.contains(p))) {
                uncommitted.insert(id);
            }
            let s = if c.prev.is_empty() {
                counted.insert(id);
                inner.linear[&id].clone()
            } else if c.change == Change::Merge {
                let mut m = self.join(&c.prev, &eff, &|x| counted.contains(&x));
                m.seq = c.seq;
                // A merge counts like any certificate: attested, and signed by an owner of the join.
                if inner.invalid.contains(&id) || !SignRule::Any.met(&m.acting(&inner.signers[&id]), &m.owners) {
                    refused.push((id, OwnError::Signers));
                } else if uncommitted.contains(&id) {
                    refused.push((id, OwnError::Unattested));
                } else {
                    counted.insert(id);
                }
                m
            } else {
                let base = &eff[&c.prev[0]];
                let r = if inner.invalid.contains(&id) {
                    Err(OwnError::Signers)
                } else if uncommitted.contains(&id) {
                    Err(OwnError::Unattested)
                } else if voided.contains(&id) {
                    Err(OwnError::Conflict)
                } else if !self.approved_elsewhere(id, c, base, &counted) {
                    Err(OwnError::Signers)
                } else {
                    self.step(base, c, id, &inner.signers[&id])
                };
                match r {
                    Ok(s) => {
                        counted.insert(id);
                        s
                    }
                    Err(e) => {
                        refused.push((id, e));
                        let mut s = base.clone();
                        s.seq = c.seq;
                        s
                    }
                }
            };
            eff.insert(id, s);
        }
        (eff, counted, refused)
    }

    /// OWN-3: a removal made on one branch also needs the owners that counting additions on
    /// concurrent branches added since it split off (their parent is a strict ancestor of its
    /// parent). Additions don't: each copy's owners add independently, and the copies join
    /// (OWNS-3: an old-state fork never voids a later addition).
    fn approved_elsewhere(&self, id: CertId, c: &Cert, base: &Owners, counted: &BTreeSet<CertId>) -> bool {
        let target = match c.change {
            Change::Remove { key } => key,
            _ => return true,
        };
        let p = c.prev[0];
        let added: BTreeSet<Key> = counted
            .iter()
            .copied()
            .filter(|&d| self.concurrent(d, id))
            .filter(|&d| self.parent(d).is_some_and(|dp| dp != p && self.inner.anc[&p].contains(&dp)))
            .filter_map(|d| match self.inner.certs[&d].change {
                Change::Add { key } => Some(key),
                _ => None,
            })
            .collect();
        if added.is_empty() {
            return true;
        }
        let mut need: BTreeSet<Key> = base.owners.union(&added).copied().collect();
        need.remove(&target);
        base.rule.met(&base.acting(&self.inner.signers[&id]), &need)
    }

    /// The heads: counting certificates with no counting child.
    pub fn heads(&self) -> Vec<CertId> {
        self.inner.view.heads.clone()
    }

    pub fn view(&self) -> View {
        self.inner.view.clone()
    }

    /// Does `id` count (held, valid, attested, valid at its effective parent)?
    pub fn counts(&self, id: CertId) -> bool {
        self.inner.counted.contains(&id)
    }

    /// Every pair of valid certificates with one parent and different changes: concurrent changes,
    /// and when a key signed both, a transferable equivocation proof against it.
    pub fn forks(&self) -> Vec<ForkProof> {
        let mut by_prev: BTreeMap<CertId, Vec<&Cert>> = BTreeMap::new();
        for (id, c) in &self.inner.certs {
            if c.prev.len() == 1 && !self.inner.invalid.contains(id) {
                by_prev.entry(c.prev[0]).or_default().push(c);
            }
        }
        let mut out = Vec::new();
        for v in by_prev.values() {
            for (i, a) in v.iter().enumerate() {
                for b in &v[i + 1..] {
                    out.push(ForkProof { world: self.world, a: (*a).clone(), b: (*b).clone() });
                }
            }
        }
        out
    }

    /// The whole causal past of `id` (itself included), oldest first: a verifier's prefix. Empty for
    /// an id the book doesn't hold (OWNS-11).
    pub fn prefix_of(&self, id: CertId) -> Vec<Cert> {
        let Some(a) = self.inner.anc.get(&id) else { return vec![] };
        let mut v: Vec<&Cert> = a.iter().map(|a| &self.inner.certs[a]).collect();
        v.sort_by_key(|c| (c.seq, c.id()));
        v.into_iter().cloned().collect()
    }

    /// For the state hash: the counting certificates, their attested commits and the view (OWNU-5).
    pub fn digest(&self) -> u64 {
        self.inner.digest
    }
}

/// Check a chain offline, in any order, with the sessions' attestations (each checked against
/// `sessions`' members; a forged one is ignored). Junk is skipped (OWNS-10); the view lists what
/// doesn't count, and may be disputed.
pub fn verify_chain(genesis: &Cert, rest: &[Cert], attests: &[Attest], sessions: &dyn EpochCerts) -> Result<View, OwnError> {
    let mut b = Book::new(genesis.clone())?;
    b.insert_attested(rest, attests, sessions);
    Ok(b.view())
}

/// Two valid certificates with one parent. Anyone holding the chain up to that parent can check it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkProof {
    pub world: CertId,
    pub a: Cert,
    pub b: Cert,
}

impl ForkProof {
    /// `prefix`: the causal past of the shared parent, genesis included, in any order (OWN-9).
    /// Validity only: proving two signatures needs no attestation.
    pub fn verify(&self, prefix: &[Cert]) -> bool {
        let Some(g) = prefix.iter().find(|c| c.seq == 0) else { return false };
        let Ok(mut book) = Book::new(g.clone()) else { return false };
        let mut rest: Vec<Cert> = prefix.iter().filter(|c| c.seq > 0).cloned().collect();
        rest.sort_by_key(|c| (c.seq, c.id()));
        let all_in = rest.iter().all(|c| book.insert_raw(c.clone(), Admit::Free).is_ok());
        book.world == self.world
            && all_in
            && self.a.prev.len() == 1
            && self.a.prev == self.b.prev
            && self.a.id() != self.b.id()
            && book.insert_raw(self.a.clone(), Admit::Free) == Ok(Insert::Applied)
            && book.insert_raw(self.b.clone(), Admit::Free) == Ok(Insert::Applied)
    }

    /// The keys that signed both: whoever a client's own policy may distrust. Empty for honest
    /// concurrency, such as two owners each naming a recovery key while offline (OWN-8).
    pub fn signed_both(&self) -> BTreeSet<Key> {
        self.a.signers().intersection(&self.b.signers()).copied().collect()
    }

    pub fn equivocation(&self) -> bool {
        !self.signed_both().is_empty()
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod harness;
