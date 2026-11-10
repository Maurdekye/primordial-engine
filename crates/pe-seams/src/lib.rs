//! Primordial Engine's seams, v0 (docs/refound-plan.md 3.2; the rulings are in `RULINGS.md` beside this crate's
//! manifest).
//!
//! Six seams let every workstream code against the others from the first week, with mocks for any side that is not
//! built yet:
//!
//! | Seam | Module | Implemented by | Used by |
//! |---|---|---|---|
//! | S1 Entries | [`entry`] | (the format itself; signing by pe-net-crypto) | everyone |
//! | S2 Replica | [`replica`] | pe-replica, with pe-world's tree | pe-app, pe-diverge, pe-harness |
//! | S3 Agreement | [`agree`] | pe-agree | pe-app, pe-diverge, pe-harness |
//! | S4 Transport | [`transport`] | pe-net | pe-app, pe-harness |
//! | S5 Frame view | [`view`] | pe-replica (and pe-xr's hands) | pe-render, pe-xr, pe-api |
//! | S6 WorldApi | `world_api` (feature `world-api`) | pe-api | pe-mcp, pe-app |
//!
//! Plus [`trace`], the harness's one trace vocabulary over S3's and pe-diverge's events, and [`time`].
//!
//! **Rules for every seam:**
//! - **Sans-IO:** pe-agree, pe-diverge and pe-net's protocol logic take time and messages as inputs and return
//!   outputs. No sockets, wall clocks or unseeded randomness.
//! - **No panics on a peer's input:** a bad message is an error and an event naming its author.
//! - **Every limit is a duration, never a frame count** ([`time`]); the simulation rate is a world setting,
//!   independent of the display rate.
//! - **Determinism:** nothing here iterates a hash map into an output; ordered collections only.
//!
//! **Versions.** This is v0. Items marked *provisional* in their module's documentation may change in v1, through
//! WS-seams (@engine-architect), which also reviews every seam change. Everything else is fixed for M1.
//!
//! **Transition.** The id, hash and position types ([`ids`]) belong in pe-core and move there when WS-world's
//! `world/core-v1` lands; their paths here (`pe_seams::Hash`, ...) stay the same.

pub mod agree;
pub mod entry;
pub mod ids;
pub mod replica;
pub mod time;
pub mod trace;
pub mod transport;
pub mod view;

#[cfg(feature = "world-api")]
pub mod world_api;

#[cfg(feature = "test-util")]
pub mod test_util;

pub use agree::{
    AgreeEvent, AgreeStats, Agreement, ChainHead, Cut, CutRef, Fault, FaultKind, FinalCut, Outgoing, SelfTest,
    SubmitError, VoidReason, quorum,
};
pub use entry::{Class, DomainId, Entry, EntryError, MAX_PAYLOAD, Signature, SignedEntry, Signer, Verifier};
pub use ids::{
    ChainId, EntryRef, Frame, Hash, Hasher, LineId, MemberKey, ORIGIN_ENTRY, ObjectId, Offset, OutOfRange, Pos,
};
pub use replica::{
    Advance, CutImport, EntrySource, FieldPath, Finalized, GroupSel, ImportError, LeafKey, Mismatch, Node, Note,
    Outcome, PhysicsImage, Reason, Record, Replica, RootError, Rung1, Stage, Tree, TreePath, Version,
};
pub use time::{HostTime, SimRate, frames_ceil};
pub use trace::{DivergeEvent, Rung, TraceEvent};
pub use transport::{FrameClock, Lane, LinkState, LinkStats, NetEvent, PeerId, Received, Transport};
pub use view::{
    Appearance, BlobRef, Effect, EffectCause, EffectKey, EffectKind, FrameView, Hand, HandFeed, HandInput, HandSource,
    Holder, Light, LightKind, ObjectClass, RigPose, RigTransform, Shape, StepRecord, ViewCounters, ViewFlags,
    ViewObject,
};
