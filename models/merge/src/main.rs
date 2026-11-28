//! merge-model: run model v2's checks. `merge-model [tier1|full] [layer2] [--threads N]`. Exits non-zero if any outcome
//! differs from its expectation: the unmutated merge must hold every property and reach every example of 9.5; each
//! control must give a counterexample to the property 9.5 names for it. The first counterexample of each is printed
//! and written to results/<profile>-<control>.txt, for merge engine v1's tests to replay.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use merge_model::enumerate::{Profile, for_each};
use merge_model::members::{self, Members};
use merge_model::merge::{Input, Mutation, merge};
use merge_model::props::{EXAMPLES, Violation, check, expected};
use stateright::{Checker, Model};

const CONTROLS: [Mutation; 9] = [
    Mutation::None,
    Mutation::MergeByArrival,
    Mutation::ApplyAtStamp,
    Mutation::NoWitnessCheck,
    Mutation::RefuseAllEarlier,
    Mutation::MergeDropsSide,
    Mutation::VoidSplitEntries,
    Mutation::NoDedupe,
    Mutation::UnionAdminsAlways,
];

type First<T> = (u64, Option<T>);

#[derive(Default)]
struct Found {
    inputs: u64,
    by_property: BTreeMap<&'static str, First<(Input, Violation)>>,
    examples: BTreeMap<&'static str, First<Input>>,
}

impl Found {
    fn absorb(&mut self, other: Found) {
        self.inputs += other.inputs;
        for (k, (n, first)) in other.by_property {
            let e = self.by_property.entry(k).or_default();
            e.0 += n;
            if e.1.is_none() {
                e.1 = first;
            }
        }
        for (k, (n, first)) in other.examples {
            let e = self.examples.entry(k).or_default();
            e.0 += n;
            if e.1.is_none() {
                e.1 = first;
            }
        }
    }
}

/// Runs every input of `profile` under `m`. The unmutated merge sees every input; a control stops at its first
/// counterexample to a property it must break (so the tier-1 run stays time-bounded).
fn run(profile: &Profile, m: Mutation, threads: usize) -> Found {
    let found = Mutex::new(Found::default());
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for shard in 0..threads {
            let (found, stop) = (&found, &stop);
            s.spawn(move || {
                let mut local = Found::default();
                for_each(profile, shard, threads, |input| {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    local.inputs += 1;
                    for viol in check(input, m) {
                        if m != Mutation::None && expected(m).contains(&viol.property) {
                            stop.store(true, Ordering::Relaxed);
                        }
                        let e = local.by_property.entry(viol.property).or_default();
                        e.0 += 1;
                        if e.1.is_none() {
                            e.1 = Some((input.clone(), viol));
                        }
                    }
                    if m == Mutation::None {
                        let got = merge(input, m);
                        for (name, pred) in EXAMPLES {
                            if pred(input, &got) {
                                let e = local.examples.entry(name).or_default();
                                e.0 += 1;
                                if e.1.is_none() {
                                    e.1 = Some(input.clone());
                                }
                            }
                        }
                    }
                });
                found.lock().expect("the results").absorb(local);
            });
        }
    });
    found.into_inner().expect("the results")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let threads = args
        .iter()
        .position(|a| a == "--threads")
        .and_then(|i| args.get(i + 1))
        .and_then(|t| t.parse().ok())
        .unwrap_or(4usize);
    let profile = if args.iter().any(|a| a == "full") { Profile::full() } else { Profile::tier1() };
    let mut ok = true;
    std::fs::create_dir_all("results").ok();
    // Layer 1: the profile under every control, then step 2's profile under its two controls. `layer2` alone skips
    // layer 1.
    let mut plan: Vec<(Profile, Mutation)> = CONTROLS.iter().map(|&m| (profile.clone(), m)).collect();
    for m in [Mutation::None, Mutation::GlobalBudget, Mutation::StaleAdmSoFar] {
        plan.push((Profile::step2(), m));
    }
    if args.iter().any(|a| a == "layer2") {
        plan.clear();
    }
    for (profile, m) in plan {
        let started = Instant::now();
        let f = run(&profile, m, threads);
        // Step 2's profile is for its controls; the examples are the main profile's.
        let examples = profile.name != "step2";
        let secs = started.elapsed().as_secs_f64();
        let want = expected(m);
        let broke: Vec<&str> = f.by_property.keys().copied().collect();
        let pass = if m == Mutation::None {
            broke.is_empty() && (!examples || EXAMPLES.iter().all(|(n, _)| f.examples.contains_key(n)))
        } else {
            want.iter().all(|p| broke.contains(p))
        };
        ok &= pass;
        println!(
            "{} {m:?}: {} inputs in {secs:.1} s; broke {broke:?}; expected {want:?}: {}",
            profile.name,
            f.inputs,
            if pass { "OK" } else { "MISMATCH" }
        );
        let mut text = String::new();
        for (p, (n, first)) in &f.by_property {
            text.push_str(&format!("{p}: {n} inputs\n"));
            if let Some((input, viol)) = first {
                text.push_str(&format!("  {}\n  {input:?}\n", viol.detail));
                if m == Mutation::None || want.contains(p) {
                    println!("  {p} ({n}): {}\n    {input:?}", viol.detail);
                }
            }
        }
        if m == Mutation::None && examples {
            for (name, _) in EXAMPLES {
                match f.examples.get(name) {
                    Some((n, first)) => {
                        text.push_str(&format!("example '{name}': {n}\n  {first:?}\n"));
                        println!("  example reached ({n}): {name}");
                    }
                    None => println!("  example NOT reached: {name}"),
                }
            }
        }
        std::fs::write(format!("results/{}-{m:?}.txt", profile.name), text).ok();
    }
    ok &= witnesses();
    ok &= layer2(threads);
    if !ok {
        std::process::exit(1);
    }
}

/// Layer 2: every scenario under every mutation, exhaustively (depth-first, Stateright). The unmutated members break
/// no `always` property and reach every `sometimes` one in every scenario; each control breaks its named property in
/// its named scenario.
fn layer2(threads: usize) -> bool {
    use members::Mutation as M;
    let mut ok = true;
    for m in [
        M::None,
        M::UnboundPackageId,
        M::AcceptSecondPackage,
        M::NoRetryDedupe,
        M::M2AfterFinalOnly,
        M::VoteBeforeMerge,
        M::MBelowIntents,
        M::NoOriginalDedupe,
        M::M2WaitsForPredecessor,
        M::IncludeWithoutQuorum,
        M::FetchFromBOnly,
        M::HoldAllChains,
        M::NoFetchHashCheck,
    ] {
        for sc in members::scenarios() {
            let started = Instant::now();
            let model = Members { scenario: sc.clone(), m };
            let props: Vec<(&'static str, bool)> = model
                .properties()
                .iter()
                .map(|p| (p.name, matches!(p.expectation, stateright::Expectation::Always)))
                .collect();
            let ch = model.checker().threads(threads).spawn_dfs().join();
            let broke: Vec<&str> =
                props.iter().filter(|(n, always)| *always && ch.discovery(n).is_some()).map(|(n, _)| *n).collect();
            let missed: Vec<&str> =
                props.iter().filter(|(n, always)| !*always && ch.discovery(n).is_none()).map(|(n, _)| *n).collect();
            let pass = match members::expected(m) {
                None => broke.is_empty() && missed.is_empty(),
                Some((scenario, prop)) if scenario == sc.name => broke.contains(&prop),
                Some(_) => true,
            };
            let relevant = m == M::None || members::expected(m).is_some_and(|(s, _)| s == sc.name);
            if relevant {
                ok &= pass;
                println!(
                    "layer2 {m:?} '{}': {} states in {:.1} s; broke {broke:?}; missed {missed:?}: {}",
                    sc.name,
                    ch.unique_state_count(),
                    started.elapsed().as_secs_f64(),
                    if pass { "OK" } else { "MISMATCH" }
                );
                for n in &broke {
                    if let Some(path) = ch.discovery(n) {
                        println!("  {n}: {:?}", path.into_actions());
                    }
                }
            }
        }
    }
    ok
}

/// Layer 1b: the witness rule, at `n = 3` (sound and complete) and at `n = 4` with a Byzantine signer (the residual
/// is reached and is the only gap); each control breaks soundness at `n = 3`.
fn witnesses() -> bool {
    use merge_model::witness::{self, Mutation as W};
    let mut ok = true;
    for m in [W::None, W::WitnessByAuthor, W::NoBackdateBound, W::NoHeadsCheck] {
        for byzantine in [false, true] {
            if m != W::None && byzantine {
                continue;
            }
            let (broke, cases, residual) = witness::run(byzantine, m);
            let pass = match witness::expected(m) {
                None => broke.is_empty() && residual == byzantine,
                Some(p) => broke.contains(p),
            };
            ok &= pass;
            println!(
                "witness {m:?} n={}: {cases} cases; broke {broke:?}; residual reached {residual}: {}",
                if byzantine { 4 } else { 3 },
                if pass { "OK" } else { "MISMATCH" }
            );
        }
    }
    ok
}
