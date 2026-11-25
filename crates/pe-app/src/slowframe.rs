//! A deliberate slowdown on this machine only, for the friend test's K4 R3': a member that can't keep real time, run
//! with injected slowness (@coordinator-opus, after @fable-reviewer). No
//! machine here falls behind at 500 balls, even on one logical CPU beside 16 busy processes (K4's R3' runs), so the
//! slow-member path needs this to run on real machines at all.
//!
//! - **What it does:** `--slow-frame <ms>` arms it. At the end of every frame this machine simulates, live or
//!   re-simulated after a rollback, it busy-waits that many milliseconds ([`crate::fault::FaultWorld::end_frame`]). A
//!   really slow machine pays per frame it simulates, so its catch-up is as dear as its present; a wait per tick would
//!   make catch-up cheap. With `--slow-frame 20` a 60 Hz frame (16.7 ms) costs about 22 ms at 500 balls (about 2 ms of
//!   simulation, K4's R5'), so the machine simulates about 0.76 s a second and falls behind steadily.
//! - **The console's `slow <ms>`** changes it mid-run, `slow 0` stops it, and without the flag it is refused, as `fault`
//!   is without `--arm-fault`: R3' stops the slowness to check that confirmation comes back.
//! - **Wall time only:** the wait reads real time and touches nothing simulated or agreed, so a run with it gives the
//!   same roots as one without (`a_slowed_replica_steps_to_the_same_roots`). On a scripted test clock it costs wall
//!   time but no simulated lag, so it can't drive the slow-member path in an in-process test.
//! - **Visible while on:** a start-up line, a line at each change, the status line ("Slowed on purpose"), and the end
//!   line's `slow_frame_ms` and `slowed_frames`, so a recorded run is never taken for a really slow machine.
//! - **Only simulating waits:** the simulation thread, and the `replay` check's thread while it replays. Rendering,
//!   the network and the console don't, where a really slow machine would be late at those too.
//! - **What it leaves alone:** the start-up physics self-test steps its own world in pe-physics, not a replica, so
//!   start-up isn't slowed and the hello's digest is the same. `--check-replay` may run with it: the replay's replica
//!   waits too (every replica pe-app makes is a [`crate::fault::FaultWorld`]), which slows the replay and changes no
//!   root. Type `slow 0` before `replay` for a replay at full speed.
//! - At most [`MAX_MS`] a frame, so a typo can't hold the machine for minutes a rollback. No environment variable sets it.
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// The most a frame may wait, in milliseconds.
pub const MAX_MS: u32 = 1000;

/// The process's slowdown: main.rs arms it from `--slow-frame`, before any replica is made.
static ARMED: OnceLock<Arc<SlowFrame>> = OnceLock::new();

/// One machine's slowdown: the wait each simulated frame pays, and how many frames paid it.
#[derive(Debug, Default)]
pub struct SlowFrame {
    ms: AtomicU32,
    frames: AtomicU64,
}

impl SlowFrame {
    /// A slowdown of `ms` a frame (at most [`MAX_MS`]).
    pub fn new(ms: u32) -> Result<Arc<SlowFrame>, String> {
        let s = Arc::new(SlowFrame::default());
        s.set(ms)?;
        Ok(s)
    }

    /// The wait each frame pays now, in milliseconds; 0 when stopped.
    pub fn ms(&self) -> u32 {
        self.ms.load(Ordering::Relaxed)
    }

    /// Frames that waited so far.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Changes the wait: 0 stops it.
    pub fn set(&self, ms: u32) -> Result<(), String> {
        if ms > MAX_MS {
            return Err(format!("`slow {ms}`: at most {MAX_MS} ms a frame"));
        }
        self.ms.store(ms, Ordering::Relaxed);
        Ok(())
    }

    /// One simulated frame's wait: a busy-wait on real time, so the machine spends the CPU a slow one would. Windows'
    /// sleep granularity is too coarse for a few milliseconds.
    pub fn spin(&self) {
        let ms = self.ms();
        if ms == 0 {
            return;
        }
        self.frames.fetch_add(1, Ordering::Relaxed);
        let until = Instant::now() + Duration::from_millis(u64::from(ms));
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    }
}

/// The status line's part for a wait of `ms` a frame, `None` for 0 (stopped).
pub fn status(ms: u32) -> Option<String> {
    (ms > 0).then(|| format!("Slowed on purpose: {ms} ms a simulated frame (--slow-frame)"))
}

/// Arms this process's slowdown at `ms` a frame and returns the console's line saying so. For main.rs only, once,
/// before any replica is made: every replica made afterwards waits by it ([`armed`]). Tests never arm it; they give a
/// [`crate::fault::FaultWorld`] their own [`SlowFrame`].
pub fn arm(ms: u32) -> Result<String, String> {
    let s = SlowFrame::new(ms)?;
    ARMED.set(s).map_err(|_| "a slowdown is armed already".to_string())?;
    Ok(format!(
        "SLOWDOWN ARMED (--slow-frame {ms}): a deliberate test slowdown, not a slow machine. Every frame this machine \
         simulates, re-simulated ones too, waits {ms} ms of real time on the simulation thread; nothing simulated or \
         sent changes. `slow <ms>` changes it, `slow 0` stops it."
    ))
}

/// This process's slowdown, if `--slow-frame` armed one.
pub fn armed() -> Option<Arc<SlowFrame>> {
    ARMED.get().cloned()
}

/// What the console answers to a typed `slow <ms>`: its line, once the new wait is set.
pub fn on_command(slow: Option<&SlowFrame>, arg: &str) -> Result<String, String> {
    let slow = slow.ok_or_else(|| "`slow` is refused: this machine wasn't started with --slow-frame".to_string())?;
    let ms: u32 =
        arg.trim().parse().map_err(|_| format!("`slow {}`: say how many ms a frame, 0 to stop", arg.trim()))?;
    slow.set(ms)?;
    Ok(if ms == 0 {
        "SLOWDOWN STOPPED (--slow-frame): frames no longer wait".to_string()
    } else {
        format!("SLOWDOWN (--slow-frame): every simulated frame now waits {ms} ms")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wait is set, capped and stopped; it costs about its time per frame and counts the frames that paid it; the
    /// console refuses `slow` without the flag and a value it can't read.
    #[test]
    fn a_slowdown_waits_its_time_per_frame_and_stops_at_0() {
        let s = SlowFrame::new(3).expect("3 ms");
        let t0 = Instant::now();
        for _ in 0..5 {
            s.spin();
        }
        let took = t0.elapsed();
        assert!(took >= Duration::from_millis(15), "5 frames waited {took:?}");
        assert_eq!(s.frames(), 5);
        assert!(status(s.ms()).is_some_and(|l| l.starts_with("Slowed on purpose: 3 ms")));
        assert!(SlowFrame::new(MAX_MS + 1).is_err(), "capped");
        assert_eq!(on_command(Some(&s), "0"), Ok("SLOWDOWN STOPPED (--slow-frame): frames no longer wait".into()));
        let t0 = Instant::now();
        s.spin();
        assert!(t0.elapsed() < Duration::from_millis(2), "stopped");
        assert_eq!((s.frames(), status(s.ms())), (5, None));
        assert!(on_command(Some(&s), "20").is_ok_and(|l| l.contains("20 ms")));
        assert_eq!(s.ms(), 20);
        for bad in ["x", "-1", "", "1001"] {
            assert!(on_command(Some(&s), bad).is_err(), "`slow {bad}` refused");
        }
        assert!(on_command(None, "5").is_err(), "refused without --slow-frame");
        assert!(armed().is_none(), "no test arms the process's slowdown");
    }

    /// Wall time only (@fable-reviewer's constraint): two ball pits, one slowed 2 ms a frame, step the same 48 frames,
    /// with a rollback to frame 12 at frame 31, to the same state at every frame (every object's place, rotation and
    /// velocities in the view) and the same root at each cut on the way. Every frame the slowed one simulates waits,
    /// re-simulated ones too: each advance's waits are its `resimulated` plus its `stepped`.
    #[test]
    fn a_slowed_replica_steps_to_the_same_roots() {
        use pe_seams::{FinalCut, Frame, HostTime, Replica, SignedEntry};
        /// No entries at any frame; `changed` reports a change once, as a late entry would, to make a rollback.
        #[derive(Default)]
        struct Source {
            changed: Option<Frame>,
        }
        impl pe_seams::EntrySource for Source {
            fn final_cut(&self) -> Option<&FinalCut> {
                None
            }
            fn entries_at(&self, _frame: Frame) -> Option<&[SignedEntry]> {
                Some(&[])
            }
            fn kept_from(&self) -> Frame {
                Frame(0)
            }
            fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}
            fn take_changed_from(&mut self) -> Option<Frame> {
                self.changed.take()
            }
        }
        let rate = pe_seams::SimRate::DEFAULT;
        let me = crate::world::DEV_MEMBER;
        let runner = || {
            let (settings, run) = crate::net::runner_settings();
            Box::new(pe_replica::logic::LogicRunner::new(settings, run).expect("a runner"))
                as Box<dyn pe_replica::ScriptRunner>
        };
        let slow = SlowFrame::new(2).expect("2 ms");
        let make = |slow: Option<Arc<SlowFrame>>| {
            let clock: Arc<dyn pe_seams::FrameClock> = Arc::new(crate::clock::LocalClock::new(HostTime(0), rate));
            let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), pe_seams::Pos::ORIGIN, 8);
            let mut r = crate::world::ball_pit_replica_over(world, &[me], me, rate, clock, runner(), |w| {
                crate::fault::FaultWorld::new(w, None).with_slow(slow)
            })
            .expect("a replica");
            // A budget that never splits the rollback over two advances.
            r.set_budget(Duration::from_secs(10));
            r
        };
        let (mut plain, mut slowed) = (make(None), make(Some(slow.clone())));
        let (mut plain_src, mut slowed_src) = (Source::default(), Source::default());
        // The roots: S2's root at each frame of the cut grid as the replicas reach it, on a cut with no entries.
        let every = crate::genesis::settings().cut_every(rate.hz());
        let line = pe_world::preset::m1_line();
        let (mut roots, mut resimulated) = (0, 0);
        let t0 = Instant::now();
        for f in 1..=48u64 {
            if f == 31 {
                (plain_src.changed, slowed_src.changed) = (Some(Frame(12)), Some(Frame(12)));
            }
            plain.advance(Frame(f), &mut plain_src);
            let waited = slow.frames();
            let a = slowed.advance(Frame(f), &mut slowed_src);
            assert_eq!(slow.frames() - waited, a.resimulated + a.stepped, "frame {f}: one wait a simulated frame");
            resimulated += a.resimulated;
            let state = |r: &dyn Replica| {
                let v = r.view();
                v.objects
                    .iter()
                    .map(|o| (o.id, o.pos, o.rotation, o.linear_velocity, o.angular_velocity))
                    .collect::<Vec<_>>()
            };
            assert_eq!((slowed.frame(), state(&slowed)), (plain.frame(), state(&plain)), "frame {f}");
            let at = slowed.frame();
            if at.0 > 0 && at.0.is_multiple_of(every) {
                let cut =
                    pe_seams::Cut { at: pe_seams::CutRef { line, height: at.0 / every, frame: at }, heads: vec![] };
                let mine = slowed.root_at_cut(&cut, &mut slowed_src).expect("the slowed replica's root");
                assert_eq!(
                    mine,
                    plain.root_at_cut(&cut, &mut plain_src).expect("the plain replica's root"),
                    "at {at:?}"
                );
                roots += 1;
            }
        }
        assert!(roots >= 4, "{roots} roots compared (a cut every {every} frames)");
        assert!(resimulated >= 19, "the rollback re-simulated {resimulated} frames");
        let took = t0.elapsed();
        assert!(took >= Duration::from_millis(2 * slow.frames()), "{} waits took {took:?}", slow.frames());
    }
}
