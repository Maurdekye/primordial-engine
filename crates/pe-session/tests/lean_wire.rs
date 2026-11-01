//! The R18 below-20 item, option 1 (@coordinator-opus, 2026-11-01): an author's own live entries go
//! out lean (`Msg::Lean`), without the ids their receiver rebuilds from what it holds. The signature
//! still covers the full entry, so a receiver whose ids differ fetches the explicit form; one that
//! lacks a named entry waits for it, then asks for the explicit form (review conditions (a)-(c),
//! @engine-architect).

use pe_session::session::*;

const W: u32 = 7;

fn sim(me: MemberId) -> WorldSim {
    WorldSim::new(W, me, &[1, 2, 3], SceneKind::Empty, SimConfig::default(), KeyRing::test(me))
}

/// `e` as its receiver decodes it from the lean wire form.
fn lean(e: &SignedEntry) -> SignedEntry {
    match decode_msg(&encode_msg(&Msg::Lean(e.clone()))) {
        Some(Msg::Lean(x)) => x,
        other => panic!("lean round trip: {other:?}"),
    }
}

fn fetches(s: &mut WorldSim) -> Vec<(MemberId, MemberId, u32, u32)> {
    s.take_fetches(1_000_000)
        .into_iter()
        .filter_map(|(to, m)| match m {
            Msg::Fetch { author, from, to: upto } => Some((to, author, from, upto)),
            _ => None,
        })
        .collect()
}

#[test]
fn the_lean_form_omits_prev_and_head_ids_and_rebuilds_to_the_same_entry() {
    let (mut a, mut b) = (sim(1), sim(2));
    let a1 = a.author_ack();
    b.insert(a1.clone());
    let b1 = b.author_ack();
    let b2 = b.author_ack();
    // b2 names b1 as prev and a1 as a head: 32 + 32 bytes fewer on the wire.
    assert!(matches!(&b2.payload, Payload::Ack { heads, .. } if heads.len() == 1));
    assert_eq!(encode_msg(&Msg::Entry(b2.clone())).len() - encode_msg(&Msg::Lean(b2.clone())).len(), 64);
    let l = lean(&b2);
    assert_eq!(l.prev, EntryId::NONE);
    assert!(matches!(&l.payload, Payload::Ack { heads, .. } if heads.iter().all(|h| h.2 == EntryId::NONE)));
    assert!(a.insert_lean(2, lean(&b1)).is_empty());
    a.insert_lean(2, l);
    assert_eq!(a.id_at(2, b1.seq), Some(b1.id()));
    assert_eq!(a.id_at(2, b2.seq), Some(b2.id()));
    assert_eq!((a.stats.lean_in, a.stats.lean_fallbacks, a.stats.bad_sigs), (2, 0, 0));
}

#[test]
fn a_lean_entry_past_a_hole_waits_for_it_then_asks_for_its_explicit_form() {
    let (mut a, mut b) = (sim(1), sim(2));
    let (a1, a2, a3) = (a.author_ack(), a.author_ack(), a.author_ack());
    // a2 arrives first: it waits, and the hole is asked for, as an explicit entry past it would ask.
    b.insert_lean(1, lean(&a2));
    assert_eq!(b.id_at(1, 2), None);
    assert!(fetches(&mut b).contains(&(1, 1, 1, 1)));
    // The hole arrives: both are in.
    b.insert_lean(1, lean(&a1));
    b.resolve_lean();
    assert_eq!((b.id_at(1, 1), b.id_at(1, 2)), (Some(a1.id()), Some(a2.id())));
    // a3's predecessor is held, so it goes straight in. A 5th past a missing 4th waits for the hole
    // only: the explicit form could not extend the chain past it either.
    b.insert_lean(1, lean(&a3));
    let a4 = a.author_ack();
    let a5 = a.author_ack();
    b.insert_lean(1, lean(&a5));
    b.now += 10 * WorldSim::LEAN_HOLD_FRAMES;
    b.resolve_lean();
    assert_eq!(b.stats.lean_fallbacks, 0);
    b.insert(a4);
    b.resolve_lean();
    assert_eq!(b.id_at(1, 5), Some(a5.id()));
}

#[test]
fn a_lean_entry_that_does_not_verify_against_the_held_ids_is_fetched_explicit_not_dropped() {
    let (mut a, mut b) = (sim(1), sim(2));
    let a1 = a.author_ack();
    b.insert(a1.clone());
    // Member 1 forks seq 1: another version, and a seq 2 on top of it.
    let fork1 = test_sign(W, Entry { stamp: a1.stamp + 1, ..a1.entry.clone() });
    let fork2 = test_sign(W, Entry { member: 1, seq: 2, prev: fork1.id(), stamp: a1.stamp + 1, payload: a1.payload.clone(), carried: None });
    // Rebuilt on b's version of seq 1 it does not verify: not a bad signature, a possible fork, so
    // its explicit form is asked for from the sender at once, and everyone's version of seq 1, as the
    // explicit form would then make b ask.
    assert!(b.insert_lean(1, lean(&fork2)).is_empty());
    assert_eq!((b.stats.lean_fallbacks, b.stats.bad_sigs), (1, 0));
    let f = fetches(&mut b);
    assert!(f.contains(&(1, 1, 2, 2)));
    assert!(f.contains(&(3, 1, 1, 1)));
    // The explicit form then goes the explicit way.
    b.insert(fork2.clone());
    assert_eq!(b.id_at(1, 2), Some(fork2.id()));
}

#[test]
fn an_ack_naming_an_entry_not_yet_held_waits_for_it_and_asks_the_acker() {
    let (mut a, mut b, mut c) = (sim(1), sim(2), sim(3));
    let c1 = c.author_ack();
    b.insert(c1.clone());
    let b1 = b.author_ack();
    // a holds nothing of 3's chain: b1's head (3, 1) cannot be rebuilt yet. It waits, and 3's entry is
    // asked for from the acker, who holds it.
    a.insert_lean(2, lean(&b1));
    assert_eq!(a.id_at(2, 1), None);
    assert!(fetches(&mut a).contains(&(2, 3, 1, 1)));
    // Still missing after LEAN_HOLD_FRAMES: the explicit form is asked for from the sender, once.
    a.now += WorldSim::LEAN_HOLD_FRAMES;
    a.resolve_lean();
    a.resolve_lean();
    assert_eq!(a.stats.lean_fallbacks, 1);
    assert!(fetches(&mut a).contains(&(2, 2, 1, 1)));
    a.insert(c1);
    a.resolve_lean();
    assert_eq!(a.id_at(2, 1), Some(b1.id()));
}

#[test]
fn an_ack_naming_a_chain_far_past_what_the_peer_holds_goes_explicit() {
    let (mut a, mut c) = (sim(1), sim(3));
    // Any entry but such an ack goes lean.
    let own = a.author_ack();
    assert!(a.lean_to(2, &own));
    // An ack naming 3's chain more than LEAN_HEAD_SLACK past what 2 has acked holding of it (nothing)
    // goes explicit to 2, and lean to 3 (its own chain).
    for _ in 0..=WorldSim::LEAN_HEAD_SLACK {
        a.insert(c.author_ack());
    }
    let ack = a.author_ack();
    assert!(matches!(&ack.payload, Payload::Ack { heads, .. } if heads.iter().any(|h| h.0 == 3 && h.1 == WorldSim::LEAN_HEAD_SLACK + 1)));
    assert!(!a.lean_to(2, &ack));
    assert!(a.lean_to(3, &ack));
}
