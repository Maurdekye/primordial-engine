//! Tests for the Primordial Engine fork's scale-dependent contact-recycling radius.
//! (Not upstream.)

use crate::body::{body_get_position, create_body};
use crate::hull::make_box_hull;
use crate::math_functions::{Pos, Vec3};
use crate::recording::{deserialize_into_shell, serialize_world, GeometryRegistry, RecBuffer};
use crate::recycle_scale::*;
use crate::shape::create_hull_shape;
use crate::types::{default_body_def, default_shape_def, default_world_def, BodyType, WorldDef};
use crate::world::{
    world_get_contact_recycle_scaling, world_get_counters, world_set_contact_recycle_radius_fn,
    world_set_contact_recycle_scaling, World,
};

fn input(a: f32, b: f32, sa: bool, sb: bool) -> RecyclePairInput {
    RecyclePairInput { length_a: a, length_b: b, static_a: sa, static_b: sb, base_distance: 0.05 }
}

#[test]
fn formula_defaults() {
    let s = ContactRecycleScaling::enabled();
    // A 1 m cube (inner radius 0.5 m) on static ground gets the upstream 0.05 m.
    assert_eq!(default_recycle_radius(&input(0.5, 123.0, false, true), &s), 0.1f32 * 0.5f32);
    assert_eq!(default_recycle_radius(&input(1.0e30, 0.5, true, false), &s), 0.1f32 * 0.5f32);
    // Min combine.
    let min = ContactRecycleScaling { combine: RecycleLengthCombine::Min, ..s };
    assert_eq!(default_recycle_radius(&input(0.5, 2.0, false, false), &min), 0.1f32 * 0.5f32);
    // Clamps.
    assert_eq!(default_recycle_radius(&input(0.001, 1.0, false, false), &s), s.min_distance);
    assert_eq!(default_recycle_radius(&input(100.0, 50.0, false, false), &s), s.max_distance);
    // Harmonic mean (the default): 2ab/(a+b) of 0.5 and 2.0 = 0.8.
    assert_eq!(s.combine, RecycleLengthCombine::HarmonicMean);
    let hm = s;
    assert_eq!(default_recycle_radius(&input(0.5, 2.0, false, false), &hm), 0.1f32 * (2.0 * 0.5 * 2.0 / 2.5));
    // Mis-ordered clamps resolve to max_distance.
    let bad = ContactRecycleScaling { min_distance: 1.0, max_distance: 0.2, ..s };
    assert_eq!(default_recycle_radius(&input(0.5, 0.5, false, false), &bad), 0.2);
}

#[test]
fn defaults_are_off() {
    let def = default_world_def();
    assert!(!def.contact_recycle_scaling.enabled);
    let world = World::new(&def);
    assert!(!world.contact_recycle_scaling.enabled);
    assert!(world.contact_recycle_radius_fcn.is_none());
}

fn resting_box_world(def: &WorldDef) -> World {
    let mut world = World::new(def);
    let mut bd = default_body_def();
    bd.type_ = BodyType::Static;
    bd.position = Pos { x: 0.0, y: -0.5, z: 0.0 };
    let g = create_body(&mut world, &bd);
    create_hull_shape(&mut world, g, &default_shape_def(), &make_box_hull(10.0, 0.5, 10.0).base);
    let mut bd = default_body_def();
    bd.type_ = BodyType::Dynamic;
    bd.position = Pos { x: 0.0, y: 0.5, z: 0.0 };
    let b = create_body(&mut world, &bd);
    create_hull_shape(&mut world, b, &default_shape_def(), &make_box_hull(0.5, 0.5, 0.5).base);
    world
}

fn count_recycled(world: &mut World) -> i32 {
    let mut n = 0;
    for _ in 0..60 {
        world.step(1.0 / 60.0, 4);
        n += world_get_counters(world).recycled_contact_count;
    }
    n
}

fn zero_radius(_: &RecyclePairInput, _: &ContactRecycleScaling) -> f32 {
    0.0
}

fn nan_radius(_: &RecyclePairInput, _: &ContactRecycleScaling) -> f32 {
    f32::NAN
}

fn negative_radius(_: &RecyclePairInput, _: &ContactRecycleScaling) -> f32 {
    -1.0
}

#[test]
fn hook_controls_recycling() {
    let mut def = default_world_def();
    def.enable_sleep = false;
    def.contact_recycle_scaling = ContactRecycleScaling::enabled();

    let mut w = resting_box_world(&def);
    assert!(count_recycled(&mut w) > 0, "default formula recycles a resting box");

    for f in [zero_radius as ContactRecycleRadiusFn, nan_radius, negative_radius] {
        let mut w = resting_box_world(&def);
        world_set_contact_recycle_radius_fn(&mut w, Some(f));
        assert_eq!(count_recycled(&mut w), 0, "a radius <= 0 or NaN disables recycling");
    }

    // The hook is ignored while scaling is disabled.
    let mut def_off = def.clone();
    def_off.contact_recycle_scaling.enabled = false;
    let mut w = resting_box_world(&def_off);
    world_set_contact_recycle_radius_fn(&mut w, Some(zero_radius));
    assert!(count_recycled(&mut w) > 0);
}

/// A small cube sliding off a ledge: upstream's fixed 0.05 m tolerance keeps a 1 cm cube
/// supported by stale contact points several cube-widths past the edge; the scaled radius does not.
fn ledge_overhang(scaling: bool) -> f32 {
    let s = 0.01f32;
    let mut def = default_world_def();
    def.contact_recycle_scaling.enabled = scaling;
    let mut world = World::new(&def);
    let mut bd = default_body_def();
    bd.type_ = BodyType::Static;
    bd.position = Pos { x: (-10.0 * s) as _, y: s as _, z: 0.0 };
    let g = create_body(&mut world, &bd);
    create_hull_shape(&mut world, g, &default_shape_def(), &make_box_hull(10.0 * s, s, 10.0 * s).base);
    let mut bd = default_body_def();
    bd.type_ = BodyType::Dynamic;
    bd.enable_sleep = false;
    bd.position = Pos { x: (-6.0 * s) as _, y: (2.5 * s) as _, z: 0.0 };
    bd.linear_velocity = Vec3 { x: 2.0 * s, y: 0.0, z: 0.0 };
    let b = create_body(&mut world, &bd);
    let mut sd = default_shape_def();
    sd.base_material.friction = 0.0;
    create_hull_shape(&mut world, b, &sd, &make_box_hull(0.5 * s, 0.5 * s, 0.5 * s).base);
    for _ in 0..1200 {
        world.step(1.0 / 60.0, 4);
        let p = body_get_position(&world, b);
        if (p.y as f32) < 2.45 * s {
            return p.x as f32 / s;
        }
    }
    f32::INFINITY
}

#[test]
fn small_cube_ledge_overhang() {
    let fixed = ledge_overhang(false);
    let scaled = ledge_overhang(true);
    assert!(fixed > 1.5, "upstream fixed radius: overhang {fixed} cube widths");
    assert!(scaled < 0.75, "scaled radius: overhang {scaled} cube widths");
}

#[test]
fn snapshot_carries_scaling_only_when_enabled() {
    let custom = ContactRecycleScaling {
        enabled: true,
        length_fraction: 0.25,
        min_distance: 0.001,
        max_distance: 2.0,
        combine: RecycleLengthCombine::HarmonicMean,
    };
    let mut def = default_world_def();
    def.contact_recycle_scaling = custom;
    let mut on = resting_box_world(&def);
    on.step(1.0 / 60.0, 4);
    let off_def = default_world_def();
    let mut off = resting_box_world(&off_def);
    off.step(1.0 / 60.0, 4);

    let image = |w: &World| {
        let mut buf = RecBuffer::new();
        let mut reg = GeometryRegistry::new();
        serialize_world(w, &mut buf, &mut reg);
        (buf.data, reg.to_slots())
    };
    let (img_on, mut slots_on) = image(&on);
    let (img_off, mut slots_off) = image(&off);
    assert_eq!(img_on.len(), img_off.len() + 13, "13 parameter bytes only when enabled");

    // Restore into a default (disabled) world: the toggle and parameters come from the image.
    let mut fresh = World::new(&default_world_def());
    assert!(deserialize_into_shell(&img_on, &mut fresh, &mut slots_on));
    assert_eq!(world_get_contact_recycle_scaling(&fresh), custom);

    // A disabled image turns scaling off in a world that had it on.
    let mut shell = World::new(&def);
    world_set_contact_recycle_scaling(&mut shell, custom);
    assert!(deserialize_into_shell(&img_off, &mut shell, &mut slots_off));
    assert!(!world_get_contact_recycle_scaling(&shell).enabled);
}

/// Recordings seed replay from a frame-0 world snapshot (and keyframes are snapshots too), so a
/// world created with scaling on replays with the same parameters. Only a mid-recording call to
/// `world_set_contact_recycle_scaling` is not captured as an op.
#[test]
fn recording_replays_scaling_from_world_def() {
    use crate::recording::{validate_replay, RecPlayer, Recording};
    use crate::world::{world_start_recording, world_stop_recording};

    let custom = ContactRecycleScaling {
        enabled: true,
        length_fraction: 0.2,
        min_distance: 0.001,
        max_distance: 0.4,
        combine: RecycleLengthCombine::Min,
    };
    let run = |scaling: ContactRecycleScaling, record: bool| {
        let s = 0.01f32;
        let mut def = default_world_def();
        def.contact_recycle_scaling = scaling;
        let mut world = World::new(&def);
        if record {
            world_start_recording(&mut world, Recording::new(0));
        }
        let mut bd = default_body_def();
        bd.type_ = BodyType::Static;
        bd.position = Pos { x: (-10.0 * s) as _, y: s as _, z: 0.0 };
        let g = create_body(&mut world, &bd);
        create_hull_shape(&mut world, g, &default_shape_def(), &make_box_hull(10.0 * s, s, 10.0 * s).base);
        let mut bd = default_body_def();
        bd.type_ = BodyType::Dynamic;
        bd.enable_sleep = false;
        bd.position = Pos { x: (-6.0 * s) as _, y: (2.5 * s) as _, z: 0.0 };
        bd.linear_velocity = Vec3 { x: 2.0 * s, y: 0.0, z: 0.0 };
        let b = create_body(&mut world, &bd);
        let mut sd = default_shape_def();
        sd.base_material.friction = 0.0;
        create_hull_shape(&mut world, b, &sd, &make_box_hull(0.5 * s, 0.5 * s, 0.5 * s).base);
        for _ in 0..240 {
            world.step(1.0 / 60.0, 4);
        }
        let p = body_get_position(&world, b);
        let rec = if record { world_stop_recording(&mut world) } else { None };
        (p, b, rec)
    };

    let (p_scaled, b, rec) = run(custom, true);
    let (p_fixed, _, _) = run(ContactRecycleScaling::disabled(), false);
    // The parameters matter for this scene, so a replay that dropped them would land elsewhere.
    assert_ne!(p_scaled.x, p_fixed.x);

    let rec = rec.expect("recording active");
    assert!(validate_replay(rec.data(), 1));
    let mut player = RecPlayer::create(rec.data(), 1).expect("player");
    assert_eq!(world_get_contact_recycle_scaling(player.world()), custom);
    while player.step_frame() {}
    let p_replay = body_get_position(player.world(), b);
    assert_eq!((p_replay.x, p_replay.y, p_replay.z), (p_scaled.x, p_scaled.y, p_scaled.z));
}
