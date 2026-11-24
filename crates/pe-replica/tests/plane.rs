//! The friend test's flat plane on the real replica (pe-world `preset::plane_on`, `templates`; the friend-test item
//! @item:primordial-engine/friend-test-flat-plane-genesis-preset-with-templ): template folders stay out of play
//! across renames, and two members' spawned boxes, spheres, capsules and box stacks, grabbed and thrown, step to the
//! same roots whatever order the entries arrive in, and the same as a replay.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::mock::MockClock;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    Class, DomainId, EntrySource, FinalCut, Frame, Hash, HostTime, LineId, ObjectId, Offset, Outcome, Pos, Replica,
    SignedEntry,
};
use pe_sim::{Component, Transform};
use pe_world::components::{BodyState, Name};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::preset::{Preset, Session, find, m1_session, plane_names as n};
use pe_world::{BlobStore, HandPose, Input, World, perm};

const LINE: LineId = LineId(Hash([7; 32]));
const K: u64 = 8;
const CLOCK: MockClock = MockClock { rate: pe_seams::SimRate::DEFAULT, start: HostTime(1_000_000_000) };

fn keys() -> TestKeys {
    TestKeys::new(3, 2)
}

fn plane() -> World {
    let keys = keys();
    let s = Session {
        seed: 5,
        origin: Pos::ORIGIN,
        preset: Preset::Plane,
        line: LINE,
        rate: pe_seams::SimRate::DEFAULT,
        gravity: pe_world::GRAVITY,
        agree: b"pe-agree's settings".to_vec(),
        members: vec![(keys.key(0), perm::ADMIN | perm::EDIT), (keys.key(1), perm::EDIT)],
    };
    m1_session(&s, BlobStore::default()).expect("the plane")
}

fn replica_of(world: World) -> ReplicaImpl<PeWorld> {
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(CLOCK));
    config.cut_every = K;
    ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).expect("a replica")
}

fn row<T: Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

/// Agreement's stand-in: entries by frame, in chain and seq order, whatever order they arrive in.
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

fn roots(r: &ReplicaImpl<PeWorld>, to: u64) -> Vec<(u64, Hash)> {
    (0..=to).step_by(K as usize).filter_map(|f| r.live_root(Frame(f)).map(|h| (f, h))).collect()
}

/// One member's entries, built in stamp order. A scratch copy of the world applies each edit, so the ids a copy
/// makes are known (they derive from the entry, not from the state it meets).
struct Author {
    b: EntryBuilder,
    scratch: World,
    out: Vec<SignedEntry>,
}

impl Author {
    fn new(member: usize, world: &World) -> Self {
        Author { b: EntryBuilder::on_line(&keys(), member, LINE), scratch: world.clone(), out: vec![] }
    }

    fn edit(&mut self, at: u64, t: Transaction) -> Vec<ObjectId> {
        let e = self.b.next(Class::Edit, Frame(at), Bytes::from(t.encode()));
        let (o, _) = self.scratch.apply_edit(Frame(at), &e);
        self.out.push(e);
        match o {
            Outcome::Applied { created, .. } => created,
            r => panic!("refused at {at}: {r:?}"),
        }
    }

    fn input(&mut self, at: u64, i: &Input) {
        self.out.push(self.b.next(Class::Input, Frame(at), Bytes::from(i.encode())));
    }

    /// A copy of template `name` under `Spawned`, its root at `local` in `Spawned`'s frame: the copy's ids.
    fn spawn(&mut self, at: u64, name: &str, local: [f64; 3]) -> Vec<ObjectId> {
        let (template, spawned) =
            (find(&self.scratch, name).expect("a template"), find(&self.scratch, n::SPAWNED).unwrap());
        let snap = self.scratch.snapshot(self.scratch.frame());
        let size = 1 + descendants(&snap, template);
        let ops = vec![
            Op::Copy {
                source: Target::Existing(template),
                new: 0,
                parent: Target::Existing(spawned),
                index: None,
                size,
            },
            Op::SetRow { id: Target::New(0), row: row(Transform::local(local)) },
        ];
        self.edit(at, tx(ops, vec![]))
    }

    fn rename(&mut self, at: u64, id: ObjectId, to: &str) {
        let text = self.scratch.blobs().put(to.as_bytes());
        let ops = vec![Op::SetRow { id: Target::Existing(id), row: row(Name { text }) }];
        self.edit(at, tx(ops, vec![to.as_bytes().to_vec()]));
    }
}

fn descendants(snap: &pe_world::WorldSnapshot, id: ObjectId) -> u32 {
    snap.children(id).into_iter().map(|c| 1 + descendants(snap, c)).sum()
}

fn tx(ops: Vec<Op>, inline: Vec<Vec<u8>>) -> Transaction {
    Transaction { inline, actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
}

fn at(w: &World, id: ObjectId) -> Pos {
    w.store().world_transform(id).expect("placed").pos
}

#[test]
fn a_folder_renamed_away_and_back_agrees_on_two_replicas_and_its_bodies_come_back_at_rest() {
    let world = plane();
    let folder = find(&world, n::TEMPLATES).unwrap();
    let template_bodies: Vec<ObjectId> = {
        let snap = world.snapshot(world.frame());
        let mut all = vec![];
        let mut stack = snap.children(folder);
        while let Some(x) = stack.pop() {
            if world.store().get::<pe_world::components::Body>(x).is_some() {
                all.push(x);
            }
            stack.extend(snap.children(x));
        }
        all
    };
    let mut a = Author::new(0, &world);
    a.spawn(2, n::BOX, [0.0, 1.0, 0.0]);
    // Away at 12 (the folder's bodies fall for half a second), back at 44, away again at 90.
    a.rename(12, folder, "Shelf");
    a.rename(44, folder, n::TEMPLATES);
    a.rename(90, folder, "Shelf");
    let entries = a.out;
    let mut s = Source::default();
    entries.iter().cloned().for_each(|e| s.add(e));
    let mut one = replica_of(world.clone());
    one.advance(Frame(40), &mut s);
    {
        let w = one.world().world();
        assert!(template_bodies.iter().all(|&b| w.physical().contains(&b)), "away, they are in play");
        assert!(template_bodies.iter().all(|&b| w.store().get::<BodyState>(b).is_some()), "and moving");
    }
    one.advance(Frame(88), &mut s);
    {
        let w = one.world().world();
        assert!(template_bodies.iter().all(|&b| !w.physical().contains(&b)), "back, out of play");
        assert!(template_bodies.iter().all(|&b| w.store().get::<BodyState>(b).is_none()), "with no motion kept");
    }
    // Away again at 90: they start at rest where they were parked when they went back. By 96 each has fallen at most
    // as far as from rest over the 8 frames since 88, about 0.09 m at 60 Hz; one that kept the 32 frames of speed
    // it had when it went back would have fallen over 0.4 m.
    let parked: Vec<Pos> = template_bodies.iter().map(|&b| at(one.world().world(), b)).collect();
    one.advance(Frame(96), &mut s);
    let w = one.world().world();
    let t = 8.0 / f64::from(pe_seams::SimRate::DEFAULT.hz());
    let from_rest = 0.5 * 9.81 * t * t * 1.05 + 1e-3;
    for (&b, p) in template_bodies.iter().zip(&parked) {
        let d = at(w, b) - *p;
        let level = d.0[0].abs() < 1e-6 && d.0[2].abs() < 1e-6;
        assert!(level && d.0[1] < 0.0 && d.0[1] > -from_rest, "{b:?} moved {d:?}, from rest at most {from_rest}");
    }
    one.advance(Frame(160), &mut s);
    assert_eq!(one.faults(), 0);
    // A second replica that hears of the renames late, after stepping past all of them: the same roots.
    let mut two = replica_of(world);
    let mut late = Source::default();
    late.add(entries[0].clone());
    two.advance(Frame(120), &mut late);
    entries[1..].iter().cloned().for_each(|e| late.add(e));
    two.advance(Frame(160), &mut late);
    assert_eq!(roots(&two, 160), roots(&one, 160));
    assert_eq!(two.faults(), 0);
}

/// The friend test's play, in-crate: member 0 spawns a box, a sphere, a capsule and two stacks; member 1, at the same
/// frames, spawns a third stack and a second box, then its right hand takes member 0's box, carries it and throws it
/// at a stack.
fn mixed_play(world: &World) -> (Vec<SignedEntry>, Vec<SignedEntry>, ObjectId) {
    let mut a = Author::new(0, world);
    let mut b = Author::new(1, world);
    let the_box = a.spawn(10, n::BOX, [0.0, 0.3, 0.0])[0];
    a.spawn(10, n::SPHERE, [0.5, 0.5, 0.0]);
    a.spawn(14, n::CAPSULE, [-0.5, 0.6, 0.0]);
    a.spawn(14, n::STACK, [1.2, 0.0, -1.0]);
    a.spawn(20, n::STACK, [-1.2, 0.0, -1.0]);
    b.spawn(14, n::STACK, [0.0, 0.0, -1.5]);
    b.spawn(20, n::BOX, [0.3, 0.4, 0.4]);
    // Member 1's right hand: to just above the box once it has settled, grab, lift and carry, then throw.
    let hand = ObjectId::hand(LINE, keys().key(1), 1, Frame(0));
    let above = |dx: f64, dy: f64, dz: f64| Pos::ORIGIN.checked_add(Offset([dx, 0.2 + 0.12 + dy, dz])).unwrap();
    let pose = |p: Pos, engaged: bool| HandPose { pos: p, rot: [0.0, 0.0, 0.0, 1.0], grip: 0.8, engaged };
    for f in (60..90).step_by(4) {
        b.input(f, &Input::Hand { hand, poses: vec![pose(above(0.0, 0.0, 0.0), f >= 84)], hold: 4 });
    }
    b.input(88, &Input::Grab { object: the_box, hand: Some(hand) });
    for (k, f) in (92..140).step_by(4).enumerate() {
        let t = k as f64 / 12.0;
        b.input(f, &Input::Hand { hand, poses: vec![pose(above(0.0, 0.5 * t, -0.6 * t), true)], hold: 4 });
    }
    b.input(140, &Input::Throw { object: the_box, linear: [0.0, 1.0, -3.0], angular: [0.0, 2.0, 0.0] });
    for f in (144..160).step_by(4) {
        b.input(f, &Input::Hand { hand, poses: vec![pose(above(0.0, 0.5, -0.6), false)], hold: 4 });
    }
    (a.out, b.out, the_box)
}

#[test]
fn two_members_mixed_shapes_agree_whatever_the_arrival_order_and_equal_a_replay() {
    const TO: u64 = 480;
    let world = plane();
    let (ea, eb, the_box) = mixed_play(&world);
    // In order, as the entries' stamps come.
    let mut s = Source::default();
    ea.iter().chain(&eb).cloned().for_each(|e| s.add(e));
    let mut live = replica_of(world.clone());
    live.advance(Frame(92), &mut s);
    assert!(live.world().world().claims().any(|(o, _)| o == the_box), "member 1's hand holds the box");
    let settled = at(live.world().world(), the_box);
    live.advance(Frame(TO), &mut s);
    let want = roots(&live, TO);
    assert_eq!(want.len() as u64, TO / K + 1);
    // For the cross-machine check (Windows against Linux, `--nocapture`): the same line on both, or they diverge.
    let (f, root) = want.last().expect("a root");
    println!("MIXED-SHAPE ROOT frame {f} {root}");
    assert_eq!(live.faults(), 0);
    let w = live.world().world();
    // Everything spawned is in play under Spawned: 1 + 1 + 1 + 3 x 5 + 1 bodies, plus the floor.
    let spawned = find(w, n::SPAWNED).unwrap();
    assert_eq!(w.snapshot(w.frame()).children(spawned).len(), 7);
    assert_eq!(w.physical().len(), 1 + 4 + 3 * n::STACK_BOXES);
    let moved = at(w, the_box) - settled;
    assert!(moved.0[2] < -0.5, "the throw carried the box away: {moved:?}");
    // Member 1's entries arrive late, after member 0's have been stepped far past them: a rollback, the same roots.
    let mut late = Source::default();
    ea.iter().cloned().for_each(|e| late.add(e));
    let mut b = replica_of(world.clone());
    b.advance(Frame(300), &mut late);
    eb.iter().cloned().for_each(|e| late.add(e));
    let adv = b.advance(Frame(TO), &mut late);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 14), "rolled back: {adv:?}");
    assert_eq!(roots(&b, TO), want, "the late replica agrees");
    assert_eq!(b.faults(), 0);
    // A replay: a fresh replica with every entry from the start, stepped in one go.
    let mut replay = replica_of(world);
    let mut all = Source::default();
    eb.iter().chain(&ea).cloned().for_each(|e| all.add(e));
    replay.advance(Frame(TO), &mut all);
    assert_eq!(roots(&replay, TO), want, "live = replay");
}
