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
use std::sync::Arc;

use pe_core::{Offset, Pos};
use pe_seams::world_api::{
    ApiError, ErrorCode, FieldDiff, Json, MemberId, ObjRef, ObjectBrief, ObjectId, Refusal, RefusalCode, Version,
};
use pe_sim::FieldKind;
use pe_world::edit::{Actor, AssetTarget, Op, Precondition, Row, Target, Transaction};
use pe_world::{BlobRef, WorldSnapshot};

use crate::names::Names;
use crate::resolve::{Obj, ROp, Resolved, Script, asset_text};
use crate::world::{WorldView, parse_version};

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
/// The component names on one target object: the frame's names and the scripts attached to it, those an earlier op of
/// this change attaches or detaches included (`crate::names`; ruling).
struct On<'a> {
    names: &'a Names,
    attached: Vec<pe_world::script::AssetId>,
}

impl On<'_> {
    /// The component `name` names on the object, or the refusal's text.
    fn desc<'s>(&self, snap: &'s WorldSnapshot, name: &str) -> Result<&'s pe_sim::ComponentDesc, String> {
        let c = self.names.resolve_on(&self.attached, name).map_err(|e| e.message(name))?;
        snap.component_desc(c).ok_or_else(|| format!("There is no component {name:?}."))
    }

    /// Whether `key` is the engine's `Name.text`, which takes the name as a string (api.md 5.3).
    fn is_name_text(&self, snap: &WorldSnapshot, key: &str) -> bool {
        key.split_once('.').is_some_and(|(comp, field)| field == "text" && self.is_name(snap, comp))
    }

    /// Whether component `comp` resolves to the engine's `Name`.
    fn is_name(&self, snap: &WorldSnapshot, comp: &str) -> bool {
        let name = snap.component("Name").map(|d| d.id());
        self.desc(snap, comp).is_ok_and(|d| Some(d.id()) == name)
    }

    /// `key` (`Component.field`) with the engine's Transform named `Transform`, if its component resolves to it.
    fn is_transform(&self, snap: &WorldSnapshot, key: &str) -> Option<String> {
        let (comp, field) = key.split_once('.')?;
        let d = self.desc(snap, comp).ok()?;
        (d.id() == pe_sim::ComponentId::TRANSFORM).then(|| format!("Transform.{field}"))
    }
}

/// Whether `name` is written as the engine's Transform (bare or qualified): for [`Dirty`], which only needs to know
/// that a placement may change (a bare name a script also declares is refused when it maps).
fn transform_name(name: &str) -> bool {
    name == "Transform" || name == "Transform@engine"
}

fn set_field(snap: &WorldSnapshot, on: &On, i: usize, id: Target, key: &str, v: &Json) -> Result<Op, Unmapped> {
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
    let desc = on.desc(snap, comp).map_err(bad)?;
    let transform = desc.id() == pe_sim::ComponentId::TRANSFORM;
    // Rotations are written as the stored quaternion, from either form (api.md 4.4).
    if transform && (field == "rotation_deg" || field == "rotation_quat") {
        let n: Option<Vec<f64>> = v.as_array().and_then(|a| a.iter().map(Json::as_f64).collect());
        let q = match (field, n.as_deref()) {
            ("rotation_deg", Some(&[x, y, z])) if [x, y, z].iter().all(|f| f.is_finite()) => {
                crate::world::quat_from_euler_deg([x, y, z])
            }
            ("rotation_quat", Some(&[x, y, z, w])) => {
                let l = (x * x + y * y + z * z + w * w).sqrt();
                if !(l.is_finite() && l > 1e-6) {
                    return Err(bad(format!("{key} must not be zero.")));
                }
                [x / l, y / l, z / l, w / l].map(|c| c as f32)
            }
            ("rotation_deg", _) => return Err(bad(format!("{key} takes [x, y, z] in degrees."))),
            _ => return Err(bad(format!("{key} takes [x, y, z, w]."))),
        };
        return set_field(snap, on, i, id, "Transform@engine.rot", &serde_json::json!(q));
    }
    if transform && (field == "pos" || field == "chunk") {
        return Err(bad("Set Transform.position, in a named frame, rather than the stored pos and chunk.".into()));
    }
    let (index, f) = desc
        .fields()
        .iter()
        .enumerate()
        .find(|(_, f)| f.name == field && !f.name.starts_with('_'))
        .ok_or_else(|| bad(format!("{comp} has no field {field:?}.")))?;
    // A blob field (an asset's content: a mesh, a material) takes only `null`, for none, its default: what a query
    // shows for an unset one, so its output can be given back as it is (api.md 5.3; the M1 recordings' `create`s).
    if matches!(f.kind, FieldKind::BlobRef | FieldKind::SimBlobRef) {
        if v.is_null() {
            let value = vec![0u8; f.kind.size() as usize * f.count as usize];
            return Ok(Op::SetField { id, component: desc.id(), instance: 0, field: index as u16, value });
        }
        return Err(bad(format!(
            "{key} is a blob (an asset's content), which the API can't set yet; leave it out, or give null for none."
        )));
    }
    // A colour field takes a colour, never a bare list (api.md 4.4; ruling).
    if f.color.is_some() {
        let c = crate::colour::encode(v, f.count as usize).map_err(|why| bad(format!("{key}: {why}")))?;
        let value = c.iter().flat_map(|x| x.to_le_bytes()).collect();
        return Ok(Op::SetField { id, component: desc.id(), instance: 0, field: index as u16, value });
    }
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

/// What earlier ops of the same change do to placements, so a position is never converted against a frame they
/// change (@chunks-reviewer's Medium on 6f508e18). Positions convert against the base snapshot (api.md 4.4), so a
/// write whose frame, or `relative_to` object, an earlier op moves or re-frames is `not_yet` rather than wrong.
#[derive(Default)]
struct Dirty {
    /// Objects whose Transform an earlier op writes, adds or removes, or which it re-parents.
    moved: std::collections::HashSet<(bool, u128)>,
    /// Objects whose frame an earlier op changes: re-parented, or gaining or losing a Transform.
    reframed: std::collections::HashSet<(bool, u128)>,
}

/// Attaches and detaches by target ([`tkey`]), in op order.
/// Rows a change has added (`true`) or removed so far, by target key and component (api.md 5.3's `add` check).
type RowsNow = std::collections::HashMap<((bool, u128), pe_sim::ComponentId), bool>;
type Tagged = std::collections::HashMap<(bool, u128), Vec<([u8; 32], bool)>>;

fn tkey(t: Target) -> Option<(bool, u128)> {
    match t {
        Target::Existing(o) => Some((false, o.to_u128())),
        Target::New(k) => Some((true, u128::from(k))),
        Target::Root => None,
    }
}

impl Dirty {
    /// `o` or any of its ancestors in the base snapshot is moved earlier in the change.
    fn chain_moved(&self, snap: &WorldSnapshot, o: pe_core::ObjectId) -> bool {
        std::iter::successors(Some(o), |x| snap.parent(*x)).any(|a| self.moved.contains(&(false, a.to_u128())))
    }

    /// `o` or any of its ancestors in the base snapshot is re-framed earlier in the change.
    fn chain_reframed(&self, snap: &WorldSnapshot, o: pe_core::ObjectId) -> bool {
        std::iter::successors(Some(o), |x| snap.parent(*x)).any(|a| self.reframed.contains(&(false, a.to_u128())))
    }

    fn note(&mut self, op: &ROp) {
        let mut mark = |t: &Obj, reframe: bool| {
            if let Some(k) = tkey(target(*t)) {
                self.moved.insert(k);
                if reframe {
                    self.reframed.insert(k);
                }
            }
        };
        match op {
            ROp::Reparent { target: t, .. } => mark(t, true),
            ROp::Set { target: t, fields }
                if fields.keys().any(|k| k.split_once('.').is_some_and(|(c, _)| transform_name(c))) =>
            {
                mark(t, false)
            }
            ROp::Add { target: t, component, .. } | ROp::Remove { target: t, component }
                if transform_name(component) =>
            {
                mark(t, true)
            }
            _ => {}
        }
    }
}

/// The nearest ancestor of `id` with a `Transform`: its frame (pe-sim), or `None` for a root placement.
fn frame_of(snap: &WorldSnapshot, id: pe_core::ObjectId) -> Option<pe_core::ObjectId> {
    let mut at = snap.parent(id);
    while let Some(p) = at {
        if snap.components_on(p).contains(&pe_sim::ComponentId::TRANSFORM) {
            return Some(p);
        }
        at = snap.parent(p);
    }
    None
}

/// Where an object sits for a position write: its frame (`None`: a root placement) and its parent in the base
/// snapshot, where the walk for earlier ops starts. A new object's parent is its create's or copy's (@chunks-reviewer's
/// Low on c783ed96: walking from the frame misses a Transform-less parent in between).
#[derive(Clone, Copy)]
struct Place {
    frame: Option<pe_core::ObjectId>,
    parent: Option<pe_core::ObjectId>,
}

/// Three finite numbers.
fn num3(j: Option<&Json>) -> Option<[f64; 3]> {
    let a = j?.as_array()?;
    let v: Vec<f64> = a.iter().map(Json::as_f64).collect::<Option<_>>()?;
    <[f64; 3]>::try_from(v).ok().filter(|x| x.iter().all(|f| f.is_finite()))
}

/// `Transform.position` = `v` on `id` (api.md 4.4), as pe-world field writes carrying the frame-relative value: a
/// child's `pos` in its frame, or a root's exact `chunk` and `pos`. `place` is `id`'s frame and parent. The
/// value is an exact `{"pos": {"chunk": ["i", "j", "k"], "offset": [x, y, z]}}`, or `{"relative_to": f, "offset":
/// [x, y, z]}` with `f` one of `"parent"` (the default; refused for a root), an object or `"me"` (in its space:
/// rotation and scale applied, the inverse of what a query reports), or `"world_origin"`. Converted here, on the
/// author's machine, with pe-sim's own `compose` and `local_in`.
fn set_position(
    view: &WorldView,
    snap: &WorldSnapshot,
    i: usize,
    id: Target,
    place: Place,
    v: &Json,
    dirty: &Dirty,
) -> Result<Vec<Op>, Unmapped> {
    use pe_sim::transform::{POS_LIMIT, ROOT_CHUNK_LIMIT};
    let frame = place.frame;
    use pe_sim::{Transform, WorldTransform};
    let bad = |message: String| {
        let mut f = refusal(RefusalCode::Invalid, message);
        f.op = Some(i as u32);
        f.field = Some("Transform.position".into());
        if let Target::Existing(o) = id {
            f.object = snap.info(o).map(|x| ObjectBrief { id: o.into(), name: x.name.unwrap_or_default() });
        }
        Unmapped::Refused(f)
    };
    let range = || bad("That position is outside the world's range.".into());
    if let Target::Existing(o) = id
        && !snap.components_on(o).contains(&pe_sim::ComponentId::TRANSFORM)
    {
        let name = snap.name(o).unwrap_or_else(|| "This object".into());
        return Err(bad(format!("{name} has no Transform; add one first.")));
    }
    let desc = snap.component("Transform").ok_or_else(|| bad("The world has no Transform component.".into()))?;
    let index = |n: &str| desc.fields().iter().position(|f| f.name == n).map(|x| x as u16);
    let (Some(chunk_i), Some(pos_i)) = (index("chunk"), index("pos")) else {
        return Err(bad("This world's Transform has no chunk and pos.".into()));
    };
    let unit = |pos: [f64; 3]| Transform { chunk: [0; 3], pos, rot: [0.0, 0.0, 0.0, 1.0], scale: [1.0; 3], _pad: 0 };
    enum Want {
        /// An offset in `id`'s own frame, as stored.
        Local([f64; 3]),
        World(Pos),
    }
    let want = if let Some(p) = v.get("pos") {
        let chunk: Option<Vec<i64>> = p
            .get("chunk")
            .and_then(Json::as_array)
            .and_then(|a| a.iter().map(|c| c.as_str().and_then(|s| s.parse().ok()).or_else(|| c.as_i64())).collect());
        let (Some(chunk), Some(offset)) = (chunk.and_then(|c| <[i64; 3]>::try_from(c).ok()), num3(p.get("offset")))
        else {
            return Err(bad(
                "An exact pos is {\"chunk\": [\"i\", \"j\", \"k\"], \"offset\": [x, y, z]} (chunk indices as strings)."
                    .into(),
            ));
        };
        Want::World(Pos::at_chunk(chunk).and_then(|c| c.checked_add(Offset(offset))).map_err(|_| range())?)
    } else {
        let Some(off) = num3(v.get("offset")) else {
            return Err(bad(
                "A position is {\"relative_to\": frame, \"offset\": [x, y, z]} or an exact {\"pos\": ...}.".into(),
            ));
        };
        match v.get("relative_to").and_then(Json::as_str).unwrap_or("parent") {
            "parent" => match frame {
                Some(_) => Want::Local(off),
                None => {
                    let name = match id {
                        Target::Existing(o) => snap.name(o).unwrap_or_else(|| "This object".into()),
                        _ => "The new object".into(),
                    };
                    return Err(bad(format!("{name} has no parent; name a frame.")));
                }
            },
            "world_origin" => Want::World(Pos::ORIGIN.checked_add(Offset(off)).map_err(|_| range())?),
            r if r.starts_with('$') => {
                return Err(not_yet(i, "a position relative to an object made in this change"));
            }
            r => {
                let Some(Some(f)) = view.frame_of(&ObjRef(r.into())) else {
                    return Err(bad(format!("relative_to {r:?} names no single object.")));
                };
                if dirty.chain_moved(snap, f) {
                    return Err(not_yet(i, "a position relative to an object an earlier op in this change moves"));
                }
                let fw = snap.world_transform(f).ok_or_else(|| bad(format!("{r} has no place in the world.")))?;
                Want::World(fw.compose(&unit(off)).ok_or_else(range)?.pos)
            }
        }
    };
    // An earlier op that re-frames the object (re-parents it or an ancestor, or adds or removes a Transform on the
    // way) makes any write wrong; one that only moves the frame matters only to a value converted from the world
    // (@chunks-reviewer's optional Low on c783ed96).
    let reframed = tkey(id).is_some_and(|k| dirty.reframed.contains(&k))
        || place.parent.is_some_and(|p| dirty.chain_reframed(snap, p));
    let moved = place.parent.is_some_and(|p| dirty.chain_moved(snap, p));
    if reframed || (moved && matches!(want, Want::World(_))) {
        return Err(not_yet(i, "a position for an object whose frame an earlier op in this change moves or changes"));
    }
    let (chunk, pos) = match (want, frame) {
        (Want::Local(off), _) => ([0; 3], off),
        (Want::World(p), None) => {
            let t = WorldTransform { pos: p, rot: [0.0, 0.0, 0.0, 1.0], scale: [1.0; 3] }.local_in(None);
            (t.chunk, t.pos)
        }
        (Want::World(p), Some(f)) => {
            let fw = snap.world_transform(f).ok_or_else(|| bad("Its frame has no place in the world.".into()))?;
            ([0; 3], WorldTransform { pos: p, rot: fw.rot, scale: fw.scale }.local_in(Some(&fw)).pos)
        }
    };
    if pos.iter().any(|x| !x.is_finite() || x.abs() > POS_LIMIT) || chunk.iter().any(|c| c.abs() > ROOT_CHUNK_LIMIT) {
        return Err(range());
    }
    let mut ops = vec![];
    if frame.is_none() {
        let value = chunk.iter().flat_map(|c| c.to_le_bytes()).collect();
        ops.push(Op::SetField { id, component: desc.id(), instance: 0, field: chunk_i, value });
    }
    let value = pos.iter().flat_map(|x| x.to_le_bytes()).collect();
    ops.push(Op::SetField { id, component: desc.id(), instance: 0, field: pos_i, value });
    Ok(ops)
}

/// The refusal of an `add` on a component objects already have (api.md 5.3): each object that has it, with its
/// current values when the object exists, so the caller sees what replacing it would lose.
fn already_has(view: &WorldView, op: u32, hit: &[Obj], component: &str, c: pe_sim::ComponentId) -> Box<Refusal> {
    use crate::ports::Snapshot as _;
    let which = if hit.len() == 1 {
        "The object already has".to_string()
    } else {
        format!("{} objects already have", hit.len())
    };
    let mut f = refusal(
        RefusalCode::Invalid,
        format!(
            "{which} a {component:?} component; `add` would replace it, resetting every field not given. Nothing was changed."
        ),
    );
    f.op = Some(op);
    f.field = Some(component.to_string());
    f.next = Some(format!(
        "Use `set` to change some of its fields, or repeat the `add` with `replace: true` to replace the whole {component:?} component."
    ));
    let objects: Vec<Json> = hit
        .iter()
        .map(|t| match t {
            Obj::Existing(o) => {
                let mut j = serde_json::json!({ "id": o.to_string() });
                if let Some(x) = view.info(*o) {
                    j["name"] = Json::String(x.name);
                }
                if let Some(current) = view.fields_by_id(*o, c, &ObjRef("parent".into())) {
                    j["current"] = serde_json::json!(current);
                }
                j
            }
            Obj::New(k) => serde_json::json!({ "new": k }),
        })
        .collect();
    if let Some(Obj::Existing(o)) = hit.first() {
        f.object = view.info(*o).map(|x| ObjectBrief { id: *o, name: x.name });
    }
    f.details = Some(serde_json::json!({ "component": component, "objects": objects }));
    f
}

/// A whole row of `component` for `t`: zeros, then each given field encoded at its offset.
fn whole_row(
    snap: &WorldSnapshot,
    on: &On,
    i: usize,
    t: Target,
    component: &str,
    fields: &BTreeMap<String, Json>,
) -> Result<Row, Unmapped> {
    let desc = on.desc(snap, component).map_err(|m| {
        let mut f = refusal(RefusalCode::Invalid, m);
        f.op = Some(i as u32);
        Unmapped::Refused(f)
    })?;
    if desc.id() == pe_sim::ComponentId::TRANSFORM {
        return Err(not_yet(i, "a Transform row in an add or create"));
    }
    let mut bytes = vec![0u8; desc.size()];
    for (k, v) in fields {
        if let Op::SetField { field, value, .. } = set_field(snap, on, i, t, &format!("{component}.{k}"), v)? {
            let at = desc.fields()[field as usize].offset as usize;
            bytes[at..at + value.len()].copy_from_slice(&value);
        }
    }
    Ok(Row { component: desc.id(), instance: 0, bytes })
}

/// The longest name, in UTF-8 bytes (api.md 5.3; ruling): under pe-world's inline class (256 B), so a
/// name's bytes always travel inside the entry that sets it and every machine can show it.
pub const NAME_MAX: usize = 255;
/// The most inline bytes (names) one change may carry, its undo's included (api.md 5.3; rulings). pe-world's own cap is twice this, so an undo whose old values grew through a race still fits.
pub const INLINE_MAX: usize = 1024;
/// The most carried bytes (simulation blobs over 256 B: meshes and hulls, world-model.md 3.4) one change may carry, its
/// undo's included (@coordinator-opus): pe-world's own cap is 960 KiB (`MAX_CARRIED_BYTES`), so an undo
/// whose old values grew through a race still fits.
pub const CARRIED_MAX: usize = 768 * 1024;

/// The bytes of the inline-class blobs among `blobs` (names), each once, in ascending order of hash: the one
/// encoding pe-world takes for `Transaction::inline` (world-model.md 3.4).
pub fn inline_of(blobs: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> =
        blobs.iter().filter(|b| pe_world::inline::is_inline(&BlobRef::of(b))).cloned().collect();
    out.sort_by_key(|b| BlobRef::of(b).hash);
    out.dedup_by_key(|b| BlobRef::of(b).hash);
    out
}

/// A name's rules (api.md 5.1), for a create's name and for every name an insert's content brings.
fn name_ok(op: u32, text: &str) -> Result<(), Unmapped> {
    if text.len() > NAME_MAX {
        return Err(refused_at(
            op,
            RefusalCode::Invalid,
            format!("A name is at most {NAME_MAX} bytes of UTF-8; this one is {}.", text.len()),
        ));
    }
    // An empty text would be a zero-length blob, not pe-world's none, so a reader finds no text; and a `/` would make
    // the object unreachable by path, which addresses by names (api.md 5.1; world-model's review).
    if text.is_empty() || text.contains('/') {
        return Err(refused_at(
            op,
            RefusalCode::Invalid,
            "A name is not empty and has no `/` (paths are made of names).".into(),
        ));
    }
    Ok(())
}

/// The `Name` row for `text`, checked against [`NAME_MAX`], with its bytes added once to the change's blobs.
fn named(
    snap: &WorldSnapshot,
    i: usize,
    op: u32,
    text: &str,
    blob_refs: &mut Vec<BlobRef>,
    blobs: &mut Vec<Vec<u8>>,
) -> Result<Row, Unmapped> {
    name_ok(op, text)?;
    let (row, r) = name_row(snap, i, text)?;
    if !blob_refs.contains(&r) {
        blob_refs.push(r);
        blobs.push(text.as_bytes().to_vec());
    }
    Ok(row)
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
    /// A recorded inverse (revert): its inline bytes may reach pe-world's cap, not [`INLINE_MAX`], and its own undo
    /// isn't predicted (ruling).
    pub undo: bool,
    /// The fields an `add` with `replace: true` changes on a component the object had, old -> new (api.md 5.3).
    pub replaced: Vec<FieldDiff>,
}

/// Why a resolved change has no pe-world payload: the change is refused, or the call can't serve it yet.
#[derive(Debug)]
pub enum Unmapped {
    Refused(Box<Refusal>),
    Failed(ApiError),
}

/// pe-world's payload for a resolved change, from `actor`, with values encoded through `snap`'s schema.
/// `me` is the caller's head object, for positions given relative to `"me"`.
pub fn to_world(
    r: &Resolved,
    actor: Actor,
    snap: &Arc<WorldSnapshot>,
    me: Option<ObjectId>,
) -> Result<WorldEdit, Unmapped> {
    to_world_built(r, actor, snap, me, None)
}

fn refused_at(op: u32, code: RefusalCode, message: String) -> Unmapped {
    let mut f = refusal(code, message);
    f.op = Some(op);
    Unmapped::Refused(f)
}

/// The tag of `script`'s active version, which attach and detach write (world-model.md 7.9; ruling),
/// or why there is none: a script this change commits, or a pending first version, isn't active yet; a voided first
/// version has nothing to wait for.
fn active_tag(snap: &WorldSnapshot, script: &Script, op: u32) -> Result<(pe_sim::ComponentId, [u8; 32]), Unmapped> {
    let a = match script {
        Script::New(_) => {
            return Err(refused_at(
                op,
                RefusalCode::NotReady,
                "Nothing was submitted. A new script can't be attached in the same change: commit it without `attach`, then attach it once its `mine` item says `activated`."
                    .into(),
            ));
        }
        Script::Existing(a) => a,
    };
    let Some(m) = snap.modules().get(a) else {
        return Err(match snap.pending().get(a) {
            Some(p) if p.void.is_none() => refused_at(
                op,
                RefusalCode::NotReady,
                "Nothing was submitted. The script's first version isn't active yet: attach it once its `mine` item says `activated`."
                    .into(),
            ),
            Some(_) => refused_at(
                op,
                RefusalCode::Invalid,
                "The script's first version was voided, so there is nothing to attach: commit it again.".into(),
            ),
            None => refused_at(op, RefusalCode::NotFound, format!("There's no script {}.", asset_text(a))),
        });
    };
    let tag = pe_world::script::script_component_id(&m.v(), pe_world::script::TAG_NAME, vec![])
        .filter(|c| m.components.contains(c))
        .ok_or_else(|| {
            Unmapped::Failed(ApiError::new(ErrorCode::Internal, "The script's tag isn't in its version."))
        })?;
    Ok((tag, *a))
}

/// The refusals of a change's script ops that need no build, run before `apply` builds (a build can take up to the
/// 15 s margin): an unknown script, or an attach or detach with no active version to tag.
pub fn precheck_scripts(r: &Resolved, snap: &WorldSnapshot) -> Result<(), Unmapped> {
    for (i, op) in r.ops.iter().enumerate() {
        let at = r.op_of.get(i).copied().unwrap_or(i as u32);
        match op {
            ROp::Code { script: Script::Existing(a), .. }
                if !snap.modules().contains_key(a) && !snap.pending().contains_key(a) =>
            {
                return Err(refused_at(at, RefusalCode::NotFound, format!("There's no script {}.", asset_text(a))));
            }
            ROp::Tag { script, .. } => {
                active_tag(snap, script, at)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// [`to_world`], with the author's build of the change's `code` op (api.md 7: compiled once, at submission).
pub fn to_world_built(
    r: &Resolved,
    actor: Actor,
    snap: &Arc<WorldSnapshot>,
    me: Option<ObjectId>,
    built: Option<&crate::builds::Built>,
) -> Result<WorldEdit, Unmapped> {
    let refused = |op: u32, code: RefusalCode, message: String| {
        let mut f = refusal(code, message);
        f.op = Some(op);
        Unmapped::Refused(f)
    };
    let view = WorldView::new(snap.clone(), me);
    // Each new object's frame, for a position set in the same change: its create's parent (or that parent's own
    // frame). A copy's inner objects, or a parent created in this change, have no known frame yet.
    let mut new_place: std::collections::HashMap<u32, Place> = Default::default();
    let has_transform = |o: pe_core::ObjectId| snap.components_on(o).contains(&pe_sim::ComponentId::TRANSFORM);
    let place_for = |t: Target, new_place: &std::collections::HashMap<u32, Place>| match t {
        Target::Existing(o) => Some(Place { frame: frame_of(snap, o), parent: snap.parent(o) }),
        Target::New(k) => new_place.get(&k).copied(),
        Target::Root => None,
    };
    // A new object's place under `parent`, when that is known before the change runs.
    let place_under = |parent: Target| match parent {
        Target::Existing(p) if has_transform(p) => Some(Place { frame: Some(p), parent: Some(p) }),
        Target::Existing(p) => Some(Place { frame: frame_of(snap, p), parent: Some(p) }),
        Target::Root => Some(Place { frame: None, parent: None }),
        Target::New(_) => None,
    };
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
    let mut dirty = Dirty::default();
    let mut last: Option<&ROp> = None;
    // Earlier attaches and detaches of this change, by target, in order: they count for a later op's names.
    let mut tagged: Tagged = Default::default();
    // Which rows this change has added or removed so far, by target and component, and what each new object was
    // copied from: whether a later `add` would replace a row (api.md 5.3).
    let mut rows_now: RowsNow = Default::default();
    let mut replaced: Vec<FieldDiff> = vec![];
    let mut copied: std::collections::HashMap<u32, pe_core::ObjectId> = Default::default();
    let has_row = |rows_now: &RowsNow, copied: &std::collections::HashMap<u32, pe_core::ObjectId>, t, c| {
        if let Some(k) = tkey(t)
            && let Some(b) = rows_now.get(&(k, c))
        {
            return *b;
        }
        let from = match t {
            Target::Existing(o) => Some(o),
            Target::New(k) => copied.get(&k).copied(),
            Target::Root => None,
        };
        from.is_some_and(|o| snap.components_on(o).contains(&c))
    };
    let names = view.names();
    let on_for = |t: Target, tagged: &Tagged| {
        let mut attached = match t {
            Target::Existing(o) => names.attached(snap, o),
            _ => vec![],
        };
        for (a, on) in tkey(t).and_then(|k| tagged.get(&k)).into_iter().flatten() {
            attached.retain(|x| x != a);
            if *on {
                attached.push(*a);
            }
        }
        On { names, attached }
    };
    for (i, op) in r.ops.iter().enumerate() {
        // What the previous op did to placements and attachments (done here, so `continue` arms count too).
        if let Some(p) = last.replace(op) {
            dirty.note(p);
            if let ROp::Tag { script: Script::Existing(a), target: t, on } = p
                && let Some(k) = tkey(target(*t))
            {
                tagged.entry(k).or_default().push((*a, *on));
            }
        }
        // The ops the previous resolved op pushed came from its source op (done here, so `continue` arms count too).
        op_of.resize(ops.len(), prev);
        prev = r.op_of.get(i).copied().unwrap_or(i as u32);
        ops.push(match op {
            ROp::Delete { target: t } => Op::Delete { id: target(*t) },
            ROp::Reparent { target: t, parent, index, keep_world } => {
                Op::Reparent { id: target(*t), parent: target(*parent), index: *index, keep_world: *keep_world }
            }
            ROp::Copy { source, new, parent, size } => {
                if let Some(p) = place_under(target(*parent)) {
                    new_place.insert(*new, p);
                }
                if let Obj::Existing(o) = source {
                    copied.insert(*new, (*o).into());
                }
                Op::Copy { source: target(*source), new: *new, parent: target(*parent), index: None, size: *size }
            }
            ROp::Set { target: t, fields } => {
                // One SetField per field, in field-name order (the map's), all on the same object; a position is
                // converted to the object's frame first.
                let on = on_for(target(*t), &tagged);
                for (k, v) in fields {
                    if on.is_name_text(snap, k) {
                        // A rename: the Name row for the new text, its bytes carried inline (api.md 5.3).
                        let Some(text) = v.as_str() else {
                            return Err(refused_at(
                                prev,
                                RefusalCode::Invalid,
                                format!("{k} takes the name as a string."),
                            ));
                        };
                        let row = named(snap, i, prev, text, &mut blob_refs, &mut blobs)?;
                        ops.push(Op::SetRow { id: target(*t), row });
                        continue;
                    }
                    if on.is_transform(snap, k).as_deref() == Some("Transform.position") {
                        let Some(place) = place_for(target(*t), &new_place) else {
                            return Err(not_yet(
                                i,
                                "setting the position of an object inside a copy made in this change",
                            ));
                        };
                        ops.extend(set_position(&view, snap, i, target(*t), place, v, &dirty)?);
                    } else {
                        ops.push(set_field(snap, &on, i, target(*t), k, v)?);
                    }
                }
                continue;
            }
            ROp::Create { new, size, parent, name, template, components } => {
                if let Some(p) = place_under(target(*parent)) {
                    new_place.insert(*new, p);
                }
                let name_row = named(snap, i, prev, name, &mut blob_refs, &mut blobs)?;
                let me = Target::New(*new);
                let on = on_for(me, &tagged);
                // A `Name` among the components (as a query shows one) may only repeat `name`: the name is given once.
                let mut components = components.clone();
                if let Some(c) = components.keys().find(|c| on.is_name(snap, c)).cloned() {
                    let given = components.remove(&c).unwrap_or_default();
                    if given.iter().any(|(k, v)| k != "text" || v.as_str() != Some(name.as_str())) {
                        return Err(refused_at(
                            prev,
                            RefusalCode::Invalid,
                            format!(
                                "A create's name is its `name`; leave {c} out of `components`, or give the same text."
                            ),
                        ));
                    }
                }
                let components = &components;
                // The components given as field writes after the object exists: a template's, or a Transform.
                let writes: Vec<_> = match template {
                    None => {
                        // A Transform starts at the parent's origin (a root's at the world's), and its fields are
                        // then written as on any object, so a position can be given in any frame (api.md 5.3).
                        let mut rows = vec![name_row];
                        let mut writes = vec![];
                        for (c, fields) in components {
                            if on.desc(snap, c).is_ok_and(|d| d.id() == pe_sim::ComponentId::TRANSFORM) {
                                let bytes = bytemuck::bytes_of(&pe_sim::Transform::IDENTITY).to_vec();
                                rows.push(Row { component: pe_sim::ComponentId::TRANSFORM, instance: 0, bytes });
                                writes.push((c, fields));
                            } else {
                                rows.push(whole_row(snap, &on, i, me, c, fields)?);
                            }
                        }
                        for r in &rows {
                            rows_now.insert(((true, u128::from(*new)), r.component), true);
                        }
                        ops.push(Op::Create { new: *new, parent: target(*parent), index: None, rows });
                        writes
                    }
                    Some(tpl) => {
                        // The copy brings the template's rows; the name replaces its Name row, and the components
                        // given are field writes on the copy.
                        copied.insert(*new, (*tpl).into());
                        let source = Target::Existing((*tpl).into());
                        ops.push(Op::Copy { source, new: *new, parent: target(*parent), index: None, size: *size });
                        ops.push(Op::SetRow { id: me, row: name_row });
                        components.iter().collect()
                    }
                };
                for (c, fields) in writes {
                    for (k, v) in fields {
                        let key = format!("{c}.{k}");
                        if on.is_transform(snap, &key).as_deref() == Some("Transform.position") {
                            let Some(place) = new_place.get(new).copied() else {
                                return Err(not_yet(i, "a position under a parent made in this change"));
                            };
                            ops.extend(set_position(&view, snap, i, me, place, v, &dirty)?);
                        } else {
                            ops.push(set_field(snap, &on, i, me, &key, v)?);
                        }
                    }
                }
                continue;
            }
            ROp::Insert { new, size: _, parent, content, components } => {
                // The bytes this insert's own blobs add, against the change's caps (outcome.rs checks the whole
                // change later; this says why an insert, which can't be split, is too large).
                let (mut names_bytes, mut carried) = (0usize, 0usize);
                // One `Create` per item (api.md 5.3.1a): the top under `parent`, at the parent's origin until a
                // `Transform` write in `components` places it; the rest under their own parents, as the content keeps
                // them. Every blob a row references travels inline, so the content needs no store of its own.
                let under = parent.map_or(Target::Root, target);
                if let Some(p) = place_under(under) {
                    new_place.insert(*new, p);
                }
                for (k, item) in content.items.iter().enumerate() {
                    let k = k as u32;
                    let at = new + k;
                    let p = match item.parent {
                        None => under,
                        Some(q) => Target::New(new + q),
                    };
                    let mut rows = Vec::with_capacity(item.rows.len());
                    for (c, bytes) in &item.rows {
                        // A starter object's provenance (pe-world's `Starter`) means nothing in a domain without
                        // it: the row is left out there. Any other component the domain lacks refuses the insert.
                        if *c == pe_world::components::ids::STARTER && snap.component_desc(*c).is_none() {
                            continue;
                        }
                        let Some(desc) = snap.component_desc(*c) else {
                            return Err(refused_at(
                                prev,
                                RefusalCode::Invalid,
                                format!(
                                    "This domain has no component #{:x}, which the content uses; it can't hold this \
                                     object.",
                                    c.0
                                ),
                            ));
                        };
                        let mut bytes = bytes.clone();
                        if k == 0 && *c == pe_sim::ComponentId::TRANSFORM {
                            let t: pe_sim::Transform = bytemuck::try_pod_read_unaligned(&bytes).map_err(|_| {
                                refused_at(
                                    prev,
                                    RefusalCode::Invalid,
                                    "The content's top transform is malformed.".into(),
                                )
                            })?;
                            let t = pe_sim::Transform { chunk: [0; 3], pos: [0.0; 3], ..t };
                            bytes = bytemuck::bytes_of(&t).to_vec();
                        }
                        // A name the content brings keeps the rules a create's name keeps. A name whose blob the
                        // content doesn't carry is refused below as such.
                        if *c == pe_world::components::ids::NAME {
                            let r = pe_world::inline::row_blobs(desc, &bytes).unwrap_or_default();
                            if let Some(t) = r.first().and_then(|(b, _)| content.blobs.get(&b.hash)) {
                                name_ok(prev, std::str::from_utf8(t).unwrap_or(""))?;
                            }
                        }
                        for (b, sim) in pe_world::inline::row_blobs(desc, &bytes).unwrap_or_default() {
                            // The empty blob, like none, is on every machine and travels with nothing.
                            if b.is_none() || b.len == 0 || blob_refs.contains(&b) {
                                continue;
                            }
                            let Some(data) = content.blobs.get(&b.hash) else {
                                return Err(refused_at(
                                    prev,
                                    RefusalCode::Invalid,
                                    "The content references a blob it doesn't carry.".into(),
                                ));
                            };
                            if !pe_world::inline::is_carried(&b, sim) {
                                return Err(not_yet(i, "inserting content with a blob too large to travel in an edit"));
                            }
                            if data.len() > pe_world::inline::MAX_INLINE_BLOB {
                                carried += data.len();
                            } else {
                                names_bytes += data.len();
                            }
                            blob_refs.push(b);
                            blobs.push(data.clone());
                        }
                        rows_now.insert(((true, u128::from(at)), *c), true);
                        rows.push(Row { component: *c, instance: 0, bytes });
                    }
                    ops.push(Op::Create { new: at, parent: p, index: None, rows });
                }
                for (what, n, limit) in [("names", names_bytes, INLINE_MAX), ("meshes or hulls", carried, CARRIED_MAX)]
                {
                    if n > limit {
                        let mut f = refusal(
                            RefusalCode::TooLarge,
                            format!(
                                "This object's subtree carries {n} bytes of {what}, more than the {limit} one change \
                                 may; it can't be inserted in one change yet."
                            ),
                        );
                        f.op = Some(prev);
                        f.details = Some(serde_json::json!({ "limit": limit, "bytes": n }));
                        return Err(Unmapped::Refused(f));
                    }
                }
                let me = Target::New(*new);
                let on = on_for(me, &tagged);
                for (c, fields) in components {
                    for (k, v) in fields {
                        let key = format!("{c}.{k}");
                        if on.is_transform(snap, &key).as_deref() == Some("Transform.position") {
                            let Some(place) = new_place.get(new).copied() else {
                                return Err(not_yet(i, "a position under a parent made in this change"));
                            };
                            ops.extend(set_position(&view, snap, i, me, place, v, &dirty)?);
                        } else {
                            ops.push(set_field(snap, &on, i, me, &key, v)?);
                        }
                    }
                }
                continue;
            }
            ROp::Add { target: t, component, fields, replace } => {
                let on = on_for(target(*t), &tagged);
                let c = on
                    .desc(snap, component)
                    .map_err(|m| {
                        let mut f = refusal(RefusalCode::Invalid, m);
                        f.op = Some(i as u32);
                        Unmapped::Refused(f)
                    })?
                    .id();
                // `add` writes a whole row, so on a component the object has (its preset's, its template's, a copied
                // one, or one this change added) it would reset every field not given: refused unless asked, before
                // the row is built, naming every object of the op that has it (api.md 5.3; F6 of the M1 redo).
                let has = has_row(&rows_now, &copied, target(*t), c);
                if has && !replace {
                    let mut hit = vec![*t];
                    for (j, later) in r.ops.iter().enumerate().skip(i + 1) {
                        if r.op_of.get(j).copied().unwrap_or(j as u32) != prev {
                            break;
                        }
                        if let ROp::Add { target: u, component: uc, .. } = later
                            && uc == component
                            && has_row(&rows_now, &copied, target(*u), c)
                        {
                            hit.push(*u);
                        }
                    }
                    return Err(Unmapped::Refused(already_has(&view, prev, &hit, component, c)));
                }
                let row = whole_row(snap, &on, i, target(*t), component, fields)?;
                if let Obj::Existing(o) = t
                    && !replace
                {
                    // The row must still be as this change read it when the entry applies: absent (the zero version),
                    // or, after a remove in this change, the row that remove names. Kept whatever `if_version` says
                    // and through a re-send, so two plain adds racing can't overwrite each other (api.md 5.3).
                    let is = snap.row_version_of((*o).into(), c);
                    let p = Precondition::Version { id: (*o).into(), component: c, instance: 0, is };
                    if !preconditions.contains(&p) {
                        preconditions.push(p);
                    }
                }
                if has {
                    // A replace: what it changes, old -> new, for the outcome's `diff.set` (api.md 5.3).
                    let from = match t {
                        Obj::Existing(o) => Some(*o),
                        Obj::New(k) => copied.get(k).map(|o| ObjectId::from(*o)),
                    };
                    let untouched = tkey(target(*t)).is_none_or(|k| !rows_now.contains_key(&(k, c)));
                    if let Some(from) = from.filter(|_| untouched)
                        && let Some(before) = view.fields_by_id(from, c, &ObjRef("parent".into()))
                        && let Some(after) = view.row_json(from, c, &row.bytes)
                    {
                        let shown = names.shown_on(&on.attached, c).unwrap_or_else(|| component.clone());
                        let id = match t {
                            Obj::Existing(o) => ObjRef(o.to_string()),
                            Obj::New(k) => ObjRef(
                                r.refs
                                    .iter()
                                    .find(|(_, v)| **v == *k)
                                    .map_or_else(|| format!("${k}"), |(n, _)| n.clone()),
                            ),
                        };
                        for (k, b) in &before {
                            let a = after.get(k).cloned().unwrap_or(Json::Null);
                            if a != *b {
                                replaced.push(FieldDiff {
                                    id: id.clone(),
                                    field: format!("{shown}.{k}"),
                                    before: b.clone(),
                                    after: a,
                                });
                            }
                        }
                    }
                }
                if let Some(k) = tkey(target(*t)) {
                    rows_now.insert((k, c), true);
                }
                Op::SetRow { id: target(*t), row }
            }
            ROp::Remove { target: t, component } => {
                let Obj::Existing(o) = t else { return Err(not_yet(i, "removing a row this change adds")) };
                let on = on_for(target(*t), &tagged);
                let desc = on.desc(snap, component).map_err(|m| {
                    let mut f = refusal(RefusalCode::Invalid, m);
                    f.op = Some(i as u32);
                    Unmapped::Refused(f)
                })?;
                if let Some(k) = tkey(target(*t)) {
                    rows_now.insert((k, desc.id()), false);
                }
                match snap.row_tag_of((*o).into(), desc.id()) {
                    Some(tag) => Op::RemoveRow { id: target(*t), component: desc.id(), instance: 0, tag },
                    None => {
                        let mut f =
                            refusal(RefusalCode::NotFound, format!("The object has no {component:?} component."));
                        f.op = Some(i as u32);
                        f.object =
                            snap.info((*o).into()).map(|x| ObjectBrief { id: *o, name: x.name.unwrap_or_default() });
                        return Err(Unmapped::Refused(f));
                    }
                }
            }
            ROp::Code { script, if_version, source } => {
                let Some(b) = built else {
                    return Err(Unmapped::Failed(ApiError::new(
                        ErrorCode::Unavailable,
                        "This machine has no build service, so it can't commit scripts.",
                    )));
                };
                let archive = crate::builds::archive_of(source)
                    .map_err(|e| refused(prev, RefusalCode::Invalid, format!("The source can't be packed: {e}")))?;
                let (target, base) = match script {
                    Script::New(k) => (AssetTarget::New(*k), None),
                    Script::Existing(a) => {
                        let current = snap.modules().get(a).map(|m| m.version);
                        if current.is_none() && !snap.pending().contains_key(a) {
                            return Err(refused(
                                prev,
                                RefusalCode::NotFound,
                                format!("There's no script {}.", asset_text(a)),
                            ));
                        }
                        // The caller's base, else the current version; pe-world refuses a stale one with the
                        // winner (`stale_version`, api.md 7).
                        let base = match if_version {
                            Some(v) => Some(version(v).map_err(Unmapped::Refused)?),
                            None => current,
                        };
                        (AssetTarget::Existing(*a), base)
                    }
                };
                // Its components, the tag included, registered under the commit (world-model.md 7.9), then the
                // commit; `activate_at` is set by the submitting machine's step 2 (api.md 7).
                ops.push(Op::Register { components: b.components.clone() });
                ops.push(Op::commit_script(target, base, archive.into(), b.toolchain, b.sdk, b.module, 0));
                continue;
            }
            ROp::Tag { script, target: t, on } => {
                let (tag, a) = active_tag(snap, script, prev)?;
                if *on {
                    Op::SetRow { id: target(*t), row: Row { component: tag, instance: 0, bytes: vec![] } }
                } else {
                    // The script's other rows stay (api.md 7).
                    let Obj::Existing(o) = t else {
                        return Err(not_yet(i, "detaching from an object this change makes"));
                    };
                    let Some(row) = snap.row_tag_of((*o).into(), tag) else {
                        return Err(refused(
                            prev,
                            RefusalCode::NotFound,
                            format!("The object isn't attached to script {}.", asset_text(&a)),
                        ));
                    };
                    Op::RemoveRow { id: target(*t), component: tag, instance: 0, tag: row }
                }
            }
        });
    }
    op_of.resize(ops.len(), prev);
    let inline = inline_of(&blobs);
    let tx = Transaction { inline, actor, label: Some(r.label.clone()), preconditions, ops, blobs: blob_refs };
    Ok(WorldEdit { tx, blobs, op_of, undo: false, replaced })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::resolve_tx;
    use crate::world::{WorldView, version_text};
    use pe_core::{EntryRef, Frame, Hash, MemberKey, Offset, Pos};
    use pe_seams::world_api::Transaction as ApiTx;
    use pe_sim::Transform;
    use pe_world::components::{Body, Light, Name, body_kind, light_kind};
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
        world_with(|_| {})
    }

    /// [`world`], with `more` run on it before the snapshot.
    fn world_with(more: impl FnOnce(&mut World)) -> (Arc<WorldSnapshot>, WorldView) {
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
        more(&mut w);
        w.set_frame(Frame(9));
        let s = w.snapshot(Frame(9));
        (s.clone(), WorldView::new(s, None))
    }

    fn resolved(v: &WorldView, ops: Json) -> Resolved {
        resolve_tx(v, &tx(ops), &|_| None).map_err(|e| e.message).unwrap()
    }

    /// The f64 or i64 triple a SetField writes.
    fn f3(op: &Op) -> [f64; 3] {
        let Op::SetField { value, .. } = op else { panic!("{op:?}") };
        let v: Vec<f64> = value.chunks(8).map(|b| f64::from_le_bytes(b.try_into().unwrap())).collect();
        v.try_into().unwrap()
    }
    fn i3(op: &Op) -> [i64; 3] {
        let Op::SetField { value, .. } = op else { panic!("{op:?}") };
        let v: Vec<i64> = value.chunks(8).map(|b| i64::from_le_bytes(b.try_into().unwrap())).collect();
        v.try_into().unwrap()
    }
    fn near(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }
    fn set_pos(s: &Arc<WorldSnapshot>, v: &WorldView, obj: &str, value: Json) -> Result<Vec<Op>, Unmapped> {
        let r = resolved(v, json!([{"op": "set", "id": obj, "field": "Transform.position", "value": value}]));
        to_world(&r, Actor::Person, s, None).map(|e| e.tx.ops)
    }

    #[test]
    fn a_position_is_written_in_the_objects_own_frame() {
        let (s, v) = world();
        // A child, in its parent's frame: stored as given.
        let ops = set_pos(&s, &v, "/Table/Lamp", json!({"relative_to": "parent", "offset": [0.0, 2.0, 0.0]})).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(f3(&ops[0]), [0.0, 2.0, 0.0]);
        // A child, at the Shelf (Table at x=10, Shelf at x=40): 30 m along the Table's x.
        let ops = set_pos(&s, &v, "/Table/Lamp", json!({"relative_to": "/Shelf", "offset": [0.0, 0.0, 0.0]})).unwrap();
        assert!(near(f3(&ops[0]), [30.0, 0.0, 0.0]), "{:?}", f3(&ops[0]));
        // A root, from the world origin: its exact chunk and offset, together.
        let ops = set_pos(&s, &v, "/Table", json!({"relative_to": "world_origin", "offset": [5.0, 0.0, 0.0]})).unwrap();
        assert_eq!(ops.len(), 2);
        let p = Pos::at_chunk(i3(&ops[0])).unwrap().checked_add(Offset(f3(&ops[1]))).unwrap();
        assert!(near((p - Pos::ORIGIN).0, [5.0, 0.0, 0.0]));
        // A root, as an exact pos.
        let shelf = Pos::ORIGIN.checked_add(Offset([40.0, 0.0, 0.0])).unwrap();
        let c = shelf.chunk_index();
        let exact = json!({"pos": {"chunk": c.map(|x| x.to_string()), "offset": [1.0, 2.0, 3.0]}});
        let ops = set_pos(&s, &v, "/Shelf", exact).unwrap();
        assert_eq!(i3(&ops[0]), c);
        assert!(near(f3(&ops[1]), [1.0, 2.0, 3.0]));
        // A root has no parent frame.
        let e = set_pos(&s, &v, "/Table", json!({"offset": [0.0, 0.0, 0.0]}));
        assert!(matches!(e, Err(Unmapped::Refused(f)) if f.message.contains("has no parent")));
        // Bad shapes and the stored fields themselves are refused as invalid.
        assert!(matches!(set_pos(&s, &v, "/Table/Lamp", json!({"offset": [1.0]})), Err(Unmapped::Refused(_))));
        let r =
            resolved(&v, json!([{"op": "set", "id": "/Table", "field": "Transform.pos", "value": [0.0, 0.0, 0.0]}]));
        assert!(
            matches!(to_world(&r, Actor::Person, &s, None), Err(Unmapped::Refused(f)) if f.code == RefusalCode::Invalid)
        );
    }

    #[test]
    fn a_rotation_is_written_as_the_stored_quaternion_from_either_form() {
        let (s, v) = world();
        let set = |field: &str, value: Json| {
            let r = resolved(&v, json!([{"op": "set", "id": "/Table/Lamp", "field": field, "value": value}]));
            to_world(&r, Actor::Person, &s, None).map(|e| e.tx.ops)
        };
        let quat = |op: &Op| -> (u16, Vec<f32>) {
            let Op::SetField { field, value, .. } = op else { panic!("{op:?}") };
            (*field, value.chunks(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect())
        };
        let rot_index = s.component("Transform").unwrap().fields().iter().position(|f| f.name == "rot").unwrap();
        let (field, q) = quat(&set("Transform.rotation_deg", json!([0.0, 90.0, 0.0])).unwrap()[0]);
        assert_eq!(field as usize, rot_index);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!((q[1] - h).abs() < 1e-6 && (q[3] - h).abs() < 1e-6 && q[0].abs() < 1e-6 && q[2].abs() < 1e-6, "{q:?}");
        assert!(matches!(set("Transform.rotation_deg", json!([1.0, 2.0])), Err(Unmapped::Refused(_))));
        assert!(matches!(set("Transform.rotation_quat", json!([0.0, 0.0, 0.0, 0.0])), Err(Unmapped::Refused(_))));
        let (_, q) = quat(&set("Transform.rotation_quat", json!([0.0, 0.0, 0.0, 2.0])).unwrap()[0]);
        assert_eq!(q, vec![0.0, 0.0, 0.0, 1.0], "normalized");
    }

    #[test]
    fn a_position_is_not_yet_when_an_earlier_op_moves_its_frame() {
        let (s, v) = world();
        let not_yet = |ops: Json| {
            let r = resolved(&v, ops);
            matches!(to_world(&r, Actor::Person, &s, None), Err(Unmapped::Failed(e)) if e.code == ErrorCode::Internal)
        };
        let at = json!({"relative_to": "world_origin", "offset": [5.0, 0.0, 0.0]});
        // Re-parented first, then placed.
        assert!(not_yet(json!([
            {"op": "reparent", "id": "/Table/Lamp", "parent": "/Shelf"},
            {"op": "set", "id": "/Table/Lamp", "field": "Transform.position", "value": at}
        ])));
        // Its frame moved first.
        assert!(not_yet(json!([
            {"op": "set", "id": "/Table", "field": "Transform.position", "value": at},
            {"op": "set", "id": "/Table/Lamp", "field": "Transform.position", "value": at}
        ])));
        // Relative to an object moved first, or made in this change.
        assert!(not_yet(json!([
            {"op": "set", "id": "/Shelf", "field": "Transform.position", "value": at},
            {"op": "set", "id": "/Table/Lamp", "field": "Transform.position", "value": {"relative_to": "/Shelf", "offset": [0.0, 0.0, 0.0]}}
        ])));
        assert!(not_yet(json!([
            {"op": "create", "as": "$c", "parent": "/Shelf", "name": "c"},
            {"op": "set", "id": "/Table/Lamp", "field": "Transform.position", "value": {"relative_to": "$c", "offset": [0.0, 0.0, 0.0]}}
        ])));
        // A new object under a Transform-less parent (its frame: the Table), whose parent is re-parented first.
        let (s2, v2) = world_with(|w| {
            let text = w.blobs().put(b"Group");
            w.create(id(50), Some(id(10)), ALICE, ver(50, 0)).unwrap();
            w.insert(id(50), Name { text }).unwrap();
        });
        let r = resolved(
            &v2,
            json!([
                {"op": "create", "as": "$cup", "parent": "/Table/Group", "name": "Cup"},
                {"op": "reparent", "id": "/Table/Group", "parent": "/Shelf"},
                {"op": "set", "id": "$cup", "field": "Transform.position", "value": {"relative_to": "parent", "offset": [0.0, 1.0, 0.0]}}
            ]),
        );
        assert!(
            matches!(to_world(&r, Actor::Person, &s2, None), Err(Unmapped::Failed(e)) if e.code == ErrorCode::Internal)
        );
        // A frame that only moved leaves a parent-relative write right: it is stored as given.
        let r = resolved(
            &v,
            json!([
                {"op": "set", "id": "/Table", "field": "Transform.position", "value": at},
                {"op": "set", "id": "/Table/Lamp", "field": "Transform.position", "value": {"relative_to": "parent", "offset": [0.0, 1.0, 0.0]}}
            ]),
        );
        let ops = to_world(&r, Actor::Person, &s, None).unwrap().tx.ops;
        assert_eq!(f3(ops.last().unwrap()), [0.0, 1.0, 0.0]);
        // A later op, or an unrelated earlier one, is fine.
        let r = resolved(
            &v,
            json!([
                {"op": "set", "id": "/Table/Lamp", "field": "Transform.position", "value": at},
                {"op": "set", "id": "/Shelf", "field": "Transform.position", "value": at}
            ]),
        );
        assert!(to_world(&r, Actor::Person, &s, None).is_ok());
    }

    #[test]
    fn a_new_object_is_placed_in_its_parents_frame_in_the_same_change() {
        let (s, v) = world();
        let r = resolved(
            &v,
            json!([
                {"op": "create", "as": "$cup", "parent": "/Table", "name": "Cup"},
                {"op": "set", "id": "$cup", "field": "Transform.position", "value": {"relative_to": "/Shelf", "offset": [0.0, 1.0, 0.0]}}
            ]),
        );
        let ops = to_world(&r, Actor::Person, &s, None).unwrap().tx.ops;
        let last = ops.last().unwrap();
        assert!(matches!(last, Op::SetField { id: Target::New(0), .. }));
        assert!(near(f3(last), [30.0, 1.0, 0.0]));
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
        let w = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap().tx;
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
        let w = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap().tx;
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

    /// A colour field (api.md 4.4; ruling): hex in through pe-sdk's table, a bare list refused, and a
    /// read shows hex only when exact.
    #[test]
    fn colours_are_written_and_read_as_colours() {
        use crate::ports::Snapshot as _;
        use pe_sdk::color::srgb8_to_linear;
        use pe_world::components::Appearance;
        let red = [srgb8_to_linear(0xd2), srgb8_to_linear(0x2b), srgb8_to_linear(0x2b), 1.0];
        let (s, v) = world_with(|w| {
            w.insert(id(30), Appearance { base: red, emissive: [4.0, 0.0, 0.0], ..Default::default() }).unwrap();
        });
        // Read: base is exactly #d22b2b; an over-1 emissive has no hex, so it reads as linear.
        let f = v.fields(id(30).into(), "Appearance", &ObjRef("parent".into())).unwrap();
        assert_eq!((f["base"].clone(), f["emissive"].clone()), (json!("#d22b2b"), json!({"linear": [4.0, 0.0, 0.0]})));
        // Write: hex and srgb through the table; linear exact.
        let r = resolved(
            &v,
            json!([{"op": "set", "id": "/Table/Ball", "fields": {"Appearance.base": "#2b5bd280", "Appearance.emissive": {"linear": [0.5, 0, 0]}}}]),
        );
        let w = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap().tx;
        let want = |c: &[f32]| -> Vec<u8> { c.iter().flat_map(|x| x.to_le_bytes()).collect() };
        let values: Vec<Vec<u8>> = w
            .ops
            .iter()
            .map(|o| if let Op::SetField { value, .. } = o { value.clone() } else { panic!("{o:?}") })
            .collect();
        let blue = [srgb8_to_linear(0x2b), srgb8_to_linear(0x5b), srgb8_to_linear(0xd2), 128.0 / 255.0];
        assert_eq!(values, vec![want(&blue), want(&[0.5, 0.0, 0.0])]);
        // A bare list says no colour space: refused on its field, naming the forms.
        let r = resolved(
            &v,
            json!([{"op": "set", "id": "/Table/Ball", "field": "Appearance.base", "value": [1, 0, 0, 1]}]),
        );
        match to_world(&r, Actor::Person, &s, None) {
            Err(Unmapped::Refused(f)) => {
                assert_eq!((f.code, f.field.as_deref()), (RefusalCode::Invalid, Some("Appearance.base")));
                assert!(f.message.contains("#rrggbb") && f.message.contains("bare list"), "{}", f.message);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn create_plain_and_from_a_template() {
        let (s, v) = world();
        let r = resolved(
            &v,
            json!([{"op": "create", "as": "$cup", "parent": "/Table", "name": "Cup", "components": {"Body": {"friction": 0.25}}}]),
        );
        let e = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap();
        let cup = BlobRef::of(b"Cup");
        assert_eq!(e.tx.blobs, [cup]);
        assert_eq!(e.blobs, [b"Cup".to_vec()]);
        // The name's bytes ride inside the entry (world-model.md 3.4).
        assert_eq!(e.tx.inline, [b"Cup".to_vec()]);
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
        let e = to_world(&r, Actor::Person, &s2, None).map_err(|e| format!("{e:?}")).unwrap();
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
        let w = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap().tx;
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
            matches!(to_world(&r, Actor::Person, &s, None), Err(Unmapped::Refused(f)) if f.code == RefusalCode::NotFound)
        );
    }

    /// [`world`] with the M1 preset's lamp light on the Lamp (intensity 40, range 8) and a Body on it too.
    fn lit() -> (Arc<WorldSnapshot>, WorldView) {
        world_with(|w| {
            let light = Light {
                kind: light_kind::POINT,
                intensity: 40.0,
                range: 8.0,
                color: [1.0, 0.95, 0.85],
                ..Default::default()
            };
            w.insert(id(20), light).unwrap();
            w.insert(id(20), Body { kind: body_kind::STATIC, friction: 0.25, ..Default::default() }).unwrap();
        })
    }

    fn refused_by(s: &Arc<WorldSnapshot>, v: &WorldView, ops: Json) -> Box<Refusal> {
        match to_world(&resolved(v, ops), Actor::Person, s, None) {
            Err(Unmapped::Refused(f)) => f,
            other => panic!("{other:?}"),
        }
    }

    fn row_pre(e: &WorldEdit) -> Vec<(pe_core::ObjectId, pe_sim::ComponentId, pe_seams::Version)> {
        e.tx.preconditions
            .iter()
            .filter_map(|p| match p {
                Precondition::Version { id, component, is, .. } => Some((*id, *component, *is)),
                _ => None,
            })
            .collect()
    }

    /// F6 of the M1 redo: `add Light` on the preset lamp replaced its light without a word. Now it is refused, with
    /// the current values, unless `replace: true`, which says old -> new (api.md 5.3).
    #[test]
    fn add_on_a_component_the_object_has_is_refused_unless_replace() {
        let (s, v) = lit();
        let light = s.component("Light").unwrap().id();
        let add_light = json!({"op": "add", "id": "/Table/Lamp", "component": "Light",
            "fields": {"kind": 0, "intensity": 1.0, "range": 3.0, "color": {"linear": [1.0, 0.8, 0.6]}}});
        let f = refused_by(&s, &v, json!([add_light]));
        assert_eq!((f.code, f.op, f.field.as_deref()), (RefusalCode::Invalid, Some(0), Some("Light")), "{}", f.message);
        assert!(f.message.contains("already has a \"Light\" component"), "{}", f.message);
        assert!(f.next.as_deref().unwrap().contains("replace: true"));
        assert_eq!(f.object.as_ref().map(|o| o.name.as_str()), Some("Lamp"));
        let d = f.details.unwrap();
        assert_eq!(d["objects"][0]["current"]["intensity"], json!(40.0), "{d}");
        assert_eq!(d["objects"][0]["current"]["range"], json!(8.0), "{d}");
        // `replace: true` replaces it: no row guard, and the outcome's diff says old -> new.
        let mut replace = add_light.clone();
        replace["replace"] = json!(true);
        let e =
            to_world(&resolved(&v, json!([replace])), Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap();
        assert!(matches!(&e.tx.ops[0], Op::SetRow { row, .. } if row.component == light));
        assert!(row_pre(&e).is_empty());
        let diff = |f: &str| e.replaced.iter().find(|d| d.field == f).map(|d| (d.before.clone(), d.after.clone()));
        assert_eq!(diff("Light.intensity"), Some((json!(40.0), json!(1.0))), "{:?}", e.replaced);
        assert_eq!(diff("Light.range"), Some((json!(8.0), json!(3.0))));
        assert_eq!(diff("Light.kind"), None, "unchanged fields aren't listed");
        // Every object has a Transform: a clean `invalid`, never the unsupported whole-Transform row.
        let f = refused_by(&s, &v, json!([{"op": "add", "id": "/Table/Ball", "component": "Transform"}]));
        assert_eq!((f.code, f.field.as_deref()), (RefusalCode::Invalid, Some("Transform")));
        // A selector: one object that has it refuses the whole change, and every such object is named.
        let f = refused_by(
            &s,
            &v,
            json!([{"op": "add", "where": {"child_of": "/Table"}, "max": 10, "component": "Body", "fields": {"friction": 0.9}}]),
        );
        assert!(f.message.starts_with("2 objects already have"), "{}", f.message);
        let names: Vec<_> =
            f.details.unwrap()["objects"].as_array().unwrap().iter().map(|o| o["name"].clone()).collect();
        assert_eq!(names, [json!("Lamp"), json!("Ball")]);
        // On an object without it: added, guarded by the zero version (absent), whatever if_version says.
        let body = s.component("Body").unwrap().id();
        let e = to_world(
            &resolved(&v, json!([{"op": "add", "id": "/Table", "component": "Body", "if_version": "any"}])),
            Actor::Person,
            &s,
            None,
        )
        .unwrap();
        assert_eq!(row_pre(&e), [(id(10), body, pe_world::GENESIS)]);
        assert!(e.replaced.is_empty());
    }

    /// "Already has" is the row as the op finds it: earlier ops of the change, a create's rows, a template's and a
    /// copy's count (coordinator, item 1).
    #[test]
    fn already_has_follows_the_change_and_its_copies() {
        let (s, v) = world_with(|w| {
            for (n, name, parent) in [(1u128, "Templates", None), (2, "box", Some(1))] {
                let text = w.blobs().put(name.as_bytes());
                w.create(id(n), parent.map(id), ALICE, ver(n as u8, 0)).unwrap();
                w.insert(id(n), Name { text }).unwrap();
            }
            w.insert(id(2), Body { kind: body_kind::DYNAMIC, friction: 0.5, ..Default::default() }).unwrap();
        });
        let body = s.component("Body").unwrap().id();
        let add = |id: &str| json!({"op": "add", "id": id, "component": "Body", "fields": {"friction": 0.1}});
        // Two adds in one change: the second finds the first's row.
        let f = refused_by(&s, &v, json!([add("/Table/Lamp"), add("/Table/Lamp")]));
        assert_eq!((f.code, f.op), (RefusalCode::Invalid, Some(1)));
        // A create's components, a template's rows and a duplicate's rows.
        let create = json!({"op": "create", "parent": "/Table", "name": "c", "as": "$c", "components": {"Body": {}}});
        assert_eq!(refused_by(&s, &v, json!([create, add("$c")])).op, Some(1));
        let from_tpl = json!({"op": "create", "parent": "/Table", "name": "t", "as": "$t", "template": "box"});
        assert_eq!(refused_by(&s, &v, json!([from_tpl, add("$t")])).op, Some(1));
        let dup = json!({"op": "duplicate", "id": "/Table/Ball", "as": "$d", "parent": "/Table"});
        assert_eq!(refused_by(&s, &v, json!([dup, add("$d")])).op, Some(1));
        // Removed earlier in the change: added again, guarded by the removed row's version, so a stale remove can't
        // turn the add into an overwrite.
        let r = resolved(&v, json!([{"op": "remove", "id": "/Table/Ball", "component": "Body"}, add("/Table/Ball")]));
        let e = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap();
        let pre = row_pre(&e);
        assert_eq!(pre.len(), 1);
        assert_eq!((pre[0].0, pre[0].1), (id(30), body));
        // The fixture's rows were inserted, never edited, so their version is the zero one too; an edited row's is its
        // edit's (pe-world's Version precondition compares either).
        assert_eq!(pre[0].2, s.row_version_of(id(30), body));
    }

    /// Two plain adds racing from one read: the first applies, the second is refused `stale_version` and the first's
    /// values stay; a replace's undo restores the old row (coordinator, items 2 and 3).
    #[test]
    fn racing_plain_adds_keep_the_winner_and_a_replace_undoes() {
        let (s, v) = world_with(|w| w.set_member(ALICE, pe_world::perm::EDIT));
        let draft = pe_world::Draft {
            author: ALICE,
            line: pe_core::LineId(Hash([7; 32])),
            chain: pe_core::ChainId(Hash([9; 32])),
            seq: 1,
        };
        let add = |friction: f64| {
            let r = resolved(
                &v,
                json!([{"op": "add", "id": "/Table/Lamp", "component": "Body", "fields": {"friction": friction}, "if_version": "any"}]),
            );
            (to_world(&r, Actor::Person, &s, None).unwrap(), r)
        };
        // `if_version: "any"`, so no object version guards them: only the row guard can tell them apart.
        let ((first, _), (second, r2)) = (add(0.25), add(0.75));
        assert!(second.tx.preconditions.iter().all(|p| matches!(p, Precondition::Version { .. })));
        let (o, after) = s.dry_run_state(Frame(10), &draft, &first.tx);
        assert!(matches!(o, pe_seams::Outcome::Applied { .. }), "{o:?}");
        let friction = |snap: &WorldSnapshot| {
            snap.fields(id(20), "Body").unwrap().into_iter().find(|(k, _)| k == "friction").map(|(_, v)| v)
        };
        assert_eq!(friction(&after), Some(pe_world::Value::F(0.25)));
        let o = crate::outcome::dry_run(&after, &r2, &second, &pe_world::Draft { seq: 2, ..draft });
        let f = o.refusal.expect("the second add is refused");
        assert_eq!(f.code, RefusalCode::StaleVersion);
        assert!(f.message.contains("Someone added a \"Body\" component"), "{}", f.message);
        assert_eq!(friction(&after), Some(pe_world::Value::F(0.25)));
        // A replace, then its undo: the old row is back.
        let (s, v) = world_with(|w| {
            w.set_member(ALICE, pe_world::perm::EDIT);
            let light = Light { kind: light_kind::POINT, intensity: 40.0, range: 8.0, ..Default::default() };
            w.insert(id(20), light).unwrap();
        });
        let r = resolved(
            &v,
            json!([{"op": "add", "id": "/Table/Lamp", "component": "Light", "replace": true, "fields": {"intensity": 1.0}}]),
        );
        let e = to_world(&r, Actor::Person, &s, None).unwrap();
        let (o, after) = s.dry_run_state(Frame(10), &draft, &e.tx);
        let pe_seams::Outcome::Applied { inverse: Some(inverse), .. } = o else { panic!("{o:?}") };
        let intensity = |snap: &WorldSnapshot| {
            snap.fields(id(20), "Light").unwrap().into_iter().find(|(k, _)| k == "intensity").map(|(_, v)| v)
        };
        assert_eq!(intensity(&after), Some(pe_world::Value::F(1.0)));
        let undo = Transaction::decode(&inverse).unwrap();
        let (o, back) = after.dry_run_state(Frame(11), &pe_world::Draft { seq: 3, ..draft }, &undo);
        assert!(matches!(o, pe_seams::Outcome::Applied { .. }), "{o:?}");
        assert_eq!(intensity(&back), Some(pe_world::Value::F(40.0)));
        // And the outcome reports the replace.
        let o = crate::outcome::dry_run(&s, &r, &e, &draft);
        let set = o.diff.expect("a diff").set;
        assert!(
            set.iter().any(|d| d.field == "Light.range" && d.before == json!(8.0) && d.after == json!(0.0)),
            "{set:?}"
        );
    }

    /// api.md 5.3.1a (@api-eng's review): an insert applied for real makes the subtree under its parent, the top
    /// placed by its position write with its stored rotation kept and a child's rows as the content holds them, and
    /// its undo removes the whole subtree. Where the top goes is named (`parent` or `root: true`, one of the two); a
    /// name the content brings keeps a create's rules; and a subtree over a change's caps says it can't be inserted.
    #[test]
    fn an_inserted_subtree_applies_and_its_undo_removes_it_whole() {
        use pe_sim::Component as _;
        use pe_world::content::Builder;
        let (s, v) = world_with(|w| w.set_member(ALICE, pe_world::perm::EDIT));
        let draft = pe_world::Draft {
            author: ALICE,
            line: pe_core::LineId(Hash([7; 32])),
            chain: pe_core::ChainId(Hash([9; 32])),
            seq: 1,
        };
        let row = |c: pe_sim::ComponentId, bytes: &[u8]| pe_world::edit::Row {
            component: c,
            instance: 0,
            bytes: bytes.to_vec(),
        };
        let turned = Transform {
            rot: [0.0, std::f32::consts::FRAC_1_SQRT_2, 0.0, std::f32::consts::FRAC_1_SQRT_2],
            ..Transform::local([0.0; 3])
        };
        let body = Body { kind: body_kind::DYNAMIC, friction: 0.25, grabbable: 1, ..Default::default() };
        let mut b = Builder::default();
        let top = b.top(&[row(pe_sim::ComponentId::TRANSFORM, bytemuck::bytes_of(&turned))], "Crate");
        b.child(
            top,
            &[
                row(pe_sim::ComponentId::TRANSFORM, bytemuck::bytes_of(&Transform::local([0.0, 0.5, 0.0]))),
                row(Body::ID, bytemuck::bytes_of(&body)),
            ],
            "Ball",
        );
        let content = b.finish();
        let at = json!({"relative_to": "parent", "offset": [0.0, 2.0, 0.0]});
        let r = resolved(
            &v,
            json!([{"op": "insert", "as": "$c", "parent": "/Shelf", "content": content.to_text(),
                "components": {"Transform": {"position": at}}}]),
        );
        let e = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap();
        let (o, after) = s.dry_run_state(Frame(10), &draft, &e.tx);
        let pe_seams::Outcome::Applied { created, inverse: Some(inverse), .. } = o else { panic!("{o:?}") };
        assert_eq!(created.len(), 2);
        let (crate_id, ball) = (created[0], created[1]);
        assert_eq!(after.parent(crate_id), Some(id(40)), "the top is under the shelf");
        assert_eq!(after.parent(ball), Some(crate_id), "the child under the top");
        assert_eq!(after.name(crate_id).as_deref(), Some("Crate"));
        let shelf = after.world_transform(id(40)).unwrap();
        let placed = after.world_transform(crate_id).unwrap();
        let up = after.offset_in(crate_id, Some(id(40))).unwrap();
        assert!(near(up.0, [0.0, 2.0, 0.0]), "{up:?} (shelf {shelf:?}, top {placed:?})");
        // The shelf is unturned, so the top's world rotation is its stored one.
        let kept = placed.rot.iter().zip(turned.rot).all(|(a, b)| (a - b).abs() < 1e-6);
        assert!(kept, "the stored rotation is kept: {:?}", placed.rot);
        let ball_body = after.fields(ball, "Body").unwrap();
        assert_eq!(Some(ball_body), s.decode_row_of(Body::ID, bytemuck::bytes_of(&body)), "a child's rows unchanged");
        let undo = Transaction::decode(&inverse).unwrap();
        let (o, back) = after.dry_run_state(Frame(11), &pe_world::Draft { seq: 2, ..draft }, &undo);
        assert!(matches!(o, pe_seams::Outcome::Applied { .. }), "{o:?}");
        assert!(!back.exists(crate_id) && !back.exists(ball), "the undo removes the whole subtree");
        assert_eq!(back.all().len(), s.all().len());

        // Where the top goes: neither, or both, is refused before anything else.
        let text = content.to_text();
        let refused = |ops: Json| resolve_tx(&v, &tx(ops), &|_| None).err().map(|f| (f.code, f.message));
        for op in [
            json!({"op": "insert", "content": text}),
            json!({"op": "insert", "content": text, "parent": "/Shelf", "root": true}),
        ] {
            let (code, msg) = refused(json!([op])).expect("refused");
            assert_eq!(code, RefusalCode::Invalid);
            assert!(msg.contains("\"root\": true"), "{msg}");
        }
        assert!(refused(json!([{"op": "insert", "content": text, "root": true}])).is_none(), "a root on purpose");
        let (code, msg) = refused(json!([{"op": "insert", "content": "pe1c:zz", "root": true}])).unwrap();
        assert_eq!(code, RefusalCode::Invalid);
        assert!(msg.contains("lower-case hex") && !msg.contains("Malformed"), "words, not Debug: {msg}");

        // A name the content brings keeps a create's rules.
        for bad in ["", "a/b"] {
            let mut b = Builder::default();
            b.top(&[], bad);
            let ops = json!([{"op": "insert", "parent": "/Shelf", "content": b.finish().to_text()}]);
            match to_world(&resolved(&v, ops), Actor::Person, &s, None) {
                Err(Unmapped::Refused(f)) => {
                    assert_eq!((f.code, f.op), (RefusalCode::Invalid, Some(0)), "{bad:?}");
                    assert!(f.message.contains("no `/`"), "{}", f.message);
                }
                other => panic!("{bad:?}: {other:?}"),
            }
        }

        // Over the caps: 60 distinct names are past the change's 1 KiB of names; 4,100 objects are past its edits.
        let mut b = Builder::default();
        let top = b.top(&[], "Box of names");
        for k in 0..60 {
            b.child(top, &[], &format!("a rather long name, number {k:03}"));
        }
        let ops = json!([{"op": "insert", "parent": "/Shelf", "content": b.finish().to_text()}]);
        match to_world(&resolved(&v, ops), Actor::Person, &s, None) {
            Err(Unmapped::Refused(f)) => {
                assert_eq!(f.code, RefusalCode::TooLarge);
                assert!(
                    f.message.contains("bytes of names") && f.message.contains("can't be inserted"),
                    "{}",
                    f.message
                );
            }
            other => panic!("{other:?}"),
        }
        let mut b = Builder::default();
        let top = b.top(&[], "Crate");
        for _ in 0..4_100 {
            b.child(top, &[], "Ball");
        }
        let ops = json!([{"op": "insert", "parent": "/Shelf", "content": b.finish().to_text()}]);
        let (code, msg) = refused(ops).expect("refused");
        assert_eq!(code, RefusalCode::TooLarge);
        assert!(msg.contains("4101 objects") && msg.contains("can't be inserted"), "{msg}");
    }

    /// The M1 recordings' `create`s gave a query's output back (`"material": null`, `"Name": {"text": …}`) and were
    /// refused as "not wired": a blob field takes null for none, and Name.text names (api.md 5.3).
    #[test]
    fn blob_fields_take_null_and_name_text_names() {
        let (s, v) = world();
        // A rename: the Name row with the new text, its bytes carried inline.
        let r = resolved(&v, json!([{"op": "set", "id": "/Table/Ball", "field": "Name.text", "value": "Red ball"}]));
        let e = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap();
        let name_id = s.component("Name").unwrap().id();
        assert!(
            matches!(&e.tx.ops[0], Op::SetRow { id: Target::Existing(o), row } if *o == id(30) && row.component == name_id)
        );
        assert_eq!(e.tx.inline, [b"Red ball".to_vec()]);
        // A non-string name, and a name past NAME_MAX, are invalid.
        let f = |ops: Json| match to_world(&resolved(&v, ops), Actor::Person, &s, None) {
            Err(Unmapped::Refused(f)) => f.code,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            f(json!([{"op": "set", "id": "/Table/Ball", "field": "Name.text", "value": 3}])),
            RefusalCode::Invalid
        );
        for bad in ["", "a/b"] {
            assert_eq!(
                f(json!([{"op": "set", "id": "/Table/Ball", "field": "Name.text", "value": bad}])),
                RefusalCode::Invalid,
                "{bad:?}"
            );
        }
        let long = "n".repeat(NAME_MAX + 1);
        assert_eq!(
            f(json!([{"op": "set", "id": "/Table/Ball", "field": "Name.text", "value": long}])),
            RefusalCode::Invalid
        );
        // A create carrying a query's Name (same text) and a null blob: accepted; another text is refused.
        let create = |text: &str| {
            json!([{"op": "create", "parent": "/Table", "name": "Cube", "components": {
                "Name": {"text": text}, "Shape": {"kind": 1, "size": [0.1, 0.1, 0.1], "mesh": null}}}])
        };
        let e = to_world(&resolved(&v, create("Cube")), Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap();
        let Op::Create { rows, .. } = &e.tx.ops[0] else { panic!("{:?}", e.tx.ops) };
        assert_eq!(rows.iter().filter(|r| r.component == name_id).count(), 1, "one Name row");
        assert_eq!(f(create("Other")), RefusalCode::Invalid);
        // Any other blob value: invalid, never an internal error.
        let r = resolved(&v, json!([{"op": "set", "id": "/Table/Ball", "field": "Shape.mesh", "value": "b:00"}]));
        match to_world(&r, Actor::Person, &s, None) {
            Err(Unmapped::Refused(f)) => assert!(f.message.contains("null for none"), "{}", f.message),
            Err(Unmapped::Failed(e)) if e.code == ErrorCode::Internal => panic!("still not wired: {}", e.message),
            other => panic!("{other:?}"),
        }
    }

    /// The agent-tooling item: "double the balls" is one op, each source copied `count` times, `step` apart.
    #[test]
    fn a_bulk_duplicate_copies_each_source_count_times_step_apart() {
        let (s, v) = world_with(|w| {
            w.insert(id(20), Body { kind: body_kind::DYNAMIC, friction: 0.5, ..Default::default() }).unwrap();
        });
        // The Lamp and the Ball (both under the Table, at its local [0, 1, 0]), two copies each, 0.5 m apart in x.
        let r = resolved(
            &v,
            json!([{"op": "duplicate", "where": {"child_of": "/Table"}, "max": 10, "count": 2, "step": [0.5, 0.0, 0.0]}]),
        );
        assert_eq!(r.new_count, 4);
        let e = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap();
        let copies: Vec<_> = e.tx.ops.iter().filter(|o| matches!(o, Op::Copy { .. })).collect();
        assert_eq!(copies.len(), 4);
        assert!(
            matches!(copies[0], Op::Copy { source: Target::Existing(o), parent: Target::Existing(p), .. } if *o == id(20) && *p == id(10))
        );
        // Each copy's position, in the Table's frame: the source's [0, 1, 0] plus k steps.
        let xs: Vec<[f64; 3]> = e.tx.ops.iter().filter(|o| matches!(o, Op::SetField { .. })).map(f3).collect();
        assert_eq!(xs.len(), 4, "{:?}", e.tx.ops);
        for (x, want) in xs.iter().zip([[0.5, 1.0, 0.0], [1.0, 1.0, 0.0], [0.5, 1.0, 0.0], [1.0, 1.0, 0.0]]) {
            assert!(near(*x, want), "{x:?} != {want:?}");
        }
        // `as` names one copy only; count is 1..=1000.
        let refused = |ops: Json| resolve_tx(&v, &tx(ops), &|_| None).map(|_| ()).unwrap_err().code;
        assert_eq!(
            refused(json!([{"op": "duplicate", "id": "/Table/Ball", "as": "$b", "count": 2}])),
            RefusalCode::Invalid
        );
        assert_eq!(refused(json!([{"op": "duplicate", "id": "/Table/Ball", "count": 0}])), RefusalCode::Invalid);
        // Without step: one copy where its source is, as before.
        let r = resolved(&v, json!([{"op": "duplicate", "id": "/Table/Ball", "as": "$b"}]));
        let e = to_world(&r, Actor::Person, &s, None).unwrap();
        assert_eq!(e.tx.ops.len(), 1);
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
            match to_world(&r, Actor::Person, &s, None) {
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
                matches!(to_world(&r, Actor::Person, &s, None), Err(Unmapped::Failed(e)) if e.code == ErrorCode::Internal)
            );
        };
        // A whole Transform row isn't written yet; on an object with a Transform (every one) `add` is refused first.
        refused(
            json!([{"op": "add", "id": "/Table/Ball", "component": "Transform", "fields": {"scale": [1, 1, 1]}}]),
            "Transform",
        );
        failed(
            json!([{"op": "add", "id": "/Table/Ball", "component": "Transform", "replace": true, "fields": {"scale": [1, 1, 1]}}]),
        );
        // A create without a template takes a Transform: a default row, then its fields as writes.
        let r = resolved(
            &v,
            json!([{"op": "create", "parent": "/Table", "name": "x", "components": {"Transform": {"scale": [2, 2, 2]}}}]),
        );
        let w = to_world(&r, Actor::Person, &s, None).expect("mapped");
        let Op::Create { rows, .. } = &w.tx.ops[0] else { panic!("{:?}", w.tx.ops) };
        assert!(rows.iter().any(|r| r.component == pe_sim::ComponentId::TRANSFORM), "{rows:?}");
        assert!(matches!(w.tx.ops[1], Op::SetField { .. }), "{:?}", w.tx.ops);
        // A version this API didn't give out.
        let m = crate::query::mem::Mem::pit(0);
        let r = resolve_tx(&m, &tx(json!([{"op": "delete", "id": "/Table"}])), &|_| None).unwrap();
        assert!(
            matches!(to_world(&r, Actor::Person, &s, None), Err(Unmapped::Refused(f)) if f.code == RefusalCode::Invalid)
        );
    }
}
