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
        // The backoff profile (pe-agree's stall fix, coordinator item 4): each member's timeout may fire
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
        // v0.3, the revote case (coordinator): a wrong root, a repair at any time after the member's first
        // vote, and its correct vote in a later view (a member votes once per view). Lock and finality are unchanged.
        Check { name: "n2-repair-live", model: Cuts::new(2, None, 6, 1, 1, none, 0).corrupt(1).repairable().live(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, member 1's root wrong until repaired, liveness: final with the right root" },
        Check { name: "bound-n2-wrongroot", model: Cuts::new(2, None, 6, 1, 1, none, 0).corrupt(1).live(), expect_fail: &["liveness: every end state has a final cut at every honest member"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, member 1's root wrong and never repaired: nothing is final (why the repair must reach a later view)" },
        Check { name: "n3-root-repair", model: n3(none).corrupt(2).repairable(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "3 members, author-faulty, member 2's root wrong until repaired, safety" },
        Check { name: "n4-root-repair", model: n4(quiet).corrupt(3).repairable(), expect_fail: &[], random: false, depth: Some(18), budget_s: NO_BUDGET, what: "4 members, Byzantine, member 3's root wrong until repaired, safety" },
        // The pinned split cut (L4', coordinator): a repair that takes k views, for its episode's cut. With
        // the pin, the split cut is proposed again until it is certified, so the repair finishes and the cut ends final;
        // without it (NoPin, a newer frame each view), every view is a new episode and nothing is ever final.
        Check { name: "n2-pin-k2", model: Cuts::new(2, None, 8, 1, 1, none, 0).fixed_frames().corrupt(1).repair_in(2).live(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "2 members, member 1's root wrong until a repair of 2 views at its split cut, pinned: final" },
        Check { name: "n2-pin-k3", model: Cuts::new(2, None, 9, 1, 1, none, 0).fixed_frames().corrupt(1).repair_in(3).live(), expect_fail: &[], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "the same, a repair of 3 views, 9 views" },
        Check { name: "ctl-nopin", model: Cuts::new(2, None, 8, 1, 1, none, 0).fixed_frames().corrupt(1).repair_in(2).live().mutate(NoPin), expect_fail: &["liveness: every end state has a final cut at every honest member"], random: false, depth: UNBOUNDED, budget_s: NO_BUDGET, what: "the same without the pin: each re-proposal at a newer frame restarts the repair" },
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

/// Model v1's suite (agreement.md 8): `cuts-model --epochs [--depth D] [--budget S] [--threads N] [filter...]`.
/// Shorten an epochs counterexample the same way: drop steps while the trace still replays exactly and still ends in
/// a state that breaks the property.
fn shrink_epochs(m: &cuts_model::epochs::Epochs, prop: &str, mut acts: Vec<cuts_model::epochs::EAction>) -> Vec<cuts_model::epochs::EAction> {
    let cond = m.properties().into_iter().find(|p| p.name == prop).expect("property").condition;
    let breaks = |t: &[cuts_model::epochs::EAction]| {
        let mut s = m.init_states().remove(0);
        for a in t {
            let mut en = Vec::new();
            m.actions(&s, &mut en);
            if !en.contains(a) {
                return false;
            }
            match m.next_state(&s, a.clone()) {
                Some(n) => s = n,
                None => return false,
            }
        }
        !cond(m, &s)
    };
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

fn run_epochs(args: &[String], threads: usize) {
    use cuts_model::epochs;
    let mut depth: Option<usize> = None;
    let mut budget = 600u64;
    let mut walks: Option<u64> = None;
    let mut no_stop = false;
    let mut filters = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--epochs" => i += 1,
            "--depth" => {
                depth = Some(args[i + 1].parse().expect("--depth D"));
                i += 2;
            }
            "--budget" => {
                budget = args[i + 1].parse().expect("--budget S");
                i += 2;
            }
            "--threads" => i += 2,
            "--no-stop" => {
                no_stop = true;
                i += 1;
            }
            "--walks" => {
                walks = Some(args[i + 1].parse().expect("--walks N"));
                i += 2;
            }
            a => {
                filters.push(a.to_string());
                i += 1;
            }
        }
    }
    let (mut bad, mut ran, mut partial) = (0, 0, 0);
    for row in epochs::suite() {
        let m = &row.model;
        if !filters.is_empty() && !filters.iter().any(|f| m.name.contains(f.as_str())) {
            continue;
        }
        ran += 1;
        let d = depth.unwrap_or(row.depth);
        let t0 = Instant::now();
        eprintln!("== {}", m.name);
        let tick = |u: usize, v: usize| eprintln!("Checking (bounded). unique={u}, visits={v}");
        let mut walked: Option<(u64, Option<u64>)> = None;
        let r = if row.random {
            let mut walk = m.clone();
            walk.productive_only = true;
            let n = walks.or(row.walks).unwrap_or(epochs::DEFAULT_WALKS);
            let wtick = |w: u64, s: usize| eprintln!("Walking. walks={w}, states={s}");
            // No time budget: the walk count bounds the row, so its outcome is the same on every run.
            let w = bounded::walks(&walk, n, threads, 20261118, row.expect_fail, None, &wtick);
            walked = Some((w.walks, w.bit_at));
            bounded::Bounded { unique: w.states, visits: w.states, depth: w.depth, found: w.found, unfinished: w.unfinished }
        } else {
            // --no-stop: search a control's whole bounded space (to measure what its depth costs at worst).
            let stop: &[&'static str] = if no_stop { &[] } else { row.expect_fail };
            // A control runs without a time budget: its depth is its shrunk counterexample's length, so the exhaustive
            // search to that depth finds a counterexample whatever the thread count or timing (where a search with a
            // budget stops depended on both: ctl-f2-highqc bit in 8 s at 3 threads, not within 100 s at 16).
            let cap = if row.expect_fail.is_empty() { Some(Duration::from_secs(budget)) } else { None };
            let mut r = bounded::check(m, d, threads, stop, cap, &tick);
            // A base row cut off by its budget may not have reached its examples yet: look for those by random walks
            // (an example needs one path, not the whole space).
            let missing: Vec<&'static str> = m
                .properties()
                .iter()
                .filter(|p| matches!(p.expectation, stateright::Expectation::Sometimes) && !r.found.contains_key(p.name))
                .map(|p| p.name)
                .collect();
            if row.expect_fail.is_empty() && r.unfinished && !missing.is_empty() {
                let mut walk = m.clone();
                walk.productive_only = true;
                let w = bounded::walks(&walk, 20_000, threads, 20261118, &[], Some(Duration::from_secs(120)), &|_, _| {});
                for (k, p) in w.found {
                    if missing.contains(&k) {
                        eprintln!("example \"{k}\" found by random walks");
                        r.found.insert(k, p);
                    }
                }
            }
            r
        };
        let secs = t0.elapsed().as_secs_f64();
        let props = m.properties();
        let mut verdict = Vec::new();
        let ok = if row.expect_fail.is_empty() {
            let mut ok = true;
            for p in &props {
                let found = r.found.contains_key(p.name);
                let sometimes = matches!(p.expectation, stateright::Expectation::Sometimes);
                if sometimes && !found {
                    verdict.push(format!("MISSING example \"{}\"", p.name));
                    ok = false;
                } else if !sometimes && found {
                    verdict.push(format!("COUNTEREXAMPLE \"{}\"", p.name));
                    ok = false;
                }
            }
            if ok {
                verdict.push("all properties hold; examples found".into());
            }
            ok
        } else {
            let hit: Vec<&&str> = row.expect_fail.iter().filter(|p| r.found.contains_key(**p)).collect();
            if hit.is_empty() {
                verdict.push("NO COUNTEREXAMPLE (the control does not bite)".into());
                false
            } else {
                verdict.push(format!("counterexample as expected: {}", hit.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")));
                true
            }
        };
        let scope = if let Some((w, bit)) = walked {
            let n = walks.or(row.walks).unwrap_or(epochs::DEFAULT_WALKS);
            match (bit, r.unfinished) {
                (Some(k), false) => format!(" (seeded random walks: bit at walk {k} of {n})"),
                (Some(k), true) => format!(" (seeded random walks: bit at walk {k}; UNFINISHED at its {budget} s budget after {w} of {n} walks, so a lower walk may bite too)"),
                (None, false) => format!(" (seeded random walks: all {n})"),
                (None, true) => format!(" (seeded random walks: UNFINISHED at its {budget} s budget after {w} of {n} walks)"),
            }
        } else if r.unfinished { format!(" (UNFINISHED at its {budget} s budget: behaviours of up to {d} steps, partly)") } else { format!(" (every behaviour of up to {d} steps)") };
        println!("{:<18} {:>12} {:>13} {:>6} {:>9.1}  {}{} {}", m.name, r.unique, r.visits, r.depth, secs, if ok { "OK  " } else { "FAIL" }, scope, verdict.join("; "));
        println!("    {}", row.what);
        for p in &props {
            let sometimes = matches!(p.expectation, stateright::Expectation::Sometimes);
            if let Some(path) = r.found.get(p.name) {
                if sometimes && row.expect_fail.is_empty() {
                    continue;
                }
                if sometimes {
                    println!("    example \"{}\" ({} steps):", p.name, path.len());
                    for a in path {
                        println!("      {a:?}");
                    }
                } else {
                    let short = shrink_epochs(m, p.name, path.clone());
                    println!("    counterexample to \"{}\" ({} steps as found, {} after shrinking):", p.name, path.len(), short.len());
                    for a in &short {
                        println!("      {a:?}");
                    }
                }
            }
        }
        if !ok {
            bad += 1;
        } else if r.unfinished && row.expect_fail.is_empty() {
            partial += 1;
        }
    }
    if bad > 0 {
        println!("{bad} check(s) did not meet their expectation");
        std::process::exit(1);
    }
    println!("all {ran} epoch check(s) met their expectation{}", if partial > 0 { format!("; {partial} of them UNFINISHED at a time budget") } else { String::new() });
}

/// Model v1, a lemma control's state run forward to see whether the broken lemma forks the line (@design-reviewer): the control's shortest counterexample to its lemma is replayed as a fixed prefix, then a bounded
/// search from the state it reaches checks agreement only. The same prefix replayed on the model without the mutation
/// (each step matched by kind, sender and target, since a message's content can differ) must hold.
mod directed {
    use crate::bounded;
    use cuts_model::epochs::{Blk, EAction, EState, Epochs, Msg};
    use stateright::{Checker, Model, Property};
    use std::time::Duration;

    #[derive(Clone)]
    pub struct From {
        pub inner: Epochs,
        pub start: EState,
        pub cond: fn(&Epochs, &EState) -> bool,
    }

    impl Model for From {
        type State = EState;
        type Action = EAction;
        fn init_states(&self) -> Vec<EState> {
            vec![self.start.clone()]
        }
        fn actions(&self, s: &EState, acts: &mut Vec<EAction>) {
            self.inner.actions(s, acts)
        }
        fn next_state(&self, s: &EState, a: EAction) -> Option<EState> {
            self.inner.next_state(s, a)
        }
        fn properties(&self) -> Vec<Property<Self>> {
            vec![Property::always("agreement", |w: &From, s: &EState| (w.cond)(&w.inner, s))]
        }
    }

    /// The same step, up to a message's content: kind, sender and target.
    fn like(a: &EAction, b: &EAction) -> bool {
        use EAction::*;
        match (a, b) {
            (Deliver(t1, m1), Deliver(t2, m2)) => {
                t1 == t2
                    && match (m1, m2) {
                        (Msg::Entry { to: x, e: y }, Msg::Entry { to: u, e: v }) => x == u && y == v,
                        (Msg::Propose { b: x }, Msg::Propose { b: y }) => x.h == y.h && x.ev() == y.ev() && x.by == y.by,
                        (Msg::Vote { voter: x, b: bx }, Msg::Vote { voter: y, b: by }) => x == y && bx.h == by.h && bx.ev() == by.ev(),
                        (Msg::Timeout { voter: x, ev: ex, .. }, Msg::Timeout { voter: y, ev: ey, .. }) => x == y && ex == ey,
                        _ => false,
                    }
            }
            (Observe(x, bx), Observe(y, by)) => x == y && bx.h == by.h && bx.ev() == by.ev(),
            (AdvTimeout(x, _), AdvTimeout(y, _)) => x == y,
            _ => a == b,
        }
    }

    /// Replay `trace` on `m`: each step exactly if it is enabled, else the one enabled step like it. Returns the state
    /// and how many steps were matched by likeness, or the step that has no match.
    pub fn replay(m: &Epochs, trace: &[EAction]) -> Result<(EState, usize), String> {
        let mut s = m.init_states().remove(0);
        let mut fuzzy = 0;
        for (i, a) in trace.iter().enumerate() {
            let mut acts = Vec::new();
            m.actions(&s, &mut acts);
            let pick = if acts.contains(a) {
                a.clone()
            } else if let Some(b) = acts.iter().find(|b| like(a, b)) {
                fuzzy += 1;
                b.clone()
            } else {
                return Err(format!("step {i} {a:?} has no match"));
            };
            s = m.next_state(&s, pick).ok_or_else(|| format!("step {i} {a:?} leads nowhere"))?;
        }
        Ok((s, fuzzy))
    }

    /// The continuation runs with `views` views and the Byzantine member proposing (the attack's fork is its proposal;
    /// the lemma controls keep it quiet to keep their space small).
    type Step = (&'static str, Box<dyn Fn(&EAction, &[Blk]) -> bool>);

    /// The (f2) attack as one fixed schedule from the start (@design-reviewer's five steps), at n = 4
    /// with member 0 Byzantine (q = 3). Member 1 votes B2 in view 2 and restarts before it sees B2 certified; with its
    /// high_qc forgotten it times out in view 2 at genesis. Member 3 never saw a certificate, so a timeout certificate
    /// for view 2 from members 0, 1 and 3 has genesis as its highest: member 3, leading view 3, proposes off its
    /// high_qc, members 1 and 3 vote for it (the lock allows it), and the Byzantine leader of view 4 proposes its child.
    /// B1 is final at member 2 (B1, B2 certified in views 1, 2) and the view-3 block at member 3.
    fn f2_attack() -> Vec<Step> {
        use EAction::*;
        let at = |v: u8| move |b: &Blk| b.ev() == v;
        let fork4 = |b: &Blk| b.ev() == 4 && b.h >= 1 && b.parent().ev() == 3 && b.frame() > b.parent().frame() && b.inc() == b.parent().inc();
        vec![
            ("1 proposes B1 in view 1", Box::new(|a, _| *a == Propose(1))),
            ("1 votes B1", Box::new(move |a, _| matches!(a, Deliver(1, Msg::Propose { b }) if at(1)(b)))),
            ("2 votes B1", Box::new(move |a, _| matches!(a, Deliver(2, Msg::Propose { b }) if at(1)(b)))),
            ("0 votes B1: certified", Box::new(move |a, _| matches!(a, AdvVote(b) if at(1)(b)))),
            ("1 times out in view 1 (high genesis)", Box::new(|a, _| *a == Timeout(1))),
            ("2 times out in view 1 (high genesis)", Box::new(|a, _| *a == Timeout(2))),
            ("2 sees B1 certified: view 2", Box::new(move |a, _| matches!(a, Observe(2, b) if at(1)(b)))),
            ("1 sees B1 certified: view 2", Box::new(move |a, _| matches!(a, Observe(1, b) if at(1)(b)))),
            ("2 proposes B2 in view 2", Box::new(|a, _| *a == Propose(2))),
            ("1 votes B2", Box::new(move |a, _| matches!(a, Deliver(1, Msg::Propose { b }) if at(2)(b)))),
            ("2 votes B2", Box::new(move |a, _| matches!(a, Deliver(2, Msg::Propose { b }) if at(2)(b)))),
            ("0 votes B2: certified", Box::new(move |a, _| matches!(a, AdvVote(b) if at(2)(b)))),
            ("2 sees B2 certified: B1 final at 2", Box::new(move |a, _| matches!(a, Observe(2, b) if at(2)(b)))),
            ("1 restarts", Box::new(|a, _| *a == Restart(1))),
            ("1 times out in view 2", Box::new(|a, _| *a == Timeout(1))),
            ("3 times out in view 1", Box::new(|a, _| *a == Timeout(3))),
            ("0 times out in view 1", Box::new(|a, _| *a == AdvTimeout(1, Blk::GENESIS))),
            ("3 enters view 2 by the view-1 TC", Box::new(|a, _| *a == AdvanceTc(3))),
            ("3 times out in view 2", Box::new(|a, _| *a == Timeout(3))),
            ("0 times out in view 2", Box::new(|a, _| *a == AdvTimeout(2, Blk::GENESIS))),
            ("3 enters view 3 by the view-2 TC {0, 1, 3}", Box::new(|a, _| *a == AdvanceTc(3))),
            ("1 enters view 3 by it", Box::new(|a, _| *a == AdvanceTc(1))),
            ("3 proposes in view 3 off its high_qc", Box::new(|a, _| *a == Propose(3))),
            ("1 votes it", Box::new(move |a, _| matches!(a, Deliver(1, Msg::Propose { b }) if at(3)(b)))),
            ("3 votes it", Box::new(move |a, _| matches!(a, Deliver(3, Msg::Propose { b }) if at(3)(b)))),
            ("0 votes it: certified", Box::new(move |a, _| matches!(a, AdvVote(b) if at(3)(b)))),
            ("3 sees it certified: view 4", Box::new(move |a, _| matches!(a, Observe(3, b) if at(3)(b)))),
            ("1 sees it certified: view 4", Box::new(move |a, _| matches!(a, Observe(1, b) if at(3)(b)))),
            ("0 proposes its child in view 4", Box::new(move |a, _| matches!(a, AdvPropose(b) if fork4(b)))),
            ("1 votes it", Box::new(move |a, _| matches!(a, Deliver(1, Msg::Propose { b }) if fork4(b)))),
            ("3 votes it", Box::new(move |a, _| matches!(a, Deliver(3, Msg::Propose { b }) if fork4(b)))),
            ("0 votes it: certified", Box::new(move |a, _| matches!(a, AdvVote(b) if fork4(b)))),
            ("3 sees it certified: the view-3 block final at 3", Box::new(move |a, _| matches!(a, Observe(3, b) if fork4(b)))),
        ]
    }

    /// The (f1) attack: a double vote, then a fork. n = 4, member 0 Byzantine. Every member times out to view 4, led by
    /// member 0, which proposes two siblings off genesis: A (empty) and A' (with entry o2). A is certified by 0, 1, 2.
    /// Member 1 restarts and forgets its views, catches up to view 4 by the timeout certificates, and votes A' too: A'
    /// is certified by 0, 1, 3. Member 1 leads view 5 with A's child, so A is final at member 1. Member 2, whose
    /// high_qc is A', leads view 6 with E off A' (the view-5 timeout certificate's highest is A' too), member 3 leads
    /// view 7 with E's child, and E is final at member 3: two final blocks on two branches.
    fn f1_attack() -> Vec<Step> {
        use EAction::*;
        let a = |b: &Blk| b.h == 1 && b.ev() == 4 && b.frame() > 0 && b.inc() == 0;
        let a2 = |b: &Blk| b.h == 1 && b.ev() == 4 && b.frame() > 0 && b.inc() != 0;
        let c = move |b: &Blk| b.h == 2 && b.ev() == 5 && a(&b.parent());
        let e = move |b: &Blk| b.h == 2 && b.ev() == 6 && a2(&b.parent());
        let f = move |b: &Blk| b.h == 3 && b.ev() == 7 && e(&b.parent());
        let mut v: Vec<Step> = Vec::new();
        for view in 1..=3u8 {
            for k in 1..=3u8 {
                v.push(("times out", Box::new(move |x, _| *x == Timeout(k))));
            }
            let _ = view;
            for k in 1..=3u8 {
                v.push(("enters the next view by the TC", Box::new(move |x, _| *x == AdvanceTc(k))));
            }
        }
        let more: Vec<Step> = vec![
            ("0 proposes A in view 4", Box::new(move |x, _| matches!(x, AdvPropose(b) if a(b)))),
            ("0 proposes A' in view 4", Box::new(move |x, _| matches!(x, AdvPropose(b) if a2(b)))),
            ("1 votes A", Box::new(move |x, _| matches!(x, Deliver(1, Msg::Propose { b }) if a(b)))),
            ("2 votes A", Box::new(move |x, _| matches!(x, Deliver(2, Msg::Propose { b }) if a(b)))),
            ("0 votes A: certified", Box::new(move |x, _| matches!(x, AdvVote(b) if a(b)))),
            ("3 gets o2", Box::new(|x, _| matches!(x, Deliver(3, Msg::Entry { .. })))),
            ("3 votes A'", Box::new(move |x, _| matches!(x, Deliver(3, Msg::Propose { b }) if a2(b)))),
            ("0 votes A'", Box::new(move |x, _| matches!(x, AdvVote(b) if a2(b)))),
            ("1 restarts", Box::new(|x, _| *x == Restart(1))),
            ("1 catches up: view 2", Box::new(|x, _| *x == AdvanceTc(1))),
            ("1 catches up: view 3", Box::new(|x, _| *x == AdvanceTc(1))),
            ("1 catches up: view 4", Box::new(|x, _| *x == AdvanceTc(1))),
            ("1 gets o2", Box::new(|x, _| matches!(x, Deliver(1, Msg::Entry { .. })))),
            ("1 votes A' too: A' certified", Box::new(move |x, _| matches!(x, Deliver(1, Msg::Propose { b }) if a2(b)))),
            ("1 sees A certified: view 5", Box::new(move |x, _| matches!(x, Observe(1, b) if a(b)))),
            ("2 sees A' certified: view 5", Box::new(move |x, _| matches!(x, Observe(2, b) if a2(b)))),
            ("1 proposes C off A in view 5", Box::new(|x, _| *x == Propose(1))),
            ("1 votes C", Box::new(move |x, _| matches!(x, Deliver(1, Msg::Propose { b }) if c(b)))),
            ("2 votes C", Box::new(move |x, _| matches!(x, Deliver(2, Msg::Propose { b }) if c(b)))),
            ("0 votes C: certified", Box::new(move |x, _| matches!(x, AdvVote(b) if c(b)))),
            ("1 sees C certified: A final at 1", Box::new(move |x, _| matches!(x, Observe(1, b) if c(b)))),
            ("3 sees A' certified: view 5", Box::new(move |x, _| matches!(x, Observe(3, b) if a2(b)))),
            ("2 times out in view 5 (high A')", Box::new(|x, _| *x == Timeout(2))),
            ("3 times out in view 5 (high A')", Box::new(|x, _| *x == Timeout(3))),
            ("0 times out in view 5", Box::new(|x, _| *x == AdvTimeout(5, Blk::GENESIS))),
            ("2 enters view 6", Box::new(|x, _| *x == AdvanceTc(2))),
            ("3 enters view 6", Box::new(|x, _| *x == AdvanceTc(3))),
            ("2 proposes E off A' in view 6", Box::new(|x, _| *x == Propose(2))),
            ("2 votes E", Box::new(move |x, _| matches!(x, Deliver(2, Msg::Propose { b }) if e(b)))),
            ("3 votes E", Box::new(move |x, _| matches!(x, Deliver(3, Msg::Propose { b }) if e(b)))),
            ("0 votes E: certified", Box::new(move |x, _| matches!(x, AdvVote(b) if e(b)))),
            ("3 sees E certified: view 7", Box::new(move |x, _| matches!(x, Observe(3, b) if e(b)))),
            ("2 sees E certified: view 7", Box::new(move |x, _| matches!(x, Observe(2, b) if e(b)))),
            ("3 proposes F off E in view 7", Box::new(|x, _| *x == Propose(3))),
            ("3 votes F", Box::new(move |x, _| matches!(x, Deliver(3, Msg::Propose { b }) if f(b)))),
            ("2 votes F", Box::new(move |x, _| matches!(x, Deliver(2, Msg::Propose { b }) if f(b)))),
            ("0 votes F: certified", Box::new(move |x, _| matches!(x, AdvVote(b) if f(b)))),
            ("3 sees F certified: E final at 3", Box::new(move |x, _| matches!(x, Observe(3, b) if f(b)))),
        ];
        v.extend(more);
        v
    }

    /// Run a scripted schedule from `s`; each step is the first enabled action it matches. Stops at the first step with
    /// no match and says so, with the enabled actions there.
    fn script(m: &Epochs, mut s: EState, steps: &[Step], cond: fn(&Epochs, &EState) -> bool) {
        let mut made: Vec<Blk> = Vec::new();
        for (i, (what, f)) in steps.iter().enumerate() {
            let mut acts = Vec::new();
            m.actions(&s, &mut acts);
            let Some((a, next)) = acts.iter().filter(|a| f(a, &made)).find_map(|a| m.next_state(&s, a.clone()).map(|n| (a.clone(), n))) else {
                println!("    step {}: \"{what}\": NOT ENABLED. Enabled here:", i + 1);
                for a in &acts {
                    println!("      {a:?}");
                }
                println!("    agreement {} at the stop", if cond(m, &s) { "holds" } else { "BROKEN" });
                for (k, mm) in s.m.iter().enumerate() {
                    println!("    member {k}: view {} voted {:#x} tout {:#x} high {:?} fin {:?}", mm.view, mm.voted, mm.tout, mm.high, mm.fin);
                }
                return;
            };
            if let EAction::AdvPropose(b) = &a {
                made.push(*b);
            }
            println!("    step {}: {what}: {a:?}", i + 1);
            s = next;
        }
        println!("    agreement {} after the script", if cond(m, &s) { "holds" } else { "BROKEN" });
    }

    pub fn run(row: &str, lemma: &'static str, depth: usize, budget: u64, threads: usize, views: u8) {
        let row_m = cuts_model::epochs::suite().into_iter().find(|r| r.model.name == row).expect("a v1 row").model;
        let tick = |_: usize, _: usize| {};
        let pre = bounded::check(&row_m, 22, threads, &[lemma], Some(Duration::from_secs(600)), &tick);
        let trace = pre.found.get(lemma).expect("the control's lemma counterexample").clone();
        println!("{row}: prefix, the shortest counterexample to \"{lemma}\": {} steps", trace.len());
        for a in &trace {
            println!("      {a:?}");
        }
        let cond = row_m.properties().into_iter().find(|p| p.name == "agreement").expect("agreement").condition;
        let mut wide = row_m.clone();
        wide.views = views;
        wide.byz_proposes = true;
        let mut plain = wide.clone();
        plain.mutation = None;
        println!("continuation: {views} views, the Byzantine member proposing");
        for (label, m) in [("with the mutation", &wide), ("without it (the rule kept)", &plain)] {
            if depth == 0 {
                println!("{label}: the scripted attack, from the start");
                let steps = if row == "ctl-f1-views" { f1_attack() } else { f2_attack() };
                script(m, m.init_states().remove(0), &steps, cond);
                continue;
            }
            match replay(m, &trace) {
                Err(e) => println!("{label}: the prefix doesn't replay: {e}"),
                Ok((start, fuzzy)) => {
                    let w = From { inner: m.clone(), start, cond };
                    let t0 = std::time::Instant::now();
                    let r = bounded::check(&w, depth, threads, &["agreement"], Some(Duration::from_secs(budget)), &tick);
                    let secs = t0.elapsed().as_secs_f64();
                    let scope = if r.unfinished { format!("UNFINISHED at {budget} s, up to {depth} more steps partly") } else { format!("every continuation of up to {depth} steps") };
                    println!(
                        "{label} ({fuzzy} prefix steps matched by likeness): {} states, {secs:.1} s, {scope}: {}",
                        r.unique,
                        if r.found.contains_key("agreement") { "AGREEMENT BROKEN" } else { "agreement holds" }
                    );
                    if let Some(path) = r.found.get("agreement") {
                        println!("    continuation ({} steps):", path.len());
                        for a in path {
                            println!("      {a:?}");
                        }
                        continue;
                    }
                    // Deeper than the bound: seeded random walks from the same state (productive steps only).
                    let mut walker = w.clone();
                    walker.inner.productive_only = true;
                    let t0 = std::time::Instant::now();
                    let ch = walker
                        .checker()
                        .threads(threads)
                        .timeout(Duration::from_secs(budget))
                        .finish_when(stateright::HasDiscoveries::AnyOf(["agreement"].into_iter().collect()))
                        .spawn_simulation(20261117, stateright::UniformChooser)
                        .join();
                    let found = ch.discoveries().remove("agreement");
                    println!(
                        "{label}, random walks: {} states, depth {}, {:.1} s: {}",
                        ch.unique_state_count(),
                        ch.max_depth(),
                        t0.elapsed().as_secs_f64(),
                        if found.is_some() { "AGREEMENT BROKEN" } else { "agreement holds" }
                    );
                    if let Some(p) = found {
                        let path = p.into_actions();
                        println!("    continuation ({} steps):", path.len());
                        for a in path {
                            println!("      {a:?}");
                        }
                    }
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--directed") {
        // cuts-model --directed <row> "<lemma>" <depth> <budget s> <threads> <views>
        let threads: usize = args.get(i + 5).and_then(|t| t.parse().ok()).unwrap_or(4);
        let views: u8 = args.get(i + 6).and_then(|t| t.parse().ok()).unwrap_or(6);
        let lemma: &'static str = Box::leak(args[i + 2].clone().into_boxed_str());
        directed::run(&args[i + 1], lemma, args[i + 3].parse().unwrap(), args[i + 4].parse().unwrap(), threads, views);
        return;
    }
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    if let Some(i) = args.iter().position(|a| a == "--threads") {
        threads = args[i + 1].parse().expect("--threads N");
    }
    if args.iter().any(|a| a == "--epochs") {
        run_epochs(&args, threads);
        return;
    }
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
