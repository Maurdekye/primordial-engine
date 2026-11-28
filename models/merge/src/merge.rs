//! The merge against finality, as merge engine v1 computes it (docs/spec/divergence.md 9.2, 9.3a; scoped by the
//! coordinator's D1: whole-world replays, no grouping), with the controls of 9.5 as mutations.
//!
//! - Frames: `fb = 0`, `F_M = 4`, `A = 6` (9.5's toy values). `ours` (`E_o`) and `theirs` (`E_t`) are stamped in
//!   `[fb, F_M)`; main's entries in `[F_M, A)` are `E_o2`.
//! - The agreed order is (stamp, tie rank, author, seq): the author stands for the chain, one chain per member here.
//!   The tie rank `H(line, frame, author)` is modelled as one free bit per frame (the input's `tie`): whether the
//!   returner's entries at that stamp come before main's or after them. The enumerator sets it both ways wherever
//!   the two share a stamp.
//! - `E_r = E_t \ E_o` by id, where `E_o` counts only main's applied entries. A main entry main voided as `Late`,
//!   that the returner received and applied, is a returning entry [I: 9.2 says theirs includes "main members'
//!   entries that main voided as late", and 9.3 that those are among the entries a submitter's omitted witness can
//!   refuse; @world-chunks to confirm].
//! - `Adm` by 9.2's two steps, with a per-chain budget [`N_UNW`] for step 2.
//! - Stage 2 re-applies `E_o2` on stage 1's state, whole world, from `F_M` to `A`.

use std::collections::{BTreeMap, BTreeSet};

use crate::world::{Member, Obj, Op, Outcome, RETURNER, Reason, World, apply, slot, step};

pub const FB: u8 = 0;
pub const F_M: u8 = 4;
pub const A: u8 = 6;

/// Step 2's tests per chain (section 14's `N_unw`, 16 in the engine). Here 3: a two-entry run's first test, its
/// bisection and its remainder's test, so later work is kept past one conflict per chain, the engine's stated limit
/// (9.2). At 2, the first model run found a run's independent second entry refused with `MergeBudget` after its first
/// entry's conflict: the limit itself, at a budget too small for the run. Budget exhaustion is measured in the
/// harness (9.5), not here.
pub const N_UNW: u8 = 3;

/// An entry: its stamp, its author (and chain), its sequence in that chain, and its op.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Entry {
    pub stamp: u8,
    pub author: Member,
    pub seq: u8,
    pub op: Op,
}

pub type Id = (Member, u8);

impl Entry {
    pub fn id(&self) -> Id {
        (self.author, self.seq)
    }

    /// The agreed order's key under the input's tie bits: stamp, tie rank, author, seq.
    pub fn key(&self, tie: u8) -> (u8, u8, Member, u8) {
        let returner_first = tie & (1 << self.stamp) != 0;
        let rank = u8::from((self.author == RETURNER) != returner_first);
        (self.stamp, rank, self.author, self.seq)
    }
}

/// Main's final verdict on one of its entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict {
    Valid,
    /// Voided as late: skipped by a `Final` disposition, applied by an `AtStamp` one.
    Late,
}

/// One merge's inputs (9.2), as the agreed data gives them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Input {
    /// `E_o`, main's final log in `[fb, F_M)`, with its verdicts.
    pub e_o: Vec<(Entry, Verdict)>,
    /// `E_o2`, main's entries in `[F_M, A)`.
    pub e_o2: Vec<Entry>,
    /// `E_t`: the returner's own entries and the main entries it received and applied.
    pub e_t: Vec<Entry>,
    /// The returning entries `M` carries a witness statement for.
    pub witnessed: Vec<Id>,
    /// The one frame with contact on, if any.
    pub contact: Option<u8>,
    /// The tie rank: bit `f` set puts the returner's entries stamped `f` before main's.
    pub tie: u8,
}

/// A mutation of the merge: the controls of 9.5, each of which some property must catch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mutation {
    None,
    /// Order the replay by arrival (the inputs' order) instead of the agreed order.
    MergeByArrival,
    /// Apply admitted returning entries at their stamps on main's line, rewriting final frames.
    ApplyAtStamp,
    /// Treat every returning entry as witnessed.
    NoWitnessCheck,
    /// Refuse every returning entry stamped before a final entry it conflicts with (the general form).
    RefuseAllEarlier,
    /// Drop returning work that conflicts with nothing.
    MergeDropsSide,
    /// Leave out returning entries main voided.
    VoidSplitEntries,
    /// Count an entry in both lines twice.
    NoDedupe,
    /// Take the union of both sides' admin lists.
    UnionAdminsAlways,
    /// Step 2 with one budget for all chains instead of one per chain.
    GlobalBudget,
    /// Step 2 testing each run against the witnessed entries alone, not everything admitted before it.
    StaleAdmSoFar,
}

/// How a returning entry came out (9.3, step 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Returning {
    MergedIn(Outcome),
    /// Admitted, and refused by the ordinary rules in the merged order.
    Refused(Reason),
    /// Not admitted: its run's first failing entry, naming the earliest final entry whose outcome it changed.
    LostToFinal(Id),
    MergeBudget,
    AfterMergePoint,
}

/// The merge's result (9.2's `Merged`), with what the properties read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merged {
    /// Stage 1's state at `F_M`.
    pub s1: World,
    pub adm: BTreeSet<Id>,
    pub returning: BTreeMap<Id, Returning>,
    /// Main's entries below `F_M` whose outcome stage 1 changed, and those in `[F_M, A)` stage 2 changed.
    pub overridden: BTreeSet<Id>,
    pub overridden2: BTreeSet<Id>,
    /// Whether step 1 admitted every unwitnessed run (the merge then applies at `A1`).
    pub step1: bool,
    /// The merged line's state at each frame `0..=A`: main's below `A`, then `S_A`.
    pub line: Vec<World>,
    /// How many times each id took effect in stage 1's replay.
    pub applied: BTreeMap<Id, u8>,
}

/// How an entry enters a replay (9.2's dispositions).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disp {
    Final(Verdict),
    AtStamp,
}

/// A replay's result: the state at its end, each frame's state from its start, each entry's outcome (the last, for
/// an id applied twice), and how many times each id took effect.
pub struct Replay {
    pub end: World,
    pub states: Vec<World>,
    pub outcomes: BTreeMap<Id, Outcome>,
    pub applied: BTreeMap<Id, u8>,
}

/// `replay(S, E, to)` from frame `from` (9.2), in the agreed order, or in the given order when `by_arrival`.
pub fn replay(
    start: World,
    from: u8,
    to: u8,
    entries: &[(Entry, Disp)],
    contact: Option<u8>,
    tie: u8,
    by_arrival: bool,
) -> Replay {
    let mut list: Vec<(usize, Entry, Disp)> = entries.iter().enumerate().map(|(i, (e, d))| (i, *e, *d)).collect();
    if by_arrival {
        list.sort_by_key(|(i, e, _)| (e.stamp, *i));
    } else {
        list.sort_by_key(|(i, e, _)| (e.key(tie), *i));
    }
    let mut w = start;
    let mut states = vec![w];
    let mut outcomes = BTreeMap::new();
    let mut applied: BTreeMap<Id, u8> = BTreeMap::new();
    for f in from..to {
        let mut moved = [false; 2];
        for (_, e, d) in list.iter().filter(|(_, e, _)| e.stamp == f) {
            if *d == Disp::Final(Verdict::Late) {
                continue;
            }
            let o = apply(&mut w, e.author, e.op, &mut moved);
            outcomes.insert(e.id(), o);
            *applied.entry(e.id()).or_default() += 1;
        }
        step(&mut w, contact == Some(f), moved);
        states.push(w);
    }
    Replay { end: w, states, outcomes, applied }
}

/// The cells an op reads or writes, for the footprint-based checks (P-M6's fast-forward, `RefuseAllEarlier`'s
/// conflicts): an object's metadata, `C`, the physics domain (both bodies and `g`), and the admin list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cell {
    Meta(Obj),
    Domain,
    Admins,
}

pub fn footprint(e: &Entry) -> Vec<Cell> {
    let body = |o: Obj| matches!(o, Obj::P | Obj::X);
    match e.op {
        // A grab of a body reads its height (the reach rule).
        Op::Grab(o) if body(o) => vec![Cell::Meta(o), Cell::Domain],
        Op::Grab(o) | Op::Release(o) | Op::Attach(o) => vec![Cell::Meta(o)],
        Op::Write(o, _) | Op::WriteIf(o, _, _) => {
            let mut v = vec![Cell::Meta(o)];
            if body(o) {
                v.push(Cell::Domain);
            }
            v
        }
        Op::Delete(o) => {
            // Its subtree: either created object may be its child.
            let mut v = vec![Cell::Meta(o), Cell::Meta(Obj::C), Cell::Meta(Obj::D)];
            if body(o) {
                v.push(Cell::Domain);
            }
            v
        }
        Op::Create(under) | Op::Reparent(under) => vec![Cell::Meta(slot(e.author)), Cell::Meta(under)],
        Op::WriteG(_) => vec![Cell::Domain],
        Op::AddAdmin(_) | Op::RemoveAdmin(_) => vec![Cell::Admins],
    }
}

fn overlaps(a: &Entry, b: &Entry) -> bool {
    let fb = footprint(b);
    footprint(a).iter().any(|c| fb.contains(c))
}

/// The merge function, `merge(S_b, E_o, E_t, W) -> Merged`, under `m`.
pub fn merge(input: &Input, m: Mutation) -> Merged {
    let arrival = m == Mutation::MergeByArrival;
    let base = World::base();
    let main_applied: BTreeSet<Id> =
        input.e_o.iter().filter(|(_, v)| *v == Verdict::Valid).map(|(e, _)| e.id()).collect();
    let voided: BTreeSet<Id> = input.e_o.iter().filter(|(_, v)| *v == Verdict::Late).map(|(e, _)| e.id()).collect();
    let mut returning: BTreeMap<Id, Returning> = BTreeMap::new();
    // E_r = E_t \ E_o, by id.
    let mut e_r: Vec<Entry> = Vec::new();
    for e in &input.e_t {
        if m != Mutation::NoDedupe && main_applied.contains(&e.id()) {
            continue;
        }
        if m == Mutation::VoidSplitEntries && voided.contains(&e.id()) {
            continue;
        }
        if e.stamp >= F_M {
            returning.insert(e.id(), Returning::AfterMergePoint);
            continue;
        }
        if !e_r.iter().any(|x| x.id() == e.id()) || m == Mutation::NoDedupe {
            e_r.push(*e);
        }
    }
    let witnessed = |e: &Entry| m == Mutation::NoWitnessCheck || input.witnessed.contains(&e.id());
    let ours: Vec<(Entry, Disp)> = input.e_o.iter().map(|(e, v)| (*e, Disp::Final(*v))).collect();
    let with = |adm: &[Entry]| -> Vec<(Entry, Disp)> {
        let mut v = ours.clone();
        v.extend(adm.iter().map(|e| (*e, Disp::AtStamp)));
        v
    };
    let finals = |r: &Replay| -> BTreeMap<Id, Outcome> {
        r.outcomes.iter().filter(|(id, _)| main_applied.contains(id)).map(|(k, v)| (*k, *v)).collect()
    };
    let tie = input.tie;
    let run = |adm: &[Entry]| replay(base, FB, F_M, &with(adm), input.contact, tie, arrival);
    // The earliest final entry (agreed order) whose outcome differs between two replays, if any.
    let ordered_finals: Vec<Entry> = {
        let mut v: Vec<Entry> = input.e_o.iter().filter(|(_, v)| *v == Verdict::Valid).map(|(e, _)| *e).collect();
        v.sort_by_key(|e| e.key(tie));
        v
    };
    let first_diff = |a: &BTreeMap<Id, Outcome>, b: &BTreeMap<Id, Outcome>| -> Option<Id> {
        ordered_finals.iter().map(Entry::id).find(|id| a.get(id) != b.get(id))
    };

    let mut candidates: Vec<Entry> = e_r.clone();
    // The general-form control: refuse a returning entry stamped before a final entry it conflicts with.
    if m == Mutation::RefuseAllEarlier {
        candidates.retain(|e| {
            let lost = ordered_finals.iter().any(|f| e.key(tie) < f.key(tie) && overlaps(e, f));
            if lost {
                returning.insert(e.id(), Returning::LostToFinal((255, 255)));
            }
            !lost
        });
    }
    // The drop-a-side control: returning work that conflicts with nothing is dropped.
    if m == Mutation::MergeDropsSide {
        candidates.retain(|e| {
            let alone = !ordered_finals.iter().any(|f| overlaps(e, f));
            if alone {
                returning.insert(e.id(), Returning::MergeBudget);
            }
            !alone
        });
    }
    let w_t: Vec<Entry> = candidates.iter().filter(|e| witnessed(e)).copied().collect();
    let mut runs: Vec<Vec<Entry>> = Vec::new();
    {
        let mut by_chain: BTreeMap<Member, Vec<Entry>> = BTreeMap::new();
        for e in candidates.iter().filter(|e| !witnessed(e)) {
            by_chain.entry(e.author).or_default().push(*e);
        }
        for (_, mut r) in by_chain {
            r.sort_by_key(|e| e.seq);
            runs.push(r);
        }
        runs.sort_by_key(|r| r[0].key(tie));
    }
    let passes = |x: &[Entry], adm: &[Entry]| -> Result<(), Id> {
        let a = finals(&run(adm));
        let mut more = adm.to_vec();
        more.extend_from_slice(x);
        let b = finals(&run(&more));
        match first_diff(&a, &b) {
            None => Ok(()),
            Some(id) => Err(id),
        }
    };
    let mut adm: Vec<Entry> = w_t.clone();
    let all_unwitnessed: Vec<Entry> = runs.iter().flatten().copied().collect();
    let step1 = all_unwitnessed.is_empty() || passes(&all_unwitnessed, &adm).is_ok();
    if step1 {
        adm.extend(all_unwitnessed);
    } else {
        // Step 2: 9.2's procedure, its result the definition.
        let mut budget: BTreeMap<Member, u8> = BTreeMap::new();
        let mut queue: std::collections::VecDeque<Vec<Entry>> = runs.into_iter().collect();
        while let Some(r) = queue.pop_front() {
            // The chain's budget; under the control, one budget for every chain.
            let c = if m == Mutation::GlobalBudget { Member::MAX } else { r[0].author };
            let b = budget.entry(c).or_insert(N_UNW);
            if *b == 0 {
                for e in &r {
                    returning.insert(e.id(), Returning::MergeBudget);
                }
                continue;
            }
            *b -= 1;
            let n = r.len();
            // `failed_at` is the diff of the last failing test, which is always prefix(lo + 1)'s at the end.
            // Against everything admitted so far; under the control, against the witnessed entries alone.
            let so_far = if m == Mutation::StaleAdmSoFar { w_t.clone() } else { adm.clone() };
            let mut failed_at = match passes(&r, &so_far) {
                Ok(()) => {
                    adm.extend_from_slice(&r);
                    continue;
                }
                Err(id) => id,
            };
            let (mut lo, mut hi) = (0usize, n - 1);
            while lo < hi && *budget.get(&c).unwrap_or(&0) > 0 {
                *budget.get_mut(&c).expect("the chain's budget") -= 1;
                let mid = (lo + hi).div_ceil(2);
                match passes(&r[..mid], &so_far) {
                    Ok(()) => lo = mid,
                    Err(id) => {
                        hi = mid - 1;
                        failed_at = id;
                    }
                }
            }
            adm.extend_from_slice(&r[..lo]);
            if lo == hi {
                returning.insert(r[lo].id(), Returning::LostToFinal(failed_at));
                if lo + 2 <= n {
                    queue.push_front(r[lo + 1..].to_vec());
                }
            } else {
                for e in &r[lo..] {
                    returning.insert(e.id(), Returning::MergeBudget);
                }
            }
        }
    }
    // Stage 1.
    let main_run = run(&[]);
    let s1_run = run(&adm);
    let mut s1 = s1_run.end;
    if m == Mutation::UnionAdminsAlways {
        let theirs = replay(
            base,
            FB,
            F_M,
            &input.e_t.iter().map(|e| (*e, Disp::AtStamp)).collect::<Vec<_>>(),
            input.contact,
            tie,
            arrival,
        );
        s1.admins = main_run.end.admins | theirs.end.admins;
    }
    for e in &adm {
        let o = s1_run.outcomes.get(&e.id()).copied().unwrap_or(Outcome::Applied);
        returning.insert(
            e.id(),
            match o {
                Outcome::Refused(r) => Returning::Refused(r),
                o => Returning::MergedIn(o),
            },
        );
    }
    let main_finals = finals(&main_run);
    let s1_finals = finals(&s1_run);
    let overridden = main_finals.keys().filter(|id| main_finals.get(id) != s1_finals.get(id)).copied().collect();
    // Stage 2: E_o2 re-applied on S_1 from F_M to A, whole world.
    let ours2: Vec<(Entry, Disp)> = input.e_o2.iter().map(|e| (*e, Disp::Final(Verdict::Valid))).collect();
    let main2 = replay(main_run.end, F_M, A, &ours2, input.contact, tie, arrival);
    let stage2 = replay(s1, F_M, A, &ours2, input.contact, tie, arrival);
    let overridden2 =
        main2.outcomes.keys().filter(|id| main2.outcomes.get(id) != stage2.outcomes.get(id)).copied().collect();
    // The merged line: main's states below A, then S_A.
    let mut line: Vec<World> = main_run.states.clone();
    line.extend_from_slice(&main2.states[1..]);
    if m == Mutation::ApplyAtStamp {
        // The control: admitted entries applied at their stamps on main's line, final frames rewritten.
        let mut all = with(&adm);
        all.extend(ours2.iter().copied());
        line = replay(base, FB, A, &all, input.contact, tie, arrival).states;
    }
    let last = line.len() - 1;
    line[last] = stage2.end;
    Merged {
        s1,
        adm: adm.iter().map(Entry::id).collect(),
        returning,
        overridden,
        overridden2,
        step1,
        line,
        applied: s1_run.applied,
    }
}
