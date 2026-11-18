//! The cut protocol's consensus core: views, proposals, votes over (block, root), certificates, timeouts, the lock,
//! two-chain finality and leader reputation. It knows nothing of entries: a block carries a [`Cut`], and whether a cut
//! is acceptable (its entries held, linked, signed and stamped below its frame; its frame in bounds) is the node's
//! check, made before it calls [`Core::vote`]. The core enforces the rules that keep agreement safe whatever the cut.
//!
//! The rules are the checked model's (models/cuts, `on_propose`, `note_cert`, `leader_in`; docs/spec/agreement.md 3):
//!
//! - A member votes at most once per view, never after timing out in it, and only for a proposal of its current view
//!   from that view's leader whose parent is certified.
//! - **The lock:** the proposal extends the previous view's certificate, or carries a timeout certificate for the
//!   previous view and its parent is at least as high (by view) as the highest certificate in it.
//! - **Two-chain finality:** a block is final when its child, proposed in the very next view, is certified. Its
//!   ancestors are final with it.
//! - **Leader reputation** (DiemBFT v4, window of one): a seeded rotation, skipping the leader of the latest view that
//!   timed out, derived from the chain the proposal itself certifies (option (b), @design-reviewer; the model's
//!   `committed_in`): the highest ancestor of its parent whose child in that chain came in the very next view is
//!   final, and a view between two consecutive final blocks' views had no certified block, so it timed out.
//!   Proposer and voters derive the leader from the same data, whatever each has committed itself.
//! - **The adaptive timeout:** derived from the measured time a view takes, and growing after each timeout until views
//!   succeed again (required at n = 2: model v0.1, bound-n2-early and ctl-early).
//!
//! Where it departs from the model, and why: docs/spec/agreement.md 7. Votes go to every member, so each member
//! forms certificates itself.
//!
//! Sans-IO: time comes in as [`HostTime`], messages as values. Bad input is an `Err(CoreFault)`, never a panic.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use pe_seams::{
    Cut, CutRef, Frame, Hash, Hasher, HostTime, LineId, MemberKey, SelfTest, Signature, Signer, Verifier, quorum,
};

/// A block's name: the hash of its view, parent, proposer and cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub Hash);

/// A proposed cut, placed in a view on top of a parent block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub view: u64,
    pub parent: BlockId,
    pub proposer: MemberKey,
    pub cut: Cut,
}

impl Block {
    pub fn id(&self) -> BlockId {
        let mut h = Hasher::new("pe.agree.block");
        h.write_u64(self.view).write_hash(&self.parent.0).write(&self.proposer.0);
        write_cut(&mut h, &self.cut);
        BlockId(h.finish())
    }

    pub fn height(&self) -> u64 {
        self.cut.at.height
    }

    pub fn frame(&self) -> Frame {
        self.cut.at.frame
    }
}

fn write_cut(h: &mut Hasher, cut: &Cut) {
    h.write_hash(&cut.at.line.0).write_u64(cut.at.height).write_u64(cut.at.frame.0);
    h.write_u64(cut.heads.len() as u64);
    for x in &cut.heads {
        h.write_hash(&x.chain.0).write(&x.author.0).write_u64(x.seq).write_hash(&x.entry.0);
    }
}

/// A quorum certificate: q votes for one block with one root. The genesis certificate has no votes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Qc {
    pub block: BlockId,
    pub view: u64,
    pub root: Hash,
    /// Sorted by key, distinct.
    pub votes: Vec<(MemberKey, Signature)>,
}

/// A timeout certificate for a view: q members' timeouts, each with the view of its highest certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tc {
    pub view: u64,
    /// (member, view of its highest certificate, signature), sorted by key, distinct.
    pub timeouts: Vec<(MemberKey, u64, Signature)>,
}

impl Tc {
    /// The view of the highest certificate among its timeouts: the lock.
    pub fn high_view(&self) -> u64 {
        self.timeouts.iter().map(|t| t.1).max().unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub block: Block,
    /// Certifies `block.parent`.
    pub parent_qc: Qc,
    /// For the previous view, when it timed out.
    pub tc: Option<Tc>,
    /// The proposer's signature over the block id.
    pub sig: Signature,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vote {
    pub view: u64,
    pub block: BlockId,
    pub root: Hash,
    pub voter: MemberKey,
    /// Not signed: a hint for the tie rule at n = 2 (divergence.md 5), from an authenticated link.
    pub self_test: SelfTest,
    /// Not signed: the voter's chain heads beyond the cut, so the next proposer learns what it lacks and fetches it
    /// (bounded progress, model v0.2). A hint only; a wrong one costs a fetch.
    pub heads: Vec<pe_seams::ChainHead>,
    /// Not signed: how late the other members' entries reach this voter (stamp-ahead, agreement.md 3). A hint only;
    /// a wrong one moves the author's own lead, which pe-app clamps.
    pub lateness: Vec<Lateness>,
    pub sig: Signature,
}

/// How late one chain's entries reach the reporter, as its votes and timeouts carry it (stamp-ahead, agreement.md 3;
/// @engine-architect's plan): the 95th percentile of the arrival lateness over a window of the chain's
/// recent entries, in frames (negative when they arrive before their stamps), and `seq`, the seq of the entry at
/// that percentile. The author reads the reports on its own chain (`Agreement::own_lateness`) and corrects each by
/// the lead it stamped entry `seq` with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lateness {
    pub author: MemberKey,
    pub chain: pe_seams::ChainId,
    pub frames: i64,
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timeout {
    pub view: u64,
    pub high_qc: Qc,
    pub voter: MemberKey,
    /// Not signed: the member's chain heads beyond the final cut, as a vote's (a hint; a wrong one costs a fetch).
    /// An abstaining member's timeout carries its heads, so the next leader learns what it lacks (agreement.md 3).
    pub heads: Vec<pe_seams::ChainHead>,
    /// Not signed: as a vote's.
    pub lateness: Vec<Lateness>,
    /// Over (line, view, high_qc.view).
    pub sig: Signature,
}

/// A signed self-test verdict for one split cut (S3 v1.3): the only form in which a verdict counts. Relayed once
/// by every member that first records it, so two different verdicts from one member reach every honest member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub cut: CutRef,
    /// Names `cut` as its split.
    pub verdict: SelfTest,
    pub voter: MemberKey,
    /// Over (line, the split cut, the verdict): [`report_bytes`].
    pub sig: Signature,
}

/// A core message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    Propose(Proposal),
    Vote(Vote),
    Timeout(Timeout),
    /// Ask for a block by name (a certified ancestor this member lacks).
    Fetch(BlockId),
    /// A self-test report.
    Report(Report),
}

/// Why a message was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreFault {
    NotMember,
    BadSignature,
    /// A certificate that does not verify (too few votes, a duplicate, a bad signature, mixed roots).
    BadCertificate,
    /// The block breaks a structural rule (line, height, parent, view order).
    BadBlock,
    /// Two different votes, timeouts or proposals from one member in one view.
    Equivocation,
    /// A self-test report that names another split than its cut.
    BadReport,
    /// A vote whose view is not its block's view (or a vote for the genesis).
    BadVote,
}

/// What the core reports to its node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreEvent {
    Proposed {
        block: BlockId,
        proposer: MemberKey,
    },
    /// A block is final, in chain order, with the certificate that certified it.
    Final {
        block: Block,
        qc: Qc,
    },
    /// Votes for a block split so no root can reach a quorum.
    RootSplit {
        block: BlockId,
        roots: Vec<(Hash, Vec<(MemberKey, SelfTest)>)>,
    },
    /// Two final blocks that do not extend each other: more than t members are faulty. Never expected.
    Conflict {
        block: BlockId,
    },
    /// A member equivocated (votes, timeouts or proposals in one view).
    Equivocation {
        member: MemberKey,
        view: u64,
    },
    /// A member sent two different self-test verdicts for one split: shown as `Failed` from now on.
    VerdictEquivocation {
        member: MemberKey,
        cut: CutRef,
    },
}

/// Where a message goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum To {
    All,
    One(MemberKey),
}

/// Timing, every limit a duration (user ruling).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// The least time between a block and its child's proposal: the cut cadence.
    pub cadence_ns: i64,
    /// The view timeout's floor, beyond the cadence.
    pub min_timeout_ns: i64,
    /// Its ceiling.
    pub max_timeout_ns: i64,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing { cadence_ns: 100_000_000, min_timeout_ns: 300_000_000, max_timeout_ns: 10_000_000_000 }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub line: LineId,
    /// Sorted, distinct.
    pub members: Vec<MemberKey>,
    /// The line's first cut (height 0); its frame is where the line starts.
    pub genesis: Cut,
    pub timing: Timing,
    /// The membership epoch (agreement.md 8): 0 from genesis, then one more at each reconfiguring cut, whose cut is
    /// this epoch's `genesis`. Votes, timeouts and proposals are signed under the epoch's domain, so a message of
    /// another epoch counts for nothing in this one.
    pub epoch: u64,
}

/// The domain an epoch's messages are signed under: the line itself in epoch 0 (so its signatures are unchanged), a
/// hash of the line and the epoch after.
pub fn epoch_domain(line: LineId, epoch: u64) -> LineId {
    if epoch == 0 {
        return line;
    }
    let mut h = pe_seams::Hasher::new("pe.agree.epoch");
    h.write(&line.0.0);
    h.write_u64(epoch);
    LineId(h.finish())
}

/// The hash an invite pins an epoch's members by (agreement.md 8; @engine-architect, pe-app's invite): the line, the
/// epoch and the members, sorted and distinct.
pub fn members_hash(line: LineId, epoch: u64, members: &[MemberKey]) -> Hash {
    let mut sorted = members.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut h = pe_seams::Hasher::new("pe.agree.members");
    h.write(&line.0.0);
    h.write_u64(epoch);
    h.write_u64(sorted.len() as u64);
    for m in &sorted {
        h.write(&m.0);
    }
    h.finish()
}

/// The planted defects of the harness's mutation tests, named as the model's controls (docs/spec/agreement.md 6).
/// The names always exist; a node can be given one only with the `mutants` feature, and none is ever in production
/// state: without the feature no field holds one, and every check below is the constant `false`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mutation {
    /// d1: vote without holding and validating each chain in order back to the parent's head.
    VoteWithoutChainValidation,
    /// Vote for the proposer's cut without checking it at all.
    AcceptProposalUnchecked,
    /// A quorum one smaller than `quorum(n)`: split-brain.
    QuorumOneTooSmall,
    /// Vote ignoring the timeout certificate's highest certificate.
    NoLockRule,
    /// Hold entries without checking their author's signature.
    NoSignatureCheck,
    /// Never time out a view: a faulty proposer stalls the line.
    NoTimeouts,
    /// Votes carry no root: a diverged replica confirms without learning it.
    VoteWithoutRoot,
    /// No per-author rate budget (inert until the budget exists).
    RateLimitRemoved,
    /// Accept a cut's frame however far beyond the voter's present.
    HorizonRuleOff,
    /// The outage rule's voter side off: vote for a cut that omits an own entry stamped below its frame.
    NoAbstention,
    /// The outage rule's leader side off: name a cut's frame whatever backlog other members claim.
    NoBacklogCap,
}

/// The stored block, with what came with it.
#[derive(Clone, Debug)]
struct Stored {
    proposal: Proposal,
    /// When this member first held it.
    seen: HostTime,
    /// The reputation state of its chain, were it final.
    rep: Rep,
}

/// Leader reputation along a chain of final blocks: the view of the latest, and the member index of the leader of
/// the latest view that timed out (window of one).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rep {
    prev_view: u64,
    last_bad: Option<usize>,
}

/// What the leader derivation needs of a block, kept for committed blocks after their bodies are pruned.
#[derive(Clone, Copy, Debug)]
struct Hdr {
    parent: BlockId,
    view: u64,
    rep: Rep,
}

/// How many committed blocks' headers (and proposals) stay after pruning, so a voter that has committed further than a
/// proposal certifies can still derive that proposal's leader, and a member that fell behind can fetch the blocks it
/// missed. Past it, the voter refuses (a timeout, never safety), and a member further behind needs an import (join).
const HEADER_HISTORY: usize = 256;

/// Votes, timeouts and orphan proposals are kept only up to this many views above the current one (a faulty member
/// can't grow them without limit; certificates carried in proposals and timeouts still move a member past it).
const VIEW_WINDOW: u64 = 16;

/// Self-test reports are kept for split cuts from the committed height up to this many heights above it.
const REPORT_WINDOW: u64 = 64;
/// At most this many split cuts with a verdict per member (a bound on what a faulty member can make others store).
const REPORTS_PER_MEMBER: usize = 16;

/// A member's verdict for one split: the first signed one, or `Failed` once it sent a second, different one.
#[derive(Clone, Debug)]
struct Verdict {
    shown: SelfTest,
    equivocated: bool,
}

#[derive(Clone, Debug, Default)]
struct VoteSet {
    view: u64,
    by_root: BTreeMap<Hash, Vec<(MemberKey, Signature, SelfTest)>>,
}

/// What a leader needs to propose: the parent and the certificates its proposal carries.
#[derive(Clone, Debug)]
pub struct Lead {
    pub view: u64,
    pub parent: Block,
    pub parent_qc: Qc,
    pub tc: Option<Tc>,
    /// Not before this time (the cadence).
    pub not_before: HostTime,
}

pub struct Core {
    cfg: Config,
    me: MemberKey,
    q: usize,
    offset: u64,
    genesis: Block,
    genesis_id: BlockId,
    blocks: BTreeMap<BlockId, Stored>,
    /// Certificates by block.
    certs: BTreeMap<BlockId, Qc>,
    /// Proposals waiting for their parent block, per (view, proposer).
    orphans: BTreeMap<(u64, MemberKey), Proposal>,
    /// Blocks taken in after their parent arrived later than they did (a child before its parent).
    orphans_adopted: u64,
    shared_cuts: std::cell::Cell<u64>,
    /// Votes for a block not yet held: at most one per voter, the highest view, counted when the block arrives.
    pending_votes: BTreeMap<MemberKey, Vote>,
    requested: BTreeSet<BlockId>,
    /// The block a member proposed in a view, per (view, proposer).
    proposed_in: BTreeMap<(u64, MemberKey), BlockId>,
    // Votes: per view, per voter, the vote; per block, by root.
    votes_by_voter: BTreeMap<(u64, MemberKey), (BlockId, Hash)>,
    votes: BTreeMap<BlockId, VoteSet>,
    split_reported: BTreeSet<BlockId>,
    /// Views abandoned at once, whose timeout does not grow the next one: a root split was seen, this member
    /// abstained (the outage rule, agreement.md 3), or more than n - q members timed out in it, so it can't be
    /// certified.
    quick_views: BTreeSet<u64>,
    /// Views whose timeout doesn't grow the next one, though they run to their timer: this member led them and only
    /// its backlog cap stopped its cut (the outage rule).
    spared_views: BTreeSet<u64>,
    /// The chain heads this member's next timeouts carry (set by the node before each tick).
    timeout_heads: Vec<pe_seams::ChainHead>,
    /// The lateness reports this member's next votes and timeouts carry (set by the node before each tick).
    lateness: Vec<Lateness>,
    timeouts: BTreeMap<u64, BTreeMap<MemberKey, (u64, Signature)>>,
    tcs: BTreeMap<u64, Tc>,

    view: u64,
    view_entered: HostTime,
    high_qc: Qc,
    last_voted: u64,
    timed_out: u64,
    /// This member's timeouts for its current and previous view, re-sent every timeout period, so members that lost
    /// them (an outage past pe-net's idle limit, transport.md 3.2) can still form the certificate: the previous view's
    /// is the one whose certificate moved this member on, which the others may lack.
    recent_timeouts: BTreeMap<u64, Timeout>,
    last_resend: HostTime,
    /// Time out this view at the next tick: t + 1 members timed out in it, so at least one honest member is there.
    join_view: u64,
    proposed_view: u64,
    /// The timeout's backoff: views that timed out, halved at each commit, never reset by a certificate alone. On an
    /// asymmetric link a member can form every other view's certificate early and time out the next before its leader
    /// can propose, so views alternate certified and timed out, and the two-chain rule never commits (finality-stall
    /// item, @coordinator-opus; halving, not zeroing, so a steady slow link doesn't
    /// drop back to a too-short timeout after each commit, @fable-reviewer).
    consecutive_timeouts: u32,
    /// Exponentially weighted time from entering a view to its certificate, ns.
    view_time_ns: i64,
    /// Exponentially weighted time from entering a view to its rightful leader's proposal arriving, ns (None until the
    /// first sample, which is taken whole; a sample of 0, as when the proposal's own certificate starts the view, is a
    /// real sample, @design-reviewer's Low on 3ab9c3be). It includes the cadence the leader waits before proposing,
    /// which the view time leaves out, so the timeout's 2 x wait counts the cadence (0.1 s) twice over: harmless at
    /// that size, and it only lengthens the timeout (the reviewer's nit). Sampled once per view, from another member's proposal that passed the checks (and decayed by a
    /// view this member led and saw certified), clamped at this
    /// view's timeout, and also in a view that timed out before the proposal came (@fable-reviewer):
    /// the view time alone samples only views that succeeded, and misses a member that enters views ahead of its
    /// leader. The clamp means a leader can raise others' timeouts no faster than it already can by proposing late.
    proposal_wait_ns: Option<i64>,
    /// The last view whose proposal wait was sampled.
    wait_sampled: u64,

    committed: BlockId,
    committed_height: u64,
    committed_view: u64,
    /// A block whose finality is known but whose ancestors are still being fetched.
    commit_target: Option<BlockId>,
    /// Headers of recently committed blocks, oldest first (leader derivation below the committed block).
    history: BTreeMap<BlockId, Hdr>,
    history_order: VecDeque<BlockId>,
    /// The proposals of the same recently committed blocks, served to a member that fetches them to catch up.
    history_bodies: BTreeMap<BlockId, Proposal>,
    /// Self-test verdicts per split cut and member (S3 v1.3).
    verdicts: BTreeMap<CutRef, BTreeMap<MemberKey, Verdict>>,

    out: Vec<(To, Msg)>,
    events: Vec<CoreEvent>,
    #[cfg(feature = "mutants")]
    mutation: Option<Mutation>,
}

const TC_DOMAIN: &str = "pe.agree.timeout";
const VOTE_DOMAIN: &str = "pe.agree.vote";
const PROPOSE_DOMAIN: &str = "pe.agree.propose";
const REPORT_DOMAIN: &str = "pe.agree.selftest";

/// The bytes a vote signs.
pub fn vote_bytes(line: LineId, view: u64, block: BlockId, root: Hash) -> Vec<u8> {
    let mut h = Hasher::new(VOTE_DOMAIN);
    h.write_hash(&line.0).write_u64(view).write_hash(&block.0).write_hash(&root);
    h.finish().0.to_vec()
}

/// The bytes a timeout signs.
pub fn timeout_bytes(line: LineId, view: u64, high_view: u64) -> Vec<u8> {
    let mut h = Hasher::new(TC_DOMAIN);
    h.write_hash(&line.0).write_u64(view).write_u64(high_view);
    h.finish().0.to_vec()
}

/// The bytes a self-test report signs: (line, the split cut, the verdict).
pub fn report_bytes(cut: CutRef, verdict: SelfTest) -> Vec<u8> {
    let mut h = Hasher::new(REPORT_DOMAIN);
    h.write_hash(&cut.line.0).write_u64(cut.height).write_u64(cut.frame.0).write_u8(match verdict {
        SelfTest::NotRun => 0,
        SelfTest::Passed { .. } => 1,
        SelfTest::Failed { .. } => 2,
    });
    h.finish().0.to_vec()
}

/// The bytes a proposer signs.
pub fn propose_bytes(line: LineId, block: BlockId) -> Vec<u8> {
    let mut h = Hasher::new(PROPOSE_DOMAIN);
    h.write_hash(&line.0).write_hash(&block.0);
    h.finish().0.to_vec()
}

impl Core {
    pub fn new(cfg: Config, me: MemberKey, now: HostTime) -> Core {
        let q = quorum(cfg.members.len());
        let offset = {
            let h = Hash::of("pe.agree.rotation", &epoch_domain(cfg.line, cfg.epoch).0.0);
            u64::from_le_bytes(h.0[..8].try_into().unwrap_or([0; 8]))
        };
        let genesis =
            Block { view: 0, parent: BlockId(Hash::ZERO), proposer: MemberKey([0; 32]), cut: cfg.genesis.clone() };
        let genesis_id = genesis.id();
        let gqc = Qc { block: genesis_id, view: 0, root: Hash::ZERO, votes: Vec::new() };
        let mut certs = BTreeMap::new();
        certs.insert(genesis_id, gqc.clone());
        let base = cfg.timing.min_timeout_ns;
        Core {
            q,
            offset,
            genesis_id,
            blocks: BTreeMap::new(),
            certs,
            orphans: BTreeMap::new(),
            orphans_adopted: 0,
            shared_cuts: std::cell::Cell::new(0),
            pending_votes: BTreeMap::new(),
            requested: BTreeSet::new(),
            proposed_in: BTreeMap::new(),
            votes_by_voter: BTreeMap::new(),
            votes: BTreeMap::new(),
            split_reported: BTreeSet::new(),
            quick_views: BTreeSet::new(),
            timeout_heads: Vec::new(),
            lateness: Vec::new(),
            spared_views: BTreeSet::new(),
            timeouts: BTreeMap::new(),
            tcs: BTreeMap::new(),
            view: 1,
            view_entered: now,
            high_qc: gqc,
            last_voted: 0,
            timed_out: 0,
            recent_timeouts: BTreeMap::new(),
            last_resend: now,
            join_view: 0,
            proposed_view: 0,
            consecutive_timeouts: 0,
            view_time_ns: base,
            proposal_wait_ns: None,
            wait_sampled: 0,
            committed: genesis_id,
            committed_height: 0,
            committed_view: 0,
            commit_target: None,
            history: BTreeMap::new(),
            history_order: VecDeque::new(),
            history_bodies: BTreeMap::new(),
            verdicts: BTreeMap::new(),
            out: Vec::new(),
            events: Vec::new(),
            #[cfg(feature = "mutants")]
            mutation: None,
            me,
            genesis,
            cfg,
        }
    }

    /// Plant a defect (feature `mutants` only).
    #[cfg(feature = "mutants")]
    pub fn set_mutation(&mut self, m: Option<Mutation>) {
        self.mutation = m;
        let q = quorum(self.cfg.members.len());
        self.q = if m == Some(Mutation::QuorumOneTooSmall) { q.saturating_sub(1).max(1) } else { q };
    }

    #[cfg(feature = "mutants")]
    pub fn mutated(&self, m: Mutation) -> bool {
        self.mutation == Some(m)
    }

    #[cfg(not(feature = "mutants"))]
    #[inline(always)]
    pub fn mutated(&self, _m: Mutation) -> bool {
        false
    }

    /// The domain this epoch's messages are signed under.
    fn domain(&self) -> LineId {
        epoch_domain(self.cfg.line, self.cfg.epoch)
    }

    pub fn epoch(&self) -> u64 {
        self.cfg.epoch
    }

    pub fn view(&self) -> u64 {
        self.view
    }

    pub fn quorum(&self) -> usize {
        self.q
    }

    pub fn members(&self) -> &[MemberKey] {
        &self.cfg.members
    }

    /// The epoch's genesis block: the line's first cut in epoch 0, the reconfiguring cut E after.
    pub fn genesis(&self) -> &Block {
        &self.genesis
    }

    pub fn high_qc(&self) -> &Qc {
        &self.high_qc
    }

    pub fn committed(&self) -> (BlockId, u64) {
        (self.committed, self.committed_height)
    }

    /// A block this member holds whose cut is `cut`: the votable proposal's first (the block a vote goes to), then the
    /// genesis, then the one held of the highest view. `None` if two held blocks carry the cut under different
    /// parents: the cut's verdicts depend on its ancestry, and the cut doesn't name it (counted in `shared_cuts`).
    /// How many blocks arrived before their parent and were taken in once it came.
    pub fn orphans_adopted(&self) -> u64 {
        self.orphans_adopted
    }

    pub fn block_with_cut(&self, cut: &Cut) -> Option<&Block> {
        if let Some(p) = self.votable().filter(|p| p.block.cut == *cut) {
            return Some(&p.block);
        }
        if self.genesis.cut == *cut {
            return Some(&self.genesis);
        }
        let same: Vec<&Block> = self.blocks.values().map(|s| &s.proposal.block).filter(|b| b.cut == *cut).collect();
        if same.iter().any(|b| b.parent != same[0].parent) {
            // One cut under two parents: the answers may differ, and nothing says which block is meant
            // (@design-reviewer, @harness-eng). Counted, and no answer rather than a guess.
            self.shared_cuts.set(self.shared_cuts.get() + 1);
            return None;
        }
        same.into_iter().max_by_key(|b| (b.view, b.id().0.0))
    }

    /// How often a cut other than the votable one was looked up that two held blocks carry under different parents.
    /// It should stay 0; if it fires, `Cut` should come to name its parent or block (a seam change).
    pub fn shared_cuts(&self) -> u64 {
        self.shared_cuts.get()
    }

    /// The certified chain from block `to` up to the highest certified block, oldest first, each block with a
    /// certificate of it: the highest certificate for the top, and each child's `parent_qc` below it. `None` if the
    /// highest certified block doesn't descend from `to` within [`crate::epoch::MAX_CERT_BLOCKS`] blocks this member
    /// still holds. An epoch certificate (crate::epoch).
    pub fn certified_chain(&self, to: BlockId) -> Option<Vec<(Block, Qc)>> {
        let (proposals, top) = self.certified_proposals(to)?;
        Some(crate::epoch::chain_of(&proposals, &top))
    }

    /// The same chain as [`Core::certified_chain`], as the blocks' proposals (each carries its parent's certificate)
    /// and the top's certificate: what a member that missed the epoch's reconfiguring cut takes in
    /// ([`Core::adopt`]).
    pub fn certified_proposals(&self, to: BlockId) -> Option<(Vec<Proposal>, Qc)> {
        let mut out = Vec::new();
        let top = self.high_qc.clone();
        let mut id = top.block;
        loop {
            let p = self.blocks.get(&id).map(|s| &s.proposal).or_else(|| self.history_bodies.get(&id))?;
            out.push(p.clone());
            if id == to {
                break;
            }
            if out.len() >= crate::epoch::MAX_CERT_BLOCKS {
                return None;
            }
            id = p.block.parent;
        }
        out.reverse();
        Some((out, top))
    }

    /// The proposals of the recently committed blocks (at most [`HEADER_HISTORY`]), which this member serves to
    /// fetches: kept past the epoch's end to serve a member that missed its reconfiguring cut (agreement.md 8).
    pub fn committed_proposals(&self) -> Vec<Proposal> {
        self.history_order.iter().filter_map(|id| self.history_bodies.get(id).cloned()).collect()
    }

    /// Take in an epoch certificate (agreement.md 8: a member that missed its epoch's reconfiguring cut E): each
    /// proposal as a block this member fetched, then the top's certificate. The certificates count as any carried
    /// certificate does, so the two-chain rule commits E here, and the ancestors this member lacks are fetched as
    /// usual (`Msg::Fetch`). Every proposal and certificate is checked as on arrival; the first that fails is
    /// returned, and what came before it stays taken in (each is certified, so harmless).
    pub fn adopt(
        &mut self,
        proposals: Vec<Proposal>,
        top: &Qc,
        now: HostTime,
        verifier: &dyn Verifier,
    ) -> Result<(), CoreFault> {
        if proposals.last().is_none_or(|p| p.block.id() != top.block) {
            return Err(CoreFault::BadCertificate);
        }
        self.check_qc(top, verifier)?;
        let lowest = proposals.first().map(|p| p.block.parent);
        for p in proposals {
            let id = p.block.id();
            let marked = !self.blocks.contains_key(&id) && self.requested.insert(id);
            if let Err(f) = self.on_proposal(p, now, verifier) {
                // A forged copy of a real block mustn't stop this member from fetching the real one.
                if marked {
                    self.requested.remove(&id);
                }
                return Err(f);
            }
        }
        self.note_qc(top, now);
        if self.commit_target.is_some() {
            self.advance_commit();
        }
        // Ask again for the gap below E: a fetch is sent once per block, and one lost on a link that has just come back
        // (or before it did) would leave it open for good. Only the gap this certificate needs (@design-reviewer's Low 1
        // on 5d532552: `requested` also holds ids nobody will serve, which must not crowd it out): walk down from E's
        // parent through the orphans held to the first block neither held nor committed. Each certificate taken in
        // follows a fresh answer, so this repeats at the ask's back-off (1 s doubling to 8 s), not more often.
        if let Some(gap) = lowest.and_then(|id| self.gap_below(id)) {
            self.out.push((To::All, Msg::Fetch(gap)));
        }
        Ok(())
    }

    /// The first block below `id` this member lacks: `id` itself if it is neither held, held as an orphan, nor the
    /// committed block or the genesis; else the same walk from the orphan's parent. `None` once the walk reaches a held
    /// block (its ancestry is complete) or the committed one.
    fn gap_below(&self, mut id: BlockId) -> Option<BlockId> {
        let parents: BTreeMap<BlockId, BlockId> =
            self.orphans.values().map(|o| (o.block.id(), o.block.parent)).collect();
        for _ in 0..=parents.len() {
            if id == self.committed || id == self.genesis_id || self.blocks.contains_key(&id) {
                return None;
            }
            match parents.get(&id) {
                Some(p) => id = *p,
                None => return Some(id),
            }
        }
        None
    }

    pub fn block(&self, id: &BlockId) -> Option<&Block> {
        if *id == self.genesis_id {
            return Some(&self.genesis);
        }
        self.blocks.get(id).map(|s| &s.proposal.block)
    }

    /// Messages to send, and events, since the last call.
    pub fn take_out(&mut self) -> Vec<(To, Msg)> {
        std::mem::take(&mut self.out)
    }

    pub fn take_events(&mut self) -> Vec<CoreEvent> {
        std::mem::take(&mut self.events)
    }

    fn is_member(&self, k: &MemberKey) -> bool {
        self.cfg.members.binary_search(k).is_ok()
    }

    // ---- leaders ----

    fn rot(&self, v: u64) -> usize {
        let n = self.cfg.members.len() as u64;
        ((v.wrapping_sub(1)).wrapping_add(self.offset) % n) as usize
    }

    fn rep_leader(&self, v: u64, last_bad: Option<usize>) -> usize {
        let r = self.rot(v);
        if last_bad == Some(r) { (r + 1) % self.cfg.members.len() } else { r }
    }

    /// The chain's reputation once a block of view `bv` is final on top of `r`: the views between it and the previous
    /// final block timed out. Only the latest timed-out view's leader counts (window of one); every view of the gap
    /// derives its leader from the chain before it.
    fn rep_step(&self, r: Rep, bv: u64) -> Rep {
        let last_bad = if bv > r.prev_view + 1 { Some(self.rep_leader(bv - 1, r.last_bad)) } else { r.last_bad };
        Rep { prev_view: bv, last_bad }
    }

    /// A block's header: the genesis, a stored block, or a committed block whose body was pruned.
    fn hdr(&self, id: &BlockId) -> Option<Hdr> {
        if *id == self.genesis_id {
            return Some(Hdr { parent: BlockId(Hash::ZERO), view: 0, rep: Rep::default() });
        }
        if let Some(s) = self.blocks.get(id) {
            let b = &s.proposal.block;
            return Some(Hdr { parent: b.parent, view: b.view, rep: s.rep });
        }
        self.history.get(id).copied()
    }

    /// The reputation of the chain final within `b`'s own certified chain (the model's `committed_in`): that of the
    /// highest ancestor x below `b` whose child in that chain was proposed in the very next view. None if the chain
    /// runs below what this member still knows.
    fn committed_rep(&self, b: &BlockId) -> Option<Rep> {
        let mut child = self.hdr(b)?;
        loop {
            if child.view == 0 {
                return Some(Rep::default());
            }
            let x = self.hdr(&child.parent)?;
            if child.view == x.view + 1 {
                return Some(x.rep);
            }
            child = x;
        }
    }

    /// The leader of view `v` for a proposal on `parent`, derived from what that parent certifies.
    pub fn leader_on(&self, v: u64, parent: &BlockId) -> Option<MemberKey> {
        let r = self.committed_rep(parent)?;
        Some(self.cfg.members[self.rep_leader(v, r.last_bad)])
    }

    /// The leader of view `v` for a proposal on this member's highest certificate.
    pub fn leader(&self, v: u64) -> Option<MemberKey> {
        self.leader_on(v, &self.high_qc.block)
    }

    // ---- timing ----

    fn timeout_ns(&self) -> i64 {
        let t = self.cfg.timing;
        let mut d = (self.view_time_ns.saturating_mul(3))
            .max(self.proposal_wait_ns.unwrap_or(0).saturating_mul(2))
            .max(t.min_timeout_ns);
        for _ in 0..self.consecutive_timeouts.min(32) {
            d = d.saturating_add(d / 2);
            if d >= t.max_timeout_ns {
                break;
            }
        }
        t.cadence_ns.saturating_add(d.min(t.max_timeout_ns))
    }

    /// The current view's timeout, ns (beyond the cadence included).
    pub fn timeout(&self) -> i64 {
        self.timeout_ns()
    }

    /// The proposal-wait estimate, ns (0 before the first sample).
    pub fn proposal_wait(&self) -> i64 {
        self.proposal_wait_ns.unwrap_or(0)
    }

    /// When this view times out.
    pub fn deadline(&self) -> HostTime {
        self.view_entered.plus_nanos(self.timeout_ns())
    }

    fn enter(&mut self, v: u64, now: HostTime) {
        if v > self.view {
            self.view = v;
            self.view_entered = now;
        }
    }

    // ---- certificates ----

    fn check_qc(&self, qc: &Qc, verifier: &dyn Verifier) -> Result<(), CoreFault> {
        if qc.block == self.genesis_id && qc.view == 0 && qc.votes.is_empty() && qc.root == Hash::ZERO {
            return Ok(());
        }
        if qc.view == 0 || qc.votes.len() < self.q {
            return Err(CoreFault::BadCertificate);
        }
        let msg = vote_bytes(self.domain(), qc.view, qc.block, qc.root);
        for (i, (k, s)) in qc.votes.iter().enumerate() {
            if i > 0 && qc.votes[i - 1].0 >= *k {
                return Err(CoreFault::BadCertificate);
            }
            if !self.is_member(k) || !verifier.verify(k, &msg, s) {
                return Err(CoreFault::BadCertificate);
            }
        }
        Ok(())
    }

    fn check_tc(&self, tc: &Tc, verifier: &dyn Verifier) -> Result<(), CoreFault> {
        if tc.timeouts.len() < self.q {
            return Err(CoreFault::BadCertificate);
        }
        for (i, (k, hv, s)) in tc.timeouts.iter().enumerate() {
            if i > 0 && tc.timeouts[i - 1].0 >= *k {
                return Err(CoreFault::BadCertificate);
            }
            if *hv >= tc.view
                || !self.is_member(k)
                || !verifier.verify(k, &timeout_bytes(self.domain(), tc.view, *hv), s)
            {
                return Err(CoreFault::BadCertificate);
            }
        }
        Ok(())
    }

    /// A verified certificate: the highest one, the view, and the two-chain commit.
    fn note_qc(&mut self, qc: &Qc, now: HostTime) {
        if qc.view > self.high_qc.view {
            self.high_qc = qc.clone();
        }
        self.certs.entry(qc.block).or_insert_with(|| qc.clone());
        if qc.view >= self.view {
            if qc.view == self.view {
                // A view that succeeded: measure it. The backoff resets only at a commit (`consecutive_timeouts`).
                let took = now.0.saturating_sub(self.view_entered.0).saturating_sub(self.cfg.timing.cadence_ns).max(0);
                self.view_time_ns = (self.view_time_ns.saturating_mul(7).saturating_add(took)) / 8;
                // A view this member led and saw certified waited for no one's proposal: the wait estimate decays, so
                // a member that leads every view (the reputation rule at n = 2) doesn't keep a slow link's estimate
                // after the link recovered. Its own view time still covers its certificate's round trip.
                if self.proposed_view == qc.view {
                    self.proposal_wait_ns = self.proposal_wait_ns.map(|w| w.saturating_mul(7) / 8);
                }
            }
            self.enter(qc.view + 1, now);
        }
        self.try_commit_from(qc.block);
    }

    fn note_tc(&mut self, tc: Tc, now: HostTime) {
        let v = tc.view;
        self.tcs.entry(v).or_insert(tc);
        if v >= self.view {
            // A view abandoned at once (a root split, an abstention, or no certificate possible) is no sign of slow
            // links: it doesn't grow the timeout.
            // A spared view counts only if this member didn't propose in it after all (@design-reviewer's L1).
            let spared = self.spared_views.contains(&v) && self.proposed_view != v;
            if !self.quick_views.contains(&v) && !spared {
                self.consecutive_timeouts = self.consecutive_timeouts.saturating_add(1);
            }
            self.enter(v + 1, now);
        }
    }

    /// If block `x` is certified and its parent was proposed in the view just before, the parent is final.
    fn try_commit_from(&mut self, x: BlockId) {
        let Some(xb) = self.block(&x).cloned() else { return };
        if !self.certs.contains_key(&x) {
            return;
        }
        let Some(pb) = self.block(&xb.parent).cloned() else {
            self.request(xb.parent);
            return;
        };
        if pb.height() == 0 || xb.view != pb.view + 1 {
            return;
        }
        if pb.height() <= self.committed_height {
            return;
        }
        if self.commit_target.is_none_or(|t| self.block(&t).is_none_or(|tb| tb.height() < pb.height())) {
            self.commit_target = Some(xb.parent);
        }
        self.advance_commit();
    }

    /// Commit the target and its uncommitted ancestors, oldest first, once all of them are held.
    fn advance_commit(&mut self) {
        let Some(target) = self.commit_target else { return };
        let mut chain = Vec::new();
        let mut x = target;
        loop {
            if x == self.committed {
                break;
            }
            let Some(b) = self.block(&x) else {
                self.request(x);
                return;
            };
            if b.height() <= self.committed_height {
                // Reached the committed height on another branch.
                self.events.push(CoreEvent::Conflict { block: target });
                self.commit_target = None;
                return;
            }
            chain.push(x);
            x = b.parent;
        }
        self.commit_target = None;
        let mut decayed = false;
        for id in chain.into_iter().rev() {
            let block = self.block(&id).cloned().expect("walked above");
            let Some(qc) = self.certs.get(&id).cloned() else {
                // Every block but the target is certified by its child's parent_qc; the target by its own child.
                self.request(id);
                return;
            };
            self.committed = id;
            self.committed_height = block.height();
            self.committed_view = block.view;
            if !decayed {
                decayed = true;
                self.consecutive_timeouts /= 2;
            }
            if let Some(h) = self.hdr(&id) {
                self.history.insert(id, h);
                if let Some(s) = self.blocks.get(&id) {
                    self.history_bodies.insert(id, s.proposal.clone());
                }
                self.history_order.push_back(id);
                while self.history_order.len() > HEADER_HISTORY {
                    if let Some(old) = self.history_order.pop_front() {
                        self.history.remove(&old);
                        self.history_bodies.remove(&old);
                    }
                }
            }
            self.events.push(CoreEvent::Final { block, qc });
        }
        self.prune();
    }

    fn request(&mut self, id: BlockId) {
        if self.requested.insert(id) {
            self.out.push((To::All, Msg::Fetch(id)));
        }
    }

    fn prune(&mut self) {
        let h = self.committed_height;
        let keep = self.committed;
        self.blocks.retain(|id, s| *id == keep || s.proposal.block.height() > h);
        let cv = self.committed_view;
        self.certs.retain(|id, qc| qc.view >= cv || *id == keep);
        self.votes_by_voter.retain(|(v, _), _| *v >= cv);
        self.votes.retain(|_, s| s.view >= cv);
        let votes = &self.votes;
        self.split_reported.retain(|id| votes.contains_key(id));
        let v = self.view;
        self.timeouts.retain(|tv, _| *tv + 1 >= v && *tv >= cv);
        self.tcs.retain(|tv, _| *tv + 1 >= v);
        self.proposed_in.retain(|(pv, _), _| *pv >= cv);
        self.quick_views.retain(|sv| *sv + 1 >= v);
        self.spared_views.retain(|sv| *sv + 1 >= v);
        self.orphans.retain(|(pv, _), _| *pv >= cv);
        self.pending_votes.retain(|_, pv| pv.view >= cv);
        self.verdicts.retain(|c, _| c.height >= h);
    }

    // ---- receiving ----

    pub fn receive(
        &mut self,
        from: MemberKey,
        msg: Msg,
        now: HostTime,
        verifier: &dyn Verifier,
    ) -> Result<(), CoreFault> {
        if !self.is_member(&from) {
            return Err(CoreFault::NotMember);
        }
        match msg {
            Msg::Propose(p) => self.on_proposal(p, now, verifier),
            Msg::Vote(v) => {
                if v.voter != from {
                    return Err(CoreFault::BadSignature);
                }
                self.receive_vote(v, now, verifier)
            }
            Msg::Timeout(t) => {
                if t.voter != from {
                    return Err(CoreFault::BadSignature);
                }
                self.on_timeout(t, now, verifier)
            }
            Msg::Fetch(id) => {
                let p = self.blocks.get(&id).map(|s| &s.proposal).or_else(|| self.history_bodies.get(&id));
                if let Some(p) = p {
                    self.out.push((To::One(from), Msg::Propose(p.clone())));
                }
                Ok(())
            }
            Msg::Report(r) => {
                if !self.is_member(&r.voter) {
                    return Err(CoreFault::NotMember);
                }
                if !verifier.verify(&r.voter, &report_bytes(r.cut, r.verdict), &r.sig) {
                    return Err(CoreFault::BadSignature);
                }
                self.on_report(r)
            }
        }
    }

    // ---- self-test verdicts (S3 v1.3) ----

    /// Record a signed verdict. The first one from a member for a split is final; a second, different one is
    /// equivocation and shows `Failed`. A newly recorded report is relayed once, and every split of that cut is
    /// reported again with the verdicts now shown.
    fn on_report(&mut self, r: Report) -> Result<(), CoreFault> {
        if r.verdict.split() != Some(r.cut) || r.cut.line != self.cfg.line {
            return Err(CoreFault::BadReport);
        }
        if r.cut.height < self.committed_height || r.cut.height > self.committed_height + REPORT_WINDOW {
            // An honest relay can arrive after this member committed past the split, or before it caught up: drop it
            // silently, no fault.
            return Ok(());
        }
        let per_member = self.verdicts.values().filter(|m| m.contains_key(&r.voter)).count();
        let known = self.verdicts.get(&r.cut).and_then(|m| m.get(&r.voter)).cloned();
        match known {
            None => {
                if per_member >= REPORTS_PER_MEMBER {
                    return Err(CoreFault::BadReport);
                }
                self.verdicts
                    .entry(r.cut)
                    .or_default()
                    .insert(r.voter, Verdict { shown: r.verdict, equivocated: false });
            }
            Some(v) if v.equivocated || v.shown == r.verdict => return Ok(()),
            Some(_) => {
                let shown = SelfTest::Failed { split: r.cut };
                self.verdicts.entry(r.cut).or_default().insert(r.voter, Verdict { shown, equivocated: true });
                self.events.push(CoreEvent::VerdictEquivocation { member: r.voter, cut: r.cut });
            }
        }
        self.out.push((To::All, Msg::Report(r.clone())));
        self.reemit_splits(r.cut);
        Ok(())
    }

    /// Send this member's verdict as a signed report, unless it already sent one for that split: its first verdict
    /// is final, and a different second one from its driver is refused. False when nothing was sent.
    pub fn report(&mut self, verdict: SelfTest, signer: &dyn Signer) -> bool {
        let Some(cut) = verdict.split() else { return false };
        if self.verdicts.get(&cut).is_some_and(|m| m.contains_key(&self.me)) {
            return false;
        }
        let r = Report { cut, verdict, voter: self.me, sig: signer.sign(&report_bytes(cut, verdict)) };
        self.on_report(r).is_ok()
    }

    /// The verdict shown for `member` in a split of `cut`: its signed one, else `NotRun`.
    fn shown(&self, cut: &CutRef, member: &MemberKey) -> SelfTest {
        self.verdicts.get(cut).and_then(|m| m.get(member)).map_or(SelfTest::NotRun, |v| v.shown)
    }

    fn split_roots(&self, block: &BlockId, cut: &CutRef) -> Vec<(Hash, Vec<(MemberKey, SelfTest)>)> {
        let Some(set) = self.votes.get(block) else { return Vec::new() };
        set.by_root.iter().map(|(r, l)| (*r, l.iter().map(|x| (x.0, self.shown(cut, &x.0))).collect())).collect()
    }

    /// Report every split block of `cut` again, with the verdicts now shown (a newer RootSplit supersedes).
    fn reemit_splits(&mut self, cut: CutRef) {
        let blocks: Vec<BlockId> =
            self.split_reported.iter().copied().filter(|b| self.cut_ref(b) == Some(cut)).collect();
        for b in blocks {
            let roots = self.split_roots(&b, &cut);
            if !roots.is_empty() {
                self.events.push(CoreEvent::RootSplit { block: b, roots });
            }
        }
    }

    /// Whether this member's latest vote was for a block of `cut`, not yet final: a revote after a repair, whose
    /// view may already have ended (a split ends it at once, a certificate moves it on).
    pub fn last_vote_on(&self, cut: &CutRef) -> bool {
        if cut.height <= self.committed_height {
            return false;
        }
        self.votes_by_voter
            .iter()
            .rev()
            .find(|((_, voter), _)| *voter == self.me)
            .is_some_and(|(_, (b, _))| self.cut_ref(b) == Some(*cut))
    }

    /// Receive a vote whose signature the caller has not checked: verify it first.
    pub fn receive_vote(&mut self, v: Vote, now: HostTime, verifier: &dyn Verifier) -> Result<(), CoreFault> {
        if !self.is_member(&v.voter) {
            return Err(CoreFault::NotMember);
        }
        if !verifier.verify(&v.voter, &vote_bytes(self.domain(), v.view, v.block, v.root), &v.sig) {
            return Err(CoreFault::BadSignature);
        }
        self.on_vote(v, now)
    }

    fn on_proposal(&mut self, p: Proposal, now: HostTime, verifier: &dyn Verifier) -> Result<(), CoreFault> {
        let b = &p.block;
        let id = b.id();
        if !self.is_member(&b.proposer) {
            return Err(CoreFault::NotMember);
        }
        if !verifier.verify(&b.proposer, &propose_bytes(self.domain(), id), &p.sig) {
            return Err(CoreFault::BadSignature);
        }
        if b.cut.at.line != self.cfg.line || p.parent_qc.block != b.parent || b.view <= p.parent_qc.view {
            return Err(CoreFault::BadBlock);
        }
        self.check_qc(&p.parent_qc, verifier)?;
        if let Some(tc) = &p.tc {
            if tc.view + 1 != b.view {
                return Err(CoreFault::BadBlock);
            }
            self.check_tc(tc, verifier)?;
        }
        if self.blocks.contains_key(&id) {
            return Ok(());
        }
        let wanted = self.requested.remove(&id);
        // One block per proposer per view (a fetched one excepted: it is certified).
        if self.proposed_in.get(&(b.view, b.proposer)).is_some_and(|prev| *prev != id) && !wanted {
            self.events.push(CoreEvent::Equivocation { member: b.proposer, view: b.view });
            return Err(CoreFault::Equivocation);
        }
        // The certificates it carries count whatever becomes of the block.
        let pqc = p.parent_qc.clone();
        let tc = p.tc.clone();
        self.note_qc(&pqc, now);
        if let Some(tc) = tc {
            self.note_tc(tc, now);
        }
        let Some(parent) = self.block(&b.parent).cloned() else {
            if wanted {
                // A block this member fetched, whose parent it lacks too: keep it, still marked as fetched, so the
                // chain fills in one round trip per missing block rather than one walk down the gap per block.
                self.requested.insert(id);
                self.orphans.insert((b.view, b.proposer), p.clone());
            } else if b.view >= self.view && b.view <= self.view + VIEW_WINDOW {
                self.orphans.insert((b.view, b.proposer), p.clone());
            }
            self.request(b.parent);
            return Ok(());
        };
        if b.cut.at.height != parent.height() + 1 || b.view <= parent.view {
            return Err(CoreFault::BadBlock);
        }
        if !wanted && b.view != self.view {
            // Neither this view's proposal nor a block this member asked for.
            return Ok(());
        }
        self.proposed_in.insert((b.view, b.proposer), id);
        // The proposal wait: once per view, from the view's rightful leader (another member), clamped at the timeout.
        // A proposal that comes after this member timed out in the view still counts, at the clamp.
        if b.view == self.view
            && self.wait_sampled < b.view
            && b.proposer != self.me
            && self.leader_on(b.view, &b.parent) == Some(b.proposer)
        {
            self.wait_sampled = b.view;
            let wait = now.0.saturating_sub(self.view_entered.0).clamp(0, self.timeout_ns());
            self.proposal_wait_ns =
                Some(self.proposal_wait_ns.map_or(wait, |w| (w.saturating_mul(7).saturating_add(wait)) / 8));
        }
        let parent_rep = self.hdr(&b.parent).map(|h| h.rep).unwrap_or_default();
        let rep = self.rep_step(parent_rep, b.view);
        self.blocks.insert(id, Stored { proposal: p.clone(), seen: now, rep });
        self.events.push(CoreEvent::Proposed { block: id, proposer: b.proposer });
        // Its parent is certified by the proposal itself.
        self.certs.entry(b.parent).or_insert(pqc);
        // Orphans waiting for this block, and commits that waited for it.
        let waiting: Vec<(u64, MemberKey)> =
            self.orphans.iter().filter(|(_, o)| o.block.parent == id).map(|(k, _)| *k).collect();
        for k in waiting {
            if let Some(o) = self.orphans.remove(&k) {
                self.orphans_adopted += 1;
                let _ = self.on_proposal(o, now, verifier);
            }
        }
        // Votes that came before the block.
        let early: Vec<MemberKey> =
            self.pending_votes.iter().filter(|(_, pv)| pv.block == id).map(|(k, _)| *k).collect();
        for k in early {
            if let Some(pv) = self.pending_votes.remove(&k) {
                let _ = self.on_vote(pv, now);
            }
        }
        self.try_commit_from(id);
        if self.commit_target.is_some() {
            self.advance_commit();
        }
        Ok(())
    }

    fn on_vote(&mut self, v: Vote, now: HostTime) -> Result<(), CoreFault> {
        if v.view < self.committed_view || v.view > self.view + VIEW_WINDOW {
            return Ok(());
        }
        match self.votes_by_voter.get(&(v.view, v.voter)) {
            Some(&(b, r)) if b == v.block && r == v.root => return Ok(()),
            Some(_) => {
                self.events.push(CoreEvent::Equivocation { member: v.voter, view: v.view });
                return Err(CoreFault::Equivocation);
            }
            None => {}
        }
        // Equivocation against a counted vote is checked first: both are signed. A vote counts only for a held block,
        // in that block's own view (@design-reviewer's High: a vote signed for another view would otherwise give a
        // certificate whose view is not its block's).
        match self.block(&v.block) {
            None => {
                match self.pending_votes.get(&v.voter) {
                    Some(p) if p.view == v.view && (p.block, p.root) != (v.block, v.root) => {
                        self.events.push(CoreEvent::Equivocation { member: v.voter, view: v.view });
                        return Err(CoreFault::Equivocation);
                    }
                    Some(p) if p.view >= v.view => {}
                    _ => {
                        self.pending_votes.insert(v.voter, v);
                    }
                }
                return Ok(());
            }
            Some(b) if b.view != v.view || v.view == 0 => return Err(CoreFault::BadVote),
            Some(_) => {}
        }
        self.votes_by_voter.insert((v.view, v.voter), (v.block, v.root));
        let set = self.votes.entry(v.block).or_default();
        set.view = v.view;
        let list = set.by_root.entry(v.root).or_default();
        list.push((v.voter, v.sig, v.self_test));
        if list.len() == self.q {
            let mut votes: Vec<(MemberKey, Signature)> = list.iter().map(|x| (x.0, x.1)).collect();
            votes.sort_by_key(|x| x.0);
            let qc = Qc { block: v.block, view: v.view, root: v.root, votes };
            self.note_qc(&qc, now);
            if self.commit_target.is_some() {
                self.advance_commit();
            }
        }
        // RootSplit: no root can reach q any more.
        let by_root = &self.votes[&v.block].by_root;
        let total: usize = by_root.values().map(|l| l.len()).sum();
        let best = by_root.values().map(|l| l.len()).max().unwrap_or(0);
        let n = self.cfg.members.len();
        if best < self.q && best + (n - total.min(n)) < self.q && self.split_reported.insert(v.block) {
            // Verdicts shown are the signed ones for this split (S3 v1.3); a vote's own verdict is only a hint.
            let roots = match self.cut_ref(&v.block) {
                Some(cut) => self.split_roots(&v.block, &cut),
                None => {
                    by_root.iter().map(|(r, l)| (*r, l.iter().map(|x| (x.0, SelfTest::NotRun)).collect())).collect()
                }
            };
            self.events.push(CoreEvent::RootSplit { block: v.block, roots });
            // Abandon the view at once (L4', coordinator): each repair step then costs about a link
            // delay, not a timeout. A timeout is always safe.
            self.quick_views.insert(v.view);
        }
        Ok(())
    }

    fn on_timeout(&mut self, t: Timeout, now: HostTime, verifier: &dyn Verifier) -> Result<(), CoreFault> {
        if !verifier.verify(&t.voter, &timeout_bytes(self.domain(), t.view, t.high_qc.view), &t.sig) {
            return Err(CoreFault::BadSignature);
        }
        if t.high_qc.view >= t.view {
            return Err(CoreFault::BadCertificate);
        }
        self.check_qc(&t.high_qc, verifier)?;
        self.note_qc(&t.high_qc, now);
        if t.view < self.view || t.view > self.view + VIEW_WINDOW || self.tcs.contains_key(&t.view) {
            return Ok(());
        }
        let set = self.timeouts.entry(t.view).or_default();
        if let Some(&(hv, _)) = set.get(&t.voter) {
            if hv != t.high_qc.view {
                self.events.push(CoreEvent::Equivocation { member: t.voter, view: t.view });
                return Err(CoreFault::Equivocation);
            }
            return Ok(());
        }
        set.insert(t.voter, (t.high_qc.view, t.sig));
        let count = set.len();
        if count >= self.q {
            let timeouts = set.iter().map(|(k, (hv, s))| (*k, *hv, *s)).collect();
            self.note_tc(Tc { view: t.view, timeouts }, now);
        } else if t.view == self.view && count > self.cfg.members.len() - self.q {
            // No certificate is possible in this view any more (a member never votes after timing out): time out at
            // once. A timeout is always safe. At n = 2 that is the other member's single timeout, an abstention.
            self.quick_views.insert(t.view);
        } else if t.view > self.view && count > self.faults_tolerated() {
            // Catch up (a member that missed a certificate while cut off): t + 1 members timed out in a later view,
            // so an honest member is there. Join it and time out at once; that only gives up voting in it.
            self.enter(t.view, now);
            self.join_view = t.view;
        }
        Ok(())
    }

    /// t: how many Byzantine members the quorum tolerates.
    fn faults_tolerated(&self) -> usize {
        (self.cfg.members.len() - 1) / 3
    }

    // ---- acting ----

    /// Advance on time: time out the view when its deadline passes.
    pub fn tick(&mut self, now: HostTime, signer: &dyn Signer) {
        let due = now >= self.deadline() || self.quick_views.contains(&self.view) || self.join_view == self.view;
        if self.timed_out < self.view && due && !self.mutated(Mutation::NoTimeouts) {
            self.timed_out = self.view;
            let t = Timeout {
                view: self.view,
                high_qc: self.high_qc.clone(),
                voter: self.me,
                heads: self.timeout_heads.clone(),
                lateness: self.lateness.clone(),
                sig: signer.sign(&timeout_bytes(self.domain(), self.view, self.high_qc.view)),
            };
            self.out.push((To::All, Msg::Timeout(t.clone())));
            self.recent_timeouts.insert(t.view, t.clone());
            self.last_resend = now;
            let _ = self.on_timeout_local(t, now);
        } else if now >= self.last_resend.plus_nanos(self.timeout_ns()) {
            // No certificate for a timeout period: send this member's recent timeouts again (each the same message, so
            // never equivocation).
            let v = self.view;
            self.recent_timeouts.retain(|tv, _| *tv + 1 >= v);
            let again: Vec<Timeout> = self.recent_timeouts.values().cloned().collect();
            for t in again {
                self.out.push((To::All, Msg::Timeout(t)));
            }
            self.last_resend = now;
        }
    }

    fn on_timeout_local(&mut self, t: Timeout, now: HostTime) -> Result<(), CoreFault> {
        let set = self.timeouts.entry(t.view).or_default();
        set.insert(t.voter, (t.high_qc.view, t.sig));
        if set.len() >= self.q && !self.tcs.contains_key(&t.view) {
            let timeouts = set.iter().map(|(k, (hv, s))| (*k, *hv, *s)).collect();
            self.note_tc(Tc { view: t.view, timeouts }, now);
        }
        Ok(())
    }

    /// The chain heads this member's timeouts carry from now on.
    pub fn set_timeout_heads(&mut self, heads: Vec<pe_seams::ChainHead>) {
        self.timeout_heads = heads;
    }

    /// The lateness reports this member's votes and timeouts carry from now on.
    pub fn set_lateness(&mut self, lateness: Vec<Lateness>) {
        self.lateness = lateness;
    }

    /// Abstain in the current view (the outage rule, agreement.md 3): time it out at the next tick, without growing
    /// the timeout. A timeout is always safe.
    pub fn abstain(&mut self) {
        self.quick_views.insert(self.view);
    }

    /// The current view's timeout, if it comes, doesn't grow the next one (a leader whose backlog cap stops its cut:
    /// no cut possible isn't a failed leader).
    pub fn spare_view(&mut self) {
        self.spared_views.insert(self.view);
    }

    /// If this member leads the current view and has not proposed in it: what its proposal builds on.
    pub fn lead(&self) -> Option<Lead> {
        let v = self.view;
        if self.proposed_view >= v || self.timed_out >= v || self.leader(v) != Some(self.me) {
            return None;
        }
        let tc = if self.high_qc.view + 1 == v { None } else { Some(self.tcs.get(&(v - 1))?.clone()) };
        let parent = self.block(&self.high_qc.block)?.clone();
        let seen = if self.high_qc.block == self.genesis_id {
            self.view_entered
        } else {
            self.blocks.get(&self.high_qc.block)?.seen
        };
        let not_before = seen.plus_nanos(self.cfg.timing.cadence_ns).max(self.view_entered);
        Some(Lead { view: v, parent, parent_qc: self.high_qc.clone(), tc, not_before })
    }

    /// Propose `cut` as the leader of the current view. The cut's height is the parent's + 1.
    pub fn propose(
        &mut self,
        cut: Cut,
        now: HostTime,
        signer: &dyn Signer,
        verifier: &dyn Verifier,
    ) -> Option<BlockId> {
        let lead = self.lead()?;
        if cut.at.height != lead.parent.height() + 1 || cut.at.line != self.cfg.line {
            return None;
        }
        let block = Block { view: lead.view, parent: lead.parent.id(), proposer: self.me, cut };
        let id = block.id();
        let sig = signer.sign(&propose_bytes(self.domain(), id));
        let p = Proposal { block, parent_qc: lead.parent_qc, tc: lead.tc, sig };
        self.proposed_view = lead.view;
        self.out.push((To::All, Msg::Propose(p.clone())));
        self.on_proposal(p, now, verifier).ok()?;
        Some(id)
    }

    /// The proposal this member may vote for now, by the safety rules: of the current view, from its leader, the
    /// lock satisfied, not yet voted or timed out in this view. The node still checks its cut.
    pub fn votable(&self) -> Option<&Proposal> {
        let v = self.view;
        if self.last_voted >= v || self.timed_out >= v {
            return None;
        }
        // Of the view's proposals (one per proposer), the first from the leader its own parent names.
        let lo = (v, MemberKey([0; 32]));
        let hi = (v, MemberKey([0xff; 32]));
        self.proposed_in.range(lo..=hi).find_map(|((_, proposer), id)| {
            let p = &self.blocks.get(id)?.proposal;
            if self.leader_on(v, &p.block.parent) != Some(*proposer) {
                return None;
            }
            let lock_ok = p.parent_qc.view + 1 == v
                || p.tc.as_ref().is_some_and(|tc| p.parent_qc.view >= tc.high_view())
                || self.mutated(Mutation::NoLockRule);
            lock_ok.then_some(p)
        })
    }

    /// Vote for the current view's proposal `block` with `root`. None if the rules no longer allow it.
    pub fn vote(
        &mut self,
        block: BlockId,
        root: Hash,
        self_test: SelfTest,
        heads: Vec<pe_seams::ChainHead>,
        now: HostTime,
        signer: &dyn Signer,
    ) -> Option<Vote> {
        let p = self.votable()?;
        if p.block.id() != block {
            return None;
        }
        let root = if self.mutated(Mutation::VoteWithoutRoot) { Hash::ZERO } else { root };
        let view = self.view;
        self.last_voted = view;
        let v = Vote {
            view,
            block,
            root,
            voter: self.me,
            self_test,
            heads,
            lateness: self.lateness.clone(),
            sig: signer.sign(&vote_bytes(self.domain(), view, block, root)),
        };
        self.out.push((To::All, Msg::Vote(v.clone())));
        let _ = self.on_vote(v.clone(), now);
        Some(v)
    }

    /// The cut reference of a stored block.
    pub fn cut_ref(&self, id: &BlockId) -> Option<CutRef> {
        self.block(id).map(|b| b.cut.at)
    }
}
