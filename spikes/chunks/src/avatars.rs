//! R17: built-in avatar and avatar-feature culling, measured on a synthetic crowd.
//!
//! Culling is a per-viewer presentation decision. It reads sim state and never writes it:
//! `sim_isolation` drives the culling pass from a real region's box3d world through `&World` only (the
//! borrow checker forbids writes) and checks that the region's per-frame hashes are unchanged.
//! Avatar parts that do touch the sim (engaged hand colliders, an optional body collider, the activation
//! anchor, held objects) are not part of the culled avatar at all: they are perturbation sources in the
//! region log (7.2, 18.4, 25.5) and every region member simulates them regardless of what it renders.
//!
//! The workload is a stand-in for a VRChat/Resonite-class avatar, run on the CPU so it can be timed here:
//! * skinning: 20,000 vertices x 4 weights from 60 bones (LOD1 8,000, LOD2 2,000);
//! * secondary motion ("dynamic bones"): 8 chains x 10 joints of Verlet with 4 constraint passes;
//! * particles: 2 emitters x 400;
//! * presentation logic: 2,000 arithmetic ops per tick.
//!
//! GPU work is not timed (no renderer here); the run reports its proxies instead: skinned vertices,
//! draw calls, lights and particles submitted. Pose-stream bandwidth is computed per tier.

use pe_spike_chunks::coords::WorldPos;
use pe_spike_chunks::spatial::{BTreeCells, Entry, HashGrid, Query, QueryStats, Spatial};
use pe_spike_chunks::{splitmix64, time_ms, unit_f64};

const BONES: usize = 60;
const VERTS: [usize; 4] = [20_000, 8_000, 2_000, 0];
const CHAINS: [usize; 4] = [8, 4, 0, 0];
const PARTICLES: [usize; 4] = [800, 200, 0, 0];
const LOGIC_EVERY: [u64; 4] = [1, 2, 4, 0];
const DRAWS: [u32; 5] = [4, 2, 1, 1, 0];
const LIGHTS: [u32; 5] = [2, 1, 0, 0, 0];
/// Pose stream per tier: (Hz, bytes per update). T0 full body 22 bones quantized to 8 B + 16 B root.
const POSE: [(f64, f64); 5] = [(60.0, 192.0), (30.0, 192.0), (15.0, 96.0), (5.0, 24.0), (0.0, 0.0)];

/// Tier by angular size (height / distance), the R16 relevance metric, with a frustum test.
/// T0 full, T1 reduced, T2 far, T3 impostor, T4 culled.
pub fn tier(height: f64, dist: f64, in_view: bool) -> usize {
    let a = height / dist.max(1e-6);
    let t = if a > 1.0 / 8.0 {
        0
    } else if a > 1.0 / 32.0 {
        1
    } else if a > 1.0 / 128.0 {
        2
    } else if a > 1.0 / 512.0 {
        3
    } else {
        4
    };
    if in_view { t } else { t.max(3) }
}

struct Model {
    rest: Vec<[f32; 3]>,
    weights: Vec<[(u16, f32); 4]>,
}

impl Model {
    fn new() -> Self {
        let n = VERTS[0];
        let rest = (0..n)
            .map(|i| {
                let h = splitmix64(i as u64);
                [unit_f64(h) as f32, unit_f64(h >> 7) as f32 * 1.7, unit_f64(h >> 13) as f32]
            })
            .collect();
        let weights = (0..n)
            .map(|i| {
                let b = (i * BONES / n) as u16;
                [(b, 0.55), ((b + 1) % BONES as u16, 0.25), ((b + 2) % BONES as u16, 0.15), ((b + 3) % BONES as u16, 0.05)]
            })
            .collect();
        Model { rest, weights }
    }
}

#[derive(Clone)]
struct AvatarState {
    bones: Vec<[f32; 12]>,
    skinned: Vec<[f32; 3]>,
    chains: Vec<[[f32; 3]; 10]>,
    prev: Vec<[[f32; 3]; 10]>,
    particles: Vec<[f32; 6]>,
    logic: f32,
}

impl AvatarState {
    fn new() -> Self {
        AvatarState {
            bones: vec![[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0]; BONES],
            skinned: vec![[0.0; 3]; VERTS[0]],
            chains: vec![[[0.0; 3]; 10]; CHAINS[0]],
            prev: vec![[[0.0; 3]; 10]; CHAINS[0]],
            particles: vec![[0.0; 6]; PARTICLES[0]],
            logic: 0.0,
        }
    }
}

/// One frame of presentation work for one avatar at tier `t`. Returns a checksum so the optimizer
/// keeps the work.
fn present(model: &Model, s: &mut AvatarState, t: usize, frame: u64, seed: u64) -> f32 {
    if t >= 4 {
        return 0.0;
    }
    let ph = (frame as f32 * 0.016) + seed as f32 * 0.1;
    // Pose: bone matrices (rotation about y by a small angle per bone, from a cheap polynomial).
    for (i, b) in s.bones.iter_mut().enumerate() {
        let a = ph + i as f32 * 0.05;
        let c = 1.0 - a * a * 0.5;
        let sn = a - a * a * a / 6.0;
        *b = [c, 0.0, sn, 0.01 * i as f32, 0.0, 1.0, 0.0, 0.0, -sn, 0.0, c, 0.0];
    }
    if t >= 3 {
        return s.bones[0][0];
    }
    // Skinning at the tier's LOD (every k-th vertex stands in for the decimated mesh).
    let n = VERTS[t];
    let step = VERTS[0] / n;
    let mut acc = 0.0f32;
    for k in 0..n {
        let i = k * step;
        let p = model.rest[i];
        let mut o = [0.0f32; 3];
        for &(b, w) in &model.weights[i] {
            let m = &s.bones[b as usize];
            o[0] += w * (m[0] * p[0] + m[1] * p[1] + m[2] * p[2] + m[3]);
            o[1] += w * (m[4] * p[0] + m[5] * p[1] + m[6] * p[2] + m[7]);
            o[2] += w * (m[8] * p[0] + m[9] * p[1] + m[10] * p[2] + m[11]);
        }
        s.skinned[k] = o;
        acc += o[1];
    }
    // Secondary motion: Verlet chains with 4 distance-constraint passes.
    for c in 0..CHAINS[t] {
        let ch = &mut s.chains[c];
        let pv = &mut s.prev[c];
        ch[0] = [0.1 * c as f32 + ph.sin_approx(), 1.6, 0.0];
        for j in 1..10 {
            let cur = ch[j];
            for a in 0..3 {
                ch[j][a] += (cur[a] - pv[j][a]) * 0.98;
            }
            ch[j][1] -= 0.0027;
            pv[j] = cur;
        }
        for _ in 0..4 {
            for j in 1..10 {
                let d = [ch[j][0] - ch[j - 1][0], ch[j][1] - ch[j - 1][1], ch[j][2] - ch[j - 1][2]];
                let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
                let k = (l - 0.05) / l * 0.5;
                for a in 0..3 {
                    ch[j][a] -= d[a] * k;
                }
            }
        }
        acc += ch[9][1];
    }
    // Particles.
    for p in s.particles.iter_mut().take(PARTICLES[t]) {
        p[3] *= 0.99;
        p[4] = p[4] * 0.99 - 0.0027;
        p[0] += p[3] * 0.016;
        p[1] += p[4] * 0.016;
        if p[1] < 0.0 {
            p[1] = 1.5;
            p[4] = 0.5;
        }
        acc += p[1];
    }
    // Presentation logic.
    if LOGIC_EVERY[t] > 0 && frame % LOGIC_EVERY[t] == 0 {
        let mut x = s.logic;
        for i in 0..2000 {
            x = x * 0.999 + (i as f32) * 1e-4;
        }
        s.logic = x;
        acc += x;
    }
    acc
}

trait SinApprox {
    fn sin_approx(self) -> f32;
}
impl SinApprox for f32 {
    fn sin_approx(self) -> f32 {
        let x = self.fract() * 2.0 - 1.0;
        x * (1.0 - x.abs())
    }
}

struct Crowd {
    pos: Vec<WorldPos>,
    height: Vec<f64>,
}

fn crowd(n: usize, half: f64, scales: bool) -> Crowd {
    let mut pos = Vec::new();
    let mut height = Vec::new();
    for i in 0..n as u64 {
        let h = splitmix64(0xAB ^ i);
        let x = (unit_f64(h) - 0.5) * 2.0 * half;
        let z = (unit_f64(splitmix64(h)) - 0.5) * 2.0 * half;
        pos.push(WorldPos::from_abs([x, 0.0, z]));
        // Mixed scales: 10% tiny (17 cm), 10% giant (17 m), the rest 1.7 m.
        let ht = if scales && i % 10 == 1 { 0.17 } else if scales && i % 10 == 2 { 17.0 } else { 1.7 };
        height.push(ht);
    }
    Crowd { pos, height }
}

#[derive(Default, Debug, Clone, Copy)]
struct Load {
    cpu_ms: f64,
    verts: u64,
    draws: u32,
    lights: u32,
    particles: u64,
    pose_bps: f64,
    tiers: [u32; 5],
}

/// Apply the per-class budgets: at most `max_full` T0 avatars and `max_lights` lights; the largest
/// angular size wins (ties by index, so the result is deterministic per viewer).
fn budget(tiers: &mut [usize], prio: &[f64], max_full: usize) {
    let mut full: Vec<usize> = (0..tiers.len()).filter(|&i| tiers[i] == 0).collect();
    full.sort_by(|&a, &b| prio[b].partial_cmp(&prio[a]).unwrap().then(a.cmp(&b)));
    for &i in full.iter().skip(max_full) {
        tiers[i] = 1;
    }
}

fn run_case(model: &Model, c: &Crowd, policy: &str, frames: u64) -> Load {
    let n = c.pos.len();
    let viewer = WorldPos::from_abs([0.0, 1.6, 0.0]);
    // Index the crowd in the R16 grid and find avatars inside the farthest tier reach.
    let mut grid: HashGrid<BTreeCells> = HashGrid::new(512.0);
    for i in 0..n {
        grid.insert(Entry { id: i as u32, pos: c.pos[i], r: c.height[i] * 0.5 });
    }
    let mut near = Vec::new();
    grid.query(&Query { center: viewer, ratio: 1024.0, min_r: 0.0 }, &mut near, &mut QueryStats::default());
    near.sort_unstable();
    // Without culling every avatar is drawn in full, wherever it is.
    let mut tiers = vec![if policy == "none" { 0usize } else { 4 }; n];
    let mut prio = vec![0.0; n];
    for &id in &near {
        let i = id as usize;
        let d = c.pos[i].sub(&viewer);
        let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        // 110 degree horizontal field of view looking along +x.
        let in_view = d[0] > 0.0 && d[2].abs() <= d[0] * 1.43;
        prio[i] = c.height[i] / dist.max(0.01);
        tiers[i] = match policy {
            "none" => 0,
            "distance" => tier(c.height[i], dist, true),
            "frustum" => if in_view { 0 } else { 4 },
            "distance+frustum" => tier(c.height[i], dist, in_view),
            _ => tier(c.height[i], dist, in_view),
        };
    }
    if policy == "distance+frustum+budget" {
        budget(&mut tiers, &prio, 16);
    }
    let mut states = vec![AvatarState::new(); n];
    let mut sink = 0.0f32;
    let (_, ms) = time_ms(|| {
        for f in 0..frames {
            for i in 0..n {
                sink += present(model, &mut states[i], tiers[i], f, i as u64);
            }
        }
    });
    std::hint::black_box(sink);
    let mut l = Load { cpu_ms: ms / frames as f64, ..Default::default() };
    let mut lights = 0;
    for i in 0..n {
        let t = tiers[i];
        l.tiers[t] += 1;
        l.verts += if t < 3 { VERTS[t] as u64 } else { 0 };
        l.draws += DRAWS[t];
        lights += LIGHTS[t];
        l.particles += if t < 3 { PARTICLES[t] as u64 } else { 0 };
        l.pose_bps += POSE[t].0 * (POSE[t].1 + 52.0 / 8.0); // shared datagrams: 1/8 of the overhead each
    }
    l.lights = if policy == "distance+frustum+budget" { lights.min(8) } else { lights };
    l
}

pub fn run() {
    println!("== avatars (R17): per-viewer culling on a synthetic crowd ==");
    let model = Model::new();
    for (n, half, scales) in [(64usize, 50.0, false), (256, 15.0, false), (256, 100.0, false), (256, 100.0, true), (256, 1000.0, false)] {
        for policy in ["none", "frustum", "distance", "distance+frustum", "distance+frustum+budget"] {
            let frames = if policy == "none" { 20 } else { 60 };
            let l = run_case(&model, &crowd(n, half, scales), policy, frames);
            println!(
                "RESULT avatars n={n} plaza_half_m={half} mixed_scales={scales} policy={policy} cpu_ms_per_frame={:.3} skinned_verts={} draws={} lights={} particles={} pose_rx_kBps={:.1} tiers_T0..T4={:?}",
                l.cpu_ms,
                l.verts,
                l.draws,
                l.lights,
                l.particles,
                l.pose_bps / 1e3,
                l.tiers
            );
        }
    }
}


/// Review R17-1: run a real region (4 ball pits with sweeping hands, the sim-relevant hand colliders)
/// and, every frame, a culling and presentation pass whose avatars stand at the hands. The pass gets
/// `&World` only. The region's per-frame hashes must be identical with no pass, with culling off and
/// with full culling.
pub fn sim_isolation() {
    use box3d_rust::body::body_get_position;
    use box3d_rust::recording::hash_world_state;
    use box3d_rust::world::World;
    let model = Model::new();
    let pass = |world: &World, r: &crate::players::Region, policy: &str, f: u64, states: &mut Vec<AvatarState>| -> f32 {
        let viewer = [0.0f64, 1.6, 0.0];
        let mut acc = 0.0;
        for (i, pit) in r.pits.iter().enumerate() {
            let p = pe_spike_chunks::phys::pos_f64(body_get_position(world, pit.hand));
            let d = ((p[0] - viewer[0]).powi(2) + (p[2] - viewer[2]).powi(2)).sqrt();
            let t = if policy == "none" { 0 } else { tier(1.7, d, p[0] > viewer[0]) };
            acc += present(&model, &mut states[i], t, f, i as u64);
        }
        acc
    };
    let mut digests = Vec::new();
    for policy in ["no-pass", "none", "distance+frustum"] {
        let mut r = crate::players::Region::new(4, 6.0, 0);
        let mut states = vec![AvatarState::new(); 4];
        let mut h = 0xcbf29ce484222325u64;
        let mut sink = 0.0;
        for f in 0..300u64 {
            r.step();
            if policy != "no-pass" {
                sink += pass(&r.world, &r, policy, f, &mut states);
            }
            h = (h ^ hash_world_state(&r.world)).wrapping_mul(0x100000001b3);
        }
        std::hint::black_box(sink);
        digests.push((policy, h));
    }
    let same = digests.iter().all(|d| d.1 == digests[0].1);
    println!("RESULT avatars-sim-isolation digests={:?} identical={same}", digests.iter().map(|d| format!("{}:{:016x}", d.0, d.1)).collect::<Vec<_>>());
    // Review R17-2: worst-case upload for one avatar that every other member sees at T0.
    for viewers in [15usize, 63, 255] {
        let up = viewers as f64 * POSE[0].0 * (POSE[0].1 + 52.0 / 8.0);
        println!("RESULT avatars-upload viewers_at_T0={viewers} upload_kBps={:.0} (computed)", up / 1e3);
    }
}
