//! Bullets against meshes (world-model.md 3.2; CCD part 2 E): the user's paddle case, static mesh walls at 30, 60
//! and 120 Hz, a fast slide across a dense mesh's inner edges, bullets resting and rolling on a mesh, and a scene of
//! all of them on any worker count and through a restore.
//!
//! The user: "even if the paddles use a mesh collider, setting the ball to be a bullet should be
//! enough to maintain continuous collisions".

use pe_physics::{BodyDesc, BodyKind, Geometry, Mesh, ObjectId, PhysicsConfig, PhysicsWorld, Pose, ShapeDesc};

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((11u128 << 32) | u128::from(n))
}

/// A box's 8 corners and 12 triangles, each counter-clockwise seen from outside: half extents `h`, centred at `c`.
fn box_parts(c: [f32; 3], h: [f32; 3]) -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
    let vertices = (0..8u32)
        .map(|k| {
            let s = |bit: u32| if k & bit != 0 { 1.0 } else { -1.0 };
            [c[0] + s(1) * h[0], c[1] + s(2) * h[1], c[2] + s(4) * h[2]]
        })
        .collect();
    let triangles = vec![
        [0, 2, 1],
        [1, 2, 3],
        [4, 5, 6],
        [5, 7, 6],
        [0, 1, 4],
        [1, 5, 4],
        [2, 6, 3],
        [3, 6, 7],
        [0, 4, 2],
        [2, 4, 6],
        [1, 3, 5],
        [3, 7, 5],
    ];
    (vertices, triangles)
}

fn closed_box(c: [f32; 3], h: [f32; 3]) -> Mesh {
    let (v, t) = box_parts(c, h);
    Mesh::new(&v, &t).unwrap()
}

fn mesh_shape(mesh: Mesh, both_sides: bool) -> ShapeDesc {
    ShapeDesc::new(Geometry::Mesh { mesh, scale: [1.0; 3], both_sides })
}

/// A world with no gravity ticking at `hz` (240 / `hz` sub-steps, each 1/240 s).
fn weightless(hz: u32) -> PhysicsWorld {
    let config = PhysicsConfig { gravity: [0.0; 3], ..PhysicsConfig::engine() }.with_tick_rate(hz).unwrap();
    PhysicsWorld::new(config).unwrap()
}

/// Never damped and never put to sleep, so a body's velocity changes only at a contact.
fn undamped(mut desc: BodyDesc) -> BodyDesc {
    desc.linear_damping = 0.0;
    desc.angular_damping = 0.0;
    desc.enable_sleep = false;
    desc
}

/// A half-elastic steel ball of `radius` moving at `velocity`; a bullet if `bullet`.
fn ball(at: [f64; 3], radius: f32, velocity: [f32; 3], bullet: bool) -> BodyDesc {
    let sphere = Geometry::Sphere { center: [0.0; 3], radius };
    let mut desc = BodyDesc::new(BodyKind::Dynamic, at)
        .with_shape(ShapeDesc::new(sphere).with_density(7800.0).with_restitution(0.5))
        .with_velocity(velocity, [0.0; 3]);
    desc.bullet = bullet;
    undamped(desc)
}

/// The paddle: a bar 2 m long (x), 20 cm tall (y) and 5 cm thick (z) turning about y through its centre at the
/// origin, so each end is 1 m out (60 m/s at 60 rad/s).
const HALF: [f64; 3] = [1.0, 0.1, 0.025];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Paddle {
    /// A closed box mesh on a kinematic body (a held paddle is one).
    Kinematic,
    /// The same closed box mesh on a dynamic body, 2,000 kg/m³ (40 kg), spinning freely.
    Dynamic,
    /// An open blade with both sides, two triangles at z = 0, on a kinematic body.
    Blade,
}

/// The paddle `kind` turned `theta` about y, turning at `omega` rad/s about y. Also the turn its quaternion holds.
fn paddle(kind: Paddle, theta: f64, omega: f32) -> (BodyDesc, f64) {
    let h = HALF.map(|x| x as f32);
    let (body, shape) = match kind {
        Paddle::Kinematic => (BodyKind::Kinematic, mesh_shape(closed_box([0.0; 3], h), false)),
        Paddle::Dynamic => (BodyKind::Dynamic, mesh_shape(closed_box([0.0; 3], h), false).with_density(2000.0)),
        Paddle::Blade => {
            let v = [[-h[0], -h[1], 0.0], [h[0], -h[1], 0.0], [h[0], h[1], 0.0], [-h[0], h[1], 0.0]];
            (BodyKind::Kinematic, mesh_shape(Mesh::new(&v, &[[0, 1, 2], [0, 2, 3]]).unwrap(), true))
        }
    };
    let (s, c) = (theta / 2.0).sin_cos();
    let (s, c) = (s as f32, c as f32);
    let desc = BodyDesc::new(body, [0.0; 3])
        .with_shape(shape)
        .with_rotation([0.0, s, 0.0, c])
        .with_velocity([0.0; 3], [0.0, omega, 0.0]);
    (undamped(desc), 2.0 * f64::from(s).atan2(f64::from(c)))
}

/// How far `q` is outside the paddle's box (0 inside or on it), in the paddle's frame.
fn outside(q: [f64; 3]) -> f64 {
    let d = |i: usize| (q[i].abs() - HALF[i]).max(0.0);
    (d(0) * d(0) + d(1) * d(1) + d(2) * d(2)).sqrt()
}

/// `p` in the frame of a body at `pose`.
fn local(pose: Pose, p: [f64; 3]) -> [f64; 3] {
    let d = [p[0] - pose.position[0], p[1] - pose.position[1], p[2] - pose.position[2]];
    let [x, y, z, w] = pose.rotation.map(f64::from);
    // The inverse rotation: v + 2 w (u x v) + 2 u x (u x v), with u = -(x, y, z).
    let u = [-x, -y, -z];
    let cross =
        |a: [f64; 3], b: [f64; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let c1 = cross(u, d);
    let c2 = cross(u, c1);
    [d[0] + 2.0 * (w * c1[0] + c2[0]), d[1] + 2.0 * (w * c1[1] + c2[1]), d[2] + 2.0 * (w * c1[2] + c2[2])]
}

/// How fast Box3D turns a body set turning at `omega` rad/s about one axis, in a world ticking at `hz`: at most a
/// quarter of pi a frame (the fork's `MAX_ROTATION * inv_dt`, integrate.rs; pe-physics never allows a body fast
/// rotation), so 23.6 rad/s at 30 Hz, 47.1 at 60 and 94.2 at 120, then each sub-step of h seconds by its quaternion
/// step, 2 atan(h ω / 2).
fn turn_rate(omega: f32, hz: u32) -> f64 {
    let dt = 1.0 / hz as f32;
    let cap = 0.25 * std::f32::consts::PI * (1.0 / dt);
    let applied = if omega > cap { cap / omega * omega } else { omega };
    let h = f64::from(dt / 240u32.div_ceil(hz) as f32);
    2.0 * (h * f64::from(applied) / 2.0).atan() / h
}

/// What a ball's straight flight does against the paddle turning at `rate` ([`turn_rate`]), judged on a fine clock
/// (2 mm of relative motion a sample).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ghost {
    /// The ball's centre crosses the blade's mid-plane at time `t`, well inside the face (2 r in from its edges, and
    /// 2 cm more at the ends for the clock): it can't miss the paddle. Its first touch is on a face, not an end or an
    /// edge, and `side` is that face: the sign of the ball's z in the paddle's frame then.
    Meets { t: f64, side: f64 },
    /// Its centre stays more than r + 5 cm from the paddle's box, beyond Box3D's 2 cm speculative reach: nothing may
    /// touch it.
    Misses,
    /// Anything between, not judged: it comes within 5 cm without crossing inside the face, or it first touches an
    /// end or an edge.
    Grazes,
}

fn ghost(start: [f64; 3], v: [f64; 3], theta: f64, rate: f64, horizon: f64, r: f64) -> Ghost {
    let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let tick = 0.002 / (speed + rate.abs() * HALF[0]);
    let mut last: Option<[f64; 3]> = None;
    let mut nearest = f64::INFINITY;
    // The face the ball first touches (0 for an end or an edge), once it does.
    let mut touch: Option<f64> = None;
    for k in 0..=(horizon / tick).ceil() as u64 {
        let t = k as f64 * tick;
        let p = [start[0] + v[0] * t, start[1] + v[1] * t, start[2] + v[2] * t];
        let (s, c) = (theta + rate * t).sin_cos();
        let q = [c * p[0] - s * p[2], p[1], s * p[0] + c * p[2]];
        nearest = nearest.min(outside(q));
        if touch.is_none() && outside(q) <= r {
            touch = Some(if q[0].abs() < HALF[0] && q[1].abs() < HALF[1] { q[2].signum() } else { 0.0 });
        }
        if let Some(l) = last
            && (l[2] > 0.0) != (q[2] > 0.0)
        {
            let f = l[2] / (l[2] - q[2]);
            let (x, y) = (l[0] + f * (q[0] - l[0]), l[1] + f * (q[1] - l[1]));
            if x.abs() < HALF[0] - 2.0 * r - 0.02 && y.abs() < HALF[1] - 2.0 * r {
                return match touch {
                    Some(side) if side != 0.0 => Ghost::Meets { t: t - (1.0 - f) * tick, side },
                    _ => Ghost::Grazes,
                };
            }
        }
        last = Some(q);
    }
    if nearest > r + 0.05 { Ghost::Misses } else { Ghost::Grazes }
}

/// One shot at the paddle.
#[derive(Clone, Copy, Debug)]
struct Shot {
    kind: Paddle,
    hz: u32,
    omega: f32,
    speed: f32,
    /// The ball's line runs along x at this distance from the turning axis.
    offset: f64,
    /// +1 runs with the paddle's near face, -1 against it.
    way: f32,
    height: f64,
    /// The paddle's turn at the start, radians.
    theta: f64,
}

const R: f32 = 0.02;

/// Box3D's linear slop: how far a resting contact may overlap.
const SLOP: f64 = 0.005;

/// What one shot did.
struct Fired {
    /// The frame the ball's velocity first changed in, if it did.
    changed: Option<u32>,
    /// Its centre ended a frame inside the closed paddle.
    inside: bool,
    /// From that change until it was clear (more than r + 5 cm from the paddle's box), its centre ended a frame within
    /// the face's footprint and more than a slop past the face it met (the near face for the closed paddle, the
    /// plane for the blade): hit, it went in or through, and could leave by the far side (@design-reviewer's M1 on
    /// part 2 E: the failure the sweep's fix for pairs that meet during a step is about).
    crossed: bool,
}

impl Shot {
    fn frames(&self) -> u32 {
        (3.2 / f64::from(self.speed) * f64::from(self.hz)).ceil() as u32 + 1
    }

    fn start(&self) -> ([f64; 3], [f32; 3]) {
        ([-1.6 * f64::from(self.way), self.height, self.offset], [self.speed * self.way, 0.0, 0.0])
    }

    fn ghost(&self) -> Ghost {
        let (start, v) = self.start();
        let (_, theta) = paddle(self.kind, self.theta, self.omega);
        let horizon = f64::from(self.frames()) / f64::from(self.hz);
        ghost(start, v.map(f64::from), theta, turn_rate(self.omega, self.hz), horizon, f64::from(R))
    }

    /// Fire the shot, a bullet or not. `side` is the face the ball meets, if it meets the paddle ([`Ghost::Meets`]).
    fn fire(&self, bullet: bool, side: Option<f64>) -> Fired {
        let mut pw = weightless(self.hz);
        pw.add_body(id(1), &paddle(self.kind, self.theta, self.omega).0).unwrap();
        let (start, v) = self.start();
        pw.add_body(id(2), &ball(start, R, v, bullet)).unwrap();
        let v0 = pw.velocity(id(2)).unwrap().0;
        let mut fired = Fired { changed: None, inside: false, crossed: false };
        let mut watching = false;
        let least = if self.kind == Paddle::Blade { -SLOP } else { HALF[2] - SLOP };
        for frame in 1..=self.frames() {
            pw.step(1.0 / self.hz as f32).unwrap();
            if fired.changed.is_none() && pw.velocity(id(2)).unwrap().0 != v0 {
                fired.changed = Some(frame);
                watching = side.is_some();
            }
            let q = local(pw.pose(id(1)).unwrap(), pw.pose(id(2)).unwrap().position);
            if self.kind != Paddle::Blade {
                fired.inside |= outside(q) == 0.0;
            }
            if watching {
                if outside(q) > f64::from(R) + 0.05 {
                    watching = false;
                } else if let Some(side) = side
                    && q[0].abs() <= HALF[0]
                    && q[1].abs() <= HALF[1]
                {
                    fired.crossed |= q[2] * side < least;
                }
            }
        }
        fired
    }
}

/// The shots: each paddle at 30, 60 and 120 Hz, set turning at 20 and 60 rad/s (60 is capped below 120 Hz,
/// [`turn_rate`]), 2 cm balls at 10, 60 and 240 m/s on lines 0.35, 0.65 and 0.9 m from the axis, with and against
/// the near face, at the blade's middle, near its top edge and above it, at eight starting turns.
fn shots() -> Vec<Shot> {
    let mut all = Vec::new();
    for kind in [Paddle::Kinematic, Paddle::Dynamic, Paddle::Blade] {
        for hz in [30, 60, 120] {
            for omega in [20.0, 60.0] {
                for speed in [10.0, 60.0, 240.0] {
                    for offset in [0.35, 0.65, 0.9] {
                        for way in [1.0, -1.0] {
                            for height in [0.0, 0.05, 0.25] {
                                for k in 0..8 {
                                    let theta = std::f64::consts::PI * f64::from(k) / 8.0;
                                    all.push(Shot { kind, hz, omega, speed, offset, way, height, theta });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    all
}

/// The user's paddle case. A 2 cm bullet never passes a turning mesh paddle: every shot whose straight flight would
/// cross the paddle is turned by the frame after, never ends a frame inside it, and stays on the side of the face it
/// met until it is clear ([`Fired::crossed`]); every shot that stays clear is untouched, bit for bit. The same balls
/// without the flag pass through some paddles (counted, for contrast).
#[test]
fn a_bullet_never_passes_a_turning_mesh_paddle() {
    let shots = shots();
    let (mut meets, mut misses, mut grazes) = (0, 0, 0);
    let mut failed = Vec::new();
    let mut unflagged_passed = 0;
    for shot in &shots {
        let ghost = shot.ghost();
        let side = if let Ghost::Meets { side, .. } = ghost { Some(side) } else { None };
        for bullet in [true, false] {
            let Fired { changed, inside, crossed } = shot.fire(bullet, side);
            match ghost {
                Ghost::Meets { t, .. } => {
                    let by = (t * f64::from(shot.hz)).ceil() as u32 + 1;
                    let turned = changed.is_some_and(|f| f <= by) && !inside && !crossed;
                    if bullet {
                        meets += 1;
                        if !turned {
                            failed.push(format!(
                                "{shot:?}: meets at {t:.4} s, changed {changed:?}, inside {inside}, crossed {crossed}"
                            ));
                        }
                    } else if !turned {
                        unflagged_passed += 1;
                    }
                }
                Ghost::Misses => {
                    misses += usize::from(bullet);
                    if changed.is_some() {
                        failed.push(format!("{shot:?} (bullet {bullet}): misses, but changed at frame {changed:?}"));
                    }
                }
                Ghost::Grazes => grazes += usize::from(bullet),
            }
        }
    }
    println!(
        "COUNTS mesh paddles: {} shots; bullets: {meets} meetings all turned, {misses} misses untouched, {grazes} \
         grazes; without the flag, {unflagged_passed} of the {meets} meetings passed through",
        shots.len()
    );
    assert!(failed.is_empty(), "{} failed:\n{}", failed.len(), failed[..failed.len().min(20)].join("\n"));
    assert!(meets > 1000 && misses > 500, "too few judged shots: {meets} meetings, {misses} misses");
    assert!(unflagged_passed > 0, "no unflagged ball passed: the paddle case no longer needs the flag");
}

/// Box3D turns a body at most a quarter of pi a frame ([`turn_rate`]), so a paddle set turning at 60 rad/s turns at
/// 23.6 rad/s at 30 Hz and 47.1 at 60, and at the full 60 only at 120 Hz. A limit of the user's paddle case, pinned
/// here so that a change to it is seen.
#[test]
fn a_paddle_turns_at_most_a_quarter_of_pi_a_frame() {
    for (hz, expect) in [(30, 23.56f32), (60, 47.12), (120, 60.0)] {
        for kind in [Paddle::Kinematic, Paddle::Dynamic] {
            let mut pw = weightless(hz);
            pw.add_body(id(1), &paddle(kind, 0.0, 60.0).0).unwrap();
            pw.step(1.0 / hz as f32).unwrap();
            let w = pw.velocity(id(1)).unwrap().1;
            assert!((w[1] - expect).abs() < 0.01, "{kind:?} at {hz} Hz turns at {w:?}");
        }
    }
}

/// A static mesh wall at x = 1, 2 m square: one-sided facing the ball (-x), a closed 1 cm box mesh (x from 1 to
/// 1.01), or both-sided.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Wall {
    Front,
    Closed,
    BothSides,
}

fn wall(kind: Wall) -> BodyDesc {
    let quad = || {
        let v = [[0.0, -1.0, -1.0], [0.0, -1.0, 1.0], [0.0, 1.0, 1.0], [0.0, 1.0, -1.0]];
        Mesh::new(&v, &[[0, 1, 2], [0, 2, 3]]).unwrap()
    };
    let shape = match kind {
        Wall::Front => mesh_shape(quad(), false),
        Wall::Closed => mesh_shape(closed_box([0.005, 0.0, 0.0], [0.005, 1.0, 1.0]), false),
        Wall::BothSides => mesh_shape(quad(), true),
    };
    BodyDesc::new(BodyKind::Static, [1.0, 0.0, 0.0]).with_shape(shape)
}

/// A bullet of `radius` fired at `speed`, `aim` radians off the wall's normal, for half a second at `hz`: from x = 0
/// (the wall's front), or from x = 2 behind it. The least and the most x its centre ends a frame at.
fn crossing(hz: u32, wall_kind: Wall, behind: bool, speed: f32, radius: f32, aim: f32) -> (f64, f64) {
    let mut pw = weightless(hz);
    pw.add_body(id(1), &wall(wall_kind)).unwrap();
    let (at, way) = if behind { (2.0, -1.0) } else { (0.0, 1.0) };
    let v = [way * speed * aim.cos(), speed * aim.sin(), 0.0];
    pw.add_body(id(2), &ball([at, 0.0, 0.0], radius, v, true)).unwrap();
    let (mut least, mut most) = (f64::INFINITY, f64::NEG_INFINITY);
    for _ in 0..hz / 2 {
        pw.step(1.0 / hz as f32).unwrap();
        let x = pw.pose(id(2)).unwrap().position[0];
        (least, most) = (least.min(x), most.max(x));
    }
    (least, most)
}

/// A bullet never crosses a static mesh wall at 30, 60 or 120 Hz: a one-sided wall facing it, a closed 1 cm box
/// mesh, and a both-sided wall from either side. A one-sided wall approached from behind lets it through, as Box3D's
/// one-sided mesh contact does (part 2 A).
#[test]
fn a_bullet_never_crosses_a_static_mesh_wall_at_30_60_or_120_hz() {
    for hz in [30, 60, 120] {
        for speed in [20.0f32, 100.0, 400.0] {
            for radius in [0.005f32, 0.03] {
                for k in 0..9 {
                    let aim = 0.15 * k as f32 - 0.6;
                    let case = format!("{hz} Hz, {speed} m/s, r {radius}, aim {aim}");
                    for kind in [Wall::Front, Wall::Closed, Wall::BothSides] {
                        let (_, most) = crossing(hz, kind, false, speed, radius, aim);
                        assert!(most < 1.0, "{case}, {kind:?}: centre at x = {most}");
                    }
                    let (least, _) = crossing(hz, Wall::BothSides, true, speed, radius, aim);
                    assert!(least > 1.0, "{case}, both sides from behind: centre at x = {least}");
                    let (least, _) = crossing(hz, Wall::Front, true, speed, radius, aim);
                    assert!(least < 1.0 - f64::from(radius), "{case}, a one-sided wall from behind held it: {least}");
                }
            }
        }
    }
}

/// A flat mesh floor of `n` x `n` cells `cell` metres wide (2 n² triangles), centred on the origin, front up.
fn grid_floor(n: u32, cell: f32) -> Mesh {
    let half = n as f32 * cell / 2.0;
    let vertices: Vec<[f32; 3]> = (0..(n + 1) * (n + 1))
        .map(|k| [(k % (n + 1)) as f32 * cell - half, 0.0, (k / (n + 1)) as f32 * cell - half])
        .collect();
    let mut triangles = Vec::new();
    for z in 0..n {
        for x in 0..n {
            let a = z * (n + 1) + x;
            triangles.extend([[a, a + n + 1, a + n + 2], [a, a + n + 2, a + 1]]);
        }
    }
    Mesh::new(&vertices, &triangles).unwrap()
}

/// A ball and a cube bullet sliding at 50 m/s across a flat mesh of 1,152 triangles, diagonally to its edges, keep to
/// it: the inner edges never bump them (part 2 A's face normal at smooth edges).
#[test]
fn bullets_sliding_fast_across_a_dense_mesh_floor_keep_to_it() {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    pw.add_body(id(1), &BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(mesh_shape(grid_floor(24, 0.5), false)))
        .unwrap();
    let cube = Geometry::Box { half_extents: [0.05; 3] };
    let sliders =
        [(0.03, ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.03 })), (0.05, ShapeDesc::new(cube))];
    for (k, (rest, shape)) in sliders.iter().enumerate() {
        let mut desc = BodyDesc::new(BodyKind::Dynamic, [-5.5, *rest, -4.0 + k as f64])
            .with_shape(shape.clone())
            .with_velocity([50.0, 0.0, 6.0], [0.0; 3]);
        desc.bullet = true;
        pw.add_body(id(2 + k as u32), &desc).unwrap();
    }
    let mut worst = 0.0f32;
    for _ in 0..12 {
        pw.step(1.0 / 60.0).unwrap();
        for (k, (rest, _)) in sliders.iter().enumerate() {
            let y = pw.pose(id(2 + k as u32)).unwrap().position[1];
            let v = pw.velocity(id(2 + k as u32)).unwrap().0;
            assert!((y - rest).abs() < 0.01, "slider {k} left the floor: at y = {y}, moving {v:?}");
            worst = worst.max(v[1].abs());
        }
    }
    assert!(worst < 0.5, "bumped: a vertical speed of {worst} m/s");
    assert!(pw.pose(id(2)).unwrap().position[0] > 3.0, "the ball should have crossed the floor");
}

/// Bullets resting and rolling on a mesh floor raise no sweep event and are never held, and move exactly as the same
/// bodies without the flag.
#[test]
fn bullets_resting_and_rolling_on_a_mesh_raise_no_sweep_event() {
    let scene = |bullet: bool| {
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        pw.add_body(
            id(1),
            &BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(mesh_shape(grid_floor(8, 0.5), false)),
        )
        .unwrap();
        let shapes = [
            (0.05, Geometry::Sphere { center: [0.0; 3], radius: 0.05 }, [0.0f32; 3]),
            (0.05, Geometry::Sphere { center: [0.0; 3], radius: 0.05 }, [1.0, 0.0, 0.3]),
            (0.04, Geometry::Box { half_extents: [0.04; 3] }, [0.0; 3]),
            (
                0.03,
                Geometry::Capsule { center1: [-0.05, 0.0, 0.0], center2: [0.05, 0.0, 0.0], radius: 0.03 },
                [0.0, 0.0, 0.5],
            ),
        ];
        for (k, (rest, geometry, v)) in shapes.into_iter().enumerate() {
            let mut desc = BodyDesc::new(BodyKind::Dynamic, [-1.0 + 0.6 * k as f64, rest, 0.2 * k as f64])
                .with_shape(ShapeDesc::new(geometry))
                .with_velocity(v, [0.0; 3]);
            desc.bullet = bullet;
            pw.add_body(id(2 + k as u32), &desc).unwrap();
        }
        pw
    };
    let (mut swept, mut plain) = (scene(true), scene(false));
    for frame in 0..240 {
        swept.step(1.0 / 60.0).unwrap();
        plain.step(1.0 / 60.0).unwrap();
        let c = swept.raw().swept_counters;
        assert_eq!((c.hits, c.holds, c.embedded, c.exhausted), (0, 0, 0, 0), "frame {frame}: {c:?}");
        for k in 0..4 {
            assert_eq!(swept.pose(id(2 + k)).unwrap(), plain.pose(id(2 + k)).unwrap(), "frame {frame}, body {k}");
        }
    }
}

/// A scene of bullets at every kind of mesh paddle and wall: the kinematic and the dynamic closed paddle, the blade,
/// a one-sided and a both-sided wall, a dense mesh floor, under gravity.
fn paddle_scene(config: PhysicsConfig) -> PhysicsWorld {
    let mut pw = PhysicsWorld::new(config).unwrap();
    pw.add_body(id(1), &BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(mesh_shape(grid_floor(16, 0.5), false)))
        .unwrap();
    // Literal turns, no trigonometry, so the RESULT line below is the same on every platform: (0.6, 0.8) and
    // (0.28, 0.96) are unit.
    let turns = [[0.0, 0.0, 0.0, 1.0], [0.0, 0.6, 0.0, 0.8], [0.0, 0.28, 0.0, 0.96]];
    for (k, kind) in [Paddle::Kinematic, Paddle::Dynamic, Paddle::Blade].into_iter().enumerate() {
        let (mut desc, _) = paddle(kind, 0.0, 30.0 + 15.0 * k as f32);
        desc.position = [-2.0 + 2.0 * k as f64, 0.6, 0.0];
        desc.rotation = turns[k];
        desc.gravity_scale = 0.0;
        pw.add_body(id(10 + k as u32), &desc).unwrap();
    }
    let mut front = wall(Wall::Front);
    front.position = [3.5, 1.0, 0.0];
    pw.add_body(id(20), &front).unwrap();
    let mut both = wall(Wall::BothSides);
    both.position = [-3.5, 1.0, 0.0];
    pw.add_body(id(21), &both).unwrap();
    for k in 0..9u32 {
        let at = [-2.0 + 2.0 * f64::from(k % 3), 0.6 + 0.02 * f64::from(k / 3), 1.2];
        let v = [7.0 * (f64::from(k % 2) - 0.5) as f32, 0.0, -(60.0 + 60.0 * (k / 3) as f32)];
        pw.add_body(id(30 + k), &ball(at, 0.02, v, true)).unwrap();
    }
    pw
}

/// The paddle scene steps to the same state frame by frame on 1, 2, 4 and 8 workers and through a restore, and its
/// last state is a RESULT line for tier 2 (the same on every platform).
#[test]
fn the_paddle_scene_steps_the_same_on_any_worker_count_and_through_a_restore() {
    let run = |pw: &mut PhysicsWorld, frames: u32| {
        (0..frames)
            .map(|_| {
                pw.step(1.0 / 60.0).unwrap();
                pw.state_hash().unwrap()
            })
            .collect::<Vec<u64>>()
    };
    let one = run(&mut paddle_scene(PhysicsConfig::engine()), 120);
    for workers in [2, 4, 8] {
        let config = PhysicsConfig { workers, ..PhysicsConfig::engine() };
        assert_eq!(run(&mut paddle_scene(config), 120), one, "{workers} workers");
    }
    let mut pw = paddle_scene(PhysicsConfig::engine());
    run(&mut pw, 30);
    let image = pw.save_image().unwrap();
    let rest = run(&mut pw, 90);
    pw.restore_image(&image).unwrap();
    assert_eq!(run(&mut pw, 90), rest, "after a restore");
    assert_eq!(rest[..], one[30..], "the restored run is the straight one");
    println!("RESULT mesh paddle scene, 120 frames: state {:016x}", one[119]);
}
