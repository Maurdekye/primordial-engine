//! cuts-model: run the suite of model checks. `cuts-model [--threads N] [name-filter...]`
//! Exits non-zero if any check's outcome differs from its expectation (a base check must pass; a control must give a
//! counterexample to one of the properties it names).

use std::time::{Duration, Instant};

use cuts_model::{Adversary, Cuts, Fault, Mutation};
use stateright::{Checker, HasDiscoveries, Model};

struct Check {
    name: &'static str,
    model: Cuts,
    /// Empty: every "always" property must hold and every "sometimes" example be found. Otherwise a control: one of
    /// these properties must get a counterexample.
    expect_fail: &'static [&'static str],
    what: &'static str,
}

fn suite() -> Vec<Check> {
    use Fault::*;
    use Mutation::*;
    // Adversary entry sets: chain faults (a fork of seq 1, an entry back-dated below its predecessor) and signature
    // faults (an entry forged in member 1's name, one signed for another world). Checked separately to keep each
    // state space small; each base check has its control on the same set.
    let chain = Adversary { fork: true, backdate: true, forge: false, replay: false };
    let sig = Adversary { fork: false, backdate: false, forge: true, replay: true };
    let none = Adversary { fork: false, backdate: false, forge: false, replay: false };
    // offset 0: the faulty member 0 leads view 1; offset 2 (n = 4): it leads view 3.
    let n3 = |adv| Cuts::new(3, Author, 3, 1, 1, adv, 0);
    let n4 = |adv| Cuts::new(4, Byzantine, 3, 1, 1, adv, 0);
    vec![
        Check { name: "n3-chain", model: n3(chain), expect_fail: &[], what: "3 members, one author-faulty with chain faults (forks, back-dates, withholds), safety" },
        Check { name: "n3-sig", model: n3(sig), expect_fail: &[], what: "3 members, one author-faulty with signature faults (forges, replays), safety" },
        Check { name: "n3-chain-live", model: n3(chain).live(), expect_fail: &[], what: "3 members, author-faulty, chain faults, liveness" },
        Check { name: "n3-crash-live", model: Cuts::new(3, Silent, 3, 1, 2, none, 0).live(), expect_fail: &[], what: "3 members, one silent from the start, liveness" },
        Check { name: "n3-chain-h2", model: Cuts::new(3, Author, 4, 2, 1, chain, 0), expect_fail: &[], what: "3 members, author-faulty, chain faults, two content cuts deep, safety" },
        Check { name: "n4-byz", model: n4(none), expect_fail: &[], what: "4 members, one Byzantine (equivocates votes, timeouts and proposals), safety" },
        Check { name: "n4-byz-chain", model: n4(chain), expect_fail: &[], what: "4 members, Byzantine with chain faults too, safety" },
        Check { name: "n4-byz-live", model: n4(none).live(), expect_fail: &[], what: "4 members, Byzantine, liveness" },
        Check { name: "n4-crash-live", model: Cuts::new(4, Silent, 3, 1, 2, none, 0).live(), expect_fail: &[], what: "4 members, one silent, liveness" },
        Check { name: "n4-byz-v4", model: Cuts::new(4, Byzantine, 4, 1, 1, none, 2), expect_fail: &[], what: "4 members, Byzantine leads view 3 of 4 (the lock's case), safety" },
        // The size bound, not a control: at n = 3, t = 0, so one equivocating voter splits the two honest members.
        Check { name: "bound-n3-byz", model: Cuts::new(3, Byzantine, 3, 1, 1, none, 0), expect_fail: &["agreement"], what: "3 members, one Byzantine: t = 0, agreement must fail (why n >= 4)" },
        // Controls.
        Check { name: "ctl-d1", model: n3(chain).mutate(VoteWithoutChainValidation), expect_fail: &["validity: void set the same everywhere", "no final frame undone"], what: "d1: vote without validating each chain in order" },
        Check { name: "ctl-unchecked", model: n4(sig).mutate(AcceptProposalUnchecked), expect_fail: &["validity: chains"], what: "accept the proposer's cut unchecked" },
        Check { name: "ctl-quorum", model: n4(none).mutate(QuorumOneTooSmall), expect_fail: &["agreement"], what: "quorum one too small: split-brain" },
        Check { name: "ctl-nolock", model: Cuts::new(4, Byzantine, 4, 1, 1, none, 2).mutate(NoLockRule), expect_fail: &["agreement"], what: "vote ignoring the timeout certificate's highest certificate" },
        Check { name: "ctl-nosig", model: n3(sig).mutate(NoSignatureCheck), expect_fail: &["validity: chains"], what: "no own-hands / world-id signature check" },
        Check { name: "ctl-notimeout", model: n4(none).live().mutate(NoTimeouts), expect_fail: &["liveness: every end state has a final cut at every honest member"], what: "no timeouts: a faulty proposer stalls the line" },
    ]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut filters = Vec::new();
    let mut max_states: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--threads" {
            threads = args[i + 1].parse().expect("--threads N");
            i += 2;
        } else if args[i] == "--max-states" {
            max_states = Some(args[i + 1].parse().expect("--max-states N"));
            i += 2;
        } else {
            filters.push(args[i].clone());
            i += 1;
        }
    }
    let mut bad = 0;
    println!("{:<16} {:>12} {:>6} {:>9}  result", "check", "states", "depth", "seconds");
    for c in suite() {
        if !filters.is_empty() && !filters.iter().any(|f| c.name == f.as_str()) {
            continue;
        }
        let names: Vec<(&'static str, bool)> = c.model.properties().iter().map(|p| (p.name, format!("{:?}", p.expectation) == "Sometimes")).collect();
        let entries: Vec<String> = c.model.entries.iter().enumerate().map(|(i, e)| format!("{i}={}", e.name)).collect();
        let t0 = Instant::now();
        let q = c.model.quorum();
        let mut b = c.model.checker().threads(threads);
        if !c.expect_fail.is_empty() {
            b = b.finish_when(HasDiscoveries::AnyOf(c.expect_fail.iter().copied().collect()));
        }
        if let Some(m) = max_states {
            b = b.target_state_count(m);
        }
        let ch = b.timeout(Duration::from_secs(3600)).spawn_bfs().join();
        let secs = t0.elapsed().as_secs_f64();
        let found = ch.discoveries();
        let mut verdict = Vec::new();
        let ok = if c.expect_fail.is_empty() {
            let mut ok = true;
            for (p, sometimes) in &names {
                let d = found.contains_key(p);
                if *sometimes && !d {
                    verdict.push(format!("MISSING example \"{p}\""));
                    ok = false;
                } else if !*sometimes && d {
                    verdict.push(format!("COUNTEREXAMPLE \"{p}\""));
                    ok = false;
                }
            }
            if ok {
                verdict.push("all properties hold; examples found".into());
            }
            ok
        } else {
            let hit: Vec<&&str> = c.expect_fail.iter().filter(|p| found.contains_key(**p)).collect();
            if hit.is_empty() {
                verdict.push("NO COUNTEREXAMPLE (the control does not bite)".into());
                false
            } else {
                verdict.push(format!("counterexample as expected: {}", hit.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")));
                true
            }
        };
        let done = if ch.is_done() { "" } else { " (timed out)" };
        println!("{:<16} {:>12} {:>6} {:>9.1}  {}{} {}", c.name, ch.unique_state_count(), ch.max_depth(), secs, if ok { "OK  " } else { "FAIL" }, done, verdict.join("; "));
        println!("    {}; q = {}; entries {}", c.what, q, entries.join(" "));
        if !c.expect_fail.is_empty() {
            for p in c.expect_fail {
                if let Some(path) = found.get(p) {
                    println!("    counterexample to \"{p}\" ({} steps):", path.clone().into_actions().len());
                    for a in path.clone().into_actions() {
                        println!("      {a:?}");
                    }
                    break;
                }
            }
        }
        if !ok {
            bad += 1;
        }
    }
    if bad > 0 {
        println!("{bad} check(s) did not meet their expectation");
        std::process::exit(1);
    }
}
