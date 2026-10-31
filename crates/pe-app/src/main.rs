//! `pe-app`: the headless M1 peer (design 23.4, P2-10).
//!
//! ```text
//! pe-app init --members N (--host IP | --addrs A1,A2,..) [--base-port P] [--frames F] [--scene pile|ballpit]
//!             [--size S] [--window W] [--seed X] [--name NAME] --out DIR
//!     Write DIR/session.json (ids, addresses, public keys) and DIR/member-<id>.key (secret keys).
//! pe-app run --session FILE --key FILE --me N [--bind ADDR] [--faults FILE] [--crash-at-ms T]
//!            [--touches-per-s R] [--out DIR] [-v] [--mcp-pipe] [--mcp-http] [--scopes ..] [--principal NAME] [-- CMD ARGS..]
//!     Run one member until every frame of the session is confirmed (plus a short linger), then
//!     write DIR/hashes-<id>.txt and DIR/summary-<id>.json. With --mcp-*, an MCP server is attached
//!     and CMD (a harness such as Claude Code) is launched with PE_MCP_TOKEN / PE_MCP_PIPE / PE_MCP_URL.
//! pe-app compare DIR
//!     Compare every summary-<id>.json and hashes-<id>.txt in DIR: identical histories?
//! ```

mod config;
mod node;
mod wire;

use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use config::{hex, new_key, read_key, MemberEntry, SessionFile};
use node::{FaultStep, Node, NodeOpts, Summary};
use pe_mcp::engine::{Engine, EngineHandle};
use pe_mcp::host::HostOpts;
use pe_mcp::model::Scope;

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("pe-app: {msg}");
    ExitCode::from(2)
}

struct Args(Vec<String>);

impl Args {
    fn take(&mut self, k: &str) -> Option<String> {
        let i = self.0.iter().position(|a| a == k)?;
        self.0.remove(i);
        (i < self.0.len()).then(|| self.0.remove(i))
    }
    fn flag(&mut self, k: &str) -> bool {
        match self.0.iter().position(|a| a == k) {
            Some(i) => {
                self.0.remove(i);
                true
            }
            None => false,
        }
    }
    fn num<T: std::str::FromStr>(&mut self, k: &str, default: T) -> Result<T, String> {
        match self.take(k) {
            Some(v) => v.parse().map_err(|_| format!("{k}: not a number: {v}")),
            None => Ok(default),
        }
    }
    fn rest_after_dashes(&mut self) -> Vec<String> {
        match self.0.iter().position(|a| a == "--") {
            Some(i) => {
                let v = self.0[i + 1..].to_vec();
                self.0.truncate(i);
                v
            }
            None => vec![],
        }
    }
    fn done(&self) -> Result<(), String> {
        match self.0.first() {
            Some(a) => Err(format!("unknown argument {a:?}")),
            None => Ok(()),
        }
    }
}

fn init(mut a: Args) -> Result<(), String> {
    let n: u32 = a.num("--members", 0)?;
    let out = PathBuf::from(a.take("--out").ok_or("--out DIR is required")?);
    let base: u16 = a.num("--base-port", 7101)?;
    let addrs: Vec<SocketAddr> = match (a.take("--addrs"), a.take("--host")) {
        (Some(list), _) => list.split(',').map(|s| s.trim().parse().map_err(|_| format!("bad address {s}"))).collect::<Result<_, _>>()?,
        (None, Some(h)) => (0..n).map(|i| format!("{h}:{}", base + i as u16).parse().map_err(|_| format!("bad host {h}"))).collect::<Result<_, _>>()?,
        (None, None) => return Err("give --host IP or --addrs A1,A2,..".into()),
    };
    let n = if n == 0 { addrs.len() as u32 } else { n };
    if addrs.len() != n as usize {
        return Err(format!("{n} members but {} addresses", addrs.len()));
    }
    let mut session_id = [0u8; 8];
    getrandom::fill(&mut session_id).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let mut members = vec![];
    for (i, addr) in addrs.into_iter().enumerate() {
        let id = i as u32 + 1;
        let k = new_key();
        write_secret(&out.join(format!("member-{id}.key")), &(hex(&k.to_bytes()) + "\n")).map_err(|e| e.to_string())?;
        members.push(MemberEntry { id, addr, key: hex(&k.verifying_key().to_bytes()) });
    }
    let f = SessionFile {
        session: u64::from_le_bytes(session_id),
        name: a.take("--name").unwrap_or_else(|| "m1".into()),
        scene: a.take("--scene").unwrap_or_else(|| "pile".into()),
        size: a.num("--size", 4)?,
        frames: a.num("--frames", 1800)?,
        window: a.num("--window", 120)?,
        seed: a.num("--seed", 0x5052_494D_4F52_4449)?,
        members,
    };
    a.done()?;
    f.validate()?;
    std::fs::write(out.join("session.json"), serde_json::to_string_pretty(&f).unwrap() + "\n").map_err(|e| e.to_string())?;
    println!("wrote {} (session {:016x}, {} members) and {} key files", out.join("session.json").display(), f.session, f.members.len(), f.members.len());
    Ok(())
}

async fn run(mut a: Args) -> Result<(), String> {
    let cmd = a.rest_after_dashes();
    let session = SessionFile::read(Path::new(&a.take("--session").ok_or("--session FILE is required")?))?;
    let key = read_key(Path::new(&a.take("--key").ok_or("--key FILE is required")?))?;
    let me: u32 = a.num("--me", 0)?;
    let entry = session.members.iter().find(|m| m.id == me).ok_or(format!("member {me} is not in the session"))?;
    if hex(&key.verifying_key().to_bytes()) != entry.key {
        return Err(format!("the key file is not member {me}'s key"));
    }
    let bind: SocketAddr = match a.take("--bind") {
        Some(b) => b.parse().map_err(|_| format!("bad --bind {b}"))?,
        // The session address itself: a loopback session never listens on the network (and never
        // triggers a firewall prompt). Use --bind 0.0.0.0:PORT behind NAT or with several interfaces.
        None => entry.addr,
    };
    let faults: Vec<FaultStep> = match a.take("--faults") {
        Some(p) => {
            let mut v: Vec<FaultStep> = serde_json::from_str(&std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?).map_err(|e| format!("{p}: {e}"))?;
            v.sort_by_key(|f| f.at_ms);
            v
        }
        None => vec![],
    };
    let crash_at_ms = match a.take("--crash-at-ms") {
        Some(v) => Some(v.parse().map_err(|_| "--crash-at-ms: not a number")?),
        None => None,
    };
    let touches_per_s: u32 = a.num("--touches-per-s", 0)?;
    let out_dir = PathBuf::from(a.take("--out").unwrap_or_else(|| ".".into()));
    let verbose = a.flag("-v");
    let (mcp_pipe, mcp_http) = (a.flag("--mcp-pipe"), a.flag("--mcp-http"));
    let scopes = match a.take("--scopes") {
        Some(s) => s.split(',').map(|x| Scope::parse(x.trim()).ok_or(format!("unknown scope {x}"))).collect::<Result<Vec<_>, _>>()?,
        // A live peer has no scenario control; world_step waits in real time.
        None => vec![Scope::Read, Scope::Interact, Scope::Control],
    };
    let principal = a.take("--principal").unwrap_or_else(|| "agent".into());
    a.done()?;
    if !cmd.is_empty() && !mcp_pipe && !mcp_http {
        return Err("a harness command needs --mcp-pipe or --mcp-http".into());
    }

    let socket = UdpSocket::bind(bind).map_err(|e| format!("bind {bind}: {e}"))?;
    let finished = Arc::new(AtomicBool::new(false));
    let opts = NodeOpts { me, session: session.clone(), key, socket, faults, crash_at_ms, touches_per_s, out_dir: out_dir.clone(), finished: finished.clone(), verbose };
    eprintln!("pe-app: member {me} of session {:016x} on {bind}; {} members, {} frames", session.session, session.members.len(), session.frames);
    let engine = EngineHandle::spawn_ticking(move || Engine::new(Box::new(Node::new(opts))), Duration::from_millis(1));
    let _hosted = if mcp_pipe || mcp_http {
        let o = HostOpts { pipe: mcp_pipe, http: mcp_http, scopes, principal, cmd, ..HostOpts::default() };
        Some(pe_mcp::host::start(engine.clone(), &o).await.map_err(|e| e.to_string())?)
    } else {
        None
    };
    while !finished.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // A panic in the node faults the engine, which then stops ticking it: end the process with
        // the message rather than look like a stalled member (review of 2f9ca05).
        // An error means the engine thread itself is gone (a panic outside catch_unwind, e.g. in
        // Node::new): stopped too.
        match engine.call(|e| Ok(e.faulted().map(str::to_string))).await {
            Ok(None) => {}
            Ok(Some(why)) => return Err(format!("member {me}: the peer stopped: {why}")),
            Err(e) => return Err(format!("member {me}: the engine thread is gone: {}", e.message)),
        }
    }
    let path = out_dir.join(format!("summary-{me}.json"));
    let s: Summary = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    println!(
        "member {me}: frames 0..{} digest {} replay {} ({}), conf {}, members at end {:?}, rollbacks {}, checks ok {} bad {}",
        s.frames,
        s.digest,
        s.replay_digest,
        if s.replay_equal { "EQUAL" } else { "DIFFERENT" },
        s.conf,
        s.members_at_end,
        s.rollbacks,
        s.checks_ok,
        s.checks_bad
    );
    Ok(())
}

/// Write a secret key file: readable by its owner only on Unix (Windows files inherit the folder's ACL).
fn write_secret(path: &Path, text: &str) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    std::io::Write::write_all(&mut o.open(path)?, text.as_bytes())
}

fn compare(mut a: Args) -> Result<bool, String> {
    let dir = PathBuf::from(a.0.pop().ok_or("compare DIR")?);
    a.done()?;
    let mut sums: Vec<(Summary, Vec<String>)> = vec![];
    for e in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let p = e.map_err(|e| e.to_string())?.path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if let Some(id) = name.strip_prefix("summary-").and_then(|r| r.strip_suffix(".json")) {
            let s: Summary = serde_json::from_str(&std::fs::read_to_string(&p).map_err(|e| e.to_string())?).map_err(|e| format!("{name}: {e}"))?;
            let h = std::fs::read_to_string(dir.join(format!("hashes-{id}.txt"))).map_err(|e| e.to_string())?;
            sums.push((s, h.lines().map(|l| l.trim().to_string()).collect()));
        }
    }
    sums.sort_by_key(|(s, _)| s.member);
    if sums.len() < 2 {
        return Err(format!("{}: fewer than 2 members' results", dir.display()));
    }
    let mut ok = true;
    println!("| member | frames | digest | replay | conf | members at end | rollbacks | checks ok/bad | datagrams sent/faulted |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for (s, _) in &sums {
        println!(
            "| {} | {} | {} | {} | {} | {:?} | {} | {}/{} | {}/{} |",
            s.member,
            s.frames,
            s.digest,
            if s.replay_equal { "equal" } else { "DIFFERENT" },
            s.conf,
            s.members_at_end,
            s.rollbacks,
            s.checks_ok,
            s.checks_bad,
            s.traffic.datagrams_sent,
            s.traffic.datagrams_dropped_by_fault
        );
        ok &= s.replay_equal && s.checks_bad == 0;
    }
    for (s, h) in &sums {
        if let Some(l) = h.iter().find(|l| l.ends_with("missing")) {
            println!("member {}: no state hash for frame {}", s.member, l.split(' ').next().unwrap_or("?"));
            ok = false;
        }
    }
    let (first, fh) = &sums[0];
    for (s, h) in &sums[1..] {
        if s.session != first.session || s.frames != first.frames {
            println!("member {}: a different session or length", s.member);
            ok = false;
        } else if let Some(i) = (0..fh.len()).find(|&i| fh.get(i) != h.get(i)) {
            println!("member {} differs from member {} first at: {:?} vs {:?}", s.member, first.member, h.get(i), fh.get(i));
            ok = false;
        }
    }
    println!("{}", if ok { "ALL EQUAL" } else { "MISMATCH" });
    Ok(ok)
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return fail("usage: pe-app init|run|compare ...");
    }
    let mode = args.remove(0);
    let a = Args(args);
    match mode.as_str() {
        "init" => init(a).map_or_else(fail, |_| ExitCode::SUCCESS),
        "compare" => match compare(a) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => fail(e),
        },
        "run" => {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
            rt.block_on(run(a)).map_or_else(fail, |_| ExitCode::SUCCESS)
        }
        m => fail(format!("unknown mode {m:?}")),
    }
}
