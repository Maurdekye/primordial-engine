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
use pe_session::session::*;

use crate::p24r::{before_fix, AdminKick, confs, each_sim, guests, peers_of, roles_line, run_quiet, sim_mut, GUEST, SCENE};
use crate::review2::{link, plan, touch, verdict};
use crate::viewchange::{sim_secs, vc_report};

/// Each repro: the run with its fix switched off, then the run as shipped.
const RUNS: [bool; 2] = [false, true];

fn send_all(sim: &mut Sim, from: MemberId, to: &[MemberId], e: &Entry) {
    for &m in to {
        if m != from {
            sim.net.send(1, from, m, &Msg::Entry(test_sign(1, e.clone())));
        }
    }
}

/// The newest entry of `author`'s chain that replica `m` holds: (seq, id).
fn head_of(sim: &Sim, m: MemberId, author: MemberId) -> (u32, EntryId) {
    sim.replica(m, 1).sim.all_entries().into_iter().filter(|e| e.member == author).map(|e| (e.seq, e.id())).max().unwrap_or((0, EntryId::NONE))
}

/// When replica `m` decided the removal of `x`, if it did (scenario seconds).
fn removed_at(sim: &Sim, m: MemberId, x: MemberId) -> Option<f64> {
    let rep = sim.replica(m, 1);
    rep.sim.vcm.vc_log.iter().find(|d| d.3.contains(&x)).map(|d| sim_secs(rep, d.1))
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
            let (members, resets) = (r2.vcm.members.clone(), r2.stats.resets);
            let label = format!("strikeframe (9 silent {:.2} s in each of 4 windows; its last entries before each reach 2 and 3 {} ms late)", resume as f64 / 1e6, delay / 1000);
            if !fixed {
                println!("  {label}: at m2: members {members:?}, resets {resets}, strikes {:?}", r2.vcm.strikes);
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

/// Review VC3R-1 (round 4): a climbing member that keeps its chain a step ahead somewhere. Trust survey
/// #4 retired VC-5's vote with the newer-epoch timer VC3R-1 fixed: a climbing 9 holds confirmation
/// (no reader counts its acks from the claimed epoch on) until admin 1 kicks it at 6 s. Rotating alone
/// stays harmless.
pub fn aheadshield() {
    {
        let variants: &[(bool, bool)] = &[(true, false), (false, true), (true, true)];
        for &(climb, rotate) in variants {
            let ids = [1, 2, 3, 9];
            let ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (600, touch(33, 3.0))]) } else { BTreeMap::new() });
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
            let mut kick = climb.then(|| AdminKick::new(1, 9, 6_000_000));
            let stopped = run_quiet(&mut sim, |sim| {
                if let Some(k) = kick.as_mut() {
                    k.step(sim, &[1, 2, 3], &ids);
                }
                if climb && !set && sim.net.now_us >= 2_000_000 {
                    sim_mut(sim, 9).vcm.fake_ack_epoch = Some(1);
                    set = true;
                }
            });
            let honest = [1, 2, 3];
            let removed = removed_at(&sim, 1, 9);
            let label = format!("aheadshield (from 2 s, 9's acks claim epoch 1: {climb}; 9 sends each of its entries to one honest member only, in turn: {rotate})");
            // Rotating alone is harmless (fetches fill every gap): 9 stays and confirmation goes on.
            if let Some(k) = &kick {
                println!("RESULT aheadshield climb={climb} rotate={rotate}: {}", confs(&sim, &honest));
                k.report(&sim, &honest, &format!("aheadshield climb={climb} rotate={rotate}"));
            } else {
                let ok = removed.is_none() && sim.replica(1, 1).sim.conf > 600;
                println!(
                    "RESULT aheadshield climb={climb} rotate={rotate}: 9 removed at {}; {} -> {}",
                    secs(removed),
                    confs(&sim, &honest),
                    if ok { "HARMLESS" } else { "FAIL" }
                );
            }
            verdict(&sim, &honest, &label, stopped);
        }
    }
}

/// Not a repro: a check that the class VC3R-1 belongs to does not reach silence. Another member's
/// acks claiming more of a silent member's chain spare it from the silence vote while this replica
/// fetches, but only for a silence limit from when it fell behind (VC4R-3): then the silence vote
/// removes it all the same. (Until trust survey #4 the withholding rule removed it first.)
pub fn fakeref() {
    let ids = [1, 2, 3, 4, 5, 8, 9];
    let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (900, touch(33, 3.0))]) } else { BTreeMap::new() });
    // 8 takes no part in view changes; it only lies in its acks.
    for p in ps.iter_mut().filter(|p| p.id == 8) {
        p.worlds.get_mut(&1).unwrap().sim.vcm.withhold_vc = true;
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
        // Trust survey #4: the cap holds confirmation until an admin kicks 9.
        let mut kick = fixed.then(|| AdminKick::new(1, 9, 6_000_000));
        let stopped = run_quiet(&mut sim, |sim| {
            if let Some(k) = kick.as_mut() {
                k.step(sim, &[1, 2, 3], &ids);
            }
            if sim.net.now_us < 2_000_000 || !sim.replica(1, 1).sim.vcm.members.contains(&9) {
                return;
            }
            let r9 = sim_mut(sim, 9);
            let epoch = r9.vcm.epoch;
            if capped.insert(epoch) {
                let boundary = r9.now + 30;
                let sig = test_fact_sig(9, 1, vc_fact(epoch, 0, 0xBAD0 ^ epoch as u64));
                let e = r9.author(Payload::Precommit { epoch, round: 0, value: 0xBAD0 ^ epoch as u64, boundary, polka: vec![], sig }, None);
                send_all(sim, 9, &ids, &e);
            }
        });
        let honest = [1, 2, 3];
        let removed = removed_at(&sim, 1, 9);
        let resets = sim.replica(1, 1).sim.stats.resets;
        let label = "capstale (from 2 s, 9 signs a capped precommit for no proposal in every epoch; admin 1 kicks 9 at 6 s)";
        if !fixed {
            println!("  with round 3's stale-cap resets and strikes: 9 removed at {}, after {resets} resets", secs(removed));
            before_fix(label, "VC3R-2's fix", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "capstale");
        println!("RESULT capstale: {resets} resets -> {}; {}", if resets == 0 { "NO RESET" } else { "FAIL" }, confs(&sim, &honest));
        if let Some(k) = &kick {
            k.report(&sim, &honest, "capstale");
        }
        verdict(&sim, &honest, label, stopped);
    }
}

/// 9's churn: from `from_us` until `until_us`, in every epoch it is in, a suspicion vote against 2
/// (whom everyone hears), so an honest proposer answers it with a reset.
fn churn(sim: &mut Sim, voted: &mut BTreeSet<u32>, ids: &[MemberId], from_us: u64, until_us: u64) {
    if sim.net.now_us < from_us || sim.net.now_us >= until_us || !sim.replica(1, 1).sim.vcm.members.contains(&9) {
        return;
    }
    let frozen = head_of(sim, 9, 2);
    let r9 = sim_mut(sim, 9);
    let epoch = r9.vcm.epoch;
    if voted.insert(epoch) {
        let e = r9.author(Payload::Vote { epoch, target: 2, seq: 0, frozen, kick: EntryId::NONE }, None);
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
        let during = r1.sim.vcm.vc_log.iter().filter(|d| (2.0..10.0).contains(&sim_secs(r1, d.1)) && d.3.is_empty()).count();
        let after = r1.sim.vcm.vc_log.iter().filter(|d| sim_secs(r1, d.1) > 10.5 && d.3.is_empty()).count();
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
            let epoch = r9.vcm.epoch;
            let e = r9.author_backdated(Payload::SetRole { epoch, member: 3, role: GUEST }, 30);
            r9.byz_change = Some(Change { epoch, admin: vec![(e.id(), true)], ..Default::default() });
            let p = r9.author(Payload::Prevote { epoch, round: 0, value: 0 }, None);
            send_all(sim, 9, &ids, &e);
            send_all(sim, 9, &ids, &p);
            sent = true;
        });
        let honest = [1, 2, 3];
        let role = sim.replica(1, 1).sim.vcm.roles.get(&3).copied();
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
        each_sim(&mut ps, |s| s.vcm.join_rules = JoinRules::friends_only(GUEST));
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
                    sim.net.send(1, 9, m, &Msg::Join { member: joiner, attests: vec![], sig: [0; 64], expires: u64::MAX });
                }
                step = 1;
            } else if step == 1 && t >= 2_000_000 {
                for m in ids {
                    sim.net.send(1, joiner, m, &join_msg(&TEST_SESSION, joiner, vec![], u64::MAX));
                }
                step = 2;
            } else if step == 2 && t >= 2_500_000 {
                for m in ids {
                    sim.net.send(1, joiner, m, &join_msg(&TEST_SESSION, joiner, vec![attest(Rel::Friend, joiner, 1, 1)], u64::MAX));
                }
                step = 3;
            }
        });
        let all = [1, 2, 3, 5];
        let role = sim.replica(3, 1).sim.vcm.roles.get(&joiner).copied();
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
                    sim.net.send(1, joiner, m, &join_msg(&TEST_SESSION, joiner, vec![], u64::MAX));
                }
                let mut log = sim.replica(1, 1).sim.all_entries();
                log.sort_by_key(|e| (e.member, e.seq));
                for e in log {
                    sim.net.send(1, 1, joiner, &Msg::Entry(test_sign(1, e)));
                }
                asked = true;
            }
        });
        let all = [1, 2, 3, 5];
        let members = sim.replica(1, 1).sim.vcm.members.clone();
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

/// VC4R-2 (design-reviewer's round-4 repro): 5's signed join request reaches 1, 2 and 9 only, and 9
/// crashes after its first prevote, so the join needs 3, which never received the request. With the fix
/// off the change waits on a local copy and the session wedges; with it the request travels in the
/// change and 3 validates it. The `reach3` control (the request also reaches 3) always succeeds.
pub fn joinwedge() {
    for fixed in RUNS {
        for reach3 in [false, true] {
            let ids = [1, 2, 3, 9];
            let joiner: MemberId = 5;
            let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (600, touch(33, 3.0))]) } else { BTreeMap::new() });
            let mut p = Peer::new(joiner, 0);
            p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
            ps.push(p);
            for p in ps.iter_mut().filter(|p| p.id != joiner) {
                p.worlds.get_mut(&1).unwrap().sim.follow(joiner);
            }
            each_sim(&mut ps, |s| s.vcm.join_rules = JoinRules::friends_only(GUEST));
            guests(&mut ps, &[1, 2, 3, 9]);
            if !fixed {
                each_sim(&mut ps, |s| s.fixes_off = fix::VC4R2);
            }
            let crashed = std::rc::Rc::new(std::cell::Cell::new(false));
            let c2 = crashed.clone();
            let mut net = Net::new(link(30, 10, 0), 7);
            net.filter = Some(Box::new(move |_t, _w, from, to, m| {
                if from == 9 && (c2.get() || matches!(m, Msg::Entry(e) if matches!(e.payload, Payload::Precommit { .. }))) {
                    return Fate::Drop;
                }
                if !reach3 && to == 3 && matches!(m, Msg::Join { .. }) {
                    return Fate::Drop;
                }
                Fate::Deliver
            }));
            let mut sim = Sim::new(net, ps, 14_000_000);
            let mut sent = false;
            let stopped = run_quiet(&mut sim, |sim| {
                let t = sim.net.now_us;
                if !sent && t >= 2_000_000 {
                    for m in ids {
                        sim.net.send(1, joiner, m, &join_msg(&TEST_SESSION, joiner, vec![attest(Rel::Friend, joiner, 1, 1)], u64::MAX));
                    }
                    sent = true;
                }
                if sent && !crashed.get() && sim.replica(9, 1).sim.all_entries().iter().any(|e| e.member == 9 && matches!(e.payload, Payload::Prevote { .. })) {
                    crashed.set(true);
                }
            });
            let honest = [1, 2, 3];
            let admitted = sim.replica(1, 1).sim.vcm.members.contains(&joiner);
            let label = format!("joinwedge (request reaches 3: {reach3}; 9 crashes after its first prevote)");
            if !fixed {
                if reach3 {
                    continue;
                }
                println!("  5 admitted at m1: {admitted}");
                before_fix(&label, "VC4R-2's fix", &sim, &honest, stopped);
                continue;
            }
            vc_report(&sim, &honest, "joinwedge");
            println!(
                "RESULT joinwedge reach3={reach3}: 5 admitted {admitted}, 9 removed at {} -> {}; {}",
                secs(removed_at(&sim, 1, 9)),
                if admitted { "ADMITTED" } else { "FAIL" },
                confs(&sim, &honest)
            );
            verdict(&sim, &[1, 2, 3, 5], &label, stopped);
        }
    }
}

/// VC4R-3 (design-reviewer's round-4 repro): fakeref, but 9 stays connected and grows slower (a message
/// sent at t arrives at 2t - 2 s), while 8's acks claim 5 more of 9's entries than anyone holds. With
/// the fix off that claim shields 9 for good and the session halts; with it the claim spares 9 only a
/// silence limit, and 9 is demoted.
pub fn slowref() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 4, 5, 8, 9];
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (900, touch(33, 3.0))]) } else { BTreeMap::new() });
        for p in ps.iter_mut().filter(|p| p.id == 8) {
            p.worlds.get_mut(&1).unwrap().sim.vcm.withhold_vc = true;
        }
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VC4R3);
        }
        let mut net = Net::new(link(30, 10, 0), 7);
        net.filter = Some(Box::new(|t, _w, from, _to, _m| if from == 9 && t >= 2_000_000 { Fate::Delay(t - 2_000_000 + 30_000) } else { Fate::Deliver }));
        let mut sim = Sim::new(net, ps, 16_000_000);
        let mut set = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if !set && sim.net.now_us >= 2_000_000 {
                sim_mut(sim, 8).fake_refs = Some((9, 5));
                set = true;
            }
        });
        let honest = [1, 2, 3, 4, 5];
        let removed = removed_at(&sim, 1, 9);
        let label = "slowref (9 stays connected but grows slower while 8's acks claim 5 more of 9's entries than anyone holds)";
        if !fixed {
            println!("  9 removed: {}", secs(removed));
            before_fix(label, "VC4R-3's fix", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "slowref");
        println!(
            "RESULT slowref: 9 removed at {} -> {}; {}",
            secs(removed),
            if removed.is_some() { "DEMOTED" } else { "FAIL" },
            confs(&sim, &honest)
        );
        verdict(&sim, &honest, label, stopped);
    }
}


// ---- design-reviewer round-5 repros (reviewer's worktree only) ------------------------------------

/// VC5R-2: 8 alternates every 50 ms between acks naming a fake id at its current seq of chain 1
/// and acks naming a stale real head (`freeze_others`), so its verified reference keeps rising.
/// Control: the fake for good.
pub fn fakemix() {
    for (mix, refresh) in [(false, 0u64), (true, 0), (true, 1_000_000), (true, 2_000_000)] {
        let ids = [1, 2, 3, 4, 8];
        let mut snap = 0u64;
        let ps = peers_of(&ids, |m| if m == 1 { plan(&(0..40).map(|k| (60 + 20 * k, touch(10 + (k % 3) as u32, 1.0))).collect::<Vec<_>>()) } else { BTreeMap::new() });
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 14_000_000);
        let mut last = 0u64;
        let mut fake = false;
        // Trust survey #4: the pure fake holds confirmation until an admin kicks 8.
        let mut kick = (!mix).then(|| AdminKick::new(1, 8, 6_000_000));
        let stopped = run_quiet(&mut sim, |sim| {
            if let Some(k) = kick.as_mut() {
                k.step(sim, &[1, 2, 3, 4], &ids);
            }
            let t = sim.net.now_us;
            if t < 2_000_000 {
                return;
            }
            if !mix {
                sim_mut(sim, 8).fake_refs = Some((1, 0));
                return;
            }
            if t >= last + 50_000 {
                last = t;
                fake = !fake;
                let r8 = sim_mut(sim, 8);
                if fake {
                    if t >= snap + refresh {
                        snap = t;
                        r8.freeze_others();
                    }
                    r8.fake_refs = Some((1, 0));
                } else {
                    r8.fake_refs = None;
                }
            }
        });
        let honest = [1, 2, 3, 4];
        println!("REPRO fakemix mix={mix} stale-refresh={refresh}us: 8 removed at {}; {}", secs(removed_at(&sim, 1, 8)), confs(&sim, &honest));
        if let Some(k) = &kick {
            k.report(&sim, &honest, "fakemix");
        }
        verdict(&sim, &honest, &format!("fakemix mix={mix}"), stopped);
    }
}

/// VC5R-1: faulty admin 9 forks one seq (a SetRole demoting 3 to 1 and 2, an Attest to 3), proposes
/// a change listing the SetRole, drops its precommits, then crashes; the proof reaches 1 and 2 after
/// they lock. The control switches round 5's canonical check back on (`VC3R3S | VC6R3`: since round 7,
/// `VC3R3S` acts only with `VC6R3` off, review VC7R-7).
pub fn adminfork() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 9];
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (600, touch(33, 3.0))]) } else { BTreeMap::new() });
        guests(&mut ps, &[1, 2, 3, 9]);
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VC3R3S | fix::VC6R3);
        }
        let a_id = std::rc::Rc::new(std::cell::Cell::new(EntryId::NONE));
        let crashed = std::rc::Rc::new(std::cell::Cell::new(false));
        let (fa, fc) = (a_id.clone(), crashed.clone());
        let mut net = Net::new(link(30, 10, 0), 7);
        net.filter = Some(Box::new(move |t, _w, from, to, m| {
            if from == 9 {
                if fc.get() {
                    return Fate::Drop;
                }
                if let Msg::Entry(e) = m {
                    match e.payload {
                        Payload::Precommit { .. } => return Fate::Drop,
                        Payload::Prevote { value, .. } if to == 3 && value != 0 => return Fate::Drop,
                        _ if to == 3 && fa.get() != EntryId::NONE && e.id() == fa.get() => return Fate::Drop,
                        _ => {}
                    }
                }
            }
            if from == 3 && (to == 1 || to == 2) && (2_000_000..4_000_000).contains(&t) {
                return Fate::Delay(2_000_000);
            }
            Fate::Deliver
        }));
        let mut sim = Sim::new(net, ps, 12_000_000);
        let mut sent = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if !sent && sim.net.now_us >= 2_000_000 {
                let r9 = sim_mut(sim, 9);
                let epoch = r9.vcm.epoch;
                let a = r9.author(Payload::SetRole { epoch, member: 3, role: GUEST }, None);
                let b = Entry { payload: Payload::Attest { fact: Fact([1; 32]), sig: [0; 64] }, ..a.entry.clone() };
                a_id.set(a.id());
                r9.byz_change = Some(Change { epoch, admin: vec![(a.id(), true)], ..Default::default() });
                let p = r9.author(Payload::Prevote { epoch, round: 0, value: 0 }, None);
                send_all(sim, 9, &[1, 2], &a);
                sim.net.send(1, 9, 3, &Msg::Entry(test_sign(1, b)));
                send_all(sim, 9, &ids, &p);
                sent = true;
            }
            if sent && !crashed.get() {
                let mine = sim.replica(9, 1).sim.all_entries().into_iter().any(|e| e.member == 9 && matches!(e.payload, Payload::Prevote { value, .. } if value != 0));
                if mine {
                    crashed.set(true);
                }
            }
        });
        let honest = [1, 2, 3];
        println!(
            "REPRO adminfork fixed={fixed}: 9 crashed {}; 9 removed at {}; 3's role at m1 {:?}; {}",
            crashed.get(),
            secs(removed_at(&sim, 1, 9)),
            sim.replica(1, 1).sim.vcm.roles.get(&3).copied(),
            confs(&sim, &honest)
        );
        vc_report(&sim, &honest, "adminfork");
        verdict(&sim, &honest, &format!("adminfork fixed={fixed}"), stopped);
    }
}

/// prevotefork's members: the equivocating proposer, the capper, the observer, the joiner.
const PREVOTEFORK: (MemberId, MemberId, MemberId, MemberId) = (1, 2, 3, 5);

/// prevotefork's run (members 1, 2, 3, 9 and joiner 5) with `off` switched off; 9's round-0 prevote is
/// lost until `nine_until_us`, and everything 9 sends from `nine_silent_us` on. `slow_us` > 0 delays
/// every delivery of 1's forked value-prevote to the observer by that much (`slowcapper`, VC7R-2): the
/// capper alone holds it, so its replies are what the observer's fetch waits for.
fn prevotefork_sim(off: u64, slow_us: u64, nine_until_us: u64, nine_silent_us: u64) -> (Sim, Option<u64>) {
    let ids = [1, 2, 3, 9];
    let proposer = peers_of(&ids, |_| BTreeMap::new())[0].worlds[&1].sim.proposer_of(0, 0).unwrap();
    assert_eq!(proposer, 1, "prevotefork assumes member 1 is the round-0 proposer");
    let (equiv, capper, obs, joiner) = PREVOTEFORK;
    let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (600, touch(33, 3.0))]) } else { BTreeMap::new() });
    let mut p = Peer::new(joiner, 0);
    p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
    ps.push(p);
    for p in ps.iter_mut().filter(|p| p.id != joiner) {
        p.worlds.get_mut(&1).unwrap().sim.follow(joiner);
    }
    each_sim(&mut ps, |s| s.vcm.join_rules = JoinRules::public(GUEST));
    if off != 0 {
        each_sim(&mut ps, move |s| s.fixes_off = off);
    }
    let forged = std::rc::Rc::new(std::cell::RefCell::new(BTreeSet::<EntryId>::new()));
    let vv_id = std::rc::Rc::new(std::cell::Cell::new(EntryId::NONE));
    let (fg, fv) = (forged.clone(), vv_id.clone());
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(Box::new(move |t, _w, from, to, m| match m {
        _ if from == 9 && t >= nine_silent_us => Fate::Drop,
        Msg::Entry(e) if slow_us > 0 && to == obs && fv.get() != EntryId::NONE && e.id() == fv.get() => Fate::Delay(slow_us),
        Msg::Entry(e) if from == equiv && matches!(e.payload, Payload::Prevote { round: 0, .. }) && !fg.borrow().contains(&e.id()) => Fate::Drop,
        // 9's round-0 prevote is lost until 2.5 s, past the cap's judgment, then repaired as after GST.
        // Round 6 dropped it for good, repairs included, which made 9 a second faulty member (two of
        // four, beyond t = 1) and halted the session (@ballpit-viz's finding).
        Msg::Entry(e) if from == 9 && t < nine_until_us && matches!(e.payload, Payload::Prevote { round: 0, .. }) => Fate::Drop,
        _ => Fate::Deliver,
    }));
    let mut sim = Sim::new(net, ps, 10_000_000);
    let mut asked = false;
    let mut forked = false;
    let stopped = run_quiet(&mut sim, |sim| {
        let now = sim.net.now_us;
        if !asked && now >= 1_000_000 {
            for m in ids {
                sim.net.send(1, joiner, m, &join_msg(&TEST_SESSION, joiner, vec![], u64::MAX));
            }
            asked = true;
        }
        if asked && !forked {
            let r = &sim.replica(equiv, 1).sim;
            let pv = r.all_entries().into_iter().find(|e| e.member == equiv && matches!(e.payload, Payload::Prevote { round: 0, .. }));
            let prop = r.all_entries().into_iter().find(|e| e.member == equiv && matches!(e.payload, Payload::Propose { round: 0, .. }));
            if let (Some(pv), Some(Payload::Propose { change, .. })) = (pv, prop.map(|e| e.payload)) {
                let v = change.value();
                let Payload::Prevote { epoch, round, .. } = pv.payload else { return };
                let mut vv = pv.clone();
                vv.payload = Payload::Prevote { epoch, round, value: v };
                let mut nil = pv.clone();
                nil.payload = Payload::Prevote { epoch, round, value: 0 };
                forged.borrow_mut().insert(vv.id());
                forged.borrow_mut().insert(nil.id());
                vv_id.set(vv.id());
                sim.net.send(1, equiv, capper, &Msg::Entry(test_sign(1, vv)));
                for m in [obs, 9] {
                    sim.net.send(1, equiv, m, &Msg::Entry(test_sign(1, nil.clone())));
                }
                forked = true;
            }
        }
    });
    (sim, stopped)
}

/// VC4R-1 (design-reviewer's inferred finding, built in round 5): an equivocated prevote got an honest
/// capper voted out. The round-0 proposer (member 1) equivocates its own prevote for the join's value
/// `v`: the real value to capper 2, nil to observer 3. Member 9's prevote is dropped, so without 1's
/// honest prevote the observer sees only 2 and 3 prevote `v` -- short of the quorum of 3 -- while the
/// capper sees {1, 2, 3} and precommits with a cap. Before VC4R-1's fix, observer 3 voted honest capper
/// 2 out for an unjustified cap. Trust survey #4 retired the cap rule, so no member is voted out for a
/// cap at all; the run checks that 3 casts no vote against 2 and the session decides and goes on.
///
/// PF-1 (@ballpit-viz, mirroring this repro): 1's chain continues from its own version of the forked
/// seq, which 3 and 9 never hold; each keeps the first version it received, so neither links 1's later
/// entries. 1 and 2 decide the join on precommits {1, 2, 9}; with PF-1 off, 3, 9 and the joiner cannot
/// count 1's precommit, stay in epoch 0, and confirmation halts for good. Since VC8R-1, 3 and 9 count
/// 1's prevote and decide without it, so the controls switch VC8R-1 off too (`pfsilent` shows PF-1
/// needed with VC8R-1 on).
pub fn prevotefork() {
    let (_, capper, obs, joiner) = PREVOTEFORK;
    // Since VC8R-1, 3 and 9 count 1's prevote toward the round-0 polka, lock with 2 and decide the join
    // before any cap is judged and without 1's precommit; so each control switches VC8R-1 off as well.
    for off in [fix::PF1 | fix::VC8R1, 0] {
        let (sim, stopped) = prevotefork_sim(off, 0, 2_500_000, u64::MAX);
        let honest = [1, 2, 3];
        // 1 is the faulty member (its prevote is forked) and is removed; liveness is the others'.
        let remaining = [capper, obs, 9, joiner];
        let voted = sim.replica(obs, 1).sim.events.iter().any(|e| e.contains(&format!("member {capper}'s precommit caps")));
        let kept = sim.replica(obs, 1).sim.vcm.members.contains(&capper) && sim.replica(capper, 1).sim.vcm.members.contains(&capper);
        let label = "prevotefork (proposer 1 equivocates its prevote: v to capper 2, nil to observer 3; 2 caps on the polka, 3 cannot see it)";
        let epochs = [1, 2, 3, 9, joiner].map(|m| format!("m{m} {}", sim.replica(m, 1).sim.vcm.epoch)).join(", ");
        if off == fix::PF1 | fix::VC8R1 {
            println!("  epochs at the end: {epochs}; confirmation live at the end (2, 3, 9, 5): {}", live(&sim, &remaining));
            before_fix(label, "PF-1's fix (and VC8R-1's)", &sim, &honest, stopped);
            continue;
        }
        vc_report(&sim, &honest, "prevotefork");
        println!(
            "RESULT prevotefork: m{obs} votes to remove capper {capper}: {voted}; {capper} kept: {kept}; epochs at the end: {epochs}; confirmation live at the end (2, 3, 9, 5): {} -> {}",
            live(&sim, &remaining),
            if !voted && kept && live(&sim, &remaining) { "NO VOTE AGAINST THE CAPPER, AND CONFIRMATION GOES ON" } else { "FAIL" }
        );
        verdict(&sim, &honest, label, stopped);
    }
}

// ---- design-reviewer round-6 repros, fixed in round 7 ---------------------------------------------

/// Whether every honest member still confirms near its present at the end of the run. A halted member
/// sits at the speculation limit, its frame a whole window past its confirmed frame. A check on the vote a
/// repro targets alone passed a halted session (@ballpit-viz, mirroring prevotefork).
fn live(sim: &Sim, honest: &[MemberId]) -> bool {
    honest.iter().all(|&m| {
        let r = &sim.replica(m, 1).sim;
        r.frame.saturating_sub(r.conf) < r.cfg.window
    })
}

/// The verdict of a round-7 or round-8 repro (@ballpit-viz's request): the fix-off run must still show
/// the failure it was written for, and the run as shipped must pass; anything else prints FAIL, which
/// the suite's check catches whatever the bytes around it.
fn repro_verdict(name: &str, fixed: bool, ok: bool, pass: &str, control: &str) {
    let what = if fixed { pass } else { control };
    println!("RESULT {name} fixed={fixed}: -> {}", if ok { what } else { "FAIL" });
}

/// capstale, but 9's capped precommit names a polka id that exists nowhere (review VC6R-1, round 7).
/// Trust survey #4 retired the cap rule and its fetch budget, so the ids a capped precommit carries are
/// dead bytes that nothing reads (their removal, a wire change, is a follow-up; capfake then folds into
/// capstale). The cap holds confirmation until admin 1 kicks 9 at 6 s.
pub fn capfake() {
    {
        let ids = [1, 2, 3, 9];
        let ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (700, touch(33, 3.0))]) } else { BTreeMap::new() });
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 14_000_000);
        let mut capped: BTreeSet<u32> = BTreeSet::new();
        let mut kick = AdminKick::new(1, 9, 6_000_000);
        let stopped = run_quiet(&mut sim, |sim| {
            kick.step(sim, &[1, 2, 3], &ids);
            if sim.net.now_us < 2_000_000 || !sim.replica(1, 1).sim.vcm.members.contains(&9) {
                return;
            }
            let r9 = sim_mut(sim, 9);
            let epoch = r9.vcm.epoch;
            if capped.insert(epoch) {
                let boundary = r9.now + 30;
                let polka = vec![EntryId::fake(0xDEAD_BEEF_0000_0001)];
                let sig = test_fact_sig(9, 1, vc_fact(epoch, 0, 0xBAD0 ^ epoch as u64));
                let e = r9.author(Payload::Precommit { epoch, round: 0, value: 0xBAD0 ^ epoch as u64, boundary, polka, sig }, None);
                send_all(sim, 9, &ids, &e);
            }
        });
        let honest = [1, 2, 3];
        println!("REPRO capfake: 9 removed at {}; live at the end: {}; {}", secs(removed_at(&sim, 1, 9)), live(&sim, &honest), confs(&sim, &honest));
        kick.report(&sim, &honest, "capfake");
        verdict(&sim, &honest, "capfake", stopped);
    }
}

/// fakemix's pure fake, but 8 also authors a no-op SetRole every 2 s, so a decision comes every 2 s
/// (review VC6R-4, round 6: a decision once reset the withholding timers that would catch 8). Trust
/// survey #4 retired the withholding rule: 8 holds confirmation until admin 1 kicks it at 6 s, through
/// the decisions its SetRoles force.
pub fn fakeadmin() {
    {
        let setrole = true;
        let ids = [1, 2, 3, 4, 8];
        let ps = peers_of(&ids, |m| if m == 1 { plan(&(0..40).map(|k| (60 + 20 * k, touch(10 + (k % 3) as u32, 1.0))).collect::<Vec<_>>()) } else { BTreeMap::new() });
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 14_000_000);
        let mut last = 0u64;
        let mut kick = AdminKick::new(1, 8, 6_000_000);
        let stopped = run_quiet(&mut sim, |sim| {
            kick.step(sim, &[1, 2, 3, 4], &ids);
            let t = sim.net.now_us;
            if t < 2_000_000 || !sim.replica(1, 1).sim.vcm.members.contains(&8) {
                return;
            }
            sim_mut(sim, 8).fake_refs = Some((1, 0));
            if setrole && t >= last + 2_000_000 {
                last = t;
                let r8 = sim_mut(sim, 8);
                let epoch = r8.vcm.epoch;
                let e = r8.author(Payload::SetRole { epoch, member: 8, role: ADMIN }, None);
                send_all(sim, 8, &ids, &e);
            }
        });
        let honest = [1, 2, 3, 4];
        let vcs = sim.replica(1, 1).sim.vcm.vc_log.len();
        println!("REPRO fakeadmin (no-op SetRole every 2 s): 8 removed at {}; decisions at m1 {vcs}; live at the end: {}; {}", secs(removed_at(&sim, 1, 8)), live(&sim, &honest), confs(&sim, &honest));
        kick.report(&sim, &honest, "fakeadmin");
        verdict(&sim, &honest, "fakeadmin", stopped);
    }
}

/// VC6R-2: a join into a session of n members (64 and 72), admitted by quorum.
pub fn bigjoin() {
    for n in [64u32, 72] {
        let ids: Vec<MemberId> = (1..=n).collect();
        let joiner: MemberId = 500;
        let mut ps = peers_of(&ids, |_| BTreeMap::new());
        let mut p = Peer::new(joiner, 0);
        p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
        ps.push(p);
        for p in ps.iter_mut().filter(|p| p.id != joiner) {
            p.worlds.get_mut(&1).unwrap().sim.follow(joiner);
        }
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 6_000_000);
        let mut sent = false;
        let stopped = run_quiet(&mut sim, |sim| {
            if !sent && sim.net.now_us >= 1_000_000 {
                for &m in &ids {
                    sim.net.send(1, joiner, m, &join_msg(&TEST_SESSION, joiner, vec![], u64::MAX));
                }
                sent = true;
            }
        });
        let admitted = sim.replica(1, 1).sim.vcm.members.contains(&joiner);
        println!("REPRO bigjoin n={n}: run stopped early (panic or assertion) at {stopped:?} us; joiner admitted at m1 {admitted}");
    }
}

/// VC6R-3: the admin-fork wedge, after the subagent's schedule. {1,2,3,4,9}, all admins. An honest
/// member P (not the round-0 proposer) is cut off 2.0-3.2 s. At 2.1 s faulty 9 sends a SetRole A to
/// the three others and drops its own precommits; once they have precommitted it crashes. Its fork
/// A' (same seq, an Attest) reaches P at 3.1 s and the others at 3.2 s. Fix off (VC6R3): the locked
/// members find their value's admin entry disputed, nothing releases the lock, and the session halts.
pub fn adminwedge() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 4, 9];
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (700, touch(33, 3.0))]) } else { BTreeMap::new() });
        let p0 = ps[0].worlds[&1].sim.proposer_of(0, 0).unwrap();
        let part: MemberId = if p0 != 4 { 4 } else { 3 };
        let others: Vec<MemberId> = [1, 2, 3, 4].into_iter().filter(|&m| m != part).collect();
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VC6R3);
        }
        let fork_id = std::rc::Rc::new(std::cell::Cell::new(EntryId::NONE));
        let crashed = std::rc::Rc::new(std::cell::Cell::new(false));
        let (ff, fc) = (fork_id.clone(), crashed.clone());
        let mut net = Net::new(link(30, 10, 0), 7);
        net.filter = Some(Box::new(move |t, _w, from, to, m| {
            let whitelisted = matches!(m, Msg::Entry(e) if ff.get() != EntryId::NONE && e.id() == ff.get());
            if whitelisted {
                return Fate::Deliver;
            }
            if (from == part || to == part) && (2_000_000..3_200_000).contains(&t) {
                return Fate::Drop;
            }
            if from == 9 && t >= 2_000_000 {
                if fc.get() {
                    return Fate::Drop;
                }
                if matches!(m, Msg::Entry(e) if matches!(e.payload, Payload::Precommit { .. })) {
                    return Fate::Drop;
                }
            }
            Fate::Deliver
        }));
        let mut sim = Sim::new(net, ps, 14_000_000);
        let mut step = 0;
        let mut fork: Option<Entry> = None;
        let mut crash_at = None;
        let stopped = run_quiet(&mut sim, |sim| {
            let t = sim.net.now_us;
            if step == 0 && t >= 2_100_000 {
                let r9 = sim_mut(sim, 9);
                let epoch = r9.vcm.epoch;
                let a = r9.author(Payload::SetRole { epoch, member: 9, role: ADMIN }, None);
                let b = Entry { payload: Payload::Attest { fact: Fact([1; 32]), sig: [0; 64] }, ..a.entry.clone() };
                fork_id.set(b.id());
                fork = Some(b);
                send_all(sim, 9, &[1, 2, 3, 4], &a);
                step = 1;
            } else if step == 1 {
                let all_pc = others.iter().all(|&m| sim.replica(m, 1).sim.all_entries().iter().any(|e| e.member == m && matches!(e.payload, Payload::Precommit { .. })));
                if all_pc {
                    crashed.set(true);
                    crash_at = Some(t);
                    step = 2;
                }
            } else if step == 2 && t >= 3_100_000 {
                sim.net.send(1, 9, part, &Msg::Entry(test_sign(1, fork.clone().unwrap())));
                step = 3;
            } else if step == 3 && t >= 3_200_000 {
                for &m in &others {
                    sim.net.send(1, 9, m, &Msg::Entry(test_sign(1, fork.clone().unwrap())));
                }
                step = 4;
            }
        });
        let honest = [1, 2, 3, 4];
        println!(
            "REPRO adminwedge fixed={fixed}: proposer(0,0)={p0}, partitioned {part}; step reached {step}, 9 crashed at {crash_at:?} us; 9 removed at {}; live at the end: {}; {}",
            secs(removed_at(&sim, 1, 9)),
            live(&sim, &honest),
            confs(&sim, &honest)
        );
        let (gone, up) = (removed_at(&sim, 1, 9).is_some(), live(&sim, &honest));
        repro_verdict("adminwedge", fixed, if fixed { gone && up } else { !gone && !up }, "REMOVED, CONFIRMATION GOES ON", "HALTS, AS BEFORE THE FIX");
        vc_report(&sim, &honest, "adminwedge");
        verdict(&sim, &honest, &format!("adminwedge fixed={fixed}"), stopped);
    }
}

// ---- design-reviewer round-7 repros, fixed in round 8 ---------------------------------------------

/// VC7R-1: round 7's admin validity still followed the branch of its author's chain a replica had linked.
/// Faulty member f, the round-0 proposer, authors an Attest `b` as its seq s and forges `a`, a SetRole
/// with the same seq and parent, stamped behind the horizon f has declared (horizon-void, VCR-9). `a`
/// reaches member P first and the others second; `b` the others first and P second. f's own chain goes
/// on from `b`, so the others follow it; f proposes a change listing `a`, prevotes it (its own code
/// skips the admin checks), drops its precommits, and crashes once the others have precommitted. P
/// linked `a`, so it is void there; the others hold it as a duplicate, which round 7 never validated, so
/// with the fix off (`VC7R1`) they lock on the change while P prevotes nil for it forever.
pub fn voidfork() {
    for fixed in RUNS {
        let ids = [1, 2, 3, 4];
        let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (700, touch(33, 3.0))]) } else { BTreeMap::new() });
        let f = ps[0].worlds[&1].sim.proposer_of(0, 0).unwrap();
        let honest: Vec<MemberId> = ids.iter().copied().filter(|&m| m != f).collect();
        let p = *honest.last().unwrap();
        let others: Vec<MemberId> = honest.iter().copied().filter(|&m| m != p).collect();
        guests(&mut ps, &ids);
        if !fixed {
            each_sim(&mut ps, |s| s.fixes_off = fix::VC7R1);
        }
        for pe in ps.iter_mut().filter(|pe| pe.id == f) {
            pe.worlds.get_mut(&1).unwrap().sim.fixes_off |= fix::VC3R3;
        }
        let crashed = std::rc::Rc::new(std::cell::Cell::new(false));
        let fc = crashed.clone();
        let mut net = Net::new(link(30, 10, 0), 7);
        net.filter = Some(Box::new(move |_t, _w, from, _to, m| {
            if from == f && (fc.get() || matches!(m, Msg::Entry(e) if matches!(e.payload, Payload::Precommit { .. }))) {
                return Fate::Drop;
            }
            Fate::Deliver
        }));
        let mut sim = Sim::new(net, ps, 14_000_000);
        let mut step = 0;
        let mut fork: Option<(SignedEntry, SignedEntry)> = None;
        let mut crash_at = None;
        let stopped = run_quiet(&mut sim, |sim| {
            let t = sim.net.now_us;
            if step == 0 && t >= 2_000_000 {
                let rf = sim_mut(sim, f);
                let epoch = rf.vcm.epoch;
                let b = rf.author(Payload::Attest { fact: Fact([1; 32]), sig: [0; 64] }, None);
                let a = test_sign(1, Entry { payload: Payload::SetRole { epoch, member: p, role: GUEST }, stamp: b.stamp.saturating_sub(60), ..b.entry.clone() });
                rf.byz_change = Some(Change { epoch, admin: vec![(a.id(), true)], ..Default::default() });
                send_all(sim, f, &others, &b);
                sim.net.send(1, f, p, &Msg::Entry(a.clone()));
                fork = Some((a, b));
                step = 1;
            } else if step == 1 && t >= 2_100_000 {
                let (a, b) = fork.clone().unwrap();
                send_all(sim, f, &others, &a);
                sim.net.send(1, f, p, &Msg::Entry(b));
                step = 2;
            } else if step == 2 {
                let all_pc = others.iter().all(|&m| sim.replica(m, 1).sim.all_entries().iter().any(|e| e.member == m && matches!(e.payload, Payload::Precommit { .. })));
                if all_pc {
                    crashed.set(true);
                    crash_at = Some(t);
                    step = 3;
                }
            }
        });
        println!(
            "REPRO voidfork fixed={fixed}: proposer(0,0)={f}, linked the void version first: {p}; step reached {step}, {f} crashed at {crash_at:?} us; {f} removed at {}; live at the end: {}; {}",
            secs(removed_at(&sim, honest[0], f)),
            live(&sim, &honest),
            confs(&sim, &honest)
        );
        let (gone, up) = (removed_at(&sim, honest[0], f).is_some(), live(&sim, &honest));
        repro_verdict("voidfork", fixed, if fixed { gone && up } else { !gone && !up }, "THE VOID LISTING IS REFUSED EVERYWHERE, CONFIRMATION GOES ON", "HALTS, AS BEFORE THE FIX");
        vc_report(&sim, &honest, "voidfork");
        verdict(&sim, &honest, &format!("voidfork fixed={fixed}"), stopped);
    }
}

/// VC7R-2 (round 7): prevotefork's layout, with every delivery of the forked value-prevote to observer
/// 3 delayed 1.5 s, and 9's prevote lost until 5 s (in prevotefork it is back at 2.5 s and completes the
/// polka by itself). Only capper 2 holds the forked prevote, so 3's fetch waits on 2's replies. Before
/// VC7R-2's fix, 3 voted against honest capper 2 when a fetch budget ran out (for 2's vote against 1,
/// whose frozen head lies past 1's fork). Trust survey #4 retired both budgets with the rules they fed,
/// so the run checks that 3 casts no vote against 2 and confirmation goes on.
pub fn slowcapper() {
    let (_, capper, obs, joiner) = PREVOTEFORK;
    {
        let (sim, stopped) = prevotefork_sim(0, 1_500_000, 5_000_000, u64::MAX);
        let honest = [1, 2, 3];
        let remaining = [capper, obs, 9, joiner];
        let voted = sim.replica(obs, 1).sim.all_entries().iter().any(|e| e.member == obs && matches!(e.payload, Payload::Vote { target, .. } if target == capper));
        let kept = sim.replica(obs, 1).sim.vcm.members.contains(&capper);
        vc_report(&sim, &honest, "slowcapper");
        println!(
            "REPRO slowcapper (3 receives the cap's polka prevote 1.5 s late): m{obs} votes to remove capper {capper}: {voted}; {capper} kept: {kept}; live at the end (2, 3, 9, 5): {}; {}",
            live(&sim, &remaining),
            confs(&sim, &honest)
        );
        let up = live(&sim, &remaining);
        println!("RESULT slowcapper: -> {}", if !voted && kept && up { "THE SLOW CAPPER IS KEPT, CONFIRMATION GOES ON" } else { "FAIL" });
        verdict(&sim, &honest, "slowcapper", stopped);
    }
}

// ---- design-reviewer round-8 repros (its probe `voiddeep`), fixed in round 8 ----------------------

/// What faulty member f forks at seq s in `fork_segment_sim`.
#[derive(Clone, Copy, PartialEq)]
enum Segment {
    /// [an ack declaring horizon H, a SetRole stamped behind H], so the SetRole is void (VC7R-1).
    VoidRole,
    /// The same, but the ack breaks VC-5, so it counts for nothing and the SetRole is valid.
    ValidRole,
    /// Two Attests, and an empty change: no admin entry at all (VC8R-1).
    Plain,
}

/// @design-reviewer's probe `voiddeep` (round 8). Members 1-4, f the round-0 proposer. At 2 s f authors
/// two Attests as seq s and s+1, its real chain, and forges branch A at the same seqs: `seg` decides
/// what A holds and what f proposes. A reaches member P first and the others second; f's own branch
/// the others first and P second. f's own code skips the admin checks, so it prevotes its listing; it
/// drops its precommits, and crashes once the others have precommitted (or at 6 s).
fn fork_segment_sim(seg: Segment, off: u64) -> (Sim, Option<u64>, MemberId, MemberId, Vec<MemberId>) {
    let ids = [1, 2, 3, 4];
    let mut ps = peers_of(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (700, touch(33, 3.0))]) } else { BTreeMap::new() });
    let f = ps[0].worlds[&1].sim.proposer_of(0, 0).unwrap();
    let honest: Vec<MemberId> = ids.iter().copied().filter(|&m| m != f).collect();
    let p = *honest.last().unwrap();
    let others: Vec<MemberId> = honest.iter().copied().filter(|&m| m != p).collect();
    guests(&mut ps, &ids);
    each_sim(&mut ps, move |s| s.fixes_off = off);
    for pe in ps.iter_mut().filter(|pe| pe.id == f) {
        pe.worlds.get_mut(&1).unwrap().sim.fixes_off |= fix::VC3R3;
    }
    let crashed = std::rc::Rc::new(std::cell::Cell::new(false));
    let fc = crashed.clone();
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(Box::new(move |_t, _w, from, _to, m| {
        if from == f && (fc.get() || matches!(m, Msg::Entry(e) if matches!(e.payload, Payload::Precommit { .. }))) {
            return Fate::Drop;
        }
        Fate::Deliver
    }));
    let mut sim = Sim::new(net, ps, 14_000_000);
    let mut step = 0;
    let mut branch_a: Vec<SignedEntry> = vec![];
    let mut branch_b: Vec<SignedEntry> = vec![];
    let stopped = run_quiet(&mut sim, |sim| {
        let t = sim.net.now_us;
        if step == 0 && t >= 2_000_000 {
            let rf = sim_mut(sim, f);
            let epoch = rf.vcm.epoch;
            let b1 = rf.author(Payload::Attest { fact: Fact([1; 32]), sig: [0; 64] }, None);
            let b2 = rf.author(Payload::Attest { fact: Fact([2; 32]), sig: [0; 64] }, None);
            branch_a = if seg == Segment::Plain {
                rf.byz_change = Some(Change { epoch, ..Default::default() });
                vec![test_sign(1, Entry { payload: Payload::Attest { fact: Fact([3; 32]), sig: [0; 64] }, ..b1.entry.clone() })]
            } else {
                let h = b1.stamp + 40;
                let ack_epoch = if seg == Segment::ValidRole { epoch + 2 } else { epoch };
                let a1 = test_sign(1, Entry { payload: Payload::Ack { horizon: h, heads: vec![], check: None, epoch: ack_epoch }, ..b1.entry.clone() });
                let a2 = test_sign(1, Entry { seq: b2.seq, prev: a1.id(), stamp: b1.stamp + 10, payload: Payload::SetRole { epoch, member: p, role: GUEST }, carried: None, ..b2.entry.clone() });
                rf.byz_change = Some(Change { epoch, admin: vec![(a2.id(), true)], ..Default::default() });
                vec![a1, a2]
            };
            branch_b = vec![b1, b2];
            for e in branch_b.clone() {
                send_all(sim, f, &others, &e);
            }
            for e in branch_a.clone() {
                sim.net.send(1, f, p, &Msg::Entry(e));
            }
            step = 1;
        } else if step == 1 && t >= 2_100_000 {
            for e in branch_a.clone() {
                send_all(sim, f, &others, &e);
            }
            for e in branch_b.clone() {
                sim.net.send(1, f, p, &Msg::Entry(e));
            }
            step = 2;
        } else if step == 2 {
            let all_pc = others.iter().all(|&m| sim.replica(m, 1).sim.all_entries().iter().any(|e| e.member == m && matches!(e.payload, Payload::Precommit { .. })));
            if all_pc || t >= 6_000_000 {
                crashed.set(true);
                step = 3;
            }
        }
    });
    (sim, stopped, f, p, honest)
}

/// VC7R-1 over a fork segment of two entries (@design-reviewer's probe): branch A is [an ack declaring
/// horizon H, a SetRole stamped behind H], so the members that kept f's other branch replay A's ack
/// along the SetRole's own ancestry. Void: refused everywhere (with `VC7R1` off they lock on it and the
/// session halts). With the ack breaking VC-5 it counts for nothing, the SetRole is valid on its own
/// ancestry, and every member decides it (P demoted), which also needs VC8R-1.
pub fn voiddeep() {
    for (seg, off) in [(Segment::VoidRole, fix::VC7R1), (Segment::VoidRole, 0), (Segment::ValidRole, 0)] {
        let (sim, stopped, f, p, honest) = fork_segment_sim(seg, off);
        let (gone, up) = (removed_at(&sim, honest[0], f).is_some(), live(&sim, &honest));
        let demoted = honest.iter().filter(|&&m| m != p).all(|&m| sim.replica(m, 1).sim.vcm.roles.get(&p) == Some(&GUEST));
        let label = if seg == Segment::VoidRole { "void" } else { "valid" };
        println!(
            "REPRO voiddeep {label} fixes_off={off:#x}: f={f}, P={p}; {f} removed at {}; P demoted: {demoted}; live at the end: {up}; {}",
            secs(removed_at(&sim, honest[0], f)),
            confs(&sim, &honest)
        );
        match (seg, off) {
            (Segment::VoidRole, 0) => repro_verdict("voiddeep void", true, gone && up && !demoted, "THE VOID LISTING IS REFUSED EVERYWHERE, CONFIRMATION GOES ON", ""),
            (Segment::VoidRole, _) => repro_verdict("voiddeep void", false, !gone && !up, "", "HALTS, AS BEFORE THE FIX"),
            _ => repro_verdict("voiddeep valid", true, gone && up && demoted, "THE LISTING, VALID ON ITS OWN ANCESTRY, IS DECIDED EVERYWHERE", ""),
        }
        vc_report(&sim, &honest, "voiddeep");
        verdict(&sim, &honest, &format!("voiddeep {label} fixes_off={off:#x}"), stopped);
    }
}

/// VC8R-1 (@design-reviewer's probe `voiddeep plain`): one faulty proposer halted the session for good
/// with no admin entry at all. f forks seq s into two Attests (one to P, one to the others), goes on
/// from the others' version, proposes an empty change, prevotes it, drops its precommits and crashes.
/// The others lock on the round-0 polka (theirs and f's). With the fix off (`VC8R1`), P cannot count
/// f's prevote (it kept the other version), never sees that polka, and the locked members prevote nil
/// on everything P proposes.
pub fn plainfork() {
    for fixed in RUNS {
        let (sim, stopped, f, p, honest) = fork_segment_sim(Segment::Plain, if fixed { 0 } else { fix::VC8R1 });
        let (gone, up) = (removed_at(&sim, honest[0], f).is_some(), live(&sim, &honest));
        println!(
            "REPRO plainfork fixed={fixed}: f={f}, P={p}; {f} removed at {}; live at the end: {up}; {}",
            secs(removed_at(&sim, honest[0], f)),
            confs(&sim, &honest)
        );
        repro_verdict("plainfork", fixed, if fixed { gone && up } else { !gone && !up }, "THE FORKER IS REMOVED, CONFIRMATION GOES ON", "HALTS, AS BEFORE THE FIX");
        vc_report(&sim, &honest, "plainfork");
        verdict(&sim, &honest, &format!("plainfork fixed={fixed}"), stopped);
    }
}

/// PF-1 with VC8R-1 on: prevotefork, but 9 falls silent at 1 s, so the only quorum is {1, 2, 3} and the
/// forker's precommit is in it. 3 counts 1's prevote toward the polka (VC8R-1), locks and precommits,
/// but can decide only by counting 1's precommit, which sits past 1's fork in 3's copy. With PF-1 off,
/// 3 and the joiner stay in epoch 0 while 1 and 2 decide, and confirmation halts.
pub fn pfsilent() {
    let (_, capper, obs, joiner) = PREVOTEFORK;
    for fixed in RUNS {
        let (sim, stopped) = prevotefork_sim(if fixed { 0 } else { fix::PF1 }, 0, 2_500_000, 1_000_000);
        let honest = [1, 2, 3];
        let remaining = [capper, obs, joiner];
        let epochs = [1, 2, 3, joiner].map(|m| format!("m{m} {}", sim.replica(m, 1).sim.vcm.epoch)).join(", ");
        let up = live(&sim, &remaining);
        println!("REPRO pfsilent fixed={fixed} (9 silent from 1 s): epochs at the end: {epochs}; live at the end (2, 3, 5): {up}; {}", confs(&sim, &honest));
        repro_verdict("pfsilent", fixed, if fixed { up } else { !up }, "EVERY MEMBER DECIDES ON THE FORKER'S PRECOMMIT, CONFIRMATION GOES ON", "HALTS, AS BEFORE THE FIX");
        vc_report(&sim, &honest, "pfsilent");
        verdict(&sim, &honest, &format!("pfsilent fixed={fixed}"), stopped);
    }
}
