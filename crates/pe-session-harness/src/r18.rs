//! R18 (recovery from collapse): the ball pit through a capped link, as in @ballpit-viz's matrix
//! cells (50 ms one-way, token-bucket cap, 500 ms router queue), with the round-2 protocol and
//! three transports: one datagram per message (3d0f98a), per-tick batching, and batching plus the
//! congestion controller (thinner hand entries and acks while the uplink queue grows).
//!
//! Round 3 (review R3-9): `--members N` (member 1 sweeps, the others watch), `--shared` (each
//! member's uplink cap is split evenly over its N-1 peers, as one home connection), `--end S`.

use std::collections::BTreeMap;

use pe_session_sim::*;
use pe_session::session::*;

pub struct Opts {
    pub kbps: u64,
    pub members: u32,
    pub shared: bool,
    pub end_s: u64,
    /// One member on a worse link: (member, kbit/s for every link to and from it).
    pub slow: Option<(MemberId, u64)>,
    /// The cap drops to this many kbit/s at this time (ms): (ms, kbit/s).
    pub step: Option<(u64, u64)>,
}

pub fn capped(o: &Opts) {
    let hz = 60;
    let (w0, w1) = (100_000 + 2_000_000, 100_000 + 5_660_000);
    let n = o.members as usize;
    let div = if o.shared { (o.members - 1).max(1) as u64 } else { 1 };
    println!(
        "RESULT r18 capped: 500-ball pit, {n} members, member 1 sweeps, 50+-5 ms one-way, cap {} kbit/s {} (4 kB burst, 500 ms queue, tail drop), 60 Hz, dead reckoning on, {} s",
        o.kbps,
        if o.shared { "per uplink, shared by its peers" } else { "per directed link" },
        o.end_s
    );
    if let Some((sm, sk)) = o.slow {
        println!("RESULT r18 capped: member {sm} is on a slow link: {sk} kbit/s to and from every peer");
    }
    if let Some((ms, sk)) = o.step {
        println!("RESULT r18 capped: at t = {ms} ms every cap drops to {sk} kbit/s");
    }
    println!("RESULT   transport | wire kbit/s per member | max queue ms per member | cap drops | stalled frames per member | max conf lag per member | author rollbacks | observer rollbacks/s, max depth | back to real time after the gesture | end state");
    for (label, batching, adapt) in [("per message", false, false), ("batched", true, false), ("batched+adaptive", true, true)] {
        let cfg = SimConfig { dt: 1.0 / hz as f32, window: 2 * hz, hand_predict: 6, ..SimConfig::default() };
        let scene = SceneKind::BallPit { balls: 500, hands: 2 };
        let ids: Vec<MemberId> = (1..=o.members).collect();
        let peers: Vec<Peer> = ids
            .iter()
            .map(|&id| {
                let mut p = Peer::new(id, [1000i64, -1000, 400, -400][(id as usize - 1) % 4]);
                let pl = if id == 1 { crate::history::sweep(hz, 500) } else { BTreeMap::new() };
                p.join(1, &ids, scene, cfg, 100_000, pl);
                p.worlds.get_mut(&1).unwrap().adapt = adapt;
                p
            })
            .collect();
        let mut net = Net::new(Link { delay_us: 50_000, jitter_us: 5_000, loss_pct: 0 }, 17);
        net.default_cap = Some(Cap { rate_bps: o.kbps * 1000 / div, burst_bytes: 4000 / div, max_queue_us: 500_000 });
        if let Some((sm, sk)) = o.slow {
            for &x in &ids {
                if x != sm {
                    for pair in [(sm, x), (x, sm)] {
                        net.caps.insert(pair, Cap { rate_bps: sk * 1000, burst_bytes: 4000, max_queue_us: 500_000 });
                    }
                }
            }
        }
        net.batching = batching;
        net.count_from_us = w0;
        net.count_to_us = w1;
        let mut sim = Sim::new(net, peers, o.end_s * 1_000_000);
        let mut qmax = vec![0u64; n];
        let mut lag_max = vec![0u64; n];
        let mut back: Option<u64> = None;
        let (mut rb0, mut rb1) = (0u64, 0u64);
        while sim.net.now_us <= sim.end_us {
            if let Some((ms, sk)) = o.step {
                if sim.net.now_us == ms * 1000 {
                    sim.net.default_cap = Some(Cap { rate_bps: sk * 1000 / div, burst_bytes: 4000 / div, max_queue_us: 500_000 });
                }
            }
            sim.step_ms();
            let now = sim.net.now_us;
            let mut all_lag_ok = true;
            let mut q_now = 0u64;
            // Real time is judged over the members still in the session (a demoted one follows).
            let live: Vec<MemberId> = sim.replica(1, 1).sim.vcm.members.iter().copied().collect();
            for (i, &m) in ids.iter().enumerate() {
                for &p in &ids {
                    if p != m {
                        let b = sim.net.backlog_us(m, p);
                        qmax[i] = qmax[i].max(b);
                        if live.contains(&m) && live.contains(&p) {
                            q_now = q_now.max(b);
                        }
                    }
                }
                let r = &sim.replica(m, 1).sim;
                let l = r.now.saturating_sub(r.conf);
                lag_max[i] = lag_max[i].max(l);
                all_lag_ok &= l < 20 || !live.contains(&m);
            }
            if now == w0 {
                rb0 = sim.replica(2, 1).sim.stats.rollbacks;
            }
            if now == w1 {
                rb1 = sim.replica(2, 1).sim.stats.rollbacks;
            }
            // Back to real time: every member within 20 frames of confirmation, queues under 50 ms.
            if now >= w1 && back.is_none() && all_lag_ok && q_now < 50_000 {
                back = Some(now - w1);
            }
            sim.net.now_us += 1000;
        }
        let secs = (w1 - w0) as f64 / 1e6;
        let kbps = |m: MemberId| sim.net.wire_from.get(&m).copied().unwrap_or(0) as f64 * 8.0 / 1000.0 / secs;
        // A member demoted (removed) from the session no longer counts: compare the others.
        let live: Vec<MemberId> = ids.iter().copied().filter(|m| sim.replica(1, 1).sim.vcm.members.contains(m)).collect();
        let last = live.iter().map(|&m| sim.replica(m, 1).sim.frame).min().unwrap().saturating_sub(1);
        let hs: Vec<u64> = live.iter().map(|&m| digest(&sim.replica(m, 1).sim.hashes, last)).collect();
        let r1 = &sim.replica(1, 1).sim;
        let members: Vec<MemberId> = r1.vcm.members.iter().copied().collect();
        let rd = digest(&replay_sim_with(1, scene, r1.cfg, r1.public_keys(), &members, &r1.peers_of, &final_log(r1), last).hashes, last);
        let restamped = r1.all_entries().iter().filter(|e| e.member == 1 && r1.effective_frame(e.id()).is_some_and(|f| f != e.stamp)).count();
        let join = |v: Vec<String>| v.join(" / ");
        let s2 = sim.replica(2, 1).sim.stats;
        println!(
            "RESULT   {label:<16} | {} | {} | {:4} | {} | {} | {} (own entries moved: {restamped}) | {:4.1}/s, {:3} | {} | {} replay {rd:016x} {}",
            join(ids.iter().map(|&m| format!("{:5.1}", kbps(m))).collect()),
            join(qmax.iter().map(|q| format!("{:4}", q / 1000)).collect()),
            sim.net.cap_dropped,
            join(ids.iter().map(|&m| format!("{:3}", sim.replica(m, 1).sim.stats.paused_frames)).collect()),
            join(lag_max.iter().map(|l| format!("{l:3}")).collect()),
            r1.stats.rollbacks,
            (rb1 - rb0) as f64 / secs,
            s2.max_depth,
            back.map_or("never".to_string(), |t| format!("{} ms", t / 1000)),
            join(live.iter().zip(&hs).map(|(m, h)| format!("m{m} {h:016x}")).collect()),
            if hs.iter().all(|h| *h == rd) { "EQUAL" } else { "DIFFER" }
        );
        let ev = sim.replica(1, 1).sim.members_evicted().clone();
        if !ev.is_empty() {
            println!("RESULT     removed from the session (cut, from frame): {ev:?}");
        }
        if adapt {
            let l = sim.replica(1, 1).level_ms;
            println!(
                "RESULT     controller on member 1: time at each level (hand stride {:?}, acks every {:?} frames): {:?} ms",
                HAND_STRIDE, ACK_FRAMES, l
            );
        }
    }
}
