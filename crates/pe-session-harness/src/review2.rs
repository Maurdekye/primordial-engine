//! Round-2 review repros (design-reviewer's `repro2`), ported to the round-2 protocol: every
//! decision input is a chain entry, nothing is re-stamped, confirmation uses stable data only.
//!
//! * `ackfork`:   member 9 shows member 2 a different chain (acks without its touch). Before: the
//!   honest members forked. Now: the conflicting references expose the equivocation, both honest
//!   members vote, 9 is evicted with a cut before the disputed entry, and they agree.
//! * `lateequiv`: member 9 signs a second version of an entry after it was confirmed. Before: both
//!   versions voided, rollback below the confirmed frame, panic. Now: the stable version stands.
//! * `evict`:     an entry that reached two members 600 ms late, then an eviction. Before: the
//!   eviction re-stamped a confirmed entry and crashed a member. Now: nothing moves.
//! * `outage`:    honest members, member 2's acks to member 3 lost for 6.5 s. Before: late reports
//!   expired and the members forked. Now: member 3 fetches member 2's acks from member 1.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use pe_session_sim::*;
use pe_session::session::*;

pub(crate) fn link(d: u64, j: u64, l: u32) -> Link {
    Link { delay_us: d * 1000, jitter_us: j * 1000, loss_pct: l }
}

pub(crate) fn touch(obj: u32, y: f32) -> Action {
    Action::Touch { obj, impulse: [0.0, y, 0.0], grab: false }
}

pub(crate) fn plan(v: &[(u64, Action)]) -> BTreeMap<u64, Vec<Action>> {
    let mut m: BTreeMap<u64, Vec<Action>> = BTreeMap::new();
    for (f, a) in v {
        m.entry(*f).or_default().push(a.clone());
    }
    m
}

pub(crate) fn peers(ids: &[MemberId], plans: impl Fn(MemberId) -> BTreeMap<u64, Vec<Action>>) -> Vec<Peer> {
    ids.iter()
        .map(|&id| {
            let mut p = Peer::new(id, 0);
            p.join(1, ids, SceneKind::Pile { side: 8, layers: 4 }, SimConfig::default(), 100_000, plans(id));
            p
        })
        .collect()
}

/// Step the sim, turning a panic into a reported failure.
pub(crate) fn run_to_end(sim: &mut Sim, mut each_ms: impl FnMut(&mut Sim)) -> Option<u64> {
    while sim.net.now_us <= sim.end_us {
        each_ms(sim);
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sim.step_ms())).is_err() {
            return Some(sim.net.now_us / 1000);
        }
        sim.net.now_us += 1000;
    }
    None
}

/// Honest members agree with each other and with an offline replay of the final log, over the
/// frames every honest member has simulated; no confirmed frame was rolled back (the run would
/// have panicked); no false check failures.
pub(crate) fn verdict(sim: &Sim, honest: &[MemberId], label: &str, panic_at: Option<u64>) -> bool {
    if let Some(t) = panic_at {
        println!("RESULT {label}: PANIC at t={t} ms -> FAIL");
        return false;
    }
    let frames = honest.iter().map(|&m| sim.replica(m, 1).sim.frame).min().unwrap_or(0).saturating_sub(1);
    let first = &sim.replica(honest[0], 1).sim;
    crate::dump_outcomes(first, 1, label);
    let members: Vec<MemberId> = first.members.iter().copied().collect();
    let reference = replay(1, SceneKind::Pile { side: 8, layers: 4 }, first.cfg, &members, &final_log(first), frames);
    let rd = digest(&reference, frames);
    let ds: Vec<(MemberId, u64)> = honest.iter().map(|&m| (m, digest(&sim.replica(m, 1).sim.hashes, frames))).collect();
    let equal = ds.iter().all(|(_, d)| *d == rd);
    let bad: u64 = honest.iter().map(|&m| sim.replica(m, 1).sim.stats.checks_bad).sum();
    let ok: u64 = honest.iter().map(|&m| sim.replica(m, 1).sim.stats.checks_ok).sum();
    for &m in honest {
        let r = &sim.replica(m, 1).sim;
        println!(
            "  m{m}: conf {} frame {} members {:?} evicted {:?} rollbacks {} max depth {} violations {} equivocations {} checks ok {} bad {}",
            r.conf, r.frame, r.members, r.members_evicted(), r.stats.rollbacks, r.stats.max_depth,
            r.stats.entries_rejected_violation, r.stats.equivocations, r.stats.checks_ok, r.stats.checks_bad
        );
    }
    println!(
        "RESULT {label}: frames 0..{frames} honest {} | offline replay {rd:016x} | {} ; checks ok {ok} bad {bad}",
        ds.iter().map(|(m, d)| format!("m{m}={d:016x}")).collect::<Vec<_>>().join(" "),
        if equal && bad == 0 { "ALL EQUAL, NO FALSE CHECKS" } else { "FAIL" }
    );
    equal && bad == 0
}

pub(crate) fn touch_state(sim: &Sim, m: MemberId, author: MemberId) -> String {
    let r = &sim.replica(m, 1).sim;
    let void = r.void_ids();
    r.all_entries()
        .into_iter()
        .filter(|e| e.member == author && matches!(e.payload, Payload::Touch { .. }))
        .map(|e| format!("m{}s{}@{} eff={:?} void={}", e.member, e.seq, e.stamp, r.effective_frame(e.id()), void.contains(&e.id())))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn ackfork() {
    let ids = [1, 2, 9];
    let ps = peers(&ids, |m| if m == 9 { plan(&[(50, touch(10, 6.0))]) } else { BTreeMap::new() });
    let mut net = Net::new(link(30, 10, 0), 7);
    // Member 2 never receives 9's real chain from 9, only the forged one.
    let forged_ids: Rc<RefCell<BTreeSet<EntryId>>> = Rc::default();
    let f2 = forged_ids.clone();
    net.filter = Some(Box::new(move |_t, _w, from, to, m| match m {
        Msg::Entry(e) if from == 9 && to == 2 && !f2.borrow().contains(&e.id()) => Fate::Drop,
        _ => Fate::Deliver,
    }));
    let mut sim = Sim::new(net, ps, 8_000_000);
    // 9 signs a second chain for member 2: acks with honest horizons, and no touch.
    let mut forged: (u32, EntryId) = (0, EntryId::NONE);
    let panic_at = run_to_end(&mut sim, |sim| {
        if sim.net.now_us % 33_000 == 0 {
            let now = sim.replica(9, 1).sim.now;
            let e = Entry {
                member: 9,
                seq: forged.0 + 1,
                prev: forged.1,
                stamp: now,
                payload: Payload::Ack { horizon: now.saturating_sub(1), heads: vec![], check: None, epoch: 0 },
                carried: None,
            };
            forged = (e.seq, e.id());
            forged_ids.borrow_mut().insert(e.id());
            sim.net.send(1, 9, 2, &Msg::Entry(test_sign(1, e)));
        }
    });
    println!("  9's touch at m1: {}", touch_state(&sim, 1, 9));
    println!("  9's touch at m2: {}", touch_state(&sim, 2, 9));
    verdict(&sim, &[1, 2], "ackfork (9 shows member 2 a different chain)", panic_at);
}

pub fn lateequiv() {
    let ids = [1, 2, 9];
    let ps = peers(&ids, |m| if m == 9 { plan(&[(50, touch(10, 6.0))]) } else { BTreeMap::new() });
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 8_000_000);
    let mut done = false;
    let panic_at = run_to_end(&mut sim, |sim| {
        if !done && sim.replica(1, 1).sim.conf > 200 {
            let x = sim.replica(9, 1).sim.all_entries().into_iter().find(|e| e.member == 9 && matches!(e.payload, Payload::Touch { .. })).unwrap();
            println!(
                "  t={} ms: m1 conf {}; 9's touch (frame {}) confirmed at m1: {}, at m2: {} -> 9 now signs a second version of it",
                sim.net.now_us / 1000,
                sim.replica(1, 1).sim.conf,
                x.stamp,
                sim.replica(1, 1).sim.is_confirmed(x.id()),
                sim.replica(2, 1).sim.is_confirmed(x.id())
            );
            let mut x2 = x.clone();
            x2.payload = Payload::Touch { obj: 10, impulse: [6.0, 0.0, 0.0], grab: false };
            x2.carried = None;
            sim.net.send(1, 9, 1, &Msg::Entry(test_sign(1, x2.clone())));
            sim.net.send(1, 9, 2, &Msg::Entry(test_sign(1, x2)));
            done = true;
        }
    });
    println!("  9's touches at m1: {}", touch_state(&sim, 1, 9));
    verdict(&sim, &[1, 2], "lateequiv (second version of a confirmed entry)", panic_at);
}

pub fn evict() {
    let ids = [1, 2, 3, 9];
    let ps = peers(&ids, |m| match m {
        1 => plan(&[(60, touch(10, 6.0))]),
        9 => plan(&[(400, touch(33, 3.0))]),
        _ => BTreeMap::new(),
    });
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(Box::new(|_t, _w, from, to, m| match m {
        Msg::Entry(e) if from == 1 && (to == 3 || to == 9) && e.stamp == 60 && matches!(e.payload, Payload::Touch { .. }) => Fate::Delay(600_000),
        _ => Fate::Deliver,
    }));
    let mut sim = Sim::new(net, ps, 10_000_000);
    let mut done = false;
    let panic_at = run_to_end(&mut sim, |sim| {
        if !done {
            if let Some(x) = sim.replica(9, 1).sim.all_entries().into_iter().find(|e| e.member == 9 && matches!(e.payload, Payload::Touch { .. })) {
                println!("  t={} ms: before 9 equivocates: member 1's touch at m2: {}", sim.net.now_us / 1000, touch_state(sim, 2, 1));
                let mut x2 = x.clone();
                x2.payload = Payload::Touch { obj: 33, impulse: [0.0, 0.0, 3.0], grab: false };
                x2.carried = None;
                for to in [1, 2, 3] {
                    sim.net.send(1, 9, to, &Msg::Entry(test_sign(1, x2.clone())));
                }
                done = true;
            }
        }
    });
    for m in [1, 2, 3] {
        println!("  member 1's touch at m{m}: {}", touch_state(&sim, m, 1));
    }
    verdict(&sim, &[1, 2, 3], "evict (late entry, then an eviction)", panic_at);
}

pub fn outage() {
    let ids = [1, 2, 3];
    let ps = peers(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0))]) } else { BTreeMap::new() });
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(Box::new(|t, _w, from, to, m| match m {
        Msg::Entry(e) if from == 1 && e.stamp == 60 && to != 1 && matches!(e.payload, Payload::Touch { .. }) => Fate::Delay(600_000),
        Msg::Entry(e) if matches!(e.payload, Payload::Ack { .. }) && from == 2 && to == 3 && (1_000_000..7_500_000).contains(&t) => Fate::Drop,
        _ => Fate::Deliver,
    }));
    let mut sim = Sim::new(net, ps, 12_000_000);
    let panic_at = run_to_end(&mut sim, |_| {});
    for m in [1, 2, 3] {
        println!("  member 1's touch at m{m}: {}", touch_state(&sim, m, 1));
    }
    verdict(&sim, &ids, "outage (honest members, 6.5 s one-way ack outage)", panic_at);
}
