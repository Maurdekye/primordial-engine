//! Session-protocol scenarios (design review C1/C2/H4/H5/M3, rulings 09:30Z/09:32Z/09:39Z, the
//! ball-pit question and R14), run in-process on a virtual clock. Deterministic: the same command
//! prints the same RESULT lines on every run.
//!
//! usage: pe-spike-session-sim <scenario> [options]
//!   c1 | c2 | state-frame | diverged | loss | ties | claims | backdate
//!   r14 [ok|abort|reject|throttle|forge|late|crash] [--reverse]
//!   ballpit [--hz 60|90] [--ahead N] [--hands 1|2] [--loss P]
//!   snap [--balls N] [--stall]        snapshot-policy sweep (the memory/CPU knob, design 6.7)
//!   ackfork | lateequiv | evict | outage   round-2 review repros (design 10.8)
//!   observer [--delay MS] [--hz H]    what an observer sees of a remote hand (design 6.9)

use std::collections::BTreeMap;

mod history;
mod r18;
mod review2;
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
    let reference = replay_sim_with(w, r.scene, r.sim.cfg, &members, &r.sim.peers_of, &final_log(&r.sim), frames).hashes;
    let rd = digest(&reference, frames);
    let all_equal = digests.iter().all(|(_, d)| *d == rd);
    if !all_equal {
        if let Some(f) = (0..frames).find(|f| reference.get(f) != r.sim.hashes.get(f)) {
            let log = final_log(&r.sim);
            let near: Vec<String> = log.iter().filter(|(_, ef)| *ef + 3 >= f && *ef <= f + 1).map(|(e, ef)| format!("m{}s{}@{} eff {ef}", e.member, e.seq, e.stamp)).collect();
            println!("  first replay mismatch at frame {f}; final-log entries near it: {}", near.join(", "));
            let rs = replay_sim(w, r.scene, r.sim.cfg, &members, &log, f + 1);
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

fn r14(variant: &str, reverse: bool) {
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
    if variant == "forge" {
        // Reviews R3-4, R3-5: one-member worlds that a single member controls, D (4, member 2) and
        // E (5, member 3). Their owners route them to A and B; A and B do not route to them.
        let tiny = SceneKind::Pile { side: 3, layers: 1 };
        peers[1].join(4, &[2], tiny, SimConfig { seed: 0xD, ..cfg60() }, 120_000, BTreeMap::new());
        peers[2].join(5, &[3], tiny, SimConfig { seed: 0xE, ..cfg60() }, 120_000, BTreeMap::new());
        peers[1].route(4, &[1]);
        peers[2].route(5, &[2]);
    }
    // Route lists (genesis config, D46): A and B trade with each other only.
    for p in &mut peers {
        p.route(1, &[2]);
        p.route(2, &[1]);
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
    if variant == "late" {
        // Review R3-6: a deadline 2 frames of B after the departure, shorter than the certificate
        // round: the arrival is late, B refuses it, and that certified refusal returns the object.
        sim.carry_deadline_frames = 2;
    }
    let mut lag_samples: Vec<(u64, u64)> = Vec::new();
    // "forge" (reviews NEW-3..NEW-5): proofless relays by members who cannot have proofs.
    let mut forged: Vec<(WorldId, MemberId, EntryId, &'static str)> = Vec::new();
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
                let e = sim.peers[i].worlds.get_mut(&1).unwrap().sim.author(Payload::Depart { obj: 50, dest: 4, deadline: u64::MAX }, None);
                forged.push((1, 2, e.id(), "member 2 departs A's object 50 to D, a world it controls alone"));
                send_all(&mut sim, 1, 2, &e);
                // R3-6: member 2 departs A's object 51 to a world nobody knows, deadline 0.
                let e = sim.peers[i].worlds.get_mut(&1).unwrap().sim.author(Payload::Depart { obj: 51, dest: 999, deadline: 0 }, None);
                forged.push((1, 2, e.id(), "member 2 departs A's object 51 to unknown world 999"));
                send_all(&mut sim, 1, 2, &e);
            }
            if t == 2_200_000 {
                // R3-5: member 3 certifies, as the whole membership of its world E, a departure that
                // never happened, of a record carrying A's object 40's global id.
                let record = ObjRecord { global: (1u64 << 32) | 40, kind: 1, state: BodyState([0; 13]), src_world: 5, src_frame: 0 };
                let fake = 0x1234_5678;
                let fact = fact_departed(5, fake, &record, 2, u64::MAX);
                let i = pidx(&sim, 3);
                let e = sim.peers[i].worlds.get_mut(&2).unwrap().sim.author(
                    Payload::Arrive { transfer: transfer_id(5, fake), record, depart: fake, deadline: u64::MAX, cert: vec![(3, member_sig(5, 3, fact))] },
                    None,
                );
                forged.push((2, 3, e.id(), "member 3 mints A's object 40 into B from its own world E"));
                send_all(&mut sim, 2, 3, &e);
            }
            if t == 2_500_000 {
                // NEW-4: member 3 (only in B) mints object 40 into B with no departure behind it.
                let record = ObjRecord { global: (1u64 << 32) | 40, kind: 1, state: BodyState([0; 13]), src_world: 1, src_frame: 100 };
                let fake = 0xDEAD_BEEF;
                let fact = fact_departed(1, fake, &record, 2, u64::MAX);
                let i = pidx(&sim, 3);
                let e = sim.peers[i].worlds.get_mut(&2).unwrap().sim.author(
                    Payload::Arrive { transfer: transfer_id(1, fake), record, depart: fake, deadline: u64::MAX, cert: vec![(3, member_sig(1, 3, fact))] },
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
                        Payload::Arrive { transfer, record: x.record.unwrap(), depart: x.depart, deadline: x.deadline, cert: vec![] },
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
                let record = ObjRecord { global: (1u64 << 32) | 41, kind: 1, state: st, src_world: 1, src_frame: 150 };
                let fake = 0x0BAD_0BAD;
                let fact = fact_departed(1, fake, &record, 2, u64::MAX);
                let cert: Vec<(MemberId, u64)> = [1u32, 2].iter().map(|&m| (m, member_sig(1, m, fact))).collect();
                let i = pidx(&sim, 1);
                let e = sim.peers[i].worlds.get_mut(&2).unwrap().sim.author(
                    Payload::Arrive { transfer: transfer_id(1, fake), record, depart: fake, deadline: u64::MAX, cert },
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
                    let fact = fact_departed(1, x.depart, &rec, 2, x.deadline);
                    let cert = sim.replica(1, 1).sim.cert(fact);
                    if let Some(cert) = cert {
                        let bad = ObjRecord { kind: (rec.kind + 1) % 3, ..rec };
                        let i = pidx(&sim, 1);
                        let e = sim.peers[i].worlds.get_mut(&2).unwrap().sim.author(
                            Payload::Arrive { transfer, record: bad, depart: x.depart, deadline: x.deadline, cert },
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
    if variant == "forge" {
        let live = |m: MemberId, w: WorldId, g: u64| sim.replica(m, w).sim.t.objs.iter().flatten().filter(|o| o.global == g).count();
        let (g50, g51) = ((1u64 << 32) | 50, (1u64 << 32) | 51);
        let (a50, a51, d50, b40) = (live(1, 1, g50), live(1, 1, g51), live(2, 4, g50), live(3, 2, gid));
        println!(
            "RESULT r14 forge: A's objects 50 and 51 live in A {a50} and {a51}, object 50 in D {d50}; copies of A:40 in B {b40} -> {}",
            if a50 == 1 && a51 == 1 && d50 == 0 && b40 <= 1 { "CONSERVED" } else { "NOT CONSERVED" }
        );
        // Every attack must have been tried: one whose moment passed (the carry resolved earlier)
        // would otherwise drop out silently.
        if forged.len() != 9 {
            println!("RESULT r14 forge: FAIL: {} of 9 forged entries authored", forged.len());
        }
    }
    print_stats(&sim, 1);
    print_stats(&sim, 2);
    // The crashed carrier's own replicas are not compared.
    let skip: &[MemberId] = if crash { &[1] } else { &[] };
    check_world_except(&sim, 1, 500, &format!("r14 {variant} A"), skip);
    check_world_except(&sim, 2, 400, &format!("r14 {variant} B"), skip);
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
        Some("r14") => r14(args.get(2).map(|s| s.as_str()).unwrap_or("ok"), args.iter().any(|a| a == "--reverse")),
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
        Some("vcscale") => viewchange::vc_scale(
            get("--members").map_or(64, |v| v.parse().unwrap()),
            get("--silent").map_or(1, |v| v.parse().unwrap()),
            !args.iter().any(|a| a == "--no-heads"),
        ),
        Some("snap") => history::snap(get("--balls").map_or(500, |v| v.parse().unwrap()), args.iter().any(|a| a == "--stall"), get("--reps").map_or(3, |v| v.parse().unwrap())),
        Some("observer") => history::observer(get("--delay").map_or(60, |v| v.parse().unwrap()), get("--hz").map_or(60, |v| v.parse().unwrap())),
        _ => eprintln!("usage: pe-spike-session-sim c1|c2|state-frame|diverged|loss|ties|claims|backdate|r14 [ok|abort|reject|throttle|forge|late|crash] [--reverse]|ballpit [--hz H] [--ahead N] [--hands H] [--loss P]"),
    }
}
