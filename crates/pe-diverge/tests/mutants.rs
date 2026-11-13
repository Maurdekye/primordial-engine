//! Controls that bite (divergence.md 12): with each planted defect switched on, the property its test checks must
//! fail. Run with `--features mutants`. (ImportUnchecked and PatchNoReplay are pe-replica's: its `import_at_cut`
//! checks proofs and re-simulates.)

#![cfg(feature = "mutants")]

mod common;

use common::{Lie, Net};
use pe_diverge::{Action, Mutant};

const VICTIM: usize = 2;

fn damage_cv(net: &mut Net, ids: &[u128]) {
    net.run(20);
    let cv = net.peers[VICTIM].d.verified_cut().unwrap().0.frame.0;
    for id in ids {
        net.peers[VICTIM].r.corrupt(*id, cv, false);
    }
}

#[test]
fn skipping_verify_leaves_hidden_damage_that_comes_back() {
    // The schedule of `damage_hidden_behind_a_stale_cache_is_found_and_never_comes_back`, where it diverges once.
    let mut net = Net::new(3, 40, &[(33, 200)]);
    net.peers[VICTIM].d.mutant = Some(Mutant::SkipVerify);
    damage_cv(&mut net, &[5, 33]);
    net.run(300);
    let c = net.peers[VICTIM].d.counters();
    assert!(c.diverged >= 2, "the control did not bite: {c:?}");
}

#[test]
fn sticking_with_a_lying_source_never_cures() {
    // The schedule of `a_lying_source_is_refused_and_the_next_one_cures`, where it cures once.
    let mut net = Net::new(3, 40, &[]);
    net.peers[0].lie = Lie::Records;
    net.peers[VICTIM].d.mutant = Some(Mutant::StickWithSource);
    damage_cv(&mut net, &[5]);
    net.run(40);
    let c = net.peers[VICTIM].d.counters();
    assert_eq!(c.cured_by_rung[1], 0, "the control did not bite: {c:?}");
}

#[test]
fn sticking_with_a_silent_source_never_cures() {
    // The schedule of `a_silent_source_times_out_and_the_next_one_cures`, where it cures once.
    let mut net = Net::new(3, 40, &[]);
    net.peers[0].silent = true;
    net.peers[VICTIM].d.mutant = Some(Mutant::StickWithSource);
    damage_cv(&mut net, &[5]);
    net.run(60);
    let c = net.peers[VICTIM].d.counters();
    assert_eq!(c.cured_by_rung[1], 0, "the control did not bite: {c:?}");
}

#[test]
fn unchecked_nodes_let_a_lying_source_steer_the_descent() {
    // The schedule of `a_source_lying_in_its_nodes_is_caught_at_its_first_node`: there the liar is caught at once and
    // only object 5 is fetched; unchecked, the liar is never caught and its false hashes drag in other leaves.
    let mut net = Net::new(3, 40, &[]);
    net.peers[0].lie = Lie::Nodes;
    net.peers[VICTIM].d.mutant = Some(Mutant::UncheckedNodes);
    damage_cv(&mut net, &[5]);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    let differing = net.peers[VICTIM]
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::WriteBundle(b) => Some(b.differing.len()),
            _ => None,
        })
        .next_back();
    assert!(c.source_faults == 0 || differing.is_some_and(|n| n > 1), "the control did not bite: {c:?}");
}
