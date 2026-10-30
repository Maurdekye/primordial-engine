//! Thin box3d-rust helpers that compile in both precision modes (`Pos` is `f32` or `f64`).

use box3d_rust::body::{
    body_get_angular_velocity, body_get_linear_velocity, body_get_transform, body_is_awake,
    body_set_target_transform, create_body,
};
use box3d_rust::geometry::Sphere;
use box3d_rust::hull::make_box_hull;
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::{Pos, Quat, Vec3, WorldTransform, QUAT_IDENTITY};
use box3d_rust::recording::{
    deserialize_into_shell, serialize_world, GeometryRegistry, RecBuffer, RegistrySlot,
};
use box3d_rust::shape::{create_hull_shape, create_sphere_shape};
use box3d_rust::types::{default_body_def, default_shape_def, default_world_def, BodyType, WorldDef};
use box3d_rust::world::World;

pub const DT: f32 = 1.0 / 60.0;
pub const SUBSTEPS: i32 = 4;

pub fn mode() -> &'static str {
    if box3d_rust::core::is_double_precision() { "f64" } else { "f32" }
}

pub fn pos(v: [f64; 3]) -> Pos {
    Pos { x: v[0] as _, y: v[1] as _, z: v[2] as _ }
}

pub fn pos_f64(p: Pos) -> [f64; 3] {
    [p.x as f64, p.y as f64, p.z as f64]
}

pub fn v3(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

/// A world whose length unit is `unit` metres: callers pass every length divided by `unit`.
/// `gravity` is in m/s^2; `invariant` keeps gravity at 10 units/s^2 instead (scale-invariant
/// dynamics). The speed cap is box3d's default 400 m/s expressed in units.
pub fn world_def(unit: f64, gravity: f64, invariant: bool) -> WorldDef {
    let mut d = default_world_def();
    let g = if invariant { gravity } else { gravity / unit };
    d.gravity = v3(0.0, -(g as f32), 0.0);
    d.maximum_linear_speed = if invariant { 400.0 } else { (400.0 / unit).min(1.0e9) as f32 };
    d
}

pub fn static_box(world: &mut World, center: [f64; 3], h: [f32; 3]) -> BodyId {
    let mut d = default_body_def();
    d.type_ = BodyType::Static;
    d.position = pos(center);
    let b = create_body(world, &d);
    let hull = make_box_hull(h[0], h[1], h[2]);
    create_hull_shape(world, b, &default_shape_def(), &hull.base);
    b
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    Ball(f32),
    Cube(f32),
}

pub struct BodyInit {
    pub center: [f64; 3],
    pub rot: Quat,
    pub lin: Vec3,
    pub ang: Vec3,
    pub awake: bool,
    pub kinematic: bool,
    pub sleep_threshold: f32,
}

impl BodyInit {
    pub fn at(center: [f64; 3]) -> Self {
        BodyInit {
            center,
            rot: QUAT_IDENTITY,
            lin: v3(0.0, 0.0, 0.0),
            ang: v3(0.0, 0.0, 0.0),
            awake: true,
            kinematic: false,
            sleep_threshold: 0.05,
        }
    }
}

pub fn make_body(world: &mut World, init: &BodyInit, shape: Shape) -> BodyId {
    let mut d = default_body_def();
    d.type_ = if init.kinematic { BodyType::Kinematic } else { BodyType::Dynamic };
    d.position = pos(init.center);
    d.rotation = init.rot;
    d.linear_velocity = init.lin;
    d.angular_velocity = init.ang;
    d.is_awake = init.awake;
    d.sleep_threshold = init.sleep_threshold;
    d.linear_damping = 0.1;
    d.angular_damping = 0.3;
    let b = create_body(world, &d);
    let mut sd = default_shape_def();
    sd.density = 1.0;
    match shape {
        Shape::Ball(r) => {
            let s = Sphere { center: v3(0.0, 0.0, 0.0), radius: r };
            create_sphere_shape(world, b, &sd, &s);
        }
        Shape::Cube(h) => {
            let hull = make_box_hull(h, h, h);
            create_hull_shape(world, b, &sd, &hull.base);
        }
    }
    b
}

/// The full dynamic state of one body as exact bit patterns (what a dormant record keeps).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyState {
    pub p: [f64; 3],
    pub q: [f32; 4],
    pub v: [f32; 3],
    pub w: [f32; 3],
    pub awake: bool,
}

pub fn read_state(world: &World, b: BodyId) -> BodyState {
    let t = body_get_transform(world, b);
    let v = body_get_linear_velocity(world, b);
    let w = body_get_angular_velocity(world, b);
    BodyState {
        p: pos_f64(t.p),
        q: [t.q.v.x, t.q.v.y, t.q.v.z, t.q.s],
        v: [v.x, v.y, v.z],
        w: [w.x, w.y, w.z],
        awake: body_is_awake(world, b),
    }
}

pub fn set_kinematic_target(world: &mut World, b: BodyId, center: [f64; 3]) {
    body_set_target_transform(world, b, WorldTransform { p: pos(center), q: QUAT_IDENTITY }, DT, true);
}

/// Saves and restores whole box3d worlds (the only bit-exact restore box3d-rust offers; partial
/// restores diverge, design D51).
pub struct Snapshotter {
    registry: GeometryRegistry,
    slots: Vec<RegistrySlot>,
    slot_count: usize,
}

impl Default for Snapshotter {
    fn default() -> Self {
        Self::new()
    }
}

impl Snapshotter {
    pub fn new() -> Self {
        Snapshotter { registry: GeometryRegistry::new(), slots: Vec::new(), slot_count: 0 }
    }

    pub fn save(&mut self, world: &World) -> Vec<u8> {
        let mut buf = RecBuffer::new();
        serialize_world(world, &mut buf, &mut self.registry);
        if self.registry.entries.len() != self.slot_count {
            self.slots = self.registry.to_slots();
            self.slot_count = self.registry.entries.len();
        }
        buf.data
    }

    pub fn restore(&mut self, world: &mut World, image: &[u8]) {
        assert!(deserialize_into_shell(image, world, &mut self.slots), "restore failed");
    }
}

pub fn new_world(def: &WorldDef) -> World {
    World::new(def)
}

/// A pile like the phase-1 b6 benchmark (`sim-core::build_pile_scene`), shifted by `offset`:
/// boxes and spheres of 0.4-0.45 m on a 40 m ground slab, `side * side * layers` bodies.
pub fn build_pile(world: &mut World, side: usize, layers: usize, offset: [f64; 3]) -> Vec<BodyId> {
    static_box(world, [offset[0], offset[1] - 1.0, offset[2]], [40.0, 1.0, 40.0]);
    let mut bodies = Vec::with_capacity(side * side * layers);
    for layer in 0..layers {
        for ix in 0..side {
            for iz in 0..side {
                let n = bodies.len();
                let jitter = (n as f64 * 0.618_034).fract() * 0.2 - 0.1;
                let c = [
                    offset[0] + (ix as f64 - side as f64 * 0.5) * 1.1 + jitter,
                    offset[1] + 0.5 + layer as f64 * 1.2,
                    offset[2] + (iz as f64 - side as f64 * 0.5) * 1.1 - jitter,
                ];
                let shape = if n % 2 == 0 { Shape::Cube(0.4) } else { Shape::Ball(0.45) };
                bodies.push(make_body(world, &BodyInit::at(c), shape));
            }
        }
    }
    bodies
}

pub const BALL_R: f32 = 0.06;
pub const HAND_R: f32 = 0.09;

/// One player's ball pit (`balls` balls of 6 cm in a 2.4 m pit) plus a kinematic hand, centred on
/// `c`. Returns (balls, hand).
pub fn build_ballpit(world: &mut World, balls: usize, c: [f64; 3]) -> (Vec<BodyId>, BodyId) {
    let (w, h) = (1.2f32, 0.6f32);
    let at = |x: f64, y: f64, z: f64| [c[0] + x, c[1] + y, c[2] + z];
    static_box(world, at(0.0, -0.5, 0.0), [2.0, 0.5, 2.0]);
    static_box(world, at((w + 0.1) as f64, h as f64, 0.0), [0.1, h, w + 0.2]);
    static_box(world, at(-(w + 0.1) as f64, h as f64, 0.0), [0.1, h, w + 0.2]);
    static_box(world, at(0.0, h as f64, (w + 0.1) as f64), [w + 0.2, h, 0.1]);
    static_box(world, at(0.0, h as f64, -(w + 0.1) as f64), [w + 0.2, h, 0.1]);
    let per_row = 16usize;
    let spacing = 2.0 * w as f64 / per_row as f64;
    let mut out = Vec::with_capacity(balls);
    for i in 0..balls {
        let (ix, iz, iy) = (i % per_row, (i / per_row) % per_row, i / (per_row * per_row));
        let jitter = (i as f64 * 0.618_034).fract() * 0.02 - 0.01;
        let p = at(
            -w as f64 + spacing * (ix as f64 + 0.5) + jitter,
            BALL_R as f64 + 0.01 + iy as f64 * (2.0 * BALL_R as f64 + 0.01),
            -w as f64 + spacing * (iz as f64 + 0.5) - jitter,
        );
        out.push(make_body(world, &BodyInit::at(p), Shape::Ball(BALL_R)));
    }
    let mut hi = BodyInit::at(at(0.0, 1.5, 0.0));
    hi.kinematic = true;
    let hand = make_body(world, &hi, Shape::Ball(HAND_R));
    (out, hand)
}

/// Hand target at frame `f` for a pit centred on `c`: descend, then sweep a sine path through
/// the balls forever (period 3 s), so the pit never settles. `phase` desynchronizes pits.
pub fn hand_target(c: [f64; 3], f: u64, phase: f64) -> [f64; 3] {
    let t = f as f64 / 60.0 + phase;
    let u = (t / 3.0).fract();
    let x = if u < 0.5 { -0.9 + 3.6 * u } else { 0.9 - 3.6 * (u - 0.5) };
    // Deterministic triangle-wave approximation of a sine (no transcendental in sim code).
    let tri = |s: f64| {
        let s = s.fract();
        if s < 0.5 { 4.0 * s - 1.0 } else { 3.0 - 4.0 * s }
    };
    let z = 0.6 * tri(1.5 * u);
    [c[0] + x, c[1] + 0.2, c[2] + z]
}
