//! The M1 acceptance's "the live run equals the replay" (docs/refound-plan.md, M1, "The demo's script in CI";
//! @item:primordial-engine/m1-acceptance-the-live-run-equals-the-replay-a-r; @coordinator-opus m2d13525ba3cd and
//! m2203ab8d0618). With `--check-replay`, the opener keeps its session's final log as it runs. On the console's
//! `replay` it replays that log from frame 0 on a fresh replica, and compares the root at every final cut with the one
//! agreement made final.
//!
//! - **The log** is the one the session keeps ([`ReplayLog::record`]). As the driver finalizes each final cut, the
//!   recorder takes that cut's entries from S3 (`entries_for_cut` at each frame since the previous final cut), while
//!   agreement still keeps them, and the cut with its root ([`pe_seams::FinalCut`]). Nothing is written to a file.
//! - **The replay** ([`replay`]) runs on a thread of its own, so the session goes on. It starts from the genesis
//!   record's frame-0 replica with no local member ([`Genesis::replay_replica`], whose root the record pins: a replay
//!   computes the agreed state, never the author's own prediction of its commit). It steps straight through the log,
//!   every entry at its stamp, as pe-harness's s2replay does for P2. Its root at each recorded final cut is compared
//!   with the live one, and each matching cut is finalized, so the replay holds no more state than the live replica.
//! - **Scripts:** the replay's runner, with the live runner's settings, holds every module the log commits (each
//!   `CommitScript`'s `author_module`). It loads them from the live replica's bytes as they ran ([`Replica::module`],
//!   pe-logic's `Script::load_metered`, which checks the bytes against the hash).
//! - **Who replays:** a machine whose history starts at frame 0, the opener. A joiner starts at its import of E, and
//!   at n = 2 every final cut's root is both members' root, so the opener's replay covers the session.
//! - A machine with a test fault armed refuses `--check-replay` (main.rs): its replay would compute the fault again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use pe_replica::logic::LogicRunner;
use pe_seams::SignedEntry;
use pe_seams::{CutRef, EntrySource, FinalCut, Frame, FrameClock, Hash, HostTime, Replica};
use pe_world::edit::{Op, Transaction};

use crate::clock::LocalClock;
use crate::genesis::Genesis;

/// The least stack a replay's thread gets, as the simulation thread's; more when the runner needs it.
pub const MIN_STACK: usize = 4 << 20;

/// The most `advance` calls the frames up to one cut may take. With the replay's budget one is enough.
const MAX_CALLS: usize = 1_000;

/// The console's `replay` (main.rs), for the simulation thread, which takes it once a tick.
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// The console's `replay`: the simulation thread starts a replay at its next tick, if `--check-replay` armed one.
pub fn request() {
    REQUESTED.store(true, Ordering::Release);
}

/// Whether the console asked for a replay since the last call.
pub(crate) fn take_request() -> bool {
    REQUESTED.swap(false, Ordering::AcqRel)
}

/// The opener's final log, kept as the session runs ([`ReplayLog::record`]).
pub struct ReplayLog {
    genesis: Genesis,
    /// The newest final cut recorded: the genesis cut at first.
    last: CutRef,
    /// Each final cut recorded, with the root agreement made final, in order.
    cuts: Vec<FinalCut>,
    /// The final log since frame 0, in agreed order.
    entries: Vec<SignedEntry>,
    /// Why the log has a hole: a range agreement trimmed before it was taken. A replay then says so.
    gap: Option<String>,
}

impl ReplayLog {
    /// The log of the session `genesis` opened.
    pub fn new(genesis: Genesis) -> ReplayLog {
        let last = genesis.genesis_cut().at;
        ReplayLog { genesis, last, cuts: Vec::new(), entries: Vec::new(), gap: None }
    }

    /// Takes the final cut `fc`'s part of the log: its entries at each frame from the previous final cut's up to its
    /// own, as S3 answers them for it (`entries_for_cut`: exactly the final log's entries there, in agreed order,
    /// void ones left out). Called as the driver finalizes `fc` (crate::phases's `Timed`), before the replica's
    /// finalize moves its `Cv`: agreement keeps its final log only from `Cv`, and at n = 1 a cut is voted, final and
    /// finalized within one `drive`. A cut seen again (a finalize that answered `NotReady`) is taken once. If the log
    /// below `fc` is no longer kept, the log has a hole, which the replay reports.
    pub fn record(&mut self, fc: &FinalCut, source: &dyn EntrySource) {
        if self.gap.is_some() || fc.cut.at == self.last || fc.cut.at.frame < self.last.frame {
            return;
        }
        let mut entries = Vec::new();
        for f in self.last.frame.0..fc.cut.at.frame.0 {
            match source.entries_for_cut(&fc.cut, Frame(f)) {
                Some(v) => entries.extend(v),
                None => {
                    self.gap = Some(format!("agreement no longer kept frame {f} of the final log when it was taken"));
                    return;
                }
            }
        }
        self.entries.extend(entries);
        self.cuts.push(fc.clone());
        self.last = fc.cut.at;
    }

    /// The final cuts recorded so far.
    pub fn cuts(&self) -> usize {
        self.cuts.len()
    }

    /// A replay of the log so far, taken on the simulation thread: the log, and the bytes of every module it commits
    /// that `live` holds.
    pub fn job(&self, live: &dyn Replica) -> ReplayJob {
        let committed = committed_modules(&self.entries);
        let modules: Vec<(Hash, Bytes)> = (committed.iter()).filter_map(|h| live.module(h).map(|b| (*h, b))).collect();
        ReplayJob {
            genesis: self.genesis.clone(),
            cuts: self.cuts.clone(),
            entries: self.entries.clone(),
            absent: committed.len() - modules.len(),
            modules,
            gap: self.gap.clone(),
        }
    }

    /// Drops the first recorded entry that `keep` refuses: a test's way to break the log.
    #[cfg(test)]
    pub(crate) fn drop_entry(&mut self, keep: impl Fn(&SignedEntry) -> bool) -> bool {
        match self.entries.iter().position(|e| !keep(e)) {
            Some(i) => {
                self.entries.remove(i);
                true
            }
            None => false,
        }
    }
}

/// The modules the log commits: each `CommitScript`'s `author_module`, the hash of the metered bytes that ran.
fn committed_modules(entries: &[SignedEntry]) -> BTreeSet<Hash> {
    let mut out = BTreeSet::new();
    for e in entries {
        let Some(tx) = Transaction::decode(&e.entry.payload) else { continue };
        for op in &tx.ops {
            if let Op::CommitScript { author_module, .. } = op {
                out.insert(*author_module);
            }
        }
    }
    out
}

/// What a replay runs on ([`ReplayLog::job`]): sent to its own thread.
pub struct ReplayJob {
    genesis: Genesis,
    cuts: Vec<FinalCut>,
    entries: Vec<SignedEntry>,
    modules: Vec<(Hash, Bytes)>,
    /// Committed modules the live replica doesn't hold: a commit that never activated here.
    absent: usize,
    gap: Option<String>,
}

/// What a replay found.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// Every recorded final cut's root is the live one.
    Equal { cuts: usize, last: Frame, modules: usize },
    /// The first final cut whose root differs, after `equal` cuts that matched.
    Differs { cut: CutRef, live: Hash, replay: Hash, equal: usize },
    /// The replay couldn't run: why.
    Failed(String),
}

impl Verdict {
    /// The console's line.
    pub fn line(&self) -> String {
        match self {
            Verdict::Equal { cuts, last, modules } => format!(
                "replay: the live run equals the replay: {cuts} final cuts from frame 0 to frame {}, every root equal \
                 ({modules} modules)",
                last.0
            ),
            Verdict::Differs { cut, live, replay, equal } => format!(
                "replay: DIFFERS at the final cut of height {} at frame {} (after {equal} equal): live root {live:?}, \
                 replay root {replay:?}",
                cut.height, cut.frame.0
            ),
            Verdict::Failed(why) => format!("replay: couldn't replay: {why}"),
        }
    }
}

/// The log as a replay's entry source: every entry at its stamp, in agreed order, every frame kept.
struct Static {
    at: BTreeMap<Frame, Vec<SignedEntry>>,
    final_cut: Option<FinalCut>,
}

impl EntrySource for Static {
    fn final_cut(&self) -> Option<&FinalCut> {
        self.final_cut.as_ref()
    }
    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        Some(self.at.get(&frame).map_or(&[], Vec::as_slice))
    }
    fn kept_from(&self) -> Frame {
        Frame(0)
    }
    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}
    fn take_changed_from(&mut self) -> Option<Frame> {
        None
    }
}

/// Replays `job` (module doc). Its thread needs the runner's stack, as the simulation thread does.
pub fn replay(job: &ReplayJob) -> Verdict {
    if let Some(gap) = &job.gap {
        return Verdict::Failed(gap.clone());
    }
    if job.absent > 0 {
        // A module this machine never held can't have run here: the replay goes on, and differs if it was needed.
        eprintln!("pe-app: replay: {} committed modules aren't held here, so the replay runs without them", job.absent);
    }
    let (settings, run) = crate::net::runner_settings();
    let runner = match LogicRunner::new(settings, run) {
        Ok(r) => r,
        Err(e) => return Verdict::Failed(format!("no script runner: {e}")),
    };
    let inbox = runner.inbox();
    for (hash, bytes) in &job.modules {
        let loaded = pe_logic::runtime::Script::load_metered(inbox.engine(), inbox.settings(), bytes, *hash)
            .map_err(|e| format!("{e:?}"))
            .and_then(|script| inbox.load_script(script).map_err(|e| format!("{e:?}")));
        if let Err(e) = loaded {
            return Verdict::Failed(format!("module {hash:?} doesn't load: {e}"));
        }
    }
    let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), job.genesis.rate));
    let mut replica = match job.genesis.replay_replica(clock, Box::new(runner)) {
        Ok(replica) => replica,
        Err(e) => return Verdict::Failed(format!("the genesis record's replica: {e}")),
    };
    let mut at: BTreeMap<Frame, Vec<SignedEntry>> = BTreeMap::new();
    for e in &job.entries {
        at.entry(e.entry.stamp).or_default().push(e.clone());
    }
    let mut src = Static { at, final_cut: None };
    replica.set_budget(Duration::from_secs(3_600));
    for (equal, fc) in job.cuts.iter().enumerate() {
        let mut calls = 0;
        while !replica.advance(fc.cut.at.frame, &mut src).caught_up {
            calls += 1;
            if calls >= MAX_CALLS {
                return Verdict::Failed(format!("the replay didn't reach frame {}", fc.cut.at.frame.0));
            }
        }
        match replica.root_at_cut(&fc.cut, &mut src) {
            Ok(root) if root == fc.root => {}
            Ok(root) => return Verdict::Differs { cut: fc.cut.at, live: fc.root, replay: root, equal },
            Err(e) => {
                return Verdict::Failed(format!("no root at the final cut of frame {}: {e:?}", fc.cut.at.frame.0));
            }
        }
        // Finalized behind, so the replica keeps no state below the cut, as the live one doesn't.
        src.final_cut = Some(fc.clone());
        let _ = replica.finalize(fc, &mut src);
    }
    let last = job.cuts.last().map_or(Frame(0), |c| c.cut.at.frame);
    Verdict::Equal { cuts: job.cuts.len(), last, modules: job.modules.len() }
}

/// Runs [`replay`] on a thread of its own with `stack` bytes of stack, and prints its line when it ends.
pub fn spawn(job: ReplayJob, stack: usize) -> std::io::Result<std::thread::JoinHandle<Verdict>> {
    std::thread::Builder::new().name("pe-replay".into()).stack_size(stack).spawn(move || {
        let verdict = replay(&job);
        println!("{}", verdict.line());
        verdict
    })
}
