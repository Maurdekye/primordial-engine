//! The nine tools over a real rmcp client/server pair, against the mock world (api.md 6.2, 6.3).

use std::sync::Arc;

use pe_mcp::auth::{Grant, Principal};
use pe_mcp::mock::MockWorld;
use pe_mcp::server::{Auth, WorldServer};
use pe_seams::world_api::{MemberId, Producer, ProducerKind, Scope};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

type Client = RunningService<RoleClient, ()>;

fn grant() -> Grant {
    Grant {
        member: MemberId([1; 32]),
        producer: Producer {
            kind: ProducerKind::Agent,
            harness: Some("claude-code".into()),
            principal: None,
            requested_by: None,
        },
        confirm: false,
    }
}

async fn client_as(world: Arc<MockWorld>, scopes: &[Scope]) -> Client {
    let (s, c) = tokio::io::duplex(1 << 20);
    let server = WorldServer::new(world, Auth::Fixed(Principal::local("agent", scopes, grant())));
    tokio::spawn(async move {
        if let Ok(r) = server.serve(s).await {
            let _ = r.waiting().await;
        }
    });
    ().serve(c).await.expect("client handshake")
}

async fn client() -> (Arc<MockWorld>, Client) {
    let w = Arc::new(MockWorld::new(5));
    let c = client_as(w.clone(), &pe_mcp::auth::ALL_SCOPES).await;
    (w, c)
}

async fn call(c: &Client, name: &str, args: Value) -> CallToolResult {
    let mut p = CallToolRequestParams::new(name.to_string());
    if let Value::Object(m) = args {
        p = p.with_arguments(m);
    }
    c.call_tool(p).await.unwrap_or_else(|e| panic!("{name}: transport error {e}"))
}

async fn ok(c: &Client, name: &str, args: Value) -> Value {
    let r = call(c, name, args.clone()).await;
    assert_ne!(r.is_error, Some(true), "{name} {args} failed: {:?}", r.structured_content);
    r.structured_content.unwrap_or_else(|| panic!("{name}: no structured content"))
}

/// The envelope of a failed call; asserts its code, and that the text content carries the same object.
async fn err(c: &Client, name: &str, args: Value, code: &str) -> Value {
    let r = call(c, name, args.clone()).await;
    assert_eq!(r.is_error, Some(true), "{name} {args} should fail with {code}, got {:?}", r.structured_content);
    let e = r.structured_content.unwrap_or_else(|| panic!("{name}: error without structured content"));
    assert_eq!(e["code"], json!(code), "{name} {args}: {e}");
    let text = r.content[0].as_text().expect("text content").text.clone();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), e);
    e
}

#[tokio::test]
async fn nine_tools_with_underscore_names_and_clean_schemas() {
    let (_w, c) = client().await;
    let tools = c.list_all_tools().await.unwrap();
    let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "code_check",
            "history_list",
            "history_revert",
            "session_info",
            "view_capture",
            "world_apply",
            "world_query",
            "world_schema",
            "world_watch"
        ]
    );
    for t in &tools {
        assert!(t.name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'), "{}", t.name);
        let s = serde_json::to_string(&t.input_schema).unwrap();
        for f in ["\"uint32\"", "\"uint64\"", "\"uint8\"", "\"uint16\"", "\"double\""] {
            assert!(!s.contains(f), "{} keeps format {f}", t.name);
        }
    }
}

#[tokio::test]
async fn the_tool_list_stays_within_its_budget() {
    // api.md 6.2: under 5,000 tokens. A byte proxy until the Claude Code count is recorded: about 4 bytes per token.
    let (_w, c) = client().await;
    let tools = c.list_all_tools().await.unwrap();
    let bytes = serde_json::to_string(&tools).unwrap().len();
    assert!(bytes < 20_000, "the tool list is {bytes} bytes, over the 5,000-token proxy");
}

#[tokio::test]
async fn read_only_tokens_see_and_call_only_read_tools() {
    let c = client_as(Arc::new(MockWorld::new(1)), &[Scope::Read]).await;
    let tools = c.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 6);
    let e = err(&c, "world_apply", json!({"label": "x", "ops": []}), "forbidden").await;
    assert_eq!(e["details"]["need"], "edit");
}

#[tokio::test]
async fn query_pages_and_session_reports_the_caller() {
    let (_w, c) = client().await;
    let s = ok(&c, "session_info", json!({})).await;
    assert_eq!(s["me"]["principal"], "agent");
    assert_eq!(s["me"]["producer"]["harness"], "claude-code");
    assert_eq!(s["kept"], true);
    let q = ok(&c, "world_query", json!({"where": {"has": "Ball"}, "limit": 3})).await;
    assert_eq!(q["objects"].as_array().unwrap().len(), 3);
    let more = q["more"].clone();
    let q2 = ok(&c, "world_query", json!({"where": {"has": "Ball"}, "limit": 3, "cursor": more})).await;
    assert_eq!(q2["objects"].as_array().unwrap().len(), 2);
    assert!(q2.get("more").is_none());
    let v = &q["objects"][0]["version"];
    assert!(v.as_str().unwrap().starts_with("v:"));
}

#[tokio::test]
async fn apply_accepts_and_dry_run_changes_nothing() {
    let (w, c) = client().await;
    let o = ok(
        &c,
        "world_apply",
        json!({"label": "make the balls blue", "ops": [
            {"op": "set", "where": {"has": "Ball"}, "max": 1000, "field": "Appearance.color", "value": "#2b5bd2"}
        ]}),
    )
    .await;
    assert_eq!(o["status"], "accepted");
    assert_eq!(o["count"], 5);
    let d = ok(&c, "world_apply", json!({"label": "try it", "mode": "dry_run", "ops": []})).await;
    assert_eq!(d["dry_run"], true);
    assert_eq!(*w.applied.lock().unwrap(), vec!["make the balls blue".to_string()]);
}

#[tokio::test]
async fn a_refused_change_is_an_error_with_the_winner() {
    let (_w, c) = client().await;
    let e = err(&c, "world_apply", json!({"label": "refuse", "ops": []}), "stale_version").await;
    assert!(e["message"].as_str().unwrap().contains("Bob"));
    assert_eq!(e["details"]["status"], "refused");
    assert_eq!(e["details"]["winner"]["by"]["name"], "Bob");
    assert_eq!(e["details"]["winner"]["producer"]["kind"], "hand");
    assert!(e["details"]["next"].as_str().is_some());
    // A protection refusal names its code.
    err(&c, "world_apply", json!({"label": "x", "ops": [{"op": "delete", "id": "/Lamp (right)"}]}), "protected").await;
    // A selector over its max.
    err(&c, "world_apply", json!({"label": "x", "ops": [{"op": "set", "where": {"has": "Ball"}, "max": 2, "field": "Appearance.color", "value": "#000000"}]}), "too_many").await;
}

#[tokio::test]
async fn bad_arguments_get_the_envelope() {
    let (_w, c) = client().await;
    err(&c, "world_apply", json!({"label": "x", "ops": [{"op": "explode"}]}), "invalid_argument").await;
    err(&c, "world_query", json!({"limit": 1000}), "invalid_argument").await;
    err(&c, "world_query", json!({"where": {"no_such_selector": 1}}), "invalid_argument").await;
    let e = err(&c, "world_schema", json!({"type": "Nope"}), "invalid_argument").await;
    assert_eq!(e["field"], "type");
}

#[tokio::test]
async fn watch_gives_a_cursor_and_reports_expiry() {
    let (_w, c) = client().await;
    let w = ok(&c, "world_watch", json!({})).await;
    assert_eq!(w["cursor"], "c:0.0");
    let w = ok(&c, "world_watch", json!({"cursor": "c:0.0", "wait_ms": 10})).await;
    assert_eq!(w["changes"], json!([]));
    let e = err(&c, "world_watch", json!({"cursor": "c:0.9"}), "cursor_expired").await;
    assert_eq!(e["details"]["fresh"], "c:0.0");
}

#[tokio::test]
async fn capture_returns_a_png_image_and_its_camera() {
    let (_w, c) = client().await;
    let r = call(&c, "view_capture", json!({"from": {"orbit": "/Table"}, "visible_list": true})).await;
    assert_ne!(r.is_error, Some(true), "{:?}", r.structured_content);
    let img = r.content[0].as_image().expect("an image content block first");
    assert_eq!(img.mime_type, "image/png");
    assert!(img.data.starts_with("iVBORw0KGgo"), "base64 of a PNG");
    let s = r.structured_content.unwrap();
    assert_eq!(s["visible"][0]["name"], "Table");
    assert_eq!(s["visible_approx"], true);
    err(&c, "view_capture", json!({"size": [4000, 4000]}), "invalid_argument").await;
}

#[tokio::test]
async fn code_check_and_history() {
    let (_w, c) = client().await;
    let d = ok(&c, "code_check", json!({"source": "pub fn spin() {}"})).await;
    assert_eq!(d["ok"], true);
    let d = ok(&c, "code_check", json!({"source": "compile_error!(\"no\");"})).await;
    assert_eq!(d["ok"], false);
    assert_eq!(d["diagnostics"][0]["kind"], "rustc");
    ok(&c, "world_apply", json!({"label": "put a red cube on the table", "ops": []})).await;
    let h = ok(&c, "history_list", json!({})).await;
    assert_eq!(h["entries"][0]["label"], "put a red cube on the table");
    let r = ok(&c, "history_revert", json!({"change": format!("x:{}", "07".repeat(32))})).await;
    assert_eq!(r["status"], "accepted");
    err(&c, "history_revert", json!({"change": "x:nothex"}), "invalid_argument").await;
}

#[tokio::test]
async fn the_instructions_say_the_world_is_live() {
    let (_w, c) = client().await;
    let info = c.peer_info().expect("server info");
    let text = info.instructions.clone().unwrap_or_default();
    assert!(text.contains("Time runs on its own"));
    assert!(!text.contains("world_step"));
}

// Unknown fields in an op are refused, not ignored (coordinator m8e173fe643ad, condition 3).
#[tokio::test]
async fn an_unknown_op_field_is_refused() {
    let (_w, c) = client().await;
    let ops = json!([{"op": "create", "parent": "/Table", "name": "Cup", "if_version": "any"}]);
    let e = err(&c, "world_apply", json!({"label": "x", "ops": ops}), "invalid_argument").await;
    assert!(e["message"].as_str().unwrap().contains("if_version"), "{e}");
}
