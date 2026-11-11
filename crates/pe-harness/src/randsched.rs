//! randsched: random fault schedules, and a shrinker for the ones that fail (DESIGN.md 7).
//!
//! A schedule is a pure function of its seed. Each kind of choice draws from its own stream, so adding a kind of
//! choice later does not move the existing ones (kept from the old widening). A failing schedule is shrunk one item
//! at a time, then in length, while it still fails the same way. The result is small enough to read, and it is the
//! regression scenario.
//!
//! This is the generator's M0 form, over [`crate::mock::LockstepNode`] and its faults: silences, partitions and
//! adversary garbage. The Byzantine shapes of the old widening, the GST stabilisation and P5 and P11 checks come with
//! pe-agree. The mock does not retransmit, so it can only be judged on safety (P1-P4, P6).

use std::collections::BTreeMap;

use crate::clock::{MS, Nanos, SEC};
use crate::kv::{KvReplica, Op, Template};
use crate::mock;
use crate::net::{LinkCfg, NodeId, Partition};
use crate::props::{self, Prop, Violation};
use crate::rng::{Purpose, below, draw};
use crate::sim::{NodeFault, NodeUnderTest, Scenario, reference_log, replay, run};

pub const HZ: u32 = 60;
pub const OBJECTS: [u128; 4] = [10, 11, 12, 13];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Garbage {
    /// Bytes that do not decode.
    Short,
    /// A well-formed message for `frame`, in the name of `name`, which is not the sender.
    ForgedName { name: NodeId, frame: u64, value: i64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Input { at: Nanos, node: NodeId, op: Op },
    Silent { node: NodeId, at: Nanos, until: Nanos },
    Partition { groups: Vec<Vec<NodeId>>, from: Nanos, until: Nanos },
    Garbage { from: NodeId, to: NodeId, at: Nanos, kind: Garbage },
}

impl Item {
    fn latest(&self) -> Nanos {
        match *self {
            Item::Input { at, .. } | Item::Garbage { at, .. } => at,
            Item::Silent { until, at, .. } => until.min(at + 10 * SEC),
            Item::Partition { until, .. } => until,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    pub seed: u64,
    pub members: Vec<NodeId>,
    /// The faulty member, if any: the last one.
    pub faulty: Option<NodeId>,
    pub link: LinkCfg,
    pub end: Nanos,
    pub items: Vec<Item>,
}

/// Streams: one per kind of choice.
mod stream {
    pub const SHAPE: u64 = 1;
    pub const INPUTS: u64 = 2;
    pub const FAULTS: u64 = 3;
    pub const GARBAGE: u64 = 4;
}

/// The schedule for `seed`.
pub fn generate(seed: u64) -> Schedule {
    let mut k = [0u64; 5];
    let mut d = |s: u64, n: u64| {
        let i = &mut k[s as usize];
        *i += 1;
        below(draw(seed, s, *i, Purpose::Schedule), n)
    };
    let n = 2 + d(stream::SHAPE, 4) as NodeId; // 2..=5 members
    let members: Vec<NodeId> = (1..=n).collect();
    let faulty = (n >= 3 && d(stream::SHAPE, 2) == 1).then_some(n);
    let end = (2 + d(stream::SHAPE, 3)) * SEC;
    let link = LinkCfg {
        delay_ns: (10 + d(stream::SHAPE, 50)) * MS,
        jitter_ns: d(stream::SHAPE, 20) * MS,
        ..Default::default()
    };
    let honest: Vec<NodeId> = members.iter().copied().filter(|&m| Some(m) != faulty).collect();
    let mut items = Vec::new();
    for _ in 0..1 + d(stream::INPUTS, 6) {
        let at = 100 * MS + d(stream::INPUTS, end - 1_100 * MS);
        let node = honest[d(stream::INPUTS, honest.len() as u64) as usize];
        let obj = OBJECTS[d(stream::INPUTS, 4) as usize];
        let op = match d(stream::INPUTS, 4) {
            0 => Op::Set { obj, field: 0, value: d(stream::INPUTS, 100) as i64 },
            1 => Op::Add { obj, field: 1, delta: 1 },
            2 => Op::Create { obj: obj + 100 },
            _ => Op::Delete { obj },
        };
        items.push(Item::Input { at, node, op });
    }
    if d(stream::FAULTS, 3) == 0 {
        let at = d(stream::FAULTS, end);
        let node = members[d(stream::FAULTS, n as u64) as usize];
        items.push(Item::Silent { node, at, until: at + d(stream::FAULTS, 2 * SEC) });
    }
    if n >= 3 && d(stream::FAULTS, 4) == 0 {
        let from = d(stream::FAULTS, end);
        let cut = 1 + d(stream::FAULTS, n as u64 - 1) as usize;
        let groups = vec![members[..cut].to_vec(), members[cut..].to_vec()];
        items.push(Item::Partition { groups, from, until: from + d(stream::FAULTS, 2 * SEC) });
    }
    if let Some(f) = faulty {
        for _ in 0..d(stream::GARBAGE, 3) {
            let to = honest[d(stream::GARBAGE, honest.len() as u64) as usize];
            let at = d(stream::GARBAGE, end - 500 * MS);
            let kind = if d(stream::GARBAGE, 2) == 0 {
                Garbage::Short
            } else {
                let name = honest[d(stream::GARBAGE, honest.len() as u64) as usize];
                let frame = 1 + d(stream::GARBAGE, end * u64::from(HZ) / SEC);
                Garbage::ForgedName { name, frame, value: 1_000 + d(stream::GARBAGE, 1_000) as i64 }
            };
            items.push(Item::Garbage { from: f, to, at, kind });
        }
    }
    Schedule { seed, members, faulty, link, end, items }
}

pub fn to_scenario(s: &Schedule) -> Scenario {
    let mut sc = Scenario::new(s.seed, s.end);
    sc.link = s.link.clone();
    sc.faulty.extend(s.faulty);
    for it in &s.items {
        match it {
            Item::Input { at, node, op } => sc.plan.push((*at, *node, op.clone())),
            Item::Silent { node, at, until } => {
                sc.faults.push(NodeFault::Silent { node: *node, at: *at, until: *until })
            }
            Item::Partition { groups, from, until } => {
                sc.partitions.push(Partition { groups: groups.clone(), from: *from, until: *until })
            }
            Item::Garbage { from, to, at, kind } => {
                let (bytes, label) = match kind {
                    Garbage::Short => (vec![0xEE; 7], "short".to_string()),
                    Garbage::ForgedName { name, frame, value } => (
                        mock::encode(*name, *frame, &[Op::Set { obj: OBJECTS[0], field: 9, value: *value }]),
                        format!("in the name of {name}"),
                    ),
                };
                sc.faults.push(NodeFault::Garbage { from: *from, to: *to, at: *at, bytes, label });
            }
        }
    }
    sc
}

/// What randsched runs a schedule against.
pub trait Target {
    type Node: NodeUnderTest;
    fn nodes(&self, s: &Schedule) -> BTreeMap<NodeId, Self::Node>;
    fn genesis(&self, s: &Schedule) -> KvReplica;
}

/// The lockstep mock, optionally with its planted bug.
pub struct MockTarget {
    pub accept_forged_name: bool,
}

impl Target for MockTarget {
    type Node = mock::LockstepNode;
    fn nodes(&self, s: &Schedule) -> BTreeMap<NodeId, mock::LockstepNode> {
        s.members
            .iter()
            .map(|&m| {
                let mut node = mock::LockstepNode::new(m, &s.members, &Template::AllAdmin, &OBJECTS, HZ);
                node.accept_forged_name = self.accept_forged_name;
                (m, node)
            })
            .collect()
    }
    fn genesis(&self, s: &Schedule) -> KvReplica {
        KvReplica::new(&s.members, &Template::AllAdmin, &OBJECTS)
    }
}

/// Runs one schedule and returns its safety violations (P1-P4, P6).
pub fn evaluate<T: Target>(t: &T, s: &Schedule) -> Vec<Violation> {
    let r = run(&to_scenario(s), t.nodes(s));
    props::defaults(&r.trace, &replay(t.genesis(s), &reference_log(&r)))
}

/// The properties a set of violations breaks, sorted and without repeats.
pub fn broken(vs: &[Violation]) -> Vec<Prop> {
    let mut p: Vec<Prop> = vs.iter().map(|v| v.prop).collect();
    p.sort();
    p.dedup();
    p
}

/// The first failing seed in `seeds`, with its violations.
pub fn search<T: Target>(t: &T, seeds: impl IntoIterator<Item = u64>) -> Option<(Schedule, Vec<Violation>)> {
    seeds.into_iter().find_map(|seed| {
        let s = generate(seed);
        let vs = evaluate(t, &s);
        (!vs.is_empty()).then_some((s, vs))
    })
}

/// Shrinks a failing schedule while it still breaks every property it broke at first. It drops items one at a time,
/// then halves the run's length down to just past its last item.
pub fn shrink<T: Target>(t: &T, mut s: Schedule) -> Schedule {
    let want = broken(&evaluate(t, &s));
    if want.is_empty() {
        return s;
    }
    let still = |c: &Schedule| {
        let got = broken(&evaluate(t, c));
        want.iter().all(|p| got.contains(p))
    };
    loop {
        let mut changed = false;
        for i in 0..s.items.len() {
            let mut c = s.clone();
            c.items.remove(i);
            if still(&c) {
                s = c;
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }
    let floor = s.items.iter().map(Item::latest).max().unwrap_or(0) + 500 * MS;
    while s.end / 2 >= floor {
        let mut c = s.clone();
        c.end /= 2;
        if !still(&c) {
            break;
        }
        s = c;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_pure_and_varied() {
        assert_eq!(generate(17), generate(17));
        let shapes: std::collections::BTreeSet<usize> = (0..200).map(|s| generate(s).members.len()).collect();
        assert_eq!(shapes, [2, 3, 4, 5].into());
        assert!((0..200).any(|s| { generate(s).items.iter().any(|i| matches!(i, Item::Garbage { .. })) }));
        assert!((0..200).any(|s| { generate(s).items.iter().any(|i| matches!(i, Item::Partition { .. })) }));
    }
}
