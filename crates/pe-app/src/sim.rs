//! The simulation thread (app.md 1, 3): one tick per frame. It never waits on rendering, the network or the API.
//!
//! [`Sim::tick`] runs app.md 3's steps in order. This first slice runs step 3 (agreement's time), step 4
//! (pe-replica's [`Driver::drive`], within the replica's [`Budget`]), the draining half of step 5 (S3's events,
//! counted until pe-diverge takes them) and step 6 (the publish). Step 1 (the network inbox, pe-net), step 2 (local
//! input, pe-world's codecs) and the rest of step 5 (pe-diverge's ordered list and its actions) join as those crates
//! land.
//!
//! [`spawn`] runs the loop on a thread of its own and does step 7: between ticks it waits until the next frame starts,
//! or until a [`Waker`] wakes it (the network inbox and the edit queue will hold one).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pe_render::present::FrameSlot;
use pe_replica::{DriveReport, Driver};
use pe_seams::{Agreement, Frame, FrameClock, FrameView, HostTime, Replica, SelfTest, SimRate};

use crate::clock::HostClock;

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
    /// Messages agreement made that had nowhere to go: there is no network thread in this slice.
    pub unsent: u64,
    /// S3's events, drained each tick. pe-diverge takes them once it lands.
    pub events: u64,
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
        }
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
        // 3. Agreement's time. Its messages go to the network's outbox once pe-net's runner is wired.
        self.stats.unsent += self.agree.tick(now, &*self.clock).len() as u64;
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

    /// When the next tick is due: the start of the frame after `ticked`, or of frame 0, but never sooner than a
    /// millisecond from `now` while the clock has no frame yet.
    fn next_due(&self, ticked: Option<Frame>, now: HostTime) -> HostTime {
        match ticked {
            Some(p) => self.clock.host_time_of(Frame(p.0 + 1)),
            None => self.clock.host_time_of(Frame(0)).max(now.plus(Duration::from_millis(1))),
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
    use pe_seams::LineId;

    const RATE: SimRate = SimRate::DEFAULT;

    fn sim(clock: Arc<dyn FrameClock>, budget: Budget) -> (Sim, Arc<FrameSlot>) {
        let slot = Arc::new(FrameSlot::new());
        let replica = EmptyReplica::new(clock.clone(), Frame(0));
        let sim =
            Sim::new(Box::new(SoloAgreement::new(LineId::origin())), Box::new(replica), clock, slot.clone(), budget);
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
        // The next tick is due at frame 0, but never sooner than a millisecond away.
        assert_eq!(s.next_due(None, HostTime(epoch.0 - 2_000_000_000)), epoch);
        assert_eq!(s.next_due(None, HostTime(epoch.0 - 1)), HostTime(epoch.0 - 1 + 1_000_000));
    }

    #[test]
    fn a_tick_simulates_its_frame_and_publishes_the_view() {
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (mut s, slot) = sim(clock.clone(), Budget::new(RATE));
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
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
        let (mut s, slot) = sim(clock.clone(), Budget::with_share(RATE, 0.0));
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
