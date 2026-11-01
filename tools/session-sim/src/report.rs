//! Reports: what a scenario prints after its run, and the probes some of them sample during it.
//! Every format is the spike's (spikes/session-sim main.rs, review2.rs, review3.rs), so the logs
//! compare line for line with the committed ones.
//!
//! Templates (`say` of a Byzantine behaviour, `text` of a `line` report) substitute:
//! `{t_ms}` virtual time; `{conf@M}`, `{frame@M}`, `{now@M}` member M's confirmed, simulated and
//! clock frame; `{member@M:X}` whether M holds X as a member; `{members@M}`, `{evicted@M}`;
//! `{touches@M:A}` A's touches as M holds them; and, for the entry a behaviour acts on,
//! `{x.stamp}`, `{x.seq}`, `{x.target}` (a vote's target), `{x.confirmed@M}`.

use std::collections::{BTreeMap, BTreeSet};

use pe_sim_core::ownership::{test_key_name, Book, Insert, Key, View};
use pe_sim_core::session::*;
use pe_sim_core::{build_pile_scene, GameState, InputError, Interaction, RollbackSim};
use spike::{Acceptance, Replica, Sim, WorldId};

use crate::byzantine::Forged;
use crate::scenario::{Honest, Report, Scenario};
use crate::setup::hz;

pub struct Vars<'a> {
    pub sim: &'a Sim,
    pub world: WorldId,
    pub entry: Option<&'a Entry>,
}

fn member(s: &str) -> MemberId {
    s.parse().unwrap_or_else(|_| panic!("template: `{s}` is not a member id"))
}

/// Substitute `{...}` variables (see the module docs); `{{` and `}}` are literal braces.
pub fn template(t: &str, v: &Vars) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(i) = rest.find(['{', '}']) {
        if rest[i..].starts_with("{{") || rest[i..].starts_with("}}") {
            out.push_str(&rest[..=i]);
            rest = &rest[i + 2..];
            continue;
        }
        if rest[i..].starts_with('}') {
            panic!("template: unmatched `}}` in {t:?}");
        }
        out.push_str(&rest[..i]);
        let j = rest[i..].find('}').map(|j| i + j).unwrap_or_else(|| panic!("template: unclosed `{{` in {t:?}"));
        out.push_str(&var(&rest[i + 1..j], v));
        rest = &rest[j + 1..];
    }
    out.push_str(rest);
    out
}

fn var(name: &str, v: &Vars) -> String {
    let (key, arg) = name.split_once('@').unwrap_or((name, ""));
    let (m, x) = arg.split_once(':').unwrap_or((arg, ""));
    let r = || &v.sim.replica(member(m), v.world).sim;
    let e = || v.entry.unwrap_or_else(|| panic!("template: `{{{name}}}` needs an entry"));
    match key {
        "t_ms" => (v.sim.net.now_us / 1000).to_string(),
        "conf" => r().conf.to_string(),
        "frame" => r().frame.to_string(),
        "now" => r().now.to_string(),
        "member" => r().vcm.members.contains(&member(x)).to_string(),
        "members" => format!("{:?}", r().vcm.members),
        "evicted" => format!("{:?}", r().members_evicted()),
        "touches" => touch_state(v.sim, v.world, member(m), member(x)),
        "x.stamp" => e().stamp.to_string(),
        "x.seq" => e().seq.to_string(),
        "x.target" => match e().payload {
            Payload::Vote { target, .. } => target.to_string(),
            _ => panic!("template: {{x.target}} needs a vote"),
        },
        "x.confirmed" => r().is_confirmed(e().id()).to_string(),
        "proposer" => r().proposer_of(0, 0).map_or("none".to_string(), |p| p.to_string()),
        "role" => format!("{:?}", r().vcm.roles.get(&member(x))),
        "resets" => r().stats.resets.to_string(),
        "member_list" => format!("{:?}", r().vcm.members.iter().copied().collect::<Vec<_>>()),
        "removed" => format!("{:?}", removed_at(v.sim, member(m), member(x))),
        "votes" => format!("{:?}", own_votes(v.sim, member(m))),
        "removed_s" => secs(removed_at(v.sim, member(m), member(x))),
        "strikes" => format!("{:?}", r().vcm.strikes),
        _ => panic!("template: unknown variable `{{{name}}}`"),
    }
}

/// The `check` report's cases.
pub const CHECKS: &[&str] = &[
    "bridge",
    "kickwar",
    "kickpartition",
    "lastadmin",
    "adminclose",
    "joinrules",
    "vcscale",
    "selfkickhead",
    "capchurn",
    "joinattests",
    "slowadmin",
    "kickbackdate",
    "lagprecommit",
    "aheadlink",
    "onesided",
    "removed",
    "strikeframe",
    "strikeframe_total",
    "aheadshield",
    "fakeref",
    "capstale",
    "lagchurn",
    "votechurn",
    "votechurn_off",
    "adminvoid",
    "joinskip",
    "joinwedge",
    "slowref",
    "fakemix",
    "adminfork",
    "prevotefork",
    "prevotefork_off",
    "prevotefork_pf",
    "capfake",
    "fakeadmin",
    "adminwedge",
    "voidfork",
    "slowcapper",
    "voiddeep",
    "plainfork",
    "pfsilent",
];

/// What the run left for the reports besides the simulator.
#[derive(Default)]
pub struct Ctx {
    pub forged: Vec<Forged>,
    pub panic_at_ms: Option<u64>,
    /// When the run started (the wall time `vcscale` reports).
    pub started: Option<std::time::Instant>,
    /// `fork_branch`'s summary.
    pub fork_info: Option<String>,
    /// The members `go_silent` silenced.
    pub silenced: Vec<MemberId>,
    /// `admin_wedge`'s (round-0 proposer, partitioned member, step reached, crash time in us).
    pub wedge: Option<(MemberId, MemberId, u32, Option<u64>)>,
    /// `fork_segment`'s (forker, P, step reached, crash time in us).
    pub segment: Option<(MemberId, MemberId, u32, Option<u64>)>,
}

/// session-sim p24r4.rs `repro_verdict`: a round-7 or round-8 repro's fix-off run must still show
/// the failure it was written for and the shipped run must pass; anything else prints FAIL.
fn repro_verdict(name: &str, fixed: bool, ok: bool, pass: &str, control: &str) {
    let what = if fixed { pass } else { control };
    println!("RESULT {name} fixed={fixed}: -> {}", if ok { what } else { "FAIL" });
}

/// A repro's name and run from its label, "<name> fixed=<b>[ (...)]".
fn name_fixed(label: &str) -> (&str, bool) {
    let (n, f) = label.split_once(" fixed=").unwrap_or_else(|| panic!("check: label `{label}` is not `<name> fixed=<bool>`"));
    (n, f.split(' ').next() == Some("true"))
}

/// Whether every one of `ids` still confirms near its present at the end of the run (session-sim
/// p24r4.rs `live`): a halted member sits at the speculation limit, a whole window past its
/// confirmed frame.
fn live(sim: &Sim, ids: &[MemberId]) -> bool {
    ids.iter().all(|&m| {
        let r = &sim.replica(m, 1).sim;
        r.frame.saturating_sub(r.conf) < r.cfg.window
    })
}

/// The epoch each of `ids` ends in: "m1 1, m2 1, ...".
fn epochs(sim: &Sim, ids: &[MemberId]) -> String {
    ids.iter().map(|&m| format!("m{m} {}", sim.replica(m, 1).sim.vcm.epoch)).collect::<Vec<_>>().join(", ")
}

/// "2, 3, 9, 5".
fn id_list(ids: &[MemberId]) -> String {
    ids.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(", ")
}

/// Finished runs kept by name (a part's `keep`), with when they panicked.
pub type Kept = BTreeMap<String, (Sim, Option<u64>)>;

/// The members a report covers (see [`Honest`]).
pub fn honest(h: &Honest, sim: &Sim) -> Vec<MemberId> {
    match h {
        Honest::List(v) => v.clone(),
        Honest::At { members_at } => sim.replica(*members_at, 1).sim.vcm.members.iter().copied().collect(),
        Honest::Among { members_at, among } => {
            let ms = &sim.replica(*members_at, 1).sim.vcm.members;
            among.iter().copied().filter(|m| ms.contains(m)).collect()
        }
    }
}

/// A report and the samples it collects during the run.
pub struct Probe {
    spec: Report,
    lag: Vec<(u64, u64)>,
    pit: Pit,
    /// Ball pit: the header's parameters, from the scenario.
    pit_params: (u64, usize, u64, u32, usize),
}

#[derive(Default)]
struct Pit {
    prev: Vec<Option<BodyState>>,
    naive_changed: u64,
    naive_frames: u64,
    base_changed: u64,
    base_frames: u64,
    last_frame: u64,
    r2_before: (u64, u64, u64),
    r2_after: (u64, u64, u64),
}

impl Probe {
    pub fn new(spec: Report, s: &Scenario) -> Result<Self, String> {
        let mut pit_params = (0, 0, 0, 0, 0);
        if let Report::Ballpit { .. } = &spec {
            let w = s.world.get("1").ok_or("ballpit report: needs world 1")?;
            let balls = match w.scene {
                crate::scenario::SceneCfg::Ballpit { balls, .. } => balls,
                _ => return Err("ballpit report: world 1 is not a ball pit".into()),
            };
            let sweeps: Vec<u64> = w.plan.values().flatten().filter_map(|a| a.ballpit_sweep.as_ref().map(|x| x.ahead)).collect();
            pit_params = (hz(w), sweeps.len(), sweeps.first().copied().unwrap_or(0), s.net.link.loss_pct, balls);
        }
        if let Report::Phase1Rule { case } = &spec {
            if case != "c1" && case != "c2" {
                return Err(format!("phase1_rule: case must be c1 or c2, not `{case}`"));
            }
        }
        if let Report::Check { case, .. } = &spec {
            if !CHECKS.contains(&case.as_str()) {
                return Err(format!("check: case must be one of {CHECKS:?}, not `{case}`"));
            }
        }
        if let Report::Outcomes { mode, .. } = &spec {
            if mode != "wins" && mode != "tally" {
                return Err(format!("outcomes: mode must be wins or tally, not `{mode}`"));
            }
        }
        let pit = Pit { prev: vec![None; pit_params.4], ..Default::default() };
        Ok(Probe { spec, lag: Vec::new(), pit, pit_params })
    }

    /// Before every 1 ms step (where the spike's loops sampled): `votechurn`'s confirmation lag at
    /// `at`, every 100 ms from 0.5 s.
    pub fn pre_sample(&mut self, sim: &Sim) {
        if let Report::Check { case, at: Some(a), .. } = &self.spec {
            let t = sim.net.now_us;
            if case == "votechurn" && t % 100_000 == 0 && t >= 500_000 {
                let r = &sim.replica(*a, 1).sim;
                self.lag.push((t, r.now.saturating_sub(r.conf)));
            }
        }
    }

    /// After every 1 ms step.
    pub fn sample(&mut self, sim: &Sim) {
        let now = sim.net.now_us;
        match &self.spec {
            Report::R14Lag { .. } => {
                if now % 100_000 == 0 && now > 1_000_000 {
                    let r = sim.replica(2, 1);
                    self.lag.push((now / 1000, r.sim.now.saturating_sub(r.sim.conf)));
                }
            }
            Report::Ballpit { baseline_ms } => {
                let (w0, w1) = (sim.net.count_from_us, sim.net.count_to_us);
                let (b0, b1) = (baseline_ms[0] * 1000, baseline_ms[1] * 1000);
                let p = &mut self.pit;
                let r = sim.replica(1, 1);
                if r.sim.frame != p.last_frame && r.sim.caught_up() {
                    p.last_frame = r.sim.frame;
                    let mut changed = 0;
                    for (i, prev) in p.prev.iter_mut().enumerate() {
                        let s = r.sim.obj_state(i as u32);
                        if s != *prev {
                            changed += 1;
                        }
                        *prev = s;
                    }
                    if (w0..w1).contains(&now) {
                        p.naive_changed += changed;
                        p.naive_frames += 1;
                    }
                    if (b0..b1).contains(&now) {
                        p.base_changed += changed;
                        p.base_frames += 1;
                    }
                }
                let s2 = sim.replica(2, 1).sim.stats;
                if now == w0 {
                    p.r2_before = (s2.rollbacks, s2.resim_frames, s2.resim_us);
                }
                if now == w1 {
                    p.r2_after = (s2.rollbacks, s2.resim_frames, s2.resim_us);
                }
            }
            _ => {}
        }
    }

    pub fn print(&self, sim: &Sim, ctx: &Ctx, kept: &Kept) {
        match &self.spec {
            Report::Handowner { forged, clean, fixoff, member, hand, label } => {
                let get = |k: &String| kept.get(k).unwrap_or_else(|| panic!("handowner: no kept run `{k}`"));
                handowner(get(forged), get(clean), get(fixoff), *member, *hand, label);
            }
            Report::Phase1Rule { case } => phase1_rule(case),
            Report::CheckWorld { world, frames, label, skip, skip_evicted_at } => {
                let mut skip = skip.clone();
                let mut frames = *frames;
                if let Some(at) = skip_evicted_at {
                    let gone: Vec<MemberId> = sim.replica(*at, *world).sim.members_evicted().keys().copied().collect();
                    frames = sim.peers.iter().filter(|p| !gone.contains(&p.id)).filter_map(|p| p.worlds.get(world)).map(|r| r.sim.frame).min().unwrap_or(0).min(frames);
                    skip.extend(gone);
                }
                check_world(sim, *world, frames, label, &skip);
            }
            Report::Ownership { world, frames, label, at } => {
                ownership(sim, *world, *frames, label, at);
            }
            Report::Stats { world } => print_stats(sim, *world),
            Report::Events { world, filter } => {
                for p in &sim.peers {
                    if let Some(r) = p.worlds.get(world) {
                        for e in r.sim.events.iter().filter(|e| e.contains(filter.as_str())) {
                            println!("  event {e}");
                        }
                    }
                }
            }
            Report::Acceptance { label } => acceptance_summary(sim, label),
            Report::Traffic { world } => {
                let (n, b) = sim.net.bytes(*world, "entry");
                let (na, ba) = sim.net.bytes(*world, "ack");
                println!("  bytes: entries {n} msgs {b} B (avg {} B), acks {na} msgs {ba} B, dropped {}", b / n.max(1), sim.net.dropped);
            }
            Report::FirstEntryFrame { world, author, text } => {
                for p in &sim.peers {
                    let Some(r) = p.worlds.get(world) else { continue };
                    let id = r.sim.all_entries().into_iter().find(|e| e.member == *author).map(|e| e.id());
                    println!("  m{} {text}; effective frame {:?}", p.id, id.and_then(|id| r.sim.effective_frame(id)));
                }
            }
            Report::Entries { world, at, author, prefix } => {
                let r = &sim.replica(*at, *world).sim;
                for e in r.all_entries().into_iter().filter(|e| e.member == *author && matches!(e.payload, Payload::Touch { .. })) {
                    println!("  {prefix} entry stamped {} -> effective {:?} (void: {})", e.stamp, r.effective_frame(e.id()), r.void_ids().contains(&e.id()));
                }
            }
            Report::Evicted { world, at, label } => {
                let r = &sim.replica(*at, *world).sim;
                if !r.members_evicted().is_empty() {
                    println!("RESULT {label}: removed from the session (cut, from frame): {:?}", r.members_evicted());
                }
            }
            Report::Outcomes { world, at, mode, prefix } => {
                let r = &sim.replica(*at, *world).sim;
                if mode == "wins" {
                    let mut wins: BTreeMap<MemberId, u32> = BTreeMap::new();
                    for (id, (_, o)) in &r.outcomes {
                        if *o == Outcome::Accepted {
                            if let Some(e) = r.entry(*id) {
                                *wins.entry(e.member).or_default() += 1;
                            }
                        }
                    }
                    println!("{prefix}{wins:?}");
                } else {
                    let mut tally: BTreeMap<(MemberId, &str), u32> = BTreeMap::new();
                    for (id, (_, o)) in &r.outcomes {
                        if let Some(e) = r.entry(*id) {
                            let k = if *o == Outcome::Accepted { "accepted" } else { "rejected" };
                            *tally.entry((e.member, k)).or_default() += 1;
                        }
                    }
                    println!("{prefix}{tally:?}");
                }
            }
            Report::Touches { world, author, at, text } => {
                for &m in at {
                    println!("  {}: {}", text.replace("{m}", &m.to_string()), touch_state(sim, *world, m, *author));
                }
            }
            Report::Verdict { honest: h, label } => {
                verdict(sim, &honest(h, sim), label, ctx.panic_at_ms);
            }
            Report::ViewChanges { honest: h, label } => vc_report(sim, &honest(h, sim), label),
            Report::Admitted { honest, joiner } => {
                for &m in honest {
                    let r = &sim.replica(m, 1).sim;
                    println!("  m{m}: admitted {:?}; {joiner}'s touches: {}", r.vcm.admitted, touch_state(sim, 1, m, *joiner));
                }
            }
            Report::Admin { honest: h, label, outcomes: o, roles } => {
                for m in honest(h, sim) {
                    if *o {
                        println!("RESULT {label}: m{m}: {}", outcomes(sim, m));
                    }
                    if *roles {
                        println!("  {}", roles_line(sim, m));
                    }
                }
            }
            Report::Confs { honest, label } => println!("RESULT {label}: {}", confs(sim, honest)),
            Report::BeforeFix { honest: h, label, fix } => before_fix(sim, &honest(h, sim), label, fix, ctx.panic_at_ms),
            Report::ForkInfo => println!("  {}", ctx.fork_info.as_deref().unwrap_or("no fork_branch behaviour")),
            Report::Check { .. } => check(sim, ctx, kept, &self.spec, &self.lag),
            Report::Membership { honest: h, label } => {
                for m in honest(h, sim) {
                    let r = &sim.replica(m, 1).sim;
                    println!(
                        "RESULT {label}: m{m} members {:?} evicted {:?} conf {} frame {} paused frames {} lying votes found {}",
                        r.vcm.members,
                        r.members_evicted(),
                        r.conf,
                        r.frame,
                        r.stats.paused_frames,
                        r.stats.lying_votes
                    );
                }
            }
            Report::Expect { world, at, conf_max, conf_min, members, label } => {
                let r = &sim.replica(*at, *world).sim;
                let mut bad = Vec::new();
                if conf_max.is_some_and(|m| r.conf > m) {
                    bad.push(format!("conf above {}", conf_max.unwrap()));
                }
                if conf_min.is_some_and(|m| r.conf < m) {
                    bad.push(format!("conf below {}", conf_min.unwrap()));
                }
                if let Some(want) = members {
                    if r.vcm.members.iter().copied().collect::<Vec<_>>() != *want {
                        bad.push(format!("members not {want:?}"));
                    }
                }
                println!(
                    "RESULT {label}: m{at} world {world} conf {} members {:?} evicted {:?} -> {}",
                    r.conf,
                    r.vcm.members,
                    r.members_evicted(),
                    if bad.is_empty() { "OK".to_string() } else { format!("FAIL ({})", bad.join(", ")) }
                );
            }
            Report::Line { world, text } => println!("{}", template(text, &Vars { sim, world: *world, entry: None })),
            Report::R14Forged => r14_forged(sim, &ctx.forged),
            Report::R14Jobs => r14_jobs(sim),
            Report::R14Crash { at } => {
                for &[w, m] in at {
                    let r = &sim.replica(m, w).sim;
                    println!(
                        "RESULT r14 crash: world {w} at m{m}: members {:?} evicted {:?} conf {} frame {} paused frames {}",
                        r.vcm.members,
                        r.members_evicted(),
                        r.conf,
                        r.frame,
                        r.stats.paused_frames
                    );
                }
            }
            Report::R14Conservation { label } => r14_conservation(sim, label),
            Report::R14ForgeConservation { label, original, attacks, control } => r14_forge_conservation(sim, &ctx.forged, label, *original, *attacks, *control),
            Report::R14ChainJobs => r14_chain_jobs(sim),
            Report::R14ChainOutcomes => r14_chain_outcomes(sim),
            Report::R14ChainConservation { label } => r14_chain_conservation(sim, label),
            Report::R14ChainEpoch => r14_chain_epoch(sim),
            Report::R14Lag { label, throttled } => r14_lag(sim, &self.lag, label, *throttled),
            Report::Ballpit { .. } => self.ballpit(sim),
        }
    }

    fn ballpit(&self, sim: &Sim) {
        let (hz, hands, ahead, loss, _) = self.pit_params;
        let p = &self.pit;
        let (w0, w1) = (sim.net.count_from_us, sim.net.count_to_us);
        let secs = (w1 - w0) as f64 / 1e6;
        let (hn, hb) = sim.net.bytes(1, "entry/hand");
        let (an, ab) = sim.net.bytes(1, "ack");
        let (en, eb) = sim.net.bytes(1, "");
        // Wire estimate: one datagram per message with ~52 B of IP/UDP/AEAD/session overhead.
        let wire = |n: u64, b: u64| (b + 52 * n) as f64 / secs;
        let naive_payload = p.naive_changed * (4 + std::mem::size_of::<BodyState>() as u64) + p.naive_frames * 16; // id + exact state
        let naive_quant = p.naive_changed * (2 + 24) + p.naive_frames * 16;
        println!("RESULT ballpit {hz} Hz, {hands} hand(s), input delay {ahead} frames, loss {loss}%: sweep window {secs:.2} s, {} frames", p.naive_frames);
        println!(
            "RESULT   perturbation source (hand target per frame + acks): hand entries {hn} ({:.0} B/s payload, {:.0} B/s wire est.), acks {an} ({:.0} B/s wire est.), all messages {:.0} B/s wire est. (both directions)",
            hb as f64 / secs,
            wire(hn, hb),
            wire(an, ab),
            wire(en, eb)
        );
        println!(
            "RESULT   naive state streaming measured on member 1: avg {:.0} balls changed per frame; exact state {:.0} B/s, quantized ~26 B/ball {:.0} B/s (one direction, payload only)",
            p.naive_changed as f64 / p.naive_frames.max(1) as f64,
            naive_payload as f64 / secs,
            naive_quant as f64 / secs
        );
        println!("RESULT   before the sweep (settled pit, 1.0-1.9 s): avg {:.1} balls changed per frame", p.base_changed as f64 / p.base_frames.max(1) as f64);
        for ((_, k), (n, b)) in sim.net.sent.iter() {
            println!("  sweep-window traffic {k}: {n} messages, {b} B payload ({:.0} B/s wire est.)", wire(*n, *b));
        }
        let (rb, rf, ru) = (p.r2_after.0 - p.r2_before.0, p.r2_after.1 - p.r2_before.1, p.r2_after.2 - p.r2_before.2);
        let s2 = sim.replica(2, 1).sim.stats;
        println!(
            "RESULT   member 2 during the sweep: {:.1} rollbacks/s, {:.1} re-simulated frames/s, re-sim {:.2} ms per rendered sim frame ({:.0} ms over the window); step avg {:.3} ms; max depth {}",
            rb as f64 / secs,
            rf as f64 / secs,
            ru as f64 / 1000.0 / p.naive_frames.max(1) as f64,
            ru as f64 / 1000.0,
            s2.step_us as f64 / 1000.0 / s2.steps.max(1) as f64,
            s2.max_depth
        );
        // End state: the full state hash (every ball, the hands, every table) at the last frame
        // both members have simulated.
        let last = sim.replica(1, 1).sim.frame.min(sim.replica(2, 1).sim.frame) - 1;
        let (h1, h2) = (sim.replica(1, 1).sim.hashes[&last], sim.replica(2, 1).sim.hashes[&last]);
        println!(
            "RESULT   end state at frame {last} (all 500 balls, both hands, all tables): m1 {h1:016x} m2 {h2:016x} -> {}",
            if h1 == h2 { "IDENTICAL" } else { "DIFFERENT" }
        );
    }
}

// ---- P2-4 and D80 (session-sim viewchange.rs, p24r.rs, p24r3.rs), unchanged ------------------

/// The scenario time at which a replica's clock frame `f` began.
fn sim_secs(rep: &Replica, f: u64) -> f64 {
    (rep.epoch_us + f * rep.dt_us) as f64 / 1e6
}

/// Every decision each listed replica took: (epoch reached, clock frame, round, removed).
fn vc_report(sim: &Sim, members: &[MemberId], label: &str) {
    for &m in members {
        let rep = sim.replica(m, 1);
        let r = &rep.sim;
        let at: Vec<String> = r.vcm.vc_log.iter().map(|(e, f, rd, rm)| format!("epoch {e} at frame {f} (t={:.2} s) round {rd} removed {rm:?}", sim_secs(rep, *f))).collect();
        println!(
            "RESULT {label}: m{m} epoch {} members {:?} view changes {} (resets {}, highest round {}) | {}",
            r.vcm.epoch,
            r.vcm.members,
            r.stats.view_changes,
            r.stats.resets,
            r.stats.max_round,
            if at.is_empty() { "no decision".to_string() } else { at.join("; ") }
        );
        if std::env::var("PE_EVENTS").is_ok_and(|v| !v.is_empty()) {
            for e in &r.events {
                println!("  event {e}");
            }
            println!("  vc {}", r.vc_debug());
        }
    }
}

fn confs(sim: &Sim, ids: &[MemberId]) -> String {
    ids.iter()
        .map(|&m| {
            let r = &sim.replica(m, 1).sim;
            format!("m{m} conf {} of frame {}", r.conf, r.frame)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether every listed replica equals the offline replay, with no false check, printing nothing a
/// suite check reads: for the runs with a fix switched off.
fn quiet_equal(sim: &Sim, honest: &[MemberId]) -> bool {
    let frames = honest.iter().map(|&m| sim.replica(m, 1).sim.frame).min().unwrap_or(0).saturating_sub(1);
    let first = &sim.replica(honest[0], 1).sim;
    let members: Vec<MemberId> = first.vcm.members.iter().copied().collect();
    let reference = replay(1, SceneKind::Pile { side: 8, layers: 4 }, first.cfg, first.public_keys(), &members, &final_log(first), frames);
    let rd = digest(&reference, frames);
    let bad: u64 = honest.iter().map(|&m| sim.replica(m, 1).sim.stats.checks_bad).sum();
    honest.iter().all(|&m| digest(&sim.replica(m, 1).sim.hashes, frames) == rd) && bad == 0
}

fn before_fix(sim: &Sim, honest: &[MemberId], label: &str, fix: &str, stopped: Option<u64>) {
    let state = match stopped {
        Some(t) => format!("the run stopped at t={t} ms: a replica hit an assertion or a panic"),
        None if quiet_equal(sim, honest) => "every replica equals the replay".to_string(),
        None => "the replicas do not all equal the replay".to_string(),
    };
    let r = &sim.replica(honest[0], 1).sim;
    println!(
        "  {label} with {fix} switched off (the behaviour before the fix): {state}; {}; at m{}: members {:?}, evicted {:?}",
        confs(sim, honest),
        honest[0],
        r.vcm.members,
        r.members_evicted().keys().collect::<Vec<_>>()
    );
}

fn outcomes(sim: &Sim, m: MemberId) -> String {
    let r = &sim.replica(m, 1).sim;
    let mut v: Vec<(u64, String)> = r
        .vcm.admin_outcomes
        .iter()
        .filter_map(|(id, ok)| {
            let e = r.all_entries().into_iter().find(|e| e.id() == *id)?;
            let what = match e.payload {
                Payload::Kick { target, .. } => format!("{} kicks {target}", e.member),
                Payload::SetRole { member, role, .. } => format!("{} gives {member} role {role}", e.member),
                Payload::DefineRole { role, perms, .. } => format!("{} defines role {role} = {perms:#x}", e.member),
                _ => return None,
            };
            Some((e.stamp, format!("{what} @{}: {}", e.stamp, if *ok { "applied" } else { "refused" })))
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join("; ")
}

fn roles_line(sim: &Sim, m: MemberId) -> String {
    let r = &sim.replica(m, 1).sim;
    format!("m{m}: members {:?} roles {:?} admins {:?} closed at {:?}", r.vcm.members, r.vcm.roles, r.admins(), r.vcm.closed_at)
}

/// When member `m` decided to remove `x` (scenario seconds), if it did.
fn removed_at(sim: &Sim, m: MemberId, x: MemberId) -> Option<f64> {
    let rep = sim.replica(m, 1);
    rep.sim.vcm.vc_log.iter().find(|d| d.3.contains(&x)).map(|d| sim_secs(rep, d.1))
}

/// The targets of member `m`'s own votes (not kick votes), as it holds them.
fn own_votes(sim: &Sim, m: MemberId) -> Vec<MemberId> {
    sim.replica(m, 1)
        .sim
        .all_entries()
        .into_iter()
        .filter(|e| e.member == m)
        .filter_map(|e| match e.payload {
            Payload::Vote { target, kick, .. } if kick == EntryId::NONE => Some(target),
            _ => None,
        })
        .collect()
}

/// The spike's `secs`: a time in seconds, or "never".
fn secs(t: Option<f64>) -> String {
    t.map_or("never".to_string(), |t| format!("{t:.2} s"))
}

fn need<T: Copy>(v: Option<T>, case: &str, field: &str) -> T {
    v.unwrap_or_else(|| panic!("check {case}: needs `{field}`"))
}

/// The spike's one-off checks (see scenario.rs `Report::Check`); `samples` are `votechurn`'s.
fn check(sim: &Sim, ctx: &Ctx, kept: &Kept, spec: &Report, samples: &[(u64, u64)]) {
    let Report::Check { case, label, text, honest, sides, at, members, target, until_ms, role, silent, from_ms, runs, event, expect, conf_min } = spec else { unreachable!() };
    let (case, label, text) = (case.as_str(), label.as_str(), text.as_str());
    let (at, target, until_ms, role, silent, from_ms) = (*at, *target, *until_ms, *role, *silent, *from_ms);
    let events = |m: MemberId, what: &str| sim.replica(m, 1).sim.events.iter().filter(|e| e.contains(what)).count();
    match case {
        "bridge" => {
            let a = sim.replica(sides[0][0], 1).sim.vcm.members.clone();
            let b = sim.replica(sides[1][0], 1).sim.vcm.members.clone();
            println!(
                "RESULT {label}: member lists after the partition healed: m{} {a:?}, m{} {b:?} -> {}",
                sides[0][0],
                sides[1][0],
                if a == b { "ONE SESSION" } else { "FORKED (two sessions)" }
            );
            if a == b {
                verdict(sim, honest, text, ctx.panic_at_ms);
            } else {
                for side in sides {
                    let names: Vec<String> = side.iter().map(|m| m.to_string()).collect();
                    verdict(sim, side, &format!("{label} side {{{}}}", names.join(",")), ctx.panic_at_ms);
                }
            }
        }
        "kickwar" => {
            let left: Vec<MemberId> = sim.replica(need(at, case, "at"), 1).sim.vcm.members.iter().copied().collect();
            let one = left.contains(&members[0]) != left.contains(&members[1]);
            println!("RESULT {label}: members left {left:?}: exactly one of the two admins removed: {one}{}", if one { "" } else { " -> FAIL" });
        }
        "kickpartition" => {
            let rep1 = sim.replica(members[0], 1);
            let heal = need(until_ms, case, "until_ms") as f64 / 1000.0;
            let during = rep1.sim.vcm.vc_log.iter().any(|d| sim_secs(rep1, d.1) < heal);
            let (a, b) = (sim.replica(members[0], 1).sim.vcm.members.clone(), sim.replica(members[1], 1).sim.vcm.members.clone());
            let t = need(target, case, "target");
            println!(
                "RESULT {label}: decided during the partition: {during}; members after: m{} {a:?}, m{} {b:?} -> {}",
                members[0],
                members[1],
                if a == b && !a.contains(&t) && !during { format!("ONE SESSION, {t} removed after the heal") } else { "FAIL".to_string() }
            );
        }
        "lastadmin" => {
            let admins: Vec<Vec<MemberId>> = honest.iter().map(|&m| sim.replica(m, 1).sim.admins()).collect();
            let ok = admins.iter().all(|a| a == members);
            println!("RESULT {label}: admins at the end {admins:?} -> {}", if ok { "the release was refused, the hand-over held" } else { "FAIL" });
        }
        "adminclose" => {
            let closed: Vec<Option<u64>> = honest.iter().map(|&m| sim.replica(m, 1).sim.vcm.closed_at).collect();
            let ok = closed[0].is_some() && closed.iter().all(|c| *c == closed[0]);
            println!(
                "RESULT {label}: {text}; session closed at frame {:?} at m{}, {:?} at m{} -> {}",
                closed[0],
                honest[0],
                closed[1],
                honest[1],
                if ok { "CLOSED, the same frame everywhere" } else { "FAIL" }
            );
        }
        "joinrules" => {
            let [friend, stranger, confidant, owner] = members[..] else { panic!("check joinrules: members = [friend, stranger, confidant, owner]") };
            let guest = need(role, case, "role");
            let admitted: Vec<MemberId> = sim.replica(1, 1).sim.vcm.members.iter().copied().collect();
            let roles = &sim.replica(1, 1).sim.vcm.roles;
            let ok = roles.get(&friend) == Some(&guest) && roles.get(&confidant) == Some(&ADMIN) && roles.get(&owner) == Some(&ADMIN) && !admitted.contains(&stranger);
            println!(
                "RESULT {label}: admitted {admitted:?}, roles {roles:?} -> {}",
                if ok { "friend of an admin as Guest, owner's confidant as Admin, owner by row 0, stranger refused" } else { "FAIL" }
            );
        }
        "vcscale" => {
            let silent = need(silent, case, "silent");
            let from_s = need(from_ms, case, "from_ms") as f64 / 1000.0;
            // Every peer is a genesis member here, in id order (the spike's 1..=n).
            let ids: Vec<MemberId> = sim.peers.iter().map(|p| p.id).collect();
            let n = ids.len();
            let heads = sim.replica(ids[0], 1).sim.cfg.ack_heads;
            let gone: BTreeSet<MemberId> = ids.iter().rev().take(silent as usize).copied().collect();
            let live: Vec<MemberId> = ids.iter().copied().filter(|m| !gone.contains(m)).collect();
            let mut at: Vec<f64> = Vec::new();
            let mut undecided = 0;
            let mut members_after = BTreeSet::new();
            for &m in &live {
                let rep = sim.replica(m, 1);
                let r = &rep.sim;
                match r.vcm.vc_log.iter().find(|d| d.3.iter().any(|x| gone.contains(x))) {
                    Some(d) => at.push(sim_secs(rep, d.1) - from_s),
                    None => undecided += 1,
                }
                members_after.insert(r.vcm.members.len());
            }
            at.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let r1 = &sim.replica(live[0], 1).sim;
            println!(
                "RESULT vcscale n={n} silent={silent} heads={heads}: {} of {} live members decided the removal; seconds after the silence began: min {:.2} median {:.2} max {:.2}; undecided {undecided}; member counts after {members_after:?}; m{} decisions {:?}; conf {} | {} | wall {:.1} s",
                at.len(),
                live.len(),
                at.first().copied().unwrap_or(f64::NAN),
                at.get(at.len() / 2).copied().unwrap_or(f64::NAN),
                at.last().copied().unwrap_or(f64::NAN),
                live[0],
                r1.vcm.vc_log,
                r1.conf,
                if ctx.panic_at_ms.is_some() { "PANIC" } else { "no panic" },
                ctx.started.map_or(0.0, |t| t.elapsed().as_secs_f64())
            );
        }
        "selfkickhead" => {
            let removed = removed_at(sim, need(at, case, "at"), need(target, case, "target"));
            println!(
                "RESULT {label}: {} removed at {} -> {}",
                need(target, case, "target"),
                removed.map_or("never".to_string(), |t| format!("{t:.2} s")),
                if removed.is_some() { "KICKED" } else { "FAIL" }
            );
        }
        "capchurn" => {
            let (at, t) = (need(at, case, "at"), need(target, case, "target"));
            let removed = removed_at(sim, at, t);
            println!(
                "RESULT {label}: {} resets, {t} removed at {} -> {}",
                sim.replica(at, 1).sim.stats.resets,
                removed.map_or("never".to_string(), |t| format!("{t:.2} s")),
                if removed.is_some() { "VOTED OUT" } else { "FAIL" }
            );
        }
        "joinattests" => {
            let (at, j, guest) = (need(at, case, "at"), need(target, case, "target"), role);
            let role = sim.replica(at, 1).sim.vcm.roles.get(&j).copied();
            println!(
                "RESULT {label}: {j}'s role at m{at} {role:?} -> {}; {}",
                if role == Some(need(guest, case, "role")) { "ADMITTED AS GUEST" } else { "FAIL" },
                confs(sim, honest)
            );
        }
        "slowadmin" => {
            let defs: Vec<String> = honest.iter().map(|&m| format!("m{m} {:?}", sim.replica(m, 1).sim.vcm.role_defs)).collect();
            println!("RESULT {label}: role definitions {}", defs.join(", "));
        }
        "kickbackdate" => {
            let left: Vec<MemberId> = sim.replica(need(at, case, "at"), 1).sim.vcm.members.iter().copied().collect();
            let ok = left == *members;
            println!("RESULT {label}: members left {left:?} -> {}", if ok { "THE BACKDATED KICK IS VOID: 2 removed" } else { "FAIL" });
        }
        "lagprecommit" => {
            let admitted = sim.replica(need(at, case, "at"), 1).sim.vcm.members.contains(&need(target, case, "target"));
            println!("RESULT {label}: joiner admitted {admitted} -> {}; {}", if admitted { "JOINED" } else { "FAIL" }, confs(sim, honest));
        }
        "aheadlink" => {
            // Its votes are cast in an epoch the others have left, and an epoch decides once, so none
            // of the members ahead is removed.
            let a = need(at, case, "at");
            let votes: Vec<(u32, MemberId)> = sim
                .replica(a, 1)
                .sim
                .all_entries()
                .into_iter()
                .filter(|e| e.member == a)
                .filter_map(|e| match e.payload {
                    Payload::Vote { epoch, target, kick: EntryId::NONE, .. } => Some((epoch, target)),
                    _ => None,
                })
                .collect();
            let ms = sim.replica(honest[0], 1).sim.vcm.members.clone();
            let kept = members.iter().all(|m| ms.contains(m));
            println!(
                "RESULT {label}: {a}'s votes against the members ahead, by (epoch, target): {votes:?}; members {ms:?} -> {}",
                if kept && !votes.is_empty() { "CAST, AND THE MEMBERS AHEAD ARE KEPT" } else { "FAIL" }
            );
        }
        "removed" => {
            let t = need(target, case, "target");
            let removed = removed_at(sim, need(at, case, "at"), t);
            println!("RESULT {label}: {t} removed at {} -> {}", secs(removed), if removed.is_some() { text } else { "FAIL" });
        }
        "onesided" => {
            let first = |m: MemberId| sim.replica(m, 1).sim.vcm.vc_log.first().map(|d| d.3.clone());
            let (a, b) = (first(members[0]), first(members[1]));
            println!(
                "RESULT onesided (control: 3 of 6 collude, above t(6) = 1): epoch 1 removed {a:?} at m{}, {b:?} at m{} -> {}{}",
                members[0],
                members[1],
                if a.is_some() && b.is_some() && a != b { "two different epoch-1 decisions: the session forked" } else { "no fork in this run" },
                match ctx.panic_at_ms {
                    Some(t) => format!("; a replica stopped at t={t} ms"),
                    None => String::new(),
                }
            );
        }
        "strikeframe" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            let r = &sim.replica(a, 1).sim;
            let ok = r.vcm.members.contains(&members[0]);
            println!(
                "RESULT strikeframe {text}: at m{a} members {:?}, resets {}, {t} removed at {} -> {}",
                r.vcm.members,
                r.stats.resets,
                secs(removed_at(sim, a, t)),
                if ok { format!("{} KEPT", members[0]) } else { "FAIL".to_string() }
            );
        }
        "strikeframe_total" => {
            let a = need(at, case, "at");
            let kept_n = runs.iter().filter(|k| kept.get(*k).unwrap_or_else(|| panic!("check: no kept run `{k}`")).0.replica(a, 1).sim.vcm.members.contains(&members[0])).count();
            println!("RESULT strikeframe: {} kept in {kept_n} of {} timings -> {}", members[0], runs.len(), if kept_n == runs.len() { "OK" } else { "FAIL" });
        }
        "aheadshield" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            let removed = removed_at(sim, a, t);
            let voted_out = expect.as_deref() == Some("voted_out");
            let ok = if voted_out { removed.is_some() } else { removed.is_none() && sim.replica(a, 1).sim.conf > need(*conf_min, case, "conf_min") };
            println!(
                "RESULT aheadshield {text}: {t} removed at {}; {} -> {}",
                secs(removed),
                confs(sim, honest),
                match (ok, voted_out) {
                    (false, _) => "FAIL",
                    (true, true) => "VOTED OUT",
                    (true, false) => "HARMLESS",
                }
            );
        }
        "fakeref" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            let removed = removed_at(sim, a, t);
            let n = events(a, event.as_deref().unwrap_or(""));
            println!(
                "RESULT {label}: {t} removed at {} ({n} withholding vote at m{a}) -> {}; {}",
                secs(removed),
                if removed.is_some() { "VOTED OUT" } else { "FAIL" },
                confs(sim, honest)
            );
        }
        "capstale" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            let removed = removed_at(sim, a, t);
            let resets = sim.replica(a, 1).sim.stats.resets;
            let n = events(a, event.as_deref().unwrap_or(""));
            println!(
                "RESULT {label}: {resets} resets, {t} removed at {} ({n} cap vote at m{a}) -> {}; {}",
                secs(removed),
                if removed.is_some() && resets == 0 { "VOTED OUT, NO RESET" } else { "FAIL" },
                confs(sim, honest)
            );
        }
        "lagchurn" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            let removed = removed_at(sim, a, t);
            let resets = sim.replica(a, 1).sim.stats.resets;
            let n = events(a, event.as_deref().unwrap_or(""));
            println!(
                "RESULT {label}: {resets} resets, {t} removed at {} ({n} demotion vote at m{a}) -> {}; {}",
                secs(removed),
                if removed.is_some() && n > 0 { "DEMOTED" } else { "FAIL" },
                confs(sim, honest)
            );
        }
        "votechurn" | "votechurn_off" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            let (from, until) = (need(from_ms, case, "from_ms") as f64 / 1000.0, need(until_ms, case, "until_ms") as f64 / 1000.0);
            let r1 = sim.replica(a, 1);
            let removed = removed_at(sim, a, t);
            let during = r1.sim.vcm.vc_log.iter().filter(|d| (from..until).contains(&sim_secs(r1, d.1)) && d.3.is_empty()).count();
            if case == "votechurn_off" {
                println!("  with round 3's strikes: {t} removed at {}; {during} resets during the churn", secs(removed));
                return;
            }
            let after = r1.sim.vcm.vc_log.iter().filter(|d| sim_secs(r1, d.1) > until + 0.5 && d.3.is_empty()).count();
            // The samples: (time, lag), bucketed before, during and from 2 s after the churn.
            let (f_us, u_us) = ((from * 1e6) as u64, (until * 1e6) as u64);
            let mut lag: [Vec<u64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
            for &(t_us, l) in samples {
                let k = if t_us < f_us { 0 } else if t_us < u_us { 1 } else if t_us >= u_us + 2_000_000 { 2 } else { continue };
                lag[k].push(l);
            }
            let mm = |mut v: Vec<u64>| {
                v.sort_unstable();
                (v.get(v.len() / 2).copied().unwrap_or(0), v.last().copied().unwrap_or(0))
            };
            let [b, d, af] = lag.map(mm);
            println!(
                "RESULT votechurn: {during} resets in the {:.0} s of churn ({:.1} per s); m{a}'s confirmation lag in frames, median/max: {}/{} before, {}/{} during, {}/{} after; {t} kicked at {}, {after} resets after -> {}",
                until - from,
                during as f64 / (until - from),
                b.0,
                b.1,
                d.0,
                d.1,
                af.0,
                af.1,
                secs(removed),
                if removed.is_some() && after == 0 { "THE KICK ENDS IT" } else { "FAIL" }
            );
        }
        "adminvoid" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            let role = sim.replica(a, 1).sim.vcm.roles.get(&t).copied();
            println!(
                "RESULT {label}: {t}'s role at m{a} {role:?} -> {}; {}",
                if role == Some(ADMIN) { "THE VOID ENTRY IS NOT APPLIED" } else { "FAIL" },
                confs(sim, honest)
            );
        }
        "joinskip" => {
            let (a, j) = (need(at, case, "at"), need(target, case, "target"));
            let ms = sim.replica(a, 1).sim.vcm.members.clone();
            println!("RESULT {label}: members at m{a} {ms:?} -> {}; {}", if ms.contains(&j) { format!("{j} STAYS") } else { "FAIL".to_string() }, confs(sim, honest));
        }
        "joinwedge" => {
            let (a, t, j) = (need(at, case, "at"), need(target, case, "target"), members[0]);
            let admitted = sim.replica(a, 1).sim.vcm.members.contains(&j);
            println!(
                "RESULT {label}: {j} admitted {admitted}, {t} removed at {} -> {}; {}",
                secs(removed_at(sim, a, t)),
                if admitted { "ADMITTED" } else { "FAIL" },
                confs(sim, honest)
            );
        }
        "slowref" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            let removed = removed_at(sim, a, t);
            println!("RESULT {label}: {t} removed at {} -> {}; {}", secs(removed), if removed.is_some() { "DEMOTED" } else { "FAIL" }, confs(sim, honest));
        }
        "fakemix" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            let n = events(a, event.as_deref().unwrap_or(""));
            println!("REPRO {label}: {t} removed at {}; withholding votes at m{a} {n}; {}", secs(removed_at(sim, a, t)), confs(sim, honest));
        }
        "adminfork" => {
            let (a, t, x) = (need(at, case, "at"), need(target, case, "target"), members[0]);
            println!(
                "REPRO {label}: {t} crashed {}; {t} removed at {}; {x}'s role at m{a} {:?}; {}",
                ctx.silenced.contains(&t),
                secs(removed_at(sim, a, t)),
                sim.replica(a, 1).sim.vcm.roles.get(&x).copied(),
                confs(sim, honest)
            );
        }
        "prevotefork" | "prevotefork_off" => {
            let (obs, capper) = (need(at, case, "at"), need(target, case, "target"));
            let voted = events(obs, event.as_deref().unwrap_or("")) > 0;
            if case == "prevotefork_off" {
                println!("  m{obs} votes to remove capper {capper}: {voted}");
            } else {
                // Round 7: liveness of the members other than the faulty proposer (`honest`).
                let kept = sim.replica(obs, 1).sim.vcm.members.contains(&capper) && sim.replica(capper, 1).sim.vcm.members.contains(&capper);
                let ok = live(sim, honest);
                println!(
                    "RESULT {label}: m{obs} votes to remove capper {capper}: {voted}; {capper} kept: {kept}; epochs at the end: {}; confirmation live at the end ({}): {ok} -> {}",
                    epochs(sim, members),
                    id_list(honest),
                    if !voted && kept && ok { "THE CAP IS JUSTIFIED BY THE FETCHED PREVOTE, AND CONFIRMATION GOES ON" } else { "FAIL" }
                );
            }
        }
        "prevotefork_pf" => {
            println!("  epochs at the end: {}; confirmation live at the end ({}): {}", epochs(sim, members), id_list(honest), live(sim, honest));
        }
        "capfake" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            println!("REPRO {label}: {t} removed at {}; live at the end: {}; {}", secs(removed_at(sim, a, t)), live(sim, honest), confs(sim, honest));
            let (name, fixed) = name_fixed(label);
            let (gone, up) = (removed_at(sim, a, t).is_some(), live(sim, honest));
            repro_verdict(name, fixed, if fixed { gone && up } else { !gone && !up }, "VOTED OUT, CONFIRMATION GOES ON", "HALTS, AS BEFORE THE FIX");
        }
        "fakeadmin" => {
            let (a, t) = (need(at, case, "at"), need(target, case, "target"));
            let vcs = sim.replica(a, 1).sim.vcm.vc_log.len();
            println!("REPRO {label}: {t} removed at {}; decisions at m{a} {vcs}; live at the end: {}; {}", secs(removed_at(sim, a, t)), live(sim, honest), confs(sim, honest));
            let (name, fixed) = name_fixed(label);
            let (gone, up) = (removed_at(sim, a, t).is_some(), live(sim, honest));
            repro_verdict(name, fixed, if fixed { gone && up } else { !gone && !up }, "VOTED OUT, CONFIRMATION GOES ON", "HALTS, AS BEFORE THE FIX");
        }
        "adminwedge" => {
            let t = need(target, case, "target");
            let (p0, part, step, crash_at) = ctx.wedge.expect("adminwedge: validated to have an admin_wedge behaviour");
            println!(
                "REPRO {label}: proposer(0,0)={p0}, partitioned {part}; step reached {step}, {t} crashed at {crash_at:?} us; {t} removed at {}; live at the end: {}; {}",
                secs(removed_at(sim, 1, t)),
                live(sim, honest),
                confs(sim, honest)
            );
            let (name, fixed) = name_fixed(label);
            let (gone, up) = (removed_at(sim, 1, t).is_some(), live(sim, honest));
            repro_verdict(name, fixed, if fixed { gone && up } else { !gone && !up }, "REMOVED, CONFIRMATION GOES ON", "HALTS, AS BEFORE THE FIX");
        }
        "voidfork" | "plainfork" | "voiddeep" => {
            // session-sim p24r4.rs `voidfork`, `plainfork` and `voiddeep` (over `fork_segment_sim`).
            let (f, p, step, crash_at) = ctx.segment.expect("validated to have a fork_segment behaviour");
            let (gone, up) = (removed_at(sim, honest[0], f).is_some(), live(sim, honest));
            let removed = secs(removed_at(sim, honest[0], f));
            match case {
                "voidfork" => {
                    println!(
                        "REPRO {label}: proposer(0,0)={f}, linked the void version first: {p}; step reached {step}, {f} crashed at {crash_at:?} us; {f} removed at {removed}; live at the end: {up}; {}",
                        confs(sim, honest)
                    );
                    let (name, fixed) = name_fixed(label);
                    repro_verdict(name, fixed, if fixed { gone && up } else { !gone && !up }, "THE VOID LISTING IS REFUSED EVERYWHERE, CONFIRMATION GOES ON", "HALTS, AS BEFORE THE FIX");
                }
                "plainfork" => {
                    println!("REPRO {label}: f={f}, P={p}; {f} removed at {removed}; live at the end: {up}; {}", confs(sim, honest));
                    let (name, fixed) = name_fixed(label);
                    repro_verdict(name, fixed, if fixed { gone && up } else { !gone && !up }, "THE FORKER IS REMOVED, CONFIRMATION GOES ON", "HALTS, AS BEFORE THE FIX");
                }
                _ => {
                    // The label is the spike's "voiddeep <void|valid> fixes_off=<hex>".
                    let guest = need(role, case, "role");
                    let demoted = honest.iter().filter(|&&m| m != p).all(|&m| sim.replica(m, 1).sim.vcm.roles.get(&p) == Some(&guest));
                    println!("REPRO {label}: f={f}, P={p}; {f} removed at {removed}; P demoted: {demoted}; live at the end: {up}; {}", confs(sim, honest));
                    let fixed = label.ends_with("fixes_off=0x0");
                    if label.starts_with("voiddeep valid") {
                        repro_verdict("voiddeep valid", true, gone && up && demoted, "THE LISTING, VALID ON ITS OWN ANCESTRY, IS DECIDED EVERYWHERE", "");
                    } else if fixed {
                        repro_verdict("voiddeep void", true, gone && up && !demoted, "THE VOID LISTING IS REFUSED EVERYWHERE, CONFIRMATION GOES ON", "");
                    } else {
                        repro_verdict("voiddeep void", false, !gone && !up, "", "HALTS, AS BEFORE THE FIX");
                    }
                }
            }
        }
        "pfsilent" => {
            // session-sim p24r4.rs `pfsilent`: epochs of `members`, liveness of all of them but the
            // forker (`target`), confirmed frames of `honest`.
            let forker = need(target, case, "target");
            let remaining: Vec<MemberId> = members.iter().copied().filter(|&m| m != forker).collect();
            let up = live(sim, &remaining);
            println!("REPRO {label} (9 silent from 1 s): epochs at the end: {}; live at the end ({}): {up}; {}", epochs(sim, members), id_list(&remaining), confs(sim, honest));
            let (name, fixed) = name_fixed(label);
            repro_verdict(name, fixed, if fixed { up } else { !up }, "EVERY MEMBER DECIDES ON THE FORKER'S PRECOMMIT, CONFIRMATION GOES ON", "HALTS, AS BEFORE THE FIX");
        }
        "slowcapper" => {
            // session-sim p24r4.rs `slowcapper`: observer `at`, capper `target`, liveness of
            // `members`, confirmed frames of `honest`.
            let (obs, capper) = (need(at, case, "at"), need(target, case, "target"));
            let voted = sim.replica(obs, 1).sim.all_entries().iter().any(|e| e.member == obs && matches!(e.payload, Payload::Vote { target, .. } if target == capper));
            let kept = sim.replica(obs, 1).sim.vcm.members.contains(&capper);
            let up = live(sim, members);
            println!(
                "REPRO {label} (3 receives the cap's polka prevote 1.5 s late): m{obs} votes to remove capper {capper}: {voted}; {capper} kept: {kept}; live at the end ({}): {up}; {}",
                id_list(members),
                confs(sim, honest)
            );
            let (name, fixed) = name_fixed(label);
            repro_verdict(name, fixed, if fixed { !voted && kept && up } else { voted }, "THE SLOW CAPPER IS KEPT, CONFIRMATION GOES ON", "3 VOTES AGAINST THE HONEST CAPPER, AS BEFORE THE FIX");
        }
        _ => panic!("check: unknown case `{case}`"),
    }
}

/// Review HO-1 (session-sim main.rs `handowner`, after its three runs), unchanged but for its
/// parameters: `a` with `member`'s forgeries of hand `hand`, `b` without them, `c` with them and
/// HO-1's fix switched off.
fn handowner((a, pa): &(Sim, Option<u64>), (b, pb): &(Sim, Option<u64>), (c, pc): &(Sim, Option<u64>), member: MemberId, h1: u32, label: &str) {
    let ids = [1, 2];
    // Only frames every compared replica has confirmed: past its confirmed frame a replica may still
    // roll back.
    let lowest = |ss: [&Sim; 2]| ss.iter().flat_map(|s| ids.map(|m| s.replica(m, 1).sim.frame.saturating_sub(1).min(s.replica(m, 1).sim.conf))).min().unwrap_or(0);
    let frames = lowest([a, b]);
    let mut all_refused = true;
    for m in ids {
        let r = &a.replica(m, 1).sim;
        let forged: Vec<EntryId> =
            r.all_entries().iter().filter(|e| e.member == member && matches!(e.payload, Payload::Hand { obj, .. } if obj == h1)).map(|e| e.id()).collect();
        let refused = forged.iter().filter(|&&id| r.outcomes.get(&id).is_some_and(|(_, o)| *o == Outcome::RejectedNotOwner)).count();
        all_refused &= refused == forged.len() && !forged.is_empty();
        println!("  m{m}: member {member}'s hand entries for member 1's hand: {}, refused {refused}", forged.len());
        // Review HO-1: the grab and the carry are refused too, and the grab took no claim.
        let others: Vec<(String, Option<Outcome>)> = r
            .all_entries()
            .iter()
            .filter(|e| e.member == member && matches!(e.payload, Payload::Touch { obj, .. } | Payload::Depart { obj, .. } if obj == h1))
            .map(|e| (if matches!(e.payload, Payload::Touch { .. }) { "grab".to_string() } else { "carry".to_string() }, r.outcomes.get(&e.id()).map(|(_, o)| *o)))
            .collect();
        let own = r.all_entries().iter().find(|e| e.member == 1 && matches!(e.payload, Payload::Touch { obj, .. } if obj == h1)).and_then(|e| r.outcomes.get(&e.id()).map(|(_, o)| *o));
        all_refused &= others.len() == 2 && others.iter().all(|(_, o)| *o == Some(Outcome::RejectedNotOwner)) && own == Some(Outcome::Accepted);
        println!("  m{m}: member {member}'s grab and carry of member 1's hand: {others:?}; member 1's own touch of it: {own:?}");
        let planted: Vec<Option<Outcome>> =
            r.all_entries().iter().filter(|e| e.member == member && e.carried.is_some_and(|(o, _)| o == h1)).map(|e| r.outcomes.get(&e.id()).map(|(_, o)| *o)).collect();
        all_refused &= planted.len() == 1 && planted[0] == Some(Outcome::RejectedInvalid);
        println!("  m{m}: member {member}'s touch of a ball carrying a state for member 1's hand: {planted:?}");
    }
    let frames_c = lowest([c, b]);
    let (dc, dbc) = (digest(&c.replica(1, 1).sim.hashes, frames_c), digest(&b.replica(1, 1).sim.hashes, frames_c));
    println!(
        "  handowner with HO-1's round-2 fix switched off (the behaviour before the fix): {}{}",
        if dc == dbc { "same history as without member 2's entries" } else { "member 1's history differs from the run without member 2's entries" },
        if pc.is_some() { "; a replica stopped" } else { "" }
    );
    let (da, db) = (digest(&a.replica(1, 1).sim.hashes, frames), digest(&b.replica(1, 1).sim.hashes, frames));
    println!(
        "RESULT handowner: member {member}'s entries for member 1's hand refused at every replica: {all_refused}; frames 0..{frames} with them {da:016x}, without them {db:016x} -> {}",
        if all_refused && da == db { "SAME HISTORY" } else { "FAIL" }
    );
    if pa.is_some() || pb.is_some() {
        println!("RESULT handowner: PANIC");
    }
    check_world(a, 1, frames, label, &[]);
}

// ---- the spike's checks, unchanged ------------------------------------------------------------

/// Every replica of world `w` (except `skip`) agrees with every other and with an offline replay
/// of its final log.
pub fn check_world(sim: &Sim, w: WorldId, frames: u64, label: &str, skip: &[MemberId]) -> bool {
    let mut digests = Vec::new();
    let mut first: Option<&Replica> = None;
    for p in sim.peers.iter().filter(|p| !skip.contains(&p.id)) {
        if let Some(r) = p.worlds.get(&w) {
            digests.push((p.id, digest(&r.sim.hashes, frames)));
            first.get_or_insert(r);
        }
    }
    if std::env::var("PE_EVENTS").is_ok() {
        for p in &sim.peers {
            if let Some(r) = p.worlds.get(&w) {
                for e in &r.sim.events {
                    println!("  event {e}");
                }
            }
        }
    }
    let r = first.unwrap();
    let members: Vec<MemberId> = r.sim.vcm.members.iter().copied().collect();
    let reference = replay_sim_like(&r.sim, r.scene, &members, &final_log(&r.sim), frames).hashes;
    let rd = digest(&reference, frames);
    let all_equal = digests.iter().all(|(_, d)| *d == rd);
    if !all_equal {
        if let Some(f) = (0..frames).find(|f| reference.get(f) != r.sim.hashes.get(f)) {
            let log = final_log(&r.sim);
            let near: Vec<String> =
                log.iter().filter(|(_, ef)| *ef + 3 >= f && *ef <= f + 1).map(|(e, ef)| format!("m{}s{}@{} eff {ef}", e.member, e.seq, e.stamp)).collect();
            println!("  first replay mismatch at frame {f}; final-log entries near it: {}", near.join(", "));
            let rs = replay_sim_like(&r.sim, r.scene, &members, &log, f + 1);
            for (e, _) in log.iter().filter(|(_, ef)| *ef == f) {
                println!("    m{}s{}: live {:?} replay {:?} carried {:?}", e.member, e.seq, r.sim.outcomes.get(&e.id()), rs.outcomes.get(&e.id()), e.carried.map(|c| c.0));
            }
        }
    }
    println!(
        "RESULT {label} world {w}: frames 0..{frames} digests {} | offline replay {rd:016x} | {}",
        digests.iter().map(|(m, d)| format!("m{m}={d:016x}")).collect::<Vec<_>>().join(" "),
        if all_equal { "ALL EQUAL" } else { "MISMATCH" }
    );
    // Frames past a replica's confirmed frame are still speculative there: a correct run may
    // legitimately differ in them, so a check must not reach them (review of fffdcbc).
    let conf = sim.peers.iter().filter(|p| !skip.contains(&p.id)).filter_map(|p| p.worlds.get(&w)).map(|r| r.sim.conf).min().unwrap_or(0);
    if frames > conf {
        println!("  UNCONFIRMED: {label} checks frames {conf}..{frames}, past the lowest confirmed frame of the checked replicas ({conf})");
    }
    all_equal
}

/// Keys as the scenario names them (test identities, by number), sorted by number.
fn key_names<'a>(ks: impl IntoIterator<Item = &'a Key>) -> String {
    let mut known: Vec<u32> = Vec::new();
    let mut other: Vec<String> = Vec::new();
    for k in ks {
        match test_key_name(*k) {
            Some(n) => known.push(n),
            None => other.push(format!("{k:?}")),
        }
    }
    known.sort_unstable();
    let mut v: Vec<String> = known.iter().map(|n| n.to_string()).collect();
    v.extend(other);
    format!("{{{}}}", v.join(", "))
}

fn view_line(v: &View) -> String {
    let o = &v.owners;
    let recovery = || {
        let mut r: Vec<String> = o.recovery.iter().map(|(k, r)| format!("{}: {}", key_names([k]), key_names(r.keys()))).collect();
        r.sort();
        format!(" recovery [{}]", r.join(", "))
    };
    format!(
        "owners {} rule {:?} heads {}{}{}",
        key_names(&o.owners),
        o.rule,
        v.heads.len(),
        if v.disputed { " DISPUTED" } else { "" },
        if o.recovery.is_empty() { String::new() } else { recovery() }
    )
}

/// World ownership (12.5, on the D80 admin layer): every replica's view of the ownership chain and
/// its outcomes for the certificate entries, against an offline reference: a fresh book from the
/// genesis certificate, with the certificates the first replica's decided view changes list applied
/// in their commit order. Also each entry no decision listed, the fork proofs (each checked as a
/// third party holding only the chain up to the fork would check it) and, with `at`, the
/// reference's view after the decisions with boundaries up to each frame.
pub fn ownership(sim: &Sim, w: WorldId, frames: u64, label: &str, at: &[u64]) -> bool {
    let replicas: Vec<(u32, &Replica)> = sim.peers.iter().filter_map(|p| p.worlds.get(&w).map(|r| (p.id, r))).collect();
    let r = replicas[0].1;
    let b0 = r.sim.vcm.book.as_ref().expect("a world with an ownership chain");
    let mut rb = Book::new(b0.cert(b0.genesis).unwrap().clone()).unwrap();
    let mut listed = BTreeMap::new();
    let mut agree = true;
    let mut after: Vec<(u64, Book)> = Vec::new();
    let mut lines = Vec::new();
    for c in r.sim.decided_changes() {
        let mut any = false;
        for &(id, ok) in &c.admin {
            let Some(e) = r.sim.entry(id) else { continue };
            let Payload::Ownership { cert, .. } = &e.payload else { continue };
            let res = rb.commit_local(cert.clone(), r.sim.world_id, c.boundary);
            let applied = matches!(res, Ok(Insert::Applied));
            agree &= applied == ok;
            listed.insert(id, ok);
            any = true;
            lines.push(format!(
                "  epoch {} boundary {}: cert m{}s{} stamped {}: seq {} {:?} by {} -> {}{}",
                c.epoch + 1,
                c.boundary,
                e.member,
                e.seq,
                e.stamp,
                cert.seq,
                cert.change,
                key_names(&cert.signers()),
                if ok { "applied".to_string() } else { "refused".to_string() },
                if applied == ok { String::new() } else { format!(" (the reference says {res:?}: MISMATCH)") }
            ));
        }
        if any {
            after.push((c.boundary, rb.clone()));
        }
    }
    let mut all_equal = agree;
    let mut views = Vec::new();
    for (m, x) in &replicas {
        let b = x.sim.vcm.book.as_ref().unwrap();
        let outcomes: BTreeMap<EntryId, bool> =
            x.sim.vcm.admin_outcomes.iter().filter(|(id, _)| matches!(x.sim.entry(**id).map(|e| &e.payload), Some(Payload::Ownership { .. }))).map(|(a, b)| (*a, *b)).collect();
        all_equal &= b.view() == rb.view() && b.digest() == rb.digest() && outcomes == listed;
        views.push((*m, b.view(), b.digest()));
    }
    println!("RESULT {label} world {w}: offline reference {} | {}", view_line(&rb.view()), if all_equal { "ALL EQUAL" } else { "MISMATCH" });
    for (m, v, d) in &views {
        println!("  m{m}: {} digest {d:016x}", view_line(v));
    }
    for l in lines {
        println!("{l}");
    }
    // An entry no decision listed was never pending, or a decision made it unable to apply (12.5):
    // the reason shown is the reference book's, as decided before its stamp.
    let genesis = Book::new(b0.cert(b0.genesis).unwrap().clone()).unwrap();
    let book_at = |f: u64| after.iter().rev().find(|(b, _)| *b <= f).map_or(&genesis, |(_, b)| b);
    let mut log = r.sim.all_entries();
    log.sort_by_key(|e| (e.stamp, e.member, e.seq));
    for e in &log {
        if let Payload::Ownership { cert, .. } = &e.payload
            && !listed.contains_key(&e.id())
        {
            let why = match book_at(e.stamp).clone().commit_local(cert.clone(), r.sim.world_id, e.stamp) {
                Err(err) => format!("{err:?}"),
                Ok(i) => format!("{i:?} then, unable to apply after a later decision"),
            };
            println!("  cert m{}s{} stamped {}: seq {} {:?} by {} -> never committed ({why})", e.member, e.seq, e.stamp, cert.seq, cert.change, key_names(&cert.signers()));
        }
    }
    for p in rb.forks() {
        let prefix = rb.prefix_of(p.a.prev[0]);
        println!(
            "  fork: {:?} vs {:?}, signed both by {} ({}); a third party holding {} certificate(s) verifies it: {}",
            p.a.change,
            p.b.change,
            key_names(&p.signed_both()),
            if p.equivocation() { "equivocation" } else { "concurrent changes" },
            prefix.len(),
            p.verify(&prefix)
        );
    }
    for &f in at {
        println!("  after the decisions up to frame {f}: {}", view_line(&book_at(f).view()));
    }
    let conf = replicas.iter().map(|(_, r)| r.sim.conf).min().unwrap_or(0);
    if frames > conf {
        println!("  UNCONFIRMED: {label} checks frames {conf}..{frames}, past the lowest confirmed frame ({conf})");
    }
    all_equal
}

pub fn print_stats(sim: &Sim, w: WorldId) {
    for p in &sim.peers {
        if let Some(r) = p.worlds.get(&w) {
            let s = r.sim.stats;
            println!(
                "  m{} world {w}: rollbacks {} max depth {} resim frames {} ({:.1} ms) overwrites {} paused frames {} violations {} equivocations {} checks ok {} bad {} nonfinite {} conf {} frame {}",
                p.id,
                s.rollbacks,
                s.max_depth,
                s.resim_frames,
                s.resim_us as f64 / 1000.0,
                s.overwrites,
                s.paused_frames,
                s.entries_rejected_violation,
                s.equivocations,
                s.checks_ok,
                s.checks_bad,
                s.nonfinite,
                r.sim.conf,
                r.sim.frame
            );
        }
    }
}

fn acceptance_summary(sim: &Sim, label: &str) {
    let n = sim.acceptance.len();
    let eq = sim.acceptance.iter().filter(|a| a.equal).count();
    let fin = |a: &Acceptance| sim.replica(a.author, a.world).sim.obj_hist.get(&(a.frame, a.obj)).copied() == Some(a.receiver_hash);
    let eq_final = sim.acceptance.iter().filter(|a| fin(a)).count();
    // Contended: another member touched the same object within the 10 frames before the check,
    // so a competing entry may still have been in flight to one side.
    let contended = |a: &Acceptance| {
        sim.replica(a.author, a.world)
            .sim
            .all_entries()
            .iter()
            .any(|e| e.member != a.author && matches!(e.payload, Payload::Touch { obj, .. } if obj == a.obj) && e.stamp + 10 >= a.frame && e.stamp <= a.frame)
    };
    let unc: Vec<&Acceptance> = sim.acceptance.iter().filter(|a| !contended(a)).collect();
    println!(
        "RESULT {label} acceptance, uncontended checks: {}/{} equal at that instant and {}/{} equal to the author's final history; contended checks (another member touched the object in the previous 10 frames): {}",
        unc.iter().filter(|a| a.equal).count(),
        unc.len(),
        unc.iter().filter(|a| fin(a)).count(),
        unc.len(),
        n - unc.len()
    );
    println!(
        "RESULT {label} acceptance (after re-sim to its present frame, the receiver's copy of the touched object vs the author's copy at that frame): {eq}/{n} equal to the author's copy at that instant, {eq_final}/{n} equal to the author's final history"
    );
    for a in sim.acceptance.iter().filter(|a| !a.equal) {
        println!(
            "  differs at that instant: world {} receiver m{} author m{} obj {} frame {}; equal to the author's final history: {}",
            a.world,
            a.receiver,
            a.author,
            a.obj,
            a.frame,
            fin(a)
        );
    }
}

/// Honest members agree with each other and with an offline replay of the final log, over the
/// frames every honest member has simulated; no confirmed frame was rolled back (the run would
/// have panicked); no false check failures (review2.rs `verdict`).
fn verdict(sim: &Sim, honest: &[MemberId], label: &str, panic_at: Option<u64>) -> bool {
    if let Some(t) = panic_at {
        println!("RESULT {label}: PANIC at t={t} ms -> FAIL");
        return false;
    }
    let frames = honest.iter().map(|&m| sim.replica(m, 1).sim.frame).min().unwrap_or(0).saturating_sub(1);
    let r0 = sim.replica(honest[0], 1);
    let first = &r0.sim;
    let members: Vec<MemberId> = first.vcm.members.iter().copied().collect();
    let reference = replay(1, r0.scene, first.cfg, first.public_keys(), &members, &final_log(first), frames);
    let rd = digest(&reference, frames);
    let ds: Vec<(MemberId, u64)> = honest.iter().map(|&m| (m, digest(&sim.replica(m, 1).sim.hashes, frames))).collect();
    let equal = ds.iter().all(|(_, d)| *d == rd);
    let bad: u64 = honest.iter().map(|&m| sim.replica(m, 1).sim.stats.checks_bad).sum();
    let ok: u64 = honest.iter().map(|&m| sim.replica(m, 1).sim.stats.checks_ok).sum();
    for &m in honest {
        let r = &sim.replica(m, 1).sim;
        println!(
            "  m{m}: conf {} frame {} members {:?} evicted {:?} rollbacks {} max depth {} violations {} equivocations {} checks ok {} bad {}",
            r.conf,
            r.frame,
            r.vcm.members,
            r.members_evicted(),
            r.stats.rollbacks,
            r.stats.max_depth,
            r.stats.entries_rejected_violation,
            r.stats.equivocations,
            r.stats.checks_ok,
            r.stats.checks_bad
        );
    }
    println!(
        "RESULT {label}: frames 0..{frames} honest {} | offline replay {rd:016x} | {} ; checks ok {ok} bad {bad}",
        ds.iter().map(|(m, d)| format!("m{m}={d:016x}")).collect::<Vec<_>>().join(" "),
        if equal && bad == 0 { "ALL EQUAL, NO FALSE CHECKS" } else { "FAIL" }
    );
    equal && bad == 0
}

pub fn touch_state(sim: &Sim, w: WorldId, m: MemberId, author: MemberId) -> String {
    let r = &sim.replica(m, w).sim;
    let void = r.void_ids();
    r.all_entries()
        .into_iter()
        .filter(|e| e.member == author && matches!(e.payload, Payload::Touch { .. }))
        .map(|e| format!("m{}s{}@{} eff={:?} void={}", e.member, e.seq, e.stamp, r.effective_frame(e.id()), void.contains(&e.id())))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The phase-1 rule's failures that C1 and C2 fix, on the old RollbackSim (main.rs c1/c2).
fn phase1_rule(case: &str) {
    let mk = || RollbackSim::new(GameState::new(build_pile_scene(8, 4), 0x5052_494D_4F52_4449), 120);
    let (mut a, mut b) = (mk(), mk());
    if case == "c1" {
        // TooLate by local ring position, exactly the reviewer's repro.
        let late = Interaction { frame: 50, peer: 9, seq: 0, body: 10, impulse: [0.0, 6.0, 0.0] };
        a.advance_to(169);
        b.advance_to(171);
        let ra = a.add_input(late);
        let rb = b.add_input(late);
        a.advance_to(300);
        b.advance_to(300);
        println!(
            "RESULT c1 before (phase-1 rule): A {:?}, B {:?}; hash@299 equal: {}",
            ra.map_err(|e: InputError| e),
            rb,
            a.hashes[&299] == b.hashes[&299]
        );
    } else {
        // First-seen-wins dedup by (peer, seq).
        let x = Interaction { frame: 50, peer: 9, seq: 0, body: 10, impulse: [0.0, 6.0, 0.0] };
        let x2 = Interaction { impulse: [6.0, 0.0, 0.0], ..x };
        a.add_input(x).unwrap();
        b.add_input(x2).unwrap();
        a.advance_to(200);
        b.advance_to(200);
        let (ra, rb) = (a.add_input(x2), b.add_input(x));
        a.advance_to(200);
        b.advance_to(200);
        println!("RESULT c2 before (phase-1 rule): repair a<-x' {ra:?}, b<-x {rb:?}; hash@199 equal: {}", a.hashes[&199] == b.hashes[&199]);
    }
}

// ---- R14 (main.rs r14) ------------------------------------------------------------------------

fn r14_forged(sim: &Sim, forged: &[Forged]) {
    for (w, by, id, what) in forged {
        // An honest member of that world other than the author.
        let witness = match (*w, *by) {
            (1, 2) => 1,
            (1, _) => 2,
            (_, 3) => 1,
            _ => 3,
        };
        let o = sim.replica(witness, *w).sim.outcomes.get(id).copied();
        println!("RESULT   forged by m{by} in world {w}: {what} -> {:?}", o.map(|x| x.1));
    }
}

fn r14_jobs(sim: &Sim) {
    for j in &sim.peers[0].jobs {
        for l in &j.log {
            println!("  carrier: {l}");
        }
        println!("  carrier: phase {:?} accepted {:?}", j.phase, j.accepted);
        let a = &sim.replica(1, 1).sim;
        if let Some((f, o)) = a.outcomes.get(&j.depart) {
            println!("  carrier: depart entry final outcome at A frame {f}: {o:?}");
        }
    }
    for p in sim.peers.iter().skip(1) {
        for j in &p.jobs {
            for l in &j.log {
                println!("  relayer m{}: {l}", p.id);
            }
            println!("  relayer m{}: phase {:?} accepted {:?}", p.id, j.phase, j.accepted);
        }
    }
}

const GID40: u64 = (1u64 << 32) | 40;

fn r14_conservation(sim: &Sim, label: &str) {
    for (w, name) in [(1, "A"), (2, "B")] {
        for p in &sim.peers {
            if let Some(r) = p.worlds.get(&w) {
                let live = r.sim.objects().map(|(_, o)| o).filter(|o| o.global == GID40).count();
                let esc: Vec<String> = r.sim.t.escrow.iter().map(|(t, e)| format!("T{t}:{:?}", e.status)).collect();
                let arr: Vec<String> = r.sim.t.arrivals.iter().map(|(t, a)| format!("T{t}:{a:?}")).collect();
                println!("  m{} world {name}: object 40 live copies {live}; escrow {esc:?}; arrivals {arr:?}", p.id);
            }
        }
    }
    let live_a = sim.replica(2, 1).sim.objects().map(|(_, o)| o).filter(|o| o.global == GID40).count();
    let live_b = sim.replica(3, 2).sim.objects().map(|(_, o)| o).filter(|o| o.global == GID40).count();
    let pending = sim.replica(2, 1).sim.t.escrow.values().filter(|e| e.status == EscrowStatus::Pending).count();
    println!(
        "RESULT r14 {label}: object 40 live in A {live_a}, in B {live_b}, pending escrow {pending} -> {}",
        if live_a + live_b == 1 && pending == 0 { "CONSERVED (exactly one live copy, nothing in limbo)" } else { "NOT CONSERVED" }
    );
}

fn r14_forge_conservation(sim: &Sim, forged: &[Forged], label: &str, original: bool, attacks: Option<usize>, control: bool) {
    let live = |m: MemberId, w: WorldId, g: u64| sim.replica(m, w).sim.objects().map(|(_, o)| o).filter(|o| o.global == g).count();
    let (g50, g51) = ((1u64 << 32) | 50, (1u64 << 32) | 51);
    let (a50, a51, d50, b40) = (live(1, 1, g50), live(1, 1, g51), live(2, 4, g50), live(3, 2, GID40));
    if original {
        println!(
            "RESULT r14 forge: A's objects 50 and 51 live in A {a50} and {a51}, object 50 in D {d50}; copies of A:40 in B {b40} -> {}",
            if a50 == 1 && a51 == 1 && d50 == 0 && b40 <= 1 { "CONSERVED" } else { "NOT CONSERVED" }
        );
    }
    // Every attack must have been tried: one whose moment passed (the carry resolved earlier)
    // would otherwise drop out silently.
    if let Some(n) = attacks {
        if forged.len() != n {
            println!("RESULT r14 forge: FAIL: {} of {n} forged entries authored", forged.len());
        }
    }
    // P2-5: copies in the honest worlds A and B (W's own replicas are its majority's word).
    let pending_of = |m: MemberId, w: WorldId, g: u64| {
        sim.replica(m, w).sim.t.escrow.values().filter(|e| e.status == EscrowStatus::Pending && e.record.is_some_and(|r| r.global == g)).count()
    };
    let mut ok = true;
    let mut parts = Vec::new();
    for (name, g, want) in [("A:52", (1u64 << 32) | 52, 1), ("A:53", (1 << 32) | 53, 1), ("A:54", (1 << 32) | 54, 1), ("A:55", (1 << 32) | 55, 1), ("W:500", (6 << 32) | 500, 0)] {
        let (a, b, w) = (live(1, 1, g), live(3, 2, g), live(2, 6, g));
        let p = pending_of(1, 1, g) + pending_of(3, 2, g);
        ok &= a + b == want && p == 0;
        parts.push(format!("{name} A {a} B {b} pending {p} (W {w})"));
    }
    println!(
        "RESULT r14 {label}: dishonest-majority W ({}): {} -> {}",
        if control { "CONTROL, provenance off" } else { "provenance on" },
        parts.join("; "),
        if ok { "CONSERVED in the honest worlds" } else { "NOT CONSERVED in the honest worlds" }
    );
    for (m, w) in [(1, 1), (1, 2)] {
        let r = &sim.replica(m, w).sim;
        for p in r.fork_proofs.values() {
            // P25-3: the proof carries both certificates, so a third party checks it on its own.
            println!(
                "RESULT r14 {label}: world {w} holds a fork proof against world {}: it exported {:#x} twice from its entry {} (transfers {} and {}); verifies for a third party: {}",
                p.world,
                p.global,
                p.via,
                p.a.transfer(),
                p.b.transfer(),
                p.verify(&r.epochs)
            );
        }
    }
}

// ---- R14 chain (main.rs r14_chain, P2-5) --------------------------------------------------------

fn r14_chain_jobs(sim: &Sim) {
    for p in &sim.peers {
        for j in &p.jobs {
            for l in &j.log {
                println!("  m{}: {l}", p.id);
            }
            println!("  m{}: transfer {} world {} -> {}: phase {:?} accepted {:?}", p.id, j.transfer, j.src, j.dst, j.phase, j.accepted);
        }
    }
}

fn r14_chain_outcomes(sim: &Sim) {
    for (w, label) in [(1, "A"), (2, "B"), (3, "C")] {
        let r = &sim.replica(1, w).sim;
        for e in r.all_entries() {
            if let Payload::Depart { obj, dest, hop, .. } = &e.payload {
                println!(
                    "RESULT r14 chain: departure of local {obj} from {label} to world {dest} (hop in carried: {}) -> {:?}",
                    hop.is_some(),
                    r.outcomes.get(&e.id()).map(|x| x.1)
                );
            }
            if let Payload::Arrive { chain, .. } = &e.payload {
                let route: Vec<String> = chain.iter().map(|h| format!("{}->{}", h.src, h.dest)).collect();
                println!("RESULT r14 chain: arrival into {label} with a chain of {} hops [{}] -> {:?}", chain.len(), route.join(", "), r.outcomes.get(&e.id()).map(|x| x.1));
            }
        }
    }
}

/// session-sim main.rs `r14_chain_epoch`'s report, unchanged.
fn r14_chain_epoch(sim: &Sim) {
    for p in &sim.peers {
        for j in &p.jobs {
            for l in &j.log {
                println!("  m{}: {l}", p.id);
            }
        }
    }
    let a = &sim.replica(2, 1).sim;
    println!("RESULT r14 chain-epoch: A at m2: epoch {} members {:?} decisions {:?}", a.vcm.epoch, a.vcm.members, a.vcm.vc_log);
    for (w, label, m) in [(2, "B", 3), (3, "C", 4)] {
        let r = &sim.replica(m, w).sim;
        for e in r.all_entries() {
            if let Payload::Provenance { hop } = &e.payload {
                println!(
                    "RESULT r14 chain-epoch: hop {}->{} relayed into {label}: signed by epoch {} at frame {}, carrying {} epoch certificate(s)",
                    hop.src,
                    hop.dest,
                    hop.epoch,
                    hop.frame,
                    hop.certs.len()
                );
            }
            if let Payload::Arrive { chain, .. } = &e.payload {
                let route: Vec<String> = chain.iter().map(|h| format!("{}->{} (epoch {}, {} certs)", h.src, h.dest, h.epoch, h.certs.len())).collect();
                println!("RESULT r14 chain-epoch: arrival into {label} with chain [{}] -> {:?}", route.join(", "), r.outcomes.get(&e.id()).map(|x| x.1));
            }
        }
    }
    let (mut total, mut pending) = (0, 0);
    let mut parts = Vec::new();
    for (w, label, m) in [(1, "A", 2), (2, "B", 3), (3, "C", 4)] {
        let r = &sim.replica(m, w).sim;
        let live = r.objects().map(|(_, o)| o).filter(|o| o.global == GID40).count();
        pending += r.t.escrow.values().filter(|e| e.status == EscrowStatus::Pending).count();
        total += live;
        parts.push(format!("{label} {live}"));
    }
    let in_c = sim.replica(4, 3).sim.objects().map(|(_, o)| o).any(|o| o.global == GID40);
    println!(
        "RESULT r14 chain-epoch: A:40 live copies {} ; pending escrows {pending} -> {}",
        parts.join(", "),
        if total == 1 && pending == 0 && in_c { "CONSERVED (in C, nothing in limbo)" } else { "NOT CONSERVED" }
    );
}

fn r14_chain_conservation(sim: &Sim, label: &str) {
    let (mut total, mut pending) = (0, 0);
    let mut parts = Vec::new();
    for (w, name, m) in [(1, "A", 2), (2, "B", 3), (3, "C", 4)] {
        let r = &sim.replica(m, w).sim;
        let live = r.objects().map(|(_, o)| o).filter(|o| o.global == GID40).count();
        pending += r.t.escrow.values().filter(|e| e.status == EscrowStatus::Pending).count();
        total += live;
        parts.push(format!(
            "{name} {live} ({:?})",
            r.t.seen.get(&GID40).map(|s| match s {
                Seen::Here { chain, settled, .. } => format!("here, chain {} hops, settled {settled}", chain.len()),
                Seen::Left { dest, .. } => format!("left for {dest}"),
            })
        ));
    }
    println!(
        "RESULT {label}: A:40 live copies {} ; pending escrows {pending} -> {}",
        parts.join(", "),
        if total == 1 && pending == 0 && sim.replica(2, 1).sim.objects().map(|(_, o)| o).any(|o| o.global == GID40) {
            "CONSERVED (back in A, nothing in limbo)"
        } else {
            "NOT CONSERVED"
        }
    );
}

fn r14_lag(sim: &Sim, lag: &[(u64, u64)], label: &str, throttled: bool) {
    let before: Vec<u64> = lag.iter().filter(|(t, _)| (1_500..3_000).contains(t)).map(|x| x.1).collect();
    let after: Vec<u64> = lag.iter().filter(|(t, _)| (3_500..8_500).contains(t)).map(|x| x.1).collect();
    let avg = |v: &Vec<u64>| v.iter().sum::<u64>() as f64 / v.len().max(1) as f64;
    println!(
        "RESULT r14 {label}: member 2's confirmation lag in A (present - confirmed, frames): avg {:.1} before t=3 s, {:.1} after (member 1 {} from t=3 s)",
        avg(&before),
        avg(&after),
        if throttled { "throttled to one burst per 250 ms" } else { "not throttled" }
    );
    // Review NEW-12: the background member's own entries, and how late its check hashes come.
    let a2 = &sim.replica(2, 1).sim;
    for e in a2.all_entries() {
        if let (1, Payload::Touch { obj: 25, .. }) = (e.member, &e.payload) {
            println!("RESULT r14 {label}: member 1's touch in A after t=3 s: stamp {}, outcome at member 2 {:?}", e.stamp, a2.outcomes.get(&e.id()).map(|x| x.1));
        }
    }
    let delays = |m: MemberId| -> Vec<u64> {
        let mut first: BTreeMap<u64, u64> = BTreeMap::new();
        for e in a2.all_entries() {
            if let Payload::Ack { check: Some((f, _)), .. } = e.payload {
                if e.member == m && f >= 180 {
                    let s = first.entry(f).or_insert(e.stamp);
                    *s = (*s).min(e.stamp);
                }
            }
        }
        first.iter().map(|(f, s)| s - f).collect()
    };
    let (d1, d2) = (delays(1), delays(2));
    let fmt = |v: &Vec<u64>| format!("avg {:.1}, max {} over {} check frames", avg(v), v.iter().max().unwrap_or(&0), v.len());
    println!(
        "RESULT r14 {label}: check hashes in A from frame 180 (frames from check frame to the first ack carrying it): member 1 {}; member 2 {}",
        fmt(&d1),
        fmt(&d2)
    );
}
