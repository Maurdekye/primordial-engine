//! divergence.md 12, on seams S2 v1.2, against a mock replica (a real nibble trie) and an in-process network.

mod common;

use common::{Lie, Net, key, obj, oid, spread};
use pe_diverge::msg::Msg;
use pe_diverge::{Action, Config, CvState, FaultBundle};
use pe_seams::{
    CutRef, DivergeEvent, Frame, Hash, ImportError, LeafKey, LineId, MemberKey, ObjectId, Replica, Rung, SignedEntry,
    SimRate,
};
use std::time::Duration;

const VICTIM: usize = 2;

fn cv_frame(net: &Net, i: usize) -> u64 {
    net.peers[i].d.verified_cut().unwrap().0.frame.0
}

fn bundles(net: &Net, i: usize) -> Vec<pe_diverge::FaultBundle> {
    net.peers[i]
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::WriteBundle(b) => Some((**b).clone()),
            _ => None,
        })
        .collect()
}

/// Run until `Cv` is set, then damage the saved state at `Cv` with stale caches, so rung 1's replay cannot cure it.
fn damage_cv(net: &mut Net, objects: &[u128]) -> u64 {
    net.run(20);
    let cv = cv_frame(net, VICTIM);
    for n in objects {
        net.peers[VICTIM].r.corrupt(*n, cv, false);
    }
    cv
}

#[test]
fn a_clean_run_never_diverges() {
    let mut net = Net::new(3, 40, &[]);
    net.run(300);
    for p in &net.peers {
        let c = p.d.counters();
        assert_eq!(c.diverged, 0);
        assert!(c.cuts_matched >= 35, "{c:?}");
    }
    assert!(net.converged());
}

#[test]
fn a_transient_fault_is_cured_by_rung_1() {
    let mut net = Net::new(3, 40, &[]);
    net.run(20);
    let at = net.peers[VICTIM].r.now;
    net.peers[VICTIM].r.corrupt(5, at, true);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.diverged, 1);
    assert_eq!(c.cured_by_rung[0], 1, "{c:?}");
    assert_eq!(c.bundles_written, 0, "rung 1 writes no bundle");
    assert!(net.converged());
}

#[test]
fn rung_1_waits_for_a_replay_that_takes_several_ticks() {
    // S2 v1.1: the replica replays within a budget and answers Pending meanwhile; the ladder asks again each tick.
    let mut net = Net::new(3, 40, &[]);
    net.peers[VICTIM].r.pending_frames = 12;
    net.run(20);
    let at = net.peers[VICTIM].r.now;
    net.peers[VICTIM].r.corrupt(5, at, true);
    net.run(60);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.cured_by_rung[0], 1, "{c:?}");
    assert_eq!(c.rung_failures, 0, "Pending is never a failure: {c:?}");
    assert!(net.converged());
}

#[test]
fn a_persistent_fault_is_cured_by_rung_2_fetching_only_what_differs() {
    let mut net = Net::new(3, 40, &[]);
    damage_cv(&mut net, &[5]);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.diverged, 1, "{c:?}");
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    assert_eq!(c.bundles_written, 1);
    assert!(c.repair_bytes <= 16, "fetched {} record bytes for one object", c.repair_bytes);
    let b = bundles(&net, VICTIM).pop().unwrap();
    assert_eq!(b.differing, vec![obj(5)]);
    assert_eq!(b.rung, 2);
    assert!(net.converged());
}

#[test]
fn a_rung_2_bundle_carries_cv_s_whole_state_for_offline_replay() {
    // divergence.md 6: the bundle imports into an empty replica. The state is captured before the import moves Cv
    // past it, so it is the old Cv's: its cut, its root, the world's rate, and a record for every leaf.
    let mut net = Net::new(3, 40, &[]);
    let cv = damage_cv(&mut net, &[5]);
    let (cv_cut, cv_root) = net.peers[VICTIM].d.verified_cut().unwrap();
    net.run(40);
    assert_eq!(net.peers[VICTIM].d.counters().cured_by_rung[1], 1);
    assert_eq!(net.peers[VICTIM].d.counters().evidence_skipped, 0);
    let b = bundles(&net, VICTIM).pop().unwrap();
    assert_eq!(FaultBundle::decode(&b.encode().unwrap()), Ok(b.clone()), "the bundle round-trips with its state");
    let s = CvState::decode(&b.state_at_cv).expect("the state decodes");
    assert_eq!((s.cv.at, s.root, s.rate_hz), (cv_cut, cv_root, 60));
    assert_eq!(s.cv.at.frame.0, cv);
    // One `Cv` in the bundle: the one the repair started from, not the cut its cure moved `Cv` to (Low 1).
    assert_eq!(b.cv, Some((s.cv.at, s.root)));
    let st = &net.peers[0].r.states[&cv];
    let want = 1 + st.tables.len() + st.objs.len(); // the config, the tables, the objects; physics is the image
    assert_eq!(s.records.len(), want);
    assert_eq!(s.records[0].key, LeafKey::Config);
    assert!(s.records.iter().any(|r| r.key == obj(5)));
    assert!(s.records.iter().all(|r| !r.proof.is_empty()), "each record carries its proof");
    assert_eq!(s.physics.as_ref().map(|p| p.frame), Some(Frame(cv)));
    assert!(b.entries.is_empty(), "no entries were given");
}

/// The bundle's entries, decoded, as (stamp, n).
fn bundle_entries(b: &FaultBundle) -> Vec<(u64, u8)> {
    b.entries
        .iter()
        .map(|e| {
            let e = SignedEntry::decode(e).expect("an entry decodes");
            (e.entry.stamp.0, e.entry.author.0[0])
        })
        .collect()
}

#[test]
fn a_bundle_carries_exactly_the_entries_stamped_from_cv_to_the_cut_in_order() {
    // @design-reviewer on 7d4ae387: entries stamped in [Cv, C), in the source's agreed order; none below Cv, none
    // from C up.
    let mut net = Net::new(3, 40, &[]);
    let cv = damage_cv(&mut net, &[5]);
    let src = &mut net.peers[VICTIM].src;
    for (f, n) in [(cv - 1, 1), (cv, 2), (cv, 3), (cv + 5, 4), (cv + 400, 5)] {
        src.entries.entry(f).or_default().push(common::entry(f, n));
    }
    net.run(40);
    let b = bundles(&net, VICTIM).pop().unwrap();
    assert!(b.cut.frame.0 > cv + 5 && b.cut.frame.0 < cv + 400, "{:?}", b.cut);
    assert_eq!(bundle_entries(&b), vec![(cv, 2), (cv, 3), (cv + 5, 4)]);
}

#[test]
fn entries_extend_to_a_newer_cut_without_duplicates() {
    // Every source silent past W_fg: rung-2 repairs are overtaken and restart at newer cuts. The entries extend to
    // each, every frame once.
    let mut net = Net::new(3, 40, &[]);
    let cv = damage_cv(&mut net, &[5]);
    net.peers[0].silent = true;
    net.peers[1].silent = true;
    for f in cv..cv + 500 {
        net.peers[VICTIM].src.entries.insert(f, vec![common::entry(f, (f % 200) as u8)]);
    }
    for i in 0..300 {
        if i == 180 {
            net.peers[0].silent = false;
            net.peers[1].silent = false;
        }
        net.step();
    }
    let p = &net.peers[VICTIM];
    let starts = p.events.iter().filter(|e| matches!(e, DivergeEvent::RungStarted { rung: Rung(2), .. })).count();
    assert!(starts >= 2, "the repair restarted at a newer cut: {starts} rung-2 starts");
    let b = bundles(&net, VICTIM).pop().unwrap();
    let got: Vec<u64> = bundle_entries(&b).iter().map(|e| e.0).collect();
    assert_eq!(got, (cv..b.cut.frame.0).collect::<Vec<_>>(), "every frame from Cv to the cut, once");
}

#[test]
fn an_entry_frame_no_longer_kept_leaves_the_evidence_out_counted_once() {
    // S3 trimmed past Cv: the entries from Cv cannot all be had, so the bundle goes without evidence, and the
    // omission is counted once for this Cv, however often rung 2 starts.
    let mut net = Net::with_config(3, 40, &[], stuck_config());
    let cv = damage_cv(&mut net, &[5]);
    net.peers[VICTIM].src.kept_from = cv + 2;
    net.peers[VICTIM].r.refuse = Some(ImportError::NotKept);
    assert!(run_until(&mut net, 600, |n| n.peers[VICTIM].d.counters().repairs_stuck == 1));
    net.run(300); // probes run
    let c = net.peers[VICTIM].d.counters().clone();
    assert!(rung2_failures(&net, VICTIM) > 3, "{c:?}");
    assert_eq!(c.evidence_skipped, 1, "{c:?}");
    assert!(bundles(&net, VICTIM).iter().all(|b| b.state_at_cv.is_empty() && b.entries.is_empty()));
}

#[test]
fn a_bundle_left_without_evidence_is_counted() {
    // Over `evidence_max_bytes`, the state is left out: the bundle is still written, and the omission counted.
    let cfg = Config { evidence_max_bytes: 16, ..Config::default() };
    let mut net = Net::with_config(3, 40, &[], cfg);
    damage_cv(&mut net, &[5]);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!((c.cured_by_rung[1], c.evidence_skipped), (1, 1), "{c:?}");
    assert!(bundles(&net, VICTIM).pop().unwrap().state_at_cv.is_empty());
}

#[test]
fn rung_2_waits_for_an_import_that_takes_several_ticks() {
    let mut net = Net::new(3, 40, &[]);
    net.peers[VICTIM].r.pending_frames = 10;
    damage_cv(&mut net, &[5]);
    net.run(80);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    assert_eq!(c.source_faults, 0, "waiting on the replica is not a silent source: {c:?}");
    assert_eq!(net.peers[VICTIM].r.imports.len(), 1, "the same import is asked again, not rebuilt");
    assert!(net.converged());
}

#[test]
fn damage_hidden_behind_a_stale_cache_is_found_and_never_comes_back() {
    // Object 33 sleeps until frame 200; its damaged record keeps a stale cached hash until something rehashes it.
    let mut net = Net::new(3, 40, &[(33, 200)]);
    damage_cv(&mut net, &[5, 33]);
    net.run(300);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.diverged, 1, "the sleeper's damage must be repaired with the first cure: {c:?}");
    let d = bundles(&net, VICTIM)[0].differing.clone();
    assert!(d.contains(&obj(5)) && d.contains(&obj(33)), "{d:?}");
    assert!(net.converged());
}

#[test]
fn a_bound_body_is_repaired_with_the_physics_image() {
    // Object 20 is a bound body: its record alone cannot be imported (NeedsPhysicsImage).
    let mut net = Net::new(3, 40, &[]);
    damage_cv(&mut net, &[20]);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    assert!(net.converged());
}

#[test]
fn a_physics_divergence_is_repaired_by_restoring_the_image() {
    let mut net = Net::new(3, 40, &[]);
    net.run(20);
    let cv = cv_frame(&net, VICTIM);
    net.peers[VICTIM].r.corrupt_physics(cv);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert!(c.cured_by_rung[1] == 1 || c.cured_by_rung[0] == 1, "{c:?}");
    assert!(net.converged());
}

#[test]
fn a_table_divergence_is_cured_by_rung_2() {
    // A governance table differs (R-H2): its record is fetched and imported like an object's.
    let mut net = Net::new(3, 40, &[]);
    net.run(20);
    let cv = cv_frame(&net, VICTIM);
    net.peers[VICTIM].r.corrupt_table(1, cv);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert!(c.cured_by_rung[1] == 1 || c.cured_by_rung[0] == 1, "{c:?}");
    assert!(net.converged());
}

#[test]
fn an_object_only_i_have_is_removed_and_one_only_they_have_is_fetched() {
    // Two shapes of difference: an extra object far from the others, and a missing one. Rung 1 replays from Cv,
    // which has neither, so both are made in the saved state at Cv (stale: the root at Cv is unchanged).
    let far = 0xabcd_u128 << 112;
    let mut net = Net::new(3, 40, &[]);
    net.run(20);
    let cv = cv_frame(&net, VICTIM);
    // Damage at Cv so rung 1 fails, then the shape differences from Cv on.
    net.peers[VICTIM].r.corrupt(5, cv, false);
    net.peers[VICTIM].r.spawn_extra(far, cv + 1);
    net.peers[VICTIM].r.remove(spread(7), cv + 1);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    let b = bundles(&net, VICTIM).pop().unwrap();
    assert!(b.differing.contains(&LeafKey::Object(ObjectId::from_u128(far))), "removed: {:?}", b.differing);
    assert!(b.differing.contains(&obj(7)), "fetched: {:?}", b.differing);
    let imp = net.peers[VICTIM].r.imports.last().unwrap();
    assert_eq!(imp.absent, vec![LeafKey::Object(ObjectId::from_u128(far))]);
    assert!(net.converged());
}

#[test]
fn a_structural_difference_under_one_prefix_is_repaired_both_ways() {
    // An object whose id shares 31 nibbles with object 9's: where one trie has a leaf, the other has an inner node with
    // a 31-nibble prefix. The victim has it (removed) in one run, and lacks it (fetched) in the other.
    let twin = spread(9) ^ 1;
    for victim_has_it in [true, false] {
        let mut net = Net::new(3, 40, &[]);
        net.run(20);
        let cv = cv_frame(&net, VICTIM);
        for i in 0..3 {
            if (i == VICTIM) == victim_has_it {
                let now = net.peers[i].r.now;
                net.peers[i].r.spawn_extra(twin, now);
            }
        }
        net.peers[VICTIM].r.corrupt(5, cv, false);
        net.run(40);
        let c = net.peers[VICTIM].d.counters().clone();
        assert_eq!(c.cured_by_rung[1], 1, "victim_has_it {victim_has_it}: {c:?}");
        assert!(net.converged(), "victim_has_it {victim_has_it}");
    }
}

#[test]
fn a_member_whose_history_was_trimmed_imports_the_whole_state() {
    // S3 v1.2: a trim at or above the floor makes finalize answer Damaged. With no root of mine at the cut, rung 2
    // fetches every leaf and the physics image, names nothing absent, and the replica imports from empty.
    let mut net = Net::new(3, 40, &[]);
    net.run(20);
    let now = net.peers[VICTIM].r.now;
    net.peers[VICTIM].r.trim_from(now);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.damaged, 1, "{c:?}");
    assert_eq!(c.diverged, 0, "Damaged is not compared: {c:?}");
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    let imp = net.peers[VICTIM].r.imports.last().unwrap();
    assert!(imp.absent.is_empty(), "nothing is named absent without a root of mine");
    assert_eq!(imp.records.len(), 40 + 2 + 1, "every object of the agreed state, both tables and the config");
    assert!(imp.physics.is_some());
    assert!(net.converged());
}

#[test]
fn a_trimmed_member_imports_a_world_larger_than_any_node_cap() {
    // @design-reviewer, re-review Medium: a cap sized from my own tree (0 nodes after a trim) stopped an honest
    // whole-state fetch at 4,096 nodes. The hash check is the only bound: 5,000 objects (over 5,000 nodes) come whole.
    // Large requests keep the test quick; pacing has its own test.
    let cfg =
        Config { node_request_paths: 1024, record_request_keys: 1024, serve_burst_bytes: 1 << 30, ..Config::default() };
    let mut net = Net::with_config(3, 5000, &[], cfg);
    net.run(20);
    let now = net.peers[VICTIM].r.now;
    net.peers[VICTIM].r.trim_from(now);
    net.run(60);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.damaged, 1, "{c:?}");
    assert_eq!(c.source_faults, 0, "an honest source is never faulted for the size of its tree: {c:?}");
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    assert_eq!(net.peers[VICTIM].r.imports.last().unwrap().records.len(), 5000 + 2 + 1);
    assert!(net.converged());
}

#[test]
fn a_repair_paces_its_requests_and_no_honest_source_refuses_it() {
    // Found by the test above (and @shader-reviewer's re-review, Medium A): requests sent as fast as replies came
    // drained the source's serving bucket, its refusals read as NotHeld, and every honest source was faulted. Here a
    // whole-state fetch, one key or path per request, is many times the source's byte budget; the requester paces
    // itself to its copy of the bucket, so nobody refuses and nobody is faulted. T_keep is long so that the root
    // stays servable for the whole (slow) fetch.
    let cfg = Config {
        node_request_paths: 1,
        record_request_keys: 1,
        serve_burst_bytes: 2_000,
        serve_rate: 20_000,
        t_keep: Duration::from_secs(60),
        w_fg: Duration::from_secs(60),
        ..Config::default()
    };
    let mut net = Net::with_config(3, 40, &[], cfg);
    net.run(20);
    let now = net.peers[VICTIM].r.now;
    net.peers[VICTIM].r.trim_from(now);
    net.run(400);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.source_faults, 0, "{c:?}");
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    // The fetch was paced: it took longer than the burst alone would allow.
    assert!(c.repair_latency_nanos[0] > 100_000_000, "{c:?}");
    for i in (0..3).filter(|i| *i != VICTIM) {
        assert_eq!(net.peers[i].d.counters().serve_refused, 0, "source {i} refused a paced request");
    }
    assert!(net.converged());
}

#[test]
fn a_source_answering_busy_forever_is_rotated_and_the_next_one_cures() {
    // Short waits: `n_busy_max` Busy answers come well inside `W_fg`, and the source is left as busy, not faulty.
    let mut net = Net::new(3, 40, &[]);
    net.peers[0].lie = Lie::Busy(50_000);
    damage_cv(&mut net, &[5]);
    net.run(400);
    let c = net.peers[VICTIM].d.counters().clone();
    assert!(c.busy_rotations >= 1, "{c:?}");
    assert_eq!(c.source_faults, 0, "a busy source is not a faulty one: {c:?}");
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    assert!(net.converged());
}

#[test]
fn a_source_naming_long_busy_waits_is_left_when_the_repair_is_overtaken() {
    // Waits capped at `busy_wait_max` (1 s) but named at 30 s: `n_busy_max` of them outlast `W_fg`, so the repair
    // is overtaken first, and the next one starts at the next source (4.2). It still cures.
    let mut net = Net::new(3, 40, &[]);
    net.peers[0].lie = Lie::Busy(30_000_000);
    damage_cv(&mut net, &[5]);
    net.run(400);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    assert!(net.converged());
}

#[test]
fn a_whole_state_fetch_past_sixteen_requests_cures_with_the_default_config() {
    // @shader-reviewer, re-review Medium A: past 16 x 64 objects a whole-state fetch needs more than 16 requests of a
    // kind, which the request-counting bucket refused. The default byte budget serves it whole.
    let mut net = Net::new(3, 1200, &[]);
    net.run(20);
    let now = net.peers[VICTIM].r.now;
    net.peers[VICTIM].r.trim_from(now);
    net.run(60);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.source_faults, 0, "{c:?}");
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    // ... and inside T_keep, so its root is still served at the end (@shader-reviewer).
    assert!(c.repair_latency_nanos[0] < Config::default().t_keep.as_nanos() as u64, "{c:?}");
    for i in (0..3).filter(|i| *i != VICTIM) {
        assert_eq!(net.peers[i].d.counters().serve_refused, 0, "source {i} refused an honest request");
    }
    assert!(net.converged());
}

#[test]
fn a_lying_source_is_refused_and_the_next_one_cures() {
    let mut net = Net::new(3, 40, &[]);
    net.peers[0].lie = Lie::Records;
    damage_cv(&mut net, &[5]);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert!(c.source_faults >= 1, "{c:?}");
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    assert!(net.imported_only_truth(VICTIM, 1), "a record that failed its proof was imported");
    assert!(net.converged());
}

#[test]
fn a_source_lying_in_its_nodes_is_caught_at_its_first_node() {
    let mut net = Net::new(3, 40, &[]);
    net.peers[0].lie = Lie::Nodes;
    damage_cv(&mut net, &[5]);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert!(c.source_faults >= 1, "{c:?}");
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    assert_eq!(bundles(&net, VICTIM).pop().unwrap().differing, vec![obj(5)], "only what differs was fetched");
    assert!(net.converged());
}

#[test]
fn a_silent_source_times_out_and_the_next_one_cures() {
    let mut net = Net::new(3, 40, &[]);
    net.peers[0].silent = true;
    damage_cv(&mut net, &[5]);
    net.run(60);
    let c = net.peers[VICTIM].d.counters().clone();
    assert!(c.source_faults >= 1, "{c:?}");
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    assert!(net.converged());
}

#[test]
fn a_computational_fault_is_kept_in_step_and_reported() {
    let mut net = Net::new(3, 40, &[]);
    net.peers[VICTIM].r.faulty = Some(5);
    net.run(200);
    let p = &net.peers[VICTIM];
    let c = p.d.counters();
    assert!(c.cured_by_rung[1] >= 3, "{c:?}");
    assert!(c.repeated_divergence >= 1, "{c:?}");
    assert!(p.events.iter().any(|e| matches!(e, DivergeEvent::RepeatedDivergence { .. })));
    assert_eq!(net.peers[0].d.counters().diverged, 0, "the honest members never diverge");
    // Control for 4.3's stop: every cure holds for a cut, so nothing failed again and again.
    assert_eq!(c.repairs_stuck, 0, "{c:?}");
}

#[test]
fn many_differing_leaves_are_fetched_in_chunks() {
    // 100 objects differ: more than one request's worth of record keys (64 by default).
    let mut net = Net::new(3, 140, &[]);
    damage_cv(&mut net, &(1..=100).collect::<Vec<_>>());
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    // Bound bodies are among them, so the physics image comes too.
    let differing = bundles(&net, VICTIM).pop().unwrap().differing;
    assert_eq!(differing.iter().filter(|k| matches!(k, LeafKey::Object(_))).count(), 100);
    let requests: Vec<usize> = net.peers[VICTIM]
        .sent
        .iter()
        .filter_map(|m| match m {
            Msg::RecordRequest { keys, .. } => Some(keys.len()),
            _ => None,
        })
        .collect();
    assert_eq!(requests, vec![64, 36], "{requests:?}");
    assert!(net.converged());
}

#[test]
fn a_large_answer_is_split_into_several_replies() {
    let cfg = Config { reply_budget: 200, ..Config::default() };
    let mut net = Net::with_config(3, 40, &[], cfg);
    damage_cv(&mut net, &[1, 2, 3, 4, 5, 6]);
    net.run(40);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.cured_by_rung[1], 1, "{c:?}");
    let replies = net
        .peers
        .iter()
        .flat_map(|p| p.sent.iter())
        .filter(|m| matches!(m, Msg::RecordReply { records, .. } if !records.is_empty()))
        .count();
    assert!(replies >= 2, "six records within 200 bytes per reply came in {replies} replies");
    assert!(net.converged());
}

#[test]
fn a_flood_of_requests_is_refused_past_the_budget() {
    let cfg = Config { serve_burst_bytes: 1_000, serve_rate: 1_000, ..Config::default() };
    let mut net = Net::with_config(3, 40, &[], cfg);
    net.run(20);
    let (cut, root) = {
        let p = &net.peers[0];
        let (cut, root) = p.d.verified_cut().unwrap();
        (cut, root)
    };
    let now = net.now();
    let flooder = key(1);
    let p = &mut net.peers[0];
    let ask = |m: Msg| m.encode().unwrap().to_vec();
    // A second physics image of one root is refused at once.
    for _ in 0..3 {
        let _ =
            p.d.receive(flooder, &ask(Msg::PhysicsRequest { cut, root }), &mut p.r, &mut p.src, now, &common::Clock);
    }
    let images =
        p.d.tick(now, &common::Clock, &mut p.r, &mut p.src)
            .iter()
            .filter(|o| matches!(Msg::decode(&o.bytes), Ok(Msg::PhysicsReply { .. })))
            .count();
    assert_eq!(images, 1);
    // Records, from another peer (each peer has its own bucket): served while its 1,000 bytes last, then `Busy` with
    // the wait, never `NotHeld`.
    let flooder = key(7);
    let keys = vec![obj(1)];
    let refused = p.d.counters().serve_refused;
    for _ in 0..40 {
        let m = Msg::RecordRequest { cut, root, keys: keys.clone() };
        let _ = p.d.receive(flooder, &ask(m), &mut p.r, &mut p.src, now, &common::Clock);
    }
    let out: Vec<Msg> =
        p.d.tick(now, &common::Clock, &mut p.r, &mut p.src).iter().map(|o| Msg::decode(&o.bytes).unwrap()).collect();
    let served = out.iter().filter(|m| matches!(m, Msg::RecordReply { .. })).count();
    let busy = out.iter().filter(|m| matches!(m, Msg::Busy { retry_after_us, .. } if *retry_after_us > 0)).count();
    assert!((1..40).contains(&served), "{served}");
    assert_eq!(served + busy, 40);
    assert_eq!(p.d.counters().serve_refused - refused, busy as u64);
    // An unknown root is never served.
    let other = CutRef { line: LineId::origin(), height: 99, frame: Frame(1) };
    let m = Msg::NodeRequest { cut: other, root: pe_seams::Hash([9; 32]), paths: vec![pe_seams::TreePath::World] };
    let _ = p.d.receive(key(2), &ask(m), &mut p.r, &mut p.src, now, &common::Clock);
    let out = p.d.tick(now, &common::Clock, &mut p.r, &mut p.src);
    assert!(matches!(Msg::decode(&out[0].bytes), Ok(Msg::NotHeld { .. })));
}

#[test]
fn a_late_failed_verdict_never_makes_the_correct_member_import_the_faulty_state() {
    // L4' (@design-reviewer's n = 2 race; @coordinator-opus): B has a lasting fault its self-test
    // catches, and its failed verdict reaches A three cuts late. A must wait for it rather than pick on B's missing
    // verdict, never import B's state, and B repairs toward A.
    let mut net = Net::new(2, 40, &[]);
    net.run(20);
    net.peers[1].r.faulty = Some(5);
    net.peers[1].self_test_fails = true;
    net.peers[1].verdict_delay = 24;
    for _ in 0..200 {
        net.step();
        if let Some(r) = net.peers[0].d.split_reference() {
            assert_eq!(r, key(0), "A chose the member whose self-test failed");
        }
    }
    assert!(net.peers[0].r.imports.is_empty(), "the correct member imported the faulty member's state");
    let c = net.peers[1].d.counters().clone();
    assert!(c.root_splits >= 1 && c.cured_by_rung[1] >= 1, "{c:?}");
    // A root split's bundles carry no replay evidence (divergence.md 6; @design-reviewer, Medium on 7d4ae387).
    assert!(bundles(&net, 1).iter().all(|b| b.state_at_cv.is_empty() && b.entries.is_empty()));
}

#[test]
fn members_that_see_the_split_in_different_views_name_one_reference() {
    // L4' (@coordinator-opus): a lasting fault neither self-test catches, and A sees the first RootSplit
    // one emission after B. Both must name the same reference, and neither may change it within the episode.
    let mut net = Net::new(2, 40, &[]);
    net.run(20);
    net.peers[1].r.faulty = Some(5);
    net.peers[0].skip_splits = 1;
    let mut chosen: std::collections::BTreeMap<(usize, Frame), MemberKey> = Default::default();
    let mut both = 0;
    for _ in 0..200 {
        net.step();
        let Some(cut) = net.split_cut().map(|c| c.at.frame) else { continue };
        let refs: Vec<Option<MemberKey>> = net.peers.iter().map(|p| p.d.split_reference()).collect();
        for (i, r) in refs.iter().enumerate() {
            if let Some(r) = r {
                assert_eq!(*chosen.entry((i, cut)).or_insert(*r), *r, "peer {i} changed its reference within a split");
            }
        }
        if let [Some(a), Some(b)] = refs[..] {
            assert_eq!(a, b, "the two members named different references");
            both += 1;
        }
    }
    assert!(both > 0, "no split reached a pick on both members");
}

#[test]
fn two_members_with_no_quorum_root_agree_on_a_sticky_reference() {
    let mut net = Net::new(2, 40, &[]);
    net.run(20);
    let cv = cv_frame(&net, 1);
    net.peers[1].r.corrupt(5, cv, false);
    let mut splits = 0;
    for _ in 0..80 {
        net.step();
        // While the split lasts its cut is not final: no replica's Cv may reach it (S2 v1: only finalize moves Cv).
        if let Some(cut) = net.split_cut() {
            splits += 1;
            for p in &net.peers {
                assert!(p.r.cv.is_none_or(|f| f < cut.at.frame.0), "Cv moved to an unfinal split cut");
            }
        }
    }
    assert!(splits > 0);
    for p in &net.peers {
        assert!(p.events.iter().any(|e| matches!(e, DivergeEvent::RootSplitStarted { .. })));
        assert!(p.events.iter().any(|e| matches!(e, DivergeEvent::RootSplitEnded { .. })));
        assert!(p.actions.iter().any(|a| matches!(a, Action::RunSelfTest { .. })));
    }
    assert_eq!(net.peers[0].d.split_reference(), None, "the episode ended");
    // Exactly one of the two repaired toward the other, once, and neither moved Cv to the split cut while it lasted.
    let cured: u64 = net.peers.iter().map(|p| p.d.counters().cured_by_rung[1]).sum();
    assert_eq!(cured, 1);
    assert!(net.converged());
}

#[test]
fn three_members_with_three_roots_choose_one_reference() {
    // n = 3, q = 2: three different roots split the cut. Every member picks the same reference, from what all hold
    // identically (the split cut and the standings), never its own Cv.
    let mut net = Net::new(3, 40, &[]);
    net.run(20);
    for (i, n) in [(1, 5), (2, 6)] {
        let cv = cv_frame(&net, i);
        net.peers[i].r.corrupt(n, cv, false);
    }
    let mut refs = Vec::new();
    for _ in 0..80 {
        net.step();
        let r: Vec<_> = net.peers.iter().map(|p| p.d.split_reference()).collect();
        if r.iter().all(|x| x.is_some()) {
            refs.push(r);
        }
    }
    assert!(!refs.is_empty(), "the split persisted past step 1");
    for r in &refs {
        assert!(r.iter().all(|x| *x == r[0]), "references differ: {r:?}");
    }
    assert!(net.converged());
}

#[test]
fn a_repair_overtaken_past_w_fg_restarts_at_the_newest_cut_and_cv_never_goes_back() {
    // Cut A diverges and its replay runs long (Pending); B arrives within W_fg and waits; C arrives past W_fg and
    // abandons A, dropping B with it (@shader-reviewer, M2). Cv never goes back, in pe-diverge or in the replica.
    let mut net = Net::new(3, 40, &[]);
    net.peers[VICTIM].r.pending_frames = 130;
    damage_cv(&mut net, &[5]);
    let (mut last, mut last_r) = (net.peers[VICTIM].d.verified_cut().map(|c| c.0), net.peers[VICTIM].r.cv);
    let mut in_a_row = 0;
    for i in 0..400 {
        if i == 300 {
            net.peers[VICTIM].r.pending_frames = 0;
        }
        net.step();
        let p = &net.peers[VICTIM];
        let v = p.d.verified_cut().map(|c| c.0);
        assert!(v >= last && p.r.cv >= last_r, "Cv moved backwards");
        (last, last_r) = (v, p.r.cv);
        in_a_row = in_a_row.max(p.d.counters().overtaken_in_a_row);
    }
    let p = &net.peers[VICTIM];
    let overtaken = p
        .events
        .iter()
        .filter(|e| matches!(e, DivergeEvent::RungFailed { why, .. } if why.contains("overtaken")))
        .count();
    assert!(overtaken >= 2, "{overtaken}");
    assert!(in_a_row >= 2, "overtakes counted in a row: {in_a_row}");
    assert_eq!(p.d.counters().overtaken_in_a_row, 0, "the cure cleared the gauge");
    assert_eq!(p.d.counters().cured_by_rung[1], 1, "{:?}", p.d.counters());
    assert!(net.converged());
}

#[test]
fn arbitrary_bytes_on_the_repair_lane_never_panic() {
    let mut net = Net::new(3, 40, &[]);
    damage_cv(&mut net, &[5]);
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    for _ in 0..3000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let len = (x % 300) as usize;
        let mut b: Vec<u8> = (0..len).map(|i| (x.rotate_left(i as u32) & 0xff) as u8).collect();
        if let Some(v) = b.first_mut() {
            *v = 1; // the current version, so decoding goes deeper
        }
        let from = key((x % 3) as usize);
        let now = net.now();
        let p = &mut net.peers[VICTIM];
        let _ = p.d.receive(from, &b, &mut p.r, &mut p.src, now, &common::Clock);
        if x.is_multiple_of(50) {
            net.step();
        }
    }
    net.run(60);
    assert!(net.converged());
}

#[test]
fn no_final_frame_is_undone_and_play_never_pauses() {
    // Across a persistent fault, every source silent past W_fg (so repairs are overtaken and restart at newer cuts),
    // and the cure: every peer advances every frame, and Cv only moves forward, in pe-diverge and in the replica.
    let mut net = Net::new(3, 40, &[]);
    damage_cv(&mut net, &[5]);
    net.peers[0].silent = true;
    net.peers[1].silent = true;
    let mut last = [None; 3];
    let mut last_r = [None; 3];
    for i in 0..300 {
        if i == 180 {
            net.peers[0].silent = false;
            net.peers[1].silent = false;
        }
        let before: Vec<u64> = net.peers.iter().map(|p| p.r.now).collect();
        net.step();
        for (i, p) in net.peers.iter().enumerate() {
            assert_eq!(p.r.now, before[i] + 1, "a frame was held up");
            let v = p.d.verified_cut().map(|c| c.0);
            assert!(v >= last[i], "Cv moved backwards");
            assert!(p.r.cv >= last_r[i], "the replica's Cv moved backwards");
            last[i] = v;
            last_r[i] = p.r.cv;
        }
    }
    let c = net.peers[VICTIM].d.counters();
    assert!(c.rung_failures >= 2, "the repairs were overtaken: {c:?}");
    assert_eq!(c.repairs_stuck, 0, "an outage is not a repair refused again and again (4.3): {c:?}");
    assert!(net.converged());
    assert_eq!(net.peers[VICTIM].r.frame(), net.peers[0].r.frame());
    let _ = oid(1);
}

// ---- 4.3: a repair that keeps failing stops the ladder (coordinator) ---------------

fn stuck_config() -> Config {
    Config { t_stuck_probe: Duration::from_secs(2), ..Config::default() }
}

/// The stuck bundles the peer wrote, by their `stuck` lists.
fn stuck_bundles(net: &Net, i: usize) -> Vec<Vec<String>> {
    net.peers[i]
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::WriteBundle(b) if !b.stuck.is_empty() => Some(b.stuck.clone()),
            _ => None,
        })
        .collect()
}

/// Rung-2 failures the peer reported.
fn rung2_failures(net: &Net, i: usize) -> usize {
    net.peers[i].events.iter().filter(|e| matches!(e, DivergeEvent::RungFailed { rung: Rung(2), .. })).count()
}

fn run_until(net: &mut Net, frames: u64, done: impl Fn(&Net) -> bool) -> bool {
    for _ in 0..frames {
        net.step();
        if done(net) {
            return true;
        }
    }
    false
}

#[test]
fn a_repair_refused_the_same_way_stops_the_ladder_and_names_the_mismatch() {
    let mut net = Net::with_config(3, 40, &[], stuck_config());
    damage_cv(&mut net, &[5]);
    net.peers[VICTIM].r.refuse = Some(ImportError::RootMismatch { got: Hash::ZERO });
    assert!(run_until(&mut net, 400, |n| n.peers[VICTIM].d.counters().repairs_stuck == 1));
    assert_eq!(rung2_failures(&net, VICTIM), 3, "stopped after n_stuck failures with one key");
    let b = stuck_bundles(&net, VICTIM);
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!(b[0].len(), 1, "one key: {b:?}");
    assert!(b[0][0].starts_with("rung 2: the patch stayed incomplete; differing: "), "{b:?}");
    assert!(
        b[0][0].contains("differing: Config (the world's config differs), Object("),
        "a whole refetch names the config leaf first: {b:?}"
    );
    // Stopped: cuts are compared, nothing is fetched, until the probe (2 s = 120 frames).
    let imports = net.peers[VICTIM].r.imports.len();
    net.run(60);
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(net.peers[VICTIM].r.imports.len(), imports, "a stopped ladder fetched again before its probe");
    assert!(c.stuck_skipped >= 3, "{c:?}");
    // Probes run, fail the same way, and write nothing new.
    net.run(400);
    let c = net.peers[VICTIM].d.counters().clone();
    assert!(net.peers[VICTIM].r.imports.len() > imports, "no probe ran");
    assert_eq!(c.repairs_stuck, 1, "{c:?}");
    assert_eq!(stuck_bundles(&net, VICTIM).len(), 1);
    // One RepairStuck event (S3.8 v1.4), with the typed differing leaves.
    let stuck: Vec<&DivergeEvent> =
        net.peers[VICTIM].events.iter().filter(|e| matches!(e, DivergeEvent::RepairStuck { .. })).collect();
    assert_eq!(stuck.len(), 1, "{stuck:?}");
    let DivergeEvent::RepairStuck { rung, differing, why, .. } = stuck[0] else { unreachable!() };
    assert_eq!(*rung, Rung(2));
    assert!(differing.contains(&LeafKey::Config) && differing.contains(&LeafKey::Object(oid(5))), "{differing:?}");
    assert!(why.starts_with("rung 2: the patch stayed incomplete"), "{why}");
}

#[test]
fn a_joiner_at_another_rate_is_stopped_and_the_rates_are_named() {
    // S2 v1.4 RateMismatch (@coordinator-opus item 3): import refuses a world at another rate. No retry
    // or other source can cure it, so the ladder stops on the first refusal (@design-reviewer), names
    // both rates and the config leaf, and never adopts the world's rate.
    let mut net = Net::with_config(3, 40, &[], stuck_config());
    damage_cv(&mut net, &[5]);
    net.peers[VICTIM].r.refuse =
        Some(ImportError::RateMismatch { world: SimRate::new(30).unwrap(), mine: SimRate::new(60).unwrap() });
    assert!(run_until(&mut net, 400, |n| n.peers[VICTIM].d.counters().repairs_stuck == 1));
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(rung2_failures(&net, VICTIM), 1, "stopped on the first refusal: {c:?}");
    assert_eq!((c.source_faults, c.busy_rotations), (0, 0), "no source was spent on it: {c:?}");
    let b = stuck_bundles(&net, VICTIM);
    assert_eq!(b.len(), 1, "{b:?}");
    assert!(
        b[0][0]
            .starts_with("rung 2: the world runs at 30 Hz and this replica at 60 Hz; differing: Config (the world's"),
        "{b:?}"
    );
    let stuck = net.peers[VICTIM].events.iter().filter(|e| matches!(e, DivergeEvent::RepairStuck { .. })).count();
    assert_eq!(stuck, 1);
    // In a live world the differing leaves drift from probe to probe. The key is `Config` alone, and only a cure
    // ends this stop, so later probes stay quiet (@shader-reviewer, Low on 30a26079).
    let cv = cv_frame(&net, VICTIM);
    net.peers[VICTIM].r.corrupt(7, cv, false);
    let before = rung2_failures(&net, VICTIM);
    net.run(150); // 2.5 s of 60 Hz frames, past at least one 2 s probe
    net.peers[VICTIM].r.corrupt(9, cv, false);
    net.run(150);
    let c = net.peers[VICTIM].d.counters().clone();
    assert!(rung2_failures(&net, VICTIM) > before, "the probes ran: {c:?}");
    assert_eq!(c.repairs_stuck, 1, "{c:?}");
    assert_eq!(stuck_bundles(&net, VICTIM).len(), 1);
}

#[test]
fn a_changed_failure_restarts_the_ladder_and_a_cure_clears_the_stop() {
    let mut net = Net::with_config(3, 40, &[], stuck_config());
    damage_cv(&mut net, &[5]);
    net.peers[VICTIM].r.refuse = Some(ImportError::RootMismatch { got: Hash::ZERO });
    assert!(run_until(&mut net, 400, |n| n.peers[VICTIM].d.counters().repairs_stuck == 1));
    // The next probe fails another way: the key changed, so the ladder runs at every cut again and stops anew.
    net.peers[VICTIM].r.refuse = Some(ImportError::NotKept);
    let before = rung2_failures(&net, VICTIM);
    assert!(run_until(&mut net, 600, |n| n.peers[VICTIM].d.counters().repairs_stuck == 2));
    assert_eq!(rung2_failures(&net, VICTIM) - before, 3, "the new key counted from one");
    let b = stuck_bundles(&net, VICTIM);
    assert!(b[1][0].starts_with("rung 2: the cut's state is no longer kept; differing: Object("), "{b:?}");
    // The replica takes the state again: the next probe cures, and the stop is gone.
    net.peers[VICTIM].r.refuse = None;
    assert!(run_until(&mut net, 600, |n| n.peers[VICTIM].d.counters().cured_by_rung[1] >= 1));
    net.run(80);
    assert!(net.converged());
    let c = net.peers[VICTIM].d.counters().clone();
    assert_eq!(c.repairs_stuck, 2, "{c:?}");
}

#[test]
fn failures_whose_key_keeps_changing_stop_after_n_stuck_any() {
    // Two failures take turns (A, B, A, B...): no key reaches n_stuck, so n_stuck_any (10) stops the ladder, and the
    // bundle names both.
    let mut net = Net::with_config(3, 40, &[], stuck_config());
    damage_cv(&mut net, &[5]);
    let mut done = false;
    for _ in 0..2000 {
        let refuse = if rung2_failures(&net, VICTIM).is_multiple_of(2) {
            ImportError::RootMismatch { got: Hash::ZERO }
        } else {
            ImportError::NotKept
        };
        net.peers[VICTIM].r.refuse = Some(refuse);
        net.step();
        if net.peers[VICTIM].d.counters().repairs_stuck == 1 {
            done = true;
            break;
        }
    }
    assert!(done, "{:?}", net.peers[VICTIM].d.counters());
    assert_eq!(rung2_failures(&net, VICTIM), 10, "stopped after n_stuck_any failures with no cure");
    let b = stuck_bundles(&net, VICTIM);
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!(b[0].len(), 2, "both keys named: {b:?}");
    // Stopped by keys that kept changing: a probe failing either way does not restart it; only a cure would.
    for _ in 0..600 {
        let refuse = if rung2_failures(&net, VICTIM).is_multiple_of(2) {
            ImportError::RootMismatch { got: Hash::ZERO }
        } else {
            ImportError::NotKept
        };
        net.peers[VICTIM].r.refuse = Some(refuse);
        net.step();
    }
    assert_eq!(net.peers[VICTIM].d.counters().repairs_stuck, 1);
}
