//! Thin links Part B (thin-link plan revision 17) in the spike: a scenario per rule as each lands.
//!
//! * `stale`: rule 2. Four members; member 4's downlink runs 1 s behind while its uplink is fine, so it
//!   acts on objects without having seen the others' recent writes. Its carried state is skipped where
//!   another member's write is more than G_{4,a} frames older and unseen, kept within G (a race), and a
//!   member that had seen the write keeps its own. Run with Part B off, then on.

use std::collections::BTreeMap;

use pe_session_sim::*;
use pe_sim_core::session::*;

use crate::review2::{link, plan, run_to_end, touch};

const SCENE: SceneKind = SceneKind::Pile { side: 8, layers: 2 };

fn peers(ids: &[MemberId], localize: bool, plans: impl Fn(MemberId) -> BTreeMap<u64, Vec<Action>>) -> Vec<Peer> {
    ids.iter()
        .map(|&id| {
            let mut p = Peer::new(id, 0);
            p.join(1, ids, SCENE, SimConfig { localize, ..SimConfig::default() }, 100_000, plans(id));
            p
        })
        .collect()
}

/// Every member agrees with every other and with an offline replay of the final log, over the frames every
/// member has simulated. The replay reads the live replica's Part B window values and ack heads
/// (`replay_sim_like`), which every replica derives from the same log.
fn verdict(sim: &Sim, ids: &[MemberId], label: &str, panic_at: Option<u64>) -> bool {
    if let Some(t) = panic_at {
        println!("RESULT {label}: PANIC at t={t} ms -> FAIL");
        return false;
    }
    let frames = ids.iter().map(|&m| sim.replica(m, 1).sim.frame).min().unwrap_or(0).saturating_sub(1);
    let first = &sim.replica(ids[0], 1).sim;
    let members: Vec<MemberId> = first.members.iter().copied().collect();
    let rd = digest(&replay_sim_like(first, SCENE, &members, &final_log(first), frames).hashes, frames);
    let ds: Vec<(MemberId, u64)> = ids.iter().map(|&m| (m, digest(&sim.replica(m, 1).sim.hashes, frames))).collect();
    let bad: u64 = ids.iter().map(|&m| sim.replica(m, 1).sim.stats.checks_bad).sum();
    let equal = ds.iter().all(|(_, d)| *d == rd);
    for &m in ids {
        let r = &sim.replica(m, 1).sim;
        println!(
            "  m{m}: conf {} frame {} rollbacks {} max depth {} paused frames {} checks ok {} bad {}",
            r.conf, r.frame, r.stats.rollbacks, r.stats.max_depth, r.stats.paused_frames, r.stats.checks_ok, r.stats.checks_bad
        );
    }
    println!(
        "RESULT {label}: frames 0..{frames} {} | offline replay {rd:016x} | {}",
        ds.iter().map(|(m, d)| format!("m{m}={d:016x}")).collect::<Vec<_>>().join(" "),
        if equal && bad == 0 { "ALL EQUAL, NO FALSE CHECKS" } else { "FAIL" }
    );
    equal && bad == 0
}

pub fn stale() {
    let ids = [1, 2, 3, 4];
    // (frame, member, object, impulse): who writes which object when.
    let script: Vec<(u64, MemberId, u32, f32)> = vec![
        // Windows 0 and 1: no data yet, so G = max(J_acc, Y_acc) + R + 4 = 29 frames.
        (100, 1, 50, 6.0),
        (140, 4, 50, 4.0), // 40 frames after member 1's write, which member 4 hasn't seen: stale
        (160, 2, 50, 3.0), // member 2 has seen both: kept
        (200, 1, 77, 6.0),
        (215, 4, 77, 4.0), // 15 frames after an unseen write: a race, kept
        // Windows 4 and 5: G from logged data.
        (1300, 1, 20, 6.0),
        (1345, 4, 20, 4.0),
        (1400, 1, 33, 6.0),
        (1420, 4, 33, 4.0),
    ];
    for localize in [false, true] {
        let label = if localize { "stale (Part B on)" } else { "stale (Part B off)" };
        let plans = |m: MemberId| plan(&script.iter().filter(|x| x.1 == m).map(|&(f, _, o, y)| (f, touch(o, y))).collect::<Vec<_>>());
        let mut sim = Sim::new(Net::new(link(30, 5, 0), 23), peers(&ids, localize, plans), 30_000_000);
        // Member 4's downlink: 1 s from every other member. Its uplink keeps the default 30 ms.
        for x in [1, 2, 3] {
            sim.net.links.insert((x, 4), link(1000, 5, 0));
        }
        let panic_at = run_to_end(&mut sim, |_| {});
        if panic_at.is_none() {
            let r1 = &sim.replica(1, 1).sim;
            for e in r1.all_entries().into_iter().filter(|e| e.member != 1 && matches!(e.payload, Payload::Touch { .. })) {
                let at: Vec<String> = ids
                    .iter()
                    .map(|&m| match sim.replica(m, 1).sim.rule2.get(&e.id()) {
                        Some(true) => "skipped".to_string(),
                        Some(false) => "kept".to_string(),
                        None => "-".to_string(),
                    })
                    .collect();
                let mut same = at.clone();
                same.dedup();
                println!(
                    "RESULT {label}: m{} s{} stamped {} carries {:?}: {:?}; rule 2: {}",
                    e.member,
                    e.seq,
                    e.stamp,
                    e.carried.map(|c| c.0),
                    r1.outcomes.get(&e.id()).map(|(_, o)| *o),
                    if same.len() == 1 { format!("{} at every member", at[0]) } else { format!("per member {}", at.join(" / ")) }
                );
            }
            if localize {
                for (k, v) in sim.replica(1, 1).sim.window_values().range(2..=5) {
                    println!(
                        "RESULT {label}: window {k}: L {:?}, Q {} (floor {}), E {}, C_max {}, P {:?}, G(4,1) {}",
                        v.l,
                        v.q,
                        v.q_floor,
                        v.e,
                        v.c_max,
                        v.p,
                        WorldSim::race_window(v, &SimConfig::default(), 4, 1)
                    );
                }
            }
        }
        verdict(&sim, &ids, label, panic_at);
    }
}

/// Rules 1 and 3: every member touches its own object every 20 frames; the last member's uplink runs behind
/// (1.2 s, past C_max; or 2.5 s, past the speculation window) while its downlink is fine. Its actions are void at
/// every member (q lackers at each deadline) and nobody else's are. At 8 members the others' confirmation stops
/// waiting for it past its deadline (q = 6 of the 7 others make echo-stability). At 4, q = 3 is every other
/// member, so the others still wait for its references (the plan's n = 4-5 case). Part B off, then on.
pub fn late() {
    // (members, uplink delay, slow members: the last ones).
    for (n, delay_ms, n_slow) in [(4u32, 1200u64, 1u32), (4, 2500, 1), (5, 1200, 1), (8, 1200, 1), (8, 2500, 1), (8, 1200, 2), (4, 1200, 2), (8, 1200, 3)] {
        let ids: Vec<MemberId> = (1..=n).collect();
        let slow = n;
        let slow_set: Vec<MemberId> = (n + 1 - n_slow..=n).collect();
        for localize in [false, true] {
            let label = format!("late n={n} uplink {delay_ms} ms x{n_slow} (Part B {})", if localize { "on" } else { "off" });
            let plans = |m: MemberId| plan(&(100..1500).step_by(20).map(|f| (f + m as u64, touch(10 + m, 2.0))).collect::<Vec<_>>());
            let mut sim = Sim::new(Net::new(link(30, 5, 0), 29), peers(&ids, localize, plans), 30_000_000);
            for &s in &slow_set {
                for &x in ids.iter().filter(|&&x| x != s) {
                    sim.net.links.insert((s, x), link(delay_ms, 5, 0));
                }
            }
            let mut lag_sum = vec![0u64; ids.len()];
            let mut lag_n = 0u64;
            // The first frame member 1 confirmed past 0, and member 1's present then.
            let mut first_conf: Option<(u64, u64)> = None;
            let panic_at = run_to_end(&mut sim, |s| {
                if first_conf.is_none() {
                    let r = &s.replica(1, 1).sim;
                    if r.conf > 0 {
                        first_conf = Some((r.conf, r.now));
                    }
                }
                if s.net.now_us >= 3_000_000 && s.net.now_us % 100_000 == 0 {
                    for (i, &m) in ids.iter().enumerate() {
                        let r = &s.replica(m, 1).sim;
                        lag_sum[i] += r.now.saturating_sub(r.conf);
                    }
                    lag_n += 1;
                }
            });
            if panic_at.is_none() {
                // Verdicts per author, at member 1, and whether every member reached the same ones.
                let r1 = &sim.replica(1, 1).sim;
                let mut tally: BTreeMap<MemberId, (u32, u32, u32)> = BTreeMap::new();
                for e in r1.all_entries().into_iter().filter(|e| matches!(e.payload, Payload::Touch { .. })) {
                    let t = tally.entry(e.member).or_default();
                    match r1.verdicts.get(&e.id()) {
                        Some(true) => t.0 += 1,
                        Some(false) => t.1 += 1,
                        None => t.2 += 1,
                    }
                }
                let same = ids.iter().all(|&m| {
                    let r = &sim.replica(m, 1).sim;
                    r1.verdicts.iter().all(|(id, v)| r.verdicts.get(id).is_none_or(|w| w == v))
                });
                println!(
                    "RESULT {label}: touches per author (count / void / undecided) at m1: {}; every member's verdicts agree: {same}",
                    tally.iter().map(|(m, (c, v, u))| format!("m{m} {c}/{v}/{u}")).collect::<Vec<_>>().join(", ")
                );
                if let Some((c, at)) = first_conf {
                    println!("RESULT {label}: member 1 first confirmed past frame 0 at its frame {at} (conf {c})");
                }
                println!(
                    "RESULT {label}: mean confirmation lag from 3 s, frames: {}",
                    lag_sum.iter().zip(&ids).map(|(s, m)| format!("m{m} {:.1}", *s as f64 / lag_n.max(1) as f64)).collect::<Vec<_>>().join(" / ")
                );
                for (k, v) in r1.window_values() {
                    println!(
                        "RESULT {label}: window {k}: Q {} (floor {}), E {}, C_max {}, L {:?}, deadline offset for m1 {}, for m{slow} {}",
                        v.q,
                        v.q_floor,
                        v.e,
                        v.c_max,
                        v.l,
                        WorldSim::deadline_offset(v, &SimConfig::default(), 1),
                        WorldSim::deadline_offset(v, &SimConfig::default(), slow)
                    );
                }
            }
            verdict(&sim, &ids, &label, panic_at);
        }
    }
}
