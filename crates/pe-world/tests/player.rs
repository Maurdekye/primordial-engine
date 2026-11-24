//! Player bodies (world-model.md 8.2; friend-test locomotion, piece L2): the body at admission, leave and rejoin,
//! the `Move` input, `BodyHand` poses in the body's frame, and their packs in the root and in an import.

use bytes::Bytes;
use pe_core::{EntryRef, Frame, Hash, LineId, MemberKey, ObjectId, Offset, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Cut, CutImport, CutRef, DomainId, LeafKey, Outcome, PhysicsImage, Reason, SignedEntry, Tree};
use pe_world::components::{Hand, Player};
use pe_world::preset::{Preset, Session, m1_session};
use pe_world::tree::table;
use pe_world::{BlobStore, BodyHandPose, Footprint, HandPose, Input, MoveSample, World, perm};

const LINE: LineId = LineId(pe_core::Hash([7; 32]));
const D: DomainId = DomainId(1);

fn keys() -> TestKeys {
    TestKeys::new(3, 2)
}

fn session(preset: Preset) -> Session {
    let keys = keys();
    Session {
        seed: 5,
        origin: Pos::ORIGIN,
        preset,
        line: LINE,
        rate: pe_seams::SimRate::DEFAULT,
        gravity: pe_world::GRAVITY,
        agree: b"pe-agree's settings".to_vec(),
        members: vec![(keys.key(0), perm::ADMIN | perm::EDIT), (keys.key(1), perm::EDIT)],
    }
}

fn plane() -> World {
    m1_session(&session(Preset::Plane), BlobStore::default()).expect("the plane")
}

fn pit() -> World {
    m1_session(&session(Preset::BallPit { balls: 8 }), BlobStore::default()).expect("the pit")
}

fn input(w: &mut World, f: u64, b: &mut EntryBuilder, i: &Input) -> (Outcome, Footprint) {
    let e: SignedEntry = b.next(Class::Input, Frame(f), Bytes::from(i.encode()));
    w.apply_input(Frame(f), &e)
}

fn applied(o: &Outcome) -> bool {
    matches!(o, Outcome::Applied { .. })
}

fn reason(o: &Outcome) -> Reason {
    match o {
        Outcome::Refused { reason, .. } => reason.clone(),
        other => panic!("not refused: {other:?}"),
    }
}

fn sample(stick: [i8; 2], jumps: u32, head: [f32; 3]) -> MoveSample {
    MoveSample {
        stick,
        stick_yaw: 1 << 30,
        heading: 7,
        jumps,
        head_pos: head,
        head_rot: [0.0, 0.0, 0.0, 1.0],
        tracked: true,
    }
}

fn player(w: &World, body: ObjectId) -> Player {
    *w.store().get::<Player>(body).expect("a Player row")
}

fn hands_of(w: &World, member: MemberKey) -> Vec<ObjectId> {
    let mut v: Vec<ObjectId> = w.store().iter::<Hand>().filter(|(_, h)| h.owner == member).map(|(id, _)| id).collect();
    v.sort_by_key(|&id| w.store().get::<Hand>(id).map(|h| h.side));
    v
}

/// `id`'s local position, checked to be in `parent`'s frame.
fn offset(w: &World, id: ObjectId, parent: ObjectId) -> [f64; 3] {
    assert_eq!(w.store().parent(id), Some(parent));
    w.store().get::<pe_sim::Transform>(id).expect("a transform").pos
}

#[test]
fn the_plane_gives_each_member_a_body_at_its_feet_with_its_hands_as_children() {
    let keys = keys();
    let w = plane();
    for i in 0..2 {
        let k = keys.key(i);
        let body = w.body_of(k).expect("a body");
        assert_eq!(player(&w, body).owner, k);
        assert_eq!(w.store().parent(body), None, "the body is a root");
        let hands = hands_of(&w, k);
        assert_eq!(hands.len(), 2);
        for (side, &h) in hands.iter().enumerate() {
            assert_eq!(w.store().parent(h), Some(body), "the hands are the body's children");
            let x = if side == 0 { -0.25 } else { 0.25 };
            assert_eq!(offset(&w, h, body), [x, 1.2, 0.3]);
        }
    }
    assert_eq!(w.players().len(), 2);
    assert!(w.players().windows(2).all(|p| p[0].body < p[1].body), "in id order");
    // The pit has no bodies: root hands, no Player in its registry, so its roots are M1's.
    let p = pit();
    assert_eq!(p.body_of(keys.key(0)), None);
    assert!(p.players().is_empty());
    assert!(p.store().registry().get(pe_world::components::ids::PLAYER).is_none());
    assert!(hands_of(&p, keys.key(0)).iter().all(|&h| p.store().parent(h).is_none()));
}

#[test]
fn a_move_and_a_body_hand_round_trip_and_their_codecs_are_bounded() {
    let body = ObjectId::from_u128(9);
    let m = Input::Move { body, samples: vec![sample([127, -127], 3, [0.1, 1.6, -0.2]); 3], hold: 4 };
    let bytes = m.encode();
    assert_eq!(Input::decode(&bytes), Some(m.clone()));
    let mut long = bytes.clone();
    long.push(0);
    assert_eq!(Input::decode(&long), None, "a trailing byte");
    assert_eq!(Input::decode(&Input::Move { body, samples: vec![], hold: 0 }.encode()), None, "no samples");
    let many = Input::Move { body, samples: vec![sample([0, 0], 0, [0.0; 3]); 17], hold: 0 };
    assert_eq!(Input::decode(&many.encode()), None, "more than 16 samples");
    let pose = BodyHandPose { offset: Offset([0.3, 1.1, -0.4]), rot: [0.0, 0.0, 0.0, 1.0], grip: 0.5, engaged: true };
    let bh = Input::BodyHand { hand: body, poses: vec![pose; 2], hold: 1 };
    assert_eq!(Input::decode(&bh.encode()), Some(bh));
    assert_eq!(MoveSample::quantise_stick([1.0, 0.0]), [127, 0]);
    assert_eq!(MoveSample::quantise_stick([-1.0, 0.0]), [-127, 0], "symmetric, never -128");
    assert_eq!(MoveSample::quantise_stick([1.0, 1.0]), [90, 90], "the length is clamped to 1");
    assert_eq!(MoveSample::quantise_stick([f32::NAN, -5.0]), [0, -127]);
}

#[test]
fn only_the_owner_moves_a_body_and_a_bad_sample_is_refused() {
    let keys = keys();
    let mut w = plane();
    let (mut b0, mut b1) = (EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 1));
    let body = w.body_of(keys.key(0)).unwrap();
    let m = Input::Move { body, samples: vec![sample([40, 0], 0, [0.0, 1.6, 0.0])], hold: 0 };
    let before = w.root_hash(D);
    let (o, _) = input(&mut w, 1, &mut b1, &m);
    assert_eq!(reason(&o), Reason::NotYourHand, "another member's body");
    assert_eq!(w.root_hash(D), before, "a refused Move changes nothing");
    let bad = Input::Move { body, samples: vec![sample([i8::MIN, 0], 0, [0.0; 3])], hold: 0 };
    assert!(matches!(reason(&input(&mut w, 1, &mut b0, &bad).0), Reason::Invalid { .. }));
    let far = Input::Move { body, samples: vec![sample([0, 0], 0, [0.0, 99.0, 0.0])], hold: 0 };
    assert!(matches!(reason(&input(&mut w, 1, &mut b0, &far).0), Reason::Invalid { .. }));
    let (o, fp) = input(&mut w, 1, &mut b0, &m);
    assert!(applied(&o), "{o:?}");
    assert_eq!(fp.players, [body]);
    assert_eq!(player(&w, body).stick, [40, 0]);
    assert_eq!(pe_world::input_phase(&b0.next(Class::Input, Frame(1), Bytes::from(m.encode()))), 0);
}

#[test]
fn a_move_pack_is_read_frame_by_frame_then_held_then_the_stick_stops() {
    let keys = keys();
    let mut w = plane();
    let mut b = EntryBuilder::new(&keys, 0);
    let body = w.body_of(keys.key(0)).unwrap();
    let m = Input::Move { body, samples: vec![sample([10, 0], 0, [0.0; 3]), sample([20, 5], 0, [0.0; 3])], hold: 2 };
    assert!(applied(&input(&mut w, 1, &mut b, &m).0));
    let p = player(&w, body);
    assert_eq!((p.stamp, p.span, p.stick), (1, 4, [10, 0]));
    let mut sticks = Vec::new();
    for f in 1..=5 {
        w.end_frame(Frame(f));
        sticks.push(player(&w, body).stick);
    }
    assert_eq!(sticks, [[20, 5], [20, 5], [20, 5], [0, 0], [0, 0]], "frames 2-6: the second sample, held twice, then none");
    assert_eq!(player(&w, body).heading, 7, "the heading stays after the pack runs out");
}

#[test]
fn a_jump_lasts_one_frame_and_a_first_or_repeated_count_never_jumps() {
    let keys = keys();
    let mut w = plane();
    let mut b = EntryBuilder::new(&keys, 0);
    let body = w.body_of(keys.key(0)).unwrap();
    let mv = |jumps| Input::Move { body, samples: vec![sample([0, 0], jumps, [0.0; 3])], hold: 0 };
    assert!(applied(&input(&mut w, 1, &mut b, &mv(5)).0));
    assert_eq!(player(&w, body).jump, 0, "the first sample adopts the count");
    w.end_frame(Frame(1));
    assert!(applied(&input(&mut w, 2, &mut b, &mv(6)).0));
    assert_eq!(player(&w, body).jump, 1, "a press");
    w.end_frame(Frame(2));
    assert_eq!(player(&w, body).jump, 0, "for one frame");
    assert!(applied(&input(&mut w, 3, &mut b, &mv(6)).0));
    assert_eq!(player(&w, body).jump, 0, "the same count again is no press");
}

#[test]
fn what_the_step_used_last_frame_rolls_forward_for_the_room_scale_step() {
    let keys = keys();
    let mut w = plane();
    let mut b = EntryBuilder::new(&keys, 0);
    let body = w.body_of(keys.key(0)).unwrap();
    let mv = |head, heading| {
        Input::Move { body, samples: vec![MoveSample { heading, ..sample([0, 0], 0, head) }], hold: 9 }
    };
    assert!(applied(&input(&mut w, 1, &mut b, &mv([0.0, 1.6, 0.0], 100)).0));
    let p = player(&w, body);
    assert_eq!((p.prev_head, p.prev_heading), ([0.0, 1.6, 0.0], 100), "no room-scale step at the start");
    w.end_frame(Frame(1));
    assert!(applied(&input(&mut w, 2, &mut b, &mv([0.1, 1.6, 0.0], 200)).0));
    let p = player(&w, body);
    assert_eq!((p.head_pos, p.prev_head, p.heading, p.prev_heading), ([0.1, 1.6, 0.0], [0.0, 1.6, 0.0], 200, 100));
    w.end_frame(Frame(2));
    let p = player(&w, body);
    assert_eq!((p.prev_head, p.prev_heading), ([0.1, 1.6, 0.0], 200), "a held sample is no further step");
}

#[test]
fn body_hand_poses_are_in_the_body_s_frame_and_the_hands_follow_the_body() {
    let keys = keys();
    let mut w = plane();
    let mut b = EntryBuilder::new(&keys, 0);
    let body = w.body_of(keys.key(0)).unwrap();
    let [left, _] = hands_of(&w, keys.key(0))[..] else { panic!("two hands") };
    let pose = |x| BodyHandPose { offset: Offset([x, 1.0, -0.4]), rot: [0.0, 0.0, 0.0, 1.0], grip: 1.0, engaged: true };
    let (o, fp) = input(&mut w, 1, &mut b, &Input::BodyHand { hand: left, poses: vec![pose(0.3), pose(0.4)], hold: 0 });
    assert!(applied(&o), "{o:?}");
    assert_eq!(fp.hands, [left]);
    assert_eq!(offset(&w, left, body), [0.3, 1.0, -0.4]);
    // The body moves (physics writes its pose back): the hand moves with it, at the same offset.
    let moved = Pos::ORIGIN.checked_add(Offset([5.0, 0.0, -3.0])).unwrap();
    w.write_poses(&[(body, moved, [0.0, 0.0, 0.0, 1.0])]);
    assert_eq!(offset(&w, left, body), [0.3, 1.0, -0.4]);
    let world = w.snapshot(w.frame()).offset_in(left, None).unwrap().0;
    assert!(world.iter().zip([5.3, 1.0, -3.4]).all(|(a, b)| (a - b).abs() < 1e-9), "{world:?}");
    w.end_frame(Frame(1));
    assert_eq!(offset(&w, left, body), [0.4, 1.0, -0.4], "the pack's second pose");
    // Another member's, a pose too far from the body, and a root hand are refused.
    let mut b1 = EntryBuilder::new(&keys, 1);
    let one = Input::BodyHand { hand: left, poses: vec![pose(0.3)], hold: 0 };
    assert_eq!(reason(&input(&mut w, 2, &mut b1, &one).0), Reason::NotYourHand);
    let far = Input::BodyHand { hand: left, poses: vec![pose(9.0)], hold: 0 };
    assert!(matches!(reason(&input(&mut w, 2, &mut b, &far).0), Reason::Invalid { .. }));
    let mut p = pit();
    let root_hand = hands_of(&p, keys.key(0))[0];
    let r = Input::BodyHand { hand: root_hand, poses: vec![pose(0.3)], hold: 0 };
    assert!(matches!(reason(&input(&mut p, 1, &mut b, &r).0), Reason::Invalid { .. }), "a root hand has no body");
    // A world-frame `Hand` input on a body's hand still places it in the world (converted to the body's frame), and
    // ends a pending BodyHand pack.
    let at = Pos::ORIGIN.checked_add(Offset([5.0, 1.5, -3.0])).unwrap();
    let hp = HandPose { pos: at, rot: [0.0, 0.0, 0.0, 1.0], grip: 0.0, engaged: false };
    assert!(applied(&input(&mut w, 2, &mut b, &Input::Hand { hand: left, poses: vec![hp], hold: 0 }).0));
    assert_eq!(offset(&w, left, body), [0.0, 1.5, 0.0]);
}

#[test]
fn leaving_removes_the_body_with_its_hands_and_a_rejoin_makes_a_new_one() {
    let keys = keys();
    let mut w = plane();
    let k = keys.key(1);
    let body = w.body_of(k).unwrap();
    let mut b = EntryBuilder::new(&keys, 1);
    let m = Input::Move { body, samples: vec![sample([5, 5], 0, [0.0; 3]); 3], hold: 0 };
    assert!(applied(&input(&mut w, 1, &mut b, &m).0));
    let report = w.leave(k, EntryRef(Hash([0x1e; 32]))).expect("a member");
    assert_eq!(report.bodies, [body]);
    assert_eq!(report.hands.len(), 2);
    assert!(!w.store().exists(body));
    assert_eq!(w.body_of(k), None);
    assert_eq!(w.players().len(), 1);
    w.end_frame(Frame(1));
    let spawn = w.join_spawn();
    w.admit(LINE, k, perm::EDIT, Frame(9), spawn).expect("a rejoin");
    let again = w.body_of(k).expect("a new body");
    assert_ne!(again, body, "the ids include the admission frame");
    assert_eq!(again, ObjectId::body(LINE, k, 0, Frame(9)));
    assert!(hands_of(&w, k).iter().all(|&h| w.store().parent(h) == Some(again)));
}

/// Every object and table of `target`, and its config, as records.
fn full_import(target: &World) -> CutImport {
    let t = target.tree(D);
    let mut keys: Vec<LeafKey> = target.store().objects().iter().map(|&id| LeafKey::Object(id)).collect();
    keys.extend((0..table::COUNT as u8).map(LeafKey::Table));
    keys.push(LeafKey::Config);
    let physics = PhysicsImage { domain: D, frame: target.frame(), global: Hash::ZERO, bytes: Default::default() };
    CutImport {
        blobs: vec![],
        cut: Cut { at: CutRef { line: target.line(), height: 0, frame: target.frame() }, heads: Vec::new() },
        target_root: t.root(),
        records: t.export(&keys),
        absent: Vec::new(),
        physics: Some(physics),
    }
}

#[test]
fn pending_body_packs_are_in_the_root_and_reach_a_joiner_through_an_import() {
    let keys = keys();
    let mut w = plane();
    let mut b = EntryBuilder::new(&keys, 0);
    let body = w.body_of(keys.key(0)).unwrap();
    let left = hands_of(&w, keys.key(0))[0];
    let before = (w.root_hash(D), w.root());
    let m = Input::Move { body, samples: vec![sample([1, 2], 0, [0.0; 3]); 4], hold: 2 };
    assert!(applied(&input(&mut w, 1, &mut b, &m).0));
    let after_move = (w.root_hash(D), w.root());
    assert_ne!(after_move.0, before.0);
    assert_ne!(after_move.1, before.1, "the older state hash covers the packs too");
    let pose = BodyHandPose { offset: Offset([0.1, 1.0, 0.0]), rot: [0.0, 0.0, 0.0, 1.0], grip: 0.0, engaged: false };
    assert!(applied(&input(&mut w, 1, &mut b, &Input::BodyHand { hand: left, poses: vec![pose; 3], hold: 0 }).0));
    assert_ne!(w.root_hash(D), after_move.0);
    let entries = w.table_entries(table::HAND_PACKS);
    assert_eq!(entries.iter().filter(|(k, _)| k.len() == 17).count(), 2, "one Move and one BodyHand pack");
    assert!(entries.windows(2).all(|e| e[0].0 < e[1].0), "in key order");
    // A joiner from a bare world: the registry brings `Player`, and the packs come with the hand-packs table.
    let mut got = World::new(0, w.blobs().clone()).import(D, &full_import(&w)).expect("an import");
    assert_eq!(got.root_hash(D), w.root_hash(D));
    assert_eq!(got.players(), w.players());
    // Both go on reading the same packs.
    w.end_frame(Frame(1));
    got.end_frame(Frame(1));
    assert_eq!(got.root_hash(D), w.root_hash(D));
    assert_eq!(player(&got, body).stick, [1, 2]);
}
