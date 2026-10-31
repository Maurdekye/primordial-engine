//! Session-protocol scenarios (design review C1/C2/H4/H5/M3, rulings 09:30Z/09:32Z/09:39Z, the
//! ball-pit question and R14), run in-process on a virtual clock. Deterministic: the same command
//! prints the same RESULT lines on every run.
//!
//! usage: pe-spike-session-sim <scenario> [options]
//!   c1 | c2 | state-frame | diverged | loss | ties | claims | backdate
//!   r14 [ok|abort|reject|throttle|forge|late|crash|chain] [--reverse] [--no-provenance]
//!   ballpit [--hz 60|90] [--ahead N] [--hands 1|2] [--loss P]
//!   snap [--balls N] [--stall]        snapshot-policy sweep (the memory/CPU knob, design 6.7)
//!   ackfork | lateequiv | evict | outage   round-2 review repros (design 10.8)
//!   observer [--delay MS] [--hz H]    what an observer sees of a remote hand (design 6.9)

use std::collections::BTreeMap;

mod history;
mod r18;
mod review2;
mod p24r;
mod p24r3;
mod p24r4;
mod randsched;
mod review3;
mod viewchange;

use pe_session_sim::*;
use pe_sim_core::session::*;
use pe_sim_core::{build_pile_scene, GameState, Interaction, InputError, RollbackSim, DT as OLD_DT};

fn cfg60() -> SimConfig {
    SimConfig::default()
}

fn link(delay_ms: u64, jitter_ms: u64, loss: u32) -> Link {
    Link { delay_us: delay_ms * 1000, jitter_us: jitter_ms * 1000, loss_pct: loss }
}

fn plan(items: &[(u64, Action)]) -> BTreeMap<u64, Vec<Action>> {
    let mut m: BTreeMap<u64, Vec<Action>> = BTreeMap::new();
    for (f, a) in items {
        m.entry(*f).or_default().push(a.clone());
    }
    m
}

fn touch(obj: u32, x: f32, y: f32, z: f32) -> Action {
    Action::Touch { obj, impulse: [x, y, z], grab: false }
}

/// The phase-1 scripted schedule, by member.
fn scripted(member: MemberId) -> BTreeMap<u64, Vec<Action>> {
    let all: Vec<(u64, MemberId, Action)> = vec![
        (60, 1, touch(10, 0.0, 6.0, 0.0)),
        (90, 2, touch(33, 3.0, 2.0, 0.0)),
        (120, 1, touch(50, 0.0, 8.0, 2.0)),
        (120, 2, touch(50, -4.0, 8.0, 0.0)),
        (125, 2, touch(50, 0.0, 9.0, 0.0)),
        (126, 1, touch(50, 0.0, 3.0, 0.0)),
        (200, 2, touch(77, 5.0, 5.0, 5.0)),
        (260, 1, touch(120, -6.0, 4.0, 1.0)),
        (330, 2, touch(5, 0.0, 12.0, 0.0)),
        (400, 1, touch(140, 2.0, 7.0, -3.0)),
        (470, 2, touch(99, -2.0, 6.0, 4.0)),
        (150, 3, touch(20, 1.0, 7.0, 0.0)),
        (300, 3, touch(77, -3.0, 6.0, 2.0)),
        (305, 1, touch(77, 0.0, 4.0, 0.0)),
        (450, 3, touch(140, 0.0, 5.0, -2.0)),
    ];
    plan(&all.into_iter().filter(|(_, m, _)| *m == member).map(|(f, _, a)| (f, a)).collect::<Vec<_>>())
}

/// Every replica of world `w` agrees with every other and with an offline replay of its final log.
fn check_world(sim: &Sim, w: WorldId, frames: u64, label: &str) -> bool {
    check_world_except(sim, w, frames, label, &[])
}

fn check_world_except(sim: &Sim, w: WorldId, frames: u64, label: &str, skip: &[MemberId]) -> bool {
    let mut digests = Vec::new();
    let mut first: Option<&Replica> = None;
    for p in sim.peers.iter().filter(|p| !skip.contains(&p.id)) {
        if let Some(r) = p.worlds.get(&w) {
            digests.push((p.id, digest(&r.sim.hashes, frames)));
            first.get_or_insert(r);
        }
    }
    if std::env::var("PE_EVENTS").is_ok() {
        for p in &sim.peers {
            if let Some(r) = p.worlds.get(&w) {
                for e in &r.sim.events {
                    println!("  event {e}");
                }
            }
        }
    }
    let r = first.unwrap();
    let members: Vec<MemberId> = r.sim.members.iter().copied().collect();
    let reference = replay_sim_like(&r.sim, r.scene, &members, &final_log(&r.sim), frames).hashes;
    let rd = digest(&reference, frames);
    let all_equal = digests.iter().all(|(_, d)| *d == rd);
    if !all_equal {
        if let Some(f) = (0..frames).find(|f| reference.get(f) != r.sim.hashes.get(f)) {
            let log = final_log(&r.sim);
            let near: Vec<String> = log.iter().filter(|(_, ef)| *ef + 3 >= f && *ef <= f + 1).map(|(e, ef)| format!("m{}s{}@{} eff {ef}", e.member, e.seq, e.stamp)).collect();
            println!("  first replay mismatch at frame {f}; final-log entries near it: {}", near.join(", "));
            let rs = replay_sim_like(&r.sim, r.scene, &members, &log, f + 1);
            for (e, _) in log.iter().filter(|(_, ef)| *ef == f) {
                println!("    m{}s{}: live {:?} replay {:?} carried {:?}", e.member, e.seq, r.sim.outcomes.get(&e.id()), rs.outcomes.get(&e.id()), e.carried.map(|c| c.0));
            }
        }
    }
    println!(
        "RESULT {label} world {w}: frames 0..{frames} digests {} | offline replay {rd:016x} | {}",
        digests.iter().map(|(m, d)| format!("m{m}={d:016x}")).collect::<Vec<_>>().join(" "),
        if all_equal { "ALL EQUAL" } else { "MISMATCH" }
    );
    all_equal
}

fn print_stats(sim: &Sim, w: WorldId) {
    for p in &sim.peers {
        if let Some(r) = p.worlds.get(&w) {
            let s = r.sim.stats;
            println!(
                "  m{} world {w}: rollbacks {} max depth {} resim frames {} ({:.1} ms) overwrites {} paused frames {} violations {} equivocations {} checks ok {} bad {} nonfinite {} conf {} frame {}",
                p.id, s.rollbacks, s.max_depth, s.resim_frames, s.resim_us as f64 / 1000.0, s.overwrites, s.paused_frames,
                s.entries_rejected_violation, s.equivocations, s.checks_ok, s.checks_bad, s.nonfinite, r.sim.conf, r.sim.frame
            );
        }
    }
}

fn print_events(sim: &Sim, w: WorldId, filter: &str) {
    for p in &sim.peers {
        if let Some(r) = p.worlds.get(&w) {
            for e in r.sim.events.iter().filter(|e| e.contains(filter)) {
                println!("  event {e}");
            }
        }
    }
}

fn acceptance_summary(sim: &Sim, label: &str) {
    let n = sim.acceptance.len();
    let eq = sim.acceptance.iter().filter(|a| a.equal).count();
    let fin = |a: &Acceptance| sim.replica(a.author, a.world).sim.obj_hist.get(&(a.frame, a.obj)).copied() == Some(a.receiver_hash);
    let eq_final = sim.acceptance.iter().filter(|a| fin(a)).count();
    // Contended: another member touched the same object within the 10 frames before the check, so
    // a competing entry may still have been in flight to one side.
    let contended = |a: &Acceptance| {
        sim.replica(a.author, a.world).sim.all_entries().iter().any(|e| {
            e.member != a.author
                && matches!(e.payload, Payload::Touch { obj, .. } if obj == a.obj)
                && e.stamp + 10 >= a.frame
                && e.stamp <= a.frame
        })
    };
    let unc: Vec<&Acceptance> = sim.acceptance.iter().filter(|a| !contended(a)).collect();
    println!(
        "RESULT {label} acceptance, uncontended checks: {}/{} equal at that instant and {}/{} equal to the author's final history; contended checks (another member touched the object in the previous 10 frames): {}",
        unc.iter().filter(|a| a.equal).count(), unc.len(), unc.iter().filter(|a| fin(a)).count(), unc.len(), n - unc.len()
    );
    println!(
        "RESULT {label} acceptance (after re-sim to its present frame, the receiver's copy of the touched object vs the author's copy at that frame): {eq}/{n} equal to the author's copy at that instant, {eq_final}/{n} equal to the author's final history"
    );
    for a in sim.acceptance.iter().filter(|a| !a.equal) {
        println!(
            "  differs at that instant: world {} receiver m{} author m{} obj {} frame {}; equal to the author's final history: {}",
            a.world, a.receiver, a.author, a.obj, a.frame, fin(a)
        );
    }
}

fn pile_peers(ids: &[MemberId], scene: SceneKind, cfg: SimConfig, plans: impl Fn(MemberId) -> BTreeMap<u64, Vec<Action>>) -> Vec<Peer> {
    ids.iter()
        .map(|&id| {
            let mut p = Peer::new(id, (id as i64 % 3 - 1) * 1500);
            p.join(1, ids, scene, cfg, 100_000, plans(id));
            p
        })
        .collect()
}

// ---- C1 ------------------------------------------------------------------------------------------

fn c1() {
    // Before: the phase-1 rule (TooLate by local ring position), exactly the reviewer's repro.
    let mk = || RollbackSim::new(GameState::new(build_pile_scene(8, 4), 0x5052_494D_4F52_4449), 120);
    let (mut a, mut b) = (mk(), mk());
    let late = Interaction { frame: 50, peer: 9, seq: 0, body: 10, impulse: [0.0, 6.0, 0.0] };
    a.advance_to(169);
    b.advance_to(171);
    let ra = a.add_input(late);
    let rb = b.add_input(late);
    a.advance_to(300);
    b.advance_to(300);
    let _ = OLD_DT;
    println!(
        "RESULT c1 before (phase-1 rule): A {:?}, B {:?}; hash@299 equal: {}",
        ra.map_err(|e: InputError| e),
        rb,
        a.hashes[&299] == b.hashes[&299]
    );

    // After: confirmation by acks. Member 3's link is congested for ~2 s, so its frame-50 entry
    // reaches member 1 at about frame 169 and member 2 at about frame 171.
    let ids = [1, 2, 3];
    let peers = pile_peers(&ids, SceneKind::Pile { side: 8, layers: 4 }, cfg60(), |m| {
        if m == 3 { plan(&[(50, touch(10, 0.0, 6.0, 0.0))]) } else { scripted(m) }
    });
    let mut net = Net::new(link(30, 10, 0), 7);
    net.filter = Some(Box::new(|t, _w, from, to, _m| {
        if from == 3 && (900_000..2_950_000).contains(&t) {
            Fate::Delay(if to == 1 { 1_950_000 } else { 1_983_000 })
        } else {
            Fate::Deliver
        }
    }));
    let mut sim = Sim::new(net, peers, 12_500_000);
    sim.run();
    for p in &sim.peers {
        let r = &p.worlds[&1];
        let id = r.sim.all_entries().into_iter().find(|e| e.member == 3).map(|e| e.id()).unwrap();
        let arrived = r.sim.events.len();
        let _ = arrived;
        println!("  m{} holds member 3's frame-50 entry; effective frame {:?}", p.id, r.sim.effective_frame(id));
    }
    print_stats(&sim, 1);
    check_world(&sim, 1, 700, "c1 after");
}

// ---- C2 ------------------------------------------------------------------------------------------

fn c2() {
    // Before: first-seen-wins dedup by (peer, seq).
    let mk = || RollbackSim::new(GameState::new(build_pile_scene(8, 4), 0x5052_494D_4F52_4449), 120);
    let (mut a, mut b) = (mk(), mk());
    let x = Interaction { frame: 50, peer: 9, seq: 0, body: 10, impulse: [0.0, 6.0, 0.0] };
    let x2 = Interaction { impulse: [6.0, 0.0, 0.0], ..x };
    a.add_input(x).unwrap();
    b.add_input(x2).unwrap();
    a.advance_to(200);
    b.advance_to(200);
    let (ra, rb) = (a.add_input(x2), b.add_input(x));
    a.advance_to(200);
    b.advance_to(200);
    println!("RESULT c2 before (phase-1 rule): repair a<-x' {ra:?}, b<-x {rb:?}; hash@199 equal: {}", a.hashes[&199] == b.hashes[&199]);

    // After: member 9 signs two different entries for seq 1 (frame 50) and sends one to each.
    let ids = [1, 2, 9];
    let peers = pile_peers(&ids, SceneKind::Pile { side: 8, layers: 4 }, cfg60(), |m| {
        if m == 9 { plan(&[(50, touch(10, 0.0, 6.0, 0.0))]) } else { scripted(m) }
    });
    let mut net = Net::new(link(30, 10, 0), 11);
    let mut dropped_once = false;
    net.filter = Some(Box::new(move |_t, _w, from, to, m| {
        if let Msg::Entry(e) = m {
            if from == 9 && to == 2 && e.seq == 1 && !dropped_once {
                dropped_once = true;
                return Fate::Drop;
            }
        }
        Fate::Deliver
    }));
    let mut sim = Sim::new(net, peers, 8_000_000);
    let mut injected = false;
    while sim.net.now_us <= sim.end_us {
        if !injected {
            let r = sim.replica(9, 1);
            if let Some(x) = r.sim.all_entries().into_iter().find(|e| e.member == 9) {
                let mut x2 = x.clone();
                x2.payload = Payload::Touch { obj: 10, impulse: [6.0, 0.0, 0.0], grab: false };
                x2.carried = None;
                sim.net.send(1, 9, 2, &Msg::Entry(x2));
                injected = true;
            }
        }
        sim.step_ms();
        sim.net.now_us += 1000;
    }
    print_events(&sim, 1, "EQUIVOCATION");
    print_events(&sim, 1, "evicted");
    print_stats(&sim, 1);
    // Member 9 keeps its own (one-sided) history; compare the honest members.
    let honest = Sim::new(Net::new(link(0, 0, 0), 0), sim.peers.into_iter().filter(|p| p.id != 9).collect(), 0);
    check_world(&honest, 1, 450, "c2 after (honest members 1,2)");
}

// ---- state + frame acceptance --------------------------------------------------------------------

fn state_frame(diverged: bool, loss: u32) {
    let ids = [1, 2, 3];
    let peers = pile_peers(&ids, SceneKind::Pile { side: 8, layers: 4 }, cfg60(), scripted);
    let mut net = Net::new(if loss > 0 { link(40, 30, loss) } else { link(60, 20, 0) }, 3);
    if diverged {
        // Member 3's frame-300 entry never reaches member 2 directly, and member 3's acks to
        // member 2 are delayed 400 ms, so member 2 runs without it for a while.
        net.filter = Some(Box::new(|t, _w, from, to, m| match m {
            Msg::Entry(e) if from == 3 && to == 2 && e.stamp == 300 && t < 6_000_000 => Fate::Drop,
            Msg::Entry(e) if matches!(e.payload, Payload::Ack { .. }) && from == 3 && to == 2 && (5_000_000..5_600_000).contains(&t) => Fate::Delay(400_000),
            Msg::Entry(e) if matches!(e.payload, Payload::Ack { .. }) && from == 1 && to == 2 && (5_000_000..5_600_000).contains(&t) => Fate::Delay(400_000),
            _ => Fate::Deliver,
        }));
    }
    let mut sim = Sim::new(net, peers, 13_000_000);
    sim.run();
    let label = if diverged { "diverged" } else if loss > 0 { "loss" } else { "state-frame" };
    acceptance_summary(&sim, label);
    print_stats(&sim, 1);
    let (n, b) = sim.net.bytes(1, "entry");
    let (na, ba) = sim.net.bytes(1, "ack");
    println!("  bytes: entries {n} msgs {b} B (avg {} B), acks {na} msgs {ba} B, dropped {}", b / n.max(1), sim.net.dropped);
    check_world(&sim, 1, 700, label);
}

// ---- ties, claims, backdating --------------------------------------------------------------------

fn ties() {
    let ids = [1, 2, 3];
    let frames: Vec<u64> = (1..=150).map(|k| 40 * k).collect();
    let peers = pile_peers(&ids, SceneKind::Pile { side: 8, layers: 2 }, cfg60(), |m| {
        plan(&frames.iter().map(|&f| (f, touch(50, 0.0, 0.5, 0.0))).collect::<Vec<_>>()).into_iter().map(|(f, v)| (f, v)).collect::<BTreeMap<_, _>>()
            .into_iter().filter(|_| m > 0).collect()
    });
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 5), peers, 6_100 * 16_667 + 200_000);
    sim.run();
    let r = sim.replica(1, 1);
    let mut wins: BTreeMap<MemberId, u32> = BTreeMap::new();
    for (id, (_, o)) in &r.sim.outcomes {
        if *o == Outcome::Accepted {
            if let Some(e) = r.sim.entry(*id) {
                *wins.entry(e.member).or_default() += 1;
            }
        }
    }
    println!("RESULT ties: same-frame conflicts on one object at 150 frames, winners per member {wins:?}");
    check_world(&sim, 1, 6000, "ties");
}

fn claims() {
    let ids = [1, 2];
    let p1: Vec<(u64, Action)> = (0..21).map(|k| (100 + 29 * k, touch(50, 0.0, 0.2, 0.0))).collect();
    let p2: Vec<(u64, Action)> = (100..700).map(|f| (f, touch(50, 0.0, 0.2, 0.0))).collect();
    let peers = pile_peers(&ids, SceneKind::Pile { side: 8, layers: 2 }, cfg60(), |m| if m == 1 { plan(&p1) } else { plan(&p2) });
    let mut sim = Sim::new(Net::new(link(30, 10, 0), 9), peers, 13_000_000);
    sim.run();
    let r = sim.replica(1, 1);
    let mut tally: BTreeMap<(MemberId, &str), u32> = BTreeMap::new();
    for (id, (_, o)) in &r.sim.outcomes {
        if let Some(e) = r.sim.entry(*id) {
            let k = if *o == Outcome::Accepted { "accepted" } else { "rejected" };
            *tally.entry((e.member, k)).or_default() += 1;
        }
    }
    println!("RESULT claims (member 1 nudges every 29 frames, member 2 every frame, impulses do not renew claims): {tally:?}");
    check_world(&sim, 1, 750, "claims");
}

fn backdate() {
    // Part 1: member 3 backdates by 20 frames but publishes honest horizons -> chain-local violation.
    // Part 2: member 3 also lags its published horizon by 30 frames to open a backdating window ->
    // every such ack breaks the horizon-slack rule and is void for every member, so no window
    // opens; member 3 instead blocks confirmation until the others demote it (review R3-9: its
    // horizon trails the present by more than half the speculation limit for 3 s, design 10.8).
    for (part, lag) in [(1, 0u64), (2, 30)] {
        let ids = [1, 2, 3];
        let cheats: Vec<(u64, Action)> = [200u64, 320, 440].iter().map(|&f| (f, Action::Backdated { obj: 50, impulse: [0.0, 6.0, 0.0], back: 20 })).collect();
        let mut peers = pile_peers(&ids, SceneKind::Pile { side: 8, layers: 2 }, cfg60(), |m| if m == 3 { plan(&cheats) } else { scripted(m) });
        peers[2].worlds.get_mut(&1).unwrap().sim.horizon_lag = lag;
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 13), peers, 11_000_000);
        sim.run();
        let r = sim.replica(1, 1);
        for e in r.sim.all_entries().into_iter().filter(|e| e.member == 3 && matches!(e.payload, Payload::Touch { .. })) {
            println!(
                "  part {part}: member 3 entry stamped {} -> effective {:?} (void: {})",
                e.stamp,
                r.sim.effective_frame(e.id()),
                r.sim.void_ids().contains(&e.id())
            );
        }
        print_stats(&sim, 1);
        // A demoted member no longer counts: compare the members still in the session.
        let gone: Vec<MemberId> = r.sim.members_evicted().keys().copied().collect();
        if !gone.is_empty() {
            println!("RESULT backdate part {part}: removed from the session (cut, from frame): {:?}", r.sim.members_evicted());
        }
        let frames = sim.peers.iter().filter(|p| !gone.contains(&p.id)).map(|p| p.worlds[&1].sim.frame).min().unwrap_or(0).min(600);
        check_world_except(&sim, 1, frames, &format!("backdate part {part}"), &gone);
    }
}

// ---- R14 -----------------------------------------------------------------------------------------

/// P2-5: a forged arrival from the dishonest-majority world W (6, members 2 and 3), signed by both
/// of W's members, into world `w` by member `by`.
fn forge_from_w(sim: &mut Sim, w: WorldId, by: MemberId, record: ObjRecord, chain: Vec<Hop>) -> Entry {
    let fake = pe_sim_core::splitmix64(record.global ^ record.via.rotate_left(17) ^ ((w as u64) << 48) ^ chain.len() as u64);
    let deadline = sim.replica(by, w).sim.now + 600;
    let fact = fact_departed(6, fake, &record, w, deadline, chain_hash(&chain));
    let cert: Vec<(MemberId, u64)> = [2u32, 3].iter().map(|&m| (m, member_sig(6, m, fact))).collect();
    let i = sim.peers.iter().position(|p| p.id == by).unwrap();
    let e = sim.peers[i].worlds.get_mut(&w).unwrap().sim.author(Payload::Arrive { transfer: transfer_id(6, fake), record, depart: fake, deadline, cert, chain }, None);
    let members: Vec<MemberId> = sim.replica(by, w).sim.members.iter().copied().collect();
    for to in members {
        if to != by {
            sim.net.send(w, by, to, &Msg::Entry(e.clone()));
        }
    }
    e
}

fn r14(variant: &str, reverse: bool, control: bool) {
    if variant == "chain" {
        return r14_chain(reverse);
    }
    if variant == "chain-epoch" {
        return r14_chain_epoch();
    }
    // World 1 (A): 60 Hz pile; world 2 (B): 50 Hz pile. Member 1 is in both and carries object 40
    // from A to B. Member 2 is only in A, member 3 only in B.
    let a_cfg = SimConfig { seed: 0xA, ..cfg60() };
    let b_cfg = SimConfig { seed: 0xB, dt: 1.0 / 50.0, ..cfg60() };
    let a_scene = SceneKind::Pile { side: 6, layers: 3 };
    let b_scene = SceneKind::Pile { side: 5, layers: 3 };
    let grab = |f: u64| (f, Action::Touch { obj: 40, impulse: [0.0, 0.3, 0.0], grab: true });
    // In "abort" the carrier grabs and carries in one motion (no earlier hold), so another member's
    // slightly earlier grab can still win the object.
    let mut p1_a: Vec<(u64, Action)> = if variant == "abort" { vec![] } else { (0..12).map(|k| grab(100 + 5 * k)).collect() };
    p1_a.push((160, Action::Carry { transfer: 7, obj: 40, dest: 2 }));
    // After crossing, member 1 authors in A only the relay's Verdict (state-free, stamped at the
    // present) and, in "ok" and "throttle", one touch at frame 260 (it carries state, so a
    // throttled replica catches up first: design 22.3 rule 2). So "throttle" and "ok" have
    // identical logs and must produce identical histories (review NEW-12).
    if variant == "ok" || variant == "throttle" {
        p1_a.push((260, touch(25, 0.0, 5.0, 0.0)));
    }
    let mut p2_a: Vec<(u64, Action)> = vec![(140, touch(33, 2.0, 4.0, 0.0)), (175, touch(60, 0.0, 6.0, 1.0)), (230, touch(12, 0.0, 5.0, 0.0))];
    if variant == "abort" {
        // Member 2 grabbed object 40 at frame 155 and keeps holding it. Its entries reach member 1
        // 150 ms late (inside the 250 ms late allowance, so they keep their frames), after member
        // 1 already departed speculatively at frame 160: A rolls back past fA.
        p2_a.extend((0..14).map(|k| (155 + 5 * k, Action::Touch { obj: 40, impulse: [0.0, 0.3, 0.0], grab: true })));
    }
    // "doublespend" (P2-5's measured residual): the same worlds as "forge", but W's majority only
    // double-spends A:52, which it legitimately received, into A, which has not seen it since.
    let doublespend = variant == "doublespend";
    let forge = variant == "forge" || doublespend;
    if forge {
        // P2-5: member 2 carries A's objects 52 and 55 to W, a routed world whose majority (members
        // 2 and 3) is dishonest. 52 then goes on to B (member 3) and back to A (member 1); 55 is the
        // object of the refund-and-keep attack.
        p2_a.push((100, Action::Carry { transfer: 52, obj: 52, dest: 6 }));
        if !doublespend {
            p2_a.push((110, Action::Carry { transfer: 55, obj: 55, dest: 6 }));
        }
    }
    let mut p3_b: Vec<(u64, Action)> = vec![(120, touch(30, 0.0, 6.0, 0.0)), (160, touch(8, 3.0, 3.0, 0.0))];
    if variant == "reject" {
        // Member 3 closes B's arrival portal at B frame 134; member 1 hears of it 150 ms late,
        // after it already relayed the arrival: B rolls back past fB.
        p3_b.push((134, Action::Portal { open: false }));
    }
    let mut peers = vec![Peer::new(1, 1200), Peer::new(2, -900), Peer::new(3, 400)];
    // "crash" (reviews NEW-6, R3-7): member 4 is in both worlds too, so A and B have three members
    // each and keep a majority when the carrier crashes.
    let crash = variant == "crash";
    let (a_members, b_members): (Vec<MemberId>, Vec<MemberId>) = if crash { (vec![1, 2, 4], vec![1, 3, 4]) } else { (vec![1, 2], vec![1, 3]) };
    if crash {
        peers.push(Peer::new(4, 300));
    }
    let order: Vec<WorldId> = if reverse { vec![2, 1] } else { vec![1, 2] };
    for w in order {
        if w == 1 {
            peers[0].join(1, &a_members, a_scene, a_cfg, 100_000, plan(&p1_a));
            peers[1].join(1, &a_members, a_scene, a_cfg, 100_000, plan(&p2_a));
        } else {
            peers[0].join(2, &b_members, b_scene, b_cfg, 150_000, BTreeMap::new());
            peers[2].join(2, &b_members, b_scene, b_cfg, 150_000, plan(&p3_b));
        }
    }
    if crash {
        peers[3].join(1, &a_members, a_scene, a_cfg, 100_000, BTreeMap::new());
        peers[3].join(2, &b_members, b_scene, b_cfg, 150_000, BTreeMap::new());
    }
    if forge {
        // Reviews R3-4, R3-5: one-member worlds that a single member controls, D (4, member 2) and
        // E (5, member 3). Their owners route them to A and B; A and B do not route to them.
        let tiny = SceneKind::Pile { side: 3, layers: 1 };
        peers[1].join(4, &[2], tiny, SimConfig { seed: 0xD, ..cfg60() }, 120_000, BTreeMap::new());
        peers[2].join(5, &[3], tiny, SimConfig { seed: 0xE, ..cfg60() }, 120_000, BTreeMap::new());
        peers[1].route(4, &[1]);
        peers[2].route(5, &[2]);
        // P2-5: W (6) is on A's and B's route lists, and members 2 and 3 are its whole membership:
        // a partner world with a dishonest majority. It runs honestly except for the forged entries.
        // A:52 arrives as W's local object 9 (after its 9 bodies), and member 3 carries it on to B.
        let w_cfg = SimConfig { seed: 0x6, ..cfg60() };
        peers[1].join(6, &[2, 3], tiny, w_cfg, 110_000, BTreeMap::new());
        peers[2].join(6, &[2, 3], tiny, w_cfg, 110_000, plan(&[(203, Action::Carry { transfer: 9, obj: 9, dest: 2 })]));
        peers[1].route(6, &[1, 2]);
        peers[2].route(6, &[1, 2]);
    }
    // Route lists (genesis config, D46): A and B trade with each other only (and with W in "forge").
    for p in &mut peers {
        p.route(1, if forge { &[2, 6] } else { &[2] });
        p.route(2, if forge { &[1, 6] } else { &[1] });
    }
    if variant == "throttle" {
        // After the departure, world A becomes a background world for member 1: it advances only
        // every 250 ms, in one burst (design 22.3).
        let r = peers[0].worlds.get_mut(&1).unwrap();
        r.throttle_us = Some(250_000);
        r.throttle_from_us = 3_000_000;
    }
    let mut net = Net::new(link(50, 15, 1), 21);
    let v = variant.to_string();
    net.filter = Some(Box::new(move |t, w, from, to, m| match m {
        Msg::Entry(_) if v == "abort" && w == 1 && from == 2 && to == 1 && t < 4_000_000 => Fate::Delay(150_000),
        Msg::Entry(_) if v == "reject" && w == 2 && from == 3 && to == 1 && t < 4_000_000 => Fate::Delay(150_000),
        _ => Fate::Deliver,
    }));
    let mut sim = Sim::new(net, peers, 9_000_000);
    sim.reverse_order = reverse;
    if control {
        // Control run: arrivals skip the P2-5 provenance checks, to show what the attacks did before.
        for p in &mut sim.peers {
            for r in p.worlds.values_mut() {
                r.sim.provenance = false;
            }
        }
    }
    if variant == "late" {
        // Review R3-6: a deadline 2 frames of B after the departure, shorter than the certificate
        // round: the arrival is late, B refuses it, and that certified refusal returns the object.
        sim.carry_deadline_frames = 2;
    }
    let mut lag_samples: Vec<(u64, u64)> = Vec::new();
    // "forge" (reviews NEW-3..NEW-5): proofless relays by members who cannot have proofs.
    let mut forged: Vec<(WorldId, MemberId, EntryId, &'static str)> = Vec::new();
    // P2-5 attack state: A:55's transfer and escrow, once W's majority has certified a refusal to A.
    let mut refunded: Option<(u64, Escrow)> = None;
    let g = |i: u64| (1u64 << 32) | i;
    let send_all = |sim: &mut Sim, w: WorldId, from: MemberId, e: &Entry| {
        let members: Vec<MemberId> = sim.replica(from, w).sim.members.iter().copied().collect();
        for to in members {
            if to != from {
                sim.net.send(w, from, to, &Msg::Entry(e.clone()));
            }
        }
    };
    while sim.net.now_us <= sim.end_us {
        if crash && sim.net.now_us == 2_850_000 {
            sim.peers[0].crashed = true;
        }
        sim.step_ms();
        if variant == "forge" {
            let t = sim.net.now_us;
            let pidx = |sim: &Sim, id: MemberId| sim.peers.iter().position(|p| p.id == id).unwrap();
            // The carry's escrow while it is pending (the forged departures add rejected escrows, and
            // transfer ids, hence map order, depend on the carried state).
            let esc = sim.replica(2, 1).sim.t.escrow.iter().find(|(_, e)| e.status == EscrowStatus::Pending).map(|(t, e)| (*t, *e));
            if t == 2_000_000 {
                // R3-4: member 2 departs A's object 50 to its own one-member world D, where it could
                // certify anything (accept, then "rejected" for a refund).
                let i = pidx(&sim, 2);
                let e = sim.peers[i].worlds.get_mut(&1).unwrap().sim.author(Payload::Depart { obj: 50, dest: 4, deadline: u64::MAX, hop: None }, None);
                forged.push((1, 2, e.id(), "member 2 departs A's object 50 to D, a world it controls alone"));
                send_all(&mut sim, 1, 2, &e);
                // R3-6: member 2 departs A's object 51 to a world nobody knows, deadline 0.
                let e = sim.peers[i].worlds.get_mut(&1).unwrap().sim.author(Payload::Depart { obj: 51, dest: 999, deadline: 0, hop: None }, None);
                forged.push((1, 2, e.id(), "member 2 departs A's object 51 to unknown world 999"));
                send_all(&mut sim, 1, 2, &e);
            }
            if t == 2_200_000 {
                // R3-5: member 3 certifies, as the whole membership of its world E, a departure that
                // never happened, of a record carrying A's object 40's global id.
                let record = ObjRecord { global: (1u64 << 32) | 40, kind: 1, state: BodyState([0; 13]), src_world: 5, src_frame: 0, via: 0 };
                let fake = 0x1234_5678;
                let fact = fact_departed(5, fake, &record, 2, u64::MAX, chain_hash(&[]));
                let i = pidx(&sim, 3);
                let e = sim.peers[i].worlds.get_mut(&2).unwrap().sim.author(
                    Payload::Arrive { transfer: transfer_id(5, fake), record, depart: fake, deadline: u64::MAX, cert: vec![(3, member_sig(5, 3, fact))], chain: vec![] },
                    None,
                );
                forged.push((2, 3, e.id(), "member 3 mints A's object 40 into B from its own world E"));
                send_all(&mut sim, 2, 3, &e);
            }
            if t == 2_500_000 {
                // NEW-4: member 3 (only in B) mints object 40 into B with no departure behind it.
                let record = ObjRecord { global: (1u64 << 32) | 40, kind: 1, state: BodyState([0; 13]), src_world: 1, src_frame: 100, via: 0 };
                let fake = 0xDEAD_BEEF;
                let fact = fact_departed(1, fake, &record, 2, u64::MAX, chain_hash(&[]));
                let i = pidx(&sim, 3);
                let e = sim.peers[i].worlds.get_mut(&2).unwrap().sim.author(
                    Payload::Arrive { transfer: transfer_id(1, fake), record, depart: fake, deadline: u64::MAX, cert: vec![(3, member_sig(1, 3, fact))], chain: vec![] },
                    None,
                );
                forged.push((2, 3, e.id(), "member 3 mints object 40 into B (no departure; signs as itself)"));
                send_all(&mut sim, 2, 3, &e);
            }
            if t == 2_900_000 {
                if let Some((transfer, x)) = esc {
                    // NEW-5: member 3 squats the real transfer id before the carrier relays.
                    let i = pidx(&sim, 3);
                    let e = sim.peers[i].worlds.get_mut(&2).unwrap().sim.author(
                        Payload::Arrive { transfer, record: x.record.unwrap(), depart: x.depart, deadline: x.deadline, cert: vec![], chain: vec![] },
                        None,
                    );
                    forged.push((2, 3, e.id(), "member 3 squats the real transfer id with a proofless Arrive"));
                    send_all(&mut sim, 2, 3, &e);
                }
            }
            if t == 2_700_000 {
                // NEW-21: a certified but invalid record (a bug, or a compromised majority, in A): the
                // signatures are genuine A signatures over a record with a NaN position.
                let mut st = BodyState([0; 13]);
                st.0[0] = f32::NAN.to_bits();
                st.0[6] = 1.0f32.to_bits();
                let record = ObjRecord { global: (1u64 << 32) | 41, kind: 1, state: st, src_world: 1, src_frame: 150, via: 0 };
                let fake = 0x0BAD_0BAD;
                let fact = fact_departed(1, fake, &record, 2, u64::MAX, chain_hash(&[]));
                let cert: Vec<(MemberId, u64)> = [1u32, 2].iter().map(|&m| (m, member_sig(1, m, fact))).collect();
                let i = pidx(&sim, 1);
                let e = sim.peers[i].worlds.get_mut(&2).unwrap().sim.author(
                    Payload::Arrive { transfer: transfer_id(1, fake), record, depart: fake, deadline: u64::MAX, cert, chain: vec![] },
                    None,
                );
                forged.push((2, 1, e.id(), "a certified record with a NaN position (a bug or a compromised majority in A)"));
                send_all(&mut sim, 2, 1, &e);
            }
            if t == 3_000_000 {
                if let Some((transfer, _)) = esc {
                    // NEW-3 (loss): member 2 (only in A) commits the escrow with a proofless verdict.
                    let i = pidx(&sim, 2);
                    let e = sim.peers[i].worlds.get_mut(&1).unwrap().sim.author(Payload::Verdict { transfer, accepted: true, cert: vec![] }, None);
                    forged.push((1, 2, e.id(), "member 2 commits the escrow with a proofless Verdict"));
                    send_all(&mut sim, 1, 2, &e);
                }
            }
            if t == 3_020_000 {
                if let Some((transfer, x)) = esc {
                    // R3-8: the carrier relays A's genuine certificate with the record's kind changed.
                    let rec = x.record.unwrap();
                    let fact = fact_departed(1, x.depart, &rec, 2, x.deadline, chain_hash(&[]));
                    let cert = sim.replica(1, 1).sim.cert(fact);
                    if let Some(cert) = cert {
                        let bad = ObjRecord { kind: (rec.kind + 1) % 3, ..rec };
                        let i = pidx(&sim, 1);
                        let e = sim.peers[i].worlds.get_mut(&2).unwrap().sim.author(
                            Payload::Arrive { transfer, record: bad, depart: x.depart, deadline: x.deadline, cert, chain: vec![] },
                            None,
                        );
                        forged.push((2, 1, e.id(), "the carrier relays A's genuine certificate with the record's kind changed"));
                        send_all(&mut sim, 2, 1, &e);
                    }
                }
            }
            if t == 3_050_000 {
                if let Some((transfer, _)) = esc {
                    // NEW-3 (duplication): before the real verdict lands, the carrier claims B
                    // rejected the object, presenting B's genuine signatures over the acceptance.
                    let fact = fact_verdict(2, transfer, true);
                    let cert: Vec<(MemberId, u64)> = [1u32, 3].iter().map(|&m| (m, member_sig(2, m, fact))).collect();
                    let i = pidx(&sim, 1);
                    let e = sim.peers[i].worlds.get_mut(&1).unwrap().sim.author(Payload::Verdict { transfer, accepted: false, cert }, None);
                    forged.push((1, 1, e.id(), "the carrier claims a rejection with B's signatures over the acceptance"));
                    send_all(&mut sim, 1, 1, &e);
                }
            }
        }
        if forge {
            // ---- P2-5: dishonest-majority attacks by W (members 2 and 3 sign as W) ----
            let t = sim.net.now_us;
            // A sane body state (at rest, 2 m up, unit rotation), so only provenance can refuse it.
            let mut st = BodyState([0; 13]);
            st.0[1] = 2.0f32.to_bits();
            st.0[6] = 1.0f32.to_bits();
            let rec = |global: u64, via: u64| ObjRecord { global, kind: 1, state: st, src_world: 6, src_frame: 50, via };
            if doublespend {
                // A:52 went A -> W -> B. W's majority exports it again, into A, citing its genuine hop
                // in: A's own last exit is on the chain, so A cannot tell.
                let a = &sim.replica(1, 1).sim;
                let in_b = sim.replica(3, 2).sim.t.objs.iter().flatten().any(|o| o.global == g(52));
                if let (4_600_000, true, Some(Seen::Left { transfer: t1, dest: 6, .. })) = (t, in_b, a.t.seen.get(&g(52)).cloned()) {
                    let cert = a.cert(fact_committed(1, t1, g(52), 6, 0)).unwrap();
                    let hop = Hop { src: 1, dest: 6, transfer: t1, via: 0, epoch: 0, frame: 0, certs: vec![], cert };
                    let e = forge_from_w(&mut sim, 1, 2, rec(g(52), t1), vec![hop]);
                    forged.push((1, 2, e.id(), "RESIDUAL: W's majority double-spends A:52 (now in B) into A, citing its genuine hop in"));
                }
            }
            if t == 3_300_000 && !doublespend {
                let e = forge_from_w(&mut sim, 2, 3, rec(g(53), 0), vec![]);
                forged.push((2, 3, e.id(), "W's majority mints A:53 (live in A) into B, as if born in W"));
                let e = forge_from_w(&mut sim, 2, 3, rec((6u64 << 32) | 500, 0), vec![]);
                forged.push((2, 3, e.id(), "W's majority mints W:500, an id W was never born with, into B"));
            }
            if t == 3_400_000 && !doublespend {
                // A hop A -> W that A never certified: the attackers hold only member 2's key of A.
                let fake = 0xF0F0;
                let fact = fact_committed(1, fake, g(54), 6, 0);
                let hop = Hop { src: 1, dest: 6, transfer: fake, via: 0, epoch: 0, frame: 0, certs: vec![], cert: vec![(2, member_sig(1, 2, fact))] };
                let e = forge_from_w(&mut sim, 2, 3, rec(g(54), fake), vec![hop]);
                forged.push((2, 3, e.id(), "W's majority forges an earlier hop (A -> W of A:54, signed by m2 of A only)"));
            }
            if t == 4_000_000 && !doublespend {
                // A's genuine commit certificate of A:52's hop into W, spliced onto A:54.
                let a = &sim.replica(1, 1).sim;
                if let Some(Seen::Left { transfer, dest: 6, via: 0, .. }) = a.t.seen.get(&g(52)).cloned() {
                    if let Some(cert) = a.cert(fact_committed(1, transfer, g(52), 6, 0)) {
                        let hop = Hop { src: 1, dest: 6, transfer, via: 0, epoch: 0, frame: 0, certs: vec![], cert };
                        let e = forge_from_w(&mut sim, 2, 3, rec(g(54), transfer), vec![hop]);
                        forged.push((2, 3, e.id(), "W's majority splices A's genuine hop of A:52 onto A:54"));
                    }
                }
            }
            if refunded.is_none() && !doublespend {
                // Refund and keep: once A has certified A:55's departure to W (so W's replicas will
                // accept it), W's majority also certifies a refusal to A, which refunds it.
                let a = &sim.replica(2, 1).sim;
                let esc = a.t.escrow.iter().find(|(_, e)| e.dest == 6 && e.status == EscrowStatus::Pending && e.record.is_some_and(|r| r.global == g(55)));
                if let Some((transfer, x)) = esc.map(|(t, e)| (*t, *e)) {
                    let r = x.record.unwrap();
                    if a.cert(fact_departed(1, x.depart, &r, 6, x.deadline, chain_hash(&[]))).is_some() {
                        let fact = fact_verdict(6, transfer, false);
                        let wcert: Vec<(MemberId, u64)> = [2u32, 3].iter().map(|&m| (m, member_sig(6, m, fact))).collect();
                        let i = sim.peers.iter().position(|p| p.id == 2).unwrap();
                        let e = sim.peers[i].worlds.get_mut(&1).unwrap().sim.author(Payload::Verdict { transfer, accepted: false, cert: wcert }, None);
                        println!("  setup t={} ms: W's majority certifies to A a refusal of A:55 (transfer {transfer}), while W's replicas accept it", t / 1000);
                        send_all(&mut sim, 1, 2, &e);
                        refunded = Some((transfer, x));
                    }
                }
            }
            if let (Some((transfer, x)), 4_500_000) = (refunded, t) {
                // ... then W exports A:55 to B, presenting A's departure certificate (all A ever
                // signed of this hop) in place of a commit certificate.
                let r = x.record.unwrap();
                let cert = sim.replica(2, 1).sim.cert(fact_departed(1, x.depart, &r, 6, x.deadline, chain_hash(&[]))).unwrap();
                let hop = Hop { src: 1, dest: 6, transfer, via: 0, epoch: 0, frame: 0, certs: vec![], cert };
                let e = forge_from_w(&mut sim, 2, 3, rec(g(55), transfer), vec![hop]);
                forged.push((2, 3, e.id(), "refund and keep: W exports A:55 (refunded to A) into B with A's departure certificate"));
            }
            if t == 5_000_000 {
                // Member 1 carries A:52 from B back to A (found by global id: in the control run, the
                // accepted forgeries shift B's local ids). Member 1 is in both, so it relays.
                let i = sim.peers.iter().position(|p| p.id == 1).unwrap();
                let r = sim.peers[i].worlds.get_mut(&2).unwrap();
                let obj = r.sim.t.objs.iter().position(|o| o.is_some_and(|o| o.global == g(52))).map(|i| i as u32);
                if let Some(obj) = obj {
                    let hop = r.sim.pending_hop(obj);
                    let deadline = sim.replica(1, 1).sim.now + 600;
                    let r = sim.peers[i].worlds.get_mut(&2).unwrap();
                    let e = r.sim.author(Payload::Depart { obj, dest: 1, deadline, hop }, None);
                    println!("  setup t={} ms: member 1 departs A:52 (B's local {obj}) from B back to A, hop in carried: {}", t / 1000, matches!(&e.payload, Payload::Depart { hop: Some(_), .. }));
                    send_all(&mut sim, 2, 1, &e);
                }
            }
            if t == 7_000_000 && !doublespend {
                // Resurrection: A:52 went A -> W -> B -> A. W re-exports it, citing its genuine hop in.
                let a = &sim.replica(1, 1).sim;
                let back = a.t.objs.iter().flatten().any(|o| o.global == g(52)) && matches!(a.t.seen.get(&g(52)), Some(Seen::Here { from: 2, .. }));
                let first = sim.peers.iter().flat_map(|p| p.jobs.iter()).find(|j| j.src == 1 && j.dst == 6 && j.record.is_some_and(|r| r.global == g(52))).map(|j| j.transfer);
                if let (true, Some(t1)) = (back, first) {
                    let cert = a.cert(fact_committed(1, t1, g(52), 6, 0)).unwrap();
                    let hop = Hop { src: 1, dest: 6, transfer: t1, via: 0, epoch: 0, frame: 0, certs: vec![], cert };
                    let e = forge_from_w(&mut sim, 2, 3, rec(g(52), t1), vec![hop.clone()]);
                    forged.push((2, 3, e.id(), "W's majority resurrects A:52 into B, which it has left for A since"));
                    let e = forge_from_w(&mut sim, 1, 2, rec(g(52), t1), vec![hop]);
                    forged.push((1, 2, e.id(), "W's majority resurrects A:52 into A, where it is live again"));
                }
            }
        }
        if sim.net.now_us % 100_000 == 0 && sim.net.now_us > 1_000_000 {
            let r = sim.replica(2, 1);
            lag_samples.push((sim.net.now_us / 1000, r.sim.now.saturating_sub(r.sim.conf)));
        }
        sim.net.now_us += 1000;
    }
    for (w, by, id, what) in &forged {
        // An honest member of that world other than the author.
        let witness = match (*w, *by) {
            (1, 2) => 1,
            (1, _) => 2,
            (_, 3) => 1,
            _ => 3,
        };
        let o = sim.replica(witness, *w).sim.outcomes.get(id).copied();
        println!("RESULT   forged by m{by} in world {w}: {what} -> {:?}", o.map(|x| x.1));
    }
    for j in &sim.peers[0].jobs {
        for l in &j.log {
            println!("  carrier: {l}");
        }
        println!("  carrier: phase {:?} accepted {:?}", j.phase, j.accepted);
        let a = &sim.replica(1, 1).sim;
        if let Some((f, o)) = a.outcomes.get(&j.depart) {
            println!("  carrier: depart entry final outcome at A frame {f}: {o:?}");
        }
    }
    for p in sim.peers.iter().skip(1) {
        for j in &p.jobs {
            for l in &j.log {
                println!("  relayer m{}: {l}", p.id);
            }
            println!("  relayer m{}: phase {:?} accepted {:?}", p.id, j.phase, j.accepted);
        }
    }
    if crash {
        for (w, at) in [(1, 2), (2, 3)] {
            let r = &sim.replica(at, w).sim;
            println!(
                "RESULT r14 crash: world {w} at m{at}: members {:?} evicted {:?} conf {} frame {} paused frames {}",
                r.members,
                r.members_evicted(),
                r.conf,
                r.frame,
                r.stats.paused_frames
            );
        }
    }
    // Conservation: global id of object 40 in A.
    let gid = (1u64 << 32) | 40;
    for (w, label) in [(1, "A"), (2, "B")] {
        for p in &sim.peers {
            if let Some(r) = p.worlds.get(&w) {
                let live = r.sim.t.objs.iter().flatten().filter(|o| o.global == gid).count();
                let esc: Vec<String> = r.sim.t.escrow.iter().map(|(t, e)| format!("T{t}:{:?}", e.status)).collect();
                let arr: Vec<String> = r.sim.t.arrivals.iter().map(|(t, a)| format!("T{t}:{a:?}")).collect();
                println!("  m{} world {label}: object 40 live copies {live}; escrow {esc:?}; arrivals {arr:?}", p.id);
            }
        }
    }
    let live_a = sim.replica(2, 1).sim.t.objs.iter().flatten().filter(|o| o.global == gid).count();
    let live_b = sim.replica(3, 2).sim.t.objs.iter().flatten().filter(|o| o.global == gid).count();
    let pending = sim.replica(2, 1).sim.t.escrow.values().filter(|e| e.status == EscrowStatus::Pending).count();
    println!(
        "RESULT r14 {variant}{}: object 40 live in A {live_a}, in B {live_b}, pending escrow {pending} -> {}",
        if reverse { " (reverse order)" } else { "" },
        if live_a + live_b == 1 && pending == 0 { "CONSERVED (exactly one live copy, nothing in limbo)" } else { "NOT CONSERVED" }
    );
    if forge {
        let live = |m: MemberId, w: WorldId, g: u64| sim.replica(m, w).sim.t.objs.iter().flatten().filter(|o| o.global == g).count();
        let (g50, g51) = ((1u64 << 32) | 50, (1u64 << 32) | 51);
        let (a50, a51, d50, b40) = (live(1, 1, g50), live(1, 1, g51), live(2, 4, g50), live(3, 2, gid));
        if variant == "forge" {
            println!(
                "RESULT r14 forge: A's objects 50 and 51 live in A {a50} and {a51}, object 50 in D {d50}; copies of A:40 in B {b40} -> {}",
                if a50 == 1 && a51 == 1 && d50 == 0 && b40 <= 1 { "CONSERVED" } else { "NOT CONSERVED" }
            );
        }
        // Every attack must have been tried: one whose moment passed (the carry resolved earlier)
        // would otherwise drop out silently.
        if variant == "forge" && forged.len() != 16 {
            println!("RESULT r14 forge: FAIL: {} of 16 forged entries authored", forged.len());
        }
        // P2-5: copies in the honest worlds A and B (W's own replicas are its majority's word).
        let pending_of = |m: MemberId, w: WorldId, g: u64| {
            sim.replica(m, w).sim.t.escrow.values().filter(|e| e.status == EscrowStatus::Pending && e.record.is_some_and(|r| r.global == g)).count()
        };
        let mut ok = true;
        let mut parts = Vec::new();
        for (name, g, want) in [("A:52", (1u64 << 32) | 52, 1), ("A:53", (1 << 32) | 53, 1), ("A:54", (1 << 32) | 54, 1), ("A:55", (1 << 32) | 55, 1), ("W:500", (6 << 32) | 500, 0)] {
            let (a, b, w) = (live(1, 1, g), live(3, 2, g), live(2, 6, g));
            let p = pending_of(1, 1, g) + pending_of(3, 2, g);
            ok &= a + b == want && p == 0;
            parts.push(format!("{name} A {a} B {b} pending {p} (W {w})"));
        }
        println!(
            "RESULT r14 {variant}: dishonest-majority W ({}): {} -> {}",
            if control { "CONTROL, provenance off" } else { "provenance on" },
            parts.join("; "),
            if ok { "CONSERVED in the honest worlds" } else { "NOT CONSERVED in the honest worlds" }
        );
        for (m, w) in [(1, 1), (1, 2)] {
            let r = &sim.replica(m, w).sim;
            for p in r.fork_proofs.values() {
                // P25-3: the proof carries both certificates, so a third party checks it on its own.
                println!(
                    "RESULT r14 {variant}: world {w} holds a fork proof against world {}: it exported {:#x} twice from its entry {} (transfers {} and {}); verifies for a third party: {}",
                    p.world, p.global, p.via, p.a.transfer(), p.b.transfer(), p.verify(&r.epochs)
                );
            }
        }
        print_stats(&sim, 6);
    }
    print_stats(&sim, 1);
    print_stats(&sim, 2);
    // The crashed carrier's own replicas are not compared.
    let skip: &[MemberId] = if crash { &[1] } else { &[] };
    check_world_except(&sim, 1, 500, &format!("r14 {variant} A"), skip);
    check_world_except(&sim, 2, 400, &format!("r14 {variant} B"), skip);
    if forge {
        check_world_except(&sim, 6, 480, &format!("r14 {variant} W"), skip);
    }
    if variant == "throttle" || variant == "ok" {
        let before: Vec<u64> = lag_samples.iter().filter(|(t, _)| (1_500..3_000).contains(t)).map(|x| x.1).collect();
        let after: Vec<u64> = lag_samples.iter().filter(|(t, _)| (3_500..8_500).contains(t)).map(|x| x.1).collect();
        let avg = |v: &Vec<u64>| v.iter().sum::<u64>() as f64 / v.len().max(1) as f64;
        println!(
            "RESULT r14 {variant}: member 2's confirmation lag in A (present - confirmed, frames): avg {:.1} before t=3 s, {:.1} after (member 1 {} from t=3 s)",
            avg(&before),
            avg(&after),
            if variant == "throttle" { "throttled to one burst per 250 ms" } else { "not throttled" }
        );
        // Review NEW-12: the background member's own entries, and how late its check hashes come.
        let a2 = &sim.replica(2, 1).sim;
        for e in a2.all_entries() {
            if let (1, Payload::Touch { obj: 25, .. }) = (e.member, &e.payload) {
                println!(
                    "RESULT r14 {variant}: member 1's touch in A after t=3 s: stamp {}, outcome at member 2 {:?}",
                    e.stamp,
                    a2.outcomes.get(&e.id()).map(|x| x.1)
                );
            }
        }
        let delays = |m: MemberId| -> Vec<u64> {
            let mut first: BTreeMap<u64, u64> = BTreeMap::new();
            for e in a2.all_entries() {
                if let Payload::Ack { check: Some((f, _)), .. } = e.payload {
                    if e.member == m && f >= 180 {
                        let s = first.entry(f).or_insert(e.stamp);
                        *s = (*s).min(e.stamp);
                    }
                }
            }
            first.iter().map(|(f, s)| s - f).collect()
        };
        let (d1, d2) = (delays(1), delays(2));
        let fmt = |v: &Vec<u64>| format!("avg {:.1}, max {} over {} check frames", avg(v), v.iter().max().unwrap_or(&0), v.len());
        println!(
            "RESULT r14 {variant}: check hashes in A from frame 180 (frames from check frame to the first ack carrying it): member 1 {}; member 2 {}",
            fmt(&d1),
            fmt(&d2)
        );
    }
}

/// P2-5: A -> B -> C -> A. Worlds A (60 Hz, members 1 and 2), B (50 Hz, 1 and 3) and C (60 Hz, 1
/// and 4) route to each other. Member 1 is in all three and carries A's object 40 round the loop;
/// each arrival checks the chain back to A, and the return into A passes A's own last exit.
fn r14_chain(reverse: bool) {
    let a_cfg = SimConfig { seed: 0xA, ..cfg60() };
    let b_cfg = SimConfig { seed: 0xB, dt: 1.0 / 50.0, ..cfg60() };
    let c_cfg = SimConfig { seed: 0xC, ..cfg60() };
    let (a_scene, b_scene, c_scene) = (SceneKind::Pile { side: 6, layers: 3 }, SceneKind::Pile { side: 5, layers: 3 }, SceneKind::Pile { side: 4, layers: 3 });
    // Local ids: A:40 arrives in B as 75 (B has 75 bodies) and in C as 48 (C has 48).
    let mut p1_a: Vec<(u64, Action)> = (0..12).map(|k| (100 + 5 * k, Action::Touch { obj: 40, impulse: [0.0, 0.3, 0.0], grab: true })).collect();
    p1_a.push((160, Action::Carry { transfer: 1, obj: 40, dest: 2 }));
    let p1_b = vec![(217, Action::Carry { transfer: 2, obj: 75, dest: 3 })];
    let p1_c = vec![(370, Action::Carry { transfer: 3, obj: 48, dest: 1 })];
    let p2_a = vec![(140, touch(33, 2.0, 4.0, 0.0)), (230, touch(12, 0.0, 5.0, 0.0))];
    let p3_b = vec![(120, touch(30, 0.0, 6.0, 0.0)), (260, touch(8, 3.0, 3.0, 0.0))];
    let p4_c = vec![(150, touch(20, 0.0, 6.0, 0.0)), (400, touch(5, 2.0, 5.0, 0.0))];
    let mut peers = vec![Peer::new(1, 1200), Peer::new(2, -900), Peer::new(3, 400), Peer::new(4, -300)];
    let order: Vec<WorldId> = if reverse { vec![3, 2, 1] } else { vec![1, 2, 3] };
    for w in order {
        match w {
            1 => {
                peers[0].join(1, &[1, 2], a_scene, a_cfg, 100_000, plan(&p1_a));
                peers[1].join(1, &[1, 2], a_scene, a_cfg, 100_000, plan(&p2_a));
            }
            2 => {
                peers[0].join(2, &[1, 3], b_scene, b_cfg, 150_000, plan(&p1_b));
                peers[2].join(2, &[1, 3], b_scene, b_cfg, 150_000, plan(&p3_b));
            }
            _ => {
                peers[0].join(3, &[1, 4], c_scene, c_cfg, 130_000, plan(&p1_c));
                peers[3].join(3, &[1, 4], c_scene, c_cfg, 130_000, plan(&p4_c));
            }
        }
    }
    for p in &mut peers {
        p.route(1, &[2, 3]);
        p.route(2, &[1, 3]);
        p.route(3, &[1, 2]);
    }
    let mut sim = Sim::new(Net::new(link(50, 15, 1), 23), peers, 9_000_000);
    sim.reverse_order = reverse;
    sim.run();
    for p in &sim.peers {
        for j in &p.jobs {
            for l in &j.log {
                println!("  m{}: {l}", p.id);
            }
            println!("  m{}: transfer {} world {} -> {}: phase {:?} accepted {:?}", p.id, j.transfer, j.src, j.dst, j.phase, j.accepted);
        }
    }
    let gid = (1u64 << 32) | 40;
    // Each departure's final outcome, at the carrier.
    for (w, label) in [(1, "A"), (2, "B"), (3, "C")] {
        let r = &sim.replica(1, w).sim;
        for e in r.all_entries() {
            if let Payload::Depart { obj, dest, hop, .. } = &e.payload {
                println!(
                    "RESULT r14 chain: departure of local {obj} from {label} to world {dest} (hop in carried: {}) -> {:?}",
                    hop.is_some(),
                    r.outcomes.get(&e.id()).map(|x| x.1)
                );
            }
            if let Payload::Arrive { chain, .. } = &e.payload {
                let route: Vec<String> = chain.iter().map(|h| format!("{}->{}", h.src, h.dest)).collect();
                println!("RESULT r14 chain: arrival into {label} with a chain of {} hops [{}] -> {:?}", chain.len(), route.join(", "), r.outcomes.get(&e.id()).map(|x| x.1));
            }
        }
    }
    let mut total = 0;
    let mut pending = 0;
    let mut parts = Vec::new();
    for (w, label, m) in [(1, "A", 2), (2, "B", 3), (3, "C", 4)] {
        let r = &sim.replica(m, w).sim;
        let live = r.t.objs.iter().flatten().filter(|o| o.global == gid).count();
        pending += r.t.escrow.values().filter(|e| e.status == EscrowStatus::Pending).count();
        total += live;
        parts.push(format!("{label} {live} ({:?})", r.t.seen.get(&gid).map(|s| match s {
            Seen::Here { chain, settled, .. } => format!("here, chain {} hops, settled {settled}", chain.len()),
            Seen::Left { dest, .. } => format!("left for {dest}"),
        })));
    }
    println!(
        "RESULT r14 chain{}: A:40 live copies {} ; pending escrows {pending} -> {}",
        if reverse { " (reverse order)" } else { "" },
        parts.join(", "),
        if total == 1 && pending == 0 && sim.replica(2, 1).sim.t.objs.iter().flatten().any(|o| o.global == gid) {
            "CONSERVED (back in A, nothing in limbo)"
        } else {
            "NOT CONSERVED"
        }
    );
    for w in [1, 2, 3] {
        print_stats(&sim, w);
    }
    check_world(&sim, 1, 500, "r14 chain A");
    check_world(&sim, 2, 400, "r14 chain B");
    check_world(&sim, 3, 480, "r14 chain C");
}

/// P25-5: provenance across a membership change. A has three members (1, 2, 5); member 5 goes silent
/// at t = 1 s and A removes it (epoch 1), so the A -> B commit is signed by A's epoch 1. C routes only
/// to B, so A is not on C's route list: C checks the A -> B hop of the object's chain against the
/// epoch certificate the hop carries, from A's genesis.
fn r14_chain_epoch() {
    let a_cfg = SimConfig { seed: 0xA, ..cfg60() };
    let b_cfg = SimConfig { seed: 0xB, dt: 1.0 / 50.0, ..cfg60() };
    let c_cfg = SimConfig { seed: 0xC, ..cfg60() };
    let (a_scene, b_scene, c_scene) = (SceneKind::Pile { side: 6, layers: 3 }, SceneKind::Pile { side: 5, layers: 3 }, SceneKind::Pile { side: 4, layers: 3 });
    // A:40 arrives in B as local 75 and in C as local 48. A's commit comes after the removal (t about 4.2 s).
    let mut p1_a: Vec<(u64, Action)> = (0..12).map(|k| (240 + 5 * k, Action::Touch { obj: 40, impulse: [0.0, 0.3, 0.0], grab: true })).collect();
    p1_a.push((300, Action::Carry { transfer: 1, obj: 40, dest: 2 }));
    let p1_b = vec![(400, Action::Carry { transfer: 2, obj: 75, dest: 3 })];
    let p2_a = vec![(140, touch(33, 2.0, 4.0, 0.0)), (330, touch(12, 0.0, 5.0, 0.0))];
    let p3_b = vec![(120, touch(30, 0.0, 6.0, 0.0)), (260, touch(8, 3.0, 3.0, 0.0))];
    let p4_c = vec![(150, touch(20, 0.0, 6.0, 0.0)), (600, touch(5, 2.0, 5.0, 0.0))];
    let mut peers = vec![Peer::new(1, 1200), Peer::new(2, -900), Peer::new(3, 400), Peer::new(4, -300), Peer::new(5, 200)];
    peers[0].join(1, &[1, 2, 5], a_scene, a_cfg, 100_000, plan(&p1_a));
    peers[1].join(1, &[1, 2, 5], a_scene, a_cfg, 100_000, plan(&p2_a));
    peers[4].join(1, &[1, 2, 5], a_scene, a_cfg, 100_000, BTreeMap::new());
    peers[0].join(2, &[1, 3], b_scene, b_cfg, 150_000, plan(&p1_b));
    peers[2].join(2, &[1, 3], b_scene, b_cfg, 150_000, plan(&p3_b));
    peers[0].join(3, &[1, 4], c_scene, c_cfg, 130_000, BTreeMap::new());
    peers[3].join(3, &[1, 4], c_scene, c_cfg, 130_000, plan(&p4_c));
    for p in &mut peers {
        p.route(1, &[2]);
        p.route(2, &[1, 3]);
        p.route(3, &[2]);
    }
    let mut net = Net::new(link(50, 15, 1), 23);
    net.filter = Some(Box::new(|t, _w, from, to, _m| if (from == 5 || to == 5) && t >= 1_000_000 { Fate::Drop } else { Fate::Deliver }));
    let mut sim = Sim::new(net, peers, 12_000_000);
    sim.run();
    for p in &sim.peers {
        for j in &p.jobs {
            for l in &j.log {
                println!("  m{}: {l}", p.id);
            }
        }
    }
    let a = &sim.replica(2, 1).sim;
    println!("RESULT r14 chain-epoch: A at m2: epoch {} members {:?} decisions {:?}", a.epoch, a.members, a.vc_log);
    let gid = (1u64 << 32) | 40;
    for (w, label, m) in [(2, "B", 3), (3, "C", 4)] {
        let r = &sim.replica(m, w).sim;
        for e in r.all_entries() {
            if let Payload::Provenance { hop } = &e.payload {
                println!("RESULT r14 chain-epoch: hop {}->{} relayed into {label}: signed by epoch {} at frame {}, carrying {} epoch certificate(s)", hop.src, hop.dest, hop.epoch, hop.frame, hop.certs.len());
            }
            if let Payload::Arrive { chain, .. } = &e.payload {
                let route: Vec<String> = chain.iter().map(|h| format!("{}->{} (epoch {}, {} certs)", h.src, h.dest, h.epoch, h.certs.len())).collect();
                println!("RESULT r14 chain-epoch: arrival into {label} with chain [{}] -> {:?}", route.join(", "), r.outcomes.get(&e.id()).map(|x| x.1));
            }
        }
    }
    let mut total = 0;
    let mut pending = 0;
    let mut parts = Vec::new();
    for (w, label, m) in [(1, "A", 2), (2, "B", 3), (3, "C", 4)] {
        let r = &sim.replica(m, w).sim;
        let live = r.t.objs.iter().flatten().filter(|o| o.global == gid).count();
        pending += r.t.escrow.values().filter(|e| e.status == EscrowStatus::Pending).count();
        total += live;
        parts.push(format!("{label} {live}"));
    }
    let in_c = sim.replica(4, 3).sim.t.objs.iter().flatten().any(|o| o.global == gid);
    println!(
        "RESULT r14 chain-epoch: A:40 live copies {} ; pending escrows {pending} -> {}",
        parts.join(", "),
        if total == 1 && pending == 0 && in_c { "CONSERVED (in C, nothing in limbo)" } else { "NOT CONSERVED" }
    );
    for w in [1, 2, 3] {
        print_stats(&sim, w);
    }
    check_world_except(&sim, 1, 600, "r14 chain-epoch A", &[5]);
    check_world(&sim, 2, 500, "r14 chain-epoch B");
    check_world(&sim, 3, 600, "r14 chain-epoch C");
}

// ---- ball pit ------------------------------------------------------------------------------------

fn ballpit(hz: u64, ahead: u64, hands: usize, loss: u32) {
    let dt = 1.0 / hz as f32;
    let cfg = SimConfig { dt, window: 2 * hz, ..cfg60() };
    let scene = SceneKind::BallPit { balls: 500, hands: 2 };
    let f = |s: f64| (s * hz as f64).round() as u64; // seconds -> frames
    let sweep = |member: MemberId| ballpit_sweep(hz, member, ahead);
    let ids = [1, 2];
    let peers: Vec<Peer> = ids
        .iter()
        .map(|&id| {
            let mut p = Peer::new(id, if id == 1 { 1000 } else { -1000 });
            let pl = if id == 1 || hands == 2 { plan(&sweep(id)) } else { BTreeMap::new() };
            p.join(1, &ids, scene, cfg, 100_000, pl);
            p
        })
        .collect();
    let mut net = Net::new(link(60, 10, loss), 17);
    // Measure the sweep window only.
    let (w0, w1) = (100_000 + 2_000_000, 100_000 + 5_660_000);
    net.count_from_us = w0;
    net.count_to_us = w1;
    let end_us = 9_000_000;
    let mut sim = Sim::new(net, peers, end_us);
    // Naive state streaming, measured on member 1: every frame, the state of each ball whose
    // state changed that frame (what "send the resulting state of every touched object" costs).
    let mut prev: Vec<Option<BodyState>> = vec![None; 500];
    let mut naive_changed: u64 = 0;
    let mut naive_frames: u64 = 0;
    let (mut base_changed, mut base_frames) = (0u64, 0u64);
    let (b0, b1) = (100_000 + 1_000_000, 100_000 + 1_900_000);
    let mut last_frame = 0;
    let mut r2_before = (0u64, 0u64, 0u64);
    let mut r2_after = (0u64, 0u64, 0u64);
    while sim.net.now_us <= sim.end_us {
        sim.step_ms();
        let now = sim.net.now_us;
        let r = sim.replica(1, 1);
        if r.sim.frame != last_frame && r.sim.caught_up() {
            last_frame = r.sim.frame;
            let mut changed = 0;
            for (i, p) in prev.iter_mut().enumerate() {
                let s = r.sim.obj_state(i as u32);
                if s != *p {
                    changed += 1;
                }
                *p = s;
            }
            if (w0..w1).contains(&now) {
                naive_changed += changed;
                naive_frames += 1;
            }
            if (b0..b1).contains(&now) {
                base_changed += changed;
                base_frames += 1;
            }
        }
        let s2 = sim.replica(2, 1).sim.stats;
        if now == w0 {
            r2_before = (s2.rollbacks, s2.resim_frames, s2.resim_us);
        }
        if now == w1 {
            r2_after = (s2.rollbacks, s2.resim_frames, s2.resim_us);
        }
        sim.net.now_us += 1000;
    }
    let secs = (w1 - w0) as f64 / 1e6;
    let (hn, hb) = sim.net.bytes(1, "entry/hand");
    let (an, ab) = sim.net.bytes(1, "ack");
    let (en, eb) = sim.net.bytes(1, "");
    // Wire estimate: one datagram per message with ~52 B of IP/UDP/AEAD/session overhead.
    let wire = |n: u64, b: u64| (b + 52 * n) as f64 / secs;
    let naive_payload = naive_changed * (4 + 52) + naive_frames * 16;
    let naive_quant = naive_changed * (2 + 24) + naive_frames * 16;
    println!(
        "RESULT ballpit {hz} Hz, {hands} hand(s), input delay {ahead} frames, loss {loss}%: sweep window {secs:.2} s, {naive_frames} frames"
    );
    println!(
        "RESULT   perturbation source (hand target per frame + acks): hand entries {hn} ({:.0} B/s payload, {:.0} B/s wire est.), acks {an} ({:.0} B/s wire est.), all messages {:.0} B/s wire est. (both directions)",
        hb as f64 / secs, wire(hn, hb), wire(an, ab), wire(en, eb)
    );
    println!(
        "RESULT   naive state streaming measured on member 1: avg {:.0} balls changed per frame; exact state {:.0} B/s, quantized ~26 B/ball {:.0} B/s (one direction, payload only)",
        naive_changed as f64 / naive_frames.max(1) as f64, naive_payload as f64 / secs, naive_quant as f64 / secs
    );
    println!(
        "RESULT   before the sweep (settled pit, 1.0-1.9 s): avg {:.1} balls changed per frame",
        base_changed as f64 / base_frames.max(1) as f64
    );
    for ((_, k), (n, b)) in sim.net.sent.iter() {
        println!("  sweep-window traffic {k}: {n} messages, {b} B payload ({:.0} B/s wire est.)", wire(*n, *b));
    }
    let (rb, rf, ru) = (r2_after.0 - r2_before.0, r2_after.1 - r2_before.1, r2_after.2 - r2_before.2);
    let s2 = sim.replica(2, 1).sim.stats;
    println!(
        "RESULT   member 2 during the sweep: {:.1} rollbacks/s, {:.1} re-simulated frames/s, re-sim {:.2} ms per rendered sim frame ({:.0} ms over the window); step avg {:.3} ms; max depth {}",
        rb as f64 / secs, rf as f64 / secs, ru as f64 / 1000.0 / naive_frames.max(1) as f64, ru as f64 / 1000.0,
        s2.step_us as f64 / 1000.0 / s2.steps.max(1) as f64, s2.max_depth
    );
    // End state: the full state hash (every ball, the hands, every table) at the last frame both
    // members have simulated.
    let last = sim.replica(1, 1).sim.frame.min(sim.replica(2, 1).sim.frame) - 1;
    let (h1, h2) = (sim.replica(1, 1).sim.hashes[&last], sim.replica(2, 1).sim.hashes[&last]);
    println!("RESULT   end state at frame {last} (all 500 balls, both hands, all tables): m1 {h1:016x} m2 {h2:016x} -> {}", if h1 == h2 { "IDENTICAL" } else { "DIFFERENT" });
    print_stats(&sim, 1);
    check_world(&sim, 1, f(8.5), &format!("ballpit {hz}Hz"));
}

/// A member moves only its own hand (design 8.4; P2-8 review, from @shader-reviewer's probe through
/// pe-mcp). In a 100-ball pit member 1 sweeps its hand (object 100) while member 2 sweeps its own
/// (101) and, every fourth frame, also sends a hand entry for member 1's hand. Review HO-1: member 2
/// also grabs member 1's hand at 2.0 s and carries it towards a portal at 2.2 s, and member 1 touches
/// its own hand at 2.1 s. Every replica refuses member 2's entries (`RejectedNotOwner`), member 2's
/// grab takes no claim, so member 1's touch is accepted, and the history equals the run without
/// member 2's entries.
fn handowner() {
    let hz = 60u64;
    let scene = SceneKind::BallPit { balls: 100, hands: 2 };
    let f = |s: f64| (s * hz as f64).round() as u64;
    let (h1, h2) = (100u32, 101u32);
    let (a0, a1) = (f(1.0), f(4.0));
    let path = |fr: u64, from: f64, to: f64, z: f64| {
        let u = (fr - a0) as f64 / (a1 - a0) as f64;
        [((from + (to - from) * u) * 1000.0).round() as i32, 400, (z * 1000.0).round() as i32]
    };
    let run = |forge: bool, fixes_off: u64| {
        let ids = [1, 2];
        let peers: Vec<Peer> = ids
            .iter()
            .map(|&id| {
                let own = if id == 1 { h1 } else { h2 };
                let mut v = Vec::new();
                for fr in a0..=a1 {
                    let target_mm = if id == 1 { path(fr, -0.6, 0.6, 0.0) } else { path(fr, 0.6, -0.6, 0.5) };
                    v.push((fr, Action::Hand { obj: own, target_mm, active: true, ahead: 0 }));
                    if id == 1 && fr == f(2.1) {
                        v.push((fr, Action::Touch { obj: h1, impulse: [0.0, 0.5, 0.0], grab: false }));
                    }
                    if id == 2 && forge && fr % 4 == 0 {
                        v.push((fr, Action::Hand { obj: h1, target_mm: [0, 3000, 0], active: true, ahead: 0 }));
                    }
                }
                v.push((a1 + 1, Action::Hand { obj: own, target_mm: [0, 1500, 0], active: false, ahead: 0 }));
                let mut p = Peer::new(id, 0);
                p.join(1, &ids, scene, cfg60(), 100_000, plan(&v));
                p.worlds.get_mut(&1).unwrap().sim.fixes_off = fixes_off;
                p
            })
            .collect();
        let mut sim = Sim::new(Net::new(link(30, 10, 0), 7), peers, 6_000_000);
        let i2 = sim.peers.iter().position(|p| p.id == 2).unwrap();
        let (mut grabbed, mut carried, mut planted) = (false, false, false);
        let panic_at = review2::run_to_end(&mut sim, |sim| {
            let now = sim.net.now_us;
            let send = |sim: &mut Sim, p: Payload| {
                let e = sim.peers[i2].worlds.get_mut(&1).unwrap().sim.author(p, None);
                sim.net.send(1, 2, 1, &Msg::Entry(e));
            };
            if forge && !grabbed && now >= 2_000_000 {
                send(sim, Payload::Touch { obj: h1, impulse: [0.0, 5.0, 0.0], grab: true });
                grabbed = true;
            }
            if forge && !carried && now >= 2_200_000 {
                send(sim, Payload::Depart { obj: h1, dest: 99, deadline: u64::MAX, hop: None });
                carried = true;
            }
            // Review HO-1, round 2: a touch of a free ball that carries a state for member 1's hand,
            // 2 m above where member 2 sees it.
            if forge && !planted && now >= 2_400_000 {
                let r2 = &mut sim.peers[i2].worlds.get_mut(&1).unwrap().sim;
                if let Some(mut st) = r2.obj_state(h1) {
                    st.0[1] = (f32::from_bits(st.0[1]) + 2.0).to_bits();
                    let e = r2.author_carrying(Payload::Touch { obj: 5, impulse: [0.0, 0.5, 0.0], grab: false }, (h1, st));
                    sim.net.send(1, 2, 1, &Msg::Entry(e));
                }
                planted = true;
            }
        });
        (sim, panic_at)
    };
    let (a, pa) = run(true, 0);
    let (b, pb) = run(false, 0);
    // The behaviour before HO-1's round-2 fix: the carried state for member 1's hand is applied.
    let (c, pc) = run(true, fix::HO1);
    // Only frames every compared replica has confirmed: past its confirmed frame a replica may still
    // roll back (ballpit-viz, mirroring this in tools/session-sim).
    let lowest = |ss: [&Sim; 2]| ss.iter().flat_map(|s| [1, 2].map(|m| s.replica(m, 1).sim.frame.saturating_sub(1).min(s.replica(m, 1).sim.conf))).min().unwrap_or(0);
    let frames = lowest([&a, &b]);
    let mut all_refused = true;
    for m in [1, 2] {
        let r = &a.replica(m, 1).sim;
        let forged: Vec<EntryId> = r
            .all_entries()
            .iter()
            .filter(|e| e.member == 2 && matches!(e.payload, Payload::Hand { obj, .. } if obj == h1))
            .map(|e| e.id())
            .collect();
        let refused = forged.iter().filter(|&&id| r.outcomes.get(&id).is_some_and(|(_, o)| *o == Outcome::RejectedNotOwner)).count();
        all_refused &= refused == forged.len() && !forged.is_empty();
        println!("  m{m}: member 2's hand entries for member 1's hand: {}, refused {refused}", forged.len());
        // Review HO-1: the grab and the carry are refused too, and the grab took no claim.
        let others: Vec<(String, Option<Outcome>)> = r
            .all_entries()
            .iter()
            .filter(|e| e.member == 2 && matches!(e.payload, Payload::Touch { obj, .. } | Payload::Depart { obj, .. } if obj == h1))
            .map(|e| (if matches!(e.payload, Payload::Touch { .. }) { "grab".to_string() } else { "carry".to_string() }, r.outcomes.get(&e.id()).map(|(_, o)| *o)))
            .collect();
        let own = r
            .all_entries()
            .iter()
            .find(|e| e.member == 1 && matches!(e.payload, Payload::Touch { obj, .. } if obj == h1))
            .and_then(|e| r.outcomes.get(&e.id()).map(|(_, o)| *o));
        all_refused &= others.len() == 2 && others.iter().all(|(_, o)| *o == Some(Outcome::RejectedNotOwner)) && own == Some(Outcome::Accepted);
        println!("  m{m}: member 2's grab and carry of member 1's hand: {others:?}; member 1's own touch of it: {own:?}");
        let planted: Vec<Option<Outcome>> = r
            .all_entries()
            .iter()
            .filter(|e| e.member == 2 && e.carried.is_some_and(|(o, _)| o == h1))
            .map(|e| r.outcomes.get(&e.id()).map(|(_, o)| *o))
            .collect();
        all_refused &= planted.len() == 1 && planted[0] == Some(Outcome::RejectedInvalid);
        println!("  m{m}: member 2's touch of a ball carrying a state for member 1's hand: {planted:?}");
    }
    let frames_c = lowest([&c, &b]);
    let (dc, dbc) = (digest(&c.replica(1, 1).sim.hashes, frames_c), digest(&b.replica(1, 1).sim.hashes, frames_c));
    println!(
        "  handowner with HO-1's round-2 fix switched off (the behaviour before the fix): {}{}",
        if dc == dbc { "same history as without member 2's entries" } else { "member 1's history differs from the run without member 2's entries" },
        if pc.is_some() { "; a replica stopped" } else { "" }
    );
    let (da, db) = (digest(&a.replica(1, 1).sim.hashes, frames), digest(&b.replica(1, 1).sim.hashes, frames));
    println!(
        "RESULT handowner: member 2's entries for member 1's hand refused at every replica: {all_refused}; frames 0..{frames} with them {da:016x}, without them {db:016x} -> {}",
        if all_refused && da == db { "SAME HISTORY" } else { "FAIL" }
    );
    if pa.is_some() || pb.is_some() {
        println!("RESULT handowner: PANIC");
    }
    check_world(&a, 1, frames, "handowner (member 2 also sends hand entries for member 1's hand)");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| args.iter().position(|x| x == k).and_then(|i| args.get(i + 1)).cloned();
    match args.get(1).map(|s| s.as_str()) {
        Some("c1") => c1(),
        Some("c2") => c2(),
        Some("state-frame") => state_frame(false, 0),
        Some("diverged") => state_frame(true, 0),
        Some("loss") => state_frame(false, 10),
        Some("ties") => ties(),
        Some("claims") => claims(),
        Some("backdate") => backdate(),
        Some("r14") => r14(
            args.get(2).map(|s| s.as_str()).unwrap_or("ok"),
            args.iter().any(|a| a == "--reverse"),
            args.iter().any(|a| a == "--no-provenance"),
        ),
        Some("ballpit") => ballpit(
            get("--hz").map_or(60, |v| v.parse().unwrap()),
            get("--ahead").map_or(0, |v| v.parse().unwrap()),
            get("--hands").map_or(1, |v| v.parse().unwrap()),
            get("--loss").map_or(2, |v| v.parse().unwrap()),
        ),
        Some("capped") => r18::capped(&r18::Opts {
            kbps: get("--kbps").map_or(64, |v| v.parse().unwrap()),
            members: get("--members").map_or(2, |v| v.parse().unwrap()),
            shared: args.iter().any(|a| a == "--shared"),
            end_s: get("--end").map_or(10, |v| v.parse().unwrap()),
            slow: get("--slow").map(|v| (v.parse().unwrap(), get("--slow-kbps").map_or(8, |k| k.parse().unwrap()))),
            step: get("--step-ms").map(|v| (v.parse().unwrap(), get("--step-kbps").map_or(16, |k| k.parse().unwrap()))),
        }),
        Some("ackfork") => review2::ackfork(),
        Some("lateequiv") => review2::lateequiv(),
        Some("evict") => review2::evict(),
        Some("outage") => review2::outage(),
        Some("badvote") => review3::badvote(),
        Some("liar") => review3::liar(),
        Some("crashstall") => review3::crashstall(),
        Some("twosilent") => viewchange::twosilent(),
        Some("twosilent5") => viewchange::twosilent5(),
        Some("withheld") => viewchange::withheld(),
        Some("colluders") => viewchange::colluders(),
        Some("badproposer") => viewchange::badproposer(),
        Some("bridge") => viewchange::bridge(),
        Some("join") => viewchange::join(),
        Some("refwithhold") => viewchange::refwithhold(),
        Some("handowner") => handowner(),
        Some("fork9") => p24r::fork9(),
        Some("jointiming") => p24r::jointiming(),
        Some("bogusprecommit") => p24r::bogusprecommit(),
        Some("futureack") => p24r::futureack(),
        Some("kickwar") => p24r::kickwar(),
        Some("kickpartition") => p24r::kickpartition(),
        Some("lastadmin") => p24r::lastadmin(),
        Some("adminclose") => p24r::adminclose(),
        Some("ghostkick") => p24r3::ghostkick(),
        Some("selfkickhead") => p24r3::selfkickhead(),
        Some("ackgap") => p24r3::ackgap(),
        Some("latejoin") => p24r3::latejoin(),
        Some("capchurn") => p24r3::capchurn(),
        Some("deepfork") => p24r3::deepfork(),
        Some("cutbackdate") => p24r3::cutbackdate(),
        Some("joinattests") => p24r3::joinattests(),
        Some("slowadmin") => p24r3::slowadmin(),
        Some("kickbackdate") => p24r3::kickbackdate(),
        Some("lagprecommit") => p24r3::lagprecommit(),
        Some("aheadlink") => p24r3::aheadlink(),
        Some("strikeframe") => p24r4::strikeframe(),
        Some("aheadshield") => p24r4::aheadshield(),
        Some("fakeref") => p24r4::fakeref(),
        Some("joinwedge") => p24r4::joinwedge(),
        Some("slowref") => p24r4::slowref(),
        Some("prevotefork") => p24r4::prevotefork(),
        Some("fakemix") => p24r4::fakemix(),
        Some("adminfork") => p24r4::adminfork(),
        Some("randsched") => randsched::main(&args),
        Some("capfake") => p24r4::capfake(),
        Some("voidfork") => p24r4::voidfork(),
        Some("slowcapper") => p24r4::slowcapper(),
        Some("fakeadmin") => p24r4::fakeadmin(),
        Some("bigjoin") => p24r4::bigjoin(),
        Some("adminwedge") => p24r4::adminwedge(),
        Some("capstale") => p24r4::capstale(),
        Some("lagchurn") => p24r4::lagchurn(),
        Some("votechurn") => p24r4::votechurn(),
        Some("adminvoid") => p24r4::adminvoid(),
        Some("joinsquat") => p24r4::joinsquat(),
        Some("joinskip") => p24r4::joinskip(),
        Some("joinrules") => p24r::joinrules(),
        Some("onesided") => p24r::onesided(),
        Some("vcscale") => viewchange::vc_scale(
            get("--members").map_or(64, |v| v.parse().unwrap()),
            get("--silent").map_or(1, |v| v.parse().unwrap()),
            !args.iter().any(|a| a == "--no-heads"),
        ),
        Some("snap") => history::snap(get("--balls").map_or(500, |v| v.parse().unwrap()), args.iter().any(|a| a == "--stall"), get("--reps").map_or(3, |v| v.parse().unwrap())),
        Some("observer") => history::observer(get("--delay").map_or(60, |v| v.parse().unwrap()), get("--hz").map_or(60, |v| v.parse().unwrap())),
        _ => eprintln!("usage: pe-spike-session-sim c1|c2|state-frame|diverged|loss|ties|claims|backdate|r14 [ok|abort|reject|throttle|forge|late|crash|chain] [--reverse] [--no-provenance]|ballpit [--hz H] [--ahead N] [--hands H] [--loss P]"),
    }
}
