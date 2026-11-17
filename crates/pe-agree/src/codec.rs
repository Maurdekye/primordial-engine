//! pe-agree's wire messages (Lane::Protocol) and their codec. Little-endian, length-checked, bounded counts: decoding
//! never panics, and refuses anything over the limits. Entries travel as `SignedEntry::encode` bytes.

use pe_seams::{
    ChainHead, ChainId, Cut, CutRef, EntryRef, Frame, Hash, LineId, MemberKey, SelfTest, Signature, SignedEntry,
};

use crate::core::{Block, BlockId, Lateness, Msg, Proposal, Qc, Report, Tc, Timeout, Vote};

/// Most heads in a cut: one per chain, and a member has one chain per line (a few more over restarts).
pub const MAX_HEADS: usize = 1024;
/// Most signatures in a certificate.
pub const MAX_SIGNERS: usize = 256;
/// Most entries named in one fetch, or carried in one reply.
pub const MAX_FETCH: usize = 256;

/// Everything pe-agree sends.
#[allow(clippy::large_enum_variant)] // decoded one at a time, never stored in bulk
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wire {
    /// A core message of epoch 0 (agreement.md 8).
    Core(Msg),
    /// A core message of a later epoch: the epoch, then the message as [`Wire::Core`] encodes it.
    Epoch(u64, Msg),
    /// Unsigned, to former members (agreement.md 8): the sender has the reconfiguring cut that starts `epoch` final. A
    /// leaver quits once every remaining member has sent it; a forged one can only make a leaver quit early.
    EpochFinal { epoch: u64 },
    /// Ask for entries by name.
    FetchEntries(Vec<EntryRef>),
    /// Entries: the author's own, a relay, or a fetch's answer.
    Entries(Vec<SignedEntry>),
    /// Ask for an author's chain from `from_seq`, at most `count` entries (at most [`MAX_FETCH`]): the range refetch
    /// that heals holes after a stall or a loss (coordinator).
    FetchRange { author: MemberKey, chain: ChainId, from_seq: u64, count: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeError;

const T_PROPOSE: u8 = 1;
const T_VOTE: u8 = 2;
const T_TIMEOUT: u8 = 3;
const T_FETCH_BLOCK: u8 = 4;
const T_FETCH_ENTRIES: u8 = 5;
const T_ENTRIES: u8 = 6;
const T_REPORT: u8 = 7;
const T_FETCH_RANGE: u8 = 8;
const T_EPOCH: u8 = 9;
const T_EPOCH_FINAL: u8 = 10;

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn b32(&mut self, v: &[u8; 32]) {
        self.0.extend_from_slice(v);
    }
    fn sig(&mut self, s: &Signature) {
        self.0.extend_from_slice(&s.0);
    }
    fn cut(&mut self, c: &Cut) {
        self.b32(&c.at.line.0.0);
        self.u64(c.at.height);
        self.u64(c.at.frame.0);
        self.u32(c.heads.len() as u32);
        for h in &c.heads {
            self.head(h);
        }
    }
    fn head(&mut self, h: &ChainHead) {
        self.b32(&h.chain.0.0);
        self.b32(&h.author.0);
        self.u64(h.seq);
        self.b32(&h.entry.0.0);
    }
    /// Frames as i32 (saturating): a lateness past 2^31 frames either way is no different from one at it.
    fn lateness(&mut self, ls: &[Lateness]) {
        self.u32(ls.len() as u32);
        for l in ls {
            self.b32(&l.author.0);
            self.b32(&l.chain.0.0);
            self.u32(l.frames.clamp(i32::MIN as i64, i32::MAX as i64) as i32 as u32);
            self.u64(l.seq);
        }
    }
    fn qc(&mut self, q: &Qc) {
        self.b32(&q.block.0.0);
        self.u64(q.view);
        self.b32(&q.root.0);
        self.u32(q.votes.len() as u32);
        for (k, s) in &q.votes {
            self.b32(&k.0);
            self.sig(s);
        }
    }
    fn tc(&mut self, t: &Tc) {
        self.u64(t.view);
        self.u32(t.timeouts.len() as u32);
        for (k, hv, s) in &t.timeouts {
            self.b32(&k.0);
            self.u64(*hv);
            self.sig(s);
        }
    }
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return Err(DecodeError);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| DecodeError)?))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| DecodeError)?))
    }
    fn b32(&mut self) -> Result<[u8; 32], DecodeError> {
        self.take(32)?.try_into().map_err(|_| DecodeError)
    }
    fn hash(&mut self) -> Result<Hash, DecodeError> {
        Ok(Hash(self.b32()?))
    }
    fn sig(&mut self) -> Result<Signature, DecodeError> {
        Ok(Signature(self.take(64)?.try_into().map_err(|_| DecodeError)?))
    }
    fn count(&mut self, max: usize) -> Result<usize, DecodeError> {
        let n = self.u32()? as usize;
        if n > max { Err(DecodeError) } else { Ok(n) }
    }
    fn cut(&mut self) -> Result<Cut, DecodeError> {
        let line = LineId(self.hash()?);
        let height = self.u64()?;
        let frame = Frame(self.u64()?);
        let n = self.count(MAX_HEADS)?;
        let mut heads = Vec::with_capacity(n);
        for _ in 0..n {
            heads.push(self.head()?);
        }
        Ok(Cut { at: CutRef { line, height, frame }, heads })
    }
    fn head(&mut self) -> Result<ChainHead, DecodeError> {
        Ok(ChainHead {
            chain: ChainId(self.hash()?),
            author: MemberKey(self.b32()?),
            seq: self.u64()?,
            entry: EntryRef(self.hash()?),
        })
    }
    fn lateness(&mut self) -> Result<Vec<Lateness>, DecodeError> {
        let n = self.count(MAX_HEADS)?;
        let mut ls = Vec::with_capacity(n);
        for _ in 0..n {
            ls.push(Lateness {
                author: MemberKey(self.b32()?),
                chain: ChainId(self.hash()?),
                frames: self.u32()? as i32 as i64,
                seq: self.u64()?,
            });
        }
        Ok(ls)
    }
    fn qc(&mut self) -> Result<Qc, DecodeError> {
        let block = BlockId(self.hash()?);
        let view = self.u64()?;
        let root = self.hash()?;
        let n = self.count(MAX_SIGNERS)?;
        let mut votes = Vec::with_capacity(n);
        for _ in 0..n {
            votes.push((MemberKey(self.b32()?), self.sig()?));
        }
        Ok(Qc { block, view, root, votes })
    }
    fn self_test(&mut self) -> Result<SelfTest, DecodeError> {
        let tag = self.u8()?;
        if tag == 0 {
            return Ok(SelfTest::NotRun);
        }
        let split = CutRef { line: LineId(self.hash()?), height: self.u64()?, frame: Frame(self.u64()?) };
        match tag {
            1 => Ok(SelfTest::Passed { split }),
            2 => Ok(SelfTest::Failed { split }),
            _ => Err(DecodeError),
        }
    }
    fn tc(&mut self) -> Result<Tc, DecodeError> {
        let view = self.u64()?;
        let n = self.count(MAX_SIGNERS)?;
        let mut timeouts = Vec::with_capacity(n);
        for _ in 0..n {
            timeouts.push((MemberKey(self.b32()?), self.u64()?, self.sig()?));
        }
        Ok(Tc { view, timeouts })
    }
}

impl W {
    /// A verdict: 0, or 1 / 2 followed by its split cut (line, height, frame).
    fn self_test(&mut self, s: SelfTest) {
        let (tag, split) = match s {
            SelfTest::NotRun => (0, None),
            SelfTest::Passed { split } => (1, Some(split)),
            SelfTest::Failed { split } => (2, Some(split)),
        };
        self.u8(tag);
        if let Some(c) = split {
            self.b32(&c.line.0.0);
            self.u64(c.height);
            self.u64(c.frame.0);
        }
    }
}

pub fn encode(m: &Wire) -> Vec<u8> {
    if let Wire::Epoch(epoch, msg) = m {
        let inner = encode(&Wire::Core(msg.clone()));
        let mut w = W(Vec::with_capacity(inner.len() + 9));
        w.u8(T_EPOCH);
        w.u64(*epoch);
        w.0.extend_from_slice(&inner);
        return w.0;
    }
    let mut w = W(Vec::with_capacity(256));
    match m {
        Wire::Core(Msg::Propose(p)) => {
            w.u8(T_PROPOSE);
            w.u64(p.block.view);
            w.b32(&p.block.parent.0.0);
            w.b32(&p.block.proposer.0);
            w.cut(&p.block.cut);
            w.qc(&p.parent_qc);
            match &p.tc {
                Some(t) => {
                    w.u8(1);
                    w.tc(t);
                }
                None => w.u8(0),
            }
            w.sig(&p.sig);
        }
        Wire::Core(Msg::Vote(v)) => {
            w.u8(T_VOTE);
            w.u64(v.view);
            w.b32(&v.block.0.0);
            w.b32(&v.root.0);
            w.b32(&v.voter.0);
            w.self_test(v.self_test);
            w.u32(v.heads.len() as u32);
            for h in &v.heads {
                w.head(h);
            }
            w.lateness(&v.lateness);
            w.sig(&v.sig);
        }
        Wire::Core(Msg::Timeout(t)) => {
            w.u8(T_TIMEOUT);
            w.u64(t.view);
            w.qc(&t.high_qc);
            w.b32(&t.voter.0);
            w.u32(t.heads.len() as u32);
            for h in &t.heads {
                w.head(h);
            }
            w.lateness(&t.lateness);
            w.sig(&t.sig);
        }
        Wire::Core(Msg::Report(rp)) => {
            w.u8(T_REPORT);
            w.self_test(rp.verdict);
            w.b32(&rp.voter.0);
            w.sig(&rp.sig);
        }
        Wire::Core(Msg::Fetch(id)) => {
            w.u8(T_FETCH_BLOCK);
            w.b32(&id.0.0);
        }
        Wire::FetchEntries(ids) => {
            w.u8(T_FETCH_ENTRIES);
            w.u32(ids.len() as u32);
            for id in ids {
                w.b32(&id.0.0);
            }
        }
        // Encoded above, as an envelope around the plain core message.
        Wire::Epoch(..) => {}
        Wire::EpochFinal { epoch } => {
            w.u8(T_EPOCH_FINAL);
            w.u64(*epoch);
        }
        Wire::FetchRange { author, chain, from_seq, count } => {
            w.u8(T_FETCH_RANGE);
            w.b32(&author.0);
            w.b32(&chain.0.0);
            w.u64(*from_seq);
            w.u32(*count);
        }
        Wire::Entries(es) => {
            w.u8(T_ENTRIES);
            w.u32(es.len() as u32);
            for e in es {
                let b = e.encode();
                w.u32(b.len() as u32);
                w.0.extend_from_slice(&b);
            }
        }
    }
    w.0
}

pub fn decode(bytes: &[u8]) -> Result<Wire, DecodeError> {
    if bytes.first() == Some(&T_EPOCH) {
        let mut r = R(&bytes[1..]);
        let epoch = r.u64()?;
        // Epoch 0 travels as a plain core message, and an envelope never nests.
        return match decode(r.0)? {
            Wire::Core(m) if epoch > 0 => Ok(Wire::Epoch(epoch, m)),
            _ => Err(DecodeError),
        };
    }
    let mut r = R(bytes);
    let m = match r.u8()? {
        T_PROPOSE => {
            let view = r.u64()?;
            let parent = BlockId(r.hash()?);
            let proposer = MemberKey(r.b32()?);
            let cut = r.cut()?;
            let parent_qc = r.qc()?;
            let tc = match r.u8()? {
                0 => None,
                1 => Some(r.tc()?),
                _ => return Err(DecodeError),
            };
            let sig = r.sig()?;
            Wire::Core(Msg::Propose(Proposal { block: Block { view, parent, proposer, cut }, parent_qc, tc, sig }))
        }
        T_VOTE => {
            let view = r.u64()?;
            let block = BlockId(r.hash()?);
            let root = r.hash()?;
            let voter = MemberKey(r.b32()?);
            let self_test = r.self_test()?;
            let n = r.count(MAX_HEADS)?;
            let mut heads = Vec::with_capacity(n);
            for _ in 0..n {
                heads.push(r.head()?);
            }
            let lateness = r.lateness()?;
            let sig = r.sig()?;
            Wire::Core(Msg::Vote(Vote { view, block, root, voter, self_test, heads, lateness, sig }))
        }
        T_TIMEOUT => {
            let view = r.u64()?;
            let high_qc = r.qc()?;
            let voter = MemberKey(r.b32()?);
            let n = r.count(MAX_HEADS)?;
            let mut heads = Vec::with_capacity(n);
            for _ in 0..n {
                heads.push(r.head()?);
            }
            let lateness = r.lateness()?;
            let sig = r.sig()?;
            Wire::Core(Msg::Timeout(Timeout { view, high_qc, voter, heads, lateness, sig }))
        }
        T_REPORT => {
            let verdict = r.self_test()?;
            let cut = verdict.split().ok_or(DecodeError)?;
            let voter = MemberKey(r.b32()?);
            let sig = r.sig()?;
            Wire::Core(Msg::Report(Report { cut, verdict, voter, sig }))
        }
        T_FETCH_BLOCK => Wire::Core(Msg::Fetch(BlockId(r.hash()?))),
        T_EPOCH_FINAL => Wire::EpochFinal { epoch: r.u64()? },
        T_FETCH_ENTRIES => {
            let n = r.count(MAX_FETCH)?;
            let mut ids = Vec::with_capacity(n);
            for _ in 0..n {
                ids.push(EntryRef(r.hash()?));
            }
            Wire::FetchEntries(ids)
        }
        T_FETCH_RANGE => {
            let author = MemberKey(r.b32()?);
            let chain = ChainId(r.hash()?);
            let from_seq = r.u64()?;
            let count = r.u32()?;
            if count == 0 || count as usize > MAX_FETCH {
                return Err(DecodeError);
            }
            Wire::FetchRange { author, chain, from_seq, count }
        }
        T_ENTRIES => {
            let n = r.count(MAX_FETCH)?;
            let mut es = Vec::with_capacity(n);
            for _ in 0..n {
                let len = r.u32()? as usize;
                let b = r.take(len)?;
                es.push(SignedEntry::decode(b).map_err(|_| DecodeError)?);
            }
            Wire::Entries(es)
        }
        _ => return Err(DecodeError),
    };
    if !r.0.is_empty() {
        return Err(DecodeError);
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Wire> {
        let line = LineId::origin();
        let h = |s: &[u8]| Hash::of("t", s);
        let cut = Cut {
            at: CutRef { line, height: 3, frame: Frame(77) },
            heads: vec![ChainHead {
                chain: ChainId(h(b"c")),
                author: MemberKey([4; 32]),
                seq: 9,
                entry: EntryRef(h(b"e")),
            }],
        };
        let qc = Qc {
            block: BlockId(h(b"b")),
            view: 5,
            root: h(b"r"),
            votes: vec![(MemberKey([1; 32]), Signature([7; 64]))],
        };
        let tc = Tc { view: 6, timeouts: vec![(MemberKey([2; 32]), 5, Signature([8; 64]))] };
        let block = Block { view: 7, parent: BlockId(h(b"b")), proposer: MemberKey([3; 32]), cut };
        vec![
            Wire::Core(Msg::Propose(Proposal {
                block: block.clone(),
                parent_qc: qc.clone(),
                tc: Some(tc),
                sig: Signature([9; 64]),
            })),
            Wire::Core(Msg::Propose(Proposal { block, parent_qc: qc.clone(), tc: None, sig: Signature([9; 64]) })),
            Wire::Core(Msg::Vote(Vote {
                view: 7,
                block: BlockId(h(b"x")),
                root: h(b"r"),
                voter: MemberKey([5; 32]),
                self_test: SelfTest::Passed { split: CutRef { line: LineId(h(b"l")), height: 3, frame: Frame(70) } },
                heads: vec![ChainHead {
                    chain: ChainId(h(b"c")),
                    author: MemberKey([4; 32]),
                    seq: 2,
                    entry: EntryRef(h(b"z")),
                }],
                lateness: vec![Lateness { author: MemberKey([4; 32]), chain: ChainId(h(b"c")), frames: 144, seq: 2 }],
                sig: Signature([1; 64]),
            })),
            Wire::Core(Msg::Report(Report {
                cut: CutRef { line: LineId(h(b"l")), height: 3, frame: Frame(70) },
                verdict: SelfTest::Failed { split: CutRef { line: LineId(h(b"l")), height: 3, frame: Frame(70) } },
                voter: MemberKey([6; 32]),
                sig: Signature([2; 64]),
            })),
            Wire::Core(Msg::Timeout(Timeout {
                view: 8,
                high_qc: qc,
                voter: MemberKey([6; 32]),
                heads: vec![ChainHead {
                    chain: ChainId(h(b"d")),
                    author: MemberKey([4; 32]),
                    seq: 9,
                    entry: EntryRef(h(b"y")),
                }],
                lateness: vec![Lateness { author: MemberKey([4; 32]), chain: ChainId(h(b"d")), frames: -3, seq: 9 }],
                sig: Signature([2; 64]),
            })),
            Wire::Core(Msg::Fetch(BlockId(h(b"f")))),
            Wire::Epoch(3, Msg::Fetch(BlockId(h(b"g")))),
            Wire::EpochFinal { epoch: 4 },
            Wire::FetchEntries(vec![EntryRef(h(b"1")), EntryRef(h(b"2"))]),
            Wire::Entries(Vec::new()),
            Wire::FetchRange { author: MemberKey([3; 32]), chain: ChainId(h(b"c")), from_seq: 7, count: 12 },
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for m in sample() {
            assert_eq!(decode(&encode(&m)), Ok(m));
        }
    }

    #[test]
    fn truncated_or_padded_bytes_are_refused_without_panic() {
        for m in sample() {
            let b = encode(&m);
            for n in 0..b.len() {
                assert_eq!(decode(&b[..n]), Err(DecodeError));
            }
            let mut long = b.clone();
            long.push(0);
            assert_eq!(decode(&long), Err(DecodeError));
        }
        // A count beyond the limit is refused before anything is allocated.
        let mut b = vec![T_FETCH_ENTRIES];
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&b), Err(DecodeError));
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut x = 0x1234_5678_9abc_def0u64;
        for len in 0..400 {
            let mut b = Vec::with_capacity(len);
            for _ in 0..len {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                b.push(x as u8);
            }
            if let Some(t) = b.first_mut() {
                *t = (len % 7) as u8;
            }
            let _ = decode(&b);
        }
    }
}
