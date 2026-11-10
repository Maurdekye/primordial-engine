//! P2-3 scenarios on pe-session's own additions.
//!
//! * `statejoin` (design 9.5, reviews P22R-1, P22S-1 and P23-3): a joiner catches up by state transfer.
//!   It asks members 2 and 1 (`request_state`). Member 2 answers first with a tampered state, which the
//!   joiner refuses (Hash); member 2's honest answer then arrives unrequested (its request was used)
//!   and is refused too (Unrequested); member 1's state, vouched for by a quorum's signed acks, is
//!   restored, and from its frame on the joiner's history equals the members'.

use std::collections::{BTreeMap, BTreeSet};

use pe_session_sim::*;
use pe_session::session::*;

use crate::p24r::{confs, peers_of, run_quiet, SCENE};
use crate::review2::{link, plan, touch, verdict};

pub fn statejoin() {
    let ids = [1, 2, 3];
    let joiner: MemberId = 5;
    let mut ps = peers_of(&ids, |m| match m {
        1 => plan(&[(40, touch(10, 6.0)), (150, touch(21, 4.0)), (260, touch(33, 3.0)), (500, touch(44, 5.0))]),
        2 => plan(&[(90, touch(12, 5.0)), (320, touch(27, 2.0))]),
        _ => BTreeMap::new(),
    });
    let mut p = Peer::new(joiner, 0);
    p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
    // The joiner does not simulate before its state arrives (a background world that never bursts):
    // a state never rewinds a replica (review P23-3), so a joiner that had simulated past the state's
    // frame would refuse it.
    p.worlds.get_mut(&1).unwrap().core.throttle_us = Some(u64::MAX / 2);
    ps.push(p);
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 10_000_000);
    let relay = |sim: &mut Sim| {
        let mut log = sim.replica(1, 1).sim.all_entries();
        log.sort_by_key(|e| (e.member, e.seq));
        // Each entry with its authentication: signed on itself, or in its run up to its cover (option 2).
        let mut sent = BTreeSet::new();
        for e in log {
            if sent.contains(&e.id()) {
                continue;
            }
            let m = sim.replica(1, 1).sim.relay(e.id()).expect("held entries are authenticated");
            match &m {
                Msg::Run { entries, .. } => sent.extend(entries.iter().map(|x| x.id())),
                _ => {
                    sent.insert(e.id());
                }
            }
            sim.net.send(1, 1, joiner, &m);
        }
    };
    let mut step = 0;
    let mut sent_bytes = (0, 0, 0);
    let stopped = run_quiet(&mut sim, |sim| {
        let now = sim.net.now_us;
        if step == 0 && now >= 3_000_000 {
            // The joiner asks to join, and member 1 hands it the log (as latejoin).
            for m in ids {
                sim.net.send(1, joiner, m, &join_msg(&TEST_SESSION, joiner, vec![], u64::MAX));
            }
            relay(sim);
            step = 1;
        } else if step == 1 && now >= 3_500_000 {
            // The log again (the acks vouching for the newest confirmed check frame), then the state.
            relay(sim);
            // Member 2 answers with its state, one byte of the physics part changed: a hostile donor.
            let mut bad = sim.replica(2, 1).sim.export_state().expect("a confirmed check frame");
            let at = bad.physics.len() / 3;
            bad.physics[at] ^= 0x01;
            sim.net.send(1, 2, joiner, &Msg::State(Box::new(bad)));
            let r = sim.replica_mut(joiner, 1);
            r.core.request_state(2);
            r.core.request_state(1);
            step = 2;
        } else if step == 2 && sim.replica(joiner, 1).sim.stats.imports > 0 {
            sim.replica_mut(joiner, 1).core.throttle_us = None;
            let st = sim.replica(1, 1).sim.export_state().expect("a confirmed check frame");
            sent_bytes = (st.physics.len(), st.store.len(), st.tables.len());
            step = 3;
        }
    });
    let all = [1, 2, 3, joiner];
    let label = "statejoin (5 joins by state transfer; member 2 first sends a tampered state)";
    let j = &sim.replica(joiner, 1);
    let floor = j.sim.hashes.keys().next().copied().unwrap_or(0);
    let refused = j.sim.stats.transfers_refused;
    let r1 = &sim.replica(1, 1).sim;
    let to = all.iter().map(|&m| sim.replica(m, 1).sim.conf).min().unwrap_or(0);
    let equal = to > floor + 60 && (floor..to).all(|f| all.iter().all(|&m| sim.replica(m, 1).sim.hashes.get(&f) == r1.hashes.get(&f)));
    let checks: Vec<u64> = r1.check_hashes.keys().copied().filter(|f| (floor..to).contains(f)).collect();
    let checks_equal = checks.iter().all(|f| sim.replica(joiner, 1).sim.check_hashes.get(f) == r1.check_hashes.get(f));
    println!(
        "  transfer: imports {} refused {} ({:?}); restored the state after frame {}; member 1's newest state then: physics {} B, store {} B, tables {} B",
        j.sim.stats.imports, refused, j.sim.stats.last_refusal, floor.saturating_sub(1), sent_bytes.0, sent_bytes.1, sent_bytes.2
    );
    println!(
        "RESULT statejoin: joiner restored member 1's state after refusing member 2's tampered one; frames {floor}..{to} at all four {}; {} check frames {}; {}",
        if equal { "EQUAL" } else { "DIFFERENT" },
        checks.len(),
        if checks_equal { "equal" } else { "DIFFERENT" },
        confs(&sim, &all)
    );
    verdict(&sim, &ids, label, stopped);
}
