//! Review R16-2: activation must be closed under support and must not freeze awake bodies.
//!
//! Scene: a 0.9 m crate resting on a bed of 3 cm stones, and a 1.5 cm coin knocked off a 1 m
//! table at frame 0. The player's anchor stands 20 m away and walks in at 4 m/s from t = 3 s, ending
//! beside the crate (stones within 0.96 m of the anchor become relevant there). Neither the stones
//! nor the coin is ever relevant at 20 m (`r < h/64`, and `32 r` is well under the distance), the crate is.
//!
//! * `relevance-only`: the rule as committed at 45d4940 (25.4 rules 1-2): active = relevant, plus the
//!   coin's wake at frame 0; everything else is frozen out of the box3d world at each pass.
//! * `closed`: the revised rule: (a) a dormant object whose bounds touch a relevant or awake active
//!   body is kept (support closure, one hop); (b) an awake body stays active until it sleeps, inside a
//!   hard reach of 2 x the relevance reach cap.

use std::collections::BTreeSet;

use box3d_rust::body::{body_get_linear_velocity, body_get_position, body_is_awake, destroy_body};
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::Quat;
use box3d_rust::world::World;

use pe_spike_chunks::phys::{
    make_body, mode, new_world, pos_f64, read_state, static_box, v3, world_def, BodyInit, BodyState, Shape, DT,
    SUBSTEPS,
};

const RATIO: f64 = 32.0;
const H: f64 = 1.7;

#[derive(Clone)]
struct Obj {
    r: f64,
    cube: bool,
    state: BodyState,
    body: Option<BodyId>,
}

fn init_state(p: [f64; 3], v: [f32; 3], awake: bool) -> BodyState {
    BodyState { p, q: [0.0, 0.0, 0.0, 1.0], v, w: [0.0; 3], awake }
}

struct Outcome {
    crate_min_y: f64,
    crate_final_y: f64,
    coin_final_y: f64,
    coin_frozen_midair: bool,
    max_speed_after_approach: f64,
    active_max: usize,
    stones_inside_crate: usize,
}

fn run(closed: bool) -> Outcome {
    let mut w: World = new_world(&world_def(1.0, 10.0, false));
    static_box(&mut w, [0.0, -0.5, 0.0], [50.0, 0.5, 50.0]);
    static_box(&mut w, [3.0, 0.5, 0.0], [0.5, 0.5, 0.5]); // table, top at y = 1
    let mut objs: Vec<Obj> = Vec::new();
    // Stone bed: 12 x 12 stones of r = 3 cm, 10 cm apart. Relevant only within 0.96 m.
    for i in 0..144 {
        let (x, z) = ((i % 12) as f64 * 0.1 - 0.55, (i / 12) as f64 * 0.1 - 0.55);
        objs.push(Obj { r: 0.03, cube: false, state: init_state([x, 0.03, z], [0.0; 3], false), body: None });
    }
    // Crate (half-extent 0.45, bounding radius 0.78) just placed on the stones (awake).
    let crate_id = objs.len();
    objs.push(Obj { r: 0.78, cube: true, state: init_state([0.0, 0.06 + 0.45, 0.0], [0.0; 3], true), body: None });
    // Coin knocked off the table edge at 1 m/s.
    let coin_id = objs.len();
    objs.push(Obj { r: 0.015, cube: false, state: init_state([3.49, 1.015, 0.0], [1.0, 0.0, 0.0], true), body: None });

    let create = |w: &mut World, o: &mut Obj| {
        let s = o.state;
        let mut b = BodyInit::at(s.p);
        b.rot = Quat { v: v3(s.q[0], s.q[1], s.q[2]), s: s.q[3] };
        b.lin = v3(s.v[0], s.v[1], s.v[2]);
        b.ang = v3(s.w[0], s.w[1], s.w[2]);
        b.awake = s.awake;
        let shape = if o.cube { Shape::Cube(0.45) } else { Shape::Ball(o.r as f32) };
        o.body = Some(make_body(w, &b, shape));
    };
    let mut active: BTreeSet<usize> = BTreeSet::new();
    let mut out = Outcome {
        crate_min_y: f64::INFINITY,
        crate_final_y: 0.0,
        coin_final_y: 0.0,
        coin_frozen_midair: false,
        max_speed_after_approach: 0.0,
        active_max: 0,
        stones_inside_crate: 0,
    };
    for f in 0..600u64 {
        let t = f as f64 / 60.0;
        let px = if t < 3.0 { 20.0 } else { (20.0 - 4.0 * (t - 3.0)).max(0.3) };
        let player = [px, 0.9, 0.0];
        if f % 8 == 0 {
            // Current states of active bodies.
            for &i in &active {
                objs[i].state = read_state(&w, objs[i].body.unwrap());
            }
            let dist = |a: [f64; 3], b: [f64; 3]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
            let mut want: BTreeSet<usize> = (0..objs.len())
                .filter(|&i| objs[i].r >= H / 64.0 && dist(player, objs[i].state.p) <= RATIO * objs[i].r)
                .collect();
            if f == 0 {
                want.insert(coin_id); // woken by the knock (rule 2)
            }
            if closed {
                // (b) awake bodies stay active until they sleep, inside the hard reach.
                for &i in &active {
                    if objs[i].state.awake && dist(player, objs[i].state.p) <= 4_000.0 {
                        want.insert(i);
                    }
                }
                // (a) one-hop support closure around relevant-or-awake active bodies.
                let anchors: Vec<usize> = want.iter().copied().collect();
                for a in anchors {
                    for j in 0..objs.len() {
                        if dist(objs[a].state.p, objs[j].state.p) <= objs[a].r + objs[j].r + 0.01 {
                            want.insert(j);
                        }
                    }
                }
            }
            for &i in active.difference(&want).copied().collect::<Vec<_>>().iter() {
                let b = objs[i].body.take().unwrap();
                objs[i].state = read_state(&w, b);
                destroy_body(&mut w, b);
            }
            for &i in want.difference(&active).copied().collect::<Vec<_>>().iter() {
                create(&mut w, &mut objs[i]);
            }
            active = want;
            out.active_max = out.active_max.max(active.len());
        }
        w.step(DT, SUBSTEPS);
        if let Some(b) = objs[crate_id].body {
            out.crate_min_y = out.crate_min_y.min(pos_f64(body_get_position(&w, b))[1]);
        }
        if t > 3.0 {
            for &i in &active {
                let v = body_get_linear_velocity(&w, objs[i].body.unwrap());
                let s = ((v.x * v.x + v.y * v.y + v.z * v.z) as f64).sqrt();
                if i != coin_id {
                    out.max_speed_after_approach = out.max_speed_after_approach.max(s);
                }
            }
        }
    }
    let y = |i: usize, objs: &Vec<Obj>, w: &World| match objs[i].body {
        Some(b) => (pos_f64(body_get_position(w, b))[1], body_is_awake(w, b)),
        None => (objs[i].state.p[1], false),
    };
    out.crate_final_y = y(crate_id, &objs, &w).0;
    // Interpenetration left behind: stones whose centre lies inside the crate's volume.
    let bottom = out.crate_final_y - 0.45;
    out.stones_inside_crate = (0..crate_id)
        .filter(|&i| {
            let p = match objs[i].body {
                Some(b) => pos_f64(body_get_position(&w, b)),
                None => objs[i].state.p,
            };
            p[0].abs() < 0.45 && p[2].abs() < 0.45 && p[1] > bottom
        })
        .count();
    let (cy, _) = y(coin_id, &objs, &w);
    out.coin_final_y = cy;
    out.coin_frozen_midair = cy > 0.05;
    out
}

pub fn run_all() {
    println!("== closure (review R16-2): support and awake bodies ({}) ==", mode());
    for (name, closed) in [("relevance-only", false), ("closed", true)] {
        let o = run(closed);
        println!(
            "RESULT closure mode={} rule={name} crate_rest_y=0.510 crate_min_y={:.3} crate_final_y={:.3} coin_final_y={:.3} coin_frozen_midair={} max_body_speed_during_approach_mps={:.2} active_max={} stones_inside_crate={}",
            mode(),
            o.crate_min_y,
            o.crate_final_y,
            o.coin_final_y,
            o.coin_frozen_midair,
            o.max_speed_after_approach,
            o.active_max,
            o.stones_inside_crate
        );
    }
}
