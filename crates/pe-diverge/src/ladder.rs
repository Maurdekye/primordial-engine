//! Detection at final cuts, the repair ladder's rungs 1 and 2, and root splits (divergence.md 3-5), on seams S2 v1.
//!
//! Sans-IO: [`Diverge`] takes final cuts with the replica's verdict, root splits, peer messages and host time, drives
//! the replica through S2, and returns repair-lane messages, trace events ([`DivergeEvent`]) and actions for the
//! driver ([`Action`]: revote, self-test, write a bundle). It owns no thread, socket, clock or randomness.
//!
//! Who checks what:
//! - the replica compares roots at a final cut (`finalize`, the only call that moves `Cv` and the floor), replays
//!   from `Cv` (`replay_to`, rung 1) and checks every imported record's proof against the target root
//!   (`import_at_cut`);
//! - pe-diverge checks every node a source sends against the hash its parent named, from the target root down
//!   (`Node::hash`), so a lying source is caught at its first lie; and it decides what to fetch, from whom, and what
//!   to do when a rung fails. After a repair at a final cut it asks the replica to `finalize` that cut again.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bytes::Bytes;
use pe_seams::{
    ChainHead, Cut, CutImport, CutRef, DivergeEvent, DomainId, EntrySource, Fault, FaultKind, FinalCut, Finalized,
    Frame, FrameClock, GroupSel, Hash, HostTime, ImportError, Lane, LeafKey, MemberKey, Nibbles, Node, Outgoing,
    PeerId, PhysicsImage, Record, Replica, RootError, Rung, SelfTest, Tree, TreePath, frames_ceil,
};

use crate::bundle::FaultBundle;
use crate::counters::Counters;
use crate::evidence::{CvState, all_keys};
use crate::msg::{MAX_ITEMS, Msg, blob_size, node_size, path_size, record_size};
use crate::split;

/// The ladder's parameters (divergence.md 14). Every limit is a duration or a count, never a frame count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Roots of final cuts this recent are served to peers (and at least the last 2 cuts).
    pub t_keep: Duration,
    /// The deepest replay run in the foreground: a repair older than this restarts at the newest cut (4.2).
    pub w_fg: Duration,
    /// Wait for a repair source before trying the next.
    pub t_peer: Duration,
    /// A join's wait for its source (divergence.md 4.5): a whole-state reply, the physics image among them, can be
    /// megabytes, and arrives only whole. Also the least time before a donor serves one peer the same image again.
    pub t_join_peer: Duration,
    /// A join's deadline from its start: until then, a join whose sources all failed asks them again in turn, after
    /// `t_peer`. pe-app's own wait for a join step is 30 s (`T_join`, app.md 4).
    pub t_join: Duration,
    /// Descents per rung-2 attempt.
    pub n_descend: u32,
    /// Time within which repeated cures of one leaf count, and how many raise the event (4.3).
    pub t_recur: Duration,
    pub n_recur_max: u32,
    /// Paths per node request, and keys per record request.
    pub node_request_paths: usize,
    pub record_request_keys: usize,
    /// Bytes per reply message: a larger answer is split into several replies.
    pub reply_budget: usize,
    /// Serving, per peer: a bucket of `serve_burst_bytes`, refilled at `serve_rate` bytes per second. A request is
    /// served while the bucket is above zero, and its reply's bytes are taken from it; past that the answer is `Busy`.
    /// The burst covers a whole-state fetch of an M1 world.
    pub serve_burst_bytes: u64,
    pub serve_rate: u64,
    /// `Busy` answers in a row from one source before it is rotated, and the longest wait one of them can ask for.
    pub n_busy_max: u32,
    pub busy_wait_max: Duration,
    /// A repair failing again and again stops the ladder (4.3): after `n_stuck` failed rung-2 repairs in a row with
    /// one key, or `n_stuck_any` in a row with no cure whatever the key. While stopped, cuts are still compared, and
    /// one probe repair runs every `t_stuck_probe` to see whether the key changed.
    pub n_stuck: u32,
    pub n_stuck_any: u32,
    pub t_stuck_probe: Duration,
    /// The largest replay evidence a bundle carries (6): `Cv`'s whole state, encoded. A larger state is left out,
    /// and counted (`evidence_skipped`).
    pub evidence_max_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            t_keep: Duration::from_millis(1500),
            w_fg: Duration::from_secs(2),
            t_peer: Duration::from_millis(250),
            t_join_peer: Duration::from_secs(5),
            t_join: Duration::from_secs(25),
            n_descend: 3,
            t_recur: Duration::from_millis(1500),
            n_recur_max: 3,
            node_request_paths: 64,
            record_request_keys: 64,
            reply_budget: 1 << 20,
            serve_burst_bytes: 8 << 20,
            serve_rate: 4 << 20,
            n_busy_max: 4,
            busy_wait_max: Duration::from_secs(1),
            n_stuck: 3,
            n_stuck_any: 10,
            t_stuck_probe: Duration::from_secs(30),
            evidence_max_bytes: 64 << 20,
        }
    }
}

/// What the driver does for pe-diverge: S3 and the file system are the driver's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Vote again on a pending cut with this root (S3 `Agreement::vote`, divergence.md 5).
    Revote { cut: CutRef, root: Hash },
    /// Re-run the physics self-test now, and publish the verdict in the next vote (5, step 2).
    RunSelfTest { cut: CutRef },
    /// Complete the bundle (build, state at `Cv`, entries) and write it under its name (6).
    WriteBundle(Box<FaultBundle>),
}

/// Planted defects, each of which a test must catch (divergence.md 12; pe-harness DESIGN.md 6.1). Only the `mutants`
/// feature can switch one on. (ImportUnchecked and PatchNoReplay are pe-replica's: its `import_at_cut` checks the
/// proofs and re-simulates to the present.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutant {
    /// Diff against my own tree without rehashing it first.
    SkipVerify,
    /// Keep asking a source that failed: a bad node or proof, `NotHeld`, or silence.
    StickWithSource,
    /// Accept a node without checking it against the hash its parent named.
    UncheckedNodes,
}

enum Phase {
    /// The paths asked, each with the hash its parent named, and those not asked yet.
    Nodes {
        asked: Vec<(TreePath, Hash)>,
        queued: Vec<(TreePath, Hash)>,
    },
    /// The keys of the request in flight still unanswered, and the keys not asked yet.
    Records {
        asked: BTreeSet<LeafKey>,
        queued: Vec<LeafKey>,
    },
    Physics,
    /// The import is with the replica, which answered `Pending`: asked again, unchanged, on each tick (S2 v1.1).
    Importing {
        import: Box<CutImport>,
        mine: Vec<Record>,
    },
}

/// Rung 1, or a root split's step 1: a replay the replica may run over several ticks (`RootError::Pending`, S2 v1.1),
/// asked again with the same cut on each tick until it answers.
struct Replay {
    cut: Cut,
    /// The final cut for rung 1; `None` for a root split's step 1.
    final_cut: Option<FinalCut>,
    /// My root at the cut, as `finalize` found it.
    mine: Option<Hash>,
}

struct Repair {
    cut: Cut,
    /// `Cv` when the repair started: the bundle's `cv`, whatever a cure's `finalize` moves `Cv` to.
    from_cv: Option<(CutRef, Hash)>,
    /// Replay evidence was captured for this repair, at `from_cv` (6). Never for a root split's.
    evidence: bool,
    /// The final cut repaired toward, or `None` for a root split's reference root (the cut is not final).
    final_cut: Option<FinalCut>,
    target: Hash,
    voters: Vec<MemberKey>,
    /// My root at the cut, or `None` when I hold none there (`Damaged`): then every leaf is fetched and nothing is
    /// named absent, and the replica imports the whole state from empty (S2 v1.2).
    my_root: Option<Hash>,
    sources: Vec<MemberKey>,
    src: usize,
    /// A request is ready but held back: the source would refuse it (pacing, 4.2 step 2). Not a silent source.
    due: bool,
    /// The source answered `Busy`: not asked again before this.
    retry_at: HostTime,
    /// `Busy` answers in a row from this source.
    busy: u32,
    sent_at: HostTime,
    started: HostTime,
    descents: u32,
    phase: Phase,
    /// My tree at the cut passed its full rehash: hashes can be compared. If not, every leaf is fetched.
    trusted: bool,
    /// Object and table leaves to fetch records for.
    leaves: Vec<LeafKey>,
    absent: BTreeSet<LeafKey>,
    physics: Option<DomainId>,
    records: Vec<Record>,
    /// The inline blobs the sources sent with the records, by hash: once each, in the order `CutImport.blobs` takes.
    blobs: BTreeMap<Hash, Bytes>,
    image: Option<PhysicsImage>,
    bytes: u64,
}

impl Repair {
    fn at(&self) -> CutRef {
        self.cut.at
    }
    fn first_phase(&self) -> Phase {
        Phase::Nodes { asked: Vec::new(), queued: vec![(TreePath::World, self.target)] }
    }
}

struct Episode {
    cut: Cut,
    /// The newest standings agreement reported for the cut: a newer `RootSplit` supersedes the older (S3 v1.3).
    roots: Vec<(Hash, Vec<(MemberKey, SelfTest)>)>,
    /// Members any of whose verdicts for this split showed `Failed`: out for the whole episode (the latch).
    failed: BTreeSet<MemberKey>,
    /// The reference, once a pick was allowed; it changes only if the latch does.
    reference: Option<MemberKey>,
    picked: bool,
    /// The split persisted past step 1: its bundle is written once.
    reported: bool,
}

/// A byte bucket, held in byte-nanoseconds so that no part of a byte earned is lost (4.2, step 2).
#[derive(Clone, Copy, Debug)]
struct Bucket {
    units: i128,
    last: HostTime,
}

const NS: i128 = 1_000_000_000;

impl Bucket {
    fn full(burst: u64, now: HostTime) -> Bucket {
        Bucket { units: i128::from(burst) * NS, last: now }
    }
    fn refill(&mut self, burst: u64, rate: u64, now: HostTime) {
        if now.0 > self.last.0 {
            let dt = i128::from(now.0 - self.last.0);
            self.units = (self.units + dt * i128::from(rate)).min(i128::from(burst) * NS);
            self.last = now;
        }
    }
    /// Nanoseconds until the bucket is above zero.
    fn wait_ns(&self, rate: u64) -> i128 {
        if self.units > 0 { 0 } else { (1 - self.units + i128::from(rate.max(1)) - 1) / i128::from(rate.max(1)) }
    }
    fn take(&mut self, bytes: usize) {
        self.units -= bytes as i128 * NS;
    }
}

/// Whether to serve a request.
enum Serve {
    Yes,
    NotHeld,
    Busy(u64),
}

/// What makes two failed repairs the same (4.3): the rung, the reason, and the differing leaves found (`Config`
/// among them when the world's config differs).
#[derive(Clone, Debug, PartialEq, Eq)]
struct FailKey {
    rung: u8,
    why: String,
    leaves: Vec<LeafKey>,
}

impl FailKey {
    /// The mismatch, named for the bundle and the user: at most 8 leaves listed.
    fn name(&self) -> String {
        let mut s = format!("rung {}: {}; differing: ", self.rung, self.why);
        if self.leaves.is_empty() {
            s.push_str("none found");
        }
        // The config first: when it differs, it is most likely the cause.
        let mut leaves: Vec<&LeafKey> = self.leaves.iter().filter(|k| **k == LeafKey::Config).collect();
        leaves.extend(self.leaves.iter().filter(|k| **k != LeafKey::Config));
        for (i, k) in leaves.into_iter().take(8).enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&match k {
                LeafKey::Config => "Config (the world's config differs)".to_string(),
                LeafKey::Object(o) => format!("Object({})", o.to_u128()),
                LeafKey::Table(t) => format!("Table({t})"),
                LeafKey::Physics(d) => format!("Physics({d:?})"),
            });
        }
        if self.leaves.len() > 8 {
            s.push_str(&format!(" and {} more", self.leaves.len() - 8));
        }
        s
    }
}

/// A bundle's replay evidence (6): `Cv`'s state, encoded ([`CvState`]), and the entries from `Cv`'s frame up to
/// `upto`, each [`pe_seams::SignedEntry::encode`]d.
struct Evidence {
    /// The `Cv` it was captured at: its cut and root, so a `Cv` replaced at the same frame never reuses it.
    key: (CutRef, Hash),
    state: Bytes,
    entries: Vec<Bytes>,
    upto: Frame,
    /// The cut the entries were last extended to, and its heads, a bundle's `cut_heads`. The evidence goes only to a
    /// bundle whose `cut` is that cut (@shader-reviewer on 18198b28 and 076bb902: the argument that they always
    /// match, made a check).
    to: Option<(CutRef, Vec<ChainHead>)>,
}

/// A bundle's replay evidence: `state_at_cv`, `entries`, `cut_heads`.
type EvidenceParts = (Bytes, Vec<Bytes>, Vec<ChainHead>);

impl Evidence {
    /// The state, entries and heads for a bundle at `cut`, all three together, if the evidence was last extended to
    /// `cut`; otherwise none of them, since entries extended to another cut would replay to a wrong root at `cut`.
    fn for_cut(&self, cut: CutRef) -> Option<EvidenceParts> {
        match &self.to {
            Some((at, heads)) if *at == cut => Some((self.state.clone(), self.entries.clone(), heads.clone())),
            _ => None,
        }
    }
}

/// A join from a final cut (divergence.md 4.5, app.md 4): where this machine's whole-state fetch of `E` stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Join {
    /// Fetching and importing `E`'s state.
    Joining { cut: CutRef },
    /// The replica holds `E`'s certified state and finalize agreed there (Cv is `E`): the embedding may start the
    /// driver.
    Joined { cut: CutRef },
    /// No source served it, or the replica could not take it; the embedding gives up or joins again at a newer cut.
    Failed { cut: CutRef, why: String },
}

/// The ladder stopped (4.3).
struct Stuck {
    /// Stopped by `n_stuck_any` (keys kept changing) or by a rate mismatch: only a cure starts it again. Otherwise a
    /// probe failing with a different key does.
    any: bool,
    key: FailKey,
    /// The next probe repair; set at the first final cut after the stop.
    probe_at: Option<HostTime>,
}

pub struct Diverge {
    cfg: Config,
    me: MemberKey,
    cv: Option<(CutRef, Hash)>,
    /// `Cv`'s cut with its heads, for the replay evidence.
    cv_cut: Option<Cut>,
    /// The replay evidence for the bundles of this divergence (6), captured once per `Cv`, and kept past the cure
    /// that moves `Cv`, whose bundle carries it.
    evidence: Option<Evidence>,
    /// The `Cv` whose capture failed: not tried again, and counted once (`evidence_skipped`).
    evidence_failed: Option<(CutRef, Hash)>,
    replay: Option<Replay>,
    repair: Option<Repair>,
    pending: Option<FinalCut>,
    /// The newest final cut I have heard of (`on_cut_final`, `on_final`), which may be ahead of the one whose entries I
    /// hold: a split's pick waits until they are the same.
    final_seen: Option<CutRef>,
    episode: Option<Episode>,
    recur: BTreeMap<LeafKey, Vec<HostTime>>,
    source_offset: usize,
    /// The differing leaves of the rung-2 repair just dropped, for its failure's key.
    last_leaves: Vec<LeafKey>,
    /// Refused rung-2 repairs (`refused`) since the last cure: in a row with one key, and with any key; the keys seen
    /// (at most 16).
    fail_same: Option<(FailKey, u32)>,
    fails_any: u32,
    keys_seen: Vec<FailKey>,
    stuck: Option<Stuck>,
    /// The join from a final cut, if one was asked ([`Diverge::join`]), and when it gives up (`t_join`).
    join: Option<Join>,
    join_deadline: Option<HostTime>,
    /// Roots I serve: the agreed roots of recent final cuts, and my root at a cut in a root split.
    servable: BTreeMap<Hash, CutRef>,
    buckets: BTreeMap<MemberKey, Bucket>,
    /// My copy of each source's bucket for me: requests are paced so that an honest repair is never refused.
    ask_buckets: BTreeMap<MemberKey, Bucket>,
    /// When each peer was last served each root's physics image.
    physics_served: BTreeMap<(MemberKey, Hash), HostTime>,
    counters: Counters,
    events: Vec<DivergeEvent>,
    /// Plain information for the log that is no seam event (`notes`).
    notes: Vec<String>,
    actions: Vec<Action>,
    out: Vec<Outgoing>,
    #[cfg(feature = "mutants")]
    pub mutant: Option<Mutant>,
}

impl Diverge {
    pub fn new(me: MemberKey, cfg: Config) -> Self {
        Diverge {
            cfg,
            me,
            cv: None,
            cv_cut: None,
            evidence: None,
            evidence_failed: None,
            replay: None,
            repair: None,
            pending: None,
            final_seen: None,
            episode: None,
            recur: BTreeMap::new(),
            source_offset: 0,
            last_leaves: Vec::new(),
            fail_same: None,
            fails_any: 0,
            keys_seen: Vec::new(),
            stuck: None,
            join: None,
            join_deadline: None,
            servable: BTreeMap::new(),
            buckets: BTreeMap::new(),
            ask_buckets: BTreeMap::new(),
            physics_served: BTreeMap::new(),
            counters: Counters::default(),
            events: Vec::new(),
            notes: Vec::new(),
            actions: Vec::new(),
            out: Vec::new(),
            #[cfg(feature = "mutants")]
            mutant: None,
        }
    }

    fn mutated(&self, m: Mutant) -> bool {
        #[cfg(feature = "mutants")]
        return self.mutant == Some(m);
        #[cfg(not(feature = "mutants"))]
        {
            let _ = m;
            false
        }
    }

    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// The newest final cut whose root this machine matched.
    pub fn verified_cut(&self) -> Option<(CutRef, Hash)> {
        self.cv
    }

    /// Where a join asked with [`Diverge::join`] stands; `None` if none was.
    pub fn join_state(&self) -> Option<&Join> {
        self.join.as_ref()
    }

    /// Whether the rung-2 fetch under way is a join's.
    fn joining(&self) -> bool {
        matches!(self.join, Some(Join::Joining { .. }))
    }

    /// Joining at a final cut `e` (divergence.md 4.5): fetch `e`'s whole state, as rung 2 does for a `Damaged` cut,
    /// from `inviter` first and then `e`'s other voters, and import it into the empty replica. Everything is verified
    /// against `e`'s certified root, whichever peer serves it. This is no divergence: no evidence is captured, no
    /// bundle is written, no episode opens, no trace event is emitted, the ladder's stop rule (4.3) never counts it,
    /// and only `joins` / `join_failures` count it. [`Diverge::join_state`] says when it is done.
    /// Until then the embedding runs no driver: it calls only [`Diverge::tick`] and [`Diverge::receive`], never
    /// `on_final`. [`Diverge::is_repairing`] is true until the import ends (not while a `NotReady` finalize waits), so
    /// read [`Diverge::join_state`]; the replica publishes no view, so no health shows the join.
    pub fn join(&mut self, e: &FinalCut, inviter: MemberKey, replica: &mut dyn Replica, now: HostTime) {
        self.join = Some(Join::Joining { cut: e.cut.at });
        self.join_deadline = Some(now.plus(self.cfg.t_join));
        // The joiner holds nothing verified: Cv starts at E, once finalize agrees there.
        self.cv = None;
        self.pending = None;
        let mut sources = vec![inviter];
        sources.extend(e.voters.iter().map(|v| v.0).filter(|v| *v != inviter));
        self.start_rung2(e.cut.clone(), Some(e.clone()), e.root, sources, None, replica, now);
    }

    pub fn is_repairing(&self) -> bool {
        self.repair.is_some() || self.replay.is_some()
    }

    /// The frame an open episode is about, for the health the embedding reports (divergence.md 7): the earliest of a
    /// root split's cut and the cut a replay or repair runs toward. `None` while nothing is open. A split stays open
    /// until a cut at or past it agrees, even while no rung runs (this machine is the reference, or waits to pick).
    pub fn open_since(&self) -> Option<Frame> {
        let split = self.episode.as_ref().map(|e| e.cut.at.frame);
        let busy = self.busy().map(|(at, _, _)| at.frame);
        match (split, busy) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The cut of the replay or repair under way, and whether it is final.
    fn busy(&self) -> Option<(CutRef, bool, u8)> {
        if let Some(r) = &self.replay {
            return Some((r.cut.at, r.final_cut.is_some(), 1));
        }
        self.repair.as_ref().map(|r| (r.at(), r.final_cut.is_some(), 2))
    }

    /// The reference member of the current root-split episode, once chosen.
    pub fn split_reference(&self) -> Option<MemberKey> {
        self.episode.as_ref().and_then(|e| e.reference)
    }

    /// Trace events, for the harness and pe-app's log.
    pub fn events(&mut self) -> Vec<DivergeEvent> {
        std::mem::take(&mut self.events)
    }

    /// Plain information for the log, beside [`Self::events`]: what happened but is neither a seam event nor a
    /// failure, such as a repair a root split overtook (5).
    pub fn notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// What the driver must do.
    pub fn actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }

    // ---- inputs ------------------------------------------------------------------------------------------------

    /// S3 reported a cut final (`AgreeEvent::CutFinal`), perhaps before this member holds its entries. The driver
    /// passes every one as it arrives: a root split's pick waits until `EntrySource::final_cut` names the newest one
    /// (divergence.md 5, step 3).
    pub fn on_cut_final(&mut self, cut: CutRef) {
        if self.final_seen.is_none_or(|f| (cut.frame, cut.height) > (f.frame, f.height)) {
            self.final_seen = Some(cut);
        }
    }

    /// A cut became final; `finalized` is what [`Replica::finalize`] found for it.
    pub fn on_final(
        &mut self,
        fc: &FinalCut,
        finalized: Finalized,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        self.on_cut_final(fc.cut.at);
        self.note_servable(fc.root, fc.cut.at, clock);
        if self.cv.is_some_and(|(cv, _)| fc.cut.at.frame <= cv.frame) {
            // At or below my Cv: nothing to compare, and never a reason to move Cv back (@shader-reviewer, M2).
            return;
        }
        let mine = match finalized {
            // Not re-simulated to the cut yet: the driver asks again on a later tick. A cut this ladder finalizes
            // itself after a repair is kept, and asked again once nothing is under way (`tick`).
            Finalized::NotReady => {
                if self.pending.as_ref().is_none_or(|p| p.cut.at.frame <= fc.cut.at.frame) {
                    self.pending = Some(fc.clone());
                }
                return;
            }
            Finalized::Agreed => {
                self.counters.cuts_compared += 1;
                self.counters.cuts_matched += 1;
                self.set_cv(&fc.cut, fc.root);
                self.reset_stuck();
                // A later cut agreed: any replay or repair toward an older one, or toward a split's reference, is moot.
                // A replay or import the replica was still finishing (`Pending`) is what made it agree: it counts as
                // that rung's cure.
                if let Some(rp) = self.replay.take()
                    && rp.final_cut.is_some()
                {
                    self.counters.cured_by_rung[0] += 1;
                    self.counters.repair_latency_nanos.push(0);
                    self.events.push(DivergeEvent::Repaired { cut: rp.cut.at, rung: Rung(1) });
                }
                if self.joining() {
                    // A join's finalize at E, asked again after `NotReady`: no cure, the join is done.
                    self.repair = None;
                    if matches!(self.join, Some(Join::Joining { cut }) if cut == fc.cut.at) {
                        self.joined(fc.cut.at);
                    }
                } else if let Some(mut rep) = self.repair.take()
                    && let Phase::Importing { mine, .. } = std::mem::replace(&mut rep.phase, Phase::Physics)
                {
                    self.count_cure(rep, mine, now);
                }
                // Whatever evidence is left was for an older `Cv`, and no bundle will take it: up to
                // `evidence_max_bytes` is not kept resident (@shader-reviewer, Low).
                self.evidence = None;
                self.pending = None;
                if let Some(ep) = self.episode.take() {
                    self.counters.root_splits_ended += 1;
                    self.events.push(DivergeEvent::RootSplitEnded { cut: ep.cut.at });
                }
                return;
            }
            Finalized::Diverged { mine } => {
                self.counters.cuts_compared += 1;
                Some(mine)
            }
            // I cannot take my root at the cut, and waiting will not help (a frame from my floor up to it is no
            // longer kept, S3 v1.2): rung 2 at once, importing the whole state, since there is no root of mine to
            // diff against (pe-seams RULINGS, S2 v1.2 round 3).
            Finalized::Damaged => None,
        };
        if let Some((at, is_final, rung)) = self.busy() {
            let lag = fc.cut.at.frame.0.saturating_sub(at.frame.0);
            if !is_final || lag > frames_ceil(self.cfg.w_fg, clock.rate_hz()) {
                // A split's replay or repair is moot once a cut is final; a repair the data came too late for
                // restarts at the newest cut (4.2).
                self.replay = None;
                self.repair = None;
                self.pending = None;
                if is_final {
                    // The data came too late, again and again for a world too big to fetch within `w_fg` (4.2).
                    self.counters.overtaken_in_a_row += 1;
                }
                self.fail(rung, at, "overtaken by a newer cut");
            } else {
                self.pending = Some(fc.clone());
                return;
            }
        }
        if let Some(st) = &mut self.stuck {
            // The ladder stopped (4.3): the cut is compared, and no rung runs, but for one probe every `t_stuck_probe`.
            match st.probe_at {
                Some(t) if now >= t => st.probe_at = Some(now.plus(self.cfg.t_stuck_probe)),
                _ => {
                    st.probe_at.get_or_insert(now.plus(self.cfg.t_stuck_probe));
                    self.counters.stuck_skipped += 1;
                    return;
                }
            }
        }
        match mine {
            Some(mine) => self.diverged(fc, mine, replica, source, now, clock),
            None => {
                self.counters.damaged += 1;
                self.capture(replica, source, &fc.cut, clock);
                let voters = fc.voters.iter().map(|v| v.0).collect();
                self.start_rung2(fc.cut.clone(), Some(fc.clone()), fc.root, voters, None, replica, now);
            }
        }
    }

    /// Votes for `cut` split so that no root can reach a quorum (S3 `RootSplit`).
    pub fn on_root_split(
        &mut self,
        cut: &Cut,
        roots: &[(Hash, Vec<(MemberKey, SelfTest)>)],
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        if self.episode.as_ref().is_none_or(|e| e.cut.at != cut.at) {
            // Step 1: replay from Cv (which moves neither Cv nor the floor, S2 v1), vote again with the root it gives,
            // and run the self-test, once per episode: its verdict is final for the split (S3 v1.3). Transient faults
            // end here.
            let mut failed = BTreeSet::new();
            split::latch(&cut.at, roots, &mut failed);
            self.episode = Some(Episode {
                cut: cut.clone(),
                roots: roots.to_vec(),
                failed,
                reference: None,
                picked: false,
                reported: false,
            });
            self.counters.root_splits += 1;
            self.events.push(DivergeEvent::RootSplitStarted { cut: cut.at });
            self.actions.push(Action::RunSelfTest { cut: cut.at });
            // A replay or repair toward an older cut is overtaken: this cut is newer, and the split is what matters.
            // At n = 2 that is the usual course: a split ends its view at once (agreement.md L4'), the height is
            // proposed again, and the member still repairing votes its old root there. Not a failure: the work goes
            // on at this cut, and a note keeps the story in the log. A join's import still fails the join (4.5).
            if let Some((at, _, rung)) = self.busy() {
                self.replay = None;
                self.repair = None;
                if self.joining() {
                    self.fail(rung, at, "overtaken by a root split");
                } else {
                    self.counters.split_overtook += 1;
                    let again = if at.height == cut.at.height { " (its height proposed again)" } else { "" };
                    self.notes.push(format!(
                        "rung {rung} at {at:?} overtaken by the root split at {:?}{again}; continuing there",
                        cut.at
                    ));
                }
            }
            self.replay = Some(Replay { cut: cut.clone(), final_cut: None, mine: None });
            self.poll_replay(replica, source, now, clock);
            return;
        }
        // The same split again, with newer standings: they supersede the old, and the latch only grows.
        let ep = self.episode.as_mut().expect("checked above");
        ep.roots = roots.to_vec();
        split::latch(&cut.at, roots, &mut ep.failed);
        self.try_pick(replica, source, now);
    }

    /// Steps 2-4, once step 1's replay is done: choose the reference when every member can choose the same one (5,
    /// step 3), and repair toward it. Asked again on each new standing and each tick until a pick is allowed.
    fn try_pick(&mut self, replica: &mut dyn Replica, source: &mut dyn EntrySource, now: HostTime) {
        if self.replay.is_some() {
            // Step 1's replay is still running (`Pending`): the revote follows it.
            return;
        }
        let final_now = source.final_cut().map(|f| f.cut.at);
        let Some(ep) = self.episode.as_mut() else { return };
        let reference =
            match split::pick(&ep.cut.at, final_now.as_ref(), self.final_seen.as_ref(), &ep.roots, &ep.failed) {
                split::Pick::Wait(_) => return,
                split::Pick::Ready(r) => r,
            };
        let changed = ep.picked && ep.reference != reference;
        ep.picked = true;
        ep.reference = reference;
        let (cut, roots) = (ep.cut.clone(), ep.roots.clone());
        let first_report = !ep.reported;
        ep.reported = true;
        let mine = replica.root_at_cut(&cut, source).ok();
        if first_report {
            // Written only once the split persisted past step 1 (@design-reviewer, Low 1).
            let voters = roots.iter().flat_map(|r| r.1.iter().map(|v| v.0)).collect();
            self.bundle(FaultBundle {
                cv: self.cv,
                cut: cut.at,
                my_root: mine.unwrap_or(Hash::ZERO),
                voters,
                ..FaultBundle::default()
            });
        }
        if changed && let Some(rep) = self.repair.take() {
            // The reference moved: a latch grew (an equivocating member), or, from n = 4, a voter missing when the
            // split was declared voted later (5, step 4; a known M1 gap). The old target is not repaired to.
            self.fail(2, rep.at(), "the split's reference changed");
        }
        let Some(reference) = reference else { return };
        let Some((target, holders)) = roots.iter().find(|(_, v)| v.iter().any(|x| x.0 == reference)) else { return };
        if reference == self.me || self.repair.is_some() || mine == Some(*target) {
            return;
        }
        let mut sources = vec![reference];
        sources.extend(holders.iter().map(|x| x.0).filter(|m| *m != reference));
        self.start_rung2(cut, None, *target, sources, mine, replica, now);
    }

    /// Time out a silent source, and hand over what is to be sent.
    pub fn tick(
        &mut self,
        now: HostTime,
        clock: &dyn FrameClock,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
    ) -> Vec<Outgoing> {
        // Work the replica runs over several ticks (`Pending`, S2 v1.1) is asked again with the same input.
        if self.replay.is_some() {
            self.poll_replay(replica, source, now, clock);
        }
        if matches!(self.repair.as_ref().map(|r| &r.phase), Some(Phase::Importing { .. })) {
            self.try_import(replica, source, now, clock);
        }
        if self.repair.as_ref().is_some_and(|r| r.due) {
            self.ask(now);
        }
        if self.episode.as_ref().is_some_and(|e| !e.picked) {
            self.try_pick(replica, source, now);
        }
        // A join past its deadline ends (4.5); one waits longer for its source than a repair does.
        if self.joining() && self.join_deadline.is_some_and(|d| now > d) && self.repair.is_some() {
            let cut = self.repair.as_ref().map(Repair::at).expect("checked");
            self.repair = None;
            self.fail(2, cut, "no source served the join before its deadline");
        }
        let wait = if self.joining() { self.cfg.t_join_peer } else { self.cfg.t_peer };
        let waiting_on_peer =
            |r: &Repair| !r.due && !matches!(r.phase, Phase::Importing { .. }) && now > r.sent_at.plus(wait);
        if self.repair.as_ref().is_some_and(waiting_on_peer) {
            self.source_fault(replica, source, now, clock);
        }
        // A final cut kept for later (it arrived during a repair, or was not ready): take it up once nothing runs.
        if self.busy().is_none() && self.pending.is_some() {
            self.after_repair(replica, source, now, clock);
        }
        std::mem::take(&mut self.out)
    }

    /// A message on the repair lane.
    pub fn receive(
        &mut self,
        from: PeerId,
        bytes: &[u8],
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) -> Result<(), Fault> {
        let msg = match Msg::decode(bytes) {
            Ok(m) => m,
            Err(e) => {
                self.counters.bad_messages += 1;
                return Err(Fault { from, author: None, kind: FaultKind::Malformed, detail: format!("repair: {e:?}") });
            }
        };
        match msg {
            Msg::NodeRequest { cut, root, paths } => {
                if !self.may_serve(from, cut, root, now) {
                    return Ok(());
                }
                let nodes: Option<Vec<(TreePath, Node)>> =
                    replica.tree(&root).map(|t| paths.into_iter().filter_map(|p| t.node(&p).map(|n| (p, n))).collect());
                match nodes {
                    Some(nodes) => {
                        let budget = self.cfg.reply_budget;
                        for nodes in split_by_bytes(nodes, budget, |(p, n)| path_size(p) + node_size(n)) {
                            self.serve(from, Msg::NodeReply { cut, root, nodes });
                        }
                    }
                    None => self.send(from, Msg::NotHeld { cut, root }),
                }
            }
            Msg::RecordRequest { cut, root, keys } => {
                if !self.may_serve(from, cut, root, now) {
                    return Ok(());
                }
                // Each reply carries the inline blobs its own records' rows reference (S2 v1.8), so a reply split by
                // bytes stays whole on its own. The blobs count toward the reply's bytes and toward MAX_ITEMS: a part
                // whose blobs take it past either is halved again (a single record is sent as it is).
                let budget = self.cfg.reply_budget;
                let replies = replica.tree(&root).map(|t| {
                    let mut out = Vec::new();
                    let mut todo: Vec<Vec<Record>> = split_by_bytes(t.export(&keys), budget, record_size);
                    todo.reverse();
                    while let Some(mut records) = todo.pop() {
                        let blobs = t.export_blobs(&records.iter().map(|r| r.key).collect::<Vec<_>>());
                        let bytes = records.iter().map(record_size).sum::<usize>()
                            + blobs.iter().map(|b| blob_size(b)).sum::<usize>();
                        if records.len() > 1 && (blobs.len() > MAX_ITEMS || bytes > budget) {
                            let back = records.split_off(records.len() / 2);
                            todo.push(back);
                            todo.push(records);
                        } else {
                            out.push((records, blobs));
                        }
                    }
                    out
                });
                match replies {
                    Some(replies) => {
                        for (records, blobs) in replies {
                            self.serve(from, Msg::RecordReply { cut, root, records, blobs });
                        }
                    }
                    None => self.send(from, Msg::NotHeld { cut, root }),
                }
            }
            Msg::PhysicsRequest { cut, root } => {
                if !self.may_serve(from, cut, root, now) {
                    return Ok(());
                }
                // One image per peer and root, and again only after `t_join_peer`: a requester that asks sooner is
                // flooding. A join that timed out a slow image and asked again past that is served (4.5).
                let again = self.cfg.t_join_peer;
                if self.physics_served.get(&(from, root)).is_some_and(|t| now < t.plus(again)) {
                    self.counters.serve_refused += 1;
                    self.send(from, Msg::NotHeld { cut, root });
                    return Ok(());
                }
                self.physics_served.insert((from, root), now);
                match replica.physics_image(&root) {
                    Some(image) => self.serve(from, Msg::PhysicsReply { cut, root, image }),
                    None => self.send(from, Msg::NotHeld { cut, root }),
                }
            }
            reply => {
                let (cut, root) = reply.head();
                let expected =
                    self.repair.as_ref().is_some_and(|r| r.sources[r.src] == from && r.at() == cut && r.target == root);
                if !expected {
                    self.counters.stale_replies += 1;
                    return Ok(());
                }
                if !matches!(reply, Msg::Busy { .. } | Msg::NotHeld { .. }) {
                    // Pacing: what the source took from its bucket for me, I take from my copy of it.
                    let (burst, _) = self.ask_budget();
                    self.ask_buckets.entry(from).or_insert_with(|| Bucket::full(burst, now)).take(bytes.len());
                }
                match reply {
                    Msg::Busy { retry_after_us, .. } => self.on_busy(retry_after_us, replica, source, now, clock),
                    Msg::NodeReply { nodes, .. } => self.on_nodes(nodes, replica, source, now, clock),
                    Msg::RecordReply { records, blobs, .. } => {
                        self.on_records(records, blobs, replica, source, now, clock)
                    }
                    Msg::PhysicsReply { image, .. } => self.on_physics(image, replica, source, now, clock),
                    _ => self.source_fault(replica, source, now, clock),
                }
            }
        }
        Ok(())
    }

    // ---- serving -------------------------------------------------------------------------------------------------

    /// Remember an agreed root as servable, and forget those older than `T_keep` (keeping at least two).
    fn note_servable(&mut self, root: Hash, cut: CutRef, clock: &dyn FrameClock) {
        self.servable.insert(root, cut);
        let keep = frames_ceil(self.cfg.t_keep, clock.rate_hz());
        let newest = self.servable.values().map(|c| c.frame.0).max().unwrap_or(0);
        while self.servable.len() > 2 {
            let Some((&oldest, c)) = self.servable.iter().min_by_key(|(_, c)| c.frame.0) else { break };
            if newest.saturating_sub(c.frame.0) <= keep {
                break;
            }
            self.servable.remove(&oldest);
        }
        let servable = &self.servable;
        self.physics_served.retain(|(_, r), _| servable.contains_key(r));
    }

    /// Whether to answer `peer` a request for `root`: only servable roots, and only while the peer's byte bucket is
    /// above zero (the plan's "a flooding member's excess is refused"). Otherwise the answer is sent here: `NotHeld`
    /// for a root not served, `Busy` with the wait for a peer over its budget. A refusal is counted.
    fn may_serve(&mut self, peer: MemberKey, cut: CutRef, root: Hash, now: HostTime) -> bool {
        let answer = if !self.servable.contains_key(&root) {
            Serve::NotHeld
        } else {
            let (burst, rate) = (self.cfg.serve_burst_bytes, self.cfg.serve_rate);
            let b = self.buckets.entry(peer).or_insert_with(|| Bucket::full(burst, now));
            b.refill(burst, rate, now);
            match b.wait_ns(rate) {
                0 => Serve::Yes,
                ns => Serve::Busy(u64::try_from((ns + 999) / 1000).unwrap_or(u64::MAX)),
            }
        };
        match answer {
            Serve::Yes => return true,
            Serve::NotHeld => self.send(peer, Msg::NotHeld { cut, root }),
            Serve::Busy(retry_after_us) => self.send(peer, Msg::Busy { cut, root, retry_after_us }),
        }
        self.counters.serve_refused += 1;
        false
    }

    /// Send a reply that serves a request, and take its bytes from the peer's bucket.
    fn serve(&mut self, to: PeerId, msg: Msg) {
        let n = self.out.len();
        self.send(to, msg);
        if let Some(o) = self.out.get(n) {
            let len = o.bytes.len();
            if let Some(b) = self.buckets.get_mut(&to) {
                b.take(len);
            }
        }
    }

    /// My copy of a source's bucket: half its burst, refilled a quarter slower, a margin for the two clocks and the
    /// network's jitter.
    fn ask_budget(&self) -> (u64, u64) {
        (self.cfg.serve_burst_bytes / 2, self.cfg.serve_rate / 4 * 3)
    }

    fn send(&mut self, to: PeerId, msg: Msg) {
        match msg.encode() {
            Ok(bytes) => self.out.push(Outgoing { to, lane: Lane::Repair, bytes }),
            Err(_) => self.counters.encode_refused += 1,
        }
    }

    // ---- detection and rung 1 ------------------------------------------------------------------------------------

    fn diverged(
        &mut self,
        fc: &FinalCut,
        mine: Hash,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        let cut = fc.cut.at;
        self.counters.diverged += 1;
        self.events.push(DivergeEvent::Diverged { cut, mine, agreed: fc.root });
        // Rung 1 (4.1): the replica restores Cv, verifies it, replays to the cut and on to the present, perhaps over
        // several ticks.
        self.events.push(DivergeEvent::RungStarted { cut, rung: Rung(1) });
        self.replay = Some(Replay { cut: fc.cut.clone(), final_cut: Some(fc.clone()), mine: Some(mine) });
        self.poll_replay(replica, source, now, clock);
    }

    /// Ask the replica for the replay under way; act once it answers anything but `Pending`.
    fn poll_replay(
        &mut self,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        let Some(rp) = &self.replay else { return };
        let result = replica.replay_to(&rp.cut, source);
        if result == Err(RootError::Pending) {
            return;
        }
        let rp = self.replay.take().expect("checked above");
        match rp.final_cut {
            Some(fc) => self.rung1_done(&fc, rp.mine, result, replica, source, now, clock),
            None => {
                // A root split's step 1: vote again with the replayed root, or with my root at the cut if the replay
                // could not run. Nothing moves Cv here; the cut is not final.
                // Either way my root at the cut is asked of the replica: a replay forgets its votes, and a reference
                // must still hold the root it serves at the split's cut (4.1) when a repair toward it asks.
                let held = replica.root_at_cut(&rp.cut, source).ok();
                let root = result.ok().or(held);
                if let Some(root) = root {
                    self.servable.insert(root, rp.cut.at);
                    self.actions.push(Action::Revote { cut: rp.cut.at, root });
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rung1_done(
        &mut self,
        fc: &FinalCut,
        mine: Option<Hash>,
        result: Result<Hash, RootError>,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        let cut = fc.cut.at;
        let mine = match result {
            Ok(root) if root == fc.root => {
                // Only finalize moves Cv (S2 v1): the replayed root matched, so the cut is agreed now. If a newer final
                // cut is pending, the replica's floor is already past this one: it is not finalized again, and the
                // newer cut moves Cv next (@shader-reviewer, re-review L1).
                let newer = self.newer_pending(cut);
                let f = (!newer).then(|| replica.finalize(fc, source));
                if newer || matches!(f, Some(Finalized::Agreed)) {
                    self.reset_stuck();
                    self.counters.cured_by_rung[0] += 1;
                    self.counters.repair_latency_nanos.push(0);
                    self.events.push(DivergeEvent::Repaired { cut, rung: Rung(1) });
                    if !newer {
                        self.set_cv(&fc.cut, fc.root);
                        self.evidence = None; // a rung-1 cure writes no bundle
                    }
                    self.after_repair(replica, source, now, clock);
                    return;
                }
                self.fail(1, cut, "the replay agreed, but finalize did not");
                Some(root)
            }
            Ok(root) => {
                self.fail(1, cut, "the replay from Cv gives a wrong root again");
                Some(root)
            }
            Err(RootError::Damaged) => {
                self.fail(1, cut, "the saved state at Cv is damaged, or a frame up to the cut is no longer kept");
                mine
            }
            Err(RootError::NotKept) => {
                self.fail(1, cut, "the entries from Cv are no longer kept");
                mine
            }
            Err(_) => {
                self.fail(1, cut, "the replica could not replay to the cut");
                mine
            }
        };
        self.capture(replica, source, &fc.cut, clock);
        let voters = fc.voters.iter().map(|v| v.0).collect();
        self.start_rung2(fc.cut.clone(), Some(fc.clone()), fc.root, voters, mine, replica, now);
    }

    /// The replay evidence captured at the current `Cv`, if any.
    fn evidence_at_cv(&self) -> Option<&Evidence> {
        self.evidence.as_ref().filter(|e| Some(e.key) == self.cv)
    }

    /// A bundle's evidence from [`Evidence::for_cut`]: evidence extended to another cut than the bundle's is left out
    /// and counted (`evidence_skipped`), so a broken argument shows instead of giving a quietly wrong replay.
    fn parts(&mut self, found: Option<Option<EvidenceParts>>) -> EvidenceParts {
        match found {
            Some(Some(parts)) => parts,
            Some(None) => {
                self.counters.evidence_skipped += 1;
                EvidenceParts::default()
            }
            None => EvidenceParts::default(),
        }
    }

    /// `Cv` moved to an agreed cut.
    fn set_cv(&mut self, cut: &Cut, root: Hash) {
        self.cv = Some((cut.at, root));
        self.cv_cut = Some(cut.clone());
        // The evidence stays for one step: a rung-2 cure's `finalize` moves `Cv` just before `count_cure` writes its
        // bundle from the old `Cv`'s evidence and takes it. The other paths that move `Cv` drop it.
    }

    /// Save the replay evidence (6) before rung 2 runs toward a final cut at `upto`: `Cv`'s whole state, once per
    /// `Cv`, and the entries from `Cv` up to `upto`, extended when a newer cut is repaired toward. Left out, and
    /// counted, when `Cv`'s state or an entry frame is no longer kept, or the state is over `evidence_max_bytes`.
    fn capture(&mut self, replica: &dyn Replica, source: &dyn EntrySource, to: &Cut, clock: &dyn FrameClock) {
        let upto = to.at.frame;
        let (Some(cut), Some((_, root))) = (&self.cv_cut, self.cv) else {
            return;
        };
        let key = (cut.at, root);
        if self.evidence_failed == Some(key) || self.evidence.as_ref().is_some_and(|e| e.key == key && e.upto >= upto) {
            return;
        }
        let max = self.cfg.evidence_max_bytes;
        let ev = match self.evidence.take() {
            Some(ev) if ev.key == key => Some(ev),
            _ => replica.tree(&root).and_then(|tree| {
                let keys = all_keys(&*tree)?;
                let records = tree.export(&keys);
                (records.len() == keys.len()).then_some(())?;
                let physics = replica.physics_image(&root);
                // Over the bound: bail before encoding, from the sizes in hand (@design-reviewer, Low 2).
                let size = records.iter().map(record_size).sum::<usize>()
                    + physics.as_ref().map_or(0, |p| p.bytes.len())
                    + cut.heads.len() * 104;
                (size <= max).then_some(())?;
                let state = CvState {
                    cv: cut.clone(),
                    root,
                    domain: replica.domain(),
                    rate_hz: clock.rate_hz(),
                    records,
                    physics,
                };
                let state = state.encode().ok().filter(|b| b.len() <= max)?;
                Some(Evidence { key, state, entries: Vec::new(), upto: cut.at.frame, to: None })
            }),
        };
        // The entries stamped from `Cv`'s frame up to the cut's: what a replay from `Cv` steps.
        let ev = ev.and_then(|mut ev| {
            for f in ev.upto.0..upto.0 {
                ev.entries.extend(source.entries_at(Frame(f))?.iter().map(|e| Bytes::from(e.encode())));
            }
            ev.upto = upto;
            ev.to = Some((to.at, to.heads.clone()));
            Some(ev)
        });
        if ev.is_none() {
            self.counters.evidence_skipped += 1;
            self.evidence_failed = Some(key);
        }
        self.evidence = ev;
    }

    // ---- rung 2 --------------------------------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn start_rung2(
        &mut self,
        cut: Cut,
        final_cut: Option<FinalCut>,
        target: Hash,
        voters: Vec<MemberKey>,
        my_root: Option<Hash>,
        replica: &mut dyn Replica,
        now: HostTime,
    ) {
        if !self.joining() {
            self.events.push(DivergeEvent::RungStarted { cut: cut.at, rung: Rung(2) });
        }
        // Diff against an honest tree: rehash mine first; a stale cache would hide damage (4.2, step 2).
        // No node cap: a node is taken only if asked for, once, and hashing to what its checked parent named, so the
        // descent never takes more nodes than the target tree has (@design-reviewer, re-review Medium).
        let trusted = match my_root.and_then(|r| replica.tree(&r)) {
            Some(t) => self.mutated(Mutant::SkipVerify) || t.verify().is_ok(),
            None => false,
        };
        let mut sources: Vec<MemberKey> = voters.iter().copied().filter(|v| *v != self.me).collect();
        if final_cut.is_some() && !self.joining() {
            let n = sources.len().max(1);
            sources.rotate_left(self.source_offset % n);
        }
        if sources.is_empty() {
            self.fail(2, cut.at, "no other member holds the root");
            return;
        }
        let final_cut_is_some = final_cut.is_some();
        let mut rep = Repair {
            cut,
            final_cut,
            target,
            voters,
            my_root,
            sources,
            src: 0,
            due: false,
            retry_at: now,
            busy: 0,
            sent_at: now,
            started: now,
            from_cv: self.cv,
            evidence: final_cut_is_some && self.evidence.as_ref().is_some_and(|e| Some(e.key) == self.cv),
            descents: 0,
            phase: Phase::Physics,
            trusted,
            leaves: Vec::new(),
            absent: BTreeSet::new(),
            physics: None,
            records: Vec::new(),
            blobs: BTreeMap::new(),
            image: None,
            bytes: 0,
        };
        rep.phase = rep.first_phase();
        self.repair = Some(rep);
        self.ask(now);
    }

    /// Send the next request of the current phase to the current source.
    fn ask(&mut self, now: HostTime) {
        let (np, nk) = (self.cfg.node_request_paths, self.cfg.record_request_keys);
        let (burst, rate) = self.ask_budget();
        let Some(rep) = &mut self.repair else { return };
        if matches!(rep.phase, Phase::Importing { .. }) {
            // Nothing to ask a peer: the import is with the replica.
            return;
        }
        // Pace to the source's serving budget (4.2, step 2): a request it would answer `Busy` waits here, and so does
        // one it did answer `Busy`, until the wait it asked for is over.
        let to = rep.sources[rep.src];
        let b = self.ask_buckets.entry(to).or_insert_with(|| Bucket::full(burst, now));
        b.refill(burst, rate, now);
        rep.due = b.units <= 0 || now < rep.retry_at;
        if rep.due {
            return;
        }
        let (cut, root) = (rep.at(), rep.target);
        let msg = match &mut rep.phase {
            Phase::Nodes { asked, queued } => {
                let n = queued.len().min(np);
                *asked = queued.drain(..n).collect();
                Msg::NodeRequest { cut, root, paths: asked.iter().map(|a| a.0.clone()).collect() }
            }
            Phase::Records { asked, queued } => {
                let n = queued.len().min(nk);
                let keys: Vec<LeafKey> = queued.drain(..n).collect();
                *asked = keys.iter().copied().collect();
                Msg::RecordRequest { cut, root, keys }
            }
            Phase::Physics => Msg::PhysicsRequest { cut, root },
            Phase::Importing { .. } => unreachable!("returned above"),
        };
        rep.sent_at = now;
        self.send(to, msg);
    }

    /// Start the descent again from the root, at the current source. A physics image that passed the replica's check
    /// is kept for a re-descent at the same source: it is the target's, and a source serves one image per peer and
    /// root (@shader-reviewer, re-review L3).
    fn restart(&mut self, now: HostTime, keep_image: bool) {
        self.restart_at(now, now, keep_image);
    }

    /// [`Self::restart`], with the first request held back until `at`.
    fn restart_at(&mut self, now: HostTime, at: HostTime, keep_image: bool) {
        if let Some(rep) = &mut self.repair {
            rep.phase = rep.first_phase();
            rep.leaves.clear();
            rep.absent.clear();
            rep.physics = None;
            rep.records.clear();
            rep.blobs.clear();
            rep.busy = 0;
            rep.retry_at = at;
            if !keep_image {
                rep.image = None;
            }
        }
        self.ask(now);
    }

    /// A source did not hold the root, went silent, sent a node that does not hash to what its parent named, or sent
    /// records that failed their proofs: ask the next one.
    fn source_fault(
        &mut self,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        self.counters.source_faults += 1;
        self.next_source(replica, source, now, clock);
    }

    /// Move the repair to the next source, or fail the rung when none is left.
    fn next_source(
        &mut self,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        let stick = self.mutated(Mutant::StickWithSource);
        let Some(rep) = &mut self.repair else { return };
        if !stick {
            rep.src += 1;
        }
        // A join asks its sources again, in turn, until its deadline: at n = 2 the inviter is its only source, and one
        // slow reply must not end it (@engine-architect md2fa377743a4). A pause of `t_peer` comes first.
        let deadline = self.join_deadline.filter(|_| matches!(self.join, Some(Join::Joining { .. })));
        if rep.src >= rep.sources.len() && deadline.is_some_and(|d| now < d) {
            rep.src = 0;
            self.restart_at(now, now.plus(self.cfg.t_peer), false);
            return;
        }
        if rep.src >= rep.sources.len() {
            let cut = rep.at();
            self.repair = None;
            self.fail(2, cut, "no source served the repair (each was faulty or stayed busy)");
            self.after_repair(replica, source, now, clock);
            return;
        }
        self.restart(now, false);
    }

    /// The source is over its serving budget for me: ask it the same again once the wait it named is over, at most
    /// `busy_wait_max`. A source that answers `Busy` `n_busy_max` times in a row is rotated, so it cannot hold a repair.
    fn on_busy(
        &mut self,
        retry_after_us: u64,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        let max = self.cfg.n_busy_max;
        let wait_max = self.cfg.busy_wait_max;
        let Some(rep) = &mut self.repair else { return };
        match &mut rep.phase {
            Phase::Importing { .. } => {
                self.counters.stale_replies += 1;
                return;
            }
            Phase::Nodes { asked, queued } => {
                let back: Vec<_> = std::mem::take(asked);
                queued.splice(0..0, back);
            }
            Phase::Records { asked, queued } => {
                let mut back: Vec<LeafKey> = std::mem::take(asked).into_iter().collect();
                back.append(queued);
                *queued = back;
            }
            Phase::Physics => {}
        }
        rep.busy += 1;
        if rep.busy >= max {
            self.counters.busy_rotations += 1;
            self.next_source(replica, source, now, clock);
            return;
        }
        let wait = Duration::from_micros(retry_after_us).min(wait_max);
        rep.retry_at = now.plus(wait);
        rep.due = true;
    }

    fn on_nodes(
        &mut self,
        nodes: Vec<(TreePath, Node)>,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        if !matches!(self.repair.as_ref().map(|r| &r.phase), Some(Phase::Nodes { .. })) {
            // A stale node reply: the repair moved on (@shader-reviewer, L2).
            self.counters.stale_replies += 1;
            return;
        }
        let unchecked = self.mutated(Mutant::UncheckedNodes);
        let mut rep = self.repair.take().expect("checked above");
        let Phase::Nodes { mut asked, mut queued } = std::mem::replace(&mut rep.phase, Phase::Physics) else {
            unreachable!("checked above")
        };
        let tree = rep.my_root.and_then(|r| replica.tree(&r));
        let mut ok = true;
        let mut answered = false;
        // A reply may be one of several (split by bytes): take the nodes asked for, wait for the rest. A source that
        // never completes the set times out. A node not asked for is ignored.
        for (path, node) in &nodes {
            let Some(i) = asked.iter().position(|a| a.0 == *path) else { continue };
            let (_, expected) = asked.swap_remove(i);
            answered = true;
            // Every node must hash to what its verified parent named, from the target root down (S2 v1's
            // `Node::hash`): a lying source is caught at its first lie (@design-reviewer, Medium 3).
            if !unchecked && (node.hash() != Some(expected) || !extends(path, node)) {
                ok = false;
                break;
            }
            let mine = tree.as_ref().and_then(|t| t.node(path));
            compare(&mut rep, tree.as_deref(), path, node, mine.as_ref(), &mut queued);
        }
        drop(tree);
        let complete = asked.is_empty();
        rep.phase = Phase::Nodes { asked, queued };
        // Only a reply that answered something I asked shows the source is working: an empty one does not hold the
        // repair off its timeout (@shader-reviewer, re-review L2).
        if answered {
            rep.sent_at = now;
            rep.busy = 0;
        }
        self.repair = Some(rep);
        if !ok {
            self.source_fault(replica, source, now, clock);
            return;
        }
        if !complete {
            return;
        }
        let rep = self.repair.as_mut().expect("just put back");
        let more = matches!(&rep.phase, Phase::Nodes { queued, .. } if !queued.is_empty());
        if more {
            self.ask(now);
            return;
        }
        // The descent is done. A leaf the source has is fetched, not removed, even if my side listed it as absent.
        let fetched: BTreeSet<LeafKey> = rep.leaves.iter().copied().collect();
        rep.absent.retain(|k| !fetched.contains(k));
        if !rep.leaves.is_empty() {
            rep.phase = Phase::Records { asked: BTreeSet::new(), queued: rep.leaves.clone() };
            self.ask(now);
        } else if rep.physics.is_some() && rep.image.is_none() {
            rep.phase = Phase::Physics;
            self.ask(now);
        } else {
            self.import(replica, source, now, clock);
        }
    }

    fn on_records(
        &mut self,
        records: Vec<Record>,
        blobs: Vec<Bytes>,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        let Some(rep) = &mut self.repair else { return };
        let Phase::Records { asked, queued } = &mut rep.phase else {
            self.counters.stale_replies += 1;
            return;
        };
        // A reply may be one of several (split by bytes): keep what was asked, wait for the rest. A source that never
        // completes the set times out.
        let mut answered = false;
        for r in records {
            if asked.remove(&r.key) {
                rep.bytes += r.bytes.len() as u64;
                rep.records.push(r);
                rep.sent_at = now;
                rep.busy = 0;
                answered = true;
            }
        }
        // The blobs of a reply that answered something asked. Unchecked here: the replica refuses one no imported row
        // references (`BadBlob`), and a source that sent it is left (`try_import`).
        if answered {
            for b in blobs {
                let h = Hash::plain(&b);
                if !rep.blobs.contains_key(&h) {
                    rep.bytes += b.len() as u64;
                    rep.blobs.insert(h, b);
                }
            }
        }
        if !asked.is_empty() {
            return;
        }
        if !queued.is_empty() {
            self.ask(now);
        } else if rep.physics.is_some() && rep.image.is_none() {
            rep.phase = Phase::Physics;
            self.ask(now);
        } else {
            self.import(replica, source, now, clock);
        }
    }

    fn on_physics(
        &mut self,
        image: PhysicsImage,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        let Some(rep) = &mut self.repair else { return };
        if !matches!(rep.phase, Phase::Physics) {
            self.counters.stale_replies += 1;
            return;
        }
        rep.bytes += image.bytes.len() as u64;
        rep.image = Some(image);
        self.import(replica, source, now, clock);
    }

    /// Hand the fetched parts to the replica, which checks each against the target root and re-simulates to the
    /// present.
    fn import(
        &mut self,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        let Some(rep) = &mut self.repair else { return };
        let mine: Vec<Record> = match rep.my_root.filter(|_| rep.trusted).and_then(|r| replica.tree(&r)) {
            Some(t) => t.export(&rep.leaves),
            None => Vec::new(),
        };
        let import = CutImport {
            blobs: rep.blobs.values().cloned().collect(),
            cut: rep.cut.clone(),
            target_root: rep.target,
            records: rep.records.clone(),
            absent: rep.absent.iter().copied().collect(),
            physics: rep.image.clone(),
        };
        rep.phase = Phase::Importing { import: Box::new(import), mine };
        self.try_import(replica, source, now, clock);
    }

    /// Give the replica the import (again, unchanged, while it answers `Pending`), and act on its answer.
    fn try_import(
        &mut self,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        let Some(rep) = &mut self.repair else { return };
        let Phase::Importing { import, .. } = &rep.phase else { return };
        let result = replica.import_at_cut(&GroupSel::Domain, (**import).clone(), source);
        match result {
            Err(ImportError::Pending) => {}
            Ok(root) if root == rep.target => {
                let mut rep = self.repair.take().expect("checked above");
                let Phase::Importing { mine, .. } = std::mem::replace(&mut rep.phase, Phase::Physics) else {
                    unreachable!("checked above")
                };
                self.cured(rep, mine, replica, source, now, clock);
            }
            Ok(_) | Err(ImportError::RootMismatch { .. }) => {
                // Everything checked, but the patch was incomplete. Descend again against my own tree, fetching every
                // leaf this time (@shader-reviewer, L3).
                rep.descents += 1;
                if rep.descents >= self.cfg.n_descend {
                    let cut = rep.at();
                    self.forget_repair();
                    self.refused(cut, "the patch stayed incomplete", false);
                    self.after_repair(replica, source, now, clock);
                } else {
                    rep.trusted = false;
                    self.restart(now, true);
                }
            }
            // A blob no imported row references, or a small simulation blob the source did not send and this machine
            // lacks: the source's fault, so the next one is asked (inline blobs phase 3b; @world-model m90bbd5b28176).
            Err(ImportError::BadProof(_))
            | Err(ImportError::PhysicsCheck)
            | Err(ImportError::BadBlob(_))
            | Err(ImportError::MissingBlob(_)) => {
                self.source_fault(replica, source, now, clock);
            }
            Err(ImportError::NeedsPhysicsImage(_)) if rep.image.is_none() => {
                rep.physics = Some(replica.domain());
                rep.phase = Phase::Physics;
                self.ask(now);
            }
            Err(e) => {
                let cut = rep.at();
                self.forget_repair();
                let at_once = matches!(e, ImportError::RateMismatch { .. });
                let why = match e {
                    ImportError::NotKept => "the cut's state is no longer kept".to_string(),
                    // S2 v1.4: a fault to report, never a rate to adopt. Only the config differs at heart, so it is
                    // named, and the ladder stops at once (4.3).
                    // Keyed on `Config` alone, its whole cause: the other leaves drift from probe to probe in a live
                    // world, and must not make each probe a new key (@shader-reviewer, Low on 30a26079).
                    ImportError::RateMismatch { world, mine } => {
                        self.last_leaves = vec![LeafKey::Config];
                        format!("the world runs at {} Hz and this replica at {} Hz", world.hz(), mine.hz())
                    }
                    _ => "a bound body needs the physics image, and the image did not do".to_string(),
                };
                self.refused(cut, &why, at_once);
                self.after_repair(replica, source, now, clock);
            }
        }
    }

    fn cured(
        &mut self,
        rep: Repair,
        mine: Vec<Record>,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        let cut = rep.at();
        match &rep.final_cut {
            // A newer final cut is pending: the replica's floor is past this one, so it is not finalized again
            // (@shader-reviewer, re-review L1). The import matched its target; the newer cut moves Cv next.
            Some(_) if self.newer_pending(cut) => {}
            Some(fc) => {
                // Only finalize moves Cv (S2 v1): ask the replica to compare the repaired cut again.
                match replica.finalize(fc, source) {
                    Finalized::Agreed => self.set_cv(&rep.cut, rep.target),
                    // Not caught up to the cut within its budget yet: asked again once nothing runs (`tick`).
                    Finalized::NotReady => {
                        if self.pending.as_ref().is_none_or(|p| p.cut.at.frame <= cut.frame) {
                            self.pending = Some(fc.clone());
                        }
                    }
                    _ => {
                        self.last_leaves = differing_of(&rep);
                        self.refused(cut, "the import matched, but finalize did not agree", false);
                        self.after_repair(replica, source, now, clock);
                        return;
                    }
                }
            }
            None => {
                // A root split's cut is not final: vote again with the reference's root (5, step 3).
                self.actions.push(Action::Revote { cut, root: rep.target });
            }
        }
        if self.joining() {
            // The replica holds E's certified state: nothing diverged, so nothing is counted as a cure. Joined once
            // finalize agreed at E (Cv is E); a finalize answered `NotReady` is asked again from `tick`, and `on_final`
            // reports it (@engine-architect, Low).
            if self.cv.is_some_and(|(c, _)| c == cut) {
                self.joined(cut);
            }
        } else {
            self.count_cure(rep, mine, now);
        }
        self.after_repair(replica, source, now, clock);
    }

    /// A join's import matched E's root and finalize agreed there.
    fn joined(&mut self, cut: CutRef) {
        self.counters.joins += 1;
        self.join = Some(Join::Joined { cut });
    }

    /// A rung-2 repair's import matched its target: count it, write its bundle, and note recurrence.
    fn count_cure(&mut self, rep: Repair, mine: Vec<Record>, now: HostTime) {
        let cut = rep.at();
        let taken = u64::try_from(now.0.saturating_sub(rep.started.0)).unwrap_or(0);
        let differing = differing_of(&rep);
        self.reset_stuck();
        self.counters.cured_by_rung[1] += 1;
        self.counters.repair_latency_nanos.push(taken);
        self.counters.repair_bytes += rep.bytes;
        self.events.push(DivergeEvent::Repaired { cut, rung: Rung(2) });
        // The evidence goes with the repair it was captured for, and only that one (@design-reviewer, Medium): the
        // cure's bundle takes it, and nothing after it needs it.
        let ev = self.evidence.take_if(|e| rep.evidence && Some(e.key) == rep.from_cv);
        let (state_at_cv, entries, cut_heads) = self.parts(ev.map(|e| e.for_cut(cut)));
        self.bundle(FaultBundle {
            cv: rep.from_cv,
            cut,
            agreed_root: rep.target,
            my_root: rep.my_root.unwrap_or(Hash::ZERO),
            voters: rep.voters.clone(),
            differing: differing.clone(),
            mine,
            theirs: rep.records.clone(),
            rung: 2,
            sources: rep.sources[..=rep.src.min(rep.sources.len() - 1)].to_vec(),
            nanos_taken: taken,
            state_at_cv,
            cut_heads,
            entries,
            ..FaultBundle::default()
        });
        self.note_recurrence(cut, &differing, now);
    }

    /// The same leaves cured again and again: this machine computes them differently (4.3).
    fn note_recurrence(&mut self, cut: CutRef, leaves: &[LeafKey], now: HostTime) {
        let window = self.cfg.t_recur;
        let mut hit = 0;
        for k in leaves {
            let v = self.recur.entry(*k).or_default();
            v.retain(|t| now <= t.plus(window));
            v.push(now);
            if v.len() as u32 >= self.cfg.n_recur_max {
                hit += 1;
                v.clear();
            }
        }
        self.recur.retain(|_, v| !v.is_empty());
        if hit > 0 {
            self.counters.repeated_divergence += hit;
            self.events.push(DivergeEvent::RepeatedDivergence { cut });
        }
    }

    /// A rung failed. A join's rung 2 ends the join instead (`true`): counted as `join_failures`, no event, and no
    /// source rotation.
    fn fail(&mut self, rung: u8, cut: CutRef, why: &str) -> bool {
        if rung == 2 && self.joining() {
            self.counters.join_failures += 1;
            self.join = Some(Join::Failed { cut, why: why.to_string() });
            return true;
        }
        self.counters.rung_failures += 1;
        if rung == 2 {
            self.source_offset = self.source_offset.wrapping_add(1);
        }
        self.events.push(DivergeEvent::RungFailed { cut, rung: Rung(rung), why: why.to_string() });
        false
    }

    /// Rung 2 failed with the agreed state in hand: the replica could not take it (4.3). This is what counts toward
    /// the stop. A failure for want of data (every source silent or busy, no source at all, overtaken) does not: no
    /// inputs arrived, and a network outage must not stop repair for `t_stuck_probe`. A root split's repair is not
    /// counted either; the split's own rules end it (5).
    /// `at_once`: no retry can cure it (a rate mismatch: every honest source holds the same config), so the ladder
    /// stops on this first failure (@design-reviewer m467dddd5ba66).
    fn refused(&mut self, cut: CutRef, why: &str, at_once: bool) {
        let joined = self.fail(2, cut, why);
        let leaves = std::mem::take(&mut self.last_leaves);
        if !joined && self.episode.is_none() {
            self.note_failure(cut, FailKey { rung: 2, why: why.to_string(), leaves }, at_once);
        }
    }

    /// Drop the repair under way, keeping its differing leaves for its failure's key.
    fn forget_repair(&mut self) {
        if let Some(rep) = self.repair.take() {
            self.last_leaves = differing_of(&rep);
        }
    }

    /// A refused repair (4.3): stop the ladder after `n_stuck` in a row with one key, or `n_stuck_any` in a row with
    /// none cured. A probe failing the same way while stopped writes nothing new.
    fn note_failure(&mut self, cut: CutRef, key: FailKey, at_once: bool) {
        if let Some(st) = &self.stuck {
            if st.any || st.key == key {
                return;
            }
            // The key changed: the ladder runs again, and counts this key from one.
            self.stuck = None;
            self.fail_same = None;
        }
        self.fails_any += 1;
        if !self.keys_seen.contains(&key) && self.keys_seen.len() < 16 {
            self.keys_seen.push(key.clone());
        }
        match &mut self.fail_same {
            Some((k, n)) if *k == key => *n += 1,
            _ => self.fail_same = Some((key.clone(), 1)),
        }
        let same = self.fail_same.as_ref().map_or(0, |f| f.1);
        // `sticky`: only a cure ends the stop, a probe failing another way does not restart the ladder. So for a rate
        // mismatch, which no retry can cure, and for keys that kept changing.
        let (sticky, all_keys) = if at_once {
            (true, false)
        } else if same >= self.cfg.n_stuck {
            (false, false)
        } else if self.fails_any >= self.cfg.n_stuck_any {
            (true, true)
        } else {
            return;
        };
        self.counters.repairs_stuck += 1;
        let named: Vec<String> =
            if all_keys { self.keys_seen.iter().map(FailKey::name).collect() } else { vec![key.name()] };
        self.events.push(DivergeEvent::RepairStuck {
            cut,
            rung: Rung(key.rung),
            differing: key.leaves.clone(),
            why: named.join("; "),
        });
        // Root splits' failures never reach here (`refused`), so the evidence of the current `Cv` is this one's.
        let (state_at_cv, entries, cut_heads) = self.parts(self.evidence_at_cv().map(|e| e.for_cut(cut)));
        self.bundle(FaultBundle {
            cv: self.cv,
            cut,
            differing: key.leaves.clone(),
            rung: key.rung,
            stuck: named,
            state_at_cv,
            entries,
            cut_heads,
            ..FaultBundle::default()
        });
        self.stuck = Some(Stuck { any: sticky, key, probe_at: None });
    }

    /// A cure, by any rung, or a cut that agreed: the failure counts start again, and a stopped ladder runs (4.3).
    fn reset_stuck(&mut self) {
        self.counters.overtaken_in_a_row = 0;
        self.fail_same = None;
        self.fails_any = 0;
        self.keys_seen.clear();
        self.stuck = None;
    }

    /// A final cut newer than `cut` arrived during the repair and waits.
    fn newer_pending(&self, cut: CutRef) -> bool {
        self.pending.as_ref().is_some_and(|p| p.cut.at.frame > cut.frame)
    }

    /// A repair ended: take up a final cut that arrived meanwhile, with the replica's fresh verdict on it.
    fn after_repair(
        &mut self,
        replica: &mut dyn Replica,
        source: &mut dyn EntrySource,
        now: HostTime,
        clock: &dyn FrameClock,
    ) {
        if let Some(fc) = self.pending.take() {
            let f = replica.finalize(&fc, source);
            self.on_final(&fc, f, replica, source, now, clock);
        }
    }

    fn bundle(&mut self, b: FaultBundle) {
        self.counters.bundles_written += 1;
        self.events.push(DivergeEvent::BundleWritten { cut: b.cut, name: b.name() });
        self.actions.push(Action::WriteBundle(Box::new(b)));
    }
}

/// A repair's differing leaves: records fetched, the physics image, and leaves named absent; sorted, once each.
fn differing_of(rep: &Repair) -> Vec<LeafKey> {
    let mut v: Vec<LeafKey> = rep.leaves.clone();
    if let Some(image) = &rep.image {
        v.push(LeafKey::Physics(image.domain));
    }
    v.extend(rep.absent.iter().copied());
    v.sort();
    v.dedup();
    v
}

/// Split `items` into groups whose encoded size stays within `budget`, at least one item per group.
fn split_by_bytes<T>(items: Vec<T>, budget: usize, size: impl Fn(&T) -> usize) -> Vec<Vec<T>> {
    let mut groups: Vec<Vec<T>> = Vec::new();
    let mut used = 0;
    for item in items {
        let s = size(&item);
        match groups.last_mut() {
            Some(g) if used + s <= budget => {
                used += s;
                g.push(item);
            }
            _ => {
                used = s;
                groups.push(vec![item]);
            }
        }
    }
    if groups.is_empty() {
        groups.push(Vec::new());
    }
    groups
}

/// The children of a node, as (path, the hash its parent names for it). Leaves and `Empty` have none.
fn children(path: &TreePath, node: &Node) -> Vec<(TreePath, Hash)> {
    match node {
        Node::World { tables, domains, .. } => {
            let mut c = vec![(TreePath::Tables, *tables)];
            c.extend(domains.iter().map(|(d, h)| (TreePath::Domain(*d), *h)));
            c
        }
        Node::Domain { domain, objects, .. } => {
            vec![(TreePath::Objects { domain: *domain, prefix: Nibbles::default() }, *objects)]
        }
        Node::Inner { prefix, children } => {
            let domain = match path {
                TreePath::Objects { domain, .. } => *domain,
                _ => return Vec::new(),
            };
            children
                .iter()
                .filter_map(|(n, h)| prefix.child(*n).map(|p| (TreePath::Objects { domain, prefix: p }, *h)))
                .collect()
        }
        Node::Tables(_) | Node::Leaf { .. } | Node::Empty => Vec::new(),
    }
}

/// Compare one of the source's (verified) nodes with mine at the same path, and note what to fetch, what to remove
/// and where to descend. Hashes are compared only if my tree passed its rehash; structure always is. Two objects-trie
/// nodes with different prefixes are a structural difference: fetch everything under theirs, remove everything under
/// mine (what both have is fetched, not removed).
fn compare(
    rep: &mut Repair,
    tree: Option<&dyn Tree>,
    path: &TreePath,
    node: &Node,
    mine: Option<&Node>,
    queued: &mut Vec<(TreePath, Hash)>,
) {
    let trusted = rep.trusted;
    let same = |a: Option<Hash>, b: Option<Hash>| trusted && a.is_some() && a == b;
    match node {
        Node::World { .. } | Node::Domain { .. } => {
            // The world's config bytes are a leaf of the world node (S2 v1.3): fetched like a table when they differ,
            // and always on a whole-state fetch.
            if let Node::World { config, .. } = node {
                let my_config = match mine {
                    Some(Node::World { config: c, .. }) => Some(*c),
                    _ => None,
                };
                if !same(Some(*config), my_config) {
                    rep.leaves.push(LeafKey::Config);
                }
            }
            if let Node::Domain { domain, physics, .. } = node {
                let my_physics = match mine {
                    Some(Node::Domain { physics: p, .. }) => Some(*p),
                    _ => None,
                };
                if !same(Some(*physics), my_physics) {
                    rep.physics = Some(*domain);
                }
            }
            let mine_children: BTreeMap<TreePath, Hash> =
                mine.map(|m| children(path, m).into_iter().collect()).unwrap_or_default();
            for (p, h) in children(path, node) {
                if !same(Some(h), mine_children.get(&p).copied()) {
                    queued.push((p, h));
                }
            }
            for p in mine_children.keys() {
                if !children(path, node).iter().any(|c| &c.0 == p) {
                    leaves_under(tree, p, &mut rep.absent);
                }
            }
        }
        Node::Tables(hs) => {
            let my_hs: &[Hash] = match mine {
                Some(Node::Tables(m)) => m,
                _ => &[],
            };
            for (i, h) in hs.iter().enumerate() {
                if !same(Some(*h), my_hs.get(i).copied()) {
                    rep.leaves.push(LeafKey::Table(i as u8));
                }
            }
            for i in hs.len()..my_hs.len() {
                rep.absent.insert(LeafKey::Table(i as u8));
            }
        }
        Node::Inner { prefix, children: theirs } => {
            match mine {
                Some(Node::Inner { prefix: mp, children: mc }) if mp == prefix => {
                    for (n, h) in theirs {
                        let my_h = mc.iter().find(|c| c.0 == *n).map(|c| c.1);
                        if !same(Some(*h), my_h)
                            && let Some(p) = prefix.child(*n)
                        {
                            queued.push((with_prefix(path, p), *h));
                        }
                    }
                    for (n, _) in mc {
                        if !theirs.iter().any(|c| c.0 == *n)
                            && let Some(p) = prefix.child(*n)
                        {
                            leaves_under(tree, &with_prefix(path, p), &mut rep.absent);
                        }
                    }
                }
                _ => {
                    // Structural: the tries differ in shape here.
                    leaves_under(tree, path, &mut rep.absent);
                    for (n, h) in theirs {
                        if let Some(p) = prefix.child(*n) {
                            queued.push((with_prefix(path, p), *h));
                        }
                    }
                }
            }
        }
        Node::Leaf { id, .. } => {
            if !same(node.hash(), mine.and_then(|m| m.hash())) {
                leaves_under(tree, path, &mut rep.absent);
                rep.leaves.push(LeafKey::Object(*id));
            }
        }
        Node::Empty => leaves_under(tree, path, &mut rep.absent),
    }
}

/// An objects-trie node returned for a prefix must lie under it: its own prefix, or a leaf's id, extends the prefix
/// asked for. Redundant with the hash check, but a clearer fault (@design-reviewer's Low on S2 v1).
fn extends(path: &TreePath, node: &Node) -> bool {
    let TreePath::Objects { prefix: asked, .. } = path else { return true };
    match node {
        Node::Inner { prefix, .. } => prefix.starts_with(asked),
        Node::Leaf { id, .. } => Nibbles::of_id(*id).starts_with(asked),
        Node::Empty => true,
        _ => false,
    }
}

fn with_prefix(path: &TreePath, prefix: Nibbles) -> TreePath {
    match path {
        TreePath::Objects { domain, .. } => TreePath::Objects { domain: *domain, prefix },
        other => other.clone(),
    }
}

/// Every leaf of my tree under `path`: what removing that subtree removes.
fn leaves_under(tree: Option<&dyn Tree>, path: &TreePath, out: &mut BTreeSet<LeafKey>) {
    let Some(t) = tree else { return };
    let mut stack = vec![path.clone()];
    while let Some(p) = stack.pop() {
        let Some(node) = t.node(&p) else { continue };
        match &node {
            Node::Leaf { id, .. } => {
                out.insert(LeafKey::Object(*id));
            }
            Node::Tables(hs) => {
                out.extend((0..hs.len()).map(|i| LeafKey::Table(i as u8)));
            }
            _ => stack.extend(children(&p, &node).into_iter().map(|c| c.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{LineId, MemberKey};

    #[test]
    fn evidence_goes_whole_and_only_to_the_cut_it_was_extended_to() {
        let line = LineId(Hash([1; 32]));
        let c = CutRef { line, height: 3, frame: Frame(24) };
        let head = ChainHead {
            chain: pe_seams::ChainId(Hash([2; 32])),
            author: MemberKey([3; 32]),
            seq: 5,
            entry: pe_seams::EntryRef(Hash([4; 32])),
        };
        let mut ev =
            Evidence { key: (c, Hash::ZERO), state: Bytes::new(), entries: Vec::new(), upto: c.frame, to: None };
        ev.state = Bytes::from_static(&[7; 4]);
        ev.entries = vec![Bytes::from_static(&[8; 3])];
        assert_eq!(ev.for_cut(c), None, "nothing extended yet: no evidence");
        ev.to = Some((c, vec![head]));
        assert_eq!(ev.for_cut(c), Some((ev.state.clone(), ev.entries.clone(), vec![head])));
        assert_eq!(ev.for_cut(CutRef { height: 4, ..c }), None, "another height: none of it");
        assert_eq!(ev.for_cut(CutRef { line: LineId(Hash([9; 32])), ..c }), None, "another line: none of it");
        assert_eq!(ev.for_cut(CutRef { frame: Frame(32), ..c }), None, "another frame: none of it");
    }
}
