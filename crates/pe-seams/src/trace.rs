//! One trace vocabulary (DESIGN.md 10, S3.8): what the harness records and its properties read, from both cores
//! that decide things about a session, agreement (S3) and divergence (pe-diverge). Each core returns its own event
//! type; the harness wraps them in [`TraceEvent`].
//!
//! The model (models/cuts) keeps its own action vocabulary; the harness's `conform` maps these onto it.

use crate::agree::{AgreeEvent, CutRef};
use crate::ids::{Frame, Hash};

/// A repair rung (divergence.md 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rung(pub u8);

/// What pe-diverge reports (divergence.md 11.4; @diverge-eng's list in DESIGN.md S3.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DivergeEvent {
    /// My root at a final cut differs from the quorum's.
    Diverged {
        cut: CutRef,
        mine: Hash,
        agreed: Hash,
    },
    RungStarted {
        cut: CutRef,
        rung: Rung,
    },
    RungFailed {
        cut: CutRef,
        rung: Rung,
        why: String,
    },
    Repaired {
        cut: CutRef,
        rung: Rung,
    },
    /// Diverged again soon after a repair: the fault is not transient.
    RepeatedDivergence {
        cut: CutRef,
    },
    RootSplitStarted {
        cut: CutRef,
    },
    RootSplitEnded {
        cut: CutRef,
    },
    /// A fault bundle was written (pe-app writes it to disk).
    BundleWritten {
        cut: CutRef,
        name: String,
    },
    /// A merge was computed (M2).
    MergeComputed {
        at: Frame,
    },
}

/// The harness's trace event: one vocabulary, two producers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceEvent {
    Agree(AgreeEvent),
    Diverge(DivergeEvent),
}

impl From<AgreeEvent> for TraceEvent {
    fn from(e: AgreeEvent) -> Self {
        TraceEvent::Agree(e)
    }
}

impl From<DivergeEvent> for TraceEvent {
    fn from(e: DivergeEvent) -> Self {
        TraceEvent::Diverge(e)
    }
}
