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
    ApiError, ErrorCode, Json, MemberId, ObjRef, ObjectBrief, ObjectId, Refusal, RefusalCode, Version,
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
                "Nothing was submitted. A new script can't be attached in the same change: commit it without `attach`,                  then attach it once its `mine` item says `activated`."
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
                "Nothing was submitted. The script's first version isn't active yet: attach it once its `mine` item                  says `activated`."
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
                Op::Copy { source: target(*source), new: *new, parent: target(*parent), index: None, size: *size }
            }
            ROp::Set { target: t, fields } => {
                // One SetField per field, in field-name order (the map's), all on the same object; a position is
                // converted to the object's frame first.
                let on = on_for(target(*t), &tagged);
                for (k, v) in fields {
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
                let (name_row, r) = name_row(snap, i, name)?;
                if !blob_refs.contains(&r) {
                    blob_refs.push(r);
                    blobs.push(name.as_bytes().to_vec());
                }
                let me = Target::New(*new);
                let on = on_for(me, &tagged);
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
                        ops.push(Op::Create { new: *new, parent: target(*parent), index: None, rows });
                        writes
                    }
                    Some(tpl) => {
                        // The copy brings the template's rows; the name replaces its Name row, and the components
                        // given are field writes on the copy.
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
            ROp::Add { target: t, component, fields } => {
                let on = on_for(target(*t), &tagged);
                Op::SetRow { id: target(*t), row: whole_row(snap, &on, i, target(*t), component, fields)? }
            }
            ROp::Remove { target: t, component } => {
                let Obj::Existing(o) = t else { return Err(not_yet(i, "removing a row this change adds")) };
                let on = on_for(target(*t), &tagged);
                let desc = on.desc(snap, component).map_err(|m| {
                    let mut f = refusal(RefusalCode::Invalid, m);
                    f.op = Some(i as u32);
                    Unmapped::Refused(f)
                })?;
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
        failed(json!([{"op": "add", "id": "/Table/Ball", "component": "Transform", "fields": {"scale": [1, 1, 1]}}]));
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
