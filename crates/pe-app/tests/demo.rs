//! M1's demo as the two-process acceptance test (app.md 6). A opens the ball pit headless and prints an invite; B
//! joins. An agent on A commits a lamp script through MCP, binds it to the preset's Lamp with the script's `Blink`
//! and `Glow`, then commits a second version that glows blue instead of warm white. B's own agent watches the Lamp
//! through B's MCP and finds the first kept frame it glows blue: `F + 2`, where `F` is the activation frame A's `mine`
//! reports, which every machine takes from final frames. The new code runs from `F + 1` (S6's
//! `ActivationNote::Activated`; logic.md 6.2 step 3), and a snapshot at frame f is the state before f is stepped
//! (app.md 3.1), so `F + 1` still shows warm white and `F + 2` is the first state the new code wrote. The test drives
//! pe-app through its command line and MCP only.
//!
//! Each agent is this test binary again: pe-app's `--agent` runs it with `demo_agent` as the one test, and with
//! `PE_DEMO_ROLE` (`author` on A, `observer` on B) and `PE_DEMO_DIR` passed through pe-app's environment. It writes
//! `<role>.json`, or `<role>.err` if it failed, in that folder, where the harness reads it. The folder keeps both
//! machines' output (`a.out.log`, `a.err.log`, and B's).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

/// pe-logic's lamp; its "on" colour is the first version's.
const LAMP: &str = include_str!("../../pe-logic/tests/scripts/lamp/src/lib.rs");
const WARM: &str = "Glow { r: 1.0, g: 0.8, b: 0.6, intensity: 1.0 }";
/// The second version's "on" colour.
const BLUE: &str = "Glow { r: 0.2, g: 0.4, b: 1.0, intensity: 1.0 }";
const WARM_RGBA: [f64; 4] = [1.0, 0.8, 0.6, 1.0];
const BLUE_RGBA: [f64; 4] = [0.2, 0.4, 1.0, 1.0];
/// Each pe-app's `--seconds`: a run that fails ends on its own by then, with its stats in its log.
const RUN_S: u64 = 150;
/// The whole demo's limit, from A's start.
const LIMIT: Duration = Duration::from_secs(RUN_S + 20);

type Client = RunningService<RoleClient, ()>;

/// Release only: a debug pe-app can't step M1's 500 balls in real time on a loaded machine, and pe-api refuses
/// edits (`not_ready`) while its replica is behind the present. `cargo test --release -p pe-app --test demo`.
#[test]
#[cfg_attr(debug_assertions, ignore = "release only: a debug pe-app can't keep M1's world in real time")]
fn an_agent_on_a_edits_the_lamp_and_b_sees_it_at_the_agreed_frame() {
    assert!(LAMP.contains(WARM), "the lamp's colour moved");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("m1-demo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the demo's folder");
    let until = Instant::now() + LIMIT;

    let (mut a, mut a_in, a_out) = start(&dir, "a", "author", &["--open"]);
    let invite = line_after(&a_out, "invite: ", until, &dir);
    let (mut b, _b_in, b_out) = start(&dir, "b", "observer", &["--join", &invite]);
    let reply = line_after(&b_out, "reply code for the opener: ", until, &dir);
    writeln!(a_in, "{reply}").and_then(|()| a_in.flush()).expect("A reads B's reply code");

    let author = result(&dir, "author", &mut [&mut a, &mut b], until);
    let observer = result(&dir, "observer", &mut [&mut a, &mut b], until);
    let f2 = author["f2"].as_u64().unwrap_or_else(|| panic!("A's activation frame: {author}"));
    let first = observer["first_blue"].as_u64().unwrap_or_else(|| panic!("B's first blue frame: {observer}"));
    // The new code runs from F + 1 (logic.md 6.2 step 3), and a snapshot at frame f is the state before f is stepped
    // (app.md 3.1): F + 1 is the last warm state, and F + 2 the first one the new code wrote.
    assert_eq!(
        first,
        f2 + 2,
        "B's Lamp is first blue once F + 1, the new code's first frame, is stepped: {author} {observer}"
    );
    let before = rgba(&observer["before"][1]).unwrap_or_else(|| panic!("B's Lamp at F + 1: {observer}"));
    assert!(is(before, WARM_RGBA), "and warm white before F + 1 is stepped: {observer}");
}

/// The agents (module docs). Run on its own, without `PE_DEMO_ROLE`, it does nothing.
#[test]
fn demo_agent() {
    let Some(role) = std::env::var_os("PE_DEMO_ROLE") else { return };
    let role = role.to_string_lossy().into_owned();
    let dir = PathBuf::from(std::env::var_os("PE_DEMO_DIR").expect("PE_DEMO_DIR"));
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
        rt.block_on(async {
            match role.as_str() {
                "author" => author().await,
                "observer" => observer().await,
                r => panic!("no role {r}"),
            }
        })
    }));
    match run {
        Ok(v) => publish(&dir, &format!("{role}.json"), &v.to_string()),
        Err(e) => {
            let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
            publish(&dir, &format!("{role}.err"), &msg.unwrap_or_default());
            panic!("the {role} agent failed");
        }
    }
}

/// Writes `name` in `dir` whole or not at all: the harness polls for it, and must never read it half-written.
fn publish(dir: &Path, name: &str, text: &str) {
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, text).expect("the agent's report");
    std::fs::rename(&tmp, dir.join(name)).expect("the agent's report, in place");
}

/// A: commits the lamp, binds it to the Lamp, waits for the warm glow, commits the blue lamp and reports the second
/// version's activation frame, then checks A's final state glows blue after it.
async fn author() -> Value {
    let c = connect().await;
    let lamp = lamp_id(&c).await;
    both_here(&c).await;
    let cursor = now(&c).await;
    let o =
        apply(&c, json!({"label": "demo: the lamp script", "ops": [{"op": "code", "id": "$lamp", "source": LAMP}]}))
            .await;
    let script = o["scripts"]["$lamp"]["id"].as_str().unwrap_or_else(|| panic!("no script id: {o}")).to_string();
    let f1 = activated(&c, cursor, &change(&o)).await;
    // Bound to the Lamp, with the script's components: a period long enough that the Lamp stays on.
    let cursor = now(&c).await;
    let bind = apply(
        &c,
        json!({"label": "demo: the lamp glows", "wait": "final", "ops": [
            {"op": "attach", "script": script, "to": [lamp], "mode": "linked"},
            {"op": "add", "id": lamp, "component": format!("Blink@{script}"),
             "fields": {"period": 1.0e6, "phase": 0.0, "near": 0.0}},
            {"op": "add", "id": lamp, "component": format!("Glow@{script}"),
             "fields": {"r": 0.0, "g": 0.0, "b": 0.0, "intensity": 0.0}},
        ]}),
    )
    .await;
    if bind["status"] != "final" && !settled(&c, cursor, &change(&bind)).await {
        let session = call(&c, "session_info", json!({})).await;
        let history = call(&c, "history_list", json!({})).await;
        panic!("the bind never became final: {bind}\nsession: {session:?}\nhistory: {history:?}");
    }
    let warm = wait_glow(&c, &lamp, WARM_RGBA).await;
    let cursor = now(&c).await;
    let blue = LAMP.replacen(WARM, BLUE, 1);
    let o =
        apply(&c, json!({"label": "demo: a blue lamp", "ops": [{"op": "code", "id": script, "source": blue}]})).await;
    let f2 = activated(&c, cursor, &change(&o)).await;
    let until = Instant::now() + Duration::from_secs(60);
    // The first state the new code wrote is F + 2, as the observer checks (module doc): a final frame at F + 1, which
    // a cut lands on once in every interval, still shows warm white.
    let fin = loop {
        match glow(&c, &lamp, json!("final")).await {
            Some((f, g)) if f > f2 + 1 => break (f, g),
            _ if Instant::now() > until => panic!("no final frame past F + 1 = {}", f2 + 1),
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };
    assert!(is(fin.1, BLUE_RGBA), "A's final Lamp after F = {f2}: {fin:?}");
    json!({"lamp": lamp, "script": script, "f1": f1, "f2": f2, "warm_seen_at": warm, "final": [fin.0, fin.1]})
}

/// B: polls its own newest frames until the Lamp glows blue, then finds the first blue frame among the kept ones
/// (api.md 9.1, the last 2 s) and reads the frame before it.
async fn observer() -> Value {
    let c = connect().await;
    let lamp = lamp_id(&c).await;
    both_here(&c).await;
    let until = Instant::now() + Duration::from_secs(240);
    let mut last_warm = None;
    let mut said = Instant::now();
    loop {
        if said.elapsed() > Duration::from_secs(5) {
            progress(&c, "B").await;
            said = Instant::now();
        }
        assert!(Instant::now() < until, "B's Lamp never turned blue; last warm frame {last_warm:?}");
        match glow(&c, &lamp, json!("latest")).await {
            Some((frame, g)) if is(g, WARM_RGBA) => last_warm = Some(frame),
            Some((frame, g)) if is(g, BLUE_RGBA) => {
                let warm = last_warm.unwrap_or_else(|| panic!("blue at {frame} before any warm frame"));
                let mut first = frame;
                for f in warm + 1..frame {
                    match glow(&c, &lamp, json!(f)).await {
                        Some((_, g)) if is(g, BLUE_RGBA) => {
                            first = f;
                            break;
                        }
                        Some(_) => {}
                        None => panic!("frame {f} isn't kept"),
                    }
                }
                let before = glow(&c, &lamp, json!(first - 1)).await;
                return json!({"first_blue": first, "before": before, "seen_blue_at": frame, "last_warm_seen": warm});
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// One line of where this machine is, on the agent's error output, which is pe-app's.
async fn progress(c: &Client, who: &str) {
    if let Ok(s) = call(c, "session_info", json!({})).await {
        let here = s["members"].as_array().map_or(0, |m| m.iter().filter(|m| m["here"] == true).count());
        eprintln!(
            "demo {who}: frame {} final {} health {} caught up {} here {here}",
            s["frame"], s["final_frame"], s["health"], s["caught_up"]
        );
    }
}

async fn connect() -> Client {
    let path = std::env::var("PE_MCP_PIPE").expect("PE_MCP_PIPE");
    let token = std::env::var("PE_MCP_TOKEN").expect("PE_MCP_TOKEN");
    let pipe = pe_mcp::pipe::connect(&path).await.expect("pe-app's pipe");
    let conn = pe_mcp::pipe::client_handshake(pipe, &token).await.expect("the token handshake");
    ().serve(conn).await.expect("MCP's initialize")
}

/// One tool call's structured content; `Err` when the tool answered with an error.
async fn call(c: &Client, name: &str, args: Value) -> Result<Value, Value> {
    let mut p = CallToolRequestParams::new(name.to_string());
    if let Value::Object(m) = args {
        p = p.with_arguments(m);
    }
    let r = c.call_tool(p).await.unwrap_or_else(|e| panic!("{name}: transport error {e}"));
    let v = r.structured_content.unwrap_or(Value::Null);
    if r.is_error == Some(true) { Err(v) } else { Ok(v) }
}

/// This machine's `session_info` lists both members as here (pe-net's links; the genesis peer's came up in setup).
async fn both_here(c: &Client) {
    let s = call(c, "session_info", json!({})).await.expect("session_info");
    let here = s["members"].as_array().map_or(0, |m| m.iter().filter(|m| m["here"] == true).count());
    assert_eq!(here, 2, "both members here: {s}");
}

/// The preset's Lamp (pe-world's preset, `names::LAMP`), named on every machine.
async fn lamp_id(c: &Client) -> String {
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
async fn apply(c: &Client, tx: Value) -> Value {
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

fn change(o: &Value) -> String {
    o["change"].as_str().unwrap_or_else(|| panic!("no change: {o}")).to_string()
}

/// A watch cursor for now.
async fn now(c: &Client) -> Value {
    call(c, "world_watch", json!({})).await.expect("a watch cursor")["cursor"].clone()
}

/// Watches `mine` from `cursor` until `change`'s version activates: the activation frame (it runs from the next).
async fn activated(c: &Client, mut cursor: Value, change: &str) -> u64 {
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
async fn settled(c: &Client, mut cursor: Value, change: &str) -> bool {
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

/// The Lamp's `Glow` at `at` and the frame read; `None` while it has none, or for a frame not kept.
async fn glow(c: &Client, lamp: &str, at: Value) -> Option<(u64, [f64; 4])> {
    parse_glow(&call(c, "world_query", json!({"where": {"id": lamp}, "fields": ["Glow"], "at": at})).await.ok()?)
}

fn parse_glow(q: &Value) -> Option<(u64, [f64; 4])> {
    let comps = q["objects"][0]["components"].as_object()?;
    let g = comps.iter().find(|(k, _)| k.starts_with("Glow")).map(|(_, v)| v)?;
    Some((q["frame"].as_u64()?, rgba(g)?))
}

/// The newest frame whose Lamp glows `want`. On failure, says what the Lamp has.
async fn wait_glow(c: &Client, lamp: &str, want: [f64; 4]) -> u64 {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let q = match call(c, "world_query", json!({"where": {"id": lamp}, "fields": ["Glow"]})).await {
            Ok(q) | Err(q) => q,
        };
        if let Some((f, g)) = parse_glow(&q)
            && is(g, want)
        {
            return f;
        }
        if Instant::now() > until {
            let types = call(c, "world_query", json!({"where": {"id": lamp}})).await;
            panic!("the Lamp never glowed {want:?}; last Glow query: {q}; the Lamp: {types:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn rgba(g: &Value) -> Option<[f64; 4]> {
    if let Some(a) = g.as_array() {
        return Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?, a.get(2)?.as_f64()?, a.get(3)?.as_f64()?]);
    }
    Some([g["r"].as_f64()?, g["g"].as_f64()?, g["b"].as_f64()?, g["intensity"].as_f64()?])
}

fn is(c: [f64; 4], want: [f64; 4]) -> bool {
    c.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-3)
}

/// Ends the process when the test ends, passed or not.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// pe-app, headless, with the demo's agent; its output's lines go to `<name>.out.log` and the returned channel.
fn start(dir: &Path, name: &str, role: &str, args: &[&str]) -> (Proc, ChildStdin, mpsc::Receiver<String>) {
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
        .args(["demo_agent", "--exact", "--nocapture", "--test-threads=1"])
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
fn line_after(rx: &mpsc::Receiver<String>, prefix: &str, until: Instant, dir: &Path) -> String {
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

/// The agent's result for `role`, failing as soon as it fails or either pe-app ends.
fn result(dir: &Path, role: &str, procs: &mut [&mut Proc], until: Instant) -> Value {
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
fn fail_after_exit(procs: &mut [&mut Proc], dir: &Path, why: String) -> ! {
    let until = Instant::now() + Duration::from_secs(RUN_S + 30);
    while Instant::now() < until && procs.iter_mut().any(|p| matches!(p.0.try_wait(), Ok(None))) {
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("{why}\n{}", logs(dir));
}

/// The last lines of both machines' output.
fn logs(dir: &Path) -> String {
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
