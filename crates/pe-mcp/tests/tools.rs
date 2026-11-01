//! Every tool, its results and its errors, over a real MCP client/server pair (in-memory transport).

mod common;

use common::*;
use pe_mcp::model::Scope;
use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams};
use serde_json::{json, Value};

const READ_TOOLS: [&str; 7] = [
    "session_info",
    "session_members",
    "session_sync_status",
    "world_get_object",
    "world_get_state",
    "world_hashes",
    "world_interaction_status",
];

async fn step(c: &Client, ms: u64) -> Value {
    ok(c, "world_step", json!({ "ms": ms })).await
}

async fn touch(c: &Client, object: u32, impulse: [f64; 3]) -> Value {
    ok(c, "world_propose_interaction", json!({ "object": object, "action": "touch", "impulse": impulse })).await
}

#[tokio::test]
async fn tools_list_follows_scopes() {
    let e = engine();
    let all = client(&e).await;
    let mut names: Vec<String> = all.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(names.len(), 12, "{names:?}");
    let read = client_as(&e, "reader", &[Scope::Read]).await;
    let mut r: Vec<String> = read.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
    r.sort();
    assert_eq!(r, READ_TOOLS);
    let actor = client_as(&e, "actor", &[Scope::Interact]).await;
    let mut a: Vec<String> = actor.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
    a.sort();
    assert_eq!(a, ["world_interaction_status", "world_propose_interaction"]);
    let none = client_as(&e, "nobody", &[]).await;
    assert!(none.list_all_tools().await.unwrap().is_empty());
    // Every tool has an input schema, an output schema and a description.
    for t in all.list_all_tools().await.unwrap() {
        assert!(t.description.as_deref().is_some_and(|d| d.len() > 20), "{}", t.name);
        assert!(t.output_schema.is_some(), "{} has no output schema", t.name);
        assert_eq!(t.input_schema.get("type"), Some(&json!("object")), "{}", t.name);
        let text = serde_json::to_string(&(&t.input_schema, &t.output_schema)).unwrap();
        assert!(!text.contains("\"format\":\"uint") && !text.contains("\"format\":\"int"), "{}: non-standard format", t.name);
    }
}

#[tokio::test]
async fn forbidden_without_scope() {
    let e = engine();
    let read = client_as(&e, "reader", &[Scope::Read]).await;
    let x = err(&read, "world_step", json!({ "ms": 10 }), "forbidden").await;
    assert_eq!(x["details"]["need"], json!("control"));
    err(&read, "world_propose_interaction", json!({ "object": 0, "action": "touch", "impulse": [0, 1, 0] }), "forbidden").await;
    for t in ["scenario_load", "scenario_fault", "scenario_check"] {
        err(&read, t, json!({ "fault": "heal" }), "forbidden").await;
    }
    let actor = client_as(&e, "actor", &[Scope::Interact]).await;
    for t in READ_TOOLS.iter().filter(|t| **t != "world_interaction_status") {
        err(&actor, t, json!({ "object": 0, "ticket": 1, "from": 0, "to": 1 }), "forbidden").await;
    }
    // Interact alone may propose, and read its own tickets.
    let p = ok(&actor, "world_propose_interaction", json!({ "object": 0, "action": "touch", "impulse": [0, 1, 0] })).await;
    ok(&actor, "world_interaction_status", json!({ "ticket": p["ticket"] })).await;
}

#[tokio::test]
async fn unknown_tool_is_a_protocol_error() {
    let e = engine();
    let c = client(&e).await;
    let r = c.call_tool(CallToolRequestParams::new("world_teleport")).await;
    assert!(r.is_err(), "{r:?}");
}

#[tokio::test]
async fn session_info_reports_principal_and_session() {
    let e = engine();
    let c = client_as(&e, "alice", &[Scope::Read, Scope::Control]).await;
    let i = ok(&c, "session_info", json!({})).await;
    assert_eq!(i["principal"], "alice");
    assert_eq!(i["scopes"], json!(["read", "control"]));
    assert_eq!(i["backend"], "session-sim");
    assert_eq!((i["me"].clone(), i["members"].clone(), i["object_count"].clone()), (json!(1), json!([1, 2, 3, 4]), json!(32)));
    assert_eq!(i["frame"], 0);
    step(&c, 1000).await;
    let i = ok(&c, "session_info", json!({})).await;
    assert_eq!(i["time_ms"], 1000);
    assert!(i["frame"].as_u64().unwrap() >= 59 && i["conf"].as_u64().unwrap() > 0, "{i}");
}

#[tokio::test]
async fn world_get_state_pages_and_errors() {
    let e = engine();
    let c = client(&e).await;
    let s = ok(&c, "world_get_state", json!({})).await;
    assert_eq!((s["object_count"].clone(), s["objects"].as_array().unwrap().len(), s["next_offset"].clone()), (json!(32), 16, json!(16)));
    assert!(s.get("state_hash").is_none(), "no frame simulated yet");
    let s2 = ok(&c, "world_get_state", json!({ "offset": 16 })).await;
    assert_eq!(s2["objects"][0]["object"], 16);
    assert!(s2.get("next_offset").is_none());
    let s3 = ok(&c, "world_get_state", json!({ "offset": 1000, "limit": 5 })).await;
    assert!(s3["objects"].as_array().unwrap().is_empty());
    let all = ok(&c, "world_get_state", json!({ "limit": 256 })).await;
    assert_eq!(all["objects"].as_array().unwrap().len(), 32);
    let x = err(&c, "world_get_state", json!({ "limit": 257 }), "invalid_argument").await;
    assert_eq!(x["field"], "limit");
    err(&c, "world_get_state", json!({ "offset": -1 }), "invalid_argument").await;
    step(&c, 100).await;
    let s = ok(&c, "world_get_state", json!({})).await;
    assert_eq!(s["state_hash"].as_str().unwrap().len(), 16);
}

#[tokio::test]
async fn world_get_object_and_errors() {
    let e = engine();
    let c = client(&e).await;
    let o = ok(&c, "world_get_object", json!({ "object": 3 })).await;
    assert_eq!(o["object"], 3);
    assert_eq!(o["kind"], "box"); // kinds cycle box, sphere, capsule
    assert_eq!(o["rotation"].as_array().unwrap().len(), 4);
    assert_eq!(o["global_id"], "0000000100000003");
    let x = err(&c, "world_get_object", json!({ "object": 32 }), "unknown_object").await;
    assert_eq!(x["details"]["object_count"], 32);
    err(&c, "world_get_object", json!({}), "invalid_argument").await;
    err(&c, "world_get_object", json!({ "object": "three" }), "invalid_argument").await;
}

#[tokio::test]
async fn propose_touch_resolves_and_moves_the_object() {
    let e = engine();
    let c = client(&e).await;
    step(&c, 500).await; // let the pile settle a little
    let before = ok(&c, "world_get_object", json!({ "object": 20 })).await;
    let p = touch(&c, 20, [0.0, 30.0, 0.0]).await;
    assert_eq!(p["ticket"], 1);
    assert_eq!(p["entry"].as_str().unwrap().len(), 64);
    let st = ok(&c, "world_interaction_status", json!({ "ticket": 1 })).await;
    assert_eq!((st["state"].clone(), st["final"].clone()), (json!("pending"), json!(false)));
    let r = step(&c, 1000).await;
    let ch = r["changed"].as_array().unwrap();
    assert_eq!(ch.len(), 1, "{r}");
    assert_eq!((ch[0]["state"].clone(), ch[0]["final"].clone()), (json!("accepted"), json!(true)));
    let after = ok(&c, "world_get_object", json!({ "object": 20 })).await;
    assert_ne!(before["state_hash"], after["state_hash"]);
    // A step with nothing new reports no changes.
    let r = step(&c, 100).await;
    assert!(r["changed"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn propose_validates_arguments() {
    let e = engine();
    let c = client(&e).await;
    let p = |v: Value| v;
    for (args, field) in [
        (p(json!({ "object": 1, "action": "touch" })), "impulse"),
        (p(json!({ "object": 1, "action": "touch", "impulse": [0, 1000, 0] })), "impulse"),
        (p(json!({ "object": 1, "action": "touch", "impulse": [0, 1, 0], "target_mm": [0, 0, 0] })), "target_mm"),
        (p(json!({ "object": 1, "action": "hand", "target_mm": [0, 500, 0] })), "action"), // a box, not a hand
        (p(json!({ "object": 1, "action": "hand" })), "target_mm"),
        (p(json!({ "object": 1, "action": "hand", "impulse": [0, 1, 0] })), "impulse"),
    ] {
        let x = err(&c, "world_propose_interaction", args.clone(), "invalid_argument").await;
        assert_eq!(x["field"], field, "{args}: {x}");
    }
    err(&c, "world_propose_interaction", json!({ "object": 1, "action": "kick", "impulse": [0, 1, 0] }), "invalid_argument").await;
    err(&c, "world_propose_interaction", json!({ "object": 1, "action": "touch", "impulse": [0, 1] }), "invalid_argument").await;
    err(&c, "world_propose_interaction", json!({ "object": 99, "action": "touch", "impulse": [0, 1, 0] }), "unknown_object").await;
}

#[tokio::test]
async fn hands_in_the_ball_pit() {
    let e = engine();
    let c = client(&e).await;
    ok(&c, "scenario_load", json!({ "scene": "ballpit", "size": 50, "members": 2 })).await;
    let hand = 50; // member 1's hand follows the balls
    assert_eq!(ok(&c, "world_get_object", json!({ "object": hand })).await["kind"], "hand");
    let x = err(&c, "world_propose_interaction", json!({ "object": hand, "action": "touch", "impulse": [0, 1, 0] }), "invalid_argument").await;
    assert_eq!(x["field"], "action");
    err(&c, "world_propose_interaction", json!({ "object": hand, "action": "hand", "target_mm": [0, 200_000, 0] }), "invalid_argument").await;
    // Member 2's hand is not ours to move (review of 358cfb7, finding 1).
    let before = ok(&c, "world_get_object", json!({ "object": hand + 1 })).await;
    let x = err(&c, "world_propose_interaction", json!({ "object": hand + 1, "action": "hand", "target_mm": [0, 400, 0] }), "not_owner").await;
    assert_eq!((x["details"]["owner"].clone(), x["details"]["yours"].clone()), (json!(2), json!(hand)));
    ok(&c, "world_propose_interaction", json!({ "object": hand, "action": "hand", "target_mm": [0, 400, 0] })).await;
    let r = step(&c, 500).await;
    assert_eq!(r["changed"][0]["state"], "accepted", "{r}");
    let y = ok(&c, "world_get_object", json!({ "object": hand })).await["position"][1].as_f64().unwrap();
    assert!((y - 0.4).abs() < 0.01, "hand at y={y}");
    let after = ok(&c, "world_get_object", json!({ "object": hand + 1 })).await;
    assert_eq!(before["position"], after["position"], "member 2's hand did not move");
    ok(&c, "world_propose_interaction", json!({ "object": hand, "action": "hand", "target_mm": [0, 400, 0], "active": false })).await;
}

#[tokio::test]
async fn interaction_status_errors_and_isolation() {
    let e = engine();
    let a = client_as(&e, "alice", &Scope::ALL).await;
    let b = client_as(&e, "bob", &Scope::ALL).await;
    err(&a, "world_interaction_status", json!({ "ticket": 1 }), "unknown_ticket").await;
    touch(&a, 2, [0.0, 5.0, 0.0]).await;
    ok(&a, "world_interaction_status", json!({ "ticket": 1 })).await;
    // Tickets are per principal: bob cannot read alice's.
    err(&b, "world_interaction_status", json!({ "ticket": 1 }), "unknown_ticket").await;
    err(&a, "world_interaction_status", json!({}), "invalid_argument").await;
}

#[tokio::test]
async fn claims_reject_contending_touches() {
    let e = engine();
    let c = client(&e).await;
    // Two objects, three other members grabbing them 3 times a second each: our grabs must meet claims
    // (design D72: only a grab claims, and only a grab is refused).
    ok(&c, "scenario_load", json!({ "size": 1, "background_touches_per_s": 3, "background_grabs": true, "seed": 7 })).await;
    step(&c, 1100).await;
    let mut tickets = vec![];
    for _ in 0..30 {
        let g = json!({ "object": 0, "action": "grab", "impulse": [0.0, 1.0, 0.0] });
        tickets.push(ok(&c, "world_propose_interaction", g).await["ticket"].as_u64().unwrap());
        // Past our own claim (0.5 s) and its cooldown, so every grab contests afresh.
        step(&c, 700).await;
    }
    step(&c, 1000).await;
    let mut reasons = std::collections::BTreeMap::new();
    for t in tickets {
        let s = ok(&c, "world_interaction_status", json!({ "ticket": t })).await;
        assert_eq!(s["final"], true);
        *reasons.entry(format!("{}/{}", s["state"], s["reason"])).or_insert(0) += 1;
        if s["reason"] == "claimed" {
            assert!(s["by_member"].as_u64().unwrap() > 1 && s["until_frame"].is_u64(), "{s}");
        }
    }
    assert!(reasons.keys().any(|k| k.contains("claimed")), "{reasons:?}");
    assert!(reasons.keys().any(|k| k.contains("accepted")), "{reasons:?}");
}

#[tokio::test]
async fn rate_limit_on_unresolved_interactions() {
    let e = engine();
    let c = client(&e).await;
    // No step: none of these can resolve.
    for _ in 0..pe_mcp::engine::MAX_PENDING {
        touch(&c, 1, [0.0, 0.1, 0.0]).await;
    }
    let x = err(&c, "world_propose_interaction", json!({ "object": 1, "action": "touch", "impulse": [0, 0.1, 0] }), "rate_limited").await;
    assert_eq!(x["details"]["limit"], 64);
    step(&c, 1000).await;
    touch(&c, 1, [0.0, 0.1, 0.0]).await; // resolved and final now
}

#[tokio::test]
async fn world_step_arguments() {
    let e = engine();
    let c = client(&e).await;
    let r = ok(&c, "world_step", json!({ "frames": 30 })).await;
    assert_eq!((r["from_frame"].clone(), r["frame"].clone()), (json!(0), json!(30)));
    let r = ok(&c, "world_step", json!({ "frames": 1 })).await;
    assert_eq!(r["frame"], 31);
    let r = ok(&c, "world_step", json!({ "frames": 600 })).await;
    assert_eq!(r["frame"], 631);
    step(&c, 7).await; // off a frame boundary
    let f0 = ok(&c, "world_step", json!({ "frames": 1 })).await;
    let f1 = ok(&c, "world_step", json!({ "frames": 2 })).await;
    assert_eq!(f1["frame"].as_u64().unwrap(), f0["frame"].as_u64().unwrap() + 2);
    assert_eq!(step(&c, 1).await["time_ms"], f1["time_ms"].as_u64().unwrap() + 1);
    for (args, field) in [
        (json!({}), "ms"),
        (json!({ "ms": 10, "frames": 1 }), "ms"),
        (json!({ "ms": 0 }), "ms"),
        (json!({ "ms": 10_001 }), "ms"),
        (json!({ "frames": 0 }), "frames"),
        (json!({ "frames": 601 }), "frames"),
    ] {
        let x = err(&c, "world_step", args.clone(), "invalid_argument").await;
        assert_eq!(x["field"], field, "{args}");
    }
}

#[tokio::test]
async fn world_hashes_range() {
    let e = engine();
    let c = client(&e).await;
    step(&c, 2000).await;
    let h = ok(&c, "world_hashes", json!({ "from": 10, "to": 20 })).await;
    let f = h["frames"].as_array().unwrap();
    assert_eq!(f.len(), 10);
    assert_eq!(f[0]["frame"], 10);
    // Check hashes come every 30 frames (none in 10..20).
    assert!(h["checks"].as_array().unwrap().is_empty());
    let h = ok(&c, "world_hashes", json!({ "from": 0, "to": 600 })).await;
    let checks = h["checks"].as_array().unwrap();
    assert!(checks.len() >= 3, "{h}");
    assert!(checks.iter().all(|c| c["frame"].as_u64().unwrap() % 30 == 0));
    assert_eq!(h["frames"].as_array().unwrap().len() as u64, h["frames"].as_array().unwrap().last().unwrap()["frame"].as_u64().unwrap() + 1);
    err(&c, "world_hashes", json!({ "from": 20, "to": 20 }), "invalid_argument").await;
    err(&c, "world_hashes", json!({ "from": 0, "to": 601 }), "invalid_argument").await;
    err(&c, "world_hashes", json!({ "from": 0 }), "invalid_argument").await;
}

#[tokio::test]
async fn crash_pause_eviction_and_recovery() {
    let e = engine();
    let c = client(&e).await;
    ok(&c, "scenario_load", json!({ "window": 30 })).await;
    step(&c, 1000).await;
    let s = ok(&c, "session_sync_status", json!({})).await;
    assert_eq!((s["paused"].clone(), s["checks_bad"].clone()), (json!(false), json!(0)));
    assert!(s["checks_ok"].as_u64().unwrap() > 0, "{s}");
    let f = ok(&c, "scenario_fault", json!({ "fault": "crash", "member": 4 })).await;
    assert_eq!(f["active"], json!(["member 4 crashed"]));
    // The window (30 frames) is shorter than the silence threshold (45): the peer pauses first.
    step(&c, 650).await;
    let s = ok(&c, "session_sync_status", json!({})).await;
    assert_eq!(s["paused"], true, "{s}");
    let x = err(&c, "world_propose_interaction", json!({ "object": 1, "action": "touch", "impulse": [0, 1, 0] }), "not_ready").await;
    assert_eq!(x["details"]["window"], 30);
    // The silent member is voted out and the session resumes.
    let r = step(&c, 3000).await;
    let m = ok(&c, "session_members", json!({})).await;
    let m4 = &m["members"][3];
    assert_eq!(m4["member"], 4);
    assert!(m4["evicted"]["from_frame"].is_u64(), "{m} {r}");
    assert!(m["suspicions"].as_u64().unwrap() >= 1);
    assert!(!r["events"].as_array().unwrap().is_empty());
    let s = ok(&c, "session_sync_status", json!({})).await;
    assert_eq!(s["paused"], false, "{s}");
    touch(&c, 1, [0.0, 1.0, 0.0]).await;
    let k = ok(&c, "scenario_check", json!({})).await;
    assert_eq!((k["replicas_equal"].clone(), k["replay_equal"].clone()), (json!(true), json!(true)), "{k}");
    assert_eq!((k["live_members"].clone(), k["crashed_members"].clone()), (json!([1, 2, 3]), json!([4])));
    assert!(k["frames"].as_u64().unwrap() > 200);
}

#[tokio::test]
async fn driven_peer_crash() {
    let e = engine();
    let c = client(&e).await;
    ok(&c, "scenario_fault", json!({ "fault": "crash", "member": 1 })).await;
    err(&c, "world_propose_interaction", json!({ "object": 1, "action": "touch", "impulse": [0, 1, 0] }), "peer_crashed").await;
    step(&c, 100).await; // the others go on
    ok(&c, "scenario_check", json!({})).await;
}

#[tokio::test]
async fn scenario_fault_arguments() {
    let e = engine();
    let c = client(&e).await;
    for (args, field) in [
        (json!({ "fault": "crash" }), "member"),
        (json!({ "fault": "crash", "member": 9 }), "member"),
        (json!({ "fault": "link", "from": 1 }), "to"),
        (json!({ "fault": "link", "from": 2, "to": 2 }), "to"),
        (json!({ "fault": "link", "from": 1, "to": 2, "delay_ms": 5000 }), "delay_ms"),
        (json!({ "fault": "partition" }), "groups"),
        (json!({ "fault": "partition", "groups": [[1, 2], [2, 3]] }), "groups"),
        (json!({ "fault": "partition", "groups": [[1], []] }), "groups"),
        (json!({ "fault": "partition", "groups": [[1, 7]] }), "groups"),
        (json!({ "fault": "heal" }), "fault"),
    ] {
        let x = err(&c, "scenario_fault", args.clone(), "invalid_argument").await;
        assert_eq!(x["field"], field, "{args}: {x}");
    }
    err(&c, "scenario_fault", json!({ "fault": "flood" }), "invalid_argument").await;
    ok(&c, "scenario_fault", json!({ "fault": "crash", "member": 3 })).await;
    let x = err(&c, "scenario_fault", json!({ "fault": "crash", "member": 3 }), "invalid_argument").await;
    assert!(x["message"].as_str().unwrap().contains("already"));
}

#[tokio::test]
async fn partition_heal_and_slow_links_stay_consistent() {
    let e = engine();
    let c = client(&e).await;
    ok(&c, "scenario_load", json!({ "background_touches_per_s": 4, "jitter_ms": 10, "loss_pct": 5 })).await;
    step(&c, 1000).await;
    let f = ok(&c, "scenario_fault", json!({ "fault": "link", "from": 1, "to": 3, "delay_ms": 150, "jitter_ms": 30, "loss_pct": 20, "one_way": true })).await;
    assert_eq!(f["applied"], "link 1->3: 150 ms +-30 ms, 20% loss");
    touch(&c, 5, [1.0, 3.0, 0.0]).await;
    step(&c, 500).await;
    let f = ok(&c, "scenario_fault", json!({ "fault": "partition", "groups": [[1, 2], [3, 4]] })).await;
    assert_eq!(f["active"].as_array().unwrap().len(), 2, "{f}");
    let conf0 = step(&c, 1000).await["conf"].as_u64().unwrap();
    // No side has a majority view of every member's acks: confirmation stalls.
    let conf1 = step(&c, 500).await["conf"].as_u64().unwrap();
    assert_eq!(conf0, conf1);
    let f = ok(&c, "scenario_fault", json!({ "fault": "heal" })).await;
    assert_eq!(f["active"], json!(["link 1->3: 150 ms +-30 ms, 20% loss"]));
    let r = step(&c, 4000).await;
    assert!(r["conf"].as_u64().unwrap() > conf1, "{r}");
    let k = ok(&c, "scenario_check", json!({})).await;
    assert_eq!((k["replicas_equal"].clone(), k["replay_equal"].clone()), (json!(true), json!(true)), "{k}");
}

#[tokio::test]
async fn scenario_load_validates_and_resets() {
    let e = engine();
    let c = client(&e).await;
    touch(&c, 1, [0.0, 1.0, 0.0]).await;
    for (args, field) in [
        (json!({ "members": 9 }), "members"),
        (json!({ "members": 1 }), "members"),
        (json!({ "members": 3, "me": 4 }), "me"),
        (json!({ "scene": "ballpit", "size": 501 }), "size"),
        (json!({ "size": 9 }), "size"),
        (json!({ "delay_ms": 1001 }), "delay_ms"),
        (json!({ "loss_pct": 51 }), "loss_pct"),
        (json!({ "window": 10 }), "window"),
        (json!({ "background_touches_per_s": 31 }), "background_touches_per_s"),
    ] {
        let x = err(&c, "scenario_load", args.clone(), "invalid_argument").await;
        assert_eq!(x["field"], field, "{args}");
    }
    err(&c, "scenario_load", json!({ "scene": "volcano" }), "invalid_argument").await;
    // A failed load leaves the session as it was.
    ok(&c, "world_interaction_status", json!({ "ticket": 1 })).await;
    let i = ok(&c, "scenario_load", json!({ "members": 3, "me": 2, "scene": "pile", "size": 2 })).await;
    assert_eq!((i["me"].clone(), i["members"].clone(), i["object_count"].clone(), i["frame"].clone()), (json!(2), json!([1, 2, 3]), json!(8), json!(0)));
    err(&c, "world_interaction_status", json!({ "ticket": 1 }), "unknown_ticket").await;
}

#[tokio::test]
async fn same_scenario_same_run() {
    // Two independent engines, same parameters and proposals: identical hashes (design 17.3).
    let mut digests = vec![];
    for _ in 0..2 {
        let e = engine();
        let c = client(&e).await;
        ok(&c, "scenario_load", json!({ "background_touches_per_s": 5, "jitter_ms": 8, "loss_pct": 3, "seed": 42 })).await;
        step(&c, 700).await;
        touch(&c, 9, [2.0, 6.0, -1.0]).await;
        step(&c, 1500).await;
        let h = ok(&c, "world_hashes", json!({ "from": 0, "to": 120 })).await;
        digests.push(h["frames"].clone());
    }
    assert_eq!(digests[0], digests[1]);
    assert_eq!(digests[0].as_array().unwrap().len(), 120);
}

#[tokio::test]
async fn engine_panic_faults_the_session_until_reload() {
    let e = engine();
    let c = client(&e).await;
    let r = e.call(|en| {
        en.panic_for_test();
        Ok(())
    });
    let x = r.await.unwrap_err();
    assert_eq!(x.code, pe_mcp::model::ErrorCode::Internal);
    for t in ["world_get_state", "world_step", "session_members", "session_sync_status", "scenario_check"] {
        err(&c, t, json!({ "ms": 10 }), "session_faulted").await;
    }
    let i = ok(&c, "session_info", json!({})).await;
    assert!(i["faulted"].as_str().unwrap().contains("injected test panic"), "{i}");
    ok(&c, "scenario_load", json!({})).await;
    ok(&c, "world_step", json!({ "ms": 10 })).await;
}

#[tokio::test]
async fn resources_list_and_read() {
    let e = engine();
    let c = client(&e).await;
    let list = c.list_all_resources().await.unwrap();
    let mut uris: Vec<String> = list.iter().map(|r| r.uri.clone()).collect();
    uris.sort();
    assert_eq!(uris, ["primordial://session/members", "primordial://session/sync", "primordial://world/state"]);
    for uri in uris {
        let r = c.read_resource(ReadResourceRequestParams::new(uri.clone())).await.unwrap();
        let rmcp::model::ResourceContents::TextResourceContents { text, mime_type, .. } = &r.contents[0] else { panic!() };
        assert_eq!(mime_type.as_deref(), Some("application/json"));
        let v: Value = serde_json::from_str(text).unwrap();
        assert!(v.is_object(), "{uri}");
    }
    assert!(c.read_resource(ReadResourceRequestParams::new("primordial://world/secrets")).await.is_err());
    let none = client_as(&e, "nobody", &[Scope::Interact]).await;
    assert!(none.list_all_resources().await.unwrap().is_empty());
    assert!(none.read_resource(ReadResourceRequestParams::new("primordial://world/state")).await.is_err());
}

#[tokio::test]
async fn world_state_resource_holds_every_object() {
    // Review of 358cfb7, finding 2: the resource was cut at 256 objects.
    let e = engine();
    let c = client(&e).await;
    ok(&c, "scenario_load", json!({ "scene": "ballpit", "size": 500, "members": 2 })).await;
    let r = c.read_resource(ReadResourceRequestParams::new("primordial://world/state")).await.unwrap();
    let rmcp::model::ResourceContents::TextResourceContents { text, .. } = &r.contents[0] else { panic!() };
    let v: Value = serde_json::from_str(text).unwrap();
    assert_eq!((v["object_count"].clone(), v["objects"].as_array().unwrap().len()), (json!(502), 502));
    assert!(v.get("next_offset").is_none());
}

/// `schema/tools.json` is the documented schema; it must match what the server advertises.
/// Regenerate with UPDATE_SCHEMA=1.
#[tokio::test]
async fn schema_file_is_current() {
    let e = engine();
    let c = client(&e).await;
    let mut tools = c.list_all_tools().await.unwrap();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let text = serde_json::to_string_pretty(&tools).unwrap() + "\n";
    // PE_MCP_DIR overrides the compile-time crate path, for test binaries run under WSL.
    let dir = std::env::var("PE_MCP_DIR").unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string());
    let path = std::path::Path::new(&dir).join("schema/tools.json");
    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
    assert!(committed == text, "schema/tools.json is stale: run with UPDATE_SCHEMA=1");
}
