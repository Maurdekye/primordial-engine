//! The member's agreement node: seam S3 ([`Agreement`]) and the replica's [`EntrySource`], over the consensus core
//! ([`crate::core`]), the cut rules ([`crate::cuts`]) and the chains ([`pe_log::ChainLog`]).
//!
//! - **Entries:** a member's own go to every member at once; an entry first received from its author is relayed once
//!   to the rest (the fast path). Entries a cut names that this member lacks are fetched by name from every member,
//!   and asked again after a while.
//! - **Voting:** the core says which proposal its rules allow; the node checks the cut (entries held and linked, the
//!   frame above the parent's and at most its present plus the tolerance) and offers it through
//!   [`Agreement::wants_roots`], unless a hold covers its frame. The driver answers with [`Agreement::vote`].
//! - **Finality:** the core reports final blocks in chain order. The node reports each through
//!   [`EntrySource::final_cut`] only once it holds every entry the cut newly includes and the chains before them, and
//!   then gives each its verdict (pe-log's `judge`): late or back-stamped entries are void ([`AgreeEvent::Voided`],
//!   naming the deciding cut); the rest are final, in agreed order.
//! - **The log the replica reads:** per frame, the final entries and then the predicted rest: linked entries beyond
//!   the final cut stamped at or above its frame. Entries stamped below the final frame that no final cut includes are
//!   pending and absent. Same-frame order: the seeded rank (D37), then the author, chain and seq.
//! - **Retention:** final entries are kept from the later of the replica's `Cv` and the time cap (`keep_s` behind the
//!   final frame), and below the replica's floor also within a byte limit.
//!
//! - **Self-test reports** (S3 v1.3): only signed verdicts count, relayed once; see [`crate::core`].
//!
//! Not yet: the idle pause, join and leave (docs/spec/agreement.md 8). Back-stamp verdicts read only the new segment
//! (the node keeps each chain's highest final stamp), so pe-log may later prune final chains.

use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use pe_log::{ChainKey, ChainLog, Inserted, Rejected, Verdict};
use pe_seams::agree::{AgreeEvent, AgreeStats, Agreement, Cut, CutRef, FinalCut, Outgoing, SubmitError};
use pe_seams::{
    EntryRef, EntrySource, Fault, FaultKind, Frame, FrameClock, Hash, Hasher, HostTime, Lane, LineId, MemberKey,
    PeerId, SelfTest, SignedEntry, Signer, Verifier, VoteResult,
};

use crate::codec::{self, MAX_FETCH, Wire};
use crate::core::{self, Block, Config as CoreConfig, Core, CoreEvent, CoreFault, Mutation, Qc, Timing, To};

/// The NoSignatureCheck mutation's verifier.
struct AcceptAll;

impl Verifier for AcceptAll {
    fn verify(&self, _: &MemberKey, _: &[u8], _: &pe_seams::Signature) -> bool {
        true
    }
}
use crate::cuts::{self, Check};

#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub line: LineId,
    /// Sorted, distinct.
    pub members: Vec<MemberKey>,
    /// The line's first cut.
    pub genesis: Cut,
    pub timing: Timing,
    /// How far beyond its present frame a voter accepts a cut's frame, in seconds (a world setting).
    pub frame_tolerance_s: f64,
    /// How far behind its present frame a leader names its cut's frame, in seconds, until it has measured enough
    /// entries: an honest entry still in flight when the cut passes its stamp would be late (coordinator:
    /// 0.25 s).
    pub cut_lag_s: f64,
    /// The adaptive lag (world settings): a high percentile of how late other members' entries arrive here (arrival
    /// against stamp), plus a margin, clamped between a floor and a cap. Members need not agree on it: the cut carries
    /// its frame.
    pub cut_lag_floor_s: f64,
    pub cut_lag_cap_s: f64,
    pub cut_lag_margin_s: f64,
    /// Ask again for missing entries after this long, ns.
    pub fetch_retry_ns: i64,
    /// The time cap on final entries kept behind the final frame: `T_diverge` plus `T_keep`, seconds.
    pub keep_s: f64,
    /// The byte limit on final entries kept below the replica's floor.
    pub keep_bytes: usize,
    /// Relay each entry first received from its author once (the fast path).
    pub relay: bool,
    /// The per-author rate budget (world settings): unsolicited entries per second and the burst, and the most
    /// entries of one author held beyond the final cut. Entries this member asked for (a cut names them) always pass,
    /// or it could never finalize that cut; they still count.
    pub rate_per_s: u32,
    pub rate_burst: u32,
    pub pending_cap: usize,
    /// A planted defect for the harness's mutation tests (feature `mutants` only).
    #[cfg(feature = "mutants")]
    pub mutation: Option<core::Mutation>,
}

impl NodeConfig {
    pub fn new(line: LineId, members: Vec<MemberKey>, genesis: Cut) -> NodeConfig {
        NodeConfig {
            line,
            members,
            genesis,
            timing: Timing::default(),
            frame_tolerance_s: 0.1,
            cut_lag_s: 0.25,
            cut_lag_floor_s: 0.05,
            cut_lag_cap_s: 1.0,
            cut_lag_margin_s: 0.05,
            fetch_retry_ns: 500_000_000,
            keep_s: 60.0 + 10.0,
            keep_bytes: 64 << 20,
            relay: true,
            rate_per_s: 150,
            rate_burst: 60,
            pending_cap: 600,
            #[cfg(feature = "mutants")]
            mutation: None,
        }
    }
}

pub struct Node {
    cfg: NodeConfig,
    me: MemberKey,
    signer: Box<dyn Signer>,
    verifier: Box<dyn Verifier>,
    core: Core,
    log: ChainLog,
    by_ref: HashMap<EntryRef, ChainKey>,
    /// This member's chain: its key and head.
    my_head: Option<(ChainKey, u64, EntryRef)>,
    out: Vec<Outgoing>,
    events: Vec<AgreeEvent>,

    /// Final by the core, waiting for their entries.
    final_queue: Vec<(Block, Qc)>,
    final_cut: Option<FinalCut>,
    /// The last cut reported final (the genesis cut before any).
    final_heads: Cut,
    final_by_frame: BTreeMap<Frame, Vec<SignedEntry>>,
    final_bytes: usize,
    predicted: BTreeMap<Frame, Vec<SignedEntry>>,
    pred_head: HashMap<ChainKey, (u64, EntryRef)>,
    view_by_frame: BTreeMap<Frame, Vec<SignedEntry>>,
    changed_from: Option<Frame>,
    kept_from: Frame,
    keep_cv: Frame,
    keep_floor: Frame,

    wanted: BTreeMap<EntryRef, i64>,
    /// Entries a verified vote's heads name, not held: (author, since, last asked). Not solicited.
    hinted: BTreeMap<EntryRef, (MemberKey, i64, i64)>,
    /// Per chain, the highest stamp through its final head (void entries included), for back-stamp verdicts.
    final_high: HashMap<ChainKey, Frame>,
    arrived: HashMap<EntryRef, HostTime>,
    holds: BTreeMap<Frame, u32>,
    /// Per author: the rate budget's tokens (thousandths of an entry) and when they were last topped up, and how many
    /// of its entries are held beyond the final cut.
    budget: HashMap<MemberKey, (u64, HostTime)>,
    pending_by_author: HashMap<MemberKey, usize>,
    /// How late other members' entries arrived, in frames: the last LATENESS_SAMPLES.
    lateness: std::collections::VecDeque<u64>,
    now: HostTime,
    present: Option<Frame>,
    rate_hz: u32,
}

const LATENESS_SAMPLES: usize = 256;
/// At most this many entries fetched on a vote's hint at once.
const MAX_HINTS: usize = 256;
/// A hinted fetch is dropped after this long if the entry hasn't come (no cut named it).
const HINT_TTL_NS: i64 = 5_000_000_000;
/// Samples needed before the lag adapts.
const LATENESS_MIN: usize = 32;

fn tie_rank(line: LineId, frame: Frame, author: &MemberKey) -> u64 {
    let mut h = Hasher::new("pe.log.tie");
    h.write_hash(&line.0).write_u64(frame.0).write(&author.0);
    let b = h.finish().0;
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn rejected_kind(r: &Rejected) -> FaultKind {
    match r {
        Rejected::Shape(_) => FaultKind::Malformed,
        Rejected::OtherLine => FaultKind::WrongLine,
        Rejected::BadSignature => FaultKind::BadSignature,
    }
}

fn fault_kind(f: &CoreFault) -> FaultKind {
    match f {
        CoreFault::NotMember => FaultKind::NotMember,
        CoreFault::BadSignature => FaultKind::BadSignature,
        CoreFault::Equivocation => FaultKind::Equivocation,
        CoreFault::BadCertificate | CoreFault::BadBlock | CoreFault::BadReport | CoreFault::BadVote => {
            FaultKind::Protocol
        }
    }
}

impl Node {
    pub fn new(cfg: NodeConfig, signer: Box<dyn Signer>, verifier: Box<dyn Verifier>, now: HostTime) -> Node {
        let me = signer.key();
        let core = Core::new(
            CoreConfig {
                line: cfg.line,
                members: cfg.members.clone(),
                genesis: cfg.genesis.clone(),
                timing: cfg.timing,
            },
            me,
            now,
        );
        #[cfg(feature = "mutants")]
        let core = {
            let mut core = core;
            core.set_mutation(cfg.mutation);
            core
        };
        let start = cfg.genesis.at.frame;
        Node {
            me,
            signer,
            verifier,
            core,
            log: ChainLog::new(cfg.line),
            by_ref: HashMap::new(),
            my_head: None,
            out: Vec::new(),
            events: Vec::new(),
            final_queue: Vec::new(),
            final_cut: None,
            final_heads: cfg.genesis.clone(),
            final_by_frame: BTreeMap::new(),
            final_bytes: 0,
            predicted: BTreeMap::new(),
            pred_head: HashMap::new(),
            view_by_frame: BTreeMap::new(),
            changed_from: None,
            kept_from: start,
            keep_cv: start,
            keep_floor: start,
            wanted: BTreeMap::new(),
            hinted: BTreeMap::new(),
            final_high: HashMap::new(),
            arrived: HashMap::new(),
            holds: BTreeMap::new(),
            budget: HashMap::new(),
            pending_by_author: HashMap::new(),
            lateness: std::collections::VecDeque::new(),
            now,
            present: None,
            rate_hz: 60,
            cfg,
        }
    }

    pub fn core(&self) -> &Core {
        &self.core
    }

    fn final_frame(&self) -> Frame {
        self.final_heads.at.frame
    }

    fn frames(&self, s: f64) -> u64 {
        (s * self.rate_hz as f64).ceil().max(0.0) as u64
    }

    /// How far behind its present this member, as leader, names a cut's frame.
    pub fn cut_lag_frames(&self) -> u64 {
        if self.lateness.len() < LATENESS_MIN {
            return self.frames(self.cfg.cut_lag_s);
        }
        let mut v: Vec<u64> = self.lateness.iter().copied().collect();
        v.sort_unstable();
        // The 95th percentile.
        let p = v[(v.len() * 95 / 100).min(v.len() - 1)];
        let lag = p + self.frames(self.cfg.cut_lag_margin_s);
        lag.clamp(self.frames(self.cfg.cut_lag_floor_s), self.frames(self.cfg.cut_lag_cap_s))
    }

    fn tolerance_frames(&self) -> u64 {
        if self.core.mutated(Mutation::HorizonRuleOff) {
            return u64::MAX / 2;
        }
        (self.cfg.frame_tolerance_s * self.rate_hz as f64).ceil().max(0.0) as u64
    }

    fn send_all(&mut self, lane: Lane, w: &Wire, except: &[MemberKey]) {
        let bytes = Bytes::from(codec::encode(w));
        for m in &self.cfg.members {
            if *m != self.me && !except.contains(m) {
                self.out.push(Outgoing { to: *m, lane, bytes: bytes.clone() });
            }
        }
    }

    fn fault(&mut self, from: PeerId, author: Option<MemberKey>, kind: FaultKind, detail: String) -> Fault {
        let f = Fault { from, author, kind, detail };
        self.events.push(AgreeEvent::Fault(f.clone()));
        f
    }

    // ---- entries ----

    /// Hold an entry. Returns whether it is new.
    fn hold(&mut self, e: SignedEntry, from: PeerId, at: HostTime) -> Result<bool, Fault> {
        let id = e.id();
        let author = e.entry.author;
        if !self.cfg.members.contains(&author) {
            self.events.push(AgreeEvent::Refused { entry: id, author, kind: FaultKind::NotMember });
            return Err(self.fault(from, Some(author), FaultKind::NotMember, "entry by a non-member".into()));
        }
        let key = ChainKey::of(&e);
        let prev = e.entry.prev;
        let solicited = self.wanted.contains_key(&id);
        let hinted = self.hinted.contains_key(&id);
        let unchecked = AcceptAll;
        let skip_sig = self.core.mutated(Mutation::NoSignatureCheck);
        let checked = {
            let verifier: &dyn Verifier = if skip_sig { &unchecked } else { self.verifier.as_ref() };
            self.log.check(&e, verifier)
        };
        // Spec 2's order: shape, line, member, signature, then the budget (@design-reviewer's Medium 1). An entry that
        // fails a check is the sending peer's fault: the author it claims never signed it.
        if let Err(r) = checked {
            let kind = rejected_kind(&r);
            self.events.push(AgreeEvent::Refused { entry: id, author, kind });
            return Err(self.fault(from, None, kind, format!("{r:?} (claims author {author:?})")));
        }
        if author != self.me && !self.by_ref.contains_key(&id) && !self.within_budget(author, solicited, at) {
            self.events.push(AgreeEvent::Refused { entry: id, author, kind: FaultKind::RateExceeded });
            return Err(self.fault(from, Some(author), FaultKind::RateExceeded, "over the author's budget".into()));
        }
        let inserted = {
            let verifier: &dyn Verifier = if skip_sig { &unchecked } else { self.verifier.as_ref() };
            self.log.insert(e, verifier)
        };
        match inserted {
            Ok(Inserted::Duplicate) => Ok(false),
            Ok(Inserted::New) => {
                // Lateness from unsolicited entries only: a fetched one arrives late by construction.
                if let Some(p) = self.present.filter(|_| author != self.me && !solicited && !hinted) {
                    {
                        let stamp = self.log.get(&key, &id).map_or(p, |e| e.entry.stamp);
                        if self.lateness.len() == LATENESS_SAMPLES {
                            self.lateness.pop_front();
                        }
                        self.lateness.push_back(p.0.saturating_sub(stamp.0));
                    }
                }
                self.by_ref.insert(id, key);
                self.arrived.entry(id).or_insert(at);
                self.wanted.remove(&id);
                self.hinted.remove(&id);
                *self.pending_by_author.entry(author).or_default() += 1;
                // A fetched entry past a hole: fetch the one before it too, so the chain can link.
                if let Some(p) = prev.filter(|p| !self.by_ref.contains_key(p)) {
                    if solicited {
                        self.want([p]);
                    } else if hinted {
                        self.hint(author, p);
                    }
                }
                if self.log.next_of(&key, prev).len() > 1 {
                    let seq = self.log.get(&key, &id).map(|e| e.entry.seq).unwrap_or(0);
                    self.events.push(AgreeEvent::Equivocation { author, chain: key.chain, seq });
                }
                self.repredict(key);
                Ok(true)
            }
            Err(r) => {
                let kind = rejected_kind(&r);
                self.events.push(AgreeEvent::Refused { entry: id, author, kind });
                Err(self.fault(from, None, kind, format!("{r:?}")))
            }
        }
    }

    /// Whether the author's budget admits one more entry; takes it if so.
    fn within_budget(&mut self, author: MemberKey, solicited: bool, at: HostTime) -> bool {
        if self.core.mutated(Mutation::RateLimitRemoved) {
            return true;
        }
        let cap = self.cfg.rate_burst as u64 * 1000;
        let rate = self.cfg.rate_per_s as u128;
        let (tokens, last) = self.budget.get(&author).copied().unwrap_or((cap, at));
        let dt = at.0.saturating_sub(last.0).max(0) as u128;
        let tokens = (tokens as u128 + rate * dt / 1_000_000).min(cap as u128) as u64;
        let pending = self.pending_by_author.get(&author).copied().unwrap_or(0);
        if !solicited && (tokens < 1000 || pending >= self.cfg.pending_cap) {
            self.budget.insert(author, (tokens, at.max(last)));
            return false;
        }
        self.budget.insert(author, (tokens.saturating_sub(1000), at.max(last)));
        true
    }

    fn want(&mut self, ids: impl IntoIterator<Item = EntryRef>) {
        for id in ids {
            self.wanted.entry(id).or_insert(i64::MIN);
        }
    }

    /// Fetch an entry a vote's heads name, as a hint: at most one per author's free pending slot, at most
    /// [`MAX_HINTS`] in all, and dropped after [`HINT_TTL_NS`] if it hasn't come.
    fn hint(&mut self, author: MemberKey, id: EntryRef) {
        if self.wanted.contains_key(&id) || self.hinted.contains_key(&id) || self.hinted.len() >= MAX_HINTS {
            return;
        }
        let pending = self.pending_by_author.get(&author).copied().unwrap_or(0);
        let hinted = self.hinted.values().filter(|(a, _, _)| *a == author).count();
        if pending + hinted >= self.cfg.pending_cap {
            return;
        }
        self.hinted.insert(id, (author, self.now.0, i64::MIN));
    }

    fn ask(&mut self) {
        let now = self.now.0;
        self.hinted.retain(|_, (_, since, _)| now.saturating_sub(*since) < HINT_TTL_NS);
        let retry = self.cfg.fetch_retry_ns;
        let due_at = |t: i64| t == i64::MIN || now.saturating_sub(t) >= retry;
        let mut due: Vec<EntryRef> =
            self.wanted.iter().filter(|(_, t)| due_at(**t)).map(|(id, _)| *id).take(MAX_FETCH).collect();
        let room = MAX_FETCH - due.len();
        due.extend(self.hinted.iter().filter(|(_, (_, _, t))| due_at(*t)).map(|(id, _)| *id).take(room));
        if due.is_empty() {
            return;
        }
        for id in &due {
            if let Some(t) = self.wanted.get_mut(id) {
                *t = now;
            } else if let Some(h) = self.hinted.get_mut(id) {
                h.2 = now;
            }
        }
        self.send_all(Lane::Protocol, &Wire::FetchEntries(due), &[]);
    }

    // ---- the log the replica reads ----

    fn order(&self, v: &mut [SignedEntry]) {
        let line = self.cfg.line;
        v.sort_by_cached_key(|e| {
            (tie_rank(line, e.entry.stamp, &e.entry.author), e.entry.author, e.entry.chain, e.entry.seq)
        });
    }

    fn rebuild(&mut self, f: Frame) {
        if let Some(mut fv) = self.final_by_frame.remove(&f) {
            self.order(&mut fv);
            self.final_by_frame.insert(f, fv);
        }
        let mut v: Vec<SignedEntry> = self.final_by_frame.get(&f).cloned().unwrap_or_default();
        let fin = v.len();
        if let Some(p) = self.predicted.get(&f) {
            v.extend(p.iter().cloned());
        }
        self.order(&mut v[..fin]);
        self.order(&mut v[fin..]);
        let old = self.view_by_frame.get(&f);
        if old.map(|o| o.as_slice()) != Some(v.as_slice()) && !(old.is_none() && v.is_empty()) {
            self.changed_from = Some(self.changed_from.map_or(f, |c| c.min(f)));
            if v.is_empty() {
                self.view_by_frame.remove(&f);
            } else {
                self.view_by_frame.insert(f, v);
            }
        }
    }

    /// Extend the prediction along chain `key` from its predicted (or final) head.
    fn repredict(&mut self, key: ChainKey) {
        let fin = self
            .final_heads
            .heads
            .binary_search_by(|h| h.chain.cmp(&key.chain))
            .ok()
            .map(|i| self.final_heads.heads[i]);
        let fin = fin.filter(|h| h.author == key.author).map(|h| (h.seq, h.entry));
        let mut head = match (self.pred_head.get(&key).copied(), fin) {
            (Some(p), Some(f)) if p.0 < f.0 => Some(f),
            (Some(p), _) => Some(p),
            (None, f) => f,
        };
        let ff = self.final_frame();
        let mut touched = Vec::new();
        loop {
            let next = self.log.next_of(&key, head.map(|h| h.1));
            if next.len() != 1 {
                break;
            }
            let e = next[0].clone();
            head = Some((e.entry.seq, e.id()));
            if e.entry.stamp >= ff {
                touched.push(e.entry.stamp);
                self.predicted.entry(e.entry.stamp).or_default().push(e);
            }
        }
        if let Some(h) = head {
            self.pred_head.insert(key, h);
        }
        for f in touched {
            self.rebuild(f);
        }
    }

    // ---- finality ----

    /// Report the core's final blocks whose entries are all held, in order.
    fn finalize(&mut self) {
        while let Some((block, qc)) = self.final_queue.first().cloned() {
            let parent = self.final_heads.clone();
            let mut verdicts: Vec<(ChainKey, EntryRef, Verdict)> = Vec::new();
            let mut highs: Vec<(ChainKey, Frame)> = Vec::new();
            let mut missing = Vec::new();
            for h in &block.cut.heads {
                let key = ChainKey { author: h.author, chain: h.chain };
                let from = parent.heads.binary_search_by(|p| p.chain.cmp(&h.chain)).ok().map(|i| parent.heads[i].entry);
                if from == Some(h.entry) {
                    continue;
                }
                match self.log.judge(&key, from, h.entry, parent.at.frame, self.final_high.get(&key).copied()) {
                    Ok((v, high)) => {
                        verdicts.extend(v.into_iter().map(|(id, v)| (key, id, v)));
                        if let Some(f) = high {
                            highs.push((key, f));
                        }
                    }
                    Err(pe_log::SegmentError::Missing(x)) => missing.push(x),
                    Err(_) => {
                        // A certified cut this member can't walk: more than t faulty voters. Never expected.
                        self.events.push(AgreeEvent::Fault(Fault {
                            from: block.proposer,
                            author: Some(h.author),
                            kind: FaultKind::Protocol,
                            detail: "a final cut's chain does not link".into(),
                        }));
                        return;
                    }
                }
            }
            if !missing.is_empty() {
                self.want(missing);
                return;
            }
            self.final_queue.remove(0);
            self.final_high.extend(highs);
            let at = block.cut.at;
            let old_ff = parent.at.frame;
            let mut touched: Vec<Frame> = Vec::new();
            for (key, id, v) in verdicts {
                let Some(e) = self.log.get(&key, &id).cloned() else { continue };
                if let Some(c) = self.pending_by_author.get_mut(&key.author) {
                    *c = c.saturating_sub(1);
                }
                match v {
                    Verdict::Effective => {
                        touched.push(e.entry.stamp);
                        self.final_bytes += e.entry.payload.len() + 200;
                        self.final_by_frame.entry(e.entry.stamp).or_default().push(e);
                    }
                    Verdict::Void(pe_log::VoidReason::Late) => {
                        self.events.push(AgreeEvent::Voided {
                            entry: id,
                            author: key.author,
                            reason: pe_seams::agree::VoidReason::Late,
                            cut: at,
                        });
                    }
                    Verdict::Void(pe_log::VoidReason::BackStamped) => {
                        self.events.push(AgreeEvent::Voided {
                            entry: id,
                            author: key.author,
                            reason: pe_seams::agree::VoidReason::BackStamped,
                            cut: at,
                        });
                    }
                    // A fork of an included entry: the equivocation was reported when it arrived; it never applies.
                    Verdict::Void(pe_log::VoidReason::Fork) => {}
                }
            }
            self.final_heads = block.cut.clone();
            self.final_cut = Some(FinalCut { cut: block.cut.clone(), root: qc.root, voters: qc.votes.clone() });
            // Predictions below the new final frame end: final now, or pending (absent).
            let gone: Vec<Frame> = self.predicted.range(old_ff..at.frame).map(|(f, _)| *f).collect();
            for f in gone {
                self.predicted.remove(&f);
                touched.push(f);
            }
            // Predicted entries the cut includes at or above... none: a cut covers stamps below its frame.
            touched.sort();
            touched.dedup();
            for f in touched {
                self.rebuild(f);
            }
            let keys: Vec<ChainKey> =
                block.cut.heads.iter().map(|h| ChainKey { author: h.author, chain: h.chain }).collect();
            for k in keys {
                self.repredict(k);
            }
            self.trim();
        }
    }

    fn trim(&mut self) {
        let window = (self.cfg.keep_s * self.rate_hz as f64).ceil() as u64;
        let cap = Frame(self.final_frame().0.saturating_sub(window));
        let mut to = self.keep_cv.max(cap).min(self.final_frame());
        loop {
            let first = self.final_by_frame.keys().next().copied();
            match first {
                Some(f) if f < to => {}
                Some(f) if self.final_bytes > self.cfg.keep_bytes && f < self.keep_floor => to = Frame(f.0 + 1),
                _ => break,
            }
            let f = first.expect("matched above");
            if let Some(v) = self.final_by_frame.remove(&f) {
                self.final_bytes = self.final_bytes.saturating_sub(v.iter().map(|e| e.entry.payload.len() + 200).sum());
            }
            self.view_by_frame.remove(&f);
        }
        if to > self.kept_from {
            self.kept_from = to;
            let stale: Vec<Frame> = self.view_by_frame.range(..to).map(|(f, _)| *f).collect();
            for f in stale {
                self.view_by_frame.remove(&f);
            }
        }
    }

    // ---- the core's output ----

    fn pump(&mut self) {
        for (to, msg) in self.core.take_out() {
            let w = Wire::Core(msg);
            match to {
                To::All => self.send_all(Lane::Protocol, &w, &[]),
                To::One(k) => {
                    self.out.push(Outgoing { to: k, lane: Lane::Protocol, bytes: Bytes::from(codec::encode(&w)) })
                }
            }
        }
        for e in self.core.take_events() {
            match e {
                CoreEvent::Proposed { block, proposer } => {
                    if let Some(cut) = self.core.cut_ref(&block) {
                        self.events.push(AgreeEvent::Proposed { cut, proposer });
                    }
                }
                CoreEvent::Final { block, qc } => {
                    self.events.push(AgreeEvent::CutFinal {
                        cut: block.cut.at,
                        root: qc.root,
                        voters: qc.votes.iter().map(|v| v.0).collect(),
                    });
                    self.final_queue.push((block, qc));
                }
                CoreEvent::RootSplit { block, roots } => {
                    if let Some(cut) = self.core.cut_ref(&block) {
                        self.events.push(AgreeEvent::RootSplit { cut, roots });
                    }
                }
                CoreEvent::Conflict { block } => {
                    self.events.push(AgreeEvent::Fault(Fault {
                        from: self.me,
                        author: None,
                        kind: FaultKind::Protocol,
                        detail: format!("conflicting final block {:?}: more than t faulty", block),
                    }));
                }
                CoreEvent::Equivocation { member, view } => {
                    self.events.push(AgreeEvent::Fault(Fault {
                        from: member,
                        author: Some(member),
                        kind: FaultKind::Equivocation,
                        detail: format!("two messages in view {view}"),
                    }));
                }
                CoreEvent::VerdictEquivocation { member, cut } => {
                    self.events.push(AgreeEvent::Fault(Fault {
                        from: member,
                        author: Some(member),
                        kind: FaultKind::Equivocation,
                        detail: format!("two self-test verdicts for the split at height {}", cut.height),
                    }));
                }
            }
        }
        self.finalize();
        self.ask();
    }

    fn cast(&mut self, cut: &CutRef, root: Hash, self_test: SelfTest) -> VoteResult {
        // The current view's proposal for this cut, if this member may still vote for it; else a revote of the cut its
        // latest vote went to (the new root counts from the next view; the verdict went out as a report).
        let id = match self.candidate() {
            Some((p, Check::Ok)) if p.block.cut.at == *cut && !self.held(cut.frame) => p.block.id(),
            _ if self.core.last_vote_on(cut) => return VoteResult::AlreadyVotedThisView,
            _ => return VoteResult::NotVotable,
        };
        let heads = self.heads_beyond(cut);
        if self.core.vote(id, root, self_test, heads, self.now, self.signer.as_ref()).is_some() {
            self.events.push(AgreeEvent::Voted { cut: *cut, root, self_test });
            VoteResult::Voted
        } else {
            VoteResult::NotVotable
        }
    }

    /// The proposal the core allows a vote for, with the node's check of its cut.
    fn candidate(&self) -> Option<(&core::Proposal, Check)> {
        let p = self.core.votable()?;
        let parent = self.core.block(&p.block.parent)?;
        let present = self.present?;
        if self.core.mutated(Mutation::AcceptProposalUnchecked) {
            return Some((p, Check::Ok));
        }
        let c = cuts::check(&self.log, &parent.cut, &p.block.cut, present, self.tolerance_frames());
        if self.core.mutated(Mutation::VoteWithoutChainValidation) && matches!(c, Check::Missing(_)) {
            return Some((p, Check::Ok));
        }
        Some((p, c))
    }

    /// This member's linked chain heads beyond the cut `at` names (for its vote).
    fn heads_beyond(&self, at: &CutRef) -> Vec<pe_seams::ChainHead> {
        let cut = self.core.votable().map(|p| &p.block.cut).filter(|c| c.at == *at);
        let mut out: Vec<pe_seams::ChainHead> = self
            .pred_head
            .iter()
            .filter(|(k, (seq, _))| {
                let in_cut = cut.and_then(|c| c.heads.iter().find(|h| h.chain == k.chain && h.author == k.author));
                in_cut.is_none_or(|h| h.seq < *seq)
            })
            .map(|(k, (seq, id))| pe_seams::ChainHead { chain: k.chain, author: k.author, seq: *seq, entry: *id })
            .collect();
        out.sort();
        out.truncate(crate::codec::MAX_HEADS);
        out
    }

    fn held(&self, frame: Frame) -> bool {
        self.holds.keys().next().is_some_and(|h| frame >= *h)
    }
}

impl EntrySource for Node {
    fn final_cut(&self) -> Option<&FinalCut> {
        self.final_cut.as_ref()
    }

    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        if frame < self.kept_from {
            return None;
        }
        Some(self.view_by_frame.get(&frame).map(|v| v.as_slice()).unwrap_or(&[]))
    }

    fn kept_from(&self) -> Frame {
        self.kept_from
    }

    fn keep_final_from(&mut self, cv: Frame, floor: Frame) {
        self.keep_cv = self.keep_cv.max(cv);
        self.keep_floor = self.keep_floor.max(floor);
        self.trim();
    }

    fn take_changed_from(&mut self) -> Option<Frame> {
        self.changed_from.take()
    }

    fn arrived_at(&self, entry: &EntryRef) -> Option<HostTime> {
        self.arrived.get(entry).copied()
    }
}

impl Agreement for Node {
    fn line(&self) -> LineId {
        self.cfg.line
    }

    fn submit(&mut self, entry: pe_seams::Entry) -> Result<SignedEntry, SubmitError> {
        if entry.author != self.me || entry.line != self.cfg.line {
            return Err(SubmitError::NotNext);
        }
        entry.check_shape().map_err(SubmitError::Shape)?;
        let key = ChainKey { author: entry.author, chain: entry.chain };
        match self.my_head {
            None if entry.seq == 1 => {}
            Some((k, seq, id)) if k == key && entry.seq == seq + 1 && entry.prev == Some(id) => {}
            _ => return Err(SubmitError::NotNext),
        }
        if entry.stamp < self.final_frame() {
            return Err(SubmitError::TooLate);
        }
        let e = entry.sign(self.signer.as_ref());
        let id = e.id();
        let now = self.now;
        if self.hold(e.clone(), self.me, now).is_err() {
            return Err(SubmitError::NotNext);
        }
        self.my_head = Some((key, e.entry.seq, id));
        self.send_all(Lane::Action, &Wire::Entries(vec![e.clone()]), &[]);
        Ok(e)
    }

    fn receive(&mut self, from: PeerId, lane: Lane, bytes: &[u8], at: HostTime) -> Result<(), Fault> {
        let _ = lane;
        if !self.cfg.members.contains(&from) {
            return Err(self.fault(from, None, FaultKind::NotMember, "not a member".into()));
        }
        let w = match codec::decode(bytes) {
            Ok(w) => w,
            Err(_) => return Err(self.fault(from, None, FaultKind::Malformed, "undecodable message".into())),
        };
        let r = match w {
            Wire::Core(m) => {
                // Heads beyond the cut in a vote, once the core has verified the vote: fetch what this member lacks, so
                // as leader it can include them. A hint only (@design-reviewer's Medium 2): such entries are not
                // solicited, so they pass the author's budget like any other, and the hint expires.
                let heads = if let core::Msg::Vote(v) = &m { v.heads.clone() } else { Vec::new() };
                match self.core.receive(from, m, at, self.verifier.as_ref()) {
                    Ok(()) => {
                        for h in heads {
                            if self.cfg.members.contains(&h.author) && !self.by_ref.contains_key(&h.entry) {
                                self.hint(h.author, h.entry);
                            }
                        }
                        Ok(())
                    }
                    Err(f) => Err(self.fault(from, None, fault_kind(&f), format!("{f:?}"))),
                }
            }
            Wire::FetchEntries(ids) => {
                let es: Vec<SignedEntry> = ids
                    .iter()
                    .filter_map(|id| self.by_ref.get(id).and_then(|k| self.log.get(k, id)).cloned())
                    .collect();
                if !es.is_empty() {
                    let b = Bytes::from(codec::encode(&Wire::Entries(es)));
                    self.out.push(Outgoing { to: from, lane: Lane::Action, bytes: b });
                }
                Ok(())
            }
            Wire::Entries(es) => {
                let mut first_err = None;
                for e in es {
                    let author = e.entry.author;
                    match self.hold(e.clone(), from, at) {
                        Ok(true) if self.cfg.relay && from == author => {
                            self.send_all(Lane::Action, &Wire::Entries(vec![e]), &[from]);
                        }
                        Ok(_) => {}
                        Err(f) => {
                            first_err.get_or_insert(f);
                        }
                    }
                }
                first_err.map_or(Ok(()), Err)
            }
        };
        self.pump();
        r
    }

    fn tick(&mut self, now: HostTime, clock: &dyn FrameClock) -> Vec<Outgoing> {
        self.now = now;
        self.rate_hz = clock.rate_hz();
        self.present = clock.frame_at(now);
        self.core.tick(now, self.signer.as_ref());
        let lag = self.cut_lag_frames();
        if let (Some(lead), Some(present)) = (self.core.lead(), self.present) {
            let frame = Frame(present.0.saturating_sub(lag));
            if now >= lead.not_before && frame > lead.parent.frame() {
                let cut = cuts::build(&self.log, &lead.parent.cut, frame);
                self.core.propose(cut, now, self.signer.as_ref(), self.verifier.as_ref());
            }
        }
        let missing = match self.candidate() {
            Some((_, Check::Missing(ids))) => ids,
            _ => Vec::new(),
        };
        self.want(missing);
        self.pump();
        std::mem::take(&mut self.out)
    }

    fn wants_roots(&self) -> Vec<Cut> {
        match self.candidate() {
            Some((p, Check::Ok)) if !self.held(p.block.frame()) => vec![p.block.cut.clone()],
            _ => Vec::new(),
        }
    }

    fn vote(&mut self, cut: &CutRef, root: Hash, self_test: SelfTest) -> VoteResult {
        // A verdict naming a split goes out signed, whatever becomes of the vote (S3 v1.3): only signed verdicts
        // count, so every member shows the same ones.
        if self_test.split().is_some() {
            self.core.report(self_test, self.signer.as_ref());
        }
        let result = self.cast(cut, root, self_test);
        self.pump();
        result
    }

    fn hold_votes_from(&mut self, frame: Frame) {
        *self.holds.entry(frame).or_default() += 1;
    }

    fn release_votes_from(&mut self, frame: Frame) {
        if let Some(n) = self.holds.get_mut(&frame) {
            *n -= 1;
            if *n == 0 {
                self.holds.remove(&frame);
            }
        }
    }

    fn final_entries(&self, from: &CutRef, to: &CutRef) -> Option<Vec<SignedEntry>> {
        // The whole range must be kept and final: never a partial log.
        if from.frame < self.kept_from || to.frame < from.frame || to.frame > self.final_heads.at.frame {
            return None;
        }
        Some(self.final_by_frame.range(from.frame..to.frame).flat_map(|(_, v)| v.iter().cloned()).collect())
    }

    fn members(&self) -> Vec<MemberKey> {
        self.cfg.members.clone()
    }

    fn stats(&self) -> AgreeStats {
        AgreeStats {
            members: self.cfg.members.len(),
            quorum: self.core.quorum(),
            final_height: self.final_heads.at.height,
            final_frame: self.final_cut.as_ref().map(|c| c.cut.at.frame),
            unconfirmed_since: None,
            pending_entries: self.predicted.values().map(|v| v.len()).sum(),
        }
    }

    fn events(&mut self) -> Vec<AgreeEvent> {
        std::mem::take(&mut self.events)
    }
}
