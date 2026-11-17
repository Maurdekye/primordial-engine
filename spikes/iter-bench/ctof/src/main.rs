//! The lamp's c-to-F probe (logic.md 6.2a). Two machines, A and B, open and join a session over real UDP on loopback
//! as pe-app's binary does (`--open` / `--join`), each simulation on its own thread with its build service and
//! attester (main.rs `start_builds`). A commits the lamp through pe-api's `code` op, as an agent would over MCP: the
//! author's own build runs inside the call, then the commit is stamped `c`. B's attester builds it and posts its
//! attestation; at n = 2 the quorum is both, so `F` is B's stamp, and the new code runs from `F + 1`.
//!
//! Each repetition commits a new script whose source differs in one literal, so every V is new and both machines
//! build it. Repetition 0 runs on empty build folders (pe-sdk compiled cold on both machines); the rest are warm.
//!
//! Two PCs: `open` (A, the author) on one and `join` (B, the attester) on the other; see [`open`] and [`join`].
//!
//! Usage: `ctof-probe [local] [reps] [balls] [build-root]`, with `CARGO` set to the pinned toolchain's cargo (pe-logic's
//! `Toolchain::current`); one JSON line per repetition on stdout. `CTOF_DEBUG=1` prints a dry run of each commit first.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pe_app::api::Ports;
use pe_app::builds::{self, BuildConfig, Builder};
use pe_app::clock;
use pe_app::local::chain_nonce;
use pe_app::net::{self, Endpoint, OpenParams, WakeLater};
use pe_app::sim::{self, Budget, Sim};
use pe_logic::run::RunSettings;
use pe_net_crypto::identity::{Invite, Reply};
use pe_render::present::FrameSlot;
use pe_replica::ScriptRunner;
use pe_replica::logic::LogicRunner;
use pe_seams::world_api::{
    ApplyOpts, Caller, ConnectionId, MemberId, Mode, Producer, ProducerKind, RefusalCode, Scope, Status, Transaction,
    Wait,
};
use pe_seams::{Hand, HandFeed, HandInput, HandSource, HostTime, MemberKey, RigPose, SimRate};
use serde_json::json;

const RATE: SimRate = SimRate::DEFAULT;
const SIM_STACK: usize = 4 << 20;
const LAMP: &str = include_str!("../../../../crates/pe-logic/tests/scripts/lamp/src/lib.rs");

struct StillHands([f32; 3]);

impl HandFeed for StillHands {
    fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput> {
        let x = if hand == Hand::Left { -0.2 } else { 0.2 };
        Some(HandInput {
            hand,
            source: HandSource::Tracked,
            sampled_at: at,
            tracked: true,
            pose: RigPose { position: [self.0[0] + x, self.0[1], self.0[2]], orientation: [0.0, 0.0, 0.0, 1.0] },
            linear_velocity: None,
            angular_velocity: None,
            grip: 0.0,
            closed: false,
            discontinuity: 0,
        })
    }
}

/// One machine, running: its simulation thread, its api ports, its attester and its member.
struct Machine {
    sim: sim::SimThread,
    ports: Ports,
    attester: builds::AttesterThread,
    me: MemberKey,
}

fn runner() -> (Box<dyn ScriptRunner>, pe_replica::logic::ModuleInbox, usize) {
    let r = LogicRunner::new(pe_logic::Settings::default(), RunSettings::default()).expect("a runner");
    let (inbox, stack) = (r.inbox(), SIM_STACK.max(r.thread_stack()));
    (Box::new(r), inbox, stack)
}

fn start(
    session: net::Session,
    wake: WakeLater,
    feed: [f32; 3],
    dir: PathBuf,
    inbox: pe_replica::logic::ModuleInbox,
    stack: usize,
    host: &clock::HostClock,
) -> Machine {
    let slot = Arc::new(FrameSlot::new());
    let me = session.me;
    let sim = Sim::new(Box::new(session.node), session.replica, session.frames, slot, Budget::new(RATE))
        .with_local(me, chain_nonce(), Arc::new(StillHands(feed)))
        .with_transport(net::transport(session.handle), Some(Box::new(session.hook)));
    let arrival = wake.arrival();
    let ports = sim.ports("ctof probe", move || arrival()).expect("pe-world's ports");
    let sim = sim.with_api(ports.clone());
    let sim = sim::spawn(sim, host.clone(), stack).expect("the simulation thread");
    wake.set(sim.waker());
    let toolchain = pe_logic::build::Toolchain::current().expect("a toolchain");
    let config = BuildConfig::new(toolchain, pe_logic::build::engine_sdk(), dir);
    let builder = Arc::new(Builder::new(config, inbox.clone()).expect("a builder"));
    let attester = builds::spawn_attester(builder.clone(), inbox, ports.shared.clone(), ports.queue.clone(), me)
        .expect("the attester");
    let ports = ports.with_builds(builder);
    Machine { sim, ports, attester, me }
}

fn caller(me: MemberKey) -> Caller {
    Caller {
        connection: ConnectionId(1),
        principal: "ctof-probe".into(),
        member: MemberId::from(me),
        scopes: [Scope::Read, Scope::Edit].into_iter().collect(),
        producer: Producer {
            kind: ProducerKind::Agent,
            harness: Some("ctof-probe".into()),
            principal: None,
            requested_by: None,
        },
        confirm: false,
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    match args.peek().map(String::as_str) {
        Some("open") => return open(args.skip(1).collect()),
        Some("join") => return join(args.skip(1).collect()),
        Some("local") => {
            args.next();
        }
        _ => {}
    }
    let reps: usize = args.next().map_or(9, |a| a.parse().expect("reps"));
    let balls: u32 = args.next().map_or(500, |a| a.parse().expect("balls"));
    let root = PathBuf::from(args.next().unwrap_or_else(|| "S:/primordial-engine/tmp/iter-bench-ctof".into()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the build root");
    eprintln!("ctof: {reps} reps, {balls} balls, {} Hz, build folders under {}", RATE.hz(), root.display());

    let host = clock::shared();
    let (wa, wb) = (WakeLater::default(), WakeLater::default());
    let lo = "127.0.0.1:0".parse().unwrap();
    let a = Endpoint::bind([21; 32], lo, Vec::new(), true, host.clone(), Some(wa.arrival())).expect("A binds");
    let mut b = Endpoint::bind([22; 32], lo, Vec::new(), false, host.clone(), Some(wb.arrival())).expect("B binds");
    let invite = a.invite(Duration::from_secs(600)).unwrap();
    let reply = b.reply(&invite);
    b.dial(&invite).unwrap();
    let (ra, ia, sa_stack) = runner();
    let (rb, ib, sb_stack) = runner();
    let theirs = invite.clone();
    let joiner = std::thread::spawn(move || b.join_session(&theirs, RATE, Duration::from_secs(20), rb));
    let params = OpenParams { seed: 7, balls, rate: RATE, epoch_lead: net::EPOCH_LEAD };
    let sa = a.open_session(&invite, &reply, &params, Duration::from_secs(20), ra).expect("A's session");
    let sb = joiner.join().unwrap().expect("B's session");
    let ma = start(sa, wa, [-0.5, 1.0, -0.4], root.join("a"), ia, sa_stack, &host);
    let mb = start(sb, wb, [0.5, 1.1, -0.3], root.join("b"), ib, sb_stack, &host);
    // Past frame 0 (EPOCH_LEAD) and a few seconds of finality.
    std::thread::sleep(net::EPOCH_LEAD + Duration::from_secs(3));
    run_reps(&ma, Some(&mb), reps);
    eprintln!("ctof: done; stopping");
    ma.attester.stop();
    mb.attester.stop();
    let sa = ma.sim.stop();
    let sb = mb.sim.stop();
    eprintln!("ctof: A {:?}\nctof: B {:?}", sa.map(|s| format!("{s:?}")), sb.map(|s| format!("{s:?}")));
    std::process::exit(0);
}

/// A commits the lamp `reps` times and reads c and F. With B in this process (`local`), B's rows too; without it
/// (`open`, B on another PC), F comes from A's agreed row, and B's own log has B's side.
fn run_reps(ma: &Machine, mb: Option<&Machine>, reps: usize) {
    let api = ma.ports.api();
    let who = caller(ma.me);
    let period_ms = 1e3 / f64::from(RATE.hz());
    for (n, m) in std::iter::once(("A", ma)).chain(mb.map(|m| ("B", m))) {
        let snap = m.ports.shared.agreed_snapshot();
        let members: Vec<String> = snap.members().map(|(k, _)| format!("{:02x}{:02x}", k.0[0], k.0[1])).collect();
        eprintln!(
            "ctof: {n} is {:02x}{:02x}; world members {members:?} at frame {:?}",
            m.me.0[0],
            m.me.0[1],
            snap.frame()
        );
    }

    for rep in 0..reps {
        let src = LAMP.replacen("intensity: 1.0", &format!("intensity: 0.{:02}", 10 + rep), 1);
        assert_ne!(src, LAMP, "the lamp's literal moved");
        let before: BTreeSet<_> = ma.ports.shared.agreed_snapshot().modules().keys().copied().collect();
        let tx: Transaction = serde_json::from_value(json!({
            "label": format!("ctof {rep}"),
            "ops": [{"op": "code", "id": "$lamp", "source": src}],
        }))
        .unwrap();
        if std::env::var_os("CTOF_DEBUG").is_some() {
            let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
            let t: Transaction =
                serde_json::from_value(json!({"label": "dry", "ops": [{"op": "code", "id": "$lamp", "source": src}]}))
                    .unwrap();
            let d = api.apply(&who, t, &dry);
            eprintln!("dry run: {d:?}");
        }
        let opts = ApplyOpts { mode: Mode::Live, wait: Wait::Local, ..Default::default() };
        // Until this machine has caught up, pe-api answers `NotReady` (after the author's build, which the retry then
        // finds cached), and the probe retries every 100 ms, as an agent following `next` would. The author's call
        // is timed from the first try; the retries are counted.
        let ready = Instant::now() + Duration::from_secs(60);
        let (t0, mut not_ready, mut first_try_ms) = (Instant::now(), 0u32, None);
        let o = loop {
            let t = Instant::now();
            let o = api.apply(&who, tx.clone(), &opts).expect("the code op");
            first_try_ms.get_or_insert(ms(t.elapsed()));
            if o.refusal.as_ref().is_some_and(|r| r.code == RefusalCode::NotReady) && Instant::now() < ready {
                not_ready += 1;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            break o;
        };
        let t_applied = Instant::now();
        assert!(matches!(o.status, Status::Accepted | Status::Final), "rep {rep}: {:?} {:?}", o.status, o.refusal);

        // Watch both machines' live snapshots and A's agreed one. A, the author, predicts its own commit (logic.md
        // 6.2a (i); pe-replica's `Predict`): its live row first runs from c + 1, then, once B's attestation arrives, A
        // steps again from c under the rule and its row runs from F + 1. B's row and A's agreed row are the rule's.
        let new_row = |snap: &pe_world::WorldSnapshot| {
            snap.modules().iter().find(|(id, _)| !before.contains(*id)).map(|(_, r)| (r.activate_at.0, snap.frame().0))
        };
        let (mut seen_a, mut seen_b) = (0, 0);
        let (mut a_first, mut a_fixed, mut b_row, mut a_final, mut b_pending) = (None, None, None, None, None);
        let mut attested: Vec<u64> = Vec::new();
        let limit = t_applied + Duration::from_secs(120);
        while (a_fixed.is_none() || (mb.is_some() && b_row.is_none()) || a_final.is_none()) && Instant::now() < limit {
            let (sa, snap_a) = ma.ports.shared.wait_newer(seen_a, Duration::from_millis(2));
            seen_a = sa;
            let now = Instant::now();
            if let Some((at, frame)) = new_row(&snap_a) {
                match a_first {
                    None => a_first = Some((now, at, frame)),
                    Some((_, first, _)) if at != first && a_fixed.is_none() => a_fixed = Some((now, at, frame)),
                    _ => {}
                }
            }
            if let Some(mb) = mb {
                let (sb, snap_b) = mb.ports.shared.wait_newer(seen_b, Duration::from_millis(0));
                seen_b = sb;
                for (id, p) in snap_b.pending() {
                    if !before.contains(id) {
                        b_pending.get_or_insert(now);
                        assert!(p.void.is_none() && p.failed.is_empty(), "rep {rep}: {:?} {:?}", p.void, p.failed);
                        attested = p.attested.iter().map(|(_, f)| f.0).collect();
                    }
                }
                if b_row.is_none()
                    && let Some((at, frame)) = new_row(&snap_b)
                {
                    b_row = Some((now, at, frame));
                }
            }
            if a_final.is_none()
                && let Some((at, frame)) = new_row(&ma.ports.shared.agreed_snapshot())
            {
                a_final = Some((now, at, frame));
            }
            // A row that never changes needs no fix: the prediction was F = c (B attested in frame c).
            if let (Some((_, first, _)), Some((_, real, _))) = (a_first, b_row.or(a_final))
                && first == real
                && a_fixed.is_none()
            {
                a_fixed = a_first;
            }
        }
        let since = |t: Option<(Instant, u64, u64)>| t.map(|t| ms(t.0 - t_applied));
        let c = o.frame.expect("the commit's frame");
        let a_final = a_final.unwrap_or_else(|| panic!("rep {rep}: A's final row; pending {attested:?}"));
        let real = b_row.unwrap_or(a_final);
        let f = real.1 - 1;
        assert_eq!(a_final.1, real.1, "rep {rep}: A's agreed row and B's row run from the same frame");
        if let Some((_, fixed, _)) = a_fixed {
            assert_eq!(fixed, real.1, "rep {rep}: A's corrected row runs from F + 1");
        }
        let line = json!({
            "rep": rep,
            "cold": rep == 0,
            "author_call_ms": ms(t_applied - t0),
            "author_first_try_ms": first_try_ms,
            "not_ready_retries": not_ready,
            "c": c,
            "F": f,
            "F_minus_c_frames": f - c,
            "F_minus_c_ms": (f - c) as f64 * period_ms,
            "B_saw_attested_stamps": attested,
            "A_prediction_runs_from": a_first.map(|t| t.1),
            "wall_ms_after_call": {
                "A_prediction_seen": since(a_first),
                "B_saw_pending": b_pending.map(|t| ms(t - t_applied)),
                "B_active": since(b_row),
                "A_corrected": since(a_fixed),
                "A_final": since(Some(a_final)),
            },
        });
        println!("{line}");
        // Let things settle before the next one.
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn wait_for_file(path: &Path, limit: Duration) -> String {
    let end = Instant::now() + limit;
    loop {
        if let Ok(t) = std::fs::read_to_string(path)
            && !t.trim().is_empty()
        {
            return t.trim().to_string();
        }
        assert!(Instant::now() < end, "no {} within {limit:?}", path.display());
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `open <reps> <balls> <build-root> <listen> <advertise> <invite-file> <reply-file>`: A, the author, on this PC.
/// Writes the invite, waits up to 10 minutes for B's reply code in `reply-file`, then commits as `local` does.
fn open(a: Vec<String>) {
    let [reps, balls, root, listen, advertise, invite_file, reply_file] = &a[..] else {
        panic!("open <reps> <balls> <build-root> <listen> <advertise> <invite-file> <reply-file>")
    };
    let (reps, balls): (usize, u32) = (reps.parse().unwrap(), balls.parse().unwrap());
    let root = PathBuf::from(root);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the build root");
    let host = clock::shared();
    let wa = WakeLater::default();
    let advertise = vec![advertise.parse().expect("advertise")];
    let ep =
        Endpoint::bind([21; 32], listen.parse().expect("listen"), advertise, true, host.clone(), Some(wa.arrival()))
            .expect("A binds");
    let invite = ep.invite(Duration::from_secs(600)).unwrap();
    std::fs::write(invite_file, invite.encode()).expect("the invite file");
    eprintln!("ctof: A wrote its invite to {invite_file}; waiting for {reply_file}");
    let reply = Reply::decode(&wait_for_file(Path::new(reply_file), Duration::from_secs(600))).expect("the reply");
    let (ra, ia, stack) = runner();
    let params = OpenParams { seed: 7, balls, rate: RATE, epoch_lead: net::EPOCH_LEAD };
    let sa = ep.open_session(&invite, &reply, &params, Duration::from_secs(60), ra).expect("A's session");
    let ma = start(sa, wa, [-0.5, 1.0, -0.4], root.join("a"), ia, stack, &host);
    std::thread::sleep(net::EPOCH_LEAD + Duration::from_secs(5));
    eprintln!("ctof: {reps} reps, {balls} balls, {} Hz, B on another PC", RATE.hz());
    run_reps(&ma, None, reps);
    eprintln!("ctof: done; stopping");
    ma.attester.stop();
    eprintln!("ctof: A {:?}", ma.sim.stop().map(|s| format!("{s:?}")));
    std::process::exit(0);
}

/// `join <build-root> <listen> <advertise> <invite-file> <reply-file> <seconds>`: B, the attester, headless. Writes
/// its reply code, joins, attests as pe-app's binary does, and logs one JSON line per change it sees: a commit
/// pending (with c) and a row active (with F + 1), by frame and by its own clock.
fn join(a: Vec<String>) {
    let [root, listen, advertise, invite_file, reply_file, seconds] = &a[..] else {
        panic!("join <build-root> <listen> <advertise> <invite-file> <reply-file> <seconds>")
    };
    let root = PathBuf::from(root);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the build root");
    let host = clock::shared();
    let wb = WakeLater::default();
    let advertise = vec![advertise.parse().expect("advertise")];
    let mut ep =
        Endpoint::bind([22; 32], listen.parse().expect("listen"), advertise, false, host.clone(), Some(wb.arrival()))
            .expect("B binds");
    let invite = Invite::decode(&wait_for_file(Path::new(invite_file), Duration::from_secs(60))).expect("the invite");
    std::fs::write(reply_file, ep.reply(&invite).encode()).expect("the reply file");
    ep.dial(&invite).expect("B dials");
    let (rb, ib, stack) = runner();
    let sb = ep.join_session(&invite, RATE, Duration::from_secs(600), rb).expect("B's session");
    let mb = start(sb, wb, [0.5, 1.1, -0.3], root.join("b"), ib, stack, &host);
    let t0 = Instant::now();
    eprintln!("ctof: B joined; build identity {}", String::from_utf8_lossy(net::build_id()));
    let end = t0 + Duration::from_secs(seconds.parse().expect("seconds"));
    let (mut seen, mut pending, mut active) = (0, BTreeSet::new(), BTreeSet::new());
    while Instant::now() < end {
        let (s, snap) = mb.ports.shared.wait_newer(seen, Duration::from_millis(5));
        seen = s;
        let t = ms(t0.elapsed());
        for (id, p) in snap.pending() {
            if pending.insert(*id) {
                let line = json!({"event": "pending", "asset": format!("{:02x}{:02x}", id[0], id[1]), "c": p.commit_frame.0, "frame": snap.frame().0, "t_ms": t});
                println!("{line}");
            }
            if let Some(v) = p.void {
                println!(
                    "{}",
                    json!({"event": "void", "frame": snap.frame().0, "why": format!("{v:?}"), "failed": format!("{:?}", p.failed)})
                );
            }
        }
        for (id, r) in snap.modules() {
            if active.insert(*id) {
                let line = json!({"event": "active", "asset": format!("{:02x}{:02x}", id[0], id[1]), "runs_from": r.activate_at.0, "frame": snap.frame().0, "t_ms": t});
                println!("{line}");
            }
        }
    }
    eprintln!("ctof: B stopping");
    mb.attester.stop();
    eprintln!("ctof: B {:?}", mb.sim.stop().map(|s| format!("{s:?}")));
    std::process::exit(0);
}
