//! The logic stages (logic.md 6.2, 9.2a and 10.1): pe-seams' `Stage::LogicPrePhysics` and `Stage::LogicPostPhysics`,
//! run through a [`ScriptRunner`] the replica is given (@coordinator-opus m339785d800ca, with @fable-reviewer's
//! conditions m46630a61ded7). So pe-replica needs no pe-logic to step a world without scripts. The `logic` feature
//! adds pe-logic's runner (@coordinator-opus mba546f07b57f); the step is the same code with or without it, and only the
//! runner differs.
//!
//! - **The contract:** a run is a pure function of the module bytes, the state it is given and the fuel: no clock, no
//!   host randomness, nothing kept from one frame to the next. A stage at `LogicPrePhysics` starts its frame, so a
//!   replay of a frame from a restored state runs as the first run did.
//! - **Identity:** a runner reports [`ScriptRunner::identity`], the canonical bytes of S2's `RunnerIdentity`: the
//!   runtime and its version, every setting that can change a result, and the runner's own build. The hello compares
//!   them, and every fault bundle records them, so that an offline replay refuses another runner.
//! - **No runner:** a replica without one refuses, locally, to step a world with module rows, or a frame with a code
//!   commit. It never steps such a world as having no scripts ([`crate::ReplicaImpl`] counts one fault per stop).
//! - **Which scripts run** ([`crate::world::WorldModel::logic_stage`]): every row of the world's modules table whose
//!   `activate_at` is at or before the frame, in asset order, with the row's module hash (`author_module`), its version
//!   hash V and its component ids, the tag among them. A script runs only where the runner holds that module and it
//!   binds under the row's ids (logic.md 6.2, 3). Otherwise it does not run on this machine, and the stage reports it
//!   as missing: a build fault on this machine.
//! - **Events** become S5 effects ([`event_kind`]): their keys are stable across replays, so the view plays each once.
//! - **Every runner** the repo builds passes [`conformance`] (@fable-reviewer m46630a61ded7): the test fake, and
//!   pe-logic's (`LogicRunner`, the `logic` feature).

use std::collections::BTreeSet;

use bytes::Bytes;
use pe_core::{Frame, Hash, LineId, ObjectId};
use pe_seams::Stage;
use pe_sim::{ComponentId, SimStore};
use pe_world::LogicReport;
use pe_world::script::AssetId;

#[cfg(feature = "logic")]
mod runner;
#[cfg(feature = "logic")]
pub use runner::{LogicRunner, ModuleInbox};

/// One script a logic stage runs: its row of the world's modules table (world-model.md 5.4 and 7.9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptRow {
    /// The script's stable asset id: scripts run in its order, and faults are keyed by it.
    pub asset: AssetId,
    /// The module to run: the row's `author_module`, the hash of the metered bytes (logic.md 6.2).
    pub module: Hash,
    /// The version hash V, which the component ids derive from (world-model.md 3.2).
    pub version: Hash,
    /// The version's component ids, ascending, its tag among them.
    pub components: Vec<ComponentId>,
}

/// The world's checks for an engine row a script wrote (`World::script_row_checker`): an `Err` faults the call.
pub type RowCheck<'a> = dyn Fn(ComponentId, ObjectId, &[u8]) -> Result<(), String> + 'a;

/// What the world gives a logic stage besides the store: pe-world's hooks (`World::logic_stage`).
pub struct StageWorld<'a> {
    pub frame: Frame,
    /// The world's line, which seeds the scripts' random streams.
    pub line: LineId,
    pub rate_hz: u32,
    /// True for an object a script may not destroy now: a held one.
    pub guard: &'a dyn Fn(ObjectId) -> bool,
    /// The assets the world's state marks faulted (logic.md 3.6): they do not run.
    pub faulted: &'a BTreeSet<AssetId>,
    pub check: &'a RowCheck<'a>,
}

/// An event a script emitted (logic.md 3.2), for the frame's effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptEvent {
    pub asset: AssetId,
    pub system: String,
    /// The event type's name, as the system's `emits` list declares it.
    pub name: String,
    /// The object whose row emitted it.
    pub object: ObjectId,
    pub bytes: Vec<u8>,
}

/// What one logic stage did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StageOutcome {
    /// What `World::after_logic` records. Its faults are world state: a host-stack fault is not among them.
    pub report: LogicReport,
    /// In the order the scripts emitted them.
    pub events: Vec<ScriptEvent>,
    /// Host-stack faults: this machine's, not world state (logic.md 3.6).
    pub host_faults: u32,
    /// Times the floating-point environment had changed at a guest entry and was restored (N11). Local.
    pub fp_resets: u32,
    /// Scripts that did not run: the runner holds no module with the row's hash, or it would not bind under the row's
    /// ids. A build fault on this machine (logic.md 6.2, 3).
    pub missing: Vec<AssetId>,
}

/// A script runner, injected into the replica (module doc).
pub trait ScriptRunner: Send {
    /// The canonical bytes of S2's `RunnerIdentity`: two runners with equal bytes run every module alike.
    fn identity(&self) -> Bytes;
    /// Whether it holds the module with this hash (its metered bytes' hash), ready to run.
    fn holds(&self, module: &Hash) -> bool;
    /// The metered bytes of a module it holds, for a fault bundle.
    fn module(&self, module: &Hash) -> Option<Bytes>;
    /// Runs one logic stage of `world.frame` over `store`: each of `scripts`, in their order, whose module it holds and
    /// binds, unless the world marks it faulted. `stage` is `LogicPrePhysics` or `LogicPostPhysics`; the first starts
    /// the frame.
    fn run_stage(
        &mut self,
        stage: Stage,
        scripts: &[ScriptRow],
        store: &mut SimStore,
        world: &StageWorld<'_>,
    ) -> StageOutcome;
}

/// S5's number for a script event's kind (`EffectKind::Script`): the first 8 bytes, little-endian, of
/// `Hash::of("pe.script.event", asset ‖ name)`, so the same for every system of the script that emits it, and stable
/// while the event keeps its name (@coordinator-opus m6c9cfed886fe).
pub fn event_kind(asset: &AssetId, name: &str) -> u64 {
    let mut b = asset.to_vec();
    b.extend_from_slice(name.as_bytes());
    let h = Hash::of("pe.script.event", &b);
    u64::from_le_bytes(h.0[..8].try_into().expect("a hash has 32 bytes"))
}

/// This machine's script counts, kept by the replica's live steps: never world state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScriptCounts {
    /// Host-stack faults (logic.md 3.6).
    pub host_faults: u64,
    /// Script runs skipped because the module was missing or would not bind: one per script and stage.
    pub missing: u64,
}

/// A fixture for [`conformance`]: a store and the rows to run over it, frame after frame from the store's frame.
pub struct Fixture {
    pub store: SimStore,
    pub scripts: Vec<ScriptRow>,
    pub line: LineId,
    pub rate_hz: u32,
    /// How many frames to run, both stages each. The replay check starts halfway.
    pub frames: u64,
}

/// What two runs must agree on: everything a stage reports except this machine's own counts.
fn agreed(o: &StageOutcome) -> (LogicReport, Vec<ScriptEvent>, Vec<AssetId>) {
    (o.report.clone(), o.events.clone(), o.missing.clone())
}

fn run_fixture_stage(
    r: &mut dyn ScriptRunner,
    stage: Stage,
    fx: &Fixture,
    store: &mut SimStore,
    faulted: &BTreeSet<AssetId>,
) -> StageOutcome {
    let guard = |_: ObjectId| false;
    let check = |_: ComponentId, _: ObjectId, _: &[u8]| -> Result<(), String> { Ok(()) };
    let frame = store.frame();
    let world = StageWorld { frame, line: fx.line, rate_hz: fx.rate_hz, guard: &guard, faulted, check: &check };
    r.run_stage(stage, &fx.scripts, store, &world)
}

/// The conformance check every runner the repo builds passes (Q1's testing condition, @fable-reviewer m46630a61ded7).
/// Over `fx`'s frames:
/// 1. two runners from `make` have equal identities, not empty, and at every stage they report the same and leave
///    the same store;
/// 2. the first runner, having run every frame, replays the second half from the store saved there exactly as it ran
///    it the first time: nothing it keeps from one frame to the next changes a result.
///
/// A fault puts its script among the faulted for the frames after, as the world records it. Host-stack faults and
/// floating-point resets are this machine's own and are not compared. Returns the first difference.
pub fn conformance(make: &dyn Fn() -> Box<dyn ScriptRunner>, fx: &Fixture) -> Result<(), String> {
    let (mut a, mut b) = (make(), make());
    let id = a.identity();
    if id.is_empty() || id != b.identity() {
        return Err("two runners' identities differ, or one is empty".into());
    }
    let image = |s: &SimStore| s.snapshot().to_bytes();
    let (first, end) = (fx.store.frame().0, fx.store.frame().0 + fx.frames);
    let half = first + fx.frames / 2;
    let (mut sa, mut sb) = (fx.store.clone(), fx.store.clone());
    let mut faulted = BTreeSet::new();
    let mut saved = None;
    let mut first_run = Vec::new();
    for f in first..end {
        if f == half {
            saved = Some((sa.clone(), faulted.clone()));
        }
        sa.set_frame(Frame(f));
        sb.set_frame(Frame(f));
        for stage in [Stage::LogicPrePhysics, Stage::LogicPostPhysics] {
            let oa = run_fixture_stage(&mut *a, stage, fx, &mut sa, &faulted);
            let ob = run_fixture_stage(&mut *b, stage, fx, &mut sb, &faulted);
            if agreed(&oa) != agreed(&ob) || image(&sa) != image(&sb) {
                return Err(format!("frame {f}, {stage:?}: two runners differ"));
            }
            if f >= half {
                first_run.push((agreed(&oa), image(&sa)));
            }
            faulted.extend(oa.report.faults.iter().map(|(asset, _)| *asset));
        }
    }
    let Some((mut s, mut faulted)) = saved else { return Ok(()) };
    let mut first_run = first_run.into_iter();
    for f in half..end {
        s.set_frame(Frame(f));
        for stage in [Stage::LogicPrePhysics, Stage::LogicPostPhysics] {
            let o = run_fixture_stage(&mut *a, stage, fx, &mut s, &faulted);
            if Some((agreed(&o), image(&s))) != first_run.next() {
                return Err(format!("frame {f}, {stage:?}: the replay from frame {half} differs from the first run"));
            }
            faulted.extend(o.report.faults.iter().map(|(asset, _)| *asset));
        }
    }
    Ok(())
}
