//! What the two-process tests share (tests/demo.rs, tests/light_blink.rs): pe-app started headless with this test
//! binary as its agent (`--agent`, the agent being one test of the binary, given `PE_DEMO_ROLE` and `PE_DEMO_DIR`),
//! its console read line by line, and the agents' MCP calls. Each test binary uses only part of it.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

/// Each pe-app's `--seconds`: a run that fails ends on its own by then, with its stats in its log.
pub const RUN_S: u64 = 150;
/// The whole test's limit, from A's start.
pub const LIMIT: Duration = Duration::from_secs(RUN_S + 20);

pub type Client = RunningService<RoleClient, ()>;

/// Waits for A's replay verdict (pe-app's `replay`), failing at once on a replay that differs or couldn't run.
pub fn replay_verdict(rx: &mpsc::Receiver<String>, until: Instant, dir: &Path) -> String {
    loop {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(l) if l.contains("replay: the live run equals the replay") => return l,
            Ok(l) if l.contains("replay: DIFFERS") || l.contains("replay: couldn't") => {
                panic!("A's replay: {l}\n{}", logs(dir))
            }
            Ok(_) => {}
            Err(e) => panic!("no replay verdict from A ({e})\n{}", logs(dir)),
        }
    }
}

/// Writes `name` in `dir` whole or not at all: the harness polls for it, and must never read it half-written.
pub fn publish(dir: &Path, name: &str, text: &str) {
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, text).expect("the agent's report");
    std::fs::rename(&tmp, dir.join(name)).expect("the agent's report, in place");
}

/// One line of where this machine is, on the agent's error output, which is pe-app's.
pub async fn progress(c: &Client, who: &str) {
    if let Ok(s) = call(c, "session_info", json!({})).await {
        let here = s["members"].as_array().map_or(0, |m| m.iter().filter(|m| m["here"] == true).count());
        eprintln!(
            "demo {who}: frame {} final {} health {} caught up {} here {here}",
            s["frame"], s["final_frame"], s["health"], s["caught_up"]
        );
    }
}

pub async fn connect() -> Client {
    let path = std::env::var("PE_MCP_PIPE").expect("PE_MCP_PIPE");
    let token = std::env::var("PE_MCP_TOKEN").expect("PE_MCP_TOKEN");
    let pipe = pe_mcp::pipe::connect(&path).await.expect("pe-app's pipe");
    let conn = pe_mcp::pipe::client_handshake(pipe, &token).await.expect("the token handshake");
    ().serve(conn).await.expect("MCP's initialize")
}

/// One tool call's structured content; `Err` when the tool answered with an error.
pub async fn call(c: &Client, name: &str, args: Value) -> Result<Value, Value> {
    let mut p = CallToolRequestParams::new(name.to_string());
    if let Value::Object(m) = args {
        p = p.with_arguments(m);
    }
    let r = c.call_tool(p).await.unwrap_or_else(|e| panic!("{name}: transport error {e}"));
    let v = r.structured_content.unwrap_or(Value::Null);
    if r.is_error == Some(true) { Err(v) } else { Ok(v) }
}

/// Waits until this machine's `session_info` lists both members as here (pe-net's links): A opens alone, and B joins
/// at a cut on the invite alone (app.md 4, "Joining from a cut"; invite-only).
pub async fn both_here(c: &Client) {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let s = call(c, "session_info", json!({})).await.expect("session_info");
        let here = s["members"].as_array().map_or(0, |m| m.iter().filter(|m| m["here"] == true).count());
        if here == 2 {
            return;
        }
        assert!(Instant::now() < until, "both members here: {s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits for the harness's word (`joined` in the demo's folder) that A's console said B joined: only then may A's agent
/// commit a script (the test's body).
pub async fn joined(dir: &Path) {
    let until = Instant::now() + LIMIT;
    while !dir.join("joined").exists() {
        assert!(Instant::now() < until, "A's console never said B joined");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The preset's Lamp (pe-world's preset, `names::LAMP`), named on every machine.
pub async fn lamp_id(c: &Client) -> String {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(q) = call(c, "world_query", json!({"where": {"name": "Lamp"}})).await
            && let Some(id) = q["objects"][0]["id"].as_str()
        {
            return id.to_string();
        }
        assert!(Instant::now() < until, "no Lamp");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `world_apply`, tried again while this machine catches up (`not_ready`), as an agent following `next` would.
pub async fn apply(c: &Client, tx: Value) -> Value {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let o = match call(c, "world_apply", tx.clone()).await {
            Ok(o) | Err(o) => o,
        };
        let code = o["refusal"]["code"].as_str().or(o["code"].as_str());
        if code == Some("not_ready") && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        // `queued` when this machine hasn't applied it within the call's local wait: `mine` reports the rest.
        assert!(matches!(o["status"].as_str(), Some("queued" | "accepted" | "final")), "{tx}\n-> {o}");
        return o;
    }
}

pub fn change(o: &Value) -> String {
    o["change"].as_str().unwrap_or_else(|| panic!("no change: {o}")).to_string()
}

/// A watch cursor for now.
pub async fn now(c: &Client) -> Value {
    call(c, "world_watch", json!({})).await.expect("a watch cursor")["cursor"].clone()
}

/// Watches `mine` from `cursor` until `change`'s version activates: the activation frame (it runs from the next).
pub async fn activated(c: &Client, mut cursor: Value, change: &str) -> u64 {
    let until = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(Instant::now() < until, "{change} never activated");
        progress(c, "A").await;
        let w = call(c, "world_watch", json!({"cursor": cursor, "wait_ms": 1000})).await.expect("watch");
        for o in w["mine"].as_array().into_iter().flatten() {
            if o["change"].as_str() == Some(change) {
                match o["activation"]["result"].as_str() {
                    Some("activated") => return o["activation"]["frame"].as_u64().expect("the activation frame"),
                    Some(other) => panic!("{change}: {other}: {o}"),
                    None => {}
                }
            }
        }
        cursor = w["cursor"].clone();
    }
}

/// Watches `mine` from `cursor` until `change` is final (true), or 60 s pass (false); any other end fails.
pub async fn settled(c: &Client, mut cursor: Value, change: &str) -> bool {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() > until {
            return false;
        }
        progress(c, "A").await;
        let w = call(c, "world_watch", json!({"cursor": cursor, "wait_ms": 1000})).await.expect("watch");
        for o in w["mine"].as_array().into_iter().flatten() {
            if o["change"].as_str() == Some(change) {
                match o["status"].as_str() {
                    Some("final") => return true,
                    Some("queued" | "accepted") => {}
                    _ => panic!("{change}: {o}"),
                }
            }
        }
        cursor = w["cursor"].clone();
    }
}

/// Ends the process when the test ends, passed or not.
pub struct Proc(pub Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// pe-app, headless, with the test's agent (`agent`, the test binary's test of that name, as `role`); its output's lines go to `<name>.out.log` and the returned channel.
pub fn start(
    dir: &Path,
    name: &str,
    agent: &str,
    role: &str,
    args: &[&str],
) -> (Proc, ChildStdin, mpsc::Receiver<String>) {
    let me = std::env::current_exe().expect("this test's path");
    let err = std::fs::File::create(dir.join(format!("{name}.err.log"))).expect("a log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_pe-app"))
        .args(["--headless", "--seconds"])
        .arg(RUN_S.to_string())
        .args(["--listen", "127.0.0.1:0", "--identity"])
        .arg(dir.join(format!("{name}.seed")))
        .args(args)
        .args(["--mcp-pipe", "--agent"])
        .arg(&me)
        .args([agent, "--exact", "--nocapture", "--test-threads=1"])
        .env("PE_DEMO_ROLE", role)
        .env("PE_DEMO_DIR", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(err)
        .spawn()
        .expect("pe-app starts");
    let stdin = child.stdin.take().expect("its input");
    let stdout = child.stdout.take().expect("its output");
    let mut log = std::fs::File::create(dir.join(format!("{name}.out.log"))).expect("a log");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = writeln!(log, "{line}");
            let _ = tx.send(line);
        }
    });
    (Proc(child), stdin, rx)
}

/// The rest of the first output line that starts with `prefix`.
pub fn line_after(rx: &mpsc::Receiver<String>, prefix: &str, until: Instant, dir: &Path) -> String {
    loop {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(l) => {
                if let Some(rest) = l.strip_prefix(prefix) {
                    return rest.trim().to_string();
                }
            }
            Err(e) => panic!("no {prefix:?} line ({e})\n{}", logs(dir)),
        }
    }
}

/// Waits for an output line holding `text` anywhere: the agent pe-app starts shares its output, so a console line
/// may follow the agent's own "test demo_agent ... " on one line. A join that failed ends the wait at once.
pub fn line_with(rx: &mpsc::Receiver<String>, text: &str, until: Instant, dir: &Path) {
    loop {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(l) if l.contains(text) => return,
            Ok(l) if l.contains("couldn't join") => panic!("the join failed: {l}\n{}", logs(dir)),
            Ok(_) => {}
            Err(e) => panic!("no line with {text:?} ({e})\n{}", logs(dir)),
        }
    }
}

/// The agent's result for `role`, failing as soon as it fails or either pe-app ends.
pub fn result(dir: &Path, role: &str, procs: &mut [&mut Proc], until: Instant) -> Value {
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join(format!("{role}.json"))) {
            return serde_json::from_str(&text).unwrap_or_else(|e| panic!("{role}.json: {e}: {text}"));
        }
        if let Ok(why) = std::fs::read_to_string(dir.join(format!("{role}.err"))) {
            fail_after_exit(procs, dir, format!("the {role} agent failed: {why}"));
        }
        for (i, p) in procs.iter_mut().enumerate() {
            if let Ok(Some(status)) = p.0.try_wait() {
                panic!("pe-app {} ended early ({status})\n{}", ["A", "B"][i.min(1)], logs(dir));
            }
        }
        if Instant::now() > until {
            fail_after_exit(procs, dir, format!("no {role} result in time"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Waits for both pe-apps to end on their own (`--seconds`), so their logs end with their stats, then fails.
pub fn fail_after_exit(procs: &mut [&mut Proc], dir: &Path, why: String) -> ! {
    let until = Instant::now() + Duration::from_secs(RUN_S + 30);
    while Instant::now() < until && procs.iter_mut().any(|p| matches!(p.0.try_wait(), Ok(None))) {
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("{why}\n{}", logs(dir));
}

/// The last lines of both machines' output.
pub fn logs(dir: &Path) -> String {
    let mut s = format!("logs in {}:", dir.display());
    for name in ["a.out.log", "a.err.log", "b.out.log", "b.err.log"] {
        let text = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        let lines: Vec<_> = text.lines().collect();
        s += &format!(
            "\n--- {name} (last 40 of {})\n{}",
            lines.len(),
            lines[lines.len().saturating_sub(40)..].join("\n")
        );
    }
    s
}
