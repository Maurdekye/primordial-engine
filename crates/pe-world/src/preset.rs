//! M1's ball pit and member admission (world-model.md 2.4: @coordinator-opus m949a335461b2, option
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
use pe_seams::Outcome;
use pe_sim::{Component, Transform};

use crate::apply::Draft;
use crate::components::{
    Appearance, BlobRef, Body, Hand, Light, Name, Shape, SimSize, body_kind, light_kind, shape_kind,
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
    let balls = balls.clamp(1, MAX_BALLS);
    let mut w = World::new(seed, blobs.clone());
    w.set_origin(origin);
    w.m1_marker = Some(m1_line());
    let tx = transaction(seed, &blobs, origin, balls);
    // World logic authors the preset. Logic is no member, so it holds `EDIT` only while the transaction applies.
    w.set_member(MemberKey::LOGIC, perm::EDIT);
    let (outcome, mut w) = w.dry_run_draft_state(Frame(0), &draft(seed), &tx);
    assert!(matches!(outcome, Outcome::Applied { .. }), "the ball-pit transaction was refused: {outcome:?}");
    w.set_member(MemberKey::LOGIC, 0);
    w
}

/// The coordinates the preset's ids derive from (2.1): world logic on [`m1_line`], with a chain from the seed.
fn draft(seed: u64) -> Draft {
    let mut h = Hasher::new("pe.m1.ball-pit.chain");
    h.write_u64(seed);
    Draft { author: MemberKey::LOGIC, line: m1_line(), chain: ChainId(h.finish()), seq: 0 }
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
    let lamp = Light { kind: light_kind::POINT, intensity: 40.0, range: 8.0, color: [1.0, 0.95, 0.85] };
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
        actor: Actor::Person,
        label: Some("M1 ball pit (disposable genesis)".into()),
        preconditions: Vec::new(),
        ops,
        blobs: used,
    }
}

/// Where a member's hands start, relative to the spawn point: left, then right.
const HAND_OFFSETS: [[f64; 3]; 2] = [[-0.25, 1.2, 0.3], [0.25, 1.2, 0.3]];

impl World {
    /// The M1 marker (`Some(m1_line())`) if this world was made by M1's disposable genesis code ([`ball_pit`]).
    pub fn m1_marker(&self) -> Option<LineId> {
        self.m1_marker
    }

    /// A member's join taking effect at `frame` (section 8): world logic records the member with `perms` and creates
    /// its two hands, owned by its key, with ids `ObjectId::hand(line, member, side, frame)`, as roots near `spawn`.
    /// Every replica calls it at the same frame (it is not an edit, since no edit writes a `Hand` row, 7.6). Returns
    /// the hands, left then right.
    ///
    /// Not yet: the avatar root that parents the hands (3.5), whose id derivation is open. A rejoin at a later frame
    /// makes two new hands (the ids include the frame); removing a leaver's hands is the leave's, not built yet.
    pub fn admit(
        &mut self,
        line: LineId,
        member: MemberKey,
        perms: u32,
        frame: Frame,
        spawn: Pos,
    ) -> Result<[ObjectId; 2], crate::WorldError> {
        let ids = [0u8, 1].map(|side| ObjectId::hand(line, member, side, frame));
        for id in ids {
            if self.store.exists(id) {
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
        let mut w = self.clone();
        for (side, (id, at)) in ids.into_iter().zip(places).enumerate() {
            w.create(id, None, MemberKey::LOGIC, crate::GENESIS)?;
            w.insert(id, Transform::at(at))?;
            w.insert(id, Hand { owner: member, side: side as u32, ..Hand::default() })?;
            w.insert(id, SimSize { metres: 1.0 })?;
        }
        w.set_member(member, perms);
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
