//! M1's ball pit and member admission (world-model.md 2.4: @coordinator-opus, option
//! (a) with @fable-reviewer's conditions).
//!
//! The ball pit is **disposable genesis code**, not the branch entries of a preset line (2.3): [`ball_pit`] builds one
//! world at frame 0 by applying a single ordinary edit transaction through the normal edit path, authored by world
//! logic on a reserved line, [`m1_line`]. Nothing here is the origin, or stands for it. A world made here carries the
//! M1 marker ([`World::m1_marker`]), so a later build recognises it and refuses it or imports it as unrelated
//! content. The content holds no hands: a member's hands come from [`World::admit`], as for anyone who joins.
//!
//! Nothing outside this module may depend on the preset's ids: find its objects by name ([`names`]).

use pe_core::commit::Hasher;
use pe_core::{ChainId, Frame, Hash, LineId, MemberKey, ObjectId, Offset, Pos};
use pe_seams::{Outcome, SimRate};
use pe_sim::{Component, Transform};

use crate::apply::Draft;
use crate::components::{
    Appearance, BlobRef, Body, Hand, Light, Name, Player, Shape, SimSize, body_kind, light_kind, shape_kind,
};
use crate::edit::{Actor, Op, Row, Target, Transaction};
use crate::world::{BlobStore, World, perm};

/// The context of the reserved line M1's disposable worlds are made on.
pub const M1_LINE_CONTEXT: &str = "pe.m1.disposable";

/// The reserved line M1's ball pit is made on: `BLAKE3("pe.m1.disposable" ‖ "ball-pit")`. It marks an M1 world (the
/// marker a saved world carries), and is neither the origin nor a preset line.
pub fn m1_line() -> LineId {
    LineId(Hash::of(M1_LINE_CONTEXT, b"ball-pit"))
}

/// The line an M1 interim session's world is on (world-model.md 2.4, interim sessions): the hash of the session's
/// genesis record without its root, `Hasher::new("pe.m1.genesis.line").write_len_prefixed(record)`. The record is
/// pe-app's (`pe_app::genesis`); it cannot cover the root, since the frame-0 root covers the line. Like a branch
/// entry's line (2.3), the line is the hash of the record that makes the world, so two sessions never share a line,
/// and it is neither the origin nor a preset line.
pub fn genesis_line(record_without_root: &[u8]) -> LineId {
    let mut h = Hasher::new("pe.m1.genesis.line");
    h.write_len_prefixed(record_without_root);
    LineId(h.finish())
}

/// How many balls [`ball_pit`] makes.
pub const BALLS: usize = 200;
/// The most balls [`ball_pit_with`] makes (the old prototype's bound).
pub const MAX_BALLS: usize = 500;

/// The ball radius, in metres (the old prototype's `BALL_R`).
pub const BALL_R: f64 = 0.06;
/// The ball density, kg/m³: a real 12 cm ball-pit ball of about 18 g (`20 × 4/3 π 0.06³` = 0.0181 kg). The old
/// prototype's 0.2 gave 0.18 g, lighter than air (@design-reviewer's Medium on b461fddd).
pub const BALL_DENSITY: f32 = 20.0;
/// The pit's inner half width, in metres, and its walls' half height.
const PIT_W: f64 = 1.2;
const WALL_H: f64 = 0.6;
const BALLS_PER_ROW: usize = 16;

/// The names the preset gives its objects. Tests, the harness and API examples find the objects by these.
pub mod names {
    pub const ROOT: &str = "Ball pit";
    pub const FLOOR: &str = "Floor";
    pub const WALLS: [&str; 4] = ["Wall +x", "Wall -x", "Wall +z", "Wall -z"];
    pub const LAMP: &str = "Lamp";
    /// Ball `i` is `Ball <i>`, from 0.
    pub const BALL_PREFIX: &str = "Ball ";
}

/// The M1 ball pit with [`BALLS`] balls: see [`ball_pit_with`].
pub fn ball_pit(seed: u64, blobs: BlobStore, origin: Pos) -> World {
    ball_pit_with(seed, blobs, origin, BALLS)
}

/// The M1 ball pit at frame 0, with `balls` balls (clamped to `1..=MAX_BALLS`), centred on `origin` (its floor's top
/// at `origin`). `origin` is also the domain's physics origin. Deterministic: the same arguments give the same world
/// and the same state root on every machine. The world has no members and no hands; admit them with
/// [`World::admit`].
///
/// # Panics
/// If the preset's own transaction is refused, which is a bug in this module (a test applies it), or if `origin` is
/// so near the edge of the position range that the pit does not fit.
pub fn ball_pit_with(seed: u64, blobs: BlobStore, origin: Pos, balls: usize) -> World {
    ball_pit_on(m1_line(), seed, blobs, origin, balls)
}

/// [`ball_pit_with`] on line `line`: the world's line, and the line the pit's object ids derive from (2.1). An M1
/// interim session passes [`genesis_line`] of its genesis record, before admitting anyone, so the hands' ids derive
/// from it too. The world still carries the M1 marker, `Some(m1_line())`, whatever its line.
///
/// # Panics
/// As [`ball_pit_with`], and if `line` is the origin (`LineId::origin()`): nothing M1 makes is the origin (2.4).
pub fn ball_pit_on(line: LineId, seed: u64, blobs: BlobStore, origin: Pos, balls: usize) -> World {
    assert_ne!(line, LineId::origin(), "an M1 world is never on the origin's line");
    let balls = balls.clamp(1, MAX_BALLS);
    let mut w = World::new(seed, blobs.clone());
    w.set_origin(origin);
    w.m1_marker = Some(m1_line());
    w.line = line;
    let tx = transaction(seed, &blobs, origin, balls);
    // World logic authors the preset. Logic is no member, so it holds `EDIT` only while the transaction applies.
    w.set_member(MemberKey::LOGIC, perm::EDIT);
    let (outcome, mut w) = w.dry_run_draft_state(Frame(0), &draft(line, seed), &tx);
    assert!(matches!(outcome, Outcome::Applied { .. }), "the ball-pit transaction was refused: {outcome:?}");
    w.set_member(MemberKey::LOGIC, 0);
    w
}

/// Which world a session's genesis builds (world-model.md 2.4): the ball pit, or the friend test's flat plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// [`ball_pit_on`] with this many balls.
    BallPit { balls: usize },
    /// [`plane_on`].
    Plane,
}

/// What M1's session genesis is built from ([`m1_session`]): the preset and its arguments, and the settings and
/// members a session's genesis record carries (pe-app's `genesis`).
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub seed: u64,
    /// The floor's top and the domain's physics origin, as for [`ball_pit_with`] and [`plane_on`].
    pub origin: Pos,
    pub preset: Preset,
    /// The session's line: [`genesis_line`] of its genesis record without the root, never [`m1_line`].
    pub line: LineId,
    pub rate: SimRate,
    /// Metres per second squared, in the world frame.
    pub gravity: [f32; 3],
    /// pe-agree's settings record (6.6, config item 4), opaque here. M1's genesis passes pe-agree's 32-byte
    /// `NodeConfig::consensus_hash`, so in M1 config item 4 holds that hash.
    pub agree: Vec<u8>,
    /// Every member with its permission bits ([`perm`]), in any order; they are admitted in key order. The genesis
    /// record decides the bits (pe-app maps each member's role to them), so a change to them moves the root.
    pub members: Vec<(MemberKey, u32)>,
}

/// M1's session genesis with every member at frame 0: the interim until a join from a cut exists (@coordinator-opus). The preset's world ([`ball_pit_on`] or [`plane_on`]) on the session's line ([`genesis_line`]), at
/// its rate, with its gravity and pe-agree's settings record. Each member is admitted at frame 0 with its permission
/// bits through [`World::admit`] (the function a join entry calls), in key order, standing 0.6 m apart along the
/// genesis row (just outside the pit's near wall; on the plane, the same spots). Deterministic: the same `Session` gives the same world and the same root on every machine, which is what a
/// second machine checks against its record before it starts. A key listed twice is refused (`WorldError::Duplicate`,
/// from its second admission).
pub fn m1_session(s: &Session, blobs: BlobStore) -> Result<World, crate::WorldError> {
    let mut w = match s.preset {
        Preset::BallPit { balls } => ball_pit_on(s.line, s.seed, blobs, s.origin, balls),
        Preset::Plane => plane_on(s.line, s.seed, blobs, s.origin),
    };
    w.set_rate(s.rate);
    w.set_gravity(s.gravity);
    w.set_agree_settings(&s.agree);
    let mut members = s.members.clone();
    members.sort_by_key(|m| m.0);
    // A key listed twice: `admit` would take the second as a returning member, so it is refused here.
    if let Some(w2) = members.windows(2).find(|w2| w2[0].0 == w2[1].0) {
        return Err(crate::WorldError::Duplicate(ObjectId::hand(s.line, w2[1].0, 0, Frame(0))));
    }
    for (i, &(m, perms)) in members.iter().enumerate() {
        w.admit(s.line, m, perms, Frame(0), slot_feet(s.origin, i))?;
    }
    Ok(w)
}

/// The coordinates the ball pit's ids derive from (2.1): world logic on `line`, with a chain from the seed.
fn draft(line: LineId, seed: u64) -> Draft {
    draft_in("pe.m1.ball-pit.chain", line, seed)
}

fn draft_in(context: &str, line: LineId, seed: u64) -> Draft {
    let mut h = Hasher::new(context);
    h.write_u64(seed);
    Draft { author: MemberKey::LOGIC, line, chain: ChainId(h.finish()), seq: 0 }
}

// ---- the plane ----------------------------------------------------------------------------------------------------

/// The names the plane preset gives its objects (world-model.md 2.4, "The plane"): pe-app's spawn buttons and the
/// self-tests find them by these. `Templates` is a template folder ([`crate::templates::FOLDER`]).
pub mod plane_names {
    pub const ROOT: &str = "Plane";
    pub const FLOOR: &str = "Floor";
    pub const LAMP: &str = "Lamp";
    /// The empty root spawned objects go under.
    pub const SPAWNED: &str = "Spawned";
    pub const TEMPLATES: &str = crate::templates::FOLDER;
    /// The templates, in their folder's order.
    pub const BOX: &str = "Box";
    pub const SPHERE: &str = "Sphere";
    pub const CAPSULE: &str = "Capsule";
    /// A plain parent over [`STACK_BOXES`] box bodies, `Box 1` at the bottom.
    pub const STACK: &str = "Box stack";
    pub const STACK_BOX_PREFIX: &str = "Box ";
    pub const STACK_BOXES: usize = 5;
}

/// The plane's floor: a static box this many metres on a side, 1 m thick, its top at the origin.
pub const PLANE_SIDE: f64 = 20.0;
/// How far below the floor's top the template folder is parked, in metres: out of sight, though it is inert anyway.
pub const TEMPLATES_DEPTH: f64 = 50.0;
/// The templates' density, kg/m³: a 0.2 m box is 2.4 kg, light enough to throw and heavy enough to stack.
pub const TEMPLATE_DENSITY: f32 = 300.0;
/// The template box's half extent, in metres.
pub const BOX_HALF: f32 = 0.1;
/// The template sphere's radius, in metres.
pub const SPHERE_R: f32 = 0.1;
/// The template capsule's radius and the half length of its segment, in metres.
pub const CAPSULE_R: f32 = 0.07;
pub const CAPSULE_HALF: f32 = 0.1;
/// A stack box's half extent, in metres.
pub const STACK_HALF: f32 = 0.075;
/// The gap under each box of the stack as it is copied, in metres, so none starts in contact with the next.
const STACK_GAP: f64 = 0.002;

/// The friend test's flat plane at frame 0 on line `line` (world-model.md 2.4, "The plane"; the friend-test
/// prototype): a root `Plane` holding a static floor [`PLANE_SIDE`] m square with its top at `origin`, and a lamp; an
/// empty root `Spawned`; and a template folder `Templates` [`TEMPLATES_DEPTH`] m below, holding a box, a sphere, a
/// capsule and a stack of boxes, each a ready dynamic, grabbable body. Spawning is a copy of a template under
/// `Spawned` (pe-api's `create` with `template`); the folder itself is inert ([`crate::templates`]). One genesis
/// transaction by world logic, as for [`ball_pit_on`], with the M1 marker. Deterministic: the same arguments give the
/// same world and root on every machine. `seed` only seeds the ids' chain.
///
/// # Panics
/// As [`ball_pit_on`]: if the preset's transaction is refused (a bug here; a test applies it), if `origin` is so near
/// the edge of the position range that the plane does not fit, or if `line` is the origin.
pub fn plane_on(line: LineId, seed: u64, blobs: BlobStore, origin: Pos) -> World {
    assert_ne!(line, LineId::origin(), "an M1 world is never on the origin's line");
    let mut w = World::new(seed, blobs.clone());
    w.set_origin(origin);
    w.m1_marker = Some(m1_line());
    w.line = line;
    // The plane gives each member a player body (8.2).
    crate::components::register_player(&mut w.store);
    let tx = plane_transaction(&blobs, origin);
    w.set_member(MemberKey::LOGIC, perm::EDIT);
    let (outcome, mut w) = w.dry_run_draft_state(Frame(0), &draft_in("pe.m1.plane.chain", line, seed), &tx);
    assert!(matches!(outcome, Outcome::Applied { .. }), "the plane transaction was refused: {outcome:?}");
    w.set_member(MemberKey::LOGIC, 0);
    w
}

fn dynamic_body() -> Row {
    row(Body {
        kind: body_kind::DYNAMIC,
        friction: 0.6,
        linear_damping: 0.1,
        angular_damping: 0.3,
        rolling_resistance: 0.05,
        gravity_scale: 1.0,
        may_sleep: 1,
        grabbable: 1,
        ..Body::default()
    })
}

fn template_shape(kind: u32, size: [f32; 3]) -> Row {
    row(Shape { kind, density: TEMPLATE_DENSITY, size, ..Shape::default() })
}

/// The plane's one transaction: `Plane`, its floor and lamp; `Spawned`; `Templates` and its templates. New index 0
/// is `Plane`; the rest follow in op order.
fn plane_transaction(blobs: &BlobStore, origin: Pos) -> Transaction {
    use plane_names as n;
    let mut used = Vec::new();
    let mut ops = Vec::new();
    let mut next = 0u32;
    let mut create = |ops: &mut Vec<Op>, parent: Target, rows: Vec<Row>| {
        let new = next;
        next += 1;
        ops.push(Op::Create { new, parent, index: None, rows });
        new
    };
    let plane = create(&mut ops, Target::Root, vec![row(Transform::at(origin)), name(blobs, &mut used, n::ROOT)]);
    let half = (PLANE_SIDE / 2.0) as f32;
    let mut rows = vec![row(Transform::local([0.0, -0.5, 0.0])), name(blobs, &mut used, n::FLOOR)];
    rows.push(colour([0.42, 0.44, 0.46]));
    rows.extend(static_box([half, 0.5, half]));
    create(&mut ops, Target::New(plane), rows);
    let lamp =
        Light { kind: light_kind::POINT, intensity: 80.0, range: 16.0, color: [1.0, 0.95, 0.85], ..Light::default() };
    create(
        &mut ops,
        Target::New(plane),
        vec![row(Transform::local([0.0, 3.0, 0.0])), name(blobs, &mut used, n::LAMP), row(lamp)],
    );
    create(&mut ops, Target::Root, vec![row(Transform::at(origin)), name(blobs, &mut used, n::SPAWNED)]);
    let parked = origin.checked_add(Offset([0.0, -TEMPLATES_DEPTH, 0.0])).expect("the template folder fits");
    let folder = create(&mut ops, Target::Root, vec![row(Transform::at(parked)), name(blobs, &mut used, n::TEMPLATES)]);
    let under = Target::New(folder);
    let (b, r, c) = (f64::from(BOX_HALF), f64::from(SPHERE_R), f64::from(CAPSULE_R + CAPSULE_HALF));
    let singles = [
        (n::BOX, [-1.5, b, 0.0], [0.85, 0.2, 0.15], shape_kind::BOX, [BOX_HALF; 3]),
        (n::SPHERE, [-0.5, r, 0.0], [0.2, 0.45, 0.9], shape_kind::SPHERE, [SPHERE_R, 0.0, 0.0]),
        (n::CAPSULE, [0.5, c, 0.0], [0.2, 0.7, 0.3], shape_kind::CAPSULE, [CAPSULE_R, CAPSULE_HALF, 0.0]),
    ];
    for (label, at, rgb, kind, size) in singles {
        let rows = vec![
            row(Transform::local(at)),
            name(blobs, &mut used, label),
            colour(rgb),
            dynamic_body(),
            template_shape(kind, size),
        ];
        create(&mut ops, under, rows);
    }
    let stack = create(&mut ops, under, vec![row(Transform::local([1.5, 0.0, 0.0])), name(blobs, &mut used, n::STACK)]);
    let side = 2.0 * f64::from(STACK_HALF) + STACK_GAP;
    for k in 0..n::STACK_BOXES {
        let rows = vec![
            row(Transform::local([0.0, f64::from(STACK_HALF) + STACK_GAP + k as f64 * side, 0.0])),
            name(blobs, &mut used, &format!("{}{}", n::STACK_BOX_PREFIX, k + 1)),
            colour([0.95, 0.6, 0.15]),
            dynamic_body(),
            template_shape(shape_kind::BOX, [STACK_HALF; 3]),
        ];
        create(&mut ops, Target::New(stack), rows);
    }
    Transaction {
        inline: vec![],
        actor: Actor::Person,
        label: Some("M1 plane (disposable genesis)".into()),
        preconditions: Vec::new(),
        ops,
        blobs: used,
    }
}

fn row<T: Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn name(blobs: &BlobStore, used: &mut Vec<BlobRef>, text: &str) -> Row {
    let r = blobs.put(text.as_bytes());
    if !used.contains(&r) {
        used.push(r);
    }
    row(Name { text: r })
}

fn static_box(half: [f32; 3]) -> [Row; 2] {
    [
        row(Body { kind: body_kind::STATIC, friction: 0.6, ..Body::default() }),
        row(Shape { kind: shape_kind::BOX, density: 1.0, size: half, ..Shape::default() }),
    ]
}

fn colour(base: [f32; 3]) -> Row {
    row(Appearance { base: [base[0], base[1], base[2], 1.0], roughness: 0.6, ..Appearance::default() })
}

/// The ball colours, cycled.
const PALETTE: [[f32; 3]; 6] =
    [[0.85, 0.15, 0.15], [0.95, 0.65, 0.1], [0.95, 0.9, 0.2], [0.2, 0.7, 0.3], [0.2, 0.45, 0.9], [0.6, 0.3, 0.8]];

/// The preset's one transaction: the pit's root, its floor, four walls and lamp, then the balls, each a `Create`
/// under the root. New index 0 is the root; the rest follow in op order.
fn transaction(seed: u64, blobs: &BlobStore, origin: Pos, balls: usize) -> Transaction {
    let mut used = Vec::new();
    let mut ops = Vec::new();
    let mut next = 0u32;
    let mut create = |ops: &mut Vec<Op>, parent: Target, rows: Vec<Row>| {
        let new = next;
        next += 1;
        ops.push(Op::Create { new, parent, index: None, rows });
        new
    };
    let root = create(&mut ops, Target::Root, vec![row(Transform::at(origin)), name(blobs, &mut used, names::ROOT)]);
    let under = Target::New(root);
    let (w, h) = (PIT_W, WALL_H);
    let floor = colour([0.35, 0.35, 0.38]);
    let mut rows = vec![row(Transform::local([0.0, -0.5, 0.0])), name(blobs, &mut used, names::FLOOR), floor];
    rows.extend(static_box([2.0, 0.5, 2.0]));
    create(&mut ops, under, rows);
    let walls: [([f64; 3], [f32; 3]); 4] = [
        ([w + 0.1, h, 0.0], [0.1, h as f32, (w + 0.2) as f32]),
        ([-w - 0.1, h, 0.0], [0.1, h as f32, (w + 0.2) as f32]),
        ([0.0, h, w + 0.1], [(w + 0.2) as f32, h as f32, 0.1]),
        ([0.0, h, -w - 0.1], [(w + 0.2) as f32, h as f32, 0.1]),
    ];
    for (k, (p, half)) in walls.into_iter().enumerate() {
        let mut rows = vec![row(Transform::local(p)), name(blobs, &mut used, names::WALLS[k])];
        rows.push(colour([0.55, 0.5, 0.45]));
        rows.extend(static_box(half));
        create(&mut ops, under, rows);
    }
    let lamp =
        Light { kind: light_kind::POINT, intensity: 40.0, range: 8.0, color: [1.0, 0.95, 0.85], ..Light::default() };
    create(
        &mut ops,
        under,
        vec![row(Transform::local([0.0, 2.5, 0.0])), name(blobs, &mut used, names::LAMP), row(lamp)],
    );
    // The old prototype's layout: rows of 16 across the pit, layered upwards, with a small jitter (here from the
    // seed too) so the pile settles unevenly.
    let spacing = 2.0 * w / BALLS_PER_ROW as f64;
    let phase = (seed % 1_000_003) as f64;
    for i in 0..balls {
        let (ix, iz, iy) =
            (i % BALLS_PER_ROW, (i / BALLS_PER_ROW) % BALLS_PER_ROW, i / (BALLS_PER_ROW * BALLS_PER_ROW));
        let jitter = ((i as f64 + phase) * 0.618_034).fract() * 0.02 - 0.01;
        let p = [
            -w + spacing * (ix as f64 + 0.5) + jitter,
            BALL_R + 0.01 + iy as f64 * (2.0 * BALL_R + 0.01),
            -w + spacing * (iz as f64 + 0.5) - jitter,
        ];
        let body = Body {
            kind: body_kind::DYNAMIC,
            friction: 0.6,
            linear_damping: 0.1,
            angular_damping: 0.3,
            rolling_resistance: 0.2,
            gravity_scale: 1.0,
            may_sleep: 1,
            grabbable: 1,
            ..Body::default()
        };
        let shape = Shape {
            kind: shape_kind::SPHERE,
            density: BALL_DENSITY,
            size: [BALL_R as f32, 0.0, 0.0],
            ..Shape::default()
        };
        let rows = vec![
            row(Transform::local(p)),
            name(blobs, &mut used, &format!("{}{i}", names::BALL_PREFIX)),
            colour(PALETTE[i % PALETTE.len()]),
            row(body),
            row(shape),
        ];
        create(&mut ops, under, rows);
    }
    Transaction {
        inline: vec![],
        actor: Actor::Person,
        label: Some("M1 ball pit (disposable genesis)".into()),
        preconditions: Vec::new(),
        ops,
        blobs: used,
    }
}

/// Where a member's hands start, relative to the spawn point: left, then right.
const HAND_OFFSETS: [[f64; 3]; 2] = [[-0.25, 1.2, 0.3], [0.25, 1.2, 0.3]];

/// The spacing of the spawn slots along the row just outside the pit's near wall.
const SLOT_SPACING: f64 = 0.6;

/// Slot `i`'s feet: `origin + (0.6 i, 0, 1.5)`, the row genesis admits its members along ([`m1_session`]).
fn slot_feet(origin: Pos, i: usize) -> Pos {
    origin.checked_add(Offset([SLOT_SPACING * i as f64, 0.0, 1.5])).unwrap_or(origin)
}

/// Whether a hand at `hand` takes the slot whose feet are `feet`: it is horizontally (x, z) closer than half the
/// spacing to the midpoint of where that slot's two hands start. A hand takes at most one slot that way.
fn takes(hand: Pos, feet: Pos) -> bool {
    let mid = [(HAND_OFFSETS[0][0] + HAND_OFFSETS[1][0]) / 2.0, (HAND_OFFSETS[0][2] + HAND_OFFSETS[1][2]) / 2.0];
    let d = hand - feet;
    let (dx, dz) = (d.0[0] - mid[0], d.0[2] - mid[1]);
    let r = SLOT_SPACING / 2.0;
    dx * dx + dz * dz < r * r
}

impl World {
    /// Where a member admitted now stands (world-model.md 2.4, @engine-architect): the feet of the
    /// lowest slot along the genesis row that no current member's hand takes ([`takes`]). A function of the world
    /// alone, since the hands' poses are hashed state, so every replica applying an `Admit` at the same frame admits
    /// at the same spot; it keeps no counter, so it changes no root. A leaver's slot comes free when its hands go,
    /// and a slot someone's tracked hands still crowd is skipped. `n` hands take at most `n` slots, so one of the
    /// first `n + 1` is free.
    pub fn join_spawn(&self) -> Pos {
        let hands: Vec<Pos> =
            self.store.iter::<Hand>().filter_map(|(id, _)| self.store.world_transform(id).map(|t| t.pos)).collect();
        (0..=hands.len())
            .map(|i| slot_feet(self.origin, i))
            .find(|&feet| !hands.iter().any(|&h| takes(h, feet)))
            .unwrap_or_else(|| slot_feet(self.origin, hands.len()))
    }

    /// The M1 marker (`Some(m1_line())`) if this world was made by M1's disposable genesis code ([`ball_pit`]).
    pub fn m1_marker(&self) -> Option<LineId> {
        self.m1_marker
    }

    /// A member's join taking effect at `frame` (section 8): world logic records the member with `perms` and creates
    /// its two hands, owned by its key, with ids `ObjectId::hand(line, member, side, frame)`, as roots near `spawn`.
    /// Every replica calls it at the same frame (it is not an edit, since no edit writes a `Hand` row, 7.6). Returns
    /// the hands, left then right.
    ///
    /// **A player body** (8.2), in a world whose preset registered [`Player`] (the plane): the member also gets a body,
    /// `ObjectId::body(line, member, 0, frame)`, a root with its feet at `spawn`, owned by its key: the avatar root
    /// (3.5). Its hands are its children, at the same offsets from the feet. A world without `Player` (the ball pit)
    /// makes root hands as before, so its roots do not change.
    ///
    /// A rejoin at a later frame makes new hands (and a new body; the ids include the frame), once [`World::leave`]
    /// has removed the old ones.
    ///
    /// **A returning member** (F3's n = 2 branch, world-model.md 8; @diverge-eng's design v5, L3): a key that is
    /// already a member of this world keeps its permissions, its hands and what they hold. Only a hand side it lacks
    /// is created, with this admission's id; one with both hands changes nothing, and the hands it had are returned.
    /// A branch removes no member, so B's admission on the new line finds B's rows from the old one. pe-agree's
    /// epoch, not the world, decides whether a key votes.
    pub fn admit(
        &mut self,
        line: LineId,
        member: MemberKey,
        perms: u32,
        frame: Frame,
        spawn: Pos,
    ) -> Result<[ObjectId; 2], crate::WorldError> {
        let returning = self.members.get(&member).copied();
        let bodies = self.store.registry().get(crate::components::ids::PLAYER).is_some();
        let had_body = if bodies && returning.is_some() {
            self.store.iter::<Player>().filter(|(_, p)| p.owner == member).map(|(b, _)| b).min()
        } else {
            None
        };
        let body = bodies.then(|| had_body.unwrap_or_else(|| ObjectId::body(line, member, 0, frame)));
        if let Some(b) = body
            && had_body.is_none()
            && self.store.exists(b)
        {
            return Err(crate::WorldError::Duplicate(b));
        }
        let mut had: [Option<ObjectId>; 2] = [None, None];
        if returning.is_some() {
            for (id, h) in self.store.iter::<Hand>() {
                if h.owner == member
                    && let Some(slot) = had.get_mut(h.side as usize)
                {
                    *slot = Some(slot.map_or(id, |s: ObjectId| s.min(id)));
                }
            }
        }
        let ids = [0u8, 1].map(|side| had[side as usize].unwrap_or_else(|| ObjectId::hand(line, member, side, frame)));
        for (side, id) in ids.into_iter().enumerate() {
            if had[side].is_none() && self.store.exists(id) {
                return Err(crate::WorldError::Duplicate(id));
            }
        }
        let place = |side: usize| {
            spawn
                .checked_add(Offset(HAND_OFFSETS[side]))
                .map_err(|_| crate::WorldError::Store(pe_core::Error::OutOfRange { object: ids[side] }))
        };
        let places = [place(0)?, place(1)?];
        // Everything that can refuse was checked above, so the writes below cannot fail; they go to a copy anyway,
        // swapped in only when all succeed, so an `Err` never leaves a member with one hand (@chunks-reviewer's Low 1).
        if had.iter().all(Option::is_some) && body.is_none_or(|_| had_body.is_some()) {
            return Ok(ids);
        }
        let mut w = self.clone();
        if let Some(b) = body
            && had_body.is_none()
        {
            w.create(b, None, MemberKey::LOGIC, crate::GENESIS)?;
            w.insert(b, Transform::at(spawn))?;
            w.insert(b, Player { owner: member, prev_feet: (spawn - Pos::ORIGIN).0, ..Player::default() })?;
            w.insert(b, SimSize { metres: 1.0 })?;
        }
        for (side, (id, at)) in ids.into_iter().zip(places).enumerate() {
            if had[side].is_some() {
                continue;
            }
            w.create(id, body, MemberKey::LOGIC, crate::GENESIS)?;
            match body {
                Some(_) => w.insert(id, Transform { pos: HAND_OFFSETS[side], ..Transform::IDENTITY })?,
                None => w.insert(id, Transform::at(at))?,
            }
            w.insert(id, Hand { owner: member, side: side as u32, ..Hand::default() })?;
            w.insert(id, SimSize { metres: 1.0 })?;
        }
        w.set_member(member, returning.unwrap_or(perms));
        *self = w;
        Ok(ids)
    }
}

/// The first object named `name` in a depth-first walk of the world's roots and their subtrees, children in their
/// order. How tests, the harness and examples find the preset's objects, never by id.
pub fn find(world: &World, name: &str) -> Option<ObjectId> {
    let snap = world.snapshot(world.frame());
    let mut stack: Vec<ObjectId> = snap.roots();
    stack.reverse();
    while let Some(id) = stack.pop() {
        if snap.name(id).as_deref() == Some(name) {
            return Some(id);
        }
        let mut kids = snap.children(id);
        kids.reverse();
        stack.extend(kids);
    }
    None
}
