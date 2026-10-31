//! P2-4 review round 3: the fixtures for design-reviewer's round-2 findings (review-p2-4.md,
//! "Round 2"). Each runs twice: once with its fix switched off (`WorldSim::fixes_off`), reported as an
//! informational line, and once as shipped, with the RESULT verdict.
//! * `ghostkick` (VCR-1): 9 votes against 2 for a kick that does not exist.
//! * `selfkickhead` (VCR-2): admin 1 kicks 4; 4's vote on its own kick names a head it never signed.
//! * `ackgap` (VCR-3): 3 acks once a second while two decisions fall between two of its acks.
//! * `latejoin` (VCR-3): a joiner that catches up on two decisions at once, then joins at epoch 2.
//! * `capchurn` (VCR-4): 9 signs a capped precommit for no proposal in every epoch, and its acks
//!   claim the next epoch.
//! * `deepfork` (VCR-5) and `cutbackdate` (VCR-6): `fork9` with a cut branch 3 must fetch more than
//!   16 entries of, and with a backdated touch on the cut branch.
//! * `joinattests` (VCR-7): a joiner shows a friendship to 1 and 2 only; 9 prevotes but never
//!   precommits.
//! * `slowadmin` (VCR-8): a follower decides from precommits before it holds the admin entry.
//! * `kickbackdate` (VCR-9): `kickwar`, with 2's kick backdated to come first.
//! * `lagprecommit` (VCR-12): a member that acks once every 1.5 s precommits a join that needs it.
//! * `aheadlink` (VCR-12): a member cut off from all but one other member while that one moves on.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use pe_session_sim::*;
use pe_sim_core::session::*;

use crate::p24r::{before_fix, confs, each_sim, fork_run, guests, outcomes, peers_of, roles_line, run_quiet, sim_mut, GUEST, SCENE};
use crate::review2::{link, plan, touch, verdict};
use crate::review3::report;
use crate::viewchange::{sim_secs, vc_report};

/// Each repro: the run with its fix switched off, then the run as shipped.
const RUNS: [bool; 2] = [false, true];

fn send_all(sim: &mut Sim, from: MemberId, to: &[MemberId], e: &Entry) {
    for &m in to {
        if m != from {
            sim.net.send(1, from, m, &Msg::Entry(e.clone()));
        }
    }
}

/// The newest entry of `author`'s chain that replica `m` holds: (seq, id).
fn head_of(sim: &Sim, m: MemberId, author: MemberId) -> (u32, EntryId) {
    sim.replica(m, 1).sim.all_entries().into_iter().filter(|e| e.member == author).map(|e| (e.seq, e.id())).max().unwrap_or((0, 0))
}

pub fn ghostkick() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 9];
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (500, touch(33, 3.0))]) } else { BTreeMap::new() });
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VCR1);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
        let mut sent = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if !sent && sim.net.now_us >= 2_000_000 {
                let frozen = head_of(sim, 9, 2);
                let r9 = sim_mut(sim, 9);
                let epoch = r9.epoch;
                let e = r9.author(Payload::Vote { epoch, target: 2, seq: 0, frozen, kick: 0xDEAD_BEEF }, None);
                send_all(sim, 9, &ids, &e);
                sent = true;
            }
        });
        let honest = [1, 2, 3];
        let label = "ghostkick (9 votes against 2 at t=2 s, naming a kick that does not exist)";
        if !fixed {
            before_fix(label, "VCR-1's fix", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "ghostkick");
        println!("RESULT ghostkick: {}", confs(&sim, &honest));
        verdict(&sim, &honest, label, stopped);
    }
}

pub fn selfkickhead() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 4];
        let mut ps = peers_of(&ids, |m| match m {
            1 => plan(&[(60, touch(10, 6.0)), (120, Action::Kick { target: 4 })]),
            2 => plan(&[(400, touch(33, 3.0))]),
            _ => BTreeMap::new(),
        });
        // 4 casts no vote of its own: the fixture signs its forged one.
        for p in ps.iter_mut().filter(|p| p.id == 4) {
            p.worlds.get_mut(&1).unwrap().sim.withhold_vc = true;
        }
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VCR2);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
        let mut sent = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if sent {
                return;
            }
            let r4 = &sim.replica(4, 1).sim;
            let Some(kick) = r4.all_entries().into_iter().find(|e| matches!(e.payload, Payload::Kick { target: 4, .. })).map(|e| e.id()) else { return };
            let r4 = sim_mut(sim, 4);
            let epoch = r4.epoch;
            let e = r4.author(Payload::Vote { epoch, target: 4, seq: 0, frozen: (10_000, 0xBAD), kick }, None);
            send_all(sim, 4, &ids, &e);
            sent = true;
        });
        let honest = [1, 2, 3];
        let label = "selfkickhead (admin 1 kicks 4 at 2 s; 4's vote on its own kick names a head it never signed)";
        let removed = sim.replica(1, 1).sim.vc_log.iter().find(|d| d.3.contains(&4)).map(|d| sim_secs(sim.replica(1, 1), d.1));
        if !fixed {
            println!("  4 removed at m1: {removed:?} s");
            before_fix(label, "VCR-2's fix", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "selfkickhead");
        println!("RESULT selfkickhead: 4 removed at {} -> {}", removed.map_or("never".to_string(), |t| format!("{t:.2} s")), if removed.is_some() { "KICKED" } else { "FAIL" });
        verdict(&sim, &honest, label, stopped);
    }
}

pub fn ackgap() {
    for fixed in RUNS {
        let ids = [1, 2, 3];
        let mut ps = peers_of(&ids, |m| match m {
            1 => plan(&[
                (60, touch(10, 6.0)),
                (125, Action::DefineRole { role: GUEST, perms: 0 }),
                (140, Action::DefineRole { role: GUEST + 1, perms: 0 }),
                (500, touch(33, 3.0)),
            ]),
            _ => BTreeMap::new(),
        });
        for p in ps.iter_mut().filter(|p| p.id == 3) {
            p.worlds.get_mut(&1).unwrap().ack_every_us = 1_000_000;
        }
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VCR3);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 9_000_000);
        let stopped = run_quiet(&mut sim, |_| {});
        let honest = [1, 2, 3];
        let label = "ackgap (3 acks once a second; two decisions fall between two of its acks)";
        if !fixed {
            before_fix(label, "VCR-3's fix", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "ackgap");
        println!("RESULT ackgap: {}", confs(&sim, &honest));
        verdict(&sim, &honest, label, stopped);
    }
}

pub fn latejoin() {
    for fixed in RUNS {
        let ids = [1, 2, 3];
        let joiner: MemberId = 5;
        let mut ps = peers_of(&ids, |m| match m {
            1 => plan(&[
                (40, touch(10, 6.0)),
                (60, Action::DefineRole { role: GUEST, perms: 0 }),
                (100, Action::DefineRole { role: GUEST + 1, perms: 0 }),
                (500, touch(33, 3.0)),
            ]),
            _ => BTreeMap::new(),
        });
        let mut p = Peer::new(joiner, 0);
        p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
        ps.push(p);
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VCR3);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
        let mut asked = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if !asked && sim.net.now_us >= 3_000_000 {
                // The joiner asks, and member 1 hands it the whole log at once: it catches up on both
                // decisions within a tick.
                for m in ids {
                    sim.net.send(1, joiner, m, &Msg::Join { member: joiner, attests: vec![] });
                }
                for e in sim.replica(1, 1).sim.all_entries() {
                    sim.net.send(1, 1, joiner, &Msg::Entry(e));
                }
                asked = true;
            }
        });
        let all = [1, 2, 3, 5];
        let label = "latejoin (two decisions, then 5 catches up on both at once and joins at epoch 2)";
        if !fixed {
            let admitted = sim.replica(1, 1).sim.members.contains(&joiner);
            println!("  joiner admitted at m1: {admitted}");
            before_fix(label, "VCR-3's fix", &sim, &all, stopped);
            continue;
        }
        vc_report(&sim, &all, "latejoin");
        println!("RESULT latejoin: {}", confs(&sim, &all));
        verdict(&sim, &all, label, stopped);
    }
}

pub fn capchurn() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 9];
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (700, touch(33, 3.0))]) } else { BTreeMap::new() });
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VCR4);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 14_000_000);
        let mut capped: BTreeSet<u32> = BTreeSet::new();
        let stopped = run_quiet(&mut sim, |sim| {
            if sim.net.now_us < 2_000_000 || !sim.replica(1, 1).sim.members.contains(&9) {
                return;
            }
            let r9 = sim_mut(sim, 9);
            let epoch = r9.epoch;
            // Its acks claim the epoch after its own, so they rise by one per decision.
            r9.fake_ack_epoch = Some(epoch + 1);
            if capped.insert(epoch) {
                let boundary = r9.now + 30;
                let e = r9.author(Payload::Precommit { epoch, round: 0, value: 0xBAD0 ^ epoch as u64, boundary }, None);
                send_all(sim, 9, &ids, &e);
            }
        });
        let honest = [1, 2, 3];
        let label = "capchurn (from 2 s, 9 signs a capped precommit for no proposal in every epoch; its acks claim the next one)";
        let r1 = sim.replica(1, 1);
        let removed = r1.sim.vc_log.iter().find(|d| d.3.contains(&9)).map(|d| sim_secs(r1, d.1));
        let resets = r1.sim.stats.resets;
        if !fixed {
            println!("  9 removed: {removed:?} s; resets {resets}");
            before_fix(label, "VCR-4's fix", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "capchurn");
        println!(
            "RESULT capchurn: {} resets, 9 removed at {} -> {}",
            resets,
            removed.map_or("never".to_string(), |t| format!("{t:.2} s")),
            if removed.is_some() { "VOTED OUT" } else { "FAIL" }
        );
        println!("RESULT capchurn: {}", confs(&sim, &honest));
        verdict(&sim, &honest, label, stopped);
    }
}

pub fn deepfork() {
    for fixed in RUNS {
        let off = if fixed { 0 } else { fix::VCR5 };
        let (sim, stopped, info) = fork_run(off, 1_600_000, false, 1_500_000);
        let honest = [1, 2, 3];
        let label = "deepfork (fork9, with 3's entries reaching 1 and 2 1.5 s late: they find the fork late, and the cut lies over 16 entries past what 3 holds of A)";
        if !fixed {
            before_fix(label, "VCR-5's fix", &sim, &honest, stopped);
            continue;
        }
        println!("  {info}");
        report(&sim, &honest, "deepfork");
        vc_report(&sim, &honest, "deepfork");
        verdict(&sim, &honest, label, stopped);
    }
}

pub fn cutbackdate() {
    for fixed in RUNS {
        let off = if fixed { 0 } else { fix::VCR6 };
        let (sim, stopped, info) = fork_run(off, 700_000, true, 0);
        let honest = [1, 2, 3];
        let label = "cutbackdate (fork9 with a touch backdated 90 frames on the cut branch, which 3 does not hold)";
        if !fixed {
            before_fix(label, "VCR-6's fix", &sim, &honest, stopped);
            continue;
        }
        println!("  {info}");
        report(&sim, &honest, "cutbackdate");
        vc_report(&sim, &honest, "cutbackdate");
        verdict(&sim, &honest, label, stopped);
    }
}

pub fn joinattests() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 9];
        let joiner: MemberId = 5;
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (600, touch(33, 3.0))]) } else { BTreeMap::new() });
        let mut p = Peer::new(joiner, 0);
        p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
        ps.push(p);
        for p in ps.iter_mut().filter(|p| p.id != joiner) {
            p.worlds.get_mut(&1).unwrap().sim.follow(joiner);
        }
        each_sim(&mut ps, |s| s.join_rules = JoinRules::friends_only(GUEST));
        guests(&mut ps, &[1, 2, 3, 9]);
        for p in ps.iter_mut().filter(|p| p.id == 9) {
            let s = &mut p.worlds.get_mut(&1).unwrap().sim;
            s.byz_ally = true;
            s.byz_no_precommit = true;
        }
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VCR7);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
        let mut asked = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if !asked && sim.net.now_us >= 2_000_000 {
                let friend = vec![attest(Rel::Friend, joiner, 1, 1)];
                for m in ids {
                    let attests = if m == 1 || m == 2 { friend.clone() } else { vec![] };
                    sim.net.send(1, joiner, m, &Msg::Join { member: joiner, attests });
                }
                asked = true;
            }
        });
        let honest = [1, 2, 3];
        let label = "joinattests (5, a friend of admin 1, shows it to 1 and 2 only; 9 prevotes but never precommits)";
        let role = sim.replica(3, 1).sim.roles.get(&joiner).copied();
        if !fixed {
            println!("  5's role at m3: {role:?}");
            before_fix(label, "VCR-7's fix", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "joinattests");
        for &m in &honest {
            println!("  {}", roles_line(&sim, m));
        }
        println!("RESULT joinattests: 5's role at m3 {role:?} -> {}; {}", if role == Some(GUEST) { "ADMITTED AS GUEST" } else { "FAIL" }, confs(&sim, &honest));
        verdict(&sim, &[1, 2, 3, 5], label, stopped);
    }
}

pub fn slowadmin() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 4];
        let follower: MemberId = 6;
        // The admin entry's author is not the first proposer, so 6 gets the proposal on time.
        let proposer = peers_of(&ids, |_| BTreeMap::new())[0].worlds[&1].sim.proposer_of(0, 0).unwrap();
        let admin = *ids.iter().find(|&&m| m != proposer).unwrap();
        let mut ps = peers_of(&ids, |m| {
            if m == admin {
                plan(&[(60, touch(10, 6.0)), (120, Action::DefineRole { role: GUEST, perms: 0 }), (400, touch(33, 3.0))])
            } else {
                BTreeMap::new()
            }
        });
        let mut p = Peer::new(follower, 0);
        p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
        ps.push(p);
        for p in ps.iter_mut().filter(|p| p.id != follower) {
            p.worlds.get_mut(&1).unwrap().sim.follow(follower);
        }
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VCR8);
        }
        let mut net = Net::new(link(30, 10, 0), 7);
        // The admin's entries reach 6 2.5 s late on every path (fetch replies from the others included).
        net.filter = Some(Box::new(move |t, _w, from, to, m| {
            let during = (1_900_000..3_500_000).contains(&t);
            match m {
                Msg::Entry(e) if during && to == 6 && e.member == admin => Fate::Delay(2_500_000),
                _ if during && from == admin && to == 6 => Fate::Delay(2_500_000),
                _ => Fate::Deliver,
            }
        }));
        let mut sim = Sim::new(net, ps, 9_000_000);
        let stopped = run_quiet(&mut sim, |_| {});
        let all = [1, 2, 3, 4, 6];
        let label = "slowadmin (an admin's entries reach follower 6 2.5 s late on every path, so it sees the precommits first)";
        if !fixed {
            before_fix(label, "VCR-8's fix", &sim, &all, stopped);
            continue;
        }
        vc_report(&sim, &all, "slowadmin");
        let defs: Vec<String> = all.iter().map(|&m| format!("m{m} {:?}", sim.replica(m, 1).sim.role_defs)).collect();
        println!("RESULT slowadmin: role definitions {}", defs.join(", "));
        verdict(&sim, &all, label, stopped);
    }
}

pub fn kickbackdate() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 4];
        let mut ps = peers_of(&ids, |m| match m {
            1 => plan(&[(60, touch(10, 6.0)), (120, Action::Kick { target: 2 })]),
            3 => plan(&[(400, touch(33, 3.0))]),
            _ => BTreeMap::new(),
        });
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VCR9);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 8_000_000);
        let mut sent = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if !sent && sim.replica(2, 1).sim.frame >= 120 {
                let r2 = sim_mut(sim, 2);
                let epoch = r2.epoch;
                let e = r2.author_backdated(Payload::Kick { epoch, target: 1 }, 30);
                send_all(sim, 2, &ids, &e);
                sent = true;
            }
        });
        let left: Vec<MemberId> = sim.replica(3, 1).sim.members.iter().copied().collect();
        let label = "kickbackdate (admins 1 and 2 kick each other at frame 120; 2 backdates its kick 30 frames to come first)";
        if !fixed {
            println!("  members left at m3: {left:?}");
            before_fix(label, "VCR-9's fix", &sim, &left, stopped);
            continue;
        }
        vc_report(&sim, &left, "kickbackdate");
        for &m in &left {
            println!("RESULT kickbackdate: m{m}: {}", outcomes(&sim, m));
        }
        let ok = left == [1, 3, 4];
        println!("RESULT kickbackdate: members left {left:?} -> {}", if ok { "THE BACKDATED KICK IS VOID: 2 removed" } else { "FAIL" });
        verdict(&sim, &left, label, stopped);
    }
}

pub fn lagprecommit() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 9];
        let joiner: MemberId = 5;
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (600, touch(33, 3.0))]) } else { BTreeMap::new() });
        let mut p = Peer::new(joiner, 0);
        p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
        ps.push(p);
        for p in ps.iter_mut().filter(|p| p.id != joiner) {
            p.worlds.get_mut(&1).unwrap().sim.follow(joiner);
        }
        for p in ps.iter_mut() {
            let r = p.worlds.get_mut(&1).unwrap();
            if p.id == 3 {
                r.ack_every_us = 1_500_000;
            }
            if p.id == 9 {
                r.sim.withhold_vc = true;
            }
        }
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VCR12);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
        let mut asked = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if !asked && sim.net.now_us >= 2_800_000 {
                for m in ids {
                    sim.net.send(1, joiner, m, &Msg::Join { member: joiner, attests: vec![] });
                }
                asked = true;
            }
        });
        let all = [1, 2, 3, 5];
        let label = "lagprecommit (3 acks once every 1.5 s; 9 casts no view-change vote, so the join needs 3's precommit)";
        let admitted = sim.replica(1, 1).sim.members.contains(&joiner);
        if !fixed {
            println!("  joiner admitted at m1: {admitted}");
            before_fix(label, "VCR-12's fix", &sim, &[1, 2, 3], stopped);
            continue;
        }
        vc_report(&sim, &all, "lagprecommit");
        println!("RESULT lagprecommit: joiner admitted {admitted} -> {}; {}", if admitted { "JOINED" } else { "FAIL" }, confs(&sim, &all));
        verdict(&sim, &all, label, stopped);
    }
}

pub fn aheadlink() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 4];
        let mut ps = peers_of(&ids, |m| match m {
            2 => plan(&[(60, touch(10, 6.0)), (120, Action::DefineRole { role: GUEST, perms: 0 }), (600, touch(33, 3.0))]),
            _ => BTreeMap::new(),
        });
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VCR12);
        }
        let mut net = Net::new(link(30, 10, 0), 7);
        // 3 hears only 4's own acks on time from 1.9 s to 7 s; everything else reaches it 5 s late.
        net.filter = Some(Box::new(|t, _w, from, to, m| {
            let during = (1_900_000..7_000_000).contains(&t);
            match m {
                Msg::Entry(e) if during && to == 3 && !(from == 4 && e.member == 4 && matches!(e.payload, Payload::Ack { .. })) => Fate::Delay(5_000_000),
                _ if during && to == 3 && from != 4 => Fate::Delay(5_000_000),
                _ => Fate::Deliver,
            }
        }));
        let mut sim = Sim::new(net, ps, 14_000_000);
        let stopped = run_quiet(&mut sim, |_| {});
        let honest = [1, 2, 3, 4];
        let r3 = &sim.replica(3, 1).sim;
        let votes: Vec<MemberId> = r3
            .all_entries()
            .into_iter()
            .filter(|e| e.member == 3)
            .filter_map(|e| match e.payload {
                Payload::Vote { target, kick: 0, .. } => Some(target),
                _ => None,
            })
            .collect();
        let label = "aheadlink (3 hears only 4's acks for 5 s while the others decide an epoch; it is removed for not keeping up)";
        if !fixed {
            println!("  3's votes: {votes:?}");
            before_fix(label, "VCR-12's fix", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "aheadlink");
        // 3's votes are cast in an epoch the others have left, so none of the members ahead is removed.
        let members = sim.replica(1, 1).sim.members.clone();
        let kept = [1, 2, 4].iter().all(|m| members.contains(m));
        println!(
            "RESULT aheadlink: 3's votes against the members ahead {votes:?}; members {members:?} -> {}",
            if kept { "THE MEMBERS AHEAD ARE KEPT" } else { "FAIL" }
        );
        verdict(&sim, &honest, label, stopped);
    }
}

#[allow(dead_code)]
fn unused(_: &Rc<Cell<u64>>, _: &Rc<RefCell<u64>>) {}
