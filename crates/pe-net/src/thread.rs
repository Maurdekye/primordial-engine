//! The network thread (transport.md 7): it owns the UDP socket and a [`Core`], waits on the socket until the
//! Core's next timeout, feeds it what arrives, and sends what it returns.
//!
//! - **To and from the simulation thread:** bounded queues. [`NetHandle`] is the simulation thread's end and
//!   implements seam S4's `Transport`; nothing on it blocks. A full outgoing queue is a local fault, reported as
//!   `QueueFull`, never a stall.
//! - **Waking:** after queueing a command, the handle sends an empty datagram to the thread's socket from a
//!   loopback socket of its own, so the thread doesn't sleep through new work. The Core never sees empty
//!   datagrams.
//! - **Link statistics** come back as messages every `STATS_EVERY`, so no lock is shared.
//! - **The control end:** [`NetHandle::control`] splits off a [`NetControl`] that issues commands and takes the
//!   lifecycle's outcomes (6.3-6.6) on a channel of their own, so the handle itself can live on as the
//!   simulation's `Transport` while the app's network code keeps admission and dialling (app.md 3). Lifecycle
//!   outcomes are not ordered relative to the handle's messages and events. If the control end is dropped,
//!   later outcomes are discarded (`Fault`s too); the thread runs on.
//! - **A bounded lifecycle backlog:** `Refused` and `Fault` are triggered by whoever connects, so outcomes nobody
//!   polls are held up to `queue` beyond the channel, then the oldest `Refused` or `Fault` goes first
//!   (@design-reviewer). Every drop is counted (`lifecycle_dropped`).
//! - **Time** is the app's host clock (seam S5), passed in, so pe-net and the simulation agree on `HostTime`.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use pe_net_crypto::identity::{Invite, Reply};
use pe_seams::{HostTime, Lane, LinkStats, NetEvent, PeerId, Received, Transport};

use crate::core::{Core, Lifecycle};
use crate::lifecycle::{Refusal, Welcome};

/// How often link statistics come back.
const STATS_EVERY: Duration = Duration::from_millis(100);
/// The longest wait on the socket, so statistics stay fresh.
const MAX_WAIT: Duration = Duration::from_millis(100);

/// What the simulation thread asks of the network thread.
#[derive(Debug)]
pub enum Command {
    Send { to: PeerId, lane: Lane, bytes: Bytes },
    Dial(Invite),
    TakeReply(Reply),
    Admit { peer: PeerId, welcome: Welcome },
    Refuse { peer: PeerId, refusal: Refusal },
    SetMember(bool),
    StartFirstMember { epoch_ns: i64, rate: pe_seams::SimRate },
    SetClockMembers(Vec<PeerId>),
    Stop,
}

/// What comes back on the handle's queue (lifecycle outcomes have their own, see [`NetControl`]).
#[derive(Debug)]
pub enum Output {
    Received(Received),
    Event(NetEvent),
    Stats(PeerId, LinkStats),
}

/// The host clock, shared with the simulation (seam S5).
pub type HostClock = Arc<dyn Fn() -> HostTime + Send + Sync>;

/// Called on the network thread when it has queued messages or events for the handle, once per batch, so the
/// simulation thread can wake rather than wait out its frame (app.md 3 step 7). It must be cheap and never block.
pub type Arrival = Arc<dyn Fn() + Send + Sync>;

/// The simulation thread's end.
pub struct NetHandle {
    /// The frame clock the simulation reads without crossing a thread (5.7).
    pub frame_clock: Arc<crate::clock::SharedClock>,
    waker: Waker,
    out: Receiver<Output>,
    /// The lifecycle channel, until [`NetHandle::control`] takes it.
    life: Option<Receiver<Lifecycle>>,
    life_dropped: Arc<AtomicU64>,
    join: Option<JoinHandle<io::Result<()>>>,
    events: Vec<NetEvent>,
    /// Messages `events` took off the queue, which `poll` serves first: reliable lanes deliver each message once,
    /// so the handle never drops one. A caller that only calls `events` (a join loop) thus takes messages past the
    /// channel's backpressure: this grows at the incoming rate, bounded in practice by the join window and the
    /// peer's link budget (3.3).
    received: VecDeque<Received>,
    stats: BTreeMap<PeerId, LinkStats>,
}

/// Queues a command and wakes the thread.
struct Waker {
    cmd: SyncSender<Command>,
    socket: UdpSocket,
    to: SocketAddr,
}

impl Waker {
    fn command(&self, c: Command) -> bool {
        match self.cmd.try_send(c) {
            Ok(()) => {
                let _ = self.socket.send_to(&[], self.to);
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }

    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self { cmd: self.cmd.clone(), socket: self.socket.try_clone()?, to: self.to })
    }
}

/// The app's control end of the network thread, split off the handle by [`NetHandle::control`]: it issues
/// commands (dial, admit, refuse, clock members) and takes the lifecycle's outcomes. `Send`, so it can live on
/// whichever thread decides admission.
pub struct NetControl {
    waker: Waker,
    life: Receiver<Lifecycle>,
    life_dropped: Arc<AtomicU64>,
}

impl NetControl {
    /// Queue a command and wake the thread. `false` if the queue is full (a local fault).
    pub fn command(&mut self, c: Command) -> bool {
        self.waker.command(c)
    }

    /// The lifecycle's next outcome (6.3-6.6), if any.
    pub fn poll_lifecycle(&mut self) -> Option<Lifecycle> {
        self.life.try_recv().ok()
    }

    /// Lifecycle outcomes dropped so far because nobody took them (the backlog's cap).
    pub fn lifecycle_dropped(&self) -> u64 {
        self.life_dropped.load(Ordering::Relaxed)
    }
}

/// Hold an outcome for the lifecycle channel, at most `cap` of them: when full, the oldest `Refused` or `Fault`
/// (which any connecting peer can cause) goes first, else the oldest of all. Each drop is counted.
fn hold(backlog: &mut VecDeque<Lifecycle>, l: Lifecycle, cap: usize, dropped: &AtomicU64) {
    if backlog.len() >= cap.max(1) {
        let i = backlog.iter().position(|o| matches!(o, Lifecycle::Refused { .. } | Lifecycle::Fault { .. }));
        backlog.remove(i.unwrap_or(0));
        dropped.fetch_add(1, Ordering::Relaxed);
    }
    backlog.push_back(l);
}

impl NetHandle {
    /// Start the network thread on `socket`. `queue` bounds each direction, in messages (sized for `T_queue` of
    /// traffic, 10).
    pub fn spawn(core: Core, socket: UdpSocket, clock: HostClock, queue: usize) -> io::Result<Self> {
        Self::spawn_with(core, socket, clock, queue, None)
    }

    /// As [`NetHandle::spawn`], calling `arrival` whenever messages or events reach the handle's queue.
    pub fn spawn_with(
        core: Core,
        socket: UdpSocket,
        clock: HostClock,
        queue: usize,
        arrival: Option<Arrival>,
    ) -> io::Result<Self> {
        let frame_clock = core.frame_clock();
        let local = socket.local_addr()?;
        let wake_to = match local.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), local.port()),
            IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(Ipv6Addr::LOCALHOST.into(), local.port()),
            _ => local,
        };
        let waker = UdpSocket::bind(SocketAddr::new(
            if wake_to.is_ipv4() { Ipv4Addr::LOCALHOST.into() } else { Ipv6Addr::LOCALHOST.into() },
            0,
        ))?;
        let (cmd, cmd_rx) = sync_channel(queue);
        let (out_tx, out) = sync_channel(queue);
        let (life_tx, life) = sync_channel(queue);
        let life_dropped = Arc::new(AtomicU64::new(0));
        let dropped = life_dropped.clone();
        let join = std::thread::Builder::new()
            .name("pe-net".into())
            .spawn(move || run(core, socket, clock, cmd_rx, (out_tx, arrival), (life_tx, queue, dropped)))?;
        Ok(Self {
            frame_clock,
            waker: Waker { cmd, socket: waker, to: wake_to },
            out,
            life: Some(life),
            life_dropped,
            join: Some(join),
            events: Vec::new(),
            received: VecDeque::new(),
            stats: BTreeMap::new(),
        })
    }

    /// Queue a command and wake the thread. `false` if the queue is full (a local fault).
    pub fn command(&mut self, c: Command) -> bool {
        self.waker.command(c)
    }

    /// The lifecycle's next outcome (6.3-6.6), while the handle still has the lifecycle channel; `None` after
    /// [`NetHandle::control`] has taken it.
    pub fn poll_lifecycle(&mut self) -> Option<Lifecycle> {
        self.life.as_ref()?.try_recv().ok()
    }

    /// Split off the control end: commands, and from now on the lifecycle's outcomes. `None` if it was taken
    /// already. The handle keeps its own command path (it still sends) and stays the `Transport`.
    pub fn control(&mut self) -> io::Result<Option<NetControl>> {
        let Some(life) = self.life.take() else { return Ok(None) };
        Ok(Some(NetControl { waker: self.waker.try_clone()?, life, life_dropped: self.life_dropped.clone() }))
    }

    /// Lifecycle outcomes dropped so far because nobody took them (the backlog's cap).
    pub fn lifecycle_dropped(&self) -> u64 {
        self.life_dropped.load(Ordering::Relaxed)
    }

    /// Take outputs off the queue until a message arrives (`until_received`) or none are left. Without
    /// `until_received`, messages are kept, in order, for `poll`.
    fn pump(&mut self, until_received: bool) -> Option<Received> {
        if until_received && let Some(r) = self.received.pop_front() {
            return Some(r);
        }
        while let Ok(o) = self.out.try_recv() {
            match o {
                Output::Received(r) if until_received => return Some(r),
                Output::Received(r) => self.received.push_back(r),
                Output::Event(e) => self.events.push(e),
                Output::Stats(p, s) => {
                    self.stats.insert(p, s);
                }
            }
        }
        None
    }

    /// Stop the thread and wait for it: it closes every connection first (6.5).
    pub fn stop(mut self) -> io::Result<()> {
        self.stop_thread()
    }

    fn stop_thread(&mut self) -> io::Result<()> {
        let Some(join) = self.join.take() else { return Ok(()) };
        let _ = self.waker.cmd.send(Command::Stop);
        let _ = self.waker.socket.send_to(&[], self.waker.to);
        match join.join() {
            Ok(r) => r,
            Err(_) => Err(io::Error::other("the network thread panicked")),
        }
    }
}

/// A handle dropped without `stop` (pe-app's transport, dropped with the simulation) stops the thread the same way,
/// so its peers see the connections close rather than wait out `T_idle` (the first two-machine run).
impl Drop for NetHandle {
    fn drop(&mut self) {
        let _ = self.stop_thread();
    }
}

impl Transport for NetHandle {
    fn send(&mut self, to: PeerId, lane: Lane, bytes: Bytes) {
        if !self.command(Command::Send { to, lane, bytes }) {
            self.events.push(NetEvent::QueueFull { lane });
        }
    }

    fn poll(&mut self) -> Option<Received> {
        self.pump(true)
    }

    /// The latest statistics taken in by `poll` or `events` (they come every `STATS_EVERY`; `link` itself takes
    /// `&self` and can't drain the queue).
    fn link(&self, peer: PeerId) -> LinkStats {
        self.stats.get(&peer).cloned().unwrap_or_default()
    }

    fn events(&mut self) -> Vec<NetEvent> {
        self.pump(false);
        std::mem::take(&mut self.events)
    }
}

/// Close every connection with code 0 (`Left`) and send the closes before the socket goes (6.5), so each peer
/// sees the link go down at once rather than after `T_idle`.
fn shut(core: &mut Core, socket: &UdpSocket, now: HostTime) -> io::Result<()> {
    core.close_all(now);
    while let Some(t) = core.poll_transmit() {
        let _ = socket.send_to(&t.bytes, t.to);
    }
    Ok(())
}

fn run(
    mut core: Core,
    socket: UdpSocket,
    clock: HostClock,
    cmds: Receiver<Command>,
    (out, arrival): (SyncSender<Output>, Option<Arrival>),
    (life, life_cap, life_dropped): (SyncSender<Lifecycle>, usize, Arc<AtomicU64>),
) -> io::Result<()> {
    let mut buf = vec![0u8; 65_536];
    let mut backlog: VecDeque<Output> = VecDeque::new();
    let mut life_backlog: VecDeque<Lifecycle> = VecDeque::new();
    let mut life_open = true;
    let mut next_stats = clock();
    loop {
        let now = clock();
        while let Ok(c) = cmds.try_recv() {
            match c {
                Command::Send { to, lane, bytes } => core.send(to, lane, bytes),
                Command::Dial(i) => core.dial(now, &i),
                Command::TakeReply(r) => core.take_reply(now, &r),
                Command::Admit { peer, welcome } => core.admit(now, peer, &welcome),
                Command::Refuse { peer, refusal } => core.refuse(now, peer, refusal),
                Command::SetMember(m) => core.set_member(m),
                Command::StartFirstMember { epoch_ns, rate } => core.start_first_member(now, epoch_ns, rate),
                Command::SetClockMembers(m) => core.set_clock_members(m),
                Command::Stop => return shut(&mut core, &socket, now),
            }
        }
        if core.poll_timeout().is_some_and(|t| t <= now) {
            core.handle_timeout(now);
        }
        while let Some(t) = core.poll_transmit() {
            // A send error (an unreachable address, a full buffer) is a lost packet: QUIC recovers.
            let _ = socket.send_to(&t.bytes, t.to);
        }
        while let Some(r) = core.poll() {
            backlog.push_back(Output::Received(r));
        }
        backlog.extend(core.events().into_iter().map(Output::Event));
        while let Some(l) = core.poll_lifecycle() {
            if life_open {
                hold(&mut life_backlog, l, life_cap, &life_dropped);
            }
        }
        // Lifecycle outcomes wait for a full channel; with the control end gone, they are discarded.
        while let Some(l) = life_backlog.pop_front() {
            match life.try_send(l) {
                Ok(()) => {}
                Err(TrySendError::Full(l)) => {
                    life_backlog.push_front(l);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    life_open = false;
                    life_backlog.clear();
                }
            }
        }
        if now >= next_stats {
            next_stats = now.plus(STATS_EVERY);
            for p in core.peers() {
                backlog.push_back(Output::Stats(p, core.link(p)));
            }
        }
        // A full outgoing queue waits here: the simulation thread is behind, and nothing is dropped.
        let mut arrived = false;
        while let Some(o) = backlog.pop_front() {
            let news = matches!(o, Output::Received(_) | Output::Event(_));
            match out.try_send(o) {
                Ok(()) => arrived |= news,
                Err(TrySendError::Full(o)) => {
                    backlog.push_front(o);
                    break;
                }
                // The handle is gone without a stop (its `Drop` stops first, so this is a backstop).
                Err(TrySendError::Disconnected(_)) => return shut(&mut core, &socket, now),
            }
        }
        if arrived && let Some(a) = &arrival {
            a();
        }
        let wait = core
            .poll_timeout()
            .map_or(MAX_WAIT, |t| Duration::from_nanos(u64::try_from(t.0 - now.0).unwrap_or(0)))
            .clamp(Duration::from_micros(500), MAX_WAIT);
        socket.set_read_timeout(Some(wait))?;
        match socket.recv_from(&mut buf) {
            Ok((0, _)) => {} // a wake-up
            Ok((n, from)) => core.handle_datagram(clock(), from, &buf[..n]),
            // Timeouts, and on Windows an ICMP port-unreachable surfacing as a reset: nothing to read.
            Err(_) => {}
        }
    }
}
