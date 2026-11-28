//! The local home domain, "My Domain" (design domain-eng d1, 1.1-1.4; the user's rulings, 11-27; @coordinator-opus).
//!
//! - **A home is an ordinary world** built at frame 0 from two parts: its **scenery** (a static floor, a lamp and the
//!   `World settings` root holding its limits), which genesis always makes and the save never keeps, and its
//!   **content** ([`crate::content::Content`]), the player's own objects, which the save keeps.
//! - **The starter set** ([`starter`]) is the content of a new home: the objects shipped with the game, each a
//!   ready physical object (physics on by default), laid out in a row ahead of where players stand.
//!   Later starter objects (artifacts, the pool table) are added here.
//! - **Built through the edit path:** the scenery is one transaction by world logic, as for the plane; the content
//!   follows as further transactions ([`crate::content::Content::transactions`]) on the same chain, so every row is
//!   checked as an edit's would be, and the objects' ids derive from the session's line (2.1).
//! - **Top-level objects are live, deeper ones too:** every object of the content takes part in play unless an
//!   explicit setting says otherwise: a static body, or a folder ([`crate::components::Folder`]), whose contents are
//!   stored, not played (the user's "by default they are rendered if saved in the top level").

use pe_core::{Frame, LineId, MemberKey, ObjectId, Pos};
use pe_seams::Outcome;
use pe_sim::Transform;

use super::{BOX_HALF, CAPSULE_HALF, CAPSULE_R, HEAVY, HEAVY_FRICTION, SPHERE_R, STACK_GAP, STACK_HALF, plane_names};
use super::{colour, draft_in, dynamic_body, dynamic_body_with, name, row, static_box, template_shape};
use crate::components::{Light, Shape, Starter, light_kind, shape_kind, starter_key};
use crate::content::{Builder, Content, ContentError};
use crate::edit::{Actor, Op, Target, Transaction};
use crate::world::{BlobStore, World, perm};

/// The names the home gives its scenery.
pub mod names {
    pub const FLOOR: &str = "Floor";
    pub const LAMP: &str = "Lamp";
    pub const SETTINGS: &str = "World settings";
}

/// The side of the home's square floor, in metres. Its top is at the world's origin.
pub const HOME_SIDE: f64 = 32.0;
/// The z of the starter row, relative to the origin (forward is -z; players stand at z +1.5).
pub const STARTER_ROW_Z: f64 = -1.0;
/// The spacing of the starter row along x.
pub const STARTER_STEP: f64 = 0.6;

/// The chain context of a home's genesis.
const CHAIN: &str = "pe.home.chain";

/// A home domain at frame 0, with the ids of its scenery roots (which the save leaves out).
#[derive(Clone, Debug)]
pub struct Home {
    pub world: World,
    /// The scenery's roots: the floor, the lamp and `World settings`, in that order.
    pub scenery: Vec<ObjectId>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HomeError {
    /// The content can't be turned into transactions (a malformed placement, or a subtree too large for one edit).
    Content(ContentError),
    /// The edit path refused a transaction of the content: the content holds a row this build refuses.
    Refused(String),
}

impl From<ContentError> for HomeError {
    fn from(e: ContentError) -> Self {
        HomeError::Content(e)
    }
}

/// The home on line `line`: its scenery, then `content` placed at `origin` (the floor's top). Deterministic: the same
/// arguments give the same world and root on every machine. `seed` seeds the ids' chain.
///
/// # Panics
/// If the scenery's own transaction is refused (a bug here; a test applies it), or if `line` is the origin.
pub fn home_on(line: LineId, seed: u64, blobs: BlobStore, origin: Pos, content: &Content) -> Result<Home, HomeError> {
    assert_ne!(line, LineId::origin(), "a home is never on the origin's line");
    let mut w = World::new(seed, blobs.clone());
    w.set_origin(origin);
    w.line = line;
    crate::components::register_player(&mut w.store);
    crate::components::register_limits(&mut w.store);
    crate::components::register_folder(&mut w.store);
    let draft = draft_in(CHAIN, line, seed);
    let tx = scenery(&blobs, origin);
    let scenery = scenery_ids(line, seed).to_vec();
    debug_assert_eq!(tx.ops.len(), scenery.len());
    w.set_member(MemberKey::LOGIC, perm::EDIT);
    let (outcome, mut w) = w.dry_run_draft_state(Frame(0), &draft, &tx);
    assert!(matches!(outcome, Outcome::Applied { .. }), "the home's scenery was refused: {outcome:?}");
    let txs = content.transactions(w.store.registry(), &blobs, Target::Root, origin, Actor::Person, "My Domain")?;
    for (k, tx) in txs.iter().enumerate() {
        let d = crate::apply::Draft { seq: k as u64 + 1, ..draft };
        let (outcome, next) = w.dry_run_draft_state(Frame(0), &d, tx);
        if !matches!(outcome, Outcome::Applied { .. }) {
            w.set_member(MemberKey::LOGIC, 0);
            return Err(HomeError::Refused(format!("{outcome:?}")));
        }
        w = next;
    }
    w.set_member(MemberKey::LOGIC, 0);
    Ok(Home { world: w, scenery })
}

/// The ids of a home's scenery roots on `line` with `seed` (the floor, the lamp, `World settings`): what a save leaves
/// out ([`crate::World::content_roots`]). A function of the line and seed alone, so a running session needs no list.
pub fn scenery_ids(line: LineId, seed: u64) -> [ObjectId; 3] {
    let d = draft_in(CHAIN, line, seed);
    [0, 1, 2].map(|k| ObjectId::created(line, d.author, d.chain, 0, k))
}

/// The scenery's one transaction: `Floor`, `Lamp` and `World settings`, each a root, in that order.
fn scenery(blobs: &BlobStore, origin: Pos) -> Transaction {
    let mut used = Vec::new();
    let half = (HOME_SIDE / 2.0) as f32;
    let floor_at = origin.checked_add(pe_core::Offset([0.0, -0.5, 0.0])).expect("the home's floor fits");
    let mut floor = vec![row(Transform::at(floor_at)), name(blobs, &mut used, names::FLOOR)];
    floor.push(colour([0.42, 0.46, 0.44]));
    floor.extend(static_box([half, 0.5, half]));
    let lamp_at = origin.checked_add(pe_core::Offset([0.0, 3.0, 0.0])).expect("the home's lamp fits");
    let lamp =
        Light { kind: light_kind::POINT, intensity: 80.0, range: 16.0, color: [1.0, 0.95, 0.85], ..Light::default() };
    let lamp = vec![row(Transform::at(lamp_at)), name(blobs, &mut used, names::LAMP), row(lamp)];
    let limits = crate::components::Limits { max_shapes: crate::edit::MAX_SHAPES as u32 };
    let settings = vec![row(Transform::at(origin)), name(blobs, &mut used, names::SETTINGS), row(limits)];
    let ops = [floor, lamp, settings]
        .into_iter()
        .enumerate()
        .map(|(k, rows)| Op::Create { new: k as u32, parent: Target::Root, index: None, rows })
        .collect();
    Transaction {
        inline: vec![],
        actor: Actor::Person,
        label: Some("My Domain (scenery)".into()),
        preconditions: Vec::new(),
        ops,
        blobs: used,
    }
}

/// The starter set: the content of a new home (module doc). Each object is top-level, physical and grabbable: the box,
/// sphere and capsule of the plane's templates, its stack of boxes, and its three heavy objects, in one row along
/// [`STARTER_ROW_Z`], each resting on the floor.
pub fn starter() -> Content {
    use plane_names as n;
    let mut b = Builder::default();
    let (bx, r, c) = (f64::from(BOX_HALF), f64::from(SPHERE_R), f64::from(CAPSULE_R + CAPSULE_HALF));
    let singles = [
        (n::BOX, bx, [0.85, 0.2, 0.15], shape_kind::BOX, [BOX_HALF; 3]),
        (n::SPHERE, r, [0.2, 0.45, 0.9], shape_kind::SPHERE, [SPHERE_R, 0.0, 0.0]),
        (n::CAPSULE, c, [0.2, 0.7, 0.3], shape_kind::CAPSULE, [CAPSULE_R, CAPSULE_HALF, 0.0]),
    ];
    let mut x = -2.0 * STARTER_STEP;
    for (label, up, rgb, kind, size) in singles {
        let rows = [
            row(Transform::local([x, up, STARTER_ROW_Z])),
            colour(rgb),
            dynamic_body(),
            template_shape(kind, size),
            starter_row(label),
        ];
        b.top(&rows, label);
        x += STARTER_STEP;
    }
    let stack = b.top(&[row(Transform::local([x, 0.0, STARTER_ROW_Z])), starter_row(n::STACK)], n::STACK);
    let side = 2.0 * f64::from(STACK_HALF) + STACK_GAP;
    for k in 0..n::STACK_BOXES {
        let rows = [
            row(Transform::local([0.0, f64::from(STACK_HALF) + STACK_GAP + k as f64 * side, 0.0])),
            colour([0.95, 0.6, 0.15]),
            dynamic_body(),
            template_shape(shape_kind::BOX, [STACK_HALF; 3]),
        ];
        b.child(stack, &rows, &format!("{}{}", n::STACK_BOX_PREFIX, k + 1));
    }
    x += STARTER_STEP;
    for (label, half, density, rgb) in HEAVY {
        x += f64::from(half[0]);
        let rows = [
            row(Transform::local([x, f64::from(half[1]), STARTER_ROW_Z])),
            colour(rgb),
            dynamic_body_with(HEAVY_FRICTION),
            row(Shape { kind: shape_kind::BOX, density, size: half, ..Shape::default() }),
            starter_row(label),
        ];
        b.top(&rows, label);
        x += f64::from(half[0]) + STARTER_STEP;
    }
    b.finish()
}

fn starter_row(name: &str) -> crate::edit::Row {
    row(Starter { key: starter_key(name) })
}

/// "Restore starter set" (C7): the starter objects `world` no longer holds, as content to put back. An object counts
/// as held while any object anywhere in the world carries its [`Starter`] key (moved, renamed or put in a folder).
pub fn missing_starters(world: &World) -> Content {
    let have: std::collections::BTreeSet<u64> = world.store().iter::<Starter>().map(|(_, s)| s.key).collect();
    let all = starter();
    let tops: Vec<u32> = all.tops().collect();
    let mut out = Content { blobs: all.blobs.clone(), ..Content::default() };
    for (t, &top) in tops.iter().enumerate() {
        let end = tops.get(t + 1).copied().unwrap_or(all.items.len() as u32);
        let key = all.items[top as usize].rows.iter().find(|r| r.0 == crate::components::ids::STARTER).map(|r| {
            let s: Starter = bytemuck::pod_read_unaligned(&r.1);
            s.key
        });
        if key.is_some_and(|k| have.contains(&k)) {
            continue;
        }
        let base = out.items.len() as u32;
        for i in top..end {
            let mut it = all.items[i as usize].clone();
            it.parent = it.parent.map(|p| p - top + base);
            out.items.push(it);
        }
    }
    out
}
