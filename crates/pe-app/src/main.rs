//! pe-app's binary (docs/spec/app.md). It runs a world alone: the simulation thread, with the stand-in agreement
//! (`pe_app::standin`) and pe-replica's replica over M1's ball pit with this machine's member admitted
//! (`pe_app::world`), drawn in VR, where the member's hands follow the headset's tracking, or in the window on the
//! main thread. The network, MCP, configuration and invites join as their crates land.
//!
//! `pe-app [--vr | --headless] [--seconds <n>] [--open | --join <invite>] [--listen <addr>] [--advertise <addr>]...
//! [--identity <path>]`. `--vr` turns VR on, as the configuration will (pe-xr's headset guard): VR opens first, and
//! pe-app goes to the desktop when VR does not open or when its session ends. `--headless` opens neither a window nor
//! VR: the world runs, with no hands, until `--seconds` passes or the console asks to quit (tests, and machines with
//! no display). `--seconds` ends the run after `n` seconds, in any mode; without it the window runs until it is
//! closed.
//!
//! Without `--open` or `--join` the world runs alone (the headset tests need that). With them, machines share one
//! (app.md 4, "Joining from a cut"; `pe_app::net`): `--open` opens the world alone at once and prints an invite, and
//! each line on standard input is a joiner's reply code; `--join <invite>` prints this machine's reply code for the
//! opener, joins at the cut that admits it and imports the world's state there. `--listen` is the UDP address to bind
//! (default 0.0.0.0:4433), `--advertise` an address the invite or reply names (default: the bound one, which must
//! then be specific), and `--identity` this machine's identity file (default: `identity.seed` in the per-user data
//! folder, `%LOCALAPPDATA%\Primordial Engine` on Windows; made on first use).
//!
//! `--drawn-time present:<ms> | a:<ms> | b:<ms>` overrides X8, the drawn time (render.md 7), for tests: Present with
//! that `L_max`, (a) with that `L_max`, or (b) with that `D_render`. By default VR draws Present at two simulation
//! frames and the desktop (a) (@coordinator-opus m4ead69b6e74e, me48e560e5151). `present:` is Present only in VR: the
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
use pe_app::join::T_JOIN;
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
use pe_seams::{Frame, FrameClock, LineId, MemberKey, RigPose, SimRate};
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

/// A world shared with other machines (app.md 4): open it alone, or join from an invite.
enum Shared {
    Open,
    Join(Invite),
}

/// The command line, until configuration lands (app.md 4 step 1).
struct Options {
    vr: bool,
    /// No window and no VR (app.md 4): the main thread waits for `--seconds` or the console's quit.
    headless: bool,
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
    /// `--inject-fault-at`: the frame at which this machine injects a deliberate test fault (pe-app's `fault`).
    inject_fault_at: Option<Frame>,
    /// `--arm-fault`: the console's typed `fault` injects that test fault at the next frame.
    arm_fault: bool,
    /// `--check-replay`: the opener keeps its final log, and the console's `replay` replays it (pe-app's `replay`).
    check_replay: bool,
}

/// The joiner's wait for the opener's welcome and its clock: a person copies the reply over.
const JOIN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long an invite stays valid.
const INVITE_VALID: Duration = Duration::from_secs(3600);

fn options() -> Options {
    let mut o = Options {
        vr: false,
        headless: false,
        run_for: None,
        shared: None,
        listen: "0.0.0.0:4433".parse().unwrap_or_else(|_| usage()),
        advertise: Vec::new(),
        identity: None,
        mcp_pipe: false,
        mcp_http: false,
        agent: Vec::new(),
        drawn_time: None,
        inject_fault_at: None,
        arm_fault: false,
        check_replay: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--vr" => o.vr = true,
            "--headless" => o.headless = true,
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
            "--arm-fault" => o.arm_fault = true,
            "--check-replay" => o.check_replay = true,
            "--inject-fault-at" => {
                o.inject_fault_at =
                    Some(Frame(args.next().and_then(|n| n.parse::<u64>().ok()).unwrap_or_else(|| usage())));
            }
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

/// The console (app.md 4): each line it reads is a command. `fault` fires the test fault `--arm-fault` armed, and is
/// refused without it (pe-app's `fault`). `replay` replays the opener's session from frame 0, with `--check-replay`
/// (`replay` true; pe-app's `replay`). On A, any other line is a joiner's reply code, for the session's hook, which
/// checks it. The thread waits on standard input for the whole run; the process's end ends it.
fn read_console(replies: Option<std::sync::mpsc::Sender<Reply>>, replay: bool) {
    let read = move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line == "replay" {
                if replay {
                    pe_app::replay::request();
                } else {
                    eprintln!("pe-app: `replay` is refused: this machine wasn't started with --check-replay");
                }
                continue;
            }
            if line == "fault" {
                match pe_app::fault::on_command(pe_app::fault::armed().as_deref()) {
                    Ok(said) => println!("{said}"),
                    Err(e) => eprintln!("pe-app: {e}"),
                }
                continue;
            }
            let Some(replies) = &replies else {
                eprintln!(
                    "pe-app: `{line}` isn't a command here: `fault` (with --arm-fault) or `replay` (--check-replay)"
                );
                continue;
            };
            match Reply::decode(line) {
                Ok(reply) => {
                    if replies.send(reply).is_err() {
                        return;
                    }
                }
                Err(e) => eprintln!("pe-app: that isn't a reply code ({e:?}): paste the code the joiner printed"),
            }
        }
    };
    if let Err(e) = std::thread::Builder::new().name("pe-console".into()).spawn(read) {
        eprintln!("pe-app: the console can't be read ({e}): nobody can join, and `fault` isn't heard");
    }
}

/// The shared session (app.md 4 steps 4 and 5): open alone or join at a cut, then the simulation over pe-agree and
/// pe-net. A joiner's simulation starts with its import of the cut's state under way; if that fails, `console`
/// quits.
#[allow(clippy::too_many_arguments)]
fn shared_sim(
    o: &Options,
    shared: &Shared,
    host: &clock::HostClock,
    wake: &WakeLater,
    slot: Arc<FrameSlot>,
    hands: Arc<SwitchedHands>,
    runner: Box<dyn ScriptRunner>,
    console: &Arc<Quit>,
) -> Sim {
    let rate = SimRate::DEFAULT;
    let path = o.identity.clone().map_or_else(net::default_identity_path, Ok).unwrap_or_else(|e| fail(&e.to_string()));
    // The build service's folder follows this path too (start_builds).
    let seed =
        net::identity_seed(&path).unwrap_or_else(|e| fail(&format!("the identity file {}: {e}", path.display())));
    let opener = matches!(shared, Shared::Open);
    let mut ep = Endpoint::bind(seed, o.listen, o.advertise.clone(), opener, host.clone(), Some(wake.arrival()))
        .unwrap_or_else(|e| fail(&e.to_string()));
    let short = |k: MemberKey| k.0[..4].iter().map(|b| format!("{b:02x}")).collect::<String>();
    // pe-diverge (app.md 3, step 5): detection and repair, with fault bundles beside the identity file. It also serves
    // the opener's state to a joiner, and fetches it for the joiner.
    let diverge = |sim: Sim| {
        sim.with_diverge(pe_diverge::Config::default(), Some(pe_app::diverge::bundle_dir(&path)))
            .unwrap_or_else(|e| fail(&format!("pe-diverge did not start: {e}")))
    };
    match shared {
        Shared::Open => {
            let p = OpenParams { seed: WORLD_SEED, balls: 500, rate, epoch_lead: Duration::ZERO };
            let opened = ep
                .open_alone(&p, INVITE_VALID, T_JOIN, runner)
                .unwrap_or_else(|e| fail(&format!("the session did not start: {e}")));
            let s = opened.session;
            println!("session: open, this machine {}", short(s.me));
            println!("invite: {}", opened.invite.encode());
            println!("to let someone join, paste their reply code here:");
            read_console(Some(opened.replies), o.check_replay);
            let sim = diverge(
                Sim::new(Box::new(s.node), s.replica, s.frames, slot, Budget::new(rate))
                    .with_local(s.me, chain_nonce(), hands)
                    .with_transport(net::transport(s.handle), Some(Box::new(s.hook)))
                    .with_linked(s.linked),
            );
            // `--check-replay` (pe-app's `replay`): the opener keeps its final log, and replays it on the console's
            // `replay`.
            if o.check_replay {
                println!("replay: keeping this session's final log; type `replay` to replay it from frame 0");
                sim.with_replay_check(pe_app::replay::ReplayLog::new(opened.genesis))
            } else {
                sim
            }
        }
        Shared::Join(invite) => {
            println!("reply code for the opener: {}", ep.reply(invite).encode());
            ep.dial(invite).unwrap_or_else(|e| fail(&e.to_string()));
            let cj = ep
                .join_at_cut(invite, rate, JOIN_TIMEOUT, T_JOIN)
                .unwrap_or_else(|e| fail(&format!("couldn't join the session: {e}")));
            println!(
                "session: joined at frame {}, {} members, this machine {}",
                cj.e.at.frame.0,
                cj.members.len(),
                short(cj.me)
            );
            if let Some(at) = pe_app::fault::armed().and_then(|f| f.at()).filter(|at| *at < cj.e.at.frame) {
                println!(
                    "FAULT INJECTION: frame {} is before this machine's first frame, {}, so the fault never fires",
                    at.0, cj.e.at.frame.0
                );
            }
            read_console(None, false);
            let replica = cj
                .record
                .empty_replica(cj.me, cj.frames.clone(), runner)
                .unwrap_or_else(|e| fail(&format!("the replica did not start: {e}")));
            let linked = cj.members.iter().copied().filter(|m| *m != cj.me).collect();
            let quit = console.clone();
            let mut sim = diverge(
                Sim::new(Box::new(cj.node), replica, cj.frames, slot, Budget::new(rate))
                    .with_local(cj.me, chain_nonce(), hands)
                    .with_transport(net::transport_with(cj.handle, cj.pending), Some(Box::new(cj.hook)))
                    .with_linked(linked),
            )
            .with_on_join_failed(Box::new(move || quit.request_from_app("the join failed")));
            sim.join_at_final(MemberKey(invite.key), host())
                .unwrap_or_else(|e| fail(&format!("couldn't join the session: {e}")));
            sim
        }
    }
}

fn main() {
    // First, before any thread starts (console.rs): Ctrl+C or closing the console quits gracefully.
    let console = Quit::install().unwrap_or_else(|e| {
        eprintln!("pe-app: {e}; Ctrl+C ends the process without leaving");
        Quit::new()
    });
    let options = options();
    // Others share the world (`--open` or `--join`): the quit then leaves gracefully (app.md 4).
    // Which sources this build came from, and its commit (app.md 4, "The build id").
    println!("{}", pe_app::net::source_line());
    // Which pe-sdk scripts build against: PE_SDK, beside the executable, or the source tree (builds::sdk).
    match pe_app::builds::sdk_line() {
        Ok(line) => println!("{line}"),
        Err(warning) => eprintln!("{warning}"),
    }
    // A deliberate test fault on this machine only (M1 demo step 7, pe-app's `fault`), armed before any replica is made.
    // `--check-replay` replays from the genesis record, which only the opener has, and a test fault would replay too.
    if options.check_replay && !matches!(options.shared, Some(Shared::Open)) {
        fail("--check-replay needs --open: only the opener's session starts at frame 0");
    }
    if options.check_replay && (options.arm_fault || options.inject_fault_at.is_some()) {
        fail("--check-replay excludes a test fault: the replay would compute the fault again");
    }
    let fault = match (options.inject_fault_at, options.arm_fault) {
        (Some(_), true) => fail("--inject-fault-at and --arm-fault exclude each other"),
        (Some(at), false) => Some(pe_app::fault::Fault::new(at)),
        (None, true) => Some(pe_app::fault::Fault::typed()),
        (None, false) => None,
    };
    if let Some(fault) = fault {
        match pe_app::fault::arm(fault) {
            Ok(line) => println!("{line}"),
            Err(e) => fail(&e),
        }
    }
    let networked = options.shared.is_some();
    let (vr, headless, run_for) = (options.vr, options.headless, options.run_for);
    if vr && headless {
        fail("--vr and --headless exclude each other");
    }
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
        Some(shared) => shared_sim(&options, shared, &host, &wake, slot.clone(), hands.clone(), runner, &console),
        None => {
            let frames: Arc<dyn FrameClock> = Arc::new(LocalClock::new(host(), rate));
            let pit = world::open_ball_pit(WORLD_SEED, world::DEV_MEMBER, rate, frames.clone(), runner);
            read_console(None, false);
            let replica = match pit {
                Ok(r) => r,
                Err(e) => fail(&format!("the replica did not start: {e}")),
            };
            Sim::new(Box::new(SoloAgreement::new(LineId::origin())), replica, frames, slot.clone(), Budget::new(rate))
                .with_local(world::DEV_MEMBER, chain_nonce(), hands.clone())
        }
    };
    // pe-api's ports (app.md 3.1) whenever the replica is pe-world's and this machine has a member: the build thread
    // submits its attestations through their edit queue (@engine-architect m668e8458cf86). The MCP thread only when a
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
    // view_capture's queue (api.md 8), served by the headset's loop or the window's, whichever draws. Headless draws
    // nothing, so it has no queue, and pe-api answers view_capture with `capture_unavailable` at once.
    let captures = (mcp_wanted && !headless).then(pe_render::capture_service::CaptureQueue::new);
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

    if headless {
        // No window and no VR (app.md 4): the world runs until `--seconds` passes or the console asks to quit, and the
        // main thread runs the quit itself, as in VR. The desktop hand has no window to feed it, so no hands move.
        console.poll();
        wait_headless(run_for.map(|d| started + d), &console);
        console.quitting();
        shutdown();
        console.finished();
        return;
    }

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

/// Headless's wait (app.md 4): until `deadline` passes or the console asks to quit, in short slices so a request is
/// seen within one. Without a deadline it waits for the console alone.
fn wait_headless(deadline: Option<Instant>, console: &Quit) {
    const SLICE: Duration = Duration::from_millis(100);
    while !console.requested() {
        let left = deadline.map_or(SLICE, |t| t.saturating_duration_since(Instant::now()).min(SLICE));
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left);
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
        let sdk = builds::sdk().clone()?.0;
        let config = builds::BuildConfig::new(toolchain, sdk, builds::build_dir(&identity));
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
        "usage: pe-app [--vr | --headless] [--seconds <n>] [--open | --join <invite>] [--listen <addr>] \
         [--advertise <addr>]... [--identity <path>] [--drawn-time present:<ms> (VR)|a:<ms>|b:<ms>] [--mcp-pipe] \
         [--mcp-http] [--arm-fault | --inject-fault-at <frame>] [--check-replay] [--agent <command> <args>...]",
    )
}

fn fail(msg: &str) -> ! {
    eprintln!("pe-app: {msg}");
    std::process::exit(2)
}
