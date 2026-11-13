//! pe-world's [`World`] as the replica's [`WorldModel`] (replica.md 8.1; world-model.md 9.2), agreed with
//! @world-model (m9249bf73037f, m20847ce60303, meef9a881048b). pe-world's side is its `replica` calls (its slice A).
//!
//! - **The domain:** pe-world has none, so [`PeWorld`] carries it.
//! - **`apply`:** edits go through `World::apply_edit`, the one edit path, and inputs through `World::apply_input`
//!   (pe-world's slice B). The `Footprint` becomes the physics [`Effects`] here by comparing each affected body's row
//!   before and after the entry: a body that appears is `Created`, one that goes is `Removed`, changed shapes or
//!   fields are `Rebuilt`, and a moved world pose is `Teleported`. A body whose shapes and pose both changed gets
//!   `Rebuilt` and then `Teleported`, since a rebuild keeps a moving body's physics pose. Affected bodies: those of
//!   every object the entry created, removed, wrote a row of or reparented, with each object's body being itself or
//!   its nearest ancestor with a `Body` (a compound child's), before and after; for a `Transform` write or a
//!   reparent, those of the whole subtree. A hand input writes its hand's `Transform` for its own frame, so it counts
//!   as one: a body under the hand moves with it on that frame. The pack's later poses are written by `end_frame`,
//!   which gives no effects, so such a body does not follow them yet (@design-reviewer's Medium on e6d3ee9c). Grabs
//!   hold by claims, not parenting, and the M1 ball pit puts no body under a hand. An input's pushes become
//!   [`PushFx`], one for one.
//! - **Entry classes:** edits and inputs as above. Governance and Protocol entries are `Applied` with no effects, as
//!   pe-world applies neither yet.
//! - **The order within a frame:** [`WorldModel::phase`] is pe-world's `input_phase`, so a frame's hand inputs apply
//!   before its other entries, and a grab's reach is measured after the frame's hand movement (world-model.md 8.1).
//! - **`end_frame(f)`:** `World::end_frame(f)`, which expires claims and push windows and writes each pending hand
//!   pack's pose for `f + 1`, then the frame becomes `f + 1`.
//!   `World::frame()` is the next frame to step, so a cut's root is taken when it equals the cut's frame.
//! - **The tree** (pe-world's slice C, world-model.md 6.2-6.6): `root` is the world root of pe-world's hash tree for
//!   this domain, `tree` is pe-world's `WorldTree` of a saved world, and `import` is `World::import`, which checks
//!   every record against the target root before anything applies and takes every setting from the config record.
//!   The replica compares the patched root with the target, which covers absent leaves and a missing config record.
//!   `empty` is a bare `World::new` with the engine registry and default settings but this machine's rate, which a
//!   joiner imports into.
//! - **Hands:** every `Hand` row, in id order. The target is the hand object's world transform, which its inputs
//!   write (and `end_frame` writes a pending pack's next pose). The rest is the row's: `span` counts frames, and
//!   `previous` is the input before, when there was one (@world-model, m419e8711ae50, mc69f033224d1). A hand whose
//!   object has no world transform is left out.
//! - **Claims:** every claim, with its hand. An agent's grab has no hand (`ObjectId::NONE`), so it gets no joint.
//! - **The view** (S5): every object with a `Shape`, a `Light` or a `Hand`, at its world transform, with its
//!   shape in its own units (the view's scale scales it), appearance, light, hand owner and holder. An object with no
//!   world transform (no `Transform`, or out of range) is not drawn; a hand without a `Shape` is drawn as a
//!   [`HAND_RADIUS`] sphere. A compound child without a body of its own is published with no velocity (nothing in
//!   M1's ball pit is compound). A spot light carries its cone angles from pe-world's `Light` into S5's `Spot`.
//!
//! **Shapes** (replica.md 2.1): a body's shapes are its own `Shape`, then those of its compound children (the
//! descendants reached without passing another `Body`) in id order, each placed in the body's frame and scaled by
//! its object's world scale. A child's place composes the local transforms from the body down to it, starting from
//! the body's world scale, so it does not depend on the body's pose; an object without a `Transform` sits at its
//! frame (world-model.md 3.3). Boxes scale per axis. A sphere or capsule under non-uniform scale, a mesh, a hull or an
//! unknown kind cannot be built yet, and the object has no body.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pe_seams::{
    Class, DomainId, EntryRef, Frame, Hash, Holder, ImportError, LightKind, ObjectClass, ObjectId, Outcome, Pos,
    SignedEntry, Tree, Version, ViewFlags, ViewObject,
};
use pe_sim::transform::rotate;
use pe_sim::{Component, SimStore, Transform, WorldTransform};
use pe_world::components::{Appearance, Body, BodyState, Hand, Light, Shape, body_kind, light_kind, shape_kind};
use pe_world::{Footprint, World, input_phase};

use crate::world::{
    BodyKind, BodyRow, BodyStateRow, Change, ClaimRow, Effects, HAND_RADIUS, HandRow, Phase, PhysicsSettings, PushFx,
    ShapePart, ShapeRow, WorldModel,
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

    /// pe-world's `input_phase`: 0, a hand input, is `First`; anything else is `Second`.
    fn phase(entry: &SignedEntry) -> Phase {
        if input_phase(entry) == 0 { Phase::First } else { Phase::Second }
    }

    fn apply(&mut self, frame: Frame, entry: &SignedEntry) -> (Outcome, Effects) {
        match entry.entry.class {
            class @ (Class::Edit | Class::Input) => {
                let before = self.world.clone();
                let (outcome, footprint) = if class == Class::Edit {
                    self.world.apply_edit(frame, entry)
                } else {
                    self.world.apply_input(frame, entry)
                };
                let effects = match outcome {
                    Outcome::Applied { .. } => Effects {
                        // pe-world reports the re-keyed objects in id order, tombstoned ones included (7.9).
                        rekeyed: footprint
                            .rekey
                            .as_ref()
                            .map_or_else(Default::default, |r| r.objects.as_slice().into()),
                        ..effects(&before, &self.world, &footprint)
                    },
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
        let store = self.world.store();
        let mut hands: Vec<HandRow> = store
            .iter::<Hand>()
            .filter_map(|(hand, h)| {
                let at = store.world_transform(hand)?;
                Some(HandRow {
                    hand,
                    owner: h.owner,
                    target: at.pos,
                    rotation: at.rot,
                    grip: h.grip,
                    engaged: h.engaged != 0,
                    stamp: Frame(h.stamp),
                    span: h.span,
                    previous: (h.has_prev != 0).then_some((Frame(h.prev_stamp), h.prev_span)),
                    mode: h.mode,
                })
            })
            .collect();
        hands.sort_unstable_by_key(|h| h.hand);
        hands
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

    fn snapshot(&self, final_frame: Frame) -> Option<Arc<dyn Any + Send + Sync>> {
        let snap: Arc<dyn Any + Send + Sync> = self.world.snapshot(final_frame);
        Some(snap)
    }

    fn view_objects(&self) -> Vec<ViewObject> {
        let s = self.world.store();
        let mut ids: BTreeSet<ObjectId> = s.iter::<Shape>().map(|(id, _)| id).collect();
        ids.extend(s.iter::<Light>().map(|(id, _)| id));
        ids.extend(s.iter::<Hand>().map(|(id, _)| id));
        let held: BTreeMap<ObjectId, Holder> =
            self.world.claims().map(|(object, c)| (object, Holder { member: c.member, hand: c.hand })).collect();
        ids.into_iter()
            .filter_map(|id| {
                let t = s.world_transform(id)?;
                // pe-world writes a hand's side as 0 or 1; any other value is a bad row, drawn as no hand.
                let hand = s.get::<Hand>(id).filter(|h| h.side <= 1);
                let own = s.get::<Shape>(id).filter(|sh| view_shape(sh).is_some());
                // A hand without a shape of its own is drawn as the sphere it will push with.
                let shape =
                    own.and_then(view_shape).or_else(|| hand.map(|_| pe_seams::Shape::Sphere { radius: HAND_RADIUS }));
                let radius = match (own, hand) {
                    (Some(sh), _) => shape_radius(sh, t.scale),
                    (None, Some(_)) => HAND_RADIUS * largest(t.scale),
                    (None, None) => 0.0,
                };
                let node = s.node(id);
                Some(ViewObject {
                    id,
                    class: ObjectClass::World,
                    pos: t.pos,
                    rotation: t.rot,
                    scale: t.scale,
                    linear_velocity: [0.0; 3],
                    angular_velocity: [0.0; 3],
                    radius,
                    shape,
                    appearance: s.get::<Appearance>(id).map(view_appearance),
                    light: s.get::<Light>(id).and_then(view_light),
                    hand_of: hand
                        .map(|h| (h.owner, if h.side == 0 { pe_seams::Hand::Left } else { pe_seams::Hand::Right })),
                    held_by: held.get(&id).cloned(),
                    flags: ViewFlags::default(),
                    // The last edit that changed anything about the object (world-model.md 7.3), or none if no edit
                    // has written it: pe-world's GENESIS, for genesis and trusted engine code.
                    version: Some(Version { entry: EntryRef(Hash(node.version_entry)), op: node.version_op })
                        .filter(|v| *v != pe_world::GENESIS),
                })
            })
            .collect()
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
        // The last write before a cut's root (the replica's take_root): store the leaf hash of every object written
        // since the last cut, so this root and later ones rehash only what changed (world-model.md 6.4;
        // @chunks-reviewer's Low on 5e7effd2). Roots are the same either way.
        self.world.refresh_tree_cache();
    }

    fn root(&self) -> Hash {
        self.world.root_hash(self.domain)
    }

    fn save(&self) -> World {
        self.world.clone()
    }

    fn restore(&mut self, saved: &World) {
        self.world = saved.clone();
    }

    fn tree<'a>(&'a self, saved: &'a World) -> Box<dyn Tree + 'a> {
        Box::new(saved.tree(self.domain))
    }

    /// A bare world (engine registry, default settings) at this machine's rate, the welcome's: pe-world's import
    /// refuses a config record at another rate than its base's (world-model.md 6.6), so a base at the default rate
    /// would refuse every world not at 60 Hz, whatever rate this replica runs at.
    fn empty(&self) -> World {
        let mut w = World::new(0, self.world.blobs().clone());
        w.set_rate(self.world.rate());
        w
    }

    fn import(&self, saved: &World, import: &pe_seams::CutImport) -> Result<World, ImportError> {
        saved.import(self.domain, import)
    }
}

/// pe-world's shape as the view's (S5), in the object's own units; `None` for an unknown kind or a missing mesh.
fn view_shape(sh: &Shape) -> Option<pe_seams::Shape> {
    match sh.kind {
        shape_kind::BOX => Some(pe_seams::Shape::Box { half_extents: sh.size }),
        shape_kind::SPHERE => Some(pe_seams::Shape::Sphere { radius: sh.size[0] }),
        shape_kind::CAPSULE => Some(pe_seams::Shape::Capsule { radius: sh.size[0], half_height: sh.size[1] }),
        shape_kind::MESH | shape_kind::HULL if !sh.mesh.is_none() => {
            Some(pe_seams::Shape::Mesh(pe_seams::BlobRef(Hash(sh.mesh.hash))))
        }
        _ => None,
    }
}

/// The bounding radius the view culls by (render.md 12): the shape's, world scale applied. A mesh's or a hull's
/// `size` is the half extents of its bounding box (world-model.md 8.1), so it is bounded like a box.
fn shape_radius(sh: &Shape, scale: [f32; 3]) -> f32 {
    match sh.kind {
        shape_kind::SPHERE => sh.size[0] * largest(scale),
        shape_kind::CAPSULE => (sh.size[0] + sh.size[1]) * largest(scale),
        _ => {
            let (x, y, z) = (sh.size[0] * scale[0], sh.size[1] * scale[1], sh.size[2] * scale[2]);
            (x * x + y * y + z * z).sqrt()
        }
    }
}

fn largest(scale: [f32; 3]) -> f32 {
    scale.iter().fold(0.0_f32, |m, s| m.max(s.abs()))
}

fn view_appearance(a: &Appearance) -> pe_seams::Appearance {
    pe_seams::Appearance {
        base_color: a.base,
        emissive: a.emissive,
        roughness: a.roughness,
        metalness: a.metalness,
        material: (!a.material.is_none()).then_some(pe_seams::BlobRef(Hash(a.material.hash))),
    }
}

/// pe-world's light as the view's: a spot light with its cone angles (radians from the object's local -z, which
/// pe-world keeps within 0 <= inner <= outer <= pi/2). `None` for a kind the view has no name for.
fn view_light(l: &Light) -> Option<pe_seams::Light> {
    let kind = match l.kind {
        light_kind::POINT => LightKind::Point,
        light_kind::SPOT => LightKind::Spot { inner_angle: l.inner_angle, outer_angle: l.outer_angle },
        light_kind::DIRECTIONAL => LightKind::Directional,
        _ => return None,
    };
    Some(pe_seams::Light { kind, color: l.color, intensity: l.intensity, range: l.range })
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
    let origin = WorldTransform { pos: Pos::ORIGIN, rot: IDENTITY_ROTATION, scale: at.scale };
    let mut shapes = Vec::new();
    if let Some(shape) = store.get::<Shape>(id) {
        shapes.push(part(shape, &origin)?);
    }
    for child in compound_children(world, id) {
        if let Some(shape) = store.get::<Shape>(child) {
            shapes.push(part(shape, &in_body(store, &origin, id, child)?)?);
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

/// Where a compound child sits in its body's frame: `origin` (the body's frame at `Pos::ORIGIN`, with the body's
/// world scale), composed with the local transforms of the objects from the body down to `child`. An object without
/// a `Transform` sits at its frame, so it adds nothing (chunks-reviewer's Medium and L1 on 48e4e521).
fn in_body(store: &SimStore, origin: &WorldTransform, body: ObjectId, child: ObjectId) -> Option<WorldTransform> {
    let mut chain = Vec::new();
    let mut at = child;
    while at != body {
        if let Some(t) = store.get::<Transform>(at) {
            chain.push(*t);
        }
        at = store.parent(at)?;
    }
    let mut placed = *origin;
    while let Some(t) = chain.pop() {
        placed = placed.compose(&t)?;
    }
    Some(placed)
}

/// One shape at `placed`, its place in its body's frame and its world scale. The body's own shape sits at the
/// body's origin, unturned.
fn part(shape: &Shape, placed: &WorldTransform) -> Option<ShapePart> {
    let center = (placed.pos - Pos::ORIGIN).0.map(|c| c as f32);
    let rotation = placed.rot;
    let scale = placed.scale.map(f32::abs);
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
    // A hand input writes its hand's `Transform`.
    for &id in &footprint.hands {
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
    let pushes = footprint.pushes.iter().map(|p| PushFx { object: p.object, impulse: p.impulse, at: p.at }).collect();
    Effects { changed, pushes, ..Effects::default() }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind of pe-world's `Light` the view names, a spot light with its cone; an unknown kind is not drawn.
    #[test]
    fn the_view_takes_each_light_kind_and_a_spot_lights_cone() {
        let light = |kind, inner_angle, outer_angle| Light {
            kind,
            intensity: 2.0,
            range: 5.0,
            color: [1.0, 0.9, 0.8],
            inner_angle,
            outer_angle,
        };
        let kind = |l: Light| view_light(&l).map(|v| v.kind);
        assert_eq!(kind(light(light_kind::POINT, 0.0, 0.0)), Some(LightKind::Point));
        assert_eq!(kind(light(light_kind::DIRECTIONAL, 0.0, 0.0)), Some(LightKind::Directional));
        let spot = view_light(&light(light_kind::SPOT, 0.3, 0.6)).expect("a spot light is drawn");
        assert_eq!(spot.kind, LightKind::Spot { inner_angle: 0.3, outer_angle: 0.6 });
        assert_eq!((spot.color, spot.intensity, spot.range), ([1.0, 0.9, 0.8], 2.0, 5.0));
        assert_eq!(kind(light(7, 0.0, 0.0)), None);
    }
}
