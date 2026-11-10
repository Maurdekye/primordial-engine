//! Randomized Byzantine schedule generator (coordinator's request, P2-4 round 5). Each seed draws a
//! membership, a faulty set within the fault bound t = floor((n-1)/3), a behaviour for each faulty
//! member, an optional outside joiner with partial delivery, and a GST after which the faulty members'
//! network games stop. It runs the deterministic simulator and checks the protocol invariants (see
//! `docs/design.md` 10.8 and `spikes/out/session-sim/INVARIANTS.md`).
//!
//! The links between honest members are clean throughout, but for VC4R-1's drawn late members (whose
//! round-0 prevotes are held back), so the invariants are sharp: an honest, non-crashed member is never
//! removed or falsely accused (S6), and the live honest members keep confirming after GST. The
//! adversary is the faulty set, whose members may crash (at a time, or right after their first
//! prevote), delay or steer their own messages, claim false references or epochs, withhold view-change
//! votes, sign bogus capped precommits, churn suspicion votes, equivocate a prevote, or (as an outside
//! joiner) have their request reach only part of the session. Four review holes (VC4R-1, VC4R-2,
//! VCR-12, VC3R-1) get a shape drawn a share of the time, each from its own stream (INVARIANTS.md).
//!
//! `pe-spike-session-sim randsched [--seeds N] [--from S] [--seed S] [--fixes-off HEX] [--verbose]`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use pe_session_sim::*;
use pe_session::session::*;

use crate::p24r::{each_sim, peers_of, run_quiet, sim_mut, GUEST, SCENE};
use crate::review2::{link, plan, touch};

fn mix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A uniform integer in `0..n`.
fn below(rng: &mut u64, n: u64) -> u64 {
    mix(rng) % n.max(1)
}

/// What one faulty member does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// Stops sending anything from `at` us (a permanent crash).
    Crash { at: u64 },
    /// Delays everything it sends by `by` us, until GST.
    Slow { by: u64 },
    /// Each of its own entries reaches only one member, rotating by seq, until GST.
    Rotate,
    /// Its acks claim `n` more of member `on`'s chain than anyone holds, from `at` us.
    FakeRefs { on: MemberId, n: u32, at: u64 },
    /// Its acks climb to the next epoch with no decision, from `at` us.
    Climb { at: u64 },
    /// It casts no view-change vote.
    Withhold,
    /// It signs a capped precommit for a value no one proposed, in every epoch from `at` us.
    BogusCap { at: u64 },
    /// It votes to remove honest `target`, whom everyone hears, in every epoch from `at` us.
    Churn { target: MemberId, at: u64 },
    /// It casts a vote naming a kick that does not exist, once at `at` us.
    GhostVote { target: MemberId, at: u64 },
    /// It equivocates its view-change prevote: the real value to a drawn subset of the other members
    /// (`v_pick`'s bits over them), nil to the rest, so a polka is visible to some and not others; and
    /// drawn honest members' round-0 prevotes reach the nil side `late_us` late, by any path (review VC4R-1).
    EquivPrevote { v_pick: u64, late_pick: u64, late_us: u64 },
    /// It crashes once it has cast its first view-change prevote (review VC4R-2, joinwedge's member 9).
    CrashAfterPrevote,
    /// From `at` us its acks climb to the next epoch with no decision, and each of its own entries reaches
    /// one member only, rotating by seq, for the rest of the run (review VC3R-1: round 3 restarted the
    /// newer-epoch timer while another member's acks showed more of some chain than this replica held,
    /// and its own chain is always a step ahead at whichever member got its newest entry).
    ClimbAhead { at: u64 },
}

/// A run's drawn schedule.
struct Schedule {
    ids: Vec<MemberId>,
    faulty: BTreeMap<MemberId, Fault>,
    gst: u64,
    /// An outside joiner, its recipients among the members, and whether it crashes after its prevote.
    joiner: Option<(MemberId, Vec<MemberId>, bool)>,
    /// An honest member that acks only once every 1.5 s, strictly on its grid (review VCR-12).
    slow_acker: Option<MemberId>,
    /// When the outside joiner asks (us).
    join_at: u64,
    end_us: u64,
}

fn draw(seed: u64) -> Schedule {
    let mut rng = seed ^ 0xA5A5_5A5A_1234_ABCD;
    let n = 4 + below(&mut rng, 4) as usize; // 4..=7
    let ids: Vec<MemberId> = (1..=n as u32).collect();
    let t = (n - 1) / 3;
    let f = below(&mut rng, t as u64 + 1) as usize; // 0..=t
    let mut pool = ids.clone();
    let mut faulty = BTreeMap::new();
    for _ in 0..f {
        let i = below(&mut rng, pool.len() as u64) as usize;
        let m = pool.remove(i);
        // An honest member for Churn/GhostVote/FakeRefs to point at.
        let honest_other = *ids.iter().find(|&&h| h != m && !faulty.contains_key(&h)).unwrap_or(&ids[0]);
        let at = 1_500_000 + below(&mut rng, 4_000_000);
        let fault = match below(&mut rng, 11) {
            0 => Fault::Crash { at },
            1 => Fault::Slow { by: 500_000 + below(&mut rng, 2_000_000) },
            2 => Fault::Rotate,
            3 => Fault::FakeRefs { on: honest_other, n: 2 + below(&mut rng, 6) as u32, at },
            4 => Fault::Climb { at },
            5 => Fault::Withhold,
            6 => Fault::BogusCap { at },
            9 | 10 => Fault::EquivPrevote { v_pick: mix(&mut rng), late_pick: mix(&mut rng), late_us: 500_000 + below(&mut rng, 2_500_000) },
            7 => Fault::Churn { target: honest_other, at },
            _ => Fault::GhostVote { target: honest_other, at },
        };
        faulty.insert(m, fault);
    }
    let gst = 8_000_000 + below(&mut rng, 3_000_000);
    // Sometimes an outside joiner whose request reaches only some members; always one, reaching every
    // member, when a member equivocates its prevote, so a view change gives it something to fork.
    let equiv_drawn = faulty.values().any(|f| matches!(f, Fault::EquivPrevote { .. }));
    let coin = below(&mut rng, 2) == 0;
    let joiner = if equiv_drawn || coin {
        let j = n as u32 + 1;
        let k = 1 + below(&mut rng, n as u64) as usize;
        let mut to: Vec<MemberId> = ids.clone();
        // Keep a random k recipients.
        while to.len() > k {
            let i = below(&mut rng, to.len() as u64) as usize;
            to.remove(i);
        }
        // A joiner that crashes once admitted is one more faulty member of n + 1: draw it only while the
        // faulty set stays within t(n + 1) = floor(n / 3), or the schedule is outside the bound
        // (a randsched seed found it: 2 of 5 silent, a safe halt as twosilent5 (b), reported as a lag).
        let crash = below(&mut rng, 2) == 0;
        if equiv_drawn {
            Some((j, ids.clone(), false))
        } else {
            Some((j, to, crash && f + 1 <= n / 3))
        }
    } else {
        None
    };
    // Review VC4R-2's shape (joinwedge's), drawn from a separate stream so every other draw stays as it
    // was: a quarter of the time, with a faulty member and a joiner, one faulty member instead crashes
    // right after its first view-change prevote, and the join request reaches it and q - 1 honest members
    // only. Their polka needs its prevote, and once it is gone the decision needs a member that never
    // received the request.
    let mut rng2 = seed ^ 0x0C4F_2EED_5EED_0002;
    let mut joiner = joiner;
    if !equiv_drawn && f >= 1 && joiner.is_some() && below(&mut rng2, 4) == 0 {
        let (_, q) = quorum(QuorumRule::Bft, n);
        let honest_ids: Vec<MemberId> = ids.iter().copied().filter(|m| !faulty.contains_key(m)).collect();
        if q >= 2 && honest_ids.len() >= q {
            let fm = *faulty.keys().nth(below(&mut rng2, f as u64) as usize).unwrap();
            faulty.insert(fm, Fault::CrashAfterPrevote);
            let mut pool = honest_ids;
            let mut to = vec![fm];
            for _ in 0..q - 1 {
                let i = below(&mut rng2, pool.len() as u64) as usize;
                to.push(pool.remove(i));
            }
            to.sort();
            joiner = Some((n as u32 + 1, to, false));
        }
    }
    // Review VCR-12's shape (lagprecommit's), from its own stream: a quarter of the time with a faulty
    // member, every faulty member casts no view-change vote and one honest member acks only once every
    // 1.5 s, while a joiner asks everyone. When the honest members are no more than q, the join needs the
    // slow acker's precommit, whose boundary lies far past its stale horizon.
    let mut rng3 = seed ^ 0x0C12_2EED_5EED_0003;
    let mut slow_acker = None;
    let mut join_at = 2_000_000;
    let (_, q) = quorum(QuorumRule::Bft, n);
    let reshaped = faulty.values().any(|f| matches!(f, Fault::EquivPrevote { .. } | Fault::CrashAfterPrevote));
    if !reshaped && f >= 1 && n - f <= q && below(&mut rng3, 4) == 0 {
        for v in faulty.values_mut() {
            *v = Fault::Withhold;
        }
        let honest_ids: Vec<MemberId> = ids.iter().copied().filter(|m| !faulty.contains_key(m)).collect();
        slow_acker = Some(honest_ids[below(&mut rng3, honest_ids.len() as u64) as usize]);
        joiner = Some((n as u32 + 1, ids.clone(), false));
        // Late in the slow acker's period (its acks fall near 0.1, 1.58 and 3.07 s), so when it
        // precommits its declared horizon is over a second old, as in lagprecommit (asking at 2.8 s).
        join_at = 2_700_000 + below(&mut rng3, 250_000);
    }
    // Review VC3R-1's shape, from its own stream: a quarter of the remaining draws with a faulty member
    // turn one into a climber that also keeps a chain a step ahead.
    let mut rng4 = seed ^ 0x0C3A_2EED_5EED_0004;
    let reshaped = slow_acker.is_some() || faulty.values().any(|f| matches!(f, Fault::EquivPrevote { .. } | Fault::CrashAfterPrevote));
    if !reshaped && f >= 1 && below(&mut rng4, 4) == 0 {
        let fm = *faulty.keys().nth(below(&mut rng4, f as u64) as usize).unwrap();
        faulty.insert(fm, Fault::ClimbAhead { at: 1_500_000 + below(&mut rng4, 4_000_000) });
    }
    Schedule { ids, faulty, gst, joiner, slow_acker, join_at, end_us: gst + 6_000_000 }
}

/// The result of checking one seed.
struct Outcome {
    ok: bool,
    note: String,
}

fn run_seed(seed: u64, fixes_off: u64, verbose: bool) -> Outcome {
    let sch = draw(seed);
    let ids = sch.ids.clone();
    let faulty = sch.faulty.clone();
    let gst = sch.gst;

    // Honest members each get a few touches, so the log has content to confirm.
    let mut rng = seed ^ 0x1357_9BDF_2468_ACE0;
    let plans: BTreeMap<MemberId, BTreeMap<u64, Vec<Action>>> = ids
        .iter()
        .map(|&m| {
            let mut acts = Vec::new();
            if !faulty.contains_key(&m) {
                let count = 1 + below(&mut rng, 3);
                for _ in 0..count {
                    let frame = 40 + below(&mut rng, 400);
                    let obj = 10 + below(&mut rng, 40) as u32;
                    acts.push((frame, touch(obj, 2.0 + below(&mut rng, 5) as f32)));
                }
            }
            (m, plan(&acts))
        })
        .collect();
    let mut ps = peers_of(&ids, |m| plans.get(&m).cloned().unwrap_or_default());
    if fixes_off != 0 {
        each_sim(&mut ps, move |s| s.fixes_off = fixes_off);
    }
    for (&m, f) in &faulty {
        for p in ps.iter_mut().filter(|p| p.id == m) {
            if *f == Fault::Withhold {
                p.worlds.get_mut(&1).unwrap().sim.vcm.withhold_vc = true;
            }
        }
    }
    if let Some(sa) = sch.slow_acker {
        for p in ps.iter_mut().filter(|p| p.id == sa) {
            let r = p.worlds.get_mut(&1).unwrap();
            r.ack_every_us = 1_500_000;
            // Strictly on its grid, as lagprecommit's fixture: idle thinning's ack on leaving idle would
            // refresh its horizon as the join begins.
            r.ack_grid_only = true;
        }
    }
    if let Some((j, _, _)) = sch.joiner {
        each_sim(&mut ps, |s| s.vcm.join_rules = JoinRules::public(GUEST));
        let mut p = Peer::new(j, 0);
        p.join(1, &ids, SCENE, SimConfig::default(), 100_000, BTreeMap::new());
        ps.push(p);
        for p in ps.iter_mut().filter(|p| p.id != j) {
            p.worlds.get_mut(&1).unwrap().sim.follow(j);
        }
    }

    // The network filter: the faulty members' crashes, delays and selective delivery.
    let joiner = sch.joiner.clone();
    let faulty_net = faulty.clone();
    let net_ids = ids.clone();
    let crashed_after: Rc<RefCell<BTreeMap<MemberId, u64>>> = Rc::default();
    let ca = crashed_after.clone();
    // Review VC4R-1: an equivocator's own honest prevote is dropped; the forged pair (below) stands.
    // To make the forged prevote pivotal, one honest member's round-0 prevote is also dropped to the
    // nil half, so those members are short of a polka without the equivocator's value.
    let forged: Rc<RefCell<BTreeSet<EntryId>>> = Rc::default();
    let fg = forged.clone();
    let equiv = faulty.iter().find_map(|(m, f)| match f {
        Fault::EquivPrevote { v_pick, late_pick, late_us } => Some((*m, *v_pick, *late_pick, *late_us)),
        _ => None,
    });
    // The value side (a non-empty proper subset of the others), the nil side, and the late honest members
    // with the members their round-0 prevotes reach late (`late_us`, by any path).
    let (v_side, _nil_side, late): (Vec<MemberId>, Vec<MemberId>, Option<(Vec<MemberId>, u64, Vec<MemberId>)>) = match equiv {
        Some((eq, v_pick, late_pick, late_us)) => {
            let others: Vec<MemberId> = ids.iter().copied().filter(|&x| x != eq).collect();
            let mut v: Vec<MemberId> = others.iter().enumerate().filter(|(i, _)| v_pick >> i & 1 == 1).map(|(_, &x)| x).collect();
            if v.is_empty() || v.len() == others.len() {
                v = vec![others[0]];
            }
            let honest_others: Vec<MemberId> = others.iter().copied().filter(|x| !faulty.contains_key(x)).collect();
            // Review VC4R-1's shape (prevotefork's), drawn half the time. Of the h honest members, k cappers
            // get the equivocator's value-prevote; h - q + 1 late members' round-0 prevotes reach everyone
            // late; the rest observe. A capper then counts h - (h - q + 1) + 1 = q prevotes for the value
            // and precommits with a cap; an observer counts q - 1 and stays short, so it precommits nothing
            // and judges the caps; and k + 1 < q precommits it can process (the late members' follow their
            // late prevotes in their chains) decide nothing before the late prevotes arrive. A capper must
            // not be late itself: its precommit would wait behind its own prevote, and the polka with it.
            let (_, q) = quorum(QuorumRule::Bft, ids.len());
            let h = honest_others.len();
            let n_late = (h + 1).saturating_sub(q);
            let k_hi = q.saturating_sub(2).min(h.saturating_sub(n_late + 1));
            let shaped = late_pick & 1 == 0 && n_late >= 1 && k_hi >= 1;
            let mut late_set: Vec<MemberId> = Vec::new();
            if shaped {
                let k = 1 + ((late_pick >> 1) % k_hi as u64) as usize;
                let mut pool = honest_others.clone();
                let mut pick = late_pick >> 8;
                let mut take = |pool: &mut Vec<MemberId>| {
                    let i = (pick % pool.len() as u64) as usize;
                    pick = pick.rotate_right(7) ^ 0x9E37_79B9;
                    pool.remove(i)
                };
                v = (0..k).map(|_| take(&mut pool)).collect();
                late_set = (0..n_late).map(|_| take(&mut pool)).collect();
                v.sort();
                late_set.sort();
            }
            let nil: Vec<MemberId> = others.iter().copied().filter(|x| !v.contains(x)).collect();
            let late = if shaped {
                // Late long enough that a cap ripens (half a window) before the polka can complete.
                Some((late_set, late_us.max(1_200_000), ids.clone()))
            } else {
                (!honest_others.is_empty())
                    .then(|| (vec![honest_others[((late_pick >> 1) % honest_others.len() as u64) as usize]], late_us, nil.clone()))
            };
            (v, nil, late)
        }
        None => (Vec::new(), Vec::new(), None),
    };
    let v_side2 = v_side.clone();
    // S6's exceptions, the honest members that are not clean: the drawn late members (round-0 prevotes
    // held back past W/2), and VCR-12's slow acker, which does not keep up (its declared horizon falls
    // up to 1.5 s behind its present).
    let mut unclean: Vec<MemberId> = late.as_ref().map(|l| l.0.clone()).unwrap_or_default();
    unclean.extend(sch.slow_acker);
    // When each late member's round-0 prevote first went out (us).
    let late_t0: Rc<RefCell<BTreeMap<MemberId, u64>>> = Rc::default();
    let mut net = Net::new(link(30, 10, 0), (seed & 0xFFFF) | 1);
    net.filter = Some(Box::new(move |t, _w, from, to, m| {
        if let Some(&at) = ca.borrow().get(&from) {
            if t >= at {
                return Fate::Drop;
            }
        }
        if let Some(f) = faulty_net.get(&from) {
            match *f {
                Fault::Crash { at } if t >= at => return Fate::Drop,
                Fault::Slow { by } if t < gst => return Fate::Delay(by),
                Fault::ClimbAhead { at } if t >= at => {
                    if let Msg::Entry(e) = m {
                        let peers: Vec<MemberId> = net_ids.iter().copied().filter(|&x| x != from).collect();
                        if e.member == from && !peers.is_empty() && peers[e.seq as usize % peers.len()] != to {
                            return Fate::Drop;
                        }
                    }
                }
                Fault::Rotate if t < gst => {
                    if let Msg::Entry(e) = m {
                        // Each of its own entries reaches only one member, rotating by seq.
                        let peers: Vec<MemberId> = net_ids.iter().copied().filter(|&x| x != from).collect();
                        if e.member == from && !peers.is_empty() && peers[e.seq as usize % peers.len()] != to {
                            return Fate::Drop;
                        }
                    }
                }
                Fault::EquivPrevote { .. } => {
                    if let Msg::Entry(e) = m {
                        if e.member == from && matches!(e.payload, Payload::Prevote { .. }) && !fg.borrow().contains(&e.id()) {
                            return Fate::Drop;
                        }
                    }
                }
                _ => {}
            }
        }
        // Review VC4R-1: the late members' round-0 prevotes reach the nil side late, so for a while
        // the equivocator's forged value-prevote is what tips a member into (or out of) a polka. Late by
        // any path: a repair from a third member that holds it (the equivocator, the joiner) waits too,
        // or the nil side fetches it at once and the polka never goes missing.
        if let (Some((hs, late_us, late_to)), Msg::Entry(e)) = (&late, m) {
            if hs.contains(&e.member) && matches!(e.payload, Payload::Prevote { round: 0, .. }) {
                let t0 = *late_t0.borrow_mut().entry(e.member).or_insert(t);
                let due = t0 + late_us;
                if late_to.contains(&to) && to != e.member && t < due {
                    return Fate::Delay(due - t);
                }
            }
        }
        // An outside joiner's request reaches only its chosen recipients.
        if let (Some((j, keep, _)), Msg::Join { member, .. }) = (&joiner, m) {
            if *member == *j && !keep.contains(&to) {
                return Fate::Drop;
            }
        }
        Fate::Deliver
    }));

    let mut sim = Sim::new(net, ps, sch.end_us);
    let mut sent_join = false;
    let mut cap_done: BTreeSet<(MemberId, u32)> = BTreeSet::new();
    let mut vote_done: BTreeSet<(MemberId, u32)> = BTreeSet::new();
    let mut ghost_done: BTreeSet<MemberId> = BTreeSet::new();
    let mut climbed: BTreeSet<MemberId> = BTreeSet::new();
    let mut conf_at_gst: Option<u64> = None;
    let joiner2 = sch.joiner.clone();
    let join_at = sch.join_at;
    let faulty2 = faulty.clone();
    let honest: Vec<MemberId> = ids.iter().copied().filter(|m| !faulty.contains_key(m)).collect();
    let honest_snapshot = honest.clone();

    let stopped = run_quiet(&mut sim, |sim| {
        let now = sim.net.now_us;
        // Field-based faults, set once at their start time.
        for (&m, f) in &faulty2 {
            match *f {
                Fault::Crash { at } if now >= at => {
                    crashed_after.borrow_mut().entry(m).or_insert(at);
                }
                Fault::FakeRefs { on, n, at } if now >= at && sim.replica(m, 1).sim.fake_refs.is_none() => {
                    sim_mut(sim, m).fake_refs = Some((on, n));
                }
                Fault::CrashAfterPrevote
                    if !crashed_after.borrow().contains_key(&m)
                        && sim.replica(m, 1).sim.all_entries().iter().any(|e| e.member == m && matches!(e.payload, Payload::Prevote { .. })) =>
                {
                    crashed_after.borrow_mut().insert(m, now);
                }
                Fault::ClimbAhead { at } if now >= at && climbed.insert(m) => {
                    let e = sim_mut(sim, m).vcm.epoch;
                    sim_mut(sim, m).vcm.fake_ack_epoch = Some(e + 1);
                }
                Fault::Climb { at } if now >= at && climbed.insert(m) => {
                    let e = sim_mut(sim, m).vcm.epoch;
                    sim_mut(sim, m).vcm.fake_ack_epoch = Some(e + 1);
                }
                _ => {}
            }
        }
        // Authored adversarial entries, at most once per epoch.
        for (&m, f) in &faulty2 {
            if !sim.replica(m, 1).sim.vcm.members.contains(&m) {
                continue;
            }
            match *f {
                Fault::BogusCap { at } if now >= at && now < gst => {
                    let epoch = sim.replica(m, 1).sim.vcm.epoch;
                    if cap_done.insert((m, epoch)) {
                        let b = sim.replica(m, 1).sim.now + 30;
                        let sig = test_fact_sig(m, 1, vc_fact(epoch, 0, 0xBAD0 ^ epoch as u64));
                        let e = sim_mut(sim, m).author(Payload::Precommit { epoch, round: 0, value: 0xBAD0 ^ epoch as u64, boundary: b, sig }, None);
                        send_to(sim, m, &ids, &e);
                    }
                }
                Fault::Churn { target, at } if now >= at && now < gst => {
                    let epoch = sim.replica(m, 1).sim.vcm.epoch;
                    if sim.replica(m, 1).sim.vcm.members.contains(&target) && vote_done.insert((m, epoch)) {
                        let frozen = head_of(sim, m, target);
                        let e = sim_mut(sim, m).author(Payload::Vote { epoch, target, seq: 0, frozen, kick: EntryId::NONE }, None);
                        send_to(sim, m, &ids, &e);
                    }
                }
                Fault::GhostVote { target, at } if now >= at => {
                    let epoch = sim.replica(m, 1).sim.vcm.epoch;
                    if sim.replica(m, 1).sim.vcm.members.contains(&target) && ghost_done.insert(m) {
                        let frozen = head_of(sim, m, target);
                        let e = sim_mut(sim, m).author(Payload::Vote { epoch, target, seq: 0, frozen, kick: EntryId::fake(0xDEAD_BEEF) }, None);
                        send_to(sim, m, &ids, &e);
                    }
                }
                Fault::EquivPrevote { .. } => {
                    // Fork each round's prevote once: the real value to the drawn value side, nil to the
                    // rest. The filter drops m's own honest prevote.
                    let r = &sim.replica(m, 1).sim;
                    let pv = r.all_entries().into_iter().find(|e| {
                        e.member == m && matches!(e.payload, Payload::Prevote { value, .. } if value != 0) && !vote_done.contains(&(m, e.seq))
                    });
                    if let Some(pv) = pv {
                        let Payload::Prevote { epoch, round, value } = pv.payload else { unreachable!() };
                        vote_done.insert((m, pv.seq));
                        let mut vv = pv.clone();
                        vv.payload = Payload::Prevote { epoch, round, value };
                        let mut nil = pv.clone();
                        nil.payload = Payload::Prevote { epoch, round, value: 0 };
                        forged.borrow_mut().insert(vv.id());
                        forged.borrow_mut().insert(nil.id());
                        for &peer in ids.iter().filter(|&&x| x != m) {
                            let e = if v_side2.contains(&peer) { &vv } else { &nil };
                            sim.net.send(1, m, peer, &Msg::Entry(test_sign(1, e.clone())));
                        }
                    }
                }
                _ => {}
            }
        }
        // The outside joiner asks once, at `join_at` (2 s but in VCR-12's shape), then crashes after its
        // first prevote if drawn.
        if let Some((j, _, crashes)) = joiner2 {
            if !sent_join && now >= join_at {
                let sid = TEST_SESSION;
                for &m in &ids {
                    sim.net.send(1, j, m, &join_msg(&sid, j, vec![], u64::MAX));
                }
                let mut log = sim.replica(1, 1).sim.all_entries();
                log.sort_by_key(|e| (e.member, e.seq));
                for e in log {
                    sim.net.send(1, 1, j, &Msg::Entry(test_sign(1, e)));
                }
                sent_join = true;
            }
            if crashes && sent_join && !crashed_after.borrow().contains_key(&j) && sim.replica(j, 1).sim.all_entries().iter().any(|e| e.member == j && matches!(e.payload, Payload::Prevote { .. })) {
                crashed_after.borrow_mut().insert(j, now);
            }
        }
        // Snapshot the live honest members' confirmed frame at GST, for the liveness check.
        if conf_at_gst.is_none() && now >= gst {
            conf_at_gst = Some(min_conf(sim, &honest_snapshot));
            // Trust survey #4 (user rulings, 2026-11-01): a live faulty member whose lever holds
            // confirmation (fake references, a bogus cap, a ghost kick vote, acks climbing to an epoch
            // with no decision) is no longer voted out; it is left to an admin. At GST the honest admin
            // with the lowest id kicks every such member still in the session, so L1 is checked "once
            // the admin has kicked" (INVARIANTS.md).
            let admin = honest_snapshot[0];
            for (&m, f) in &faulty2 {
                let stalls = matches!(f, Fault::FakeRefs { .. } | Fault::BogusCap { .. } | Fault::GhostVote { .. } | Fault::Climb { .. } | Fault::ClimbAhead { .. });
                if stalls && sim.replica(admin, 1).sim.vcm.members.contains(&m) {
                    let e = sim_mut(sim, admin).kick(m);
                    let to: Vec<MemberId> = sim.replica(admin, 1).sim.vcm.members.iter().copied().collect();
                    for t in to {
                        if t != admin {
                            sim.net.send(1, admin, t, &Msg::Entry(e.clone()));
                        }
                    }
                }
            }
        }
    });

    check(&sim, &honest, &faulty, &crashed_after.borrow(), &unclean, sch.joiner.as_ref(), sch.slow_acker, conf_at_gst, stopped, seed, verbose)
}

fn send_to(sim: &mut Sim, from: MemberId, ids: &[MemberId], e: &Entry) {
    for &m in ids {
        if m != from {
            sim.net.send(1, from, m, &Msg::Entry(test_sign(1, e.clone())));
        }
    }
}

fn head_of(sim: &Sim, m: MemberId, author: MemberId) -> (u32, EntryId) {
    sim.replica(m, 1).sim.all_entries().into_iter().filter(|e| e.member == author).map(|e| (e.seq, e.id())).max().unwrap_or((0, EntryId::NONE))
}

fn min_conf(sim: &Sim, members: &[MemberId]) -> u64 {
    members
        .iter()
        .filter(|&&m| sim.replica(m, 1).sim.vcm.members.contains(&m))
        .map(|&m| sim.replica(m, 1).sim.conf)
        .min()
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn check(
    sim: &Sim,
    honest: &[MemberId],
    faulty: &BTreeMap<MemberId, Fault>,
    crashed: &BTreeMap<MemberId, u64>,
    unclean: &[MemberId],
    joiner: Option<&(MemberId, Vec<MemberId>, bool)>,
    slow_acker: Option<MemberId>,
    conf_at_gst: Option<u64>,
    stopped: Option<u64>,
    seed: u64,
    verbose: bool,
) -> Outcome {
    let mut fails: Vec<String> = Vec::new();
    // I2 safety: any panic (the confirmed-frame rollback assertion included) is a failure.
    if let Some(t) = stopped {
        fails.push(format!("panic/stop at t={t} ms"));
    }
    // A member is honest-live if it is honest, not crashed, and still a member in its own view.
    let live: Vec<MemberId> = honest
        .iter()
        .copied()
        .filter(|m| !crashed.contains_key(m) && sim.replica(*m, 1).sim.vcm.members.contains(m))
        .collect();

    // I1 agreement: over the lowest confirmed frame among the live honest members, every one equals
    // the offline replay of the final log.
    if live.len() >= 1 {
        let frames = live.iter().map(|&m| sim.replica(m, 1).sim.conf).min().unwrap_or(0);
        if frames > 0 {
            let first = &sim.replica(live[0], 1).sim;
            let members: Vec<MemberId> = first.vcm.members.iter().copied().collect();
            let reference = replay(1, SCENE, first.cfg, first.public_keys(), &members, &final_log(first), frames);
            let rd = digest(&reference, frames);
            for &m in &live {
                if digest(&sim.replica(m, 1).sim.hashes, frames) != rd {
                    fails.push(format!("m{m} diverges from the replay at frames 0..{frames}"));
                }
            }
        }
    }
    // I3 integrity: no false check, no non-finite state, among the honest members.
    for &m in honest {
        let r = &sim.replica(m, 1).sim;
        if r.stats.checks_bad > 0 {
            fails.push(format!("m{m} has {} false checks", r.stats.checks_bad));
        }
        if r.stats.nonfinite > 0 {
            fails.push(format!("m{m} has {} non-finite states", r.stats.nonfinite));
        }
    }
    // I4 no honest removal: an honest, non-crashed member is on clean links and must never be
    // evicted at any honest replica.
    for &m in honest {
        if crashed.contains_key(&m) {
            continue;
        }
        for &h in honest {
            if sim.replica(h, 1).sim.members_evicted().contains_key(&m) {
                fails.push(format!("honest m{m} evicted at m{h}"));
            }
        }
    }
    // S6 no false accusation (INVARIANTS.md): an honest member never votes to remove or demote an
    // honest, non-crashed, clean member: every message it sends reaches every honest member within W/2
    // frames, it acks at its configured interval, and its declared horizon stays within W/2 of its
    // present (review S6-1: a stalled member's few messages arrive promptly, yet silence and demotion
    // rightly vote against it). The generator's honest members are clean but for `unclean`. VC4R-1's
    // harm is such a vote; with one faulty member it never gathers a quorum, so S5 alone misses it.
    for &h in honest {
        for ev in &sim.replica(h, 1).sim.events {
            let Some(rest) = ev.split_once("] member ").map(|(_, r)| r) else { continue };
            if !(ev.contains("vote to remove it") || ev.contains("vote to demote it")) {
                continue;
            }
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let Ok(target) = digits.parse::<MemberId>() else { continue };
            if honest.contains(&target) && !crashed.contains_key(&target) && !unclean.contains(&target) {
                fails.push(format!("S6: honest m{h} votes against honest m{target}: {}", &ev[..ev.len().min(120)]));
            }
        }
    }
    // I5 membership convergence: the live honest members agree on the member set.
    if let Some(&first) = live.first() {
        let set0 = sim.replica(first, 1).sim.vcm.members.clone();
        for &m in &live {
            if sim.replica(m, 1).sim.vcm.members != set0 {
                fails.push(format!("m{m}'s member set differs from m{first}'s"));
            }
        }
    }
    // L1 liveness (the reviewer's lag bound, not mere progress): after GST the live honest members
    // confirm within a bounded lag of their PRESENT. The present is the member's clock frame `now`
    // (set from the wall clock each tick), not its sim `frame`: on a halt the sim stops one speculation
    // window ahead of the frozen `conf`, so `frame - conf` is pinned at one window however long the halt
    // lasts (review of 6.1: the old `frame - conf` measure could never reach the bound, so L1 caught no
    // halt). `now - conf` is what grows on a halt, because the clock keeps ticking while nothing confirms.
    let end_conf = min_conf(sim, &live);
    let lag = live
        .iter()
        .filter(|&&m| sim.replica(m, 1).sim.vcm.members.contains(&m))
        .map(|&m| {
            let r = &sim.replica(m, 1).sim;
            r.now.saturating_sub(r.conf)
        })
        .max()
        .unwrap_or(0);
    // The bound sits between a healthy or recovered session (confirmation trails the present by well
    // under a speculation window) and a halt that begins near GST (its lag grows by the whole post-GST
    // tail, about three windows, on top of the window it already trails): two windows (about 4 s).
    let bound = 2 * sim.replica(live.first().copied().unwrap_or(honest[0]), 1).sim.cfg.window;
    if !live.is_empty() && lag > bound {
        let at_gst = conf_at_gst.unwrap_or(0);
        let present = sim.replica(live.first().copied().unwrap_or(honest[0]), 1).sim.now;
        fails.push(format!("confirmation lag {lag} > {bound} frames at the end (present {present}, min honest conf {end_conf}, was {at_gst} at GST): not keeping up"));
    }
    // L2 a drawn faulty member that disrupts should end up removed or crashed, or the session
    // reaches a stable epoch without it being able to keep forcing changes. We only report this,
    // never fail on it: churn by a live voter is left to admins (V4).

    let ok = fails.is_empty();
    if !ok || verbose {
        let faults: Vec<String> = faulty.iter().map(|(m, f)| format!("m{m}={f:?}")).collect();
        let mut joined = joiner.map(|(j, to, c)| format!("; joiner m{j} -> {to:?} crash={c}")).unwrap_or_default();
        if let Some(sa) = slow_acker {
            joined.push_str(&format!("; m{sa} acks every 1.5 s"));
        }
        println!(
            "seed {seed}: n={} faulty [{}]{joined} -> {}",
            honest.len() + faulty.len(),
            faults.join(", "),
            if ok { "ok".to_string() } else { format!("FAIL: {}", fails.join("; ")) }
        );
        if verbose {
            for &m in honest {
                let r = &sim.replica(m, 1).sim;
                println!("  m{m}: conf {} frame {} members {:?} evicted {:?}", r.conf, r.frame, r.vcm.members, r.members_evicted().keys().collect::<Vec<_>>());
            }
            for &hm in honest {
                let r = &sim.replica(hm, 1).sim;
                for e in r.events.iter().filter(|e| e.contains("vote to remove") || e.contains("VIEW CHANGE") || e.contains("claim") || e.contains("silent") || e.contains("delivered")) {
                    println!("  m{hm} ev {}", &e[..e.len().min(150)]);
                }
            }
        }
    }
    Outcome { ok, note: fails.join("; ") }
}

pub fn main(args: &[String]) {
    let get = |k: &str| args.iter().position(|x| x == k).and_then(|i| args.get(i + 1)).cloned();
    let verbose = args.iter().any(|a| a == "--verbose");
    let fixes_off = get("--fixes-off").map_or(0, |v| u64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(0));
    if let Some(s) = get("--seed") {
        let seed: u64 = s.parse().unwrap();
        let o = run_seed(seed, fixes_off, true);
        println!("RESULT randsched: seed {seed} -> {}", if o.ok { "ok".to_string() } else { format!("FAIL: {}", o.note) });
        return;
    }
    let from: u64 = get("--from").map_or(0, |v| v.parse().unwrap());
    let seeds: u64 = get("--seeds").map_or(50, |v| v.parse().unwrap());
    let mut failed = Vec::new();
    for seed in from..from + seeds {
        let o = run_seed(seed, fixes_off, verbose);
        if !o.ok {
            failed.push(seed);
        }
    }
    let tag = if fixes_off != 0 { format!(" (fixes_off {fixes_off:#x})") } else { String::new() };
    if failed.is_empty() {
        println!("RESULT randsched{tag}: {seeds} seeds from {from}, every invariant held -> ALL PASS");
    } else {
        println!("RESULT randsched{tag}: {seeds} seeds from {from}, {} failed: {:?} -> FAIL", failed.len(), failed);
    }
}
