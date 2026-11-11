//! The run's trace: what every node did, stamped with global time (DESIGN.md 3.2, 5.3).
//!
//! The property checkers read only this. The event names follow the agreed TraceEvent vocabulary (DESIGN.md 10,
//! S3.8), but the types here are local stand-ins until pe-seams v0 carries `TraceEvent`. The recorder's own events
//! (`ReplicaRoot`, `Restore`, `Panicked`) come from the harness, not from code under test.

use std::collections::{BTreeMap, BTreeSet};

use crate::clock::Nanos;

pub type Node = u32;
pub type Frame = u64;
pub type Line = u64;
pub type Height = u64;
pub type Hash = [u8; 32];

/// One entry, named by its author's chain position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryRef {
    pub author: Node,
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    /// pe-agree: a cut became final at this node. `cut` is the cut's hash; `root` its state hash.
    Final { line: Line, height: Height, frame: Frame, cut: Hash, root: Hash },
    /// pe-agree: an entry is void, decided by the named final cut. `None` means it was decided some other way, which
    /// P4 forbids.
    Voided { entry: EntryRef, by: Option<(Line, Height)>, reason: u16 },
    /// pe-agree or the replica: an input refused, naming its author.
    Refused { entry: EntryRef, reason: u16 },
    /// pe-agree: `receive` returned a fault naming an author.
    Fault { author: Node, reason: u16 },
    /// pe-agree: a frame at which the node did not apply local input or step. Must never happen (P8).
    Paused { frame: Frame },
    /// Recorder: the replica's root after stepping to `frame`.
    ReplicaRoot { frame: Frame, root: Hash },
    /// Recorder: the replica's floor moved to `frame`. That is the frame of the newest final cut whose entries this
    /// node holds in full (`EntrySource::final_cut`). It can lag the line's `Final`: a non-voter can learn that a
    /// cut is final before all its entries arrive (engine-architect mb5f08692ac31).
    Floor { frame: Frame },
    /// Recorder: the replica answered `Damaged` for the cut at `frame`, from `root_at_cut` when voting or from
    /// `finalize` (S3 v1.2): a frame the root needs, at or above the floor, is no longer kept. The floor stays, and
    /// rung 2 is due. Once per cut.
    Damaged { frame: Frame },
    /// Recorder: an `Advance` rolled back to `frame` (`Advance.rolled_back_to`) and replayed forward from it.
    RolledBack { frame: Frame },
    /// Recorder: the entry source broke its contract (P15): it handed over a cut through `rule` (`wants_roots` or
    /// `final_cut`) while the member lacked an entry the cut names, it reported a void (`verdict`) of an entry it
    /// does not hold, a final cut's height went back, or `entries_at` below the final frame held an entry the final
    /// cut does not cover or has voided. All per member (S3 v1.1): never an ordering across members.
    Breach { rule: &'static str, detail: String },
    /// Recorder: a call into code under test panicked.
    Panicked { call: String, message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub at: Nanos,
    pub node: Node,
    pub ev: Ev,
}

/// An invalid input that an adversary sent to a node (garbage, forged, foreign, beyond the rate, equivocating).
/// Legal behaviour on the wire is not listed (P6 is scoped to invalid inputs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidInput {
    pub at: Nanos,
    pub author: Node,
    pub to: Node,
    pub label: String,
}

#[derive(Clone, Debug, Default)]
pub struct Trace {
    /// The nodes the scenario did not make faulty.
    pub honest: BTreeSet<Node>,
    /// Nodes whose replica the scenario corrupted on purpose, with the window in which P1 leaves them out of the
    /// root comparison: from the corruption until its repair is due (P1, P12).
    pub corrupted: BTreeMap<Node, (Nanos, Nanos)>,
    pub records: Vec<Record>,
    pub invalid_inputs: Vec<InvalidInput>,
}

impl Trace {
    pub fn push(&mut self, at: Nanos, node: Node, ev: Ev) {
        self.records.push(Record { at, node, ev });
    }

    pub fn of(&self, node: Node) -> impl Iterator<Item = &Record> {
        self.records.iter().filter(move |r| r.node == node)
    }

    pub fn excused(&self, node: Node, at: Nanos) -> bool {
        self.corrupted.get(&node).is_some_and(|&(from, until)| at >= from && at < until)
    }
}
