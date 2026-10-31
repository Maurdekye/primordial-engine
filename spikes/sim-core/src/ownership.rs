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

    /// The certificate's id: its body and the set of keys that validly signed it (OWNR-4).
    pub fn id(&self) -> CertId {
        let s = self.signers();
        fnv_u64s([self.body_hash(), s.len() as u64].into_iter().chain(s.iter().map(|&k| k as u64)))
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

/// "Session `lineage` (in membership epoch `epoch`) committed certificate `cert` of world `world` at
/// world frame `frame`", signed by a strict majority of that session's members. Production: P2-4's
/// epoch certificates over a session id derived from its genesis [I].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attest {
    pub cert: CertId,
    pub lineage: Lineage,
    pub epoch: u32,
    pub frame: u64,
    pub sigs: Vec<(MemberId, u64)>,
}

impl Attest {
    pub fn fact(world: u64, cert: CertId, lineage: Lineage, frame: u64) -> u64 {
        fnv_u64s([0x00A7_7E57, world, cert, lineage as u64, frame])
    }
    /// Signed by every member given (spike signatures).
    pub fn make(world: u64, cert: CertId, lineage: Lineage, frame: u64, members: &[MemberId]) -> Attest {
        let f = Attest::fact(world, cert, lineage, frame);
        Attest { cert, lineage, epoch: 0, frame, sigs: members.iter().map(|&m| (m, member_sig(lineage, m, f))).collect() }
    }
    pub fn verify(&self, world: u64, sessions: &dyn EpochCerts) -> bool {
        sessions.cert_ok(self.lineage, self.epoch, Attest::fact(world, self.cert, self.lineage, self.frame), &self.sigs)
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
    /// The signatures do not satisfy the rule of the owner set at the parent (including owners
    /// added on concurrent branches, OWN-3).
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
    /// A session commits only certificates built on its current head.
    Stale,
    /// Two concurrent removals each remove a signer of the other (OWNR-7).
    Conflict,
    /// The recovery key had been replaced, attested, before it acted.
    Revoked,
    /// The book holds [`MAX_CERTS`] certificates.
    Full,
}

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
    /// The certificate that set `rule` (genesis or a `SetRule`).
    pub rule_tag: CertId,
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

    pub fn digest(&self) -> u64 {
        let mut v = vec![self.rule.code(), self.rule_tag, self.seq as u64, self.owners.len() as u64];
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
    pub heads: Vec<CertId>,
    /// More than one head: show "Ownership disputed" until a merge covers them.
    pub disputed: bool,
    pub owners: Owners,
    pub refused: Vec<(CertId, OwnError)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct Inner {
    certs: BTreeMap<CertId, Cert>,
    signers: BTreeMap<CertId, BTreeSet<Key>>,
    /// [`Cert::digest`] of each, computed once (OWNR-6).
    digests: BTreeMap<CertId, u64>,
    /// Each certificate's state along its own history (validity).
    linear: BTreeMap<CertId, Owners>,
    /// Each certificate's causal past, itself included. Production: an ancestry index [I].
    anc: BTreeMap<CertId, BTreeSet<CertId>>,
    /// Attested commits: certificate -> lineage -> world frame (the earliest attested).
    commits: BTreeMap<CertId, BTreeMap<Lineage, u64>>,
    /// Cached results of the effective pass.
    view: View,
    digest: u64,
}

/// What one verifier holds of a world's ownership. Cloning shares everything until the next insert
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
        let mut s = Owners { rule: *rule, rule_tag: id, ..Default::default() };
        for &k in &set {
            s.add_owner(k, id);
        }
        let inner = Inner {
            signers: [(id, genesis.signers())].into(),
            digests: [(id, genesis.digest())].into(),
            certs: [(id, genesis)].into(),
            linear: [(id, s)].into(),
            anc: [(id, [id].into())].into(),
            ..Default::default()
        };
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

    /// Store `c` if it is valid against its parent's state. It counts only once attested.
    pub fn insert(&mut self, c: Cert) -> Result<Insert, OwnError> {
        let r = self.insert_raw(c)?;
        if r == Insert::Applied {
            self.recompute();
        }
        Ok(r)
    }

    /// The local session committed `id` at world `frame` (its own record; exported as an [`Attest`]).
    pub fn record(&mut self, id: CertId, lineage: Lineage, frame: u64) {
        if self.inner.certs.contains_key(&id) {
            let inner = Arc::make_mut(&mut self.inner);
            let f = inner.commits.entry(id).or_default().entry(lineage).or_insert(frame);
            *f = (*f).min(frame);
            self.recompute();
        }
    }

    /// Import another session's attestation of a held certificate, if its signatures check out.
    pub fn attest(&mut self, a: &Attest, sessions: &dyn EpochCerts) -> bool {
        let ok = self.inner.certs.contains_key(&a.cert) && a.verify(self.world, sessions);
        if ok {
            self.record(a.cert, a.lineage, a.frame);
        }
        ok
    }

    /// The attestations of every certificate session `lineage` committed in this book.
    pub fn export(&self, lineage: Lineage, members: &[MemberId]) -> Vec<Attest> {
        self.inner
            .commits
            .iter()
            .filter_map(|(c, m)| m.get(&lineage).map(|&f| Attest::make(self.world, *c, lineage, f, members)))
            .collect()
    }

    /// A session commits `c` at world `frame`: only on its current head (or a merge of heads), and
    /// only if it then counts. Otherwise nothing changes.
    pub fn commit_local(&mut self, c: Cert, lineage: Lineage, frame: u64) -> Result<Insert, OwnError> {
        if self.inner.certs.contains_key(&c.id()) {
            return Ok(Insert::Duplicate);
        }
        let heads = self.heads();
        let known = c.prev.iter().all(|p| self.inner.certs.contains_key(p));
        if known && !c.prev.is_empty() && !c.prev.iter().all(|p| heads.contains(p)) {
            return Err(OwnError::Stale);
        }
        let mut b = self.clone();
        let id = c.id();
        if b.insert_raw(c)? == Insert::Duplicate {
            return Ok(Insert::Duplicate);
        }
        b.record(id, lineage, frame);
        if let Some(&(_, e)) = b.inner.view.refused.iter().find(|(r, _)| *r == id) {
            return Err(e);
        }
        *self = b;
        Ok(Insert::Applied)
    }

    fn insert_raw(&mut self, c: Cert) -> Result<Insert, OwnError> {
        if c.world != self.world {
            return Err(OwnError::WrongWorld);
        }
        let id = c.id();
        if self.inner.certs.contains_key(&id) {
            return Ok(Insert::Duplicate);
        }
        if self.inner.certs.len() >= MAX_CERTS {
            return Err(OwnError::Full);
        }
        if c.prev.is_empty() {
            return Err(OwnError::BadGenesis);
        }
        if !c.prev.iter().all(|p| self.inner.certs.contains_key(p)) {
            return Err(OwnError::MissingParent);
        }
        let signers = c.signers();
        let s = if c.change == Change::Merge {
            let mut p = c.prev.clone();
            p.sort_unstable();
            p.dedup();
            // OWN-1: only certificates that really diverged can be joined.
            let concurrent = p.iter().enumerate().all(|(i, a)| p[i + 1..].iter().all(|b| self.concurrent(*a, *b)));
            if p.len() < 2 || p != c.prev || !concurrent {
                return Err(OwnError::BadLink);
            }
            let mut m = self.join(&p, &self.inner.linear, &|_| true);
            if c.seq != m.seq + 1 {
                return Err(OwnError::BadLink);
            }
            if !SignRule::Any.met(&signers, &m.owners) {
                return Err(OwnError::Signers);
            }
            m.seq = c.seq;
            m
        } else {
            if c.prev.len() != 1 {
                return Err(OwnError::BadLink);
            }
            self.step(&self.inner.linear[&c.prev[0]], &c, id, &signers, false)?
        };
        let mut anc: BTreeSet<CertId> = c.prev.iter().flat_map(|p| self.inner.anc[p].iter().copied()).collect();
        anc.insert(id);
        let inner = Arc::make_mut(&mut self.inner);
        inner.anc.insert(id, anc);
        inner.linear.insert(id, s);
        inner.signers.insert(id, signers);
        inner.digests.insert(id, c.digest());
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

    fn first_frame(&self, id: CertId) -> Option<u64> {
        self.inner.commits.get(&id).and_then(|m| m.values().min().copied())
    }

    /// The window from `start` to `end` ran `frames` in one session lineage that attested both: the
    /// frames of both ends there.
    fn window(&self, start: CertId, end: CertId, frames: u64) -> Result<(u64, u64), OwnError> {
        let (Some(s), Some(e)) = (self.inner.commits.get(&start), self.inner.commits.get(&end)) else { return Err(OwnError::Unattested) };
        let common: Vec<(u64, u64)> = s.iter().filter_map(|(l, &fs)| e.get(l).map(|&fe| (fs, fe))).collect();
        if common.is_empty() {
            return Err(OwnError::Unattested);
        }
        common.into_iter().find(|&(fs, fe)| fe >= fs.saturating_add(frames)).ok_or(OwnError::WindowOpen)
    }

    /// `k` signed a certificate outside `start`'s past that some session attested at a world frame
    /// in `[from, to)`: the window was cancelled (OWNR-2: unattested signatures, and signatures after
    /// the completion, never count).
    fn signed_between(&self, k: Key, start: CertId, from: u64, to: u64) -> bool {
        self.inner.commits.iter().any(|(e, m)| {
            *e != start && !self.inner.anc[&start].contains(e) && self.inner.signers[e].contains(&k) && m.values().any(|&f| f >= from && f < to)
        })
    }

    /// The state after `c` (id `id`), whose single parent left state `p`. `effective`: also check what
    /// depends on attestations (windows, cancellation, revocation); validity alone leaves them to the
    /// effective pass.
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
            if p.rule == SignRule::Any {
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
                s.rule_tag = id;
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
                    let (from, to) = self.window(ch, id, OBJECTION_FRAMES)?;
                    if self.signed_between(key, ch, from, to) {
                        return Err(OwnError::WindowOpen);
                    }
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
                // A recovery key the owner replaced or cleared, attested before it acted, is dead.
                if effective {
                    let at = self.first_frame(id).unwrap_or(u64::MAX);
                    let revoked = self.inner.certs.iter().any(|(e, x)| {
                        matches!(x.change, Change::SetRecovery { owner: o, .. } if o == owner)
                            && *e != named
                            && self.inner.anc[e].contains(&named)
                            && self.first_frame(*e).is_some_and(|f| f < at)
                    });
                    if revoked {
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
                    let (from, to) = self.window(r.start, id, RECOVERY_FRAMES)?;
                    if self.signed_between(owner, r.start, from, to) {
                        return Err(OwnError::WindowOpen);
                    }
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
            let ps = &states[&p];
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
        if tags.is_empty() {
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
        // A window stays open unless its target signed an attested certificate since it opened.
        let open = |k: Key, start: CertId| !self.signed_between(k, start, self.first_frame(start).unwrap_or(0), u64::MAX);
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
        // The rule: the newest by causality; among concurrent ones, the strictest (OWNR-9).
        let newest: Vec<&&Owners> =
            st.iter().filter(|s| !st.iter().any(|o| o.rule_tag != s.rule_tag && self.inner.anc[&o.rule_tag].contains(&s.rule_tag))).collect();
        let (rule, rule_tag) = newest.iter().map(|s| (s.rule, s.rule_tag)).max_by_key(|&(r, t)| (r, std::cmp::Reverse(t))).unwrap();
        Owners {
            owners,
            rule,
            rule_tag,
            tags,
            recovery,
            challenges,
            recovering,
            seq: st.iter().map(|s| s.seq).max().unwrap_or(0),
        }
    }

    /// The effective pass (OWNR-1): every held certificate in causal order, applied to its parent's
    /// effective state only if it counts there.
    fn recompute(&mut self) {
        let inner = &self.inner;
        let mut order: Vec<CertId> = inner.certs.keys().copied().collect();
        order.sort_by_key(|id| (inner.certs[id].seq, *id));
        // OWNR-7: concurrent removals that each remove a signer of the other void each other.
        let removals: Vec<CertId> = order.iter().copied().filter(|id| inner.certs[id].change.removes().is_some()).collect();
        let voided: BTreeSet<CertId> = removals
            .iter()
            .copied()
            .filter(|&a| {
                removals.iter().any(|&b| {
                    let (ta, tb) = (inner.certs[&a].change.removes().unwrap(), inner.certs[&b].change.removes().unwrap());
                    a != b && self.concurrent(a, b) && inner.signers[&b].contains(&ta) && inner.signers[&a].contains(&tb)
                })
            })
            .collect();
        let mut eff: BTreeMap<CertId, Owners> = BTreeMap::new();
        let mut counted: BTreeSet<CertId> = BTreeSet::new();
        let mut refused: Vec<(CertId, OwnError)> = Vec::new();
        for &id in &order {
            let c = &inner.certs[&id];
            let s = if c.prev.is_empty() {
                counted.insert(id);
                inner.linear[&id].clone()
            } else if c.change == Change::Merge {
                counted.insert(id);
                let mut m = self.join(&c.prev, &eff, &|x| counted.contains(&x));
                m.seq = c.seq;
                m
            } else {
                let base = &eff[&c.prev[0]];
                let r = if !inner.commits.contains_key(&id) {
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
        let parents: BTreeSet<CertId> = inner.certs.values().flat_map(|c| c.prev.iter().copied()).collect();
        let heads: Vec<CertId> = inner.certs.keys().copied().filter(|id| !parents.contains(id)).collect();
        let owners = match &heads[..] {
            [h] => eff[h].clone(),
            hs => self.join(hs, &eff, &|x| counted.contains(&x)),
        };
        let view = View { disputed: heads.len() > 1, heads, owners, refused };
        let mut v = vec![self.world, inner.certs.len() as u64];
        v.extend(inner.digests.values());
        v.extend(inner.commits.iter().flat_map(|(c, m)| std::iter::once(*c).chain(m.iter().flat_map(|(&l, &f)| [l as u64, f]))));
        v.push(view.owners.digest());
        let digest = fnv_u64s(v);
        let inner = Arc::make_mut(&mut self.inner);
        inner.view = view;
        inner.digest = digest;
    }

    /// OWN-3: a change made on one branch also needs the owners that counting additions on concurrent
    /// branches added since it split off (their parent is a strict ancestor of its parent).
    fn approved_elsewhere(&self, id: CertId, c: &Cert, base: &Owners, counted: &BTreeSet<CertId>, eff: &BTreeMap<CertId, Owners>) -> bool {
        let target = match c.change {
            Change::Add { .. } | Change::SetRule { .. } => None,
            Change::Challenge { key } | Change::RemoveDormant { key } | Change::Remove { key } => Some(key),
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
        if let Some(t) = target {
            need.remove(&t);
        }
        let signers = &self.inner.signers[&id];
        match c.change {
            Change::SetRule { .. } => SignRule::All.met(signers, &need),
            _ => base.rule.met(signers, &need),
        }
    }

    /// The heads: held certificates with no held child.
    pub fn heads(&self) -> Vec<CertId> {
        self.inner.view.heads.clone()
    }

    pub fn view(&self) -> View {
        self.inner.view.clone()
    }

    /// Does `id` count (held, attested, valid at its effective parent)?
    pub fn counts(&self, id: CertId) -> bool {
        self.inner.certs.contains_key(&id) && !self.inner.view.refused.iter().any(|(r, _)| *r == id)
    }

    /// Every pair of held certificates with one parent and different changes: concurrent changes,
    /// and when a key signed both, a transferable equivocation proof against it.
    pub fn forks(&self) -> Vec<ForkProof> {
        let mut by_prev: BTreeMap<CertId, Vec<&Cert>> = BTreeMap::new();
        for c in self.inner.certs.values().filter(|c| c.prev.len() == 1) {
            by_prev.entry(c.prev[0]).or_default().push(c);
        }
        let mut out = Vec::new();
        for v in by_prev.values() {
            for (i, a) in v.iter().enumerate() {
                for b in &v[i + 1..] {
                    if a.body_hash() != b.body_hash() {
                        out.push(ForkProof { world: self.world, a: (*a).clone(), b: (*b).clone() });
                    }
                }
            }
        }
        out
    }

    /// The whole causal past of `id` (itself included), oldest first: a verifier's prefix.
    pub fn prefix_of(&self, id: CertId) -> Vec<Cert> {
        let mut v: Vec<&Cert> = self.inner.anc[&id].iter().map(|a| &self.inner.certs[a]).collect();
        v.sort_by_key(|c| (c.seq, c.id()));
        v.into_iter().cloned().collect()
    }

    /// For the state hash: every held certificate (with signatures), the attested commits and the view.
    pub fn digest(&self) -> u64 {
        self.inner.digest
    }
}

/// Insert certificates in any order: each round inserts what has its parents. Stops at the first
/// refusal; leftovers whose parents never arrive are `MissingParent`.
fn insert_all(book: &mut Book, certs: &[Cert]) -> Result<(), OwnError> {
    let mut todo: Vec<&Cert> = certs.iter().collect();
    while !todo.is_empty() {
        let n = todo.len();
        let mut rest = Vec::new();
        for c in todo {
            match book.insert(c.clone()) {
                Ok(_) => {}
                Err(OwnError::MissingParent) => rest.push(c),
                Err(e) => return Err(e),
            }
        }
        if rest.len() == n {
            return Err(OwnError::MissingParent);
        }
        todo = rest;
    }
    Ok(())
}

/// Check a whole chain offline, in any order, with the sessions' attestations (each checked against
/// `sessions`' members; a forged one is ignored). It must be complete; the answer may be disputed,
/// and lists what doesn't count.
pub fn verify_chain(genesis: &Cert, rest: &[Cert], attests: &[Attest], sessions: &dyn EpochCerts) -> Result<View, OwnError> {
    let mut b = Book::new(genesis.clone())?;
    insert_all(&mut b, rest)?;
    for a in attests {
        b.attest(a, sessions);
    }
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
        let rest: Vec<Cert> = prefix.iter().filter(|c| c.seq > 0).cloned().collect();
        book.world == self.world
            && insert_all(&mut book, &rest).is_ok()
            && self.a.prev.len() == 1
            && self.a.prev == self.b.prev
            && self.a.body_hash() != self.b.body_hash()
            && book.insert(self.a.clone()) == Ok(Insert::Applied)
            && book.insert(self.b.clone()) == Ok(Insert::Applied)
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
