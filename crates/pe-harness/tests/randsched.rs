//! randsched over the mock: a fixed seed budget finds nothing in the correct mock, finds the mock's planted bug,
//! and the shrinker reduces that failure to the one item that causes it (DESIGN.md 7, 6).

use pe_harness::props::Prop;
use pe_harness::randsched::{Garbage, Item, MockTarget, broken, evaluate, search, shrink};

const SEEDS: u64 = 120;

#[test]
fn the_correct_mock_passes_the_seed_budget() {
    let found = search(&MockTarget { accept_forged_name: false }, 0..SEEDS);
    assert!(found.is_none(), "{found:#?}");
}

#[test]
fn the_planted_bug_is_found_and_shrunk_to_its_cause() {
    let t = MockTarget { accept_forged_name: true };
    let (s, vs) = search(&t, 0..SEEDS).expect("the budget finds the planted bug");
    let first = broken(&vs);
    assert!(first.contains(&Prop::P6), "an accepted forgery is an unrefused invalid input: {first:?}");
    let small = shrink(&t, s.clone());
    assert!(small.items.len() <= s.items.len() && small.end <= s.end);
    assert!(
        small.items.iter().any(|i| matches!(i, Item::Garbage { kind: Garbage::ForgedName { .. }, .. })),
        "the forgery survives shrinking: {small:#?}"
    );
    let left = broken(&evaluate(&t, &small));
    assert!(first.iter().all(|p| left.contains(p)), "shrinking kept every broken property: {first:?} -> {left:?}");
    // Nothing removable is left: dropping any one item loses a broken property.
    for i in 0..small.items.len() {
        let mut c = small.clone();
        c.items.remove(i);
        let got = broken(&evaluate(&t, &c));
        assert!(!first.iter().all(|p| got.contains(p)), "item {i} was removable: {:#?}", small.items[i]);
    }
}

#[test]
fn garbage_lost_to_a_partition_is_not_an_unrefused_input() {
    // randsched's first finding, seed 97: node 1 is cut off from 3 when 3's garbage would reach it. P6 counts an
    // invalid input from its delivery, not its sending. (Since the Action lane became reliable, this garbage now
    // arrives after the partition heals and is refused; a message that never arrives is never counted.)
    use pe_harness::randsched::generate;
    let t = MockTarget { accept_forged_name: false };
    assert!(evaluate(&t, &generate(97)).is_empty());
}
