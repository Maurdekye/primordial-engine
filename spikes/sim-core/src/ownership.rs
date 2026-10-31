//! World ownership as a chain of signed certificates (user ruling 2026-10-30 22:39Z, "Signed chain,
//! no blockchain"; design 12.5). Ownership is a lasting property of a world, separate from a session's
//! admins, and can be shared. It needs no registry or service: anyone can verify the chain offline.
//!
//! * **Genesis.** Signed by every creator. The world id is the hash of the genesis certificate (in
//!   production, of the genesis document that contains it), so the original owners can't be
//!   rewritten without changing the world's identity.
//! * **Changes.** Each later certificate names its parent and is valid only if its signatures satisfy
//!   the rule of the owner set *at the parent*. Signers outside that set never count. Validity is a
//!   function of a certificate's causal past alone (review OWN-5), never of arrival order.
//! * **Rule A** (22:54Z, O1): adding an owner needs every owner; anyone may leave, except the last
//!   owner. Nobody is ever removed against their will while active (O2, OWN-4): removal is a 30-day
//!   objection window that every other owner opens and that any signature of the target cancels. A
//!   recovery key only replaces its owner's lost key, after a 7-day delay that the owner's own
//!   signature cancels (OWN-6). Windows count session time that a session recorded when it committed
//!   each certificate, never a time a signer chose (OWN-7), so they can't be checked offline.
//!   Rules B (any owner adds; no removal) and C (k of n, with instant removal) are kept for comparison.
//! * **Forks** (two valid certificates with one parent) are detect-only, as D79: a [`ForkProof`]
//!   convinces any third party holding the prefix. The owner set after a fork is a **union** (O4) that
//!   is a function of the set of certificates held ([`Book::view`]):
//!   * an owner is in it if some certificate that added them (genesis included) has not been seen by a
//!     removal of them (Leave, a completed window, a recovery that replaced them): observed-remove, so
//!     a Leave or removal sticks unless the same history adds the person back (OWN-2);
//!   * a change made on one branch counts only if the owners that concurrent branches added approve
//!     it too (OWN-3): a removal needs every one of them; an addition only those added after its own
//!     branch left theirs, so additions right at a fork point (a double sale) still make both buyers
//!     co-owners;
//!   * a merge certificate joins concurrent heads only (OWN-1) and invalidates nothing.
//!
//! What a fork can still do [I]: keys that once held full authority over the world can add a co-owner
//! by signing on that old state. It is always provable, and the world shows "Ownership disputed".
//! Preventing it needs a global order (a registry), which the user rejected (D79).
//!
//! Spike signatures are keyless hashes ([`key_sig`]) standing in for Ed25519, as `member_sig` does
//! for world members.

use std::collections::{BTreeMap, BTreeSet};

use crate::{fnv_u64s, splitmix64};

/// A player's long-term identity key (design 10.3). Spike: a number.
pub type Key = u32;
pub type CertId = u64;

/// The objection window before an owner can be removed: 30 days of session time at 60 Hz.
pub const OBJECTION_FRAMES: u64 = 30 * 24 * 3600 * 60;
/// The delay before a recovery key's replacement of its owner's key completes: 7 days.
pub const RECOVERY_FRAMES: u64 = 7 * 24 * 3600 * 60;

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
    /// Joins concurrent heads: the chain is linear again. Its owner set is forced (the union).
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
    /// The certificate's id: everything but the signatures, which is also what each key signs.
    pub fn id(&self) -> CertId {
        fnv_u64s(self.body())
    }

    fn body(&self) -> Vec<u64> {
        let mut v = vec![self.world, self.seq as u64, self.prev.len() as u64];
        v.extend(&self.prev);
        self.change.words(&mut v);
        v
    }

    pub fn signed(mut self, keys: &[Key]) -> Cert {
        let id = self.id();
        self.sigs.extend(keys.iter().map(|&k| (k, key_sig(k, id))));
        self
    }

    /// The keys with a valid signature on this certificate.
    pub fn signers(&self) -> BTreeSet<Key> {
        let id = self.id();
        self.sigs.iter().filter(|&&(k, s)| s == key_sig(k, id)).map(|&(k, _)| k).collect()
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

/// Why a certificate is refused.
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
    /// The signatures do not satisfy the rule of the owner set at the parent.
    Signers,
    AlreadyOwner,
    NotOwner,
    /// The last owner can't leave or be removed.
    LastOwner,
    /// Not under this rule (instant removal outside rule C, any removal under rule B), or no rule.
    NotAllowed,
    /// No open window, or it hasn't run yet.
    WindowOpen,
    /// A window's length can be checked only against session time a session recorded for both
    /// certificates (OWN-7); offline, or without the start's time, it can't.
    Unattested,
}

/// A recovery in progress: `by` started replacing its owner with `new_key` at certificate `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recovering {
    pub start: CertId,
    pub by: Key,
    pub new_key: Key,
}

/// The owner set and its rules after some certificate (or the union of several heads).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Owners {
    pub owners: BTreeSet<Key>,
    pub rule: SignRule,
    /// Each owner's recovery keys, with the certificate that named each (normally at most one; a
    /// fork can leave two, both named by that owner).
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

    fn drop_owner(&mut self, key: Key) {
        self.owners.remove(&key);
        self.recovery.remove(&key);
        self.challenges.remove(&key);
        self.recovering.remove(&key);
    }

    pub fn digest(&self) -> u64 {
        let mut v = vec![self.rule.code(), self.seq as u64, self.owners.len() as u64];
        v.extend(self.owners.iter().map(|&k| k as u64));
        for (o, r) in &self.recovery {
            v.extend([*o as u64, r.len() as u64]);
            v.extend(r.iter().flat_map(|(&k, &c)| [k as u64, c]));
        }
        v.extend(self.challenges.iter().flat_map(|(&k, &c)| [k as u64 | 1 << 40, c]));
        v.extend(self.recovering.iter().flat_map(|(&o, r)| [o as u64 | 1 << 41, r.start, r.by as u64, r.new_key as u64]));
        fnv_u64s(v)
    }
}

/// What one verifier holds of a world's ownership: every valid certificate it has seen, and the
/// session time at which a session committed each (if one did).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Book {
    pub world: u64,
    pub genesis: CertId,
    certs: BTreeMap<CertId, Cert>,
    states: BTreeMap<CertId, Owners>,
    /// Each certificate's causal past, itself included.
    anc: BTreeMap<CertId, BTreeSet<CertId>>,
    /// Session time (frame) at which a session committed the certificate. Production: the session's
    /// commit attestation travels with the certificate [I].
    times: BTreeMap<CertId, u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    Applied,
    Duplicate,
}

/// A book's current answer: the heads, and the owner set the world acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub heads: Vec<CertId>,
    /// More than one head: show "Ownership disputed" until a merge covers them.
    pub disputed: bool,
    pub owners: Owners,
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
        let s = Owners { owners: set, rule: *rule, ..Default::default() };
        Ok(Book {
            world: id,
            genesis: id,
            certs: [(id, genesis)].into(),
            states: [(id, s)].into(),
            anc: [(id, [id].into())].into(),
            times: BTreeMap::new(),
        })
    }

    pub fn cert(&self, id: CertId) -> Option<&Cert> {
        self.certs.get(&id)
    }

    pub fn len(&self) -> usize {
        self.certs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.certs.is_empty()
    }

    /// Offline: no session time.
    pub fn insert(&mut self, c: Cert) -> Result<Insert, OwnError> {
        self.insert_at(c, None)
    }

    /// Accept `c` if it is valid against the state at its parents. `at` is the session time at which a
    /// session commits it (None offline). Refused certificates leave no trace.
    pub fn insert_at(&mut self, c: Cert, at: Option<u64>) -> Result<Insert, OwnError> {
        if c.world != self.world {
            return Err(OwnError::WrongWorld);
        }
        let id = c.id();
        if self.certs.contains_key(&id) {
            if let Some(t) = at {
                self.times.entry(id).or_insert(t);
            }
            return Ok(Insert::Duplicate);
        }
        if c.prev.is_empty() {
            return Err(OwnError::BadGenesis);
        }
        if !c.prev.iter().all(|p| self.certs.contains_key(p)) {
            return Err(OwnError::MissingParent);
        }
        let s = if c.change == Change::Merge {
            let mut p = c.prev.clone();
            p.sort_unstable();
            p.dedup();
            // OWN-1: only heads that really diverged can be joined.
            let concurrent = p.iter().enumerate().all(|(i, a)| p[i + 1..].iter().all(|b| self.concurrent(*a, *b)));
            if p.len() < 2 || p != c.prev || !concurrent {
                return Err(OwnError::BadLink);
            }
            let mut m = self.union(&p);
            if c.seq != m.seq + 1 {
                return Err(OwnError::BadLink);
            }
            if !SignRule::Any.met(&c.signers(), &m.owners) {
                return Err(OwnError::Signers);
            }
            m.seq = c.seq;
            m
        } else {
            if c.prev.len() != 1 {
                return Err(OwnError::BadLink);
            }
            self.step(&self.states[&c.prev[0]], &c, at)?
        };
        let mut anc: BTreeSet<CertId> = c.prev.iter().flat_map(|p| self.anc[p].iter().copied()).collect();
        anc.insert(id);
        self.anc.insert(id, anc);
        self.states.insert(id, s);
        self.certs.insert(id, c);
        if let Some(t) = at {
            self.times.insert(id, t);
        }
        Ok(Insert::Applied)
    }

    fn concurrent(&self, a: CertId, b: CertId) -> bool {
        !self.anc[&a].contains(&b) && !self.anc[&b].contains(&a)
    }

    /// A window that opened at `start` has run at session time `at` (OWN-7).
    fn window_ran(&self, start: CertId, at: Option<u64>, frames: u64) -> Result<(), OwnError> {
        match (self.times.get(&start), at) {
            (Some(&t0), Some(t)) if t >= t0.saturating_add(frames) => Ok(()),
            (Some(_), Some(_)) => Err(OwnError::WindowOpen),
            _ => Err(OwnError::Unattested),
        }
    }

    /// The state after `c`, whose single parent left state `p`.
    fn step(&self, p: &Owners, c: &Cert, at: Option<u64>) -> Result<Owners, OwnError> {
        if c.seq != p.seq + 1 {
            return Err(OwnError::BadLink);
        }
        let id = c.id();
        let signers = c.signers();
        let mut s = p.clone();
        // A signature by an owner cancels any window against it.
        s.challenges.retain(|k, _| !signers.contains(k));
        s.recovering.retain(|k, _| !signers.contains(k));
        // Removals other than leaving: the other owners under the rule (instant under C only).
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
            if !p.rule.met(&signers, &p.others(key)) {
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
                if !p.rule.met(&signers, &p.owners) {
                    return Err(OwnError::Signers);
                }
                s.owners.insert(key);
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
                if !SignRule::All.met(&signers, &p.owners) {
                    return Err(OwnError::Signers);
                }
                s.rule = rule;
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
                self.window_ran(ch, at, OBJECTION_FRAMES)?;
                s.drop_owner(key);
            }
            Change::StartRecover { owner, new_key } => {
                if !p.owners.contains(&owner) || p.owners.contains(&new_key) {
                    return Err(OwnError::NotOwner);
                }
                let Some(&by) = p.recovery.get(&owner).and_then(|r| r.keys().find(|k| signers.contains(k))) else {
                    return Err(OwnError::Signers);
                };
                s.recovering.insert(owner, Recovering { start: id, by, new_key });
            }
            Change::FinishRecover { owner } => {
                let Some(r) = p.recovering.get(&owner) else { return Err(OwnError::WindowOpen) };
                if !signers.contains(&r.by) || p.owners.contains(&r.new_key) {
                    return Err(OwnError::Signers);
                }
                self.window_ran(r.start, at, RECOVERY_FRAMES)?;
                let n = r.new_key;
                s.drop_owner(owner);
                s.owners.insert(n);
            }
        }
        // A recovery key never stays one once it is an owner itself.
        for r in s.recovery.values_mut() {
            r.retain(|k, _| !s.owners.contains(k));
        }
        s.recovery.retain(|_, r| !r.is_empty());
        s.seq = c.seq;
        Ok(s)
    }

    /// The union of several heads (O4): a function of the certificates in their causal past only.
    fn union(&self, heads: &[CertId]) -> Owners {
        let all: BTreeSet<CertId> = heads.iter().flat_map(|h| self.anc[h].iter().copied()).collect();
        let seen = |r: CertId, t: CertId| r != t && self.anc[&r].contains(&t);
        let parent = |id: CertId| self.certs[&id].prev.first().copied();
        // The key a valid certificate adds, if any.
        let adds = |id: CertId| match self.certs[&id].change {
            Change::Add { key } => Some(key),
            Change::FinishRecover { owner } => self.states[&parent(id).unwrap()].recovering.get(&owner).map(|r| r.new_key),
            _ => None,
        };
        // OWN-3: owners that branches concurrent with `id` added. For an addition, only those added
        // after `id`'s branch left theirs (by a certificate whose parent is a strict ancestor of `id`'s
        // parent), so additions right at a fork point, such as a double sale, don't need each other. A
        // removal needs every owner any concurrent branch added: it takes something away.
        let added_elsewhere = |id: CertId, removal: bool| -> BTreeSet<Key> {
            let p = parent(id).unwrap();
            all.iter()
                .copied()
                .filter(|&d| self.concurrent(d, id))
                .filter(|&d| removal || parent(d).is_some_and(|dp| dp != p && self.anc[&p].contains(&dp)))
                .filter_map(adds)
                .collect()
        };
        // A target signed something after its window opened, or on another branch.
        let cancelled = |k: Key, start: CertId| {
            all.iter().any(|&e| e != start && !self.anc[&start].contains(&e) && self.certs[&e].signers().contains(&k))
        };
        // Recovery keys: observed-remove over SetRecovery.
        let mut recovery: BTreeMap<Key, BTreeMap<Key, CertId>> = BTreeMap::new();
        for &x in &all {
            if let Change::SetRecovery { owner, recovery: Some(r) } = self.certs[&x].change {
                let cleared = all.iter().any(|&y| matches!(self.certs[&y].change, Change::SetRecovery { owner: o, .. } if o == owner) && seen(y, x));
                if !cleared {
                    recovery.entry(owner).or_default().insert(r, x);
                }
            }
        }
        // Does a certificate's effect count in the union?
        let counts = |id: CertId| -> bool {
            let c = &self.certs[&id];
            let Some(p) = parent(id).filter(|_| c.prev.len() == 1) else { return true };
            let ps = &self.states[&p];
            let signers = c.signers();
            let need = |target: Option<Key>| -> BTreeSet<Key> {
                let mut n: BTreeSet<Key> = ps.owners.union(&added_elsewhere(id, target.is_some())).copied().collect();
                if let Some(t) = target {
                    n.remove(&t);
                }
                n
            };
            match c.change {
                Change::Add { .. } => ps.rule.met(&signers, &need(None)),
                Change::Challenge { key } | Change::Remove { key } => ps.rule.met(&signers, &need(Some(key))),
                Change::RemoveDormant { key } => {
                    ps.rule.met(&signers, &need(Some(key))) && ps.challenges.get(&key).is_some_and(|&ch| !cancelled(key, ch))
                }
                Change::FinishRecover { owner } => ps.recovering.get(&owner).is_some_and(|r| {
                    !cancelled(owner, r.start) && recovery.get(&owner).is_some_and(|keys| keys.contains_key(&r.by))
                }),
                _ => true,
            }
        };
        let counting: BTreeSet<CertId> = all.iter().copied().filter(|&id| counts(id)).collect();
        // Owners: observed-remove (OWN-2). Tags are the certificates that added each key.
        let mut tags: Vec<(Key, CertId)> = Vec::new();
        let mut removals: Vec<(Key, CertId)> = Vec::new();
        for &id in &counting {
            let c = &self.certs[&id];
            match c.change {
                Change::Genesis { ref owners, .. } => tags.extend(owners.iter().map(|&k| (k, id))),
                Change::Add { key } => tags.push((key, id)),
                Change::Leave { key } | Change::Remove { key } | Change::RemoveDormant { key } => removals.push((key, id)),
                Change::FinishRecover { owner } => {
                    removals.push((owner, id));
                    tags.extend(adds(id).map(|n| (n, id)));
                }
                _ => {}
            }
        }
        let present = |k: Key| tags.iter().any(|&(tk, t)| tk == k && !removals.iter().any(|&(rk, r)| rk == k && seen(r, t)));
        let mut owners: BTreeSet<Key> = tags.iter().map(|&(k, _)| k).filter(|&k| present(k)).collect();
        if owners.is_empty() {
            // Owners who removed each other on two copies of the world: nobody is left out.
            owners = heads.iter().flat_map(|h| self.states[h].owners.iter().copied()).collect();
        }
        recovery.retain(|o, r| {
            r.retain(|k, _| !owners.contains(k));
            owners.contains(o) && !r.is_empty()
        });
        // Open windows: started by a certificate that counts, not cancelled anywhere, target present.
        let latest = |a: CertId, b: CertId| if (self.times.get(&a), a) >= (self.times.get(&b), b) { a } else { b };
        let mut challenges: BTreeMap<Key, CertId> = BTreeMap::new();
        let mut recovering: BTreeMap<Key, Recovering> = BTreeMap::new();
        for &id in &counting {
            match self.certs[&id].change {
                Change::Challenge { key } if owners.contains(&key) && !cancelled(key, id) => {
                    let e = challenges.entry(key).or_insert(id);
                    *e = latest(*e, id);
                }
                Change::StartRecover { owner, .. } if owners.contains(&owner) && !cancelled(owner, id) => {
                    let r = self.states[&id].recovering[&owner];
                    if recovery.get(&owner).is_some_and(|k| k.contains_key(&r.by)) {
                        let e = recovering.entry(owner).or_insert(r);
                        if latest(e.start, id) == id {
                            *e = r;
                        }
                    }
                }
                _ => {}
            }
        }
        Owners {
            owners,
            rule: heads.iter().map(|h| self.states[h].rule).max().unwrap_or_default(),
            recovery,
            challenges,
            recovering,
            seq: heads.iter().map(|h| self.states[h].seq).max().unwrap_or(0),
        }
    }

    /// The heads: held certificates with no held child.
    pub fn heads(&self) -> Vec<CertId> {
        let parents: BTreeSet<CertId> = self.certs.values().flat_map(|c| c.prev.iter().copied()).collect();
        self.certs.keys().copied().filter(|id| !parents.contains(id)).collect()
    }

    pub fn view(&self) -> View {
        let heads = self.heads();
        let owners = match &heads[..] {
            [h] => self.states[h].clone(),
            hs => self.union(hs),
        };
        View { disputed: heads.len() > 1, heads, owners }
    }

    /// Every pair of held certificates with one parent: concurrent changes, and when a key signed both,
    /// a transferable equivocation proof against it.
    pub fn forks(&self) -> Vec<ForkProof> {
        let mut by_prev: BTreeMap<CertId, Vec<&Cert>> = BTreeMap::new();
        for c in self.certs.values().filter(|c| c.prev.len() == 1) {
            by_prev.entry(c.prev[0]).or_default().push(c);
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

    /// The whole causal past of `id` (itself included), oldest first: a verifier's prefix.
    pub fn prefix_of(&self, id: CertId) -> Vec<Cert> {
        let mut v: Vec<&Cert> = self.anc[&id].iter().map(|a| &self.certs[a]).collect();
        v.sort_by_key(|c| (c.seq, c.id()));
        v.into_iter().cloned().collect()
    }

    /// For the state hash: every held certificate (with signatures), session times and the view.
    pub fn digest(&self) -> u64 {
        let mut v = vec![self.world, self.certs.len() as u64];
        v.extend(self.certs.values().map(Cert::digest));
        v.extend(self.times.iter().flat_map(|(&c, &t)| [c, t]));
        v.push(self.view().owners.digest());
        fnv_u64s(v)
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

/// Check a whole chain offline, in any order. It must be complete; the answer may be disputed.
pub fn verify_chain(genesis: &Cert, rest: &[Cert]) -> Result<View, OwnError> {
    let mut b = Book::new(genesis.clone())?;
    insert_all(&mut b, rest)?;
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
    pub fn verify(&self, prefix: &[Cert]) -> bool {
        let Some(g) = prefix.iter().find(|c| c.seq == 0) else { return false };
        let Ok(mut book) = Book::new(g.clone()) else { return false };
        let rest: Vec<Cert> = prefix.iter().filter(|c| c.seq > 0).cloned().collect();
        book.world == self.world
            && insert_all(&mut book, &rest).is_ok()
            && self.a.prev.len() == 1
            && self.a.prev == self.b.prev
            && self.a.id() != self.b.id()
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
mod tests {
    use super::*;

    const S: Key = 10; // the seller / creator
    const B: Key = 20;
    const C: Key = 30;
    const I: Key = 31;
    const J: Key = 32;
    const X: Key = 40;
    const R: Key = 50; // a recovery key
    const R2: Key = 51;
    const N: Key = 60; // a replacement key
    const EVE: Key = 66;

    fn next(book: &Book, prev: CertId, change: Change, keys: &[Key]) -> Cert {
        let seq = book.cert(prev).unwrap().seq + 1;
        Cert { world: book.world, seq, prev: vec![prev], change, sigs: vec![] }.signed(keys)
    }

    fn head(b: &Book) -> CertId {
        let h = b.heads();
        assert_eq!(h.len(), 1, "expected a linear chain");
        h[0]
    }

    fn add_at(b: &mut Book, change: Change, keys: &[Key], at: Option<u64>) -> Result<Insert, OwnError> {
        let c = next(b, head(b), change, keys);
        b.insert_at(c, at)
    }

    fn add(b: &mut Book, change: Change, keys: &[Key]) -> Result<Insert, OwnError> {
        add_at(b, change, keys, None)
    }

    /// Insert and return the id.
    fn put(b: &mut Book, prev: CertId, change: Change, keys: &[Key]) -> CertId {
        let c = next(b, prev, change, keys);
        let id = c.id();
        assert_eq!(b.insert(c), Ok(Insert::Applied));
        id
    }

    fn merge(b: &Book, keys: &[Key]) -> Cert {
        let heads = b.heads();
        let seq = heads.iter().map(|h| b.cert(*h).unwrap().seq).max().unwrap() + 1;
        Cert { world: b.world, seq, prev: heads, change: Change::Merge, sigs: vec![] }.signed(keys)
    }

    fn owners(b: &Book) -> Vec<Key> {
        b.view().owners.owners.into_iter().collect()
    }

    fn set(v: &View) -> Vec<Key> {
        v.owners.owners.iter().copied().collect()
    }

    #[test]
    fn genesis_binds_the_world_id() {
        let g = Cert::genesis(&[S, B], SignRule::All, 1);
        let b = Book::new(g.clone()).unwrap();
        assert_eq!(b.world, g.id());
        assert_eq!(owners(&b), [S, B]);
        let mut half = g.clone();
        half.sigs.pop();
        assert_eq!(Book::new(half), Err(OwnError::BadGenesis));
        assert_ne!(Cert::genesis(&[S, B], SignRule::All, 2).id(), b.world);
        let other = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let mut b2 = b.clone();
        assert_eq!(b2.insert(next(&other, other.genesis, Change::Add { key: B }, &[S])), Err(OwnError::WrongWorld));
    }

    #[test]
    fn forged_transfer_and_rogue_co_owner_are_refused() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        assert_eq!(add(&mut b, Change::Add { key: EVE }, &[EVE]), Err(OwnError::Signers));
        let mut forged = next(&b, head(&b), Change::Add { key: EVE }, &[]);
        forged.sigs.push((S, 12345));
        assert_eq!(b.insert(forged), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Add { key: B }, &[S]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Add { key: EVE }, &[B]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Leave { key: S }, &[B]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Leave { key: S }, &[S]), Ok(Insert::Applied));
        assert_eq!(owners(&b), [B]);
    }

    /// OWN-12: under a threshold of 1 a removed owner's signature alone would suffice if it counted.
    #[test]
    fn removed_or_departed_owner_is_a_stranger() {
        let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::Threshold(1), 1)).unwrap();
        assert_eq!(add(&mut b, Change::Remove { key: C }, &[S]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Add { key: X }, &[C]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Leave { key: S }, &[S]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Add { key: X }, &[S]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Add { key: X }, &[B]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Leave { key: X }, &[X]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Leave { key: B }, &[B]), Err(OwnError::LastOwner));
    }

    /// OWN-4: under rule A nobody is removed at once, with two owners or more.
    #[test]
    fn no_instant_removal_under_rule_a() {
        let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
        assert_eq!(add(&mut b, Change::Remove { key: S }, &[B]), Err(OwnError::NotAllowed));
        let mut b3 = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        assert_eq!(add(&mut b3, Change::Remove { key: S }, &[B, C]), Err(OwnError::NotAllowed));
        let mut any = Book::new(Cert::genesis(&[S, B], SignRule::Any, 1)).unwrap();
        assert_eq!(add(&mut any, Change::Challenge { key: S }, &[B]), Err(OwnError::NotAllowed));
    }

    /// O2 as now proposed: the window, which an active owner always cancels.
    #[test]
    fn objection_window() {
        let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        assert_eq!(add_at(&mut b, Change::Challenge { key: S }, &[B], Some(10)), Err(OwnError::Signers));
        assert_eq!(add_at(&mut b, Change::Challenge { key: S }, &[B, C], Some(10)), Ok(Insert::Applied));
        let h = head(&b);
        let rd = next(&b, h, Change::RemoveDormant { key: S }, &[B, C]);
        assert_eq!(b.clone().insert_at(rd.clone(), Some(10 + OBJECTION_FRAMES - 1)), Err(OwnError::WindowOpen));
        // OWN-7: offline nobody can tell how much session time passed.
        assert_eq!(b.clone().insert(rd.clone()), Err(OwnError::Unattested));
        // S signs anything during the window: it is cancelled.
        let mut b2 = b.clone();
        assert_eq!(add_at(&mut b2, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], Some(20)), Ok(Insert::Applied));
        assert_eq!(add_at(&mut b2, Change::RemoveDormant { key: S }, &[B, C], Some(10 + OBJECTION_FRAMES)), Err(OwnError::WindowOpen));
        // S stays silent: the others remove it once the window has run.
        assert_eq!(b.insert_at(rd, Some(10 + OBJECTION_FRAMES)), Ok(Insert::Applied));
        assert_eq!(owners(&b), [B, C]);
    }

    /// The window also cancels when the target signs on another branch (concurrently).
    #[test]
    fn window_cancelled_by_a_concurrent_signature() {
        let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let ch = next(&b, g, Change::Challenge { key: S }, &[B, C]);
        b.insert_at(ch.clone(), Some(5)).unwrap();
        let rd = next(&b, ch.id(), Change::RemoveDormant { key: S }, &[B, C]);
        b.insert_at(rd, Some(5 + OBJECTION_FRAMES)).unwrap();
        assert_eq!(owners(&b), [B, C]);
        // S had named a recovery key on a copy of the world the others never saw.
        put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]);
        assert_eq!(owners(&b), [S, B, C]);
    }

    /// OWN-6: a recovery key never signs for its owner; it only replaces a lost key, after a delay the
    /// owner cancels; a replaced recovery key stops counting, through forks too.
    #[test]
    fn recovery_replaces_a_lost_key_only() {
        let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
        assert_eq!(add(&mut b, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Add { key: R2 }, &[R, B]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Leave { key: S }, &[R]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::SetRecovery { owner: S, recovery: Some(EVE) }, &[B]), Err(OwnError::Signers));
        assert_eq!(add_at(&mut b, Change::StartRecover { owner: S, new_key: N }, &[EVE], Some(1)), Err(OwnError::Signers));
        assert_eq!(add_at(&mut b, Change::StartRecover { owner: S, new_key: N }, &[R], Some(1)), Ok(Insert::Applied));
        // S still has its key and cancels.
        let mut cancel = b.clone();
        assert_eq!(add_at(&mut cancel, Change::SetRecovery { owner: S, recovery: Some(R2) }, &[S], Some(2)), Ok(Insert::Applied));
        assert_eq!(add_at(&mut cancel, Change::FinishRecover { owner: S }, &[R], Some(1 + RECOVERY_FRAMES)), Err(OwnError::WindowOpen));
        assert_eq!(add_at(&mut b.clone(), Change::FinishRecover { owner: S }, &[R], Some(RECOVERY_FRAMES)), Err(OwnError::WindowOpen));
        assert_eq!(add_at(&mut b, Change::FinishRecover { owner: S }, &[R], Some(1 + RECOVERY_FRAMES)), Ok(Insert::Applied));
        assert_eq!(owners(&b), [B, N]);
        assert!(b.view().owners.recovery.is_empty());
    }

    #[test]
    fn rotated_recovery_key_is_dead_on_every_branch() {
        let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let r1 = put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]);
        put(&mut b, r1, Change::SetRecovery { owner: S, recovery: Some(R2) }, &[S]);
        // R forks at r1 and starts replacing S, and completes it there (session times attested).
        let st = next(&b, r1, Change::StartRecover { owner: S, new_key: EVE }, &[R]);
        b.insert_at(st.clone(), Some(1)).unwrap();
        let fin = next(&b, st.id(), Change::FinishRecover { owner: S }, &[R]);
        b.insert_at(fin, Some(1 + RECOVERY_FRAMES)).unwrap();
        let v = b.view();
        assert!(v.disputed);
        assert_eq!(set(&v), [S, B]);
        assert_eq!(v.owners.recovery[&S].keys().copied().collect::<Vec<_>>(), [R2]);
    }

    /// The sole owner sells twice on two disconnected copies of the world.
    fn double_sale() -> (Cert, Vec<Cert>) {
        let g = Cert::genesis(&[S], SignRule::All, 7);
        let b = Book::new(g.clone()).unwrap();
        let to_b = next(&b, b.genesis, Change::Add { key: B }, &[S]);
        let to_c = next(&b, b.genesis, Change::Add { key: C }, &[S]);
        let mut bb = b.clone();
        bb.insert(to_b.clone()).unwrap();
        let b_out = next(&bb, to_b.id(), Change::Leave { key: S }, &[S]);
        let mut bc = b.clone();
        bc.insert(to_c.clone()).unwrap();
        let c_out = next(&bc, to_c.id(), Change::Leave { key: S }, &[S]);
        (g, vec![to_b, b_out, to_c, c_out])
    }

    #[test]
    fn double_sale_is_detected_and_merges_in_any_order() {
        let (g, certs) = double_sale();
        let mut views = BTreeSet::new();
        for perm in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2], [3, 1, 2, 0]] {
            let order: Vec<Cert> = perm.iter().map(|&i| certs[i].clone()).collect();
            let v = verify_chain(&g, &order).unwrap();
            assert!(v.disputed);
            views.insert((set(&v), v.owners.digest()));
        }
        assert_eq!(views.len(), 1);
        assert_eq!(views.into_iter().next().unwrap().0, [B, C]);
        let mut b = Book::new(g.clone()).unwrap();
        insert_all(&mut b, &certs).unwrap();
        let forks = b.forks();
        assert_eq!(forks.len(), 1);
        let p = &forks[0];
        assert!(p.verify(std::slice::from_ref(&g)));
        assert!(p.equivocation());
        assert_eq!(p.signed_both(), [S].into());
        let mut bad = p.clone();
        bad.b.sigs.clear();
        assert!(!bad.verify(std::slice::from_ref(&g)));
        // Either buyer merges; afterwards rule A runs over the union.
        let m = merge(&b, &[EVE]);
        assert_eq!(b.insert(m), Err(OwnError::Signers));
        assert_eq!(b.insert(merge(&b, &[B])), Ok(Insert::Applied));
        assert!(!b.view().disputed);
        assert_eq!(owners(&b), [B, C]);
        assert_eq!(add(&mut b, Change::Add { key: X }, &[B]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Add { key: X }, &[B, C]), Ok(Insert::Applied));
    }

    /// OWN-1: a merge joins concurrent heads only, so nobody rolls the chain back.
    #[test]
    fn merge_cannot_roll_back() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let c1 = put(&mut b, g, Change::Add { key: B }, &[S]);
        let c2 = put(&mut b, c1, Change::Leave { key: S }, &[S]);
        let mut p = vec![g, c1];
        p.sort_unstable();
        let back = Cert { world: b.world, seq: 2, prev: p, change: Change::Merge, sigs: vec![] }.signed(&[S, B]);
        assert_eq!(b.insert(back), Err(OwnError::BadLink));
        let mut p = vec![c1, c2];
        p.sort_unstable();
        let back = Cert { world: b.world, seq: 3, prev: p, change: Change::Merge, sigs: vec![] }.signed(&[B]);
        assert_eq!(b.insert(back), Err(OwnError::BadLink));
        assert_eq!(owners(&b), [B]);
    }

    /// OWN-2: a seller who left can't come back through a fork and a merge.
    #[test]
    fn leave_sticks_across_forks() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let c1 = put(&mut b, g, Change::Add { key: B }, &[S]);
        put(&mut b, c1, Change::Leave { key: S }, &[S]);
        put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]);
        assert_eq!(owners(&b), [B]);
        assert_eq!(b.insert(merge(&b, &[S])), Err(OwnError::Signers));
        assert_eq!(b.insert(merge(&b, &[B])), Ok(Insert::Applied));
        assert_eq!(owners(&b), [B]);
    }

    /// OWN-3: a fork from an older, smaller owner set doesn't keep that set's power.
    #[test]
    fn old_state_fork_needs_the_owners_added_since() {
        // G{S} -> Add I -> Add J. S forks at "Add I" and opens a window against I alone: J never agreed.
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let c1 = put(&mut b, g, Change::Add { key: I }, &[S]);
        let c2 = put(&mut b, c1, Change::Add { key: J }, &[S, I]);
        let ch = next(&b, c1, Change::Challenge { key: I }, &[S]);
        b.insert_at(ch.clone(), Some(1)).unwrap();
        b.insert_at(next(&b, ch.id(), Change::RemoveDormant { key: I }, &[S]), Some(1 + OBJECTION_FRAMES)).unwrap();
        assert_eq!(owners(&b), [S, I, J]);
        // Under rule C (instant removal) the same fork is also powerless.
        let mut c = Book::new(Cert::genesis(&[S], SignRule::Threshold(9), 1)).unwrap();
        let g = c.genesis;
        let d1 = put(&mut c, g, Change::Add { key: I }, &[S]);
        put(&mut c, d1, Change::Add { key: J }, &[S, I]);
        put(&mut c, d1, Change::Remove { key: I }, &[S]);
        assert_eq!(owners(&c), [S, I, J]);
        let _ = c2;
    }

    #[test]
    fn stale_branch_owner_cannot_add_alone() {
        // After a double sale B (sole owner of its branch) adds EVE there, without C.
        let (g, certs) = double_sale();
        let mut b = Book::new(g).unwrap();
        insert_all(&mut b, &certs).unwrap();
        put(&mut b, certs[1].id(), Change::Add { key: EVE }, &[B]);
        assert_eq!(owners(&b), [B, C]);
        // With C's signature too it counts.
        put(&mut b, certs[1].id(), Change::Add { key: X }, &[B, C]);
        assert_eq!(owners(&b), [B, C, X]);
    }

    /// The residual [I]: keys that once held full authority add a co-owner by forking at that state.
    #[test]
    fn former_sole_owner_can_still_add_through_a_fork() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let c1 = put(&mut b, g, Change::Add { key: B }, &[S]);
        put(&mut b, c1, Change::Leave { key: S }, &[S]);
        put(&mut b, g, Change::Add { key: EVE }, &[S]);
        let v = b.view();
        assert!(v.disputed);
        assert_eq!(set(&v), [B, EVE]);
        let p = &b.forks()[0];
        assert!(p.equivocation() && p.verify(&[b.cert(g).unwrap().clone()]));
    }

    /// OWN-5: the same certificates give the same book in any arrival order.
    #[test]
    fn arrival_order_never_matters() {
        let (g, mut certs) = double_sale();
        let mut b = Book::new(g.clone()).unwrap();
        insert_all(&mut b, &certs).unwrap();
        let ext = next(&b, certs[1].id(), Change::SetRecovery { owner: B, recovery: Some(R) }, &[B]);
        let m = merge(&b, &[B]);
        let m2 = Cert { world: b.world, seq: 4, prev: { let mut p = vec![m.id(), ext.id()]; p.sort_unstable(); p }, change: Change::Merge, sigs: vec![] }.signed(&[C]);
        certs.extend([ext, m, m2]);
        let mut seen = BTreeSet::new();
        for rot in 0..certs.len() {
            let mut order = certs.clone();
            order.rotate_left(rot);
            let mut bk = Book::new(g.clone()).unwrap();
            insert_all(&mut bk, &order).unwrap();
            assert_eq!(bk.len(), 8);
            seen.insert(bk.digest());
        }
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn merge_is_causal() {
        // C left in the shared prefix and is added back on one branch; the old Leave doesn't count there.
        let g = Cert::genesis(&[S, B, C], SignRule::All, 6);
        let mut b = Book::new(g.clone()).unwrap();
        assert_eq!(add(&mut b, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Leave { key: C }, &[C]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Leave { key: B }, &[B]), Ok(Insert::Applied));
        let fork = head(&b);
        let c_back = put(&mut b, fork, Change::Add { key: C }, &[S]);
        put(&mut b, fork, Change::Add { key: X }, &[S]);
        // On C's branch S clears its recovery key; the other branch saw it named.
        put(&mut b, c_back, Change::SetRecovery { owner: S, recovery: None }, &[S]);
        let v = b.view();
        assert!(v.disputed);
        assert_eq!(set(&v), [S, C, X]);
        assert!(v.owners.recovery.is_empty());
        // A recovery key named on one branch only survives.
        let g3 = Cert::genesis(&[S, B], SignRule::All, 8);
        let b3 = Book::new(g3.clone()).unwrap();
        let named = next(&b3, b3.genesis, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]);
        let other = next(&b3, b3.genesis, Change::Add { key: X }, &[S, B]);
        let v3 = verify_chain(&g3, &[other, named.clone()]).unwrap();
        assert_eq!(v3.owners.recovery, [(S, [(R, named.id())].into())].into());
    }

    /// OWN-8: honest concurrency is a fork without an equivocator, and loses nobody.
    #[test]
    fn honest_concurrency_blames_nobody() {
        let g = Cert::genesis(&[S, B], SignRule::Any, 3);
        let b = Book::new(g.clone()).unwrap();
        let x = next(&b, b.genesis, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]);
        let c = next(&b, b.genesis, Change::SetRecovery { owner: B, recovery: Some(R2) }, &[B]);
        let mut bk = b.clone();
        insert_all(&mut bk, &[x, c]).unwrap();
        let p = &bk.forks()[0];
        assert!(!p.equivocation());
        assert_eq!(bk.view().owners.recovery.len(), 2);
        // Rule B: two owners each add someone offline; both stay.
        let x = next(&b, b.genesis, Change::Add { key: X }, &[S]);
        let c = next(&b, b.genesis, Change::Add { key: C }, &[B]);
        assert_eq!(set(&verify_chain(&g, &[c, x]).unwrap()), [S, B, C, X]);
    }

    #[test]
    fn removal_survives_a_fork_by_its_target_under_rule_c() {
        let g = Cert::genesis(&[S, B, X], SignRule::Threshold(2), 4);
        let b = Book::new(g.clone()).unwrap();
        let rm = next(&b, b.genesis, Change::Remove { key: X }, &[S, B]);
        let revive = next(&b, b.genesis, Change::SetRecovery { owner: X, recovery: Some(R) }, &[X]);
        assert_eq!(set(&verify_chain(&g, &[revive, rm]).unwrap()), [S, B]);
        // Two owners who removed each other on two copies: both stay.
        let g2 = Cert::genesis(&[S, B], SignRule::Threshold(1), 5);
        let b2 = Book::new(g2.clone()).unwrap();
        let a = next(&b2, b2.genesis, Change::Remove { key: B }, &[S]);
        let c = next(&b2, b2.genesis, Change::Remove { key: S }, &[B]);
        assert_eq!(set(&verify_chain(&g2, &[a, c]).unwrap()), [S, B]);
    }

    #[test]
    fn tampered_or_reordered_chains_are_refused() {
        let g = Cert::genesis(&[S], SignRule::All, 9);
        let mut b = Book::new(g.clone()).unwrap();
        let c1 = next(&b, b.genesis, Change::Add { key: B }, &[S]);
        b.insert(c1.clone()).unwrap();
        let c2 = next(&b, c1.id(), Change::Leave { key: S }, &[S]);
        let mut t = c1.clone();
        t.change = Change::Add { key: EVE };
        assert_eq!(verify_chain(&g, &[t, c2.clone()]), Err(OwnError::Signers));
        assert_eq!(verify_chain(&g, std::slice::from_ref(&c2)), Err(OwnError::MissingParent));
        let mut skip = next(&b, c1.id(), Change::Leave { key: S }, &[]);
        skip.seq = 5;
        assert_eq!(b.clone().insert(skip.signed(&[S])), Err(OwnError::BadLink));
        assert_eq!(verify_chain(&g, &[c1.clone(), c2.clone()]), verify_chain(&g, &[c2, c1]));
    }

    /// OWN-10: a certificate without its parents is refused and not kept.
    #[test]
    fn orphans_are_not_stored() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let orphan = Cert { world: b.world, seq: 5, prev: vec![12345], change: Change::Add { key: EVE }, sigs: vec![] };
        assert_eq!(b.insert(orphan), Err(OwnError::MissingParent));
        assert_eq!(b.len(), 1);
    }

    /// OWN-9: a prefix in any order, and one containing a merge, verifies.
    #[test]
    fn fork_proof_prefix_order_and_merges() {
        let (g, certs) = double_sale();
        let mut b = Book::new(g).unwrap();
        insert_all(&mut b, &certs).unwrap();
        let m = merge(&b, &[B]);
        b.insert(m.clone()).unwrap();
        let x = next(&b, m.id(), Change::Add { key: I }, &[B, C]);
        let y = next(&b, m.id(), Change::Add { key: J }, &[B, C]);
        let p = ForkProof { world: b.world, a: x, b: y };
        let mut prefix = b.prefix_of(m.id());
        assert!(p.verify(&prefix));
        prefix.reverse();
        assert!(p.verify(&prefix));
        assert_eq!(p.signed_both(), [B, C].into());
    }

    #[test]
    fn threshold_rule_c() {
        let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        assert_eq!(add(&mut b, Change::SetRule { rule: SignRule::Threshold(2) }, &[S, B]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::SetRule { rule: SignRule::Threshold(2) }, &[S, B, C]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Add { key: X }, &[S]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Add { key: X }, &[S, B]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Remove { key: C }, &[S, C]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Remove { key: C }, &[S, B]), Ok(Insert::Applied));
        assert_eq!(owners(&b), [S, B, X]);
    }

    #[test]
    fn wire_round_trip_and_one_encoding() {
        let (g, mut certs) = double_sale();
        certs.push(g);
        certs.push(Cert { world: 5, seq: 9, prev: vec![1, 2], change: Change::Merge, sigs: vec![] }.signed(&[S]));
        for change in [
            Change::SetRule { rule: SignRule::Threshold(3) },
            Change::SetRecovery { owner: S, recovery: Some(0) },
            Change::SetRecovery { owner: S, recovery: None },
            Change::Remove { key: X },
            Change::Challenge { key: X },
            Change::RemoveDormant { key: X },
            Change::StartRecover { owner: S, new_key: N },
            Change::FinishRecover { owner: S },
        ] {
            certs.push(Cert { world: 5, seq: 1, prev: vec![1], change, sigs: vec![] }.signed(&[S, B]));
        }
        for c in certs {
            let mut w = c.to_words();
            assert_eq!(Cert::from_words(&w), Some(c));
            w.push(7);
            assert_eq!(Cert::from_words(&w), None);
        }
        assert_eq!(Cert::from_words(&[1, 2, 3]), None);
    }
}

/// Review round 2 (OWNR-1..9), written against 61ab504's API to show each fails before the fix.
#[cfg(test)]
mod round2_before {
    use super::*;
    const S: Key = 10;
    const A: Key = 11;
    const B: Key = 20;
    const C: Key = 30;
    const K: Key = 33;
    const X: Key = 40;
    const Z: Key = 41;
    const R: Key = 50;
    const N: Key = 60;
    const EVE: Key = 66;

    fn next(book: &Book, prev: CertId, change: Change, keys: &[Key]) -> Cert {
        let seq = book.cert(prev).unwrap().seq + 1;
        Cert { world: book.world, seq, prev: vec![prev], change, sigs: vec![] }.signed(keys)
    }
    fn put(b: &mut Book, prev: CertId, change: Change, keys: &[Key], at: Option<u64>) -> CertId {
        let c = next(b, prev, change, keys);
        let id = c.id();
        assert_eq!(b.insert_at(c, at), Ok(Insert::Applied));
        id
    }
    fn owners(b: &Book) -> Vec<Key> {
        b.view().owners.owners.into_iter().collect()
    }

    #[test]
    fn ownr1a_add_after_a_cancelled_removal() {
        let mut b = Book::new(Cert::genesis(&[A, B], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let ch = put(&mut b, g, Change::Challenge { key: B }, &[A], Some(1));
        put(&mut b, ch, Change::SetRecovery { owner: B, recovery: Some(R) }, &[B], Some(2));
        let rd = put(&mut b, ch, Change::RemoveDormant { key: B }, &[A], Some(1 + OBJECTION_FRAMES));
        put(&mut b, rd, Change::Add { key: X }, &[A], Some(2 + OBJECTION_FRAMES));
        assert_eq!(owners(&b), [A, B]);
    }

    #[test]
    fn ownr1b_add_after_a_cancelled_recovery() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let r = put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], Some(1));
        let st = put(&mut b, r, Change::StartRecover { owner: S, new_key: N }, &[R], Some(2));
        put(&mut b, st, Change::SetRule { rule: SignRule::All }, &[S], Some(3));
        let fin = put(&mut b, st, Change::FinishRecover { owner: S }, &[R], Some(2 + RECOVERY_FRAMES));
        put(&mut b, fin, Change::Add { key: EVE }, &[N], Some(3 + RECOVERY_FRAMES));
        assert_eq!(owners(&b), [S]);
    }

    #[test]
    fn ownr2_completed_removal_not_undone_by_a_stale_signature() {
        let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let ch = put(&mut b, g, Change::Challenge { key: S }, &[B, C], Some(1));
        put(&mut b, ch, Change::RemoveDormant { key: S }, &[B, C], Some(1 + OBJECTION_FRAMES));
        // Months later the old key signs on a stale fork (never committed by any session).
        put(&mut b, ch, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], None);
        assert_eq!(owners(&b), [B, C]);
    }

    #[test]
    fn ownr3_offline_verification_of_a_completed_window() {
        let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let ch = put(&mut b, g, Change::Challenge { key: S }, &[B, C], Some(1));
        put(&mut b, ch, Change::RemoveDormant { key: S }, &[B, C], Some(1 + OBJECTION_FRAMES));
        let certs: Vec<Cert> = b.prefix_of(b.heads()[0]).into_iter().filter(|c| c.seq > 0).collect();
        assert!(verify_chain(b.cert(g).unwrap(), &certs).is_ok());
    }

    #[test]
    fn ownr4_signature_copies_converge() {
        let b0 = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        let x = next(&b0, b0.genesis, Change::Challenge { key: S }, &[B, C]);
        let mut x2 = x.clone();
        x2.sigs.push((S, key_sig(S, x.id())));
        let (mut b1, mut b2) = (b0.clone(), b0.clone());
        let _ = b1.insert(x.clone());
        let _ = b1.insert(x2.clone());
        let _ = b2.insert(x2);
        let _ = b2.insert(x);
        assert_eq!(b1.digest(), b2.digest());
    }

    #[test]
    fn ownr5_unattested_fork_add_cannot_revive_a_removed_owner() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let a = put(&mut b, g, Change::Add { key: A }, &[S], Some(1));
        let k = put(&mut b, a, Change::Add { key: K }, &[S, A], Some(2));
        let l = put(&mut b, k, Change::Leave { key: S }, &[S], Some(3));
        let ch = put(&mut b, l, Change::Challenge { key: K }, &[A], Some(4));
        put(&mut b, ch, Change::RemoveDormant { key: K }, &[A], Some(4 + OBJECTION_FRAMES));
        assert_eq!(owners(&b), [A]);
        // The former sole owner signs a sock Add at G offline; no session ever committed it.
        put(&mut b, g, Change::Add { key: Z }, &[S], None);
        assert_eq!(owners(&b), [A]);
    }

    #[test]
    fn ownr6_cert_count_is_capped() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let mut h = b.genesis;
        let mut refused = false;
        for i in 0..5000u32 {
            let c = next(&b, h, Change::SetRecovery { owner: S, recovery: Some(1000 + i) }, &[S]);
            let id = c.id();
            if b.insert(c).is_err() {
                refused = true;
                break;
            }
            h = id;
        }
        assert!(refused);
    }

    #[test]
    fn ownr7_mutual_removal_under_rule_c_keeps_both() {
        let g = Cert::genesis(&[A, B], SignRule::Threshold(1), 1);
        let mut b = Book::new(g).unwrap();
        let gid = b.genesis;
        put(&mut b, gid, Change::Remove { key: B }, &[A], None);
        let c = put(&mut b, gid, Change::Remove { key: A }, &[B], None);
        put(&mut b, c, Change::Add { key: EVE }, &[B], None);
        let o = owners(&b);
        assert!(o.contains(&A) && o.contains(&B));
    }

    #[test]
    fn ownr8_duplicate_changes_nothing() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let c = next(&b, b.genesis, Change::Add { key: B }, &[S]);
        b.insert(c.clone()).unwrap();
        let d = b.digest();
        assert_eq!(b.insert_at(c, Some(5)), Ok(Insert::Duplicate));
        assert_eq!(b.digest(), d);
    }

    #[test]
    fn ownr9_old_fork_doesnt_revert_an_agreed_rule() {
        let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        put(&mut b, g, Change::SetRule { rule: SignRule::Threshold(1) }, &[S, B], Some(1));
        put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], Some(2));
        assert_eq!(b.view().owners.rule, SignRule::Threshold(1));
    }
}
