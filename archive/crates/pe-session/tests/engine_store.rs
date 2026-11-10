//! The per-object engine tables on pe-sim (P2-3 S5b): claims and cooldowns roll back with the keyframe,
//! object indices are never reused, and the store survives its canonical serialization.

use pe_session::session::*;
use pe_sim::{SimStore, Snapshot};

fn touch(member: MemberId, seq: u32, prev: EntryId, stamp: u64, obj: u32, grab: bool) -> Entry {
    Entry { member, seq, prev, stamp, payload: Payload::Touch { obj, impulse: [0.0, 0.2, 0.0], grab }, carried: None }
}

fn sim() -> WorldSim {
    WorldSim::new(1, 3, &[1, 2, 3], SceneKind::Pile { side: 4, layers: 1 }, SimConfig::default(), KeyRing::test(3))
}

#[test]
fn claims_and_cooldowns_roll_back_with_the_keyframe() {
    // Member 1 grabs object 5 at frame 10 and pushes object 6 at frame 12; member 2 grabs object 6 at 14.
    let g1 = touch(1, 1, EntryId::NONE, 10, 5, true);
    let p1 = touch(1, 2, g1.id(), 12, 6, false);
    let g2 = touch(2, 1, EntryId::NONE, 14, 6, true);
    // In order: every entry before the frames run.
    let mut a = sim();
    for e in [&g1, &p1, &g2] {
        a.insert(test_sign(1, e.clone()));
    }
    a.advance(45);
    // Late: member 2's grab arrives after frame 30, so frames 14..30 are simulated again.
    let mut b = sim();
    for e in [&g1, &p1] {
        b.insert(test_sign(1, e.clone()));
    }
    b.advance(30);
    assert!(b.claim(6).is_none());
    b.insert(test_sign(1, g2.clone()));
    b.advance(45);
    assert!(b.stats.rollbacks > 0, "the late grab rolls back");
    for s in [&a, &b] {
        // Object 5's claim lapsed at 40: member 1 cools down until 50. Object 6 is member 2's until 44.
        assert!(s.claim(5).is_none());
        assert_eq!(s.store.get::<CooldownRow>(object_id(5)).map(|c| (c.member, c.until)), Some((1, 50)));
        assert!(s.claim(6).is_none() && s.store.get::<CooldownRow>(object_id(6)).is_some_and(|c| c.member == 2));
    }
    assert_eq!(a.store.state_hash(), b.store.state_hash());
    assert_eq!(a.state_hash(), b.state_hash());
    assert_eq!(a.check_hash(), b.check_hash());
}

#[test]
fn object_indices_are_never_reused() {
    let mut s = sim();
    let n = s.obj_slots();
    assert_eq!(s.objects().count() as u32, n);
    assert!(s.objects().all(|(i, o)| o.id == object_id(i as usize) && s.live(i) == Some(o)));
    s.store.destroy(object_id(3)).unwrap();
    assert!(s.live(3).is_none());
    assert_eq!(s.obj_slots(), n, "a destroyed index stays used");
    assert_eq!(s.objects().count() as u32, n - 1);
}

#[test]
fn the_store_survives_its_serialization() {
    let mut s = sim();
    let g1 = touch(1, 1, EntryId::NONE, 10, 5, true);
    s.insert(test_sign(1, g1));
    s.advance(20);
    assert!(s.claim(5).is_some());
    let bytes = s.store.snapshot().to_bytes();
    let snap = Snapshot::from_bytes(&bytes, &session_registry()).unwrap();
    let mut fresh = SimStore::new(session_registry(), 0);
    fresh.restore(&snap).unwrap();
    assert_eq!(fresh.state_hash(), s.store.state_hash());
    assert_eq!(fresh.get::<ClaimRow>(object_id(5)), s.store.get::<ClaimRow>(object_id(5)));
}
