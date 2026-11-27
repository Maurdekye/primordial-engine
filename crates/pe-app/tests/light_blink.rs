//! M1 redo F5 as a two-process test: an agent-style lamp that blinks the engine's `Light`, on both machines.
//!
//! In the M1 recording the agent's lamp script built on both machines and never ran: its system wrote `Light`, which
//! its `script!` didn't list, so it never bound, and nothing said so. Here A's agent does what that agent did, through
//! MCP only. First the recorded source, byte for byte (pe-logic `tests/scripts/lamp-m1`): `code_check` and the commit
//! both refuse it, with the fix in the message (the red control). Then the source with `Light` listed: committed,
//! attached to the preset's Lamp with a `Light` and a `Blink { period: 1, phase: 0 }` added by their bare names, as
//! the agent did. Both agents then read the Lamp's `Light.intensity` at final frames, and the harness checks that the
//! two machines agree at every frame both read, that each saw it on and off, and that it is the period's square wave
//! of the frame. Both consoles say the script runs there; neither says it doesn't. Last, A replays its session and
//! every final cut's root must be the live one.

mod common;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};

/// The recording's lamp, as the agent committed it.
const RECORDED: &str = include_str!("../../pe-logic/tests/scripts/lamp-m1/src/lib.rs");
/// The refusal `code_check` and the commit give for it.
const REFUSAL: &str = "system blink uses pe.Light, which the script doesn't declare, so it would never run: a script \
                       lists every component its systems use, the engine's too. Write `pe_sdk::script!(components: \
                       [Blink, Light], systems: [blink]);` (Light is pe_sdk::engine::Light)";
/// Frames at 60 Hz in the lamp's 1 s period.
const PERIOD: u64 = 60;
/// Final frames each agent reads before it reports.
const SAMPLES: usize = 40;

fn fixed() -> String {
    assert!(RECORDED.contains("components: [Blink]"), "the recorded lamp moved");
    RECORDED.replace("components: [Blink]", "components: [Blink, Light]")
}

/// Release only, as the demo test: a debug pe-app can't keep M1's world in real time.
/// `cargo test --release -p pe-app --test light_blink`.
#[test]
#[cfg_attr(debug_assertions, ignore = "release only: a debug pe-app can't keep M1's world in real time")]
fn an_agent_s_light_blink_runs_on_both_machines_at_the_same_frames() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("m1-light-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the test's folder");
    let until = Instant::now() + LIMIT;

    let (mut a, mut a_in, a_out) = start(&dir, "a", "light_agent", "author", &["--open", "--check-replay"]);
    let invite = line_after(&a_out, "invite: ", until, &dir);
    // B joins on the invite alone (invite-only, the user's ruling). A's console names the join.
    let (mut b, _b_in, b_out) = start(&dir, "b", "light_agent", "observer", &["--join", &invite]);
    line_with(&a_out, " is joining with your invite…", until, &dir);
    // As in the demo test: no script before the join has ended.
    line_with(&a_out, " joined the session.", until, &dir);
    publish(&dir, "joined", "");

    let author = result(&dir, "author", &mut [&mut a, &mut b], until);
    let observer = result(&dir, "observer", &mut [&mut a, &mut b], until);
    let samples = |v: &Value| -> Vec<(u64, f64)> {
        (v["samples"].as_array().into_iter().flatten())
            .map(|s| (s[0].as_u64().expect("a frame"), s[1].as_f64().expect("an intensity")))
            .collect()
    };
    let (sa, sb) = (samples(&author), samples(&observer));
    // The machines agree at every frame both read.
    let mut at: BTreeMap<u64, f64> = sa.iter().copied().collect();
    for &(f, i) in &sb {
        if let Some(&j) = at.get(&f) {
            assert_eq!(i, j, "A and B differ at frame {f}: {author} {observer}");
        }
        at.insert(f, i);
    }
    for (who, s) in [("A", &sa), ("B", &sb)] {
        for want in [0.0, 1.0] {
            assert!(s.iter().any(|&(_, i)| i == want), "{who} never read intensity {want}: {s:?}");
        }
    }
    // A square wave of the frame with the period: on for one half, off for the other, from one phase.
    let wave = |shift: u64| at.iter().all(|(f, i)| *i == if (f + shift) % PERIOD < PERIOD / 2 { 1.0 } else { 0.0 });
    let shift = (0..PERIOD).find(|&k| wave(k));
    assert!(shift.is_some(), "not the lamp's 1 s square wave at these frames: {at:?}");
    for (name, rx) in [("A", &a_out), ("B", &b_out)] {
        let runs = rx.try_iter().any(|l| l.contains("scripts: script ") && l.contains(" runs on this machine"));
        assert!(runs, "{name}'s console never said the script runs\n{}", logs(&dir));
    }
    for name in ["a.err.log", "b.err.log"] {
        let err = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        assert!(!err.contains("doesn't run on this machine"), "{name}: {err}");
        assert!(!err.contains("faulted at frame"), "{name}: {err}");
    }
    writeln!(a_in, "replay").and_then(|()| a_in.flush()).expect("A reads `replay`");
    println!("{}", replay_verdict(&a_out, until, &dir));
}

/// The agents (module docs). Run on its own, without `PE_DEMO_ROLE`, it does nothing.
#[test]
fn light_agent() {
    let Some(role) = std::env::var_os("PE_DEMO_ROLE") else { return };
    let role = role.to_string_lossy().into_owned();
    let dir = PathBuf::from(std::env::var_os("PE_DEMO_DIR").expect("PE_DEMO_DIR"));
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
        rt.block_on(async {
            match role.as_str() {
                "author" => author(&dir).await,
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

/// A, once B joined: the recorded lamp refused twice, the fixed one committed and attached as the agent did, then
/// A's final readings of the Lamp's intensity.
async fn author(dir: &Path) -> Value {
    let c = connect().await;
    let lamp = lamp_id(&c).await;
    joined(dir).await;

    // The red control: the recorded source is refused by the check and by the commit, with the fix.
    let check = match call(&c, "code_check", json!({"source": RECORDED})).await {
        Ok(v) | Err(v) => v,
    };
    assert_eq!(check["ok"], false, "code_check took the recorded lamp: {check}");
    let said = check["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["severity"] == "error" && d["message"].as_str().is_some_and(|m| m.contains(REFUSAL)));
    assert!(said, "code_check's diagnostics don't give the fix: {check}");
    let commit = loop {
        let o = match call(
            &c,
            "world_apply",
            json!({"label": "light: the recorded lamp", "ops": [
            {"op": "code", "id": "$lamp", "source": RECORDED}]}),
        )
        .await
        {
            Ok(o) | Err(o) => o,
        };
        if o["refusal"]["code"] == "not_ready" || o["code"] == "not_ready" {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        break o;
    };
    assert!(commit.to_string().contains("which the script doesn't declare"), "the commit wasn't refused: {commit}");

    // The fixed source, committed, then attached with a Light and a Blink by their bare names, as the agent did. The
    // Lamp has a Light already, so since F6 the `add` replaces it only when asked (api.md 5.3): the agent's `add` with
    // `replace: true`, as F6's refusal tells it.
    let cursor = now(&c).await;
    let o =
        apply(&c, json!({"label": "light: the lamp", "ops": [{"op": "code", "id": "$lamp", "source": fixed()}]})).await;
    let script = o["scripts"]["$lamp"]["id"].as_str().unwrap_or_else(|| panic!("no script id: {o}")).to_string();
    let activated_at = activated(&c, cursor, &change(&o)).await;
    let cursor = now(&c).await;
    let bind = apply(
        &c,
        json!({"label": "light: the lamp blinks", "wait": "final", "ops": [
            {"op": "attach", "script": script, "to": [lamp], "mode": "linked"},
            {"op": "add", "id": lamp, "component": "Light", "replace": true,
             "fields": {"kind": 0, "intensity": 1, "range": 3, "color": {"linear": [1, 0.8, 0.6]},
                        "inner_angle": 0, "outer_angle": 0}},
            {"op": "add", "id": lamp, "component": "Blink", "fields": {"period": 1, "phase": 0}},
        ]}),
    )
    .await;
    if bind["status"] != "final" && !settled(&c, cursor, &change(&bind)).await {
        let session = call(&c, "session_info", json!({})).await;
        panic!("the attach never became final: {bind}\nsession: {session:?}");
    }
    let samples = read_light(&c, &lamp).await;
    let session = call(&c, "session_info", json!({})).await.expect("session_info");
    assert!(session.get("scripts_not_running").is_none(), "A: {session}");
    json!({"lamp": lamp, "script": script, "activated": activated_at, "samples": samples})
}

/// B: once the Lamp has a Light and a Blink, its own final readings of the intensity.
async fn observer() -> Value {
    let c = connect().await;
    let lamp = lamp_id(&c).await;
    both_here(&c).await;
    let samples = read_light(&c, &lamp).await;
    let session = call(&c, "session_info", json!({})).await.expect("session_info");
    assert!(session.get("scripts_not_running").is_none(), "B: {session}");
    json!({"lamp": lamp, "samples": samples})
}

/// The Lamp's `Light.intensity` at distinct final frames from two frames after the first that shows its `Blink` (so
/// the script has run on it: a snapshot at f is the state before f is stepped), until [`SAMPLES`] are read with both
/// values among them. A query of the Lamp that lists a script not running ends it at once, with the reason.
async fn read_light(c: &Client, lamp: &str) -> Vec<(u64, f64)> {
    let until = Instant::now() + Duration::from_secs(240);
    let mut out: Vec<(u64, f64)> = Vec::new();
    let mut blink_from = None;
    let mut said = Instant::now();
    loop {
        if said.elapsed() > Duration::from_secs(5) {
            progress(c, "light").await;
            said = Instant::now();
        }
        let q =
            match call(c, "world_query", json!({"where": {"id": lamp}, "fields": ["Light", "Blink"], "at": "final"}))
                .await
            {
                Ok(q) | Err(q) => q,
            };
        let o = &q["objects"][0];
        assert!(o.get("scripts_not_running").is_none(), "the Lamp's script doesn't run: {q}");
        if let (Some(f), Some(i), true) = (
            q["frame"].as_u64(),
            o["components"]["Light"]["intensity"].as_f64(),
            o["components"].get("Blink").is_some(),
        ) {
            let from = *blink_from.get_or_insert(f + 2);
            if f >= from && out.last().is_none_or(|&(g, _)| g != f) {
                out.push((f, i));
            }
        }
        let both = out.iter().any(|s| s.1 == 0.0) && out.iter().any(|s| s.1 == 1.0);
        if out.len() >= SAMPLES && both {
            return out;
        }
        assert!(Instant::now() < until, "the Lamp never blinked; read {out:?}; last {q}");
        tokio::time::sleep(Duration::from_millis(37)).await;
    }
}
