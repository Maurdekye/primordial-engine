//! Silent members (S3 v1.14; agreement.md 8, "Silent members"; M2 S3,
//! @item:primordial-engine/m2-s1-s3-agreement-at-3-8-players-open-internet-h): a member that hears nothing new signed
//! by k for `silence_s` signs `Remove{k, Silent}`; the vote that brings the epoch's tally to quorum(n) takes k out, and
//! every remaining member then signs `Remove{k, Departed}` for the world to follow (the user's and the
//! coordinator's rulings). The sim signs what `take_protocol` offers, as pe-app's driver does.

use super::*;
use pe_seams::{Remove, RemoveReason};

fn sim(n: u32, seed: u64, silence_s: f64) -> Sim {
    let mut s = Sim::with(n, seed, |c| c.silence_s = silence_s);
    s.drive_protocol = true;
    s
}

fn keys_of(s: &Sim, who: &[usize]) -> Vec<MemberKey> {
    let mut v: Vec<MemberKey> = who.iter().map(|&i| s.keys.key(i)).collect();
    v.sort();
    v
}

fn present(s: &Sim) -> Frame {
    s.clock.frame_at(HostTime(s.now)).unwrap()
}

fn lowest_final(s: &Sim, who: &[usize]) -> Frame {
    who.iter().map(|&i| s.final_frame(i)).min().unwrap()
}

fn faults(s: &Sim, i: usize) -> Vec<&AgreeEvent> {
    s.events[i].iter().filter(|e| matches!(e, AgreeEvent::Fault(_))).collect()
}

/// What member `i` signed through the driver about member `k`'s key for `reason`.
fn offered(s: &Sim, i: usize, k: usize, reason: RemoveReason) -> Vec<(EntryRef, Frame)> {
    let key = s.keys.key(k);
    s.protocol[i]
        .iter()
        .filter(|(_, p, _)| Remove::decode(p).flatten() == Some(Remove { key, reason }))
        .map(|(id, _, f)| (*id, *f))
        .collect()
}

/// Member `i` signs `Remove{k, reason}` by hand, at the present.
fn sign(s: &mut Sim, i: usize, k: usize, reason: RemoveReason) -> EntryRef {
    let p = present(s);
    let payload = Remove { key: s.keys.key(k), reason }.encode();
    let e = s.builders[i].next_unsigned(Class::Protocol, p, payload);
    let e = s.nodes[i].submit(e).expect("own Remove refused");
    s.submitted[i].push(e.id());
    s.submitted_at[i].push(p);
    e.id()
}

/// Run until every member in `who` holds exactly `members`, within `within_ms`.
fn run_to_members(s: &mut Sim, who: &[usize], members: &[usize], within_ms: i64) {
    let want = keys_of(s, members);
    let end = s.now / MS + within_ms;
    while who.iter().any(|&i| s.nodes[i].members() != want) && s.now / MS < end {
        let next = s.now / MS + 10;
        s.run(next);
    }
    for &i in who {
        assert_eq!(s.nodes[i].members(), want, "member {i}'s members within {within_ms} ms");
    }
}

/// The remaining members go on: 2 s on, their lowest final frame is within 700 ms of the present, with no fault.
fn assert_go_on(s: &mut Sim, live: &[usize], what: &str) {
    let t = s.now / MS;
    s.run(t + 2_000);
    let behind_ms = (present(s).0 - lowest_final(s, live).0) * 1000 / 60;
    assert!(behind_ms < 700, "{what}: the remaining members didn't go on: {behind_ms} ms behind");
    for &i in live {
        assert!(faults(s, i).is_empty(), "{what}: faults at {i}: {:?}", faults(s, i));
    }
}

/// Every member in `live` signed a `Remove{k, Departed}`, and at each of them one is final and effective; the rest
/// are duplicates, void as `NoChange` (k no longer awaits departure), never as anything else.
fn assert_world_follows(s: &Sim, live: &[usize], k: usize, what: &str) {
    let below = lowest_final(s, live);
    for &i in live {
        let mine = offered(s, i, k, RemoveReason::Departed);
        assert!(!mine.is_empty(), "{what}: member {i} signed no departure of {k}");
    }
    let all: Vec<EntryRef> = live.iter().flat_map(|&i| offered(s, i, k, RemoveReason::Departed)).map(|x| x.0).collect();
    for &i in live {
        let void: Vec<_> = s.voids(i).into_iter().filter(|v| all.contains(&v.0)).collect();
        let effective = all.iter().filter(|id| s.is_final(i, **id, below) && !void.iter().any(|v| v.0 == **id)).count();
        assert_eq!(effective, 1, "{what}: departures of {k} effective at {i}");
        assert!(void.iter().all(|v| v.1 == VoidReason::NoChange), "{what}: departures of {k} void at {i}: {void:?}");
        assert!(Agreement::awaiting_departure(&s.nodes[i]).is_empty(), "{what}: {i} still awaits a departure");
    }
}

#[test]
fn a_crashed_member_at_4_is_removed_after_30_s_of_silence_and_the_world_follows() {
    for seed in [451u64, 452] {
        let mut s = sim(4, seed, 30.0);
        s.down_from[3] = 1_000;
        s.run(29_000);
        let live = [0, 1, 2];
        for &i in &live {
            assert_eq!(s.nodes[i].epoch(), 0, "seed {seed}: member {i} removed before 30 s");
            assert!(offered(&s, i, 3, RemoveReason::Silent).is_empty(), "seed {seed}: {i} voted before 30 s");
        }
        for &i in &live {
            s.run_to_epoch(i, 1, 4_000);
        }
        let at = s.now / MS;
        eprintln!("seed {seed}: down at 1000 ms, out at every member by {at} ms");
        assert!(at <= 32_000, "seed {seed}: removed only at {at} ms");
        for &i in &live {
            assert_eq!(s.nodes[i].members(), keys_of(&s, &live), "seed {seed}: member {i}'s members");
            assert_eq!(offered(&s, i, 3, RemoveReason::Silent).len(), 1, "seed {seed}: member {i}'s votes");
        }
        assert_go_on(&mut s, &live, &format!("seed {seed}"));
        assert_world_follows(&s, &live, 3, &format!("seed {seed}"));
        s.assert_same_final_log_among(&live, Frame(0));
    }
}

#[test]
fn two_members_down_at_7_are_removed_by_silence_and_the_five_go_on() {
    let mut s = sim(7, 717, 3.0);
    s.down_from[5] = 1_000;
    s.down_from[6] = 1_000;
    let live = [0, 1, 2, 3, 4];
    run_to_members(&mut s, &live, &live, 8_000);
    eprintln!("7 -> 5 by silence at {} ms, epoch {}", s.now / MS, s.nodes[0].epoch());
    assert_go_on(&mut s, &live, "7 -> 5");
    assert_world_follows(&s, &live, 5, "7 -> 5");
    assert_world_follows(&s, &live, 6, "7 -> 5");
    s.assert_same_final_log_among(&live, Frame(0));
}

#[test]
fn at_3_the_two_live_members_remove_the_silent_one_and_both_sign_its_departure() {
    let mut s = sim(3, 331, 3.0);
    s.down_from[2] = 1_000;
    let live = [0, 1];
    run_to_members(&mut s, &live, &live, 6_000);
    assert_go_on(&mut s, &live, "3 -> 2");
    assert_world_follows(&s, &live, 2, "3 -> 2");
}

#[test]
fn at_2_a_silent_member_is_never_removed() {
    // quorum(2) = 2 needs the silent member's own vote, so none is offered (agreement.md 8: a crash at n = 2 is S7's).
    let mut s = sim(2, 221, 1.0);
    s.down_from[1] = 1_000;
    s.run(5_000);
    assert!(s.protocol[0].is_empty(), "member 0 signed {:?}", s.protocol[0]);
    assert_eq!(s.nodes[0].members(), keys_of(&s, &[0, 1]));
}

#[test]
fn votes_made_before_a_member_came_back_never_remove_it() {
    // @design-reviewer's M2 on rev 2: by hand, members 0 and 1 vote against member 3 while it is live (its entries
    // keep becoming final, each dropping the votes stamped before it); a second later member 2 votes alone; then all
    // three at once, while 3's next entries follow theirs. The tally never reaches 3 votes stamped after 3's newest
    // final entry, so 3 stays.
    for seed in [461u64, 462, 463] {
        let mut s = Sim::new(4, seed);
        s.run(1_500);
        sign(&mut s, 0, 3, RemoveReason::Silent);
        sign(&mut s, 1, 3, RemoveReason::Silent);
        s.run(2_500);
        sign(&mut s, 2, 3, RemoveReason::Silent);
        s.run(3_500);
        for i in 0..3 {
            sign(&mut s, i, 3, RemoveReason::Silent);
        }
        s.run(6_000);
        for i in 0..4 {
            assert_eq!(s.nodes[i].epoch(), 0, "seed {seed}: member {i} changed epochs");
            assert_eq!(s.nodes[i].members(), keys_of(&s, &[0, 1, 2, 3]), "seed {seed}: member {i}'s members");
            assert!(faults(&s, i).is_empty(), "seed {seed}: faults at {i}: {:?}", faults(&s, i));
        }
        s.assert_same_final_log();
    }
}

#[test]
fn a_member_that_receives_but_cannot_send_is_removed_and_sees_it() {
    // The one-way case the rule can't tell from a crash (coordinator): member 3 hears everyone, but
    // nothing it sends arrives. It is removed, and since it still receives, it sees itself out of the members.
    let mut s = sim(4, 471, 3.0);
    s.mute = Some((3, 1_000, 30_000));
    let live = [0, 1, 2];
    run_to_members(&mut s, &[0, 1, 2, 3], &live, 6_000);
    assert_go_on(&mut s, &live, "mute");
    assert_world_follows(&s, &live, 3, "mute");
}

#[test]
fn a_member_that_sends_but_cannot_receive_is_never_removed() {
    // The other one-way case: member 3 receives nothing for 10 s, but its entries arrive, so the others hear it. Its
    // own votes against the three, one voter each, never count enough.
    let mut s = sim(4, 481, 3.0);
    s.deaf = Some((3, 1_000, 11_000));
    s.run(12_000);
    for i in 0..3 {
        assert_eq!(s.nodes[i].members(), keys_of(&s, &[0, 1, 2, 3]), "member {i}'s members");
        assert!(offered(&s, i, 3, RemoveReason::Silent).is_empty(), "member {i} voted against the deaf one");
        assert!(faults(&s, i).is_empty(), "faults at {i}: {:?}", faults(&s, i));
    }
    assert!(!offered(&s, 3, 0, RemoveReason::Silent).is_empty(), "the deaf member never voted (no evidence)");
    let behind_ms = (present(&s).0 - lowest_final(&s, &[0, 1, 2]).0) * 1000 / 60;
    assert!(behind_ms < 700, "the three stopped: {behind_ms} ms behind");
}

#[test]
fn the_removed_members_late_entries_arrive_after_the_epoch_drop_and_never_apply() {
    // The coordinator's ask: member 3 is cut off but alive, signing entries (and votes against the
    // three) all along. It is removed at about 4 s; at 7 s its link returns and its backlog arrives. The three take
    // none of it (no fault: a former member's entries are dropped), and member 3 sees itself out and settles.
    for seed in [4101u64, 4102] {
        let mut s = sim(4, seed, 3.0);
        s.partition = Some((3, 1_000, 7_000));
        let live = [0, 1, 2];
        run_to_members(&mut s, &live, &live, 6_000);
        let dropped_at = present(&s);
        s.run(12_000);
        assert_eq!(s.nodes[3].members(), keys_of(&s, &live), "seed {seed}: member 3's view of the members");
        assert!(Agreement::leave_settled(&s.nodes[3]), "seed {seed}: member 3 never settled");
        let k = s.keys.key(3);
        for &i in &live {
            assert!(faults(&s, i).is_empty(), "seed {seed}: faults at {i}: {:?}", faults(&s, i));
            let newest = (0..s.final_frame(i).0)
                .filter_map(|f| s.nodes[i].entries_at(Frame(f)))
                .flat_map(|v| v.iter().filter(|e| e.entry.author == k).map(|e| e.entry.stamp).collect::<Vec<_>>())
                .max();
            assert!(
                newest.is_some_and(|f| f.0 < 66 + 30),
                "seed {seed}: member 3's entries final at {i} up to {newest:?} (cut off at frame 60, out at {dropped_at:?})"
            );
        }
        let behind_ms = (present(&s).0 - lowest_final(&s, &live).0) * 1000 / 60;
        assert!(behind_ms < 700, "seed {seed}: the three stopped: {behind_ms} ms behind");
        assert_world_follows(&s, &live, 3, &format!("seed {seed}"));
        s.assert_same_final_log_among(&live, Frame(0));
    }
}

#[test]
fn a_replay_of_a_crashed_members_old_entries_does_not_hold_off_its_removal() {
    // @design-reviewer's M1: only new content signed by k resets its clock. Member 0 replays k's newest final entry to
    // members 1 and 2 every 500 ms after k crashes; k is still removed.
    let mut s = sim(4, 491, 3.0);
    s.down_from[3] = 1_000;
    s.run(1_800);
    let k = s.keys.key(3);
    let last = (0..s.final_frame(0).0)
        .filter_map(|f| s.nodes[0].entries_at(Frame(f)))
        .flat_map(|v| v.iter().filter(|e| e.entry.author == k).cloned().collect::<Vec<_>>())
        .max_by_key(|e| e.entry.seq)
        .expect("k's entries final at 0");
    let bytes = Bytes::from(crate::codec::encode(&crate::codec::Wire::Entries(vec![last])));
    let live = [0, 1, 2];
    let want = keys_of(&s, &live);
    while s.nodes[1].members() != want && s.now / MS < 7_000 {
        for j in [1, 2] {
            s.seq += 1;
            s.net.insert((s.now + 5 * MS, s.seq), (0, j, Lane::Action, bytes.clone()));
        }
        let t = s.now / MS + 500;
        s.run(t);
    }
    run_to_members(&mut s, &live, &live, 2_000);
    assert_go_on(&mut s, &live, "replay");
    assert_world_follows(&s, &live, 3, "replay");
}

#[test]
fn silence_votes_and_departures_that_may_not_or_change_nothing_are_void() {
    // Key 4 is never a member (a joiner never started). In epoch 0: member 1 votes against itself (NotAuthorized);
    // member 0 votes against the non-member 4 (NoChange); member 0 signs the departure of the current member 2
    // (NotAuthorized: it can never remove a current member) and of the never-member 4 (NoChange: it awaits no
    // departure; decided by the awaiting set, which a joiner holds too).
    let mut s = Sim::build(5, 4, 4111, |_, _| {});
    s.run(1_500);
    let own = sign(&mut s, 1, 1, RemoveReason::Silent);
    let stranger = sign(&mut s, 0, 4, RemoveReason::Silent);
    let current = sign(&mut s, 0, 2, RemoveReason::Departed);
    let never = sign(&mut s, 0, 4, RemoveReason::Departed);
    s.run(3_000);
    let live = [0, 1, 2, 3];
    for &i in &live {
        assert_eq!(s.nodes[i].members(), keys_of(&s, &live), "member {i}'s members");
        let v: Vec<(EntryRef, VoidReason)> = s.voids(i).iter().map(|v| (v.0, v.1)).collect();
        assert!(v.contains(&(own, VoidReason::NotAuthorized)), "member {i}: a vote against itself: {v:?}");
        assert!(v.contains(&(stranger, VoidReason::NoChange)), "member {i}: a vote against a non-member: {v:?}");
        assert!(v.contains(&(current, VoidReason::NotAuthorized)), "member {i}: a current member's departure: {v:?}");
        assert!(v.contains(&(never, VoidReason::NoChange)), "member {i}: a never-member's departure: {v:?}");
    }
}

#[test]
fn a_rejoin_before_the_world_follows_is_void_and_the_retry_after_it_admits() {
    // Coordinator: member 3 is removed by silence; before any `Departed{3}` is final, member 0 admits
    // 3 again: void as `NoChange`, and 3 awaits departure everywhere. Once a `Departed{3}` is final, the retry admits.
    // The votes by hand and the driver off, so no `Departed` is signed before the early `Admit`.
    let mut s = Sim::new(4, 4121);
    s.down_from[3] = 1_000;
    s.run(1_500);
    let live = [0, 1, 2];
    for &i in &live {
        sign(&mut s, i, 3, RemoveReason::Silent);
    }
    run_to_members(&mut s, &live, &live, 3_000);
    let k = s.keys.key(3);
    for &i in &live {
        assert_eq!(Agreement::awaiting_departure(&s.nodes[i]), vec![k], "member {i} doesn't await 3's departure");
    }
    let admit = |s: &mut Sim| {
        let p = present(s);
        let e = s.builders[0].next_unsigned(Class::Protocol, p, crate::node::Admit::new(k, 3).encode());
        let e = s.nodes[0].submit(e).expect("own Admit refused");
        s.submitted[0].push(e.id());
        s.submitted_at[0].push(p);
        e.id()
    };
    let early = admit(&mut s);
    let t = s.now / MS;
    s.run(t + 1_500);
    for &i in &live {
        let v: Vec<(EntryRef, VoidReason)> = s.voids(i).iter().map(|v| (v.0, v.1)).collect();
        assert!(v.contains(&(early, VoidReason::NoChange)), "member {i}: the early Admit: {v:?}");
        assert_eq!(s.nodes[i].members(), keys_of(&s, &live), "member {i}'s members after the early Admit");
    }
    s.drive_protocol = true;
    let end = s.now / MS + 3_000;
    while live.iter().any(|&i| !Agreement::awaiting_departure(&s.nodes[i]).is_empty()) && s.now / MS < end {
        let t = s.now / MS + 10;
        s.run(t);
    }
    for &i in &live {
        assert!(Agreement::awaiting_departure(&s.nodes[i]).is_empty(), "member {i} still awaits 3's departure");
    }
    s.drive_protocol = false;
    let retry = admit(&mut s);
    let epoch = s.nodes[0].epoch();
    for &i in &live {
        s.run_to_epoch(i, epoch + 1, 3_000);
        assert_eq!(s.nodes[i].members(), keys_of(&s, &[0, 1, 2, 3]), "member {i}'s members after the retry");
        assert!(s.voids(i).iter().all(|v| v.0 != retry), "member {i}: the retry voided");
    }
}

#[test]
fn a_member_admitted_after_the_drop_offers_the_departure_too() {
    // @chunks-reviewer's Low 1 on 544655cf: 3 is removed by silence (votes by hand, the driver off, so no `Departed`
    // is signed), then 0 admits the joiner 4. 4 takes the awaiting set from its start record and offers
    // `Departed{3}` itself, so the departure doesn't hang on the members present at the drop.
    let mut s = Sim::with_joiners(5, 4, 4131);
    s.down_from[3] = 1_000;
    s.run(1_500);
    let live = [0, 1, 2];
    for &i in &live {
        sign(&mut s, i, 3, RemoveReason::Silent);
    }
    run_to_members(&mut s, &live, &live, 3_000);
    admit(&mut s, 0, 4, 3);
    s.run_to_epoch(0, 2, 3_000);
    s.join(4, 0, &live, None);
    let k = s.keys.key(3);
    assert_eq!(Agreement::awaiting_departure(&s.nodes[4]), vec![k], "the joiner doesn't await 3's departure");
    let offer = Agreement::take_protocol(&mut s.nodes[4]).map(|p| Remove::decode(&p).flatten());
    assert_eq!(offer, Some(Some(Remove { key: k, reason: RemoveReason::Departed })), "the joiner's offer");
}

#[test]
fn a_silence_vote_the_driver_did_not_sign_is_offered_again_a_second_later() {
    // @chunks-reviewer's Low 2 on 544655cf: an offer the driver drops (a refused submit) comes again after about a
    // second, not after `silence_s`; once one is signed, the next waits `silence_s`.
    let mut s = Sim::with(4, 4141, |c| c.silence_s = 5.0);
    s.down_from[3] = 1_000;
    let vote = |s: &mut Sim| {
        Agreement::take_protocol(&mut s.nodes[0]).and_then(|p| Remove::decode(&p).flatten()).map(|r| (r.key, r.reason))
    };
    let k = s.keys.key(3);
    let until = |s: &mut Sim, end: i64| {
        while s.now / MS < end {
            if vote(s) == Some((k, RemoveReason::Silent)) {
                return Some(s.now / MS);
            }
            let t = s.now / MS + 10;
            s.run(t);
        }
        None
    };
    let first = until(&mut s, 9_000).expect("no silence vote within 9 s");
    let again = until(&mut s, first + 2_000).expect("the dropped vote wasn't offered again within 2 s");
    assert!(again - first >= 900, "offered again after {} ms", again - first);
    sign(&mut s, 0, 3, RemoveReason::Silent);
    let t = s.now / MS;
    assert_eq!(until(&mut s, t + 4_000), None, "a signed vote was offered again within 4 s");
}
