//! `SeamNode`: one node over the real seams, S3's [`Agreement`] plus S2's [`Replica`] (DESIGN.md 12, M0 step 2).
//!
//! The drive loop here is a copy of pe-replica's `Driver::drive`, kept only until that lands on main (ws/replica,
//! @vdw-physics). Each tick: the agreement ticks, the replica advances to the present (`p + 1`), every cut in
//! `wants_roots` gets the replica's root and a vote, and each new `final_cut()` goes to `finalize` (retried while
//! `NotReady`).
//!
//! The runner and the checkers still read the harness's local trace ([`Ev`]); this node maps the seam's events onto
//! it until the trace is retyped to `TraceEvent`:
//! - `CutFinal` becomes `Final`, with the cut's hash over its final entries, so P1 compares cut contents;
//! - `Voided` keeps its deciding cut (P4); `Paused` stays `Paused` (P8);
//! - the recorder adds `ReplicaRoot` at each voted cut, `Floor` at each finalize and `RolledBack` from `Advance`.
//!
//! Node ids are the members' places in key order, from 1, as [`KvSeamReplica`] numbers them, so P2's offline replay
//! over [`KvReplica`](crate::kv::KvReplica) sees the same authors. An entry stamped `s` is the replay's frame `s + 1`:
//! the replay counts frames stepped, and the cut at frame `f` covers the entries stamped below `f`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    AgreeEvent, Agreement, ChainId, Class, Cut, CutRef, FaultKind, Finalized, Frame, FrameClock, FrameView, Hasher,
    HostTime, Lane, MemberKey, Replica, SelfTest, SignedEntry, SimRate,
};

use crate::clock::SessionClock;
use crate::kv::{KvEntry, Op, Template};
use pe_replica::Phase;

use crate::kvrep::{KvSeamReplica, payload};
use crate::lockstep::LockstepAgreement;
use crate::net::NodeId;
use crate::sim::{Fault, NodeUnderTest, Out};
use crate::trace::{EntryRef as TraceEntry, Ev, Frame as TFrame};

/// Retention's rotating probe: the frame after `last` (absolute), at least `kept + 1`, wrapping back to `kept + 1` at
/// `present`; none when no frame lies above `kept` and below `present`.
pub fn next_probe(last: u64, kept: u64, present: u64) -> Option<u64> {
    let next = last.max(kept) + 1;
    let next = if next >= present { kept + 1 } else { next };
    (next < present).then_some(next)
}

/// An agreement's entry framing, read back: the entries a message carries, if any.
pub type EntryDecoder = fn(&[u8]) -> Vec<SignedEntry>;

/// The lockstep mock's entry framing, read back.
fn lockstep_entries(bytes: &[u8]) -> Vec<SignedEntry> {
    LockstepAgreement::decode_entry_message(bytes).into_iter().collect()
}

/// pe-agree's entry framing, read back: an `Entries` message's entries (an author's own, a relay or a fetch's answer).
/// The recorder counts them held once `receive` accepts the message; pe-agree may still drop one (its per-author
/// budget), so P15 can miss a vote on such an entry, never report a false one.
pub fn agree_entries(bytes: &[u8]) -> Vec<SignedEntry> {
    match pe_agree::codec::decode(bytes) {
        Ok(pe_agree::codec::Wire::Entries(v)) => v,
        _ => Vec::new(),
    }
}

/// A pe-agree configuration for a harness session over `keys`: line origin, genesis at frame 0, a cut every
/// `cadence_ns`, view timeouts from 300 ms to 5 s (as pe-agree's own node tests), everything else pe-agree's default.
pub fn agree_config(keys: &TestKeys, cadence_ns: i64) -> pe_agree::NodeConfig {
    let line = pe_seams::LineId::origin();
    let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
    let mut cfg = pe_agree::NodeConfig::new(line, keys.sorted_keys(), genesis);
    cfg.timing = pe_agree::core::Timing { cadence_ns, min_timeout_ns: 300_000_000, max_timeout_ns: 5_000_000_000 };
    cfg
}

/// Where a node records its replica's frame views: each new publication, with the host time it was read at.
pub type ViewSink = Arc<Mutex<Vec<(i64, Arc<FrameView>)>>>;

/// What a node saw become final, for an offline replay through S2 ([`crate::s2replay`]): a cut at each height, and
/// the final entries in agreed order, voided ones absent. A height's cut is its final one once the node has seen it
/// final (in `finals`); until then, the first cut it was asked to vote on there, which a different final cut may
/// replace (a voted cut that timed out or split, under pe-agree; @design-reviewer's Low on b9257cbd).
#[derive(Clone, Debug, Default)]
pub struct FinalLog {
    pub cuts: BTreeMap<u64, Cut>,
    /// The heights whose cut in `cuts` the node saw final.
    pub finals: BTreeSet<u64>,
    pub entries: Vec<SignedEntry>,
}

impl FinalLog {
    /// Records `cut`: a final one replaces whatever is at its height; a voted one fills only an empty height.
    pub fn record(&mut self, cut: &Cut, is_final: bool) {
        let h = cut.at.height;
        if is_final {
            self.cuts.insert(h, cut.clone());
            self.finals.insert(h);
        } else {
            self.cuts.entry(h).or_insert_with(|| cut.clone());
        }
    }
}

/// Where a node records its [`FinalLog`] (`record_final`).
pub type FinalSink = Arc<Mutex<FinalLog>>;

/// What a [`SeamNode`] needs from its replica beyond S2: the order its frames apply in, and what an entry adds to
/// P2's reference log.
pub trait NodeReplica: Replica {
    /// The phase function the replica applies each frame's entries in (coordinator).
    fn phase(&self) -> fn(&SignedEntry) -> Phase;
    /// The key-value ops an entry adds to the reference log P2 replays over [`KvReplica`](crate::kv::KvReplica).
    /// A replica with no key-value oracle (pe-world's) adds none, and its runs leave P2 unchecked.
    fn log_ops(&self, entry: &SignedEntry) -> Vec<Op>;
}

impl NodeReplica for KvSeamReplica {
    fn phase(&self) -> fn(&SignedEntry) -> Phase {
        KvSeamReplica::phase(self)
    }
    fn log_ops(&self, entry: &SignedEntry) -> Vec<Op> {
        crate::mock::decode(&entry.entry.payload).map(|(_, _, ops)| ops).unwrap_or_default()
    }
}

impl NodeReplica for pe_replica::ReplicaImpl<pe_replica::PeWorld> {
    fn phase(&self) -> fn(&SignedEntry) -> Phase {
        <pe_replica::PeWorld as pe_replica::WorldModel>::phase
    }
    fn log_ops(&self, _entry: &SignedEntry) -> Vec<Op> {
        Vec::new()
    }
}

/// An agreement's named safety counter, as SeamNode reads it (`check_counter`).
type Counter<A> = (&'static str, fn(&A) -> u64);

pub struct SeamNode<A: Agreement, R: NodeReplica = KvSeamReplica> {
    pub agree: A,
    pub replica: R,
    clock: SessionClock,
    builder: EntryBuilder,
    /// Member keys in node-id order: node `i` is `keys[i - 1]`.
    keys: Vec<MemberKey>,
    last_final: CutRef,
    finalized_height: u64,
    log: BTreeMap<TFrame, Vec<KvEntry>>,
    events: Vec<Ev>,
    self_test: SelfTest,
    /// The agreement's entry framing (its test-util codec), so the recorder knows which entries this member holds:
    /// its own and every valid one delivered to it. `None` turns the hold checks of P15 off.
    decode: Option<EntryDecoder>,
    held: BTreeSet<(ChainId, u64)>,
    held_ids: BTreeSet<pe_seams::EntryRef>,
    voided: BTreeSet<pe_seams::EntryRef>,
    /// Frames below the final frame whose `entries_at` P15 has checked.
    checked_to: u64,
    /// The highest `kept_from` seen: it never goes back.
    kept_seen: u64,
    /// Retention's rotating probe: the last frame it checked, absolute, so a moving `kept_from` doesn't make it skip
    /// frames (design-reviewer).
    probe: u64,
    /// Cut frames already recorded as `Damaged`.
    damaged: BTreeSet<u64>,
    seen_height: u64,
    /// The class each scenario input is signed with (`Class::Edit` unless `with_order` says otherwise).
    class_of: fn(&Op) -> Class,
    /// Raw entries this node submits on its own, by host time (`submit_at`): inputs that are not key-value ops.
    script: Vec<(i64, Class, Vec<u8>)>,
    /// Where each new publication of the replica's view goes, if anywhere (`record_views`).
    views: Option<ViewSink>,
    last_view: Option<Arc<FrameView>>,
    /// Cuts reported final whose entries this member doesn't hold yet (`final_entries` answers `None`), in order,
    /// with their roots. pe-agree reports a cut final before its entries arrive; the lockstep mock never does.
    pending_final: std::collections::VecDeque<(CutRef, pe_seams::Hash)>,
    /// Whether `void_reason` may answer `None` for a voided entry: pe-agree, until it implements S2 v1.5's method.
    void_reason_missing_ok: bool,
    /// S3's `entries_for_cut` answers for each cut handed over in `wants_roots`, frame by frame from the last absorbed
    /// final cut up to the cut's frame, as entry ids in order: P15 holds them to the final log if the cut goes final.
    answers: BTreeMap<CutRef, Vec<(u64, Vec<pe_seams::EntryRef>)>>,
    /// The final log's entry ids by frame, from the oldest frame an outstanding answer covers.
    final_ids: BTreeMap<u64, Vec<pe_seams::EntryRef>>,
    /// An agreement's own safety counter, read after every tick: a P15 breach whenever it rises. pe-agree's is
    /// `Core::shared_cuts`, cuts it won't answer for because two held blocks carry them under different parents
    /// (world-chunks, agree/void 4ca4f9b1). `None` for agreements without one.
    counter: Option<Counter<A>>,
    counter_seen: u64,
    /// Where the final cuts and entries go, if anywhere (`record_final`).
    finals: Option<FinalSink>,
}

/// The node id of member `key`: its place in key order, from 1.
pub fn node_of(keys: &[MemberKey], key: &MemberKey) -> NodeId {
    keys.binary_search(key).map_or(0, |i| i as NodeId + 1)
}

/// A short stand-in for an entry's chain position in the local trace: the id's first eight bytes.
pub fn trace_seq(e: &pe_seams::EntryRef) -> u64 {
    u64::from_le_bytes(e.0.0[..8].try_into().unwrap_or_default())
}

fn fault_code(k: FaultKind) -> u16 {
    match k {
        FaultKind::Malformed => 1,
        FaultKind::TooLarge => 2,
        FaultKind::BadSignature => 3,
        FaultKind::WrongLine => 4,
        FaultKind::NotMember => 5,
        FaultKind::Equivocation => 6,
        FaultKind::RateExceeded => 7,
        FaultKind::Protocol => 8,
    }
}

impl SeamNode<LockstepAgreement> {
    /// Node `id` (from 1, in key order) of a lockstep session over `keys`, cutting every `cadence_ns`.
    pub fn lockstep(
        keys: &TestKeys,
        id: NodeId,
        template: &Template,
        objects: &[u128],
        rate: SimRate,
        cadence_ns: i64,
    ) -> Self {
        let replica = KvSeamReplica::new(keys.sorted_keys(), template, objects);
        SeamNode::lockstep_over(keys, id, replica, rate, cadence_ns)
    }
}

impl<R: NodeReplica> SeamNode<LockstepAgreement, R> {
    /// Node `id` (from 1, in key order) of a lockstep session over `keys`, cutting every `cadence_ns`, over `replica`.
    pub fn lockstep_over(keys: &TestKeys, id: NodeId, replica: R, rate: SimRate, cadence_ns: i64) -> Self {
        let i = key_index(keys, id);
        let agree = LockstepAgreement::new(keys, i, cadence_ns);
        let genesis = agree.genesis();
        SeamNode::over(agree, keys, i, genesis, replica, rate, lockstep_entries)
    }
}

impl<R: NodeReplica> SeamNode<pe_agree::Node, R> {
    /// Node `id` (from 1, in key order) of a pe-agree session over `keys`, configured by `cfg` (its members must be
    /// `keys`'s, sorted: [`agree_config`] gives them), over `replica`.
    pub fn agree_over(keys: &TestKeys, id: NodeId, cfg: pe_agree::NodeConfig, replica: R, rate: SimRate) -> Self {
        let i = key_index(keys, id);
        let genesis = cfg.genesis.at;
        let agree = pe_agree::Node::new(cfg, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
        let mut node = SeamNode::over(agree, keys, i, genesis, replica, rate, agree_entries);
        // TODO(harness-eng): drop once pe-agree implements `void_reason` (world-chunks).
        node.void_reason_missing_ok = true;
        node.counter = Some(("shared_cuts", |a| a.core().shared_cuts()));
        node
    }
}

/// The index in `keys` of node `id`, the `id`-th member in key order (from 1).
fn key_index(keys: &TestKeys, id: NodeId) -> usize {
    let sorted = keys.sorted_keys();
    let key = sorted[(id as usize).saturating_sub(1)];
    (0..keys.len()).find(|&j| keys.key(j) == key).unwrap_or(0)
}

impl<A: Agreement, R: NodeReplica> SeamNode<A, R> {
    /// A node over `agree`, whose member is `keys`'s `i`-th key, from the line's genesis cut `genesis`.
    fn over(
        agree: A,
        keys: &TestKeys,
        i: usize,
        genesis: CutRef,
        replica: R,
        rate: SimRate,
        decode: EntryDecoder,
    ) -> Self {
        SeamNode {
            agree,
            replica,
            clock: SessionClock { rate, start: HostTime(0) },
            builder: EntryBuilder::new(keys, i),
            keys: keys.sorted_keys(),
            last_final: genesis,
            finalized_height: 0,
            log: BTreeMap::new(),
            events: Vec::new(),
            self_test: SelfTest::NotRun,
            decode: Some(decode),
            held: BTreeSet::new(),
            held_ids: BTreeSet::new(),
            voided: BTreeSet::new(),
            checked_to: 0,
            kept_seen: 0,
            probe: 0,
            damaged: BTreeSet::new(),
            seen_height: 0,
            class_of: |_| Class::Edit,
            script: Vec::new(),
            views: None,
            last_view: None,
            finals: None,
            pending_final: std::collections::VecDeque::new(),
            void_reason_missing_ok: false,
            answers: BTreeMap::new(),
            final_ids: BTreeMap::new(),
            counter: None,
            counter_seen: 0,
        }
    }
}

impl<A: Agreement, R: NodeReplica> SeamNode<A, R> {
    /// This node submits `payload` as a `class` entry, stamped with its frame at host time `at`, on its first tick at
    /// or after `at`: how a scenario gives a node inputs that are not key-value ops (pe-world's edits).
    #[must_use]
    pub fn submit_at(mut self, at: i64, class: Class, payload: Vec<u8>) -> Self {
        let i = self.script.partition_point(|s| s.0 <= at);
        self.script.insert(i, (at, class, payload));
        self
    }

    /// This node records what became final in `sink`: each cut it votes on or finalizes, and each final cut's entries.
    #[must_use]
    pub fn record_final(mut self, sink: FinalSink) -> Self {
        self.finals = Some(sink);
        self
    }

    /// This node records each new publication of its replica's view in `sink`, after the tick that made it.
    #[must_use]
    pub fn record_views(mut self, sink: ViewSink) -> Self {
        self.views = Some(sink);
        self
    }
}

impl<A: Agreement> SeamNode<A, KvSeamReplica> {
    /// A two-phase world: each scenario input is signed with `class_of`'s class, and the replica applies a frame's
    /// entries in `phase` order (coordinator). Both read only the entry, as the ruling requires.
    #[must_use]
    pub fn with_order(mut self, class_of: fn(&Op) -> Class, phase: fn(&SignedEntry) -> Phase) -> Self {
        self.class_of = class_of;
        self.replica = self.replica.with_phase(phase);
        self
    }
}

impl<A: Agreement, R: NodeReplica> SeamNode<A, R> {
    fn frame_at(&self, now: i64) -> u64 {
        self.clock.frame_at(HostTime(now)).map_or(0, |f| f.0)
    }

    /// Signs and submits one entry stamped with this node's frame at `now`; a refused one is dropped.
    fn submit(&mut self, now: i64, class: Class, payload: bytes::Bytes) {
        let stamp = Frame(self.frame_at(now));
        let mut b = self.builder.clone();
        let entry = b.next_unsigned(class, stamp, payload);
        if let Ok(signed) = self.agree.submit(entry) {
            self.held.insert((signed.entry.chain, signed.entry.seq));
            self.held_ids.insert(signed.id());
            self.builder = b;
        }
    }

    /// Records S3's `entries_for_cut` answers for a cut handed over for roots, once per cut.
    fn record_answers(&mut self, cut: &Cut) {
        if self.answers.contains_key(&cut.at) {
            return;
        }
        // From the lowest frame S3 still answers for, not the last final cut (design-reviewer's Low on 4eba171a): a
        // pipelining S3's late entries are stamped below the previous final cut, and an answer that wrongly holds one
        // there must be compared too.
        let rec = (self.agree.kept_from().0..cut.at.frame.0)
            .filter_map(|f| {
                let ids = self.agree.entries_for_cut(cut, Frame(f))?.iter().map(SignedEntry::id).collect();
                Some((f, ids))
            })
            .collect();
        self.answers.insert(cut.at, rec);
    }

    /// P15's `entries_for_cut` line (S3 v1.4): what S3 answered for `cut` while it was out for roots must be exactly
    /// what the final log holds at each of those frames now that it is final, same entries in the same order. That
    /// includes frames below the previous final cut, which another cut finalized while this one was out (a pipelining
    /// S3's late entries live there). Answers for cuts at or below this height are dropped: such a cut is final now,
    /// or never will be.
    fn check_answers(&mut self, cut: CutRef, entries: &[SignedEntry]) {
        for e in entries {
            self.final_ids.entry(e.entry.stamp.0).or_default().push(e.id());
        }
        let rec = self.answers.remove(&cut);
        self.answers.retain(|c, _| c.height > cut.height);
        if let Some(rec) = rec {
            for (f, ids) in &rec {
                let got = self.final_ids.get(f).map_or(&[][..], Vec::as_slice);
                if got != ids.as_slice() {
                    let detail = format!(
                        "cut {}/{} at frame {f}: answered {} entries for roots, the final log holds {}",
                        cut.height,
                        cut.frame.0,
                        ids.len(),
                        got.len()
                    );
                    self.events.push(Ev::Breach { rule: "entries_for_cut", detail });
                }
            }
        }
        // Keep the final log's ids as far back as an outstanding answer reaches, or a future one can (`kept_from` only
        // moves forward).
        let oldest = self
            .answers
            .values()
            .filter_map(|r| r.first().map(|x| x.0))
            .min()
            .unwrap_or(cut.frame.0)
            .min(self.agree.kept_from().0);
        self.final_ids = self.final_ids.split_off(&oldest);
    }

    /// Records a cut, final or voted, for the offline replay.
    fn record_cut(&self, cut: &Cut, is_final: bool) {
        if let Some(Ok(mut log)) = self.finals.as_ref().map(|s| s.lock()) {
            log.record(cut, is_final);
        }
    }

    /// Records the replica's view if it was published again since the last one recorded.
    fn record_view(&mut self, now: i64) {
        let Some(sink) = &self.views else { return };
        let v = self.replica.view();
        if self.last_view.as_ref().is_some_and(|l| Arc::ptr_eq(l, &v)) {
            return;
        }
        if let Ok(mut s) = sink.lock() {
            s.push((now, Arc::clone(&v)));
        }
        self.last_view = Some(v);
    }

    /// The (chain, seq) positions `cut` names that this member does not hold, by the recorder's count.
    fn lacking(&self, cut: &Cut) -> Vec<(ChainId, u64)> {
        if self.decode.is_none() {
            return Vec::new();
        }
        cut.heads
            .iter()
            .flat_map(|h| (1..=h.seq).map(move |s| (h.chain, s)))
            .filter(|k| !self.held.contains(k))
            .collect()
    }

    fn breach_if_lacking(&mut self, rule: &'static str, cut: &Cut) {
        let missing = self.lacking(cut);
        if !missing.is_empty() {
            let detail = format!("cut {}/{} names {} entries not held", cut.at.height, cut.at.frame.0, missing.len());
            self.events.push(Ev::Breach { rule, detail });
        }
    }

    /// P15's check of `entries_at` below the final frame: only entries the final cut covers, none it voided. A frame
    /// no longer kept (S3 v1.2) holds nothing to check.
    fn check_applied(&mut self) {
        let Some(fc) = self.agree.final_cut().cloned() else { return };
        let to = fc.cut.at.frame.0;
        for f in self.checked_to..to {
            for e in self.agree.entries_at(Frame(f)).into_iter().flatten() {
                let bad = if self.voided.contains(&e.id()) {
                    Some("a voided entry")
                } else if !fc.cut.includes(&e.entry.chain, e.entry.seq) {
                    Some("an entry the final cut does not cover")
                } else {
                    None
                };
                if let Some(what) = bad {
                    let detail = format!("frame {f} below final frame {to} applies {what}");
                    self.events.push(Ev::Breach { rule: "entries_at", detail });
                }
            }
        }
        self.checked_to = self.checked_to.max(to);
    }

    /// P15's retention lines (S3 v1.2): `kept_from` never goes back, and it is truthful: `entries_at` answers for
    /// it, for the final frame's predecessor, and for one frame above it and below `present` that rotates each step
    /// (design-reviewer). (That nothing
    /// at or above the floor is trimmed except past the time cap needs the cap, which is S3's own: pe-agree's
    /// conformance target checks it.)
    fn check_retention(&mut self, present: u64) {
        let kept = self.agree.kept_from().0;
        if kept < self.kept_seen {
            let detail = format!("went back from {} to {kept}", self.kept_seen);
            self.events.push(Ev::Breach { rule: "kept_from", detail });
        }
        self.kept_seen = self.kept_seen.max(kept);
        let last = self.agree.final_cut().map(|fc| fc.cut.at.frame.0.saturating_sub(1));
        // Every frame from `kept_from` to the present is promised, final or not: one more of them each step.
        let between = next_probe(self.probe, kept, present);
        if let Some(f) = between {
            self.probe = f;
        }
        for f in std::iter::once(kept).chain(last).chain(between) {
            if f >= kept && f < present && self.agree.entries_at(Frame(f)).is_none() {
                let detail = format!("frame {f} is at or above kept_from {kept} but no longer kept");
                self.events.push(Ev::Breach { rule: "kept_from", detail });
            }
        }
    }

    /// One drive step: advance, vote on what wants a root, finalize the newest final cut.
    fn drive(&mut self, now: i64) {
        let present = Frame(self.frame_at(now) + 1);
        self.check_retention(present.0);
        let adv = self.replica.advance(present, &mut self.agree);
        if let Some(f) = adv.rolled_back_to {
            self.events.push(Ev::RolledBack { frame: f.0 });
        }
        for cut in self.agree.wants_roots() {
            self.breach_if_lacking("wants_roots", &cut);
            self.record_answers(&cut);
            self.record_cut(&cut, false);
            match self.replica.root_at_cut(&cut, &mut self.agree) {
                Ok(root) => {
                    self.events.push(Ev::ReplicaRoot { frame: cut.at.frame.0, root: root.0 });
                    // The lockstep stand-in casts every vote (it has no views).
                    let _ = self.agree.vote(&cut.at, root, self.self_test);
                }
                // S3 v1.2: no root at this cut will come; rung 2 is due (M0 runs none). Recorded once per cut.
                Err(pe_seams::RootError::Damaged) if self.damaged.insert(cut.at.frame.0) => {
                    self.events.push(Ev::Damaged { frame: cut.at.frame.0 });
                }
                Err(_) => {}
            }
        }
        if let Some(fc) = self.agree.final_cut().cloned() {
            if fc.cut.at.height < self.seen_height {
                let detail = format!("height {} after {}", fc.cut.at.height, self.seen_height);
                self.events.push(Ev::Breach { rule: "final_cut", detail });
            }
            if fc.cut.at.height > self.seen_height {
                self.seen_height = fc.cut.at.height;
                self.record_cut(&fc.cut, true);
                self.breach_if_lacking("final_cut", &fc.cut);
            }
        }
        // Take the agreement's events, and absorb final cuts into the final log, before the replica finalizes: its
        // `keep_final_from` lets S3 trim below `Cv`, and `final_entries` from the last absorbed cut must still answer.
        self.pump();
        if let Some(fc) = self.agree.final_cut().cloned()
            && fc.cut.at.height > self.finalized_height
        {
            match self.replica.finalize(&fc, &mut self.agree) {
                Finalized::NotReady => {}
                // The root the replica compared, as its last root at the cut's frame: P1 holds every honest member's
                // root there to the cut's, voter or not (a quorum agreement finalizes members that never voted), and
                // a member that voted the quorum's root but re-simulates to another by finalize is caught here.
                Finalized::Agreed => {
                    self.finalized_height = fc.cut.at.height;
                    self.events.push(Ev::ReplicaRoot { frame: fc.cut.at.frame.0, root: fc.root.0 });
                    self.events.push(Ev::Floor { frame: fc.cut.at.frame.0 });
                }
                Finalized::Diverged { mine } => {
                    self.finalized_height = fc.cut.at.height;
                    self.events.push(Ev::ReplicaRoot { frame: fc.cut.at.frame.0, root: mine.0 });
                    self.events.push(Ev::Floor { frame: fc.cut.at.frame.0 });
                }
                // S3 v1.2: my root at the cut cannot be taken, and waiting will not change that. The floor stays, so
                // there is no floor event, and the cut is not retried; rung 2 is pe-diverge's, which M0 does not run.
                Finalized::Damaged => {
                    self.finalized_height = fc.cut.at.height;
                    if self.damaged.insert(fc.cut.at.frame.0) {
                        self.events.push(Ev::Damaged { frame: fc.cut.at.frame.0 });
                    }
                }
            }
        }
        self.check_counter();
    }

    /// Breaches once each time the agreement's safety counter (`counter`) rises.
    fn check_counter(&mut self) {
        let Some((rule, read)) = self.counter else { return };
        let n = read(&self.agree);
        if n > self.counter_seen {
            let detail = format!("rose from {} to {n}", self.counter_seen);
            self.counter_seen = n;
            self.events.push(Ev::Breach { rule, detail });
        }
    }

    /// Takes the agreement's events into the recorder's buffer (the trace's seam events), then absorbs the pending
    /// final cuts it can.
    fn pump(&mut self) {
        for e in self.agree.events() {
            match e {
                AgreeEvent::CutFinal { cut, root, .. } => {
                    self.pending_final.push_back((cut, root));
                    self.absorb_pending();
                }
                AgreeEvent::Voided { entry, author, reason, cut } => {
                    self.voided.insert(entry);
                    if self.decode.is_some() && !self.held_ids.contains(&entry) {
                        // A verdict on an entry this member does not hold (S3 v1.1, per member).
                        let detail = format!("void of an entry not held, under cut {}", cut.height);
                        self.events.push(Ev::Breach { rule: "verdict", detail });
                    }
                    // S2 v1.5: `void_reason` answers the reason the void was reported with. pe-agree doesn't implement
                    // it yet (`void_reason_missing_ok`, world-chunks): it may answer `None`, never
                    // another reason.
                    let said = self.agree.void_reason(&entry);
                    if said != Some(reason) && !(said.is_none() && self.void_reason_missing_ok) {
                        let detail = format!("void_reason answers {said:?} for an entry voided {reason:?}");
                        self.events.push(Ev::Breach { rule: "void_reason", detail });
                    }
                    self.events.push(Ev::Voided {
                        entry: TraceEntry { author: node_of(&self.keys, &author), seq: trace_seq(&entry) },
                        by: Some((0, cut.height)),
                        reason: reason as u16,
                    });
                }
                AgreeEvent::Refused { entry, author, kind } => self.events.push(Ev::Refused {
                    entry: TraceEntry { author: node_of(&self.keys, &author), seq: trace_seq(&entry) },
                    reason: fault_code(kind),
                }),
                AgreeEvent::Paused { at } => self.events.push(Ev::Paused { frame: at.0 }),
                // Faults reach the trace from `receive`'s result; the rest is not in the local trace yet.
                _ => {}
            }
        }
        self.absorb_pending();
    }

    /// Absorbs the pending final cuts, oldest first, while this member holds their entries; each becomes `Ev::Final`
    /// then. A cut reported final is recorded only once its entries are in the final log, so P2 never replays a log
    /// with a final cut's entries missing.
    fn absorb_pending(&mut self) {
        while let Some(&(cut, root)) = self.pending_final.front() {
            let Some(entries) = self.agree.final_entries(&self.last_final, &cut) else { break };
            self.pending_final.pop_front();
            self.check_answers(cut, &entries);
            let hash = self.absorb_final(cut, entries);
            self.events.push(Ev::Final { line: 0, height: cut.height, frame: cut.frame.0, cut: hash.0, root: root.0 });
        }
    }

    /// The cut's hash over its final entries since the previous final cut, which also extends the final log.
    fn absorb_final(&mut self, cut: CutRef, entries: Vec<SignedEntry>) -> pe_seams::Hash {
        let mut h = Hasher::new("pe.harness.cut");
        h.write_u64(cut.height).write_u64(cut.frame.0);
        // The cut's hash in agreed order; the log P2 replays in apply order, as the replica stepped them.
        // `entries` spans every frame since the last final cut, so this sort spans frames too (a later frame's
        // input moves ahead of an earlier frame's edit). It is right only because each entry then goes into its
        // own frame's row and the sort is stable: every row keeps its frame's phase order. Never flatten this.
        for e in &entries {
            h.write_hash(&e.id().0);
        }
        if let Some(Ok(mut log)) = self.finals.as_ref().map(|s| s.lock()) {
            log.entries.extend(entries.iter().cloned());
        }
        for e in pe_replica::apply_order(&entries, self.replica.phase()) {
            let author = node_of(&self.keys, &e.entry.author);
            let ops = self.replica.log_ops(e);
            let row = self.log.entry(e.entry.stamp.0 + 1).or_default();
            row.extend(ops.into_iter().map(|op| KvEntry { author, op }));
        }
        self.log.entry(cut.frame.0).or_default();
        self.last_final = cut;
        h.finish()
    }
}

impl<A: Agreement, R: NodeReplica> NodeUnderTest for SeamNode<A, R> {
    fn input(&mut self, now: i64, op: Op) {
        let class = (self.class_of)(&op);
        self.submit(now, class, payload(&[op]));
    }

    fn receive(&mut self, now: i64, from: NodeId, bytes: &[u8]) -> Result<(), Fault> {
        let Some(&peer) = (from as usize).checked_sub(1).and_then(|i| self.keys.get(i)) else {
            return Err(Fault { author: from, reason: fault_code(FaultKind::NotMember) });
        };
        let keys = &self.keys;
        let r = self
            .agree
            .receive(peer, Lane::Action, bytes, HostTime(now))
            .map_err(|f| Fault { author: f.author.map_or(from, |a| node_of(keys, &a)), reason: fault_code(f.kind) });
        if r.is_ok()
            && let Some(d) = self.decode
        {
            for e in d(bytes) {
                self.held.insert((e.entry.chain, e.entry.seq));
                self.held_ids.insert(e.id());
            }
        }
        r
    }

    fn tick(&mut self, now: i64) -> Vec<Out> {
        let due = self.script.partition_point(|s| s.0 <= now);
        let due: Vec<_> = self.script.drain(..due).collect();
        for (_, class, bytes) in due {
            self.submit(now, class, bytes.into());
        }
        let outs = self.agree.tick(HostTime(now), &self.clock);
        self.drive(now);
        self.record_view(now);
        outs.into_iter()
            .map(|o| Out { to: node_of(&self.keys, &o.to), lane: o.lane, bytes: o.bytes.to_vec() })
            .collect()
    }

    fn drain(&mut self) -> Vec<Ev> {
        self.pump();
        self.check_applied();
        std::mem::take(&mut self.events)
    }

    fn final_entries(&self) -> Vec<(TFrame, Vec<KvEntry>)> {
        self.log.iter().map(|(&f, e)| (f, e.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::next_probe;

    #[test]
    fn the_probe_visits_every_frame_while_kept_from_moves_under_it() {
        // A steady cap: kept_from and the present each move one frame a step. An offset from kept_from would advance
        // two frames a step and skip every other one (design-reviewer).
        let (mut last, mut seen) = (0, Vec::new());
        for step in 0..10u64 {
            let (kept, present) = (step, step + 40);
            last = next_probe(last, kept, present).unwrap_or(last);
            seen.push(last);
        }
        assert_eq!(seen, (1..=10).collect::<Vec<u64>>());
        // It wraps to kept + 1 at the present, and has nothing to probe when no frame lies between.
        assert_eq!(next_probe(39, 5, 40), Some(6));
        assert_eq!(next_probe(0, 5, 6), None);
    }
}
