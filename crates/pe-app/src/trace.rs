//! The simulation thread's tick trace (`PE_TICK_TRACE`; @coordinator-opus, after the M1 recording's
//! 2.13 s tick, which b.log's totals could not place). Off unless the variable names a file. Then [`crate::sim::Sim`]
//! holds a [`TickTrace`], and each tick records its phases, what the replica stepped and the thread's CPU time. Off, a
//! tick pays one branch on an empty `Option`.
//!
//! The file starts with a `#` line, then has two kinds of line, each a word and `key=value` pairs, times in ms:
//! - `sec`: one per second of the frame clock, for that second's ticks: sums, the worst tick, and the state at its end.
//!   Its `inputs`, `refused`, `thinned` and `unpublished` count, in that second, the hand entries agreement took
//!   from this member, those it refused, the frames whose pose-only entries step 2 thinned while backlogged (F3 S1),
//!   and the ticks that published no view. A member whose hands are held makes no inputs, nor does an untracked
//!   hand, so the self-test checker's no_hold reads `inputs` (with `refused` beside it). A stretch of
//!   `unpublished` is a stretch with no new view (the friend test's K8). `holds` is the run's total so far;
//!   [`crate::sim::SimStats`] has the meaning of each. Its `lag_max`, and a `long` line's `lag`, is where the replica
//!   is stepping, the present less its frame: it counts a rollback's re-simulation, which the slow watch's lag, the
//!   shown world's age, doesn't ([`crate::slow`]). Last on the line, `others_input`, `others_edit` and
//!   `others_other` count the other members' entries agreement held, by class, at each frame when it became final or
//!   a second old, whichever came first: what of theirs this machine holds to apply (a held entry not yet final may
//!   still be voided as late). A final frame's entries are its cut's, and a second on an entry in flight has
//!   arrived, so a frame short of another member's entries is one this machine hadn't admitted by then: deferred at
//!   its pending cap (agreement.md 3), fetched late or never sent. The trace can't tell late from never: a deferred
//!   entry admitted later is applied by a rollback and never counted. An author whose entries this machine defers
//!   drops to 0 here while its own `inputs` go on. `pending` is agreement's entries pending finality at the second's
//!   end. `others_late_max_ms` is the largest receive lateness among the entries counted that second: how long after
//!   its stamp's time an entry reached this machine (pe-net's receive time; negative: before it, stamped ahead), `-`
//!   for none. It leaves out the author's stamp-ahead lead, which this machine doesn't know, so an action's latency is
//!   it plus that lead, and it carries the two machines' clock-sync error (@diverge-eng, for
//!   transport.md 9's action-latency figure). It counts refetched and backlog entries too, so after a loss a second's
//!   largest is the refetch's: quote it over seconds without one. An entry agreement deferred at its pending cap
//!   carries the arrival pe-agree keeps for it, its admission until @diverge-eng's arrival-time fix and its network
//!   arrival after (@design-reviewer on d7c8e33f). `lead_max` is the largest stamp-ahead lead this member stamped its
//!   own entries with that second, in frames ([`crate::stamp`]): its own actions lag by it, so a rise shows here
//!   (the friend test's host lag, @item:primordial-engine/friend-test-11-27-the-host-s-0-5-s-input-lag-at-j).
//! - `long`: one per tick longer than `PE_TICK_TRACE_MS` (default 50), with its every phase. `cpu_ms` against
//!   `tick_ms` says whether the thread ran or waited: descheduled, or blocked on the console, a lock or the disk.
//!   Windows counts thread time in clock ticks (about 15.6 ms), so `cpu_ms` is coarse below that.
//!
//! Between two ticks the loop does its own work (the console's lines, the quit's shared state), then waits for the
//! next frame. `loop_ms` is that work, after the tick it follows, and `late_ms` how long past the end of the wait the
//! next tick started: a console write that blocks (a selection in a QuickEdit console) shows in `loop_ms`, a thread
//! woken late in `late_ms`. Either one over the threshold also writes a `long` line, for the tick after it.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use pe_seams::{Frame, HostTime};

use crate::phases::PhaseTimes;

/// The default threshold of a `long` line, in milliseconds.
pub const LONG_MS: u64 = 50;

/// What a tick's record is measured against: the counters at its start.
pub struct Before {
    phases: PhaseTimes,
    received: u64,
    cpu_ns: u64,
}

impl Before {
    pub fn take(phases: &PhaseTimes, received: u64) -> Before {
        Before { phases: phases.clone(), received, cpu_ns: thread_cpu_ns() }
    }
}

/// One tick's state at its end, besides the counters.
pub struct Tick {
    /// The frame clock's present and the shown present.
    pub p: Frame,
    pub ps: Frame,
    pub now: HostTime,
    pub tick_ns: u64,
    pub rolled_back_to: Option<Frame>,
    pub caught_up: bool,
    /// The replica's frame after the tick, and the last final cut's.
    pub frame: Frame,
    pub final_frame: Option<Frame>,
    /// The budget in frames and the measured cost of a frame.
    pub budget: u64,
    pub cost_ns: Option<u64>,
    pub backlogged: bool,
    pub holds: u64,
    /// The run's totals at the tick's end: the hand entries agreement took from this member and those it refused,
    /// the frames step 2 thinned (F3 S1), and the views published (`SimStats::inputs`, `inputs_refused`,
    /// `hands_floor_thinned`, `published`). The trace writes each second's increase.
    pub inputs: u64,
    pub refused: u64,
    pub thinned: u64,
    pub published: u64,
    /// The run's total of hand poses not sent because the hand was past its body's reach
    /// (`SimStats::hands_out_of_reach`): a second with no entries but some of these is a hand out of reach, not a hold.
    pub out_of_reach: u64,
    /// The other members' entries agreement held, by class (`Class` as an index: Input, Edit, Governance, Protocol), at
    /// the frames that became final or a second old in this tick, whichever came first. Not a running total.
    pub others: [u64; 4],
    /// Agreement's entries pending finality at the tick's end (`AgreeStats::pending_entries`).
    pub pending: usize,
    /// The largest receive lateness among the entries in `others`, in ns: arrival (`EntrySource::arrived_at`, pe-net's
    /// receive time) less its stamp's host time; `None` when none was counted or none has an arrival time.
    pub others_late_max_ns: Option<i64>,
    /// The stamp-ahead lead in force at the tick's end, in frames (0 without a controller).
    pub lead: u64,
}

#[derive(Default)]
struct Second {
    ticks: u64,
    tick_ns: u64,
    tick_max_ns: u64,
    cpu_ns: u64,
    loop_max_ns: u64,
    late_max_ns: u64,
    lag_max: u64,
    backlogged: u64,
    received: u64,
    inputs: u64,
    refused: u64,
    thinned: u64,
    unpublished: u64,
    out_of_reach: u64,
    others: [u64; 4],
    others_late_max_ns: Option<i64>,
    lead_max: u64,
    phases: PhaseTimes,
}

/// The trace file and the second being summed.
pub struct TickTrace {
    out: BufWriter<File>,
    long_ns: u64,
    rate_hz: u64,
    /// The host time of the first tick recorded: `long` lines count from it.
    first: Option<HostTime>,
    /// The second being summed, by the frame clock, and the last tick's state for its line.
    second: Option<u64>,
    sums: Second,
    last: Option<Tick>,
    /// The loop's work after the last tick, and when its wait was to end ([`TickTrace::wait`]).
    wait: Option<(u64, HostTime)>,
}

impl TickTrace {
    /// The trace `PE_TICK_TRACE` names, if any, at `rate_hz`; `PE_TICK_TRACE_MS` sets a `long` line's threshold.
    pub fn from_env(rate_hz: u32) -> Option<std::io::Result<(TickTrace, String)>> {
        let path = std::env::var_os("PE_TICK_TRACE").filter(|p| !p.is_empty())?;
        let ms = std::env::var("PE_TICK_TRACE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(LONG_MS);
        let shown = Path::new(&path).display().to_string();
        Some(TickTrace::create(Path::new(&path), ms, rate_hz).map(|t| (t, shown)))
    }

    /// A new trace in a new file at `path` (an existing one is refused, as `--log` refuses one).
    pub fn create(path: &Path, long_ms: u64, rate_hz: u32) -> std::io::Result<TickTrace> {
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut out = BufWriter::new(file);
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        writeln!(
            out,
            "# pe-app tick trace, opened at Unix time {:.3}: `sec` per frame-clock second, `long` per tick over {long_ms} \
             ms; times in ms",
            unix.as_secs_f64()
        )?;
        out.flush()?;
        Ok(TickTrace {
            out,
            long_ns: long_ms.saturating_mul(1_000_000),
            rate_hz: u64::from(rate_hz.max(1)),
            first: None,
            second: None,
            sums: Second::default(),
            last: None,
            wait: None,
        })
    }

    /// The loop after a tick that ended at `tick_end`: it starts its wait at `from`, for the next tick due at `due`.
    /// A wait that would end before it starts doesn't happen, so the next tick is late only past the later of the two.
    pub fn wait(&mut self, tick_end: HostTime, from: HostTime, due: HostTime) {
        let work = u64::try_from(from.0.saturating_sub(tick_end.0)).unwrap_or(0);
        self.wait = Some((work, HostTime(due.0.max(from.0))));
    }

    /// Records one tick: `before` its counters at its start, `phases` and `received` at its end. A write error is
    /// dropped: the trace never stops the simulation.
    pub fn record(&mut self, before: &Before, phases: &PhaseTimes, received: u64, t: Tick) {
        let _ = self.try_record(before, phases, received, t);
    }

    fn try_record(&mut self, before: &Before, phases: &PhaseTimes, received: u64, t: Tick) -> std::io::Result<()> {
        let d = delta(&before.phases, phases);
        let cpu_ns = thread_cpu_ns().saturating_sub(before.cpu_ns);
        let (loop_ns, late_ns) = self
            .wait
            .take()
            .map_or((0, 0), |(work, end)| (work, u64::try_from(t.now.0.saturating_sub(end.0)).unwrap_or(0)));
        let received = received.saturating_sub(before.received);
        let first = *self.first.get_or_insert(t.now);
        let sec = t.p.0 / self.rate_hz;
        if self.second.is_some_and(|s| s != sec) {
            self.write_second()?;
        }
        self.second = Some(sec);
        let lag = (t.ps.0 + 1).saturating_sub(t.frame.0);
        let (inputs, refused, thinned, published, out_of_reach) = self
            .last
            .as_ref()
            .map_or((0, 0, 0, 0, 0), |l| (l.inputs, l.refused, l.thinned, l.published, l.out_of_reach));
        let s = &mut self.sums;
        s.inputs += t.inputs.saturating_sub(inputs);
        s.out_of_reach += t.out_of_reach.saturating_sub(out_of_reach);
        for (sum, n) in s.others.iter_mut().zip(t.others) {
            *sum += n;
        }
        s.others_late_max_ns = s.others_late_max_ns.max(t.others_late_max_ns);
        s.refused += t.refused.saturating_sub(refused);
        s.thinned += t.thinned.saturating_sub(thinned);
        s.unpublished += u64::from(t.published == published);
        s.ticks += 1;
        s.tick_ns += t.tick_ns;
        s.tick_max_ns = s.tick_max_ns.max(t.tick_ns);
        s.cpu_ns += cpu_ns;
        s.loop_max_ns = s.loop_max_ns.max(loop_ns);
        s.late_max_ns = s.late_max_ns.max(late_ns);
        s.lag_max = s.lag_max.max(lag);
        s.lead_max = s.lead_max.max(t.lead);
        s.backlogged += u64::from(t.backlogged);
        s.received += received;
        add(&mut s.phases, &d);
        if t.tick_ns.max(loop_ns).max(late_ns) > self.long_ns {
            let at = (t.now.0.saturating_sub(first.0)) as f64 / 1e9;
            write!(
                self.out,
                "long t={at:.3} p={} ps={} tick_ms={} cpu_ms={} loop_ms={} late_ms={}",
                t.p.0,
                t.ps.0,
                ms(t.tick_ns),
                ms(cpu_ns),
                ms(loop_ns),
                ms(late_ns)
            )?;
            phases_line(&mut self.out, &d)?;
            writeln!(
                self.out,
                " rollback_to={} caught_up={} frame={} lag={lag} final={} final_lag={} budget={} cost_ms={} backlogged={} \
                 received={received}",
                t.rolled_back_to.map_or("-".to_string(), |f| f.0.to_string()),
                u8::from(t.caught_up),
                t.frame.0,
                t.final_frame.map_or("-".to_string(), |f| f.0.to_string()),
                final_lag(&t),
                t.budget,
                t.cost_ns.map_or("-".to_string(), ms),
                u8::from(t.backlogged),
            )?;
            self.out.flush()?;
        }
        self.last = Some(t);
        Ok(())
    }

    fn write_second(&mut self) -> std::io::Result<()> {
        let (Some(sec), Some(t)) = (self.second, self.last.as_ref()) else { return Ok(()) };
        let s = std::mem::take(&mut self.sums);
        write!(
            self.out,
            "sec s={sec} ticks={} tick_ms={} tick_max_ms={} cpu_ms={} loop_max_ms={} late_max_ms={} lag_max={} final_lag={} budget={} \
             cost_ms={} backlogged={} holds={} received={} inputs={} refused={} thinned={} unpublished={}",
            s.ticks,
            ms(s.tick_ns),
            ms(s.tick_max_ns),
            ms(s.cpu_ns),
            ms(s.loop_max_ns),
            ms(s.late_max_ns),
            s.lag_max,
            final_lag(t),
            t.budget,
            t.cost_ns.map_or("-".to_string(), ms),
            s.backlogged,
            t.holds,
            s.received,
            s.inputs,
            s.refused,
            s.thinned,
            s.unpublished,
        )?;
        phases_line(&mut self.out, &s.phases)?;
        // Last on the line, so the fields before them keep their places for the readers that match them in order.
        writeln!(
            self.out,
            " out_of_reach={} others_input={} others_edit={} others_other={} pending={} others_late_max_ms={} lead_max={}",
            s.out_of_reach,
            s.others[0],
            s.others[1],
            s.others[2] + s.others[3],
            t.pending,
            s.others_late_max_ns.map_or("-".to_string(), |ns| format!("{:.1}", ns as f64 / 1e6)),
            s.lead_max,
        )?;
        self.out.flush()
    }
}

impl Drop for TickTrace {
    /// The last second's line, then the rest of the buffer.
    fn drop(&mut self) {
        let _ = self.write_second();
        let _ = self.out.flush();
    }
}

/// How far the last final cut trails the present, in frames; `-` with none yet.
fn final_lag(t: &Tick) -> String {
    t.final_frame.map_or("-".to_string(), |f| t.p.0.saturating_sub(f.0).to_string())
}

fn ms(ns: u64) -> String {
    format!("{:.1}", ns as f64 / 1e6)
}

fn phases_line(out: &mut impl Write, d: &PhaseTimes) -> std::io::Result<()> {
    write!(
        out,
        " ingest_ms={} inputs_ms={} agree_ms={} drive_ms={} advance_ms={} roots_ms={} finalize_ms={} events_ms={} \
         diverge_ms={} publish_ms={} stepped={} resim={} rollbacks={} roots={}",
        ms(d.ingest_ns),
        ms(d.inputs_ns),
        ms(d.agree_ns),
        ms(d.drive_ns),
        ms(d.advance_ns),
        ms(d.roots_ns),
        ms(d.finalize_ns),
        ms(d.events_ns),
        ms(d.diverge_ns),
        ms(d.publish_ns),
        d.stepped,
        d.resimulated,
        d.rollbacks,
        d.roots,
    )
}

/// `after - before`, field by field (the counters only grow).
fn delta(before: &PhaseTimes, after: &PhaseTimes) -> PhaseTimes {
    let d = |a: u64, b: u64| a.saturating_sub(b);
    PhaseTimes {
        ingest_ns: d(after.ingest_ns, before.ingest_ns),
        inputs_ns: d(after.inputs_ns, before.inputs_ns),
        agree_ns: d(after.agree_ns, before.agree_ns),
        drive_ns: d(after.drive_ns, before.drive_ns),
        advance_ns: d(after.advance_ns, before.advance_ns),
        roots_ns: d(after.roots_ns, before.roots_ns),
        roots: d(after.roots, before.roots),
        finalize_ns: d(after.finalize_ns, before.finalize_ns),
        events_ns: d(after.events_ns, before.events_ns),
        diverge_ns: d(after.diverge_ns, before.diverge_ns),
        publish_ns: d(after.publish_ns, before.publish_ns),
        tick_ns: d(after.tick_ns, before.tick_ns),
        longest_tick_ns: 0,
        idle_ns: d(after.idle_ns, before.idle_ns),
        stepped: d(after.stepped, before.stepped),
        resimulated: d(after.resimulated, before.resimulated),
        rollbacks: d(after.rollbacks, before.rollbacks),
    }
}

fn add(to: &mut PhaseTimes, d: &PhaseTimes) {
    to.ingest_ns += d.ingest_ns;
    to.inputs_ns += d.inputs_ns;
    to.agree_ns += d.agree_ns;
    to.drive_ns += d.drive_ns;
    to.advance_ns += d.advance_ns;
    to.roots_ns += d.roots_ns;
    to.roots += d.roots;
    to.finalize_ns += d.finalize_ns;
    to.events_ns += d.events_ns;
    to.diverge_ns += d.diverge_ns;
    to.publish_ns += d.publish_ns;
    to.tick_ns += d.tick_ns;
    to.stepped += d.stepped;
    to.resimulated += d.resimulated;
    to.rollbacks += d.rollbacks;
}

/// This thread's CPU time (user and kernel), in nanoseconds; 0 where the platform gives none.
#[cfg(windows)]
pub fn thread_cpu_ns() -> u64 {
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut core::ffi::c_void;
        fn GetThreadTimes(
            thread: *mut core::ffi::c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }
    let (mut c, mut e, mut k, mut u) =
        (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
    // SAFETY: the pseudo-handle of the calling thread, which needs no closing, and four valid out pointers.
    let ok = unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) };
    if ok == 0 {
        return 0;
    }
    let t = |f: FileTime| (u64::from(f.high) << 32) | u64::from(f.low);
    // FILETIME counts 100 ns.
    (t(k) + t(u)).saturating_mul(100)
}

#[cfg(unix)]
pub fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid out pointer; CLOCK_THREAD_CPUTIME_ID is the calling thread's clock.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } != 0 {
        return 0;
    }
    #[allow(clippy::useless_conversion)]
    let ns = i64::from(ts.tv_sec).saturating_mul(1_000_000_000).saturating_add(i64::from(ts.tv_nsec));
    u64::try_from(ns).unwrap_or(0)
}

#[cfg(not(any(windows, unix)))]
pub fn thread_cpu_ns() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(p: u64, tick_ms: u64, now_ms: i64) -> Tick {
        Tick {
            p: Frame(p),
            ps: Frame(p),
            now: HostTime(now_ms * 1_000_000),
            tick_ns: tick_ms * 1_000_000,
            rolled_back_to: None,
            caught_up: true,
            frame: Frame(p + 1),
            final_frame: Some(Frame(p.saturating_sub(10))),
            budget: 2,
            cost_ns: Some(1_500_000),
            backlogged: false,
            holds: 0,
            inputs: 2 * (p + 1),
            refused: 0,
            thinned: 0,
            published: p + 1,
            out_of_reach: 0,
            others: [0; 4],
            pending: 0,
            others_late_max_ns: None,
            lead: 0,
        }
    }

    /// Each `sec` line counts its second's hand entries, thinned frames and ticks with no new view, from the run's
    /// totals (T4's no_hold reads `inputs`; the friend test's K8 reads `unpublished`).
    #[test]
    fn a_second_counts_its_hand_entries_thinned_frames_and_ticks_without_a_view() {
        let exe = std::env::current_exe().expect("the test executable");
        let path = exe.parent().expect("its folder").join(format!("pe-tick-trace-counts-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut t = TickTrace::create(&path, 50, 60).expect("a trace");
        let phases = PhaseTimes::default();
        let (mut inputs, mut thinned, mut published, mut out_of_reach) = (0, 0, 0, 0);
        for f in 0..180u64 {
            // Second 0: two entries a frame and a view each tick. Second 1: frames 70-89 publish nothing, and every
            // other frame of 100-119 is thinned to one entry. Second 2: held, no entries at all.
            match f {
                0..60 => (inputs, published) = (inputs + 2, published + 1),
                60..120 => {
                    let thin = (100..120).contains(&f) && f % 2 == 1;
                    inputs += if thin { 1 } else { 2 };
                    thinned += u64::from(thin);
                    published += u64::from(!(70..90).contains(&f));
                }
                _ => {
                    published += 1;
                    // Frames 120-149: both hands past their body's reach, so no entries but no hold either.
                    out_of_reach += if f < 150 { 2 } else { 0 };
                }
            }
            let refused = u64::from(f >= 150);
            // The other member's entries: an input a frame in second 1, an edit at frame 130, nothing else.
            let others = [u64::from((60..120).contains(&f)), u64::from(f == 130), 0, 0];
            let pending = f as usize;
            // Second 1's entries arrive 5 ms after their stamps (frame 100's 20 ms early); second 2's one 30 ms late.
            let others_late_max_ns = match f {
                100 => Some(-20_000_000),
                60..120 => Some(5_000_000),
                130 => Some(30_000_000),
                _ => None,
            };
            // The lead: 49 frames at frame 75 of second 1, shedding a frame a frame after; none in seconds 0 and 2.
            let lead = if (75..120).contains(&f) { 49 - (f - 75) } else { 0 };
            let at = Tick {
                lead,
                inputs,
                refused,
                thinned,
                published,
                out_of_reach,
                others,
                pending,
                others_late_max_ns,
                ..tick(f, 1, f as i64 * 1000 / 60)
            };
            t.record(&Before::take(&phases, 0), &phases, 0, at);
        }
        drop(t);
        let text = std::fs::read_to_string(&path).expect("the trace");
        let _ = std::fs::remove_file(&path);
        let secs: Vec<&str> = text.lines().filter(|l| l.starts_with("sec ")).collect();
        assert_eq!(secs.len(), 3, "{text}");
        assert!(secs[0].contains("inputs=120 refused=0 thinned=0 unpublished=0"), "{}", secs[0]);
        assert!(secs[1].contains("inputs=110 refused=0 thinned=10 unpublished=20"), "{}", secs[1]);
        // One refused entry, at frame 150: the running total goes from 0 to 1 there.
        assert!(secs[2].contains("inputs=0 refused=1 thinned=0 unpublished=0"), "{}", secs[2]);
        assert!(secs[0].contains(" out_of_reach=0 ") && secs[2].contains(" out_of_reach=60 "), "{}", secs[2]);
        assert!(
            secs[0]
                .ends_with(" others_input=0 others_edit=0 others_other=0 pending=59 others_late_max_ms=- lead_max=0")
        );
        let s1 = " others_input=60 others_edit=0 others_other=0 pending=119 others_late_max_ms=5.0 lead_max=49";
        assert!(secs[1].ends_with(s1), "{}", secs[1]);
        let s2 = " others_input=0 others_edit=1 others_other=0 pending=179 others_late_max_ms=30.0 lead_max=0";
        assert!(secs[2].ends_with(s2), "{}", secs[2]);
    }

    /// A `sec` line per frame-clock second, and a `long` line for each tick over the threshold with its phases and
    /// its lateness after the loop's wait.
    #[test]
    fn a_trace_has_a_line_a_second_and_one_per_long_tick() {
        let exe = std::env::current_exe().expect("the test executable");
        let path = exe.parent().expect("its folder").join(format!("pe-tick-trace-test-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut t = TickTrace::create(&path, 50, 60).expect("a trace");
        assert!(TickTrace::create(&path, 50, 60).is_err(), "an existing file is refused");
        let mut phases = PhaseTimes::default();
        for f in 0..130u64 {
            let before = Before::take(&phases, 0);
            let long = f == 70;
            phases.advance_ns += if long { 2_000_000_000 } else { 1_000_000 };
            phases.stepped += 1;
            if long {
                // The loop's work before it: 5 ms after a tick that ended at 1,100 ms, then a wait to 1,136 ms
                // that ended 30 ms before this tick started (1,166 ms).
                let ms = |v: i64| HostTime(v * 1_000_000);
                t.wait(ms(1100), ms(1105), ms(1136));
            }
            t.record(&before, &phases, 0, tick(f, if long { 2000 } else { 1 }, f as i64 * 1000 / 60));
        }
        drop(t);
        let text = std::fs::read_to_string(&path).expect("the trace");
        let _ = std::fs::remove_file(&path);
        let secs: Vec<&str> = text.lines().filter(|l| l.starts_with("sec ")).collect();
        let longs: Vec<&str> = text.lines().filter(|l| l.starts_with("long ")).collect();
        assert!(text.starts_with("# pe-app tick trace"), "{text}");
        assert_eq!(secs.len(), 3, "frames 0-59, 60-119 and 120-129: {text}");
        assert!(secs[0].contains("ticks=60") && secs[0].contains("stepped=60"), "{}", secs[0]);
        assert!(secs[1].contains("tick_max_ms=2000.0") && secs[1].contains("advance_ms=2059.0"), "{}", secs[1]);
        assert_eq!(longs.len(), 1, "{text}");
        assert!(longs[0].contains("p=70") && longs[0].contains("advance_ms=2000.0"), "{}", longs[0]);
        assert!(longs[0].contains("loop_ms=5.0 late_ms=30.0") && longs[0].contains("final_lag=10"), "{}", longs[0]);
    }

    /// The thread's CPU time moves while it works. It works until the reading moves, for up to 10 s of wall time, not
    /// for a fixed 60 ms: at idle priority on a loaded machine, 60 ms of wall time can give the thread less than one of
    /// Windows' 15.6 ms CPU-time ticks, and both readings were 0 (@vdw-physics's flake).
    #[test]
    fn the_thread_s_cpu_time_grows_with_work() {
        let start = thread_cpu_ns();
        let mut x = 0u64;
        let t = std::time::Instant::now();
        let mut now = start;
        while now <= start && t.elapsed() < std::time::Duration::from_secs(10) {
            for _ in 0..100_000 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            }
            now = thread_cpu_ns();
        }
        std::hint::black_box(x);
        assert!(now > start, "{start} then {now} after {:?} of work", t.elapsed());
    }
}
