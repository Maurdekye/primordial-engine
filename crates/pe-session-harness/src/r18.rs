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
    /// A second gesture this many seconds after the first ends (adaptive transport only; spike 867495a).
    pub gap_s: Option<u64>,
    /// With `gap_s`, how many gestures in all, each `gap_s` after the one before (default 2; spike
    /// fb12cb4, review PA-2).
    pub gestures: u32,
    /// Only the adaptive transport (the thin-link cells), not the per-message and batched ones (spike 11c20c1).
    pub adaptive_only: bool,
    /// Also print each member's offered bytes in the gesture window by message kind.
    pub by_kind: bool,
    /// [`crate::Net::what_if`] for every run (measurement only).
    pub what_if: u8,
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
    // Member 1's targets by frame (its hand poses are stamped at their clock frame), for its own-hand
    // error: a kinematic hand reaches its frame's target within the frame, so after frame f it
    // should be at target(f) (D73(a): exact even when R18 thins the stream).
    // A second gesture this many seconds after the first ends, for the first-gesture-after-idle cells.
    let shift_frames = o.gap_s.map(|g| (340 - 120) + g * hz);
    // With `gestures`, more of them, each as long after the one before (thin links, review PA-2).
    let n_gest = if o.gap_s.is_some() { o.gestures.max(2) as u64 } else { 1 };
    let transports: Vec<(&str, bool, bool)> = if o.gap_s.is_some() || o.adaptive_only {
        vec![("batched+adaptive", true, true)]
    } else {
        vec![("per message", false, false), ("batched", true, false), ("batched+adaptive", true, true)]
    };
    let mut plan1 = crate::history::sweep(hz, 500);
    if let Some(sh) = shift_frames {
        for k in 1..n_gest {
            for (fr, acts) in crate::history::sweep(hz, 500) {
                plan1.entry(fr + k * sh).or_default().extend(acts);
            }
        }
    }
    let target: BTreeMap<u64, [f64; 3]> = plan1
        .iter()
        .filter_map(|(&f, v)| v.iter().find_map(|a| match a {
            Action::Hand { target_mm, active: true, .. } => Some((f, target_mm.map(|c| c as f64 * 0.001))),
            _ => None,
        }))
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
        net.what_if = o.what_if;
        net.count_from_us = w0;
        net.count_to_us = w1;
        let mut sim = Sim::new(net, peers, o.end_s * 1_000_000);
        let mut qmax = vec![0u64; n];
        let mut lag_max = vec![0u64; n];
        let mut back: Option<u64> = None;
        // Each later gesture's window, its queue peak (to 10 s after it), its stalls and recovery.
        let sh_us = shift_frames.map_or(0, |s| s * 1_000_000 / hz);
        let later: Vec<(u64, u64)> = (1..n_gest).map(|k| (w0 + k * sh_us, w1 + k * sh_us)).collect();
        let mut q2 = vec![vec![0u64; n]; later.len()];
        let mut paused2 = vec![vec![0u64; n]; later.len()];
        let mut back2: Vec<Option<u64>> = vec![None; later.len()];
        let mut level2: Vec<Option<usize>> = vec![None; later.len()];
        // Thin links: each member's mean confirmation lag (frames, its present minus its confirmed
        // frame) from the gesture's start to the end of the run (spike 11c20c1).
        let mut lag_sum = vec![0u64; n];
        let mut lag_n = 0u64;
        // When each frame became confirmed at each member (wall clock, us), for the hand entries'
        // confirmation latency below (spike 5ad7546).
        let mut conf_at: Vec<Vec<u64>> = vec![Vec::new(); n];
        // Member 1's own hand against its target, first time each sweep frame is simulated (mm).
        let mut own_err: BTreeMap<u64, f64> = BTreeMap::new();
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
                if now >= w0 {
                    lag_sum[i] += l;
                }
                while (conf_at[i].len() as u64) < r.conf.min(r.now + 1) {
                    conf_at[i].push(now);
                }
                all_lag_ok &= l < 20 || !live.contains(&m);
            }
            {
                let r1 = &sim.replica(1, 1).sim;
                let g = r1.frame.saturating_sub(1);
                if let (Some(t), Some(st)) = (target.get(&g), r1.obj_state(500)) {
                    let p = st.pos();
                    own_err.entry(g).or_insert_with(|| ((p[0] - t[0]).powi(2) + (p[1] - t[1]).powi(2) + (p[2] - t[2]).powi(2)).sqrt() * 1000.0);
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
            for (j, &(w2, w3)) in later.iter().enumerate() {
                if now >= w2 && level2[j].is_none() {
                    level2[j] = Some(sim.replica(1, 1).level);
                    for (i, &m) in ids.iter().enumerate() {
                        paused2[j][i] = sim.replica(m, 1).sim.stats.paused_frames;
                    }
                }
                if now >= w2 && now < w3 + 10_000_000 {
                    for (i, &m) in ids.iter().enumerate() {
                        for &p in &ids {
                            if p != m {
                                q2[j][i] = q2[j][i].max(sim.net.backlog_us(m, p));
                            }
                        }
                    }
                }
                if now >= w3 && back2[j].is_none() && all_lag_ok && q_now < 50_000 {
                    back2[j] = Some(now - w3);
                }
            }
            if now >= w0 {
                lag_n += 1;
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
        // The offline replay is built from a live member's log, as the spike's (11c20c1): a demoted
        // member's log stops at its removal (a thin member 1 can be demoted). While member 1 is live,
        // this is member 1's, as before.
        let rl = &sim.replica(live[0], 1).sim;
        let members: Vec<MemberId> = rl.vcm.members.iter().copied().collect();
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
        // A DIFFER at the run's end compares present frames, which an unconverged run has not settled.
        // Then check what is confirmed: each live replica's confirmed frames against an offline replay of
        // its own final log, and the frames every live replica has confirmed, across replicas. Any
        // mismatch there is a desync.
        if !hs.iter().all(|h| *h == rd) {
            let confs: Vec<u64> = live.iter().map(|&m| sim.replica(m, 1).sim.conf.min(sim.replica(m, 1).sim.frame)).collect();
            let own: Vec<String> = live
                .iter()
                .zip(&confs)
                .map(|(&m, &c)| {
                    let r = &sim.replica(m, 1).sim;
                    let members: Vec<MemberId> = r.vcm.members.iter().copied().collect();
                    let rep = replay_sim_like(r, scene, &members, &final_log(r), c);
                    let ok = digest(&r.hashes, c) == digest(&rep.hashes, c);
                    format!("m{m} {c} {}", if ok { "EQUAL" } else { "DESYNC" })
                })
                .collect();
            let common = confs.iter().copied().min().unwrap_or(0);
            let ch: Vec<u64> = live.iter().map(|&m| digest(&sim.replica(m, 1).sim.hashes, common)).collect();
            println!(
                "RESULT     confirmed prefixes (end state DIFFER): each live replica's confirmed frames against a replay of its own log: {}; the {common} frames all have confirmed, across replicas: {}",
                own.join(", "),
                if ch.iter().all(|h| *h == ch[0]) { "EQUAL" } else { "DESYNC" }
            );
        }
        // Each of member 1's hand entries, from the frame it is stamped at until each member confirms it
        // (design 7.6: unchanged by idle thinning while anything happens). In frames, max / mean.
        let dt_us = 1_000_000 / hz;
        let stamps: Vec<u64> = r1.all_entries().iter().filter(|e| e.member == 1 && matches!(e.payload, Payload::Hand { .. })).map(|e| e.stamp).collect();
        // The first gesture's entries, or gesture k's (the gap cells: from its first frame to the next
        // gesture's), per member.
        let s0b = shift_frames.map_or(u64::MAX, |sh| 120 + sh);
        let lat_in = |from: u64, to: u64| -> Vec<String> {
            let st: Vec<u64> = stamps.iter().copied().filter(|&s| s >= from && s < to).collect();
            conf_at
                .iter()
                .zip(&ids)
                .map(|(c, m)| {
                    let l: Vec<f64> = st.iter().filter(|&&s| (s as usize) < c.len()).map(|&s| (c[s as usize] as f64 - (100_000 + s * dt_us) as f64) / dt_us as f64).collect();
                    let open = st.len() - l.len();
                    let max = l.iter().cloned().fold(0.0, f64::max);
                    let mean = l.iter().sum::<f64>() / l.len().max(1) as f64;
                    match open {
                        _ if l.is_empty() => format!("m{m} none of {} confirmed", st.len()),
                        0 => format!("m{m} {max:.1} / {mean:.1}"),
                        _ => format!("m{m} {max:.1} / {mean:.1} ({open} of {} unconfirmed)", st.len()),
                    }
                })
                .collect()
        };
        let lat = lat_in(0, s0b);
        println!("RESULT     hand entries' confirmation latency, frames (max / mean): {}", lat.join(", "));
        if let (Some(g), Some(sh)) = (o.gap_s, shift_frames) {
            for (j, k) in (1..n_gest).enumerate() {
                let (from, to) = (120 + k * sh, if k + 1 < n_gest { 120 + (k + 1) * sh } else { u64::MAX });
                let fields = format!(
                    "level at its start {}; max queue ms per member {}; stalled frames during it {}; back to real time after it {}; hand entries' latency (max / mean) {}",
                    level2[j].map_or("-".to_string(), |l| l.to_string()),
                    join(q2[j].iter().map(|q| format!("{}", q / 1000)).collect()),
                    join(ids.iter().enumerate().map(|(i, &m)| format!("{}", sim.replica(m, 1).sim.stats.paused_frames - paused2[j][i])).collect()),
                    back2[j].map_or("never".to_string(), |t| format!("{} ms", t / 1000)),
                    lat_in(from, to).join(", ")
                );
                if k == 1 {
                    println!("RESULT     second gesture ({g} s after the first): {fields}");
                } else {
                    // Member 1's own hand over this gesture's frames (spike fb12cb4).
                    let e: Vec<f64> = own_err.range(from..=340 + k * sh).map(|(_, &v)| v).collect();
                    println!(
                        "RESULT     gesture {} ({g} s after the one before): {fields}; member 1's own hand during it: mean {:.2} mm, max {:.2} mm over {} frames",
                        k + 1,
                        e.iter().sum::<f64>() / e.len().max(1) as f64,
                        e.iter().cloned().fold(0.0, f64::max),
                        e.len()
                    );
                }
            }
        }
        if o.by_kind {
            // Where each member's offered bytes went during the gesture (kbit/s, messages/s, mean bytes).
            for &m in &ids {
                let mut v: Vec<(&str, (u64, u64))> = sim.net.kinds_from.iter().filter(|((f, _), _)| *f == m).map(|((_, k), c)| (*k, *c)).collect();
                v.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
                let parts: Vec<String> = v
                    .iter()
                    .map(|(k, (n, b))| format!("{k} {:.1} kbit/s ({:.1}/s x {} B)", *b as f64 * 8.0 / 1000.0 / secs, *n as f64 / secs, b / (*n).max(1)))
                    .collect();
                println!("RESULT     member {m}'s offered bytes by kind: {}", parts.join(", "));
                // Entries by kind and part (measurement only): entries/s, then mean bytes per entry:
                // header (member, seq, stamp), prev id, ack head ids, payload, signatures.
                let mut tot = [0u64; 6];
                let mut pk: Vec<String> = Vec::new();
                for ((_, k), p) in sim.net.parts_from.iter().filter(|((f, _), _)| *f == m) {
                    for i in 0..6 {
                        tot[i] += p[i];
                    }
                    let n = p[0].max(1) as f64;
                    pk.push(format!(
                        "{k} {:.1}/s x {:.1} B (header {:.1}, prev {:.1}, head ids {:.1}, payload {:.1}, sigs {:.1})",
                        p[0] as f64 / secs, p[1..].iter().sum::<u64>() as f64 / n, p[1] as f64 / n, p[2] as f64 / n, p[3] as f64 / n, p[4] as f64 / n, p[5] as f64 / n
                    ));
                }
                let fr = sim.net.framing_from.get(&m).copied().unwrap_or(0);
                let dg = sim.net.kinds_from.get(&(m, "(datagram overhead)")).copied().unwrap_or((0, 0));
                let n = tot[0].max(1) as f64;
                println!("RESULT     member {m}'s entries by part: {}", pk.join("; "));
                println!(
                    "RESULT     member {m}'s entries: {:.1}/s; per entry, B: header {:.1}, prev {:.1}, head ids {:.1}, payload {:.1}, sigs {:.1}, framing and other messages {:.1}, datagram overhead {:.1} ({:.1} datagrams/s); total {:.1}",
                    tot[0] as f64 / secs, tot[1] as f64 / n, tot[2] as f64 / n, tot[3] as f64 / n, tot[4] as f64 / n, tot[5] as f64 / n,
                    fr as f64 / n, dg.1 as f64 / n, dg.0 as f64 / secs,
                    (tot[1..].iter().sum::<u64>() + fr + dg.1) as f64 / n
                );
                let st = sim.replica(m, 1).sim.stats;
                println!(
                    "RESULT     member {m}'s lean entries received: {}, explicit form asked for: {}; waited: {} (mean {:.1}, max {} frames); fetch requests sent (whole run): {}",
                    st.lean_in,
                    st.lean_fallbacks,
                    st.lean_waited,
                    st.lean_wait_frames as f64 / st.lean_waited.max(1) as f64,
                    st.lean_wait_max,
                    st.fetches_sent
                );
                println!(
                    "RESULT     member {m}'s rollbacks: {}, max depth {} frames, frames resimulated {}",
                    st.rollbacks, st.max_depth, st.resim_frames
                );
            }
        }
        println!(
            "RESULT     mean confirmation lag per member from the gesture's start, frames: {}",
            join(lag_sum.iter().map(|s| format!("{:.1}", *s as f64 / lag_n.max(1) as f64)).collect())
        );
        let n_err = own_err.len().max(1) as f64;
        println!(
            "RESULT     member 1's own hand against its target, over {} gesture frames: mean {:.2} mm, max {:.2} mm",
            own_err.len(),
            own_err.values().sum::<f64>() / n_err,
            own_err.values().cloned().fold(0.0, f64::max)
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
            if let Some((sm, _)) = o.slow.filter(|&(sm, _)| sm != 1) {
                println!("RESULT     controller on member {sm}, the slow member: time at each level: {:?} ms", sim.replica(sm, 1).level_ms);
            }
        }
    }
}
