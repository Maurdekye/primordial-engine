//! The simulation thread (app.md 1, 3): one tick per frame. It never waits on rendering, the network or the API.
//!
//! [`Sim::tick`] runs app.md 3's steps in order: step 1 (the network inbox into agreement, once a transport is
//! wired: pe-net's runner through S4), step 2 (the local player's hands as pe-world inputs), step 3 (agreement's
//! time; its messages go out through the transport), step 4 (pe-replica's [`Driver::drive`], within the replica's
//! [`Budget`]), the draining half of step 5 (S3's events, counted until pe-diverge takes them) and step 6 (the
//! publish). Step 2's edit queue (pe-api), the Repair and Bulk lanes, and the rest of step 5 (pe-diverge's ordered
//! list and its actions) join as those crates land.
//!
//! [`spawn`] runs the loop on a thread of its own and does step 7: between ticks it waits until the next frame starts,
//! or until a [`Waker`] wakes it (the network inbox and the edit queue will hold one).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pe_api::queue::Receipt;
use pe_render::present::FrameSlot;
use pe_replica::{DriveReport, Driver};
use pe_seams::world_api::TxId;
use pe_seams::{
    Advance, AgreeEvent, Agreement, ChainId, Class, Cut, CutRef, EntryRef, Frame, FrameClock, FrameView, Hand,
    HandFeed, Health, HostTime, Lane, MemberKey, NetEvent, ObjectId, Outcome, Pos, Replica, SelfTest, SimRate,
    SubmitError, Transport, ViewFlags, VoidReason,
};

use crate::clock::HostClock;
use crate::leave::{LeaveStatus, Leaving, ViewTime};
use crate::local::{self, Chain};
use crate::phases::{self, PhaseTimes, Timed};

/// The replica's per-call budget (app.md 3; S2 v1.1), in simulated time. It is a local setting: it changes how fast the
/// replica catches up, never what it computes. It is the largest of:
/// - two frames at the world's rate (S2's floor): real time adds a frame per tick, and a budget of one frame never
///   catches up;
/// - the keep-up floor ([`Budget::at_present`]): the whole of what the last call needed to reach its present, rollback
///   included ([`Budget::called`]), plus the frames the frame clock's present advanced since, plus one
///   (@coordinator-opus, which replaced's advance plus one). The advance covers
///   ticks dearer than a frame period: a tick also takes messages in, signs the hand inputs and runs agreement, and
///   when that alone outlasts a period, real time gains several frames on each tick. Sized from stepping alone, the
///   budget then let a replica fall further behind every tick, though its frames were five times cheaper than real
///   time (@engine-architect's debug measurement). The last call's need covers late entries: a member whose entries
///   arrive `L` frames late on every frame rolls the replica back about `L` frames every tick, and a budget under
///   `L + 1` never reached the present again, so the view held for good (a 300 ms link froze it for a whole run,
///   @engine-architect's measurement, <notes>). The keep-up floor is capped at
///   [`Budget::KEEP_UP_CAP`] of stepping at the measured cost of a frame, and at [`Budget::MAX`]: a machine that
///   steps slower than real time, one back from a long stall, or one re-stepping more than that every tick, holds
///   and reports it, rather than making each tick longer than the last;
/// - what the measured cost of a frame fits into a share of a frame period ([`Budget::observe`]), up to `MAX`.
#[derive(Clone, Debug)]
pub struct Budget {
    rate: SimRate,
    /// The share of a frame period one call may spend; 0 keeps this part of the budget at the floor.
    share: f64,
    /// The measured wall time of one frame, in nanoseconds, smoothed.
    cost_ns: Option<f64>,
    /// What the measured cost fits into the share, between S2's floor and `MAX`.
    fit: u64,
    /// The frame clock's present at the latest tick, the greatest seen ([`Budget::at_present`]).
    present: Option<Frame>,
    /// The keep-up floor before the cap ([`Budget::at_present`]); 0 before any tick.
    keep_up: u64,
    /// What the last call needed to reach its present, rollback included: the frames it stepped and re-stepped plus
    /// those it left short ([`Budget::called`]); 0 before any call.
    need: u64,
    frames: u64,
}

impl Budget {
    /// S2's floor.
    pub const FLOOR_FRAMES: u64 = 2;
    /// Half a frame period: the tick also runs agreement, pe-diverge and the publish.
    pub const DEFAULT_SHARE: f64 = 0.5;
    /// The most simulated time one call is given, however cheap a frame is: twice `W_fg`.
    pub const MAX: Duration = Duration::from_secs(4);
    /// The most stepping the keep-up floor asks of one call, at the measured cost of a frame (@coordinator-opus): uncapped, a 10 s stall would become one 600-frame tick that sends no votes or timeouts.
    pub const KEEP_UP_CAP: Duration = Duration::from_millis(100);

    pub fn new(rate: SimRate) -> Budget {
        Budget::with_share(rate, Budget::DEFAULT_SHARE)
    }

    pub fn with_share(rate: SimRate, share: f64) -> Budget {
        let floor = Budget::FLOOR_FRAMES;
        let share = share.max(0.0);
        Budget { rate, share, cost_ns: None, fit: floor, present: None, keep_up: 0, need: 0, frames: floor }
    }

    /// The budget in frames.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The measured wall time of one frame, smoothed; `None` until a call has stepped a frame. A frame period over it
    /// is stepping's margin over real time: below 1, a replica that falls behind never catches up.
    pub fn cost(&self) -> Option<Duration> {
        self.cost_ns.map(|ns| Duration::from_nanos(ns as u64))
    }

    /// The budget in simulated time, rounded down to the nanosecond, so that `SimRate::frames` gives back
    /// [`Budget::frames`] exactly.
    pub fn current(&self) -> Duration {
        let ns = u128::from(self.frames) * 1_000_000_000 / u128::from(self.rate.hz());
        Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX))
    }

    /// Records that `frames` frames took `spent` of wall time. Returns the new budget when it changed.
    pub fn observe(&mut self, frames: u64, spent: Duration) -> Option<Duration> {
        if frames == 0 {
            return None;
        }
        let per = spent.as_nanos() as f64 / frames as f64;
        let cost = self.cost_ns.map_or(per, |c| c + (per - c) / 8.0);
        self.cost_ns = Some(cost);
        let period_ns = 1e9 / f64::from(self.rate.hz());
        // A float-to-integer cast saturates, so a frame that costs nothing gives the most.
        let fit = (self.share * period_ns / cost.max(1.0)) as u64;
        self.fit = fit.clamp(Budget::FLOOR_FRAMES, self.max_frames());
        self.settle()
    }

    /// Records the frame clock's present at a tick, before the replica is driven (app.md 3). The budget is then at
    /// least the whole of what the last call needed ([`Budget::called`]) plus the frames present advanced since, plus
    /// one: a replica whose frames are cheaper than a period then gains at least a frame on real time every tick,
    /// whatever else the tick costs, and late entries, which roll a replica back about as far on every tick, never
    /// keep it from the present. What the last call needed bounds what is left of it, so this asks for no less than
    /// reaching the present takes, and the replica stops there. The first tick, and a present that moved backwards,
    /// count as no advance. Returns the new budget when it changed.
    pub fn at_present(&mut self, present: Frame) -> Option<Duration> {
        let advanced = self.present.map_or(0, |last| present.0.saturating_sub(last.0));
        self.present = Some(self.present.map_or(present, |last| last.max(present)));
        self.keep_up = self.need.saturating_add(advanced).saturating_add(1);
        self.settle()
    }

    /// Records what a call did, after the replica is driven: `frames` stepped and re-stepped, and `short` of its
    /// present still to step (S2's `frame()` is the re-simulation frontier, so after a partial re-simulation that is
    /// what is left of it). The next tick's keep-up floor asks for at least their sum again ([`Budget::at_present`]).
    pub fn called(&mut self, frames: u64, short: u64) {
        self.need = frames.saturating_add(short);
    }

    /// The keep-up floor's cap in frames: [`Budget::KEEP_UP_CAP`] of stepping at the measured cost, between S2's floor
    /// and `MAX`. With no frame measured yet there is no cost to size it by, so it stays at S2's floor.
    pub fn keep_up_cap(&self) -> u64 {
        let Some(cost) = self.cost_ns else { return Budget::FLOOR_FRAMES };
        // A float-to-integer cast saturates, so a frame that costs nothing gives the most.
        let fit = (Budget::KEEP_UP_CAP.as_nanos() as f64 / cost.max(1.0)) as u64;
        fit.clamp(Budget::FLOOR_FRAMES, self.max_frames())
    }

    fn max_frames(&self) -> u64 {
        self.rate.frames(Budget::MAX)
    }

    fn settle(&mut self) -> Option<Duration> {
        let keep_up = self.keep_up.min(self.keep_up_cap());
        let frames = self.fit.max(keep_up).clamp(Budget::FLOOR_FRAMES, self.max_frames());
        (frames != self.frames).then(|| {
            self.frames = frames;
            self.current()
        })
    }
}

/// What the simulation thread counts. [`SimThread::stop`] returns it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SimStats {
    pub ticks: u64,
    /// Ticks before frame 0, while the clock converges.
    pub before_frame_0: u64,
    /// Frame views published. A tick that left the replica's view as it was publishes nothing.
    pub published: u64,
    /// Times the replica fell behind the present (`Advance::caught_up` false). Meanwhile the view is the newest frame
    /// it completed (S2 v1.10).
    pub holds: u64,
    /// The longest hold, in ticks.
    pub longest_hold: u64,
    /// Root-split episodes (S3 reported a `RootSplit` while none was open: no root could reach a quorum, divergence.md
    /// 5), and episodes that ended with a cut final at or past every reported split's frame. A split reported for a
    /// cut at or below the final cut's frame opens none.
    pub root_splits: u64,
    pub root_splits_ended: u64,
    /// pe-agree's re-cast votes (S3's `AgreeStats::recasts`): this member's votes cast again in the pinned views that
    /// re-proposed a split cut, at most one a view; pe-diverge's revotes aren't counted. With `root_splits`, a run
    /// reads roughly how many views its split episodes took.
    pub recasts: u64,
    /// Messages from the network that agreement took (step 1).
    pub received: u64,
    /// Messages agreement refused as a peer's fault (step 1): counted, never fatal.
    pub refused: u64,
    /// Messages on lanes this loop does not route: Bulk (the blob store, not yet wired), Pose (the display), and
    /// Repair when no pe-diverge runs ([`Sim::with_diverge`]).
    pub unrouted: u64,
    /// Protocol-lane messages the network hook took as pe-app's own, the join's (step 1; [`NetHook::claim`]).
    pub app_in: u64,
    /// The hook's entries (the inviter's `Admit`) agreement took into this member's chain, and those it refused
    /// (step 2; [`NetHook::take_entry`]).
    pub hook_entries: u64,
    pub hook_entries_refused: u64,
    /// Repair-lane messages pe-diverge took, and those it refused as a peer's fault (step 1): counted, never fatal.
    pub repair_in: u64,
    pub repair_refused: u64,
    /// Repair-lane messages pe-diverge made that went to the transport (step 5); with no transport they count in
    /// `unsent`.
    pub repair_out: u64,
    /// pe-diverge's actions (step 5): revotes, and how agreement took them (`Voted` or `AlreadyVotedThisView`; the
    /// rest are refused); self-tests run, and those whose digest differed from start-up's (`Failed`).
    pub revotes: u64,
    pub revotes_refused: u64,
    pub self_tests: u64,
    pub self_tests_failed: u64,
    /// Self-tests, the start-up one included, that found this thread's floating-point mode other than the default and
    /// reset it first (pe-physics `SelfTestDigest::mode_reset`): their digests are still the default mode's.
    pub self_test_mode_resets: u64,
    /// Self-tests reported `Failed` because `--inject-fault-at`'s fault fired here ([`crate::fault`]), whatever their
    /// digest: those with a different digest also count in `self_tests_failed`.
    pub self_tests_injected: u64,
    /// Fault bundles handed to the I/O thread, and those dropped because its queue was full or there is no writer.
    pub bundles: u64,
    pub bundles_dropped: u64,
    /// `RootSplit`s for a cut this machine never had to vote on, so pe-diverge cannot replay to it: counted and logged.
    pub splits_unknown: u64,
    /// pe-diverge's own counters (divergence.md 7), copied in when the simulation thread ends, so its exit line
    /// carries them ([`Sim::diverge_counters`] reads them live); default while none runs.
    pub diverge: pe_diverge::Counters,
    /// The transport's events (links, join requests), each passed to the net-event hook if one is set.
    pub net_events: u64,
    /// Messages agreement made that went to the transport (step 3).
    pub sent: u64,
    /// Messages agreement made that had nowhere to go: no transport is wired (a world alone).
    pub unsent: u64,
    /// S3's events, drained each tick (step 5).
    pub events: u64,
    /// The local player's hand inputs agreement took, and those it refused (step 2).
    pub inputs: u64,
    pub inputs_refused: u64,
    /// Frames whose hand inputs step 2 held back. Always 0 since F3's S1 (hands are never held); kept so logs from
    /// before it compare.
    pub hands_paced: u64,
    /// Frames whose pose-only hand entries step 2 left out while agreement reported this member backlogged (S3 v1.5,
    /// [`Agreement::backlogged`]): thinned to [`HAND_FLOOR_HZ`], never below; a grip's change always goes (F3 S1).
    pub hands_floor_thinned: u64,
    /// A member that can't keep real time (divergence.md 4.6, [`crate::slow`]): resyncs started, the time spent
    /// slow, and the largest lag behind the clock, in ms.
    pub resyncs_started: u64,
    pub slow_ms: u64,
    pub lag_max_ms: u64,
    /// `HandMode` entries step 2 sent for the console's `hand-mode` ([`crate::handfeel`]), and those agreement
    /// refused.
    pub hand_modes: u64,
    pub hand_modes_refused: u64,
    /// Edits step 2 took from pe-api's queue that agreement took, and those it refused (app.md 3.1).
    pub edits: u64,
    pub edits_refused: u64,
    pub votes: u64,
    pub finalized: u64,
    /// This member's `Leave` submissions agreement refused (app.md 4): each is tried again at the next tick.
    pub leave_refused: u64,
    /// Stamp-ahead's lead at the latest tick, in frames (app.md 3.2).
    pub lead: u64,
    /// The largest lead so far, and the longest drain of a surplus, in frames (@coordinator-opus).
    pub largest_lead: u64,
    pub longest_drain: u64,
    /// Hand poses not sent because the frame's stamp stood still while the lead shed (app.md 3.2).
    pub poses_thinned: u64,
    /// Frames stamp-ahead's speed test found a hand moving, so the lead shed at most one frame in 11 (app.md 3.2).
    pub moving_frames: u64,
    /// B's shown present (app.md 3, [`crate::shown`]): how far ahead of the present it ran at the latest tick, and the
    /// most, in frames; and the ticks it ran ahead at all (an XR session drawing).
    pub shown_ahead: u64,
    pub largest_shown_ahead: u64,
    pub ticks_shown_ahead: u64,
    /// Where the thread's time went, step by step ([`crate::phases`]): wall time on this machine.
    pub phases: PhaseTimes,
    /// The local player's grasps, left hand then right (app.md 3.1 step 2).
    pub hands: [HandStats; 2],
    /// Rollbacks whose cause step 5 noted ([`Sim::note_rollback`]): one line each in the cut log (1.11).
    pub rollback_lines: u64,
    /// The deepest rollback of the run, in frames: the most any published view's `counters.deepest_rollback` read
    /// (the plan's "measured and reported": the deepest rollback).
    pub deepest_rollback: u64,
    /// The finality lag at the end of each tick once a cut is final: the clock's present frame minus the final cut's
    /// frame.
    /// About how long an action taken now takes to become final (the plan's "how long before an action is final").
    pub final_lag: FinalLag,
}

/// A distribution of the finality lag, in frames ([`SimStats::final_lag`]): exact below [`FinalLag::EXACT`] frames,
/// one bucket above. Its `Debug` prints the summary the end line needs, not the buckets.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct FinalLag {
    n: u64,
    sum: u64,
    max: u64,
    counts: Vec<u64>,
}

impl FinalLag {
    /// Lags counted one by one; anything larger shares the top bucket.
    pub const EXACT: u64 = 1024;

    pub fn record(&mut self, lag: u64) {
        if self.counts.is_empty() {
            self.counts = vec![0; Self::EXACT as usize + 1];
        }
        self.n += 1;
        self.sum = self.sum.saturating_add(lag);
        self.max = self.max.max(lag);
        self.counts[lag.min(Self::EXACT) as usize] += 1;
    }

    /// Ticks sampled.
    pub fn samples(&self) -> u64 {
        self.n
    }

    pub fn mean(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.sum as f64 / self.n as f64 }
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    /// The smallest lag at or below which a fraction `q` of the samples lie; [`FinalLag::EXACT`] stands for any lag at
    /// or above it. 0 with no samples.
    pub fn quantile(&self, q: f64) -> u64 {
        let need = (q.clamp(0.0, 1.0) * self.n as f64).ceil() as u64;
        let mut seen = 0;
        for (lag, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= need.max(1) {
                return lag as u64;
            }
        }
        0
    }
}

impl std::fmt::Debug for FinalLag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FinalLag {{ ticks: {}, mean: {:.1}, p99: {}, max: {} }}",
            self.n,
            self.mean(),
            self.quantile(0.99),
            self.max
        )
    }
}

/// One hand's grasps (app.md 3.1 step 2; @coordinator-opus's ruling on the pick): its `engaged` edges as sent, the
/// grabs and releases they sent, and the grabs' outcomes as the newest view tells them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandStats {
    /// `engaged` edges sent: false to true (a close), true to false (an open).
    pub closes: u64,
    pub opens: u64,
    /// `Grab`s agreement took at a close, and `Release`s at an open; of those, the `Throw`s, which carried the velocity
    /// the hand gave the object at the real opening (world-model.md 8.1).
    pub grabs: u64,
    pub releases: u64,
    pub throws: u64,
    /// The grabs applied, and those that weren't, by reason: a refusal's `Reason` (`Held`, `OutOfReach`, `Cooldown`,
    /// ...), or `Dropped` or `Unknown` for a voided entry or an unknown verdict. A grab no view has stepped yet is in
    /// neither.
    pub grabs_applied: u64,
    pub grabs_refused: BTreeMap<String, u64>,
}

/// Step 2's grasp questions (app.md 3.1), answered from the replica's present world: pe-world's pick and claims.
struct WorldGrasps<'a>(&'a dyn Replica);

impl local::Grasps for WorldGrasps<'_> {
    fn pick(&self, author: MemberKey, hand: ObjectId, target: Pos, now: Frame) -> Option<ObjectId> {
        crate::api::snapshot_of(self.0)?.pick_grab(now, author, hand, target)
    }

    fn held(&self, hand: ObjectId, now: Frame) -> Vec<ObjectId> {
        crate::api::snapshot_of(self.0).map_or_else(Vec::new, |s| s.held_by_hand(hand, now))
    }
}

/// How a `Grab` fared in a view's record: `None` if applied, else the reason's name.
fn grab_fate(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Applied { .. } => None,
        Outcome::Refused { reason, .. } => {
            let name = format!("{reason:?}");
            Some(name.split([' ', '{', '(']).next().unwrap_or_default().to_string())
        }
        Outcome::Dropped { .. } => Some("Dropped".into()),
        Outcome::Unknown { .. } => Some("Unknown".into()),
    }
}

/// `view` with each of `objects` (sorted) marked `RELEASED_LOCAL`.
fn released_local(view: &FrameView, objects: &[ObjectId]) -> FrameView {
    let mut v = view.clone();
    for o in &mut v.objects {
        if objects.binary_search(&o.id).is_ok() {
            o.flags = o.flags.with(ViewFlags::RELEASED_LOCAL);
        }
    }
    v
}

/// The time since `mark`, which moves to now: one step's share of a tick.
fn lap(mark: &mut Instant) -> u64 {
    let now = Instant::now();
    let d = phases::nanos(now - *mark);
    *mark = now;
    d
}

/// `view` published again with only its health changed: a publication with nothing new since the last (S5): no
/// changes, effects, retractions, activations or counters, and no object marked `CORRECTED`, since those were the
/// last publication's (@engine-architect's and @design-reviewer's Lows on 785ab327).
fn health_only(view: &FrameView, health: Health) -> FrameView {
    let mut v = FrameView {
        health,
        changes: Vec::new(),
        effects: Vec::new(),
        retracted: Vec::new(),
        activations: Vec::new(),
        activations_retracted: Vec::new(),
        counters: pe_seams::ViewCounters::default(),
        ..view.clone()
    };
    for o in &mut v.objects {
        o.flags = pe_seams::ViewFlags(o.flags.0 & !pe_seams::ViewFlags::CORRECTED.0);
    }
    v
}

/// The simulation thread's state: agreement, the replica and the loop around them.
pub struct Sim {
    agree: Box<dyn Agreement>,
    replica: Box<dyn Replica>,
    clock: Arc<dyn FrameClock>,
    slot: Arc<FrameSlot>,
    driver: Driver,
    budget: Budget,
    /// The verdict this tick's votes carry: `NotRun` until pe-diverge asks for a self-test.
    self_test: SelfTest,
    /// pe-diverge (step 5, [`Sim::with_diverge`]): detection and repair. `None` runs none, so a root split stays open.
    diverge: Option<crate::diverge::DivergeLoop>,
    published: Option<Arc<FrameView>>,
    /// The open root splits: the frames of the cuts S3 reported `RootSplit` for, above the final cut's frame. While
    /// any is open, the health this machine reports is `Repairing` from the first ([`Sim::health_of`]; divergence.md
    /// 7). A set, not the earliest alone: a revote can settle an early split before a later one (@engine-architect).
    splits: std::collections::BTreeSet<Frame>,
    /// The replica's view behind `published`: `published` is a copy with the composed health while a split is open.
    source_view: Option<Arc<FrameView>>,
    /// Ticks into the current hold.
    hold: Option<u64>,
    stats: SimStats,
    /// The local player: this member's chain, and the hands step 2 reads. `None` runs no local input.
    local: Option<Local>,
    /// The frame step 2 last submitted the local player's inputs for.
    input_frame: Option<Frame>,
    /// The stamp of the last frame's inputs: a frame whose stamp equals it thins its poses (app.md 3.2).
    input_stamp: Option<Frame>,
    /// The last frame whose pose-only hand entries went out unthinned: the floor's clock while backlogged (F3 S1).
    pose_frame: Option<Frame>,
    /// Stamp-ahead (app.md 3.2, [`crate::stamp`]): the lead this member's own entries are stamped ahead by, fed each
    /// tick from the other members' lateness reports (S3 v1.8). `None` stamps them at the present.
    lead: Option<crate::stamp::StampLead>,
    /// Stamp-ahead's speed test (app.md 3.2): whether a hand moves, which slows the lead's shedding.
    motion: crate::stamp::Motion,
    /// B (app.md 3, [`crate::shown`]): the display lead the XR thread measures. The shown present runs it, less the
    /// session's delay, plus a period and the publication's margin ahead of the present. `None` (the default) and an
    /// empty lead keep it at the present.
    shown: Option<Arc<crate::shown::DisplayLead>>,
    /// The session's delay (the genesis record's `display_delay_ms`; the user's "one per session"), 0 by default.
    display_delay: Duration,
    /// The network (S4): pe-net's handle, or a test's in-process network. `None` is a world alone.
    net: Option<Box<dyn Transport + Send>>,
    net_hook: Option<Box<dyn NetHook>>,
    /// The members whose links were up before the simulation started ([`Sim::with_linked`]).
    linked: Vec<MemberKey>,
    /// S3's events, kept for a test or a diagnostic (`with_event_log`); otherwise only counted.
    event_log: Option<Arc<std::sync::Mutex<Vec<AgreeEvent>>>>,
    /// pe-api's ports (app.md 3.1), with what this loop keeps for them.
    api: Option<ApiLoop>,
    /// The graceful quit (app.md 4, [`crate::leave`]).
    leaving: Leaving,
    /// The stamp of this member's newest script commit step 2 submitted: until the replica has stepped it, the
    /// `Admit`'s check counts it as scripts the world holds (app.md 4; @engine-architect's M1 on 1.6).
    script_stamp: Option<Frame>,
    /// The smoothed view time the quit's bound follows, from S3's proposals.
    view_time: ViewTime,
    /// The quit's bound, if set locally ([`Sim::with_leave_wait`]); otherwise it follows the view time.
    leave_wait: Option<Duration>,
    /// The members whose link is down (S4's `LinkDown`, until a `LinkUp`).
    down: std::collections::BTreeSet<MemberKey>,
    /// What the console should say (app.md 4), drained by [`Sim::take_notices`].
    notices: Vec<String>,
    /// `--inject-fault-at`'s fault ([`crate::fault`]): this process's armed one, which its replica injects. The loop
    /// prints its announcement and fails the self-test once it has fired.
    fault: Option<Arc<crate::fault::Fault>>,
    /// `--check-replay`'s final log ([`crate::replay`]): kept each tick, replayed on the console's `replay`.
    replay: Option<crate::replay::ReplayLog>,
    /// A joiner's import of E's state (app.md 4, "B's state at E"): the inviter, while pe-diverge's join runs. The
    /// replica is empty meanwhile, so nothing drives it, no edit goes in and no view goes out ([`Sim::join_at_final`]).
    joining: Option<MemberKey>,
    /// Why the join failed, once it has: the joiner then quits ([`Sim::join_failed`]).
    join_failed: Option<String>,
    /// Called once when the join fails: the binary's quit ([`Sim::with_on_join_failed`]).
    on_join_failed: Option<Box<dyn FnOnce() + Send>>,
    /// The window's status line ([`Sim::with_status`]): "Joining: catching up…" from a join's start until the
    /// replica first reaches the present.
    status: Option<Arc<pe_render::notices::StatusLine>>,
    /// The status this simulation put up ([`Sim::set_status`]): it takes the line over the hand-feel pair.
    status_own: Option<String>,
    /// The console's hand-feel switch ([`Sim::with_hand_feel`], [`crate::handfeel`]), and the pair's version last
    /// shown.
    hand_feel: Option<Arc<crate::handfeel::HandFeel>>,
    hand_feel_shown: u64,
    /// The status line shows the join's catch-up.
    catching_up: bool,
    /// The tick trace, when `PE_TICK_TRACE` names a file ([`Sim::with_tick_trace`], [`crate::trace`]).
    trace: Option<crate::trace::TickTrace>,
    /// How far behind the clock this member is, and when it must freeze and resync (divergence.md 4.6).
    slow: crate::slow::SlowWatch,
    /// A resync's import is under way ([`Sim::resyncing`]): frozen, nothing steps, nothing is appended.
    resyncing: bool,
    /// Slow past the freeze at `n = 2`, where no resync is possible: told once per slow stint.
    cant_resync: bool,
    /// The cut log, when `PE_CUT_LOG` names a file ([`Sim::with_cut_log`], [`crate::cutlog`]).
    cut_log: Option<crate::cutlog::CutLog>,
}

/// pe-api's ports and the loop's own state for them (app.md 3.1).
struct ApiLoop {
    ports: crate::api::Ports,
    /// The frame step 2 last took an edit for: one a frame.
    edit_frame: Option<Frame>,
    /// The entries of the `Voided` events drained since the last `confirm_cut`, with their reasons (pe-api's re-send
    /// backstop re-sends a late one, api.md 9.4).
    voided: Vec<(EntryRef, VoidReason)>,
    /// The final cut the change log last confirmed.
    confirmed: Option<CutRef>,
}

/// What net.rs does on the simulation thread (@shader-lang). Admission is decided from the agreed
/// governance, which lives here with agreement, and carried out through pe-net's control end, which the hook owns.
/// What the network hook may ask of the world (app.md 4: a join's M1 limit, and what this machine can serve).
pub trait WorldFacts {
    /// Whether the present world holds a script version, or a commit pending and not voided.
    fn holds_scripts(&self) -> bool;
    /// The cut this machine's state is verified at (pe-diverge's Cv), whose state it serves; `None` without one, or
    /// without pe-diverge.
    fn verified_cut(&self) -> Option<CutRef>;
}

/// The replica's present world, as the hook asks it: pe-world's snapshot, read only when asked.
struct ReplicaFacts<'a>(&'a dyn Replica, Option<&'a crate::diverge::DivergeLoop>);

impl WorldFacts for ReplicaFacts<'_> {
    fn holds_scripts(&self) -> bool {
        crate::api::snapshot_of(self.0)
            .is_some_and(|s| !s.modules().is_empty() || s.pending().values().any(|c| c.void.is_none()))
    }

    fn verified_cut(&self) -> Option<CutRef> {
        self.1.and_then(|d| d.d.verified_cut()).map(|(cut, _)| cut)
    }
}

/// The `Admit`'s facts: the replica's, and whether this member's own script commit is stamped ahead of what the
/// replica has stepped, so the check that the world holds no scripts can't miss it (app.md 4).
struct AdmitFacts<'a>(ReplicaFacts<'a>, bool);

impl WorldFacts for AdmitFacts<'_> {
    fn holds_scripts(&self) -> bool {
        self.1 || self.0.holds_scripts()
    }

    fn verified_cut(&self) -> Option<CutRef> {
        self.0.verified_cut()
    }
}

pub trait NetHook: Send {
    /// Each of the transport's events, after step 1 counts it. `world` answers the join's M1 limit.
    fn on_event(&mut self, event: &NetEvent, agree: &mut dyn Agreement, world: &dyn WorldFacts);
    /// Once a tick, after step 1's messages and events: for what reaches the hook other than as an event, such as
    /// the lifecycle outcomes on pe-net's control end, which would otherwise wait for an unrelated event
    /// (@design-reviewer's Low on 5ca43067).
    fn on_tick(&mut self, now: HostTime, agree: &mut dyn Agreement, world: &dyn WorldFacts) {
        let _ = (now, agree, world);
    }
    /// A Protocol-lane message, offered before agreement sees it: `true` if it is pe-app's own (the join's messages,
    /// crate::join's tag) and the hook took it. Agreement never sees a message the hook took.
    fn claim(&mut self, from: MemberKey, bytes: &[u8], now: HostTime, agree: &dyn Agreement) -> bool {
        let _ = (from, bytes, now, agree);
        false
    }
    /// A Protocol entry this member's chain is to carry, taken once: the inviter's `Admit` (app.md 4). Step 2 writes
    /// it first in the frame, at the frame's stamp, and reports with [`NetHook::entry_written`]. Never asked once this
    /// member's `Leave` is asked for. `world` answers the join's M1 limit, checked again in this tick.
    fn take_entry(&mut self, world: &dyn WorldFacts, agree: &dyn Agreement, now: HostTime) -> Option<Vec<u8>> {
        let _ = (world, agree, now);
        None
    }
    /// Whether this member's `Leave` must wait: the inviter never writes it while a join it admitted is in flight
    /// (app.md 4, "A quit during a join"). The quit's bound still holds.
    fn holds_leave(&self) -> bool {
        false
    }
    /// Whether a join this member admits holds script commits back ([`crate::join::Inviter::holds_script_commits`]): pe-api
    /// refuses them `unavailable` meanwhile (app.md 4).
    fn holds_script_commits(&self) -> bool {
        false
    }
    /// What became of the entry [`NetHook::take_entry`] handed over: agreement took it into this member's chain, or
    /// refused it.
    fn entry_written(&mut self, written: Result<(), SubmitError>, now: HostTime, agree: &dyn Agreement) {
        let _ = (written, now, agree);
    }
    /// What the console should say since the last call (the join's lines, app.md 4).
    fn take_notices(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// The most messages step 1 takes from the network in one tick, so a flood cannot stall the tick: the rest wait
/// in the transport's bounded queue for the next one.
pub const MAX_IN_PER_TICK: usize = 1024;

/// The floor of a hand's canonical pose rate while agreement reports this member backlogged (F3 S1, S5 Part 1;
/// @coordinator-opus): thinned to this and no further, so the other players never see this player's
/// hands degrade however long finality trails. A target to benchmark (T-S5 judges grabs and throws at it), not a
/// measured value.
pub const HAND_FLOOR_HZ: u64 = 30;

/// The host time of `t`, a present in fractional frames (stamp-ahead's pose clock): between its frame's start and the
/// next's, in proportion.
fn host_time_at(clock: &dyn FrameClock, t: f64) -> HostTime {
    let f = t.max(0.0).floor();
    let (a, b) = (clock.host_time_of(Frame(f as u64)), clock.host_time_of(Frame(f as u64 + 1)));
    // A float-to-integer cast saturates, and the fraction is in [0, 1), so this stays between the two.
    HostTime(a.0 + ((b.0 - a.0) as f64 * (t - f)) as i64)
}

/// The local player's chain and hand feed (app.md 3, step 2).
struct Local {
    chain: Chain,
    hands: Arc<dyn HandFeed>,
    /// Each hand's grasp as last sent, left then right (`local::submit_hands`).
    grasp: local::Grasp,
    /// This member's `Grab`s: the hand each came from, and how the newest view that stepped it says it fared (`None`
    /// before any has, `Some(None)` applied, `Some(Some(reason))` not), for [`HandStats`].
    grabs: BTreeMap<EntryRef, (usize, Option<Option<String>>)>,
    /// This member's `Release`s not yet behind a final frame: the object each names, its stamp, and whether a view's
    /// records have applied it. Each publication marks the objects of applied ones `RELEASED_LOCAL` (render.md 8, D4's
    /// hand-off), so a view the display loses in a gap doesn't lose the mark (@design-reviewer's L1 on bb23fc58).
    released: BTreeMap<EntryRef, (ObjectId, Frame, bool)>,
}

/// How long past its stamp a `Release`'s mark lasts at most ([`Local::let_go`]): far longer than a gap in the views
/// (over 250 ms lost, render.md 3), so the view after one still carries it.
const RELEASED_KEPT: Duration = Duration::from_secs(2);

impl Local {
    /// The objects this member's own `Release`s have let go of, for `view`'s publication, sorted: each `Release` an
    /// applied record in `view`'s or an earlier publication's changes names, until a final frame passes its stamp or
    /// [`RELEASED_KEPT`] has, whichever is first (a world alone never finalizes). One the view shows held by this
    /// member again is forgotten, so a later hold end isn't read as this release. A record that says otherwise (a
    /// re-simulation that dropped it) clears it.
    fn let_go(&mut self, view: &FrameView, rate_hz: u32) -> Vec<ObjectId> {
        for r in &view.changes {
            if let Some((_, _, applied)) = self.released.get_mut(&r.entry) {
                *applied = matches!(r.outcome, Outcome::Applied { .. });
            }
        }
        if let Some(f) = view.final_frame {
            self.released.retain(|_, &mut (_, at, _)| at > f);
        }
        let kept = (RELEASED_KEPT.as_nanos() * u128::from(rate_hz) / 1_000_000_000) as u64;
        self.released.retain(|_, &mut (_, at, _)| at.0.saturating_add(kept) > view.frame.0);
        // Held by this member again after the release applied: that release is over, and its mark must not come back
        // at the next hold end, whatever ends it (@design-reviewer's L1 on dc6a7a9b). Before it applies the object
        // is still held in every view up to its stamp (a lead stamps it ahead of the present), so a release that
        // hasn't applied is kept (@design-reviewer's M1 on 5277241c).
        let held_again = |o: ObjectId| view.object(o).is_some_and(|x| x.flags.contains(ViewFlags::HELD_LOCAL));
        self.released.retain(|_, &mut (o, _, applied)| !(applied && held_again(o)));
        let mut out: Vec<ObjectId> =
            self.released.values().filter(|&&(_, _, applied)| applied).map(|&(o, _, _)| o).collect();
        out.sort();
        out.dedup();
        out
    }

    /// Takes the fates of this member's grabs from `changes`, and recounts each hand's tally if one changed.
    fn grab_fates(&mut self, changes: &[pe_seams::StepRecord], hands: &mut [HandStats; 2]) {
        let mut changed = false;
        for r in changes {
            if let Some((_, fate)) = self.grabs.get_mut(&r.entry) {
                let now = Some(grab_fate(&r.outcome));
                changed |= *fate != now;
                *fate = now;
            }
        }
        if changed {
            for h in hands.iter_mut() {
                h.grabs_applied = 0;
                h.grabs_refused.clear();
            }
            for (i, fate) in self.grabs.values() {
                match fate {
                    Some(None) => hands[*i].grabs_applied += 1,
                    Some(Some(reason)) => *hands[*i].grabs_refused.entry(reason.clone()).or_default() += 1,
                    None => {}
                }
            }
        }
    }
}

impl Sim {
    /// `budget` is set on the replica at once.
    pub fn new(
        agree: Box<dyn Agreement>,
        mut replica: Box<dyn Replica>,
        clock: Arc<dyn FrameClock>,
        slot: Arc<FrameSlot>,
        budget: Budget,
    ) -> Sim {
        replica.set_budget(budget.current());
        // Stamp-ahead on by default (app.md 3.2): with no reports, or reports within the target, the lead stays 0.
        let lead = SimRate::new(clock.rate_hz()).map(|r| crate::stamp::StampLead::new(crate::stamp::LeadConfig::m1(r)));
        let motion = crate::stamp::Motion::new(crate::stamp::MotionConfig::m1(clock.rate_hz()));
        Sim {
            agree,
            replica,
            clock,
            slot,
            driver: Driver::new(),
            budget,
            self_test: SelfTest::NotRun,
            diverge: None,
            published: None,
            splits: std::collections::BTreeSet::new(),
            source_view: None,
            hold: None,
            stats: SimStats::default(),
            local: None,
            input_frame: None,
            input_stamp: None,
            pose_frame: None,
            lead,
            motion,
            shown: None,
            display_delay: Duration::ZERO,
            net: None,
            net_hook: None,
            linked: Vec::new(),
            event_log: None,
            api: None,
            leaving: Leaving::Staying,
            script_stamp: None,
            view_time: ViewTime::default(),
            leave_wait: None,
            down: std::collections::BTreeSet::new(),
            notices: Vec::new(),
            fault: crate::fault::armed(),
            replay: None,
            joining: None,
            join_failed: None,
            on_join_failed: None,
            status: None,
            status_own: None,
            hand_feel: None,
            hand_feel_shown: 0,
            catching_up: false,
            trace: None,
            slow: crate::slow::SlowWatch::default(),
            resyncing: false,
            cant_resync: false,
            cut_log: None,
        }
    }

    /// pe-api's ports over this simulation (app.md 3.1), from its replica's present world as the frame-0 snapshot, its
    /// agreement's line, its clock's rate and its local member; `wake` wakes the simulation thread when an edit is
    /// queued. `None` unless the replica is a pe-world replica and there is a local player (`with_local` first): an
    /// edit goes on the member's chain, so a machine with no member has no edit port (@design-reviewer's Low on
    /// slice 1). Call before the first tick, then [`Sim::with_api`].
    pub fn ports(&self, world_name: &str, wake: impl Fn() + Send + Sync + 'static) -> Option<crate::api::Ports> {
        let genesis = crate::api::snapshot_of(&*self.replica)?;
        let facts = crate::api::Facts {
            world_name: world_name.into(),
            line: self.agree.line(),
            rate: SimRate::new(self.clock.rate_hz())?,
            me: Some(self.local.as_ref()?.chain.me()),
        };
        Some(crate::api::Ports::new(genesis, facts, wake))
    }

    /// pe-api's ports (app.md 3.1): step 2 drains `ports.queue` one edit a frame, the change log follows the views and
    /// agreement's final cut, and the shared state `ApiWorld` reads follows the replica.
    ///
    /// # Panics
    ///
    /// Without a local player (`with_local` first): its queued edits could never be submitted, and their callers would
    /// wait out pe-api's timeout with no reason given.
    pub fn with_api(mut self, ports: crate::api::Ports) -> Sim {
        assert!(self.local.is_some(), "pe-api's edit port needs this machine's member: with_local first");
        self.api = Some(ApiLoop { ports, edit_frame: None, voided: Vec::new(), confirmed: None });
        self.seed_here();
        self
    }

    /// The network (app.md 2): step 1 drains `net` into agreement and step 3 sends agreement's messages through it.
    /// `hook` gets each of its events after they are counted, and a call once a tick; without it events are only
    /// counted.
    pub fn with_transport(mut self, net: Box<dyn Transport + Send>, hook: Option<Box<dyn NetHook>>) -> Sim {
        self.net = Some(net);
        self.net_hook = hook;
        self
    }

    /// B (app.md 3, [`crate::shown`]): run the shown present ahead by `lead`'s percentile, less the session's delay,
    /// plus a period and the publication's margin, whenever the XR thread records one.
    pub fn with_shown(mut self, lead: Arc<crate::shown::DisplayLead>) -> Sim {
        self.shown = Some(lead);
        self
    }

    /// The session's delay (the genesis record's `display_delay_ms`, or the option alone): how much of the display
    /// lead the shown present leaves uncovered. The VR presenter draws the world that long before the moment shown.
    pub fn with_display_delay(mut self, delay: Duration) -> Sim {
        self.display_delay = delay;
        self
    }

    /// The session's delay ([`Sim::with_display_delay`]).
    pub fn display_delay(&self) -> Duration {
        self.display_delay
    }

    /// The members whose links were already up when the simulation started: the M1 interim's setup takes their
    /// `LinkUp` itself (net.rs), so step 1 never sees it. pe-api's `here` (session_info) starts from them, and later
    /// `LinkUp` and `LinkDown` events keep it current.
    pub fn with_linked(mut self, linked: Vec<MemberKey>) -> Sim {
        self.linked = linked;
        self.seed_here();
        self
    }

    /// pe-api's `here` for [`Sim::with_linked`]'s members, once both it and pe-api's ports are in place. pe-net's
    /// link statistics can't stand in: they arrive every `STATS_EVERY`, so at startup they often say nothing yet.
    fn seed_here(&self) {
        let Some(api) = &self.api else { return };
        for m in &self.linked {
            api.ports.shared.link(*m, true);
        }
    }

    /// The quit's bound, set locally (app.md 4): `None` follows the link ([`crate::leave::bound`]).
    pub fn with_leave_wait(mut self, wait: Option<Duration>) -> Sim {
        self.leave_wait = wait;
        self
    }

    /// Asks this member to leave (app.md 4): step 2 submits its `Leave` at the next tick, and nothing local after it.
    /// A machine with no member has nothing to leave.
    pub fn leave(&mut self) {
        if self.leaving == Leaving::Staying && self.local.is_some() {
            self.leaving = Leaving::Asked;
        }
    }

    /// Where the quit's wait stands ([`LeaveStatus`]).
    pub fn leave_status(&self) -> LeaveStatus {
        if !matches!(self.leaving, Leaving::Left { .. }) {
            return LeaveStatus::Pending;
        }
        if self.agree.leave_settled() {
            return LeaveStatus::Settled;
        }
        let me = self.local_member();
        let others: Vec<MemberKey> = self.agree.members().into_iter().filter(|m| Some(*m) != me).collect();
        if others.iter().all(|m| self.down.contains(m)) { LeaveStatus::Alone } else { LeaveStatus::Waiting }
    }

    /// The quit's bound: the local setting, or [`crate::leave::VIEWS`] smoothed view times between 5 and 15 s.
    pub fn leave_bound(&self) -> Duration {
        self.leave_wait.unwrap_or_else(|| crate::leave::bound(self.view_time.get()))
    }

    /// The other members of this member's epoch: those its `Leave` waits for.
    pub fn other_members(&self) -> Vec<MemberKey> {
        let me = self.local_member();
        self.agree.members().into_iter().filter(|m| Some(*m) != me).collect()
    }

    /// What the console should say since the last call (app.md 4: the leave's messages, and the network hook's, the
    /// join's).
    pub fn take_notices(&mut self) -> Vec<String> {
        if let Some(hook) = &mut self.net_hook {
            self.notices.extend(hook.take_notices());
        }
        std::mem::take(&mut self.notices)
    }

    /// Whether this machine holds a signed `Leave` by `m` that isn't final yet: S3's entries from a little below the
    /// final frame to past the present by stamp-ahead's largest lead, read only when a link drops.
    fn holds_leave_of(&self, m: MemberKey, present: Frame) -> bool {
        let final_frame = self.agree.final_cut().map_or(0, |fc| fc.cut.at.frame.0);
        let ahead = (u64::try_from(crate::leave::SCAN.as_millis()).unwrap_or(u64::MAX))
            .saturating_mul(u64::from(self.clock.rate_hz()))
            .div_ceil(1000);
        (final_frame.saturating_sub(ahead)..present.0.saturating_add(ahead)).any(|f| {
            self.agree
                .entries_at(Frame(f))
                .is_some_and(|es| es.iter().any(|e| e.entry.author == m && pe_seams::is_leave(e)))
        })
    }

    /// A link went down (S4): the console says why, at once (app.md 4). A member still in this epoch whose `Leave` is
    /// in hand left before the session confirmed it; one with no `Leave` dropped and may come back; one already out of
    /// the epoch left as it should.
    fn link_down(&mut self, peer: MemberKey, present: Frame) {
        // A member that is leaving itself says only how its own quit ended (`SimThread::leave_and_stop`).
        if !self.down.insert(peer) || self.local_member() == Some(peer) || self.leaving != Leaving::Staying {
            return;
        }
        let notice = if !self.agree.members().contains(&peer) {
            crate::leave::left(peer)
        } else if self.holds_leave_of(peer, present) {
            crate::leave::stranded(peer)
        } else {
            crate::leave::dropped(peer)
        };
        self.notices.push(notice);
    }

    /// Stamp-ahead's lead (app.md 3.2), M1's settings by default: `None` stamps this member's entries at the present,
    /// as a test of the receivers' side does; `Some` replaces the controller, for one with other settings or a fixed
    /// lead ([`crate::stamp::StampLead::fixed`]).
    pub fn with_stamp_lead(mut self, lead: Option<crate::stamp::StampLead>) -> Sim {
        self.lead = lead;
        self
    }

    /// Keep S3's events in `log` as well as counting them (a test's, or a diagnostic's: it grows without a bound).
    pub fn with_event_log(mut self, log: Arc<std::sync::Mutex<Vec<AgreeEvent>>>) -> Sim {
        self.event_log = Some(log);
        self
    }

    /// Keeps this session's final log for `--check-replay` ([`crate::replay`]): the opener's, whose history starts at
    /// frame 0. The console's `replay` then replays it from frame 0 on a thread of its own.
    pub fn with_replay_check(mut self, log: crate::replay::ReplayLog) -> Sim {
        self.replay = Some(log);
        self
    }

    /// A replay of the final log kept so far ([`Sim::with_replay_check`]), on its own thread, as the console's `replay`
    /// starts one; `None` without `--check-replay`.
    pub fn start_replay(&mut self) -> Option<std::io::Result<std::thread::JoinHandle<crate::replay::Verdict>>> {
        let log = self.replay.as_ref()?;
        let job = log.job(&*self.replica);
        let stack = crate::replay::MIN_STACK.max(crate::net::runner_settings().0.thread_stack());
        self.notices.push(format!("replay: replaying {} final cuts from frame 0 on a thread of its own", log.cuts()));
        Some(crate::replay::spawn(job, stack))
    }

    /// `fault` in place of this process's armed one ([`crate::fault::armed`]): a test gives one member a fault, which
    /// its replica injects too (`crate::world::ball_pit_replica_with`).
    #[cfg(test)]
    pub(crate) fn with_fault(mut self, fault: Arc<crate::fault::Fault>) -> Sim {
        self.fault = Some(fault);
        self
    }

    /// Runs pe-diverge for the local member (after [`Sim::with_local`]): step 1 routes the Repair lane to it, and
    /// step 5 hands it S3's events with the finalize results and routes its actions (app.md 3). It takes the physics
    /// self-test's start-up digest at its first step, on the simulation thread ([`crate::diverge`]). Fault bundles
    /// are written under `bundles` by a `pe-io` thread; `None` writes none. Without a local member, or if the thread
    /// cannot start, an error.
    pub fn with_diverge(
        mut self,
        cfg: pe_diverge::Config,
        bundles: Option<std::path::PathBuf>,
    ) -> std::io::Result<Sim> {
        let me = self.local_member().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "pe-diverge needs the local member (with_local)")
        })?;
        self.diverge = Some(crate::diverge::DivergeLoop::new(me, cfg, bundles)?);
        Ok(self)
    }

    /// A joiner's start (app.md 4, "B's state at E"; after [`Sim::with_diverge`]): pe-diverge fetches the state at
    /// agreement's final cut E, the cut that admitted this machine (`Node::join` makes it final at once), from
    /// `inviter` first, and imports it into the empty replica (divergence.md 4.5). Until the join ends nothing drives
    /// the replica: no hand entry or edit goes in, no vote, and no view goes out. When the import has matched E's root
    /// and finalize agreed there, the driver starts, the inviter is told (`JoinImported`), and the replica simulates on
    /// to the present. A failed join is [`Sim::join_failed`]: the joiner says so and quits.
    pub fn join_at_final(&mut self, inviter: MemberKey, now: HostTime) -> Result<(), String> {
        let e = self.agree.final_cut().cloned().ok_or("agreement has no final cut to join at")?;
        let d = self.diverge.as_mut().ok_or("a join needs pe-diverge (with_diverge)")?;
        d.d.join(&e, inviter, &mut *self.replica, now);
        self.joining = Some(inviter);
        self.notices.push(crate::join::joining_here());
        self.catching_up = true;
        self.set_status(Some(crate::join::catching_up_status()));
        Ok(())
    }

    /// The status line the window shows (pe-render's [`pe_render::present::Presenter::set_status`]): a join puts up
    /// [`crate::join::catching_up_status`] from its start until the replica first reaches the present.
    pub fn with_status(mut self, status: Arc<pe_render::notices::StatusLine>) -> Sim {
        self.status = Some(status);
        self
    }

    /// A rollback's cause, as far as this tick shows it (1.11; @coordinator-opus): the keyframe restored
    /// (`to`), how far the replica had stepped past it, the two causes S2 v1.11 reports (the frame whose entries
    /// changed, and the commit frame of my own code commit whose prediction ended), the entries agreement now holds at
    /// the changed frame, and the voids this tick reported (@design-reviewer's M1 on 6408fdbe). It goes to the cut log
    /// ([`crate::cutlog`]) for the self-test checker; a member alone also says it on the console, since nothing from
    /// another machine should ever roll it back.
    fn note_rollback(
        &mut self,
        advance: &Advance,
        stepped: Frame,
        present: Frame,
        now: HostTime,
        events: &[AgreeEvent],
    ) {
        let Some(to) = advance.rolled_back_to else { return };
        let short = |m: &MemberKey| m.0[..4].iter().map(|b| format!("{b:02x}")).collect::<String>();
        let me = self.local_member();
        let at: Vec<String> = (advance.changed_from.and_then(|f| self.agree.entries_at(f)).unwrap_or(&[]).iter())
            .map(|e| {
                let mine = if Some(e.entry.author) == me { "*" } else { "" };
                format!("{mine}{}:{:?}:{}", short(&e.entry.author), e.entry.class, e.entry.seq)
            })
            .collect();
        let voided: Vec<String> = (events.iter())
            .filter_map(|e| match e {
                AgreeEvent::Voided { author, reason, .. } => Some(format!("{}:{reason:?}", short(author))),
                _ => None,
            })
            .collect();
        let line = crate::cutlog::RollbackLine {
            to: to.0,
            depth: stepped.0.saturating_sub(to.0),
            present: present.0,
            changed: advance.changed_from.map(|f| f.0),
            replay: advance.replay_from.map(|f| f.0),
            at,
            voided,
        };
        self.stats.rollback_lines += 1;
        if let Some(log) = &mut self.cut_log {
            log.rollback(now, &line);
        }
        if self.agree.members().len() <= 1 {
            let frame = |f: Option<u64>| f.map_or_else(|| "-".to_string(), |f| f.to_string());
            self.notices.push(format!(
                "rollback: alone, stepped again from frame {}, {} frames back (present {}); entries changed at {} [{}], \
                 my commit's prediction ended at {}, voided this tick [{}]",
                line.to,
                line.depth,
                line.present,
                frame(line.changed),
                line.at.join(", "),
                frame(line.replay),
                line.voided.join(", ")
            ));
        }
    }

    /// Sends the console's `hand-mode` for this member's hands, and shows the `hand-mode` and `draw` pair on the status
    /// line once either was given (the pull-ahead plan's 1.10, [`crate::handfeel`]).
    pub fn with_hand_feel(mut self, feel: Arc<crate::handfeel::HandFeel>) -> Sim {
        self.hand_feel = Some(feel);
        self
    }

    /// Puts this simulation's own status up, or takes it down with `None`. Below it, the line shows the hand-feel
    /// pair once a command gave one.
    fn set_status(&mut self, text: Option<String>) {
        self.status_own = text;
        self.show_status();
    }

    fn show_status(&mut self) {
        if let Some(s) = &self.status {
            self.hand_feel_shown = self.hand_feel.as_ref().map_or(0, |f| f.version());
            s.set(self.status_own.clone().or_else(|| self.hand_feel.as_ref().and_then(|f| f.status())));
        }
    }

    /// Tests only: freeze and resync after `secs` slow instead of a minute ([`crate::slow::FREEZE_AFTER_S`]).
    #[cfg(test)]
    pub(crate) fn with_freeze_after(mut self, secs: f64) -> Sim {
        self.slow.freeze_after_s = Some(secs);
        self
    }

    /// Records every tick in `trace` ([`crate::trace`]): off unless the binary was given `PE_TICK_TRACE`.
    pub fn with_tick_trace(mut self, trace: crate::trace::TickTrace) -> Sim {
        self.trace = Some(trace);
        self
    }

    /// Writes each final cut's root and this machine's result there to `log` ([`crate::cutlog`]): off unless the binary
    /// was given `PE_CUT_LOG`.
    pub fn with_cut_log(mut self, log: crate::cutlog::CutLog) -> Sim {
        self.cut_log = Some(log);
        self
    }

    /// What a failed join does besides its console line, once: the binary quits.
    pub fn with_on_join_failed(mut self, f: Box<dyn FnOnce() + Send>) -> Sim {
        self.on_join_failed = Some(f);
        self
    }

    /// Whether a join's import is under way ([`Sim::join_at_final`]).
    pub fn joining(&self) -> bool {
        self.joining.is_some()
    }

    /// Whether the replica holds no state of its own yet: a join's import is under way, or failed (the joiner quits,
    /// and its empty replica is never driven).
    fn importing(&self) -> bool {
        self.joining.is_some() || self.join_failed.is_some() || self.resyncing
    }

    /// Whether a resync's import is under way (divergence.md 4.6): the member is frozen.
    pub fn resyncing(&self) -> bool {
        self.resyncing
    }

    /// After step 4: how far behind the clock this member is (divergence.md 4.6). The status line says so while it is
    /// behind by half a second or more; slow for a minute, it freezes and resyncs at the newest final cut, which at
    /// `n` >= 3 the others certified without it. At `n = 2` every cut needs this member's own vote, so no newer
    /// final cut exists: it says so and keeps trying to catch up.
    fn watch_lag(&mut self, p: Frame, now: HostTime) {
        use crate::slow::Verdict;
        let rate = f64::from(self.clock.rate_hz().max(1));
        let lag_s = (p.0 + 1).saturating_sub(self.replica.frame().0) as f64 / rate;
        let verdict = self.slow.observe(now, lag_s);
        self.stats.lag_max_ms = (self.slow.lag_max_s * 1000.0) as u64;
        self.stats.slow_ms = self.slow.slow_ns / 1_000_000;
        if !self.slow.slow() {
            self.cant_resync = false;
        }
        match verdict {
            Verdict::Run { behind_s } if !self.catching_up => {
                let text = behind_s.map(|b| {
                    if self.cant_resync && self.agree.members().len() <= 2 {
                        crate::slow::cant_resync_status(b)
                    } else {
                        crate::slow::behind_status(b)
                    }
                });
                self.set_status(text);
            }
            Verdict::Run { .. } => {}
            Verdict::Freeze { behind_s } => self.start_resync(behind_s, now),
        }
    }

    /// A minute slow (F3): nobody freezes, at any `n` (the user's rulings, "players should never have
    /// their actions interrupted", and; @coordinator-opus). The member keeps playing and
    /// is told once per slow stint. At `n = 2` every cut needs its vote, so its status says the session confirms late;
    /// at `n >= 3` the others confirm without it. The automatic catch-up that doesn't pause it (a snapshot plus a merged
    /// package) is F3's package work; `Diverge::resync` is its fetch, not started here yet.
    fn start_resync(&mut self, behind_s: f64, now: HostTime) {
        self.slow.resync_ended(now);
        if self.cant_resync {
            return;
        }
        self.cant_resync = true;
        let members = self.agree.members().len();
        self.notices.push(if members <= 2 {
            format!(
                "slow: this machine has been {behind_s:.0} s behind real time for a minute. Alone or with two players \
                 every confirmation needs its own vote, so the session confirms at its pace; it keeps playing and \
                 catching up"
            )
        } else {
            format!(
                "slow: this machine has been {behind_s:.0} s behind real time for a minute. It keeps playing and \
                 catching up; the others confirm without its vote"
            )
        });
    }

    /// After step 5: a resync's import ended. Imported, the member steps on from the cut and catches up; failed, it
    /// runs on, and if it is still slow it resyncs again a minute later.
    fn check_resync(&mut self, now: HostTime) {
        if !self.resyncing {
            return;
        }
        let state = self.diverge.as_ref().and_then(|d| d.d.join_state().cloned());
        let line = match state {
            Some(pe_diverge::Join::Joined { cut }) => {
                format!("resync: imported the world at frame {}; catching up to the present", cut.frame.0)
            }
            Some(pe_diverge::Join::Failed { why, .. }) => {
                format!("resync: failed ({why}); trying again if still behind")
            }
            _ => return,
        };
        self.resyncing = false;
        self.slow.resync_ended(now);
        self.notices.push(line);
    }

    /// Why the join failed, if it did: the joiner quits.
    pub fn join_failed(&self) -> Option<&str> {
        self.join_failed.as_deref()
    }

    /// After step 5: a join's import ended. Joined starts the driver from the next tick and tells the inviter;
    /// failed stops here for good.
    fn check_join(&mut self) {
        let Some(inviter) = self.joining else { return };
        let state = self.diverge.as_ref().and_then(|d| d.d.join_state().cloned());
        let failed = match state {
            Some(pe_diverge::Join::Joined { .. }) => match self.imported_members_differ() {
                // logic.md 6.2a's voter snapshot takes the voters from the world, and in M1 every member votes, so
                // the imported world's governance members must be agreement's (app.md 4; @design-reviewer), as genesis checks its own (`GenesisError::Governance`).
                Some(why) => why,
                None => {
                    self.joining = None;
                    if let Some(net) = &mut self.net {
                        net.send(inviter, Lane::Protocol, crate::join::AppMsg::JoinImported.encode().into());
                    }
                    self.notices.push(crate::join::imported_here());
                    return;
                }
            },
            Some(pe_diverge::Join::Failed { why, .. }) => why,
            _ => return,
        };
        self.joining = None;
        self.notices.push(crate::join::couldnt_join_here(&failed));
        self.join_failed = Some(failed);
        self.catching_up = false;
        self.set_status(None);
        if let Some(f) = self.on_join_failed.take() {
            f();
        }
    }

    /// Why an imported world can't be joined: its governance members aren't agreement's (the start's). `None` when
    /// they are, or when the replica isn't a pe-world one (nothing to compare).
    fn imported_members_differ(&self) -> Option<String> {
        let world = crate::api::snapshot_of(&*self.replica)?;
        let governance: Vec<MemberKey> = world.members().map(|(k, _)| k).collect();
        let agree = self.agree.members();
        (governance != agree).then(|| {
            let short = |ks: &[MemberKey]| ks.iter().map(|k| crate::leave::player(*k)).collect::<Vec<_>>().join(", ");
            format!("the imported world's members ({}) aren't agreement's ({})", short(&governance), short(&agree))
        })
    }

    /// pe-diverge's counters (divergence.md 7), if it runs.
    pub fn diverge_counters(&self) -> Option<&pe_diverge::Counters> {
        self.diverge.as_ref().map(|d| d.counters())
    }

    /// The verdict the next votes carry (`NotRun` until a root split asks for a self-test).
    pub fn self_test(&self) -> SelfTest {
        self.self_test
    }

    /// This machine's member, if there is a local player.
    pub fn local_member(&self) -> Option<MemberKey> {
        self.local.as_ref().map(|l| l.chain.me())
    }

    /// The local player's chain and the seq of its last entry agreement took, if there is a local player.
    pub fn local_head(&self) -> Option<(ChainId, u64)> {
        self.local.as_ref().map(|l| l.chain.head())
    }

    /// Agreement's final cut, once one is (S3's `EntrySource::final_cut`).
    pub fn final_cut(&self) -> Option<Cut> {
        self.agree.final_cut().map(|f| f.cut.clone())
    }

    /// The local player `me`: a new chain on agreement's line from `nonce` (`local::chain_nonce`), and the hands
    /// whose inputs step 2 submits each tick. A restart makes a new chain under the same key, which pe-agree v0 does
    /// not take yet (agreement.md 8): the first restart test against pe-agree meets it (@design-reviewer's nit).
    pub fn with_local(mut self, me: MemberKey, nonce: [u8; 32], hands: Arc<dyn HandFeed>) -> Sim {
        self.local = Some(Local {
            chain: Chain::new(self.agree.line(), me, nonce),
            hands,
            grasp: local::Grasp::default(),
            grabs: BTreeMap::new(),
            released: BTreeMap::new(),
        });
        self
    }

    pub fn stats(&self) -> &SimStats {
        &self.stats
    }

    /// Agreement's own statistics (S3's `stats`).
    pub fn agree_stats(&self) -> pe_seams::AgreeStats {
        self.agree.stats()
    }

    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    /// Agreement's final cut's root (S3's `final_cut`), for tests.
    #[cfg(test)]
    pub(crate) fn final_root(&self) -> Option<pe_seams::Hash> {
        self.agree.final_cut().map(|fc| fc.root)
    }

    /// The replica's present world as pe-world's snapshot, for tests.
    #[cfg(test)]
    pub(crate) fn world(&self) -> Option<Arc<pe_world::WorldSnapshot>> {
        crate::api::snapshot_of(&*self.replica)
    }

    /// The newest view this simulation published, for tests.
    #[cfg(test)]
    pub(crate) fn published(&self) -> Option<Arc<FrameView>> {
        self.published.clone()
    }

    /// The replica's root at `frame`, a frame of its cut grid it still keeps, through S2's `root_at_cut` on a cut with
    /// no entries: the keep-up floor's test compares it (the floor changes how fast, never what is computed).
    #[cfg(test)]
    pub(crate) fn root_at(&mut self, frame: Frame) -> Result<pe_seams::Hash, pe_seams::RootError> {
        let cut = Cut { at: CutRef { line: self.agree.line(), height: 1, frame }, heads: Vec::new() };
        self.replica.root_at_cut(&cut, &mut *self.agree)
    }

    /// One tick at host time `now` (app.md 3): the frame it ticked and what the driver did, or `None` before frame 0.
    pub fn tick(&mut self, now: HostTime) -> Option<(Frame, DriveReport)> {
        self.stats.ticks += 1;
        let Some(p) = self.clock.frame_at(now) else {
            self.stats.before_frame_0 += 1;
            return None;
        };
        // B (app.md 3, [`crate::shown`]): the shown present, the moment the headset shows plus two periods while an XR
        // session draws, else the present. The hands, stamps and the replica's target go there; agreement, the links
        // and pe-diverge stay on the present, the group's clock.
        let ps = self.shown_present(p, now);
        let tick_start = Instant::now();
        let mut mark = tick_start;
        // The tick trace's counters at the start (crate::trace); nothing at all without one.
        let traced = self.trace.as_ref().map(|_| crate::trace::Before::take(&self.stats.phases, self.stats.received));
        // 1. Messages in. Protocol and Action go to agreement, Repair to pe-diverge; a refusal is a peer's fault,
        // counted and never fatal. TODO(blob store): Bulk; Pose is display-only.
        let mut links = Vec::new();
        if let Some(net) = &mut self.net {
            for _ in 0..MAX_IN_PER_TICK {
                let Some(r) = net.poll() else { break };
                // pe-app's own messages (the join's) ride the Protocol lane under their own tag: the hook takes them
                // before agreement could count them as a peer's bad bytes.
                if r.lane == Lane::Protocol
                    && let Some(hook) = &mut self.net_hook
                    && hook.claim(r.from, &r.bytes, now, &*self.agree)
                {
                    self.stats.app_in += 1;
                    continue;
                }
                match r.lane {
                    Lane::Protocol | Lane::Action => match self.agree.receive(r.from, r.lane, &r.bytes, r.at) {
                        Ok(()) => self.stats.received += 1,
                        Err(_) => self.stats.refused += 1,
                    },
                    Lane::Repair if self.diverge.is_some() => {
                        let d = self.diverge.as_mut().expect("checked above");
                        match d.receive(r.from, &r.bytes, &mut *self.replica, &mut *self.agree, now, &*self.clock) {
                            Ok(()) => self.stats.repair_in += 1,
                            Err(_) => self.stats.repair_refused += 1,
                        }
                    }
                    _ => self.stats.unrouted += 1,
                }
            }
            for e in net.events() {
                self.stats.net_events += 1;
                if let (Some(api), NetEvent::LinkUp { peer } | NetEvent::LinkDown { peer }) = (&self.api, &e) {
                    api.ports.shared.link(*peer, matches!(e, NetEvent::LinkUp { .. }));
                }
                if let Some(hook) = &mut self.net_hook {
                    hook.on_event(&e, &mut *self.agree, &ReplicaFacts(&*self.replica, self.diverge.as_ref()));
                }
                match e {
                    NetEvent::LinkUp { peer } => links.push((peer, true)),
                    NetEvent::LinkDown { peer } => links.push((peer, false)),
                    _ => {}
                }
            }
            if let Some(hook) = &mut self.net_hook {
                hook.on_tick(now, &mut *self.agree, &ReplicaFacts(&*self.replica, self.diverge.as_ref()));
            }
        }
        for (peer, up) in links {
            if up {
                self.down.remove(&peer);
            } else {
                self.link_down(peer, p);
            }
        }
        self.stats.phases.ingest_ns += lap(&mut mark);
        // 2. Local input: both hands at frame `p`'s host time, through the newest view's rig, one input entry each.
        // Before the first view the hands' objects are not known yet, so nothing goes in. Once per frame: a wake (step
        // 7) ticks again within a frame, and that tick takes in messages and re-simulates but adds no second set of
        // inputs for the frame, which would count against the rate budget.
        // Hands are never held (F3 S1; the user's: a hand freeze of any length is unacceptable). While
        // agreement reports this member backlogged (S3 v1.5), its pose-only hand entries are thinned to
        // [`HAND_FLOOR_HZ`] and no further, and a grip's change always goes, with its pose. Local hands keep applying
        // every frame. Edits never come through here, so nothing thins them.
        // The frame's edit goes first, before its hand entries, so that a dry run made while pe-api's queue was empty
        // names the seq the edit gets (app.md 3.1). One a frame, never paced: an edit is never void as late for
        // being held back, and the queue's wake (step 7) brings its frame's tick.
        // Stamp-ahead (app.md 3.2): the lead follows the other members' reports, and the frame's entries go that far
        // ahead of the present, never below the chain's last stamp. The speed test reads both hands at `p`'s host time
        // once a frame: while either moves, the lead sheds at most one frame in 11. The pose clock then says which
        // moment the frame's poses carry, so a shed is spread over the stamps up to the next. A member that is leaving
        // sends no more hands, so it reads none.
        // A member frozen for a resync makes no hand entries (divergence.md 4.6).
        let fresh = self.leaving == Leaving::Staying && !self.resyncing && self.input_frame.is_none_or(|f| ps > f);
        if let (true, Some(_), Some(l), Some(view)) = (fresh, &self.lead, &self.local, &self.published) {
            // A frame's view is the state at its end (the replica's frame after the step), so the hands go in at the
            // end of the frame they are stamped for: the simulated hand is then where the real one is at that moment.
            let at = self.clock.host_time_of(Frame(ps.0 + 1));
            for (i, side) in [Hand::Left, Hand::Right].into_iter().enumerate() {
                let pose = l
                    .hands
                    .hand_at(side, at)
                    .filter(|h| h.tracked)
                    .and_then(|h| local::world_pose(&view.local_rig, &h).map(|w| (w.pos, w.rot, h.discontinuity)));
                self.motion.observe(ps, i, pose);
            }
            self.stats.moving_frames += u64::from(self.motion.moving());
        }
        let moving = self.motion.moving();
        let (stamp, pose_time) = match (&mut self.lead, &self.local) {
            (Some(lead), Some(l)) => {
                let reports: Vec<(i64, u64)> = self.agree.own_lateness().iter().map(|r| (r.frames, r.seq)).collect();
                self.stats.lead = lead.update(ps, &reports, l.chain.head().1 + 1, moving);
                (self.stats.largest_lead, self.stats.longest_drain) = (lead.largest(), lead.longest_drain());
                let stamp = lead.stamp(ps, l.chain.last_stamp());
                (stamp, fresh.then(|| lead.pose_time(ps, stamp, moving)))
            }
            _ => (ps, None),
        };
        // The graceful quit (app.md 4): once asked, this member's `Leave` goes out first in the frame, at the frame's
        // stamp (never below the chain's last, which a lead may hold above the present), and it is the chain's last
        // entry. No edit or hand entry follows it, and an edit still queued is refused as agreement would refuse it.
        if self.leaving == Leaving::Asked
            && !self.net_hook.as_ref().is_some_and(|h| h.holds_leave())
            && let Some(l) = &mut self.local
        {
            let leave = l.chain.entry(self.replica.domain(), stamp, Class::Protocol, pe_seams::LEAVE.to_vec());
            match self.agree.submit(leave) {
                Ok(signed) => {
                    l.chain.took(&signed);
                    self.leaving = Leaving::Left { stamp };
                }
                Err(_) => self.stats.leave_refused += 1,
            }
        }
        let leaving = self.leaving != Leaving::Staying;
        // A join this member admits holds script commits back from the joiner's ready (app.md 4). Read after step 1,
        // which may have just made the joiner ready, so pe-api's check and this frame's drain below agree with the
        // `Admit` written next.
        let holds_commits = self.net_hook.as_ref().is_some_and(|h| h.holds_script_commits());
        if let Some(api) = &self.api {
            api.ports.shared.joining(holds_commits);
        }
        // The network hook's entry (the inviter's `Admit`, app.md 4), first after a `Leave` would be and never with
        // one: at the frame's stamp, a class Protocol entry of this member's chain. Its check that the world holds no
        // scripts also counts this member's own script commit stamped ahead and not yet stepped.
        let unstepped_script = self.script_stamp.is_some_and(|f| f >= self.replica.frame());
        if !leaving
            && let (Some(hook), Some(l)) = (&mut self.net_hook, &mut self.local)
            && let Some(payload) = hook.take_entry(
                &AdmitFacts(ReplicaFacts(&*self.replica, self.diverge.as_ref()), unstepped_script),
                &*self.agree,
                now,
            )
        {
            let entry = l.chain.entry(self.replica.domain(), stamp, Class::Protocol, payload);
            let written = self.agree.submit(entry).map(|signed| l.chain.took(&signed));
            match written {
                Ok(()) => self.stats.hook_entries += 1,
                Err(_) => self.stats.hook_entries_refused += 1,
            }
            hook.entry_written(written, now, &*self.agree);
        }
        if leaving && let Some(api) = &mut self.api {
            for (ticket, _) in api.ports.queue.drain(usize::MAX) {
                api.ports.queue.settle(ticket, Receipt::Refused(pe_seams::agree::SubmitError::AfterLeave));
            }
        }
        if let (false, false, Some(l), Some(api)) = (leaving, self.importing(), &mut self.local, &mut self.api)
            && api.edit_frame.is_none_or(|f| p > f)
        {
            api.edit_frame = Some(p);
            for (ticket, mut sub) in api.ports.queue.drain(1) {
                let script = sub.tx.ops.iter().any(|o| matches!(o, pe_world::edit::Op::CommitScript { .. }));
                // pe-api checks the window too, but a commit it checked before the joiner's ready can reach this
                // drain after it: held back here, nothing sent (app.md 4).
                if script && holds_commits {
                    api.ports.queue.settle(ticket, Receipt::Unavailable(pe_api::api::JOINING.into()));
                    continue;
                }
                // A code commit's `activate_at` is the frame its entry is stamped at, which only this machine knows:
                // the present plus the lead. pe-api leaves it at 0, and pe-world refuses a commit whose `activate_at`
                // is not its stamp (api.md 7.1, world-model.md 7.9).
                sub.tx.set_activation(stamp);
                let payload = sub.tx.encode();
                let bytes = payload.len();
                let entry = l.chain.entry(self.replica.domain(), stamp, Class::Edit, payload);
                let receipt = match self.agree.submit(entry) {
                    Ok(signed) => {
                        l.chain.took(&signed);
                        self.stats.edits += 1;
                        if script {
                            self.script_stamp = Some(stamp);
                        }
                        // Indexed now, not only when a view's record names it, so `edit()` answers for this machine's
                        // own change at once (@api-eng's Low on slice 1).
                        api.ports.shared.index_edit(signed.id(), Arc::new(sub.tx), bytes);
                        let (chain, seq) = l.chain.head();
                        Receipt::Submitted { change: TxId::from(signed.id()), frame: stamp.0, chain, seq }
                    }
                    Err(e) => {
                        self.stats.edits_refused += 1;
                        Receipt::Refused(e)
                    }
                };
                api.ports.queue.settle(ticket, receipt);
            }
        }
        let floor_gap = (u64::from(self.clock.rate_hz()) / HAND_FLOOR_HZ).max(1);
        let floor_thin =
            fresh && self.agree.backlogged() && self.pose_frame.is_some_and(|f| ps.0 < f.0.saturating_add(floor_gap));
        if let (true, Some(l), Some(view)) = (fresh, &mut self.local, &self.published) {
            {
                // At the end of the stamped frame, as the speed test above reads them.
                let at = pose_time
                    .map_or_else(|| self.clock.host_time_of(Frame(ps.0 + 1)), |t| host_time_at(&*self.clock, t + 1.0));
                let domain = self.replica.domain();
                // While the lead sheds, a frame's stamp can stand still: its poses would only replace the last frame's.
                // While backlogged, only every `floor_gap`-th frame's poses go (F3 S1).
                let thin = self.input_stamp == Some(stamp) || floor_thin;
                if floor_thin {
                    self.stats.hands_floor_thinned += 1;
                } else if self.input_stamp != Some(stamp) {
                    self.pose_frame = Some(ps);
                }
                let sent = local::submit_hands(
                    &mut l.chain,
                    &mut l.grasp,
                    &*l.hands,
                    &WorldGrasps(&*self.replica),
                    view,
                    at,
                    now,
                    stamp,
                    domain,
                    &mut *self.agree,
                    thin,
                );
                self.stats.inputs += u64::from(sent.took);
                self.stats.inputs_refused += u64::from(sent.refused);
                self.stats.poses_thinned += u64::from(sent.thinned);
                for (i, h) in self.stats.hands.iter_mut().enumerate() {
                    h.closes += u64::from(sent.closes[i]);
                    h.opens += u64::from(sent.opens[i]);
                    h.releases += u64::from(sent.releases[i]);
                    h.throws += u64::from(sent.throws[i]);
                }
                // Each throw, with the velocity it carries and the fastest the object moved with the hand over the
                // 50 ms up to the opening (the weak-throw fix): a window can be tuned from these with no format change.
                for t in &sent.thrown {
                    let speed = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                    crate::out!(
                        "throw: {} hand, object {:?}, {:.2} m/s (peak {:.2} m/s over 50 ms), spin {:.1} rad/s, stamp {}",
                        ["left", "right"][t.hand],
                        t.object,
                        speed(t.linear),
                        t.peak,
                        speed(t.angular),
                        stamp.0
                    );
                }
                for (i, id) in sent.grabs {
                    self.stats.hands[i].grabs += 1;
                    l.grabs.insert(id, (i, None));
                }
                for (id, object) in sent.released {
                    l.released.insert(id, (object, stamp, false));
                }
            }
            self.input_frame = Some(ps);
            self.input_stamp = Some(stamp);
        }
        // The console's `hand-mode` (the pull-ahead plan's 1.10, crate::handfeel): the mode asked for, for both of this
        // member's hands, at the frame's stamp. It waits while no hand of this member is in the view yet, and goes
        // again, to both hands, at the next tick if agreement refused either (`TooLate` on a member behind, or
        // `BelowOwnStamp`): sending the mode again to a hand that has it changes nothing (@design-reviewer's L1). A
        // member that is leaving sends none.
        if let (false, Some(feel), Some(l), Some(view)) = (leaving, &self.hand_feel, &mut self.local, &self.published)
            && let Some(mode) = feel.take_pending()
        {
            let sent =
                local::submit_hand_mode(&mut l.chain, view, mode, stamp, self.replica.domain(), &mut *self.agree);
            if sent.took == 0 || sent.refused > 0 {
                feel.put_back(mode);
            }
            self.stats.hand_modes += u64::from(sent.took);
            self.stats.hand_modes_refused += u64::from(sent.refused);
        }
        if self.hand_feel.as_ref().is_some_and(|f| f.version() != self.hand_feel_shown) {
            self.show_status();
        }
        if let (Some(l), Some(api)) = (&self.local, &self.api) {
            let (chain, last) = l.chain.head();
            api.ports.shared.draft(chain, last + 1);
        }
        self.stats.phases.inputs_ns += lap(&mut mark);
        // 3. Agreement's time. Its messages go to the network's outbox; a full queue is pe-net's local fault, never
        // a stall (transport.md 7). A world alone has nowhere to send them.
        let out = self.agree.tick(now, &*self.clock);
        match &mut self.net {
            Some(net) => {
                self.stats.sent += out.len() as u64;
                for o in out {
                    net.send(o.to, o.lane, o.bytes);
                }
            }
            None => self.stats.unsent += out.len() as u64,
        }
        self.stats.phases.agree_ns += lap(&mut mark);
        // 4. The replica, through pe-replica's driver. `present` is exclusive, so frame `p` is simulated. First the
        // budget's keep-up floor: what the last call needed to reach its present, rollback included, plus the frames
        // `p` advanced since the previous tick, plus one, capped (`Budget`). A joiner's replica is empty until its import
        // ends, so nothing drives it then (`join_at_final`).
        // The console's typed `fault` (`--arm-fault`, crate::fault): it fires at the next frame the replica steps.
        if !self.importing()
            && let Some(at) = self.fault.as_ref().and_then(|f| f.take_request(self.replica.frame()))
        {
            self.notices.push(format!("FAULT: the test fault fires at the end of frame {} (--arm-fault)", at.0));
        }
        let stepped_before = self.replica.frame();
        let report = if self.importing() { DriveReport::default() } else { self.drive(ps) };
        if !self.importing() {
            self.watch_lag(p, now);
        }
        // `PE_CUT_LOG` (crate::cutlog): each finalize result with the root its cut is final with. The driver
        // finalizes only `final_cut()`, so a result is always for it. The poll flushes within a second even when no
        // cut follows.
        if let Some(log) = &mut self.cut_log {
            if let Some(fc) = self.agree.final_cut() {
                for (cut, f) in report.finalized.iter().filter(|(c, _)| *c == fc.cut.at) {
                    log.record(now, cut, &fc.root, f);
                }
            }
            log.poll(now);
        }
        self.stats.phases.drive_ns += lap(&mut mark);
        if let Some(line) = self.fault.as_ref().and_then(|f| f.take_announcement()) {
            self.notices.push(line);
        }
        // 5. S3's events, and pe-diverge: one drain after `drive` gives the spec's ordered list (S3's queue keeps its
        // order), with each finalize result placed after its cut's `CutFinal` ([`crate::diverge`]). Its actions are
        // routed in this tick, and its Repair-lane messages go to the outbox.
        let events = self.agree.events();
        self.stats.events += events.len() as u64;
        self.track_split(&events);
        self.note_rollback(&report.advance, stepped_before, p, now, &events);
        self.stats.recasts = self.agree.stats().recasts;
        self.stats.phases.events_ns += lap(&mut mark);
        if let Some(d) = &mut self.diverge {
            let out = d.step(
                &events,
                &report.finalized,
                &mut *self.replica,
                &mut *self.agree,
                now,
                &*self.clock,
                &mut self.self_test,
                self.fault.as_deref(),
                &mut self.stats,
                &mut self.notices,
            );
            match &mut self.net {
                Some(net) => {
                    self.stats.repair_out += out.len() as u64;
                    for o in out {
                        net.send(o.to, o.lane, o.bytes);
                    }
                }
                None => self.stats.unsent += out.len() as u64,
            }
        }
        self.check_join();
        self.check_resync(now);
        // `--check-replay` (crate::replay): a replay the console asked for.
        if self.replay.is_some()
            && crate::replay::take_request()
            && let Some(Err(e)) = self.start_replay()
        {
            self.notices.push(format!("replay: couldn't start the replay's thread: {e}"));
        }
        self.stats.phases.diverge_ns += lap(&mut mark);
        // A proposal opens each view: the quit's bound follows the smoothed interval between them (app.md 4).
        for _ in events.iter().filter(|e| matches!(e, AgreeEvent::Proposed { .. })) {
            self.view_time.proposed(now);
        }
        if let Some(api) = &mut self.api {
            api.voided.extend(events.iter().filter_map(|e| match e {
                AgreeEvent::Voided { entry, reason, .. } => Some((*entry, *reason)),
                _ => None,
            }));
            // On `final_cut`, never on `CutFinal`: a cut's voids are queued no later than the call in which
            // `final_cut` first reports it (S3 v1.6), so they are in hand here.
            if let Some(fc) = self.agree.final_cut()
                && api.confirmed.as_ref() != Some(&fc.cut.at)
            {
                api.ports.log.confirm_cut(&fc.cut, &api.voided);
                api.voided.clear();
                api.confirmed = Some(fc.cut.at);
            }
        }
        if let Some(log) = &self.event_log {
            log.lock().expect("the event log").extend(events);
        }
        self.stats.phases.events_ns += lap(&mut mark);
        // 6. The replica's newest view, at the newest frame it completed (S2 v1.10). While it re-simulates the view
        // holds, so nothing new goes out. It carries the composed health, so pe-render's "resyncing" line and
        // session_info read one value (@design-reviewer's Low): while a root split is open, a copy of the view with
        // it. When only the health changed (a split opened or closed while the replica's view held), the copy is a
        // publication with nothing new since the last ([`health_only`]): readers that take each publication's records
        // in order see none twice.
        let view = self.replica.view();
        let health = self.health_of(&view);
        let same_view = self.source_view.as_ref().is_some_and(|v| Arc::ptr_eq(v, &view));
        // Its rollbacks since the view before it: the run's deepest (G9).
        self.stats.deepest_rollback = self.stats.deepest_rollback.max(view.counters.deepest_rollback);
        // A joiner's empty replica has nothing to show until its import ends (app.md 4).
        if !self.importing() && (!same_view || self.published.as_ref().is_some_and(|v| v.health != health)) {
            let mut out = if same_view {
                Arc::new(health_only(&view, health))
            } else if view.health == health {
                view.clone()
            } else {
                Arc::new(FrameView { health, ..(*view).clone() })
            };
            if let Some(api) = &self.api {
                self.feed_api(api, &out);
            }
            if let Some(l) = &mut self.local {
                l.grab_fates(&out.changes, &mut self.stats.hands);
                // What this member's own `Release`s let go of in these steps: the display hands it off rather than
                // correcting it (render.md 8, D4).
                let let_go = l.let_go(&out, self.clock.rate_hz());
                if !let_go.is_empty() {
                    out = Arc::new(released_local(&out, &let_go));
                }
            }
            self.slot.publish(out.clone());
            self.published = Some(out);
            self.source_view = Some(view);
            self.stats.published += 1;
        }
        self.stats.phases.publish_ns += lap(&mut mark);
        // How far finality trails the present at the end of this tick (G9): about how long an action taken now takes
        // to become final. Not while a joiner imports: it has no history to finalize yet.
        if !self.importing()
            && let Some(fc) = self.agree.final_cut()
        {
            self.stats.final_lag.record(p.0.saturating_sub(fc.cut.at.frame.0));
        }
        let tick = phases::since(tick_start);
        self.stats.phases.tick_ns += tick;
        self.stats.phases.longest_tick_ns = self.stats.phases.longest_tick_ns.max(tick);
        if let (Some(t), Some(before)) = (self.trace.as_mut(), traced) {
            let at_end = crate::trace::Tick {
                p,
                ps,
                now,
                tick_ns: tick,
                rolled_back_to: report.advance.rolled_back_to,
                caught_up: report.advance.caught_up,
                frame: self.replica.frame(),
                final_frame: self.agree.final_cut().map(|fc| fc.cut.at.frame),
                budget: self.budget.frames(),
                cost_ns: self.budget.cost().map(|c| u64::try_from(c.as_nanos()).unwrap_or(u64::MAX)),
                backlogged: self.agree.backlogged(),
                holds: self.stats.holds,
                inputs: self.stats.inputs,
                refused: self.stats.inputs_refused,
                thinned: self.stats.hands_floor_thinned,
                published: self.stats.published,
            };
            t.record(&before, &self.stats.phases, self.stats.received, at_end);
        }
        Some((p, report))
    }

    /// B's shown present for the present `p` at `now` (app.md 3, [`crate::shown`]): `p` itself without a display lead.
    fn shown_present(&mut self, p: Frame, now: HostTime) -> Frame {
        let Some(lead) = &self.shown else { return p };
        let period = i64::try_from(1_000_000_000 / u64::from(self.clock.rate_hz().max(1))).unwrap_or(i64::MAX);
        let ahead = crate::shown::ahead_ns(lead.p95(now), self.display_delay, period);
        let ps =
            if ahead > 0 { self.clock.frame_at(HostTime(now.0.saturating_add(ahead))).unwrap_or(p).max(p) } else { p };
        self.stats.shown_ahead = ps.0 - p.0;
        self.stats.largest_shown_ahead = self.stats.largest_shown_ahead.max(self.stats.shown_ahead);
        self.stats.ticks_shown_ahead += u64::from(ps > p);
        ps
    }

    /// Step 4: the replica through pe-replica's driver, to `p` included.
    fn drive(&mut self, p: Frame) -> DriveReport {
        if let Some(b) = self.budget.at_present(p) {
            self.replica.set_budget(b);
        }
        // pe-diverge replays to a split's whole cut, and a `RootSplit` names only its `CutRef`: the cuts the driver is
        // about to answer are kept for it. Nothing touches agreement between here and `drive`, so this is its list.
        if let Some(d) = &mut self.diverge {
            d.note_cuts(&*self.agree);
        }
        let started = Instant::now();
        let report = {
            // Times the replica's calls into `phases`, and passes each through unchanged.
            let log = self.replay.as_mut();
            let mut timed = Timed { inner: &mut *self.replica, times: &mut self.stats.phases, log };
            self.driver.drive(&mut timed, &mut *self.agree, Frame(p.0 + 1), self.self_test)
        };
        let frames = report.advance.resimulated + report.advance.stepped;
        if let Some(b) = self.budget.observe(frames, started.elapsed()) {
            self.replica.set_budget(b);
        }
        // The next keep-up floor: what reaching the present asked of this call, rollback included.
        self.budget.called(frames, (p.0 + 1).saturating_sub(self.replica.frame().0));
        self.stats.votes += report.votes.len() as u64;
        self.stats.finalized += report.finalized.len() as u64;
        if let Some(api) = &self.api {
            if let Some(k) = report.advance.rolled_back_to {
                api.ports.shared.rolled_back(k);
            }
            api.ports.shared.caught_up(report.advance.caught_up);
        }
        self.count_hold(report.advance.caught_up);
        if self.catching_up && self.joining.is_none() && report.advance.caught_up {
            self.catching_up = false;
            self.slow.caught_up(self.clock.host_time_of(p));
            self.set_status(None);
        }
        report
    }

    /// Opens a root split on S3's `RootSplit` for a cut above the final cut's frame, and closes every split at or
    /// below the final cut's frame: votes reached a quorum on one root there (divergence.md 5, step 4). The episode
    /// lasts while any is open.
    fn track_split(&mut self, events: &[AgreeEvent]) {
        let final_frame = self.agree.final_cut().map(|fc| fc.cut.at.frame);
        let was_open = !self.splits.is_empty();
        for e in events {
            if let AgreeEvent::RootSplit { cut, .. } = e
                && final_frame.is_none_or(|f| cut.frame > f)
            {
                self.splits.insert(cut.frame);
            }
        }
        if let Some(f) = final_frame {
            self.splits.retain(|&s| s > f);
        }
        match (was_open, !self.splits.is_empty()) {
            (false, true) => self.stats.root_splits += 1,
            (true, false) => self.stats.root_splits_ended += 1,
            _ => {}
        }
    }

    /// The health this machine reports (session_info's): the replica's, and `Repairing` from the earliest frame
    /// while a root split is open or pe-diverge has an episode open ([`pe_diverge::Diverge::open_since`]). The
    /// replica only learns of a divergence at a final cut, and in a split none is final (divergence.md 7;
    /// @coordinator-opus's ruling on @item:primordial-engine/pe-app-runs-pe-diverge-app-md-step-5-and-health-r).
    fn health_of(&self, view: &FrameView) -> Health {
        let open = self.diverge.as_ref().and_then(|d| d.d.open_since());
        let open = match (self.splits.first().copied(), open) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match (view.health, open) {
            (h, None) => h,
            (Health::Agreed, Some(s)) => Health::Repairing { since: s },
            (Health::Repairing { since }, Some(s)) => Health::Repairing { since: since.min(s) },
        }
    }

    /// The health of the newest view published ([`Sim::health_of`] its replica's view), `Agreed` before the first.
    pub fn health(&self) -> Health {
        self.published.as_deref().map_or(Health::Agreed, |v| v.health)
    }

    /// Step 6 for pe-api (app.md 3.1): the view's change records, the present world, the world at `Cv`, and the
    /// transactions of the edit entries the records name, each decoded once.
    fn feed_api(&self, api: &ApiLoop, view: &FrameView) {
        let shared = &api.ports.shared;
        // Activations first: the publication's final frame then tells those it passes (api.md 7).
        api.ports.log.activations(&view.activations, &view.activations_retracted);
        api.ports.log.publish(&view.changes, view.final_frame.map(|f| f.0), view.health);
        if let Some(s) = crate::api::snapshot_of(&*self.replica) {
            shared.keep(s);
        }
        if let Some(s) = crate::api::agreed_of(&*self.replica) {
            shared.agreed(s);
        }
        for r in view.changes.iter().filter(|r| r.class == Class::Edit && !shared.has_edit(&r.entry)) {
            let signed = self.agree.entries_at(r.frame).and_then(|es| es.iter().find(|e| e.id() == r.entry));
            if let Some(e) = signed
                && let Some(tx) = pe_world::edit::Transaction::decode(&e.entry.payload)
            {
                shared.index_edit(r.entry, Arc::new(tx), e.entry.payload.len());
            }
        }
    }

    fn count_hold(&mut self, caught_up: bool) {
        self.hold = match (caught_up, self.hold) {
            (true, _) => None,
            (false, None) => {
                self.stats.holds += 1;
                Some(1)
            }
            (false, Some(n)) => Some(n + 1),
        };
        if let Some(n) = self.hold {
            self.stats.longest_hold = self.stats.longest_hold.max(n);
        }
    }

    /// When the next tick is due: the start of the frame after `ticked`, or of frame 0, but never sooner than one
    /// frame period from `now` while the clock has no frame yet. With pe-net's clock model, `host_time_of(Frame(0))`
    /// means nothing until the clock converges (transport.md 5.4), so a shorter floor would wake the thread many
    /// times a frame for the whole convergence. Once pe-net's runner is wired, it ends the wait early through the
    /// thread's [`Waker`] when the clock converges.
    fn next_due(&self, ticked: Option<Frame>, now: HostTime) -> HostTime {
        match ticked {
            Some(p) => self.clock.host_time_of(Frame(p.0 + 1)),
            None => {
                let period = Duration::from_nanos(1_000_000_000 / u64::from(self.clock.rate_hz()));
                self.clock.host_time_of(Frame(0)).max(now.plus(period))
            }
        }
    }
}

/// Wakes the simulation thread before its next frame is due. The network inbox and the edit queue call it when they
/// push (app.md 3, step 7). A wake that comes while the thread is busy is kept for its next wait, so none is lost.
#[derive(Clone, Debug)]
pub struct Waker(thread::Thread);

impl Waker {
    pub fn wake(&self) {
        self.0.unpark();
    }
}

/// The running simulation thread.
pub struct SimThread {
    join: JoinHandle<SimStats>,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

/// What the simulation thread and its owner share beyond the stop flag: the graceful quit (app.md 4).
#[derive(Default)]
struct Shared {
    /// The owner asks this member to leave.
    leave: AtomicBool,
    /// The loop has ticked since the request, so `bound_ms` is the quit's bound.
    asked: AtomicBool,
    /// The quit's [`LeaveStatus`], as [`LeaveStatus::code`], after the latest tick.
    status: std::sync::atomic::AtomicU8,
    /// The quit's bound in milliseconds after the latest tick; 0 before the first.
    bound_ms: std::sync::atomic::AtomicU64,
    /// The other members of this member's epoch, after the latest tick.
    others: std::sync::Mutex<Vec<MemberKey>>,
}

/// How a graceful quit ended (app.md 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaveOutcome {
    /// Every remaining member confirmed it, after `after`.
    Settled { after: Duration },
    /// No other member was connected.
    Alone,
    /// The bound passed first: these members hadn't confirmed it.
    Unconfirmed { others: Vec<MemberKey> },
    /// The bound passed before agreement ever took the `Leave` (each try refused): the others see a dropped link.
    NotSent { others: Vec<MemberKey> },
}

impl SimThread {
    pub fn waker(&self) -> Waker {
        Waker(self.join.thread().clone())
    }

    /// Stops the loop after its current tick, and returns what it counted.
    pub fn stop(self) -> thread::Result<SimStats> {
        self.stop.store(true, Ordering::Release);
        self.join.thread().unpark();
        self.join.join()
    }

    /// The graceful quit (app.md 4): asks the loop to submit this member's `Leave`, which then goes on ticking, so
    /// this member still votes E's suffix. Waits until every remaining member has confirmed E (S3's
    /// `leave_settled`), until no other member is connected, or until the quit's bound passes, then stops the loop.
    /// `say` takes the console's lines: "leaving…" first. Ctrl-C ends the process at once, which is how a second quit
    /// request ends the wait.
    pub fn leave_and_stop(self, say: &mut dyn FnMut(&str)) -> (LeaveOutcome, thread::Result<SimStats>) {
        self.shared.leave.store(true, Ordering::Release);
        self.join.thread().unpark();
        say("leaving the session…");
        let started = Instant::now();
        // The bound is taken from the first tick after the request, so slower views during the wait don't stretch
        // it; until that tick, the shortest.
        let mut bound = None;
        let outcome = loop {
            let status = LeaveStatus::from_code(self.shared.status.load(Ordering::Acquire));
            match status {
                LeaveStatus::Settled => break LeaveOutcome::Settled { after: started.elapsed() },
                LeaveStatus::Alone => break LeaveOutcome::Alone,
                LeaveStatus::Pending | LeaveStatus::Waiting => {}
            }
            if bound.is_none() && self.shared.asked.load(Ordering::Acquire) {
                bound = Some(Duration::from_millis(self.shared.bound_ms.load(Ordering::Acquire)));
            }
            if started.elapsed() >= bound.unwrap_or(crate::leave::MIN_WAIT) {
                let others = self.shared.others.lock().unwrap_or_else(|e| e.into_inner()).clone();
                // Still pending: agreement refused every try, so no `Leave` ever went out (@design-reviewer's Low 1).
                break if status == LeaveStatus::Pending {
                    LeaveOutcome::NotSent { others }
                } else {
                    LeaveOutcome::Unconfirmed { others }
                };
            }
            thread::sleep(Duration::from_millis(20));
        };
        match &outcome {
            LeaveOutcome::Settled { .. } | LeaveOutcome::Alone => say("left the session."),
            LeaveOutcome::Unconfirmed { others } => say(&crate::leave::left_unconfirmed(others)),
            LeaveOutcome::NotSent { others } => say(&crate::leave::leave_not_sent(others)),
        }
        (outcome, self.stop())
    }
}

/// Starts the simulation thread on a stack of `stack` bytes (app.md 1: pe-logic's `Settings::thread_stack()`, since
/// it runs wasm guests), reading the time from `host`.
pub fn spawn(sim: Sim, host: HostClock, stack: usize) -> std::io::Result<SimThread> {
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared::default());
    let (flag, inner) = (stop.clone(), shared.clone());
    let join =
        thread::Builder::new().name("pe-sim".into()).stack_size(stack).spawn(move || run(sim, &host, &flag, &inner))?;
    Ok(SimThread { join, stop, shared })
}

fn run(mut sim: Sim, host: &HostClock, stop: &AtomicBool, shared: &Shared) -> SimStats {
    while !stop.load(Ordering::Acquire) {
        if shared.leave.load(Ordering::Acquire) {
            sim.leave();
        }
        let now = host();
        let ticked = sim.tick(now).map(|(p, _)| p);
        // The tick trace times the loop's own work after the tick (crate::trace); no clock read without one.
        let tick_end = sim.trace.is_some().then(|| host());
        // The graceful quit's standing for the owner's wait, and the console's lines (app.md 4).
        shared.status.store(sim.leave_status().code(), Ordering::Release);
        shared.bound_ms.store(u64::try_from(sim.leave_bound().as_millis()).unwrap_or(u64::MAX), Ordering::Release);
        if sim.leaving != Leaving::Staying {
            *shared.others.lock().unwrap_or_else(|e| e.into_inner()) = sim.other_members();
            shared.asked.store(true, Ordering::Release);
        }
        for line in sim.take_notices() {
            crate::out!("{line}");
        }
        // 7. Wait for the next frame or a wake. A tick that ran long leaves no wait, and the next one jumps ahead.
        let due = sim.next_due(ticked, now);
        let wait_from = host();
        if let (Some(t), Some(end)) = (&mut sim.trace, tick_end) {
            t.wait(end, wait_from, due);
        }
        let left = due.0.saturating_sub(wait_from.0);
        if left > 0 {
            let start = Instant::now();
            thread::park_timeout(Duration::from_nanos(left as u64));
            sim.stats.phases.idle_ns += phases::since(start);
        }
    }
    if let Some(c) = sim.diverge_counters() {
        sim.stats.diverge = c.clone();
    }
    sim.stats
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::clock::{self, LocalClock};
    use crate::standin::{EmptyReplica, SoloAgreement};
    use pe_replica::mock::MockWorld;
    use pe_replica::{ReplicaConfig, ReplicaImpl};
    use pe_seams::{DomainId, LineId, LinkStats, Received};

    pub(crate) const RATE: SimRate = SimRate::DEFAULT;

    type MakeSim = fn(Arc<dyn FrameClock>, Budget) -> (Sim, Arc<FrameSlot>);

    fn sim(clock: Arc<dyn FrameClock>, budget: Budget) -> (Sim, Arc<FrameSlot>) {
        let replica = EmptyReplica::new(clock.clone(), Frame(0));
        with_replica(Box::new(replica), clock, budget)
    }

    /// pe-replica's `ReplicaImpl` over its small world model, until pe-world implements `WorldModel`. The replica
    /// takes the frame clock, so the view's `sim_time` is on the shared time base (S5), as the renderer reads it.
    fn real(clock: Arc<dyn FrameClock>, budget: Budget) -> (Sim, Arc<FrameSlot>) {
        let config = ReplicaConfig::new(RATE, Duration::from_millis(133), clock.clone());
        let replica = ReplicaImpl::new(MockWorld::new(DomainId(0), RATE), config).expect("a replica");
        with_replica(Box::new(replica), clock, budget)
    }

    /// pe-logic's runner, as the binary hands it to each replica it builds.
    pub(crate) fn runner() -> Box<dyn pe_replica::ScriptRunner> {
        let (settings, run) = crate::net::runner_settings();
        Box::new(pe_replica::logic::LogicRunner::new(settings, run).expect("a runner"))
    }

    /// pe-replica's replica over a new, empty pe-world world, as the binary opens it.
    fn on_pe_world(clock: Arc<dyn FrameClock>, budget: Budget) -> (Sim, Arc<FrameSlot>) {
        let replica = crate::world::open_new(0, RATE, clock.clone(), runner()).expect("a replica");
        with_replica(replica, clock, budget)
    }

    fn in_the_ball_pit(clock: Arc<dyn FrameClock>, budget: Budget) -> (Sim, Arc<FrameSlot>) {
        let me = crate::world::DEV_MEMBER;
        let replica = crate::world::open_ball_pit(7, me, RATE, clock.clone(), runner()).expect("pit");
        with_replica(replica, clock, budget)
    }

    /// A joiner refuses an imported world whose governance members aren't agreement's (app.md 4): here the pit holds
    /// the development member while the stand-in agreement names none, and the refusal names both lists.
    #[test]
    fn an_imported_world_whose_members_are_not_agreement_s_is_refused() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, _) = in_the_ball_pit(clock, Budget::new(RATE));
        let why = s.imported_members_differ().expect("refused");
        assert!(why.contains(&crate::leave::player(crate::world::DEV_MEMBER)), "{why}");
        assert!(why.contains("aren't agreement's ()"), "{why}");
        let (s, _) = on_pe_world(Arc::new(LocalClock::new(HostTime(0), RATE)), Budget::new(RATE));
        assert_eq!(s.imported_members_differ(), None, "no members on either side");
    }

    /// M1's ball pit opens with this machine's member admitted: the first view draws every ball as a sphere, and the
    /// member's two hands marked as the local player's (`OWN_HAND`), so VR draws them where the headset tracks them.
    #[test]
    fn the_ball_pit_opens_with_its_balls_and_my_hands() {
        use pe_seams::{Hand, Shape, ViewFlags};
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (mut s, slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        s.tick(clock.host_time_of(Frame(1))).expect("ticked");
        let v = slot.newest().expect("published");
        let ball = Some(Shape::Sphere { radius: pe_world::preset::BALL_R as f32 });
        assert_eq!(v.objects.iter().filter(|o| o.shape == ball).count(), pe_world::preset::BALLS);
        let me = crate::world::DEV_MEMBER;
        let hands: Vec<_> =
            v.objects.iter().filter(|o| o.flags.contains(ViewFlags::OWN_HAND)).map(|o| o.hand_of).collect();
        assert_eq!(hands.len(), 2, "{hands:?}");
        assert!(hands.contains(&Some((me, Hand::Left))) && hands.contains(&Some((me, Hand::Right))), "{hands:?}");
    }

    /// Both of the local player's hands, tracked and open, at fixed rig-frame poses either side of `centre`.
    pub(crate) struct StillHands(pub(crate) [f32; 3]);

    impl pe_seams::HandFeed for StillHands {
        fn hand_at(&self, hand: pe_seams::Hand, at: HostTime) -> Option<pe_seams::HandInput> {
            let x = if hand == pe_seams::Hand::Left { -0.2 } else { 0.2 };
            Some(pe_seams::HandInput {
                hand,
                source: pe_seams::HandSource::Tracked,
                sampled_at: at,
                tracked: true,
                pose: pe_seams::RigPose {
                    position: [self.0[0] + x, self.0[1], self.0[2]],
                    orientation: [0.0, 0.0, 0.0, 1.0],
                },
                linear_velocity: None,
                angular_velocity: None,
                grip: 0.0,
                closed: false,
                discontinuity: 0,
            })
        }
    }

    /// Step 2 (app.md 3; xr.md 5.2): with the local player's hands fed, each tick submits one input entry per tracked
    /// hand on this member's chain, and the hand objects move to the fed poses in the world, through the rig (at the
    /// origin for now), which is where a second machine sees them.
    #[test]
    fn the_local_players_hands_move_to_their_fed_poses() {
        use pe_seams::{Hand, Offset, Pos};
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let me = crate::world::DEV_MEMBER;
        let (s, slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        let mut s = s.with_local(me, [7; 32], Arc::new(StillHands([0.0, 1.0, -0.4])));
        for f in 1..=4 {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        assert_eq!((s.stats().inputs, s.stats().inputs_refused), (6, 0), "two a tick once the first view is out");
        let v = slot.newest().expect("published");
        for (side, x) in [(Hand::Left, -0.2), (Hand::Right, 0.2)] {
            let o = v.objects.iter().find(|o| o.hand_of == Some((me, side))).expect("the hand");
            let want = Pos::ORIGIN.checked_add(Offset([x, 1.0, -0.4])).expect("in range");
            let d = (o.pos - want).0;
            assert!(d.iter().all(|c| c.abs() < 1e-4), "{side:?} is {d:?} off");
        }
    }

    /// A wake ticks again within a frame (step 7): that tick takes in messages and re-simulates, but submits no second
    /// set of hand inputs for the frame, or every wake would add a frame's inputs against the rate budget.
    #[test]
    fn a_second_tick_in_one_frame_submits_no_second_input() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, _slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        let mut s = s.with_local(crate::world::DEV_MEMBER, [7; 32], Arc::new(StillHands([0.0, 1.0, -0.4])));
        let half = Duration::from_nanos(1_000_000_000 / u64::from(RATE.hz()) / 2);
        let f2 = clock.host_time_of(Frame(2));
        for at in [clock.host_time_of(Frame(1)), f2, f2.plus(half), clock.host_time_of(Frame(3))] {
            s.tick(at).expect("ticked");
        }
        assert_eq!(s.stats().ticks, 4);
        assert_eq!(s.stats().inputs, 4, "two at frame 2 and two at frame 3, none from the second tick at frame 2");
    }

    /// An edit through pe-api's queue (app.md 3.1): step 2 submits it first in its frame and settles its ticket, the
    /// world steps it, the change log records it, and `ApiWorld` shows the new object and answers `edit()` for it.
    #[test]
    fn an_edit_from_pe_api_s_queue_is_submitted_stepped_and_recorded() {
        use crate::api::{ApiWorld, Facts, Ports};
        use pe_api::api::WorldSource;
        use pe_api::queue::Submission;
        use pe_seams::world_api::Wait;
        use pe_world::edit::{Actor, Op, Target, Transaction};
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let me = crate::world::DEV_MEMBER;
        let replica = crate::world::open_ball_pit(7, me, RATE, clock.clone(), runner()).expect("pit");
        let genesis = crate::api::snapshot_of(&*replica).expect("a pe-world replica");
        let objects = genesis.all().len();
        let facts = Facts { world_name: "Pit".into(), line: LineId::origin(), rate: RATE, me: Some(me) };
        let ports = Ports::new(genesis, facts, || {});
        let (s, _slot) = with_replica(replica, clock.clone(), Budget::new(RATE));
        let mut s = s.with_local(me, [7; 32], Arc::new(StillHands([0.0, 1.0, -0.4]))).with_api(ports.clone());
        let world = ApiWorld(ports.shared.clone());
        s.tick(clock.host_time_of(Frame(1))).expect("ticked");
        let draft = world.draft_for(me.into());
        let create = Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![] };
        let tx = Transaction {
            inline: vec![],
            actor: Actor::Person,
            label: None,
            preconditions: vec![],
            ops: vec![create],
            blobs: vec![],
        };
        let ticket = ports.queue.submit(Submission { tx: tx.clone(), base: 1 }).expect("room in the queue");
        s.tick(clock.host_time_of(Frame(2))).expect("ticked");
        let Some(Receipt::Submitted { change, frame, seq, .. }) = ports.queue.receipt(ticket, Instant::now()) else {
            panic!("not submitted");
        };
        assert_eq!((frame, seq), (2, draft.seq), "stamped at its frame, and first in it: the seq the draft named");
        for f in 3..=5 {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        let fate = ports.log.wait(change, Wait::Local, Instant::now()).expect("stepped here");
        assert_eq!(fate.frame, Some(2));
        assert!(matches!(fate.outcome, Some(pe_seams::Outcome::Applied { .. })), "{fate:?}");
        assert_eq!(world.latest().all().len(), objects + 1, "the new object is in the newest snapshot");
        assert_eq!(world.edit(change.into()).as_deref(), Some(&tx), "and its transaction in the index");
        assert_eq!((s.stats().edits, s.stats().edits_refused), (1, 0));
    }

    /// A code commit through pe-api's queue (api.md 7.1), made with stamp-ahead's `lead` (none: at the present):
    /// pe-api leaves `activate_at` at 0, and step 2 sets it to the frame it stamps the entry at, so pe-world takes the
    /// commit rather than refusing it `Invalid` (world-model.md 7.9). Returns the receipt's frame and the indexed
    /// transaction's `activate_at`.
    fn code_commit_through_the_queue(lead: Option<u64>) -> (u64, Option<u64>) {
        use crate::api::{ApiWorld, Facts, Ports};
        use crate::stamp::{LeadConfig, StampLead};
        use pe_api::api::WorldSource;
        use pe_api::queue::Submission;
        use pe_seams::Hash;
        use pe_seams::world_api::Wait;
        use pe_world::edit::{Actor, AssetTarget, Op, Transaction};
        use pe_world::script::{pack_source, tag_desc};
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let me = crate::world::DEV_MEMBER;
        let replica = crate::world::open_ball_pit(7, me, RATE, clock.clone(), runner()).expect("pit");
        let genesis = crate::api::snapshot_of(&*replica).expect("a pe-world replica");
        let facts = Facts { world_name: "Pit".into(), line: LineId::origin(), rate: RATE, me: Some(me) };
        let ports = Ports::new(genesis, facts, || {});
        let (s, _slot) = with_replica(replica, clock.clone(), Budget::new(RATE));
        let s = s.with_stamp_lead(lead.map(|n| StampLead::fixed(LeadConfig::m1(RATE), n)));
        let mut s = s.with_local(me, [7; 32], Arc::new(StillHands([0.0, 1.0, -0.4]))).with_api(ports.clone());
        let world = ApiWorld(ports.shared.clone());
        s.tick(clock.host_time_of(Frame(1))).expect("ticked");
        let archive: bytes::Bytes = pack_source(&[("src/lib.rs", b"// a script\n".as_slice())]).expect("one").into();
        let v = Hash(pe_world::BlobRef::of(&archive).hash);
        let ops = vec![
            Op::Register { components: vec![tag_desc(&v)] },
            Op::commit_script(AssetTarget::New(0), None, archive, Hash([1; 32]), Hash([2; 32]), Hash([3; 32]), 0),
        ];
        let tx = Transaction {
            inline: vec![],
            actor: Actor::Person,
            label: None,
            preconditions: vec![],
            ops,
            blobs: vec![],
        };
        let ticket = ports.queue.submit(Submission { tx, base: 1 }).expect("room in the queue");
        s.tick(clock.host_time_of(Frame(2))).expect("ticked");
        let Some(Receipt::Submitted { change, frame, .. }) = ports.queue.receipt(ticket, Instant::now()) else {
            panic!("not submitted");
        };
        for f in 3..=frame + 2 {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        let fate = ports.log.wait(change, Wait::Local, Instant::now()).expect("stepped here");
        assert_eq!(fate.frame, Some(frame), "stepped at its stamp");
        assert!(matches!(fate.outcome, Some(pe_seams::Outcome::Applied { .. })), "taken, not refused: {fate:?}");
        let indexed = world.edit(change.into()).expect("in the index");
        let activate_at = indexed.ops.iter().find_map(|op| match op {
            Op::CommitScript { activate_at, .. } => Some(*activate_at),
            _ => None,
        });
        (frame, activate_at)
    }

    /// A hook that stands for an inviter (app.md 4): it holds script commits back while `hold` is set, and records
    /// what the `Admit`'s check would see each tick, writing no `Admit`.
    struct ScriptWindow {
        hold: Arc<std::sync::atomic::AtomicBool>,
        seen: Arc<std::sync::Mutex<Vec<bool>>>,
    }

    impl NetHook for ScriptWindow {
        fn on_event(&mut self, _: &NetEvent, _: &mut dyn Agreement, _: &dyn WorldFacts) {}
        fn take_entry(&mut self, world: &dyn WorldFacts, _: &dyn Agreement, _: HostTime) -> Option<Vec<u8>> {
            self.seen.lock().expect("seen").push(world.holds_scripts());
            None
        }
        fn holds_script_commits(&self) -> bool {
            self.hold.load(Ordering::SeqCst)
        }
    }

    /// A pit with pe-api's ports and a [`ScriptWindow`] hook, stamping ahead by `lead`; and a script commit's tx.
    fn script_window(
        lead: Option<u64>,
    ) -> (Sim, crate::api::Ports, Arc<dyn FrameClock>, ScriptWindow, pe_world::edit::Transaction) {
        use crate::api::{Facts, Ports};
        use crate::stamp::{LeadConfig, StampLead};
        use pe_seams::Hash;
        use pe_world::edit::{Actor, AssetTarget, Op, Transaction};
        use pe_world::script::{pack_source, tag_desc};
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let me = crate::world::DEV_MEMBER;
        let replica = crate::world::open_ball_pit(7, me, RATE, clock.clone(), runner()).expect("pit");
        let genesis = crate::api::snapshot_of(&*replica).expect("a pe-world replica");
        let facts = Facts { world_name: "Pit".into(), line: LineId::origin(), rate: RATE, me: Some(me) };
        let ports = Ports::new(genesis, facts, || {});
        let (s, _slot) = with_replica(replica, clock.clone(), Budget::new(RATE));
        let s = s.with_stamp_lead(lead.map(|n| StampLead::fixed(LeadConfig::m1(RATE), n)));
        let hook = ScriptWindow { hold: Default::default(), seen: Default::default() };
        let mine = ScriptWindow { hold: hook.hold.clone(), seen: hook.seen.clone() };
        let s = s
            .with_local(me, [7; 32], Arc::new(StillHands([0.0, 1.0, -0.4])))
            .with_api(ports.clone())
            .with_transport(Box::new(Port::new(me, Hub::default())), Some(Box::new(hook)));
        let archive: bytes::Bytes = pack_source(&[(
            "src/lib.rs",
            b"// a script
"
            .as_slice(),
        )])
        .expect("one")
        .into();
        let v = Hash(pe_world::BlobRef::of(&archive).hash);
        let ops = vec![
            Op::Register { components: vec![tag_desc(&v)] },
            Op::commit_script(AssetTarget::New(0), None, archive, Hash([1; 32]), Hash([2; 32]), Hash([3; 32]), 0),
        ];
        let tx = Transaction {
            inline: vec![],
            actor: Actor::Person,
            label: None,
            preconditions: vec![],
            ops,
            blobs: vec![],
        };
        (s, ports, clock, mine, tx)
    }

    /// app.md 4, path 1 of @engine-architect's M1 on 1.6: a script commit pe-api queued before the joiner's ready
    /// reaches step 2's drain in the ready tick. The drain holds it back as `Unavailable`, sending nothing, while other
    /// edits go ahead; pe-api's flag is set in the same tick; once the window closes the commit is submitted.
    #[test]
    fn a_script_commit_drained_while_a_join_holds_commits_is_held_back() {
        use crate::api::ApiWorld;
        use pe_api::api::WorldSource;
        use pe_api::queue::Submission;
        use pe_world::edit::{Op, Target};
        let (mut s, ports, clock, w, tx) = script_window(None);
        let world = ApiWorld(ports.shared.clone());
        s.tick(clock.host_time_of(Frame(1))).expect("ticked");
        assert!(!world.session().joining);
        let commit = ports.queue.submit(Submission { tx: tx.clone(), base: 1 }).expect("room");
        w.hold.store(true, Ordering::SeqCst);
        s.tick(clock.host_time_of(Frame(2))).expect("ticked");
        assert!(world.session().joining, "set in the tick the window opened");
        assert_eq!(
            ports.queue.receipt(commit, Instant::now()),
            Some(Receipt::Unavailable(pe_api::api::JOINING.into()))
        );
        assert_eq!(s.stats().edits, 0, "nothing sent");
        let mut create = tx.clone();
        create.ops = vec![Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![] }];
        let other = ports.queue.submit(Submission { tx: create, base: 2 }).expect("room");
        s.tick(clock.host_time_of(Frame(3))).expect("ticked");
        assert!(
            matches!(ports.queue.receipt(other, Instant::now()), Some(Receipt::Submitted { .. })),
            "only scripts wait"
        );
        w.hold.store(false, Ordering::SeqCst);
        let again = ports.queue.submit(Submission { tx, base: 3 }).expect("room");
        s.tick(clock.host_time_of(Frame(4))).expect("ticked");
        assert!(!world.session().joining);
        assert!(matches!(ports.queue.receipt(again, Instant::now()), Some(Receipt::Submitted { .. })));
    }

    /// app.md 4, path 2 of the same M1: a script commit drained before the joiner's ready, stamped ahead by a lead and
    /// not yet stepped, is counted by the `Admit`'s check that the world holds no scripts, which then refuses the join.
    #[test]
    fn the_admit_s_check_counts_a_script_commit_stamped_ahead_and_not_yet_stepped() {
        use pe_api::queue::Submission;
        let (mut s, ports, clock, w, tx) = script_window(Some(5));
        s.tick(clock.host_time_of(Frame(1))).expect("ticked");
        let commit = ports.queue.submit(Submission { tx, base: 1 }).expect("room");
        s.tick(clock.host_time_of(Frame(2))).expect("ticked");
        let Some(Receipt::Submitted { frame, .. }) = ports.queue.receipt(commit, Instant::now()) else {
            panic!("not submitted");
        };
        assert_eq!(frame, 7, "stamped ahead of the present");
        s.tick(clock.host_time_of(Frame(3))).expect("ticked");
        let seen = w.seen.lock().expect("seen").clone();
        assert_eq!(seen.first(), Some(&false), "an empty world at first: {seen:?}");
        assert_eq!(seen.last(), Some(&true), "stamped at 7, unstepped at 3, and counted: {seen:?}");
    }

    /// At the present, the commit's `activate_at` is the frame the receipt names.
    #[test]
    fn a_code_commit_from_pe_api_s_queue_carries_its_own_stamp_as_activate_at() {
        let (frame, activate_at) = code_commit_through_the_queue(None);
        assert_eq!((frame, activate_at), (2, Some(2)), "the frame step 2 stamped the entry at");
    }

    /// Stamped ahead by a lead of 5 frames, the commit's `activate_at` is its stamp, not the present it was made at.
    #[test]
    fn a_code_commit_stamped_ahead_carries_its_stamp_as_activate_at() {
        let (frame, activate_at) = code_commit_through_the_queue(Some(5));
        assert_eq!((frame, activate_at), (7, Some(7)), "the present 2 plus the lead");
    }

    /// A machine with no member has no edit port: no ports, and `with_api` refuses to attach any (@design-reviewer's
    /// Low on slice 1), so no queued edit can wait unanswered.
    #[test]
    #[should_panic(expected = "with_local first")]
    fn a_machine_with_no_member_has_no_edit_port() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, _slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        assert!(s.ports("Pit", || {}).is_none(), "no ports without a local player");
        let local = s.with_local(crate::world::DEV_MEMBER, [7; 32], Arc::new(StillHands([0.0, 1.0, -0.4])));
        let ports = local.ports("Pit", || {}).expect("ports with one");
        let (bare, _slot) = in_the_ball_pit(clock, Budget::new(RATE));
        let _ = bare.with_api(ports);
    }

    /// A root split S3 reports opens an episode: the health this machine reports, in the published view and to
    /// pe-api, reads `Repairing` from the first open split's frame, though the replica, which only learns of a
    /// divergence at a final cut, still says `Agreed`. Every split stays open until a cut at or past its frame is
    /// final, and a split reported for a cut already settled opens nothing
    /// (@item:primordial-engine/pe-app-runs-pe-diverge-app-md-step-5-and-health-r; @engine-architect's and
    /// @design-reviewer's Lows on 2aad7048).
    #[test]
    fn an_open_root_split_reports_repairing_until_a_cut_past_it_is_final() {
        use pe_seams::{FinalCut, Hash, Health};
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let agree = SoloAgreement::new(LineId::origin());
        let script = agree.script.clone();
        let replica = EmptyReplica::new(clock.clone(), Frame(0));
        let slot = Arc::new(FrameSlot::new());
        let mut s = Sim::new(Box::new(agree), Box::new(replica), clock.clone(), slot.clone(), Budget::new(RATE));
        let at = |frame: u64, height: u64| CutRef { line: LineId::origin(), height, frame: Frame(frame) };
        let split = |c: CutRef| AgreeEvent::RootSplit { cut: c, roots: vec![] };
        let final_at = |c: CutRef| FinalCut { cut: Cut { at: c, heads: vec![] }, root: Hash([1; 32]), voters: vec![] };
        let mut f = 0;
        let mut tick = |s: &mut Sim| {
            f += 1;
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
            // One value for both readers: the view the display takes, and what `health` (and pe-api) report.
            assert_eq!(slot.newest().expect("published").health, s.health());
            s.health()
        };
        for _ in 0..4 {
            assert_eq!(tick(&mut s), Health::Agreed);
        }
        // Splits at cuts 6 and then 3: one episode, from the first.
        script.lock().unwrap().events = vec![split(at(6, 2)), split(at(3, 1))];
        assert_eq!(tick(&mut s), Health::Repairing { since: Frame(3) });
        script.lock().unwrap().events = vec![split(at(9, 3))];
        assert_eq!(tick(&mut s), Health::Repairing { since: Frame(3) });
        assert_eq!((s.stats().root_splits, s.stats().root_splits_ended), (1, 0));
        // A final cut below the first split's frame closes none; one at 3 closes 3, and 6 and 9 stay open.
        script.lock().unwrap().final_cut = Some(final_at(at(2, 0)));
        assert_eq!(tick(&mut s), Health::Repairing { since: Frame(3) });
        script.lock().unwrap().final_cut = Some(final_at(at(3, 4)));
        assert_eq!(tick(&mut s), Health::Repairing { since: Frame(6) });
        // A split reported late, for a cut at or below the final cut's frame, opens nothing.
        script.lock().unwrap().events = vec![split(at(3, 1))];
        assert_eq!(tick(&mut s), Health::Repairing { since: Frame(6) });
        script.lock().unwrap().final_cut = Some(final_at(at(9, 5)));
        assert_eq!(tick(&mut s), Health::Agreed);
        assert_eq!((s.stats().root_splits, s.stats().root_splits_ended), (1, 1));
        script.lock().unwrap().events = vec![split(at(8, 3))];
        assert_eq!(tick(&mut s), Health::Agreed, "a late split opens no episode");
        assert_eq!((s.stats().root_splits, s.stats().root_splits_ended), (1, 1));
    }

    /// A health-only republish is a publication with nothing new: the last publication's records and corrections
    /// aren't carried twice (@engine-architect's and @design-reviewer's Lows on 785ab327).
    #[test]
    fn a_health_only_republish_carries_nothing_new() {
        use pe_seams::ViewFlags;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (mut s, slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        s.tick(clock.host_time_of(Frame(1))).expect("ticked");
        let mut v = (*slot.newest().expect("published")).clone();
        for o in &mut v.objects {
            o.flags = o.flags.with(ViewFlags::CORRECTED);
        }
        let h = Health::Repairing { since: Frame(1) };
        let out = health_only(&v, h);
        assert_eq!(out.health, h);
        assert!(out.changes.is_empty() && out.effects.is_empty() && out.retracted.is_empty());
        assert!(out.activations.is_empty() && out.activations_retracted.is_empty());
        assert_eq!(
            out.counters,
            pe_seams::ViewCounters::default(),
            "the last publication's counters aren't counted twice"
        );
        assert!(out.objects.iter().all(|o| !o.flags.contains(ViewFlags::CORRECTED)), "CORRECTED cleared");
        let own = |w: &FrameView| w.objects.iter().filter(|o| o.flags.contains(ViewFlags::OWN_HAND)).count();
        assert_eq!(own(&out), 2, "the other flags kept");
        assert_eq!((out.frame, out.objects.len(), out.final_frame), (v.frame, v.objects.len(), v.final_frame));
    }

    /// While agreement reports this member backlogged (S3 v1.5), step 2 never holds its hands (F3 S1; the user's): their pose-only entries are thinned to [`HAND_FLOOR_HZ`], one frame's in `rate / 30`, and every
    /// frame's go again once the backlog clears. Red on main 74314c81: no hand inputs while backlogged, `hands_paced` 3.
    #[test]
    fn hand_inputs_are_thinned_to_the_floor_never_held_while_agreement_reports_a_backlog() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let me = crate::world::DEV_MEMBER;
        let replica = crate::world::open_ball_pit(7, me, RATE, clock.clone(), runner()).expect("pit");
        let agree = SoloAgreement::new(LineId::origin());
        let backlog = agree.backlog.clone();
        let slot = Arc::new(FrameSlot::new());
        let mut s = Sim::new(Box::new(agree), replica, clock.clone(), slot, Budget::new(RATE)).with_local(
            me,
            [7; 32],
            Arc::new(StillHands([0.0, 1.0, -0.4])),
        );
        s.tick(clock.host_time_of(Frame(1))).expect("ticked");
        let before = s.stats().inputs;
        backlog.store(true, Ordering::Relaxed);
        let gap = (u64::from(RATE.hz()) / HAND_FLOOR_HZ).max(1);
        let frames = 4 * gap;
        for f in 2..2 + frames {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        let (inputs, thinned) = (s.stats().inputs - before, s.stats().hands_floor_thinned);
        assert_eq!(s.stats().hands_paced, 0, "never held");
        // Both hands, on one frame in `gap`: the floor, and no fewer.
        assert!(
            inputs >= 2 * (frames / gap) && inputs <= 2 * (frames / gap + 1),
            "{inputs} inputs over {frames} frames"
        );
        assert!(thinned >= frames - frames / gap - 1, "{thinned} frames thinned");
        backlog.store(false, Ordering::Relaxed);
        let (inputs, thinned) = (s.stats().inputs, s.stats().hands_floor_thinned);
        for f in 2 + frames..2 + frames + 3 {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        assert_eq!(s.stats().inputs - inputs, 6, "both hands every frame once it clears");
        assert_eq!(s.stats().hands_floor_thinned, thinned);
    }

    /// The console's `hand-mode` (the pull-ahead plan's 1.10, [`crate::handfeel`]): a member alone sends `HandMode` for
    /// both its hands at the next frame, and the world then has both in that mode; `closed` sets them back. Once a
    /// command was given, the status line shows the pair.
    #[test]
    fn the_console_s_hand_mode_reaches_both_hands_in_the_world() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let me = crate::world::DEV_MEMBER;
        let replica = crate::world::open_ball_pit(7, me, RATE, clock.clone(), runner()).expect("pit");
        let slot = Arc::new(FrameSlot::new());
        let feel = Arc::new(crate::handfeel::HandFeel::default());
        let status = Arc::new(pe_render::notices::StatusLine::default());
        let agree = Box::new(SoloAgreement::new(LineId::origin()));
        let mut s = Sim::new(agree, replica, clock.clone(), slot.clone(), Budget::new(RATE))
            .with_local(me, [7; 32], Arc::new(StillHands([0.0, 1.0, -0.4])))
            .with_status(status.clone())
            .with_hand_feel(feel.clone());
        // Each of this member's hands' `Hand.mode`, as the world has it now.
        let modes = |s: &Sim| -> Vec<u64> {
            let world = crate::api::snapshot_of(&*s.replica).expect("a world");
            let view = slot.newest().expect("published");
            (view.objects.iter().filter(|o| o.hand_of.is_some_and(|(m, _)| m == me)))
                .map(|o| match world.fields(o.id, "Hand").expect("a hand row").into_iter().find(|(n, _)| n == "mode") {
                    Some((_, pe_world::Value::U(m))) => m,
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        let mut f = 0;
        let mut run = |s: &mut Sim, n: u64| {
            for _ in 0..n {
                f += 1;
                s.tick(clock.host_time_of(Frame(f))).expect("ticked");
            }
        };
        run(&mut s, 3);
        assert_eq!(modes(&s), vec![0, 0], "closed hand, the default");
        assert_eq!(status.get(), None, "no pair before a command");
        assert!(feel.command("hand-mode reach").expect("ours").is_ok());
        run(&mut s, 3);
        assert_eq!(s.stats().hand_modes, 2, "one for each hand");
        assert_eq!(modes(&s), vec![1, 1], "always within reach");
        assert_eq!(status.get().as_deref(), Some("Hands: always within reach · drawn at the tracked pose"));
        assert!(feel.command("hand-mode closed").expect("ours").is_ok());
        run(&mut s, 3);
        assert_eq!((s.stats().hand_modes, s.stats().hand_modes_refused), (4, 0));
        assert_eq!(modes(&s), vec![0, 0], "back to closed hand");
        assert_eq!(status.get().as_deref(), Some("Hands: closed hand · drawn at the tracked pose"));
    }

    /// Hands that are never tracked: step 2 sends no pose for them, so a test sees only the other entries.
    struct UntrackedHands;

    impl pe_seams::HandFeed for UntrackedHands {
        fn hand_at(&self, _: pe_seams::Hand, _: HostTime) -> Option<pe_seams::HandInput> {
            None
        }
    }

    /// A refused `HandMode` isn't lost (@design-reviewer's L1 on 9057acd0): agreement refuses the first one, the left
    /// hand's, and the next tick sends the mode to both hands again, so both end in it.
    #[test]
    fn a_refused_hand_mode_goes_again_at_the_next_tick() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let me = crate::world::DEV_MEMBER;
        let replica = crate::world::open_ball_pit(7, me, RATE, clock.clone(), runner()).expect("pit");
        let slot = Arc::new(FrameSlot::new());
        let feel = Arc::new(crate::handfeel::HandFeel::default());
        let agree = SoloAgreement::new(LineId::origin());
        let refuse = agree.refuse_inputs.clone();
        let mut s = Sim::new(Box::new(agree), replica, clock.clone(), slot.clone(), Budget::new(RATE))
            .with_local(me, [7; 32], Arc::new(UntrackedHands))
            .with_hand_feel(feel.clone());
        for f in 1..=3 {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        refuse.store(1, Ordering::Relaxed);
        assert!(feel.command("hand-mode reach").expect("ours").is_ok());
        s.tick(clock.host_time_of(Frame(4))).expect("ticked");
        assert_eq!((s.stats().hand_modes, s.stats().hand_modes_refused), (1, 1), "the left refused, the right taken");
        assert_eq!(feel.take_pending(), Some(pe_world::components::hand_mode::WITHIN_REACH), "put back");
        feel.put_back(pe_world::components::hand_mode::WITHIN_REACH);
        for f in 5..=6 {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        assert_eq!((s.stats().hand_modes, s.stats().hand_modes_refused), (3, 1), "both again, once");
        assert_eq!(feel.take_pending(), None, "nothing left to send");
        let world = crate::api::snapshot_of(&*s.replica).expect("a world");
        let view = slot.newest().expect("published");
        let hands: Vec<_> = view.objects.iter().filter(|o| o.hand_of.is_some_and(|(m, _)| m == me)).collect();
        assert_eq!(hands.len(), 2);
        for o in hands {
            let mode = world.fields(o.id, "Hand").expect("a hand row").into_iter().find(|(n, _)| n == "mode");
            assert!(matches!(mode, Some((_, pe_world::Value::U(1)))), "{:?}: {mode:?}", o.hand_of);
        }
    }

    /// Both hands tracked and closed up to frame `.1`, then lost and open: pe-xr's release of a lost hand that held.
    struct LostAfter(Arc<dyn FrameClock>, u64);

    impl pe_seams::HandFeed for LostAfter {
        fn hand_at(&self, hand: pe_seams::Hand, at: HostTime) -> Option<pe_seams::HandInput> {
            let tracked = self.0.frame_at(at).is_some_and(|f| f.0 <= self.1);
            let held = StillHands([0.0, 1.0, -0.4]).hand_at(hand, at)?;
            Some(pe_seams::HandInput { tracked, closed: tracked, grip: if tracked { 1.0 } else { 0.0 }, ..held })
        }
    }

    /// A lost hand that held something releases once (@xr-eng's Medium on cc8d0a96): tracked and closed it sends
    /// `engaged`; lost and open it sends one release at its held pose; lost and still open it sends nothing more.
    #[test]
    fn a_lost_hand_sends_its_release_once_and_then_nothing() {
        use pe_world::input::Input;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, _slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        // Tracked for the entries stamped through frame 2: each samples its hands at its frame's end (app.md 3).
        let mut s = s.with_local(crate::world::DEV_MEMBER, [7; 32], Arc::new(LostAfter(clock.clone(), 3)));
        for f in 1..=5 {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        let engaged = |f: u64| -> Vec<bool> {
            (s.agree.entries_at(Frame(f)).unwrap_or(&[]).iter())
                .map(|e| match Input::decode(&e.entry.payload) {
                    Some(Input::Hand { poses, .. }) => poses[0].engaged,
                    other => panic!("not a hand input: {other:?}"),
                })
                .collect()
        };
        assert_eq!(engaged(2), vec![true, true], "tracked and closed");
        assert_eq!(engaged(3), vec![false, false], "lost: one release each");
        assert_eq!((engaged(4), engaged(5)), (vec![], vec![]), "lost and open: nothing more");
        assert_eq!(s.stats().inputs, 4);
    }

    /// Hands the test moves, left then right: each tracked at a rig position, closed or open, as the test last set it.
    /// A hand never set gives no input.
    #[derive(Clone, Default)]
    struct Puppet(Arc<std::sync::Mutex<[Option<PuppetHand>; 2]>>);

    /// A puppet hand's rig position, and whether it is closed.
    type PuppetHand = ([f32; 3], bool);

    impl Puppet {
        /// The right hand.
        fn set(&self, at: [f32; 3], closed: bool) {
            self.set_hand(1, at, closed);
        }

        /// Hand `i`: 0 left, 1 right.
        fn set_hand(&self, i: usize, at: [f32; 3], closed: bool) {
            self.0.lock().expect("the puppet")[i] = Some((at, closed));
        }
    }

    impl pe_seams::HandFeed for Puppet {
        fn hand_at(&self, hand: pe_seams::Hand, at: HostTime) -> Option<pe_seams::HandInput> {
            let (position, closed) = self.0.lock().expect("the puppet")[usize::from(hand == pe_seams::Hand::Right)]?;
            Some(pe_seams::HandInput {
                hand,
                source: pe_seams::HandSource::Tracked,
                sampled_at: at,
                tracked: true,
                pose: pe_seams::RigPose { position, orientation: [0.0, 0.0, 0.0, 1.0] },
                linear_velocity: None,
                angular_velocity: None,
                grip: if closed { 1.0 } else { 0.0 },
                closed,
                discontinuity: 0,
            })
        }
    }

    /// Ticks `s` through the next `n` frames after `*f`.
    fn run_frames(s: &mut Sim, clock: &Arc<dyn FrameClock>, f: &mut u64, n: u64) {
        for _ in 0..n {
            *f += 1;
            s.tick(clock.host_time_of(Frame(*f))).expect("ticked");
        }
    }

    /// `o`'s place as a rig position, the rig being the identity at the origin, as the development member's is.
    fn rig_at(v: &FrameView, o: ObjectId) -> [f32; 3] {
        assert_eq!(
            (v.local_rig.pos, v.local_rig.rotation, v.local_rig.scale),
            (Pos::ORIGIN, [0.0, 0.0, 0.0, 1.0], 1.0)
        );
        let p = v.objects.iter().find(|x| x.id == o).expect("the object").pos;
        (p - Pos::ORIGIN).0.map(|c| c as f32)
    }

    /// The highest ball in the view: one with nothing on top of it.
    fn top_ball(v: &FrameView) -> ObjectId {
        let balls =
            v.objects.iter().filter(|o| o.hand_of.is_none() && matches!(o.shape, Some(pe_seams::Shape::Sphere { .. })));
        balls.max_by(|a, b| (a.pos - Pos::ORIGIN).0[1].total_cmp(&(b.pos - Pos::ORIGIN).0[1])).expect("a ball").id
    }

    /// The grab, end to end on one machine (app.md 3.1 step 2; @coordinator-opus): a hand that closes on
    /// a ball sends a `Grab` with its hand input, the world's claim names that hand, the ball rises with the hand, and
    /// opening sends the `Release` that ends the claim; the end line's counts say so.
    #[test]
    fn a_closing_hand_picks_up_the_ball_in_reach_carries_it_and_lets_it_go() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        let puppet = Puppet::default();
        puppet.set([0.0, 3.0, 0.0], false);
        let me = crate::world::DEV_MEMBER;
        let mut s = s.with_local(me, [7; 32], Arc::new(puppet.clone()));
        let mut f = 0;
        run_frames(&mut s, &clock, &mut f, 120);
        let v = slot.newest().expect("published");
        let ball = top_ball(&v);
        let hand = v.objects.iter().find(|o| o.hand_of == Some((me, pe_seams::Hand::Right))).expect("my hand").id;
        let at = rig_at(&v, ball);
        puppet.set(at, false);
        run_frames(&mut s, &clock, &mut f, 3);
        assert_eq!(s.stats().hands[1], HandStats::default(), "open: no edge yet");
        puppet.set(at, true);
        run_frames(&mut s, &clock, &mut f, 1);
        assert_eq!((s.stats().hands[1].closes, s.stats().hands[1].grabs), (1, 1), "the close sent one grab");
        run_frames(&mut s, &clock, &mut f, 10);
        let claim = crate::api::snapshot_of(&*s.replica).expect("a world").claim(ball).expect("claimed");
        assert_eq!((claim.member, claim.hand), (me, hand));
        let v = slot.newest().expect("published");
        let held = v.objects.iter().find(|o| o.id == ball).expect("the ball").held_by;
        assert_eq!(held, Some(pe_seams::Holder { member: me, hand }));
        assert_eq!(s.stats().hands[1].grabs_applied, 1);
        // Carried 0.3 m up at 0.6 m/s, then held still.
        for k in 1..=30 {
            puppet.set([at[0], at[1] + 0.01 * k as f32, at[2]], true);
            run_frames(&mut s, &clock, &mut f, 1);
        }
        run_frames(&mut s, &clock, &mut f, 20);
        let lifted = rig_at(&slot.newest().expect("published"), ball);
        let off: Vec<f32> = (0..3).map(|i| lifted[i] - [at[0], at[1] + 0.3, at[2]][i]).collect();
        assert!(off.iter().all(|d| d.abs() < 0.08), "the ball follows the hand: {off:?} off its place");
        // Opened: the release ends the claim, and the ball falls.
        puppet.set([at[0], at[1] + 0.3, at[2]], false);
        run_frames(&mut s, &clock, &mut f, 1);
        assert_eq!((s.stats().hands[1].opens, s.stats().hands[1].releases), (1, 1), "the open sent one release");
        run_frames(&mut s, &clock, &mut f, 30);
        assert_eq!(crate::api::snapshot_of(&*s.replica).expect("a world").claim(ball), None, "released");
        let v = slot.newest().expect("published");
        assert_eq!(v.objects.iter().find(|o| o.id == ball).expect("the ball").held_by, None);
        assert!(rig_at(&v, ball)[1] < lifted[1] - 0.1, "and it falls");
        let h = &s.stats().hands[1];
        assert_eq!((h.closes, h.opens, h.grabs, h.grabs_applied, h.releases), (1, 1, 1, 1, 1), "{h:?}");
        assert!(h.grabs_refused.is_empty(), "{h:?}");
        assert_eq!(s.stats().hands[0], HandStats::default(), "the left hand did nothing");
    }

    /// The scripted hands (scripted.rs, the pull-ahead's T1) through the simulation: `grab` takes the top ball, lifts
    /// it and lets it go from a still hand, and `throw` sends it off fast; each closes once, grabs once and opens once.
    /// Scripted hands carry their velocity, as VR's do, so each open goes out as a `Throw` (world-model.md 8.1): the
    /// ball leaves at the hand's speed, about 0 from `grab`'s still hand.
    #[test]
    fn the_scripted_routines_grab_lift_and_throw_a_ball() {
        struct NoDesktop;
        impl pe_seams::HandFeed for NoDesktop {
            fn hand_at(&self, _: pe_seams::Hand, _: HostTime) -> Option<pe_seams::HandInput> {
                None
            }
        }
        for routine in ["grab", "throw"] {
            let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
            let (s, slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
            let hands = Arc::new(crate::scripted::ScriptedHands::new(slot.clone(), Arc::new(NoDesktop)));
            let me = crate::world::DEV_MEMBER;
            let mut s = s.with_local(me, [7; 32], hands.clone());
            let mut f = 0;
            run_frames(&mut s, &clock, &mut f, 120);
            let ball = top_ball(&slot.newest().expect("published"));
            hands.start(crate::scripted::load(routine).expect("a routine"));
            // The routine's close comes 1.2 s in: watch the ball's top speed once it's let go.
            let hz = u64::from(RATE.hz());
            let (mut held, mut released_at) = (false, None::<f32>);
            for _ in 0..(7 * hz) {
                run_frames(&mut s, &clock, &mut f, 1);
                let v = slot.newest().expect("published");
                let b = v.objects.iter().find(|o| o.id == ball).expect("the ball");
                held |= b.held_by.is_some();
                if held && b.held_by.is_none() && released_at.is_none() {
                    // Across the floor: by the first view without the hold, gravity has already pulled for a frame or more.
                    let [vx, _, vz] = b.linear_velocity;
                    released_at = Some((vx * vx + vz * vz).sqrt());
                }
            }
            let h = &s.stats().hands[1];
            assert!(held, "{routine}: the top ball was held");
            assert_eq!((h.closes, h.opens, h.grabs, h.grabs_applied), (1, 1, 1, 1), "{routine}: {h:?}");
            assert!(h.grabs_refused.is_empty(), "{routine}: {h:?}");
            assert_eq!((h.releases, h.throws), (1, 1), "{routine}: one open, carrying the hand's velocity: {h:?}");
            let speed = released_at.expect("the ball was let go");
            match routine {
                "grab" => assert!(speed < 0.3, "grab: the ball left a still hand at {speed} m/s across"),
                _ => assert!(speed > 2.5, "throw: the ball left at {speed} m/s across"),
            }
            assert_eq!(s.stats().hands[0], HandStats::default(), "{routine}: the left hand did nothing");
        }
    }

    /// D4's hand-off (render.md 8; @item:primordial-engine/letting-go-in-vr-the-ball-flies-at-once-with-no-g): the
    /// publication whose steps apply this member's own `Release` marks the ball `RELEASED_LOCAL`, and it is the one in
    /// which the ball stops being held. No earlier publication marks anything; later ones mark only the ball, until
    /// the release is behind a final frame or 2 s old.
    #[test]
    fn the_view_that_steps_my_release_marks_the_ball_released_local() {
        the_view_that_steps_my_release_marks_it(None);
    }

    /// The same with this member's entries stamped 4 frames ahead of the present (stamp-ahead's lead, app.md 3.2): the
    /// views before the release's frame still show the ball held, and its mark outlasts them (@design-reviewer's M1
    /// on 5277241c: a mark forgotten at the first view that showed the ball held left nothing to mark when the release
    /// applied, so the display blended the hand-off as a correction, the ghost).
    #[test]
    fn a_release_stamped_ahead_is_marked_in_the_view_that_steps_it() {
        the_view_that_steps_my_release_marks_it(Some(4));
    }

    /// The release-mark test, with a fixed stamp-ahead lead of `lead` frames if given, else stamping at the present.
    fn the_view_that_steps_my_release_marks_it(lead: Option<u64>) {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        let puppet = Puppet::default();
        puppet.set([0.0, 3.0, 0.0], false);
        let me = crate::world::DEV_MEMBER;
        let mut s = s.with_local(me, [7; 32], Arc::new(puppet.clone()));
        if let Some(lead) = lead {
            let fixed = crate::stamp::StampLead::fixed(crate::stamp::LeadConfig::m1(RATE), lead);
            s = s.with_stamp_lead(Some(fixed));
        }
        let mut f = 0;
        run_frames(&mut s, &clock, &mut f, 120);
        let ball = top_ball(&slot.newest().expect("published"));
        let at = rig_at(&slot.newest().expect("published"), ball);
        puppet.set(at, false);
        run_frames(&mut s, &clock, &mut f, 3);
        let marked = |v: &FrameView| {
            v.objects.iter().filter(|o| o.flags.contains(ViewFlags::RELEASED_LOCAL)).map(|o| o.id).collect::<Vec<_>>()
        };
        let mut views = Vec::new();
        puppet.set(at, true);
        for _ in 0..20 {
            run_frames(&mut s, &clock, &mut f, 1);
            views.extend(slot.take().views);
        }
        assert!(slot.newest().and_then(|v| v.object(ball).and_then(|o| o.held_by)).is_some(), "held");
        puppet.set(at, false);
        let opened = views.len();
        for _ in 0..10 {
            run_frames(&mut s, &clock, &mut f, 1);
            views.extend(slot.take().views);
        }
        assert_eq!(s.stats().hands[1].releases, 1);
        if let Some(lead) = lead {
            assert_eq!(s.stats().lead, lead, "the lead held");
        }
        let free = views[opened..]
            .iter()
            .position(|v| v.object(ball).is_some_and(|o| o.held_by.is_none()))
            .map(|k| opened + k)
            .expect("let go");
        assert_eq!(marked(&views[free]), vec![ball], "the publication that ends the hold marks the ball");
        let before: Vec<usize> = (0..free).filter(|&k| !marked(&views[k]).is_empty()).collect();
        assert!(before.is_empty(), "no publication before it marks anything: {before:?} (free at {free})");
        // It stays marked until the release is behind a final frame, or 2 s (a view lost in a gap mustn't lose the
        // mark, @design-reviewer's L1 on bb23fc58), and nothing else is.
        assert!(views[free..].iter().all(|v| marked(v).iter().all(|&o| o == ball)), "only the ball");
        // The stand-in agreement here never finalizes: the mark ends 2 s after the release's stamp.
        run_frames(&mut s, &clock, &mut f, 120);
        assert!(marked(&slot.newest().expect("published")).is_empty(), "no mark 2 s on");
    }

    /// A re-grab ends a release's mark (@design-reviewer's L1 on dc6a7a9b): the ball let go, grabbed again, then lost
    /// with no `Release` (the hand vanishes and the claim lapses), all within the mark's 2 s. The publication in which
    /// that hold ends marks nothing, so the display treats it as the correction it is, not as a hand-off.
    #[test]
    fn a_hold_that_lapses_after_a_re_grab_is_not_marked_as_a_release() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        let puppet = Puppet::default();
        puppet.set([0.0, 3.0, 0.0], false);
        let mut s = s.with_local(crate::world::DEV_MEMBER, [7; 32], Arc::new(puppet.clone()));
        let mut f = 0;
        run_frames(&mut s, &clock, &mut f, 120);
        let ball = top_ball(&slot.newest().expect("published"));
        let at = rig_at(&slot.newest().expect("published"), ball);
        let held = |v: &FrameView| v.object(ball).is_some_and(|o| o.held_by.is_some());
        let marked = |v: &FrameView| v.object(ball).is_some_and(|o| o.flags.contains(ViewFlags::RELEASED_LOCAL));
        puppet.set(at, false);
        run_frames(&mut s, &clock, &mut f, 3);
        puppet.set(at, true);
        run_frames(&mut s, &clock, &mut f, 20);
        assert!(held(&slot.newest().expect("published")), "held");
        // Let go: the mark, as before.
        puppet.set(at, false);
        run_frames(&mut s, &clock, &mut f, 5);
        let v = slot.newest().expect("published");
        assert!(!held(&v) && marked(&v), "let go and marked");
        // Grabbed again where it lies.
        let again = rig_at(&v, ball);
        puppet.set(again, true);
        run_frames(&mut s, &clock, &mut f, 20);
        assert!(held(&slot.newest().expect("published")), "held again");
        // The hand vanishes: no input, no `Release`; the claim lapses after `claim_ms` (0.5 s).
        puppet.0.lock().expect("the puppet")[1] = None;
        let _ = slot.take();
        let mut views = Vec::new();
        for _ in 0..60 {
            run_frames(&mut s, &clock, &mut f, 1);
            views.extend(slot.take().views);
        }
        let lapsed = views.iter().find(|v| !held(v)).expect("the claim lapsed");
        assert!(!marked(lapsed), "a lapse after a re-grab is not marked as the first release");
        assert_eq!(s.stats().hands[1].releases, 1, "only the first open sent a Release");
    }

    /// B's shown present (app.md 3, [`crate::shown`]): while the XR thread records a display lead, the replica runs the
    /// lead plus two periods ahead of the present, and this member's hand entries are stamped there. With no lead for
    /// two seconds it falls back to the present, and no stamp ever steps down.
    #[test]
    fn a_display_lead_runs_the_shown_present_ahead_and_stamps_there() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        let puppet = Puppet::default();
        puppet.set([0.0, 3.0, 0.0], false);
        let lead = Arc::new(crate::shown::DisplayLead::default());
        let mut s = s.with_local(crate::world::DEV_MEMBER, [7; 32], Arc::new(puppet)).with_shown(lead.clone());
        let mut f = 0;
        run_frames(&mut s, &clock, &mut f, 60);
        let behind = f as i64 - slot.newest().expect("published").frame.0 as i64;
        assert_eq!(s.stats().shown_ahead, 0, "no headset: the present");
        let mut stamps = Vec::new();
        for _ in 0..60 {
            lead.record(clock.host_time_of(Frame(f)), 46_000_000);
            run_frames(&mut s, &clock, &mut f, 1);
            stamps.push(s.local.as_ref().expect("local").chain.last_stamp());
        }
        // 46 ms plus a 60 Hz period and the publication's 5 ms is 67.7 ms: 4 whole frames past each frame's start.
        assert_eq!((s.stats().shown_ahead, s.stats().largest_shown_ahead), (4, 4));
        let v = slot.newest().expect("published");
        assert_eq!(f as i64 - v.frame.0 as i64, behind - 4, "the replica runs 4 frames further ahead");
        assert_eq!(stamps.last().copied().flatten(), Some(Frame(f + 4)), "and the hands are stamped there");
        // The headset off: no lead after two seconds, the present again, and the stamps never step down.
        for _ in 0..150 {
            run_frames(&mut s, &clock, &mut f, 1);
            stamps.push(s.local.as_ref().expect("local").chain.last_stamp());
        }
        assert_eq!(s.stats().shown_ahead, 0);
        assert!(stamps.windows(2).all(|w| w[0] <= w[1]), "stamps never step down: {stamps:?}");
        assert_eq!(stamps.last().copied().flatten(), Some(Frame(f)), "back at the present");
    }

    /// B's acceptance test (@item:primordial-engine/vr-run-the-local-simulation-at-the-moment-the-he; the user's: "ideally ... zero jumps in a singleplayer session"): one member in the ball pit, its hand forecast
    /// perfectly (the pose at any time from the script) and its grasp read at the real present only, never ahead; the
    /// display drawn at a 46 ms display lead under Shown, at 144 Hz. The hand picks up the top ball, lifts it, swings it
    /// at 4 m/s (25 m/s² up and down) and opens at the peak. Every display frame lies between two simulated frames, so
    /// nothing is guessed: no correction, no extrapolation miss, no cue and no frame past the newest view, and the ball
    /// leaves the drawn hand with no gap. Printed: the launch speed against the hand's peak and against its speed when
    /// the simulation let go, which is the shown lead after the real opening (a grasp is never predicted).
    #[test]
    fn solo_play_on_the_shown_present_draws_every_frame_between_simulated_ones() {
        use std::sync::atomic::AtomicI64;
        type Path = dyn Fn(f64) -> [f32; 3] + Send + Sync;
        #[derive(Clone)]
        struct Scripted {
            path: Arc<Path>,
            velocity: Arc<Path>,
            closed: Arc<dyn Fn(f64) -> bool + Send + Sync>,
            now: Arc<AtomicI64>,
        }
        impl pe_seams::HandFeed for Scripted {
            fn hand_at(&self, hand: pe_seams::Hand, at: HostTime) -> Option<pe_seams::HandInput> {
                if hand != pe_seams::Hand::Right {
                    return None;
                }
                let closed = (self.closed)(self.now.load(Ordering::Relaxed) as f64 / 1e9);
                Some(pe_seams::HandInput {
                    hand,
                    source: pe_seams::HandSource::Tracked,
                    sampled_at: at,
                    tracked: true,
                    pose: pe_seams::RigPose {
                        position: (self.path)(at.0 as f64 / 1e9),
                        orientation: [0.0, 0.0, 0.0, 1.0],
                    },
                    linear_velocity: Some((self.velocity)(at.0 as f64 / 1e9)),
                    angular_velocity: Some([0.0; 3]),
                    grip: if closed { 1.0 } else { 0.0 },
                    closed,
                    discontinuity: 0,
                })
            }
        }
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        // Settle the pit with no hand, then script the throw from the top ball.
        let mut sim = s;
        let mut f = 0;
        run_frames(&mut sim, &clock, &mut f, 120);
        let v = slot.newest().expect("published");
        let ball = top_ball(&v);
        let at = rig_at(&v, ball);
        let t0 = clock.host_time_of(Frame(f)).0 as f64 / 1e9;
        let (t_grab, t_lift, t_swing) = (t0 + 0.3, t0 + 0.6, t0 + 1.0);
        let (a, ramp) = (25.0_f64, 0.16_f64);
        let dir: f32 = if at[0] > 0.0 { -1.0 } else { 1.0 };
        let t_open = t_swing + ramp;
        let along = move |t: f64| -> f64 {
            let u = (t - t_swing).clamp(0.0, 2.0 * ramp);
            if u < ramp {
                0.5 * a * u * u
            } else {
                0.5 * a * ramp * ramp + a * ramp * (u - ramp) - 0.5 * a * (u - ramp).powi(2)
            }
        };
        let hand_speed = move |t: f64| -> f64 {
            let u = (t - t_swing).clamp(0.0, 2.0 * ramp);
            if u < ramp { a * u } else { a * (2.0 * ramp - u) }
        };
        let path: Arc<Path> = Arc::new(move |t: f64| {
            let lift = 0.4 * ((t - t_lift) / 0.4).clamp(0.0, 1.0) as f32;
            [at[0] + dir * along(t) as f32, at[1] + lift, at[2]]
        });
        let velocity: Arc<Path> = Arc::new(move |t: f64| {
            let lifting = if (t_lift..t_lift + 0.4).contains(&t) { 1.0 } else { 0.0 };
            [dir * hand_speed(t) as f32, lifting, 0.0]
        });
        let now = Arc::new(AtomicI64::new(0));
        let hand = Scripted {
            path: path.clone(),
            velocity,
            closed: Arc::new(move |t| t >= t_grab && t < t_open),
            now: now.clone(),
        };
        let lead = Arc::new(crate::shown::DisplayLead::default());
        let mut sim = sim.with_local(crate::world::DEV_MEMBER, [7; 32], Arc::new(hand)).with_shown(lead.clone());
        let mut layer = pe_render::display::DisplayLayer::new(pe_render::display::DisplaySettings {
            drawn_time: pe_render::drawn_time::DrawnTime::shown(Duration::ZERO, RATE.hz()),
            ..Default::default()
        });
        let display_lead = 46_000_000_i64;
        let (mut released, mut launched) = (None::<f64>, None::<f64>);
        let mut next_tick = clock.host_time_of(Frame(f + 1));
        let start = clock.host_time_of(Frame(f));
        for k in 0..(2.4 * 144.0) as i64 {
            let t = HostTime(start.0 + k * 1_000_000_000 / 144);
            while next_tick <= t {
                now.store(next_tick.0, Ordering::Relaxed);
                f += 1;
                sim.tick(next_tick).expect("ticked");
                next_tick = clock.host_time_of(Frame(f + 1));
            }
            lead.record(t, display_lead);
            let batch = slot.take();
            let Some(newest) = batch.views.last().cloned().or_else(|| slot.newest()) else { continue };
            for v in batch.views.iter().take(batch.views.len().saturating_sub(1)) {
                layer.take(v, HostTime(t.0 + display_lead), &[]);
            }
            for v in &batch.views {
                if let Some(o) = v.object(ball)
                    && released.is_none()
                    && v.frame.0 > 0
                    && o.held_by.is_none()
                    && o.flags.contains(ViewFlags::RELEASED_LOCAL)
                {
                    released = Some(clock.host_time_of(v.frame).0 as f64 / 1e9);
                    let [x, y, z] = o.linear_velocity;
                    launched = Some(f64::from((x * x + y * y + z * z).sqrt()));
                }
            }
            let shown = HostTime(t.0 + display_lead);
            let drawn_hand = pe_render::DisplayHand {
                pose: pe_seams::RigPose { position: path(shown.0 as f64 / 1e9), orientation: [0.0, 0.0, 0.0, 1.0] },
                joints: None,
            };
            let local = pe_render::LocalPresentation { display_time: shown, hands: [None, Some(drawn_hand)] };
            layer.frame(pe_render::display::FrameInput {
                view: &newest,
                display_time: shown,
                volumes: &[],
                local: &local,
            });
            if k == 36 {
                // The first quarter second: the headset's first frames, drawn before the simulation has their lead and
                // runs ahead (the start of any XR session). From here on every frame counts.
                let _ = layer.take_counters();
            }
        }
        let c = layer.counters();
        let (released, launched) = (released.expect("let go"), launched.expect("a launch speed"));
        println!(
            "B solo: corrections {}, extrapolation misses {}, cues {}, past the newest {}, hand-offs {}, gap {:.4} m; \
             launch {launched:.2} m/s against a {:.2} m/s peak and {:.2} m/s where the simulation let go, {:.0} ms \
             after the real opening",
            c.corrections,
            c.extrapolation_misses,
            c.cues,
            c.drawn_past_newest,
            c.hand_offs,
            c.hand_off_gap_max,
            hand_speed(t_open),
            hand_speed(released),
            (released - t_open) * 1e3
        );
        assert_eq!((c.corrections, c.extrapolation_misses, c.cues, c.drawn_past_newest), (0, 0, 0, 0), "{c:?}");
        assert_eq!(c.hand_offs, 1, "{c:?}");
        assert!(c.hand_off_gap_max < 0.02, "the ball leaves the drawn hand where it is: {} m", c.hand_off_gap_max);
        // The grasp is read now, never forecast, so the simulation lets go the shown lead after the real opening, on a
        // hand already slowing. The throw carries the velocity the hand had at the opening, read at the first tick
        // after it (the weak-throw fix, world-model.md 8.1), so the ball leaves at that speed, within a few percent
        // (one frame's gravity on top), not at the slowed hand's.
        let opened = (t_open * 60.0).ceil() / 60.0;
        assert!(
            (launched - hand_speed(opened)).abs() < 0.03 * hand_speed(t_open),
            "left at the opening's {:.2} m/s: {launched}",
            hand_speed(opened)
        );
        assert_eq!(sim.stats().hands[1].throws, 1, "the opening went as a throw");
    }

    /// A ball passed from the left hand to the right stays in the right when the left opens at once (@design-reviewer's
    /// Medium on 82644261). pe-world's `Release` ends the member's claim whichever hand holds it, so the left's open
    /// must not release what the right just took, though the present world may not show the pass yet.
    #[test]
    fn a_ball_passed_from_hand_to_hand_stays_held_when_the_first_hand_opens() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        let puppet = Puppet::default();
        puppet.set_hand(0, [0.0, 3.0, 0.0], false);
        puppet.set_hand(1, [0.3, 3.0, 0.0], false);
        let me = crate::world::DEV_MEMBER;
        let mut s = s.with_local(me, [7; 32], Arc::new(puppet.clone()));
        let mut f = 0;
        run_frames(&mut s, &clock, &mut f, 120);
        let v = slot.newest().expect("published");
        let ball = top_ball(&v);
        let hand_of = |side| v.objects.iter().find(|o| o.hand_of == Some((me, side))).expect("my hand").id;
        let (left, right) = (hand_of(pe_seams::Hand::Left), hand_of(pe_seams::Hand::Right));
        let at = rig_at(&v, ball);
        puppet.set_hand(0, at, false);
        run_frames(&mut s, &clock, &mut f, 3);
        puppet.set_hand(0, at, true);
        run_frames(&mut s, &clock, &mut f, 10);
        let claim = |s: &Sim| crate::api::snapshot_of(&*s.replica).expect("a world").claim(ball);
        assert_eq!(claim(&s).map(|c| c.hand), Some(left), "the left hand holds it");
        for k in 1..=30 {
            puppet.set_hand(0, [at[0], at[1] + 0.01 * k as f32, at[2]], true);
            run_frames(&mut s, &clock, &mut f, 1);
        }
        run_frames(&mut s, &clock, &mut f, 10);
        let up = [at[0], at[1] + 0.3, at[2]];
        // The right hand comes to the ball and closes on it, and the left opens on the next frame.
        puppet.set_hand(1, up, false);
        run_frames(&mut s, &clock, &mut f, 2);
        puppet.set_hand(1, up, true);
        run_frames(&mut s, &clock, &mut f, 1);
        puppet.set_hand(0, up, false);
        run_frames(&mut s, &clock, &mut f, 30);
        assert_eq!(claim(&s).map(|c| c.hand), Some(right), "the right hand holds it");
        let now = rig_at(&slot.newest().expect("published"), ball);
        let off: Vec<f32> = (0..3).map(|i| now[i] - up[i]).collect();
        assert!(off.iter().all(|d| d.abs() < 0.08), "still in the hand, not dropped: {off:?} off");
        let (l, r) = (&s.stats().hands[0], &s.stats().hands[1]);
        assert_eq!((l.closes, l.opens, l.grabs, l.releases), (1, 1, 1, 0), "the left's open released nothing: {l:?}");
        assert_eq!((r.closes, r.grabs, r.grabs_applied), (1, 1, 1), "{r:?}");
    }

    /// Two members close on one ball in the same frame, over the in-process network (world-model.md 7.2: the first in
    /// the agreed order wins): neither machine's pick can see the other's grab yet, so both send one. One is applied
    /// and the other refused `Held` by the winner, both machines agree who holds the ball, and the loser's end line
    /// counts the refusal by its reason.
    #[test]
    fn two_members_closing_on_one_ball_in_one_frame_one_holds_it_and_the_other_is_refused_held() {
        use pe_agree::{Node, NodeConfig};
        use pe_seams::agree::{Cut, CutRef};
        use pe_seams::test_util::TestKeys;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let keys = TestKeys::new(11, 2);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let hub = Hub::default();
        let mut sims = Vec::new();
        for i in 0..2 {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), Pos::ORIGIN, 8);
            let replica =
                crate::world::ball_pit(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
            let slot = Arc::new(FrameSlot::new());
            let puppet = Puppet::default();
            puppet.set([0.0, 3.0, 0.4 * i as f32], false);
            let sim = Sim::new(Box::new(node), replica, clock.clone(), slot.clone(), Budget::new(RATE))
                .with_local(me, keys.nonce(i, 0), Arc::new(puppet.clone()))
                .with_transport(Box::new(Port::new(me, hub.clone())), None);
            sims.push((sim, slot, me, puppet));
        }
        let mut f = 0;
        let mut run = |sims: &mut Vec<(Sim, Arc<FrameSlot>, MemberKey, Puppet)>, n: u64| {
            for _ in 0..n {
                f += 1;
                let now = clock.host_time_of(Frame(f));
                hub.now.store(now.0, Ordering::SeqCst);
                for (s, ..) in sims.iter_mut() {
                    s.tick(now).expect("ticked");
                }
            }
        };
        run(&mut sims, 120);
        let v = sims[0].1.newest().expect("published");
        let ball = top_ball(&v);
        let at = rig_at(&v, ball);
        assert_eq!(top_ball(&sims[1].1.newest().expect("published")), ball, "one world");
        for (.., puppet) in &sims {
            puppet.set(at, false);
        }
        run(&mut sims, 3);
        for (.., puppet) in &sims {
            puppet.set(at, true);
        }
        run(&mut sims, 120);
        let claims: Vec<_> = sims
            .iter()
            .map(|(s, ..)| crate::api::snapshot_of(&*s.replica).expect("a world").claim(ball).map(|c| c.member))
            .collect();
        let winner = claims[0].expect("held");
        assert_eq!(claims[1], Some(winner), "both machines agree who holds it");
        for (s, slot, me, _) in &sims {
            let h = &s.stats().hands[1];
            assert_eq!((h.closes, h.grabs), (1, 1), "{me:?}: one close, one grab sent: {h:?}");
            let held = slot.newest().expect("published").objects.iter().find(|o| o.id == ball).expect("ball").held_by;
            assert_eq!(held.map(|h| h.member), Some(winner), "{me:?} sees the winner holding it");
            if *me == winner {
                assert_eq!((h.grabs_applied, h.grabs_refused.len()), (1, 0), "the winner's: {h:?}");
            } else {
                assert_eq!(h.grabs_applied, 0, "the loser's: {h:?}");
                assert_eq!(h.grabs_refused.get("Held"), Some(&1), "the loser's, refused Held: {h:?}");
            }
        }
    }

    /// B with mixed leads (@item:primordial-engine/vr-run-the-local-simulation-at-the-moment-the-he; the user accepts
    /// the asymmetry, to be measured): two members on a 20 ms one-way link close on one ball at the same
    /// real moment, with neither, one (VR beside a desktop) or both running the shown present 46 ms ahead. Each time
    /// both machines agree who holds it and on the world. Printed per mix: the winner, and each member's deepest
    /// rollback and frames re-simulated, which show who pays for the lead.
    #[test]
    fn mixed_display_leads_agree_on_the_race_and_the_world() {
        type Member = (Sim, Arc<FrameSlot>, MemberKey, Puppet, Arc<crate::shown::DisplayLead>);
        use pe_agree::{Node, NodeConfig};
        use pe_seams::agree::{Cut, CutRef};
        use pe_seams::test_util::TestKeys;
        for leads in [[false, false], [true, false], [true, true]] {
            let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
            let keys = TestKeys::new(11, 2);
            let members = keys.sorted_keys();
            let line = pe_world::preset::m1_line();
            let hub = Hub::default();
            {
                let mut d = hub.delay.lock().expect("the delays");
                d.insert((keys.key(0), keys.key(1)), 20_000_000);
                d.insert((keys.key(1), keys.key(0)), 20_000_000);
            }
            let mut sims = Vec::new();
            for i in 0..2 {
                let me = keys.key(i);
                let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
                let config = NodeConfig::new(line, members.clone(), genesis);
                let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
                let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), Pos::ORIGIN, 8);
                let replica =
                    crate::world::ball_pit(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
                let slot = Arc::new(FrameSlot::new());
                let puppet = Puppet::default();
                puppet.set([0.0, 3.0, 0.4 * i as f32], false);
                let lead = Arc::new(crate::shown::DisplayLead::default());
                let sim = Sim::new(Box::new(node), replica, clock.clone(), slot.clone(), Budget::new(RATE))
                    .with_local(me, keys.nonce(i, 0), Arc::new(puppet.clone()))
                    .with_transport(Box::new(Port::new(me, hub.clone())), None)
                    .with_shown(lead.clone());
                sims.push((sim, slot, me, puppet, lead));
            }
            let mut f = 0;
            let mut run = |sims: &mut Vec<Member>, n: u64| {
                for _ in 0..n {
                    f += 1;
                    let now = clock.host_time_of(Frame(f));
                    hub.now.store(now.0, Ordering::SeqCst);
                    for (k, (s, .., lead)) in sims.iter_mut().enumerate() {
                        if leads[k] {
                            lead.record(now, 46_000_000);
                        }
                        s.tick(now).expect("ticked");
                    }
                }
            };
            run(&mut sims, 120);
            let v = sims[0].1.newest().expect("published");
            let ball = top_ball(&v);
            let at = rig_at(&v, ball);
            for (.., puppet, _) in &sims {
                puppet.set(at, false);
            }
            run(&mut sims, 3);
            for (.., puppet, _) in &sims {
                puppet.set(at, true);
            }
            run(&mut sims, 120);
            let claims: Vec<_> = sims
                .iter()
                .map(|(s, ..)| crate::api::snapshot_of(&*s.replica).expect("a world").claim(ball).map(|c| c.member))
                .collect();
            let winner = claims[0].expect("held");
            assert_eq!(claims[1], Some(winner), "{leads:?}: both machines agree who holds it");
            let roots: Vec<_> = sims.iter().map(|(_, slot, ..)| slot.newest().expect("published")).collect();
            // One world: agreement compares both machines' roots at every cut, and both finalized with no split.
            for (s, ..) in &sims {
                let st = s.stats();
                assert!(
                    st.finalized > 0 && st.root_splits == 0,
                    "{leads:?}: finalized {}, splits {}",
                    st.finalized,
                    st.root_splits
                );
            }
            let counters: Vec<_> =
                roots.iter().map(|v| (v.counters.deepest_rollback, v.counters.frames_resimulated)).collect();
            println!(
                "leads {leads:?} (true = 46 ms ahead): winner member {} (the {}); deepest rollback / frames re-simulated: \
                 member 0 {:?}, member 1 {:?}; shown ahead {} and {} frames",
                usize::from(winner == sims[1].2),
                if leads[0] != leads[1] {
                    if (winner == sims[0].2) == leads[0] { "member ahead" } else { "member at the present" }
                } else {
                    "tie broken by agreement's order"
                },
                counters[0],
                counters[1],
                sims[0].0.stats().largest_shown_ahead,
                sims[1].0.stats().largest_shown_ahead
            );
        }
    }

    /// A puppet hand that also reports a velocity, the same at every moment: a hand moving steadily, for a throw.
    #[derive(Clone)]
    struct Moving(Puppet, [f32; 3]);

    impl pe_seams::HandFeed for Moving {
        fn hand_at(&self, hand: pe_seams::Hand, at: HostTime) -> Option<pe_seams::HandInput> {
            let h = self.0.hand_at(hand, at)?;
            Some(pe_seams::HandInput { linear_velocity: Some(self.1), angular_velocity: Some([0.0; 3]), ..h })
        }
    }

    /// The weak-throw fix between two members (world-model.md 8.1;
    /// @item:primordial-engine/throws-leave-at-the-hand-s-speed-letting-go-carr): one picks up the top ball and opens a
    /// hand moving at 2 m/s, so its opening goes as a `Throw`. Both machines agree on the world (agreement compares
    /// their roots at every cut: finalized, no split), and on both the ball has flown off along the throw.
    #[test]
    fn a_throw_by_one_member_lands_the_same_on_both() {
        use pe_agree::{Node, NodeConfig};
        use pe_seams::agree::{Cut, CutRef};
        use pe_seams::test_util::TestKeys;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let keys = TestKeys::new(11, 2);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let hub = Hub::default();
        {
            let mut d = hub.delay.lock().expect("the delays");
            d.insert((keys.key(0), keys.key(1)), 20_000_000);
            d.insert((keys.key(1), keys.key(0)), 20_000_000);
        }
        let thrower = Puppet::default();
        thrower.set([0.0, 3.0, 0.0], false);
        let idle = Puppet::default();
        idle.set([0.0, 3.0, 0.4], false);
        let feeds: [Arc<dyn pe_seams::HandFeed>; 2] =
            [Arc::new(Moving(thrower.clone(), [2.0, 0.0, 0.0])), Arc::new(idle)];
        let mut sims = Vec::new();
        for (i, feed) in feeds.into_iter().enumerate() {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), Pos::ORIGIN, 8);
            let replica =
                crate::world::ball_pit(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
            let slot = Arc::new(FrameSlot::new());
            let sim = Sim::new(Box::new(node), replica, clock.clone(), slot.clone(), Budget::new(RATE))
                .with_local(me, keys.nonce(i, 0), feed)
                .with_transport(Box::new(Port::new(me, hub.clone())), None);
            sims.push((sim, slot));
        }
        let mut f = 0;
        let mut run = |sims: &mut Vec<(Sim, Arc<FrameSlot>)>, n: u64| {
            for _ in 0..n {
                f += 1;
                let now = clock.host_time_of(Frame(f));
                hub.now.store(now.0, Ordering::SeqCst);
                for (s, _) in sims.iter_mut() {
                    s.tick(now).expect("ticked");
                }
            }
        };
        run(&mut sims, 120);
        let v = sims[0].1.newest().expect("published");
        let ball = top_ball(&v);
        let at = rig_at(&v, ball);
        thrower.set(at, false);
        run(&mut sims, 3);
        thrower.set(at, true);
        run(&mut sims, 30);
        thrower.set(at, false);
        run(&mut sims, 120);
        assert_eq!(sims[0].0.stats().hands[1].throws, 1, "the opening went as a throw");
        for (s, slot) in &sims {
            let st = s.stats();
            assert!(st.finalized > 0 && st.root_splits == 0, "finalized {}, splits {}", st.finalized, st.root_splits);
            let now = rig_at(&slot.newest().expect("published"), ball);
            assert!(now[0] - at[0] > 0.2, "the ball flew off along the throw: from {at:?} to {now:?}");
        }
    }

    /// A test hub's setting per one-way link, by (from, to).
    type ByLink<T> = Arc<std::sync::Mutex<std::collections::BTreeMap<(MemberKey, MemberKey), T>>>;

    /// An in-process network (S4) for `Sim`s in one test: what one sends, the addressee polls, stamped with the host
    /// time the test last set, plus the one-way delay from the sender to it (none unless set).
    #[derive(Clone, Default)]
    pub(crate) struct Hub {
        boxes: Arc<std::sync::Mutex<std::collections::BTreeMap<MemberKey, std::collections::VecDeque<Received>>>>,
        pub(crate) now: Arc<std::sync::atomic::AtomicI64>,
        /// One-way delays in host nanoseconds by (from, to): a message is polled once the test's time reaches its
        /// arrival. Each box is kept in arrival order, those arriving together in the order sent.
        delay: ByLink<i64>,
        /// Jittered one-way delays by (from, to), in place of `delay`: each message's is drawn uniformly from the
        /// range, inclusive, by a fixed sequence, so a run repeats.
        jitter: ByLink<(i64, i64)>,
        /// Extra delay by (from, to) for a repair-lane physics image (pe-diverge's `PhysicsReply`), on top of the
        /// link's: an image slower than the rest of the repair lane, as a large one from a busy source is (F1).
        image_late: ByLink<i64>,
        /// Messages sent, for the jitter's sequence.
        sent: Arc<std::sync::atomic::AtomicU64>,
    }

    pub(crate) struct Port {
        me: MemberKey,
        hub: Hub,
        events: Arc<std::sync::Mutex<Vec<NetEvent>>>,
    }

    impl Port {
        pub(crate) fn new(me: MemberKey, hub: Hub) -> Port {
            Port { me, hub, events: Arc::default() }
        }
    }

    impl Transport for Port {
        fn send(&mut self, to: MemberKey, lane: Lane, bytes: bytes::Bytes) {
            let jitter = self.hub.jitter.lock().expect("the jitter").get(&(self.me, to)).copied();
            let d = match jitter {
                Some((lo, hi)) => {
                    // splitmix64 over the count of messages sent.
                    let mut z = self.hub.sent.fetch_add(1, Ordering::SeqCst).wrapping_add(0x9e37_79b9_7f4a_7c15);
                    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                    z ^= z >> 31;
                    lo + (z % (hi - lo + 1) as u64) as i64
                }
                None => self.hub.delay.lock().expect("the delays").get(&(self.me, to)).copied().unwrap_or(0),
            };
            let late = self.hub.image_late.lock().expect("the image delays").get(&(self.me, to)).copied();
            let image =
                |b: &[u8]| matches!(pe_diverge::msg::Msg::decode(b), Ok(pe_diverge::msg::Msg::PhysicsReply { .. }));
            let d = match late {
                Some(late) if matches!(lane, Lane::Repair) && image(&bytes) => d + late,
                _ => d,
            };
            let at = HostTime(self.hub.now.load(Ordering::SeqCst) + d);
            let r = Received { from: self.me, lane, bytes, at };
            let mut boxes = self.hub.boxes.lock().expect("the hub");
            let q = boxes.entry(to).or_default();
            let i = q.partition_point(|m| m.at <= at);
            q.insert(i, r);
        }
        fn poll(&mut self) -> Option<Received> {
            let now = self.hub.now.load(Ordering::SeqCst);
            let mut boxes = self.hub.boxes.lock().expect("the hub");
            let q = boxes.get_mut(&self.me)?;
            if q.front()?.at.0 <= now { q.pop_front() } else { None }
        }
        fn link(&self, _: MemberKey) -> LinkStats {
            LinkStats::default()
        }
        fn events(&mut self) -> Vec<NetEvent> {
            std::mem::take(&mut *self.events.lock().expect("the events"))
        }
    }

    /// Counts what net.rs's hook would get: link-up events, and calls once a tick.
    struct CountingHook(Arc<std::sync::Mutex<(u64, u64)>>);

    impl NetHook for CountingHook {
        fn on_event(&mut self, event: &NetEvent, _: &mut dyn Agreement, _: &dyn WorldFacts) {
            if matches!(event, NetEvent::LinkUp { .. }) {
                self.0.lock().expect("the counts").0 += 1;
            }
        }
        fn on_tick(&mut self, _: HostTime, _: &mut dyn Agreement, _: &dyn WorldFacts) {
            self.0.lock().expect("the counts").1 += 1;
        }
    }

    /// The hook gets each event once, and runs once a tick even when no event came, so net.rs can poll pe-net's
    /// control end for lifecycle outcomes (@design-reviewer's Low on 5ca43067).
    #[test]
    fn the_net_hook_gets_each_event_and_runs_once_a_tick() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (s, _slot) = sim(clock.clone(), Budget::new(RATE));
        let port = Port::new(MemberKey([1; 32]), Hub::default());
        let events = port.events.clone();
        let counts = Arc::new(std::sync::Mutex::new((0, 0)));
        let mut s = s.with_transport(Box::new(port), Some(Box::new(CountingHook(counts.clone()))));
        s.tick(clock.host_time_of(Frame(1))).expect("ticked");
        events.lock().expect("the events").push(NetEvent::LinkUp { peer: MemberKey([2; 32]) });
        for f in 2..=4 {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        assert_eq!(*counts.lock().expect("the counts"), (1, 4), "one event, four ticks");
        assert_eq!(s.stats().net_events, 1);
    }

    /// Two members over an in-process network, each with its own pe-agree node and its own replica of a small ball
    /// pit with both admitted at genesis (pe-agree M1's fixed membership). Steps 1 and 3 carry agreement's
    /// messages, so each machine's view shows the other's hands where the other put them, and cuts become final with
    /// both members' votes.
    #[test]
    fn two_members_see_each_others_hands_and_finalize_over_a_network() {
        use pe_agree::{Node, NodeConfig};
        use pe_seams::agree::{Cut, CutRef};
        use pe_seams::test_util::TestKeys;
        use pe_seams::{Hand, Offset, Pos};
        const FRAMES: u64 = 180;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let keys = TestKeys::new(11, 2);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let hub = Hub::default();
        let at = [[-0.5, 1.0, -0.4], [0.5, 1.1, -0.3]];
        let mut sims = Vec::new();
        for (i, feed) in at.iter().enumerate() {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), Pos::ORIGIN, 8);
            let replica =
                crate::world::ball_pit(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
            let slot = Arc::new(FrameSlot::new());
            let sim = Sim::new(Box::new(node), replica, clock.clone(), slot.clone(), Budget::new(RATE))
                .with_local(me, keys.nonce(i, 0), Arc::new(StillHands(*feed)))
                .with_transport(Box::new(Port::new(me, hub.clone())), None);
            sims.push((sim, slot, me));
        }
        for f in 1..=FRAMES {
            let now = clock.host_time_of(Frame(f));
            hub.now.store(now.0, Ordering::SeqCst);
            for (s, _, _) in &mut sims {
                s.tick(now).expect("ticked");
            }
        }
        for (i, (s, slot, _)) in sims.iter().enumerate() {
            let (them, feed) = (sims[1 - i].2, at[1 - i]);
            let v = slot.newest().expect("published");
            for (side, dx) in [(Hand::Left, -0.2f64), (Hand::Right, 0.2)] {
                let o = v.objects.iter().find(|o| o.hand_of == Some((them, side))).expect("their hand");
                let [x, y, z] = feed.map(f64::from);
                let want = Pos::ORIGIN.checked_add(Offset([x + dx, y, z])).expect("in range");
                let d = (o.pos - want).0;
                assert!(d.iter().all(|c| c.abs() < 1e-4), "member {i} sees their {side:?} {d:?} off");
            }
            let st = s.stats();
            println!("member {i}: final {:?} at frame {:?}; {st:?}", v.final_frame, v.frame);
            // Final within a second of the present: the cut lag (0.25 s at first) plus two members' votes.
            assert!(v.final_frame.is_some_and(|f| f.0 + 60 >= FRAMES), "member {i}: final frame {:?}", v.final_frame);
            assert!(st.received > 0 && st.sent > 0 && st.votes > 0 && st.finalized > 0, "member {i}: {st:?}");
            assert_eq!((st.refused, st.unrouted, st.unsent, st.inputs_refused), (0, 0, 0, 0), "member {i}: {st:?}");
        }
    }

    /// The cut log (`PE_CUT_LOG`, [`crate::cutlog`]; the pull-ahead plan's 1.2): two members over the hub, each with
    /// its own log. Each logs every cut it finalizes once, with the root the cut is final with and `agreed`, and both
    /// logs name the same root at every cut both reached. The run also reports its finality lag and deepest rollback
    /// (G9).
    #[test]
    fn both_members_log_the_same_root_at_every_final_cut() {
        use crate::cutlog::{CutLog, parse, test_util::Shared};
        use pe_agree::{Node, NodeConfig};
        use pe_seams::Pos;
        use pe_seams::agree::{Cut, CutRef};
        use pe_seams::test_util::TestKeys;
        const FRAMES: u64 = 300;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let keys = TestKeys::new(11, 2);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let hub = Hub::default();
        let at = [[-0.5, 1.0, -0.4], [0.5, 1.1, -0.3]];
        let mut sims = Vec::new();
        let mut logs = Vec::new();
        for (i, feed) in at.iter().enumerate() {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), Pos::ORIGIN, 8);
            let replica =
                crate::world::ball_pit(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
            let buf = Shared::default();
            let log = CutLog::to_writer(Box::new(buf.clone()), HostTime(0)).expect("a cut log");
            logs.push(buf);
            let sim = Sim::new(Box::new(node), replica, clock.clone(), Arc::new(FrameSlot::new()), Budget::new(RATE))
                .with_local(me, keys.nonce(i, 0), Arc::new(StillHands(*feed)))
                .with_transport(Box::new(Port::new(me, hub.clone())), None)
                .with_cut_log(log);
            sims.push(sim);
        }
        for f in 1..=FRAMES {
            let now = clock.host_time_of(Frame(f));
            hub.now.store(now.0, Ordering::SeqCst);
            for s in &mut sims {
                s.tick(now).expect("ticked");
            }
        }
        let stats: Vec<SimStats> = sims.iter().map(|s| s.stats().clone()).collect();
        // Dropping each simulation drops its log, which flushes it.
        drop(sims);
        let cuts: Vec<_> = logs.iter().map(|b| parse(&b.text())).collect();
        for (i, (c, st)) in cuts.iter().zip(&stats).enumerate() {
            println!(
                "member {i}: {} cut lines, finalized {}, final lag {:?}, deepest rollback {}",
                c.len(),
                st.finalized,
                st.final_lag,
                st.deepest_rollback
            );
            // `finalized` also counts the driver's `NotReady` results, which write no line.
            assert!(c.len() >= 30 && c.len() as u64 <= st.finalized, "member {i}: {} lines, {st:?}", c.len());
            assert!(c.iter().all(|l| l.result == "agreed" && l.mine.is_none()), "member {i}: {c:?}");
            assert!(c.windows(2).all(|w| w[0].height < w[1].height), "member {i}: each cut once, in order");
            // Final within a second of the present, as the network test above.
            assert!(st.final_lag.samples() > 0 && st.final_lag.max() <= 60, "member {i}: {:?}", st.final_lag);
        }
        let key = |l: &crate::cutlog::CutLine| (l.line.clone(), l.height, l.frame);
        let theirs: BTreeMap<_, _> = cuts[1].iter().map(|l| (key(l), l.root.clone())).collect();
        let both: Vec<_> = cuts[0].iter().filter_map(|l| theirs.get(&key(l)).map(|r| (l, r))).collect();
        // Each finalizes at its own tick, so the newest cut or two may be on one side only.
        let lens: Vec<usize> = cuts.iter().map(Vec::len).collect();
        assert!(both.len() + 2 >= lens[0].max(lens[1]), "{} cuts in both logs, of {lens:?}", both.len());
        for (l, r) in both {
            assert_eq!(&l.root, r, "cut {} at frame {}: the two machines logged different roots", l.height, l.frame);
        }
    }

    /// [`FinalLag`]'s summary: exact below 1024 frames, one bucket above, and the `Debug` the end line prints.
    #[test]
    fn the_final_lag_summarizes_its_samples() {
        let mut l = FinalLag::default();
        assert_eq!((l.samples(), l.mean(), l.quantile(0.99), l.max()), (0, 0.0, 0, 0));
        for lag in 1..=100 {
            l.record(lag);
        }
        assert_eq!((l.samples(), l.max()), (100, 100));
        assert!((l.mean() - 50.5).abs() < 1e-9);
        assert_eq!((l.quantile(0.5), l.quantile(0.99), l.quantile(1.0)), (50, 99, 100));
        l.record(5000);
        assert_eq!((l.max(), l.quantile(1.0)), (5000, FinalLag::EXACT));
        assert_eq!(format!("{l:?}"), "FinalLag { ticks: 101, mean: 99.5, p99: 100, max: 5000 }");
    }

    /// A replica that damages the pit's balls once: after the first `advance` that reaches `at` or later, every
    /// moving ball's velocity gains 1 cm/s along x in the live physics world (pe-replica's test-only `damage_body`), as
    /// a machine's whose physics computed wrongly for a moment. Its saved states before that are untouched, so a
    /// replay from the last verified cut computes the honest state (rung 1, as pe-diverge's tests/kinds.rs). `at` is
    /// off the cut grid: a cut frame's live root is taken before the damage could reach it. Everything else is the
    /// replica's.
    struct Damaged {
        inner: ReplicaImpl<crate::fault::FaultWorld>,
        at: Option<Frame>,
        damaged: Arc<std::sync::atomic::AtomicU64>,
        /// From this frame on, the replica steps at half the clock's rate: a machine at half speed (F3, divergence.md
        /// 4.6). `half_base` is (its frame, the present) when the half rate started, moved up after an import jumps it.
        half_from: Option<Frame>,
        half_base: Option<(Frame, Frame)>,
    }

    impl Replica for Damaged {
        fn domain(&self) -> DomainId {
            self.inner.domain()
        }
        fn frame(&self) -> Frame {
            self.inner.frame()
        }
        fn set_budget(&mut self, simulated: Duration) {
            self.inner.set_budget(simulated)
        }
        fn advance(&mut self, present: Frame, source: &mut dyn pe_seams::EntrySource) -> pe_seams::Advance {
            let mut present = present;
            if self.half_from.is_some_and(|h| present >= h) {
                let mine = self.inner.frame();
                let (bf, bp) = *self.half_base.get_or_insert((mine, present));
                let allowed = Frame(bf.0 + present.0.saturating_sub(bp.0) / 2);
                if mine > allowed {
                    // A resync's import jumped the replica ahead: the half rate starts again from there.
                    self.half_base = Some((mine, present));
                }
                present = present.min(allowed.max(mine));
            }
            let a = self.inner.advance(present, source);
            if self.at.is_some_and(|at| self.inner.frame() >= at) {
                self.at = None;
                let ids: Vec<_> =
                    self.inner.view().objects.iter().filter(|o| o.hand_of.is_none()).map(|o| o.id).collect();
                let n = ids.into_iter().filter(|id| self.inner.damage_body(*id, [0.01, 0.0, 0.0])).count();
                self.damaged.fetch_add(n as u64, Ordering::SeqCst);
            }
            a
        }
        fn outcome(&self, entry: &EntryRef) -> Option<(Frame, pe_seams::Outcome)> {
            self.inner.outcome(entry)
        }
        fn root_at_cut(
            &mut self,
            cut: &Cut,
            source: &mut dyn pe_seams::EntrySource,
        ) -> Result<pe_seams::Hash, pe_seams::RootError> {
            self.inner.root_at_cut(cut, source)
        }
        fn finalize(
            &mut self,
            cut: &pe_seams::FinalCut,
            source: &mut dyn pe_seams::EntrySource,
        ) -> pe_seams::Finalized {
            self.inner.finalize(cut, source)
        }
        fn replay_to(
            &mut self,
            cut: &Cut,
            source: &mut dyn pe_seams::EntrySource,
        ) -> Result<pe_seams::Hash, pe_seams::RootError> {
            self.inner.replay_to(cut, source)
        }
        fn tree(&self, root: &pe_seams::Hash) -> Option<Box<dyn pe_seams::Tree + '_>> {
            self.inner.tree(root)
        }
        fn physics_image(&self, root: &pe_seams::Hash) -> Option<pe_seams::PhysicsImage> {
            self.inner.physics_image(root)
        }
        fn set_served(&mut self, roots: &[pe_seams::Hash]) {
            self.inner.set_served(roots)
        }
        fn import_at_cut(
            &mut self,
            group: &pe_seams::GroupSel,
            import: pe_seams::CutImport,
            source: &mut dyn pe_seams::EntrySource,
        ) -> Result<pe_seams::Hash, pe_seams::ImportError> {
            self.inner.import_at_cut(group, import, source)
        }
        fn view(&self) -> Arc<FrameView> {
            self.inner.view()
        }
        fn world_snapshot(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
            self.inner.world_snapshot()
        }
        fn agreed_snapshot(&self) -> Option<(Frame, Arc<dyn std::any::Any + Send + Sync>)> {
            self.inner.agreed_snapshot()
        }
        fn vote_holds(&self) -> Vec<Frame> {
            self.inner.vote_holds()
        }
        fn runner_identity(&self) -> Option<pe_seams::RunnerIdentity> {
            self.inner.runner_identity()
        }
        fn module(&self, hash: &pe_seams::Hash) -> Option<bytes::Bytes> {
            self.inner.module(hash)
        }
    }

    /// `n` members on one hub, each with pe-diverge and a 3 s freeze delay; the last one at half speed from frame 60,
    /// with `status` as its status line (F3's T2 and its n = 2 variant).
    #[allow(clippy::type_complexity)]
    fn half_speed_session(
        n: usize,
        clock: &Arc<dyn FrameClock>,
        hub: &Hub,
        status: &Arc<pe_render::notices::StatusLine>,
    ) -> Vec<(Sim, Arc<FrameSlot>, Vec<String>)> {
        use pe_agree::{Node, NodeConfig};
        use pe_seams::test_util::TestKeys;
        let keys = TestKeys::new(11, n as u32);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let at = [[-0.5, 1.0, -0.4], [0.5, 1.1, -0.3], [0.0, 1.0, 0.5]];
        let mut sims = Vec::new();
        for (i, feed) in at.iter().take(n).enumerate() {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), pe_seams::Pos::ORIGIN, 8);
            let inner =
                crate::world::ball_pit_replica(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
            let damaged = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let half_from = (i == n - 1).then_some(Frame(60));
            let replica = Box::new(Damaged { inner, at: None, damaged, half_from, half_base: None });
            let slot = Arc::new(FrameSlot::new());
            let mut sim = Sim::new(Box::new(node), replica, clock.clone(), slot.clone(), Budget::new(RATE))
                .with_local(me, keys.nonce(i, 0), Arc::new(StillHands(*feed)))
                .with_transport(Box::new(Port::new(me, hub.clone())), None)
                .with_diverge(pe_diverge::Config::default(), None)
                .expect("pe-diverge")
                .with_freeze_after(3.0);
            if i == n - 1 {
                sim = sim.with_status(status.clone());
            }
            sims.push((sim, slot, Vec::new()));
        }
        sims
    }

    /// F3 at n = 2 (@engine-architect's L2 on fc24bc9c; F3 S5, the user's ruling "2"): member 1 at half speed. Every
    /// cut needs its vote, so it can't resync; past the freeze delay (3 s here) it doesn't freeze. It says so once,
    /// its status says the session confirms late at its pace, and member 0's hands are never held.
    #[test]
    fn at_n_2_a_member_at_half_speed_never_freezes_and_says_the_session_confirms_late() {
        const FRAMES: u64 = 900;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let hub = Hub::default();
        let status = Arc::new(pe_render::notices::StatusLine::default());
        let mut sims = half_speed_session(2, &clock, &hub, &status);
        let (mut shown_late, mut shown_behind) = (0u64, 0u64);
        for f in 1..=FRAMES {
            let now = clock.host_time_of(Frame(f));
            hub.now.store(now.0, Ordering::SeqCst);
            for (s, _, notices) in sims.iter_mut() {
                s.tick(now).expect("ticked");
                notices.extend(s.take_notices());
                assert!(!s.resyncing(), "nobody freezes at n = 2");
            }
            match status.get() {
                Some(t) if t.contains("confirming") => shown_late += 1,
                Some(t) if t.starts_with("Behind by") => shown_behind += 1,
                _ => {}
            }
        }
        let (slow, fast) = (&sims[1], &sims[0]);
        println!(
            "late {shown_late} behind {shown_behind}; slow {:?}; notices {:?}; fast inputs {}",
            slow.0.stats(),
            slow.2,
            fast.0.stats().inputs
        );
        assert_eq!(slow.0.stats().resyncs_started, 0, "no resync at n = 2");
        assert!(slow.0.stats().lag_max_ms >= 2000, "it fell behind: {:?}", slow.0.stats());
        assert_eq!(slow.2.iter().filter(|n| n.starts_with("slow:")).count(), 1, "told once: {:?}", slow.2);
        assert!(shown_behind > 0 && shown_late > 0, "behind first, then confirming late: {shown_behind} {shown_late}");
        // Never held (@engine-architect's nit on fe61d5f1: `hands_paced` is 0 since S1, so count what went in): both
        // hands at the floor at least, every frame from the first.
        let fs = fast.0.stats();
        let gap = (u64::from(RATE.hz()) / HAND_FLOOR_HZ).max(1);
        assert!(
            fs.inputs + fs.inputs_refused >= 2 * (FRAMES / gap) - 4,
            "the other member's hands went in at the floor: {} + {}",
            fs.inputs,
            fs.inputs_refused
        );
        for (i, (s, _, _)) in sims.iter().enumerate() {
            let d = s.diverge_counters().expect("pe-diverge");
            assert_eq!((d.root_splits, d.diverged), (0, 0), "member {i}: {d:?}");
        }
    }

    /// F3 (@item:primordial-engine/a-member-that-can-t-simulate-in-real-time-freeze; divergence.md 4.6): three members,
    /// member 2 at half speed from frame 60. It falls behind and says so on its status line. Past the freeze delay (3 s
    /// here, a minute in play) it never freezes (the user's; @coordinator-opus): it is
    /// told once, keeps playing and stepping, and starts no resync. The other two finalize throughout without it, and
    /// nobody's roots ever split.
    #[test]
    fn a_member_at_half_speed_among_three_never_freezes_and_the_others_finalize_without_it() {
        const FRAMES: u64 = 1500;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let hub = Hub::default();
        let status = Arc::new(pe_render::notices::StatusLine::default());
        let mut sims = half_speed_session(3, &clock, &hub, &status);
        let (mut shown_behind, mut shown_late, mut frozen_ticks) = (0u64, 0u64, 0u64);
        let mut final_gap_max = [0u64; 2];
        for f in 1..=FRAMES {
            let now = clock.host_time_of(Frame(f));
            hub.now.store(now.0, Ordering::SeqCst);
            for (i, (s, slot, notices)) in sims.iter_mut().enumerate() {
                s.tick(now).expect("ticked");
                notices.extend(s.take_notices());
                if i == 2 {
                    frozen_ticks += u64::from(s.resyncing());
                } else if f > 120 {
                    let fin = slot.newest().and_then(|v| v.final_frame).map_or(0, |fr| fr.0);
                    final_gap_max[i] = final_gap_max[i].max(f.saturating_sub(fin));
                }
            }
            match status.get() {
                Some(t) if t.contains("confirming") => shown_late += 1,
                Some(t) if t.starts_with("Behind by") => shown_behind += 1,
                _ => {}
            }
        }
        for (i, (s, slot, notices)) in sims.iter().enumerate() {
            let v = slot.newest().expect("published");
            println!(
                "member {i}: final {:?} at frame {:?}; diverge {:?}; {:?}; notices {notices:?}",
                v.final_frame,
                v.frame,
                s.diverge_counters(),
                s.stats()
            );
        }
        let slow = &sims[2].0;
        assert_eq!(frozen_ticks, 0, "it never froze");
        assert_eq!(slow.stats().resyncs_started, 0, "and started no resync");
        assert!(slow.stats().lag_max_ms >= 2000, "it fell behind: {:?}", slow.stats());
        assert!(
            shown_behind > 0 && shown_late == 0,
            "behind, never 'confirming late' at n = 3: {shown_behind} {shown_late}"
        );
        let notices = &sims[2].2;
        assert_eq!(notices.iter().filter(|n| n.starts_with("slow:")).count(), 1, "told once: {notices:?}");
        for (i, (s, _, _)) in sims.iter().enumerate() {
            let d = s.diverge_counters().expect("pe-diverge");
            assert_eq!((d.root_splits, d.diverged, d.rung_failures), (0, 0, 0), "member {i}: {d:?}");
        }
        // The other two finalize throughout: within 2 s of the present after the start.
        assert!(final_gap_max.iter().all(|g| *g <= 2 * u64::from(RATE.hz())), "{final_gap_max:?}");
    }

    /// What [`split_run`] saw on one member.
    #[derive(Debug)]
    struct SplitSide {
        stats: SimStats,
        final_frame: Option<Frame>,
        /// Ticks whose published health read `Repairing`.
        repairing_ticks: u64,
        health: Health,
        /// pe-diverge's split episodes started and ended, and the frame of one still open.
        episodes: (u64, u64),
        open: Option<Frame>,
        damaged: u64,
    }

    /// Two members over sim.rs's hub, each with a pe-agree node and the ball pit, as in
    /// [`two_members_see_each_others_hands_and_finalize_over_a_network`]. Member 1's replica damages its balls at
    /// frame 121, so the two roots differ from the next cut and S3 reports a root split (n = 2: no root reaches a
    /// quorum). `wired` runs pe-diverge on both; member 1's start-up digest is made to differ, so its self-test's verdict is
    /// `Failed`, as a machine's whose fault the self-test catches.
    fn split_run(wired: bool) -> (Vec<SplitSide>, u64) {
        use pe_agree::{Node, NodeConfig};
        use pe_seams::test_util::TestKeys;
        const FRAMES: u64 = 720;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let keys = TestKeys::new(11, 2);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let hub = Hub::default();
        let at = [[-0.5, 1.0, -0.4], [0.5, 1.1, -0.3]];
        let mut sims = Vec::new();
        let mut counts = Vec::new();
        for (i, feed) in at.iter().enumerate() {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), pe_seams::Pos::ORIGIN, 8);
            let inner =
                crate::world::ball_pit_replica(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
            let damaged = Arc::new(std::sync::atomic::AtomicU64::new(0));
            counts.push(damaged.clone());
            let replica = Box::new(Damaged {
                inner,
                at: (i == 1).then_some(Frame(121)),
                damaged,
                half_from: None,
                half_base: None,
            });
            let slot = Arc::new(FrameSlot::new());
            let mut sim = Sim::new(Box::new(node), replica, clock.clone(), slot.clone(), Budget::new(RATE))
                .with_local(me, keys.nonce(i, 0), Arc::new(StillHands(*feed)))
                .with_transport(Box::new(Port::new(me, hub.clone())), None);
            if wired {
                sim = sim.with_diverge(pe_diverge::Config::default(), None).expect("pe-diverge");
                if i == 1 {
                    sim.diverge.as_mut().expect("wired").set_startup_digest(Err("a different machine".into()));
                }
            }
            sims.push((sim, slot, 0u64));
        }
        for f in 1..=FRAMES {
            let now = clock.host_time_of(Frame(f));
            hub.now.store(now.0, Ordering::SeqCst);
            for (s, _, repairing) in &mut sims {
                s.tick(now).expect("ticked");
                *repairing += u64::from(matches!(s.health(), Health::Repairing { .. }));
            }
        }
        let sides = sims
            .into_iter()
            .zip(counts)
            .map(|((s, slot, repairing_ticks), damaged)| SplitSide {
                final_frame: slot.newest().expect("published").final_frame,
                health: s.health(),
                episodes: s.diverge_counters().map_or((0, 0), |c| (c.root_splits, c.root_splits_ended)),
                open: s.diverge.as_ref().and_then(|d| d.d.open_since()),
                stats: s.stats().clone(),
                repairing_ticks,
                damaged: damaged.load(Ordering::SeqCst),
            })
            .collect();
        (sides, FRAMES)
    }

    /// @item:primordial-engine/pe-app-runs-pe-diverge-app-md-step-5-and-health-r's acceptance (@coordinator-opus): an injected root split opens an episode, health reads `Repairing` meanwhile, and
    /// pe-diverge heals it, so finality resumes and health reads `Agreed` again on both members. Step 1 or the next cut
    /// heals it: member 1's replay from the last verified cut computes the honest state, and its vote in the cut's next
    /// view carries that root. A revote can be refused when the split's view has moved on, which is no fault; which
    /// members' land, if any, varies from run to run, since the replica's budget follows measured time.
    #[test]
    fn a_root_split_opens_an_episode_and_pe_diverge_heals_it() {
        let (sides, frames) = split_run(true);
        for (i, side) in sides.iter().enumerate() {
            println!("member {i}: {side:?}");
        }
        assert!(sides[1].damaged > 0, "the damage ran");
        assert!(sides.iter().all(|s| s.stats.root_splits >= 1), "a root split opened on both");
        assert!(sides.iter().any(|s| s.repairing_ticks > 0), "health read Repairing while it was open");
        // Not started == ended: an episode a newer split replaces is started and never ended (@engine-architect's nit).
        assert!(sides.iter().all(|s| s.episodes.1 >= 1 && s.open.is_none()), "pe-diverge's episodes ended");
        // A member asked agreement for a revote. Whether it goes in is timing: tier 2 on windows-x64 (run
        // 44222657480) saw both refused, and every other check here held. On a slower machine the next cut can also
        // overtake one member's replay, which then asks for none (ladder.rs, a later agreeing cut), so not `all`.
        assert!(sides.iter().any(|s| s.stats.revotes + s.stats.revotes_refused >= 1), "a member revoted");
        assert_eq!(
            (sides[1].stats.self_tests, sides[1].stats.self_tests_failed),
            (1, 1),
            "member 1's self-test failed"
        );
        assert_eq!(
            (sides[0].stats.self_tests, sides[0].stats.self_tests_failed),
            (1, 0),
            "member 0's self-test passed"
        );
        for (i, s) in sides.iter().enumerate() {
            assert_eq!(s.stats.root_splits, s.stats.root_splits_ended, "member {i}: every split ended");
            assert_eq!(s.health, Health::Agreed, "member {i}: agreed again");
            assert!(s.final_frame.is_some_and(|f| f.0 + 60 >= frames), "member {i}: final {:?}", s.final_frame);
            assert_eq!((s.stats.repair_refused, s.stats.splits_unknown), (0, 0), "member {i}");
        }
    }

    /// The red half: the same split with no pe-diverge stays open for good. Finality stops, and health reads
    /// `Repairing` from the split to the end (pe-app's split set, slice b).
    #[test]
    fn without_pe_diverge_a_root_split_never_heals() {
        let (sides, frames) = split_run(false);
        for (i, s) in sides.iter().enumerate() {
            println!("member {i}: {s:?}");
            assert!(s.stats.root_splits >= 1 && s.stats.root_splits_ended < s.stats.root_splits, "member {i}");
            assert!(matches!(s.health, Health::Repairing { .. }), "member {i}: {:?}", s.health);
            assert!(s.final_frame.is_none_or(|f| f.0 + 300 < frames), "member {i}: final {:?}", s.final_frame);
        }
    }

    /// What [`fault_run`] saw on one member.
    #[derive(Debug)]
    struct FaultSide {
        me: MemberKey,
        stats: SimStats,
        /// pe-diverge's counters at the end (`stats.diverge` is filled only for the end line).
        diverge: pe_diverge::Counters,
        final_frame: Option<Frame>,
        health: Health,
        /// pe-diverge's cures by rung, rung 1 first.
        cured: [u64; 5],
        /// pe-diverge's split episodes started.
        episodes: u64,
        /// The radius this member's views drew the fault's ball at: the largest seen, and the last.
        radius_max: f32,
        radius_last: Option<f32>,
        /// Every notice it printed.
        notices: Vec<String>,
        /// Each bundle it wrote: its rung and its `build` line.
        bundles: Vec<(u8, String)>,
        /// The first root split's cut, and this member's own votes there (S3's `Voted`, in order: its live vote, then
        /// pe-diverge's revote with its step-1 replay's root), and the root that cut was final with, if it was.
        split: Option<CutRef>,
        split_votes: Vec<pe_seams::Hash>,
        split_final: Option<pe_seams::Hash>,
        /// In a [`FaultCase::hold`] run, who holds the held ball at the end, as this member's world says.
        held: Option<(MemberKey, ObjectId)>,
        /// Its cut log (`PE_CUT_LOG`, [`crate::cutlog`]), read back: the cuts, and the rollbacks' lines (1.11).
        cuts: CutLines,
        rollbacks: RollbackLines,
    }

    /// A member's rollback lines, read back; its `Debug` gives the count.
    struct RollbackLines(Vec<crate::cutlog::RollbackLine>);

    impl std::fmt::Debug for RollbackLines {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{} rollback lines", self.0.len())
        }
    }

    /// A member's cut log, read back; its `Debug` counts the lines by result instead of printing each.
    struct CutLines(Vec<crate::cutlog::CutLine>);

    impl std::fmt::Debug for CutLines {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let n = |r: &str| self.0.iter().filter(|l| l.result == r).count();
            let (agreed, diverged, damaged) = (n("agreed"), n("diverged"), n("damaged"));
            write!(f, "{} lines ({agreed} agreed, {diverged} diverged, {damaged} damaged)", self.0.len())
        }
    }

    /// How [`fault_run_with`]'s two members are linked, and what member 0's hands do.
    #[derive(Clone, Copy, Debug, Default)]
    struct FaultCase {
        /// Each message between the two members this late, both ways, in host nanoseconds.
        delay_ns: i64,
        /// Member 0's physics images to member 1 this much later again ([`Hub::image_late`]).
        image_late_ns: i64,
        /// Member 0's left hand closes on the highest ball at frame 93 and lifts it 5 mm a frame from frame 104 to
        /// 160, through the split (the fault fires at 121), its step-1 replay and the repair; it holds it to the end.
        hold: bool,
    }

    /// The fault's ball's radius in `view`, if it is drawn as a sphere.
    fn ball_radius(view: &FrameView, ball: pe_seams::ObjectId) -> Option<f32> {
        view.objects.iter().find(|o| o.id == ball).and_then(|o| match o.shape {
            Some(pe_seams::Shape::Sphere { radius }) => Some(radius),
            _ => None,
        })
    }

    /// Two members over sim.rs's hub, each with a pe-agree node, the ball pit and pe-diverge, as [`split_run`]. Member 1
    /// runs `fault` ([`crate::fault`]): its replica doubles the highest ball's radius at the end of the fault's frame,
    /// each time it steps that frame, and from the first firing its self-test reports `Failed` by injection. With
    /// `typed`, the console's `fault` is typed for it before the tick of that host frame. Each member writes its
    /// bundles to a folder of its own, which the run reads back and removes.
    fn fault_run(fault: &Arc<crate::fault::Fault>, typed: Option<u64>) -> (Vec<FaultSide>, u64) {
        fault_run_over(fault, typed, 0)
    }

    /// [`fault_run`] with each message between the two members `delay_ns` late, both ways.
    fn fault_run_over(fault: &Arc<crate::fault::Fault>, typed: Option<u64>, delay_ns: i64) -> (Vec<FaultSide>, u64) {
        fault_run_with(fault, typed, FaultCase { delay_ns, ..FaultCase::default() })
    }

    /// [`fault_run`] as `case` says.
    fn fault_run_with(fault: &Arc<crate::fault::Fault>, typed: Option<u64>, case: FaultCase) -> (Vec<FaultSide>, u64) {
        use pe_agree::{Node, NodeConfig};
        use pe_seams::test_util::TestKeys;
        const FRAMES: u64 = 720;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let keys = TestKeys::new(11, 2);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let hub = Hub::default();
        if case.delay_ns > 0 {
            let mut d = hub.delay.lock().expect("the delays");
            d.insert((members[0], members[1]), case.delay_ns);
            d.insert((members[1], members[0]), case.delay_ns);
        }
        if case.image_late_ns > 0 {
            hub.image_late.lock().expect("the image delays").insert((keys.key(0), keys.key(1)), case.image_late_ns);
        }
        let puppet = Puppet::default();
        let at = [[-0.5, 1.0, -0.4], [0.5, 1.1, -0.3]];
        // A folder of this run's own, beside the test executable: the fault tests run in parallel in one process.
        static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let run = RUNS.fetch_add(1, Ordering::SeqCst);
        let exe = std::env::current_exe().expect("the test executable");
        let base = exe.parent().expect("its folder").join(format!("pe-app-fault-run-{}-{run}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut sims = Vec::new();
        for (i, feed) in at.iter().enumerate() {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), pe_seams::Pos::ORIGIN, 8);
            let mine = (i == 1).then(|| fault.clone());
            let replica =
                crate::world::ball_pit_replica_with(world, &members, me, RATE, clock.clone(), runner(), mine.clone())
                    .expect("a replica");
            let slot = Arc::new(FrameSlot::new());
            let hands: Arc<dyn pe_seams::HandFeed> = if case.hold && i == 0 {
                // Both hands tracked and open where `StillHands` would put them, until the left takes the ball.
                puppet.set_hand(0, [feed[0] - 0.2, feed[1], feed[2]], false);
                puppet.set_hand(1, [feed[0] + 0.2, feed[1], feed[2]], false);
                Arc::new(puppet.clone())
            } else {
                Arc::new(StillHands(*feed))
            };
            let events = Arc::new(std::sync::Mutex::new(Vec::new()));
            let cut_log = crate::cutlog::test_util::Shared::default();
            let log = crate::cutlog::CutLog::to_writer(Box::new(cut_log.clone()), HostTime(0)).expect("a cut log");
            let mut sim = Sim::new(Box::new(node), Box::new(replica), clock.clone(), slot.clone(), Budget::new(RATE))
                .with_local(me, keys.nonce(i, 0), hands)
                .with_transport(Box::new(Port::new(me, hub.clone())), None)
                .with_event_log(events.clone())
                .with_cut_log(log)
                .with_diverge(pe_diverge::Config::default(), Some(base.join(format!("member{i}"))))
                .expect("pe-diverge");
            if let Some(f) = mine {
                sim = sim.with_fault(f);
            }
            sims.push((sim, slot, Vec::new(), 0f32, events, cut_log));
        }
        let mut held: Option<(ObjectId, [f32; 3])> = None;
        for f in 1..=FRAMES {
            let now = clock.host_time_of(Frame(f));
            hub.now.store(now.0, Ordering::SeqCst);
            if typed == Some(f) {
                println!("typed: {}", crate::fault::on_command(Some(fault)).expect("`fault` goes in"));
            }
            for (s, slot, notices, radius_max, _, _) in &mut sims {
                s.tick(now).expect("ticked");
                notices.extend(s.take_notices());
                let drawn = fault.ball().zip(slot.newest()).and_then(|(b, v)| ball_radius(&v, b));
                *radius_max = radius_max.max(drawn.unwrap_or(0.0));
            }
            // The hold, as the grab test's: to the ball open, closed three frames later, lifted after ten more.
            match (case.hold, f, held) {
                (true, 90, _) => {
                    let v = sims[0].1.newest().expect("published");
                    let ball = top_ball(&v);
                    let at = rig_at(&v, ball);
                    puppet.set_hand(0, at, false);
                    held = Some((ball, at));
                }
                (true, 93, Some((_, at))) => puppet.set_hand(0, at, true),
                (true, 104..=160, Some((_, at))) => {
                    puppet.set_hand(0, [at[0], at[1] + 0.005 * (f - 103) as f32, at[2]], true);
                }
                _ => {}
            }
        }
        let sides: Vec<FaultSide> = (sims.into_iter().enumerate())
            .map(|(i, (s, slot, notices, radius_max, events, cut_log))| {
                let view = slot.newest().expect("published");
                let (stats, health) = (s.stats().clone(), s.health());
                let cured = s.diverge_counters().map_or([0; 5], |c| c.cured_by_rung);
                let episodes = s.diverge_counters().map_or(0, |c| c.root_splits);
                let diverge = s.diverge_counters().cloned().unwrap_or_default();
                let claim = |ball| crate::api::snapshot_of(&*s.replica).expect("a world").claim(ball);
                let held = held.and_then(|(ball, _)| claim(ball)).map(|c| (c.member, c.hand));
                let events = events.lock().expect("the events").clone();
                let split = events.iter().find_map(|e| match e {
                    AgreeEvent::RootSplit { cut, .. } => Some(*cut),
                    _ => None,
                });
                let split_votes = (events.iter())
                    .filter_map(|e| match e {
                        AgreeEvent::Voted { cut, root, .. } if Some(*cut) == split => Some(*root),
                        _ => None,
                    })
                    .collect();
                let split_final = events.iter().find_map(|e| match e {
                    AgreeEvent::CutFinal { cut, root, .. } if Some(*cut) == split => Some(*root),
                    _ => None,
                });
                // Dropping the simulation closes its I/O thread, which first writes every queued bundle, and
                // flushes its cut log.
                drop(s);
                let cuts = CutLines(crate::cutlog::parse(&cut_log.text()));
                let rollbacks = RollbackLines(crate::cutlog::parse_rollbacks(&cut_log.text()));
                let dir = base.join(format!("member{i}"));
                let mut bundles: Vec<(u8, String)> = (std::fs::read_dir(&dir).into_iter().flatten().flatten())
                    .map(|e| std::fs::read(e.path()).expect("a bundle"))
                    .map(|bytes| pe_diverge::FaultBundle::decode(&bytes).expect("a bundle decodes"))
                    .map(|b| (b.rung, b.build))
                    .collect();
                bundles.sort();
                FaultSide {
                    me: keys.key(i),
                    stats,
                    diverge,
                    final_frame: view.final_frame,
                    health,
                    cured,
                    episodes,
                    radius_max,
                    radius_last: fault.ball().and_then(|b| ball_radius(&view, b)),
                    notices,
                    bundles,
                    split,
                    split_votes,
                    split_final,
                    held,
                    cuts,
                    rollbacks,
                }
            })
            .collect();
        let _ = std::fs::remove_dir_all(&base);
        for (i, s) in sides.iter().enumerate() {
            println!("member {i}: {s:?}");
        }
        println!("fault: {fault:?}");
        (sides, FRAMES)
    }

    /// The cut logs through a split and its repair (`PE_CUT_LOG`, the pull-ahead plan's 1.2), at n = 2. The driver
    /// finalizes each final cut once, and here every final cut needs both members' votes. So the split's cut goes
    /// final only after member 1's repair has revoted member 0's root: each log carries it once, `agreed`, at member
    /// 0's live root, and no line says `diverged` or `damaged` (those need a cut final without this machine's vote).
    /// Both logs name the same root at every cut both reached, and finality goes on after the repair.
    #[test]
    fn both_cut_logs_carry_the_split_cut_once_agreed_at_the_reference_s_root() {
        let fault = crate::fault::Fault::new(Frame(121));
        let (sides, _) = fault_run(&fault, None);
        let split = sides[1].split.expect("member 1 saw the root split");
        let reference = *sides[0].split_votes.first().expect("member 0 voted at the split");
        for (i, s) in sides.iter().enumerate() {
            let lines = &s.cuts.0;
            let other: Vec<_> = lines.iter().filter(|l| l.result != "agreed").collect();
            assert!(other.is_empty(), "member {i}: {other:?}");
            assert!(lines.windows(2).all(|w| w[0].height < w[1].height), "member {i}: each cut once, in order");
            let at: Vec<_> = lines.iter().filter(|l| l.height == split.height).collect();
            println!("member {i}: the split cut's lines {at:?}");
            assert_eq!(at.len(), 1, "member {i}: {at:?}");
            assert_eq!(at[0].frame, split.frame.0, "member {i}: {at:?}");
            assert_eq!(at[0].root, reference.to_string(), "member {i}: final at member 0's live root");
            let last = lines.last().map(|l| l.height);
            assert!(last.is_some_and(|h| h > split.height + 10), "member {i}: finality went on? last {last:?}");
        }
        let key = |l: &crate::cutlog::CutLine| (l.line.clone(), l.height, l.frame);
        let theirs: BTreeMap<_, _> = sides[1].cuts.0.iter().map(|l| (key(l), l.root.clone())).collect();
        let both: Vec<_> = sides[0].cuts.0.iter().filter_map(|l| theirs.get(&key(l)).map(|r| (l, r))).collect();
        assert!(both.len() >= 50, "{} cuts in both logs", both.len());
        for (l, r) in both {
            assert_eq!(&l.root, r, "cut {} at frame {}: the two machines logged different roots", l.height, l.frame);
        }
    }

    /// The rollback-cause line (1.11; @coordinator-opus; @design-reviewer's M1 on 6408fdbe): two members
    /// 30 ms apart each way, the fault never firing. Each rolls back for the other's late entries and writes one line
    /// per rollback in its cut log. Each line names the changed frame, at or above the keyframe restored, and the
    /// other member's entries there. Keyframes are 4 frames apart at 60 Hz, so many changes fall between two, and
    /// there `to` alone would point at the wrong frame. No code commit runs, so no line names a prediction's end.
    #[test]
    fn each_rollback_s_line_names_the_entries_it_stepped_again_for() {
        let fault = crate::fault::Fault::new(Frame(1_000_000));
        let (sides, _) = fault_run_over(&fault, None, 30_000_000);
        for (i, s) in sides.iter().enumerate() {
            let rb = &s.rollbacks.0;
            let between = rb.iter().filter(|r| r.changed.is_some_and(|f| f > r.to)).count();
            println!(
                "member {i}: {} rollbacks, {} lines, {between} changed above the keyframe; first {:?}",
                s.stats.phases.rollbacks,
                rb.len(),
                rb.first()
            );
            assert!(!rb.is_empty(), "member {i} rolled back over the slow link");
            assert_eq!((rb.len() as u64, s.stats.rollback_lines), (s.stats.phases.rollbacks, s.stats.phases.rollbacks));
            assert!(rb.iter().all(|r| r.depth >= 1 && r.to < r.present + 60), "member {i}: {rb:?}");
            // Every line has its cause: a change at or above the keyframe, and no prediction.
            let bad: Vec<_> = rb.iter().filter(|r| r.replay.is_some() || r.changed.is_none_or(|f| f < r.to)).collect();
            assert!(bad.is_empty(), "member {i}: {bad:?}");
            assert!(between > 0, "member {i}: some change fell between two keyframes");
            // What changed there: the other member's entry at the changed frame, or a void.
            let explained =
                |r: &&crate::cutlog::RollbackLine| r.at.iter().any(|e| !e.starts_with('*')) || !r.voided.is_empty();
            let unexplained: Vec<_> = rb.iter().filter(|r| !explained(r)).collect();
            assert!(unexplained.is_empty(), "member {i}: {} of {}: {unexplained:?}", unexplained.len(), rb.len());
        }
    }

    /// @item:primordial-engine/pe-app-inject-fault-at-a-runtime-flag-that-alter (M1 demo step 7; @coordinator-opus): the fault on member 1 makes the next cut's roots differ, a root split at n = 2. Member 1's
    /// rung 1 can't cure it, since its replay steps frame 121 again; its self-test fails by injection, so member 0 is
    /// the reference, and member 1's rung 2 fetches the ball's record and restores it. Both write bundles, member 1's
    /// saying the failure was injected, and finality resumes on equal roots.
    #[test]
    fn an_injected_fault_is_found_and_rung_2_restores_the_ball() {
        let fault = crate::fault::Fault::new(Frame(121));
        let (sides, frames) = fault_run(&fault, None);
        a_fault_was_found_and_rung_2_restored_the_ball(&sides, &fault, frames);
    }

    /// The same with `--arm-fault` (@coordinator-opus): the console's `fault`, typed before the tick of
    /// host frame 121, fires at the next frame the replica steps, which the console names, and the repair is the same.
    #[test]
    fn a_typed_fault_fires_at_the_next_frame_and_rung_2_restores_the_ball() {
        let fault = crate::fault::Fault::typed();
        assert!(crate::fault::on_command(None).is_err(), "refused without --arm-fault");
        let (sides, frames) = fault_run(&fault, Some(121));
        let at = fault.at().expect("the frame was set");
        assert!((119..=123).contains(&at.0), "the next frame: {at:?}");
        let told = format!("FAULT: the test fault fires at the end of frame {} (--arm-fault)", at.0);
        assert!(sides[1].notices.contains(&told), "{told}: {:?}", sides[1].notices);
        assert!(crate::fault::on_command(Some(&fault)).is_err(), "refused once its frame is set");
        a_fault_was_found_and_rung_2_restored_the_ball(&sides, &fault, frames);
    }

    /// What `replay_run` adds to the two members' plain run.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Case {
        Plain,
        /// The control: one of B's entries dropped from A's kept log.
        Dropped,
        /// The forged pair (`replay_run`).
        Void,
        /// Its control: the pair, with seq 2 put back into A's kept log.
        VoidKept,
        /// M1's demo step 8: B leaves at frame 300, and once its `Leave` has settled, B stops and A goes on alone.
        Leave,
    }

    /// What `replay_run` saw.
    struct Ran {
        verdict: crate::replay::Verdict,
        /// The final cuts A kept.
        cuts: usize,
        /// The forged pair's fate on A, in the void cases.
        forged: Forged,
        /// The frame B's `Leave` had settled by, in [`Case::Leave`].
        settled: Option<u64>,
    }

    /// What became of `replay_run`'s forged pair on A.
    #[derive(Debug, Default)]
    struct Forged {
        /// The pair's void events on A: the entry's index in the pair (0 for seq 1), the reason and the deciding cut.
        voided: Vec<(usize, pe_seams::agree::VoidReason, pe_seams::agree::CutRef)>,
        /// Seq 1's last outcome on A: the frame, and whether it applied (so the final state has its object).
        applied: Option<(Frame, bool)>,
        /// Whether A's replica ever stepped seq 2 before the cut voided it, which its predicted log decides.
        stepped: bool,
    }

    /// Two members from the M1 interim's two-member genesis record over sim.rs's hub, B's closed hands sweeping through
    /// the pit's balls, and A keeping its final log for `--check-replay` ([`crate::replay`]). After 600 frames, A
    /// replays its session from frame 0 ([`Case`] says what else happens).
    ///
    /// The void cases: at frame 300 both members receive two edits, each creating an object, on a second chain of B's
    /// that B's sim never made, signed by B, as a faulty B's entries would come. Seq 1 is stamped 2 frames after a grid
    /// frame `g` 6 cuts past A's final frame, and seq 2 one frame after `g`: below seq 1's, so back-stamped. B's own
    /// agreement refuses to submit an entry like seq 2 (`BelowOwnStamp`). No grid frame lies between `g` and either
    /// stamp, so the first final cut past the pair includes both, on a parent at or below `g`: seq 1 is effective, and
    /// seq 2 void as back-stamped (S3 v1.1). A's log records the deciding cut's own frames, `g` and up, when that cut is
    /// final, so a final log that kept seq 2 would make the replay differ. A late entry can't show that: it is stamped
    /// below its deciding cut's parent, and A recorded those frames before the entry came.
    fn replay_run(case: Case) -> Ran {
        use pe_agree::Node;
        use pe_seams::test_util::TestKeys;
        const FRAMES: u64 = 600;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let keys = TestKeys::new(17, 2);
        let (a, b) = (keys.key(0), keys.key(1));
        let params = crate::genesis::Params {
            nonce: [7; 32],
            build: b"pe test build".to_vec(),
            opener: a,
            joiner: Some(b),
            rate: RATE,
            display_delay_ms: 0,
            gravity: pe_world::GRAVITY,
            seed: 5,
            balls: 8,
            origin: pe_seams::Pos::ORIGIN,
            epoch_ns: 0,
        };
        let (genesis, replica_a, config_a) =
            crate::genesis::Genesis::ball_pit(&params, clock.clone(), runner()).expect("A's record");
        let every = config_a.cut_every(RATE.hz());
        assert!(every >= 3, "the pair's two stamps fit between grid frames");
        let (replica_b, config_b) = genesis.build(b, clock.clone(), runner()).expect("B's replica");
        let hub = Hub::default();
        let sweep = clock.host_time_of(Frame(FRAMES - 120));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut sims = Vec::new();
        for (i, (me, replica, config)) in [(a, replica_a, config_a), (b, replica_b, config_b)].into_iter().enumerate() {
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let hands: Arc<dyn pe_seams::HandFeed> =
                if i == 1 { Arc::new(SweepHands::until(sweep)) } else { Arc::new(StillHands([-0.5, 1.0, -0.4])) };
            let mut sim =
                Sim::new(Box::new(node), replica, clock.clone(), Arc::new(FrameSlot::new()), Budget::new(RATE))
                    .with_local(me, keys.nonce(i, 0), hands)
                    .with_transport(Box::new(Port::new(me, hub.clone())), None);
            if i == 0 {
                sim = sim
                    .with_replay_check(crate::replay::ReplayLog::new(genesis.clone()))
                    .with_event_log(events.clone());
            }
            sims.push(sim);
        }
        let mut pair: Vec<pe_seams::EntryRef> = Vec::new();
        let mut void_entry = None;
        let mut forged = Forged::default();
        let mut settled = None;
        for f in 1..=FRAMES {
            let now = clock.host_time_of(Frame(f));
            hub.now.store(now.0, Ordering::SeqCst);
            // Once its leave has settled, B has quit.
            let running = if settled.is_some() { 1 } else { 2 };
            for s in &mut sims[..running] {
                s.tick(now).expect("ticked");
            }
            if case == Case::Leave && f == 300 {
                assert_eq!(sims[1].leave_status(), LeaveStatus::Pending);
                sims[1].leave();
            } else if case == Case::Leave && f > 300 && settled.is_none() {
                settled = (sims[1].leave_status() == LeaveStatus::Settled).then_some(f);
            }
            if matches!(case, Case::Void | Case::VoidKept) && f == 300 {
                use pe_world::edit::{Actor, Op, Target, Transaction};
                let create = || {
                    let op = Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![] };
                    let tx = Transaction {
                        inline: vec![],
                        actor: Actor::Person,
                        label: None,
                        preconditions: vec![],
                        ops: vec![op],
                        blobs: vec![],
                    };
                    tx.encode()
                };
                let g = sims[0].agree.final_cut().expect("A has a final cut").cut.at.frame.0 + 6 * every;
                let mut chain = crate::local::Chain::new(sims[0].agree.line(), b, keys.nonce(1, 1));
                let mut signed = Vec::new();
                for stamp in [g + 2, g + 1] {
                    let e = chain
                        .entry(sims[0].replica.domain(), Frame(stamp), pe_seams::Class::Edit, create())
                        .sign(&keys.signer(1));
                    chain.took(&e);
                    signed.push(e);
                }
                pair = signed.iter().map(|e| e.id()).collect();
                void_entry = Some(signed[1].clone());
                let bytes = bytes::Bytes::from(pe_agree::codec::encode(&pe_agree::codec::Wire::Entries(signed)));
                let mut boxes = hub.boxes.lock().expect("the hub");
                // A has them from B, and B's node from A, as if relayed: B's sim never made this chain.
                for (to, from) in [(a, b), (b, a)] {
                    let q = boxes.entry(to).or_default();
                    let i = q.partition_point(|m| m.at <= now);
                    q.insert(i, Received { from, lane: pe_seams::Lane::Action, bytes: bytes.clone(), at: now });
                }
            }
            if let [seq1, seq2] = pair[..] {
                // The replica keeps outcomes only for a while: the last one seen is the final cut's.
                if let Some((at, outcome)) = sims[0].replica.outcome(&seq1) {
                    forged.applied = Some((at, matches!(outcome, pe_seams::Outcome::Applied { .. })));
                }
                forged.stepped |= sims[0].replica.outcome(&seq2).is_some();
            }
        }
        if case == Case::Leave {
            assert!(settled.is_some(), "B's leave settled");
            assert_eq!(sims[0].other_members(), Vec::<MemberKey>::new(), "A's epoch is A alone");
        }
        let a_sim = &mut sims[0];
        forged.voided = events
            .lock()
            .expect("the events")
            .iter()
            .filter_map(|e| match e {
                AgreeEvent::Voided { entry, reason, cut, .. } => {
                    pair.iter().position(|x| x == entry).map(|i| (i, *reason, *cut))
                }
                _ => None,
            })
            .collect();
        let log = a_sim.replay.as_mut().expect("A keeps its log");
        match case {
            Case::Dropped => assert!(log.drop_entry(|e| e.entry.author != b), "one of B's entries to drop"),
            Case::VoidKept => log.add_entry(void_entry.expect("the pair")),
            Case::Plain | Case::Void | Case::Leave => {}
        }
        let cuts = log.cuts();
        let replay = a_sim.start_replay().expect("armed").expect("the replay's thread");
        Ran { verdict: replay.join().expect("the replay ends"), cuts, forged, settled }
    }

    /// @item:primordial-engine/m1-acceptance-the-live-run-equals-the-replay-a-r: A's replay of its session from frame 0
    /// reaches every final cut's root, B's hand entries and the pit's contacts included.
    #[test]
    fn the_openers_replay_from_frame_0_reaches_every_final_cuts_root() {
        let Ran { verdict, cuts, .. } = replay_run(Case::Plain);
        println!("{}", verdict.line());
        // About 96 on an idle machine; a slow one finalizes fewer, and any number above a handful shows the replay ran.
        assert!(cuts >= 30, "A kept {cuts} final cuts");
        assert!(
            matches!(verdict, crate::replay::Verdict::Equal { cuts: n, last, modules: 0 } if n == cuts && last.0 >= 300),
            "{verdict:?}"
        );
    }

    /// The control: a log missing one of B's entries replays to another root, and the replay names the first final cut
    /// whose root differs.
    #[test]
    fn a_replay_of_a_log_missing_an_entry_differs() {
        let verdict = replay_run(Case::Dropped).verdict;
        println!("{}", verdict.line());
        assert!(matches!(verdict, crate::replay::Verdict::Differs { .. }), "{verdict:?}");
    }

    /// @design-reviewer's L1 on cbd81a02: a void entry inside its deciding cut's frames. The cut
    /// voids seq 2 of `replay_run`'s forged pair as back-stamped and makes seq 1 effective, and S3's final log, which A
    /// kept, has seq 1 and not seq 2. The replay steps exactly that log, and reaches every final cut's root: the agreed
    /// roots were taken with the cut's effective entries only (`Replica::root_at_cut`).
    #[test]
    fn the_replay_equals_the_live_run_with_a_void_entry() {
        use pe_seams::agree::VoidReason;
        let Ran { verdict, cuts, forged, .. } = replay_run(Case::Void);
        println!("{} ({forged:?})", verdict.line());
        assert!(matches!(forged.voided[..], [(1, VoidReason::BackStamped, _)]), "only seq 2 void, as back-stamped");
        assert!(matches!(forged.applied, Some((_, true))), "seq 1 applied on A, so the replay has to step it");
        assert!(
            matches!(verdict, crate::replay::Verdict::Equal { cuts: n, .. } if n == cuts && cuts >= 30),
            "{verdict:?}"
        );
    }

    /// The void case's control: A's kept log with seq 2 put back, as a final log that kept the void entry would be,
    /// replays to another root at exactly the cut that voided it.
    #[test]
    fn a_replay_of_a_log_that_kept_a_void_entry_differs() {
        use pe_seams::agree::VoidReason;
        let Ran { verdict, forged, .. } = replay_run(Case::VoidKept);
        println!("{} ({forged:?})", verdict.line());
        let [(1, VoidReason::BackStamped, decided)] = forged.voided[..] else {
            panic!("only seq 2 void, as back-stamped: {forged:?}");
        };
        assert!(matches!(verdict, crate::replay::Verdict::Differs { cut, .. } if cut == decided), "{verdict:?}");
    }

    /// M1's demo step 9 after step 8 (@coordinator-opus): B leaves at frame 300, and once its `Leave` has
    /// settled B quits, and A goes on alone, in an epoch of A alone. A's replay then reaches every final cut's root, from
    /// frame 0 through the epoch change to A's cuts alone.
    #[test]
    fn the_replay_equals_the_live_run_after_a_member_leaves() {
        let Ran { verdict, cuts, settled, .. } = replay_run(Case::Leave);
        let settled = settled.expect("B's leave settled");
        println!("{} (B's leave settled by frame {settled})", verdict.line());
        assert!(
            matches!(verdict, crate::replay::Verdict::Equal { cuts: n, last, .. } if n == cuts && last.0 >= settled + 60),
            "{verdict:?}"
        );
    }

    /// @item:primordial-engine/at-n-2-a-root-split-s-first-rung-2-repair-fails-t's acceptance (@coordinator-opus): over a link 50 ms late each way, member 1's rung 2 outlasts several cut intervals, so agreement
    /// proposes the split's height again many times while it runs. It still ends in a final cut with equal roots, in
    /// one episode on each member. Before the split cut was pinned (agreement.md 3, L4') it failed: each re-proposal was
    /// a new episode, which overtook the repair.
    #[test]
    fn a_fault_over_a_slow_link_heals_in_one_episode() {
        let fault = crate::fault::Fault::new(Frame(121));
        let (sides, frames) = fault_run_over(&fault, None, 50_000_000);
        for (i, s) in sides.iter().enumerate() {
            assert_eq!(s.episodes, 1, "member {i}: one split episode: {s:?}");
        }
        a_fault_was_found_and_rung_2_restored_the_ball(&sides, &fault, frames);
    }

    /// @item:primordial-engine/m1-redo-f1-a-s-replay-didn-t-reproduce-its-live-r (the M1 redo's step 7, 2026-11-20): at
    /// n = 2 the reference's physics image reaches member 1 400 ms after the rest of the repair lane, as a large image
    /// from a busy reference did in the redo. Rung 2 still cures the split in one episode, and the reference's live
    /// vote at the split cut is the root the cut is final with: its step-1 replay reproduced it.
    ///
    /// Before F1's fix the split never ended [V, red on 74314c81]. Member 1 gave up on the image after `t_peer`
    /// (250 ms) and asked again, and member 0 answered every new ask within `t_join_peer` (5 s) of a served image
    /// `NotHeld`, as flooding, which fails the rung on its only source. In this run [I, from its counters and the
    /// ball drawn at its own size again] the late image from the first ask reached the second one and was imported,
    /// and the `NotHeld` that followed still failed the rung. Member 1 held the reference's state but never voted
    /// it, so the cut stayed split.
    #[test]
    fn a_physics_image_slower_than_t_peer_still_heals_the_split() {
        let fault = crate::fault::Fault::new(Frame(121));
        let case = FaultCase { image_late_ns: 400_000_000, ..FaultCase::default() };
        let (sides, frames) = fault_run_with(&fault, None, case);
        the_split_healed_to_the_references_live_root(&sides);
        a_fault_was_found_and_rung_2_restored_the_ball(&sides, &fault, frames);
    }

    /// The same with member 0's hand holding a ball through the split, its step-1 replay and the repair, lifting it
    /// as it goes: the redo's A held objects in its hands, which the dry runs never did. The ball is the highest, so
    /// it is also the one the fault doubles on member 1. Rung 2 cures it, the reference's replay of the held ball's
    /// frames gives its live root, and both members agree that hand still holds the ball. Before F1's fix it failed as
    /// the redo did [V, red on 74314c81]: 40 rung failures, 38 of them member 0's `NotHeld`.
    #[test]
    fn a_split_with_a_held_ball_heals_to_the_references_live_root() {
        let fault = crate::fault::Fault::new(Frame(121));
        let case = FaultCase { image_late_ns: 400_000_000, hold: true, ..FaultCase::default() };
        let (sides, frames) = fault_run_with(&fault, None, case);
        the_split_healed_to_the_references_live_root(&sides);
        a_fault_was_found_and_rung_2_restored_the_ball(&sides, &fault, frames);
        let held = (sides[0].held, sides[1].held);
        assert!(held.0.is_some_and(|(m, _)| m == sides[0].me) && held.0 == held.1, "held: {held:?}");
        assert_eq!(
            sides[0].stats.hands[0].grabs_applied, 1,
            "member 0's left hand took it: {:?}",
            sides[0].stats.hands
        );
    }

    /// Both members saw one split, at one cut, and that cut became final with member 0's first vote there, so member
    /// 0's step-1 replay (its revote) gave the root it had voted live.
    fn the_split_healed_to_the_references_live_root(sides: &[FaultSide]) {
        let (a, b) = (&sides[0], &sides[1]);
        for (i, s) in sides.iter().enumerate() {
            let d = &s.diverge;
            println!(
                "member {i}: rung_failures {} source_faults {} stale_replies {} serve_refused {} repair_bytes {}",
                d.rung_failures, d.source_faults, d.stale_replies, d.serve_refused, d.repair_bytes
            );
        }
        for (i, s) in sides.iter().enumerate() {
            assert_eq!(s.episodes, 1, "member {i}: one split episode: {s:?}");
        }
        assert!(a.split.is_some() && a.split == b.split, "one split cut: {:?} {:?}", a.split, b.split);
        let live = a.split_votes.first().copied();
        assert!(live.is_some(), "member 0 voted at the split cut: {:?}", a.split_votes);
        assert!(
            a.split_votes.iter().all(|r| Some(*r) == live),
            "member 0's revote is its live root: {:?}",
            a.split_votes
        );
        assert_eq!((a.split_final, b.split_final), (live, live), "the split cut is final with member 0's live root");
    }

    /// What both fault tests assert of a run.
    fn a_fault_was_found_and_rung_2_restored_the_ball(sides: &[FaultSide], fault: &crate::fault::Fault, frames: u64) {
        let ball = fault.ball().expect("the fault fired");
        let (a, b) = (&sides[0], &sides[1]);
        let original = pe_world::preset::BALL_R as f32;
        assert!((b.radius_max - 2.0 * original).abs() < 1e-6, "member 1 drew ball {ball:?} doubled: {b:?}");
        assert!((a.radius_max - original).abs() < 1e-6, "member 0 never drew it changed: {a:?}");
        assert_eq!((a.radius_last, b.radius_last), (Some(original), Some(original)), "restored on member 1");
        for (i, s) in sides.iter().enumerate() {
            assert!(s.stats.root_splits >= 1, "member {i}: a root split");
            assert_eq!(s.stats.root_splits, s.stats.root_splits_ended, "member {i}: every split ended");
            assert_eq!(s.health, Health::Agreed, "member {i}: agreed again");
            assert!(s.final_frame.is_some_and(|f| f.0 + 60 >= frames), "member {i}: final {:?}", s.final_frame);
            assert_eq!((s.stats.repair_refused, s.stats.splits_unknown), (0, 0), "member {i}");
            assert_eq!(s.stats.self_tests_failed, 0, "member {i}: the real self-test passed");
        }
        // The split healed in a pinned view, where a member's vote is a re-cast unless pe-diverge's revote cast it
        // afresh: the driver votes each cut once. Here rung 2 spans several pinned views, so re-casts happen, and the
        // hub runs are deterministic (@design-reviewer). If a change ever trips this, check member 1's
        // cured vote rather than weaken the assert.
        let recasts = (a.stats.recasts, b.stats.recasts);
        assert!(recasts.0 + recasts.1 >= 1, "re-cast votes in the pinned views: {recasts:?}");
        assert!(
            b.cured[1] >= 1 && a.cured[1] == 0,
            "member 1 cured by rung 2, member 0 not repaired: {:?} {:?}",
            a.cured,
            b.cured
        );
        assert!(a.stats.self_tests >= 1 && a.stats.self_tests_injected == 0, "member 0's self-test: {:?}", a.stats);
        assert!(b.stats.self_tests_injected >= 1, "member 1's self-test failed by injection");
        let said = |s: &FaultSide, what: &str| s.notices.iter().any(|n| n.contains(what));
        let (fired, failed) =
            (format!("FAULT INJECTED ({})", fault.source()), format!("BY INJECTION ({}", fault.source()));
        assert!(said(b, &fired) && said(b, &failed), "{fired} and {failed}: {b:?}");
        assert!(!said(a, "INJECT"), "member 0 says nothing of it: {:?}", a.notices);
        assert!(!a.bundles.is_empty() && a.bundles.iter().all(|(_, build)| !build.contains("INJECTION")), "{a:?}");
        assert!(b.bundles.iter().any(|(rung, _)| *rung == 2), "member 1's rung-2 bundle: {:?}", b.bundles);
        let bundled = format!("BY INJECTION: {} changed ball", fault.source());
        assert!(b.bundles.iter().all(|(_, build)| build.contains(&bundled)), "{bundled}: {b:?}");
        // Rung 1's replay, or a root's re-step, stepped the fault's frame again with the fault.
        assert!(fault.steps() >= 2, "the fault's frame was stepped again with the fault: {}", fault.steps());
    }

    /// B's hands, closed (physical under the default `CLOSED_HAND` mode), circling 0.3 m around half a turn apart, a
    /// turn every `period` seconds (3 s: 0.63 m/s), the left one through the front row of the pit's balls, until
    /// `until`, then still where they were. With `spin`, each also turns about its forearm (x) at that many rad/s.
    /// `offset` shifts the whole motion in time, in seconds.
    struct SweepHands {
        until: HostTime,
        period: f64,
        spin: f64,
        offset: f64,
    }

    impl SweepHands {
        /// The sweep at 0.63 m/s until `until`, with no turn.
        fn until(until: HostTime) -> SweepHands {
            SweepHands { until, period: 3.0, spin: 0.0, offset: 0.0 }
        }

        /// The rig-frame position of `hand` at host time `at` (the rig is at the origin).
        fn at(&self, hand: pe_seams::Hand, at: HostTime) -> [f64; 3] {
            let t = self.t(at);
            let (cx, phase) = if hand == pe_seams::Hand::Left { (-0.45, 0.0) } else { (0.45, std::f64::consts::PI) };
            let a = std::f64::consts::TAU * t / self.period + phase;
            [cx + 0.3 * a.cos(), 0.08, -0.85 + 0.3 * a.sin()]
        }

        /// The motion's time at host time `at`, in seconds.
        fn t(&self, at: HostTime) -> f64 {
            at.min(self.until).0 as f64 * 1e-9 + self.offset
        }

        /// The tracking speed of a hand's origin, in m/s.
        fn speed(&self) -> f64 {
            std::f64::consts::TAU * 0.3 / self.period
        }
    }

    impl pe_seams::HandFeed for SweepHands {
        fn hand_at(&self, hand: pe_seams::Hand, at: HostTime) -> Option<pe_seams::HandInput> {
            let [x, y, z] = self.at(hand, at);
            let h = self.spin * self.t(at) / 2.0;
            let orientation = [h.sin() as f32, 0.0, 0.0, h.cos() as f32];
            Some(pe_seams::HandInput {
                hand,
                source: pe_seams::HandSource::Tracked,
                sampled_at: at,
                tracked: true,
                pose: pe_seams::RigPose { position: [x as f32, y as f32, z as f32], orientation },
                linear_velocity: None,
                angular_velocity: None,
                grip: 1.0,
                closed: true,
                discontinuity: 0,
            })
        }
    }

    /// @coordinator-opus, tests 1 and 3, with two pe-world members: B's messages reach A 300 ms late and
    /// A's reach B in 25 ms, and B's closed hands sweep through the balls, so every frame brings A an entry 18 frames
    /// behind its present. Both budgets have no share (the floors alone), so the result doesn't depend on how fast
    /// this build steps. Before the keep-up floor covered the last call's need, A's view held from the first late
    /// entry on. Now it is at the present on nearly every tick; both members finalize cuts that cover the sweep, so
    /// they computed the same roots there (q = 2 of 2); and once B's hands are still, both show the same objects at
    /// the same frame.
    #[test]
    fn late_entries_on_every_frame_no_longer_hold_a_two_member_view() {
        use pe_agree::{Node, NodeConfig};
        use pe_seams::Pos;
        use pe_seams::agree::{Cut, CutRef};
        use pe_seams::test_util::TestKeys;
        const FRAMES: u64 = 240;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let still = clock.host_time_of(Frame(150));
        let keys = TestKeys::new(11, 2);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let hub = Hub::default();
        let (a, b) = (keys.key(0), keys.key(1));
        {
            let mut d = hub.delay.lock().expect("the delays");
            d.insert((b, a), 300_000_000);
            d.insert((a, b), 25_000_000);
        }
        let mut sims = Vec::new();
        for i in 0..2 {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), Pos::ORIGIN, 8);
            let replica =
                crate::world::ball_pit(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
            let slot = Arc::new(FrameSlot::new());
            let hands: Arc<dyn pe_seams::HandFeed> =
                if i == 0 { Arc::new(StillHands([-0.5, 1.0, -0.4])) } else { Arc::new(SweepHands::until(still)) };
            // No stamp-ahead: this test is the receivers' side, every frame bringing A an entry 18 frames late.
            let sim = Sim::new(Box::new(node), replica, clock.clone(), slot.clone(), Budget::with_share(RATE, 0.0))
                .with_local(me, keys.nonce(i, 0), hands)
                .with_transport(Box::new(Port::new(me, hub.clone())), None)
                .with_stamp_lead(None);
            sims.push((sim, slot));
        }
        let (mut current, mut most) = (0u64, 0u64);
        for f in 1..=FRAMES {
            let now = clock.host_time_of(Frame(f));
            hub.now.store(now.0, Ordering::SeqCst);
            let (_, report) = sims[0].0.tick(now).expect("ticked");
            sims[1].0.tick(now);
            most = most.max(report.advance.resimulated + report.advance.stepped);
            if f > 60 && sims[0].1.newest().is_some_and(|v| v.frame == Frame(f + 1)) {
                current += 1;
            }
        }
        assert!(most > 18, "A's largest call stepped {most} frames: the late frames were re-stepped");
        let after = FRAMES - 60;
        let budget = sims[0].0.budget();
        assert!(
            current >= after * 9 / 10,
            "A's view was at the present on {current} of {after} ticks (budget {} frames, cost {:?}, keep-up cap {})",
            budget.frames(),
            budget.cost(),
            budget.keep_up_cap()
        );
        let (va, vb) = (sims[0].1.newest().expect("A's view"), sims[1].1.newest().expect("B's view"));
        for (i, v) in [&va, &vb].into_iter().enumerate() {
            assert!(v.final_frame.is_some_and(|f| f.0 >= 150), "member {i}: final frame {:?}", v.final_frame);
        }
        // B's hands have been still for 1.5 s, five times the 300 ms that A's view of them trails by.
        assert_eq!(va.frame, vb.frame);
        let state = |v: &FrameView| {
            v.objects
                .iter()
                .map(|o| (o.id, o.pos, o.rotation, o.linear_velocity, o.angular_velocity))
                .collect::<Vec<_>>()
        };
        assert_eq!(state(&va), state(&vb));
    }

    /// Stamp-ahead's acceptance (@item:primordial-engine/pe-app-stamps-a-slow-member-s-entries-ahead-by-i; app.md
    /// 3.2): B's messages reach A `lo_ms..=hi_ms` late (uniform, delivered in arrival order), A's reach B in 25 ms;
    /// B's closed hands sweep the balls and B makes an edit every 10 frames, with M1's stamp-ahead settings. Returns
    /// B's lead at the end, B's entries voided as late over the whole run (@design-reviewer counted none there
    /// either), the share of ticks past 6 s at which A's view was at the present, both final frames, and the edits B
    /// made.
    fn a_slow_member_run(lo_ms: i64, hi_ms: i64, stamp_ahead: bool) -> (u64, usize, f64, [u64; 2], u64) {
        use crate::api::{Facts, Ports};
        use pe_agree::{Node, NodeConfig};
        use pe_api::queue::Submission;
        use pe_seams::agree::{Cut, CutRef};
        use pe_seams::test_util::TestKeys;
        use pe_seams::{Pos, VoidReason};
        use pe_world::edit::{Actor, Op, Target, Transaction};
        const FRAMES: u64 = 720;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let keys = TestKeys::new(11, 2);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let hub = Hub::default();
        let (a, b) = (keys.key(0), keys.key(1));
        hub.jitter.lock().expect("the jitter").insert((b, a), (lo_ms * 1_000_000, hi_ms * 1_000_000));
        hub.delay.lock().expect("the delays").insert((a, b), 25_000_000);
        let mut sims = Vec::new();
        let mut b_ports = None;
        for i in 0..2 {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), Pos::ORIGIN, 8);
            let replica =
                crate::world::ball_pit(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
            let slot = Arc::new(FrameSlot::new());
            let log = Arc::new(std::sync::Mutex::new(Vec::new()));
            let hands: Arc<dyn pe_seams::HandFeed> = if i == 0 {
                Arc::new(StillHands([-0.5, 1.0, -0.4]))
            } else {
                Arc::new(SweepHands::until(HostTime(i64::MAX)))
            };
            let ports = (i == 1).then(|| {
                let snapshot = crate::api::snapshot_of(&*replica).expect("a pe-world replica");
                let facts = Facts { world_name: "Pit".into(), line, rate: RATE, me: Some(me) };
                Ports::new(snapshot, facts, || {})
            });
            let mut sim = Sim::new(Box::new(node), replica, clock.clone(), slot.clone(), Budget::new(RATE))
                .with_local(me, keys.nonce(i, 0), hands)
                .with_transport(Box::new(Port::new(me, hub.clone())), None)
                .with_event_log(log.clone());
            if let Some(p) = &ports {
                sim = sim.with_api(p.clone());
            }
            if i == 1 && !stamp_ahead {
                sim = sim.with_stamp_lead(None);
            }
            b_ports = b_ports.or(ports);
            sims.push((sim, slot, log));
        }
        let ports = b_ports.expect("B's ports");
        let (mut current, mut counted) = (0u64, 0u64);
        for f in 1..=FRAMES {
            let now = clock.host_time_of(Frame(f));
            hub.now.store(now.0, Ordering::SeqCst);
            if f % 10 == 0 {
                let create = Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![] };
                let tx = Transaction {
                    inline: vec![],
                    actor: Actor::Person,
                    label: None,
                    preconditions: vec![],
                    ops: vec![create],
                    blobs: vec![],
                };
                ports.queue.submit(Submission { tx, base: 1 }).expect("room in the queue");
            }
            sims[0].0.tick(now);
            sims[1].0.tick(now);
            if f > 360 {
                counted += 1;
                current += u64::from(sims[0].1.newest().is_some_and(|v| v.frame == Frame(f + 1)));
            }
        }
        let late = sims
            .iter()
            .map(|(_, _, log)| {
                let log = log.lock().expect("the log");
                let late = |e: &&AgreeEvent| {
                    matches!(e, AgreeEvent::Voided { author, reason: VoidReason::Late, .. } if *author == b)
                };
                log.iter().filter(late).count()
            })
            .max()
            .unwrap_or(0);
        let fin = |i: usize| sims[i].1.newest().and_then(|v| v.final_frame).map_or(0, |f| f.0);
        let lead = sims[1].0.stats().lead;
        (lead, late, current as f64 / counted as f64, [fin(0), fin(1)], sims[1].0.stats().edits)
    }

    /// The acceptance on a fixed 1.2 s link: B's lead settles at the excess (72 - 6 + 3 = 69 frames, give or take the
    /// p95's rounding), none of B's entries, hands or edits, is voided as late, from the start, A's view stays at the
    /// present, and both members keep finalizing.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "12 s of two members in the ball pit: release (--include-ignored)")]
    fn a_slow_member_stamps_ahead_and_none_of_its_entries_is_void_on_a_fixed_link() {
        let (lead, late, current, fin, edits) = a_slow_member_run(1200, 1200, true);
        println!(
            "fixed 1.2 s: lead {lead}, late voids {late}, A at present {current:.3}, finals {fin:?}, edits {edits}"
        );
        assert!((66..=72).contains(&lead), "B's lead {lead}");
        assert_eq!(late, 0, "B's entries voided as late");
        assert!(current >= 0.9, "A's view at the present on {current:.3} of the ticks");
        // Still finalizing past the first 6 s: cuts that name stamped-ahead entries become final. A's final frame
        // trails by about 3.6 s here (B's votes take 1.2 s each way round), which the acceptance doesn't bound.
        assert!(fin.iter().all(|&f| f > 360), "final frames {fin:?}");
        assert!(edits >= 70, "B's edits {edits}");
    }

    /// The acceptance on a jittered 0.9-1.5 s link: the lead follows the p95, so nothing of B's is voided as late.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "12 s of two members in the ball pit: release (--include-ignored)")]
    fn a_slow_member_stamps_ahead_and_none_of_its_entries_is_void_on_a_jittered_link() {
        let (lead, late, current, fin, edits) = a_slow_member_run(900, 1500, true);
        println!("0.9-1.5 s: lead {lead}, late voids {late}, A at present {current:.3}, finals {fin:?}, edits {edits}");
        assert!((70..=93).contains(&lead), "B's lead {lead}");
        assert_eq!(late, 0, "B's entries voided as late");
        assert!(current >= 0.8, "A's view at the present on {current:.3} of the ticks");
        assert!(fin.iter().all(|&f| f > 360), "final frames {fin:?}");
        assert!(edits >= 70, "B's edits {edits}");
    }

    /// The red control: the same fixed link without stamp-ahead voids B's entries as late, or stalls finality.
    #[test]
    #[ignore = "a control, in release: without stamp-ahead the slow member's entries void or finality stalls"]
    fn without_stamp_ahead_a_slow_member_s_entries_void_or_finality_stalls() {
        let (lead, late, current, fin, edits) = a_slow_member_run(1200, 1200, false);
        println!("no lead: lead {lead}, late voids {late}, A at present {current:.3}, finals {fin:?}, edits {edits}");
        assert!(late > 0 || fin.iter().any(|&f| f < 540), "late voids {late}, finals {fin:?}");
    }

    fn with_replica(replica: Box<dyn Replica>, clock: Arc<dyn FrameClock>, budget: Budget) -> (Sim, Arc<FrameSlot>) {
        let slot = Arc::new(FrameSlot::new());
        let sim = Sim::new(Box::new(SoloAgreement::new(LineId::origin())), replica, clock, slot.clone(), budget);
        (sim, slot)
    }

    #[test]
    fn the_budget_starts_at_the_floor_and_never_goes_below_it() {
        let mut b = Budget::new(RATE);
        assert_eq!(b.frames(), Budget::FLOOR_FRAMES);
        assert_eq!(RATE.frames(b.current()), 2, "the duration gives back the frames");
        // A frame costing a whole period fits less than one frame in half a period: the floor holds.
        assert_eq!(b.observe(10, Duration::from_millis(167)), None);
        assert_eq!(b.frames(), 2);
        // Cheap frames raise it, up to MAX.
        let mut cheap = Budget::new(RATE);
        let raised = cheap.observe(100, Duration::from_nanos(100)).expect("raised");
        assert_eq!(cheap.frames(), RATE.frames(Budget::MAX));
        assert_eq!(RATE.frames(raised), cheap.frames());
        // A share of 0 keeps it at the floor whatever the cost.
        let mut pinned = Budget::with_share(RATE, 0.0);
        assert_eq!(pinned.observe(100, Duration::from_nanos(100)), None);
        assert_eq!(pinned.frames(), 2);
    }

    #[test]
    fn the_budget_follows_the_measured_cost() {
        let mut b = Budget::new(RATE);
        // 1 ms a frame at 60 Hz: half of a 16.67 ms period fits 8 frames.
        b.observe(4, Duration::from_millis(4));
        assert_eq!(b.frames(), 8);
        // The cost is smoothed: one slow call moves it only part of the way.
        b.observe(1, Duration::from_millis(9));
        assert!(b.frames() < 8 && b.frames() >= Budget::FLOOR_FRAMES, "{}", b.frames());
    }

    #[test]
    fn the_keep_up_floor_follows_the_present_and_is_capped() {
        // The first tick is no advance, and with no frame measured the cap is S2's floor.
        let mut fresh = Budget::new(RATE);
        assert_eq!(fresh.at_present(Frame(10)), None);
        assert_eq!(fresh.at_present(Frame(40)), None, "no cost yet: capped at the floor");
        assert_eq!(fresh.frames(), Budget::FLOOR_FRAMES);
        // 1 ms a frame: the share fits 8, the cap is 100 ms of stepping.
        let mut b = Budget::new(RATE);
        b.observe(10, Duration::from_millis(10));
        assert_eq!((b.frames(), b.keep_up_cap()), (8, 100));
        assert_eq!(b.at_present(Frame(40)), None);
        // One frame since the last tick asks for 2: the share's 8 stands.
        assert_eq!(b.at_present(Frame(41)), None);
        assert_eq!(b.frames(), 8);
        // 20 frames since the last tick ask for 21.
        let raised = b.at_present(Frame(61)).expect("raised");
        assert_eq!((b.frames(), RATE.frames(raised)), (21, 21));
        // A 10 s stall asks for 601, capped at 100 ms of stepping.
        b.at_present(Frame(661));
        assert_eq!(b.frames(), 100);
        // A present that moved backwards is no advance, and the greatest present seen stays the reference.
        b.at_present(Frame(650));
        assert_eq!(b.frames(), 8);
        b.at_present(Frame(662));
        assert_eq!(b.frames(), 8, "one frame past 661, not twelve past 650");
        // Cheap frames: the cap is MAX.
        let mut cheap = Budget::with_share(RATE, 0.0);
        cheap.observe(100, Duration::from_micros(10));
        cheap.at_present(Frame(0));
        cheap.at_present(Frame(600));
        assert_eq!(cheap.frames(), RATE.frames(Budget::MAX));
    }

    /// The simulation thread's loop as a model in simulated wall time: each tick costs `fixed` (taking messages in,
    /// signing, agreement), then `cost` for each frame stepped, and a replica that caught up waits for its next frame,
    /// as `run` does. It starts `behind` frames behind, and real time jumps by `stall` before tick `stall_at`. With
    /// `late`, another member's entries arrive that many frames behind the present on every tick, so each tick first
    /// rolls the replica back there. Returns each tick's frames stepped and whether the replica caught up (a tick that
    /// didn't is a hold, `count_hold`). `keep_up` false never calls `at_present` or `called`: the budget sized from
    /// stepping alone, as before the keep-up floor.
    fn model(
        cost: Duration,
        fixed: Duration,
        behind: u64,
        stall: (usize, Duration),
        ticks: usize,
        keep_up: bool,
        late: u64,
    ) -> Vec<(u64, bool)> {
        let period = 1e9 / f64::from(RATE.hz());
        let mut b = Budget::new(RATE);
        let mut t = behind as f64 * period;
        let mut next = 0u64;
        let mut out = Vec::new();
        for i in 0..ticks {
            if i == stall.0 {
                t += stall.1.as_nanos() as f64;
            }
            let present = (t / period) as u64;
            if keep_up {
                b.at_present(Frame(present));
            }
            if late > 0 {
                next = next.min(present.saturating_sub(late));
            }
            let steps = (present + 1).saturating_sub(next).min(b.frames());
            next += steps;
            if steps > 0 {
                b.observe(steps, cost * steps as u32);
            }
            if keep_up {
                b.called(steps, (present + 1).saturating_sub(next));
            }
            t += fixed.as_nanos() as f64 + cost.as_nanos() as f64 * steps as f64;
            let caught_up = next > present;
            if caught_up {
                t = t.max((present + 1) as f64 * period);
            }
            out.push((steps, caught_up));
        }
        out
    }

    /// @coordinator-opus, test 1: a tick that costs 100 ms besides stepping (a debug build's crypto on
    /// a loaded machine) still catches up when a frame costs less than a period, 3 ms here. Sized from stepping
    /// alone, the budget (2 frames) falls further behind every tick.
    #[test]
    fn a_fixed_100_ms_a_tick_still_catches_up() {
        let (cost, fixed, never) = (Duration::from_millis(3), Duration::from_millis(100), (usize::MAX, Duration::ZERO));
        let with = model(cost, fixed, 120, never, 400, true, 0);
        let first = with.iter().position(|&(_, up)| up).expect("the keep-up floor catches up");
        assert!(with[first..].iter().all(|&(_, up)| up), "and stays caught up");
        let without = model(cost, fixed, 120, never, 400, false, 0);
        assert!(without.iter().all(|&(_, up)| !up), "the budget from stepping alone never catches up");
    }

    /// Test 2: a 10 s stall never gives a tick more than `KEEP_UP_CAP` of stepping, or more than `MAX`, and the
    /// replica still catches up after it. After the stall's first, capped tick the floor is what real time adds plus
    /// one, so with dear frames (3 ms, where the share fits only S2's floor) it gains about a frame a tick, and the
    /// 600 frames take several hundred short ticks.
    #[test]
    fn a_ten_second_stall_never_gives_a_tick_longer_than_the_cap() {
        for cost in [Duration::from_micros(300), Duration::from_millis(3)] {
            let ticks = model(cost, Duration::from_millis(2), 0, (50, Duration::from_secs(10)), 1500, true, 0);
            for (i, &(steps, _)) in ticks.iter().enumerate() {
                assert!(cost * steps as u32 <= Budget::KEEP_UP_CAP, "tick {i} at {cost:?}: {steps} frames");
                assert!(steps <= RATE.frames(Budget::MAX), "tick {i} at {cost:?}: {steps} frames");
            }
            assert!(!ticks[50].1, "the stall leaves it behind at {cost:?}");
            assert!(ticks[50..].iter().any(|&(_, up)| up), "and it catches up at {cost:?}");
        }
    }

    /// Test 3: frames dearer than a period (30 ms at 60 Hz) keep every tick's stepping within the cap, and the
    /// replica holds: it falls behind for good, and each tick that leaves it behind is a hold (`count_hold`).
    #[test]
    fn frames_dearer_than_a_period_keep_ticks_bounded_and_hold() {
        let cost = Duration::from_millis(30);
        let ticks = model(cost, Duration::from_millis(1), 0, (usize::MAX, Duration::ZERO), 200, true, 0);
        let cap = (Budget::KEEP_UP_CAP.as_nanos() / cost.as_nanos()) as u64;
        assert!(ticks.iter().all(|&(steps, _)| steps <= cap.max(Budget::FLOOR_FRAMES)), "{ticks:?}");
        assert!(ticks[10..].iter().all(|&(_, up)| !up), "it holds: {:?}", &ticks[..20]);
    }

    /// @coordinator-opus, test 1: another member's entries arriving 300 ms (18 frames) late on every
    /// frame roll the replica back 18 frames on every tick. With frames at 1 ms the share fits 8, so before the floor
    /// covered the last call's need the replica never reached the present again and its view held for good. Now it
    /// catches up on every tick after the first few, each within the cap.
    #[test]
    fn entries_late_on_every_frame_no_longer_hold_the_view() {
        let (cost, fixed, never) = (Duration::from_millis(1), Duration::from_millis(1), (usize::MAX, Duration::ZERO));
        let ticks = model(cost, fixed, 0, never, 600, true, 18);
        let cap = (Budget::KEEP_UP_CAP.as_nanos() / cost.as_nanos()) as u64;
        assert!(ticks.iter().all(|&(steps, _)| steps <= cap), "within the cap");
        // Past the first 40 ticks, while the present is under 18 and the late entries roll back to frame 0.
        assert!(ticks[40..].iter().all(|&(_, up)| up), "caught up on every tick: {:?}", &ticks[30..60]);
        assert!(ticks[40..].iter().all(|&(steps, _)| steps >= 19), "each re-stepping the 18 frames");
    }

    /// Test 2: with entries late on every frame as well, a 10 s stall still never gives a tick more than
    /// `KEEP_UP_CAP` of stepping, and the replica catches up after it.
    #[test]
    fn a_stall_with_late_entries_never_gives_a_tick_longer_than_the_cap() {
        for cost in [Duration::from_micros(300), Duration::from_millis(1)] {
            let ticks = model(cost, Duration::from_millis(2), 0, (50, Duration::from_secs(10)), 3000, true, 18);
            for (i, &(steps, _)) in ticks.iter().enumerate() {
                assert!(cost * steps as u32 <= Budget::KEEP_UP_CAP, "tick {i} at {cost:?}: {steps} frames");
            }
            assert!(!ticks[50].1, "the stall leaves it behind at {cost:?}");
            assert!(ticks[2900..].iter().all(|&(_, up)| up), "and it catches up for good at {cost:?}");
        }
    }

    /// Test 4: the keep-up floor changes how fast a replica catches up, never what it computes. A pe-world replica
    /// that comes back from a quarter-second stall in larger steps publishes the same objects at the same frame as one
    /// ticked every frame. Kept short: a ball-pit frame costs about 20 ms in a debug build, so the cap allows a few
    /// frames a tick there, and the replica then gains about one frame a tick.
    #[test]
    fn the_keep_up_floor_gives_the_same_state_as_stepping_every_frame() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (mut steady, steady_slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        let (mut stalled, stalled_slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        let end = 70u64;
        for f in 1..=end {
            steady.tick(clock.host_time_of(Frame(f)));
        }
        for f in 1..=10 {
            stalled.tick(clock.host_time_of(Frame(f)));
        }
        let mut most = 0;
        for f in 25..=end {
            let (_, report) = stalled.tick(clock.host_time_of(Frame(f))).expect("ticked");
            most = most.max(report.advance.stepped + report.advance.resimulated);
        }
        assert!(most > Budget::FLOOR_FRAMES, "the keep-up floor stepped {most} frames in its largest tick");
        assert!(most <= RATE.frames(Budget::MAX), "{most}");
        let (a, b) = (steady_slot.newest().expect("a view"), stalled_slot.newest().expect("a view"));
        assert_eq!(a.frame, Frame(end + 1));
        assert_eq!(b.frame, a.frame, "caught up by frame {end}");
        assert_eq!(b.objects, a.objects);
        // And the same root at a frame of the 6-frame grid both keep (@design-reviewer's nit on da7f4947).
        let (r_steady, r_stalled) = (steady.root_at(Frame(66)), stalled.root_at(Frame(66)));
        assert!(r_steady.is_ok(), "{r_steady:?}");
        assert_eq!(r_stalled, r_steady);
    }

    /// The tick trace (crate::trace) records the simulation's own ticks: a `sec` line for each second of the frame
    /// clock and, with a threshold of 0, a `long` line for every tick, each with the replica's frame after it.
    #[test]
    fn a_tick_trace_records_the_simulation_s_ticks() {
        let exe = std::env::current_exe().expect("the test executable");
        let path = exe.parent().expect("its folder").join(format!("pe-sim-tick-trace-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (sim, _slot) = in_the_ball_pit(clock.clone(), Budget::new(RATE));
        let trace = crate::trace::TickTrace::create(&path, 0, RATE.hz()).expect("a trace");
        let mut sim = sim.with_tick_trace(trace);
        for f in 0..=70 {
            sim.tick(clock.host_time_of(Frame(f)));
        }
        drop(sim);
        let text = std::fs::read_to_string(&path).expect("the trace");
        let _ = std::fs::remove_file(&path);
        assert_eq!(text.lines().filter(|l| l.starts_with("sec ")).count(), 2, "frames 0-59 and 60-70: {text}");
        assert_eq!(text.lines().filter(|l| l.starts_with("long ")).count(), 71, "{text}");
        let last = text.lines().rfind(|l| l.starts_with("long ")).expect("a long line");
        assert!(last.contains(" p=70 ") && last.contains(" frame=71 "), "{last}");
    }

    #[test]
    fn no_tick_before_frame_0() {
        let epoch = HostTime(5_000_000_000);
        let (mut s, slot) = sim(Arc::new(LocalClock::new(epoch, RATE)), Budget::new(RATE));
        assert!(s.tick(HostTime(epoch.0 - 1)).is_none());
        assert_eq!(s.stats().before_frame_0, 1);
        assert!(slot.newest().is_none(), "nothing published before frame 0");
        // The next tick is due at frame 0, but never sooner than one frame period away.
        let period = 1_000_000_000 / i64::from(RATE.hz());
        assert_eq!(s.next_due(None, HostTime(epoch.0 - 2_000_000_000)), epoch);
        assert_eq!(s.next_due(None, HostTime(epoch.0 - 1)), HostTime(epoch.0 - 1 + period));
    }

    /// A clock still converging: no frame yet, and a frame 0 that would already have started.
    struct Converging;

    impl FrameClock for Converging {
        fn rate_hz(&self) -> u32 {
            RATE.hz()
        }
        fn frame_at(&self, _: HostTime) -> Option<Frame> {
            None
        }
        fn host_time_of(&self, _: Frame) -> HostTime {
            HostTime(0)
        }
    }

    /// design-reviewer's Low on slice 1: while the clock converges, the thread waits a frame period between ticks,
    /// not a millisecond.
    #[test]
    fn a_converging_clock_ticks_once_a_period() {
        let (mut s, slot) = sim(Arc::new(Converging), Budget::new(RATE));
        let now = HostTime(9_000_000_000);
        assert!(s.tick(now).is_none());
        assert_eq!(s.next_due(None, now), HostTime(now.0 + 1_000_000_000 / i64::from(RATE.hz())));
        assert!(slot.newest().is_none());
    }

    #[test]
    fn a_tick_simulates_its_frame_and_publishes_the_view() {
        ticks_and_publishes(sim);
    }

    #[test]
    fn a_tick_simulates_its_frame_and_publishes_the_view_on_the_real_replica() {
        ticks_and_publishes(real);
    }

    #[test]
    fn a_tick_simulates_its_frame_and_publishes_the_view_on_pe_world() {
        ticks_and_publishes(on_pe_world);
    }

    fn ticks_and_publishes(make: MakeSim) {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (mut s, slot) = make(clock.clone(), Budget::new(RATE));
        let (p, report) = s.tick(clock.host_time_of(Frame(1))).expect("ticked");
        assert_eq!(p, Frame(1));
        assert!(report.advance.caught_up);
        assert_eq!(report.advance.stepped, 2, "frames 0 and 1");
        let v = slot.newest().expect("published");
        assert_eq!(v.frame, Frame(2), "the state after frame 1");
        assert_eq!(v.sim_time, clock.host_time_of(Frame(2)));
        // A second tick in the same frame has nothing to step and publishes nothing new.
        s.tick(clock.host_time_of(Frame(1)).plus_nanos(1));
        assert_eq!(s.stats().published, 1);
        assert_eq!(s.next_due(Some(p), clock.host_time_of(Frame(1))), clock.host_time_of(Frame(2)));
    }

    /// The phase timers ([`crate::phases`]): the steps' shares add up to no more than the ticks, the replica's calls
    /// to no more than the driver's, and `advance`'s frames are counted.
    #[test]
    fn the_phase_timers_split_the_ticks() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (mut s, _slot) = on_pe_world(clock.clone(), Budget::new(RATE));
        for f in 1..=30 {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        let t = &s.stats().phases;
        assert_eq!((t.stepped, t.resimulated, t.rollbacks), (31, 0, 0), "frames 0 to 30, alone: {t:?}");
        assert!(t.advance_ns > 0 && t.longest_tick_ns > 0, "{t:?}");
        let steps = t.ingest_ns + t.inputs_ns + t.agree_ns + t.drive_ns + t.events_ns + t.diverge_ns + t.publish_ns;
        assert!(steps <= t.tick_ns && t.longest_tick_ns <= t.tick_ns, "{t:?}");
        assert!(t.advance_ns + t.roots_ns + t.finalize_ns <= t.drive_ns, "{t:?}");
    }

    /// app.md 7, no stall: a backlog of `W_fg` (2 s) at the floor budget. Every tick still ticks agreement and the
    /// driver, the view moves on to the newest frame the replica completed at every tick while it catches up (S2
    /// v1.10, F3's S5), and it catches up within `W_fg` plus two ticks.
    #[test]
    fn a_backlog_of_w_fg_at_the_floor_catches_up_within_w_fg() {
        catches_up(sim);
    }

    #[test]
    fn a_backlog_of_w_fg_at_the_floor_catches_up_within_w_fg_on_the_real_replica() {
        catches_up(real);
    }

    #[test]
    fn a_backlog_of_w_fg_at_the_floor_catches_up_within_w_fg_on_pe_world() {
        catches_up(on_pe_world);
    }

    fn catches_up(make: MakeSim) {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (mut s, slot) = make(clock.clone(), Budget::with_share(RATE, 0.0));
        let w_fg = RATE.frames(Duration::from_secs(2));
        let (mut ticks, mut completed) = (0u64, 0u64);
        loop {
            let (_, report) = s.tick(clock.host_time_of(Frame(w_fg + ticks))).expect("ticked");
            ticks += 1;
            completed += report.advance.stepped;
            if report.advance.caught_up {
                break;
            }
            // Behind the present, the view is the newest frame the replica completed (S2 v1.10), never frame 0.
            assert_eq!(slot.newest().map(|v| v.frame), Some(Frame(completed)), "the view moves on at tick {ticks}");
            assert!(ticks <= w_fg + 2, "still behind after {ticks} ticks");
        }
        assert!(ticks <= w_fg + 2, "{ticks} ticks");
        assert_eq!(s.stats().holds, 1);
        assert_eq!(s.stats().longest_hold, ticks - 1);
        assert_eq!(s.stats().ticks, ticks);
        let v = slot.newest().expect("published");
        assert_eq!(v.frame, Frame(w_fg + ticks), "the view resumes at the present");
    }

    /// app.md 7, no waiting: nothing draws, so nothing reads the frame slot, and the simulation keeps ticking.
    #[test]
    fn the_simulation_keeps_ticking_with_nobody_drawing() {
        let host = clock::shared();
        let rate = SimRate::new(120).unwrap();
        let frames: Arc<dyn FrameClock> = Arc::new(LocalClock::new(host(), rate));
        let (s, slot) = sim(frames, Budget::new(rate));
        let t = spawn(s, host, 1 << 20).expect("spawned");
        t.waker().wake();
        thread::sleep(Duration::from_millis(250));
        let stats = t.stop().expect("joined");
        // Loose bounds: the tests may run at low priority on a busy machine.
        assert!(stats.ticks >= 3 && stats.published >= 3, "{stats:?}");
        assert!(slot.newest().is_some_and(|v| v.frame.0 >= 3));
    }

    // ---- the graceful quit (app.md 4; @item:primordial-engine/pe-app-m1-leave-a-graceful-quit-submits-leave-ke) ----

    /// A pair of members (A first, then B) of a small ball pit over the in-process network, each with its own pe-agree
    /// node, as in `two_members_see_each_others_hands_and_finalize_over_a_network`, with each port's event queue.
    #[allow(clippy::type_complexity)]
    fn agreeing_pair(
        clock: &Arc<dyn FrameClock>,
        hub: &Hub,
    ) -> Vec<(Sim, Arc<FrameSlot>, MemberKey, Arc<std::sync::Mutex<Vec<NetEvent>>>)> {
        use pe_agree::{Node, NodeConfig};
        use pe_seams::Pos;
        use pe_seams::agree::{Cut, CutRef};
        use pe_seams::test_util::TestKeys;
        let keys = TestKeys::new(11, 2);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let at = [[-0.5, 1.0, -0.4], [0.5, 1.1, -0.3]];
        let mut pair = Vec::new();
        for (i, feed) in at.iter().enumerate() {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), Pos::ORIGIN, 8);
            let replica =
                crate::world::ball_pit(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
            let slot = Arc::new(FrameSlot::new());
            let port = Port::new(me, hub.clone());
            let events = port.events.clone();
            let sim = Sim::new(Box::new(node), replica, clock.clone(), slot.clone(), Budget::new(RATE))
                .with_local(me, keys.nonce(i, 0), Arc::new(StillHands(*feed)))
                .with_transport(Box::new(port), None);
            pair.push((sim, slot, me, events));
        }
        pair
    }

    /// Ticks the members `who` of `pair` at frame `f`'s host time, with the network's time set to it.
    #[allow(clippy::type_complexity)]
    fn tick_at(
        pair: &mut [(Sim, Arc<FrameSlot>, MemberKey, Arc<std::sync::Mutex<Vec<NetEvent>>>)],
        clock: &Arc<dyn FrameClock>,
        hub: &Hub,
        who: &[usize],
        f: u64,
    ) {
        let now = clock.host_time_of(Frame(f));
        hub.now.store(now.0, Ordering::SeqCst);
        for &i in who {
            pair[i].0.tick(now).expect("ticked");
        }
    }

    /// M1's demo step 8 (app.md 4): after 3 s together, B leaves. Its `Leave` is its last entry, it keeps ticking, and
    /// its wait ends once A has confirmed E (S3's `leave_settled`). Then B stops, and A goes on alone: its epoch is A
    /// alone, it finalizes on its own, B's hands are gone from its view, and when B's link drops A's console says B
    /// left the session.
    #[test]
    fn a_member_leaves_and_the_other_finalizes_alone() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let hub = Hub::default();
        let mut pair = agreeing_pair(&clock, &hub);
        let mut f = 0;
        for _ in 0..180 {
            f += 1;
            tick_at(&mut pair, &clock, &hub, &[0, 1], f);
        }
        assert_eq!(pair[1].0.leave_status(), LeaveStatus::Pending);
        pair[1].0.leave();
        let (left_at, inputs) = (f, pair[1].0.stats().inputs);
        let settled = (0..600).find(|_| {
            f += 1;
            tick_at(&mut pair, &clock, &hub, &[0, 1], f);
            pair[1].0.leave_status() == LeaveStatus::Settled
        });
        assert!(settled.is_some(), "B's leave settles within 10 s");
        println!("B's leave settled after {} frames", f - left_at);
        assert_eq!(pair[1].0.stats().inputs, inputs, "no hand entry after the Leave");
        assert_eq!(pair[1].0.stats().leave_refused, 0);
        for _ in 0..300 {
            f += 1;
            tick_at(&mut pair, &clock, &hub, &[0], f);
        }
        let (a, b) = (&pair[0], pair[1].2);
        assert_eq!(a.0.other_members(), Vec::<MemberKey>::new(), "A's epoch is A alone");
        let v = a.1.newest().expect("published");
        assert!(v.objects.iter().all(|o| o.hand_of.is_none_or(|(m, _)| m != b)), "B's hands are gone");
        assert!(v.final_frame.is_some_and(|ff| ff.0 + 60 >= f), "A finalizes alone: final {:?} at {f}", v.final_frame);
        pair[0].3.lock().expect("the events").push(NetEvent::LinkDown { peer: b });
        f += 1;
        tick_at(&mut pair, &clock, &hub, &[0], f);
        assert_eq!(pair[0].0.take_notices(), vec![crate::leave::left(b)]);
    }

    /// The remaining member is told at once why the other went (app.md 4, @coordinator-opus). B signs its
    /// `Leave` and vanishes before A can confirm E: when B's link drops, A, holding the `Leave` with B still in its
    /// epoch, says B left before the session could confirm it. Without a `Leave` in hand, a dropped link says the other
    /// may come back.
    #[test]
    fn the_remaining_member_hears_at_once_why_the_other_went() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        for leaves in [true, false] {
            let hub = Hub::default();
            let mut pair = agreeing_pair(&clock, &hub);
            let mut f = 0;
            for _ in 0..120 {
                f += 1;
                tick_at(&mut pair, &clock, &hub, &[0, 1], f);
            }
            if leaves {
                pair[1].0.leave();
            }
            f += 1;
            tick_at(&mut pair, &clock, &hub, &[0, 1], f);
            for _ in 0..30 {
                f += 1;
                tick_at(&mut pair, &clock, &hub, &[0], f);
            }
            let b = pair[1].2;
            assert_eq!(pair[0].0.other_members(), vec![b], "E isn't final at A: B is still in its epoch");
            pair[0].3.lock().expect("the events").push(NetEvent::LinkDown { peer: b });
            f += 1;
            tick_at(&mut pair, &clock, &hub, &[0], f);
            let want = if leaves { crate::leave::stranded(b) } else { crate::leave::dropped(b) };
            assert_eq!(pair[0].0.take_notices(), vec![want], "with a Leave in hand: {leaves}");
        }
    }

    /// F3 S1 (@engine-architect's M1 on fc24bc9c): at n = 2, B stalls for 20 s, past the receivers' pending cap for A's
    /// entries (about 15 s of A's hands at the floor), then runs again. A's hands go in at the floor throughout, never
    /// held, and once B is back, finality resumes on equal roots within a few seconds: the stand-still doesn't feed
    /// itself. B, behind for the freeze delay (3 s here), says so, and at n = 2 it doesn't freeze (F3 S5, ruling "2"):
    /// the notice comes once, the status says the session confirms late, and it comes down once B has caught up.
    #[test]
    fn a_member_stalled_past_the_pending_cap_comes_back_and_finality_resumes() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let hub = Hub::default();
        let mut pair = agreeing_pair(&clock, &hub);
        let status = Arc::new(pe_render::notices::StatusLine::default());
        for i in 0..2 {
            let (s, slot, me, events) = pair.remove(i);
            let mut s = s.with_diverge(pe_diverge::Config::default(), None).expect("pe-diverge").with_freeze_after(3.0);
            if i == 1 {
                s = s.with_status(status.clone());
            }
            pair.insert(i, (s, slot, me, events));
        }
        let hz = u64::from(RATE.hz());
        let mut f = 0;
        for _ in 0..2 * hz {
            f += 1;
            tick_at(&mut pair, &clock, &hub, &[0, 1], f);
        }
        let (inputs, refused) = (pair[0].0.stats().inputs, pair[0].0.stats().inputs_refused);
        let stall = 20 * hz;
        for _ in 0..stall {
            f += 1;
            tick_at(&mut pair, &clock, &hub, &[0], f);
        }
        let a = pair[0].0.stats();
        let (inputs, refused) = (a.inputs - inputs, a.inputs_refused - refused);
        let gap = (hz / HAND_FLOOR_HZ).max(1);
        println!(
            "stall of {stall} frames: A sent {inputs} hand inputs ({refused} refused), {} paced, {} thinned; A \
             backlogged: {}",
            a.hands_paced,
            a.hands_floor_thinned,
            pair[0].0.agree.backlogged()
        );
        assert_eq!(a.hands_paced, 0, "A's hands are never held");
        assert!(inputs + refused >= 2 * (stall / gap) - 4, "A's hands went in at the floor: {inputs} + {refused}");
        let (mut resumed, mut shown_late, mut longest_tick) = (None, 0u64, 0u64);
        let back = f;
        for _ in 0..30 * hz {
            f += 1;
            tick_at(&mut pair, &clock, &hub, &[0, 1], f);
            let fin = pair[0].1.newest().and_then(|v| v.final_frame).map_or(0, |x| x.0);
            if resumed.is_none() && fin + hz >= f {
                resumed = Some(f - back);
            }
            if status.get().is_some_and(|t| t.contains("confirming")) {
                shown_late += 1;
            }
            longest_tick = longest_tick.max(pair[0].0.stats().phases.longest_tick_ns);
        }
        let notices = pair[1].0.take_notices();
        println!(
            "finality back within 1 s of the present {resumed:?} frames after B came back; A's longest tick {} ms; B \
             showed 'confirming late' on {shown_late} frames; notices {notices:?}",
            longest_tick / 1_000_000
        );
        assert!(resumed.is_some_and(|r| r <= 10 * hz), "finality resumes within 10 s of B's return: {resumed:?}");
        for (i, (s, _, _, _)) in pair.iter().enumerate() {
            let d = s.diverge_counters().expect("pe-diverge");
            assert_eq!((d.root_splits, d.diverged), (0, 0), "member {i}: {d:?}");
        }
        assert!(
            notices.iter().filter(|n| n.starts_with("slow:")).count() <= 1,
            "one notice per slow stint: {notices:?}"
        );
        assert!(!status.get().is_some_and(|t| t.contains("confirming")), "the status came down once B caught up");
    }

    /// M1 redo F2 (@item:primordial-engine/m1-redo-f2-an-unconfirmed-leave-kept-a-s-hands-h): B signs its `Leave`
    /// and its link drops before A confirms E. B stays in A's epoch, so A finalizes nothing more, and pe-agree's backlog
    /// pacing counts B as a member that never acknowledges A's entries. A was told it can keep playing, so its hands
    /// keep going in. Since F3's S1 no member's hands are ever held, whether B left unconfirmed or only dropped (it may
    /// come back): while backlogged they are thinned to [`HAND_FLOOR_HZ`], never below.
    #[test]
    fn a_member_left_alone_by_an_unconfirmed_leave_keeps_its_hands() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        for leaves in [true, false] {
            let hub = Hub::default();
            let mut pair = agreeing_pair(&clock, &hub);
            let mut f = 0;
            for _ in 0..120 {
                f += 1;
                tick_at(&mut pair, &clock, &hub, &[0, 1], f);
            }
            if leaves {
                pair[1].0.leave();
            }
            f += 1;
            tick_at(&mut pair, &clock, &hub, &[0, 1], f);
            let b = pair[1].2;
            pair[0].3.lock().expect("the events").push(NetEvent::LinkDown { peer: b });
            let (inputs, paced) = (pair[0].0.stats().inputs, pair[0].0.stats().hands_paced);
            for _ in 0..600 {
                f += 1;
                tick_at(&mut pair, &clock, &hub, &[0], f);
            }
            assert_eq!(pair[0].0.other_members(), vec![b], "E isn't final at A: B is still in its epoch");
            let (inputs, paced) = (pair[0].0.stats().inputs - inputs, pair[0].0.stats().hands_paced - paced);
            let (refused, thinned) = (pair[0].0.stats().inputs_refused, pair[0].0.stats().hands_floor_thinned);
            println!(
                "with a Leave in hand {leaves}: {inputs} hand inputs ({refused} refused), {paced} frames paced, {thinned} thinned in 600"
            );
            let gap = (u64::from(RATE.hz()) / HAND_FLOOR_HZ).max(1);
            assert_eq!(paced, 0, "A's hands are never held (F3 S1)");
            assert!(inputs >= 2 * (590 / gap), "A's hands keep going in, at the floor or better: {inputs}");
            assert_eq!(refused, 0, "and agreement takes them");
        }
    }

    /// The quit on the simulation thread (app.md 4). A world alone has nobody to wait for: its `Leave` settles at once.
    /// A member whose peer never answers waits for its bound (set short here), then says the peer hadn't confirmed it.
    #[test]
    fn the_quit_waits_for_the_confirmation_or_its_bound() {
        let host = clock::shared();
        let frames: Arc<dyn FrameClock> = Arc::new(LocalClock::new(host(), RATE));
        let (s, _slot) = in_the_ball_pit(frames.clone(), Budget::new(RATE));
        let s = s.with_local(crate::world::DEV_MEMBER, [7; 32], Arc::new(StillHands([0.0, 1.0, -0.4])));
        let t = spawn(s, host.clone(), 16 << 20).expect("spawned");
        thread::sleep(Duration::from_millis(100));
        let mut lines = Vec::new();
        let (outcome, stats) = t.leave_and_stop(&mut |l| lines.push(l.to_string()));
        assert!(matches!(outcome, LeaveOutcome::Settled { .. }), "{outcome:?}");
        assert_eq!(lines, ["leaving the session…", "left the session."]);
        assert_eq!(stats.expect("joined").leave_refused, 0);

        let hub = Hub::default();
        let mut pair = agreeing_pair(&frames, &hub);
        let (b, a) = (pair.pop().expect("B"), pair[0].2);
        let t = spawn(b.0.with_leave_wait(Some(Duration::from_millis(300))), host, 16 << 20).expect("spawned");
        thread::sleep(Duration::from_millis(100));
        let mut lines = Vec::new();
        let started = Instant::now();
        let (outcome, stats) = t.leave_and_stop(&mut |l| lines.push(l.to_string()));
        assert_eq!(outcome, LeaveOutcome::Unconfirmed { others: vec![a] });
        assert!(started.elapsed() >= Duration::from_millis(300), "{:?}", started.elapsed());
        assert_eq!(lines, ["leaving the session…".to_string(), crate::leave::left_unconfirmed(&[a])]);
        stats.expect("joined");
    }

    /// A `Leave` that agreement refuses every time never goes out, so the quit doesn't claim it left unconfirmed: it
    /// says the leave couldn't be sent and the others will see a dropped connection (@design-reviewer's Low 1).
    #[test]
    fn a_leave_that_never_goes_out_says_so() {
        let host = clock::shared();
        let frames: Arc<dyn FrameClock> = Arc::new(LocalClock::new(host(), RATE));
        let me = crate::world::DEV_MEMBER;
        let replica = crate::world::open_ball_pit(7, me, RATE, frames.clone(), runner()).expect("pit");
        let agree = SoloAgreement::new(LineId::origin());
        agree.refuse_protocol.store(true, Ordering::Relaxed);
        let slot = Arc::new(FrameSlot::new());
        let s = Sim::new(Box::new(agree), replica, frames, slot, Budget::new(RATE))
            .with_local(me, [7; 32], Arc::new(StillHands([0.0, 1.0, -0.4])))
            .with_leave_wait(Some(Duration::from_millis(300)));
        let t = spawn(s, host, 16 << 20).expect("spawned");
        thread::sleep(Duration::from_millis(100));
        let mut lines = Vec::new();
        let (outcome, stats) = t.leave_and_stop(&mut |l| lines.push(l.to_string()));
        assert_eq!(outcome, LeaveOutcome::NotSent { others: Vec::new() });
        assert_eq!(lines, ["leaving the session…".to_string(), crate::leave::leave_not_sent(&[])]);
        assert!(stats.expect("joined").leave_refused > 0, "each tick tried again");
    }

    /// One frame of A's view in a drain run: its frame, B's faster hand's speed and the fastest ball's speed in it.
    #[derive(Clone, Copy, Debug)]
    struct DrainFrame {
        frame: u64,
        hand: f32,
        ball: f32,
    }

    /// A drain run: each frame A's view advanced by, B's lead after each present frame's tick, and B's stats.
    struct DrainRun {
        view: Vec<DrainFrame>,
        /// From present frame 1.
        leads: Vec<u64>,
        stats: SimStats,
    }

    impl DrainRun {
        /// The stamp of B's entries at the present `f` (from 1): a stamp that stands still is the same either way.
        fn stamp(&self, f: u64) -> u64 {
            f + self.leads[(f - 1) as usize]
        }

        /// The present frames at which B shed a frame of lead.
        fn sheds(&self) -> Vec<u64> {
            (2..=self.leads.len() as u64).filter(|&f| self.leads[f as usize - 1] < self.leads[f as usize - 2]).collect()
        }

        /// The shortest gap between two sheds, in frames.
        fn shortest_shed(&self) -> Option<u64> {
            self.sheds().windows(2).map(|w| w[1] - w[0]).min()
        }

        /// The first present frame with no lead.
        fn gone(&self) -> Option<u64> {
            self.leads.iter().position(|&l| l == 0).map(|i| i as u64 + 1)
        }

        /// The view frames at which A plays what B stamped at the presents `from..=to`: A plays stamp `s` at its view
        /// frame `s + 1`.
        fn played(&self, from: u64, to: u64) -> std::ops::RangeInclusive<u64> {
            self.stamp(from) + 1..=self.stamp(to) + 1
        }

        /// B's faster hand's speeds at A over the view frames `frames`.
        fn hand(&self, frames: &std::ops::RangeInclusive<u64>) -> Vec<f32> {
            self.view.iter().filter(|d| frames.contains(&d.frame)).map(|d| d.hand).collect()
        }

        /// The fastest ball A sees over the view frames `frames`.
        fn ball(&self, frames: &std::ops::RangeInclusive<u64>) -> f32 {
            self.view.iter().filter(|d| frames.contains(&d.frame)).map(|d| d.ball).fold(0.0, f32::max)
        }
    }

    /// Two pe-agree members on a fast link (25 ms each way) for `frames` frames: A still, B's hands fed by `hands`. B
    /// starts with `lead` frames of lead (`None`: stamp-ahead off), which the reports, finding no delay, soon drain.
    fn drain_run(lead: Option<u64>, hands: Arc<dyn pe_seams::HandFeed>, frames: u64) -> DrainRun {
        use crate::stamp::{LeadConfig, StampLead};
        use pe_agree::{Node, NodeConfig};
        use pe_seams::Pos;
        use pe_seams::agree::{Cut, CutRef};
        use pe_seams::test_util::TestKeys;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let keys = TestKeys::new(11, 2);
        let members = keys.sorted_keys();
        let line = pe_world::preset::m1_line();
        let hub = Hub::default();
        let (a, b) = (keys.key(0), keys.key(1));
        hub.delay.lock().expect("the delays").insert((a, b), 25_000_000);
        hub.delay.lock().expect("the delays").insert((b, a), 25_000_000);
        let mut sims = Vec::new();
        for i in 0..2 {
            let me = keys.key(i);
            let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
            let config = NodeConfig::new(line, members.clone(), genesis);
            let node = Node::new(config, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), Pos::ORIGIN, 8);
            let replica =
                crate::world::ball_pit(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
            let slot = Arc::new(FrameSlot::new());
            let feed: Arc<dyn pe_seams::HandFeed> =
                if i == 0 { Arc::new(StillHands([-0.5, 1.0, -0.4])) } else { hands.clone() };
            let mut sim = Sim::new(Box::new(node), replica, clock.clone(), slot.clone(), Budget::new(RATE))
                .with_local(me, keys.nonce(i, 0), feed)
                .with_transport(Box::new(Port::new(me, hub.clone())), None);
            if i == 1 {
                sim = sim.with_stamp_lead(lead.map(|n| StampLead::fixed(LeadConfig::m1(RATE), n)));
            }
            sims.push((sim, slot));
        }
        let speed = |v: [f32; 3]| v.iter().map(|c| c * c).sum::<f32>().sqrt();
        let (mut view, mut leads, mut last) = (Vec::new(), Vec::new(), None);
        for f in 1..=frames {
            let now = clock.host_time_of(Frame(f));
            hub.now.store(now.0, Ordering::SeqCst);
            sims[0].0.tick(now);
            sims[1].0.tick(now);
            leads.push(sims[1].0.stats().lead);
            let Some(v) = sims[0].1.newest() else { continue };
            if last == Some(v.frame) {
                continue;
            }
            last = Some(v.frame);
            let hands = v.objects.iter().filter(|o| o.hand_of.is_some_and(|(m, _)| m == b));
            let ball = v.objects.iter().filter(|o| o.hand_of.is_none()).map(|o| speed(o.linear_velocity));
            view.push(DrainFrame {
                frame: v.frame.0,
                hand: hands.map(|o| speed(o.linear_velocity)).fold(0.0, f32::max),
                ball: ball.fold(0.0, f32::max),
            });
        }
        DrainRun { view, leads, stats: sims[1].0.stats().clone() }
    }

    fn median(mut v: Vec<f32>) -> f32 {
        v.sort_by(f32::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    }

    /// A strike through a drain (@coordinator-opus, test 1). B's closed hands sweep at 0.63 m/s, above
    /// the speed test's threshold all run, while its 3 s lead drains. In M1 a player sends only hand inputs, so a strike
    /// is a closed hand pushing balls.
    ///
    /// - A sees B's hands at most 1.1 times their tracking speed while the lead drains, and never paused: the pose
    ///   clock spreads each shed frame over the next ten stamps (without it, one stamp in eleven carried two frames).
    /// - The sweep's first turn, through the pit's resting balls, strikes them at most 10% harder than the no-lead
    ///   maximum: the hardest the same turn strikes with no lead, over four sub-frame phases (a strike's speed depends
    ///   on where the contact falls between frames). The same turn with no lead and the hands 10% faster is printed
    ///   beside it.
    /// - Later turns meet balls the earlier ones scattered, which differ from run to run, so the fastest ball over the
    ///   whole drain is printed for each run, not compared.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "six 36 s runs of two members in the ball pit: release (--include-ignored)")]
    fn a_strike_through_a_drain_lands_at_most_about_10_percent_harder() {
        let sweep = || SweepHands::until(HostTime(i64::MAX));
        let tracking = sweep().speed() as f32;
        let frames = 36 * 60;
        let drained = drain_run(Some(180), Arc::new(sweep()), frames);
        let gone = drained.gone().expect("the lead drained");
        let draining = drained.played(drained.sheds().first().copied().expect("a shed"), gone - 1);
        let hand = drained.hand(&draining);
        let (fastest, slowest) = hand.iter().fold((0.0f32, f32::INFINITY), |(a, b), &h| (a.max(h), b.min(h)));
        // The first turn of the sweep, B's presents 1 to 180, as each run's A plays them.
        let first = |r: &DrainRun| r.ball(&r.played(1, 180));
        let phases: Vec<DrainRun> = (0..4)
            .map(|k| drain_run(None, Arc::new(SweepHands { offset: f64::from(k) / 240.0, ..sweep() }), frames))
            .collect();
        let faster = drain_run(None, Arc::new(SweepHands { period: 3.0 / 1.1, ..sweep() }), frames);
        let no_lead: Vec<f32> = phases.iter().map(first).collect();
        let most = no_lead.iter().copied().fold(0.0f32, f32::max);
        let strike = first(&drained);
        let whole: Vec<f32> = phases.iter().map(|r| r.ball(&draining)).collect();
        println!(
            "B's hands at A while draining (view frames {draining:?}): median {:.3}, fastest {fastest:.3}, slowest \
             {slowest:.3} m/s against tracking {tracking:.3}. The first turn's fastest ball: {strike:.3} m/s, against \
             {no_lead:.3?} with no lead at phases 0, 1/4, 1/2 and 3/4 of a frame ({:.3}x their most) and {:.3} with no \
             lead and the hands 10% faster. Over the whole drain: {:.3} m/s, against {whole:.3?} and {:.3}. B's stats \
             {:?}",
            median(hand.clone()),
            strike / most.max(1e-6),
            first(&faster),
            drained.ball(&draining),
            faster.ball(&draining),
            drained.stats
        );
        assert!(hand.len() > 600, "a drain of {} view frames", hand.len());
        assert!(fastest <= 1.1 * tracking * 1.01, "the hand at {:.3}x its tracking speed", fastest / tracking);
        assert!(slowest >= 0.95 * tracking, "a pause: {slowest:.3} m/s");
        assert!(most > 0.2, "the first turn strikes: {no_lead:?}");
        assert!(strike <= 1.1 * most, "the first turn's strike {:.3}x the no-lead maximum", strike / most);
    }

    /// A wrist flick with a nearly still origin (@coordinator-opus, test 2). B's closed hands circle at
    /// 0.08 m/s, below the speed test's thresholds, each turning about its forearm at 6 rad/s, so its farthest point
    /// moves at 0.08 + 6 x 0.09 = 0.62 m/s: the speed test finds them moving, and B's 1 s lead sheds one frame in 11 at
    /// most. Without the turn the same hands are slow and the lead sheds up to one frame in 2. In M1 a hand's collider
    /// is a sphere about its pose's origin, so the turn itself strikes nothing; what the shed may not do is play the
    /// origin faster than 1.1 times its tracking speed.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "two 8 s runs of two members in the ball pit: release (--include-ignored)")]
    fn a_wrist_flick_with_a_nearly_still_origin_sheds_one_frame_in_11_at_most() {
        let frames = 8 * 60;
        let flick = || SweepHands { period: 24.0, spin: 6.0, ..SweepHands::until(HostTime(i64::MAX)) };
        let tracking = flick().speed() as f32;
        let flicked = drain_run(Some(60), Arc::new(flick()), frames);
        let still = drain_run(Some(60), Arc::new(SweepHands { spin: 0.0, ..flick() }), frames);
        let draining = flicked.played(flicked.sheds().first().copied().expect("a shed"), frames);
        let fastest = flicked.hand(&draining).into_iter().fold(0.0f32, f32::max);
        println!(
            "with the flick: moving on {} frames, shortest gap between sheds {:?}, lead {} at the end, the origin at A \
             {fastest:.3} m/s against {tracking:.3}; without: moving on {} frames, shortest gap {:?}, lead gone at \
             frame {:?}",
            flicked.stats.moving_frames,
            flicked.shortest_shed(),
            flicked.leads.last().copied().unwrap_or(0),
            still.stats.moving_frames,
            still.shortest_shed(),
            still.gone()
        );
        assert!(flicked.stats.moving_frames + 10 >= frames, "moving on {} frames", flicked.stats.moving_frames);
        assert!(flicked.shortest_shed().is_some_and(|g| g >= 11), "{:?}", flicked.shortest_shed());
        assert!(fastest <= 1.1 * tracking * 1.01, "the origin at {:.3}x its tracking speed", fastest / tracking);
        assert!(still.stats.moving_frames <= 5, "without the turn: moving on {} frames", still.stats.moving_frames);
        assert_eq!(still.shortest_shed(), Some(2), "without the turn the lead sheds at the slow hands' rate");
        assert!(still.gone().is_some_and(|f| f < frames), "and is gone within the run");
    }

    /// A player who never pauses (@coordinator-opus, test 3): B's hands sweep at 0.63 m/s all run, so the
    /// speed test finds them moving on every frame, and B's 3 s lead sheds one frame in 11 at most. It is gone within
    /// 35 s. From the first report, its first 175 frames go at one in 11 and its last 5 at the small surplus's slower
    /// rate (12, 15, 20, 30 and 30 frames): 33.7 s. A's view stays at the present.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "a 36 s run of two members in the ball pit: release (--include-ignored)")]
    fn a_player_who_never_pauses_sheds_a_3_s_lead_within_35_s() {
        let frames = 36 * 60;
        let run = drain_run(Some(180), Arc::new(SweepHands::until(HostTime(i64::MAX))), frames);
        let behind = run.view.last().map_or(u64::MAX, |d| frames.saturating_sub(d.frame));
        println!(
            "moving on {} of {frames} frames, shortest gap between sheds {:?}, first shed at {:?}, lead gone at frame \
             {:?} ({:.1} s), longest drain {} frames, A's view {behind} frames behind at the end",
            run.stats.moving_frames,
            run.shortest_shed(),
            run.sheds().first(),
            run.gone(),
            run.gone().unwrap_or(0) as f64 / 60.0,
            run.stats.longest_drain
        );
        assert!(run.stats.moving_frames + 10 >= frames, "moving on {} frames", run.stats.moving_frames);
        assert!(run.shortest_shed().is_some_and(|g| g >= 11), "{:?}", run.shortest_shed());
        assert!(run.gone().is_some_and(|f| f <= 35 * 60), "gone at frame {:?}", run.gone());
        assert!(behind <= 2, "A's view {behind} frames behind");
    }
}
