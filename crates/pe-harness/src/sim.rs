//! The runner (DESIGN.md 3.2, 4): one session over the network model, driven to the end of its scenario, writing
//! the trace that the checkers read.
//!
//! The runner drives anything that implements [`NodeUnderTest`]: a node made of an agreement, a replica and the
//! rollback driver. Until pe-seams v0, the only one is the mock [`crate::mock::LockstepNode`]. With the seams, a
//! node is pe-agree (S3) plus a replica (S2) plus the shared driver, and this runner stays as it is.
//!
//! Each step, in order: deliveries that are due, scenario faults, plan inputs, `tick`, then the node's events into
//! the trace. Every call into a node is wrapped in `catch_unwind`. A panic is recorded (P6), and that node is not
//! driven again.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::clock::{LocalClock, MS, Nanos};
use crate::kv::{KvEntry, KvReplica, Op};
use crate::net::{Lane, LinkCfg, Net, NodeId, Partition};
use crate::trace::{Ev, Frame, Hash, InvalidInput, Line, Trace};

/// A fault a node reports from `receive`, naming the author it blames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub author: NodeId,
    pub reason: u16,
}

/// One outgoing message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Out {
    pub to: NodeId,
    pub lane: Lane,
    pub bytes: Vec<u8>,
}

/// What the runner needs from a node. A local stand-in for S3 plus S2 plus the driver, until pe-seams v0.
pub trait NodeUnderTest {
    /// A local input, applied at once on this node's own line (local-first).
    fn input(&mut self, now: i64, op: Op);
    fn receive(&mut self, now: i64, from: NodeId, bytes: &[u8]) -> Result<(), Fault>;
    /// `now` is the node's HostTime: local monotonic nanoseconds.
    fn tick(&mut self, now: i64) -> Vec<Out>;
    fn drain(&mut self) -> Vec<Ev>;
    /// The final log, in agreed order, frame by frame (S3's `final_entries`). P2's replay reads only this.
    fn final_entries(&self) -> Vec<(Frame, Vec<KvEntry>)>;
}

/// A scenario fault against a node or the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeFault {
    /// From `at` on, the node sends nothing; it still receives and runs.
    Silent { node: NodeId, at: Nanos, until: Nanos },
    /// From `at` on, the node is not driven at all.
    Crash { node: NodeId, at: Nanos },
    /// An adversary sends raw bytes. Recorded as an invalid input when it is delivered, and P6 requires its
    /// receiver to refuse it.
    Garbage { from: NodeId, to: NodeId, at: Nanos, bytes: Vec<u8>, label: String },
    /// An adversary sends bytes that are not invalid in themselves (a signed entry it withheld from others, one
    /// back-dated within the rules), so P6 expects no refusal. The probes' faulty authors use it (DESIGN.md 9.1).
    Send { from: NodeId, to: NodeId, at: Nanos, bytes: Vec<u8> },
}

/// A session to run: the nodes, the network, the plan and the faults. The Rust form of a scenario (DESIGN.md 4.1).
pub struct Scenario {
    pub seed: u64,
    pub end: Nanos,
    pub step: Nanos,
    pub link: LinkCfg,
    pub links: Vec<(NodeId, NodeId, LinkCfg)>,
    pub partitions: Vec<Partition>,
    pub clocks: BTreeMap<NodeId, LocalClock>,
    /// The nodes the scenario makes faulty. Everyone else is honest.
    pub faulty: BTreeSet<NodeId>,
    pub plan: Vec<(Nanos, NodeId, Op)>,
    pub faults: Vec<NodeFault>,
}

impl Scenario {
    pub fn new(seed: u64, end: Nanos) -> Self {
        Self {
            seed,
            end,
            step: MS,
            link: LinkCfg::default(),
            links: Vec::new(),
            partitions: Vec::new(),
            clocks: BTreeMap::new(),
            faulty: BTreeSet::new(),
            plan: Vec::new(),
            faults: Vec::new(),
        }
    }

    pub fn at(mut self, t: Nanos, node: NodeId, op: Op) -> Self {
        self.plan.push((t, node, op));
        self
    }

    pub fn fault(mut self, f: NodeFault) -> Self {
        self.faults.push(f);
        self
    }
}

/// The outcome of a run: the trace, and each node's final log for the replay.
pub struct Run {
    pub trace: Trace,
    pub final_logs: BTreeMap<NodeId, Vec<(Frame, Vec<KvEntry>)>>,
}

fn guarded<T>(trace: &mut Trace, at: Nanos, node: NodeId, call: &str, f: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(p) => {
            let message = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            trace.push(at, node, Ev::Panicked { call: call.into(), message });
            None
        }
    }
}

/// Runs `sc` over `nodes`. Deterministic: the same scenario and nodes give the same trace.
pub fn run<N: NodeUnderTest>(sc: &Scenario, mut nodes: BTreeMap<NodeId, N>) -> Run {
    let mut net = Net::new(sc.seed, sc.link.clone());
    for (a, b, cfg) in &sc.links {
        net.set_link(*a, *b, cfg.clone());
    }
    for p in &sc.partitions {
        net.add_partition(p.clone());
    }
    let mut trace =
        Trace { honest: nodes.keys().copied().filter(|n| !sc.faulty.contains(n)).collect(), ..Default::default() };
    let mut plan = sc.plan.clone();
    plan.sort_by_key(|&(t, n, _)| (t, n));
    let mut plan = plan.into_iter().peekable();
    let mut garbage: Vec<_> = sc
        .faults
        .iter()
        .filter_map(|f| match f {
            NodeFault::Garbage { from, to, at, bytes, label } => {
                Some((*at, *from, *to, bytes.clone(), Some(label.clone())))
            }
            NodeFault::Send { from, to, at, bytes } => Some((*at, *from, *to, bytes.clone(), None)),
            _ => None,
        })
        .collect();
    garbage.sort_by_key(|g| (g.0, g.1, g.2));
    let mut garbage = garbage.into_iter().peekable();
    let mut dead: BTreeSet<NodeId> = BTreeSet::new();
    let silent = |n: NodeId, t: Nanos| {
        sc.faults
            .iter()
            .any(|f| matches!(*f, NodeFault::Silent { node, at, until } if node == n && t >= at && t < until))
    };
    let crashed = |n: NodeId, t: Nanos| {
        sc.faults.iter().any(|f| matches!(*f, NodeFault::Crash { node, at } if node == n && t >= at))
    };
    let host = |n: NodeId, t: Nanos| sc.clocks.get(&n).copied().unwrap_or_default().host_time(t);

    let mut bad_in_flight: BTreeMap<(NodeId, NodeId, u64), String> = BTreeMap::new();
    let mut t: Nanos = 0;
    while t <= sc.end {
        for d in net.advance(t) {
            if dead.contains(&d.to) || crashed(d.to, t) {
                continue;
            }
            let Some(node) = nodes.get_mut(&d.to) else {
                continue;
            };
            // An invalid input counts from its delivery: one that a partition or loss dropped was never there to
            // refuse (found by randsched, seed 97).
            if let Some(label) = bad_in_flight.get(&(d.from, d.to, d.index)) {
                trace.invalid_inputs.push(InvalidInput { at: t, author: d.from, to: d.to, label: label.clone() });
            }
            let now = host(d.to, t);
            match guarded(&mut trace, t, d.to, "receive", || node.receive(now, d.from, &d.bytes)) {
                Some(Err(Fault { author, reason })) => trace.push(t, d.to, Ev::Fault { author, reason }),
                Some(Ok(())) => {}
                None => {
                    dead.insert(d.to);
                }
            }
        }
        while garbage.peek().is_some_and(|g| g.0 <= t) {
            let (_, from, to, bytes, label) = garbage.next().unwrap_or_default();
            if let Some(label) = label {
                bad_in_flight.insert((from, to, net.truth(from, to).offered), label);
            }
            net.send(t, from, to, Lane::Action, bytes);
        }
        while plan.peek().is_some_and(|p| p.0 <= t) {
            let Some((_, n, op)) = plan.next() else { break };
            if dead.contains(&n) || crashed(n, t) {
                continue;
            }
            if let Some(node) = nodes.get_mut(&n) {
                let now = host(n, t);
                if guarded(&mut trace, t, n, "input", || node.input(now, op)).is_none() {
                    dead.insert(n);
                }
            }
        }
        for (&n, node) in nodes.iter_mut() {
            if dead.contains(&n) || crashed(n, t) {
                continue;
            }
            let now = host(n, t);
            match guarded(&mut trace, t, n, "tick", || node.tick(now)) {
                Some(outs) => {
                    if !silent(n, t) {
                        for o in outs {
                            net.send(t, n, o.to, o.lane, o.bytes);
                        }
                    }
                }
                None => {
                    dead.insert(n);
                    continue;
                }
            }
            match guarded(&mut trace, t, n, "drain", || node.drain()) {
                Some(evs) => {
                    for ev in evs {
                        trace.push(t, n, ev);
                    }
                }
                None => {
                    dead.insert(n);
                }
            }
        }
        t += sc.step;
    }
    let final_logs = nodes.iter().map(|(&n, node)| (n, node.final_entries())).collect();
    Run { trace, final_logs }
}

/// P2's offline replay: a fresh replica stepped through a final log, frame by frame, with nothing from the
/// agreement. Returns the root at every final frame, on line 0. Frames with no entries still step.
pub fn replay(mut genesis: KvReplica, log: &[(Frame, Vec<KvEntry>)]) -> BTreeMap<(Line, Frame), Hash> {
    let mut out = BTreeMap::new();
    let by_frame: BTreeMap<Frame, &Vec<KvEntry>> = log.iter().map(|(f, e)| (*f, e)).collect();
    let Some(&last) = by_frame.keys().next_back() else {
        return out;
    };
    while genesis.frame() < last {
        let f = genesis.frame() + 1;
        let entries = by_frame.get(&f).map(|e| e.as_slice()).unwrap_or(&[]);
        genesis.step(entries);
        if by_frame.contains_key(&f) {
            out.insert((0, f), genesis.root_hash());
        }
    }
    out
}

/// The longest final log among the honest nodes: the replay's input. P1 checks that the others agree with it.
pub fn reference_log(run: &Run) -> Vec<(Frame, Vec<KvEntry>)> {
    run.final_logs
        .iter()
        .filter(|(n, _)| run.trace.honest.contains(n))
        .map(|(_, l)| l)
        .max_by_key(|l| l.len())
        .cloned()
        .unwrap_or_default()
}
