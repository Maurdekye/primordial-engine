//! By default (`HostOpts::inherit_stdin` off, as pe-app launches its `--agent`) the harness pe-mcp launches gets
//! no standard input: pe-app reads its own console there (B's reply code, `quit`), and on Windows a child that
//! shared that stdin could hang at start-up. While pe-app's console thread waits in a read on an inherited pipe,
//! every other operation on the same file object waits behind it, and Python's start-up asks the handle its type.
//! The self-test's step agent (T2) hung so in about half its runs under heavy.sh.
//!
//! The test runs this binary three ways: the test itself; a host, whose stdin is a pipe the test holds open and never
//! writes, which launches the agent through `host::start`; and the agent, which reads its stdin to the end. With
//! stdin inherited the agent waits on the open pipe and the host gives up; with no stdin it reads nothing at once.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pe_mcp::auth::Grant;
use pe_mcp::host::{HostOpts, start};
use pe_mcp::mock::MockWorld;
use pe_seams::world_api::{MemberId, Producer, ProducerKind};

/// Set for the host and, inherited, for the agent: without it their tests do nothing.
const ROLE: &str = "PE_MCP_LAUNCH_TEST";

fn grant() -> Grant {
    Grant {
        member: MemberId([1; 32]),
        producer: Producer {
            kind: ProducerKind::Agent,
            harness: Some("launch".into()),
            principal: None,
            requested_by: None,
        },
        confirm: false,
    }
}

/// This binary, running only `test`.
fn me(test: &str) -> Vec<String> {
    let exe = std::env::current_exe().expect("the test binary").to_string_lossy().into_owned();
    vec![exe, "--exact".into(), test.into(), "--test-threads".into(), "1".into(), "--nocapture".into()]
}

#[test]
fn the_launched_harness_gets_no_stdin() {
    let cmd = me("launch_host");
    let mut host =
        Command::new(&cmd[0]).args(&cmd[1..]).env(ROLE, "1").stdin(Stdio::piped()).spawn().expect("the host");
    // Held open, never written: an agent that inherited it would wait on it for ever.
    let _open = host.stdin.take();
    let until = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = host.try_wait().expect("the host's status") {
            break s;
        }
        if Instant::now() > until {
            let _ = host.kill();
            panic!("the host never finished");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "the host failed: the agent didn't finish reading its stdin ({status:?})");
}

/// The host: launches the agent the way pe-app does and waits for it, up to 20 s.
#[test]
fn launch_host() {
    if std::env::var_os(ROLE).is_none() {
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("a runtime");
    rt.block_on(async {
        let mut o = HostOpts::new(grant());
        o.http = true;
        o.cmd = me("launch_agent");
        let mut hosted = start(Arc::new(MockWorld::new(1)), &o).await.expect("the transports and the agent");
        let exited = tokio::time::timeout(Duration::from_secs(20), hosted.wait()).await;
        let status = exited.expect("the agent waited on its stdin").expect("the agent's status");
        assert!(status.is_some_and(|s| s.success()), "the agent failed: {status:?}");
    });
}

/// The agent: reads its stdin to the end; no stdin is an empty one.
#[test]
fn launch_agent() {
    if std::env::var_os(ROLE).is_none() {
        return;
    }
    let mut got = Vec::new();
    std::io::stdin().read_to_end(&mut got).expect("stdin");
    assert!(got.is_empty(), "the agent read {} bytes", got.len());
}
