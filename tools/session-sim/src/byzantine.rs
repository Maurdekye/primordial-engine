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
use crate::scenario::{Byzantine, JoinReq, R14Attack, When};

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
    // P2-5, by the dishonest-majority world W (6, members 2 and 3):
    "w_doublespend",
    "w_mint_live_id",
    "w_mint_unborn_id",
    "w_forge_earlier_hop",
    "w_splice_hop",
    "w_refund_setup",
    "w_refund_export",
    "carry_back",
    "w_resurrect",
];

pub struct Actor {
    spec: Byzantine,
    pub fired: bool,
    /// HideRefs: it has stopped hiding (`until_ms`).
    done: bool,
    /// ForkChain: (seq, id) of the forged chain's head.
    head: (u32, EntryId),
    forged_ids: Rc<RefCell<BTreeSet<EntryId>>>,
    forged_vote: Rc<Cell<EntryId>>,
    /// P2-5 refund-and-keep: A:55's transfer and escrow, once W has certified a refusal to A.
    refunded: Option<(u64, Escrow)>,
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
        Ok(Actor { spec, fired: false, done: false, head: (0, 0), forged_ids: Rc::default(), forged_vote: Rc::default(), refunded: None })
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
        if let Byzantine::ForgeProposal { world, member, .. } = &self.spec {
            let p0 = sim.peers.iter().find_map(|p| p.worlds.get(world)).and_then(|r| r.sim.proposer_of(0, 0));
            if p0 != Some(*member) {
                return Err(format!("forge_proposal: member {member} is not world {world}'s first proposer ({p0:?})"));
            }
            return Ok(());
        }
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
            Byzantine::ForgeProposal { member, to, .. } => {
                let (member, to, fv) = (*member, to.clone(), self.forged_vote.clone());
                Some(Box::new(move |_t, _w, from, dest, m| match m {
                    Msg::Entry(e) if from == member && to.contains(&dest) && matches!(e.payload, Payload::Propose { .. }) && e.id() != fv.get() => Some(Fate::Drop),
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
                let s = &sim.replica(*member, *world).sim;
                let (now, epoch) = (s.now, s.epoch);
                let e = Entry {
                    member: *member,
                    seq: act.head.0 + 1,
                    prev: act.head.1,
                    stamp: now,
                    payload: Payload::Ack { horizon: now.saturating_sub(1), heads: vec![], check: None, epoch },
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
                let Payload::Vote { epoch, target, seq, kick, .. } = v.payload else { unreachable!() };
                let mut f = v.clone();
                f.payload = Payload::Vote { epoch, target, seq, frozen: (frozen[0] as u32, frozen[1]), kick };
                act.forged_vote.set(f.id());
                for &m in to {
                    sim.net.send(*world, *member, m, &Msg::Entry(f.clone()));
                }
                if let Some(s) = say {
                    println!("{}", template(s, &Vars { sim, world: *world, entry: Some(&f) }));
                }
                act.fired = true;
            }
            Byzantine::HideRefs { world, member, from_ms, until_ms, ally_until } => {
                let now = sim.net.now_us;
                if !act.fired && now >= from_ms * 1000 {
                    sim_mut(sim, *member, *world).freeze_others();
                    act.fired = true;
                }
                if let (true, false, Some(u)) = (act.fired, act.done, until_ms) {
                    if now >= u * 1000 {
                        let r = sim_mut(sim, *member, *world);
                        if *ally_until {
                            r.byz_ally = false;
                        }
                        r.unfreeze_others();
                        act.done = true;
                    }
                }
            }
            Byzantine::ForgeProposal { world, member, to } if !act.fired => {
                let genuine = sim.replica(*member, *world).sim.all_entries().into_iter().find(|e| e.member == *member && matches!(e.payload, Payload::Propose { epoch: 0, round: 0, .. }));
                let Some(g) = genuine else { continue };
                let Payload::Propose { epoch, round, valid_round, ref change } = g.payload else { unreachable!() };
                let mut f = g.clone();
                let reset = Change { epoch, remove: vec![], add: vec![], boundary: 0, excluded: change.excluded.clone(), admin: vec![], add_roles: vec![], add_attests: vec![] };
                f.payload = Payload::Propose { epoch, round, valid_round, change: reset };
                act.forged_vote.set(f.id());
                for &m in to {
                    sim.net.send(*world, *member, m, &Msg::Entry(f.clone()));
                }
                let fooled: BTreeSet<MemberId> = to.iter().copied().collect();
                println!(
                    "  t={} ms: {member} proposes removing {:?} to the others and a reset to {fooled:?}",
                    sim.net.now_us / 1000,
                    change.remove.iter().map(|r| r.member).collect::<Vec<_>>()
                );
                act.fired = true;
            }
            _ => {}
        }
    }
}

fn sim_mut(sim: &mut Sim, member: MemberId, w: WorldId) -> &mut WorldSim {
    let i = sim.peers.iter().position(|p| p.id == member).unwrap();
    &mut sim.peers[i].worlds.get_mut(&w).unwrap().sim
}

/// Before the step: the join requests due (each once, at the first millisecond at or after its
/// time), in file order.
pub fn send_joins(sim: &mut Sim, joins: &[JoinReq], sent: &mut [bool]) -> Result<(), String> {
    for (j, done) in joins.iter().zip(sent.iter_mut()) {
        if *done || sim.net.now_us < j.at_ms * 1000 {
            continue;
        }
        let attests = j.attests.iter().map(|a| Ok(attest(crate::setup::rel(&a.rel)?, j.member, a.with, a.world))).collect::<Result<Vec<_>, String>>()?;
        for &m in &j.to {
            let a = if j.attests_to.as_ref().is_none_or(|t| t.contains(&m)) { attests.clone() } else { vec![] };
            sim.net.send(j.world, j.member, m, &Msg::Join { member: j.member, attests: a });
        }
        if let Some(from) = j.log_from {
            for e in sim.replica(from, j.world).sim.all_entries() {
                sim.net.send(j.world, from, j.member, &Msg::Entry(e));
            }
        }
        *done = true;
    }
    Ok(())
}

/// After the step: the R14 forgeries due at this millisecond.
pub fn after_step(sim: &mut Sim, actors: &mut [Actor], forged: &mut Vec<Forged>) {
    for act in actors.iter_mut() {
        if let Byzantine::R14Forgeries { attacks } = &act.spec {
            r14_forgeries(sim, attacks, forged, &mut act.refunded);
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

/// P2-5: a forged arrival from the dishonest-majority world W (6, members 2 and 3), signed by both
/// of W's members, into world `w` by member `by` (session-sim main.rs `forge_from_w`).
fn forge_from_w(sim: &mut Sim, w: WorldId, by: MemberId, record: ObjRecord, chain: Vec<Hop>) -> Entry {
    let fake = pe_sim_core::splitmix64(record.global ^ record.via.rotate_left(17) ^ ((w as u64) << 48) ^ chain.len() as u64);
    let deadline = sim.replica(by, w).sim.now + 600;
    let fact = fact_departed(6, fake, &record, w, deadline, chain_hash(&chain));
    let cert: Vec<(MemberId, u64)> = [2u32, 3].iter().map(|&m| (m, member_sig(6, m, fact))).collect();
    let e = author(sim, by, w, Payload::Arrive { transfer: transfer_id(6, fake), record, depart: fake, deadline, cert, chain });
    send_all(sim, w, by, &e);
    e
}

/// A's object `i` as a global id.
const fn ga(i: u64) -> u64 {
    (1u64 << 32) | i
}

/// World A is 1, B is 2, D (member 2 alone) is 4, E (member 3 alone) is 5, W (members 2 and 3, a
/// dishonest majority) is 6; the carry moves A's object 40 from A to B (session-sim r14). Ported
/// from the spike's `r14 forge` and `r14 doublespend` loops; list the attacks in the spike's order.
fn r14_forgeries(sim: &mut Sim, attacks: &[R14Attack], forged: &mut Vec<Forged>, refunded: &mut Option<(u64, Escrow)>) {
    let t = sim.net.now_us;
    // The carry's escrow while it is pending (the forged departures add rejected escrows, and
    // transfer ids, hence map order, depend on the carried state).
    let esc = sim.replica(2, 1).sim.t.escrow.iter().find(|(_, e)| e.status == EscrowStatus::Pending).map(|(t, e)| (*t, *e));
    // A sane body state (at rest, 2 m up, unit rotation), so only provenance can refuse it.
    let mut st = BodyState([0; 13]);
    st.0[1] = 2.0f32.to_bits();
    st.0[6] = 1.0f32.to_bits();
    let rec = |global: u64, via: u64| ObjRecord { global, kind: 1, state: st, src_world: 6, src_frame: 50, via };
    // The refund setup watches every millisecond from its time until it fires; the rest fire once.
    let due = |a: &R14Attack| if a.attack == "w_refund_setup" { t >= a.at_ms * 1000 } else { t == a.at_ms * 1000 };
    for a in attacks.iter().filter(|a| due(a)) {
        match a.attack.as_str() {
            "depart_to_own_world" => {
                // R3-4: member 2 departs A's object 50 to its own one-member world D, where it
                // could certify anything (accept, then "rejected" for a refund).
                let e = author(sim, 2, 1, Payload::Depart { obj: 50, dest: 4, deadline: u64::MAX, hop: None });
                forged.push((1, 2, e.id(), "member 2 departs A's object 50 to D, a world it controls alone"));
                send_all(sim, 1, 2, &e);
            }
            "depart_to_unknown_world" => {
                // R3-6: member 2 departs A's object 51 to a world nobody knows, deadline 0.
                let e = author(sim, 2, 1, Payload::Depart { obj: 51, dest: 999, deadline: 0, hop: None });
                forged.push((1, 2, e.id(), "member 2 departs A's object 51 to unknown world 999"));
                send_all(sim, 1, 2, &e);
            }
            "mint_from_own_world" => {
                // R3-5: member 3 certifies, as the whole membership of its world E, a departure
                // that never happened, of a record carrying A's object 40's global id.
                let record = ObjRecord { global: (1u64 << 32) | 40, kind: 1, state: BodyState([0; 13]), src_world: 5, src_frame: 0, via: 0 };
                let fake = 0x1234_5678;
                let fact = fact_departed(5, fake, &record, 2, u64::MAX, chain_hash(&[]));
                let p = Payload::Arrive { transfer: transfer_id(5, fake), record, depart: fake, deadline: u64::MAX, cert: vec![(3, member_sig(5, 3, fact))], chain: vec![] };
                let e = author(sim, 3, 2, p);
                forged.push((2, 3, e.id(), "member 3 mints A's object 40 into B from its own world E"));
                send_all(sim, 2, 3, &e);
            }
            "mint_without_departure" => {
                // NEW-4: member 3 (only in B) mints object 40 into B with no departure behind it.
                let record = ObjRecord { global: (1u64 << 32) | 40, kind: 1, state: BodyState([0; 13]), src_world: 1, src_frame: 100, via: 0 };
                let fake = 0xDEAD_BEEF;
                let fact = fact_departed(1, fake, &record, 2, u64::MAX, chain_hash(&[]));
                let p = Payload::Arrive { transfer: transfer_id(1, fake), record, depart: fake, deadline: u64::MAX, cert: vec![(3, member_sig(1, 3, fact))], chain: vec![] };
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
                let record = ObjRecord { global: (1u64 << 32) | 41, kind: 1, state: st, src_world: 1, src_frame: 150, via: 0 };
                let fake = 0x0BAD_0BAD;
                let fact = fact_departed(1, fake, &record, 2, u64::MAX, chain_hash(&[]));
                let cert: Vec<(MemberId, u64)> = [1u32, 2].iter().map(|&m| (m, member_sig(1, m, fact))).collect();
                let e = author(sim, 1, 2, Payload::Arrive { transfer: transfer_id(1, fake), record, depart: fake, deadline: u64::MAX, cert, chain: vec![] });
                forged.push((2, 1, e.id(), "a certified record with a NaN position (a bug or a compromised majority in A)"));
                send_all(sim, 2, 1, &e);
            }
            "squat_transfer" => {
                if let Some((transfer, x)) = esc {
                    // NEW-5: member 3 squats the real transfer id before the carrier relays.
                    let p = Payload::Arrive { transfer, record: x.record.unwrap(), depart: x.depart, deadline: x.deadline, cert: vec![], chain: vec![] };
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
                    let fact = fact_departed(1, x.depart, &rec, 2, x.deadline, chain_hash(&[]));
                    if let Some(cert) = sim.replica(1, 1).sim.cert(fact) {
                        let bad = ObjRecord { kind: (rec.kind + 1) % 3, ..rec };
                        let e = author(sim, 1, 2, Payload::Arrive { transfer, record: bad, depart: x.depart, deadline: x.deadline, cert, chain: vec![] });
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
            // ---- P2-5: dishonest-majority attacks by W (members 2 and 3 sign as W) ----
            "w_doublespend" => {
                // A:52 went A -> W -> B. W's majority exports it again, into A, citing its genuine
                // hop in: A's own last exit is on the chain, so A cannot tell.
                let a = &sim.replica(1, 1).sim;
                let in_b = sim.replica(3, 2).sim.t.objs.iter().flatten().any(|o| o.global == ga(52));
                if let (true, Some(Seen::Left { transfer: t1, dest: 6, .. })) = (in_b, a.t.seen.get(&ga(52)).cloned()) {
                    let cert = a.cert(fact_committed(1, t1, ga(52), 6, 0)).unwrap();
                    let hop = Hop { src: 1, dest: 6, transfer: t1, via: 0, epoch: 0, frame: 0, certs: Vec::new(), cert };
                    let e = forge_from_w(sim, 1, 2, rec(ga(52), t1), vec![hop]);
                    forged.push((1, 2, e.id(), "RESIDUAL: W's majority double-spends A:52 (now in B) into A, citing its genuine hop in"));
                }
            }
            "w_mint_live_id" => {
                let e = forge_from_w(sim, 2, 3, rec(ga(53), 0), vec![]);
                forged.push((2, 3, e.id(), "W's majority mints A:53 (live in A) into B, as if born in W"));
            }
            "w_mint_unborn_id" => {
                let e = forge_from_w(sim, 2, 3, rec((6u64 << 32) | 500, 0), vec![]);
                forged.push((2, 3, e.id(), "W's majority mints W:500, an id W was never born with, into B"));
            }
            "w_forge_earlier_hop" => {
                // A hop A -> W that A never certified: the attackers hold only member 2's key of A.
                let fake = 0xF0F0;
                let fact = fact_committed(1, fake, ga(54), 6, 0);
                let hop = Hop { src: 1, dest: 6, transfer: fake, via: 0, epoch: 0, frame: 0, certs: Vec::new(), cert: vec![(2, member_sig(1, 2, fact))] };
                let e = forge_from_w(sim, 2, 3, rec(ga(54), fake), vec![hop]);
                forged.push((2, 3, e.id(), "W's majority forges an earlier hop (A -> W of A:54, signed by m2 of A only)"));
            }
            "w_splice_hop" => {
                // A's genuine commit certificate of A:52's hop into W, spliced onto A:54.
                let a = &sim.replica(1, 1).sim;
                if let Some(Seen::Left { transfer, dest: 6, via: 0, .. }) = a.t.seen.get(&ga(52)).cloned() {
                    if let Some(cert) = a.cert(fact_committed(1, transfer, ga(52), 6, 0)) {
                        let hop = Hop { src: 1, dest: 6, transfer, via: 0, epoch: 0, frame: 0, certs: Vec::new(), cert };
                        let e = forge_from_w(sim, 2, 3, rec(ga(54), transfer), vec![hop]);
                        forged.push((2, 3, e.id(), "W's majority splices A's genuine hop of A:52 onto A:54"));
                    }
                }
            }
            "w_refund_setup" => {
                if refunded.is_some() {
                    continue;
                }
                // Refund and keep: once A has certified A:55's departure to W (so W's replicas will
                // accept it), W's majority also certifies a refusal to A, which refunds it.
                let a = &sim.replica(2, 1).sim;
                let esc55 = a.t.escrow.iter().find(|(_, e)| e.dest == 6 && e.status == EscrowStatus::Pending && e.record.is_some_and(|r| r.global == ga(55)));
                if let Some((transfer, x)) = esc55.map(|(t, e)| (*t, *e)) {
                    let r = x.record.unwrap();
                    if a.cert(fact_departed(1, x.depart, &r, 6, x.deadline, chain_hash(&[]))).is_some() {
                        let fact = fact_verdict(6, transfer, false);
                        let wcert: Vec<(MemberId, u64)> = [2u32, 3].iter().map(|&m| (m, member_sig(6, m, fact))).collect();
                        let e = author(sim, 2, 1, Payload::Verdict { transfer, accepted: false, cert: wcert });
                        println!("  setup t={} ms: W's majority certifies to A a refusal of A:55 (transfer {transfer}), while W's replicas accept it", t / 1000);
                        send_all(sim, 1, 2, &e);
                        *refunded = Some((transfer, x));
                    }
                }
            }
            "w_refund_export" => {
                if let Some((transfer, x)) = *refunded {
                    // ... then W exports A:55 to B, presenting A's departure certificate (all A ever
                    // signed of this hop) in place of a commit certificate.
                    let r = x.record.unwrap();
                    let cert = sim.replica(2, 1).sim.cert(fact_departed(1, x.depart, &r, 6, x.deadline, chain_hash(&[]))).unwrap();
                    let hop = Hop { src: 1, dest: 6, transfer, via: 0, epoch: 0, frame: 0, certs: Vec::new(), cert };
                    let e = forge_from_w(sim, 2, 3, rec(ga(55), transfer), vec![hop]);
                    forged.push((2, 3, e.id(), "refund and keep: W exports A:55 (refunded to A) into B with A's departure certificate"));
                }
            }
            "carry_back" => {
                // Member 1 carries A:52 from B back to A (found by global id: in the control run, the
                // accepted forgeries shift B's local ids). Member 1 is in both, so it relays.
                let i = sim.peers.iter().position(|p| p.id == 1).unwrap();
                let r = &sim.peers[i].worlds[&2];
                let obj = r.sim.t.objs.iter().position(|o| o.is_some_and(|o| o.global == ga(52))).map(|i| i as u32);
                if let Some(obj) = obj {
                    let hop = r.sim.pending_hop(obj);
                    let deadline = sim.replica(1, 1).sim.now + 600;
                    let e = author(sim, 1, 2, Payload::Depart { obj, dest: 1, deadline, hop });
                    println!(
                        "  setup t={} ms: member 1 departs A:52 (B's local {obj}) from B back to A, hop in carried: {}",
                        t / 1000,
                        matches!(&e.payload, Payload::Depart { hop: Some(_), .. })
                    );
                    send_all(sim, 2, 1, &e);
                }
            }
            "w_resurrect" => {
                // Resurrection: A:52 went A -> W -> B -> A. W re-exports it, citing its genuine hop in.
                let a = &sim.replica(1, 1).sim;
                let back = a.t.objs.iter().flatten().any(|o| o.global == ga(52)) && matches!(a.t.seen.get(&ga(52)), Some(Seen::Here { from: 2, .. }));
                let first = sim.peers.iter().flat_map(|p| p.jobs.iter()).find(|j| j.src == 1 && j.dst == 6 && j.record.is_some_and(|r| r.global == ga(52))).map(|j| j.transfer);
                if let (true, Some(t1)) = (back, first) {
                    let cert = a.cert(fact_committed(1, t1, ga(52), 6, 0)).unwrap();
                    let hop = Hop { src: 1, dest: 6, transfer: t1, via: 0, epoch: 0, frame: 0, certs: Vec::new(), cert };
                    let e = forge_from_w(sim, 2, 3, rec(ga(52), t1), vec![hop.clone()]);
                    forged.push((2, 3, e.id(), "W's majority resurrects A:52 into B, which it has left for A since"));
                    let e = forge_from_w(sim, 1, 2, rec(ga(52), t1), vec![hop]);
                    forged.push((1, 2, e.id(), "W's majority resurrects A:52 into A, where it is live again"));
                }
            }
            _ => unreachable!("validated in Actor::new"),
        }
    }
}
