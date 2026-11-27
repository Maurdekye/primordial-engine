//! Per-player fields (logic.md 7.4; world-model.md 3.7; the design @world-model reviewed): two world
//! tables (5.4), so they are hashed, rolled back, saved and joined with the world.
//!
//! - **Field modes:** which fields are per-player, keyed (component, field, scope, object). A component-wide row
//!   (scope 0) comes from a script version's manifest; an object row (scope 1), from a `SetFieldMode` edit, and
//!   overrides it. A synchronized field has no row. A field's mode is not in its component description, so a mode
//!   change re-keys nothing and the field's base value (the shared row) stays as it was.
//! - **The feed:** each member's last published value of a decoupled field, keyed (object, component, field,
//!   member), with the frame and the input's index in that frame's applied order at which it was accepted. "Live" is
//!   not stored: it is whether the member is in the governance table.
//!
//! Caps: at most [`MAX_PER_OBJECT`] per-player fields per object (object rows); a published value is at most
//! [`MAX_VALUE`] bytes. Component-wide rows come only from manifests, which their own limits bound.

use std::collections::BTreeMap;

use pe_core::{Frame, MemberKey, ObjectId};
use pe_sim::ComponentId;

/// At most this many per-player fields on one object (object-scope rows).
pub const MAX_PER_OBJECT: usize = 16;
/// The largest published value: a `Pos` (48 bytes) or a colour fits.
pub const MAX_VALUE: usize = 64;

/// A per-player field's mode (the user's names).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    Decoupled = 1,
    /// Decoupled and locked: never published.
    Desynchronized = 2,
}

impl Mode {
    pub fn from_code(c: u8) -> Option<Mode> {
        match c {
            1 => Some(Mode::Decoupled),
            2 => Some(Mode::Desynchronized),
            _ => None,
        }
    }
}

/// What a shared script's plain read of a decoupled field returns (logic.md 7.1); pe-logic's `feed::Selector`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Selector {
    Latest,
    Any,
    All,
    Sum,
    Average,
    Min,
    Max,
    Member(MemberKey),
    Holder,
    Owner,
}

impl Selector {
    /// Its code and member bytes (zero unless it names a member).
    pub fn code(&self) -> (u8, [u8; 32]) {
        match self {
            Selector::Latest => (0, [0; 32]),
            Selector::Any => (1, [0; 32]),
            Selector::All => (2, [0; 32]),
            Selector::Sum => (3, [0; 32]),
            Selector::Average => (4, [0; 32]),
            Selector::Min => (5, [0; 32]),
            Selector::Max => (6, [0; 32]),
            Selector::Member(m) => (7, m.0),
            Selector::Holder => (8, [0; 32]),
            Selector::Owner => (9, [0; 32]),
        }
    }

    /// The selector a code and member bytes name; `None` for an unknown code, or member bytes on a selector that names
    /// no member (one encoding per selector).
    pub fn from_code(c: u8, member: [u8; 32]) -> Option<Selector> {
        let none = member == [0; 32];
        Some(match c {
            0 if none => Selector::Latest,
            1 if none => Selector::Any,
            2 if none => Selector::All,
            3 if none => Selector::Sum,
            4 if none => Selector::Average,
            5 if none => Selector::Min,
            6 if none => Selector::Max,
            7 => Selector::Member(MemberKey(member)),
            8 if none => Selector::Holder,
            9 if none => Selector::Owner,
            _ => return None,
        })
    }
}

/// A field modes row's key: the component, the field's index in its description (fields in offset order), and the
/// object for an object row (`None`: component-wide).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModeKey {
    pub component: ComponentId,
    pub field: u16,
    pub object: Option<ObjectId>,
}

/// A field modes row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeRow {
    pub mode: Mode,
    pub selector: Selector,
}

/// A feed row's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeedKey {
    pub object: ObjectId,
    pub component: ComponentId,
    pub field: u16,
    pub member: MemberKey,
}

/// A member's last published value of a field, and where in the agreed order it was accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedValue {
    pub value: Vec<u8>,
    pub frame: Frame,
    /// The input's index in its frame's applied order.
    pub index: u32,
}

/// Both tables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tables {
    pub modes: BTreeMap<ModeKey, ModeRow>,
    pub feed: BTreeMap<FeedKey, FeedValue>,
}

impl Tables {
    /// The mode of a field on an object: its object row, else its component-wide row, else synchronized (`None`).
    pub fn mode_of(&self, object: ObjectId, component: ComponentId, field: u16) -> Option<ModeRow> {
        let row = |object| self.modes.get(&ModeKey { component, field, object }).copied();
        row(Some(object)).or_else(|| row(None))
    }

    /// The per-player fields of `component` on `object`, by field index: each object row, and each component-wide
    /// row no object row overrides.
    pub fn fields_of(&self, object: ObjectId, component: ComponentId) -> Vec<(u16, ModeRow)> {
        let lo = ModeKey { component, field: 0, object: None };
        let mut out: BTreeMap<u16, ModeRow> = BTreeMap::new();
        for (k, r) in self.modes.range(lo..) {
            if k.component != component {
                break;
            }
            match k.object {
                None => {
                    out.entry(k.field).or_insert(*r);
                }
                Some(o) if o == object => {
                    out.insert(k.field, *r);
                }
                Some(_) => {}
            }
        }
        out.into_iter().collect()
    }

    /// Whether any object has a per-player field of `component`.
    pub fn any(&self, component: ComponentId) -> bool {
        let lo = ModeKey { component, field: 0, object: None };
        self.modes.range(lo..).next().is_some_and(|(k, _)| k.component == component)
    }

    /// Every member's published value of one field.
    pub fn feed_of(&self, object: ObjectId, component: ComponentId, field: u16) -> Vec<(MemberKey, &FeedValue)> {
        let lo = FeedKey { object, component, field, member: MemberKey([0; 32]) };
        (self.feed.range(lo..))
            .take_while(|(k, _)| k.object == object && k.component == component && k.field == field)
            .map(|(k, v)| (k.member, v))
            .collect()
    }

    /// The object rows of `object`: at most [`MAX_PER_OBJECT`].
    pub fn object_rows(&self, object: ObjectId) -> usize {
        self.modes.keys().filter(|k| k.object == Some(object)).count()
    }

    /// Drops a leaving member's feed rows (`World::leave`).
    pub fn drop_member(&mut self, member: MemberKey) {
        self.feed.retain(|k, _| k.member != member);
    }

    /// Drops an object's rows in both tables (a delete): its object mode rows, returned for its tombstone, and its
    /// feed rows, which an undelete doesn't bring back (they are inputs; the next publish refills them).
    pub fn drop_object(&mut self, object: ObjectId) -> Vec<(ModeKey, ModeRow)> {
        let gone: Vec<_> = self.modes.iter().filter(|(k, _)| k.object == Some(object)).map(|(k, r)| (*k, *r)).collect();
        for (k, _) in &gone {
            self.modes.remove(k);
        }
        self.feed.retain(|k, _| k.object != object);
        gone
    }

    /// The field modes table's entries (6.6): key `component u128 BE ‖ field u16 BE ‖ scope u8 ‖ object u128 BE
    /// (zero for scope 0)`, value `mode u8 ‖ selector u8 ‖ member (32)`. Key order is the map's order.
    pub fn mode_entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.modes.iter().map(|(k, r)| (mode_key_bytes(k), mode_value_bytes(r))).collect()
    }

    /// The feed table's entries (6.6): key `object u128 BE ‖ component u128 BE ‖ field u16 BE ‖ member (32)`, value
    /// `value bytes ‖ frame u64 LE ‖ index u32 LE`.
    pub fn feed_entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.feed
            .iter()
            .map(|(k, v)| {
                let mut key = k.object.to_u128().to_be_bytes().to_vec();
                key.extend_from_slice(&k.component.0.to_be_bytes());
                key.extend_from_slice(&k.field.to_be_bytes());
                key.extend_from_slice(&k.member.0);
                let mut val = v.value.clone();
                val.extend_from_slice(&v.frame.0.to_le_bytes());
                val.extend_from_slice(&v.index.to_le_bytes());
                (key, val)
            })
            .collect()
    }

    /// The field modes table from its entries; `None` if one doesn't decode canonically.
    pub fn modes_from(entries: &[(&[u8], &[u8])]) -> Option<BTreeMap<ModeKey, ModeRow>> {
        let mut m = BTreeMap::new();
        for (k, v) in entries {
            if k.len() != 35 || v.len() != 34 {
                return None;
            }
            let component = ComponentId(u128::from_be_bytes(k[..16].try_into().ok()?));
            let field = u16::from_be_bytes(k[16..18].try_into().ok()?);
            let o = u128::from_be_bytes(k[19..35].try_into().ok()?);
            let object = match k[18] {
                0 if o == 0 => None,
                1 => Some(ObjectId::from_u128(o)),
                _ => return None,
            };
            let row = ModeRow {
                mode: Mode::from_code(v[0])?,
                selector: Selector::from_code(v[1], v[2..34].try_into().ok()?)?,
            };
            let key = ModeKey { component, field, object };
            if mode_key_bytes(&key) != *k || m.insert(key, row).is_some() {
                return None;
            }
        }
        // The cap per object (16), as a live table holds it, so a world accepted here is one whose tombstones (which
        // hold an object's rows) decode again after a delete (@world-model's M3).
        let mut per: BTreeMap<ObjectId, usize> = BTreeMap::new();
        for k in m.keys() {
            if let Some(o) = k.object {
                let n = per.entry(o).or_default();
                *n += 1;
                if *n > MAX_PER_OBJECT {
                    return None;
                }
            }
        }
        Some(m)
    }

    /// The feed table from its entries; `None` if one doesn't decode.
    pub fn feed_from(entries: &[(&[u8], &[u8])]) -> Option<BTreeMap<FeedKey, FeedValue>> {
        let mut m = BTreeMap::new();
        for (k, v) in entries {
            if k.len() != 66 || v.len() < 12 || v.len() - 12 > MAX_VALUE {
                return None;
            }
            let key = FeedKey {
                object: ObjectId::from_u128(u128::from_be_bytes(k[..16].try_into().ok()?)),
                component: ComponentId(u128::from_be_bytes(k[16..32].try_into().ok()?)),
                field: u16::from_be_bytes(k[32..34].try_into().ok()?),
                member: MemberKey(k[34..66].try_into().ok()?),
            };
            let n = v.len() - 12;
            let value = FeedValue {
                value: v[..n].to_vec(),
                frame: Frame(u64::from_le_bytes(v[n..n + 8].try_into().ok()?)),
                index: u32::from_le_bytes(v[n + 8..].try_into().ok()?),
            };
            if m.insert(key, value).is_some() {
                return None;
            }
        }
        Some(m)
    }
}

/// A delete's tombstones take their objects' object-scope mode rows, and the objects' rows leave both tables
/// (world-model.md 7.5; @world-model). A no-op, with no copy of the tables, when none has any.
pub(crate) fn bury(tables: &mut std::sync::Arc<Tables>, stones: &mut [(ObjectId, crate::world::Tombstone)]) {
    let touched =
        |o: ObjectId| tables.modes.keys().any(|k| k.object == Some(o)) || tables.feed.keys().any(|k| k.object == o);
    if !stones.iter().any(|(o, _)| touched(*o)) {
        return;
    }
    let t = std::sync::Arc::make_mut(tables);
    for (o, stone) in stones.iter_mut() {
        stone.modes = t.drop_object(*o);
    }
}

/// An undelete puts an object's mode rows back from its tombstone.
pub(crate) fn unbury(tables: &mut std::sync::Arc<Tables>, modes: &[(ModeKey, ModeRow)]) {
    if modes.is_empty() {
        return;
    }
    let t = std::sync::Arc::make_mut(tables);
    for (k, r) in modes {
        t.modes.insert(*k, *r);
    }
}

/// A tombstone's mode rows in its record (6.6): nothing when it has none; otherwise a count `u32`, then each row's
/// key and value bytes as the field modes table has them.
pub(crate) fn tombstone_modes_bytes(out: &mut Vec<u8>, modes: &[(ModeKey, ModeRow)]) {
    if modes.is_empty() {
        return;
    }
    out.extend_from_slice(&(modes.len() as u32).to_le_bytes());
    for (k, r) in modes {
        out.extend_from_slice(&mode_key_bytes(k));
        out.extend_from_slice(&mode_value_bytes(r));
    }
}

/// The length [`tombstone_modes_bytes`] writes.
pub(crate) fn tombstone_modes_len(modes: &[(ModeKey, ModeRow)]) -> usize {
    if modes.is_empty() { 0 } else { 4 + modes.len() * (35 + 34) }
}

fn mode_key_bytes(k: &ModeKey) -> Vec<u8> {
    let mut b = k.component.0.to_be_bytes().to_vec();
    b.extend_from_slice(&k.field.to_be_bytes());
    match k.object {
        None => {
            b.push(0);
            b.extend_from_slice(&[0; 16]);
        }
        Some(o) => {
            b.push(1);
            b.extend_from_slice(&o.to_u128().to_be_bytes());
        }
    }
    b
}

fn mode_value_bytes(r: &ModeRow) -> Vec<u8> {
    let (code, member) = r.selector.code();
    let mut b = vec![r.mode as u8, code];
    b.extend_from_slice(&member);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables() -> Tables {
        let mut t = Tables::default();
        let c = ComponentId(2000);
        let o = ObjectId::from_u128(5);
        t.modes.insert(
            ModeKey { component: c, field: 1, object: None },
            ModeRow { mode: Mode::Decoupled, selector: Selector::Latest },
        );
        t.modes.insert(
            ModeKey { component: c, field: 1, object: Some(o) },
            ModeRow { mode: Mode::Desynchronized, selector: Selector::Member(MemberKey([3; 32])) },
        );
        t.modes.insert(
            ModeKey { component: ComponentId(2001), field: 0, object: Some(o) },
            ModeRow { mode: Mode::Decoupled, selector: Selector::Any },
        );
        t.feed.insert(
            FeedKey { object: o, component: c, field: 1, member: MemberKey([1; 32]) },
            FeedValue { value: vec![1, 2, 3, 4], frame: Frame(9), index: 2 },
        );
        t
    }

    #[test]
    fn the_tables_round_trip_through_their_entries() {
        let t = tables();
        let m = t.mode_entries();
        let m: Vec<(&[u8], &[u8])> = m.iter().map(|(k, v)| (&k[..], &v[..])).collect();
        assert_eq!(Tables::modes_from(&m).unwrap(), t.modes);
        let f = t.feed_entries();
        let f: Vec<(&[u8], &[u8])> = f.iter().map(|(k, v)| (&k[..], &v[..])).collect();
        assert_eq!(Tables::feed_from(&f).unwrap(), t.feed);
        // The map's order is the bytes' order, so the hashed record is in key order.
        let keys: Vec<_> = t.mode_entries().into_iter().map(|(k, _)| k).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn an_object_row_overrides_the_component_wide_one() {
        let t = tables();
        let (c, o) = (ComponentId(2000), ObjectId::from_u128(5));
        assert_eq!(t.mode_of(o, c, 1).unwrap().mode, Mode::Desynchronized);
        assert_eq!(t.mode_of(ObjectId::from_u128(6), c, 1).unwrap().mode, Mode::Decoupled);
        assert_eq!(t.mode_of(o, c, 0), None);
        assert_eq!(t.fields_of(o, c).len(), 1);
        assert!(t.any(c) && t.any(ComponentId(2001)) && !t.any(ComponentId(1999)));
    }

    #[test]
    fn a_delete_takes_both_tables_rows_and_a_leave_the_members() {
        let mut t = tables();
        let o = ObjectId::from_u128(5);
        let mut u = t.clone();
        u.drop_member(MemberKey([1; 32]));
        assert!(u.feed.is_empty() && u.modes.len() == 3);
        let gone = t.drop_object(o);
        assert_eq!(gone.len(), 2);
        assert!(t.feed.is_empty());
        assert_eq!(t.modes.len(), 1, "the component-wide row stays");
    }

    #[test]
    fn malformed_entries_are_refused() {
        let bad_scope = {
            let mut k = mode_key_bytes(&ModeKey { component: ComponentId(1), field: 0, object: None });
            k[18] = 2;
            k
        };
        let v = mode_value_bytes(&ModeRow { mode: Mode::Decoupled, selector: Selector::Latest });
        assert!(Tables::modes_from(&[(&bad_scope, &v)]).is_none());
        let mut member_on_latest = v.clone();
        member_on_latest[2] = 1;
        let k = mode_key_bytes(&ModeKey { component: ComponentId(1), field: 0, object: None });
        assert!(Tables::modes_from(&[(&k, &member_on_latest)]).is_none());
        assert!(Tables::feed_from(&[(&[0u8; 66], &[0u8; 11])]).is_none());
    }
}
