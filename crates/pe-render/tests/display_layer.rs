//! render.md 14, tests 1-3: the display layer (pure CPU), effects, and rates (D8) in a fake clock.

use std::sync::Arc;
use std::time::Duration;

use glam::DQuat;
use pe_render::camera::{Camera, DepthRange, Fov, ViewVolume};
use pe_render::display::{Controls, DisplayLayer, DisplaySettings, FrameInput};
use pe_render::drawn_time::DrawnTime;
use pe_render::effects::{EffectCommand, EffectGate, EffectSettings};
use pe_render::pose::{WorldPose, offset_vec};
use pe_render::{DisplayHand, LocalPresentation};
use pe_seams::{
    DomainId, Effect, EffectCause, EffectKey, EffectKind, Frame, FrameView, Hand, Holder, HostTime, MemberKey,
    ObjectClass, ObjectId, Offset, Pos, RigPose, RigTransform, Shape, ViewCounters, ViewFlags, ViewObject,
};

const NS: i64 = 1_000_000_000;

fn at(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).unwrap()
}

fn ball(id: u128, p: Pos, v: [f32; 3]) -> ViewObject {
    ViewObject {
        id: ObjectId(id),
        class: ObjectClass::World,
        pos: p,
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0; 3],
        linear_velocity: v,
        angular_velocity: [0.0; 3],
        shape: Some(Shape::Sphere { radius: 0.05 }),
        appearance: None,
        light: None,
        radius: 0.05,
        hand_of: None,
        held_by: None,
        flags: ViewFlags::default(),
        version: None,
    }
}

fn view(frame: u64, sim_time: HostTime, mut objects: Vec<ViewObject>) -> Arc<FrameView> {
    objects.sort_by_key(|o| o.id);
    Arc::new(FrameView {
        frame: Frame(frame),
        domain: DomainId(0),
        sim_time,
        final_frame: None,
        local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
        objects,
        effects: vec![],
        retracted: vec![],
        counters: ViewCounters::default(),
        changes: vec![],
    })
}

/// The frame's host time at `hz`, rounded to the nanosecond as a clock would give it.
fn frame_time(f: u64, hz: u32) -> HostTime {
    HostTime(((f as i128 * NS as i128) / hz as i128) as i64)
}

/// One camera at the origin looking down −Z.
fn volumes() -> Vec<ViewVolume> {
    vec![
        Camera::placed(
            WorldPose { pos: Pos::ORIGIN, rot: DQuat::IDENTITY },
            Fov::symmetric(1.2, 1.6),
            DepthRange::DESKTOP,
        )
        .volume(),
    ]
}

fn dist(a: Pos, b: Pos) -> f64 {
    offset_vec(a - b).length()
}

fn extrapolating() -> DisplaySettings {
    DisplaySettings { drawn_time: DrawnTime::Extrapolate { l_max: Duration::from_millis(80) }, ..Default::default() }
}

/// Drive a display at `display_hz` against a simulation at `sim_hz`: views are published 2 ms after their frame's
/// time; each display frame is rendered at its start and shown `lead` later. `world(f, t)` builds frame `f`'s objects.
struct Run {
    sim_hz: u32,
    display_hz: u32,
    lead: Duration,
}

impl Run {
    fn drive(
        &self,
        layer: &mut DisplayLayer,
        seconds: f64,
        world: &dyn Fn(u64, HostTime) -> Vec<ViewObject>,
        mut each: impl FnMut(HostTime, &pe_render::display::DisplayFrame),
    ) {
        let vols = volumes();
        let publish = 2_000_000;
        let mut published: Option<(u64, Arc<FrameView>)> = None;
        let frames = (seconds * self.display_hz as f64) as u64;
        for k in 10..frames {
            let render_at = HostTime((k as i128 * NS as i128 / self.display_hz as i128) as i64);
            // The newest frame whose view is published by now.
            let mut f = published.as_ref().map(|(f, _)| *f).unwrap_or(0);
            while frame_time(f + 1, self.sim_hz).0 + publish <= render_at.0 {
                f += 1;
            }
            if published.as_ref().is_none_or(|(pf, _)| *pf != f) {
                let t = frame_time(f, self.sim_hz);
                published = Some((f, view(f, t, world(f, t))));
            }
            let v = published.as_ref().unwrap().1.clone();
            let local = LocalPresentation { display_time: render_at.plus(self.lead), hands: [None, None] };
            let out =
                layer.frame(FrameInput { view: &v, display_time: local.display_time, volumes: &vols, local: &local });
            each(local.display_time, &out);
        }
    }
}

// ---- test 3: rates (D8) ------------------------------------------------------------------------------------------

/// Steady motion keeps a zero offset across every frame-view arrival, at every rate pair, under both rules, and the
/// drawn pose's step is bounded by velocity × display period (review M3).
#[test]
fn steady_motion_never_hitches_at_any_rate_pair() {
    let v = [1.5f32, 0.0, -0.75];
    let w = [0.0f32, 2.0, 0.0];
    let speed = offset_vec(Offset([1.5, 0.0, -0.75])).length();
    let world = move |_f: u64, t: HostTime| {
        let s = t.0 as f64 / NS as f64;
        let mut o = ball(1, at(-2.0 + 1.5 * s, 0.0, -5.0 - 0.75 * s), v);
        o.angular_velocity = w;
        let q = DQuat::from_rotation_y(2.0 * s);
        o.rotation = [q.x as f32, q.y as f32, q.z as f32, q.w as f32];
        vec![o]
    };
    for rule in [
        DrawnTime::Extrapolate { l_max: Duration::from_millis(80) },
        DrawnTime::InterpolateLate { d_render: Duration::from_millis(70) },
    ] {
        // The supported simulation range is 30-120 Hz (SimRate, @engine-architect): both ends, the default, and
        // two rates between.
        for sim_hz in [30, 60, 75, 90, 120] {
            for display_hz in [72, 90, 120, 165] {
                let run = Run { sim_hz, display_hz, lead: Duration::from_millis(30) };
                let mut layer = DisplayLayer::new(DisplaySettings { drawn_time: rule, ..Default::default() });
                let mut last: Option<Pos> = None;
                let mut worst_step = 0.0f64;
                run.drive(&mut layer, 2.0, &world, |t, out| {
                    let o = &out.objects[0];
                    assert!(dist(o.pose.pos, o.target.pos) < 1e-9, "{rule:?} {sim_hz}/{display_hz}: offset");
                    assert!(o.cue.is_none());
                    if t.0 < NS / 4 {
                        return; // warm-up: (b) needs a history before t_draw has views on both sides
                    }
                    if let Some(p) = last {
                        worst_step = worst_step.max(dist(o.pose.pos, p));
                    }
                    last = Some(o.pose.pos);
                });
                let c = layer.counters();
                assert_eq!(
                    (c.corrections, c.extrapolation_misses, c.cues),
                    (0, 0, 0),
                    "{rule:?} {sim_hz}/{display_hz}"
                );
                let bound = speed / display_hz as f64 + 1e-6;
                assert!(worst_step <= bound, "{rule:?} {sim_hz}/{display_hz}: step {worst_step} > {bound}");
                assert!(c.views_taken > 0);
            }
        }
    }
}

/// Gravity under X8 (a): velocity-only extrapolation misses every tick by about g × tick × span. Those misses blend
/// and count, but none reaches `E_cue`, so a falling ball never shows a cue.
#[test]
fn a_falling_ball_misses_by_less_than_a_hands_width() {
    let world = |_f: u64, t: HostTime| {
        let s = t.0 as f64 / NS as f64;
        let mut o = ball(1, at(0.0, 10.0 - 4.905 * s * s, -5.0), [0.0, (-9.81 * s) as f32, 0.0]);
        o.radius = 0.05;
        vec![o]
    };
    let run = Run { sim_hz: 60, display_hz: 90, lead: Duration::from_millis(50) };
    let mut layer = DisplayLayer::new(extrapolating());
    let mut worst = 0.0f64;
    run.drive(&mut layer, 1.0, &world, |_, out| {
        let o = &out.objects[0];
        worst = worst.max(dist(o.pose.pos, o.target.pos));
        assert!(o.cue.is_none());
    });
    let c = layer.counters();
    assert!(c.extrapolation_misses > 0, "gravity is a miss under velocity extrapolation");
    assert!(c.extrapolation_error_max < 0.10 && c.cues == 0, "{c:?}");
    // Measured, for the record (render.md 13: extrapolation_error is reported apart).
    eprintln!(
        "falling ball, 60 Hz sim, 90 Hz display, 50 ms lead: miss max {:.4} m, drawn-target max {worst:.4} m",
        c.extrapolation_error_max
    );
}

// ---- test 1: the display layer ------------------------------------------------------------------------------------

/// Arrivals at 60 Hz on a 120 Hz display, with an object at rest; `jump(f)` gives frame f's displacement.
fn correction_run(
    settings: DisplaySettings,
    place: Pos,
    jumps: &[(u64, [f64; 3])],
) -> (DisplayLayer, Vec<(HostTime, u64, Pos, Pos, bool)>) {
    let mut layer = DisplayLayer::new(settings);
    let vols = volumes();
    let mut rows = Vec::new();
    let mut off = [0.0f64; 3];
    let mut cur: Option<Arc<FrameView>> = None;
    for k in 0..240u64 {
        let now = HostTime(k as i64 * NS / 120);
        let f = k / 2;
        if cur.as_ref().is_none_or(|v| v.frame.0 != f) {
            let mut corrected = false;
            for (jf, d) in jumps {
                if *jf == f {
                    for a in 0..3 {
                        off[a] += d[a];
                    }
                    corrected = true;
                }
            }
            let mut o = ball(1, place.checked_add(Offset(off)).unwrap(), [0.0; 3]);
            if corrected {
                o.flags = o.flags.with(ViewFlags::CORRECTED);
            }
            cur = Some(view(f, frame_time(f, 60), vec![o]));
        }
        let v = cur.clone().unwrap();
        let local = LocalPresentation { display_time: now, hands: [None, None] };
        let out = layer.frame(FrameInput { view: &v, display_time: now, volumes: &vols, local: &local });
        let o = &out.objects[0];
        rows.push((now, f, o.pose.pos, o.target.pos, o.cue.is_some()));
    }
    (layer, rows)
}

#[test]
fn a_correction_blends_to_the_target_within_t_blend_without_overshoot() {
    let place = at(0.0, 0.0, -4.0);
    let (layer, rows) = correction_run(extrapolating(), place, &[(30, [0.04, 0.0, 0.0])]);
    let i = rows.iter().position(|r| r.1 == 30).unwrap();
    // At the arrival the drawn pose has not moved.
    assert!(dist(rows[i].2, rows[i - 1].2) < 1e-12);
    // Monotone toward the target, never past it (the x axis carries the whole correction).
    let x = |p: Pos| offset_vec(p - Pos::ORIGIN).x;
    let mut prev = x(rows[i].2);
    for r in &rows[i..] {
        let xi = x(r.2);
        assert!(xi >= prev - 1e-12 && xi <= x(r.3) + 1e-12, "overshoot or reversal");
        prev = xi;
    }
    // Reaches the target within T_blend of the correction (100 ms = 12 frames at 120 Hz).
    assert!(dist(rows[i + 12].2, rows[i + 12].3) < 1e-12);
    assert!(dist(rows[i + 11].2, rows[i + 11].3) > 0.0);
    let c = layer.counters();
    assert_eq!((c.corrections, c.cues, c.cueless_large_corrections), (1, 0, 0));
}

#[test]
fn a_mid_blend_correction_never_jumps() {
    let place = at(0.0, 0.0, -4.0);
    let (_, rows) = correction_run(extrapolating(), place, &[(30, [0.04, 0.0, 0.0]), (32, [-0.03, 0.02, 0.0])]);
    let (_, alone) = correction_run(extrapolating(), place, &[(30, [0.04, 0.0, 0.0])]);
    let i = rows.iter().position(|r| r.1 == 32).unwrap();
    // At the second arrival the drawn pose is exactly where the first blend alone puts it.
    assert!(dist(rows[i].2, alone[i].2) < 1e-12, "the second arrival moved the drawn pose");
    assert!(dist(rows[i].2, rows[i].3) > 0.01, "mid-blend");
    let j = rows.iter().position(|r| r.1 == 30).unwrap();
    for w in rows[j..].windows(2) {
        // Continuous: no step larger than the blend can make in one display frame.
        assert!(dist(w[1].2, w[0].2) < 0.012, "step {}", dist(w[1].2, w[0].2));
    }
}

#[test]
fn every_in_view_correction_above_e_cue_has_a_cue() {
    // A deterministic spread of sizes, in and out of view.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut expected = 0;
    let mut checked = 0;
    for case in 0..40 {
        let in_view = case % 2 == 0;
        let place = if in_view { at(0.0, 0.0, -4.0) } else { at(0.0, 0.0, 6.0) };
        let size = 0.002 + rnd() * 1.6;
        let d = [size * 0.6, size * 0.8, 0.0];
        let (layer, rows) = correction_run(extrapolating(), place, &[(30, d)]);
        let c = layer.counters();
        let i = rows.iter().position(|r| r.1 == 30).unwrap();
        let large = in_view && size > 0.10;
        assert_eq!(rows[i].4, large, "case {case} size {size:.3} in_view {in_view}");
        assert_eq!(c.cueless_large_corrections, 0);
        if !in_view && size > 1.0 {
            assert_eq!(c.jumps_out_of_view, 1, "out of view above E_snap jumps");
            assert!(dist(rows[i].2, rows[i].3) < 1e-12);
        }
        if in_view {
            assert_eq!(c.snaps_in_view, 0);
        }
        expected += large as u32;
        checked += 1;
    }
    assert!(expected > 5 && checked == 40);
}

#[test]
fn the_controls_bite() {
    let place = at(0.0, 0.0, -4.0);
    let s = |controls| DisplaySettings { controls, ..extrapolating() };
    let (layer, _) =
        correction_run(s(Controls { blend: false, ..Controls::default() }), place, &[(30, [0.04, 0.0, 0.0])]);
    assert_eq!(layer.counters().snaps_in_view, 1, "blending off must show as snaps in view");
    let (layer, _) = correction_run(s(Controls { cue: false, ..Controls::default() }), place, &[(30, [0.3, 0.0, 0.0])]);
    assert_eq!(layer.counters().cueless_large_corrections, 1, "the cue off must count as cueless");
}

/// D4: the own hand and the held ball carry no offset, through a correction and a hold lost to a race; the control
/// that blends the own hand makes `own_error` non-zero.
#[test]
fn own_hands_and_held_objects_are_never_corrected() {
    for correct_own in [false, true] {
        let settings = DisplaySettings { controls: Controls { correct_own, ..Controls::default() }, ..extrapolating() };
        let mut layer = DisplayLayer::new(settings);
        let vols = volumes();
        let me = MemberKey([7; 32]);
        let hand_id = ObjectId(10);
        let mut cues_on_ball = 0;
        let mut last_ball: Option<Pos> = None;
        for k in 0..120u64 {
            let now = HostTime(k as i64 * NS / 120);
            let f = k / 2;
            let t = frame_time(f, 60);
            // The simulated hand drifts (late-latching draws it elsewhere), and is corrected at frame 20.
            let hx = 0.2 + if f >= 20 { 0.15 } else { 0.0 };
            let mut hand = ball(10, at(hx, 0.1, -0.5), [0.0; 3]);
            hand.hand_of = Some((me, Hand::Right));
            hand.flags = ViewFlags::OWN_HAND.with(if f == 20 { ViewFlags::CORRECTED } else { ViewFlags::default() });
            let mut b = if f < 40 {
                let mut b = ball(1, at(hx, 0.1, -0.6), [0.0; 3]);
                b.held_by = Some(Holder { member: me, hand: hand_id });
                b.flags = ViewFlags::HELD_LOCAL;
                b
            } else {
                // Lost the race at frame 40: the winner has it 40 cm away.
                ball(1, at(hx + 0.4, 0.1, -0.6), [0.0; 3])
            };
            if f == 40 {
                b.flags = b.flags.with(ViewFlags::CORRECTED);
            }
            let mut v = Arc::unwrap_or_clone(view(f, t, vec![hand, b]));
            if f == 40 {
                v.effects.push(Effect {
                    key: EffectKey {
                        frame: Frame(40),
                        cause: EffectCause::Physics,
                        object: ObjectId(1),
                        kind: EffectKind::GrabLost,
                        ordinal: 0,
                    },
                    at: at(0.0, 0.0, 0.0),
                    own: true,
                    resimulated: false,
                    trigger_arrived: None,
                    data: Default::default(),
                });
            }
            let v = Arc::new(v);
            let local = LocalPresentation {
                display_time: now,
                hands: [
                    None,
                    Some(DisplayHand {
                        pose: RigPose { position: [0.25, 0.1, -0.5], orientation: [0.0, 0.0, 0.0, 1.0] },
                    }),
                ],
            };
            let out = layer.frame(FrameInput { view: &v, display_time: now, volumes: &vols, local: &local });
            let h = out.objects.iter().find(|o| o.id == hand_id).unwrap();
            let bo = out.objects.iter().find(|o| o.id == ObjectId(1)).unwrap();
            if !correct_own {
                // The hand is drawn at the late-latched pose (an f32 rig pose, hence 1e-6).
                assert!(dist(h.pose.pos, at(0.25, 0.1, -0.5)) < 1e-6);
                if f < 40 {
                    // The held ball at the hand's drawn pose times the grip offset (10 cm in front of the hand).
                    assert!(dist(bo.pose.pos, at(0.25, 0.1, -0.6)) < 1e-6, "held ball at {:?}", bo.pose.pos);
                }
                if f == 40 && k % 2 == 0 {
                    // The hold ended: the ball leaves the hand from where it was drawn.
                    assert!(dist(bo.pose.pos, last_ball.unwrap()) < 1e-12);
                }
            }
            cues_on_ball += (f >= 40 && bo.cue.is_some()) as u32;
            last_ball = Some(bo.pose.pos);
        }
        let c = layer.counters();
        if correct_own {
            assert!(c.own_error > 0.1, "blending the own hand must show in own_error: {c:?}");
        } else {
            assert_eq!(c.own_error, 0.0, "{c:?}");
            assert_eq!(c.hold_ends, 1);
            assert!(cues_on_ball > 0, "a lost grab carries its cue (D7)");
        }
    }
}

/// X8 (b): a correction blends too, from when the drawn time reaches the corrected view, instead of showing as one
/// simulation period of fast motion; in view above `E_cue` it carries the cue.
#[test]
fn under_interpolation_a_correction_blends_and_cues() {
    let settings = DisplaySettings {
        drawn_time: DrawnTime::InterpolateLate { d_render: Duration::from_millis(60) },
        ..Default::default()
    };
    let mut layer = DisplayLayer::new(settings);
    let vols = volumes();
    let mut rows = Vec::new();
    let mut cur: Option<Arc<FrameView>> = None;
    for k in 0..240u64 {
        let now = HostTime(k as i64 * NS / 120);
        let f = k / 2;
        if cur.as_ref().is_none_or(|v| v.frame.0 != f) {
            let mut o = ball(1, at(if f >= 30 { 0.5 } else { 0.0 }, 0.0, -4.0), [0.0; 3]);
            if f == 30 {
                o.flags = o.flags.with(ViewFlags::CORRECTED);
            }
            cur = Some(view(f, frame_time(f, 60), vec![o]));
        }
        let v = cur.clone().unwrap();
        let local = LocalPresentation { display_time: now, hands: [None, None] };
        let out = layer.frame(FrameInput { view: &v, display_time: now, volumes: &vols, local: &local });
        rows.push((out.objects[0].pose.pos, out.objects[0].cue.is_some()));
    }
    for w in rows.windows(2) {
        assert!(dist(w[1].0, w[0].0) < 0.1, "a 50 cm correction shown as a jump of {}", dist(w[1].0, w[0].0));
    }
    assert!(rows.iter().any(|r| r.1));
    let c = layer.counters();
    assert_eq!((c.corrections, c.cueless_large_corrections, c.extrapolation_misses), (1, 0, 0), "{c:?}");
    assert!(dist(rows.last().unwrap().0, at(0.5, 0.0, -4.0)) < 1e-12);
}

// ---- test 2: effects --------------------------------------------------------------------------------------------

fn effect(frame: u64, own: bool, resimulated: bool, trigger_arrived: Option<HostTime>) -> Effect {
    Effect {
        key: EffectKey {
            frame: Frame(frame),
            cause: EffectCause::Physics,
            object: ObjectId(1),
            kind: EffectKind::ContactBegin,
            ordinal: 0,
        },
        at: Pos::ORIGIN,
        own,
        resimulated,
        trigger_arrived,
        data: Default::default(),
    }
}

fn with_effects(frame: u64, effects: Vec<Effect>, retracted: Vec<EffectKey>) -> FrameView {
    let mut v = Arc::unwrap_or_clone(view(frame, frame_time(frame, 60), vec![]));
    v.effects = effects;
    v.retracted = retracted;
    v
}

fn plays(c: &[EffectCommand]) -> usize {
    c.iter().filter(|c| matches!(c, EffectCommand::Play(_))).count()
}

#[test]
fn a_replayed_key_fires_once_and_a_retracted_one_stops() {
    let mut g = EffectGate::new(EffectSettings::default());
    let e = effect(100, false, false, None);
    assert_eq!(plays(&g.take(&with_effects(100, vec![e.clone()], vec![]), 60, frame_time(100, 60))), 1);
    // A replay re-publishes the same key: not played again.
    let mut again = e.clone();
    again.resimulated = true;
    again.trigger_arrived = Some(frame_time(101, 60));
    assert_eq!(plays(&g.take(&with_effects(103, vec![again], vec![]), 60, frame_time(103, 60))), 0);
    let out = g.take(&with_effects(104, vec![], vec![e.key]), 60, frame_time(104, 60));
    assert_eq!(out, vec![EffectCommand::Stop(e.key)]);
    assert_eq!(g.counters().replays_suppressed, 1);
}

#[test]
fn freshness_plays_a_late_push_once_and_a_merge_never() {
    let mut g = EffectGate::new(EffectSettings::default());
    // A remote push arriving 0.6 s after its frame re-simulates a collision: it sounds once.
    let late = effect(200, false, true, Some(frame_time(200, 60).plus(Duration::from_millis(600))));
    assert_eq!(plays(&g.take(&with_effects(240, vec![late], vec![]), 60, frame_time(240, 60))), 1);
    // A cause arriving 1.2 s after the frame: past T_effect_lag.
    let stale = EffectKey { ordinal: 1, ..effect(300, false, true, None).key };
    let mut e = effect(300, false, true, Some(frame_time(300, 60).plus(Duration::from_millis(1200))));
    e.key = stale;
    assert_eq!(plays(&g.take(&with_effects(380, vec![e], vec![]), 60, frame_time(380, 60))), 0);
    // A 60 s merge: re-simulated effects with no newly arrived cause never play.
    let merge: Vec<Effect> = (0..50)
        .map(|i| {
            let mut e = effect(1000 + i, false, true, None);
            e.key.ordinal = 9;
            e
        })
        .collect();
    assert_eq!(plays(&g.take(&with_effects(4600, merge, vec![]), 60, frame_time(4600, 60))), 0);
    assert_eq!(g.counters().stale_dropped, 51);
}

#[test]
fn own_effects_play_at_once_and_others_wait_d_effect() {
    let mut g = EffectGate::new(EffectSettings { d_effect: Duration::from_millis(50), ..Default::default() });
    let mine = EffectKey { ordinal: 1, ..effect(10, true, false, None).key };
    let mut own = effect(10, true, false, None);
    own.key = mine;
    let other = effect(10, false, false, None);
    let now = frame_time(10, 60);
    assert_eq!(plays(&g.take(&with_effects(10, vec![own, other], vec![]), 60, now)), 1);
    assert_eq!(plays(&g.due(now.plus(Duration::from_millis(49)))), 0);
    assert_eq!(plays(&g.due(now.plus(Duration::from_millis(50)))), 1);
}
