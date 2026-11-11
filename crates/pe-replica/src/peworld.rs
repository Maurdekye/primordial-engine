//! pe-world's [`World`] as the replica's [`WorldModel`] (replica.md 8.1; world-model.md 9.2), agreed with
//! @world-model (m9249bf73037f, m20847ce60303, meef9a881048b). pe-world's side is its `replica` calls (its slice A).
//!
//! - **The domain:** pe-world has none, so [`PeWorld`] carries it.
//! - **`apply`:** edits go through `World::apply_edit`, the one edit path. Its `Footprint` becomes the physics
//!   [`Effects`] here by comparing each affected body's row before and after the entry: a body that appears is
//!   `Created`, one that goes is `Removed`, changed shapes or fields are `Rebuilt`, and a moved world pose is
//!   `Teleported`. A body whose shapes and pose both changed gets `Rebuilt` and then `Teleported`, since a rebuild
//!   keeps a moving body's physics pose. Affected bodies: those of every object the entry created, removed, wrote a
//!   row of or reparented, with each object's body being itself or its nearest ancestor with a `Body` (a compound
//!   child's), before and after; for a `Transform` write or a reparent, those of the whole subtree.
//! - **Entry classes:** pe-world applies edits only. It refuses an input entry until its input slice (B). Governance
//!   and Protocol entries are `Applied` with no effects, as pe-world applies neither yet.
//! - **`end_frame(f)`:** `World::end_frame(f)`, which only expires, then the frame becomes `f + 1`.
//!   `World::frame()` is the next frame to step, so a cut's root is taken when it equals the cut's frame.
//! - **Until pe-world's hash tree (its slice C):** `root` is pe-world's stand-in root; `tree` gives that root and
//!   the frame, with no nodes, records or object hashes; `import` answers `NotKept`; `empty` is a fresh world.
//! - **Hands:** none until pe-world's input slice (B).
//!
//! **Shapes** (replica.md 2.1): a body's shapes are its own `Shape`, then those of its compound children (the
//! descendants reached without passing another `Body`) in id order, each placed in the body's frame and scaled by
//! its object's world scale. Boxes scale per axis. A sphere or capsule under non-uniform scale, a mesh, a hull or an
//! unknown kind cannot be built yet, and the object has no body.

use std::collections::BTreeSet;

use pe_seams::{
    Class, DomainId, Frame, Hash, ImportError, LeafKey, Mismatch, Node, ObjectId, Outcome, Pos, Record, SignedEntry,
    Tree, TreePath,
};
use pe_sim::transform::{quat_mul, rotate};
use pe_sim::{Component, Transform, WorldTransform};
use pe_world::components::{Body, BodyState, Shape, body_kind, shape_kind};
use pe_world::{Footprint, World};

use crate::world::{
    BodyKind, BodyRow, BodyStateRow, Change, ClaimRow, Effects, HandRow, PhysicsSettings, ShapePart, ShapeRow,
    WorldModel,
};

const IDENTITY_ROTATION: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// pe-world's world, for one domain.
#[derive(Clone, Debug)]
pub struct PeWorld {
    world: World,
    domain: DomainId,
}

impl PeWorld {
    pub fn new(world: World, domain: DomainId) -> PeWorld {
        PeWorld { world, domain }
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    /// The world, for trusted construction (genesis and tests) before the replica takes it.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }
}

impl WorldModel for PeWorld {
    type Saved = World;

    fn domain(&self) -> DomainId {
        self.domain
    }

    fn apply(&mut self, frame: Frame, entry: &SignedEntry) -> (Outcome, Effects) {
        match entry.entry.class {
            Class::Edit | Class::Input => {
                let before = self.world.clone();
                let (outcome, footprint) = self.world.apply_edit(frame, entry);
                let effects = match outcome {
                    Outcome::Applied { .. } => effects(&before, &self.world, &footprint),
                    _ => Effects::default(),
                };
                (outcome, effects)
            }
            Class::Governance | Class::Protocol => {
                let applied =
                    Outcome::Applied { created: Vec::new(), overwrote: Vec::new(), notes: Vec::new(), inverse: None };
                (applied, Effects::default())
            }
        }
    }

    fn end_frame(&mut self, frame: Frame) {
        self.world.end_frame(frame);
        self.world.set_frame(frame.next());
    }

    fn body(&self, id: ObjectId) -> Option<BodyRow> {
        body_row(&self.world, id)
    }

    fn physical(&self) -> Vec<ObjectId> {
        self.world.physical()
    }

    fn hands(&self) -> Vec<HandRow> {
        Vec::new()
    }

    fn claims(&self) -> Vec<ClaimRow> {
        self.world
            .claims()
            .map(|(object, c)| ClaimRow { object, hand: c.hand, member: c.member, since: c.since })
            .collect()
    }

    fn settings(&self) -> PhysicsSettings {
        let s = self.world.settings();
        PhysicsSettings { rate: s.rate, gravity: s.gravity, origin: s.origin }
    }

    fn write_poses(&mut self, poses: &[(ObjectId, Pos, [f32; 4])]) -> Vec<ObjectId> {
        self.world.write_poses(poses)
    }

    fn maintain_transforms(&mut self) -> u32 {
        u32::try_from(self.world.maintain_transforms()).unwrap_or(u32::MAX)
    }

    fn write_physics(&mut self, rows: &[(ObjectId, BodyStateRow)], global: Hash) {
        let rows: Vec<(ObjectId, BodyState)> = rows
            .iter()
            .map(|&(id, r)| {
                let state = BodyState {
                    linear: r.linear,
                    angular: r.angular,
                    sleep_time: r.sleep_time,
                    awake: u8::from(r.awake),
                    _pad: [0; 3],
                    digest: r.digest,
                };
                (id, state)
            })
            .collect();
        self.world.write_physics(&rows, global);
    }

    fn root(&self) -> Hash {
        self.world.root()
    }

    fn save(&self) -> World {
        self.world.clone()
    }

    fn restore(&mut self, saved: &World) {
        self.world = saved.clone();
    }

    fn tree<'a>(&'a self, saved: &'a World) -> Box<dyn Tree + 'a> {
        Box::new(StandInTree(saved))
    }

    fn empty(&self) -> World {
        World::new(0, self.world.blobs().clone())
    }

    fn import(&self, _saved: &World, _import: &pe_seams::CutImport) -> Result<World, ImportError> {
        Err(ImportError::NotKept)
    }
}

/// The tree until pe-world's hash tree: the stand-in root and the frame only.
struct StandInTree<'a>(&'a World);

impl Tree for StandInTree<'_> {
    fn root(&self) -> Hash {
        self.0.root()
    }
    fn frame(&self) -> Frame {
        self.0.frame()
    }
    fn node(&self, _path: &TreePath) -> Option<Node> {
        None
    }
    fn export(&self, _keys: &[LeafKey]) -> Vec<Record> {
        Vec::new()
    }
    fn object_hashes(&self, ids: &[ObjectId]) -> Vec<(ObjectId, Option<Hash>)> {
        ids.iter().map(|&id| (id, None)).collect()
    }
    fn verify(&self) -> Result<(), Mismatch> {
        Ok(())
    }
}

// ---- bodies ---------------------------------------------------------------------------------------------------------

/// The body row of `id`: `None` if it has no `Body`, no world transform, no shape, or a shape that cannot be built.
fn body_row(world: &World, id: ObjectId) -> Option<BodyRow> {
    let store = world.store();
    let body = store.get::<Body>(id)?;
    let kind = match body.kind {
        body_kind::STATIC => BodyKind::Static,
        body_kind::KINEMATIC => BodyKind::Kinematic,
        body_kind::DYNAMIC => BodyKind::Dynamic,
        _ => return None,
    };
    let at = store.world_transform(id)?;
    let mut shapes = Vec::new();
    if let Some(shape) = store.get::<Shape>(id) {
        shapes.push(part(shape, &at, None)?);
    }
    for child in compound_children(world, id) {
        if let Some(shape) = store.get::<Shape>(child) {
            shapes.push(part(shape, &at, Some(&store.world_transform(child)?))?);
        }
    }
    if shapes.is_empty() {
        return None;
    }
    Some(BodyRow {
        kind,
        pos: at.pos,
        rotation: at.rot,
        shapes,
        friction: body.friction,
        restitution: body.restitution,
        rolling_resistance: body.rolling_resistance,
        linear_damping: body.linear_damping,
        angular_damping: body.angular_damping,
        gravity_scale: body.gravity_scale,
        bullet: body.bullet != 0,
        may_sleep: body.may_sleep != 0,
        grabbable: body.grabbable != 0,
    })
}

/// The descendants of `body` reached without passing another `Body`, in id order.
fn compound_children(world: &World, body: ObjectId) -> Vec<ObjectId> {
    let store = world.store();
    let mut found = Vec::new();
    let mut stack: Vec<ObjectId> = store.children(body).collect();
    while let Some(x) = stack.pop() {
        if store.has::<Body>(x) {
            continue;
        }
        found.push(x);
        stack.extend(store.children(x));
    }
    found.sort_unstable();
    found
}

/// One shape in its body's frame. `own` is the shape's object's world transform when it is a compound child; the
/// body's own shape sits at the body's origin, unturned.
fn part(shape: &Shape, body: &WorldTransform, own: Option<&WorldTransform>) -> Option<ShapePart> {
    let (center, rotation, scale) = match own {
        None => ([0.0; 3], IDENTITY_ROTATION, body.scale),
        Some(o) => {
            let inverse = [-body.rot[0], -body.rot[1], -body.rot[2], body.rot[3]];
            let offset = rotate(inverse, (o.pos - body.pos).0);
            (offset.map(|c| c as f32), quat_mul(inverse, o.rot), o.scale)
        }
    };
    let scale = scale.map(f32::abs);
    let uniform = scale[0] == scale[1] && scale[1] == scale[2];
    let size = shape.size;
    let row = match shape.kind {
        shape_kind::BOX => ShapeRow::Box {
            center,
            rotation,
            half_extents: [size[0] * scale[0], size[1] * scale[1], size[2] * scale[2]],
        },
        shape_kind::SPHERE if uniform => ShapeRow::Sphere { center, radius: size[0] * scale[0] },
        shape_kind::CAPSULE if uniform => {
            let axis = rotate(rotation, [0.0, f64::from(size[1] * scale[0]), 0.0]).map(|c| c as f32);
            ShapeRow::Capsule {
                center1: [center[0] - axis[0], center[1] - axis[1], center[2] - axis[2]],
                center2: [center[0] + axis[0], center[1] + axis[1], center[2] + axis[2]],
                radius: size[0] * scale[0],
            }
        }
        _ => return None,
    };
    Some(ShapePart { shape: row, density: shape.density })
}

// ---- effects --------------------------------------------------------------------------------------------------------

/// An applied entry's physics effects, from the bodies it affected (module doc), in id order.
fn effects(before: &World, after: &World, footprint: &Footprint) -> Effects {
    let mut touched: BTreeSet<ObjectId> = BTreeSet::new();
    let mut moved: Vec<ObjectId> = Vec::new();
    touched.extend(&footprint.created);
    touched.extend(&footprint.removed);
    for &(id, component) in &footprint.rows {
        touched.insert(id);
        if component == Transform::ID {
            moved.push(id);
        }
    }
    for &(id, _) in &footprint.reparented {
        touched.insert(id);
        moved.push(id);
    }
    for top in moved {
        for world in [before, after] {
            touched.extend(subtree(world, top));
        }
    }
    let bodies: BTreeSet<ObjectId> =
        touched.iter().flat_map(|&id| [owner(before, id), owner(after, id)]).flatten().collect();
    let mut changed = Vec::new();
    for id in bodies {
        match (body_row(before, id), body_row(after, id)) {
            (None, Some(_)) => changed.push((id, Change::Created)),
            (Some(_), None) => changed.push((id, Change::Removed)),
            (Some(a), Some(b)) => {
                let reshaped = BodyRow { pos: a.pos, rotation: a.rotation, ..b.clone() } != a;
                if reshaped {
                    changed.push((id, Change::Rebuilt));
                }
                if a.pos != b.pos || a.rotation != b.rotation {
                    changed.push((id, Change::Teleported));
                }
            }
            (None, None) => {}
        }
    }
    Effects { changed, ..Effects::default() }
}

/// `id`'s body: itself if it has a `Body`, else its nearest ancestor with one.
fn owner(world: &World, id: ObjectId) -> Option<ObjectId> {
    let store = world.store();
    let mut at = id;
    for _ in 0..=pe_world::DEPTH_LIMIT_MAX {
        if !store.exists(at) {
            return None;
        }
        if store.has::<Body>(at) {
            return Some(at);
        }
        at = store.parent(at)?;
    }
    None
}

/// `top` and every descendant of it.
fn subtree(world: &World, top: ObjectId) -> Vec<ObjectId> {
    let store = world.store();
    if !store.exists(top) {
        return Vec::new();
    }
    let mut found = vec![top];
    let mut i = 0;
    while let Some(&x) = found.get(i) {
        found.extend(store.children(x));
        i += 1;
    }
    found
}
