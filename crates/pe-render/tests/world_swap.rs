//! pe-app's in-game Join and New session (app.md 4.1): one frame slot and one presenter serve every session a process
//! runs. The presenter starts over at a new world's first view, so nothing of the old world is blended into it, and
//! the frames between, which draw the old world's last view again, are counted as held, not as display frames.

use std::sync::Arc;
use std::time::Duration;

use glam::DQuat;
use pe_render::LocalPresentation;
use pe_render::camera::{Camera, DepthRange, Fov};
use pe_render::display::DisplaySettings;
use pe_render::drawn_time::DrawnTime;
use pe_render::effects::{EffectCommand, EffectSettings};
use pe_render::pose::{WorldPose, offset_vec};
use pe_render::present::{FrameSlot, PresentedFrame, Presenter, WorldStart};
use pe_render::scene::Environment;
use pe_seams::{
    DomainId, Effect, EffectCause, EffectKey, EffectKind, Frame, FrameView, HostTime, ObjectClass, ObjectId, Offset,
    Pos, RigTransform, Shape, ViewCounters, ViewFlags, ViewObject,
};

const NS: i64 = 1_000_000_000;

fn at(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).unwrap()
}

fn ball(id: u128, p: Pos) -> ViewObject {
    ViewObject {
        id: ObjectId::from_u128(id),
        class: ObjectClass::World,
        pos: p,
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0; 3],
        linear_velocity: [0.0; 3],
        angular_velocity: [0.0; 3],
        shape: Some(Shape::Sphere { radius: 0.05 }),
        appearance: None,
        light: None,
        radius: 0.05,
        hand_of: None,
        body_of: None,
        held_by: None,
        flags: ViewFlags::default(),
        version: None,
    }
}

/// Frame `frame` of a world whose frame 0 was at `epoch`, at 60 Hz, holding `objects` and `effects`.
fn view(frame: u64, epoch: HostTime, objects: Vec<ViewObject>, effects: Vec<Effect>) -> Arc<FrameView> {
    Arc::new(FrameView {
        frame: Frame(frame),
        domain: DomainId(0),
        sim_time: HostTime(epoch.0 + (frame as i64 * NS) / 60),
        final_frame: None,
        health: pe_seams::Health::Agreed,
        local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
        objects,
        effects,
        retracted: vec![],
        counters: ViewCounters::default(),
        changes: vec![],
        activations: Vec::new(),
        activations_retracted: Vec::new(),
        hits: Vec::new(),
        hits_from: None,
        heads: Vec::new(),
    })
}

/// The local player's own effect at `frame` on ball 1: it plays at once.
fn own_effect(frame: u64) -> Effect {
    Effect {
        key: EffectKey {
            frame: Frame(frame),
            cause: EffectCause::Physics,
            object: ObjectId::from_u128(1),
            kind: EffectKind::ContactBegin,
            ordinal: 0,
        },
        at: Pos::ORIGIN,
        own: true,
        resimulated: false,
        trigger_arrived: None,
        data: Default::default(),
    }
}

fn camera() -> Camera {
    Camera::placed(WorldPose { pos: Pos::ORIGIN, rot: DQuat::IDENTITY }, Fov::symmetric(1.2, 1.6), DepthRange::DESKTOP)
}

/// One display frame at `t`, as the VR sink and the window run it: the slot's batch, the drawn rig, then the frame.
fn display(p: &mut Presenter, slot: &FrameSlot, t: HostTime) -> PresentedFrame {
    let batch = slot.take();
    let local = LocalPresentation { display_time: t, hands: [None, None] };
    p.drawn_rig(&batch, t, None);
    p.frame_batch(&batch, &local, &[camera()]).expect("a view has been published")
}

fn dist(a: Pos, b: Pos) -> f64 {
    offset_vec(a - b).length()
}

fn presenter(drawn_time: DrawnTime) -> Presenter {
    let settings = DisplaySettings { drawn_time, ..Default::default() };
    Presenter::new(settings, EffectSettings::default(), Environment::default(), 60)
}

/// World 1 holds ball 1 at A and plays an effect on it; the session changes; world 2 holds a ball with the same id at
/// B, its frames starting again at 0. Until world 2's first view the old view is drawn again, as held frames. World
/// 2's first frame draws the ball exactly at B, with no blend from A, no gap, correction or cue, and stops world 1's
/// effect.
#[test]
fn a_new_world_starts_the_presenter_over_at_its_first_view() {
    let slot = FrameSlot::new();
    let mut p = presenter(DrawnTime::default());
    let (a, b) = (at(0.0, 0.0, -3.0), at(2.0, 0.0, -3.0));
    let epoch1 = HostTime(0);
    let mut played = Vec::new();
    for f in 1..=30 {
        let effects = if f == 5 { vec![own_effect(5)] } else { vec![] };
        slot.publish(view(f, epoch1, vec![ball(1, a)], effects));
        let out = display(&mut p, &slot, HostTime((f as i64 * NS) / 60 + 5_000_000));
        played.extend(out.effects.iter().filter_map(|c| match c {
            EffectCommand::Play(e) => Some(e.key),
            _ => None,
        }));
        assert!(dist(out.display.objects[0].pose.pos, a) < 1e-9);
    }
    assert_eq!(played.len(), 1, "world 1's effect played once");
    let before = p.counters().run();

    // The session changes. World 1's simulation has stopped, so nothing more is published to it.
    assert_eq!(slot.begin_world(WorldStart::default()), 1);
    let swap_at = HostTime(NS);
    for k in 0..5 {
        let out = display(&mut p, &slot, HostTime(swap_at.0 + k * 11_111_111));
        assert!(dist(out.display.objects[0].pose.pos, a) < 1e-9, "the old world's last view, drawn again");
        assert!(out.effects.is_empty());
        assert_eq!(p.world(), 0, "not world 2 before its first view");
    }
    let held = p.counters().run();
    assert_eq!(held.held_for_swap, 5);
    assert_eq!(held.display.display_frames, before.display.display_frames, "held frames aren't display frames");
    assert_eq!(held.view_age_ms.count, before.view_age_ms.count, "nor do they age the view");

    // World 2: a new clock, frame 0 at 1.1 s, the same object id at another place.
    let epoch2 = HostTime(NS + 100_000_000);
    slot.publish(view(0, epoch2, vec![ball(1, b)], vec![]));
    let out = display(&mut p, &slot, HostTime(epoch2.0 + 5_000_000));
    assert_eq!(p.world(), 1);
    assert!(
        dist(out.display.objects[0].pose.pos, b) < 1e-9,
        "drawn where world 2 put it: {:?}",
        out.display.objects[0]
    );
    assert!(out.display.objects[0].cue.is_none(), "no correction cue");
    let stops: Vec<EffectKey> = out
        .effects
        .iter()
        .filter_map(|c| match c {
            EffectCommand::Stop(k) => Some(*k),
            _ => None,
        })
        .collect();
    assert_eq!(stops, played, "world 1's effect stops at the change");
    for f in 1..=10 {
        slot.publish(view(f, epoch2, vec![ball(1, b)], vec![]));
        let out = display(&mut p, &slot, HostTime(epoch2.0 + (f as i64 * NS) / 60 + 5_000_000));
        assert!(dist(out.display.objects[0].pose.pos, b) < 1e-9);
        assert!(out.effects.is_empty(), "the stop is sent once");
    }
    let after = p.counters().run();
    assert_eq!(p.gaps(), 0, "a new world is not a gap");
    assert_eq!(after.display.corrections, 0, "nothing corrected across the change");
    assert_eq!(after.held_for_swap, 5);
    assert_eq!(after.display.display_frames, before.display.display_frames + 11);
}

/// The new world's delay reaches `Shown` (the session's setting, from its record); an explicit other rule is kept.
#[test]
fn shown_takes_the_new_world_s_delay_and_another_rule_is_kept() {
    let delay = Duration::from_millis(50);
    for (rule, want) in [
        (DrawnTime::shown(Duration::ZERO, 60), DrawnTime::shown(delay, 60)),
        (
            DrawnTime::Present { l_max: Duration::from_millis(33) },
            DrawnTime::Present { l_max: Duration::from_millis(33) },
        ),
    ] {
        let slot = FrameSlot::new();
        let mut p = presenter(rule);
        slot.publish(view(1, HostTime(0), vec![ball(1, at(0.0, 0.0, -3.0))], vec![]));
        display(&mut p, &slot, HostTime(20_000_000));
        slot.begin_world(WorldStart { display_delay: delay });
        slot.publish(view(0, HostTime(NS), vec![ball(1, at(0.0, 0.0, -3.0))], vec![]));
        display(&mut p, &slot, HostTime(NS + 20_000_000));
        assert_eq!(p.display().settings().drawn_time, want);
    }
}

/// The slot: `begin_world` drops the views not yet taken and keeps `newest`; `newest_current` is `None` until the new
/// world publishes; each batch names its world and start.
#[test]
fn a_new_world_drops_untaken_views_and_keeps_newest() {
    let slot = FrameSlot::new();
    for f in 1..=3 {
        slot.publish(view(f, HostTime(0), vec![], vec![]));
    }
    assert_eq!(slot.newest_current().map(|v| v.frame), Some(Frame(3)));
    let start = WorldStart { display_delay: Duration::from_millis(7) };
    assert_eq!(slot.begin_world(start), 1);
    assert_eq!(slot.world(), 1);
    let b = slot.take();
    assert!(b.views.is_empty(), "world 0's untaken views went");
    assert_eq!((b.world, b.start), (1, Some(start)));
    assert_eq!(slot.newest().map(|v| v.frame), Some(Frame(3)), "newest kept for readers that wait for a view");
    assert!(slot.newest_current().is_none(), "but it isn't the current world's");
    slot.publish(view(0, HostTime(NS), vec![], vec![]));
    assert_eq!(slot.newest_current().map(|v| v.frame), Some(Frame(0)));
    let b = slot.take();
    assert_eq!((b.world, b.views.len()), (1, 1));
}

/// The old world's last view is held, not run on (@xr-eng): a moving ball is drawn moved on by its
/// velocity for at most `l_max` past the newest view, then stays there through every held frame of the change.
#[test]
fn the_old_world_is_held_where_its_extrapolation_stopped() {
    let slot = FrameSlot::new();
    let l_max = Duration::from_millis(80);
    let mut p = presenter(DrawnTime::Extrapolate { l_max });
    let moving = |f: u64| {
        let mut b = ball(1, at(f as f64 / 60.0, 0.0, -3.0));
        b.linear_velocity = [1.0, 0.0, 0.0];
        b
    };
    for f in 1..=30 {
        slot.publish(view(f, HostTime(0), vec![moving(f)], vec![]));
        display(&mut p, &slot, HostTime((f as i64 * NS) / 60 + 5_000_000));
    }
    slot.begin_world(WorldStart::default());
    let held_at = at(30.0 / 60.0 + l_max.as_secs_f64(), 0.0, -3.0);
    // 15 s of held frames at 90 Hz, sampled once a second.
    for k in 1..=15 {
        let out = display(&mut p, &slot, HostTime(NS / 2 + k * NS));
        let drawn = out.display.objects[0].pose.pos;
        assert!(dist(drawn, held_at) < 1e-6, "held at newest + v·l_max, {k} s in: {drawn:?}");
    }
    assert_eq!(p.counters().run().held_for_swap, 15);
}
