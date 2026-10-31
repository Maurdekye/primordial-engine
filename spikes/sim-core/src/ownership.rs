//! World ownership as a chain of signed certificates (user ruling 2026-10-30 22:39Z, "Signed chain,
//! no blockchain"; the design is provenance-eng's world-ownership draft 4, which @engine-architect
//! folds into design.md). Ownership is a lasting property of a world, separate from a session's
//! admins, and can be shared. It needs no registry or service: anyone can verify the chain offline.
//!
//! * **Genesis.** Signed by every creator. The world id is the hash of the genesis certificate (in
//!   production, of the genesis document that contains it), so the original owners can't be
//!   rewritten without changing the world's identity.
//! * **Changes.** Each later certificate names its parent and is valid only if its signatures
//!   satisfy the rule of the owner set *at the parent*. Signers outside that set never count, so an
//!   owner who left or was removed is a stranger to every later certificate.
//! * **Rule A** (ruled 22:54Z, "for now"): adding an owner needs every current owner, removing one
//!   needs every other owner, and anyone may leave alone, except the last owner. A recovery key that
//!   an owner names signs in its place, but never to make itself an owner. An owner with no recovery
//!   key can be removed after a 30-day objection window in the world's session time, which any
//!   signature of theirs cancels. Rules B (any owner adds) and C (k of n) are kept for comparison.
//! * **Forks** (two valid certificates with one parent) are detect-only, as D79: a [`ForkProof`]
//!   convinces any third party holding the prefix. The owner set is the **union** of every head's
//!   owners, minus keys a `Remove` took out on any branch (ruled 22:54Z, O4): a function of the set
//!   of certificates, so the order they arrive in never matters. A `Merge` certificate, which any
//!   owner of the union may sign, makes the chain linear again; afterwards nobody may extend a
//!   merged head.
//!
//! Spike signatures are keyless hashes ([`key_sig`]) standing in for Ed25519, as `member_sig` does
//! for world members.

use std::collections::{BTreeMap, BTreeSet};

use crate::{fnv_u64s, splitmix64};

/// A player's long-term identity key (design 10.3). Spike: a number.
pub type Key = u32;
pub type CertId = u64;

/// The objection window before an owner with no recovery key can be removed: 30 days of the world's
/// session time at 60 Hz.
pub const OBJECTION_FRAMES: u64 = 30 * 24 * 3600 * 60;

/// Spike model of `key` signing `msg`. Production: Ed25519.
pub fn key_sig(key: Key, msg: u64) -> u64 {
    splitmix64(msg ^ splitmix64(key as u64 ^ 0x4F57_4E45_5253_4950))
}

/// Who must sign an `Add` (and, for `Remove`, who among the other owners). Ordered by strictness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum SignRule {
    /// B: any one owner adds; nobody can remove another owner.
    Any,
    /// C: `k` of the owners (all of them if fewer).
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
    /// `approvals` meet this rule over the owners in `of`.
    fn met(self, approvals: &BTreeSet<Key>, of: &BTreeSet<Key>) -> bool {
        let n = approvals.intersection(of).count();
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
    /// An owner gives up ownership; signed by that owner (or its recovery key).
    Leave { key: Key },
    /// The other owners remove `key` (never under rule B).
    Remove { key: Key },
    /// Every owner signs.
    SetRule { rule: SignRule },
    /// `owner` names (or clears) its recovery key.
    SetRecovery { owner: Key, recovery: Option<Key> },
    /// The other owners open the objection window against `key`, which has no recovery key.
    Challenge { key: Key },
    /// The other owners remove `key` once its window has run without a signature from it.
    RemoveDormant { key: Key },
    /// Joins every parent: the chain is linear again. Its owner set is forced (the union).
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
    /// Parents: none for genesis, several for a merge, one otherwise.
    pub prev: Vec<CertId>,
    pub change: Change,
    /// The world's session time (frame) the certificate claims. Never decreases along a chain; in a
    /// session it can't be later than the entry that publishes it. Only the objection window reads it.
    pub time: u64,
    pub sigs: Vec<(Key, u64)>,
}

impl Cert {
    /// The certificate's id: everything but the signatures, which is also what each key signs.
    pub fn id(&self) -> CertId {
        let mut v = vec![self.world, self.seq as u64, self.time, self.prev.len() as u64];
        v.extend(&self.prev);
        self.change.words(&mut v);
        fnv_u64s(v)
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
        Cert { world: 0, seq: 0, prev: vec![], change: Change::Genesis { owners: o, rule, nonce }, time: 0, sigs: vec![] }.signed(owners)
    }

    /// For the state hash: every field, signatures included.
    pub fn digest(&self) -> u64 {
        let mut v = vec![self.id(), self.sigs.len() as u64];
        v.extend(self.sigs.iter().flat_map(|&(k, s)| [k as u64, s]));
        fnv_u64s(v)
    }

    /// Spike wire form (session log entries): a list of words.
    pub fn to_words(&self) -> Vec<u64> {
        let mut v = vec![self.world, self.seq as u64, self.time, self.prev.len() as u64];
        v.extend(&self.prev);
        self.change.words(&mut v);
        v.push(self.sigs.len() as u64);
        v.extend(self.sigs.iter().flat_map(|&(k, s)| [k as u64, s]));
        v
    }

    pub fn from_words(w: &[u64]) -> Option<Cert> {
        let mut it = w.iter().copied();
        let mut next = || it.next();
        let (world, seq, time, np) = (next()?, u32::try_from(next()?).ok()?, next()?, next()? as usize);
        let prev = (0..np.min(w.len())).map(|_| next()).collect::<Option<Vec<_>>>()?;
        let key = |x: u64| u32::try_from(x).ok();
        let rule = |x: u64| match x {
            1 => Some(SignRule::Any),
            3 => Some(SignRule::All),
            x if x >> 8 == 1 => Some(SignRule::Threshold(x as u8)),
            _ => None,
        };
        let change = match next()? {
            1 => {
                let (rule, nonce, n) = (rule(next()?)?, next()?, next()? as usize);
                let owners = (0..n.min(w.len())).map(|_| next().and_then(key)).collect::<Option<Vec<_>>>()?;
                Change::Genesis { owners, rule, nonce }
            }
            2 => Change::Add { key: key(next()?)? },
            3 => Change::Leave { key: key(next()?)? },
            4 => Change::Remove { key: key(next()?)? },
            5 => Change::SetRule { rule: rule(next()?)? },
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
            _ => return None,
        };
        let ns = next()? as usize;
        let sigs = (0..ns.min(w.len())).map(|_| Some((key(next()?)?, next()?))).collect::<Option<Vec<_>>>()?;
        Some(Cert { world, seq, prev, change, time, sigs })
    }
}

/// Why a certificate is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnError {
    WrongWorld,
    /// Not a well-formed genesis, or not signed by every creator.
    BadGenesis,
    /// A parent is not known (yet).
    MissingParent,
    /// `seq` or `time` does not follow its parents, or the shape of `prev` is wrong for the change.
    BadLink,
    /// The signatures do not satisfy the rule of the owner set at the parent.
    Signers,
    AlreadyOwner,
    NotOwner,
    /// The last owner can't leave or be removed.
    LastOwner,
    /// Rule B has no removal, and a threshold below one is no rule.
    NotAllowed,
    /// The target has a recovery key: it signs instead, so there is no window.
    HasRecovery,
    /// No open window against the target, or it hasn't run yet.
    WindowOpen,
    /// The parent is a head that a merge already joined.
    Superseded,
}

/// The owner set and its rules after some certificate (or after the merge of several heads).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Owners {
    pub owners: BTreeSet<Key>,
    pub rule: SignRule,
    /// Each owner's recovery keys, with the certificate that named each (normally at most one; a
    /// fork can leave two, both named by that owner).
    pub recovery: BTreeMap<Key, BTreeMap<Key, CertId>>,
    /// Open objection windows: target -> the session time it opened.
    pub challenges: BTreeMap<Key, u64>,
    /// Keys a `Remove` took out on this history and that were not added back, with that `Remove`.
    pub removed: BTreeMap<Key, CertId>,
    pub seq: u32,
    pub time: u64,
}

impl Owners {
    /// The owners whose approval `signers` carries: an owner's own key, or one of its recovery keys,
    /// except a recovery key voting to make itself an owner.
    pub fn approvals(&self, signers: &BTreeSet<Key>, adding: Option<Key>) -> BTreeSet<Key> {
        self.owners
            .iter()
            .copied()
            .filter(|o| {
                signers.contains(o)
                    || self.recovery.get(o).is_some_and(|r| r.keys().any(|k| signers.contains(k) && Some(*k) != adding))
            })
            .collect()
    }

    fn others(&self, key: Key) -> BTreeSet<Key> {
        self.owners.iter().copied().filter(|&o| o != key).collect()
    }

    /// The state after `c`, whose single parent left this state.
    fn apply(&self, c: &Cert) -> Result<Owners, OwnError> {
        if c.seq != self.seq + 1 || c.time < self.time || c.prev.len() != 1 {
            return Err(OwnError::BadLink);
        }
        let (id, signers) = (c.id(), c.signers());
        let mut s = self.clone();
        let adding = match c.change {
            Change::Add { key } => Some(key),
            _ => None,
        };
        let ok = self.approvals(&signers, adding);
        // Any signature by a challenged owner cancels its window (a recovery key can't exist here).
        s.challenges.retain(|k, _| !signers.contains(k));
        let removal = |key: Key| -> Result<(), OwnError> {
            if !self.owners.contains(&key) {
                return Err(OwnError::NotOwner);
            }
            if self.owners.len() == 1 {
                return Err(OwnError::LastOwner);
            }
            if self.rule == SignRule::Any {
                return Err(OwnError::NotAllowed);
            }
            let others = self.others(key);
            let ok: BTreeSet<Key> = ok.iter().copied().filter(|&o| o != key).collect();
            if !self.rule.met(&ok, &others) {
                return Err(OwnError::Signers);
            }
            Ok(())
        };
        match c.change {
            Change::Genesis { .. } => return Err(OwnError::BadGenesis),
            Change::Merge => return Err(OwnError::BadLink),
            Change::Add { key } => {
                if self.owners.contains(&key) {
                    return Err(OwnError::AlreadyOwner);
                }
                if !self.rule.met(&ok, &self.owners) {
                    return Err(OwnError::Signers);
                }
                s.owners.insert(key);
                s.removed.remove(&key);
            }
            Change::Leave { key } => {
                if !self.owners.contains(&key) {
                    return Err(OwnError::NotOwner);
                }
                if self.owners.len() == 1 {
                    return Err(OwnError::LastOwner);
                }
                if !ok.contains(&key) {
                    return Err(OwnError::Signers);
                }
                s.drop_owner(key);
            }
            Change::Remove { key } => {
                removal(key)?;
                s.drop_owner(key);
                s.removed.insert(key, id);
            }
            Change::SetRule { rule } => {
                if rule == SignRule::Threshold(0) {
                    return Err(OwnError::NotAllowed);
                }
                if !SignRule::All.met(&ok, &self.owners) {
                    return Err(OwnError::Signers);
                }
                s.rule = rule;
            }
            Change::SetRecovery { owner, recovery } => {
                if !self.owners.contains(&owner) || recovery.is_some_and(|r| self.owners.contains(&r)) {
                    return Err(OwnError::NotOwner);
                }
                if !ok.contains(&owner) {
                    return Err(OwnError::Signers);
                }
                s.recovery.remove(&owner);
                if let Some(r) = recovery {
                    s.recovery.insert(owner, [(r, id)].into());
                }
                s.challenges.remove(&owner);
            }
            Change::Challenge { key } => {
                if self.recovery.get(&key).is_some_and(|r| !r.is_empty()) {
                    return Err(OwnError::HasRecovery);
                }
                removal(key)?;
                s.challenges.insert(key, c.time);
            }
            Change::RemoveDormant { key } => {
                if !self.challenges.get(&key).is_some_and(|&t| c.time >= t.saturating_add(OBJECTION_FRAMES)) || signers.contains(&key) {
                    return Err(OwnError::WindowOpen);
                }
                removal(key)?;
                s.drop_owner(key);
                s.removed.insert(key, id);
            }
        }
        // A recovery key never stays one once it is an owner itself.
        for r in s.recovery.values_mut() {
            r.retain(|k, _| !s.owners.contains(k));
        }
        s.recovery.retain(|_, r| !r.is_empty());
        s.seq = c.seq;
        s.time = c.time;
        Ok(s)
    }

    fn drop_owner(&mut self, key: Key) {
        self.owners.remove(&key);
        self.recovery.remove(&key);
        self.challenges.remove(&key);
    }

    /// The union merge of several heads (O4), each given with its ancestors (itself included).
    /// Removals are observed-remove: a key that a `Remove` on one branch took out stays out unless a
    /// head that saw that `Remove` (has it as an ancestor) holds the key again, so a removal in the
    /// shared prefix that a later `Add` undid on one branch does not count, and a target cannot undo
    /// its removal by a concurrent certificate of its own. A `Leave` counts only if every branch
    /// contains it (plain union). Recovery keys merge the same way: an owner's key stays unless a
    /// branch that saw it named has since cleared or replaced it. If the removals would leave nobody
    /// (two owners who removed each other on two copies of the world), every head's owners stay.
    /// The strictest rule wins; a window stays open only if it is open on every branch.
    fn merge(heads: &[(&Owners, &BTreeSet<CertId>)]) -> Owners {
        let all: BTreeSet<Key> = heads.iter().flat_map(|(h, _)| h.owners.iter().copied()).collect();
        let mut removed: BTreeMap<Key, CertId> = BTreeMap::new();
        for (h, _) in heads {
            for (k, r) in &h.removed {
                // Out unless some head that holds the key saw this removal.
                let undone = heads.iter().any(|(j, anc)| j.owners.contains(k) && anc.contains(r));
                if !undone {
                    removed.entry(*k).or_insert(*r);
                }
            }
        }
        let mut owners: BTreeSet<Key> = all.iter().copied().filter(|k| !removed.contains_key(k)).collect();
        if owners.is_empty() {
            owners = all;
        }
        let mut recovery: BTreeMap<Key, BTreeMap<Key, CertId>> = BTreeMap::new();
        for (h, _) in heads {
            for (o, rs) in &h.recovery {
                for (r, named) in rs {
                    let cleared = heads.iter().any(|(j, anc)| {
                        j.owners.contains(o) && anc.contains(named) && !j.recovery.get(o).is_some_and(|x| x.contains_key(r))
                    });
                    if owners.contains(o) && !owners.contains(r) && !cleared {
                        recovery.entry(*o).or_default().insert(*r, *named);
                    }
                }
            }
        }
        let challenges = heads[0]
            .0
            .challenges
            .keys()
            .filter(|k| owners.contains(k) && heads.iter().all(|(h, _)| h.challenges.contains_key(k)))
            .map(|k| (*k, heads.iter().map(|(h, _)| h.challenges[k]).max().unwrap()))
            .collect();
        removed.retain(|k, _| !owners.contains(k));
        Owners {
            removed,
            rule: heads.iter().map(|(h, _)| h.rule).max().unwrap_or_default(),
            recovery,
            challenges,
            owners,
            seq: heads.iter().map(|(h, _)| h.seq).max().unwrap_or(0),
            time: heads.iter().map(|(h, _)| h.time).max().unwrap_or(0),
        }
    }

    pub fn digest(&self) -> u64 {
        let mut v = vec![self.rule.code(), self.seq as u64, self.time, self.owners.len() as u64];
        v.extend(self.owners.iter().map(|&k| k as u64));
        for (o, r) in &self.recovery {
            v.extend([*o as u64, r.len() as u64]);
            v.extend(r.iter().flat_map(|(&k, &c)| [k as u64, c]));
        }
        for (k, t) in &self.challenges {
            v.extend([*k as u64 | 1 << 40, *t]);
        }
        v.extend(self.removed.iter().flat_map(|(&k, &c)| [k as u64 | 1 << 41, c]));
        fnv_u64s(v)
    }
}

/// What one verifier holds of a world's ownership: every valid certificate it has seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Book {
    pub world: u64,
    pub genesis: CertId,
    certs: BTreeMap<CertId, Cert>,
    states: BTreeMap<CertId, Owners>,
    /// Certificates waiting for a parent.
    orphans: BTreeMap<CertId, Cert>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    Applied,
    Duplicate,
    /// Held until its parents arrive.
    Waiting,
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
        let s = Owners { owners: set, rule: *rule, time: genesis.time, ..Default::default() };
        Ok(Book { world: id, genesis: id, certs: [(id, genesis)].into(), states: [(id, s)].into(), orphans: BTreeMap::new() })
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

    /// Every parent some held merge joined.
    fn merged(&self) -> BTreeSet<CertId> {
        self.certs.values().filter(|c| c.change == Change::Merge).flat_map(|c| c.prev.iter().copied()).collect()
    }

    /// Accept `c` if it is valid against the state at its parents. A certificate whose parent is
    /// missing waits; one that arrives completes any waiting descendants.
    pub fn insert(&mut self, c: Cert) -> Result<Insert, OwnError> {
        let r = self.try_insert(&c);
        match r {
            Ok(Insert::Applied) => {
                // Waiting descendants may now apply; a refused one is dropped.
                loop {
                    let ready: Vec<CertId> =
                        self.orphans.iter().filter(|(_, o)| o.prev.iter().all(|p| self.certs.contains_key(p))).map(|(id, _)| *id).collect();
                    if ready.is_empty() {
                        break;
                    }
                    for id in ready {
                        let o = self.orphans.remove(&id).unwrap();
                        let _ = self.try_insert(&o);
                    }
                }
            }
            Ok(Insert::Waiting) => {
                self.orphans.insert(c.id(), c);
            }
            _ => {}
        }
        r
    }

    fn try_insert(&mut self, c: &Cert) -> Result<Insert, OwnError> {
        if c.world != self.world {
            return Err(OwnError::WrongWorld);
        }
        let id = c.id();
        if self.certs.contains_key(&id) {
            return Ok(Insert::Duplicate);
        }
        if c.prev.is_empty() {
            return Err(OwnError::BadGenesis);
        }
        if !c.prev.iter().all(|p| self.certs.contains_key(p)) {
            return Ok(Insert::Waiting);
        }
        let s = if c.change == Change::Merge {
            let mut p = c.prev.clone();
            p.sort_unstable();
            p.dedup();
            if p.len() < 2 || p != c.prev {
                return Err(OwnError::BadLink);
            }
            let mut m = self.merge_of(&p);
            if c.seq != m.seq + 1 || c.time < m.time {
                return Err(OwnError::BadLink);
            }
            if m.approvals(&c.signers(), None).is_empty() {
                return Err(OwnError::Signers);
            }
            (m.seq, m.time) = (c.seq, c.time);
            m
        } else {
            if c.prev.len() == 1 && self.merged().contains(&c.prev[0]) {
                return Err(OwnError::Superseded);
            }
            self.states[&c.prev[0]].apply(c)?
        };
        self.certs.insert(id, c.clone());
        self.states.insert(id, s);
        Ok(Insert::Applied)
    }

    /// The live heads: held certificates with no live child. A non-merge certificate whose parent a
    /// merge joined does not count, nor does anything built on it (it arrived before the merge).
    pub fn heads(&self) -> Vec<CertId> {
        let merged = self.merged();
        let mut order: Vec<&Cert> = self.certs.values().collect();
        order.sort_by_key(|c| (c.seq, c.id()));
        let mut dead: BTreeSet<CertId> = BTreeSet::new();
        for c in &order {
            let stale = c.change != Change::Merge && c.prev.len() == 1 && merged.contains(&c.prev[0]);
            if stale || c.prev.iter().any(|p| dead.contains(p)) {
                dead.insert(c.id());
            }
        }
        let parents: BTreeSet<CertId> =
            self.certs.values().filter(|c| !dead.contains(&c.id())).flat_map(|c| c.prev.iter().copied()).collect();
        self.certs.keys().copied().filter(|id| !dead.contains(id) && !parents.contains(id)).collect()
    }

    pub fn view(&self) -> View {
        let heads = self.heads();
        let owners = match &heads[..] {
            [h] => self.states[h].clone(),
            hs => self.merge_of(hs),
        };
        View { disputed: heads.len() > 1, heads, owners }
    }

    /// Every held certificate `id` descends from, itself included.
    fn ancestors(&self, id: CertId) -> BTreeSet<CertId> {
        let mut seen = BTreeSet::new();
        let mut todo = vec![id];
        while let Some(x) = todo.pop() {
            if seen.insert(x) {
                todo.extend(self.certs.get(&x).map_or(&[][..], |c| &c.prev[..]));
            }
        }
        seen
    }

    fn merge_of(&self, heads: &[CertId]) -> Owners {
        let anc: Vec<BTreeSet<CertId>> = heads.iter().map(|&h| self.ancestors(h)).collect();
        Owners::merge(&heads.iter().zip(&anc).map(|(h, a)| (&self.states[h], a)).collect::<Vec<_>>())
    }

    /// Every pair of held certificates with one parent: transferable double-transfer proofs.
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

    /// The certificates from genesis to `id` along first parents, oldest first (a verifier's
    /// prefix for a fork proof; a merge's other parents are not included).
    pub fn path_to(&self, id: CertId) -> Vec<Cert> {
        let mut v = Vec::new();
        let mut at = Some(id);
        while let Some(x) = at {
            let c = &self.certs[&x];
            v.push(c.clone());
            at = c.prev.first().copied();
        }
        v.reverse();
        v
    }

    /// For the state hash: every held certificate (with signatures) and every waiting one.
    pub fn digest(&self) -> u64 {
        let mut v = vec![self.world, self.certs.len() as u64, self.orphans.len() as u64];
        v.extend(self.certs.values().map(Cert::digest));
        v.extend(self.orphans.values().map(Cert::digest));
        v.push(self.view().owners.digest());
        fnv_u64s(v)
    }
}

/// Check a whole chain, in any order. It must be complete (nothing waiting); the answer may be
/// disputed.
pub fn verify_chain(genesis: &Cert, rest: &[Cert]) -> Result<View, OwnError> {
    let mut b = Book::new(genesis.clone())?;
    for c in rest {
        b.insert(c.clone())?;
    }
    if !b.orphans.is_empty() {
        return Err(OwnError::MissingParent);
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
    /// `prefix`: the chain from genesis to the shared parent, in any order.
    pub fn verify(&self, prefix: &[Cert]) -> bool {
        let Some(g) = prefix.iter().find(|c| c.seq == 0) else { return false };
        let Ok(mut book) = Book::new(g.clone()) else { return false };
        for c in prefix.iter().filter(|c| c.seq > 0) {
            if book.insert(c.clone()) != Ok(Insert::Applied) {
                return false;
            }
        }
        book.world == self.world
            && self.a.prev.len() == 1
            && self.a.prev == self.b.prev
            && self.a.id() != self.b.id()
            && book.insert(self.a.clone()) == Ok(Insert::Applied)
            && book.insert(self.b.clone()) == Ok(Insert::Applied)
    }

    /// The keys that signed both branches: whoever a client's own policy may choose to distrust.
    pub fn signed_both(&self) -> BTreeSet<Key> {
        self.a.signers().intersection(&self.b.signers()).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Key = 10; // the seller / creator
    const B: Key = 20;
    const C: Key = 30;
    const X: Key = 40;
    const R: Key = 50; // a recovery key
    const EVE: Key = 66;

    fn next(book: &Book, prev: CertId, change: Change, time: u64, keys: &[Key]) -> Cert {
        let seq = book.cert(prev).unwrap().seq + 1;
        Cert { world: book.world, seq, prev: vec![prev], change, time, sigs: vec![] }.signed(keys)
    }

    fn head(b: &Book) -> CertId {
        let h = b.heads();
        assert_eq!(h.len(), 1, "expected a linear chain");
        h[0]
    }

    fn add(b: &mut Book, change: Change, keys: &[Key]) -> Result<Insert, OwnError> {
        let h = head(b);
        let t = b.cert(h).unwrap().time + 1;
        let c = next(b, h, change, t, keys);
        b.insert(c)
    }

    fn owners(b: &Book) -> Vec<Key> {
        b.view().owners.owners.into_iter().collect()
    }

    #[test]
    fn genesis_binds_the_world_id() {
        let g = Cert::genesis(&[S, B], SignRule::All, 1);
        let b = Book::new(g.clone()).unwrap();
        assert_eq!(b.world, g.id());
        assert_eq!(owners(&b), [S, B]);
        // Not signed by every creator, or the same people on another world.
        let mut half = g.clone();
        half.sigs.pop();
        assert_eq!(Book::new(half), Err(OwnError::BadGenesis));
        assert_ne!(Cert::genesis(&[S, B], SignRule::All, 2).id(), b.world);
        // A certificate of another world never applies here.
        let other = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let mut b2 = b.clone();
        assert_eq!(b2.insert(next(&other, other.genesis, Change::Add { key: B }, 1, &[S])), Err(OwnError::WrongWorld));
    }

    #[test]
    fn forged_transfer_and_rogue_co_owner_are_refused() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        // A stranger signs their own Add.
        assert_eq!(add(&mut b, Change::Add { key: EVE }, &[EVE]), Err(OwnError::Signers));
        // A forged signature (right key, wrong bytes) counts for nothing.
        let h = head(&b);
        let mut forged = next(&b, h, Change::Add { key: EVE }, 1, &[]);
        forged.sigs.push((S, 12345));
        assert_eq!(b.insert(forged), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Add { key: B }, &[S]), Ok(Insert::Applied));
        // Rule A: a co-owner alone can't add a sock puppet, nor remove the other owner.
        assert_eq!(add(&mut b, Change::Add { key: EVE }, &[B]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Remove { key: S }, &[B, EVE]), Ok(Insert::Applied));
        assert_eq!(owners(&b), [B]);
    }

    #[test]
    fn removed_or_departed_owner_is_a_stranger() {
        let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        assert_eq!(add(&mut b, Change::Remove { key: C }, &[S]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Remove { key: C }, &[S, B]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Leave { key: S }, &[S]), Ok(Insert::Applied));
        assert_eq!(owners(&b), [B]);
        // C (removed) and S (left) sign an Add, with and without B's signature.
        assert_eq!(add(&mut b, Change::Add { key: X }, &[S, C]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Remove { key: B }, &[S, C]), Err(OwnError::LastOwner));
        assert_eq!(add(&mut b, Change::Add { key: X }, &[B, S, C]), Ok(Insert::Applied));
        // The last owner can't leave.
        assert_eq!(add(&mut b, Change::Leave { key: X }, &[X]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Leave { key: B }, &[B]), Err(OwnError::LastOwner));
    }

    #[test]
    fn recovery_key_signs_for_its_owner_but_never_adds_itself() {
        let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
        assert_eq!(add(&mut b, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]), Ok(Insert::Applied));
        // S lost its key: R signs in its place.
        assert_eq!(add(&mut b, Change::Add { key: C }, &[R, B]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Add { key: R }, &[R, B, C]), Err(OwnError::Signers));
        // Another owner can't name S's recovery key.
        assert_eq!(add(&mut b, Change::SetRecovery { owner: S, recovery: Some(EVE) }, &[B]), Err(OwnError::Signers));
        // With S's own signature the recovery key may become an owner, and stops being one.
        assert_eq!(add(&mut b, Change::Add { key: R }, &[S, B, C]), Ok(Insert::Applied));
        assert!(b.view().owners.recovery.is_empty());
    }

    #[test]
    fn objection_window() {
        let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        assert_eq!(add(&mut b, Change::SetRecovery { owner: C, recovery: Some(R) }, &[C]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Challenge { key: C }, &[S, B]), Err(OwnError::HasRecovery));
        // No window against S without every other owner.
        assert_eq!(add(&mut b, Change::Challenge { key: S }, &[B]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Challenge { key: S }, &[B, C]), Ok(Insert::Applied));
        let opened = b.view().owners.challenges[&S];
        let h = head(&b);
        let early = next(&b, h, Change::RemoveDormant { key: S }, opened + OBJECTION_FRAMES - 1, &[B, C]);
        assert_eq!(b.clone().insert(early), Err(OwnError::WindowOpen));
        // S signs anything during the window: it is cancelled.
        let mut b2 = b.clone();
        assert_eq!(add(&mut b2, Change::SetRule { rule: SignRule::All }, &[S, B, C]), Ok(Insert::Applied));
        let h2 = head(&b2);
        let late = next(&b2, h2, Change::RemoveDormant { key: S }, opened + OBJECTION_FRAMES, &[B, C]);
        assert_eq!(b2.insert(late), Err(OwnError::WindowOpen));
        // S stays silent: the others remove it once the window has run.
        let on_time = next(&b, h, Change::RemoveDormant { key: S }, opened + OBJECTION_FRAMES, &[B, C]);
        assert_eq!(b.insert(on_time), Ok(Insert::Applied));
        assert_eq!(owners(&b), [B, C]);
    }

    /// The sole owner sells twice on two disconnected copies of the world.
    fn double_sale() -> (Cert, Vec<Cert>) {
        let g = Cert::genesis(&[S], SignRule::All, 7);
        let b = Book::new(g.clone()).unwrap();
        let to_b = next(&b, b.genesis, Change::Add { key: B }, 10, &[S]);
        let to_c = next(&b, b.genesis, Change::Add { key: C }, 12, &[S]);
        let mut bb = b.clone();
        bb.insert(to_b.clone()).unwrap();
        let b_out = next(&bb, to_b.id(), Change::Leave { key: S }, 11, &[S]);
        let mut bc = b.clone();
        bc.insert(to_c.clone()).unwrap();
        let c_out = next(&bc, to_c.id(), Change::Leave { key: S }, 13, &[S]);
        (g, vec![to_b, b_out, to_c, c_out])
    }

    #[test]
    fn double_sale_is_detected_and_merges_in_any_order() {
        let (g, certs) = double_sale();
        let mut views = BTreeSet::new();
        // Every arrival order, parents after children included.
        for perm in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2], [3, 1, 2, 0]] {
            let order: Vec<Cert> = perm.iter().map(|&i| certs[i].clone()).collect();
            let v = verify_chain(&g, &order).unwrap();
            assert!(v.disputed);
            views.insert((v.owners.owners.iter().copied().collect::<Vec<_>>(), v.owners.digest()));
        }
        assert_eq!(views.len(), 1);
        assert_eq!(views.into_iter().next().unwrap().0, [B, C]);
        // The fork proof convinces a third party that holds only the genesis certificate.
        let mut b = Book::new(g.clone()).unwrap();
        for c in &certs {
            b.insert(c.clone()).unwrap();
        }
        let forks = b.forks();
        assert_eq!(forks.len(), 1);
        let p = &forks[0];
        assert!(p.verify(&[g.clone()]));
        assert_eq!(p.signed_both(), [S].into());
        // A tampered half convinces nobody.
        let mut bad = p.clone();
        bad.b.sigs.clear();
        assert!(!bad.verify(&[g.clone()]));
        // Either buyer merges; afterwards nobody extends a merged head.
        let heads = b.heads();
        let m = Cert { world: b.world, seq: 3, prev: heads.clone(), change: Change::Merge, time: 20, sigs: vec![] };
        assert_eq!(b.insert(m.clone().signed(&[EVE])), Err(OwnError::Signers));
        assert_eq!(b.insert(m.clone().signed(&[B])), Ok(Insert::Applied));
        let v = b.view();
        assert!(!v.disputed);
        assert_eq!(v.owners.owners.iter().copied().collect::<Vec<_>>(), [B, C]);
        let stale = next(&b, heads[0], Change::Add { key: EVE }, 21, &[B, C]);
        assert_eq!(b.insert(stale), Err(OwnError::Superseded));
        // Rule A now runs over the union: B alone can't add anyone.
        assert_eq!(add(&mut b, Change::Add { key: X }, &[B]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Add { key: X }, &[B, C]), Ok(Insert::Applied));
    }

    #[test]
    fn a_stale_extension_that_arrives_before_the_merge_stops_counting() {
        let (g, certs) = double_sale();
        let mut b = Book::new(g.clone()).unwrap();
        for c in &certs {
            b.insert(c.clone()).unwrap();
        }
        let heads = b.heads();
        let m = Cert { world: b.world, seq: 3, prev: heads.clone(), change: Change::Merge, time: 20, sigs: vec![] }.signed(&[C]);
        // B (sole owner of its branch) adds EVE without C, offline, before the merge reaches it.
        let mut early = b.clone();
        let sneak = next(&early, certs[1].id(), Change::Add { key: EVE }, 21, &[B]);
        assert_eq!(early.insert(sneak.clone()), Ok(Insert::Applied));
        assert!(early.view().owners.owners.contains(&EVE));
        early.insert(m.clone()).unwrap();
        b.insert(m).unwrap();
        assert_eq!(b.insert(sneak), Err(OwnError::Superseded));
        assert_eq!(early.view(), b.view());
        assert!(!b.view().owners.owners.contains(&EVE));
    }

    #[test]
    fn benign_fork_under_rule_b_loses_nobody() {
        let g = Cert::genesis(&[S, B], SignRule::Any, 3);
        let b = Book::new(g.clone()).unwrap();
        let x = next(&b, b.genesis, Change::Add { key: X }, 5, &[S]);
        let c = next(&b, b.genesis, Change::Add { key: C }, 5, &[B]);
        let v = verify_chain(&g, &[c, x]).unwrap();
        assert!(v.disputed);
        assert_eq!(v.owners.owners.into_iter().collect::<Vec<_>>(), [S, B, C, X]);
        // Rule B has no removal.
        let mut b2 = b.clone();
        assert_eq!(add(&mut b2, Change::Remove { key: S }, &[B]), Err(OwnError::NotAllowed));
    }

    #[test]
    fn removal_survives_a_fork_by_its_target() {
        // S and B remove X; X, offline, signs a recovery key on the same parent and publishes it.
        let g = Cert::genesis(&[S, B, X], SignRule::All, 4);
        let b = Book::new(g.clone()).unwrap();
        let rm = next(&b, b.genesis, Change::Remove { key: X }, 5, &[S, B]);
        let revive = next(&b, b.genesis, Change::SetRecovery { owner: X, recovery: Some(R) }, 5, &[X]);
        let v = verify_chain(&g, &[revive, rm]).unwrap();
        assert_eq!(v.owners.owners.into_iter().collect::<Vec<_>>(), [S, B]);
        // Two owners who removed each other on two copies: nobody is left out rather than everyone.
        let g2 = Cert::genesis(&[S, B], SignRule::All, 5);
        let b2 = Book::new(g2.clone()).unwrap();
        let a = next(&b2, b2.genesis, Change::Remove { key: B }, 5, &[S]);
        let c = next(&b2, b2.genesis, Change::Remove { key: S }, 5, &[B]);
        assert_eq!(verify_chain(&g2, &[a, c]).unwrap().owners.owners.into_iter().collect::<Vec<_>>(), [S, B]);
    }

    #[test]
    fn merge_is_causal() {
        // Found by the session scenario: C was removed in the shared prefix and added back on one
        // branch; the other branch's old removal must not take C out again.
        let g = Cert::genesis(&[S, B, C], SignRule::All, 6);
        let mut b = Book::new(g.clone()).unwrap();
        assert_eq!(add(&mut b, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Remove { key: C }, &[S, B]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Leave { key: B }, &[B]), Ok(Insert::Applied));
        let fork = head(&b);
        let c_back = next(&b, fork, Change::Add { key: C }, 10, &[S]);
        let x_in = next(&b, fork, Change::Add { key: X }, 10, &[S]);
        let mut b2 = b.clone();
        b2.insert(c_back.clone()).unwrap();
        // On C's branch S also clears its recovery key; the other branch saw it named and kept it.
        let clear = next(&b2, c_back.id(), Change::SetRecovery { owner: S, recovery: None }, 11, &[S, C]);
        let v = verify_chain(&g, &b.path_to(fork)[1..].iter().cloned().chain([x_in, c_back, clear]).collect::<Vec<_>>()).unwrap();
        assert!(v.disputed);
        assert_eq!(v.owners.owners.iter().copied().collect::<Vec<_>>(), [S, C, X]);
        assert!(v.owners.recovery.is_empty(), "a branch that saw R named cleared it");
        // A recovery key named on one branch only survives the merge.
        let g3 = Cert::genesis(&[S, B], SignRule::All, 8);
        let b3 = Book::new(g3.clone()).unwrap();
        let named = next(&b3, b3.genesis, Change::SetRecovery { owner: S, recovery: Some(R) }, 5, &[S]);
        let other = next(&b3, b3.genesis, Change::Add { key: X }, 5, &[S, B]);
        let v3 = verify_chain(&g3, &[other, named.clone()]).unwrap();
        assert_eq!(v3.owners.recovery, [(S, [(R, named.id())].into())].into());
    }

    #[test]
    fn tampered_or_reordered_chains_are_refused() {
        let g = Cert::genesis(&[S], SignRule::All, 9);
        let mut b = Book::new(g.clone()).unwrap();
        let c1 = next(&b, b.genesis, Change::Add { key: B }, 5, &[S]);
        b.insert(c1.clone()).unwrap();
        let c2 = next(&b, c1.id(), Change::Leave { key: S }, 6, &[S]);
        // A changed field breaks its signatures.
        let mut t = c1.clone();
        t.change = Change::Add { key: EVE };
        assert_eq!(verify_chain(&g, &[t, c2.clone()]), Err(OwnError::Signers));
        // A chain with a gap never completes; time can't run backwards; seq must follow.
        assert_eq!(verify_chain(&g, &[c2.clone()]), Err(OwnError::MissingParent));
        let back = next(&b, c1.id(), Change::Leave { key: S }, 4, &[S]);
        assert_eq!(b.clone().insert(back), Err(OwnError::BadLink));
        let mut skip = next(&b, c1.id(), Change::Leave { key: S }, 6, &[]);
        skip.seq = 5;
        assert_eq!(b.clone().insert(skip.signed(&[S])), Err(OwnError::BadLink));
        // In order or reversed, the valid chain gives the same answer.
        assert_eq!(verify_chain(&g, &[c1.clone(), c2.clone()]), verify_chain(&g, &[c2, c1]));
    }

    #[test]
    fn wire_round_trip() {
        let (g, mut certs) = double_sale();
        certs.push(g);
        certs.push(Cert { world: 5, seq: 9, prev: vec![1, 2], change: Change::Merge, time: 3, sigs: vec![] }.signed(&[S]));
        for change in [
            Change::SetRule { rule: SignRule::Threshold(3) },
            Change::SetRecovery { owner: S, recovery: Some(0) },
            Change::SetRecovery { owner: S, recovery: None },
            Change::Remove { key: X },
            Change::Challenge { key: X },
            Change::RemoveDormant { key: X },
        ] {
            certs.push(Cert { world: 5, seq: 1, prev: vec![1], change, time: 3, sigs: vec![] }.signed(&[S, B]));
        }
        for c in certs {
            assert_eq!(Cert::from_words(&c.to_words()), Some(c));
        }
        assert_eq!(Cert::from_words(&[1, 2, 3]), None);
    }

    #[test]
    fn threshold_rule_c() {
        let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        assert_eq!(add(&mut b, Change::SetRule { rule: SignRule::Threshold(2) }, &[S, B]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::SetRule { rule: SignRule::Threshold(2) }, &[S, B, C]), Ok(Insert::Applied));
        assert_eq!(add(&mut b, Change::Add { key: X }, &[S]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Add { key: X }, &[S, B]), Ok(Insert::Applied));
        // A two-of-four majority removes an active owner without its signature.
        assert_eq!(add(&mut b, Change::Remove { key: C }, &[S, C]), Err(OwnError::Signers));
        assert_eq!(add(&mut b, Change::Remove { key: C }, &[S, B]), Ok(Insert::Applied));
        assert_eq!(owners(&b), [S, B, X]);
    }
}

/// Review OWN-1..12 attacks, written against ade1ba6's API to show they fail before the fix.
#[cfg(test)]
mod attacks_before {
    use super::*;
    const S: Key = 10;
    const B: Key = 20;
    const C: Key = 30;
    const I: Key = 31;
    const J: Key = 32;
    const R: Key = 50;
    const R2: Key = 51;
    const EVE: Key = 66;

    fn next(book: &Book, prev: CertId, change: Change, time: u64, keys: &[Key]) -> Cert {
        let seq = book.cert(prev).unwrap().seq + 1;
        Cert { world: book.world, seq, prev: vec![prev], change, time, sigs: vec![] }.signed(keys)
    }
    fn owners(b: &Book) -> Vec<Key> {
        b.view().owners.owners.into_iter().collect()
    }
    fn chain(b: &mut Book, prev: CertId, change: Change, keys: &[Key]) -> CertId {
        let t = b.cert(prev).unwrap().time + 1;
        let c = next(b, prev, change, t, keys);
        let id = c.id();
        assert_eq!(b.insert(c), Ok(Insert::Applied));
        id
    }

    #[test]
    fn own1_merge_rolls_back() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let c1 = chain(&mut b, g, Change::Add { key: B }, &[S]);
        chain(&mut b, c1, Change::Leave { key: S }, &[S]);
        let mut p = vec![g, c1];
        p.sort_unstable();
        let m = Cert { world: b.world, seq: 2, prev: p, change: Change::Merge, time: 9, sigs: vec![] }.signed(&[S]);
        let _ = b.insert(m);
        assert_eq!(owners(&b), [B]);
    }

    #[test]
    fn own2_leave_revived_by_fork() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let c1 = chain(&mut b, g, Change::Add { key: B }, &[S]);
        chain(&mut b, c1, Change::Leave { key: S }, &[S]);
        let _ = b.insert(next(&b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, 0, &[S]));
        assert_eq!(owners(&b), [B]);
    }

    #[test]
    fn own3_old_state_fork_bypasses_later_owner() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let c1 = chain(&mut b, g, Change::Add { key: I }, &[S]);
        chain(&mut b, c1, Change::Add { key: J }, &[S, I]);
        // S forks at c1 and removes I alone: J never agreed.
        let _ = b.insert(next(&b, c1, Change::Remove { key: I }, 2, &[S]));
        assert!(owners(&b).contains(&I));
    }

    #[test]
    fn own3_stale_branch_owner_adds_alone() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let tb = chain(&mut b, g, Change::Add { key: B }, &[S]);
        let hb = chain(&mut b, tb, Change::Leave { key: S }, &[S]);
        let _ = b.insert(next(&b, g, Change::Add { key: C }, 1, &[S]));
        // B, sole owner on its branch, adds EVE without C.
        let _ = b.insert(next(&b, hb, Change::Add { key: EVE }, 3, &[B]));
        assert!(!owners(&b).contains(&EVE));
    }

    #[test]
    fn own4_two_owners_remove_each_other() {
        let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let _ = b.insert(next(&b, g, Change::Remove { key: S }, 1, &[B]));
        assert_eq!(owners(&b), [S, B]);
    }

    #[test]
    fn own5_arrival_order_changes_owners() {
        let g = Cert::genesis(&[S], SignRule::All, 1);
        let b0 = Book::new(g).unwrap();
        let tb = next(&b0, b0.genesis, Change::Add { key: B }, 1, &[S]);
        let tc = next(&b0, b0.genesis, Change::Add { key: C }, 1, &[S]);
        let x = next(&b0, b0.genesis, Change::Add { key: EVE }, 1, &[S]);
        let mut p = vec![tb.id(), tc.id()];
        p.sort_unstable();
        let m = Cert { world: b0.world, seq: 2, prev: p, change: Change::Merge, time: 2, sigs: vec![] }.signed(&[B]);
        let ext = Cert { world: b0.world, seq: 2, prev: vec![tb.id()], change: Change::SetRecovery { owner: B, recovery: Some(R) }, time: 2, sigs: vec![] }.signed(&[B]);
        let mut a = b0.clone();
        let mut z = b0.clone();
        for c in [&tb, &tc, &x, &ext, &m] {
            let _ = a.insert(c.clone());
        }
        for c in [&tb, &tc, &x, &m, &ext] {
            let _ = z.insert(c.clone());
        }
        assert_eq!(a.view(), z.view());
        assert_eq!(a.digest(), z.digest());
    }

    #[test]
    fn own6_recovery_key_takes_over() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let c1 = chain(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]);
        assert!(b.insert(next(&b, c1, Change::Add { key: R2 }, 2, &[R])).is_err());
    }

    #[test]
    fn own6_rotated_recovery_key_survives() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let c1 = chain(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S]);
        chain(&mut b, c1, Change::SetRecovery { owner: S, recovery: Some(R2) }, &[S]);
        let _ = b.insert(next(&b, c1, Change::Add { key: EVE }, 2, &[R]));
        assert!(!owners(&b).contains(&EVE));
    }

    #[test]
    fn own7_backdated_window() {
        let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let ch = chain(&mut b, g, Change::Challenge { key: S }, &[B, C]);
        // No session time ever passed: the signers just claim it.
        let r = b.insert(next(&b, ch, Change::RemoveDormant { key: S }, 2 + OBJECTION_FRAMES, &[B, C]));
        assert!(r.is_err());
    }

    #[test]
    fn own9_fork_proof_prefix_order_and_merges() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let g = b.genesis;
        let c1 = chain(&mut b, g, Change::Add { key: B }, &[S]);
        let c2 = chain(&mut b, c1, Change::Add { key: C }, &[S, B]);
        let x = next(&b, c2, Change::Add { key: I }, 5, &[S, B, C]);
        let y = next(&b, c2, Change::Add { key: J }, 5, &[S, B, C]);
        let p = ForkProof { world: b.world, a: x, b: y };
        let mut prefix = b.path_to(c2);
        prefix.reverse();
        assert!(p.verify(&prefix));
    }

    #[test]
    fn own10_orphans_are_not_stored() {
        let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
        let orphan = Cert { world: b.world, seq: 5, prev: vec![12345], change: Change::Add { key: EVE }, time: 1, sigs: vec![] };
        assert_eq!(b.insert(orphan), Err(OwnError::MissingParent));
    }

    #[test]
    fn own11_trailing_words_refused() {
        let mut w = Cert::genesis(&[S], SignRule::All, 1).to_words();
        w.push(7);
        assert_eq!(Cert::from_words(&w), None);
    }
}
