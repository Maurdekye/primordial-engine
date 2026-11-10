//! `pe-mcp` command line.
//!
//! ```text
//! pe-mcp stdio [--scopes read,interact,control] [--principal NAME]
//!     Embedded test bench: the engine runs in this process and MCP is spoken on stdio.
//! pe-mcp serve [--pipe] [--http] [--origin ORIGIN]... [--scopes ...] [--principal NAME] [--ttl-s N] -- CMD ARGS...
//!     The engine side (design 17.1): starts the engine and its transports, issues a fresh token,
//!     and launches CMD (the harness) with PE_MCP_TOKEN, PE_MCP_PIPE and/or PE_MCP_URL in its
//!     environment. When CMD exits, every token is revoked and pe-mcp exits with CMD's code.
//! pe-mcp bridge
//!     What the harness runs (`pe-app mcp` in the design): connects to PE_MCP_PIPE with
//!     PE_MCP_TOKEN and relays stdio.
//! ```

use std::process::ExitCode;
use std::time::Duration;

use pe_mcp::auth::Principal;
use pe_mcp::host::HostOpts;
use pe_mcp::engine::{Engine, EngineHandle};
use pe_mcp::model::Scope;
use pe_mcp::server::{Auth, PeerServer};
use rmcp::ServiceExt;

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("pe-mcp: {msg}");
    ExitCode::from(2)
}

struct Opts {
    scopes: Vec<Scope>,
    principal: String,
    pipe: bool,
    http: bool,
    origins: Vec<String>,
    ttl: Duration,
    cmd: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { scopes: Scope::ALL.to_vec(), principal: "agent".into(), pipe: false, http: false, origins: vec![], ttl: Duration::from_secs(3600), cmd: vec![] };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut val = || {
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("{a} needs a value"))
        };
        match a {
            "--scopes" => o.scopes = val()?.split(',').map(|s| Scope::parse(s.trim()).ok_or_else(|| format!("unknown scope {s:?}"))).collect::<Result<_, _>>()?,
            "--principal" => o.principal = val()?,
            "--pipe" => o.pipe = true,
            "--http" => o.http = true,
            "--origin" => o.origins.push(val()?),
            "--ttl-s" => o.ttl = Duration::from_secs(val()?.parse().map_err(|_| "--ttl-s needs seconds")?),
            "--" => {
                o.cmd = args[i + 1..].to_vec();
                break;
            }
            _ => return Err(format!("unknown argument {a:?}")),
        }
        i += 1;
    }
    Ok(o)
}

async fn stdio(o: Opts) -> ExitCode {
    let engine = EngineHandle::spawn(Engine::with_default_scenario);
    let server = PeerServer::new(engine, Auth::Fixed(Principal::local(&o.principal, &o.scopes)));
    match server.serve(rmcp::transport::stdio()).await {
        Ok(running) => {
            let _ = running.waiting().await;
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

async fn serve(o: Opts) -> ExitCode {
    if o.cmd.is_empty() {
        return fail("serve needs a command to launch after --");
    }
    if !o.pipe && !o.http {
        return fail("serve needs --pipe and/or --http");
    }
    let engine = EngineHandle::spawn(Engine::with_default_scenario);
    let opts = HostOpts { pipe: o.pipe, http: o.http, origins: o.origins, scopes: o.scopes, principal: o.principal, ttl: o.ttl, cmd: o.cmd };
    let mut hosted = match pe_mcp::host::start(engine, &opts).await {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    match hosted.wait().await {
        Ok(Some(status)) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

async fn bridge() -> ExitCode {
    let (Ok(path), Ok(token)) = (std::env::var("PE_MCP_PIPE"), std::env::var("PE_MCP_TOKEN")) else {
        return fail("bridge needs PE_MCP_PIPE and PE_MCP_TOKEN in its environment");
    };
    let conn = match pe_mcp::pipe::connect(&path).await {
        Ok(c) => c,
        Err(e) => return fail(format!("connect {path}: {e}")),
    };
    let conn = match pe_mcp::pipe::client_handshake(conn, &token).await {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    match pe_mcp::pipe::relay_stdio(conn).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(mode) = args.first().cloned() else {
        return fail("usage: pe-mcp stdio | serve ... -- CMD | bridge");
    };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    rt.block_on(async {
        match mode.as_str() {
            "stdio" | "serve" => match parse(&args[1..]) {
                Ok(o) if mode == "stdio" => stdio(o).await,
                Ok(o) => serve(o).await,
                Err(e) => fail(e),
            },
            "bridge" => bridge().await,
            m => fail(format!("unknown mode {m:?}")),
        }
    })
}
