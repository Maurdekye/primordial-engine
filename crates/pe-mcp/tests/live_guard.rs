//! A live peer (no scenario control) keeps refusing scenario tools, also after a panic has faulted
//! the session (review of 358cfb7, finding 3).

mod common;

use common::*;
use pe_mcp::backend::*;
use pe_mcp::engine::{Engine, EngineHandle};
use pe_mcp::model::*;
use serde_json::json;

/// The smallest DrivenPeer with no simulator behind it. `advance_ms` panics, to fault the session;
/// so does `tick` when `tick_panics` is set.
struct FakeLive {
    tick_panics: bool,
}

impl DrivenPeer for FakeLive {
    fn scenario_label(&self) -> String {
        "live".into()
    }
    fn backend_name(&self) -> &'static str {
        "fake-live"
    }
    fn me(&self) -> MemberId {
        1
    }
    fn world(&self) -> u32 {
        1
    }
    fn members(&self) -> Vec<MemberId> {
        vec![1, 2]
    }
    fn scene_label(&self) -> String {
        "none".into()
    }
    fn dt_ms(&self) -> f64 {
        1000.0 / 60.0
    }
    fn time_ms(&self) -> u64 {
        0
    }
    fn frame(&self) -> u64 {
        0
    }
    fn conf(&self) -> u64 {
        0
    }
    fn object_count(&self) -> u32 {
        0
    }
    fn state(&self, _: u32, _: u32) -> WorldState {
        WorldState { frame: 0, conf: 0, state_hash: None, object_count: 0, objects: vec![], claims: vec![], next_offset: None }
    }
    fn object(&self, o: u32) -> ToolResult<ObjectState> {
        Err(ToolError::new(ErrorCode::UnknownObject, format!("no live object {o}")))
    }
    fn propose(&mut self, _: Interaction) -> ToolResult<Authored> {
        Err(ToolError::new(ErrorCode::UnknownObject, "no objects"))
    }
    fn status(&self, _: EntryId) -> Option<EntryStatus> {
        None
    }
    fn advance_ms(&mut self, _: u64) -> ToolResult<()> {
        panic!("fake live peer: injected panic");
    }
    fn counters(&self) -> Counters {
        Counters::default()
    }
    fn paused(&self) -> bool {
        false
    }
    fn events_since(&self, _: usize) -> (Vec<String>, usize) {
        (vec![], 0)
    }
    fn hashes(&self, _: u64, _: u64) -> Hashes {
        Hashes { conf: 0, frames: vec![], checks: vec![] }
    }
    fn member_info(&self) -> Members {
        Members { me: 1, epoch: 0, members: vec![], suspicions: 0, demotions: 0, view_changes: 0, resets: 0, equivocations: 0, lying_votes: 0, decisions: vec![] }
    }
    fn sync(&self) -> SyncStatus {
        SyncStatus {
            frame: 0,
            now: 0,
            conf: 0,
            speculation: 0,
            window: 120,
            paused: false,
            caught_up: true,
            checks_ok: 0,
            checks_bad: 0,
            latest_check: None,
            rollbacks: 0,
            max_rollback_depth: 0,
            resim_frames: 0,
            paused_frames: 0,
            nonfinite: 0,
        }
    }
    fn scenario(&mut self) -> Option<&mut dyn ScenarioControl> {
        None
    }
    fn tick(&mut self) {
        if self.tick_panics {
            panic!("fake live peer: tick panic");
        }
    }
}

#[tokio::test]
async fn live_peer_refuses_scenario_tools_before_and_after_a_fault() {
    let e = EngineHandle::spawn(|| Engine::new(Box::new(FakeLive { tick_panics: false })));
    let c = client(&e).await;
    assert_eq!(ok(&c, "session_info", json!({})).await["backend"], "fake-live");
    for t in ["scenario_load", "scenario_check"] {
        err(&c, t, json!({}), "no_scenario_control").await;
    }
    err(&c, "scenario_fault", json!({ "fault": "heal" }), "no_scenario_control").await;
    // The step panics inside the backend: the session faults...
    err(&c, "world_step", json!({ "ms": 1 }), "internal").await;
    err(&c, "world_get_state", json!({}), "session_faulted").await;
    // ...and scenario_load still refuses to swap the simulator in.
    err(&c, "scenario_load", json!({}), "no_scenario_control").await;
    err(&c, "world_get_state", json!({}), "session_faulted").await;
}

/// A panic in a ticking engine's housekeeping faults it, and `faulted()` says why, so the host (pe-app's
/// run loop) can exit instead of waiting forever (review of 2f9ca05, medium).
#[tokio::test]
async fn a_panic_in_tick_is_reported_by_faulted() {
    let e = EngineHandle::spawn_ticking(|| Engine::new(Box::new(FakeLive { tick_panics: true })), std::time::Duration::from_millis(1));
    let mut why = None;
    for _ in 0..200 {
        why = e.call(|en| Ok(en.faulted().map(str::to_string))).await.unwrap();
        if why.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(why.as_deref().is_some_and(|w| w.contains("tick panic")), "{why:?}");
}
