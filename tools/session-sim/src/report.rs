//! Reports: what a scenario prints after its run, and the probes some of them sample during it.
//! Every format is the spike's (spikes/session-sim main.rs, review2.rs, review3.rs), so the logs
//! compare line for line with the committed ones.
//!
//! Templates (`say` of a Byzantine behaviour, `text` of a `line` report) substitute:
//! `{t_ms}` virtual time; `{conf@M}`, `{frame@M}`, `{now@M}` member M's confirmed, simulated and
//! clock frame; `{member@M:X}` whether M holds X as a member; `{members@M}`, `{evicted@M}`;
//! `{touches@M:A}` A's touches as M holds them; and, for the entry a behaviour acts on,
//! `{x.stamp}`, `{x.seq}`, `{x.target}` (a vote's target), `{x.confirmed@M}`.

use std::collections::BTreeMap;

use pe_sim_core::session::*;
use pe_sim_core::{build_pile_scene, GameState, InputError, Interaction, RollbackSim};
use spike::{Acceptance, Replica, Sim, WorldId};

use crate::byzantine::Forged;
use crate::scenario::{Report, Scenario};
use crate::setup::hz;

pub struct Vars<'a> {
    pub sim: &'a Sim,
    pub world: WorldId,
    pub entry: Option<&'a Entry>,
}

fn member(s: &str) -> MemberId {
    s.parse().unwrap_or_else(|_| panic!("template: `{s}` is not a member id"))
}

/// Substitute `{...}` variables (see the module docs).
pub fn template(t: &str, v: &Vars) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(i) = rest.find('{') {
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
        "member" => r().members.contains(&member(x)).to_string(),
        "members" => format!("{:?}", r().members),
        "evicted" => format!("{:?}", r().members_evicted()),
        "touches" => touch_state(v.sim, v.world, member(m), member(x)),
        "x.stamp" => e().stamp.to_string(),
        "x.seq" => e().seq.to_string(),
        "x.target" => match e().payload {
            Payload::Vote { target, .. } => target.to_string(),
            _ => panic!("template: {{x.target}} needs a vote"),
        },
        "x.confirmed" => r().is_confirmed(e().id()).to_string(),
        _ => panic!("template: unknown variable `{{{name}}}`"),
    }
}

/// What the run left for the reports besides the simulator.
#[derive(Default)]
pub struct Ctx {
    pub forged: Vec<Forged>,
    pub panic_at_ms: Option<u64>,
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
        if let Report::Outcomes { mode, .. } = &spec {
            if mode != "wins" && mode != "tally" {
                return Err(format!("outcomes: mode must be wins or tally, not `{mode}`"));
            }
        }
        let pit = Pit { prev: vec![None; pit_params.4], ..Default::default() };
        Ok(Probe { spec, lag: Vec::new(), pit, pit_params })
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

    pub fn print(&self, sim: &Sim, ctx: &Ctx) {
        match &self.spec {
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
            Report::Verdict { honest, label } => {
                verdict(sim, honest, label, ctx.panic_at_ms);
            }
            Report::Membership { honest, label } => {
                for &m in honest {
                    let r = &sim.replica(m, 1).sim;
                    println!(
                        "RESULT {label}: m{m} members {:?} evicted {:?} conf {} frame {} paused frames {} lying votes found {}",
                        r.members,
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
                    if r.members.iter().copied().collect::<Vec<_>>() != *want {
                        bad.push(format!("members not {want:?}"));
                    }
                }
                println!(
                    "RESULT {label}: m{at} world {world} conf {} members {:?} evicted {:?} -> {}",
                    r.conf,
                    r.members,
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
                        r.members,
                        r.members_evicted(),
                        r.conf,
                        r.frame,
                        r.stats.paused_frames
                    );
                }
            }
            Report::R14Conservation { label } => r14_conservation(sim, label),
            Report::R14ForgeConservation => r14_forge_conservation(sim, &ctx.forged),
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
        let naive_payload = p.naive_changed * (4 + 52) + p.naive_frames * 16;
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
    let members: Vec<MemberId> = r.sim.members.iter().copied().collect();
    let reference = replay_sim_with(w, r.scene, r.sim.cfg, &members, &r.sim.peers_of, &final_log(&r.sim), frames).hashes;
    let rd = digest(&reference, frames);
    let all_equal = digests.iter().all(|(_, d)| *d == rd);
    if !all_equal {
        if let Some(f) = (0..frames).find(|f| reference.get(f) != r.sim.hashes.get(f)) {
            let log = final_log(&r.sim);
            let near: Vec<String> =
                log.iter().filter(|(_, ef)| *ef + 3 >= f && *ef <= f + 1).map(|(e, ef)| format!("m{}s{}@{} eff {ef}", e.member, e.seq, e.stamp)).collect();
            println!("  first replay mismatch at frame {f}; final-log entries near it: {}", near.join(", "));
            let rs = replay_sim(w, r.scene, r.sim.cfg, &members, &log, f + 1);
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
    let members: Vec<MemberId> = first.members.iter().copied().collect();
    let reference = replay(1, r0.scene, first.cfg, &members, &final_log(first), frames);
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
            r.members,
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
                let live = r.sim.t.objs.iter().flatten().filter(|o| o.global == GID40).count();
                let esc: Vec<String> = r.sim.t.escrow.iter().map(|(t, e)| format!("T{t}:{:?}", e.status)).collect();
                let arr: Vec<String> = r.sim.t.arrivals.iter().map(|(t, a)| format!("T{t}:{a:?}")).collect();
                println!("  m{} world {name}: object 40 live copies {live}; escrow {esc:?}; arrivals {arr:?}", p.id);
            }
        }
    }
    let live_a = sim.replica(2, 1).sim.t.objs.iter().flatten().filter(|o| o.global == GID40).count();
    let live_b = sim.replica(3, 2).sim.t.objs.iter().flatten().filter(|o| o.global == GID40).count();
    let pending = sim.replica(2, 1).sim.t.escrow.values().filter(|e| e.status == EscrowStatus::Pending).count();
    println!(
        "RESULT r14 {label}: object 40 live in A {live_a}, in B {live_b}, pending escrow {pending} -> {}",
        if live_a + live_b == 1 && pending == 0 { "CONSERVED (exactly one live copy, nothing in limbo)" } else { "NOT CONSERVED" }
    );
}

fn r14_forge_conservation(sim: &Sim, forged: &[Forged]) {
    let live = |m: MemberId, w: WorldId, g: u64| sim.replica(m, w).sim.t.objs.iter().flatten().filter(|o| o.global == g).count();
    let (g50, g51) = ((1u64 << 32) | 50, (1u64 << 32) | 51);
    let (a50, a51, d50, b40) = (live(1, 1, g50), live(1, 1, g51), live(2, 4, g50), live(3, 2, GID40));
    println!(
        "RESULT r14 forge: A's objects 50 and 51 live in A {a50} and {a51}, object 50 in D {d50}; copies of A:40 in B {b40} -> {}",
        if a50 == 1 && a51 == 1 && d50 == 0 && b40 <= 1 { "CONSERVED" } else { "NOT CONSERVED" }
    );
    // Every attack must have been tried: one whose moment passed (the carry resolved earlier)
    // would otherwise drop out silently.
    if forged.len() != 9 {
        println!("RESULT r14 forge: FAIL: {} of 9 forged entries authored", forged.len());
    }
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
