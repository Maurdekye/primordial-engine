//! Resolved edits to pe-world's edit payload (api.md 9.2 steps 2-4, 11 S1): the `Transaction` the simulation thread
//! submits, with this API's versions as `ObjectVersion` preconditions and each `Copy` carrying the size resolved.
//!
//! Mapped: the preconditions, `delete`, `reparent`, `duplicate`, `set` of plain fields encoded through the schema
//! (numbers, bools, object refs, member keys, entry ids, hashes, and fixed-length lists of them), `add` (a whole row:
//! zeros, then the fields given), `remove` on an existing object's row (by its instance tag), and `create`: a
//! `Name` row (its text a blob, returned for the store) and whole component rows, or, from a template, a `Copy` of
//! it, then the name and per-field sets on the copy. Not yet: blob fields, `Transform.position` (converted op by op
//! against the state at that op, api.md 4.4), `Transform` in an add or create, a `remove` of a row this change
//! adds, and `code`. Those fail the call as `internal`
//! "not yet", so nothing is ever sent wrong.

use std::collections::BTreeMap;

use pe_seams::world_api::{ApiError, ErrorCode, Json, MemberId, ObjectBrief, ObjectId, Refusal, RefusalCode, Version};
use pe_sim::FieldKind;
use pe_world::edit::{Actor, Op, Precondition, Row, Target, Transaction};
use pe_world::{BlobRef, WorldSnapshot};

use crate::resolve::{Obj, ROp, Resolved};
use crate::world::parse_version;

fn target(o: Obj) -> Target {
    match o {
        Obj::Existing(id) => Target::Existing(id.into()),
        Obj::New(k) => Target::New(k),
    }
}

fn refusal(code: RefusalCode, message: impl Into<String>) -> Box<Refusal> {
    Box::new(Refusal {
        code,
        message: message.into(),
        op: None,
        object: None,
        field: None,
        read: None,
        now: None,
        winner: None,
        next: None,
        details: None,
    })
}

/// A precondition's version must be one this API gave out (`v:<entry>.<op>`); anything else is refused.
fn version(v: &Version) -> Result<pe_seams::Version, Box<Refusal>> {
    parse_version(v).ok_or_else(|| {
        refusal(
            RefusalCode::Invalid,
            format!("{:?} is not a version this world gave out; read the object again, or use \"any\".", v.0),
        )
    })
}

fn not_yet(i: usize, what: &str) -> Unmapped {
    Unmapped::Failed(ApiError::new(ErrorCode::Internal, format!("Op {i}: {what} isn't wired to the world yet.")))
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let mut out = [0u8; 32];
    for (k, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * k..2 * k + 2], 16).ok()?;
    }
    Some(out)
}

/// One element of a field, little-endian as pe-world stores it; `Err` names what was expected.
fn encode_one(kind: FieldKind, v: &Json) -> Result<Vec<u8>, &'static str> {
    let int = |lo: i128, hi: i128| -> Result<i128, &'static str> {
        let n = v.as_i64().map(i128::from).or_else(|| v.as_u64().map(i128::from)).ok_or("an integer")?;
        if n < lo || n > hi { Err("an integer in the field's range") } else { Ok(n) }
    };
    let float = || v.as_f64().filter(|x| x.is_finite()).ok_or("a finite number");
    Ok(match kind {
        FieldKind::U8 => vec![int(0, u8::MAX as i128)? as u8],
        FieldKind::U16 => (int(0, u16::MAX as i128)? as u16).to_le_bytes().to_vec(),
        FieldKind::U32 => (int(0, u32::MAX as i128)? as u32).to_le_bytes().to_vec(),
        FieldKind::U64 => (int(0, u64::MAX as i128)? as u64).to_le_bytes().to_vec(),
        FieldKind::I8 => vec![int(i8::MIN as i128, i8::MAX as i128)? as i8 as u8],
        FieldKind::I16 => (int(i16::MIN as i128, i16::MAX as i128)? as i16).to_le_bytes().to_vec(),
        FieldKind::I32 => (int(i32::MIN as i128, i32::MAX as i128)? as i32).to_le_bytes().to_vec(),
        FieldKind::I64 => (int(i64::MIN as i128, i64::MAX as i128)? as i64).to_le_bytes().to_vec(),
        FieldKind::F32 => {
            let x = float()? as f32;
            if !x.is_finite() {
                return Err("a number within f32's range");
            }
            x.to_le_bytes().to_vec()
        }
        FieldKind::F64 => float()?.to_le_bytes().to_vec(),
        FieldKind::Bool => vec![v.as_bool().ok_or("true or false")? as u8],
        FieldKind::ObjectRef => {
            let id: pe_core::ObjectId = match v {
                Json::Null => pe_core::ObjectId::from_u128(0),
                Json::String(t) => ObjectId::parse(t).ok_or("an object id (o:...) or null")?.into(),
                _ => return Err("an object id (o:...) or null"),
            };
            // pe-world reads the field with bytemuck, so write the id's own byte layout.
            bytemuck::bytes_of(&id).to_vec()
        }
        FieldKind::MemberKey => {
            let t = v.as_str().ok_or("a member id (m:...)")?;
            let m = MemberId::parse(t).ok_or("a member id (m:...)")?;
            m.0.to_vec()
        }
        FieldKind::EntryRef => {
            let t = v.as_str().ok_or("an entry id (e:...)")?;
            pe_seams::world_api::EntryId::parse(t).ok_or("an entry id (e:...)")?.0.to_vec()
        }
        FieldKind::Hash => hex32(v.as_str().ok_or("64 hex digits")?).ok_or("64 hex digits")?.to_vec(),
        FieldKind::BlobRef | FieldKind::SimBlobRef | FieldKind::Pos => return Err("not yet"),
    })
}

/// `Component.field` = `v` as pe-world's `SetField` on `id`.
fn set_field(snap: &WorldSnapshot, i: usize, id: Target, key: &str, v: &Json) -> Result<Op, Unmapped> {
    let bad = |message: String| {
        let mut f = refusal(RefusalCode::Invalid, message);
        f.op = Some(i as u32);
        f.field = Some(key.to_string());
        if let Target::Existing(o) = id {
            f.object = snap.info(o).map(|x| ObjectBrief { id: o.into(), name: x.name.unwrap_or_default() });
        }
        Unmapped::Refused(f)
    };
    let (comp, field) = key.split_once('.').ok_or_else(|| bad(format!("{key:?} isn't Component.field.")))?;
    if comp == "Transform" && (field == "position" || field == "pos" || field == "chunk") {
        return Err(not_yet(i, "setting Transform.position"));
    }
    let desc = snap.component(comp).ok_or_else(|| bad(format!("There is no component {comp:?}.")))?;
    let (index, f) = desc
        .fields()
        .iter()
        .enumerate()
        .find(|(_, f)| f.name == field && !f.name.starts_with('_'))
        .ok_or_else(|| bad(format!("{comp} has no field {field:?}.")))?;
    let items: Vec<&Json> = match (f.count, v) {
        (1, v) => vec![v],
        (n, Json::Array(a)) if a.len() == n as usize => a.iter().collect(),
        (n, _) => return Err(bad(format!("{key} takes a list of {n} values."))),
    };
    let mut bytes = Vec::with_capacity(f.kind.size() as usize * items.len());
    for it in items {
        match encode_one(f.kind, it) {
            Ok(b) => bytes.extend(b),
            Err("not yet") => return Err(not_yet(i, &format!("setting {key} (a blob or position field)"))),
            Err(want) => return Err(bad(format!("{key} takes {want}."))),
        }
    }
    Ok(Op::SetField { id, component: desc.id(), instance: 0, field: index as u16, value: bytes })
}

/// A whole row of `component` for `t`: zeros, then each given field encoded at its offset.
fn whole_row(
    snap: &WorldSnapshot,
    i: usize,
    t: Target,
    component: &str,
    fields: &BTreeMap<String, Json>,
) -> Result<Row, Unmapped> {
    if component == "Transform" {
        return Err(not_yet(i, "a Transform row in an add or create"));
    }
    let desc = snap.component(component).ok_or_else(|| {
        let mut f = refusal(RefusalCode::Invalid, format!("There is no component {component:?}."));
        f.op = Some(i as u32);
        Unmapped::Refused(f)
    })?;
    let mut bytes = vec![0u8; desc.size()];
    for (k, v) in fields {
        if let Op::SetField { field, value, .. } = set_field(snap, i, t, &format!("{component}.{k}"), v)? {
            let at = desc.fields()[field as usize].offset as usize;
            bytes[at..at + value.len()].copy_from_slice(&value);
        }
    }
    Ok(Row { component: desc.id(), instance: 0, bytes })
}

/// The `Name` row for `text`, and the blob it refers to.
fn name_row(snap: &WorldSnapshot, i: usize, text: &str) -> Result<(Row, BlobRef), Unmapped> {
    let desc = snap.component("Name").ok_or_else(|| not_yet(i, "naming (the world has no Name component)"))?;
    let f = desc.fields().iter().find(|f| f.name == "text").ok_or_else(|| not_yet(i, "naming"))?;
    let r = BlobRef::of(text.as_bytes());
    let mut bytes = vec![0u8; desc.size()];
    let at = f.offset as usize;
    bytes[at..at + 32].copy_from_slice(&r.hash);
    bytes[at + 32..at + 40].copy_from_slice(&r.len.to_le_bytes());
    Ok((Row { component: desc.id(), instance: 0, bytes }, r))
}

/// pe-world's payload for one change, and the blob bytes it refers to, which the caller puts in the blob store
/// before the entry is submitted.
#[derive(Debug)]
pub struct WorldEdit {
    pub tx: Transaction,
    pub blobs: Vec<Vec<u8>>,
    /// For each of `tx.ops`, the index of the API transaction's op it came from, to name it in a refusal.
    pub op_of: Vec<u32>,
}

/// Why a resolved change has no pe-world payload: the change is refused, or the call can't serve it yet.
#[derive(Debug)]
pub enum Unmapped {
    Refused(Box<Refusal>),
    Failed(ApiError),
}

/// pe-world's payload for a resolved change, from `actor`, with values encoded through `snap`'s schema.
pub fn to_world(r: &Resolved, actor: Actor, snap: &WorldSnapshot) -> Result<WorldEdit, Unmapped> {
    let mut blob_refs: Vec<BlobRef> = vec![];
    let mut blobs: Vec<Vec<u8>> = vec![];
    let mut preconditions = Vec::with_capacity(r.preconditions.len());
    for (id, v) in &r.preconditions {
        preconditions
            .push(Precondition::ObjectVersion { id: (*id).into(), is: version(v).map_err(Unmapped::Refused)? });
    }
    let mut ops = Vec::with_capacity(r.ops.len());
    let mut op_of: Vec<u32> = Vec::with_capacity(r.ops.len());
    let mut prev = 0u32;
    for (i, op) in r.ops.iter().enumerate() {
        // The ops the previous resolved op pushed came from its source op (done here, so `continue` arms count too).
        op_of.resize(ops.len(), prev);
        prev = r.op_of.get(i).copied().unwrap_or(i as u32);
        ops.push(match op {
            ROp::Delete { target: t } => Op::Delete { id: target(*t) },
            ROp::Reparent { target: t, parent, index, keep_world } => {
                Op::Reparent { id: target(*t), parent: target(*parent), index: *index, keep_world: *keep_world }
            }
            ROp::Copy { source, new, parent, size } => {
                Op::Copy { source: target(*source), new: *new, parent: target(*parent), index: None, size: *size }
            }
            ROp::Set { target: t, fields } => {
                // One SetField per field, in field-name order (the map's), all on the same object.
                for (k, v) in fields {
                    ops.push(set_field(snap, i, target(*t), k, v)?);
                }
                continue;
            }
            ROp::Create { new, size, parent, name, template, components } => {
                let (name_row, r) = name_row(snap, i, name)?;
                if !blob_refs.contains(&r) {
                    blob_refs.push(r);
                    blobs.push(name.as_bytes().to_vec());
                }
                let me = Target::New(*new);
                match template {
                    None => {
                        let mut rows = vec![name_row];
                        for (c, fields) in components {
                            rows.push(whole_row(snap, i, me, c, fields)?);
                        }
                        ops.push(Op::Create { new: *new, parent: target(*parent), index: None, rows });
                    }
                    Some(tpl) => {
                        // The copy brings the template's rows; the name replaces its Name row, and the components
                        // given are field writes on the copy.
                        let source = Target::Existing((*tpl).into());
                        ops.push(Op::Copy { source, new: *new, parent: target(*parent), index: None, size: *size });
                        ops.push(Op::SetRow { id: me, row: name_row });
                        for (c, fields) in components {
                            for (k, v) in fields {
                                ops.push(set_field(snap, i, me, &format!("{c}.{k}"), v)?);
                            }
                        }
                    }
                }
                continue;
            }
            ROp::Add { target: t, component, fields } => {
                Op::SetRow { id: target(*t), row: whole_row(snap, i, target(*t), component, fields)? }
            }
            ROp::Remove { target: t, component } => {
                let Obj::Existing(o) = t else { return Err(not_yet(i, "removing a row this change adds")) };
                let desc = snap.component(component);
                let tag = snap.row_tag((*o).into(), component);
                match (desc, tag) {
                    (Some(d), Some(tag)) => Op::RemoveRow { id: target(*t), component: d.id(), instance: 0, tag },
                    _ => {
                        let mut f =
                            refusal(RefusalCode::NotFound, format!("The object has no {component:?} component."));
                        f.op = Some(i as u32);
                        f.object =
                            snap.info((*o).into()).map(|x| ObjectBrief { id: *o, name: x.name.unwrap_or_default() });
                        return Err(Unmapped::Refused(f));
                    }
                }
            }
            ROp::Code { .. } => return Err(not_yet(i, "code")),
        });
    }
    op_of.resize(ops.len(), prev);
    let tx = Transaction { actor, label: Some(r.label.clone()), preconditions, ops, blobs: blob_refs };
    Ok(WorldEdit { tx, blobs, op_of })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::resolve_tx;
    use crate::world::{WorldView, version_text};
    use pe_core::{EntryRef, Frame, Hash, MemberKey, Offset, Pos};
    use pe_seams::world_api::Transaction as ApiTx;
    use pe_sim::Transform;
    use pe_world::components::{Body, Name, body_kind};
    use pe_world::{BlobStore, World};
    use serde_json::json;
    use std::sync::Arc;

    const ALICE: MemberKey = MemberKey([1; 32]);

    fn id(n: u128) -> pe_core::ObjectId {
        pe_core::ObjectId::from_u128(n)
    }
    fn ver(b: u8, op: u32) -> pe_seams::Version {
        pe_seams::Version { entry: EntryRef(Hash([b; 32])), op }
    }
    fn tx(ops: Json) -> ApiTx {
        serde_json::from_value(json!({ "label": "edit things", "ops": ops })).unwrap()
    }

    /// A Table (root) with a Lamp and a Ball under it; the Ball has a Body. Plus a Shelf root.
    fn world() -> (Arc<WorldSnapshot>, WorldView) {
        let mut w = World::new(7, BlobStore::default());
        for (n, name, parent) in
            [(10, "Table", None), (20, "Lamp", Some(10)), (30, "Ball", Some(10)), (40, "Shelf", None)]
        {
            let text = w.blobs().put(name.as_bytes());
            w.create(id(n), parent.map(id), ALICE, ver(n as u8, 0)).unwrap();
            let t = match parent {
                None => Transform::at(Pos::ORIGIN.checked_add(Offset([n as f64, 0.0, 0.0])).unwrap()),
                Some(_) => Transform::local([0.0, 1.0, 0.0]),
            };
            w.insert(id(n), t).unwrap();
            w.insert(id(n), Name { text }).unwrap();
        }
        w.insert(id(30), Body { kind: body_kind::DYNAMIC, friction: 0.5, grabbable: 1, ..Default::default() }).unwrap();
        w.set_frame(Frame(9));
        let s = w.snapshot(Frame(9));
        (s.clone(), WorldView::new(s, None))
    }

    fn resolved(v: &WorldView, ops: Json) -> Resolved {
        resolve_tx(v, &tx(ops), &|_| None).map_err(|e| e.message).unwrap()
    }

    #[test]
    fn structural_ops_and_preconditions_map_to_pe_world() {
        let (s, v) = world();
        let r = resolved(
            &v,
            json!([
                {"op": "duplicate", "id": "/Table", "parent": "/Shelf", "as": "$t"},
                {"op": "reparent", "id": "/Shelf", "parent": "/Table/Lamp"},
                {"op": "delete", "id": "/Table/Ball"},
            ]),
        );
        let w = to_world(&r, Actor::Person, &s).map_err(|e| format!("{e:?}")).unwrap().tx;
        assert_eq!(w.label.as_deref(), Some("edit things"));
        let e = |n: u128| Target::Existing(id(n));
        assert_eq!(
            w.ops,
            [
                Op::Copy { source: e(10), new: 0, parent: e(40), index: None, size: 3 },
                Op::Reparent { id: e(40), parent: e(20), index: None, keep_world: true },
                Op::Delete { id: e(30) },
            ]
        );
        // The Table (duplicated), the Shelf (reparented) and the Ball (deleted), each at its base version, exactly
        // as pe-world has it; a parent gets none.
        assert_eq!(
            w.preconditions,
            [
                Precondition::ObjectVersion { id: id(10), is: ver(10, 0) },
                Precondition::ObjectVersion { id: id(40), is: ver(40, 0) },
                Precondition::ObjectVersion { id: id(30), is: ver(30, 0) },
            ]
        );
        let _ = version_text(&ver(1, 0));
    }

    #[test]
    fn set_encodes_fields_through_the_schema() {
        let (s, v) = world();
        let r = resolved(
            &v,
            json!([{"op": "set", "id": "/Table/Ball", "fields": {"Body.friction": 0.25, "Body.grabbable": false}}]),
        );
        let w = to_world(&r, Actor::Person, &s).map_err(|e| format!("{e:?}")).unwrap().tx;
        let desc = s.component("Body").unwrap();
        let idx = |n: &str| desc.fields().iter().position(|f| f.name == n).unwrap() as u16;
        let fr = &desc.fields()[idx("friction") as usize];
        let want = match fr.kind {
            FieldKind::F32 => 0.25f32.to_le_bytes().to_vec(),
            FieldKind::F64 => 0.25f64.to_le_bytes().to_vec(),
            k => panic!("{k:?}"),
        };
        assert_eq!(w.ops.len(), 2);
        assert_eq!(
            w.ops[0],
            Op::SetField {
                id: Target::Existing(id(30)),
                component: desc.id(),
                instance: 0,
                field: idx("friction"),
                value: want
            }
        );
        assert!(
            matches!(&w.ops[1], Op::SetField { field, value, .. } if *field == idx("grabbable") && value.iter().all(|b| *b == 0))
        );
    }

    #[test]
    fn create_plain_and_from_a_template() {
        let (s, v) = world();
        let r = resolved(
            &v,
            json!([{"op": "create", "as": "$cup", "parent": "/Table", "name": "Cup", "components": {"Body": {"friction": 0.25}}}]),
        );
        let e = to_world(&r, Actor::Person, &s).map_err(|e| format!("{e:?}")).unwrap();
        let cup = BlobRef::of(b"Cup");
        assert_eq!(e.tx.blobs, [cup]);
        assert_eq!(e.blobs, [b"Cup".to_vec()]);
        let Op::Create { new: 0, parent, index: None, rows } = &e.tx.ops[0] else { panic!("{:?}", e.tx.ops) };
        assert_eq!(*parent, Target::Existing(id(10)));
        assert_eq!(rows[0].component, s.component("Name").unwrap().id());
        assert_eq!(&rows[0].bytes[..32], cup.hash);
        assert_eq!(rows[1].component, s.component("Body").unwrap().id());
        // From a template under /Templates.
        let mut w = World::new(7, BlobStore::default());
        for (n, name, parent) in [(1u128, "Templates", None), (2, "box", Some(1)), (3, "Floor", None)] {
            let text = w.blobs().put(name.as_bytes());
            w.create(id(n), parent.map(id), ALICE, ver(n as u8, 0)).unwrap();
            w.insert(id(n), Name { text }).unwrap();
        }
        w.insert(id(2), Body { kind: body_kind::DYNAMIC, friction: 0.5, grabbable: 1, ..Default::default() }).unwrap();
        let s2 = w.snapshot(Frame(1));
        let v2 = WorldView::new(s2.clone(), None);
        let r = resolved(
            &v2,
            json!([{"op": "create", "parent": "/Floor", "name": "Red box", "template": "box", "components": {"Body": {"friction": 0.9}}}]),
        );
        let e = to_world(&r, Actor::Person, &s2).map_err(|e| format!("{e:?}")).unwrap();
        let src = Target::Existing(id(2));
        assert_eq!(
            e.tx.ops[0],
            Op::Copy { source: src, new: 0, parent: Target::Existing(id(3)), index: None, size: 1 }
        );
        let name_id = s2.component("Name").unwrap().id();
        assert!(matches!(&e.tx.ops[1], Op::SetRow { id: Target::New(0), row } if row.component == name_id));
        assert!(matches!(&e.tx.ops[2], Op::SetField { id: Target::New(0), .. }));
        assert_eq!(e.blobs, [b"Red box".to_vec()]);
    }

    #[test]
    fn add_and_remove_rows() {
        let (s, v) = world();
        let r = resolved(
            &v,
            json!([
                {"op": "add", "id": "/Table/Lamp", "component": "Body", "fields": {"friction": 0.75, "grabbable": true}},
                {"op": "remove", "id": "/Table/Ball", "component": "Body"},
            ]),
        );
        let w = to_world(&r, Actor::Person, &s).map_err(|e| format!("{e:?}")).unwrap().tx;
        let desc = s.component("Body").unwrap();
        let Op::SetRow { id: t, row } = &w.ops[0] else { panic!("{:?}", w.ops[0]) };
        assert_eq!(
            (*t, row.component, row.instance, row.bytes.len()),
            (Target::Existing(id(20)), desc.id(), 0, desc.size())
        );
        let off = |n: &str| desc.fields().iter().find(|f| f.name == n).unwrap().offset as usize;
        assert_eq!(&row.bytes[off("friction")..off("friction") + 4], 0.75f32.to_le_bytes());
        assert_eq!(row.bytes[off("grabbable")], 1);
        let tag = s.row_tag(id(30), "Body").unwrap();
        assert_eq!(w.ops[1], Op::RemoveRow { id: Target::Existing(id(30)), component: desc.id(), instance: 0, tag });
        // A component the object lacks is not_found.
        let r = resolved(&v, json!([{"op": "remove", "id": "/Table/Lamp", "component": "Body"}]));
        assert!(
            matches!(to_world(&r, Actor::Person, &s), Err(Unmapped::Refused(f)) if f.code == RefusalCode::NotFound)
        );
    }

    #[test]
    fn object_refs_and_keys_encode_as_pe_world_reads_them() {
        let o = id(0x1234_5678_9abc_def0_1122_3344_5566_7788);
        let b = encode_one(FieldKind::ObjectRef, &json!(ObjectId::from(o).to_string())).unwrap();
        assert_eq!(bytemuck::pod_read_unaligned::<pe_core::ObjectId>(&b), o);
        let none = encode_one(FieldKind::ObjectRef, &Json::Null).unwrap();
        assert!(bytemuck::pod_read_unaligned::<pe_core::ObjectId>(&none).is_none());
        let m = encode_one(FieldKind::MemberKey, &json!(MemberId::from(ALICE).to_string())).unwrap();
        assert_eq!(m, ALICE.0);
        assert_eq!(encode_one(FieldKind::U16, &json!(70_000)), Err("an integer in the field's range"));
        assert_eq!(encode_one(FieldKind::F32, &json!(1e300)), Err("a number within f32's range"));
        assert_eq!(encode_one(FieldKind::I8, &json!(-128)).unwrap(), [0x80]);
    }

    #[test]
    fn bad_values_are_refused_and_unwired_ops_fail() {
        let (s, v) = world();
        let refused = |ops: Json, field: &str| {
            let r = resolved(&v, ops);
            match to_world(&r, Actor::Person, &s) {
                Err(Unmapped::Refused(f)) => {
                    assert_eq!(
                        (f.code, f.op, f.field.as_deref()),
                        (RefusalCode::Invalid, Some(0), Some(field)),
                        "{}",
                        f.message
                    )
                }
                other => panic!("{other:?}"),
            }
        };
        refused(
            json!([{"op": "set", "id": "/Table/Ball", "field": "Body.friction", "value": "fast"}]),
            "Body.friction",
        );
        refused(json!([{"op": "set", "id": "/Table/Ball", "field": "Body.nope", "value": 1}]), "Body.nope");
        refused(json!([{"op": "set", "id": "/Table/Ball", "field": "Nope.x", "value": 1}]), "Nope.x");
        refused(json!([{"op": "set", "id": "/Table/Ball", "field": "Body.grabbable", "value": 300}]), "Body.grabbable");
        let failed = |ops: Json| {
            let r = resolved(&v, ops);
            assert!(
                matches!(to_world(&r, Actor::Person, &s), Err(Unmapped::Failed(e)) if e.code == ErrorCode::Internal)
            );
        };
        failed(
            json!([{"op": "create", "parent": "/Table", "name": "x", "components": {"Transform": {"scale": [1, 1, 1]}}}]),
        );
        failed(
            json!([{"op": "set", "id": "/Table/Ball", "field": "Transform.position", "value": {"relative_to": "parent", "offset": [0, 2, 0]}}]),
        );
        // A version this API didn't give out.
        let m = crate::query::mem::Mem::pit(0);
        let r = resolve_tx(&m, &tx(json!([{"op": "delete", "id": "/Table"}])), &|_| None).unwrap();
        assert!(matches!(to_world(&r, Actor::Person, &s), Err(Unmapped::Refused(f)) if f.code == RefusalCode::Invalid));
    }
}
