//! Component descriptions (design 16.1, 16.5).
//!
//! A component is a fixed-size row of plain data. Its description lists every field (name, kind,
//! offset, element count) and must cover the row exactly: no gaps, no overlaps, every field at its
//! natural alignment and the row a multiple of its largest alignment. So a row has no padding bytes:
//! every byte is a field, which is what makes rows safe to hash and copy as bytes. Padding a native
//! struct needs is written as an explicit field (for example `_pad: u32`).
//!
//! The description doubles as the IDL: logic modules declare components the same way (16.5), and the MCP
//! JSON Schemas come from it (17.2).

use std::collections::BTreeMap;

use bytemuck::Pod;
use pe_core::Error;
use pe_core::hash::Hasher;

/// A component type. Ids below [`ComponentId::FIRST_USER`] are reserved for the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentId(pub u128);

impl ComponentId {
    /// The hierarchy ([`Node`](crate::Node)).
    pub const NODE: ComponentId = ComponentId(1);
    /// [`Transform`](crate::Transform).
    pub const TRANSFORM: ComponentId = ComponentId(2);
    /// The first id available to logic modules and tests. Lower ids are the engine's
    /// ([`Registry::register_engine`]).
    pub const FIRST_USER: u128 = 1024;
}

/// The largest row a component may have: 64 KiB.
pub const MAX_ROW: u32 = 64 * 1024;

/// The kind of one field element.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldKind {
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    /// Must be finite.
    F32,
    /// Must be finite.
    F64,
    /// One byte, 0 or 1.
    Bool,
    /// An [`ObjectId`](pe_core::ObjectId): 16 bytes, 8-byte aligned (world-model.md 3.2).
    ObjectRef,
    /// A display blob: a 32-byte hash and a `u64` length; all-zero is "none" (3.4).
    BlobRef,
    /// A simulation blob, the same layout (3.4).
    SimBlobRef,
    /// An exact position, a [`PosRow`](pe_core::coords::PosRow): 48 bytes (4.2).
    Pos,
    /// A member's public key, 32 bytes.
    MemberKey,
    /// An entry id, 32 bytes.
    EntryRef,
    /// Any other 256-bit hash.
    Hash,
}

impl FieldKind {
    pub fn size(self) -> u32 {
        match self {
            FieldKind::U8 | FieldKind::I8 | FieldKind::Bool => 1,
            FieldKind::U16 | FieldKind::I16 => 2,
            FieldKind::U32 | FieldKind::I32 | FieldKind::F32 => 4,
            FieldKind::U64 | FieldKind::I64 | FieldKind::F64 => 8,
            FieldKind::ObjectRef => 16,
            FieldKind::MemberKey | FieldKind::EntryRef | FieldKind::Hash => 32,
            FieldKind::BlobRef | FieldKind::SimBlobRef => 40,
            FieldKind::Pos => 48,
        }
    }

    pub fn align(self) -> u32 {
        match self {
            FieldKind::ObjectRef | FieldKind::BlobRef | FieldKind::SimBlobRef | FieldKind::Pos => 8,
            FieldKind::MemberKey | FieldKind::EntryRef | FieldKind::Hash => 1,
            k => k.size(),
        }
    }

    fn code(self) -> u8 {
        self as u8
    }
}

/// The colour space of a field that holds a colour (world-model.md 3.6; api.md 4.4; @coordinator-opus
/// m42dc307a66de). It changes nothing about the stored bytes or the simulation: it tells a reader such as WorldApi
/// how the floats are meant, so a colour is written and read in one space, never guessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ColorSpace {
    /// Linear light with sRGB (Rec. 709) primaries; a fourth element is straight (not premultiplied) alpha.
    LinearSrgb,
}

impl ColorSpace {
    /// Every space, in code order.
    pub const ALL: [ColorSpace; 1] = [ColorSpace::LinearSrgb];

    /// The space's code in a description's bytes, 1 and up; 0 is "not a colour" (3.6).
    pub fn code(self) -> u8 {
        match self {
            ColorSpace::LinearSrgb => 1,
        }
    }
}

/// One field: `count` elements of `kind` starting at byte `offset` of the row, in `unit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDesc {
    pub name: String,
    pub kind: FieldKind,
    pub offset: u32,
    pub count: u32,
    /// The field's unit in [`crate::unit`]'s canonical spelling, or empty for none (world-model.md 3.6). Part of
    /// the description and its hashes; [`ComponentDesc::new`] refuses any other spelling.
    pub unit: String,
    /// The colour space, if the field is a colour: only on 3 or 4 `F32` elements, never with a unit. Part of the
    /// description and its hashes, in the high bits of the kind's byte (3.6), so a field without one hashes as before.
    pub color: Option<ColorSpace>,
}

impl FieldDesc {
    /// A field with no unit. An offset beyond `u32` becomes `u32::MAX`, which no valid layout accepts.
    pub fn new(name: &str, kind: FieldKind, offset: usize, count: u32) -> Self {
        FieldDesc {
            name: name.to_string(),
            kind,
            offset: u32::try_from(offset).unwrap_or(u32::MAX),
            count,
            unit: String::new(),
            color: None,
        }
    }

    /// The same field as a colour in `space` (checked when the description is built).
    pub fn with_color(mut self, space: ColorSpace) -> Self {
        self.color = Some(space);
        self
    }

    /// The field's kind and colour space as one byte (world-model.md 3.6): the kind's code in the low five bits,
    /// the space's code (0 for none) in the high three.
    pub fn kind_byte(&self, kind_code: u8) -> u8 {
        kind_code | self.color.map_or(0, ColorSpace::code) << 5
    }

    /// Splits a [`Self::kind_byte`] into the kind's code and the colour space; `None` for an unknown space.
    pub fn split_kind_byte(b: u8) -> Option<(u8, Option<ColorSpace>)> {
        let space = match b >> 5 {
            0 => None,
            c => Some(*ColorSpace::ALL.iter().find(|s| s.code() == c)?),
        };
        Some((b & 0x1F, space))
    }

    /// The same field in `unit` (checked when the description is built).
    pub fn with_unit(mut self, unit: &str) -> Self {
        self.unit = unit.to_string();
        self
    }

    /// The byte after the field, or `None` on overflow (review P21-3).
    fn end(&self) -> Option<u32> {
        self.kind.size().checked_mul(self.count)?.checked_add(self.offset)
    }
}

/// A validated component description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentDesc {
    id: ComponentId,
    name: String,
    size: u32,
    fields: Vec<FieldDesc>,
    schema_hash: u64,
}

impl ComponentDesc {
    /// Validates the layout (see the module docs). Fields may be listed in any order; they are kept sorted
    /// by offset. No fields is a zero-size component, whose rows are empty: a script's tag (world-model.md 7.9).
    pub fn new(id: ComponentId, name: &str, mut fields: Vec<FieldDesc>) -> Result<Self, Error> {
        let bad = |m: String| Err(Error::BadLayout(format!("component {name} ({}): {m}", id.0)));
        fields.sort_by_key(|f| f.offset);
        let mut at = 0u32;
        let mut max_align = 1u32;
        for f in &fields {
            if f.count == 0 {
                return bad(format!("field {} has no elements", f.name));
            }
            if f.name.starts_with('_') && !f.unit.is_empty() {
                return bad(format!("padding field {} has a unit", f.name));
            }
            if let Err(e) = crate::unit::check_unit(&f.unit) {
                return bad(format!("field {}: {e}", f.name));
            }
            if f.color.is_some()
                && (f.kind != FieldKind::F32
                    || !(3..=4).contains(&f.count)
                    || !f.unit.is_empty()
                    || f.name.starts_with('_'))
            {
                return bad(format!("field {} is a colour but not 3 or 4 F32 elements without a unit", f.name));
            }
            if f.offset != at {
                return bad(format!("field {} at {} leaves a gap or overlaps (expected {at})", f.name, f.offset));
            }
            if f.offset % f.kind.align() != 0 {
                return bad(format!("field {} is not aligned to {}", f.name, f.kind.align()));
            }
            max_align = max_align.max(f.kind.align());
            at = match f.end() {
                Some(e) if e <= MAX_ROW => e,
                _ => return bad(format!("field {} ends beyond the {MAX_ROW}-byte row limit", f.name)),
            };
        }
        if !at.is_multiple_of(max_align) {
            return bad(format!("row size {at} is not a multiple of its alignment {max_align}"));
        }
        let mut h = Hasher::new(0x5045_5343_4845_4D41); // "PESCHEMA"
        h.write(&id.0.to_le_bytes());
        h.write_u64(name.len() as u64);
        h.write(name.as_bytes());
        h.write_u32(at);
        for f in &fields {
            h.write_u64(f.name.len() as u64);
            h.write(f.name.as_bytes());
            h.write_u8(f.kind_byte(f.kind.code()));
            h.write_u32(f.offset);
            h.write_u32(f.count);
            h.write_u64(f.unit.len() as u64);
            h.write(f.unit.as_bytes());
        }
        Ok(ComponentDesc { id, name: name.to_string(), size: at, fields, schema_hash: h.finish() })
    }

    pub fn id(&self) -> ComponentId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Row size in bytes.
    pub fn size(&self) -> usize {
        self.size as usize
    }

    pub fn fields(&self) -> &[FieldDesc] {
        &self.fields
    }

    /// Hash of the whole description: snapshots record it, and restoring into a different schema fails.
    pub fn schema_hash(&self) -> u64 {
        self.schema_hash
    }

    /// The first field of `row` that holds a non-finite float, a bool other than 0 or 1, an invalid `Pos`, or a
    /// nonzero byte in a padding field (a field whose name starts with `_`). Padding must be zero because readers
    /// hide it while every hash covers it: a nonzero pad would be state no tool can see (@chunks-reviewer's M1 on
    /// world/read-v1). Inserts, `check_values` and `Snapshot::from_bytes` all call this.
    pub fn invalid_field(&self, row: &[u8]) -> Option<&str> {
        debug_assert_eq!(row.len(), self.size());
        for f in &self.fields {
            if f.name.starts_with('_') {
                let end = f.end().map_or(row.len(), |e| (e as usize).min(row.len()));
                if row.get(f.offset as usize..end).is_none_or(|b| b.iter().any(|&x| x != 0)) {
                    return Some(&f.name);
                }
                continue;
            }
            let w = f.kind.size() as usize;
            for k in 0..f.count as usize {
                let b = &row[f.offset as usize + k * w..][..w];
                let ok = match f.kind {
                    FieldKind::F32 => f32::from_le_bytes(b.try_into().unwrap()).is_finite(),
                    FieldKind::F64 => f64::from_le_bytes(b.try_into().unwrap()).is_finite(),
                    FieldKind::Bool => b[0] <= 1,
                    FieldKind::Pos => pe_core::Pos::from_row(bytemuck::pod_read_unaligned(b)).is_ok(),
                    _ => true,
                };
                if !ok {
                    return Some(&f.name);
                }
            }
        }
        None
    }
}

/// A native component type: a plain-data struct with a description that matches its layout.
///
/// `fields()` must describe the struct exactly (use `core::mem::offset_of!`); [`Component::desc`] checks
/// the size, and `Pod` guarantees there is no padding.
pub trait Component: Pod {
    const ID: ComponentId;
    const NAME: &'static str;
    fn fields() -> Vec<FieldDesc>;

    fn desc() -> ComponentDesc {
        let d = ComponentDesc::new(Self::ID, Self::NAME, Self::fields())
            .unwrap_or_else(|e| panic!("bad description for {}: {e}", Self::NAME));
        assert_eq!(
            d.size(),
            core::mem::size_of::<Self>(),
            "{}: description does not match the struct size",
            Self::NAME
        );
        assert!(core::mem::align_of::<Self>() <= 8, "{}: alignment above 8", Self::NAME);
        d
    }
}

/// The components a world knows. Its schema is configuration, not state: every peer of a world registers
/// the same components (engine version plus the active logic modules), and snapshots carry schema hashes
/// to check it.
#[derive(Clone, Debug)]
pub struct Registry {
    comps: BTreeMap<ComponentId, ComponentDesc>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// A registry with the built-in components ([`Node`](crate::Node), [`Transform`](crate::Transform)).
    pub fn new() -> Self {
        let mut r = Registry { comps: BTreeMap::new() };
        r.register_type::<crate::Node>().unwrap();
        r.register_type::<crate::Transform>().unwrap();
        r
    }

    /// Adds a runtime description (logic modules, tests): its id must be at least
    /// [`ComponentId::FIRST_USER`]. Registering the same description twice is a no-op; a different one under
    /// the same id is an error.
    pub fn register(&mut self, desc: ComponentDesc) -> Result<(), Error> {
        if desc.id.0 < ComponentId::FIRST_USER {
            return Err(Error::Reserved { component: desc.id.0 });
        }
        self.register_engine(desc)
    }

    /// Adds a description under any id, including the engine's reserved ids below
    /// [`ComponentId::FIRST_USER`]. For engine crates only (`pe-physics` and others).
    pub fn register_engine(&mut self, desc: ComponentDesc) -> Result<(), Error> {
        match self.comps.get(&desc.id) {
            Some(d) if d == &desc => Ok(()),
            Some(_) => Err(Error::SchemaMismatch { component: desc.id.0 }),
            None => {
                self.comps.insert(desc.id, desc);
                Ok(())
            }
        }
    }

    /// Registers a native component type (engine or user id).
    pub fn register_type<T: Component>(&mut self) -> Result<(), Error> {
        self.register_engine(T::desc())
    }

    pub fn get(&self, id: ComponentId) -> Option<&ComponentDesc> {
        self.comps.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ComponentDesc> {
        self.comps.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_rules() {
        let id = ComponentId(2000);
        let ok = ComponentDesc::new(
            id,
            "c",
            vec![FieldDesc::new("b", FieldKind::F32, 8, 2), FieldDesc::new("a", FieldKind::U64, 0, 1)],
        )
        .unwrap();
        assert_eq!(ok.size(), 16);
        assert_eq!(ok.fields()[0].name, "a");
        // A gap (implicit padding) is refused.
        assert!(
            ComponentDesc::new(
                id,
                "c",
                vec![FieldDesc::new("a", FieldKind::U8, 0, 1), FieldDesc::new("b", FieldKind::U32, 4, 1)]
            )
            .is_err()
        );
        // A trailing pad to the alignment is required.
        assert!(
            ComponentDesc::new(
                id,
                "c",
                vec![FieldDesc::new("a", FieldKind::U64, 0, 1), FieldDesc::new("b", FieldKind::U8, 8, 1)]
            )
            .is_err()
        );
        // Misaligned and overlapping fields are refused.
        assert!(
            ComponentDesc::new(
                id,
                "c",
                vec![FieldDesc::new("a", FieldKind::U8, 0, 2), FieldDesc::new("b", FieldKind::U16, 1, 1)]
            )
            .is_err()
        );
        // The schema hash changes with any part of the description.
        let other = ComponentDesc::new(
            id,
            "c",
            vec![FieldDesc::new("a", FieldKind::U64, 0, 1), FieldDesc::new("b", FieldKind::I32, 8, 2)],
        )
        .unwrap();
        assert_ne!(ok.schema_hash(), other.schema_hash());
        let mut r = Registry::new();
        r.register(ok.clone()).unwrap();
        r.register(ok).unwrap();
        assert_eq!(r.register(other), Err(Error::SchemaMismatch { component: 2000 }));
    }

    /// world-model.md 3.6: a unit is part of the description and its hash; only the canonical spelling is accepted,
    /// "no unit" has one form, and padding has none.
    #[test]
    fn units() {
        let id = ComponentId(2005);
        let with = |u: &str| ComponentDesc::new(id, "u", vec![FieldDesc::new("d", FieldKind::F32, 0, 1).with_unit(u)]);
        let none = with("").unwrap();
        let si = with("kg/m^3").unwrap();
        assert_eq!(si.fields()[0].unit, "kg/m^3");
        assert_ne!(none.schema_hash(), si.schema_hash());
        assert_ne!(si.schema_hash(), with("m").unwrap().schema_hash());
        for bad in ["m^-3*kg", "kg/m3", "meter", "1", "m/m", " ", "srgb"] {
            assert!(matches!(with(bad), Err(Error::BadLayout(_))), "{bad:?} accepted");
        }
        let pad = ComponentDesc::new(
            id,
            "p",
            vec![
                FieldDesc::new("a", FieldKind::U32, 0, 1),
                FieldDesc::new("_pad", FieldKind::U32, 4, 1).with_unit("m"),
            ],
        );
        assert!(pad.is_err());
        let mut r = Registry::new();
        r.register(none).unwrap();
        assert_eq!(r.register(si), Err(Error::SchemaMismatch { component: 2005 }));
    }

    /// world-model.md 3.6 (@coordinator-opus m42dc307a66de): a colour space is part of the description and its
    /// hash, only on 3 or 4 F32 elements without a unit, and a field without one hashes as it did before.
    #[test]
    fn colour_spaces() {
        let id = ComponentId(2006);
        let one = |f: FieldDesc| ComponentDesc::new(id, "c", vec![f]);
        let plain = one(FieldDesc::new("c", FieldKind::F32, 0, 4)).unwrap();
        let colour = one(FieldDesc::new("c", FieldKind::F32, 0, 4).with_color(ColorSpace::LinearSrgb)).unwrap();
        assert_eq!(colour.fields()[0].color, Some(ColorSpace::LinearSrgb));
        assert_ne!(plain.schema_hash(), colour.schema_hash());
        assert!(one(FieldDesc::new("c", FieldKind::F32, 0, 3).with_color(ColorSpace::LinearSrgb)).is_ok());
        for bad in [
            FieldDesc::new("c", FieldKind::F32, 0, 2),
            FieldDesc::new("c", FieldKind::F32, 0, 5),
            FieldDesc::new("c", FieldKind::F64, 0, 3),
            FieldDesc::new("c", FieldKind::U32, 0, 4),
            FieldDesc::new("c", FieldKind::F32, 0, 3).with_unit("m"),
            FieldDesc::new("_c", FieldKind::F32, 0, 4),
        ] {
            assert!(one(bad.clone().with_color(ColorSpace::LinearSrgb)).is_err(), "{bad:?} accepted as a colour");
        }
        // The byte: an unmarked field's is its kind's code alone, so every description without a colour keeps its hash.
        let f = FieldDesc::new("c", FieldKind::F32, 0, 4);
        assert_eq!(f.kind_byte(8), 8);
        let c = f.with_color(ColorSpace::LinearSrgb);
        assert_eq!(c.kind_byte(8), 8 | 1 << 5);
        assert_eq!(FieldDesc::split_kind_byte(8 | 1 << 5), Some((8, Some(ColorSpace::LinearSrgb))));
        assert_eq!(FieldDesc::split_kind_byte(8), Some((8, None)));
        assert_eq!(FieldDesc::split_kind_byte(8 | 2 << 5), None);
    }

    #[test]
    fn overflow_and_reserved_ids() {
        // Review P21-3: a count that overflows u32 is refused, not wrapped to a zero-size row.
        let huge =
            ComponentDesc::new(ComponentId(2002), "h", vec![FieldDesc::new("a", FieldKind::U32, 0, 0x4000_0000)]);
        assert!(matches!(huge, Err(Error::BadLayout(_))));
        let big = ComponentDesc::new(ComponentId(2003), "b", vec![FieldDesc::new("a", FieldKind::U8, 0, MAX_ROW + 1)]);
        assert!(big.is_err());
        assert!(
            ComponentDesc::new(ComponentId(2004), "o", vec![FieldDesc::new("a", FieldKind::U8, usize::MAX, 1)])
                .is_err()
        );
        // Review P21-11: runtime descriptions cannot take the engine's ids.
        let low = ComponentDesc::new(ComponentId(7), "low", vec![FieldDesc::new("a", FieldKind::U32, 0, 1)]).unwrap();
        let mut r = Registry::new();
        assert_eq!(r.register(low.clone()), Err(Error::Reserved { component: 7 }));
        r.register_engine(low).unwrap();
    }

    #[test]
    fn value_checks() {
        let d = ComponentDesc::new(
            ComponentId(2001),
            "v",
            vec![
                FieldDesc::new("x", FieldKind::F32, 0, 2),
                FieldDesc::new("flag", FieldKind::Bool, 8, 1),
                FieldDesc::new("_pad", FieldKind::U8, 9, 3),
            ],
        )
        .unwrap();
        let mut row = [0u8; 12];
        assert_eq!(d.invalid_field(&row), None);
        row[4..8].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(d.invalid_field(&row), Some("x"));
        row[4..8].copy_from_slice(&1.0f32.to_le_bytes());
        row[8] = 2;
        assert_eq!(d.invalid_field(&row), Some("flag"));
        row[8] = 1;
        assert_eq!(d.invalid_field(&row), None);
        row[11] = 0x80;
        assert_eq!(d.invalid_field(&row), Some("_pad"), "padding must be zero");
    }
}
