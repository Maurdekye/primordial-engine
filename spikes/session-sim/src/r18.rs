//! R18 (recovery from collapse): the ball pit through a capped link, as in @ballpit-viz's matrix
//! cells (50 ms one-way, token-bucket cap, 500 ms router queue), with the round-2 protocol and
//! three transports: one datagram per message (3d0f98a), per-tick batching, and batching plus the
//! congestion controller (thinner hand entries and acks while the uplink queue grows).
//!
//! Round 3 (review R3-9): `--members N` (member 1 sweeps, the others watch), `--shared` (each
//! member's uplink cap is split evenly over its N-1 peers, as one home connection), `--end S`.

use std::collections::BTreeMap;

use pe_session_sim::*;
use pe_sim_core::session::*;

pub struct Opts {
    pub kbps: u64,
    pub members: u32,
    pub shared: bool,
    pub end_s: u64,
    /// One member on a worse link: (member, kbit/s for every link to and from it).
    pub slow: Option<(MemberId, u64)>,
    /// The cap drops to this many kbit/s at this time (ms): (ms, kbit/s).
    pub step: Option<(u64, u64)>,
    /// A second gesture this many seconds after the first ends (adaptive transport only).
    pub gap_s: Option<u64>,
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
    // A second gesture this many seconds after the first ends, for the first-gesture-after-idle cells.
    let shift_frames = o.gap_s.map(|g| (340 - 120) + g * hz);
    let transports: Vec<(&str, bool, bool)> = if o.gap_s.is_some() {
        vec![("batched+adaptive", true, true)]
    } else {
        vec![("per message", false, false), ("batched", true, false), ("batched+adaptive", true, true)]
    };
    // Member 1's targets by frame (its hand poses are stamped at their clock frame), for its own-hand
    // error: a kinematic hand reaches its frame's target within the frame, so after frame f it should be
    // at target(f) (D73(a) makes this exact even when R18 thins the stream).
    let mut plan1 = crate::history::sweep(hz, 500);
    if let Some(sh) = shift_frames {
        for (fr, acts) in crate::history::sweep(hz, 500) {
            plan1.entry(fr + sh).or_default().extend(acts);
        }
    }
    let target: BTreeMap<u64, [f64; 3]> = plan1
        .iter()
        .filter_map(|(&f, v)| {
            v.iter().find_map(|a| match a {
                Action::Hand { target_mm, active: true, .. } => Some((f, target_mm.map(|c| c as f64 * 0.001))),
                _ => None,
            })
        })
        .collect();
    for (label, batching, adapt) in transports {
        let cfg = SimConfig { dt: 1.0 / hz as f32, window: 2 * hz, hand_predict: 6, ..SimConfig::default() };
        let scene = SceneKind::BallPit { balls: 500, hands: 2 };
        let ids: Vec<MemberId> = (1..=o.members).collect();
        let peers: Vec<Peer> = ids
            .iter()
            .map(|&id| {
                let mut p = Peer::new(id, [1000i64, -1000, 400, -400][(id as usize - 1) % 4]);
                let pl = if id == 1 { plan1.clone() } else { BTreeMap::new() };
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
        // When each frame became confirmed at each member (wall clock, us), for the hand entries'
        // confirmation latency below.
        let mut conf_at: Vec<Vec<u64>> = vec![Vec::new(); n];
        let mut back: Option<u64> = None;
        // Member 1's own hand against its target, the first time each sweep frame is simulated (mm).
        let mut own_err: BTreeMap<u64, f64> = BTreeMap::new();
        let (mut rb0, mut rb1) = (0u64, 0u64);
        // The second gesture's window, its queue peak (to 10 s after it), its stalls and recovery.
        let sh_us = shift_frames.map_or(0, |s| s * 1_000_000 / hz);
        let (w2, w3) = (w0 + sh_us, w1 + sh_us);
        let mut q2 = vec![0u64; n];
        let mut paused2 = vec![0u64; n];
        let mut back2: Option<u64> = None;
        let mut level2: Option<usize> = None;
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
            let live: Vec<MemberId> = sim.replica(1, 1).sim.members.iter().copied().collect();
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
                while (conf_at[i].len() as u64) < r.conf.min(r.now + 1) {
                    conf_at[i].push(now);
                }
                all_lag_ok &= l < 20 || !live.contains(&m);
            }
            {
                let r1 = &sim.replica(1, 1).sim;
                let g = r1.frame.saturating_sub(1);
                if let (Some(t), Some(st)) = (target.get(&g), r1.obj_state(500)) {
                    let q = st.pos();
                    let d = [q.x as f64 - t[0], q.y as f64 - t[1], q.z as f64 - t[2]];
                    own_err.entry(g).or_insert_with(|| (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() * 1000.0);
                }
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
            if shift_frames.is_some() {
                if now >= w2 && level2.is_none() {
                    level2 = Some(sim.replica(1, 1).level);
                    for (i, &m) in ids.iter().enumerate() {
                        paused2[i] = sim.replica(m, 1).sim.stats.paused_frames;
                    }
                }
                if now >= w2 && now < w3 + 10_000_000 {
                    for (i, &m) in ids.iter().enumerate() {
                        for &p in &ids {
                            if p != m {
                                q2[i] = q2[i].max(sim.net.backlog_us(m, p));
                            }
                        }
                    }
                }
                if now >= w3 && back2.is_none() && all_lag_ok && q_now < 50_000 {
                    back2 = Some(now - w3);
                }
            }
            sim.net.now_us += 1000;
        }
        let secs = (w1 - w0) as f64 / 1e6;
        let kbps = |m: MemberId| sim.net.wire_from.get(&m).copied().unwrap_or(0) as f64 * 8.0 / 1000.0 / secs;
        // A member demoted (removed) from the session no longer counts: compare the others.
        let live: Vec<MemberId> = ids.iter().copied().filter(|m| sim.replica(1, 1).sim.members.contains(m)).collect();
        let last = live.iter().map(|&m| sim.replica(m, 1).sim.frame).min().unwrap().saturating_sub(1);
        let hs: Vec<u64> = live.iter().map(|&m| digest(&sim.replica(m, 1).sim.hashes, last)).collect();
        let r1 = &sim.replica(1, 1).sim;
        // The offline replay is built from a live member's log, and keeps that replica's hand owners, as
        // the suite's check does: a demoted member's log stops at its removal, and its hand entries
        // before it still count (D73 made member 1 removable here, on the shared uplink).
        let rl = &sim.replica(live[0], 1).sim;
        let members: Vec<MemberId> = rl.members.iter().copied().collect();
        let rd = digest(&replay_sim_like(rl, scene, &members, &final_log(rl), last).hashes, last);
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
        // Each of member 1's hand entries, from the frame it is stamped at until each member confirms it
        // (design 7.6: unchanged by idle thinning while anything happens). In frames, max / mean.
        let dt_us = 1_000_000 / hz;
        let stamps: Vec<u64> = r1.all_entries().iter().filter(|e| e.member == 1 && matches!(e.payload, Payload::Hand { .. })).map(|e| e.stamp).collect();
        let lat: Vec<String> = conf_at
            .iter()
            .zip(&ids)
            .map(|(c, m)| {
                let l: Vec<f64> = stamps.iter().filter(|&&s| (s as usize) < c.len()).map(|&s| (c[s as usize] as f64 - (100_000 + s * dt_us) as f64) / dt_us as f64).collect();
                let open = stamps.len() - l.len();
                let max = l.iter().cloned().fold(0.0, f64::max);
                let mean = l.iter().sum::<f64>() / l.len().max(1) as f64;
                match open {
                    _ if l.is_empty() => format!("m{m} none of {} confirmed", stamps.len()),
                    0 => format!("m{m} {max:.1} / {mean:.1}"),
                    _ => format!("m{m} {max:.1} / {mean:.1} ({open} of {} unconfirmed)", stamps.len()),
                }
            })
            .collect();
        println!("RESULT     hand entries' confirmation latency, frames (max / mean): {}", lat.join(", "));
        println!(
            "RESULT     member 1's own hand against its target, over {} gesture frames: mean {:.2} mm, max {:.2} mm",
            own_err.len(),
            own_err.values().sum::<f64>() / own_err.len().max(1) as f64,
            own_err.values().cloned().fold(0.0, f64::max)
        );
        if let Some(g) = o.gap_s {
            let s0b = 120 + shift_frames.unwrap();
            let lat2: Vec<String> = conf_at
                .iter()
                .zip(&ids)
                .map(|(c, m)| {
                    let l: Vec<f64> = stamps.iter().filter(|&&s| s >= s0b && (s as usize) < c.len()).map(|&s| (c[s as usize] as f64 - (100_000 + s * dt_us) as f64) / dt_us as f64).collect();
                    let max = l.iter().cloned().fold(0.0, f64::max);
                    format!("m{m} {max:.1} / {:.1}", l.iter().sum::<f64>() / l.len().max(1) as f64)
                })
                .collect();
            println!(
                "RESULT     second gesture ({g} s after the first): level at its start {:?}; max queue ms per member {}; stalled frames during it {}; back to real time after it {}; hand entries' latency (max / mean) {}",
                level2,
                join(q2.iter().map(|q| format!("{}", q / 1000)).collect()),
                join(ids.iter().enumerate().map(|(i, &m)| format!("{}", sim.replica(m, 1).sim.stats.paused_frames - paused2[i])).collect()),
                back2.map_or("never".to_string(), |t| format!("{} ms", t / 1000)),
                lat2.join(", ")
            );
        }
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
