//! pe-app's binary (docs/spec/app.md). It runs a world alone: the simulation thread, with the stand-in agreement
//! (`pe_app::standin`) and pe-replica's replica over M1's ball pit with this machine's member admitted
//! (`pe_app::world`), drawn in VR, where the member's hands follow the headset's tracking, or in the window on the
//! main thread. The network, MCP, configuration and invites join as their crates land.
//!
//! `pe-app [--vr | --headless] [--seconds <n>] [--open | --join <invite>] [--listen <addr>] [--advertise <addr>]...
//! [--identity <path>]`. `--vr` turns VR on, as the configuration will (pe-xr's headset guard): VR opens first, and
//! pe-app goes to the desktop when VR does not open or when its session ends. `--headless` opens neither a window nor
//! VR: the world runs, with no hands but scripted ones (`--hands`, scripted.rs), until `--seconds` passes or the console asks to quit (tests, and machines with
//! no display). `--seconds` ends the run after `n` seconds, in any mode; without it the window runs until it is
//! closed.
//!
//! Without `--open` or `--join`, a run in VR or in a window opens its own session as `--open` does (option D,
//! app.md 4.1); a headless run, or one with `--alone`, runs the world alone (the headset tests need that). Machines
//! share one world (app.md 4, "Joining from a cut"; `pe_app::net`): `--open` opens the world alone at once and prints
//! an invite, and each line on standard input (or the menu's Paste reply) is a joiner's reply code; `--join <invite>` prints this machine's reply code for the
//! opener, joins at the cut that admits it and imports the world's state there. `--listen` is the UDP address to bind
//! (default 0.0.0.0:4433), `--advertise` an address the invite or reply names (default: the bound one when it is
//! specific; bound to every interface, this machine's tailnet and default-route addresses, `pe_app::net::route_addresses`), and `--identity` this machine's identity file (default: `identity.seed` in the per-user data
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
//! transport's address and a token in its environment. `--agent-name <name>`, before it, is what the world's history
//! calls the agent; by default the command's first word (`cmd` for `cmd /c start ... claude`).
//!
//! `--log <file>` writes every line the console gets, standard output and standard error, to a new UTF-8 file as
//! well, each line prefixed with the seconds since the log opened; it never overwrites a file (`pe_app::log`). With
//! it, a run needs no pipe or redirection, so Ctrl+C reaches pe-app and the file keeps the end lines. The console
//! takes `quit`, which is the same request as Ctrl+C, beside `fault` and `replay`.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pe_app::clock;
use pe_app::console::{EXIT_INTERRUPTED, Quit, REQUEST_QUIT_LIMIT, Shutdown};
use pe_app::local::{SwitchedHands, SwitchedMove};
use pe_app::net;
use pe_app::scripted::ScriptedHands;
use pe_app::session::{self, Start, Want};
use pe_app::vr::{self, XrWait};
use pe_mcp::auth::Grant;
use pe_mcp::host::HostOpts;
use pe_net_crypto::identity::{Invite, Reply};
use pe_render::desktop_hand::{DesktopHandFeed, DesktopHandSettings};
use pe_render::desktop_move::DesktopMoveFeed;
use pe_render::display::DisplaySettings;
use pe_render::drawn_time::DrawnTime;
use pe_render::effects::{EffectCommand, EffectSettings};
use pe_render::gpu::ContextError;
use pe_render::notices::StatusLine;
use pe_render::present::{FrameSlot, Presenter};
use pe_render::scene::{Environment, RenderSettings};
use pe_render::window::{DesktopConfig, DesktopReport, EffectSink, run_desktop};
use pe_seams::world_api::{MemberId, Producer, ProducerKind};
use pe_seams::{Frame, MemberKey, RigPose, SimRate};
use pe_xr::guard::Guard;
use pe_xr::loader::SystemDiscovery;
use pe_xr::locomotion::MoveRing;
use pe_xr::ring::{DEFAULT_WINDOW_NS, HandRing};

#[cfg(all(feature = "steam-audio-trial", target_arch = "x86_64", any(windows, target_os = "linux")))]
mod steam_audio;

/// The headset's display period until its frame loop reports the runtime's own (`HandRing::set_display_period`).
const VR_PERIOD_NS: i64 = 11_111_111;

/// M1 counts and logs effects; the sound library comes later (app.md 8).
struct LogEffects;

impl EffectSink for LogEffects {
    fn command(&mut self, c: &EffectCommand) {
        pe_app::err!("effect: {c:?}");
    }
}

/// What the menu needs (S3): the first session's role, the folder its copies go to, and the session manager it asks
/// (app.md 4.1), with the current session's admission.
#[derive(Clone)]
struct MenuSeed {
    role: pe_app::menu::Role,
    files: std::path::PathBuf,
    asker: session::Asker,
    replies: session::Replies,
    link_notes: pe_app::leave::LinkNotes,
}

impl MenuSeed {
    fn ui(self, console: &Arc<Quit>) -> Box<dyn pe_render::menu::Ui> {
        let host = pe_app::menu::AppHost::new(self.files, None, console.clone())
            .with_sessions(self.asker, self.replies)
            .with_link_notes(self.link_notes);
        Box::new(pe_app::menu::Menu::new(self.role, Box::new(host)))
    }
}

/// The menu (if any), with the frame-rate counter (pe_app::fps), menu or not.
fn with_fps(menu: Option<Box<dyn pe_render::menu::Ui>>) -> Option<Box<dyn pe_render::menu::Ui>> {
    Some(Box::new(pe_app::fps::FpsUi::new(menu)))
}

/// The menu's role for a session's.
fn menu_role(r: &session::Role) -> pe_app::menu::Role {
    match r {
        session::Role::Opener { invite, expires_unix_s, .. } => {
            pe_app::menu::Role::Opener { invite: invite.clone(), expires_unix_s: *expires_unix_s }
        }
        session::Role::Joiner { .. } => pe_app::menu::Role::Joiner { reply: String::new() },
        session::Role::Alone => pe_app::menu::Role::Alone,
    }
}

fn short(key: [u8; 32]) -> String {
    key[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// A second pe-app on this machine finds the port taken (Fable's condition 4): say so plainly. A relaunched run (the
/// menu's restart, `PE_MENU_RELAUNCH=1`) waits briefly for the old process to let go of the port. Later sessions in
/// this process take another port instead, and say so beside the invite (D8).
fn port_check(listen: std::net::SocketAddr) {
    let wait_until =
        std::env::var_os(pe_app::menu::RELAUNCHED).map(|_| Instant::now() + pe_app::menu::RELAUNCH_PORT_WAIT);
    let in_use = loop {
        match std::net::UdpSocket::bind(listen) {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if wait_until.is_some_and(|t| Instant::now() < t) {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                break true;
            }
            _ => break false,
        }
    };
    if in_use {
        fail(&format!(
            "another pe-app is running on this machine (UDP {listen} is in use): quit it first, or start this one with \
             --listen <another address>"
        ));
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
    /// `--alone`: the world alone, with no session. Without it and without `--open` or `--join`, a run in VR or in a
    /// window opens its own session (option D, @coordinator-opus); a headless run stays alone, so tests
    /// and tools never take the port.
    alone: bool,
    listen: std::net::SocketAddr,
    advertise: Vec<std::net::SocketAddr>,
    identity: Option<std::path::PathBuf>,
    /// pe-mcp's transports for an agent (app.md 3.1): a pipe, loopback HTTP, or both; none runs no MCP thread.
    mcp_pipe: bool,
    mcp_http: bool,
    /// The agent's command line, launched with the transport's address and a token in its environment.
    agent: Vec<String>,
    /// `--agent-name`: what the world's history calls the agent (its producer's harness), in place of the command's
    /// first word.
    agent_name: Option<String>,
    /// `--drawn-time`: X8 for every presenter, in place of the defaults (VR Present, the desktop (a)).
    drawn_time: Option<DrawnTime>,
    /// B's delay setting (the user's, app.md 3): how much of the display lead the shown present does
    /// not cover, so the world is drawn that long before the moment shown. 0 by default.
    display_delay: Duration,
    /// `--preset ball-pit|plane`: the world an `--open` session starts (genesis.rs `PRESET`, `PLANE`); the ball pit
    /// with 500 balls when not given. Only the opener's choice counts: a joiner builds the world its imported record
    /// names, and says so if it was given one (@engine-architect).
    preset: Option<pe_world::preset::Preset>,
    /// `--inject-fault-at`: the frame at which this machine injects a deliberate test fault (pe-app's `fault`).
    inject_fault_at: Option<Frame>,
    /// `--arm-fault`: the console's typed `fault` injects that test fault at the next frame.
    arm_fault: bool,
    /// `--slow-frame <ms>`: every simulated frame waits that long, and the console's `slow <ms>` changes it
    /// (pe_app::slowframe; K4's R3').
    slow_frame: Option<u32>,
    /// `--hands <routine|file>`: scripted hands from the start (scripted.rs).
    hands: Option<String>,
    /// `--check-replay`: the opener keeps its final log, and the console's `replay` replays it (pe-app's `replay`).
    check_replay: bool,
    /// `--log`: a file that gets every line the console does, prefixed with the time (pe-app's `log`).
    log: Option<std::path::PathBuf>,
}

fn options() -> Options {
    let mut o = Options {
        vr: false,
        headless: false,
        hands: None,
        run_for: None,
        shared: None,
        alone: false,
        listen: "0.0.0.0:4433".parse().unwrap_or_else(|_| usage()),
        advertise: Vec::new(),
        identity: None,
        mcp_pipe: false,
        mcp_http: false,
        agent: Vec::new(),
        agent_name: None,
        drawn_time: None,
        display_delay: Duration::ZERO,
        preset: None,
        inject_fault_at: None,
        arm_fault: false,
        slow_frame: None,
        check_replay: false,
        log: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--vr" => o.vr = true,
            "--headless" => o.headless = true,
            "--open" => o.shared = Some(Shared::Open),
            "--alone" => o.alone = true,
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
            "--agent-name" => o.agent_name = Some(args.next().filter(|n| !n.is_empty()).unwrap_or_else(|| usage())),
            // The rest of the command line is the agent's.
            "--agent" => o.agent = args.by_ref().collect(),
            "--drawn-time" => o.drawn_time = Some(args.next().and_then(|a| drawn_time(&a)).unwrap_or_else(|| usage())),
            "--display-delay" => {
                let ms = args.next().and_then(|n| n.parse::<f64>().ok()).filter(|ms| *ms >= 0.0);
                o.display_delay =
                    ms.and_then(|ms| Duration::try_from_secs_f64(ms / 1e3).ok()).unwrap_or_else(|| usage());
            }
            "--preset" => {
                o.preset = Some(match args.next().as_deref() {
                    Some("ball-pit") => pe_world::preset::Preset::BallPit { balls: 500 },
                    Some("plane") => pe_world::preset::Preset::Plane,
                    _ => usage(),
                })
            }
            "--arm-fault" => o.arm_fault = true,
            "--slow-frame" => o.slow_frame = Some(args.next().and_then(|a| a.parse().ok()).unwrap_or_else(|| usage())),
            "--hands" => o.hands = Some(args.next().filter(|h| !h.is_empty()).unwrap_or_else(|| usage())),
            "--check-replay" => o.check_replay = true,
            "--log" => o.log = Some(args.next().filter(|p| !p.is_empty()).unwrap_or_else(|| usage()).into()),
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

/// A presenter drawing by `drawn_time`, showing the simulation's status line (a join's catch-up).
fn presenter(
    rate: SimRate,
    drawn_time: DrawnTime,
    status: &Arc<StatusLine>,
    #[cfg(all(feature = "steam-audio-trial", target_arch = "x86_64", any(windows, target_os = "linux")))] audio: Option<
        &steam_audio::PublishAudio,
    >,
) -> Presenter {
    let display = DisplaySettings { drawn_time, ..DisplaySettings::default() };
    let mut p = Presenter::new(display, EffectSettings::default(), Environment::default(), rate.hz());
    p.set_status(status.clone());
    // The console's `draw latched|simulated` (pe_app::handfeel).
    p.set_own_hands_switch(pe_app::handfeel::console().draw.clone());
    #[cfg(all(feature = "steam-audio-trial", target_arch = "x86_64", any(windows, target_os = "linux")))]
    if let Some(audio) = audio {
        audio.attach(&mut p);
    }
    p
}

/// The console (app.md 4): each line it reads is a command. `quit` is the same request as Ctrl+C: the first one
/// quits gracefully, a second one ends the process at once (console.rs). `fault` fires the test fault `--arm-fault`
/// armed, and is refused without it (pe-app's `fault`). `replay` replays the opener's session from frame 0, with
/// `--check-replay` (`replay` true; pe-app's `replay`). On A, any other line is a joiner's reply code, for the
/// session's hook, which checks it. The thread waits on standard input for the whole run; the process's end ends it.
/// A debug build also takes `panic`, which panics this thread: the test of `--log`'s panic path (tests/console_log.rs).
/// The console's lines, on a thread of its own, for the whole run. A reply code goes to the current opener session's
/// admission (the hidden fallback); `new`, `join <invite>` and `cancel` ask the session manager (D6), once it runs.
fn read_console(replies: session::Replies, replay: bool, quit: Arc<Quit>, sessions: Arc<OnceLock<session::Asker>>) {
    let read = move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line == "quit" {
                quit.request_from_app("typed `quit`");
                continue;
            }
            if cfg!(debug_assertions) && line == "panic" {
                panic!("the console's test panic (`panic`, debug builds only)");
            }
            // Undocumented: the menu's New session from the console, for the no-orphan check of a relaunch and a
            // relaunch of a relaunch without a menu press (@coordinator-opus; app.md 4.1).
            if line == "relaunch" {
                pe_app::menu::ask_relaunch(&pe_app::menu::Relaunch::NewSession);
                quit.request_from_app("the console: restarting");
                continue;
            }
            // In-game sessions (app.md 4.1, D6): what the menu's New session, Join and Cancel do.
            if line == "new" || line == "cancel" || line == "join" || line.starts_with("join ") {
                let Some(asker) = sessions.get() else {
                    pe_app::err!("pe-app: `{line}` waits for the first session to start");
                    continue;
                };
                if line == "new" {
                    asker.ask(Want::New);
                } else if line == "cancel" {
                    if !asker.ask(Want::Cancel) {
                        pe_app::err!("pe-app: they've let this machine in: the join can't be cancelled now");
                    }
                } else {
                    match Invite::decode(line["join".len()..].trim()) {
                        Ok(i) if i.expiry_unix_s <= unix_now() => {
                            pe_app::err!("pe-app: that invite has expired: ask for a new one");
                        }
                        Ok(i) => {
                            asker.ask(Want::Join(i));
                        }
                        Err(e) => pe_app::err!("pe-app: `join` takes an invite (pe1:…): {e:?}"),
                    }
                }
                continue;
            }
            if line == "replay" {
                if replay {
                    pe_app::replay::request();
                } else {
                    pe_app::err!("pe-app: `replay` is refused: this machine wasn't started with --check-replay");
                }
                continue;
            }
            if line == "hands" || line.starts_with("hands ") {
                match pe_app::scripted::installed() {
                    Some(h) => {
                        if let Err(e) = h.on_command(&line["hands".len()..]) {
                            pe_app::err!("pe-app: {e}");
                        }
                    }
                    None => pe_app::err!("pe-app: `hands` isn't ready yet"),
                }
                continue;
            }
            if line == "spawn" || line.starts_with("spawn ") {
                match pe_app::spawn::installed() {
                    Some(s) => match s.on_command(&line["spawn".len()..]) {
                        Ok(said) => pe_app::out!("{said}"),
                        Err(e) => pe_app::err!("pe-app: {e}"),
                    },
                    None => pe_app::err!("pe-app: `spawn` needs a session with a member on the plane (--preset plane)"),
                }
                continue;
            }
            // The world's shape limit (the friend test 11-27): `limit`, or `limit shapes <n>` to change it mid-session.
            if line == "limit" || line.starts_with("limit ") {
                match pe_app::spawn::installed() {
                    Some(s) => match s.on_limit(&line["limit".len()..]) {
                        Ok(said) => pe_app::out!("{said}"),
                        Err(e) => pe_app::err!("pe-app: {e}"),
                    },
                    None => pe_app::err!("pe-app: `limit` needs a session with a member on the plane (--preset plane)"),
                }
                continue;
            }
            if line == "fault" {
                match pe_app::fault::on_command(pe_app::fault::armed().as_deref()) {
                    Ok(said) => pe_app::out!("{said}"),
                    Err(e) => pe_app::err!("pe-app: {e}"),
                }
                continue;
            }
            if let Some(arg) = line.strip_prefix("slow").filter(|a| a.is_empty() || a.starts_with(' ')) {
                match pe_app::slowframe::on_command(pe_app::slowframe::armed().as_deref(), arg) {
                    Ok(said) => pe_app::out!("{said}"),
                    Err(e) => pe_app::err!("pe-app: {e}"),
                }
                continue;
            }
            // `hand-mode closed|reach` and `draw latched|simulated` (the headset test's hand feel, pe_app::handfeel).
            if let Some(said) = pe_app::handfeel::console().command(line) {
                match said {
                    Ok(said) => pe_app::out!("{said}"),
                    Err(e) => pe_app::err!("pe-app: {e}"),
                }
                continue;
            }
            let admitting = replies.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let Some(admitting) = admitting else {
                pe_app::err!(
                    "pe-app: `{line}` isn't a command here: `quit`, `new`, `join <invite>`, `cancel`, \
                     `hands <routine|file|stop>`, `spawn box|sphere|capsule|stack [count]|clear`, `fault` (with \
                     --arm-fault), `slow <ms>` (with --slow-frame), `replay` (--check-replay), \
                     `hand-mode closed|reach` or `draw latched|simulated`"
                );
                continue;
            };
            match Reply::decode(line) {
                Ok(reply) => {
                    if admitting.send(reply).is_err() {
                        pe_app::err!("pe-app: this session's admission has ended");
                    }
                }
                Err(e) => pe_app::err!("pe-app: that isn't a reply code ({e:?}): paste the code the joiner printed"),
            }
        }
    };
    if let Err(e) = std::thread::Builder::new().name("pe-console".into()).spawn(read) {
        pe_app::err!("pe-app: the console can't be read ({e}): nobody can join, and `fault` isn't heard");
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn main() {
    let mut options = options();
    if options.alone && options.shared.is_some() {
        fail("--alone excludes --open and --join");
    }
    if options.shared.is_none() && !options.alone && !options.headless {
        options.shared = Some(Shared::Open);
    }
    // The run's log (`--log`, pe-app's `log`), before anything else prints: it never overwrites a file. pe-xr's,
    // pe-mcp's and Box3D's lines come through it too, with or without a log. Neither starts a thread.
    if let Some(path) = &options.log {
        if let Err(e) = pe_app::log::open(path) {
            let why = if e.kind() == std::io::ErrorKind::AlreadyExists {
                "it exists, and pe-app never overwrites a log: name a new file".to_string()
            } else {
                e.to_string()
            };
            fail(&format!("the log {} can't be opened: {why}", path.display()));
        }
        pe_app::log::install_panic_hook();
    }
    pe_xr::diag::set_sink(pe_app::log::err_line);
    pe_mcp::host::set_line_sink(pe_app::log::err_line);
    pe_physics::set_warning_sink(pe_app::log::err_line);
    // Before any thread starts (console.rs): Ctrl+C or closing the console quits gracefully.
    let console = Quit::install().unwrap_or_else(|e| {
        pe_app::err!("pe-app: {e}; Ctrl+C ends the process without leaving");
        Quit::new()
    });
    // Others share the world (`--open` or `--join`): the quit then leaves gracefully (app.md 4).
    // Which sources this build came from, and its commit (app.md 4, "The build id").
    pe_app::out!("{}", pe_app::net::source_line());
    // Which pe-sdk scripts build against: PE_SDK, beside the executable, or the source tree (builds::sdk).
    match pe_app::builds::sdk_line() {
        Ok(line) => pe_app::out!("{line}"),
        Err(warning) => pe_app::err!("{warning}"),
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
            Ok(line) => pe_app::out!("{line}"),
            Err(e) => fail(&e),
        }
    }
    // The replay costs (pe_replica::costs; the 11-27 test's profiling), armed before any replica is made, as the fault
    // is: on unless `PE_REPLAY_COSTS=0`. The end line's `replay_costs` shows them. They change no state.
    if std::env::var("PE_REPLAY_COSTS").is_ok_and(|v| v.trim() == "0") {
        pe_app::out!("replay costs: off (PE_REPLAY_COSTS=0)");
    } else {
        pe_app::world::arm_cost_probe();
    }
    // `PE_PHYSICS_WORKERS=<n>`: physics worker threads for every replica (pe_app::genesis::set_physics_workers; the
    // re-solve ladder's E2 trial), set before any replica is made. 1 by default; it changes no result.
    if let Some(n) = std::env::var("PE_PHYSICS_WORKERS").ok().and_then(|v| v.trim().parse::<i32>().ok()) {
        let n = pe_app::genesis::set_physics_workers(n);
        pe_app::out!("physics workers: {n} (PE_PHYSICS_WORKERS)");
    }
    // `PE_SKIP_REPLAY_ROOTS=0`: re-steps take their grid roots again (E3 off; pe_app::genesis::set_skip_replay_roots).
    if std::env::var("PE_SKIP_REPLAY_ROOTS").is_ok_and(|v| v.trim() == "0") {
        pe_app::genesis::set_skip_replay_roots(false);
        pe_app::out!("roots in replays: taken (PE_SKIP_REPLAY_ROOTS=0)");
    }
    // A deliberate slowdown on this machine only (K4's R3'), armed before any replica is made, as the fault is. It changes
    // no state, so `--check-replay` may run with it (the replay waits too).
    if let Some(ms) = options.slow_frame {
        match pe_app::slowframe::arm(ms) {
            Ok(line) => pe_app::out!("{line}"),
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
    pe_render::gpu::set_context_log(|d| pe_app::out!("render: {d}"));
    let rate = SimRate::DEFAULT;
    let host = clock::shared();
    let slot = Arc::new(FrameSlot::new());
    #[cfg(all(feature = "steam-audio-trial", target_arch = "x86_64", any(windows, target_os = "linux")))]
    let audio_trial = steam_audio::AudioTrial::from_env();
    #[cfg(all(feature = "steam-audio-trial", target_arch = "x86_64", any(windows, target_os = "linux")))]
    let audio = audio_trial.as_ref().map(steam_audio::AudioTrial::publisher);
    // The local player's hands (app.md 3, step 2): VR's ring while an XR session runs, the desktop hand otherwise.
    let ring = Arc::new(HandRing::new(DEFAULT_WINDOW_NS, VR_PERIOD_NS));
    // B (app.md 3): the simulation asks for hands up to two simulation periods and the publication's margin past the
    // newest sample, the display time's pose (xr.md 6.2; the margin is what `shown::ahead_ns` adds, so the worst read
    // stays inside the clamp, @xr-eng's nit on dc6a7a9b). Reads that far past it are by design: judge xr.md 6.2's
    // switch-over threshold, and the sample-age histogram of a worn run, against one display period plus this
    // lookahead, not one period.
    ring.set_lookahead_ns(2 * 1_000_000_000 / i64::from(rate.hz()) + pe_app::shown::PUBLISH_MARGIN_NS);
    let desktop_hand = Arc::new(DesktopHandFeed::default());
    // Scripted hands (scripted.rs) over the desktop hand: `--hands` or the console's `hands` plays a script.
    let scripted = Arc::new(ScriptedHands::new(slot.clone(), desktop_hand.clone()));
    if let Some(what) = &options.hands {
        match pe_app::scripted::load(what) {
            Ok(script) => scripted.start(script),
            Err(e) => fail(&format!("--hands: {e}")),
        }
    }
    pe_app::scripted::install(scripted.clone());
    let hands = Arc::new(SwitchedHands::new(ring.clone(), scripted));
    // The local player's locomotion (friend-test L4b), switched with the hands: the desktop's keys and look, and
    // pe-xr's move ring (L4a) while an XR session runs, written each focused display frame.
    let desktop_moves = Arc::new(DesktopMoveFeed::default());
    let vr_moves = Arc::new(MoveRing::default());
    let moves = Arc::new(SwitchedMove::new(vr_moves.clone(), desktop_moves.clone()));
    // B (app.md 3): the display lead the VR sink measures under Shown, for the simulation's shown present.
    let display_lead = Arc::new(pe_app::shown::DisplayLead::default());
    // The window's status line (render.md 8.1): a joiner's "Joining: catching up…", and a swap's steps.
    let status = Arc::new(StatusLine::default());
    // pe-api's ports (app.md 3.1) whenever the replica is pe-world's and this machine has a member: the build thread
    // submits its attestations through their edit queue (@engine-architect). The MCP thread only when a
    // transport is asked for, restarted with each session (D4).
    let mcp_wanted = options.mcp_pipe || options.mcp_http;
    if !mcp_wanted && !options.agent.is_empty() {
        fail("--agent needs --mcp-pipe or --mcp-http");
    }
    if options.agent_name.is_some() && options.agent.is_empty() {
        fail("--agent-name needs --agent, after it: --agent takes the rest of the line");
    }
    // view_capture's queue (api.md 8), served by the headset's loop or the window's, whichever draws. Headless draws
    // nothing, so it has no queue, and pe-api answers view_capture with `capture_unavailable` at once.
    let captures = (mcp_wanted && !headless).then(pe_render::capture_service::CaptureQueue::new);
    // This machine's identity: every session's, so each is the same member. The build service's folder and the
    // menu's files sit beside it.
    let identity =
        options.identity.clone().map_or_else(net::default_identity_path, Ok).unwrap_or_else(|e| fail(&e.to_string()));
    let seed = if networked {
        net::identity_seed(&identity)
            .unwrap_or_else(|e| fail(&format!("the identity file {}: {e}", identity.display())))
    } else {
        [0; 32]
    };
    if networked {
        port_check(options.listen);
    }
    let mcp: Option<session::McpOpts> = mcp_wanted.then(|| {
        let (name, agent, pipe, http) =
            (options.agent_name.clone(), options.agent.clone(), options.mcp_pipe, options.mcp_http);
        let file = Some(pe_app::menu::files_dir(&identity).join("mcp.json"));
        Arc::new(move |member| mcp_opts(name.as_deref(), &agent, pipe, http, &file, member)) as session::McpOpts
    });
    let spec = session::Spec {
        listen: options.listen,
        advertise: options.advertise.clone(),
        identity: identity.clone(),
        seed,
        preset: options.preset.unwrap_or(pe_world::preset::Preset::BallPit { balls: 500 }),
        display_delay: options.display_delay,
        check_replay: options.check_replay,
        mcp,
    };
    let replies: session::Replies = Arc::new(Mutex::new(None));
    // The menu's note about a member's link (app.md 4.1): it dropped, or it closed and New session makes a new invite.
    let link_notes = pe_app::leave::LinkNotes::default();
    let kept = session::Kept {
        host: host.clone(),
        slot: slot.clone(),
        hands: hands.clone(),
        moves: moves.clone(),
        display_lead: display_lead.clone(),
        status: status.clone(),
        captures: captures.clone(),
        quit: console.clone(),
        replies: replies.clone(),
        link_notes: link_notes.clone(),
    };
    // The console from now (@coordinator-opus): `quit` ends a joiner that is still waiting.
    let sessions: Arc<OnceLock<session::Asker>> = Arc::new(OnceLock::new());
    read_console(replies.clone(), options.check_replay, console.clone(), sessions.clone());
    // The first session (D9, @coordinator-opus): a headless `--join` joins before anything else, and a
    // failure ends the run with exit code 2, as the scripts expect. In VR or a window, `--join` opens a session of
    // this machine's own first and joins in-game, so the display starts at once and the menu shows the join.
    let first_start = match &options.shared {
        None => Start::Alone,
        Some(Shared::Join(invite)) if headless => Start::Join(invite.clone()),
        Some(_) => Start::Open,
    };
    if matches!(options.shared, Some(Shared::Join(_))) && options.preset.is_some() {
        pe_app::out!("--preset is ignored when joining: the world is the one the opener's record names");
    }
    let on_join_failed = matches!(first_start, Start::Join(_)).then(|| {
        let quit = console.clone();
        Box::new(move |why: String| {
            pe_app::err!("pe-app: the join failed: {why}");
            quit.request_from_app("the join failed")
        }) as Box<dyn FnOnce(String) + Send>
    });
    let first =
        session::start_with(&spec, &kept, &first_start, &AtomicBool::new(false), 1, on_join_failed, &mut || true)
            .unwrap_or_else(|e| fail(&e.to_string()));
    let display_delay = first.display_delay;
    let first_role = first.role.clone();
    let manager = session::Manager::spawn(spec, kept, first)
        .unwrap_or_else(|e| fail(&format!("the session manager did not start: {e}")));
    let asker = manager.asker();
    let _ = sessions.set(asker.clone());
    if let (Some(Shared::Join(invite)), false) = (&options.shared, headless) {
        pe_app::out!("joining {}'s session in-game: this machine's own session runs meanwhile", short(invite.key));
        asker.ask(Want::Join(invite.clone()));
    }
    // The menu (S3), for a session shared with others: its role follows the manager's (app.md 4.1).
    let mut menu_seed = networked.then(|| MenuSeed {
        role: menu_role(&first_role),
        files: pe_app::menu::files_dir(&identity),
        asker: asker.clone(),
        replies: replies.clone(),
        link_notes: link_notes.clone(),
    });

    // The quit (app.md 4), run once: by the main thread at the end of the run, or by the console's request. The
    // manager stops the session it runs, in the shutdown's order (crate::session's `Running::stop`).
    let quit_captures = captures.clone();
    let shutdown: Shutdown = Box::new(move || {
        #[cfg(all(feature = "steam-audio-trial", target_arch = "x86_64", any(windows, target_os = "linux")))]
        drop(audio_trial);
        close_captures(&quit_captures);
        manager.quit();
    });

    if headless {
        // No window and no VR (app.md 4): the world runs until `--seconds` passes or the console asks to quit, and the
        // main thread runs the quit itself, as in VR. The desktop hand has no window to feed it, so no hands move.
        console.poll();
        pe_app::out!("headless: running until --seconds passes, Ctrl+C or a typed `quit`");
        #[cfg(all(feature = "steam-audio-trial", target_arch = "x86_64", any(windows, target_os = "linux")))]
        {
            let render = steam_audio::headless::render_requested();
            if audio.is_some() || render {
                let presenter = presenter(rate, options.drawn_time.unwrap_or_default(), &status, audio.as_ref());
                steam_audio::headless::run(presenter, &slot, &console, run_for, render);
                console.quitting();
                shutdown();
                console.finished();
                // As every quit path does: a relaunch asked for (the console's `relaunch`) runs now.
                pe_app::menu::relaunch_if_asked();
                return;
            }
        }
        wait_headless(run_for.map(|d| started + d), &console);
        console.quitting();
        shutdown();
        console.finished();
        pe_app::menu::relaunch_if_asked();
        return;
    }

    if vr {
        console.poll();
        hands.use_vr(true);
        moves.use_vr(true);
        let guard = Guard::from_env(true);
        let report = vr::ReportSlot::default();
        // B (render.md 7, app.md 3): the world at the moment shown, less the user's delay setting (0 by default).
        let drawn_time = options.drawn_time.unwrap_or_else(|| DrawnTime::shown(display_delay, rate.hz()));
        // A joiner started for VR tries again while VR doesn't start or ends (`VR_RETRY_*`): its person may still have
        // the headset off from sending the reply code (@coordinator-opus).
        let retries = if matches!(options.shared, Some(Shared::Join(_))) { VR_RETRIES } else { 0 };
        let mut tries = 0;
        loop {
            let drawing = vr::Drawing {
                presenter: presenter(
                    rate,
                    drawn_time,
                    &status,
                    #[cfg(all(
                        feature = "steam-audio-trial",
                        target_arch = "x86_64",
                        any(windows, target_os = "linux")
                    ))]
                    audio.as_ref(),
                ),
                render: RenderSettings::default(),
                effects: Box::new(LogEffects),
                captures: captures.clone(),
                report: report.clone(),
                shown: Some(display_lead.clone()),
                // The menu, with the frame-rate counter under the wrist button (pe_app::fps), menu or not.
                ui: with_fps(menu_seed.clone().map(|s| s.ui(&console))),
                // The move feed the drawn rig turns with (locomotion L4c): pe-xr's MoveRing once L4a/L4b wire it, the
                // same Arc the simulation samples.
                // L4c: the sink's drawn rig reads the same ring the simulation does.
                moves: Some(vr_moves.clone()),
            };
            let why = match vr::start(&SystemDiscovery, &guard, ring.clone(), vr_moves.clone(), slot.clone(), drawing) {
                Ok(xr) => {
                    // TODO(pe-render): in VR mode the main thread pumps a window's events and input, the desktop
                    // window draws on the XR device while the session is idle or stopping (xr.md 3.1), and the XR
                    // thread draws the desktop mirror (app.md 1, render.md 10). Until pe-render has that window the
                    // main thread waits here; xr.md 3.3's retry after a lost session comes with it (a joiner's
                    // retry below is the friend test's stand-in).
                    let waited = wait_vr(&xr, run_for.map(|d| started + d), &console);
                    pe_app::out!("vr: {waited:?}; at its end {:?}", xr.stop());
                    match report.lock().ok().and_then(|mut r| r.take()) {
                        Some(r) => {
                            pe_app::out!("vr display lead (ms): {:?}", r.lead_ms);
                            pe_app::out!("display (vr, {drawn_time:?}): {:?}", r.render);
                        }
                        None => pe_app::out!("display (vr): no report (the XR thread has not ended)"),
                    }
                    if waited == XrWait::Running || console.requested() {
                        console.quitting();
                        shutdown();
                        console.finished();
                        // After the XR thread stopped (the session and instance gone) and the quit closed the port.
                        pe_app::menu::relaunch_if_asked();
                        return;
                    }
                    "the VR session ended".to_string()
                }
                Err(why) => why.to_string(),
            };
            vr_moves.clear();
            let over = run_for.is_some_and(|d| started.elapsed() >= d);
            if tries >= retries || over || console.requested() {
                pe_app::err!("pe-app: {why}; the desktop instead");
                break;
            }
            tries += 1;
            pe_app::err!(
                "pe-app: {why}; trying VR again in {} s ({tries} of {retries}): put the headset on, or type quit",
                VR_RETRY_EVERY.as_secs()
            );
            let again = Instant::now() + VR_RETRY_EVERY;
            while Instant::now() < again && !console.requested() {
                std::thread::sleep(Duration::from_millis(100));
            }
            if console.requested() {
                console.quitting();
                shutdown();
                console.finished();
                // A relaunch asked for while VR is retried (the console's `relaunch`) runs now, as on every quit path.
                pe_app::menu::relaunch_if_asked();
                return;
            }
        }
        hands.use_vr(false);
        moves.use_vr(false);
        vr_moves.clear();
    }

    // A request during VR, or before the window: quit now. Otherwise a request runs the quit on its own thread.
    if let Some(now) = console.arm(shutdown) {
        now();
        console.finished();
        pe_app::menu::relaunch_if_asked();
        return;
    }
    let cfg = DesktopConfig {
        title: "Primordial Engine".into(),
        size: [1280, 720],
        present_mode: pe_render::wgpu::PresentMode::Fifo,
        vertical_fov: 1.0,
        clock: host,
        presenter: presenter(
            rate,
            options.drawn_time.unwrap_or_default(),
            &status,
            #[cfg(all(feature = "steam-audio-trial", target_arch = "x86_64", any(windows, target_os = "linux")))]
            audio.as_ref(),
        ),
        render: RenderSettings::default(),
        hand: DesktopHandSettings::default(),
        eye: RigPose { position: [0.0, 1.4, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
        // What is left of the run after VR, never zero, which the window would take as no time at all.
        run_for: run_for.map(|d| d.saturating_sub(started.elapsed()).max(Duration::from_millis(1))),
        // A test aid: PE_WINDOW_SNAPSHOT=<png path> saves the window's own frame (read back, never a screen capture)
        // 4 s into the run, the menu included.
        snapshot: std::env::var_os("PE_WINDOW_SNAPSHOT").map(|p| (Duration::from_secs(4), p.into())),
        captures: captures.clone(),
        // The menu, if VR didn't take it (a run in the window, or VR that ended), with the frame-rate counter at the top
        // of the window (pe_app::fps), menu or not.
        ui: with_fps(menu_seed.take().map(|s| s.ui(&console))),
        moves: Some(desktop_moves),
        walk: Default::default(),
    };
    // The window, its surface and its GPU context are made on this thread, one context at a time (app.md 4): the XR
    // thread has dropped its own context before `wait` answers.
    let window = run_desktop(cfg, slot, desktop_hand, Box::new(LogEffects));
    report_window(window);
    match console.take() {
        Some(shutdown) => {
            shutdown();
            console.finished();
            pe_app::menu::relaunch_if_asked();
        }
        // A console request is running the quit, and ends the process when it is done; if it hasn't within the leave's
        // longest bound (a panic in it), end the process here.
        None => {
            let done = console.wait_quit(REQUEST_QUIT_LIMIT);
            if !done {
                pe_app::err!("pe-app: the console's quit did not finish; ending now");
            }
            std::process::exit(if done { 0 } else { EXIT_INTERRUPTED });
        }
    }
}

/// A joiner started for VR tries VR again this often, and this many times (3 minutes), before the desktop: while it
/// doesn't start (no headset yet), or after its session ends (@coordinator-opus).
const VR_RETRY_EVERY: Duration = Duration::from_secs(5);
const VR_RETRIES: u32 = 36;

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

/// What the world's history calls the agent: `--agent-name` if given, else the command's first word as a file stem
/// (`claude` for `tools/claude.exe`, and for `C:\tools\claude.exe` on Windows only, where a backslash
/// separates path components; `cmd` for `cmd /c start ... claude`).
fn harness_name(name: Option<&str>, command: &[String]) -> Option<String> {
    if let Some(n) = name {
        return Some(n.to_owned());
    }
    command.first().map(|c| {
        let stem = std::path::Path::new(c).file_stem().map(|s| s.to_string_lossy().into_owned());
        stem.unwrap_or_else(|| c.clone())
    })
}

/// pe-mcp's options: the transports asked for, and the agent's command line and grant. The agent acts for this
/// machine's member, as an agent (pe-world's `Actor::Agent`), named by `--agent-name` or else its command.
fn mcp_opts(
    agent_name: Option<&str>,
    agent: &[String],
    pipe: bool,
    http: bool,
    connection_file: &Option<std::path::PathBuf>,
    member: MemberKey,
) -> HostOpts {
    let harness = harness_name(agent_name, agent);
    let producer = Producer { kind: ProducerKind::Agent, harness, principal: None, requested_by: None };
    let mut opts = HostOpts::new(Grant { member: MemberId::from(member), producer, confirm: false });
    opts.pipe = pipe;
    opts.http = http;
    opts.cmd = agent.to_vec();
    // With no `--agent`, the endpoint and a token go to a file for an agent started by hand (the friend test 11-27:
    // the user's own agent drives the running game): `mcp.json` in the menu's folder beside the identity file.
    if agent.is_empty() && http {
        opts.connection_file = connection_file.clone();
    }
    opts
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

fn report_window(window: Result<DesktopReport, ContextError>) {
    match window {
        Ok(r) => {
            pe_app::out!("window: {} frames on {}, {} skipped", r.frames, r.adapter, r.skipped);
            if let Some(d) = &r.render {
                pe_app::out!("display (desktop): {d:?}");
            }
        }
        Err(e) => pe_app::err!("window: {e:?}"),
    }
}

fn usage() -> ! {
    fail(
        "usage: pe-app [--vr | --headless] [--seconds <n>] [--open | --join <invite>] [--listen <addr>] \
         [--alone] [--advertise <addr>]... [--identity <path>] [--drawn-time present:<ms>|a:<ms>|b:<ms>] [--display-delay <ms>] [--preset ball-pit|plane] [--mcp-pipe] \
         [--mcp-http] [--arm-fault | --inject-fault-at <frame>] [--slow-frame <ms>] [--check-replay] [--hands <routine|file>] [--log <file>] \
         [--agent-name <name>] [--agent <command> <args>...]",
    )
}

fn fail(msg: &str) -> ! {
    pe_app::err!("pe-app: {msg}");
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::harness_name;

    #[test]
    fn the_agent_is_named_by_agent_name_or_else_by_its_command() {
        let line = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let through_cmd = line("cmd /c start Claude /wait claude");
        assert_eq!(harness_name(None, &through_cmd).as_deref(), Some("cmd"));
        assert_eq!(harness_name(Some("claude"), &through_cmd).as_deref(), Some("claude"));
        assert_eq!(harness_name(None, &line("tools/claude.exe --verbose")).as_deref(), Some("claude"));
        // A backslash separates path components only on Windows (tier 2 run 44331596600 failed elsewhere).
        if cfg!(windows) {
            assert_eq!(harness_name(None, &line(r"C:\tools\claude.exe --verbose")).as_deref(), Some("claude"));
        }
        assert_eq!(harness_name(None, &[]), None);
    }
}
