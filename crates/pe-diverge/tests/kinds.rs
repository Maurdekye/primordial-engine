//! M1's "What the hash covers", repair half (@item:primordial-engine/m1-test-a-divergence-in-each-hashed-kind-is-foun):
//! a divergence in each hashed kind `pe_world::test_util` can damage is found at a final cut and repaired by the
//! ladder, on the real pe-world replica (`ReplicaImpl<PeWorld>`), not the stand-in net of tests/common.
//!
//! - Rung 1: the victim's live world is damaged once, after a verified cut. The next final cut finds it, and the replay
//!   from `Cv` cures it with nothing fetched.
//! - Rung 2: the victim starts from a damaged world, so no replay can cure it. The walk finds exactly the leaf the
//!   damage names, only that record is fetched and imported, and the roots converge.
//!
//! - `BodyState`: the replica rewrites every `BodyState` row from its physics world right before it takes each root
//!   (pe-replica's `take_root`), so a damaged row never reaches a root and there is nothing to repair. That is
//!   asserted instead.
//! - The physics world (@item:primordial-engine/m1-follow-up-a-physics-world-divergence-is-repai): a body's velocity
//!   is damaged in the replica's own physics world (pe-replica's `test-util` hook `damage_body`), and both rungs cure
//!   it. Rung 2 fetches the body's object leaf and the honest physics image, which the import builds the new physics
//!   world from (never from fetched `BodyState` rows).
//!
//! Agreement is a stand-in (as in tests/common): every 8 frames a cut 2 frames back, final with the root a quorum of
//! the three members hold. The replicas, their trees, their replays and their imports are pe-replica's and pe-world's.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use pe_diverge::msg::Msg;
use pe_diverge::{Action, Config, Diverge};
use pe_replica::logic::{ScriptRow, ScriptRunner, StageOutcome, StageWorld};
use pe_replica::mock::MockClock;
use pe_replica::world::{BodyRow, BodyStateRow, ClaimRow, Effects, HandRow, LogicFx, PhysicsSettings, Unbuildable};
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl, WorldModel};
use pe_seams::{
    Activation, Cut, CutImport, CutRef, DomainId, EntrySource, FinalCut, Frame, FrameClock, Hash, HostTime,
    ImportError, LeafKey, LineId, MemberKey, ObjectId, Outcome, Pos, Replica, Signature, SignedEntry, SimRate, Stage,
    Tree, ViewObject,
};
use pe_sim::{SimStore, Transform};
use pe_world::components::{Body, Shape, body_kind, shape_kind};
use pe_world::test_util::{Kind, divergence, five_kinds};
use pe_world::{GENESIS, World};

const VICTIM: usize = 2;
const CUT_EVERY: u64 = 8;

/// A runner that holds every module and runs nothing: `five_kinds`' module is not real wasm, and these tests are about
/// hashed state, not scripts. Every member runs the same one, so the logic stages change nothing anywhere.
struct InertRunner;

impl ScriptRunner for InertRunner {
    fn identity(&self) -> Bytes {
        Bytes::from_static(b"pe-diverge kinds.rs inert runner")
    }
    fn holds(&self, _module: &Hash) -> bool {
        true
    }
    fn module(&self, _module: &Hash) -> Option<Bytes> {
        None
    }
    fn run_stage(
        &mut self,
        _stage: Stage,
        _scripts: &[ScriptRow],
        _store: &mut SimStore,
        _world: &StageWorld<'_>,
    ) -> StageOutcome {
        StageOutcome::default()
    }
}

/// `PeWorld`, damaged once in the live history: at the end of frame `at`, `divergence(kind)` damages the world. The plan
/// is shared and taken when it fires, so a replay of that frame (which restores a saved state and steps again) does not
/// damage it again: a transient fault.
struct Faulty {
    inner: PeWorld,
    plan: Arc<Mutex<Option<(Frame, Kind)>>>,
    /// When the plan fired: the leaf it damaged, and whether the world's own root moved.
    damaged: Arc<Mutex<Option<(LeafKey, bool)>>>,
}

impl WorldModel for Faulty {
    type Saved = World;

    fn domain(&self) -> DomainId {
        self.inner.domain()
    }
    fn phase(entry: &SignedEntry) -> pe_replica::Phase {
        <PeWorld as WorldModel>::phase(entry)
    }
    fn apply(&mut self, frame: Frame, entry: &SignedEntry) -> (Outcome, Effects) {
        self.inner.apply(frame, entry)
    }
    fn end_frame(&mut self, frame: Frame) -> Vec<Activation> {
        let acts = self.inner.end_frame(frame);
        let mut plan = self.plan.lock().unwrap();
        if let Some((at, kind)) = *plan
            && at == frame
        {
            *plan = None;
            let before = self.inner.world().root();
            let leaf = divergence(self.inner.world_mut(), kind);
            let moved = self.inner.world().root() != before;
            *self.damaged.lock().unwrap() = leaf.map(|l| (l, moved));
        }
        acts
    }
    fn scripted(&self) -> bool {
        self.inner.scripted()
    }
    fn logic_stage(&mut self, frame: Frame, stage: Stage, runner: &mut dyn ScriptRunner) -> LogicFx {
        self.inner.logic_stage(frame, stage, runner)
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
    fn settings(&self) -> PhysicsSettings {
        self.inner.settings()
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

/// No entries: the worlds step empty frames. Only the final cut is the source's.
#[derive(Default)]
struct Source {
    final_cut: Option<FinalCut>,
}

impl EntrySource for Source {
    fn final_cut(&self) -> Option<&FinalCut> {
        self.final_cut.as_ref()
    }
    fn entries_at(&self, _frame: Frame) -> Option<&[SignedEntry]> {
        Some(&[])
    }
    fn kept_from(&self) -> Frame {
        Frame(0)
    }
    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}
    fn take_changed_from(&mut self) -> Option<Frame> {
        None
    }
}

fn clock() -> MockClock {
    MockClock { rate: SimRate::DEFAULT, start: HostTime(0) }
}

fn key(i: usize) -> MemberKey {
    MemberKey(Hash::of("kinds.member", &[i as u8]).0)
}

struct Peer {
    key: MemberKey,
    d: Diverge,
    r: ReplicaImpl<Faulty>,
    src: Source,
    actions: Vec<Action>,
    /// Repair-lane messages this peer sent.
    sent: Vec<Msg>,
    damaged: Arc<Mutex<Option<(LeafKey, bool)>>>,
}

impl Peer {
    fn new(i: usize, world: World, plan: Option<(Frame, Kind)>) -> Peer {
        let rate = SimRate::DEFAULT;
        let config = ReplicaConfig::new(rate, Duration::from_millis(133), Arc::new(clock()));
        let damaged = Arc::new(Mutex::new(None));
        let w = Faulty {
            inner: PeWorld::new(world, DomainId(0)),
            plan: Arc::new(Mutex::new(plan)),
            damaged: damaged.clone(),
        };
        let r = ReplicaImpl::new(w, config).expect("a replica").with_runner(Box::new(InertRunner));
        Peer {
            key: key(i),
            d: Diverge::new(key(i), Config::default()),
            r,
            src: Source::default(),
            actions: Vec::new(),
            sent: Vec::new(),
            damaged,
        }
    }

    /// The leaves this peer asked for in record requests.
    fn fetched(&self) -> Vec<LeafKey> {
        self.sent
            .iter()
            .filter_map(|m| match m {
                Msg::RecordRequest { keys, .. } => Some(keys.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn bundles(&self) -> Vec<pe_diverge::FaultBundle> {
        self.actions
            .iter()
            .filter_map(|a| match a {
                Action::WriteBundle(b) => Some((**b).clone()),
                _ => None,
            })
            .collect()
    }
}

struct Net {
    peers: Vec<Peer>,
    frame: u64,
    height: u64,
}

impl Net {
    /// Three members on `fixture()`; the victim's world comes from `victim_world`, and its live history is
    /// damaged by `plan`.
    fn new(victim_world: World, plan: Option<(Frame, Kind)>) -> Net {
        let peers = (0..3)
            .map(|i| if i == VICTIM { Peer::new(i, victim_world.clone(), plan) } else { Peer::new(i, fixture(), None) })
            .collect();
        Net { peers, frame: 0, height: 0 }
    }

    fn now(&self) -> HostTime {
        clock().host_time_of(Frame(self.frame))
    }

    /// The agreement stand-in: final with the root a quorum holds (every test here has one).
    fn decide(&mut self, cut: Cut) {
        let now = self.now();
        let mut groups: BTreeMap<Hash, Vec<MemberKey>> = BTreeMap::new();
        for p in &mut self.peers {
            if let Ok(r) = p.r.root_at_cut(&cut, &mut p.src) {
                groups.entry(r).or_default().push(p.key);
            }
        }
        let q = pe_seams::quorum(self.peers.len());
        let (root, voters) = groups.iter().find(|(_, v)| v.len() >= q).expect("a quorum root");
        let fc = FinalCut { cut, root: *root, voters: voters.iter().map(|k| (*k, Signature([0; 64]))).collect() };
        for p in &mut self.peers {
            p.src.final_cut = Some(fc.clone());
            let f = p.r.finalize(&fc, &mut p.src);
            p.d.on_final(&fc, f, &mut p.r, &mut p.src, now, &clock());
        }
    }

    fn step(&mut self) {
        self.frame += 1;
        for p in &mut self.peers {
            p.r.advance(Frame(self.frame), &mut p.src);
        }
        if self.frame.is_multiple_of(CUT_EVERY) {
            self.height += 1;
            let at = CutRef { line: LineId::origin(), height: self.height, frame: Frame(self.frame - 2) };
            self.decide(Cut { at, heads: Vec::new() });
        }
        self.deliver();
    }

    fn run(&mut self, frames: u64) {
        for _ in 0..frames {
            self.step();
        }
    }

    fn deliver(&mut self) {
        let now = self.now();
        let index = |peers: &[Peer], k: &MemberKey| peers.iter().position(|p| p.key == *k).unwrap();
        for _ in 0..64 {
            let mut batch = Vec::new();
            for p in self.peers.iter_mut() {
                for o in p.d.tick(now, &clock(), &mut p.r, &mut p.src) {
                    if let Ok(m) = Msg::decode(&o.bytes) {
                        p.sent.push(m);
                    }
                    batch.push((p.key, o));
                }
                p.d.events();
                p.actions.extend(p.d.actions());
            }
            if batch.is_empty() {
                break;
            }
            for (from, o) in batch {
                let ti = index(&self.peers, &o.to);
                let p = &mut self.peers[ti];
                let _ = p.d.receive(from, &o.bytes, &mut p.r, &mut p.src, now, &clock());
            }
        }
        for p in &mut self.peers {
            p.d.events();
            p.actions.extend(p.d.actions());
        }
    }

    fn roots(&self) -> Vec<Hash> {
        self.peers.iter().map(|p| p.r.world().inner.world().root()).collect()
    }

    fn converged(&self) -> bool {
        let r = self.roots();
        r.iter().all(|x| *x == r[0])
    }
}

/// A falling ball, so the physics writes a `BodyState` row the live damage can find.
const BALL: u128 = 0xba11;

/// `five_kinds()` plus [`BALL`].
fn fixture() -> World {
    let mut w = five_kinds();
    let id = ObjectId::from_u128(BALL);
    w.create(id, None, MemberKey([1; 32]), GENESIS).expect("fresh id");
    w.insert(id, Transform::at(Pos::ORIGIN)).expect("live object");
    w.insert(id, Body { kind: body_kind::DYNAMIC, gravity_scale: 1.0, ..Body::default() }).expect("live object");
    w.insert(id, Shape { kind: shape_kind::SPHERE, density: 800.0, size: [0.25, 0.0, 0.0], ..Shape::default() })
        .expect("live object");
    w
}

/// The kinds a repair must cure: every kind but `BodyState` (module doc), each one the fixture holds something of
/// (checked, so a fixture change can't silently skip one).
fn kinds() -> Vec<Kind> {
    Kind::ALL
        .into_iter()
        .filter(|k| *k != Kind::BodyState)
        .inspect(|k| assert!(divergence(&mut fixture(), *k).is_some(), "the fixture has nothing of {k:?}"))
        .collect()
}

#[test]
fn each_kind_damaged_after_a_verified_cut_is_cured_by_rung_1() {
    for kind in kinds() {
        // Cuts at frames 6 and 14 verify; the damage lands at the end of frame 18, the cut at 22 finds it.
        let mut net = Net::new(fixture(), Some((Frame(18), kind)));
        net.run(16);
        assert!(net.converged(), "{kind:?}: diverged before the damage");
        assert_eq!(net.peers[VICTIM].d.verified_cut().map(|(c, _)| c.frame), Some(Frame(14)), "{kind:?}");
        net.run(3);
        let damaged = *net.peers[VICTIM].damaged.lock().unwrap();
        assert!(damaged.is_some_and(|(_, moved)| moved), "{kind:?}: no damage, or it didn't move the world's root");
        assert!(!net.converged(), "{kind:?}: the damage didn't move the victim's root");
        net.run(13);
        let v = &net.peers[VICTIM];
        let c = v.d.counters().clone();
        assert_eq!(c.diverged, 1, "{kind:?}: {c:?}");
        assert_eq!(c.cured_by_rung[..2], [1, 0], "{kind:?}: {c:?}");
        assert!(v.fetched().is_empty(), "{kind:?}: rung 1 fetched {:?}", v.fetched());
        assert!(v.bundles().is_empty(), "{kind:?}: a rung-1 cure writes no bundle");
        assert!(net.converged(), "{kind:?}: not converged after rung 1");
        assert!(!v.d.is_repairing(), "{kind:?}");
    }
}

#[test]
fn each_kind_damaged_from_the_start_is_cured_by_rung_2_fetching_only_its_leaf() {
    for kind in kinds() {
        let mut w = fixture();
        let leaf = divergence(&mut w, kind).expect("checked by kinds()");
        let mut net = Net::new(w, None);
        assert!(!net.converged(), "{kind:?}: the damage didn't move the victim's root");
        net.run(40);
        let v = &net.peers[VICTIM];
        let c = v.d.counters().clone();
        assert_eq!(c.diverged, 1, "{kind:?}: {c:?}");
        assert_eq!(c.cured_by_rung[..2], [0, 1], "{kind:?}: {c:?}");
        assert_eq!(v.fetched(), vec![leaf], "{kind:?}: rung 2 must fetch exactly the damaged leaf");
        let b = v.bundles().pop().unwrap_or_else(|| panic!("{kind:?}: a rung-2 cure writes a bundle"));
        assert_eq!(b.differing, vec![leaf], "{kind:?}");
        assert!(net.converged(), "{kind:?}: not converged after rung 2");
        assert!(!v.d.is_repairing(), "{kind:?}");
        for (i, p) in net.peers.iter().enumerate() {
            if i != VICTIM {
                assert_eq!(p.d.counters().diverged, 0, "{kind:?}: honest member {i} diverged");
            }
        }
    }
}

#[test]
fn a_damaged_body_state_row_never_reaches_a_root() {
    // Live, after a verified cut: the physics' own row replaces it before the cut at 22 takes its root.
    let mut net = Net::new(fixture(), Some((Frame(18), Kind::BodyState)));
    net.run(32);
    let damaged = *net.peers[VICTIM].damaged.lock().unwrap();
    // The damage was real: it moved the world's own root, and only the replica's rewrite keeps it out of the cuts'.
    assert_eq!(damaged, Some((LeafKey::Object(ObjectId::from_u128(BALL)), true)), "the live damage hit the ball's row");
    // From the start: the replica's first root (frame 0) is taken after the same rewrite.
    let mut w = fixture();
    let before = w.root();
    assert!(divergence(&mut w, Kind::BodyState).is_some());
    assert_ne!(w.root(), before, "the damage from the start moves the world's own root");
    let mut start = Net::new(w, None);
    assert!(start.converged(), "the first root already has the physics' rows");
    start.run(32);
    for net in [&net, &start] {
        assert!(net.converged());
        for p in &net.peers {
            let c = p.d.counters();
            assert_eq!((c.diverged, c.cuts_compared, c.cuts_matched), (0, 4, 4), "{c:?}");
        }
    }
}

/// The ball's velocity off by 1 cm/s along x in the victim's physics world.
const DV: [f32; 3] = [0.01, 0.0, 0.0];

#[test]
fn a_physics_world_divergence_after_a_verified_cut_is_cured_by_rung_1() {
    let ball = ObjectId::from_u128(BALL);
    let mut net = Net::new(fixture(), None);
    net.run(18);
    assert!(net.converged());
    assert_eq!(net.peers[VICTIM].d.verified_cut().map(|(c, _)| c.frame), Some(Frame(14)));
    // Live, at frame 18: the next cut (22) takes its root after the physics writes the ball's rows.
    assert!(net.peers[VICTIM].r.damage_body(ball, DV), "the ball has a moving body");
    net.run(14);
    let v = &net.peers[VICTIM];
    let c = v.d.counters().clone();
    assert_eq!(c.diverged, 1, "the physics damage reached a root: {c:?}");
    assert_eq!(c.cured_by_rung[..2], [1, 0], "{c:?}");
    assert!(v.fetched().is_empty(), "rung 1 fetched {:?}", v.fetched());
    assert!(!v.sent.iter().any(|m| matches!(m, Msg::PhysicsRequest { .. })), "rung 1 asked for a physics image");
    assert!(v.bundles().is_empty());
    assert!(net.converged(), "not converged after rung 1");
    assert!(!v.d.is_repairing());
}

#[test]
fn a_physics_world_divergence_from_the_start_is_cured_by_rung_2_with_the_honest_image() {
    let ball = ObjectId::from_u128(BALL);
    let mut net = Net::new(fixture(), None);
    // From the start: the damage is in the victim's saved state at frame 0 too, so a replay reproduces it.
    let r = &mut net.peers[VICTIM].r;
    assert!(r.damage_body(ball, DV), "the ball has a moving body");
    r.rekeyframe();
    net.run(40);
    let v = &net.peers[VICTIM];
    let c = v.d.counters().clone();
    assert_eq!(c.diverged, 1, "{c:?}");
    assert_eq!(c.cured_by_rung[..2], [0, 1], "{c:?}");
    // The walk names the ball's leaf, and only it is fetched; the import needs the honest physics image with it.
    assert_eq!(v.fetched(), vec![LeafKey::Object(ball)]);
    assert!(v.sent.iter().any(|m| matches!(m, Msg::PhysicsRequest { .. })), "no physics image asked for");
    let b = v.bundles().pop().expect("a rung-2 cure writes a bundle");
    assert!(b.differing.contains(&LeafKey::Object(ball)), "{:?}", b.differing);
    assert!(net.converged(), "not converged after rung 2");
    assert!(!v.d.is_repairing());
    // It stays cured: the next cuts match (the physics world itself was replaced, not just the rows).
    let matched = c.cuts_matched;
    let mut net = net;
    net.run(24);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!((c.diverged, c.cuts_matched), (1, matched + 3), "{c:?}");
    for (i, p) in net.peers.iter().enumerate() {
        if i != VICTIM {
            assert_eq!(p.d.counters().diverged, 0, "honest member {i} diverged");
        }
    }
}
