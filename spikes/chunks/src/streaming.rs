//! Streaming: a player crossing a procedurally generated world at 1.5 m/s to 1.5 km/s.
//!
//! * Content is generated per (level, cell) like OWO's procedural layers: level `L` cells hold
//!   objects of radius `S_L / 48 .. S_L / 24`, so big objects come from big, sparse cells and small
//!   ones from small cells near the player. Generation is a pure function of (seed, cell).
//! * Every 8 frames the relevance query (`|p - o| <= 24 r`) picks the simulated set; objects
//!   entering are created in the region's box3d world from their record, objects leaving are
//!   frozen back into a dormant record (exact state) and destroyed.
//! * The region's origin is rebased when the player is far from it: every 4 km in the f32 build
//!   (to keep box3d's f32 coordinates small), never in this run in the f64 build.

use std::collections::{BTreeMap, BTreeSet};

use box3d_rust::body::destroy_body;
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::Quat;
use box3d_rust::world::{world_get_awake_body_count, World};

use pe_spike_chunks::coords::{cell_size, CellKey, WorldPos};
use pe_spike_chunks::phys::{
    make_body, mode, new_world, read_state, set_kinematic_target, static_box, v3, world_def, BodyInit, BodyState,
    Shape, Snapshotter, DT, SUBSTEPS,
};
use pe_spike_chunks::spatial::{BTreeCells, Entry, HashGrid, Query, QueryStats, Spatial};
use pe_spike_chunks::{splitmix64, time_ms, unit_f64};

const RATIO: f64 = 24.0;
const LMIN: i32 = -9;
const LMAX: i32 = 3;
const PER_CELL: u64 = 24;
const PASS_EVERY: u64 = 8;

#[derive(Clone)]
struct Obj {
    r: f64,
    cube: bool,
    init: WorldPos,
    /// Exact frozen state once the object has been simulated (positions as region-independent
    /// world coordinates).
    record: Option<(WorldPos, BodyState)>,
    body: Option<BodyId>,
    in_grid: bool,
}

struct Stream {
    world: World,
    origin: [i64; 3],
    ground: BodyId,
    ground_at: WorldPos,
    objs: Vec<Obj>,
    grid: HashGrid<BTreeCells>,
    generated: BTreeMap<CellKey, Vec<u32>>,
    persisted: BTreeMap<(CellKey, u32), (WorldPos, BodyState)>,
    active: Vec<u32>,
    player: BodyId,
    player_pos: WorldPos,
    rebase_dist: f64,
    rebases: u32,
    rebase_ms: f64,
}

fn shape(o: &Obj) -> Shape {
    if o.cube { Shape::Cube((o.r * 0.577) as f32) } else { Shape::Ball(o.r as f32) }
}

impl Stream {
    fn new(rebase_dist: f64, player_y: f64) -> Self {
        let mut world = new_world(&world_def(1.0, 10.0, false));
        let origin = [0, 0, 0];
        let ground_at = WorldPos::new(origin, [512.0, 0.0, 512.0]);
        let g = ground_at.to_region(origin, 0);
        let ground = static_box(&mut world, [g[0], g[1] - 1.0, g[2]], [20_000.0, 1.0, 20_000.0]);
        let player_pos = WorldPos::new(origin, [512.0, player_y, 512.0]);
        let mut pi = BodyInit::at(player_pos.to_region(origin, 0));
        pi.kinematic = true;
        let player = make_body(&mut world, &pi, Shape::Ball(0.4));
        Stream {
            world,
            origin,
            ground,
            ground_at,
            objs: Vec::new(),
            grid: HashGrid::new(RATIO),
            generated: BTreeMap::new(),
            persisted: BTreeMap::new(),
            active: Vec::new(),
            player,
            player_pos,
            rebase_dist,
            rebases: 0,
            rebase_ms: 0.0,
        }
    }

    fn ground_cell(&self, level: i32) -> CellKey {
        let mut k = self.player_pos.cell(level);
        k.c[1] = WorldPos::new([0, 0, 0], [0.0; 3]).cell(level).c[1];
        k
    }

    /// Generate the 3x3 ground cells around the player at every level; evict cells two or more
    /// cells away whose objects are all dormant. Returns (cells generated, cells evicted).
    fn generate(&mut self) -> (u32, u32) {
        let mut made = 0;
        let mut want = BTreeSet::new();
        for level in LMIN..=LMAX {
            let c = self.ground_cell(level);
            for dx in -1..=1 {
                for dz in -1..=1 {
                    want.insert(CellKey { level, c: [c.c[0] + dx, c.c[1], c.c[2] + dz] });
                }
            }
        }
        for key in &want {
            if self.generated.contains_key(key) {
                continue;
            }
            made += 1;
            let s = cell_size(key.level);
            let corner = key.min_corner();
            let mut ids = Vec::new();
            for i in 0..PER_CELL {
                let hk = splitmix64(
                    0x5EED ^ (key.level as u64).wrapping_mul(0x1000193) ^ (key.c[0] as u64).wrapping_mul(0x9E3779B97F4A7C15)
                        ^ (key.c[2] as u64).wrapping_mul(0xC2B2AE3D27D4EB4F)
                        ^ i,
                );
                let r = s / 48.0 * (1.0 + unit_f64(splitmix64(hk ^ 1)));
                let x = unit_f64(splitmix64(hk ^ 2)) * s;
                let z = unit_f64(splitmix64(hk ^ 3)) * s;
                let init = corner.offset([x, r, z]);
                let id = self.objs.len() as u32;
                let record = self.persisted.remove(&(*key, i as u32));
                let pos = record.map(|r| r.0).unwrap_or(init);
                self.objs.push(Obj { r, cube: hk & 1 == 0, init, record, body: None, in_grid: true });
                self.grid.insert(Entry { id, pos, r });
                ids.push(id);
            }
            self.generated.insert(*key, ids);
        }
        // Eviction.
        let mut evicted = 0;
        let stale: Vec<CellKey> = self
            .generated
            .keys()
            .filter(|k| {
                let c = self.ground_cell(k.level);
                (k.c[0] - c.c[0]).abs() > 2 || (k.c[2] - c.c[2]).abs() > 2
            })
            .copied()
            .collect();
        for key in stale {
            let ids = &self.generated[&key];
            if ids.iter().any(|&id| self.objs[id as usize].body.is_some()) {
                continue;
            }
            let ids = self.generated.remove(&key).unwrap();
            for (i, id) in ids.into_iter().enumerate() {
                let o = &mut self.objs[id as usize];
                if let Some(rec) = o.record.take() {
                    self.persisted.insert((key, i as u32), rec);
                }
                if o.in_grid {
                    self.grid.remove(id);
                    o.in_grid = false;
                }
            }
            evicted += 1;
        }
        (made, evicted)
    }

    fn activate(&mut self, id: u32) {
        let o = &self.objs[id as usize];
        let (p, st) = match &o.record {
            Some((p, st)) => (*p, Some(*st)),
            None => (o.init, None),
        };
        let mut init = BodyInit::at(p.to_region(self.origin, 0));
        init.sleep_threshold = 0.05;
        match st {
            Some(st) => {
                init.rot = Quat { v: v3(st.q[0], st.q[1], st.q[2]), s: st.q[3] };
                init.lin = v3(st.v[0], st.v[1], st.v[2]);
                init.ang = v3(st.w[0], st.w[1], st.w[2]);
                init.awake = st.awake;
            }
            None => init.awake = false,
        }
        let sh = shape(o);
        let b = make_body(&mut self.world, &init, sh);
        self.objs[id as usize].body = Some(b);
    }

    fn deactivate(&mut self, id: u32) {
        let b = self.objs[id as usize].body.take().expect("active");
        let st = read_state(&self.world, b);
        let p = WorldPos::from_region(self.origin, 0, st.p);
        destroy_body(&mut self.world, b);
        let o = &mut self.objs[id as usize];
        o.record = Some((p, st));
        if o.in_grid {
            self.grid.update(id, p);
        }
    }

    fn rebase(&mut self) {
        let (_, ms) = time_ms(|| {
            let ids = self.active.clone();
            for &id in &ids {
                self.deactivate(id);
            }
            self.origin = self.player_pos.chunk;
            destroy_body(&mut self.world, self.player);
            let mut pi = BodyInit::at(self.player_pos.to_region(self.origin, 0));
            pi.kinematic = true;
            self.player = make_body(&mut self.world, &pi, Shape::Ball(0.4));
            self.recenter_ground();
            for &id in &ids {
                self.activate(id);
            }
        });
        self.rebases += 1;
        self.rebase_ms += ms;
    }

    fn recenter_ground(&mut self) {
        destroy_body(&mut self.world, self.ground);
        self.ground_at = WorldPos::new(self.player_pos.chunk, [512.0, 0.0, 512.0]);
        let g = self.ground_at.to_region(self.origin, 0);
        self.ground = static_box(&mut self.world, [g[0], g[1] - 1.0, g[2]], [20_000.0, 1.0, 20_000.0]);
    }
}

#[derive(Default)]
struct Stats {
    frames: u64,
    step_ms: Vec<f64>,
    pass_ms: Vec<f64>,
    gen_ms: f64,
    query_ms: f64,
    act_ms: f64,
    deact_ms: f64,
    created: u64,
    destroyed: u64,
    cells_made: u64,
    cells_evicted: u64,
    active: Vec<usize>,
    awake: Vec<i32>,
}

fn run_speed(speed: f64, seconds: f64, rebase_dist: f64) {
    let player_y = if speed <= 20.0 { 1.0 } else { 40.0 };
    let mut s = Stream::new(rebase_dist, player_y);
    let frames = (seconds * 60.0) as u64;
    let warm = 120;
    let mut st = Stats::default();
    let mut initial_ms = 0.0;
    for f in 0..frames {
        // Move the player.
        s.player_pos = s.player_pos.offset([speed / 60.0, 0.0, 0.0]);
        if s.player_pos.sub(&WorldPos::new(s.origin, [512.0; 3]))[0].abs() > s.rebase_dist {
            s.rebase();
        } else if s.player_pos.sub(&s.ground_at)[0].abs() > 4_000.0 {
            s.recenter_ground();
        }
        let target = s.player_pos.to_region(s.origin, 0);
        set_kinematic_target(&mut s.world, s.player, target);
        let mut pass_total = 0.0;
        if f % PASS_EVERY == 0 {
            let ((made, evicted), gms) = time_ms(|| s.generate());
            // Relevance around the anchor now and where it will be at the next pass (look-ahead),
            // so fast movers find their surroundings already active.
            let ahead = s.player_pos.offset([speed * PASS_EVERY as f64 / 60.0, 0.0, 0.0]);
            let (want, qms) = time_ms(|| {
                let mut v = Vec::new();
                for c in [s.player_pos, ahead] {
                    let q = Query { center: c, ratio: RATIO, min_r: 1.7 / 64.0 };
                    s.grid.query(&q, &mut v, &mut QueryStats::default());
                }
                v.sort_unstable();
                v.dedup();
                v
            });
            // Sorted-list difference: deterministic order of creation and destruction.
            let (mut add, mut del) = (Vec::new(), Vec::new());
            let (mut i, mut j) = (0, 0);
            while i < want.len() || j < s.active.len() {
                if j >= s.active.len() || (i < want.len() && want[i] < s.active[j]) {
                    add.push(want[i]);
                    i += 1;
                } else if i >= want.len() || s.active[j] < want[i] {
                    del.push(s.active[j]);
                    j += 1;
                } else {
                    i += 1;
                    j += 1;
                }
            }
            let (_, dms) = time_ms(|| {
                for &id in &del {
                    s.deactivate(id);
                }
            });
            let (_, ams) = time_ms(|| {
                for &id in &add {
                    s.activate(id);
                }
            });
            s.active = want;
            pass_total = gms + qms + dms + ams;
            if f >= warm {
                st.gen_ms += gms;
                st.query_ms += qms;
                st.deact_ms += dms;
                st.act_ms += ams;
                st.created += add.len() as u64;
                st.destroyed += del.len() as u64;
                st.cells_made += made as u64;
                st.cells_evicted += evicted as u64;
                st.pass_ms.push(pass_total);
            } else if f == 0 {
                initial_ms = pass_total;
            }
        }
        let (_, sms) = time_ms(|| s.world.step(DT, SUBSTEPS));
        if f >= warm {
            st.frames += 1;
            st.step_ms.push(sms + pass_total);
            st.active.push(s.active.len());
            st.awake.push(world_get_awake_body_count(&s.world));
        }
    }
    let n = st.frames as f64;
    let secs = n / 60.0;
    let mean = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let max = |v: &Vec<f64>| v.iter().cloned().fold(0.0, f64::max);
    let mut sorted = st.step_ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p99 = sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)];
    println!(
        "RESULT streaming mode={} speed_mps={speed} seconds={seconds} rebase_every_m={rebase_dist} initial_load_ms={initial_ms:.1} frame_ms_mean={:.3} frame_ms_p99={p99:.3} frame_ms_max={:.3} pass_ms_mean={:.3} pass_ms_max={:.3} gen_ms_per_s={:.2} query_ms_per_s={:.2} activate_ms_per_s={:.2} deactivate_ms_per_s={:.2} created_per_s={:.0} destroyed_per_s={:.0} cells_made_per_s={:.1} active_mean={:.0} active_max={} awake_mean={:.0} rebases={} rebase_ms_each={:.2} objects_known={} persisted={}",
        mode(),
        mean(&st.step_ms),
        max(&st.step_ms),
        mean(&st.pass_ms),
        max(&st.pass_ms),
        st.gen_ms / secs,
        st.query_ms / secs,
        st.act_ms / secs,
        st.deact_ms / secs,
        st.created as f64 / secs,
        st.destroyed as f64 / secs,
        st.cells_made as f64 / secs,
        st.active.iter().sum::<usize>() as f64 / n,
        st.active.iter().max().unwrap(),
        st.awake.iter().map(|&a| a as f64).sum::<f64>() / n,
        s.rebases,
        if s.rebases > 0 { s.rebase_ms / s.rebases as f64 } else { 0.0 },
        s.objs.len(),
        s.persisted.len()
    );
}

pub fn run() {
    println!("== streaming ({}) ==", mode());
    let rebase = if mode() == "f32" { 4_000.0 } else { 1.0e9 };
    for speed in [1.5, 15.0, 150.0, 1500.0] {
        run_speed(speed, 20.0, rebase);
    }
    // What activating whole chunks within 1 km (OWO-style, without size culling) would mean for
    // the same content: objects within 1 km at every level.
    let mut total = 0.0;
    for level in LMIN..=LMAX {
        let s = cell_size(level);
        let area = std::f64::consts::PI * 1000.0f64.powi(2);
        total += PER_CELL as f64 * (area / (s * s)).min(PER_CELL as f64 * 1e12);
    }
    println!("RESULT streaming-flat-radius objects_within_1km_all_sizes={total:.3e} (computed from generation density)");

    // Activation from dormant records vs from a box3d world image, 1,000 bodies.
    let mut w = new_world(&world_def(1.0, 10.0, false));
    static_box(&mut w, [0.0, -1.0, 0.0], [100.0, 1.0, 100.0]);
    let recs: Vec<BodyInit> = (0..1000)
        .map(|i| {
            let mut b = BodyInit::at([(i % 32) as f64 * 1.5 - 24.0, 0.5, (i / 32) as f64 * 1.5 - 24.0]);
            b.awake = false;
            b
        })
        .collect();
    let (ids, create_ms) = time_ms(|| recs.iter().map(|r| make_body(&mut w, r, Shape::Cube(0.4))).collect::<Vec<_>>());
    let mut snap = Snapshotter::new();
    let (img, save_ms) = time_ms(|| snap.save(&w));
    let (_, restore_ms) = time_ms(|| snap.restore(&mut w, &img));
    let (states, read_ms) = time_ms(|| ids.iter().map(|&b| read_state(&w, b)).collect::<Vec<_>>());
    let (_, destroy_ms) = time_ms(|| {
        for &b in &ids {
            destroy_body(&mut w, b);
        }
    });
    let rec_bytes = size_of::<u32>() + size_of::<WorldPos>() + 4 * 4 + 6 * 4 + 8 + 1;
    println!(
        "RESULT activation-cost mode={} bodies=1000 create_from_records_ms={create_ms:.3} freeze_read_ms={read_ms:.3} destroy_ms={destroy_ms:.3} image_save_ms={save_ms:.3} image_restore_ms={restore_ms:.3} image_bytes_per_body={:.0} record_bytes_per_body={rec_bytes}",
        mode(),
        img.len() as f64 / 1000.0,
    );
    let _ = states;
}
