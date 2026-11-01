//! Thin links Part B (thin-link plan revision 17) in the spike: a scenario per rule as each lands.
//!
//! * `stale`: rule 2. Four members; member 4's downlink runs 1 s behind while its uplink is fine, so it
//!   acts on objects without having seen the others' recent writes. Its carried state is skipped where
//!   another member's write is more than G_{4,a} frames older and unseen, kept within G (a race), and a
//!   member that had seen the write keeps its own. Run with Part B off, then on.

use std::collections::{BTreeMap, BTreeSet};

use pe_session_sim::*;
use pe_sim_core::session::*;

use crate::p24r::sim_mut;
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
    let rs = replay_sim_like(first, SCENE, &members, &final_log(first), frames);
    let rd = digest(&rs.hashes, frames);
    // Diagnostics: the first frame where the first replica and the offline replay differ, and what acts there.
    if let Some(f) = (0..frames).find(|f| first.hashes.get(f) != rs.hashes.get(f)) {
        let acts: Vec<String> = final_log(first)
            .iter()
            .filter(|(_, eff)| eff + 2 >= f && *eff <= f + 2)
            .map(|(e, eff)| {
                let id = e.id();
                format!(
                    "m{} s{} stamp {} eff {} live {:?} rule2 {:?} | replay {:?} rule2 {:?}",
                    e.member,
                    e.seq,
                    e.stamp,
                    eff,
                    first.outcomes.get(&id),
                    first.rule2.get(&id),
                    rs.outcomes.get(&id),
                    rs.rule2.get(&id)
                )
            })
            .collect();
        println!("  first difference from the replay at frame {f} (conf {}): acting near it {:?}", first.conf, acts);
    }
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

/// RV16-2 (rule 4's exception, plan item 7). Member 3 is fed on an 8 kbit/s link, which falls to 0.5 kbit/s (under
/// the floors) from 5 s to 25 s, then recovers to 64 kbit/s. The others vote it out like a disconnection: no
/// timely echo of their chains for X frames. The removed player follows at once as a spectator under its next id
/// (a new member, 10.8), and its client asks to join only once its link has stayed above both floors for 10 s.
/// `early` is the control: it asks 1 s after the removal, still under the floor.
pub fn rejoin() {
    for early in [false, true] {
        rejoin_run(early);
    }
}

fn rejoin_run(early: bool) {
    const NEXT: MemberId = 103;
    // The bandwidth floor at 4 players (plan, rule 4: about 2.5 kbit/s each way).
    const FLOOR_BPS: u64 = 2_500;
    let ids: Vec<MemberId> = vec![1, 2, 3, 4];
    let others: [MemberId; 3] = [1, 2, 4];
    let cfg = SimConfig { localize: true, ..SimConfig::default() };
    let plans = |m: MemberId| plan(&(120..2700).step_by(if m == 3 { 60 } else { 30 }).map(|f| (f + m as u64, touch(10 + m, 2.0))).collect::<Vec<_>>());
    let mut ps: Vec<Peer> = ids
        .iter()
        .map(|&id| {
            let mut p = Peer::new(id, 0);
            p.join(1, &ids, SCENE, cfg, 100_000, plans(id));
            p.worlds.get_mut(&1).unwrap().adapt = true;
            p
        })
        .collect();
    let mut p = Peer::new(NEXT, 0);
    p.join(1, &ids, SCENE, cfg, 100_000, BTreeMap::new());
    {
        let r = p.worlds.get_mut(&1).unwrap();
        r.adapt = true;
        r.sim.quiet_until_member = true;
    }
    ps.push(p);
    let mut net = Net::new(Link { delay_us: 50_000, jitter_us: 5_000, loss_pct: 0 }, 31);
    net.default_cap = Some(Cap { rate_bps: 256_000, burst_bytes: 4000, max_queue_us: 500_000 });
    net.batching = true;
    let mut sim = Sim::new(net, ps, 50_000_000);
    sim.feed = true;
    // Player 3's link, both directions, to every peer, under either id.
    let set_link = |s: &mut Sim, bps: u64| {
        for &x in &others {
            for a in [3, NEXT] {
                for pair in [(a, x), (x, a)] {
                    s.net.caps.insert(pair, Cap { rate_bps: bps, burst_bytes: 4000, max_queue_us: 500_000 });
                }
            }
        }
    };
    set_link(&mut sim, 8_000);
    let (mut removed, mut above, mut asked, mut admitted, mut removed2): (Option<u64>, Option<u64>, Option<u64>, Option<u64>, Option<u64>) = (None, None, None, None, None);
    let mut why: Option<String> = None;
    let mut fed3 = 0u64;
    // The others' paused frames when 3 is removed and when its next id is admitted, and their worst confirmation lag
    // in each phase (frames, time).
    let mut paused_at: [Vec<u64>; 2] = [Vec::new(), Vec::new()];
    let mut worst = [(0u64, 0u64); 3];
    let panic_at = run_to_end(&mut sim, |s| {
        let now = s.net.now_us;
        if now == 5_000_000 {
            set_link(s, 500);
        }
        if now == 25_000_000 {
            set_link(s, 64_000);
        }
        if s.is_fed(1, 3) {
            fed3 += 1;
        }
        let (has3, has_next) = {
            let r1 = &s.replica(1, 1).sim;
            (r1.members.contains(&3), r1.members.contains(&NEXT))
        };
        let phase = if removed.is_none() { 0 } else if admitted.is_none() { 1 } else { 2 };
        let lag = others.iter().map(|&m| { let r = &s.replica(m, 1).sim; r.now.saturating_sub(r.conf) }).max().unwrap_or(0);
        if lag > worst[phase].0 {
            worst[phase] = (lag, now);
        }
        if removed.is_none() && !has3 {
            removed = Some(now);
            why = s.replica(1, 1).sim.events.iter().find(|e| e.contains("member 3") && e.contains("vote to")).cloned();
            paused_at[0] = others.iter().map(|&m| s.replica(m, 1).sim.stats.paused_frames).collect();
            // The removed player follows at once as a spectator, under its next id (a new member, 10.8).
            let i3 = s.peers.iter().position(|p| p.id == 3).unwrap();
            s.peers[i3].crashed = true;
            for &m in &others {
                sim_mut(s, m).follow(NEXT);
            }
        }
        // Its client measures its link (the spike reads the caps) against both floors.
        let up = s.net.caps.get(&(NEXT, 1)).is_none_or(|c| c.rate_bps >= FLOOR_BPS) && s.net.caps.get(&(1, NEXT)).is_none_or(|c| c.rate_bps >= FLOOR_BPS);
        above = if up { Some(above.unwrap_or(now)) } else { None };
        if let (Some(t0), None) = (removed, asked) {
            let due = if early { now >= t0 + 1_000_000 } else { above.is_some_and(|a| now >= a + 10_000_000) };
            if due {
                for &m in &others {
                    s.net.send(1, NEXT, m, &join_msg(session_id(1, &ids), NEXT, vec![], u64::MAX));
                }
                asked = Some(now);
            }
        }
        if admitted.is_none() && has_next {
            admitted = Some(now);
            paused_at[1] = others.iter().map(|&m| s.replica(m, 1).sim.stats.paused_frames).collect();
        }
        if admitted.is_some() && removed2.is_none() && !has_next {
            removed2 = Some(now);
        }
    });
    let label = format!("rejoin ({})", if early { "control: asks 1 s after the removal, under the floor" } else { "asks after 10 s above both floors" });
    let secs = |t: Option<u64>| t.map_or("never".to_string(), |t| format!("{:.2} s", t as f64 / 1e6));
    let r1 = &sim.replica(1, 1).sim;
    let paused_end: Vec<u64> = others.iter().map(|&m| sim.replica(m, 1).sim.stats.paused_frames).collect();
    println!(
        "RESULT {label}: 3 fed {} ms; removed at {} ({}); spectator {} at once; join asked at {}; admitted at {}; removed again at {}",
        fed3,
        secs(removed),
        why.as_deref().map_or("no vote seen at m1".to_string(), |e| e.split("] ").nth(1).unwrap_or(e).to_string()),
        NEXT,
        secs(asked),
        secs(admitted),
        secs(removed2)
    );
    println!(
        "RESULT {label}: the others' paused frames: to the removal {:?}, removal to admission {:?}, after admission {:?}; worst lag (frames at s) before the removal {} at {:.2}, until admission {} at {:.2}, after {} at {:.2}",
        paused_at[0],
        paused_at[0].iter().zip(&paused_at[1]).map(|(a, b)| b.saturating_sub(*a)).collect::<Vec<_>>(),
        paused_at[1].iter().zip(&paused_end).map(|(a, b)| b.saturating_sub(*a)).collect::<Vec<_>>(),
        worst[0].0, worst[0].1 as f64 / 1e6, worst[1].0, worst[1].1 as f64 / 1e6, worst[2].0, worst[2].1 as f64 / 1e6
    );
    println!("RESULT {label}: floor votes per member {:?}; members at m1 {:?}", others.iter().map(|&m| sim.replica(m, 1).sim.stats.floor_removals).collect::<Vec<_>>(), r1.members);
    let finals: Vec<MemberId> = r1.members.iter().copied().collect();
    verdict(&sim, &finals, &label, panic_at);
}

// ---- Randomized Part B schedules (plan section 6: randsched's shapes that remove no one) --------------------

/// An honest member's link (at most n − q of them, with the faulty members, so every rule is in scope).
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// Its uplink runs `ms` late to every peer.
    SlowUp { ms: u64 },
    /// Its downlink is capped at `kbps` from every peer (it goes on the feed); `heal`: the cap lifts at GST.
    ThinDown { kbps: u64, heal: bool },
    /// Both directions run `ms` late.
    Distant { ms: u64 },
}

/// A faulty member's game, none of which gets anyone removed. Network games stop at GST.
#[derive(Clone, Debug)]
enum PbFault {
    /// Its touches are stamped `frames` before its present (within its deadline).
    Backdate { frames: u64 },
    /// Its entries reach `late` `by_us` late and the others on time.
    Split { late: Vec<MemberId>, by_us: u64 },
    /// Everything it sends is `by_us` late.
    Slow { by_us: u64 },
    /// Each of its own entries reaches one member only, rotating by seq.
    Rotate,
    /// As a feeder it sends every body 0.2 m higher than it is (it still acts and acks honestly).
    LyingFeeder,
}

struct PbSchedule {
    ids: Vec<MemberId>,
    shapes: BTreeMap<MemberId, Shape>,
    faulty: BTreeMap<MemberId, PbFault>,
    gst: u64,
    end_us: u64,
}

fn pb_mix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn pb_below(rng: &mut u64, n: u64) -> u64 {
    pb_mix(rng) % n.max(1)
}

fn pb_draw(seed: u64) -> PbSchedule {
    let mut rng = seed ^ 0x5041_5254_4252_4E44;
    let n = 4 + pb_below(&mut rng, 4) as u32; // 4..=7
    let ids: Vec<MemberId> = (1..=n).collect();
    let t = (n as usize - 1) / 3;
    let q = quorum(QuorumRule::Bft, n as usize).1;
    let room = n as usize - q;
    let f = pb_below(&mut rng, t.min(room) as u64 + 1) as usize;
    let k = pb_below(&mut rng, (room - f) as u64 + 1) as usize;
    let mut pool = ids.clone();
    let mut pick = |rng: &mut u64| pool.remove(pb_below(rng, pool.len() as u64) as usize);
    let mut faulty = BTreeMap::new();
    for _ in 0..f {
        let m = pick(&mut rng);
        let fault = match pb_below(&mut rng, 5) {
            0 => PbFault::Backdate { frames: 5 + pb_below(&mut rng, 20) },
            1 => {
                let others: Vec<MemberId> = ids.iter().copied().filter(|&x| x != m).collect();
                let late: Vec<MemberId> = others.iter().copied().filter(|_| pb_below(&mut rng, 2) == 0).collect();
                let late = if late.is_empty() || late.len() == others.len() { vec![others[0]] } else { late };
                PbFault::Split { late, by_us: 300_000 + pb_below(&mut rng, 1_200_000) }
            }
            2 => PbFault::Slow { by_us: 300_000 + pb_below(&mut rng, 1_200_000) },
            3 => PbFault::Rotate,
            _ => PbFault::LyingFeeder,
        };
        faulty.insert(m, fault);
    }
    let mut shapes = BTreeMap::new();
    for _ in 0..k {
        let m = pick(&mut rng);
        let shape = match pb_below(&mut rng, 3) {
            0 => Shape::SlowUp { ms: 300 + pb_below(&mut rng, 1_200) },
            1 => Shape::ThinDown { kbps: 8 + pb_below(&mut rng, 9), heal: pb_below(&mut rng, 2) == 0 },
            _ => Shape::Distant { ms: 150 + pb_below(&mut rng, 250) },
        };
        shapes.insert(m, shape);
    }
    // On a whole millisecond: the run steps 1 ms at a time.
    let gst = (8_000_000 + pb_below(&mut rng, 3_000_000)) / 1000 * 1000;
    PbSchedule { ids, shapes, faulty, gst, end_us: gst + 12_000_000 }
}

/// One seed: (ok, the line printed).
fn pb_run(seed: u64) -> (bool, String) {
    let sch = pb_draw(seed);
    let ids = sch.ids.clone();
    let honest: Vec<MemberId> = ids.iter().copied().filter(|m| !sch.faulty.contains_key(m)).collect();
    let cfg = SimConfig { localize: true, ..SimConfig::default() };
    let backdater: Option<(MemberId, u64)> = sch.faulty.iter().find_map(|(&m, f)| match f {
        PbFault::Backdate { frames } => Some((m, *frames)),
        _ => None,
    });
    // Everyone touches its own object every 0.5 s (a backdater does so by hand, backdated).
    let plans = |m: MemberId| {
        if backdater.is_some_and(|(b, _)| b == m) {
            BTreeMap::new()
        } else {
            plan(&(120..(sch.end_us / 1_000_000 * 60).saturating_sub(120)).step_by(30).map(|f| (f + m as u64, touch(10 + m, 2.0))).collect::<Vec<_>>())
        }
    };
    let ps: Vec<Peer> = ids
        .iter()
        .map(|&id| {
            let mut p = Peer::new(id, 0);
            p.join(1, &ids, SCENE, cfg, 100_000, plans(id));
            p.worlds.get_mut(&1).unwrap().adapt = true;
            p
        })
        .collect();
    let mut net = Net::new(Link { delay_us: 40_000, jitter_us: 5_000, loss_pct: 0 }, 41 ^ seed);
    net.default_cap = Some(Cap { rate_bps: 256_000, burst_bytes: 4000, max_queue_us: 500_000 });
    net.batching = true;
    for (&m, shape) in &sch.shapes {
        for &x in ids.iter().filter(|&&x| x != m) {
            match *shape {
                Shape::SlowUp { ms } => {
                    net.links.insert((m, x), Link { delay_us: ms * 1000, jitter_us: 5_000, loss_pct: 0 });
                }
                Shape::ThinDown { kbps, .. } => {
                    net.caps.insert((x, m), Cap { rate_bps: kbps * 1000, burst_bytes: 4000, max_queue_us: 500_000 });
                }
                Shape::Distant { ms } => {
                    for pair in [(m, x), (x, m)] {
                        net.links.insert(pair, Link { delay_us: ms * 1000, jitter_us: 5_000, loss_pct: 0 });
                    }
                }
            }
        }
    }
    let gst = sch.gst;
    let games: BTreeMap<MemberId, PbFault> = sch.faulty.clone();
    net.filter = Some(Box::new(move |t, _w, from, to, m| {
        if t >= gst {
            return Fate::Deliver;
        }
        match (games.get(&from), m) {
            (Some(PbFault::Slow { by_us }), _) => Fate::Delay(*by_us),
            (Some(PbFault::Split { late, by_us }), Msg::Entry(e)) if e.member == from && late.contains(&to) => Fate::Delay(*by_us),
            (Some(PbFault::Rotate), Msg::Entry(e)) if e.member == from => {
                let others: Vec<MemberId> = (1..=64).filter(|&x| x != from).collect();
                if others[(e.seq as usize) % 3] == to { Fate::Deliver } else { Fate::Drop }
            }
            _ => Fate::Deliver,
        }
    }));
    let mut sim = Sim::new(net, ps, sch.end_us);
    sim.feed = true;
    for (&m, f) in &sch.faulty {
        if matches!(f, PbFault::LyingFeeder) {
            sim.lying_feeders.insert(m);
        }
    }
    let heal: Vec<MemberId> = sch.shapes.iter().filter_map(|(&m, s)| matches!(s, Shape::ThinDown { heal: true, .. }).then_some(m)).collect();
    let mut paused_gst: BTreeMap<MemberId, u64> = BTreeMap::new();
    let mut lag_after = 0u64;
    let mut fed_ever: BTreeSet<MemberId> = BTreeSet::new();
    let panic_at = run_to_end(&mut sim, |s| {
        let now = s.net.now_us;
        if now == gst {
            for &m in &heal {
                s.net.caps.retain(|&(_, b), _| b != m);
            }
            for &m in &honest {
                paused_gst.insert(m, s.replica(m, 1).sim.stats.paused_frames);
            }
        }
        for &m in &ids {
            if s.is_fed(1, m) {
                fed_ever.insert(m);
            }
        }
        if let Some((b, frames)) = backdater {
            if now >= 2_000_000 && now % 500_000 == 0 && now + 1_000_000 < s.end_us {
                let e = sim_mut(s, b).author_backdated(Payload::Touch { obj: 10 + b, impulse: [0.0, 2.0, 0.0], grab: false }, frames);
                for &x in ids.iter().filter(|&&x| x != b) {
                    s.net.send(1, b, x, &Msg::Entry(e.clone()));
                }
            }
        }
        if std::env::var("PE_PB_TRACE").is_ok() && now >= gst && now % 1_000_000 == 0 {
            let r1 = &s.replica(ids[0], 1).sim;
            let r2 = &s.replica(ids[1], 1).sim;
            println!("TRACE t={} s: m{} held of m{}'s chain {:?} (m{} at {:?}); conf {} frame {} fed {}", now / 1_000_000, ids[0], ids[1], r1.held_of(ids[1]), ids[1], r2.held_of(ids[1]), r1.conf, r1.frame, s.is_fed(1, ids[0]));
        }
        if now >= gst + 2_000_000 {
            for &m in honest.iter().filter(|m| !sch.shapes.contains_key(m)) {
                let r = &s.replica(m, 1).sim;
                lag_after = lag_after.max(r.now.saturating_sub(r.conf));
            }
        }
    });
    let mut fails: Vec<String> = Vec::new();
    if let Some(t) = panic_at {
        fails.push(format!("PANIC at {t} ms"));
    }
    let desc = format!(
        "n={} shapes {:?} faulty {:?} gst {:.1} s",
        ids.len(),
        sch.shapes,
        sch.faulty.iter().map(|(m, f)| (*m, match f { PbFault::Split { late, .. } => format!("Split{late:?}"), f => format!("{f:?}") })).collect::<BTreeMap<_, _>>(),
        gst as f64 / 1e6
    );
    if panic_at.is_none() {
        let r1 = &sim.replica(honest[0], 1).sim;
        // S6: no honest member removed or voted against.
        for &h in &honest {
            if !r1.members.contains(&h) {
                fails.push(format!("S6: honest m{h} removed"));
            }
            let r = &sim.replica(h, 1).sim;
            if let Some(ev) = r.events.iter().find(|e| e.contains("vote to") && honest.iter().any(|&o| e.contains(&format!("member {o} ")))) {
                fails.push(format!("S6: m{h}: {}", &ev[..ev.len().min(110)]));
            }
        }
        // Every honest replica reaches the same verdicts (on the entries both hold).
        for &h in honest[1..].iter().filter(|&&h| !sim.is_fed(1, h)) {
            let r = &sim.replica(h, 1).sim;
            let differ = r1.verdicts.iter().filter(|(id, v)| r.verdicts.get(id).is_some_and(|w| w != *v)).count();
            if differ > 0 {
                fails.push(format!("m{} and m{h} differ on {differ} verdicts", honest[0]));
            }
        }
        // A timely honest entry is never void: every touch of an honest member without a shape, stamped before
        // the last 3 s, counts at every honest replica.
        let cutoff = (sch.end_us.saturating_sub(3_000_000)) * 60 / 1_000_000;
        let mut void_clean = 0;
        let mut void_shaped = 0;
        for e in r1.all_entries().into_iter().filter(|e| matches!(e.payload, Payload::Touch { .. }) && honest.contains(&e.member) && e.stamp < cutoff) {
            // Below each replica's own confirmed frame (rule 3: every entry there has its verdict); a member back
            // from the feed may still be catching up past it.
            if honest.iter().filter(|&&h| !sim.is_fed(1, h)).any(|&h| { let r = &sim.replica(h, 1).sim; e.stamp < r.conf && r.verdicts.get(&e.id()) != Some(&true) }) {
                if sch.shapes.contains_key(&e.member) {
                    void_shaped += 1;
                } else {
                    void_clean += 1;
                }
            }
        }
        if void_clean > 0 {
            fails.push(format!("{void_clean} touches of clean honest members don't count everywhere"));
        }
        // L1: after GST no clean honest member pauses.
        let paused: Vec<u64> = honest
            .iter()
            .filter(|m| !sch.shapes.contains_key(m))
            .map(|&m| sim.replica(m, 1).sim.stats.paused_frames - paused_gst.get(&m).copied().unwrap_or(0))
            .collect();
        if paused.iter().any(|&p| p > 0) {
            fails.push(format!("L1: clean honest members paused after GST {paused:?}"));
        }
        // Digests: every honest replica not on the feed at the end equals the offline replay (L3 for one that
        // left the feed).
        let fed_end: Vec<MemberId> = honest.iter().copied().filter(|&m| sim.is_fed(1, m)).collect();
        let cmp: Vec<MemberId> = honest.iter().copied().filter(|m| !fed_end.contains(m)).collect();
        // Confirmed frames only: a member back from the feed re-simulates as its log arrives (L3), and the spike
        // recovers by replaying the log rather than by 9.5's state transfer, so it can still be catching up.
        let frames = cmp.iter().map(|&m| sim.replica(m, 1).sim.conf).min().unwrap_or(0).saturating_sub(1);
        let src = *cmp.iter().max_by_key(|&&m| sim.replica(m, 1).sim.conf).unwrap();
        let first = &sim.replica(src, 1).sim;
        let members: Vec<MemberId> = first.members.iter().copied().collect();
        let rd = digest(&replay_sim_like(first, SCENE, &members, &final_log(first), frames).hashes, frames);
        let bad: Vec<MemberId> = cmp.iter().copied().filter(|&m| digest(&sim.replica(m, 1).sim.hashes, frames) != rd).collect();
        if !bad.is_empty() {
            fails.push(format!("DIFFER from the offline replay: {bad:?}"));
        }
        let checks_bad: u64 = honest.iter().map(|&m| sim.replica(m, 1).sim.stats.checks_bad).sum();
        if checks_bad > 0 {
            fails.push(format!("{checks_bad} false checks"));
        }
        if !fails.is_empty() && std::env::var("PE_PB_DEBUG").is_ok() {
            for &h in &honest {
                let r = &sim.replica(h, 1).sim;
                println!("DEBUG m{h}: conf {} frame {} verdicts {} windows {:?} fed {}", r.conf, r.frame, r.verdicts.len(), r.window_values().keys().next_back(), sim.is_fed(1, h));
                println!("DEBUG   pending: {}", r.debug_pending());
            }
            // The first entry whose verdict differs between the first honest replica and another, with both
            // replicas' window values for its window.
            for &h in honest[1..].iter().filter(|&&h| !sim.is_fed(1, h)) {
                let r = &sim.replica(h, 1).sim;
                if let Some((id, v)) = r1.verdicts.iter().find(|(id, v)| r.verdicts.get(id).is_some_and(|w| w != *v)) {
                    let e = r1.all_entries().into_iter().find(|e| e.id() == *id);
                    if let Some(e) = e {
                        let k = e.stamp / 300;
                        println!("DEBUG m{} s{} stamp {} kind {:?}: verdict at m{} {} at m{h} {:?}", e.member, e.seq, e.stamp, std::mem::discriminant(&e.payload), honest[0], v, r.verdicts.get(id));
                        for (who, rr) in [(honest[0], r1), (h, r)] {
                            let wv = rr.window_values().get(&k);
                            println!("DEBUG   m{who} window {k}: {:?}", wv.map(|w| (w.q, w.e, w.c_max, w.l.clone())));
                        }
                    }
                }
            }
        }
        if !fails.is_empty() {
            // Diagnostics: entries each honest replica rejected as horizon violations, and the first few.
            let viol: Vec<(MemberId, u64)> = honest.iter().map(|&m| (m, sim.replica(m, 1).sim.stats.entries_rejected_violation)).collect();
            fails.push(format!("violations per honest replica {viol:?}"));
            for &m in &honest {
                if let Some(ev) = sim.replica(m, 1).sim.events.iter().find(|e| e.contains("violation:")) {
                    fails.push(format!("m{m} first: {}", &ev[..ev.len().min(140)]));
                }
            }
        }
        let left: Vec<MemberId> = fed_ever.iter().copied().filter(|m| !fed_end.contains(m)).collect();
        let catch: Vec<String> = left.iter().map(|&m| { let r = &sim.replica(m, 1).sim; format!("m{m} conf {} of frame {}", r.conf, r.frame) }).collect();
        let line = format!(
            "seed {seed} {desc}: fed {:?} (left {:?}: {}); digests over frames 0..{frames}; slow-honest touches not counting {void_shaped}; worst clean lag after GST + 2 s {lag_after} frames -> {}",
            fed_ever,
            left,
            catch.join(", "),
            if fails.is_empty() { "OK".to_string() } else { format!("FAIL: {}", fails.join("; ")) }
        );
        return (fails.is_empty(), line);
    }
    (false, format!("seed {seed} {desc} -> FAIL: {}", fails.join("; ")))
}

/// `partbrand [--seeds N] [--from S]`: Part B's randomized schedules that remove no one.
pub fn partbrand(args: &[String]) {
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u64>().ok());
    let n = get("--seeds").unwrap_or(12);
    let from = get("--from").unwrap_or(1);
    let mut ok = 0;
    for seed in from..from + n {
        let (good, line) = pb_run(seed);
        println!("RESULT partbrand {line}");
        if good {
            ok += 1;
        }
    }
    println!("RESULT partbrand: {ok} of {n} seeds OK -> {}", if ok == n { "ALL EQUAL, NO FALSE CHECKS" } else { "FAIL" });
}
