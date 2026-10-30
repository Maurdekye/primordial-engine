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

/// Mask measured CPU timings, which vary run to run: any decimal number of milliseconds
/// ("(88.8 ms)", "step avg 0.400 ms") and the ball pit's "(772 ms over the window)".
pub fn mask(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(i) = rest.find(" ms") {
        let head = &rest[..i];
        let start = head.rfind(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(0, |j| j + 1);
        let num = &head[start..];
        let timed = !num.is_empty() && (num.contains('.') || rest[i..].starts_with(" ms over the window"));
        out.push_str(if timed { &head[..start] } else { head });
        if timed {
            out.push('X');
        }
        out.push_str(" ms");
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    out
}

/// What the old run script failed on: a fork, replay mismatch, panic, lost or duplicated
/// object, false check or non-finite state.
fn failures(log: &str) -> Vec<String> {
    let mut v = Vec::new();
    for l in log.lines() {
        for w in ["MISMATCH", "DIFFER", "FAIL", "PANIC", "panicked", "NOT CONSERVED"] {
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
    let (mut same, mut new, mut bad) = (0, 0, 0);
    for (name, (ok, text)) in &results {
        let want = fs::read_to_string(expected.join(format!("{name}.log"))).ok();
        let fails = failures(text);
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
        println!("== {name}: {verdict}{}", if *ok { "" } else { "; the run exited with an error" });
        for l in text.lines().filter(|l| l.starts_with("RESULT")) {
            println!("{l}");
        }
        for l in &fails {
            println!("  FAILURE: {l}");
        }
        if bless {
            let _ = fs::create_dir_all(&expected);
            if let Err(e) = fs::write(expected.join(format!("{name}.log")), text) {
                println!("  bless failed: {e}");
            }
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    if bad == 0 && (new == 0 || bless) {
        println!(
            "SUMMARY: {} scenarios, {same} identical to the committed logs{}; every scenario ALL EQUAL, no panic, 0 false checks, 0 non-finite states ({secs:.0} s)",
            results.len(),
            if new > 0 { format!(", {new} new (blessed)") } else { String::new() }
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
