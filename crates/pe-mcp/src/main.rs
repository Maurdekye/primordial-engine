//! `pe-mcp` command line.
//!
//! ```text
//! pe-mcp bridge
//!     What the harness runs: connects to PE_MCP_PIPE with PE_MCP_TOKEN and relays stdio.
//! pe-mcp serve-mock [--pipe] [--http] [--origin ORIGIN]... [--scopes read,edit] [--principal NAME] [--ttl-s N] -- CMD ARGS...
//!     A test bench: serves the fixed mock world (src/mock.rs) and launches CMD with PE_MCP_TOKEN, PE_MCP_PIPE and/or
//!     PE_MCP_URL in its environment. When CMD exits, every token is revoked and pe-mcp exits with CMD's code.
//!     The live world is served by pe-app, which owns it (api.md 6.1).
//! ```

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use pe_mcp::auth::Grant;
use pe_mcp::host::HostOpts;
use pe_mcp::mock::MockWorld;
use pe_seams::world_api::{MemberId, Producer, ProducerKind};

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("pe-mcp: {msg}");
    ExitCode::from(2)
}

fn parse(args: &[String]) -> Result<HostOpts, String> {
    let grant = Grant {
        member: MemberId([1; 32]),
        producer: Producer {
            kind: ProducerKind::Agent,
            harness: Some("mock".into()),
            principal: Some("agent".into()),
            requested_by: None,
        },
        confirm: false,
    };
    let mut o = HostOpts::new(grant);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut val = || {
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("{a} needs a value"))
        };
        match a {
            "--scopes" => {
                o.scopes = val()?
                    .split(',')
                    .map(|s| pe_mcp::auth::parse_scope(s.trim()).ok_or_else(|| format!("unknown scope {s:?}")))
                    .collect::<Result<_, _>>()?
            }
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

async fn serve_mock(o: HostOpts) -> ExitCode {
    if o.cmd.is_empty() {
        return fail("serve-mock needs a command to launch after --");
    }
    if !o.pipe && !o.http {
        return fail("serve-mock needs --pipe and/or --http");
    }
    let mut hosted = match pe_mcp::host::start(Arc::new(MockWorld::new(20)), &o).await {
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
        return fail("usage: pe-mcp bridge | serve-mock ... -- CMD");
    };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    rt.block_on(async {
        match mode.as_str() {
            "serve-mock" => match parse(&args[1..]) {
                Ok(o) => serve_mock(o).await,
                Err(e) => fail(e),
            },
            "bridge" => bridge().await,
            m => fail(format!("unknown mode {m:?}")),
        }
    })
}
