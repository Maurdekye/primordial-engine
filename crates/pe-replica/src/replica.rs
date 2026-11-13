//! The replica (seam S2) over a [`WorldModel`] and pe-physics: one frame (replica.md 3), rollback (4), roots at cuts
//! (5), finalize and rung 1 (6.1, 8), within a budget of simulated time per call (S2 v1.1).
//!
//! **The live history** runs from the floor (the newest cut I finalized, or frame 0) to `frame`, the next frame to
//! step. Nothing below the floor changes again, except that rung 1 rebuilds the history from `Cv`. `reached` is the
//! newest frame the live history has been at: after a rollback, the frames up to it are re-simulated.
//!
//! **Roots.** At every cut-grid frame (multiples of the cut interval in frames) the replica writes the physics rows and
//! takes the root, live or re-simulated. It keeps those roots from the floor to the present, with the ids of the
//! entries each frame stepped. `root_at_cut` re-steps from the newest keyframe at or before the first frame whose live
//! entries are not exactly the cut's (the cut's frame itself when they all are), through the same frame function as
//! the live history, and puts the live state back. Every root it gives out is held as a snapshot of its own (world
//! state, physics image and transfer) until its cut is final or superseded; the floor's and `Cv`'s stay held.
//!
//! **Bounded work.** Each call steps at most the budget's frames (never fewer than two). A rollback, a re-simulation
//! or a rung-1 replay that does not fit resumes on the next call; the results never depend on the split. Two steps
//! are not budgeted yet (@design-reviewer's Lows on 3d13ff88):
//! - `root_at_cut`'s re-step off the live history, from the first frame whose entries differ from the cut's to the
//!   cut's frame: at most the distance from the floor to the cut, normally one or two cut intervals;
//! - `replay_to`'s check of `Cv`: a full tree `verify` and a physics rehash, O(world) in one call (M2's worker).
//!
//! **Entries S3 no longer keeps (S3 v1.2).** Rung 1 replays from `Cv`, which can be older than the floor, and reads
//! those frames' entries from the source too. Whenever `finalize` moves `Cv` or the floor, it tells the source both
//! (`keep_final_from(cv, floor)`). S3 never trims at or above the floor except by its time cap, and below the floor
//! it keeps from `Cv` on unless the time cap or its byte cap trimmed past it. Each case of a trimmed frame
//! (`entries_at` answers `None`) is handled once, never by stepping it empty:
//! - **Before a replay:** a cap trimmed past `Cv`, so `replay_to` answers `NotKept` before restoring anything, and
//!   the ladder goes to rung 2.
//! - **During a replay, below the floor:** the byte cap trimmed since the replay began. The replay is abandoned
//!   and answers `NotKept` when asked again; this is no fault. The live state goes back to the floor's snapshot, and
//!   the next call re-simulates from there, so the member keeps simulating while rung 2 goes ahead
//!   (@design-reviewer, m76a7a148f6b5).
//! - **At or above the floor, met by `advance`:** only a replica more than the time cap behind, or an S3 fault,
//!   meets one. The live history stops there, with one fault per stop, and the view keeps the last state it
//!   published. `root_at_cut` answers `Damaged` for any cut past that frame, and `finalize` answers
//!   `Finalized::Damaged`, so rung 2 is due at once. A replay stops there too: `replay_to` answers `Damaged` if its
//!   cut's root needed that frame, and otherwise the root it took below the frame.
//! - **At or above the floor, needed for a root:** a root needs every frame from the floor to the cut's frame, to
//!   compare with the live history and to re-step. When one of them is trimmed, `root_at_cut` and `finalize` answer
//!   as for a stop, with one fault per such frame, not one per call (@chunks-reviewer's Medium on 3d8e9442, option
//!   (a) by @coordinator-opus, ma30cdfd63b94).
//!
//! **The d1 rule is S3's.** `root_at_cut` and `replay_to` never answer "missing entries": a cut may cover void
//! entries that `entries_at` leaves out, so the replica cannot count them (@coordinator-opus, m8126544ad530).
//!
//! **Rung 2 and joining** (`import_at_cut`, 6.2; S2 v1.2 round 3): the import is checked whole before the live state
//! changes, then the live state is the imported one at the cut's frame, in a new physics world, and re-simulates to
//! the present through the replay. pe-physics restores a transfer only into a new world, whose images are not the
//! old world's, so the ring starts again at the cut, and until the floor reaches the cut the replica keeps no
//! history below it (`base_frame`): there, roots answer `Damaged` (rung 2 again), changes are faults, and no replay
//! starts from an older `Cv`.
//!
//! **Not yet here** (the M1 increments that follow): hands, grips and grab joints (3.3), carried checks (3.4), logic
//! stages (3, steps 3 and 8), and the view's rows beyond bodies (7).

use core::time::Duration;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use pe_physics::{CheckHash, PhysicsConfig, PhysicsError, PhysicsWorld, Pose, Transfer, WorldImage};
use pe_seams::{
    Advance, ChainId, Class, Cut, CutImport, CutRef, DomainId, Effect, EffectCause, EffectKey, EffectKind, EntryRef,
    EntrySource, FinalCut, Finalized, Frame, FrameClock, FrameView, GroupSel, Hash, Health, HostTime, ImportError,
    MemberKey, ObjectId, Offset, Outcome, PhysicsImage, Pos, Replica, RigTransform, RootError, SignedEntry, SimRate,
    Stage, StepRecord, Tree, ViewCounters, ViewFlags, VoidReason, frames_ceil,
};

use crate::bodies;
use crate::hands;
use crate::logic::{ScriptCounts, ScriptEvent, ScriptRunner, event_kind};
use crate::ring::{Policy, Ring};
use crate::world::{BodyStateRow, HandRow, PhysicsSettings, ShapePart, ShapeRow, WorldModel, apply_order};

/// The replica's settings. Only `cut_every` follows from world state (the world's cut interval); the rest are local.
#[derive(Clone)]
pub struct ReplicaConfig {
    /// pe-physics's engine configuration: the worker count, sleep and contact recycling. Its gravity and sub-step count
    /// are never read. Every physics world the replica builds, at its start and at each import, takes the gravity
    /// (and the origin) from the world model's settings and the sub-steps from the rate, so no construction-time copy
    /// of a world setting is kept (world-model.md 6.6; @coordinator-opus m1eb5e9623174).
    pub physics: PhysicsConfig,
    /// The cut interval in frames: roots are taken at its multiples.
    pub cut_every: u64,
    /// How long final cuts' roots stay held for peers' repairs (`T_keep`); at least the last two are kept.
    pub keep_finals: Duration,
    /// The keyframe ring's policy.
    pub policy: Policy,
    /// The shared time base (S5): the view's `sim_time` is its host time of the view's frame. Required, so no replica
    /// publishes a time off that base (@design-reviewer's Low on pe-app slice 2, via @engine-architect m440144e592fc).
    pub clock: Arc<dyn FrameClock>,
    /// The local member, whose hands the frame view marks `OWN_HAND` (S5), so the display draws them at the device's
    /// late-latched poses. `None` for a replica with no local player (tests, a headless host).
    pub local: Option<MemberKey>,
}

impl ReplicaConfig {
    /// The defaults at `rate`, with the cut interval agreed with WS-agree (a world setting, 3.5's rule for frames),
    /// on the shared time base `clock`.
    pub fn new(rate: SimRate, cut_interval: Duration, clock: Arc<dyn FrameClock>) -> ReplicaConfig {
        ReplicaConfig {
            physics: PhysicsConfig::engine(),
            cut_every: frames_ceil(cut_interval, rate.hz()).max(1),
            keep_finals: Duration::from_millis(1500),
            policy: Policy::log4(rate.hz()),
            clock,
            local: None,
        }
    }
}

/// A keyframe: the world model's snapshot and the physics image of the same instant.
struct Key<S> {
    world: S,
    image: WorldImage,
}

/// A held root: the state at a cut with exactly its entries, kept apart from the ring.
struct Held<S> {
    frame: Frame,
    world: S,
    image: WorldImage,
    /// The canonical physics transfer, and its check hash (`physics_global`).
    transfer: Bytes,
    global: Hash,
    /// The physics world `image` belongs to (`ReplicaImpl::epoch`). The records and the transfer serve exports from
    /// any world, but the image restores only into its own.
    epoch: u64,
}

/// The newest cut finalized here, and my root at it.
struct Floor {
    cut: Cut,
    root: Hash,
}

/// Rung 1 under way: the live history is rebuilt from `Cv`, with exactly the cut's entries below its frame.
struct Replay {
    cut: Cut,
    root: Option<Hash>,
}

/// The replica over a world model `W` and pe-physics.
pub struct ReplicaImpl<W: WorldModel> {
    world: W,
    physics: PhysicsWorld,
    config: ReplicaConfig,
    domain: DomainId,
    rate: SimRate,
    dt: f32,
    origin: Pos,
    frame: Frame,
    reached: Frame,
    /// `None` until the first finalize; frame 0 is the floor until then.
    floor: Option<Floor>,
    /// My last agreed cut, and my root there.
    cv: Option<(Cut, Hash)>,
    ring: Ring<Key<W::Saved>>,
    /// Roots at cut-grid frames of the live history, from the floor.
    live: BTreeMap<Frame, Hash>,
    /// The ids of the entries each live frame stepped, in apply order, from the floor.
    stepped: BTreeMap<Frame, Vec<EntryRef>>,
    outcomes: BTreeMap<EntryRef, (Frame, Outcome)>,
    held: BTreeMap<Hash, Held<W::Saved>>,
    /// My roots at cuts not final yet, by cut.
    voted: BTreeMap<CutRef, Hash>,
    /// Final cuts within `T_keep`, oldest first, with my root at each.
    finals: VecDeque<(CutRef, Hash)>,
    /// My root at a cut I diverged at, until a later finalize agrees.
    diverged: Option<(CutRef, Hash)>,
    replay: Option<Replay>,
    /// The last rung-1 replay that finished, and its root: asked again, it answers at once.
    replayed: Option<(CutRef, Hash)>,
    /// The cut of my last import (rung 2 or joining) and my root there, until the floor reaches it. The replica
    /// keeps no history below it: its physics world is new, and older keyframes cannot be restored into it.
    imported: Option<(CutRef, Hash)>,
    /// The physics world's generation: each import builds a new one, and an image from an older one never restores
    /// into it. Held roots from before an import keep its number, so none of their images is ever restored
    /// (@chunks-reviewer's L2 on 05903341).
    epoch: u64,
    /// A replay that ended at a frame no longer kept, and what it answers when asked again: `NotKept` below the floor
    /// (abandoned), `Damaged` at or above it when its cut's root needed that frame (S2 v1.2 round 3).
    lost: Option<(CutRef, RootError)>,
    /// The frame the live history stopped at because S3 no longer keeps its entries, at or above the floor: no root
    /// past it is given until the frame can be stepped (S3 v1.2).
    stuck_at: Option<Frame>,
    /// The last trimmed frame a root needed, counted as a fault once (the driver asks again on every tick).
    unkept_met: Option<Frame>,
    /// Frames one call may step.
    budget: u64,
    view: Arc<FrameView>,
    counters: ViewCounters,
    /// Refused physics calls, requests below the floor, and other faults that 3.1 reports instead of panicking.
    faults: u64,
    /// Bodies stopped at their last valid pose because the step took them out of range (3, step 7).
    stopped: u64,
    /// What re-steps off the live history counted, kept apart from the live counts (@chunks-reviewer's L1 on
    /// 40f9cca4): a fault in a frame both histories step is counted once against the live history.
    scratch: Scratch,
    /// The records of the last re-step off the live history: a re-base on the floor takes them for `[g, floor)`.
    scratch_records: BTreeMap<EntryRef, (StepRecord, Seen)>,
    /// Spans `[g, floor)` that a `finalize` re-based on the cut-exact floor: S3's change report inside one is expected
    /// once, at the next `advance` (@design-reviewer, m46c0d79ab4ad).
    excused: Vec<(Frame, Frame)>,
    /// Drawn objects whose pose at the last published frame a re-simulation rewrote (a replay, a repair or a merge):
    /// the next publication marks them `CORRECTED` (S5), then forgets them.
    corrected: BTreeSet<ObjectId>,
    /// The change records made since the last publication, in the order they were made: the next view's `changes`
    /// (S5).
    pending: Vec<StepRecord>,
    /// The entries I made a record for, by frame, from my last agreed cut's frame up, with their author and class.
    /// Before `final_frame` passes a frame, each of them the final order leaves out gets a `Dropped` record (S5 v1.5).
    recorded: BTreeMap<Frame, BTreeMap<EntryRef, Seen>>,
    /// The frame up to which a rung-2 import has recorded `Unknown`, so a later import never repeats an entry.
    unknown_to: Frame,
    /// The frame of the final cut where my root first differed, or could not be computed, since I last agreed: the
    /// view's `Health::Repairing { since }` (S5 v1.5).
    repairing_since: Option<Frame>,
    /// The script runner it was given (crate::logic). Without one it refuses to step a world with scripts.
    runner: Option<Box<dyn ScriptRunner>>,
    /// The frame a step without a runner was last refused at, so that each stop counts one fault.
    refused_at: Option<Frame>,
    /// Each frame's script effects from its latest live step, from the base frame up (S5).
    fx: BTreeMap<Frame, Vec<Effect>>,
    /// The keys of the effects a view has carried, per frame: a key carried is never carried again (D6).
    played: BTreeMap<Frame, BTreeSet<EffectKey>>,
    /// The frames stepped live since the last publication, whose effects the next view compares.
    fx_stepped: BTreeSet<Frame>,
    script_counts: ScriptCounts,
}

/// What the replica keeps of an entry it made a change record for, until `final_frame` passes its frame.
#[derive(Clone, Copy, Debug)]
struct Seen {
    author: MemberKey,
    class: Class,
    chain: ChainId,
    seq: u64,
}

impl Seen {
    fn of(e: &SignedEntry) -> Seen {
        Seen { author: e.entry.author, class: e.entry.class, chain: e.entry.chain, seq: e.entry.seq }
    }
}

/// What re-steps off the live history (`root_at_cut`'s scratch) counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scratch {
    pub faults: u64,
    pub stopped: u64,
    pub fp_resets: u32,
}

impl<W: WorldModel> ReplicaImpl<W> {
    /// A replica of `world` at frame 0: a body for each of its physical objects, in id order, in a physics world with
    /// the world's gravity, origin and rate.
    pub fn new(world: W, config: ReplicaConfig) -> Result<ReplicaImpl<W>, PhysicsError> {
        let settings = world.settings();
        let mut physics = PhysicsWorld::new(physics_config(config.physics, &settings, settings.rate)?)?;
        for id in world.physical() {
            if let Some(row) = world.body(id) {
                physics.add_body(id, &bodies::desc(&row, settings.origin))?;
            }
        }
        let domain = world.domain();
        let mut r = ReplicaImpl {
            world,
            physics,
            domain,
            rate: settings.rate,
            dt: 1.0 / settings.rate.hz() as f32,
            origin: settings.origin,
            config,
            frame: Frame(0),
            reached: Frame(0),
            floor: None,
            cv: None,
            ring: Ring::new(),
            live: BTreeMap::new(),
            stepped: BTreeMap::new(),
            outcomes: BTreeMap::new(),
            held: BTreeMap::new(),
            voted: BTreeMap::new(),
            finals: VecDeque::new(),
            diverged: None,
            replay: None,
            replayed: None,
            imported: None,
            epoch: 0,
            lost: None,
            stuck_at: None,
            unkept_met: None,
            budget: u64::MAX,
            view: Arc::new(empty_view(domain)),
            counters: ViewCounters::default(),
            faults: 0,
            stopped: 0,
            scratch: Scratch::default(),
            scratch_records: BTreeMap::new(),
            excused: Vec::new(),
            corrected: BTreeSet::new(),
            pending: Vec::new(),
            recorded: BTreeMap::new(),
            unknown_to: Frame(0),
            repairing_since: None,
            runner: None,
            refused_at: None,
            fx: BTreeMap::new(),
            played: BTreeMap::new(),
            fx_stepped: BTreeSet::new(),
            script_counts: ScriptCounts::default(),
        };
        if r.is_cut_frame(Frame(0)) {
            let root = r.take_root();
            r.live.insert(Frame(0), root);
        }
        r.keyframe(Frame(0));
        r.ring.pin([Frame(0)]);
        r.publish();
        Ok(r)
    }

    /// The world model, for reads (tests, the harness).
    pub fn world(&self) -> &W {
        &self.world
    }

    /// The physics world, for reads (tests, diagnostics).
    pub fn physics(&self) -> &PhysicsWorld {
        &self.physics
    }

    /// Faults counted so far.
    pub fn faults(&self) -> u64 {
        self.faults
    }

    /// The replica with its script runner (crate::logic): without one it refuses to step a world with scripts.
    pub fn with_runner(mut self, runner: Box<dyn ScriptRunner>) -> Self {
        self.runner = Some(runner);
        self
    }

    /// The canonical bytes of its runner's identity (S2's `RunnerIdentity`), or `None` without a runner.
    pub fn runner_identity(&self) -> Option<Bytes> {
        self.runner.as_ref().map(|r| r.identity())
    }

    /// The script counts of its live steps: this machine's, never world state.
    pub fn script_counts(&self) -> ScriptCounts {
        self.script_counts
    }

    /// Bodies stopped because the step took them out of range.
    pub fn stopped(&self) -> u64 {
        self.stopped
    }

    /// What re-steps off the live history counted: never part of [`Self::faults`] or [`Self::stopped`].
    pub fn scratch(&self) -> Scratch {
        self.scratch
    }

    /// The live root at a cut-grid frame, if it has been taken and is still kept.
    pub fn live_root(&self, frame: Frame) -> Option<Hash> {
        self.live.get(&frame).copied()
    }

    /// The frame below which nothing changes again (rung 1 aside).
    pub fn floor_frame(&self) -> Frame {
        self.floor.as_ref().map_or(Frame(0), |f| f.cut.at.frame)
    }

    /// The world model's state at `Cv`, the newest final cut whose root this replica matched, and that cut's frame
    /// (@engine-architect, m2900ddc05c01, for pe-api's `WorldSource::final_`). `None` before the first agreed finalize.
    /// It is not the floor's: the floor moves to a final cut I diverged at too. It is exactly the cut's entries, never
    /// a prediction. A rollback, a replay or an import leaves it as it is, and only a finalize that answers `Agreed`
    /// moves it.
    pub fn agreed_state(&self) -> Option<(Frame, &W::Saved)> {
        let (cut, root) = self.cv.as_ref()?;
        self.held.get(root).map(|h| (cut.at.frame, &h.world))
    }

    fn is_cut_frame(&self, f: Frame) -> bool {
        f.0.is_multiple_of(self.config.cut_every) || self.replay.as_ref().is_some_and(|r| r.cut.at.frame == f)
    }

    /// The lowest frame a rollback may reach: the floor, or a replay's cut frame while the replay runs.
    fn rollback_floor(&self) -> Frame {
        let floor = self.base_frame();
        self.replay.as_ref().map_or(floor, |r| floor.max(r.cut.at.frame))
    }

    /// The lowest frame the replica re-simulates from: the floor, or my last import's cut frame above it.
    fn base_frame(&self) -> Frame {
        let floor = self.floor_frame();
        self.imported.map_or(floor, |i| floor.max(i.0.frame))
    }

    /// The held root's snapshot, if its image restores into the current physics world: not one held from before my
    /// last import.
    fn held_here(&self, root: &Hash) -> Option<&Held<W::Saved>> {
        self.held.get(root).filter(|h| h.epoch == self.epoch)
    }

    /// Whether the live history stopped below `at`, at a frame S3 no longer keeps: no root at `at` will come.
    fn stopped_short_of(&self, at: Frame) -> bool {
        self.stuck_at.is_some_and(|f| at > f)
    }

    /// The entries frame `f` steps: the source's, or below a replay's cut frame exactly what S3 answers for that cut
    /// ([`EntrySource::entries_for_cut`], S3 v1.4). `None` when the source no longer keeps the frame's entries (S3
    /// v1.2).
    fn entries_for<'s>(&self, source: &'s dyn EntrySource, f: Frame) -> Option<Cow<'s, [SignedEntry]>> {
        match &self.replay {
            Some(r) if f < r.cut.at.frame => source.entries_for_cut(&r.cut, f).map(Cow::Owned),
            _ => source.entries_at(f).map(Cow::Borrowed),
        }
    }

    // ---- one frame --------------------------------------------------------------------------------------------------

    /// Step `self.frame` with these entries, given in agreed order (replica.md 3). They apply in apply order
    /// ([`apply_order`]: by the world model's phase, then agreed order), the one path for every step: live, in a
    /// re-simulation, in rung 1's replay and in the re-step for a root at a cut. Live frames record their outcomes,
    /// the ids they stepped, their roots and keyframes; a scratch frame (a root at a cut computed off the live
    /// history) records nothing. Returns the root if the next frame is a cut frame.
    fn step(&mut self, entries: &[&SignedEntry], live: bool) -> Option<Hash> {
        let f = self.frame;
        // A world with scripts, or a frame with a code commit, needs a runner (crate::logic). Without one the frame is
        // not stepped: nothing changes, and each stop counts one fault.
        if self.runner.is_none() && (self.world.scripted() || entries.iter().any(|e| W::commits_code(e))) {
            if self.refused_at != Some(f) {
                self.faults += 1;
                self.refused_at = Some(f);
            }
            return None;
        }
        let resimulated = f < self.reached;
        if !pe_core::fpenv::check_and_reset() {
            self.counters.fp_resets += 1;
        }
        let mut ids = Vec::with_capacity(entries.len());
        for e in apply_order(entries.iter().copied(), W::phase) {
            let (outcome, fx) = self.world.apply(f, e);
            let id = e.id();
            let record = StepRecord {
                entry: id,
                author: e.entry.author,
                class: e.entry.class,
                frame: f,
                outcome,
                rekeyed: fx.rekeyed.clone(),
            };
            if live {
                self.outcomes.insert(id, (f, record.outcome.clone()));
                self.note(record, Seen::of(e));
            } else {
                self.scratch_records.insert(id, (record, Seen::of(e)));
            }
            ids.push(id);
            if !fx.is_empty() {
                self.faults += u64::from(bodies::reconcile(&self.world, &mut self.physics, self.origin, &fx.changed));
                for p in &fx.pushes {
                    self.faults += u64::from(bodies::push(&mut self.physics, self.origin, p).is_err());
                }
            }
        }
        let mut events = Vec::new();
        self.logic(f, Stage::LogicPrePhysics, live, &mut events);
        // The hand bodies for this frame, rebuilt from the world model's hands and the physics state (3.3).
        let hands = self.world.hands();
        let (faults, reaches) = hands::sync(&self.world, &mut self.physics, &hands, self.origin, self.rate, f);
        self.faults += faults;
        let awake_before = self.awake(&hands);
        self.faults += u64::from(self.physics.step_holding(self.dt, &reaches).is_err());
        self.write_back(awake_before, &hands);
        self.logic(f, Stage::LogicPostPhysics, live, &mut events);
        self.faults += u64::from(self.world.maintain_transforms());
        self.world.end_frame(f);
        let next = Frame(f.0 + 1);
        self.frame = next;
        let root = self.is_cut_frame(next).then(|| self.take_root());
        if live {
            self.note_events(f, events, resimulated);
            self.stepped.insert(f, ids);
            if let Some(root) = root {
                self.live.insert(next, root);
            }
            if self.config.policy.takes(next) || self.is_cut_frame(next) {
                self.keyframe(next);
            }
            self.reached = self.reached.max(next);
        }
        root
    }

    /// One logic stage (crate::logic): the runner over the world's scripts, then the physics effects of what they
    /// changed, acted on as an entry's are. Only a live step adds to the script counts and keeps the events.
    fn logic(&mut self, f: Frame, stage: Stage, live: bool, events: &mut Vec<(ScriptEvent, Pos)>) {
        let Some(runner) = self.runner.as_deref_mut() else { return };
        if !self.world.scripted() {
            return;
        }
        let out = self.world.logic_stage(f, stage, runner);
        if !out.effects.is_empty() {
            let changed = &out.effects.changed;
            self.faults += u64::from(bodies::reconcile(&self.world, &mut self.physics, self.origin, changed));
        }
        self.counters.fp_resets += out.fp_resets;
        if live {
            self.script_counts.host_faults += u64::from(out.host_faults);
            self.script_counts.missing += out.missing.len() as u64;
            events.extend(out.events);
        }
    }

    /// Keeps frame `f`'s script effects from its live step (S5 `EffectKind::Script`): their keys are the same in every
    /// replay that emits the same events, so the view carries each once ([`Self::frame_effects`]).
    fn note_events(&mut self, f: Frame, events: Vec<(ScriptEvent, Pos)>, resimulated: bool) {
        if events.is_empty() && !self.fx.contains_key(&f) && !self.played.contains_key(&f) {
            return;
        }
        let mut ordinals: BTreeMap<(ObjectId, u64), u32> = BTreeMap::new();
        let list = (events.into_iter())
            .map(|(e, at)| {
                let kind = event_kind(&e.asset, &e.name);
                let n = ordinals.entry((e.object, kind)).or_insert(0);
                let ordinal = *n;
                *n += 1;
                let cause = EffectCause::Logic;
                let key = EffectKey { frame: f, cause, object: e.object, kind: EffectKind::Script(kind), ordinal };
                Effect { key, at, own: false, resimulated, trigger_arrived: None, data: Bytes::from(e.bytes) }
            })
            .collect();
        self.fx.insert(f, list);
        self.fx_stepped.insert(f);
    }

    /// The effects the next view carries (S5): for each frame stepped live since the last view, in frame order, those
    /// no view has carried yet, and the keys of carried ones its latest step no longer produced (`retracted`). Frames
    /// below the base frame, which no rollback reaches, are forgotten.
    fn frame_effects(&mut self) -> (Vec<Effect>, Vec<EffectKey>) {
        let (mut effects, mut retracted) = (Vec::new(), Vec::new());
        for f in std::mem::take(&mut self.fx_stepped) {
            let now = self.fx.get(&f).map_or(&[][..], Vec::as_slice);
            let played = self.played.entry(f).or_default();
            let keys: BTreeSet<EffectKey> = now.iter().map(|e| e.key).collect();
            retracted.extend(played.iter().filter(|k| !keys.contains(k)).copied());
            played.retain(|k| keys.contains(k));
            effects.extend(now.iter().filter(|e| played.insert(e.key)).cloned());
        }
        let base = self.base_frame();
        self.fx = self.fx.split_off(&base);
        self.fx.retain(|_, list| !list.is_empty());
        self.played = self.played.split_off(&base);
        self.played.retain(|_, keys| !keys.is_empty());
        (effects, retracted)
    }

    /// The awake bodies of world objects: hand bodies are the replica's own (3.3).
    fn awake(&self, hands: &[HandRow]) -> Vec<ObjectId> {
        let awake = |id: &ObjectId| self.physics.is_awake(*id).unwrap_or(false);
        self.physics.objects().filter(|id| awake(id) && !hands::is_hand(hands, *id)).collect()
    }

    /// Step 7: every body that moved writes its pose into the world model, in id order. A body awake before or after
    /// the step moved (one that fell asleep at the end of it too); static and sleeping bodies did not. A pose out of
    /// range is not written: the body stops at its last valid pose, the one its row still holds.
    fn write_back(&mut self, awake_before: Vec<ObjectId>, hands: &[HandRow]) {
        let mut moved = awake_before;
        moved.extend(self.awake(hands));
        moved.sort_unstable();
        moved.dedup();
        let (mut poses, mut refused) = (Vec::with_capacity(moved.len()), Vec::new());
        for id in moved {
            let Ok(pose) = self.physics.pose(id) else {
                self.faults += 1;
                continue;
            };
            let within = pose.position.iter().all(|c| c.abs() <= pe_physics::limits::MAX_POSITION);
            match self.origin.checked_add(Offset(pose.position)).ok().filter(|_| within) {
                Some(pos) => poses.push((id, pos, pose.rotation)),
                None => refused.push(id),
            }
        }
        // One batch, so pe-world converts every pose against the frame's final parents (@world-model).
        refused.extend(self.world.write_poses(&poses));
        refused.sort_unstable();
        for id in refused {
            self.stopped += 1;
            if let Some(row) = self.world.body(id) {
                let last = Pose { position: (row.pos - self.origin).0, rotation: row.rotation };
                self.faults += u64::from(self.physics.set_state(id, last, [0.0; 3], [0.0; 3]).is_err());
            }
        }
    }

    /// Step 10, at a cut frame: every bound body's `BodyState` and the domain's `physics_global`, then the root.
    fn take_root(&mut self) -> Hash {
        match self.physics_rows() {
            Ok((rows, global)) => self.world.write_physics(&rows, global),
            Err(_) => self.faults += 1,
        }
        self.world.root()
    }

    fn physics_rows(&mut self) -> Result<(Vec<(ObjectId, BodyStateRow)>, Hash), PhysicsError> {
        let digests = self.physics.body_digests()?;
        // Hand bodies are the replica's own: the world model has no `Body` for them, so no `BodyState` row either.
        // `physics_global` still covers them.
        let hands = self.world.hands();
        let mut rows = Vec::with_capacity(digests.len());
        for (id, digest) in digests.into_iter().filter(|(id, _)| !hands::is_hand(&hands, *id)) {
            let (linear, angular) = self.physics.velocity(id)?;
            let awake = self.physics.is_awake(id)?;
            let sleep_time = self.sleep_time(id);
            rows.push((id, BodyStateRow { linear, angular, awake, sleep_time, digest: digest.0 }));
        }
        Ok((rows, Hash(self.physics.check_hash()?.0)))
    }

    /// The body's sleep timer, read from Box3D's body (pe-physics has no accessor yet; the digest covers it too).
    fn sleep_time(&self, id: ObjectId) -> f32 {
        let body = self.physics.body_id(id).and_then(|b| usize::try_from(b.index1 - 1).ok());
        body.and_then(|i| self.physics.raw().bodies.get(i)).map_or(0.0, |b| b.sleep_time)
    }

    fn keyframe(&mut self, at: Frame) {
        match self.physics.save_image() {
            Ok(image) => self.ring.insert(at, Key { world: self.world.save(), image }),
            Err(_) => self.faults += 1,
        }
    }

    /// Restore the newest keyframe at or before `to` as the live state, dropping what the live history held after it.
    fn roll_back(&mut self, to: Frame) -> Option<Frame> {
        let ReplicaImpl { ring, world, physics, .. } = self;
        let (k, key) = ring.at_or_before(to)?;
        // Physics first: it refuses a bad image before writing anything, so a refusal leaves the world and the
        // physics at the same frame (@chunks-reviewer's L2 on 40f9cca4).
        if physics.restore_image(&key.image).is_err() {
            self.faults += 1;
            return None;
        }
        world.restore(&key.world);
        self.frame = k;
        self.ring.truncate_after(k);
        self.forget_after(k);
        Some(k)
    }

    /// A running replay met a frame below the floor that S3's byte cap trimmed after the replay began. The replay
    /// cannot finish: it ends, and asked again it answers `NotKept`. Everything from the floor up is still kept, so
    /// the live state goes back to the floor's held snapshot, which is the pinned keyframe there again, and the next
    /// call re-simulates from the floor to `reached`. The member keeps simulating while rung 2 goes ahead
    /// (@design-reviewer, m76a7a148f6b5). Outcomes below the floor stay as the replay left them: the replay's own up
    /// to where it stopped, none above. False if the floor's snapshot is not held in this physics world or is refused
    /// (a fault): the live state is then unchanged, and the caller stops instead.
    fn abandon_replay(&mut self) -> bool {
        let Some((at, root)) = self.floor.as_ref().map(|f| (f.cut.at.frame, f.root)) else { return false };
        let Some(key) = self.held_here(&root).map(|h| Key { world: h.world.clone(), image: h.image.clone() }) else {
            return false;
        };
        // `replay_to` dropped the floor's keyframe with the history it was replacing.
        self.ring.insert(at, key);
        let cv = self.cv.as_ref().map(|c| c.0.at.frame);
        self.ring.pin([Some(at), cv].into_iter().flatten());
        if self.roll_back(at) != Some(at) {
            return false;
        }
        self.lost = self.replay.take().map(|r| (r.cut.at, RootError::NotKept));
        // The replay's history below the floor goes; the next `advance` trims its keyframes there.
        self.live.retain(|f, _| *f >= at);
        self.stepped.retain(|f, _| *f >= at);
        self.live.insert(at, root);
        self.counters.deepest_rollback = self.counters.deepest_rollback.max(self.reached.0.saturating_sub(at.0));
        true
    }

    /// The live history after frame `k` is being replaced: its roots, stepped ids and outcomes go.
    fn forget_after(&mut self, k: Frame) {
        self.live.retain(|f, _| *f <= k);
        self.stepped.retain(|f, _| *f < k);
        self.outcomes.retain(|_, (f, _)| *f < k);
    }

    /// Step the live history toward `present` within `budget` frames, continuing a replay first.
    fn run(&mut self, present: Frame, source: &dyn EntrySource, budget: &mut u64, out: &mut Advance) {
        loop {
            let target = if self.replay.is_some() { self.reached.max(present) } else { present };
            if self.frame >= target || *budget == 0 {
                break;
            }
            let Some(entries) = self.entries_for(source, self.frame) else {
                // No longer kept (S3 v1.2), and never stepped empty. Below the floor, a running replay met S3's byte
                // cap, which may trim there at any time: the replay is abandoned, and the live state goes back to the
                // floor, which is still kept.
                if self.replay.is_some() && self.frame < self.floor_frame() && self.abandon_replay() {
                    break;
                }
                // At or above the floor only the time cap trims (or S3 broke its contract): the live history stops
                // here, one fault per stop, until the frame can be stepped. A replay cannot finish past it.
                if self.stuck_at != Some(self.frame) {
                    self.faults += 1;
                    self.stuck_at = Some(self.frame);
                }
                // A replay stops here too: its root stands if it took it below this frame; otherwise its cut's root
                // needs this frame, and asked again it answers `Damaged`.
                if let Some(r) = self.replay.take() {
                    match r.root {
                        Some(root) => self.replayed = Some((r.cut.at, root)),
                        None => self.lost = Some((r.cut.at, RootError::Damaged)),
                    }
                }
                break;
            };
            let resimulated = self.frame < self.reached;
            let before = self.frame;
            let root = self.step(&entries.iter().collect::<Vec<_>>(), true);
            // Refused: a world with scripts and no runner (crate::logic). The live history stops here.
            if self.frame == before {
                break;
            }
            if resimulated && self.frame == self.view.frame {
                self.note_corrections();
            }
            // Stepped past the frame it had stopped at: S3 answers for it again.
            self.stuck_at = self.stuck_at.filter(|&f| f >= self.frame);
            *budget -= 1;
            if resimulated {
                out.resimulated += 1;
            } else {
                out.stepped += 1;
            }
            let frame = self.frame;
            if let Some(r) = self.replay.as_mut().filter(|r| r.cut.at.frame == frame) {
                r.root = root;
            }
            if self.replay.as_ref().is_some_and(|r| r.root.is_some() && frame >= self.reached) {
                let r = self.replay.take().expect("checked");
                self.replayed = r.root.map(|root| (r.cut.at, root));
            }
        }
    }

    // ---- roots at cuts ----------------------------------------------------------------------------------------------

    // The d1 rule (a member votes and compares only at cuts whose entries it holds) is S3's alone: `wants_roots` and
    // `final_cut` hand over only such cuts. The replica cannot check it itself: a cut may cover void entries (late, or
    // stamped back), which `entries_at` rightly leaves out, so counting the entries it sees would refuse such a cut
    // for ever (@harness-eng, mcc79fdb5df77, under S3's covered-only void rule). So it never answers `MissingEntries`.

    /// The first frame from the floor below the cut's frame whose live entries are not exactly the cut's, compared in
    /// apply order, the order `stepped` records.
    fn first_difference(&self, cut: &Cut, source: &dyn EntrySource) -> Option<Frame> {
        (self.base_frame().0..cut.at.frame.0).map(Frame).find(|&g| {
            // A frame no longer kept counts as different (`my_root` refuses before asking).
            let Some(all) = source.entries_for_cut(cut, g) else { return true };
            let want: Vec<EntryRef> = apply_order(&all, W::phase).iter().map(|e| e.id()).collect();
            self.stepped.get(&g).is_none_or(|had| *had != want)
        })
    }

    /// My root at the cut, computed (no cache) and held.
    fn my_root(&mut self, cut: &Cut, source: &dyn EntrySource) -> Result<Hash, RootError> {
        // At my last import's cut, my root is the imported one, with no re-step below it (S2 v1.2).
        if let Some((_, root)) = self.imported.filter(|i| i.0 == cut.at) {
            return Ok(root);
        }
        let at = cut.at.frame;
        if at < self.floor_frame() {
            return Err(RootError::NotKept);
        }
        // Below my last import there is no history of mine, so no root of mine will come: rung 2 is due there.
        if self.imported.is_some_and(|i| at < i.0.frame) {
            return Err(RootError::Damaged);
        }
        if self.stopped_short_of(at) {
            return Err(RootError::Damaged);
        }
        if at > self.frame {
            let coming = at <= self.reached || self.replay.is_some();
            return Err(if coming { RootError::Pending } else { RootError::NotYetSimulated });
        }
        if self.replay.as_ref().is_some_and(|r| at > r.cut.at.frame || r.root.is_none()) {
            return Err(RootError::Pending);
        }
        // Every frame from the floor to the cut's is needed: to compare with the live history, and to re-step where
        // it differs. One S3 no longer keeps (its time cap, for a replica far behind, or an S3 fault) means no root
        // here will ever come: rung 2 is due (@chunks-reviewer's Medium on 3d8e9442). One fault per such frame, since
        // the driver asks again each tick.
        let floor = self.base_frame();
        if let Some(gone) = (floor.0..at.0).map(Frame).find(|&f| source.entries_at(f).is_none()) {
            if self.unkept_met != Some(gone) {
                self.faults += 1;
                self.unkept_met = Some(gone);
            }
            return Err(RootError::Damaged);
        }
        let from = self.first_difference(cut, source).unwrap_or(at);
        let (root, held) = self.state_at_cut(cut, from, source)?;
        if from == at {
            // The live history stepped exactly the cut's entries, through the same frame function.
            debug_assert!(self.live.get(&at).is_none_or(|&live| live == root), "the live root at {at:?} is the cut's");
        }
        // The same root held from before my last import has the old physics world's image: this one replaces it.
        if self.held_here(&root).is_none() {
            self.held.insert(root, held);
        }
        Ok(root)
    }

    /// My root at the cut and its snapshot, from the newest keyframe at or before `from`, stepping exactly the cut's
    /// entries to its frame. The live state is put back.
    fn state_at_cut(
        &mut self,
        cut: &Cut,
        from: Frame,
        source: &dyn EntrySource,
    ) -> Result<(Hash, Held<W::Saved>), RootError> {
        let live = (self.world.save(), self.physics.save_image().map_err(|_| RootError::Damaged)?, self.frame);
        let tally = (self.faults, self.stopped, self.counters.fp_resets);
        self.scratch_records.clear();
        let result = self.scratch_to(cut, from.min(cut.at.frame), source);
        // What the re-step counted is the scratch's: the live counts stay as they were (L1).
        self.scratch.faults += self.faults - tally.0;
        self.scratch.stopped += self.stopped - tally.1;
        self.scratch.fp_resets += self.counters.fp_resets - tally.2;
        (self.faults, self.stopped, self.counters.fp_resets) = tally;
        self.world.restore(&live.0);
        if self.physics.restore_image(&live.1).is_err() {
            self.faults += 1;
        }
        self.frame = live.2;
        result
    }

    fn scratch_to(
        &mut self,
        cut: &Cut,
        from: Frame,
        source: &dyn EntrySource,
    ) -> Result<(Hash, Held<W::Saved>), RootError> {
        let at = cut.at.frame;
        let ReplicaImpl { ring, world, physics, faults, .. } = self;
        let (k, key) = ring.at_or_before(from).ok_or(RootError::NotKept)?;
        world.restore(&key.world);
        // A refused physics call is a fault, counted (@chunks-reviewer's nit on 7a360722); here it is the scratch's.
        if physics.restore_image(&key.image).is_err() {
            *faults += 1;
            return Err(RootError::Damaged);
        }
        self.frame = k;
        let mut root = None;
        while self.frame < at {
            let entries = source.entries_for_cut(cut, self.frame).ok_or(RootError::NotKept)?;
            let before = self.frame;
            root = self.step(&entries.iter().collect::<Vec<_>>(), false);
            // Refused: a world with scripts and no runner (crate::logic), so this replica cannot compute the root.
            if self.frame == before {
                return Err(RootError::NotYetSimulated);
            }
        }
        // Stepped onto a grid frame: its root was just taken. Otherwise (a keyframe restored at the cut's frame, or a
        // cut off the grid) take it here; at a grid keyframe the rows are already written, so nothing changes.
        let root = match root {
            Some(root) => root,
            None => self.take_root(),
        };
        let (Ok(transfer), Ok(image)) = (self.physics.save_transfer(), self.physics.save_image()) else {
            self.faults += 1;
            return Err(RootError::Damaged);
        };
        let global = Hash(transfer.check_hash().0);
        let transfer = Bytes::from(transfer.to_bytes());
        let held = Held { frame: at, world: self.world.save(), image, transfer, global, epoch: self.epoch };
        Ok((root, held))
    }

    /// Release held roots nobody needs: kept are the floor's, `Cv`'s, the finals within `T_keep` (at least two), the
    /// diverged root, and my roots at cuts not final yet.
    fn release(&mut self) {
        let keep_frames = self.rate.frames(self.config.keep_finals);
        while self.finals.len() > 2 {
            let (newest, oldest) = (self.finals[self.finals.len() - 1].0.frame, self.finals[0].0.frame);
            if newest.0.saturating_sub(oldest.0) <= keep_frames {
                break;
            }
            self.finals.pop_front();
        }
        let mut keep: Vec<Hash> = self.finals.iter().map(|f| f.1).collect();
        keep.extend(self.floor.as_ref().map(|f| f.root));
        keep.extend(self.cv.as_ref().map(|c| c.1));
        keep.extend(self.diverged.map(|d| d.1));
        keep.extend(self.voted.values().copied());
        keep.extend(self.imported.map(|i| i.1));
        self.held.retain(|h, _| keep.contains(h));
    }

    /// The floor moves to the cut: its snapshot becomes the pinned keyframe there, and the history below it goes. A
    /// snapshot held from before my last import never enters the ring: its image is the old physics world's.
    fn move_floor(&mut self, cut: &Cut, root: Hash) {
        let at = cut.at.frame;
        if let Some(key) = self.held_here(&root).map(|h| Key { world: h.world.clone(), image: h.image.clone() }) {
            self.ring.insert(at, key);
        }
        self.floor = Some(Floor { cut: cut.clone(), root });
        let cv = self.cv.as_ref().map(|c| c.0.at.frame);
        self.ring.pin([Some(at), cv].into_iter().flatten());
        self.ring.trim(self.config.policy, self.frame, at);
        self.live.retain(|f, _| *f >= at);
        self.stepped.retain(|f, _| *f >= at);
        let oldest = self.finals.front().map_or(at, |f| f.0.frame);
        self.outcomes.retain(|_, (f, _)| *f >= oldest);
    }

    /// The floor just moved over live frames that stepped other entries than the cut's, from `g` on: a late entry the
    /// cut did not cover, voided as the cut went final, often by my own vote inside `drive`. The live state goes back
    /// to the floor's cut-exact snapshot at once, with the cut-exact root and outcomes below the floor, and the next
    /// call re-simulates to `reached`. S3's change report inside `[g, floor)` is then expected, and excused once
    /// (@chunks-reviewer's M1 on 40f9cca4, option (a) by @coordinator-opus, mb9b8ce2ae8da).
    fn rebase_on_floor(&mut self, g: Frame, root: Hash) {
        let at = self.floor_frame();
        if let Some(k) = self.roll_back(at) {
            self.counters.deepest_rollback = self.counters.deepest_rollback.max(self.reached.0 - k.0);
        }
        self.live.insert(at, root);
        // finalize's my_root just re-stepped `[k, at)` with exactly the cut's entries, k at or before g.
        self.outcomes.retain(|_, (f, _)| *f < g);
        let scratch = std::mem::take(&mut self.scratch_records);
        for (id, (r, seen)) in scratch.into_iter().filter(|(_, (r, _))| (g..at).contains(&r.frame)) {
            self.outcomes.insert(id, (r.frame, r.outcome.clone()));
            self.note(r, seen);
        }
        self.excused.push((g, at));
    }

    /// A change record for the next view, and its entry remembered until `final_frame` passes its frame (S5).
    fn note(&mut self, r: StepRecord, seen: Seen) {
        if self.cv.as_ref().is_none_or(|c| r.frame >= c.0.at.frame) {
            self.recorded.entry(r.frame).or_default().insert(r.entry, seen);
        }
        self.pending.push(r);
    }

    /// `final_frame` is about to pass every frame below `to` (S5 v1.5).
    ///
    /// - **Dropped.** Each entry I recorded there that `cut`'s order leaves out gets a `Dropped` record, with S3's
    ///   reason ([`EntrySource::void_reason`]), or `Late` for one no cut has voided yet, as S3 will void it.
    /// - **Unknown,** after a rung-2 import (`unknown_from`): each entry of the cut's order from that frame up gets an
    ///   `Unknown` record, since I stepped it on the wrong state, or not at all.
    ///
    /// Where S3 no longer keeps a frame, the cut's heads still say whether it includes an entry I recorded: included
    /// is `Unknown` after an import (applied or void, I can't tell), and not included is `Dropped`.
    fn settle(&mut self, cut: &Cut, to: Frame, source: &dyn EntrySource, unknown_from: Option<Frame>) {
        let rest = self.recorded.split_off(&to);
        let mine = std::mem::replace(&mut self.recorded, rest);
        let kept = source.kept_from();
        let frames: BTreeSet<Frame> = (mine.keys().copied())
            .chain(unknown_from.into_iter().flat_map(|u| (u.max(kept).0..to.0).map(Frame)))
            .collect();
        let unknown = |entry: EntryRef, author: MemberKey, class: Class, frame: Frame| StepRecord {
            entry,
            author,
            class,
            frame,
            outcome: Outcome::Unknown { repaired_at: cut.at },
            rekeyed: Arc::default(),
        };
        let mut records = Vec::new();
        for f in frames {
            let here = mine.get(&f).into_iter().flatten();
            let repaired = unknown_from.is_some_and(|u| f >= u);
            let dropped = |entry: EntryRef, seen: &Seen| {
                let reason = source.void_reason(&entry).unwrap_or(VoidReason::Late);
                let (author, class) = (seen.author, seen.class);
                StepRecord {
                    entry,
                    author,
                    class,
                    frame: f,
                    outcome: Outcome::Dropped { reason },
                    rekeyed: Arc::default(),
                }
            };
            match source.entries_for_cut(cut, f) {
                Some(order) => {
                    if repaired {
                        records.extend(order.iter().map(|e| unknown(e.id(), e.entry.author, e.entry.class, f)));
                    }
                    for (&entry, seen) in here {
                        if !order.iter().any(|e| e.id() == entry) {
                            records.push(dropped(entry, seen));
                        }
                    }
                }
                None => {
                    for (&entry, seen) in here {
                        if !cut.includes(&seen.chain, seen.seq) {
                            records.push(dropped(entry, seen));
                        } else if repaired {
                            records.push(unknown(entry, seen.author, seen.class, f));
                        }
                    }
                }
            }
        }
        self.pending.extend(records);
    }

    fn publish(&mut self) {
        // What the model draws, with what only the replica knows: the velocities and the local member's hands.
        let mut objects = self.world.view_objects();
        for o in &mut objects {
            if let Ok((linear, angular)) = self.physics.velocity(o.id) {
                o.linear_velocity = linear;
                o.angular_velocity = angular;
            }
            if let (Some(me), Some((owner, _))) = (self.config.local, o.hand_of)
                && owner == me
            {
                o.flags = o.flags.with(ViewFlags::OWN_HAND);
            }
            // Only a hold by one of the local member's own hands: a hand-less hold (an agent's) has no tracked hand
            // to be drawn from, so it stays corrected like any other object.
            if let (Some(me), Some(holder)) = (self.config.local, &o.held_by)
                && holder.member == me
                && holder.hand != ObjectId::NONE
            {
                o.flags = o.flags.with(ViewFlags::HELD_LOCAL);
            }
            // The local member's hands and what they hold are drawn from tracking, never corrected (S5).
            let local = o.flags.contains(ViewFlags::OWN_HAND) || o.flags.contains(ViewFlags::HELD_LOCAL);
            if !local && self.corrected.contains(&o.id) {
                o.flags = o.flags.with(ViewFlags::CORRECTED);
            }
        }
        self.corrected.clear();
        let sim_time = self.config.clock.host_time_of(self.frame);
        let mut view = empty_view(self.domain);
        view.frame = self.frame;
        view.sim_time = sim_time;
        view.final_frame = self.cv.as_ref().map(|c| c.0.at.frame);
        view.health = self.repairing_since.map_or(Health::Agreed, |since| Health::Repairing { since });
        view.changes = std::mem::take(&mut self.pending);
        view.objects = objects;
        (view.effects, view.retracted) = self.frame_effects();
        view.counters = self.counters;
        self.view = Arc::new(view);
    }
}

fn empty_view(domain: DomainId) -> FrameView {
    FrameView {
        frame: Frame(0),
        domain,
        sim_time: HostTime::default(),
        final_frame: None,
        health: Health::Agreed,
        local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
        objects: Vec::new(),
        effects: Vec::new(),
        retracted: Vec::new(),
        counters: ViewCounters::default(),
        changes: Vec::new(),
    }
}

/// The physics configuration for a world with `settings`, stepped at `rate`: the engine's configuration with the
/// world's gravity and the rate's sub-steps. The one place a physics world's configuration is made, so none keeps a
/// setting from an earlier world (world-model.md 6.6; @coordinator-opus m1eb5e9623174).
fn physics_config(
    engine: PhysicsConfig,
    settings: &PhysicsSettings,
    rate: SimRate,
) -> Result<PhysicsConfig, PhysicsError> {
    PhysicsConfig { gravity: settings.gravity, ..engine }.with_tick_rate(rate.hz())
}

/// A new physics world from a canonical transfer whose check hash is `global`; `None` if it is not one.
fn restored(config: PhysicsConfig, bytes: &[u8], global: Hash) -> Option<PhysicsWorld> {
    let transfer = Transfer::from_bytes(bytes).ok()?;
    PhysicsWorld::from_transfer(config, &transfer, &CheckHash(global.0)).ok()
}

impl<W: WorldModel> ReplicaImpl<W> {
    /// A re-simulation has come back to the last published frame: every drawn object whose pose there differs from
    /// the published one was rewritten by a replay, a repair or a merge, and the next publication marks it
    /// `CORRECTED` (S5). The re-simulation is deterministic, so an unchanged object compares equal bit for bit. An
    /// object new since the publication is not a correction. A rung-2 import at a cut at or past the last published
    /// frame re-simulates nothing below it, so its changes are not marked and the display snaps; imports are at a
    /// final cut below the present, so this is rare (@vdw-physics's note).
    fn note_corrections(&mut self) {
        let published: BTreeMap<ObjectId, (Pos, [f32; 4])> =
            self.view.objects.iter().map(|o| (o.id, (o.pos, o.rotation))).collect();
        for o in self.world.view_objects() {
            if published.get(&o.id).is_some_and(|&shown| shown != (o.pos, o.rotation)) {
                self.corrected.insert(o.id);
            }
        }
    }
}

pub(crate) fn bounding_radius(shapes: &[ShapePart]) -> f32 {
    let len = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    shapes
        .iter()
        .map(|p| match p.shape {
            ShapeRow::Sphere { center, radius } => len(center) + radius,
            ShapeRow::Capsule { center1, center2, radius } => len(center1).max(len(center2)) + radius,
            ShapeRow::Box { center, half_extents, .. } => len(center) + len(half_extents),
        })
        .fold(0.0, f32::max)
}

impl<W: WorldModel> Replica for ReplicaImpl<W> {
    fn domain(&self) -> DomainId {
        self.domain
    }

    fn frame(&self) -> Frame {
        self.frame
    }

    fn set_budget(&mut self, simulated: Duration) {
        let frames = simulated.as_nanos().saturating_mul(u128::from(self.rate.hz())) / 1_000_000_000;
        self.budget = u64::try_from(frames).unwrap_or(u64::MAX).max(2);
    }

    fn advance(&mut self, present: Frame, source: &mut dyn EntrySource) -> Advance {
        let mut out = Advance::default();
        let mut budget = self.budget;
        // A span a finalize re-based excuses one report, the first after it; unused, it lapses here.
        let excused = std::mem::take(&mut self.excused);
        if let Some(f) = source.take_changed_from() {
            let expected = excused.iter().any(|&(g, floor)| (g..floor).contains(&f));
            if f < self.base_frame() && !expected {
                // A protocol fault (4.2): reported, and not acted on below the floor, or below my last import, whose
                // history the replica no longer has.
                self.faults += 1;
            }
            let to = f.max(self.rollback_floor());
            if let Some(k) = (to < self.frame).then(|| self.roll_back(to)).flatten() {
                out.rolled_back_to = Some(k);
                self.counters.deepest_rollback = self.counters.deepest_rollback.max(self.reached.0 - k.0);
            }
        }
        self.run(present, &*source, &mut budget, &mut out);
        // The ring's policy holds without a finalize too, so memory stays logarithmic in the unconfirmed span.
        self.ring.trim(self.config.policy, self.frame, self.floor_frame());
        self.counters.frames_resimulated += out.resimulated;
        self.counters.corrections += u32::from(out.resimulated > 0);
        out.caught_up = self.frame >= present && self.replay.is_none();
        if out.caught_up && (out.stepped > 0 || out.resimulated > 0) {
            self.publish();
        }
        out
    }

    /// The outcome my live history gave `entry`, and its frame. `None` for an entry this replica has not applied: one
    /// not stepped yet, one in a frame an abandoned replay never reached, or one above the floor and below my last
    /// import's cut, whose history the import replaced (@design-reviewer's Low on 7a360722).
    fn outcome(&self, entry: &EntryRef) -> Option<(Frame, Outcome)> {
        self.outcomes.get(entry).cloned()
    }

    fn root_at_cut(&mut self, cut: &Cut, source: &mut dyn EntrySource) -> Result<Hash, RootError> {
        // S3 v1.2: stopped at a frame no longer kept, it gives no root past it, not even one computed before.
        if self.stopped_short_of(cut.at.frame) {
            return Err(RootError::Damaged);
        }
        // After an import at the cut, my root there is the imported one, even at my floor's cut (rung 2 after a
        // divergence there), until finalize takes it.
        if let Some((_, root)) = self.imported.filter(|i| i.0 == cut.at) {
            return Ok(root);
        }
        if let Some(floor) = self.floor.as_ref().filter(|f| f.cut.at == cut.at) {
            return Ok(floor.root);
        }
        if let Some(&root) = self.voted.get(&cut.at) {
            return Ok(root);
        }
        let root = self.my_root(cut, &*source)?;
        self.voted.insert(cut.at, root);
        Ok(root)
    }

    fn finalize(&mut self, cut: &FinalCut, source: &mut dyn EntrySource) -> Finalized {
        let at = cut.cut.at;
        let repeat = self.floor.as_ref().is_some_and(|f| f.cut.at == at);
        if repeat && self.diverged.is_none_or(|d| d.0 != at) {
            return Finalized::Agreed;
        }
        if !repeat && at.frame < self.floor_frame() {
            // Older than my floor: the source's final cut never goes back, so this is a protocol fault.
            self.faults += 1;
            return Finalized::NotReady;
        }
        // My root at the cut: recomputed, since after a repair it is the repaired history's. `Damaged`: a frame from
        // my floor to the cut is no longer kept, or I stopped short of the cut at one, so no root will ever come and
        // rung 2 is due; `my_root` counted the fault. Otherwise I have not re-simulated to the cut yet.
        let mine = match self.my_root(&cut.cut, &*source) {
            Ok(mine) => mine,
            Err(RootError::Damaged) => {
                self.repairing_since.get_or_insert(at.frame);
                return Finalized::Damaged;
            }
            Err(_) => return Finalized::NotReady,
        };
        // Whether the live history below the cut's frame stepped exactly the cut's entries, asked before the floor
        // moves past those frames.
        let differs = self.first_difference(&cut.cut, &*source);
        self.voted.retain(|c, _| c.height > at.height);
        self.finals.retain(|f| f.0 != at);
        self.finals.push_back((at, mine));
        let verdict = if mine == cut.root {
            self.cv = Some((cut.cut.clone(), mine));
            self.diverged = None;
            self.repairing_since = None;
            Finalized::Agreed
        } else {
            self.diverged = Some((at, mine));
            self.repairing_since.get_or_insert(at.frame);
            Finalized::Diverged { mine }
        };
        self.move_floor(&cut.cut, mine);
        if self.imported.is_some_and(|i| i.0.frame <= at.frame) {
            self.imported = None;
        }
        // What I still need (S3 v1.2): from `Cv`'s frame for rung 1's replay, from the floor's for re-simulation. The
        // floor moved either way; S3 ignores a frame that does not move forward.
        // `final_frame` (my last agreed cut) passes the cut's frame at the next publication: first, a `Dropped` record
        // for each entry I stepped below it that the cut's order leaves out, while S3 still keeps those frames.
        if matches!(verdict, Finalized::Agreed) {
            self.settle(&cut.cut, at.frame, &*source, None);
        }
        let cv = self.cv.as_ref().map_or(Frame(0), |c| c.0.at.frame);
        source.keep_final_from(cv, at.frame);
        if let Some(g) = differs {
            self.rebase_on_floor(g, mine);
        }
        self.release();
        verdict
    }

    fn replay_to(&mut self, cut: &Cut, source: &mut dyn EntrySource) -> Result<Hash, RootError> {
        if let Some((_, root)) = self.replayed.filter(|r| r.0 == cut.at) {
            return Ok(root);
        }
        if let Some(r) = self.replay.as_ref().filter(|r| r.cut.at == cut.at) {
            return r.root.ok_or(RootError::Pending);
        }
        if let Some((_, e)) = self.lost.filter(|l| l.0 == cut.at) {
            return Err(e);
        }
        let cv_root = self.cv.as_ref().map(|c| c.1).ok_or(RootError::NotKept)?;
        let cv_frame = self.cv.as_ref().map_or(Frame(0), |c| c.0.at.frame);
        // S3 v1.2: below `kept_from` the entries are gone, so a replay from `Cv` would step empty frames. And a `Cv`
        // held from before my last import has the old physics world's image, which never restores into the new one
        // (@chunks-reviewer's L2 on 05903341). Both are refused before anything is restored, so the live state is
        // unchanged and the ladder goes on to rung 2.
        if cut.at.frame < cv_frame || self.held_here(&cv_root).is_none() || cv_frame < source.kept_from() {
            return Err(RootError::NotKept);
        }
        // Check `Cv` from its records and from the restored physics itself, keeping the live state until it passes.
        let live = (self.world.save(), self.physics.save_image().map_err(|_| RootError::Damaged)?);
        let held = &self.held[&cv_root];
        let sound = self.world.tree(&held.world).verify().is_ok() && {
            self.world.restore(&held.world);
            self.physics.restore_image(&held.image).is_ok()
                && self.physics.check_hash().is_ok_and(|c| Hash(c.0) == held.global)
                && self.world.root() == cv_root
        };
        if !sound {
            self.world.restore(&live.0);
            self.faults += u64::from(self.physics.restore_image(&live.1).is_err());
            return Err(RootError::Damaged);
        }
        // The live history starts again at `Cv`, with exactly the cut's entries below the cut's frame. Everything the
        // ring held after `Cv` belongs to the history being replaced, the floor's keyframe included.
        self.frame = cv_frame;
        self.ring.rewind_to(cv_frame);
        self.forget_after(cv_frame);
        self.voted.clear();
        self.replayed = None;
        self.lost = None;
        let root = (cut.at.frame == cv_frame).then_some(cv_root);
        self.replay = Some(Replay { cut: cut.clone(), root });
        let (mut budget, mut out) = (self.budget, Advance::default());
        let present = self.reached;
        self.run(present, &*source, &mut budget, &mut out);
        self.counters.frames_resimulated += out.resimulated;
        match (&self.replay, self.replayed) {
            (Some(r), _) => r.root.ok_or(RootError::Pending),
            (None, Some((_, root))) => Ok(root),
            // It met a frame no longer kept on its first stint, and ended.
            (None, None) => Err(self.lost.filter(|l| l.0 == cut.at).map_or(RootError::Damaged, |l| l.1)),
        }
    }

    fn tree(&self, root: &Hash) -> Option<Box<dyn Tree + '_>> {
        self.held.get(root).map(|h| self.world.tree(&h.world))
    }

    fn physics_image(&self, root: &Hash) -> Option<PhysicsImage> {
        self.held.get(root).map(|h| PhysicsImage {
            domain: self.domain,
            frame: h.frame,
            global: h.global,
            bytes: h.transfer.clone(),
        })
    }

    /// Rung 2 and joining (S2 v1.2 round 3; replica.md 6.2). Every group is the domain in M1. Everything is checked
    /// before the live state changes: the records by the world model, the physics image by
    /// `PhysicsWorld::from_transfer` (`PhysicsCheck`), and the patched state's root against the target
    /// (`RootMismatch`). The base is my own state at the cut when I hold my root there, else the empty state; without
    /// an image, the physics is my own at the cut, or an empty world. The new physics world takes the imported world's
    /// gravity and origin, and an imported rate other than mine is `RateMismatch` (world-model.md 6.6). The live state
    /// then becomes the imported one at the cut's frame, in that physics world: the ring starts again at the cut, the
    /// replica keeps no history below it, and no image held from before is restored again. It re-simulates to the present within the budget, through
    /// the replay, answering `Pending` until it has. The same import asked again answers its progress; it is not
    /// imported twice. A cut below my floor answers `NotKept`.
    fn import_at_cut(
        &mut self,
        _group: &GroupSel,
        import: CutImport,
        source: &mut dyn EntrySource,
    ) -> Result<Hash, ImportError> {
        let cut = import.cut.clone();
        if let Some((at, root)) = self.imported.filter(|i| i.0 == cut.at && i.1 == import.target_root) {
            let running = self.replay.as_ref().is_some_and(|r| r.cut.at == at);
            return if running { Err(ImportError::Pending) } else { Ok(root) };
        }
        let c = cut.at.frame;
        // Below my floor nothing changes again, and the driver never asks there (@chunks-reviewer's L1 on 05903341).
        if c < self.floor_frame() {
            return Err(ImportError::NotKept);
        }
        let mine = (self.voted.get(&cut.at).copied())
            .or(self.diverged.filter(|d| d.0 == cut.at).map(|d| d.1))
            .or(self.finals.iter().find(|f| f.0 == cut.at).map(|f| f.1))
            .or(self.floor.as_ref().filter(|f| f.cut.at == cut.at).map(|f| f.root));
        let own = mine.and_then(|r| self.held.get(&r)).filter(|h| h.frame == c);
        let base = own.map_or_else(|| self.world.empty(), |h| h.world.clone());
        let own_physics = own.map(|h| (h.transfer.clone(), h.global));
        let patched = self.world.import(&base, &import)?;
        // The new physics world is built for the imported world's settings, never from mine (world-model.md 6.6;
        // @coordinator-opus m1eb5e9623174): its gravity, and its origin below. A world runs at one rate, so another
        // rate than mine is a fault to report, never one to adopt.
        let live = self.world.save();
        self.world.restore(&patched);
        let settings = self.world.settings();
        let physics = if settings.rate == self.rate {
            physics_config(self.config.physics, &settings, self.rate)
                .ok()
                .and_then(|config| match (&import.physics, own_physics) {
                    (Some(image), _) => restored(config, &image.bytes, image.global),
                    (None, Some((bytes, global))) => restored(config, &bytes, global),
                    (None, None) => PhysicsWorld::new(config).ok(),
                })
                .ok_or(ImportError::PhysicsCheck)
        } else {
            Err(ImportError::RateMismatch { world: settings.rate, mine: self.rate })
        };
        let physics = match physics {
            Ok(physics) => physics,
            Err(e) => {
                self.world.restore(&live);
                return Err(e);
            }
        };
        // The patched state's root in the new physics world; the live state comes back unless it is the target.
        let old = std::mem::replace(&mut self.physics, physics);
        let old_origin = std::mem::replace(&mut self.origin, settings.origin);
        let root = self.take_root();
        let saved = (root == import.target_root).then(|| (self.physics.save_transfer(), self.physics.save_image()));
        let Some((Ok(transfer), Ok(image))) = saved else {
            self.world.restore(&live);
            self.physics = old;
            self.origin = old_origin;
            return Err(if root == import.target_root {
                ImportError::PhysicsCheck
            } else {
                ImportError::RootMismatch { got: root }
            });
        };
        // `final_frame` passes the cut once the drive finalizes it again, and my records below it were made on the
        // wrong state: each entry of the cut's order from my last agreed cut up is `Unknown` here, and one I stepped
        // that the cut leaves out is `Dropped` (S5 v1.5).
        let from = self.cv.as_ref().map_or(Frame(0), |c| c.0.at.frame).max(self.unknown_to);
        self.settle(&cut, c, &*source, Some(from));
        self.unknown_to = self.unknown_to.max(c);
        // The live history starts again at the cut. What my replaced history held above the floor is gone, its
        // outcomes included: entries there that I never applied report none.
        let floor = self.floor_frame();
        self.frame = c;
        self.reached = self.reached.max(c);
        self.ring = Ring::new();
        self.ring.insert(c, Key { world: self.world.save(), image: image.clone() });
        self.ring.pin([c]);
        self.forget_after(c);
        self.live.retain(|f, _| *f < floor);
        self.live.insert(c, root);
        self.stepped.retain(|f, _| *f < floor);
        self.outcomes.retain(|_, (f, _)| *f < floor);
        // A new physics world: the snapshots held from before keep their records and transfers for exports, but none
        // of their images is restored again.
        self.epoch += 1;
        let global = Hash(transfer.check_hash().0);
        let transfer = Bytes::from(transfer.to_bytes());
        let held = Held { frame: c, world: self.world.save(), image, transfer, global, epoch: self.epoch };
        self.held.insert(root, held);
        self.voted.clear();
        (self.replay, self.replayed, self.lost, self.stuck_at, self.unkept_met) = (None, None, None, None, None);
        self.imported = Some((cut.at, root));
        if self.frame >= self.reached {
            self.replayed = Some((cut.at, root));
            return Ok(root);
        }
        self.replay = Some(Replay { cut, root: Some(root) });
        let (mut budget, mut out) = (self.budget, Advance::default());
        let present = self.reached;
        self.run(present, &*source, &mut budget, &mut out);
        self.counters.frames_resimulated += out.resimulated;
        match (&self.replay, self.replayed) {
            (Some(_), _) => Err(ImportError::Pending),
            (None, Some((_, r))) if r == root => Ok(root),
            _ => Err(ImportError::NotKept),
        }
    }

    fn view(&self) -> Arc<FrameView> {
        self.view.clone()
    }

    fn world_snapshot(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        // The final frame the view names: the last agreed cut's, or the genesis cut's before any.
        self.world.snapshot(self.cv.as_ref().map_or(Frame(0), |c| c.0.at.frame))
    }

    fn agreed_snapshot(&self) -> Option<(Frame, Arc<dyn std::any::Any + Send + Sync>)> {
        let (frame, saved) = self.agreed_state()?;
        W::snapshot_of(saved, frame).map(|s| (frame, s))
    }
}
