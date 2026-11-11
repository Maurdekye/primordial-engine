//! pe-api's snapshot port over pe-world's published frame (api.md 3, 4.4, 9.1): `WorldSnapshot` read through the
//! narrow `Snapshot` trait, with field values in the API's JSON forms and versions in their opaque text form.
//!
//! - Ids convert one to one (pe-seams' `From`s). Versions are `"v:"`, the entry's 64 hex digits, `"."` and the op
//!   index; `version_text` and `parse_version` are inverse, so a precondition goes back to pe-world exactly.
//! - Positions never come out as plain numbers (api.md 4.4). A root's `Transform.position` is its exact `"pos"`
//!   (chunk indices as decimal strings, the in-chunk offset as numbers); a child's is an offset from its parent.
//!   Asked for another frame (an object, `"me"`, `"world_origin"`), it is pe-world's `offset_in` from that frame,
//!   and the result names the frame it used.
//! - pe-world's field names are the API's, except `Transform.pos`, which the API calls `position`, and
//!   `Transform.rot`, which comes out as `rotation_deg` (Euler angles in Unity's order, applied Z, then X, then Y,
//!   each in [0, 360) as Unity's `eulerAngles`) and `rotation_quat` (`[x, y, z, w]`), api.md 4.4.

use std::collections::BTreeMap;
use std::sync::Arc;

use pe_core::{EntryRef, Hash, Offset, Pos};
use pe_seams::world_api::{EntryId, Frame, Json, MemberId, ObjRef, ObjectId, Version};
use pe_world::{BlobRef, Value, WorldSnapshot};
use serde_json::json;

use crate::ports::{ObjInfo, Snapshot};
use crate::query::{Named, resolve};

/// The API's text form of a pe-world version: `v:<entry hex>.<op>`.
pub fn version_text(v: &pe_seams::Version) -> Version {
    Version(format!("v:{}.{}", hex(&v.entry.0.0), v.op))
}

/// The inverse of `version_text`; `None` for anything else (including `"any"`).
pub fn parse_version(v: &Version) -> Option<pe_seams::Version> {
    let (h, op) = v.0.strip_prefix("v:")?.split_once('.')?;
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let mut e = [0u8; 32];
    for (i, b) in e.iter_mut().enumerate() {
        *b = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).ok()?;
    }
    // ASCII digits only (u32's parser would take a leading `+`), no leading zeros: one text per version.
    if op.is_empty() || !op.bytes().all(|b| b.is_ascii_digit()) || (op.len() > 1 && op.starts_with('0')) {
        return None;
    }
    Some(pe_seams::Version { entry: EntryRef(Hash(e)), op: op.parse().ok()? })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// An exact position as api.md 4.4's `"pos"`: chunk indices as decimal strings, the in-chunk offset as numbers.
pub fn pos_json(p: &Pos) -> Json {
    let c = p.chunk_index();
    let local = Pos::at_chunk(c).map(|o| (*p - o).0).unwrap_or([0.0; 3]);
    json!({ "pos": { "chunk": c.map(|x| x.to_string()), "offset": local } })
}

fn offset_json(frame: &str, o: &Offset) -> Json {
    json!({ "relative_to": frame, "offset": o.0 })
}

fn blob_json(r: &Option<BlobRef>) -> Json {
    match r {
        None => Json::Null,
        Some(r) => json!({ "hash": hex(&r.hash), "len": r.len }),
    }
}

/// A field value in the API's JSON (positions are handled by the caller, who knows the frame).
fn value_json(v: &Value) -> Json {
    match v {
        Value::U(x) => json!(x),
        Value::I(x) => json!(x),
        Value::F(x) => json!(x),
        Value::Bool(x) => json!(x),
        Value::Object(o) => o.map_or(Json::Null, |id| json!(ObjectId::from(id).to_string())),
        Value::Blob(r) | Value::SimBlob(r) => blob_json(r),
        Value::Pos(p) => pos_json(p),
        Value::Offset(o) => offset_json("parent", o),
        Value::Member(m) => json!(MemberId::from(*m).to_string()),
        Value::Entry(e) => json!(EntryId::from(*e).to_string()),
        Value::Hash(h) => json!(hex(&h.0)),
        Value::List(vs) => Json::Array(vs.iter().map(value_json).collect()),
    }
}

/// A unit quaternion `[x, y, z, w]` as Euler angles in degrees, in Unity's order: the rotation applies Z, then X,
/// then Y (q = qy·qx·qz), each in [0, 360). At gimbal lock (X = ±90°) Z is 0 and Y takes the whole turn.
pub fn euler_deg(q: [f32; 4]) -> [f64; 3] {
    let [x, y, z, w] = q.map(f64::from);
    let n = (x * x + y * y + z * z + w * w).sqrt();
    let (x, y, z, w) = if n > 0.0 { (x / n, y / n, z / n, w / n) } else { (0.0, 0.0, 0.0, 1.0) };
    // The rotation matrix's entries this needs (row, column).
    let m12 = 2.0 * (y * z - w * x);
    let (m02, m22) = (2.0 * (x * z + w * y), 1.0 - 2.0 * (x * x + y * y));
    let (m10, m11) = (2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z));
    let (m00, m20) = (1.0 - 2.0 * (y * y + z * z), 2.0 * (x * z - w * y));
    let ax = (-m12).clamp(-1.0, 1.0).asin();
    let (ay, az) = if m12.abs() < 1.0 - 1e-9 { (m02.atan2(m22), m10.atan2(m11)) } else { ((-m20).atan2(m00), 0.0) };
    [ax, ay, az].map(|r| {
        let d = r.to_degrees().rem_euclid(360.0);
        // Round away float noise so 90 reads as 90, and keep 360 as 0.
        let d = (d * 1e9).round() / 1e9;
        if d >= 360.0 { 0.0 } else { d }
    })
}

/// One published pe-world frame as pe-api's `Snapshot`, for one caller (whose head `"me"` names).
#[derive(Clone)]
pub struct WorldView {
    snap: Arc<WorldSnapshot>,
    me: Option<ObjectId>,
}

impl WorldView {
    pub fn new(snap: Arc<WorldSnapshot>, me: Option<ObjectId>) -> WorldView {
        WorldView { snap, me }
    }

    /// The frame `r` names, as pe-world's `offset_in` takes it: `Some(None)` for the world origin, `Some(Some(id))`
    /// for an object (or `"me"`), `None` if it names nothing.
    fn frame_of(&self, r: &ObjRef) -> Option<Option<pe_core::ObjectId>> {
        match r.0.as_str() {
            "world_origin" => Some(None),
            "me" => self.me.map(|m| Some(m.into())),
            _ => match resolve(self, r) {
                Named::One(id) => Some(Some(id.into())),
                _ => None,
            },
        }
    }
}

impl Snapshot for WorldView {
    fn frame(&self) -> Frame {
        self.snap.frame().0
    }
    fn final_frame(&self) -> Frame {
        self.snap.final_frame().0
    }
    fn roots(&self) -> Vec<ObjectId> {
        self.snap.roots().into_iter().map(ObjectId::from).collect()
    }
    fn all(&self) -> Vec<ObjectId> {
        self.snap.all().iter().copied().map(ObjectId::from).collect()
    }
    fn info(&self, id: ObjectId) -> Option<ObjInfo> {
        let i = self.snap.info(id.into())?;
        Some(ObjInfo {
            id,
            name: i.name.unwrap_or_default(),
            parent: i.parent.map(ObjectId::from),
            version: version_text(&i.version),
            creator: i.creator.into(),
            held_by: i.held_by.map(MemberId::from),
            protected_by: i.protected_by.map(MemberId::from),
            types: i.types,
        })
    }
    fn children(&self, id: ObjectId) -> Vec<ObjectId> {
        self.snap.children(id.into()).into_iter().map(ObjectId::from).collect()
    }
    fn fields(&self, id: ObjectId, component: &str, frame: &ObjRef) -> Option<BTreeMap<String, Json>> {
        let rows = self.snap.fields(id.into(), component)?;
        let mut out = BTreeMap::new();
        for (name, v) in rows {
            let place = component == "Transform" && name == "pos";
            let j = match (&v, place) {
                // The place in the frame asked for; "parent" (the default) keeps the stored form.
                (Value::Pos(_) | Value::Offset(_), true) if frame.0 != "parent" => {
                    match self.frame_of(frame).and_then(|f| self.snap.offset_in(id.into(), f)) {
                        Some(o) => offset_json(&frame.0, &o),
                        None => Json::Null,
                    }
                }
                _ => value_json(&v),
            };
            if component == "Transform"
                && name == "rot"
                && let Value::List(vs) = &v
            {
                let q: Vec<f32> =
                    vs.iter().filter_map(|x| if let Value::F(f) = x { Some(*f as f32) } else { None }).collect();
                if let Ok(q) = <[f32; 4]>::try_from(q) {
                    out.insert("rotation_deg".to_string(), json!(euler_deg(q)));
                    out.insert("rotation_quat".to_string(), json!(q));
                    continue;
                }
            }
            out.insert(if place { "position".to_string() } else { name }, j);
        }
        Some(out)
    }
    fn distance(&self, a: ObjectId, b: &ObjRef) -> Option<f64> {
        let other: pe_core::ObjectId = match b.0.as_str() {
            "me" => self.me?.into(),
            _ => match resolve(self, b) {
                Named::One(id) => id.into(),
                _ => return None,
            },
        };
        self.snap.distance(a.into(), other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{select, walk_path};
    use pe_core::{Frame as CoreFrame, MemberKey};
    use pe_seams::world_api::Selector;
    use pe_sim::Transform;
    use pe_world::components::{Name, Protection};
    use pe_world::{BlobStore, World};

    fn id(n: u128) -> pe_core::ObjectId {
        pe_core::ObjectId::from_u128(n)
    }
    fn aid(n: u128) -> ObjectId {
        id(n).into()
    }
    const ALICE: MemberKey = MemberKey([1; 32]);

    fn ver(b: u8, op: u32) -> pe_seams::Version {
        pe_seams::Version { entry: EntryRef(Hash([b; 32])), op }
    }
    fn at(x: f64, y: f64, z: f64) -> Pos {
        Pos::ORIGIN.checked_add(Offset([x, y, z])).unwrap()
    }

    /// A table at (5, 0, 0) (root, protected by Alice), a lamp 1 m above it, and the caller's head at (5, 1.6, 2).
    fn view() -> WorldView {
        let mut w = World::new(7, BlobStore::default());
        let table = w.blobs().put(b"Table");
        let lamp = w.blobs().put(b"Lamp");
        let head = w.blobs().put(b"Head");
        w.create(id(10), None, ALICE, ver(1, 0)).unwrap();
        w.insert(id(10), Transform::at(at(5.0, 0.0, 0.0))).unwrap();
        w.insert(id(10), Name { text: table }).unwrap();
        w.insert(id(10), Protection { owner: ALICE }).unwrap();
        w.create(id(30), Some(id(10)), ALICE, ver(1, 1)).unwrap();
        w.insert(id(30), Transform::local([0.0, 1.0, 0.0])).unwrap();
        w.insert(id(30), Name { text: lamp }).unwrap();
        w.create(id(40), None, ALICE, ver(2, 0)).unwrap();
        w.insert(id(40), Transform::at(at(5.0, 1.6, 2.0))).unwrap();
        w.insert(id(40), Name { text: head }).unwrap();
        w.set_frame(CoreFrame(9));
        WorldView::new(w.snapshot(CoreFrame(4)), Some(aid(40)))
    }

    fn quat_mul(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
        let [ax, ay, az, aw] = a;
        let [bx, by, bz, bw] = b;
        [
            aw * bx + ax * bw + ay * bz - az * by,
            aw * by - ax * bz + ay * bw + az * bx,
            aw * bz + ax * by - ay * bx + az * bw,
            aw * bw - ax * bx - ay * by - az * bz,
        ]
    }
    fn axis(i: usize, deg: f64) -> [f64; 4] {
        let h = deg.to_radians() / 2.0;
        let mut q = [0.0, 0.0, 0.0, h.cos()];
        q[i] = h.sin();
        q
    }
    /// Unity's Quaternion.Euler(x, y, z): q = qy * qx * qz.
    fn unity(x: f64, y: f64, z: f64) -> [f32; 4] {
        quat_mul(quat_mul(axis(1, y), axis(0, x)), axis(2, z)).map(|c| c as f32)
    }

    #[test]
    fn rotations_come_out_as_unity_euler_degrees() {
        let close = |a: [f64; 3], b: [f64; 3]| {
            a.iter().zip(b).all(|(x, y)| ((x - y + 180.0).rem_euclid(360.0) - 180.0).abs() < 1e-3)
        };
        assert_eq!(euler_deg([0.0, 0.0, 0.0, 1.0]), [0.0, 0.0, 0.0]);
        // +90 about +y, the spec's example (takes +z to +x).
        assert!(close(euler_deg(unity(0.0, 90.0, 0.0)), [0.0, 90.0, 0.0]));
        for (x, y, z) in [(20.0, 30.0, 10.0), (-45.0, 170.0, 300.0), (10.0, 0.0, 250.0), (89.0, 12.0, 34.0)] {
            assert!(close(euler_deg(unity(x, y, z)), [x, y, z]), "{x} {y} {z}: {:?}", euler_deg(unity(x, y, z)));
        }
        // Gimbal lock: X = 90 keeps the same rotation with Z folded into Y.
        let g = euler_deg(unity(90.0, 30.0, 0.0));
        assert!(close(g, [90.0, 30.0, 0.0]), "{g:?}");
        // Each angle in [0, 360).
        assert!(euler_deg(unity(-10.0, -20.0, -30.0)).iter().all(|d| (0.0..360.0).contains(d)));
    }

    #[test]
    fn versions_round_trip_as_text() {
        let v = ver(0xab, 7);
        let t = version_text(&v);
        assert_eq!(t.0, format!("v:{}.7", "ab".repeat(32)));
        assert_eq!(parse_version(&t), Some(v));
        for bad in [
            "any",
            "v:ab.7",
            &format!("v:{}.07", "ab".repeat(32)),
            &format!("v:{}.", "AB".repeat(32)),
            &format!("v:{}.+7", "ab".repeat(32)),
        ] {
            assert_eq!(parse_version(&Version(bad.to_string())), None, "{bad}");
        }
    }

    #[test]
    fn the_port_reads_pe_world() {
        let v = view();
        assert_eq!((v.frame(), v.final_frame()), (9, 4));
        assert_eq!(v.roots(), [aid(10), aid(40)]);
        assert_eq!(v.children(aid(10)), [aid(30)]);
        let t = v.info(aid(10)).unwrap();
        assert_eq!((t.name.as_str(), t.parent), ("Table", None));
        assert_eq!(t.version, version_text(&ver(1, 0)));
        assert_eq!(t.protected_by, Some(MemberId::from(ALICE)));
        assert_eq!(v.info(aid(30)).unwrap().protected_by, Some(MemberId::from(ALICE)));
        // Paths and selectors work over it as over the test snapshot.
        assert_eq!(walk_path(&v, "/Table/Lamp"), [aid(30)]);
        assert_eq!(select(&v, Some(&Selector::Has("Protection".into()))), [aid(10)]);
    }

    #[test]
    fn positions_come_out_in_the_frame_asked_for() {
        let v = view();
        let p =
            |id: u128, frame: &str| v.fields(aid(id), "Transform", &ObjRef(frame.into())).unwrap()["position"].clone();
        // A root: its exact pos. A child: an offset from its parent.
        assert_eq!(p(10, "parent"), json!({"pos": {"chunk": ["0", "0", "0"], "offset": [5.0, 0.0, 0.0]}}));
        assert_eq!(p(30, "parent"), json!({"relative_to": "parent", "offset": [0.0, 1.0, 0.0]}));
        // Other frames, named in the result.
        assert_eq!(p(30, "world_origin"), json!({"relative_to": "world_origin", "offset": [5.0, 1.0, 0.0]}));
        assert_eq!(p(30, "/Table"), json!({"relative_to": "/Table", "offset": [0.0, 1.0, 0.0]}));
        let me = p(30, "me");
        assert_eq!(me["relative_to"], "me");
        let o: Vec<f64> = me["offset"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        assert!((o[0] - 0.0).abs() < 1e-9 && (o[1] + 0.6).abs() < 1e-9 && (o[2] + 2.0).abs() < 1e-9, "{o:?}");
        // Rotations: degrees and the quaternion.
        let t = v.fields(aid(30), "Transform", &ObjRef("parent".into())).unwrap();
        assert_eq!(t["rotation_deg"], json!([0.0, 0.0, 0.0]));
        assert_eq!(t["rotation_quat"], json!([0.0, 0.0, 0.0, 1.0]));
        assert!(!t.contains_key("rot"));
        // A frame that names nothing gives null, not a number.
        assert_eq!(p(30, "/Nope"), Json::Null);
        // Distances, from an object and from "me".
        assert!((v.distance(aid(30), &ObjRef("/Table".into())).unwrap() - 1.0).abs() < 1e-9);
        assert!((v.distance(aid(30), &ObjRef("me".into())).unwrap() - (0.36f64 + 4.0).sqrt()).abs() < 1e-9);
    }
}
