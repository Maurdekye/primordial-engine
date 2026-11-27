//! `pe.Pose` (docs/spec/logic.md 8): the bound object's world pose, which the runner computes for each bound row
//! from the store's composed transforms. A script binds it like an engine component, read only; it is never a column
//! of the store, never written, stored or hashed.
//!
//! - **Its rows** are pe-sim's `SimStore::world_transform` (ancestors included; plain IEEE arithmetic in a fixed
//!   order, so the same bits on every machine, pe-sim `transform.rs`), in pe-sdk's `Pose` layout: the `Pos` as the
//!   store's 48-byte row form, the world rotation and scale, and whether the object has no frame.
//! - **Who has one:** binding it joins `Transform`, so a system that binds it runs over objects with a `Transform`.
//! - **A world position out of range** (the composition leaves the `Pos` range, world-model.md 4.3) faults the call,
//!   the same on every machine; nothing is clamped.
//! - **For skipping unchanged systems (M2):** a `Pose` row depends on the object's own `Transform` and every
//!   ancestor's, not only on bound rows. A system that binds `pe.Pose` must count those as its inputs, or never be
//!   skipped (@shader-reviewer, U3 design review).

use pe_core::{ObjectId, Pos};
use pe_sim::{ComponentDesc, ComponentId, FieldDesc, FieldKind, SimStore};

use crate::runtime::Fault;

/// The name a script binds it by.
pub const NAME: &str = "pe.Pose";

/// Its id inside the runner: a reserved engine id (below `FIRST_USER`) that no registry holds, so it can never name
/// a store column. pe-logic's tests check that the world's registry leaves it free.
pub const ID: ComponentId = ComponentId(1000);

/// The row size: pe-sdk's `Pose`.
pub const SIZE: usize = 80;

/// The layout a script must declare for it, as a store description would give it (so the binder compares it as it
/// compares an engine component's).
pub fn desc() -> ComponentDesc {
    ComponentDesc::new(
        ID,
        NAME,
        vec![
            FieldDesc::new("pos", FieldKind::Pos, 0, 1),
            FieldDesc::new("rot", FieldKind::F32, 48, 4),
            FieldDesc::new("scale", FieldKind::F32, 64, 3),
            FieldDesc::new("root", FieldKind::U32, 76, 1),
        ],
    )
    .expect("a valid layout")
}

/// The 48-byte row form of a `Pos`: three `i64` chunk indices, then three `f64` offsets in the chunk, little-endian.
pub fn pos_row(p: Pos) -> [u8; 48] {
    let chunk = p.chunk_index();
    // Exact: the difference from the chunk's own corner is the offset inside the chunk (dc = 0).
    let local = (p - Pos::at_chunk(chunk).expect("a valid Pos's chunk is valid")).0;
    let mut out = [0u8; 48];
    for a in 0..3 {
        out[8 * a..8 * a + 8].copy_from_slice(&chunk[a].to_le_bytes());
        out[24 + 8 * a..32 + 8 * a].copy_from_slice(&local[a].to_le_bytes());
    }
    out
}

/// One object's `Pose` row.
pub fn row(store: &SimStore, object: ObjectId) -> Result<[u8; SIZE], Fault> {
    let w = store.world_transform(object).ok_or_else(|| Fault::InvalidRow {
        component: NAME.into(),
        field: "pos: the world position is out of range".into(),
    })?;
    let mut out = [0u8; SIZE];
    out[..48].copy_from_slice(&pos_row(w.pos));
    for (k, v) in w.rot.iter().enumerate() {
        out[48 + 4 * k..52 + 4 * k].copy_from_slice(&v.to_le_bytes());
    }
    for (k, v) in w.scale.iter().enumerate() {
        out[64 + 4 * k..68 + 4 * k].copy_from_slice(&v.to_le_bytes());
    }
    let root = store.frame_of(object).is_none() as u32;
    out[76..80].copy_from_slice(&root.to_le_bytes());
    Ok(out)
}
