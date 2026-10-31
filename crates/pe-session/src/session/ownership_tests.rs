//! World ownership on the admin layer (design 12.5), from the spike's review round 10: joins read the
//! owners the change's own admin entries leave (OWNY-1); a certificate built on a pending one's head
//! waits for it (OWNY-2), at most MAX_HELD_PER_AUTHOR per author (OWNZ-1); tag 20 is bounded (OWNY-3).

use super::*;
use crate::ownership::{test_key, SignRule};

const GUEST: RoleId = 5;

fn k(n: u32) -> Key {
    test_key(n)
}

/// Member 1 of a session of [1, 2] whose world is owned by test identities `owners` (rule A), where
/// everyone else joins as GUEST.
fn world(owners: &[u32]) -> WorldSim {
    let mut s = WorldSim::new(1, 1, &[1, 2], SceneKind::Empty, SimConfig::default(), KeyRing::test(1));
    s.set_ownership(Cert::genesis_test(owners, SignRule::All, 7));
    s.vcm.join_rules = JoinRules::public(GUEST);
    s
}

/// A change listing admin entry `id` (applied) and member 9's join with `role`.
fn change_with_join(s: &WorldSim, id: EntryId, role: RoleId) -> Change {
    let exp = 1000;
    Change { epoch: s.vcm.epoch, admin: vec![(id, true)], add: vec![9], add_roles: vec![role], add_attests: vec![vec![]], add_reqs: vec![(test_join_sig(9, &s.session(), exp), exp)], boundary: 10, ..Default::default() }
}

#[test]
fn an_owner_leaving_in_the_change_of_its_join_is_not_row_0() {
    let mut s = world(&[1, 9]);
    let cert = s.owner_cert(OwnChange::Leave { key: k(9) }, &[9]).unwrap();
    let id = s.publish_ownership(cert).id();
    assert!(s.vcm.admin_pending.contains(&id));
    // The decided book still lists 9; the change's own Leave removes it before the join.
    assert!(s.owner_keys().contains(&k(9)));
    assert_eq!(s.change_valid(&change_with_join(&s, id, ADMIN)), Some(false));
    assert_eq!(s.change_valid(&change_with_join(&s, id, GUEST)), Some(true));
}

#[test]
fn an_owner_added_in_the_change_of_its_join_is_row_0() {
    let mut s = world(&[1]);
    let cert = s.owner_cert(OwnChange::Add { key: k(9) }, &[1]).unwrap();
    let id = s.publish_ownership(cert).id();
    assert!(!s.owner_keys().contains(&k(9)));
    assert_eq!(s.change_valid(&change_with_join(&s, id, ADMIN)), Some(true));
    assert_eq!(s.change_valid(&change_with_join(&s, id, GUEST)), Some(false));
}

/// Owners [1]: 1 adds 9 (pending), then 1 and 9 add 8 on top of that certificate.
fn chained(fixes_off: u64) -> (WorldSim, EntryId, EntryId) {
    let mut s = world(&[1]);
    s.fixes_off = fixes_off;
    let c1 = s.owner_cert(OwnChange::Add { key: k(9) }, &[1]).unwrap();
    let c2 = Cert { world: c1.world, seq: c1.seq + 1, prev: vec![c1.id()], change: OwnChange::Add { key: k(8) }, sigs: vec![] }.signed_test(&[1, 9]);
    let id1 = s.publish_ownership(c1).id();
    let id2 = s.publish_ownership(c2).id();
    (s, id1, id2)
}

#[test]
fn a_certificate_on_a_pending_head_is_held_until_its_parent_is_decided() {
    let (mut s, id1, id2) = chained(0);
    assert!(s.vcm.admin_pending.contains(&id1));
    assert!(!s.vcm.admin_pending.contains(&id2) && s.vcm.own_held.contains(&id2));
    let c = Change { epoch: s.vcm.epoch, admin: vec![(id1, true)], boundary: 10, ..Default::default() };
    assert_eq!(s.change_valid(&c), Some(true));
    s.apply_change(c, 0, &mut Vec::new());
    assert!(s.owner_keys().contains(&k(9)));
    assert!(s.vcm.admin_pending.contains(&id2) && s.vcm.own_held.is_empty());
    // Its own change then commits it.
    let c = Change { epoch: s.vcm.epoch, admin: vec![(id2, true)], boundary: 20, ..Default::default() };
    assert_eq!(s.change_valid(&c), Some(true));
    s.apply_change(c, 0, &mut Vec::new());
    assert_eq!(s.owner_keys(), BTreeSet::from([k(1), k(8), k(9)]));
}

#[test]
fn without_owny2_a_certificate_on_a_pending_head_is_lost() {
    let (mut s, id1, id2) = chained(fix::OWNY2);
    assert!(!s.vcm.admin_pending.contains(&id2) && s.vcm.own_held.is_empty());
    let c = Change { epoch: s.vcm.epoch, admin: vec![(id1, true)], boundary: 10, ..Default::default() };
    s.apply_change(c, 0, &mut Vec::new());
    assert!(!s.vcm.admin_pending.contains(&id2));
}

#[test]
fn an_author_holds_at_most_max_held_per_author() {
    // Review OWNZ-1: certificates naming parents that never exist are held up to the cap only.
    let mut s = world(&[1]);
    let world = s.vcm.book.as_ref().unwrap().world;
    for n in 0..MAX_HELD_PER_AUTHOR as u8 + 4 {
        let c = Cert { world, seq: 2, prev: vec![Fact([n; 32])], change: OwnChange::Add { key: k(9) }, sigs: vec![] }.signed_test(&[1]);
        s.publish_ownership(c);
    }
    assert_eq!(s.vcm.own_held.len(), MAX_HELD_PER_AUTHOR);
    assert!(s.vcm.admin_pending.is_empty());
}

#[test]
fn ownership_entries_round_trip_and_an_oversized_one_is_refused() {
    let s = world(&[1, 9]);
    let cert = s.owner_cert(OwnChange::Leave { key: k(9) }, &[9]).unwrap();
    let e = test_sign(1, Entry { member: 1, seq: 1, prev: EntryId::NONE, stamp: 5, payload: Payload::Ownership { epoch: 0, cert }, carried: None });
    assert_eq!(decode_msg(&encode_msg(&Msg::Entry(e.clone()))), Some(Msg::Entry(e)));
    // Review OWNY-3: a tag-20 payload claiming more than MAX_CERT_BYTES is never read.
    let mut w = Wr(Vec::new());
    w.u8(1); // Msg::Entry
    w.u32(1); w.u32(1); w.id(EntryId::NONE); w.u64(5); // member, seq, prev, stamp
    w.u8(20); w.u32(0); w.u32(MAX_CERT_BYTES as u32 + 1);
    w.0.extend(std::iter::repeat_n(0u8, MAX_CERT_BYTES + 1));
    w.u8(0); // carried: none
    w.sig(&[0; 64]); // the entry's signature
    assert_eq!(decode_msg(&w.0), None);
}

/// Review OWNP-1 (the reviewer's probe): a Target::Owner rule with Rel::Named(n) tests the joiner, as
/// the spike does (`n == j`), not whether member n holds an owner key.
#[test]
fn a_named_owner_rule_tests_the_joiner() {
    let mut s = world(&[2]);
    assert_eq!(s.key_of(2), Some(k(2)));
    s.vcm.join_rules = JoinRules { rules: vec![JoinRule { target: Target::Owner, all: false, rel: Rel::Named(2), role: Some(ADMIN) }], fallback: Some(GUEST) };
    let exp = 1000;
    let mk = |role| Change { epoch: s.vcm.epoch, add: vec![9], add_roles: vec![role], add_attests: vec![vec![]], add_reqs: vec![(test_join_sig(9, &s.session(), exp), exp)], boundary: 10, ..Default::default() };
    let (ca, cg) = (mk(ADMIN), mk(GUEST));
    // Joiner 9 is not member 2: the fallback, GUEST.
    assert_eq!((s.change_valid(&ca), s.change_valid(&cg)), (Some(false), Some(true)));
}
