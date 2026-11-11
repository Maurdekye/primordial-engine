//! Unknown fields in a transaction's ops are refused, not ignored (coordinator m8e173fe643ad, condition 3): serde
//! skips fields it doesn't know, so `"if_version": "any"` on a `create` would silently do nothing. `Op` is an
//! internally tagged enum with a flattened `Target`, where serde's `deny_unknown_fields` doesn't work, so the check
//! reads each op's allowed keys from `Op`'s own JSON schema and stays right as the types change.
//!
//! Every JSON entry point that deserialises a `Transaction` must call `unknown_op_field` first. Today that is
//! pe-mcp's `world_apply`; typed Rust callers (pe-app, the inspector) can't carry unknown fields.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use pe_seams::world_api::{Json, Op};

/// Allowed top-level keys per op name, from `Op`'s schema.
fn table() -> &'static BTreeMap<String, BTreeSet<String>> {
    static T: OnceLock<BTreeMap<String, BTreeSet<String>>> = OnceLock::new();
    T.get_or_init(|| {
        let root = serde_json::to_value(schemars::schema_for!(Op)).expect("a schema is JSON");
        let mut t = BTreeMap::new();
        for v in variants(&root, &root) {
            let mut keys = BTreeSet::new();
            collect(&root, v, &mut keys, 0);
            let name = v
                .pointer("/properties/op/const")
                .or_else(|| v.pointer("/properties/op/enum/0"))
                .and_then(Json::as_str)
                .map(str::to_string);
            if let Some(name) = name {
                t.insert(name, keys);
            }
        }
        assert!(!t.is_empty(), "Op's schema lists its variants");
        t
    })
}

fn deref<'a>(root: &'a Json, s: &'a Json) -> &'a Json {
    match s.get("$ref").and_then(Json::as_str).and_then(|r| r.rsplit('/').next()) {
        Some(name) => root
            .pointer(&format!("/$defs/{name}"))
            .or_else(|| root.pointer(&format!("/definitions/{name}")))
            .unwrap_or(s),
        None => s,
    }
}

fn variants<'a>(root: &'a Json, s: &'a Json) -> Vec<&'a Json> {
    let s = deref(root, s);
    for k in ["oneOf", "anyOf"] {
        if let Some(a) = s.get(k).and_then(Json::as_array) {
            return a.iter().map(|v| deref(root, v)).collect();
        }
    }
    vec![s]
}

fn collect(root: &Json, s: &Json, keys: &mut BTreeSet<String>, depth: u32) {
    if depth > 8 {
        return;
    }
    let s = deref(root, s);
    if let Some(p) = s.get("properties").and_then(Json::as_object) {
        keys.extend(p.keys().cloned());
    }
    for k in ["allOf", "anyOf", "oneOf"] {
        if let Some(a) = s.get(k).and_then(Json::as_array) {
            for v in a {
                collect(root, v, keys, depth + 1);
            }
        }
    }
}

/// The first op (index and key) carrying a field its op type doesn't have, if any. Ops that aren't objects or have
/// an unknown `op` are left to deserialization, which refuses them.
pub fn unknown_op_field(ops: &[Json]) -> Option<(usize, String)> {
    let t = table();
    for (i, op) in ops.iter().enumerate() {
        let Some(o) = op.as_object() else { continue };
        let Some(allowed) = o.get("op").and_then(Json::as_str).and_then(|n| t.get(n)) else { continue };
        if let Some(k) = o.keys().find(|k| !allowed.contains(k.as_str())) {
            return Some((i, k.clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_fields_are_found_and_known_ones_pass() {
        let ok = json!([
            {"op": "create", "as": "$c", "parent": "/Table", "name": "Cup", "template": "box", "components": {}},
            {"op": "set", "where": {"has": "Ball"}, "max": 10, "field": "A.b", "value": 1, "if_version": "any"},
            {"op": "reparent", "id": "/Cup", "parent": "/", "index": 0, "keep": "world_pose"},
            {"op": "duplicate", "id": "$c", "as": "$d", "parent": "/Table"},
            {"op": "delete", "id": "/Cup"},
            {"op": "code", "id": "$s", "source": "", "attach": []},
        ]);
        assert_eq!(unknown_op_field(ok.as_array().unwrap()), None);
        let bad =
            json!([{"op": "delete", "id": "/Cup"}, {"op": "create", "parent": "/", "name": "x", "if_version": "any"}]);
        assert_eq!(unknown_op_field(bad.as_array().unwrap()), Some((1, "if_version".into())));
        let typo = json!([{"op": "set", "id": "/Cup", "feild": "A.b", "value": 1}]);
        assert_eq!(unknown_op_field(typo.as_array().unwrap()), Some((0, "feild".into())));
    }
}
