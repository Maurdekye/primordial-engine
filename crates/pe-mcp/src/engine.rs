//! The engine thread. It owns the driven peer; MCP handlers on the async runtime send it closures
//! and await the replies, so the sim is touched by one thread only (as the real peer's sim thread
//! will be). A panic inside the sim marks the session faulted instead of killing the server.

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;

use tokio::sync::oneshot;

use crate::auth::Principal;
use crate::backend::*;
use crate::model::*;
use crate::sim_backend::SimSession;

/// At most this many of one principal's interactions may be unresolved (not final) at once.
pub const MAX_PENDING: usize = 64;
const MAX_EVENTS: usize = 50;

#[derive(Default)]
struct Book {
    next: u64,
    /// ticket -> (entry id, last status reported to this principal)
    tickets: BTreeMap<u64, (u64, Option<InteractionStatus>)>,
    events_seen: usize,
}

pub struct Engine {
    backend: Option<Box<dyn DrivenPeer>>,
    /// Whether the backend is the simulator. Kept apart from `backend`, which is None after a panic:
    /// a faulted live peer must still refuse `scenario_load` (review of 358cfb7, finding 3).
    simulated: bool,
    faulted: Option<String>,
    /// Per principal name, so a rotated token for the same principal keeps its tickets.
    books: BTreeMap<String, Book>,
}

fn status_of(ticket: u64, entry: u64, s: EntryStatus) -> InteractionStatus {
    InteractionStatus {
        ticket,
        entry: format!("{entry:016x}"),
        stamp: s.stamp,
        state: s.state,
        reason: s.reason,
        by_member: s.by_member,
        until_frame: s.until_frame,
        is_final: s.is_final,
    }
}

impl Engine {
    pub fn new(mut backend: Box<dyn DrivenPeer>) -> Engine {
        let simulated = backend.scenario().is_some();
        Engine { backend: Some(backend), simulated, faulted: None, books: BTreeMap::new() }
    }

    pub fn with_default_scenario() -> Engine {
        Engine::new(Box::new(SimSession::load(&LoadParams::default()).expect("default scenario")))
    }

    fn peer(&self) -> ToolResult<&dyn DrivenPeer> {
        match (&self.backend, &self.faulted) {
            (Some(b), None) => Ok(b.as_ref()),
            (_, f) => Err(ToolError::new(
                ErrorCode::SessionFaulted,
                format!("the session stopped: {}; call scenario_load to start a fresh one", f.as_deref().unwrap_or("no session")),
            )),
        }
    }

    fn peer_mut(&mut self) -> ToolResult<&mut Box<dyn DrivenPeer>> {
        self.peer()?;
        Ok(self.backend.as_mut().unwrap())
    }

    pub fn info(&self, who: &Principal) -> SessionInfo {
        let scopes = who.scopes.iter().copied().collect();
        match self.peer() {
            Ok(p) => SessionInfo {
                scenario: p.scenario_label(),
                backend: p.backend_name().into(),
                principal: who.name.clone(),
                scopes,
                me: p.me(),
                world: p.world(),
                members: p.members(),
                scene: p.scene_label(),
                object_count: p.object_count(),
                dt_ms: p.dt_ms(),
                time_ms: p.time_ms(),
                frame: p.frame(),
                conf: p.conf(),
                faulted: None,
            },
            Err(_) => SessionInfo {
                scenario: String::new(),
                backend: String::new(),
                principal: who.name.clone(),
                scopes,
                me: 0,
                world: 0,
                members: vec![],
                scene: String::new(),
                object_count: 0,
                dt_ms: 0.0,
                time_ms: 0,
                frame: 0,
                conf: 0,
                faulted: Some(self.faulted.clone().unwrap_or_default()),
            },
        }
    }

    pub fn state(&self, p: &GetStateParams) -> ToolResult<WorldState> {
        let limit = p.limit.unwrap_or(16);
        if limit > 256 {
            return Err(ToolError::arg("limit", format!("limit must be in 0..=256, got {limit}")));
        }
        Ok(self.peer()?.state(p.offset.unwrap_or(0), limit))
    }

    pub fn object(&self, p: &ObjectParams) -> ToolResult<ObjectState> {
        self.peer()?.object(p.object)
    }

    pub fn propose(&mut self, who: &Principal, p: &ProposeParams) -> ToolResult<Proposed> {
        self.peer()?;
        let book = self.books.entry(who.name.clone()).or_default();
        let backend = self.backend.as_ref().unwrap();
        // A ticket reported final stays final (confirmed frames are never undone), so only the
        // others need a fresh look.
        let pending = book
            .tickets
            .values()
            .filter(|(e, last)| !last.as_ref().is_some_and(|l| l.is_final) && backend.status(*e).is_none_or(|s| !s.is_final))
            .count();
        if pending >= MAX_PENDING {
            return Err(ToolError::new(ErrorCode::RateLimited, format!("{pending} of your interactions are unresolved (limit {MAX_PENDING}); step until they are final"))
                .with_details(serde_json::json!({ "pending": pending, "limit": MAX_PENDING })));
        }
        let i = match p.action {
            InteractionAction::Touch | InteractionAction::Grab => {
                if p.target_mm.is_some() || p.active.is_some() {
                    return Err(ToolError::arg("target_mm", "target_mm and active apply to action \"hand\" only"));
                }
                let imp = p.impulse.ok_or_else(|| ToolError::arg("impulse", "impulse is required for touch and grab"))?;
                if imp.iter().any(|x| !x.is_finite()) {
                    return Err(ToolError::arg("impulse", "impulse must be three finite numbers"));
                }
                let mag = imp.iter().map(|x| x * x).sum::<f64>().sqrt();
                if mag > 100.0 {
                    return Err(ToolError::arg("impulse", format!("impulse magnitude {mag:.1} exceeds 100 N*s")));
                }
                Interaction::Touch {
                    object: p.object,
                    impulse: [imp[0] as f32, imp[1] as f32, imp[2] as f32],
                    grab: p.action == InteractionAction::Grab,
                }
            }
            InteractionAction::Hand => {
                if p.impulse.is_some() {
                    return Err(ToolError::arg("impulse", "impulse applies to touch and grab only"));
                }
                let t = p.target_mm.ok_or_else(|| ToolError::arg("target_mm", "target_mm is required for hand"))?;
                if t.iter().any(|v| v.abs() > 100_000) {
                    return Err(ToolError::arg("target_mm", "each target_mm coordinate must be within +-100000"));
                }
                Interaction::Hand { object: p.object, target_mm: [t[0] as i32, t[1] as i32, t[2] as i32], active: p.active.unwrap_or(true) }
            }
        };
        let a = self.backend.as_mut().unwrap().propose(i)?;
        let book = self.books.get_mut(&who.name).unwrap();
        book.next += 1;
        let ticket = book.next;
        book.tickets.insert(ticket, (a.entry, None));
        Ok(Proposed {
            ticket,
            entry: format!("{:016x}", a.entry),
            stamp: a.stamp,
            seq: a.seq,
            note: "resolved when its frame is simulated; final once confirmed (world_interaction_status, world_step)".into(),
        })
    }

    pub fn ticket(&mut self, who: &Principal, p: &TicketParams) -> ToolResult<InteractionStatus> {
        let peer = self.peer()?;
        let unknown = || ToolError::new(ErrorCode::UnknownTicket, format!("no interaction with ticket {}", p.ticket));
        let book = self.books.get(&who.name).ok_or_else(unknown)?;
        let &(entry, _) = book.tickets.get(&p.ticket).ok_or_else(unknown)?;
        let st = peer.status(entry).ok_or_else(|| ToolError::new(ErrorCode::Internal, "the peer no longer holds this entry"))?;
        let s = status_of(p.ticket, entry, st);
        self.books.get_mut(&who.name).unwrap().tickets.get_mut(&p.ticket).unwrap().1 = Some(s.clone());
        Ok(s)
    }

    pub fn step(&mut self, who: &Principal, p: &StepParams) -> ToolResult<StepReport> {
        let ms = match (p.ms, p.frames) {
            (Some(ms), None) => {
                if !(1..=10_000).contains(&ms) {
                    return Err(ToolError::arg("ms", format!("ms must be in 1..=10000, got {ms}")));
                }
                ms
            }
            (None, Some(f)) => {
                if !(1..=600).contains(&f) {
                    return Err(ToolError::arg("frames", format!("frames must be in 1..=600, got {f}")));
                }
                // Members tick once per millisecond, at now_us, now_us + 1000, ... The clock frame is
                // that of the last tick; advance it by exactly f.
                let peer = self.peer()?;
                let dt_us = (peer.dt_ms() * 1000.0).round() as u64;
                let now_us = peer.time_ms() * 1000;
                let clock = now_us.saturating_sub(1000) / dt_us;
                let target_us = (clock + f) * dt_us;
                target_us.saturating_sub(now_us).div_ceil(1000) + 1
            }
            _ => return Err(ToolError::arg("ms", "give exactly one of ms and frames")),
        };
        let before = self.peer()?.counters();
        let from_frame = self.peer()?.frame();
        self.peer_mut()?.advance_ms(ms)?;
        self.peer()?;
        let peer = self.backend.as_deref().unwrap();
        let after = peer.counters();
        let book = self.books.entry(who.name.clone()).or_default();
        let mut changed = Vec::new();
        for (&t, (entry, last)) in book.tickets.iter_mut() {
            if last.as_ref().is_some_and(|l| l.is_final) {
                continue;
            }
            if let Some(st) = peer.status(*entry) {
                let s = status_of(t, *entry, st);
                if last.as_ref() != Some(&s) {
                    *last = Some(s.clone());
                    changed.push(s);
                }
            }
        }
        let (mut events, total) = peer.events_since(book.events_seen);
        book.events_seen = total;
        if events.len() > MAX_EVENTS {
            events.drain(..events.len() - MAX_EVENTS);
        }
        Ok(StepReport {
            from_frame,
            frame: peer.frame(),
            conf: peer.conf(),
            time_ms: peer.time_ms(),
            rollbacks: after.rollbacks - before.rollbacks,
            resim_frames: after.resim_frames - before.resim_frames,
            paused: peer.paused(),
            changed,
            events,
        })
    }

    pub fn hashes(&self, p: &HashesParams) -> ToolResult<Hashes> {
        if p.to <= p.from {
            return Err(ToolError::arg("to", "to must be greater than from"));
        }
        if p.to - p.from > 600 {
            return Err(ToolError::arg("to", "at most 600 frames per call"));
        }
        Ok(self.peer()?.hashes(p.from, p.to))
    }

    /// Every object, for the `primordial://world/state` resource (no page limit).
    pub fn state_all(&self) -> ToolResult<WorldState> {
        Ok(self.peer()?.state(0, u32::MAX))
    }

    pub fn members(&self) -> ToolResult<Members> {
        Ok(self.peer()?.member_info())
    }

    pub fn sync(&self) -> ToolResult<SyncStatus> {
        Ok(self.peer()?.sync())
    }

    /// Replace the session. Allowed while faulted (it is the way out).
    pub fn load(&mut self, who: &Principal, p: &LoadParams) -> ToolResult<SessionInfo> {
        if !self.simulated {
            return Err(ToolError::new(ErrorCode::NoScenarioControl, "this peer is live, not the simulator"));
        }
        let s = SimSession::load(p)?;
        self.backend = Some(Box::new(s));
        self.faulted = None;
        self.books.clear();
        Ok(self.info(who))
    }

    pub fn fault(&mut self, p: &FaultParams) -> ToolResult<FaultReport> {
        let b = self.peer_mut()?;
        b.scenario().ok_or_else(|| ToolError::new(ErrorCode::NoScenarioControl, "this peer is live, not the simulator"))?.fault(p)
    }

    pub fn check(&mut self) -> ToolResult<CheckReport> {
        let b = self.peer_mut()?;
        Ok(b.scenario().ok_or_else(|| ToolError::new(ErrorCode::NoScenarioControl, "this peer is live, not the simulator"))?.check())
    }

    /// Let a live backend do its housekeeping. A panic faults the session, as in `call`.
    pub fn tick(&mut self) {
        if self.faulted.is_some() {
            return;
        }
        let Some(b) = self.backend.as_mut() else { return };
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| b.tick())) {
            self.fault_with(panic);
        }
    }

    fn fault_with(&mut self, panic: Box<dyn std::any::Any + Send>) -> String {
        let msg = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".into());
        self.backend = None;
        self.faulted = Some(format!("internal error: {msg}"));
        msg
    }

    /// Test hook: what a panic inside the sim does.
    #[doc(hidden)]
    pub fn panic_for_test(&mut self) {
        panic!("injected test panic");
    }
}

type Job = Box<dyn FnOnce(&mut Engine) + Send>;

#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::Sender<Job>,
}

impl EngineHandle {
    /// Start the engine thread. `init` runs on it, so the backend never crosses threads.
    pub fn spawn(init: impl FnOnce() -> Engine + Send + 'static) -> EngineHandle {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("pe-mcp-engine".into())
            .spawn(move || {
                let mut engine = init();
                while let Ok(job) = rx.recv() {
                    job(&mut engine);
                }
            })
            .expect("spawn engine thread");
        EngineHandle { tx }
    }

    /// Start the engine thread for a live backend: between jobs it calls `Engine::tick` at least every
    /// `period`, so the peer keeps its network and clock going with or without MCP traffic.
    pub fn spawn_ticking(init: impl FnOnce() -> Engine + Send + 'static, period: std::time::Duration) -> EngineHandle {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("pe-engine".into())
            .spawn(move || {
                let mut engine = init();
                let mut next = std::time::Instant::now();
                loop {
                    let wait = next.saturating_duration_since(std::time::Instant::now());
                    match rx.recv_timeout(wait) {
                        Ok(job) => job(&mut engine),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    if std::time::Instant::now() >= next {
                        engine.tick();
                        next = std::time::Instant::now() + period;
                    }
                }
            })
            .expect("spawn engine thread");
        EngineHandle { tx }
    }

    /// Run `f` on the engine thread and return its result.
    pub async fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut Engine) -> ToolResult<R> + Send + 'static) -> ToolResult<R> {
        let (rtx, rrx) = oneshot::channel();
        let job: Job = Box::new(move |e: &mut Engine| {
            let r = match catch_unwind(AssertUnwindSafe(|| f(e))) {
                Ok(r) => r,
                Err(panic) => {
                    let msg = e.fault_with(panic);
                    Err(ToolError::new(ErrorCode::Internal, format!("the engine panicked: {msg}; the session is now faulted")))
                }
            };
            let _ = rtx.send(r);
        });
        self.tx.send(job).map_err(|_| ToolError::new(ErrorCode::Internal, "engine thread is gone"))?;
        rrx.await.map_err(|_| ToolError::new(ErrorCode::Internal, "engine thread is gone"))?
    }
}
