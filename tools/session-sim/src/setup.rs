//! Build the spike simulator (`pe_session_harness::Sim`) from a scenario.

use std::collections::{BTreeMap, BTreeSet};

use pe_session::ownership::{test_key, Cert, Change, SignRule};
use pe_session::session::{fix, JoinRule, JoinRules, QuorumRule, Rejoin, Rel, SceneKind, SimConfig, Target, ADMIN};
use pe_session_harness::{ballpit_sweep, Action, Cap, Link, Net, Peer, Sim};

use crate::scenario::{id, ActionSpec, OwnerSpec, CapCfg, LinkCfg, Scenario, SceneCfg, WorldCfg};

pub fn scene(s: SceneCfg) -> SceneKind {
    match s {
        SceneCfg::Pile { side, layers } => SceneKind::Pile { side, layers },
        SceneCfg::Ballpit { balls, hands } => SceneKind::BallPit { balls, hands },
        SceneCfg::Empty => SceneKind::Empty,
    }
}

/// The review fixes by name (session.rs `fix`).
pub const FIXES: &[(&str, u64)] = &[
    ("VC2", fix::VC2),
    ("VC3", fix::VC3),
    ("VC4", fix::VC4),
    ("VC5", fix::VC5),
    ("VCR1", fix::VCR1),
    ("VCR2", fix::VCR2),
    ("VCR3", fix::VCR3),
    ("VCR4", fix::VCR4),
    ("VCR5", fix::VCR5),
    ("VCR6", fix::VCR6),
    ("VCR7", fix::VCR7),
    ("VCR8", fix::VCR8),
    ("VCR9", fix::VCR9),
    ("VCR12", fix::VCR12),
    ("HO1", fix::HO1),
    ("VC3R2", fix::VC3R2),
    ("VC3R3", fix::VC3R3),
    ("VC3R4", fix::VC3R4),
    ("VC3R6", fix::VC3R6),
    ("VC3R7", fix::VC3R7),
    ("LAGKEEP", fix::LAGKEEP),
    ("VC4R2", fix::VC4R2),
    ("VC4R3", fix::VC4R3),
    ("VC4R4", fix::VC4R4),
    ("VC4R5", fix::VC4R5),
    ("VC4R6", fix::VC4R6),
    ("VC3R3S", fix::VC3R3S),
    ("VC6R3", fix::VC6R3),
    ("VC6R4", fix::VC6R4),
    ("PF1", fix::PF1),
    ("VC7R1", fix::VC7R1),
    ("RECUT", fix::RECUT),
    ("VC8R1", fix::VC8R1),
    ("OWNY2", fix::OWNY2),
];

pub fn fixes_off(w: &WorldCfg) -> Result<u64, String> {
    w.fixes_off.iter().try_fold(0, |acc, n| {
        FIXES.iter().find(|(k, _)| k == n).map(|(_, b)| acc | b).ok_or_else(|| format!("fixes_off: unknown fix `{n}` (one of {:?})", FIXES.iter().map(|f| f.0).collect::<Vec<_>>()))
    })
}

pub fn rel(r: &str) -> Result<Rel, String> {
    match r {
        "friend" => Ok(Rel::Friend),
        "confidant" => Ok(Rel::Confidant),
        "invited" => Ok(Rel::Invited),
        _ => Err(format!("rel must be friend, confidant or invited, not `{r}`")),
    }
}

fn join_rules(c: &crate::scenario::JoinRulesCfg) -> Result<JoinRules, String> {
    let rules = c
        .rules
        .iter()
        .map(|r| {
            let target = match r.target.as_str() {
                "player" => Target::Player,
                "admin" => Target::Admin,
                "owner" => Target::Owner,
                t => return Err(format!("join_rules: target must be player, admin or owner, not `{t}`")),
            };
            Ok(JoinRule { target, all: r.all, rel: rel(&r.rel)?, role: r.role })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(JoinRules { rules, fallback: c.fallback })
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
        quorum: match w.quorum.as_deref() {
            Some("majority") => QuorumRule::Majority,
            Some("bft") => QuorumRule::Bft,
            _ => d.quorum,
        },
        ack_heads: w.ack_heads.unwrap_or(d.ack_heads),
        ..d
    }
}

pub fn sign_rule(s: &str) -> Result<SignRule, String> {
    match s {
        "all" => Ok(SignRule::All),
        "any" => Ok(SignRule::Any),
        k => k.parse::<u8>().ok().filter(|&k| k > 0).map(SignRule::Threshold).ok_or_else(|| format!("rule `{s}`: all, any or a threshold of 1 or more")),
    }
}

pub fn owner_change(o: &OwnerSpec) -> Result<Change, String> {
    let key = || o.key.map(test_key).ok_or_else(|| format!("owner change `{}` needs `key`", o.change));
    Ok(match o.change.as_str() {
        "add" => Change::Add { key: key()? },
        "leave" => Change::Leave { key: key()? },
        "remove" => Change::Remove { key: key()? },
        "set_recovery" => Change::SetRecovery { owner: key()?, recovery: o.recovery.map(test_key) },
        "set_rule" => Change::SetRule { rule: sign_rule(o.rule.as_deref().ok_or("owner change `set_rule` needs `rule`")?)? },
        "merge" => Change::Merge,
        c => return Err(format!("owner change `{c}`: add, leave, remove, set_recovery, set_rule or merge")),
    })
}

fn link(l: LinkCfg) -> Link {
    Link { delay_us: l.delay_ms * 1000, jitter_us: l.jitter_ms * 1000, loss_pct: l.loss_pct }
}

fn cap(c: CapCfg) -> Cap {
    Cap { rate_bps: c.kbps * 1000, burst_bytes: c.burst_bytes.unwrap_or(4000), max_queue_us: c.max_queue_ms.unwrap_or(500) * 1000 }
}

/// One action spec expanded into (frame, action) pairs, in order.
fn expand(a: &ActionSpec, member: u32, hz: u64) -> Result<Vec<(u64, Action)>, String> {
    let kinds = [
        a.touch.is_some(),
        a.hand.is_some(),
        a.portal.is_some(),
        a.carry.is_some(),
        a.backdated.is_some(),
        a.ballpit_sweep.is_some(),
        a.kick.is_some(),
        a.set_role.is_some(),
        a.define_role.is_some(),
        a.hand_path.is_some(),
        a.owner.is_some(),
        a.ban.is_some(),
    ];
    if kinds.iter().filter(|&&k| k).count() != 1 {
        return Err(format!(
            "member {member}: each action needs exactly one of touch, hand, portal, carry, backdated, ballpit_sweep, hand_path, kick, set_role, define_role, owner, ban: {a:?}"
        ));
    }
    if let Some(s) = &a.ballpit_sweep {
        return Ok(ballpit_sweep(hz, member, s.ahead));
    }
    if let Some(p) = &a.hand_path {
        if p.to_frame <= p.from_frame {
            return Err(format!("member {member}: hand_path: to_frame must be after from_frame"));
        }
        // session-sim `handowner`'s `path`.
        return Ok((p.from_frame..=p.to_frame)
            .map(|fr| {
                let u = (fr - p.from_frame) as f64 / (p.to_frame - p.from_frame) as f64;
                let target_mm = [((p.from_x + (p.to_x - p.from_x) * u) * 1000.0).round() as i32, p.y_mm, (p.z * 1000.0).round() as i32];
                (fr, Action::Hand { obj: p.obj, target_mm, active: true, ahead: 0 })
            })
            .collect());
    }
    let act = if let Some(t) = &a.touch {
        Action::Touch { obj: t.obj, impulse: t.impulse, grab: t.grab }
    } else if let Some(h) = &a.hand {
        Action::Hand { obj: h.obj, target_mm: h.target_mm, active: h.active, ahead: h.ahead }
    } else if let Some(p) = &a.portal {
        Action::Portal { open: p.open }
    } else if let Some(c) = &a.carry {
        Action::Carry { transfer: c.transfer, obj: c.obj, dest: c.dest }
    } else if let Some(k) = &a.kick {
        match (k.remove_admin, k.ban, k.perma) {
            (None, false, false) => Action::Kick { target: k.target },
            (Some(r), false, false) => Action::KickWith { target: k.target, rejoin: Rejoin::Role(r) },
            (None, true, false) => Action::KickWith { target: k.target, rejoin: Rejoin::Ban },
            (None, false, true) => Action::KickWith { target: k.target, rejoin: Rejoin::PermaBan },
            _ => return Err(format!("member {member}: a kick takes at most one of remove_admin, ban, perma")),
        }
    } else if let Some(b) = &a.ban {
        Action::Ban { key: b.key, on: b.on, perma: b.perma }
    } else if let Some(r) = &a.set_role {
        Action::SetRole { member: r.member, role: r.role }
    } else if let Some(r) = &a.define_role {
        Action::DefineRole { role: r.role, perms: r.perms }
    } else if let Some(o) = &a.owner {
        Action::Owner { change: owner_change(o)?, keys: o.signers.clone() }
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
    let all: BTreeSet<u32> = s.world.values().flat_map(|w| w.members.iter().chain(&w.joiners).copied()).collect();
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
            let joiner = w.joiners.contains(&pid);
            if !w.members.contains(&pid) && !joiner {
                continue;
            }
            if !w.keys.is_empty() {
                p.identities.insert(wid, w.keys.iter().copied().collect());
            }
            p.join(wid, &w.members, scene(w.scene), config(w), w.epoch_ms * 1000, plan(w, pid)?);
            p.route(wid, &w.routes);
            let off = fixes_off(w)?;
            let rules = w.join_rules.as_ref().map(join_rules).transpose()?;
            let r = p.worlds.get_mut(&wid).unwrap();
            if let Some(f) = w.ack_every_frames {
                r.ack_every_us = f * r.dt_us;
            }
            if let Some(rc) = w.member.get(&pid.to_string()) {
                r.throttle_us = rc.throttle_ms.map(|x| x * 1000);
                r.throttle_from_us = rc.throttle_from_ms * 1000;
                r.adapt = rc.adapt;
                if rc.ack_grid_only {
                    r.ack_grid_only = true;
                }
                if let Some(ms) = rc.ack_every_ms {
                    r.ack_every_us = ms * 1000;
                }
                r.sim.vcm.withhold_vc = rc.withhold_vc;
                r.sim.vcm.byz_ally = rc.byz_ally;
                r.sim.vcm.byz_no_precommit = rc.byz_no_precommit;
                r.sim.quiet_until_member = rc.quiet_until_member;
            }
            if !joiner && w.follow_joiners != Some(false) {
                for &j in &w.joiners {
                    r.sim.follow(j);
                }
            }
            if let Some(g) = &w.guests {
                // The spike's `guests`: every member (the roles table) a guest but the admins.
                r.sim.vcm.role_defs.insert(g.role, 0);
                let ms: Vec<u32> = r.sim.vcm.roles.keys().copied().collect();
                for m in ms {
                    r.sim.vcm.roles.insert(m, if g.admins.contains(&m) { ADMIN } else { g.role });
                }
            }
            for &(role, perms) in &w.role_defs {
                r.sim.vcm.role_defs.insert(role, perms);
            }
            for &(m, role) in &w.roles {
                r.sim.vcm.roles.insert(m, role);
            }
            if !w.owners.is_empty() {
                r.sim.vcm.owners = w.owners.iter().copied().collect();
            }
            if let Some(j) = rules {
                r.sim.vcm.join_rules = j;
            }
            if let Some(o) = &w.ownership {
                r.sim.set_ownership(Cert::genesis_test(&o.keys, sign_rule(&o.rule)?, o.nonce));
            }
            if !w.perma_bans.is_empty() {
                r.sim.set_perma_bans(w.perma_bans.iter().map(|&n| test_key(n)).collect());
            }
            r.sim.fixes_off = off;
        }
        peers.push(p);
    }
    for (wk, w) in &s.world {
        // An empty plan may name a non-member (to clear an included one).
        for k in w.plan.iter().filter(|(_, v)| !v.is_empty()).map(|(k, _)| k).chain(w.member.keys()) {
            let m = id(k)?;
            if !w.members.contains(&m) && !w.joiners.contains(&m) {
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
    fn hand_path_matches_the_spike() {
        // session-sim `handowner`'s `path` for member 2: x from 0.6 to -0.6 m over frames 60..=240.
        let w: WorldCfg = toml::from_str(
            "members = [1]\nscene = { ballpit = { balls = 10, hands = 2 } }\nepoch_ms = 0\n[plan]\n1 = [{ hand_path = { obj = 11, from_frame = 60, to_frame = 240, from_x = 0.6, to_x = -0.6, y_mm = 400, z = 0.5 } }]\n",
        )
        .unwrap();
        let p = plan(&w, 1).unwrap();
        assert_eq!(p.len(), 181);
        let at = |f: u64| match &p[&f][..] {
            [Action::Hand { obj: 11, target_mm, active: true, ahead: 0 }] => *target_mm,
            a => panic!("{a:?}"),
        };
        assert_eq!(at(60), [600, 400, 500]);
        assert_eq!(at(150), [0, 400, 500]);
        assert_eq!(at(240), [-600, 400, 500]);
        let u = (77 - 60) as f64 / 180.0;
        assert_eq!(at(77)[0], ((0.6 + (-0.6 - 0.6) * u) * 1000.0).round() as i32);
    }

    #[test]
    fn sweep_matches_the_spike() {
        let w: WorldCfg = toml::from_str("members = [1]\nscene = { ballpit = { balls = 10, hands = 2 } }\nepoch_ms = 0\nhz = 90\n[plan]\n1 = [{ ballpit_sweep = { ahead = 4 } }]\n").unwrap();
        let n: usize = plan(&w, 1).unwrap().values().map(Vec::len).sum();
        assert_eq!(n, ballpit_sweep(90, 1, 4).len());
        assert_eq!(config(&w).dt, 1.0 / 90.0);
    }
}
