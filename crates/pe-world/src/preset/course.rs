//! The plane's movement and physics test course (the friend test of 2026-11-27; the user's): static
//! stations around the spawn row, every piece a static box or sphere. Meshes and hulls stay off in every preset
//! (config item 8), so curves are arcs of overlapping box segments and rough ground is half-buried spheres and tilted
//! slabs.
//!
//! The open ground around the spawn row and the spawn grid, x and z within about ±7 m and -10..4 m of the origin, is
//! kept clear. Every station stands on the floor, above the void (pe-replica's `VOID_DEPTH`).
//!
//! **Deterministic on every machine:** the layout is a constant, and every number in it comes from `+ - * /` and
//! `sqrt` on f64 (IEEE exact), never from `sin`, `cos` or `atan2`, whose results may differ between platforms' maths
//! libraries. Angles are given by their tangents or by rational points on the circle.

use pe_sim::Transform;
use pe_sim::transform::quat_mul;

use super::{colour, name, row};
use crate::components::{BlobRef, Body, Shape, body_kind, shape_kind};
use crate::edit::{Op, Row, Target};
use crate::world::BlobStore;

/// The names of the course's frames: one plain parent, `Course`, under the plane's root, holding one plain frame per
/// station. The pieces themselves are unnamed.
pub mod names {
    pub const COURSE: &str = "Course";
    pub const SLOPES: &str = "Slopes";
    pub const STAIRS: &str = "Stairs";
    pub const LEDGES: &str = "Ledges";
    pub const HALFPIPE: &str = "Halfpipe";
    pub const BASIN: &str = "Basin";
    pub const WALLS: &str = "Walls";
    pub const BEAMS: &str = "Beams and gaps";
    pub const ROUGH: &str = "Rough ground";
    pub const STATIONS: [&str; 8] = [SLOPES, STAIRS, LEDGES, HALFPIPE, BASIN, WALLS, BEAMS, ROUGH];
}

/// Open ground ahead of the spawn row that no piece enters, for the spawn grid: x from -4.5 to 7.0 m and z from -30
/// to -3 m, relative to the origin (forward is -z). The basin ends at x -5 and the halfpipe starts at x 7.5.
pub const GRID_CLEAR: [[f64; 2]; 2] = [[-4.5, 7.0], [-30.0, -3.0]];
/// Open ground around the spawn row itself, which no piece enters either: x within ±7 m and z from -10 to 4 m.
pub const SPAWN_CLEAR: [[f64; 2]; 2] = [[-7.0, 7.0], [-10.0, 4.0]];

/// The slopes' tangents, shallowest first: 10°, 20°, 30°, 40°, 45°, 55° and 70°. The mover holds a player on ground up
/// to about 50° (pe-physics `PLAYER.min_ground_normal` 0.64), so the last two can't be walked up.
pub const SLOPE_TANS: [f64; 7] = [0.176_327, 0.363_970, 0.577_350, 0.839_100, 1.0, 1.428_148, 2.747_477];
/// The slopes' height, m: they all reach a plateau this high.
pub const SLOPE_H: f64 = 1.5;
/// The stair flights' step heights, m. The mover steps up to 0.3 m (`PLAYER.ride`), so the 0.4 m flight takes jumps.
pub const STEP_HEIGHTS: [f64; 4] = [0.1, 0.2, 0.3, 0.4];
/// The stairs' height, m, and each step's depth.
pub const STAIRS_H: f64 = 1.2;
const TREAD: f64 = 0.4;
/// The jump ledges' heights, m. A jump rises 0.9 m and the ride adds up to 0.3 m, so the 1.5 m ledge is out of reach.
pub const LEDGE_HEIGHTS: [f64; 5] = [0.5, 0.8, 1.0, 1.2, 1.5];

/// A piece's shape: a box's half extents, or a sphere's radius.
#[derive(Clone, Copy)]
enum Form {
    Box([f64; 3]),
    Sphere(f64),
}

#[derive(Clone, Copy)]
struct Piece {
    at: [f64; 3],
    rot: [f32; 4],
    form: Form,
}

const IDENTITY: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

fn block(at: [f64; 3], half: [f64; 3]) -> Piece {
    Piece { at, rot: IDENTITY, form: Form::Box(half) }
}

/// A box standing on the floor, spanning x0..x1 and z0..z1, its top at `top`.
fn column(x: [f64; 2], z: [f64; 2], top: f64) -> Piece {
    block([(x[0] + x[1]) / 2.0, top / 2.0, (z[0] + z[1]) / 2.0], [(x[1] - x[0]) / 2.0, top / 2.0, (z[1] - z[0]) / 2.0])
}

/// The rotation by the angle whose cosine and sine are `c` and `s` (`c² + s² = 1`) about the unit axis `axis`, from
/// the half-angle identities: square roots only.
fn turn(axis: [f64; 3], c: f64, s: f64) -> [f32; 4] {
    let ch = ((1.0 + c) / 2.0).max(0.0).sqrt();
    let sh = ((1.0 - c) / 2.0).max(0.0).sqrt();
    let sh = if s < 0.0 { -sh } else { sh };
    [(axis[0] * sh) as f32, (axis[1] * sh) as f32, (axis[2] * sh) as f32, ch as f32]
}

/// A slab `t` thick under the line from `p` to `q` in the vertical plane through the x axis (x across, y up), `w`
/// wide along z, its solid side away from the air point `air`, and carried on past `p` by `ext[0]` and past `q` by
/// `ext[1]` so that neighbours overlap and a foot never meets a seam. Carry an end on only into the floor or into the
/// next piece's solid (a concave joint): past a convex edge or a top edge it would stand up as a lip.
fn slab(p: [f64; 2], q: [f64; 2], air: [f64; 2], t: f64, w: f64, ext: [f64; 2]) -> ([f64; 2], [f32; 4], [f64; 3]) {
    let (dx, dy) = (q[0] - p[0], q[1] - p[1]);
    let len = (dx * dx + dy * dy).sqrt();
    let (c, s) = (dx / len, dy / len);
    let mid = [(p[0] + q[0]) / 2.0, (p[1] + q[1]) / 2.0];
    let mut n = [-s, c];
    if n[0] * (air[0] - mid[0]) + n[1] * (air[1] - mid[1]) > 0.0 {
        n = [s, -c];
    }
    let shift = (ext[1] - ext[0]) / 2.0;
    let centre = [mid[0] + n[0] * t / 2.0 + c * shift, mid[1] + n[1] * t / 2.0 + s * shift];
    (centre, turn([0.0, 0.0, 1.0], c, s), [len / 2.0 + (ext[0] + ext[1]) / 2.0, t / 2.0, w / 2.0])
}

/// [`slab`] in a station's x-y plane, at z.
fn ramp(p: [f64; 2], q: [f64; 2], air: [f64; 2], t: f64, w: f64, ext: [f64; 2], z: f64) -> Piece {
    let (c, rot, half) = slab(p, q, air, t, w, ext);
    Piece { at: [c[0], c[1], z], rot, form: Form::Box(half) }
}

/// Point `k` of `n` on the quarter circle from (0, 0) (its bottom) to (r, r), centre (0, r): a rational point of the
/// circle, `t = k/n`, so exact up to rounding. Returns (along, up).
fn quarter(k: usize, n: usize, r: f64) -> [f64; 2] {
    let t = k as f64 / n as f64;
    let d = 1.0 + t * t;
    [r * 2.0 * t / d, r - r * (1.0 - t * t) / d]
}

/// The 12 directions 30° apart, `(cos, sin)`, from the exact (√3/2, 1/2) by complex multiplication.
fn twelve() -> [(f64, f64); 12] {
    let (c30, s30) = (3.0f64.sqrt() / 2.0, 0.5);
    let mut out = [(1.0, 0.0); 12];
    for k in 1..12 {
        let (c, s) = out[k - 1];
        out[k] = (c * c30 - s * s30, c * s30 + s * c30);
    }
    out
}

/// A tiny fixed generator for the rough ground's layout (SplitMix64): the same numbers everywhere, seed-independent.
struct Mix(u64);

impl Mix {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A station: its name, its frame's place on the plane (relative to the origin, the floor's top), its colour and its
/// pieces in that frame.
struct Station {
    name: &'static str,
    at: [f64; 3],
    rgb: [f32; 3],
    pieces: Vec<Piece>,
}

/// Seven slopes side by side along z, rising towards +x to one plateau [`SLOPE_H`] up, shallowest at -z.
fn slopes() -> Station {
    let mut pieces = Vec::new();
    for (k, tan) in SLOPE_TANS.into_iter().enumerate() {
        let z = (k as f64 - 3.0) * 3.0;
        let run = SLOPE_H / tan;
        // Into the floor at the foot; flush with the plateau's edge at the top.
        pieces.push(ramp([-run, 0.0], [0.0, SLOPE_H], [-run, SLOPE_H], 0.2, 2.4, [0.3, 0.0], z));
    }
    pieces.push(column([0.0, 4.0], [-10.5, 10.5], SLOPE_H));
    Station { name: names::SLOPES, at: [24.0, 0.0, 0.0], rgb: [0.55, 0.62, 0.7], pieces }
}

/// Four flights side by side along z, rising towards -x to one landing [`STAIRS_H`] up, one per step height.
fn stairs() -> Station {
    let mut pieces = Vec::new();
    for (k, h) in STEP_HEIGHTS.into_iter().enumerate() {
        let z = (k as f64 - 1.5) * 3.0;
        let n = (STAIRS_H / h).round() as usize;
        for i in 1..n {
            let x0 = (n - i) as f64 * TREAD;
            pieces.push(column([x0, x0 + TREAD], [z - 1.2, z + 1.2], i as f64 * h));
        }
    }
    pieces.push(column([-3.0, 0.0], [-5.7, 5.7], STAIRS_H));
    Station { name: names::STAIRS, at: [-18.0, 0.0, -1.5], rgb: [0.7, 0.6, 0.5], pieces }
}

/// Five 1.5 m square ledges in a row along z, one per height of [`LEDGE_HEIGHTS`].
fn ledges() -> Station {
    let pieces = LEDGE_HEIGHTS
        .into_iter()
        .enumerate()
        .map(|(k, h)| {
            let z = (k as f64 - 2.0) * 3.0;
            column([-0.75, 0.75], [z - 0.75, z + 0.75], h)
        })
        .collect();
    Station { name: names::LEDGES, at: [-27.0, 0.0, -1.5], rgb: [0.75, 0.5, 0.45], pieces }
}

/// A halfpipe 8 m long along z: a 2 m flat (the floor) between two quarter circles of radius 2.5 m, six segments each,
/// each up to a deck 2.5 m up. Open at both ends.
fn halfpipe() -> Station {
    const R: f64 = 2.5;
    const N: usize = 6;
    let mut pieces = Vec::new();
    for side in [-1.0, 1.0] {
        let point = |k: usize| {
            let [a, u] = quarter(k, N, R);
            [side * (1.0 + a), u]
        };
        let centre = [side * 1.0, R];
        for k in 0..N {
            // Every joint of the arc is concave; the top segment stops at the deck's height.
            let top = if k + 1 == N { 0.0 } else { 0.1 };
            pieces.push(ramp(point(k), point(k + 1), centre, 0.25, 8.0, [0.1, top], 0.0));
        }
        let x = side * (1.0 + R);
        let deck = if side < 0.0 { [x - 2.0, x - 0.125] } else { [x + 0.125, x + 2.0] };
        pieces.push(column(deck, [-4.0, 4.0], R));
    }
    Station { name: names::HALFPIPE, at: [13.0, 0.0, -22.0], rgb: [0.45, 0.65, 0.55], pieces }
}

/// A round basin: a flat floor 2 m in radius, a shallow ring out to 4 m rising 0.4 m, a steeper ring out to 5.3 m
/// reaching the 1.5 m crest, and an outer slope back down to the floor at 8 m, 12 segments a ring.
fn basin() -> Station {
    // (radius, height) along a radial line, inside out.
    const PROFILE: [[f64; 2]; 4] = [[2.0, 0.0], [4.0, 0.4], [5.3, 1.5], [8.0, 0.0]];
    // A segment's half width along the ring: its outer end's half chord, tan 15° = 2 - √3, plus an overlap.
    let tan15 = 2.0 - 3.0f64.sqrt();
    let mut pieces = Vec::new();
    for (c, s) in twelve() {
        let yaw = turn([0.0, 1.0, 0.0], c, s);
        for k in 0..3 {
            let (p, q) = (PROFILE[k], PROFILE[k + 1]);
            // The air is above the ring: towards the basin's axis inside the crest, away from it outside.
            let air = [(p[0] + q[0]) / 2.0, p[1].max(q[1]) + 1.0];
            let w = 2.0 * (p[0].max(q[0]) + 0.3) * tan15 + 0.2;
            // The crest (between the second and third rings) is convex: no ring is carried on past it.
            let ext = [if k == 2 { 0.0 } else { 0.15 }, if k == 1 { 0.0 } else { 0.15 }];
            let (centre, tilt, half) = slab(p, q, air, 0.25, w, ext);
            // The yaw turns local +x to (c, 0, -s).
            let at = [centre[0] * c, centre[1], -centre[0] * s];
            pieces.push(Piece { at, rot: quat_mul(yaw, tilt), form: Form::Box(half) });
        }
    }
    Station { name: names::BASIN, at: [-13.0, 0.0, -20.0], rgb: [0.5, 0.55, 0.75], pieces }
}

/// Walls 3 m high: a long one, a corner, one at 45°, a doorway, and a 1.2 m corridor half covered by a 2 m ceiling.
fn walls() -> Station {
    const H: f64 = 3.0;
    let w = 0.15;
    let mut pieces = vec![
        // The long wall along x, at the back.
        column([-6.0, 6.0], [6.0 - w, 6.0 + w], H),
        // The corner: a wall along z meeting it.
        column([6.0 - w, 6.0 + w], [1.0, 6.0], H),
        // The doorway: two posts and a lintel, a 1.2 m by 2.2 m opening.
        column([-6.0, -4.6], [0.0 - w, 0.0 + w], H),
        column([-3.4, -2.0], [0.0 - w, 0.0 + w], H),
        block([-4.0, 2.2 + (H - 2.2) / 2.0, 0.0], [0.6, (H - 2.2) / 2.0, w]),
        // The corridor along z, its two walls 1.2 m apart, and a ceiling over its far half, 2 m up.
        column([0.0 - w, 0.0 + w], [-1.0, 5.0], H),
        column([1.2 + w, 1.2 + 3.0 * w], [-1.0, 5.0], H),
        block([0.6 + w, 2.0 + 0.1, 3.5], [0.6 + 2.0 * w, 0.1, 1.5]),
    ];
    // A wall at 45° to the others, 4 m long, to slide along.
    let r = 0.5f64.sqrt();
    pieces.push(Piece {
        at: [4.0, H / 2.0, -2.0],
        rot: turn([0.0, 1.0, 0.0], r, r),
        form: Form::Box([2.0, H / 2.0, w]),
    });
    Station { name: names::WALLS, at: [8.0, 0.0, 14.0], rgb: [0.62, 0.6, 0.56], pieces }
}

/// Balance beams 0.5 m up and 6 m long, 1 m, 0.5 m and 0.25 m wide; four 1 m high platforms in a row with gaps of 1,
/// 2 and 3 m between them; and a winding path, 0.8 m wide and 0.25 m up, in five legs at ±30°.
fn beams() -> Station {
    let mut pieces = Vec::new();
    for (k, wide) in [1.0, 0.5, 0.25].into_iter().enumerate() {
        let x = k as f64 * 2.0;
        pieces.push(column([x - wide / 2.0, x + wide / 2.0], [-3.0, 3.0], 0.5));
    }
    let mut z = -4.0;
    for gap in [1.0, 2.0, 3.0, 0.0] {
        pieces.push(column([6.0, 7.5], [z, z + 1.5], 1.0));
        z += 1.5 + gap;
    }
    // The path's legs, 3 m each, alternating +30° and -30° from +z, west of the beams.
    let (c30, s30) = (3.0f64.sqrt() / 2.0, 0.5);
    let leg = 3.0;
    let mut p = [-3.5, -3.0];
    for k in 0..5 {
        let side = if k % 2 == 0 { 1.0 } else { -1.0 };
        // The direction in the x-z plane: z forward, x by the side.
        let d = [side * s30 * leg, c30 * leg];
        let mid = [p[0] + d[0] / 2.0, p[1] + d[1] / 2.0];
        // A box long along its local z: yawed by ±30° about y (a yaw of φ turns +z to (sin φ, 0, cos φ)).
        let rot = turn([0.0, 1.0, 0.0], c30, side * s30);
        pieces.push(Piece { at: [mid[0], 0.125, mid[1]], rot, form: Form::Box([0.4, 0.125, leg / 2.0 + 0.4]) });
        p = [p[0] + d[0], p[1] + d[1]];
    }
    Station { name: names::BEAMS, at: [20.0, 0.0, 16.0], rgb: [0.72, 0.66, 0.4], pieces }
}

/// Rough ground over about 10 m by 10 m: 25 half-buried spheres 0.3 to 0.9 m in radius on a jittered grid, and 10
/// small slabs tilted up to about 20°.
fn rough() -> Station {
    let mut g = Mix(0x5EED_C0DE_2026_1127);
    let mut pieces = Vec::new();
    for i in 0..5 {
        for j in 0..5 {
            let r = 0.3 + 0.6 * g.next();
            let x = (i as f64 - 2.0) * 2.0 + (g.next() - 0.5) * 0.8;
            let z = (j as f64 - 2.0) * 2.0 + (g.next() - 0.5) * 0.8;
            // Out of the floor by a third to a half of its radius.
            let up = r * (0.33 + 0.17 * g.next());
            pieces.push(Piece { at: [x, up - r, z], rot: IDENTITY, form: Form::Sphere(r) });
        }
    }
    for _ in 0..10 {
        let x = (g.next() - 0.5) * 9.0;
        let z = (g.next() - 0.5) * 9.0;
        // A tilt of up to about 20° (tan 0.36) about a horizontal axis between x and z.
        let tan = 0.36 * g.next();
        let (ax, az) = (g.next() - 0.5, g.next() - 0.5);
        let a = (ax * ax + az * az).sqrt().max(1e-6);
        let c = 1.0 / (1.0 + tan * tan).sqrt();
        let rot = turn([ax / a, 0.0, az / a], c, tan * c);
        pieces.push(Piece { at: [x, 0.05, z], rot, form: Form::Box([0.6, 0.15, 0.6]) });
    }
    Station { name: names::ROUGH, at: [-12.0, 0.0, 16.0], rgb: [0.5, 0.45, 0.4], pieces }
}

fn stations() -> [Station; 8] {
    [slopes(), stairs(), ledges(), halfpipe(), basin(), walls(), beams(), rough()]
}

/// How many pieces (static shapes) the course adds: each counts against the world's shape limit (`MAX_SHAPES`).
pub fn piece_count() -> usize {
    stations().iter().map(|s| s.pieces.len()).sum()
}

/// Makes one object under a parent from its rows, appending its op, and returns its new index.
pub(super) type Create<'a> = dyn FnMut(&mut Vec<Op>, Target, Vec<Row>) -> u32 + 'a;

/// The course's creates, under `parent` (the plane's root): `Course`, each station's frame, and each piece, a static
/// body with one box or sphere shape. `create` makes one object and returns its new index.
pub(super) fn ops(
    parent: Target,
    blobs: &BlobStore,
    used: &mut Vec<BlobRef>,
    ops: &mut Vec<Op>,
    create: &mut Create<'_>,
) {
    let course = create(ops, parent, vec![row(Transform::local([0.0; 3])), name(blobs, used, names::COURSE)]);
    for st in stations() {
        let frame = create(ops, Target::New(course), vec![row(Transform::local(st.at)), name(blobs, used, st.name)]);
        for p in st.pieces {
            let shape = match p.form {
                Form::Box(h) => Shape {
                    kind: shape_kind::BOX,
                    density: 1.0,
                    size: [h[0] as f32, h[1] as f32, h[2] as f32],
                    ..Shape::default()
                },
                Form::Sphere(r) => {
                    Shape { kind: shape_kind::SPHERE, density: 1.0, size: [r as f32, 0.0, 0.0], ..Shape::default() }
                }
            };
            let rows = vec![
                row(Transform::local(p.at).with_rot(p.rot)),
                colour(st.rgb),
                row(Body { kind: body_kind::STATIC, friction: 0.6, ..Body::default() }),
                row(shape),
            ];
            create(ops, Target::New(frame), rows);
        }
    }
}
