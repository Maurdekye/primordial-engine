//! Seam S4, transport: lanes and delivery (transport.md 3, 4.4, 5.7; @shader-lang's shape).
//!
//! - pe-agree, pe-diverge and pe-world give pe-net opaque bytes and never see QUIC. pe-net never decodes a payload.
//! - **No order** between messages on any lane; receivers order by what the bytes say. **Once:** duplicates are
//!   dropped. **Per link:** one connection, one budget, one set of queues per peer.
//! - A link that goes down drops what is queued for it, and the caller gets [`NetEvent::LinkDown`]. pe-net only
//!   reports a slow link; it removes and demotes nobody.
//! - **The frame clock** ([`FrameClock`]) is pe-net's: a small model the simulation thread reads without crossing a
//!   thread (transport.md 5.7). Sans-IO cores get it as an argument.
//!
//! **Provisional in v0:** [`LinkStats`]' fields and [`NetEvent`]'s join request (its hello and welcome types are
//! pe-net's until WS-app and WS-gov need them at a seam).

use bytes::Bytes;

use crate::ids::{Frame, MemberKey};
use crate::time::HostTime;

/// A peer on the network: its Ed25519 public key, which is the member's signing key (transport.md 2.2).
pub type PeerId = MemberKey;

/// The five lanes, in priority order (transport.md 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Lane {
    /// pe-agree's messages: proposals, votes, cuts, joins and leaves. Reliable, each once, any order.
    Protocol = 0,
    /// Signed entries: inputs, hand entries, edits, governance. Reliable, each once, any order.
    Action = 1,
    /// Display-only poses that are not entries. Newest only: an unsent pose is superseded, never queued.
    Pose = 2,
    /// pe-diverge's four repair messages. Reliable, each once, any order.
    Repair = 3,
    /// Blobs fetched by hash; state for a newcomer. Reliable, lowest priority.
    Bulk = 4,
}

impl Lane {
    pub const ALL: [Lane; 5] = [Lane::Protocol, Lane::Action, Lane::Pose, Lane::Repair, Lane::Bulk];
}

/// A delivered message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub from: PeerId,
    pub lane: Lane,
    pub bytes: Bytes,
    /// When it arrived, on this machine's clock.
    pub at: HostTime,
}

/// A link's state (transport.md 6.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LinkState {
    #[default]
    Connecting,
    /// The handshake is confirmed.
    Confirmed,
    Welcomed,
    Up,
    /// Nothing heard for `T_degraded`.
    Degraded,
    Down,
}

/// Per peer: what pe-net knows of the link (transport.md 4.4).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkStats {
    pub state: LinkState,
    /// Smoothed round-trip time, in nanoseconds.
    pub srtt_ns: u64,
    pub min_rtt_ns: u64,
    /// The rate budget, in bytes per second.
    pub budget_bps: u64,
    /// The fraction of packets lost, 0 to 1.
    pub loss: f32,
    /// Bytes queued per lane, in [`Lane`] order.
    pub queued: [u64; 5],
    /// The age of the oldest unacknowledged Protocol and Action messages, in nanoseconds.
    pub oldest_unacked_ns: [u64; 2],
}

/// What pe-net reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetEvent {
    LinkUp {
        peer: PeerId,
    },
    LinkDegraded {
        peer: PeerId,
    },
    /// The link went down; what was queued for it is dropped.
    LinkDown {
        peer: PeerId,
    },
    /// The oldest unacknowledged Action message is older than `T_saturated`.
    LinkSaturated {
        peer: PeerId,
    },
    /// Saturation cleared.
    LinkOk {
        peer: PeerId,
    },
    /// A connecting peer asks to be admitted; the app decides from the agreed governance (transport.md 6.3). The
    /// hello's bytes are pe-net's.
    JoinRequest {
        peer: PeerId,
        hello: Bytes,
    },
    /// A local queue was full: a local fault, reported, never a stall.
    QueueFull {
        lane: Lane,
    },
}

/// S4. pe-net implements it over QUIC; the harness over its in-process network.
pub trait Transport {
    /// Never blocks. What each lane promises is in [`Lane`].
    fn send(&mut self, to: PeerId, lane: Lane, bytes: Bytes);
    fn poll(&mut self) -> Option<Received>;
    fn link(&self, peer: PeerId) -> LinkStats;
    /// Drain the events since the last call.
    fn events(&mut self) -> Vec<NetEvent>;
}

/// The session's frame clock: pe-net's clock model (transport.md 5.1, 5.7), steered gradually and published for
/// lock-free reads. The harness gives its own, driven by each node's local clock with an offset and a drift.
pub trait FrameClock: Send + Sync {
    /// The world's simulation rate, in frames per second. Fixed for the session in M1, a world setting (60 Hz by
    /// default) within a supported range, and independent of the display rate.
    fn rate_hz(&self) -> u32;
    /// The session frame at this machine's host time; `None` before frame 0 (the clock is converging).
    fn frame_at(&self, now: HostTime) -> Option<Frame>;
    /// The host time at which `frame` starts on this machine.
    fn host_time_of(&self, frame: Frame) -> HostTime;
}
