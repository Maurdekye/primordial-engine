//! The merge cost gate (M2 S7, slice 1; @diverge-eng's package delta r1, "The cost gate comes first", and r2, "The
//! cost gate, extended"): what a package merge costs the healthy machine, and what a lagging voter pays to run two
//! simulations, measured on the ball pit before any merge engine code exists.
//!
//! **The merge.** Two members, A (the healthy voter) and B (cut off), share a history up to the base `Cb`. Over the
//! cut-off span `[Cb, F_M)` both keep acting, and neither sees the other's entries. The merge is an exact whole-world
//! re-step from the first contested stamp, as r1 scopes merge engine v1 (no grouping), and it isn't done at `F_M`:
//! the merged history must then catch up with A's present, which moved on while it ran (@design-reviewer's M1 on
//! 2fb8edb8). So:
//! 1. **A second replica of A's state** (A's entries, stepped to `F_M`, untimed) is handed B's package: B's span
//!    entries join its source and the source reports a change from `Cb`. Its next `advance` restores the newest
//!    keyframe at or below `Cb` and re-steps, as a merge replay on the rollback path does ([`Replica::advance`]). That
//!    is the replay, timed to `F_M`.
//! 2. **The chase:** it then steps on with A's entries towards A's frame, which A's loop keeps advancing, until it
//!    reaches it. That is the time to converge, about span / (ρ − 1): unbounded as ρ falls to 1.
//! 3. Meanwhile **A's frame loop**, paced to real time on its own thread, keeps stepping the present. Its tick times
//!    are taken alone first (the control), then over the whole replay and chase.
//!
//! **Measured** ([`GateReport`]): the replay's wall time and ρ (simulated seconds re-stepped per wall second, which
//! sets `R_merge`, divergence.md 14, @diverge-eng); the time to converge, or that it didn't within its
//! bound; A's ticks alone and during, with the ticks over one frame's budget (the gate needs none: r1); and at `F_M`,
//! A's corrections and B's overwrites: the balls whose state the merge changes from A's live state, and from B's own,
//! by hash and by visible position (more than 1 cm, and the largest move).
//!
//! **The lagging voter** ([`lagging_voter`]): r2's Live option 3 has a B that lags keep a second simulation every
//! frame, its playable state ahead of the canonical one. Measured as one replica stepped per frame against two,
//! unpaced: the cost ratio `k` and the frames each manages per second of wall time, so how fast the lag grows on a
//! machine that can't keep up with two.
//!
//! **What this is not [I]:**
//! - **Not measured:** the swap of the converged state into A's live replica, and the agreement's part of the time to
//!   a final `M` (the finality lag `L`).
//! - **The re-step's roots:** before re-solve E3, a re-step takes a root at every cut frame (42 % of re-step time in
//!   @diverge-eng's E2 trial), which a merge replay needn't. Figures before E3 lands are pessimistic by up to that.
//! - **Stand-ins:** B's actions are lifts (edits) and hand pushes (a hand move, then an impulse), as the ball-pit run
//!   makes them; a grab adds a joint per hold, little beside the pushes' body motion. The second simulation has none
//!   of option 3's rollbacks.
//! - **Contention:** two threads on an otherwise idle machine. pe-app also runs its render and compositor threads,
//!   so on the VR machines the clean run is the one that counts.
//! - **Pacing:** on Windows `thread::sleep` has the timer's resolution (up to about 15 ms), so A's cadence drifts.
//!   Its tick times are unaffected: only `advance` is timed.
//!
//! Each figure depends on the machine and its load: the ignored release test in `tests/merge_gate.rs` prints them,
//! and a clean run follows the coordinator's clean-run procedure.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pe_replica::{PeWorld, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    Class, Cut, CutRef, EntrySource, FinalCut, Frame, Hash, Hasher, LineId, MemberKey, ObjectId, Pos, Replica,
    SignedEntry,
};
use pe_world::{World, preset};

use crate::ballpit::{self, HZ};
use crate::clock::{MS, Nanos, SEC};

/// One gate run's shape. Frames are at [`HZ`]; `base` and `span` are multiples of the replicas' cut interval (six
/// frames), since a root is taken at both ends.
#[derive(Clone, Debug)]
pub struct Gate {
    pub seed: u64,
    pub balls: usize,
    /// `Cb`'s frame: the shared history before the cut-off.
    pub base: u64,
    /// The cut-off span, in frames: `F_M = base + span`.
    pub span: u64,
    /// One action every `every` across both members (each member's share drawn from the seed), the whole run.
    pub every: Nanos,
    /// How many frames A's live loop runs before the replay starts (the control), and at least while it runs.
    pub live: u64,
    /// Pace A's live loop to real time, as a member's frame loop is. Off, it steps flat out, and the chase can't end.
    pub paced: bool,
    /// The chase gives up after this many frames of A's loop past the replay's start.
    pub chase_frames: u64,
}

impl Gate {
    /// A run of `balls` balls with a span of `span_s` seconds, after two seconds of shared history, an action every
    /// 125 ms (about four a second per member), two seconds of A's loop before the replay, and up to ten minutes of
    /// chase.
    pub fn new(seed: u64, balls: usize, span_s: u64) -> Gate {
        let hz = u64::from(HZ);
        Gate {
            seed,
            balls,
            base: 2 * hz,
            span: span_s * hz,
            every: 125 * MS,
            live: 2 * hz,
            paced: true,
            chase_frames: 600 * hz,
        }
    }

    fn f_m(&self) -> u64 {
        self.base + self.span
    }
}

/// Tick times of a frame loop.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ticks {
    pub n: usize,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    /// Ticks longer than one frame's budget (1/[`HZ`]).
    pub over: usize,
}

impl Ticks {
    fn of(mut ns: Vec<u64>) -> Ticks {
        ns.sort_unstable();
        let n = ns.len();
        let q = |p: f64| {
            let i = ((p * n as f64).ceil() as usize).clamp(1, n.max(1)) - 1;
            ns.get(i).copied().unwrap_or(0) as f64 / 1e6
        };
        let budget = SEC / u64::from(HZ);
        Ticks {
            n,
            mean_ms: ns.iter().sum::<u64>() as f64 / n.max(1) as f64 / 1e6,
            p50_ms: q(0.5),
            p95_ms: q(0.95),
            p99_ms: q(0.99),
            max_ms: q(1.0),
            over: ns.iter().filter(|&&t| t > budget).count(),
        }
    }

    fn line(&self) -> String {
        format!(
            "n {} mean {:.2} p95 {:.2} p99 {:.2} max {:.2} over {}",
            self.n, self.mean_ms, self.p95_ms, self.p99_ms, self.max_ms, self.over
        )
    }
}

/// How the merge changes one side's balls at `F_M`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Changed {
    /// Balls whose hash differs (any bit of their state).
    pub by_hash: usize,
    /// Balls whose position moved more than [`VISIBLE_M`].
    pub visible: usize,
    /// The largest position change, in metres.
    pub max_m: f64,
}

/// A position change the user could see, in metres.
pub const VISIBLE_M: f64 = 0.01;

/// One gate run's figures.
#[derive(Clone, Debug)]
pub struct GateReport {
    pub gate: Gate,
    /// Entries each member signed in the cut-off span.
    pub a_entries: usize,
    pub b_entries: usize,
    /// The replay: frames re-stepped to `F_M`, and its wall time on its own thread.
    pub replay_frames: u64,
    pub replay_wall: Duration,
    /// From the package's arrival to the merged state reaching A's frame, if it did within the bound; and the frames
    /// it stepped past `F_M` meanwhile.
    pub converged: Option<Duration>,
    pub chase_frames: u64,
    /// A's loop with nothing else running, then over the replay and the chase.
    pub a_alone: Ticks,
    pub a_during: Ticks,
    /// The balls compared, and how the merge changes A's and B's.
    pub objects: usize,
    pub corrections: Changed,
    pub overwrites: Changed,
}

impl GateReport {
    /// The replay speed over real time, `ρ` (divergence.md 9.3a): simulated seconds re-stepped per wall second.
    pub fn rho(&self) -> f64 {
        self.replay_frames as f64 / f64::from(HZ) / self.replay_wall.as_secs_f64().max(1e-9)
    }

    pub fn replay_ms_per_frame(&self) -> f64 {
        self.replay_wall.as_secs_f64() * 1e3 / self.replay_frames.max(1) as f64
    }

    /// One line per run, for the trial's output.
    pub fn line(&self) -> String {
        let c = |x: &Changed| format!("{} by hash, {} > 1 cm, max {:.3} m", x.by_hash, x.visible, x.max_m);
        let conv = match self.converged {
            Some(d) => format!("converged in {:.0} ms ({} frames past F_M)", d.as_secs_f64() * 1e3, self.chase_frames),
            None => format!("did NOT converge ({} frames past F_M)", self.chase_frames),
        };
        // The replica restores its newest keyframe at or below Cb, so the re-step starts this many frames early: the
        // wall time is longer than a restore at Cb exactly would give, while ms/frame and rho use the real count.
        let below = self.replay_frames.saturating_sub(self.gate.span);
        format!(
            "gate balls {} span {:.0} s: entries A {} B {}; re-step {} frames ({below} below Cb, from the keyframe) in \
             {:.0} ms ({:.2} ms/frame, rho {:.1}); {conv}; A alone [{}]; A during [{}]; corrections {}; overwrites {} \
             (of {})",
            self.gate.balls,
            self.gate.span as f64 / f64::from(HZ),
            self.a_entries,
            self.b_entries,
            self.replay_frames,
            self.replay_wall.as_secs_f64() * 1e3,
            self.replay_ms_per_frame(),
            self.rho(),
            self.a_alone.line(),
            self.a_during.line(),
            c(&self.corrections),
            c(&self.overwrites),
            self.objects,
        )
    }
}

/// pe-agree's tie rank within a frame (agreement.md 3, D37: `H(line id, frame, author key)`; pe-agree's node.rs
/// `tie_rank`), so the merged order is the agreed one: stamp, tie rank, author, chain, seq.
fn tie_rank(line: LineId, frame: Frame, author: &MemberKey) -> u64 {
    let mut h = Hasher::new("pe.log.tie");
    h.write_hash(&line.0).write_u64(frame.0).write(&author.0);
    let b = h.finish().0;
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// A final entry source: every entry at its stamp, in the agreed order, every frame kept. Its answer for a cut is
/// `entries_at`'s, since every entry it holds is final and none is void. [`Static::add`] brings in a package: the
/// entries join their frames, and the source reports a change from the given frame.
struct Static {
    at: BTreeMap<u64, Vec<SignedEntry>>,
    changed: Option<Frame>,
}

impl Static {
    fn of<'a>(entries: impl IntoIterator<Item = &'a SignedEntry>) -> Static {
        let mut s = Static { at: BTreeMap::new(), changed: None };
        s.insert(entries);
        s
    }

    fn insert<'a>(&mut self, entries: impl IntoIterator<Item = &'a SignedEntry>) {
        for e in entries {
            self.at.entry(e.entry.stamp.0).or_default().push(e.clone());
        }
        for list in self.at.values_mut() {
            list.sort_by_key(|e| {
                let x = &e.entry;
                (tie_rank(x.line, x.stamp, &x.author), x.author.0, x.chain, x.seq)
            });
        }
    }

    /// B's package arrives: its entries join the source, and the replica re-steps from `from`.
    fn add<'a>(&mut self, entries: impl IntoIterator<Item = &'a SignedEntry>, from: Frame) {
        self.insert(entries);
        self.changed = Some(from);
    }
}

impl EntrySource for Static {
    fn final_cut(&self) -> Option<&FinalCut> {
        None
    }
    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        Some(self.at.get(&frame.0).map_or(&[], Vec::as_slice))
    }
    fn entries_for_cut(&self, _cut: &Cut, frame: Frame) -> Option<Vec<SignedEntry>> {
        self.entries_at(frame).map(<[SignedEntry]>::to_vec)
    }
    fn kept_from(&self) -> Frame {
        Frame(0)
    }
    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}
    fn take_changed_from(&mut self) -> Option<Frame> {
        self.changed.take()
    }
}

type Rep = ReplicaImpl<PeWorld>;

/// The seeded actions of both members, signed on their own chains, in stamp order per chain.
struct Actions {
    /// Node 1 (A) and node 2 (B), each `(stamp, entry)`.
    by: [Vec<SignedEntry>; 2],
}

impl Actions {
    fn new(world: &World, keys: &TestKeys, balls: &[ObjectId], seed: u64, every: Nanos, to: u64) -> Option<Actions> {
        let frame_ns = SEC / u64::from(HZ);
        let end = to * frame_ns;
        let sorted = keys.sorted_keys();
        let mut timed: [Vec<(Nanos, Class, Vec<u8>)>; 2] = [Vec::new(), Vec::new()];
        for p in ballpit::seeded_pushes(seed, 2, balls.len(), 250 * MS, end, every * 2) {
            let ball = *balls.get(p.ball)?;
            timed[p.member as usize - 1].push((p.at, Class::Edit, ballpit::push_payload(ball, p.to)));
        }
        for p in ballpit::seeded_hand_pushes(seed, 2, balls.len(), 250 * MS + every, end, every * 2) {
            let k = p.member as usize - 1;
            let ball = *balls.get(p.ball)?;
            let hand = ballpit::first_hand(world, *sorted.get(k)?)?;
            let to = ballpit::hand_target(world, ball)?;
            timed[k].push((p.at.saturating_sub(ballpit::HAND_LEAD), Class::Input, ballpit::hand_payload(hand, to)));
            timed[k].push((p.at, Class::Input, ballpit::shove_payload(ball, p.impulse)));
        }
        let mut by: [Vec<SignedEntry>; 2] = [Vec::new(), Vec::new()];
        for (k, list) in timed.iter_mut().enumerate() {
            list.sort_by_key(|(at, ..)| *at);
            let mut b = EntryBuilder::new(keys, keys.sorted_keys().iter().position(|&x| x == sorted[k])?);
            for (at, class, payload) in list.drain(..) {
                by[k].push(b.next(class, Frame(at / frame_ns), payload.into()));
            }
        }
        Some(Actions { by })
    }

    /// Member `k`'s entries stamped in `from..to`.
    fn of(&self, k: usize, from: u64, to: u64) -> impl Iterator<Item = &SignedEntry> {
        self.by[k].iter().filter(move |e| (from..to).contains(&e.entry.stamp.0))
    }
}

/// Steps `r` to `to` (exclusive) with `src`, whatever it takes; returns the frames re-stepped and stepped.
fn step_to(r: &mut Rep, to: u64, src: &mut Static) -> (u64, u64) {
    let (mut resim, mut stepped) = (0, 0);
    for _ in 0..100_000 {
        let a = r.advance(Frame(to), src);
        resim += a.resimulated;
        stepped += a.stepped;
        if a.caught_up {
            break;
        }
    }
    (resim, stepped)
}

/// What one side looks like at `F_M`: each ball's hash, and its position in the view there.
struct Seen {
    hashes: Vec<(ObjectId, Option<Hash>)>,
    pos: BTreeMap<ObjectId, Pos>,
}

/// `r`'s balls at frame `at`, which it has just stepped to.
fn seen_at(r: &mut Rep, at: u64, src: &mut Static, ids: &[ObjectId]) -> Option<Seen> {
    let cut = Cut { at: CutRef { line: LineId::origin(), height: at, frame: Frame(at) }, heads: Vec::new() };
    let root = r.root_at_cut(&cut, src).ok()?;
    let hashes = r.tree(&root)?.object_hashes(ids);
    let view = r.view();
    if view.frame != Frame(at) {
        return None;
    }
    let pos = view.objects.iter().filter(|o| ids.contains(&o.id)).map(|o| (o.id, o.pos)).collect();
    Some(Seen { hashes, pos })
}

fn changed(a: &Seen, b: &Seen) -> Changed {
    let by_hash = a.hashes.iter().zip(&b.hashes).filter(|(x, y)| x != y).count();
    let mut c = Changed { by_hash, ..Changed::default() };
    for (id, p) in &a.pos {
        let Some(q) = b.pos.get(id) else {
            c.visible += 1;
            continue;
        };
        let d = (*q - *p).0;
        let m = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if m > VISIBLE_M {
            c.visible += 1;
        }
        c.max_m = c.max_m.max(m);
    }
    c
}

/// The stack a thread stepping a ball-pit replica needs (its script runner's).
fn stack() -> usize {
    pe_replica::logic::LogicRunner::with_defaults().map_or(64 << 20, |r| r.thread_stack()).max(8 << 20)
}

/// Runs one gate run. `None` if the world, a replica or a root can't be made.
pub fn run(g: &Gate) -> Option<GateReport> {
    let keys = ballpit::keys(2);
    let world = ballpit::genesis(g.seed, g.balls, &keys)?;
    let balls: Vec<ObjectId> = (0..g.balls)
        .map(|i| preset::find(&world, &format!("{}{i}", preset::names::BALL_PREFIX)))
        .collect::<Option<_>>()?;
    let sorted = keys.sorted_keys();
    let (base, f_m) = (g.base, g.f_m());
    // A's actions run on past F_M for as long as its loop may.
    let horizon = f_m + g.live + g.chase_frames + 600;
    let acts = Actions::new(&world, &keys, &balls, g.seed, g.every, horizon)?;
    let shared = || acts.of(0, 0, base).chain(acts.of(1, 0, base));
    let a_src = Static::of(shared().chain(acts.of(0, base, horizon)));
    let b_src = Static::of(shared().chain(acts.of(1, base, f_m)));
    let m_src = Static::of(shared().chain(acts.of(0, base, horizon)));
    let r_src = Static::of(shared().chain(acts.of(0, base, f_m)).chain(acts.of(1, base, f_m)));
    let package: Vec<SignedEntry> = acts.of(1, base, f_m).cloned().collect();
    let (a_entries, b_entries) = (acts.of(0, base, f_m).count(), package.len());
    let (go, done, a_stopped) = (AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false));
    let a_frame = AtomicU64::new(0);
    let (go, done, a_stopped, a_frame) = (&go, &done, &a_stopped, &a_frame);
    let (balls, world, package) = (&balls, &world, &package);

    let stack = stack();
    std::thread::scope(|s| {
        // Set-up, untimed: A's live replica, B's own and the merge's (a copy of A's), all at F_M; and the merged
        // reference at F_M, for the comparisons.
        let setup = |key, mut src: Static, seen: bool| {
            move || {
                let mut r = ballpit::replica(world.clone(), key)?;
                r.set_budget(Duration::from_secs(3_600));
                step_to(&mut r, f_m, &mut src);
                let seen = if seen { Some(seen_at(&mut r, f_m, &mut src, balls)?) } else { None };
                Some((r, src, seen))
            }
        };
        let a = std::thread::Builder::new().stack_size(stack).spawn_scoped(s, setup(sorted[0], a_src, true));
        let b = std::thread::Builder::new().stack_size(stack).spawn_scoped(s, setup(sorted[1], b_src, true));
        let m = std::thread::Builder::new().stack_size(stack).spawn_scoped(s, setup(sorted[0], m_src, false));
        let r = std::thread::Builder::new().stack_size(stack).spawn_scoped(s, setup(sorted[0], r_src, true));
        let (mut a_rep, mut a_src, a_seen) = a.ok()?.join().ok()??;
        let (_, _, b_seen) = b.ok()?.join().ok()??;
        let (mut m_rep, mut m_src, _) = m.ok()?.join().ok()??;
        let (_, _, r_seen) = r.ok()?.join().ok()??;
        let (a_seen, b_seen, r_seen) = (a_seen?, b_seen?, r_seen?);

        // The timed part: A's loop alone for `live` frames; then B's package arrives at the merge replica, which
        // re-steps from Cb and chases A's frame, while A's loop goes on until the chase ends and `live` frames more.
        let merge = std::thread::Builder::new().stack_size(stack).spawn_scoped(s, move || {
            while !go.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            let started = Instant::now();
            m_src.add(package.iter(), Frame(base));
            let (resim, _) = step_to(&mut m_rep, f_m, &mut m_src);
            let replay_wall = started.elapsed();
            let mut converged = None;
            loop {
                let target = a_frame.load(Ordering::SeqCst);
                if m_rep.frame().0 >= target {
                    converged = Some(started.elapsed());
                    break;
                }
                if a_stopped.load(Ordering::SeqCst) {
                    break;
                }
                m_rep.advance(Frame(target), &mut m_src);
            }
            done.store(true, Ordering::SeqCst);
            (resim, replay_wall, converged, m_rep.frame().0.saturating_sub(f_m))
        });
        let frame = Duration::from_nanos(SEC / u64::from(HZ));
        let (live, paced, cap) = (g.live, g.paced, g.chase_frames);
        let live = std::thread::Builder::new().stack_size(stack).spawn_scoped(s, move || {
            let (mut alone, mut during) = (Vec::new(), Vec::new());
            let mut f = a_rep.frame().0;
            a_frame.store(f, Ordering::SeqCst);
            let t0 = Instant::now();
            let mut k = 0u32;
            loop {
                if paced && let Some(wait) = (frame * k).checked_sub(t0.elapsed()) {
                    std::thread::sleep(wait);
                }
                let t = Instant::now();
                a_rep.advance(Frame(f + 1), &mut a_src);
                let ns = u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX);
                f += 1;
                k += 1;
                a_frame.store(f, Ordering::SeqCst);
                if (alone.len() as u64) < live {
                    alone.push(ns);
                    if alone.len() as u64 == live {
                        go.store(true, Ordering::SeqCst);
                    }
                    continue;
                }
                during.push(ns);
                if done.load(Ordering::SeqCst) && during.len() as u64 >= live {
                    break;
                }
                if during.len() as u64 >= cap || f + 1 >= horizon {
                    // The chase's bound: stop, and release a replay still parked.
                    a_stopped.store(true, Ordering::SeqCst);
                    go.store(true, Ordering::SeqCst);
                    if done.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
            (Ticks::of(alone), Ticks::of(during))
        });
        let (a_alone, a_during) = live.ok()?.join().ok()?;
        let (replay_frames, replay_wall, converged, chase_frames) = merge.ok()?.join().ok()?;
        Some(GateReport {
            gate: g.clone(),
            a_entries,
            b_entries,
            replay_frames,
            replay_wall,
            converged,
            chase_frames,
            a_alone,
            a_during,
            objects: balls.len(),
            corrections: changed(&a_seen, &r_seen),
            overwrites: changed(&b_seen, &r_seen),
        })
    })
}

/// The lagging voter's second simulation (r2, Live option 3): one replica stepped frame by frame against two stepped
/// in turn over the same frames, unpaced, on one thread (a member's frame loop is one thread).
#[derive(Clone, Debug)]
pub struct LaggingVoter {
    pub balls: usize,
    pub frames: u64,
    pub one: Duration,
    pub two: Duration,
}

impl LaggingVoter {
    /// The cost ratio of two simulations to one.
    pub fn k(&self) -> f64 {
        self.two.as_secs_f64() / self.one.as_secs_f64().max(1e-9)
    }

    /// Frames stepped per second of wall time, with one simulation and with two.
    pub fn fps(&self) -> (f64, f64) {
        let f = self.frames as f64;
        (f / self.one.as_secs_f64().max(1e-9), f / self.two.as_secs_f64().max(1e-9))
    }

    /// How many seconds of lag a member gains per second of wall time when it can step only `fps` frames a second.
    pub fn lag_growth(fps: f64) -> f64 {
        (1.0 - fps / f64::from(HZ)).max(0.0)
    }

    pub fn line(&self) -> String {
        let (one, two) = self.fps();
        format!(
            "lagging voter balls {}: {} frames, one sim {:.2} ms/frame ({one:.0} frames/s, lag +{:.2} s/s), two sims \
             {:.2} ms/frame ({two:.0} frames/s, lag +{:.2} s/s), k {:.2}",
            self.balls,
            self.frames,
            self.one.as_secs_f64() * 1e3 / self.frames.max(1) as f64,
            LaggingVoter::lag_growth(one),
            self.two.as_secs_f64() * 1e3 / self.frames.max(1) as f64,
            LaggingVoter::lag_growth(two),
            self.k(),
        )
    }
}

/// Measures [`LaggingVoter`] over `frames` frames after `base` frames of the gate's seeded actions (B's share and the
/// shared history). The second simulation stands in for option 3's canonical replay, without its rollbacks [I].
pub fn lagging_voter(seed: u64, balls: usize, base: u64, frames: u64) -> Option<LaggingVoter> {
    let keys = ballpit::keys(2);
    let world = ballpit::genesis(seed, balls, &keys)?;
    let ids: Vec<ObjectId> = (0..balls)
        .map(|i| preset::find(&world, &format!("{}{i}", preset::names::BALL_PREFIX)))
        .collect::<Option<_>>()?;
    let sorted = keys.sorted_keys();
    let to = base + frames;
    let acts = Actions::new(&world, &keys, &ids, seed, 125 * MS, to)?;
    let source = || Static::of(acts.of(0, 0, to).chain(acts.of(1, 0, to)));
    let stack = stack();
    std::thread::scope(|s| {
        let t = std::thread::Builder::new().stack_size(stack).spawn_scoped(s, || {
            let mut src = source();
            let mut reps = Vec::new();
            for _ in 0..3 {
                let mut r = ballpit::replica(world.clone(), sorted[1])?;
                r.set_budget(Duration::from_secs(3_600));
                step_to(&mut r, base, &mut src);
                reps.push(r);
            }
            let started = Instant::now();
            for f in base..to {
                reps[0].advance(Frame(f + 1), &mut src);
            }
            let one = started.elapsed();
            let started = Instant::now();
            for f in base..to {
                reps[1].advance(Frame(f + 1), &mut src);
                reps[2].advance(Frame(f + 1), &mut src);
            }
            let two = started.elapsed();
            Some(LaggingVoter { balls, frames, one, two })
        });
        t.ok()?.join().ok()?
    })
}
