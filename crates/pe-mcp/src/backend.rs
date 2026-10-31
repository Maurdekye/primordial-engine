//! What the MCP server needs from a peer. Phase 2 implements it over the session simulator
//! (`sim_backend`); P2-3's `pe-session` implements it for a live peer (P2-10), with no change to
//! the tools.
//!
//! A backend lives on the engine thread only (`engine.rs`), so it need not be `Send`.

pub use pe_session::session::EntryId;

use crate::model::*;

/// One proposed interaction, as validated input for the backend.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Interaction {
    Touch { object: u32, impulse: [f32; 3], grab: bool },
    Hand { object: u32, target_mm: [i32; 3], active: bool },
}

/// A backend's receipt for an authored entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Authored {
    pub entry: EntryId,
    pub stamp: u64,
    pub seq: u32,
}

/// The verdict on an entry, as the driven peer currently sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryStatus {
    pub stamp: u64,
    pub state: InteractionState,
    pub reason: Option<String>,
    pub by_member: Option<MemberId>,
    pub until_frame: Option<u64>,
    pub is_final: bool,
}

/// Counters a step report is computed from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub rollbacks: u64,
    pub resim_frames: u64,
}

/// The peer the agent drives: its member's view of the session. Everything here is something a
/// live peer knows locally.
pub trait DrivenPeer {
    fn scenario_label(&self) -> String;
    fn backend_name(&self) -> &'static str;
    fn me(&self) -> MemberId;
    fn world(&self) -> u32;
    fn members(&self) -> Vec<MemberId>;
    fn scene_label(&self) -> String;
    fn dt_ms(&self) -> f64;
    fn time_ms(&self) -> u64;
    fn frame(&self) -> u64;
    fn conf(&self) -> u64;
    fn object_count(&self) -> u32;
    fn state(&self, offset: u32, limit: u32) -> WorldState;
    fn object(&self, object: u32) -> ToolResult<ObjectState>;
    /// Author an interaction at the present frame, through the same path a player's input takes.
    fn propose(&mut self, i: Interaction) -> ToolResult<Authored>;
    /// The current verdict on one of this peer's own entries; None if it holds no such entry.
    fn status(&self, entry: EntryId) -> Option<EntryStatus>;
    /// Advance the peer's clock by `ms` milliseconds of (virtual) time.
    fn advance_ms(&mut self, ms: u64) -> ToolResult<()>;
    fn counters(&self) -> Counters;
    fn paused(&self) -> bool;
    /// Protocol events logged so far (the backend keeps them in order).
    fn events_since(&self, n: usize) -> (Vec<String>, usize);
    fn hashes(&self, from: u64, to: u64) -> Hashes;
    fn member_info(&self) -> Members;
    fn sync(&self) -> SyncStatus;
    /// The simulator's extra controls; None for a live peer.
    fn scenario(&mut self) -> Option<&mut dyn ScenarioControl>;
    /// A live peer's housekeeping (network, clock), called by the engine thread between MCP calls
    /// (`EngineHandle::spawn_ticking`). The simulator needs none: its time moves only in `advance_ms`.
    fn tick(&mut self) {}
}

/// Test-bench controls only the simulator has (they reach into other members and the network).
pub trait ScenarioControl {
    fn fault(&mut self, p: &FaultParams) -> ToolResult<FaultReport>;
    /// The oracle: every live replica against the driven peer and an offline replay.
    fn check(&self) -> CheckReport;
}
