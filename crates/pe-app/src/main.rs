//! pe-app's binary (docs/spec/app.md). It runs a world alone: the simulation thread, with the stand-in agreement
//! (`pe_app::standin`) and pe-replica's replica over M1's ball pit with this machine's member admitted
//! (`pe_app::world`), drawn in VR, where the member's hands follow the headset's tracking, or in the window on the
//! main thread. The network, MCP, configuration and invites join as their crates land.
//!
//! `pe-app [--vr] [--seconds <n>] [--open | --join <invite>] [--listen <addr>] [--advertise <addr>]... [--identity <path>]`.
//! `--vr` turns VR on, as the configuration will (pe-xr's headset guard): VR opens first, and pe-app goes to the
//! desktop when VR does not open or when its session ends. `--seconds` ends the run after `n` seconds, in VR or on the
//! desktop; without it the window runs until it is closed.
//!
//! Without `--open` or `--join` the world runs alone (the headset tests need that). With them, two machines share one
//! (the M1 interim, `pe_app::net`): `--open` prints an invite and waits for the joiner's reply code on standard input;
//! `--join <invite>` prints this machine's reply code for the opener and joins. `--listen` is the UDP address to bind
//! (default 0.0.0.0:4433), `--advertise` an address the invite or reply names (default: the bound one, which must
//! then be specific), and `--identity` this machine's identity file (default: `identity.seed` in the per-user data
//! folder, `%LOCALAPPDATA%\Primordial Engine` on Windows; made on first use).
//!
//! `--drawn-time present:<ms> | a:<ms> | b:<ms>` overrides X8, the drawn time (render.md 7), for tests: Present with
//! that `L_max`, (a) with that `L_max`, or (b) with that `D_render`. By default VR draws Present at two simulation
//! frames and the desktop (a) (@coordinator-opus). `present:` is Present only in VR: the
//! desktop's display time is the next refresh, not now, so there it is (a) with the measured display period added to
//! `L_max` (render.md 7; @shader-reviewer's nit on 197fc122). Every run ends by printing the
//! presenter's counters (`display:`), and a VR run the measured display lead too.
//!
//! `--mcp-pipe` and `--mcp-http` start pe-mcp's transports over pe-api, so an agent can read and edit the world for
//! this machine's member (app.md 3.1); `--agent <command> <args>...`, the rest of the line, launches it with the
//! transport's address and a token in its environment.

use std::fmt::Debug;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pe_app::api;
use pe_app::builds;
use pe_app::clock::{self, LocalClock};
use pe_app::console::{EXIT_INTERRUPTED, Quit, REQUEST_QUIT_LIMIT, Shutdown};
use pe_app::local::{SwitchedHands, chain_nonce};
use pe_app::net::{self, Endpoint, OpenParams, WakeLater};
use pe_app::sim::{self, Budget, Sim};
use pe_app::standin::SoloAgreement;
use pe_app::vr::{self, XrWait};
use pe_app::world;
use pe_mcp::auth::Grant;
use pe_mcp::host::HostOpts;
use pe_net_crypto::identity::{Invite, Reply};
use pe_render::desktop_hand::{DesktopHandFeed, DesktopHandSettings};
use pe_render::display::DisplaySettings;
use pe_render::drawn_time::DrawnTime;
use pe_render::effects::{EffectCommand, EffectSettings};
use pe_render::gpu::ContextError;
use pe_render::present::{FrameSlot, Presenter};
use pe_render::scene::{Environment, RenderSettings};
use pe_render::window::{DesktopConfig, DesktopReport, EffectSink, run_desktop};
use pe_replica::ScriptRunner;
use pe_replica::logic::LogicRunner;
use pe_seams::world_api::{MemberId, Producer, ProducerKind};
use pe_seams::{FrameClock, LineId, MemberKey, RigPose, SimRate};
use pe_xr::guard::Guard;
use pe_xr::loader::SystemDiscovery;
use pe_xr::ring::{DEFAULT_WINDOW_NS, HandRing};

/// The simulation thread's least stack (app.md 1). It gets more when the script runner needs it: the replica steps
/// scripts on it, and wasmtime checks its stack limit against the stack it entered on (`LogicRunner::thread_stack`,
/// from the same pe-logic `Settings` the runner runs guests with).
const SIM_STACK: usize = 4 << 20;

/// A new world's seed, until configuration names the world and its settings (app.md 4, step 1). Its cut grid is this
/// build's consensus setting, solo or in a session (genesis.rs `replica_config`).
const WORLD_SEED: u64 = 0;

/// The headset's display period until its frame loop reports the runtime's own (`HandRing::set_display_period`).
const VR_PERIOD_NS: i64 = 11_111_111;

/// M1 counts and logs effects; the sound library comes later (app.md 8).
struct LogEffects;

impl EffectSink for LogEffects {
    fn command(&mut self, c: &EffectCommand) {
        eprintln!("effect: {c:?}");
    }
}

/// A world shared with a second machine (the M1 interim): open it, or join from an invite.
enum Shared {
    Open,
    Join(Invite),
}

/// The command line, until configuration lands (app.md 4 step 1).
struct Options {
    vr: bool,
    run_for: Option<Duration>,
    shared: Option<Shared>,
    listen: std::net::SocketAddr,
    advertise: Vec<std::net::SocketAddr>,
    identity: Option<std::path::PathBuf>,
    /// pe-mcp's transports for an agent (app.md 3.1): a pipe, loopback HTTP, or both; none runs no MCP thread.
    mcp_pipe: bool,
    mcp_http: bool,
    /// The agent's command line, launched with the transport's address and a token in its environment.
    agent: Vec<String>,
    /// `--drawn-time`: X8 for every presenter, in place of the defaults (VR Present, the desktop (a)).
    drawn_time: Option<DrawnTime>,
}

/// The joiner's wait for the opener to take its reply, admit it and start frame 0: a person copies the reply over.
const JOIN_TIMEOUT: Duration = Duration::from_secs(300);
/// The opener's wait, once it has the reply, for the joiner's link.
const OPEN_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an invite stays valid.
const INVITE_VALID: Duration = Duration::from_secs(3600);

fn options() -> Options {
    let mut o = Options {
        vr: false,
        run_for: None,
        shared: None,
        listen: "0.0.0.0:4433".parse().unwrap_or_else(|_| usage()),
        advertise: Vec::new(),
        identity: None,
        mcp_pipe: false,
        mcp_http: false,
        agent: Vec::new(),
        drawn_time: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--vr" => o.vr = true,
            "--open" => o.shared = Some(Shared::Open),
            "--join" => {
                let text = args.next().unwrap_or_else(|| usage());
                let invite = Invite::decode(&text).unwrap_or_else(|e| fail(&format!("the invite: {e:?}")));
                o.shared = Some(Shared::Join(invite));
            }
            "--listen" => o.listen = args.next().and_then(|a| a.parse().ok()).unwrap_or_else(|| usage()),
            "--advertise" => o.advertise.push(args.next().and_then(|a| a.parse().ok()).unwrap_or_else(|| usage())),
            "--identity" => o.identity = Some(args.next().unwrap_or_else(|| usage()).into()),
            "--mcp-pipe" => o.mcp_pipe = true,
            "--mcp-http" => o.mcp_http = true,
            // The rest of the command line is the agent's.
            "--agent" => o.agent = args.by_ref().collect(),
            "--drawn-time" => o.drawn_time = Some(args.next().and_then(|a| drawn_time(&a)).unwrap_or_else(|| usage())),
            "--seconds" => {
                let s = args.next().and_then(|n| n.parse::<f64>().ok()).filter(|s| *s > 0.0);
                o.run_for = Some(s.and_then(|s| Duration::try_from_secs_f64(s).ok()).unwrap_or_else(|| usage()));
            }
            _ => usage(),
        }
    }
    o
}

/// `present:<ms>`, `a:<ms>` or `b:<ms>` (milliseconds, may be fractional).
fn drawn_time(arg: &str) -> Option<DrawnTime> {
    let (rule, ms) = arg.split_once(':')?;
    let d = Duration::try_from_secs_f64(ms.parse::<f64>().ok()? / 1e3).ok()?;
    match rule {
        "present" => Some(DrawnTime::Present { l_max: d }),
        "a" => Some(DrawnTime::Extrapolate { l_max: d }),
        "b" => Some(DrawnTime::InterpolateLate { d_render: d }),
        _ => None,
    }
}

/// A presenter drawing by `drawn_time`.
fn presenter(rate: SimRate, drawn_time: DrawnTime) -> Presenter {
    let display = DisplaySettings { drawn_time, ..DisplaySettings::default() };
    Presenter::new(display, EffectSettings::default(), Environment::default(), rate.hz())
}

/// The M1 interim's session (app.md 4 step 4): open or join, then the simulation over pe-agree and pe-net.
fn shared_sim(
    o: &Options,
    shared: &Shared,
    host: &clock::HostClock,
    wake: &WakeLater,
    slot: Arc<FrameSlot>,
    hands: Arc<SwitchedHands>,
    runner: Box<dyn ScriptRunner>,
) -> Sim {
    let rate = SimRate::DEFAULT;
    let path = o.identity.clone().map_or_else(net::default_identity_path, Ok).unwrap_or_else(|e| fail(&e.to_string()));
    // The build service's folder follows this path too (start_builds).
    let seed =
        net::identity_seed(&path).unwrap_or_else(|e| fail(&format!("the identity file {}: {e}", path.display())));
    let opener = matches!(shared, Shared::Open);
    let mut ep = Endpoint::bind(seed, o.listen, o.advertise.clone(), opener, host.clone(), Some(wake.arrival()))
        .unwrap_or_else(|e| fail(&e.to_string()));
    let session = match shared {
        Shared::Open => {
            let invite = ep.invite(INVITE_VALID).unwrap_or_else(|e| fail(&e.to_string()));
            println!("invite: {}", invite.encode());
            println!("waiting for the second player's reply code (paste it here):");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            let reply = Reply::decode(line.trim()).unwrap_or_else(|e| fail(&format!("the reply code: {e:?}")));
            let p = OpenParams { seed: WORLD_SEED, balls: 500, rate, epoch_lead: net::EPOCH_LEAD };
            ep.open_session(&invite, &reply, &p, OPEN_TIMEOUT, runner)
        }
        Shared::Join(invite) => {
            println!("reply code for the opener: {}", ep.reply(invite).encode());
            ep.dial(invite).unwrap_or_else(|e| fail(&e.to_string()));
            ep.join_session(invite, rate, JOIN_TIMEOUT, runner)
        }
    }
    .unwrap_or_else(|e| fail(&format!("the session did not start: {e}")));
    println!(
        "session: {} members, this machine {}",
        session.members.len(),
        session.me.0[..4].iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    // With two members, both are needed for finality: if one leaves, the other's finality stops until both restart.
    // pe-diverge (app.md 3, step 5): detection and repair, with fault bundles beside the identity file.
    Sim::new(Box::new(session.node), session.replica, session.frames, slot, Budget::new(rate))
        .with_local(session.me, chain_nonce(), hands)
        .with_transport(net::transport(session.handle), Some(Box::new(session.hook)))
        .with_diverge(pe_diverge::Config::default(), Some(pe_app::diverge::bundle_dir(&path)))
        .unwrap_or_else(|e| fail(&format!("pe-diverge did not start: {e}")))
}

fn main() {
    // First, before any thread starts (console.rs): Ctrl+C or closing the console quits gracefully.
    let console = Quit::install().unwrap_or_else(|e| {
        eprintln!("pe-app: {e}; Ctrl+C ends the process without leaving");
        Quit::new()
    });
    let options = options();
    // Others share the world (`--open` or `--join`): the quit then leaves gracefully (app.md 4).
    let networked = options.shared.is_some();
    let (vr, run_for) = (options.vr, options.run_for);
    let started = Instant::now();

    // Each GPU context's backend, adapter and driver, as it is made (render.md 4).
    pe_render::gpu::set_context_log(|d| println!("render: {d}"));
    let rate = SimRate::DEFAULT;
    let host = clock::shared();
    let slot = Arc::new(FrameSlot::new());
    // The script runner (pe-replica's `logic`): pe-app owns it, and the simulation thread gets the stack it needs.
    // Its settings are the ones the hello's build id names (net.rs `runner_settings`), so peers compare this runner.
    let (settings, run) = pe_app::net::runner_settings();
    let runner = match LogicRunner::new(settings, run) {
        Ok(r) => r,
        Err(e) => fail(&format!("the script runner did not start: {e}")),
    };
    let sim_stack = SIM_STACK.max(runner.thread_stack());
    // The build service loads modules into the runner from its own thread (builds.rs), through the runner's inbox,
    // which is taken before the replica owns the runner.
    let inbox = runner.inbox();
    let runner: Box<dyn ScriptRunner> = Box::new(runner);
    // The local player's hands (app.md 3, step 2): VR's ring while an XR session runs, the desktop hand otherwise.
    let ring = Arc::new(HandRing::new(DEFAULT_WINDOW_NS, VR_PERIOD_NS));
    let desktop_hand = Arc::new(DesktopHandFeed::default());
    let hands = Arc::new(SwitchedHands::new(ring.clone(), desktop_hand.clone()));
    let wake = WakeLater::default();
    let sim = match &options.shared {
        Some(shared) => shared_sim(&options, shared, &host, &wake, slot.clone(), hands.clone(), runner),
        None => {
            let frames: Arc<dyn FrameClock> = Arc::new(LocalClock::new(host(), rate));
            let pit = world::open_ball_pit(WORLD_SEED, world::DEV_MEMBER, rate, frames.clone(), runner);
            let replica = match pit {
                Ok(r) => r,
                Err(e) => fail(&format!("the replica did not start: {e}")),
            };
            Sim::new(Box::new(SoloAgreement::new(LineId::origin())), replica, frames, slot.clone(), Budget::new(rate))
                .with_local(world::DEV_MEMBER, chain_nonce(), hands.clone())
        }
    };
    // pe-api's ports (app.md 3.1) whenever the replica is pe-world's and this machine has a member: the build thread
    // submits its attestations through their edit queue (@engine-architect). The MCP thread only when a
    // transport is asked for.
    let mcp_wanted = options.mcp_pipe || options.mcp_http;
    if !mcp_wanted && !options.agent.is_empty() {
        fail("--agent needs --mcp-pipe or --mcp-http");
    }
    let me = sim.local_member();
    let ports = {
        let arrival = wake.arrival();
        sim.ports(WORLD_NAME, move || arrival())
    };
    if mcp_wanted && ports.is_none() {
        fail("pe-api needs a pe-world replica");
    }
    let sim = match &ports {
        Some(p) => sim.with_api(p.clone()),
        None => sim,
    };
    let sim = match sim::spawn(sim, host.clone(), sim_stack) {
        Ok(t) => t,
        Err(e) => fail(&format!("the simulation thread did not start: {e}")),
    };
    wake.set(sim.waker());
    let builds = ports.as_ref().zip(me).and_then(|(p, me)| start_builds(inbox, p, me, options.identity.as_deref()));
    let (attester, builder) = match builds {
        Some((a, b)) => (Some(a), Some(b)),
        None => (None, None),
    };
    // pe-api builds through the attester's own service (api.md 7.1): one build at a time and one cache by V.
    let ports = match (ports, builder) {
        (Some(p), Some(b)) => Some(p.with_builds(b)),
        (p, _) => p,
    };
    // view_capture's queue (api.md 8), served by the headset's loop or the window's, whichever draws.
    let captures = mcp_wanted.then(pe_render::capture_service::CaptureQueue::new);
    let ports = match (ports, &captures) {
        (Some(p), Some(q)) => Some(p.with_captures(Arc::new(pe_app::capture::RenderCaptures(q.clone())))),
        (p, _) => p,
    };
    let mcp = ports.filter(|_| mcp_wanted).map(|p| {
        let member = me.unwrap_or_else(|| fail("an agent acts for this machine's member, and there is none"));
        match api::spawn_mcp(p.api(), mcp_opts(&options, member)) {
            Ok(t) => t,
            Err(e) => fail(&format!("the MCP thread did not start: {e}")),
        }
    });

    // The quit (app.md 4), run once: by the main thread at the end of the run, or by the console's request.
    let quit_captures = captures.clone();
    let shutdown: Shutdown = Box::new(move || {
        close_captures(&quit_captures);
        stop_mcp(mcp);
        stop_builds(attester);
        finish(quit(sim, networked));
    });

    if vr {
        console.poll();
        hands.use_vr(true);
        let guard = Guard::from_env(true);
        let effects = Box::new(LogEffects);
        let report = vr::ReportSlot::default();
        let drawn_time = options.drawn_time.unwrap_or_else(|| DrawnTime::present(rate.hz()));
        match vr::start(
            &SystemDiscovery,
            &guard,
            ring,
            slot.clone(),
            vr::Drawing {
                presenter: presenter(rate, drawn_time),
                render: RenderSettings::default(),
                effects,
                captures: captures.clone(),
                report: report.clone(),
            },
        ) {
            Ok(xr) => {
                // TODO(pe-render): in VR mode the main thread pumps a window's events and input, the desktop window
                // draws on the XR device while the session is idle or stopping (xr.md 3.1), and the XR thread draws
                // the desktop mirror (app.md 1, render.md 10). Until pe-render has that window the main thread waits
                // here; xr.md 3.3's retry after a lost session comes with it.
                let waited = wait_vr(&xr, run_for.map(|d| started + d), &console);
                println!("vr: {waited:?}; at its end {:?}", xr.stop());
                match report.lock().ok().and_then(|mut r| r.take()) {
                    Some(r) => {
                        println!("vr display lead (ms): {:?}", r.lead_ms);
                        println!("display (vr, {drawn_time:?}): {:?}", r.render);
                    }
                    None => println!("display (vr): no report (the XR thread has not ended)"),
                }
                if waited == XrWait::Running || console.requested() {
                    console.quitting();
                    shutdown();
                    console.finished();
                    return;
                }
                eprintln!("pe-app: the VR session ended; the desktop instead");
            }
            Err(why) => eprintln!("pe-app: {why}; the desktop instead"),
        }
        hands.use_vr(false);
    }

    // A request during VR, or before the window: quit now. Otherwise a request runs the quit on its own thread.
    if let Some(now) = console.arm(shutdown) {
        now();
        console.finished();
        return;
    }
    let cfg = DesktopConfig {
        title: "Primordial Engine".into(),
        size: [1280, 720],
        present_mode: pe_render::wgpu::PresentMode::Fifo,
        vertical_fov: 1.0,
        clock: host,
        presenter: presenter(rate, options.drawn_time.unwrap_or_default()),
        render: RenderSettings::default(),
        hand: DesktopHandSettings::default(),
        eye: RigPose { position: [0.0, 1.4, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
        // What is left of the run after VR, never zero, which the window would take as no time at all.
        run_for: run_for.map(|d| d.saturating_sub(started.elapsed()).max(Duration::from_millis(1))),
        snapshot: None,
        captures: captures.clone(),
    };
    // The window, its surface and its GPU context are made on this thread, one context at a time (app.md 4): the XR
    // thread has dropped its own context before `wait` answers.
    let window = run_desktop(cfg, slot, desktop_hand, Box::new(LogEffects));
    report_window(window);
    match console.take() {
        Some(shutdown) => {
            shutdown();
            console.finished();
        }
        // A console request is running the quit, and ends the process when it is done; if it hasn't within the leave's
        // longest bound (a panic in it), end the process here.
        None => {
            let done = console.wait_quit(REQUEST_QUIT_LIMIT);
            if !done {
                eprintln!("pe-app: the console's quit did not finish; ending now");
            }
            std::process::exit(if done { 0 } else { EXIT_INTERRUPTED });
        }
    }
}

/// VR's wait (app.md 4): until the session ends, `--seconds` passes or the console asks to quit, in short slices so a
/// request is seen within one.
fn wait_vr(xr: &vr::XrThread, deadline: Option<Instant>, console: &Quit) -> XrWait {
    const SLICE: Duration = Duration::from_millis(100);
    loop {
        let left = deadline.map_or(SLICE, |t| t.saturating_duration_since(Instant::now()).min(SLICE));
        let waited = xr.wait(left);
        let over = deadline.is_some_and(|t| Instant::now() >= t);
        if waited != XrWait::Running || over || console.requested() {
            return waited;
        }
    }
}

/// The quit (app.md 4), once MCP and the attester have stopped. In a session with others this member leaves
/// gracefully: its `Leave` goes out, it goes on voting until every remaining member has confirmed it or the quit's
/// bound passes, and the console says so ("leaving…"). A world alone just stops.
fn quit(sim: sim::SimThread, networked: bool) -> std::thread::Result<sim::SimStats> {
    if !networked {
        return sim.stop();
    }
    sim.leave_and_stop(&mut |line| println!("{line}")).1
}

/// The world's name as pe-api reports it, until the invite or the configuration names it (api.md 5.1).
const WORLD_NAME: &str = "M1 ball pit";

/// pe-mcp's options: the transports asked for, and the agent's command line and grant. The agent acts for this
/// machine's member, as an agent (pe-world's `Actor::Agent`), named by its command.
fn mcp_opts(o: &Options, member: MemberKey) -> HostOpts {
    let harness = o.agent.first().map(|c| {
        let name = std::path::Path::new(c).file_stem().map(|s| s.to_string_lossy().into_owned());
        name.unwrap_or_else(|| c.clone())
    });
    let producer = Producer { kind: ProducerKind::Agent, harness, principal: None, requested_by: None };
    let mut opts = HostOpts::new(Grant { member: MemberId::from(member), producer, confirm: false });
    opts.pipe = o.mcp_pipe;
    opts.http = o.mcp_http;
    opts.cmd = o.agent.clone();
    opts
}

/// The build service and its attester (builds.rs): pe-logic's toolchain (`Toolchain::current` until D75's installed
/// copy), the engine's pe-sdk, and the build folder beside this machine's identity file, the `--identity` given or the
/// default (`builds::build_dir`). Without a toolchain this machine still runs but builds no script: it attests none (at
/// n = 2 a commit then voids at its deadline), and pe-api's `code_check` answers `unavailable`.
fn start_builds(
    inbox: pe_replica::logic::ModuleInbox,
    ports: &api::Ports,
    me: MemberKey,
    identity: Option<&std::path::Path>,
) -> Option<(builds::AttesterThread, Arc<builds::Builder>)> {
    let started = (|| -> Result<_, String> {
        let toolchain = pe_logic::build::Toolchain::current().map_err(|e| e.to_string())?;
        let identity =
            identity.map_or_else(net::default_identity_path, |p| Ok(p.to_path_buf())).map_err(|e| e.to_string())?;
        let config = builds::BuildConfig::new(toolchain, pe_logic::build::engine_sdk(), builds::build_dir(&identity));
        let builder = Arc::new(builds::Builder::new(config, inbox.clone()).map_err(|e| e.to_string())?);
        let thread = builds::spawn_attester(builder.clone(), inbox, ports.shared.clone(), ports.queue.clone(), me)
            .map_err(|e| e.to_string())?;
        Ok((thread, builder))
    })();
    started
        .map_err(|e| eprintln!("pe-app: the build service did not start, so this machine builds no script: {e}"))
        .ok()
}

fn stop_builds(attester: Option<builds::AttesterThread>) {
    if let Some(a) = attester {
        a.stop();
    }
}

/// Refuses any capture still waiting, so an agent's view_capture doesn't hold the MCP thread, and with it the quit,
/// until its deadline; the queue stays closed, so a later order is refused at once. At the end of a run nothing draws
/// any more by then. On a console request the window may still draw while the quit runs, but it serves no capture
/// after this.
fn close_captures(captures: &Option<Arc<pe_render::capture_service::CaptureQueue>>) {
    if let Some(q) = captures {
        q.close();
    }
}

fn stop_mcp(mcp: Option<api::McpThread>) {
    if let Some(m) = mcp {
        match m.stop() {
            Ok(exited) => println!("mcp: stopped; the agent {exited:?}"),
            Err(e) => eprintln!("mcp: {e}"),
        }
    }
}

fn report_window(window: Result<DesktopReport, ContextError>) {
    match window {
        Ok(r) => {
            println!("window: {} frames on {}, {} skipped", r.frames, r.adapter, r.skipped);
            if let Some(d) = &r.render {
                println!("display (desktop): {d:?}");
            }
        }
        Err(e) => eprintln!("window: {e:?}"),
    }
}

fn finish<S: Debug, E>(stats: Result<S, E>) {
    match stats {
        Ok(s) => println!("simulation: {s:?}"),
        Err(_) => fail("the simulation thread panicked"),
    }
}

fn usage() -> ! {
    fail(
        "usage: pe-app [--vr] [--seconds <n>] [--open | --join <invite>] [--listen <addr>] [--advertise <addr>]... \
         [--identity <path>] [--drawn-time present:<ms> (VR)|a:<ms>|b:<ms>] [--mcp-pipe] [--mcp-http] \
         [--agent <command> <args>...]",
    )
}

fn fail(msg: &str) -> ! {
    eprintln!("pe-app: {msg}");
    std::process::exit(2)
}
