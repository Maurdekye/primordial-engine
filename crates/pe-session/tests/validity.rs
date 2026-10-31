//! Validity rules for touches (P2-3 S4): D72's grabs-only claims and HO-1's carried-object rule,
//! checked at a replica that receives the entries.

use pe_session::session::*;

fn touch(member: MemberId, seq: u32, prev: EntryId, stamp: u64, obj: u32, grab: bool, carried: Option<(u32, BodyState)>) -> Entry {
    Entry { member, seq, prev, stamp, payload: Payload::Touch { obj, impulse: [0.0, 0.2, 0.0], grab }, carried }
}

fn outcome(s: &WorldSim, e: &Entry) -> Option<Outcome> {
    s.outcomes.get(&e.id()).map(|(_, o)| *o)
}

#[test]
fn only_grabs_claim_and_only_grabs_are_refused() {
    let members = [1, 2, 3];
    let mut s = WorldSim::new(1, 3, &members, SceneKind::Pile { side: 4, layers: 1 }, SimConfig::default(), KeyRing::test(3));
    // Member 1 grabs object 5 at frame 10; member 2 pushes it at 12 and grabs it at 14 (held).
    let g1 = touch(1, 1, EntryId::NONE, 10, 5, true, None);
    let p2 = touch(2, 1, EntryId::NONE, 12, 5, false, None);
    let g2 = touch(2, 2, p2.id(), 14, 5, true, None);
    // After the claim lapses (10 + CLAIM_FRAMES), member 2's grab at 50 is accepted.
    let g2b = touch(2, 3, g2.id(), 50, 5, true, None);
    for e in [&g1, &p2, &g2, &g2b] {
        s.insert(test_sign(1, e.clone()));
    }
    s.advance(60);
    assert_eq!(outcome(&s, &g1), Some(Outcome::Accepted));
    assert_eq!(outcome(&s, &p2), Some(Outcome::Accepted), "a push on a held object applies");
    assert_eq!(outcome(&s, &g2), Some(Outcome::RejectedClaimed { by: 1, until: 40 }));
    assert_eq!(outcome(&s, &g2b), Some(Outcome::Accepted));
}

#[test]
fn a_touch_carries_only_its_own_object() {
    let members = [1, 2];
    let mut s = WorldSim::new(1, 2, &members, SceneKind::Pile { side: 4, layers: 1 }, SimConfig::default(), KeyRing::test(2));
    let st = s.obj_state(6).unwrap();
    let bad = touch(1, 1, EntryId::NONE, 10, 5, false, Some((6, st)));
    let good = touch(1, 2, bad.id(), 12, 6, false, Some((6, st)));
    for e in [&bad, &good] {
        s.insert(test_sign(1, e.clone()));
    }
    s.advance(20);
    assert_eq!(outcome(&s, &bad), Some(Outcome::RejectedInvalid));
    assert_eq!(outcome(&s, &good), Some(Outcome::Accepted));
}
