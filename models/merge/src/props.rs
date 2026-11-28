//! The properties model v2 checks on every input (docs/spec/divergence.md 9.5, P-M1 to P-M8, as they apply to the
//! scoped merge v1 of the coordinator's D1), and the examples it must reach.
//!
//! **Independent of the merge** (@world-chunks's review): this file has its own `replay` and its own
//! `Adm`, both written from 9.2's text, and shares with merge.rs only the world's ordinary rules (world.rs), the input
//! types and the footprints (S2's, used here only to pick the inputs P-M5 and P-M6's fast-forward apply to). P-M1
//! compares the merge's `Adm`, step-1 verdict, per-entry outcomes, `S_1`, `S_A` and override lists with the ones
//! computed here.

use std::collections::{BTreeMap, BTreeSet};

use crate::merge::{A, Entry, F_M, FB, Id, Input, Merged, Mutation, N_UNW, Returning, Verdict, footprint, merge};
use crate::world::{Member, Obj, Op, Outcome, RETURNER, Reason, World, apply, created, step};

/// A property a merge broke on an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub property: &'static str,
    pub detail: String,
}

fn v(property: &'static str, detail: String) -> Violation {
    Violation { property, detail }
}

/// A replay's result.
pub struct Run {
    pub end: World,
    pub states: Vec<World>,
    pub outcomes: BTreeMap<Id, Outcome>,
}

/// 9.2's `replay(S, E, to)` from frame `from`: the entries to apply (a `Final` void is left out by the caller), in the
/// agreed order under the input's tie bits; after each frame's entries, one step.
pub fn rerun(start: World, from: u8, to: u8, entries: &[Entry], input: &Input) -> Run {
    let mut es = entries.to_vec();
    es.sort_by_key(|e| e.key(input.tie));
    let mut w = start;
    let mut states = vec![w];
    let mut outcomes = BTreeMap::new();
    for f in from..to {
        let mut moved = [false; 2];
        for e in es.iter().filter(|e| e.stamp == f) {
            outcomes.insert(e.id(), apply(&mut w, e.author, e.op, &mut moved));
        }
        step(&mut w, input.contact == Some(f), moved);
        states.push(w);
    }
    Run { end: w, states, outcomes }
}

/// Main's applied entries below `F_M`: `E_o` as `Final`, its voids left out.
pub fn main_valid(input: &Input) -> Vec<Entry> {
    input.e_o.iter().filter(|(_, v)| *v == Verdict::Valid).map(|(e, _)| *e).collect()
}

fn ids(es: &[Entry]) -> BTreeSet<Id> {
    es.iter().map(Entry::id).collect()
}

fn union(a: &[Entry], b: &[Entry]) -> Vec<Entry> {
    let mut v = a.to_vec();
    v.extend(b.iter().filter(|e| !a.iter().any(|x| x.id() == e.id())));
    v
}

/// The returning entries by 9.2's definition: `E_t` minus main's applied entries, by id, below `F_M`, in agreed order.
pub fn returning_entries(input: &Input) -> Vec<Entry> {
    let main = ids(&main_valid(input));
    let mut out: Vec<Entry> = Vec::new();
    for e in &input.e_t {
        if !main.contains(&e.id()) && e.stamp < F_M && !out.iter().any(|x| x.id() == e.id()) {
            out.push(*e);
        }
    }
    out.sort_by_key(|e| e.key(input.tie));
    out
}

/// The outcomes of main's applied entries below `F_M` in the replay of `E_o` with `extra` added.
fn finals(input: &Input, extra: &[Entry]) -> BTreeMap<Id, Outcome> {
    let valid = main_valid(input);
    let main = ids(&valid);
    rerun(World::base(), FB, F_M, &union(&valid, extra), input)
        .outcomes
        .into_iter()
        .filter(|(id, _)| main.contains(id))
        .collect()
}

/// 9.2's definition of `Adm`, written from its text: witnessed entries, runs, passing, step 1, step 2's procedure.
pub struct Def {
    pub e_r: Vec<Entry>,
    pub w_t: Vec<Entry>,
    pub adm: Vec<Entry>,
    pub step1: bool,
    /// Every returning entry not admitted, with its outcome.
    pub refused: BTreeMap<Id, Returning>,
}

pub fn definition(input: &Input) -> Def {
    let main = ids(&main_valid(input));
    let mut refused = BTreeMap::new();
    for e in &input.e_t {
        if !main.contains(&e.id()) && e.stamp >= F_M {
            refused.insert(e.id(), Returning::AfterMergePoint);
        }
    }
    let e_r = returning_entries(input);
    let witnessed = |e: &Entry| input.witnessed.contains(&e.id());
    let w_t: Vec<Entry> = e_r.iter().filter(|e| witnessed(e)).copied().collect();
    // Runs: one chain's unwitnessed entries in chain order (a witnessed entry doesn't break one), taken in the agreed
    // order of their first entries.
    let mut runs: Vec<Vec<Entry>> = Vec::new();
    let chains: BTreeSet<Member> = e_r.iter().map(|e| e.author).collect();
    for c in chains {
        let mut r: Vec<Entry> = e_r.iter().filter(|e| e.author == c && !witnessed(e)).copied().collect();
        r.sort_by_key(|e| e.seq);
        if !r.is_empty() {
            runs.push(r);
        }
    }
    runs.sort_by_key(|r| r[0].key(input.tie));
    // `X` passes against `Adm_so_far` when no entry of `E_o` has a different outcome in the replay of
    // `E_o ∪ W_t ∪ Adm_so_far` and in the same with `X` added; else the earliest final entry that differs.
    let order: Vec<Entry> = {
        let mut v = main_valid(input);
        v.sort_by_key(|e| e.key(input.tie));
        v
    };
    let passes = |x: &[Entry], so_far: &[Entry]| -> Result<(), Id> {
        let before = union(&w_t, so_far);
        let a = finals(input, &before);
        let b = finals(input, &union(&before, x));
        match order.iter().map(Entry::id).find(|id| a.get(id) != b.get(id)) {
            None => Ok(()),
            Some(id) => Err(id),
        }
    };
    let all: Vec<Entry> = runs.iter().flatten().copied().collect();
    let step1 = all.is_empty() || passes(&all, &[]).is_ok();
    let mut so_far: Vec<Entry> = Vec::new();
    if step1 {
        so_far = all;
    } else {
        let mut queue: std::collections::VecDeque<Vec<Entry>> = runs.into_iter().collect();
        let mut budget: BTreeMap<Member, u8> = BTreeMap::new();
        while let Some(r) = queue.pop_front() {
            let c = r[0].author;
            let n = r.len();
            let b = budget.entry(c).or_insert(N_UNW);
            if *b == 0 {
                for e in &r {
                    refused.insert(e.id(), Returning::MergeBudget);
                }
                continue;
            }
            *b -= 1;
            if passes(&r, &so_far).is_ok() {
                so_far.extend_from_slice(&r);
                continue;
            }
            let (mut lo, mut hi) = (0usize, n - 1);
            while lo < hi && budget[&c] > 0 {
                *budget.get_mut(&c).expect("the chain's budget") -= 1;
                let mid = (lo + hi).div_ceil(2);
                if passes(&r[..mid], &so_far).is_ok() {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            if lo == hi {
                // Entry lo + 1 (1-based) is refused, naming the earliest final entry that differs in the failed test
                // of prefix(lo + 1), recomputed here from the definition.
                let named = passes(&r[..=lo], &so_far).expect_err("prefix(lo + 1) fails");
                so_far.extend_from_slice(&r[..lo]);
                refused.insert(r[lo].id(), Returning::LostToFinal(named));
                if lo + 2 <= n {
                    queue.push_front(r[lo + 1..].to_vec());
                }
            } else {
                so_far.extend_from_slice(&r[..lo]);
                for e in &r[lo..] {
                    refused.insert(e.id(), Returning::MergeBudget);
                }
            }
        }
    }
    let adm = union(&w_t, &so_far);
    Def { e_r, w_t, adm, step1, refused }
}

/// The same input with every list reversed: the arrival order of entries, witness statements and `M` changes.
pub fn reversed(input: &Input) -> Input {
    let mut r = input.clone();
    r.e_o.reverse();
    r.e_o2.reverse();
    r.e_t.reverse();
    r.witnessed.reverse();
    r
}

fn same_result(a: &Merged, b: &Merged) -> bool {
    a.s1 == b.s1
        && a.adm == b.adm
        && a.returning == b.returning
        && a.overridden == b.overridden
        && a.overridden2 == b.overridden2
        && a.line == b.line
}

fn touches(a: &Entry, b: &Entry) -> bool {
    let fb = footprint(b);
    footprint(a).iter().any(|c| fb.contains(c))
}

/// Every property's violations by the merge under `m` on `input`.
pub fn check(input: &Input, m: Mutation) -> Vec<Violation> {
    let got = merge(input, m);
    let mut out = Vec::new();
    let tie = input.tie;
    let base = World::base();
    let d = definition(input);
    let e_r = &d.e_r;
    let witnessed = |e: &Entry| input.witnessed.contains(&e.id());
    let valid = main_valid(input);
    let main_ids = ids(&valid);

    // P-M1: the merge's Adm, step-1 verdict and outcomes are the definition's; S_1, S_A and the override lists are the
    // references'.
    if got.adm != ids(&d.adm) {
        out.push(v("P-M1", format!("Adm {:?}, the definition's {:?}", got.adm, ids(&d.adm))));
    }
    if got.step1 != d.step1 {
        out.push(v("P-M1", format!("step 1 {}, the definition's {}", got.step1, d.step1)));
    }
    for (id, r) in &d.refused {
        if got.returning.get(id) != Some(r) {
            out.push(v("P-M1", format!("{id:?}: {:?}, the definition's {r:?}", got.returning.get(id))));
        }
    }
    let s1 = rerun(base, FB, F_M, &union(&valid, &d.adm), input);
    for e in &d.adm {
        let want = match s1.outcomes.get(&e.id()) {
            Some(Outcome::Refused(r)) => Returning::Refused(*r),
            Some(o) => Returning::MergedIn(*o),
            None => continue,
        };
        if got.returning.get(&e.id()) != Some(&want) {
            out.push(v("P-M1", format!("{:?}: {:?}, the reference's {want:?}", e.id(), got.returning.get(&e.id()))));
        }
    }
    if got.s1 != s1.end {
        out.push(v("P-M1", format!("S_1 {:?} differs from the reference {:?}", got.s1, s1.end)));
    }
    let main_out = finals(input, &[]);
    let overridden: BTreeSet<Id> =
        main_out.iter().filter(|(id, o)| s1.outcomes.get(id) != Some(o)).map(|(id, _)| *id).collect();
    if got.overridden != overridden {
        out.push(v("P-M1", format!("overridden {:?}, the reference's {overridden:?}", got.overridden)));
    }
    let with2 = union(&union(&valid, &d.adm), &input.e_o2);
    let s_ref = rerun(base, FB, A, &with2, input);
    if got.line.last() != Some(&s_ref.end) {
        out.push(v("P-M1", format!("S_A {:?} differs from S_ref {:?}", got.line.last(), s_ref.end)));
    }
    let main_line = rerun(base, FB, A, &union(&valid, &input.e_o2), input);
    let overridden2: BTreeSet<Id> =
        input.e_o2.iter().map(Entry::id).filter(|id| main_line.outcomes.get(id) != s_ref.outcomes.get(id)).collect();
    if got.overridden2 != overridden2 {
        out.push(v("P-M1", format!("overridden2 {:?}, the reference's {overridden2:?}", got.overridden2)));
    }

    // P-M2: the same result whatever the arrival order.
    let again = merge(&reversed(input), m);
    if !same_result(&got, &again) {
        out.push(v("P-M2", "the result depends on the arrival order".into()));
    }

    // P-M3: no state below A changes; the merged line is main's own below A.
    for f in 0..usize::from(A) {
        if got.line.get(f) != main_line.states.get(f) {
            out.push(v("P-M3", format!("frame {f} changed below A")));
            break;
        }
    }

    // P-M4, causal: the merge's admitted entries change no final outcome below F_M that its witnessed admitted entries
    // alone don't (9.2: an unwitnessed set is admitted only if it passes), and each final entry changed has a
    // witnessed admitted entry before it.
    let adm_got: Vec<Entry> = e_r.iter().filter(|e| got.adm.contains(&e.id())).copied().collect();
    // The merge's own stage 1, replayed here: P-M4, P-M5 and P-M8 judge what the merge admitted (P-M1 compares that
    // with the definition).
    let s1_got = rerun(base, FB, F_M, &union(&valid, &adm_got), input);
    let w_got: Vec<Entry> = adm_got.iter().filter(|e| witnessed(e)).copied().collect();
    let with_all = finals(input, &adm_got);
    let with_w = finals(input, &w_got);
    if with_all != with_w {
        let id = valid.iter().map(Entry::id).find(|id| with_all.get(id) != with_w.get(id));
        out.push(v("P-M4", format!("final {id:?} changed by an unwitnessed admitted entry")));
    }
    for (id, o) in &main_out {
        let now = with_all.get(id).copied();
        if now == Some(*o) {
            continue;
        }
        let Some(e) = valid.iter().find(|e| e.id() == *id) else { continue };
        if !w_got.iter().any(|w| w.key(tie) < e.key(tie)) {
            out.push(v("P-M4", format!("final {id:?} {o:?} -> {now:?} with no witnessed returning entry before it")));
        }
    }

    // P-M5: the user's delete rule for witnessed entries: a delete before an add-on orphans an object add-on (created
    // under, or reparented to, the object) and refuses a component add-on; after, it destroys them. Checked where the
    // two (and, for a reparent, the create of the object it moves) are the only entries touching them.
    for d_e in e_r.iter().filter(|x| witnessed(x)) {
        let Op::Delete(o) = d_e.op else { continue };
        for e in &valid {
            if e.author == RETURNER || !matches!(e.op, Op::Create(x) | Op::Attach(x) | Op::Reparent(x) if x == o) {
                continue;
            }
            // A reparent moves main's `C`: it needs the create of `C` under another object, before it.
            let creator = valid.iter().find(|x| {
                matches!(e.op, Op::Reparent(_))
                    && matches!(x.op, Op::Create(u) if u != o && !created(u))
                    && x.key(tie) < e.key(tie)
            });
            if matches!(e.op, Op::Reparent(_)) && creator.is_none() {
                continue;
            }
            let skip: Vec<Id> = [Some(d_e.id()), Some(e.id()), creator.map(Entry::id)].into_iter().flatten().collect();
            let others = input
                .e_o
                .iter()
                .map(|(x, _)| x)
                .chain(e_r.iter())
                .filter(|x| !skip.contains(&x.id()))
                .any(|x| touches(x, e) || touches(x, d_e));
            if others {
                continue;
            }
            let o_e = s1_got.outcomes.get(&e.id()).copied();
            let ok = if d_e.key(tie) < e.key(tie) {
                match e.op {
                    Op::Attach(_) => o_e == Some(Outcome::Refused(Reason::Gone)),
                    _ => o_e == Some(Outcome::Orphaned),
                }
            } else {
                match e.op {
                    Op::Attach(_) => !got.s1.obj(o).exists,
                    _ => !got.s1.obj(Obj::C).exists,
                }
            };
            if !ok {
                out.push(v("P-M5", format!("delete {:?} vs add-on {:?}: {o_e:?}", d_e.id(), e.id())));
            }
        }
    }

    // P-M6: every returning entry has an outcome; if step 1's test passes, every one is admitted; a returning entry
    // whose footprint no final entry shares, and whose earlier overlapping returning entries are admitted, comes out
    // as it did in theirs (fast-forward).
    for e in e_r {
        if !got.returning.contains_key(&e.id()) {
            out.push(v("P-M6", format!("returning {:?} has no outcome", e.id())));
        }
    }
    if d.step1 {
        for e in e_r {
            if !got.adm.contains(&e.id()) {
                out.push(v("P-M6", format!("step 1 passes but {:?} is not admitted", e.id())));
            }
        }
    }
    let t_run = rerun(base, FB, F_M, &input.e_t, input);
    for e in e_r {
        let alone = !input.e_o.iter().any(|(x, _)| touches(x, e));
        let earlier_ok =
            e_r.iter().filter(|x| x.key(tie) < e.key(tie) && touches(x, e)).all(|x| got.adm.contains(&x.id()));
        let received_ok =
            input.e_t.iter().all(|x| !touches(x, e) || x.id() == e.id() || e_r.iter().any(|y| y.id() == x.id()));
        if alone && earlier_ok && received_ok {
            let want = t_run.outcomes.get(&e.id()).copied();
            let ok = match (got.returning.get(&e.id()), want) {
                (Some(Returning::MergedIn(o)), Some(w)) => *o == w,
                (Some(Returning::Refused(r)), Some(w)) => Outcome::Refused(*r) == w,
                _ => false,
            };
            if !ok {
                out.push(v(
                    "P-M6",
                    format!("fast-forward {:?}: {:?}, theirs {want:?}", e.id(), got.returning.get(&e.id())),
                ));
            }
        }
    }

    // P-M7: every entry takes effect at most once; a second merge naming entries already merged in changes nothing.
    for (id, n) in &got.applied {
        if *n > 1 {
            out.push(v("P-M7", format!("{id:?} took effect {n} times")));
        }
    }
    if m == Mutation::None {
        let mut second = input.clone();
        for e in &adm_got {
            second.e_o.push((*e, Verdict::Valid));
        }
        second.e_t.retain(|e| adm_got.iter().any(|a| a.id() == e.id()) || main_ids.contains(&e.id()));
        let again = merge(&second, m);
        if !again.adm.is_empty() || again.s1 != got.s1 {
            out.push(v("P-M7", "a second merge of the same entries changed something".into()));
        }
    }

    // P-M8 (9.6, an automatic merge): base's admins, plus additions on either side, minus removals on either side;
    // an add and a remove of one member are decided by the ordinary order and the witnessed rule, so P-M1 checks
    // those. An addition or removal counts when it takes effect in the merge (main's final ones, and the returning
    // ones admitted).
    let in_merge = union(&valid, &adm_got);
    let took = |x: &Entry| s1_got.outcomes.get(&x.id()) == Some(&Outcome::Applied);
    for i in 0..8u8 {
        let adds = in_merge.iter().any(|x| x.op == Op::AddAdmin(i) && took(x));
        let removes = in_merge.iter().any(|x| x.op == Op::RemoveAdmin(i) && took(x));
        let want = match (adds, removes) {
            (false, false) => base.admins & (1 << i) != 0,
            (true, false) => true,
            (false, true) => false,
            (true, true) => continue,
        };
        if (got.s1.admins & (1 << i) != 0) != want {
            out.push(v("P-M8", format!("admin {i}: {:#b}, 9.6 says {want}", got.s1.admins)));
        }
    }
    out
}

/// The examples of 9.5 the model must reach, each a predicate on an input and its unmutated merge.
pub type Example = fn(&Input, &Merged) -> bool;

pub const EXAMPLES: &[(&str, Example)] = &[
    ("a lone returner's back-stamped grab loses", |i, m| {
        returning_entries(i).iter().any(|e| {
            matches!(e.op, Op::Grab(_))
                && !i.witnessed.contains(&e.id())
                && matches!(m.returning.get(&e.id()), Some(Returning::LostToFinal(_)))
        })
    }),
    ("its back-stamped delete of an object main later edited loses", |i, m| {
        returning_entries(i).iter().any(|e| {
            let Op::Delete(o) = e.op else { return false };
            !i.witnessed.contains(&e.id())
                && i.e_o.iter().any(|(x, _)| x.key(i.tie) > e.key(i.tie) && matches!(x.op, Op::Write(t, _) if t == o))
                && matches!(m.returning.get(&e.id()), Some(Returning::LostToFinal(_)))
        })
    }),
    ("a returning push that hits a ball a final grab holds is refused", |i, m| {
        i.contact.is_some()
            && returning_entries(i).iter().any(|e| {
                matches!(e.op, Op::Write(Obj::X, 1))
                    && i.e_o.iter().all(|(x, _)| !matches!(x.op, Op::Write(..) | Op::WriteIf(..)))
                    && i.e_o.iter().any(|(x, _)| matches!(x.op, Op::Grab(Obj::P)))
                    && matches!(m.returning.get(&e.id()), Some(Returning::LostToFinal(_)))
            })
    }),
    ("a run whose early entry conflicts keeps its later independent entries", |i, m| {
        let r = returning_entries(i);
        r.iter().any(|a| {
            !i.witnessed.contains(&a.id())
                && matches!(m.returning.get(&a.id()), Some(Returning::LostToFinal(_)))
                && r.iter().any(|b| {
                    b.author == a.author
                        && b.seq > a.seq
                        && !i.witnessed.contains(&b.id())
                        && matches!(m.returning.get(&b.id()), Some(Returning::MergedIn(_)))
                })
        })
    }),
    ("a witnessed earlier delete orphans main's later add-on", |i, m| {
        returning_entries(i).iter().any(|d| {
            let Op::Delete(o) = d.op else { return false };
            i.witnessed.contains(&d.id())
                && m.adm.contains(&d.id())
                && i.e_o.iter().any(|(x, _)| {
                    x.key(i.tie) > d.key(i.tie) && x.op == Op::Create(o) && m.overridden.contains(&x.id())
                })
                && m.s1.obj(Obj::C).exists
                && m.s1.obj(Obj::C).parent.is_none()
        })
    }),
    ("a witnessed earlier delete orphans main's later reparent", |i, m| {
        returning_entries(i).iter().any(|d| {
            let Op::Delete(o) = d.op else { return false };
            i.witnessed.contains(&d.id())
                && m.adm.contains(&d.id())
                && i.e_o.iter().any(|(x, _)| {
                    x.key(i.tie) > d.key(i.tie) && x.op == Op::Reparent(o) && m.overridden.contains(&x.id())
                })
                && m.s1.obj(Obj::C).exists
                && m.s1.obj(Obj::C).parent.is_none()
        })
    }),
    ("a returning entry main voided as Late that changes no final outcome is merged in", |i, m| {
        i.e_o.iter().any(|(x, v)| {
            *v == Verdict::Late
                && i.e_t.iter().any(|t| t.id() == x.id())
                && matches!(m.returning.get(&x.id()), Some(Returning::MergedIn(_)))
        })
    }),
    ("a returning write to an object main never touched is kept", |i, m| {
        returning_entries(i).iter().any(|e| {
            let Op::Write(o, val) = e.op else { return false };
            i.e_o.iter().all(|(x, _)| !touches(x, e))
                && matches!(m.returning.get(&e.id()), Some(Returning::MergedIn(_)))
                && m.s1.obj(o).field == val
        })
    }),
    ("an entry both lines hold is applied once", |i, m| {
        i.e_o.iter().any(|(x, v)| {
            *v == Verdict::Valid
                && matches!(x.op, Op::Create(_))
                && i.e_t.iter().any(|t| t.id() == x.id())
                && m.applied.get(&x.id()) == Some(&1)
        })
    }),
    ("a returning create and a final create both stand", |i, m| {
        returning_entries(i).iter().any(|e| {
            matches!(e.op, Op::Create(_))
                && i.e_o.iter().any(|(x, v)| *v == Verdict::Valid && matches!(x.op, Op::Create(_)))
                && matches!(m.returning.get(&e.id()), Some(Returning::MergedIn(Outcome::Applied)))
                && m.s1.obj(Obj::C).exists
                && m.s1.obj(Obj::D).exists
        })
    }),
    ("a witnessed returning grab ranked first at a final grab's stamp wins it", |i, m| {
        returning_entries(i).iter().any(|e| {
            matches!(e.op, Op::Grab(_))
                && i.witnessed.contains(&e.id())
                && i.tie & (1 << e.stamp) != 0
                && i.e_o.iter().any(|(x, v)| {
                    *v == Verdict::Valid && x.op == e.op && x.stamp == e.stamp && m.overridden.contains(&x.id())
                })
                && matches!(m.returning.get(&e.id()), Some(Returning::MergedIn(Outcome::Granted)))
        })
    }),
    ("a returning removal of an admin who then acts is lost to final", |i, m| {
        returning_entries(i).iter().any(|e| {
            let Op::RemoveAdmin(a) = e.op else { return false };
            !i.witnessed.contains(&e.id())
                && i.e_o.iter().any(|(x, v)| {
                    *v == Verdict::Valid
                        && x.author == a
                        && matches!(x.op, Op::AddAdmin(_) | Op::RemoveAdmin(_))
                        && x.key(i.tie) > e.key(i.tie)
                })
                && matches!(m.returning.get(&e.id()), Some(Returning::LostToFinal(_)))
        })
    }),
    ("a returning removal against main's addition of one member is decided by order", |i, m| {
        returning_entries(i).iter().any(|e| {
            let Op::RemoveAdmin(a) = e.op else { return false };
            i.e_o.iter().any(|(x, v)| *v == Verdict::Valid && x.op == Op::AddAdmin(a) && x.key(i.tie) < e.key(i.tie))
                && matches!(m.returning.get(&e.id()), Some(Returning::MergedIn(Outcome::Applied)))
                && m.s1.admins & (1 << a) == 0
        })
    }),
];

/// Which property each control must break (9.5's table, and the two step-2 controls of the review).
pub fn expected(m: Mutation) -> &'static [&'static str] {
    match m {
        Mutation::None => &[],
        Mutation::MergeByArrival => &["P-M2"],
        Mutation::ApplyAtStamp => &["P-M3"],
        Mutation::NoWitnessCheck => &["P-M4"],
        Mutation::RefuseAllEarlier => &["P-M5"],
        Mutation::MergeDropsSide | Mutation::VoidSplitEntries => &["P-M6"],
        Mutation::NoDedupe => &["P-M7"],
        Mutation::UnionAdminsAlways => &["P-M8"],
        Mutation::GlobalBudget | Mutation::StaleAdmSoFar => &["P-M1"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(stamp: u8, author: Member, seq: u8, op: Op) -> Entry {
        Entry { stamp, author, seq, op }
    }

    /// A returning entry stamped at `F_M` is refused `AfterMergePoint` (tier1's stamps stay below `F_M`).
    #[test]
    fn after_merge_point() {
        let late = e(F_M, RETURNER, 1, Op::Write(Obj::X, 1));
        let input = Input {
            e_o: vec![(e(1, 0, 1, Op::Grab(Obj::P)), Verdict::Valid)],
            e_o2: vec![],
            e_t: vec![late],
            witnessed: vec![],
            contact: None,
            tie: 0,
        };
        assert_eq!(merge(&input, Mutation::None).returning.get(&late.id()), Some(&Returning::AfterMergePoint));
        assert_eq!(check(&input, Mutation::None), vec![]);
    }

    /// The two step-2 controls on the cases the step2 profile must find, built by hand.
    #[test]
    fn step2_controls_bite() {
        // GlobalBudget: the returner's run spends its chain's three tests; main's late entry, received and
        // unwitnessed, is a second chain whose own budget admits it.
        let global = Input {
            e_o: vec![
                (e(3, 0, 2, Op::Write(Obj::P, 1)), Verdict::Valid),
                (e(2, 0, 1, Op::Write(Obj::X, 1)), Verdict::Late),
            ],
            e_o2: vec![],
            e_t: vec![
                e(0, RETURNER, 1, Op::Grab(Obj::P)),
                e(1, RETURNER, 2, Op::Write(Obj::X, 1)),
                e(2, 0, 1, Op::Write(Obj::X, 1)),
            ],
            witnessed: vec![],
            contact: None,
            tie: 0,
        };
        assert_eq!(check(&global, Mutation::None), vec![]);
        assert!(check(&global, Mutation::GlobalBudget).iter().any(|x| x.property == "P-M1"));
        // StaleAdmSoFar: main's late raise of P is admitted first; the returner's delete fails; its grab passes only
        // against the raise (P is moving), and against nothing it takes the hold from the final write.
        let stale = Input {
            e_o: vec![
                (e(3, 0, 2, Op::Write(Obj::P, 1)), Verdict::Valid),
                (e(0, 0, 1, Op::Write(Obj::P, 1)), Verdict::Late),
            ],
            e_o2: vec![],
            e_t: vec![
                e(1, RETURNER, 1, Op::Delete(Obj::P)),
                e(2, RETURNER, 2, Op::Grab(Obj::P)),
                e(0, 0, 1, Op::Write(Obj::P, 1)),
            ],
            witnessed: vec![],
            contact: None,
            tie: 0,
        };
        assert_eq!(check(&stale, Mutation::None), vec![]);
        assert!(check(&stale, Mutation::StaleAdmSoFar).iter().any(|x| x.property == "P-M1"));
    }
}
