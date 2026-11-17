//! pe-app's network side (app.md 3 and 4 step 4; transport.md 6 and 7), @shader-lang's.
//!
//! - **The endpoint** ([`Endpoint::bind`]): this machine's identity as pe-net's key, the socket, pe-net's network
//!   thread and its handle. The handle becomes the simulation's `Transport` (app.md 3); [`MemberHook`], the
//!   simulation's `NetHook`, keeps pe-net's control end.
//! - **The M1 interim** (@coordinator-opus md61c5a56424e): pe-agree has no join yet, so the second machine becomes a
//!   member at genesis. A opens, prints an invite and takes B's reply ([`Endpoint::open_session`]); only then does it
//!   build the genesis record with both keys, start the clock with frame 0 a little ahead, and admit B with a welcome
//!   carrying the record. B dials, prints its reply and, on the welcome, checks the line, the members, the build, the
//!   epoch and the rate before it rebuilds genesis itself ([`Endpoint::join_session`]). Any other join request is
//!   refused, "not admitted". When pe-agree's join from a cut lands, only B's path changes (a state import instead of
//!   rebuilding genesis).
//! - **Both hellos and the welcome carry one fixed line** ([`pre_genesis_line`]): the connection guards compare lines
//!   (transport.md 6.3) and pe-net refuses a welcome naming another, and B cannot know the session's line before its
//!   welcome. The session's own line is the genesis record's (`Genesis::line`), which B derives from the record
//!   itself; every pe-agree signature commits to it.

use std::fmt;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use pe_agree::Node;
use pe_net::core::CoreError;
use pe_net::core::{Config, Core, Lifecycle};
use pe_net::lifecycle::{CloseCode, Hello, HelloError, Refusal, Welcome};
use pe_net::thread::{Arrival, Command, HostClock, NetControl, NetHandle};
use pe_net_crypto::identity::{Identity, Invite, Reply, StrictVerifier};
use pe_seams::{Agreement, FrameClock, HostTime, MemberKey, NetEvent, Replica, SimRate, Transport};

use pe_replica::ScriptRunner;

use crate::genesis::{Genesis, GenesisError, Params, fresh_nonce};
use crate::sim::{NetHook, Waker};
use crate::world::DEV_MEMBER;

/// The engine build both sides' hellos carry and the genesis record names: connections between different builds are
/// refused at the hello (transport.md 6.3), and a record from another build at the welcome, by name.
///
/// It is pe-app's version, the script build service's identity B and the script runner's identity:
/// - B (logic.md 6.2b, `pe_logic::build::build_identity`: the toolchain's release, its wasm32 std, pe-sdk, the build
///   service's and pe-logic's result versions), so two machines whose builds of one script could differ never share a
///   session, and a build that one member passes and another fails, which the build-asymmetry ruling makes a split
///   (m327e1d2d1222), cannot come from a different engine or toolchain. Without a toolchain the service can run (no
///   `CARGO` until D75 installs the pinned one) it reads "scripts unavailable", with no detail, so the text is the same
///   on every such machine [I: two such machines can still share a session, and neither can build a script].
/// - The runner's identity ([`runner_settings`]' `pe_logic::identity`, the bytes the runner reports as S2's
///   `runner_identity`: pe-logic's result version, the wasmtime release, the metering pass's and the ABI's versions,
///   the limits and the fuel), so two machines whose runners could compute one script's frame differently never
///   share a session either (Q1, @coordinator-opus m339785d800ca; S2 v1.7: "the hello compares it").
///
/// Each appears as the first 6 bytes of its hash in hex, since a hello's build id is at most `MAX_BUILD` (64) bytes and
/// `bind` refuses a longer one: 55 at version 0.1.0, which leaves the version 9 bytes to grow, and 48 bits still tell
/// any two builds apart by accident (a peer can claim any build id at any length). Computed once per process.
pub fn build_id() -> &'static [u8] {
    static ID: OnceLock<Vec<u8>> = OnceLock::new();
    ID.get_or_init(|| {
        let scripts = pe_logic::build::Toolchain::current()
            .and_then(|tc| pe_logic::build::build_identity(&tc, &pe_logic::build::engine_sdk()))
            .map(|b| hex6(&b))
            .unwrap_or_else(|_| "unavailable".into());
        let (settings, run) = runner_settings();
        let runner = runner_part(&settings, &run);
        format!("pe-app {}; scripts {scripts}; runner {runner}", env!("CARGO_PKG_VERSION")).into_bytes()
    })
}

/// The script runner's settings: main.rs starts its `LogicRunner` with these, and [`build_id`] carries their identity,
/// so the hello compares what the runner computes with.
pub fn runner_settings() -> (pe_logic::Settings, pe_logic::run::RunSettings) {
    (pe_logic::Settings::default(), pe_logic::run::RunSettings::default())
}

/// [`build_id`]'s runner part: the hash of the identity a runner with these settings reports (`pe_logic::identity`).
fn runner_part(settings: &pe_logic::Settings, run: &pe_logic::run::RunSettings) -> String {
    hex6(&pe_seams::Hash::of("pe.app.runner", &pe_logic::identity(settings, run)))
}

/// A hash's first 6 bytes in hex.
fn hex6(h: &pe_seams::Hash) -> String {
    h.0[..6].iter().map(|x| format!("{x:02x}")).collect()
}

/// pe-net's protocol version in the hello.
pub const PROTOCOL: u16 = 1;

/// Each direction of pe-net's queues, in messages: about `T_queue` of traffic (transport.md 10).
pub const QUEUE: usize = 4096;

/// How far ahead of the session's start A puts frame 0, so B's clock converges first (transport.md 5.4).
pub const EPOCH_LEAD: Duration = Duration::from_secs(2);

/// How far in the past frame 0 may be when B's clock converges (@coordinator-opus's L3): A puts it `EPOCH_LEAD`
/// ahead, and convergence may outlast that, after which B replays from frame 0. In the human flow A builds on the
/// join request of B's early, closed dial, so the lead must also cover A's redial and handshake: about 7 s on a LAN,
/// more over a relayed path (@engine-architect's Low 2 on f7d31a31). 20 s still refuses a welcome from minutes ago,
/// and catching up 20 s is about 1,200 frames.
pub const EPOCH_PAST_MAX: Duration = Duration::from_secs(20);

/// How far ahead frame 0 may be when B's clock converges: A's lead is 2 s; more is a welcome B shouldn't sit on.
pub const EPOCH_AHEAD_MAX: Duration = Duration::from_secs(10);

/// B's bound on the welcome's epoch, once its clock has converged and can say when frame 0 is on this machine
/// (host clocks differ per machine, so not before).
pub fn check_epoch(frame_0_at: HostTime, now: HostTime) -> Result<(), NetError> {
    // In i128: the epoch is A's to choose, and an extreme one must be refused, not overflow.
    let ahead = i128::from(frame_0_at.0) - i128::from(now.0);
    let as_ns = |d: Duration| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX);
    if ahead > as_ns(EPOCH_AHEAD_MAX) {
        return Err(NetError::Welcome(format!(
            "frame 0 is {:.1} s ahead (at most {} s)",
            ahead as f64 * 1e-9,
            EPOCH_AHEAD_MAX.as_secs()
        )));
    }
    if -ahead > as_ns(EPOCH_PAST_MAX) {
        return Err(NetError::Welcome(format!(
            "frame 0 was {:.1} s ago (at most {} s)",
            -ahead as f64 * 1e-9,
            EPOCH_PAST_MAX.as_secs()
        )));
    }
    Ok(())
}

/// The line both hellos carry before genesis (module docs): M1's preset line, never a session's own.
pub fn pre_genesis_line() -> [u8; 32] {
    pe_world::preset::m1_line().0.0
}

/// Why a session didn't start.
#[derive(Debug)]
pub enum NetError {
    Io(io::Error),
    Core(String),
    /// Nothing came in time: what was awaited.
    Timeout(&'static str),
    /// The other side refused us: its reason.
    Refused(String),
    /// The welcome didn't match: which part.
    Welcome(String),
    Genesis(GenesisError),
    /// This machine's hello breaks a bound its peers enforce (transport.md 6.3): a build id over `MAX_BUILD`.
    Hello(HelloError),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Io(e) => write!(f, "network: {e}"),
            NetError::Core(e) => write!(f, "pe-net: {e}"),
            NetError::Timeout(what) => write!(f, "timed out waiting for {what}"),
            NetError::Refused(why) => write!(f, "refused: {why}"),
            NetError::Welcome(why) => write!(f, "the welcome doesn't match: {why}"),
            NetError::Genesis(e) => write!(f, "the genesis record is refused: {e}"),
            NetError::Hello(e) => write!(f, "this build can't connect: {e}"),
        }
    }
}

impl From<io::Error> for NetError {
    fn from(e: io::Error) -> Self {
        NetError::Io(e)
    }
}

/// Where this machine's identity seed lives unless `--identity` names a file: the platform's per-user data folder
/// (`%LOCALAPPDATA%\Primordial Engine` on Windows, per machine, never the roaming profile, which could carry one
/// identity to two machines; `~/Library/Application Support/Primordial Engine` on macOS, the XDG
/// data folder elsewhere), never the working directory, where a checkout could commit it (@engine-architect's L3).
pub fn default_identity_path() -> io::Result<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = if cfg!(windows) {
        var("LOCALAPPDATA").map(|d| d.join("Primordial Engine"))
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Application Support/Primordial Engine"))
    } else {
        var("XDG_DATA_HOME")
            .or_else(|| var("HOME").map(|h| h.join(".local/share")))
            .map(|d| d.join("primordial-engine"))
    };
    dir.map(|d| d.join("identity.seed")).ok_or_else(|| {
        io::Error::other("no per-user data folder (LOCALAPPDATA, HOME or XDG_DATA_HOME): pass --identity")
    })
}

/// This machine's identity seed: read from `path` (64 hex digits), or made from the OS's randomness and written there
/// on first use, readable by its owner only on Unix. The member key and pe-agree's signer both derive from it
/// (app.md 4 step 2).
pub fn identity_seed(path: &Path) -> io::Result<[u8; 32]> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            parse_seed(text.trim()).ok_or_else(|| io::Error::other(format!("{}: not 64 hex digits", path.display())))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut seed = [0u8; 32];
            pe_net_crypto::random::os().fill(&mut seed).map_err(|_| io::Error::other("no randomness"))?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            let mut f = o.open(path)?;
            let text = seed.iter().map(|b| format!("{b:02x}")).collect::<String>() + "\n";
            io::Write::write_all(&mut f, text.as_bytes())?;
            f.sync_all()?;
            Ok(seed)
        }
        Err(e) => Err(e),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A's session, built on B's first join request.
struct Built {
    welcome: Welcome,
    members: Vec<MemberKey>,
    replica: Box<dyn Replica>,
    cfg: pe_agree::NodeConfig,
    frames: Arc<dyn FrameClock>,
    /// The record's `agree`, which `into_session` compares with `cfg` again.
    agree: pe_seams::Hash,
}

fn parse_seed(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The simulation thread's waker, once it exists: pe-net's arrival callback reads it (app.md 3 step 7). The waker
/// exists only after the simulation thread is spawned, after the network thread; until then a wake does nothing.
#[derive(Clone, Default)]
pub struct WakeLater(Arc<OnceLock<Waker>>);

impl WakeLater {
    pub fn set(&self, w: Waker) {
        let _ = self.0.set(w);
    }

    pub fn arrival(&self) -> Arrival {
        let cell = self.0.clone();
        Arc::new(move || {
            if let Some(w) = cell.get() {
                w.wake();
            }
        })
    }
}

/// This machine on the network: its key, its socket and pe-net's thread.
pub struct Endpoint {
    seed: [u8; 32],
    me: MemberKey,
    handle: NetHandle,
    addrs: Vec<SocketAddr>,
    /// The address the socket is bound to.
    local: SocketAddr,
    host: HostClock,
    /// The session's welcome, once there is one: a member that reconnects is welcomed with it again.
    welcome: Option<Welcome>,
}

/// What A chooses for a new session.
#[derive(Clone, Debug)]
pub struct OpenParams {
    pub seed: u64,
    /// The ball pit's ball count (500 in M1's demo).
    pub balls: u32,
    pub rate: SimRate,
    pub epoch_lead: Duration,
}

/// A session, started: the pieces the simulation thread takes.
pub struct Session {
    pub me: MemberKey,
    pub members: Vec<MemberKey>,
    pub handle: NetHandle,
    pub hook: MemberHook,
    pub replica: Box<dyn Replica>,
    pub node: Node,
    pub frames: Arc<dyn FrameClock>,
}

impl Endpoint {
    /// Bind `listen` and start pe-net's thread with this machine's identity. `advertise` are the addresses the invite
    /// or reply names; empty means the socket's own address (which must then be a specific one, not 0.0.0.0).
    /// `member`: A, which admits, or B, which waits to be welcomed (transport.md 6.4).
    pub fn bind(
        seed: [u8; 32],
        listen: SocketAddr,
        advertise: Vec<SocketAddr>,
        member: bool,
        host: HostClock,
        arrival: Option<Arrival>,
    ) -> Result<Endpoint, NetError> {
        Self::bind_with(seed, listen, advertise, member, host, arrival, |_| {})
    }

    /// As [`Endpoint::bind`], with `tweak` applied to pe-net's settings (tests shorten its timers).
    fn bind_with(
        seed: [u8; 32],
        listen: SocketAddr,
        advertise: Vec<SocketAddr>,
        member: bool,
        host: HostClock,
        arrival: Option<Arrival>,
        tweak: impl FnOnce(&mut Config),
    ) -> Result<Endpoint, NetError> {
        let socket = UdpSocket::bind(listen)?;
        let local = socket.local_addr()?;
        let addrs = if advertise.is_empty() { vec![socket.local_addr()?] } else { advertise };
        if addrs.iter().any(|a| a.ip().is_unspecified()) {
            return Err(NetError::Core("an unspecified address can't be advertised: name one".into()));
        }
        let hello = Hello {
            protocol: PROTOCOL,
            build: build_id().to_vec(),
            // TODO(pe-diverge): the physics self-test's digest and time, once the worker runs it (app.md 1).
            selftest: [0; 32],
            selftest_unix_s: 0,
            line: pre_genesis_line(),
            token: Vec::new(),
            addrs: addrs.clone(),
            clock_ready: member,
        };
        let identity = Identity::from_seed(&seed);
        let me = MemberKey(identity.public());
        if me == DEV_MEMBER {
            return Err(NetError::Core("the identity is the development key".into()));
        }
        let mut quinn_seed = [0u8; 32];
        pe_net_crypto::random::os().fill(&mut quinn_seed).map_err(|_| NetError::Core("no randomness".into()))?;
        let mut cfg = Config::new(hello, member);
        tweak(&mut cfg);
        let core = Core::new(cfg, identity, quinn_seed).map_err(|e| match e {
            CoreError::Hello(e) => NetError::Hello(e),
            e => NetError::Core(format!("{e:?}")),
        })?;
        let handle = NetHandle::spawn_with(core, socket, host.clone(), QUEUE, arrival)?;
        Ok(Endpoint { seed, me, handle, addrs, local, host, welcome: None })
    }

    pub fn me(&self) -> MemberKey {
        self.me
    }

    /// The address the socket is bound to (with `--listen 0.0.0.0:4433`, that one, not an advertised one).
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// A's invite: its key and addresses (transport.md 2.4), valid for `valid_for`.
    pub fn invite(&self, valid_for: Duration) -> Result<Invite, NetError> {
        let mut id = [0u8; 8];
        pe_net_crypto::random::os().fill(&mut id).map_err(|_| NetError::Core("no randomness".into()))?;
        Ok(Invite {
            id,
            key: self.me.0,
            expiry_unix_s: unix_now().saturating_add(valid_for.as_secs()),
            addrs: self.addrs.clone(),
        })
    }

    /// B's reply to `invite`: its key and addresses, which A takes so the session can name B (the interim).
    pub fn reply(&self, invite: &Invite) -> Reply {
        Reply { invite_id: invite.id, key: self.me.0, addrs: self.addrs.clone() }
    }

    /// B: dial the invite's addresses (the app checks the expiry).
    pub fn dial(&mut self, invite: &Invite) -> Result<(), NetError> {
        if invite.expiry_unix_s < unix_now() {
            return Err(NetError::Refused("the invite has expired".into()));
        }
        self.command(Command::Dial(invite.clone()))
    }

    fn command(&mut self, c: Command) -> Result<(), NetError> {
        if self.handle.command(c) { Ok(()) } else { Err(NetError::Core("pe-net's command queue is full".into())) }
    }

    /// A, with B's reply to its own `invite`: dial B, and when B's join request comes, start frame 0 `epoch_lead`
    /// ahead, build genesis with both keys and admit B (B's own early dial may have timed out meanwhile). Waits up
    /// to `timeout` for B's link to come up.
    pub fn open_session(
        mut self,
        invite: &Invite,
        reply: &Reply,
        p: &OpenParams,
        timeout: Duration,
        runner: Box<dyn ScriptRunner>,
    ) -> Result<Session, NetError> {
        let them = MemberKey(reply.key);
        if them == self.me || them == DEV_MEMBER {
            return Err(NetError::Refused("the reply names this machine's own key, or the development key".into()));
        }
        // A reply to another invite (an earlier run's, say) would have A dial a stale key and wait out `timeout`.
        if reply.invite_id != invite.id || invite.key != self.me.0 {
            return Err(NetError::Refused("the reply answers another invite".into()));
        }
        if invite.expiry_unix_s < unix_now() {
            return Err(NetError::Refused("the invite has expired".into()));
        }
        self.command(Command::TakeReply(reply.clone()))?;
        let deadline = Instant::now() + timeout;
        let mut built: Option<Built> = None;
        // Genesis is built once, on B's first join request: the runner goes to that replica.
        let mut runner = Some(runner);
        let mut up = false;
        while !up {
            if Instant::now() >= deadline {
                return Err(NetError::Timeout(if built.is_some() {
                    "the second machine's link"
                } else {
                    "its join request"
                }));
            }
            for e in self.handle.events() {
                match e {
                    // Every join request from B is admitted: one may come from a connection already closed (B's
                    // early dial, at `T_welcome`), where pe-net's admit does nothing, and the next from A's own dial.
                    NetEvent::JoinRequest { peer, .. } if peer == them => {
                        if built.is_none() {
                            built = Some(self.genesis_for(them, p, runner.take().expect("built once"))?);
                        }
                        let welcome = built.as_ref().map(|b| b.welcome.clone()).expect("built above");
                        self.command(Command::Admit { peer, welcome })?;
                    }
                    NetEvent::JoinRequest { peer, .. } => {
                        self.command(Command::Refuse { peer, refusal: not_admitted(peer) })?;
                    }
                    NetEvent::LinkUp { peer } if peer == them && built.is_some() => up = true,
                    _ => {}
                }
            }
            while let Some(l) = self.handle.poll_lifecycle() {
                log_lifecycle(&l);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let b = built.expect("up only after admitting");
        self.welcome = Some(b.welcome);
        self.into_session(b.members, b.replica, b.cfg, b.frames, b.agree)
    }

    /// A, on B's first join request (@engine-architect's L1): frame 0 is `epoch_lead` from now, so the lead covers
    /// only the welcome and B's clock convergence, however long the reply took to paste. Building 500 balls takes
    /// well under pe-net's `T_welcome` (10 s).
    fn genesis_for(
        &mut self,
        them: MemberKey,
        p: &OpenParams,
        runner: Box<dyn ScriptRunner>,
    ) -> Result<Built, NetError> {
        let frames: Arc<dyn FrameClock> = self.handle.frame_clock.clone();
        // The first member's session time is its host time (transport.md 5.4).
        let epoch_ns = (self.host)().plus(p.epoch_lead).0;
        let params = Params {
            nonce: fresh_nonce().map_err(NetError::Genesis)?,
            build: build_id().to_vec(),
            opener: self.me,
            joiner: them,
            rate: p.rate,
            gravity: pe_world::GRAVITY,
            seed: p.seed,
            balls: p.balls,
            origin: pe_seams::Pos::ORIGIN,
            epoch_ns,
        };
        // The clock first: genesis refuses a frame clock that doesn't run at the record's rate.
        self.command(Command::StartFirstMember { epoch_ns, rate: p.rate })?;
        let started = Instant::now() + Duration::from_secs(2);
        while frames.rate_hz() != p.rate.hz() {
            if Instant::now() >= started {
                return Err(NetError::Timeout("the clock to start"));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let (genesis, replica, cfg) = Genesis::ball_pit(&params, frames.clone(), runner).map_err(NetError::Genesis)?;
        let members: Vec<MemberKey> = genesis.members.iter().map(|m| m.0).collect();
        self.command(Command::SetClockMembers(others(&members, self.me)))?;
        let welcome = Welcome {
            // pe-net's welcome must name the hellos' line (transport.md 6.4); the session's own line is the record's,
            // which B derives itself (module docs).
            line: pre_genesis_line(),
            epoch_ns,
            rate_hz: p.rate.hz(),
            members: vec![(self.me, self.addrs.clone())],
            agree: genesis.encode(),
        };
        Ok(Built { welcome, members, replica, cfg, frames, agree: genesis.agree })
    }

    /// B, after dialling `invite`: wait up to `timeout` for the welcome, check it against the invite, this build and
    /// `rate_pin` (the coordinator's welcome-rate rule), and rebuild genesis.
    pub fn join_session(
        mut self,
        invite: &Invite,
        rate_pin: SimRate,
        timeout: Duration,
        runner: Box<dyn ScriptRunner>,
    ) -> Result<Session, NetError> {
        let opener = MemberKey(invite.key);
        let deadline = Instant::now() + timeout;
        let mut clock_ready = false;
        let welcome = loop {
            if Instant::now() >= deadline {
                return Err(NetError::Timeout("the welcome"));
            }
            let _ = self.handle.events();
            let mut got = None;
            while let Some(l) = self.handle.poll_lifecycle() {
                match l {
                    Lifecycle::Welcomed { peer, welcome } if peer == opener => got = Some(welcome),
                    // Convergence on the welcome's members only: the clock begins joining at the welcome.
                    Lifecycle::ClockReady if got.is_some() => clock_ready = true,
                    // A refusal names its reason. A close for want of a decision (`T_welcome`, code 8) is not
                    // final: A dials again when it takes the reply.
                    Lifecycle::Refused { refusal, by_me, .. } if !by_me && refusal.code != CloseCode::Overloaded => {
                        return Err(NetError::Refused(format!("{:?}: {}", refusal.code, refusal.detail)));
                    }
                    other => log_lifecycle(&other),
                }
            }
            if let Some(w) = got {
                break w;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        let genesis = match check_welcome(&welcome, self.me, opener, rate_pin) {
            Ok(g) => g,
            Err(e) => return self.fail(e),
        };
        self.welcome = Some(welcome);
        let frames: Arc<dyn FrameClock> = self.handle.frame_clock.clone();
        // The converged clock first: the epoch's bound needs it (L3), and genesis refuses a frame clock that doesn't
        // run at the record's rate.
        while !clock_ready {
            if Instant::now() >= deadline {
                return self.fail(NetError::Timeout("the clock to converge"));
            }
            let _ = self.handle.events();
            while let Some(l) = self.handle.poll_lifecycle() {
                match l {
                    Lifecycle::ClockReady => clock_ready = true,
                    other => log_lifecycle(&other),
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if let Err(e) = check_epoch(frames.host_time_of(pe_seams::Frame(0)), (self.host)()) {
            return self.fail(e);
        }
        let (replica, cfg) = match genesis.build(self.me, frames.clone(), runner) {
            Ok(r) => r,
            Err(e) => return self.fail(NetError::Genesis(e)),
        };
        let members: Vec<MemberKey> = genesis.members.iter().map(|m| m.0).collect();
        self.command(Command::SetMember(true))?;
        self.command(Command::SetClockMembers(others(&members, self.me)))?;
        self.into_session(members, replica, cfg, frames, genesis.agree)
    }

    fn into_session(
        mut self,
        members: Vec<MemberKey>,
        replica: Box<dyn Replica>,
        cfg: pe_agree::NodeConfig,
        frames: Arc<dyn FrameClock>,
        agree: pe_seams::Hash,
    ) -> Result<Session, NetError> {
        // The configuration pe-agree runs must still be the one whose hash the record names (genesis's `start`
        // compared them): a must-match setting changed on the way here would refuse the other member's entries, votes
        // or cuts later (@design-reviewer's Low on m1/consensus-check).
        let mine = cfg.consensus_hash();
        if mine != agree {
            return self.fail(NetError::Genesis(GenesisError::Agree { record: agree, mine }));
        }
        let control = self.handle.control()?.ok_or_else(|| NetError::Core("the control end was taken".into()))?;
        let node = Node::new(cfg, Box::new(Identity::from_seed(&self.seed)), Box::new(StrictVerifier), (self.host)());
        let welcome = self.welcome.take().ok_or_else(|| NetError::Core("no welcome".into()))?;
        Ok(Session {
            me: self.me,
            hook: MemberHook::new(control, members.clone(), welcome),
            members,
            handle: self.handle,
            replica,
            node,
            frames,
        })
    }

    /// Stop pe-net's thread (a session that didn't start).
    pub fn stop(self) -> io::Result<()> {
        self.handle.stop()
    }

    /// A session that didn't start: close the connection and stop the thread, keeping the reason.
    fn fail(self, e: NetError) -> Result<Session, NetError> {
        let _ = self.handle.stop();
        Err(e)
    }
}

/// B's checks on a welcome, in order, each naming what differs (the coordinator's "verified, not trusted").
pub fn check_welcome(w: &Welcome, me: MemberKey, opener: MemberKey, rate_pin: SimRate) -> Result<Genesis, NetError> {
    if w.line != pre_genesis_line() {
        return Err(NetError::Welcome("its line is not the interim's pre-genesis line".into()));
    }
    // The session's line is the record's own (`Genesis::line`), derived here, never taken from the welcome: every
    // pe-agree signature commits to it, so a record A didn't send can't yield signatures B accepts.
    let genesis = Genesis::decode(&w.agree).map_err(NetError::Genesis)?;
    genesis.check(me, opener, build_id()).map_err(NetError::Genesis)?;
    if w.epoch_ns != genesis.epoch_ns {
        return Err(NetError::Welcome(format!("epoch {} against the record's {}", w.epoch_ns, genesis.epoch_ns)));
    }
    if w.rate_hz != genesis.rate.hz() || w.rate_hz != rate_pin.hz() {
        return Err(NetError::Welcome(format!(
            "rate {} Hz (record {} Hz) against this machine's {} Hz",
            w.rate_hz,
            genesis.rate.hz(),
            rate_pin.hz()
        )));
    }
    Ok(genesis)
}

/// The members a clock samples and whose rendezvous it takes: every member but this machine (transport.md 5.3, 6.7).
fn others(members: &[MemberKey], me: MemberKey) -> Vec<MemberKey> {
    members.iter().copied().filter(|m| *m != me).collect()
}

fn not_admitted(peer: MemberKey) -> Refusal {
    let why = if peer == DEV_MEMBER { "the development key" } else { "this session's membership is fixed (M1)" };
    Refusal { code: CloseCode::NotAdmitted, detail: format!("not admitted: {why}") }
}

fn log_lifecycle(l: &Lifecycle) {
    match l {
        Lifecycle::Fault { peer, what } => eprintln!("net: fault from {}: {what}", short(peer)),
        Lifecycle::Refused { peer, refusal, by_me } => {
            eprintln!(
                "net: {} {}: {:?} {}",
                if *by_me { "refused" } else { "refused by" },
                short(peer),
                refusal.code,
                refusal.detail
            )
        }
        Lifecycle::ClockFault(f) => eprintln!("net: clock fault {f:?}"),
        Lifecycle::ClockReady | Lifecycle::Welcomed { .. } => {}
    }
}

fn short(k: &MemberKey) -> String {
    k.0[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// The simulation's `NetHook` (app.md 3): it owns pe-net's control end. With M1's fixed membership it refuses every
/// join request, "not admitted", and drains the lifecycle's outcomes once a tick, logging faults and refusals.
pub struct MemberHook {
    control: NetControl,
    members: Vec<MemberKey>,
    welcome: Welcome,
    /// Join requests refused, and faults seen.
    pub refused: u64,
    pub faults: u64,
}

impl MemberHook {
    pub fn new(control: NetControl, members: Vec<MemberKey>, welcome: Welcome) -> MemberHook {
        MemberHook { control, members, welcome, refused: 0, faults: 0 }
    }
}

impl NetHook for MemberHook {
    fn on_event(&mut self, event: &NetEvent, _agree: &mut dyn Agreement) {
        match event {
            NetEvent::JoinRequest { peer, .. } => {
                // A member reconnecting after a drop isn't a join: pe-net admits a known member's new connection the
                // same way, so a member gets a welcome; anyone else is refused.
                let key = *peer;
                let ok = if self.members.contains(&key) && key != DEV_MEMBER {
                    self.control.command(Command::Admit { peer: *peer, welcome: self.welcome.clone() })
                } else {
                    self.refused += 1;
                    self.control.command(Command::Refuse { peer: *peer, refusal: not_admitted(key) })
                };
                if !ok {
                    eprintln!("net: the control queue is full");
                }
            }
            NetEvent::LinkDown { peer } => eprintln!("net: link to {} down", short(peer)),
            NetEvent::LinkUp { peer } => eprintln!("net: link to {} up", short(peer)),
            _ => {}
        }
    }

    fn on_tick(&mut self, _now: HostTime, _agree: &mut dyn Agreement) {
        while let Some(l) = self.control.poll_lifecycle() {
            if matches!(l, Lifecycle::Fault { .. }) {
                self.faults += 1;
            }
            log_lifecycle(&l);
        }
    }
}

/// The handle, as the simulation's transport.
pub fn transport(handle: NetHandle) -> Box<dyn Transport + Send> {
    Box::new(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leave::LeaveStatus;
    use crate::sim::{Budget, Sim};
    use pe_render::present::FrameSlot;
    use pe_seams::{Hand, HandFeed, HandInput, HandSource, RigPose};

    /// pe-logic's runner, as the binary hands it to each replica it builds.
    fn runner() -> Box<dyn ScriptRunner> {
        let (settings, run) = runner_settings();
        Box::new(pe_replica::logic::LogicRunner::new(settings, run).expect("a runner"))
    }

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

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    const RATE: SimRate = SimRate::DEFAULT;

    /// This build's id fits the hello's bound, or no peer could decode its hello (@iter-bench's finding).
    #[test]
    fn the_build_id_fits_the_hello() {
        assert!(build_id().len() <= pe_net::lifecycle::MAX_BUILD, "{} bytes", build_id().len());
    }

    /// A build id over the hello's bound is refused by `bind` with a typed error, never sent to time out.
    #[test]
    fn bind_refuses_a_build_id_over_the_hello_s_bound() {
        let long = vec![b'x'; pe_net::lifecycle::MAX_BUILD + 1];
        let r = Endpoint::bind_with([21; 32], loopback(), Vec::new(), true, crate::clock::shared(), None, |c| {
            c.hello.build = long
        });
        assert!(matches!(r, Err(NetError::Hello(HelloError::TooLong { field: "build id", .. }))), "{:?}", r.err());
    }

    fn params() -> OpenParams {
        OpenParams { seed: 3, balls: 8, rate: RATE, epoch_lead: Duration::from_millis(1500) }
    }

    /// One machine of [`joined_pair`]: its simulation, frame slot, member and clock.
    type Machine = (Sim, Arc<FrameSlot>, MemberKey, Arc<dyn FrameClock>);

    /// The M1 interim's two machines over real UDP on loopback: A opens and invites, B replies and dials, A takes the
    /// reply and admits B with the genesis record, and B checks and rebuilds it. Each is a `Sim` with still hands at
    /// its feed, on pe-agree over pe-net: A first, then B.
    fn joined_pair(host: &crate::clock::HostClock) -> (Vec<Machine>, [[f32; 3]; 2]) {
        let a = Endpoint::bind([21; 32], loopback(), Vec::new(), true, host.clone(), None).unwrap();
        let mut b = Endpoint::bind([22; 32], loopback(), Vec::new(), false, host.clone(), None).unwrap();
        let invite = a.invite(Duration::from_secs(600)).unwrap();
        let reply = b.reply(&invite);
        b.dial(&invite).unwrap();
        let theirs = invite.clone();
        let joiner = std::thread::spawn(move || b.join_session(&theirs, RATE, Duration::from_secs(10), runner()));
        let sa = a.open_session(&invite, &reply, &params(), Duration::from_secs(10), runner()).expect("A's session");
        let sb = joiner.join().unwrap().expect("B's session");
        assert_eq!(sa.members, sb.members);
        assert_eq!(sa.frames.rate_hz(), sb.frames.rate_hz());
        let feeds = [[-0.5f32, 1.0, -0.4], [0.5, 1.1, -0.3]];
        let mut sims = Vec::new();
        for (s, feed) in [sa, sb].into_iter().zip(feeds) {
            let slot = Arc::new(FrameSlot::new());
            let me = s.me;
            let sim = Sim::new(Box::new(s.node), s.replica, s.frames.clone(), slot.clone(), Budget::new(RATE))
                .with_local(me, crate::local::chain_nonce(), Arc::new(StillHands(feed)))
                .with_transport(transport(s.handle), Some(Box::new(s.hook)));
            sims.push((sim, slot, me, s.frames));
        }
        (sims, feeds)
    }

    /// The graceful quit over real UDP on loopback (app.md 4; @design-reviewer's Low 2 on the leave, which the
    /// in-process hub couldn't show). After 2 s together B leaves. The remaining member keeps its link to B after the
    /// epoch switch, and its confirmation reaches B over pe-net, so B's leave settles within the longest bound. Then B
    /// stops, and A goes on alone: its epoch is A alone, B's hands are gone, and its final frame keeps moving.
    #[test]
    #[cfg_attr(
        debug_assertions,
        ignore = "release only, as a two-machine run (@coordinator-opus mce61106b6b96): run with --release"
    )]
    fn a_member_that_leaves_over_the_network_settles_and_the_other_goes_on() {
        let host = crate::clock::shared();
        let (mut sims, _) = joined_pair(&host);
        let run_for = |sims: &mut Vec<Machine>, d: Duration| {
            let end = Instant::now() + d;
            while Instant::now() < end {
                for (sim, ..) in sims.iter_mut() {
                    let _ = sim.tick(host());
                }
                std::thread::sleep(Duration::from_millis(4));
            }
        };
        run_for(&mut sims, Duration::from_millis(1500 + 2500));
        sims[1].0.leave();
        let (asked, b) = (Instant::now(), sims[1].2);
        while sims[1].0.leave_status() != LeaveStatus::Settled {
            let status = sims[1].0.leave_status();
            assert!(
                asked.elapsed() < crate::leave::MAX_WAIT,
                "B's leave didn't settle: {status:?}, {:?}",
                sims[1].0.stats()
            );
            for (sim, ..) in sims.iter_mut() {
                let _ = sim.tick(host());
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        println!("B's leave settled over the network after {:?}", asked.elapsed());
        drop(sims.pop());
        let before = sims[0].1.newest().and_then(|v| v.final_frame).expect("A has a final frame");
        run_for(&mut sims, Duration::from_secs(2));
        let (sim, slot, ..) = &sims[0];
        assert_eq!(sim.other_members(), Vec::<MemberKey>::new(), "A's epoch is A alone");
        let v = slot.newest().expect("a view");
        assert!(v.objects.iter().all(|o| o.hand_of.is_none_or(|(m, _)| m != b)), "B's hands are gone");
        let after = v.final_frame.expect("A's final frame");
        assert!(after.0 >= before.0 + 30, "A finalizes alone: final {before:?} then {after:?}");
    }

    #[test]
    #[cfg_attr(
        debug_assertions,
        ignore = "release only, as a two-machine run (@coordinator-opus mce61106b6b96): it passes in debug since the \
                  dev profile optimizes dependencies (a tick's fixed cost 0.5 ms, from 50-200 ms with them at \
                  opt-level 0), but timing assertions and two-machine runs stay in release: run with --release"
    )]
    fn a_second_machine_joins_from_an_invite_and_both_finalize() {
        // The M1 interim end to end over real UDP on loopback: A opens and invites, B replies and dials, A takes the
        // reply and admits B with the genesis record, B checks and rebuilds it, and both simulate the ball pit on
        // pe-agree over pe-net: each sees the other's hands, and cuts become final.
        let host = crate::clock::shared();
        let (mut sims, feeds) = joined_pair(&host);
        // Run both in real time to 2 s after frame 0.
        let end = Instant::now() + Duration::from_millis(1500 + 2500);
        while Instant::now() < end {
            for (sim, ..) in &mut sims {
                let _ = sim.tick(host());
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        for (i, (sim, slot, _, frames)) in sims.iter().enumerate() {
            let v = slot.newest();
            // A frame period over the measured cost of a frame is stepping's margin over real time (sim.rs `Budget`).
            // Both machines step on this one thread, so here the margin each one has is half of it.
            let cost = sim.budget().cost();
            let period = Duration::from_nanos(1_000_000_000 / u64::from(RATE.hz()));
            println!(
                "machine {i}: now {:?}, view {:?}, final {:?}; frame cost {cost:?}, margin {:.2}, budget {} frames; {:?}",
                frames.frame_at(host()),
                v.as_ref().map(|v| v.frame),
                v.as_ref().and_then(|v| v.final_frame),
                cost.map_or(0.0, |c| period.as_secs_f64() / c.as_secs_f64().max(1e-9)),
                sim.budget().frames(),
                sim.stats()
            );
        }
        for i in 0..2 {
            let (them, feed) = (sims[1 - i].2, feeds[1 - i]);
            let (sim, slot, _, frames) = &sims[i];
            let v = slot.newest().expect("a view");
            let o = v.objects.iter().find(|o| o.hand_of == Some((them, Hand::Right))).expect("their right hand");
            let want = pe_seams::Pos::ORIGIN.checked_add(pe_seams::Offset([
                f64::from(feed[0]) + 0.2,
                f64::from(feed[1]),
                f64::from(feed[2]),
            ]));
            let d = (o.pos - want.unwrap()).0;
            assert!(d.iter().all(|c| c.abs() < 1e-3), "machine {i} sees the other's right hand {d:?} off");
            let st = sim.stats();
            let now = frames.frame_at(host()).expect("past frame 0");
            println!("machine {i}: frame {:?}, final {:?}, now {now:?}; {st:?}", v.frame, v.final_frame);
            assert!(st.finalized > 0, "machine {i}: nothing final: {st:?}");
            assert_eq!((st.refused, st.unsent), (0, 0), "machine {i}: {st:?}");
        }
    }

    fn a_welcome() -> (Welcome, MemberKey, MemberKey) {
        let (me, opener) = (MemberKey([2; 32]), MemberKey([1; 32]));
        let host = crate::clock::shared();
        let frames: Arc<dyn FrameClock> = Arc::new(crate::clock::LocalClock::new(host(), RATE));
        let p = Params {
            nonce: [7; 32],
            build: build_id().to_vec(),
            opener,
            joiner: me,
            rate: RATE,
            gravity: pe_world::GRAVITY,
            seed: 3,
            balls: 8,
            origin: pe_seams::Pos::ORIGIN,
            epoch_ns: 123,
        };
        let (g, _, _) = Genesis::ball_pit(&p, frames, runner()).unwrap();
        let w = Welcome {
            line: pre_genesis_line(),
            epoch_ns: 123,
            rate_hz: RATE.hz(),
            members: vec![(opener, Vec::new())],
            agree: g.encode(),
        };
        (w, me, opener)
    }

    #[test]
    fn a_welcome_that_differs_from_its_record_or_this_machine_is_refused_naming_what() {
        let (w, me, opener) = a_welcome();
        assert!(check_welcome(&w, me, opener, RATE).is_ok());
        let refused = |w: &Welcome, me, opener, rate| match check_welcome(w, me, opener, rate) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("accepted"),
        };
        let mut x = w.clone();
        x.line[0] ^= 1;
        assert!(refused(&x, me, opener, RATE).contains("line"));
        let mut x = w.clone();
        x.epoch_ns += 1;
        assert!(refused(&x, me, opener, RATE).contains("epoch"));
        let mut x = w.clone();
        x.rate_hz = 90;
        assert!(refused(&x, me, opener, RATE).contains("rate"));
        assert!(refused(&w, me, opener, SimRate::new(90).unwrap()).contains("rate"), "the joiner's own pin");
        assert!(refused(&w, MemberKey([3; 32]), opener, RATE).contains("not the joiner"), "not me");
        assert!(refused(&w, me, MemberKey([3; 32]), RATE).contains("not the invite's key"));
        let mut x = w.clone();
        x.agree.push(0);
        assert!(refused(&x, me, opener, RATE).contains("not a genesis record"));
    }

    #[test]
    fn the_epoch_must_be_within_its_bound_once_the_clock_converges() {
        let now = HostTime(1_000_000_000_000);
        let s = |x: f64| HostTime(now.0 + (x * 1e9) as i64);
        assert!(check_epoch(s(2.0), now).is_ok(), "A's lead");
        assert!(check_epoch(s(-19.9), now).is_ok(), "a redial and convergence outlasted the lead");
        assert!(check_epoch(s(-20.1), now).unwrap_err().to_string().contains("ago"), "stale or replayed");
        assert!(check_epoch(s(10.1), now).unwrap_err().to_string().contains("ahead"));
        // An extreme epoch is refused, never an overflow (@engine-architect's L2).
        assert!(check_epoch(HostTime(i64::MAX), HostTime(i64::MIN)).unwrap_err().to_string().contains("ahead"));
        assert!(check_epoch(HostTime(i64::MIN), HostTime(i64::MAX)).unwrap_err().to_string().contains("ago"));
    }

    #[test]
    fn a_reply_to_another_invite_is_refused_before_any_dial() {
        let host = crate::clock::shared();
        let a = Endpoint::bind([21; 32], loopback(), Vec::new(), true, host.clone(), None).unwrap();
        let b = Endpoint::bind([22; 32], loopback(), Vec::new(), false, host, None).unwrap();
        let invite = a.invite(Duration::from_secs(600)).unwrap();
        let mut old = invite.clone();
        old.id[0] ^= 1;
        let stale = b.reply(&old);
        match a.open_session(&invite, &stale, &params(), Duration::from_secs(5), runner()) {
            Err(e) => assert!(e.to_string().contains("another invite"), "{e}"),
            Ok(_) => panic!("a stale reply opened a session"),
        }
    }

    #[test]
    fn a_reply_pasted_after_the_joiner_s_first_dial_closed_still_opens() {
        // The slow paste (@engine-architect's L5): B dials as soon as it prints its reply; A takes no decision before
        // `T_welcome` and closes that connection (code 8, which B waits through); A's events still hold the closed
        // connection's join request. Then A takes the reply, dials B, and admits the new connection.
        let host = crate::clock::shared();
        let t_welcome = Duration::from_millis(300);
        let a = Endpoint::bind_with([21; 32], loopback(), Vec::new(), true, host.clone(), None, |c| {
            c.t_welcome = t_welcome
        })
        .unwrap();
        let mut b = Endpoint::bind([22; 32], loopback(), Vec::new(), false, host.clone(), None).unwrap();
        let invite = a.invite(Duration::from_secs(600)).unwrap();
        let reply = b.reply(&invite);
        b.dial(&invite).unwrap();
        let theirs = invite.clone();
        let joiner = std::thread::spawn(move || b.join_session(&theirs, RATE, Duration::from_secs(15), runner()));
        std::thread::sleep(t_welcome * 3);
        let sa = a.open_session(&invite, &reply, &params(), Duration::from_secs(10), runner()).expect("A's session");
        let sb = joiner.join().unwrap().expect("B's session");
        assert_eq!(sa.members, sb.members);
    }

    #[test]
    fn the_identity_seed_is_made_once_then_read_back() {
        let dir = std::env::temp_dir().join(format!("pe-app-net-test-{}", std::process::id()));
        let path = dir.join("identity.seed");
        let _ = std::fs::remove_file(&path);
        let a = identity_seed(&path).unwrap();
        assert_eq!(identity_seed(&path).unwrap(), a);
        std::fs::write(&path, "not a seed").unwrap();
        assert!(identity_seed(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hands that move: both tracked, swaying 10 cm at 0.5 Hz, so every frame makes two hand entries.
    struct MovingHands([f32; 3]);

    impl HandFeed for MovingHands {
        fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput> {
            let x = if hand == Hand::Left { -0.2 } else { 0.2 };
            let sway = 0.1 * (at.0 as f64 * 1e-9 * std::f64::consts::PI).sin() as f32;
            let mut h = StillHands([self.0[0] + sway, self.0[1], self.0[2]]).hand_at(hand, at)?;
            h.pose.position[0] = self.0[0] + sway + x;
            Some(h)
        }
    }

    /// A UDP relay standing in for one machine's address: what a peer sends it goes to the machine, what the machine
    /// sends back goes to the last peer heard. `dark` drops everything both ways, a blackout of the path.
    struct Relay {
        stop: Arc<std::sync::atomic::AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl Relay {
        fn start_on(bind: SocketAddr, machine: SocketAddr, dark: Arc<std::sync::atomic::AtomicBool>) -> Relay {
            use std::sync::atomic::Ordering;
            let socket = UdpSocket::bind(bind).unwrap();
            socket.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let halt = stop.clone();
            let join = std::thread::spawn(move || {
                let mut buf = vec![0u8; 65_536];
                let mut peer: Option<SocketAddr> = None;
                while !halt.load(Ordering::Relaxed) {
                    let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
                    if dark.load(Ordering::Relaxed) {
                        continue;
                    }
                    let to = if from == machine {
                        let Some(p) = peer else { continue };
                        p
                    } else {
                        peer = Some(from);
                        machine
                    };
                    let _ = socket.send_to(&buf[..n], to);
                }
            });
            Relay { stop, join: Some(join) }
        }
    }

    impl Drop for Relay {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    type Machines = [(Sim, MemberKey)];

    #[test]
    #[ignore = "16-25 s in release, up to about 70 s if it fails (debug simulates too slowly to finalize in time): run with --release --include-ignored"]
    fn a_ten_second_blackout_resumes_and_every_entry_becomes_final() {
        // @coordinator-opus m47dee2c21d3a / mee4f40bff810, @design-reviewer m1631c628fe4b, @world-chunks: two
        // machines with hands moving (two entries a frame) lose their path for 10 s, under T_IDLE_M1 (30 s). After
        // it, the session resumes on the same connections: finality comes back within two of the view timeout's
        // ceilings plus a margin (the view running when the path returns may wait out one ceiling, and the next one
        // may time out once more; @engine-architect measured 3.9-13.3 s and once past 15 s under load, @coordinator-opus
        // me1c7fc4e8bf5), every entry either member made becomes final, and neither side refuses or blames the
        // other (no Refused or Fault naming it, no message refused). Each member also queues an edit through pe-api's
        // port 3 s into the blackout, so an edit sits in each outage backlog: no entry of either is voided as late
        // (pe-agree's outage rule, gate item 4; @coordinator-opus md904d9b1a9c7), and both edits reach the final order
        // on both machines.
        use crate::api::Ports;
        use pe_api::queue::{Receipt, Submission};
        use pe_seams::world_api::{Status, Wait};
        use pe_seams::{AgreeEvent, VoidReason};
        use pe_world::edit::{Actor, Op, Target, Transaction};
        use std::sync::atomic::Ordering;
        const BLACKOUT: Duration = Duration::from_secs(10);
        const EDIT_AT: Duration = Duration::from_secs(3);
        let ceiling = Duration::from_nanos(crate::genesis::settings().timing.max_timeout_ns as u64);
        let resume_within = 2 * ceiling + Duration::from_secs(5);
        let host = crate::clock::shared();
        let DarkPair { mut sims, logs, ports, clocks, dark, relays } = dark_pair(&host);
        let run = |sims: &mut Vec<(Sim, MemberKey)>, until: &mut dyn FnMut(&Machines) -> bool, max: Duration| {
            run_pair(&host, sims, until, max)
        };
        // Both finalize before the blackout.
        assert!(
            run(&mut sims, &mut |s| s.iter().all(|(sim, _)| sim.stats().finalized > 0), Duration::from_secs(20)),
            "nothing final before the blackout: {:?}",
            sims.iter().map(|(s, _)| s.stats().clone()).collect::<Vec<_>>()
        );
        dark.store(true, Ordering::Relaxed);
        let dark_since = Instant::now();
        run(&mut sims, &mut |_| false, EDIT_AT);
        let tickets: Vec<_> = ports
            .iter()
            .zip(&clocks)
            .map(|(p, clock)| {
                let create = Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![] };
                let tx = Transaction {
                    inline: vec![],
                    actor: Actor::Person,
                    label: None,
                    preconditions: vec![],
                    ops: vec![create],
                    blobs: vec![],
                };
                let base = clock.frame_at(host()).expect("a frame").0;
                p.queue.submit(Submission { tx, base }).expect("room in pe-api's queue")
            })
            .collect();
        let mut receipts: Vec<Option<Receipt>> = vec![None, None];
        run(
            &mut sims,
            &mut |_| {
                for (i, r) in receipts.iter_mut().enumerate() {
                    if r.is_none() {
                        *r = ports[i].queue.receipt(tickets[i], Instant::now());
                    }
                }
                receipts.iter().all(Option::is_some)
            },
            Duration::from_secs(2),
        );
        let edits: Vec<_> = receipts
            .into_iter()
            .enumerate()
            .map(|(i, r)| match r {
                Some(Receipt::Submitted { change, .. }) => change,
                other => panic!("machine {i}'s edit was not submitted during the blackout: {other:?}"),
            })
            .collect();
        run(&mut sims, &mut |_| false, BLACKOUT.saturating_sub(dark_since.elapsed()));
        dark.store(false, Ordering::Relaxed);
        let back = Instant::now();
        // Counted from the path's return, not from before the blackout: a cut voted just before it went dark can
        // finalize during the blackout, and would count as resumed at once.
        let before: Vec<u64> = sims.iter().map(|(s, _)| s.stats().finalized).collect();
        // Every entry made up to the blackout's end, both chains, must become final on both sides.
        let heads: Vec<_> = sims.iter().map(|(s, _)| s.local_head().expect("a local chain")).collect();
        let made: Vec<u64> = sims.iter().map(|(s, _)| s.stats().inputs).collect();
        let resumed = run(
            &mut sims,
            &mut |s| s.iter().enumerate().all(|(i, (sim, _))| sim.stats().finalized > before[i]),
            resume_within,
        );
        let resumed_after = back.elapsed();
        let all_final = run(
            &mut sims,
            &mut |s| {
                s.iter().all(|(sim, _)| {
                    sim.final_cut().is_some_and(|c| heads.iter().all(|(chain, seq)| c.includes(chain, *seq)))
                })
            },
            Duration::from_secs(30),
        );
        let all_final_after = back.elapsed();
        // Both edits' fates on both machines. In M1 a machine decides finality only for its own changes (api.md 5.4,
        // `mine`), so its own edit must settle as final, and the other's must be stepped here; the final cuts above
        // include both, and the Late count below shows neither was voided.
        let fate = |p: &Ports, c| p.log.wait(c, Wait::Final, Instant::now());
        let settled = |s: Status, is_final: bool| {
            matches!(s, Status::Final | Status::Dropped | Status::Unknown) || (s == Status::Refused && is_final)
        };
        let done = |i: usize, j: usize| {
            fate(&ports[i], edits[j]).is_some_and(|f| settled(f.status, f.is_final) || (i != j && f.frame.is_some()))
        };
        run(&mut sims, &mut |_| (0..2).all(|i| (0..2).all(|j| done(i, j))), Duration::from_secs(10));
        println!(
            "after the path came back: finality resumed {resumed} ({resumed_after:?}); every entry final {all_final} ({all_final_after:?})"
        );
        for (i, p) in ports.iter().enumerate() {
            for (j, c) in edits.iter().enumerate() {
                let f = fate(p, *c);
                println!("machine {i}: machine {j}'s edit {:?}", f.as_ref().map(|f| (f.status, f.frame, &f.outcome)));
                // Unknown: in the final order, but a repair replaced the state it was stepped on here (api.md 5.4).
                let expected: &[Status] =
                    if i == j { &[Status::Final, Status::Unknown] } else { &[Status::Accepted, Status::Unknown] };
                assert!(
                    f.as_ref().is_some_and(|f| expected.contains(&f.status)),
                    "machine {i}: machine {j}'s edit, queued during the blackout, is not {expected:?}: {f:?}"
                );
                if let Some(f) = f.filter(|f| f.status != Status::Unknown) {
                    assert!(
                        matches!(f.outcome, Some(pe_seams::Outcome::Applied { .. })),
                        "machine {i}: machine {j}'s edit was not applied: {f:?}"
                    );
                }
            }
        }
        for (i, (sim, me)) in sims.iter().enumerate() {
            let them = sims[1 - i].1;
            let log = logs[i].lock().unwrap();
            let blamed: Vec<&AgreeEvent> = log
                .iter()
                .filter(|e| match e {
                    AgreeEvent::Refused { author, .. } => *author == them,
                    AgreeEvent::Fault(f) => f.from == them || f.author == Some(them),
                    _ => false,
                })
                .collect();
            // Under pe-agree's outage rule an entry made during the outage is delayed, never voided as late.
            let late = log
                .iter()
                .filter(|e| matches!(e, AgreeEvent::Voided { author, reason: VoidReason::Late, .. } if *author == them))
                .count();
            println!(
                "machine {i} ({}): {:?}; entries made {}; voided the other's as late {late}; blamed the other {} times, \
                 first {:?}",
                short(me),
                sim.stats(),
                made[i],
                blamed.len(),
                blamed.first()
            );
            assert!(blamed.is_empty(), "machine {i} blamed the other: {} events, first {:?}", blamed.len(), blamed[0]);
            assert_eq!(sim.stats().refused, 0, "machine {i} refused messages");
            assert_eq!(late, 0, "machine {i} voided {late} of the other's entries as late (pe-agree's outage rule)");
            // pe-agree's outage mark keeps the outage's backlog out of the lateness reports, so stamp-ahead's lead
            // stays at or near 0 through the return (2 and 4 frames measured): before it, both leads rose to the 3 s
            // clamp, 180 frames, and took 90 s to drain (@design-reviewer's Medium on a6b1035a; @coordinator-opus
            // m744fc393cdf9). A loaded machine's loopback ticks run late, so the bound allows 0.2 s.
            let lead = sim.stats().largest_lead;
            assert!(lead <= 12, "machine {i}'s stamp-ahead lead rose to {lead} frames over the blackout");
        }
        assert!(resumed, "finality did not resume within {resume_within:?}");
        assert!(all_final, "not every entry made became final on both sides");
        drop(relays);
    }

    /// The blackout tests' two machines, over real UDP on loopback, each reached through a [`Relay`] that drops
    /// everything while `dark` is set. Their hands move (two entries a frame), each has pe-api's ports, and each logs
    /// its agreement events. A first, then B.
    struct DarkPair {
        sims: Vec<(Sim, MemberKey)>,
        logs: Vec<Arc<std::sync::Mutex<Vec<pe_seams::AgreeEvent>>>>,
        ports: Vec<crate::api::Ports>,
        clocks: Vec<Arc<dyn FrameClock>>,
        dark: Arc<std::sync::atomic::AtomicBool>,
        relays: [Relay; 2],
    }

    fn dark_pair(host: &crate::clock::HostClock) -> DarkPair {
        let dark = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Each machine advertises its relay; the relays are started once the machines' own addresses are known.
        let (sa_sock, sb_sock) = (UdpSocket::bind(loopback()).unwrap(), UdpSocket::bind(loopback()).unwrap());
        let (ra_addr, rb_addr) = (sa_sock.local_addr().unwrap(), sb_sock.local_addr().unwrap());
        drop((sa_sock, sb_sock));
        let a = Endpoint::bind([21; 32], loopback(), vec![ra_addr], true, host.clone(), None).unwrap();
        let mut b = Endpoint::bind([22; 32], loopback(), vec![rb_addr], false, host.clone(), None).unwrap();
        let ra = Relay::start_on(ra_addr, a.local_addr(), dark.clone());
        let rb = Relay::start_on(rb_addr, b.local_addr(), dark.clone());
        let invite = a.invite(Duration::from_secs(600)).unwrap();
        let reply = b.reply(&invite);
        b.dial(&invite).unwrap();
        let theirs = invite.clone();
        let joiner = std::thread::spawn(move || b.join_session(&theirs, RATE, Duration::from_secs(20), runner()));
        let sa = a.open_session(&invite, &reply, &params(), Duration::from_secs(20), runner()).expect("A's session");
        let sb = joiner.join().unwrap().expect("B's session");

        let feeds = [[-0.5f32, 1.0, -0.4], [0.5, 1.1, -0.3]];
        let (mut sims, mut logs, mut ports, mut clocks) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (s, feed) in [sa, sb].into_iter().zip(feeds) {
            let slot = Arc::new(FrameSlot::new());
            let log = Arc::new(std::sync::Mutex::new(Vec::new()));
            let me = s.me;
            let sim = Sim::new(Box::new(s.node), s.replica, s.frames.clone(), slot, Budget::new(RATE)).with_local(
                me,
                crate::local::chain_nonce(),
                Arc::new(MovingHands(feed)),
            );
            let port = sim.ports("Pit", || {}).expect("pe-api's ports: a pe-world replica and a local player");
            let sim = sim
                .with_api(port.clone())
                .with_transport(transport(s.handle), Some(Box::new(s.hook)))
                .with_event_log(log.clone());
            sims.push((sim, me));
            logs.push(log);
            ports.push(port);
            clocks.push(s.frames);
        }
        DarkPair { sims, logs, ports, clocks, dark, relays: [ra, rb] }
    }

    /// Ticks the machines until `until` holds, or for at most `max`: whether it held.
    fn run_pair(
        host: &crate::clock::HostClock,
        sims: &mut [(Sim, MemberKey)],
        until: &mut dyn FnMut(&Machines) -> bool,
        max: Duration,
    ) -> bool {
        let end = Instant::now() + max;
        while Instant::now() < end {
            for (sim, _) in sims.iter_mut() {
                let _ = sim.tick(host());
            }
            if until(sims) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    #[test]
    #[ignore = "about 10 s in release (debug simulates too slowly to finalize in time): run with --release --include-ignored"]
    fn a_half_second_dropout_leaves_both_stamp_leads_at_or_near_0() {
        // A Wi-Fi hiccup: the path goes dark for 0.5 s. pe-agree's outage mark at 0.3 s (`outage_silence_s`,
        // agreement.md 3; @coordinator-opus m3981d59ac415) keeps the dropout's backlog out of the lateness reports, so
        // neither member's stamp-ahead lead rises. At the old 1 s mark a 0.3 s blackout left leads of 20 and 23 frames,
        // and a 0.7 s one 49, each held for the 3 s window and then drained over 2-9 s: seconds of laggy hands after
        // every hiccup (@engine-architect m40ceaa4a049d).
        use pe_seams::{AgreeEvent, VoidReason};
        use std::sync::atomic::Ordering;
        const DROPOUT: Duration = Duration::from_millis(500);
        let ceiling = Duration::from_nanos(crate::genesis::settings().timing.max_timeout_ns as u64);
        let resume_within = 2 * ceiling + Duration::from_secs(5);
        let host = crate::clock::shared();
        let DarkPair { mut sims, logs, ports: _ports, clocks: _clocks, dark, relays } = dark_pair(&host);
        let run = |sims: &mut Vec<(Sim, MemberKey)>, until: &mut dyn FnMut(&Machines) -> bool, max: Duration| {
            run_pair(&host, sims, until, max)
        };
        assert!(
            run(&mut sims, &mut |s| s.iter().all(|(sim, _)| sim.stats().finalized > 0), Duration::from_secs(20)),
            "nothing final before the dropout: {:?}",
            sims.iter().map(|(s, _)| s.stats().clone()).collect::<Vec<_>>()
        );
        // A second of steady state, so each lead's window holds the link's own reports.
        run(&mut sims, &mut |_| false, Duration::from_secs(1));
        dark.store(true, Ordering::Relaxed);
        run(&mut sims, &mut |_| false, DROPOUT);
        dark.store(false, Ordering::Relaxed);
        let back = Instant::now();
        let before: Vec<u64> = sims.iter().map(|(s, _)| s.stats().finalized).collect();
        let resumed = run(
            &mut sims,
            &mut |s| s.iter().enumerate().all(|(i, (sim, _))| sim.stats().finalized > before[i]),
            resume_within,
        );
        let resumed_after = back.elapsed();
        // Past the 3 s window: a lead the dropout's reports called for would have risen by now.
        run(&mut sims, &mut |_| false, Duration::from_secs(4));
        println!("after the path came back: finality resumed {resumed} ({resumed_after:?})");
        for (i, (sim, me)) in sims.iter().enumerate() {
            let them = sims[1 - i].1;
            let late = logs[i]
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, AgreeEvent::Voided { author, reason: VoidReason::Late, .. } if *author == them))
                .count();
            let st = sim.stats();
            println!(
                "machine {i} ({}): largest lead {} frames, lead now {}, longest drain {} frames; voided the other's as \
                 late {late}",
                short(me),
                st.largest_lead,
                st.lead,
                st.longest_drain
            );
            // A loaded machine's loopback ticks run late, so the bound allows 0.2 s, as the 10 s blackout's does.
            assert!(st.largest_lead <= 12, "machine {i}'s stamp-ahead lead rose to {} frames", st.largest_lead);
            assert_eq!(late, 0, "machine {i} voided {late} of the other's entries as late");
        }
        assert!(resumed, "finality did not resume within {resume_within:?} of the path's return");
        drop(relays);
    }

    #[test]
    fn the_build_id_names_pe_app_the_script_build_identity_and_the_runner() {
        use pe_replica::logic::ScriptRunner;
        let id = String::from_utf8(build_id().to_vec()).unwrap();
        assert!(id.starts_with(concat!("pe-app ", env!("CARGO_PKG_VERSION"), "; scripts ")), "{id}");
        let (scripts, runner) = id.split_once("; scripts ").and_then(|(_, r)| r.split_once("; runner ")).expect("both");
        // Under cargo the toolchain is known, so B is there. Each part is its hash's first 6 bytes in hex.
        for part in [scripts, runner] {
            assert!(part.len() == 12 && part.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        }
        let tc = pe_logic::build::Toolchain::current().expect("a toolchain under cargo");
        assert_eq!(scripts, hex6(&pe_logic::build::build_identity(&tc, &pe_logic::build::engine_sdk()).unwrap()));
        // The runner part is the identity main.rs's runner reports, from the same settings (Q1; S2 v1.7).
        let (settings, run) = runner_settings();
        let reported = pe_replica::logic::LogicRunner::new(settings, run).expect("a runner").identity();
        assert_eq!(runner, hex6(&pe_seams::Hash::of("pe.app.runner", &reported)));
        // Other fuel can change what a script computes, so the part differs; the same settings give the same part.
        let more_fuel = pe_logic::run::RunSettings { fuel_per_call: run.fuel_per_call + 1, ..run };
        assert_ne!(runner_part(&settings, &more_fuel), runner);
        assert_eq!(runner_part(&settings, &run), runner);
        // The hello's limit is the tighter: `bind` refuses an id over it (NetError::Hello).
        assert!(build_id().len() <= pe_net::lifecycle::MAX_BUILD, "over the hello's bound: {}", build_id().len());
        assert_eq!(build_id(), build_id());
    }
}
