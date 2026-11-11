//! `LockstepAgreement`: the lockstep mock behind S3's [`Agreement`] and [`EntrySource`] (DESIGN.md 12, M0 step 2).
//!
//! It keeps the old unanimous rule on purpose, so P8 still bites on a real run: a cut is final only when every
//! member has voted the same root for it, so one silent member stops finality, and the mock reports `Paused` once the
//! final frame lags its clock by more than the old 2 s window. It exists only to test the harness and the node
//! driver over the real seam traits. It is never a reference for pe-agree.
//!
//! - **Entries** are S1's signed entries, verified with test_util's [`TestKeys`]. Two different entries at one
//!   (chain, seq) are an equivocation. An entry stamped below an earlier stamp of its chain is predicted absent.
//! - **Dissemination** (S3's line, coordinator me16a12755f58): each new valid entry is relayed once (the fast path),
//!   and a member that lacks an entry a known cut names fetches it by name from the others (the guarantee). So every
//!   honest member holds what a cut covers before it can vote for it, and P4's void sets can be equal.
//! - **Cuts** come every `cadence_ns` (a duration, converted at the clock's rate). For the cut at frame `f`, each
//!   member sends a signed mark naming the last entry stamped below `f` in the contiguous held prefix of its own
//!   chain. With every member's mark the cut is known; a member votes once it holds every entry the cut includes (the
//!   d1 rule, through `wants_roots`).
//! - **Only a covered entry can be void** (coordinator mccb3d2b98d1c). An uncovered entry stamped below a final cut's
//!   frame is held but never applied (parked), with no event. The first final cut that covers an entry voids it if
//!   it is back-stamped (`BackStamped`) or stamped below the previous final cut's frame (`Late`), on every member
//!   alike. Voids are never decided by arrival.
//! - **The genesis cut** is height 0 at frame 0 and includes nothing, so `final_entries(genesis, c)` is the final
//!   log up to `c`.
//! - **Wire:** a tag byte, then an entry (S1's encoding), a mark or a vote. Marks and votes are signed by their
//!   author and must come from it. A fault blames the sender, except an equivocation, which the two signed
//!   entries prove against their author. `receive` never panics; bad bytes are a [`Fault`] and an event.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use pe_seams::test_util::{TestKeys, TestSigner};
use pe_seams::{
    AgreeEvent, AgreeStats, Agreement, ChainHead, ChainId, Cut, CutRef, Entry, EntryRef, EntrySource, Fault, FaultKind,
    FinalCut, Frame, FrameClock, Hash, HostTime, Lane, LineId, MemberKey, Outgoing, PeerId, SelfTest, Signature,
    SignedEntry, Signer, SubmitError, Verifier, VoidReason,
};

/// The final frame lagging the clock by more than this makes the mock pause: the old 2 s window.
pub const WINDOW_NS: i64 = 2_000_000_000;

const TAG_ENTRY: u8 = 1;
const TAG_MARK: u8 = 2;
const TAG_VOTE: u8 = 3;
const TAG_FETCH: u8 = 4;
const SIG_CONTEXT: &[u8] = b"pe.mock.lockstep\0";

type Head = Option<(ChainId, u64, EntryRef)>;

pub struct LockstepAgreement {
    /// Relay each new entry once (the default). Off, the mock has no way to bring an entry to a member its author
    /// skipped: probe-d1's relay-off run uses that to test pe-agree's fetch path (coordinator me16a12755f58).
    pub relay: bool,
    /// Fetch what a known cut names and this member lacks (the default). Off together with `relay`, the mock cannot
    /// disseminate at all: probe-d1's control that the probe sees the stall.
    pub fetch: bool,
    /// The mock's planted defect for P15's control: hand over cuts in `wants_roots` without holding what they name
    /// (the d1 defect, `VoteWithoutChainValidation`'s shape). Off by default.
    pub vote_without_holding: bool,
    keys: TestKeys,
    signer: TestSigner,
    me: MemberKey,
    members: Vec<MemberKey>,
    line: LineId,
    cadence_ns: i64,
    /// Frames per cut, fixed at the first tick from the clock's rate.
    k: u64,
    /// My chain: its id and its last (seq, id).
    own: Option<(ChainId, u64, EntryRef)>,
    held: BTreeMap<(ChainId, u64), SignedEntry>,
    /// Entries predicted absent: back-stamped in their chain, or voided by a final cut.
    absent: BTreeSet<EntryRef>,
    /// Voids already reported.
    reported: BTreeSet<EntryRef>,
    /// Uncovered entries stamped below a final cut's frame: held, never applied, not void.
    parked: BTreeSet<EntryRef>,
    /// When each missing (chain, seq) was last asked for, in frames.
    asked: BTreeMap<(ChainId, u64), u64>,
    at: BTreeMap<u64, Vec<SignedEntry>>,
    arrived: BTreeMap<EntryRef, HostTime>,
    marks: BTreeMap<u64, BTreeMap<MemberKey, Head>>,
    /// The frame of the last cut I marked (0: none).
    marked: u64,
    votes: BTreeMap<u64, BTreeMap<MemberKey, (Hash, SelfTest, Signature)>>,
    splits: BTreeSet<u64>,
    finals: Vec<FinalCut>,
    holds: BTreeMap<u64, usize>,
    outbox: Vec<Outgoing>,
    changed: Option<u64>,
    events: Vec<AgreeEvent>,
    now_frame: u64,
    rate_hz: u32,
    paused: bool,
}

impl LockstepAgreement {
    /// Member `i` of `keys` (an index into `keys`, not the sorted order), on the origin line.
    pub fn new(keys: &TestKeys, i: usize, cadence_ns: i64) -> Self {
        Self {
            relay: true,
            fetch: true,
            vote_without_holding: false,
            keys: keys.clone(),
            signer: keys.signer(i),
            me: keys.key(i),
            members: keys.sorted_keys(),
            line: LineId::origin(),
            cadence_ns: cadence_ns.max(1),
            k: 0,
            own: None,
            held: BTreeMap::new(),
            absent: BTreeSet::new(),
            reported: BTreeSet::new(),
            parked: BTreeSet::new(),
            asked: BTreeMap::new(),
            at: BTreeMap::new(),
            arrived: BTreeMap::new(),
            marks: BTreeMap::new(),
            marked: 0,
            votes: BTreeMap::new(),
            splits: BTreeSet::new(),
            finals: Vec::new(),
            holds: BTreeMap::new(),
            outbox: Vec::new(),
            changed: None,
            events: Vec::new(),
            now_frame: 0,
            rate_hz: 60,
            paused: false,
        }
    }

    /// The message that carries `e`: what an adversary sends to deliver an entry it built itself (the mock's side of
    /// pe-agree's test-util codec).
    pub fn entry_message(e: &SignedEntry) -> Vec<u8> {
        let mut bytes = vec![TAG_ENTRY];
        bytes.extend(e.encode());
        bytes
    }

    /// The entry an `entry_message` carries, if `bytes` is one: the recorder's view of what a member holds.
    pub fn decode_entry_message(bytes: &[u8]) -> Option<SignedEntry> {
        match bytes.split_first() {
            Some((&TAG_ENTRY, body)) => SignedEntry::decode(body).ok(),
            _ => None,
        }
    }

    /// The genesis cut's address: height 0, frame 0, nothing included.
    pub fn genesis(&self) -> CutRef {
        CutRef { line: self.line, height: 0, frame: Frame(0) }
    }

    fn final_frame(&self) -> u64 {
        self.finals.last().map_or(0, |c| c.cut.at.frame.0)
    }

    fn touch(&mut self, frame: u64) {
        self.changed = Some(self.changed.map_or(frame, |c| c.min(frame)));
    }

    fn broadcast(&mut self, lane: Lane, bytes: Bytes) {
        for &m in &self.members {
            if m != self.me {
                self.outbox.push(Outgoing { to: m, lane, bytes: bytes.clone() });
            }
        }
    }

    fn sign(&self, body: &[u8]) -> Signature {
        let mut msg = SIG_CONTEXT.to_vec();
        msg.extend_from_slice(body);
        self.signer.sign(&msg)
    }

    fn verify(&self, author: &MemberKey, body: &[u8], sig: &Signature) -> bool {
        let mut msg = SIG_CONTEXT.to_vec();
        msg.extend_from_slice(body);
        self.keys.verify(author, &msg, sig)
    }

    fn present(&mut self, e: &SignedEntry) {
        let id = e.id();
        let row = self.at.entry(e.entry.stamp.0).or_default();
        if !row.iter().any(|x| x.id() == id) {
            row.push(e.clone());
            row.sort_by_key(|x| (x.entry.author, x.entry.seq));
            self.touch(e.entry.stamp.0);
        }
    }

    fn hide(&mut self, e: &SignedEntry) {
        let id = e.id();
        if let Some(row) = self.at.get_mut(&e.entry.stamp.0)
            && let Some(i) = row.iter().position(|x| x.id() == id)
        {
            row.remove(i);
            self.touch(e.entry.stamp.0);
        }
    }

    /// Recompute which of a chain's entries are back-stamped, over its contiguous held prefix, and show or hide each.
    fn recheck_chain(&mut self, chain: ChainId) {
        let entries: Vec<SignedEntry> =
            self.held.range((chain, 0)..=(chain, u64::MAX)).map(|(_, e)| e.clone()).collect();
        let mut max_stamp = 0u64;
        let mut expect = 1u64;
        let mut gap = false;
        for e in entries {
            let id = e.id();
            gap |= e.entry.seq != expect;
            expect = e.entry.seq + 1;
            // Past a gap the chain's earlier stamps are unknown: the entry is predicted present for now.
            let back = !gap && e.entry.stamp.0 < max_stamp;
            if !gap {
                max_stamp = max_stamp.max(e.entry.stamp.0);
            }
            if self.reported.contains(&id) {
                continue;
            }
            if back {
                self.absent.insert(id);
            } else {
                self.absent.remove(&id);
            }
            if back || self.parked.contains(&id) {
                self.hide(&e);
            } else {
                self.present(&e);
            }
        }
    }

    /// Takes a verified, well-formed entry; `Ok(true)` if it is new here. `Err` is an equivocation.
    fn insert(&mut self, e: SignedEntry) -> Result<bool, FaultKind> {
        let key = (e.entry.chain, e.entry.seq);
        if let Some(old) = self.held.get(&key) {
            return if old.id() == e.id() { Ok(false) } else { Err(FaultKind::Equivocation) };
        }
        self.held.insert(key, e.clone());
        // Stamped below the final frame and not covered: held, never applied, until a cut covers it.
        if let Some(last) = self.finals.last()
            && e.entry.stamp < last.cut.at.frame
            && !last.cut.includes(&e.entry.chain, e.entry.seq)
        {
            self.parked.insert(e.id());
        }
        self.recheck_chain(e.entry.chain);
        Ok(true)
    }

    fn void(&mut self, e: &SignedEntry, reason: VoidReason, cut: CutRef) {
        let id = e.id();
        if self.reported.insert(id) {
            self.absent.insert(id);
            self.hide(e);
            self.events.push(AgreeEvent::Voided { entry: id, author: e.entry.author, reason, cut });
        }
    }

    fn cut_at(&self, f: u64) -> Option<Cut> {
        let marks = self.marks.get(&f)?;
        if self.k == 0 || marks.len() < self.members.len() {
            return None;
        }
        let mut heads: Vec<ChainHead> = marks
            .iter()
            .filter_map(|(&author, h)| h.map(|(chain, seq, entry)| ChainHead { chain, author, seq, entry }))
            .collect();
        heads.sort_by_key(|h| h.chain);
        Some(Cut { at: CutRef { line: self.line, height: f / self.k, frame: Frame(f) }, heads })
    }

    fn holds_all(&self, cut: &Cut) -> bool {
        cut.heads.iter().all(|h| {
            (1..=h.seq).all(|s| self.held.contains_key(&(h.chain, s)))
                && self.held.get(&(h.chain, h.seq)).is_some_and(|e| e.id() == h.entry)
        })
    }

    /// The last entry stamped below `f` in the contiguous held prefix of my chain: the one I submit to, or else the
    /// first chain of mine I hold (an adversary's script may build it for me).
    fn my_head_below(&self, f: u64) -> Head {
        let chain = match self.own {
            Some((chain, _, _)) => chain,
            None => self.held.values().find(|e| e.entry.author == self.me)?.entry.chain,
        };
        let mut head = None;
        for (s, (_, e)) in (1u64..).zip(self.held.range((chain, 0)..=(chain, u64::MAX))) {
            if e.entry.seq != s {
                break;
            }
            if e.entry.stamp.0 < f {
                head = Some((chain, s, e.id()));
            }
        }
        head
    }

    /// Ask every other member for the entries a known, not yet final cut names and I lack; again after a cut's span.
    fn fetch_missing(&mut self) {
        if !self.fetch || self.k == 0 {
            return;
        }
        let mut want = Vec::new();
        let mut f = self.final_frame() + self.k;
        while let Some(cut) = self.cut_at(f) {
            for h in &cut.heads {
                for s in 1..=h.seq {
                    if !self.held.contains_key(&(h.chain, s)) {
                        want.push((h.chain, s));
                    }
                }
            }
            f += self.k;
        }
        for key in want {
            if self.asked.get(&key).is_some_and(|&t| self.now_frame < t + self.k) {
                continue;
            }
            self.asked.insert(key, self.now_frame);
            let mut bytes = vec![TAG_FETCH];
            bytes.extend(key.0.0.0);
            bytes.extend(key.1.to_le_bytes());
            self.broadcast(Lane::Protocol, Bytes::from(bytes));
        }
    }

    fn mark_body(f: u64, author: &MemberKey, head: &Head) -> Vec<u8> {
        let mut b = Vec::with_capacity(8 + 32 + 1 + 72);
        b.extend(f.to_le_bytes());
        b.extend(author.0);
        match head {
            Some((chain, seq, entry)) => {
                b.push(1);
                b.extend(chain.0.0);
                b.extend(seq.to_le_bytes());
                b.extend(entry.0.0);
            }
            None => b.push(0),
        }
        b
    }

    fn vote_body(f: u64, author: &MemberKey, root: &Hash, st: SelfTest) -> Vec<u8> {
        let mut b = Vec::with_capacity(8 + 32 + 32 + 1);
        b.extend(f.to_le_bytes());
        b.extend(author.0);
        b.extend(root.0);
        b.push(match st {
            SelfTest::NotRun => 0,
            SelfTest::Passed => 1,
            SelfTest::Failed => 2,
        });
        b
    }

    /// Finalize what the votes allow, in order; report a split once.
    fn progress(&mut self) {
        while self.k > 0 {
            let f = self.final_frame() + self.k;
            let Some(votes) = self.votes.get(&f) else { return };
            if votes.len() < self.members.len() {
                return;
            }
            let mut by_root: BTreeMap<Hash, Vec<(MemberKey, SelfTest)>> = BTreeMap::new();
            for (&m, &(root, st, _)) in votes {
                by_root.entry(root).or_default().push((m, st));
            }
            let Some(cut) = self.cut_at(f) else { return };
            if by_root.len() > 1 {
                if self.splits.insert(f) {
                    self.events.push(AgreeEvent::RootSplit { cut: cut.at, roots: by_root.into_iter().collect() });
                }
                return;
            }
            let root = *by_root.keys().next().unwrap_or(&Hash([0; 32]));
            let voters: Vec<(MemberKey, Signature)> = votes.iter().map(|(&m, &(_, _, sig))| (m, sig)).collect();
            let prev = self.finals.last().map(|c| c.cut.clone());
            self.finals.push(FinalCut { cut: cut.clone(), root, voters: voters.clone() });
            self.events.push(AgreeEvent::CutFinal { cut: cut.at, root, voters: voters.iter().map(|v| v.0).collect() });
            // The entries this cut covers for the first time: back-stamped or late ones are void, decided here.
            let prev_frame = prev.as_ref().map_or(0, |p| p.at.frame.0);
            let newly: Vec<SignedEntry> = self
                .held
                .values()
                .filter(|e| {
                    cut.includes(&e.entry.chain, e.entry.seq)
                        && !prev.as_ref().is_some_and(|p| p.includes(&e.entry.chain, e.entry.seq))
                })
                .cloned()
                .collect();
            for e in newly {
                self.parked.remove(&e.id());
                if self.absent.contains(&e.id()) {
                    self.void(&e, VoidReason::BackStamped, cut.at);
                } else if e.entry.stamp.0 < prev_frame {
                    self.void(&e, VoidReason::Late, cut.at);
                }
            }
            // Uncovered and stamped below the frame: parked, never applied, not void.
            let park: Vec<SignedEntry> = self
                .held
                .values()
                .filter(|e| {
                    e.entry.stamp.0 < f
                        && !cut.includes(&e.entry.chain, e.entry.seq)
                        && !self.reported.contains(&e.id())
                        && !self.parked.contains(&e.id())
                })
                .cloned()
                .collect();
            for e in park {
                self.parked.insert(e.id());
                self.hide(&e);
            }
        }
    }

    fn fault(&mut self, from: PeerId, author: Option<MemberKey>, kind: FaultKind, detail: &str) -> Fault {
        let f = Fault { from, author, kind, detail: detail.into() };
        self.events.push(AgreeEvent::Fault(f.clone()));
        f
    }

    fn receive_inner(
        &mut self,
        from: PeerId,
        bytes: &[u8],
        at: HostTime,
    ) -> Result<(), (Option<MemberKey>, FaultKind, &'static str)> {
        let (&tag, body) = bytes.split_first().ok_or((None, FaultKind::Malformed, "empty"))?;
        match tag {
            TAG_ENTRY => {
                let e = SignedEntry::decode(body).map_err(|_| (None, FaultKind::Malformed, "entry bytes"))?;
                let author = Some(e.entry.author);
                if e.entry.line != self.line {
                    return Err((None, FaultKind::WrongLine, "entry for another line"));
                }
                if self.members.binary_search(&e.entry.author).is_err() {
                    return Err((None, FaultKind::NotMember, "entry author"));
                }
                if !e.verify(&self.keys) {
                    return Err((None, FaultKind::BadSignature, "entry signature"));
                }
                e.entry.check_shape().map_err(|_| (None, FaultKind::Malformed, "chain shape"))?;
                let (id, chain, seq, who) = (e.id(), e.entry.chain, e.entry.seq, e.entry.author);
                let message = Self::entry_message(&e);
                let new = self.insert(e).map_err(|k| {
                    self.events.push(AgreeEvent::Equivocation { author: who, chain, seq });
                    (author, k, "two entries at one position")
                })?;
                self.arrived.entry(id).or_insert(at);
                if new && self.relay {
                    // Relay each new entry once, so every member comes to hold every valid entry and the void sets
                    // can be equal (P4), even for an entry its author sent to one member alone (probe-d1).
                    self.broadcast(Lane::Action, Bytes::from(message));
                }
                Ok(())
            }
            TAG_MARK | TAG_VOTE => {
                let fixed = if tag == TAG_MARK { 8 + 32 + 1 } else { 8 + 32 + 32 + 1 };
                if body.len() < fixed + 64 {
                    return Err((None, FaultKind::Malformed, "short"));
                }
                let (msg, sig) = body.split_at(body.len() - 64);
                let sig = Signature(sig.try_into().map_err(|_| (None, FaultKind::Malformed, "sig"))?);
                let f = u64::from_le_bytes(msg[..8].try_into().unwrap_or_default());
                let author = MemberKey(msg[8..40].try_into().unwrap_or_default());
                if author != from {
                    return Err((None, FaultKind::Protocol, "names another author"));
                }
                if self.members.binary_search(&author).is_err() {
                    return Err((Some(author), FaultKind::NotMember, "not a member"));
                }
                if !self.verify(&author, msg, &sig) {
                    return Err((Some(author), FaultKind::BadSignature, "signature"));
                }
                if tag == TAG_MARK {
                    let head = match (msg[40], msg.len()) {
                        (0, 41) => None,
                        (1, 113) => Some((
                            ChainId(Hash(msg[41..73].try_into().unwrap_or_default())),
                            u64::from_le_bytes(msg[73..81].try_into().unwrap_or_default()),
                            EntryRef(Hash(msg[81..113].try_into().unwrap_or_default())),
                        )),
                        _ => return Err((Some(author), FaultKind::Malformed, "mark")),
                    };
                    let row = self.marks.entry(f).or_default();
                    match row.get(&author) {
                        Some(h) if *h != head => return Err((Some(author), FaultKind::Protocol, "two marks")),
                        _ => {
                            row.insert(author, head);
                        }
                    }
                } else {
                    if msg.len() != fixed {
                        return Err((Some(author), FaultKind::Malformed, "vote"));
                    }
                    let root = Hash(msg[40..72].try_into().unwrap_or_default());
                    let st = match msg[72] {
                        0 => SelfTest::NotRun,
                        1 => SelfTest::Passed,
                        2 => SelfTest::Failed,
                        _ => return Err((Some(author), FaultKind::Malformed, "self-test")),
                    };
                    // A later vote replaces an earlier one: a revote after a repair.
                    self.votes.entry(f).or_default().insert(author, (root, st, sig));
                }
                Ok(())
            }
            TAG_FETCH => {
                if body.len() != 40 {
                    return Err((None, FaultKind::Malformed, "fetch"));
                }
                let chain = ChainId(Hash(body[..32].try_into().unwrap_or_default()));
                let seq = u64::from_le_bytes(body[32..].try_into().unwrap_or_default());
                if let Some(e) = self.held.get(&(chain, seq)) {
                    let bytes = Bytes::from(Self::entry_message(e));
                    self.outbox.push(Outgoing { to: from, lane: Lane::Action, bytes });
                }
                Ok(())
            }
            _ => Err((None, FaultKind::Malformed, "tag")),
        }
    }
}

impl EntrySource for LockstepAgreement {
    fn final_cut(&self) -> Option<&FinalCut> {
        self.finals.last()
    }

    /// It never trims (S3 v1.2): every frame is kept.
    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        Some(self.at.get(&frame.0).map_or(&[], |v| v.as_slice()))
    }

    fn kept_from(&self) -> Frame {
        Frame(0)
    }

    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}

    fn take_changed_from(&mut self) -> Option<Frame> {
        self.changed.take().map(Frame)
    }

    fn arrived_at(&self, entry: &EntryRef) -> Option<HostTime> {
        self.arrived.get(entry).copied()
    }
}

impl Agreement for LockstepAgreement {
    fn line(&self) -> LineId {
        self.line
    }

    fn submit(&mut self, entry: Entry) -> Result<SignedEntry, SubmitError> {
        let (next_seq, prev, chain) = match self.own {
            Some((chain, seq, id)) => (seq + 1, Some(id), Some(chain)),
            None => (1, None, None),
        };
        if entry.author != self.me
            || entry.line != self.line
            || entry.seq != next_seq
            || entry.prev != prev
            || chain.is_some_and(|c| c != entry.chain)
        {
            return Err(SubmitError::NotNext);
        }
        entry.check_shape().map_err(SubmitError::Shape)?;
        if entry.stamp.0 < self.marked.max(self.final_frame()) {
            return Err(SubmitError::TooLate);
        }
        let signed = entry.sign(&self.signer);
        self.own = Some((signed.entry.chain, signed.entry.seq, signed.id()));
        self.held.insert((signed.entry.chain, signed.entry.seq), signed.clone());
        self.recheck_chain(signed.entry.chain);
        self.broadcast(Lane::Action, Bytes::from(Self::entry_message(&signed)));
        Ok(signed)
    }

    fn receive(&mut self, from: PeerId, _lane: Lane, bytes: &[u8], at: HostTime) -> Result<(), Fault> {
        let r = self.receive_inner(from, bytes, at);
        self.progress();
        r.map_err(|(author, kind, detail)| self.fault(from, author, kind, detail))
    }

    fn tick(&mut self, now: HostTime, clock: &dyn FrameClock) -> Vec<Outgoing> {
        self.rate_hz = clock.rate_hz().max(1);
        if self.k == 0 {
            let k = i128::from(self.cadence_ns) * i128::from(self.rate_hz) / 1_000_000_000;
            self.k = k.max(1) as u64;
        }
        self.now_frame = clock.frame_at(now).map_or(0, |f| f.0);
        while self.marked + self.k <= self.now_frame {
            let f = self.marked + self.k;
            let head = self.my_head_below(f);
            let body = Self::mark_body(f, &self.me, &head);
            let sig = self.sign(&body);
            self.marks.entry(f).or_default().insert(self.me, head);
            let mut bytes = vec![TAG_MARK];
            bytes.extend(body);
            bytes.extend(sig.0);
            self.broadcast(Lane::Protocol, Bytes::from(bytes));
            self.marked = f;
        }
        self.progress();
        self.fetch_missing();
        let behind = self.now_frame.saturating_sub(self.final_frame());
        let behind_ns = i128::from(behind) * 1_000_000_000 / i128::from(self.rate_hz);
        if behind_ns > i128::from(WINDOW_NS) {
            if !self.paused {
                self.paused = true;
                self.events.push(AgreeEvent::Paused { at: Frame(self.now_frame) });
            }
        } else {
            self.paused = false;
        }
        std::mem::take(&mut self.outbox)
    }

    fn wants_roots(&self) -> Vec<Cut> {
        let mut out = Vec::new();
        if self.k == 0 {
            return out;
        }
        let hold = self.holds.keys().next().copied();
        let mut f = self.final_frame() + self.k;
        while let Some(cut) = self.cut_at(f) {
            if hold.is_some_and(|h| f >= h) || !(self.vote_without_holding || self.holds_all(&cut)) {
                break;
            }
            if !self.votes.get(&f).is_some_and(|v| v.contains_key(&self.me)) {
                out.push(cut);
            }
            f += self.k;
        }
        out
    }

    fn vote(&mut self, cut: &CutRef, root: Hash, self_test: SelfTest) {
        let f = cut.frame.0;
        let body = Self::vote_body(f, &self.me, &root, self_test);
        let sig = self.sign(&body);
        self.votes.entry(f).or_default().insert(self.me, (root, self_test, sig));
        self.events.push(AgreeEvent::Voted { cut: *cut, root, self_test });
        let mut bytes = vec![TAG_VOTE];
        bytes.extend(body);
        bytes.extend(sig.0);
        self.broadcast(Lane::Protocol, Bytes::from(bytes));
        self.progress();
    }

    fn hold_votes_from(&mut self, frame: Frame) {
        *self.holds.entry(frame.0).or_default() += 1;
    }

    fn release_votes_from(&mut self, frame: Frame) {
        if let Some(n) = self.holds.get_mut(&frame.0) {
            *n -= 1;
            if *n == 0 {
                self.holds.remove(&frame.0);
            }
        }
    }

    fn final_entries(&self, from: &CutRef, to: &CutRef) -> Option<Vec<SignedEntry>> {
        let find = |r: &CutRef| self.finals.iter().find(|c| c.cut.at == *r).map(|c| &c.cut);
        let lower = if from.height == 0 { None } else { Some(find(from)?) };
        let upper = find(to)?;
        let mut out: Vec<SignedEntry> = self
            .held
            .values()
            .filter(|e| {
                upper.includes(&e.entry.chain, e.entry.seq)
                    && !lower.is_some_and(|l| l.includes(&e.entry.chain, e.entry.seq))
                    && !self.absent.contains(&e.id())
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            (a.entry.stamp, a.entry.author, a.entry.seq).cmp(&(b.entry.stamp, b.entry.author, b.entry.seq))
        });
        Some(out)
    }

    fn members(&self) -> Vec<MemberKey> {
        self.members.clone()
    }

    fn stats(&self) -> AgreeStats {
        let ff = self.final_frame();
        AgreeStats {
            members: self.members.len(),
            quorum: self.members.len(),
            final_height: self.finals.last().map_or(0, |c| c.cut.at.height),
            final_frame: self.finals.last().map(|c| c.cut.at.frame),
            unconfirmed_since: None,
            pending_entries: self.held.values().filter(|e| e.entry.stamp.0 >= ff).count(),
        }
    }

    fn events(&mut self) -> Vec<AgreeEvent> {
        std::mem::take(&mut self.events)
    }
}
