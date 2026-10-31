//! State transfer to a joiner (design 9.5, reviews P22R-1 and P22S-1, P2-3 S5): the joiner restores a
//! donor's confirmed check-frame state only against a check hash a quorum signed, at least one signer
//! other than the donor, and then simulates in step with the members.

use pe_session::peer::SessionCore;
use pe_session::session::*;

const SCENE: SceneKind = SceneKind::Pile { side: 4, layers: 2 };
const MEMBERS: [MemberId; 3] = [1, 2, 3];

fn sim(me: MemberId) -> WorldSim {
    WorldSim::new(1, me, &MEMBERS, SCENE, SimConfig::default(), KeyRing::test(me))
}

/// Runs the three members back to back, member 1 touching an object every 97 ms.
fn run(cores: &mut [SessionCore], from_ms: u64, to_ms: u64) {
    for ms in from_ms..to_ms {
        let now = ms * 1000;
        for c in cores.iter_mut() {
            c.tick(now);
        }
        if ms % 97 == 0 {
            let obj = (ms / 97 % 16) as u32;
            let _ = cores[0].author(Payload::Touch { obj, impulse: [0.4, 2.0, -0.3], grab: ms % 2 == 0 }, 0);
        }
        let outs: Vec<(MemberId, Vec<_>)> = cores.iter_mut().enumerate().map(|(i, c)| (i as MemberId + 1, c.outbox())).collect();
        for (from, out) in outs {
            for o in out {
                for &to in &o.to {
                    cores[to as usize - 1].receive(from, o.msg.clone(), now);
                }
            }
        }
    }
}

/// Every entry `donor_view` holds, signed, in (author, seq) order; only `authors`' if given.
fn log(donor_view: &WorldSim, authors: Option<&[MemberId]>) -> Vec<SignedEntry> {
    let mut v: Vec<Entry> = donor_view.all_entries();
    v.retain(|e| authors.is_none_or(|a| a.contains(&e.member)));
    v.sort_by_key(|e| (e.member, e.seq));
    v.iter().map(|e| donor_view.signed_entry(e.id()).expect("held entries are signed")).collect()
}

fn members() -> Vec<SessionCore> {
    let mut cores: Vec<SessionCore> = MEMBERS.iter().map(|&m| SessionCore::new(sim(m))).collect();
    for c in &mut cores {
        c.start(0);
    }
    cores
}

#[test]
fn a_joiner_restores_a_quorum_vouched_state_and_keeps_step() {
    let mut cores = members();
    run(&mut cores, 0, 3000);
    let st = cores[0].sim.export_state().expect("a confirmed check frame");
    assert!(st.frame > 60 && st.frame % 30 == 0, "frame {}", st.frame);
    // The joiner (not a member) holds the log, as a relayed catch-up gives it.
    let mut j = sim(4);
    for se in log(&cores[0].sim, None) {
        j.insert(se);
    }
    j.import_state(&st, 1).expect("a vouched transfer restores");
    assert_eq!(j.frame, st.frame + 1);
    assert_eq!(j.stats.imports, 1);
    // It continues with the members: the next entries, then the same frames.
    run(&mut cores, 3000, 5000);
    for se in log(&cores[0].sim, None) {
        j.insert(se);
    }
    let a = &cores[1].sim;
    j.advance(a.frame);
    let (from, to) = (st.frame + 1, a.conf.min(j.frame));
    assert!(to > from + 60, "{from}..{to}");
    for f in from..to {
        assert_eq!(j.hashes.get(&f), a.hashes.get(&f), "state hash at frame {f}");
    }
    let common: Vec<u64> = a.check_hashes.keys().copied().filter(|f| (from..to).contains(f)).collect();
    assert!(common.len() >= 2);
    assert!(common.iter().all(|f| j.check_hashes.get(f) == a.check_hashes.get(f)));
}

#[test]
fn a_joiner_refuses_what_a_quorum_did_not_vouch_for() {
    let mut cores = members();
    run(&mut cores, 0, 3000);
    let st = cores[0].sim.export_state().expect("a confirmed check frame");
    let mut j = sim(4);
    // Only the donor's own acks: the donor alone cannot vouch for its transfer.
    for se in log(&cores[0].sim, Some(&[1])) {
        j.insert(se);
    }
    assert_eq!(j.import_state(&st, 1), Err(TransferError::NoQuorum));
    for se in log(&cores[0].sim, None) {
        j.insert(se);
    }
    let before = (j.frame, j.state_hash());
    let flip = |mut v: Vec<u8>, at: usize| {
        v[at] ^= 0x10;
        v
    };
    let tampered = [
        StateTransfer { physics: flip(st.physics.clone(), st.physics.len() / 2), ..st.clone() },
        StateTransfer { physics: [st.physics.as_slice(), &[0]].concat(), ..st.clone() },
        StateTransfer { store: flip(st.store.clone(), st.store.len() - 3), ..st.clone() },
        StateTransfer { tables: [st.tables.as_slice(), &[0]].concat(), ..st.clone() },
        // Another vouched frame's label on this state.
        StateTransfer { frame: st.frame - 30, ..st.clone() },
        StateTransfer { world: 2, ..st.clone() },
    ];
    for (i, bad) in tampered.iter().enumerate() {
        let r = j.import_state(bad, 1);
        assert!(r.is_err(), "tampered transfer {i} accepted");
        assert_eq!((j.frame, j.state_hash()), before, "refusal {i} changed the replica ({r:?})");
    }
    assert_eq!(j.import_state(&tampered[4], 1), Err(TransferError::Hash));
    assert_eq!(j.import_state(&st, 1), Ok(()));
}
