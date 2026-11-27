//! M1's ball pit and admission (world-model.md 2.4): @coordinator-opus's conditions, with objects
//! found by name, never by id.

use bytes::Bytes;
use pe_core::{Frame, MemberKey, Offset, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Outcome};
use pe_world::components::{Body, Hand, Shape, SimSize, body_kind, shape_kind};
use pe_world::preset::{
    BALLS, MAX_BALLS, Preset, Session, ball_pit, ball_pit_on, ball_pit_with, find, genesis_line, m1_line, m1_session,
    names,
};
use pe_world::{BlobStore, HandPose, Input, World, WorldError, perm};

fn origin() -> Pos {
    Pos::ORIGIN.checked_add(Offset([3.0, 0.0, -2.0])).unwrap()
}

fn pit(seed: u64) -> World {
    ball_pit(seed, BlobStore::default(), origin())
}

#[test]
fn the_same_arguments_give_the_same_root() {
    let (a, b) = (pit(7), pit(7));
    assert_eq!(a.root(), b.root());
    assert_eq!(a.store().state_hash(), b.store().state_hash());
    // The seed moves the balls' jitter and the ids' chain, so another seed is another world.
    assert_ne!(a.root(), pit(8).root());
    assert_ne!(a.root(), ball_pit(7, BlobStore::default(), Pos::ORIGIN).root());
}

#[test]
fn the_pit_has_its_named_objects_and_no_hands() {
    let w = pit(1);
    let root = find(&w, names::ROOT).expect("the pit's root");
    assert_eq!(w.store().parent(root), None);
    for n in [names::FLOOR, names::LAMP].into_iter().chain(names::WALLS) {
        let id = find(&w, n).unwrap_or_else(|| panic!("{n}"));
        assert_eq!(w.store().parent(id), Some(root), "{n}");
    }
    let snap = w.snapshot(Frame(0));
    assert_eq!(snap.children(root).len(), 6 + BALLS);
    for i in [0, BALLS - 1] {
        let ball = find(&w, &format!("{}{i}", names::BALL_PREFIX)).unwrap();
        let body = w.store().get::<Body>(ball).unwrap();
        assert_eq!(body.kind, body_kind::DYNAMIC);
        assert_eq!(body.gravity_scale, 1.0);
        let shape = w.store().get::<Shape>(ball).unwrap();
        assert_eq!(shape.kind, shape_kind::SPHERE);
        // About 18 g, a real ball-pit ball.
        let kg = f64::from(shape.density) * 4.0 / 3.0 * std::f64::consts::PI * f64::from(shape.size[0]).powi(3);
        assert!((0.015..0.02).contains(&kg), "{kg} kg");
    }
    assert!(find(&w, &format!("{}{BALLS}", names::BALL_PREFIX)).is_none());
    assert_eq!(w.store().iter::<Hand>().count(), 0, "content only: hands come from admission");
    let floor = find(&w, names::FLOOR).unwrap();
    assert_eq!(w.store().get::<Body>(floor).unwrap().kind, body_kind::STATIC);
    assert_eq!(ball_pit_with(1, BlobStore::default(), origin(), 3).snapshot(Frame(0)).children(root).len(), 9);
}

#[test]
fn the_pit_is_made_by_an_ordinary_edit_from_world_logic() {
    let w = pit(1);
    let ball = find(&w, "Ball 0").unwrap();
    let info = w.snapshot(Frame(0)).info(ball).unwrap();
    // An edit made it, so it has an edit version, and world logic is its creator.
    assert_ne!(info.version, pe_world::GENESIS);
    assert_eq!(w.store().node(ball).creator, MemberKey::LOGIC);
    // Logic held `EDIT` only while the preset applied.
    assert!(!w.permits(MemberKey::LOGIC, perm::EDIT));
    assert_eq!(w.frame(), Frame(0));
    // Every object of the pit has a simulation size (3.5): root, floor, four walls, lamp and the balls.
    assert_eq!(w.store().iter::<SimSize>().count(), 7 + BALLS);
    // The world's physics origin is at the pit.
    assert_eq!(w.settings().origin, origin());
}

#[test]
fn an_m1_world_carries_the_marker_in_its_root() {
    let w = pit(1);
    assert_eq!(w.m1_marker(), Some(m1_line()));
    assert_ne!(m1_line(), pe_core::LineId::origin(), "the M1 line is not the origin");
    assert_eq!(World::new(1, BlobStore::default()).m1_marker(), None);
}

#[test]
fn admission_gives_a_member_two_hands_it_alone_moves() {
    let keys = TestKeys::new(1, 2);
    let (admin, other) = (keys.key(0), keys.key(1));
    let mut w = pit(1);
    let line = m1_line();
    let hands = w.admit(line, admin, perm::ADMIN | perm::EDIT, Frame(0), origin()).unwrap();
    assert_eq!(hands, [0, 1].map(|s| pe_core::ObjectId::hand(line, admin, s, Frame(0))));
    for (side, h) in hands.iter().enumerate() {
        let row = w.store().get::<Hand>(*h).unwrap();
        assert_eq!((row.owner, row.side), (admin, side as u32));
        assert!(w.store().world_transform(*h).is_some());
    }
    assert!(w.permits(admin, perm::EDIT));
    let snap = w.snapshot(Frame(0));
    assert_eq!(snap.perms(admin), Some(perm::ADMIN | perm::EDIT));
    assert_eq!(snap.perms(other), None);
    assert_eq!(snap.members().collect::<Vec<_>>(), vec![(admin, perm::ADMIN | perm::EDIT)]);
    // Admitted again (a returning member, F3 L3): the same hands, the same permissions, nothing changed.
    let before = w.root();
    assert_eq!(w.admit(line, admin, perm::EDIT, Frame(0), origin()), Ok(hands));
    assert_eq!((w.root(), w.snapshot(Frame(0)).perms(admin)), (before, Some(perm::ADMIN | perm::EDIT)));
    let mut w2 = pit(1);
    w2.admit(line, admin, perm::ADMIN | perm::EDIT, Frame(0), origin()).unwrap();
    assert_eq!(w2.root(), before, "admission is deterministic");

    // The admin's hand reaches a ball and grabs it; the other key cannot move the admin's hand.
    let ball = find(&w, "Ball 0").unwrap();
    let to = w.store().world_transform(ball).unwrap().pos;
    let pose = HandPose { pos: to, rot: [0.0, 0.0, 0.0, 1.0], grip: 1.0, engaged: true };
    let mut a = EntryBuilder::new(&keys, 0);
    let mut b = EntryBuilder::new(&keys, 1);
    let mv = Input::Hand { hand: hands[1], poses: vec![pose], hold: 0 };
    let e = b.next(Class::Input, Frame(0), Bytes::from(mv.encode()));
    assert!(matches!(w.apply_input(Frame(0), &e).0, Outcome::Refused { .. }));
    let e = a.next(Class::Input, Frame(0), Bytes::from(mv.encode()));
    assert!(matches!(w.apply_input(Frame(0), &e).0, Outcome::Applied { .. }));
    w.end_frame(Frame(0));
    w.set_frame(Frame(1));
    let g = Input::Grab { object: ball, hand: Some(hands[1]) };
    let e = a.next(Class::Input, Frame(1), Bytes::from(g.encode()));
    let out = w.apply_input(Frame(1), &e).0;
    assert!(matches!(out, Outcome::Applied { .. }), "{out:?}");
    assert_eq!(w.claims().find(|(o, _)| *o == ball).map(|(_, c)| c.member), Some(admin));
}

#[test]
fn the_largest_pit_builds_and_the_count_is_clamped() {
    let w = ball_pit_with(3, BlobStore::default(), origin(), MAX_BALLS);
    let root = find(&w, names::ROOT).unwrap();
    assert_eq!(w.snapshot(Frame(0)).children(root).len(), 6 + MAX_BALLS);
    assert_eq!(ball_pit_with(3, BlobStore::default(), origin(), MAX_BALLS + 100).root(), w.root());
    let one = ball_pit_with(3, BlobStore::default(), origin(), 0);
    assert_eq!(one.snapshot(Frame(0)).children(find(&one, names::ROOT).unwrap()).len(), 7);
}

#[test]
fn an_interim_session_builds_the_pit_on_its_genesis_line() {
    // world-model.md 2.4, interim sessions: the line is the hash of the genesis record without its root.
    let line = genesis_line(b"a genesis record");
    assert_ne!(line, genesis_line(b"another genesis record"), "two sessions never share a line");
    assert_ne!(line, m1_line());
    assert_ne!(line, pe_core::LineId::origin());
    let a = ball_pit_on(line, 1, BlobStore::default(), origin(), BALLS);
    assert_eq!(a.line(), line);
    assert_eq!(a.m1_marker(), Some(m1_line()), "still marked an M1 world");
    assert_eq!(a.root(), ball_pit_on(line, 1, BlobStore::default(), origin(), BALLS).root(), "deterministic");
    // The pit's object ids derive from the line, so another line gives other ids and another root.
    let b = ball_pit_on(genesis_line(b"another genesis record"), 1, BlobStore::default(), origin(), BALLS);
    assert_ne!(find(&a, "Ball 0"), find(&b, "Ball 0"));
    assert_ne!(a.root(), b.root());
    // The M1 default is unchanged: ball_pit_with is ball_pit_on with m1_line().
    assert_eq!(pit(1).root(), ball_pit_on(m1_line(), 1, BlobStore::default(), origin(), BALLS).root());
}

#[test]
#[should_panic(expected = "never on the origin's line")]
fn no_pit_is_built_on_the_origins_line() {
    ball_pit_on(pe_core::LineId::origin(), 1, BlobStore::default(), origin(), BALLS);
}

/// M1's session genesis (the two-member interim, @coordinator-opus): the same `Session` gives the same
/// frame-0 root on every machine, which a second machine checks against its genesis record before it starts. The
/// constant is the root on Windows; the same test cross-built for x86_64-unknown-linux-musl and run under WSL gives it
/// too. The members' order doesn't change it; the line, which a session derives from its record, does (the hands' ids
/// carry it), and so does each setting and each member's permission bits. A key listed twice is refused.
#[test]
fn a_session_s_frame_0_root_is_the_same_on_every_machine() {
    let (opener, joiner) = (MemberKey([2; 32]), MemberKey([1; 32]));
    let session = Session {
        seed: 7,
        origin: Pos::ORIGIN,
        preset: Preset::BallPit { balls: BALLS },
        line: pe_core::LineId(pe_core::Hash([9; 32])),
        rate: pe_seams::SimRate::DEFAULT,
        gravity: pe_world::GRAVITY,
        agree: b"pe-agree's consensus settings".to_vec(),
        members: vec![(opener, perm::ADMIN | perm::EDIT | perm::AGENT), (joiner, perm::EDIT | perm::AGENT)],
    };
    let root = |s: &Session| m1_session(s, BlobStore::default()).expect("a session").root_hash(pe_seams::DomainId(0));
    let r = root(&session);
    // Regenerated when the engine's colour fields were marked (3.6; @coordinator-opus): the marks
    // are in the registry's hashed descriptions; and again when the hand row gained its strength (8.1, the tight hold),
    // which changes both hands' rows and the row's hashed description; and again when the config gained item 8, the
    // blob-shapes byte (6.6, @coordinator-opus); and again when the tables node gained the two per-player tables
    // (logic.md 7.4), empty here.
    assert_eq!(r.to_string(), "24d3057af29493ffc3495169355e4a181ace9d01218a686187ed241d60aef283");
    let w = m1_session(&session, BlobStore::default()).expect("a session");
    assert_eq!((w.line(), w.m1_marker()), (session.line, Some(m1_line())));
    assert_eq!(
        w.snapshot(w.frame()).members().filter(|m| m.0 != MemberKey::LOGIC).collect::<Vec<_>>(),
        vec![(joiner, perm::EDIT | perm::AGENT), (opener, perm::ADMIN | perm::EDIT | perm::AGENT)],
        "each member with its own bits, in key order"
    );
    let mut reordered = session.clone();
    reordered.members.reverse();
    assert_eq!(root(&reordered), r, "members in any order");
    let changed: [fn(&mut Session); 6] = [
        |s| s.line = pe_core::LineId(pe_core::Hash([8; 32])),
        |s| s.seed = 8,
        |s| s.rate = pe_seams::SimRate::new(90).expect("90 Hz"),
        |s| s.gravity = [0.0, -3.0, 0.0],
        |s| s.agree = b"other settings".to_vec(),
        |s| s.members[1].1 |= perm::ADMIN,
    ];
    for (i, change) in changed.iter().enumerate() {
        let mut s = session.clone();
        change(&mut s);
        assert_ne!(root(&s), r, "change {i} is in the root");
    }
    let mut twice = session.clone();
    twice.members.push((joiner, perm::EDIT));
    assert!(matches!(m1_session(&twice, BlobStore::default()), Err(WorldError::Duplicate(_))), "a key listed twice");
}

/// A joiner stands at the lowest slot of genesis's row that no current member's hand takes (2.4; @engine-architect): a function of the world alone, so every replica admits at the same spot.
#[test]
fn a_joiner_stands_at_the_lowest_slot_no_hand_takes() {
    let slot = |i: usize| origin().checked_add(Offset([0.6 * i as f64, 0.0, 1.5])).unwrap();
    let line = m1_line();
    let mut w = pit(1);
    let root = w.root_hash(pe_seams::DomainId(0));
    assert_eq!(w.join_spawn(), slot(0), "no hands: the first slot");
    assert_eq!(w.root_hash(pe_seams::DomainId(0)), root, "asking changes nothing");

    // Hands far along the row take only their own slot.
    let (a, b, c, d) = (MemberKey([1; 32]), MemberKey([2; 32]), MemberKey([3; 32]), MemberKey([4; 32]));
    w.admit(line, a, perm::EDIT, Frame(0), slot(3)).unwrap();
    assert_eq!(w.join_spawn(), slot(0));
    w.admit(line, b, perm::EDIT, Frame(0), w.join_spawn()).unwrap();
    assert_eq!(w.join_spawn(), slot(1), "the slot just given is taken");

    // A member standing between slots 1 and 2 has a hand in each, so both are taken.
    let between = origin().checked_add(Offset([0.9, 0.0, 1.5])).unwrap();
    w.admit(line, c, perm::EDIT, Frame(0), between).unwrap();
    assert_eq!(w.join_spawn(), slot(4), "slots 0-3 are taken");

    // A leave frees the leaver's slot once its hands are gone.
    w.leave(b, pe_core::EntryRef(pe_core::Hash([0x1e; 32]))).unwrap();
    assert_eq!(w.join_spawn(), slot(0));
    w.admit(line, d, perm::EDIT, Frame(5), w.join_spawn()).unwrap();
    assert_eq!(w.join_spawn(), slot(4));
}

/// F3's n = 2 branch (@diverge-eng's design v5, L3; world-model.md 2.3 and 8). A continues the world on a new line:
/// `switch_line` moves only the config record's line id, so the root changes and every object keeps its id and its
/// leaf. B, a member from the old line, is admitted on the new one at a later frame: it keeps its hands, what they
/// hold and its permissions, and nothing is spawned, so the root is unchanged. A key that is a member without hands
/// gets two, with this admission's ids, and keeps its permissions.
#[test]
fn a_branch_keeps_every_object_and_a_returning_member_keeps_its_hands() {
    let keys = TestKeys::new(1, 3);
    let (a, b, agent) = (keys.key(0), keys.key(1), keys.key(2));
    let line = m1_line();
    let mut w = pit(1);
    w.admit(line, a, perm::ADMIN | perm::EDIT, Frame(0), origin()).unwrap();
    let b_hands = w.admit(line, b, perm::EDIT, Frame(0), origin()).unwrap();
    w.set_member(agent, perm::AGENT);
    let objects = |w: &World| w.store().objects().to_vec();
    let tree_root = |w: &World| w.root_hash(pe_seams::DomainId(0));
    let (before, old_objects) = (tree_root(&w), objects(&w));

    let fresh = pe_core::LineId(pe_core::Hash([7; 32]));
    w.switch_line(fresh);
    assert_eq!(w.line(), fresh);
    assert_ne!(tree_root(&w), before, "the root hashes the line");
    assert_eq!(objects(&w), old_objects, "every object keeps its id");
    let branched = tree_root(&w);
    let mut back = w.clone();
    back.switch_line(line);
    assert_eq!(tree_root(&back), before, "only the line moved");

    // B comes back on the new line at frame 600: its hands from the old line, its permissions, nothing new.
    assert_eq!(w.admit(fresh, b, perm::ADMIN | perm::EDIT, Frame(600), origin()), Ok(b_hands));
    assert_eq!(tree_root(&w), branched, "a returning member's admission changes nothing");
    assert_eq!(w.snapshot(Frame(600)).perms(b), Some(perm::EDIT), "and grants nothing");
    assert_eq!(w.store().iter::<Hand>().filter(|(_, h)| h.owner == b).count(), 2);

    // A member without hands gets them, with this admission's ids, and keeps its permissions.
    let made = w.admit(fresh, agent, perm::EDIT, Frame(600), origin()).unwrap();
    assert_eq!(made, [0, 1].map(|s| pe_core::ObjectId::hand(fresh, agent, s, Frame(600))));
    assert_eq!(w.snapshot(Frame(600)).perms(agent), Some(perm::AGENT));
}
