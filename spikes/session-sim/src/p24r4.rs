//! P2-4 review round 4: the fixtures for design-reviewer's round-3 findings (review-p2-4.md,
//! "Round 3") and for what round 4 changes besides. As in round 3, each repro first runs with its
//! fix switched off (`WorldSim::fixes_off`), reported as an informational line, then as shipped,
//! with the RESULT verdict.
//! * `strikeframe` (VC3R-2, the reviewer's repro): 9 runs honest code and only its network timing is
//!   hostile, in five timings. Round 3's strikes got 1 voted out in two of them.
//! * `aheadshield` (VC3R-1, the reviewer's repro): 9's acks climb an epoch with no decision while it
//!   sends each of its entries to one honest member only, in turn.
//! * `fakeref` (round 4, a check): 9 falls silent while 8's acks claim more of 9's chain than anyone
//!   holds; the withholding rule removes it all the same.
//! * `capstale` (VC3R-2): 9 signs a capped precommit for no proposal in every epoch (`capchurn`
//!   without the climbing acks).
//! * `lagchurn` (VC3R-2): 9's messages arrive 1.3 s late (past half the window) and, in every epoch,
//!   it votes against a member everyone hears, which forces a reset each time.
//! * `votechurn` (VC3R-2, the residual): 9 does the same, without lagging, for 8 s; then admin 1
//!   kicks it. What the churn costs, and that a kick ends it.
//! * `adminvoid` (VC3R-3): 9, as proposer, lists its own backdated (horizon-void) SetRole.
//! * `joinsquat` (VC3R-4): 9 sends a join request in 5's name before 5's own.
//! * `joinskip` (VC3R-6): a joiner that authors no ack before it is admitted, at epoch 2.

use std::collections::{BTreeMap, BTreeSet};

use pe_session_sim::*;
use pe_sim_core::session::*;

use crate::p24r::{before_fix, confs, each_sim, guests, peers_of, roles_line, run_quiet, sim_mut, GUEST, SCENE};
use crate::review2::{link, plan, touch, verdict};
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

/// When replica `m` decided the removal of `x`, if it did (scenario seconds).
fn removed_at(sim: &Sim, m: MemberId, x: MemberId) -> Option<f64> {
    let rep = sim.replica(m, 1);
    rep.sim.vc_log.iter().find(|d| d.3.contains(&x)).map(|d| sim_secs(rep, d.1))
}

fn secs(t: Option<f64>) -> String {
    t.map_or("never".to_string(), |t| format!("{t:.2} s"))
}

/// How many of replica `m`'s events contain `what`.
fn events(sim: &Sim, m: MemberId, what: &str) -> usize {
    sim.replica(m, 1).sim.events.iter().filter(|e| e.contains(what)).count()
}

pub fn strikeframe() {
    // Per timing: how long 9 is silent in each of four windows, and how late its entries of the
    // window's last stretch reach everyone but 1 (the reviewer's five settings).
    let timings: [(u64, u64); 5] = [(3_050_000, 300_000), (3_050_000, 1_000_000), (3_020_000, 600_000), (3_080_000, 1_000_000), (2_980_000, 1_000_000)];
    for fixed in RUNS {
        let mut kept = 0;
        for (resume, delay) in timings {
            let ids = [1, 2, 3, 9];
            let mut ps = peers_of(&ids, |m| if m == 2 { plan(&[(60, touch(10, 6.0)), (1300, touch(33, 3.0))]) } else { BTreeMap::new() });
            if !fixed {
                each_sim(&mut ps, |s| s.fixes_off = fix::VC3R2);
            }
            let mut net = Net::new(link(30, 10, 0), 7);
            net.filter = Some(Box::new(move |t, _w, from, to, _m| {
                if from != 9 {
                    return Fate::Deliver;
                }
                for k in 0..4u64 {
                    let s = 2_000_000 + 5_000_000 * k;
                    if (s..s + resume).contains(&t) {
                        return Fate::Drop;
                    }
                    if (s - delay..s).contains(&t) && to != 1 {
                        return Fate::Delay(delay);
                    }
                }
                Fate::Deliver
            }));
            let mut sim = Sim::new(net, ps, 26_000_000);
            let stopped = run_quiet(&mut sim, |_| {});
            let r2 = &sim.replica(2, 1).sim;
            let (members, resets) = (r2.members.clone(), r2.stats.resets);
            let label = format!("strikeframe (9 silent {:.2} s in each of 4 windows; its last entries before each reach 2 and 3 {} ms late)", resume as f64 / 1e6, delay / 1000);
            if !fixed {
                println!("  {label}: at m2: members {members:?}, resets {resets}, strikes {:?}", r2.strikes);
                let honest: Vec<MemberId> = [1, 2, 3].into_iter().filter(|m| members.contains(m)).collect();
                before_fix(&label, "VC3R-2's fix", &sim, &honest, stopped);
                continue;
            }
            let ok = members.contains(&1);
            kept += ok as usize;
            println!(
                "RESULT strikeframe silent {:.2} s, {} ms late: at m2 members {members:?}, resets {resets}, 9 removed at {} -> {}",
                resume as f64 / 1e6,
                delay / 1000,
                secs(removed_at(&sim, 2, 9)),
                if ok { "1 KEPT" } else { "FAIL" }
            );
            verdict(&sim, &[1, 2, 3], &label, stopped);
        }
        if fixed {
            println!("RESULT strikeframe: 1 kept in {kept} of {} timings -> {}", timings.len(), if kept == timings.len() { "OK" } else { "FAIL" });
        }
    }
}

pub fn aheadshield() {
    for fixed in RUNS {
        let variants: &[(bool, bool)] = if fixed { &[(true, false), (false, true), (true, true)][..] } else { &[(true, true)][..] };
        for &(climb, rotate) in variants {
            let ids = [1, 2, 3, 9];
            let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (600, touch(33, 3.0))]) } else { BTreeMap::new() });
            if !fixed {
                each_sim(&mut ps, |s| s.fixes_off = fix::VC3R1);
            }
            let mut net = Net::new(link(30, 10, 0), 7);
            if rotate {
                net.filter = Some(Box::new(|t, _w, from, to, m| match m {
                    Msg::Entry(e) if t >= 1_900_000 && from == 9 && e.member == 9 => {
                        if [1, 2, 3][(e.seq % 3) as usize] == to {
                            Fate::Deliver
                        } else {
                            Fate::Drop
                        }
                    }
                    _ => Fate::Deliver,
                }));
            }
            let mut sim = Sim::new(net, ps, 12_000_000);
            let mut set = false;
            let stopped = run_quiet(&mut sim, |sim| {
                if climb && !set && sim.net.now_us >= 2_000_000 {
                    sim_mut(sim, 9).fake_ack_epoch = Some(1);
                    set = true;
                }
            });
            let honest = [1, 2, 3];
            let removed = removed_at(&sim, 1, 9);
            let label = format!("aheadshield (from 2 s, 9's acks claim epoch 1: {climb}; 9 sends each of its entries to one honest member only, in turn: {rotate})");
            if !fixed {
                println!("  9 removed: {}", secs(removed));
                before_fix(&label, "VC3R-1's fix", &sim, &honest, stopped);
                continue;
            }
            // Rotating alone is harmless (fetches fill every gap): 9 stays and confirmation goes on.
            let ok = if climb { removed.is_some() } else { removed.is_none() && sim.replica(1, 1).sim.conf > 600 };
            println!(
                "RESULT aheadshield climb={climb} rotate={rotate}: 9 removed at {}; {} -> {}",
                secs(removed),
                confs(&sim, &honest),
                match (ok, climb) {
                    (false, _) => "FAIL",
                    (true, true) => "VOTED OUT",
                    (true, false) => "HARMLESS",
                }
            );
            verdict(&sim, &honest, &label, stopped);
        }
    }
}

/// Not a repro: a check that the class VC3R-1 belongs to does not reach silence. Another member's
/// acks claiming more of a silent member's chain spare it from the silence vote while this replica
/// fetches, but the withholding rule removes it all the same: its own references stop.
pub fn fakeref() {
    let ids = [1, 2, 3, 4, 5, 8, 9];
    let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (900, touch(33, 3.0))]) } else { BTreeMap::new() });
    // 8 takes no part in view changes; it only lies in its acks.
    for p in ps.iter_mut().filter(|p| p.id == 8) {
        p.worlds.get_mut(&1).unwrap().sim.withhold_vc = true;
    }
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(Box::new(|t, _w, from, _to, _m| if from == 9 && t >= 2_000_000 { Fate::Drop } else { Fate::Deliver }));
    let mut sim = Sim::new(net, ps, 14_000_000);
    let mut set = false;
    let stopped = run_quiet(&mut sim, |sim| {
        if !set && sim.net.now_us >= 2_000_000 {
            sim_mut(sim, 8).fake_refs = Some((9, 5));
            set = true;
        }
    });
    let honest = [1, 2, 3, 4, 5];
    let removed = removed_at(&sim, 1, 9);
    let label = "fakeref (9 falls silent at 2 s; from then on 8's acks claim 5 more of 9's entries than anyone holds)";
    vc_report(&sim, &honest, "fakeref");
    let withheld = events(&sim, 1, "member 9 has referenced no more");
    println!(
        "RESULT fakeref: 9 removed at {} ({withheld} withholding vote at m1) -> {}; {}",
        secs(removed),
        if removed.is_some() { "VOTED OUT" } else { "FAIL" },
        confs(&sim, &honest)
    );
    verdict(&sim, &honest, label, stopped);
}

pub fn capstale() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 9];
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (700, touch(33, 3.0))]) } else { BTreeMap::new() });
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VC3R2);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 14_000_000);
        let mut capped: BTreeSet<u32> = BTreeSet::new();
        let stopped = run_quiet(&mut sim, |sim| {
            if sim.net.now_us < 2_000_000 || !sim.replica(1, 1).sim.members.contains(&9) {
                return;
            }
            let r9 = sim_mut(sim, 9);
            let epoch = r9.epoch;
            if capped.insert(epoch) {
                let boundary = r9.now + 30;
                let e = r9.author(Payload::Precommit { epoch, round: 0, value: 0xBAD0 ^ epoch as u64, boundary }, None);
                send_all(sim, 9, &ids, &e);
            }
        });
        let honest = [1, 2, 3];
        let removed = removed_at(&sim, 1, 9);
        let resets = sim.replica(1, 1).sim.stats.resets;
        let label = "capstale (from 2 s, 9 signs a capped precommit for no proposal in every epoch)";
        if !fixed {
            println!("  with round 3's stale-cap resets and strikes: 9 removed at {}, after {resets} resets", secs(removed));
            before_fix(label, "VC3R-2's fix", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "capstale");
        let votes = events(&sim, 1, "for a value no quorum here prevoted");
        println!(
            "RESULT capstale: {resets} resets, 9 removed at {} ({votes} cap vote at m1) -> {}; {}",
            secs(removed),
            if removed.is_some() && resets == 0 { "VOTED OUT, NO RESET" } else { "FAIL" },
            confs(&sim, &honest)
        );
        verdict(&sim, &honest, label, stopped);
    }
}

/// 9's churn: from `from_us` until `until_us`, in every epoch it is in, a suspicion vote against 2
/// (whom everyone hears), so an honest proposer answers it with a reset.
fn churn(sim: &mut Sim, voted: &mut BTreeSet<u32>, ids: &[MemberId], from_us: u64, until_us: u64) {
    if sim.net.now_us < from_us || sim.net.now_us >= until_us || !sim.replica(1, 1).sim.members.contains(&9) {
        return;
    }
    let frozen = head_of(sim, 9, 2);
    let r9 = sim_mut(sim, 9);
    let epoch = r9.epoch;
    if voted.insert(epoch) {
        let e = r9.author(Payload::Vote { epoch, target: 2, seq: 0, frozen, kick: 0 }, None);
        send_all(sim, 9, ids, &e);
    }
}

pub fn lagchurn() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 9];
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (700, touch(33, 3.0))]) } else { BTreeMap::new() });
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::LAGKEEP);
        }
        // From 2 s, everything 9 sends arrives 1.3 s late: its horizon trails every reader's present by
        // about 80 frames, past half the window and short of all of it.
        let mut net = Net::new(link(30, 10, 0), 7);
        net.filter = Some(Box::new(|t, _w, from, _to, _m| if from == 9 && t >= 2_000_000 { Fate::Delay(1_300_000) } else { Fate::Deliver }));
        let mut sim = Sim::new(net, ps, 16_000_000);
        let mut voted = BTreeSet::new();
        let stopped = run_quiet(&mut sim, |sim| churn(sim, &mut voted, &ids, 2_000_000, u64::MAX));
        let honest = [1, 2, 3];
        let removed = removed_at(&sim, 1, 9);
        let resets = sim.replica(1, 1).sim.stats.resets;
        let label = "lagchurn (from 2 s, 9's messages arrive 1.3 s late and 9 votes against 2 in every epoch)";
        if !fixed {
            println!("  9 removed: {}, after {resets} resets", secs(removed));
            before_fix(label, "round 4's surviving slowness timers", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "lagchurn");
        let demotions = events(&sim, 1, "vote to demote it");
        println!(
            "RESULT lagchurn: {resets} resets, 9 removed at {} ({demotions} demotion vote at m1) -> {}; {}",
            secs(removed),
            if removed.is_some() && demotions > 0 { "DEMOTED" } else { "FAIL" },
            confs(&sim, &honest)
        );
        verdict(&sim, &honest, label, stopped);
    }
}

/// Median and maximum of `v`.
fn median_max(v: &mut [u64]) -> (u64, u64) {
    v.sort_unstable();
    (v.get(v.len() / 2).copied().unwrap_or(0), v.last().copied().unwrap_or(0))
}

pub fn votechurn() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 9];
        let mut ps = peers_of(&ids, |m| match m {
            1 => plan(&[(60, touch(10, 6.0)), (600, Action::Kick { target: 9 }), (800, touch(33, 3.0))]),
            2 => plan(&[(300, touch(20, 4.0)), (500, touch(21, 4.0))]),
            _ => BTreeMap::new(),
        });
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VC3R2);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 16_000_000);
        let mut voted = BTreeSet::new();
        // m1's confirmation lag (its present minus its confirmed frame), every 100 ms: before the
        // churn, during it (2-10 s), and from 12 s.
        let mut lag: [Vec<u64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let stopped = run_quiet(&mut sim, |sim| {
            churn(sim, &mut voted, &ids, 2_000_000, 10_000_000);
            let t = sim.net.now_us;
            if t % 100_000 == 0 && t >= 500_000 {
                let r1 = &sim.replica(1, 1).sim;
                let k = if t < 2_000_000 { 0 } else if t < 10_000_000 { 1 } else if t >= 12_000_000 { 2 } else { return };
                lag[k].push(r1.now.saturating_sub(r1.conf));
            }
        });
        let honest = [1, 2, 3];
        let r1 = sim.replica(1, 1);
        let removed = removed_at(&sim, 1, 9);
        let during = r1.sim.vc_log.iter().filter(|d| (2.0..10.0).contains(&sim_secs(r1, d.1)) && d.3.is_empty()).count();
        let after = r1.sim.vc_log.iter().filter(|d| sim_secs(r1, d.1) > 10.5 && d.3.is_empty()).count();
        let label = "votechurn (2-10 s: 9 votes against 2, whom everyone hears, in every epoch; admin 1 kicks 9 at 10 s)";
        if !fixed {
            println!("  with round 3's strikes: 9 removed at {}; {during} resets during the churn", secs(removed));
            before_fix(label, "VC3R-2's fix", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "votechurn");
        let [b, d, a] = lag.map(|mut v| median_max(&mut v));
        println!(
            "RESULT votechurn: {during} resets in the 8 s of churn ({:.1} per s); m1's confirmation lag in frames, median/max: {}/{} before, {}/{} during, {}/{} after; 9 kicked at {}, {after} resets after -> {}",
            during as f64 / 8.0,
            b.0,
            b.1,
            d.0,
            d.1,
            a.0,
            a.1,
            secs(removed),
            if removed.is_some() && after == 0 { "THE KICK ENDS IT" } else { "FAIL" }
        );
        println!("RESULT votechurn: {}", confs(&sim, &honest));
        verdict(&sim, &honest, label, stopped);
    }
}

pub fn adminvoid() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 9];
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (600, touch(33, 3.0))]) } else { BTreeMap::new() });
        guests(&mut ps, &[1, 2, 3, 9]);
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VC3R3);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
        let mut sent = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if sent || sim.net.now_us < 2_000_000 {
                return;
            }
            // 9 signs a SetRole backdated 30 frames, past its own declared horizon, so every replica
            // voids it; then lists it in the change it proposes, and starts a view change with a nil
            // prevote so that its round comes.
            let r9 = sim_mut(sim, 9);
            let epoch = r9.epoch;
            let e = r9.author_backdated(Payload::SetRole { epoch, member: 3, role: GUEST }, 30);
            r9.byz_change = Some(Change { epoch, admin: vec![(e.id(), true)], ..Default::default() });
            let p = r9.author(Payload::Prevote { epoch, round: 0, value: 0 }, None);
            send_all(sim, 9, &ids, &e);
            send_all(sim, 9, &ids, &p);
            sent = true;
        });
        let honest = [1, 2, 3];
        let role = sim.replica(1, 1).sim.roles.get(&3).copied();
        let label = "adminvoid (at 2 s, 9 signs a backdated, horizon-void SetRole demoting 3, and proposes a change that lists it)";
        if !fixed {
            println!("  3's role at m1: {role:?}");
            before_fix(label, "VC3R-3's fix", &sim, &honest, stopped);
            continue;
        }
        for &m in &honest {
            println!("  {}", roles_line(&sim, m));
        }
        println!("RESULT adminvoid: 3's role at m1 {role:?} -> {}; {}", if role == Some(ADMIN) { "THE VOID ENTRY IS NOT APPLIED" } else { "FAIL" }, confs(&sim, &honest));
        verdict(&sim, &honest, label, stopped);
    }
}

pub fn joinsquat() {
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
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VC3R4);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
        let mut step = 0;
        let stopped = run_quiet(&mut sim, |sim| {
            let t = sim.net.now_us;
            // 1.9 s: 9 asks in 5's name with no attestation and no valid signature. 2.0 s: 5 asks with
            // none yet; 2.5 s: 5 asks again with admin 1's friendship.
            if step == 0 && t >= 1_900_000 {
                for m in [1, 2, 3] {
                    sim.net.send(1, 9, m, &Msg::Join { member: joiner, attests: vec![], sig: 0 });
                }
                step = 1;
            } else if step == 1 && t >= 2_000_000 {
                for m in ids {
                    sim.net.send(1, joiner, m, &Msg::Join { member: joiner, attests: vec![], sig: join_sig(1, joiner) });
                }
                step = 2;
            } else if step == 2 && t >= 2_500_000 {
                for m in ids {
                    sim.net.send(1, joiner, m, &Msg::Join { member: joiner, attests: vec![attest(Rel::Friend, joiner, 1, 1)], sig: join_sig(1, joiner) });
                }
                step = 3;
            }
        });
        let all = [1, 2, 3, 5];
        let role = sim.replica(3, 1).sim.roles.get(&joiner).copied();
        let label = "joinsquat (9 asks to join in 5's name at 1.9 s; 5 asks at 2.0 s with no attestation, and at 2.5 s as admin 1's friend)";
        if !fixed {
            println!("  5's role at m3: {role:?}");
            before_fix(label, "VC3R-4's fix", &sim, &[1, 2, 3], stopped);
            continue;
        }
        vc_report(&sim, &[1, 2, 3], "joinsquat");
        println!("RESULT joinsquat: 5's role at m3 {role:?} -> {}; {}", if role == Some(GUEST) { "ADMITTED AS GUEST" } else { "FAIL" }, confs(&sim, &all));
        verdict(&sim, &all, label, stopped);
    }
}

pub fn joinskip() {
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
        p.worlds.get_mut(&1).unwrap().sim.quiet_until_member = true;
        ps.push(p);
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VC3R6);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
        let mut asked = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if !asked && sim.net.now_us >= 3_000_000 {
                for m in ids {
                    sim.net.send(1, joiner, m, &Msg::Join { member: joiner, attests: vec![], sig: join_sig(1, joiner) });
                }
                for e in sim.replica(1, 1).sim.all_entries() {
                    sim.net.send(1, 1, joiner, &Msg::Entry(e));
                }
                asked = true;
            }
        });
        let all = [1, 2, 3, 5];
        let members = sim.replica(1, 1).sim.members.clone();
        let label = "joinskip (two decisions, then 5 joins at epoch 2 and authors no ack before it is admitted)";
        if !fixed {
            println!("  members at m1: {members:?}");
            before_fix(label, "VC3R-6's fix", &sim, &ids, stopped);
            continue;
        }
        vc_report(&sim, &all, "joinskip");
        println!("RESULT joinskip: members at m1 {members:?} -> {}; {}", if members.contains(&joiner) { "5 STAYS" } else { "FAIL" }, confs(&sim, &all));
        verdict(&sim, &all, label, stopped);
    }
}
