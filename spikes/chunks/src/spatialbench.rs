//! Spatial-index study (user suggestion 14:12Z: "perhaps an octree on the chunk system"):
//! chunk-rooted octree vs chunk-rooted loose octree vs hashed multi-level grid (BTreeMap and
//! HashMap), on a human-scale world and an extreme-scale one.

use pe_spike_chunks::coords::WorldPos;
use pe_spike_chunks::spatial::{
    brute_force, query_sorted, BTreeCells, ChunkOctree, Entry, HashCells, HashGrid, Query, QueryStats, Spatial,
};
use pe_spike_chunks::{log_uniform, splitmix64, time_ms, unit_f64};

const RATIO: f64 = 32.0;

struct Workload {
    name: &'static str,
    objs: Vec<Entry>,
    queries: Vec<Query>,
    /// (id, velocity m/s) for the objects that move each frame
    movers: Vec<(u32, [f64; 3])>,
}

fn h(seed: u64, i: u64, k: u64) -> u64 {
    splitmix64(seed ^ splitmix64(i.wrapping_mul(0x9E37) ^ k))
}

/// 200,000 objects on a 32 x 32 km surface, radii 2 cm-50 m (log-uniform) plus 200 of
/// 50 m-2 km; 64 players at human scale; 10% of objects move at 1-5 m/s and 1% at 300 m/s.
fn human(n: usize) -> Workload {
    let seed = 0xA11CE;
    let mut objs = Vec::with_capacity(n);
    for i in 0..n {
        let r = if i < 200 { log_uniform(h(seed, i as u64, 1), 50.0, 2000.0) } else { log_uniform(h(seed, i as u64, 1), 0.02, 50.0) };
        let x = (unit_f64(h(seed, i as u64, 2)) - 0.5) * 32_000.0;
        let z = (unit_f64(h(seed, i as u64, 3)) - 0.5) * 32_000.0;
        objs.push(Entry { id: i as u32, pos: WorldPos::from_abs([x, r, z]), r });
    }
    let queries = (0..64)
        .map(|i| {
            let x = (unit_f64(h(seed, i, 7)) - 0.5) * 30_000.0;
            let z = (unit_f64(h(seed, i, 8)) - 0.5) * 30_000.0;
            Query { center: WorldPos::from_abs([x, 1.7, z]), ratio: RATIO, min_r: 1.7 / 64.0 }
        })
        .collect();
    let movers = (0..n as u64)
        .filter(|i| i % 10 == 3)
        .map(|i| {
            let fast = i % 100 == 3;
            let s = if fast { 300.0 } else { 1.0 + 4.0 * unit_f64(h(seed, i, 9)) };
            let a = unit_f64(h(seed, i, 10)) * 6.283;
            (i as u32, [s * a.cos(), 0.0, s * a.sin()])
        })
        .collect();
    Workload { name: "human-surface", objs, queries, movers }
}

/// 200,000 objects with radii 1 um-1000 km (log-uniform) in 64 clusters spread over
/// 10,000 km in 3D; an object of radius r sits within ~200 r of its cluster centre. Players range
/// from 10 um to 100 km tall (min_r = height / 64). Movers: 10% at 3 r per second.
fn extreme(n: usize) -> Workload {
    let seed = 0xE57;
    let centers: Vec<WorldPos> = (0..64u64)
        .map(|c| {
            WorldPos::new(
                [
                    ((unit_f64(h(seed, c, 1)) - 0.5) * 1.0e7 / 1024.0) as i64,
                    ((unit_f64(h(seed, c, 2)) - 0.5) * 1.0e7 / 1024.0) as i64,
                    ((unit_f64(h(seed, c, 3)) - 0.5) * 1.0e7 / 1024.0) as i64,
                ],
                [512.0; 3],
            )
        })
        .collect();
    let mut objs = Vec::with_capacity(n);
    for i in 0..n as u64 {
        let r = log_uniform(h(seed, i, 4), 1.0e-6, 1.0e6);
        let c = centers[(i % 64) as usize];
        let spread = 200.0 * r;
        let d = [
            (unit_f64(h(seed, i, 5)) - 0.5) * spread,
            (unit_f64(h(seed, i, 6)) - 0.5) * spread,
            (unit_f64(h(seed, i, 7)) - 0.5) * spread,
        ];
        objs.push(Entry { id: i as u32, pos: c.offset(d), r });
    }
    let queries = (0..64u64)
        .map(|i| {
            let height = log_uniform(h(seed, i, 8), 1.0e-5, 1.0e5);
            let c = centers[(i % 64) as usize];
            let d = [height * 3.0, height, -height * 2.0];
            Query { center: c.offset(d), ratio: RATIO, min_r: height / 64.0 }
        })
        .collect();
    let movers = objs
        .iter()
        .filter(|e| e.id % 10 == 3)
        .map(|e| {
            let a = unit_f64(h(seed, e.id as u64, 9)) * 6.283;
            (e.id, [3.0 * e.r * a.cos(), 0.0, 3.0 * e.r * a.sin()])
        })
        .collect();
    Workload { name: "extreme-scale", objs, queries, movers }
}

fn structures() -> Vec<Box<dyn Spatial>> {
    vec![
        Box::new(ChunkOctree::new(false, RATIO)),
        Box::new(ChunkOctree::new(true, RATIO)),
        Box::new(HashGrid::<BTreeCells>::new(RATIO)),
        Box::new(HashGrid::<HashCells>::new(RATIO)),
    ]
}

pub fn run() {
    for wl in [human(200_000), extreme(200_000)] {
        println!("== spatial: workload {} ({} objects, {} queries, {} movers/frame) ==", wl.name, wl.objs.len(), wl.queries.len(), wl.movers.len());
        // Ground truth for the first 16 queries.
        let truth: Vec<Vec<u32>> = wl.queries.iter().take(16).map(|q| brute_force(&wl.objs, q)).collect();
        let avg_rel = wl.queries.iter().take(16).zip(&truth).map(|(_, t)| t.len()).sum::<usize>() as f64 / 16.0;
        let mut reference: Option<Vec<Vec<u32>>> = None;
        for mut s in structures() {
            let (_, build_ms) = time_ms(|| {
                for e in &wl.objs {
                    s.insert(*e);
                }
            });
            let mem = s.mem_bytes();
            // Correctness against brute force.
            let mut st = QueryStats::default();
            for (q, t) in wl.queries.iter().zip(&truth) {
                assert_eq!(&query_sorted(s.as_ref(), q, &mut st), t, "{} disagrees with brute force", s.name());
            }
            // Query cost (all 64 queries, best of 5 passes) including the sort to canonical order.
            let mut best = f64::INFINITY;
            let mut st = QueryStats::default();
            let mut results = Vec::new();
            for _ in 0..5 {
                st = QueryStats::default();
                let (r, ms) = time_ms(|| wl.queries.iter().map(|q| query_sorted(s.as_ref(), q, &mut st)).collect::<Vec<_>>());
                results = r;
                best = best.min(ms);
            }
            let q_us = best * 1e3 / wl.queries.len() as f64;
            match &reference {
                None => reference = Some(results),
                Some(r) => assert_eq!(r, &results, "{} differs from the first structure", s.name()),
            }
            // Updates: 10 frames of movement at 60 Hz.
            let mut pos: Vec<WorldPos> = wl.objs.iter().map(|e| e.pos).collect();
            let (_, upd_ms) = time_ms(|| {
                for _ in 0..10 {
                    for &(id, v) in &wl.movers {
                        let p = pos[id as usize].offset([v[0] / 60.0, v[1] / 60.0, v[2] / 60.0]);
                        pos[id as usize] = p;
                        s.update(id, p);
                    }
                }
            });
            let upd_ns = upd_ms * 1e6 / (10 * wl.movers.len()) as f64;
            // Fast movers only (the 1% at 300 m/s in the human workload; 3 r/s in the extreme one).
            println!(
                "RESULT spatial workload={} structure={} build_ms={build_ms:.1} mem_MB={:.1} query_us={q_us:.1} nodes_per_query={:.0} tested_per_query={:.0} results_per_query={:.1} update_ns={upd_ns:.0} depth_or_levels={}",
                wl.name,
                s.name(),
                mem as f64 / 1e6,
                st.nodes as f64 / wl.queries.len() as f64,
                st.tested as f64 / wl.queries.len() as f64,
                avg_rel,
                s.max_depth()
            );
        }
        // Determinism: the same structure built in a different insertion order returns the same
        // canonical (sorted) results.
        let mut order: Vec<usize> = (0..wl.objs.len()).collect();
        order.sort_by_key(|&i| splitmix64(i as u64 ^ 0xD00D));
        let mut same = true;
        for mut s in structures() {
            for &i in &order {
                s.insert(wl.objs[i]);
            }
            let mut st = QueryStats::default();
            let r: Vec<Vec<u32>> = wl.queries.iter().map(|q| query_sorted(s.as_ref(), q, &mut st)).collect();
            same &= Some(&r) == reference.as_ref();
        }
        println!("RESULT spatial-determinism workload={} shuffled_insert_same_results={same}", wl.name);
    }
}
