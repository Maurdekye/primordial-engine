//! cuts-model: run the suite of model checks. `cuts-model [--threads N] [name-filter...]`
//! Exits non-zero if any check's outcome differs from its expectation (a base check must pass; a control must give a
//! counterexample to one of the properties it names).

mod bounded;

use std::time::{Duration, Instant};

use cuts_model::{Adversary, Backoff, Cuts, Fault, Mutation};
use stateright::report::WriteReporter;
use stateright::{Checker, HasDiscoveries, Model, UniformChooser};

/// What a finished check found: counts, and each discovered property's action trace.
struct Summary {
    unique: usize,
    total: usize,
    depth: usize,
    found: std::collections::HashMap<&'static str, Vec<cuts_model::Action>>,
}

impl Summary {
    fn of<C: Checker<Cuts>>(ch: C) -> Summary {
        let found = ch.discoveries().into_iter().map(|(k, p)| (k, p.into_actions())).collect();
        Summary { unique: ch.unique_state_count(), total: ch.state_count(), depth: ch.max_depth(), found }
    }
}

/// Replay a trace from the initial state. A step that does nothing there (or isn't enabled) is skipped; the shrinker
/// then drops it.
fn replay(m: &Cuts, acts: &[cuts_model::Action]) -> Option<cuts_model::State> {
    let mut s = m.init_states().remove(0);
    for a in acts {
        let mut enabled = Vec::new();
        m.actions(&s, &mut enabled);
        if enabled.contains(a) {
            if let Some(n) = m.next_state(&s, a.clone()) {
                s = n;
            }
        }
    }
    Some(s)
}

/// Shorten a counterexample: drop steps one at a time while the trace still replays and still ends in a state that
/// breaks the property.
fn shrink(m: &Cuts, prop: &str, mut acts: Vec<cuts_model::Action>) -> Vec<cuts_model::Action> {
    let cond = m.properties().into_iter().find(|p| p.name == prop).expect("property").condition;
    let breaks = |a: &[cuts_model::Action]| replay(m, a).is_some_and(|s| !cond(m, &s));
    if !breaks(&acts) {
        return acts;
    }
    loop {
        let mut changed = false;
        let mut i = 0;
        while i < acts.len() {
            let mut t = acts.clone();
            t.remove(i);
            if breaks(&t) {
                acts = t;
                changed = true;
            } else {
                i += 1;
            }
        }
        if !changed {
            return acts;
        }
    }
}

struct Check {
    name: &'static str,
    model: Cuts,
    /// Empty: every "always" property must hold and every "sometimes" example be found. Otherwise a control: one of
    /// these properties must get a counterexample.
    expect_fail: &'static [&'static str],
    what: &'static str,
    /// Search by seeded random walks instead of exhaustively. Only for controls whose counterexample is long and whose
    /// exhaustive space is too big to reach it depth-first in reasonable time: a walk that breaks the property is a
    /// counterexample all the same.
    random: bool,
    /// Bounded: every behaviour of at most this many steps (the n = 4 Byzantine checks, whose full space passed 184 M
    /// states without finishing). The controls of these checks use the same bound, so the bound reaches their attacks.
    depth: Option<usize>,
    /// A time budget for a bounded check whose bound can't be covered in a job's time (n4-byz-progress: 429 M states
    /// in 2 h, unfinished). Past it the row says UNFINISHED: no violation in what was searched, not a proof. Its
    /// unmutated random-walk twin carries the rest.
    budget_s: Option<u64>,
}

const UNBOUNDED: Option<usize> = Option::None;
const NO_BUDGET: Option<u64> = Option::None;

fn suite() -> Vec<Check> {
    use Fault::*;
    use Mutation::*;
    // Adversary entry sets: chain faults (a fork of seq 1, an entry back-dated below its predecessor) and signature
    // faults (an entry forged in member 1's name, one signed for another world). Checked separately to keep each
    // state space small; each base check has its control on the same set.
    let chain = Adversary { fork: true, backdate: true, forge: false, replay: false, send: true };
    let sig = Adversary { fork: false, backdate: false, forge: true, replay: true, send: true };
    let none = Adversary { fork: false, backdate: false, forge: false, replay: false, send: true };
    // The n = 4 quorum checks: the faulty member's entry travels only inside its proposals.
    let quiet = Adversary { send: false, ..none };
    // offset 0: the faulty member 0 leads view 1; offset 2 (n = 4): it leads view 3.
    // n = 3 content checks (validity, void set, D-1): one final cut carries the entry faults, so two views suffice
    // (timeouts still free); the consensus checks (agreement, locks, liveness) get more views and fewer entries.
    let n3 = |adv| Cuts::new(3, Author, 2, 1, 1, adv, 0);
    // n = 4 Byzantine checks carry no honest entries: chain faults are checked at n = 3; here the attacks are on
    // quorums and locks, plus the faulty member's own entries where a check needs them.
    // Fixed frames (v0.2): with the Byzantine member also choosing frames, n4-byz passed 454 M states at 18 steps
    // without finishing (integration run, 2026-11-11). These checks attack quorums, locks and roots, not frames; the
    // Byzantine frame choice has its own checks (n4-byz-frames, ctl-noframe), as the lock checks already did.
    let n4 = |adv| Cuts::new(4, Byzantine, 3, 1, 0, adv, 0).fixed_frames();
    vec![
        Check { name: "n3-chain", model: n3(chain), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "3 members, one author-faulty with chain faults (forks, back-dates, withholds), safety" },
        Check { name: "n3-sig", model: n3(sig), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "3 members, one author-faulty with signature faults (forges, replays), safety" },
        Check { name: "n3-crash", model: Cuts::new(3, Silent, 4, 1, 2, none, 0), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "3 members, one silent, 4 views (consensus: agreement through timeouts), safety" },
        Check { name: "n3-author-v4", model: Cuts::new(3, Author, 4, 1, 0, quiet, 0), expect_fail: &[], random: false, depth: Some(20), budget_s: NO_BUDGET, what: "3 members, author-faulty, 4 views (consensus; its full space passed 110 M states), safety" },
        Check { name: "n3-chain-live", model: Cuts::new(3, Author, 3, 1, 1, chain, 0).live(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "3 members, author-faulty, chain faults, liveness" },
        Check { name: "n3-crash-live", model: Cuts::new(3, Silent, 3, 1, 2, none, 0).live(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "3 members, one silent from the start, liveness" },
        Check { name: "n3-chain-h2", model: Cuts::new(3, Author, 3, 2, 1, chain, 0), expect_fail: &[], random: false, depth: Some(16), budget_s: NO_BUDGET, what: "3 members, author-faulty, chain faults, two content cuts deep, safety (depth 20 passed 247 M states without finishing)" },
        Check { name: "n4-byz", model: n4(quiet), expect_fail: &[], random: false, depth: Some(18), budget_s: NO_BUDGET, what: "4 members, one Byzantine (equivocates votes, timeouts and proposals), safety" },
        Check { name: "n4-byz-chain", model: n4(Adversary { send: false, ..chain }), expect_fail: &[], random: false, depth: Some(14), budget_s: NO_BUDGET, what: "4 members, Byzantine with chain faults (fork, back-date) inside its proposals, safety; a light check (depth 16 passed 211 M states; chain faults are exhaustive at n = 3)" },
        Check { name: "n4-byz-live", model: n4(quiet).live(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "4 members, Byzantine, liveness" },
        // n = 2 (v0.1, coordinator condition 2): where most sessions sit. t = 0, q = 2, both honest. Liveness with early
        // timeouts: up to P timeouts may fire before the network settles (a timeout below the round trip, e.g. the
        // slow member's), then only at quiescence (the adaptive timeout has grown past the round trip).
        Check { name: "n2", model: Cuts::new(2, None, 4, 2, 1, none, 0), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, honest, two content cuts, safety (frames chosen by the proposer)" },
        Check { name: "n2-live", model: Cuts::new(2, None, 4, 1, 1, none, 0).live(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, liveness, timeouts only once settled" },
        // At n = 2 one early timeout spoils its view (q = 2: the member that timed out no longer votes in it), and the
        // two-chain needs two good views in a row, so P early timeouts alternating with good views hold finality off
        // for 2P + 1 views: live with 2P + 2 views, not with 2P + 1 (bound-n2-early).
        Check { name: "n2-live-early", model: Cuts::new(2, None, 8, 1, 1, none, 0).live().early_timeouts(3), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, liveness, up to 3 timeouts before the network settles, 8 views" },
        Check { name: "n2-live-early-o1", model: Cuts::new(2, None, 8, 1, 1, none, 1).live().early_timeouts(3), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "the same, member 1 leads view 1" },
        Check { name: "bound-n2-early", model: Cuts::new(2, None, 7, 1, 1, none, 0).live().early_timeouts(3), expect_fail: &["liveness: every end state has a final cut at every honest member"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, 3 early timeouts, 7 views: alternating spoiled views hold finality off" },
        // Not a rule removed: a timeout that never grows past the round trip, so timeouts may fire early in every view.
        Check { name: "n2-live-h2", model: Cuts::new(2, None, 5, 2, 1, none, 0).live(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, liveness, two content cuts: both end final" },
        Check { name: "ctl-skipempty", model: Cuts::new(2, None, 5, 2, 1, none, 0).live().mutate(SkipEmptyCut), expect_fail: &["liveness: every content cut ends final"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "a proposer with nothing new proposes nothing" },
        // Bounded progress (v0.2): two content cuts, so an entry all hold before the first certificate must be in cut 2.
        Check { name: "n2-progress", model: Cuts::new(2, None, 4, 2, 1, none, 0).progress(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, two content cuts, bounded progress (timeouts free)" },
        Check { name: "n3-live-h2", model: Cuts::new(3, None, 5, 2, 2, none, 0).live(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "3 members, honest, two authors, two content cuts, liveness and bounded progress" },
        Check { name: "ctl-ownonly", model: Cuts::new(2, None, 4, 2, 1, none, 0).progress().mutate(OwnEntriesOnly), expect_fail: &["progress: an entry all hold at h is covered by h + 2"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "a proposer covers only its own entries" },
        // Bounded progress under a fault (@design-reviewer's v0.2 Medium): three content heights, so a faulty or silent
        // proposer's cut can come first and the next protocol member's cut must recover the entry.
        Check { name: "n3-silent-progress", model: Cuts::new(3, Silent, 6, 3, 1, none, 0).progress(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "3 members, one silent, one honest author, three content heights, bounded progress" },
        Check { name: "n4-byz-progress", model: Cuts::new(4, Byzantine, 5, 3, 1, quiet, 0).fixed_frames().progress(), expect_fail: &[], random: false, depth: Some(15), budget_s: Some(1800), what: "4 members, Byzantine proposer that may leave the honest entry out, three content heights, bounded progress" },
        Check { name: "walk-n4-progress", model: Cuts::new(4, Byzantine, 5, 3, 1, quiet, 0).fixed_frames().progress(), expect_fail: &[], random: true, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "twin of ctl-skipafterfaulty, unmutated, random walks for 120 s" },
        Check { name: "ctl-skipafterfaulty", model: Cuts::new(4, Byzantine, 5, 3, 1, quiet, 0).fixed_frames().progress().mutate(SkipAfterFaulty), expect_fail: &["progress: an entry all hold at h is covered by h + 2"], random: false, depth: Some(20), budget_s: NO_BUDGET, what: "after a faulty proposer's cut, a proposer adds nothing" },
        // Leader reputation (v0.2): on in every liveness check. Its control skips every member that ever timed out, so
        // at n = 2 after both have, nobody leads and the second content cut never gets its child.
        Check { name: "n2-live-early-h2", model: Cuts::new(2, None, 8, 2, 1, none, 0).live().early_timeouts(2), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, two content cuts, 2 early timeouts, leader reputation" },
        Check { name: "ctl-skipall", model: Cuts::new(2, None, 8, 2, 1, none, 0).live().early_timeouts(2).mutate(SkipEveryTimedOut), expect_fail: &["liveness: every content cut ends final"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "reputation without its window: skip every member that ever timed out" },
        Check { name: "ctl-early", model: Cuts::new(2, None, 6, 1, 1, none, 0).live().early_timeouts(12), expect_fail: &["liveness: every end state has a final cut at every honest member"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, timeouts may always fire before delivery (no adaptive timeout)" },
        // The backoff profile (pe-agree's stall fix, coordinator mdf35a62dc095 item 4): each member's timeout may fire
        // early while its count is below K. Its count halves at each commit (pe-agree's rule), so a slow link spoils a
        // bounded number of views per final cut. The control is the rule it replaced, reset on every certified view.
        // Two content cuts with K = 1 (so early timeouts come back after the first commit) passed 28 M states at 8 views
        // without finishing; the re-armed case is covered by pe-agree's sim tests instead.
        Check { name: "n2-live-backoff", model: Cuts::new(2, None, 6, 1, 1, none, 0).live().backoff(1, Backoff::HalveOnCommit), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, backoff K = 1 halved at each commit, 6 views" },
        Check { name: "bound-n2-backoff", model: Cuts::new(2, None, 3, 1, 1, none, 0).live().backoff(1, Backoff::HalveOnCommit), expect_fail: &["liveness: every end state has a final cut at every honest member"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "the same, 3 views: live in 4 (bound): each member's one early timeout can spoil only a view it does not lead" },
        Check { name: "ctl-backoff-reset", model: Cuts::new(2, None, 8, 1, 1, none, 0).live().backoff(1, Backoff::ResetOnCert), expect_fail: &["liveness: every end state has a final cut at every honest member"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "backoff K = 1 reset on every certified view (the old rule), 8 views: finality never comes" },
        Check { name: "n4-crash-live", model: Cuts::new(4, Silent, 3, 1, 2, none, 0).live(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "4 members, one silent, liveness" },
        Check { name: "n4-byz-v4", model: Cuts::new(4, Byzantine, 4, 1, 0, quiet, 2).fixed_frames(), expect_fail: &[], random: false, depth: Some(18), budget_s: NO_BUDGET, what: "4 members, Byzantine leads view 3 of 4 (the lock's case), safety" },
        Check { name: "n3-root", model: n3(none).corrupt(2), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "3 members, author-faulty, member 2's replica computes a wrong root, safety" },
        Check { name: "n4-root", model: n4(quiet).corrupt(3), expect_fail: &[], random: false, depth: Some(18), budget_s: NO_BUDGET, what: "4 members, Byzantine, member 3's replica computes a wrong root, safety" },
        // Random-walk twins of the two random controls: the same search, unmutated, must find nothing (the walks are
        // not exhaustive; n4-byz-v4 is the exhaustive check of the second where it completes).
        Check { name: "n4-byz-v4b", model: Cuts::new(4, Byzantine, 4, 1, 0, quiet, 0).fixed_frames(), expect_fail: &[], random: false, depth: Some(18), budget_s: NO_BUDGET, what: "twin of ctl-quorum: 4 members, Byzantine leads view 1 of 4, safety" },
        Check { name: "walk-n4-v4", model: Cuts::new(4, Byzantine, 4, 1, 0, quiet, 0).fixed_frames(), expect_fail: &[], random: true, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "twin of ctl-quorum, unmutated, random walks for 120 s" },
        Check { name: "walk-n4-v4-lock", model: Cuts::new(4, Byzantine, 4, 1, 0, quiet, 2).fixed_frames(), expect_fail: &[], random: true, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "twin of ctl-nolock, unmutated, random walks for 120 s" },
        // The size bound, not a control: at n = 3, t = 0, so one equivocating voter splits the two honest members.
        Check { name: "bound-n3-byz", model: Cuts::new(3, Byzantine, 4, 1, 0, quiet, 0), expect_fail: &["agreement"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "3 members, one Byzantine: t = 0, agreement must fail (why n >= 4)" },
        // Controls.
        Check { name: "ctl-d1", model: n3(chain).mutate(VoteWithoutChainValidation), expect_fail: &["no final frame undone"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "d1: vote without validating each chain in order" },
        Check { name: "ctl-unchecked", model: n4(sig).mutate(AcceptProposalUnchecked), expect_fail: &["validity: chains"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "accept the proposer's cut unchecked" },
        Check { name: "ctl-quorum", model: Cuts::new(4, Byzantine, 4, 1, 0, quiet, 0).fixed_frames().mutate(QuorumOneTooSmall), expect_fail: &["agreement"], random: false, depth: Some(18), budget_s: NO_BUDGET, what: "quorum one too small: split-brain" },
        Check { name: "ctl-nolock", model: Cuts::new(4, Byzantine, 4, 1, 0, quiet, 2).fixed_frames().mutate(NoLockRule), expect_fail: &["agreement"], random: false, depth: Some(18), budget_s: NO_BUDGET, what: "vote ignoring the timeout certificate's highest certificate" },
        Check { name: "n4-byz-frames", model: Cuts::new(4, Byzantine, 4, 1, 0, quiet, 0), expect_fail: &[], random: false, depth: Some(12), budget_s: NO_BUDGET, what: "twin of ctl-noframe: 4 members, Byzantine leads view 1 and names any frame from parent + 1 to + 2K" },
        Check { name: "ctl-noframe", model: Cuts::new(4, Byzantine, 4, 1, 0, quiet, 0).mutate(NoFrameCheck), expect_fail: &["no entry takes effect below an earlier final frame"], random: false, depth: Some(10), budget_s: NO_BUDGET, what: "voters accept a frame not above the parent's (v0.1)" },
        Check { name: "ctl-nosig", model: n3(sig).mutate(NoSignatureCheck), expect_fail: &["validity: chains"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "no own-hands / world-id signature check" },
        Check { name: "ctl-noroot", model: n4(quiet).corrupt(3).mutate(VoteWithoutRoot), expect_fail: &["root: a diverged replica learns it at the cut"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "votes carry no root: a diverged replica confirms without learning it" },
        Check { name: "ctl-notimeout", model: n4(quiet).live().mutate(NoTimeouts), expect_fail: &["liveness: every end state has a final cut at every honest member"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "no timeouts: a faulty proposer stalls the line" },
    ]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut filters = Vec::new();
    let mut max_states: Option<usize> = None;
    // Depth-first by default: it keeps only fingerprints and the current path, where breadth-first keeps the whole
    // frontier (the n = 4 Byzantine checks passed 20 GB that way). --bfs gives the shortest counterexamples.
    let mut bfs = false;
    // Depth-bounded: every behaviour of at most N steps, exhaustively (a bounded model check).
    let mut depth: Option<usize> = None;
    let mut budget_override: Option<u64> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--threads" {
            threads = args[i + 1].parse().expect("--threads N");
            i += 2;
        } else if args[i] == "--depth" {
            depth = Some(args[i + 1].parse().expect("--depth N"));
            i += 2;
        } else if args[i] == "--budget" {
            // Override the time budget of the checks that have one (seconds).
            budget_override = Some(args[i + 1].parse().expect("--budget SECONDS"));
            i += 2;
        } else if args[i] == "--bfs" {
            bfs = true;
            i += 1;
        } else if args[i] == "--max-states" {
            max_states = Some(args[i + 1].parse().expect("--max-states N"));
            i += 2;
        } else {
            filters.push(args[i].clone());
            i += 1;
        }
    }
    let mut bad = 0;
    let mut ran = 0;
    let mut partial = 0;
    println!("{:<16} {:>12} {:>13} {:>6} {:>9}  result", "check", "states", "generated", "depth", "seconds");
    for c in suite() {
        if !filters.is_empty() && !filters.iter().any(|f| c.name == f.as_str()) {
            continue;
        }
        ran += 1;
        let names: Vec<(&'static str, bool)> = c.model.properties().iter().map(|p| (p.name, format!("{:?}", p.expectation) == "Sometimes")).collect();
        let entries: Vec<String> = c.model.entries.iter().enumerate().map(|(i, e)| format!("{i}={}", e.name)).collect();
        let t0 = Instant::now();
        let mut c = c;
        c.model.productive_only = c.random;
        let q = c.model.quorum();
        let model = c.model.clone();
        let mut b = c.model.checker().threads(threads);
        if !c.expect_fail.is_empty() {
            b = b.finish_when(HasDiscoveries::AnyOf(c.expect_fail.iter().copied().collect()));
        }
        if let Some(m) = max_states {
            b = b.target_state_count(m);
        }
        let depth = depth.or(c.depth);

        // Progress goes to stderr every few seconds.
        let b = b.timeout(Duration::from_secs(3600));
        let mut err = std::io::stderr();
        let mut rep = WriteReporter::new(&mut err);
        eprintln!("== {}", c.name);
        let mut unfinished = false;
        let ch = if let Some(d) = depth.filter(|_| !c.random) {
            // Depth-bounded: our own search (see bounded.rs), sound up to the bound and light on memory.
            let model2 = model.clone();
            let tick = |u: usize, v: usize| eprintln!("Checking (bounded). unique={u}, visits={v}");
            let budget = c.budget_s.map(|b| Duration::from_secs(budget_override.unwrap_or(b)));
            let r = bounded::check(&model2, d, threads, c.expect_fail, budget, &tick);
            unfinished = r.unfinished;
            Summary { unique: r.unique, total: r.visits, depth: r.depth, found: r.found }
        } else if c.random {
            let secs = if c.expect_fail.is_empty() { 120 } else { 900 };
            Summary::of(b.timeout(Duration::from_secs(secs)).spawn_simulation(20261110, UniformChooser).report(&mut rep))
        } else if bfs {
            Summary::of(b.spawn_bfs().report(&mut rep))
        } else {
            Summary::of(b.spawn_dfs().report(&mut rep))
        };
        let secs = t0.elapsed().as_secs_f64();
        let found = &ch.found;
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
        // A base check that stopped at the state cap or the time limit is not exhaustive: that is a failure.
        let capped = max_states.is_some_and(|m| ch.total >= m) || secs >= 3599.0;
        let ok = ok && !(capped && c.expect_fail.is_empty());
        let capped = capped && !c.random;
        let done = if c.random { " (random walks)".to_string() } else if let (true, Some(d)) = (unfinished, depth) { format!(" (UNFINISHED at its {} s budget: behaviours of up to {d} steps, partly; no violation in what was searched, not a proof)", budget_override.or(c.budget_s).unwrap_or(0)) } else if let Some(d) = depth { format!(" (every behaviour of up to {d} steps)") } else if capped { " (STOPPED AT THE CAP: not exhaustive)".to_string() } else { String::new() };
        println!("{:<16} {:>12} {:>13} {:>6} {:>9.1}  {}{} {}", c.name, ch.unique, ch.total, ch.depth, secs, if ok { "OK  " } else { "FAIL" }, done, verdict.join("; "));
        println!("    {}; q = {}; entries {}", c.what, q, entries.join(" "));
        if c.expect_fail.is_empty() {
            // An unexpected counterexample: print it shrunk too.
            for (p, sometimes) in &names {
                if let (false, Some(path)) = (*sometimes, found.get(p)) {
                    let short = shrink(&model, p, path.clone());
                    println!("    counterexample to \"{p}\" ({} steps as found, {} after shrinking):", path.len(), short.len());
                    for a in &short {
                        println!("      {a:?}");
                    }
                }
            }
        } else {
            for p in c.expect_fail {
                if let Some(path) = found.get(p) {
                    let short = shrink(&model, p, path.clone());
                    println!("    counterexample to \"{p}\" ({} steps as found, {} after shrinking):", path.len(), short.len());
                    for a in &short {
                        println!("      {a:?}");
                    }
                    break;
                }
            }
        }
        if !ok {
            bad += 1;
        } else if unfinished {
            partial += 1;
        }
    }
    if bad > 0 {
        println!("{bad} check(s) did not meet their expectation");
        std::process::exit(1);
    }
    if partial > 0 {
        println!("all {ran} check(s) met their expectation; {partial} of them UNFINISHED at a time budget (bounded, not exhaustive)");
    } else {
        println!("all {ran} check(s) met their expectation");
    }
}
