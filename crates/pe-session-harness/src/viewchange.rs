//! P2-4 scenarios: the proposer-based view change (design 10.8).
//!
//! * `twosilent`:   two of six members go silent at once (t = 3 s). One view change removes both.
//! * `twosilent5`:  two of five go silent at once, under both quorum rules (D80). A strict majority
//!   removes them; the Byzantine quorum (4 of 5) cannot, and the three left pause, safely.
//! * `withheld`:    9 signs a second version of a confirmed touch; colluder 8 acks normally but
//!   never casts a view-change entry. 1, 2 and 3 remove 9 without 8 (3 of the 4 not proven).
//! * `colluders`:   `badvote` with five members: 9 equivocates, colluder 8 forks its vote and shows
//!   the others a forged frozen head of (0, 0). Both are proven faulty and removed.
//! * `badproposer`: member 5 goes silent; the first proposer equivocates, showing half the members a
//!   reset instead of the removal. That round fails, a later one removes 5, and the equivocating
//!   proposer is proven faulty and removed too.
//! * `bridge`:      a partition between {1, 2} and {3, 4} from 3 to 12 s, with faulty member 8
//!   reaching both sides: it hides what it hears (its references stop), backs every vote, prevotes
//!   and precommits every proposal, echoes the nil votes of rounds without one (so each side moves
//!   on to rounds its own members propose in), and applies no decision (D80). Under a strict majority each side
//!   removes the other: the session forks. Under the Byzantine quorum neither side can, and the
//!   session carries on whole once the partition heals and 8 behaves.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use pe_session_sim::*;
use pe_session::session::*;

use crate::review2::{link, plan, run_to_end, touch, touch_state, verdict};
use crate::review3::{equivocate_after_conf, report};

fn peers_with(ids: &[MemberId], cfg: SimConfig, plans: impl Fn(MemberId) -> BTreeMap<u64, Vec<Action>>) -> Vec<Peer> {
    ids.iter()
        .map(|&id| {
            let mut p = Peer::new(id, 0);
            p.join(1, ids, SceneKind::Pile { side: 8, layers: 4 }, cfg, 100_000, plans(id));
            p
        })
        .collect()
}

fn rule_name(q: QuorumRule) -> &'static str {
    match q {
        QuorumRule::Bft => "bft",
        QuorumRule::Majority => "majority",
    }
}

/// Every decision each listed replica took: (epoch reached, clock frame, round, removed).
pub(crate) fn vc_report(sim: &Sim, members: &[MemberId], label: &str) {
    for &m in members {
        let rep = sim.replica(m, 1);
        let r = &rep.sim;
        let at: Vec<String> = r.vcm.vc_log.iter().map(|(e, f, rd, rm)| format!("epoch {e} at frame {f} (t={:.2} s) round {rd} removed {rm:?}", sim_secs(rep, *f))).collect();
        println!(
            "RESULT {label}: m{m} epoch {} members {:?} view changes {} (resets {}, highest round {}) | {}",
            r.vcm.epoch,
            r.vcm.members,
            r.stats.view_changes,
            r.stats.resets,
            r.stats.max_round,
            if at.is_empty() { "no decision".to_string() } else { at.join("; ") }
        );
        if std::env::var("PE_EVENTS").is_ok_and(|v| !v.is_empty()) {
            for e in &r.events {
                println!("  event {e}");
            }
            println!("  vc {}", r.vc_debug());
        }
    }
}

/// The scenario time (the clock its events are scheduled in) at which a replica's clock frame `f` began.
pub(crate) fn sim_secs(rep: &Replica, f: u64) -> f64 {
    (rep.epoch_us + f * rep.dt_us) as f64 / 1e6
}

fn silent_from(ids: &'static [MemberId], t_us: u64) -> Filter {
    Box::new(move |t, _w, from, to, _m| if (ids.contains(&from) || ids.contains(&to)) && t >= t_us { Fate::Drop } else { Fate::Deliver })
}

pub fn twosilent() {
    let ids = [1, 2, 3, 4, 5, 6];
    let ps = peers_with(&ids, SimConfig::default(), |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (400, touch(33, 3.0))]) } else { BTreeMap::new() });
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(silent_from(&[5, 6], 3_000_000));
    let mut sim = Sim::new(net, ps, 15_000_000);
    let panic_at = run_to_end(&mut sim, |_| {});
    report(&sim, &[1, 2, 3, 4], "twosilent");
    vc_report(&sim, &[1, 2, 3, 4], "twosilent");
    verdict(&sim, &[1, 2, 3, 4], "twosilent (members 5 and 6 of 6 silent from t=3 s)", panic_at);
}

pub fn twosilent5() {
    for q in [QuorumRule::Majority, QuorumRule::Bft] {
        let ids = [1, 2, 3, 4, 5];
        let cfg = SimConfig { quorum: q, ..SimConfig::default() };
        let ps = peers_with(&ids, cfg, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (400, touch(33, 3.0))]) } else { BTreeMap::new() });
        let mut net = Net::new(link(30, 10, 0), 7);
        net.filter = Some(silent_from(&[4, 5], 3_000_000));
        let mut sim = Sim::new(net, ps, 15_000_000);
        let panic_at = run_to_end(&mut sim, |_| {});
        let label = format!("twosilent5-{}", rule_name(q));
        report(&sim, &[1, 2, 3], &label);
        vc_report(&sim, &[1, 2, 3], &label);
        verdict(&sim, &[1, 2, 3], &format!("{label} (members 4 and 5 of 5 silent from t=3 s)"), panic_at);
    }
}

pub fn withheld() {
    let ids = [1, 2, 3, 8, 9];
    let mut ps = peers_with(&ids, SimConfig::default(), |m| if m == 9 { plan(&[(50, touch(10, 6.0))]) } else { BTreeMap::new() });
    for p in ps.iter_mut().filter(|p| p.id == 8) {
        p.worlds.get_mut(&1).unwrap().sim.vcm.withhold_vc = true;
    }
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 8_000_000);
    let mut equivocated = false;
    let panic_at = run_to_end(&mut sim, |sim| {
        if !equivocated {
            equivocated = equivocate_after_conf(sim, &[1, 2, 3, 8]);
        }
    });
    for m in [1, 2, 3] {
        println!("  9's touches at m{m}: {}", touch_state(&sim, m, 9));
    }
    report(&sim, &[1, 2, 3], "withheld");
    vc_report(&sim, &[1, 2, 3], "withheld");
    verdict(&sim, &[1, 2, 3], "withheld (9 equivocates on a confirmed touch; 8 casts no view-change entry)", panic_at);
}

pub fn colluders() {
    let ids = [1, 2, 3, 8, 9];
    let ps = peers_with(&ids, SimConfig::default(), |m| if m == 9 { plan(&[(50, touch(10, 6.0))]) } else { BTreeMap::new() });
    let mut net = Net::new(link(30, 10, 0), 7);
    let forged_vote: Rc<Cell<EntryId>> = Rc::default();
    let fv = forged_vote.clone();
    // 8's genuine vote never leaves 8 (nor does any fetch of it); only the forged copy does.
    net.filter = Some(Box::new(move |_t, _w, from, _to, m| match m {
        Msg::Entry(e) if from == 8 && matches!(e.payload, Payload::Vote { .. }) && e.id() != fv.get() => Fate::Drop,
        _ => Fate::Deliver,
    }));
    let mut sim = Sim::new(net, ps, 8_000_000);
    let (mut equivocated, mut forged) = (false, false);
    let panic_at = run_to_end(&mut sim, |sim| {
        if !equivocated {
            equivocated = equivocate_after_conf(sim, &[1, 2, 3, 8]);
        }
        if equivocated && !forged {
            let v = sim.replica(8, 1).sim.all_entries().into_iter().find(|e| e.member == 8 && matches!(e.payload, Payload::Vote { .. }));
            if let Some(v) = v {
                if let Payload::Vote { epoch, target, seq, kick, .. } = v.payload {
                    let mut f = v.clone();
                    f.payload = Payload::Vote { epoch, target, seq, frozen: (0, EntryId::NONE), kick };
                    forged_vote.set(f.id());
                    for to in [1, 2, 3] {
                        sim.net.send(1, 8, to, &Msg::Entry(test_sign(1, f.clone())));
                    }
                    println!("  t={} ms: 8 sends members 1, 2 and 3 a forged vote against {target} with frozen head (0, 0)", sim.net.now_us / 1000);
                    forged = true;
                }
            }
        }
    });
    for m in [1, 2, 3] {
        println!("  9's touches at m{m}: {}", touch_state(&sim, m, 9));
    }
    report(&sim, &[1, 2, 3], "colluders");
    vc_report(&sim, &[1, 2, 3], "colluders");
    verdict(&sim, &[1, 2, 3], "colluders (two of five: 9 equivocates on a confirmed touch; 8 forks its vote)", panic_at);
}

pub fn badproposer() {
    let ids = [1, 2, 3, 4, 5];
    let ps = peers_with(&ids, SimConfig::default(), |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (400, touch(33, 3.0))]) } else { BTreeMap::new() });
    let p0 = ps[0].worlds[&1].sim.proposer_of(0, 0).unwrap();
    // The silent member is one that is not the first proposer; half the rest see the forgery.
    let silent: MemberId = *ids.iter().rev().find(|&&m| m != p0).unwrap();
    let others: Vec<MemberId> = ids.iter().copied().filter(|&m| m != p0 && m != silent).collect();
    let fooled: BTreeSet<MemberId> = others.iter().copied().take(others.len() / 2).collect();
    println!("  first proposer {p0}; member {silent} goes silent at t=3 s; {p0} shows {fooled:?} a reset instead of the removal");
    let forged: Rc<Cell<EntryId>> = Rc::default();
    let fid = forged.clone();
    let fooled_f = fooled.clone();
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(Box::new(move |t, _w, from, to, m| {
        if (from == silent || to == silent) && t >= 3_000_000 {
            return Fate::Drop;
        }
        match m {
            Msg::Entry(e) if from == p0 && fooled_f.contains(&to) && matches!(e.payload, Payload::Propose { .. }) && e.id() != fid.get() => Fate::Drop,
            _ => Fate::Deliver,
        }
    }));
    let mut sim = Sim::new(net, ps, 15_000_000);
    let mut done = false;
    let panic_at = run_to_end(&mut sim, |sim| {
        if done {
            return;
        }
        let genuine = sim.replica(p0, 1).sim.all_entries().into_iter().find(|e| e.member == p0 && matches!(e.payload, Payload::Propose { epoch: 0, round: 0, .. }));
        if let Some(g) = genuine {
            if let Payload::Propose { epoch, round, valid_round, ref change } = g.payload {
                let mut f = g.clone();
                let reset = Change { epoch, remove: vec![], add: vec![], boundary: 0, excluded: change.excluded.clone(), admin: vec![], add_roles: vec![], add_attests: vec![], add_reqs: vec![] };
                f.payload = Payload::Propose { epoch, round, valid_round, change: reset };
                forged.set(f.id());
                for &to in &fooled {
                    sim.net.send(1, p0, to, &Msg::Entry(test_sign(1, f.clone())));
                }
                println!(
                    "  t={} ms: {p0} proposes removing {:?} to the others and a reset to {fooled:?}",
                    sim.net.now_us / 1000,
                    change.remove.iter().map(|r| r.member).collect::<Vec<_>>()
                );
                done = true;
            }
        }
    });
    let honest: Vec<MemberId> = others.clone();
    report(&sim, &honest, "badproposer");
    vc_report(&sim, &honest, "badproposer");
    verdict(&sim, &honest, &format!("badproposer (proposer {p0} equivocates; member {silent} silent from t=3 s)"), panic_at);
}

pub fn bridge() {
    for q in [QuorumRule::Majority, QuorumRule::Bft] {
        let ids = [1, 2, 3, 4, 8];
        let cfg = SimConfig { quorum: q, ..SimConfig::default() };
        let mut ps = peers_with(&ids, cfg, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (400, touch(33, 3.0))]) } else { BTreeMap::new() });
        for p in ps.iter_mut().filter(|p| p.id == 8) {
            p.worlds.get_mut(&1).unwrap().sim.vcm.byz_ally = true;
        }
        let mut net = Net::new(link(30, 10, 0), 7);
        let side = |m: MemberId| if m <= 2 { 1 } else if m <= 4 { 2 } else { 0 };
        net.filter = Some(Box::new(move |t, _w, from, to, _m| {
            if (3_000_000..12_000_000).contains(&t) && side(from) * side(to) == 2 {
                Fate::Drop
            } else {
                Fate::Deliver
            }
        }));
        let mut sim = Sim::new(net, ps, 20_000_000);
        // 8 hides what it hears of everyone during the partition, then behaves once it heals.
        let (mut hiding, mut healed) = (false, false);
        let i8 = sim.peers.iter().position(|p| p.id == 8).unwrap();
        let panic_at = run_to_end(&mut sim, |sim| {
            if !hiding && sim.net.now_us >= 3_000_000 {
                sim.peers[i8].worlds.get_mut(&1).unwrap().sim.freeze_others();
                hiding = true;
            }
            if !healed && sim.net.now_us >= 12_000_000 {
                let r = &mut sim.peers[i8].worlds.get_mut(&1).unwrap().sim;
                r.vcm.byz_ally = false;
                r.unfreeze_others();
                healed = true;
            }
        });
        let label = format!("bridge-{}", rule_name(q));
        report(&sim, &[1, 2, 3, 4], &label);
        vc_report(&sim, &[1, 2, 3, 4], &label);
        let a = sim.replica(1, 1).sim.vcm.members.clone();
        let b = sim.replica(3, 1).sim.vcm.members.clone();
        println!(
            "RESULT {label}: member lists after the partition healed: m1 {a:?}, m3 {b:?} -> {}",
            if a == b { "ONE SESSION" } else { "FORKED (two sessions)" }
        );
        if a == b {
            verdict(&sim, &[1, 2, 3, 4], &format!("{label} (partition {{1,2}} | {{3,4}} 3-12 s, faulty 8 backs both sides)"), panic_at);
        } else {
            for (side, honest) in [("side {1,2}", [1, 2]), ("side {3,4}", [3, 4])] {
                verdict(&sim, &honest, &format!("{label} {side}"), panic_at);
            }
        }
    }
}

/// World-log eviction at scale (25.5.2): `n` members in a log-only world (no bodies), acks every 30
/// frames; the last `silent` members go silent at t = 1 s. Reports when each other member decided.
/// Without ack heads nothing becomes stable, so this measures the view change alone.
pub fn vc_scale(n: u32, silent: u32, heads: bool) {
    let ids: Vec<MemberId> = (1..=n).collect();
    let cfg = SimConfig { ack_heads: heads, ..SimConfig::default() };
    let ps: Vec<Peer> = ids
        .iter()
        .map(|&id| {
            let mut p = Peer::new(id, 0);
            p.join(1, &ids, SceneKind::Empty, cfg, 100_000, BTreeMap::new());
            let r = p.worlds.get_mut(&1).unwrap();
            r.ack_every_us = 30 * r.dt_us;
            p
        })
        .collect();
    let gone: BTreeSet<MemberId> = ids.iter().rev().take(silent as usize).copied().collect();
    let g = gone.clone();
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(Box::new(move |t, _w, from, to, _m| if (g.contains(&from) || g.contains(&to)) && t >= 1_000_000 { Fate::Drop } else { Fate::Deliver }));
    let mut sim = Sim::new(net, ps, 7_000_000);
    let t0 = std::time::Instant::now();
    let panic_at = run_to_end(&mut sim, |_| {});
    let live: Vec<MemberId> = ids.iter().copied().filter(|m| !gone.contains(m)).collect();
    let mut at: Vec<f64> = Vec::new();
    let mut undecided = 0;
    let mut members_after = BTreeSet::new();
    for &m in &live {
        let rep = sim.replica(m, 1);
        let r = &rep.sim;
        match r.vcm.vc_log.iter().find(|d| d.3.iter().any(|x| gone.contains(x))) {
            Some(d) => at.push(sim_secs(rep, d.1) - 1.0),
            None => undecided += 1,
        }
        members_after.insert(r.vcm.members.len());
    }
    at.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let r1 = &sim.replica(live[0], 1).sim;
    println!(
        "RESULT vcscale n={n} silent={silent} heads={heads}: {} of {} live members decided the removal; seconds after the silence began: min {:.2} median {:.2} max {:.2}; undecided {undecided}; member counts after {members_after:?}; m{} decisions {:?}; conf {} | {} | wall {:.1} s",
        at.len(),
        live.len(),
        at.first().copied().unwrap_or(f64::NAN),
        at.get(at.len() / 2).copied().unwrap_or(f64::NAN),
        at.last().copied().unwrap_or(f64::NAN),
        live[0],
        r1.vcm.vc_log,
        r1.conf,
        if panic_at.is_some() { "PANIC" } else { "no panic" },
        t0.elapsed().as_secs_f64()
    );
}

/// A join (10.8): member 4 follows the log from the start and asks to join at t = 2 s; once admitted
/// it touches an object at frame 420.
pub fn join() {
    let ids = [1, 2, 3];
    let joiner: MemberId = 4;
    let mut ps = peers_with(&ids, SimConfig::default(), |m| if m == 1 { plan(&[(60, touch(10, 6.0))]) } else { BTreeMap::new() });
    let mut p = Peer::new(joiner, 0);
    p.join(1, &ids, SceneKind::Pile { side: 8, layers: 4 }, SimConfig::default(), 100_000, plan(&[(420, touch(33, 3.0))]));
    ps.push(p);
    for p in ps.iter_mut().filter(|p| p.id != joiner) {
        p.worlds.get_mut(&1).unwrap().sim.follow(joiner);
    }
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
    let mut asked = false;
    let panic_at = run_to_end(&mut sim, |sim| {
        if !asked && sim.net.now_us >= 2_000_000 {
            for m in [1, 2, 3] {
                sim.net.send(1, joiner, m, &join_msg(&TEST_SESSION, joiner, vec![], u64::MAX));
            }
            asked = true;
        }
    });
    report(&sim, &[1, 2, 3, 4], "join");
    vc_report(&sim, &[1, 2, 3, 4], "join");
    for m in [1, 2, 3, 4] {
        let r = &sim.replica(m, 1).sim;
        println!("  m{m}: admitted {:?}; 4's touches: {}", r.vcm.admitted, touch_state(&sim, m, joiner));
    }
    verdict(&sim, &[1, 2, 3, 4], "join (member 4 asks to join at t=2 s and touches an object at frame 420)", panic_at);
}

/// A member that withholds references (P2-4): from t = 2 s member 8's acks reference no more of anyone's
/// chain, while its horizon keeps advancing. Before, that stalled every member's confirmation for good;
/// now 1, 2 and 3 vote it out once it has held back for `suspect_after` frames.
pub fn refwithhold() {
    let ids = [1, 2, 3, 8];
    let ps = peers_with(&ids, SimConfig::default(), |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (400, touch(33, 3.0))]) } else { BTreeMap::new() });
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 12_000_000);
    let i8 = sim.peers.iter().position(|p| p.id == 8).unwrap();
    let mut hiding = false;
    let panic_at = run_to_end(&mut sim, |sim| {
        if !hiding && sim.net.now_us >= 2_000_000 {
            sim.peers[i8].worlds.get_mut(&1).unwrap().sim.freeze_others();
            hiding = true;
        }
    });
    report(&sim, &[1, 2, 3], "refwithhold");
    vc_report(&sim, &[1, 2, 3], "refwithhold");
    verdict(&sim, &[1, 2, 3], "refwithhold (8 stops referencing others' chains at t=2 s)", panic_at);
}
