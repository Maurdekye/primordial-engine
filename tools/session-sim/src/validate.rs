//! Meaning checks on a loaded scenario, after the simulator is built and before it runs: every
//! id names a peer or member that exists, every time lies inside the run, and every value is in
//! range. A scenario that fails them is refused by `check`, `run` and `regress` (review of
//! fffdcbc: a fault aimed at a member who doesn't exist would otherwise do nothing, and pass).

use std::collections::BTreeSet;

use spike::Sim;

use crate::scenario::{id, ActionSpec, Byzantine, Honest, Ids, Report, Scenario, When};
use crate::setup::{end_us, fixes_off, hz, peer_order, rel};

struct V<'a> {
    s: &'a Scenario,
    sim: &'a Sim,
    errors: Vec<String>,
    end_us: u64,
}

impl V<'_> {
    fn err(&mut self, e: String) {
        self.errors.push(e);
    }

    fn peer(&mut self, m: u32, what: &str) {
        if !self.sim.peers.iter().any(|p| p.id == m) {
            self.err(format!("{what}: member {m} is not a peer"));
        }
    }

    fn ids(&mut self, ids: &Option<Ids>, what: &str) {
        match ids {
            Some(Ids::One(m)) => self.peer(*m, what),
            Some(Ids::Many(v)) => v.iter().for_each(|&m| self.peer(m, what)),
            None => {}
        }
    }

    fn member(&mut self, w: u32, m: u32, what: &str) {
        let ok = self.sim.peers.iter().any(|p| p.id == m && p.worlds.contains_key(&w));
        if !ok {
            self.err(format!("{what}: member {m} is not a member of world {w}"));
        }
    }

    fn world(&mut self, w: u32, what: &str) -> bool {
        let ok = self.s.world.contains_key(&w.to_string());
        if !ok {
            self.err(format!("{what}: no world {w}"));
        }
        ok
    }

    fn time(&mut self, ms: u64, what: &str) {
        if ms * 1000 > self.end_us {
            self.err(format!("{what}: {ms} ms is after the end of the run ({} ms)", self.end_us / 1000));
        }
    }

    /// The last clock frame of world `w` that every member reaches before the run ends.
    fn frames_reached(&self, w: u32) -> u64 {
        self.sim
            .peers
            .iter()
            .filter_map(|p| p.worlds.get(&w).map(|r| (p.skew_us, r)))
            .map(|(skew, r)| ((self.end_us as i64 + skew).max(0) as u64).saturating_sub(r.epoch_us) / r.dt_us)
            .min()
            .unwrap_or(0)
    }

    /// The last clock frame of world `w` that member `m` reaches.
    fn member_frames(&self, w: u32, m: u32) -> u64 {
        let Some(p) = self.sim.peers.iter().find(|p| p.id == m) else { return u64::MAX };
        let Some(r) = p.worlds.get(&w) else { return u64::MAX };
        ((self.end_us as i64 + p.skew_us).max(0) as u64).saturating_sub(r.epoch_us) / r.dt_us
    }

    fn honest(&mut self, h: &Honest, what: &str) {
        match h {
            Honest::List(v) => {
                if v.is_empty() {
                    self.err(format!("{what}: no members"));
                }
                v.iter().for_each(|&m| self.member(1, m, what));
            }
            Honest::At { members_at } => self.member(1, *members_at, what),
            Honest::Among { members_at, among } => {
                self.member(1, *members_at, what);
                among.iter().for_each(|&m| self.member(1, m, what));
            }
        }
    }

    fn members(&mut self, v: &[u32], what: &str) {
        if v.is_empty() {
            self.err(format!("{what}: no members"));
        }
        v.iter().for_each(|&m| self.member(1, m, what));
    }

    fn object(&mut self, w: u32, obj: u32, what: &str) {
        let r = self.sim.peers.iter().find_map(|p| p.worlds.get(&w));
        let ok = r.is_some_and(|r| r.sim.t.objs.get(obj as usize).is_some_and(|o| o.is_some()));
        if !ok {
            self.err(format!("{what}: world {w} has no object {obj}"));
        }
    }

    fn action(&mut self, w: u32, m: u32, a: &ActionSpec) {
        let what = format!("world {w} member {m} plan");
        if a.every == Some(0) || a.count == Some(0) {
            self.err(format!("{what}: every and count must be at least 1"));
        }
        if let Some(at) = a.at {
            let last = at + a.every.unwrap_or(1) * a.count.unwrap_or(1).saturating_sub(1);
            let reached = self.member_frames(w, m);
            if last > reached {
                self.err(format!("{what}: an action at frame {last} is past the last frame the member reaches ({reached})"));
            }
        }
        let obj = a.touch.as_ref().map(|t| t.obj).or(a.hand.as_ref().map(|h| h.obj)).or(a.carry.as_ref().map(|c| c.obj)).or(a.backdated.as_ref().map(|b| b.obj));
        if let (Some(o), false) = (obj, a.missing_ok) {
            self.object(w, o, &what);
        }
        if a.owner.is_some() && self.s.world.get(&w.to_string()).is_some_and(|c| c.ownership.is_none()) {
            self.err(format!("{what}: an owner certificate in world {w}, which has no `ownership`"));
        }
        if let Some(c) = &a.carry {
            if !self.s.world.contains_key(&c.dest.to_string()) {
                self.err(format!("{what}: carry to world {}, which does not exist", c.dest));
            }
        }
    }
}

pub fn validate(s: &Scenario, sim: &Sim) -> Result<(), String> {
    let mut v = V { s, sim, errors: Vec::new(), end_us: 0 };
    if s.end_ms.is_some() && s.end_us.is_some() {
        v.err("give end_ms or end_us, not both".into());
    }
    v.end_us = end_us(s)?;
    // Every world member must be a peer (a missing one panics inside the simulator).
    let peers: BTreeSet<u32> = peer_order(s)?.into_iter().collect();
    if peers.len() != peer_order(s)?.len() {
        v.err("peers: a member is listed twice".into());
    }
    for (wk, w) in &s.world {
        let wid = id(wk)?;
        for m in &w.members {
            if !peers.contains(m) {
                v.err(format!("world {wid}: member {m} is not in `peers`"));
            }
        }
        if w.members.is_empty() {
            v.err(format!("world {wid}: no members"));
        }
        if hz(w) == 0 {
            v.err(format!("world {wid}: hz must be at least 1"));
        }
        for r in &w.routes {
            if !s.world.contains_key(&r.to_string()) {
                v.err(format!("world {wid}: routes to world {r}, which does not exist"));
            }
        }
        for j in &w.joiners {
            if w.members.contains(j) {
                v.err(format!("world {wid}: joiner {j} is already a member"));
            }
            if !peers.contains(j) {
                v.err(format!("world {wid}: joiner {j} is not in `peers`"));
            }
        }
        if let Some(q) = &w.quorum {
            if q != "bft" && q != "majority" {
                v.err(format!("world {wid}: quorum must be bft or majority, not `{q}`"));
            }
        }
        if let Err(e) = fixes_off(w) {
            v.err(format!("world {wid}: {e}"));
        }
        if w.ack_every_frames == Some(0) {
            v.err(format!("world {wid}: ack_every_frames must be at least 1"));
        }
        if let Some(g) = &w.guests {
            for a in &g.admins {
                if !w.members.contains(a) {
                    v.err(format!("world {wid}: guests: admin {a} is not a member"));
                }
            }
        }
    }
    // Links and caps.
    let n = &s.net;
    let mut losses = vec![("net.link", n.link.loss_pct)];
    for l in &n.links {
        v.peer(l.from, "net.links");
        v.peer(l.to, "net.links");
        losses.push(("net.links", l.loss_pct));
    }
    for (what, l) in losses {
        if l > 100 {
            v.err(format!("{what}: loss_pct {l} is above 100"));
        }
    }
    if n.cap.is_some_and(|c| c.kbps == 0) {
        v.err("net.cap: kbps must be at least 1".into());
    }
    for c in &n.caps {
        v.peer(c.from, "net.caps");
        v.peer(c.to, "net.caps");
        if c.kbps == 0 {
            v.err("net.caps: kbps must be at least 1".into());
        }
    }
    if let Some([a, b]) = n.count_ms {
        if b <= a {
            v.err(format!("net.count_ms: empty window [{a}, {b})"));
        }
    }
    // Plans.
    for (wk, w) in &s.world {
        let wid = id(wk)?;
        for (mk, acts) in &w.plan {
            let m = id(mk)?;
            if !w.members.contains(&m) && !w.joiners.contains(&m) {
                continue; // an empty plan for a non-member (setup refuses a non-empty one)
            }
            for a in acts {
                v.action(wid, m, a);
            }
        }
    }
    // Fault rules.
    for (i, r) in s.fault.iter().enumerate() {
        let what = format!("fault {}", i + 1);
        v.ids(&r.from, &what);
        v.ids(&r.to, &what);
        v.ids(&r.involves, &what);
        if let Some(w) = r.world {
            v.world(w, &what);
        }
        if let (Some(a), Some(b)) = (r.after_ms, r.before_ms) {
            if b <= a {
                v.err(format!("{what}: empty window: before_ms {b} is not after after_ms {a}"));
            }
        }
        if let Some(a) = r.after_ms {
            v.time(a, &what);
        }
        if let Some(groups) = &r.partition {
            let mut seen = BTreeSet::new();
            for &m in groups.iter().flatten() {
                v.peer(m, &what);
                if !seen.insert(m) {
                    v.err(format!("{what}: member {m} is in two partition groups"));
                }
            }
            if groups.len() < 2 {
                v.err(format!("{what}: a partition needs at least two groups"));
            }
        }
        if r.times == Some(0) {
            v.err(format!("{what}: times = 0 never applies"));
        }
    }
    // Crashes.
    for c in &s.crash {
        v.peer(c.member, "crash");
        v.time(c.at_ms, "crash");
        if let Some(r) = c.recover_ms {
            if r <= c.at_ms {
                v.err(format!("crash: recover_ms {r} is not after at_ms {}", c.at_ms));
            }
            v.time(r, "crash recover");
        }
    }
    // Join requests.
    for j in &s.join {
        if v.world(j.world, "join") {
            v.member(j.world, j.member, "join");
            if let Some(f) = j.from {
                v.member(j.world, f, "join from");
            }
            j.to.iter().for_each(|&m| v.member(j.world, m, "join to"));
            j.attests_to.iter().flatten().for_each(|&m| v.member(j.world, m, "join attests_to"));
            if let Some(f) = j.log_from {
                v.member(j.world, f, "join log_from");
            }
        }
        v.time(j.at_ms, "join");
        for a in &j.attests {
            if let Err(e) = rel(&a.rel) {
                v.err(format!("join attests: {e}"));
            }
            v.peer(a.with, "join attests with");
        }
    }
    // Byzantine behaviour.
    let names: Vec<&str> = s
        .byzantine
        .iter()
        .filter_map(|b| match b {
            Byzantine::Equivocate { name, .. } | Byzantine::ForkChain { name, .. } | Byzantine::ForkVote { name, .. } => name.as_deref(),
            _ => None,
        })
        .collect();
    for b in &s.byzantine {
        match b {
            Byzantine::Equivocate { after, world, member, when, touch, to, hide_from, .. } => {
                if v.world(*world, "equivocate") {
                    v.member(*world, *member, "equivocate");
                    to.iter().chain(hide_from).for_each(|&m| v.member(*world, m, "equivocate to"));
                    if let When::ConfAbove { at, .. } = when {
                        v.member(*world, *at, "equivocate when");
                    }
                    if let When::AtMs(t) = when {
                        v.time(*t, "equivocate when");
                    }
                    v.object(*world, touch.obj, "equivocate touch");
                }
                if let Some(a) = after {
                    if !names.contains(&a.as_str()) {
                        v.err(format!("equivocate: after `{a}`, but no behaviour has that name"));
                    }
                }
            }
            Byzantine::ForkChain { world, member, to, every_ms, .. } => {
                if v.world(*world, "fork_chain") {
                    v.member(*world, *member, "fork_chain");
                    to.iter().for_each(|&m| v.member(*world, m, "fork_chain to"));
                }
                if *every_ms == 0 {
                    v.err("fork_chain: every_ms must be at least 1".into());
                }
            }
            Byzantine::ForkVote { after, world, member, to, .. } => {
                if v.world(*world, "fork_vote") {
                    v.member(*world, *member, "fork_vote");
                    to.iter().for_each(|&m| v.member(*world, m, "fork_vote to"));
                }
                if let Some(a) = after {
                    if !names.contains(&a.as_str()) {
                        v.err(format!("fork_vote: after `{a}`, but no behaviour has that name"));
                    }
                }
            }
            Byzantine::LieInVotes { world, member } | Byzantine::LagHorizon { world, member, .. } => {
                if v.world(*world, "byzantine") {
                    v.member(*world, *member, "byzantine");
                }
            }
            Byzantine::HideRefs { world, member, from_ms, until_ms, .. } => {
                if v.world(*world, "hide_refs") {
                    v.member(*world, *member, "hide_refs");
                }
                v.time(*from_ms, "hide_refs");
                if let Some(u) = until_ms {
                    if u <= from_ms {
                        v.err(format!("hide_refs: until_ms {u} is not after from_ms {from_ms}"));
                    }
                    v.time(*u, "hide_refs until");
                }
            }
            Byzantine::ForkBranch { member, a_to, b_to, .. } => {
                v.member(1, *member, "fork_branch");
                v.member(1, *b_to, "fork_branch b_to");
                a_to.iter().for_each(|&m| v.member(1, m, "fork_branch a_to"));
            }
            Byzantine::Precommit { member, from_ms, to, while_member_at, boundary, ahead, .. } => {
                v.member(1, *member, "precommit");
                to.iter().chain(while_member_at).for_each(|&m| v.member(1, m, "precommit"));
                v.time(*from_ms, "precommit");
                if boundary.is_some() == ahead.is_some() {
                    v.err("precommit: give boundary or ahead".into());
                }
            }
            Byzantine::FakeAckEpoch { member, from_ms, epoch, ahead, while_member_at } => {
                v.member(1, *member, "fake_ack_epoch");
                while_member_at.iter().for_each(|&m| v.member(1, m, "fake_ack_epoch"));
                v.time(*from_ms, "fake_ack_epoch");
                if epoch.is_some() == ahead.is_some() {
                    v.err("fake_ack_epoch: give epoch or ahead".into());
                }
            }
            Byzantine::HandForgery { member, to, grab_ms, carry_ms, plant_ms, .. } => {
                v.member(1, *member, "hand_forgery");
                v.member(1, *to, "hand_forgery to");
                if !(grab_ms < carry_ms && carry_ms < plant_ms) {
                    v.err("hand_forgery: grab_ms, carry_ms, plant_ms must increase".into());
                }
                v.time(*plant_ms, "hand_forgery");
            }
            Byzantine::RotateEntries { member, from_ms, to } => {
                v.member(1, *member, "rotate_entries");
                to.iter().for_each(|&m| v.member(1, m, "rotate_entries to"));
                v.time(*from_ms, "rotate_entries");
                if to.is_empty() {
                    v.err("rotate_entries: no members in to".into());
                }
            }
            Byzantine::ChurnVote { member, target, from_ms, until_ms, while_member_at, to } => {
                v.member(1, *member, "churn_vote");
                v.member(1, *target, "churn_vote target");
                to.iter().chain(while_member_at).for_each(|&m| v.member(1, m, "churn_vote"));
                v.time(*from_ms, "churn_vote");
                if until_ms.is_some_and(|u| u <= *from_ms) {
                    v.err("churn_vote: until_ms is not after from_ms".into());
                }
            }
            Byzantine::VoidAdmin { member, at_ms, target, to, .. } => {
                v.member(1, *member, "void_admin");
                v.member(1, *target, "void_admin target");
                to.iter().for_each(|&m| v.member(1, m, "void_admin to"));
                v.time(*at_ms, "void_admin");
            }
            Byzantine::NoopSetrole { member, from_ms, every_ms, to, while_member_at, .. } => {
                v.member(1, *member, "noop_setrole");
                to.iter().chain(while_member_at).for_each(|&m| v.member(1, m, "noop_setrole"));
                v.time(*from_ms, "noop_setrole");
                if *every_ms == 0 {
                    v.err("noop_setrole: every_ms is 0".into());
                }
            }
            Byzantine::AdminWedge { member, at_ms, a_to, part, others, part_ms, others_ms, .. } => {
                v.member(1, *member, "admin_wedge");
                a_to.iter().chain(others).chain([part]).for_each(|&m| v.member(1, m, "admin_wedge"));
                v.time(*at_ms, "admin_wedge");
                if !(at_ms < part_ms && part_ms <= others_ms) {
                    v.err("admin_wedge: needs at_ms < part_ms <= others_ms".into());
                }
                if others.contains(part) || others.contains(member) {
                    v.err("admin_wedge: others must not hold part or the member".into());
                }
            }
            Byzantine::GoSilent { member, from_ms, .. } => {
                v.member(1, *member, "go_silent");
                v.time(*from_ms, "go_silent");
            }
            Byzantine::MixRefs { member, of, from_ms, every_ms, .. } => {
                v.member(1, *member, "mix_refs");
                v.member(1, *of, "mix_refs of");
                v.time(*from_ms, "mix_refs");
                if *every_ms == 0 {
                    v.err("mix_refs: every_ms is 0".into());
                }
            }
            Byzantine::AdminFork { member, at_ms, target, a_to, b_to, prevote_to, .. } => {
                v.member(1, *member, "admin_fork");
                v.member(1, *target, "admin_fork target");
                a_to.iter().chain(b_to).chain(prevote_to).for_each(|&m| v.member(1, m, "admin_fork to"));
                v.time(*at_ms, "admin_fork");
                if a_to.iter().any(|m| b_to.contains(m)) {
                    v.err("admin_fork: a_to and b_to overlap".into());
                }
            }
            Byzantine::EquivocatePrevote { member, from_ms, value_to, nil_to } => {
                v.member(1, *member, "equivocate_prevote");
                value_to.iter().chain(nil_to).for_each(|&m| v.member(1, m, "equivocate_prevote to"));
                v.time(*from_ms, "equivocate_prevote");
            }
            Byzantine::FakeRefs { member, from_ms, of, .. } => {
                v.member(1, *member, "fake_refs");
                v.member(1, *of, "fake_refs of");
                v.time(*from_ms, "fake_refs");
            }
            Byzantine::GhostVote { member, at_ms, target, to, .. } => {
                v.member(1, *member, "ghost_vote");
                v.member(1, *target, "ghost_vote target");
                to.iter().for_each(|&m| v.member(1, m, "ghost_vote to"));
                v.time(*at_ms, "ghost_vote");
            }
            Byzantine::SelfKickVote { member, to, .. } | Byzantine::BackdatedKick { member, to, .. } => {
                v.member(1, *member, "byzantine");
                to.iter().for_each(|&m| v.member(1, m, "byzantine to"));
            }
            Byzantine::ForgeProposal { world, member, to } => {
                if v.world(*world, "forge_proposal") {
                    v.member(*world, *member, "forge_proposal");
                    to.iter().for_each(|&m| v.member(*world, m, "forge_proposal to"));
                }
            }
            Byzantine::R14Forgeries { attacks } => {
                for (w, m) in [(1, 1), (1, 2), (2, 1), (2, 3)] {
                    if v.world(w, "r14_forgeries (r14's worlds)") {
                        v.member(w, m, "r14_forgeries (r14's members)");
                    }
                }
                for a in attacks {
                    v.time(a.at_ms, "r14_forgeries");
                }
            }
        }
    }
    // Reports.
    let mut checks = 0;
    for r in &s.report {
        match r {
            Report::CheckWorld { world, frames, skip, skip_evicted_at, .. } => {
                checks += 1;
                if v.world(*world, "check_world") {
                    skip.iter().for_each(|&m| v.member(*world, m, "check_world skip"));
                    if let Some(m) = skip_evicted_at {
                        v.member(*world, *m, "check_world skip_evicted_at");
                    }
                    let reached = v.frames_reached(*world);
                    if *frames > reached {
                        v.err(format!("check_world: frames {frames} is past the last frame every member of world {world} reaches ({reached})"));
                    }
                }
            }
            Report::Ownership { world, frames, at, .. } => {
                checks += 1;
                if v.world(*world, "ownership") {
                    if v.s.world[&world.to_string()].ownership.is_none() {
                        v.err(format!("ownership: world {world} has no `ownership`"));
                    }
                    let reached = v.frames_reached(*world);
                    if *frames > reached {
                        v.err(format!("ownership: frames {frames} is past the last frame every member of world {world} reaches ({reached})"));
                    }
                    if at.iter().any(|f| f > frames) {
                        v.err(format!("ownership: `at` {at:?} reaches past frames {frames}"));
                    }
                }
            }
            Report::Verdict { honest, .. } => {
                checks += 1;
                v.world(1, "verdict");
                v.honest(honest, "verdict honest");
            }
            Report::Membership { honest, .. } => v.honest(honest, "membership"),
            Report::ViewChanges { honest, .. } => v.honest(honest, "view_changes"),
            Report::Admin { honest, .. } => v.honest(honest, "admin"),
            Report::Admitted { honest, joiner } => {
                v.members(honest, "admitted");
                v.member(1, *joiner, "admitted joiner");
            }
            Report::Confs { honest, .. } => v.members(honest, "confs"),
            Report::BeforeFix { honest, .. } => {
                checks += 1;
                v.honest(honest, "before_fix");
                // Its part may stop at an assertion without failing: only with a fix switched off.
                if s.world.values().all(|w| w.fixes_off.is_empty()) {
                    v.err("before_fix: the part switches no fix off (fixes_off), so a panic in shipped code would go unreported".into());
                }
            }
            Report::Handowner { member, .. } => {
                checks += 1;
                v.member(1, *member, "handowner");
            }
            Report::ForkInfo => {
                if !s.byzantine.iter().any(|b| matches!(b, Byzantine::ForkBranch { .. })) {
                    v.err("fork_info: no fork_branch behaviour".into());
                }
            }
            Report::Check { case, honest, sides, at, members, target, runs, event, expect, conf_min, from_ms, until_ms, .. } => {
                checks += 1;
                honest.iter().for_each(|&m| v.member(1, m, "check honest"));
                sides.iter().flatten().for_each(|&m| v.member(1, m, "check sides"));
                members.iter().filter(|_| case != "joinrules").for_each(|&m| v.member(1, m, "check members"));
                at.iter().chain(target).for_each(|&m| v.member(1, m, "check"));
                let want = match case.as_str() {
                    "bridge" => sides.len() == 2 && sides.iter().all(|s| !s.is_empty()) && !honest.is_empty(),
                    "kickwar" => at.is_some() && members.len() == 2,
                    "kickpartition" => members.len() == 2 && target.is_some(),
                    "lastadmin" => !honest.is_empty(),
                    "adminclose" => honest.len() == 2,
                    "joinrules" => members.len() == 4,
                    "selfkickhead" | "capchurn" | "removed" => at.is_some() && target.is_some(),
                    "joinattests" | "lagprecommit" => at.is_some() && target.is_some() && !honest.is_empty(),
                    "slowadmin" => !honest.is_empty(),
                    "kickbackdate" => at.is_some() && !members.is_empty(),
                    "aheadlink" => at.is_some() && !members.is_empty() && !honest.is_empty(),
                    "onesided" => members.len() == 2,
                    "strikeframe" => at.is_some() && target.is_some() && !members.is_empty(),
                    "strikeframe_total" => at.is_some() && !members.is_empty() && !runs.is_empty(),
                    "aheadshield" => {
                        at.is_some()
                            && target.is_some()
                            && !honest.is_empty()
                            && matches!(expect.as_deref(), Some("voted_out") | Some("harmless"))
                            && (expect.as_deref() == Some("voted_out") || conf_min.is_some())
                    }
                    "fakeref" | "capstale" | "lagchurn" => at.is_some() && target.is_some() && event.is_some() && !honest.is_empty(),
                    "votechurn" | "votechurn_off" => at.is_some() && target.is_some() && from_ms.is_some() && until_ms.is_some(),
                    "adminvoid" | "joinskip" | "slowref" => at.is_some() && target.is_some() && !honest.is_empty(),
                    "joinwedge" | "adminfork" => at.is_some() && target.is_some() && members.len() == 1 && !honest.is_empty(),
                    "fakemix" => at.is_some() && target.is_some() && event.is_some() && !honest.is_empty(),
                    "prevotefork_off" => at.is_some() && target.is_some() && event.is_some(),
                    "prevotefork" => at.is_some() && target.is_some() && event.is_some() && !members.is_empty() && !honest.is_empty(),
                    "prevotefork_pf" => !members.is_empty() && !honest.is_empty(),
                    "capfake" | "fakeadmin" => at.is_some() && target.is_some() && !honest.is_empty(),
                    "adminwedge" => target.is_some() && !honest.is_empty() && s.byzantine.iter().any(|b| matches!(b, Byzantine::AdminWedge { .. })),
                    // The silence start must be a fault rule's, so the two cannot drift apart.
                    "vcscale" => matches!(r, Report::Check { silent: Some(_), from_ms: Some(t), .. } if s.fault.iter().any(|f| f.drop && f.after_ms == Some(*t))),
                    _ => true,
                };
                if !want {
                    v.err(format!("check {case}: missing or malformed fields (see scenario.rs `Report::Check`)"));
                }
            }
            Report::Stats { world } | Report::Events { world, .. } | Report::Traffic { world } | Report::Line { world, .. } => {
                v.world(*world, "report");
            }
            Report::FirstEntryFrame { world, author, .. } => {
                if v.world(*world, "first_entry_frame") {
                    v.member(*world, *author, "first_entry_frame");
                }
            }
            Report::Entries { world, at, author, .. } => {
                if v.world(*world, "entries") {
                    v.member(*world, *at, "entries");
                    v.member(*world, *author, "entries author");
                }
            }
            Report::Evicted { world, at, .. } | Report::Outcomes { world, at, .. } | Report::Expect { world, at, .. } => {
                if v.world(*world, "report") {
                    v.member(*world, *at, "report at");
                }
            }
            Report::Touches { world, author, at, .. } => {
                if v.world(*world, "touches") {
                    v.member(*world, *author, "touches author");
                    at.iter().for_each(|&m| v.member(*world, m, "touches at"));
                }
            }
            Report::R14Crash { at } => {
                for &[w, m] in at {
                    if v.world(w, "r14_crash") {
                        v.member(w, m, "r14_crash");
                    }
                }
            }
            Report::R14ChainJobs | Report::R14ChainOutcomes | Report::R14ChainConservation { .. } | Report::R14ChainEpoch => {
                for (w, m) in [(1, 1), (1, 2), (2, 1), (2, 3), (3, 1), (3, 4)] {
                    if v.world(w, "r14 chain report (r14 chain's worlds)") {
                        v.member(w, m, "r14 chain report (r14 chain's members)");
                    }
                }
            }
            Report::R14ForgeConservation { .. } => {
                for (w, m) in [(1, 1), (2, 1), (2, 3), (6, 2)] {
                    if v.world(w, "r14_forge_conservation (r14 forge's worlds, W = 6)") {
                        v.member(w, m, "r14_forge_conservation (r14 forge's members)");
                    }
                }
            }
            Report::R14Conservation { .. } | Report::R14Lag { .. } | Report::R14Forged | Report::R14Jobs => {
                for (w, m) in [(1, 2), (2, 3), (1, 1)] {
                    if v.world(w, "r14 report (r14's worlds)") {
                        v.member(w, m, "r14 report (r14's members)");
                    }
                }
            }
            Report::Ballpit { baseline_ms } => {
                if baseline_ms[1] <= baseline_ms[0] {
                    v.err("ballpit: empty baseline window".into());
                }
                if s.net.count_ms.is_none() {
                    v.err("ballpit: needs net.count_ms (the sweep window)".into());
                }
                for m in [1, 2] {
                    if v.world(1, "ballpit") {
                        v.member(1, m, "ballpit");
                    }
                }
            }
            Report::Phase1Rule { .. } | Report::Acceptance { .. } => {}
        }
    }
    if checks == 0 && s.keep.is_none() {
        v.err("no checking report: add a check_world or verdict, or the scenario can never fail".into());
    }
    if v.errors.is_empty() {
        Ok(())
    } else {
        v.errors.dedup();
        Err(v.errors.join("\n  "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::build;

    const OK: &str = r#"
end_ms = 3000
[world.1]
members = [1, 2, 3]
scene = { pile = { side = 2, layers = 1 } }
epoch_ms = 100
[world.1.plan]
1 = [{ at = 10, touch = { obj = 1, impulse = [0.0, 1.0, 0.0] } }]
[net]
seed = 1
link = { delay_ms = 30 }
[[report]]
kind = "check_world"
frames = 100
label = "x"
"#;

    fn check(extra: &str) -> Result<(), String> {
        let mut v: toml::Value = toml::from_str(OK).unwrap();
        crate::scenario::merge(&mut v, toml::from_str(extra).unwrap());
        let s: Scenario = v.try_into().map_err(|e| e.to_string())?;
        let sim = build(&s)?;
        validate(&s, &sim)
    }

    fn refused(extra: &str, want: &str) {
        match check(extra) {
            Ok(()) => panic!("accepted: {extra}"),
            Err(e) => assert!(e.contains(want), "{extra}: error {e:?} does not mention {want:?}"),
        }
    }

    #[test]
    fn accepts_a_valid_scenario() {
        check("").unwrap();
    }

    #[test]
    fn refuses_meaningless_scenarios() {
        refused("report = [{ kind = \"check_world\", frames = 99999, label = \"x\" }]", "past the last frame");
        refused("byzantine = [{ kind = \"equivocate\", member = 9, entry = \"first\", when = \"exists\", touch = { obj = 1, impulse = [0.0, 1.0, 0.0] }, to = [2] }]", "member 9 is not a member");
        refused("peers = [1, 2]", "member 3 is not in `peers`");
        refused("crash = [{ member = 7, at_ms = 100 }]", "member 7 is not a peer");
        refused("fault = [{ from = 8, drop = true }]", "member 8 is not a peer");
        refused("fault = [{ after_ms = 4000, before_ms = 2000, drop = true }]", "empty window");
        refused("fault = [{ after_ms = 5000, drop = true }]", "after the end of the run");
        refused("crash = [{ member = 3, at_ms = 2000, recover_ms = 1000 }]", "not after at_ms");
        refused("fault = [{ partition = [[1, 2], [2, 3]], drop = true }]", "in two partition groups");
        refused("[net]\nlink = { delay_ms = 30, loss_pct = 150 }", "above 100");
        refused("[world.1]\nhz = 0", "hz must be at least 1");
        refused("end_us = 3000000", "not both");
        refused("[world.1.plan]\n1 = [{ at = 5000, touch = { obj = 1, impulse = [0.0, 1.0, 0.0] } }]", "past the last frame the member reaches");
        refused("[world.1.plan]\n1 = [{ at = 10, touch = { obj = 99999, impulse = [0.0, 1.0, 0.0] } }]", "no object 99999");
        refused("report = [{ kind = \"stats\" }]", "no checking report");
        refused("[net]\ncap = { kbps = 0 }", "kbps must be at least 1");
    }
}
