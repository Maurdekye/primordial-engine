//! The logic stages through an injected runner (crate::logic): a fake runner without wasm, so these test the replica's
//! side only. pe-logic's runner has its own tests behind the `logic` feature.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::logic::{ScriptEvent, ScriptRow, StageOutcome, StageWorld};
use pe_replica::mock::MockClock;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl, ScriptRunner, event_kind};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    ChainHead, Class, Cut, CutRef, DomainId, EffectCause, EffectKey, EffectKind, EntrySource, FinalCut, Frame,
    FrameView, Hash, HostTime, LineId, MemberKey, ObjectId, Offset, Outcome, Pos, Replica, RootError, SignedEntry,
    Stage,
};
use pe_sim::{Component, ComponentId, SimStore, Transform};
use pe_world::components::{Body, Shape, body_kind, shape_kind};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::script::tag_desc;
use pe_world::{BlobRef, BlobStore, Claim, GENESIS, World, perm};

const ASSET: [u8; 32] = [7; 32];
const MODULE: Hash = Hash([3; 32]);
const BALL: u128 = 60;
const K: u64 = 8;
/// The fake's fault code for a refused write: any code from 1 to 13 is world state.
const REFUSED: u8 = 7;

fn oid(n: u128) -> ObjectId {
    ObjectId::from_u128(n)
}

fn pos(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).expect("in range")
}

fn tx(ops: Vec<Op>) -> Transaction {
    Transaction { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
}

fn keys() -> TestKeys {
    TestKeys::new(5, 2)
}

/// The version hash V of the mover's one version, which its tag's id derives from.
fn v1() -> Hash {
    Hash([1; 32])
}

fn tag() -> ComponentId {
    tag_desc(&v1()).id()
}

/// The mover's code commit, stamped `at` and activating there.
fn commit(b: &mut EntryBuilder, at: u64) -> SignedEntry {
    let t = tx(vec![
        Op::Register { components: vec![tag_desc(&v1())] },
        Op::CommitScript {
            script: ASSET,
            base: None,
            source: BlobRef { hash: v1().0, len: 10 },
            toolchain: Hash([1; 32]),
            sdk: Hash([2; 32]),
            author_module: MODULE,
            activate_at: at,
        },
    ]);
    b.next(Class::Edit, Frame(at), Bytes::from(t.encode()))
}

fn edit(b: &mut EntryBuilder, at: u64, ops: Vec<Op>) -> SignedEntry {
    b.next(Class::Edit, Frame(at), Bytes::from(tx(ops).encode()))
}

fn applied(o: &Outcome) {
    assert!(matches!(o, Outcome::Applied { .. }), "refused: {o:?}");
}

/// A static floor (1) and a ball 1 m above it (`BALL`), with an editor (key 0).
fn floor_and_ball() -> World {
    let mut w = World::new(1, BlobStore::default());
    w.set_member(keys().key(0), perm::EDIT);
    w.create(oid(1), None, MemberKey([1; 32]), GENESIS).expect("floor");
    w.insert(oid(1), Transform::at(pos(0.0, -0.5, 0.0))).expect("floor transform");
    w.insert(oid(1), Body { friction: 0.6, ..Body::default() }).expect("floor body");
    let floor = Shape { kind: shape_kind::BOX, density: 500.0, size: [10.0, 0.5, 10.0], ..Shape::default() };
    w.insert(oid(1), floor).expect("floor shape");
    w.create(oid(BALL), None, MemberKey([1; 32]), GENESIS).expect("ball");
    w.insert(oid(BALL), Transform::at(pos(0.0, 1.0, 0.0))).expect("ball transform");
    let body = Body { kind: body_kind::DYNAMIC, friction: 0.6, gravity_scale: 1.0, may_sleep: 1, ..Body::default() };
    w.insert(oid(BALL), body).expect("ball body");
    let ball = Shape { kind: shape_kind::SPHERE, density: 800.0, size: [0.1, 0.0, 0.0], ..Shape::default() };
    w.insert(oid(BALL), ball).expect("ball shape");
    w
}

/// The floor and ball, the mover committed at frame 0 and its tag on the ball: a world with a script from the start.
fn scripted() -> World {
    let mut w = floor_and_ball();
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    applied(&w.apply_edit(Frame(0), &commit(&mut b, 0)).0);
    let tag_row = Row { component: tag(), instance: 0, bytes: vec![] };
    applied(
        &w.apply_edit(Frame(0), &edit(&mut b, 0, vec![Op::SetRow { id: Target::Existing(oid(BALL)), row: tag_row }])).0,
    );
    assert_eq!(w.modules().len(), 1);
    w
}

/// A script runner without wasm: each script whose module it holds moves every object carrying the script's tag 1 cm
/// along x before the physics (a `Transform` write, which the world's check must accept for the whole call, or the
/// call faults and writes nothing), and emits a `ping` from each after it, with the frame's number. Stateless.
struct Mover {
    holds: Hash,
}

impl ScriptRunner for Mover {
    fn identity(&self) -> Bytes {
        Bytes::from_static(b"the test mover, version 1")
    }

    fn holds(&self, module: &Hash) -> bool {
        *module == self.holds
    }

    fn module(&self, module: &Hash) -> Option<Bytes> {
        self.holds(module).then(|| Bytes::from_static(b"mover"))
    }

    fn run_stage(
        &mut self,
        stage: Stage,
        scripts: &[ScriptRow],
        store: &mut SimStore,
        w: &StageWorld<'_>,
    ) -> StageOutcome {
        let mut out = StageOutcome::default();
        for s in scripts.iter().filter(|s| !w.faulted.contains(&s.asset)) {
            if s.module != self.holds {
                out.missing.push(s.asset);
                continue;
            }
            let tagged: Vec<ObjectId> = store.column_raw(tag_desc(&s.version).id()).0.to_vec();
            if stage == Stage::LogicPostPhysics {
                for &object in &tagged {
                    let bytes = w.frame.0.to_le_bytes().to_vec();
                    out.events.push(ScriptEvent {
                        asset: s.asset,
                        system: "pinger".into(),
                        name: "ping".into(),
                        object,
                        bytes,
                    });
                }
                continue;
            }
            let mut moved = Vec::new();
            for &id in &tagged {
                let Some(mut t) = store.get::<Transform>(id).copied() else { continue };
                t.pos[0] += 0.01;
                moved.push((id, t));
            }
            if moved.iter().any(|(id, t)| (w.check)(ComponentId::TRANSFORM, *id, bytemuck::bytes_of(t)).is_err()) {
                out.report.faults.push((s.asset, REFUSED));
                continue;
            }
            for (id, t) in moved {
                store.insert(id, t).expect("a transform");
                out.report.written.push((ComponentId::TRANSFORM, id));
            }
        }
        out
    }
}

fn mover() -> Box<dyn ScriptRunner> {
    Box::new(Mover { holds: MODULE })
}

#[derive(Default)]
struct Source {
    entries: BTreeMap<Frame, Vec<SignedEntry>>,
    changed: Option<Frame>,
}

impl Source {
    fn add(&mut self, e: SignedEntry) {
        let f = e.entry.stamp;
        let at = self.entries.entry(f).or_default();
        at.push(e);
        at.sort_by_key(|e| (e.entry.chain, e.entry.seq));
        self.changed = Some(self.changed.map_or(f, |c| c.min(f)));
    }
}

impl EntrySource for Source {
    fn final_cut(&self) -> Option<&FinalCut> {
        None
    }
    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        Some(self.entries.get(&frame).map_or(&[][..], Vec::as_slice))
    }
    fn kept_from(&self) -> Frame {
        Frame(0)
    }
    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}
    fn take_changed_from(&mut self) -> Option<Frame> {
        self.changed.take()
    }
}

const CLOCK: MockClock = MockClock { rate: pe_seams::SimRate::DEFAULT, start: HostTime(1_234_567_891) };

fn replica_of(world: World) -> ReplicaImpl<PeWorld> {
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(CLOCK));
    config.cut_every = K;
    ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).expect("a replica")
}

fn roots(r: &ReplicaImpl<PeWorld>, to: u64) -> Vec<(u64, Hash)> {
    (0..=to).step_by(K as usize).filter_map(|f| r.live_root(Frame(f)).map(|h| (f, h))).collect()
}

fn ball_x(r: &ReplicaImpl<PeWorld>) -> f64 {
    let at = r.world().world().store().world_transform(oid(BALL)).expect("the ball").pos;
    (at - Pos::ORIGIN).0[0]
}

fn ping(frame: u64) -> EffectKey {
    let kind = EffectKind::Script(event_kind(&ASSET, "ping"));
    EffectKey { frame: Frame(frame), cause: EffectCause::Logic, object: oid(BALL), kind, ordinal: 0 }
}

fn keys_of(view: &FrameView) -> Vec<EffectKey> {
    view.effects.iter().map(|e| e.key).collect()
}

/// A script runs in both logic stages: what it writes before the physics reaches the body (the ball moves 1 cm along x
/// each frame while it falls), and what it emits after reaches the view once, as S5 script effects.
#[test]
fn scripts_run_around_the_physics_and_their_events_become_effects() {
    let mut s = Source::default();
    let mut r = replica_of(scripted()).with_runner(mover());
    r.advance(Frame(30), &mut s);
    assert_eq!((r.frame(), r.faults()), (Frame(30), 0));
    assert!((ball_x(&r) - 0.30).abs() < 1e-4, "moved 30 times: {}", ball_x(&r));
    let body = r.physics().pose(oid(BALL)).expect("the ball's body").position;
    assert!((body[0] - 0.30).abs() < 1e-4, "the body follows the script's moves: {body:?}");
    let view = r.view();
    assert_eq!(keys_of(&view), (0..30).map(ping).collect::<Vec<_>>());
    for (f, e) in view.effects.iter().enumerate() {
        assert_eq!(e.data, Bytes::from((f as u64).to_le_bytes().to_vec()));
        assert!(!e.own && !e.resimulated && e.trigger_arrived.is_none());
    }
    assert!(view.retracted.is_empty());
    assert_eq!(r.runner_identity(), Some(Bytes::from_static(b"the test mover, version 1")));
    assert_eq!(r.script_counts().missing, 0);
}

/// A late entry rolls the replica back across scripted frames: the replay reaches the roots the in-order run reached,
/// and a view never carries a script effect twice.
#[test]
fn a_rollback_replays_the_scripts_to_the_same_roots_and_carries_each_effect_once() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let drop_at = |x: f64| {
        let body = Body { kind: body_kind::DYNAMIC, friction: 0.6, gravity_scale: 1.0, ..Body::default() };
        let shape = Shape { kind: shape_kind::SPHERE, density: 800.0, size: [0.1, 0.0, 0.0], ..Shape::default() };
        let rows = vec![
            Row {
                component: Transform::ID,
                instance: 0,
                bytes: bytemuck::bytes_of(&Transform::at(pos(x, 1.0, 0.0))).to_vec(),
            },
            Row { component: Body::ID, instance: 0, bytes: bytemuck::bytes_of(&body).to_vec() },
            Row { component: Shape::ID, instance: 0, bytes: bytemuck::bytes_of(&shape).to_vec() },
        ];
        vec![Op::Create { new: 0, parent: Target::Root, index: None, rows }]
    };
    let late = edit(&mut b, 12, drop_at(0.05));

    let mut all = Source::default();
    all.add(late.clone());
    let mut a = replica_of(scripted()).with_runner(mover());
    a.advance(Frame(40), &mut all);
    assert_eq!(a.faults(), 0);

    let mut s = Source::default();
    let mut r = replica_of(scripted()).with_runner(mover());
    r.advance(Frame(30), &mut s);
    let first = keys_of(&r.view());
    s.add(late);
    let adv = r.advance(Frame(40), &mut s);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 12), "rolled back: {adv:?}");
    assert_eq!(roots(&r, 40), roots(&a, 40));
    assert_eq!(r.faults(), 0);
    let second = keys_of(&r.view());
    assert_eq!(first, (0..30).map(ping).collect::<Vec<_>>());
    assert_eq!(second, (30..40).map(ping).collect::<Vec<_>>(), "the replayed frames' pings were carried already");
    assert!(r.view().retracted.is_empty());
}

/// A replay that no longer emits an event retracts it: a late delete of the ball at frame 20 takes back the pings the
/// view carried for frames 20 to 29.
#[test]
fn a_replay_that_drops_an_event_retracts_it() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let delete = edit(&mut b, 20, vec![Op::Delete { id: Target::Existing(oid(BALL)) }]);
    let mut s = Source::default();
    let mut r = replica_of(scripted()).with_runner(mover());
    r.advance(Frame(30), &mut s);
    s.add(delete);
    r.advance(Frame(40), &mut s);
    let view = r.view();
    assert!(view.effects.is_empty(), "nothing left to ping: {:?}", keys_of(&view));
    let retracted: BTreeSet<EffectKey> = view.retracted.iter().copied().collect();
    assert_eq!(retracted, (20..30).map(ping).collect::<BTreeSet<_>>());
    assert_eq!(r.faults(), 0);
}

/// Without a runner, a replica refuses to step a world with scripts: it stays at its frame, with one fault for the
/// stop, however often it is asked. It never steps the world as one without scripts.
#[test]
fn without_a_runner_a_world_with_scripts_is_not_stepped() {
    let mut s = Source::default();
    let mut r = replica_of(scripted());
    let root = r.live_root(Frame(0));
    r.advance(Frame(10), &mut s);
    r.advance(Frame(20), &mut s);
    assert_eq!((r.frame(), r.faults()), (Frame(0), 1));
    assert_eq!(r.live_root(Frame(0)), root);
    assert!((ball_x(&r)).abs() < 1e-9, "nothing ran");
    assert_eq!(r.runner_identity(), None);
}

/// Without a runner, a replica steps a world without scripts up to the frame of a code commit and stops there. With
/// one, the script runs from the commit's frame.
#[test]
fn without_a_runner_the_replica_stops_at_a_code_commit() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    s.add(commit(&mut b, 5));
    let tag_row = Row { component: tag(), instance: 0, bytes: vec![] };
    s.add(edit(&mut b, 5, vec![Op::SetRow { id: Target::Existing(oid(BALL)), row: tag_row }]));
    let mut bare = replica_of(floor_and_ball());
    bare.advance(Frame(10), &mut s);
    assert_eq!((bare.frame(), bare.faults()), (Frame(5), 1));

    let mut s2 = Source::default();
    let mut b2 = EntryBuilder::new(&keys, 0);
    s2.add(commit(&mut b2, 5));
    let tag_row = Row { component: tag(), instance: 0, bytes: vec![] };
    s2.add(edit(&mut b2, 5, vec![Op::SetRow { id: Target::Existing(oid(BALL)), row: tag_row }]));
    let mut r = replica_of(floor_and_ball()).with_runner(mover());
    r.advance(Frame(10), &mut s2);
    assert_eq!((r.frame(), r.faults()), (Frame(10), 0));
    assert!((ball_x(&r) - 0.05).abs() < 1e-4, "moved from frame 5: {}", ball_x(&r));
    assert_eq!(keys_of(&r.view()), (5..10).map(ping).collect::<Vec<_>>());
}

/// A script whose module this machine lacks does not run here, and each skipped run is counted: a build fault on
/// this machine, never world state.
#[test]
fn a_script_whose_module_is_missing_does_not_run_and_is_counted() {
    let mut s = Source::default();
    let mut r = replica_of(scripted()).with_runner(Box::new(Mover { holds: Hash([4; 32]) }));
    r.advance(Frame(10), &mut s);
    assert_eq!((r.frame(), r.faults()), (Frame(10), 0));
    assert!(ball_x(&r).abs() < 1e-9, "never moved");
    assert!(r.view().effects.is_empty());
    assert_eq!(r.script_counts().missing, 20, "two stages a frame for ten frames");
}

/// A script's fault is world state: the world refuses a held object's pose from a script, the call faults, the
/// modules row records the frame and code, and the script runs no more. A replay from the start agrees.
#[test]
fn a_script_fault_is_world_state_and_stops_the_script() {
    let mut world = scripted();
    let agent = keys().key(1);
    let claim = Claim { hand: ObjectId::NONE, member: agent, since: Frame(0), until: Frame(1000) };
    world.set_claim(oid(BALL), claim).expect("held by an agent");
    let mut s = Source::default();
    let mut r = replica_of(world.clone()).with_runner(mover());
    r.advance(Frame(16), &mut s);
    assert_eq!(r.faults(), 0);
    let row = &r.world().world().modules()[&ASSET];
    assert_eq!(row.faulted, Some((Frame(0), REFUSED)));
    assert!(ball_x(&r).abs() < 1e-9, "the held ball was never moved");
    assert!(r.view().effects.is_empty(), "faulted before its first ping");

    let mut again = replica_of(world).with_runner(mover());
    again.advance(Frame(16), &mut Source::default());
    assert_eq!(roots(&again, 16), roots(&r, 16));
}

/// A replay that adds an event carries it, marked re-simulated and with no trigger time, so the view never plays it
/// (D6). The frames past the old present carry theirs as first simulations, and the ball's replayed pings are not
/// carried again.
#[test]
fn a_replay_that_adds_an_event_carries_it_marked_resimulated() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let at = bytemuck::bytes_of(&Transform::at(pos(2.0, 0.5, 0.0))).to_vec();
    let rows = vec![
        Row { component: Transform::ID, instance: 0, bytes: at },
        Row { component: tag(), instance: 0, bytes: vec![] },
    ];
    let marker = edit(&mut b, 20, vec![Op::Create { new: 0, parent: Target::Root, index: None, rows }]);
    let mut s = Source::default();
    let mut r = replica_of(scripted()).with_runner(mover());
    r.advance(Frame(30), &mut s);
    s.add(marker);
    r.advance(Frame(40), &mut s);
    assert_eq!(r.faults(), 0);
    let view = r.view();
    let new: Vec<_> = view.effects.iter().filter(|e| e.key.object != oid(BALL)).collect();
    assert_eq!(new.iter().map(|e| e.key.frame.0).collect::<Vec<_>>(), (20..40).collect::<Vec<_>>());
    assert!(new.iter().all(|e| e.resimulated == (e.key.frame.0 < 30) && e.trigger_arrived.is_none()), "{new:?}");
    let ball: Vec<u64> = view.effects.iter().filter(|e| e.key.object == oid(BALL)).map(|e| e.key.frame.0).collect();
    assert_eq!(ball, (30..40).collect::<Vec<_>>());
    assert!(view.retracted.is_empty());
}

fn cut_at(entries: &[SignedEntry], height: u64, frame: u64) -> Cut {
    let mut heads: BTreeMap<_, ChainHead> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.entry.stamp.0 < frame) {
        let h = ChainHead { chain: e.entry.chain, author: e.entry.author, seq: e.entry.seq, entry: e.id() };
        if heads.get(&h.chain).is_none_or(|old| old.seq < h.seq) {
            heads.insert(h.chain, h);
        }
    }
    Cut { at: CutRef { line: LineId::origin(), height, frame: Frame(frame) }, heads: heads.into_values().collect() }
}

/// Without a runner, a cut's root that needs a code commit's frame re-stepped is not computed: the re-step stops at
/// the commit and answers `NotYetSimulated`, and the live state and counts stay as they were (the scratch counts the
/// fault).
#[test]
fn without_a_runner_a_cut_root_across_a_code_commit_is_not_computed() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let late = commit(&mut b, 5);
    let mut s = Source::default();
    let mut r = replica_of(floor_and_ball());
    r.advance(Frame(20), &mut s);
    assert_eq!((r.frame(), r.faults()), (Frame(20), 0));
    s.add(late.clone());
    let cut = cut_at(&[late], 1, 16);
    assert_eq!(r.root_at_cut(&cut, &mut s), Err(RootError::NotYetSimulated));
    assert_eq!((r.frame(), r.faults(), r.scratch().faults), (Frame(20), 0, 1));
}
