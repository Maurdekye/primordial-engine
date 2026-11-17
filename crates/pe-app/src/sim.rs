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
    AgreeEvent, Agreement, ChainId, Class, Cut, CutRef, EntryRef, Frame, FrameClock, FrameView, HandFeed, HostTime,
    Lane, MemberKey, NetEvent, Replica, SelfTest, SimRate, Transport,
};

use crate::clock::HostClock;
use crate::local::{self, Chain};

/// The replica's per-call budget (app.md 3; S2 v1.1), in simulated time. It is a local setting: it changes how fast the
/// replica catches up, never what it computes. It is the largest of:
/// - two frames at the world's rate (S2's floor): real time adds a frame per tick, and a budget of one frame never
///   catches up;
/// - the keep-up floor ([`Budget::at_present`]): the frames the frame clock's present advanced since the previous
///   tick, plus one. A tick also takes messages in, signs the hand inputs and runs agreement, and when that alone
///   outlasts a frame period, ticks run back to back and real time gains several frames on each. Sized from stepping
///   alone, the budget then let a replica fall further behind every tick, though its frames were five times cheaper
///   than real time (@engine-architect's debug measurement; @coordinator-opus, after @fable-reviewer).
///   The keep-up floor is capped at [`Budget::KEEP_UP_CAP`] of stepping at the measured cost of a frame, and at
///   [`Budget::MAX`]: a machine that steps slower than real time, or one back from a long stall, holds and reports
///   it, rather than making each tick longer than the last;
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
    /// The frames present advanced at the latest tick, plus one, before the cap; 0 before any tick.
    keep_up: u64,
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
        Budget { rate, share: share.max(0.0), cost_ns: None, fit: floor, present: None, keep_up: 0, frames: floor }
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
    /// least the frames present advanced since the previous tick, plus one, so that a replica whose frames are cheaper
    /// than a period gains at least a frame on real time every tick, whatever else the tick costs. The first tick, and
    /// a present that moved backwards, count as no advance. Returns the new budget when it changed.
    pub fn at_present(&mut self, present: Frame) -> Option<Duration> {
        let advanced = self.present.map_or(0, |last| present.0.saturating_sub(last.0));
        self.present = Some(self.present.map_or(present, |last| last.max(present)));
        self.keep_up = advanced.saturating_add(1);
        self.settle()
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
    /// The network (S4): pe-net's handle, or a test's in-process network. `None` is a world alone.
    net: Option<Box<dyn Transport + Send>>,
    net_hook: Option<Box<dyn NetHook>>,
    /// S3's events, kept for a test or a diagnostic (`with_event_log`); otherwise only counted.
    event_log: Option<Arc<std::sync::Mutex<Vec<AgreeEvent>>>>,
    /// pe-api's ports (app.md 3.1), with what this loop keeps for them.
    api: Option<ApiLoop>,
}

/// pe-api's ports and the loop's own state for them (app.md 3.1).
struct ApiLoop {
    ports: crate::api::Ports,
    /// The frame step 2 last took an edit for: one a frame.
    edit_frame: Option<Frame>,
    /// The entries of the `Voided` events drained since the last `confirm_cut`.
    voided: Vec<EntryRef>,
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
            net: None,
            net_hook: None,
            event_log: None,
            api: None,
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

    /// One tick at host time `now` (app.md 3): the frame it ticked and what the driver did, or `None` before frame 0.
    pub fn tick(&mut self, now: HostTime) -> Option<(Frame, DriveReport)> {
        self.stats.ticks += 1;
        let Some(p) = self.clock.frame_at(now) else {
            self.stats.before_frame_0 += 1;
            return None;
        };
        // 1. Messages in. Protocol and Action go to agreement; a refusal is a peer's fault, counted and never fatal.
        // TODO(pe-diverge): Repair to pe-diverge; TODO(blob store): Bulk; Pose is display-only.
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
            }
            if let Some(hook) = &mut self.net_hook {
                hook.on_tick(now, &mut *self.agree);
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
        if let (Some(l), Some(api)) = (&mut self.local, &mut self.api)
            && api.edit_frame.is_none_or(|f| p > f)
        {
            api.edit_frame = Some(p);
            for (ticket, sub) in api.ports.queue.drain(1) {
                let payload = sub.tx.encode();
                let bytes = payload.len();
                let entry = l.chain.entry(self.replica.domain(), p, Class::Edit, payload);
                let receipt = match self.agree.submit(entry) {
                    Ok(signed) => {
                        l.chain.took(&signed);
                        self.stats.edits += 1;
                        // Indexed now, not only when a view's record names it, so `edit()` answers for this machine's
                        // own change at once (@api-eng's Low on slice 1).
                        api.ports.shared.index_edit(signed.id(), Arc::new(sub.tx), bytes);
                        let (chain, seq) = l.chain.head();
                        Receipt::Submitted { change: TxId::from(signed.id()), frame: p.0, chain, seq }
                    }
                    Err(e) => {
                        self.stats.edits_refused += 1;
                        Receipt::Refused(e)
                    }
                };
                api.ports.queue.settle(ticket, receipt);
            }
        }
        let fresh = self.input_frame.is_none_or(|f| p > f);
        if let (true, Some(l), Some(view)) = (fresh, &mut self.local, &self.published) {
            if self.agree.backlogged() {
                self.stats.hands_paced += 1;
            } else {
                let at = self.clock.host_time_of(p);
                let domain = self.replica.domain();
                let (took, refused) =
                    local::submit_hands(&mut l.chain, &mut l.sent, &*l.hands, view, at, p, domain, &mut *self.agree);
                self.stats.inputs += u64::from(took);
                self.stats.inputs_refused += u64::from(refused);
            }
            self.input_frame = Some(p);
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
        // budget's keep-up floor: the frames `p` advanced since the previous tick, plus one, capped (`Budget`).
        if let Some(b) = self.budget.at_present(p) {
            self.replica.set_budget(b);
        }
        let started = Instant::now();
        let report = self.driver.drive(&mut *self.replica, &mut *self.agree, Frame(p.0 + 1), self.self_test);
        let frames = report.advance.resimulated + report.advance.stepped;
        if let Some(b) = self.budget.observe(frames, started.elapsed()) {
            self.replica.set_budget(b);
        }
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
        if let Some(api) = &mut self.api {
            api.voided.extend(events.iter().filter_map(|e| match e {
                AgreeEvent::Voided { entry, .. } => Some(*entry),
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
}

impl SimThread {
    pub fn waker(&self) -> Waker {
        Waker(self.join.thread().clone())
    }

    /// Stops the loop after its current tick, and returns what it counted. The leave entry, and the wait for the cut
    /// that includes it (app.md 4), come with pe-agree and pe-net.
    pub fn stop(self) -> thread::Result<SimStats> {
        self.stop.store(true, Ordering::Release);
        self.join.thread().unpark();
        self.join.join()
    }
}

/// Starts the simulation thread on a stack of `stack` bytes (app.md 1: pe-logic's `Settings::thread_stack()`, since
/// it runs wasm guests), reading the time from `host`.
pub fn spawn(sim: Sim, host: HostClock, stack: usize) -> std::io::Result<SimThread> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let join = thread::Builder::new().name("pe-sim".into()).stack_size(stack).spawn(move || run(sim, &host, &flag))?;
    Ok(SimThread { join, stop })
}

fn run(mut sim: Sim, host: &HostClock, stop: &AtomicBool) -> SimStats {
    while !stop.load(Ordering::Acquire) {
        let now = host();
        let ticked = sim.tick(now).map(|(p, _)| p);
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
        let run = pe_logic::run::RunSettings::default();
        Box::new(pe_replica::logic::LogicRunner::new(pe_logic::Settings::default(), run).expect("a runner"))
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

    /// An in-process network (S4) for `Sim`s in one test: what one sends, the addressee polls, stamped with the host
    /// time the test last set.
    #[derive(Clone, Default)]
    struct Hub {
        boxes: Arc<std::sync::Mutex<std::collections::BTreeMap<MemberKey, std::collections::VecDeque<Received>>>>,
        now: Arc<std::sync::atomic::AtomicI64>,
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
            let at = HostTime(self.hub.now.load(Ordering::SeqCst));
            let r = Received { from: self.me, lane, bytes, at };
            self.hub.boxes.lock().expect("the hub").entry(to).or_default().push_back(r);
        }
        fn poll(&mut self) -> Option<Received> {
            self.hub.boxes.lock().expect("the hub").get_mut(&self.me)?.pop_front()
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
    /// as `run` does. It starts `behind` frames behind, and real time jumps by `stall` before tick `stall_at`. Returns
    /// each tick's frames stepped and whether the replica caught up (a tick that didn't is a hold, `count_hold`).
    /// `keep_up` false never calls `at_present`: the budget sized from stepping alone, as before the keep-up floor.
    fn model(
        cost: Duration,
        fixed: Duration,
        behind: u64,
        stall: (usize, Duration),
        ticks: usize,
        keep_up: bool,
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
            let steps = (present + 1).saturating_sub(next).min(b.frames());
            next += steps;
            if steps > 0 {
                b.observe(steps, cost * steps as u32);
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
        let with = model(cost, fixed, 120, never, 400, true);
        let first = with.iter().position(|&(_, up)| up).expect("the keep-up floor catches up");
        assert!(with[first..].iter().all(|&(_, up)| up), "and stays caught up");
        let without = model(cost, fixed, 120, never, 400, false);
        assert!(without.iter().all(|&(_, up)| !up), "the budget from stepping alone never catches up");
    }

    /// Test 2: a 10 s stall never gives a tick more than `KEEP_UP_CAP` of stepping, or more than `MAX`, and the
    /// replica still catches up after it. After the stall's first, capped tick the floor is what real time adds plus
    /// one, so with dear frames (3 ms, where the share fits only S2's floor) it gains about a frame a tick, and the
    /// 600 frames take several hundred short ticks.
    #[test]
    fn a_ten_second_stall_never_gives_a_tick_longer_than_the_cap() {
        for cost in [Duration::from_micros(300), Duration::from_millis(3)] {
            let ticks = model(cost, Duration::from_millis(2), 0, (50, Duration::from_secs(10)), 1500, true);
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
        let ticks = model(cost, Duration::from_millis(1), 0, (usize::MAX, Duration::ZERO), 200, true);
        let cap = (Budget::KEEP_UP_CAP.as_nanos() / cost.as_nanos()) as u64;
        assert!(ticks.iter().all(|&(steps, _)| steps <= cap.max(Budget::FLOOR_FRAMES)), "{ticks:?}");
        assert!(ticks[10..].iter().all(|&(_, up)| !up), "it holds: {:?}", &ticks[..20]);
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
}
