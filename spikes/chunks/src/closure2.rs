//! Review N7: support closure on static bounds once per pass, one hop, misses gravel heaps and objects
//! thrown onto gravel. The crate (a 20 cm cube) is the relevant body throughout; no gravel is relevant.
//! Gravel is small cubes. A first version used a 0.9 m crate on 2.5 cm gravel: there even the all-dynamic
//! control let the crate sink to the ground, because box3d cannot hold a body up on bodies 1/47,000 of
//! its mass; these sizes keep the heap case inside the solver's range.
//!
//! * `heap`: the crate set down on a three-layer stepped heap of 5 cm gravel (mass ratio 64).
//! * `all-dynamic`: control, every object simulated from the start (what physics alone does).
//! * `thrown`: the crate thrown at 2 m/s from 0.3 m onto a bed of 1.2 cm gravel. Gravel is under r/16 of
//!   the crate (mass ratio 4,600), so the wake rule (25.4 rule 3) never applies; only closure brings it in.
//!
//! Rules (closure anchors are the relevant crate and awake dynamic bodies, as in 25.4 rule 4):
//! * `one-hop` (2a228c6): every 8 frames, dormant objects whose bounds touch an anchor's bounds become
//!   dynamic bodies.
//! * `swept+proxy` (revised): the same over each anchor's bounds swept by its velocity for the coming
//!   pass; and objects touching that first hop come in as **static proxies** (they collide, they are not
//!   simulated). A proxy becomes dynamic when it is itself in the first hop.

use std::collections::{BTreeMap, HashMap};

use box3d_rust::body::{body_get_position, destroy_body};
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::Quat;
use box3d_rust::world::World;

use pe_spike_chunks::phys::{
    make_body, mode, new_world, pos_f64, read_state, static_box, v3, world_def, BodyInit, BodyState, Shape, DT,
    SUBSTEPS,
};

const PASS: u64 = 8;
const CRATE_H: f64 = 0.1;
const CELL: f64 = 0.1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Dynamic,
    Proxy,
}

struct Obj {
    /// Half-extent of the cube.
    h: f64,
    state: BodyState,
    body: Option<(BodyId, Kind)>,
}

impl Obj {
    fn r(&self) -> f64 {
        self.h * 3f64.sqrt()
    }
}

fn st(p: [f64; 3], v: [f32; 3], awake: bool) -> BodyState {
    BodyState { p, q: [0.0, 0.0, 0.0, 1.0], v, w: [0.0; 3], awake }
}

fn create(w: &mut World, o: &mut Obj, kind: Kind) {
    let s = o.state;
    let b = match kind {
        Kind::Dynamic => {
            let mut b = BodyInit::at(s.p);
            b.rot = Quat { v: v3(s.q[0], s.q[1], s.q[2]), s: s.q[3] };
            b.lin = v3(s.v[0], s.v[1], s.v[2]);
            b.ang = v3(s.w[0], s.w[1], s.w[2]);
            b.awake = s.awake;
            make_body(w, &b, Shape::Cube(o.h as f32))
        }
        // Gravel at rest is axis-aligned here, so a static box at its position is its exact proxy.
        Kind::Proxy => static_box(w, s.p, [o.h as f32; 3]),
    };
    o.body = Some((b, kind));
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn key(p: [f64; 3]) -> (i64, i64, i64) {
    ((p[0] / CELL).floor() as i64, (p[1] / CELL).floor() as i64, (p[2] / CELL).floor() as i64)
}

/// Indices of objects within `reach` of `c` (bounds test done by the caller).
fn near(grid: &HashMap<(i64, i64, i64), Vec<usize>>, c: [f64; 3], reach: f64, out: &mut Vec<usize>) {
    out.clear();
    let (lo, hi) = (key([c[0] - reach, c[1] - reach, c[2] - reach]), key([c[0] + reach, c[1] + reach, c[2] + reach]));
    for x in lo.0..=hi.0 {
        for y in lo.1..=hi.1 {
            for z in lo.2..=hi.2 {
                if let Some(v) = grid.get(&(x, y, z)) {
                    out.extend_from_slice(v);
                }
            }
        }
    }
}

struct Res {
    crate_rest_y: f64,
    crate_min_y: f64,
    crate_final_y: f64,
    gravel_inside_crate: usize,
    gravel_sunk: usize,
    dynamic_max: usize,
    proxies_max: usize,
}

fn run(scene: &str, swept: bool, all_dynamic: bool) -> Res {
    let mut w = new_world(&world_def(1.0, 10.0, false));
    static_box(&mut w, [0.0, -0.5, 0.0], [50.0, 0.5, 50.0]);
    let mut objs: Vec<Obj> = Vec::new();
    let (gh, crate_rest_y) = if scene == "heap" {
        let h = 0.025;
        let pitch = 2.0 * h + 0.001;
        for layer in 0..3 {
            let n = 12 - 2 * layer;
            let half = (n as f64 - 1.0) * pitch / 2.0;
            let y = h + layer as f64 * (2.0 * h + 0.0005);
            for i in 0..n * n {
                let p = [(i % n) as f64 * pitch - half, y, (i / n) as f64 * pitch - half];
                objs.push(Obj { h, state: st(p, [0.0; 3], false), body: None });
            }
        }
        (h, 6.0 * h + 0.001 + CRATE_H)
    } else {
        let h = 0.006;
        let pitch = 2.0 * h + 0.001;
        for i in 0..(120 * 30) {
            let p = [(i % 120) as f64 * pitch - 0.3, h, (i / 120) as f64 * pitch - 0.25];
            objs.push(Obj { h, state: st(p, [0.0; 3], false), body: None });
        }
        (h, 2.0 * h + CRATE_H)
    };
    let crate_id = objs.len();
    let (cp, cv) = if scene == "heap" { ([0.0, crate_rest_y + 0.005, 0.0], [0.0f32; 3]) } else { ([-0.1, 0.3 + CRATE_H, 0.0], [2.0f32, 0.0, 0.0]) };
    objs.push(Obj { h: CRATE_H, state: st(cp, cv, true), body: None });
    let init_y: Vec<f64> = objs.iter().map(|o| o.state.p[1]).collect();

    let mut res = Res { crate_rest_y, crate_min_y: f64::INFINITY, crate_final_y: 0.0, gravel_inside_crate: 0, gravel_sunk: 0, dynamic_max: 0, proxies_max: 0 };
    let mut cand = Vec::new();
    for f in 0..300u64 {
        if f % PASS == 0 {
            for o in objs.iter_mut() {
                if let Some((b, Kind::Dynamic)) = o.body {
                    o.state = read_state(&w, b);
                }
            }
            let mut grid: HashMap<(i64, i64, i64), Vec<usize>> = HashMap::new();
            for (i, o) in objs.iter().enumerate() {
                if i != crate_id {
                    grid.entry(key(o.state.p)).or_default().push(i);
                }
            }
            let mut anchors: Vec<usize> = vec![crate_id];
            for (i, o) in objs.iter().enumerate() {
                if i != crate_id && matches!(o.body, Some((_, Kind::Dynamic))) && o.state.awake {
                    anchors.push(i);
                }
            }
            let mut want: BTreeMap<usize, Kind> = anchors.iter().map(|&a| (a, Kind::Dynamic)).collect();
            if all_dynamic {
                want = (0..objs.len()).map(|i| (i, Kind::Dynamic)).collect();
            }
            let t_pass = PASS as f64 / 60.0;
            for &a in &anchors {
                let s = objs[a].state;
                let (c, extra) = if swept {
                    let d = [s.v[0] as f64 * t_pass, s.v[1] as f64 * t_pass, s.v[2] as f64 * t_pass];
                    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                    ([s.p[0] + d[0] / 2.0, s.p[1] + d[1] / 2.0, s.p[2] + d[2] / 2.0], len / 2.0)
                } else {
                    (s.p, 0.0)
                };
                if all_dynamic {
                    break;
                }
                let reach = objs[a].r() + extra + 0.05;
                near(&grid, c, reach, &mut cand);
                for &j in &cand {
                    if j != a && dist(c, objs[j].state.p) <= objs[a].r() + extra + objs[j].r() + 0.005 {
                        want.insert(j, Kind::Dynamic);
                    }
                }
            }
            if swept && !all_dynamic {
                let first: Vec<usize> = want.keys().copied().filter(|&i| i != crate_id).collect();
                for &a in &first {
                    near(&grid, objs[a].state.p, objs[a].r() * 2.0 + 0.01, &mut cand);
                    for &j in &cand {
                        if !want.contains_key(&j) && dist(objs[a].state.p, objs[j].state.p) <= objs[a].r() + objs[j].r() + 0.003 {
                            want.insert(j, Kind::Proxy);
                        }
                    }
                }
            }
            for i in 0..objs.len() {
                let have = objs[i].body.map(|b| b.1);
                let need = want.get(&i).copied();
                if have == need {
                    continue;
                }
                if let Some((b, k)) = objs[i].body.take() {
                    if k == Kind::Dynamic {
                        objs[i].state = read_state(&w, b);
                    }
                    destroy_body(&mut w, b);
                }
                if let Some(k) = need {
                    create(&mut w, &mut objs[i], k);
                }
            }
            res.dynamic_max = res.dynamic_max.max(want.values().filter(|&&k| k == Kind::Dynamic).count());
            res.proxies_max = res.proxies_max.max(want.values().filter(|&&k| k == Kind::Proxy).count());
        }
        w.step(DT, SUBSTEPS);
        if let Some((b, _)) = objs[crate_id].body {
            let y = pos_f64(body_get_position(&w, b))[1];
            if f > 45 || scene == "heap" {
                res.crate_min_y = res.crate_min_y.min(y);
            }
        }
    }
    let at = |o: &Obj| match o.body {
        Some((b, _)) => pos_f64(body_get_position(&w, b)),
        None => o.state.p,
    };
    let cpos = at(&objs[crate_id]);
    res.crate_final_y = cpos[1];
    for i in 0..crate_id {
        let p = at(&objs[i]);
        res.gravel_inside_crate += (0..3).all(|k| (p[k] - cpos[k]).abs() < CRATE_H) as usize;
        res.gravel_sunk += (init_y[i] - p[1] > gh) as usize;
    }
    res
}

pub fn run_all() {
    println!("== closure2 (review N7): gravel heap and crate thrown onto gravel ({}) ==", mode());
    let scene_arg = std::env::args().nth(2);
    for scene in ["heap", "thrown"] {
        if scene_arg.as_deref().is_some_and(|a| a != scene) {
            continue;
        }
        for (rule, swept, all) in [("all-dynamic (control)", false, true), ("one-hop", false, false), ("swept+proxy", true, false)] {
            let r = run(scene, swept, all);
            println!(
                "RESULT closure2 mode={} scene={scene} rule=\"{rule}\" crate_rest_y_on_gravel={:.3} crate_min_y={:.3} crate_final_y={:.3} gravel_inside_crate={} gravel_sunk_more_than_half_extent={} dynamic_max={} proxies_max={}",
                mode(),
                r.crate_rest_y,
                r.crate_min_y,
                r.crate_final_y,
                r.gravel_inside_crate,
                r.gravel_sunk,
                r.dynamic_max,
                r.proxies_max
            );
        }
    }
}
