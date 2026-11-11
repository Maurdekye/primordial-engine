//! The property checkers (DESIGN.md 5). Each is a function of the trace and returns every violation it finds.
//!
//! Built so far: P1 agreement, P2 live equals replay, P3 no final frame undone, P4 voids decided by cuts and equal
//! everywhere, P6 no aborts and every invalid input refused, P8's paused-frame half, P15 the entry source's
//! contract. The rest follow with the
//! runner. Every checker has planted-violation tests below (DESIGN.md 6.2): a checker that never fires proves
//! nothing.

use std::collections::BTreeMap;
use std::fmt;

use crate::clock::Nanos;
use crate::trace::{EntryRef, Ev, Frame, Hash, Height, Line, Node, Trace};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Prop {
    P1,
    P2,
    P3,
    P4,
    P5,
    P6,
    P8,
    /// The entry source's contract (coordinator m7940e24cabf1): no member votes on, or finalizes, a cut while it
    /// lacks an entry the cut names; final cuts only move forward; nothing uncovered or void is applied below the
    /// final frame.
    P15,
    /// A scenario's own `[[expect]]` on an outcome (DESIGN.md 4.1): an expectation, not a property.
    Expect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub prop: Prop,
    pub at: Nanos,
    pub node: Option<Node>,
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} at {} ns", self.prop, self.at)?;
        if let Some(n) = self.node {
            write!(f, " on node {n}")?;
        }
        write!(f, ": {}", self.detail)
    }
}

pub fn v(prop: Prop, at: Nanos, node: Option<Node>, detail: String) -> Violation {
    Violation { prop, at, node, detail }
}

fn short(h: &Hash) -> String {
    h[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// P1. No two honest nodes finalize different cuts at one (line, height), and each honest replica's root at a final
/// cut's frame equals the cut's root. A deliberately corrupted node is left out while it is excused.
pub fn p1_agreement(t: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut first: BTreeMap<(Line, Height), (Node, Hash, Hash)> = BTreeMap::new();
    for r in &t.records {
        let Ev::Final { line, height, cut, root, .. } = r.ev else {
            continue;
        };
        if !t.honest.contains(&r.node) {
            continue;
        }
        match first.get(&(line, height)) {
            None => {
                first.insert((line, height), (r.node, cut, root));
            }
            Some(&(n, c, rt)) => {
                if c != cut || rt != root {
                    out.push(v(
                        Prop::P1,
                        r.at,
                        Some(r.node),
                        format!(
                            "line {line} height {height}: cut {} root {} here, cut {} root {} on node {n}",
                            short(&cut),
                            short(&root),
                            short(&c),
                            short(&rt)
                        ),
                    ));
                }
            }
        }
    }
    // The replica's root at the final frame: the last root it reported for that frame, since a final frame is
    // never re-simulated afterwards.
    for &node in &t.honest {
        let mut last_root: BTreeMap<Frame, (Nanos, Hash)> = BTreeMap::new();
        for r in t.of(node) {
            if let Ev::ReplicaRoot { frame, root } = r.ev {
                last_root.insert(frame, (r.at, root));
            }
        }
        for r in t.of(node) {
            let Ev::Final { line, height, frame, root, .. } = r.ev else {
                continue;
            };
            match last_root.get(&frame) {
                Some(&(at, mine)) if mine != root && !t.excused(node, at) => out.push(v(
                    Prop::P1,
                    at,
                    Some(node),
                    format!(
                        "replica root {} at frame {frame} differs from final cut {line}/{height}'s root {}",
                        short(&mine),
                        short(&root)
                    ),
                )),
                None if !t.excused(node, r.at) => {
                    out.push(v(Prop::P1, r.at, Some(node), format!("no replica root recorded at final frame {frame}")))
                }
                _ => {}
            }
        }
    }
    out
}

/// P2. Every honest final cut's root equals an offline replay of the final log at that frame. `replay` maps
/// (line, frame) to the replay's root; it is built from S2 and a plain step loop, never from pe-agree.
pub fn p2_live_equals_replay(t: &Trace, replay: &BTreeMap<(Line, Frame), Hash>) -> Vec<Violation> {
    let mut out = Vec::new();
    for r in &t.records {
        let Ev::Final { line, height, frame, root, .. } = r.ev else {
            continue;
        };
        if !t.honest.contains(&r.node) {
            continue;
        }
        match replay.get(&(line, frame)) {
            Some(rep) if *rep == root => {}
            Some(rep) => out.push(v(
                Prop::P2,
                r.at,
                Some(r.node),
                format!("cut {line}/{height} at frame {frame}: live root {}, replay {}", short(&root), short(rep)),
            )),
            None => out.push(v(
                Prop::P2,
                r.at,
                Some(r.node),
                format!("the replay has no root at frame {frame} of line {line}"),
            )),
        }
    }
    out
}

/// P3. A replica never rolls back below its own floor (the newest final cut whose entries it holds in full), and
/// final heights on a line never decrease. Re-simulating between the floor and a newer `Final` it lacks entries for is
/// correct, and allowed.
pub fn p3_no_final_undone(t: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    for &node in &t.honest {
        let mut floor: Option<Frame> = None;
        let mut height: BTreeMap<Line, Height> = BTreeMap::new();
        for r in t.of(node) {
            match r.ev {
                Ev::Final { line, height: h, .. } => {
                    if let Some(&prev) = height.get(&line)
                        && h < prev
                    {
                        out.push(v(Prop::P3, r.at, Some(node), format!("line {line}: final height {h} after {prev}")));
                    }
                    height.insert(line, h.max(*height.get(&line).unwrap_or(&0)));
                }
                Ev::Floor { frame } => {
                    if let Some(f) = floor
                        && frame < f
                    {
                        out.push(v(Prop::P3, r.at, Some(node), format!("floor moved back from {f} to {frame}")));
                    }
                    floor = Some(floor.map_or(frame, |f| f.max(frame)));
                }
                Ev::RolledBack { frame } => {
                    if let Some(f) = floor
                        && frame < f
                    {
                        out.push(v(
                            Prop::P3,
                            r.at,
                            Some(node),
                            format!("rolled back to frame {frame}, below its floor {f}"),
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// A void as one node holds it: the deciding cut, and the reason.
type VoidVerdict = ((Line, Height), u16);

/// P4, the trace half: every void is decided by a named final cut, and honest nodes agree on each void. A node that
/// holds the deciding cut must hold the same void, with the same reason. (The independent re-check of included
/// chains needs the entries and comes with the runner.)
pub fn p4_voids(t: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut finals: BTreeMap<Node, Vec<(Line, Height)>> = BTreeMap::new();
    let mut voids: BTreeMap<Node, BTreeMap<EntryRef, VoidVerdict>> = BTreeMap::new();
    for r in &t.records {
        if !t.honest.contains(&r.node) {
            continue;
        }
        match r.ev {
            Ev::Final { line, height, .. } => finals.entry(r.node).or_default().push((line, height)),
            Ev::Voided { entry, by: None, .. } => {
                out.push(v(Prop::P4, r.at, Some(r.node), format!("{entry:?} voided with no deciding cut")));
            }
            Ev::Voided { entry, by: Some(c), reason } => {
                if let Some(&(c0, r0)) = voids.entry(r.node).or_default().get(&entry)
                    && (c0, r0) != (c, reason)
                {
                    out.push(v(
                        Prop::P4,
                        r.at,
                        Some(r.node),
                        format!("{entry:?} voided twice: {c0:?}/{r0}, then {c:?}/{reason}"),
                    ));
                }
                voids.entry(r.node).or_default().insert(entry, (c, reason));
            }
            _ => {}
        }
    }
    let end = t.records.last().map_or(0, |r| r.at);
    for (&a, va) in &voids {
        for (entry, &(cut, reason)) in va {
            for &b in &t.honest {
                if b == a || !finals.get(&b).is_some_and(|f| f.contains(&cut)) {
                    continue;
                }
                match voids.get(&b).and_then(|vb| vb.get(entry)) {
                    Some(&(c2, r2)) if (c2, r2) == (cut, reason) => {}
                    Some(&(c2, r2)) => out.push(v(
                        Prop::P4,
                        end,
                        Some(b),
                        format!(
                            "{entry:?}: node {a} voided it by {cut:?} for reason {reason}; this node by {c2:?} for {r2}"
                        ),
                    )),
                    None => out.push(v(
                        Prop::P4,
                        end,
                        Some(b),
                        format!("{entry:?}: node {a} voided it by {cut:?}; this node holds that cut and no void"),
                    )),
                }
            }
        }
    }
    out
}

/// P6. No call into code under test panicked on any node, and every invalid input from an adversary is refused, or
/// reported as a fault, by its receiver, naming the adversary.
pub fn p6_no_abort(t: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    for r in &t.records {
        if let Ev::Panicked { call, message } = &r.ev {
            out.push(v(Prop::P6, r.at, Some(r.node), format!("{call} panicked: {message}")));
        }
    }
    for bad in &t.invalid_inputs {
        let answered = t.of(bad.to).any(|r| {
            r.at >= bad.at
                && match r.ev {
                    Ev::Refused { entry, .. } => entry.author == bad.author,
                    Ev::Fault { author, .. } => author == bad.author,
                    _ => false,
                }
        });
        if !answered {
            out.push(v(
                Prop::P6,
                bad.at,
                Some(bad.to),
                format!("invalid input '{}' from {} was never refused", bad.label, bad.author),
            ));
        }
    }
    out
}

/// P8, the paused-frame half: an honest node never pauses. (Unconfirmed marks and the one-minute bound follow with
/// pe-agree's events.)
pub fn p8_no_pause(t: &Trace) -> Vec<Violation> {
    t.records
        .iter()
        .filter(|r| t.honest.contains(&r.node))
        .filter_map(|r| match r.ev {
            Ev::Paused { frame } => Some(v(Prop::P8, r.at, Some(r.node), format!("paused at frame {frame}"))),
            _ => None,
        })
        .collect()
}

/// P5. Once faults stop at `from`, every honest node sees a new final cut within `bound`.
pub fn p5_progress(t: &Trace, from: Nanos, bound: Nanos) -> Vec<Violation> {
    t.honest
        .iter()
        .filter(|&&n| !t.of(n).any(|r| matches!(r.ev, Ev::Final { .. }) && r.at >= from && r.at <= from + bound))
        .map(|&n| {
            v(
                Prop::P5,
                from + bound,
                Some(n),
                format!("no final cut within {bound} ns of the faults ending at {from} ns"),
            )
        })
        .collect()
}

/// Every default property of a scenario (DESIGN.md 5.2) that needs no extra input.
pub fn defaults(t: &Trace, replay: &BTreeMap<(Line, Frame), Hash>) -> Vec<Violation> {
    let mut out = p1_agreement(t);
    out.extend(p2_live_equals_replay(t, replay));
    out.extend(p3_no_final_undone(t));
    out.extend(p4_voids(t));
    out.extend(p6_no_abort(t));
    out.extend(p15_source_contract(t));
    out
}

/// P15. Every `Breach` the recorder saw on an honest node (DESIGN.md 5, P15).
///
/// The recorder counts an entry held once `receive` returns Ok: an agreement that drops a valid entry without refusing
/// it is a false negative here (DESIGN.md 13).
pub fn p15_source_contract(t: &Trace) -> Vec<Violation> {
    t.records
        .iter()
        .filter(|r| t.honest.contains(&r.node))
        .filter_map(|r| match &r.ev {
            Ev::Breach { rule, detail } => Some(v(Prop::P15, r.at, Some(r.node), format!("{rule}: {detail}"))),
            _ => None,
        })
        .collect()
}

/// The planted-violation tests (DESIGN.md 6.2): a clean trace passes every checker, and each planted fault is
/// reported by its own checker, with its own property id.
#[cfg(test)]
mod bite {
    use super::*;
    use crate::trace::InvalidInput;

    const A: Hash = [1; 32];
    const B: Hash = [2; 32];

    /// Three honest nodes; two final cuts; one void decided by the second cut; one refused invalid input from the
    /// faulty node 4; one rollback to the first final frame (allowed).
    fn clean() -> (Trace, BTreeMap<(Line, Frame), Hash>) {
        let mut t = Trace { honest: [1, 2, 3].into(), ..Default::default() };
        let e = EntryRef { author: 4, seq: 7 };
        for n in 1..=3 {
            t.push(10, n, Ev::ReplicaRoot { frame: 10, root: A });
            t.push(20, n, Ev::Final { line: 0, height: 1, frame: 10, cut: A, root: A });
            t.push(21, n, Ev::Floor { frame: 10 });
            t.push(25, n, Ev::RolledBack { frame: 10 });
            t.push(30, n, Ev::ReplicaRoot { frame: 20, root: B });
            t.push(40, n, Ev::Final { line: 0, height: 2, frame: 20, cut: B, root: B });
            t.push(41, n, Ev::Voided { entry: e, by: Some((0, 2)), reason: 3 });
        }
        t.invalid_inputs.push(InvalidInput { at: 5, author: 4, to: 1, label: "garbage".into() });
        t.push(6, 1, Ev::Fault { author: 4, reason: 1 });
        let replay = [((0, 10), A), ((0, 20), B)].into();
        (t, replay)
    }

    fn props_of(vs: &[Violation]) -> Vec<Prop> {
        let mut p: Vec<Prop> = vs.iter().map(|v| v.prop).collect();
        p.dedup();
        p
    }

    #[test]
    fn the_clean_trace_passes_everything() {
        let (t, r) = clean();
        let all = defaults(&t, &r);
        assert!(all.is_empty(), "{all:#?}");
        assert!(p8_no_pause(&t).is_empty());
    }

    #[test]
    fn p1_bites_on_a_split_cut_and_on_a_diverged_replica() {
        let (mut t, _) = clean();
        t.push(50, 2, Ev::Final { line: 0, height: 3, frame: 30, cut: A, root: A });
        t.push(49, 2, Ev::ReplicaRoot { frame: 30, root: A });
        t.push(50, 3, Ev::Final { line: 0, height: 3, frame: 30, cut: B, root: B });
        t.push(49, 3, Ev::ReplicaRoot { frame: 30, root: B });
        assert_eq!(props_of(&p1_agreement(&t)), vec![Prop::P1]);

        let (mut t, _) = clean();
        t.push(31, 3, Ev::ReplicaRoot { frame: 20, root: A }); // node 3's replica differs at the final frame
        let vs = p1_agreement(&t);
        assert_eq!(vs.len(), 1, "{vs:#?}");
        assert_eq!(vs[0].node, Some(3));
    }

    #[test]
    fn p1_excuses_a_deliberately_corrupted_node_only_inside_its_window() {
        let (mut t, _) = clean();
        t.push(31, 3, Ev::ReplicaRoot { frame: 20, root: A });
        t.corrupted.insert(3, (30, 100));
        assert!(p1_agreement(&t).is_empty());
        t.corrupted.insert(3, (30, 31));
        assert_eq!(props_of(&p1_agreement(&t)), vec![Prop::P1]);
    }

    #[test]
    fn p2_bites_when_the_replay_differs_or_is_short() {
        let (t, mut r) = clean();
        r.insert((0, 20), A);
        assert_eq!(props_of(&p2_live_equals_replay(&t, &r)), vec![Prop::P2]);
        let (t, mut r) = clean();
        r.remove(&(0, 20)); // the replay was given one entry fewer and stopped short
        assert_eq!(p2_live_equals_replay(&t, &r).len(), 3);
    }

    #[test]
    fn p3_bites_below_the_floor_but_not_at_it() {
        let (mut t, _) = clean();
        t.push(44, 2, Ev::Floor { frame: 20 });
        t.push(45, 2, Ev::RolledBack { frame: 20 }); // at the floor: the base of a replay
        assert!(p3_no_final_undone(&t).is_empty());
        t.push(46, 2, Ev::RolledBack { frame: 19 });
        let vs = p3_no_final_undone(&t);
        assert_eq!((vs.len(), vs[0].prop, vs[0].node), (1, Prop::P3, Some(2)));
    }

    #[test]
    fn p3_lets_a_non_voter_resimulate_between_its_floor_and_a_newer_final_cut() {
        // @design-reviewer's case (mb5f08692ac31): node 3 learns cut 0/3 at frame 30 is final before it holds all of
        // its entries. Its floor stays at 20, and re-simulating from 25 is correct. Once it holds them, its floor is
        // 30, and the same rollback undoes a final frame.
        let (mut t, _) = clean();
        t.push(44, 3, Ev::Floor { frame: 20 });
        t.push(50, 3, Ev::Final { line: 0, height: 3, frame: 30, cut: A, root: A });
        t.push(51, 3, Ev::RolledBack { frame: 25 });
        assert!(p3_no_final_undone(&t).is_empty());
        t.push(52, 3, Ev::Floor { frame: 30 });
        t.push(53, 3, Ev::RolledBack { frame: 25 });
        assert_eq!(p3_no_final_undone(&t).len(), 1);
    }

    #[test]
    fn p3_bites_when_a_floor_moves_back() {
        let (mut t, _) = clean();
        t.push(44, 1, Ev::Floor { frame: 20 });
        t.push(45, 1, Ev::Floor { frame: 15 });
        assert_eq!(props_of(&p3_no_final_undone(&t)), vec![Prop::P3]);
    }

    #[test]
    fn p3_bites_when_a_final_height_goes_back() {
        let (mut t, _) = clean();
        t.push(60, 1, Ev::Final { line: 0, height: 1, frame: 10, cut: A, root: A });
        assert_eq!(props_of(&p3_no_final_undone(&t)), vec![Prop::P3]);
    }

    #[test]
    fn p4_bites_on_a_void_by_arrival_a_missing_void_and_a_different_reason() {
        let (mut t, _) = clean();
        t.push(42, 1, Ev::Voided { entry: EntryRef { author: 4, seq: 9 }, by: None, reason: 3 });
        assert_eq!(props_of(&p4_voids(&t)), vec![Prop::P4]);

        let (mut t, _) = clean();
        t.records.retain(|r| !(r.node == 3 && matches!(r.ev, Ev::Voided { .. })));
        let vs = p4_voids(&t);
        assert!(!vs.is_empty() && vs.iter().all(|v| v.prop == Prop::P4 && v.node == Some(3)), "{vs:#?}");

        let (mut t, _) = clean();
        for r in &mut t.records {
            if r.node == 2
                && let Ev::Voided { reason, .. } = &mut r.ev
            {
                *reason = 4;
            }
        }
        assert!(!p4_voids(&t).is_empty());
    }

    #[test]
    fn p4_does_not_demand_a_void_from_a_node_without_the_deciding_cut() {
        let (mut t, _) = clean();
        t.records.retain(|r| !(r.node == 3 && matches!(r.ev, Ev::Voided { .. } | Ev::Final { height: 2, .. })));
        assert!(p4_voids(&t).is_empty());
    }

    #[test]
    fn p6_bites_on_a_panic_and_on_an_unrefused_invalid_input() {
        let (mut t, _) = clean();
        t.push(70, 2, Ev::Panicked { call: "receive".into(), message: "index out of bounds".into() });
        assert_eq!(props_of(&p6_no_abort(&t)), vec![Prop::P6]);

        let (mut t, _) = clean();
        t.invalid_inputs.push(InvalidInput { at: 50, author: 4, to: 2, label: "forged hand".into() });
        assert_eq!(p6_no_abort(&t).len(), 1);
        t.push(51, 2, Ev::Refused { entry: EntryRef { author: 4, seq: 8 }, reason: 2 });
        assert!(p6_no_abort(&t).is_empty());
    }

    #[test]
    fn p6_does_not_accept_a_refusal_from_before_the_input_or_naming_someone_else() {
        let (mut t, _) = clean();
        t.invalid_inputs.push(InvalidInput { at: 50, author: 4, to: 2, label: "garbage".into() });
        t.push(49, 2, Ev::Fault { author: 4, reason: 1 });
        t.push(52, 2, Ev::Fault { author: 3, reason: 1 });
        assert_eq!(p6_no_abort(&t).len(), 1);
    }

    #[test]
    fn p5_bites_when_a_node_makes_no_progress_after_the_faults_stop() {
        let (t, _) = clean();
        assert!(p5_progress(&t, 15, 30).is_empty(), "every node finalized at 20 and 40");
        let v = p5_progress(&t, 41, 1_000);
        assert_eq!(v.len(), 3);
        assert!(v.iter().all(|x| x.prop == Prop::P5));
        let (mut t, _) = clean();
        t.records.retain(|r| !(r.node == 2 && matches!(r.ev, Ev::Final { height: 2, .. })));
        assert_eq!(p5_progress(&t, 30, 20).iter().map(|x| x.node).collect::<Vec<_>>(), vec![Some(2)]);
    }

    #[test]
    fn p8_bites_on_one_paused_frame_of_an_honest_node_only() {
        let (mut t, _) = clean();
        t.push(80, 4, Ev::Paused { frame: 50 });
        assert!(p8_no_pause(&t).is_empty(), "a faulty node's pause is not P8's business");
        t.push(80, 1, Ev::Paused { frame: 50 });
        assert_eq!(props_of(&p8_no_pause(&t)), vec![Prop::P8]);
    }
    #[test]
    fn p15_bites_on_a_breach_of_an_honest_node_only() {
        let (mut t, r) = clean();
        let breach = || Ev::Breach { rule: "wants_roots", detail: "cut 2/20 names 1 entries not held".into() };
        t.push(90, 4, breach());
        assert!(p15_source_contract(&t).is_empty(), "a faulty node's breach is not P15's business");
        t.push(90, 2, breach());
        assert_eq!(props_of(&defaults(&t, &r)), vec![Prop::P15]);
    }
}
