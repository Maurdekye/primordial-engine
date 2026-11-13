//! Measurement 1 (docs/spec/logic.md section 11): the benchmark's game logic as six systems over host-owned
//! columns, the batch ABI of logic.md 3.3, against the old shape (the world kept in guest memory).
//!
//!   logic-probe cols <cols_guest.wasm>
//!
//! Per frame and per system, in tick order: the host copies the columns the system declares (reads and writes)
//! into the guest's fixed slots, calls `pe_run`, then for each written column compares every row with its own
//! copy, validates the changed rows (finite floats, booleans 0 or 1, object ids in range) and copies back only
//! those. The hit list is an event buffer one system writes and another reads.
//!
//! Correctness, every mode: the final columns (all bytes, plus the hits) equal those of the same split run with
//! the world left in guest memory, and the guest's own hash equals the native split run's. `poison` also
//! overwrites every undeclared column with 0xA5 before each call, so an undeclared read changes the result; an
//! undeclared write is lost, so it does too.

use crate::{BIG, G, HostState, Meter, N, SEED, engine, linker, ms, prepare, read, stats};
use pe_api::{World, fnv};
use std::time::Instant;
use wasmtime::Module;

const WARM: u64 = 30;
const FRAMES: u64 = 150;

// Column ids, as the guest's pe_col.
const ALIVE: u8 = 0;
const KIND: u8 = 1;
const TEAM: u8 = 2;
const POS: u8 = 3;
const VEL: u8 = 4;
const HP: u8 = 5;
const MAX_HP: u8 = 6;
const ARMOR: u8 = 7;
const AI_STATE: u8 = 8;
const AI_TIMER: u8 = 9;
const TARGET: u8 = 10;
const COOLDOWN: u8 = 11;
const INV: u8 = 12;
const EFFECTS: u8 = 13;
const RNG: u8 = 14;
const STATS: u8 = 15;
const HITS: u8 = 16;
const NCOL: usize = 17;
/// Bytes per block in the host's changed-row diff.
const BLOCK: usize = 256;
const NAMES: [&str; NCOL] = [
    "alive", "kind", "team", "pos", "vel", "hp", "max_hp", "armor", "ai_state", "ai_timer", "target", "cooldown",
    "inv", "effects", "rng", "stats", "hits",
];

/// One system's manifest entry: its bit in logic_game, what it reads and what it writes.
struct Sys {
    name: &'static str,
    bit: u32,
    reads: &'static [u8],
    writes: &'static [u8],
}

// Every system rebuilds the spatial grid from pos and alive at its start (logic_game::tick), so both are reads
// of all six. Sets from the generated code's field uses (`w.<field>`, the query helpers, respawn, add_effect),
// checked by `poison`.
const SYSTEMS: [Sys; 6] = [
    Sys {
        name: "spawn",
        bit: 16,
        reads: &[ALIVE, POS, INV, RNG, STATS],
        writes: &[
            ALIVE, KIND, TEAM, POS, VEL, HP, MAX_HP, ARMOR, AI_STATE, AI_TIMER, TARGET, COOLDOWN, INV, EFFECTS, RNG,
            STATS,
        ],
    },
    Sys {
        name: "ai",
        bit: 2,
        reads: &[ALIVE, POS, TEAM, KIND, HP, MAX_HP, VEL, AI_STATE, AI_TIMER, TARGET, COOLDOWN, RNG, HITS],
        writes: &[VEL, AI_STATE, AI_TIMER, TARGET, COOLDOWN, RNG, HITS],
    },
    Sys {
        name: "combat",
        bit: 4,
        reads: &[ALIVE, POS, VEL, HP, MAX_HP, ARMOR, COOLDOWN, INV, EFFECTS, RNG, STATS, HITS],
        writes: &[ALIVE, VEL, HP, ARMOR, COOLDOWN, INV, EFFECTS, RNG, STATS, HITS],
    },
    Sys {
        name: "effects",
        bit: 32,
        reads: &[ALIVE, POS, VEL, HP, MAX_HP, ARMOR, EFFECTS],
        writes: &[VEL, HP, ARMOR, EFFECTS],
    },
    Sys { name: "inventory", bit: 8, reads: &[ALIVE, POS, KIND, INV, RNG, STATS], writes: &[INV, RNG, STATS] },
    Sys {
        name: "movement",
        bit: 1,
        reads: &[ALIVE, POS, KIND, VEL, HP, MAX_HP, TARGET],
        writes: &[POS, VEL, HP],
    },
];

/// Row size in bytes of each column, for the changed-row diff (the singletons are one row).
fn row_bytes(id: u8, len: usize) -> usize {
    match id {
        RNG | STATS => len,
        HITS => 16,
        _ => len / N as usize,
    }
}

/// The host's validation of a changed row, as the store does for any row (logic.md 3.3, step 1).
fn valid(id: u8, row: &[u8]) -> bool {
    match id {
        POS | VEL => row.chunks(4).all(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).is_finite()),
        ALIVE => row[0] <= 1,
        TARGET => {
            let t = u32::from_le_bytes([row[0], row[1], row[2], row[3]]);
            t < N || t == u32::MAX
        }
        _ => true,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// The world stays in guest memory; one call per system, nothing copied. The reference for the bytes.
    Resident,
    /// Host-owned columns, copied per system as declared.
    Columns,
    /// Columns, with every undeclared column poisoned before each call (correctness only, not timed).
    Poison,
    /// Columns, written back by block instead of by row: a block of rows that differs is validated (only the
    /// columns that need it: floats, booleans, object ids) and copied whole.
    Blocks,
    /// Blocks, and the guest's whole memory restored from its post-init image at the start of every frame
    /// (the SDK's fixed heap with a copy reset, logic.md 5.1).
    BlocksReset,
    /// Blocks, and a declared column is copied in only if the guest's slot does not already hold the host's
    /// current version (option (c) of 3.3a). Sound only if a script cannot write a slot outside its write set:
    /// the script crate is `forbid(unsafe_code)` and the SDK hands read columns out as shared slices (4.1).
    BlocksSkip,
    /// BlocksSkip with the per-frame memory reset: every slot is stale at the frame start.
    BlocksSkipReset,
}

fn needs_validation(id: u8) -> bool {
    matches!(id, POS | VEL | ALIVE | TARGET)
}

fn unpack(v: i64) -> (usize, usize) {
    ((v as u64 & 0xffff_ffff) as usize, (v as u64 >> 32) as usize)
}

struct Out {
    times: Vec<f64>,
    copy_in: f64,
    run: f64,
    back: f64,
    reset: f64,
    back_sys: [f64; 6],
    changed_rows: [u64; 6],
    copied_in_bytes: u64,
    bad_rows: u64,
    cols_digest: u64,
    guest_digest: u64,
    fuel_per_tick: u64,
    mem_bytes: usize,
}

fn run(wasm: &[u8], meter: Meter, nan: bool, shape: Shape) -> Out {
    let e = engine(meter, nan, false, 8 << 20);
    let m = Module::new(&e, &prepare(wasm, meter)).unwrap();
    let pre = linker(&e).instantiate_pre(&m).unwrap();
    let mut g = G::new(&pre, &e, HostState::new(), meter);
    g.call::<(i32, i64), ()>("pe_init", (N as i32, SEED as i64));
    let mem = g.inst.get_memory(&mut g.store, "memory").unwrap();
    // The guest's slots (fixed after init: the vectors never change length) and the host's own copy.
    let mut slot = [(0usize, 0usize); NCOL];
    let mut host: Vec<Vec<u8>> = vec![Vec::new(); NCOL];
    for id in 0..HITS {
        slot[id as usize] = unpack(g.call::<i32, i64>("pe_col", id as i32));
        let (p, l) = slot[id as usize];
        host[id as usize] = mem.data(&g.store)[p..p + l].to_vec();
    }
    let image = mem.data(&g.store).to_vec();
    let mut o = Out {
        times: Vec::new(),
        copy_in: 0.0,
        run: 0.0,
        back: 0.0,
        reset: 0.0,
        back_sys: [0.0; 6],
        changed_rows: [0; 6],
        copied_in_bytes: 0,
        bad_rows: 0,
        cols_digest: 0,
        guest_digest: 0,
        fuel_per_tick: 0,
        mem_bytes: image.len(),
    };
    let skip = matches!(shape, Shape::BlocksSkip | Shape::BlocksSkipReset);
    let mut host_ver = [0u64; NCOL];
    let mut guest_ver = [u64::MAX; NCOL];
    let mut run_fuel = 0u64; // pe_run calls only, not the host's helper calls
    for f in 0..WARM + FRAMES {
        let keep = f >= WARM;
        let t0 = Instant::now();
        if matches!(shape, Shape::BlocksSkip | Shape::BlocksSkipReset) && f == 0 || shape == Shape::BlocksSkipReset {
            guest_ver = [u64::MAX; NCOL];
        }
        if matches!(shape, Shape::BlocksReset | Shape::BlocksSkipReset) {
            let t = Instant::now();
            mem.data_mut(&mut g.store)[..image.len()].copy_from_slice(&image);
            if keep {
                o.reset += ms(t.elapsed());
            }
        }
        g.fuel_used = 0;
        for (si, s) in SYSTEMS.iter().enumerate() {
            if shape == Shape::Resident {
                let t = Instant::now();
                let before = g.fuel_used;
                g.call::<(i32, i64), i32>("pe_run", (s.bit as i32, f as i64));
                if keep {
                    o.run += ms(t.elapsed());
                    run_fuel += g.fuel_used - before;
                }
                continue;
            }
            // Copy in.
            let t = Instant::now();
            let declared = |id: u8| s.reads.contains(&id) || s.writes.contains(&id);
            for id in 0..HITS {
                let (p, l) = slot[id as usize];
                let d = &mut mem.data_mut(&mut g.store)[p..p + l];
                if declared(id) {
                    if skip && guest_ver[id as usize] == host_ver[id as usize] {
                        continue;
                    }
                    d.copy_from_slice(&host[id as usize]);
                    guest_ver[id as usize] = host_ver[id as usize];
                    if keep {
                        o.copied_in_bytes += l as u64;
                    }
                } else if shape == Shape::Poison {
                    d.fill(0xA5);
                }
            }
            let hits_in = if declared(HITS) { host[HITS as usize].len() / 16 } else { 0 };
            let (hp, hl) = unpack(g.call::<i32, i64>("pe_hits_set", hits_in as i32));
            if declared(HITS) {
                mem.data_mut(&mut g.store)[hp..hp + hl].copy_from_slice(&host[HITS as usize]);
            }
            if keep {
                o.copy_in += ms(t.elapsed());
            }
            // Run.
            let t = Instant::now();
            let before = g.fuel_used;
            assert_eq!(g.call::<(i32, i64), i32>("pe_run", (s.bit as i32, f as i64)), 0);
            if keep {
                o.run += ms(t.elapsed());
                run_fuel += g.fuel_used - before;
            }
            // Diff, validate, write back the changed rows.
            let t = Instant::now();
            for &id in s.writes {
                let (p, l) = if id == HITS { unpack(g.call::<(), i64>("pe_hits", ())) } else { slot[id as usize] };
                let d = &mem.data(&g.store)[p..p + l];
                if id == HITS {
                    // An event buffer: taken whole.
                    host[HITS as usize].clear();
                    host[HITS as usize].extend_from_slice(d);
                    continue;
                }
                let rb = row_bytes(id, l);
                let h = &mut host[id as usize];
                let mut changed = false;
                // Compare whole blocks of rows first (one memcmp each), rows only inside a block that differs.
                let blk = rb * (BLOCK / rb).max(1);
                for (da, hb) in d.chunks(blk).zip(h.chunks_mut(blk)) {
                    if da == &hb[..] {
                        continue;
                    }
                    changed = true;
                    if matches!(shape, Shape::Blocks | Shape::BlocksReset | Shape::BlocksSkip | Shape::BlocksSkipReset) {
                        if needs_validation(id) {
                            o.bad_rows += da.chunks_exact(rb).filter(|r| !valid(id, r)).count() as u64;
                        }
                        hb.copy_from_slice(da);
                        if keep && id != RNG && id != STATS {
                            o.changed_rows[si] += 1; // blocks, in this shape
                        }
                        continue;
                    }
                    for (a, b) in da.chunks_exact(rb).zip(hb.chunks_exact_mut(rb)) {
                        if a != b {
                            if !valid(id, a) {
                                o.bad_rows += 1;
                            }
                            b.copy_from_slice(a);
                            if keep && id != RNG && id != STATS {
                                o.changed_rows[si] += 1;
                            }
                        }
                    }
                }
                // The guest's slot now equals the host's copy.
                if changed {
                    host_ver[id as usize] += 1;
                }
                guest_ver[id as usize] = host_ver[id as usize];
            }
            if keep {
                let dt = ms(t.elapsed());
                o.back += dt;
                o.back_sys[si] += dt;
            }
        }
        if keep {
            o.times.push(ms(t0.elapsed()));
        }
    }
    o.fuel_per_tick = run_fuel / FRAMES;
    if shape == Shape::Resident {
        for id in 0..HITS {
            let (p, l) = slot[id as usize];
            host[id as usize] = mem.data(&g.store)[p..p + l].to_vec();
        }
        let (p, l) = unpack(g.call::<(), i64>("pe_hits", ()));
        host[HITS as usize] = mem.data(&g.store)[p..p + l].to_vec();
    } else {
        // Copy everything in once, so the guest's own hash sees the host's columns.
        for id in 0..HITS {
            let (p, l) = slot[id as usize];
            mem.data_mut(&mut g.store)[p..p + l].copy_from_slice(&host[id as usize]);
        }
        let (hp, hl) = unpack(g.call::<i32, i64>("pe_hits_set", (host[HITS as usize].len() / 16) as i32));
        mem.data_mut(&mut g.store)[hp..hp + hl].copy_from_slice(&host[HITS as usize]);
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for c in &host {
        h = fnv(h, c.len() as u64);
        for w in c.chunks(8) {
            let mut b = [0u8; 8];
            b[..w.len()].copy_from_slice(w);
            h = fnv(h, u64::from_le_bytes(b));
        }
    }
    o.cols_digest = h;
    o.guest_digest = g.digest();
    let _ = BIG;
    o
}

/// The same split (one tick call per system) natively: the reference for the guest's hash, and its time.
fn native_split() -> (Vec<f64>, u64) {
    let mut w = World::new(N as usize, SEED);
    let mut h = HostState::new();
    let mut t = Vec::new();
    for f in 0..WARM + FRAMES {
        let s = Instant::now();
        for sys in &SYSTEMS {
            logic_game::tick(&mut w, f, sys.bit, &mut h);
        }
        if f >= WARM {
            t.push(ms(s.elapsed()));
        }
    }
    (t, fnv(fnv(w.hash(), h.emit_hash), h.emits))
}

pub fn cols(path: &str) {
    let wasm = read(path);
    for s in &SYSTEMS {
        let names = |v: &[u8]| v.iter().map(|&i| format!("\"{}\"", NAMES[i as usize])).collect::<Vec<_>>().join(",");
        println!(
            "{{\"kind\":\"manifest\",\"system\":\"{}\",\"reads\":[{}],\"writes\":[{}]}}",
            s.name,
            names(s.reads),
            names(s.writes)
        );
    }
    let (whole, _) = crate::native();
    let (nt, nd) = native_split();
    let (nmed, n10, n90) = stats(nt);
    let (wmed, _, _) = stats(whole);
    println!(
        "{{\"kind\":\"cols\",\"mode\":\"native-split\",\"n\":{N},\"median_ms\":{nmed:.4},\"p10\":{n10:.4},\"p90\":{n90:.4},\"native_one_call_median_ms\":{wmed:.4},\"digest\":\"{nd:016x}\"}}"
    );
    let modes: &[(&str, Meter, bool)] = &[
        ("cl", Meter::None, false),
        ("cl-inj-nan", Meter::Injected, true),
        ("cl-inj-nanobs", Meter::InjectedNanObs, false),
    ];
    for &(mname, meter, nan) in modes {
        let reference = run(&wasm, meter, nan, Shape::Resident);
        for (sname, shape) in [
            ("resident", Shape::Resident),
            ("columns", Shape::Columns),
            ("blocks", Shape::Blocks),
            ("blocks-reset", Shape::BlocksReset),
            ("blocks-skip", Shape::BlocksSkip),
            ("blocks-skip-reset", Shape::BlocksSkipReset),
            ("poison", Shape::Poison),
        ] {
            let o = run(&wasm, meter, nan, shape);
            let (med, p10, p90) = stats(o.times.clone());
            let per = |x: f64| x / FRAMES as f64;
            println!(
                "{{\"kind\":\"cols\",\"mode\":\"{mname}\",\"shape\":\"{sname}\",\"n\":{N},\"median_ms\":{med:.4},\"p10\":{p10:.4},\"p90\":{p90:.4},\"x_native_split\":{:.2},\"mean_copy_in_ms\":{:.4},\"mean_run_ms\":{:.4},\"mean_back_ms\":{:.4},\"mean_reset_ms\":{:.4},\"mean_back_ms_by_system\":[{}],\"copied_in_bytes_per_frame\":{},\"changed_rows_or_blocks_per_frame_by_system_summed_over_written_columns\":[{}],\"bad_rows\":{},\"fuel_per_tick\":{},\"mem_bytes\":{},\"cols_eq_resident\":{},\"guest_digest_eq_native_split\":{},\"fuel_eq_resident\":{}}}",
                med / nmed,
                per(o.copy_in),
                per(o.run),
                per(o.back),
                per(o.reset),
                o.back_sys.iter().map(|x| format!("{:.4}", per(*x))).collect::<Vec<_>>().join(","),
                o.copied_in_bytes / FRAMES,
                o.changed_rows.iter().map(|c| (c / FRAMES).to_string()).collect::<Vec<_>>().join(","),
                o.bad_rows,
                o.fuel_per_tick,
                o.mem_bytes,
                o.cols_digest == reference.cols_digest,
                o.guest_digest == nd,
                o.fuel_per_tick == reference.fuel_per_tick,
            );
        }
    }
}
