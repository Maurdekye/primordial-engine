//! render.md 14, test 6: pe-render's presenter against pe-harness's ball-pit run (@coordinator-opus m30db4de23a70,
//! option (b)). Each member's recorded frame views are published into a `FrameSlot` at the host times the member
//! read them. A `Presenter` takes them in batches at 90 Hz (render.md 3), through a desktop camera at the member's
//! place by the pit. Per (seed, delay):
//!
//! - **The user's target** (meef4f248923b, answer 8): no correction larger than a hand's width shown in view without a
//!   cue. Checked twice: the display layer's own `cueless_large_corrections`, and independently on the drawn output.
//!   No object in view may move more than `E_cue` (10 cm) in one display frame while it carries no cue. A falling ball
//!   moves at most about 4.7 cm in a 90 Hz frame (0.9 m drop), so only a pop exceeds it. A move while the drawn time
//!   crosses a jump the member's own views show between two frames, on an object not marked `CORRECTED`, is the
//!   simulation's own jump (a push's edit), drawn as it happened, and is counted apart (`simJ`): no prediction was
//!   corrected. Such view jumps on balls nobody pushed are counted too (`other`), since they would be a missing flag.
//!   The member's own objects (`OWN_HAND`, `HELD_LOCAL`) are left out of both, as they are drawn exact: the harness's
//!   hand pushes move each pusher's first hand to above its ball in one input, a jump of up to a few metres.
//! - **D4:** `own_error` stays 0. Each member's views mark its own hands `OWN_HAND`.
//! - **Both drawn-time rules,** X8 (a) (the default) and (b), each also with a reader stall of 300 ms at 3 s, past the
//!   slot's 250 ms span, so the presenter meets a gap (render.md 3; @coordinator-opus's ruling m7fa4df66d2fe).
//! - **The counters of section 13** are printed per delay, measured, for the report. Their maxima (`miss max`,
//!   `visEr max`) come from the run's deliberate teleports, not from prediction: a push edit's lift of a ball (about
//!   2.6 m) and another member's hand moved above its ball (up to about 3.5 m), blended in view (measured 2026-11-13).
//!   Quote the means and counts, or say so.
//!
//! What the run can't show yet, stated so that nobody reads more into it:
//! - **Pushes are edits** that lift a ball from the pile to 0.9 m above it, up to 2.7 m away (pe_harness::ballpit).
//!   Under X8 (a) the lift is a change of prediction like any other, so the layer blends it (over `T_fast`) and cues
//!   it, for the pusher too. Under X8 (b) a lift not marked `CORRECTED` is drawn as the jump it is. pe-harness's
//!   hand pushes (c47991be) run alongside them: an impulse on a ball, after the pusher's hand moves above it.
//! - **Notices aren't checked here:** present.rs's and notices.rs's tests cover them over a batch and a gap.
//!
//! The quick test runs 1 seed × 5 delays in release (2 in a debug build). The full grid, 20 seeds × 5 delays, is
//! `--ignored`; run it in release. Effects played and stopped at gaps are printed (`eff`, `effSt`); a played effect
//! final before a stall plays on after it (display_layer.rs's gap tests).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use glam::{DQuat, DVec3};
use pe_harness::ballpit::{self, BallPit};
use pe_harness::clock::{MS, Nanos, SEC};
use pe_render::LocalPresentation;
use pe_render::camera::{Camera, DepthRange, Fov};
use pe_render::display::{DisplayCounters, DisplaySettings};
use pe_render::drawn_time::DrawnTime;
use pe_render::effects::EffectSettings;
use pe_render::notices::{NoticeSettings, Notices};
use pe_render::pose::{WorldPose, offset_vec};
use pe_render::present::{FrameSlot, Presenter};
use pe_render::scene::Environment;
use pe_seams::{FrameView, HostTime, MemberKey, ObjectId, Offset, Pos, ViewFlags};

/// One-way link delays, ms: good links (the user's target holds "on good links").
const DELAYS_MS: [u64; 5] = [10, 25, 50, 75, 100];
/// The quick form's delays: all five in release; in a debug build (the gate's `cargo test`), the two ends, where the
/// slot's losses first showed (about 25 s instead of 60).
const QUICK_DELAYS_MS: &[u64] = if cfg!(debug_assertions) { &[10, 100] } else { &DELAYS_MS };
/// The display rate: a desktop or headset refresh that isn't a multiple of the 60 Hz simulation.
const DISPLAY_HZ: u64 = 90;
/// The run: 3 members, 48 balls, 6 s, a push every 400 ms (harness-eng's test sizes, a little longer).
const MEMBERS: u32 = 3;
const BALLS: usize = 48;
const END: Nanos = 6 * SEC;
const EVERY: Nanos = 400 * MS;
/// The stall: the reader takes nothing for 300 ms from 3 s.
const STALL_AT: Nanos = 3 * SEC;
const STALL_FOR: Nanos = 300 * MS;

fn at(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).expect("in range")
}

/// Member `n`'s camera: 1.6 m up, a little behind its hands' spawn point (pe_harness::ballpit's SPAWNS), looking at the
/// pit's middle, 90° vertical field of view, 16:9.
fn camera(n: u32) -> Camera {
    const SPAWNS: [[f64; 3]; 4] = [[1.8, 0.0, 0.0], [-1.8, 0.0, 0.0], [0.0, 0.0, 1.8], [0.0, 0.0, -1.8]];
    let s = SPAWNS[(n as usize - 1) % SPAWNS.len()];
    let eye = DVec3::new(s[0] * 1.3, 1.6, s[2] * 1.3);
    let look = (DVec3::new(0.0, 0.3, 0.0) - eye).normalize();
    let rot = DQuat::from_rotation_arc(DVec3::NEG_Z, look);
    let pose = WorldPose { pos: at(eye.x, eye.y, eye.z), rot };
    Camera::placed(pose, Fov::symmetric(90f32.to_radians(), 16.0 / 9.0), DepthRange::DESKTOP)
}

/// How a member's views are displayed: the drawn-time rule, and whether the reader stalls.
#[derive(Clone, Copy)]
struct Mode {
    name: &'static str,
    settings: DisplaySettings,
    stall: bool,
}

fn modes() -> [Mode; 4] {
    let a = DisplaySettings::default();
    let b = DisplaySettings {
        drawn_time: DrawnTime::InterpolateLate { d_render: Duration::from_millis(50) },
        ..Default::default()
    };
    [
        Mode { name: "X8 (a), the default", settings: a, stall: false },
        Mode { name: "X8 (b)", settings: b, stall: false },
        Mode { name: "X8 (a), reader stalled 300 ms at 3 s", settings: a, stall: true },
        Mode { name: "X8 (b), reader stalled 300 ms at 3 s", settings: b, stall: true },
    ]
}

/// One member's display run.
#[derive(Clone, Copy, Debug, Default)]
struct MemberResult {
    counters: DisplayCounters,
    /// Display frames in which an object in view moved more than `E_cue` with no cue, `sim_jumps` excepted: the
    /// user's target, judged on the drawn output.
    pops: u64,
    /// The largest one-frame move of an object in view without a cue, metres, `sim_jumps` excepted.
    largest_uncued_step: f64,
    /// Uncued drawn moves above `E_cue` explained by a jump in the views themselves (a push's edit).
    sim_jumps: u64,
    /// Jumps above `E_cue` between consecutive views, not marked `CORRECTED`, on objects nobody pushed.
    other_view_jumps: u64,
    /// Gaps the presenter met, the views lost to them, and those of them that marked an object `CORRECTED`.
    gaps: u64,
    lost_views: u64,
    lost_corrected: u64,
    /// Effects played, and stopped at gaps (`EffectGate::gap`).
    effects_played: u64,
    gap_stops: u64,
}

/// A jump in a member's own views: `id` moved more than `e` (beyond its velocity's reach) between the views of
/// simulated times `t0` and `t1`, without `CORRECTED`.
struct ViewJump {
    id: ObjectId,
    t0: HostTime,
    t1: HostTime,
}

fn view_jumps(views: &[(Nanos, Arc<FrameView>)], e: f64) -> Vec<ViewJump> {
    let mut out = Vec::new();
    for w in views.windows(2) {
        let (a, b) = (&w[0].1, &w[1].1);
        if b.frame <= a.frame {
            continue;
        }
        let dt = b.sim_time.secs_since(a.sim_time);
        for o in &b.objects {
            let Some(p) = a.object(o.id) else { continue };
            if o.flags.contains(ViewFlags::CORRECTED) || is_own(b, o.id) {
                continue;
            }
            let v = p.linear_velocity;
            let reach = ((v[0] * v[0] + v[1] * v[1] + v[2] * v[2]) as f64).sqrt() * dt + 9.81 * dt * dt;
            if offset_vec(o.pos - p.pos).length() > e + reach {
                out.push(ViewJump { id: o.id, t0: a.sim_time, t1: b.sim_time });
            }
        }
    }
    out
}

fn is_own(v: &FrameView, id: ObjectId) -> bool {
    v.object(id).is_some_and(|o| o.flags.contains(ViewFlags::OWN_HAND) || o.flags.contains(ViewFlags::HELD_LOCAL))
}

/// Publishes `views` (read times, views) into a slot as the member read them, and presents batches at 90 Hz from the
/// first view's read time to `end`.
fn display_run(
    mode: Mode,
    views: &[(Nanos, Arc<FrameView>)],
    camera: &Camera,
    me: MemberKey,
    end: Nanos,
    pushed: &[ObjectId],
) -> MemberResult {
    let e_cue = mode.settings.e_cue;
    let mut presenter = Presenter::new(mode.settings, EffectSettings::default(), Environment::default(), 60);
    presenter.set_notices(Notices::new(NoticeSettings::default(), me, 60));
    let slot = FrameSlot::new();
    let volume = camera.volume();
    let period = SEC / DISPLAY_HZ;
    let mut out = MemberResult::default();
    let jumps = view_jumps(views, e_cue);
    out.other_view_jumps = jumps.iter().filter(|j| !pushed.contains(&j.id)).count() as u64;
    let slack = 2 * period as i64;
    let explained =
        |id: ObjectId, t: HostTime| jumps.iter().any(|j| j.id == id && t.0 >= j.t0.0 - slack && t.0 <= j.t1.0 + slack);
    let mut last: BTreeMap<ObjectId, Pos> = BTreeMap::new();
    let Some(&(start, _)) = views.first() else { return out };
    let (mut published, mut expected) = (0usize, 0u64);
    let mut t = start;
    while t < end {
        while published < views.len() && views[published].0 <= t {
            slot.publish(views[published].1.clone());
            published += 1;
        }
        if mode.stall && (STALL_AT..STALL_AT + STALL_FOR).contains(&t) {
            t += period;
            continue;
        }
        let batch = slot.take();
        if !batch.views.is_empty() {
            for (_, v) in &views[expected as usize..batch.first as usize] {
                out.lost_views += 1;
                if v.objects.iter().any(|o| o.flags.contains(ViewFlags::CORRECTED)) {
                    out.lost_corrected += 1;
                }
            }
            expected = batch.end();
        }
        // The desktop's display time: now plus one refresh (lib.rs, LocalPresentation).
        let display_time = HostTime((t + period) as i64);
        let local = LocalPresentation { display_time, hands: [None, None] };
        let Some(frame) = presenter.frame_batch(&batch, &local, std::slice::from_ref(camera)) else {
            t += period;
            continue;
        };
        let newest = &views[published - 1].1;
        let radius: BTreeMap<ObjectId, f64> = newest.objects.iter().map(|o| (o.id, o.radius as f64)).collect();
        for d in &frame.display.objects {
            let r = radius.get(&d.id).copied().unwrap_or(0.0);
            if let Some(prev) = last.insert(d.id, d.pose.pos) {
                let step = offset_vec(d.pose.pos - prev).length();
                let in_view = volume.touches_sphere(prev, r) || volume.touches_sphere(d.pose.pos, r);
                if in_view && d.cue.is_none() && !is_own(newest, d.id) {
                    if step > e_cue && explained(d.id, frame.display.t_draw) {
                        out.sim_jumps += 1;
                    } else {
                        out.largest_uncued_step = out.largest_uncued_step.max(step);
                        if step > e_cue {
                            out.pops += 1;
                        }
                    }
                }
            }
        }
        t += period;
    }
    out.counters = presenter.counters().run().display;
    out.gaps = presenter.gaps();
    let e = presenter.effects().counters();
    (out.effects_played, out.gap_stops) = (e.played, e.gap_stops);
    out
}

/// One (seed, delay) cell: the run once, then each member's display run in each mode.
fn cell(seed: u64, delay_ms: u64, modes: &[Mode]) -> Vec<Vec<MemberResult>> {
    let cfg = BallPit::new(seed, delay_ms * MS, MEMBERS, BALLS, END, EVERY);
    let run = ballpit::run(&cfg).expect("the ball-pit run builds");
    let vs = run.violations();
    assert!(vs.is_empty(), "seed {seed}, {delay_ms} ms: the run broke a property: {vs:#?}");
    let keys = ballpit::keys(MEMBERS).sorted_keys();
    let pushed: Vec<ObjectId> = cfg.pushes.iter().map(|p| run.balls[p.ball]).collect();
    modes
        .iter()
        .map(|&m| {
            run.views
                .iter()
                .map(|(&n, views)| display_run(m, views, &camera(n), keys[n as usize - 1], END, &pushed))
                .collect()
        })
        .collect()
}

/// Sums over members and seeds, per delay.
#[derive(Default)]
struct Row {
    runs: u64,
    c: DisplayCounters,
    pops: u64,
    largest_uncued_step: f64,
    sim_jumps: u64,
    other_view_jumps: u64,
    gaps: u64,
    lost_views: u64,
    lost_corrected: u64,
    effects_played: u64,
    gap_stops: u64,
}

impl Row {
    fn add(&mut self, m: &MemberResult) {
        let (c, x) = (&mut self.c, &m.counters);
        c.corrections += x.corrections;
        c.extrapolation_misses += x.extrapolation_misses;
        c.extrapolation_error_max = c.extrapolation_error_max.max(x.extrapolation_error_max);
        c.extrapolation_error_sum += x.extrapolation_error_sum;
        c.hold_ends += x.hold_ends;
        c.snaps_in_view += x.snaps_in_view;
        c.jumps_out_of_view += x.jumps_out_of_view;
        c.cues += x.cues;
        c.cueless_large_corrections += x.cueless_large_corrections;
        c.own_error = c.own_error.max(x.own_error);
        c.visible_error_max = c.visible_error_max.max(x.visible_error_max);
        c.visible_error_sum += x.visible_error_sum;
        c.visible_samples += x.visible_samples;
        c.display_frames += x.display_frames;
        c.views_taken += x.views_taken;
        c.gaps += x.gaps;
        self.pops += m.pops;
        self.largest_uncued_step = self.largest_uncued_step.max(m.largest_uncued_step);
        self.sim_jumps += m.sim_jumps;
        self.other_view_jumps += m.other_view_jumps;
        self.gaps += m.gaps;
        self.lost_views += m.lost_views;
        self.lost_corrected += m.lost_corrected;
        self.effects_played += m.effects_played;
        self.gap_stops += m.gap_stops;
        self.runs += 1;
    }

    fn print(&self, label: &str) {
        let c = &self.c;
        let minutes = c.display_frames as f64 / DISPLAY_HZ as f64 / 60.0;
        let per_min = |n: u64| n as f64 / minutes.max(1e-9);
        let mean = |s: f64, n: u64| if n == 0 { 0.0 } else { s / n as f64 };
        println!(
            "{label:>8} | {:>4} | {:>6.1} {:>7.1} | {:>5.1} {:>6.1} | {:>6.1} {:>5} {:>4} {:>4} | {:>5.2} {:>6.1} | {:>4} {:>5.1} {:>4} {:>5} | {:>3} | {:>4} {:>4} {:>5} | {:>5} {:>5}",
            self.runs,
            per_min(c.corrections),
            per_min(c.extrapolation_misses),
            mean(c.extrapolation_error_sum, c.extrapolation_misses) * 100.0,
            c.extrapolation_error_max * 100.0,
            per_min(c.cues),
            c.cueless_large_corrections,
            c.snaps_in_view,
            c.jumps_out_of_view,
            mean(c.visible_error_sum, c.visible_samples) * 100.0,
            c.visible_error_max * 100.0,
            self.pops,
            self.largest_uncued_step * 100.0,
            self.sim_jumps,
            self.other_view_jumps,
            c.own_error,
            self.gaps,
            self.lost_views,
            self.lost_corrected,
            self.effects_played,
            self.gap_stops,
        );
    }
}

fn header(title: &str) {
    println!("\n{title}");
    println!(
        "{:>8} | {:>4} | {:>6} {:>7} | {:>5} {:>6} | {:>6} {:>5} {:>4} {:>4} | {:>5} {:>6} | {:>4} {:>5} {:>4} {:>5} | own | {:>4} {:>4} {:>5} | {:>5} {:>5}",
        "delay",
        "runs",
        "corr/m",
        "miss/m",
        "miss",
        "max",
        "cues/m",
        "noCue",
        "snap",
        "jump",
        "visEr",
        "max",
        "pops",
        "step",
        "simJ",
        "other",
        "gaps",
        "lost",
        "lostC",
        "eff",
        "effSt"
    );
    println!(
        "{:>8} | {:>4} | {:>14} | {:>5} {:>6} | {:>22} | {:>5} {:>6} | {:>4} {:>5} {:>4} {:>5} | {:>3} |",
        "", "", "", "cm", "cm", "", "cm", "cm", "", "cm", "", "", ""
    );
}

/// Runs `seeds` × `DELAYS_MS`, each cell once, displayed in every mode; prints a table per mode; returns the rows
/// per mode and delay.
fn grid(seeds: std::ops::Range<u64>, delays: &[u64]) -> Vec<(Mode, Vec<(u64, Row)>)> {
    let modes = modes();
    let mut rows: Vec<(Mode, Vec<(u64, Row)>)> = modes.iter().map(|&m| (m, Vec::new())).collect();
    for &delay in delays {
        let mut row: Vec<Row> = modes.iter().map(|_| Row::default()).collect();
        for seed in seeds.clone() {
            for (k, members) in cell(seed, delay, &modes).iter().enumerate() {
                for m in members {
                    row[k].add(m);
                }
            }
        }
        for (k, r) in row.into_iter().enumerate() {
            rows[k].1.push((delay, r));
        }
    }
    let n = seeds.end - seeds.start;
    for (mode, rows) in &rows {
        header(&format!("{}, {n} seed(s) (measured; per delay, summed over seeds and members)", mode.name));
        for (delay, r) in rows {
            r.print(&format!("{delay} ms"));
        }
    }
    rows
}

/// The acceptance in every mode: the user's target on both checks, and D4; a stalled run met its gap.
fn accept(rows: &[(Mode, Vec<(u64, Row)>)]) {
    for (mode, rows) in rows {
        let m = mode.name;
        for (delay, r) in rows {
            assert!(r.c.views_taken > 0 && r.c.visible_samples > 0, "{m}, {delay} ms: nothing was displayed");
            assert_eq!(r.c.cueless_large_corrections, 0, "{m}, {delay} ms: corrections above E_cue in view, no cue");
            assert_eq!(r.pops, 0, "{m}, {delay} ms: drawn objects in view moved more than E_cue in a frame, no cue");
            assert_eq!(r.c.snaps_in_view, 0, "{m}, {delay} ms: unblended corrections in view");
            assert_eq!(r.c.own_error, 0.0, "{m}, {delay} ms: an own object was corrected");
            assert_eq!(r.other_view_jumps, 0, "{m}, {delay} ms: a view jump on an unpushed ball without CORRECTED");
            if mode.stall {
                assert_eq!(r.gaps, r.runs, "{m}, {delay} ms: one gap per stalled run");
            } else {
                assert_eq!(r.gaps + r.lost_views, 0, "{m}, {delay} ms: views lost without a stall");
            }
        }
        let changes: u64 = rows.iter().map(|(_, r)| r.c.corrections + r.c.extrapolation_misses).sum();
        assert!(changes > 0, "{m}: no change of prediction at any delay: the run tested nothing");
    }
}

/// The quick form, on the gate: one seed at every delay.
#[test]
fn the_presenter_meets_the_users_target_on_the_ball_pit() {
    accept(&grid(1..2, QUICK_DELAYS_MS));
}

/// The plan's form: 20 seeds × 5 delays. Run in release: `cargo test -p pe-render --release --test ballpit -- --ignored
/// --nocapture`.
#[test]
#[ignore]
fn the_presenter_meets_the_users_target_over_20_seeds_and_5_delays() {
    accept(&grid(1..21, &DELAYS_MS));
}
