//! The `replay-bundle` example (divergence.md 6) on the real world: a bundle built from a host replica's state at `Cv`
//! and its entries replays to the host's roots, and a bundle missing one entry first differs where that entry applies.

#[allow(dead_code)]
#[path = "../examples/replay-bundle.rs"]
mod tool;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_diverge::evidence::all_keys;
use pe_diverge::{CvState, FaultBundle};
use pe_replica::mock::MockClock;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    Class, Cut, CutRef, DomainId, EntrySource, FinalCut, Frame, Hash, HostTime, LineId, MemberKey, ObjectId, Offset,
    Pos, Replica, SignedEntry, SimRate,
};
use pe_sim::Transform;
use pe_world::components::{Body, Shape, body_kind, shape_kind};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::{BlobStore, GENESIS, World, perm};

fn pos(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).expect("in range")
}

fn row<T: pe_sim::Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn keys() -> TestKeys {
    TestKeys::new(5, 2)
}

/// A static floor, and two editors (keys 0 and 1).
fn genesis() -> World {
    genesis_in(BlobStore::default(), false)
}

/// Genesis in `blobs`; with `mesh`, the floor's shape names a mesh blob stored there.
fn genesis_in(blobs: BlobStore, mesh: bool) -> World {
    let mesh = if mesh { blobs.put(b"a floor mesh") } else { pe_world::components::BlobRef::NONE };
    let mut w = World::new(1, blobs);
    w.set_member(keys().key(0), perm::EDIT);
    w.set_member(keys().key(1), perm::EDIT);
    let floor = ObjectId::from_u128(1);
    w.create(floor, None, MemberKey([1; 32]), GENESIS).expect("floor");
    w.insert(floor, Transform::at(pos(0.0, -0.5, 0.0))).expect("floor transform");
    w.insert(floor, Body { friction: 0.6, ..Body::default() }).expect("floor body");
    w.insert(floor, Shape { kind: shape_kind::BOX, density: 500.0, size: [10.0, 0.5, 10.0], mesh, ..Shape::default() })
        .expect("floor shape");
    w
}

/// A transaction dropping a ball at (x, y).
fn drop(x: f64, y: f64) -> Bytes {
    {
        let body =
            Body { kind: body_kind::DYNAMIC, friction: 0.6, gravity_scale: 1.0, may_sleep: 1, ..Body::default() };
        let ball = Shape { kind: shape_kind::SPHERE, density: 800.0, size: [0.25, 0.0, 0.0], ..Shape::default() };
        let rows = vec![row(Transform::at(pos(x, y, 0.0))), row(body), row(ball)];
        let tx = Transaction {
            actor: Actor::Person,
            label: None,
            preconditions: vec![],
            ops: vec![Op::Create { new: 0, parent: Target::Root, index: None, rows }],
            blobs: vec![],
        };
        Bytes::from(tx.encode())
    }
}

/// Balls dropped by editor 0 at frames 2 and 5 (before `Cv` = 8) and 12 and 20 (in the window).
fn scene() -> Vec<SignedEntry> {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    vec![
        b.next(Class::Edit, Frame(2), drop(0.0, 1.0)),
        b.next(Class::Edit, Frame(5), drop(0.3, 2.0)),
        b.next(Class::Edit, Frame(12), drop(-0.4, 1.5)),
        b.next(Class::Edit, Frame(20), drop(0.6, 1.2)),
    ]
}

#[derive(Default)]
struct Source {
    entries: BTreeMap<Frame, Vec<SignedEntry>>,
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
        None
    }
}

const CV: u64 = 8;
const C: u64 = 32;

fn c_ref() -> CutRef {
    CutRef { line: LineId::origin(), height: 4, frame: Frame(C) }
}

/// The host: all entries from the start, run past `C`.
fn host(entries: &[SignedEntry]) -> (ReplicaImpl<PeWorld>, Source) {
    host_of(genesis(), entries)
}

fn host_of(world: World, entries: &[SignedEntry]) -> (ReplicaImpl<PeWorld>, Source) {
    let rate = SimRate::DEFAULT;
    let config = ReplicaConfig::new(rate, Duration::from_millis(133), Arc::new(MockClock { rate, start: HostTime(0) }));
    let mut r = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).expect("a replica");
    let mut s = Source::default();
    for e in entries {
        let at = s.entries.entry(e.entry.stamp).or_default();
        at.push(e.clone());
        at.sort_by_key(|e| (e.entry.chain, e.entry.seq));
    }
    r.advance(Frame(C + 8), &mut s);
    (r, s)
}

/// The bundle a member holding the host's state at `Cv` writes: its whole state there, as pe-diverge's capture takes
/// it, the entries stamped in [Cv, C) in the source's order, and `C`'s heads. The cuts take `in_cut`'s entries.
fn bundle(host: &mut ReplicaImpl<PeWorld>, s: &mut Source, in_cut: &[SignedEntry]) -> (FaultBundle, Hash) {
    let entries = in_cut;
    let empty = Cut { at: CutRef { line: LineId::origin(), height: 1, frame: Frame(0) }, heads: vec![] };
    let cv = tool::cut_at(&empty, entries, &CutRef { line: LineId::origin(), height: 1, frame: Frame(CV) }, CV);
    let root = host.root_at_cut(&cv, s).expect("the host's root at Cv");
    let records = {
        let tree = host.tree(&root).expect("held");
        tree.export(&all_keys(&*tree).expect("a whole tree"))
    };
    let state = CvState {
        cv: cv.clone(),
        root,
        domain: DomainId(0),
        rate_hz: SimRate::DEFAULT.hz(),
        records,
        physics: host.physics_image(&root),
    };
    let c = tool::cut_at(&cv, entries, &c_ref(), C);
    let at_c = host.root_at_cut(&c, s).expect("the host's root at C");
    let carried = (CV..C).flat_map(|f| s.entries_at(Frame(f)).unwrap_or(&[]).to_vec());
    let b = FaultBundle {
        cv: Some((cv.at, root)),
        cut: c_ref(),
        agreed_root: at_c,
        state_at_cv: state.encode().expect("encodes"),
        entries: carried.map(|e| Bytes::from(e.encode())).collect(),
        cut_heads: c.heads.clone(),
        ..FaultBundle::default()
    };
    (b, at_c)
}

#[test]
fn a_bundle_replays_to_the_host_s_root_at_the_cut() {
    let entries = scene();
    let (mut host, mut s) = host(&entries);
    let (b, at_c) = bundle(&mut host, &mut s, &entries);
    assert_eq!(b.entries.len(), 2, "the entries at 12 and 20");
    let b = FaultBundle::decode(&b.encode().unwrap()).expect("the bundle round-trips");
    let r = tool::replay(&b, tool::MAX_FRAMES).expect("it replays");
    assert_eq!((r.cv, r.c, r.roots.len()), (CV, C, (C - CV + 1) as usize));
    assert_eq!(r.at_c, Some(at_c), "this build reproduces the agreed root at C");
    // Every frame's root is the host's at the same cut.
    for (f, root) in &r.roots {
        assert!(root.is_ok(), "frame {f} has a root: {root:?}");
        let cut = tool::cut_at(&CvState::decode(&b.state_at_cv).unwrap().cv, &entries, &c_ref(), *f);
        assert_eq!(root.as_ref().ok(), host.root_at_cut(&cut, &mut s).ok().as_ref(), "frame {f}");
    }
}

#[test]
fn a_bundle_missing_an_entry_first_differs_where_it_applies() {
    let entries = scene();
    let (mut host, mut s) = host(&entries);
    let (b, at_c) = bundle(&mut host, &mut s, &entries);
    let full = tool::replay(&b, tool::MAX_FRAMES).expect("it replays");
    let mut short = b.clone();
    short.entries.pop(); // the entry stamped 20
    let r = tool::replay(&short, tool::MAX_FRAMES).expect("it replays");
    assert_eq!(full.first_difference(&r), Some(21), "the frame-20 entry is in effect from the cut at 21 up");
    assert_ne!(r.at_c, Some(at_c));
}

#[test]
fn a_carried_entry_the_cut_does_not_include_is_left_out() {
    // Editor 1's drop at 24 is stamped below C but included only by a later cut. The host's S3 serves it, so the
    // bundle carries it; with C's heads the replay leaves it out and reproduces the agreed root at C.
    let scene = scene();
    let keys = keys();
    let late = EntryBuilder::new(&keys, 1).next(Class::Edit, Frame(24), drop(-0.7, 1.4));
    let mut all = scene.clone();
    all.push(late);
    let (mut host, mut s) = host(&all);
    let (b, at_c) = bundle(&mut host, &mut s, &scene);
    assert_eq!(b.entries.len(), 3, "the late entry is carried");
    let r = tool::replay(&b, tool::MAX_FRAMES).expect("it replays");
    assert_eq!((r.at_c, r.not_in_c), (Some(at_c), 1));
    // Without C's heads, the rebuilt cut takes the late entry too, and misses the agreed root.
    let mut old = b.clone();
    old.cut_heads.clear();
    assert_ne!(tool::replay(&old, tool::MAX_FRAMES).expect("it replays").at_c, Some(at_c));
}

#[test]
fn a_mesh_missing_from_the_local_store_is_named() {
    let blobs = BlobStore::default();
    let entries = scene();
    let (mut host, mut s) = host_of(genesis_in(blobs.clone(), true), &entries);
    let (b, at_c) = bundle(&mut host, &mut s, &entries);
    let e = tool::replay(&b, tool::MAX_FRAMES).expect_err("the mesh is not here");
    assert!(e.starts_with("blob ") && e.contains("is not in the local store"), "{e}");
    let r = tool::replay_with(&b, tool::MAX_FRAMES, blobs).expect("with the store, it replays");
    assert_eq!(r.at_c, Some(at_c));
}

#[test]
fn a_state_that_does_not_check_is_refused() {
    let entries = scene();
    let (mut host, mut s) = host(&entries);
    let (mut b, _) = bundle(&mut host, &mut s, &entries);
    let mut st = CvState::decode(&b.state_at_cv).unwrap();
    let i = st.records.iter().position(|r| matches!(r.key, pe_seams::LeafKey::Object(_))).expect("an object");
    let mut bytes = st.records[i].bytes.to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    st.records[i].bytes = Bytes::from(bytes);
    b.state_at_cv = st.encode().unwrap();
    let e = tool::replay(&b, tool::MAX_FRAMES).expect_err("refused");
    assert!(e.starts_with("the state at Cv does not import"), "{e}");
    assert!(tool::replay(&FaultBundle::default(), 10).unwrap_err().starts_with("the bundle carries no state"));
}
