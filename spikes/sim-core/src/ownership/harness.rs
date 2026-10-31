//! Adversarial harness for the ownership invariants (world-ownership-draft.md, section 0): random
//! attacker strategies against random honest histories, every invariant checked on every book.
//!
//! * Honest side: owners act through sessions with `commit_local`. Usually one lineage; sometimes the
//!   world is copied, both copies play on in two lineages, and the copies meet and merge.
//! * A1: holds former owners' keys, removed owners' keys, stolen recovery keys and stranger keys;
//!   forks anywhere, mints key chains, floods, pads, strips, replays and reorders certificates.
//! * A2: A1 plus sessions of its own (lineages 8 and 9) attesting anything at any frames, with
//!   signer subsets the certificates really carry. Forged signer sets (OWNT-1) and the window and
//!   recovery machinery are deferred with the trust root (D87): removed under OT(a).
//! * Relay: holds no key that matters; sends orphans with made-up parents, unsigned and stranger-
//!   signed certificates, stripped and partial copies, and attestations before their certificates.
//!
//! Must hold: invariants 1-7 for A1; 1, 4, 5 and 7 for A2, and 6 for Leave. Invariant 8 (A2) is
//! the residual, measured. `PE_OWN_ITERS` sets the number of histories (default 150), `PE_OWN_FROM`
//! the first seed (default 0). About one in a hundred is long (several hundred attested
//! certificates).

use super::*;
use crate::provenance::StaticEpochs;

const L1: Lineage = 1;
const L2: Lineage = 2;
const L8: Lineage = 8;
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

fn sessions() -> StaticEpochs {
    let mut s = StaticEpochs::default();
    for l in [L1, L2, L8, L9] {
        s.worlds.insert(l, (vec![1], 0));
    }
    s
}

fn cert_on(b: &Book, prev: Vec<CertId>, change: Change, keys: &[Key]) -> Cert {
    let seq = prev.iter().map(|p| b.cert(*p).or_else(|| b.inner.parked.get(p)).map_or(0, |c| c.seq)).max().unwrap_or(0) + 1;
    Cert { world: b.world, seq, prev, change, sigs: vec![] }.signed(keys)
}

/// Everything an honest history leaves behind, for the attacker to use.
#[derive(Clone)]
struct History {
    book: Book,
    frame: u64,
    next_key: Key,
    former: BTreeSet<Key>,
    recovery_keys: BTreeSet<Key>,
}

/// One honest step in `lineage` on a counting head; an invalid attempt is simply refused.
fn honest_step(rng: &mut Rng, h: &mut History, lineage: Lineage) -> Result<Insert, OwnError> {
    let v = h.book.view();
    let o: Vec<Key> = v.owners.owners.iter().copied().collect();
    h.frame += if rng.chance(15) { OBJECTION_FRAMES + 1 } else if rng.chance(10) { RECOVERY_FRAMES + 1 } else { 1 + rng.below(10) as u64 };
    if v.heads.len() > 1 {
        // Copies met: merge them first.
        let m = cert_on(&h.book, v.heads.clone(), Change::Merge, &o);
        return h.book.commit_local(m, lineage, h.frame);
    }
    let head = v.heads[0];
    let others = |k: Key| -> Vec<Key> { o.iter().copied().filter(|&x| x != k).collect() };
    let (change, keys): (Change, Vec<Key>) = match rng.below(10) {
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
        8 => {
            let k = rng.pick(&o).unwrap();
            (Change::Remove { key: k }, others(k))
        }
        _ => {
            let rule = [SignRule::All, SignRule::Threshold(2), SignRule::Any][rng.below(3)];
            (Change::SetRule { rule }, o.clone())
        }
    };
    let c = cert_on(&h.book, vec![head], change, &keys);
    let before = v.owners.owners;
    let r = h.book.commit_local(c, lineage, h.frame);
    for k in before.difference(&h.book.view().owners.owners) {
        h.former.insert(*k);
    }
    r
}

fn honest_history(rng: &mut Rng) -> History {
    let owners = [1, 2, 3];
    let rule = [SignRule::All, SignRule::All, SignRule::Threshold(2), SignRule::Any][rng.below(4)];
    let book = Book::new(Cert::genesis(&owners, rule, rng.next())).unwrap();
    let mut h = History { book, frame: 0, next_key: 100, former: BTreeSet::new(), recovery_keys: BTreeSet::new() };
    let steps = if rng.chance(1) { 700 } else { rng.below(40) + 3 };
    for _ in 0..steps {
        let _ = honest_step(rng, &mut h, L1);
    }
    if rng.chance(30) {
        // The world is copied; both copies play on, then meet.
        let mut copy = h.clone();
        for _ in 0..rng.below(6) + 1 {
            let _ = honest_step(rng, &mut copy, L2);
        }
        for _ in 0..rng.below(6) + 1 {
            let _ = honest_step(rng, &mut h, L1);
        }
        let certs: Vec<Cert> = copy.book.inner.certs.values().filter(|c| c.seq > 0).cloned().collect();
        h.book.insert_attested(&certs, &copy.book.export(L2, &[1]), &sessions());
        h.former.extend(copy.former);
        h.recovery_keys.extend(copy.recovery_keys);
        h.next_key = h.next_key.max(copy.next_key) + 1000;
        h.frame = h.frame.max(copy.frame);
        for _ in 0..rng.below(10) + 1 {
            let _ = honest_step(rng, &mut h, L1);
        }
    }
    h
}

/// A random attack on `book`, with the attacker's keys, optionally with sessions of its own.
fn attack(rng: &mut Rng, book: &mut Book, keys: &[Key], sessions_too: bool) {
    let mut sock: Key = 9000;
    let ss = sessions();
    for _ in 0..rng.below(40) + 1 {
        let ids: Vec<CertId> = book.inner.certs.keys().copied().collect();
        let parent = rng.pick(&ids).unwrap();
        let mut signers: Vec<Key> = keys.iter().copied().filter(|_| rng.chance(50)).collect();
        if signers.is_empty() {
            signers.push(rng.pick(keys).unwrap());
        }
        let target = rng.pick(keys).unwrap_or(1);
        let victim = rng.below(3) as Key + 1;
        let lineage = if rng.chance(50) { L8 } else { L9 };
        sock += 1;
        if rng.chance(8) {
            relay_flood(rng, book);
            continue;
        }
        let c = match rng.below(15) {
            0 | 1 => cert_on(book, vec![parent], Change::Add { key: sock }, &signers),
            2 => cert_on(book, vec![parent], Change::Leave { key: target }, &signers),
            3 => cert_on(book, vec![parent], Change::Challenge { key: victim }, &signers),
            4 => cert_on(book, vec![parent], Change::Remove { key: victim }, &signers),
            5 => cert_on(book, vec![parent], Change::SetRule { rule: SignRule::Any }, &signers),
            6 => cert_on(book, vec![parent], Change::SetRecovery { owner: target, recovery: Some(sock) }, &signers),
            7 => {
                let a = rng.pick(&ids).unwrap();
                let mut p = vec![parent, a];
                p.sort_unstable();
                p.dedup();
                cert_on(book, p, Change::Merge, &signers)
            }
            8 => book.cert(parent).unwrap().clone().signed(&signers),
            9 => {
                let mut held = book.cert(parent).unwrap().clone();
                held.sigs.retain(|_| rng.chance(50));
                held
            }
            10 => {
                // A chain of minted keys from an old state: each new key adds the next, then leaves.
                let mut p = parent;
                let mut me = rng.pick(keys).unwrap();
                for _ in 0..rng.below(20) + 1 {
                    sock += 1;
                    let add = cert_on(book, vec![p], Change::Add { key: sock }, &[me]);
                    let _ = book.insert(add.clone());
                    let lv = cert_on(book, vec![add.id()], Change::Leave { key: me }, &[me]);
                    let _ = book.insert(lv.clone());
                    p = lv.id();
                    me = sock;
                }
                cert_on(book, vec![p], Change::Add { key: sock + 1 }, &[me])
            }
            11 => {
                // A flood of junk from the attacker's keys.
                for _ in 0..rng.below(300) {
                    sock += 1;
                    let p = rng.pick(&ids).unwrap();
                    let _ = book.insert(cert_on(book, vec![p], Change::Add { key: sock }, &[rng.pick(keys).unwrap()]));
                }
                cert_on(book, vec![parent], Change::Add { key: sock + 1 }, &signers)
            }
            _ => {
                let mut c = cert_on(book, vec![parent], Change::Add { key: sock }, &signers);
                c.seq = c.seq.wrapping_add(rng.below(3) as u32).wrapping_sub(1);
                c
            }
        };
        let id = c.id();
        let _ = book.insert(c);
        if sessions_too && rng.chance(60) {
            let frame = if rng.chance(50) { rng.next() >> 20 } else { rng.below(1000) as u64 };
            if rng.chance(30) {
                // Two attestations of one certificate in one lineage, equal frames, signer subsets.
                if let Some(held) = book.cert(id).cloned() {
                    let s: BTreeSet<Key> = held.signers();
                    let sub: BTreeSet<Key> = s.iter().copied().filter(|_| rng.chance(50)).collect();
                    for set in [s, sub] {
                        book.attest(&Attest::make(book.world, id, lineage, frame, &set, &[1]), &ss);
                    }
                }
            } else {
                book.record(id, lineage, frame);
            }
            if rng.chance(30)
                && let Some(&p) = book.cert(id).and_then(|c| c.prev.first())
            {
                book.record(p, lineage, frame.saturating_sub(OBJECTION_FRAMES + 1));
            }
        }
    }
}

/// A relay with no key that matters: orphans with made-up parents (ids ground low), unsigned and
/// stranger-signed certificates on real parents, and attestations of certificates nobody holds. The
/// book refuses every one and stores nothing (invariant 5, OWNU-2).
fn relay_flood(rng: &mut Rng, book: &mut Book) {
    let before = book.clone();
    let ids: Vec<CertId> = book.inner.certs.keys().copied().collect();
    for i in 0..rng.below(300) + 1 {
        let parent = rng.pick(&ids).unwrap();
        let c = match i % 4 {
            0 => {
                let mut c = Cert { world: book.world, seq: 1, prev: vec![rng.next()], change: Change::Add { key: 7777 }, sigs: vec![] };
                while c.id() > u64::MAX >> 6 {
                    c.prev[0] = rng.next();
                }
                c
            }
            1 => cert_on(book, vec![parent], Change::Add { key: 7777 }, &[]),
            2 => cert_on(book, vec![parent], Change::Add { key: 7778 }, &[7777]),
            _ => {
                let a = Attest::make(book.world, rng.next(), L8, rng.below(100) as u64, &[1].into(), &[1]);
                assert!(!book.attest(&a, &sessions()), "an attestation of an unheld certificate is dropped");
                continue;
            }
        };
        let r = book.insert(c.clone());
        assert!(r.is_err(), "a keyless relay's certificate is refused: {r:?} {c:?} parent counts {}", before.counts(parent));
    }
    assert_eq!(*book, before, "a keyless relay stored something");
}

type Essence = (BTreeSet<Key>, SignRule, bool, BTreeMap<Key, BTreeMap<Key, CertId>>, BTreeMap<Key, CertId>, BTreeMap<Key, Recovering>, Vec<CertId>, bool);

/// The view minus what may legitimately differ (refused certificates).
fn essence(v: &View) -> Essence {
    let o = &v.owners;
    (o.owners.clone(), o.rule, o.no_removal, o.recovery.clone(), o.challenges.clone(), o.recovering.clone(), v.heads.clone(), v.disputed)
}

/// Rebuild a book from its certificates and attestations (invariant 4). `batch`: one batch, in a
/// shuffled order. Otherwise one by one in a random causal order, each attestation at a random
/// moment (sometimes before its certificate, so it is dropped), with a keyless relay sending stripped
/// copies and junk in between. What was refused is then sent again with its attestations, as the
/// contract says. Returns the book and how many certificates had to be sent again.
fn rebuilt(rng: &mut Rng, b: &Book, batch: bool) -> (Book, usize) {
    let mut certs: Vec<Cert> = b.inner.certs.values().filter(|c| c.seq > 0).cloned().collect();
    for i in (1..certs.len()).rev() {
        certs.swap(i, rng.below(i + 1));
    }
    let g = b.cert(b.genesis).unwrap().clone();
    let mut attests: Vec<Attest> = [L1, L2, L8, L9].iter().flat_map(|&l| b.export(l, &[1])).collect();
    for i in (1..attests.len()).rev() {
        attests.swap(i, rng.below(i + 1));
    }
    let mut r = Book::new(g).unwrap();
    if batch {
        r.insert_attested(&certs, &attests, &sessions());
        return (r, 0);
    }
    // A random causal order: by seq, ties shuffled (a parent's seq is always lower).
    certs.sort_by_key(|c| c.seq);
    let mut pending: Vec<Attest> = Vec::new();
    let mut by: BTreeMap<CertId, Vec<Attest>> = BTreeMap::new();
    for a in &attests {
        if rng.chance(10) {
            r.attest(a, &sessions());
        }
        by.entry(a.cert).or_default().push(a.clone());
    }
    // The genesis is held from the start.
    pending.extend(by.remove(&b.genesis).into_iter().flatten());
    for c in &certs {
        if rng.chance(10) {
            // The relay: a partial copy first, or junk.
            let mut part = c.clone();
            part.sigs.retain(|_| rng.chance(50));
            let _ = r.insert(part);
        }
        let _ = r.insert(c.clone());
        pending.extend(by.remove(&c.id()).into_iter().flatten());
        while !pending.is_empty() && rng.chance(60) {
            let a = pending.swap_remove(rng.below(pending.len()));
            r.attest(&a, &sessions());
        }
    }
    for a in &pending {
        r.attest(a, &sessions());
    }
    let missing: Vec<Cert> = certs.iter().filter(|c| r.cert(c.id()).is_none() || b.inner.commits.get(&c.id()) != r.inner.commits.get(&c.id())).cloned().collect();
    if !missing.is_empty() {
        r.insert_attested(&missing, &attests, &sessions());
    }
    (r, missing.len())
}

/// The keys that counting certificates added (genesis included).
fn added_keys(b: &Book) -> BTreeSet<Key> {
    let refused: BTreeSet<CertId> = b.inner.view.refused.iter().map(|(c, _)| *c).collect();
    let mut keys = BTreeSet::new();
    for (id, c) in &b.inner.certs {
        if refused.contains(id) {
            continue;
        }
        match &c.change {
            Change::Genesis { owners, .. } => keys.extend(owners.iter().copied()),
            Change::Add { key } => {
                keys.insert(*key);
            }
            Change::FinishRecover { owner } => {
                if let Some(r) = b.inner.eff.get(&c.prev[0]).and_then(|s| s.recovering.get(owner)) {
                    keys.insert(r.new_key);
                }
            }
            _ => {}
        }
    }
    keys
}

/// Invariant 6: a key a counting removal took out is out of the view, unless a counting addition that
/// removal didn't see brought it back. `dormant`: also for window removals and recoveries.
fn removals_stick(b: &Book, dormant: bool) -> Option<String> {
    let refused: BTreeSet<CertId> = b.inner.view.refused.iter().map(|(c, _)| *c).collect();
    let owners = &b.inner.view.owners.owners;
    if b.inner.view.owners.nobody_left {
        // The stated exception: the join would have left nobody, so every head's owners stayed.
        return None;
    }
    for (id, c) in &b.inner.certs {
        if refused.contains(id) {
            continue;
        }
        let gone = match c.change {
            Change::Leave { key } => Some(key),
            Change::RemoveDormant { key } | Change::Remove { key } if dormant => Some(key),
            Change::FinishRecover { owner } if dormant => Some(owner),
            _ => None,
        };
        let Some(k) = gone else { continue };
        if !owners.contains(&k) {
            continue;
        }
        // Observed-remove: the key may be back only through a counting addition this removal never
        // saw: one after it, or one on a copy that didn't see it (each copy's additions stand).
        let readded = b.inner.certs.iter().any(|(a, x)| {
            !refused.contains(a) && matches!(x.change, Change::Add { key } if key == k) && !b.inner.anc[id].contains(a)
        });
        if !readded {
            return Some(format!("key {k} removed by {id} is back"));
        }
    }
    None
}

/// Invariant 7: two attested, valid certificates with one parent mean "disputed".
fn dispute_visible(b: &Book) -> bool {
    let mut by_prev: BTreeMap<CertId, usize> = BTreeMap::new();
    for (id, c) in &b.inner.certs {
        if c.prev.len() == 1 && b.inner.commits.contains_key(id) && !b.inner.invalid.contains(id) {
            *by_prev.entry(c.prev[0]).or_default() += 1;
        }
    }
    !by_prev.values().any(|&n| n > 1) || b.view().disputed
}

#[derive(Default, Debug)]
struct Residual {
    runs: u32,
    honest_owner_lost: u32,
    owner_gained: u32,
    rule_loosened: u32,
    /// The rule loosened while the owners stayed the same: concurrent rules must take the strictest.
    loosened_without_takeover: u32,
    /// Honest histories that forked into two lineages and merged.
    forked: u32,
    /// One-by-one rebuilds that had to send certificates again (a quota or an early attestation).
    resent: u32,
    /// Long honest histories, and the most attested certificates in one.
    long: u32,
    max_attested: usize,
}

fn check(seed: u64, residual: &mut Residual) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x0E);
    let h = honest_history(&mut rng);
    if h.book.inner.commits.values().any(|m| m.contains_key(&L2)) {
        residual.forked += 1;
    }
    if h.book.inner.commits.len() > 256 {
        residual.long += 1;
    }
    residual.max_attested = residual.max_attested.max(h.book.inner.commits.len());
    let honest = h.book.view();
    // The attacker's keys: former owners, recovery keys (stolen), strangers.
    let mut keys: Vec<Key> = h.former.iter().copied().chain(h.recovery_keys.iter().copied()).collect();
    keys.extend([7001, 7002]);
    keys.retain(|k| !honest.owners.owners.contains(k));
    for sessions_too in [false, true] {
        let mut v = h.book.clone();
        attack(&mut rng, &mut v, &keys, sessions_too);
        let ctx = format!("seed {seed}, sessions {sessions_too}");
        let vv = v.view();
        // 5: bounded. Unattested certificates, held or parked, are charged only to keys that matter
        // at a counting certificate (or, at one some session attested, in its own state: the A2
        // attacker's sessions), at most UNATTESTED_PER_KEY each. A keyless relay stores nothing
        // (asserted in relay_flood).
        let keys_of = |s: &Owners| s.owners.iter().copied().chain(s.recovery.values().flat_map(|r| r.keys().copied())).collect::<Vec<_>>();
        let mattering: BTreeSet<Key> = v
            .inner
            .eff
            .iter()
            .filter(|(id, _)| v.counts(**id))
            .flat_map(|(_, s)| keys_of(s))
            .chain(v.inner.commits.keys().flat_map(|id| keys_of(&v.inner.linear[id])))
            .collect();
        let unattested = v.inner.certs.keys().filter(|id| **id != v.genesis && !v.inner.commits.contains_key(id)).count() + v.inner.parked.len();
        assert_eq!(unattested, v.inner.charged.len(), "{ctx}: an unattested certificate charged to nobody");
        assert!(v.inner.charged.values().all(|p| p.is_subset(&mattering)), "{ctx}: charged to a key that never mattered");
        assert!(unattested <= UNATTESTED_PER_KEY * mattering.len(), "{ctx}: {unattested} unattested certs for {} keys", mattering.len());
        // 4: the view and the digest don't depend on arrival order.
        for batch in [true, false] {
            let (r, resent) = rebuilt(&mut rng, &v, batch);
            residual.resent += (resent > 0) as u32;
            assert_eq!(essence(&r.view()), essence(&vv), "{ctx}: the view depends on arrival order (batch: {batch})");
            assert_eq!(r.digest(), v.digest(), "{ctx}: the digest depends on arrival order (batch: {batch})");
        }
        // 1: every owner was added by a counting certificate.
        let added = added_keys(&v);
        assert!(vv.owners.owners.is_subset(&added), "{ctx}: an owner nobody added: {:?}", vv.owners.owners.difference(&added).collect::<Vec<_>>());
        // 6: removals stick (Leave against both; windows and recoveries against A1 only, OT(a)).
        if let Some(e) = removals_stick(&v, !sessions_too) {
            panic!("{ctx}: {e}");
        }
        // 7: a dispute is visible.
        assert!(dispute_visible(&v), "{ctx}: attested siblings but not disputed");
        if !sessions_too {
            // 2: unattested changes change nothing.
            assert_eq!(essence(&vv), essence(&honest), "{ctx}: an A1 attacker changed the view");
            // 3: liveness: an owner's unattested proposal on the head still gets in, and the next
            // honest steps go the same way in both books.
            let o = *vv.owners.owners.iter().next().unwrap();
            let prop = cert_on(&v, vv.heads.clone(), Change::SetRecovery { owner: o, recovery: Some(55_555) }, &[o]);
            if vv.heads.len() == 1 {
                assert_eq!(v.clone().insert(prop.clone()), h.book.clone().insert(prop), "{ctx}: an honest proposal went differently after the attack");
            }
            let mut h1 = History { book: h.book.clone(), ..h.clone() };
            let mut h2 = History { book: v.clone(), ..h.clone() };
            for k in 0..5 {
                let mut r1 = Rng(seed ^ (k << 40));
                let mut r2 = Rng(seed ^ (k << 40));
                let a = honest_step(&mut r1, &mut h1, L1);
                let b = honest_step(&mut r2, &mut h2, L1);
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
            if vo.rule < ho.rule || (vo.rule == SignRule::Any) != (ho.rule == SignRule::Any) && vo.rule == SignRule::Any {
                residual.rule_loosened += 1;
                if vo.owners == ho.owners {
                    residual.loosened_without_takeover += 1;
                }
            }
        }
    }
}

#[test]
fn adversarial_harness() {
    let iters: u64 = std::env::var("PE_OWN_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(150);
    // A long run splits into chunks: PE_OWN_FROM is the first seed.
    let from: u64 = std::env::var("PE_OWN_FROM").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut residual = Residual::default();
    for seed in from..from + iters {
        check(seed, &mut residual);
    }
    println!(
        "HARNESS {iters} honest histories from seed {from} ({} forked into two lineages and merged; {} long, up to {} attested certs) x 2 attackers: invariants 1-7 held (A1), 1, 4, 5, 6 (Leave), 7 held (A2); A2 residual (no trust root) in {} runs: honest owner lost {}, owner gained {}, rule loosened {} (always with a takeover); {} one-by-one rebuilds sent certificates again, then converged",
        residual.forked, residual.long, residual.max_attested, residual.runs, residual.honest_owner_lost, residual.owner_gained, residual.rule_loosened, residual.resent
    );
    assert_eq!(residual.loosened_without_takeover, 0, "concurrent rules take the strictest; only a takeover loosens");
}

#[test]
#[ignore = "debug helper: PE_OWN_SEED"]
fn harness_debug_seed() {
    let seed: u64 = std::env::var("PE_OWN_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut residual = Residual::default();
    check(seed, &mut residual);
}

#[test]
#[ignore = "debug helper: PE_OWN_SEED, prints the rebuild difference"]
fn harness_debug_rebuild() {
    let seed: u64 = std::env::var("PE_OWN_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(7);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x0E);
    let h = honest_history(&mut rng);
    let honest = h.book.view();
    let mut keys: Vec<Key> = h.former.iter().copied().chain(h.recovery_keys.iter().copied()).collect();
    keys.extend([7001, 7002]);
    keys.retain(|k| !honest.owners.owners.contains(k));
    for sessions_too in [false, true] {
        let mut v = h.book.clone();
        attack(&mut rng, &mut v, &keys, sessions_too);
        for batch in [true, false] {
            let (r, _) = rebuilt(&mut rng, &v, batch);
            if essence(&r.view()) == essence(&v.view()) && r.digest() == v.digest() {
                continue;
            }
            println!("sessions {sessions_too} batch {batch}: v certs {} r certs {} parked v {} r {}", v.len(), r.len(), v.inner.parked.len(), r.inner.parked.len());
            if v.view().owners != r.view().owners {
                println!("  owners differ:
   v {:?}
   r {:?}", v.view().owners, r.view().owners);
            }
            let a: BTreeSet<CertId> = v.inner.certs.keys().copied().collect();
            let b: BTreeSet<CertId> = r.inner.certs.keys().copied().collect();
            for id in a.difference(&b) {
                println!("  only in v: {id} {:?} attested {:?} counts {}", v.inner.certs[id].change, v.inner.commits.get(id).map(|m| m.keys().collect::<Vec<_>>()), v.counts(*id));
                let mut r2 = r.clone();
                let c = v.inner.certs[id].clone();
                println!("    parked in r {}; prev held in r {:?}; insert_raw Free -> {:?}", r.inner.parked.contains_key(id), c.prev.iter().map(|p| (r.inner.certs.contains_key(p), r.inner.parked.contains_key(p), r.inner.invalid.contains(p))).collect::<Vec<_>>(), r2.insert_raw(c, Admit::Free));
            }
            for id in a.intersection(&b) {
                if v.inner.commits.get(id) != r.inner.commits.get(id) {
                    let c = &v.inner.certs[id];
                    let mut r2 = r.clone();
                    let att: Vec<Attest> = [L1, L2, L8, L9].iter().flat_map(|&l| v.export(l, &[1])).filter(|a| a.cert == *id).collect();
                    println!(
                        "  commits differ {id}: {:?} vs {:?}; seq {} {:?}; prev invalid in r {:?}, in v {:?}; invalid itself r {} v {}; resend -> {:?}; attests {}",
                        v.inner.commits.get(id),
                        r.inner.commits.get(id),
                        c.seq,
                        c.change,
                        c.prev.iter().map(|p| r.inner.invalid.contains(p)).collect::<Vec<_>>(),
                        c.prev.iter().map(|p| v.inner.invalid.contains(p)).collect::<Vec<_>>(),
                        r.inner.invalid.contains(id),
                        v.inner.invalid.contains(id),
                        r2.insert_attested(std::slice::from_ref(c), &att, &sessions()),
                        att.len()
                    );
                    println!("    after resend {:?}", r2.inner.commits.get(id));
                }
                if v.counts(*id) != r.counts(*id) {
                    println!("  counts differ {id} {:?}: v {} r {}", v.inner.certs[id].change, v.counts(*id), r.counts(*id));
                }
            }
        }
    }
}

#[test]
#[ignore = "debug helper: PE_OWN_SEED, invariant 6 on the honest book"]
fn harness_debug_removal() {
    let seed: u64 = std::env::var("PE_OWN_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(99);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x0E);
    let h = honest_history(&mut rng);
    let b = &h.book;
    println!("honest: {:?}", removals_stick(b, true));
    let mut ids: Vec<&CertId> = b.inner.certs.keys().collect();
    ids.sort_by_key(|i| b.inner.certs[i].seq);
    let refused: BTreeMap<CertId, OwnError> = b.inner.view.refused.iter().copied().collect();
    for id in ids {
        let c = &b.inner.certs[id];
        println!("  {id} seq {} prev {:?} {:?} signers {:?} lineages {:?} {}", c.seq, c.prev, c.change, c.signers(), b.inner.commits.get(id).map(|m| m.keys().collect::<Vec<_>>()), refused.get(id).map_or("counts".to_string(), |e| format!("refused {e:?}")));
    }
    println!("owners {:?}", b.view().owners.owners);
    for (id, c) in &b.inner.certs {
        if c.change == Change::Merge {
            for p in &c.prev {
                println!("merge {id} parent {p}: eff tags {:?} anc has Leave(s): {:?}", b.inner.eff[p].tags, b.inner.certs.iter().filter(|(x, y)| matches!(y.change, Change::Leave { .. }) && b.inner.anc[p].contains(x)).map(|(x, y)| (x, y.change.clone())).collect::<Vec<_>>());
            }
            println!("merge eff tags {:?}", b.inner.eff[id].tags);
        }
        if let Change::Leave { key } = c.change {
            let p = c.prev[0];
            println!("leave {id} of {key}: parent eff tags of key {:?}", b.inner.eff[&p].tags.get(&key));
        }
    }
}
