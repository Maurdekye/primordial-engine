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

use pe_render::present::FrameSlot;
use pe_replica::{DriveReport, Driver};
use pe_seams::{
    Agreement, Frame, FrameClock, FrameView, HandFeed, HostTime, Lane, MemberKey, NetEvent, Replica, SelfTest, SimRate,
    Transport,
};

use crate::clock::HostClock;
use crate::local::{self, Chain};

/// The replica's per-call budget (app.md 3; S2 v1.1), in simulated time. It is never below two frames at the world's
/// rate, because real time adds a frame per tick and a budget of one frame never catches up. Above that floor it
/// follows the measured cost of a frame, so that one call's work fits in a share of a frame period. It is a local
/// setting: it changes how fast the replica catches up, never what it computes.
#[derive(Clone, Debug)]
pub struct Budget {
    rate: SimRate,
    /// The share of a frame period one call may spend; 0 keeps the budget at the floor.
    share: f64,
    /// The measured wall time of one frame, in nanoseconds, smoothed.
    cost_ns: Option<f64>,
    frames: u64,
}

impl Budget {
    /// S2's floor.
    pub const FLOOR_FRAMES: u64 = 2;
    /// Half a frame period: the tick also runs agreement, pe-diverge and the publish.
    pub const DEFAULT_SHARE: f64 = 0.5;
    /// The most simulated time one call is given, however cheap a frame is: twice `W_fg`.
    pub const MAX: Duration = Duration::from_secs(4);

    pub fn new(rate: SimRate) -> Budget {
        Budget::with_share(rate, Budget::DEFAULT_SHARE)
    }

    pub fn with_share(rate: SimRate, share: f64) -> Budget {
        Budget { rate, share: share.max(0.0), cost_ns: None, frames: Budget::FLOOR_FRAMES }
    }

    /// The budget in frames.
    pub fn frames(&self) -> u64 {
        self.frames
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
        let frames = fit.clamp(Budget::FLOOR_FRAMES, self.rate.frames(Budget::MAX));
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
    /// The network (S4): pe-net's handle, or a test's in-process network. `None` is a world alone.
    net: Option<Box<dyn Transport + Send>>,
    net_hook: Option<Box<dyn NetHook>>,
}

/// What net.rs does on the simulation thread (@shader-lang m8a27b2a40aeb). Admission is decided from the agreed
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
            net: None,
            net_hook: None,
        }
    }

    /// The network (app.md 2): step 1 drains `net` into agreement and step 3 sends agreement's messages through it.
    /// `hook` gets each of its events after they are counted, and a call once a tick; without it events are only
    /// counted.
    pub fn with_transport(mut self, net: Box<dyn Transport + Send>, hook: Option<Box<dyn NetHook>>) -> Sim {
        self.net = Some(net);
        self.net_hook = hook;
        self
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
                if let Some(hook) = &mut self.net_hook {
                    hook.on_event(&e, &mut *self.agree);
                }
            }
            if let Some(hook) = &mut self.net_hook {
                hook.on_tick(now, &mut *self.agree);
            }
        }
        // 2. Local input: both hands at frame `p`'s host time, through the newest view's rig, one input entry each.
        // Before the first view the hands' objects are not known yet, so nothing goes in.
        if let (Some(l), Some(view)) = (&mut self.local, &self.published) {
            let at = self.clock.host_time_of(p);
            let domain = self.replica.domain();
            let (took, refused) =
                local::submit_hands(&mut l.chain, &mut l.sent, &*l.hands, view, at, p, domain, &mut *self.agree);
            self.stats.inputs += u64::from(took);
            self.stats.inputs_refused += u64::from(refused);
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
        // 4. The replica, through pe-replica's driver. `present` is exclusive, so frame `p` is simulated.
        let started = Instant::now();
        let report = self.driver.drive(&mut *self.replica, &mut *self.agree, Frame(p.0 + 1), self.self_test);
        let frames = report.advance.resimulated + report.advance.stepped;
        if let Some(b) = self.budget.observe(frames, started.elapsed()) {
            self.replica.set_budget(b);
        }
        self.stats.votes += report.votes.len() as u64;
        self.stats.finalized += report.finalized.len() as u64;
        self.count_hold(report.advance.caught_up);
        // 5. S3's events. pe-diverge will take them in one ordered list with the finalize results.
        self.stats.events += self.agree.events().len() as u64;
        // 6. The replica's newest view. While it catches up the view holds, so nothing new goes out.
        let view = self.replica.view();
        if !self.published.as_ref().is_some_and(|v| Arc::ptr_eq(v, &view)) {
            self.slot.publish(view.clone());
            self.published = Some(view);
            self.stats.published += 1;
        }
        Some((p, report))
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

    /// pe-replica's replica over a new, empty pe-world world, as the binary opens it.
    fn on_pe_world(clock: Arc<dyn FrameClock>, budget: Budget) -> (Sim, Arc<FrameSlot>) {
        let replica = crate::world::open_new(0, RATE, Duration::from_millis(133), clock.clone()).expect("a replica");
        with_replica(replica, clock, budget)
    }

    fn in_the_ball_pit(clock: Arc<dyn FrameClock>, budget: Budget) -> (Sim, Arc<FrameSlot>) {
        let me = crate::world::DEV_MEMBER;
        let replica = crate::world::open_ball_pit(7, me, RATE, Duration::from_millis(133), clock.clone()).expect("pit");
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
            let replica = crate::world::ball_pit(world, &members, me, RATE, Duration::from_millis(133), clock.clone())
                .expect("a replica");
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
