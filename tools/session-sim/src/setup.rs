//! Build the spike simulator (`spike::Sim`) from a scenario.

use std::collections::{BTreeMap, BTreeSet};

use pe_sim_core::session::{SceneKind, SimConfig};
use spike::{ballpit_sweep, Action, Cap, Link, Net, Peer, Sim};

use crate::scenario::{id, ActionSpec, CapCfg, LinkCfg, Scenario, SceneCfg, WorldCfg};

pub fn scene(s: SceneCfg) -> SceneKind {
    match s {
        SceneCfg::Pile { side, layers } => SceneKind::Pile { side, layers },
        SceneCfg::Ballpit { balls, hands } => SceneKind::BallPit { balls, hands },
    }
}

pub fn hz(w: &WorldCfg) -> u64 {
    w.hz.unwrap_or(60)
}

pub fn config(w: &WorldCfg) -> SimConfig {
    let d = SimConfig::default();
    SimConfig {
        dt: 1.0 / hz(w) as f32,
        window: w.window.unwrap_or(d.window),
        seed: w.seed.unwrap_or(d.seed),
        horizon_slack: w.horizon_slack.unwrap_or(d.horizon_slack),
        check_every: w.check_every.unwrap_or(d.check_every),
        hand_predict: w.hand_predict.unwrap_or(d.hand_predict),
        suspect_after: w.suspect_after.unwrap_or(d.suspect_after),
        ..d
    }
}

fn link(l: LinkCfg) -> Link {
    Link { delay_us: l.delay_ms * 1000, jitter_us: l.jitter_ms * 1000, loss_pct: l.loss_pct }
}

fn cap(c: CapCfg) -> Cap {
    Cap { rate_bps: c.kbps * 1000, burst_bytes: c.burst_bytes.unwrap_or(4000), max_queue_us: c.max_queue_ms.unwrap_or(500) * 1000 }
}

/// One action spec expanded into (frame, action) pairs, in order.
fn expand(a: &ActionSpec, member: u32, hz: u64) -> Result<Vec<(u64, Action)>, String> {
    let kinds = [a.touch.is_some(), a.hand.is_some(), a.portal.is_some(), a.carry.is_some(), a.backdated.is_some(), a.ballpit_sweep.is_some()];
    if kinds.iter().filter(|&&k| k).count() != 1 {
        return Err(format!("member {member}: each action needs exactly one of touch, hand, portal, carry, backdated, ballpit_sweep: {a:?}"));
    }
    if let Some(s) = &a.ballpit_sweep {
        return Ok(ballpit_sweep(hz, member, s.ahead));
    }
    let act = if let Some(t) = &a.touch {
        Action::Touch { obj: t.obj, impulse: t.impulse, grab: t.grab }
    } else if let Some(h) = &a.hand {
        Action::Hand { obj: h.obj, target_mm: h.target_mm, active: h.active, ahead: h.ahead }
    } else if let Some(p) = &a.portal {
        Action::Portal { open: p.open }
    } else if let Some(c) = &a.carry {
        Action::Carry { transfer: c.transfer, obj: c.obj, dest: c.dest }
    } else {
        let b = a.backdated.as_ref().unwrap();
        Action::Backdated { obj: b.obj, impulse: b.impulse, back: b.back }
    };
    let at = a.at.ok_or_else(|| format!("member {member}: action without `at`: {a:?}"))?;
    let (every, count) = (a.every.unwrap_or(1), a.count.unwrap_or(1));
    Ok((0..count).map(|k| (at + k * every, act.clone())).collect())
}

/// A member's plan in a world: every action keyed by frame, in file order within a frame.
pub fn plan(w: &WorldCfg, member: u32) -> Result<BTreeMap<u64, Vec<Action>>, String> {
    let mut m: BTreeMap<u64, Vec<Action>> = BTreeMap::new();
    for (k, v) in &w.plan {
        if id(k)? != member {
            continue;
        }
        for a in v {
            for (f, act) in expand(a, member, hz(w))? {
                m.entry(f).or_default().push(act);
            }
        }
    }
    Ok(m)
}

pub fn end_us(s: &Scenario) -> Result<u64, String> {
    s.end_us.or(s.end_ms.map(|x| x * 1000)).ok_or_else(|| "missing end_ms".to_string())
}

/// The peers in simulator order.
pub fn peer_order(s: &Scenario) -> Result<Vec<u32>, String> {
    if !s.peers.is_empty() {
        return Ok(s.peers.clone());
    }
    let all: BTreeSet<u32> = s.world.values().flat_map(|w| w.members.iter().copied()).collect();
    Ok(all.into_iter().collect())
}

pub fn build(s: &Scenario) -> Result<Sim, String> {
    let n = &s.net;
    let mut net = Net::new(link(n.link), n.seed);
    for l in &n.links {
        net.links.insert((l.from, l.to), link(LinkCfg { delay_ms: l.delay_ms, jitter_ms: l.jitter_ms, loss_pct: l.loss_pct }));
    }
    net.default_cap = n.cap.map(cap);
    for c in &n.caps {
        net.caps.insert((c.from, c.to), cap(CapCfg { kbps: c.kbps, burst_bytes: c.burst_bytes, max_queue_ms: c.max_queue_ms }));
    }
    net.batching = n.batching;
    if let Some([a, b]) = n.count_ms {
        net.count_from_us = a * 1000;
        net.count_to_us = b * 1000;
    }
    for k in s.peer.keys() {
        id(k)?;
    }
    let mut peers = Vec::new();
    for pid in peer_order(s)? {
        let skew = s.peer.get(&pid.to_string()).map_or(0, |p| p.skew_us);
        let mut p = Peer::new(pid, skew);
        for (wk, w) in &s.world {
            let wid = id(wk)?;
            if !w.members.contains(&pid) {
                continue;
            }
            p.join(wid, &w.members, scene(w.scene), config(w), w.epoch_ms * 1000, plan(w, pid)?);
            p.route(wid, &w.routes);
            if let Some(rc) = w.member.get(&pid.to_string()) {
                let r = p.worlds.get_mut(&wid).unwrap();
                r.throttle_us = rc.throttle_ms.map(|x| x * 1000);
                r.throttle_from_us = rc.throttle_from_ms * 1000;
                r.adapt = rc.adapt;
            }
        }
        peers.push(p);
    }
    for (wk, w) in &s.world {
        // An empty plan may name a non-member (to clear an included one).
        for k in w.plan.iter().filter(|(_, v)| !v.is_empty()).map(|(k, _)| k).chain(w.member.keys()) {
            if !w.members.contains(&id(k)?) {
                return Err(format!("world {wk}: member {k} has a plan or settings but is not a member"));
            }
        }
    }
    let mut sim = Sim::new(net, peers, end_us(s)?);
    sim.reverse_order = s.sim.reverse_order;
    if let Some(d) = s.sim.carry_deadline_frames {
        sim.carry_deadline_frames = d;
    }
    if s.sim.provenance == Some(false) {
        // Control run: arrivals skip the P2-5 provenance checks.
        for p in &mut sim.peers {
            for r in p.worlds.values_mut() {
                r.sim.provenance = false;
            }
        }
    }
    Ok(sim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_expand_in_file_order_within_a_frame() {
        let w: WorldCfg = toml::from_str(
            r#"
members = [1]
scene = { pile = { side = 2, layers = 1 } }
epoch_ms = 0
[plan]
1 = [
    { at = 10, every = 5, count = 3, touch = { obj = 1, impulse = [0.0, 1.0, 0.0] } },
    { at = 15, portal = { open = false } },
]
"#,
        )
        .unwrap();
        let p = plan(&w, 1).unwrap();
        assert_eq!(p.keys().copied().collect::<Vec<_>>(), vec![10, 15, 20]);
        assert!(matches!(p[&15][..], [Action::Touch { obj: 1, .. }, Action::Portal { open: false }]));
        assert!(plan(&w, 2).unwrap().is_empty());
    }

    #[test]
    fn sweep_matches_the_spike() {
        let w: WorldCfg = toml::from_str("members = [1]\nscene = { ballpit = { balls = 10, hands = 2 } }\nepoch_ms = 0\nhz = 90\n[plan]\n1 = [{ ballpit_sweep = { ahead = 4 } }]\n").unwrap();
        let n: usize = plan(&w, 1).unwrap().values().map(Vec::len).sum();
        assert_eq!(n, ballpit_sweep(90, 1, 4).len());
        assert_eq!(config(&w).dt, 1.0 / 90.0);
    }
}
