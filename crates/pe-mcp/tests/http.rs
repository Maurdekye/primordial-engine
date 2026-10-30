//! The loopback HTTP transport's rules (design 17.1), checked with raw HTTP/1.1 requests.

use std::sync::Arc;
use std::time::Duration;

use pe_mcp::auth::TokenStore;
use pe_mcp::engine::{Engine, EngineHandle};
use pe_mcp::http::{serve_http, HttpServer};
use pe_mcp::model::Scope;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(h, _)| h.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
    /// The JSON-RPC message in the body (plain JSON, or the data line of an SSE event).
    fn json(&self) -> Value {
        let t = self.body.lines().find_map(|l| l.strip_prefix("data:")).unwrap_or(&self.body);
        serde_json::from_str(t.trim()).unwrap_or_else(|e| panic!("body {:?}: {e}", self.body))
    }
}

async fn raw(addr: &str, req: String) -> Reply {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), s.read_to_end(&mut buf)).await.expect("reply").unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    // Undo chunked transfer encoding if present.
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let mut out = String::new();
        let mut rest = body;
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            out.push_str(&tail[..n]);
            rest = &tail[n + 2..];
        }
        out
    } else {
        body.to_string()
    };
    Reply { status, headers, body }
}

fn post(host: &str, extra: &[(&str, &str)], body: &Value) -> String {
    let b = body.to_string();
    let mut r = format!(
        "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        b.len()
    );
    for (k, v) in extra {
        r.push_str(&format!("{k}: {v}\r\n"));
    }
    r + "\r\n" + &b
}

fn init() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"raw","version":"0"}}})
}

fn call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}

struct Fixture {
    server: HttpServer,
    store: Arc<TokenStore>,
    token: String,
    host: String,
    addr: String,
}

async fn fixture(origins: Vec<String>) -> Fixture {
    let engine = EngineHandle::spawn(Engine::with_default_scenario);
    let store = Arc::new(TokenStore::new());
    let server = serve_http(engine, store.clone(), origins).await.unwrap();
    let (_, t) = store.issue("agent", &Scope::ALL, Duration::from_secs(600));
    let host = format!("127.0.0.1:{}", server.port);
    Fixture { addr: host.clone(), host, token: t.expose().to_string(), store, server }
}

impl Fixture {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

#[tokio::test]
async fn binds_loopback_only() {
    let f = fixture(vec![]).await;
    assert!(!f.server.addrs.is_empty());
    for a in &f.server.addrs {
        assert!(a.ip().is_loopback(), "{a}");
        assert_eq!(a.port(), f.server.port);
    }
    assert!(f.server.url.starts_with("http://127.0.0.1:"));
}

#[tokio::test]
async fn valid_request_is_served() {
    let f = fixture(vec![]).await;
    let r = raw(&f.addr, post(&f.host, &[("Authorization", &f.bearer())], &init())).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["result"]["serverInfo"]["name"], "primordial-engine");
    assert!(r.header("mcp-session-id").is_none(), "stateless: no session id");
    let r = raw(&f.addr, post(&f.host, &[("Authorization", &f.bearer()), ("MCP-Protocol-Version", "2025-06-18")], &call(2, "session_info", json!({})))).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["result"]["structuredContent"]["principal"], "agent");
    // localhost:<port> is accepted too.
    let r = raw(&f.addr, post(&format!("localhost:{}", f.server.port), &[("Authorization", &f.bearer())], &init())).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn ipv6_loopback_when_available() {
    let f = fixture(vec![]).await;
    if !f.server.ipv6 {
        eprintln!("no IPv6 loopback on this machine; skipped");
        return;
    }
    let addr = format!("[::1]:{}", f.server.port);
    let r = raw(&addr, post(&addr, &[("Authorization", &f.bearer())], &init())).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn foreign_host_is_refused() {
    let f = fixture(vec![]).await;
    let p = f.server.port;
    for host in [format!("evil.example:{p}"), format!("127.0.0.1:{}", p + 1), "127.0.0.1".into(), format!("127.0.0.1.evil.example:{p}"), format!("0.0.0.0:{p}")] {
        let r = raw(&f.addr, post(&host, &[("Authorization", &f.bearer())], &init())).await;
        assert_eq!(r.status, 403, "Host {host}");
    }
    // Two Host headers.
    let r = raw(&f.addr, post(&f.host, &[("Host", "evil.example"), ("Authorization", &f.bearer())], &init())).await;
    assert_ne!(r.status, 200);
}

#[tokio::test]
async fn origin_is_refused_unless_allowed() {
    let f = fixture(vec![]).await;
    for o in ["http://evil.example", "null", &format!("http://127.0.0.1:{}", f.server.port)] {
        let r = raw(&f.addr, post(&f.host, &[("Authorization", &f.bearer()), ("Origin", o)], &init())).await;
        assert_eq!(r.status, 403, "Origin {o}");
        assert!(r.header("access-control-allow-origin").is_none());
    }
    let g = fixture(vec!["http://tool.local:8080".into()]).await;
    let r = raw(&g.addr, post(&g.host, &[("Authorization", &g.bearer()), ("Origin", "http://tool.local:8080")], &init())).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.header("access-control-allow-origin").is_none(), "no CORS even for allowed origins");
    let r = raw(&g.addr, post(&g.host, &[("Authorization", &g.bearer()), ("Origin", "http://tool.local:8081")], &init())).await;
    assert_eq!(r.status, 403);
}

#[tokio::test]
async fn preflight_gets_no_cors() {
    let f = fixture(vec![]).await;
    let req = format!(
        "OPTIONS /mcp HTTP/1.1\r\nHost: {}\r\nOrigin: http://evil.example\r\nAccess-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
        f.host
    );
    let r = raw(&f.addr, req).await;
    assert_eq!(r.status, 403);
    assert!(r.headers.iter().all(|(k, _)| !k.to_ascii_lowercase().starts_with("access-control-")));
}

#[tokio::test]
async fn token_is_required_and_checked() {
    let f = fixture(vec![]).await;
    let r = raw(&f.addr, post(&f.host, &[], &init())).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    let wrong = format!("Bearer {}", "0".repeat(64));
    for auth in [wrong.as_str(), "Bearer", "Basic dXNlcjpwYXNz", &format!("Token {}", f.token), &f.token] {
        let r = raw(&f.addr, post(&f.host, &[("Authorization", auth)], &init())).await;
        assert_eq!(r.status, 401, "Authorization {auth:.20}");
    }
    // Never from the URL.
    let req = post(&f.host, &[], &init()).replace("POST /mcp ", &format!("POST /mcp?token={} ", f.token));
    assert_eq!(raw(&f.addr, req).await.status, 401);
    // Case-insensitive scheme.
    let r = raw(&f.addr, post(&f.host, &[("Authorization", &format!("bearer {}", f.token))], &init())).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn revoked_token_stops_working() {
    let f = fixture(vec![]).await;
    let p = f.store.authenticate(&f.token).unwrap();
    assert_eq!(raw(&f.addr, post(&f.host, &[("Authorization", &f.bearer())], &init())).await.status, 200);
    f.store.revoke(p.token.unwrap());
    assert_eq!(raw(&f.addr, post(&f.host, &[("Authorization", &f.bearer())], &init())).await.status, 401);
}

#[tokio::test]
async fn scopes_come_from_the_token() {
    let f = fixture(vec![]).await;
    let (_, read) = f.store.issue("reader", &[Scope::Read], Duration::from_secs(600));
    let auth = format!("Bearer {}", read.expose());
    let h = [("Authorization", auth.as_str()), ("MCP-Protocol-Version", "2025-06-18")];
    let r = raw(&f.addr, post(&f.host, &h, &call(3, "world_step", json!({ "ms": 10 })))).await;
    let res = &r.json()["result"];
    assert_eq!(res["isError"], true, "{}", r.body);
    assert_eq!(res["structuredContent"]["code"], "forbidden");
    let r = raw(&f.addr, post(&f.host, &h, &json!({"jsonrpc":"2.0","id":4,"method":"tools/list"}))).await;
    assert_eq!(r.json()["result"]["tools"].as_array().unwrap().len(), 7, "{}", r.body);
    let r = raw(&f.addr, post(&f.host, &h, &call(5, "session_info", json!({})))).await;
    assert_eq!(r.json()["result"]["structuredContent"]["principal"], "reader");
}
