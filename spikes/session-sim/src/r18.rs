//! R18 (recovery from collapse): the ball pit through a capped link, as in @ballpit-viz's matrix
//! cells (50 ms one-way, token-bucket cap, 500 ms router queue), with the round-2 protocol and
//! three transports: one datagram per message (3d0f98a), per-tick batching, and batching plus the
//! congestion controller (thinner hand entries and acks while the uplink queue grows).

use std::collections::BTreeMap;

use pe_session_sim::*;
use pe_sim_core::session::*;

pub fn capped(rate_kbps: u64) {
    let hz = 60;
    let (w0, w1) = (100_000 + 2_000_000, 100_000 + 5_660_000);
    println!(
        "RESULT r18 capped: 500-ball pit, member 1 sweeps, 50+-5 ms one-way, cap {rate_kbps} kbit/s each way (4 kB burst, 500 ms queue, tail drop), 60 Hz, dead reckoning on"
    );
    println!("RESULT   transport | wire kbit/s m1->m2 / m2->m1 | max queue ms m1->m2 / m2->m1 | cap drops | stalled frames m1 / m2 | max conf lag m1 / m2 | author rollbacks | observer rollbacks/s, max depth | back to real time after the gesture | end state");
    for (label, batching, adapt) in [("per message", false, false), ("batched", true, false), ("batched+adaptive", true, true)] {
        let cfg = SimConfig { dt: 1.0 / hz as f32, window: 2 * hz, hand_predict: 6, ..SimConfig::default() };
        let scene = SceneKind::BallPit { balls: 500, hands: 2 };
        let ids = [1, 2];
        let peers: Vec<Peer> = ids
            .iter()
            .map(|&id| {
                let mut p = Peer::new(id, if id == 1 { 1000 } else { -1000 });
                let pl = if id == 1 { crate::history::sweep(hz, 500) } else { BTreeMap::new() };
                p.join(1, &ids, scene, cfg, 100_000, pl);
                p.worlds.get_mut(&1).unwrap().adapt = adapt;
                p
            })
            .collect();
        let mut net = Net::new(Link { delay_us: 50_000, jitter_us: 5_000, loss_pct: 0 }, 17);
        net.default_cap = Some(Cap { rate_bps: rate_kbps * 1000, burst_bytes: 4000, max_queue_us: 500_000 });
        net.batching = batching;
        net.count_from_us = w0;
        net.count_to_us = w1;
        let mut sim = Sim::new(net, peers, 10_000_000);
        let (mut q12, mut q21, mut lag1, mut lag2) = (0u64, 0u64, 0u64, 0u64);
        let mut back: Option<u64> = None;
        let (mut rb0, mut rb1) = (0u64, 0u64);
        while sim.net.now_us <= sim.end_us {
            sim.step_ms();
            let now = sim.net.now_us;
            let (b12, b21) = (sim.net.backlog_us(1, 2), sim.net.backlog_us(2, 1));
            q12 = q12.max(b12);
            q21 = q21.max(b21);
            let l1 = { let r = &sim.replica(1, 1).sim; r.now.saturating_sub(r.conf) };
            let l2 = { let r = &sim.replica(2, 1).sim; r.now.saturating_sub(r.conf) };
            lag1 = lag1.max(l1);
            lag2 = lag2.max(l2);
            if now == w0 {
                rb0 = sim.replica(2, 1).sim.stats.rollbacks;
            }
            if now == w1 {
                rb1 = sim.replica(2, 1).sim.stats.rollbacks;
            }
            // Back to real time: both members within 20 frames of confirmation, queues under 50 ms.
            if now >= w1 && back.is_none() && l1 < 20 && l2 < 20 && b12 < 50_000 && b21 < 50_000 {
                back = Some(now - w1);
            }
            sim.net.now_us += 1000;
        }
        let secs = (w1 - w0) as f64 / 1e6;
        let kbps = |m: MemberId| sim.net.wire_from.get(&m).copied().unwrap_or(0) as f64 * 8.0 / 1000.0 / secs;
        let (s1, s2) = (sim.replica(1, 1).sim.stats, sim.replica(2, 1).sim.stats);
        let last = sim.replica(1, 1).sim.frame.min(sim.replica(2, 1).sim.frame).saturating_sub(1);
        let (h1, h2) = (digest(&sim.replica(1, 1).sim.hashes, last), digest(&sim.replica(2, 1).sim.hashes, last));
        let r1 = &sim.replica(1, 1).sim;
        let members: Vec<MemberId> = r1.members.iter().copied().collect();
        let rd = digest(&replay_sim_with(1, scene, r1.cfg, &members, &r1.peers_of, &final_log(r1), last).hashes, last);
        let restamped = r1.all_entries().iter().filter(|e| e.member == 1 && r1.effective_frame(e.id()).is_some_and(|f| f != e.stamp)).count();
        println!(
            "RESULT   {label:<16} | {:5.1} / {:5.1} | {:4} / {:4} | {:4} | {:3} / {:3} | {:3} / {:3} | {} (own entries moved: {restamped}) | {:4.1}/s, {:3} | {} | m1 {h1:016x} m2 {h2:016x} replay {rd:016x} {}",
            kbps(1),
            kbps(2),
            q12 / 1000,
            q21 / 1000,
            sim.net.cap_dropped,
            s1.paused_frames,
            s2.paused_frames,
            lag1,
            lag2,
            s1.rollbacks,
            (rb1 - rb0) as f64 / secs,
            s2.max_depth,
            back.map_or("never".to_string(), |t| format!("{} ms", t / 1000)),
            if h1 == h2 && h1 == rd { "EQUAL" } else { "DIFFER" }
        );
        if adapt {
            let l = sim.replica(1, 1).level_ms;
            println!("RESULT     controller on member 1: time at level 0/1/2/3 (hand stride 1/2/3/4, acks every 2/3/4/6 frames): {} / {} / {} / {} ms", l[0], l[1], l[2], l[3]);
        }
    }
}
