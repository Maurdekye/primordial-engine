//! `KvSeamReplica`: the key-value stand-in ([`crate::kv::KvReplica`]) behind S2 v1's [`pe_seams::Replica`], so
//! pe-agree's tests and the probes run without physics (DESIGN.md 3.3; @world-chunks, m194e3bbd6fad).
//!
//! - **Entries:** an entry's payload is a list of [`Op`]s in the mock's codec ([`payload`]). A payload that does not
//!   decode is refused as `Malformed`; nothing panics. Authors are the session's member keys, sorted.
//! - **Frames:** `frame()` is the next frame to step. The state at the start of frame `f` is kept for every `f` from
//!   the floor up, so `advance` re-simulates from the source's earliest changed frame, never below the floor.
//! - **Cuts:** a cut at frame `f` covers the entries stamped below `f`. `root_at_cut` replays from the floor with
//!   exactly the cut's entries and leaves the live state alone. `finalize` is the only call that moves the floor and
//!   `Cv`. `replay_to` is rung 1: from `Cv`, to the cut with its entries, then on to the present.
//! - **Not in this stand-in yet:** the state tree, physics images and imports (rung 2). `tree` and `physics_image`
//!   hold nothing, and `import_at_cut` answers `NotKept`. They come when pe-diverge's tests need them (DESIGN.md 12).
//! - **Faults:** test_util's [`pe_seams::test_util::Corrupt`] changes a field outside any entry.

use std::collections::BTreeMap;
use std::sync::Arc;

use pe_seams::{
    Advance, Cut, CutImport, DomainId, EntryRef, EntrySource, FinalCut, Finalized, Frame, FrameView, GroupSel, Hash,
    HostTime, ImportError, MemberKey, ObjectId, Outcome, PhysicsImage, Pos, Reason, Replica, RigTransform, RootError,
    SignedEntry, Tree, ViewCounters,
};

use crate::kv::{self, KvEntry, KvReplica, Op, Template};

/// An entry payload carrying `ops`, in the mock's codec.
pub fn payload(ops: &[Op]) -> bytes::Bytes {
    bytes::Bytes::from(crate::mock::encode(0, 0, ops))
}

fn decode(bytes: &[u8]) -> Option<Vec<Op>> {
    crate::mock::decode(bytes).map(|(_, _, ops)| ops)
}

fn reason(r: kv::Reason, op: &Op) -> Reason {
    let obj = ObjectId::from_u128;
    match (r, op) {
        (kv::Reason::NoPermission | kv::Reason::NotMember, _) => Reason::NoPermission,
        (kv::Reason::NoSuchObject, Op::Delete { obj: o } | Op::Set { obj: o, .. } | Op::Add { obj: o, .. }) => {
            Reason::Unknown(obj(*o))
        }
        (kv::Reason::AlreadyExists, Op::Create { obj: o }) => Reason::Exists(obj(*o)),
        _ => Reason::Invalid { field: None },
    }
}

pub struct KvSeamReplica {
    domain: DomainId,
    members: Vec<MemberKey>,
    /// The live state; its `frame()` is the next frame to step.
    live: KvReplica,
    /// The state at the start of each frame, from the floor up to the live frame.
    starts: BTreeMap<u64, KvReplica>,
    floor: u64,
    /// The last agreed cut's frame and its state.
    cv: (u64, KvReplica),
    outcomes: BTreeMap<EntryRef, (Frame, Outcome)>,
    /// Re-simulate from here at the next `advance` (set by `finalize`).
    dirty_from: Option<u64>,
    view: Arc<FrameView>,
    /// S2 v1.1's budget, kept and unused (see `set_budget`).
    budget: std::time::Duration,
}

impl KvSeamReplica {
    /// A world at frame 0: `members` (sorted here) under `template`, with `objects`. Member `i` of the sorted keys
    /// is the core's member `i + 1`.
    pub fn new(mut members: Vec<MemberKey>, template: &Template, objects: &[u128]) -> Self {
        members.sort();
        let ids: Vec<u32> = (1..=members.len() as u32).collect();
        let genesis = KvReplica::new(&ids, template, objects);
        Self {
            domain: DomainId(0),
            members,
            starts: [(0, genesis.clone())].into(),
            cv: (0, genesis.clone()),
            live: genesis,
            floor: 0,
            outcomes: BTreeMap::new(),
            dirty_from: None,
            view: Arc::new(Self::empty_view(0)),
            budget: std::time::Duration::MAX,
        }
    }

    fn empty_view(frame: u64) -> FrameView {
        FrameView {
            frame: Frame(frame),
            domain: DomainId(0),
            sim_time: HostTime(0),
            final_frame: None,
            local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
            objects: Vec::new(),
            effects: Vec::new(),
            retracted: Vec::new(),
            counters: ViewCounters::default(),
            changes: Vec::new(),
        }
    }

    /// The core's member id for a key, if it is a member.
    fn member(&self, key: &MemberKey) -> u32 {
        self.members.binary_search(key).map_or(u32::MAX, |i| i as u32 + 1)
    }

    /// The live state, for tests.
    pub fn state(&self) -> &KvReplica {
        &self.live
    }

    /// Steps `state` one frame with `entries`, recording outcomes if asked.
    fn step(
        &self,
        state: &mut KvReplica,
        entries: &[SignedEntry],
        outcomes: Option<&mut BTreeMap<EntryRef, (Frame, Outcome)>>,
    ) {
        let frame = Frame(state.frame());
        let mut batch = Vec::new();
        let mut results: Vec<(EntryRef, Option<Op>)> = Vec::new();
        for e in entries {
            let author = self.member(&e.entry.author);
            match decode(&e.entry.payload) {
                Some(ops) => {
                    for op in ops {
                        results.push((e.id(), Some(op.clone())));
                        batch.push(KvEntry { author, op });
                    }
                }
                None => results.push((e.id(), None)),
            }
        }
        let applied = state.step(&batch);
        if let Some(out) = outcomes {
            let mut it = applied.into_iter();
            for (id, op) in results {
                let o = match op {
                    None => Outcome::Refused { op: None, reason: Reason::Malformed },
                    Some(op) => match it.next() {
                        Some(kv::Outcome::Refused(r)) => Outcome::Refused { op: Some(0), reason: reason(r, &op) },
                        _ => Outcome::Applied {
                            created: Vec::new(),
                            overwrote: Vec::new(),
                            notes: Vec::new(),
                            inverse: None,
                        },
                    },
                };
                out.insert(id, (frame, o));
            }
        }
    }

    /// The state at `to` from `base` (the state at the start of `from`), with exactly the cut's entries.
    ///
    /// It does not check that every head appears in `entries_at`: a cut may cover a void entry (back-stamped, or
    /// stamped below the previous cut's frame), which the source rightly leaves out, so a replica cannot tell
    /// "missing" from "void" there. The d1 rule is the agreement's, through `wants_roots` and `final_cut`.
    ///
    /// A frame no longer kept is never stepped empty (S3 v1.2). Below the floor only a replay from `Cv` meets one:
    /// `NotKept`, and rung 2 is next. At or above it the root cannot be taken: `Damaged`, and rung 2 is due at once.
    fn replay_cut(&self, mut base: KvReplica, cut: &Cut, source: &dyn EntrySource) -> Result<KvReplica, RootError> {
        while base.frame() < cut.at.frame.0 {
            let Some(all) = source.entries_at(Frame(base.frame())) else {
                return Err(if base.frame() < self.floor { RootError::NotKept } else { RootError::Damaged });
            };
            let mine: Vec<SignedEntry> =
                all.iter().filter(|e| cut.includes(&e.entry.chain, e.entry.seq)).cloned().collect();
            self.step(&mut base, &mine, None);
        }
        Ok(base)
    }

    fn resimulate_from(&mut self, from: u64, source: &mut dyn EntrySource) -> u64 {
        let to = self.live.frame();
        let from = from.max(self.floor);
        let Some(start) = self.starts.get(&from).cloned() else { return 0 };
        self.starts.retain(|&f, _| f <= from);
        self.live = start;
        let mut outcomes = std::mem::take(&mut self.outcomes);
        while self.live.frame() < to {
            let f = self.live.frame();
            // S3 v1.2: a frame no longer kept is never stepped empty; the replica stops there.
            let Some(entries) = source.entries_at(Frame(f)).map(<[SignedEntry]>::to_vec) else { break };
            let mut live = self.live.clone();
            self.step(&mut live, &entries, Some(&mut outcomes));
            self.live = live;
            self.starts.insert(self.live.frame(), self.live.clone());
        }
        self.outcomes = outcomes;
        self.live.frame() - from
    }
}

impl Replica for KvSeamReplica {
    fn domain(&self) -> DomainId {
        self.domain
    }

    fn frame(&self) -> Frame {
        Frame(self.live.frame())
    }

    /// The stand-in steps in microseconds, so it keeps the budget but never needs it: every call catches up.
    fn set_budget(&mut self, simulated: std::time::Duration) {
        self.budget = simulated;
    }

    fn advance(&mut self, present: Frame, source: &mut dyn EntrySource) -> Advance {
        let mut adv = Advance::default();
        let changed = source.take_changed_from().map(|f| f.0);
        let from = match (changed, self.dirty_from.take()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(c) = from
            && c < self.live.frame()
        {
            let back = c.max(self.floor);
            adv.rolled_back_to = Some(Frame(back));
            adv.resimulated = self.resimulate_from(back, source);
        }
        let mut outcomes = std::mem::take(&mut self.outcomes);
        let mut stopped = false;
        while self.live.frame() < present.0 {
            let f = self.live.frame();
            // S3 v1.2: never stepped empty. The stand-in steps only from its floor up here, and replays rung 1 within
            // one call, so no trim lands in a running replay: a stop is the at-or-above-the-floor case.
            let Some(entries) = source.entries_at(Frame(f)).map(<[SignedEntry]>::to_vec) else {
                stopped = true;
                break;
            };
            let mut live = self.live.clone();
            self.step(&mut live, &entries, Some(&mut outcomes));
            self.live = live;
            self.starts.insert(self.live.frame(), self.live.clone());
            adv.stepped += 1;
        }
        self.outcomes = outcomes;
        adv.caught_up = !stopped;
        if adv.stepped > 0 || adv.resimulated > 0 {
            self.view = Arc::new(Self::empty_view(self.live.frame()));
        }
        adv
    }

    fn outcome(&self, entry: &EntryRef) -> Option<(Frame, Outcome)> {
        self.outcomes.get(entry).cloned()
    }

    fn root_at_cut(&mut self, cut: &Cut, source: &mut dyn EntrySource) -> Result<Hash, RootError> {
        let f = cut.at.frame.0;
        if f < self.floor {
            return Err(RootError::NotKept);
        }
        if f > self.live.frame() {
            return Err(RootError::NotYetSimulated);
        }
        let base = self.starts.get(&self.floor).cloned().ok_or(RootError::NotKept)?;
        let state = self.replay_cut(base, cut, source)?;
        Ok(Hash(state.root_hash()))
    }

    fn finalize(&mut self, cut: &FinalCut, source: &mut dyn EntrySource) -> Finalized {
        let mine = match self.root_at_cut(&cut.cut, source) {
            Ok(h) => h,
            // S3 v1.2: a frame from my floor up is no longer kept, and waiting will not change that: rung 2 at once.
            Err(RootError::Damaged) => return Finalized::Damaged,
            Err(_) => return Finalized::NotReady,
        };
        let f = cut.cut.at.frame.0;
        let Some(base) = self.starts.get(&self.floor).cloned() else { return Finalized::NotReady };
        let Ok(exact) = self.replay_cut(base, &cut.cut, source) else { return Finalized::NotReady };
        // The cut's exact state becomes the floor; frames above it are re-simulated from it at the next advance.
        self.floor = f;
        self.starts.retain(|&k, _| k >= f);
        self.starts.insert(f, exact.clone());
        if f < self.live.frame() {
            self.dirty_from = Some(self.dirty_from.map_or(f, |d| d.min(f)));
        } else {
            self.live = exact.clone();
        }
        let verdict = if mine == cut.root {
            self.cv = (f, exact);
            Finalized::Agreed
        } else {
            Finalized::Diverged { mine }
        };
        // S3 v1.2: what this replica still needs, `Cv` for rung 1 and the floor for re-simulation.
        source.keep_final_from(Frame(self.cv.0), Frame(self.floor));
        verdict
    }

    fn replay_to(&mut self, cut: &Cut, source: &mut dyn EntrySource) -> Result<Hash, RootError> {
        let (cv_frame, cv) = self.cv.clone();
        // S3 v1.2: below `kept_from` the entries from `Cv` on are gone; refused before anything is restored.
        if cut.at.frame.0 < cv_frame || Frame(cv_frame) < source.kept_from() {
            return Err(RootError::NotKept);
        }
        let at = self.replay_cut(cv, cut, source)?;
        let root = Hash(at.root_hash());
        // On to the present with the source's entries: the new live state, whatever the root.
        let present = self.live.frame();
        self.starts.insert(cut.at.frame.0, at.clone());
        self.live = at;
        let mut outcomes = std::mem::take(&mut self.outcomes);
        while self.live.frame() < present {
            // S3 v1.2: a frame no longer kept, past the cut, stops the replay; the root is taken already.
            let Some(entries) = source.entries_at(Frame(self.live.frame())).map(<[SignedEntry]>::to_vec) else {
                break;
            };
            let mut live = self.live.clone();
            self.step(&mut live, &entries, Some(&mut outcomes));
            self.live = live;
            self.starts.insert(self.live.frame(), self.live.clone());
        }
        self.outcomes = outcomes;
        Ok(root)
    }

    fn tree(&self, _root: &Hash) -> Option<Box<dyn Tree + '_>> {
        None
    }

    fn physics_image(&self, _root: &Hash) -> Option<PhysicsImage> {
        None
    }

    fn import_at_cut(
        &mut self,
        _group: &GroupSel,
        _import: CutImport,
        _source: &mut dyn EntrySource,
    ) -> Result<Hash, ImportError> {
        Err(ImportError::NotKept)
    }

    fn view(&self) -> Arc<FrameView> {
        self.view.clone()
    }
}

impl pe_seams::test_util::Corrupt for KvSeamReplica {
    fn corrupt(&mut self, object: ObjectId, field: u32) -> bool {
        let Ok(field) = u16::try_from(field) else { return false };
        let v = self.live.field(object.to_u128(), field).unwrap_or(0);
        self.live.corrupt(object.to_u128(), field, v.wrapping_add(1));
        true
    }
}
