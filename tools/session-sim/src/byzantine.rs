//! Byzantine behaviour: members that sign conflicting entries, fork their chains or votes, lie in
//! their acks or votes, or forge cross-world records. Each behaviour acts before or after the
//! simulator's step of a millisecond, exactly where the spike's hand-written scenario loops did.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

use pe_sim_core::session::*;
use spike::{Fate, Sim, WorldId};

use crate::faults::Hook;
use crate::report::{template, Vars};
use crate::scenario::{Byzantine, R14Attack, When};

/// One forged R14 entry: (world, author, entry, what it attempts).
pub type Forged = (WorldId, MemberId, EntryId, &'static str);

pub const R14_ATTACKS: &[&str] = &[
    "depart_to_own_world",
    "depart_to_unknown_world",
    "mint_from_own_world",
    "mint_without_departure",
    "nan_record",
    "squat_transfer",
    "proofless_verdict",
    "relabel_record",
    "false_rejection",
];

pub struct Actor {
    spec: Byzantine,
    pub fired: bool,
    /// ForkChain: (seq, id) of the forged chain's head.
    head: (u32, EntryId),
    forged_ids: Rc<RefCell<BTreeSet<EntryId>>>,
    forged_vote: Rc<Cell<EntryId>>,
}

impl Actor {
    pub fn new(spec: Byzantine) -> Result<Self, String> {
        if let Byzantine::Equivocate { entry, .. } = &spec {
            if entry != "first" && entry != "first_touch" {
                return Err(format!("equivocate: entry must be `first` or `first_touch`, not `{entry}`"));
            }
        }
        if let Byzantine::R14Forgeries { attacks } = &spec {
            for a in attacks {
                if !R14_ATTACKS.contains(&a.attack.as_str()) {
                    return Err(format!("r14_forgeries: unknown attack `{}` (one of {R14_ATTACKS:?})", a.attack));
                }
            }
        }
        Ok(Actor { spec, fired: false, head: (0, 0), forged_ids: Rc::default(), forged_vote: Rc::default() })
    }

    pub fn name(&self) -> Option<&str> {
        match &self.spec {
            Byzantine::Equivocate { name, .. } | Byzantine::ForkChain { name, .. } | Byzantine::ForkVote { name, .. } => name.as_deref(),
            _ => None,
        }
    }

    fn after(&self) -> Option<&str> {
        match &self.spec {
            Byzantine::Equivocate { after, .. } | Byzantine::ForkVote { after, .. } => after.as_deref(),
            _ => None,
        }
    }

    /// Replica settings, applied once before the run.
    pub fn configure(&self, sim: &mut Sim) -> Result<(), String> {
        let (world, member) = match &self.spec {
            Byzantine::LieInVotes { world, member } | Byzantine::LagHorizon { world, member, .. } => (*world, *member),
            _ => return Ok(()),
        };
        let p = sim.peers.iter_mut().find(|p| p.id == member).ok_or(format!("byzantine member {member} is not a peer"))?;
        let r = p.worlds.get_mut(&world).ok_or(format!("byzantine member {member} is not in world {world}"))?;
        match &self.spec {
            Byzantine::LieInVotes { .. } => r.sim.lie_in_votes = true,
            Byzantine::LagHorizon { frames, .. } => r.sim.horizon_lag = *frames,
            _ => {}
        }
        Ok(())
    }

    /// The send-filter hook of a behaviour that hides a member's real messages.
    pub fn hook(&self) -> Option<Hook> {
        match &self.spec {
            Byzantine::ForkChain { member, to, hide_real: true, .. } => {
                let (member, to, ids) = (*member, to.clone(), self.forged_ids.clone());
                Some(Box::new(move |_t, _w, from, dest, m| match m {
                    Msg::Entry(e) if from == member && to.contains(&dest) && !ids.borrow().contains(&e.id()) => Some(Fate::Drop),
                    _ => None,
                }))
            }
            Byzantine::ForkVote { member, hide_real: true, .. } => {
                let (member, fv) = (*member, self.forged_vote.clone());
                Some(Box::new(move |_t, _w, from, _to, m| match m {
                    Msg::Entry(e) if from == member && matches!(e.payload, Payload::Vote { .. }) && e.id() != fv.get() => Some(Fate::Drop),
                    _ => None,
                }))
            }
            _ => None,
        }
    }
}

/// Before the step: equivocations, forked chains and votes, in file order.
pub fn before_step(sim: &mut Sim, actors: &mut [Actor]) {
    for i in 0..actors.len() {
        if let Some(a) = actors[i].after() {
            if !actors.iter().any(|x| x.name() == Some(a) && x.fired) {
                continue;
            }
        }
        let act = &mut actors[i];
        match &act.spec {
            Byzantine::Equivocate { world, member, entry, when, touch, to, say, .. } if !act.fired => {
                let ready = match when {
                    When::Exists => true,
                    When::ConfAbove { at, frame } => sim.replica(*at, *world).sim.conf > *frame,
                };
                if !ready {
                    continue;
                }
                let touch_only = entry == "first_touch";
                let Some(x) = sim
                    .replica(*member, *world)
                    .sim
                    .all_entries()
                    .into_iter()
                    .find(|e| e.member == *member && (!touch_only || matches!(e.payload, Payload::Touch { .. })))
                else {
                    continue;
                };
                if let Some(s) = say {
                    println!("{}", template(s, &Vars { sim, world: *world, entry: Some(&x) }));
                }
                let mut x2 = x.clone();
                x2.payload = Payload::Touch { obj: touch.obj, impulse: touch.impulse, grab: touch.grab };
                x2.carried = None;
                for &m in to {
                    sim.net.send(*world, *member, m, &Msg::Entry(x2.clone()));
                }
                act.fired = true;
            }
            Byzantine::ForkChain { world, member, to, every_ms, .. } => {
                if sim.net.now_us % (every_ms * 1000) != 0 {
                    continue;
                }
                let now = sim.replica(*member, *world).sim.now;
                let e = Entry {
                    member: *member,
                    seq: act.head.0 + 1,
                    prev: act.head.1,
                    stamp: now,
                    payload: Payload::Ack { horizon: now.saturating_sub(1), heads: vec![], check: None },
                    carried: None,
                };
                act.head = (e.seq, e.id());
                act.forged_ids.borrow_mut().insert(e.id());
                for &m in to {
                    sim.net.send(*world, *member, m, &Msg::Entry(e.clone()));
                }
                act.fired = true;
            }
            Byzantine::ForkVote { world, member, frozen, to, say, .. } if !act.fired => {
                let v = sim.replica(*member, *world).sim.all_entries().into_iter().find(|e| e.member == *member && matches!(e.payload, Payload::Vote { .. }));
                let Some(v) = v else { continue };
                let Payload::Vote { target, seq, .. } = v.payload else { unreachable!() };
                let mut f = v.clone();
                f.payload = Payload::Vote { target, seq, frozen: (frozen[0] as u32, frozen[1]) };
                act.forged_vote.set(f.id());
                for &m in to {
                    sim.net.send(*world, *member, m, &Msg::Entry(f.clone()));
                }
                if let Some(s) = say {
                    println!("{}", template(s, &Vars { sim, world: *world, entry: Some(&f) }));
                }
                act.fired = true;
            }
            _ => {}
        }
    }
}

/// After the step: the R14 forgeries due at this millisecond.
pub fn after_step(sim: &mut Sim, actors: &mut [Actor], forged: &mut Vec<Forged>) {
    for act in actors.iter_mut() {
        if let Byzantine::R14Forgeries { attacks } = &act.spec {
            r14_forgeries(sim, attacks, forged);
        }
    }
}

fn send_all(sim: &mut Sim, w: WorldId, from: MemberId, e: &Entry) {
    let members: Vec<MemberId> = sim.replica(from, w).sim.members.iter().copied().collect();
    for to in members {
        if to != from {
            sim.net.send(w, from, to, &Msg::Entry(e.clone()));
        }
    }
}

fn author(sim: &mut Sim, member: MemberId, w: WorldId, p: Payload) -> Entry {
    let i = sim.peers.iter().position(|p| p.id == member).unwrap();
    sim.peers[i].worlds.get_mut(&w).unwrap().sim.author(p, None)
}

/// World A is 1, B is 2, D (member 2 alone) is 4, E (member 3 alone) is 5; the carry moves A's
/// object 40 from A to B (session-sim r14). Ported from the spike's `r14 forge` loop.
fn r14_forgeries(sim: &mut Sim, attacks: &[R14Attack], forged: &mut Vec<Forged>) {
    let t = sim.net.now_us;
    // The carry's escrow while it is pending (the forged departures add rejected escrows, and
    // transfer ids, hence map order, depend on the carried state).
    let esc = sim.replica(2, 1).sim.t.escrow.iter().find(|(_, e)| e.status == EscrowStatus::Pending).map(|(t, e)| (*t, *e));
    for a in attacks.iter().filter(|a| a.at_ms * 1000 == t) {
        match a.attack.as_str() {
            "depart_to_own_world" => {
                // R3-4: member 2 departs A's object 50 to its own one-member world D, where it
                // could certify anything (accept, then "rejected" for a refund).
                let e = author(sim, 2, 1, Payload::Depart { obj: 50, dest: 4, deadline: u64::MAX });
                forged.push((1, 2, e.id(), "member 2 departs A's object 50 to D, a world it controls alone"));
                send_all(sim, 1, 2, &e);
            }
            "depart_to_unknown_world" => {
                // R3-6: member 2 departs A's object 51 to a world nobody knows, deadline 0.
                let e = author(sim, 2, 1, Payload::Depart { obj: 51, dest: 999, deadline: 0 });
                forged.push((1, 2, e.id(), "member 2 departs A's object 51 to unknown world 999"));
                send_all(sim, 1, 2, &e);
            }
            "mint_from_own_world" => {
                // R3-5: member 3 certifies, as the whole membership of its world E, a departure
                // that never happened, of a record carrying A's object 40's global id.
                let record = ObjRecord { global: (1u64 << 32) | 40, kind: 1, state: BodyState([0; 13]), src_world: 5, src_frame: 0 };
                let fake = 0x1234_5678;
                let fact = fact_departed(5, fake, &record, 2, u64::MAX);
                let p = Payload::Arrive { transfer: transfer_id(5, fake), record, depart: fake, deadline: u64::MAX, cert: vec![(3, member_sig(5, 3, fact))] };
                let e = author(sim, 3, 2, p);
                forged.push((2, 3, e.id(), "member 3 mints A's object 40 into B from its own world E"));
                send_all(sim, 2, 3, &e);
            }
            "mint_without_departure" => {
                // NEW-4: member 3 (only in B) mints object 40 into B with no departure behind it.
                let record = ObjRecord { global: (1u64 << 32) | 40, kind: 1, state: BodyState([0; 13]), src_world: 1, src_frame: 100 };
                let fake = 0xDEAD_BEEF;
                let fact = fact_departed(1, fake, &record, 2, u64::MAX);
                let p = Payload::Arrive { transfer: transfer_id(1, fake), record, depart: fake, deadline: u64::MAX, cert: vec![(3, member_sig(1, 3, fact))] };
                let e = author(sim, 3, 2, p);
                forged.push((2, 3, e.id(), "member 3 mints object 40 into B (no departure; signs as itself)"));
                send_all(sim, 2, 3, &e);
            }
            "nan_record" => {
                // NEW-21: a certified but invalid record (a bug, or a compromised majority, in A):
                // the signatures are genuine A signatures over a record with a NaN position.
                let mut st = BodyState([0; 13]);
                st.0[0] = f32::NAN.to_bits();
                st.0[6] = 1.0f32.to_bits();
                let record = ObjRecord { global: (1u64 << 32) | 41, kind: 1, state: st, src_world: 1, src_frame: 150 };
                let fake = 0x0BAD_0BAD;
                let fact = fact_departed(1, fake, &record, 2, u64::MAX);
                let cert: Vec<(MemberId, u64)> = [1u32, 2].iter().map(|&m| (m, member_sig(1, m, fact))).collect();
                let e = author(sim, 1, 2, Payload::Arrive { transfer: transfer_id(1, fake), record, depart: fake, deadline: u64::MAX, cert });
                forged.push((2, 1, e.id(), "a certified record with a NaN position (a bug or a compromised majority in A)"));
                send_all(sim, 2, 1, &e);
            }
            "squat_transfer" => {
                if let Some((transfer, x)) = esc {
                    // NEW-5: member 3 squats the real transfer id before the carrier relays.
                    let p = Payload::Arrive { transfer, record: x.record.unwrap(), depart: x.depart, deadline: x.deadline, cert: vec![] };
                    let e = author(sim, 3, 2, p);
                    forged.push((2, 3, e.id(), "member 3 squats the real transfer id with a proofless Arrive"));
                    send_all(sim, 2, 3, &e);
                }
            }
            "proofless_verdict" => {
                if let Some((transfer, _)) = esc {
                    // NEW-3 (loss): member 2 (only in A) commits the escrow with a proofless verdict.
                    let e = author(sim, 2, 1, Payload::Verdict { transfer, accepted: true, cert: vec![] });
                    forged.push((1, 2, e.id(), "member 2 commits the escrow with a proofless Verdict"));
                    send_all(sim, 1, 2, &e);
                }
            }
            "relabel_record" => {
                if let Some((transfer, x)) = esc {
                    // R3-8: the carrier relays A's genuine certificate with the record's kind changed.
                    let rec = x.record.unwrap();
                    let fact = fact_departed(1, x.depart, &rec, 2, x.deadline);
                    if let Some(cert) = sim.replica(1, 1).sim.cert(fact) {
                        let bad = ObjRecord { kind: (rec.kind + 1) % 3, ..rec };
                        let e = author(sim, 1, 2, Payload::Arrive { transfer, record: bad, depart: x.depart, deadline: x.deadline, cert });
                        forged.push((2, 1, e.id(), "the carrier relays A's genuine certificate with the record's kind changed"));
                        send_all(sim, 2, 1, &e);
                    }
                }
            }
            "false_rejection" => {
                if let Some((transfer, _)) = esc {
                    // NEW-3 (duplication): before the real verdict lands, the carrier claims B
                    // rejected the object, presenting B's genuine signatures over the acceptance.
                    let fact = fact_verdict(2, transfer, true);
                    let cert: Vec<(MemberId, u64)> = [1u32, 3].iter().map(|&m| (m, member_sig(2, m, fact))).collect();
                    let e = author(sim, 1, 1, Payload::Verdict { transfer, accepted: false, cert });
                    forged.push((1, 1, e.id(), "the carrier claims a rejection with B's signatures over the acceptance"));
                    send_all(sim, 1, 1, &e);
                }
            }
            _ => unreachable!("validated in Actor::new"),
        }
    }
}
