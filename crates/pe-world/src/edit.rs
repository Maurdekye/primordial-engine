//! The edit payload (world-model.md 7.1) and its canonical codec.
//!
//! Every integer is little-endian; an `ObjectId` or `ComponentId` is its `u128`; options and bools are one byte, 0 or
//! 1; text is a `u16` length and UTF-8; byte strings are a `u32` length and the bytes; lists are a `u32` count and the
//! items. Decoding is total and strict: any other byte string (a bad tag, a flag of 2, a zero id, a length past the
//! end, trailing bytes, a limit broken) is refused, never panics, and each value has exactly one encoding, so
//! `encode(decode(b)) == b` for every accepted `b` (5.3).
//!
//! Not in this slice: `Register` and `CommitScript` (7.1, WS-logic's), and `Local` or `Session` classes, which no
//! edit creates in M1 (3.1).

use pe_core::{EntryRef, Hash, MemberKey, ObjectId};
use pe_seams::Version;
use pe_sim::ComponentId;

use crate::components::BlobRef;

/// The most ops a transaction may have (7.1).
pub const MAX_OPS: usize = 4096;
/// The most bytes of row data a transaction may carry, apart from blobs (7.1).
pub const MAX_ROW_BYTES: usize = 1 << 20;
/// The longest `label`, in bytes (7.1).
pub const MAX_LABEL: usize = 200;
/// The longest agent `harness` name, in bytes (7.1).
pub const MAX_HARNESS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    pub actor: Actor,
    pub label: Option<String>,
    pub preconditions: Vec<Precondition>,
    pub ops: Vec<Op>,
    pub blobs: Vec<BlobRef>,
}

/// Who produced the transaction (7.1). An honest client's statement: permissions come from the author alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Actor {
    Person,
    Agent { harness: String, principal: MemberKey, requested_by: Option<MemberKey> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// The row's edit version the author read.
    Version {
        id: ObjectId,
        component: ComponentId,
        instance: u16,
        is: Version,
    },
    /// The object version the author read: nothing about the object was edited since.
    ObjectVersion {
        id: ObjectId,
        is: Version,
    },
    Exists(ObjectId),
    Absent(ObjectId),
    /// `Hash::of("pe.content", row bytes)` of the row as the author read it; stale if any writer changed it.
    Content {
        id: ObjectId,
        component: ComponentId,
        instance: u16,
        hash: Hash,
    },
}

/// An object an op names: an existing one, the `k`-th this transaction creates, or the world's root (for a parent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Existing(ObjectId),
    New(u32),
    Root,
}

/// A component row given whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub component: ComponentId,
    pub instance: u16,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Creates object `new` (its id derives from the entry and `new`, 2.1) under `parent`, at `index` among its
    /// children (`None`: last), with `rows`.
    Create {
        new: u32,
        parent: Target,
        index: Option<u32>,
        rows: Vec<Row>,
    },
    /// Deletes the object and its subtree (7.5).
    Delete {
        id: Target,
    },
    /// Adds or overwrites a row.
    SetRow {
        id: Target,
        row: Row,
    },
    /// Sets one field, by its index in the component's description (fields sorted by offset), to `value`.
    SetField {
        id: Target,
        component: ComponentId,
        instance: u16,
        field: u16,
        value: Vec<u8>,
    },
    RemoveRow {
        id: Target,
        component: ComponentId,
        instance: u16,
    },
    /// Moves `id` under `parent` at `index` (`None`: last). `keep_world` keeps its world pose; otherwise its local
    /// transform is kept (3.3).
    Reparent {
        id: Target,
        parent: Target,
        index: Option<u32>,
        keep_world: bool,
    },
    /// Copies `source` and its subtree under `parent`. The copies take `New` indices `new .. new + n` in depth-first
    /// order, children in their order (7.1).
    Copy {
        source: Target,
        new: u32,
        parent: Target,
        index: Option<u32>,
    },
    /// Restores a deleted subtree from its tombstones, with the same ids (7.5).
    Undelete {
        id: ObjectId,
    },
}

/// The context of [`Precondition::Content`] hashes.
pub const CONTENT_CONTEXT: &str = "pe.content";

/// The content hash of a row's bytes, as [`Precondition::Content`] names it.
pub fn content_hash(row: &[u8]) -> Hash {
    Hash::of(CONTENT_CONTEXT, row)
}

// ---- encoding ------------------------------------------------------------------------------------------------------

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u128(&mut self, v: u128) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }
    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.raw(b);
    }
    fn text(&mut self, s: &str) {
        self.u16(u16::try_from(s.len()).unwrap_or(u16::MAX));
        self.raw(s.as_bytes());
    }
    fn id(&mut self, id: ObjectId) {
        self.u128(id.to_u128());
    }
    fn opt_u32(&mut self, v: Option<u32>) {
        match v {
            None => self.u8(0),
            Some(x) => {
                self.u8(1);
                self.u32(x);
            }
        }
    }
    fn version(&mut self, v: &Version) {
        self.raw(&v.entry.0.0);
        self.u32(v.op);
    }
    fn target(&mut self, t: Target) {
        match t {
            Target::Existing(id) => {
                self.u8(0);
                self.id(id);
            }
            Target::New(k) => {
                self.u8(1);
                self.u32(k);
            }
            Target::Root => self.u8(2),
        }
    }
}

impl Transaction {
    /// The canonical bytes. A transaction that breaks a limit encodes, but its bytes do not decode.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match &self.actor {
            Actor::Person => w.u8(0),
            Actor::Agent { harness, principal, requested_by } => {
                w.u8(1);
                w.text(harness);
                w.raw(&principal.0);
                match requested_by {
                    None => w.u8(0),
                    Some(k) => {
                        w.u8(1);
                        w.raw(&k.0);
                    }
                }
            }
        }
        match &self.label {
            None => w.u8(0),
            Some(l) => {
                w.u8(1);
                w.text(l);
            }
        }
        w.len(self.preconditions.len());
        for p in &self.preconditions {
            match p {
                Precondition::Version { id, component, instance, is } => {
                    w.u8(0);
                    w.id(*id);
                    w.u128(component.0);
                    w.u16(*instance);
                    w.version(is);
                }
                Precondition::ObjectVersion { id, is } => {
                    w.u8(1);
                    w.id(*id);
                    w.version(is);
                }
                Precondition::Exists(id) => {
                    w.u8(2);
                    w.id(*id);
                }
                Precondition::Absent(id) => {
                    w.u8(3);
                    w.id(*id);
                }
                Precondition::Content { id, component, instance, hash } => {
                    w.u8(4);
                    w.id(*id);
                    w.u128(component.0);
                    w.u16(*instance);
                    w.raw(&hash.0);
                }
            }
        }
        w.len(self.ops.len());
        for op in &self.ops {
            match op {
                Op::Create { new, parent, index, rows } => {
                    w.u8(0);
                    w.u32(*new);
                    w.target(*parent);
                    w.opt_u32(*index);
                    w.len(rows.len());
                    for r in rows {
                        w.u128(r.component.0);
                        w.u16(r.instance);
                        w.bytes(&r.bytes);
                    }
                }
                Op::Delete { id } => {
                    w.u8(1);
                    w.target(*id);
                }
                Op::SetRow { id, row } => {
                    w.u8(2);
                    w.target(*id);
                    w.u128(row.component.0);
                    w.u16(row.instance);
                    w.bytes(&row.bytes);
                }
                Op::SetField { id, component, instance, field, value } => {
                    w.u8(3);
                    w.target(*id);
                    w.u128(component.0);
                    w.u16(*instance);
                    w.u16(*field);
                    w.bytes(value);
                }
                Op::RemoveRow { id, component, instance } => {
                    w.u8(4);
                    w.target(*id);
                    w.u128(component.0);
                    w.u16(*instance);
                }
                Op::Reparent { id, parent, index, keep_world } => {
                    w.u8(5);
                    w.target(*id);
                    w.target(*parent);
                    w.opt_u32(*index);
                    w.u8(*keep_world as u8);
                }
                Op::Copy { source, new, parent, index } => {
                    w.u8(6);
                    w.target(*source);
                    w.u32(*new);
                    w.target(*parent);
                    w.opt_u32(*index);
                }
                Op::Undelete { id } => {
                    w.u8(7);
                    w.id(*id);
                }
            }
        }
        w.len(self.blobs.len());
        for b in &self.blobs {
            w.raw(&b.hash);
            w.raw(&b.len.to_le_bytes());
        }
        w.0
    }

    /// Decodes canonical bytes. `None` for anything else (7.6 step 1: `Malformed`).
    pub fn decode(bytes: &[u8]) -> Option<Transaction> {
        let mut r = R { b: bytes, row_bytes: 0 };
        let tx = r.transaction()?;
        r.b.is_empty().then_some(tx)
    }
}

// ---- decoding ------------------------------------------------------------------------------------------------------

struct R<'a> {
    b: &'a [u8],
    row_bytes: usize,
}

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.b.len() {
            return None;
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Some(h)
    }
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn flag(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.arr()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.arr()?))
    }
    fn u128(&mut self) -> Option<u128> {
        Some(u128::from_le_bytes(self.arr()?))
    }
    fn id(&mut self) -> Option<ObjectId> {
        ObjectId::from_u128(self.u128()?).some()
    }
    fn component(&mut self) -> Option<ComponentId> {
        Some(ComponentId(self.u128()?))
    }
    fn key(&mut self) -> Option<MemberKey> {
        Some(MemberKey(self.arr()?))
    }
    /// A count of items each at least `min` bytes long, so a count can never ask for more than the bytes left.
    fn count(&mut self, min: usize, max: usize) -> Option<usize> {
        let n = self.u32()? as usize;
        (n <= max && n.checked_mul(min)? <= self.b.len()).then_some(n)
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        self.row_bytes = self.row_bytes.checked_add(n)?;
        if self.row_bytes > MAX_ROW_BYTES {
            return None;
        }
        Some(self.take(n)?.to_vec())
    }
    fn text(&mut self, max: usize) -> Option<String> {
        let n = self.u16()? as usize;
        if n > max {
            return None;
        }
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    fn opt_u32(&mut self) -> Option<Option<u32>> {
        Some(if self.flag()? { Some(self.u32()?) } else { None })
    }
    fn version(&mut self) -> Option<Version> {
        Some(Version { entry: EntryRef(Hash(self.arr()?)), op: self.u32()? })
    }
    fn target(&mut self) -> Option<Target> {
        Some(match self.u8()? {
            0 => Target::Existing(self.id()?),
            1 => Target::New(self.u32()?),
            2 => Target::Root,
            _ => return None,
        })
    }

    fn transaction(&mut self) -> Option<Transaction> {
        let actor = match self.u8()? {
            0 => Actor::Person,
            1 => {
                let harness = self.text(MAX_HARNESS)?;
                let principal = self.key()?;
                let requested_by = if self.flag()? { Some(self.key()?) } else { None };
                Actor::Agent { harness, principal, requested_by }
            }
            _ => return None,
        };
        let label = if self.flag()? { Some(self.text(MAX_LABEL)?) } else { None };
        let n = self.count(17, MAX_OPS)?;
        let mut preconditions = Vec::with_capacity(n);
        for _ in 0..n {
            preconditions.push(match self.u8()? {
                0 => Precondition::Version {
                    id: self.id()?,
                    component: self.component()?,
                    instance: self.u16()?,
                    is: self.version()?,
                },
                1 => Precondition::ObjectVersion { id: self.id()?, is: self.version()? },
                2 => Precondition::Exists(self.id()?),
                3 => Precondition::Absent(self.id()?),
                4 => Precondition::Content {
                    id: self.id()?,
                    component: self.component()?,
                    instance: self.u16()?,
                    hash: Hash(self.arr()?),
                },
                _ => return None,
            });
        }
        let n = self.count(2, MAX_OPS)?;
        let mut ops = Vec::with_capacity(n);
        for _ in 0..n {
            ops.push(match self.u8()? {
                0 => {
                    let new = self.u32()?;
                    let parent = self.target()?;
                    let index = self.opt_u32()?;
                    let m = self.count(22, MAX_OPS)?;
                    let mut rows = Vec::with_capacity(m);
                    for _ in 0..m {
                        rows.push(Row { component: self.component()?, instance: self.u16()?, bytes: self.bytes()? });
                    }
                    Op::Create { new, parent, index, rows }
                }
                1 => Op::Delete { id: self.target()? },
                2 => Op::SetRow {
                    id: self.target()?,
                    row: Row { component: self.component()?, instance: self.u16()?, bytes: self.bytes()? },
                },
                3 => Op::SetField {
                    id: self.target()?,
                    component: self.component()?,
                    instance: self.u16()?,
                    field: self.u16()?,
                    value: self.bytes()?,
                },
                4 => Op::RemoveRow { id: self.target()?, component: self.component()?, instance: self.u16()? },
                5 => Op::Reparent {
                    id: self.target()?,
                    parent: self.target()?,
                    index: self.opt_u32()?,
                    keep_world: self.flag()?,
                },
                6 => Op::Copy {
                    source: self.target()?,
                    new: self.u32()?,
                    parent: self.target()?,
                    index: self.opt_u32()?,
                },
                7 => Op::Undelete { id: self.id()? },
                _ => return None,
            });
        }
        let n = self.count(40, MAX_OPS)?;
        let mut blobs = Vec::with_capacity(n);
        for _ in 0..n {
            blobs.push(BlobRef { hash: self.arr()?, len: u64::from_le_bytes(self.arr()?) });
        }
        Some(Transaction { actor, label, preconditions, ops, blobs })
    }
}
