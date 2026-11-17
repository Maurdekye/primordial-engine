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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pe_api::queue::Receipt;
use pe_render::present::FrameSlot;
use pe_replica::{DriveReport, Driver};
use pe_seams::world_api::TxId;
use pe_seams::{
    AgreeEvent, Agreement, ChainId, Class, Cut, CutRef, EntryRef, Frame, FrameClock, FrameView, Hand, HandFeed,
    HostTime, Lane, MemberKey, NetEvent, Replica, SelfTest, SimRate, Transport, VoidReason,
};

use crate::clock::HostClock;
use crate::leave::{LeaveStatus, Leaving, ViewTime};
use crate::local::{self, Chain};

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
    /// Times the replica fell behind the present (`Advance::caught_up` false). The view holds until it catches up.
    pub holds: u64,
    /// The longest hold, in ticks.
    pub longest_hold: u64,
    /// Messages from the network that agreement took (step 1).
    pub received: u64,
    /// Messages agreement refused as a peer's fault (step 1): counted, never fatal.
    pub refused: u64,
    /// Messages on lanes this slice does not route yet: Repair (pe-diverge), Bulk (the blob store) and Pose (the
    /// display).
    pub unrouted: u64,
    /// The transport's events (links, join requests), each passed to the net-event hook if one is set.
    pub net_events: u64,
    /// Messages agreement made that went to the transport (step 3).
    pub sent: u64,
    /// Messages agreement made that had nowhere to go: no transport is wired (a world alone).
    pub unsent: u64,
    /// S3's events, drained each tick. pe-diverge takes them once it lands.
    pub events: u64,
    /// The local player's hand inputs agreement took, and those it refused (step 2).
    pub inputs: u64,
    pub inputs_refused: u64,
    /// Frames whose hand inputs step 2 held back because agreement reported this member backlogged after a stall
    /// (S3 v1.5, [`Agreement::backlogged`]).
    pub hands_paced: u64,
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
    published: Option<Arc<FrameView>>,
    /// Ticks into the current hold.
    hold: Option<u64>,
    stats: SimStats,
    /// The local player: this member's chain, and the hands step 2 reads. `None` runs no local input.
    local: Option<Local>,
    /// The frame step 2 last submitted the local player's inputs for.
    input_frame: Option<Frame>,
    /// The stamp of the last frame's inputs: a frame whose stamp equals it thins its poses (app.md 3.2).
    input_stamp: Option<Frame>,
    /// Stamp-ahead (app.md 3.2, [`crate::stamp`]): the lead this member's own entries are stamped ahead by, fed each
    /// tick from the other members' lateness reports (S3 v1.8). `None` stamps them at the present.
    lead: Option<crate::stamp::StampLead>,
    /// Stamp-ahead's speed test (app.md 3.2): whether a hand moves, which slows the lead's shedding.
    motion: crate::stamp::Motion,
    /// The network (S4): pe-net's handle, or a test's in-process network. `None` is a world alone.
    net: Option<Box<dyn Transport + Send>>,
    net_hook: Option<Box<dyn NetHook>>,
    /// S3's events, kept for a test or a diagnostic (`with_event_log`); otherwise only counted.
    event_log: Option<Arc<std::sync::Mutex<Vec<AgreeEvent>>>>,
    /// pe-api's ports (app.md 3.1), with what this loop keeps for them.
    api: Option<ApiLoop>,
    /// The graceful quit (app.md 4, [`crate::leave`]).
    leaving: Leaving,
    /// The smoothed view time the quit's bound follows, from S3's proposals.
    view_time: ViewTime,
    /// The quit's bound, if set locally ([`Sim::with_leave_wait`]); otherwise it follows the view time.
    leave_wait: Option<Duration>,
    /// The members whose link is down (S4's `LinkDown`, until a `LinkUp`).
    down: std::collections::BTreeSet<MemberKey>,
    /// What the console should say (app.md 4), drained by [`Sim::take_notices`].
    notices: Vec<String>,
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
pub trait NetHook: Send {
    /// Each of the transport's events, after step 1 counts it.
    fn on_event(&mut self, event: &NetEvent, agree: &mut dyn Agreement);
    /// Once a tick, after step 1's messages and events: for what reaches the hook other than as an event, such as
    /// the lifecycle outcomes on pe-net's control end, which would otherwise wait for an unrelated event
    /// (@design-reviewer's Low on 5ca43067).
    fn on_tick(&mut self, now: HostTime, agree: &mut dyn Agreement) {
        let _ = (now, agree);
    }
}

/// The most messages step 1 takes from the network in one tick, so a flood cannot stall the tick: the rest wait
/// in the transport's bounded queue for the next one.
pub const MAX_IN_PER_TICK: usize = 1024;

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
    /// The `engaged` last sent for each hand, left then right (`local::submit_hands`).
    sent: [bool; 2],
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
            published: None,
            hold: None,
            stats: SimStats::default(),
            local: None,
            input_frame: None,
            input_stamp: None,
            lead,
            motion,
            net: None,
            net_hook: None,
            event_log: None,
            api: None,
            leaving: Leaving::Staying,
            view_time: ViewTime::default(),
            leave_wait: None,
            down: std::collections::BTreeSet::new(),
            notices: Vec::new(),
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

    /// What the console should say since the last call (app.md 4: the leave's messages).
    pub fn take_notices(&mut self) -> Vec<String> {
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
        self.local = Some(Local { chain: Chain::new(self.agree.line(), me, nonce), hands, sent: [false; 2] });
        self
    }

    pub fn stats(&self) -> &SimStats {
        &self.stats
    }

    pub fn budget(&self) -> &Budget {
        &self.budget
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
        // 1. Messages in. Protocol and Action go to agreement; a refusal is a peer's fault, counted and never fatal.
        // TODO(pe-diverge): Repair to pe-diverge; TODO(blob store): Bulk; Pose is display-only.
        let mut links = Vec::new();
        if let Some(net) = &mut self.net {
            for _ in 0..MAX_IN_PER_TICK {
                let Some(r) = net.poll() else { break };
                match r.lane {
                    Lane::Protocol | Lane::Action => match self.agree.receive(r.from, r.lane, &r.bytes, r.at) {
                        Ok(()) => self.stats.received += 1,
                        Err(_) => self.stats.refused += 1,
                    },
                    _ => self.stats.unrouted += 1,
                }
            }
            for e in net.events() {
                self.stats.net_events += 1;
                if let (Some(api), NetEvent::LinkUp { peer } | NetEvent::LinkDown { peer }) = (&self.api, &e) {
                    api.ports.shared.link(*peer, matches!(e, NetEvent::LinkUp { .. }));
                }
                if let Some(hook) = &mut self.net_hook {
                    hook.on_event(&e, &mut *self.agree);
                }
                match e {
                    NetEvent::LinkUp { peer } => links.push((peer, true)),
                    NetEvent::LinkDown { peer } => links.push((peer, false)),
                    _ => {}
                }
            }
            if let Some(hook) = &mut self.net_hook {
                hook.on_tick(now, &mut *self.agree);
            }
        }
        for (peer, up) in links {
            if up {
                self.down.remove(&peer);
            } else {
                self.link_down(peer, p);
            }
        }
        // 2. Local input: both hands at frame `p`'s host time, through the newest view's rig, one input entry each.
        // Before the first view the hands' objects are not known yet, so nothing goes in. Once per frame: a wake (step
        // 7) ticks again within a frame, and that tick takes in messages and re-simulates but adds no second set of
        // inputs for the frame, which would count against the rate budget.
        // While agreement reports this member backlogged after a stall (S3 v1.5), the frame's hand inputs are held
        // back: they would only be void as late, and they would lengthen the flush. Edits never come through here, so
        // nothing holds them back.
        // The frame's edit goes first, before its hand entries, so that a dry run made while pe-api's queue was empty
        // names the seq the edit gets (app.md 3.1). One a frame, never paced: an edit is never void as late for
        // being held back, and the queue's wake (step 7) brings its frame's tick.
        // Stamp-ahead (app.md 3.2): the lead follows the other members' reports, and the frame's entries go that far
        // ahead of the present, never below the chain's last stamp. The speed test reads both hands at `p`'s host time
        // once a frame: while either moves, the lead sheds at most one frame in 11. The pose clock then says which
        // moment the frame's poses carry, so a shed is spread over the stamps up to the next. A member that is leaving
        // sends no more hands, so it reads none.
        let fresh = self.leaving == Leaving::Staying && self.input_frame.is_none_or(|f| p > f);
        if let (true, Some(_), Some(l), Some(view)) = (fresh, &self.lead, &self.local, &self.published) {
            let at = self.clock.host_time_of(p);
            for (i, side) in [Hand::Left, Hand::Right].into_iter().enumerate() {
                let pose = l
                    .hands
                    .hand_at(side, at)
                    .filter(|h| h.tracked)
                    .and_then(|h| local::world_pose(&view.local_rig, &h).map(|w| (w.pos, w.rot, h.discontinuity)));
                self.motion.observe(p, i, pose);
            }
            self.stats.moving_frames += u64::from(self.motion.moving());
        }
        let moving = self.motion.moving();
        let (stamp, pose_time) = match (&mut self.lead, &self.local) {
            (Some(lead), Some(l)) => {
                let reports: Vec<(i64, u64)> = self.agree.own_lateness().iter().map(|r| (r.frames, r.seq)).collect();
                self.stats.lead = lead.update(p, &reports, l.chain.head().1 + 1, moving);
                (self.stats.largest_lead, self.stats.longest_drain) = (lead.largest(), lead.longest_drain());
                let stamp = lead.stamp(p, l.chain.last_stamp());
                (stamp, fresh.then(|| lead.pose_time(p, stamp, moving)))
            }
            _ => (p, None),
        };
        // The graceful quit (app.md 4): once asked, this member's `Leave` goes out first in the frame, at the frame's
        // stamp (never below the chain's last, which a lead may hold above the present), and it is the chain's last
        // entry. No edit or hand entry follows it, and an edit still queued is refused as agreement would refuse it.
        if self.leaving == Leaving::Asked
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
        if leaving && let Some(api) = &mut self.api {
            for (ticket, _) in api.ports.queue.drain(usize::MAX) {
                api.ports.queue.settle(ticket, Receipt::Refused(pe_seams::agree::SubmitError::AfterLeave));
            }
        }
        if let (false, Some(l), Some(api)) = (leaving, &mut self.local, &mut self.api)
            && api.edit_frame.is_none_or(|f| p > f)
        {
            api.edit_frame = Some(p);
            for (ticket, mut sub) in api.ports.queue.drain(1) {
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
        if let (true, Some(l), Some(view)) = (fresh, &mut self.local, &self.published) {
            if self.agree.backlogged() {
                self.stats.hands_paced += 1;
            } else {
                let at = pose_time.map_or_else(|| self.clock.host_time_of(p), |t| host_time_at(&*self.clock, t));
                let domain = self.replica.domain();
                // While the lead sheds, a frame's stamp can stand still: its poses would only replace the last frame's.
                let thin = self.input_stamp == Some(stamp);
                let (took, refused, thinned) = local::submit_hands(
                    &mut l.chain,
                    &mut l.sent,
                    &*l.hands,
                    view,
                    at,
                    stamp,
                    domain,
                    &mut *self.agree,
                    thin,
                );
                self.stats.inputs += u64::from(took);
                self.stats.inputs_refused += u64::from(refused);
                self.stats.poses_thinned += u64::from(thinned);
            }
            self.input_frame = Some(p);
            self.input_stamp = Some(stamp);
        }
        if let (Some(l), Some(api)) = (&self.local, &self.api) {
            let (chain, last) = l.chain.head();
            api.ports.shared.draft(chain, last + 1);
        }
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
        // 4. The replica, through pe-replica's driver. `present` is exclusive, so frame `p` is simulated. First the
        // budget's keep-up floor: what the last call needed to reach its present, rollback included, plus the frames
        // `p` advanced since the previous tick, plus one, capped (`Budget`).
        if let Some(b) = self.budget.at_present(p) {
            self.replica.set_budget(b);
        }
        let started = Instant::now();
        let report = self.driver.drive(&mut *self.replica, &mut *self.agree, Frame(p.0 + 1), self.self_test);
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
        // 5. S3's events. pe-diverge will take them in one ordered list with the finalize results.
        let events = self.agree.events();
        self.stats.events += events.len() as u64;
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
        // 6. The replica's newest view. While it catches up the view holds, so nothing new goes out.
        let view = self.replica.view();
        if !self.published.as_ref().is_some_and(|v| Arc::ptr_eq(v, &view)) {
            if let Some(api) = &self.api {
                self.feed_api(api, &view);
            }
            self.slot.publish(view.clone());
            self.published = Some(view);
            self.stats.published += 1;
        }
        Some((p, report))
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
        // The graceful quit's standing for the owner's wait, and the console's lines (app.md 4).
        shared.status.store(sim.leave_status().code(), Ordering::Release);
        shared.bound_ms.store(u64::try_from(sim.leave_bound().as_millis()).unwrap_or(u64::MAX), Ordering::Release);
        if sim.leaving != Leaving::Staying {
            *shared.others.lock().unwrap_or_else(|e| e.into_inner()) = sim.other_members();
            shared.asked.store(true, Ordering::Release);
        }
        for line in sim.take_notices() {
            println!("{line}");
        }
        // 7. Wait for the next frame or a wake. A tick that ran long leaves no wait, and the next one jumps ahead.
        let due = sim.next_due(ticked, now);
        let left = due.0.saturating_sub(host().0);
        if left > 0 {
            thread::park_timeout(Duration::from_nanos(left as u64));
        }
    }
    sim.stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{self, LocalClock};
    use crate::standin::{EmptyReplica, SoloAgreement};
    use pe_replica::mock::MockWorld;
    use pe_replica::{ReplicaConfig, ReplicaImpl};
    use pe_seams::{DomainId, LineId, LinkStats, Received};

    const RATE: SimRate = SimRate::DEFAULT;

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
    fn runner() -> Box<dyn pe_replica::ScriptRunner> {
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
    struct StillHands([f32; 3]);

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
        let tx =
            Transaction { actor: Actor::Person, label: None, preconditions: vec![], ops: vec![create], blobs: vec![] };
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
        let tx = Transaction { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] };
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

    /// While agreement reports this member backlogged (S3 v1.5), step 2 makes no hand inputs, and it makes them again
    /// once the backlog clears (@world-chunks's stall fix; @coordinator-opus).
    #[test]
    fn hand_inputs_wait_while_agreement_reports_a_backlog() {
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
        backlog.store(true, Ordering::Relaxed);
        for f in 2..=4 {
            s.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        assert_eq!((s.stats().inputs, s.stats().hands_paced), (0, 3), "no hand inputs while backlogged");
        backlog.store(false, Ordering::Relaxed);
        s.tick(clock.host_time_of(Frame(5))).expect("ticked");
        assert_eq!((s.stats().inputs, s.stats().hands_paced), (2, 3), "both hands again once it clears");
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
        let mut s = s.with_local(crate::world::DEV_MEMBER, [7; 32], Arc::new(LostAfter(clock.clone(), 2)));
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

    /// A test hub's setting per one-way link, by (from, to).
    type ByLink<T> = Arc<std::sync::Mutex<std::collections::BTreeMap<(MemberKey, MemberKey), T>>>;

    /// An in-process network (S4) for `Sim`s in one test: what one sends, the addressee polls, stamped with the host
    /// time the test last set, plus the one-way delay from the sender to it (none unless set).
    #[derive(Clone, Default)]
    struct Hub {
        boxes: Arc<std::sync::Mutex<std::collections::BTreeMap<MemberKey, std::collections::VecDeque<Received>>>>,
        now: Arc<std::sync::atomic::AtomicI64>,
        /// One-way delays in host nanoseconds by (from, to): a message is polled once the test's time reaches its
        /// arrival. Each box is kept in arrival order, those arriving together in the order sent.
        delay: ByLink<i64>,
        /// Jittered one-way delays by (from, to), in place of `delay`: each message's is drawn uniformly from the
        /// range, inclusive, by a fixed sequence, so a run repeats.
        jitter: ByLink<(i64, i64)>,
        /// Messages sent, for the jitter's sequence.
        sent: Arc<std::sync::atomic::AtomicU64>,
    }

    struct Port {
        me: MemberKey,
        hub: Hub,
        events: Arc<std::sync::Mutex<Vec<NetEvent>>>,
    }

    impl Port {
        fn new(me: MemberKey, hub: Hub) -> Port {
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
        fn on_event(&mut self, event: &NetEvent, _: &mut dyn Agreement) {
            if matches!(event, NetEvent::LinkUp { .. }) {
                self.0.lock().expect("the counts").0 += 1;
            }
        }
        fn on_tick(&mut self, _: HostTime, _: &mut dyn Agreement) {
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

    /// app.md 7, no stall: a backlog of `W_fg` (2 s) at the floor budget. Every tick still ticks agreement and the
    /// driver, the view holds while the replica catches up, and it catches up within `W_fg` plus two ticks.
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
        let mut ticks = 0u64;
        loop {
            let (_, report) = s.tick(clock.host_time_of(Frame(w_fg + ticks))).expect("ticked");
            ticks += 1;
            if report.advance.caught_up {
                break;
            }
            assert_eq!(slot.newest().map(|v| v.frame), Some(Frame(0)), "the view holds at tick {ticks}");
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
