//! Layer 2: the members, as a Stateright model (docs/spec/divergence.md 9.3a, 9.5 P-M2 and P-M3; @diverge-eng's
//! package delta r2: Fix 1, "Two successive packages", the retry). Layer 1 checks the merge function; this checks
//! what the members do with it: when each one learns a merge is final, fetches its package, applies it, and votes.
//!
//! **Abstracted** [I]: the agreement is a growing final log of cuts, one height per frame, each including any of the
//! merge entries submitted and not yet included (models v0 and v1 check that the log is agreed). A member votes on a
//! cut it knows is final, with the packages in effect there: that stands for the root it votes with on the cut's
//! proposal. The world state is the list of packages applied by each frame: the merge function's result is a pure
//! function of its package (layer 1), so the package stands for it.
//!
//! **The members:** two honest voters, 0 and 1, and the returner B, which submits merge entries naming packages and
//! serves their contents to fetches. In one scenario B is faulty and serves a second content under a package's id.
//!
//! **The rules modelled** (each with a mutation that drops it):
//! - **A package's id binds its content** (r2 Fix 1: `P`'s id is the hash of its signed commitment): a fetch is
//!   accepted only if the content hashes to the id. `UnboundPackageId` makes the id the segment alone.
//! - **One package per segment:** a second package for a segment already merged is refused. `AcceptSecondPackage`
//!   applies both.
//! - **Once:** a package applies once, however many merge entries name it (the retry). `NoRetryDedupe` applies it
//!   per merge entry.
//! - **Two successive packages:** `M2` applies only after `M1` has applied, at `M1`'s application frame or later.
//!   `M2AfterFinalOnly` applies `M2` at its own frame as soon as `M1` is merely final.
//! - **`M2` is decided at its own final cut** (proposed to @diverge-eng, after @world-chunks's review): if `M1`
//!   isn't final and accepted at or below that cut, `M2` is refused, and its id and segment
//!   stay free for a later entry. `M2WaitsForPredecessor`, r2's rule as written, waits for `M1` instead: when `M1` is
//!   refused for good (stamped too low, a second package for its segment, or void), no member votes again.
//! - **The vote waits for the merge** (9.3a): a member votes on a cut only once it can compute what is in effect
//!   there: every merge final here that applies at or below the cut, fetched and applied. `VoteBeforeMerge` votes
//!   with what it has.
//! - **`M` is stamped above its intents** (r2 Fix 1): a merge entry stamped at or below an intent of its package is
//!   refused, so no intent takes effect at or before its own stamp. `MBelowIntents` accepts it.
//! - **Q1(b)'s originals are deduplicated across packages** (r2, Q1(b)'s conditions): a parent original that two
//!   packages' `Branch`es both bind applies once. `NoOriginalDedupe` applies it per package.
//!
//! A cut at height `h` includes only merge entries stamped at or below `h`, and an author's entries only as a prefix
//! of their chain. The agreement may void an entry a scenario marks voidable (late, after a leave, its author
//! removed): it is never final, and its chain goes on. Entries on two chains stand for B's `Rechain` (or a relay):
//! only then can `M2` be final before `M1`.
//!
//! **Abstracted:** heights are frames here (one cut per frame); the application frame is the final cut's frame plus
//! `D` frames, which 9.3a states in frames, never in heights (@diverge-eng, point 3).
//!
//! **Availability** (9.3a, ruled by @coordinator-opus: (b) with (a) as the fetch path, and
//! @fable-reviewer's amendments): a merge entry joins a cut only once a quorum holds its package (B and at least one
//! voter: voters pre-fetch it, and a member votes a cut that includes `M` only if it holds the package), and any
//! holder serves a fetch, verified by hash. B may go silent. `IncludeWithoutQuorum` drops (b); `FetchFromBOnly`
//! drops (a); either lets a silent B leave a final package no live member serves. `HoldAllChains` makes the whole
//! cut wait for `M`'s quorum, where only B's chain may wait (amendment 1): a voter's own entry is then held back.
//! `NoFetchHashCheck` accepts a fetched package whose bytes don't match its id (amendment 2): a faulty B serving two
//! versions of one of its entries then splits the roots.

use std::collections::{BTreeMap, BTreeSet};

use stateright::{Model, Property};

/// The application delay `D`, in cuts: a merge final at height `F_M` applies at `F_M + D` (9.3a).
pub const D: u8 = 1;
/// The highest cut the log grows to.
pub const MAX_H: u8 = 4;
/// The honest voters.
pub const VOTERS: [usize; 2] = [0, 1];

/// A package id on the wire.
pub type Id = u32;

/// A package: its segment, its content (what the merge of it gives), its predecessor, if it is a second package
/// (r2: `P2`'s commitment names `P1`'s id), as an index into the scenario's packages, the highest stamp of its
/// intents, and the parent originals its `Branch` binds (Q1(b)), as a bit set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Package {
    pub segment: u8,
    pub content: u8,
    pub pred: Option<u8>,
    pub top: u8,
    pub originals: u8,
}

impl Package {
    /// Its id: the hash of its commitment, so of every field (injective here). Under `UnboundPackageId`, the
    /// segment alone.
    pub fn id(&self, m: Mutation) -> Id {
        if m == Mutation::UnboundPackageId {
            return Id::from(self.segment);
        }
        let pred = self.pred.map_or(0, |p| 1 + Id::from(p));
        Id::from(self.segment)
            | Id::from(self.content) << 4
            | pred << 8
            | Id::from(self.top) << 12
            | Id::from(self.originals) << 16
    }
}

/// A merge entry: it names a package, by its index in the scenario (its id on the wire is [`Package::id`]), and
/// carries its own stamp. Two entries may name one package (B's retry after a lost ACK). [`PLAIN`] as the package
/// makes it an ordinary entry of another member's chain, naming no package.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MergeEntry {
    pub n: u8,
    pub package: u8,
    pub stamp: u8,
    /// Its chain: B's (0), or a new one after a `Rechain` (1).
    pub chain: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mutation {
    None,
    UnboundPackageId,
    AcceptSecondPackage,
    NoRetryDedupe,
    M2AfterFinalOnly,
    VoteBeforeMerge,
    MBelowIntents,
    NoOriginalDedupe,
    M2WaitsForPredecessor,
    IncludeWithoutQuorum,
    FetchFromBOnly,
    HoldAllChains,
    NoFetchHashCheck,
}

/// A scenario: the packages B seals and the merge entries it submits, and, for a faulty B, a second content it may
/// serve under the first package's id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Scenario {
    pub name: &'static str,
    pub packages: Vec<Package>,
    pub entries: Vec<MergeEntry>,
    pub alt: Option<Package>,
    /// The entries (by `n`) the agreement may void.
    pub voidable: Vec<u8>,
    /// Whether B may go silent (stop serving fetches) at any point.
    pub may_silence: bool,
}

/// The package index of a plain entry, which names no package.
pub const PLAIN: u8 = u8::MAX;

/// The members needed to hold a package before its merge entry joins a cut: two of the three (B and the voters).
pub const QUORUM: usize = 2;

/// Who answers a fetch: B with the package it sealed, a faulty B with its other one, or a voter that holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Src {
    B,
    BAlt,
    Peer(usize),
}

/// A member's local state.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Member {
    /// The highest final cut it knows.
    pub known: u8,
    /// The package contents it fetched and accepted, by id.
    pub fetched: BTreeMap<Id, Package>,
    /// The packages it applied: (application frame, id, the package).
    pub applied: Vec<(u8, Id, Package)>,
    /// The parent originals it applied through them: (application frame, original).
    pub originals: Vec<(u8, u8)>,
    /// Its votes: cut height -> the packages in effect there.
    pub votes: BTreeMap<u8, Vec<(Id, Package)>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct State {
    /// The final log: each cut's included merge entries.
    pub log: Vec<Vec<MergeEntry>>,
    /// The entries the agreement voided: never final.
    pub voided: BTreeSet<MergeEntry>,
    /// Whether B has gone silent.
    pub silent: bool,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// The next cut becomes final, including these merge entries.
    Cut(Vec<MergeEntry>),
    /// The agreement voids an entry.
    Void(MergeEntry),
    /// A member learns the next final cut.
    Learn(usize),
    /// A member's fetch of a package by id, answered by a source; before the entry is final it is a pre-fetch.
    Fetch(usize, Id, Src),
    /// B goes silent.
    Silence,
    /// A member votes on a cut.
    Vote(usize, u8),
}

pub struct Members {
    pub scenario: Scenario,
    pub m: Mutation,
}

/// One scheduled merge at a member: where it applies (`None` while its predecessor isn't final there), its own frame
/// `F_M + D`, and its package's id.
type Scheduled = (Option<u8>, u8, Id);

impl Members {
    fn package_by_id(&self, id: Id) -> Option<Package> {
        self.scenario.packages.iter().copied().find(|p| p.id(self.m) == id)
    }

    /// The id a merge entry names on the wire.
    fn id_of(&self, e: &MergeEntry) -> Option<Id> {
        self.scenario.packages.get(usize::from(e.package)).map(|p| p.id(self.m))
    }

    fn pred_id(&self, p: &Package) -> Option<Id> {
        p.pred.and_then(|i| self.scenario.packages.get(usize::from(i))).map(|q| q.id(self.m))
    }

    /// The merge entries a member knows are final, with the height that first included each, in log order; within a
    /// cut, first packages before second ones.
    fn final_entries(&self, s: &State, me: usize) -> Vec<(u8, MergeEntry)> {
        let mut out = Vec::new();
        for (h, cut) in s.log.iter().enumerate().take(usize::from(s.members[me].known)) {
            for e in cut {
                out.push((h as u8 + 1, *e));
            }
        }
        let second =
            |e: &MergeEntry| self.id_of(e).and_then(|i| self.package_by_id(i)).is_some_and(|p| p.pred.is_some());
        out.sort_by_key(|(h, e)| (*h, second(e)));
        out
    }

    /// Who would answer a member's fetch of `id` now: B while it isn't silent, and (a) any voter that holds it.
    fn sources(&self, s: &State, me: usize, id: Id) -> Vec<Src> {
        let mut out = Vec::new();
        if !s.silent {
            out.push(Src::B);
            if self.scenario.alt.is_some() {
                out.push(Src::BAlt);
            }
        }
        if self.m != Mutation::FetchFromBOnly {
            out.extend(
                (0..s.members.len()).filter(|&q| q != me && s.members[q].fetched.contains_key(&id)).map(Src::Peer),
            );
        }
        out
    }

    /// (b): a merge entry joins a cut only once a quorum holds its package: B, plus the voters that fetched it.
    fn held_by_quorum(&self, s: &State, e: &MergeEntry) -> bool {
        let Some(id) = self.id_of(e) else { return true };
        self.m == Mutation::IncludeWithoutQuorum
            || 1 + s.members.iter().filter(|m| m.fetched.contains_key(&id)).count() >= QUORUM
    }

    /// Whether the next cut may include exactly these entries: each chain stays a prefix, and each merge entry's
    /// package is held by a quorum. Under `HoldAllChains`, nothing joins while a ready merge entry lacks its quorum.
    fn cut_allowed(&self, s: &State, cut: &[MergeEntry]) -> bool {
        if !cut.iter().all(|e| self.chain_ready(s, e, cut) && self.held_by_quorum(s, e)) {
            return false;
        }
        if self.m == Mutation::HoldAllChains && !cut.is_empty() {
            let included: BTreeSet<MergeEntry> = s.log.iter().flatten().copied().collect();
            let waiting = self.scenario.entries.iter().any(|e| {
                e.package != PLAIN
                    && !included.contains(e)
                    && !s.voided.contains(e)
                    && self.chain_ready(s, e, &[])
                    && !self.held_by_quorum(s, e)
            });
            return !waiting;
        }
        true
    }

    /// Whether `e` may join the next cut: its chain's earlier entries are included or void, or join with it.
    fn chain_ready(&self, s: &State, e: &MergeEntry, with: &[MergeEntry]) -> bool {
        let done = |x: &MergeEntry| s.log.iter().flatten().any(|y| y == x) || s.voided.contains(x) || with.contains(x);
        self.scenario.entries.iter().filter(|x| x.chain == e.chain && x.n < e.n).all(done)
    }

    /// Where each merge known final applies at this member, in log order, deduplicated by id and by segment (the
    /// first final one wins). A merge stamped at or below one of its intents is refused. A second package applies at
    /// or after its predecessor, never below its own frame.
    fn schedule(&self, s: &State, me: usize) -> Vec<Scheduled> {
        let mut out: Vec<Scheduled> = Vec::new();
        let mut seen_ids = BTreeSet::new();
        let mut seen_segments: BTreeMap<u8, Id> = BTreeMap::new();
        let old_rule = matches!(self.m, Mutation::M2AfterFinalOnly | Mutation::M2WaitsForPredecessor);
        for (fm, e) in self.final_entries(s, me) {
            let Some(id) = self.id_of(&e) else { continue };
            let Some(p) = self.package_by_id(id) else { continue };
            // `M` is stamped above every intent of its package (r2 Fix 1); the id binds the intents' stamps, so
            // every member refuses the same entries.
            if self.m != Mutation::MBelowIntents && e.stamp <= p.top {
                continue;
            }
            // `M2` is decided at its own final cut: its predecessor must be final and accepted at or below it (it
            // is then earlier in this list). Else it is refused, and takes neither its id nor its segment.
            let pred_at = match self.pred_id(&p) {
                None => None,
                Some(pred) => match out.iter().find(|(_, _, i)| *i == pred) {
                    Some((Some(pa), ..)) => Some(*pa),
                    _ if old_rule => None,
                    _ => continue,
                },
            };
            if self.m != Mutation::NoRetryDedupe && !seen_ids.insert(id) {
                continue;
            }
            if self.m != Mutation::AcceptSecondPackage {
                if let Some(first) = seen_segments.get(&p.segment)
                    && *first != id
                {
                    continue;
                }
                seen_segments.insert(p.segment, id);
            }
            let own = fm + D;
            let at = match (self.pred_id(&p), pred_at) {
                (None, _) => Some(own),
                (Some(_), Some(pa)) if !old_rule => Some(own.max(pa)),
                _ => None,
            };
            out.push((at, own, id));
        }
        if !old_rule {
            return out;
        }
        // The old rules (the controls): a second package's frame, once its predecessor is final here, wherever it
        // sits in the log, and waiting for it until then.
        for _ in 0..out.len() {
            for k in 0..out.len() {
                let (at, own, id) = out[k];
                let Some(pred) = self.package_by_id(id).and_then(|p| self.pred_id(&p)) else { continue };
                if at.is_some() {
                    continue;
                }
                let found = out.iter().find(|(_, _, i)| *i == pred).copied();
                out[k].0 = match found {
                    // The control: once the predecessor is merely final, at the entry's own frame.
                    Some(_) if self.m == Mutation::M2AfterFinalOnly => Some(own),
                    // After the predecessor has applied: at its frame or later.
                    Some((Some(pa), ..)) => Some(own.max(pa)),
                    _ => None,
                };
            }
        }
        out
    }

    /// The packages in effect at cut `h` at this member, or `None` if it can't compute that yet: a merge final here
    /// that applies at or below `h` whose package it hasn't fetched, or one that may apply at or below `h` whose
    /// frame isn't known yet.
    fn content_at(&self, s: &State, me: usize, h: u8) -> Option<Vec<(Id, Package)>> {
        let mut out = Vec::new();
        for (at, own, id) in self.schedule(s, me) {
            match at {
                None if own <= h => return None,
                None => {}
                Some(a) if a <= h => {
                    let p = s.members[me].fetched.get(&id)?;
                    out.push((id, *p));
                }
                Some(_) => {}
            }
        }
        Some(out)
    }

    /// What the member has applied: every scheduled merge whose package it holds, in schedule order, up to the first
    /// it can't apply yet.
    fn settle(&self, s: &mut State, me: usize) {
        let mut applied = Vec::new();
        // In application order, a predecessor before its successor at one frame. In log order, an `M2` final before
        // `M1` applied alone once its package was fetched (the second model run's counterexample): merge v1 and
        // pe-app must order by application frame too.
        let mut sched: Vec<(usize, Scheduled)> = self.schedule(s, me).into_iter().enumerate().collect();
        sched.sort_by_key(|(k, (at, _, id))| {
            let second = self.package_by_id(*id).and_then(|p| self.pred_id(&p)).is_some();
            (at.unwrap_or(u8::MAX), second, *k)
        });
        for (_, (at, _, id)) in sched {
            let (Some(a), Some(p)) = (at, s.members[me].fetched.get(&id)) else { break };
            applied.push((a, id, *p));
        }
        // Q1(b): each parent original once, whichever packages bind it, at the first package's frame.
        let mut originals = Vec::new();
        let mut seen = 0u8;
        for (a, _, p) in &applied {
            for bit in 0..8u8 {
                let b = 1u8 << bit;
                if p.originals & b != 0 && (self.m == Mutation::NoOriginalDedupe || seen & b == 0) {
                    originals.push((*a, bit));
                    seen |= b;
                }
            }
        }
        s.members[me].applied = applied;
        s.members[me].originals = originals;
    }
}

impl Model for Members {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<State> {
        let m = Member {
            known: 0,
            fetched: BTreeMap::new(),
            applied: Vec::new(),
            originals: Vec::new(),
            votes: BTreeMap::new(),
        };
        vec![State { log: Vec::new(), voided: BTreeSet::new(), silent: false, members: vec![m; VOTERS.len()] }]
    }

    fn actions(&self, s: &State, acts: &mut Vec<Action>) {
        if (s.log.len() as u8) < MAX_H {
            let included: BTreeSet<MergeEntry> = s.log.iter().flatten().copied().collect();
            let next = s.log.len() as u8 + 1;
            let pending: Vec<MergeEntry> = self
                .scenario
                .entries
                .iter()
                .copied()
                .filter(|e| !included.contains(e) && !s.voided.contains(e) && e.stamp <= next)
                .collect();
            // Any subset of the pending entries that keeps each chain a prefix, the empty cut included.
            for bits in 0u32..(1 << pending.len()) {
                let cut: Vec<MergeEntry> =
                    pending.iter().enumerate().filter(|(i, _)| bits & (1 << i) != 0).map(|(_, e)| *e).collect();
                if self.cut_allowed(s, &cut) {
                    acts.push(Action::Cut(cut));
                }
            }
            for e in &pending {
                if self.scenario.voidable.contains(&e.n) && self.chain_ready(s, e, &[]) {
                    acts.push(Action::Void(*e));
                }
            }
        }
        if self.scenario.may_silence && !s.silent {
            acts.push(Action::Silence);
        }
        for me in 0..s.members.len() {
            let mem = &s.members[me];
            if usize::from(mem.known) < s.log.len() {
                acts.push(Action::Learn(me));
            }
            // Final entries' packages, and pre-fetches of those not yet included or void.
            let included: BTreeSet<MergeEntry> = s.log.iter().flatten().copied().collect();
            let mut wanted: BTreeSet<Id> =
                self.final_entries(s, me).iter().filter_map(|(_, e)| self.id_of(e)).collect();
            wanted.extend(
                self.scenario
                    .entries
                    .iter()
                    .filter(|e| !included.contains(e) && !s.voided.contains(e))
                    .filter_map(|e| self.id_of(e)),
            );
            for id in wanted {
                if mem.fetched.contains_key(&id) {
                    continue;
                }
                for src in self.sources(s, me, id) {
                    acts.push(Action::Fetch(me, id, src));
                }
            }
            for h in 1..=mem.known {
                if !mem.votes.contains_key(&h) {
                    acts.push(Action::Vote(me, h));
                }
            }
        }
    }

    fn next_state(&self, s0: &State, a: Action) -> Option<State> {
        let mut s = s0.clone();
        match a {
            Action::Cut(cut) => s.log.push(cut),
            Action::Void(e) => {
                s.voided.insert(e);
            }
            Action::Learn(me) => {
                s.members[me].known += 1;
                self.settle(&mut s, me);
            }
            Action::Silence => s.silent = true,
            Action::Fetch(me, id, src) => {
                let served = match src {
                    Src::B => self.package_by_id(id)?,
                    Src::BAlt => self.scenario.alt?,
                    Src::Peer(q) => *s.members[q].fetched.get(&id)?,
                };
                // The id binds the content: a reply whose content doesn't hash to the id is refused.
                if self.m != Mutation::NoFetchHashCheck && served.id(self.m) != id {
                    return None;
                }
                s.members[me].fetched.insert(id, served);
                self.settle(&mut s, me);
            }
            Action::Vote(me, h) => {
                let content = match self.content_at(&s, me, h) {
                    Some(c) => c,
                    // The control: vote with what it has applied so far.
                    None if self.m == Mutation::VoteBeforeMerge => {
                        s.members[me].applied.iter().filter(|(at, ..)| *at <= h).map(|(_, id, p)| (*id, *p)).collect()
                    }
                    None => return None,
                };
                s.members[me].votes.insert(h, content);
            }
        }
        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always("no false root split: honest votes at a cut agree", |_: &Members, s: &State| {
                (1..=MAX_H).all(|h| {
                    let votes: Vec<_> = s.members.iter().filter_map(|m| m.votes.get(&h)).collect();
                    votes.windows(2).all(|w| w[0] == w[1])
                })
            }),
            Property::always(
                "same package everywhere: one content per id and per segment",
                |_: &Members, s: &State| {
                    let mut by_id: BTreeMap<Id, Package> = BTreeMap::new();
                    let mut by_seg: BTreeMap<u8, Package> = BTreeMap::new();
                    s.members.iter().flat_map(|m| m.applied.iter()).all(|(_, id, p)| {
                        by_id.entry(*id).or_insert(*p) == p && by_seg.entry(p.segment).or_insert(*p) == p
                    })
                },
            ),
            Property::always("once: a package applies at most once per member", |_: &Members, s: &State| {
                s.members.iter().all(|m| {
                    let mut seen = BTreeSet::new();
                    m.applied.iter().all(|(_, id, _)| seen.insert(*id))
                })
            }),
            Property::always(
                "causal: a second package applies at or after its predecessor",
                |c: &Members, s: &State| {
                    s.members.iter().all(|m| {
                        m.applied.iter().all(|(at, _, p)| match c.pred_id(p) {
                            None => true,
                            Some(pred) => m.applied.iter().any(|(pa, id, _)| *id == pred && pa <= at),
                        })
                    })
                },
            ),
            Property::always("stamped: no intent takes effect at or before its own stamp", |_: &Members, s: &State| {
                s.members.iter().all(|m| m.applied.iter().all(|(at, _, p)| *at > p.top))
            }),
            Property::always("once: a parent original applies at most once per member", |_: &Members, s: &State| {
                s.members.iter().all(|m| {
                    let mut seen = BTreeSet::new();
                    m.originals.iter().all(|(_, o)| seen.insert(*o))
                })
            }),
            Property::always(
                "never stuck: a member holding every package final here can vote at every cut it knows",
                |c: &Members, s: &State| {
                    (0..s.members.len()).all(|me| {
                        let sched = c.schedule(s, me);
                        let all_fetched = sched.iter().all(|(_, _, id)| s.members[me].fetched.contains_key(id));
                        !all_fetched || (1..=s.members[me].known).all(|h| c.content_at(s, me, h).is_some())
                    })
                },
            ),
            Property::always(
                "available: every package final at a member is held by it or served by a live member",
                |c: &Members, s: &State| {
                    (0..s.members.len()).all(|me| {
                        c.final_entries(s, me)
                            .iter()
                            .filter_map(|(_, e)| c.id_of(e))
                            .all(|id| s.members[me].fetched.contains_key(&id) || !c.sources(s, me, id).is_empty())
                    })
                },
            ),
            Property::always(
                "only B's chain waits for M: another member's ready entry can always join the next cut",
                |c: &Members, s: &State| {
                    let included: BTreeSet<MergeEntry> = s.log.iter().flatten().copied().collect();
                    let next = s.log.len() as u8 + 1;
                    (s.log.len() as u8) >= MAX_H
                        || c.scenario.entries.iter().all(|e| {
                            e.package != PLAIN
                                || included.contains(e)
                                || e.stamp > next
                                || !c.chain_ready(s, e, &[])
                                || c.cut_allowed(s, &[*e])
                        })
                },
            ),
            Property::sometimes("a merge applies at every member", |_: &Members, s: &State| {
                s.members.iter().all(|m| !m.applied.is_empty())
            }),
            Property::sometimes("every member votes past an application frame", |_: &Members, s: &State| {
                s.members.iter().all(|m| m.votes.iter().any(|(_, c)| !c.is_empty()))
            }),
        ]
    }
}

/// The scenarios, each a small case of r2's rules.
pub fn scenarios() -> Vec<Scenario> {
    let p1 = Package { segment: 1, content: 1, pred: None, top: 0, originals: 0 };
    let p1b = Package { content: 2, ..p1 };
    // The second package names the first (index 0) as its predecessor.
    let p2 = Package { segment: 2, pred: Some(0), ..p1 };
    let e = |n, package| MergeEntry { n, package, stamp: 1, chain: 0 };
    vec![
        Scenario {
            name: "retry: one package, two merge entries",
            packages: vec![p1],
            entries: vec![e(1, 0), e(2, 0)],
            alt: None,
            voidable: vec![],
            may_silence: false,
        },
        Scenario {
            name: "two successive packages",
            packages: vec![p1, p2],
            entries: vec![e(1, 0), e(2, 1)],
            alt: None,
            voidable: vec![],
            may_silence: false,
        },
        Scenario {
            name: "a faulty B serves two versions of its package (one of its entries changed)",
            packages: vec![p1],
            entries: vec![e(1, 0)],
            alt: Some(p1b),
            voidable: vec![],
            may_silence: false,
        },
        Scenario {
            name: "two packages for one segment",
            packages: vec![p1, p1b],
            entries: vec![e(1, 0), e(2, 1)],
            alt: None,
            voidable: vec![],
            may_silence: false,
        },
        Scenario {
            // A faulty B submits a merge stamped below its package's last intent, and an honest one above it.
            name: "a merge stamped below its intents",
            packages: vec![Package { top: 2, ..p1 }],
            entries: vec![
                MergeEntry { n: 1, package: 0, stamp: 1, chain: 0 },
                MergeEntry { n: 2, package: 0, stamp: 3, chain: 0 },
            ],
            alt: None,
            voidable: vec![],
            may_silence: false,
        },
        Scenario {
            name: "two packages bind one parent original",
            packages: vec![Package { originals: 1, ..p1 }, Package { originals: 1, ..p2 }],
            entries: vec![e(1, 0), e(2, 1)],
            alt: None,
            voidable: vec![],
            may_silence: false,
        },
        Scenario {
            // B restarted and rechained: `M2` is on a new chain, so it can be final before `M1`.
            name: "two successive packages, M2 after a rechain",
            packages: vec![p1, p2],
            entries: vec![e(1, 0), MergeEntry { chain: 1, ..e(2, 1) }],
            alt: None,
            voidable: vec![],
            may_silence: false,
        },
        Scenario {
            // `M1` is stamped below its intents, so refused for good; `M2` names it; an independent `M3` keeps the
            // line busy.
            name: "M1 refused for good, M2 names it",
            packages: vec![
                Package { top: 2, ..p1 },
                Package { pred: Some(0), segment: 2, ..p1 },
                Package { segment: 3, ..p1 },
            ],
            entries: vec![
                MergeEntry { n: 1, package: 0, stamp: 1, chain: 0 },
                MergeEntry { n: 2, package: 1, stamp: 1, chain: 0 },
                MergeEntry { n: 3, package: 2, stamp: 1, chain: 0 },
            ],
            alt: None,
            voidable: vec![],
            may_silence: false,
        },
        Scenario {
            // The agreement voids `M1` (late, or after B's leave); `M2` names it.
            name: "M1 void, M2 names it",
            packages: vec![p1, p2, Package { segment: 3, ..p1 }],
            entries: vec![e(1, 0), e(2, 1), e(3, 2)],
            alt: None,
            voidable: vec![1],
            may_silence: false,
        },
        Scenario {
            // B goes silent at any point, its package final or not; a voter's own entry (chain 2) runs beside it.
            name: "B goes silent",
            packages: vec![p1],
            entries: vec![e(1, 0), MergeEntry { n: 1, package: PLAIN, stamp: 1, chain: 2 }],
            alt: None,
            voidable: vec![],
            may_silence: true,
        },
    ]
}

/// Which property each control must break, and in which scenario.
pub fn expected(m: Mutation) -> Option<(&'static str, &'static str)> {
    let same = "same package everywhere: one content per id and per segment";
    match m {
        Mutation::None => None,
        Mutation::UnboundPackageId => {
            Some(("a faulty B serves two versions of its package (one of its entries changed)", same))
        }
        Mutation::NoFetchHashCheck => Some((
            "a faulty B serves two versions of its package (one of its entries changed)",
            "no false root split: honest votes at a cut agree",
        )),
        Mutation::AcceptSecondPackage => Some(("two packages for one segment", same)),
        Mutation::NoRetryDedupe => {
            Some(("retry: one package, two merge entries", "once: a package applies at most once per member"))
        }
        Mutation::M2AfterFinalOnly => Some((
            "two successive packages, M2 after a rechain",
            "causal: a second package applies at or after its predecessor",
        )),
        Mutation::VoteBeforeMerge => {
            Some(("two successive packages", "no false root split: honest votes at a cut agree"))
        }
        Mutation::HoldAllChains => Some((
            "B goes silent",
            "only B's chain waits for M: another member's ready entry can always join the next cut",
        )),
        Mutation::IncludeWithoutQuorum | Mutation::FetchFromBOnly => Some((
            "B goes silent",
            "available: every package final at a member is held by it or served by a live member",
        )),
        Mutation::M2WaitsForPredecessor => Some((
            "M1 refused for good, M2 names it",
            "never stuck: a member holding every package final here can vote at every cut it knows",
        )),
        Mutation::MBelowIntents => {
            Some(("a merge stamped below its intents", "stamped: no intent takes effect at or before its own stamp"))
        }
        Mutation::NoOriginalDedupe => {
            Some(("two packages bind one parent original", "once: a parent original applies at most once per member"))
        }
    }
}
