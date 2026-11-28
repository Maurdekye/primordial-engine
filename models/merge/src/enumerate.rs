//! Every input within a profile's bounds: main's final log, main's entries past `F_M`, the returner's entries, which
//! main entries the returner received, which returning entries carry a witness statement, the contact frame, and
//! the tie rank at each stamp the returner and main share (both ways). Lists are generated in non-decreasing order,
//! so each multiset of entries comes once.

use crate::merge::{A, Entry, F_M, Input, Verdict};
use crate::world::{MAIN, Obj, Op, RETURNER};

/// The bounds of one enumeration.
#[derive(Clone, Debug)]
pub struct Profile {
    pub name: &'static str,
    pub ops: Vec<Op>,
    pub max_o: usize,
    pub max_r: usize,
    pub max_o2: usize,
    /// The most entries in one input, all lists together.
    pub max_total: usize,
    /// Whether contact is enumerated (else never on).
    pub contact: bool,
    /// Whether main's entries may come from both main members (else member 0 only).
    pub two_main: bool,
}

impl Profile {
    /// The tier-1 profile: a small op alphabet, at most three entries, contact off or on at one frame. Gravity and a
    /// precondition are in it, a create under each body and a reparent (P-M5's), and admin changes of the returner,
    /// of main's member 0 (an admin who then acts) and of member 3 (an add against a remove, 9.6).
    pub fn tier1() -> Profile {
        Profile {
            name: "tier1",
            ops: vec![
                Op::Grab(Obj::P),
                Op::Write(Obj::P, 1),
                Op::WriteIf(Obj::P, 0, 1),
                Op::Write(Obj::X, 1),
                Op::WriteG(1),
                Op::Delete(Obj::P),
                Op::Create(Obj::P),
                Op::Create(Obj::X),
                Op::Reparent(Obj::P),
                Op::Attach(Obj::P),
                Op::RemoveAdmin(RETURNER),
                Op::RemoveAdmin(0),
                Op::RemoveAdmin(3),
                Op::AddAdmin(3),
            ],
            max_o: 2,
            max_r: 2,
            max_o2: 1,
            max_total: 3,
            contact: true,
            two_main: false,
        }
    }

    /// Step 2's profile: four entries over a small alphabet, contact off, so that two chains of unwitnessed runs
    /// meet, one spending its budget (the `GlobalBudget` control) or one deciding the other's pass (`StaleAdmSoFar`).
    pub fn step2() -> Profile {
        Profile {
            name: "step2",
            ops: vec![Op::Grab(Obj::P), Op::Write(Obj::P, 1), Op::Delete(Obj::P), Op::Write(Obj::X, 1)],
            max_o: 2,
            max_r: 2,
            max_o2: 0,
            max_total: 4,
            contact: false,
            two_main: false,
        }
    }

    /// The full profile: every op of the sketch on every object, two main members.
    pub fn full() -> Profile {
        let mut ops = Vec::new();
        for o in [Obj::P, Obj::C, Obj::X] {
            ops.extend([
                Op::Grab(o),
                Op::Release(o),
                Op::Write(o, 1),
                Op::WriteIf(o, 0, 1),
                Op::Delete(o),
                Op::Attach(o),
            ]);
        }
        ops.extend([
            Op::Create(Obj::P),
            Op::Create(Obj::X),
            Op::Reparent(Obj::P),
            Op::Reparent(Obj::X),
            Op::WriteG(1),
            Op::RemoveAdmin(RETURNER),
            Op::RemoveAdmin(0),
            Op::RemoveAdmin(3),
            Op::AddAdmin(3),
        ]);
        // Three entries in all: at four the space passes 10^11 inputs.
        Profile { name: "full", ops, max_o: 2, max_r: 2, max_o2: 1, max_total: 3, contact: true, two_main: true }
    }
}

/// Non-decreasing sequences of length `n` over `0..items`.
fn sequences(items: usize, n: usize, out: &mut Vec<Vec<usize>>) {
    fn go(items: usize, n: usize, from: usize, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if cur.len() == n {
            out.push(cur.clone());
            return;
        }
        for i in from..items {
            cur.push(i);
            go(items, n, i, cur, out);
            cur.pop();
        }
    }
    go(items, n, 0, &mut Vec::new(), out);
}

/// Calls `f` on every input of `p`, in a fixed order; `shard` of `shards` takes every `shards`-th input.
pub fn for_each(p: &Profile, shard: usize, shards: usize, mut f: impl FnMut(&Input)) {
    let authors: Vec<u8> = if p.two_main { MAIN.to_vec() } else { vec![MAIN[0]] };
    let mut main_kinds = Vec::new();
    for stamp in 0..F_M {
        for &author in &authors {
            for &op in &p.ops {
                for verdict in [Verdict::Valid, Verdict::Late] {
                    main_kinds.push((stamp, author, op, verdict));
                }
            }
        }
    }
    let mut main2_kinds = Vec::new();
    for stamp in F_M..A {
        for &author in &authors {
            for &op in &p.ops {
                main2_kinds.push((stamp, author, op));
            }
        }
    }
    let mut ret_kinds = Vec::new();
    for stamp in 0..F_M {
        for &op in &p.ops {
            ret_kinds.push((stamp, op));
        }
    }
    let contacts: Vec<Option<u8>> =
        if p.contact { std::iter::once(None).chain((0..A).map(Some)).collect() } else { vec![None] };
    let mut n = 0usize;
    for n_o in 0..=p.max_o {
        for n_r in 0..=p.max_r {
            for n_o2 in 0..=p.max_o2 {
                if n_o + n_r + n_o2 > p.max_total || n_o + n_r == 0 {
                    continue;
                }
                let (mut so, mut sr, mut so2) = (Vec::new(), Vec::new(), Vec::new());
                sequences(main_kinds.len(), n_o, &mut so);
                sequences(ret_kinds.len(), n_r, &mut sr);
                sequences(main2_kinds.len(), n_o2, &mut so2);
                for o in &so {
                    let mut seqs = [0u8; 4];
                    let e_o: Vec<(Entry, Verdict)> = o
                        .iter()
                        .map(|&k| {
                            let (stamp, author, op, verdict) = main_kinds[k];
                            seqs[author as usize] += 1;
                            (Entry { stamp, author, seq: seqs[author as usize], op }, verdict)
                        })
                        .collect();
                    for r in &sr {
                        let own: Vec<Entry> = r
                            .iter()
                            .enumerate()
                            .map(|(i, &k)| {
                                let (stamp, op) = ret_kinds[k];
                                Entry { stamp, author: RETURNER, seq: i as u8 + 1, op }
                            })
                            .collect();
                        for o2 in &so2 {
                            let mut seqs2 = seqs;
                            let e_o2: Vec<Entry> = o2
                                .iter()
                                .map(|&k| {
                                    let (stamp, author, op) = main2_kinds[k];
                                    seqs2[author as usize] += 1;
                                    Entry { stamp, author, seq: seqs2[author as usize], op }
                                })
                                .collect();
                            // The stamps where a returner's entry and a main entry meet: the tie rank there is
                            // enumerated both ways (each subset of them); elsewhere it orders nothing. Main's
                            // entries past F_M never meet the returner's, which are stamped below it.
                            let shared: Vec<u8> = (0..F_M)
                                .filter(|&s| own.iter().any(|e| e.stamp == s) && e_o.iter().any(|(e, _)| e.stamp == s))
                                .collect();
                            let ties: Vec<u8> = (0u32..(1 << shared.len()))
                                .map(|bits| {
                                    shared
                                        .iter()
                                        .enumerate()
                                        .filter(|(i, _)| bits & (1 << i) != 0)
                                        .fold(0u8, |t, (_, s)| t | (1 << s))
                                })
                                .collect();
                            for received in 0u32..(1 << e_o.len()) {
                                let mut e_t = own.clone();
                                for (i, (e, _)) in e_o.iter().enumerate() {
                                    if received & (1 << i) != 0 {
                                        e_t.push(*e);
                                    }
                                }
                                // Witness bits over the returning candidates: its own entries, and received main
                                // entries main voided.
                                let cands: Vec<_> = e_t
                                    .iter()
                                    .filter(|e| {
                                        e.author == RETURNER
                                            || e_o.iter().any(|(x, v)| x.id() == e.id() && *v == Verdict::Late)
                                    })
                                    .map(Entry::id)
                                    .collect();
                                for wbits in 0u32..(1 << cands.len()) {
                                    let witnessed: Vec<_> = cands
                                        .iter()
                                        .enumerate()
                                        .filter(|(i, _)| wbits & (1 << i) != 0)
                                        .map(|(_, id)| *id)
                                        .collect();
                                    for &contact in &contacts {
                                        for &tie in &ties {
                                            if n % shards == shard {
                                                f(&Input {
                                                    e_o: e_o.clone(),
                                                    e_o2: e_o2.clone(),
                                                    e_t: e_t.clone(),
                                                    witnessed: witnessed.clone(),
                                                    contact,
                                                    tie,
                                                });
                                            }
                                            n += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
