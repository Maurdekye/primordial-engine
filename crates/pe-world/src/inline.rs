//! Inline blobs (world-model.md 3.4; @coordinator-opus's ruling, on @fable-reviewer's advice): a blob
//! of at most [`MAX_INLINE_BLOB`] bytes is of the inline class, by size alone, display and simulation blobs alike.
//! Its bytes ride inside the signed entry that introduces it, and in a state import, so every machine that has the
//! entry or the state has the bytes. A larger blob is never inline.
//!
//! A *carried* simulation blob (3.4; @coordinator-opus) rides the same way: one a `SimBlobRef` field
//! references, 257 bytes to [`MAX_CARRIED_BLOB`], so a mesh or a hull reaches every member and every joiner. It
//! shares the entry's inline list and its order; its bytes count against [`MAX_CARRIED_BYTES`], not the 2 KiB. A
//! display blob over 256 bytes is still by reference only.

use std::collections::{BTreeMap, BTreeSet};

use pe_core::ObjectId;
use pe_seams::Reason;
use pe_sim::{ComponentDesc, FieldKind, Registry};

use crate::components::BlobRef;
use crate::edit::{Op, Transaction};
use crate::world::World;

/// The largest inline-class blob, in bytes (3.4).
pub const MAX_INLINE_BLOB: usize = 256;
/// The most inline bytes one entry may carry (3.4): over it, apply refuses the transaction with `TooLarge`. pe-api
/// refuses any edit over 1 KiB first; this hard cap is twice that, so an undo whose old values grew through a race
/// still fits (@coordinator-opus).
pub const MAX_INLINE_BYTES: usize = 2048;
/// The largest carried simulation blob, in bytes (3.4): 960 KiB, so a carried blob and its edit fit in one entry's
/// `MAX_PAYLOAD` (1 MiB + 4 KiB) with 64 KiB to spare (@coordinator-opus, option a).
pub const MAX_CARRIED_BLOB: usize = 960 * 1024;
/// The most carried simulation bytes one entry may carry (3.4): over it, apply refuses with `TooLarge`. pe-api
/// refuses any edit over its own 768 KiB first, so an undo whose restored blobs grew through a race still fits.
pub const MAX_CARRIED_BYTES: usize = 960 * 1024;
/// The most blobs an entry's inline list can hold: every inline-class one at a byte each, and every carried one at
/// its least, 257 bytes. A decoder bound only; the byte caps are apply's.
pub const MAX_INLINE_COUNT: usize = MAX_INLINE_BYTES + MAX_CARRIED_BYTES / (MAX_INLINE_BLOB + 1);

/// Whether a reference is of the inline class (3.4): 1 to [`MAX_INLINE_BLOB`] bytes long. "None" and the empty
/// blob are not: every machine has the empty blob's bytes (@engine-architect).
pub fn is_inline(r: &BlobRef) -> bool {
    (1..=MAX_INLINE_BLOB as u64).contains(&r.len)
}

/// Whether a reference's bytes travel inside the entry that introduces it, and in a state import (3.4): an
/// inline-class one from any blob field, or, when `sim` (a `SimBlobRef` field holds it), one of 257 bytes to
/// [`MAX_CARRIED_BLOB`]. A larger simulation blob, or a display blob over 256 bytes, travels by reference only.
pub fn is_carried(r: &BlobRef, sim: bool) -> bool {
    is_inline(r) || (sim && (MAX_INLINE_BLOB as u64 + 1..=MAX_CARRIED_BLOB as u64).contains(&r.len))
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

/// The references the state holds for these objects whose bytes travel with it ([`is_carried`]), and for every
/// tombstone if `tombstones` (their kept rows, which `Undelete` restores; @engine-architect), each with
/// whether any field holding it is a simulation blob (3.4).
pub(crate) fn state_refs(w: &World, objects: &[ObjectId], tombstones: bool) -> BTreeMap<BlobRef, bool> {
    let reg = w.store.registry();
    let mut out: BTreeMap<BlobRef, bool> = BTreeMap::new();
    let mut add = |c, row: &[u8]| {
        for (r, sim) in reg.get(c).and_then(|d| row_blobs(d, row)).unwrap_or_default() {
            if is_carried(&r, sim) {
                *out.entry(r).or_default() |= sim;
            }
        }
    };
    // One row per component: M1 has a single instance of each, as the tree's hashing assumes; a second instance
    // must be read here too when instances come (@engine-architect's nit).
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

/// The references a transaction's ops introduce whose bytes travel with it (3.4, [`is_carried`]): each one a `Create`
/// row, a `SetRow` or a `SetField` of a blob field writes. A `Copy` introduces none: its rows' blobs are on every machine that has the
/// source. Read against `reg` after the ops, so a component the transaction registers is known; an op naming an
/// unknown component or a wrong size introduces nothing (the op itself was refused, or the edit would not be here).
pub fn introduced(reg: &Registry, tx: &Transaction) -> BTreeSet<BlobRef> {
    written(reg, tx).into_iter().filter(|(r, sim)| is_carried(r, *sim)).map(|b| b.0).collect()
}

/// Every blob reference a transaction's ops write (a `Create` row, a `SetRow`, a `SetField` of a blob field), each
/// with whether its field is a simulation blob; [`introduced`] keeps the carried ones.
fn written(reg: &Registry, tx: &Transaction) -> Vec<(BlobRef, bool)> {
    let row = |c, bytes: &[u8]| reg.get(c).and_then(|d| row_blobs(d, bytes)).unwrap_or_default();
    let mut out = Vec::new();
    for op in &tx.ops {
        match op {
            Op::Create { rows, .. } => rows.iter().for_each(|r| out.extend(row(r.component, &r.bytes))),
            Op::SetRow { row: r, .. } => out.extend(row(r.component, &r.bytes)),
            Op::SetField { component, field, value, .. } => {
                let f = reg.get(*component).and_then(|d| d.fields().get(*field as usize));
                if let Some(f) = f.filter(|f| matches!(f.kind, FieldKind::BlobRef | FieldKind::SimBlobRef)) {
                    let sim = f.kind == FieldKind::SimBlobRef;
                    out.extend((0..f.count as usize).filter_map(|k| blob_at(value, k * 40)).map(|r| (r, sim)));
                }
            }
            _ => {}
        }
    }
    out
}

/// Checks a transaction's inline bytes against what its ops introduce (3.4), after the ops ran: inline-class bytes
/// over [`MAX_INLINE_BYTES`] in total, or carried bytes (over 256 each) over [`MAX_CARRIED_BYTES`], is `TooLarge`
/// with that limit, and a blob no op introduces is `Invalid`. A simulation reference an op writes that is longer
/// than [`MAX_CARRIED_BLOB`] is `TooLarge { limit: MAX_CARRIED_BLOB }`: its bytes could not travel with it, so a
/// machine without them would build differently (@chunks-reviewer's M1), until the blob transport lifts it. The same verdict on every
/// machine, since it reads only the entry and the registry.
///
/// With `complete`, every inline-class reference an op introduces must come with its bytes too (3.4, phase 3), or
/// it is `Invalid`: a member's edit, received or dry-run. World logic's own drafts (genesis) are built on every
/// machine from its own store and never received, so they pass `complete = false`.
pub fn check(reg: &Registry, tx: &Transaction, inline: &[Vec<u8>], complete: bool) -> Result<(), Reason> {
    let small = |b: &&Vec<u8>| b.len() <= MAX_INLINE_BLOB;
    if inline.iter().filter(small).map(|b| b.len()).sum::<usize>() > MAX_INLINE_BYTES {
        return Err(Reason::TooLarge { limit: MAX_INLINE_BYTES as u32 });
    }
    if inline.iter().filter(|b| !small(b)).map(|b| b.len()).sum::<usize>() > MAX_CARRIED_BYTES {
        return Err(Reason::TooLarge { limit: MAX_CARRIED_BYTES as u32 });
    }
    let written = written(reg, tx);
    if written.iter().any(|(r, sim)| *sim && r.len > MAX_CARRIED_BLOB as u64) {
        return Err(Reason::TooLarge { limit: MAX_CARRIED_BLOB as u32 });
    }
    let wanted: BTreeSet<BlobRef> = written.into_iter().filter(|(r, sim)| is_carried(r, *sim)).map(|b| b.0).collect();
    let given: BTreeSet<BlobRef> = inline.iter().map(|b| BlobRef::of(b)).collect();
    if !given.is_subset(&wanted) || (complete && !wanted.is_subset(&given)) {
        return Err(Reason::Invalid { field: None });
    }
    Ok(())
}

/// Whether a list of inline blobs is in its one encoding (3.4): each 1 to [`MAX_CARRIED_BLOB`] bytes, in strictly
/// ascending order of hash, so none twice.
pub fn canonical(inline: &[Vec<u8>]) -> bool {
    inline.iter().all(|b| (1..=MAX_CARRIED_BLOB).contains(&b.len()))
        && inline.windows(2).all(|w| BlobRef::of(&w[0]).hash < BlobRef::of(&w[1]).hash)
}
