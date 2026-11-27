//! A deliberate test fault on this machine only, for the M1 demo's step 7 (docs/refound-plan.md, M1;
//! @item:primordial-engine/pe-app-inject-fault-at-a-runtime-flag-that-alter; @coordinator-opus). Two flags arm it, and they exclude each other:
//! - `--arm-fault`: the console's typed `fault` fires it at the next frame the simulation steps, `F`. Without the flag
//!   the command is refused. The runbook uses this one, so the user fires it when the demo reaches step 7.
//! - `--inject-fault-at <F>`: it fires at session frame `F`, fixed at start.
//!
//! What it does once `F` is set:
//! - **What changes:** at the end of frame `F`, one ball's radius doubles in this machine's world (its `Shape` row):
//!   the highest dynamic sphere then, the lowest id among equals. The view draws it larger, and its leaf in the hash
//!   tree differs from every other machine's. Its physics body stays as it was, so it moves as before.
//! - **Every time frame `F` is stepped:** live, in a re-simulation, in rung 1's replay and in a root's re-step, as on
//!   a machine that computes `F` wrongly. At n = 2 every divergence is a root split (divergence.md 5), whose first
//!   step is rung 1 on every member: a fault that happened once would end there, and rung 2 would never run.
//! - **The self-test fails by injection:** from the fault's first firing, this machine reports its physics self-test
//!   `Failed` ([`crate::diverge`]), and the console and the bundles it writes say that the failure is injected. At
//!   n = 2 with both self-tests passing, which member repairs is an arbitrary pick (divergence.md 5, step 3). With
//!   it, the other member is the reference, and this machine's rung 2 walks the tree to the ball and imports the
//!   other's record of it.
//! - **Nothing is sent:** the log and the other machines are unchanged. The console says so when the flag is armed
//!   and when the fault fires.
//!
//! It is a runtime flag and never a cargo feature, so the shipped build meets the feature policy
//! (scripts/feature_policy.txt). Every replica pe-app makes is over [`FaultWorld`]. Without an armed fault it passes
//! each call to pe-replica's `PeWorld` and changes nothing.

use std::any::Any;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use pe_replica::world::{
    BodyKind, BodyRow, BodyStateRow, ClaimRow, Effects, HandRow, LogicFx, PhysicsSettings, PlayerRow, Unbuildable,
};
use pe_replica::{PeWorld, Phase, ScriptRunner, WorldModel};
use pe_seams::{
    Activation, CutImport, DomainId, Frame, Hash, ImportError, MemberKey, ObjectId, Outcome, Pos, SignedEntry, Stage,
    Tree, ViewObject,
};
use pe_world::World;
use pe_world::components::{Shape, shape_kind};

/// This process's armed fault: main.rs arms it from `--arm-fault` or `--inject-fault-at`, before any replica is made.
/// It belongs to the first session only (app.md 4.1, D5): the first session change disarms it ([`disarm`]), so no
/// later session's replica or simulation takes it again.
static ARMED: std::sync::Mutex<Option<Arc<Fault>>> = std::sync::Mutex::new(None);
/// The fault was disarmed: the console's `fault` says it ended, rather than that none was armed.
static ENDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Arms this process's fault, and returns the console's line saying so. For main.rs only, once, before any replica or
/// simulation is made: every replica and simulation made afterwards takes it ([`armed`]). Tests never arm it; they
/// give a [`FaultWorld`] and a simulation their own [`Fault`].
pub fn arm(fault: Arc<Fault>) -> Result<String, String> {
    let line = fault.armed_line();
    let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
    if armed.is_some() || ENDED.load(std::sync::atomic::Ordering::Acquire) {
        return Err("a fault is armed already".to_string());
    }
    *armed = Some(fault);
    Ok(line)
}

/// This process's armed fault, if a flag armed one and the first session hasn't ended.
pub fn armed() -> Option<Arc<Fault>> {
    ARMED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Ends the armed fault with the first session (app.md 4.1, D5): later replicas and simulations get none. `true` if
/// one was armed, for the console's line saying so.
pub fn disarm() -> bool {
    let was = ARMED.lock().unwrap_or_else(|e| e.into_inner()).take().is_some();
    if was {
        ENDED.store(true, std::sync::atomic::Ordering::Release);
    }
    was
}

/// The console's word for a test fault that ended with the first session.
pub const FAULT_ENDED: &str = "`fault` is refused: the test fault ended with session 1";

/// What the console answers to a typed `fault` ([`Fault::request`]): its line, and whether it went in.
pub fn on_command(fault: Option<&Fault>) -> Result<String, String> {
    if fault.is_none() && ENDED.load(std::sync::atomic::Ordering::Acquire) {
        return Err(FAULT_ENDED.into());
    }
    let fault = fault.ok_or_else(|| "`fault` is refused: this machine wasn't started with --arm-fault".to_string())?;
    fault.request()?;
    Ok("FAULT: injecting the test fault at the next frame this machine steps (--arm-fault)".into())
}

/// One armed fault: the world that injects it and the simulation, whose self-test it fails, share it.
#[derive(Debug)]
pub struct Fault {
    /// The frame at whose end the ball changes: set when armed (`--inject-fault-at`), or by the simulation thread on
    /// the console's typed `fault` (`--arm-fault`). Set once.
    at: OnceLock<Frame>,
    /// Armed for the typed command (`--arm-fault`).
    typed: bool,
    /// A typed `fault` waiting for the simulation thread to name its frame ([`Fault::take_request`]).
    requested: AtomicBool,
    /// The ball the first firing changed. Set once, so [`Fault::fired`] stays true.
    ball: OnceLock<ObjectId>,
    /// How many times frame `at` was stepped with the fault: live, re-simulated, replayed or re-stepped for a root.
    steps: AtomicU64,
    /// The console's line for the first firing, until the simulation takes it ([`Fault::take_announcement`]).
    announce: std::sync::Mutex<Option<String>>,
    /// Whether the console has said that frame `at` had no ball to change.
    no_ball: AtomicBool,
}

impl Fault {
    /// A fault at session frame `at` (`--inject-fault-at`).
    pub fn new(at: Frame) -> Arc<Fault> {
        let fault = Fault::make(false);
        let _ = fault.at.set(at);
        Arc::new(fault)
    }

    /// A fault the console's typed `fault` fires at the next frame (`--arm-fault`).
    pub fn typed() -> Arc<Fault> {
        Arc::new(Fault::make(true))
    }

    fn make(typed: bool) -> Fault {
        Fault {
            at: OnceLock::new(),
            typed,
            requested: AtomicBool::new(false),
            ball: OnceLock::new(),
            steps: AtomicU64::new(0),
            announce: std::sync::Mutex::new(None),
            no_ball: AtomicBool::new(false),
        }
    }

    /// The frame it fires at, once set.
    pub fn at(&self) -> Option<Frame> {
        self.at.get().copied()
    }

    /// The flag that armed it, with its frame once set, for every line that names the fault.
    pub fn source(&self) -> String {
        match (self.typed, self.at()) {
            (false, Some(at)) => format!("--inject-fault-at {}", at.0),
            (true, Some(at)) => format!("--arm-fault, typed for frame {}", at.0),
            (_, None) => "--arm-fault".into(),
        }
    }

    /// The console's line at start, when a flag arms it.
    pub fn armed_line(&self) -> String {
        let when = match (self.typed, self.at()) {
            (false, Some(at)) => format!("At frame {}", at.0),
            _ => "Once `fault` is typed in this window (and Enter), at the next frame".into(),
        };
        format!(
            "FAULT INJECTION ARMED ({}): a deliberate test fault, not a real one. {when} this machine doubles one ball's \
             radius in its own state only, and again whenever it steps that frame again. Nothing is sent: the log and \
             the other machines are unchanged. From then on this machine reports its physics self-test as failed, by \
             injection, so it repairs toward the other member.",
            self.source()
        )
    }

    /// The console's typed `fault` (`--arm-fault`): the simulation thread fires it at the next frame it steps. Refused
    /// for a fault `--inject-fault-at` set, and once its frame is set.
    pub fn request(&self) -> Result<(), String> {
        if let Some(at) = self.at() {
            return Err(format!("`fault` is refused: the fault is set for frame {} already ({})", at.0, self.source()));
        }
        if !self.typed {
            return Err("`fault` is refused: this machine wasn't started with --arm-fault".into());
        }
        if self.requested.swap(true, Ordering::AcqRel) {
            return Err("`fault` is refused: it's typed already, waiting for the next frame".into());
        }
        Ok(())
    }

    /// The simulation thread, once a tick: a typed `fault` waiting for its frame gets `next`, the next frame the
    /// replica steps, and the console says so. `None` without one.
    pub fn take_request(&self, next: Frame) -> Option<Frame> {
        if !self.requested.swap(false, Ordering::AcqRel) || self.at.set(next).is_err() {
            return None;
        }
        Some(next)
    }

    /// Whether the fault has fired. From then on this machine reports its self-test as failed.
    pub fn fired(&self) -> bool {
        self.ball.get().is_some()
    }

    /// The ball the first firing changed.
    pub fn ball(&self) -> Option<ObjectId> {
        self.ball.get().copied()
    }

    /// How many times frame `at` was stepped with the fault.
    pub fn steps(&self) -> u64 {
        self.steps.load(Ordering::Relaxed)
    }

    /// The console's line for the first firing, once: the simulation thread prints it with its other notices.
    pub fn take_announcement(&self) -> Option<String> {
        self.announce.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Doubles the ball's radius in `w` (module doc). Called at the end of every step of frame `at`.
    fn inject(&self, w: &mut PeWorld) {
        let Some((ball, mut shape)) = target(w) else {
            if !self.no_ball.swap(true, Ordering::Relaxed) {
                *self.announce.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!(
                    "FAULT INJECTION ({}): that frame had no ball to change, so nothing changed",
                    self.source()
                ));
            }
            return;
        };
        shape.size[0] *= 2.0;
        if let Err(e) = w.world_mut().insert(ball, shape) {
            *self.announce.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(format!("FAULT INJECTION ({}): ball {ball:?} can't change: {e:?}", self.source()));
            return;
        }
        self.steps.fetch_add(1, Ordering::Relaxed);
        if self.ball.set(ball).is_ok() {
            *self.announce.lock().unwrap_or_else(|e| e.into_inner()) = Some(self.fired_line(ball, shape.size[0]));
        }
    }

    /// The console's line when the fault first fires.
    fn fired_line(&self, ball: ObjectId, radius: f32) -> String {
        format!(
            "FAULT INJECTED ({}): a deliberate test fault. At frame {} this machine doubled the radius of ball {:x} to \
             {radius} m in its own state only. Its next cut's root differs, and from now on this machine reports its \
             physics self-test as failed, by injection, not because its physics failed.",
            self.source(),
            self.at().map_or(0, |f| f.0),
            ball.to_u128()
        )
    }
}

/// The ball the fault changes, with its `Shape` row: the highest dynamic sphere that isn't a hand, by its body's world
/// position, and the lowest id among equals. It reads only the state, so every step of the frame picks the same one.
fn target(w: &PeWorld) -> Option<(ObjectId, Shape)> {
    let hands: BTreeSet<ObjectId> = w.hands().iter().map(|h| h.hand).collect();
    let mut best: Option<(f64, ObjectId, Shape)> = None;
    // `physical` is in id order, and only a strictly higher ball replaces the best, so the lowest id wins a tie.
    for id in w.physical() {
        if hands.contains(&id) {
            continue;
        }
        let Ok(Some(row)) = w.body(id) else { continue };
        let Some(shape) = w.world().store().get::<Shape>(id).copied() else { continue };
        if row.kind != BodyKind::Dynamic || shape.kind != shape_kind::SPHERE {
            continue;
        }
        let y = (row.pos - Pos::ORIGIN).0[1];
        if best.as_ref().is_none_or(|(top, _, _)| y > *top) {
            best = Some((y, id, shape));
        }
    }
    best.map(|(_, id, shape)| (id, shape))
}

/// pe-replica's `PeWorld` with this machine's injected fault, if one is armed. Every call but `end_frame` is
/// `PeWorld`'s own, the trait's defaulted ones included, and `end_frame` is too when no fault is armed or it is
/// another frame.
pub struct FaultWorld {
    inner: PeWorld,
    fault: Option<Arc<Fault>>,
    /// This process's deliberate slowdown (`--slow-frame`, [`crate::slowframe`]): each simulated frame waits it out.
    slow: Option<Arc<crate::slowframe::SlowFrame>>,
}

impl FaultWorld {
    /// `inner` with `fault`, and this process's armed slowdown, if any ([`crate::slowframe::armed`]).
    pub fn new(inner: PeWorld, fault: Option<Arc<Fault>>) -> FaultWorld {
        FaultWorld { inner, fault, slow: crate::slowframe::armed() }
    }

    /// The same with `slow` in place of the process's slowdown: a test slows one replica.
    pub fn with_slow(mut self, slow: Option<Arc<crate::slowframe::SlowFrame>>) -> FaultWorld {
        self.slow = slow;
        self
    }

    /// `inner` with this process's armed fault, if any ([`armed`]).
    pub fn armed(inner: PeWorld) -> FaultWorld {
        FaultWorld::new(inner, armed())
    }

    /// The world itself.
    pub fn world(&self) -> &World {
        self.inner.world()
    }
}

impl WorldModel for FaultWorld {
    type Saved = World;

    fn domain(&self) -> DomainId {
        self.inner.domain()
    }
    fn phase(entry: &SignedEntry) -> Phase {
        <PeWorld as WorldModel>::phase(entry)
    }
    fn apply(&mut self, frame: Frame, entry: &SignedEntry) -> (Outcome, Effects) {
        self.inner.apply(frame, entry)
    }
    fn end_frame(&mut self, frame: Frame) -> Vec<Activation> {
        let acts = self.inner.end_frame(frame);
        if let Some(fault) = self.fault.as_ref().filter(|f| f.at() == Some(frame)) {
            fault.inject(&mut self.inner);
        }
        // Every frame stepped, live or re-simulated, pays the slowdown: wall time only (crate::slowframe).
        if let Some(slow) = &self.slow {
            slow.spin();
        }
        acts
    }
    fn scripted(&self) -> bool {
        self.inner.scripted()
    }
    fn logic_stage(&mut self, frame: Frame, stage: Stage, runner: &mut dyn ScriptRunner) -> LogicFx {
        self.inner.logic_stage(frame, stage, runner)
    }
    fn set_local(&mut self, local: Option<MemberKey>) {
        self.inner.set_local(local)
    }
    fn take_replay_from(&mut self) -> Option<Frame> {
        self.inner.take_replay_from()
    }
    fn predicted_from(&self) -> Option<Frame> {
        self.inner.predicted_from()
    }
    fn set_canonical(&mut self, canonical: bool) {
        self.inner.set_canonical(canonical)
    }
    fn set_scratch(&mut self, scratch: bool) {
        self.inner.set_scratch(scratch)
    }
    fn body(&self, id: ObjectId) -> Result<Option<BodyRow>, Unbuildable> {
        self.inner.body(id)
    }
    fn physical(&self) -> Vec<ObjectId> {
        self.inner.physical()
    }
    fn hands(&self) -> Vec<HandRow> {
        self.inner.hands()
    }
    fn claims(&self) -> Vec<ClaimRow> {
        self.inner.claims()
    }
    fn players(&self) -> Vec<PlayerRow> {
        self.inner.players()
    }
    fn settings(&self) -> PhysicsSettings {
        self.inner.settings()
    }
    fn view_objects(&self) -> Vec<ViewObject> {
        self.inner.view_objects()
    }
    fn snapshot(&self, final_frame: Frame) -> Option<Arc<dyn Any + Send + Sync>> {
        self.inner.snapshot(final_frame)
    }
    fn snapshot_of(saved: &World, final_frame: Frame) -> Option<Arc<dyn Any + Send + Sync>> {
        <PeWorld as WorldModel>::snapshot_of(saved, final_frame)
    }
    fn write_poses(&mut self, poses: &[(ObjectId, Pos, [f32; 4])]) -> Vec<ObjectId> {
        self.inner.write_poses(poses)
    }
    fn maintain_transforms(&mut self) -> u32 {
        self.inner.maintain_transforms()
    }
    fn write_physics(&mut self, rows: &[(ObjectId, BodyStateRow)], global: Hash) {
        self.inner.write_physics(rows, global)
    }
    fn root(&self) -> Hash {
        self.inner.root()
    }
    fn save(&self) -> World {
        self.inner.save()
    }
    fn restore(&mut self, saved: &World) {
        self.inner.restore(saved)
    }
    fn tree<'a>(&'a self, saved: &'a World) -> Box<dyn Tree + 'a> {
        self.inner.tree(saved)
    }
    fn empty(&self) -> World {
        self.inner.empty()
    }
    fn import(&self, saved: &World, import: &CutImport) -> Result<World, ImportError> {
        self.inner.import(saved, import)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_world::BlobStore;

    fn pit() -> PeWorld {
        PeWorld::new(pe_world::preset::ball_pit_with(5, BlobStore::default(), Pos::ORIGIN, 8), DomainId(0))
    }

    fn radius(w: &World, id: ObjectId) -> f32 {
        w.store().get::<Shape>(id).expect("a shape").size[0]
    }

    /// The fault doubles one ball's radius at the end of its frame, and only there: beside a `PeWorld` stepped alike,
    /// the root is the same after another frame and differs after the fault's, where only that ball's radius differs.
    #[test]
    fn the_fault_doubles_one_balls_radius_at_its_frame() {
        let fault = Fault::new(Frame(3));
        let mut w = FaultWorld::new(pit(), Some(fault.clone()));
        let mut plain = pit();
        w.end_frame(Frame(2));
        plain.end_frame(Frame(2));
        assert!(!fault.fired(), "not at another frame");
        assert_eq!(w.root(), plain.root(), "another frame changes nothing");
        w.end_frame(Frame(3));
        plain.end_frame(Frame(3));
        let ball = fault.ball().expect("it fired");
        assert_ne!(w.root(), plain.root(), "the ball's leaf moved the root");
        assert_eq!(radius(w.world(), ball), 2.0 * radius(plain.world(), ball), "doubled");
        let changed: Vec<ObjectId> = (plain.world().store().iter::<Shape>())
            .filter(|(id, s)| w.world().store().get::<Shape>(*id) != Some(*s))
            .map(|(id, _)| id)
            .collect();
        assert_eq!(changed, vec![ball], "one ball");
        assert_eq!(fault.steps(), 1);
        let line = fault.take_announcement().expect("announced");
        assert!(line.starts_with("FAULT INJECTED (--inject-fault-at 3): a deliberate test fault."), "{line}");
        assert_eq!(fault.take_announcement(), None, "once");
    }

    /// The fault repeats each time its frame is stepped, as on a machine that computes it wrongly: restoring the state
    /// from before the frame and stepping it again gives the same root, so a replay from the last verified cut
    /// computes the fault again (rung 1 can't cure it). The ball it picks is the highest, and the first firing's.
    #[test]
    fn stepping_the_frame_again_repeats_the_fault() {
        let fault = Fault::new(Frame(3));
        let mut w = FaultWorld::new(pit(), Some(fault.clone()));
        let saved = w.save();
        w.end_frame(Frame(3));
        let (once, ball) = (w.root(), fault.ball().expect("fired"));
        w.restore(&saved);
        w.end_frame(Frame(3));
        assert_eq!(w.root(), once, "the same fault again");
        assert_eq!((fault.ball(), fault.steps()), (Some(ball), 2));
        // The highest ball: no other dynamic sphere's body is above it.
        let top = |id: ObjectId| (w.body(id).expect("a body").expect("physical").pos - Pos::ORIGIN).0[1];
        let highest = (w.physical().into_iter())
            .filter(|&id| w.world().store().get::<Shape>(id).is_some_and(|s| s.kind == shape_kind::SPHERE))
            .all(|id| top(id) <= top(ball));
        assert!(highest, "the highest ball");
    }

    /// `--arm-fault`'s typed `fault`: nothing fires until it's typed, then the simulation thread names the next frame
    /// and the fault fires there. A second `fault`, one without `--arm-fault`, and one for a fault `--inject-fault-at`
    /// set are refused.
    #[test]
    fn a_typed_fault_waits_for_its_command_then_fires_at_the_next_frame() {
        let fault = Fault::typed();
        let mut w = FaultWorld::new(pit(), Some(fault.clone()));
        let mut plain = pit();
        assert_eq!(fault.take_request(Frame(2)), None, "nothing typed");
        w.end_frame(Frame(2));
        plain.end_frame(Frame(2));
        assert!(!fault.fired() && fault.at().is_none(), "not before it's typed");
        assert_eq!(w.root(), plain.root());
        on_command(Some(&fault)).expect("typed");
        assert!(on_command(Some(&fault)).is_err(), "typed twice");
        assert_eq!(fault.take_request(Frame(3)), Some(Frame(3)), "the next frame");
        assert_eq!(fault.take_request(Frame(4)), None, "taken once");
        assert!(on_command(Some(&fault)).is_err(), "its frame is set");
        w.end_frame(Frame(3));
        plain.end_frame(Frame(3));
        assert!(fault.fired(), "fired at its frame");
        assert_ne!(w.root(), plain.root());
        assert_eq!(fault.source(), "--arm-fault, typed for frame 3");
        assert!(on_command(None).is_err(), "refused without --arm-fault");
        assert!(on_command(Some(&Fault::new(Frame(9)))).is_err(), "refused for --inject-fault-at's fault");
    }

    /// Without a fault, or at another frame, the wrapper is `PeWorld`: the same roots step for step.
    #[test]
    fn without_a_fault_the_world_is_pe_worlds() {
        let mut plain = pit();
        let mut wrapped = FaultWorld::new(pit(), None);
        for f in 0..4 {
            plain.end_frame(Frame(f));
            wrapped.end_frame(Frame(f));
            assert_eq!(wrapped.root(), plain.root(), "frame {f}");
        }
        assert!(armed().is_none(), "no test arms the process's fault");
    }
}
