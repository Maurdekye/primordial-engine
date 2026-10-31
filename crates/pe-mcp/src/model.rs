//! Tool inputs, outputs and errors. Every type here derives `JsonSchema`: the schemas the server
//! advertises in `tools/list` are generated from these definitions (design 17.5), and
//! `schema/tools.json` is the committed copy.
//!
//! Conventions: frames are the peer's 60 Hz sim frames; `conf` is exclusive (every frame below it is
//! confirmed and can never be rolled back); hashes are 16 hex digits; positions are metres.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub type MemberId = u32;

/// What a token may do (design 17.2 scopes, narrowed to phase 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Read state, hashes, membership and sync status.
    Read,
    /// Propose interactions as the driven peer's member.
    Interact,
    /// Test-bench control: advance the clock, load scenarios, inject faults, run the oracle check.
    Control,
}

impl Scope {
    pub const ALL: [Scope; 3] = [Scope::Read, Scope::Interact, Scope::Control];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Interact => "interact",
            Scope::Control => "control",
        }
    }

    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "read" => Some(Scope::Read),
            "interact" => Some(Scope::Interact),
            "control" => Some(Scope::Control),
            _ => None,
        }
    }
}

// ---- errors --------------------------------------------------------------------------------------

/// Machine-readable error codes (design 17.5). A tool error is returned as a tool result with
/// `isError: true` and this object as `structuredContent`; malformed JSON-RPC and unknown tools are
/// JSON-RPC errors instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// An argument is missing, out of range or inconsistent; `field` names it.
    InvalidArgument,
    /// No live object with that index.
    UnknownObject,
    /// No interaction with that ticket was proposed by this principal.
    UnknownTicket,
    /// The token lacks the scope this tool needs; `details.need` names it.
    Forbidden,
    /// Too many of this principal's interactions are still unresolved.
    RateLimited,
    /// The object belongs to another member (a ball-pit hand is moved only by its own member).
    NotOwner,
    /// The driven peer cannot author right now: it is paused at its speculation limit or behind
    /// its clock. Step the clock and retry.
    NotReady,
    /// The driven peer has crashed (a scenario fault). Load a new scenario.
    PeerCrashed,
    /// This backend has no scenario control (a live peer, not the simulator).
    NoScenarioControl,
    /// The session hit an internal error and stopped; `scenario_load` starts a fresh one.
    SessionFaulted,
    /// An unexpected engine failure.
    Internal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ToolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ToolError { code, message: message.into(), field: None, details: None }
    }
    pub fn arg(field: &str, message: impl Into<String>) -> Self {
        ToolError { code: ErrorCode::InvalidArgument, message: message.into(), field: Some(field.into()), details: None }
    }
    pub fn with_details(mut self, d: serde_json::Value) -> Self {
        self.details = Some(d);
        self
    }
}

pub type ToolResult<T> = Result<T, ToolError>;

// ---- shared output pieces ------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ClaimInfo {
    pub object: u32,
    pub member: MemberId,
    /// The claim holds for frames below this one.
    pub until_frame: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct FrameHash {
    pub frame: u64,
    pub hash: String,
}

// ---- session_info --------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    /// Human-readable scenario label.
    pub scenario: String,
    /// What implements the peer: "session-sim" (the phase-1 simulator) until P2-3 lands.
    pub backend: String,
    /// The principal this token maps to (design 17.1).
    pub principal: String,
    pub scopes: Vec<Scope>,
    /// The member id the driven peer authors as.
    pub me: MemberId,
    pub world: u32,
    pub members: Vec<MemberId>,
    pub scene: String,
    pub object_count: u32,
    /// Frame duration in milliseconds.
    pub dt_ms: f64,
    /// Virtual time since the session started, in milliseconds.
    pub time_ms: u64,
    /// The driven peer's simulated frame.
    pub frame: u64,
    /// Every frame below this is confirmed.
    pub conf: u64,
    /// Set once the session has stopped on an internal error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faulted: Option<String>,
}

// ---- world_get_state -----------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct GetStateParams {
    /// First object index to list (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
    /// How many objects to list, 0..=256 (default 16).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ObjectSummary {
    pub object: u32,
    pub kind: String,
    /// World position in metres (f64: Box3D positions are double precision, D58).
    pub position: [f64; 3],
    pub linear_velocity: [f32; 3],
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorldState {
    pub frame: u64,
    pub conf: u64,
    /// Hash of the state after the newest simulated frame (absent before the first frame).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_hash: Option<String>,
    /// Number of object slots, live or gone.
    pub object_count: u32,
    pub objects: Vec<ObjectSummary>,
    pub claims: Vec<ClaimInfo>,
    /// Pass as `offset` to read the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u32>,
}

// ---- world_get_object ----------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ObjectParams {
    /// Object index (see world_get_state).
    pub object: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ObjectState {
    pub object: u32,
    /// Session-wide id: the birth world in the high 32 bits.
    pub global_id: String,
    /// box, sphere, capsule, ball or hand. Hands are kinematic and move only by `hand` interactions.
    pub kind: String,
    /// World position in metres (f64: Box3D positions are double precision, D58).
    pub position: [f64; 3],
    /// Unit quaternion, [x, y, z, w].
    pub rotation: [f32; 4],
    pub linear_velocity: [f32; 3],
    pub angular_velocity: [f32; 3],
    /// Hash of the object's exact bit state.
    pub state_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimInfo>,
}

// ---- world_propose_interaction -------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InteractionAction {
    /// One impulse; claims the object once (other members' touches are refused while it holds).
    Touch,
    /// An impulse that is a held grab: renews the claim.
    Grab,
    /// Move one of the kinematic hands (ball-pit scenes) to `target_mm`. The target is held (sent
    /// again at every pose slot, as a held controller would) until a new target or `active: false`.
    Hand,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProposeParams {
    pub object: u32,
    pub action: InteractionAction,
    /// touch and grab: impulse in N*s, three finite numbers, magnitude at most 100.
    #[serde(default)]
    pub impulse: Option<[f64; 3]>,
    /// hand: target position in millimetres, each within +-100000.
    #[serde(default)]
    pub target_mm: Option<[i64; 3]>,
    /// hand: false releases the held target and parks the hand (default true).
    #[serde(default)]
    pub active: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Proposed {
    /// Pass to world_interaction_status.
    pub ticket: u64,
    /// The signed entry's id.
    pub entry: String,
    /// The canonical frame the entry is stamped with.
    pub stamp: u64,
    pub seq: u32,
    pub note: String,
}

// ---- world_interaction_status --------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TicketParams {
    pub ticket: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InteractionState {
    /// Its frame has not been simulated yet.
    Pending,
    Accepted,
    Rejected,
    /// Withdrawn: the entry broke a chain rule or was cut by an eviction.
    Void,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct InteractionStatus {
    pub ticket: u64,
    pub entry: String,
    pub stamp: u64,
    pub state: InteractionState,
    /// For rejected: claimed, cooldown, gone, duplicate, portal_closed, no_proof, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For rejected because claimed: the claim holder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_member: Option<MemberId>,
    /// For claimed or cooldown rejections: when the obstacle ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_frame: Option<u64>,
    /// True once the stamp is confirmed: the state can no longer change. Before that a rollback
    /// can still turn an acceptance into a rejection or the reverse.
    #[serde(rename = "final")]
    pub is_final: bool,
}

// ---- world_step ----------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct StepParams {
    /// Advance virtual time by this many milliseconds, 1..=10000. Give exactly one of ms, frames.
    #[serde(default)]
    pub ms: Option<u64>,
    /// Advance by this many frames, 1..=600.
    #[serde(default)]
    pub frames: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StepReport {
    pub from_frame: u64,
    pub frame: u64,
    pub conf: u64,
    pub time_ms: u64,
    /// Rollbacks and re-simulated frames on the driven peer during this step.
    pub rollbacks: u64,
    pub resim_frames: u64,
    /// The driven peer stopped at its speculation limit (frame - conf reached the window).
    pub paused: bool,
    /// This principal's interactions whose status changed during the step.
    pub changed: Vec<InteractionStatus>,
    /// Protocol events the driven peer logged (votes, evictions, check results), newest last, at
    /// most 50.
    pub events: Vec<String>,
}

// ---- world_hashes --------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct HashesParams {
    /// First frame (inclusive).
    pub from: u64,
    /// Last frame (exclusive); at most 600 frames per call.
    pub to: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Hashes {
    pub conf: u64,
    /// State hash after each simulated frame in the range.
    pub frames: Vec<FrameHash>,
    /// Canonical full-state check hashes in the range (compared with other members' acks).
    pub checks: Vec<FrameHash>,
}

// ---- session_members -----------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Eviction {
    /// The member's entries above this seq were cut.
    pub cut_seq: u32,
    /// The first frame the member no longer counts for.
    pub from_frame: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MemberInfo {
    pub member: MemberId,
    pub me: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted: Option<Eviction>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Members {
    pub me: MemberId,
    pub epoch: u32,
    pub members: Vec<MemberInfo>,
    /// Members this peer presumed crashed and voted to remove.
    pub suspicions: u64,
    pub demotions: u64,
    /// View-change decisions this peer applied (design 10.8), resets included.
    pub view_changes: u64,
    /// Decisions that removed no one: a vote overruled for a member heard from again.
    pub resets: u64,
    pub equivocations: u64,
    pub lying_votes: u64,
    /// Every decision this peer applied, oldest first.
    pub decisions: Vec<Decision>,
}

/// One view-change decision, as the driven peer applied it.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Decision {
    /// The epoch the decision started.
    pub epoch: u32,
    /// The peer's clock frame when it applied the decision.
    pub frame: u64,
    /// The view-change round that decided it.
    pub round: u32,
    /// The members it removed (empty: a reset or a join).
    pub removed: Vec<MemberId>,
}

// ---- session_sync_status -------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SyncStatus {
    pub frame: u64,
    /// The peer's clock frame (its present).
    pub now: u64,
    pub conf: u64,
    /// now - conf: how far the peer speculates past the confirmed frame.
    pub speculation: u64,
    /// The speculation limit, in frames.
    pub window: u64,
    pub paused: bool,
    pub caught_up: bool,
    /// Check hashes that matched / differed from other members' (a mismatch means divergence).
    pub checks_ok: u64,
    pub checks_bad: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_check: Option<FrameHash>,
    pub rollbacks: u64,
    pub max_rollback_depth: u64,
    pub resim_frames: u64,
    pub paused_frames: u64,
    /// Check frames at which some body held a non-finite value.
    pub nonfinite: u64,
}

// ---- scenario_load -------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SceneChoice {
    /// A pile of boxes, spheres and capsules: size x size x 2 objects.
    Pile,
    /// A walled pit of small balls plus one kinematic hand per member; `size` = ball count.
    Ballpit,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct LoadParams {
    /// Session members, 2..=8 (default 4). Members are numbered 1..=members.
    #[serde(default)]
    pub members: Option<u32>,
    /// The member the agent drives (default 1).
    #[serde(default)]
    pub me: Option<u32>,
    /// Default pile.
    #[serde(default)]
    pub scene: Option<SceneChoice>,
    /// pile: side length 1..=8 (default 4); ballpit: balls 1..=500 (default 100).
    #[serde(default)]
    pub size: Option<u32>,
    /// One-way link delay, 0..=1000 ms (default 20).
    #[serde(default)]
    pub delay_ms: Option<u64>,
    /// Link jitter, 0..=500 ms (default 0).
    #[serde(default)]
    pub jitter_ms: Option<u64>,
    /// Datagram loss, 0..=50 percent (default 0).
    #[serde(default)]
    pub loss_pct: Option<u32>,
    /// Touches per second each other member makes on random objects, 0..=30 (default 0).
    #[serde(default)]
    pub background_touches_per_s: Option<u32>,
    /// Background touches are grabs (default false). Only a grab claims its object, for 0.5 s, and
    /// only a grab can be refused for a claim (design D72).
    #[serde(default)]
    pub background_grabs: Option<bool>,
    /// Speculation limit in frames, 30..=600 (default 120).
    #[serde(default)]
    pub window: Option<u64>,
    /// Session seed (default fixed): the same parameters and seed give the same run.
    #[serde(default)]
    pub seed: Option<u64>,
}

// ---- scenario_fault ------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    /// The member stops receiving, ticking and sending, for good.
    Crash,
    /// Set the link between `from` and `to` (both directions unless `one_way`).
    Link,
    /// Drop every datagram between members of different `groups`.
    Partition,
    /// Remove the partition (crashes are permanent; links stay as set).
    Heal,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FaultParams {
    pub fault: FaultKind,
    /// crash: the member.
    #[serde(default)]
    pub member: Option<u32>,
    /// link: the two ends.
    #[serde(default)]
    pub from: Option<u32>,
    #[serde(default)]
    pub to: Option<u32>,
    #[serde(default)]
    pub one_way: Option<bool>,
    #[serde(default)]
    pub delay_ms: Option<u64>,
    #[serde(default)]
    pub jitter_ms: Option<u64>,
    #[serde(default)]
    pub loss_pct: Option<u32>,
    /// partition: disjoint groups covering some members; a member in no group is alone.
    #[serde(default)]
    pub groups: Option<Vec<Vec<u32>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FaultReport {
    pub applied: String,
    /// Every fault in force.
    pub active: Vec<String>,
}

// ---- scenario_check ------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Divergence {
    pub frame: u64,
    pub member: MemberId,
    pub hash: Option<String>,
    pub reference: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CheckReport {
    /// Frames 0..frames were compared: the lowest confirmed frame among live members.
    pub frames: u64,
    pub live_members: Vec<MemberId>,
    pub crashed_members: Vec<MemberId>,
    /// Every live member's hashes equal the driven peer's for every compared frame.
    pub replicas_equal: bool,
    /// The driven peer's hashes equal an offline replay of its final log.
    pub replay_equal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_divergence: Option<Divergence>,
}
