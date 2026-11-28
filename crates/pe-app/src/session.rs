//! In-game sessions (app.md 4.1; design d3 on @item:primordial-engine/pe-app-join-and-start-a-new-session-inside-the-r,
//! approved by @design-reviewer): Join and New session happen inside the running process, so VR never closes.
//!
//! - **What a session is:** everything one world needs and nothing the process keeps. That is the endpoint (pe-net's
//!   thread and socket), agreement, the replica with its script runner, the simulation thread with pe-diverge, pe-api's
//!   ports, the build service, the spawn worker and MCP. [`start`] builds one as main.rs used to at start-up, in the same
//!   order; [`Running::stop`] stops it in the shutdown's order (app.md 4), each step with a bound.
//! - **What the process keeps** ([`Kept`]): the frame slot, the input rings, the display lead, the status line, the
//!   capture queue and the console. A new world goes into the same frame slot after `FrameSlot::begin_world`, and the
//!   presenter starts over at its first view (render.md 3).
//! - **The network and agreement** see what they saw at a relaunch: the same identity, a new endpoint, new connections,
//!   a new chain nonce and a new replica.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pe_net_crypto::identity::{Invite, Reply};
use pe_render::notices::StatusLine;
use pe_render::present::{FrameSlot, WorldStart};
use pe_replica::ScriptRunner;
use pe_replica::logic::LogicRunner;
use pe_seams::{FrameClock, LineId, MemberKey, MoveFeed, SimRate};

use crate::api::{self, McpThread, Ports};
use crate::builds::{self, AttesterThread, Builder};
use crate::clock::{HostClock, LocalClock};
use crate::console::Quit;
use crate::join::T_JOIN;
use crate::local::{SwitchedHands, chain_nonce};
use crate::net::{self, Endpoint, OpenParams, WakeLater};
use crate::sim::{self, Budget, Sim, SimThread};
use crate::standin::SoloAgreement;

/// The simulation thread's least stack (app.md 1), raised to what the session's script runner needs.
pub const SIM_STACK: usize = 4 << 20;
/// A new world's seed, until configuration names the world (app.md 4, step 1).
pub const WORLD_SEED: u64 = 0;
/// The world's name as pe-api reports it, until the invite or the configuration names it (api.md 5.1).
pub const WORLD_NAME: &str = "M1 ball pit";
/// The joiner's wait for the opener's welcome.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long an invite stays valid.
pub const INVITE_VALID: Duration = Duration::from_secs(3600);

/// The stop's bounds (design d3 section 2, M1): past one, the thread is left behind, named, and the stop goes on.
pub const SPAWN_STOP: Duration = Duration::from_secs(6);
pub const MCP_STOP: Duration = Duration::from_secs(3);
pub const ATTESTER_STOP: Duration = Duration::from_secs(3);
/// After the leave's own bound (5-15 s, `crate::leave`), for the simulation thread to end.
pub const SIM_STOP: Duration = Duration::from_secs(3);
/// How long a new session tries to bind the port the last one let go of, before it takes another (D8).
pub const REBIND_WAIT: Duration = Duration::from_secs(2);

/// What a session starts as.
#[derive(Clone, Debug)]
pub enum Start {
    /// A session of this machine's own (`--open`, New session, Leave).
    Open,
    /// Join the session this invite names.
    Join(Invite),
    /// The world alone, with no session (`--alone`, tests).
    Alone,
}

/// The process's options every session takes (the command line, until configuration lands).
#[derive(Clone)]
pub struct Spec {
    pub listen: SocketAddr,
    pub advertise: Vec<SocketAddr>,
    /// This machine's identity file, and its seed.
    pub identity: PathBuf,
    pub seed: [u8; 32],
    /// The world an opened session starts (`--preset`); a joiner builds the record's.
    pub preset: pe_world::preset::Preset,
    /// The session delay an opened session records (`--display-delay`); a joiner takes the record's.
    pub display_delay: Duration,
    /// `--check-replay`: the first session only (D5).
    pub check_replay: bool,
    /// MCP's options for this machine's member, if a transport is asked for (D4: restarted with each session).
    pub mcp: Option<McpOpts>,
}

/// MCP's options for a member: the transports, the agent's command line and its grant.
pub type McpOpts = Arc<dyn Fn(MemberKey) -> pe_mcp::host::HostOpts + Send + Sync>;

/// What the process keeps across sessions (design d3 section 1).
#[derive(Clone)]
pub struct Kept {
    pub host: HostClock,
    pub slot: Arc<FrameSlot>,
    pub hands: Arc<SwitchedHands>,
    pub moves: Arc<dyn MoveFeed>,
    pub display_lead: Arc<crate::shown::DisplayLead>,
    pub status: Arc<StatusLine>,
    /// view_capture's queue, served by the headset or the window; `None` headless.
    pub captures: Option<Arc<pe_render::capture_service::CaptureQueue>>,
    pub quit: Arc<Quit>,
    /// Where the console's reply codes go: the current opener session's admission (the hidden fallback).
    pub replies: Replies,
    /// The menu's note about a member's link (app.md 4.1), which each session's simulation writes; a new session
    /// starts with none.
    pub link_notes: crate::leave::LinkNotes,
}

/// The current opener session's admission, which each session replaces: the console's and the menu's reply codes.
pub type Replies = Arc<Mutex<Option<Sender<Reply>>>>;

/// What the menu shows of the session (`crate::menu`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// Its own session: the invite, its expiry (Unix seconds), and a warning to show beside it (D8's port).
    Opener { invite: String, expires_unix_s: u64, warning: Option<String> },
    /// In someone else's session: whose invite it joined.
    Joiner { opener: MemberKey },
    /// The world alone.
    Alone,
}

/// A session, running.
pub struct Running {
    /// 1 for the process's first session.
    pub ordinal: u32,
    pub role: Role,
    pub me: Option<MemberKey>,
    /// The session's display delay (its record's, or the option's): the first presenter's drawn time takes it.
    pub display_delay: Duration,
    sim: Option<SimThread>,
    networked: bool,
    ports: Option<Ports>,
    mcp: Option<McpThread>,
    attester: Option<AttesterThread>,
    builder: Option<Arc<Builder>>,
    spawner: Option<Arc<crate::spawn::Spawner>>,
}

/// How a session's stop went: each thread left behind past its bound, and the leave's outcome.
#[derive(Debug, Default)]
pub struct StopReport {
    pub left_behind: Vec<&'static str>,
    pub leave: Option<sim::LeaveOutcome>,
    pub stats: Option<sim::SimStats>,
}

/// Why a session didn't start.
#[derive(Debug)]
pub struct Failed(pub String);

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `p` with `-s<n>` before its file name's extension: session `n`'s file for a per-session log (D5). Edited as text, so
/// the path keeps its own separators.
pub fn session_path(p: &str, n: u32) -> String {
    if n <= 1 {
        return p.to_string();
    }
    let name_at = p.rfind(['/', '\\']).map_or(0, |i| i + 1);
    match p[name_at..].rfind('.').filter(|&i| i > 0) {
        Some(i) => format!("{}-s{n}{}", &p[..name_at + i], &p[name_at + i..]),
        None => format!("{p}-s{n}"),
    }
}

fn short(k: MemberKey) -> String {
    k.0[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Binds `listen` for a session, waiting up to [`REBIND_WAIT`] while the port is in use (the last session's socket
/// closes as its network thread ends). Past that, the same interface on a port of the OS's choice, and why (D8).
fn bind(spec: &Spec, opener: bool, host: &HostClock, wake: &WakeLater) -> Result<(Endpoint, Option<String>), Failed> {
    let until = Instant::now() + REBIND_WAIT;
    loop {
        match Endpoint::bind(spec.seed, spec.listen, spec.advertise.clone(), opener, host.clone(), Some(wake.arrival()))
        {
            Ok(ep) => return Ok((ep, None)),
            Err(net::NetError::Io(e)) if e.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(net::NetError::Io(e)) if e.kind() == std::io::ErrorKind::AddrInUse => {
                let any = SocketAddr::new(spec.listen.ip(), 0);
                // A named advertised address names the busy port: the OS's port can't be advertised by it.
                let advertise: Vec<SocketAddr> = spec.advertise.iter().map(|a| SocketAddr::new(a.ip(), 0)).collect();
                if !advertise.is_empty() {
                    return Err(Failed(format!(
                        "port {} is busy (perhaps an earlier copy of the game), and --advertise names it",
                        spec.listen.port()
                    )));
                }
                let ep = Endpoint::bind(spec.seed, any, Vec::new(), opener, host.clone(), Some(wake.arrival()))
                    .map_err(|e| Failed(e.to_string()))?;
                let warning = format!(
                    "port {} was busy, perhaps an earlier copy of the game: this invite names port {} and may not reach \
                     you",
                    spec.listen.port(),
                    ep.local_addr().port()
                );
                return Ok((ep, Some(warning)));
            }
            Err(e) => return Err(Failed(e.to_string())),
        }
    }
}

/// Starts session `ordinal` as `start` asks (design d3 section 2): the endpoint, open or join, the simulation and its
/// options, its thread, pe-api, the build service, the spawn worker and MCP, in main.rs's old order. `cancel` ends a
/// join's waits. A failure leaves nothing running.
pub fn start(spec: &Spec, kept: &Kept, start: &Start, cancel: &AtomicBool, ordinal: u32) -> Result<Running, Failed> {
    start_with(spec, kept, start, cancel, ordinal, None, &mut || true)
}

/// [`start`], with what a join that fails after it starts (at its import) calls with why: the manager's fallback, or
/// the binary's quit; and `ready`, which a join asks just before it tells the inviter it is ready
/// ([`net::Endpoint::join_at_cut_with`]): `false` ends the join as cancelled.
pub fn start_with(
    spec: &Spec,
    kept: &Kept,
    start: &Start,
    cancel: &AtomicBool,
    ordinal: u32,
    on_join_failed: Option<Box<dyn FnOnce(String) + Send>>,
    ready: &mut dyn FnMut() -> bool,
) -> Result<Running, Failed> {
    let rate = SimRate::DEFAULT;
    let (settings, run) = net::runner_settings();
    let runner =
        LogicRunner::new(settings, run).map_err(|e| Failed(format!("the script runner did not start: {e}")))?;
    let sim_stack = SIM_STACK.max(runner.thread_stack());
    let inbox = runner.inbox();
    let runner: Box<dyn ScriptRunner> = Box::new(runner);
    let wake = WakeLater::default();
    let first = ordinal <= 1;
    let (sim, role, networked) = match start {
        Start::Alone => {
            let frames: Arc<dyn FrameClock> = Arc::new(LocalClock::new((kept.host)(), rate));
            let replica =
                crate::world::open_ball_pit(WORLD_SEED, crate::world::DEV_MEMBER, rate, frames.clone(), runner)
                    .map_err(|e| Failed(format!("the replica did not start: {e}")))?;
            let sim = Sim::new(
                Box::new(SoloAgreement::new(LineId::origin())),
                replica,
                frames,
                kept.slot.clone(),
                Budget::new(rate),
            )
            .with_local(crate::world::DEV_MEMBER, chain_nonce(), kept.hands.clone())
            .with_display_delay(spec.display_delay);
            (sim, Role::Alone, false)
        }
        Start::Open => {
            let (ep, warning) = bind(spec, true, &kept.host, &wake)?;
            let display_delay_ms = u32::try_from(spec.display_delay.as_millis()).unwrap_or(u32::MAX);
            let p = OpenParams {
                seed: WORLD_SEED,
                preset: spec.preset,
                rate,
                epoch_lead: Duration::ZERO,
                display_delay_ms,
            };
            let opened = ep
                .open_alone(&p, INVITE_VALID, T_JOIN, runner)
                .map_err(|e| Failed(format!("the session did not start: {e}")))?;
            let s = opened.session;
            crate::out!("session {ordinal}: open, this machine {}", short(s.me));
            crate::out!("invite: {}", opened.invite.encode());
            if let Some(w) = &warning {
                crate::err!("pe-app: {w}");
            }
            *kept.replies.lock().unwrap_or_else(|e| e.into_inner()) = Some(opened.replies.clone());
            let sim = Sim::new(Box::new(s.node), s.replica, s.frames, kept.slot.clone(), Budget::new(rate))
                .with_local(s.me, chain_nonce(), kept.hands.clone())
                .with_transport(net::transport(s.handle), Some(Box::new(s.hook)))
                .with_linked(s.linked)
                .with_display_delay(Duration::from_millis(u64::from(display_delay_ms)));
            let sim = diverge(sim, &spec.identity)?;
            let sim = if first && spec.check_replay {
                crate::out!("replay: keeping this session's final log; type `replay` to replay it from frame 0");
                sim.with_replay_check(crate::replay::ReplayLog::new(opened.genesis))
            } else {
                sim
            };
            let role =
                Role::Opener { invite: opened.invite.encode(), expires_unix_s: opened.invite.expiry_unix_s, warning };
            (sim, role, true)
        }
        Start::Join(invite) => {
            let (mut ep, warning) = bind(spec, false, &kept.host, &wake)?;
            if let Some(w) = warning {
                crate::err!("pe-app: {w}");
            }
            crate::out!("session {ordinal}: joining {}'s session with the invite", short(MemberKey(invite.key)));
            ep.dial(invite).map_err(|e| Failed(e.to_string()))?;
            if cancel.load(Ordering::Acquire) {
                let _ = ep.stop();
                return Err(Failed("the join was cancelled".into()));
            }
            let cj = ep.join_at_cut_with(invite, rate, JOIN_TIMEOUT, T_JOIN, cancel, ready).map_err(|e| match e {
                net::NetError::Cancelled => Failed("the join was cancelled".into()),
                e if still_counted(&e.to_string()) => Failed(counted_still(MemberKey(invite.key))),
                e => Failed(format!("couldn't join the session: {e}")),
            })?;
            crate::out!(
                "session {ordinal}: joined at frame {}, {} members, this machine {}",
                cj.e.at.frame.0,
                cj.members.len(),
                short(cj.me)
            );
            let replica = cj
                .record
                .empty_replica(cj.me, cj.frames.clone(), runner)
                .map_err(|e| Failed(format!("the replica did not start: {e}")))?;
            let linked = cj.members.iter().copied().filter(|m| *m != cj.me).collect();
            let sim = Sim::new(Box::new(cj.node), replica, cj.frames, kept.slot.clone(), Budget::new(rate))
                .with_local(cj.me, chain_nonce(), kept.hands.clone())
                .with_transport(net::transport_with(cj.handle, cj.pending), Some(Box::new(cj.hook)))
                .with_linked(linked)
                .with_display_delay(Duration::from_millis(u64::from(cj.record.display_delay_ms)));
            let sim = diverge(sim, &spec.identity)?;
            let mut sim = match on_join_failed {
                Some(f) => sim.with_on_join_failed(f),
                None => sim,
            };
            sim.join_at_final(MemberKey(invite.key), (kept.host)())
                .map_err(|e| Failed(format!("couldn't join the session: {e}")))?;
            (sim, Role::Joiner { opener: MemberKey(invite.key) }, true)
        }
    };
    let sim = decorate(sim, kept, ordinal, rate, matches!(role, Role::Opener { .. }))?;
    let me = sim.local_member();
    let ports = {
        let arrival = wake.arrival();
        sim.ports(WORLD_NAME, move || arrival())
    };
    let sim = match &ports {
        Some(p) => sim.with_api(p.clone()),
        None => sim,
    };
    // The next world goes into the same frame slot: its first view starts the presenter over (render.md 3). The last
    // session's simulation thread has ended, so no view of the old world comes after this.
    let display_delay = sim.display_delay();
    kept.slot.begin_world(WorldStart { display_delay });
    let sim = sim::spawn(sim, kept.host.clone(), sim_stack)
        .map_err(|e| Failed(format!("the simulation thread did not start: {e}")))?;
    wake.set(sim.waker());
    let mut running = Running {
        ordinal,
        role,
        me,
        display_delay,
        sim: Some(sim),
        networked,
        ports: None,
        mcp: None,
        attester: None,
        builder: None,
        spawner: None,
    };
    let Some(ports) = ports else { return Ok(running) };
    let builds = me.and_then(|me| start_builds(inbox, &ports, me, &spec.identity));
    let ports = match builds {
        Some((attester, builder)) => {
            running.attester = Some(attester);
            running.builder = Some(builder.clone());
            ports.with_builds(builder)
        }
        None => ports,
    };
    let ports = match &kept.captures {
        Some(q) => ports.with_captures(Arc::new(crate::capture::RenderCaptures(q.clone()))),
        None => ports,
    };
    if let Some(me) = me {
        running.spawner = crate::spawn::install(ports.api(), me);
    }
    if let (Some(opts), Some(me)) = (&spec.mcp, me) {
        match api::spawn_mcp(ports.api(), opts(me)) {
            Ok(t) => running.mcp = Some(t),
            Err(e) => crate::err!("pe-app: the MCP thread did not start: {e}"),
        }
    }
    running.ports = Some(ports);
    Ok(running)
}

/// A join refused because the opener still counts this machine as a player (design d3, M2), since it left moments
/// ago or the invite is from before it left: a welcome that names this machine, a welcome whose members aren't the
/// invite's pin, or the opener's own refusal of an invite whose players changed (join.rs, `NotAdmitted`).
fn still_counted(refusal: &str) -> bool {
    refusal.contains("a member already")
        || refusal.contains("the invite pins")
        || refusal.contains("the players changed since the invite was made")
}

/// What the menu says for [`still_counted`] (design d3, M2).
fn counted_still(opener: MemberKey) -> String {
    let who = crate::leave::player(opener);
    format!(
        "{who}'s session still counts you as a player (you left moments ago, or this invite is from before you left). \
         Wait a few seconds and join again; if it happens again, ask {who} for a new invite."
    )
}

/// pe-diverge (app.md 3, step 5), with fault bundles beside the identity file.
fn diverge(sim: Sim, identity: &std::path::Path) -> Result<Sim, Failed> {
    sim.with_diverge(pe_diverge::Config::default(), Some(crate::diverge::bundle_dir(identity)))
        .map_err(|e| Failed(format!("pe-diverge did not start: {e}")))
}

/// The options every session's simulation takes from the process: the shown present, the status line, the hand feel,
/// the moves, the link note, the slowdown, the coalescing window, and this session's tick trace and cut log (D5: one
/// file per session).
fn decorate(sim: Sim, kept: &Kept, ordinal: u32, rate: SimRate, opener: bool) -> Result<Sim, Failed> {
    // The last session's note about a link isn't this one's.
    *kept.link_notes.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let sim = sim
        .with_shown(kept.display_lead.clone())
        .with_status(kept.status.clone())
        .with_hand_feel(crate::handfeel::console())
        .with_moves(kept.moves.clone())
        .with_link_notes(kept.link_notes.clone(), opener)
        .with_slow_frame(crate::slowframe::armed());
    // `PE_COALESCE_TICKS=<n>`: the coalesced pose rollbacks' window (crate::coalesce), 1 for none; default 3.
    let sim = match std::env::var("PE_COALESCE_TICKS").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(n) => {
            crate::out!("coalesced pose rollbacks: every {n} ticks (PE_COALESCE_TICKS)");
            sim.with_coalesce_window(n)
        }
        None => sim,
    };
    let env = |var: &str| std::env::var(var).ok().filter(|p| !p.is_empty()).map(|p| session_path(&p, ordinal));
    let sim = match env("PE_TICK_TRACE") {
        Some(path) => {
            let ms =
                std::env::var("PE_TICK_TRACE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(crate::trace::LONG_MS);
            match crate::trace::TickTrace::create(std::path::Path::new(&path), ms, rate.hz()) {
                Ok(t) => {
                    crate::out!("tick trace: writing {path} (PE_TICK_TRACE)");
                    sim.with_tick_trace(t)
                }
                Err(e) => {
                    crate::err!("tick trace: couldn't open {path} ({e}); running without it");
                    sim
                }
            }
        }
        None => sim,
    };
    let sim = match env("PE_CUT_LOG") {
        Some(path) => match crate::cutlog::CutLog::create(std::path::Path::new(&path), (kept.host)()) {
            Ok(l) => {
                crate::out!("cut log: writing {path} (PE_CUT_LOG)");
                sim.with_cut_log(l)
            }
            Err(e) => {
                crate::err!("cut log: couldn't open {path} ({e}); running without it");
                sim
            }
        },
        None => sim,
    };
    Ok(sim)
}

/// The build service and its attester (builds.rs) over this session's runner and ports, with the build folder beside
/// the identity file. Without a toolchain this machine builds no script, and says so.
pub fn start_builds(
    inbox: pe_replica::logic::ModuleInbox,
    ports: &Ports,
    me: MemberKey,
    identity: &std::path::Path,
) -> Option<(AttesterThread, Arc<Builder>)> {
    let started = (|| -> Result<_, String> {
        let toolchain = pe_logic::build::Toolchain::current().map_err(|e| e.to_string())?;
        let sdk = builds::sdk().clone()?.0;
        let config = builds::BuildConfig::new(toolchain, sdk, builds::build_dir(identity));
        let builder = Arc::new(Builder::new(config, inbox.clone()).map_err(|e| e.to_string())?);
        let thread = builds::spawn_attester(builder.clone(), inbox, ports.shared.clone(), ports.queue.clone(), me)
            .map_err(|e| e.to_string())?;
        Ok((thread, builder))
    })();
    started
        .map_err(|e| crate::err!("pe-app: the build service did not start, so this machine builds no script: {e}"))
        .ok()
}

impl Running {
    /// Stops the session (design d3 section 2, M1), in the shutdown's order, each step bounded: pe-api's ports close
    /// first, then the spawn worker, MCP and its agent, the attester with any build in flight killed, then the graceful
    /// leave when others share the world. `cut_short` (Cancel or Quit during the stop) ends the leave's wait at once.
    /// The simulation thread's end stops pe-net (closing every connection and the socket) and pe-diverge's writer.
    pub fn stop(mut self, cut_short: &AtomicBool) -> StopReport {
        let mut report = StopReport::default();
        if let Some(p) = &self.ports {
            p.close();
        }
        if let Some(s) = self.spawner.take() {
            // Only this session's own: in tests an opener and a joiner share one process, and each stops its own.
            let _ = crate::spawn::uninstall_if(&s);
            if !s.stop(SPAWN_STOP) {
                report.left_behind.push("pe-spawn");
            }
        }
        if let Some(m) = self.mcp.take() {
            match m.stop_within(MCP_STOP) {
                Some(Ok(exited)) => crate::out!("mcp: stopped; the agent {exited:?}"),
                Some(Err(e)) => crate::err!("mcp: {e}"),
                None => report.left_behind.push("mcp"),
            }
        }
        if let Some(b) = self.builder.take() {
            b.cancel();
        }
        if let Some(a) = self.attester.take()
            && !a.stop_within(ATTESTER_STOP)
        {
            report.left_behind.push("pe-attester");
        }
        if let Some(sim) = self.sim.take() {
            let (leave, stats) = if self.networked {
                let (outcome, stats) =
                    sim.leave_and_stop_within(&mut |line| crate::out!("{line}"), cut_short, SIM_STOP);
                (Some(outcome), stats)
            } else {
                (None, sim.stop_within(SIM_STOP))
            };
            report.leave = leave;
            match stats {
                Some(s) => {
                    // Session 1's line reads as it always has, for the scripts that read it.
                    if self.ordinal <= 1 {
                        crate::out!("simulation: {s:?}");
                    } else {
                        crate::out!("simulation (session {}): {s:?}", self.ordinal);
                    }
                    report.stats = Some(s);
                }
                None => report.left_behind.push("pe-sim"),
            }
        }
        crate::replay::clear_request();
        if self.ordinal <= 1 && crate::fault::disarm() {
            crate::out!("FAULT INJECTION: the test fault ended with session 1; later sessions run without it");
        }
        if !report.left_behind.is_empty() {
            crate::err!("pe-app: session {}'s stop left behind: {}", self.ordinal, report.left_behind.join(", "));
        }
        report
    }

    /// The opener's invite, if this is an opener's session.
    pub fn invite(&self) -> Option<&str> {
        match &self.role {
            Role::Opener { invite, .. } => Some(invite),
            _ => None,
        }
    }
}

// ---- the manager (design d3 section 2) -------------------------------------------------------------------------

/// What the menu and the console ask of the sessions.
#[derive(Clone, Debug)]
pub enum Want {
    /// Leave this session for a new one of this machine's own (New session, Leave).
    New,
    /// Leave this session and join the one this invite names.
    Join(Invite),
    /// Stop a swap under way: a join's waits until it says it is ready, or a leave's wait.
    Cancel,
    /// End the run: leave the session, or stop a join, and end the manager.
    Quit,
    /// The running join failed while it imported (the Sim's callback): back to a session of this machine's own.
    JoinFailed(String),
}

/// A swap's step, for the menu (D2: the step, a count of seconds, the bound when it is long).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Leaving the session, for at most `bound` (the leave's, with others present).
    Leaving { since: Instant, bound: Option<Duration> },
    /// Joining `who`'s session; `ready` once this machine told them it is ready, when Cancel stops being offered.
    Joining { who: MemberKey, since: Instant, ready: bool },
    /// Starting a session of this machine's own.
    Opening { since: Instant },
}

impl Step {
    /// The menu's line for this step, `now` being the time it is drawn.
    pub fn line(&self, now: Instant) -> String {
        let secs = |since: &Instant| now.saturating_duration_since(*since).as_secs();
        match self {
            Step::Leaving { since, bound: Some(b) } => {
                format!("Leaving the session… {} s (up to {} s)", secs(since), b.as_secs())
            }
            Step::Leaving { since, bound: None } => format!("Leaving the session… {} s", secs(since)),
            Step::Joining { who, since, ready: false } => {
                format!("Joining {}'s session… {} s", crate::leave::player(*who), secs(since))
            }
            Step::Joining { who, since, ready: true } => format!(
                "Joining {}'s session… {} s (they've let you in: this can't be cancelled)",
                crate::leave::player(*who),
                secs(since)
            ),
            Step::Opening { since } => format!("Starting a session of your own… {} s", secs(since)),
        }
    }
}

/// What the menu reads each frame: the running session's role, a swap's step, and the last failure, which stays until
/// the player dismisses it (D3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub role: Option<Role>,
    pub step: Option<Step>,
    pub failure: Option<String>,
}

/// The sessions' manager: a thread that owns the running session and carries out each [`Want`] in turn.
pub struct Manager {
    tx: Sender<Want>,
    view: Arc<Mutex<View>>,
    cancel: Arc<AtomicBool>,
    quitting: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// What the menu and the console send through: cheap to clone.
#[derive(Clone)]
pub struct Asker {
    tx: Sender<Want>,
    view: Arc<Mutex<View>>,
    cancel: Arc<AtomicBool>,
    /// Set by a Quit and never cleared: the manager starts nothing more, whatever it was doing (@design-reviewer's M1).
    quitting: Arc<AtomicBool>,
}

impl Asker {
    /// Asks for `w`. Cancel and Quit also set the swap's cancel at once, which a join's waits and a leave's wait see
    /// within a few milliseconds. Once a join has said it is ready, Cancel is refused (design d3, L2): `false`. The
    /// check and the store happen under the view's lock, which the join's `ready` also takes, so a Cancel either
    /// comes before `JoinReady` (and it isn't sent) or is refused.
    pub fn ask(&self, w: Want) -> bool {
        match &w {
            Want::Cancel => {
                let v = self.view.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(v.step, Some(Step::Joining { ready: true, .. })) {
                    return false;
                }
                self.cancel.store(true, Ordering::Release);
            }
            Want::Quit => {
                self.quitting.store(true, Ordering::Release);
                self.cancel.store(true, Ordering::Release);
            }
            _ => {}
        }
        self.tx.send(w).is_ok()
    }

    pub fn view(&self) -> View {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The player dismissed the failure line.
    pub fn dismiss(&self) {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).failure = None;
    }
}

impl Manager {
    /// Takes over `first`, the session main.rs started, and carries out what is asked from now on.
    pub fn spawn(spec: Spec, kept: Kept, first: Running) -> std::io::Result<Manager> {
        let (tx, rx) = std::sync::mpsc::channel();
        let view = Arc::new(Mutex::new(View { role: Some(first.role.clone()), ..View::default() }));
        let cancel = Arc::new(AtomicBool::new(false));
        let quitting = Arc::new(AtomicBool::new(false));
        let (v, c, q, t) = (view.clone(), cancel.clone(), quitting.clone(), tx.clone());
        let thread = std::thread::Builder::new()
            .name("pe-session".into())
            .spawn(move || manage(&spec, &kept, first, &rx, &t, &v, &c, &q))?;
        Ok(Manager { tx, view, cancel, quitting, thread: Some(thread) })
    }

    pub fn asker(&self) -> Asker {
        Asker {
            tx: self.tx.clone(),
            view: self.view.clone(),
            cancel: self.cancel.clone(),
            quitting: self.quitting.clone(),
        }
    }

    /// Ends the run: the running session leaves (or a join in flight stops), and the manager's thread is joined.
    pub fn quit(mut self) {
        let _ = self.asker().ask(Want::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            self.quitting.store(true, Ordering::Release);
            self.cancel.store(true, Ordering::Release);
            let _ = self.tx.send(Want::Quit);
            let _ = t.join();
        }
    }
}

fn set_view(view: &Mutex<View>, f: impl FnOnce(&mut View)) {
    f(&mut view.lock().unwrap_or_else(|e| e.into_inner()));
}

/// The manager's loop: one want at a time, until Quit. A Quit is sticky (`quitting`): whatever is under way when it
/// comes, the manager starts nothing more (@design-reviewer's M1).
#[allow(clippy::too_many_arguments)]
fn manage(
    spec: &Spec,
    kept: &Kept,
    first: Running,
    rx: &std::sync::mpsc::Receiver<Want>,
    tx: &Sender<Want>,
    view: &Mutex<View>,
    cancel: &AtomicBool,
    quitting: &AtomicBool,
) {
    let mut ordinal = first.ordinal;
    let mut current = Some(first);
    while let Ok(w) = rx.recv() {
        if quitting.load(Ordering::Acquire) {
            break;
        }
        let next = match w {
            // Nothing is under way between wants: a stray Cancel only clears the flag.
            Want::Cancel => {
                cancel.store(false, Ordering::Release);
                continue;
            }
            Want::Quit => break,
            Want::New => Start::Open,
            Want::Join(invite) => Start::Join(invite),
            Want::JoinFailed(why) => {
                set_view(view, |v| v.failure = Some(format!("Couldn't join: {why}")));
                Start::Open
            }
        };
        // The flag isn't cleared here: a Cancel pressed since the Join (it is queued behind it) cuts the leave short
        // and cancels the join below. A stray Cancel clears it in its own turn above.
        if let Some(r) = current.take() {
            stop(r, kept, view, cancel);
        }
        if quitting.load(Ordering::Acquire) {
            break;
        }
        // A Cancel while a join was leaving the old session cancels the join: the leave is over (or was cut short), so
        // this machine opens a session of its own (D2, D3).
        let next = match next {
            Start::Join(_) if cancel.load(Ordering::Acquire) => {
                set_view(view, |v| v.failure = Some("Couldn't join: the join was cancelled".into()));
                Start::Open
            }
            n => n,
        };
        cancel.store(false, Ordering::Release);
        ordinal += 1;
        current = swap_in(spec, kept, &next, view, cancel, quitting, tx, &mut ordinal);
        if quitting.load(Ordering::Acquire) {
            break;
        }
    }
    if let Some(r) = current.take() {
        stop(r, kept, view, cancel);
    }
    kept.status.set_override(None);
}

/// Stops `r`, showing the leave on the menu and the status line.
fn stop(r: Running, kept: &Kept, view: &Mutex<View>, cancel: &AtomicBool) {
    let bound = r.networked.then_some(crate::leave::MAX_WAIT);
    let step = Step::Leaving { since: Instant::now(), bound };
    kept.status.set_override(Some(step.line(Instant::now())));
    set_view(view, |v| v.step = Some(step));
    *kept.replies.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let report = r.stop(cancel);
    if !report.left_behind.is_empty() {
        set_view(view, |v| {
            v.failure = Some(format!("The last session left work running: {}", report.left_behind.join(", ")))
        });
    }
}

/// Starts the session `next` asks for. A join that fails or is cancelled falls back to a session of this machine's
/// own, and the failure stays on the menu (D3). `None` only if even that fails.
#[allow(clippy::too_many_arguments)]
fn swap_in(
    spec: &Spec,
    kept: &Kept,
    next: &Start,
    view: &Mutex<View>,
    cancel: &AtomicBool,
    quitting: &AtomicBool,
    tx: &Sender<Want>,
    ordinal: &mut u32,
) -> Option<Running> {
    let step = match next {
        Start::Join(i) => Step::Joining { who: MemberKey(i.key), since: Instant::now(), ready: false },
        _ => Step::Opening { since: Instant::now() },
    };
    kept.status.set_override(Some(step.line(Instant::now())));
    set_view(view, |v| {
        v.step = Some(step);
        v.role = None;
    });
    let failed = tx.clone();
    let on_failed: Box<dyn FnOnce(String) + Send> = Box::new(move |why| {
        let _ = failed.send(Want::JoinFailed(why));
    });
    // The join's last chance to be cancelled (L2): under the view's lock, as Asker::ask's Cancel.
    let mut ready = || {
        let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        if let Some(Step::Joining { ready, .. }) = &mut v.step {
            *ready = true;
            kept.status.set_override(Some(v.step.as_ref().map(|s| s.line(Instant::now())).unwrap_or_default()));
        }
        true
    };
    let started = start_with(spec, kept, next, cancel, *ordinal, Some(on_failed), &mut ready);
    let running = match (started, next) {
        (Ok(r), _) => Some(r),
        (Err(e), Start::Join(_)) if quitting.load(Ordering::Acquire) => {
            crate::out!("pe-app: {e}; quitting");
            None
        }
        (Err(e), Start::Join(_)) => {
            crate::err!("pe-app: {e}; starting a session of your own");
            set_view(view, |v| v.failure = Some(format!("Couldn't join: {e}")));
            *ordinal += 1;
            cancel.store(false, Ordering::Release);
            match start(spec, kept, &Start::Open, cancel, *ordinal) {
                Ok(r) => Some(r),
                Err(e) => {
                    crate::err!("pe-app: no session could start: {e}");
                    set_view(view, |v| v.failure = Some(format!("No session could start: {e}")));
                    None
                }
            }
        }
        (Err(e), _) => {
            crate::err!("pe-app: no session could start: {e}");
            set_view(view, |v| v.failure = Some(format!("No session could start: {e}")));
            None
        }
    };
    kept.status.set_override(None);
    set_view(view, |v| {
        v.step = None;
        v.role = running.as_ref().map(|r| r.role.clone());
    });
    running
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::tests::StillHands;

    /// The spawn slot is one per process (spawn.rs), and these tests open sessions in parallel, each installing its
    /// spawner there: a test that opens one holds this for reading, and the one that checks the slot is empty at its
    /// end holds it for writing.
    static SPAWN_SLOT: std::sync::RwLock<()> = std::sync::RwLock::new(());

    fn opens_sessions() -> std::sync::RwLockReadGuard<'static, ()> {
        SPAWN_SLOT.read().unwrap_or_else(|e| e.into_inner())
    }

    /// A loopback port nothing holds now.
    fn free_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn spec(seed: u8, port: u16) -> Spec {
        let dir = std::env::temp_dir().join(format!("pe-app-session-{}-{seed}", std::process::id()));
        Spec {
            listen: SocketAddr::from(([127, 0, 0, 1], port)),
            advertise: Vec::new(),
            identity: dir.join("identity.seed"),
            seed: [seed; 32],
            preset: pe_world::preset::Preset::BallPit { balls: 8 },
            display_delay: Duration::ZERO,
            check_replay: false,
            mcp: None,
        }
    }

    fn kept() -> Kept {
        let hands: Arc<dyn pe_seams::HandFeed> = Arc::new(StillHands([0.0, 1.0, -0.4]));
        let moves: Arc<dyn MoveFeed> = Arc::new(pe_render::desktop_move::DesktopMoveFeed::default());
        Kept {
            host: crate::clock::shared(),
            slot: Arc::new(FrameSlot::new()),
            hands: Arc::new(SwitchedHands::new(hands.clone(), hands)),
            moves,
            display_lead: Arc::new(crate::shown::DisplayLead::default()),
            status: Arc::new(StatusLine::default()),
            captures: None,
            quit: Quit::new(),
            replies: Arc::new(Mutex::new(None)),
            link_notes: Default::default(),
        }
    }

    /// Waits up to `within` for `ok`.
    fn until(within: Duration, mut ok: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + within;
        while Instant::now() < end {
            if ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        ok()
    }

    fn invite_of(view: &View) -> Option<String> {
        match &view.role {
            Some(Role::Opener { invite, .. }) => Some(invite.clone()),
            _ => None,
        }
    }

    /// New session twice in one process (design d3, test 3): each new session binds the same port again (no D8
    /// fallback), makes a new invite, publishes its world into the same slot as world n + 1, and the last session's
    /// threads are joined (nothing left behind). The quit ends the manager.
    #[test]
    fn new_session_twice_rebinds_the_same_port_and_leaves_nothing_behind() {
        let _slot = SPAWN_SLOT.write().unwrap_or_else(|e| e.into_inner());
        let port = free_port();
        let (spec, kept) = (spec(31, port), kept());
        let first = start(&spec, &kept, &Start::Open, &AtomicBool::new(false), 1).expect("session 1");
        let Role::Opener { invite: first_invite, warning: None, .. } = first.role.clone() else {
            panic!("an opener on its own port: {:?}", first.role)
        };
        let m = Manager::spawn(spec, kept.clone(), first).expect("the manager");
        let ask = m.asker();
        let mut invites = vec![first_invite];
        for n in 2..=3u64 {
            assert!(
                until(Duration::from_secs(5), || kept.slot.newest_current().is_some()),
                "world {} publishes",
                n - 1
            );
            assert!(ask.ask(Want::New));
            assert!(
                until(Duration::from_secs(30), || {
                    let v = ask.view();
                    v.step.is_none() && invite_of(&v).is_some_and(|i| !invites.contains(&i))
                }),
                "session {n} opened: {:?}",
                ask.view()
            );
            let v = ask.view();
            assert_eq!(v.failure, None, "nothing failed or was left behind");
            let Some(Role::Opener { invite, warning, .. }) = v.role else { unreachable!() };
            assert_eq!(warning, None, "the same port bound again, no fallback");
            let decoded = Invite::decode(&invite).expect("an invite");
            assert!(decoded.addrs.iter().all(|a| a.port() == port), "{:?}", decoded.addrs);
            invites.push(invite);
            assert_eq!(kept.slot.world(), n, "world {n} in the same slot");
            assert!(until(Duration::from_secs(5), || kept.slot.newest_current().is_some()), "world {n} publishes");
        }
        m.quit();
        assert!(crate::spawn::installed().is_none(), "no spawner once the run ended");
        // The port is free again: every session's network thread ended.
        assert!(std::net::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], port))).is_ok());
    }

    /// Joining a running opener through the manager (design d3, test 3): B leaves its own session, joins A's at a cut
    /// on the invite alone, and imports A's world; a join that then fails isn't what happens here.
    #[test]
    fn a_manager_joins_a_running_opener_on_its_invite() {
        let _slot = opens_sessions();
        let (spec_a, kept_a) = (spec(41, free_port()), kept());
        let a = start(&spec_a, &kept_a, &Start::Open, &AtomicBool::new(false), 1).expect("A's session");
        let invite = Invite::decode(a.invite().expect("A's invite")).unwrap();
        let invite_key = invite.key;
        let (spec_b, kept_b) = (spec(42, free_port()), kept());
        let b = start(&spec_b, &kept_b, &Start::Open, &AtomicBool::new(false), 1).expect("B's own session");
        let m = Manager::spawn(spec_b, kept_b.clone(), b).expect("B's manager");
        let ask = m.asker();
        assert!(ask.ask(Want::Join(invite)));
        // Until the join ends either way: B in A's session, or a failure (B's own session is the fallback's role too).
        assert!(
            until(Duration::from_secs(60), || {
                let v = ask.view();
                v.step.is_none() && (matches!(v.role, Some(Role::Joiner { .. })) || v.failure.is_some())
            }),
            "B's join ended: {:?}",
            ask.view()
        );
        let v = ask.view();
        assert_eq!(
            (v.role, v.failure),
            (Some(Role::Joiner { opener: MemberKey(invite_key) }), None),
            "B joined A's session"
        );
        assert_eq!(kept_b.slot.world(), 2, "A's world is B's world 2");
        m.quit();
        let _ = a.stop(&AtomicBool::new(false));
    }

    /// A join to an invite nobody answers, cancelled (design d3, test 3): B is back in a session of its own within a
    /// second, the failure says it was cancelled, and the join's network thread is gone (its port binds again).
    #[test]
    fn a_cancelled_join_to_an_invite_nobody_answers_opens_a_session_of_your_own() {
        let _slot = opens_sessions();
        let (spec_a, kept_a) = (spec(51, free_port()), kept());
        let a = start(&spec_a, &kept_a, &Start::Open, &AtomicBool::new(false), 1).expect("A's session");
        let invite = Invite::decode(a.invite().expect("A's invite")).unwrap();
        let _ = a.stop(&AtomicBool::new(false));
        let port = free_port();
        let (spec_b, kept_b) = (spec(52, port), kept());
        let b = start(&spec_b, &kept_b, &Start::Open, &AtomicBool::new(false), 1).expect("B's own session");
        let m = Manager::spawn(spec_b, kept_b.clone(), b).expect("B's manager");
        let ask = m.asker();
        assert!(ask.ask(Want::Join(invite)));
        assert!(
            until(Duration::from_secs(20), || matches!(ask.view().step, Some(Step::Joining { ready: false, .. }))),
            "B is joining: {:?}",
            ask.view()
        );
        std::thread::sleep(Duration::from_millis(500));
        let asked = Instant::now();
        assert!(ask.ask(Want::Cancel), "Cancel is taken before JoinReady");
        assert!(
            until(Duration::from_secs(20), || {
                let v = ask.view();
                v.step.is_none() && matches!(v.role, Some(Role::Opener { .. }))
            }),
            "B is back in a session of its own: {:?}",
            ask.view()
        );
        let v = ask.view();
        assert!(v.failure.as_deref().is_some_and(|f| f.contains("cancelled")), "{:?}", v.failure);
        let Some(Role::Opener { warning, .. }) = v.role else { unreachable!() };
        assert_eq!(warning, None, "the join's endpoint let the port go before the new session bound it");
        // The cancel itself is quick; the rest is the new session's start.
        crate::out!("cancel to a session of its own: {} ms", asked.elapsed().as_millis());
        m.quit();
        assert!(std::net::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], port))).is_ok());
    }

    /// Leaving A's session and at once joining A again on the same invite (design d3, test 3, M2): B is refused within
    /// seconds, never at the join's timeout, with M2's message (or, if A already made the leave final and took the
    /// invite again, B is in), and B is back in a session of its own.
    #[test]
    fn rejoining_a_session_just_left_is_answered_at_once() {
        let _slot = opens_sessions();
        let (spec_a, kept_a) = (spec(61, free_port()), kept());
        let a = start(&spec_a, &kept_a, &Start::Open, &AtomicBool::new(false), 1).expect("A's session");
        let invite = Invite::decode(a.invite().expect("A's invite")).unwrap();
        let (spec_b, kept_b) = (spec(62, free_port()), kept());
        let b = start(&spec_b, &kept_b, &Start::Open, &AtomicBool::new(false), 1).expect("B's own session");
        let m = Manager::spawn(spec_b, kept_b.clone(), b).expect("B's manager");
        let ask = m.asker();
        assert!(ask.ask(Want::Join(invite.clone())));
        assert!(
            until(Duration::from_secs(60), || {
                let v = ask.view();
                v.step.is_none() && matches!(v.role, Some(Role::Joiner { .. }))
            }),
            "B joined A: {:?}",
            ask.view()
        );
        // At once: B's Leave goes out with the swap, and the same invite names B's old membership.
        let asked = Instant::now();
        assert!(ask.ask(Want::Join(invite)));
        // Until the swap has made a new world (joined, or a session of its own after a refusal) and ended.
        assert!(
            until(Duration::from_secs(40), || {
                let v = ask.view();
                v.step.is_none()
                    && kept_b.slot.world() > 2
                    && (v.failure.is_some() || matches!(v.role, Some(Role::Joiner { .. })))
            }),
            "the rejoin ended: {:?}",
            ask.view()
        );
        let took = asked.elapsed();
        let v = ask.view();
        crate::out!("rejoin answered in {} ms: {:?}", took.as_millis(), v.failure);
        match &v.failure {
            Some(f) => {
                assert!(f.contains("still counts you as a player"), "M2's message: {f}");
                assert!(!f.contains("  "), "one sentence after another: {f}");
                assert!(matches!(v.role, Some(Role::Opener { .. })), "back in a session of its own: {:?}", v.role);
            }
            None => assert!(matches!(v.role, Some(Role::Joiner { .. }))),
        }
        assert!(took < JOIN_TIMEOUT / 2, "not a wait for the join's timeout: {took:?}");
        m.quit();
        let _ = a.stop(&AtomicBool::new(false));
    }

    /// B in A's session, and an invite nobody answers: a join to it would wait for the join's timeout (300 s).
    fn joined_with_a_dead_invite() -> (Manager, Asker, Kept, Invite, Running) {
        let (spec_x, kept_x) = (spec(71, free_port()), kept());
        let x = start(&spec_x, &kept_x, &Start::Open, &AtomicBool::new(false), 1).expect("X's session");
        let dead = Invite::decode(x.invite().expect("X's invite")).unwrap();
        let _ = x.stop(&AtomicBool::new(false));
        let (spec_a, kept_a) = (spec(72, free_port()), kept());
        let a = start(&spec_a, &kept_a, &Start::Open, &AtomicBool::new(false), 1).expect("A's session");
        let invite = Invite::decode(a.invite().expect("A's invite")).unwrap();
        let (spec_b, kept_b) = (spec(73, free_port()), kept());
        let b = start(&spec_b, &kept_b, &Start::Open, &AtomicBool::new(false), 1).expect("B's own session");
        let m = Manager::spawn(spec_b, kept_b.clone(), b).expect("B's manager");
        let ask = m.asker();
        assert!(ask.ask(Want::Join(invite)));
        assert!(
            until(Duration::from_secs(60), || {
                let v = ask.view();
                v.step.is_none() && matches!(v.role, Some(Role::Joiner { .. }))
            }),
            "B joined A: {:?}",
            ask.view()
        );
        (m, ask, kept_b, dead, a)
    }

    /// A Quit pressed right after a Join, while the manager leaves the old session for it, ends the manager within
    /// the leave's bound and starts nothing: no join, no session of its own (@design-reviewer's M1). Without the sticky
    /// quit the join to the dead invite would hold the quit for its 300 s.
    #[test]
    fn a_quit_while_a_join_leaves_ends_the_manager_and_dials_nobody() {
        let _slot = opens_sessions();
        let (m, ask, kept_b, dead, a) = joined_with_a_dead_invite();
        let world = kept_b.slot.world();
        let asked = Instant::now();
        assert!(ask.ask(Want::Join(dead)));
        assert!(ask.ask(Want::Quit));
        m.quit();
        let took = asked.elapsed();
        assert!(took < crate::leave::MAX_WAIT + Duration::from_secs(1), "the quit waited {took:?}");
        assert_eq!(kept_b.slot.world(), world, "no session started after the quit");
        let _ = a.stop(&AtomicBool::new(false));
    }

    /// A Cancel pressed right after a Join, while the manager leaves the old session for it, cancels the join: B opens
    /// a session of its own within seconds, and the menu says the join was cancelled (@design-reviewer's M1).
    #[test]
    fn a_cancel_while_a_join_leaves_opens_a_session_of_your_own() {
        let _slot = opens_sessions();
        let (m, ask, _, dead, a) = joined_with_a_dead_invite();
        assert!(ask.ask(Want::Join(dead)));
        assert!(ask.ask(Want::Cancel));
        assert!(
            until(Duration::from_secs(20), || {
                let v = ask.view();
                v.step.is_none() && matches!(v.role, Some(Role::Opener { .. }))
            }),
            "B is in a session of its own: {:?}",
            ask.view()
        );
        let v = ask.view();
        assert!(v.failure.as_deref().is_some_and(|f| f.contains("cancelled")), "{:?}", v.failure);
        m.quit();
        let _ = a.stop(&AtomicBool::new(false));
    }

    /// M2's refusals read as M2's message; others don't.
    #[test]
    fn both_welcome_refusals_of_a_member_still_counted_read_as_m2() {
        assert!(still_counted("this machine is a member already"));
        assert!(still_counted("its members are not the ones the invite pins (epoch 0)"));
        assert!(still_counted("refused: NotAdmitted: not admitted: the players changed since the invite was made"));
        assert!(!still_counted("the inviter is not among its members"));
    }

    /// Cancel is refused once the join told the inviter it is ready (L2), and doesn't touch the swap's flag.
    #[test]
    fn cancel_is_refused_once_the_join_said_it_is_ready() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let who = MemberKey([7; 32]);
        let view = Arc::new(Mutex::new(View {
            step: Some(Step::Joining { who, since: Instant::now(), ready: true }),
            ..View::default()
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let ask = Asker { tx, view: view.clone(), cancel: cancel.clone(), quitting: Default::default() };
        assert!(!ask.ask(Want::Cancel));
        assert!(!cancel.load(Ordering::Acquire));
        assert!(ask.ask(Want::Quit), "Quit stays");
        assert!(cancel.load(Ordering::Acquire), "and stops the wait");
        view.lock().unwrap().step = Some(Step::Joining { who, since: Instant::now(), ready: false });
        cancel.store(false, Ordering::Release);
        assert!(ask.ask(Want::Cancel), "before JoinReady");
        assert!(cancel.load(Ordering::Acquire));
    }

    /// `-s<n>` before the extension, from session 2 on (D5).
    #[test]
    fn a_later_session_writes_its_own_files() {
        assert_eq!(session_path("/a/cuts.txt", 1), "/a/cuts.txt");
        assert_eq!(session_path("/a/cuts.txt", 2), "/a/cuts-s2.txt");
        assert_eq!(session_path(r"C:\a.b\trace", 3), r"C:\a.b\trace-s3");
    }

    /// The menu's lines for a swap's steps (D2): the step, its seconds, the bound when it is long, and no Cancel once
    /// the join said it is ready.
    #[test]
    fn a_swap_s_steps_read_with_their_seconds_and_bound() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(4);
        let leaving = Step::Leaving { since: t0, bound: Some(crate::leave::MAX_WAIT) };
        assert_eq!(leaving.line(later), "Leaving the session… 4 s (up to 15 s)");
        let who = MemberKey([0xab; 32]);
        let ready = Step::Joining { who, since: t0, ready: true };
        assert!(ready.line(later).contains("can't be cancelled"), "{}", ready.line(later));
    }
}
