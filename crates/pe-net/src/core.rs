//! `Core` (transport.md 7): everything above the socket, sans-IO. It holds quinn-proto's endpoint and
//! connections, each link's lanes, and the connection lifecycle (6). It takes time and bytes in and gives bytes
//! and events out, so the harness drives exactly this code.
//!
//! **Built so far:** dialling an invite's addresses `T_stagger` apart (6.2) and the reply code's dial back; one
//! connection per peer, by the lower-client-key rule (6.2); the hello and its guards on both sides (6.3);
//! admission by the app and the welcome (6.4); the lanes over QUIC DATAGRAM frames and unidirectional streams
//! (3); the close reasons (6.6); seam S4's `Transport`.
//!
//! **The budget** (4): datagrams go to quinn only while its send buffer has room, so priority holds above it
//! (4.2); redundant rides follow the ride gate; Bulk streams start and continue only under the delay guard (4.3),
//! Repair streams whenever their share of the stream credit allows.
//!
//! **The clock** (5): every connected peer gets a clock ping each `T_sample` (`T_sample_fast` while converging),
//! answered with the session time; the answers are the clock's samples, and it ticks at the same cadence. A
//! joiner begins converging on the welcome and starts its frame clock when it is ready.
//!
//! **Link states** (6.5): Degraded after `T_degraded` with nothing received, Up again when traffic resumes.
//!
//! **The mesh** (6.7): a joiner dials every member its welcome names. A member whose link to a peer comes up
//! introduces it to every other peer it is up with, and them to it, with rendezvous records; both sides of each
//! pair then dial at once, and the lower-client-key rule keeps one connection.
//!
//! **Not yet:**
//! Degraded, keep-alive state and address changes (6.5); the rendezvous (6.7).
//!
//! **The welcome exchange** (6.4, 6.5): lane traffic is closed both ways until it completes, and each side opens
//! so that nothing is sent before the other side takes it in:
//! - the role comes from [`Core::set_member`], fixed per connection when it is confirmed;
//! - a member opens nothing before its own `admit`: then it sends the welcome and opens its receive side;
//! - a joiner that gets a welcome opens both sides and answers with an "up" record, on which the member opens its
//!   send side. Between two members, each opens its send side once it has both admitted and been welcomed;
//! - a link is Up when both sides are open. `T_welcome` closes a connection that doesn't get there.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use pe_net_crypto::Random;
use pe_net_crypto::identity::{AnyPeerKey, Identity, Invite, PinnedKey, Reply, peer_id_of};
use pe_net_crypto::quinn::{Cids, QuicClientConfig, QuicServerConfig, ResetKey, TokenKey};
use pe_seams::{HostTime, Lane, LinkState, LinkStats, MemberKey, NetEvent, PeerId, Received, SimRate, Transport};
use quinn_proto::{
    ClientConfig, Connection, ConnectionError, ConnectionHandle, DatagramEvent, Dir, Endpoint, EndpointConfig, Event,
    IdleTimeout, ServerConfig, StreamEvent, StreamId, TransportConfig, VarInt,
};

use crate::budget::{Budget, BudgetParams};
use crate::clock::{Clock, ClockEvent, ClockFault, ClockParams, Sample, SharedClock};
use crate::lanes::{Control, LaneParams, Link, Reader, put_varint};
use crate::lifecycle::{
    CloseCode, Hello, MAX_HELLO, MAX_RENDEZVOUS, MAX_WELCOME, Refusal, Welcome, decode_rendezvous, encode_rendezvous,
};

const ALPN: &[u8] = b"pe/1";
/// Stream kinds past the lanes (0-4): the lifecycle records.
const K_HELLO: u8 = 0x80;
const K_WELCOME: u8 = 0x81;
const K_UP: u8 = 0x82;
const K_RENDEZVOUS: u8 = 0x83;
/// The datagram payload before quinn reports one (QUIC's minimum path MTU, transport.md 3.6).
const MIN_DGRAM: usize = 1162;

/// `T_idle` for M1 (transport.md 6.5, 10): 30 s, the longest outage a session survives.
pub const T_IDLE_M1: Duration = Duration::from_secs(30);

/// transport.md 10, the parts the Core uses so far.
#[derive(Clone)]
pub struct Config {
    pub lanes: LaneParams,
    pub budget: BudgetParams,
    pub clock: ClockParams,
    /// `T_sample` and `T_sample_fast` (5.2).
    pub t_sample: Duration,
    pub t_sample_fast: Duration,
    /// `T_degraded` (6.5).
    pub t_degraded: Duration,
    /// My hello. Its token is replaced by the invite's id on each dial.
    pub hello: Hello,
    /// Whether I'm a member, so I admit peers (6.3) rather than wait for a welcome.
    pub member: bool,
    /// `T_welcome`: the longest a confirmed connection waits for the welcome exchange (6.4).
    pub t_welcome: Duration,
    /// `T_stagger`: the gap between dials to an invite's addresses.
    pub t_stagger: Duration,
    /// `T_keepalive` and `T_idle` (6.5). `T_idle` is how long a silent link survives: an outage shorter than it
    /// resumes on the same connection; a longer one closes the link, and a member's is then redialled (`redial`).
    pub t_keepalive: Duration,
    pub t_idle: Duration,
    /// The redial (6.5): a member's link that closed for silence is dialled again. `None`: never (M1's behaviour).
    pub redial: Option<RedialParams>,
    /// `T_trip`'s floor (6.5, [`crate::outage`]): the outage probe opens the window after this long with no ACK.
    /// `None`: quinn's plain Cubic, whose probe backoff keeps a link dark for up to twice an outage.
    pub t_trip: Option<Duration>,
    /// `N_streams_in`: incoming streams open at once per peer.
    pub n_streams_in: u32,
    /// `N_dgram_buf`: quinn's datagram send buffer, in datagrams.
    pub n_dgram_buf: usize,
    /// `W_send`: quinn's `send_window`.
    pub w_send: u64,
    /// The randomness for keys, connection ids and tokens: the OS's, or a seeded one in the harness (8).
    pub random: &'static dyn Random,
}

impl Config {
    pub fn new(hello: Hello, member: bool) -> Self {
        Self {
            lanes: LaneParams::default(),
            budget: BudgetParams::default(),
            clock: ClockParams::default(),
            t_sample: Duration::from_secs(1),
            t_sample_fast: Duration::from_millis(200),
            t_degraded: Duration::from_secs(1),
            hello,
            member,
            t_welcome: Duration::from_secs(10),
            t_stagger: Duration::from_millis(250),
            // 250 ms: during an outage it is the probe of a link with nothing else to send (6.5).
            t_keepalive: Duration::from_millis(250),
            // M1 (@coordinator-opus): 30 s, so Wi-Fi or Tailscale hiccups don't end a session while
            // pe-agree has no return. Shorter again once a member can return (model v1).
            t_idle: T_IDLE_M1,
            redial: Some(RedialParams::default()),
            t_trip: Some(Duration::from_millis(300)),
            n_streams_in: 64,
            n_dgram_buf: 4,
            w_send: 64 * 1024,
            random: pe_net_crypto::random::os(),
        }
    }
}

/// The redial's settings (transport.md 6.5, 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedialParams {
    /// `T_redial`: how long after the close pe-net keeps dialling, before it reports the link lost.
    pub window: Duration,
    /// `T_redial_first` and `T_redial_max`: the first gap between attempts, doubling up to the last. An attempt still
    /// connecting when the next is due is closed, so a handshake's own backoff into a dead path never sets the pace.
    pub first: Duration,
    pub max: Duration,
}

impl Default for RedialParams {
    fn default() -> Self {
        // 90 s, so with `T_idle` (30 s) a member may be dark for 2 min in all: the longest outage after which the
        // whole backlog was measured to become final again (pe-app's blackout test: 120 s dark, every entry final
        // 29 s after the return; 290 s dark resumed finality but left part of the backlog unfinal 400 s later;
        // transport.md 6.5). 4 s apart at most: once the path is back, the next attempt reaches it within about 4 s
        // plus a round trip.
        Self { window: Duration::from_secs(90), first: Duration::from_millis(500), max: Duration::from_secs(4) }
    }
}

/// A member's link being redialled (6.5).
#[derive(Debug)]
struct Redial {
    /// Its last addresses: the ones its hello listed, and where its packets came from.
    addrs: Vec<SocketAddr>,
    /// `T_redial` after the close.
    until: HostTime,
    /// The next attempt, and the gap after it.
    next: HostTime,
    gap: Duration,
}

/// A packet to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transmit {
    pub to: SocketAddr,
    pub bytes: Vec<u8>,
}

/// The lifecycle's outcomes for the app (6.3-6.6), beside seam S4's [`NetEvent`]s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// A member welcomed me (6.4).
    Welcomed { peer: PeerId, welcome: Welcome },
    /// A connection was refused, by me (`by_me`) or by the peer, before or at the welcome. A close before the
    /// welcome counts as a refusal (9.4).
    Refused { peer: PeerId, refusal: Refusal, by_me: bool },
    /// The peer sent bytes pe-net can't accept; the connection is closed with code 6.
    Fault { peer: PeerId, what: String },
    /// My clock converged before frame 0 (5.4): pe-agree's join may proceed.
    ClockReady,
    /// A clock fault (5.6).
    ClockFault(ClockFault),
    /// A member's link closed for silence (`T_idle`, or a stateless reset), with no close from either side: pe-net
    /// dials it again until `until` (6.5). A `LinkUp` ends it.
    Redialling { peer: PeerId, until: HostTime },
    /// A member's link is down and pe-net won't dial it again (6.5): it closed with a close, its redial ran out of
    /// `T_redial`, or the peer refused a redial.
    Lost { peer: PeerId },
}

#[derive(Debug)]
enum Role {
    /// I dialled, pinning the peer's key.
    Dialler {
        pin: PeerId,
    },
    Acceptor,
}

struct Conn {
    q: Connection,
    role: Role,
    peer: Option<PeerId>,
    state: LinkState,
    link: Link,
    budget: Budget,
    /// `link.first_sent_wire` already given to the budget.
    first_sent_seen: u64,
    hello: Hello,
    theirs: Option<Hello>,
    /// My role on this connection, fixed when it was confirmed: a member admits, a joiner waits (6.4).
    member: bool,
    /// When the handshake was confirmed (`T_welcome` runs from here), until the link is first Up.
    confirmed_at: Option<HostTime>,
    /// I admitted the peer (sent a welcome).
    admitted: bool,
    /// I was welcomed.
    welcomed: bool,
    /// The joiner's up record arrived.
    got_up: bool,
    /// I introduced this peer to my other peers (6.7), once it was up.
    introduced: bool,
    /// Rendezvous records from this peer in the current `T_rdv` window: (window start, count) (6.7).
    rdv: (HostTime, u32),
    /// I closed it (once).
    closed: bool,
    /// quinn's count of datagrams received, and when it last moved (6.5).
    rx_seen: u64,
    heard_at: HostTime,
    open_send: bool,
    open_recv: bool,
    /// Another connection to the same peer won (6.2): this one is closing quietly.
    superseded: bool,
    /// Incoming streams being read, with the bound their first byte sets.
    inbound: HashMap<StreamId, (Vec<u8>, usize)>,
    /// Outgoing stream bytes quinn hasn't taken yet, and the stream's priority (`open_stream`): under 2 a low lane
    /// (Repair 1, Bulk 0), which shares half the peer's stream credit; under 1 Bulk, which the delay guard holds too.
    outbound: VecDeque<(StreamId, Bytes, i32)>,
    /// Stream messages quinn can't open yet, the peer's `N_streams_in` credit being used up (3.5), with their
    /// priority: highest first, in order within one priority. They wait for the peer's MAX_STREAMS, which comes in a
    /// packet, so each `drive` retries them. Nothing is dropped.
    unopened: VecDeque<(i32, Bytes)>,
    /// A welcome that overtook the peer's hello (streams aren't ordered): handled once the hello is in.
    early_welcome: Option<Vec<u8>>,
    /// `L_msg` and `L_bulk`, the bounds of incoming lane streams.
    limits: (usize, usize),
}

/// The close detail of a duplicate connection (code Left, 6.2).
const DUPLICATE: &str = "duplicate connection";
/// `T_rdv` and `R_rdv`: at most `R_rdv` rendezvous records from one peer per `T_rdv` (6.7).
const T_RDV: Duration = Duration::from_secs(10);
const R_RDV: u32 = 64;
/// The most mesh and rendezvous dials (addresses) queued or under way at once (6.7).
const MAX_RDV_DIALS: usize = 64;

/// The sans-IO core.
pub struct Core {
    cfg: Config,
    identity: Identity,
    endpoint: Endpoint,
    server: Arc<ServerConfig>,
    transport: Arc<TransportConfig>,
    conns: HashMap<ConnectionHandle, Conn>,
    peers: BTreeMap<PeerId, ConnectionHandle>,
    /// The session's members, as the app last gave them (`set_clock_members`): whose rendezvous I take (6.7).
    members: BTreeSet<PeerId>,
    /// Dials not yet made: (when, address, pinned key, token).
    dials: Vec<(HostTime, SocketAddr, PeerId, Vec<u8>)>,
    /// Members' links being redialled (6.5).
    redials: BTreeMap<PeerId, Redial>,
    out: VecDeque<Transmit>,
    received: VecDeque<Received>,
    events: Vec<NetEvent>,
    lifecycle: VecDeque<Lifecycle>,
    /// quinn counts time in `Instant`s: `base` stands for `origin`, set by the first call.
    base: Instant,
    origin: Option<HostTime>,
    /// The latest time given.
    now: HostTime,
    /// Something was queued since the last drive: wake at once.
    dirty: bool,
    clock: Clock,
    /// A joiner's frame 0 and rate, from the welcome, for when its clock is ready.
    pending_start: Option<(i64, SimRate)>,
    next_ping: HostTime,
}

#[derive(Debug)]
pub enum CoreError {
    Tls(String),
    /// The configured hello breaks a bound its peers enforce (6.3): refused here rather than sent and dropped.
    Hello(crate::lifecycle::HelloError),
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::Tls(e) => write!(f, "TLS configuration: {e}"),
            CoreError::Hello(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CoreError {}

fn tls<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Tls(e.to_string())
}

fn key(p: &PeerId) -> [u8; 32] {
    p.0
}

impl Core {
    /// `seed` seeds quinn's own randomness (packet numbers, padding); the harness passes a fixed one (8).
    pub fn new(cfg: Config, identity: Identity, seed: [u8; 32]) -> Result<Self, CoreError> {
        cfg.hello.check().map_err(CoreError::Hello)?;
        let rng = cfg.random;
        let mut ecfg = EndpointConfig::new(Arc::new(ResetKey::new(rng).map_err(tls)?));
        ecfg.cid_generator(move || Box::new(Cids::new(rng)));
        let mut t = TransportConfig::default();
        t.max_concurrent_uni_streams(VarInt::from_u32(cfg.n_streams_in))
            .max_concurrent_bidi_streams(VarInt::from_u32(0))
            .keep_alive_interval(Some(cfg.t_keepalive))
            .max_idle_timeout(IdleTimeout::try_from(cfg.t_idle).ok())
            .datagram_receive_buffer_size(Some(64 * MIN_DGRAM))
            .datagram_send_buffer_size(cfg.n_dgram_buf * MIN_DGRAM)
            .send_window(cfg.w_send);
        if let Some(quiet) = cfg.t_trip {
            let inner = Arc::new(quinn_proto::congestion::CubicConfig::default());
            t.congestion_controller_factory(Arc::new(crate::outage::OutageConfig { inner, quiet }));
        }
        let transport = Arc::new(t);
        let p = pe_net_crypto::provider_with(rng);
        let ck = identity.certified_key(&p).map_err(tls)?;
        let mut sc = rustls::ServerConfig::builder_with_provider(p)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(tls)?
            .with_client_cert_verifier(Arc::new(AnyPeerKey))
            .with_cert_resolver(Arc::new(rustls::server::AlwaysResolvesServerRawPublicKeys::new(ck)));
        sc.alpn_protocols = vec![ALPN.to_vec()];
        let mut server = ServerConfig::new(
            Arc::new(QuicServerConfig::new(sc).map_err(tls)?),
            Arc::new(TokenKey::new(rng).map_err(tls)?),
        );
        server.transport_config(transport.clone());
        let server = Arc::new(server);
        let endpoint = Endpoint::new(Arc::new(ecfg), Some(server.clone()), true, Some(seed));
        let clock = Clock::idle(cfg.clock.clone());
        Ok(Self {
            cfg,
            identity,
            endpoint,
            server,
            transport,
            conns: HashMap::new(),
            peers: BTreeMap::new(),
            members: BTreeSet::new(),
            redials: BTreeMap::new(),
            dials: Vec::new(),
            out: VecDeque::new(),
            received: VecDeque::new(),
            events: Vec::new(),
            lifecycle: VecDeque::new(),
            base: Instant::now(),
            origin: None,
            now: HostTime(i64::MIN),
            dirty: false,
            clock,
            pending_start: None,
            next_ping: HostTime(i64::MIN),
        })
    }

    /// The server config, for a harness that sets a fixed token time source (transport.md 8).
    pub fn server_config(&self) -> &Arc<ServerConfig> {
        &self.server
    }

    /// My key.
    pub fn me(&self) -> PeerId {
        MemberKey(self.identity.public())
    }

    /// The frame clock the simulation thread reads (5.7), the same one for the Core's whole life.
    pub fn frame_clock(&self) -> Arc<SharedClock> {
        self.clock.shared()
    }

    /// I am a world's first member: my clock is ready at once, with frame 0 at `epoch_ns` (5.4).
    pub fn start_first_member(&mut self, now: HostTime, epoch_ns: i64, rate: SimRate) {
        self.clock.begin_first_member(now, epoch_ns, rate);
    }

    /// The members whose clocks count (5.3, 5.4): the start cut's list, then the current members.
    pub fn set_clock_members(&mut self, members: Vec<PeerId>) {
        self.members = members.iter().copied().collect();
        // A key that is no longer a member isn't redialled (6.5).
        let gone: Vec<PeerId> = self.redials.keys().filter(|k| !self.members.contains(k)).copied().collect();
        let now = self.now;
        for k in gone {
            self.end_redial(now, k);
        }
        self.clock.set_members(members);
    }

    /// I became a member (pe-agree's join completed): from now on I admit peers.
    pub fn set_member(&mut self, member: bool) {
        self.cfg.member = member;
    }

    fn inst(&mut self, now: HostTime) -> Instant {
        self.now = self.now.max(now);
        let o = *self.origin.get_or_insert(now);
        self.base + Duration::from_nanos(u64::try_from(now.0 - o.0).unwrap_or(0))
    }

    fn host(&self, t: Instant) -> HostTime {
        let o = self.origin.unwrap_or(HostTime(0));
        o.plus(t.saturating_duration_since(self.base))
    }

    /// Dial an invite: its addresses `T_stagger` apart, all pinning its key (6.2). The app checks the expiry.
    pub fn dial(&mut self, now: HostTime, invite: &Invite) {
        let pin = MemberKey(invite.key);
        for (i, a) in invite.addrs.iter().enumerate() {
            let at = now.plus(self.cfg.t_stagger * u32::try_from(i).unwrap_or(u32::MAX));
            self.dials.push((at, *a, pin, invite.id.to_vec()));
        }
        self.drive(now);
    }

    /// Take a joiner's reply code (2.4): dial its addresses at once. My Initial packets are the punch packets; the
    /// lower-client-key rule keeps one connection if both dials succeed.
    pub fn take_reply(&mut self, now: HostTime, reply: &Reply) {
        let pin = MemberKey(reply.key);
        for a in &reply.addrs {
            self.dials.push((now, *a, pin, reply.invite_id.to_vec()));
        }
        self.drive(now);
    }

    /// Admit a peer whose `JoinRequest` I raised: send the welcome (6.4).
    pub fn admit(&mut self, now: HostTime, peer: PeerId, welcome: &Welcome) {
        let Some(h) = self.peers.get(&peer).copied() else { return };
        if let Some(c) = self.conns.get_mut(&h)
            && c.member
            && c.theirs.is_some()
            && !c.admitted
            && !c.closed
        {
            c.admitted = true;
            c.state = LinkState::Welcomed;
            let mut b = vec![K_WELCOME];
            b.extend_from_slice(&welcome.encode());
            open_stream(c, 10, b.into());
            c.open_recv = true;
            // Between two members, the peer's welcome may already be here, held until now (6.4).
            c.open_send = c.welcomed;
            update_open(c, &mut self.events);
        }
        self.drive(now);
    }

    /// Refuse a peer: close with the reason (6.6).
    pub fn refuse(&mut self, now: HostTime, peer: PeerId, refusal: Refusal) {
        if let Some(h) = self.peers.get(&peer).copied() {
            let t = self.inst(now);
            if let Some(c) = self.conns.get_mut(&h) {
                close(c, t, &refusal);
            }
            self.lifecycle.push_back(Lifecycle::Refused { peer, refusal, by_me: true });
        }
        self.drive(now);
    }

    /// This endpoint stops: every open connection closes with code 0 (`Left`), so each peer learns at once rather
    /// than after `T_idle` (6.5). The close packets are in `poll_transmit` when this returns.
    pub fn close_all(&mut self, now: HostTime) {
        self.redials.clear();
        let t = self.inst(now);
        for c in self.conns.values_mut().filter(|c| !c.closed) {
            close(c, t, &Refusal { code: CloseCode::Left, detail: "stopped".into() });
        }
        self.drive(now);
    }

    /// A UDP datagram arrived.
    pub fn handle_datagram(&mut self, now: HostTime, from: SocketAddr, data: &[u8]) {
        let t = self.inst(now);
        let mut buf = Vec::new();
        match self.endpoint.handle(t, from, None, None, BytesMut::from(data), &mut buf) {
            Some(DatagramEvent::NewConnection(inc)) => match self.endpoint.accept(inc, t, &mut buf, None) {
                Ok((h, q)) => {
                    let hello = self.cfg.hello.clone();
                    self.conns.insert(
                        h,
                        Conn::new(q, Role::Acceptor, hello, self.cfg.lanes.clone(), self.cfg.budget.clone()),
                    );
                }
                Err(e) => {
                    if let Some(tx) = e.response {
                        self.out.push_back(Transmit { to: tx.destination, bytes: buf[..tx.size].to_vec() });
                    }
                }
            },
            Some(DatagramEvent::ConnectionEvent(h, ev)) => {
                if let Some(c) = self.conns.get_mut(&h) {
                    c.q.handle_event(ev);
                }
            }
            Some(DatagramEvent::Response(tx)) => {
                self.out.push_back(Transmit { to: tx.destination, bytes: buf[..tx.size].to_vec() })
            }
            None => {}
        }
        self.drive(now);
    }

    /// Time passed: timers fire.
    pub fn handle_timeout(&mut self, now: HostTime) {
        let t = self.inst(now);
        for c in self.conns.values_mut() {
            if c.q.poll_timeout().is_some_and(|d| d <= t) {
                c.q.handle_timeout(t);
            }
        }
        self.drive(now);
    }

    /// The next packet to send.
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.out.pop_front()
    }

    /// When to call [`Core::handle_timeout`] next.
    /// (`&mut`, as quinn-proto's own `poll_timeout` is.)
    pub fn poll_timeout(&mut self) -> Option<HostTime> {
        if self.dirty {
            return Some(self.now);
        }
        let quinn = self.conns.values_mut().filter_map(|c| c.q.poll_timeout()).min().map(|t| self.host(t));
        let lanes = self.conns.values().filter(|c| c.open_send).filter_map(|c| c.link.next_timeout()).min();
        let dial = self.dials.iter().map(|d| d.0).min();
        let redial = self.redials.values().map(|r| r.next.min(r.until)).min();
        let ping = (!self.conns.is_empty()).then_some(self.next_ping);
        let t_degraded = self.cfg.t_degraded;
        let degraded =
            self.conns.values().filter(|c| c.state == LinkState::Up).map(|c| c.heard_at.plus(t_degraded)).min();
        let t_welcome = self.cfg.t_welcome;
        let welcome = self
            .conns
            .values()
            .filter(|c| !c.closed && c.state != LinkState::Up)
            .filter_map(|c| c.confirmed_at.map(|at| at.plus(t_welcome)))
            .min();
        [quinn, lanes, dial, redial, ping, degraded, welcome].into_iter().flatten().min()
    }

    /// The lifecycle's outcomes since the last call.
    pub fn poll_lifecycle(&mut self) -> Option<Lifecycle> {
        self.lifecycle.pop_front()
    }

    /// The peers with a connection.
    pub fn peers(&self) -> Vec<PeerId> {
        self.peers.keys().copied().collect()
    }

    /// The peer's hello, once it has arrived (the app reads it with a `JoinRequest`).
    pub fn hello_of(&self, peer: &PeerId) -> Option<&Hello> {
        self.peers.get(peer).and_then(|h| self.conns.get(h)).and_then(|c| c.theirs.as_ref())
    }

    /// Run everything that is due: dials, quinn's events, the lanes, and the packets out.
    fn drive(&mut self, now: HostTime) {
        self.dirty = false;
        let t = self.inst(now);
        self.redial(now, t);
        let due: Vec<_> = {
            let (due, later): (Vec<_>, Vec<_>) = self.dials.drain(..).partition(|d| d.0 <= now);
            self.dials = later;
            due
        };
        for (_, addr, pin, token) in due {
            // Already connected: the first confirmed dial won (6.2). A connection closed here and still draining
            // (a refusal, `T_welcome`) is not a connection (@engine-architect's Low 1 on net/app-net f7d31a31).
            if self
                .peers
                .get(&pin)
                .and_then(|h| self.conns.get(h))
                .is_some_and(|c| c.state != LinkState::Connecting && !c.closed)
            {
                continue;
            }
            if let Some((h, q)) = self.connect(t, addr, &pin) {
                let mut hello = self.cfg.hello.clone();
                hello.token = token;
                self.conns.insert(
                    h,
                    Conn::new(q, Role::Dialler { pin }, hello, self.cfg.lanes.clone(), self.cfg.budget.clone()),
                );
            }
        }
        if now >= self.next_ping {
            self.clock_round(now);
        }
        let handles: Vec<ConnectionHandle> = self.conns.keys().copied().collect();
        for h in handles {
            self.drive_conn(now, t, h);
        }
        if self.introduce() {
            let handles: Vec<ConnectionHandle> = self.conns.keys().copied().collect();
            for h in handles {
                self.drive_conn(now, t, h);
            }
        }
    }

    /// The redials (6.5). A link up again ends its redial. Past `T_redial` it ends, reported `Lost`. Otherwise, when an
    /// attempt is due, the one still connecting is closed and every address is dialled again, the gap doubling up to
    /// `T_redial_max`; while a connection to the peer is confirmed and awaiting its welcome, no new attempt starts.
    fn redial(&mut self, now: HostTime, t: Instant) {
        if self.redials.is_empty() {
            return;
        }
        let max = self.cfg.redial.map_or(Duration::ZERO, |p| p.max);
        let keys: Vec<PeerId> = self.redials.keys().copied().collect();
        for k in keys {
            let held = self.peers.get(&k).and_then(|h| self.conns.get(h)).filter(|c| !c.closed).map(|c| c.state);
            if matches!(held, Some(LinkState::Up | LinkState::Degraded)) {
                self.redials.remove(&k);
                continue;
            }
            let Some(r) = self.redials.get_mut(&k) else { continue };
            let ended = now >= r.until;
            if !ended && now < r.next {
                continue;
            }
            r.next = now.plus(r.gap);
            if held.is_some() && !ended {
                continue;
            }
            if ended {
                self.end_redial(now, k);
                self.lifecycle.push_back(Lifecycle::Lost { peer: k });
                continue;
            }
            r.gap = (r.gap * 2).min(max);
            let addrs = r.addrs.clone();
            self.drop_attempts(t, k);
            self.dials.extend(addrs.into_iter().map(|a| (now, a, k, Vec::new())));
        }
    }

    /// A redial ends without a link: its attempts still connecting are closed, and its dials not yet made dropped.
    fn end_redial(&mut self, now: HostTime, k: PeerId) {
        if self.redials.remove(&k).is_some() {
            let t = self.inst(now);
            self.drop_attempts(t, k);
            self.dirty = true;
        }
    }

    /// Close my attempts to `k` that are still connecting, and drop the dials to it no invite or reply asked for.
    fn drop_attempts(&mut self, t: Instant, k: PeerId) {
        let attempt = |c: &Conn| {
            !c.closed && c.state == LinkState::Connecting && matches!(&c.role, Role::Dialler { pin } if *pin == k)
        };
        for c in self.conns.values_mut().filter(|c| attempt(c)) {
            close(c, t, &Refusal { code: CloseCode::Left, detail: "redial".into() });
        }
        self.dials.retain(|d| d.2 != k || !d.3.is_empty());
    }

    /// A member introduces each newly up peer to its other up peers, and them to it (6.7). `true` if it did.
    fn introduce(&mut self) -> bool {
        let up = |c: &Conn| c.state == LinkState::Up && !c.closed && c.peer.is_some();
        let new: Vec<ConnectionHandle> =
            self.conns.iter().filter(|(_, c)| c.member && up(c) && !c.introduced).map(|(h, _)| *h).collect();
        if new.is_empty() {
            return false;
        }
        let all: Vec<(ConnectionHandle, PeerId, Vec<SocketAddr>)> = self
            .conns
            .iter()
            .filter(|(_, c)| up(c))
            .filter_map(|(h, c)| c.peer.map(|p| (*h, p, addresses(c))))
            .collect();
        for h in new {
            let Some((_, p, pa)) = all.iter().find(|x| x.0 == h).cloned() else { continue };
            for (oh, op, oa) in all.iter().filter(|x| x.0 != h) {
                if let Some(c) = self.conns.get_mut(&h) {
                    let mut b = vec![K_RENDEZVOUS];
                    b.extend_from_slice(&encode_rendezvous(op, oa));
                    open_stream(c, 10, b.into());
                }
                if let Some(o) = self.conns.get_mut(oh) {
                    let mut b = vec![K_RENDEZVOUS];
                    b.extend_from_slice(&encode_rendezvous(&p, &pa));
                    open_stream(o, 10, b.into());
                }
            }
            if let Some(c) = self.conns.get_mut(&h) {
                c.introduced = true;
            }
        }
        true
    }

    /// One clock round (5.2-5.6): ping every connected peer, tick, act on what the clock reports.
    fn clock_round(&mut self, now: HostTime) {
        for c in self.conns.values_mut().filter(|c| c.peer.is_some() && !c.closed) {
            c.link.send_control(Control::ClockPing { t1: now.0 });
        }
        self.clock.tick(now);
        for e in self.clock.events() {
            match e {
                ClockEvent::Ready => {
                    if let Some((epoch, rate)) = self.pending_start.take() {
                        self.clock.start(now, epoch, rate);
                    }
                    self.lifecycle.push_back(Lifecycle::ClockReady);
                }
                ClockEvent::Fault(f) => self.lifecycle.push_back(Lifecycle::ClockFault(f)),
            }
        }
        let every = if self.clock.is_ready() { self.cfg.t_sample } else { self.cfg.t_sample_fast };
        self.next_ping = now.plus(every);
    }

    fn connect(&mut self, t: Instant, addr: SocketAddr, pin: &PeerId) -> Option<(ConnectionHandle, Connection)> {
        let rng = self.cfg.random;
        let p = pe_net_crypto::provider_with(rng);
        let ck = self.identity.certified_key(&p).ok()?;
        let mut cc = rustls::ClientConfig::builder_with_provider(p)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .ok()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedKey::new(&key(pin)).ok()?))
            .with_client_cert_resolver(Arc::new(rustls::client::AlwaysResolvesClientRawPublicKeys::new(ck)));
        cc.alpn_protocols = vec![ALPN.to_vec()];
        let mut cfg = ClientConfig::new(Arc::new(QuicClientConfig::new(cc).ok()?));
        cfg.initial_dst_cid_provider(Cids::initial_dcid(rng));
        cfg.transport_config(self.transport.clone());
        self.endpoint.connect(t, cfg, addr, "pe").ok()
    }

    fn drive_conn(&mut self, now: HostTime, t: Instant, h: ConnectionHandle) {
        let me = self.me();
        let Some(c) = self.conns.get_mut(&h) else { return };
        // quinn's own events, both ways with the endpoint.
        while let Some(ev) = c.q.poll_endpoint_events() {
            if let Some(back) = self.endpoint.handle_event(h, ev) {
                c.q.handle_event(back);
            }
        }
        let mut connected = false;
        let mut lost = None;
        let mut readable = Vec::new();
        while let Some(e) = c.q.poll() {
            match e {
                Event::Connected => connected = true,
                Event::ConnectionLost { reason } => lost = Some(reason),
                Event::Stream(StreamEvent::Opened { dir: Dir::Uni }) => {
                    while let Some(id) = c.q.streams().accept(Dir::Uni) {
                        readable.push(id);
                    }
                }
                Event::Stream(StreamEvent::Readable { id }) => readable.push(id),
                _ => {}
            }
        }
        if connected {
            self.on_connected(now, t, h, me);
        }
        let Some(c) = self.conns.get_mut(&h) else { return };
        let peer = c.peer;
        // Datagrams in.
        while let Some(d) = c.q.datagrams().recv() {
            match c.link.on_datagram(now, &d) {
                Ok(_) if c.link.closed_drops > 0 => {
                    fault(c, t, peer, &mut self.lifecycle, "lane traffic before the welcome".into());
                    break;
                }
                Ok(inc) => {
                    if let Some(from) = peer {
                        for (lane, bytes) in inc.messages {
                            self.received.push_back(Received { from, lane, bytes, at: now });
                        }
                        for ctl in inc.control {
                            match ctl {
                                // Only a ready clock answers (5.2): a converging one's session time is still
                                // its host clock, which would pull a member's target and its rate estimate.
                                Control::ClockPing { t1 } if self.clock.is_ready() => {
                                    let s = self.clock.session_ns(now);
                                    c.link.send_control(Control::ClockPong { t1, t2: s, t3: s });
                                }
                                Control::ClockPing { .. } => {}
                                Control::ClockPong { t1, t2, t3 } => {
                                    self.clock.on_sample(from, Sample { t1: HostTime(t1), t2, t3, t4: now });
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    fault(c, t, peer, &mut self.lifecycle, format!("bad datagram: {e:?}"));
                    break;
                }
            }
        }
        // Streams in.
        for id in readable {
            let Some(c) = self.conns.get_mut(&h) else { return };
            if let Some(done) = read_stream(c, id) {
                match done {
                    Ok(bytes) => self.on_stream(now, t, h, bytes),
                    Err(what) => fault(c, t, peer, &mut self.lifecycle, what),
                }
            }
        }
        let Some(c) = self.conns.get_mut(&h) else { return };
        // Link states (6.5): anything received keeps a link Up; `T_degraded` of silence makes it Degraded.
        let rx = c.q.stats().udp_rx.datagrams;
        if rx != c.rx_seen {
            c.rx_seen = rx;
            c.heard_at = now;
            if c.state == LinkState::Degraded
                && let Some(peer) = c.peer
            {
                c.state = LinkState::Up;
                self.events.push(NetEvent::LinkUp { peer });
            }
        } else if c.state == LinkState::Up
            && now >= c.heard_at.plus(self.cfg.t_degraded)
            && let Some(peer) = c.peer
        {
            c.state = LinkState::Degraded;
            self.events.push(NetEvent::LinkDegraded { peer });
        }
        // `T_welcome` (6.4): a connection left between confirmation and Up is closed.
        let t_welcome = self.cfg.t_welcome;
        if !c.closed
            && c.state != LinkState::Up
            && c.confirmed_at.is_some_and(|at| now >= at.plus(t_welcome))
            && let Some(peer) = c.peer
        {
            let refusal = if c.member && !c.admitted {
                Refusal { code: CloseCode::Overloaded, detail: "no admission decision in time".into() }
            } else {
                Refusal { code: CloseCode::NotAdmitted, detail: "not admitted in time".into() }
            };
            close(c, t, &refusal);
            self.lifecycle.push_back(Lifecycle::Refused { peer, refusal, by_me: true });
        }
        // The budget reads quinn's path (4.1), and the lanes its RTT. The window is the congestion controller's own
        // (its metrics): the outage probe's open window lifts only quinn's gate (6.5).
        let st = c.q.stats().path;
        let cwnd = c.q.congestion_state().metrics().congestion_window;
        c.budget.observe(now, st.rtt, st.rtt, cwnd);
        c.link.set_rtt(st.rtt, st.rtt.saturating_sub(st.min_rtt).max(st.rtt / 8));
        // A link that has heard nothing for `T_trip` = max(t_trip, 3 x srtt), the outage probe's own wait, may be out
        // (6.5): it sends no Repair, Bulk or rides, whatever the budget says, so an outage probe's open window carries
        // only Protocol, Action, Pose and PINGs into it.
        let silent = self.cfg.t_trip.is_some_and(|t| now >= c.heard_at.plus(t.max(st.rtt * 3)));
        // The delay guard (4.3), for Bulk.
        let low = c.budget.low_lanes_may_send() && !silent;
        // Streams that waited for the peer's credit first (3.5).
        open_waiting(c);
        // Lanes out: datagrams while quinn has room (4.2), then streams.
        if c.open_send {
            let max_size = c.q.datagrams().max_size().unwrap_or(MIN_DGRAM);
            loop {
                let space = c.q.datagrams().send_buffer_space();
                let max = max_size.min(space);
                if max < 64 {
                    break;
                }
                let rides = c.budget.rides_on(now) && !silent;
                let Some(d) = c.link.poll_datagram(now, max, rides) else { break };
                let first = c.link.first_sent_wire - c.first_sent_seen;
                if first > 0 {
                    c.first_sent_seen = c.link.first_sent_wire;
                    c.budget.on_first_send(now, first);
                }
                if c.q.datagrams().send(Bytes::from(d), false).is_err() {
                    break;
                }
            }
            // A lane message leaves its lane's queue only once nothing waits for a stream, so at most one is outside
            // it, and the lane order and the delay guard still apply to the rest (3.5). Repair and Bulk streams not
            // yet fully handed to quinn (the delay guard holds Bulk mid-write) keep at most half the peer's stream
            // credit, so Protocol and Action always have the other half (both sides run one build, so the peer's
            // `N_streams_in` is ours; @design-reviewer's Low 1 on 9e71ac4c). Repair is not under the delay guard
            // (4.3): a guard that held it for `W_base` after a rise in delay failed every repair at n = 2 (K4 R4').
            let low_cap = (self.cfg.n_streams_in / 2).max(1) as usize; // at least 1, or a limit of 0-1 never sends low
            while c.unopened.is_empty()
                && let Some((lane, seq, bytes)) =
                    c.link.poll_stream(!silent && c.outbound.iter().filter(|o| o.2 < 2).count() < low_cap, low)
            {
                let mut b = vec![lane as u8];
                put_varint(&mut b, seq);
                b.extend_from_slice(&bytes);
                open_stream(c, 4 - lane as i32, b.into());
            }
        } else if let Some(d) = c.link.poll_datagram(now, MIN_DGRAM, false) {
            let _ = c.q.datagrams().send(Bytes::from(d), false); // control records only (6.4)
        }
        flush_streams(c, low, silent);
        // Packets out.
        loop {
            let mut buf = Vec::new();
            match c.q.poll_transmit(t, 1, &mut buf) {
                Some(tx) => self.out.push_back(Transmit { to: tx.destination, bytes: buf[..tx.size].to_vec() }),
                None => break,
            }
        }
        while let Some(ev) = c.q.poll_endpoint_events() {
            if let Some(back) = self.endpoint.handle_event(h, ev) {
                c.q.handle_event(back);
            }
        }
        if let Some(reason) = lost {
            self.on_lost(h, reason);
        }
        if self.conns.get(&h).is_some_and(|c| c.q.is_drained()) {
            self.conns.remove(&h);
        }
    }

    /// The handshake is confirmed: learn the peer's key, keep one connection per peer, send my hello.
    fn on_connected(&mut self, now: HostTime, t: Instant, h: ConnectionHandle, me: PeerId) {
        let member = self.cfg.member;
        let Some(c) = self.conns.get_mut(&h) else { return };
        let Some(peer) = peer_key(&c.q) else {
            close(c, t, &Refusal { code: CloseCode::Fault, detail: "no peer key".into() });
            return;
        };
        if let Role::Dialler { pin } = &c.role
            && *pin != peer
        {
            close(c, t, &Refusal { code: CloseCode::Fault, detail: "not the pinned key".into() });
            return;
        }
        c.peer = Some(peer);
        c.state = LinkState::Confirmed;
        c.member = member;
        c.confirmed_at = Some(now);
        let client = |c: &Conn| if c.q.side().is_client() { key(&me) } else { key(&peer) };
        let mine = client(c);
        let my_tag = dup_tag(&c.q);
        // A connection I've closed (a refusal, `T_welcome`, a guard) stands for nothing: a fresh one from the same
        // peer must never lose the contest to it, or it would be left neither refused nor welcomed.
        if let Some(&other) = self.peers.get(&peer)
            && other != h
            && let Some(o) = self.conns.get(&other)
            && !o.closed
        {
            // Two connections to one peer (6.2): keep the one whose client has the lower key. With the same
            // client (staggered dials), the lower duplicate tag, a TLS exporter value both sides derive alike;
            // "the first confirmed" could differ between the sides (@design-reviewer's Medium 2 on v1).
            let theirs = client(o);
            let lower = if mine == theirs { my_tag < dup_tag(&o.q) } else { mine < theirs };
            let (keep, drop) = if lower { (h, other) } else { (other, h) };
            if let Some(d) = self.conns.get_mut(&drop) {
                d.superseded = true;
                close(d, t, &Refusal { code: CloseCode::Left, detail: DUPLICATE.into() });
            }
            if keep == other {
                return;
            }
        }
        self.peers.insert(peer, h);
        let Some(c) = self.conns.get_mut(&h) else { return };
        let mut b = vec![K_HELLO];
        b.extend_from_slice(&c.hello.encode());
        open_stream(c, 10, b.into());
    }

    fn on_stream(&mut self, now: HostTime, t: Instant, h: ConnectionHandle, bytes: Vec<u8>) {
        let me = self.me();
        let Some(c) = self.conns.get_mut(&h) else { return };
        let Some(peer) = c.peer else { return };
        let member = c.member;
        let Some((&kind, body)) = bytes.split_first() else { return };
        match kind {
            K_HELLO => {
                if c.theirs.is_some() {
                    return fault(c, t, Some(peer), &mut self.lifecycle, "a second hello".into());
                }
                let theirs = match Hello::decode(body) {
                    Ok(x) => x,
                    Err(e) => return fault(c, t, Some(peer), &mut self.lifecycle, format!("bad hello: {e:?}")),
                };
                if let Some(refusal) = c.hello.guard(&theirs) {
                    close(c, t, &refusal);
                    self.lifecycle.push_back(Lifecycle::Refused { peer, refusal, by_me: true });
                    return;
                }
                c.theirs = Some(theirs);
                if member {
                    self.events.push(NetEvent::JoinRequest { peer, hello: Bytes::copy_from_slice(body) });
                }
                if let Some(w) = c.early_welcome.take() {
                    self.on_stream(now, t, h, w);
                }
            }
            K_WELCOME if c.theirs.is_none() && c.early_welcome.is_none() => c.early_welcome = Some(bytes),
            K_WELCOME => {
                let w = match Welcome::decode(body) {
                    Ok(w) if c.theirs.is_some() && !c.welcomed => w,
                    Ok(_) => return fault(c, t, Some(peer), &mut self.lifecycle, "an unexpected welcome".into()),
                    Err(e) => return fault(c, t, Some(peer), &mut self.lifecycle, format!("bad welcome: {e:?}")),
                };
                if w.line != c.hello.line {
                    let refusal = Refusal { code: CloseCode::Line, detail: "the welcome names another line".into() };
                    close(c, t, &refusal);
                    self.lifecycle.push_back(Lifecycle::Refused { peer, refusal, by_me: true });
                    return;
                }
                c.welcomed = true;
                if member {
                    // Between two members: my send side opens with my own admit, never before it (6.4,
                    // @design-reviewer's Medium on draft 1.6). A welcome before it is held.
                    c.open_send = c.admitted;
                } else {
                    // A joiner trusts the member that welcomed it: take its traffic in, and say so.
                    let Some(rate) = SimRate::new(w.rate_hz) else {
                        return fault(c, t, Some(peer), &mut self.lifecycle, "a welcome with a bad rate".into());
                    };
                    c.open_send = true;
                    c.open_recv = true;
                    open_stream(c, 10, Bytes::from_static(&[K_UP]));
                    // Converge on the members it names, until the start cut's list replaces them (5.4, 6.4).
                    // The mesh (6.7): dial every member the welcome names, as one would dial an invite.
                    for (k, addrs) in w.members.iter().filter(|(k, _)| *k != peer && *k != me) {
                        if !self.peers.contains_key(k) {
                            for a in addrs {
                                self.dials.push((now, *a, *k, Vec::new()));
                            }
                        }
                    }
                    if !self.clock.is_running() && self.pending_start.is_none() {
                        self.pending_start = Some((w.epoch_ns, rate));
                        self.clock.begin_joining(now, w.members.iter().map(|m| m.0).collect());
                        self.next_ping = now;
                        self.dirty = true;
                    }
                }
                update_open(c, &mut self.events);
                self.lifecycle.push_back(Lifecycle::Welcomed { peer, welcome: w });
            }
            K_UP => {
                // Only a joiner sends one, once, to the member that admitted it; joiners never send welcomes.
                if !member || !c.admitted || c.welcomed || c.got_up || !body.is_empty() {
                    return fault(c, t, Some(peer), &mut self.lifecycle, "an unexpected up record".into());
                }
                c.got_up = true;
                c.open_send = true;
                update_open(c, &mut self.events);
            }
            K_RENDEZVOUS => {
                // Only a member introduces (6.7), and only once the link is up: its word is what starts a dial.
                // A peer is a member if it welcomed me (only members send welcomes) or the app lists it.
                if !(c.open_send && c.open_recv) {
                    return fault(c, t, Some(peer), &mut self.lifecycle, "a rendezvous before the welcome".into());
                }
                if !(c.welcomed || self.members.contains(&peer)) {
                    return fault(c, t, Some(peer), &mut self.lifecycle, "a rendezvous from a non-member".into());
                }
                if now >= c.rdv.0.plus(T_RDV) {
                    c.rdv = (now, 0);
                }
                c.rdv.1 += 1;
                if c.rdv.1 > R_RDV {
                    return fault(c, t, Some(peer), &mut self.lifecycle, "too many rendezvous records".into());
                }
                let (key, addrs) = match decode_rendezvous(body) {
                    Ok(x) => x,
                    Err(e) => return fault(c, t, Some(peer), &mut self.lifecycle, format!("bad rendezvous: {e:?}")),
                };
                // One set of dials per key: a key I'm connected to or already dialling is skipped (two members may
                // both introduce the same pair, so it isn't a fault). At most `MAX_RDV_DIALS` dials wait at once.
                if key == me || self.peers.contains_key(&key) || self.dialling(&key) {
                    return;
                }
                let room = MAX_RDV_DIALS.saturating_sub(self.pending_dials());
                for a in addrs.into_iter().take(room) {
                    self.dials.push((now, a, key, Vec::new()));
                }
                self.dirty = true;
            }
            lane @ 0..=4 => {
                let mut r = Reader(body);
                let seq = match r.varint() {
                    Ok(s) => s,
                    Err(_) => return fault(c, t, Some(peer), &mut self.lifecycle, "a truncated stream".into()),
                };
                let Some(&lane) = Lane::ALL.get(usize::from(lane)) else { return };
                if lane == Lane::Pose {
                    return fault(c, t, Some(peer), &mut self.lifecycle, "a pose on a stream".into());
                }
                if !c.open_recv {
                    return fault(c, t, Some(peer), &mut self.lifecycle, "lane traffic before the welcome".into());
                }
                if let Some((lane, bytes)) = c.link.on_stream(lane, seq, Bytes::copy_from_slice(r.0)) {
                    self.received.push_back(Received { from: peer, lane, bytes, at: now });
                }
            }
            k => fault(c, t, Some(peer), &mut self.lifecycle, format!("unknown stream kind {k}")),
        }
    }

    /// Test and fuzz hook (feature `testing`): send `record`, a stream kind byte and its body, to `peer` as is.
    #[cfg(feature = "testing")]
    pub fn testing_send_record(&mut self, peer: &PeerId, record: Vec<u8>) {
        if let Some(c) = self.peers.get(peer).and_then(|h| self.conns.get_mut(h)) {
            open_stream(c, 10, record.into());
            self.dirty = true;
        }
    }

    /// A dial to `key` is queued or under way.
    fn dialling(&self, key: &PeerId) -> bool {
        self.dials.iter().any(|d| d.2 == *key)
            || self
                .conns
                .values()
                .any(|c| !c.closed && c.peer.is_none() && matches!(&c.role, Role::Dialler { pin } if pin == key))
    }

    /// Dials that have no invite or reply behind them (the mesh and rendezvous), queued or under way.
    fn pending_dials(&self) -> usize {
        self.dials.iter().filter(|d| d.3.is_empty()).count()
            + self
                .conns
                .values()
                .filter(|c| {
                    !c.closed && c.peer.is_none() && c.hello.token.is_empty() && matches!(c.role, Role::Dialler { .. })
                })
                .count()
    }

    fn on_lost(&mut self, h: ConnectionHandle, reason: ConnectionError) {
        let now = self.now;
        let Some(c) = self.conns.get_mut(&h) else { return };
        let was = c.state;
        // A Degraded link was up: its loss is a loss, not a refusal.
        let was_up = matches!(was, LinkState::Up | LinkState::Degraded);
        let addrs = addresses(c);
        c.state = LinkState::Down;
        if c.superseded {
            return;
        }
        let peer = c.peer.or(match &c.role {
            Role::Dialler { pin } => Some(*pin),
            Role::Acceptor => None,
        });
        let Some(peer) = peer else { return };
        // Quiet only if another connection to the peer stands in for this one (6.2): it already holds my map entry,
        // or this one never did and another is live. A peer's close saying "duplicate" proves nothing on its own
        // (@design-reviewer's Medium on v1b): with no other connection it is an ordinary loss.
        let primary = self.peers.get(&peer) == Some(&h);
        let other_live = self.conns.iter().any(|(oh, o)| {
            *oh != h && !o.closed && (o.peer == Some(peer) || matches!(&o.role, Role::Dialler { pin } if *pin == peer))
        });
        if !primary && (other_live || self.peers.contains_key(&peer)) {
            return;
        }
        if primary {
            self.peers.remove(&peer);
            self.clock.forget(&peer);
        }
        // The peer kept another connection that is still opening here: its duplicate close is not a refusal.
        let duplicate = matches!(&reason, ConnectionError::ApplicationClosed(a)
            if a.error_code.into_inner() == CloseCode::Left as u64 && a.reason.as_ref() == DUPLICATE.as_bytes());
        // A member's link that was up (6.5): closed for silence, with no close from either side, it is redialled;
        // closed any other way, it is lost. A peer's close of a redial attempt (a refusal, not a duplicate and not
        // `Overloaded`) ends the redial: its app no longer takes this key.
        if primary && was_up && self.members.contains(&peer) {
            // A duplicate close of the only link isn't meant to end it either: the peer holds another connection
            // to me, which comes up or is lost in turn.
            let silent = duplicate || matches!(reason, ConnectionError::TimedOut | ConnectionError::Reset);
            match self.cfg.redial.filter(|_| silent && self.cfg.member) {
                Some(p) => {
                    let r = Redial { addrs, until: now.plus(p.window), next: now, gap: p.first };
                    self.lifecycle.push_back(Lifecycle::Redialling { peer, until: r.until });
                    self.redials.insert(peer, r);
                    self.dirty = true;
                }
                None => {
                    self.end_redial(now, peer);
                    self.lifecycle.push_back(Lifecycle::Lost { peer });
                }
            }
        } else if matches!(&reason, ConnectionError::ApplicationClosed(a)
                if a.error_code.into_inner() != CloseCode::Overloaded as u64)
            && !duplicate
            && self.redials.contains_key(&peer)
        {
            // `Overloaded` isn't a refusal of this key: the peer's app didn't decide within `T_welcome`, as when it is
            // busy catching up after the outage, so the redial carries on at its next gap (@chunks-reviewer's Low).
            self.end_redial(now, peer);
            self.lifecycle.push_back(Lifecycle::Lost { peer });
        }
        if !was_up && !(duplicate && other_live) {
            // A close before the link was up counts as a refusal (9.4), unless I closed it myself (and said so).
            let refusal = match reason {
                ConnectionError::ApplicationClosed(a) => Some(Refusal {
                    code: CloseCode::from_u64(a.error_code.into_inner()).unwrap_or(CloseCode::Fault),
                    detail: String::from_utf8_lossy(&a.reason).into_owned(),
                }),
                ConnectionError::LocallyClosed => None,
                other => Some(Refusal { code: CloseCode::Fault, detail: other.to_string() }),
            };
            if let Some(refusal) = refusal {
                self.lifecycle.push_back(Lifecycle::Refused { peer, refusal, by_me: false });
            }
        }
        if was != LinkState::Connecting {
            self.events.push(NetEvent::LinkDown { peer });
        }
    }
}

impl Conn {
    fn new(q: Connection, role: Role, hello: Hello, lanes: LaneParams, budget: BudgetParams) -> Self {
        let mut link = Link::new(lanes.clone());
        link.set_open(false, false);
        Self {
            q,
            role,
            peer: None,
            state: LinkState::Connecting,
            link,
            budget: Budget::new(budget),
            first_sent_seen: 0,
            hello,
            theirs: None,
            member: false,
            confirmed_at: None,
            admitted: false,
            welcomed: false,
            got_up: false,
            introduced: false,
            rdv: (HostTime(i64::MIN), 0),
            closed: false,
            rx_seen: 0,
            heard_at: HostTime(i64::MIN),
            open_send: false,
            open_recv: false,
            superseded: false,
            inbound: HashMap::new(),
            outbound: VecDeque::new(),
            unopened: VecDeque::new(),
            early_welcome: None,
            limits: (lanes.l_msg, lanes.l_bulk),
        }
    }
}

/// A peer's addresses for a rendezvous: what its hello lists, and where its packets come from.
fn addresses(c: &Conn) -> Vec<SocketAddr> {
    let mut v = c.theirs.as_ref().map(|h| h.addrs.clone()).unwrap_or_default();
    let seen = c.q.remote_address();
    if !v.contains(&seen) {
        v.insert(0, seen);
    }
    v.truncate(crate::lifecycle::MAX_ADDRS);
    v
}

/// The duplicate tag (6.2): a TLS exporter value, the same on both sides of a connection and unique to it.
fn dup_tag(q: &Connection) -> [u8; 16] {
    let mut o = [0xff; 16];
    // rustls exports from any TLS 1.3 session once the handshake is complete, which it is here. Were it ever to
    // fail, both tags would be all-ones and the rule would fall back to keeping the connection already held.
    let ok = q.crypto_session().export_keying_material(&mut o, b"pe-net duplicate", b"").is_ok();
    debug_assert!(ok, "TLS exporter failed");
    o
}

fn peer_key(q: &Connection) -> Option<PeerId> {
    let certs =
        q.crypto_session().peer_identity()?.downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>().ok()?;
    peer_id_of(certs.first()?.as_ref()).map(MemberKey)
}

fn close(c: &mut Conn, t: Instant, r: &Refusal) {
    c.closed = true;
    c.q.close(t, VarInt::from_u32(r.code as u32), Bytes::from(r.detail.clone().into_bytes()));
}

fn fault(c: &mut Conn, t: Instant, peer: Option<PeerId>, out: &mut VecDeque<Lifecycle>, what: String) {
    close(c, t, &Refusal { code: CloseCode::Fault, detail: what.clone() });
    if let Some(peer) = peer {
        out.push_back(Lifecycle::Fault { peer, what });
    }
}

fn update_open(c: &mut Conn, events: &mut Vec<NetEvent>) {
    c.link.set_open(c.open_send, c.open_recv);
    if c.open_send && c.open_recv && c.state != LinkState::Up {
        c.state = LinkState::Up;
        // `T_welcome` covers only the way to the first Up: a later Degraded is not a failed welcome (6.4, 6.5;
        // @design-reviewer's High on v1).
        c.confirmed_at = None;
        if let Some(peer) = c.peer {
            events.push(NetEvent::LinkUp { peer });
        }
    }
}

/// Open a stream at `priority`: the lanes at `4 - lane` (Protocol highest), the lifecycle records at 10. At the
/// peer's stream limit the message waits in `unopened`, behind every message of the same or a higher priority.
fn open_stream(c: &mut Conn, priority: i32, bytes: Bytes) {
    let at = c.unopened.iter().position(|(p, _)| *p < priority).unwrap_or(c.unopened.len());
    c.unopened.insert(at, (priority, bytes));
    open_waiting(c);
}

/// Open streams for the waiting messages, in order, while the peer's stream credit lasts.
fn open_waiting(c: &mut Conn) {
    while let Some((priority, bytes)) = c.unopened.pop_front() {
        let Some(id) = c.q.streams().open(Dir::Uni) else {
            c.unopened.push_front((priority, bytes));
            break;
        };
        let _ = c.q.send_stream(id).set_priority(priority);
        c.outbound.push_back((id, bytes, priority));
    }
}

/// Hand quinn what it takes now; finish each stream when its bytes are all in. Bulk streams wait while the delay
/// guard is closed (`low` false), even halfway through; Repair's don't (4.3). Both wait on a silent link (6.5).
fn flush_streams(c: &mut Conn, low: bool, silent: bool) {
    let mut i = 0;
    while i < c.outbound.len() {
        let (id, rest, priority) = &mut c.outbound[i];
        if (*priority < 1 && !low) || (*priority < 2 && silent) {
            i += 1;
            continue;
        }
        let mut s = c.q.send_stream(*id);
        match s.write(rest) {
            Ok(n) => {
                let _ = rest.split_to(n);
            }
            Err(_) => {
                i += 1;
                continue;
            }
        }
        if rest.is_empty() {
            let _ = s.finish();
            c.outbound.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Read what has arrived on a stream: `Some` when it is finished (or broke a rule).
fn read_stream(c: &mut Conn, id: StreamId) -> Option<Result<Vec<u8>, String>> {
    let lanes_msg = c.limits;
    let (buf, limit) = c.inbound.entry(id).or_insert_with(|| (Vec::new(), usize::MAX));
    let mut rs = c.q.recv_stream(id);
    let mut chunks = rs.read(true).ok()?;
    let mut done = None;
    loop {
        match chunks.next(usize::MAX) {
            Ok(Some(ch)) => {
                buf.extend_from_slice(&ch.bytes);
                if *limit == usize::MAX && !buf.is_empty() {
                    *limit = match buf[0] {
                        K_HELLO => 1 + MAX_HELLO,
                        K_WELCOME => 1 + MAX_WELCOME,
                        K_UP => 1,
                        K_RENDEZVOUS => 1 + MAX_RENDEZVOUS,
                        0 | 1 => 1 + 8 + lanes_msg.0,
                        3 | 4 => 1 + 8 + lanes_msg.1,
                        k => {
                            done = Some(Err(format!("unknown stream kind {k}")));
                            break;
                        }
                    };
                }
                if buf.len() > *limit {
                    done = Some(Err("a stream over its bound".into()));
                    break;
                }
            }
            Ok(None) => {
                done = Some(Ok(std::mem::take(buf)));
                break;
            }
            Err(quinn_proto::ReadError::Blocked) => break,
            Err(e) => {
                done = Some(Err(format!("stream reset: {e}")));
                break;
            }
        }
    }
    let _ = chunks.finalize();
    if done.is_some() {
        c.inbound.remove(&id);
    }
    done
}

impl Transport for Core {
    /// Queue a message. A link that isn't there drops it, as a down link does (S4).
    fn send(&mut self, to: PeerId, lane: Lane, bytes: Bytes) {
        let Some(c) = self.peers.get(&to).and_then(|h| self.conns.get_mut(h)) else { return };
        let max = c.q.datagrams().max_size().unwrap_or(MIN_DGRAM);
        if c.link.send(lane, bytes, max).is_err() {
            self.events.push(NetEvent::QueueFull { lane });
        }
        self.dirty = true;
    }

    fn poll(&mut self) -> Option<Received> {
        self.received.pop_front()
    }

    fn link(&self, peer: PeerId) -> LinkStats {
        let Some(c) = self.peers.get(&peer).and_then(|h| self.conns.get(h)) else { return LinkStats::default() };
        let s = c.q.stats();
        let srtt = s.path.rtt.as_nanos() as u64;
        let (queued, oldest) = c.link.queued(self.now);
        LinkStats {
            state: c.state,
            srtt_ns: srtt,
            min_rtt_ns: s.path.min_rtt.as_nanos() as u64,
            budget_bps: c.budget.bps(),
            loss: if s.path.sent_packets == 0 { 0.0 } else { s.path.lost_packets as f32 / s.path.sent_packets as f32 },
            queued,
            oldest_unacked_ns: oldest.map(|d| d.as_nanos() as u64),
        }
    }

    fn events(&mut self) -> Vec<NetEvent> {
        std::mem::take(&mut self.events)
    }
}
