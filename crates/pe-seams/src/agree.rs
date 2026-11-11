//! Seam S3, agreement: agreed cuts at fixed membership (plan 3.2; models/cuts; divergence.md 11.2; the harness's
//! DESIGN.md 10).
//!
//! - **Sans-IO, no hidden inputs:** no wall clock, no unseeded randomness, no iteration order that reaches an
//!   output. Time comes in as [`HostTime`]; the frame comes from pe-net's clock model through [`FrameClock`]; keys and
//!   the seed come in pe-agree's configuration.
//! - **Quorum:** `t = floor((n-1)/3)`, `q = floor((n+t)/2) + 1` ([`quorum`]); Byzantine members are modelled only at
//!   `n >= 4`.
//! - **Votes are over (cut, the voter's own root)**, so a cut is final only with a root that a quorum computed for
//!   itself. A member whose root differs still votes; its vote counts for nothing, and everyone sees it. When no root
//!   can reach a quorum, [`AgreeEvent::RootSplit`]. After a repair the driver votes again ([`Agreement::vote`]).
//! - **Cuts** come on a fixed cadence (a duration, a world setting), carry the line id, and name per author the last
//!   entry they include.
//! - **Dissemination before finality** (S3 v1.1, RULINGS.md): an entry held by any honest member of a log is held by
//!   each honest member of that log before that member reports the first cut that includes it through
//!   [`EntrySource::final_cut`], or reports a verdict on it under that cut ([`AgreeEvent::Voided`]). The promise is
//!   per member, not a barrier: one honest member may report a cut while another is still fetching its entries. So
//!   honest members apply and void the same entries, at the same cut. An entry that only its faulty author holds is
//!   out of scope. pe-agree relays each new entry once (the fast path), and keeps the promise by fetching what a cut
//!   names, from any voter, before it reports the cut.
//! - **Only an included entry can be void** (S3 v1.1): a cut voids the late and back-stamped entries it includes
//!   ([`VoidReason`]). An entry stamped below a final cut's frame that no final cut includes yet is pending: it is
//!   held but never applied, on any member, until the first cut whose head passes it voids it as
//!   [`VoidReason::Late`]. Two different entries at one seq of a chain are a fork fault, never two pending entries.
//! - **No cut without its entries** (S3 v1.1): [`Agreement::wants_roots`] and [`EntrySource::final_cut`] never hand
//!   over a cut unless this member holds every entry it newly includes, void ones among them: for each author,
//!   every entry after the head the previous final cut named, up to the head this cut names. The d1 rule is S3's
//!   alone. The replica can't check it, because a cut may include void entries that [`EntrySource::entries_at`]
//!   rightly leaves out, so it relies on this line.
//! - **Retention for rung 1** (S3 v1.2): the replica tells S3 what it still needs
//!   ([`EntrySource::keep_final_from`]): `Cv`'s frame, for rung 1's replay, and its floor, for re-simulation, both
//!   forward only. S3 never trims a final entry at or above the floor except by the time cap: `T_diverge` (one
//!   minute, the user's divergence bound, divergence.md 14) plus `T_keep`, behind the frame of this member's final
//!   cut, in frames at the world's `SimRate`, never a fixed frame count. Below the floor it keeps from `Cv` on,
//!   unless the time cap or a byte limit (so a busy minute can't grow without bound) trimmed past it. It may trim at
//!   any time, a running replay included: the replica copes ([`crate::Replica::advance`]), so S3 never waits for it.
//!   [`EntrySource::kept_from`] reports what is kept, truthfully, and [`EntrySource::entries_at`] answers `None`
//!   below it. The window serves rung 1, and a merge only while it lasts ([`Agreement::final_entries`]); the world
//!   record on disk is separate and not bounded by it.
//! - **[`Agreement::receive`] never panics** on any bytes: bad bytes are `Err(Fault)` naming the sender, and an event.
//! - **The `mutants` feature** with named mutations lives in pe-agree (and pe-diverge, pe-world for their rules), never
//!   here: inert unless the node's configuration names one, and never in a production build.
//!
//! **Provisional in v0:** everything below quorum (unconfirmed play, the one-minute bound, line divergence) and the
//! merge events are named but their payloads are v1's (WS-model v3, M3). pe-agree's own message codec (proposals,
//! votes, timeouts, certificates) is pe-agree's, not a seam; its test-util export is @world-chunks's call.

use bytes::Bytes;

use crate::entry::{Entry, EntryError, SignedEntry};
use crate::ids::{ChainId, EntryRef, Frame, Hash, LineId, MemberKey};
use crate::replica::EntrySource;
use crate::time::HostTime;
use crate::transport::{FrameClock, Lane, PeerId};

/// The quorum at `n` members (D80 (b); coordinator ruling 2026-11-10): 2 at n = 2 or 3, 3 at 4, 4 at 5 or 6, 5 at 7.
pub fn quorum(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let t = (n - 1) / 3;
    (n + t) / 2 + 1
}

/// One author's last entry that a cut includes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainHead {
    pub chain: ChainId,
    pub author: MemberKey,
    pub seq: u64,
    pub entry: EntryRef,
}

/// A cut's address: its line and height, and the frame it closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CutRef {
    pub line: LineId,
    pub height: u64,
    pub frame: Frame,
}

/// A cut: a frame `f`, and for each author the last entry included. It covers the entries stamped below `f` that it
/// includes; an entry it includes that is late or back-stamped is void ([`VoidReason`]). An entry stamped below a
/// final cut's frame that the cut does not include is not void but pending, until a later cut includes it (S3 v1.1).
/// The root it is final with is in [`FinalCut`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cut {
    pub at: CutRef,
    /// Sorted by chain id; one head per chain that has entries in the cut or before it.
    pub heads: Vec<ChainHead>,
}

impl Cut {
    /// Whether the cut includes the entry at (chain, seq).
    pub fn includes(&self, chain: &ChainId, seq: u64) -> bool {
        match self.heads.binary_search_by(|h| h.chain.cmp(chain)) {
            Ok(i) => seq <= self.heads[i].seq,
            Err(_) => false,
        }
    }
}

/// A final cut: the cut, the root its quorum voted with, and the quorum's signatures over (cut, root).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalCut {
    pub cut: Cut,
    pub root: Hash,
    /// The voters whose votes carried `root`, with their signatures, sorted by key.
    pub voters: Vec<(MemberKey, crate::entry::Signature)>,
}

/// A member's last self-test result, carried in its votes, so a tie at n = 2 can be broken by the member whose guard
/// passed (divergence.md 5, step 2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SelfTest {
    #[default]
    NotRun,
    Passed,
    Failed,
}

/// A message to send, on a lane, to one peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub to: PeerId,
    pub lane: Lane,
    pub bytes: Bytes,
}

/// Why a peer's bytes were refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FaultKind {
    /// Not a well-formed message or entry.
    Malformed,
    /// Over a size or count limit.
    TooLarge,
    /// A signature that does not verify.
    BadSignature,
    /// Signed for another line.
    WrongLine,
    /// From, or naming, a key that is not a member.
    NotMember,
    /// Two different entries at one (chain id, seq): proof against the author.
    Equivocation,
    /// Beyond the author's entry rate.
    RateExceeded,
    /// Breaks a rule of the cut protocol (a vote for an unchecked cut, a stale view, ...).
    Protocol,
}

/// A peer's bad input. Never a panic; also raised as [`AgreeEvent::Fault`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    pub from: PeerId,
    /// The author it implicates, if not the sender.
    pub author: Option<MemberKey>,
    pub kind: FaultKind,
    pub detail: String,
}

impl Fault {
    pub fn entry(from: PeerId, e: EntryError) -> Fault {
        Fault { from, author: None, kind: FaultKind::Malformed, detail: format!("{e:?}") }
    }
}

/// Why a local entry was not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// The envelope is not this member's next entry (wrong author, chain, seq or prev).
    NotNext,
    Shape(EntryError),
    /// Stamped below the newest final cut's frame: it would be void.
    TooLate,
}

/// What agreement reports. Part of the harness's one trace vocabulary ([`crate::TraceEvent`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgreeEvent {
    /// A proposal for a cut was made or received.
    Proposed {
        cut: CutRef,
        proposer: MemberKey,
    },
    /// This member voted, with its root.
    Voted {
        cut: CutRef,
        root: Hash,
        self_test: SelfTest,
    },
    /// A cut is final on the line, with the root a quorum computed for itself, and who voted for it. The replica's
    /// source reports it as its final cut only once this member holds every entry it names, void ones included
    /// ([`EntrySource::final_cut`]).
    CutFinal {
        cut: CutRef,
        root: Hash,
        voters: Vec<MemberKey>,
    },
    /// Votes for one cut split so that no root can reach a quorum; every member sees who holds which root, with each
    /// voter's self-test verdict from its vote, for the tie rule at n = 2 (divergence.md 5).
    RootSplit {
        cut: CutRef,
        roots: Vec<(Hash, Vec<(MemberKey, SelfTest)>)>,
    },
    /// A final cut voided an entry (late, or stamped below its author's previous stamp).
    Voided {
        entry: EntryRef,
        author: MemberKey,
        reason: VoidReason,
        /// The final cut that decided it: a void is decided by a cut, never by arrival, and every member names the same
        /// cut for it (the harness's P4; @harness-eng, m3bb675a96918).
        cut: CutRef,
    },
    /// An entry was refused before ordering (bad signature, wrong line, rate).
    Refused {
        entry: EntryRef,
        author: MemberKey,
        kind: FaultKind,
    },
    Equivocation {
        author: MemberKey,
        chain: ChainId,
        seq: u64,
    },
    Joined {
        member: MemberKey,
        at: Frame,
    },
    Left {
        member: MemberKey,
        at: Frame,
    },
    Kicked {
        member: MemberKey,
        at: Frame,
    },
    /// No quorum is reachable. Play continues unconfirmed (Q5): this must never stop the simulation.
    QuorumLost {
        at: Frame,
    },
    /// Play is unconfirmed since this frame (provisional: v1).
    Unconfirmed {
        since: Frame,
    },
    /// Below quorum past the one-minute bound: the sides became separate lines (provisional: v1, M3).
    LineDiverged {
        line: LineId,
        at: Frame,
    },
    /// A merge entry took effect (provisional: M2).
    Merged {
        from: LineId,
        at: Frame,
    },
    /// Play paused. Must never fire (P8): it is in the vocabulary so the harness can prove it never appears.
    Paused {
        at: Frame,
    },
    /// A peer's bad bytes.
    Fault(Fault),
}

/// Why an entry was voided at a cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VoidReason {
    /// Stamped below the previous final cut's frame: an earlier final cut did not include it, so it can no longer be
    /// applied at its stamp.
    Late,
    /// Stamped below an earlier stamp in its author's chain.
    BackStamped,
}

/// What the driver reads for statistics and the harness's queries (DESIGN.md 10, S3.6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgreeStats {
    pub members: usize,
    pub quorum: usize,
    pub final_height: u64,
    pub final_frame: Option<Frame>,
    pub unconfirmed_since: Option<Frame>,
    pub pending_entries: usize,
}

/// S3. pe-agree implements it; it is also the replica's [`EntrySource`].
pub trait Agreement: EntrySource + Send {
    /// This member's line.
    fn line(&self) -> LineId;

    /// Take a local entry: check that it is this member's next one, sign it with the member key, record it, and queue
    /// it for every member (it goes out from the next [`Agreement::tick`]). The simulation thread calls it with the
    /// payload it built (pe-api's, for an edit); pe-api never calls S3.
    fn submit(&mut self, entry: Entry) -> Result<SignedEntry, SubmitError>;

    /// A message from a peer, as pe-net delivered it.
    fn receive(&mut self, from: PeerId, lane: Lane, bytes: &[u8], at: HostTime) -> Result<(), Fault>;

    /// Advance on local time. The frame comes from `clock`. Returns what to send.
    fn tick(&mut self, now: HostTime, clock: &dyn FrameClock) -> Vec<Outgoing>;

    /// The cuts this member should vote on now: it holds every entry each one names, void ones included (the d1
    /// rule, S3's alone: "No cut without its entries" in [`crate::agree`]). The driver
    /// answers each with [`Agreement::vote`], from [`crate::Replica::root_at_cut`].
    fn wants_roots(&self) -> Vec<Cut>;

    /// Vote for a cut with my root. Called again after a repair, with the repaired root (`revote`).
    fn vote(&mut self, cut: &CutRef, root: Hash, self_test: SelfTest);

    /// Do not vote for any cut at or past `frame` until released: a build of newly committed code is pending
    /// (logic.md 6.2, N3). Holds nest by frame; the earliest unreleased one counts.
    fn hold_votes_from(&mut self, frame: Frame);

    /// Release the hold placed at `frame`.
    fn release_votes_from(&mut self, frame: Frame);

    /// The final log between two final cuts, in agreed order: the fault bundle and the merge read it here, never from
    /// a node's internals; rung 1's replay reads the same log through [`EntrySource::entries_at`]. `None` when the
    /// log from `from` is no longer kept ([`EntrySource::kept_from`], S3 v1.2). The window keeps the log since `Cv`
    /// for up to `T_diverge`, which covers a split healed within `T_diverge` (divergence.md 9 and 11.1), unless the
    /// byte limit trimmed it sooner. Where a merge's base and log come from beyond that (the split point's log, or
    /// the world record, M3) is settled with the merge engine (M2); this window promises nothing more.
    fn final_entries(&self, from: &CutRef, to: &CutRef) -> Option<Vec<SignedEntry>>;

    /// The members, sorted by key.
    fn members(&self) -> Vec<MemberKey>;

    fn stats(&self) -> AgreeStats;

    /// Drain the events since the last call.
    fn events(&mut self) -> Vec<AgreeEvent>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quorum_matches_the_ruling() {
        let q: Vec<usize> = (1..=7).map(quorum).collect();
        assert_eq!(q, vec![1, 2, 2, 3, 4, 4, 5]);
    }

    #[test]
    fn a_cut_includes_up_to_each_head() {
        let c1 = ChainId(Hash::of("t", b"1"));
        let c2 = ChainId(Hash::of("t", b"2"));
        let mut heads = vec![
            ChainHead { chain: c1, author: MemberKey([1; 32]), seq: 3, entry: EntryRef(Hash::ZERO) },
            ChainHead { chain: c2, author: MemberKey([2; 32]), seq: 1, entry: EntryRef(Hash::ZERO) },
        ];
        heads.sort();
        let cut = Cut { at: CutRef { line: LineId::origin(), height: 1, frame: Frame(9) }, heads };
        assert!(cut.includes(&c1, 3));
        assert!(!cut.includes(&c1, 4));
        assert!(cut.includes(&c2, 1));
        assert!(!cut.includes(&ChainId(Hash::of("t", b"3")), 1));
    }
}
