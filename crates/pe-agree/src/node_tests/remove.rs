//! `Remove` (S3 v1.13; agreement.md 8): an admin takes another member out at the next epoch (M2 S3,
//! @item:primordial-engine/m2-s1-s3-agreement-at-3-8-players-open-internet-h). The user's automatic removal after 30 s
//! of silence builds on it: silence.rs.

use super::*;
use pe_seams::{Remove, RemoveReason};

/// Member `i` signs a `Remove` of member `j`'s key for `reason`.
fn remove(s: &mut Sim, i: usize, j: usize, reason: RemoveReason) -> EntryRef {
    let present = s.clock.frame_at(HostTime(s.now)).unwrap();
    let payload = Remove { key: s.keys.key(j), reason }.encode();
    let e = s.builders[i].next_unsigned(Class::Protocol, present, payload);
    let e = s.nodes[i].submit(e).expect("own Remove refused");
    s.submitted[i].push(e.id());
    s.submitted_at[i].push(present);
    e.id()
}

fn keys_of(s: &Sim, who: &[usize]) -> Vec<MemberKey> {
    let mut v: Vec<MemberKey> = who.iter().map(|&i| s.keys.key(i)).collect();
    v.sort();
    v
}

fn lowest_final(s: &Sim, who: &[usize]) -> Frame {
    who.iter().map(|&i| s.final_frame(i)).min().unwrap()
}

fn faults(s: &Sim, i: usize) -> Vec<&AgreeEvent> {
    s.events[i].iter().filter(|e| matches!(e, AgreeEvent::Fault(_))).collect()
}

#[test]
fn an_admin_removes_a_crashed_member_at_4_and_the_three_go_on() {
    for seed in [401u64, 402] {
        let mut s = Sim::new(4, seed);
        s.down_from[3] = 1_000;
        s.run(2_000);
        let r = remove(&mut s, 0, 3, RemoveReason::Admin);
        let live = [0, 1, 2];
        for &i in &live {
            s.run_to_epoch(i, 1, 3_000);
        }
        for &i in &live {
            assert_eq!(s.nodes[i].members(), keys_of(&s, &live), "seed {seed}: member {i}'s members");
            assert_eq!(s.nodes[i].epoch(), 1, "seed {seed}: member {i}'s epoch");
            assert!(s.voids(i).iter().all(|v| v.0 != r), "seed {seed}: the Remove voided at {i}");
        }
        let at = s.now / MS;
        s.run(at + 3_000);
        let fin = lowest_final(&s, &live);
        let behind_ms = (s.clock.frame_at(HostTime(s.now)).unwrap().0 - fin.0) * 1000 / 60;
        eprintln!("seed {seed}: in epoch 1 at {at} ms; 3 s on the three are final {behind_ms} ms behind");
        assert!(behind_ms < 700, "seed {seed}: the three didn't go on: {behind_ms} ms behind");
        for &i in &live {
            assert!(faults(&s, i).is_empty(), "seed {seed}: faults at {i}: {:?}", faults(&s, i));
        }
        let all: Vec<usize> = live.to_vec();
        assert!(scale_same_final_log(&s, &all) > 100);
    }
}

/// The final logs of `who` agree below the lowest of their final frames; how many entries that is.
fn scale_same_final_log(s: &Sim, who: &[usize]) -> usize {
    let low = lowest_final(s, who);
    let ids = |i: usize, f: u64| s.nodes[i].entries_at(Frame(f)).map(|v| v.iter().map(|e| e.id()).collect::<Vec<_>>());
    let mut count = 0;
    for f in 0..low.0 {
        for &i in &who[1..] {
            assert_eq!(ids(who[0], f), ids(i, f), "members {} and {i} differ at frame {f}", who[0]);
        }
        count += ids(who[0], f).map_or(0, |v| v.len());
    }
    count
}

#[test]
fn two_members_down_at_7_are_removed_in_one_cut_and_the_five_go_on() {
    let mut s = Sim::new(7, 707);
    s.down_from[5] = 1_000;
    s.down_from[6] = 1_000;
    s.run(2_000);
    remove(&mut s, 0, 5, RemoveReason::Admin);
    remove(&mut s, 1, 6, RemoveReason::Admin);
    let live = [0, 1, 2, 3, 4];
    for &i in &live {
        s.run_to_epoch(i, 1, 3_000);
    }
    for &i in &live {
        assert_eq!(s.nodes[i].members(), keys_of(&s, &live), "member {i}'s members");
    }
    let at = s.now / MS;
    s.run(at + 3_000);
    let fin = lowest_final(&s, &live);
    let behind_ms = (s.clock.frame_at(HostTime(s.now)).unwrap().0 - fin.0) * 1000 / 60;
    eprintln!("7 -> 5 in one cut: in epoch 1 at {at} ms; 3 s on final {behind_ms} ms behind");
    assert!(behind_ms < 700, "the five didn't go on: {behind_ms} ms behind");
    scale_same_final_log(&s, &live);
}

#[test]
fn removes_that_may_not_or_change_nothing_are_void_and_the_removed_members_later_entries_too() {
    // n = 5: member 4 isn't an admin. In one epoch: 4 removes 1 (not an admin), 0 removes itself, 0 removes a key that
    // was never a member, 0 votes 3 silent (one vote against a live member: changes nothing, at most stale),
    // 0 removes 2 (takes effect), 1 removes 2 again (no change). Member 2 keeps submitting; what it signs after the removal is void as AfterLeave.
    // Key 5 is never a member: a joiner that is never started.
    let keys = TestKeys::new(55, 6);
    let mut s = Sim::build(6, 5, 55, |_, c| {
        c.genesis_gov = pe_seams::GovState::genesis(
            (0..5).map(|i| (keys.key(i), if i == 4 { 3 } else { crate::node::ROLE_ADMIN })),
        );
    });
    s.run(1_500);
    let by_non_admin = remove(&mut s, 4, 1, RemoveReason::Admin);
    let own = remove(&mut s, 0, 0, RemoveReason::Admin);
    let stranger = remove(&mut s, 0, 5, RemoveReason::Admin);
    let silent = remove(&mut s, 0, 3, RemoveReason::Silent);
    let first = remove(&mut s, 0, 2, RemoveReason::Admin);
    s.run(1_520);
    let again = remove(&mut s, 1, 2, RemoveReason::Admin);
    let live = [0, 1, 3, 4];
    for &i in &live {
        s.run_to_epoch(i, 1, 4_000);
    }
    let t = s.now / MS;
    s.run(t + 2_000);
    for &i in &live {
        assert_eq!(s.nodes[i].members(), keys_of(&s, &live), "member {i}'s members");
        let v: Vec<(EntryRef, VoidReason)> = s.voids(i).iter().map(|v| (v.0, v.1)).collect();
        assert!(v.contains(&(by_non_admin, VoidReason::NotAuthorized)), "member {i}: the non-admin's Remove: {v:?}");
        assert!(v.contains(&(own, VoidReason::NotAuthorized)), "member {i}: a Remove of its own key: {v:?}");
        assert!(v.contains(&(stranger, VoidReason::NoChange)), "member {i}: a Remove of a non-member: {v:?}");
        // Counted, or stale (`NoChange`) if an entry of 3 stamped after it is final by its cut: never anything else.
        assert!(
            v.iter().all(|x| x.0 != silent || x.1 == VoidReason::NoChange),
            "member {i}: the one silence vote voided otherwise: {v:?}"
        );
        assert!(v.contains(&(again, VoidReason::NoChange)), "member {i}: the second Remove of 2: {v:?}");
        assert!(!v.iter().any(|x| x.0 == first), "member {i}: the Remove of 2 voided: {v:?}");
        let after_leave = v.iter().filter(|x| x.1 == VoidReason::AfterLeave).count();
        assert!(after_leave > 0, "member {i}: none of member 2's later entries void: {v:?}");
        let others: Vec<_> = v
            .iter()
            .filter(|x| ![by_non_admin, own, stranger, silent, again].contains(&x.0) && x.1 != VoidReason::AfterLeave)
            .collect();
        assert!(others.is_empty(), "member {i}: other voids: {others:?}");
    }
    scale_same_final_log(&s, &live);
}

#[test]
fn at_2_each_removing_the_other_leaves_exactly_one() {
    for seed in [21u64, 22, 23] {
        let mut s = Sim::new(2, seed);
        s.run(1_500);
        let a = remove(&mut s, 0, 1, RemoveReason::Admin);
        let b = remove(&mut s, 1, 0, RemoveReason::Admin);
        s.run_to_epoch(0, 1, 3_000);
        s.run_to_epoch(1, 1, 3_000);
        let m0 = s.nodes[0].members();
        assert_eq!(m0, s.nodes[1].members(), "seed {seed}: the two disagree on the members");
        assert_eq!(m0.len(), 1, "seed {seed}: members {m0:?}");
        let kept = if m0[0] == s.keys.key(0) { 0 } else { 1 };
        let (effective, voided) = if kept == 0 { (a, b) } else { (b, a) };
        let v: Vec<(EntryRef, VoidReason)> = s.voids(kept).iter().map(|v| (v.0, v.1)).collect();
        // The other is void as AfterLeave (its author departs first in the final log's order) or as LastMember (it
        // sorts before the effective one's place, and would leave no member).
        assert!(
            v.contains(&(voided, VoidReason::AfterLeave)) || v.contains(&(voided, VoidReason::LastMember)),
            "seed {seed}: the other Remove: {v:?}"
        );
        assert!(!v.iter().any(|x| x.0 == effective), "seed {seed}: the effective Remove voided: {v:?}");
        // The one left finalizes alone (quorum(1) = 1).
        let t = s.now / MS;
        let before = s.final_frame(kept);
        s.run(t + 2_000);
        assert!(s.final_frame(kept).0 > before.0 + 60, "seed {seed}: the one left stopped");
    }
}

#[test]
fn a_live_member_removed_by_an_admin_is_told_refuses_to_sign_and_settles_as_a_leaver_does() {
    // @chunks-reviewer's Medium on b5343f29: n = 4, member 0 removes member 3, which keeps running. Once its E is final
    // there, member 3 sees itself out of the members, its `submit` refuses as after a Leave, and `leave_settled` comes
    // true once the remaining members say E is final; the three go on.
    for seed in [431u64, 432] {
        let mut s = Sim::new(4, seed);
        s.run(1_500);
        remove(&mut s, 0, 3, RemoveReason::Admin);
        for i in 0..4 {
            s.run_to_epoch(i, 1, 3_000);
        }
        let kept = [0, 1, 2];
        assert_eq!(s.nodes[3].members(), keys_of(&s, &kept), "seed {seed}: the removed member's view of the members");
        // The sim keeps offering member 3's entries every 50 ms; once out, none is signed.
        let signed = s.submitted[3].len();
        let t = s.now / MS;
        s.run(t + 1_000);
        assert_eq!(s.submitted[3].len(), signed, "seed {seed}: the removed member signed again");
        let t = s.now / MS;
        s.run(t + 2_000);
        assert!(Agreement::leave_settled(&s.nodes[3]), "seed {seed}: the removed member's leave never settled");
        let fin = lowest_final(&s, &kept);
        let behind_ms = (s.clock.frame_at(HostTime(s.now)).unwrap().0 - fin.0) * 1000 / 60;
        assert!(behind_ms < 700, "seed {seed}: the three didn't go on: {behind_ms} ms behind");
        for &i in &kept {
            assert!(faults(&s, i).is_empty(), "seed {seed}: faults at {i}: {:?}", faults(&s, i));
        }
    }
}
