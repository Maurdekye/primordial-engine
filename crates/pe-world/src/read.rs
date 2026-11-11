//! The published, read-only view of one frame (world-model.md 9.4; pe-api's snapshot port, api.md 9.1).
//!
//! A [`WorldSnapshot`] shares the world's copy-on-write buffers, so taking one costs a reference per column and
//! table, and it gives the same answers for as long as it is held, with one exception: blob bytes come from the
//! machine's shared [`BlobStore`], so `name` and `blob` can go from `None` to `Some` when a blob arrives (never back,
//! and never to different bytes). Positions never come out as plain numbers: a root's place is a [`Pos`], and
//! anything relative is an [`Offset`] from an explicit frame (4.2).

use std::collections::BTreeMap;
use std::sync::Arc;

use pe_core::{EntryRef, Frame, Hash, MemberKey, ObjectId, Offset, Pos};
use pe_sim::{ComponentDesc, ComponentId, FieldKind, SimStore, Transform, WorldTransform};

use crate::components::{BlobRef, Name, Protection};
use crate::world::{BlobStore, Claim, DEPTH_LIMIT_MAX};
use pe_seams::Version;

/// One object as WorldApi's `query` reports it, without its component values.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectInfo {
    pub id: ObjectId,
    /// The `Name` text, if the object has one and this machine has its blob; invalid UTF-8 is replaced.
    pub name: Option<String>,
    pub parent: Option<ObjectId>,
    /// The object version (7.3): the last edit that changed anything about it.
    pub version: Version,
    /// The structural version (7.3).
    pub structural: Version,
    pub creator: MemberKey,
    /// The member whose hand holds it (7.2).
    pub held_by: Option<MemberKey>,
    /// The owner of the nearest `Protection` at or above it (7.8).
    pub protected_by: Option<MemberKey>,
    /// Its component types, in component-id order, without the internal `Node`. Instances after the first will be
    /// `Type#n` (3.1); M1's engine components have only instance 0.
    pub types: Vec<String>,
}

/// One field value, decoded through the registry.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    U(u64),
    I(i64),
    F(f64),
    Bool(bool),
    Object(Option<ObjectId>),
    Blob(Option<BlobRef>),
    SimBlob(Option<BlobRef>),
    Pos(Pos),
    /// A relative position: a child's `Transform.pos`, in its frame's space.
    Offset(Offset),
    Member(MemberKey),
    Entry(EntryRef),
    Hash(Hash),
    /// A field of more than one element.
    List(Vec<Value>),
}

/// A published frame of one world.
#[derive(Clone, Debug)]
pub struct WorldSnapshot {
    pub(crate) store: SimStore,
    pub(crate) claims: Arc<BTreeMap<ObjectId, Claim>>,
    pub(crate) blobs: BlobStore,
    pub(crate) final_frame: Frame,
}

impl WorldSnapshot {
    pub fn frame(&self) -> Frame {
        self.store.frame()
    }

    /// The newest final frame known when this was published.
    pub fn final_frame(&self) -> Frame {
        self.final_frame
    }

    pub fn exists(&self, id: ObjectId) -> bool {
        self.store.exists(id)
    }

    /// Root objects, in id order.
    pub fn roots(&self) -> Vec<ObjectId> {
        self.store.roots().collect()
    }

    /// Every object, in id order (the order paging follows).
    pub fn all(&self) -> &[ObjectId] {
        self.store.objects()
    }

    /// Children in their order. Empty for an unknown object.
    pub fn children(&self, id: ObjectId) -> Vec<ObjectId> {
        if !self.store.exists(id) {
            return Vec::new();
        }
        self.store.children(id).collect()
    }

    pub fn parent(&self, id: ObjectId) -> Option<ObjectId> {
        self.store.parent(id)
    }

    /// The component descriptions: the schema WorldApi publishes (3.2).
    pub fn schema(&self) -> impl Iterator<Item = &ComponentDesc> {
        self.store.registry().iter()
    }

    pub fn component(&self, name: &str) -> Option<&ComponentDesc> {
        self.store.registry().iter().find(|d| d.name() == name)
    }

    /// A blob's bytes, if this machine has them.
    pub fn blob(&self, r: &BlobRef) -> Option<Arc<[u8]>> {
        self.blobs.get(r)
    }

    pub fn name(&self, id: ObjectId) -> Option<String> {
        let n = self.store.get::<Name>(id)?;
        let bytes = self.blobs.get(&n.text)?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn info(&self, id: ObjectId) -> Option<ObjectInfo> {
        if !self.store.exists(id) {
            return None;
        }
        let n = self.store.node(id);
        let types = self
            .store
            .registry()
            .iter()
            .filter(|d| d.id() != ComponentId::NODE && self.store.get_raw(d.id(), id).is_some())
            .map(|d| d.name().to_string())
            .collect();
        Some(ObjectInfo {
            id,
            name: self.name(id),
            parent: self.store.parent(id),
            version: Version { entry: EntryRef(Hash(n.version_entry)), op: n.version_op },
            structural: Version { entry: EntryRef(Hash(n.structural_entry)), op: n.structural_op },
            creator: n.creator,
            held_by: self.claims.get(&id).map(|c| c.member),
            protected_by: self.protected_by(id),
            types,
        })
    }

    /// The claim on an object, if one of its holders' hands holds it.
    pub fn claim(&self, id: ObjectId) -> Option<Claim> {
        self.claims.get(&id).copied()
    }

    /// The owner of the nearest `Protection` at or above `id`. Iterative and bounded (3.3).
    pub fn protected_by(&self, id: ObjectId) -> Option<MemberKey> {
        let mut at = id;
        for _ in 0..=DEPTH_LIMIT_MAX {
            if let Some(p) = self.store.get::<Protection>(at) {
                return Some(p.owner);
            }
            at = self.store.parent(at)?;
        }
        None
    }

    /// One component's fields, by name, decoded through the registry, without padding fields. `None` if the object
    /// has no such row, or for `Node` (the hierarchy is read through [`WorldSnapshot::info`] and
    /// [`WorldSnapshot::children`]).
    ///
    /// `Transform` is special: its `chunk` and `pos` come out as one `pos` field, a [`Value::Pos`] for an object
    /// without a frame and a [`Value::Offset`] in its frame's space otherwise, so no position is ever three numbers.
    pub fn fields(&self, id: ObjectId, component: &str) -> Option<Vec<(String, Value)>> {
        let desc = self.component(component)?;
        if desc.id() == ComponentId::NODE {
            return None;
        }
        if desc.id() == ComponentId::TRANSFORM {
            let t = self.store.get::<Transform>(id)?;
            let pos = if self.store.frame_of(id).is_some() {
                Value::Offset(Offset(t.pos))
            } else {
                Value::Pos(t.root_pos()?)
            };
            return Some(vec![
                ("pos".into(), pos),
                ("rot".into(), Value::List(t.rot.iter().map(|&v| Value::F(v as f64)).collect())),
                ("scale".into(), Value::List(t.scale.iter().map(|&v| Value::F(v as f64)).collect())),
            ]);
        }
        let row = self.store.get_raw(desc.id(), id)?;
        Some(decode_row(desc, row))
    }

    /// A row's instance tag (world-model.md 7.6), which a `RemoveRow` names; `None` if the object has no such row.
    pub fn row_tag(&self, id: ObjectId, component: &str) -> Option<u128> {
        let desc = self.component(component)?;
        self.store.row_version(desc.id(), id).map(|v| v.tag())
    }

    /// The object's world transform, or `None` if it has no `Transform` or its composed position leaves the range.
    pub fn world_transform(&self, id: ObjectId) -> Option<WorldTransform> {
        self.store.world_transform(id)
    }

    /// Where `id` is in `frame`'s space (rotation and scale undone): what "0.5 m above the table" means. With
    /// `frame == None`, the offset from the world origin, `pos - Pos::ORIGIN` (4.2).
    pub fn offset_in(&self, id: ObjectId, frame: Option<ObjectId>) -> Option<Offset> {
        let w = self.world_transform(id)?;
        match frame {
            None => Some(w.pos - Pos::ORIGIN),
            Some(f) => {
                let fw = self.world_transform(f)?;
                Some(Offset(w.local_in(Some(&fw)).pos))
            }
        }
    }

    /// The distance in metres between two objects' world positions.
    pub fn distance(&self, a: ObjectId, b: ObjectId) -> Option<f64> {
        let pa = self.world_transform(a)?.pos;
        let pb = self.world_transform(b)?.pos;
        Some((pa - pb).length())
    }
}

/// Decodes every non-padding field of a row. Total: a field that runs past the row is skipped (the registry's
/// layout check makes that impossible), and an invalid stored `Pos` is skipped rather than shown as numbers.
fn decode_row(desc: &ComponentDesc, row: &[u8]) -> Vec<(String, Value)> {
    let mut out = Vec::with_capacity(desc.fields().len());
    for f in desc.fields() {
        if f.name.starts_with('_') {
            continue;
        }
        let w = f.kind.size() as usize;
        let mut items = Vec::with_capacity(f.count as usize);
        for k in 0..f.count as usize {
            let start = f.offset as usize + k * w;
            if let Some(v) = row.get(start..start + w).and_then(|b| decode(f.kind, b)) {
                items.push(v);
            }
        }
        if items.len() != f.count as usize {
            continue;
        }
        let v = if f.count == 1 { items.pop().expect("one item") } else { Value::List(items) };
        out.push((f.name.clone(), v));
    }
    out
}

fn arr<const N: usize>(b: &[u8]) -> Option<[u8; N]> {
    b.try_into().ok()
}

fn decode(kind: FieldKind, b: &[u8]) -> Option<Value> {
    Some(match kind {
        FieldKind::U8 => Value::U(b[0] as u64),
        FieldKind::U16 => Value::U(u16::from_le_bytes(arr(b)?) as u64),
        FieldKind::U32 => Value::U(u32::from_le_bytes(arr(b)?) as u64),
        FieldKind::U64 => Value::U(u64::from_le_bytes(arr(b)?)),
        FieldKind::I8 => Value::I(b[0] as i8 as i64),
        FieldKind::I16 => Value::I(i16::from_le_bytes(arr(b)?) as i64),
        FieldKind::I32 => Value::I(i32::from_le_bytes(arr(b)?) as i64),
        FieldKind::I64 => Value::I(i64::from_le_bytes(arr(b)?)),
        FieldKind::F32 => Value::F(f32::from_le_bytes(arr(b)?) as f64),
        FieldKind::F64 => Value::F(f64::from_le_bytes(arr(b)?)),
        FieldKind::Bool => Value::Bool(b[0] != 0),
        FieldKind::ObjectRef => {
            let id: ObjectId = bytemuck::pod_read_unaligned(b);
            Value::Object(if id.is_none() { None } else { Some(id) })
        }
        FieldKind::BlobRef | FieldKind::SimBlobRef => {
            let r: BlobRef = bytemuck::pod_read_unaligned(b);
            let r = if r.is_none() { None } else { Some(r) };
            if kind == FieldKind::BlobRef { Value::Blob(r) } else { Value::SimBlob(r) }
        }
        FieldKind::Pos => Value::Pos(Pos::from_row(bytemuck::pod_read_unaligned(b)).ok()?),
        FieldKind::MemberKey => Value::Member(MemberKey(arr(b)?)),
        FieldKind::EntryRef => Value::Entry(EntryRef(Hash(arr(b)?))),
        FieldKind::Hash => Value::Hash(Hash(arr(b)?)),
    })
}
