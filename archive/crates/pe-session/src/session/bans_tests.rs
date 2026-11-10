//! Bans and kick options (design 12.4; user 2026-11-01, msgs m7c37f5485e6f and m5c4b84d186a8): session
//! bans by the BAN permission, never saved; perma-bans by world owners, saved; both after row 0 and
//! before the join rows; a kick may remove Admin (a rejoin role, checked again at the join, review
//! BN-2) or ban; one key, one member (review BN-3).

use super::*;
use crate::ownership::{test_key, SignRule};
use crate::sign::{join_message, test_signing_key};
use ed25519_dalek::Signer;

const GUEST: RoleId = 5;
const GREETER: RoleId = 6;

fn k(n: u32) -> Key {
    test_key(n)
}

/// Member 1 of a session of [1, 2, 3, 4]: 1-3 Admin, 4 a GUEST (kick only). Joiners get `join`. The
/// world is owned by test identities [1, 7].
fn session(join: RoleId) -> WorldSim {
    let mut s = WorldSim::new(1, 1, &[1, 2, 3, 4], SceneKind::Empty, SimConfig::default(), KeyRing::test(1));
    s.set_ownership(Cert::genesis_test(&[1, 7], SignRule::All, 7));
    s.vcm.role_defs.insert(GUEST, perm::KICK);
    s.vcm.role_defs.insert(GREETER, 0);
    s.vcm.roles.insert(4, GUEST);
    s.vcm.join_rules = JoinRules::public(join);
    s
}

/// Member `j` signs in with test identity `id`'s key (a player back under a new member id, 10.3).
fn alias(s: &mut WorldSim, j: MemberId, id: u32) {
    s.keys.public.add(j, test_signing_key(id).verifying_key());
}

/// A change listing `admin` and member `j`'s join with `role`, its request signed by identity `id`.
fn join_change(s: &WorldSim, admin: Vec<(EntryId, bool)>, j: MemberId, id: u32, role: RoleId) -> Change {
    let exp = 1000;
    let sig = test_signing_key(id).sign(&join_message(&s.session(), j, exp)).to_bytes();
    Change { epoch: s.vcm.epoch, admin, add: vec![j], add_roles: vec![role], add_attests: vec![vec![]], add_reqs: vec![(sig, exp)], boundary: 10, ..Default::default() }
}

fn kick_entry(author: MemberId, target: MemberId, rejoin: Rejoin) -> Entry {
    Entry { member: author, seq: 1, prev: EntryId::NONE, stamp: 5, payload: Payload::Kick { epoch: 0, target, rejoin }, carried: None }
}

fn ban_entry(author: MemberId, key: Key, on: bool, perma: bool) -> Entry {
    Entry { member: author, seq: 1, prev: EntryId::NONE, stamp: 5, payload: Payload::Ban { epoch: 0, key, on, perma }, carried: None }
}

#[test]
fn a_session_ban_refuses_the_key_under_a_new_member_id() {
    let mut s = session(ADMIN);
    alias(&mut s, 13, 9);
    // Control: identity 9 joins as 13.
    assert_eq!(s.change_valid(&join_change(&s, vec![], 13, 9, ADMIN)), Some(true));
    s.vcm.bans.insert(k(9), Restrict::Ban);
    for role in [ADMIN, GUEST] {
        assert_eq!(s.change_valid(&join_change(&s, vec![], 13, 9, role)), Some(false));
    }
    // A new key gets in under Public: what a ban can't stop there (12.4).
    assert_eq!(s.change_valid(&join_change(&s, vec![], 14, 14, ADMIN)), Some(true));
}

#[test]
fn a_perma_ban_refuses_the_key_and_starts_the_next_session() {
    let mut s = session(ADMIN);
    s.set_perma_bans([k(9)].into());
    alias(&mut s, 13, 9);
    assert_eq!(s.change_valid(&join_change(&s, vec![], 13, 9, ADMIN)), Some(false));
    assert!(s.vcm.bans.is_empty(), "a session starts with no session bans");
}

#[test]
fn a_ban_refuses_a_join_by_its_key_in_its_own_change() {
    for perma in [false, true] {
        let mut s = session(ADMIN);
        alias(&mut s, 13, 9);
        let id = s.ban(k(9), true, perma).id();
        assert!(s.vcm.admin_pending.contains(&id), "perma {perma}");
        assert_eq!(s.change_valid(&join_change(&s, vec![(id, true)], 13, 9, ADMIN)), Some(false));
        assert_eq!(s.change_valid(&join_change(&s, vec![(id, true)], 14, 14, ADMIN)), Some(true));
    }
}

#[test]
fn neither_ban_names_a_member_or_an_owner() {
    let mut s = session(ADMIN);
    for perma in [false, true] {
        let (a, b) = (s.ban(k(3), true, perma).id(), s.ban(k(7), true, perma).id());
        assert!(!s.vcm.admin_pending.contains(&a), "a member's key");
        assert!(!s.vcm.admin_pending.contains(&b), "an absent owner's key");
    }
    // Kicking an owner with a ban option is refused even for a co-owner: member 1 owns, and so does 2 here.
    let mut run = s.admin_run();
    run.owners.insert(k(2));
    assert!(run.kick_ok(1, 2, Rejoin::Free));
    assert!(!run.kick_ok(1, 2, Rejoin::Ban));
    assert!(!run.kick_ok(1, 2, Rejoin::PermaBan));
}

#[test]
fn who_may_ban() {
    let s = session(ADMIN);
    let run = s.admin_run();
    // Admin 2 (not an owner): session bans yes, perma-bans no.
    assert!(run.ban_ok(2, &k(9), true, false));
    assert!(!run.ban_ok(2, &k(9), true, true));
    assert!(run.kick_ok(2, 4, Rejoin::Ban));
    assert!(!run.kick_ok(2, 4, Rejoin::PermaBan));
    // Admin 1 owns the world: both.
    assert!(run.ban_ok(1, &k(9), true, true));
    assert!(run.kick_ok(1, 4, Rejoin::PermaBan));
    // GUEST 4 kicks but holds no BAN.
    let mut run = s.admin_run();
    run.members.insert(5);
    run.roles.insert(5, GREETER);
    run.keys.insert(5, k(5));
    assert!(run.kick_ok(4, 5, Rejoin::Free));
    assert!(!run.kick_ok(4, 5, Rejoin::Ban));
    assert!(!run.ban_ok(4, &k(9), true, false));
    run.defs.insert(GUEST, perm::KICK | perm::BAN);
    assert!(run.kick_ok(4, 5, Rejoin::Ban));
}

#[test]
fn an_owner_without_admin_perma_bans_an_absent_key_but_cannot_kick() {
    let mut s = session(ADMIN);
    s.vcm.roles.insert(1, GREETER);
    let run = s.admin_run();
    assert!(run.ban_ok(1, &k(9), true, true));
    assert!(!run.kick_ok(1, 4, Rejoin::PermaBan));
}

#[test]
fn kick_and_ban_records_the_key_and_removes_the_member() {
    let s = session(ADMIN);
    let mut run = s.admin_run();
    assert!(run.apply(&kick_entry(2, 4, Rejoin::Ban)));
    assert!(!run.members.contains(&4));
    assert_eq!(run.bans.get(&k(4)), Some(&Restrict::Ban));
    assert!(run.perma.is_empty());
    let mut run = s.admin_run();
    assert!(run.apply(&kick_entry(1, 4, Rejoin::PermaBan)));
    assert!(run.perma.contains(&k(4)) && run.bans.is_empty());
}

#[test]
fn kick_and_remove_admin_gives_the_rejoin_role_in_place_of_admin() {
    let mut s = session(ADMIN);
    alias(&mut s, 13, 9);
    s.vcm.bans.insert(k(9), Restrict::Role(GREETER));
    assert_eq!(s.change_valid(&join_change(&s, vec![], 13, 9, ADMIN)), Some(false));
    assert_eq!(s.change_valid(&join_change(&s, vec![], 13, 9, GREETER)), Some(true));
    // A row giving a role that doesn't count as an admin applies unchanged.
    s.vcm.join_rules = JoinRules::public(GUEST);
    assert_eq!(s.change_valid(&join_change(&s, vec![], 13, 9, GUEST)), Some(true));
}

#[test]
fn a_rejoin_role_must_be_defined_and_not_admin_like() {
    let mut s = session(ADMIN);
    s.vcm.role_defs.insert(8, perm::ROLES);
    let run = s.admin_run();
    assert!(run.kick_ok(2, 4, Rejoin::Role(GREETER)));
    assert!(!run.kick_ok(2, 4, Rejoin::Role(ADMIN)));
    assert!(!run.kick_ok(2, 4, Rejoin::Role(99)));
    assert!(!run.kick_ok(2, 4, Rejoin::Role(8)));
}

/// Review BN-2: a rejoin role deleted or redefined to count as an admin refuses the join.
#[test]
fn the_rejoin_role_is_checked_again_at_the_join() {
    let mut s = session(ADMIN);
    alias(&mut s, 13, 9);
    s.vcm.bans.insert(k(9), Restrict::Role(GREETER));
    s.vcm.role_defs.insert(GREETER, perm::ROLES);
    for role in [ADMIN, GREETER] {
        assert_eq!(s.change_valid(&join_change(&s, vec![], 13, 9, role)), Some(false));
    }
    s.vcm.role_defs.remove(&GREETER);
    assert_eq!(s.change_valid(&join_change(&s, vec![], 13, 9, ADMIN)), Some(false));
}

/// Review BN-2: only a grant naming the player lifts its rejoin role; a redefinition doesn't.
#[test]
fn a_grant_lifts_the_rejoin_role_and_a_redefinition_does_not() {
    let mut s = session(ADMIN);
    s.vcm.bans.insert(k(4), Restrict::Role(GUEST));
    let mut run = s.admin_run();
    let redefine = Entry { member: 1, seq: 1, prev: EntryId::NONE, stamp: 5, payload: Payload::DefineRole { epoch: 0, role: GUEST, perms: perm::ROLES }, carried: None };
    assert!(run.apply(&redefine));
    assert_eq!(run.bans.get(&k(4)), Some(&Restrict::Role(GUEST)));
    let grant = Entry { member: 1, seq: 2, prev: EntryId::NONE, stamp: 6, payload: Payload::SetRole { epoch: 0, member: 4, role: ADMIN }, carried: None };
    assert!(run.apply(&grant));
    assert!(!run.bans.contains_key(&k(4)));
}

#[test]
fn the_two_lists_and_their_unbans_are_separate() {
    let mut s = session(ADMIN);
    alias(&mut s, 13, 9);
    s.vcm.bans.insert(k(9), Restrict::Ban);
    s.vcm.perma_bans.insert(k(9));
    let mut run = s.admin_run();
    // A session unban (Admin 2) leaves the perma-ban; only an owner lifts that.
    assert!(run.apply(&ban_entry(2, k(9), false, false)));
    assert!(!run.bans.contains_key(&k(9)) && run.perma.contains(&k(9)));
    assert!(!run.apply(&ban_entry(2, k(9), false, true)));
    assert!(run.apply(&ban_entry(1, k(9), false, true)));
    assert!(run.perma.is_empty());
    // Through the session: the key stays out until both are lifted.
    let id = s.ban(k(9), false, false).id();
    assert_eq!(s.change_valid(&join_change(&s, vec![(id, true)], 13, 9, ADMIN)), Some(false));
    // A lift of a key no list holds is never pending.
    let id = s.ban(k(8), false, false).id();
    assert!(!s.vcm.admin_pending.contains(&id));
}

#[test]
fn a_session_unban_clears_a_rejoin_role() {
    let s = session(ADMIN);
    let mut run = s.admin_run();
    run.bans.insert(k(9), Restrict::Role(GREETER));
    assert!(run.apply(&ban_entry(2, k(9), false, false)));
    assert!(run.bans.is_empty());
}

/// Review BN-3: one key, one member, against the members and among the change's joiners.
#[test]
fn one_key_one_member() {
    let mut s = session(ADMIN);
    alias(&mut s, 13, 2);
    assert_eq!(s.change_valid(&join_change(&s, vec![], 13, 2, ADMIN)), Some(false));
    alias(&mut s, 14, 9);
    alias(&mut s, 15, 9);
    let mut c = join_change(&s, vec![], 14, 9, ADMIN);
    assert_eq!(s.change_valid(&c), Some(true));
    let other = join_change(&s, vec![], 15, 9, ADMIN);
    c.add.push(15);
    c.add_roles.push(ADMIN);
    c.add_attests.push(vec![]);
    c.add_reqs.push(other.add_reqs[0]);
    assert_eq!(s.change_valid(&c), Some(false));
}

#[test]
fn each_list_holds_at_most_max_bans() {
    let s = session(ADMIN);
    let mut run = s.admin_run();
    for i in 0..MAX_BANS as u32 {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&i.to_le_bytes());
        b[31] = 0xEE;
        run.bans.insert(Key(b), Restrict::Ban);
        run.perma.insert(Key(b));
    }
    for perma in [false, true] {
        assert!(!run.ban_ok(1, &k(9), true, perma));
        assert!(!run.kick_ok(1, 4, if perma { Rejoin::PermaBan } else { Rejoin::Ban }));
    }
    // A rejoin role isn't a ban: it still fits.
    assert!(run.kick_ok(1, 4, Rejoin::Role(GREETER)));
}

#[test]
fn kick_options_and_ban_entries_round_trip_on_the_wire() {
    let ps = [
        Payload::Kick { epoch: 3, target: 4, rejoin: Rejoin::Free },
        Payload::Kick { epoch: 3, target: 4, rejoin: Rejoin::Role(GREETER) },
        Payload::Kick { epoch: 3, target: 4, rejoin: Rejoin::Ban },
        Payload::Kick { epoch: 3, target: 4, rejoin: Rejoin::PermaBan },
        Payload::Ban { epoch: 3, key: k(9), on: true, perma: false },
        Payload::Ban { epoch: 3, key: k(9), on: false, perma: true },
    ];
    for p in ps {
        let se = test_sign(1, Entry { member: 1, seq: 1, prev: EntryId::NONE, stamp: 5, payload: p.clone(), carried: None });
        let m = Msg::Entry(se);
        assert_eq!(decode_msg(&encode_msg(&m)), Some(m), "{p:?}");
    }
    // A plain kick keeps tag 17, so its id is the one it had before kick options.
    let plain = Entry { member: 1, seq: 1, prev: EntryId::NONE, stamp: 5, payload: Payload::Kick { epoch: 3, target: 4, rejoin: Rejoin::Free }, carried: None };
    let mut w = Wr(Vec::new());
    w.entry(&plain);
    let mut tail = vec![17u8];
    tail.extend_from_slice(&3u32.to_le_bytes());
    tail.extend_from_slice(&4u32.to_le_bytes());
    assert!(w.0.windows(tail.len()).any(|x| x == tail.as_slice()));
}
