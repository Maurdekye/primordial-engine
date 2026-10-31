//! P2-4 review round 2 (design-reviewer's review-p2-4.md) and the D80 admin layer (user rulings
//! 22:11Z-22:51Z; design 10.8).
//!
//! The review repros run twice: once with their fix switched off (`WorldSim::fixes_off`), reported
//! as an informational line, and once as shipped, with the RESULT verdict.
//! * `fork9` (VC-2): 9 forks its chain. 1 and 2 hold branch A, with a touch; 3 holds branch B,
//!   without it and longer. The change cuts 9 at A's head; 3 must not confirm B's frames.
//! * `jointiming` (VC-3): prevotes reach 3 and 4 1.5 s late, past the join's boundary, after 1 and 2
//!   have locked on the join. It must still be decided.
//! * `bogusprecommit` (VC-4): 9 signs a precommit whose boundary is 1 (part 1), or just ahead of its
//!   horizon (part 2). Confirmation must not stop for good.
//! * `futureack` (VC-5): 9's acks claim epoch 999 (part 1), or epoch 1 with no decision (part 2).
//!
//! The D80 admin layer:
//! * `kickwar`: two admins kick each other at the same frame; commit order removes one.
//! * `kickpartition`: an admin's kick during a partition waits it out, then holds.
//! * `lastadmin`: the only admin's release is refused; handing the role on, then releasing, works.
//! * `adminclose`: the only admin falls silent; removing it closes the session.
//! * `joinrules`: the join matrix: a friend of an admin as Guest, a stranger refused, an owner's
//!   confidant as Admin, an owner by row 0.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use pe_session_sim::*;
use pe_session::session::*;

use crate::review2::{link, plan, run_to_end, touch, verdict};
use crate::review3::report;
use crate::viewchange::{sim_secs, vc_report};

/// A custom role with no permissions (the scenarios' guests).
pub(crate) const GUEST: RoleId = 1;
pub(crate) const SCENE: SceneKind = SceneKind::Pile { side: 8, layers: 4 };

pub(crate) fn peers_of(ids: &[MemberId], plans: impl Fn(MemberId) -> BTreeMap<u64, Vec<Action>>) -> Vec<Peer> {
    ids.iter()
        .map(|&id| {
            let mut p = Peer::new(id, 0);
            p.join(1, ids, SCENE, SimConfig::default(), 100_000, plans(id));
            p
        })
        .collect()
}

pub(crate) fn each_sim(ps: &mut [Peer], f: impl Fn(&mut WorldSim)) {
    for p in ps.iter_mut() {
        if let Some(r) = p.worlds.get_mut(&1) {
            f(&mut r.sim);
        }
    }
}

pub(crate) fn sim_mut(sim: &mut Sim, m: MemberId) -> &mut WorldSim {
    let i = sim.peers.iter().position(|p| p.id == m).unwrap();
    &mut sim.peers[i].worlds.get_mut(&1).unwrap().sim
}

/// Whether every listed replica equals the offline replay, with no false check, printing nothing a
/// suite check reads: for the runs with a fix switched off.
fn quiet_equal(sim: &Sim, honest: &[MemberId]) -> bool {
    let frames = honest.iter().map(|&m| sim.replica(m, 1).sim.frame).min().unwrap_or(0).saturating_sub(1);
    let first = &sim.replica(honest[0], 1).sim;
    let members: Vec<MemberId> = first.vcm.members.iter().copied().collect();
    let reference = replay(1, SCENE, first.cfg, first.public_keys(), &members, &final_log(first), frames);
    let rd = digest(&reference, frames);
    let bad: u64 = honest.iter().map(|&m| sim.replica(m, 1).sim.stats.checks_bad).sum();
    honest.iter().all(|&m| digest(&sim.replica(m, 1).sim.hashes, frames) == rd) && bad == 0
}

/// Run to the end with a stopped run reported, not printed: a run with a fix switched off may trip
/// the confirmed-frame assertion.
pub(crate) fn run_quiet(sim: &mut Sim, each_ms: impl FnMut(&mut Sim)) -> Option<u64> {
    if std::env::var("PE_SHOW_PANIC").is_ok_and(|v| !v.is_empty()) {
        return run_to_end(sim, each_ms);
    }
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = run_to_end(sim, each_ms);
    std::panic::set_hook(hook);
    r
}

pub(crate) fn confs(sim: &Sim, ids: &[MemberId]) -> String {
    ids.iter()
        .map(|&m| {
            let r = &sim.replica(m, 1).sim;
            format!("m{m} conf {} of frame {}", r.conf, r.frame)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The line a run with its fix switched off ends with.
pub(crate) fn before_fix(label: &str, fix: &str, sim: &Sim, honest: &[MemberId], stopped: Option<u64>) {
    let state = match stopped {
        Some(t) => format!("the run stopped at t={t} ms: a replica hit an assertion or a panic"),
        None if quiet_equal(sim, honest) => "every replica equals the replay".to_string(),
        None => "the replicas do not all equal the replay".to_string(),
    };
    let r = &sim.replica(honest[0], 1).sim;
    println!(
        "  {label} with {fix} switched off (the behaviour before the fix): {state}; {}; at m{}: members {:?}, evicted {:?}",
        confs(sim, honest),
        honest[0],
        r.vcm.members,
        r.members_evicted().keys().collect::<Vec<_>>()
    );
}

// ---- review repros ---------------------------------------------------------------------------------

pub fn fork9() {
    for fixed in [false, true] {
        let (sim, stopped, info) = fork_run(if fixed { 0 } else { fix::VC2 }, 700_000, false, 0);
        let honest = [1, 2, 3];
        let label = "fork9 (9 forks: A with its touch to 1 and 2, a longer B without it to 3)";
        if !fixed {
            before_fix(label, "VC-2's fix", &sim, &honest, stopped);
            continue;
        }
        println!("  {info}");
        report(&sim, &honest, "fork9");
        vc_report(&sim, &honest, "fork9");
        verdict(&sim, &honest, label, stopped);
    }
}

/// `fork9`'s run: 9 forks at frame 110 and stays alive `live_us` (A to 1 and 2, B to 3), with `fixes_off`
/// switched off. With `backdate`, 9 also signs, on A only, a touch backdated 90 frames 100 ms after the
/// fork. With `detect_us`, everything 3 sends 1 and 2 in the `detect_us` after the fork arrives that late,
/// so they find the fork late and cut A deeper (round 3: `deepfork`, `cutbackdate`).
pub(crate) fn fork_run(fixes_off: u64, live_us: u64, backdate: bool, detect_us: u64) -> (Sim, Option<u64>, String) {
    {
        let ids = [1, 2, 3, 9];
        let mut ps = peers_of(&ids, |m| match m {
            9 => plan(&[(120, touch(10, 6.0))]),
            1 => plan(&[(60, touch(33, 3.0))]),
            _ => BTreeMap::new(),
        });
        each_sim(&mut ps, |s| s.fixes_off = fixes_off);
        let fork_t: Rc<Cell<u64>> = Rc::new(Cell::new(u64::MAX));
        let fork_seq: Rc<Cell<u32>> = Rc::new(Cell::new(u32::MAX));
        let crafted: Rc<RefCell<BTreeSet<EntryId>>> = Rc::default();
        let mut net = Net::new(link(30, 10, 0), 7);
        let (ft, fs, cr) = (fork_t.clone(), fork_seq.clone(), crafted.clone());
        net.filter = Some(Box::new(move |t, _w, from, to, m| {
            let silent = t >= ft.get().saturating_add(live_us);
            match m {
                Msg::Entry(e) if from == 9 && t >= ft.get() => {
                    if cr.borrow().contains(&e.id()) {
                        // With `detect_us`, 9 does not relay B (it gets B back from 3's fetch replies):
                        // 1 and 2 learn of the fork only from 3, late.
                        if detect_us > 0 && to != 3 {
                            Fate::Drop
                        } else {
                            Fate::Deliver
                        }
                    } else if silent || (to == 3 && e.member == 9 && e.seq >= fs.get()) {
                        Fate::Drop
                    } else {
                        Fate::Deliver
                    }
                }
                _ if detect_us > 0 && from == 3 && (to == 1 || to == 2) && t >= ft.get() && t < ft.get().saturating_add(detect_us) => Fate::Delay(detect_us),
                Msg::Entry(e) if from == 3 && matches!(e.payload, Payload::Vote { .. }) => Fate::Delay(300_000),
                _ if from == 9 && silent => Fate::Drop,
                _ => Fate::Deliver,
            }
        }));
        let mut sim = Sim::new(net, ps, 7_300_000 + live_us);
        let mut b_id: BTreeMap<u32, EntryId> = BTreeMap::new();
        let mut backdated = !backdate;
        let mut b_last: Option<Entry> = None;
        let mut b_ack: Option<Payload> = None;
        let (mut extra, mut next_t) = (0u32, 0u64);
        let stopped = run_quiet(&mut sim, |sim| {
            let now = sim.net.now_us;
            if fork_seq.get() == u32::MAX {
                let r9 = &sim.replica(9, 1).sim;
                if r9.frame >= 110 {
                    let top = r9.all_entries().iter().filter(|e| e.member == 9).map(|e| e.seq).max().unwrap_or(0);
                    fork_seq.set(top + 1);
                    fork_t.set(now);
                }
                return;
            }
            let s0 = fork_seq.get();
            if !backdated && now >= fork_t.get() + 100_000 {
                // On A only (the filter keeps 9's chain past the fork from 3): a touch backdated 90
                // frames, a horizon violation on that branch.
                let e = sim_mut(sim, 9).author_backdated(Payload::Touch { obj: 20, impulse: [0.0, 4.0, 0.0], grab: false }, 90);
                for m in [1, 2] {
                    sim.net.send(1, 9, m, &Msg::Entry(e.clone()));
                }
                backdated = true;
            }
            if now < fork_t.get() + live_us {
                // Branch B: 9's entries from the fork on, re-linked, with its touch a no-op.
                let mut mine: Vec<Entry> =
                    sim.replica(9, 1).sim.all_entries().into_iter().filter(|e| e.member == 9 && e.seq >= s0 && !b_id.contains_key(&e.seq)).collect();
                mine.sort_by_key(|e| e.seq);
                for e in mine {
                    let mut b = e.clone();
                    if b.seq > s0 {
                        b.prev = b_id[&(b.seq - 1)];
                    }
                    if matches!(b.payload, Payload::Touch { .. }) {
                        b.payload = Payload::Attest { fact: Fact::ZERO, sig: [0; 64] };
                        b.carried = None;
                    }
                    if matches!(b.payload, Payload::Ack { .. }) {
                        b_ack = Some(b.payload.clone());
                    }
                    b_id.insert(b.seq, b.id());
                    crafted.borrow_mut().insert(b.id());
                    sim.net.send(1, 9, 3, &Msg::Entry(test_sign(1, b.clone())));
                    b_last = Some(b);
                }
            } else if extra < 10 && now >= next_t {
                // Ten more acks on B only, so 3's copy of 9's chain is longer than A.
                let (Some(last), Some(Payload::Ack { horizon, heads, check, epoch })) = (b_last.clone(), b_ack.clone()) else { return };
                let b = Entry {
                    member: 9,
                    seq: last.seq + 1,
                    prev: last.id(),
                    stamp: last.stamp + 3,
                    payload: Payload::Ack { horizon: horizon + 3, heads, check, epoch },
                    carried: None,
                };
                b_ack = Some(b.payload.clone());
                crafted.borrow_mut().insert(b.id());
                sim.net.send(1, 9, 3, &Msg::Entry(test_sign(1, b.clone())));
                b_last = Some(b);
                extra += 1;
                next_t = now + 50_000;
            }
        });
        let info = format!("fork at 9's seq {}, t={} ms; branch B {} entries plus {extra} extra acks", fork_seq.get(), fork_t.get() / 1000, b_id.len());
        (sim, stopped, info)
    }
}

pub fn jointiming() {
    for fixed in [false, true] {
        let ids = [1, 2, 3, 4];
        let joiner: MemberId = 5;
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (500, touch(33, 3.0))]) } else { BTreeMap::new() });
        let mut p = Peer::new(joiner, 0);
        p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
        ps.push(p);
        for p in ps.iter_mut().filter(|p| p.id != joiner) {
            p.worlds.get_mut(&1).unwrap().sim.follow(joiner);
        }
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VC3);
        }
        let mut net = Net::new(link(30, 10, 0), 7);
        net.filter = Some(Box::new(|t, _w, from, to, m| match m {
            Msg::Entry(e) if matches!(e.payload, Payload::Prevote { .. }) && (to == 3 || to == 4) && from != to && t < 5_000_000 => Fate::Delay(1_500_000),
            _ => Fate::Deliver,
        }));
        let mut sim = Sim::new(net, ps, 12_000_000);
        let mut asked = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if !asked && sim.net.now_us >= 2_000_000 {
                for m in ids {
                    sim.net.send(1, joiner, m, &join_msg(&TEST_SESSION, joiner, vec![], u64::MAX));
                }
                asked = true;
            }
        });
        let all = [1, 2, 3, 4, 5];
        let label = "jointiming (5 joins; prevotes reach 3 and 4 1.5 s late, past the boundary)";
        if !fixed {
            let admitted = sim.replica(1, 1).sim.vcm.members.contains(&joiner);
            println!("  joiner admitted at m1: {admitted}");
            before_fix(label, "VC-3's fix", &sim, &[1, 2, 3, 4], stopped);
            continue;
        }
        report(&sim, &all, "jointiming");
        vc_report(&sim, &all, "jointiming");
        verdict(&sim, &all, label, stopped);
    }
}

pub fn bogusprecommit() {
    for part in [1u32, 2] {
        for fixed in [false, true] {
            let ids = [1, 2, 3, 9];
            let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (400, touch(33, 3.0))]) } else { BTreeMap::new() });
            if !fixed {
                // Round 4's cap rule (VC3R-2) would vote 9 out as well.
                each_sim(&mut ps, |s| s.fixes_off = fix::VC4 | fix::VC3R2);
            }
            let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 9_000_000);
            let mut sent = false;
            let stopped = run_quiet(&mut sim, |sim| {
                if !sent && sim.net.now_us >= 2_000_000 {
                    let r9 = sim_mut(sim, 9);
                    let boundary = if part == 1 { 1 } else { r9.now + 30 };
                    let sig = test_fact_sig(9, 1, vc_fact(0, 0, 0xBAD0));
                    let e = r9.author(Payload::Precommit { epoch: 0, round: 0, value: 0xBAD0, boundary, polka: vec![], sig }, None);
                    for m in [1, 2, 3] {
                        sim.net.send(1, 9, m, &Msg::Entry(e.clone()));
                    }
                    sent = true;
                }
            });
            let honest = [1, 2, 3];
            let label = format!("bogusprecommit part {part} (9 signs a precommit with boundary {})", if part == 1 { "1" } else { "just ahead of its horizon" });
            if !fixed {
                before_fix(&label, "VC-4's fix", &sim, &honest, stopped);
                continue;
            }
            report(&sim, &honest, &format!("bogusprecommit-{part}"));
            vc_report(&sim, &honest, &format!("bogusprecommit-{part}"));
            println!("RESULT bogusprecommit-{part}: {}", confs(&sim, &honest));
            verdict(&sim, &honest, &label, stopped);
        }
    }
}

pub fn futureack() {
    for part in [1u32, 2] {
        for fixed in [false, true] {
            let ids = [1, 2, 3, 9];
            let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (600, touch(33, 3.0))]) } else { BTreeMap::new() });
            if !fixed {
                each_sim(&mut ps, |s| s.fixes_off = fix::VC5);
            }
            let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 12_000_000);
            let mut set = false;
            let stopped = run_quiet(&mut sim, |sim| {
                if !set && sim.net.now_us >= 2_000_000 {
                    sim_mut(sim, 9).vcm.fake_ack_epoch = Some(if part == 1 { 999 } else { 1 });
                    set = true;
                }
            });
            let honest = [1, 2, 3];
            let label = format!("futureack part {part} (from t=2 s 9's acks claim epoch {})", if part == 1 { "999" } else { "1, with no decision" });
            if !fixed {
                before_fix(&label, "VC-5's fix", &sim, &honest, stopped);
                continue;
            }
            report(&sim, &honest, &format!("futureack-{part}"));
            vc_report(&sim, &honest, &format!("futureack-{part}"));
            println!("RESULT futureack-{part}: {}", confs(&sim, &honest));
            verdict(&sim, &honest, &label, stopped);
        }
    }
}

// ---- D80: admins and roles --------------------------------------------------------------------------

pub(crate) fn outcomes(sim: &Sim, m: MemberId) -> String {
    let r = &sim.replica(m, 1).sim;
    let mut v: Vec<(u64, String)> = r
        .vcm.admin_outcomes
        .iter()
        .filter_map(|(id, ok)| {
            let e = r.all_entries().into_iter().find(|e| e.id() == *id)?;
            let what = match e.payload {
                Payload::Kick { target, .. } => format!("{} kicks {target}", e.member),
                Payload::SetRole { member, role, .. } => format!("{} gives {member} role {role}", e.member),
                Payload::DefineRole { role, perms, .. } => format!("{} defines role {role} = {perms:#x}", e.member),
                _ => return None,
            };
            Some((e.stamp, format!("{what} @{}: {}", e.stamp, if *ok { "applied" } else { "refused" })))
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join("; ")
}

pub(crate) fn roles_line(sim: &Sim, m: MemberId) -> String {
    let r = &sim.replica(m, 1).sim;
    format!("m{m}: members {:?} roles {:?} admins {:?} closed at {:?}", r.vcm.members, r.vcm.roles, r.admins(), r.vcm.closed_at)
}

pub fn kickwar() {
    let ids = [1, 2, 3, 4];
    let ps = peers_of(&ids, |m| match m {
        1 => plan(&[(60, touch(10, 6.0)), (120, Action::Kick { target: 2 })]),
        2 => plan(&[(120, Action::Kick { target: 1 })]),
        3 => plan(&[(400, touch(33, 3.0))]),
        _ => BTreeMap::new(),
    });
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 8_000_000);
    let stopped = run_to_end(&mut sim, |_| {});
    let left: Vec<MemberId> = sim.replica(3, 1).sim.vcm.members.iter().copied().collect();
    let honest: Vec<MemberId> = left.clone();
    vc_report(&sim, &honest, "kickwar");
    for &m in &honest {
        println!("RESULT kickwar: m{m}: {}", outcomes(&sim, m));
        println!("  {}", roles_line(&sim, m));
    }
    let one = left.contains(&1) != left.contains(&2);
    println!("RESULT kickwar: members left {left:?}: exactly one of the two admins removed: {one}{}", if one { "" } else { " -> FAIL" });
    verdict(&sim, &honest, "kickwar (admins 1 and 2 kick each other at frame 120)", stopped);
}

pub fn kickpartition() {
    let ids = [1, 2, 3, 4];
    let ps = peers_of(&ids, |m| match m {
        1 => plan(&[(60, touch(10, 6.0)), (180, Action::Kick { target: 3 })]),
        4 => plan(&[(500, touch(33, 3.0))]),
        _ => BTreeMap::new(),
    });
    let mut net = Net::new(link(30, 10, 0), 7);
    let side = |m: MemberId| if m <= 2 { 1 } else { 2 };
    net.filter = Some(Box::new(move |t, _w, from, to, _m| if (2_000_000..6_000_000).contains(&t) && side(from) != side(to) { Fate::Drop } else { Fate::Deliver }));
    let mut sim = Sim::new(net, ps, 12_000_000);
    let stopped = run_to_end(&mut sim, |_| {});
    let honest = [1, 2, 4];
    vc_report(&sim, &honest, "kickpartition");
    for &m in &honest {
        println!("RESULT kickpartition: m{m}: {}", outcomes(&sim, m));
    }
    let rep1 = sim.replica(1, 1);
    let during = rep1.sim.vcm.vc_log.iter().any(|d| sim_secs(rep1, d.1) < 6.0);
    let (a, b) = (sim.replica(1, 1).sim.vcm.members.clone(), sim.replica(4, 1).sim.vcm.members.clone());
    println!(
        "RESULT kickpartition: decided during the partition: {during}; members after: m1 {a:?}, m4 {b:?} -> {}",
        if a == b && !a.contains(&3) && !during { "ONE SESSION, 3 removed after the heal" } else { "FAIL" }
    );
    verdict(&sim, &honest, "kickpartition (partition {1,2} | {3,4} 2-6 s; admin 1 kicks 3 at 3 s)", stopped);
}

pub(crate) fn guests(ps: &mut [Peer], admins: &[MemberId]) {
    let admins = admins.to_vec();
    each_sim(ps, move |s| {
        s.vcm.role_defs.insert(GUEST, 0);
        let ms: Vec<MemberId> = s.vcm.roles.keys().copied().collect();
        for m in ms {
            s.vcm.roles.insert(m, if admins.contains(&m) { ADMIN } else { GUEST });
        }
    });
}

pub fn lastadmin() {
    let ids = [1, 2, 3];
    let mut ps = peers_of(&ids, |m| match m {
        1 => plan(&[
            (60, touch(10, 6.0)),
            (120, Action::SetRole { member: 1, role: GUEST }),
            (240, Action::SetRole { member: 2, role: ADMIN }),
            (241, Action::SetRole { member: 1, role: GUEST }),
        ]),
        3 => plan(&[(400, touch(33, 3.0))]),
        _ => BTreeMap::new(),
    });
    guests(&mut ps, &[1]);
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 8_000_000);
    let stopped = run_to_end(&mut sim, |_| {});
    let honest = [1, 2, 3];
    vc_report(&sim, &honest, "lastadmin");
    for &m in &honest {
        println!("RESULT lastadmin: m{m}: {}", outcomes(&sim, m));
        println!("  {}", roles_line(&sim, m));
    }
    let admins: Vec<Vec<MemberId>> = honest.iter().map(|&m| sim.replica(m, 1).sim.admins()).collect();
    let ok = admins.iter().all(|a| a == &[2]);
    println!("RESULT lastadmin: admins at the end {admins:?} -> {}", if ok { "the release was refused, the hand-over held" } else { "FAIL" });
    verdict(&sim, &honest, "lastadmin (the only admin releases its role, then hands it to 2 and releases)", stopped);
}

pub fn adminclose() {
    let ids = [1, 2, 3];
    let mut ps = peers_of(&ids, |m| match m {
        2 => plan(&[(60, touch(10, 6.0)), (700, touch(33, 3.0))]),
        _ => BTreeMap::new(),
    });
    guests(&mut ps, &[1]);
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(Box::new(|t, _w, from, to, _m| if (from == 1 || to == 1) && t >= 3_000_000 { Fate::Drop } else { Fate::Deliver }));
    let mut sim = Sim::new(net, ps, 15_000_000);
    let stopped = run_to_end(&mut sim, |_| {});
    let honest = [2, 3];
    vc_report(&sim, &honest, "adminclose");
    for &m in &honest {
        println!("  {}", roles_line(&sim, m));
    }
    let closed: Vec<Option<u64>> = honest.iter().map(|&m| sim.replica(m, 1).sim.vcm.closed_at).collect();
    let ok = closed[0].is_some() && closed.iter().all(|c| *c == closed[0]);
    println!(
        "RESULT adminclose: the only admin (1) silent from 3 s; session closed at frame {:?} at m2, {:?} at m3 -> {}",
        closed[0],
        closed[1],
        if ok { "CLOSED, the same frame everywhere" } else { "FAIL" }
    );
    verdict(&sim, &honest, "adminclose (the only admin goes silent at t=3 s)", stopped);
}

pub fn joinrules() {
    let ids = [1, 2];
    let (friend, stranger, confidant, owner) = (4, 5, 6, 7);
    let joiners = [friend, stranger, confidant, owner];
    let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (500, touch(33, 3.0))]) } else { BTreeMap::new() });
    for &j in &joiners {
        let mut p = Peer::new(j, 0);
        p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
        ps.push(p);
    }
    for p in ps.iter_mut().filter(|p| ids.contains(&p.id)) {
        for &j in &joiners {
            p.worlds.get_mut(&1).unwrap().sim.follow(j);
        }
    }
    guests(&mut ps, &[1]);
    each_sim(&mut ps, |s| {
        s.vcm.owners = [owner].into();
        s.vcm.join_rules = JoinRules {
            rules: vec![
                JoinRule { target: Target::Admin, all: false, rel: Rel::Friend, role: Some(GUEST) },
                JoinRule { target: Target::Owner, all: false, rel: Rel::Confidant, role: Some(ADMIN) },
            ],
            fallback: None,
        };
    });
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
    let mut asked = false;
    let stopped = run_to_end(&mut sim, |sim| {
        if !asked && sim.net.now_us >= 2_000_000 {
            for &j in &joiners {
                let attests = match j {
                    4 => vec![attest(Rel::Friend, j, 1, 1)],
                    6 => vec![attest(Rel::Confidant, j, owner, 1)],
                    _ => vec![],
                };
                for m in ids {
                    sim.net.send(1, j, m, &join_msg(&TEST_SESSION, j, attests.clone(), u64::MAX));
                }
            }
            asked = true;
        }
    });
    let admitted: Vec<MemberId> = sim.replica(1, 1).sim.vcm.members.iter().copied().collect();
    let honest: Vec<MemberId> = admitted.clone();
    vc_report(&sim, &honest, "joinrules");
    for &m in &honest {
        println!("  {}", roles_line(&sim, m));
    }
    let roles = &sim.replica(1, 1).sim.vcm.roles;
    let ok = roles.get(&friend) == Some(&GUEST) && roles.get(&confidant) == Some(&ADMIN) && roles.get(&owner) == Some(&ADMIN) && !admitted.contains(&stranger);
    println!(
        "RESULT joinrules: admitted {admitted:?}, roles {roles:?} -> {}",
        if ok { "friend of an admin as Guest, owner's confidant as Admin, owner by row 0, stranger refused" } else { "FAIL" }
    );
    verdict(&sim, &honest, "joinrules (friends-of-admins as Guest, owners' confidants as Admin, row 0)", stopped);
}

// ---- review VC-1: a control above the bound -----------------------------------------------------------

/// Review VC-1, a control outside the suite's pass/fail check: the safety bound counts every faulty
/// member, proven or not. Six members, three of them (7, 8, 9) colluding (`byz_ally`), so t(6) = 1 is
/// exceeded. A partition {1,2} | {3} from 2.5 s; the colluders reach both sides, hide what they hear
/// of everyone, and pass 3 nothing of 1's and 2's. During it they sign second versions of their
/// touches for 1 and 2 only: 1 and 2 exclude them by proof and decide alone (a quorum of 2 over
/// {1,2,3}); 3, holding no proof, removes 1 and 2 with the colluders (4 of 6). Two different epoch-1
/// decisions: the fork the restated bound allows once it is exceeded.
pub fn onesided() {
    let ids = [1, 2, 3, 7, 8, 9];
    let mut ps = peers_of(&ids, |m| match m {
        7 | 8 | 9 => plan(&[(50, touch(10 + m, 6.0))]),
        1 => plan(&[(60, touch(33, 3.0))]),
        _ => BTreeMap::new(),
    });
    for p in ps.iter_mut().filter(|p| [7, 8, 9].contains(&p.id)) {
        p.worlds.get_mut(&1).unwrap().sim.vcm.byz_ally = true;
    }
    let crafted: Rc<RefCell<BTreeSet<EntryId>>> = Rc::default();
    let cr = crafted.clone();
    let mut net = Net::new(link(30, 10, 0), 7);
    let side = |m: MemberId| match m {
        1 | 2 => 1,
        3 => 2,
        _ => 0,
    };
    net.filter = Some(Box::new(move |t, _w, from, to, m| {
        let during = (2_500_000..12_000_000).contains(&t);
        if during && side(from) * side(to) == 2 {
            return Fate::Drop;
        }
        match m {
            Msg::Entry(e) if during && to == 3 && (e.member == 1 || e.member == 2 || cr.borrow().contains(&e.id())) => Fate::Drop,
            _ => Fate::Deliver,
        }
    }));
    let mut sim = Sim::new(net, ps, 14_000_000);
    let (mut hiding, mut forked) = (false, false);
    let stopped = run_quiet(&mut sim, |sim| {
        let now = sim.net.now_us;
        if !hiding && now >= 2_500_000 {
            for c in [7, 8, 9] {
                sim_mut(sim, c).freeze_others();
            }
            hiding = true;
        }
        if !forked && now >= 2_600_000 {
            for c in [7, 8, 9] {
                let r = &sim.replica(c, 1).sim;
                let Some(x) = r.all_entries().into_iter().find(|e| e.member == c && matches!(e.payload, Payload::Touch { .. })) else { continue };
                let mut x2 = x.clone();
                x2.payload = Payload::Touch { obj: 10 + c, impulse: [6.0, 0.0, 0.0], grab: false };
                x2.carried = None;
                crafted.borrow_mut().insert(x2.id());
                for m in [1, 2] {
                    sim.net.send(1, c, m, &Msg::Entry(test_sign(1, x2.clone())));
                }
            }
            forked = true;
        }
    });
    vc_report(&sim, &[1, 2, 3], "onesided");
    let first = |m: MemberId| sim.replica(m, 1).sim.vcm.vc_log.first().map(|d| d.3.clone());
    let (a, b) = (first(1), first(3));
    println!(
        "RESULT onesided (control: 3 of 6 collude, above t(6) = 1): epoch 1 removed {a:?} at m1, {b:?} at m3 -> {}{}",
        if a.is_some() && b.is_some() && a != b { "two different epoch-1 decisions: the session forked" } else { "no fork in this run" },
        match stopped {
            Some(t) => format!("; a replica stopped at t={t} ms"),
            None => String::new(),
        }
    );
}
