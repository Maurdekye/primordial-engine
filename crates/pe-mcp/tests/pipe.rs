//! The default transport (api.md 6.1, kept from the archived pe-mcp): named pipe / Unix socket, token handshake,
//! and the `serve-mock` + `bridge` binaries end to end.

use std::sync::Arc;
use std::time::Duration;

use pe_mcp::auth::TokenStore;
use pe_mcp::mock::MockWorld;
use pe_mcp::pipe;
use pe_seams::world_api::Scope;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

fn grant() -> pe_mcp::auth::Grant {
    pe_mcp::auth::Grant {
        member: pe_seams::world_api::MemberId([1; 32]),
        producer: pe_seams::world_api::Producer {
            kind: pe_seams::world_api::ProducerKind::Agent,
            harness: Some("test".into()),
            principal: None,
            requested_by: None,
        },
        confirm: false,
    }
}

fn setup() -> (Arc<MockWorld>, Arc<TokenStore>, String) {
    let world = Arc::new(MockWorld::new(5));
    let store = Arc::new(TokenStore::new());
    let path = pipe::serve_pipe(world.clone(), store.clone()).unwrap();
    (world, store, path)
}

async fn session_info(c: &rmcp::service::RunningService<rmcp::RoleClient, ()>) -> rmcp::model::CallToolResult {
    c.call_tool(CallToolRequestParams::new("session_info")).await.unwrap()
}

#[tokio::test]
async fn handshake_then_mcp_as_the_tokens_principal() {
    let (_e, store, path) = setup();
    let (_, t) = store.issue("piper", &[Scope::Read], grant(), Duration::from_secs(60));
    let conn = pipe::client_handshake(pipe::connect(&path).await.unwrap(), t.expose()).await.unwrap();
    let c = ().serve(conn).await.unwrap();
    let r = session_info(&c).await;
    let v = r.structured_content.unwrap();
    assert_eq!((v["me"]["principal"].clone(), v["me"]["scopes"].clone()), (json!("piper"), json!(["read"])));
    let r = c
        .call_tool(
            CallToolRequestParams::new("world_apply")
                .with_arguments(json!({"label": "x", "ops": []}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_eq!(r.structured_content.unwrap()["code"], "forbidden");
}

#[tokio::test]
async fn wrong_or_malformed_handshake_is_refused() {
    let (_e, store, path) = setup();
    store.issue("x", &pe_mcp::auth::ALL_SCOPES, grant(), Duration::from_secs(60));
    let e = pipe::client_handshake(pipe::connect(&path).await.unwrap(), &"0".repeat(64)).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    for first in [
        b"HELLO\n".to_vec(),
        vec![b'A'; 1000],
        b"PE-MCP-AUTH \n".to_vec(),
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n".to_vec(),
    ] {
        let mut s = pipe::connect(&path).await.unwrap();
        s.write_all(&first).await.unwrap();
        let mut reply = String::new();
        let mut r = tokio::io::BufReader::new(s);
        tokio::time::timeout(Duration::from_secs(10), r.read_line(&mut reply)).await.unwrap().unwrap();
        assert_eq!(reply, "ERR unauthorized\n");
        // And the engine closed the connection.
        let mut rest = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(10), r.read_to_end(&mut rest)).await.unwrap().unwrap_or(0);
        assert_eq!(n, 0);
    }
}

#[tokio::test]
async fn revocation_takes_effect_on_an_open_connection() {
    let (_e, store, path) = setup();
    let (id, t) = store.issue("agent", &pe_mcp::auth::ALL_SCOPES, grant(), Duration::from_secs(60));
    let conn = pipe::client_handshake(pipe::connect(&path).await.unwrap(), t.expose()).await.unwrap();
    let c = ().serve(conn).await.unwrap();
    assert_ne!(session_info(&c).await.is_error, Some(true));
    store.revoke(id);
    let r = session_info(&c).await;
    assert_eq!(r.is_error, Some(true));
    assert_eq!(r.structured_content.unwrap()["code"], "forbidden");
}

#[tokio::test]
async fn two_principals_share_one_world() {
    let (world, store, path) = setup();
    let (_, a) = store.issue("alice", &pe_mcp::auth::ALL_SCOPES, grant(), Duration::from_secs(60));
    let (_, b) = store.issue("bob", &[Scope::Read], grant(), Duration::from_secs(60));
    let ca =
        ().serve(pipe::client_handshake(pipe::connect(&path).await.unwrap(), a.expose()).await.unwrap()).await.unwrap();
    let cb =
        ().serve(pipe::client_handshake(pipe::connect(&path).await.unwrap(), b.expose()).await.unwrap()).await.unwrap();
    let r = ca
        .call_tool(
            CallToolRequestParams::new("world_apply")
                .with_arguments(json!({"label": "from alice", "ops": []}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true), "{r:?}");
    let v = session_info(&cb).await.structured_content.unwrap();
    assert_eq!((v["me"]["principal"].clone(), v["frame"].clone()), (json!("bob"), json!(1001)));
    assert_eq!(*world.applied.lock().unwrap(), vec!["from alice".to_string()]);
}

#[cfg(windows)]
#[tokio::test]
async fn pipe_dacl_admits_only_the_current_user() {
    let name = pipe::fresh_endpoint_name();
    let (_path, server) = pipe::bind(&name).unwrap();
    let sddl = pipe::dacl_of(&server).unwrap();
    let sid = pipe::current_user_sid().unwrap();
    // Protected DACL, one ACE: full access for this user (GA maps to FA on a pipe).
    assert_eq!(sddl, format!("D:P(A;;FA;;;{sid})"));
    // The name cannot be squatted: a second first instance fails.
    assert!(pipe::bind(&name).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn socket_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let (ep, _l) = pipe::bind(&pipe::fresh_endpoint_name()).unwrap();
    assert_eq!(std::fs::metadata(&ep.dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&ep.path).unwrap().permissions().mode() & 0o777, 0o600);
}

/// The binary under test. `PE_MCP_EXE` overrides the compile-time path, for test binaries built on
/// Windows and run under WSL.
fn exe() -> String {
    std::env::var("PE_MCP_EXE").unwrap_or_else(|_| env!("CARGO_BIN_EXE_pe-mcp").to_string())
}

/// `pe-mcp serve-mock --pipe -- pe-mcp bridge`: the engine launches the "harness" (here the bridge
/// itself), whose stdio we drive as an MCP client.
#[tokio::test]
async fn serve_launches_bridge_end_to_end() {
    let exe = exe();
    let mut child = tokio::process::Command::new(&exe)
        .args(["serve-mock", "--pipe", "--principal", "harness", "--", &exe, "bridge"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
    let c = ().serve((stdout, stdin)).await.unwrap();
    let v: Value = session_info(&c).await.structured_content.unwrap();
    assert_eq!(v["me"]["principal"], "harness");
    let r = c
        .call_tool(
            CallToolRequestParams::new("world_query")
                .with_arguments(json!({"where": {"has": "Ball"}}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_eq!(r.structured_content.unwrap()["objects"].as_array().unwrap().len(), 20);
    c.cancel().await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("serve exits with its harness")
        .unwrap();
    assert!(status.success(), "{status:?}");
}

#[tokio::test]
async fn bridge_without_environment_fails() {
    let out = tokio::process::Command::new(exe())
        .arg("bridge")
        .env_remove("PE_MCP_PIPE")
        .env_remove("PE_MCP_TOKEN")
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("PE_MCP_PIPE"));
}
