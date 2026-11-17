//! The logic stages through an injected runner (crate::logic): a fake runner without wasm, so these test the replica's
//! side only. pe-logic's runner has its own tests behind the `logic` feature.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::logic::{Fixture, ScriptEvent, ScriptRow, StageOutcome, StageWorld, conformance};
use pe_replica::mock::MockClock;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl, ScriptRunner, event_kind};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    ChainHead, Class, Cut, CutImport, CutRef, DomainId, EffectCause, EffectKey, EffectKind, EntrySource, FinalCut,
    Frame, FrameView, GroupSel, Hash, HostTime, LeafKey, LineId, MemberKey, ObjectId, Offset, Outcome, Pos, Replica,
    RootError, RunnerIdentity, SignedEntry, Stage,
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

/// The mover's source archive (logic.md 4.1a): a commit carries it, and checks it, in the entry.
fn archive() -> Bytes {
    pe_world::script::pack_source(&[("src/lib.rs", b"// the mover\n".as_slice())]).expect("an archive").into()
}

/// The version hash V of the mover's one version, which its tag's id derives from: its archive's hash.
fn v1() -> Hash {
    Hash(BlobRef::of(&archive()).hash)
}

fn tag() -> ComponentId {
    tag_desc(&v1()).id()
}

/// The mover's code commit, stamped `at`: pending until a quorum attests it (logic.md 6.2a), and with one member its
/// author is that quorum, so it activates at the end of frame `at` and the mover runs from the next frame.
fn commit(b: &mut EntryBuilder, at: u64) -> SignedEntry {
    commit_activating(b, at, at)
}

/// The mover's code commit, stamped `at`, with `activate_at` (pe-world refuses one that isn't the stamp).
fn commit_activating(b: &mut EntryBuilder, at: u64, activate_at: u64) -> SignedEntry {
    let t = tx(vec![
        Op::Register { components: vec![tag_desc(&v1())] },
        Op::commit_script(ASSET, None, archive(), Hash([1; 32]), Hash([2; 32]), MODULE, activate_at),
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
/// The commit activates at the end of frame 0 (6.2a), so the mover runs from frame 1.
fn scripted() -> World {
    let mut w = floor_and_ball();
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    applied(&w.apply_edit(Frame(0), &commit(&mut b, 0)).0);
    assert_eq!(w.end_frame(Frame(0)).activated.len(), 1, "the one member is its quorum");
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
/// each frame while it falls), and what it emits after reaches the view once, as S5 script effects. It runs from frame
/// 1: its commit activated at the end of frame 0 (logic.md 6.2a).
#[test]
fn scripts_run_around_the_physics_and_their_events_become_effects() {
    let mut s = Source::default();
    let mut r = replica_of(scripted()).with_runner(mover());
    r.advance(Frame(30), &mut s);
    assert_eq!((r.frame(), r.faults()), (Frame(30), 0));
    assert!((ball_x(&r) - 0.29).abs() < 1e-4, "moved 29 times, in frames 1 to 29: {}", ball_x(&r));
    let body = r.physics().pose(oid(BALL)).expect("the ball's body").position;
    assert!((body[0] - 0.29).abs() < 1e-4, "the body follows the script's moves: {body:?}");
    let view = r.view();
    assert_eq!(keys_of(&view), (1..30).map(ping).collect::<Vec<_>>());
    for (e, f) in view.effects.iter().zip(1u64..) {
        assert_eq!(e.data, Bytes::from(f.to_le_bytes().to_vec()));
        assert!(!e.own && !e.resimulated && e.trigger_arrived.is_none());
    }
    assert!(view.retracted.is_empty());
    assert_eq!(r.runner_identity(), Some(RunnerIdentity(Bytes::from_static(b"the test mover, version 1"))));
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
    assert_eq!(first, (1..30).map(ping).collect::<Vec<_>>());
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

/// The runner's identity and its modules reach S2's provided methods (v1.7) through `dyn Replica`, as the hello and a
/// fault bundle ask for them, not only through the concrete type (@engine-architect's Low 4 on 4ac9019f). Without a
/// runner, both are `None`.
#[test]
fn the_runner_s_identity_and_modules_reach_s2_through_dyn_replica() {
    let with: Box<dyn Replica> = Box::new(replica_of(scripted()).with_runner(mover()));
    assert_eq!(with.runner_identity(), Some(RunnerIdentity(Bytes::from_static(b"the test mover, version 1"))));
    assert_eq!(with.module(&MODULE), Some(Bytes::from_static(b"mover")));
    assert_eq!(with.module(&Hash([9; 32])), None, "a module it doesn't hold");
    let without: Box<dyn Replica> = Box::new(replica_of(scripted()));
    assert_eq!((without.runner_identity(), without.module(&MODULE)), (None, None));
}

/// Without a runner, a replica steps a world without scripts through the frame of a code commit, which only makes it
/// pending, and stops at the next frame: with one member the commit activates at the end of its frame, and its script
/// would run from the next (logic.md 6.2a). With a runner, the script runs from there, once its tag is on the ball.
#[test]
fn without_a_runner_the_replica_stops_once_a_code_commit_activates() {
    let keys = keys();
    let entries = || {
        let mut b = EntryBuilder::new(&keys, 0);
        let mut s = Source::default();
        s.add(commit(&mut b, 5));
        let tag_row = Row { component: tag(), instance: 0, bytes: vec![] };
        s.add(edit(&mut b, 6, vec![Op::SetRow { id: Target::Existing(oid(BALL)), row: tag_row }]));
        s
    };
    let mut s = entries();
    let mut bare = replica_of(floor_and_ball());
    bare.advance(Frame(10), &mut s);
    assert_eq!((bare.frame(), bare.faults()), (Frame(6), 1));
    assert_eq!(bare.world().world().modules()[&ASSET].activate_at, Frame(6));

    let mut s = entries();
    let mut r = replica_of(floor_and_ball()).with_runner(mover());
    r.advance(Frame(10), &mut s);
    assert_eq!((r.frame(), r.faults()), (Frame(10), 0));
    assert!((ball_x(&r) - 0.04).abs() < 1e-4, "moved in frames 6 to 9: {}", ball_x(&r));
    assert_eq!(keys_of(&r.view()), (6..10).map(ping).collect::<Vec<_>>());
}

/// Without a runner, a replica steps through a pending code commit: only an active modules row needs a runner (logic.md
/// 6.2a). With two members the author's own attestation is not a quorum, so the commit at 5 stays pending until the
/// second member's attestation at 12 activates it at that frame's end, and the replica stops at 13, the first frame
/// its script would run in. A commit pe-world refuses, here one activating after its stamp, never stops it.
#[test]
fn without_a_runner_a_pending_commit_does_not_stop_the_replica_until_it_activates() {
    let keys = keys();
    let (mut author, mut voter) = (EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 1));
    let mut world = floor_and_ball();
    world.set_member(keys.key(1), perm::EDIT);
    let mut s = Source::default();
    s.add(commit_activating(&mut author, 3, 9));
    s.add(commit(&mut author, 5));
    s.add(edit(&mut voter, 12, vec![Op::AttestBuild { script: ASSET, version: v1(), module: MODULE }]));
    let mut bare = replica_of(world);
    bare.advance(Frame(20), &mut s);
    assert_eq!((bare.frame(), bare.faults()), (Frame(13), 1));
    assert_eq!(bare.world().world().modules()[&ASSET].activate_at, Frame(13));
}

/// A script's move of an object without a body reaches the physics of the bodies beneath it (@engine-architect's
/// Medium on 4ac9019f): the mover carries a parent with no body, and the static platform beneath it goes along.
#[test]
fn a_script_moving_a_parent_without_a_body_carries_the_bodies_beneath_it() {
    const PARENT: u128 = 90;
    const PLATFORM: u128 = 91;
    let mut w = floor_and_ball();
    w.create(oid(PARENT), None, MemberKey([1; 32]), GENESIS).expect("a parent");
    w.insert(oid(PARENT), Transform::at(pos(0.0, 0.0, 0.0))).expect("its transform");
    w.create(oid(PLATFORM), Some(oid(PARENT)), MemberKey([1; 32]), GENESIS).expect("a platform");
    w.insert(oid(PLATFORM), Transform::at(pos(3.0, 2.0, 0.0))).expect("its transform");
    w.insert(oid(PLATFORM), Body { friction: 0.6, ..Body::default() }).expect("a static body");
    let slab = Shape { kind: shape_kind::BOX, density: 500.0, size: [1.0, 0.1, 1.0], ..Shape::default() };
    w.insert(oid(PLATFORM), slab).expect("its shape");
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    applied(&w.apply_edit(Frame(0), &commit(&mut b, 0)).0);
    assert_eq!(w.end_frame(Frame(0)).activated.len(), 1, "active from frame 1 (6.2a)");
    let tag_row = Row { component: tag(), instance: 0, bytes: vec![] };
    let tagged = edit(&mut b, 0, vec![Op::SetRow { id: Target::Existing(oid(PARENT)), row: tag_row }]);
    applied(&w.apply_edit(Frame(0), &tagged).0);
    let mut r = replica_of(w).with_runner(mover());
    r.advance(Frame(10), &mut Source::default());
    assert_eq!((r.frame(), r.faults()), (Frame(10), 0));
    let at = r.physics().pose(oid(PLATFORM)).expect("the platform's body").position;
    assert!((at[0] - 3.09).abs() < 1e-4, "the platform's body went with its parent, frames 1 to 9: {at:?}");
}

/// Each object carrying the script's tag emits `pop` after the physics and is removed in the same stage.
struct Popper;

impl ScriptRunner for Popper {
    fn identity(&self) -> Bytes {
        Bytes::from_static(b"the test popper, version 1")
    }

    fn holds(&self, module: &Hash) -> bool {
        *module == MODULE
    }

    fn module(&self, _: &Hash) -> Option<Bytes> {
        None
    }

    fn run_stage(
        &mut self,
        stage: Stage,
        scripts: &[ScriptRow],
        store: &mut SimStore,
        _: &StageWorld<'_>,
    ) -> StageOutcome {
        let mut out = StageOutcome::default();
        if stage != Stage::LogicPostPhysics {
            return out;
        }
        for s in scripts {
            for object in store.column_raw(tag_desc(&s.version).id()).0.to_vec() {
                let event =
                    ScriptEvent { asset: s.asset, system: "popper".into(), name: "pop".into(), object, bytes: vec![] };
                out.events.push(event);
                store.destroy(object).expect("removed");
                out.report.removed.push(object);
            }
        }
        out
    }
}

/// An event whose emitter the stage removed plays where the emitter was at the stage's start, not at the origin
/// (@engine-architect's Low 1 on 4ac9019f).
#[test]
fn an_event_from_an_object_the_stage_removed_plays_where_it_was() {
    let mut r = replica_of(scripted()).with_runner(Box::new(Popper));
    r.advance(Frame(2), &mut Source::default());
    assert_eq!((r.frame(), r.faults()), (Frame(2), 0), "the popper first runs at frame 1");
    assert!(!r.world().world().store().exists(oid(BALL)), "popped");
    let view = r.view();
    assert_eq!(view.effects.len(), 1);
    let at = (view.effects[0].at - Pos::ORIGIN).0;
    assert!(at[0].abs() < 1e-6 && (at[1] - 1.0).abs() < 0.01, "where the ball was: {at:?}");
}

/// `event_kind`'s known answer, so that a change to it or to the hash it reads shows (@engine-architect's test gap):
/// the first 8 bytes, little-endian, of `Hash::of("pe.script.event", asset ++ name)`.
#[test]
fn event_kind_has_its_known_answer() {
    assert_eq!(event_kind(&ASSET, "ping"), 9_909_364_129_443_047_667);
    let mut b = ASSET.to_vec();
    b.extend_from_slice(b"ping");
    let h = Hash::of("pe.script.event", &b);
    assert_eq!(event_kind(&ASSET, "ping"), u64::from_le_bytes(h.0[..8].try_into().unwrap()));
    assert_ne!(event_kind(&ASSET, "ping"), event_kind(&ASSET, "pong"));
    assert_ne!(event_kind(&ASSET, "ping"), event_kind(&[8; 32], "ping"));
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
    assert_eq!(r.script_counts().missing, 18, "two stages a frame, frames 1 to 9");
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
    assert_eq!(row.faulted, Some((Frame(1), REFUSED)), "its first frame");
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

/// Without a runner, a cut's root that needs a code commit's frames re-stepped is not computed: the re-step stops the
/// frame after the commit, where it activated (6.2a), and answers `NotYetSimulated`, and the live state and counts stay
/// as they were (the scratch counts the fault).
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

/// The scratch's stops and the live history's are counted apart (@engine-architect's Low 2 on 4ac9019f): after a cut's
/// root stopped at a late code commit off the live history, the live loop rolls back to the commit and stops after it
/// too, with its own fault. A cut above that stop then answers `NotYetSimulated`, since its root won't come, not
/// `Pending` (the review's nit).
#[test]
fn without_a_runner_the_scratch_and_the_live_history_count_their_stops_apart() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let late = commit(&mut b, 5);
    let mut s = Source::default();
    let mut r = replica_of(floor_and_ball());
    r.advance(Frame(20), &mut s);
    s.add(late.clone());
    let cut = cut_at(&[late], 1, 16);
    assert_eq!(r.root_at_cut(&cut, &mut s), Err(RootError::NotYetSimulated));
    assert_eq!((r.frame(), r.faults(), r.scratch().faults), (Frame(20), 0, 1));
    r.advance(Frame(30), &mut s);
    assert_eq!((r.frame(), r.faults(), r.scratch().faults), (Frame(6), 1, 1));
    r.advance(Frame(30), &mut s);
    assert_eq!(r.faults(), 1, "one fault per stop");
    assert_eq!(r.root_at_cut(&cut, &mut s), Err(RootError::NotYetSimulated));
}

/// A stop is over once the live history passes it, so a later stop at the same frame counts again (Low 2): the code
/// commit at frame 5 is withdrawn, the replica steps on past it, and a second commit there stops it again, each time
/// at 6, where the commit activated.
#[test]
fn without_a_runner_a_stop_passed_and_met_again_counts_again() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    s.add(commit(&mut b, 5));
    let mut r = replica_of(floor_and_ball());
    r.advance(Frame(10), &mut s);
    assert_eq!((r.frame(), r.faults()), (Frame(6), 1));
    s.entries.clear();
    s.changed = Some(Frame(5));
    r.advance(Frame(10), &mut s);
    assert_eq!((r.frame(), r.faults()), (Frame(10), 1));
    s.add(commit(&mut b, 5));
    r.advance(Frame(20), &mut s);
    assert_eq!((r.frame(), r.faults()), (Frame(6), 2));
}

/// The mover, but the first time it runs frame 20 it also emits `glitch` from the ball after the physics and moves it
/// 1 cm further: a transient fault of this machine, which rung 1 repairs. (It remembers that it glitched, which no
/// real runner may: [`conformance`].)
struct Glitching {
    inner: Mover,
    glitched: bool,
}

impl ScriptRunner for Glitching {
    fn identity(&self) -> Bytes {
        self.inner.identity()
    }

    fn holds(&self, module: &Hash) -> bool {
        self.inner.holds(module)
    }

    fn module(&self, module: &Hash) -> Option<Bytes> {
        self.inner.module(module)
    }

    fn run_stage(
        &mut self,
        stage: Stage,
        scripts: &[ScriptRow],
        store: &mut SimStore,
        w: &StageWorld<'_>,
    ) -> StageOutcome {
        let mut out = self.inner.run_stage(stage, scripts, store, w);
        if w.frame == Frame(20) && stage == Stage::LogicPostPhysics && !self.glitched {
            self.glitched = true;
            let glitch = ScriptEvent {
                asset: ASSET,
                system: "pinger".into(),
                name: "glitch".into(),
                object: oid(BALL),
                bytes: vec![],
            };
            out.events.push(glitch);
            let mut t = store.get::<Transform>(oid(BALL)).copied().expect("the ball");
            t.pos[0] += 0.01;
            store.insert(oid(BALL), t).expect("moved");
            out.report.written.push((ComponentId::TRANSFORM, oid(BALL)));
        }
        out
    }
}

/// Rung 1 replays from Cv, below the floor its divergence moved, and the views compare what it steps with what they
/// carried (@engine-architect's Low 3 on 4ac9019f): no ping is carried twice, and the glitch's event is retracted.
#[test]
fn rung_one_carries_each_effect_once_and_retracts_what_a_glitch_emitted() {
    let mut s = Source::default();
    let mut good = replica_of(scripted()).with_runner(mover());
    good.advance(Frame(50), &mut s);
    let mut bad =
        replica_of(scripted()).with_runner(Box::new(Glitching { inner: Mover { holds: MODULE }, glitched: false }));
    bad.advance(Frame(48), &mut s);
    let kind = EffectKind::Script(event_kind(&ASSET, "glitch"));
    let glitch = EffectKey { frame: Frame(20), cause: EffectCause::Logic, object: oid(BALL), kind, ordinal: 0 };
    let first = keys_of(&bad.view());
    assert!(first.contains(&glitch) && first.len() == 48, "47 pings, from frame 1, and the glitch: {first:?}");
    let (c16, c32) = (cut_at(&[], 1, 16), cut_at(&[], 2, 32));
    let (r16, r32) = (good.live_root(Frame(16)).expect("at 16"), good.live_root(Frame(32)).expect("at 32"));
    let agreed = bad.finalize(&FinalCut { cut: c16, root: r16, voters: vec![] }, &mut s);
    assert_eq!(agreed, pe_seams::Finalized::Agreed);
    let fc32 = FinalCut { cut: c32.clone(), root: r32, voters: vec![] };
    assert!(matches!(bad.finalize(&fc32, &mut s), pe_seams::Finalized::Diverged { .. }), "the glitch shows at 32");
    // A view published with the floor at 32 and Cv at 16, before the replay.
    bad.advance(Frame(49), &mut s);
    assert_eq!(keys_of(&bad.view()), [ping(48)]);
    assert_eq!(bad.replay_to(&c32, &mut s), Ok(r32));
    bad.advance(Frame(50), &mut s);
    let after = bad.view();
    assert_eq!(keys_of(&after), [ping(49)], "every replayed ping was carried already");
    assert_eq!(after.retracted, [glitch]);
    let from_32 = |r: &ReplicaImpl<PeWorld>| roots(r, 48).into_iter().filter(|x| x.0 >= 32).collect::<Vec<_>>();
    assert_eq!(from_32(&bad), from_32(&good));
}

/// A late entry the final cut leaves out was stepped live: as the cut goes final, `finalize` re-bases the live state on
/// the cut-exact floor (`rebase_on_floor`), and the next view compares the frames it re-based with what the views
/// carried there, as a replay's (@engine-architect's Low 3 on 4ac9019f). Here the late entry deleted the ball at 20, so
/// the live history had no ping from 20 on, and the cut-exact history has one each frame.
#[test]
fn a_floor_re_based_over_a_late_entry_carries_the_effects_of_the_frames_it_re_based() {
    let mut clean = replica_of(scripted()).with_runner(mover());
    clean.advance(Frame(48), &mut Source::default());
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let late = edit(&mut b, 20, vec![Op::Delete { id: Target::Existing(oid(BALL)) }]);
    let mut s = Source::default();
    s.add(late.clone());
    let mut a = replica_of(scripted()).with_runner(mover());
    a.advance(Frame(48), &mut s);
    applied(&a.outcome(&late.id()).expect("stepped").1);
    assert_eq!(keys_of(&a.view()), (1..20).map(ping).collect::<Vec<_>>(), "no ball from 20");
    // S3 voids the late entry as the cut at 24 goes final, and reports the change.
    s.entries.clear();
    s.changed = Some(Frame(20));
    let fc24 = FinalCut { cut: cut_at(&[], 3, 24), root: clean.live_root(Frame(24)).expect("at 24"), voters: vec![] };
    assert_eq!(a.finalize(&fc24, &mut s), pe_seams::Finalized::Agreed);
    a.advance(Frame(48), &mut s);
    let view = a.view();
    assert_eq!(keys_of(&view), (20..48).map(ping).collect::<Vec<_>>(), "the re-based frames' pings, then the rest");
    assert!(view.effects.iter().all(|e| e.resimulated), "every frame from 20 was stepped before");
    assert!(view.retracted.is_empty());
    let from_24 = |r: &ReplicaImpl<PeWorld>| roots(r, 48).into_iter().filter(|x| x.0 >= 24).collect::<Vec<_>>();
    assert_eq!(from_24(&a), from_24(&clean));
    assert_eq!(a.faults(), 0, "S3's report inside [20, 24) is excused");
}

/// The donor's whole state at `cut`, as a full-state fetch asks for it: every object, every table and the config
/// record, with their proofs, and the physics image (as tests/peworld.rs builds it).
fn full_import(donor: &mut ReplicaImpl<PeWorld>, s: &mut Source, cut: &Cut) -> CutImport {
    let root = donor.root_at_cut(cut, s).expect("the donor's root");
    let mut keys: Vec<LeafKey> =
        donor.world().world().store().objects().iter().map(|&id| LeafKey::Object(id)).collect();
    keys.extend((0..pe_world::tree::table::COUNT as u8).map(LeafKey::Table));
    keys.push(LeafKey::Config);
    let records = donor.tree(&root).expect("held").export(&keys);
    CutImport { cut: cut.clone(), target_root: root, records, absent: Vec::new(), physics: donor.physics_image(&root) }
}

/// A rung-2 import replaces my history below its cut: the next view retracts what the views carried there, the
/// glitch's event with the rest, and the replay from the cut to my present carries nothing twice (@engine-architect's
/// Low 3 on 4ac9019f).
#[test]
fn a_rung_two_import_retracts_what_the_views_carried_below_its_cut() {
    let mut s = Source::default();
    let mut good = replica_of(scripted()).with_runner(mover());
    good.advance(Frame(32), &mut s);
    let import = full_import(&mut good, &mut s, &cut_at(&[], 4, 32));
    good.advance(Frame(49), &mut s);
    let mut bad =
        replica_of(scripted()).with_runner(Box::new(Glitching { inner: Mover { holds: MODULE }, glitched: false }));
    bad.advance(Frame(48), &mut s);
    assert_eq!(keys_of(&bad.view()).len(), 48, "47 pings, from frame 1, and the glitch");
    assert_eq!(bad.import_at_cut(&GroupSel::Domain, import.clone(), &mut s), Ok(import.target_root));
    bad.advance(Frame(49), &mut s);
    let after = bad.view();
    assert_eq!(keys_of(&after), [ping(48)], "the replay from the cut carried nothing twice");
    let kind = EffectKind::Script(event_kind(&ASSET, "glitch"));
    let glitch = EffectKey { frame: Frame(20), cause: EffectCause::Logic, object: oid(BALL), kind, ordinal: 0 };
    let below: BTreeSet<EffectKey> = (1..32).map(ping).chain([glitch]).collect();
    assert_eq!(after.retracted.iter().copied().collect::<BTreeSet<_>>(), below);
    assert_eq!(after.retracted.len(), below.len(), "each once");
    let from_32 = |r: &ReplicaImpl<PeWorld>| roots(r, 48).into_iter().filter(|x| x.0 >= 32).collect::<Vec<_>>();
    assert_eq!(from_32(&bad), from_32(&good));
}

fn rows_of(w: &World) -> Vec<ScriptRow> {
    let row = |(&asset, m): (&[u8; 32], &pe_world::script::ModuleRow)| ScriptRow {
        asset,
        module: m.author_module,
        version: m.v(),
        components: m.components.clone(),
    };
    w.modules().iter().map(row).collect()
}

/// `w`'s store and scripts for the conformance check, and a script whose module no runner holds.
fn fixture_of(w: &World) -> Fixture {
    let mut scripts = rows_of(w);
    scripts.push(ScriptRow { asset: [8; 32], module: Hash([4; 32]), version: v1(), components: vec![tag()] });
    Fixture { store: w.store().clone(), scripts, line: LineId::origin(), rate_hz: 60, frames: 8 }
}

/// The test's own runner passes the check every runner passes (crate::logic::conformance).
#[test]
fn the_fake_runner_conforms() {
    conformance(&mover, &fixture_of(&scripted())).expect("the mover conforms");
}

/// A runner whose results depend on what it ran before fails the check: two fresh ones agree with each other, but a
/// replay from a saved store does not match the first run.
#[test]
fn a_runner_that_remembers_fails_the_conformance_check() {
    struct Remembering {
        inner: Mover,
        stages: u32,
    }
    impl ScriptRunner for Remembering {
        fn identity(&self) -> Bytes {
            self.inner.identity()
        }
        fn holds(&self, module: &Hash) -> bool {
            self.inner.holds(module)
        }
        fn module(&self, module: &Hash) -> Option<Bytes> {
            self.inner.module(module)
        }
        fn run_stage(
            &mut self,
            stage: Stage,
            scripts: &[ScriptRow],
            store: &mut SimStore,
            w: &StageWorld<'_>,
        ) -> StageOutcome {
            self.stages += 1;
            let mut out = self.inner.run_stage(stage, scripts, store, w);
            if self.stages > 12 {
                out.events.clear();
            }
            out
        }
    }
    let make = || Box::new(Remembering { inner: Mover { holds: MODULE }, stages: 0 }) as Box<dyn ScriptRunner>;
    let err = conformance(&make, &fixture_of(&scripted())).expect_err("it remembers");
    assert!(err.contains("replay"), "{err}");
}

/// pe-logic's runner (the `logic` feature) on the lamp of logic.md appendix A, which the build service builds.
#[cfg(feature = "logic")]
mod lamp {
    use std::path::Path;
    use std::sync::OnceLock;

    use pe_logic::run::{self, RunSettings};
    use pe_logic::runtime::{self, Script, Settings};
    use pe_replica::logic::LogicRunner;
    use pe_sim::ComponentDesc;
    use pe_world::script::script_component_id;

    use super::*;

    const LAMP: [u8; 32] = [11; 32];
    const LAMPS: [u128; 3] = [70, 71, 72];
    /// Each lamp blinks with a 1 s period from its phase (logic.md appendix A).
    const PHASES: [f32; 3] = [0.0, 0.5, 0.25];

    /// The lamp's bytes, built once.
    /// The toolchain pe-logic's build service runs here (main's build isolation, ed24eea5: by absolute path).
    fn toolchain() -> pe_logic::build::Toolchain {
        pe_logic::build::Toolchain::current().unwrap_or_else(|e| panic!("{e}"))
    }

    fn wasm() -> &'static [u8] {
        static W: OnceLock<Vec<u8>> = OnceLock::new();
        W.get_or_init(|| {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pe-logic/tests/scripts/lamp");
            let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scripts");
            pe_logic::build::build(&toolchain(), &dir, &pe_logic::build::engine_sdk(), &target, true)
                .unwrap_or_else(|e| panic!("{e}"))
        })
    }

    /// The lamp's version hash V, which its ids derive from: any fixed hash will do here.
    /// The source archive the lamp's and the tally's commits carry (logic.md 4.1a): the fixtures' one version.
    fn source() -> Bytes {
        pe_world::script::pack_source(&[("src/lib.rs", b"// a fixture\n".as_slice())]).expect("an archive").into()
    }

    fn v() -> Hash {
        Hash(BlobRef::of(&source()).hash)
    }

    /// The lamp's module hash and its own components (Blink, Glow) under V.
    fn parts() -> &'static (Hash, Vec<ComponentDesc>) {
        static P: OnceLock<(Hash, Vec<ComponentDesc>)> = OnceLock::new();
        P.get_or_init(|| {
            let s = Settings::default();
            let script = Script::load(&runtime::engine(&s).unwrap(), s, wasm()).unwrap();
            let m = run::read_manifest(&script).unwrap();
            let desc = |c: &pe_logic::manifest::Component| {
                let fields = run::component_desc(c, ComponentId(0)).unwrap().fields().to_vec();
                run::component_desc(c, script_component_id(&v(), &c.name, fields).unwrap()).unwrap()
            };
            (script.module_hash(), m.components.iter().map(desc).collect())
        })
    }

    /// Runs `f` on a thread with the native stack pe-logic's runner needs.
    fn big_stack(f: impl FnOnce() + Send + 'static) {
        let stack = Settings::default().thread_stack();
        std::thread::Builder::new().stack_size(stack).spawn(f).unwrap().join().unwrap();
    }

    fn runner(with_lamp: bool) -> LogicRunner {
        let mut r = LogicRunner::new(Settings::default(), RunSettings::default()).expect("a runner");
        if with_lamp {
            assert_eq!(r.insert(wasm()).expect("the lamp loads"), parts().0);
        }
        r
    }

    /// The floor and the ball, the lamp committed at frame 0, and three lamps, dark.
    fn lit() -> World {
        let (module, descs) = parts();
        let mut w = floor_and_ball();
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let mut components = vec![tag_desc(&v())];
        components.extend(descs.iter().cloned());
        let commit = tx(vec![
            Op::Register { components },
            Op::commit_script(LAMP, None, source(), Hash([1; 32]), Hash([2; 32]), *module, 0),
        ]);
        applied(&w.apply_edit(Frame(0), &b.next(Class::Edit, Frame(0), Bytes::from(commit.encode()))).0);
        assert_eq!(w.end_frame(Frame(0)).activated.len(), 1, "active from frame 1 (6.2a)");
        for (k, &n) in LAMPS.iter().enumerate() {
            w.create(oid(n), None, MemberKey([1; 32]), GENESIS).expect("a lamp");
            w.insert(oid(n), Transform::at(pos(k as f64, 2.0, 0.0))).expect("its transform");
            let blink: Vec<u8> = [1.0f32, PHASES[k], 2.0].iter().flat_map(|x| x.to_le_bytes()).collect();
            let set = |component, bytes| Op::SetRow {
                id: Target::Existing(oid(n)),
                row: Row { component, instance: 0, bytes },
            };
            let ops =
                vec![set(descs[0].id(), blink), set(descs[1].id(), vec![0; 16]), set(tag_desc(&v()).id(), vec![])];
            applied(&w.apply_edit(Frame(0), &edit(&mut b, 0, ops)).0);
        }
        w
    }

    fn intensity(r: &ReplicaImpl<PeWorld>, n: u128) -> f32 {
        let row = r.world().world().store().get_raw(parts().1[1].id(), oid(n)).expect("a glow row");
        f32::from_le_bytes(row[12..16].try_into().expect("four bytes"))
    }

    /// pe-logic's runner passes the check every runner passes, on the lamp.
    #[test]
    fn the_logic_runner_conforms_on_the_lamp() {
        big_stack(|| {
            let make = || Box::new(runner(true)) as Box<dyn ScriptRunner>;
            conformance(&make, &fixture_of(&lit())).expect("pe-logic's runner conforms");
        });
    }

    /// Each logic stage runs its own systems only: the lamp's `pulse` is a pre-physics system, so at frame 0 the first
    /// stage switches two lamps on (the third, half a period in, stays dark: unchanged) and the second writes nothing.
    /// The row whose module the runner lacks is missing at both.
    #[test]
    fn a_stage_runs_its_own_systems_only() {
        big_stack(|| {
            let fx = fixture_of(&lit());
            let mut r = runner(true);
            let mut store = fx.store.clone();
            let faulted = BTreeSet::new();
            let guard = |_: ObjectId| false;
            let check = |_: ComponentId, _: ObjectId, _: &[u8]| -> Result<(), String> { Ok(()) };
            let world = StageWorld {
                frame: Frame(0),
                line: fx.line,
                rate_hz: 60,
                guard: &guard,
                faulted: &faulted,
                check: &check,
            };
            let pre = r.run_stage(Stage::LogicPrePhysics, &fx.scripts, &mut store, &world);
            let post = r.run_stage(Stage::LogicPostPhysics, &fx.scripts, &mut store, &world);
            let glow = parts().1[1].id();
            assert_eq!(pre.report.written, [(glow, oid(LAMPS[0])), (glow, oid(LAMPS[2]))]);
            assert!(post.report.written.is_empty(), "{post:?}");
            assert_eq!((pre.missing, post.missing), (vec![[8; 32]], vec![[8; 32]]));
        });
    }

    /// pe-logic's runner conforms where its copy-in skip (`RunSettings::skip_unchanged`, on by default) fires and its
    /// input changes every frame (@engine-architect's question): in tally, `count` writes `Counter`
    /// and `double`, the next system of the same stage, binds the same objects and reads it, so the column `count`
    /// wrote back is not copied in again. The skip trusts only what this stage's calls left in the area: a replay from
    /// a saved store runs as the first run did, and the copies skipped show in the report.
    #[test]
    fn the_logic_runner_conforms_where_its_copy_in_skip_fires() {
        big_stack(|| {
            const TALLY: [u8; 32] = [14; 32];
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pe-logic/tests/scripts/tally");
            let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scripts");
            let wasm = pe_logic::build::build(&toolchain(), &dir, &pe_logic::build::engine_sdk(), &target, true)
                .unwrap_or_else(|e| panic!("{e}"));
            let s = Settings::default();
            let script = Script::load(&runtime::engine(&s).unwrap(), s, &wasm).unwrap();
            let module = script.module_hash();
            let m = run::read_manifest(&script).unwrap();
            let desc = |c: &pe_logic::manifest::Component| {
                let fields = run::component_desc(c, ComponentId(0)).unwrap().fields().to_vec();
                run::component_desc(c, script_component_id(&v(), &c.name, fields).unwrap()).unwrap()
            };
            let descs: Vec<ComponentDesc> =
                m.components.iter().filter(|c| !c.name.starts_with("pe.")).map(desc).collect();
            let id_of = |name: &str| descs.iter().find(|d| d.name() == name).expect("a tally component").id();
            let mut w = floor_and_ball();
            let keys = keys();
            let mut b = EntryBuilder::new(&keys, 0);
            let mut components = vec![tag_desc(&v())];
            components.extend(descs.iter().cloned());
            let commit = tx(vec![
                Op::Register { components },
                Op::commit_script(TALLY, None, source(), Hash([1; 32]), Hash([2; 32]), module, 0),
            ]);
            applied(&w.apply_edit(Frame(0), &b.next(Class::Edit, Frame(0), Bytes::from(commit.encode()))).0);
            assert_eq!(w.end_frame(Frame(0)).activated.len(), 1, "active from frame 1 (6.2a)");
            for (k, n) in (90u128..93).enumerate() {
                w.create(oid(n), None, MemberKey([1; 32]), GENESIS).expect("a counter");
                let counter: Vec<u8> = [k as u32, 3].iter().flat_map(|x| x.to_le_bytes()).collect();
                let set = |component, bytes| Op::SetRow {
                    id: Target::Existing(oid(n)),
                    row: Row { component, instance: 0, bytes },
                };
                let ops = vec![
                    set(id_of("Counter"), counter),
                    set(id_of("Doubled"), vec![0; 8]),
                    set(tag_desc(&v()).id(), vec![]),
                ];
                applied(&w.apply_edit(Frame(0), &edit(&mut b, 0, ops)).0);
            }
            let mut fx = fixture_of(&w);
            fx.frames = 12;
            let make = move || {
                let mut r = runner(false);
                r.insert(&wasm).expect("the tally loads");
                Box::new(r) as Box<dyn ScriptRunner>
            };
            conformance(&make, &fx).expect("pe-logic's runner conforms with its copy-in skip");
            // The skip fired: pe-logic's own report of the same stage, outside the replica, counts the bytes skipped.
            let mut logic = run::Logic::new(RunSettings::default());
            let own: Vec<ComponentId> = descs.iter().map(|d| d.id()).collect();
            let table: Vec<run::EngineComponent<'_>> = (pe_world::script::ENGINE_COMPONENTS.iter())
                .map(|e| run::EngineComponent { name: e.name, id: e.id, scripts_may_write: e.scripts_may_write })
                .collect();
            let bound =
                run::Loaded::bind(Hash(TALLY), script, m, &own, tag_desc(&v()).id(), &table, w.store().registry());
            logic.insert(bound.expect("tally binds"));
            let mut store = w.store().clone();
            let none = BTreeSet::new();
            let ctx = &mut run::StageCtx::new(LineId::origin(), 60, pe_sdk::layout::Stage::PrePhysics, &none);
            let report = logic.run_stage(&mut store, ctx);
            assert!(report.skipped_in > 0, "the skip fired: {report:?}");
        });
    }

    /// The lamp runs in the replica's frame: after frames 0 to 29 each lamp's glow is what its rule gives at frame 29
    /// (t = 29/60 s, on while (t + phase) mod 1 s < 0.5 s): on, off, off. The runner's identity is pe-logic's.
    #[test]
    fn the_lamp_runs_in_the_replicas_frame() {
        big_stack(|| {
            let mut s = Source::default();
            let mut r = replica_of(lit()).with_runner(Box::new(runner(true)));
            r.advance(Frame(30), &mut s);
            assert_eq!((r.frame(), r.faults()), (Frame(30), 0));
            assert_eq!(LAMPS.map(|n| intensity(&r, n)), [1.0, 0.0, 0.0]);
            assert_eq!(r.script_counts().missing, 0);
            let identity = pe_logic::identity(&Settings::default(), &RunSettings::default());
            assert_eq!(r.runner_identity(), Some(pe_seams::RunnerIdentity(Bytes::from(identity))));
        });
    }

    /// A late entry rolls the replica back across frames where the lamps switched: the replay reaches the roots the
    /// in-order run reached.
    #[test]
    fn a_rollback_replays_the_lamp_to_the_same_roots() {
        big_stack(|| {
            let keys = keys();
            let mut b = EntryBuilder::new(&keys, 0);
            let late = edit(&mut b, 12, vec![Op::Delete { id: Target::Existing(oid(BALL)) }]);
            let mut all = Source::default();
            all.add(late.clone());
            let mut a = replica_of(lit()).with_runner(Box::new(runner(true)));
            a.advance(Frame(40), &mut all);
            let mut s = Source::default();
            let mut r = replica_of(lit()).with_runner(Box::new(runner(true)));
            r.advance(Frame(30), &mut s);
            s.add(late);
            let adv = r.advance(Frame(40), &mut s);
            assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 12), "rolled back: {adv:?}");
            assert_eq!((a.faults(), r.faults()), (0, 0));
            assert_eq!(roots(&r, 40), roots(&a, 40));
            assert_eq!(roots(&r, 40).len(), 6);
            assert_eq!(LAMPS.map(|n| intensity(&r, n)), LAMPS.map(|n| intensity(&a, n)));
        });
    }

    /// pe-logic's tally script (its batch runner's test script) through the replica. Its `spawn` system creates a child
    /// and emits `spawned` with the count made so far: the event becomes an S5 effect under its declared name's kind,
    /// and the child is in the world. Its `trouble` system panics after the physics: the fault is world state (the
    /// modules row records it) and the script runs no more, so the second spawn never happens.
    #[test]
    fn a_script_event_reaches_the_view_and_a_fault_stops_the_script() {
        big_stack(|| {
            const TALLY: [u8; 32] = [13; 32];
            const SPAWNER: u128 = 80;
            const TROUBLE: u128 = 81;
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pe-logic/tests/scripts/tally");
            let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scripts");
            let wasm = pe_logic::build::build(&toolchain(), &dir, &pe_logic::build::engine_sdk(), &target, true)
                .unwrap_or_else(|e| panic!("{e}"));
            let mut runner = runner(false);
            let module = runner.insert(&wasm).expect("the tally loads");
            let s = Settings::default();
            let m = run::read_manifest(&Script::load(&runtime::engine(&s).unwrap(), s, &wasm).unwrap()).unwrap();
            let own = m.components.iter().filter(|c| !c.name.starts_with("pe."));
            let desc = |c: &pe_logic::manifest::Component| {
                let fields = run::component_desc(c, ComponentId(0)).unwrap().fields().to_vec();
                run::component_desc(c, script_component_id(&v(), &c.name, fields).unwrap()).unwrap()
            };
            let descs: Vec<ComponentDesc> = own.map(desc).collect();
            let id_of = |name: &str| descs.iter().find(|d| d.name() == name).expect("a tally component").id();
            let (spawner, trouble) = (id_of("Spawner"), id_of("Trouble"));
            let trouble_size = descs.iter().find(|d| d.id() == trouble).expect("Trouble").size();
            let mut w = floor_and_ball();
            let keys = keys();
            let mut b = EntryBuilder::new(&keys, 0);
            let mut components = vec![tag_desc(&v())];
            components.extend(descs.iter().cloned());
            let commit = tx(vec![
                Op::Register { components },
                Op::commit_script(TALLY, None, source(), Hash([1; 32]), Hash([2; 32]), module, 0),
            ]);
            applied(&w.apply_edit(Frame(0), &b.next(Class::Edit, Frame(0), Bytes::from(commit.encode()))).0);
            assert_eq!(w.end_frame(Frame(0)).activated.len(), 1, "active from frame 1 (6.2a)");
            for n in [SPAWNER, TROUBLE] {
                w.create(oid(n), None, MemberKey([1; 32]), GENESIS).expect("an object");
                w.insert(oid(n), Transform::at(pos(0.0, 3.0, n as f64 - 80.0))).expect("its transform");
            }
            let set = |n, component, bytes| Op::SetRow {
                id: Target::Existing(oid(n)),
                row: Row { component, instance: 0, bytes },
            };
            let left: Vec<u8> = [2u32, 0].iter().flat_map(|x| x.to_le_bytes()).collect();
            let mut panics = vec![0; trouble_size];
            panics[..4].copy_from_slice(&2u32.to_le_bytes());
            let ops = vec![
                set(SPAWNER, spawner, left),
                set(SPAWNER, tag_desc(&v()).id(), vec![]),
                set(TROUBLE, trouble, panics),
                set(TROUBLE, tag_desc(&v()).id(), vec![]),
            ];
            applied(&w.apply_edit(Frame(0), &edit(&mut b, 0, ops)).0);
            let mut r = replica_of(w).with_runner(Box::new(runner));
            let objects = r.world().world().store().objects().len();
            r.advance(Frame(3), &mut Source::default());
            assert_eq!((r.frame(), r.faults(), r.script_counts().missing), (Frame(3), 0, 0));
            let row = r.world().world().modules()[&TALLY].clone();
            assert!(row.faulted.is_some_and(|(f, _)| f == Frame(1)), "its first frame: {:?}", row.faulted);
            assert_eq!(r.script_counts().host_faults, 0);
            let view = r.view();
            let kind = EffectKind::Script(event_kind(&TALLY, "spawned"));
            let key = EffectKey { frame: Frame(1), cause: EffectCause::Logic, object: oid(SPAWNER), kind, ordinal: 0 };
            assert_eq!(keys_of(&view), [key]);
            assert_eq!(view.effects[0].data, Bytes::from(0u32.to_le_bytes().to_vec()));
            assert_eq!(r.world().world().store().objects().len(), objects + 1, "the child");
        });
    }

    /// A runner without the lamp's module steps on and counts the lamp missing at each stage; the lamps stay dark.
    #[test]
    fn without_the_module_the_lamp_is_missing_and_stays_dark() {
        big_stack(|| {
            let mut s = Source::default();
            let mut r = replica_of(lit()).with_runner(Box::new(runner(false)));
            r.advance(Frame(10), &mut s);
            assert_eq!((r.frame(), r.faults()), (Frame(10), 0));
            assert_eq!(LAMPS.map(|n| intensity(&r, n)), [0.0; 3]);
            assert_eq!(r.script_counts().missing, 18, "one per stage, frames 1 to 9");
        });
    }

    /// A module loaded through the runner's inbox on another thread (pe-app's build thread, logic.md 6.2 steps 3-4) is
    /// held from the start of the runner's next frame: the lamp, missing at frames 1 to 9, runs from frame 10, and
    /// after frame 29 each lamp's glow is what its rule gives there, as with the module from the start.
    #[test]
    fn a_module_loaded_through_the_inbox_runs_from_the_next_frame() {
        big_stack(|| {
            let runner = runner(false);
            let inbox = runner.inbox();
            let mut s = Source::default();
            let mut r = replica_of(lit()).with_runner(Box::new(runner));
            r.advance(Frame(10), &mut s);
            assert_eq!(r.script_counts().missing, 18, "one per stage, frames 1 to 9");
            let build = std::thread::Builder::new().stack_size(inbox.thread_stack());
            let loaded = build.spawn(move || inbox.load(wasm())).unwrap().join().unwrap();
            assert_eq!(loaded.expect("the lamp loads"), parts().0);
            r.advance(Frame(30), &mut s);
            assert_eq!((r.frame(), r.faults()), (Frame(30), 0));
            assert_eq!(r.script_counts().missing, 18, "none from frame 10");
            assert_eq!(LAMPS.map(|n| intensity(&r, n)), [1.0, 0.0, 0.0]);
        });
    }

    /// The runner takes what its inbox loaded at a frame's start only, so both stages of a frame see the same modules:
    /// loaded between them, the lamp is missing at both stages of that frame and runs at the next frame's first stage.
    /// A module that won't load is refused at the load.
    #[test]
    fn a_module_loaded_between_a_frames_stages_waits_for_the_next_frame() {
        big_stack(|| {
            let fx = fixture_of(&lit());
            let mut r = runner(false);
            let inbox = r.inbox();
            let mut store = fx.store.clone();
            let faulted = BTreeSet::new();
            let guard = |_: ObjectId| false;
            let check = |_: ComponentId, _: ObjectId, _: &[u8]| -> Result<(), String> { Ok(()) };
            let world = |frame| StageWorld {
                frame: Frame(frame),
                line: fx.line,
                rate_hz: 60,
                guard: &guard,
                faulted: &faulted,
                check: &check,
            };
            assert!(inbox.load(b"not a module").is_err());
            let pre = r.run_stage(Stage::LogicPrePhysics, &fx.scripts, &mut store, &world(0));
            assert_eq!(inbox.load(wasm()).expect("the lamp loads"), parts().0);
            assert!(!r.holds(&parts().0), "not before the next frame");
            let post = r.run_stage(Stage::LogicPostPhysics, &fx.scripts, &mut store, &world(0));
            assert!(pre.report.written.is_empty(), "{pre:?}");
            assert_eq!((pre.missing, post.missing), (vec![LAMP, [8; 32]], vec![LAMP, [8; 32]]));
            let next = r.run_stage(Stage::LogicPrePhysics, &fx.scripts, &mut store, &world(1));
            assert!(r.holds(&parts().0));
            assert_eq!(next.missing, vec![[8; 32]]);
            let glow = parts().1[1].id();
            assert_eq!(next.report.written, [(glow, oid(LAMPS[0])), (glow, oid(LAMPS[2]))]);
        });
    }

    /// A script bound again after its row left (as on a rollback across its activation) runs as it did the first
    /// time: the runner binds a clone of the module it holds.
    #[test]
    fn a_script_bound_again_runs_as_it_did() {
        big_stack(|| {
            let fx = fixture_of(&lit());
            let mut r = runner(true);
            let faulted = BTreeSet::new();
            let guard = |_: ObjectId| false;
            let check = |_: ComponentId, _: ObjectId, _: &[u8]| -> Result<(), String> { Ok(()) };
            let world = StageWorld {
                frame: Frame(0),
                line: fx.line,
                rate_hz: 60,
                guard: &guard,
                faulted: &faulted,
                check: &check,
            };
            let without: Vec<ScriptRow> = fx.scripts.iter().filter(|s| s.asset != LAMP).cloned().collect();
            let mut stage = |scripts: &[ScriptRow]| {
                let mut store = fx.store.clone();
                let out = r.run_stage(Stage::LogicPrePhysics, scripts, &mut store, &world);
                (out.report.written, out.missing)
            };
            let first = stage(&fx.scripts);
            assert_eq!(first.0.len(), 2, "two lamps switch on: {first:?}");
            assert_eq!(stage(&without), (vec![], vec![[8; 32]]));
            for _ in 0..3 {
                assert_eq!(stage(&fx.scripts), first);
            }
        });
    }

    /// The n = 2 path of logic.md 6.2a with pe-logic's runner (@coordinator-opus). A commits the lamp at
    /// frame 2 holding its own build. B holds no module until its build thread loads the lamp through B's inbox, then
    /// attests at frame 6. The commit activates at frame 6's end on both, and A's edit at frame 8 tags the lamps. Both
    /// replicas take the same roots at every cut, run the lamp from frame 7, and glow at frame 29 as its rule gives. A
    /// third replica, which sees A's entry at frame 4 only after reaching frame 20, rolls back across the activation,
    /// binds the lamp again (a clone of the module it holds) and reaches A's roots.
    #[test]
    fn two_members_activate_a_built_module_at_the_same_frame_and_agree() {
        big_stack(|| {
            let keys = keys();
            let (module, descs) = parts();
            let mut world = floor_and_ball();
            world.set_member(keys.key(1), perm::EDIT);
            for (k, &n) in LAMPS.iter().enumerate() {
                world.create(oid(n), None, MemberKey([1; 32]), GENESIS).expect("a lamp");
                world.insert(oid(n), Transform::at(pos(k as f64, 2.0, 0.0))).expect("its transform");
            }
            let (mut a, mut b) = (EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 1));
            let mut components = vec![tag_desc(&v())];
            components.extend(descs.iter().cloned());
            let commit = tx(vec![
                Op::Register { components },
                Op::commit_script(LAMP, None, source(), Hash([1; 32]), Hash([2; 32]), *module, 2),
            ]);
            let commit = a.next(Class::Edit, Frame(2), Bytes::from(commit.encode()));
            let late = edit(&mut a, 4, vec![Op::Delete { id: Target::Existing(oid(BALL)) }]);
            let attest = edit(&mut b, 6, vec![Op::AttestBuild { script: LAMP, version: v(), module: *module }]);
            let mut tags = Vec::new();
            for (k, &n) in LAMPS.iter().enumerate() {
                let blink: Vec<u8> = [1.0f32, PHASES[k], 2.0].iter().flat_map(|x| x.to_le_bytes()).collect();
                let set = |component, bytes| Op::SetRow {
                    id: Target::Existing(oid(n)),
                    row: Row { component, instance: 0, bytes },
                };
                tags.extend([
                    set(descs[0].id(), blink),
                    set(descs[1].id(), vec![0; 16]),
                    set(tag_desc(&v()).id(), vec![]),
                ]);
            }
            let tag = edit(&mut a, 8, tags);
            let source_of = |with_late: bool| {
                let mut s = Source::default();
                for e in [&commit, &attest, &tag] {
                    s.add(e.clone());
                }
                if with_late {
                    s.add(late.clone());
                }
                s
            };
            // B's build thread: the module reaches B's runner through its inbox, from another thread.
            let built = || {
                let runner = runner(false);
                let inbox = runner.inbox();
                let build = std::thread::Builder::new().stack_size(inbox.thread_stack());
                let loaded = build.spawn(move || inbox.load(wasm())).unwrap().join().unwrap();
                assert_eq!(loaded.expect("the lamp loads"), *module);
                runner
            };
            let mut author = replica_of(world.clone()).with_runner(Box::new(runner(true)));
            author.advance(Frame(30), &mut source_of(true));
            let mut voter = replica_of(world.clone()).with_runner(Box::new(built()));
            voter.advance(Frame(30), &mut source_of(true));
            for r in [&author, &voter] {
                assert_eq!((r.frame(), r.faults(), r.script_counts().missing), (Frame(30), 0, 0));
                assert_eq!(r.world().world().modules()[&LAMP].activate_at, Frame(7));
                assert_eq!(LAMPS.map(|n| intensity(r, n)), [1.0, 0.0, 0.0]);
            }
            assert_eq!(roots(&voter, 30), roots(&author, 30));
            assert_eq!(roots(&author, 30).len(), 4);

            let mut s = source_of(false);
            let mut later = replica_of(world).with_runner(Box::new(built()));
            later.advance(Frame(20), &mut s);
            s.add(late);
            let adv = later.advance(Frame(30), &mut s);
            assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 4), "rolled back across F: {adv:?}");
            assert_eq!((later.frame(), later.faults(), later.script_counts().missing), (Frame(30), 0, 0));
            assert_eq!(roots(&later, 30), roots(&author, 30));
            assert_eq!(LAMPS.map(|n| intensity(&later, n)), [1.0, 0.0, 0.0]);
        });
    }

    /// The inbox describes a module as a commit of a version registers it, without holding it: the hash of its
    /// metered bytes and its own components under the version's ids, the lamp's Blink and Glow. A frame later the
    /// runner still holds nothing, and the lamp is missing.
    #[test]
    fn the_inbox_describes_a_module_without_holding_it() {
        big_stack(|| {
            let fx = fixture_of(&lit());
            let mut r = runner(false);
            let d = r.inbox().describe(wasm(), &v()).expect("the lamp describes");
            assert_eq!((&d.module, &d.components), (&parts().0, &parts().1));
            assert_eq!(d.components.iter().map(|c| c.name()).collect::<Vec<_>>(), ["Blink", "Glow"]);
            let faulted = BTreeSet::new();
            let guard = |_: ObjectId| false;
            let check = |_: ComponentId, _: ObjectId, _: &[u8]| -> Result<(), String> { Ok(()) };
            let world = StageWorld {
                frame: Frame(0),
                line: fx.line,
                rate_hz: 60,
                guard: &guard,
                faulted: &faulted,
                check: &check,
            };
            let out = r.run_stage(Stage::LogicPrePhysics, &fx.scripts, &mut fx.store.clone(), &world);
            assert!(!r.holds(&d.module));
            assert_eq!(out.missing, vec![LAMP, [8; 32]]);
        });
    }
}
