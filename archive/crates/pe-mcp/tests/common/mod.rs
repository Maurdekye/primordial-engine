#![allow(dead_code)]

use pe_mcp::auth::Principal;
use pe_mcp::engine::{Engine, EngineHandle};
use pe_mcp::model::Scope;
use pe_mcp::server::{Auth, PeerServer};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};

pub type Client = RunningService<RoleClient, ()>;

pub fn engine() -> EngineHandle {
    EngineHandle::spawn(Engine::with_default_scenario)
}

/// An MCP client connected over an in-memory pipe to a server with a fixed principal.
pub async fn client_as(engine: &EngineHandle, name: &str, scopes: &[Scope]) -> Client {
    let (s, c) = tokio::io::duplex(1 << 16);
    let server = PeerServer::new(engine.clone(), Auth::Fixed(Principal::local(name, scopes)));
    tokio::spawn(async move {
        if let Ok(r) = server.serve(s).await {
            let _ = r.waiting().await;
        }
    });
    ().serve(c).await.expect("client handshake")
}

pub async fn client(engine: &EngineHandle) -> Client {
    client_as(engine, "agent", &Scope::ALL).await
}

pub async fn call(c: &Client, name: &str, args: Value) -> CallToolResult {
    let mut p = CallToolRequestParams::new(name.to_string());
    if let Value::Object(m) = args {
        p = p.with_arguments(m);
    }
    c.call_tool(p).await.unwrap_or_else(|e| panic!("{name}: transport error {e}"))
}

/// The structured result of a successful call.
pub async fn ok(c: &Client, name: &str, args: Value) -> Value {
    let r = call(c, name, args.clone()).await;
    assert_ne!(r.is_error, Some(true), "{name} {args} failed: {:?}", r.structured_content);
    r.structured_content.unwrap_or_else(|| panic!("{name}: no structured content"))
}

/// The error object of a failed call; asserts it failed with `code`.
pub async fn err(c: &Client, name: &str, args: Value, code: &str) -> Value {
    let r = call(c, name, args.clone()).await;
    assert_eq!(r.is_error, Some(true), "{name} {args} should fail with {code}, got {:?}", r.structured_content);
    let e = r.structured_content.unwrap_or_else(|| panic!("{name}: error without structured content"));
    assert_eq!(e["code"], json!(code), "{name} {args}: {e}");
    // The text content carries the same object, for clients that ignore structuredContent.
    let text = r.content[0].as_text().expect("text content").text.clone();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), e);
    e
}
