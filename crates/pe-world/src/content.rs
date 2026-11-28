//! Domain content (design domain-eng d1, 1.2; @coordinator-opus, C2 "save content"): a set of
//! objects with their rows and the blob bytes they reference, outside any world. One format carries the home domain's
//! save file, the shipped starter set, and objects taken out of or put into My Domain.
//!
//! - **Content, not state.** It holds what an object *is* (its rows), never what happened to it: no ids, no row
//!   versions, no velocities (`BodyState`), holds, history or members. Putting it into a world is ordinary `Create`
//!   ops ([`Content::transactions`]), so the objects get ids derived on the destination's line (2.1, "Carrying
//!   between worlds") and the edit path checks every row.
//! - **Placement.** A top-level object's `Transform` is stored relative to the content's origin (its `chunk` is zero
//!   and its `pos` is the offset from the origin, rotation and scale kept), so content can be put down anywhere.
//!   Rows of deeper objects are kept as they are (they are relative to their parent).
//! - **Engine components only, in v1.** Rows of script components (ids from [`ComponentId::FIRST_USER`]) need their
//!   module and registration carried too; [`Content::from_world`] leaves them out and counts them
//!   ([`Content::skipped_rows`]). `Pos` fields inside rows and object references are copied unchanged, as `Copy` does.
//! - **Swappable.** The byte format ([`Content::encode`]) is versioned and small. @fable-reviewer's git layout (M3+)
//!   replaces the home's single file; the rows stay the same.
//!
//! Decoding is checked everywhere: malformed bytes give a [`ContentError`], never a panic.

use std::collections::BTreeMap;

use pe_core::{ObjectId, Offset, Pos};
use pe_sim::{ComponentId, Registry, Transform};

use crate::components::{BlobRef, Hand, Name, Player, ids};
use crate::edit::{Actor, MAX_EDIT_BYTES, MAX_ROW_BYTES, Op, Row, Target, Transaction};
use crate::inline;
use crate::world::{BlobStore, World};

/// The first bytes of encoded content.
pub const MAGIC: &[u8; 16] = b"pe.content\0\0\0\0\0\0";
/// The format version [`Content::encode`] writes. A decoder keeps every version it ever accepted (2.3).
pub const VERSION: u16 = 1;
/// The prefix of content's text form ([`Content::to_text`]).
pub const TEXT_PREFIX: &str = "pe1c:";
const HEX: &[u8; 16] = b"0123456789abcdef";
/// The most objects one content may hold.
pub const MAX_OBJECTS: usize = 1 << 20;
/// The most rows one object may carry.
pub const MAX_ROWS: usize = 256;
/// The largest blob a content may carry.
pub const MAX_BLOB: usize = 64 << 20;

/// One object of a content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The index of its parent among the content's items, always an earlier one; `None` for a top-level object.
    pub parent: Option<u32>,
    /// Its rows, in ascending component order, without `Node` or `BodyState`.
    pub rows: Vec<(ComponentId, Vec<u8>)>,
}

/// Objects outside any world (module doc).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Content {
    /// Parents before children; each top-level object is followed by its whole subtree, depth-first.
    pub items: Vec<Item>,
    /// The bytes of every blob the rows reference, by hash.
    pub blobs: BTreeMap<[u8; 32], Vec<u8>>,
    /// Rows [`Content::from_world`] left out (script components), for the caller to report. Not encoded.
    pub skipped_rows: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentError {
    /// The bytes are not content of a known version, or are malformed (with what was being read).
    Malformed(&'static str),
    /// The object is not in the world.
    Unknown(ObjectId),
    /// The object, or one under it, is a member's hand or body, which belongs to the member, not the domain.
    MemberObject(ObjectId),
    /// The object is not a root, so it has no world placement of its own.
    NotRoot(ObjectId),
    /// A row references a blob whose bytes this machine lacks.
    MissingBlob(BlobRef),
    /// A top-level object's placement would leave the position range.
    OutOfRange,
    /// One top-level object's subtree is too large for one transaction.
    TooLarge { item: u32 },
}

impl Content {
    /// The top-level objects, as item indices.
    pub fn tops(&self) -> impl Iterator<Item = u32> + '_ {
        self.items.iter().enumerate().filter(|(_, it)| it.parent.is_none()).map(|(i, _)| i as u32)
    }

    /// The content of `roots` in `world` (each with its subtree), placed relative to `origin`. Every root must be a
    /// root of the world, and none may hold a member's hand or body.
    pub fn from_world(world: &World, roots: &[ObjectId], origin: Pos) -> Result<Content, ContentError> {
        let store = world.store();
        let registry = store.registry().clone();
        let mut c = Content::default();
        for &root in roots {
            if !store.exists(root) {
                return Err(ContentError::Unknown(root));
            }
            if store.parent(root).is_some() {
                return Err(ContentError::NotRoot(root));
            }
            let subtree = store.subtree(root);
            let mut index: BTreeMap<ObjectId, u32> = BTreeMap::new();
            for (j, &x) in subtree.iter().enumerate() {
                if store.has::<Hand>(x) || store.has::<Player>(x) {
                    return Err(ContentError::MemberObject(x));
                }
                let parent = if j == 0 { None } else { store.parent(x).and_then(|p| index.get(&p).copied()) };
                let mut rows = Vec::new();
                for desc in registry.iter().filter(|d| d.id() != ComponentId::NODE && d.id() != ids::BODY_STATE) {
                    let id = desc.id();
                    let Some(row) = store.get_raw(id, x) else { continue };
                    if id.0 >= ComponentId::FIRST_USER {
                        c.skipped_rows += 1;
                        continue;
                    }
                    let row =
                        if j == 0 && id == ComponentId::TRANSFORM { relative(row, origin)? } else { row.to_vec() };
                    for (r, _) in inline::row_blobs(desc, &row).unwrap_or_default() {
                        if r.is_none() || c.blobs.contains_key(&r.hash) {
                            continue;
                        }
                        let bytes = world.blobs().get(&r).ok_or(ContentError::MissingBlob(r))?;
                        c.blobs.insert(r.hash, bytes.to_vec());
                    }
                    rows.push((id, row));
                }
                rows.sort_by_key(|r| r.0);
                index.insert(x, c.items.len() as u32);
                c.items.push(Item { parent, rows });
            }
        }
        Ok(c)
    }

    /// The `Create` ops that put this content into a world, under `parent` (top-level objects at `origin` plus their
    /// stored offsets), split into transactions that each hold whole top-level subtrees and fit one edit
    /// ([`MAX_EDIT_BYTES`]). The blobs go into `blobs`, and each transaction lists the blobs its rows reference and
    /// carries the bytes of those that travel inside an entry ([`inline::is_carried`]), so the transactions can be
    /// signed and sent as they are. `registry` gives the rows' field layouts; rows of unknown components are refused
    /// later by the edit path, as any edit's would be.
    pub fn transactions(
        &self,
        registry: &Registry,
        blobs: &BlobStore,
        parent: Target,
        origin: Pos,
        actor: Actor,
        label: &str,
    ) -> Result<Vec<Transaction>, ContentError> {
        if matches!(parent, Target::New(_)) {
            return Err(ContentError::Malformed("parent is a new object"));
        }
        for bytes in self.blobs.values() {
            blobs.put(bytes);
        }
        let mut out = Vec::new();
        let mut tx = empty(&actor, label);
        let mut size = 0usize;
        let tops: Vec<u32> = self.tops().collect();
        for (t, &top) in tops.iter().enumerate() {
            let end = tops.get(t + 1).copied().unwrap_or(self.items.len() as u32);
            // The group: this top-level object and its subtree, its new indices from 0.
            let mut group = empty(&actor, label);
            let mut map: BTreeMap<u32, u32> = BTreeMap::new();
            for i in top..end {
                let item = &self.items[i as usize];
                let new = group.ops.len() as u32;
                let p = match item.parent {
                    None => parent,
                    Some(q) => Target::New(*map.get(&q).ok_or(ContentError::Malformed("parent outside subtree"))?),
                };
                let mut rows = Vec::with_capacity(item.rows.len());
                for (c, bytes) in &item.rows {
                    let bytes =
                        if i == top && *c == ComponentId::TRANSFORM { placed(bytes, origin)? } else { bytes.clone() };
                    if let Some(desc) = registry.get(*c) {
                        for (r, sim) in inline::row_blobs(desc, &bytes).unwrap_or_default() {
                            if r.is_none() || group.blobs.contains(&r) {
                                continue;
                            }
                            group.blobs.push(r);
                            if inline::is_carried(&r, sim)
                                && let Some(b) = blobs.get(&r)
                            {
                                group.inline.push(b.to_vec());
                            }
                        }
                    }
                    rows.push(Row { component: *c, instance: 0, bytes });
                }
                group.ops.push(Op::Create { new, parent: p, index: None, rows });
                map.insert(i, new);
            }
            let n = group.encode().len();
            if n > MAX_EDIT_BYTES {
                return Err(ContentError::TooLarge { item: top });
            }
            // A conservative running size: each group's own encoding, headers included.
            if !tx.ops.is_empty() && size + n > MAX_EDIT_BYTES {
                out.push(std::mem::replace(&mut tx, empty(&actor, label)));
                size = 0;
            }
            let by = tx.ops.len() as u32;
            append(&mut tx, shift(group, by));
            size += n;
        }
        if !tx.ops.is_empty() {
            out.push(tx);
        }
        for t in &mut out {
            t.blobs.sort();
            t.blobs.dedup();
            t.inline.sort_by_key(|b| BlobRef::of(b).hash);
            t.inline.dedup();
        }
        Ok(out)
    }

    /// Top-level object `top` (an index from [`Content::tops`]) with its subtree, as content of its own: the items
    /// renumbered from 0, and only the blobs its rows reference (`registry` gives the rows' layouts; a row of a
    /// component it lacks keeps none). `None` if `top` is not a top-level item.
    pub fn subtree(&self, top: u32, registry: &Registry) -> Option<Content> {
        if self.items.get(top as usize)?.parent.is_some() {
            return None;
        }
        let end = self.tops().find(|&t| t > top).unwrap_or(self.items.len() as u32);
        let mut out = Content::default();
        for i in top..end {
            let mut it = self.items[i as usize].clone();
            it.parent = it.parent.map(|p| p - top);
            for (c, bytes) in &it.rows {
                let refs = registry.get(*c).and_then(|d| inline::row_blobs(d, bytes)).unwrap_or_default();
                for (r, _) in refs {
                    if let Some(b) = self.blobs.get(&r.hash) {
                        out.blobs.insert(r.hash, b.clone());
                    }
                }
            }
            out.items.push(it);
        }
        Some(out)
    }

    /// The text form, for a JSON field (pe-api's `insert`): [`TEXT_PREFIX`] and the bytes in lower-case hex.
    pub fn to_text(&self) -> String {
        let bytes = self.encode();
        let mut s = String::with_capacity(TEXT_PREFIX.len() + 2 * bytes.len());
        s.push_str(TEXT_PREFIX);
        for b in bytes {
            s.push(char::from(HEX[usize::from(b >> 4)]));
            s.push(char::from(HEX[usize::from(b & 15)]));
        }
        s
    }

    /// Reads [`Content::to_text`]'s form.
    pub fn from_text(text: &str) -> Result<Content, ContentError> {
        let hex = text.strip_prefix(TEXT_PREFIX).ok_or(ContentError::Malformed("text prefix"))?.as_bytes();
        if hex.len() % 2 != 0 {
            return Err(ContentError::Malformed("hex"));
        }
        let nibble = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        };
        let bytes: Option<Vec<u8>> =
            hex.as_chunks::<2>().0.iter().map(|p| Some((nibble(p[0])? << 4) | nibble(p[1])?)).collect();
        Content::decode(&bytes.ok_or(ContentError::Malformed("hex"))?)
    }

    /// The canonical bytes: [`MAGIC`], [`VERSION`], the items, then the blobs in ascending order of hash.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Vec::new();
        w.extend_from_slice(MAGIC);
        w.extend_from_slice(&VERSION.to_le_bytes());
        w.extend_from_slice(&(self.items.len() as u64).to_le_bytes());
        for it in &self.items {
            w.extend_from_slice(&it.parent.unwrap_or(u32::MAX).to_le_bytes());
            w.extend_from_slice(&(it.rows.len() as u32).to_le_bytes());
            for (c, bytes) in &it.rows {
                w.extend_from_slice(&c.0.to_le_bytes());
                w.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                w.extend_from_slice(bytes);
            }
        }
        w.extend_from_slice(&(self.blobs.len() as u64).to_le_bytes());
        for bytes in self.blobs.values() {
            w.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            w.extend_from_slice(bytes);
        }
        w
    }

    /// Reads [`Content::encode`]'s bytes, checking every count, parent and order.
    pub fn decode(bytes: &[u8]) -> Result<Content, ContentError> {
        use ContentError::Malformed as M;
        let mut r = Rd(bytes);
        if r.take(MAGIC.len()).ok_or(M("magic"))? != MAGIC {
            return Err(M("magic"));
        }
        if r.u16().ok_or(M("version"))? != VERSION {
            return Err(M("version"));
        }
        let n = r.count(MAX_OBJECTS).ok_or(M("item count"))?;
        let mut items = Vec::with_capacity(n.min(4096));
        // Each top-level item is followed by its whole subtree: a parent is never before the latest top.
        let mut top = 0usize;
        for i in 0..n {
            let parent = match r.u32().ok_or(M("parent"))? {
                u32::MAX => {
                    top = i;
                    None
                }
                p if (p as usize) < i && (p as usize) >= top => Some(p),
                _ => return Err(M("parent")),
            };
            let k = r.u32().ok_or(M("row count"))? as usize;
            if k > MAX_ROWS {
                return Err(M("row count"));
            }
            let mut rows: Vec<(ComponentId, Vec<u8>)> = Vec::with_capacity(k);
            for _ in 0..k {
                let c = ComponentId(r.u128().ok_or(M("component"))?);
                if c == ComponentId::NODE || c == ids::BODY_STATE || rows.last().is_some_and(|l| l.0 >= c) {
                    return Err(M("component"));
                }
                let len = r.count(MAX_ROW_BYTES).ok_or(M("row"))?;
                rows.push((c, r.take(len).ok_or(M("row"))?.to_vec()));
            }
            items.push(Item { parent, rows });
        }
        let nb = r.count(MAX_OBJECTS).ok_or(M("blob count"))?;
        let mut blobs = BTreeMap::new();
        let mut last: Option<[u8; 32]> = None;
        for _ in 0..nb {
            let len = r.count(MAX_BLOB).ok_or(M("blob"))?;
            let b = r.take(len).ok_or(M("blob"))?.to_vec();
            let h = BlobRef::of(&b).hash;
            if last.is_some_and(|l| l >= h) {
                return Err(M("blob order"));
            }
            last = Some(h);
            blobs.insert(h, b);
        }
        if !r.0.is_empty() {
            return Err(M("trailing bytes"));
        }
        Ok(Content { items, blobs, skipped_rows: 0 })
    }
}

/// Builds content object by object, each with a `Name` and its blob (the starter set, other presets' objects).
#[derive(Default)]
pub struct Builder(Content);

impl Builder {
    /// A top-level object: its `Transform` row is relative to the content's origin (`Transform::local`). Returns its
    /// item index.
    pub fn top(&mut self, rows: &[Row], name: &str) -> u32 {
        self.push(None, rows, name)
    }

    /// An object under item `parent`, which must be the last top-level object or one of its subtree, so that each
    /// top-level object is followed by its whole subtree ([`Content::decode`] refuses any other order).
    pub fn child(&mut self, parent: u32, rows: &[Row], name: &str) -> u32 {
        self.push(Some(parent), rows, name)
    }

    pub fn finish(self) -> Content {
        self.0
    }

    fn push(&mut self, parent: Option<u32>, rows: &[Row], name: &str) -> u32 {
        let text = BlobRef::of(name.as_bytes());
        self.0.blobs.insert(text.hash, name.as_bytes().to_vec());
        let mut out: Vec<(ComponentId, Vec<u8>)> = rows.iter().map(|r| (r.component, r.bytes.clone())).collect();
        out.retain(|r| r.0 != ids::NAME);
        out.push((ids::NAME, bytemuck::bytes_of(&Name { text }).to_vec()));
        out.sort_by_key(|r| r.0);
        self.0.items.push(Item { parent, rows: out });
        self.0.items.len() as u32 - 1
    }
}

impl crate::WorldSnapshot {
    /// [`Content::from_world`] of the world this snapshot shows (a running session's save, off the simulation thread).
    pub fn content(&self, roots: &[ObjectId], origin: Pos) -> Result<Content, ContentError> {
        Content::from_world(&self.world, roots, origin)
    }

    /// [`World::content_roots`] of the world this snapshot shows.
    pub fn content_roots(&self, exclude: &[ObjectId]) -> Vec<ObjectId> {
        self.world.content_roots(exclude)
    }
}

impl World {
    /// The roots that make up a domain's content: every root except those in `exclude` (a domain's own scenery) and
    /// those holding a member's hand or body, in id order.
    pub fn content_roots(&self, exclude: &[ObjectId]) -> Vec<ObjectId> {
        let store = self.store();
        store
            .roots()
            .filter(|r| !exclude.contains(r))
            .filter(|&r| !store.subtree(r).iter().any(|&x| store.has::<Hand>(x) || store.has::<Player>(x)))
            .collect()
    }
}

/// A root transform's row made relative to `origin`.
fn relative(row: &[u8], origin: Pos) -> Result<Vec<u8>, ContentError> {
    let t: Transform = bytemuck::try_pod_read_unaligned(row).map_err(|_| ContentError::Malformed("transform"))?;
    let at = t.root_pos().ok_or(ContentError::OutOfRange)?;
    let d = at - origin;
    Ok(bytemuck::bytes_of(&Transform { chunk: [0; 3], pos: d.0, ..t }).to_vec())
}

/// A stored top-level transform placed at `origin`.
fn placed(row: &[u8], origin: Pos) -> Result<Vec<u8>, ContentError> {
    let t: Transform = bytemuck::try_pod_read_unaligned(row).map_err(|_| ContentError::Malformed("transform"))?;
    if t.chunk != [0; 3] {
        return Err(ContentError::Malformed("transform"));
    }
    let at = origin.checked_add(Offset(t.pos)).map_err(|_| ContentError::OutOfRange)?;
    let p = Transform::at(at);
    Ok(bytemuck::bytes_of(&Transform { chunk: p.chunk, pos: p.pos, ..t }).to_vec())
}

fn empty(actor: &Actor, label: &str) -> Transaction {
    Transaction {
        actor: actor.clone(),
        label: Some(label.chars().take(crate::edit::MAX_LABEL).collect()),
        preconditions: Vec::new(),
        ops: Vec::new(),
        blobs: Vec::new(),
        inline: Vec::new(),
    }
}

/// `b`'s ops and blobs after `t`'s (`b`'s new indices must already follow `t`'s).
fn append(t: &mut Transaction, b: Transaction) {
    t.ops.extend(b.ops);
    t.blobs.extend(b.blobs);
    t.inline.extend(b.inline);
}

/// `t` with every new index and every `New` target moved up by `by` (the caller's parent is never `New`).
fn shift(mut t: Transaction, by: u32) -> Transaction {
    for op in &mut t.ops {
        if let Op::Create { new, parent, .. } = op {
            *new += by;
            if let Target::New(k) = parent {
                *k += by;
            }
        }
    }
    t
}

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u128(&mut self) -> Option<u128> {
        Some(u128::from_le_bytes(self.take(16)?.try_into().ok()?))
    }
    /// A u64 count, at most `max`.
    fn count(&mut self, max: usize) -> Option<usize> {
        let n = u64::from_le_bytes(self.take(8)?.try_into().ok()?);
        usize::try_from(n).ok().filter(|&n| n <= max)
    }
}
