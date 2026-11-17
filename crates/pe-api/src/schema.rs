//! `schema` (api.md 5.6): the world's component types and templates, from the newest snapshot's registry.
//!
//! - Without a type or template: each component type `{name, summary}` the API shows (`crate::names`: the engine's,
//!   `Node` left out, and each script's active version's, tags left out, with `script`), the world's templates (the
//!   direct children of the root `Templates` folder, as `create`'s `template` names them), and the schema `version`,
//!   a hash of the registry and of which versions are active. `since: <version>` answers `unchanged` when it still
//!   matches.
//! - `type` is a name as a read with no object takes it: a bare name two visible types share is refused with their
//!   qualified forms, and the answer names a script's type by its full form.
//! - With a type: a JSON Schema per field, under the name `set` takes (`Transform.position` for the stored `pos`;
//!   fields whose names start with `_` are internal and left out), with `x-unit`, `x-sync`, `x-protected` and
//!   `x-read-only` annotations. Units come from the registry (`FieldDesc::unit`, pe-sim's canonical spelling, e.g.
//!   `kg/m^3`; world-model.md 3.6); summaries from a table of the engine's own components.
//! - With a template: what it holds, as a tree of names and types (at most `HOLDS_MAX` objects).

use std::collections::BTreeMap;

use pe_seams::world_api::{ApiError, Brief, Json, Schema, SchemaQuery};
use pe_sim::{ComponentDesc, FieldDesc, FieldKind};
use pe_world::WorldSnapshot;
use serde_json::json;

use crate::names::{NameError, Names};
use crate::resolve::asset_text;

/// The most objects a template's `holds` lists.
pub const HOLDS_MAX: usize = 200;

/// The schema version: a hash of every component description, in registry order, of each script's active version
/// (a version's types are registered at its commit but shown from its activation, api.md 7), and of the template
/// list, so `since` also notices a template added, removed or renamed (@chunks-reviewer's Low 1).
pub fn version(snap: &WorldSnapshot) -> String {
    let mut h = pe_core::hash::Hasher::new(0x5045_4150_4953_4348); // "PEAPISCH"
    for d in snap.schema() {
        h.write_u64(d.schema_hash());
    }
    for (a, m) in snap.modules() {
        for b in a.iter().chain(&m.version.entry.0.0) {
            h.write_u64(u64::from(*b));
        }
        h.write_u64(u64::from(m.version.op));
    }
    for (id, name) in templates(snap) {
        h.write_u64(id.to_u128() as u64);
        h.write_u64((id.to_u128() >> 64) as u64);
        h.write_u64(name.len() as u64);
        for b in name.bytes() {
            h.write_u64(u64::from(b));
        }
    }
    format!("s:{:016x}", h.finish())
}

/// What the engine's own components are for.
fn summary(name: &str) -> Option<&'static str> {
    Some(match name {
        "Transform" => "Where an object is: position, rotation and scale, relative to its parent.",
        "Name" => "An object's display name.",
        "Shape" => "A collision shape at the object's transform; mass comes from its density.",
        "Body" => "A rigid body: its shapes are its own plus those of descendants without a Body.",
        "BodyState" => "The simulated state of a body at cut frames, written by the engine.",
        "Appearance" => "How an object is drawn.",
        "Light" => "A light source.",
        "Hand" => "A member's hand, moved only by its owner's inputs.",
        "SimSize" => "The size the simulation range uses for this object, in metres.",
        "Protection" => "Protects the object and its subtree: only the owner may take, copy or re-protect them.",
        _ => return None,
    })
}

/// The engine writes these; an edit is refused.
fn read_only(component: &str) -> bool {
    matches!(component, "BodyState" | "Hand")
}

/// The name `set` takes for a stored field.
pub fn shown(component: &str, field: &str) -> String {
    match (component, field) {
        ("Transform", "pos") => "position".into(),
        _ => field.into(),
    }
}

fn int(min: i128, max: i128) -> Json {
    json!({ "type": "integer", "minimum": min, "maximum": max })
}

fn kind_schema(k: FieldKind) -> Json {
    match k {
        FieldKind::U8 => int(0, u8::MAX.into()),
        FieldKind::U16 => int(0, u16::MAX.into()),
        FieldKind::U32 => int(0, u32::MAX.into()),
        FieldKind::U64 => int(0, u64::MAX.into()),
        FieldKind::I8 => int(i8::MIN.into(), i8::MAX.into()),
        FieldKind::I16 => int(i16::MIN.into(), i16::MAX.into()),
        FieldKind::I32 => int(i32::MIN.into(), i32::MAX.into()),
        FieldKind::I64 => int(i64::MIN.into(), i64::MAX.into()),
        FieldKind::F32 | FieldKind::F64 => json!({ "type": "number", "description": "finite" }),
        FieldKind::Bool => json!({ "type": "boolean" }),
        FieldKind::ObjectRef => json!({ "type": ["string", "null"], "description": "an object id (\"o:…\"), or null" }),
        FieldKind::MemberKey => json!({ "type": "string", "description": "a member id (\"m:…\")" }),
        FieldKind::EntryRef => json!({ "type": "string", "description": "an entry id (\"e:…\")" }),
        FieldKind::Hash => json!({ "type": "string", "pattern": "^[0-9a-f]{64}$" }),
        FieldKind::BlobRef | FieldKind::SimBlobRef => {
            json!({ "type": "string", "description": "a blob; not yet settable through the API" })
        }
        FieldKind::Pos => json!({
            "type": "object",
            "description": "a position, as query reports it (an offset in a frame); not yet settable through the API",
        }),
    }
}

fn field_schema(d: &ComponentDesc, f: &FieldDesc) -> (String, Json) {
    let name = shown(d.name(), &f.name);
    if f.color.is_some() {
        // A colour, written and read as one (api.md 4.4): never a bare list, which carries no colour space.
        let alpha = f.count == 4;
        let hex = if alpha { "^#([0-9a-fA-F]{6}|[0-9a-fA-F]{8})$" } else { "^#[0-9a-fA-F]{6}$" };
        let list =
            |min: u32| json!({ "type": "array", "items": { "type": "number" }, "minItems": min, "maxItems": f.count });
        let s = json!({
            "description": crate::colour::expected(f.count as usize),
            "x-color": "linear-srgb",
            "x-sync": "synced",
            "oneOf": [
                { "type": "string", "pattern": hex },
                { "type": "object", "properties": { "srgb": list(3) }, "required": ["srgb"], "additionalProperties": false },
                { "type": "object", "properties": { "linear": list(f.count) }, "required": ["linear"], "additionalProperties": false },
            ],
        });
        return (name, s);
    }
    let mut s = kind_schema(f.kind);
    if f.count > 1 {
        s = json!({ "type": "array", "items": s, "minItems": f.count, "maxItems": f.count });
    }
    let o = s.as_object_mut().expect("an object");
    if !f.unit.is_empty() {
        o.insert("x-unit".into(), json!(f.unit));
    }
    o.insert("x-sync".into(), json!("synced"));
    if d.name() == "Protection" {
        o.insert("x-protected".into(), json!(true));
    }
    if read_only(d.name()) {
        o.insert("x-read-only".into(), json!(true));
    }
    if (d.name(), f.name.as_str()) == ("Transform", "rot") {
        o.insert("description".into(), json!("a unit quaternion [x, y, z, w]; query also shows rotation_deg"));
    }
    (name, s)
}

fn brief(names: &Names, d: &ComponentDesc) -> Brief {
    let fields: Vec<String> =
        d.fields().iter().filter(|f| !f.name.starts_with('_')).map(|f| shown(d.name(), &f.name)).collect();
    let script = names.script_of(d.id());
    let what = match script {
        Some(_) => None,
        None => summary(d.name()),
    };
    let summary = match what {
        Some(s) => format!("{s} Fields: {}.", fields.join(", ")),
        None => format!("Fields: {}.", fields.join(", ")),
    };
    Brief { name: d.name().to_string(), summary, script: script.map(|a| asset_text(&a)) }
}

/// The world's templates: the direct children of the one root named `Templates` (none if there are two).
fn templates(snap: &WorldSnapshot) -> Vec<(pe_core::ObjectId, String)> {
    let folders: Vec<_> = snap.roots().into_iter().filter(|r| snap.name(*r).as_deref() == Some("Templates")).collect();
    let [folder] = folders[..] else { return vec![] };
    snap.children(folder).into_iter().filter_map(|c| Some((c, snap.name(c)?))).collect()
}

/// An object's component types as responses name them (`crate::names`).
fn types_of(snap: &WorldSnapshot, names: &Names, id: pe_core::ObjectId) -> Vec<String> {
    let attached = names.attached(snap, id);
    names.on(snap, id).into_iter().filter_map(|c| names.shown_on(&attached, c)).collect()
}

/// A template's subtree as `{name, types, children}`, depth first, at most `HOLDS_MAX` objects in all.
fn holds(snap: &WorldSnapshot, names: &Names, id: pe_core::ObjectId, left: &mut usize) -> Json {
    *left -= 1;
    let mut children = vec![];
    for c in snap.children(id) {
        if *left == 0 {
            children.push(json!({ "more": true }));
            break;
        }
        children.push(holds(snap, names, c, left));
    }
    let mut o = json!({ "name": snap.name(id).unwrap_or_default(), "types": types_of(snap, names, id) });
    if !children.is_empty() {
        o["children"] = Json::Array(children);
    }
    o
}

pub fn schema(snap: &WorldSnapshot, q: &SchemaQuery) -> Result<Schema, ApiError> {
    let version = version(snap);
    if q.since.as_deref() == Some(version.as_str()) {
        return Ok(Schema::Unchanged { version });
    }
    let names = Names::new(snap);
    if let Some(t) = &q.type_name {
        let c = match names.resolve_all(t) {
            Ok(cs) if cs.len() == 1 => cs[0],
            Ok(cs) => {
                let all: Vec<String> = cs.iter().filter_map(|c| names.full(*c)).collect();
                return Err(ApiError::arg("type", NameError::Ambiguous(all).message(t)));
            }
            Err(NameError::Unknown) => {
                return Err(ApiError::arg(
                    "type",
                    format!("There is no component type {t:?}; schema without a type lists them."),
                ));
            }
            Err(e) => return Err(ApiError::arg("type", e.message(t))),
        };
        let d = snap.component_desc(c).expect("a visible type is registered");
        let fields: BTreeMap<String, Json> =
            d.fields().iter().filter(|f| !f.name.starts_with('_')).map(|f| field_schema(d, f)).collect();
        let summary = brief(&names, d).summary;
        let name = match names.script_of(c) {
            Some(_) => names.full(c).expect("visible"),
            None => d.name().to_string(),
        };
        return Ok(Schema::Type { version, name, summary, fields });
    }
    if let Some(name) = &q.template {
        let found: Vec<_> = templates(snap).into_iter().filter(|(_, n)| n == name).collect();
        let id = match found[..] {
            [(id, _)] => id,
            [] => {
                return Err(ApiError::arg(
                    "template",
                    format!("There is no template {name:?}; schema without a template lists them."),
                ));
            }
            // As resolve refuses `create` with this name (ambiguous), so the two never disagree.
            _ => {
                return Err(ApiError::arg(
                    "template",
                    format!("{} templates are named {name:?}; rename one.", found.len()),
                ));
            }
        };
        let mut left = HOLDS_MAX;
        let holds = holds(snap, &names, id, &mut left);
        let summary = format!("Holds {} object(s).", HOLDS_MAX - left);
        return Ok(Schema::Template { version, name: name.clone(), summary, holds });
    }
    let types = snap.schema().filter(|d| names.visible(d.id())).map(|d| brief(&names, d)).collect();
    let templates = templates(snap)
        .into_iter()
        .map(|(id, name)| Brief {
            summary: format!("Types: {}.", types_of(snap, &names, id).join(", ")),
            name,
            script: None,
        })
        .collect();
    Ok(Schema::List { version, types, templates })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_core::{EntryRef, Frame, Hash, MemberKey, Offset, Pos};
    use pe_seams::world_api::ErrorCode;
    use pe_sim::Transform;
    use pe_world::components::{Name, Protection};
    use pe_world::{BlobStore, World, perm};

    const ALICE: MemberKey = MemberKey([1; 32]);

    fn id(n: u128) -> pe_core::ObjectId {
        pe_core::ObjectId::from_u128(n)
    }

    /// A Table, and a "box" template holding a "lid".
    fn snap() -> std::sync::Arc<WorldSnapshot> {
        let mut w = World::new(1, BlobStore::default());
        w.set_member(ALICE, perm::EDIT);
        for (n, name, parent) in
            [(10u128, "Table", None), (40, "Templates", None), (50, "box", Some(40)), (60, "lid", Some(50))]
        {
            let text = w.blobs().put(name.as_bytes());
            let v = pe_seams::Version { entry: EntryRef(Hash([n as u8; 32])), op: 0 };
            w.create(id(n), parent.map(id), ALICE, v).unwrap();
            w.insert(id(n), Transform::at(Pos::ORIGIN.checked_add(Offset([0.0, 0.0, 0.0])).unwrap())).unwrap();
            w.insert(id(n), Name { text }).unwrap();
        }
        w.insert(id(10), Protection { owner: ALICE }).unwrap();
        w.set_frame(Frame(3));
        w.snapshot(Frame(3))
    }

    #[test]
    fn the_list_names_types_and_templates_with_a_stable_version() {
        let s = snap();
        let Schema::List { version, types, templates } = schema(&s, &SchemaQuery::default()).unwrap() else { panic!() };
        assert!(version.starts_with("s:"));
        let names: Vec<_> = types.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"Transform") && names.contains(&"Protection"));
        assert!(!names.contains(&"Node"), "pe-sim's internal Node is left out");
        assert_eq!(templates.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["box"]);
        let again = SchemaQuery { since: Some(version.clone()), ..Default::default() };
        assert_eq!(schema(&s, &again).unwrap(), Schema::Unchanged { version: version.clone() });
        // A template added changes the version.
        let mut w = World::new(1, BlobStore::default());
        w.set_member(ALICE, perm::EDIT);
        for (n, name, parent) in [(40u128, "Templates", None), (50, "box", Some(40)), (70, "ball", Some(40))] {
            let text = w.blobs().put(name.as_bytes());
            let v = pe_seams::Version { entry: EntryRef(Hash([n as u8; 32])), op: 0 };
            w.create(id(n), parent.map(id), ALICE, v).unwrap();
            w.insert(id(n), Name { text }).unwrap();
        }
        w.set_frame(Frame(3));
        assert_ne!(super::version(&w.snapshot(Frame(3))), version);
    }

    #[test]
    fn a_type_gives_a_json_schema_per_field_with_annotations() {
        let s = snap();
        let q = SchemaQuery { type_name: Some("Transform".into()), ..Default::default() };
        let Schema::Type { fields, .. } = schema(&s, &q).unwrap() else { panic!() };
        assert_eq!(fields["position"]["x-unit"], "m");
        assert!(!fields["rot"].as_object().unwrap().contains_key("x-unit"), "no unit, no annotation");
        assert_eq!(fields["position"]["x-sync"], "synced");
        assert!(!fields.contains_key("pos"), "the stored name is shown as set takes it");
        // Units come from the registry, in its canonical spelling (world-model.md 3.6).
        let q = SchemaQuery { type_name: Some("Shape".into()), ..Default::default() };
        let Schema::Type { fields, .. } = schema(&s, &q).unwrap() else { panic!() };
        assert_eq!(fields["density"]["x-unit"], "kg/m^3");
        let q = SchemaQuery { type_name: Some("Protection".into()), ..Default::default() };
        let Schema::Type { fields, .. } = schema(&s, &q).unwrap() else { panic!() };
        assert!(fields.values().all(|f| f["x-protected"] == true));
        let q = SchemaQuery { type_name: Some("BodyState".into()), ..Default::default() };
        if let Ok(Schema::Type { fields, .. }) = schema(&s, &q) {
            assert!(fields.values().all(|f| f["x-read-only"] == true));
        }
        let q = SchemaQuery { type_name: Some("Node".into()), ..Default::default() };
        assert_eq!(schema(&s, &q).unwrap_err().code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn a_template_says_what_it_holds() {
        let s = snap();
        let q = SchemaQuery { template: Some("box".into()), ..Default::default() };
        let Schema::Template { holds, .. } = schema(&s, &q).unwrap() else { panic!() };
        assert_eq!(holds["name"], "box");
        assert_eq!(holds["children"][0]["name"], "lid");
        let q = SchemaQuery { template: Some("nope".into()), ..Default::default() };
        assert_eq!(schema(&s, &q).unwrap_err().field.as_deref(), Some("template"));
    }
}
