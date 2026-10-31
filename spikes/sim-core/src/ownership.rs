//! World ownership as a chain of signed certificates (user ruling 2026-10-30 22:39Z, "Signed chain,
//! no blockchain"; design 12.5). Ownership is a lasting property of a world, separate from a session's
//! admins, and can be shared. It needs no registry or service of ours.
//!
//! * **Genesis.** Signed by every creator. The world id is the hash of the genesis certificate (in
//!   production, of the genesis document that contains it).
//! * **Certificates.** Each names its parent and is *valid* if its signatures satisfy the rule of the
//!   owner set at the parent: a function of its causal past alone. Its id covers its signer set, so
//!   two copies with different signatures are two certificates (review OWNR-4).
//! * **Session attestations.** A session that commits a certificate records the world frame it did so
//!   at; exported, that is an [`Attest`], signed by a strict majority of the session's members (P2-4's
//!   epoch certificates), so anyone can check it offline. A certificate **counts** only once some
//!   session attested it: one signed on an old copy and never committed changes nothing (round 2's
//!   mitigation for the residual of forks). A session commits only certificates built on its current
//!   head, so within one session the chain is linear.
//! * **Rule A** (O1): adding needs every owner; anyone may leave, except the last owner. Removal is
//!   only a 30-day objection window that every other owner opens (OB), and a recovery key only
//!   replaces a lost key after 7 days (OC). Both count attested world frames, and an ownership
//!   signature of the target attested between the window's start and its completion cancels it
//!   (OWNR-2). Rules B (any owner adds; no removal) and C (k of n, instant removal) are kept for
//!   comparison.
//! * **The owner set** ([`Book::view`]) comes from an effective pass over the certificates held, in
//!   causal order (OWNR-1): a certificate applies only if it counts at its parent's *effective* state;
//!   otherwise its branch inherits the parent's state. After a fork the heads join as an
//!   observed-remove set (O4): a Leave or removal sticks unless the same history adds the person back.
//!   A change on a branch counts only if owners that counting changes on concurrent branches added
//!   since it split off approve it too (OWN-3). Concurrent mutual removals void each other (OWNR-7).
//!   The rule after a fork is the newest by causality, the strictest among concurrent ones (OWNR-9).
//!
//! What remains [I]: keys that once held full authority can still add a co-owner by forking at that
//! state, if they also run a session on an old copy to get it attested; and a private session can
//! claim any frames, so it can age a window. Both are detected (an equivocation proof, "Ownership
//! disputed"). Preventing them needs a global order, which the user rejected (D79).
//!
//! Spike signatures are keyless hashes ([`key_sig`], `member_sig`) standing in for Ed25519.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::provenance::EpochCerts;
use crate::session::{member_sig, MemberId};
use crate::{fnv_u64s, splitmix64};

/// A player's long-term identity key (design 10.3). Spike: a number.
pub type Key = u32;
pub type CertId = u64;
/// A session lineage: the session that committed a certificate (spike: its world id).
pub type Lineage = u32;

/// The objection window before an owner can be removed: 30 days of world frames at 60 Hz.
pub const OBJECTION_FRAMES: u64 = 30 * 24 * 3600 * 60;
/// The delay before a recovery key's replacement of its owner's key completes: 7 days.
pub const RECOVERY_FRAMES: u64 = 7 * 24 * 3600 * 60;
/// Certificates one book holds at most (OWNR-6: no signer can grow it without bound). Ownership
/// changes are rare; each insert re-runs the effective pass, O(N log N). Production compacts a long
/// linear history into a checkpoint signed by the owners [I].
pub const MAX_CERTS: usize = 1024;

/// Spike model of `key` signing `msg`. Production: Ed25519.
pub fn key_sig(key: Key, msg: u64) -> u64 {
    splitmix64(msg ^ splitmix64(key as u64 ^ 0x4F57_4E45_5253_4950))
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
    fn code(self) -> u64 {
        match self {
            SignRule::Any => 1,
            SignRule::Threshold(k) => 0x100 | k as u64,
            SignRule::All => 3,
        }
    }
    fn from_code(x: u64) -> Option<SignRule> {
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
    /// The world's first certificate. `nonce` keeps two worlds made by the same people apart.
    Genesis { owners: Vec<Key>, rule: SignRule, nonce: u64 },
    Add { key: Key },
    /// An owner gives up ownership; signed by that owner's own key.
    Leave { key: Key },
    /// Rule C only: `k` of the other owners remove `key` at once.
    Remove { key: Key },
    /// Every owner signs.
    SetRule { rule: SignRule },
    /// `owner` (its own key) names, replaces or clears its recovery key.
    SetRecovery { owner: Key, recovery: Option<Key> },
    /// Every other owner (A; k of them under C) opens the objection window against `key`.
    Challenge { key: Key },
    /// The same signers remove `key` once its window has run with no signature from it.
    RemoveDormant { key: Key },
    /// `owner`'s recovery key starts replacing `owner` by `new_key`.
    StartRecover { owner: Key, new_key: Key },
    /// The same recovery key completes it once the delay has run with no signature from `owner`.
    FinishRecover { owner: Key },
    /// Joins concurrent certificates: the chain is linear again. Its owner set is forced (the join).
    Merge,
}

impl Change {
    fn words(&self, v: &mut Vec<u64>) {
        match self {
            Change::Genesis { owners, rule, nonce } => {
                v.extend([1, rule.code(), *nonce, owners.len() as u64]);
                v.extend(owners.iter().map(|&k| k as u64));
            }
            Change::Add { key } => v.extend([2, *key as u64]),
            Change::Leave { key } => v.extend([3, *key as u64]),
            Change::Remove { key } => v.extend([4, *key as u64]),
            Change::SetRule { rule } => v.extend([5, rule.code()]),
            Change::SetRecovery { owner, recovery } => v.extend([6, *owner as u64, recovery.map_or(0, |r| 1 + r as u64)]),
            Change::Challenge { key } => v.extend([7, *key as u64]),
            Change::RemoveDormant { key } => v.extend([8, *key as u64]),
            Change::Merge => v.push(9),
            Change::StartRecover { owner, new_key } => v.extend([10, *owner as u64, *new_key as u64]),
            Change::FinishRecover { owner } => v.extend([11, *owner as u64]),
        }
    }

    /// The key a removal takes out.
    fn removes(&self) -> Option<Key> {
        match self {
            Change::Remove { key } | Change::RemoveDormant { key } => Some(*key),
            _ => None,
        }
    }
}

/// One ownership certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cert {
    /// The world id (the genesis certificate's id); 0 in the genesis certificate itself.
    pub world: u64,
    /// 0 for genesis; one more than the highest parent's.
    pub seq: u32,
    /// Parents: none for genesis, several (concurrent) for a merge, one otherwise.
    pub prev: Vec<CertId>,
    pub change: Change,
    pub sigs: Vec<(Key, u64)>,
}

impl Cert {
    fn body(&self) -> Vec<u64> {
        let mut v = vec![self.world, self.seq as u64, self.prev.len() as u64];
        v.extend(&self.prev);
        self.change.words(&mut v);
        v
    }

    /// What each key signs: everything but the signatures.
    pub fn body_hash(&self) -> u64 {
        fnv_u64s(self.body())
    }

    /// The certificate's id: its body hash. Copies with different signatures are one certificate; a
    /// book merges their signatures that matter until a session attests it (OWNR-4, OWNS-5).
    pub fn id(&self) -> CertId {
        self.body_hash()
    }

    pub fn signed(mut self, keys: &[Key]) -> Cert {
        let h = self.body_hash();
        self.sigs.extend(keys.iter().map(|&k| (k, key_sig(k, h))));
        self
    }

    /// The keys with a valid signature on this certificate.
    pub fn signers(&self) -> BTreeSet<Key> {
        let h = self.body_hash();
        self.sigs.iter().filter(|&&(k, s)| s == key_sig(k, h)).map(|&(k, _)| k).collect()
    }

    pub fn genesis(owners: &[Key], rule: SignRule, nonce: u64) -> Cert {
        let mut o = owners.to_vec();
        o.sort_unstable();
        o.dedup();
        Cert { world: 0, seq: 0, prev: vec![], change: Change::Genesis { owners: o, rule, nonce }, sigs: vec![] }.signed(owners)
    }

    /// For the state hash: every field, signatures included.
    pub fn digest(&self) -> u64 {
        let mut v = vec![self.id(), self.sigs.len() as u64];
        v.extend(self.sigs.iter().flat_map(|&(k, s)| [k as u64, s]));
        fnv_u64s(v)
    }

    /// Spike wire form (session log entries): a list of words.
    pub fn to_words(&self) -> Vec<u64> {
        let mut v = self.body();
        v.push(self.sigs.len() as u64);
        v.extend(self.sigs.iter().flat_map(|&(k, s)| [k as u64, s]));
        v
    }

    /// The inverse of [`Cert::to_words`]. Refuses trailing words, so each certificate has one
    /// encoding (OWN-11).
    pub fn from_words(w: &[u64]) -> Option<Cert> {
        let mut it = w.iter().copied();
        let mut next = || it.next();
        let (world, seq, np) = (next()?, u32::try_from(next()?).ok()?, next()? as usize);
        let prev = (0..np.min(w.len())).map(|_| next()).collect::<Option<Vec<_>>>()?;
        let key = |x: u64| u32::try_from(x).ok();
        let change = match next()? {
            1 => {
                let (rule, nonce, n) = (SignRule::from_code(next()?)?, next()?, next()? as usize);
                let owners = (0..n.min(w.len())).map(|_| next().and_then(key)).collect::<Option<Vec<_>>>()?;
                Change::Genesis { owners, rule, nonce }
            }
            2 => Change::Add { key: key(next()?)? },
            3 => Change::Leave { key: key(next()?)? },
            4 => Change::Remove { key: key(next()?)? },
            5 => Change::SetRule { rule: SignRule::from_code(next()?)? },
            6 => {
                let owner = key(next()?)?;
                let recovery = match next()? {
                    0 => None,
                    r => Some(key(r - 1)?),
                };
                Change::SetRecovery { owner, recovery }
            }
            7 => Change::Challenge { key: key(next()?)? },
            8 => Change::RemoveDormant { key: key(next()?)? },
            9 => Change::Merge,
            10 => Change::StartRecover { owner: key(next()?)?, new_key: key(next()?)? },
            11 => Change::FinishRecover { owner: key(next()?)? },
            _ => return None,
        };
        let ns = next()? as usize;
        let sigs = (0..ns.min(w.len())).map(|_| Some((key(next()?)?, next()?))).collect::<Option<Vec<_>>>()?;
        if next().is_some() {
            return None;
        }
        Some(Cert { world, seq, prev, change, sigs })
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
    /// The certificate's signer set the session committed (its signatures are frozen from then on).
    pub signers: Vec<Key>,
    pub sigs: Vec<(MemberId, u64)>,
}

impl Attest {
    pub fn fact(world: u64, cert: CertId, lineage: Lineage, epoch: u32, frame: u64, signers: &[Key]) -> u64 {
        let head = [0x00A7_7E57, world, cert, lineage as u64, epoch as u64, frame, signers.len() as u64];
        fnv_u64s(head.into_iter().chain(signers.iter().map(|&k| k as u64)))
    }
    /// Signed by every member given (spike signatures).
    pub fn make(world: u64, cert: CertId, lineage: Lineage, frame: u64, signers: &BTreeSet<Key>, members: &[MemberId]) -> Attest {
        let signers: Vec<Key> = signers.iter().copied().collect();
        let f = Attest::fact(world, cert, lineage, 0, frame, &signers);
        Attest { cert, lineage, epoch: 0, frame, signers, sigs: members.iter().map(|&m| (m, member_sig(lineage, m, f))).collect() }
    }
    pub fn verify(&self, world: u64, sessions: &dyn EpochCerts) -> bool {
        let f = Attest::fact(world, self.cert, self.lineage, self.epoch, self.frame, &self.signers);
        sessions.cert_ok(self.lineage, self.epoch, f, &self.sigs)
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
    /// Not under this rule (instant removal outside rule C, any removal under rule B), or no rule.
    NotAllowed,
    /// No open window, the window hasn't run, or the target cancelled it.
    WindowOpen,
    /// No session has attested the certificate (or, for a window, both of its ends in one session).
    Unattested,
    /// A session commits only certificates built on the head of what it committed itself.
    Stale,
    /// Two concurrent counting removals each remove a signer of the other (OWNR-7).
    Conflict,
    /// The recovery key had been replaced, attested in the same session earlier, before it acted.
    Revoked,
    /// The book holds [`MAX_CERTS`] certificates.
    Full,
    /// Every signer already has [`UNATTESTED_PER_KEY`] unattested certificates in the book (OWNS-5).
    Quota,
}

/// Unattested certificates one key may have in a book (OWNS-5: nobody floods it).
pub const UNATTESTED_PER_KEY: usize = 16;

/// A recovery in progress: `by` started replacing its owner with `new_key` at certificate `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recovering {
    pub start: CertId,
    pub by: Key,
    pub new_key: Key,
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
    /// Each owner's recovery keys, with the certificate that named each.
    pub recovery: BTreeMap<Key, BTreeMap<Key, CertId>>,
    /// Open objection windows: target -> the `Challenge` that opened it.
    pub challenges: BTreeMap<Key, CertId>,
    /// Recoveries in progress, by owner.
    pub recovering: BTreeMap<Key, Recovering>,
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
        self.challenges.remove(&key);
        self.recovering.remove(&key);
    }

    /// Keys whose signatures matter on a certificate built on this state: owners and recovery keys.
    fn relevant(&self, k: Key) -> bool {
        self.owners.contains(&k) || self.recovery.values().any(|r| r.contains_key(&k))
    }

    pub fn digest(&self) -> u64 {
        let mut v = vec![self.rule.code(), self.no_removal as u64 | (self.nobody_left as u64) << 1, self.seq as u64, self.owners.len() as u64];
        for (k, t) in &self.tags {
            v.extend([*k as u64, t.len() as u64]);
            v.extend(t);
        }
        for (o, r) in &self.recovery {
            v.extend([*o as u64, r.len() as u64]);
            v.extend(r.iter().flat_map(|(&k, &c)| [k as u64, c]));
        }
        v.extend(self.challenges.iter().flat_map(|(&k, &c)| [k as u64 | 1 << 40, c]));
        v.extend(self.recovering.iter().flat_map(|(&o, r)| [o as u64 | 1 << 41, r.start, r.by as u64, r.new_key as u64]));
        fnv_u64s(v)
    }
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
    /// Certificates refused for the unattested pool (their parent didn't count yet, or the quota),
    /// kept to re-admit once a parent counts or their attestation arrives (OWNT-5). Bounded; the
    /// lowest ids stay. They count for nothing and aren't in the digest.
    parked: BTreeMap<CertId, Cert>,
    /// Verified attestations of certificates not held yet (bounded likewise).
    pending: BTreeMap<CertId, Vec<Attest>>,
    /// The parked or pending buffer ever overflowed: a certificate or attestation was dropped, and
    /// has to be sent again (invariant 4 holds up to the buffers' bounds).
    overflowed: bool,
    /// Results of the last pass: signer sets, linear validity and state, effective states, the view,
    /// the digest.
    signers: BTreeMap<CertId, BTreeSet<Key>>,
    linear: BTreeMap<CertId, Owners>,
    invalid: BTreeSet<CertId>,
    eff: BTreeMap<CertId, Owners>,
    view: View,
    digest: u64,
}

/// Unattested certificates a book holds at most, apart from [`MAX_CERTS`] (OWNT-2).
pub const UNATTESTED_MAX: usize = 256;
/// Parked certificates and pending attestations a book keeps at most.
pub const PARKED_MAX: usize = 256;

/// What one verifier holds of a world's ownership. Cloning shares everything until the next change
/// (keyframes, OWNR-6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Book {
    pub world: u64,
    pub genesis: CertId,
    inner: Arc<Inner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    Applied,
    Duplicate,
    /// Kept aside until a parent counts or its attestation arrives (it counts for nothing yet).
    Parked,
}

/// How a certificate is admitted: through the bounded pool of unattested certificates, or freely,
/// because a session attests it or because only its validity is being checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admit {
    Pool,
    Free,
}

impl Book {
    pub fn new(genesis: Cert) -> Result<Book, OwnError> {
        let Change::Genesis { owners, rule, .. } = &genesis.change else { return Err(OwnError::BadGenesis) };
        let set: BTreeSet<Key> = owners.iter().copied().collect();
        if genesis.world != 0 || genesis.seq != 0 || !genesis.prev.is_empty() || set.is_empty() || set.len() != owners.len() {
            return Err(OwnError::BadGenesis);
        }
        if *rule == SignRule::Threshold(0) || !genesis.signers().is_superset(&set) {
            return Err(OwnError::BadGenesis);
        }
        let id = genesis.id();
        let mut g = genesis;
        let h = g.body_hash();
        g.sigs.retain(|&(k, s)| set.contains(&k) && s == key_sig(k, h));
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

    /// Store `c` if it is valid against its parent's state. Unattested, it enters the bounded pool:
    /// only on a parent that counts, charged to signers that are owners or recovery keys there, each
    /// under its quota (OWNT-2); otherwise it is parked. It counts only once attested. A copy of a
    /// held certificate adds its missing signatures, until a session attests it.
    pub fn insert(&mut self, c: Cert) -> Result<Insert, OwnError> {
        let r = self.admit(c)?;
        self.recompute();
        self.readmit();
        Ok(r)
    }

    /// Pool admission, parking what the pool can't take yet.
    fn admit(&mut self, c: Cert) -> Result<Insert, OwnError> {
        let id = c.id();
        let mode = if self.inner.pending.contains_key(&id) { Admit::Free } else { Admit::Pool };
        match self.insert_raw(c.clone(), mode) {
            Ok(r) => {
                self.apply_pending(id);
                Ok(r)
            }
            // A refusal that depends on what the book holds so far (a parent, a state, the pool)
            // may change as more arrives: park and retry. Only a wrong world or a second genesis is
            // final. The certificate is still reported as refused.
            Err(e) if c.world == self.world && e != OwnError::BadGenesis => {
                self.park(c);
                if matches!(e, OwnError::Quota | OwnError::Unattested | OwnError::MissingParent) { Ok(Insert::Parked) } else { Err(e) }
            }
            Err(e) => Err(e),
        }
    }

    fn park(&mut self, c: Cert) {
        let inner = Arc::make_mut(&mut self.inner);
        inner.parked.insert(c.id(), c);
        while inner.parked.len() > PARKED_MAX {
            let last = *inner.parked.keys().next_back().unwrap();
            inner.parked.remove(&last);
            inner.overflowed = true;
        }
    }

    /// Apply buffered attestations of a certificate that just became held.
    fn apply_pending(&mut self, id: CertId) {
        if let Some(v) = Arc::make_mut(&mut self.inner).pending.remove(&id) {
            for a in v {
                self.record_raw(a.cert, a.lineage, a.frame, a.signers.iter().copied().collect());
            }
        }
    }

    /// Try the parked certificates again, until nothing more gets in.
    fn readmit(&mut self) {
        loop {
            let parked: Vec<Cert> = self.inner.parked.values().cloned().collect();
            let mut progress = false;
            for c in parked {
                let id = c.id();
                Arc::make_mut(&mut self.inner).parked.remove(&id);
                let mode = if self.inner.pending.contains_key(&id) { Admit::Free } else { Admit::Pool };
                match self.insert_raw(c.clone(), mode) {
                    Ok(_) => {
                        self.apply_pending(id);
                        progress = true;
                    }
                    Err(OwnError::WrongWorld | OwnError::BadGenesis) => {}
                    Err(_) => self.park(c),
                }
            }
            if !progress {
                break;
            }
            self.recompute();
        }
    }

    /// Insert many, in any order (OWNS-9), parking what can't get in yet. Returns what was refused
    /// for good (OWNS-10: junk is skipped, not fatal).
    pub fn insert_all(&mut self, certs: &[Cert]) -> Vec<(CertId, OwnError)> {
        self.insert_attested(certs, &[], &crate::provenance::StaticEpochs::default())
    }

    /// As [`Book::insert_all`], with the sessions' attestations: those are verified and buffered first,
    /// so an attested certificate gets in freely whatever the order (OWNT-5).
    pub fn insert_attested(&mut self, certs: &[Cert], attests: &[Attest], sessions: &dyn EpochCerts) -> Vec<(CertId, OwnError)> {
        for a in attests {
            self.buffer(a, sessions);
        }
        let mut sorted: Vec<&Cert> = certs.iter().collect();
        sorted.sort_by_key(|c| (c.seq, c.id()));
        let mut refused = Vec::new();
        for c in sorted {
            if let Err(e) = self.admit(c.clone()) {
                refused.push((c.id(), e));
            }
        }
        self.recompute();
        self.readmit();
        refused
    }

    /// Record a verified attestation, or buffer it until its certificate arrives.
    fn buffer(&mut self, a: &Attest, sessions: &dyn EpochCerts) -> bool {
        if !a.verify(self.world, sessions) {
            return false;
        }
        if self.inner.certs.contains_key(&a.cert) {
            self.record_raw(a.cert, a.lineage, a.frame, a.signers.iter().copied().collect());
        } else {
            let inner = Arc::make_mut(&mut self.inner);
            let v = inner.pending.entry(a.cert).or_default();
            if !v.contains(a) {
                v.push(a.clone());
            }
            while inner.pending.len() > PARKED_MAX {
                let last = *inner.pending.keys().next_back().unwrap();
                inner.pending.remove(&last);
                inner.overflowed = true;
            }
        }
        true
    }

    /// The local session committed `id` at its `frame` (its own record; exported as an [`Attest`]),
    /// with the certificate's signer set as it holds it now. A parked certificate gets in with it.
    pub fn record(&mut self, id: CertId, lineage: Lineage, frame: u64) {
        if let Some(c) = self.inner.parked.get(&id).cloned()
            && self.insert_raw(c, Admit::Free).is_ok()
        {
            Arc::make_mut(&mut self.inner).parked.remove(&id);
        }
        match self.inner.certs.get(&id) {
            Some(c) => {
                let signers = c.signers();
                self.record_raw(id, lineage, frame, signers);
            }
            None => {
                // Not held yet (its parent is parked too): keep the record for when it arrives.
                let signers = self.inner.parked.get(&id).map(|c| c.signers().into_iter().collect()).unwrap_or_default();
                let a = Attest { cert: id, lineage, epoch: 0, frame, signers, sigs: vec![] };
                Arc::make_mut(&mut self.inner).pending.entry(id).or_default().push(a);
            }
        }
        self.recompute();
        self.readmit();
    }

    /// One lineage's attestations of one certificate can differ (a session equivocating): the lowest
    /// frame wins, then the smallest signer set, whatever the order they arrive in (OWNT-6).
    fn record_raw(&mut self, id: CertId, lineage: Lineage, frame: u64, signers: BTreeSet<Key>) {
        if !self.inner.certs.contains_key(&id) {
            return;
        }
        let inner = Arc::make_mut(&mut self.inner);
        let e = inner.commits.entry(id).or_default().entry(lineage).or_insert((frame, signers.clone()));
        let key = |f: u64, s: &BTreeSet<Key>| (f, s.len(), s.iter().copied().collect::<Vec<_>>());
        if key(frame, &signers) < key(e.0, &e.1) {
            *e = (frame, signers);
        }
    }

    /// Import another session's attestation, if its signatures check out (buffered if its certificate
    /// isn't held yet).
    pub fn attest(&mut self, a: &Attest, sessions: &dyn EpochCerts) -> bool {
        let ok = self.buffer(a, sessions);
        if ok {
            if let Some(c) = self.inner.parked.get(&a.cert).cloned() {
                Arc::make_mut(&mut self.inner).parked.remove(&a.cert);
                if self.insert_raw(c.clone(), Admit::Free).is_ok() {
                    self.apply_pending(a.cert);
                } else {
                    self.park(c);
                }
            }
            self.recompute();
            self.readmit();
        }
        ok
    }

    /// The attestations of every certificate session `lineage` committed in this book.
    pub fn export(&self, lineage: Lineage, members: &[MemberId]) -> Vec<Attest> {
        self.inner
            .commits
            .iter()
            .filter_map(|(c, m)| m.get(&lineage).map(|(f, s)| Attest::make(self.world, *c, lineage, *f, s, members)))
            .collect()
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
        if self.attested_count() >= MAX_CERTS {
            return Err(OwnError::Full);
        }
        let mut b = self.clone();
        Arc::make_mut(&mut b.inner).parked.remove(&id);
        b.insert_raw(c, Admit::Free)?;
        let signers = b.inner.certs[&id].signers();
        b.record_raw(id, lineage, frame, signers);
        b.apply_pending(id);
        b.recompute();
        if let Some(&(_, e)) = b.inner.view.refused.iter().find(|(r, _)| *r == id) {
            return Err(e);
        }
        b.readmit();
        *self = b;
        Ok(Insert::Applied)
    }

    /// The state a certificate built on `prev` is judged against, as of the last pass.
    fn parent_state(&self, prev: &[CertId]) -> Option<Owners> {
        match prev {
            [p] => (!self.inner.invalid.contains(p)).then(|| self.inner.linear[p].clone()),
            ps => Some(self.join(ps, &self.inner.linear, &|x| !self.inner.invalid.contains(&x))),
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
        let h = c.body_hash();
        let Some(p) = self.parent_state(&c.prev) else { return Err(OwnError::BadLink) };
        // Only signatures that can matter: owners' and recovery keys' at the parent (OWNS-5).
        c.sigs.retain(|&(k, s)| p.relevant(k) && s == key_sig(k, h));
        c.sigs.sort_unstable();
        c.sigs.dedup_by_key(|x| x.0);
        if let Some(held) = self.inner.certs.get(&id) {
            if self.inner.commits.contains_key(&id) {
                return Ok(Insert::Duplicate);
            }
            let have = held.signers();
            let more: Vec<(Key, u64)> = c.sigs.iter().copied().filter(|(k, _)| !have.contains(k)).collect();
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
            if !SignRule::Any.met(&signers, &p.owners) {
                return Err(OwnError::Signers);
            }
            let mut m = p.clone();
            m.seq = c.seq;
            m
        } else {
            if c.prev.len() != 1 {
                return Err(OwnError::BadLink);
            }
            self.step(&p, &c, id, &signers, false)?
        };
        if mode == Admit::Pool {
            // OWNT-2: an unattested certificate only on a parent that counts, charged to keys that
            // are owners or recovery keys at that parent's effective state, within the pool.
            let counting = c.prev.iter().all(|x| self.counts(*x) && self.inner.eff.contains_key(x));
            if !counting {
                return Err(OwnError::Unattested);
            }
            let charged: Vec<Key> = signers.iter().copied().filter(|&k| c.prev.iter().any(|x| self.inner.eff[x].relevant(k))).collect();
            let unattested = |k: Key| {
                self.inner.certs.iter().filter(|(i, x)| !self.inner.commits.contains_key(i) && x.sigs.iter().any(|s| s.0 == k)).count()
            };
            let pool = self.inner.certs.len() - self.attested_count();
            if charged.is_empty() || pool >= UNATTESTED_MAX || charged.iter().all(|&k| unattested(k) >= UNATTESTED_PER_KEY) {
                return Err(OwnError::Quota);
            }
        } else if self.attested_count() >= MAX_CERTS {
            return Err(OwnError::Full);
        }
        let mut anc: BTreeSet<CertId> = c.prev.iter().flat_map(|p| self.inner.anc[p].iter().copied()).collect();
        anc.insert(id);
        let inner = Arc::make_mut(&mut self.inner);
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

    fn frame_in(&self, id: CertId, lineage: Lineage) -> Option<u64> {
        self.inner.commits.get(&id).and_then(|m| m.get(&lineage)).map(|(f, _)| *f)
    }

    /// The lineages in which the window from `start` to `end` ran `frames`, with both ends' frames
    /// there. Frames are compared only within one lineage (OWNS-1c).
    fn windows(&self, start: CertId, end: CertId, frames: u64) -> Result<Vec<(Lineage, u64, u64)>, OwnError> {
        let (Some(s), Some(e)) = (self.inner.commits.get(&start), self.inner.commits.get(&end)) else { return Err(OwnError::Unattested) };
        let common: Vec<(Lineage, u64, u64)> = s.iter().filter_map(|(l, (fs, _))| e.get(l).map(|(fe, _)| (*l, *fs, *fe))).collect();
        if common.is_empty() {
            return Err(OwnError::Unattested);
        }
        let ran: Vec<_> = common.into_iter().filter(|&(_, fs, fe)| fe >= fs.saturating_add(frames)).collect();
        if ran.is_empty() {
            return Err(OwnError::WindowOpen);
        }
        Ok(ran)
    }

    /// `k` signed a certificate outside `start`'s past that `lineage` attested at a frame in
    /// `[from, to)`: the window was cancelled there (OWNR-2: unattested signatures, signatures after
    /// the completion and signatures in other lineages never count).
    fn signed_between(&self, k: Key, start: CertId, lineage: Lineage, from: u64, to: u64) -> bool {
        self.inner.commits.iter().any(|(e, m)| {
            *e != start
                && !self.inner.anc[&start].contains(e)
                && m.get(&lineage).is_some_and(|(f, s)| *f >= from && *f < to && s.contains(&k))
        })
    }

    /// A completion counts if in some lineage the window ran and the target didn't cancel there.
    fn completed(&self, target: Key, start: CertId, end: CertId, frames: u64) -> Result<(), OwnError> {
        let ran = self.windows(start, end, frames)?;
        if ran.iter().any(|&(l, fs, fe)| !self.signed_between(target, start, l, fs, fe)) {
            Ok(())
        } else {
            Err(OwnError::WindowOpen)
        }
    }

    /// The state after `c` (id `id`), whose single parent left state `p`. `effective`: also check what
    /// depends on attestations (windows, cancellation, revocation).
    fn step(&self, p: &Owners, c: &Cert, id: CertId, signers: &BTreeSet<Key>, effective: bool) -> Result<Owners, OwnError> {
        if c.seq != p.seq + 1 {
            return Err(OwnError::BadLink);
        }
        let mut s = p.clone();
        // An owner's own signature cancels any window against it on this history.
        s.challenges.retain(|k, _| !signers.contains(k));
        s.recovering.retain(|k, _| !signers.contains(k));
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
            }
            Change::Remove { key } => {
                if !matches!(p.rule, SignRule::Threshold(_)) {
                    return Err(OwnError::NotAllowed);
                }
                removal(key)?;
                s.drop_owner(key);
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
                s.recovery.remove(&owner);
                if let Some(r) = recovery {
                    s.recovery.insert(owner, [(r, id)].into());
                }
            }
            Change::Challenge { key } => {
                removal(key)?;
                s.challenges.insert(key, id);
            }
            Change::RemoveDormant { key } => {
                let Some(&ch) = p.challenges.get(&key) else { return Err(OwnError::WindowOpen) };
                if signers.contains(&key) {
                    return Err(OwnError::WindowOpen);
                }
                removal(key)?;
                if effective {
                    self.completed(key, ch, id, OBJECTION_FRAMES)?;
                }
                s.drop_owner(key);
            }
            Change::StartRecover { owner, new_key } => {
                if !p.owners.contains(&owner) || p.owners.contains(&new_key) {
                    return Err(OwnError::NotOwner);
                }
                let Some((&by, &named)) = p.recovery.get(&owner).and_then(|r| r.iter().find(|(k, _)| signers.contains(k))) else {
                    return Err(OwnError::Signers);
                };
                // A recovery key the owner replaced or cleared, attested earlier in the same session,
                // is dead there. It counts only if some session attested it before any such rotation.
                if effective {
                    let rotated_before = |l: Lineage, at: u64| {
                        self.inner.certs.iter().any(|(e, x)| {
                            matches!(x.change, Change::SetRecovery { owner: o, .. } if o == owner)
                                && *e != named
                                && self.inner.anc[e].contains(&named)
                                && self.frame_in(*e, l).is_some_and(|f| f < at)
                        })
                    };
                    let alive = self.inner.commits.get(&id).is_some_and(|m| m.iter().any(|(&l, (f, _))| !rotated_before(l, *f)));
                    if !alive {
                        return Err(OwnError::Revoked);
                    }
                }
                s.recovering.insert(owner, Recovering { start: id, by, new_key });
            }
            Change::FinishRecover { owner } => {
                let Some(r) = p.recovering.get(&owner).copied() else { return Err(OwnError::WindowOpen) };
                if !signers.contains(&r.by) || p.owners.contains(&r.new_key) {
                    return Err(OwnError::Signers);
                }
                if effective {
                    self.completed(owner, r.start, id, RECOVERY_FRAMES)?;
                }
                s.drop_owner(owner);
                s.add_owner(r.new_key, id);
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
                Change::Leave { key } | Change::Remove { key } | Change::RemoveDormant { key } => Some(key),
                Change::FinishRecover { owner } => Some(owner),
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
        // A window stays open unless its target cancelled it in a lineage that attested its start.
        let open = |k: Key, start: CertId| {
            !self.inner.commits.get(&start).is_some_and(|m| m.iter().any(|(&l, (f, _))| self.signed_between(k, start, l, *f, u64::MAX)))
        };
        let mut challenges: BTreeMap<Key, CertId> = BTreeMap::new();
        let mut recovering: BTreeMap<Key, Recovering> = BTreeMap::new();
        for s in &st {
            for (&k, &ch) in &s.challenges {
                if owners.contains(&k) && open(k, ch) {
                    let e = challenges.entry(k).or_insert(ch);
                    *e = (*e).min(ch);
                }
            }
            for (&o, r) in &s.recovering {
                if owners.contains(&o) && open(o, r.start) && recovery.get(&o).is_some_and(|x| x.contains_key(&r.by)) {
                    let e = recovering.entry(o).or_insert(*r);
                    if r.start < e.start {
                        *e = *r;
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
            challenges,
            recovering,
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
                }
                Some(s)
            } else if c.prev.iter().any(|p| invalid.contains(p)) {
                None
            } else if c.change == Change::Merge {
                let mut m = self.join(&c.prev, &linear, &|x| !invalid.contains(&x));
                let ok = SignRule::Any.met(&signers[&id], &m.owners) && c.seq == m.seq + 1;
                m.seq = c.seq;
                ok.then_some(m)
            } else {
                self.step(&linear[&c.prev[0]], c, id, &signers[&id], false).ok()
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
                    a != b && self.concurrent(a, b) && self.inner.signers[&b].contains(&ta) && self.inner.signers[&a].contains(&tb)
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
        // OWNS-8, OWNT-8: disputed if two attested, valid certificates ever shared a parent.
        let mut by_prev: BTreeMap<CertId, usize> = BTreeMap::new();
        for (id, c) in &self.inner.certs {
            if c.prev.len() == 1 && self.inner.commits.contains_key(id) && !self.inner.invalid.contains(id) {
                *by_prev.entry(c.prev[0]).or_default() += 1;
            }
        }
        let disputed = by_prev.values().any(|&n| n > 1);
        let view = View { disputed, heads, owners, refused };
        let mut v = vec![self.world, self.inner.certs.len() as u64];
        v.extend(self.inner.certs.values().map(Cert::digest));
        for (c, m) in &self.inner.commits {
            v.push(*c);
            for (l, (f, s)) in m {
                v.extend([*l as u64, *f, s.len() as u64]);
                v.extend(s.iter().map(|&k| k as u64));
            }
        }
        v.push(view.owners.digest());
        let digest = fnv_u64s(v);
        let inner = Arc::make_mut(&mut self.inner);
        inner.view = view;
        inner.digest = digest;
        inner.eff = eff;
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
                if inner.invalid.contains(&id) || !SignRule::Any.met(&inner.signers[&id], &m.owners) {
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
                } else if !self.approved_elsewhere(id, c, base, &counted, &eff) {
                    Err(OwnError::Signers)
                } else {
                    self.step(base, c, id, &inner.signers[&id], true)
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
    fn approved_elsewhere(&self, id: CertId, c: &Cert, base: &Owners, counted: &BTreeSet<CertId>, eff: &BTreeMap<CertId, Owners>) -> bool {
        let target = match c.change {
            Change::Challenge { key } | Change::RemoveDormant { key } | Change::Remove { key } => key,
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
                Change::FinishRecover { owner } => eff[&self.parent(d).unwrap()].recovering.get(&owner).map(|r| r.new_key),
                _ => None,
            })
            .collect();
        if added.is_empty() {
            return true;
        }
        let mut need: BTreeSet<Key> = base.owners.union(&added).copied().collect();
        need.remove(&target);
        base.rule.met(&self.inner.signers[&id], &need)
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
        self.inner.certs.contains_key(&id) && !self.inner.view.refused.iter().any(|(r, _)| *r == id)
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

    /// For the state hash: every held certificate (with signatures), the attested commits and the view.
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
    pub world: u64,
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
