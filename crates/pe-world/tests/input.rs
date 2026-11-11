//! Inputs (world-model.md 8.1): hand packs, grabs, claims and cooldowns, releases and pushes, with the tests the
//! coordinator's rulings (md6eb506d0fce) ask for.

use bytes::Bytes;
use pe_core::{Frame, MemberKey, ObjectId, Offset, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Note, Outcome, Reason, SignedEntry};
use pe_sim::Transform;
use pe_world::components::{Body, Hand, Shape, shape_kind};
use pe_world::input::{MAX_POSES, frames};
use pe_world::{BlobStore, Footprint, HandPose, Input, InputSettings, World, input_phase, perm};

const ID: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

fn oid(n: u128) -> ObjectId {
    ObjectId::from_u128(n)
}

fn at(x: f64, y: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, 0.0])).unwrap()
}

fn pose(x: f64, engaged: bool) -> HandPose {
    HandPose { pos: at(x, 0.0), rot: ID, grip: 0.5, engaged }
}

const HAND_A: u128 = 1;
const HAND_B: u128 = 2;
/// A tray (a root body at x = 0.5) with a cup on it (its own body), and a ball far away.
const TRAY: u128 = 10;
const CUP: u128 = 11;
const FAR: u128 = 12;

struct Fx {
    a: EntryBuilder,
    b: EntryBuilder,
    ka: MemberKey,
    kb: MemberKey,
    w: World,
}

fn sphere(r: f32) -> Shape {
    Shape { kind: shape_kind::SPHERE, size: [r, 0.0, 0.0], ..Shape::default() }
}

fn fx() -> Fx {
    let keys = TestKeys::new(1, 3);
    let (ka, kb) = (keys.key(0), keys.key(1));
    let mut w = World::new(1, BlobStore::default());
    w.set_member(ka, perm::EDIT);
    w.set_member(kb, perm::EDIT);
    for (id, owner) in [(HAND_A, ka), (HAND_B, kb)] {
        w.create(oid(id), None, owner, pe_world::GENESIS).unwrap();
        w.insert(oid(id), Transform::at(at(0.5, 0.0))).unwrap();
        w.insert(oid(id), Hand { owner, ..Hand::default() }).unwrap();
    }
    w.create(oid(TRAY), None, ka, pe_world::GENESIS).unwrap();
    w.insert(oid(TRAY), Transform::at(at(0.5, 0.0))).unwrap();
    w.create(oid(CUP), Some(oid(TRAY)), ka, pe_world::GENESIS).unwrap();
    w.insert(oid(CUP), Transform::local([0.0, 0.1, 0.0])).unwrap();
    w.create(oid(FAR), None, ka, pe_world::GENESIS).unwrap();
    w.insert(oid(FAR), Transform::at(at(10.0, 0.0))).unwrap();
    for id in [TRAY, CUP, FAR] {
        w.insert(oid(id), Body::default()).unwrap();
        w.insert(oid(id), sphere(0.1)).unwrap();
    }
    Fx { a: EntryBuilder::new(&keys, 0), b: EntryBuilder::new(&keys, 1), ka, kb, w }
}

fn entry(b: &mut EntryBuilder, i: &Input) -> SignedEntry {
    b.next(Class::Input, Frame(0), Bytes::from(i.encode()))
}

fn input(w: &mut World, f: u64, b: &mut EntryBuilder, i: Input) -> (Outcome, Footprint) {
    let e = entry(b, &i);
    w.apply_input(Frame(f), &e)
}

fn ok(o: &(Outcome, Footprint)) {
    assert!(matches!(o.0, Outcome::Applied { .. }), "refused: {:?}", o.0);
}

fn refused(o: &(Outcome, Footprint)) -> Reason {
    match &o.0 {
        Outcome::Refused { reason, .. } => reason.clone(),
        a => panic!("applied: {a:?}"),
    }
}

fn grab(object: u128, hand: u128) -> Input {
    Input::Grab { object: oid(object), hand: Some(oid(hand)) }
}

fn hand(h: u128, poses: Vec<HandPose>, hold: u8) -> Input {
    Input::Hand { hand: oid(h), poses, hold }
}

/// Ends frames `from .. to`, as the replica does: `end_frame(f)`, then the frame becomes `f + 1`.
fn run(w: &mut World, from: u64, to: u64) {
    for f in from..to {
        w.end_frame(Frame(f));
        w.set_frame(Frame(f + 1));
    }
}

fn target(w: &World, h: u128) -> Pos {
    w.store().world_transform(oid(h)).unwrap().pos
}

#[test]
fn the_codec_is_strict_and_round_trips() {
    let all = [
        hand(HAND_A, vec![pose(1.0, true), pose(2.0, false)], 3),
        Input::Push { object: oid(TRAY), impulse: [1.0, -2.0, 0.5], at: Some(at(0.5, 0.1)) },
        Input::Push { object: oid(TRAY), impulse: [0.0; 3], at: None },
        grab(TRAY, HAND_A),
        Input::Grab { object: oid(TRAY), hand: None },
        Input::Release { object: oid(CUP) },
    ];
    for i in &all {
        let b = i.encode();
        assert_eq!(Input::decode(&b).as_ref(), Some(i));
        let mut longer = b.clone();
        longer.push(0);
        assert_eq!(Input::decode(&longer), None, "trailing bytes");
        assert_eq!(Input::decode(&b[..b.len() - 1]), None, "truncated");
    }
    assert_eq!(Input::decode(&[4]), None, "unknown tag");
    let mut flag = Input::Grab { object: oid(TRAY), hand: None }.encode();
    *flag.last_mut().unwrap() = 2;
    assert_eq!(Input::decode(&flag), None, "a flag of 2");
    assert_eq!(Input::decode(&hand(HAND_A, vec![], 0).encode()), None, "no poses");
    assert_eq!(Input::decode(&hand(HAND_A, vec![pose(0.0, true); MAX_POSES + 1], 0).encode()), None);
    assert!(Input::decode(&hand(HAND_A, vec![pose(0.0, true); MAX_POSES], 0).encode()).is_some());
    assert_eq!(Input::decode(&Input::Release { object: ObjectId::NONE }.encode()), None, "a zero id");
}

#[test]
fn settings_convert_exactly_and_are_bounded() {
    assert_eq!((frames(500, 60), frames(166, 60)), (30, 10), "pe-session's CLAIM_FRAMES and CLAIM_COOLDOWN");
    assert_eq!((frames(166, 90), frames(166, 72), frames(0, 60)), (15, 12, 0));
    let mut w = fx().w;
    assert_eq!(w.input_settings(), InputSettings::default());
    for bad in [
        InputSettings { claim_ms: 99, ..InputSettings::default() },
        InputSettings { claim_ms: 5_001, ..InputSettings::default() },
        InputSettings { cooldown_ms: 2_001, ..InputSettings::default() },
        InputSettings { push_budget: 1_000_001, ..InputSettings::default() },
    ] {
        assert!(w.set_input_settings(bad).is_err());
    }
    assert_eq!(w.input_settings(), InputSettings::default(), "a refused setting changes nothing");
    assert!(w.set_input_settings(InputSettings { push_budget: 0, cooldown_ms: 0, ..InputSettings::default() }).is_ok());
}

#[test]
fn a_hand_input_writes_its_target_and_row_and_only_its_owner_may_send_it() {
    let mut f = fx();
    let r = input(&mut f.w, 5, &mut f.b, hand(HAND_A, vec![pose(1.0, true)], 0));
    assert_eq!(refused(&r), Reason::NotYourHand);
    let r = input(&mut f.w, 5, &mut f.a, hand(TRAY, vec![pose(1.0, true)], 0));
    assert_eq!(refused(&r), Reason::NotYourHand, "no Hand row");
    let bare = oid(4);
    f.w.create(bare, None, f.ka, pe_world::GENESIS).unwrap();
    f.w.insert(bare, Hand { owner: f.ka, ..Hand::default() }).unwrap();
    let root = f.w.root();
    let r = input(&mut f.w, 5, &mut f.a, hand(4, vec![pose(1.0, true)], 0));
    assert_eq!(refused(&r), Reason::Invalid { field: None }, "a hand with no Transform");
    assert_eq!(f.w.root(), root, "and nothing changed");
    let r = input(&mut f.w, 5, &mut f.a, hand(99, vec![pose(1.0, true)], 0));
    assert_eq!(refused(&r), Reason::Unknown(oid(99)));
    for bad in [
        HandPose { rot: [0.0, 0.0, 0.0, 1.01], ..pose(1.0, true) },
        HandPose { rot: [f32::NAN, 0.0, 0.0, 1.0], ..pose(1.0, true) },
        HandPose { grip: 1.5, ..pose(1.0, true) },
        HandPose { grip: f32::NAN, ..pose(1.0, true) },
    ] {
        let r = input(&mut f.w, 5, &mut f.a, hand(HAND_A, vec![pose(1.0, true), bad], 0));
        assert_eq!(refused(&r), Reason::Invalid { field: None }, "{bad:?}");
    }
    let root = f.w.root();
    let r = input(&mut f.w, 5, &mut f.a, hand(HAND_A, vec![pose(1.0, true), pose(2.0, false)], 3));
    ok(&r);
    assert_eq!(r.1.hands, [oid(HAND_A)]);
    assert_ne!(f.w.root(), root);
    assert_eq!(target(&f.w, HAND_A), at(1.0, 0.0), "pose 0 at the stamp frame");
    let h = *f.w.store().get::<Hand>(oid(HAND_A)).unwrap();
    assert_eq!((h.stamp, h.span, h.has_prev, h.engaged, h.grip), (5, 5, 0, 1, 0.5), "span covers 2 poses + hold 3");
    // A slightly off-unit rotation is accepted and stored normalized.
    let q = [0.0, 0.0, 0.0, 1.0005];
    ok(&input(&mut f.w, 6, &mut f.a, hand(HAND_A, vec![HandPose { rot: q, ..pose(1.0, true) }], 0)));
    assert_eq!(f.w.store().world_transform(oid(HAND_A)).unwrap().rot, ID);
    let h = *f.w.store().get::<Hand>(oid(HAND_A)).unwrap();
    assert_eq!((h.stamp, h.span, h.prev_stamp, h.prev_span, h.has_prev), (6, 1, 5, 5, 1));
}

#[test]
fn a_pack_writes_each_pose_at_its_frame_holds_the_last_and_a_later_input_replaces_it() {
    let mut f = fx();
    let poses = vec![pose(1.0, true), pose(2.0, true), pose(3.0, false)];
    ok(&input(&mut f.w, 10, &mut f.a, hand(HAND_A, poses, 2)));
    assert_eq!(f.w.hand_pack(oid(HAND_A)).map(|(s, p, h)| (s, p.len(), h)), Some((Frame(10), 3, 2)));
    run(&mut f.w, 10, 11);
    assert_eq!(target(&f.w, HAND_A), at(2.0, 0.0), "pose 1 at frame 11");
    run(&mut f.w, 11, 12);
    assert_eq!(target(&f.w, HAND_A), at(3.0, 0.0), "pose 2 at frame 12");
    let h = *f.w.store().get::<Hand>(oid(HAND_A)).unwrap();
    assert_eq!((h.engaged, h.stamp, h.span), (0, 10, 5), "engaged as the pose says; the stamp and span stay");
    run(&mut f.w, 12, 14);
    assert!(f.w.hand_pack(oid(HAND_A)).is_some(), "the last pose holds through frame 14, the pack still pending");
    run(&mut f.w, 14, 15);
    assert!(f.w.hand_pack(oid(HAND_A)).is_none(), "past its coverage the pack is dropped");
    assert_eq!(target(&f.w, HAND_A), at(3.0, 0.0), "and the hand keeps its last pose");

    // A later input replaces the pack from its own stamp.
    ok(&input(&mut f.w, 20, &mut f.a, hand(HAND_A, vec![pose(4.0, true), pose(5.0, true), pose(6.0, true)], 0)));
    run(&mut f.w, 20, 21);
    ok(&input(&mut f.w, 21, &mut f.a, hand(HAND_A, vec![pose(7.0, true), pose(8.0, true)], 0)));
    run(&mut f.w, 21, 22);
    assert_eq!(target(&f.w, HAND_A), at(8.0, 0.0), "the replacing pack's pose 1, not the old pack's pose 2");
}

#[test]
fn grabbing_a_held_objects_ancestor_is_refused_and_its_child_allowed_in_both_orders() {
    // B holds the cup; A grabs the tray under it: refused, naming B.
    let mut f = fx();
    ok(&input(&mut f.w, 1, &mut f.b, grab(CUP, HAND_B)));
    assert_eq!(refused(&input(&mut f.w, 1, &mut f.a, grab(TRAY, HAND_A))), Reason::Held { by: f.kb });
    // A holds the tray; B takes the cup off it: allowed (a cup off a carried tray).
    let mut f = fx();
    ok(&input(&mut f.w, 1, &mut f.a, grab(TRAY, HAND_A)));
    ok(&input(&mut f.w, 1, &mut f.b, grab(CUP, HAND_B)));
    let claims: Vec<_> = f.w.claims().map(|(o, c)| (o, c.member, c.hand)).collect();
    assert_eq!(claims, [(oid(TRAY), f.ka, oid(HAND_A)), (oid(CUP), f.kb, oid(HAND_B))]);
    // The tray's holder still renews, by grab and by an engaged hand input, while B holds the cup.
    ok(&input(&mut f.w, 5, &mut f.a, grab(TRAY, HAND_A)));
    ok(&input(&mut f.w, 6, &mut f.a, hand(HAND_A, vec![pose(0.0, true)], 0)));
    let tray = f.w.claims().find(|(o, _)| *o == oid(TRAY)).unwrap().1;
    assert_eq!((tray.since, tray.until), (Frame(1), Frame(6 + 30)));
}

#[test]
fn a_renewal_that_moves_the_claim_to_another_hand_checks_that_hands_reach() {
    let mut f = fx();
    let second = oid(3);
    f.w.create(second, None, f.ka, pe_world::GENESIS).unwrap();
    f.w.insert(second, Transform::at(at(5.0, 0.0))).unwrap();
    f.w.insert(second, Hand { owner: f.ka, ..Hand::default() }).unwrap();
    ok(&input(&mut f.w, 1, &mut f.a, grab(TRAY, HAND_A)));
    assert_eq!(refused(&input(&mut f.w, 2, &mut f.a, grab(TRAY, 3))), Reason::OutOfReach);
    let c = f.w.claims().next().unwrap().1;
    assert_eq!((c.hand, c.until), (oid(HAND_A), Frame(31)), "a refused renewal changes nothing");
    ok(&input(&mut f.w, 2, &mut f.a, hand(3, vec![pose(0.5, true)], 0)));
    ok(&input(&mut f.w, 3, &mut f.a, grab(TRAY, 3)));
    let c = f.w.claims().next().unwrap().1;
    assert_eq!((c.hand, c.since, c.until), (second, Frame(1), Frame(33)));
}

#[test]
fn of_two_grabs_of_one_object_in_a_frame_the_first_in_order_wins() {
    let mut f = fx();
    ok(&input(&mut f.w, 3, &mut f.a, grab(TRAY, HAND_A)));
    assert_eq!(refused(&input(&mut f.w, 3, &mut f.b, grab(TRAY, HAND_B))), Reason::Held { by: f.ka });
    let mut f = fx();
    ok(&input(&mut f.w, 3, &mut f.b, grab(TRAY, HAND_B)));
    assert_eq!(refused(&input(&mut f.w, 3, &mut f.a, grab(TRAY, HAND_A))), Reason::Held { by: f.kb });
}

#[test]
fn a_grab_needs_the_authors_hand_a_body_and_reach() {
    let mut f = fx();
    assert_eq!(refused(&input(&mut f.w, 1, &mut f.a, grab(TRAY, HAND_B))), Reason::NotYourHand);
    assert_eq!(refused(&input(&mut f.w, 1, &mut f.a, grab(FAR, HAND_A))), Reason::OutOfReach);
    assert_eq!(refused(&input(&mut f.w, 1, &mut f.a, grab(HAND_B, HAND_A))), Reason::Invalid { field: None });
    // Reach is the hand's 0.1 m plus 0.05 m slack plus the object's 0.1 m radius, from the hand's target.
    ok(&input(&mut f.w, 1, &mut f.a, hand(HAND_A, vec![pose(9.76, true)], 0)));
    ok(&input(&mut f.w, 1, &mut f.a, grab(FAR, HAND_A)));
    ok(&input(&mut f.w, 1, &mut f.b, hand(HAND_B, vec![pose(0.24, true)], 0)));
    assert_eq!(refused(&input(&mut f.w, 1, &mut f.b, grab(TRAY, HAND_B))), Reason::OutOfReach);
    // An agent's grab has no hand and no reach check.
    ok(&input(&mut f.w, 1, &mut f.b, Input::Grab { object: oid(TRAY), hand: None }));
    assert_eq!(f.w.claims().find(|(o, _)| *o == oid(TRAY)).unwrap().1.hand, ObjectId::NONE);
}

#[test]
fn implicit_renewal_survives_one_void_renewal() {
    let mut f = fx();
    ok(&input(&mut f.w, 0, &mut f.a, grab(TRAY, HAND_A)));
    run(&mut f.w, 0, 10);
    // A void renewal (refused: a bad grip) renews nothing...
    let bad = HandPose { grip: 2.0, ..pose(0.0, true) };
    assert!(matches!(input(&mut f.w, 10, &mut f.a, hand(HAND_A, vec![bad], 0)).0, Outcome::Refused { .. }));
    run(&mut f.w, 10, 20);
    // ...and a good one within the claim keeps the grip.
    ok(&input(&mut f.w, 20, &mut f.a, hand(HAND_A, vec![pose(0.0, true)], 0)));
    run(&mut f.w, 20, 49);
    assert_eq!(f.w.claims().count(), 1, "renewed to frame 50");
    // A disengaged input does not renew.
    ok(&input(&mut f.w, 49, &mut f.a, hand(HAND_A, vec![pose(0.0, false)], 0)));
    run(&mut f.w, 49, 50);
    assert_eq!(f.w.claims().count(), 0, "expired at frame 50");
    assert_eq!(f.w.cooldown(f.ka, oid(TRAY)), Some(Frame(60)));
}

#[test]
fn a_disconnected_holders_claim_expires_and_frees_the_subtree_whatever_its_pack_holds() {
    let mut f = fx();
    ok(&input(&mut f.w, 0, &mut f.a, grab(CUP, HAND_A)));
    // A long hold, engaged: written by end_frame every frame, but those poses never renew.
    ok(&input(&mut f.w, 0, &mut f.a, hand(HAND_A, vec![pose(0.5, true)], 255)));
    run(&mut f.w, 0, 29);
    assert_eq!(refused(&input(&mut f.w, 29, &mut f.b, grab(TRAY, HAND_B))), Reason::Held { by: f.ka });
    run(&mut f.w, 29, 30);
    assert_eq!(f.w.claims().count(), 0, "expired at 30");
    ok(&input(&mut f.w, 30, &mut f.b, grab(TRAY, HAND_B)));
    // A's cooldown on the cup runs to frame 40 (10 frames); a release by B frees the tray at once, with no cooldown.
    assert_eq!(f.w.cooldown(f.ka, oid(CUP)), Some(Frame(40)));
    assert_eq!(refused(&input(&mut f.w, 31, &mut f.a, grab(CUP, HAND_A))), Reason::Cooldown { until: Frame(40) });
    run(&mut f.w, 30, 40);
    assert_eq!(f.w.cooldown(f.ka, oid(CUP)), None, "cooldowns end");
    ok(&input(&mut f.w, 40, &mut f.b, Input::Release { object: oid(TRAY) }));
    assert_eq!(f.w.cooldown(f.kb, oid(TRAY)), None);
    ok(&input(&mut f.w, 40, &mut f.a, grab(TRAY, HAND_A)));
}

#[test]
fn a_release_without_a_claim_notes_no_effect() {
    let mut f = fx();
    ok(&input(&mut f.w, 1, &mut f.a, grab(TRAY, HAND_A)));
    let r = input(&mut f.w, 1, &mut f.b, Input::Release { object: oid(TRAY) });
    assert!(matches!(&r.0, Outcome::Applied { notes, .. } if notes == &[Note::NoEffect { op: 0 }]));
    assert_eq!(f.w.claims().count(), 1, "B cannot release A's claim");
}

#[test]
fn pushes_spend_a_budget_per_member_per_second() {
    let mut f = fx();
    let push = |x: f32| Input::Push { object: oid(TRAY), impulse: [x, 0.0, 0.0], at: None };
    let r = input(&mut f.w, 0, &mut f.b, push(1.5));
    ok(&r);
    assert_eq!(r.1.pushes.len(), 1);
    assert_eq!((r.1.pushes[0].author, r.1.pushes[0].impulse), (f.kb, [1.5, 0.0, 0.0]), "recorded with its author");
    assert_eq!(refused(&input(&mut f.w, 10, &mut f.b, push(-0.6))), Reason::Invalid { field: None }, "2.1 > 2.0");
    ok(&input(&mut f.w, 10, &mut f.b, push(-0.5)));
    ok(&input(&mut f.w, 10, &mut f.a, push(1.0)));
    run(&mut f.w, 0, 59);
    assert!(matches!(input(&mut f.w, 59, &mut f.b, push(0.1)).0, Outcome::Refused { .. }), "1.5 still in (-1, 59]");
    run(&mut f.w, 59, 60);
    ok(&input(&mut f.w, 60, &mut f.b, push(1.5)));
    assert!(matches!(input(&mut f.w, 60, &mut f.b, push(f32::INFINITY)).0, Outcome::Refused { .. }));

    // A push on an object someone else holds is refused, on that object only.
    ok(&input(&mut f.w, 61, &mut f.a, grab(CUP, HAND_A)));
    assert_eq!(
        refused(&input(&mut f.w, 61, &mut f.b, Input::Push { object: oid(CUP), impulse: [0.1, 0.0, 0.0], at: None })),
        Reason::Held { by: f.ka }
    );
    ok(&input(&mut f.w, 61, &mut f.a, Input::Push { object: oid(CUP), impulse: [0.1, 0.0, 0.0], at: None }));

    // push_budget = 0 turns remote push off.
    let mut f = fx();
    f.w.set_input_settings(InputSettings { push_budget: 0, ..InputSettings::default() }).unwrap();
    assert!(matches!(input(&mut f.w, 0, &mut f.b, push(0.0)).0, Outcome::Refused { .. }));
}

#[test]
fn a_pushs_point_must_lie_within_the_objects_bound() {
    let mut f = fx();
    let push = |p: Pos| Input::Push { object: oid(TRAY), impulse: [0.1, 0.0, 0.0], at: Some(p) };
    // The tray's bound is its own sphere (0.1; the cup is a body of its own), plus 0.05 slack.
    ok(&input(&mut f.w, 0, &mut f.b, push(at(0.5, 0.14))));
    assert_eq!(refused(&input(&mut f.w, 0, &mut f.b, push(at(0.5, 0.16)))), Reason::Invalid { field: None });
    assert_eq!(refused(&input(&mut f.w, 0, &mut f.b, push(at(1000.0, 0.0)))), Reason::Invalid { field: None });
}

#[test]
fn equal_inputs_give_equal_roots_and_a_refusal_changes_nothing() {
    let mut f = fx();
    let mut g = fx();
    for w in [&mut f, &mut g] {
        ok(&input(&mut w.w, 0, &mut w.a, hand(HAND_A, vec![pose(0.5, true), pose(0.6, true)], 4)));
        ok(&input(&mut w.w, 0, &mut w.a, grab(TRAY, HAND_A)));
        ok(&input(&mut w.w, 0, &mut w.b, Input::Push { object: oid(FAR), impulse: [0.0, 1.0, 0.0], at: None }));
        run(&mut w.w, 0, 3);
    }
    assert_eq!(f.w.root(), g.w.root());
    let root = f.w.root();
    for i in [
        grab(TRAY, HAND_B),
        Input::Grab { object: oid(99), hand: None },
        Input::Push { object: oid(TRAY), impulse: [9.0; 3], at: None },
    ] {
        let r = input(&mut f.w, 3, &mut f.b, i);
        assert!(matches!(r.0, Outcome::Refused { .. }));
        assert_eq!(r.1, Footprint::default());
        assert_eq!(f.w.root(), root);
    }
    // A wrong class, or bytes that do not decode, are Malformed.
    let e = f.a.next(Class::Edit, Frame(0), Bytes::from(grab(TRAY, HAND_A).encode()));
    assert_eq!(refused(&f.w.apply_input(Frame(3), &e)), Reason::Malformed);
    let e = f.a.next(Class::Input, Frame(0), Bytes::from_static(&[9]));
    assert_eq!(refused(&f.w.apply_input(Frame(3), &e)), Reason::Malformed);
}

#[test]
fn hand_inputs_sort_before_the_frames_other_entries() {
    let mut f = fx();
    let h = entry(&mut f.a, &hand(HAND_A, vec![pose(0.0, true)], 0));
    let g = entry(&mut f.a, &grab(TRAY, HAND_A));
    let e = f.a.next(Class::Edit, Frame(0), Bytes::from_static(&[0]));
    assert_eq!((input_phase(&h), input_phase(&g), input_phase(&e)), (0, 1, 1));
}
