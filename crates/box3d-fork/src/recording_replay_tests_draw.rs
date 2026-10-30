// Replay debug-draw tests ported from test_recording.c (DebugShapeCallbacks).
//
// `include!`d into `recording_replay_tests.rs`, so it shares that module's imports.
//
// SPDX-FileCopyrightText: 2026 Erin Catto
// SPDX-License-Identifier: MIT

/// The 3D sample renderer builds per-shape GPU meshes through createDebugShape, so the replay
/// world must carry the host callbacks. Verify wiring the debug-shape callbacks fires them for
/// every replayed shape via `b3World_Draw`'s lazy path, re-fires them after a backward-seek
/// world re-seed, and drives the destroy callback at teardown. (DebugShapeCallbacks)
#[test]
fn debug_shape_callbacks() {
    use crate::debug_draw::{DebugDraw, DebugShape, HexColor};
    use crate::dynamic_tree::DEFAULT_MASK_BITS;
    use crate::math_functions::{Aabb, WorldTransform};
    use crate::recording::RecPlayer;
    use crate::world::world_draw;
    use std::cell::Cell;

    // Host debug-shape callbacks just count create/destroy so the test can prove the player
    // wires them into the world it replays. The returned token is opaque to the engine.
    struct DebugShapeCounters {
        created: Cell<i32>,
        destroyed: Cell<i32>,
    }

    fn rec_test_create_debug_shape(_shape: &DebugShape<'_>, ctx: u64) -> u64 {
        // SAFETY: `ctx` is the address of a `DebugShapeCounters` that outlives every draw and
        // teardown call in this test (mirrors C's `void* userContext`).
        let counters = unsafe { &*(ctx as *const DebugShapeCounters) };
        counters.created.set(counters.created.get() + 1);
        1 // any non-zero token; the engine stores it and hands it back to destroy
    }

    fn rec_test_destroy_debug_shape(_user_shape: u64, ctx: u64) {
        // SAFETY: see rec_test_create_debug_shape.
        let counters = unsafe { &*(ctx as *const DebugShapeCounters) };
        counters.destroyed.set(counters.destroyed.get() + 1);
    }

    // A no-op-shaped DrawShape (the void DrawShapeFcn) that just counts how many shapes the draw
    // pass emitted, so we can prove the lazy createDebugShape path actually reached the renderer.
    #[derive(Default)]
    struct RecTestDraw {
        draws: usize,
    }

    impl DebugDraw for RecTestDraw {
        fn draw_shape(&mut self, _user_shape: u64, _transform: WorldTransform, _color: HexColor) {
            self.draws += 1;
        }

        fn drawing_bounds(&self) -> Aabb {
            let big = 1.0e6;
            Aabb {
                lower_bound: Vec3 {
                    x: -big,
                    y: -big,
                    z: -big,
                },
                upper_bound: Vec3 {
                    x: big,
                    y: big,
                    z: big,
                },
            }
        }

        fn draw_shapes(&self) -> bool {
            true
        }
    }

    // Build the recording: gravity, a static ground box, and four dynamic boxes sharing one hull.
    // Ground + 4 boxes = 5 shapes total, matching the C fixture.
    let rec = Recording::new(0);
    let mut world = World::new(&default_world_def());
    world_start_recording(&mut world, rec);
    world_set_gravity(
        &mut world,
        Vec3 {
            x: 0.0,
            y: -10.0,
            z: 0.0,
        },
    );

    let mut ground_def = default_body_def();
    ground_def.type_ = BodyType::Static;
    let ground_id = create_body(&mut world, &ground_def);
    let ground_box = make_box_hull(20.0, 1.0, 20.0);
    create_hull_shape(&mut world, ground_id, &default_shape_def(), &ground_box.base);

    let box_hull = make_box_hull(0.5, 0.5, 0.5);
    let mut box_shape = default_shape_def();
    box_shape.density = 1.0;
    for i in 0..4 {
        let mut body_def = default_body_def();
        body_def.type_ = BodyType::Dynamic;
        body_def.position = Pos {
            x: 0.0,
            y: (1.0 + 1.1 * i as f32) as _,
            z: 0.0,
        };
        let body_id = create_body(&mut world, &body_def);
        create_hull_shape(&mut world, body_id, &box_shape, &box_hull.base);
    }

    let total_frames = 30;
    for _ in 0..total_frames {
        world.step(1.0 / 60.0, 4);
    }
    let rec = world_stop_recording(&mut world).expect("recording active");

    // Rust has no b3SaveRecordingToFile; sibling player tests replay from `rec.data()` in-memory.
    // `counters` is declared before `player` so the player (and its world holding the context
    // pointer) is dropped first, keeping the pointed-to counters alive for every callback.
    let counters = DebugShapeCounters {
        created: Cell::new(0),
        destroyed: Cell::new(0),
    };
    let mut player = RecPlayer::create(rec.data(), 1).expect("player");

    // Wiring the callbacks rebuilds the world and rewinds to frame 0.
    player.set_debug_shape_callbacks(
        Some(rec_test_create_debug_shape),
        Some(rec_test_destroy_debug_shape),
        &counters as *const DebugShapeCounters as u64,
    );
    assert_eq!(player.get_frame(), 0);

    // Replay to the end, then draw: createDebugShape fires once per shape (ground + 4 boxes = 5).
    while !player.is_at_end() {
        player.step_frame();
    }
    let mut draw = RecTestDraw::default();
    world_draw(player.world_mut(), &mut draw, DEFAULT_MASK_BITS);
    assert!(
        counters.created.get() >= 5,
        "createDebugShape should fire once per shape, got {}",
        counters.created.get()
    );
    assert!(
        draw.draws >= 5,
        "DrawShape should fire for every created shape, got {}",
        draw.draws
    );
    assert!(!player.has_diverged());

    // A backward seek re-seeds the world in place to the empty frame-0 seed. Because the seed has
    // no shapes, nothing carries over and every live debug-shape handle is released through
    // destroyDebugShape (mirroring b3DesShapes' teardown sweep). Forward stepping then recreates
    // the shapes, and drawing re-fires createDebugShape: more creates AND more destroys.
    let created_before = counters.created.get();
    let destroyed_before = counters.destroyed.get();
    player.seek_frame(0);
    assert!(
        counters.destroyed.get() > destroyed_before,
        "backward-seek re-seed should release the live debug-shape handles: {} !> {}",
        counters.destroyed.get(),
        destroyed_before
    );
    player.seek_frame(total_frames);
    let mut draw2 = RecTestDraw::default();
    world_draw(player.world_mut(), &mut draw2, DEFAULT_MASK_BITS);
    assert!(
        counters.created.get() > created_before,
        "backward-seek re-seed should recreate shapes: {} !> {}",
        counters.created.get(),
        created_before
    );

    // Teardown destroys the final world (b3DestroyPlayer -> b3DestroyWorld); every live shape is
    // released through destroyDebugShape, so the create/destroy counts balance exactly.
    assert!(counters.created.get() >= 10);
    drop(player);
    assert_eq!(counters.created.get(), counters.destroyed.get());

    drop(world);
    // `counters` outlives `player`; keep it referenced until here for clarity.
    let _ = &counters;
}

/// Re-wiring the debug-shape callbacks destroys the current replay world (b3DestroyWorld inside
/// b3RecPlayer_SetDebugShapeCallbacks), releasing every live debug-shape handle, then rebuilds
/// frame 0 under the new wiring.
#[test]
fn debug_shape_callbacks_rewire_releases_old_world() {
    use crate::debug_draw::{DebugDraw, DebugShape, HexColor};
    use crate::dynamic_tree::DEFAULT_MASK_BITS;
    use crate::math_functions::{Aabb, WorldTransform};
    use crate::recording::RecPlayer;
    use crate::world::world_draw;
    use std::sync::atomic::{AtomicI32, Ordering};

    struct Counters {
        created: AtomicI32,
        destroyed: AtomicI32,
    }

    fn create_cb(_shape: &DebugShape<'_>, ctx: u64) -> u64 {
        // SAFETY: `ctx` is the address of a `Counters` that outlives the player.
        let c = unsafe { &*(ctx as *const Counters) };
        c.created.fetch_add(1, Ordering::Relaxed);
        1
    }

    fn destroy_cb(_user_shape: u64, ctx: u64) {
        // SAFETY: see create_cb.
        let c = unsafe { &*(ctx as *const Counters) };
        c.destroyed.fetch_add(1, Ordering::Relaxed);
    }

    struct AllDraw;
    impl DebugDraw for AllDraw {
        fn draw_shape(&mut self, _user_shape: u64, _transform: WorldTransform, _color: HexColor) {}
        fn drawing_bounds(&self) -> Aabb {
            let big = 1.0e6;
            Aabb {
                lower_bound: Vec3 {
                    x: -big,
                    y: -big,
                    z: -big,
                },
                upper_bound: Vec3 {
                    x: big,
                    y: big,
                    z: big,
                },
            }
        }
        fn draw_shapes(&self) -> bool {
            true
        }
    }

    let mut world = recorded_sphere_world();
    let rec = world_stop_recording(&mut world).expect("recording active");

    let counters = Counters {
        created: AtomicI32::new(0),
        destroyed: AtomicI32::new(0),
    };
    let ctx = &counters as *const Counters as u64;
    let mut player = RecPlayer::create(rec.data(), 1).expect("player");
    player.set_debug_shape_callbacks(Some(create_cb), Some(destroy_cb), ctx);

    for _ in 0..10 {
        player.step_frame();
    }
    world_draw(player.world_mut(), &mut AllDraw, DEFAULT_MASK_BITS);
    let created = counters.created.load(Ordering::Relaxed);
    assert_eq!(created, 2, "ground + sphere");
    assert_eq!(counters.destroyed.load(Ordering::Relaxed), 0);

    // Re-wiring tears down the stepped world: both live handles are released, and the player
    // rewinds to frame 0.
    player.set_debug_shape_callbacks(Some(create_cb), Some(destroy_cb), ctx);
    assert_eq!(counters.destroyed.load(Ordering::Relaxed), created);
    assert_eq!(player.get_frame(), 0);
    assert!(!player.is_at_end());

    // The rebuilt world replays to the end without diverging.
    while !player.is_at_end() {
        player.step_frame();
    }
    assert!(!player.has_diverged());
    drop(player);
}
