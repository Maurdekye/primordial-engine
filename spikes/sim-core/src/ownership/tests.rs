//! Ownership tests. The attacks of review rounds 1 and 2 (OWN-*, OWNR-*) are named after them; their
//! failing-first versions are in commits cbcdadb and 5083c64.

use super::*;
use crate::provenance::StaticEpochs;

const S: Key = 10; // the seller / creator
const A: Key = 11;
const B: Key = 20;
const C: Key = 30;
const I: Key = 31;
const J: Key = 32;
const K: Key = 33;
const X: Key = 40;
const Z: Key = 41;
const R: Key = 50; // a recovery key
const R2: Key = 51;
const N: Key = 60; // a replacement key
const EVE: Key = 66;
/// Two session lineages (two disconnected copies of the world).
const L1: Lineage = 1;
const L2: Lineage = 2;
const W: u64 = OBJECTION_FRAMES;
const RF: u64 = RECOVERY_FRAMES;

fn next(book: &Book, prev: CertId, change: Change, keys: &[Key]) -> Cert {
    let seq = book.cert(prev).unwrap().seq + 1;
    Cert { world: book.world, seq, prev: vec![prev], change, sigs: vec![] }.signed(keys)
}

fn head(b: &Book) -> CertId {
    let h = b.heads();
    assert_eq!(h.len(), 1, "expected a linear chain");
    h[0]
}

/// Commit on the head in lineage L1 at `frame`, as a session does.
fn commit(b: &mut Book, change: Change, keys: &[Key], frame: u64) -> Result<Insert, OwnError> {
    let c = next(b, head(b), change, keys);
    b.commit_local(c, L1, frame)
}

/// Insert on any parent, attested in `lineage` at `frame` (None: never committed).
fn put(b: &mut Book, prev: CertId, change: Change, keys: &[Key], at: Option<(Lineage, u64)>) -> CertId {
    let c = next(b, prev, change, keys);
    let id = c.id();
    // Offline, a certificate whose parent doesn't count yet is parked until it does (OWNT-2, OWNT-5).
    assert!(matches!(b.insert(c), Ok(Insert::Applied | Insert::Parked)));
    if let Some((l, f)) = at {
        b.record(id, l, f);
    }
    id
}

fn merge(b: &Book, keys: &[Key]) -> Cert {
    let heads = b.heads();
    let seq = heads.iter().map(|h| b.cert(*h).unwrap().seq).max().unwrap() + 1;
    Cert { world: b.world, seq, prev: heads, change: Change::Merge, sigs: vec![] }.signed(keys)
}

fn owners(b: &Book) -> Vec<Key> {
    b.view().owners.owners.into_iter().collect()
}

fn set(v: &View) -> Vec<Key> {
    v.owners.owners.iter().copied().collect()
}

fn insert_all(b: &mut Book, certs: &[Cert]) -> Result<(), Vec<(CertId, OwnError)>> {
    let r = b.insert_all(certs);
    if r.is_empty() { Ok(()) } else { Err(r) }
}

fn sessions() -> StaticEpochs {
    let mut s = StaticEpochs::default();
    s.worlds.insert(L1, (vec![1, 2, 3], 0));
    s.worlds.insert(L2, (vec![4, 5], 0));
    s
}

#[test]
fn genesis_binds_the_world_id() {
    let g = Cert::genesis(&[S, B], SignRule::All, 1);
    let b = Book::new(g.clone()).unwrap();
    assert_eq!(b.world, g.id());
    assert_eq!(owners(&b), [S, B]);
    let mut half = g.clone();
    half.sigs.pop();
    assert_eq!(Book::new(half), Err(OwnError::BadGenesis));
    assert_ne!(Cert::genesis(&[S, B], SignRule::All, 2).id(), b.world);
    let other = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let mut b2 = b.clone();
    assert_eq!(b2.insert(next(&other, other.genesis, Change::Add { key: B }, &[S])), Err(OwnError::WrongWorld));
}

#[test]
fn forged_transfer_and_rogue_co_owner_are_refused() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    assert_eq!(commit(&mut b, Change::Add { key: EVE }, &[EVE], 1), Err(OwnError::Signers));
    let mut forged = next(&b, head(&b), Change::Add { key: EVE }, &[]);
    forged.sigs.push((S, 12345));
    assert_eq!(b.commit_local(forged, L1, 1), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::Add { key: B }, &[S], 2), Ok(Insert::Applied));
    assert_eq!(commit(&mut b, Change::Add { key: EVE }, &[B], 3), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::Leave { key: S }, &[B], 3), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::Leave { key: S }, &[S], 3), Ok(Insert::Applied));
    assert_eq!(owners(&b), [B]);
}

/// OWN-12: under a threshold of 1 a removed owner's signature alone would suffice if it counted.
#[test]
fn removed_or_departed_owner_is_a_stranger() {
    let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::Threshold(1), 1)).unwrap();
    assert_eq!(commit(&mut b, Change::Remove { key: C }, &[S], 1), Ok(Insert::Applied));
    assert_eq!(commit(&mut b, Change::Add { key: X }, &[C], 2), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::Leave { key: S }, &[S], 2), Ok(Insert::Applied));
    assert_eq!(commit(&mut b, Change::Add { key: X }, &[S], 3), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::Add { key: X }, &[B], 3), Ok(Insert::Applied));
    assert_eq!(commit(&mut b, Change::Leave { key: X }, &[X], 4), Ok(Insert::Applied));
    assert_eq!(commit(&mut b, Change::Leave { key: B }, &[B], 5), Err(OwnError::LastOwner));
}

/// OWN-4: under rule A nobody is removed at once, with two owners or more.
#[test]
fn no_instant_removal_under_rule_a() {
    let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
    assert_eq!(commit(&mut b, Change::Remove { key: S }, &[B], 1), Err(OwnError::NotAllowed));
    let mut b3 = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
    assert_eq!(commit(&mut b3, Change::Remove { key: S }, &[B, C], 1), Err(OwnError::NotAllowed));
    let mut any = Book::new(Cert::genesis(&[S, B], SignRule::Any, 1)).unwrap();
    assert_eq!(commit(&mut any, Change::Challenge { key: S }, &[B], 1), Err(OwnError::NotAllowed));
}

#[test]
fn objection_window_in_a_session() {
    let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
    assert_eq!(commit(&mut b, Change::Challenge { key: S }, &[B], 10), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::Challenge { key: S }, &[B, C], 10), Ok(Insert::Applied));
    assert_eq!(commit(&mut b.clone(), Change::RemoveDormant { key: S }, &[B, C], 10 + W - 1), Err(OwnError::WindowOpen));
    // S signs anything during the window: it is cancelled.
    let mut b2 = b.clone();
    assert_eq!(commit(&mut b2, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], 20), Ok(Insert::Applied));
    assert_eq!(commit(&mut b2, Change::RemoveDormant { key: S }, &[B, C], 10 + W), Err(OwnError::WindowOpen));
    // S stays silent: the others remove it once the window has run.
    assert_eq!(commit(&mut b, Change::RemoveDormant { key: S }, &[B, C], 10 + W), Ok(Insert::Applied));
    assert_eq!(owners(&b), [B, C]);
}

/// The target cancels on a concurrent branch that the window's own session attested within it.
#[test]
fn concurrent_cancel_in_the_same_lineage_protects_the_target() {
    let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let ch = put(&mut b, g, Change::Challenge { key: S }, &[B, C], Some((L1, 5)));
    put(&mut b, ch, Change::RemoveDormant { key: S }, &[B, C], Some((L1, 5 + W)));
    assert_eq!(owners(&b), [B, C]);
    // Frames of another lineage are no clock for this window (OWNS-1c): no effect.
    let other = put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], Some((L2, 100)));
    assert_eq!(owners(&b), [B, C]);
    b.record(other, L1, 100);
    assert_eq!(owners(&b), [S, B, C]);
}

/// OWNR-2: neither an unattested signature nor one attested after the completion undoes it.
#[test]
fn completed_removal_is_not_undone_later() {
    let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let ch = put(&mut b, g, Change::Challenge { key: S }, &[B, C], Some((L1, 5)));
    let rd = put(&mut b, ch, Change::RemoveDormant { key: S }, &[B, C], Some((L1, 5 + W)));
    put(&mut b, ch, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], None);
    put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R2) }, &[S], Some((L2, 6 + W)));
    assert_eq!(owners(&b), [B, C]);
    // A later co-signature by the removed key on the main chain changes nothing either.
    let c = next(&b, rd, Change::SetRule { rule: SignRule::All }, &[B, C]).signed(&[S]);
    let id = c.id();
    b.insert(c).unwrap();
    b.record(id, L1, 10 + W);
    assert_eq!(owners(&b), [B, C]);
}

/// OWNR-3: a completed window verifies offline from the certificates and the attestations.
#[test]
fn offline_verification_with_attestations() {
    let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
    commit(&mut b, Change::Challenge { key: S }, &[B, C], 5).unwrap();
    commit(&mut b, Change::RemoveDormant { key: S }, &[B, C], 5 + W).unwrap();
    let certs: Vec<Cert> = b.prefix_of(head(&b)).into_iter().filter(|c| c.seq > 0).collect();
    let g = b.cert(b.genesis).unwrap().clone();
    let attests = b.export(L1, &[1, 2, 3]);
    let v = verify_chain(&g, &certs, &attests, &sessions()).unwrap();
    assert_eq!(set(&v), [B, C]);
    assert!(v.refused.is_empty());
    // Without attestations nothing after genesis counts, and the view says why.
    let bare = verify_chain(&g, &certs, &[], &sessions()).unwrap();
    assert_eq!(set(&bare), [S, B, C]);
    assert!(!bare.refused.is_empty() && bare.refused.iter().all(|(_, e)| *e == OwnError::Unattested));
    // A forged attestation, or one by a minority of the session, is ignored.
    let mut forged = attests.clone();
    for a in &mut forged {
        a.sigs.truncate(1);
    }
    assert_eq!(set(&verify_chain(&g, &certs, &forged, &sessions()).unwrap()), [S, B, C]);
}

/// OWNR-1 (a): after a cancelled removal, the remaining owner can't add alone on that branch.
#[test]
fn add_after_a_cancelled_removal_does_not_count() {
    let mut b = Book::new(Cert::genesis(&[A, B], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let ch = put(&mut b, g, Change::Challenge { key: B }, &[A], Some((L1, 1)));
    put(&mut b, ch, Change::SetRecovery { owner: B, recovery: Some(R) }, &[B], Some((L1, 2)));
    let rd = put(&mut b, ch, Change::RemoveDormant { key: B }, &[A], Some((L1, 1 + W)));
    put(&mut b, rd, Change::Add { key: X }, &[A], Some((L1, 2 + W)));
    assert_eq!(owners(&b), [A, B]);
}

/// OWNR-1 (b): a cancelled recovery's new key can't act.
#[test]
fn add_after_a_cancelled_recovery_does_not_count() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let r = put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], Some((L1, 1)));
    let st = put(&mut b, r, Change::StartRecover { owner: S, new_key: N }, &[R], Some((L1, 2)));
    put(&mut b, st, Change::SetRule { rule: SignRule::All }, &[S], Some((L1, 3)));
    let fin = put(&mut b, st, Change::FinishRecover { owner: S }, &[R], Some((L1, 2 + RF)));
    put(&mut b, fin, Change::Add { key: EVE }, &[N], Some((L1, 3 + RF)));
    assert_eq!(owners(&b), [S]);
}

/// OWN-6: a recovery key never signs for its owner; it only replaces a lost key, after a delay the
/// owner cancels.
#[test]
fn recovery_replaces_a_lost_key_only() {
    let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
    commit(&mut b, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], 1).unwrap();
    assert_eq!(commit(&mut b, Change::Add { key: R2 }, &[R, B], 2), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::Leave { key: S }, &[R], 2), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::SetRecovery { owner: S, recovery: Some(EVE) }, &[B], 2), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::StartRecover { owner: S, new_key: N }, &[EVE], 2), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::StartRecover { owner: S, new_key: N }, &[R], 2), Ok(Insert::Applied));
    let mut cancel = b.clone();
    assert_eq!(commit(&mut cancel, Change::SetRecovery { owner: S, recovery: Some(R2) }, &[S], 3), Ok(Insert::Applied));
    assert_eq!(commit(&mut cancel, Change::FinishRecover { owner: S }, &[R], 2 + RF), Err(OwnError::WindowOpen));
    assert_eq!(commit(&mut b.clone(), Change::FinishRecover { owner: S }, &[R], 1 + RF), Err(OwnError::WindowOpen));
    assert_eq!(commit(&mut b, Change::FinishRecover { owner: S }, &[R], 2 + RF), Ok(Insert::Applied));
    assert_eq!(owners(&b), [B, N]);
    assert!(b.view().owners.recovery.is_empty());
}

/// OWN-6: a recovery key replaced (attested) before it starts a recovery on another copy is dead.
#[test]
fn rotated_recovery_key_is_dead_on_every_branch() {
    let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let r1 = put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], Some((L1, 1)));
    put(&mut b, r1, Change::SetRecovery { owner: S, recovery: Some(R2) }, &[S], Some((L1, 2)));
    // R forks at r1 and replaces S there, in the same session after the rotation.
    let st = put(&mut b, r1, Change::StartRecover { owner: S, new_key: EVE }, &[R], Some((L1, 3)));
    put(&mut b, st, Change::FinishRecover { owner: S }, &[R], Some((L1, 3 + RF)));
    let v = b.view();
    assert_eq!(set(&v), [S, B]);
    assert!(v.refused.contains(&(st, OwnError::Revoked)));
    assert_eq!(v.owners.recovery[&S].keys().copied().collect::<Vec<_>>(), [R2]);
}

/// The sole owner sells twice on two disconnected copies (two sessions).
fn double_sale() -> (Book, Vec<Cert>) {
    let g = Cert::genesis(&[S], SignRule::All, 7);
    let b = Book::new(g).unwrap();
    let to_b = next(&b, b.genesis, Change::Add { key: B }, &[S]);
    let to_c = next(&b, b.genesis, Change::Add { key: C }, &[S]);
    let mut bb = b.clone();
    bb.insert(to_b.clone()).unwrap();
    let b_out = next(&bb, to_b.id(), Change::Leave { key: S }, &[S]);
    let mut bc = b.clone();
    bc.insert(to_c.clone()).unwrap();
    let c_out = next(&bc, to_c.id(), Change::Leave { key: S }, &[S]);
    (b, vec![to_b, b_out, to_c, c_out])
}

fn sold_twice(b0: &Book, certs: &[Cert]) -> Book {
    let mut b = b0.clone();
    insert_all(&mut b, certs).unwrap();
    for (i, c) in certs.iter().enumerate() {
        b.record(c.id(), if i < 2 { L1 } else { L2 }, 1 + i as u64);
    }
    b
}

#[test]
fn double_sale_is_detected_and_merges_in_any_order() {
    let (b0, certs) = double_sale();
    let mut views = BTreeSet::new();
    for perm in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2]] {
        let order: Vec<Cert> = perm.iter().map(|&i| certs[i].clone()).collect();
        let mut b = b0.clone();
        insert_all(&mut b, &order).unwrap();
        for &i in perm.iter().rev() {
            b.record(certs[i].id(), if i < 2 { L1 } else { L2 }, 1 + i as u64);
        }
        assert!(b.view().disputed);
        views.insert((owners(&b), b.digest()));
    }
    assert_eq!(views.len(), 1);
    assert_eq!(views.into_iter().next().unwrap().0, [B, C]);
    let mut b = sold_twice(&b0, &certs);
    let forks = b.forks();
    assert_eq!(forks.len(), 1);
    let p = &forks[0];
    let g = b.cert(b.genesis).unwrap().clone();
    assert!(p.verify(std::slice::from_ref(&g)));
    assert!(p.equivocation());
    assert_eq!(p.signed_both(), [S].into());
    let mut bad = p.clone();
    bad.b.sigs.clear();
    assert!(!bad.verify(std::slice::from_ref(&g)));
    // Either buyer merges; afterwards rule A runs over the union.
    assert_eq!(b.insert(merge(&b, &[EVE])), Err(OwnError::Signers));
    let m = merge(&b, &[B]);
    b.insert(m.clone()).unwrap();
    b.record(m.id(), L1, 10);
    assert_eq!(b.heads(), [m.id()]);
    assert!(b.view().disputed, "the fork stays on record");
    assert_eq!(owners(&b), [B, C]);
    assert_eq!(commit(&mut b, Change::Add { key: X }, &[B], 11), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::Add { key: X }, &[B, C], 11), Ok(Insert::Applied));
}

/// Round 2's mitigation: a fork that no session attested changes nothing.
#[test]
fn unattested_fork_counts_for_nothing() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let c1 = put(&mut b, g, Change::Add { key: B }, &[S], Some((L1, 1)));
    put(&mut b, c1, Change::Leave { key: S }, &[S], Some((L1, 2)));
    let eve = put(&mut b, g, Change::Add { key: EVE }, &[S], None);
    assert_eq!(owners(&b), [B]);
    // With a session of its own on an old copy, the former sole owner still can (the residual).
    b.record(eve, L2, 1);
    assert_eq!(owners(&b), [B, EVE]);
    assert!(b.forks()[0].equivocation());
}

/// OWNR-5: an unattested sock Add at an old state neither revives nor blocks anything.
#[test]
fn unattested_fork_add_blocks_nothing() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    commit(&mut b, Change::Add { key: A }, &[S], 1).unwrap();
    commit(&mut b, Change::Add { key: K }, &[S, A], 2).unwrap();
    commit(&mut b, Change::Leave { key: S }, &[S], 3).unwrap();
    commit(&mut b, Change::Challenge { key: K }, &[A], 4).unwrap();
    commit(&mut b, Change::RemoveDormant { key: K }, &[A], 4 + W).unwrap();
    assert_eq!(owners(&b), [A]);
    let g = b.genesis;
    put(&mut b, g, Change::Add { key: Z }, &[S], None);
    assert_eq!(owners(&b), [A]);
}

/// OWNR-2 in a session: a certificate built on an older head is refused.
#[test]
fn a_session_refuses_stale_certificates() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    commit(&mut b, Change::Add { key: B }, &[S], 1).unwrap();
    let stale = next(&b, g, Change::Add { key: C }, &[S]);
    assert_eq!(b.commit_local(stale, L1, 2), Err(OwnError::Stale));
    assert_eq!(b.len(), 2);
}

/// OWN-1: a merge joins concurrent certificates only, so nobody rolls the chain back.
#[test]
fn merge_cannot_roll_back() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let c1 = put(&mut b, g, Change::Add { key: B }, &[S], Some((L1, 1)));
    let c2 = put(&mut b, c1, Change::Leave { key: S }, &[S], Some((L1, 2)));
    let mut p = vec![g, c1];
    p.sort_unstable();
    let back = Cert { world: b.world, seq: 2, prev: p, change: Change::Merge, sigs: vec![] }.signed(&[S, B]);
    assert_eq!(b.insert(back), Err(OwnError::BadLink));
    let mut p = vec![c1, c2];
    p.sort_unstable();
    let back = Cert { world: b.world, seq: 3, prev: p, change: Change::Merge, sigs: vec![] }.signed(&[B]);
    assert_eq!(b.insert(back), Err(OwnError::BadLink));
    assert_eq!(owners(&b), [B]);
}

/// OWN-2: a seller who left can't come back through an attested fork and a merge.
#[test]
fn leave_sticks_across_forks() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let c1 = put(&mut b, g, Change::Add { key: B }, &[S], Some((L1, 1)));
    put(&mut b, c1, Change::Leave { key: S }, &[S], Some((L1, 2)));
    put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], Some((L2, 1)));
    assert_eq!(owners(&b), [B]);
    assert_eq!(b.insert(merge(&b, &[S])), Err(OwnError::Signers));
    assert_eq!(b.insert(merge(&b, &[B])), Ok(Insert::Applied));
    assert_eq!(owners(&b), [B]);
}

/// OWN-3: a fork from an older, smaller owner set doesn't keep that set's power.
#[test]
fn old_state_fork_needs_the_owners_added_since() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let c1 = put(&mut b, g, Change::Add { key: I }, &[S], Some((L1, 1)));
    let c2 = put(&mut b, c1, Change::Add { key: J }, &[S, I], Some((L1, 2)));
    put(&mut b, c2, Change::SetRule { rule: SignRule::All }, &[S, I, J], Some((L1, 3)));
    // S forks at "Add I" in a session of its own, then removes I there: J never agreed.
    b.record(c1, L2, 1);
    let ch = put(&mut b, c1, Change::Challenge { key: I }, &[S], Some((L2, 2)));
    put(&mut b, ch, Change::RemoveDormant { key: I }, &[S], Some((L2, 2 + W)));
    assert_eq!(owners(&b), [S, I, J]);
}

/// Each copy's owners add independently while disconnected, and the copies join (OWNS-3). A removal
/// still needs the owners the other copy added (OWN-3).
#[test]
fn copies_add_independently_but_remove_only_with_the_other_copy() {
    let (b0, certs) = double_sale();
    let mut b = sold_twice(&b0, &certs);
    put(&mut b, certs[1].id(), Change::Add { key: EVE }, &[B], Some((L1, 9)));
    assert_eq!(owners(&b), [B, C, EVE]);
    let mut b2 = sold_twice(&b0, &certs);
    let e = put(&mut b2, certs[1].id(), Change::Add { key: EVE }, &[B], Some((L1, 9)));
    let ch = put(&mut b2, e, Change::Challenge { key: EVE }, &[B], Some((L1, 10)));
    put(&mut b2, ch, Change::RemoveDormant { key: EVE }, &[B], Some((L1, 10 + W)));
    // B alone can't remove EVE: C, added on the other copy, never agreed.
    assert_eq!(owners(&b2), [B, C, EVE]);
}

/// OWN-5: the same certificates and attestations give the same book in any arrival order.
#[test]
fn arrival_order_never_matters() {
    let (b0, mut certs) = double_sale();
    let b = sold_twice(&b0, &certs);
    let ext = next(&b, certs[1].id(), Change::SetRecovery { owner: B, recovery: Some(R) }, &[B]);
    let m = merge(&b, &[B]);
    let mut p = vec![m.id(), ext.id()];
    p.sort_unstable();
    let m2 = Cert { world: b.world, seq: 4, prev: p, change: Change::Merge, sigs: vec![] }.signed(&[C]);
    certs.extend([ext, m, m2]);
    let mut seen = BTreeSet::new();
    for rot in 0..certs.len() {
        let mut order = certs.clone();
        order.rotate_left(rot);
        let mut bk = b0.clone();
        insert_all(&mut bk, &order).unwrap();
        let mut ids: Vec<CertId> = certs.iter().map(Cert::id).collect();
        ids.rotate_left(rot);
        for id in ids.iter().rev() {
            bk.record(*id, L1, 1);
        }
        assert_eq!(bk.len(), 8);
        seen.insert(bk.digest());
    }
    assert_eq!(seen.len(), 1);
}

#[test]
fn merge_is_causal() {
    let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 6)).unwrap();
    commit(&mut b, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], 1).unwrap();
    commit(&mut b, Change::Leave { key: C }, &[C], 2).unwrap();
    commit(&mut b, Change::Leave { key: B }, &[B], 3).unwrap();
    let fork = head(&b);
    let c_back = put(&mut b, fork, Change::Add { key: C }, &[S], Some((L1, 4)));
    put(&mut b, fork, Change::Add { key: X }, &[S], Some((L2, 4)));
    put(&mut b, c_back, Change::SetRecovery { owner: S, recovery: None }, &[S], Some((L1, 5)));
    let v = b.view();
    assert!(v.disputed);
    assert_eq!(set(&v), [S, C, X]);
    assert!(v.owners.recovery.is_empty());
}

/// OWN-8: honest concurrency is a fork without an equivocator, and loses nobody.
#[test]
fn honest_concurrency_blames_nobody() {
    let mut b = Book::new(Cert::genesis(&[S, B], SignRule::Any, 3)).unwrap();
    let g = b.genesis;
    put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], Some((L1, 1)));
    put(&mut b, g, Change::SetRecovery { owner: B, recovery: Some(R2) }, &[B], Some((L2, 1)));
    assert!(!b.forks()[0].equivocation());
    assert_eq!(b.view().owners.recovery.len(), 2);
    let mut b2 = Book::new(Cert::genesis(&[S, B], SignRule::Any, 3)).unwrap();
    let g = b2.genesis;
    put(&mut b2, g, Change::Add { key: X }, &[S], Some((L1, 1)));
    put(&mut b2, g, Change::Add { key: C }, &[B], Some((L2, 1)));
    assert_eq!(owners(&b2), [S, B, C, X]);
}

#[test]
fn removal_survives_a_fork_by_its_target_under_rule_c() {
    let mut b = Book::new(Cert::genesis(&[S, B, X], SignRule::Threshold(2), 4)).unwrap();
    let g = b.genesis;
    put(&mut b, g, Change::Remove { key: X }, &[S, B], Some((L1, 1)));
    put(&mut b, g, Change::SetRecovery { owner: X, recovery: Some(R) }, &[X], Some((L2, 1)));
    assert_eq!(owners(&b), [S, B]);
}

/// OWNR-7: two owners who remove each other on two copies void both removals.
#[test]
fn mutual_removal_voids_both() {
    let mut b = Book::new(Cert::genesis(&[A, B], SignRule::Threshold(1), 1)).unwrap();
    let g = b.genesis;
    put(&mut b, g, Change::Remove { key: B }, &[A], Some((L1, 1)));
    let c = put(&mut b, g, Change::Remove { key: A }, &[B], Some((L2, 1)));
    put(&mut b, c, Change::Add { key: EVE }, &[B], Some((L2, 2)));
    assert_eq!(owners(&b), [A, B, EVE]);
}

/// OWNR-9, OWNS-4: concurrent rules take the strictest; the owners loosen it again after a merge.
#[test]
fn concurrent_rules_take_the_strictest() {
    let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    put(&mut b, g, Change::SetRule { rule: SignRule::Threshold(1) }, &[S, B], Some((L1, 1)));
    put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], Some((L2, 1)));
    assert_eq!(b.view().owners.rule, SignRule::All);
    let m = merge(&b, &[S]);
    b.insert(m.clone()).unwrap();
    b.record(m.id(), L1, 2);
    assert_eq!(commit(&mut b, Change::SetRule { rule: SignRule::Threshold(1) }, &[S, B], 3), Ok(Insert::Applied));
    assert_eq!(b.view().owners.rule, SignRule::Threshold(1));
}

/// OWNR-4: copies with different signatures are different certificates, and books converge.
#[test]
fn signature_copies_converge() {
    let b0 = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
    let x = next(&b0, b0.genesis, Change::Challenge { key: S }, &[B, C]);
    let x2 = x.clone().signed(&[S]);
    assert_eq!(x.id(), x2.id());
    let (mut b1, mut b2) = (b0.clone(), b0.clone());
    insert_all(&mut b1, &[x.clone(), x2.clone()]).unwrap();
    insert_all(&mut b2, &[x2, x.clone()]).unwrap();
    assert_eq!(b1.digest(), b2.digest());
    assert!(b1.forks().is_empty(), "same change, two signature sets: one certificate");
    assert_eq!(b1.cert(x.id()).unwrap().signers(), [S, B, C].into());
    // Junk signatures don't make a new certificate.
    let mut junk = x.clone();
    junk.sigs.push((EVE, 1));
    assert_eq!(junk.id(), x.id());
}

/// OWNR-8: a duplicate changes nothing, in a session too.
#[test]
fn duplicates_change_nothing() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let c = next(&b, b.genesis, Change::Add { key: B }, &[S]);
    b.commit_local(c.clone(), L1, 1).unwrap();
    let d = b.digest();
    assert_eq!(b.commit_local(c.clone(), L1, 5), Ok(Insert::Duplicate));
    assert_eq!(b.insert(c), Ok(Insert::Duplicate));
    assert_eq!(b.digest(), d);
}

/// OWNR-6: a book is capped, and cloning one shares it until the next insert.
#[test]
fn books_are_capped_and_cheap_to_clone() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let mut refused = None;
    for i in 0..MAX_CERTS as u32 + 1 {
        if let Err(e) = commit(&mut b, Change::SetRecovery { owner: S, recovery: Some(1000 + i) }, &[S], i as u64) {
            refused = Some(e);
            break;
        }
    }
    assert_eq!(refused, Some(OwnError::Full));
    let c = b.clone();
    assert!(Arc::ptr_eq(&b.inner, &c.inner));
}

#[test]
fn tampered_or_reordered_chains_are_refused() {
    let g = Cert::genesis(&[S], SignRule::All, 9);
    let mut b = Book::new(g.clone()).unwrap();
    let c1 = next(&b, b.genesis, Change::Add { key: B }, &[S]);
    b.insert(c1.clone()).unwrap();
    let c2 = next(&b, c1.id(), Change::Leave { key: S }, &[S]);
    let mut t = c1.clone();
    t.change = Change::Add { key: EVE };
    assert_eq!(set(&verify_chain(&g, &[t, c2.clone()], &[], &sessions()).unwrap()), [S]);
    assert_eq!(verify_chain(&g, std::slice::from_ref(&c2), &[], &sessions()).unwrap().heads, [b.genesis]);
    let mut skip = next(&b, c1.id(), Change::Leave { key: S }, &[]);
    skip.seq = 5;
    assert_eq!(b.clone().insert(skip.signed(&[S])), Err(OwnError::BadLink));
    assert_eq!(verify_chain(&g, &[c1.clone(), c2.clone()], &[], &sessions()), verify_chain(&g, &[c2, c1], &[], &sessions()));
}

#[test]
fn orphans_are_not_stored() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let orphan = Cert { world: b.world, seq: 5, prev: vec![12345], change: Change::Add { key: EVE }, sigs: vec![] };
    // Parked (bounded) until its parent arrives; not held, and it counts for nothing.
    assert_eq!(b.insert(orphan), Ok(Insert::Parked));
    assert_eq!(b.len(), 1);
}

/// OWN-9: a prefix in any order, containing a merge or a completed window, verifies.
#[test]
fn fork_proof_prefix_order_and_merges() {
    let (b0, certs) = double_sale();
    let mut b = sold_twice(&b0, &certs);
    let m = merge(&b, &[B]);
    b.insert(m.clone()).unwrap();
    b.record(m.id(), L1, 19);
    let ch = put(&mut b, m.id(), Change::Challenge { key: C }, &[B], Some((L1, 20)));
    let rd = put(&mut b, ch, Change::RemoveDormant { key: C }, &[B], Some((L1, 20 + W)));
    assert_eq!(owners(&b), [B]);
    let x = next(&b, rd, Change::Add { key: I }, &[B]);
    let y = next(&b, rd, Change::Add { key: J }, &[B]);
    let p = ForkProof { world: b.world, a: x, b: y };
    let mut prefix = b.prefix_of(rd);
    assert!(p.verify(&prefix));
    prefix.reverse();
    assert!(p.verify(&prefix));
    assert_eq!(p.signed_both(), [B].into());
}

#[test]
fn threshold_rule_c() {
    let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
    assert_eq!(commit(&mut b, Change::SetRule { rule: SignRule::Threshold(2) }, &[S, B], 1), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::SetRule { rule: SignRule::Threshold(2) }, &[S, B, C], 1), Ok(Insert::Applied));
    assert_eq!(commit(&mut b, Change::Add { key: X }, &[S], 2), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::Add { key: X }, &[S, B], 2), Ok(Insert::Applied));
    assert_eq!(commit(&mut b, Change::Remove { key: C }, &[S, C], 3), Err(OwnError::Signers));
    assert_eq!(commit(&mut b, Change::Remove { key: C }, &[S, B], 3), Ok(Insert::Applied));
    assert_eq!(owners(&b), [S, B, X]);
}

#[test]
fn wire_round_trip_and_one_encoding() {
    let (b0, mut certs) = double_sale();
    certs.push(b0.cert(b0.genesis).unwrap().clone());
    certs.push(Cert { world: 5, seq: 9, prev: vec![1, 2], change: Change::Merge, sigs: vec![] }.signed(&[S]));
    for change in [
        Change::SetRule { rule: SignRule::Threshold(3) },
        Change::SetRecovery { owner: S, recovery: Some(0) },
        Change::SetRecovery { owner: S, recovery: None },
        Change::Remove { key: X },
        Change::Challenge { key: X },
        Change::RemoveDormant { key: X },
        Change::StartRecover { owner: S, new_key: N },
        Change::FinishRecover { owner: S },
    ] {
        certs.push(Cert { world: 5, seq: 1, prev: vec![1], change, sigs: vec![] }.signed(&[S, B]));
    }
    for c in certs {
        let mut w = c.to_words();
        assert_eq!(Cert::from_words(&w), Some(c));
        w.push(7);
        assert_eq!(Cert::from_words(&w), None);
    }
    assert_eq!(Cert::from_words(&[1, 2, 3]), None);
}

// ---- review round 3 (OWNS-*), from @design-reviewer's repros (runs13/own3-reviewer-tests.patch),
// asserting the safe outcome. Failing first at 2daf8ab: see commit 'OWNS failing-first'.

const L9: Lineage = 9;

/// OWNS-5b: co-owner B alone floods the book with unattested forks; honest commits still go through.
#[test]
fn owns5b_unattested_flood_is_capped_per_signer() {
    let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let mut refused = 0;
    for i in 0..MAX_CERTS as u32 {
        let c = next(&b, g, Change::SetRecovery { owner: B, recovery: Some(5000 + i) }, &[B]);
        if b.insert(c) != Ok(Insert::Applied) {
            refused += 1;
        }
    }
    assert!(refused > 0 && b.len() < 64, "B alone stored {} certs", b.len());
    assert_eq!(b.commit_local(next(&b, g, Change::Add { key: X }, &[S, B]), L1, 10), Ok(Insert::Applied));
}

/// OWNS-5c: a relay's subsets of one cert's signatures are one certificate.
#[test]
fn owns5c_signature_subsets_are_one_certificate() {
    let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::Any, 1)).unwrap();
    let g = b.genesis;
    let full = next(&b, g, Change::Add { key: X }, &[S, B, C]);
    let mut ids = BTreeSet::new();
    for mask in 1u32..8 {
        let mut c = full.clone();
        c.sigs = full.sigs.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, s)| *s).collect();
        ids.insert(c.id());
        assert!(b.insert(c).is_ok());
    }
    assert_eq!(ids.len(), 1);
    assert_eq!(b.len(), 2);
}

/// OWNS-1c: after a recovery completed in L1, the old key's cancel attested by another lineage at a
/// frame it picks changes nothing: frames are compared within one lineage only.
#[test]
fn owns1c_cross_lineage_cancel_does_not_undo_a_recovery() {
    for at in [(L1, 3 + RF), (L2, 5)] {
        let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
        commit(&mut b, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], 1).unwrap();
        commit(&mut b, Change::StartRecover { owner: S, new_key: N }, &[R], 2).unwrap();
        let st = head(&b);
        commit(&mut b, Change::FinishRecover { owner: S }, &[R], 2 + RF).unwrap();
        put(&mut b, st, Change::SetRecovery { owner: S, recovery: Some(EVE) }, &[S], Some(at));
        assert_eq!(owners(&b), [B, N], "cancel attested at {at:?}");
    }
}

/// OWNS-1d: residual without a trust root (D87). An active co-owner removed through a window that the
/// challenger ran in a session of its own.
#[test]
#[ignore = "residual until the trust root is decided (D87)"]
fn owns1d_private_window_never_removes_an_active_co_owner() {
    let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
    commit(&mut b, Change::Challenge { key: B }, &[S], 1).unwrap();
    let ch = head(&b);
    commit(&mut b, Change::SetRecovery { owner: B, recovery: Some(R) }, &[B], 2).unwrap();
    b.record(ch, L2, 1_000_000);
    let rd = put(&mut b, ch, Change::RemoveDormant { key: B }, &[S], Some((L2, 1_000_000 + W)));
    put(&mut b, rd, Change::Add { key: EVE }, &[S], Some((L2, 1_000_001 + W)));
    assert_eq!(owners(&b), [S, B]);
}

/// OWNS-1a: residual without a trust root (D87). A stolen recovery key replaces an active owner in a
/// session of its own.
#[test]
#[ignore = "residual until the trust root is decided (D87)"]
fn owns1a_stolen_recovery_key_never_replaces_an_active_owner() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let r = put(&mut b, g, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], Some((L1, 1)));
    put(&mut b, r, Change::SetRule { rule: SignRule::All }, &[S], Some((L1, 1000)));
    let st = put(&mut b, r, Change::StartRecover { owner: S, new_key: N }, &[R], Some((L9, 1 << 50)));
    let fin = put(&mut b, st, Change::FinishRecover { owner: S }, &[R], Some((L9, (1 << 50) + RF)));
    put(&mut b, fin, Change::Add { key: EVE }, &[N], Some((L9, (1 << 50) + RF + 1)));
    assert_eq!(owners(&b), [S]);
}

/// OWNS-2: unattested removals void nothing.
#[test]
fn owns2_unattested_counter_removal_voids_nothing() {
    let mut b = Book::new(Cert::genesis(&[A, K], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let ch = put(&mut b, g, Change::Challenge { key: K }, &[A], Some((L1, 1)));
    put(&mut b, ch, Change::RemoveDormant { key: K }, &[A], Some((L1, 1 + W)));
    let c2 = put(&mut b, g, Change::Challenge { key: A }, &[K], None);
    put(&mut b, c2, Change::RemoveDormant { key: A }, &[K], None);
    assert_eq!(owners(&b), [A]);
}

/// OWNS-3: an old-state fork doesn't void a later addition (the fork's own addition joins the union:
/// the stated residual, shown as disputed).
#[test]
fn owns3_old_fork_does_not_void_later_additions() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    commit(&mut b, Change::Add { key: A }, &[S], 1).unwrap();
    commit(&mut b, Change::Add { key: K }, &[S, A], 2).unwrap();
    commit(&mut b, Change::Leave { key: S }, &[S], 3).unwrap();
    put(&mut b, g, Change::Add { key: Z }, &[S], Some((L2, 1)));
    let v = b.view();
    assert!(v.owners.owners.contains(&A) && v.owners.owners.contains(&K));
    assert!(v.disputed);
}

/// OWNS-4: an old-state fork doesn't loosen the rule; after a merge, one co-owner still can't add alone.
#[test]
fn owns4_old_fork_does_not_loosen_the_rule() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    commit(&mut b, Change::Add { key: B }, &[S], 1).unwrap();
    commit(&mut b, Change::Add { key: C }, &[S, B], 2).unwrap();
    commit(&mut b, Change::Leave { key: S }, &[S], 3).unwrap();
    put(&mut b, g, Change::SetRule { rule: SignRule::Any }, &[S], Some((L2, 1)));
    assert_eq!(b.view().owners.rule, SignRule::All);
    let m = merge(&b, &[B]);
    b.insert(m.clone()).unwrap();
    b.record(m.id(), L1, 9);
    assert_eq!(commit(&mut b, Change::Add { key: EVE }, &[B], 10), Err(OwnError::Signers));
}

/// OWNS-5: padding a certificate with signatures of keys that aren't owners makes no new certificate.
#[test]
fn owns5_signature_padding_makes_no_copies() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let c = next(&b, g, Change::Add { key: B }, &[S]);
    for i in 0..64u32 {
        assert!(b.insert(c.clone().signed(&[10_000 + i])).is_ok());
    }
    assert_eq!(b.len(), 2);
    assert_eq!(b.insert(next(&b, g, Change::Add { key: C }, &[S])), Ok(Insert::Applied));
}

/// OWNS-8: "disputed" comes from the fork history, so the residual's sock can't merge it away.
#[test]
fn owns8_sock_merge_does_not_clear_the_dispute() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let c1 = put(&mut b, g, Change::Add { key: B }, &[S], Some((L1, 1)));
    put(&mut b, c1, Change::Leave { key: S }, &[S], Some((L1, 2)));
    let eve = put(&mut b, g, Change::Add { key: EVE }, &[S], None);
    b.record(eve, L2, 1);
    assert!(b.view().disputed);
    let m = merge(&b, &[EVE]);
    b.insert(m.clone()).unwrap();
    b.record(m.id(), L2, 2);
    assert!(b.view().disputed);
}

/// OWNS-10, OWNS-11: junk in a chain is skipped, not fatal; an unknown id has an empty prefix.
#[test]
fn junk_is_skipped_and_unknown_prefix_is_empty() {
    let g = Cert::genesis(&[S], SignRule::All, 9);
    let b = Book::new(g.clone()).unwrap();
    let c1 = next(&b, b.genesis, Change::Add { key: B }, &[S]);
    let junk = next(&b, b.genesis, Change::Add { key: EVE }, &[EVE]);
    assert!(verify_chain(&g, &[junk, c1], &[], &sessions()).is_ok());
    assert!(b.prefix_of(12345).is_empty());
}

/// OWNS-12: a session never builds on a certificate that doesn't count (it may adopt one that does,
/// OWNT-4).
#[test]
fn owns12_session_builds_only_on_counting_certificates() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    let g = b.genesis;
    let other = put(&mut b, g, Change::Add { key: B }, &[S], None);
    let c = next(&b, other, Change::Add { key: C }, &[S, B]);
    assert_eq!(b.commit_local(c, L1, 2), Err(OwnError::Stale));
}

/// Hostile input: mutated encodings of real certificates, decoded and inserted, then the view.
#[test]
fn reviewer_mutated_words_never_panic() {
    let (b0, certs) = double_sale();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut decoded = 0;
    for round in 0..20_000 {
        let c = &certs[round % certs.len()];
        let mut w = c.to_words();
        for _ in 0..1 + rnd() % 3 {
            let i = (rnd() as usize) % w.len();
            w[i] = match rnd() % 4 {
                0 => rnd(),
                1 => w[i] ^ (1 << (rnd() % 64)),
                2 => u64::MAX,
                _ => 0,
            };
        }
        if let Some(c) = Cert::from_words(&w) {
            decoded += 1;
            let mut b = b0.clone();
            let _ = b.insert(c);
            let _ = b.view();
            let _ = b.digest();
        }
    }
    println!("REPRO fuzz: {decoded} of 20000 mutated encodings decoded; no panic");
}

// ---- review round 4 (OWNT-*), from @design-reviewer's repros (runs13/own4-reviewer-tests.patch),
// asserting the safe outcome. Failing first at 7acc6e2: see commit 'OWNT failing-first'.

fn s9() -> StaticEpochs {
    let mut s = sessions();
    s.worlds.insert(L9, (vec![99], 0));
    s
}

#[test]
#[ignore = "deferred: window and recovery machinery, removed under OT(a); see D87"]
fn rv4_owt1_attest_claims_absent_signer() {
    let mut b = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
    commit(&mut b, Change::Challenge { key: S }, &[B, C], 5).unwrap();
    commit(&mut b, Change::RemoveDormant { key: S }, &[B, C], 5 + W).unwrap();
    let rd = head(&b);
    commit(&mut b, Change::Add { key: X }, &[B, C], 6 + W).unwrap();
    let before = owners(&b);
    let ok = b.attest(&Attest::make(b.world, rd, L9, 1, &[S, B, C].into(), &[99]), &s9());
    println!("REPRO OWNT-1 (no key, one-member session claims S signed the RemoveDormant): attest accepted {ok}; owners {before:?} -> {:?}", owners(&b));
}

#[test]
#[ignore = "deferred: window and recovery machinery, removed under OT(a); see D87"]
fn rv4_owt1b_reattest_undoes_recovery() {
    let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
    commit(&mut b, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], 1).unwrap();
    commit(&mut b, Change::StartRecover { owner: S, new_key: N }, &[R], 2).unwrap();
    commit(&mut b, Change::SetRecovery { owner: B, recovery: Some(R2) }, &[B], 3).unwrap();
    let y = head(&b);
    commit(&mut b, Change::FinishRecover { owner: S }, &[R], 2 + RF).unwrap();
    commit(&mut b, Change::Add { key: X }, &[B, N], 3 + RF).unwrap();
    let before = owners(&b);
    let ok = b.attest(&Attest::make(b.world, y, L9, 7, &[S, B].into(), &[99]), &s9());
    println!("REPRO OWNT-1b (re-attest B's cert inside the delay with S added): accepted {ok}; owners {before:?} -> {:?}", owners(&b));
}

#[test]
fn rv4_owt2_minted_keys_fill_the_cap() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    commit(&mut b, Change::Add { key: B }, &[S], 1).unwrap();
    commit(&mut b, Change::Leave { key: S }, &[S], 2).unwrap();
    let (mut p, mut seq, mut me) = (b.genesis, 0, S);
    for k in 5000..5000 + MAX_CERTS as Key {
        let add = Cert { world: b.world, seq: seq + 1, prev: vec![p], change: Change::Add { key: k }, sigs: vec![] }.signed(&[me]);
        let _ = b.insert(add.clone());
        let lv = Cert { world: b.world, seq: seq + 2, prev: vec![add.id()], change: Change::Leave { key: me }, sigs: vec![] }.signed(&[me]);
        let _ = b.insert(lv.clone());
        (p, seq, me) = (lv.id(), seq + 2, k);
    }
    assert!(b.len() <= 3 + UNATTESTED_PER_KEY, "{} certs stored", b.len());
    assert_eq!(commit(&mut b, Change::Add { key: C }, &[B], 3), Ok(Insert::Applied));
}

#[test]
#[ignore = "deferred: window and recovery machinery, removed under OT(a); see D87"]
fn rv4_owt3_recovery_key_fills_the_session() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    commit(&mut b, Change::SetRecovery { owner: S, recovery: Some(R) }, &[S], 1).unwrap();
    let mut n = 0;
    let mut stop = None;
    for i in 0..MAX_CERTS as u32 {
        match commit(&mut b, Change::StartRecover { owner: S, new_key: 5000 + i }, &[R], 2 + i as u64) {
            Ok(_) => n += 1,
            Err(e) => {
                stop = Some(e);
                break;
            }
        }
    }
    let r = commit(&mut b, Change::SetRecovery { owner: S, recovery: Some(R2) }, &[S], 5000);
    println!("REPRO OWNT-3 (stolen recovery key commits StartRecovers in the session): {n} committed (stopped {stop:?}); owner's cancel -> {r:?}");
}

#[test]
fn rv4_owt4_new_lineage_and_merge() {
    let mut b = Book::new(Cert::genesis(&[S], SignRule::All, 1)).unwrap();
    commit(&mut b, Change::Add { key: B }, &[S], 1).unwrap();
    commit(&mut b, Change::Leave { key: S }, &[S], 2).unwrap();
    let c = next(&b, head(&b), Change::Add { key: C }, &[B]);
    assert_eq!(b.commit_local(c, L2, 3), Ok(Insert::Applied), "a second session adopts the chain");
    let (b0, certs) = double_sale();
    let mut b = sold_twice(&b0, &certs);
    let m = merge(&b, &[B]);
    assert_eq!(b.commit_local(m, L1, 10), Ok(Insert::Applied), "a session commits a merge");
    let mut b = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
    let c = next(&b, b.genesis, Change::Add { key: X }, &[S, B]);
    b.insert(c.clone()).unwrap();
    assert_eq!(b.commit_local(c, L1, 1), Ok(Insert::Applied), "a relayed cert is committed");
    assert_eq!(owners(&b), [S, B, X]);
}

#[test]
fn rv4_owt5_attests_after_certs() {
    let mut l = Book::new(Cert::genesis(&[S, B], SignRule::All, 1)).unwrap();
    for i in 0..20 {
        commit(&mut l, Change::SetRecovery { owner: S, recovery: Some(1000 + i) }, &[S], i as u64 + 1).unwrap();
    }
    let h = head(&l);
    let x = next(&l, h, Change::Add { key: X }, &[S, B]);
    let y = next(&l, h, Change::Add { key: C }, &[S, B]);
    assert!(ForkProof { world: l.world, a: x, b: y }.verify(&l.prefix_of(h)));
    let mut b = Book::new(l.cert(l.genesis).unwrap().clone()).unwrap();
    for c in l.prefix_of(h).into_iter().filter(|c| c.seq > 0) {
        let _ = b.insert(c);
    }
    for a in l.export(L1, &[1, 2, 3]) {
        b.attest(&a, &sessions());
    }
    assert_eq!(b.view().owners, l.view().owners);
}

#[test]
fn rv4_owt6_equal_frame_attests() {
    let mut b0 = Book::new(Cert::genesis(&[S, B, C], SignRule::All, 1)).unwrap();
    commit(&mut b0, Change::Challenge { key: S }, &[B, C], 5).unwrap();
    let x = next(&b0, head(&b0), Change::SetRecovery { owner: B, recovery: Some(R) }, &[B, S]);
    b0.insert(x.clone()).unwrap();
    let a1 = Attest::make(b0.world, x.id(), L9, 7, &[B].into(), &[99]);
    let a2 = Attest::make(b0.world, x.id(), L9, 7, &[B, S].into(), &[99]);
    let (mut p, mut q) = (b0.clone(), b0.clone());
    p.attest(&a1, &s9());
    p.attest(&a2, &s9());
    q.attest(&a2, &s9());
    q.attest(&a1, &s9());
    assert_eq!(p.view(), q.view());
    assert_eq!(p.digest(), q.digest());
}

#[test]
fn rv4_owt7_rule_join_order() {
    let mut b = Book::new(Cert::genesis(&[A, B, C], SignRule::Threshold(1), 1)).unwrap();
    let g = b.genesis;
    put(&mut b, g, Change::SetRule { rule: SignRule::Any }, &[A, B, C], Some((L1, 1)));
    put(&mut b, g, Change::SetRecovery { owner: C, recovery: Some(R) }, &[C], Some((L2, 1)));
    let m = merge(&b, &[A]);
    b.insert(m.clone()).unwrap();
    b.record(m.id(), L1, 2);
    assert_eq!(commit(&mut b, Change::Remove { key: B }, &[A], 3), Err(OwnError::NotAllowed));
    assert_eq!(owners(&b), [A, B, C]);
}

#[test]
fn rv4_owt8_mutual_removal_dispute() {
    let mut b = Book::new(Cert::genesis(&[A, B], SignRule::Threshold(1), 1)).unwrap();
    let g = b.genesis;
    put(&mut b, g, Change::Remove { key: B }, &[A], Some((L1, 1)));
    put(&mut b, g, Change::Remove { key: A }, &[B], Some((L2, 1)));
    let v = b.view();
    assert_eq!(set(&v), [A, B]);
    assert!(v.disputed);
}
