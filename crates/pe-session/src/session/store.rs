//! S5b: the per-object engine tables as pe-sim components (design 6.1). The object table, grab claims,
//! claim cooldowns and hand idle counters live in a [`SimStore`], so a keyframe snapshots them
//! copy-on-write and a rollback restores them with the physics image. The world-level tables (escrow,
//! arrivals, provenance, the portal) stay a plain struct, [`Tables`].
//!
//! Object index `i` (the `obj` of a `Touch` or `Hand`) is the store object [`object_id`]`(i)`, the same
//! id as its body in the physics world. A destroyed object's index is never reused: the store's
//! creator counter only grows, and it is [`WorldSim::obj_slots`].

use std::mem::offset_of;
use std::sync::{Arc, OnceLock};

use bytemuck::{Pod, Zeroable};
use pe_sim::{Component, ComponentId, FieldDesc, FieldKind, Registry, SimStore};

use super::*;

/// pe-session's engine component ids: 16 to 31 of the engine's range (below
/// [`ComponentId::FIRST_USER`]).
pub mod component_ids {
    use pe_sim::ComponentId;
    pub const OBJ: ComponentId = ComponentId(16);
    pub const CLAIM: ComponentId = ComponentId(17);
    pub const COOLDOWN: ComponentId = ComponentId(18);
    pub const HAND_IDLE: ComponentId = ComponentId(19);
}

/// An object of the world's table: its global id (P2-5) and its kind (scene shapes; 4 is a hand).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct ObjRow {
    pub global: u64,
    pub kind: u32,
    pub _pad: u32,
}

/// A grab claim (D72): held by `member` until frame `until`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct ClaimRow {
    pub member: MemberId,
    pub _pad: u32,
    pub until: u64,
}

/// After a claim expires, its holder may not claim the object again before frame `until`, so repeated
/// grabs cannot monopolize an object (review M3).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct CooldownRow {
    pub member: MemberId,
    pub _pad: u32,
    pub until: u64,
}

/// An active hand: frames since its last target, that target's span (its target is reached over
/// `span` frames), and whether it continues a stream (1) or not (0): only a stream coasts past its
/// span (dead reckoning, D52, `SimConfig::hand_predict`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct HandIdle {
    pub frames: u64,
    pub span: u64,
    pub stream: u64,
}

impl Component for ObjRow {
    const ID: ComponentId = component_ids::OBJ;
    const NAME: &'static str = "pe-session.Obj";
    fn fields() -> Vec<FieldDesc> {
        vec![
            FieldDesc::new("global", FieldKind::U64, offset_of!(ObjRow, global), 1),
            FieldDesc::new("kind", FieldKind::U32, offset_of!(ObjRow, kind), 1),
            FieldDesc::new("_pad", FieldKind::U32, offset_of!(ObjRow, _pad), 1),
        ]
    }
}

impl Component for ClaimRow {
    const ID: ComponentId = component_ids::CLAIM;
    const NAME: &'static str = "pe-session.Claim";
    fn fields() -> Vec<FieldDesc> {
        vec![
            FieldDesc::new("member", FieldKind::U32, offset_of!(ClaimRow, member), 1),
            FieldDesc::new("_pad", FieldKind::U32, offset_of!(ClaimRow, _pad), 1),
            FieldDesc::new("until", FieldKind::U64, offset_of!(ClaimRow, until), 1),
        ]
    }
}

impl Component for CooldownRow {
    const ID: ComponentId = component_ids::COOLDOWN;
    const NAME: &'static str = "pe-session.Cooldown";
    fn fields() -> Vec<FieldDesc> {
        vec![
            FieldDesc::new("member", FieldKind::U32, offset_of!(CooldownRow, member), 1),
            FieldDesc::new("_pad", FieldKind::U32, offset_of!(CooldownRow, _pad), 1),
            FieldDesc::new("until", FieldKind::U64, offset_of!(CooldownRow, until), 1),
        ]
    }
}

impl Component for HandIdle {
    const ID: ComponentId = component_ids::HAND_IDLE;
    const NAME: &'static str = "pe-session.HandIdle";
    fn fields() -> Vec<FieldDesc> {
        vec![
            FieldDesc::new("frames", FieldKind::U64, offset_of!(HandIdle, frames), 1),
            FieldDesc::new("span", FieldKind::U64, offset_of!(HandIdle, span), 1),
            FieldDesc::new("stream", FieldKind::U64, offset_of!(HandIdle, stream), 1),
        ]
    }
}

/// The schema every replica of a session world registers: pe-sim's own components and the four above.
pub fn session_registry() -> Arc<Registry> {
    static REG: OnceLock<Arc<Registry>> = OnceLock::new();
    REG.get_or_init(|| {
        let mut r = Registry::new();
        r.register_type::<ObjRow>().expect("pe-session components");
        r.register_type::<ClaimRow>().expect("pe-session components");
        r.register_type::<CooldownRow>().expect("pe-session components");
        r.register_type::<HandIdle>().expect("pe-session components");
        Arc::new(r)
    })
    .clone()
}

/// The object index of a store object.
pub fn obj_index(id: ObjectId) -> u32 {
    id.counter - 1
}

pub(super) fn add_obj(store: &mut SimStore, o: Obj) {
    store.create_with_id(o.id).expect("a fresh object index");
    store.insert(o.id, ObjRow { global: o.global, kind: o.kind as u32, _pad: 0 }).expect("a live object");
}

/// The store of a new world: its scene's objects. A slot of `objs` that is `None` was never created
/// (`build_scene` fills every slot), so it stays a gap in the index range.
pub(super) fn object_store(objs: &[Option<Obj>]) -> SimStore {
    // No logic module draws randomness yet; the session seed arrives with them.
    let mut store = SimStore::new(session_registry(), 0);
    for o in objs.iter().flatten() {
        add_obj(&mut store, *o);
    }
    store
}

impl WorldSim {
    /// A live object of the table.
    pub fn live(&self, obj: u32) -> Option<Obj> {
        let id = object_id(obj as usize);
        self.store.get::<ObjRow>(id).map(|r| Obj { id, global: r.global, kind: r.kind as u8 })
    }

    /// Every live object with its index, in index order.
    pub fn objects(&self) -> impl Iterator<Item = (u32, Obj)> + '_ {
        self.store.iter::<ObjRow>().map(|(id, r)| (obj_index(id), Obj { id, global: r.global, kind: r.kind as u8 }))
    }

    /// Object indices used so far: live objects are below it; indices are never reused.
    pub fn obj_slots(&self) -> u32 {
        self.store.last_counter(0)
    }

    /// The object's grab claim, expired or not (it lapses at the end of frame `until - 1`).
    pub fn claim(&self, obj: u32) -> Option<Claim> {
        self.store.get::<ClaimRow>(object_id(obj as usize)).map(|c| Claim { member: c.member, until: c.until })
    }

    /// Every claim, in object order.
    pub fn claims(&self) -> impl Iterator<Item = (u32, Claim)> + '_ {
        self.store.iter::<ClaimRow>().map(|(id, c)| (obj_index(id), Claim { member: c.member, until: c.until }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review CMP-1 through the ring: a keyframe image kept outside the ring across a compaction is
    /// refused by `restore_image`, with nothing written, rather than restored into a wrong state.
    #[test]
    fn a_keyframe_kept_past_a_compaction_is_refused() {
        let cfg = SimConfig { compact: CompactPolicy::EveryTrim, ..SimConfig::default() };
        let mut s = WorldSim::new(1, 3, &[1, 2, 3], SceneKind::Pile { side: 4, layers: 1 }, cfg, KeyRing::test(3));
        s.advance(8);
        let (&f, k) = s.ring.iter().next().expect("a keyframe");
        let stale = k.image.clone();
        assert!(s.world.restore_image(&stale).is_ok(), "before compaction it restores");
        s.restore(&k.clone(), f);
        s.advance(16);
        assert!(s.stats.compactions > 0);
        let before = (s.state_hash(), s.check_hash());
        assert!(matches!(s.world.restore_image(&stale), Err(pe_physics::PhysicsError::BadImage(_))));
        assert_eq!((s.state_hash(), s.check_hash()), before, "nothing written, not poisoned");
        // The ring's own copy of that keyframe was rewritten and restores.
        let k = s.ring.get(&f).expect("the base stays").clone();
        s.restore(&k, f);
        s.advance(16);
        assert_eq!(s.state_hash(), before.0);
    }
}
