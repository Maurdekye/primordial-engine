//! The fallback transport: Streamable HTTP on loopback (design 17.1), for clients that cannot use
//! stdio. The guard in front of rmcp enforces the engine's rules:
//!
//! * bind `127.0.0.1` and `::1` only, on an ephemeral port;
//! * `Host` must be `127.0.0.1:<port>`, `[::1]:<port>` or `localhost:<port>`, else 403 (DNS rebinding);
//! * a request with an `Origin` header is refused (403) unless the origin is on the allow-list, which
//!   is empty by default (MCP clients that are not browsers send no `Origin`);
//! * no CORS: no `Access-Control-*` header ever leaves the server;
//! * `Authorization: Bearer <token>` on every request, else 401; the token maps to the principal.
//!
//! The server runs stateless (no `Mcp-Session-Id`), so authority comes from each request's token
//! alone and a session id can never stand in for one.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::auth::TokenStore;
use crate::engine::EngineHandle;
use crate::server::{Auth, PeerServer};

struct Guard {
    hosts: Vec<String>,
    origins: Vec<String>,
    store: Arc<TokenStore>,
}

pub struct HttpServer {
    pub port: u16,
    /// The MCP endpoint, on 127.0.0.1.
    pub url: String,
    /// Whether `[::1]:port` is served too (false where IPv6 is unavailable).
    pub ipv6: bool,
    /// Every address listened on.
    pub addrs: Vec<SocketAddr>,
    stop: CancellationToken,
}

impl HttpServer {
    pub fn stop(&self) {
        self.stop.cancel();
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn deny(status: StatusCode, why: &'static str) -> Response {
    let mut r = (status, why).into_response();
    if status == StatusCode::UNAUTHORIZED {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    r
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token.trim())
}

async fn guard(State(g): State<Arc<Guard>>, mut req: Request, next: Next) -> Response {
    let h = req.headers();
    // Exactly one Host, equal to one of ours (case-insensitive).
    let mut hosts = h.get_all(header::HOST).iter();
    let host_ok = match (hosts.next(), hosts.next()) {
        (Some(v), None) => v.to_str().is_ok_and(|v| g.hosts.iter().any(|a| a.eq_ignore_ascii_case(v))),
        _ => false,
    };
    if !host_ok {
        return deny(StatusCode::FORBIDDEN, "host not allowed");
    }
    if let Some(o) = h.get(header::ORIGIN) {
        if !o.to_str().is_ok_and(|o| g.origins.iter().any(|a| a == o)) {
            return deny(StatusCode::FORBIDDEN, "origin not allowed");
        }
    }
    let Some(token) = bearer(h) else {
        return deny(StatusCode::UNAUTHORIZED, "bearer token required");
    };
    let Ok(principal) = g.store.authenticate(token) else {
        return deny(StatusCode::UNAUTHORIZED, "invalid token");
    };
    req.extensions_mut().insert(principal);
    let mut resp = next.run(req).await;
    let cors: Vec<_> = resp.headers().keys().filter(|k| k.as_str().starts_with("access-control-")).cloned().collect();
    for k in cors {
        resp.headers_mut().remove(k);
    }
    resp
}

/// Serve MCP over loopback HTTP. `origins` is the Origin allow-list (empty: refuse every Origin).
pub async fn serve_http(engine: EngineHandle, store: Arc<TokenStore>, origins: Vec<String>) -> std::io::Result<HttpServer> {
    let v4 = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
    let port = v4.local_addr()?.port();
    let v6 = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))).await.ok();
    let hosts = vec![format!("127.0.0.1:{port}"), format!("[::1]:{port}"), format!("localhost:{port}")];
    let stop = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_allowed_hosts(hosts.clone())
        .with_allowed_origins(origins.clone())
        .enforce_origin_validation()
        .with_cancellation_token(stop.child_token());
    let st = store.clone();
    let service = StreamableHttpService::new(
        move || Ok(PeerServer::new(engine.clone(), Auth::Http(st.clone()))),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let g = Arc::new(Guard { hosts, origins, store });
    let app = axum::Router::new().nest_service("/mcp", service).layer(middleware::from_fn_with_state(g, guard));
    let ipv6 = v6.is_some();
    let mut addrs = vec![];
    for l in std::iter::once(&v4).chain(v6.as_ref()) {
        addrs.push(l.local_addr()?);
    }
    for l in std::iter::once(v4).chain(v6) {
        let app = app.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).with_graceful_shutdown(async move { stop.cancelled().await }).await;
        });
    }
    Ok(HttpServer { port, url: format!("http://127.0.0.1:{port}/mcp"), ipv6, addrs, stop })
}
