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
//! `--mcp-pipe` and `--mcp-http` start pe-mcp's transports over pe-api, so an agent can read and edit the world for
//! this machine's member (app.md 3.1); `--agent <command> <args>...`, the rest of the line, launches it with the
//! transport's address and a token in its environment.

use std::fmt::Debug;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pe_app::api;
use pe_app::clock::{self, LocalClock};
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
use pe_render::effects::{EffectCommand, EffectSettings};
use pe_render::gpu::ContextError;
use pe_render::present::{FrameSlot, Presenter};
use pe_render::scene::{Environment, RenderSettings};
use pe_render::window::{DesktopConfig, DesktopReport, EffectSink, run_desktop};
use pe_seams::world_api::{MemberId, Producer, ProducerKind};
use pe_seams::{FrameClock, LineId, MemberKey, RigPose, SimRate};
use pe_xr::guard::Guard;
use pe_xr::loader::SystemDiscovery;
use pe_xr::ring::{DEFAULT_WINDOW_NS, HandRing};

/// The simulation thread's stack until pe-logic's `Settings::thread_stack()` is on main (app.md 1): wasmtime's
/// default `max_wasm_stack` of 512 KiB plus 2 MiB, rounded up.
// TODO(pe-logic): spawn with `pe_logic::Settings::thread_stack()` (`max_wasm_stack` + 2 MiB, on ws/logic) once pe-logic
// lands, from the same `Settings` the replica runs guests with (design-reviewer's Low 1 on slice 1).
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
            "--seconds" => {
                let s = args.next().and_then(|n| n.parse::<f64>().ok()).filter(|s| *s > 0.0);
                o.run_for = Some(s.and_then(|s| Duration::try_from_secs_f64(s).ok()).unwrap_or_else(|| usage()));
            }
            _ => usage(),
        }
    }
    o
}

fn presenter(rate: SimRate) -> Presenter {
    Presenter::new(DisplaySettings::default(), EffectSettings::default(), Environment::default(), rate.hz())
}

/// The M1 interim's session (app.md 4 step 4): open or join, then the simulation over pe-agree and pe-net.
fn shared_sim(
    o: &Options,
    shared: &Shared,
    host: &clock::HostClock,
    wake: &WakeLater,
    slot: Arc<FrameSlot>,
    hands: Arc<SwitchedHands>,
) -> Sim {
    let rate = SimRate::DEFAULT;
    let path = o.identity.clone().map_or_else(net::default_identity_path, Ok).unwrap_or_else(|e| fail(&e.to_string()));
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
            ep.open_session(&invite, &reply, &p, OPEN_TIMEOUT)
        }
        Shared::Join(invite) => {
            println!("reply code for the opener: {}", ep.reply(invite).encode());
            ep.dial(invite).unwrap_or_else(|e| fail(&e.to_string()));
            ep.join_session(invite, rate, JOIN_TIMEOUT)
        }
    }
    .unwrap_or_else(|e| fail(&format!("the session did not start: {e}")));
    println!(
        "session: {} members, this machine {}",
        session.members.len(),
        session.me.0[..4].iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    // With two members, both are needed for finality: if one leaves, the other's finality stops until both restart.
    Sim::new(Box::new(session.node), session.replica, session.frames, slot, Budget::new(rate))
        .with_local(session.me, chain_nonce(), hands)
        .with_transport(net::transport(session.handle), Some(Box::new(session.hook)))
}

fn main() {
    let options = options();
    let (vr, run_for) = (options.vr, options.run_for);
    let started = Instant::now();

    let rate = SimRate::DEFAULT;
    let host = clock::shared();
    let slot = Arc::new(FrameSlot::new());
    // The local player's hands (app.md 3, step 2): VR's ring while an XR session runs, the desktop hand otherwise.
    let ring = Arc::new(HandRing::new(DEFAULT_WINDOW_NS, VR_PERIOD_NS));
    let desktop_hand = Arc::new(DesktopHandFeed::default());
    let hands = Arc::new(SwitchedHands::new(ring.clone(), desktop_hand.clone()));
    let wake = WakeLater::default();
    let sim = match &options.shared {
        Some(shared) => shared_sim(&options, shared, &host, &wake, slot.clone(), hands.clone()),
        None => {
            let frames: Arc<dyn FrameClock> = Arc::new(LocalClock::new(host(), rate));
            let replica = match world::open_ball_pit(WORLD_SEED, world::DEV_MEMBER, rate, frames.clone()) {
                Ok(r) => r,
                Err(e) => fail(&format!("the replica did not start: {e}")),
            };
            Sim::new(Box::new(SoloAgreement::new(LineId::origin())), replica, frames, slot.clone(), Budget::new(rate))
                .with_local(world::DEV_MEMBER, chain_nonce(), hands.clone())
        }
    };
    // pe-api's ports and the MCP thread (app.md 3.1), when a transport is asked for.
    let mcp_wanted = options.mcp_pipe || options.mcp_http;
    if !mcp_wanted && !options.agent.is_empty() {
        fail("--agent needs --mcp-pipe or --mcp-http");
    }
    let me = sim.local_member();
    let ports = mcp_wanted.then(|| {
        let arrival = wake.arrival();
        sim.ports(WORLD_NAME, move || arrival()).unwrap_or_else(|| fail("pe-api needs a pe-world replica"))
    });
    let sim = match &ports {
        Some(p) => sim.with_api(p.clone()),
        None => sim,
    };
    let sim = match sim::spawn(sim, host.clone(), SIM_STACK) {
        Ok(t) => t,
        Err(e) => fail(&format!("the simulation thread did not start: {e}")),
    };
    wake.set(sim.waker());
    let mcp = ports.map(|p| {
        let member = me.unwrap_or_else(|| fail("an agent acts for this machine's member, and there is none"));
        match api::spawn_mcp(p.api(), mcp_opts(&options, member)) {
            Ok(t) => t,
            Err(e) => fail(&format!("the MCP thread did not start: {e}")),
        }
    });

    if vr {
        hands.use_vr(true);
        let guard = Guard::from_env(true);
        let effects = Box::new(LogEffects);
        match vr::start(
            &SystemDiscovery,
            &guard,
            ring,
            slot.clone(),
            presenter(rate),
            RenderSettings::default(),
            effects,
        ) {
            Ok(xr) => {
                // TODO(pe-render): in VR mode the main thread pumps a window's events and input, the desktop window
                // draws on the XR device while the session is idle or stopping (xr.md 3.1), and the XR thread draws
                // the desktop mirror (app.md 1, render.md 10). Until pe-render has that window the main thread waits
                // here; xr.md 3.3's retry after a lost session comes with it.
                let left = run_for.map_or(Duration::MAX, |d| d.saturating_sub(started.elapsed()));
                let waited = xr.wait(left);
                println!("vr: {waited:?}; at its end {:?}", xr.stop());
                if waited == XrWait::Running {
                    stop_mcp(mcp);
                    finish(sim.stop(), None);
                    return;
                }
                eprintln!("pe-app: the VR session ended; the desktop instead");
            }
            Err(why) => eprintln!("pe-app: {why}; the desktop instead"),
        }
        hands.use_vr(false);
    }

    let cfg = DesktopConfig {
        title: "Primordial Engine".into(),
        size: [1280, 720],
        present_mode: pe_render::wgpu::PresentMode::Fifo,
        vertical_fov: 1.0,
        clock: host,
        presenter: presenter(rate),
        render: RenderSettings::default(),
        hand: DesktopHandSettings::default(),
        eye: RigPose { position: [0.0, 1.4, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
        // What is left of the run after VR, never zero, which the window would take as no time at all.
        run_for: run_for.map(|d| d.saturating_sub(started.elapsed()).max(Duration::from_millis(1))),
        snapshot: None,
    };
    // The window, its surface and its GPU context are made on this thread, one context at a time (app.md 4): the XR
    // thread has dropped its own context before `wait` answers.
    let window = run_desktop(cfg, slot, desktop_hand, Box::new(LogEffects));
    stop_mcp(mcp);
    finish(sim.stop(), Some(window));
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

fn stop_mcp(mcp: Option<api::McpThread>) {
    if let Some(m) = mcp {
        match m.stop() {
            Ok(exited) => println!("mcp: stopped; the agent {exited:?}"),
            Err(e) => eprintln!("mcp: {e}"),
        }
    }
}

fn finish<S: Debug, E>(stats: Result<S, E>, window: Option<Result<DesktopReport, ContextError>>) {
    match window {
        Some(Ok(r)) => println!("window: {} frames on {}, {} skipped", r.frames, r.adapter, r.skipped),
        Some(Err(e)) => eprintln!("window: {e:?}"),
        None => {}
    }
    match stats {
        Ok(s) => println!("simulation: {s:?}"),
        Err(_) => fail("the simulation thread panicked"),
    }
}

fn usage() -> ! {
    fail(
        "usage: pe-app [--vr] [--seconds <n>] [--open | --join <invite>] [--listen <addr>] [--advertise <addr>]... \
         [--identity <path>] [--mcp-pipe] [--mcp-http] [--agent <command> <args>...]",
    )
}

fn fail(msg: &str) -> ! {
    eprintln!("pe-app: {msg}");
    std::process::exit(2)
}
