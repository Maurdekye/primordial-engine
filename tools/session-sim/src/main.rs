//! pe-session-sim: the session simulator as a test tool (design 23.4, P2-9).
//!
//! Scenarios are TOML files (see scenario.rs and scenarios/README.md): worlds, members, plans,
//! links, fault rules, crashes, Byzantine behaviour and reports. The simulator itself is the
//! spike's (spikes/session-sim, in-process, virtual clock, deterministic), driven unchanged, so a
//! scenario file reproduces the spike's hand-written scenario bit for bit. P2-3's `pe-session`
//! replaces it when it lands.
//!
//! usage:
//!   pe-session-sim run <file.toml>         run one scenario; its log goes to stdout
//!   pe-session-sim check <file.toml>...    load and build only
//!   pe-session-sim list [--scenarios DIR]  the scenarios `regress` runs
//!   pe-session-sim regress [--scenarios DIR] [--expected DIR] [--out DIR] [--only a,b] [--jobs N] [--bless]
//!       run every scenario, compare each log with the committed one (CPU timings masked), and
//!       fail on any difference, fork, replay mismatch, panic, false check or lost object.

mod byzantine;
mod faults;
mod regress;
mod report;
mod scenario;
mod setup;
mod validate;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use byzantine::Actor;
use report::{Ctx, Probe};
use scenario::{Report, Scenario};

/// Load-time checks on one part, and the built simulator with its behaviours and reports.
fn prepare(mut s: Scenario) -> Result<(Scenario, pe_session_harness::Sim, Vec<Actor>, Vec<Probe>), String> {
    for r in &s.fault {
        faults::validate(r)?;
    }
    let sim = setup::build(&s)?;
    validate::validate(&s, &sim)?;
    let actors = std::mem::take(&mut s.byzantine).into_iter().map(Actor::new).collect::<Result<Vec<_>, _>>()?;
    let probes = std::mem::take(&mut s.report).into_iter().map(|r| Probe::new(r, &s)).collect::<Result<Vec<_>, _>>()?;
    Ok((s, sim, actors, probes))
}

/// Run one part of a scenario and print its log; true if the simulator panicked.
fn run_part(s: Scenario, name: &str, kept: &mut report::Kept) -> Result<bool, String> {
    // A report that states a panic itself (a verdict, a run with its fix switched off, or a
    // comparison of kept runs), or a kept run, which a later report judges.
    let has_verdict = s.keep.is_some()
        || s.report.iter().any(|r| matches!(r, Report::Verdict { .. } | Report::BeforeFix { .. } | Report::Check { .. } | Report::Handowner { .. }));
    // A run with its fix switched off may stop at an assertion: that is what it shows, not a failure.
    let fix_off = s.report.iter().any(|r| matches!(r, Report::BeforeFix { .. }));
    let (s, mut sim, mut actors, mut probes) = prepare(s)?;
    for a in &actors {
        a.configure(&mut sim)?;
    }
    sim.net.filter = faults::filter(s.fault.clone(), actors.iter().filter_map(|a| a.hook()).collect());
    let mut ctx = Ctx::default();
    for t in &s.intro {
        println!("{}", report::template(t, &report::Vars { sim: &sim, world: 1, entry: None }));
    }
    let mut joined = vec![false; s.join.len()];
    // 12.4: joiners that sign with another identity's key, as (world, member, identity).
    let join_keys: Vec<(u32, u32, u32)> = s.world.iter().filter_map(|(w, c)| Some((scenario::id(w).ok()?, c))).flat_map(|(w, c)| c.keys.iter().map(move |&(m, id)| (w, m, id))).collect();
    ctx.started = Some(std::time::Instant::now());
    let hook = s.quiet_panics.then(|| {
        let h = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        h
    });
    // The spike's scenario loops: crashes, joins and Byzantine sends before a millisecond's step,
    // R14 forgeries and probes after it.
    while sim.net.now_us <= sim.end_us {
        faults::apply_crashes(&mut sim, &s.crash);
        byzantine::send_joins(&mut sim, &s.join, &mut joined, &join_keys)?;
        byzantine::before_step(&mut sim, &mut actors);
        for p in probes.iter_mut() {
            p.pre_sample(&sim);
        }
        if catch_unwind(AssertUnwindSafe(|| sim.step_ms())).is_err() {
            ctx.panic_at_ms = Some(sim.net.now_us / 1000);
            break;
        }
        byzantine::after_step(&mut sim, &mut actors, &mut ctx.forged);
        for p in probes.iter_mut() {
            p.sample(&sim);
        }
        sim.net.now_us += 1000;
    }
    if let Some(h) = hook {
        std::panic::set_hook(h);
    }
    ctx.fork_info = actors.iter().find_map(|a| a.fork_info());
    ctx.silenced = actors.iter().filter_map(|a| a.silenced()).collect();
    ctx.wedge = actors.iter().find_map(|a| a.wedge_info());
    ctx.segment = actors.iter().find_map(|a| a.segment_info());
    ctx.kick = actors.iter().find_map(|a| a.kick_info());
    if let (Some(t), false) = (ctx.panic_at_ms, has_verdict) {
        println!("RESULT {name}: PANIC at t={t} ms -> FAIL");
        return Ok(true);
    }
    let panicked = ctx.panic_at_ms.is_some() && !fix_off && s.keep.is_none();
    let sim = match &s.keep {
        Some(k) => {
            kept.insert(k.clone(), (sim, ctx.panic_at_ms));
            &kept[k].0
        }
        None => &sim,
    };
    for p in &probes {
        p.print(sim, &ctx, kept);
    }
    Ok(panicked)
}

/// Run every part; true if any panicked. Every part is checked before the first one runs.
fn run_file(path: &Path) -> Result<bool, String> {
    check_file(path)?;
    let loaded = scenario::load(path)?;
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut panicked = false;
    let mut kept = report::Kept::new();
    for part in loaded.parts {
        panicked |= run_part(part, &name, &mut kept)?;
    }
    Ok(panicked)
}

fn check_file(path: &Path) -> Result<usize, String> {
    let loaded = scenario::load(path)?;
    let n = loaded.parts.len();
    let mut kept: Vec<String> = Vec::new();
    for (i, s) in loaded.parts.into_iter().enumerate() {
        // A report may compare runs kept by earlier parts and by its own.
        kept.extend(s.keep.clone());
        for r in &s.report {
            if let Report::Handowner { forged, clean, fixoff, .. } = r {
                for k in [forged, clean, fixoff] {
                    if !kept.contains(k) {
                        return Err(format!("{} (part {}): handowner: no part up to this one keeps a run named `{k}`", path.display(), i + 1));
                    }
                }
            }
        }
        let (_, mut sim, actors, _) = prepare(s).map_err(|e| format!("{} (part {}):
  {e}", path.display(), i + 1))?;
        for a in &actors {
            a.configure(&mut sim)?;
        }
    }
    Ok(n)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| args.iter().position(|x| x == k).and_then(|i| args.get(i + 1)).cloned();
    let code = match args.first().map(String::as_str) {
        Some("run") if args.len() == 2 => match run_file(Path::new(&args[1])) {
            Ok(false) => 0,
            Ok(true) => 3,
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        },
        Some("check") if args.len() >= 2 => {
            let mut code = 0;
            for f in &args[1..] {
                match check_file(Path::new(f)) {
                    Ok(n) => println!("ok {f} ({n} part{})", if n == 1 { "" } else { "s" }),
                    Err(e) => {
                        println!("error: {e}");
                        code = 2;
                    }
                }
            }
            code
        }
        Some("list") => {
            let dir = get("--scenarios").map(PathBuf::from).unwrap_or_else(regress::default_scenarios);
            match regress::discover(&dir) {
                Ok(v) => {
                    for (name, p) in v {
                        let d = scenario::load(&p).map(|l| l.parts.first().map(|s| s.description.clone()).unwrap_or_default()).unwrap_or_else(|e| format!("error: {e}"));
                        println!("{name:<24} {}", d.lines().next().unwrap_or(""));
                    }
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    2
                }
            }
        }
        Some("regress") => regress::regress(&args[1..]),
        _ => {
            eprintln!("usage: pe-session-sim run <file.toml> | check <file.toml>... | list [--scenarios DIR] | regress [--scenarios DIR] [--expected DIR] [--out DIR] [--only a,b] [--jobs N] [--bless]");
            2
        }
    };
    std::process::exit(code);
}
