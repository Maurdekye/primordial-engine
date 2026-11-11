//! The engine side (api.md 6.1), as a library: start the pipe and/or HTTP transports over a WorldApi, issue a
//! fresh token, and launch the harness with `PE_MCP_TOKEN`, `PE_MCP_PIPE` and/or `PE_MCP_URL` in its environment.
//! pe-app owns the world and the token store and calls this (api.md 6.1, O9).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use pe_seams::world_api::Scope;

use crate::auth::{Grant, TokenStore};
use crate::http::HttpServer;
use crate::server::Api;

#[derive(Clone, Debug)]
pub struct HostOpts {
    pub pipe: bool,
    pub http: bool,
    /// The HTTP Origin allow-list (empty: refuse every Origin).
    pub origins: Vec<String>,
    pub scopes: Vec<Scope>,
    pub principal: String,
    /// What the launched harness's token acts as.
    pub grant: Grant,
    pub ttl: Duration,
    /// The harness command line; empty for none.
    pub cmd: Vec<String>,
}

impl HostOpts {
    pub fn new(grant: Grant) -> Self {
        HostOpts {
            pipe: false,
            http: false,
            origins: vec![],
            scopes: crate::auth::ALL_SCOPES.to_vec(),
            principal: "agent".into(),
            grant,
            ttl: Duration::from_secs(3600),
            cmd: vec![],
        }
    }
}

pub struct Hosted {
    pub pipe: Option<String>,
    pub http: Option<HttpServer>,
    pub store: Arc<TokenStore>,
    pub child: Option<tokio::process::Child>,
}

impl Hosted {
    /// Wait for the harness to exit (forever if none was launched); then every token is revoked.
    pub async fn wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        let r = match self.child.as_mut() {
            Some(c) => c.wait().await.map(Some),
            None => std::future::pending().await,
        };
        self.store.revoke_all();
        r
    }
}

impl Drop for Hosted {
    fn drop(&mut self) {
        self.store.revoke_all();
    }
}

/// Start the transports and launch the harness. Requires `pipe` or `http`.
pub async fn start(api: Api, o: &HostOpts) -> io::Result<Hosted> {
    if !o.pipe && !o.http {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "MCP hosting needs a pipe and/or HTTP transport"));
    }
    let store = Arc::new(TokenStore::new());
    let mut cmd = o.cmd.first().map(|c| {
        let mut c = tokio::process::Command::new(c);
        c.args(&o.cmd[1..]).kill_on_drop(true);
        c
    });
    let pipe = if o.pipe {
        let path = crate::pipe::serve_pipe(api.clone(), store.clone())?;
        eprintln!("pe-mcp: pipe {path}");
        if let Some(c) = cmd.as_mut() {
            c.env("PE_MCP_PIPE", &path);
        }
        Some(path)
    } else {
        None
    };
    let http = if o.http {
        let h = crate::http::serve_http(api.clone(), store.clone(), o.origins.clone()).await?;
        eprintln!("pe-mcp: http {} (ipv6 {})", h.url, h.ipv6);
        if let Some(c) = cmd.as_mut() {
            c.env("PE_MCP_URL", &h.url);
        }
        Some(h)
    } else {
        None
    };
    let child = match cmd {
        Some(mut c) => {
            // A fresh token per launch (design 17.1: rotation at every harness launch and resume).
            let (_, secret) = store.issue(&o.principal, &o.scopes, o.grant.clone(), o.ttl);
            c.env("PE_MCP_TOKEN", secret.expose());
            Some(c.spawn().map_err(|e| io::Error::new(e.kind(), format!("launch {:?}: {e}", o.cmd[0])))?)
        }
        None => None,
    };
    Ok(Hosted { pipe, http, store, child })
}
