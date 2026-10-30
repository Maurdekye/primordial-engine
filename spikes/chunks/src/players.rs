//! Regions: one box3d world per region of the canonical world line.
//! * `regions`: region-scoped rollback against one flat world holding the same content, serial vs
//!   parallel stepping, region merge cost, and determinism of independent regions.
//! * `players`: per-player CPU, memory and bandwidth against total player count (2/16/64/256),
//!   spread out, in groups of 8, and all clustered (the clustered case is also the cost of the
//!   current, unchunked design, where every peer simulates the whole world).

use box3d_rust::id::BodyId;
use box3d_rust::recording::hash_world_state;
use box3d_rust::world::{world_get_awake_body_count, World};

use pe_spike_chunks::phys::{
    build_ballpit, hand_target, make_body, mode, new_world, read_state, set_kinematic_target, world_def, BodyInit,
    Shape, Snapshotter, BALL_R, DT, SUBSTEPS,
};
use pe_spike_chunks::time_ms;

pub const BALLS: usize = 125;

pub struct Pit {
    pub center: [f64; 3],
    pub balls: Vec<BodyId>,
    pub hand: BodyId,
    pub phase: f64,
}

pub struct Region {
    pub world: World,
    pub pits: Vec<Pit>,
    pub frame: u64,
    pub snap: Snapshotter,
}

impl Region {
    /// `n` pits on a square grid `spacing` metres apart (one player each).
    pub fn new(n: usize, spacing: f64, first_phase: usize) -> Region {
        let mut world = new_world(&world_def(1.0, 10.0, false));
        let side = (n as f64).sqrt().ceil() as usize;
        let pits = (0..n)
            .map(|i| {
                let c = [(i % side) as f64 * spacing, 0.0, (i / side) as f64 * spacing];
                let (balls, hand) = build_ballpit(&mut world, BALLS, c);
                Pit { center: c, balls, hand, phase: (first_phase + i) as f64 * 0.37 }
            })
            .collect();
        Region { world, pits, frame: 0, snap: Snapshotter::new() }
    }

    pub fn step(&mut self) {
        for p in &self.pits {
            set_kinematic_target(&mut self.world, p.hand, hand_target(p.center, self.frame, p.phase));
        }
        self.world.step(DT, SUBSTEPS);
        self.frame += 1;
    }

    pub fn save(&mut self) -> (u64, Vec<u8>) {
        (self.frame, self.snap.save(&self.world))
    }

    pub fn restore(&mut self, k: &(u64, Vec<u8>)) {
        self.snap.restore(&mut self.world, &k.1);
        self.frame = k.0;
    }

    pub fn bodies(&self) -> usize {
        self.pits.len() * (BALLS + 1)
    }
}

/// Measured costs of one region of `n` pits after warm-up.
#[derive(Clone, Copy, Debug)]
pub struct RegionCost {
    pub n: usize,
    pub bodies: usize,
    pub awake: i32,
    pub step_ms: f64,
    pub save_ms: f64,
    pub restore_ms: f64,
    /// One coalesced rollback per tick: restore + 5 re-simulated frames + keyframe saves (K = 4).
    pub rollback5_ms: f64,
    pub image: usize,
    pub image_lz4: usize,
}

pub fn measure_region(n: usize) -> RegionCost {
    let mut r = Region::new(n, 4.0, 0);
    for _ in 0..90 {
        r.step();
    }
    let frames = (20_000 / (n * BALLS)).clamp(5, 120);
    let (_, ms) = time_ms(|| {
        for _ in 0..frames {
            r.step();
        }
    });
    let step_ms = ms / frames as f64;
    let (k, save_ms) = time_ms(|| r.save());
    let image = k.1.len();
    let image_lz4 = lz4_flex::compress(&k.1).len();
    for _ in 0..5 {
        r.step();
    }
    let reps = if n >= 64 { 1 } else { 3 };
    let mut rb = 0.0;
    let mut restore_ms = 0.0;
    for _ in 0..reps {
        let (_, ms) = time_ms(|| {
            let (_, rms) = time_ms(|| r.restore(&k));
            restore_ms += rms;
            for i in 0..5 {
                r.step();
                if i == 3 {
                    let _ = r.save();
                }
            }
        });
        rb += ms;
    }
    RegionCost {
        n,
        bodies: r.bodies(),
        awake: world_get_awake_body_count(&r.world),
        step_ms,
        save_ms,
        restore_ms: restore_ms / reps as f64,
        rollback5_ms: rb / reps as f64,
        image,
        image_lz4,
    }
}

// ---- regions vs one flat world ---------------------------------------------------------------------

pub fn regions() {
    println!("== regions vs flat world ({}) ==", mode());
    let n = 16;
    // Flat: one box3d world holding all 16 pits, 200 m apart (as if 16 players shared one world).
    let mut flat = Region::new(n, 200.0, 0);
    // Chunked: 16 regions of one pit each, each at its own origin.
    let mut regs: Vec<Region> = (0..n).map(|i| Region::new(1, 0.0, i)).collect();
    for _ in 0..90 {
        flat.step();
        for r in &mut regs {
            r.step();
        }
    }
    let frames = 60;
    let (_, flat_ms) = time_ms(|| {
        for _ in 0..frames {
            flat.step();
        }
    });
    let (_, ser_ms) = time_ms(|| {
        for _ in 0..frames {
            for r in &mut regs {
                r.step();
            }
        }
    });
    // Parallel: every region on its own thread (regions are independent worlds).
    let (_, par_ms) = time_ms(|| {
        std::thread::scope(|s| {
            for r in regs.iter_mut() {
                s.spawn(move || {
                    for _ in 0..frames {
                        r.step();
                    }
                });
            }
        });
    });
    // A late entry for one player at depth 6: the flat world restores and re-simulates everything,
    // the chunked world only that player's region.
    let kf = flat.save();
    let kr = regs[3].save();
    for _ in 0..6 {
        flat.step();
        regs[3].step();
    }
    let (_, flat_rb) = time_ms(|| {
        flat.restore(&kf);
        for _ in 0..6 {
            flat.step();
        }
    });
    let (_, reg_rb) = time_ms(|| {
        regs[3].restore(&kr);
        for _ in 0..6 {
            regs[3].step();
        }
    });
    let flat_img = flat.save().1.len();
    let reg_img: usize = regs.iter_mut().map(|r| r.save().1.len()).sum();
    println!(
        "RESULT regions mode={} players={n} bodies_total={} step_flat_ms={:.3} step_regions_serial_ms={:.3} step_regions_parallel_wall_ms={:.3} rollback6_flat_ms={flat_rb:.3} rollback6_one_region_ms={reg_rb:.3} image_flat_bytes={flat_img} image_regions_sum_bytes={reg_img}",
        mode(),
        flat.bodies(),
        flat_ms / frames as f64,
        ser_ms / frames as f64,
        par_ms / frames as f64,
    );

    // Region merge: move every body of region B into region A (read exact state, recreate).
    for nb in [1usize, 8] {
        let mut a = Region::new(nb, 4.0, 0);
        let mut b = Region::new(nb, 4.0, 100);
        for _ in 0..30 {
            a.step();
            b.step();
        }
        let shift = [1024.0, 0.0, 0.0];
        let (moved, ms) = time_ms(|| {
            let mut moved = 0;
            for p in &b.pits {
                for &ball in &p.balls {
                    let s = read_state(&b.world, ball);
                    let mut init = BodyInit::at([s.p[0] + shift[0], s.p[1], s.p[2]]);
                    init.rot = box3d_rust::math_functions::Quat { v: pe_spike_chunks::phys::v3(s.q[0], s.q[1], s.q[2]), s: s.q[3] };
                    init.lin = pe_spike_chunks::phys::v3(s.v[0], s.v[1], s.v[2]);
                    init.ang = pe_spike_chunks::phys::v3(s.w[0], s.w[1], s.w[2]);
                    init.awake = s.awake;
                    make_body(&mut a.world, &init, Shape::Ball(BALL_R));
                    moved += 1;
                }
            }
            moved
        });
        println!("RESULT region-merge mode={} bodies_moved={moved} ms={ms:.3} us_per_body={:.2}", mode(), ms * 1e3 / moved as f64);
    }

    // Determinism: independent regions give identical per-frame hashes whether they are stepped in
    // order, in reverse, or in parallel; a region restored and re-simulated matches its straight run.
    let run = |order: &str| -> Vec<Vec<u64>> {
        let mut regs: Vec<Region> = (0..8).map(|i| Region::new(2, 4.0, 2 * i)).collect();
        let mut hashes = vec![Vec::new(); 8];
        match order {
            "parallel" => std::thread::scope(|s| {
                let hs: Vec<_> = regs
                    .iter_mut()
                    .map(|r| {
                        s.spawn(move || {
                            (0..300)
                                .map(|_| {
                                    r.step();
                                    hash_world_state(&r.world)
                                })
                                .collect::<Vec<u64>>()
                        })
                    })
                    .collect();
                for (i, h) in hs.into_iter().enumerate() {
                    hashes[i] = h.join().unwrap();
                }
            }),
            _ => {
                for _ in 0..300 {
                    let idx: Vec<usize> = if order == "reverse" { (0..8).rev().collect() } else { (0..8).collect() };
                    for i in idx {
                        regs[i].step();
                        hashes[i].push(hash_world_state(&regs[i].world));
                    }
                }
            }
        }
        hashes
    };
    let fwd = run("forward");
    let rev = run("reverse");
    let par = run("parallel");
    let mut r = Region::new(2, 4.0, 0);
    let mut straight = Vec::new();
    let mut ring = Vec::new();
    for f in 0..300u64 {
        if f % 4 == 0 {
            ring.push(r.save());
        }
        r.step();
        straight.push(hash_world_state(&r.world));
    }
    // Roll back to the keyframe at frame 200 and re-simulate to 300.
    let k = ring.iter().find(|k| k.0 == 200).unwrap().clone();
    r.restore(&k);
    let mut resim_ok = true;
    for f in 200..300usize {
        r.step();
        resim_ok &= hash_world_state(&r.world) == straight[f];
    }
    println!(
        "RESULT region-determinism mode={} forward_eq_reverse={} forward_eq_parallel={} rollback_resim_eq_straight={resim_ok} final_digest={:016x}",
        mode(),
        fwd == rev,
        fwd == par,
        fwd.iter().fold(0xcbf29ce484222325u64, |h, v| (h ^ v[299]).wrapping_mul(0x100000001b3))
    );
}

// ---- per-player cost against player count ----------------------------------------------------------

/// Wire bytes per player per second, computed from the design's measured encodings (7.6):
/// hand-target entry 44 B, ack 4 + 25 n B for an n-member log, 52 B of IP/UDP/AEAD overhead per
/// datagram, acks every 2 frames. `world_ack` selects the world-tier ack format.
fn bandwidth(n_region: usize, n_world: usize, compact_world_acks: bool) -> (f64, f64) {
    let ov = 52.0;
    let others = (n_region - 1) as f64;
    // Region tier: every other region member streams hand targets at 60 Hz and acks at 30 Hz.
    let region_rx = others * (60.0 * (44.0 + ov) + 30.0 * (4.0 + 25.0 * n_region as f64 + ov));
    // World tier (membership, interest, ACL, SCM): every member re-declares interest at 1 Hz
    // (about 60 B) and acks the world log at 2 Hz.
    let wo = (n_world - 1) as f64;
    let ack = if compact_world_acks { 100.0 } else { 4.0 + 25.0 * n_world as f64 };
    let world_rx = wo * (1.0 * (60.0 + ov) + 2.0 * (ack + ov));
    (region_rx, world_rx)
}

pub fn players() {
    println!("== players: per-player cost against player count ({}) ==", mode());
    let counts = [2usize, 16, 64, 256];
    let mut sizes: Vec<usize> = vec![1, 2, 8, 16, 64, 256];
    sizes.sort();
    sizes.dedup();
    let mut costs = std::collections::BTreeMap::new();
    for &n in &sizes {
        let c = measure_region(n);
        println!(
            "RESULT region-cost mode={} pits={} bodies={} awake={} step_ms={:.3} save_ms={:.3} restore_ms={:.3} rollback5_ms={:.3} image_bytes={} image_lz4_bytes={}",
            mode(),
            c.n,
            c.bodies,
            c.awake,
            c.step_ms,
            c.save_ms,
            c.restore_ms,
            c.rollback5_ms,
            c.image,
            c.image_lz4
        );
        costs.insert(n, c);
    }
    for &total in &counts {
        for (layout, group) in [("spread", 1usize), ("groups-of-8", 8), ("clustered", total)] {
            let g = group.min(total);
            let c = costs[&g];
            // CPU per 16.7 ms tick: own region's step + keyframe save every 4 frames + (if another
            // engaged member is in the region) one coalesced 5-frame rollback per tick, as the
            // ball-pit run measured (59 rollbacks/s, about 5 frames deep).
            let cpu = c.step_ms + c.save_ms / 4.0 + if g >= 2 { c.rollback5_ms } else { 0.0 };
            // Memory: live world (about one image) + 4 keyframes.
            let mem = 5.0 * c.image as f64;
            let (region_rx, world_rx_full) = bandwidth(g, total, false);
            let (_, world_rx_compact) = bandwidth(g, total, true);
            println!(
                "RESULT players mode={} players={total} layout={layout} region_members={g} sim_bodies_per_player={} cpu_ms_per_tick={cpu:.3} mem_MB={:.2} rx_region_kBps={:.1} rx_world_kBps_fullmesh_acks={:.1} rx_world_kBps_compact_acks={:.1}",
                mode(),
                c.bodies,
                mem / 1e6,
                region_rx / 1e3,
                world_rx_full / 1e3,
                world_rx_compact / 1e3
            );
        }
    }
}
