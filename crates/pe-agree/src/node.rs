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
    ChainId, EntryRef, EntrySource, Fault, FaultKind, Frame, FrameClock, Hash, Hasher, HostTime, Lane, LineId,
    MemberKey, PeerId, SelfTest, SignedEntry, Signer, Verifier, VoteResult,
};

use crate::codec::{self, MAX_FETCH, Wire};
use crate::core::{self, Block, BlockId, Config as CoreConfig, Core, CoreEvent, CoreFault, Mutation, Qc, Timing, To};

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
    /// entries: an honest entry still in flight when the cut passes its stamp would be late (coordinator m0901fd194db2:
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
    /// The per-author rate budget (world settings; see [`NodeConfig::rate_budget`]): it follows the world's frame
    /// rate, so an honest player's per-frame inputs pass at any rate. Entries this member asked for (a cut names them)
    /// always pass, or it could never finalize that cut; they still count.
    ///
    /// Entries an author makes per frame: two hands plus one for poses and other per-frame inputs.
    pub entries_per_frame: u32,
    /// Entries per second on top of the per-frame ones, for edits and governance.
    pub extra_per_s: u32,
    /// Headroom on the per-frame entries, for jitter and bursts.
    pub budget_slack: f64,
    /// The burst, and the most entries of one author held beyond the final cut, as seconds of the rate.
    pub burst_s: f64,
    pub pending_s: f64,
    /// Floors on the rate (per second), the burst and the pending cap, whatever the frame rate.
    pub min_rate_per_s: u32,
    pub min_burst: u32,
    pub min_pending: usize,
    /// The cut interval (world settings), seconds: every cut's frame is a multiple of it in frames at the world's rate
    /// ([`NodeConfig::cut_every`]), so a replica's live root and keyframes, kept on the same grid, serve every vote
    /// without re-stepping (@engine-architect m1a122523d255). Voters refuse a cut off the grid.
    pub cut_interval_s: f64,
    /// The byte limit on one author's entries waiting for its budget (they are delayed, never lost, up to this and to
    /// the pending cap in entries).
    pub defer_bytes: usize,
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
            entries_per_frame: 3,
            extra_per_s: 30,
            budget_slack: 1.25,
            burst_s: 0.5,
            pending_s: 4.0,
            min_rate_per_s: 150,
            min_burst: 60,
            min_pending: 600,
            defer_bytes: 2 << 20,
            cut_interval_s: 0.1,
            #[cfg(feature = "mutants")]
            mutation: None,
        }
    }
}

/// The per-author rate budget at one frame rate (see [`NodeConfig::rate_budget`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateBudget {
    /// Entries per second.
    pub per_s: f64,
    /// Entries an author may send at once.
    pub burst: u32,
    /// The most entries of one author held beyond the final cut.
    pub pending: usize,
}

impl NodeConfig {
    /// A hash of the settings every member must share, or some would refuse an honest member's entries, votes or
    /// cuts: `frame_tolerance_s`, the rate budget and `Timing`. For the M1 interim's genesis record, which a joining
    /// machine checks against its own `NodeConfig` (coordinator md1a4c7ca9c0e). The line, the members and the
    /// genesis cut are left out: the record carries them itself, and the line derives from the record
    /// (@engine-architect m8e5846cff93e). The quorum rule is fixed in code. Local settings (the cut lag, fetch retry,
    /// retention, relay) are left out too: members may differ there.
    pub fn consensus_hash(&self) -> Hash {
        // Every field named, none skipped with `..`: a new field doesn't compile until it is classified as
        // must-match (hashed) or local (@design-reviewer m08ffc71b27c6). Add it to the test's lists too.
        let NodeConfig {
            // Carried by the genesis record itself; the line derives from it.
            line: _,
            members: _,
            genesis: _,
            // Must match.
            timing,
            frame_tolerance_s,
            entries_per_frame,
            extra_per_s,
            budget_slack,
            burst_s,
            pending_s,
            min_rate_per_s,
            min_burst,
            min_pending,
            // Two members with different queue sizes disagree on whether an honest author's flush is a fault.
            defer_bytes,
            // Members with different grids refuse each other's cuts.
            cut_interval_s,
            // Local.
            cut_lag_s: _,
            cut_lag_floor_s: _,
            cut_lag_cap_s: _,
            cut_lag_margin_s: _,
            fetch_retry_ns: _,
            keep_s: _,
            keep_bytes: _,
            relay: _,
            #[cfg(feature = "mutants")]
                mutation: _,
        } = self;
        // v2: defer_bytes joined the set (the stall fix); v3: cut_interval_s (the cut grid).
        let mut h = Hasher::new("pe.agree.consensus.v3");
        h.write_u64(frame_tolerance_s.to_bits());
        h.write_u64(*entries_per_frame as u64).write_u64(*extra_per_s as u64);
        h.write_u64(budget_slack.to_bits()).write_u64(burst_s.to_bits()).write_u64(pending_s.to_bits());
        h.write_u64(*min_rate_per_s as u64).write_u64(*min_burst as u64).write_u64(*min_pending as u64);
        h.write_u64(*defer_bytes as u64).write_u64(cut_interval_s.to_bits());
        h.write_u64(timing.cadence_ns as u64)
            .write_u64(timing.min_timeout_ns as u64)
            .write_u64(timing.max_timeout_ns as u64);
        h.finish()
    }

    /// The cut grid at the world's frame rate: every cut's frame is a multiple of this many frames (at least 1). The
    /// replica takes live roots and keeps keyframes on the same grid. 6 frames at 60 Hz, 9 at 90, 12 at 120 with the
    /// default 0.1 s.
    pub fn cut_every(&self, rate_hz: u32) -> u64 {
        (self.cut_interval_s * rate_hz as f64).round().max(1.0) as u64
    }

    /// The budget at the world's frame rate: `entries_per_frame × rate_hz × budget_slack + extra_per_s` entries per
    /// second, a burst of `burst_s` of that and a pending cap of `pending_s`, each at least its floor. With the
    /// defaults: 255/s, 127 and 1,020 at 60 Hz; 480/s, 240 and 1,920 at 120 Hz; at 30 Hz the rate's floor applies: 150/s, a burst of 75 and 600 pending.
    /// Every member must use the same settings, or some would refuse an honest member's entries.
    pub fn rate_budget(&self, rate_hz: u32) -> RateBudget {
        let per_frame = self.entries_per_frame as f64 * rate_hz as f64 * self.budget_slack;
        let per_s = (per_frame + self.extra_per_s as f64).max(self.min_rate_per_s as f64);
        RateBudget {
            per_s,
            burst: ((per_s * self.burst_s) as u32).max(self.min_burst),
            pending: ((per_s * self.pending_s) as usize).max(self.min_pending),
        }
    }
}

/// One author's checked entries waiting for its budget, in chain order, with who sent each.
#[derive(Default)]
struct Deferred {
    entries: BTreeMap<(ChainId, u64, EntryRef), (SignedEntry, PeerId)>,
    bytes: usize,
}

fn entry_bytes(e: &SignedEntry) -> usize {
    e.entry.payload.len() + 200
}

/// [`EntrySource::entries_for_cut`]'s verdicts for one cut, valid while the final block and the entries held are
/// unchanged.
struct RootCache {
    cut: Cut,
    version: (BlockId, u64),
    by_frame: BTreeMap<Frame, Vec<SignedEntry>>,
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
    /// The last cut reported final (the genesis cut before any), and its block.
    final_heads: Cut,
    final_block: BlockId,
    root_cache: std::cell::RefCell<Option<RootCache>>,
    /// Per author: checked entries over its budget, admitted as it refills.
    deferred: HashMap<MemberKey, Deferred>,
    /// Per (sender, author): how many more entries this member will check (tokens in thousandths), and when.
    verify_budget: HashMap<(PeerId, MemberKey), (u64, HostTime)>,
    /// Entries dropped unchecked over a sender's verify budget.
    dropped_unchecked: u64,
    /// Per chain: the entries held, by seq (the first at a seq), to serve a range refetch.
    by_seq: HashMap<ChainKey, BTreeMap<u64, EntryRef>>,
    /// Per chain: the highest seq this member has seen checked (held, waiting or refused). Only the author can raise
    /// it, by signing.
    seen_high: HashMap<ChainKey, u64>,
    /// Per chain: the highest seq a verified vote's heads name, and the voter that named it. A claim, not evidence:
    /// it is forgotten once its asks run out unanswered (@design-reviewer's Low 1), and each author has at most
    /// [`MAX_VOTE_CHAINS`] chains claimed this way.
    vote_high: HashMap<ChainKey, (u64, MemberKey)>,
    /// Per (chain, voter): a voter whose claim on the chain went unanswered, with the seq it claimed. Its claims on
    /// that chain are ignored until the chain's checked seq reaches that (an honest voter's claim is answered by the
    /// voter itself, so only an unreachable or lying voter is ignored, and its entries still come by relay and cuts).
    vote_ignored: HashMap<(ChainKey, MemberKey), u64>,
    /// Per chain: a gap seen and not yet asked for (its first missing seq, and since when). A gap is asked for only
    /// once it has lasted [`GAP_WAIT_NS`], so an entry merely reordered in flight isn't fetched twice.
    gap_since: HashMap<ChainKey, (u64, i64)>,
    /// When this member's own chain reached each seq, recently: pacing counts only entries older than the grace.
    own_times: std::collections::VecDeque<(i64, u64)>,
    /// Per chain: the range refetch asked (from seq, when, how many times).
    range_asked: HashMap<ChainKey, (u64, i64, u32)>,
    /// Per requester: the entries this member will still serve it by range refetch, in thousandths, and when
    /// (@design-reviewer's Medium 2 and later Low 1): one author's rate and burst across all authors, so a requester
    /// can't make this member upload more than about one author's stream.
    serve_budget: HashMap<PeerId, (u64, HostTime)>,
    /// Per other member: the highest seq of this member's own chain its verified votes name as held, for pacing.
    acked: HashMap<MemberKey, u64>,
    /// Entries inserted into the log so far (never decreases, whatever is pruned): the root cache's version.
    inserted: u64,
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
    /// Entries a verified vote's heads name, not held: (author, the voter that named it, since, last asked). Not
    /// solicited.
    hinted: BTreeMap<EntryRef, (MemberKey, MemberKey, i64, i64)>,
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
/// At most this many chains per author tracked from votes' heads alone.
const MAX_VOTE_CHAINS: usize = 4;
/// A range refetch claimed only by a vote's heads is given up after this many unanswered asks (the retry doubles,
/// so about 15 s at the default retry).
const RANGE_GIVE_UP: u32 = 5;
/// A gap is asked for by range only once it has lasted this long (a reorder in flight closes sooner).
const GAP_WAIT_NS: i64 = 150_000_000;

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
        let final_block = core.committed().0;
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
            final_block,
            root_cache: std::cell::RefCell::new(None),
            inserted: 0,
            deferred: HashMap::new(),
            verify_budget: HashMap::new(),
            dropped_unchecked: 0,
            by_seq: HashMap::new(),
            seen_high: HashMap::new(),
            vote_high: HashMap::new(),
            vote_ignored: HashMap::new(),
            gap_since: HashMap::new(),
            own_times: std::collections::VecDeque::new(),
            range_asked: HashMap::new(),
            serve_budget: HashMap::new(),
            acked: HashMap::new(),
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
        let solicited = self.wanted.contains_key(&id);
        if author != self.me && !solicited && !self.by_ref.contains_key(&id) && !self.may_verify(from, author, at) {
            // More than this sender can make this member check for one author: dropped unchecked, no fault. If it was
            // valid, the range refetch brings it back (coordinator m120f73db271d).
            self.dropped_unchecked += 1;
            return Ok(false);
        }
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
        let seen = self.seen_high.entry(key).or_default();
        *seen = (*seen).max(e.entry.seq);
        let links = self.fills_hole(&key, e.entry.seq);
        // While the author has entries waiting, a new one waits behind them, in chain order, rather than taking the
        // budget first.
        let queued = !solicited && self.deferred.get(&author).is_some_and(|q| !q.entries.is_empty());
        if author != self.me
            && !self.by_ref.contains_key(&id)
            && (queued || !self.within_budget(author, solicited, links, at))
        {
            // Over the budget: delay it, never lose it (coordinator m120f73db271d). It waits, checked, in the author's
            // queue, and is admitted (and relayed) in chain order as the budget refills; only an overflow is refused.
            return self.defer(e, from);
        }
        self.insert_checked(e, from, at)
    }

    /// Queue a checked entry over its author's budget; refuse it only if the author's queue is full.
    fn defer(&mut self, e: SignedEntry, from: PeerId) -> Result<bool, Fault> {
        let id = e.id();
        let author = e.entry.author;
        let cap = self.cfg.rate_budget(self.rate_hz).pending;
        let size = entry_bytes(&e);
        let q = self.deferred.entry(author).or_default();
        let k = (e.entry.chain, e.entry.seq, id);
        if q.entries.contains_key(&k) {
            return Ok(false);
        }
        // Full: the earliest entries in chain order matter most (they link the chain), so a later one makes room by
        // leaving, with no fault; the range refetch brings it back. Only an entry later than all of them is refused.
        while (q.entries.len() >= cap || q.bytes + size > self.cfg.defer_bytes)
            && q.entries.last_key_value().is_some_and(|(last, _)| *last > k)
        {
            if let Some((_, (old, _))) = q.entries.pop_last() {
                q.bytes = q.bytes.saturating_sub(entry_bytes(&old));
            }
        }
        if q.entries.len() >= cap || q.bytes + size > self.cfg.defer_bytes {
            self.events.push(AgreeEvent::Refused { entry: id, author, kind: FaultKind::RateExceeded });
            let detail = "over the author's budget and its queue".into();
            return Err(self.fault(from, Some(author), FaultKind::RateExceeded, detail));
        }
        q.bytes += size;
        q.entries.insert(k, (e, from));
        Ok(false)
    }

    /// Admit waiting entries, each author's in chain order, while its budget allows; relay each one admitted that its
    /// author sent.
    fn admit_deferred(&mut self, at: HostTime) {
        let authors: Vec<MemberKey> =
            self.deferred.iter().filter(|(_, q)| !q.entries.is_empty()).map(|(a, _)| *a).collect();
        for a in authors {
            loop {
                let Some((k, (e, from))) =
                    self.deferred.get(&a).and_then(|q| q.entries.iter().next().map(|(k, v)| (*k, v.clone())))
                else {
                    break;
                };
                let held = self.by_ref.contains_key(&k.2);
                let links = self.fills_hole(&ChainKey { author: a, chain: k.0 }, k.1);
                if !held && !self.within_budget(a, false, links, at) {
                    break;
                }
                if let Some(q) = self.deferred.get_mut(&a) {
                    q.entries.remove(&k);
                    q.bytes = q.bytes.saturating_sub(entry_bytes(&e));
                }
                if held {
                    continue;
                }
                if let Ok(true) = self.insert_checked(e.clone(), from, at)
                    && self.cfg.relay
                    && from == a
                {
                    self.send_all(Lane::Action, &Wire::Entries(vec![e]), &[from]);
                }
            }
        }
        self.deferred.retain(|_, q| !q.entries.is_empty());
    }

    /// Whether an entry at `seq` on chain `key` extends the linked head while entries beyond it are already held: it
    /// fills a hole. Exempting only these from the pending cap keeps an author's held entries within about twice the
    /// cap (a hole can be filled only once), while a cap full of entries behind a hole can always drain.
    fn fills_hole(&self, key: &ChainKey, seq: u64) -> bool {
        seq == self.linked_seq(key) + 1 && self.by_seq.get(key).is_some_and(|m| m.range(seq + 1..).next().is_some())
    }

    /// The seq this member's chain `key` links through, from the final cut's head on.
    pub(crate) fn linked_seq(&self, key: &ChainKey) -> u64 {
        if let Some((seq, _)) = self.pred_head.get(key) {
            return *seq;
        }
        let i = self.final_heads.heads.binary_search_by(|h| h.chain.cmp(&key.chain)).ok();
        i.map(|i| self.final_heads.heads[i]).filter(|h| h.author == key.author).map_or(0, |h| h.seq)
    }

    /// The range refetch (coordinator m120f73db271d): for each chain with a hole (a seq beyond its linked head seen,
    /// the next one neither held nor waiting), ask for just the gap, the author first and everyone on a retry, with
    /// the retry doubling. The answer is charged to the author's budget like any entry (it waits if over), so a
    /// refusal, an unchecked drop and a network loss all heal the same way, a gap per round trip.
    fn ask_ranges(&mut self) {
        let now = self.now.0;
        let mut asks: Vec<(ChainKey, u64, u32, u32)> = Vec::new();
        let mut give_up: Vec<ChainKey> = Vec::new();
        let mut new_gaps: Vec<(ChainKey, u64)> = Vec::new();
        let keys: std::collections::BTreeSet<ChainKey> =
            self.seen_high.keys().chain(self.vote_high.keys()).copied().collect();
        for key in &keys {
            if key.author == self.me {
                continue;
            }
            let checked = self.seen_high.get(key).copied().unwrap_or(0);
            let high = self.range_high(key);
            let next = self.linked_seq(key) + 1;
            if high < next {
                continue;
            }
            let held = self.by_seq.get(key);
            let waiting = self.deferred.get(&key.author);
            let lo = (key.chain, next, EntryRef(Hash([0; 32])));
            let first_waiting =
                waiting.and_then(|q| q.entries.range(lo..).next().filter(|(k, _)| k.0 == key.chain).map(|(k, _)| k.1));
            let first_held = held.and_then(|m| m.range(next..).next().map(|(s, _)| *s));
            let end = [first_waiting, first_held].into_iter().flatten().min().unwrap_or(high + 1);
            if end == next {
                continue;
            }
            let count = (end - next).min(MAX_FETCH as u64) as u32;
            let tries = match self.range_asked.get(key) {
                Some(&(f, at, t)) if f == next => {
                    if now.saturating_sub(at) < self.cfg.fetch_retry_ns << t.min(4) {
                        continue;
                    }
                    // Only a vote claims the gap (no checked entry beyond it), and nobody answered: forget the claim.
                    if t + 1 >= RANGE_GIVE_UP && checked < next {
                        give_up.push(*key);
                        continue;
                    }
                    t + 1
                }
                _ => match self.gap_since.get(key) {
                    Some(&(f, since)) if f == next && now.saturating_sub(since) >= GAP_WAIT_NS => 0,
                    Some(&(f, _)) if f == next => continue,
                    _ => {
                        new_gaps.push((*key, next));
                        continue;
                    }
                },
            };
            asks.push((*key, next, count, tries));
        }
        for key in give_up {
            if let Some((seq, voter)) = self.vote_high.remove(&key) {
                let s = self.vote_ignored.entry((key, voter)).or_default();
                *s = (*s).max(seq);
            }
            self.range_asked.remove(&key);
        }
        for (key, next) in new_gaps {
            self.gap_since.insert(key, (next, now));
        }
        for (key, next, count, tries) in asks {
            self.gap_since.remove(&key);
            self.range_asked.insert(key, (next, now, tries));
            let w = Wire::FetchRange { author: key.author, chain: key.chain, from_seq: next, count };
            if tries == 0 {
                let b = Bytes::from(codec::encode(&w));
                self.out.push(Outgoing { to: key.author, lane: Lane::Protocol, bytes: b });
            } else {
                self.send_all(Lane::Protocol, &w, &[]);
            }
        }
        let done: Vec<ChainKey> =
            self.range_asked.keys().filter(|k| self.range_high(k) <= self.linked_seq(k)).copied().collect();
        for k in done {
            self.range_asked.remove(&k);
        }
        let closed: Vec<ChainKey> =
            self.gap_since.iter().filter(|(k, (f, _))| self.linked_seq(k) + 1 != *f).map(|(k, _)| *k).collect();
        for k in closed {
            self.gap_since.remove(&k);
        }
        let linked: Vec<(ChainKey, u64)> = self.vote_high.keys().map(|k| (*k, self.linked_seq(k))).collect();
        for (k, l) in linked {
            if self.vote_high.get(&k).is_some_and(|(s, _)| *s <= l) {
                self.vote_high.remove(&k);
            }
        }
    }

    /// The highest seq known on a chain: checked, or claimed by a vote not yet given up.
    fn range_high(&self, key: &ChainKey) -> u64 {
        let checked = self.seen_high.get(key).copied().unwrap_or(0);
        self.vote_high.get(key).map_or(checked, |(s, _)| checked.max(*s))
    }

    /// A verified vote's heads name `seq` on `key`: remember the claim, within the bounds (see `vote_high`).
    fn claim_from_vote(&mut self, voter: MemberKey, key: ChainKey, seq: u64) {
        let checked = self.seen_high.get(&key).copied().unwrap_or(0);
        if seq <= checked {
            return;
        }
        match self.vote_ignored.get(&(key, voter)) {
            Some(&claimed) if checked < claimed => return,
            Some(_) => {
                self.vote_ignored.remove(&(key, voter));
            }
            None => {}
        }
        match self.vote_high.get_mut(&key) {
            Some(c) => {
                if seq > c.0 {
                    *c = (seq, voter);
                }
            }
            None => {
                let known = self.seen_high.contains_key(&key) || self.by_seq.contains_key(&key);
                // Given-up claims keep their slot, so fabricated chain ids can't cycle through it.
                let claimed: std::collections::BTreeSet<ChainKey> = self
                    .vote_high
                    .keys()
                    .chain(self.vote_ignored.keys().map(|(k, _)| k))
                    .filter(|k| k.author == key.author)
                    .copied()
                    .collect();
                let claimed = claimed.len();
                if known || claimed < MAX_VOTE_CHAINS {
                    self.vote_high.insert(key, (seq, voter));
                }
            }
        }
    }

    /// Whether this member will check one more entry `from` sends claiming `author`: twice the author's rate, with a
    /// burst of twice its pending cap plus its burst, so a flush the queue can take is checked whole, and a flood
    /// from one sender overflows the queue (a fault) before it is dropped unchecked.
    fn may_verify(&mut self, from: PeerId, author: MemberKey, at: HostTime) -> bool {
        if self.core.mutated(Mutation::RateLimitRemoved) {
            return true;
        }
        let b = self.cfg.rate_budget(self.rate_hz);
        let cap = 2 * (b.pending as u64 + b.burst as u64) * 1000;
        let rate = (2.0 * b.per_s * 1000.0).round() as u128;
        let (tokens, last) = self.verify_budget.get(&(from, author)).copied().unwrap_or((cap, at));
        let dt = at.0.saturating_sub(last.0).max(0) as u128;
        let tokens = (tokens as u128 + rate * dt / 1_000_000_000).min(cap as u128) as u64;
        let ok = tokens >= 1000;
        self.verify_budget.insert((from, author), (if ok { tokens - 1000 } else { tokens }, at.max(last)));
        ok
    }

    /// How many entries this member will serve `from` in one range answer, at most `want`, and takes them from its
    /// serving budget (one author's rate and burst per requester, whatever chains it asks for). An answer it can't
    /// give now is simply short; the requester asks again (@design-reviewer's Medium 2).
    fn may_serve(&mut self, from: PeerId, want: usize, at: HostTime) -> usize {
        let b = self.cfg.rate_budget(self.rate_hz);
        let cap = b.burst as u64 * 1000;
        let rate = (b.per_s * 1000.0).round() as u128;
        let (tokens, last) = self.serve_budget.get(&from).copied().unwrap_or((cap, at));
        let dt = at.0.saturating_sub(last.0).max(0) as u128;
        let tokens = (tokens as u128 + rate * dt / 1_000_000_000).min(cap as u128) as u64;
        let n = ((tokens / 1000) as usize).min(want);
        self.serve_budget.insert(from, (tokens - n as u64 * 1000, at.max(last)));
        n
    }

    /// Insert an entry that passed every check and the budget.
    fn insert_checked(&mut self, e: SignedEntry, from: PeerId, at: HostTime) -> Result<bool, Fault> {
        let id = e.id();
        let author = e.entry.author;
        let key = ChainKey::of(&e);
        let prev = e.entry.prev;
        let e_seq = e.entry.seq;
        let solicited = self.wanted.contains_key(&id);
        let hinted_by = self.hinted.get(&id).map(|h| h.1);
        let hinted = hinted_by.is_some();
        let unchecked = AcceptAll;
        let skip_sig = self.core.mutated(Mutation::NoSignatureCheck);
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
                self.by_seq.entry(key).or_default().entry(e_seq).or_insert(id);
                self.inserted += 1;
                self.arrived.entry(id).or_insert(at);
                self.wanted.remove(&id);
                self.hinted.remove(&id);
                *self.pending_by_author.entry(author).or_default() += 1;
                // A fetched entry past a hole: fetch the one before it too, so the chain can link.
                if let Some(p) = prev.filter(|p| !self.by_ref.contains_key(p)) {
                    if solicited {
                        self.want([p]);
                    } else if let Some(voter) = hinted_by {
                        self.hint(voter, author, p);
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
    /// `links`: the entry fills a hole (see [`Node::fills_hole`]). It is exempt from the pending cap, since it is what
    /// lets the author's pending entries become final, but not from the tokens.
    fn within_budget(&mut self, author: MemberKey, solicited: bool, links: bool, at: HostTime) -> bool {
        if self.core.mutated(Mutation::RateLimitRemoved) {
            return true;
        }
        // Tokens in thousandths of an entry; the rate in thousandths of an entry per second.
        let budget = self.cfg.rate_budget(self.rate_hz);
        let cap = budget.burst as u64 * 1000;
        let rate = (budget.per_s * 1000.0).round() as u128;
        let (tokens, last) = self.budget.get(&author).copied().unwrap_or((cap, at));
        let dt = at.0.saturating_sub(last.0).max(0) as u128;
        let tokens = (tokens as u128 + rate * dt / 1_000_000_000).min(cap as u128) as u64;
        let pending = self.pending_by_author.get(&author).copied().unwrap_or(0);
        if !solicited && (tokens < 1000 || (pending >= budget.pending && !links)) {
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

    /// Fetch an entry a vote's heads name, as a hint: at most one per author's free pending slot, each voter at most
    /// its share of [`MAX_HINTS`] (so a flooding voter can't crowd out an honest one's hints), and dropped after
    /// [`HINT_TTL_NS`] if it hasn't come.
    fn hint(&mut self, voter: MemberKey, author: MemberKey, id: EntryRef) {
        if self.wanted.contains_key(&id) || self.hinted.contains_key(&id) {
            return;
        }
        let share = (MAX_HINTS / self.cfg.members.len().max(1)).max(1);
        if self.hinted.values().filter(|(_, v, _, _)| *v == voter).count() >= share {
            return;
        }
        let pending = self.pending_by_author.get(&author).copied().unwrap_or(0);
        let hinted = self.hinted.values().filter(|(a, _, _, _)| *a == author).count();
        if pending + hinted >= self.cfg.rate_budget(self.rate_hz).pending {
            return;
        }
        self.hinted.insert(id, (author, voter, self.now.0, i64::MIN));
    }

    fn ask(&mut self) {
        let now = self.now.0;
        self.hinted.retain(|_, (_, _, since, _)| now.saturating_sub(*since) < HINT_TTL_NS);
        let retry = self.cfg.fetch_retry_ns;
        let due_at = |t: i64| t == i64::MIN || now.saturating_sub(t) >= retry;
        let mut due: Vec<EntryRef> =
            self.wanted.iter().filter(|(_, t)| due_at(**t)).map(|(id, _)| *id).take(MAX_FETCH).collect();
        let room = MAX_FETCH - due.len();
        due.extend(self.hinted.iter().filter(|(_, (_, _, _, t))| due_at(*t)).map(|(id, _)| *id).take(room));
        if due.is_empty() {
            return;
        }
        for id in &due {
            if let Some(t) = self.wanted.get_mut(id) {
                *t = now;
            } else if let Some(h) = self.hinted.get_mut(id) {
                h.3 = now;
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
            self.final_block = block.id();
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
                self.final_bytes = self.final_bytes.saturating_sub(v.iter().map(entry_bytes).sum());
            }
            self.view_by_frame.remove(&f);
        }
        if to > self.kept_from {
            self.kept_from = to;
            let stale: Vec<Frame> = self.view_by_frame.range(..to).map(|(f, _)| *f).collect();
            for f in stale {
                self.view_by_frame.remove(&f);
            }
            self.prune_by_seq();
        }
    }

    /// Drop the range index's rows for entries stamped below the kept frames (@design-reviewer's Low 2): no honest
    /// member asks that far back by range (it would be late everywhere). Each chain's prefix only, so the index stays
    /// a contiguous run of a chain's tail; the log itself is not pruned yet (spec 5).
    fn prune_by_seq(&mut self) {
        let to = self.kept_from;
        for (key, m) in self.by_seq.iter_mut() {
            while let Some((&seq, id)) = m.iter().next() {
                if self.log.get(key, id).is_some_and(|e| e.entry.stamp >= to) {
                    break;
                }
                m.remove(&seq);
            }
        }
        self.by_seq.retain(|_, m| !m.is_empty());
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
        let mut c = if p.block.cut.at.frame.0 % self.cfg.cut_every(self.rate_hz) != 0 {
            Check::Reject(cuts::Reject::OffGrid)
        } else {
            cuts::check(&self.log, &parent.cut, &p.block.cut, present, self.tolerance_frames())
        };
        // The d1 rule counts from the previous final cut, not the parent: every entry the cut's uncommitted ancestry
        // includes must be held too, or the root can't be the final log's (the pipelined-void fix).
        if matches!(c, Check::Ok) {
            let chain = self.ancestry(&p.block.cut)?;
            if let Err(missing) = self.effective_in(&chain) {
                if missing.is_empty() {
                    return None;
                }
                c = Check::Missing(missing);
            }
        }
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

    /// The entries `chain` (oldest first, from just above the final cut) makes effective, each block judged against
    /// its parent as at finalization; or every entry missing for that.
    fn effective_in(&self, chain: &[Block]) -> Result<Vec<SignedEntry>, Vec<EntryRef>> {
        let mut parent = &self.final_heads;
        let mut highs = self.final_high.clone();
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for b in chain {
            for h in &b.cut.heads {
                let key = ChainKey { author: h.author, chain: h.chain };
                let from = parent.heads.binary_search_by(|p| p.chain.cmp(&h.chain)).ok().map(|i| parent.heads[i].entry);
                if from == Some(h.entry) {
                    continue;
                }
                match self.log.judge(&key, from, h.entry, parent.at.frame, highs.get(&key).copied()) {
                    Ok((verdicts, high)) => {
                        if let Some(f) = high {
                            highs.insert(key, f);
                        }
                        for (id, v) in verdicts {
                            if v == Verdict::Effective {
                                out.extend(self.log.get(&key, &id).cloned());
                            }
                        }
                    }
                    Err(pe_log::SegmentError::Missing(x)) => missing.push(x),
                    // A certified chain that doesn't link: more than t faulty voters; never rooted.
                    Err(_) => return Err(Vec::new()),
                }
            }
            parent = &b.cut;
        }
        if missing.is_empty() { Ok(out) } else { Err(missing) }
    }

    /// How many of this member's own entries are beyond the final cut (a diagnostic). It grows with the cut lag, so
    /// pacing uses [`Node::backlog`] instead.
    pub fn unfinal_own(&self) -> usize {
        let Some((key, seq, _)) = self.my_head else { return 0 };
        let i = self.final_heads.heads.binary_search_by(|h| h.chain.cmp(&key.chain)).ok();
        let fin = i.map(|i| self.final_heads.heads[i]).filter(|h| h.author == key.author).map_or(0, |h| h.seq);
        seq.saturating_sub(fin) as usize
    }

    /// The burst at the world's current rate: the backlog over which a driver stops making hand entries.
    pub fn burst(&self) -> usize {
        self.cfg.rate_budget(self.rate_hz).burst as usize
    }

    /// This member's unacknowledged backlog: its own entries beyond what a quorum holds, counting itself and, for each
    /// other member, the head its latest verified vote names (at least the final cut's). Unlike
    /// [`Node::unfinal_own`], it doesn't grow with the cut lag: peers name heads beyond the cut they vote on
    /// (@design-reviewer's Medium 1). It grows when this member's entries stop reaching peers, which is what pacing is
    /// for. With no votes at all (a view change), it counts from the final cut.
    pub fn backlog(&self) -> usize {
        let Some((key, _, _)) = self.my_head else { return 0 };
        // Until a quorum's worth of peers have named this member's chain in a vote (the session's first cut lag)
        // there is nothing to count against.
        let need = self.core.quorum().saturating_sub(1);
        if self.acked.len() < need {
            return 0;
        }
        // Entries younger than the grace can't be named by a vote yet; counting them would pace on the vote cadence.
        let grace = self.cfg.timing.cadence_ns + self.cfg.timing.min_timeout_ns;
        let before = self.now.0.saturating_sub(grace);
        let seq = self.own_times.iter().rev().find(|(t, _)| *t <= before).map_or(0, |(_, s)| *s);
        let i = self.final_heads.heads.binary_search_by(|h| h.chain.cmp(&key.chain)).ok();
        let fin = i.map(|i| self.final_heads.heads[i]).filter(|h| h.author == key.author).map_or(0, |h| h.seq);
        let mut held: Vec<u64> = self
            .cfg
            .members
            .iter()
            .filter(|m| **m != self.me)
            .map(|m| self.acked.get(m).copied().unwrap_or(0).max(fin))
            .collect();
        held.sort_unstable_by(|a, b| b.cmp(a));
        // The quorum counts this member; the (q - 1)th highest of the others is held by a quorum.
        let Some(&h) = need.checked_sub(1).and_then(|k| held.get(k)) else { return 0 };
        seq.saturating_sub(h) as usize
    }

    /// Record when the own chain reached `seq`, keeping a little over the pacing grace of history.
    fn note_own(&mut self, seq: u64) {
        let now = self.now.0;
        let keep = 2 * (self.cfg.timing.cadence_ns + self.cfg.timing.min_timeout_ns);
        match self.own_times.back_mut() {
            Some((t, s)) if *t == now => *s = seq,
            _ => self.own_times.push_back((now, seq)),
        }
        while self.own_times.len() > 2 && self.own_times.get(1).is_some_and(|(t, _)| now.saturating_sub(*t) > keep) {
            self.own_times.pop_front();
        }
    }

    /// Whether the driver should make no new hand entries now: the unacknowledged backlog is over the burst, so more
    /// would only be refused or void as late (coordinator m120f73db271d, point 3). Edits are never held back.
    pub fn backlogged(&self) -> bool {
        self.backlog() > self.burst()
    }

    /// Entries held in the log, and entries waiting for their author's budget (diagnostics).
    pub fn held_entries(&self) -> usize {
        self.by_ref.len()
    }

    pub fn config(&self) -> &NodeConfig {
        &self.cfg
    }

    /// This member's own chain and its head seq (diagnostics).
    pub fn own_head(&self) -> Option<(ChainKey, u64)> {
        self.my_head.map(|(k, s, _)| (k, s))
    }

    /// The live vote claim on a chain, if any (a test).
    #[cfg(test)]
    pub(crate) fn vote_claim(&self, key: &ChainKey) -> Option<u64> {
        self.vote_high.get(key).map(|c| c.0)
    }

    /// Chains claimed by votes alone, live or given up (a test of their bound).
    #[cfg(test)]
    pub(crate) fn vote_claims(&self) -> (usize, usize) {
        (self.vote_high.len(), self.vote_ignored.len())
    }

    /// As a verified vote from `voter` naming `seq` on `key` would (a test of a Byzantine voter's heads).
    #[cfg(test)]
    pub(crate) fn claim_for_test(&mut self, voter: MemberKey, key: ChainKey, seq: u64) {
        self.claim_from_vote(voter, key, seq);
    }

    pub fn deferred_entries(&self) -> usize {
        self.deferred.values().map(|q| q.entries.len()).sum()
    }

    /// How many blocks from just above the final cut up to `cut`'s, if it descends from the final cut (a diagnostic:
    /// 1 is a cut on the final cut, 2 or more a pipeline).
    pub fn rooting_depth(&self, cut: &Cut) -> Option<usize> {
        self.ancestry(cut).map(|c| c.len())
    }

    /// The blocks from just above this member's final cut up to the one carrying `cut`, oldest first: the core's
    /// blocks, then, below the core's committed block (it prunes there), the final blocks still waiting here for
    /// their entries. `None` if `cut` doesn't descend from the final cut, or a block between is not held.
    fn ancestry(&self, cut: &Cut) -> Option<Vec<Block>> {
        let fin = &self.final_heads;
        if *cut == *fin {
            return Some(Vec::new());
        }
        let queued: Vec<BlockId> = self.final_queue.iter().map(|(b, _)| b.id()).collect();
        let mut chain = vec![self.core.block_with_cut(cut)?.clone()];
        loop {
            let b = chain.last().expect("never empty");
            if b.cut.at.height <= fin.at.height + 1 {
                break;
            }
            let parent = b.parent;
            if let Some(i) = queued.iter().position(|id| *id == parent) {
                chain.extend(self.final_queue[..=i].iter().rev().map(|(b, _)| b.clone()));
                break;
            }
            let p = self.core.block(&parent)?.clone();
            chain.push(p);
        }
        let oldest = chain.last().expect("never empty");
        if oldest.cut.at.height != fin.at.height + 1 {
            return None;
        }
        if oldest.parent != self.final_block {
            return None;
        }
        chain.reverse();
        Some(chain)
    }

    fn held(&self, frame: Frame) -> bool {
        self.holds.keys().next().is_some_and(|h| frame >= *h)
    }
}

impl EntrySource for Node {
    fn final_cut(&self) -> Option<&FinalCut> {
        self.final_cut.as_ref()
    }

    /// A cut's own final log (pe-seams v1.6; the pipelined-void fix, coordinator m90c32fc9ceb2): exactly what the final
    /// log would hold at `frame` were `cut` final, with the same entries, verdicts and order. Computed from the cut's
    /// own ancestry, never from local in-flight state: each block from just above the final cut up to the cut's is
    /// judged against its parent, as at finalization. The verdicts are computed once per cut, and kept until the final
    /// cut moves or an entry arrives (pe-replica asks for every frame up to the cut; @design-reviewer m96af23d3de0c).
    /// `None` below `kept_from`, or for a cut that doesn't descend from the final cut or whose entries aren't all
    /// held (`wants_roots` never hands such a cut out).
    fn entries_for_cut(&self, cut: &Cut, frame: Frame) -> Option<Vec<SignedEntry>> {
        if frame < self.kept_from {
            return None;
        }
        if frame >= cut.at.frame {
            return self.entries_at(frame).map(|v| v.to_vec());
        }
        if frame < self.final_frame() {
            // Final already: no later cut makes an entry stamped below the final frame effective (it would be late).
            return Some(self.final_by_frame.get(&frame).cloned().unwrap_or_default());
        }
        let version = (self.final_block, self.inserted);
        if let Some(c) = self.root_cache.borrow().as_ref().filter(|c| c.cut == *cut && c.version == version) {
            return Some(c.by_frame.get(&frame).cloned().unwrap_or_default());
        }
        let chain = self.ancestry(cut)?;
        let mut by_frame: BTreeMap<Frame, Vec<SignedEntry>> = BTreeMap::new();
        for e in self.effective_in(&chain).ok()? {
            by_frame.entry(e.entry.stamp).or_default().push(e);
        }
        for v in by_frame.values_mut() {
            self.order(v);
        }
        let out = by_frame.get(&frame).cloned().unwrap_or_default();
        *self.root_cache.borrow_mut() = Some(RootCache { cut: cut.clone(), version, by_frame });
        Some(out)
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

    /// pe-app paces hand entries on this (S3 v1.5). The provided default is `false`, so without this override pe-app
    /// would never pace (gate item 3 for the first two-machine run).
    fn backlogged(&self) -> bool {
        Node::backlogged(self)
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
        self.note_own(e.entry.seq);
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
                let heads = if let core::Msg::Vote(v) = &m { (v.voter, v.heads.clone()) } else { (from, Vec::new()) };
                match self.core.receive(from, m, at, self.verifier.as_ref()) {
                    Ok(()) => {
                        let (voter, heads) = heads;
                        let mine = self.my_head.map(|(k, _, _)| k);
                        for h in heads {
                            let key = ChainKey { author: h.author, chain: h.chain };
                            if Some(key) == mine && voter != self.me {
                                let a = self.acked.entry(voter).or_default();
                                *a = (*a).max(h.seq);
                            }
                            if self.cfg.members.contains(&h.author) && h.author != self.me {
                                self.claim_from_vote(voter, key, h.seq);
                            }
                            if self.cfg.members.contains(&h.author) && !self.by_ref.contains_key(&h.entry) {
                                self.hint(voter, h.author, h.entry);
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
            Wire::FetchRange { author, chain, from_seq, count } => {
                let key = ChainKey { author, chain };
                let count = self.may_serve(from, count as usize, at);
                let es: Vec<SignedEntry> = self
                    .by_seq
                    .get(&key)
                    .map(|m| {
                        m.range(from_seq..).take(count).filter_map(|(_, id)| self.log.get(&key, id).cloned()).collect()
                    })
                    .unwrap_or_default();
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
                self.admit_deferred(at);
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
        self.admit_deferred(now);
        self.ask_ranges();
        self.core.tick(now, self.signer.as_ref());
        let lag = self.cut_lag_frames();
        if let (Some(lead), Some(present)) = (self.core.lead(), self.present) {
            // On the grid: the latest grid frame at or below the present less the lag.
            let k = self.cfg.cut_every(self.rate_hz);
            let frame = Frame(present.0.saturating_sub(lag) / k * k);
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
