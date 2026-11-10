//! Review P23 (design-reviewer, a812805): hostile input from an authenticated peer.
//!
//! * P23-1: nesting is bounded in the decoder (a batch holds no batch; hops nest at most
//!   `MAX_HOP_NESTING` deep) and trailing bytes are refused, so one message cannot exhaust the stack.
//! * P23-3: a member restores only the state it asked for, from that donor, and never rewinds.
//! * P23-4: requests are answered for members and followers only; one state per requester per
//!   interval; every answer is a repair.

use pe_session::peer::{Class, Event, SessionCore, STATE_ANSWER_INTERVAL_US};
use pe_session::provenance::Hop;
use pe_session::session::*;
use pe_session::sign::test_sign;

const SCENE: SceneKind = SceneKind::Pile { side: 4, layers: 2 };
const MEMBERS: [MemberId; 3] = [1, 2, 3];

fn sim(me: MemberId) -> WorldSim {
    WorldSim::new(1, me, &MEMBERS, SCENE, SimConfig::default(), KeyRing::test(me))
}

fn members() -> Vec<SessionCore> {
    let mut cores: Vec<SessionCore> = MEMBERS.iter().map(|&m| SessionCore::new(sim(m))).collect();
    for c in &mut cores {
        c.start(0);
    }
    cores
}

/// The three members back to back, member 1 touching an object every 97 ms.
fn run(cores: &mut [SessionCore], from_ms: u64, to_ms: u64) {
    for ms in from_ms..to_ms {
        let now = ms * 1000;
        for c in cores.iter_mut() {
            c.tick(now);
        }
        if ms % 97 == 0 {
            let obj = (ms / 97 % 16) as u32;
            let _ = cores[0].author(Payload::Touch { obj, impulse: [0.4, 2.0, -0.3], grab: false }, 0);
        }
        let outs: Vec<(MemberId, Vec<_>)> = cores.iter_mut().enumerate().map(|(i, c)| (i as MemberId + 1, c.outbox())).collect();
        for (from, out) in outs {
            for o in out {
                for &to in &o.to {
                    cores[to as usize - 1].receive(from, o.msg.clone(), now);
                }
            }
        }
    }
}

fn transfers(c: &mut SessionCore) -> Vec<Result<(), TransferError>> {
    c.events().into_iter().filter_map(|e| if let Event::Transfer { result, .. } = e { Some(result) } else { None }).collect()
}

/// Batch tag 4 with one element, `depth` times, around a FetchState (tag 9).
fn nested_batch(depth: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(5 * depth + 1);
    for _ in 0..depth {
        b.extend([4u8, 1, 0, 0, 0]);
    }
    b.push(9);
    b
}

#[test]
fn p23_1_a_batch_holds_no_batch() {
    assert_eq!(decode_msg(&nested_batch(1)), Some(Msg::Batch(vec![Msg::FetchState])));
    assert_eq!(decode_msg(&nested_batch(2)), None);
    // The reviewer's probe: 100,000 levels on an 8 MB stack decode to a refusal, not an abort.
    let deep = nested_batch(100_000);
    let r = std::thread::Builder::new().stack_size(8 << 20).spawn(move || decode_msg(&deep)).unwrap().join().unwrap();
    assert_eq!(r, None);
}

#[test]
fn p23_1_trailing_bytes_are_refused() {
    let b = encode_msg(&Msg::FetchState);
    assert_eq!(decode_msg(&b), Some(Msg::FetchState));
    assert_eq!(decode_msg(&[b.as_slice(), &[0]].concat()), None);
}

/// A touch inside `levels` hops: each a departure whose hop's certificate carries the previous entry
/// as a proof.
fn nested_hops(levels: u32) -> Entry {
    let mut e = Entry { member: 1, seq: 1, prev: EntryId::NONE, stamp: 5, payload: Payload::Touch { obj: 1, impulse: [0.0; 3], grab: false }, carried: None };
    for _ in 0..levels {
        let cert = EpochCert { world: 1, epoch: 1, change: Change::default(), round: 0, members: vec![1], precommits: vec![], proofs: vec![e] };
        let hop = Hop { src: 1, dest: 2, transfer: 7, via: 0, epoch: 1, frame: 5, certs: vec![cert], cert: vec![] };
        e = Entry { member: 1, seq: 1, prev: EntryId::NONE, stamp: 5, payload: Payload::Depart { obj: 1, dest: 2, deadline: 9, hop: Some(hop) }, carried: None };
    }
    e
}

#[test]
fn p23_1_hops_nest_at_most_max_hop_nesting_deep() {
    let ok = Msg::Entry(test_sign(1, nested_hops(MAX_HOP_NESTING)));
    assert_eq!(decode_msg(&encode_msg(&ok)), Some(ok));
    let deep = Msg::Entry(test_sign(1, nested_hops(MAX_HOP_NESTING + 1)));
    assert_eq!(decode_msg(&encode_msg(&deep)), None);
}

#[test]
fn p23_3_a_running_member_refuses_an_old_state_it_never_asked_for() {
    let mut cores = members();
    run(&mut cores, 0, 3000);
    let old = cores[0].sim.export_state().expect("a confirmed check frame");
    run(&mut cores, 3000, 6000);
    transfers(&mut cores[1]);
    let before = (cores[1].sim.frame, cores[1].sim.conf, cores[1].sim.state_hash());
    // The reviewer's probe: unrequested, it is refused before anything is read.
    cores[1].receive(1, Msg::State(Box::new(old.clone())), 6_000_000);
    assert_eq!(transfers(&mut cores[1]), vec![Err(TransferError::Unrequested)]);
    assert_eq!((cores[1].sim.frame, cores[1].sim.conf, cores[1].sim.state_hash()), before);
    // Asked for, an old state still never rewinds the replica.
    cores[1].request_state(1);
    cores[1].receive(1, Msg::State(Box::new(old.clone())), 6_000_000);
    assert_eq!(transfers(&mut cores[1]), vec![Err(TransferError::Behind)]);
    assert_eq!(cores[1].sim.import_state(&old, 1), Err(TransferError::Behind));
    assert_eq!((cores[1].sim.frame, cores[1].sim.conf, cores[1].sim.state_hash()), before);
    assert_eq!(cores[1].sim.stats.imports, 0);
}

#[test]
fn p23_3_a_joiner_takes_one_answer_per_request_and_only_from_that_donor() {
    let mut cores = members();
    run(&mut cores, 0, 3000);
    let st = cores[0].sim.export_state().expect("a confirmed check frame");
    let mut j = SessionCore::new(sim(4));
    j.start(0);
    let mut v: Vec<Entry> = cores[0].sim.all_entries();
    v.sort_by_key(|e| (e.member, e.seq));
    for e in v {
        j.receive(1, Msg::Entry(cores[0].sim.signed_entry(e.id()).unwrap()), 3_000_000);
    }
    j.request_state(1);
    // Member 2 was not asked; member 1's answer is restored; a second copy is unrequested.
    j.receive(2, Msg::State(Box::new(st.clone())), 3_000_000);
    j.receive(1, Msg::State(Box::new(st.clone())), 3_000_000);
    j.receive(1, Msg::State(Box::new(st.clone())), 3_000_000);
    assert_eq!(transfers(&mut j), vec![Err(TransferError::Unrequested), Ok(()), Err(TransferError::Unrequested)]);
    assert_eq!(j.sim.stats.imports, 1);
}

#[test]
fn p23_4_requests_from_outside_the_audience_are_not_answered() {
    let mut cores = members();
    run(&mut cores, 0, 3000);
    cores[0].outbox();
    let held = cores[0].sim.all_entries().into_iter().find(|e| e.member == 2).unwrap();
    for m in [Msg::FetchState, Msg::Fetch { author: 2, from: 1, to: 5, held: 0 }, Msg::FetchEntry { id: held.id(), back: 3 }, Msg::FetchValue { value: 1 }] {
        cores[0].receive(9, m, 3_000_000);
    }
    assert!(cores[0].outbox().is_empty());
}

#[test]
fn p23_4_one_state_per_requester_per_interval_and_answers_are_repairs() {
    let mut cores = members();
    run(&mut cores, 0, 3000);
    cores[0].outbox();
    let states = |c: &mut SessionCore| c.outbox().into_iter().filter(|o| matches!(o.msg, Msg::State(_))).map(|o| o.class).collect::<Vec<_>>();
    // The reviewer's amplification: a batch of 1,200 requests yields one state.
    cores[0].receive(2, Msg::Batch(vec![Msg::FetchState; 1200]), 3_000_000);
    assert_eq!(states(&mut cores[0]), vec![Class::Repair]);
    cores[0].receive(2, Msg::FetchState, 3_000_000 + STATE_ANSWER_INTERVAL_US - 1);
    assert!(states(&mut cores[0]).is_empty());
    cores[0].receive(3, Msg::FetchState, 3_000_001);
    assert_eq!(states(&mut cores[0]), vec![Class::Repair]);
    cores[0].receive(2, Msg::FetchState, 3_000_000 + STATE_ANSWER_INTERVAL_US);
    assert_eq!(states(&mut cores[0]), vec![Class::Repair]);
    // Fetches by id and range are answered as repairs too.
    let held = cores[0].sim.all_entries().into_iter().find(|e| e.member == 2 && e.seq > 3).unwrap();
    cores[0].receive(2, Msg::FetchEntry { id: held.id(), back: 3 }, 4_100_000);
    cores[0].receive(2, Msg::Fetch { author: 3, from: 1, to: 4, held: 0 }, 4_100_000);
    let out = cores[0].outbox();
    assert!(!out.is_empty() && out.iter().all(|o| o.class == Class::Repair));
}
