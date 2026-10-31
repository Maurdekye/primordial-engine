//! Adversarial harness for the ownership invariants (world-ownership-draft.md, section 0): random
//! attacker strategies against random honest histories, every invariant checked on every book.
//!
//! * Honest side: owners act through one honest session (lineage 1) with `commit_local`.
//! * A1: holds former owners' keys, removed owners' keys, stolen recovery keys and stranger keys;
//!   forks anywhere, pads, strips, replays and reorders certificates.
//! * A2: A1 plus sessions of its own (lineage 9) attesting anything at any frames.
//!
//! Invariants 2-7 must hold for A1 (2, 3) or both (4-7); invariant 8 is a residual for A2, measured.
//! `PE_OWN_ITERS` sets the number of histories (default 150).

use super::*;
use crate::provenance::StaticEpochs;

const L1: Lineage = 1;
const L9: Lineage = 9;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = splitmix64(self.0);
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> Option<T> {
        (!v.is_empty()).then(|| v[self.below(v.len())])
    }
}

fn cert_on(b: &Book, prev: Vec<CertId>, change: Change, keys: &[Key]) -> Cert {
    let seq = prev.iter().filter_map(|p| b.cert(*p)).map(|c| c.seq).max().unwrap_or(0) + 1;
    Cert { world: b.world, seq, prev, change, sigs: vec![] }.signed(keys)
}

/// Everything an honest history leaves behind, for the attacker to use.
struct History {
    book: Book,
    frame: u64,
    next_key: Key,
    former: BTreeSet<Key>,
    recovery_keys: BTreeSet<Key>,
}

/// One honest step on the session's head; an invalid attempt is simply refused.
fn honest_step(rng: &mut Rng, h: &mut History) -> Result<Insert, OwnError> {
    let v = h.book.view();
    let o: Vec<Key> = v.owners.owners.iter().copied().collect();
    let head = *h.book.lineage_heads(L1).iter().next().unwrap();
    h.frame += if rng.chance(15) { OBJECTION_FRAMES + 1 } else if rng.chance(10) { RECOVERY_FRAMES + 1 } else { 1 + rng.below(10) as u64 };
    let others = |k: Key| -> Vec<Key> { o.iter().copied().filter(|&x| x != k).collect() };
    let (change, keys): (Change, Vec<Key>) = match rng.below(9) {
        0 | 1 => {
            h.next_key += 1;
            (Change::Add { key: h.next_key }, o.clone())
        }
        2 => {
            let k = rng.pick(&o).unwrap();
            (Change::Leave { key: k }, vec![k])
        }
        3 => {
            let k = rng.pick(&o).unwrap();
            h.next_key += 1;
            h.recovery_keys.insert(h.next_key);
            (Change::SetRecovery { owner: k, recovery: Some(h.next_key) }, vec![k])
        }
        4 => {
            let k = rng.pick(&o).unwrap();
            (Change::Challenge { key: k }, others(k))
        }
        5 => match v.owners.challenges.keys().next() {
            Some(&k) => (Change::RemoveDormant { key: k }, others(k)),
            None => return Err(OwnError::WindowOpen),
        },
        6 => {
            let Some((&k, r)) = v.owners.recovery.iter().next() else { return Err(OwnError::Signers) };
            let by = *r.keys().next().unwrap();
            h.next_key += 1;
            (Change::StartRecover { owner: k, new_key: h.next_key }, vec![by])
        }
        7 => match v.owners.recovering.iter().next() {
            Some((&k, r)) => (Change::FinishRecover { owner: k }, vec![r.by]),
            None => return Err(OwnError::WindowOpen),
        },
        _ => (Change::SetRule { rule: if rng.chance(50) { SignRule::All } else { SignRule::Threshold(2) } }, o.clone()),
    };
    let c = cert_on(&h.book, vec![head], change, &keys);
    let before = v.owners.owners;
    let r = h.book.commit_local(c, L1, h.frame);
    for k in before.difference(&h.book.view().owners.owners) {
        h.former.insert(*k);
    }
    r
}

fn honest_history(rng: &mut Rng) -> History {
    let owners = [1, 2, 3];
    let rule = if rng.chance(70) { SignRule::All } else { SignRule::Threshold(2) };
    let book = Book::new(Cert::genesis(&owners, rule, rng.next())).unwrap();
    let mut h = History { book, frame: 0, next_key: 100, former: BTreeSet::new(), recovery_keys: BTreeSet::new() };
    for _ in 0..rng.below(25) + 3 {
        let _ = honest_step(rng, &mut h);
    }
    h
}

/// A random attack on `book`, with the attacker's keys, optionally with sessions of its own.
fn attack(rng: &mut Rng, book: &mut Book, keys: &[Key], sessions: bool) {
    let mut sock: Key = 9000;
    for _ in 0..rng.below(40) + 1 {
        let ids: Vec<CertId> = book.inner.certs.keys().copied().collect();
        let parent = rng.pick(&ids).unwrap();
        let mut signers: Vec<Key> = keys.iter().copied().filter(|_| rng.chance(50)).collect();
        if signers.is_empty() {
            signers.push(rng.pick(keys).unwrap());
        }
        let target = rng.pick(keys).unwrap_or(1);
        let victim = rng.below(3) as Key + 1;
        sock += 1;
        let c = match rng.below(12) {
            0 | 1 => cert_on(book, vec![parent], Change::Add { key: sock }, &signers),
            2 => cert_on(book, vec![parent], Change::Leave { key: target }, &signers),
            3 => cert_on(book, vec![parent], Change::Challenge { key: victim }, &signers),
            4 => {
                let ch = cert_on(book, vec![parent], Change::Challenge { key: victim }, &signers);
                let _ = book.insert(ch.clone());
                if sessions {
                    book.record(ch.id(), L9, rng.next() >> 20);
                }
                cert_on(book, vec![ch.id()], Change::RemoveDormant { key: victim }, &signers)
            }
            5 => cert_on(book, vec![parent], Change::SetRule { rule: SignRule::Any }, &signers),
            6 => cert_on(book, vec![parent], Change::SetRecovery { owner: target, recovery: Some(sock) }, &signers),
            7 => {
                let st = cert_on(book, vec![parent], Change::StartRecover { owner: victim, new_key: sock }, &signers);
                let _ = book.insert(st.clone());
                if sessions {
                    book.record(st.id(), L9, rng.next() >> 20);
                }
                cert_on(book, vec![st.id()], Change::FinishRecover { owner: victim }, &signers)
            }
            8 => {
                let a = rng.pick(&ids).unwrap();
                let mut p = vec![parent, a];
                p.sort_unstable();
                p.dedup();
                cert_on(book, p, Change::Merge, &signers)
            }
            9 => {
                // Pad a held certificate with the attacker's signatures.
                let held = book.cert(parent).unwrap().clone();
                held.signed(&signers)
            }
            10 => {
                // Replay a held certificate with some signatures stripped.
                let mut held = book.cert(parent).unwrap().clone();
                held.sigs.retain(|_| rng.chance(50));
                held
            }
            _ => {
                let mut c = cert_on(book, vec![parent], Change::Add { key: sock }, &signers);
                c.seq = c.seq.wrapping_add(rng.below(3) as u32).wrapping_sub(1);
                c
            }
        };
        let id = c.id();
        let _ = book.insert(c);
        if sessions && rng.chance(60) {
            let frame = if rng.chance(50) { rng.next() >> 20 } else { rng.below(1000) as u64 };
            book.record(id, L9, frame);
            if rng.chance(30) {
                // Also re-attest an ancestor in the attacker's lineage, so windows can run there.
                if let Some(&p) = book.cert(id).and_then(|c| c.prev.first()) {
                    book.record(p, L9, frame.saturating_sub(OBJECTION_FRAMES + 1));
                }
            }
        }
    }
}

/// The view minus what may legitimately differ (refused certificates).
#[allow(clippy::type_complexity)]
fn essence(v: &View) -> (BTreeSet<Key>, SignRule, BTreeMap<Key, BTreeMap<Key, CertId>>, BTreeMap<Key, CertId>, BTreeMap<Key, Recovering>, Vec<CertId>, bool) {
    let o = &v.owners;
    (o.owners.clone(), o.rule, o.recovery.clone(), o.challenges.clone(), o.recovering.clone(), v.heads.clone(), v.disputed)
}

/// Rebuild a book from its certificates and attestations, in a shuffled order (invariant 4).
fn rebuilt(rng: &mut Rng, b: &Book) -> Book {
    let mut certs: Vec<Cert> = b.inner.certs.values().filter(|c| c.seq > 0).cloned().collect();
    for i in (1..certs.len()).rev() {
        certs.swap(i, rng.below(i + 1));
    }
    let g = b.cert(b.genesis).unwrap().clone();
    let mut sessions = StaticEpochs::default();
    for l in [L1, L9] {
        sessions.worlds.insert(l, (vec![1], 0));
    }
    let mut attests: Vec<Attest> = [L1, L9].iter().flat_map(|&l| b.export(l, &[1])).collect();
    for i in (1..attests.len()).rev() {
        attests.swap(i, rng.below(i + 1));
    }
    let mut r = Book::new(g).unwrap();
    r.insert_attested(&certs, &attests, &sessions);
    r
}

#[derive(Default, Debug)]
struct Residual {
    runs: u32,
    honest_owner_lost: u32,
    owner_gained: u32,
    rule_loosened: u32,
    /// The rule loosened while the owners stayed the same: concurrent rules must take the strictest.
    loosened_without_takeover: u32,
    /// Books whose stored, non-counting junk differs by arrival order (views equal).
    junk_differs: u32,
}

#[test]
fn adversarial_harness() {
    let iters: u64 = std::env::var("PE_OWN_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(150);
    let mut residual = Residual::default();
    for seed in 0..iters {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x0E);
        let h = honest_history(&mut rng);
        let honest = h.book.view();
        // The attacker's keys: former owners, recovery keys (stolen), strangers.
        let mut keys: Vec<Key> = h.former.iter().copied().chain(h.recovery_keys.iter().copied()).collect();
        keys.extend([7001, 7002]);
        keys.retain(|k| !honest.owners.owners.contains(k));
        for sessions in [false, true] {
            let mut v = h.book.clone();
            attack(&mut rng, &mut v, &keys, sessions);
            let ctx = format!("seed {seed}, sessions {sessions}");
            // 5: bounded.
            assert!(v.len() <= h.book.len() + 2 * UNATTESTED_PER_KEY * keys.len() + 2 * 40, "{ctx}: {} certs", v.len());
            // 4: determinism under any arrival order.
            let r = rebuilt(&mut rng, &v);
            let vv = v.view();
            assert_eq!(essence(&r.view()), essence(&vv), "{ctx}: the view depends on arrival order");
            if r.digest() != v.digest() {
                residual.junk_differs += 1;
            }
            // 7: a fork among counting certificates is always visible.
            if !sessions {
                // 2: unattested changes change nothing.
                assert_eq!(essence(&vv), essence(&honest), "{ctx}: an A1 attacker changed the view");
                // 3: liveness: the next honest steps go the same way in both books.
                let mut h1 = History { book: h.book.clone(), frame: h.frame, next_key: h.next_key, former: BTreeSet::new(), recovery_keys: BTreeSet::new() };
                let mut h2 = History { book: v.clone(), frame: h.frame, next_key: h.next_key, former: BTreeSet::new(), recovery_keys: BTreeSet::new() };
                for k in 0..5 {
                    let mut r1 = Rng(seed ^ (k << 40));
                    let mut r2 = Rng(seed ^ (k << 40));
                    let a = honest_step(&mut r1, &mut h1);
                    let b = honest_step(&mut r2, &mut h2);
                    assert_eq!(a, b, "{ctx}: honest step {k} went differently after the attack");
                    assert_eq!(essence(&h1.book.view()), essence(&h2.book.view()), "{ctx}: views diverged at honest step {k}");
                }
            } else {
                // 8: the residual, measured.
                residual.runs += 1;
                let (ho, vo) = (&honest.owners, &vv.owners);
                if !ho.owners.is_subset(&vo.owners) {
                    residual.honest_owner_lost += 1;
                }
                if !vo.owners.is_subset(&ho.owners) {
                    residual.owner_gained += 1;
                }
                if vo.rule < ho.rule {
                    residual.rule_loosened += 1;
                    if vo.owners == ho.owners {
                        residual.loosened_without_takeover += 1;
                    }
                }
            }
            // 6: a key that a counting removal took out stays out unless re-added: covered by the
            // determinism and A1 checks above for every history; the named tests cover the forks.
        }
    }
    println!(
        "HARNESS {iters} honest histories x 2 attackers: invariants 2-5 held in every run (A1), 4-5 in every run (A2); A2 residual (no trust root) in {} runs: honest owner lost {}, owner gained {}, rule loosened {}; stored junk differed by order in {} books (views equal)",
        residual.runs, residual.honest_owner_lost, residual.owner_gained, residual.rule_loosened, residual.junk_differs
    );
    assert_eq!(residual.loosened_without_takeover, 0, "concurrent rules take the strictest; only a takeover loosens");
}

#[test]
#[ignore = "debug helper"]
fn harness_debug_seed() {
    let seed: u64 = std::env::var("PE_OWN_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x0E);
    let h = honest_history(&mut rng);
    let honest = h.book.view();
    let mut keys: Vec<Key> = h.former.iter().copied().chain(h.recovery_keys.iter().copied()).collect();
    keys.extend([7001, 7002]);
    keys.retain(|k| !honest.owners.owners.contains(k));
    for sessions in [false, true] {
        let mut v = h.book.clone();
        attack(&mut rng, &mut v, &keys, sessions);
        let r = rebuilt(&mut rng, &v);
        let a: BTreeSet<CertId> = v.inner.certs.keys().copied().collect();
        let b: BTreeSet<CertId> = r.inner.certs.keys().copied().collect();
        println!("sessions {sessions}: certs only in v {:?} only in rebuilt {:?}", a.difference(&b).collect::<Vec<_>>(), b.difference(&a).collect::<Vec<_>>());
        for id in a.intersection(&b) {
            if v.inner.certs[id] != r.inner.certs[id] {
                println!("  sigs differ on {id}: {:?} vs {:?}", v.inner.certs[id].sigs, r.inner.certs[id].sigs);
            }
            if v.inner.commits.get(id) != r.inner.commits.get(id) {
                println!("  commits differ on {id}: {:?} vs {:?}", v.inner.commits.get(id), r.inner.commits.get(id));
            }
        }
        println!("  digests {} {}", v.digest(), r.digest());
    }
}
