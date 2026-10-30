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
use pe_core::hash::Hasher;
use pe_core::Error;

/// A component type. Ids below [`ComponentId::FIRST_USER`] are reserved for the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentId(pub u32);

impl ComponentId {
    /// The hierarchy ([`Node`](crate::Node)).
    pub const NODE: ComponentId = ComponentId(1);
    /// [`Transform`](crate::Transform).
    pub const TRANSFORM: ComponentId = ComponentId(2);
    /// The first id available to logic modules and tests.
    pub const FIRST_USER: u32 = 1024;
}

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
    /// An [`ObjectId`](pe_core::ObjectId): 8 bytes, 4-byte aligned.
    ObjectId,
}

impl FieldKind {
    pub fn size(self) -> u32 {
        match self {
            FieldKind::U8 | FieldKind::I8 | FieldKind::Bool => 1,
            FieldKind::U16 | FieldKind::I16 => 2,
            FieldKind::U32 | FieldKind::I32 | FieldKind::F32 => 4,
            FieldKind::U64 | FieldKind::I64 | FieldKind::F64 | FieldKind::ObjectId => 8,
        }
    }

    pub fn align(self) -> u32 {
        match self {
            FieldKind::ObjectId => 4,
            k => k.size(),
        }
    }

    fn code(self) -> u8 {
        self as u8
    }
}

/// One field: `count` elements of `kind` starting at byte `offset` of the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDesc {
    pub name: String,
    pub kind: FieldKind,
    pub offset: u32,
    pub count: u32,
}

impl FieldDesc {
    pub fn new(name: &str, kind: FieldKind, offset: usize, count: u32) -> Self {
        FieldDesc { name: name.to_string(), kind, offset: offset as u32, count }
    }

    fn end(&self) -> u32 {
        self.offset + self.kind.size() * self.count
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
    /// by offset.
    pub fn new(id: ComponentId, name: &str, mut fields: Vec<FieldDesc>) -> Result<Self, Error> {
        let bad = |m: String| Err(Error::BadLayout(format!("component {name} ({}): {m}", id.0)));
        if fields.is_empty() {
            return bad("no fields".into());
        }
        fields.sort_by_key(|f| f.offset);
        let mut at = 0u32;
        let mut max_align = 1u32;
        for f in &fields {
            if f.count == 0 {
                return bad(format!("field {} has no elements", f.name));
            }
            if f.offset != at {
                return bad(format!("field {} at {} leaves a gap or overlaps (expected {at})", f.name, f.offset));
            }
            if f.offset % f.kind.align() != 0 {
                return bad(format!("field {} is not aligned to {}", f.name, f.kind.align()));
            }
            max_align = max_align.max(f.kind.align());
            at = f.end();
        }
        if at % max_align != 0 {
            return bad(format!("row size {at} is not a multiple of its alignment {max_align}"));
        }
        let mut h = Hasher::new(0x5045_5343_4845_4D41); // "PESCHEMA"
        h.write_u32(id.0);
        h.write_u64(name.len() as u64);
        h.write(name.as_bytes());
        h.write_u32(at);
        for f in &fields {
            h.write_u64(f.name.len() as u64);
            h.write(f.name.as_bytes());
            h.write_u8(f.kind.code());
            h.write_u32(f.offset);
            h.write_u32(f.count);
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

    /// The first field of `row` that holds a non-finite float or a bool other than 0 or 1.
    pub fn invalid_field(&self, row: &[u8]) -> Option<&str> {
        debug_assert_eq!(row.len(), self.size());
        for f in &self.fields {
            let w = f.kind.size() as usize;
            for k in 0..f.count as usize {
                let b = &row[f.offset as usize + k * w..][..w];
                let ok = match f.kind {
                    FieldKind::F32 => f32::from_le_bytes(b.try_into().unwrap()).is_finite(),
                    FieldKind::F64 => f64::from_le_bytes(b.try_into().unwrap()).is_finite(),
                    FieldKind::Bool => b[0] <= 1,
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
        assert_eq!(d.size(), core::mem::size_of::<Self>(), "{}: description does not match the struct size", Self::NAME);
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

    /// Adds a description. Registering the same description twice is a no-op; a different one under the
    /// same id is an error.
    pub fn register(&mut self, desc: ComponentDesc) -> Result<(), Error> {
        match self.comps.get(&desc.id) {
            Some(d) if d == &desc => Ok(()),
            Some(_) => Err(Error::SchemaMismatch { component: desc.id.0 }),
            None => {
                self.comps.insert(desc.id, desc);
                Ok(())
            }
        }
    }

    pub fn register_type<T: Component>(&mut self) -> Result<(), Error> {
        self.register(T::desc())
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
        let ok = ComponentDesc::new(id, "c", vec![
            FieldDesc::new("b", FieldKind::F32, 8, 2),
            FieldDesc::new("a", FieldKind::U64, 0, 1),
        ])
        .unwrap();
        assert_eq!(ok.size(), 16);
        assert_eq!(ok.fields()[0].name, "a");
        // A gap (implicit padding) is refused.
        assert!(ComponentDesc::new(id, "c", vec![FieldDesc::new("a", FieldKind::U8, 0, 1), FieldDesc::new("b", FieldKind::U32, 4, 1)]).is_err());
        // A trailing pad to the alignment is required.
        assert!(ComponentDesc::new(id, "c", vec![FieldDesc::new("a", FieldKind::U64, 0, 1), FieldDesc::new("b", FieldKind::U8, 8, 1)]).is_err());
        // Misaligned and overlapping fields are refused.
        assert!(ComponentDesc::new(id, "c", vec![FieldDesc::new("a", FieldKind::U8, 0, 2), FieldDesc::new("b", FieldKind::U16, 1, 1)]).is_err());
        // The schema hash changes with any part of the description.
        let other = ComponentDesc::new(id, "c", vec![
            FieldDesc::new("a", FieldKind::U64, 0, 1),
            FieldDesc::new("b", FieldKind::I32, 8, 2),
        ])
        .unwrap();
        assert_ne!(ok.schema_hash(), other.schema_hash());
        let mut r = Registry::new();
        r.register(ok.clone()).unwrap();
        r.register(ok).unwrap();
        assert_eq!(r.register(other), Err(Error::SchemaMismatch { component: 2000 }));
    }

    #[test]
    fn value_checks() {
        let d = ComponentDesc::new(ComponentId(2001), "v", vec![
            FieldDesc::new("x", FieldKind::F32, 0, 2),
            FieldDesc::new("flag", FieldKind::Bool, 8, 1),
            FieldDesc::new("_pad", FieldKind::U8, 9, 3),
        ])
        .unwrap();
        let mut row = [0u8; 12];
        assert_eq!(d.invalid_field(&row), None);
        row[4..8].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(d.invalid_field(&row), Some("x"));
        row[4..8].copy_from_slice(&1.0f32.to_le_bytes());
        row[8] = 2;
        assert_eq!(d.invalid_field(&row), Some("flag"));
    }
}
