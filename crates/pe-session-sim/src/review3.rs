//! Round-3 review repros (design-reviewer's `repro3`), ported to the suite.
//!
//! * `badvote`:    9 signs a second version of an already-confirmed touch; colluding member 8 forks
//!   its own vote and shows the honest members a copy with a frozen head of (0, 0). Before: the
//!   cut fell to 0 and every honest member panicked (rollback below its confirmed frame). Now:
//!   the forged vote never becomes stable among the remaining members, so the eviction waits:
//!   two colluders of four can stall the session, but cannot rewrite confirmed history.
//! * `liar`:       as `badvote`, but 8 votes only once, with the forged head, and otherwise behaves.
//!   Its vote contradicts its own acks, so it is void; 9 is evicted with a cut that keeps the
//!   confirmed touch, and the session goes on.
//! * `crashstall`: honest member 3 goes silent at t = 3 s (review R3-2).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use pe_session_sim::*;
use pe_session::session::*;

use crate::review2::{link, peers, plan, run_to_end, touch, touch_state, verdict};

/// 9 signs a second version of its touch once member 1 has confirmed it; returns true once done.
pub(crate) fn equivocate_after_conf(sim: &mut Sim, to: &[MemberId]) -> bool {
    if sim.replica(1, 1).sim.conf <= 200 {
        return false;
    }
    let x = sim.replica(9, 1).sim.all_entries().into_iter().find(|e| e.member == 9 && matches!(e.payload, Payload::Touch { .. })).unwrap();
    println!(
        "  t={} ms: 9's touch s{} at frame {} confirmed at m1: {}, at m2: {} (m1 conf {}) -> 9 signs a second version",
        sim.net.now_us / 1000,
        x.seq,
        x.stamp,
        sim.replica(1, 1).sim.is_confirmed(x.id()),
        sim.replica(2, 1).sim.is_confirmed(x.id()),
        sim.replica(1, 1).sim.conf
    );
    let mut x2 = x.clone();
    x2.payload = Payload::Touch { obj: 10, impulse: [6.0, 0.0, 0.0], grab: false };
    x2.carried = None;
    for &m in to {
        sim.net.send(1, 9, m, &Msg::Entry(x2.clone()));
    }
    true
}

pub(crate) fn report(sim: &Sim, honest: &[MemberId], label: &str) {
    for &m in honest {
        let r = &sim.replica(m, 1).sim;
        println!(
            "RESULT {label}: m{m} members {:?} evicted {:?} conf {} frame {} paused frames {} lying votes found {}",
            r.members,
            r.members_evicted(),
            r.conf,
            r.frame,
            r.stats.paused_frames,
            r.stats.lying_votes
        );
    }
}

pub fn badvote() {
    let ids = [1, 2, 8, 9];
    let ps = peers(&ids, |m| if m == 9 { plan(&[(50, touch(10, 6.0))]) } else { BTreeMap::new() });
    let mut net = Net::new(link(30, 10, 0), 7);
    let forged_vote: Rc<Cell<EntryId>> = Rc::default();
    let fv = forged_vote.clone();
    // 8's genuine vote never leaves 8 (nor does any fetch of it); only the forged copy does.
    net.filter = Some(Box::new(move |_t, _w, from, _to, m| match m {
        Msg::Entry(e) if from == 8 && matches!(e.payload, Payload::Vote { .. }) && e.id() != fv.get() => Fate::Drop,
        _ => Fate::Deliver,
    }));
    let mut sim = Sim::new(net, ps, 8_000_000);
    let (mut equivocated, mut forged) = (false, false);
    let panic_at = run_to_end(&mut sim, |sim| {
        if !equivocated {
            equivocated = equivocate_after_conf(sim, &[1, 2, 8]);
        }
        if equivocated && !forged {
            let v = sim.replica(8, 1).sim.all_entries().into_iter().find(|e| e.member == 8 && matches!(e.payload, Payload::Vote { .. }));
            if let Some(v) = v {
                if let Payload::Vote { epoch, target, seq, .. } = v.payload {
                    let mut f = v.clone();
                    f.payload = Payload::Vote { epoch, target, seq, frozen: (0, 0) };
                    forged_vote.set(f.id());
                    for to in [1, 2] {
                        sim.net.send(1, 8, to, &Msg::Entry(f.clone()));
                    }
                    println!("  t={} ms: 8 sends members 1 and 2 a forged vote against {target} with frozen head (0, 0)", sim.net.now_us / 1000);
                    forged = true;
                }
            }
        }
    });
    for m in [1, 2] {
        println!("  9's touches at m{m}: {}", touch_state(&sim, m, 9));
    }
    report(&sim, &[1, 2], "badvote");
    verdict(&sim, &[1, 2], "badvote (9 equivocates on a confirmed touch; 8 forks its vote with a forged head)", panic_at);
}

pub fn liar() {
    let ids = [1, 2, 8, 9];
    let mut ps = peers(&ids, |m| if m == 9 { plan(&[(50, touch(10, 6.0))]) } else { BTreeMap::new() });
    for p in ps.iter_mut().filter(|p| p.id == 8) {
        p.worlds.get_mut(&1).unwrap().sim.lie_in_votes = true;
    }
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), ps, 8_000_000);
    let mut equivocated = false;
    let panic_at = run_to_end(&mut sim, |sim| {
        if !equivocated {
            equivocated = equivocate_after_conf(sim, &[1, 2, 8]);
        }
    });
    for m in [1, 2] {
        println!("  9's touches at m{m}: {}", touch_state(&sim, m, 9));
    }
    report(&sim, &[1, 2], "liar");
    verdict(&sim, &[1, 2], "liar (9 equivocates on a confirmed touch; 8 votes with a forged head)", panic_at);
}

pub fn crashstall() {
    let ids = [1, 2, 3];
    let ps = peers(&ids, |m| if m == 1 { plan(&[(60, touch(10, 6.0)), (400, touch(33, 3.0))]) } else { BTreeMap::new() });
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(Box::new(|t, _w, from, to, _m| if (from == 3 || to == 3) && t >= 3_000_000 { Fate::Drop } else { Fate::Deliver }));
    let mut sim = Sim::new(net, ps, 15_000_000);
    let panic_at = run_to_end(&mut sim, |_| {});
    report(&sim, &[1, 2], "crashstall");
    let r1 = &sim.replica(1, 1).sim;
    println!(
        "RESULT crashstall: member 3 silent from t=3 s; at t=15 s m1 conf {} (frame {}), member 3 still a member: {}",
        r1.conf,
        r1.frame,
        r1.members.contains(&3)
    );
    verdict(&sim, &[1, 2], "crashstall (member 3 silent from t=3 s)", panic_at);
}
