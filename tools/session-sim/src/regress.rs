//! The regression command: run every scenario file, compare each log with the committed one.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("tools/session-sim sits two levels below the repository").to_path_buf()
}

pub fn default_scenarios() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios")
}

/// Every scenario file under `dir` (recursively, except `common/`, which holds includes),
/// by log name (the file stem, unique across the tree).
pub fn discover(dir: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
    fn walk(d: &Path, out: &mut BTreeMap<String, PathBuf>) -> Result<(), String> {
        let mut entries: Vec<PathBuf> = fs::read_dir(d).map_err(|e| format!("{}: {e}", d.display()))?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n != "common") {
                    walk(&p, out)?;
                }
            } else if p.extension().is_some_and(|x| x == "toml") {
                let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
                if let Some(prev) = out.insert(stem.clone(), p.clone()) {
                    return Err(format!("two scenarios named {stem}: {} and {}", prev.display(), p.display()));
                }
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out)?;
    Ok(out)
}

/// The CPU timings the logs print, which vary run to run: the text before the number (`#` is any
/// integer) and the text after it. Only these are masked, so a virtual-time decimal in a future
/// log line is still compared (review of fffdcbc).
const TIMINGS: &[(&str, &str)] = &[
    ("resim frames # (", " ms) overwrites"), // stats: "resim frames 135 (88.8 ms) overwrites"
    ("re-sim ", " ms per rendered sim frame"), // ball pit
    ("rendered sim frame (", " ms over the window)"),
    ("step avg ", " ms;"),
    ("| wall ", " s"), // vcscale: "| no panic | wall 40.7 s"
];

/// Mask the measured CPU timings in a log line (see [`TIMINGS`]).
pub fn mask(line: &str) -> String {
    let mut out = line.to_string();
    for (pre, post) in TIMINGS {
        let mut from = 0;
        while let Some(i) = out[from..].find(post).map(|i| i + from) {
            let start = out[..i].rfind(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(0, |j| j + 1);
            if start < i && ends_with_pattern(&out[..start], pre) {
                out.replace_range(start..i, "X");
                from = start + 1 + post.len();
            } else {
                from = i + post.len();
            }
        }
    }
    out
}

fn ends_with_pattern(head: &str, pre: &str) -> bool {
    match pre.split_once('#') {
        None => head.ends_with(pre),
        Some((a, b)) => head.strip_suffix(b).is_some_and(|h| {
            let n = h.trim_end_matches(|c: char| c.is_ascii_digit());
            n.len() < h.len() && n.ends_with(a)
        }),
    }
}

/// What the old run script failed on: a fork, replay mismatch, panic, lost or duplicated
/// object, false check or non-finite state.
fn failures(log: &str) -> Vec<String> {
    let mut v = Vec::new();
    for l in log.lines() {
        for w in ["MISMATCH", "DIFFER", "FAIL", "PANIC", "panicked", "NOT CONSERVED", "UNCONFIRMED"] {
            if l.contains(w) {
                v.push(l.trim().to_string());
            }
        }
        // As the run script's `checks ok N bad [1-9]` and `nonfinite [1-9]`.
        let num = |t: &str| t.chars().take_while(|c| c.is_ascii_digit()).collect::<String>();
        let false_check = l.match_indices("checks ok ").any(|(i, k)| {
            let t = &l[i + k.len()..];
            let n = num(t);
            t[n.len()..].strip_prefix(" bad ").is_some_and(|b| num(b).parse::<u64>().is_ok_and(|x| x > 0))
        });
        let nonfinite = l.match_indices("nonfinite ").any(|(i, k)| num(&l[i + k.len()..]).parse::<u64>().is_ok_and(|x| x > 0));
        if false_check || nonfinite {
            v.push(l.trim().to_string());
        }
    }
    v.dedup();
    v
}

enum Outcome {
    Same,
    New,
    Differs(String),
}

fn compare(got: &str, want: Option<&str>) -> Outcome {
    let Some(want) = want else { return Outcome::New };
    let (g, w): (Vec<String>, Vec<String>) = (got.lines().map(mask).collect(), want.lines().map(mask).collect());
    if g == w {
        return Outcome::Same;
    }
    let i = (0..g.len().max(w.len())).find(|&i| g.get(i) != w.get(i)).unwrap();
    Outcome::Differs(format!(
        "line {}: expected {:?}\n           got      {:?}",
        i + 1,
        w.get(i).map_or("<end of log>", |s| s.as_str()),
        g.get(i).map_or("<end of log>", |s| s.as_str())
    ))
}

/// The committed logs that are spike-only for now: `spike-only.txt` in the scenarios folder, one log
/// name per line (as `golden`), `#` starts a comment. They are not mirrored as scenario files yet,
/// so a full run reports them instead of failing on them as orphans. The list only shrinks: a name
/// with a scenario file, or with no committed log, is an error.
pub fn spike_only(dir: &Path) -> Result<Vec<String>, String> {
    name_list(dir, "spike-only.txt")
}

/// The committed logs that are not scenarios at all: `not-scenarios.txt` in the scenarios folder,
/// in `spike-only.txt`'s format. They are the output of the spike's property tests (`randsched`,
/// a randomized schedule generator that checks its own invariants), which the spike suite runs and
/// checks; the tool has no scenario for them and never will. A name with a scenario file, or with
/// no committed log, is an error, as for `spike-only.txt`.
pub fn not_scenarios(dir: &Path) -> Result<Vec<String>, String> {
    name_list(dir, "not-scenarios.txt")
}

fn name_list(dir: &Path, file: &str) -> Result<Vec<String>, String> {
    let p = dir.join(file);
    let Ok(text) = fs::read_to_string(&p) else { return Ok(vec![]) };
    let mut v: Vec<String> = Vec::new();
    for l in text.lines() {
        let l = l.split('#').next().unwrap().trim();
        if l.is_empty() {
            continue;
        }
        if v.iter().any(|x| x == l) {
            return Err(format!("{}: {l} is listed twice", p.display()));
        }
        v.push(l.to_string());
    }
    Ok(v)
}

/// Every `.log` under `dir`, as a path relative to it without the extension (`/`-separated).
fn logs_under(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if p.is_dir() {
            logs_under(&p, &format!("{prefix}{name}/"), out);
        } else if let Some(stem) = name.strip_suffix(".log") {
            out.push(format!("{prefix}{stem}"));
        }
    }
}

pub fn regress(args: &[String]) -> i32 {
    let get = |k: &str| args.iter().position(|x| x == k).and_then(|i| args.get(i + 1)).cloned();
    let scenarios = get("--scenarios").map(PathBuf::from).unwrap_or_else(default_scenarios);
    let expected = get("--expected").map(PathBuf::from).unwrap_or_else(|| repo().join("spikes/out/session-sim"));
    let out = get("--out").map(PathBuf::from).unwrap_or_else(|| repo().join("target/session-sim"));
    let jobs: usize = get("--jobs").map_or(1, |v| v.parse().unwrap_or(1)).max(1);
    let bless = args.iter().any(|a| a == "--bless");
    let only: Option<Vec<String>> = get("--only").map(|v| v.split(',').map(String::from).collect());
    let all = match discover(&scenarios) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let todo: Vec<(String, PathBuf)> = all.into_iter().filter(|(n, _)| only.as_ref().is_none_or(|o| o.contains(n))).collect();
    if todo.is_empty() {
        eprintln!("error: no scenarios to run in {}", scenarios.display());
        return 2;
    }
    if let Err(e) = fs::create_dir_all(&out) {
        eprintln!("error: {}: {e}", out.display());
        return 2;
    }
    let exe = std::env::current_exe().expect("own path");
    println!("regress: {} scenarios from {}, logs to {}, expected {}", todo.len(), scenarios.display(), out.display(), expected.display());
    let t0 = Instant::now();
    let queue = Mutex::new(todo.iter());
    let results: Mutex<BTreeMap<String, (bool, String)>> = Mutex::new(BTreeMap::new());
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| loop {
                let Some((name, path)) = queue.lock().unwrap().next() else { break };
                let log = out.join(format!("{name}.log"));
                let f = fs::File::create(&log).expect("log file");
                let status = Command::new(&exe).arg("run").arg(path).stdout(Stdio::from(f.try_clone().unwrap())).stderr(Stdio::from(f)).status();
                let ok = status.as_ref().is_ok_and(|s| s.success());
                let text = fs::read_to_string(&log).unwrap_or_default();
                results.lock().unwrap().insert(name.clone(), (ok, text));
            });
        }
    });
    let results = results.into_inner().unwrap();
    // Each scenario's committed log (relative, without .log), whether it is a control run, and the
    // text its control must print.
    let meta: BTreeMap<String, (String, bool, Option<String>)> = todo
        .iter()
        .map(|(n, p)| {
            let first = crate::scenario::load(p).ok().and_then(|l| l.parts.into_iter().next());
            let golden = first.as_ref().and_then(|s| s.golden.clone()).unwrap_or_else(|| n.clone());
            let marker = first.as_ref().and_then(|s| s.control_marker.clone());
            (n.clone(), (golden, first.is_some_and(|s| s.control), marker))
        })
        .collect();
    // A committed log no scenario produces any more (only checked for a full run), apart from the
    // spike-only ones.
    let (spike_only, not_scenarios) = match (spike_only(&scenarios), not_scenarios(&scenarios)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let mut stale: Vec<String> = Vec::new();
    let orphans: Vec<String> = if only.is_none() {
        let goldens: std::collections::BTreeSet<&str> = meta.values().map(|(g, _, _)| g.as_str()).collect();
        let mut logs = Vec::new();
        logs_under(&expected, "", &mut logs);
        for (list, file) in [(&spike_only, "spike-only.txt"), (&not_scenarios, "not-scenarios.txt")] {
            for g in list {
                if goldens.contains(g.as_str()) {
                    stale.push(format!("{g} is listed in {file} but has a scenario file: remove it from the list"));
                } else if !logs.contains(g) {
                    stale.push(format!("{g} is listed in {file} but has no committed log"));
                } else if file == "not-scenarios.txt" && spike_only.contains(g) {
                    stale.push(format!("{g} is listed in both spike-only.txt and not-scenarios.txt"));
                }
            }
        }
        logs.into_iter().filter(|g| !goldens.contains(g.as_str()) && !spike_only.contains(g) && !not_scenarios.contains(g)).collect()
    } else {
        vec![]
    };
    let (mut same, mut new, mut bad) = (0, 0, 0);
    for (name, (ok, text)) in &results {
        let (golden, control, marker) = &meta[name];
        let path = expected.join(format!("{golden}.log"));
        let want = fs::read_to_string(&path).ok();
        // A control run is expected to show failures: its log is compared, and it must show at
        // least one failure (or its `control_marker`), so blessing cannot turn it into a pass.
        let fails = if *control {
            let shows = match marker {
                Some(m) => text.contains(m.as_str()),
                None => !failures(text).is_empty(),
            };
            if shows {
                vec![]
            } else {
                vec![format!("the control run shows no failure ({})", marker.as_ref().map_or("no failure marker".to_string(), |m| format!("no {m:?}")))]
            }
        } else {
            failures(text)
        };
        let cmp = compare(text, want.as_deref());
        let differs = matches!(cmp, Outcome::Differs(_));
        let verdict = match cmp {
            Outcome::Same => {
                same += 1;
                "identical".to_string()
            }
            Outcome::New => {
                new += 1;
                "NEW (no committed log)".to_string()
            }
            Outcome::Differs(d) => format!("DIFFERS from the committed log at {d}"),
        };
        if differs || !ok || !fails.is_empty() {
            bad += 1;
        }
        println!("== {name}{}: {verdict}{}", if *control { " (control: failures expected)" } else { "" }, if *ok { "" } else { "; the run exited with an error" });
        for l in text.lines().filter(|l| l.starts_with("RESULT")) {
            println!("{l}");
        }
        for l in &fails {
            println!("  FAILURE: {l}");
        }
        if bless {
            let _ = fs::create_dir_all(path.parent().unwrap());
            if let Err(e) = fs::write(&path, text) {
                println!("  bless failed: {e}");
            }
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    for o in &orphans {
        println!("== {o}: ORPHAN: committed log {} has no scenario", expected.join(format!("{o}.log")).display());
    }
    for g in spike_only.iter().filter(|g| !stale.iter().any(|s| s.starts_with(&format!("{g} ")))) {
        if only.is_none() {
            println!("== {g}: spike-only (not mirrored as a scenario file yet; see spike-only.txt)");
        }
    }
    for g in not_scenarios.iter().filter(|g| !stale.iter().any(|s| s.starts_with(&format!("{g} ")))) {
        if only.is_none() {
            println!("== {g}: not a scenario (a spike property test, run and checked by the spike suite; see not-scenarios.txt)");
        }
    }
    for e in &stale {
        println!("== ERROR: {e}");
    }
    bad += orphans.len() + stale.len();
    if bad == 0 && (new == 0 || bless) {
        println!(
            "SUMMARY: {} scenarios, {same} identical to the committed logs{}{}; every scenario ALL EQUAL, no panic, 0 false checks, 0 non-finite states ({secs:.0} s)",
            results.len(),
            if new > 0 { format!(", {new} new (blessed)") } else { String::new() },
            if only.is_none() && !spike_only.is_empty() { format!("; {} committed logs spike-only, not mirrored yet", spike_only.len()) } else { String::new() },
        );
        0
    } else {
        println!("SUMMARY: FAILED: {same} identical, {new} new without a committed log, {bad} failed ({secs:.0} s)");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_timings_only() {
        assert_eq!(mask("resim frames 135 (88.8 ms) overwrites"), "resim frames 135 (X ms) overwrites");
        assert_eq!(mask("re-sim 3.51 ms per rendered sim frame (772 ms over the window); step avg 0.400 ms;"), "re-sim X ms per rendered sim frame (X ms over the window); step avg X ms;");
        assert_eq!(mask("  t=2850 ms: 9's touch"), "  t=2850 ms: 9's touch");
        assert_eq!(mask("one burst per 250 ms)"), "one burst per 250 ms)");
        assert_eq!(mask("conf 360 | no panic | wall 40.7 s"), "conf 360 | no panic | wall X s");
        assert_eq!(mask("removed at 3.75 s"), "removed at 3.75 s");
        assert_eq!(mask("mean lag 33.3 ms, resim frames 2 (1.5 ms) overwrites"), "mean lag 33.3 ms, resim frames 2 (X ms) overwrites");
        assert_eq!(mask("  m1 world 1: rollbacks 0 max depth 0 resim frames 0 (0.0 ms) overwrites 0"), "  m1 world 1: rollbacks 0 max depth 0 resim frames 0 (X ms) overwrites 0");
    }

    #[test]
    fn finds_failures() {
        assert!(failures("  m1 world 1: checks ok 50 bad 0 nonfinite 0 conf 1").is_empty());
        assert_eq!(failures("  m1 world 1: checks ok 50 bad 2 nonfinite 0").len(), 1);
        assert_eq!(failures("RESULT x: ... ; checks ok 62 bad 3").len(), 1);
        assert_eq!(failures("  m1: nonfinite 4").len(), 1);
        assert_eq!(failures("RESULT x | MISMATCH").len(), 1);
        assert!(failures("RESULT x | ALL EQUAL, NO FALSE CHECKS ; checks ok 62 bad 0").is_empty());
    }
}
