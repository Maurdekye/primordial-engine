//! Player bodies on the real replica (world-model.md 8.2; the friend test's locomotion, L3): pe-replica's drive walks
//! each member's body with pe-physics's mover once a frame, before the hands, and the view's rig and heads follow the
//! bodies. On the plane, which gives each member a body at its feet with its hands as children.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::mock::MockClock;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    Class, DomainId, EntrySource, FinalCut, Frame, Hash, HostTime, LineId, MemberKey, ObjectId, Offset, Pos, Replica,
    SignedEntry, ViewFlags,
};
use pe_sim::Transform;
use pe_world::components::{Body, Shape, body_kind, shape_kind};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::preset::{Preset, Session, m1_session};
use pe_world::{BlobStore, BodyHandPose, GENESIS, Input, MoveSample, World, perm};

const LINE: LineId = LineId(Hash([7; 32]));
const K: u64 = 8;
const CLOCK: MockClock = MockClock { rate: pe_seams::SimRate::DEFAULT, start: HostTime(1_000_000_000) };
const HEAD: [f32; 3] = [0.0, 1.6, 0.0];

fn keys() -> TestKeys {
    TestKeys::new(3, 2)
}

fn member(i: usize) -> MemberKey {
    keys().key(i)
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

fn replica_of(world: World, local: Option<MemberKey>) -> ReplicaImpl<PeWorld> {
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(CLOCK));
    config.cut_every = K;
    config.local = local;
    ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).expect("a replica")
}

/// Agreement's stand-in: entries by frame, in chain and seq order, whatever order they arrive in.
#[derive(Default)]
struct Source {
    entries: BTreeMap<Frame, Vec<SignedEntry>>,
    changed: Option<Frame>,
}

impl Source {
    /// Agreement voids `e`: it was stepped, and is no longer in the log.
    fn void(&mut self, e: &SignedEntry) {
        let f = e.entry.stamp;
        if let Some(at) = self.entries.get_mut(&f) {
            at.retain(|x| x.id() != e.id());
        }
        self.changed = Some(self.changed.map_or(f, |c| c.min(f)));
    }

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

fn input(b: &mut EntryBuilder, at: u64, i: &Input) -> SignedEntry {
    b.next(Class::Input, Frame(at), Bytes::from(i.encode()))
}

fn sample(stick: [i8; 2], heading: u32, jumps: u32, head: [f32; 3]) -> MoveSample {
    MoveSample { stick, stick_yaw: 0, heading, jumps, head_pos: head, head_rot: [0.0, 0.0, 0.0, 1.0], tracked: true }
}

/// A Move of one sample, held `hold` frames more.
fn walk(body: ObjectId, stick: [i8; 2], jumps: u32, hold: u8) -> Input {
    Input::Move { body, samples: vec![sample(stick, 0, jumps, HEAD)], hold }
}

fn body_of(w: &World, i: usize) -> ObjectId {
    w.body_of(member(i)).expect("the plane gives each member a body")
}

fn feet(w: &World, body: ObjectId) -> [f64; 3] {
    (w.store().world_transform(body).expect("placed").pos - Pos::ORIGIN).0
}

fn place(w: &World, id: ObjectId) -> [f64; 3] {
    (w.store().world_transform(id).expect("placed").pos - Pos::ORIGIN).0
}

/// A box `half` in half-extents at `at`, of `kind`, in genesis (an engine-made object, as a preset's).
fn add_box(w: &mut World, id: u128, at: [f64; 3], half: [f32; 3], kind: u32, density: f32) -> ObjectId {
    let id = ObjectId::from_u128(id);
    w.create(id, None, MemberKey::LOGIC, GENESIS).expect("a box");
    w.insert(id, Transform::at(Pos::ORIGIN.checked_add(Offset(at)).unwrap())).expect("its place");
    w.insert(id, Body { kind, friction: 0.6, gravity_scale: 1.0, may_sleep: 1, grabbable: 1, ..Body::default() })
        .expect("its body");
    w.insert(id, Shape { kind: shape_kind::BOX, density, size: half, ..Shape::default() }).expect("its shape");
    id
}

/// Full stick forward for 60 frames walks the body about 4 m along -Z on the plane, level, with no faults; its
/// hands go with it, so an engaged hand's body stays at its place in the body, within a centimetre.
#[test]
fn a_body_walks_where_its_stick_points_and_its_hands_go_with_it() {
    let world = plane();
    let body = body_of(&world, 0);
    let start = feet(&world, body);
    let hand = ObjectId::hand(LINE, member(0), 1, Frame(0));
    let offset = [0.3, 1.2, -0.4];
    let pose = BodyHandPose { offset: Offset(offset), rot: [0.0, 0.0, 0.0, 1.0], grip: 1.0, engaged: true };
    let mut b = EntryBuilder::on_line(&keys(), 0, LINE);
    let mut s = Source::default();
    s.add(input(&mut b, 1, &Input::BodyHand { hand, poses: vec![pose], hold: 200 }));
    s.add(input(&mut b, 1, &walk(body, [0, 127], 0, 59)));
    let mut r = replica_of(world, Some(member(0)));
    r.advance(Frame(61), &mut s);
    let w = r.world().world();
    let end = feet(w, body);
    let d = [end[0] - start[0], end[1] - start[1], end[2] - start[2]];
    assert!(d[2] < -3.0 && d[2] > -5.1, "about 4 m forward (-Z): {d:?}");
    assert!(d[0].abs() < 1e-3 && d[1].abs() < 0.01, "straight and level: {d:?}");
    // The hand's physics body is where the moved body puts it, not a frame behind (7 cm at 4 m/s).
    let p = r.physics().pose(hand).expect("an engaged hand has a body").position;
    let want = [end[0] + offset[0], end[1] + offset[1], end[2] + offset[2]];
    let gap = (0..3).map(|i| (p[i] - want[i]).powi(2)).sum::<f64>().sqrt();
    assert!(gap < 0.01, "the hand follows its body: {gap} m off ({p:?} vs {want:?})");
    let hw = place(w, hand);
    assert!((0..3).all(|i| (hw[i] - want[i]).abs() < 1e-6), "the world composes the body and the hand: {hw:?}");
    // After the pack runs out the stick is zero and the body stops.
    r.advance(Frame(150), &mut s);
    let rest = feet(r.world().world(), body);
    r.advance(Frame(170), &mut s);
    let still = feet(r.world().world(), body);
    assert!((0..3).all(|i| (rest[i] - still[i]).abs() < 1e-6), "at rest once the stick is zero: {rest:?} {still:?}");
    assert_eq!(r.faults(), 0);
}

/// A press (the press count rising) jumps once, about 0.9 m, and the body lands where it stood; the first sample a
/// body gets adopts its count without a jump, and a re-sent count never jumps again.
#[test]
fn a_press_jumps_once_and_lands() {
    let world = plane();
    let body = body_of(&world, 0);
    let ground = feet(&world, body)[1];
    let mut b = EntryBuilder::on_line(&keys(), 0, LINE);
    let mut s = Source::default();
    s.add(input(&mut b, 1, &walk(body, [0, 0], 5, 0)));
    s.add(input(&mut b, 30, &walk(body, [0, 0], 6, 0)));
    s.add(input(&mut b, 40, &walk(body, [0, 0], 6, 0)));
    let mut r = replica_of(world, None);
    let mut top = f64::MIN;
    let mut before = f64::MIN;
    for n in 2..=160u64 {
        r.advance(Frame(n), &mut s);
        let y = feet(r.world().world(), body)[1];
        if n <= 30 {
            before = before.max(y);
        }
        top = top.max(y);
    }
    assert!((before - ground).abs() < 0.01, "no jump from the first sample: {before} vs {ground}");
    assert!(top - ground > 0.6 && top - ground < 1.2, "one jump of about 0.9 m: {}", top - ground);
    let end = feet(r.world().world(), body)[1];
    assert!((end - ground).abs() < 0.01, "landed: {end} vs {ground}");
    assert_eq!(r.faults(), 0);
}

/// An obstacle put in a world before the walk, from the walker's feet: its id and where it is.
type Obstacle = dyn Fn(&mut World, [f64; 3]) -> (ObjectId, [f64; 3]);

/// A heavy crate (500 kg, over the mover's 40 kg) blocks the body and is not moved; a light box (4 kg) in its way is
/// shoved ahead of it (user 5c, @coordinator-opus). Two runs, one obstacle each.
#[test]
fn a_heavy_crate_blocks_the_body_and_a_light_box_is_shoved() {
    let walk_into = |obstacle: &Obstacle| {
        let mut world = plane();
        let body = body_of(&world, 0);
        let f0 = feet(&world, body);
        let (id, at) = obstacle(&mut world, f0);
        let mut b = EntryBuilder::on_line(&keys(), 0, LINE);
        let mut s = Source::default();
        s.add(input(&mut b, 1, &walk(body, [0, 127], 0, 90)));
        let mut r = replica_of(world, None);
        r.advance(Frame(100), &mut s);
        assert_eq!(r.faults(), 0);
        let w = r.world().world();
        (feet(w, body), at, place(w, id))
    };
    let heavy = |w: &mut World, f0: [f64; 3]| {
        let at = [f0[0], f0[1] + 0.5, f0[2] - 2.5];
        (add_box(w, 0xC0FFEE, at, [0.5, 0.5, 0.5], body_kind::DYNAMIC, 500.0), at)
    };
    let (end, at, c) = walk_into(&heavy);
    // The capsule's radius is 0.25 m: the feet stop at the crate's near face plus about that.
    let near_face = at[2] + 0.5;
    assert!(end[2] > near_face + 0.2 && end[2] < near_face + 0.4, "blocked: feet z {} vs face {near_face}", end[2]);
    assert!((0..3).all(|i| (c[i] - at[i]).abs() < 0.01), "the crate didn't move: {c:?} from {at:?}");
    let light = |w: &mut World, f0: [f64; 3]| {
        let at = [f0[0], f0[1] + 0.1, f0[2] - 1.0];
        (add_box(w, 0xC0FFEF, at, [0.1, 0.1, 0.1], body_kind::DYNAMIC, 500.0), at)
    };
    let (end, at, l) = walk_into(&light);
    assert!(end[2] < at[2] - 1.0, "the body walked on past where the box was: {end:?}");
    assert!(l[2] < at[2] - 0.2, "the light box was shoved ahead: {l:?} from {at:?}");
}

/// The user's 6(b): a head that walks into a wall moves the body only up to the wall, and the view's rig is pushed
/// back so the tracked head stays over the feet, out of the wall.
#[test]
fn a_head_walking_into_a_wall_pushes_the_body_and_the_view_out() {
    let mut world = plane();
    let body = body_of(&world, 0);
    let f0 = feet(&world, body);
    let wall_z = f0[2] - 0.6;
    add_box(&mut world, 0xBA11, [f0[0], f0[1] + 1.0, wall_z - 0.1], [2.0, 1.0, 0.1], body_kind::STATIC, 1000.0);
    let mut b = EntryBuilder::on_line(&keys(), 0, LINE);
    let mut s = Source::default();
    // The head walks 1 m forward in the room over 1 s (16 samples a pack, 4 packs), then stays.
    let mut k = 0u32;
    for pack in 0..4u64 {
        let samples = (0..16)
            .map(|_| {
                k += 1;
                let z = -(f64::from(k.min(60)) / 60.0) as f32;
                sample([0, 0], 0, 0, [0.0, 1.6, z])
            })
            .collect();
        s.add(input(&mut b, 1 + 16 * pack, &Input::Move { body, samples, hold: if pack == 3 { 100 } else { 0 } }));
    }
    let mut r = replica_of(world, Some(member(0)));
    r.advance(Frame(120), &mut s);
    let end = feet(r.world().world(), body);
    assert!(end[2] > wall_z + 0.2 && end[2] < wall_z + 0.3, "the body stops at the wall: {} vs {wall_z}", end[2]);
    let v = r.view();
    let rig = v.local_rig;
    // The head is 1 m forward in the room; the rig is pushed back so that it stands over the feet.
    let head_world = rig.pos.checked_add(Offset([0.0, 1.6, -1.0])).unwrap();
    let off = (head_world - Pos::ORIGIN).0;
    assert!((off[0] - end[0]).abs() < 1e-6 && (off[2] - end[2]).abs() < 1e-6, "head over the feet: {off:?} {end:?}");
    assert!(off[2] > wall_z, "the view is out of the wall");
    assert_eq!(r.faults(), 0);
}

/// Two members walking, one jumping and turning: a replica that hears one member's moves late rolls back and steps
/// the same roots at every cut as one that heard them in time.
#[test]
fn late_moves_roll_back_to_the_same_roots() {
    const TO: u64 = 200;
    let world = plane();
    let (b0, b1) = (body_of(&world, 0), body_of(&world, 1));
    let mut e0 = EntryBuilder::on_line(&keys(), 0, LINE);
    let mut e1 = EntryBuilder::on_line(&keys(), 1, LINE);
    let a: Vec<SignedEntry> = vec![
        input(&mut e0, 3, &walk(b0, [40, 127], 0, 40)),
        input(&mut e0, 50, &Input::Move { body: b0, samples: vec![sample([0, 90], 1 << 29, 1, HEAD)], hold: 30 }),
    ];
    let c: Vec<SignedEntry> =
        vec![input(&mut e1, 5, &walk(b1, [-127, 0], 2, 30)), input(&mut e1, 60, &walk(b1, [0, -127], 3, 50))];
    let mut on_time = Source::default();
    a.iter().chain(&c).cloned().for_each(|e| on_time.add(e));
    let mut one = replica_of(world.clone(), None);
    one.advance(Frame(TO + 1), &mut on_time);
    let mut late = Source::default();
    a.iter().cloned().for_each(|e| late.add(e));
    let mut two = replica_of(world, None);
    two.advance(Frame(100), &mut late);
    c.iter().cloned().for_each(|e| late.add(e));
    two.advance(Frame(TO + 1), &mut late);
    assert_eq!(roots(&two, TO), roots(&one, TO));
    let (w1, w2) = (one.world().world(), two.world().world());
    assert_eq!(feet(w1, b0), feet(w2, b0));
    assert_eq!(feet(w1, b1), feet(w2, b1));
    assert!(feet(w1, b1) != feet(&plane(), b1), "member 1 moved");
    assert_eq!((one.faults(), two.faults()), (0, 0));
}

/// A member's leave removes its body from the world and its player from the physics; the other's stays.
#[test]
fn a_leave_removes_the_leaver_s_player() {
    let world = plane();
    let (b0, b1) = (body_of(&world, 0), body_of(&world, 1));
    let mut e1 = EntryBuilder::on_line(&keys(), 1, LINE);
    let mut s = Source::default();
    s.add(input(&mut e1, 2, &walk(b1, [0, 127], 0, 10)));
    s.add(e1.next(Class::Protocol, Frame(12), Bytes::from_static(pe_seams::LEAVE)));
    let mut r = replica_of(world, None);
    r.advance(Frame(10), &mut s);
    assert!(r.physics().is_player(b0) && r.physics().is_player(b1));
    r.advance(Frame(20), &mut s);
    assert!(r.world().world().body_of(member(1)).is_none(), "the leaver's body is gone");
    assert!(!r.physics().has_body(b1), "and its player");
    assert!(r.physics().is_player(b0), "the other's stays");
    assert_eq!(r.faults(), 0);
}

/// S5 v1.12: the view's rig follows the local member's body (the head over the feet, turned by the heading), one
/// head per body in member order with `own` on the local member's, and OWN_BODY only on the local member's body.
#[test]
fn the_view_s_rig_follows_my_body_and_carries_every_head() {
    let world = plane();
    let (b0, b1) = (body_of(&world, 0), body_of(&world, 1));
    let mut e0 = EntryBuilder::on_line(&keys(), 0, LINE);
    let mut e1 = EntryBuilder::on_line(&keys(), 1, LINE);
    let turned = 1u32 << 30; // a quarter turn left: forward is -X
    let head = [0.2, 1.5, -0.3];
    let mut s = Source::default();
    s.add(input(&mut e0, 1, &Input::Move { body: b0, samples: vec![sample([0, 127], turned, 0, head)], hold: 30 }));
    s.add(input(&mut e1, 1, &walk(b1, [0, 0], 0, 0)));
    let mut r = replica_of(world, Some(member(0)));
    r.advance(Frame(31), &mut s);
    let w = r.world().world();
    let f = feet(w, b0);
    assert!(f[0] < -1.0, "walked toward -X after the quarter turn: {f:?}");
    let v = r.view();
    let rig = v.local_rig;
    // The rig turned by the heading, so the head's rig-frame pose lands over the feet.
    let (c, sn) = pe_replica::players::turn(turned);
    let h = [c * f64::from(head[0]) + sn * f64::from(head[2]), -sn * f64::from(head[0]) + c * f64::from(head[2])];
    let at = (rig.pos.checked_add(Offset([h[0], 1.5, h[1]])).unwrap() - Pos::ORIGIN).0;
    assert!((at[0] - f[0]).abs() < 1e-5 && (at[2] - f[2]).abs() < 1e-5, "head over the feet: {at:?} vs {f:?}");
    assert_eq!(rig.rotation, pe_replica::players::heading_rotation(turned));
    let mut members = [member(0), member(1)];
    members.sort();
    assert_eq!(v.heads.iter().map(|h| h.member).collect::<Vec<_>>(), members, "one head per body, in member order");
    for hd in &v.heads {
        assert_eq!(hd.own, hd.member == member(0));
        let body = if hd.member == member(0) { b0 } else { b1 };
        let fb = feet(w, body);
        let hp = (hd.pos - Pos::ORIGIN).0;
        assert!((hp[0] - fb[0]).abs() < 1e-9 && (hp[2] - fb[2]).abs() < 1e-9, "a head over its feet");
    }
    for o in &v.objects {
        assert_eq!(o.flags.contains(ViewFlags::OWN_BODY), o.body_of == Some(member(0)), "{:?}", o.id);
        assert!(o.hand_of.is_none() || o.body_of.is_none(), "a hand is never a body part");
    }
    // Without a local member the rig stays at the origin.
    let mut none = replica_of(plane(), None);
    none.advance(Frame(4), &mut Source::default());
    assert_eq!(none.view().local_rig.pos, Pos::ORIGIN);
}

fn row<T: pe_sim::Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

/// An edit creating a heavy crate (a 1 m cube of 500 kg) centred at `at`.
fn crate_edit(b: &mut EntryBuilder, f: u64, at: [f64; 3]) -> SignedEntry {
    let rows = vec![
        row(Transform::at(Pos::ORIGIN.checked_add(Offset(at)).unwrap())),
        row(Body {
            kind: body_kind::DYNAMIC,
            friction: 0.6,
            gravity_scale: 1.0,
            may_sleep: 1,
            grabbable: 1,
            ..Body::default()
        }),
        row(Shape { kind: shape_kind::BOX, density: 500.0, size: [0.5, 0.5, 0.5], ..Shape::default() }),
    ];
    let t = Transaction {
        inline: vec![],
        actor: Actor::Person,
        label: None,
        preconditions: vec![],
        ops: vec![Op::Create { new: 0, parent: Target::Root, index: None, rows }],
        blobs: vec![],
    };
    b.next(Class::Edit, Frame(f), Bytes::from(t.encode()))
}

/// The other machine's edit puts a heavy crate where I stand, and I hear of it late: my replica rolls back, the mover
/// pushes my body out of the crate (6(b)), both machines agree, and my view's rig moves by the push at once, with no
/// glide (user 1a: a correction snaps).
#[test]
fn a_late_crate_where_i_stand_pushes_me_out_and_my_view_snaps() {
    let world = plane();
    let me = body_of(&world, 0);
    let f0 = feet(&world, me);
    let mut eb = EntryBuilder::on_line(&keys(), 1, LINE);
    // Its near face 0.1 m past my feet: my capsule (0.25 m) is inside it.
    let crate_at = [f0[0] + 0.6, f0[1] + 0.5, f0[2]];
    let edit = crate_edit(&mut eb, 30, crate_at);
    let mut on_time = Source::default();
    on_time.add(edit.clone());
    let mut two = replica_of(world.clone(), None);
    two.advance(Frame(120), &mut on_time);
    let mut late = Source::default();
    let mut one = replica_of(world, Some(member(0)));
    one.advance(Frame(90), &mut late);
    let before = one.view().local_rig.pos;
    assert_eq!(feet(one.world().world(), me), f0, "nothing moved me yet");
    late.add(edit);
    one.advance(Frame(91), &mut late);
    let after = one.view().local_rig.pos;
    let pushed = (after - before).0;
    assert!(pushed[0] < -0.08, "my rig moved with the push, at once: {pushed:?}");
    one.advance(Frame(120), &mut late);
    assert_eq!(roots(&one, 112), roots(&two, 112), "both machines agree");
    let end = feet(one.world().world(), me);
    assert!(crate_at[0] - 0.5 - end[0] > 0.2, "out of the crate: feet {end:?}, crate at {crate_at:?}");
    assert_eq!(end, feet(two.world().world(), me));
    assert_eq!((one.faults(), two.faults()), (0, 0));
}

/// @fable-reviewer's edge case: my own Move stepped and then voided. The replica steps again without it, so my body
/// is where the machine that never had the Move has it, and the next Move heals the heading.
#[test]
fn a_voided_move_rolls_my_walk_back_to_the_other_machine_s() {
    let world = plane();
    let me = body_of(&world, 0);
    let mut b = EntryBuilder::on_line(&keys(), 0, LINE);
    let voided =
        input(&mut b, 5, &Input::Move { body: me, samples: vec![sample([0, 127], 1 << 29, 0, HEAD)], hold: 20 });
    let next = input(&mut b, 40, &Input::Move { body: me, samples: vec![sample([0, 0], 1 << 30, 0, HEAD)], hold: 0 });
    let mut s = Source::default();
    s.add(voided.clone());
    s.add(next.clone());
    let mut one = replica_of(world.clone(), Some(member(0)));
    one.advance(Frame(30), &mut s);
    assert!(feet(one.world().world(), me) != feet(&world, me), "the Move walked me");
    s.void(&voided);
    one.advance(Frame(60), &mut s);
    let mut other = Source::default();
    other.add(next);
    let mut two = replica_of(world.clone(), None);
    two.advance(Frame(60), &mut other);
    assert_eq!(feet(one.world().world(), me), feet(two.world().world(), me), "back where the other has me");
    assert_eq!(feet(one.world().world(), me), feet(&world, me), "which is where I stood");
    assert_eq!(roots(&one, 56), roots(&two, 56));
    let heading = |r: &ReplicaImpl<PeWorld>| r.view().heads.iter().find(|h| h.body == me).map(|h| h.rotation);
    assert_eq!(heading(&one), heading(&two), "the next Move's heading on both");
}

/// A recentre (@design-reviewer's drive-side guard): the head jumps 2 m in the rig frame in one sample. The body takes
/// no room step and stays where it stood; the view's rig moves instead, so the tracked head is still over the feet.
#[test]
fn a_recentre_s_head_jump_moves_the_rig_not_the_body() {
    let world = plane();
    let me = body_of(&world, 0);
    let f0 = feet(&world, me);
    let mut b = EntryBuilder::on_line(&keys(), 0, LINE);
    let mut s = Source::default();
    s.add(input(&mut b, 1, &Input::Move { body: me, samples: vec![sample([0, 0], 0, 0, HEAD)], hold: 0 }));
    let jumped = [2.0, 1.6, 0.0];
    s.add(input(&mut b, 20, &Input::Move { body: me, samples: vec![sample([0, 0], 0, 0, jumped)], hold: 40 }));
    let mut r = replica_of(world, Some(member(0)));
    r.advance(Frame(19), &mut s);
    let rig0 = r.view().local_rig.pos;
    r.advance(Frame(60), &mut s);
    let end = feet(r.world().world(), me);
    assert!((0..3).all(|i| (end[i] - f0[i]).abs() < 1e-6), "the body stayed: {end:?} from {f0:?}");
    let moved = (r.view().local_rig.pos - rig0).0;
    assert!((moved[0] + 2.0).abs() < 1e-6 && moved[2].abs() < 1e-6, "the rig took the jump: {moved:?}");
    assert_eq!(r.faults(), 0);
}
