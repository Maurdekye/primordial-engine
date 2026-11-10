//! P2-2's done criterion: the phase-1 spike's goldens, 600-frame trace, snapshot images and
//! rollback T1-T5 reproduce through pe-physics, with the digests committed in
//! `tests/records/vdw/` (engine configuration "vdw" and upstream "off"), at several worker counts.

mod common;

use box3d_rust::determinism::{
    create_falling_ragdolls, create_mesh_drop, create_query_spawn, create_wave_pile, update_falling_ragdolls,
    update_mesh_drop, update_query_spawn, update_wave_pile,
};
use box3d_rust::math_functions::POS_ZERO;
use common::*;
use pe_physics::{PhysicsConfig, PhysicsWorld};

/// Worker counts every record is checked at (results must not depend on them, D2).
const WORKERS: [i32; 3] = [1, 3, 8];

#[derive(Debug, PartialEq)]
struct Golden {
    sleep_step: i32,
    hash: u32,
    query: Option<(i32, u32)>,
}

fn goldens(config: PhysicsConfig) -> Vec<(&'static str, Golden)> {
    let dt = 1.0 / 60.0;
    let mut out = Vec::new();
    let new = || PhysicsWorld::new(config).unwrap();
    {
        let mut pw = new();
        let mut d = create_falling_ragdolls(pw.raw_mut());
        for _ in 0..1000 {
            pw.step(dt).unwrap();
            if update_falling_ragdolls(pw.raw(), &mut d) {
                break;
            }
        }
        out.push(("falling ragdolls", Golden { sleep_step: d.sleep_step, hash: d.hash, query: None }));
    }
    {
        let mut pw = new();
        let mut d = create_wave_pile(pw.raw_mut());
        for _ in 0..1000 {
            pw.step(dt).unwrap();
            if update_wave_pile(pw.raw(), &mut d) {
                break;
            }
        }
        out.push(("wave pile", Golden { sleep_step: d.sleep_step, hash: d.hash, query: None }));
    }
    {
        let mut pw = new();
        let mut d = create_query_spawn(pw.raw_mut());
        for _ in 0..1000 {
            pw.step(dt).unwrap();
            if update_query_spawn(pw.raw_mut(), &mut d) {
                break;
            }
        }
        let query = Some((d.query_hit_count, d.query_hash));
        out.push(("query spawn", Golden { sleep_step: d.sleep_step, hash: d.hash, query }));
    }
    {
        let mut pw = new();
        let mut d = create_mesh_drop(pw.raw_mut(), POS_ZERO);
        for _ in 0..1000 {
            pw.step(dt).unwrap();
            if update_mesh_drop(pw.raw(), &mut d) {
                break;
            }
        }
        out.push(("mesh drop", Golden { sleep_step: d.sleep_step, hash: d.hash, query: None }));
    }
    out
}

/// The engine goldens (spikes/physics `goldens`) and Box3D C's goldens (`--upstream`), for the
/// precision Box3D was built with (D58: double by default). C's values are test_determinism.c's,
/// as in the fork's determinism tests.
#[test]
fn goldens_match() {
    let g = |sleep_step, hash| Golden { sleep_step, hash, query: None };
    let q = |sleep_step, hash, hits, qh| Golden { sleep_step, hash, query: Some((hits, qh)) };
    let (engine, c) = if box3d_rust::core::is_double_precision() {
        (
            [g(326, 0x5908_8655), g(299, 0xFF78_3571), q(242, 0x92E8_FC29, 59, 0x5B44_29DC), g(221, 0x9884_84F7)],
            [g(263, 0xA542_3F98), g(272, 0xC8E5_F3D6), q(242, 0x4895_85E6, 59, 0x5B44_29DC), g(218, 0xD378_CF04)],
        )
    } else {
        (
            [g(358, 0xB1EB_47AE), g(307, 0x0809_EA96), q(242, 0x9EA3_A8B9, 59, 0xE327_1F3D), g(220, 0xCE3B_7A62)],
            [g(291, 0xFF4C_605B), g(272, 0x266F_83B0), q(242, 0xF3AD_9BA6, 59, 0xE327_1F3D), g(226, 0xD2FF_069D)],
        )
    };
    let mut mismatches = Vec::new();
    for workers in WORKERS {
        for (config, expected) in [(PhysicsConfig::engine(), &engine), (PhysicsConfig::upstream(), &c)] {
            let got = goldens(config.with_workers(workers));
            let scaling = config.contact_recycle_scaling.enabled;
            for ((scene, got), want) in got.iter().zip(expected.iter()) {
                println!("RESULT golden {scene} recycle_scaling={scaling} workers={workers} {got:?}");
                if got != want {
                    mismatches.push(format!("{scene}, {workers} workers, recycle scaling {scaling}: got {got:?}, want {want:?}"));
                }
            }
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// The 600-frame trace, line for line equal to the committed record, and its digest.
/// The committed trace record of `mode` and its digest (FNV-64 of the state hashes, then the
/// world hashes), so no digest is written into this file: re-recording the traces is enough.
fn trace_record(mode: &str) -> (String, u64) {
    let text = std::fs::read_to_string(record(&format!("trace-{mode}-win-{}.txt", profile()))).expect("record");
    let text = text.replace("\r\n", "\n");
    let (mut state, mut world) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        world.push(u64::from_str_radix(f[1], 16).expect("record world hash"));
        state.push(u64::from_str_radix(f[2], 16).expect("record state hash"));
    }
    let digest = fnv_u64s(state.into_iter().chain(world));
    (text, digest)
}

#[test]
fn trace_matches_record() {
    for (config, mode) in modes() {
        let (want, digest) = trace_record(mode);
        for workers in WORKERS {
            let (state, world) = reference_run(config.with_workers(workers), 600);
            let got: String = state
                .iter()
                .zip(&world)
                .enumerate()
                .map(|(i, (s, w))| format!("{i} {w:016x} {s:016x}\n"))
                .collect();
            assert!(got == want, "trace differs from the record: {mode}, {workers} workers");
            let d = fnv_u64s(state.iter().copied().chain(world.iter().copied()));
            assert_eq!(d, digest, "trace digest, {mode}, {workers} workers");
            println!("RESULT trace {mode} workers={workers} digest {d:016x}");
        }
    }
}

/// The world-image bytes every 50 frames (FNV-64 and length), equal to the committed record:
/// pe-physics builds exactly the spike's world.
#[test]
fn images_match_record() {
    for (config, mode) in modes() {
        let want = std::fs::read_to_string(record(&format!("images-{mode}-win-{}.txt", profile()))).expect("record");
        for workers in WORKERS {
            let inputs = scripted_inputs();
            let mut g = Game::pile(config.with_workers(workers));
            let mut got = String::new();
            while g.frame <= 600 {
                if g.frame.is_multiple_of(50) {
                    let image = g.pw.save_image().unwrap();
                    let bytes = image.world_bytes();
                    got += &format!("{} {} {:016x}\n", g.frame, bytes.len(), fnv_bytes(bytes));
                }
                let f = g.frame;
                let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
                g.advance(&now);
            }
            assert!(got == want.replace("\r\n", "\n"), "images differ from the record: {mode}, {workers} workers\n{got}");
            println!("RESULT images {mode} {} workers={workers} digest {:016x}", profile(), fnv_bytes(got.as_bytes()));
        }
    }
}

fn first_mismatch(a: &[u64], b: &[u64]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y).or(if a.len() != b.len() { Some(a.len().min(b.len())) } else { None })
}

/// Rollback T1-T5 (spikes/physics `rollback`) through pe-physics images.
#[test]
fn rollback_t1_to_t5() {
    const FRAMES: u64 = 600;
    for (config, mode) in modes() {
        for workers in WORKERS {
            let config = config.with_workers(workers);
            let ctx = format!("{mode}, {workers} workers");
            let (reference, _) = reference_run(config, FRAMES);
            let inputs = scripted_inputs();

            // T1: snapshot at 300, step to 400, restore in place, step to 400 again.
            {
                let mut g = Game::pile(config);
                g.step_scripted(&inputs, 300);
                let snap = g.save();
                let first = g.step_scripted(&inputs, 400);
                g.restore(&snap);
                assert!(g.pw.all_bodies_valid(), "T1 body ids, {ctx}");
                let second = g.step_scripted(&inputs, 400);
                assert_eq!(first, second, "T1 replay, {ctx}");
                assert_eq!(first[..], reference[300..400], "T1 vs reference, {ctx}");
            }

            // T2: one interaction learned 60 frames late: one rollback of depth 60.
            {
                let late = inputs.iter().copied().find(|i| i.frame == 200).unwrap();
                let mut sim = RollbackSim::new(Game::pile(config), 120);
                for i in inputs.iter().filter(|i| i.frame != 200) {
                    sim.add_input(*i).unwrap();
                }
                sim.advance_to(260);
                assert_ne!(sim.hashes[&259], reference[259], "T2 diverges before the rollback, {ctx}");
                assert_eq!(sim.add_input(late), Ok(true), "T2 late, {ctx}");
                sim.advance_to(FRAMES);
                let got: Vec<u64> = (0..FRAMES).map(|f| sim.hashes[&f]).collect();
                assert_eq!(first_mismatch(&got, &reference), None, "T2 converges, {ctx}");
                assert_eq!((sim.rollbacks, sim.resimulated), (1, 60), "T2 rollback depth, {ctx}");
            }

            // T3: every interaction 1..=45 frames late (network-like).
            {
                let mut sim = RollbackSim::new(Game::pile(config), 120);
                let mut arrivals: Vec<(u64, Interaction)> = inputs
                    .iter()
                    .map(|i| (i.frame + 1 + splitmix64(i.frame ^ ((i.peer as u64) << 32)) % 45, *i))
                    .collect();
                arrivals.sort_by_key(|(a, i)| (*a, i.peer, i.seq));
                for f in 0..FRAMES {
                    for (_, i) in arrivals.iter().filter(|(a, _)| *a == f) {
                        sim.add_input(*i).unwrap();
                    }
                    sim.advance_to(f + 1);
                }
                let got: Vec<u64> = (0..FRAMES).map(|f| sim.hashes[&f]).collect();
                assert_eq!(first_mismatch(&got, &reference), None, "T3 converges, {ctx}");
            }

            // T4: a late joiner. The donor (a rollback sim, which saved an image every frame)
            // sends its frame-300 state as transfer bytes; the joiner restores them into a
            // brand-new world only because their check hash equals the one a second peer
            // computed for the same frame (that peer never saved an image, so the hash does not
            // depend on save history).
            {
                let mut sim = RollbackSim::new(Game::pile(config), 120);
                for i in &inputs {
                    sim.add_input(*i).unwrap();
                }
                sim.advance_to(300);
                let mut other = Game::pile(config);
                other.step_scripted(&inputs, 300);
                let confirmed = other.pw.check_hash().unwrap();
                let bytes = sim.game.pw.save_transfer().unwrap().to_bytes();
                let transfer = pe_physics::Transfer::from_bytes(&bytes).unwrap();
                assert_eq!(transfer.to_bytes(), bytes, "T4 transfer bytes round trip, {ctx}");
                assert_eq!(transfer.check_hash(), confirmed, "T4 check hash independent of save history, {ctx}");
                println!("RESULT transfer {mode} frame=300 workers={workers} check_hash {confirmed} bytes {}", bytes.len());
                // The joiner may run a different worker count.
                let joiner = config.with_workers(9 - workers);
                let mut other_hash = confirmed;
                other_hash.0[31] ^= 1;
                assert!(PhysicsWorld::from_transfer(joiner, &transfer, &other_hash).is_err(), "T4 unconfirmed, {ctx}");
                let pw = PhysicsWorld::from_transfer(joiner, &transfer, &confirmed).expect("fresh restore");
                assert_eq!(pw.check_hash().unwrap(), confirmed, "T4 joiner's check hash, {ctx}");
                let mut g = Game { pw, objects: sim.game.objects.clone(), frame: 300, claims: sim.game.claims.clone(), seed: SEED };
                assert!(g.pw.all_bodies_valid(), "T4 body ids, {ctx}");
                let hs = g.step_scripted(&inputs, FRAMES);
                assert_eq!(first_mismatch(&hs, &reference[300..]), None, "T4 continues identically, {ctx}");
            }

            // T5: an interaction older than the 120-frame window is refused.
            {
                let mut sim = RollbackSim::new(Game::pile(config), 120);
                sim.advance_to(400);
                let r = sim.add_input(Interaction { frame: 100, peer: 9, seq: 0, body: 1, impulse: [0.0, 1.0, 0.0] });
                assert_eq!(r, Err(InputError::TooLate), "T5, {ctx}");
            }
            println!("RESULT rollback T1-T5 {mode} workers={workers} PASS");
        }
    }
}

/// Several worlds in one process, stepped interleaved, with different configurations and
/// worker counts: each reproduces its own trace digest.
#[test]
fn several_worlds_per_process() {
    let inputs = scripted_inputs();
    let mut games = [
        Game::pile(PhysicsConfig::engine().with_workers(2)),
        Game::pile(PhysicsConfig::upstream().with_workers(1)),
        Game::pile(PhysicsConfig::engine().with_workers(4)),
    ];
    let mut hashes: Vec<(Vec<u64>, Vec<u64>)> = vec![(Vec::new(), Vec::new()); games.len()];
    for f in 0..600u64 {
        let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
        for (g, h) in games.iter_mut().zip(hashes.iter_mut()) {
            g.advance(&now);
            h.0.push(g.hash());
            h.1.push(g.pw.box3d_hash().unwrap());
        }
    }
    let digest = |(s, w): &(Vec<u64>, Vec<u64>)| fnv_u64s(s.iter().copied().chain(w.iter().copied()));
    let (vdw, off) = (trace_record("vdw").1, trace_record("off").1);
    assert_eq!(digest(&hashes[0]), vdw);
    assert_eq!(digest(&hashes[1]), off);
    assert_eq!(digest(&hashes[2]), vdw);
}
