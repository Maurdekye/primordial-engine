//! The hold's spring stiffness sweep for the tight hold (docket item tight-hold-for-light-objects-capped-by-a-per-pla).
//!
//! One kinematic hand holds one object, driven as pe-replica drives it (hands.rs): `set_target` once a frame, a move
//! capped at 10 m/s, and the speed-aware hold bound through `step_holding` (`reach = (10 - v_hold) / k`, support
//! `3 g / omega^2`). The grab spring is critically damped, its force capped at the holder's strength, with the
//! velocity drive off and an angular velocity drive of 100 N m, as `hands::hold` makes it; with an acceleration cap A
//! the spring's force is also at most the held mass times `A + g` (pe-replica's `HOLD_ACCELERATION`, 200 m/s^2). Each
//! case runs at the world rates 30, 60 and 120 Hz, for springs from 5 Hz up. A measurement: it prints, it asserts
//! nothing.
//!
//! `cargo run -p pe-physics --release --example hold_sweep -- [strength N] [hertz,...] [A or none] [case,...]`, the
//! cases `still`, `swing`, `knock`, `floor`, `wall`, `pile`, `overrun`, `masses` (all by default) and `cost` (a timing,
//! only when named). Defaults: 110 N, 5 to 120 Hz, A = 200.

use pe_physics::{
    BodyDesc, BodyKind, Geometry, GrabSpring, HoldReach, JointDesc, JointKey, ObjectId, PhysicsConfig, PhysicsWorld,
    Pose, Push, ShapeDesc,
};

const G: f32 = 9.81;
/// pe-replica's `MAX_HAND_PUSH_SPEED`, m/s.
const CAP: f64 = 10.0;
/// The support floor pe-replica's hold bound used before the tight hold, in multiples of the static sag
/// (`HOLD_SUPPORT_MARGIN`); with an acceleration cap A the support is `(A + g) / omega^2`, as pe-replica's `support`.
const MARGIN: f64 = 3.0;
const HAND_R: f32 = 0.09;
const BALL_R: f32 = 0.03;
/// A pit ball's density: 18 g at 3 cm.
const PIT_DENSITY: f32 = 160.0;
const KEY: JointKey = JointKey(7);
const ID_ROT: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
const HAND: u32 = 1;
const OBJ: u32 = 2;
const RATES: [u32; 3] = [30, 60, 120];

thread_local! {
    /// The acceleration cap A of the run, if any: the spring's force is then at most the held mass times `A + g`.
    static ACCEL: std::cell::Cell<Option<f32>> = const { std::cell::Cell::new(None) };
}

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((1u128 << 32) | n as u128)
}

fn density_for(mass: f32, radius: f32) -> f32 {
    mass / (4.0 / 3.0 * std::f32::consts::PI * radius * radius * radius)
}

fn ball(at: [f64; 3], density: f32) -> BodyDesc {
    BodyDesc::new(BodyKind::Dynamic, at)
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: BALL_R }).with_density(density))
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f64>().sqrt()
}

fn norm(v: [f32; 3]) -> f64 {
    f64::from((v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt())
}

/// A hand (id 1) and the object it may hold (id 2), in a world with a floor whose top is at y = 0, plus any statics
/// and other bodies the case adds.
struct Rig {
    pw: PhysicsWorld,
    dt: f32,
    spring: GrabSpring,
    held: bool,
    place: [f32; 3],
}

impl Rig {
    fn new(rate: u32, hertz: f32, strength: f32, hand: [f64; 3]) -> Rig {
        let config = PhysicsConfig { gravity: [0.0, -G, 0.0], ..PhysicsConfig::engine() }.with_tick_rate(rate).unwrap();
        let mut pw = PhysicsWorld::new(config).unwrap();
        let floor = BodyDesc::new(BodyKind::Static, [0.0, -0.5, 0.0])
            .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [20.0, 0.5, 20.0] }));
        pw.add_body(id(1000), &floor).unwrap();
        let body = BodyDesc::new(BodyKind::Kinematic, hand)
            .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: HAND_R }));
        pw.add_body(id(HAND), &body).unwrap();
        let spring = GrabSpring { hertz, damping_ratio: 1.0, max_force: strength, max_torque: 0.0 };
        Rig { pw, dt: 1.0 / rate as f32, spring, held: false, place: [0.0; 3] }
    }

    /// Adds the held object at `hand + place` and grabs it there.
    fn grab_new(&mut self, place: [f32; 3], density: f32) {
        self.grab_new_sized(place, density, BALL_R);
    }

    /// Adds the held object, a sphere of `radius`, at `hand + place` and grabs it there. With the run's acceleration
    /// cap A, the spring's force is at most the object's mass times `A + g`.
    fn grab_new_sized(&mut self, place: [f32; 3], density: f32, radius: f32) {
        let h = self.hand();
        let at = [h[0] + f64::from(place[0]), h[1] + f64::from(place[1]), h[2] + f64::from(place[2])];
        let desc = BodyDesc::new(BodyKind::Dynamic, at)
            .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius }).with_density(density));
        self.pw.add_body(id(OBJ), &desc).unwrap();
        if let Some(a) = ACCEL.with(|c| c.get()) {
            let mass = density * 4.0 / 3.0 * std::f32::consts::PI * radius * radius * radius;
            self.spring.max_force = self.spring.max_force.min(mass * (a + G));
        }
        self.grab(place);
    }

    fn grab(&mut self, place: [f32; 3]) {
        let desc = JointDesc::Grab {
            a: Some(id(HAND)),
            b: id(OBJ),
            anchor_a: place,
            anchor_b: [0.0; 3],
            max_force: 0.0,
            max_torque: 100.0,
            spring: Some(self.spring),
        };
        self.pw.add_joint(KEY, &desc).unwrap();
        self.held = true;
        self.place = place;
    }

    fn release(&mut self) {
        self.pw.remove_joint(KEY).unwrap();
        self.held = false;
    }

    fn hand(&self) -> [f64; 3] {
        self.pw.pose(id(HAND)).unwrap().position
    }

    fn obj(&self) -> [f64; 3] {
        self.pw.pose(id(OBJ)).unwrap().position
    }

    /// Where the held object's place in the hand is now.
    fn place_now(&self) -> [f64; 3] {
        let h = self.hand();
        [0, 1, 2].map(|i| h[i] + f64::from(self.place[i]))
    }

    fn v_obj(&self) -> [f32; 3] {
        self.pw.velocity(id(OBJ)).unwrap().0
    }

    fn k(&self) -> f64 {
        f64::from(self.pw.grab_spring_rate(self.spring, self.dt))
    }

    /// One frame towards the hand's `target`, as pe-replica's `drive` and `reaches` do it (no turn).
    fn frame(&mut self, target: [f64; 3]) {
        let now = self.hand();
        let farthest = dist(now, target);
        let cap = CAP * f64::from(self.dt);
        let to = if farthest > cap {
            let f = cap / farthest;
            [0, 1, 2].map(|i| now[i] + f * (target[i] - now[i]))
        } else {
            target
        };
        self.pw.set_target(id(HAND), Pose { position: to, rotation: ID_ROT }, self.dt, true).unwrap();
        let mut reaches = Vec::new();
        if self.held {
            let v = norm(self.pw.velocity(id(HAND)).unwrap().0);
            let omega = 2.0 * std::f64::consts::PI * f64::from(self.spring.hertz);
            // With a force cap A, as pe-replica's `reaches`: no more than the speed the hold can brake from.
            let mut room = (CAP - v).max(0.0);
            if ACCEL.with(|c| c.get()).is_some() {
                let (place, obj) = (self.place_now(), self.obj());
                let error = [0, 1, 2].map(|i| place[i] - obj[i]);
                let e = error.iter().map(|c| c * c).sum::<f64>().sqrt();
                if e > 0.0 {
                    let per_kg =
                        f64::from(self.pw.grab_force(KEY).unwrap()) / f64::from(self.pw.mass(id(OBJ)).unwrap());
                    let a = (per_kg + f64::from(G) * error[1] / e).max(0.0);
                    room = room.min((2.0 * a * e).sqrt());
                }
            }
            let reach = room / self.k();
            let support = support(omega);
            reaches.push((KEY, HoldReach { reach: reach as f32, support: support as f32 }));
        }
        self.pw.step_holding(self.dt, &reaches).unwrap();
    }

    fn frames(&self, secs: f64) -> usize {
        (secs / f64::from(self.dt)).round() as usize
    }
}

/// Held still at 1 m for 2 s: the sag below the place at the end, mm, and the most speed over the last second, mm/s.
/// A sphere of `mass`: its density and radius, the radius keeping the density at most 8,000 kg/m^3.
fn sized(mass: f32) -> (f32, f32) {
    let r = BALL_R.max((3.0 * mass / (4.0 * std::f32::consts::PI * 8000.0)).cbrt());
    (density_for(mass, r), r)
}

fn still(rate: u32, hertz: f32, strength: f32, mass: f32) -> (f64, f64) {
    let mut r = Rig::new(rate, hertz, strength, [0.0, 1.0, 0.0]);
    let (d, rad) = sized(mass);
    r.grab_new_sized([HAND_R + rad, 0.0, 0.0], d, rad);
    let n = r.frames(2.0);
    let mut jitter: f64 = 0.0;
    for i in 0..n {
        r.frame([0.0, 1.0, 0.0]);
        if i >= n / 2 {
            jitter = jitter.max(norm(r.v_obj()));
        }
    }
    ((r.place_now()[1] - r.obj()[1]) * 1000.0, jitter * 1000.0)
}

/// A swing along x: held still at 1 m for 0.5 s, the hand speeding up evenly over `ramp` s to `v`, keeping it for 0.1
/// s, then letting go and keeping on. The most trail behind the place while speeding up and at the end of the steady
/// part, mm; the hand's speed when it lets go and the object's right after, m/s.
fn swing(rate: u32, hertz: f32, strength: f32, mass: f32, v: f64, ramp: f64) -> (f64, f64, f64, f64) {
    let mut r = Rig::new(rate, hertz, strength, [0.0, 1.0, 0.0]);
    let (d, rad) = sized(mass);
    r.grab_new_sized([HAND_R + rad, 0.0, 0.0], d, rad);
    for _ in 0..r.frames(0.5) {
        r.frame([0.0, 1.0, 0.0]);
    }
    let speeding = r.frames(ramp).max(1);
    let steady = r.frames(0.1);
    let dt = f64::from(r.dt);
    let mut x = 0.0;
    let mut most: f64 = 0.0;
    let mut end = 0.0;
    for k in 1..=speeding + steady {
        x += v * (k.min(speeding) as f64 / speeding as f64) * dt;
        r.frame([x, 1.0, 0.0]);
        let trail = r.place_now()[0] - r.obj()[0];
        if k <= speeding {
            most = most.max(trail);
        }
        end = trail;
    }
    let hand_v = f64::from(r.pw.velocity(id(HAND)).unwrap().0[0]);
    r.release();
    x += v * dt;
    r.frame([x, 1.0, 0.0]);
    let launch = f64::from(r.v_obj()[0]);
    (most * 1000.0, end * 1000.0, hand_v, launch)
}

/// A lift: held still at 0.5 m, then the hand speeds up upwards at `a` m/s^2 to 3 m/s and
/// keeps that for 0.3 s. The most the object lags below its place, mm, and how far below the place it is at the end.
fn lift(rate: u32, hertz: f32, strength: f32, mass: f32, a: f64) -> (f64, f64) {
    let mut r = Rig::new(rate, hertz, strength, [0.0, 0.5, 0.0]);
    let (d, rad) = sized(mass);
    r.grab_new_sized([HAND_R + rad, 0.0, 0.0], d, rad);
    for _ in 0..r.frames(1.0) {
        r.frame([0.0, 0.5, 0.0]);
    }
    let dt = f64::from(r.dt);
    let (mut y, mut v) = (0.5, 0.0);
    let mut most: f64 = 0.0;
    for _ in 0..r.frames(3.0 / a + 0.3) {
        v = (v + a * dt).min(3.0);
        y += v * dt;
        r.frame([0.0, y, 0.0]);
        most = most.max(r.place_now()[1] - r.obj()[1]);
    }
    (most * 1000.0, (r.place_now()[1] - r.obj()[1]) * 1000.0)
}

/// A ball held from above and pressed `depth` m into the floor: the hand comes down at 1 m/s from 0.2 m, then stays
/// for 1 s. The deepest the ball goes into the floor, mm; its most speed over the last 0.5 s, mm/s; the most sideways
/// speed at any time, mm/s.
fn floor(rate: u32, hertz: f32, strength: f32, depth: f64) -> (f64, f64, f64) {
    let r0 = f64::from(BALL_R);
    let mut r = Rig::new(rate, hertz, strength, [0.0, 0.2 + 0.15, 0.0]);
    r.grab_new([0.0, -0.15, 0.0], PIT_DENSITY);
    let low = r0 - depth + 0.15;
    let dt = f64::from(r.dt);
    let mut y = 0.35;
    let (mut deepest, mut jitter, mut side): (f64, f64, f64) = (0.0, 0.0, 0.0);
    let total = r.frames((0.35 - low) / 1.0) + r.frames(1.0);
    for i in 0..total {
        y = (y - dt).max(low);
        r.frame([0.0, y, 0.0]);
        let o = r.obj();
        deepest = deepest.max(r0 - o[1]);
        let v = r.v_obj();
        side = side.max(f64::from((v[0] * v[0] + v[2] * v[2]).sqrt()));
        if i >= total - r.frames(0.5) {
            jitter = jitter.max(norm(v));
        }
    }
    (deepest * 1000.0, jitter * 1000.0, side * 1000.0)
}

/// A pile: 5 x 5 x 3 pit balls in a 0.30 m square box, settled for 1.5 s; then a ball held from above is pressed 5 cm
/// into the pile's top centre (the hand coming down at 1 m/s), kept there 1 s, and lifted out at 1 m/s. The most speed
/// of any pile ball during the press and after, m/s; the pile's kinetic energy over the press's last 0.5 s, mJ (most);
/// the held ball's most speed over that time, mm/s; how far the held ball stays above its place, mm.
fn pile(rate: u32, hertz: f32, strength: f32) -> (f64, f64, f64, f64) {
    let mut r = Rig::new(rate, hertz, strength, [0.0, 1.0, 0.0]);
    let half = 0.15_f32;
    for (n, (c, h)) in [
        ([half + 0.05, 0.25, 0.0], [0.05, 0.25, 0.25]),
        ([-half - 0.05, 0.25, 0.0], [0.05, 0.25, 0.25]),
        ([0.0, 0.25, half + 0.05], [0.25, 0.25, 0.05]),
        ([0.0, 0.25, -half - 0.05], [0.25, 0.25, 0.05]),
    ]
    .into_iter()
    .enumerate()
    {
        let wall = BodyDesc::new(BodyKind::Static, c.map(f64::from))
            .with_shape(ShapeDesc::new(Geometry::Box { half_extents: h }));
        r.pw.add_body(id(1001 + n as u32), &wall).unwrap();
    }
    let mut pile_ids = Vec::new();
    let mut n = 100;
    for layer in 0..3 {
        for i in 0..5 {
            for j in 0..5 {
                let at = [
                    -0.12 + 0.06 * f64::from(i) + 0.001 * f64::from(layer),
                    0.031 + 0.0605 * f64::from(layer),
                    -0.12 + 0.06 * f64::from(j),
                ];
                r.pw.add_body(id(n), &ball(at, PIT_DENSITY)).unwrap();
                pile_ids.push(id(n));
                n += 1;
            }
        }
    }
    // The held ball starts well above the pile.
    r.grab_new([0.0, -0.15, 0.0], PIT_DENSITY);
    for _ in 0..r.frames(1.5) {
        r.frame([0.0, 1.0, 0.0]);
    }
    let top = pile_ids
        .iter()
        .map(|&b| r.pw.pose(b).unwrap().position)
        .filter(|p| p[0].abs() < 0.04 && p[2].abs() < 0.04)
        .map(|p| p[1])
        .fold(0.0_f64, f64::max);
    let contact_place = top + 2.0 * f64::from(BALL_R);
    let low = contact_place - 0.05 + 0.15;
    let dt = f64::from(r.dt);
    let mass = 0.018_f64;
    let mut y = 1.0;
    let (mut fastest, mut energy, mut jitter, mut above): (f64, f64, f64, f64) = (0.0, 0.0, 0.0, 0.0);
    let down = r.frames((1.0 - low) / 1.0);
    let stay = r.frames(1.0);
    for i in 0..down + stay + r.frames(0.6) {
        y = if i < down + stay { (y - dt).max(low) } else { y + dt };
        r.frame([0.0, y, 0.0]);
        let speeds: Vec<f64> = pile_ids.iter().map(|&b| norm(r.pw.velocity(b).unwrap().0)).collect();
        fastest = fastest.max(speeds.iter().cloned().fold(0.0, f64::max));
        if i >= down + stay - r.frames(0.5) && i < down + stay {
            energy = energy.max(speeds.iter().map(|s| 0.5 * mass * s * s).sum::<f64>() * 1000.0);
            jitter = jitter.max(norm(r.v_obj()));
            above = above.max(r.obj()[1] - r.place_now()[1]);
        }
    }
    (fastest, energy, jitter * 1000.0, above * 1000.0)
}

/// Held still, then knocked sideways with 0.05 N s (2.8 m/s for a pit ball): the most it is moved from its place,
/// mm; how far it then overshoots back past its place, mm; and how long until it stays within 1 mm, ms.
fn knock(rate: u32, hertz: f32, strength: f32) -> (f64, f64, f64) {
    let mut r = Rig::new(rate, hertz, strength, [0.0, 1.0, 0.0]);
    r.grab_new([0.12, 0.0, 0.0], PIT_DENSITY);
    for _ in 0..r.frames(0.5) {
        r.frame([0.0, 1.0, 0.0]);
    }
    let base = r.obj()[2] - r.place_now()[2];
    r.pw.push(id(OBJ), Push::Impulse([0.0, 0.0, 0.05])).unwrap();
    let (mut out, mut back, mut settled): (f64, f64, Option<usize>) = (0.0, 0.0, None);
    for i in 0..r.frames(1.0) {
        r.frame([0.0, 1.0, 0.0]);
        let dz = r.obj()[2] - r.place_now()[2] - base;
        out = out.max(dz);
        back = back.max(-dz);
        if dz.abs() > 0.001 {
            settled = None;
        } else if settled.is_none() {
            settled = Some(i + 1);
        }
    }
    let ms = settled.map_or(f64::NAN, |n| n as f64 * f64::from(r.dt) * 1000.0);
    (out * 1000.0, back * 1000.0, ms)
}

/// The hand carried sideways at 1.5 times the cap for 1 s at 1 m, then stopped: the object's most speed, m/s, its most
/// sag below its place, mm, and its most speed in the 0.5 s after the stop, m/s.
fn overrun(rate: u32, hertz: f32, strength: f32) -> (f64, f64, f64) {
    let mut r = Rig::new(rate, hertz, strength, [0.0, 1.0, 0.0]);
    r.grab_new([0.12, 0.0, 0.0], PIT_DENSITY);
    for _ in 0..r.frames(0.5) {
        r.frame([0.0, 1.0, 0.0]);
    }
    let dt = f64::from(r.dt);
    let (mut fastest, mut sag, mut after): (f64, f64, f64) = (0.0, 0.0, 0.0);
    let mut x = 0.0;
    for _ in 0..r.frames(1.0) {
        x += 1.5 * CAP * dt;
        r.frame([x, 1.0, 0.0]);
        fastest = fastest.max(norm(r.v_obj()));
        sag = sag.max(r.place_now()[1] - r.obj()[1]);
    }
    for _ in 0..r.frames(0.5) {
        r.frame([x, 1.0, 0.0]);
        after = after.max(norm(r.v_obj()));
    }
    (fastest, sag * 1000.0, after)
}

/// A ball held against a static wall `thick` m thick whose near face is 0.3 m ahead: the hand goes at `u` m/s to 1 m
/// past the far face and stays 0.5 s, then the wall goes. The deepest the ball goes into the wall, mm; whether its
/// centre ever passed the wall's middle (it would come out the far side); its most speed once the wall is gone, m/s,
/// read at a frame's end (a very stiff spring closes the error it may see within a frame, so the ball moves in hops of
/// its reach and reads slower than it moves).
fn wall(rate: u32, hertz: f32, strength: f32, thick: f64, u: f64) -> (f64, bool, f64) {
    let mut r = Rig::new(rate, hertz, strength, [0.0, 1.0, 0.0]);
    let face = 0.3;
    let wall = BodyDesc::new(BodyKind::Static, [face + thick / 2.0, 1.0, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [(thick / 2.0) as f32, 1.0, 1.0] }));
    r.pw.add_body(id(2000), &wall).unwrap();
    r.grab_new([0.12, 0.0, 0.0], PIT_DENSITY);
    let dt = f64::from(r.dt);
    let end = face + thick + 1.0;
    let (mut x, mut deepest, mut through) = (0.0, 0.0_f64, false);
    for _ in 0..r.frames((end / u) + 0.5) {
        x = (x + u * dt).min(end);
        r.frame([x, 1.0, 0.0]);
        let bx = r.obj()[0];
        deepest = deepest.max(bx + f64::from(BALL_R) - face);
        through |= bx > face + thick / 2.0;
    }
    let debug = std::env::var("HOLD_SWEEP_DEBUG").is_ok();
    if debug {
        println!(
            "    before removal: ball awake {:?}, hand awake {:?}, ball at {:?}",
            r.pw.is_awake(id(OBJ)),
            r.pw.is_awake(id(HAND)),
            r.obj()
        );
    }
    r.pw.remove_body(id(2000)).unwrap();
    let mut back: f64 = 0.0;
    for i in 0..r.frames(1.0) {
        r.frame([x, 1.0, 0.0]);
        back = back.max(norm(r.v_obj()));
        if debug && i < 4 {
            println!(
                "    after removal +{}: ball awake {:?}, speed {:.3}, at {:?}",
                i + 1,
                r.pw.is_awake(id(OBJ)),
                norm(r.v_obj()),
                r.obj()
            );
        }
    }
    (deepest * 1000.0, through, back)
}

/// The hold a cost run uses: M1's (5 Hz, 1,000 N, support 3 g / omega^2, the speed-aware reach), the tight hold's
/// (60 Hz, min(110 N, m (200 + g)), support (A + g) / omega^2, the reach also bounded by the speed it can brake from),
/// or no hold (the hand stirs on its own).
#[derive(Clone, Copy, PartialEq)]
enum CostHold {
    M1,
    Tight,
    None,
}

/// The cost of a hold on an awake stack: 400 pit balls (10 x 10, 4 deep) settle 2 s in a 0.6 m square box, then a hand
/// stirs a held pit ball round a 15 cm circle through the pile at 1 m/s for 3 s, keeping it awake. Returns the step
/// times over the stirring, ns, sorted, and the awake bodies at the end.
fn cost(rate: u32, hold: CostHold) -> (Vec<u128>, i32) {
    let hertz = if hold == CostHold::M1 { 5.0 } else { 60.0 };
    let strength = if hold == CostHold::M1 { 1000.0 } else { 110.0 };
    let mut r = Rig::new(rate, hertz, strength, [0.15, 0.45, 0.0]);
    for (n, (c, h)) in [
        ([0.35, 0.3, 0.0], [0.05, 0.3, 0.4]),
        ([-0.35, 0.3, 0.0], [0.05, 0.3, 0.4]),
        ([0.0, 0.3, 0.35], [0.4, 0.3, 0.05]),
        ([0.0, 0.3, -0.35], [0.4, 0.3, 0.05]),
    ]
    .into_iter()
    .enumerate()
    {
        let wall = BodyDesc::new(BodyKind::Static, c.map(f64::from))
            .with_shape(ShapeDesc::new(Geometry::Box { half_extents: h }));
        r.pw.add_body(id(1001 + n as u32), &wall).unwrap();
    }
    let mut n = 100;
    for layer in 0..4 {
        for i in 0..10 {
            for j in 0..10 {
                let at = [
                    -0.27 + 0.06 * f64::from(i) + 0.002 * f64::from(layer),
                    0.031 + 0.0605 * f64::from(layer),
                    -0.27 + 0.06 * f64::from(j),
                ];
                r.pw.add_body(id(n), &ball(at, PIT_DENSITY)).unwrap();
                n += 1;
            }
        }
    }
    if hold != CostHold::None {
        let place = [0.0_f32, -0.12, 0.0];
        let mass = PIT_DENSITY * 4.0 / 3.0 * std::f32::consts::PI * BALL_R.powi(3);
        if hold == CostHold::Tight {
            r.spring.max_force = r.spring.max_force.min(mass * (200.0 + G));
        }
        let h = r.hand();
        let at = [0, 1, 2].map(|i| h[i] + f64::from(place[i]));
        r.pw.add_body(id(OBJ), &ball(at, PIT_DENSITY)).unwrap();
        r.grab(place);
    }
    ACCEL.with(|c| c.set(if hold == CostHold::Tight { Some(200.0) } else { None }));
    let dt = f64::from(r.dt);
    for _ in 0..r.frames(2.0) {
        r.frame([0.15, 0.45, 0.0]);
    }
    // Down into the pile, then round.
    for k in 0..r.frames(0.3) {
        let y = 0.45 - 0.25 * (k as f64 + 1.0) / (0.3 / dt);
        r.frame([0.15, y, 0.0]);
    }
    let mut times = Vec::new();
    for k in 0..r.frames(3.0) {
        let t = k as f64 * dt;
        let a = t / 0.15;
        let target = [0.15 * a.cos(), 0.20, 0.15 * a.sin()];
        let start = std::time::Instant::now();
        r.frame(target);
        times.push(start.elapsed().as_nanos());
    }
    let awake = r.pw.awake_body_count();
    ACCEL.with(|c| c.set(None));
    times.sort_unstable();
    (times, awake)
}

fn cost_table() {
    println!();
    println!(
        "cost on an awake stack (400 balls stirred 3 s by a held ball; interleaved, release, idle priority, a loaded \
         machine, not a clean timing run): step time mean / median / p95 / max, us; awake bodies at the end"
    );
    for rate in RATES {
        let mut rows: Vec<(&str, Vec<u128>, i32)> =
            vec![("M1 hold", Vec::new(), 0), ("tight hold", Vec::new(), 0), ("hand alone", Vec::new(), 0)];
        for _round in 0..3 {
            for (k, hold) in [CostHold::M1, CostHold::Tight, CostHold::None].into_iter().enumerate() {
                let (t, awake) = cost(rate, hold);
                rows[k].1.extend(t);
                rows[k].2 = awake;
            }
        }
        for (name, mut t, awake) in rows {
            t.sort_unstable();
            let mean = t.iter().sum::<u128>() as f64 / t.len() as f64 / 1000.0;
            let at = |q: f64| t[((t.len() - 1) as f64 * q) as usize] as f64 / 1000.0;
            println!(
                "  {rate:3} Hz {name:10}: {mean:7.1} / {:7.1} / {:7.1} / {:8.1} us; {awake} awake",
                at(0.5),
                at(0.95),
                at(1.0)
            );
        }
    }
}

/// The hold's support: `(A + g) / omega^2` with an acceleration cap A, else the old `MARGIN g / omega^2`.
fn support(omega: f64) -> f64 {
    let lift = ACCEL.with(|c| c.get()).map_or(MARGIN * f64::from(G), |a| f64::from(a + G));
    lift / (omega * omega)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let strength: f32 = args.get(1).map_or(110.0, |s| s.parse().expect("strength, N"));
    let hertzes: Vec<f32> = args.get(2).map_or(vec![5.0, 10.0, 15.0, 20.0, 30.0, 40.0, 60.0, 90.0, 120.0], |s| {
        s.split(',').map(|h| h.parse().expect("hertz")).collect()
    });
    let accel: Option<f32> = match args.get(3).map(String::as_str) {
        Some("none") => None,
        Some(a) => Some(a.parse().expect("A, m/s^2")),
        None => Some(200.0),
    };
    let only: Option<String> = args.get(4).cloned();
    ACCEL.with(|c| c.set(accel));
    // Every case but `cost` (a timing, slow) by default.
    let want = |name: &str| only.as_deref().map_or(name != "cost", |o| o.split(',').any(|w| w == name));
    println!(
        "hold sweep: strength {strength} N; force cap {}; springs {hertzes:?} Hz; world rates {RATES:?} Hz (sub-step \
         1/240 s)",
        accel.map_or("the strength".to_string(), |a| format!("min(strength, mass x ({a} + g))"))
    );
    println!();
    println!(
        "k = Box3D's bias rate omega / (2 zeta + h omega); reach at rest = cap / k; support = (A + g) / omega^2 (3 g / \
         omega^2 with no A)"
    );
    for &hz in &hertzes {
        let r = Rig::new(60, hz, strength, [0.0; 3]);
        let omega = 2.0 * std::f64::consts::PI * f64::from(hz);
        println!(
            "  {hz:5} Hz: k {:6.1} /s, reach at rest {:6.1} mm, support {:6.2} mm, static sag g/omega^2 {:6.2} mm",
            r.k(),
            CAP / r.k() * 1000.0,
            support(omega) * 1000.0,
            f64::from(G) / (omega * omega) * 1000.0
        );
    }
    let table = |title: &str, f: &dyn Fn(u32, f32) -> String| {
        println!();
        println!("{title}");
        for &hz in &hertzes {
            let cells: Vec<String> = RATES.iter().map(|&rate| f(rate, hz)).collect();
            println!("  {hz:5} Hz | {}", cells.join(" | "));
        }
    };
    if want("still") {
        table("still, 18 g at 1 m, 2 s: sag mm, most speed over the last 1 s mm/s  [30 | 60 | 120 Hz]", &|rate, hz| {
            let (sag, j) = still(rate, hz, strength, 0.018);
            format!("{sag:6.2} {j:7.3}")
        });
    }
    if want("swing") {
        for (v, ramp) in [(3.5, 0.15), (8.0, 0.15), (10.0, 0.1), (9.0, 0.06), (9.0, 0.045), (9.0, 0.03)] {
            table(
                &format!(
                    "swing to {v} m/s over {ramp} s ({:.0} m/s^2), 18 g: most trail speeding up, trail steady (mm); \
                     hand, launch (m/s)",
                    v / ramp
                ),
                &|rate, hz| {
                    let (most, end, hv, lv) = swing(rate, hz, strength, 0.018, v, ramp);
                    format!("{most:6.1} {end:6.1} {hv:5.2} {lv:5.2}")
                },
            );
        }
    }
    if want("knock") {
        table("knock 0.05 N s, 18 g: most out mm, overshoot back mm, settled within 1 mm at ms", &|rate, hz| {
            let (out, back, ms) = knock(rate, hz, strength);
            format!("{out:6.1} {back:6.2} {ms:6.0}")
        });
    }
    if want("floor") {
        for depth in [0.05, 0.2] {
            table(
                &format!(
                    "floor, 18 g pressed {:.0} cm in: deepest mm, last-0.5 s speed mm/s, most sideways mm/s",
                    depth * 100.0
                ),
                &|rate, hz| {
                    let (d, j, s) = floor(rate, hz, strength, depth);
                    format!("{d:6.2} {j:7.2} {s:7.2}")
                },
            );
        }
    }
    if want("wall") {
        for (thick, u) in [(0.02, 2.0), (0.02, 8.0), (0.1, 2.0), (0.1, 8.0)] {
            table(
                &format!(
                    "wall {:.0} cm thick, hand at {u} m/s to 1 m past it: deepest into it mm, centre past its middle, \
                     most speed after it goes m/s",
                    thick * 100.0
                ),
                &|rate, hz| {
                    let (d, t, b) = wall(rate, hz, strength, thick, u);
                    format!("{d:6.2} {:5} {b:5.2}", if t { "THRU" } else { "-" })
                },
            );
        }
    }
    if want("pile") {
        table(
            "pile 5x5x3, held ball pressed 5 cm in: pile fastest m/s, pile KE mJ (last 0.5 s), held speed mm/s, held \
             above place mm",
            &|rate, hz| {
                let (f, e, j, a) = pile(rate, hz, strength);
                format!("{f:5.2} {e:7.3} {j:7.1} {a:6.1}")
            },
        );
    }
    if want("overrun") {
        table("overrun 15 m/s for 1 s: most speed m/s, most sag mm, most speed after the stop m/s", &|rate, hz| {
            let (f, s, a) = overrun(rate, hz, strength);
            format!("{f:6.2} {s:6.2} {a:6.2}")
        });
    }
    if want("cost") {
        cost_table();
    }
    if want("masses") {
        println!();
        println!(
            "masses (radius keeps density <= 8,000), world rate 60 Hz: still sag mm | 100 m/s^2 flick to 5 m/s: most \
             trail mm, launch m/s | lift at 100 m/s^2 to 3 m/s: most lag mm, lag at end mm"
        );
        for &hz in &hertzes {
            for mass in [0.018_f32, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 12.0] {
                let (sag, _) = still(60, hz, strength, mass);
                let (most, _, _, launch) = swing(60, hz, strength, mass, 5.0, 0.05);
                let (lag, end) = lift(60, hz, strength, mass, 100.0);
                println!(
                    "  {hz:5} Hz {mass:6.3} kg | sag {sag:8.2} | trail {most:8.1} launch {launch:5.2} | lift \
                     {lag:8.1} {end:8.1}"
                );
            }
        }
    }
}
