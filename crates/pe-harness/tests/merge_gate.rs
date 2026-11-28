//! The merge cost gate (M2 S7 slice 1; src/gate.rs). The smoke tests run a tiny pit in every tier-1 run, so the
//! harness keeps compiling and computing; the trial measures, and runs alone, in release, at normal priority, by the
//! coordinator's clean-run procedure for figures.

use pe_harness::gate::{self, Gate};

#[test]
fn a_tiny_gate_run_re_steps_the_span_chases_a_and_compares_every_ball() {
    let g = Gate {
        seed: 5,
        balls: 12,
        base: 30,
        span: 60,
        every: 100 * 1_000_000,
        live: 12,
        paced: true,
        chase_frames: 600,
    };
    let r = gate::run(&g).expect("a gate run");
    println!("{}", r.line());
    // The package re-steps from the newest keyframe at or below Cb, so at least the span.
    assert!(r.replay_frames >= g.span, "{r:?}");
    assert!(r.converged.is_some(), "a paced A at 12 balls is caught: {r:?}");
    assert_eq!(r.objects, 12);
    assert!(r.a_entries > 0 && r.b_entries > 0, "both members act in the span: {r:?}");
    assert!(r.a_alone.n as u64 == g.live && r.a_during.n as u64 >= g.live, "{r:?}");
    // B's own state lacks A's span, and A's lacks B's: with both acting, the merge changes something on each side.
    assert!(r.corrections.by_hash > 0 && r.overwrites.by_hash > 0, "{r:?}");
    assert!(r.overwrites.visible > 0 && r.overwrites.max_m > gate::VISIBLE_M, "{r:?}");
}

#[test]
fn a_chase_against_a_flat_out_loop_never_converges() {
    // The chase's own control: an unpaced A steps as fast as the merge does, so the merged state never reaches it,
    // and the run says so at its bound instead of hanging.
    let g = Gate {
        seed: 5,
        balls: 12,
        base: 30,
        span: 60,
        every: 100 * 1_000_000,
        live: 12,
        paced: false,
        chase_frames: 240,
    };
    let r = gate::run(&g).expect("a gate run");
    println!("{}", r.line());
    assert!(r.converged.is_none(), "{r:?}");
}

#[test]
fn a_tiny_lagging_voter_measures_two_simulations() {
    let v = gate::lagging_voter(5, 12, 30, 30).expect("a lagging-voter run");
    println!("{}", v.line());
    assert_eq!(v.frames, 30);
    assert!(v.k() > 1.0, "two simulations cost more than one: {v:?}");
}

/// The gate's figures. `PE_GATE_BALLS` (default "200,500"), `PE_GATE_SPANS` (seconds, default "3,10,60,120") and
/// `PE_GATE_SEED` (default 7) set the runs; `PE_GATE_VOTER_SECS` (default 20) sets the lagging-voter run.
#[test]
#[ignore = "a timing trial: run alone, release, normal priority, by the clean-run procedure"]
fn merge_cost_gate() {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let list = |k: &str, d: &str| -> Vec<u64> {
        var(k).unwrap_or_else(|| d.into()).split(',').filter_map(|x| x.trim().parse().ok()).collect()
    };
    let seed = var("PE_GATE_SEED").and_then(|v| v.trim().parse().ok()).unwrap_or(7);
    let voter_s = var("PE_GATE_VOTER_SECS").and_then(|v| v.trim().parse().ok()).unwrap_or(20);
    for balls in list("PE_GATE_BALLS", "200,500") {
        let balls = balls as usize;
        let v = gate::lagging_voter(seed, balls, 120, voter_s * 60).expect("a lagging-voter run");
        println!("{}", v.line());
        for span in list("PE_GATE_SPANS", "3,10,60,120") {
            let r = gate::run(&Gate::new(seed, balls, span)).expect("a gate run");
            println!("{}", r.line());
        }
    }
}
