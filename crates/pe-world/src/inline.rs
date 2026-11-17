//! Inline blobs (world-model.md 3.4; @coordinator-opus's ruling m84f3b5262a91, on @fable-reviewer's advice): a blob
//! of at most [`MAX_INLINE_BLOB`] bytes is of the inline class, by size alone, display and simulation blobs alike.
//! Its bytes ride inside the signed entry that introduces it, and in a state import, so every machine that has the
//! entry or the state has the bytes. A larger blob is never inline.

use std::collections::{BTreeMap, BTreeSet};

use pe_core::ObjectId;
use pe_seams::Reason;
use pe_sim::{ComponentDesc, FieldKind, Registry};

use crate::components::BlobRef;
use crate::edit::{Op, Transaction};
use crate::world::World;

/// The largest inline-class blob, in bytes (3.4).
pub const MAX_INLINE_BLOB: usize = 256;
/// The most inline bytes one entry may carry (3.4): over it, apply refuses the transaction with `TooLarge`, the
/// verdict pe-api gives first.
pub const MAX_INLINE_BYTES: usize = 1024;

/// Whether a reference is of the inline class (3.4): 1 to [`MAX_INLINE_BLOB`] bytes long. "None" and the empty
/// blob are not: every machine has the empty blob's bytes (@engine-architect ma16d20a4d421).
pub fn is_inline(r: &BlobRef) -> bool {
    (1..=MAX_INLINE_BLOB as u64).contains(&r.len)
}

/// The blob references in a row of `desc`, in field order: every element of every `BlobRef` and `SimBlobRef` field,
/// each with whether it is a simulation blob. `None` if the row is not `desc`'s size.
pub fn row_blobs(desc: &ComponentDesc, row: &[u8]) -> Option<Vec<(BlobRef, bool)>> {
    if row.len() != desc.size() {
        return None;
    }
    let mut out = Vec::new();
    for f in desc.fields().iter().filter(|f| matches!(f.kind, FieldKind::BlobRef | FieldKind::SimBlobRef)) {
        for k in 0..f.count as usize {
            out.push((blob_at(row, f.offset as usize + k * 40)?, f.kind == FieldKind::SimBlobRef));
        }
    }
    Some(out)
}

/// The inline-class references the state holds for these objects, and for every tombstone if `tombstones` (their
/// kept rows, which `Undelete` restores; @engine-architect ma16d20a4d421), each with whether any field holding it is a
/// simulation blob (3.4).
pub(crate) fn state_refs(w: &World, objects: &[ObjectId], tombstones: bool) -> BTreeMap<BlobRef, bool> {
    let reg = w.store.registry();
    let mut out: BTreeMap<BlobRef, bool> = BTreeMap::new();
    let mut add = |c, row: &[u8]| {
        for (r, sim) in reg.get(c).and_then(|d| row_blobs(d, row)).unwrap_or_default() {
            if is_inline(&r) {
                *out.entry(r).or_default() |= sim;
            }
        }
    };
    for &id in objects {
        for d in reg.iter() {
            if let Some(row) = w.store.get_raw(d.id(), id) {
                add(d.id(), row);
            }
        }
    }
    if tombstones {
        for t in w.tombstones.values() {
            t.rows.iter().for_each(|(c, row, _)| add(*c, row));
        }
    }
    out
}

fn blob_at(bytes: &[u8], at: usize) -> Option<BlobRef> {
    let b = bytes.get(at..at + 40)?;
    Some(BlobRef { hash: b[..32].try_into().ok()?, len: u64::from_le_bytes(b[32..].try_into().ok()?) })
}

/// The inline-class references a transaction's ops introduce (3.4): each one a `Create` row, a `SetRow` or a
/// `SetField` of a blob field writes. A `Copy` introduces none: its rows' blobs are on every machine that has the
/// source. Read against `reg` after the ops, so a component the transaction registers is known; an op naming an
/// unknown component or a wrong size introduces nothing (the op itself was refused, or the edit would not be here).
pub fn introduced(reg: &Registry, tx: &Transaction) -> BTreeSet<BlobRef> {
    let row = |c, bytes: &[u8]| -> Vec<BlobRef> {
        reg.get(c)
            .and_then(|d| row_blobs(d, bytes))
            .unwrap_or_default()
            .into_iter()
            .map(|b| b.0)
            .filter(is_inline)
            .collect()
    };
    let mut out = BTreeSet::new();
    for op in &tx.ops {
        match op {
            Op::Create { rows, .. } => rows.iter().for_each(|r| out.extend(row(r.component, &r.bytes))),
            Op::SetRow { row: r, .. } => out.extend(row(r.component, &r.bytes)),
            Op::SetField { component, field, value, .. } => {
                let f = reg.get(*component).and_then(|d| d.fields().get(*field as usize));
                if let Some(f) = f.filter(|f| matches!(f.kind, FieldKind::BlobRef | FieldKind::SimBlobRef)) {
                    out.extend((0..f.count as usize).filter_map(|k| blob_at(value, k * 40)).filter(is_inline));
                }
            }
            _ => {}
        }
    }
    out
}

/// Checks a transaction's inline bytes against what its ops introduce (3.4), after the ops ran: over
/// [`MAX_INLINE_BYTES`] in total is `TooLarge`, and a blob no op introduces is `Invalid`. The same verdict on every
/// machine, since it reads only the entry and the registry.
pub fn check(reg: &Registry, tx: &Transaction, inline: &[Vec<u8>]) -> Result<(), Reason> {
    if inline.iter().map(Vec::len).sum::<usize>() > MAX_INLINE_BYTES {
        return Err(Reason::TooLarge { limit: MAX_INLINE_BYTES as u32 });
    }
    let wanted = introduced(reg, tx);
    if inline.iter().any(|b| !wanted.contains(&BlobRef::of(b))) {
        return Err(Reason::Invalid { field: None });
    }
    Ok(())
}

/// Whether a list of inline blobs is in its one encoding (3.4): each 1 to [`MAX_INLINE_BLOB`] bytes, in strictly
/// ascending order of hash, so none twice.
pub fn canonical(inline: &[Vec<u8>]) -> bool {
    inline.iter().all(|b| (1..=MAX_INLINE_BLOB).contains(&b.len()))
        && inline.windows(2).all(|w| BlobRef::of(&w[0]).hash < BlobRef::of(&w[1]).hash)
}
